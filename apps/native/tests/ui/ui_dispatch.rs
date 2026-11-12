//! A pane that follows a dispatch run: its stats line and recent actions,
//! the log kept at its end until scrolled up, the last state of a run that
//! has gone, and the note for a run never seen.

use crate::support;

use gpui::{Modifiers, ScrollDelta, ScrollWheelEvent, TestAppContext, TouchPhase, point, px};
use protocol::{
    ChatScreen, ClientMessage, ComposerState, DaemonMessage, DispatchRunStatus, GridNode,
    PaneFollow, SplitDirection,
};
use rustling_tulip_native::target::{HostKey, Target};
use support::{
    DispatchRunBuilder, Fixture, Harness, REMOTE, TestDir, dispatch_run, follow_pane, pane, repo,
    session, split, tab,
};

const STATS: &str = "working · $0.46 · 842k in · 3k out";
const ENDED_STATS: &str = "ended · $0.46 · 842k in · 3k out";
const ENDED_NOTE: &str = "Ended. Run folder: C:\\runs\\r1";

/// Repo `r1`; tab `t1`: session `s1` in `p1` and a pane `f2` following run
/// `run_id` on This PC.
fn fixture(run_id: &str) -> Fixture {
    Fixture {
        repos: vec![repo("r1", "C:/r1")],
        sessions: vec![session("s1").in_repo("r1").build()],
        tabs: vec![tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                pane("p1", Some("s1")),
                follow_pane("f2", run_id),
            ),
        )],
        ..Fixture::default()
    }
}

/// Run `r1`: working, $0.46, 842k in, 3.1k out, two actions.
fn r1() -> DispatchRunBuilder {
    dispatch_run("r1")
        .tier(2)
        .metrics(0.46, 842_000, 3_100)
        .actions(&["read a.rs", "edit b.rs"])
}

/// The fixture following `run_id`, on a connection with dispatch runs.
fn connected<'a>(cx: &'a mut TestAppContext, dir: &TestDir, run_id: &str) -> Harness<'a> {
    let mut h = Harness::with(cx, dir, &fixture(run_id));
    h.reconnect_with_features(&[protocol::FEATURE_DISPATCH_RUNS]);
    h
}

/// The fixture with `r1` listed.
fn listed<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = connected(cx, dir, "r1");
    h.send(DaemonMessage::DispatchRuns {
        runs: vec![r1().build()],
    });
    h
}

fn stats(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.dispatch_view_stats("f2"))
}

fn lines(h: &mut Harness<'_>) -> Vec<String> {
    h.root(|root, _| root.dispatch_view_lines("f2"))
}

fn note(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.dispatch_view_note("f2"))
}

fn assert_ended(h: &mut Harness<'_>) {
    assert_eq!(lines(h), ["read a.rs", "edit b.rs"], "the lines stay");
    assert_eq!(stats(h).as_deref(), Some(ENDED_STATS));
    assert_eq!(note(h).as_deref(), Some(ENDED_NOTE));
}

#[gpui::test]
fn a_followed_run_shows_its_stats_and_actions(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = listed(cx, &dir);

    assert_eq!(stats(&mut h).as_deref(), Some(STATS));
    assert_eq!(lines(&mut h), ["read a.rs", "edit b.rs"], "oldest first");
    assert_eq!(note(&mut h), None, "a live run shows no note");
    assert!(h.in_model("dispatch-log-f2"), "the log is drawn");
}

#[gpui::test]
fn an_update_refreshes_the_view(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = listed(cx, &dir);

    h.send(DaemonMessage::DispatchRunUpdated {
        run: r1()
            .actions(&["read a.rs", "edit b.rs", "run tests"])
            .build(),
    });

    assert_eq!(
        lines(&mut h).last().map(String::as_str),
        Some("run tests"),
        "the newest action is at the bottom"
    );
}

#[gpui::test]
fn the_log_stops_following_once_scrolled_up(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = connected(cx, &dir, "r1");
    let many: Vec<String> = (0..80).map(|n| format!("action {n}")).collect();
    let many: Vec<&str> = many.iter().map(String::as_str).collect();
    h.send(DaemonMessage::DispatchRuns {
        runs: vec![r1().actions(&many).build()],
    });
    let _ = h.bounds("dispatch-log-f2");
    assert_eq!(
        h.root(|root, _| root.dispatch_view_following("f2")),
        Some(true),
        "the log starts at its end"
    );

    let at = h.bounds("dispatch-log-f2").center();
    h.cx.simulate_event(ScrollWheelEvent {
        position: at,
        delta: ScrollDelta::Pixels(point(px(0.0), px(60.0))),
        modifiers: Modifiers::none(),
        touch_phase: TouchPhase::Moved,
    });
    h.cx.run_until_parked();

    assert_eq!(
        h.root(|root, _| root.dispatch_view_following("f2")),
        Some(false),
        "a wheel up stops the log following"
    );
}

#[gpui::test]
fn a_removed_run_keeps_its_last_state_and_says_it_ended(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = listed(cx, &dir);

    h.send(DaemonMessage::DispatchRunRemoved {
        run_id: "r1".to_owned(),
    });

    assert_ended(&mut h);
}

#[gpui::test]
fn a_relist_without_the_run_ends_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = listed(cx, &dir);

    h.send(DaemonMessage::DispatchRuns { runs: Vec::new() });

    assert_ended(&mut h);
}

#[gpui::test]
fn a_reconnect_keeps_a_live_run_live(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = listed(cx, &dir);

    h.reconnect_with_features(&[protocol::FEATURE_DISPATCH_RUNS]);
    assert_eq!(
        stats(&mut h).as_deref(),
        Some(STATS),
        "the run draws from its kept snapshot until the re-list"
    );
    h.send(DaemonMessage::DispatchRuns {
        runs: vec![r1().build()],
    });

    assert_eq!(stats(&mut h).as_deref(), Some(STATS), "not ended");
    assert_eq!(note(&mut h), None);
}

#[gpui::test]
fn a_reconnect_without_the_feature_ends_a_followed_run(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = listed(cx, &dir);

    h.reconnect_with_features(&[]);

    assert_ended(&mut h);
}

#[gpui::test]
fn a_new_run_on_the_same_pane_follows_from_the_bottom(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = connected(cx, &dir, "r1");
    let many: Vec<String> = (0..80).map(|n| format!("action {n}")).collect();
    let many: Vec<&str> = many.iter().map(String::as_str).collect();
    h.send(DaemonMessage::DispatchRuns {
        runs: vec![r1().actions(&many).build()],
    });
    let at = h.bounds("dispatch-log-f2").center();
    h.cx.simulate_event(ScrollWheelEvent {
        position: at,
        delta: ScrollDelta::Pixels(point(px(0.0), px(60.0))),
        modifiers: Modifiers::none(),
        touch_phase: TouchPhase::Moved,
    });
    h.cx.run_until_parked();
    assert_eq!(
        h.root(|root, _| root.dispatch_view_following("f2")),
        Some(false),
        "precondition: the log was scrolled up"
    );

    h.send(DaemonMessage::Tabs {
        tabs: fixture("r2").tabs,
    });

    assert_eq!(
        h.root(|root, _| root.dispatch_view_following("f2")),
        Some(true),
        "a pane that follows another run starts at its end"
    );
}

#[gpui::test]
fn a_run_ended_before_connecting_shows_the_gone_note(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = connected(cx, &dir, "r9");
    h.send(DaemonMessage::DispatchRuns {
        runs: vec![r1().build()],
    });

    assert_eq!(note(&mut h).as_deref(), Some("This run has ended: r9"));
    assert_eq!(stats(&mut h), None, "no stats for a run never seen");
    assert!(h.in_model("dispatch-gone-f2"), "the note is drawn");
}

#[gpui::test]
fn closing_the_follow_pane_forgets_the_run(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = listed(cx, &dir);
    h.send(DaemonMessage::DispatchRunRemoved {
        run_id: "r1".to_owned(),
    });
    assert!(
        h.root(|root, _| root.kept_run(&HostKey::Local, "r1")),
        "precondition: the removed run is kept"
    );

    h.send(DaemonMessage::Tabs {
        tabs: vec![tab("t1", &pane("p1", Some("s1")))],
    });

    assert!(
        !h.root(|root, _| root.kept_run(&HostKey::Local, "r1")),
        "a run no pane follows is forgotten"
    );
}

/// The title, chip and status pane `f2`'s header draws for its run.
fn header(h: &mut Harness<'_>) -> Option<(String, String, String)> {
    h.root(|root, _| root.dispatch_view_header("f2"))
}

/// Whether pane `f2`'s Stop waits for its confirm.
fn stop_armed(h: &mut Harness<'_>) -> bool {
    h.root(|root, _| root.dispatch_stop_armed("f2"))
}

/// The run id and request id of each `StopDispatchRun` in `sent`.
fn stops(sent: &[ClientMessage]) -> Vec<(String, Option<String>)> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::StopDispatchRun { run_id, request_id } => {
                Some((run_id.clone(), request_id.clone()))
            }
            _ => None,
        })
        .collect()
}

#[gpui::test]
fn the_header_shows_the_runs_title_chip_and_status(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = listed(cx, &dir);

    assert_eq!(
        header(&mut h),
        Some((
            "title r1".to_owned(),
            "DS · backend-x · tier 2".to_owned(),
            "working".to_owned()
        ))
    );
    let title = h.bounds("dispatch-title-f2");
    let chip = h.bounds("dispatch-chip-f2");
    let status = h.bounds("dispatch-status-f2");
    assert!(title.size.width > px(0.0), "the title is drawn");
    assert!(chip.origin.x > title.origin.x, "the chip follows the title");
    assert!(
        status.origin.x > chip.origin.x,
        "the status follows the chip"
    );
}

#[gpui::test]
fn stop_arms_then_sends_stop_dispatch_run(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = listed(cx, &dir);
    h.sent();

    h.click_on("dispatch-stop-f2");
    assert!(stops(&h.sent()).is_empty(), "the first press only arms");
    assert!(stop_armed(&mut h), "Confirm stop is drawn");

    h.click_on("dispatch-stop-confirm-f2");
    let sent = stops(&h.sent());
    assert!(
        matches!(sent.as_slice(), [(run_id, Some(_))] if run_id == "r1"),
        "the confirm sends one stop with a request id: {sent:?}"
    );
    assert!(!stop_armed(&mut h), "the confirm disarms");
}

#[gpui::test]
fn cancel_disarms_the_run_stop(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = listed(cx, &dir);
    h.sent();
    h.click_on("dispatch-stop-f2");
    assert!(stop_armed(&mut h), "precondition: armed");

    h.click_on("dispatch-stop-cancel-f2");

    assert!(!stop_armed(&mut h), "Cancel disarms");
    assert!(stops(&h.sent()).is_empty(), "Cancel sends nothing");
}

#[gpui::test]
fn stop_goes_to_a_remote_runs_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = fixture("r1");
    fixture.tabs = vec![tab(
        "t1",
        &split(
            SplitDirection::Horizontal,
            pane("p1", Some("s1")),
            GridNode::Pane {
                pane_id: "f2".to_owned(),
                session_id: None,
                host: Some(REMOTE.to_owned()),
                follow: Some(PaneFollow::DispatchRun {
                    run_id: "r1".to_owned(),
                }),
            },
        ),
    )];
    let mut h = Harness::with(cx, &dir, &fixture);
    let target = Target::tailnet(REMOTE, None);
    h.host(&target)
        .connect_with_features(&[protocol::FEATURE_DISPATCH_RUNS]);
    h.host(&target).send(DaemonMessage::DispatchRuns {
        runs: vec![r1().build()],
    });
    h.sent();
    h.host(&target).sent();

    h.click_on("dispatch-stop-f2");
    h.click_on("dispatch-stop-confirm-f2");

    let remote = stops(&h.host(&target).sent());
    assert!(
        matches!(remote.as_slice(), [(run_id, Some(_))] if run_id == "r1"),
        "the run's host gets the stop: {remote:?}"
    );
    assert!(stops(&h.sent()).is_empty(), "This PC gets no stop");
}

#[gpui::test]
fn stop_is_disabled_while_the_run_is_stopping(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = connected(cx, &dir, "r1");
    h.send(DaemonMessage::DispatchRuns {
        runs: vec![r1().status(DispatchRunStatus::Stopping).build()],
    });
    h.sent();

    h.click_on("dispatch-stop-f2");
    h.click_on("dispatch-stop-f2");

    assert!(!stop_armed(&mut h), "a stopping run's Stop never arms");
    assert!(stops(&h.sent()).is_empty(), "nothing is sent");
}

#[gpui::test]
fn a_failed_stop_shows_the_action_failed_notice(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = listed(cx, &dir);
    h.sent();
    h.click_on("dispatch-stop-f2");
    h.click_on("dispatch-stop-confirm-f2");
    let request_id = stops(&h.sent())
        .into_iter()
        .find_map(|(_, request_id)| request_id)
        .expect("the confirm sends a stop with a request id");
    assert!(
        h.root(|root, _| root.action_failed().is_none()),
        "precondition: no notice"
    );

    h.send(DaemonMessage::ActionFailed {
        title: "Could not stop the run".to_owned(),
        detail: "The run folder is gone.".to_owned(),
        hint: None,
        request_id: Some(request_id),
    });

    assert!(
        h.root(|root, _| root.action_failed().is_some()),
        "the failure shows as other action failures do"
    );
}

#[gpui::test]
fn escape_disarms_the_run_stop(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = listed(cx, &dir);
    // The run's pane holds the keyboard.
    h.click_on("pane-follow-f2");
    h.click_on("dispatch-stop-f2");
    assert!(stop_armed(&mut h), "precondition: armed");

    h.keys("escape");

    assert!(!stop_armed(&mut h), "Escape disarms");
}

#[gpui::test]
fn escape_after_the_armed_run_ended_is_not_swallowed(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = listed(cx, &dir);
    // Stacked, so each header is wide enough to keep its buttons.
    h.send(DaemonMessage::Tabs {
        tabs: vec![tab(
            "t1",
            &split(
                SplitDirection::Vertical,
                pane("p1", Some("s1")),
                follow_pane("f2", "r1"),
            ),
        )],
    });
    h.answer_scrollback("s1", b"");
    h.sent();
    // Pane `p1` shows `s1`'s chat, a turn running.
    h.click_on("pane-view-chat-p1");
    let load = h.sent().into_iter().find_map(|msg| match msg {
        ClientMessage::LoadChat {
            session_id,
            request_id,
        } if session_id == "s1" => request_id,
        _ => None,
    });
    h.send(DaemonMessage::Chat {
        session_id: "s1".to_owned(),
        entries: Vec::new(),
        hidden: 0,
        screen: ChatScreen {
            composer: ComposerState::Ready,
            prompt: None,
        },
        request_id: load,
    });
    h.send(DaemonMessage::ChatScreenChanged {
        session_id: "s1".to_owned(),
        screen: ChatScreen {
            composer: ComposerState::Working,
            prompt: None,
        },
    });
    // The chat's composer holds the keyboard.
    h.click_on("chat-composer");
    h.click_on("dispatch-stop-f2");
    assert!(stop_armed(&mut h), "precondition: armed");
    h.send(DaemonMessage::DispatchRunRemoved {
        run_id: "r1".to_owned(),
    });
    h.sent();

    h.keys("escape");

    assert!(!stop_armed(&mut h), "the stale arm is cleared");
    assert_eq!(
        h.sent_input("s1"),
        b"\x1b".to_vec(),
        "the Escape goes on to interrupt the chat's turn"
    );
}
