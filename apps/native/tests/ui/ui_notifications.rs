//! Notification specs: what an attention event notifies, the Needs you
//! toggle, when a notification for a session on screen is skipped, and the
//! Settings modal's Notifications tab.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use gpui::{TestAppContext, px};
use protocol::{
    AlertKind, AttentionReason, DaemonMessage, KeyState, SessionSnapshot, SummarySource, TabEntry,
};
use rustling_tulip_native::NotifyState;
use std::time::Duration;
use support::{Fixture, Harness, Opened, TestDir, pane, repo, session, summarizer_status, tab};

fn attention(id: &str, reason: AttentionReason) -> DaemonMessage {
    DaemonMessage::Attention {
        session_id: id.to_owned(),
        reason,
    }
}

fn updated(session: SessionSnapshot) -> DaemonMessage {
    DaemonMessage::SessionUpdated {
        session,
        request_id: None,
    }
}

/// The `WaitingAlert` worded by `kind` that ends `id`'s held Finished toast.
fn alert(id: &str, kind: AlertKind, notification: &str) -> DaemonMessage {
    DaemonMessage::WaitingAlert {
        session_id: id.to_owned(),
        kind: Some(kind),
        notification: notification.to_owned(),
        source: SummarySource::Model,
    }
}

/// The template `WaitingAlert` about `id`, which carries no kind to word a
/// title.
fn template_alert(id: &str, notification: &str) -> DaemonMessage {
    DaemonMessage::WaitingAlert {
        session_id: id.to_owned(),
        kind: None,
        notification: notification.to_owned(),
        source: SummarySource::Template,
    }
}

/// Delivers `id`'s turn: `label` working, then idle, so the turn ends.
fn turn(h: &mut Harness<'_>, id: &str, label: &str) {
    h.send(updated(
        session(id).user_label(label).status("working").build(),
    ));
    h.send(updated(session(id).user_label(label).build()));
}

fn shown(title: &str, body: &str) -> (String, String) {
    (title.to_owned(), body.to_owned())
}

/// The harness on session `s1`, labelled `fix login`, in pane `p1`, plus
/// the unshown `s2`, labelled `review`.
fn opened<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut fixture = Fixture::single(session("s1").user_label("fix login").build());
    fixture
        .sessions
        .push(session("s2").user_label("review").build());
    let mut h = Harness::with(cx, dir, &fixture);
    h.answer_scrollback("s1", b"");
    h.sent();
    h
}

#[gpui::test]
fn awaiting_input_attention_notifies_needs_you_with_label(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.send(attention("s1", AttentionReason::AwaitingInput));
    assert_eq!(
        h.notified(),
        [shown("Needs you", "fix login: Waiting for input")]
    );
}

#[gpui::test]
fn an_excluded_session_still_notifies_needs_you(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut excluded = session("s1").user_label("dev server").build();
    excluded.busy_excluded = true;
    let mut h = Harness::with(cx, &dir, &Fixture::single(excluded));
    h.answer_scrollback("s1", b"");
    h.sent();
    h.send(attention("s1", AttentionReason::AwaitingInput));
    assert_eq!(
        h.notified(),
        [shown("Needs you", "dev server: Waiting for input")],
        "\"Don't count as busy\" leaves OS notifications alone"
    );
}

#[gpui::test]
fn stopped_and_error_attention_raise_no_toast(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    for id in ["s1", "s2"] {
        h.send(attention(id, AttentionReason::Stopped));
        h.send(attention(id, AttentionReason::Error));
    }
    assert!(
        h.notified().is_empty(),
        "an exited or errored session raises no notification"
    );
}

#[gpui::test]
fn a_turn_ending_notifies_finished_with_label(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.send(updated(
        session("s2").user_label("review").status("working").build(),
    ));
    h.send(updated(session("s2").user_label("review").build()));
    assert_eq!(
        h.notified(),
        [shown("Finished", "review")],
        "only the idle update notifies"
    );
}

#[gpui::test]
fn a_turn_ending_with_background_work_waits(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.send(updated(
        session("s2").user_label("review").status("working").build(),
    ));
    let mut holding = session("s2").user_label("review").build();
    holding.background = true;
    h.send(updated(holding));
    assert!(h.notified().is_empty(), "the background work still runs");
    h.send(updated(session("s2").user_label("review").build()));
    assert_eq!(
        h.notified(),
        [shown("Finished", "review")],
        "the last of the work is done"
    );
}

#[gpui::test]
fn a_relist_never_notifies_finished(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.send(updated(
        session("s2").user_label("review").status("working").build(),
    ));
    h.send(DaemonMessage::Sessions {
        sessions: vec![
            session("s1").user_label("fix login").build(),
            session("s2").user_label("review").build(),
        ],
    });
    assert!(
        h.notified().is_empty(),
        "a relist says nothing about a turn"
    );
}

#[gpui::test]
fn finished_toggle_off_silences_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    save_notifications(&dir, &serde_json::json!({ "finished": false }));
    let mut h = opened(cx, &dir);
    h.send(updated(
        session("s2").user_label("review").status("working").build(),
    ));
    h.send(updated(session("s2").user_label("review").build()));
    assert!(h.notified().is_empty(), "the toggle is off");
    h.send(attention("s2", AttentionReason::AwaitingInput));
    assert_eq!(
        h.notified(),
        [shown("Needs you", "review: Waiting for input")],
        "Needs you still fires"
    );
}

#[gpui::test]
fn finished_skips_a_session_on_screen(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_tabs(cx, &dir);
    focus_window(&mut h);
    h.send(updated(
        session("s1")
            .user_label("fix login")
            .status("working")
            .build(),
    ));
    h.send(updated(session("s1").user_label("fix login").build()));
    assert!(
        h.notified().is_empty(),
        "the owner watched that turn finish"
    );
}

fn saved_ui(dir: &TestDir) -> serde_json::Value {
    let text = std::fs::read_to_string(dir.path().join("native-ui.json")).expect("a saved layout");
    serde_json::from_str(&text).expect("native-ui.json is JSON")
}

/// The harness on [`opened`] with Settings open on its Notifications tab.
fn on_notifications_tab<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = opened(cx, dir);
    h.keys("ctrl-,");
    h.click_on("settings-tab-notifications");
    assert_eq!(h.root(|root, _| root.settings_tab()), "Notifications");
    h
}

#[gpui::test]
fn notifications_tab_has_needs_you_and_finished_toggles(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_notifications_tab(cx, &dir);
    for selector in ["settings-notify-needs-you", "settings-notify-finished"] {
        assert!(
            h.bounds(selector).origin.x >= px(0.0),
            "{selector} is painted"
        );
    }
    for gone in ["settings-notify-stopped", "settings-notify-error"] {
        assert!(
            h.bounds(gone).origin.x < px(0.0),
            "{gone} left the Notifications tab"
        );
    }

    h.click_on("settings-notify-needs-you");
    let saved = saved_ui(&dir);
    assert_eq!(saved["notifications"]["awaiting_input"], false);
    assert_eq!(
        saved["notifications"]["finished"], true,
        "the Finished toggle is untouched"
    );
    assert!(saved["notifications"].get("stopped").is_none());
    assert!(saved["notifications"].get("error").is_none());

    h.click_on("settings-notify-finished");
    assert_eq!(saved_ui(&dir)["notifications"]["finished"], false);

    h.send(attention("s2", AttentionReason::AwaitingInput));
    assert!(h.notified().is_empty(), "the toggle took effect at once");

    assert_eq!(settings_focus(&mut h), Some("settings-notify-finished"));
    for stop in [
        "settings-notify-skip-on-screen",
        "settings-notify-skip-focused",
        "settings-notify-skip-never",
    ] {
        h.keys("tab");
        assert_eq!(settings_focus(&mut h), Some(stop), "each choice is a stop");
    }
    h.keys("tab tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-open-windows-notifications"),
        "past the tab list, round to the link"
    );
    h.keys("tab space");
    assert_eq!(settings_focus(&mut h), Some("settings-notify-needs-you"));
    assert_eq!(saved_ui(&dir)["notifications"]["awaiting_input"], true);
}

fn settings_focus(h: &mut Harness<'_>) -> Option<&'static str> {
    let root = h.root.clone();
    h.cx.update(|window, cx| root.read(cx).settings_focus(window, cx))
}

#[gpui::test]
fn skip_choice_is_saved(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_notifications_tab(cx, &dir);
    for selector in [
        "settings-notify-skip-on-screen",
        "settings-notify-skip-focused",
        "settings-notify-skip-never",
    ] {
        assert!(
            h.bounds(selector).origin.x >= px(0.0),
            "{selector} is painted"
        );
    }
    h.click_on("settings-notify-skip-focused");
    assert_eq!(saved_ui(&dir)["notifications"]["skip_when"], "focused");
}

/// Writes `notifications` as the saved Notifications settings.
fn save_notifications(dir: &TestDir, notifications: &serde_json::Value) {
    let ui = serde_json::json!({ "notifications": notifications });
    std::fs::write(dir.path().join("native-ui.json"), ui.to_string())
        .expect("write native-ui.json");
}

/// Activates the test window, which starts inactive, as the desktop does
/// when the owner clicks into it.
fn focus_window(h: &mut Harness<'_>) {
    h.cx.update(|window, _| window.activate_window());
    h.cx.run_until_parked();
    assert!(
        h.cx.update(|window, _| window.is_window_active()),
        "the window is focused"
    );
}

/// The harness on `s1` (`fix login`) in tab `t1` and `s2` (`review`) in
/// tab `t2`, with `t1` on screen.
fn two_tabs<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let fixture = Fixture {
        sessions: vec![
            session("s1").user_label("fix login").build(),
            session("s2").user_label("review").build(),
        ],
        tabs: vec![
            tab("t1", &pane("p1", Some("s1"))),
            tab("t2", &pane("p2", Some("s2"))),
        ],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, dir, &fixture);
    h.answer_scrollback("s1", b"");
    h.sent();
    assert_eq!(
        h.root(|root, _| root.active_tab_id().map(str::to_owned)),
        Some("t1".to_owned())
    );
    h
}

#[gpui::test]
fn focused_window_and_visible_pane_skips_needs_you(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_tabs(cx, &dir);
    focus_window(&mut h);
    h.send(attention("s1", AttentionReason::AwaitingInput));
    assert!(
        h.notified().is_empty(),
        "the owner is looking at s1 already"
    );
}

#[gpui::test]
fn settings_open_over_the_pane_still_notifies(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_tabs(cx, &dir);
    focus_window(&mut h);
    h.keys("ctrl-,");
    assert!(h.root(|root, _| root.settings_open()), "Settings is open");
    h.send(attention("s1", AttentionReason::AwaitingInput));
    assert_eq!(
        h.notified(),
        [shown("Needs you", "fix login: Waiting for input")],
        "the Settings modal hides s1's pane"
    );
}

#[gpui::test]
fn focused_window_other_tab_still_notifies(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_tabs(cx, &dir);
    focus_window(&mut h);
    h.send(attention("s2", AttentionReason::AwaitingInput));
    assert_eq!(
        h.notified(),
        [shown("Needs you", "review: Waiting for input")]
    );
}

#[gpui::test]
fn unfocused_window_notifies(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_tabs(cx, &dir);
    assert!(
        !h.cx.update(|window, _| window.is_window_active()),
        "the test window starts inactive"
    );
    h.send(attention("s1", AttentionReason::AwaitingInput));
    assert_eq!(
        h.notified(),
        [shown("Needs you", "fix login: Waiting for input")]
    );
}

#[gpui::test]
fn window_focused_choice_skips_a_session_on_another_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    save_notifications(&dir, &serde_json::json!({ "skip_when": "focused" }));
    let mut h = two_tabs(cx, &dir);
    focus_window(&mut h);
    h.send(attention("s2", AttentionReason::AwaitingInput));
    assert!(h.notified().is_empty(), "any session, window focused");
}

#[gpui::test]
fn never_choice_notifies_when_focused_and_visible(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    save_notifications(&dir, &serde_json::json!({ "skip_when": "never" }));
    let mut h = two_tabs(cx, &dir);
    focus_window(&mut h);
    h.send(attention("s1", AttentionReason::AwaitingInput));
    assert_eq!(
        h.notified(),
        [shown("Needs you", "fix login: Waiting for input")]
    );
}

#[gpui::test]
fn diff_tab_active_counts_as_not_on_screen(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let diff: TabEntry = serde_json::from_value(serde_json::json!({
        "id": "d1",
        "name": "a.rs",
        "content": { "kind": "diff", "repo_id": "r1", "path": "a.rs", "against": null },
        "created_at": "2026-01-01T00:00:00Z",
    }))
    .expect("diff tab fixture");
    let fixture = Fixture {
        repos: vec![repo("r1", "D:/r1")],
        sessions: vec![session("s1").user_label("fix login").build()],
        tabs: vec![tab("t1", &pane("p1", Some("s1"))), diff],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.answer_scrollback("s1", b"");
    h.click_on("tab-d1");
    assert_eq!(
        h.root(|root, _| root.active_tab_id().map(str::to_owned)),
        Some("d1".to_owned())
    );
    focus_window(&mut h);
    h.send(attention("s1", AttentionReason::AwaitingInput));
    assert_eq!(
        h.notified(),
        [shown("Needs you", "fix login: Waiting for input")],
        "a diff tab shows no session"
    );
}

#[gpui::test]
fn notifications_tab_shows_windows_state(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.set_notify_state(NotifyState::Blocked);
    h.keys("ctrl-,");
    h.click_on("settings-tab-notifications");
    h.cx.run_until_parked();
    assert_eq!(
        h.root(|root, _| root.notifications_state_line()),
        "Windows notifications: blocked by Windows"
    );
    assert!(h.bounds("settings-notify-state").origin.x >= px(0.0));

    h.set_notify_state(NotifyState::On);
    h.click_on("settings-tab-general");
    h.click_on("settings-tab-notifications");
    h.cx.run_until_parked();
    assert_eq!(
        h.root(|root, _| root.notifications_state_line()),
        "Windows notifications: on",
        "read again when the tab opens"
    );
}

#[gpui::test]
fn open_windows_settings_uses_opener(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_notifications_tab(cx, &dir);
    h.click_on("settings-open-windows-notifications");
    assert_eq!(
        h.opened(),
        [Opened::Url("ms-settings:notifications".to_owned())]
    );
}

#[gpui::test]
fn unlabelled_session_body_is_product_name(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("s1").user_label("fix login").build());
    fixture
        .sessions
        .push(session("bare").label("").shell("").build());
    let mut h = Harness::with(cx, &dir, &fixture);
    h.send(attention("bare", AttentionReason::AwaitingInput));
    h.send(attention("ghost", AttentionReason::AwaitingInput));
    assert_eq!(
        h.notified(),
        [
            shown("Needs you", "rustling-tulip: Waiting for input"),
            shown("Needs you", "rustling-tulip: Waiting for input"),
        ]
    );
}

#[gpui::test]
fn finished_waits_for_its_alert_and_takes_its_title_and_body(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.send(summarizer_status().build());
    turn(&mut h, "s2", "review");
    assert!(
        h.notified().is_empty(),
        "the toast waits for the turn's alert"
    );
    h.send(alert(
        "s2",
        AlertKind::DoneWaiting,
        "tests pass, ready for review",
    ));
    assert_eq!(
        h.notified(),
        [shown("review finished", "tests pass, ready for review")]
    );
}

#[gpui::test]
fn finished_title_follows_the_alert_kind(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.send(summarizer_status().build());
    for (kind, title) in [
        (AlertKind::NeedsAnswer, "review needs your answer"),
        (AlertKind::DoneWaiting, "review finished"),
        (AlertKind::WorkingUpdate, "review posted an update"),
    ] {
        turn(&mut h, "s2", "review");
        h.send(alert("s2", kind, "the turn's news"));
        assert_eq!(
            h.notified().last(),
            Some(&shown(title, "the turn's news")),
            "the kind words the title"
        );
    }
}

#[gpui::test]
fn finished_falls_back_after_the_hold(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.send(summarizer_status().build());
    turn(&mut h, "s2", "review");
    h.advance(Duration::from_secs(11));
    assert!(
        h.notified().is_empty(),
        "eleven seconds in, the alert may still come"
    );
    h.advance(Duration::from_secs(1));
    assert_eq!(h.notified(), [shown("Finished", "review")]);
}

#[gpui::test]
fn a_late_alert_after_the_fallback_raises_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.send(summarizer_status().build());
    turn(&mut h, "s2", "review");
    h.advance(Duration::from_secs(12));
    assert_eq!(h.notified(), [shown("Finished", "review")]);
    h.send(alert("s2", AlertKind::DoneWaiting, "tests pass"));
    assert_eq!(
        h.notified(),
        [shown("Finished", "review")],
        "the fallback already went, and no second toast follows"
    );
}

#[gpui::test]
fn finished_without_summaries_raises_at_once(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    turn(&mut h, "s2", "review");
    assert_eq!(
        h.notified(),
        [shown("Finished", "review")],
        "no status has arrived, so nothing is held"
    );

    h.send(summarizer_status().enabled(false).build());
    turn(&mut h, "s2", "review");
    assert_eq!(
        h.notified().last(),
        Some(&shown("Finished", "review")),
        "summaries are off"
    );

    h.send(summarizer_status().cap_reached(true).build());
    turn(&mut h, "s2", "review");
    assert_eq!(
        h.notified().last(),
        Some(&shown("Finished", "review")),
        "the day's cap is reached"
    );

    h.send(summarizer_status().key(KeyState::Rejected).build());
    turn(&mut h, "s2", "review");
    assert_eq!(
        h.notified().last(),
        Some(&shown("Finished", "review")),
        "the provider refused the key, so no summary can come"
    );
}

#[gpui::test]
fn finished_with_a_template_alert_uses_the_fallback(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.send(summarizer_status().build());
    turn(&mut h, "s2", "review");
    h.send(template_alert("s2", "Turn finished: edited two files"));
    assert_eq!(
        h.notified(),
        [shown("Finished", "review")],
        "a template alert words no title, so the plain toast is raised"
    );
}

#[gpui::test]
fn an_alert_for_another_session_leaves_the_hold(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.send(summarizer_status().build());
    turn(&mut h, "s2", "review");
    h.send(alert("s1", AlertKind::DoneWaiting, "s1's news"));
    assert!(h.notified().is_empty(), "s1 holds no toast");
    h.send(alert("s2", AlertKind::DoneWaiting, "s2's news"));
    assert_eq!(h.notified(), [shown("review finished", "s2's news")]);
}

#[gpui::test]
fn a_held_finished_toast_honours_the_toggle_and_skip_rule(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    save_notifications(&dir, &serde_json::json!({ "finished": false }));
    {
        let mut h = opened(cx, &dir);
        h.send(summarizer_status().build());
        turn(&mut h, "s2", "review");
        h.advance(Duration::from_secs(12));
        assert!(
            h.notified().is_empty(),
            "the toggle is off when the fallback goes"
        );
        h.send(alert("s2", AlertKind::DoneWaiting, "news"));
        assert!(h.notified().is_empty(), "and off when an alert words it");
    }

    save_notifications(&dir, &serde_json::json!({}));
    {
        let mut h = two_tabs(cx, &dir);
        focus_window(&mut h);
        h.send(summarizer_status().build());
        turn(&mut h, "s1", "fix login");
        h.send(alert("s1", AlertKind::DoneWaiting, "news"));
        assert!(
            h.notified().is_empty(),
            "the owner is looking at that session"
        );
        turn(&mut h, "s1", "fix login");
        h.advance(Duration::from_secs(12));
        assert!(
            h.notified().is_empty(),
            "the skip rule holds the fallback back too"
        );
    }
}

#[gpui::test]
fn a_session_that_works_again_drops_its_held_toast(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.send(summarizer_status().build());
    turn(&mut h, "s2", "review");
    h.send(updated(
        session("s2").user_label("review").status("working").build(),
    ));
    h.advance(Duration::from_secs(12));
    assert!(
        h.notified().is_empty(),
        "the turn went on, so the hold went with it"
    );
    h.send(alert("s2", AlertKind::DoneWaiting, "news"));
    assert!(
        h.notified().is_empty(),
        "the alert has no toast left to word"
    );
}

#[gpui::test]
fn a_reconnect_drops_held_toasts_and_the_status(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.send(summarizer_status().build());
    turn(&mut h, "s2", "review");
    h.send_welcome(support::PROTOCOL);
    h.sent();
    h.advance(Duration::from_secs(12));
    assert!(
        h.notified().is_empty(),
        "the held toast went with the old connection"
    );
    turn(&mut h, "s2", "review");
    assert_eq!(
        h.notified(),
        [shown("Finished", "review")],
        "the new connection's status says nothing yet, so the toast is raised at once"
    );
}

#[gpui::test]
fn needs_you_body_is_the_label_and_prompt_line(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("s1").user_label("fix login").build());
    for pending in [
        (
            "s2",
            "review",
            serde_json::json!({ "kind": "permission", "tool_name": "Bash", "summary": "cargo test" }),
        ),
        (
            "s3",
            "docs",
            serde_json::json!({ "kind": "question", "questions": [{ "header": "Scope", "question": "Which repo?" }] }),
        ),
        (
            "s4",
            "plan",
            serde_json::json!({ "kind": "plan_approval", "plan": "1. Edit the file\n2. Run the tests" }),
        ),
    ] {
        fixture.sessions.push(
            session(pending.0)
                .user_label(pending.1)
                .pending_input(pending.2.clone())
                .status("awaiting_input")
                .build(),
        );
    }
    let mut h = Harness::with(cx, &dir, &fixture);
    h.answer_scrollback("s1", b"");
    h.sent();
    for (id, line) in [
        ("s2", "review: Allow Bash: cargo test"),
        ("s3", "docs: Scope: Which repo?"),
        ("s4", "plan: Approve plan: 1. Edit the file"),
    ] {
        h.send(attention(id, AttentionReason::AwaitingInput));
        assert_eq!(
            h.notified().last(),
            Some(&shown("Needs you", line)),
            "the prompt line follows the label"
        );
    }
}

#[gpui::test]
fn needs_you_without_a_prompt_says_waiting_for_input(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.send(attention("s2", AttentionReason::AwaitingInput));
    assert_eq!(
        h.notified(),
        [shown("Needs you", "review: Waiting for input")],
        "no hook has said what it waits for"
    );
}

#[gpui::test]
fn a_headless_or_agentless_finished_toast_is_not_held(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.send(summarizer_status().build());

    h.send(updated(
        session("s2")
            .user_label("review")
            .headless()
            .status("working")
            .build(),
    ));
    h.send(updated(
        session("s2").user_label("review").headless().build(),
    ));
    assert_eq!(
        h.notified(),
        [shown("Finished", "review")],
        "no model writes a summary of a headless turn"
    );

    h.send(updated(
        session("s2")
            .user_label("review")
            .no_agent()
            .status("working")
            .build(),
    ));
    h.send(updated(
        session("s2").user_label("review").no_agent().build(),
    ));
    assert_eq!(
        h.notified(),
        [shown("Finished", "review"), shown("Finished", "review")],
        "a session with no agent is summarized by nobody"
    );
}

#[gpui::test]
fn an_alert_before_background_work_clears_words_the_later_finished_toast(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.send(summarizer_status().build());
    h.send(updated(
        session("s2").user_label("review").status("working").build(),
    ));
    h.send(updated(
        session("s2").user_label("review").background().build(),
    ));
    h.send(alert("s2", AlertKind::DoneWaiting, "tests pass, ready"));
    assert!(
        h.notified().is_empty(),
        "the turn is not over while its background work runs"
    );
    h.send(updated(session("s2").user_label("review").build()));
    assert_eq!(
        h.notified(),
        [shown("review finished", "tests pass, ready")],
        "the alert that came early words the toast at once"
    );
}

#[gpui::test]
fn a_sessions_resync_drops_holds_for_sessions_that_moved_on(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.send(summarizer_status().build());
    turn(&mut h, "s1", "fix login");
    h.send(updated(
        session("s2").user_label("review").status("working").build(),
    ));
    h.send(updated(
        session("s2").user_label("review").background().build(),
    ));
    h.send(alert("s2", AlertKind::DoneWaiting, "s2's news"));
    h.send(DaemonMessage::Sessions {
        sessions: vec![
            session("s1")
                .user_label("fix login")
                .status("working")
                .build(),
            session("s2").user_label("review").status("working").build(),
        ],
    });
    h.advance(Duration::from_secs(12));
    assert!(
        h.notified().is_empty(),
        "s1 left Idle and s2 works again, so neither a hold nor a kept alert survives"
    );
    h.send(alert("s1", AlertKind::DoneWaiting, "s1's news"));
    assert!(
        h.notified().is_empty(),
        "the dropped hold has nothing left to word"
    );
    h.send(updated(session("s2").user_label("review").build()));
    h.advance(Duration::from_secs(12));
    assert_eq!(
        h.notified(),
        [shown("Finished", "review")],
        "the resync dropped s2's kept alert, so the plain fallback stands"
    );
}

#[gpui::test]
fn needs_you_ignores_the_waiting_alert(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.send(summarizer_status().build());
    h.send(attention("s2", AttentionReason::AwaitingInput));
    assert_eq!(
        h.notified(),
        [shown("Needs you", "review: Waiting for input")]
    );
    h.send(alert(
        "s2",
        AlertKind::NeedsAnswer,
        "the agent asks which repo",
    ));
    assert_eq!(
        h.notified(),
        [shown("Needs you", "review: Waiting for input")],
        "an alert words no Needs you toast"
    );
}
