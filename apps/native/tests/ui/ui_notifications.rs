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
use rustling_tulip_native::{BundledSound, ClipSource, NotifyKind, NotifyState};
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

/// Waits out the grace period a Finished toast is held for.
fn grace(h: &mut Harness<'_>) {
    h.advance(Duration::from_secs(5));
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
    grace(&mut h);
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
    grace(&mut h);
    assert_eq!(
        h.notified(),
        [shown("Finished", "review")],
        "the last of the work is done"
    );
}

#[gpui::test]
fn a_finished_toast_waits_out_the_grace(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    turn(&mut h, "s2", "review");
    h.advance(Duration::from_secs(4));
    assert!(h.notified().is_empty(), "the agent may start again");
    h.advance(Duration::from_secs(1));
    assert_eq!(h.notified(), [shown("Finished", "review")]);
}

#[gpui::test]
fn working_again_inside_the_grace_raises_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    turn(&mut h, "s2", "review");
    h.advance(Duration::from_secs(1));
    h.send(updated(
        session("s2").user_label("review").status("working").build(),
    ));
    h.advance(Duration::from_secs(12));
    assert!(h.notified().is_empty(), "the turn went on");
}

#[gpui::test]
fn each_idle_starts_its_own_grace(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    turn(&mut h, "s2", "review");
    h.advance(Duration::from_secs(3));
    turn(&mut h, "s2", "review");
    h.advance(Duration::from_secs(3));
    assert!(
        h.notified().is_empty(),
        "the first idle's timer raises nothing for the second"
    );
    h.advance(Duration::from_secs(2));
    assert_eq!(h.notified(), [shown("Finished", "review")]);
}

#[gpui::test]
fn background_work_inside_the_grace_raises_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    turn(&mut h, "s2", "review");
    h.send(updated(
        session("s2").user_label("review").background().build(),
    ));
    h.advance(Duration::from_secs(12));
    assert!(
        h.notified().is_empty(),
        "the screen saw the background shell after it read Idle"
    );
    h.send(updated(session("s2").user_label("review").build()));
    grace(&mut h);
    assert_eq!(
        h.notified(),
        [shown("Finished", "review")],
        "the toast waits for the work to end"
    );
}

#[gpui::test]
fn an_alert_inside_the_grace_words_the_toast_when_it_ends(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.send(summarizer_status().build());
    turn(&mut h, "s2", "review");
    h.send(alert("s2", AlertKind::DoneWaiting, "tests pass"));
    assert!(
        h.notified().is_empty(),
        "an alert does not cut the grace short"
    );
    grace(&mut h);
    assert_eq!(h.notified(), [shown("review finished", "tests pass")]);
}

#[gpui::test]
fn an_alert_after_the_grace_words_the_toast_at_once(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.send(summarizer_status().build());
    turn(&mut h, "s2", "review");
    h.advance(Duration::from_secs(8));
    assert!(h.notified().is_empty(), "the toast waits for its alert");
    h.send(alert("s2", AlertKind::DoneWaiting, "tests pass"));
    assert_eq!(h.notified(), [shown("review finished", "tests pass")]);
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
    grace(&mut h);
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
    grace(&mut h);
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
        "settings-finished-sound-field",
        "settings-finished-sound-play",
        "settings-notify-skip-on-screen",
        "settings-notify-skip-focused",
        "settings-notify-skip-never",
        #[cfg(windows)]
        "settings-toast-style-os",
        #[cfg(windows)]
        "settings-toast-style-app",
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
    grace(&mut h);
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
        grace(&mut h);
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
fn a_ten_second_grace_holds_the_toast_past_five(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    save_notifications(&dir, &serde_json::json!({ "finished_grace_secs": 10 }));
    let mut h = opened(cx, &dir);
    turn(&mut h, "s2", "review");
    h.advance(Duration::from_secs(5));
    assert!(
        h.notified().is_empty(),
        "five seconds is short of a ten second grace"
    );
    h.advance(Duration::from_secs(5));
    assert_eq!(h.notified(), [shown("Finished", "review")]);
}

#[gpui::test]
fn no_grace_raises_the_toast_at_once(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    save_notifications(&dir, &serde_json::json!({ "finished_grace_secs": 0 }));
    let mut h = opened(cx, &dir);
    turn(&mut h, "s2", "review");
    h.advance(Duration::from_millis(1));
    assert_eq!(
        h.notified(),
        [shown("Finished", "review")],
        "a zero grace raises the toast in the first instant"
    );
}

#[gpui::test]
fn a_grace_past_the_summary_hold_raises_at_the_grace(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    save_notifications(&dir, &serde_json::json!({ "finished_grace_secs": 20 }));
    let mut h = opened(cx, &dir);
    h.send(summarizer_status().build());
    turn(&mut h, "s2", "review");
    h.advance(Duration::from_secs(12));
    assert!(
        h.notified().is_empty(),
        "the twelve second hold is inside the twenty second grace"
    );
    h.advance(Duration::from_secs(8));
    assert_eq!(
        h.notified(),
        [shown("Finished", "review")],
        "the grace ended with no summary wait left after it"
    );
    h.send(alert("s2", AlertKind::DoneWaiting, "tests pass"));
    assert_eq!(
        h.notified(),
        [shown("Finished", "review")],
        "the toast already went, and no second toast follows"
    );
}

#[gpui::test]
fn finished_without_summaries_waits_only_for_the_grace(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    turn(&mut h, "s2", "review");
    grace(&mut h);
    assert_eq!(
        h.notified(),
        [shown("Finished", "review")],
        "no status has arrived, so no alert is waited for"
    );

    h.send(summarizer_status().enabled(false).build());
    turn(&mut h, "s2", "review");
    grace(&mut h);
    assert_eq!(
        h.notified().last(),
        Some(&shown("Finished", "review")),
        "summaries are off"
    );

    h.send(summarizer_status().cap_reached(true).build());
    turn(&mut h, "s2", "review");
    grace(&mut h);
    assert_eq!(
        h.notified().last(),
        Some(&shown("Finished", "review")),
        "the day's cap is reached"
    );

    h.send(summarizer_status().key(KeyState::Rejected).build());
    turn(&mut h, "s2", "review");
    grace(&mut h);
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
    grace(&mut h);
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
    grace(&mut h);
    assert_eq!(h.notified(), [shown("review finished", "s2's news")]);
}

#[gpui::test]
fn a_needs_answer_alert_plays_the_needs_you_sound(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.send(summarizer_status().build());
    turn(&mut h, "s2", "review");
    h.send(alert(
        "s2",
        AlertKind::NeedsAnswer,
        "the agent asks which repo",
    ));
    grace(&mut h);
    assert_eq!(
        h.notified(),
        [shown(
            "review needs your answer",
            "the agent asks which repo"
        )]
    );
    assert_eq!(
        h.notified_silent(),
        [true],
        "the alert's toast is raised silent"
    );
    assert_eq!(
        h.sounds(),
        [ClipSource::Bundled(BundledSound::MalletRisingFifth)],
        "an answer request plays the Needs you sound"
    );
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
        grace(&mut h);
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
    grace(&mut h);
    assert_eq!(
        h.notified(),
        [shown("Finished", "review")],
        "the new connection's status says nothing yet, so no alert is waited for"
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
fn a_headless_or_agentless_finished_toast_waits_for_no_alert(cx: &mut TestAppContext) {
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
    grace(&mut h);
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
    grace(&mut h);
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
    grace(&mut h);
    assert_eq!(
        h.notified(),
        [shown("review finished", "tests pass, ready")],
        "the alert that came early words the toast"
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

#[gpui::test]
fn the_needs_you_sound_plays_once_for_a_new_item(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.send(attention("s1", AttentionReason::AwaitingInput));
    assert_eq!(h.notified().len(), 1, "Needs you is raised");
    assert_eq!(
        h.sounds(),
        [ClipSource::Bundled(BundledSound::MalletRisingFifth)],
        "no saved choice plays the default"
    );

    turn(&mut h, "s2", "review");
    grace(&mut h);
    assert_eq!(h.notified().len(), 2, "Finished is raised");
    assert_eq!(
        h.sounds(),
        [
            ClipSource::Bundled(BundledSound::MalletRisingFifth),
            ClipSource::Bundled(BundledSound::MalletFallingFifth)
        ],
        "Finished plays the Finished sound"
    );

    h.send(attention("s2", AttentionReason::AwaitingInput));
    assert_eq!(
        h.sounds(),
        [
            ClipSource::Bundled(BundledSound::MalletRisingFifth),
            ClipSource::Bundled(BundledSound::MalletFallingFifth),
            ClipSource::Bundled(BundledSound::MalletRisingFifth)
        ],
        "the next Needs you item plays again"
    );
}

#[gpui::test]
fn no_sound_when_the_choice_is_off(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    save_notifications(&dir, &serde_json::json!({ "needs_you_sound": "off" }));
    let mut h = opened(cx, &dir);
    h.send(attention("s1", AttentionReason::AwaitingInput));
    assert_eq!(h.notified().len(), 1, "the toast still fires");
    assert!(h.sounds().is_empty(), "the sound is off");
}

#[gpui::test]
fn no_sound_while_windows_suppresses_notifications(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.set_notify_state(NotifyState::Blocked);
    h.send(attention("s1", AttentionReason::AwaitingInput));
    assert_eq!(
        h.notified().len(),
        1,
        "the toast is still handed to Windows"
    );
    assert!(
        h.sounds().is_empty(),
        "no sound while this app's toasts are off"
    );

    h.set_notify_state(NotifyState::On);
    h.set_do_not_disturb(true);
    h.send(attention("s2", AttentionReason::AwaitingInput));
    assert_eq!(
        h.notified().len(),
        2,
        "the toast is still handed to Windows"
    );
    assert!(h.sounds().is_empty(), "no sound under Do Not Disturb");
}

#[gpui::test]
fn no_sound_when_the_skip_rule_skips_the_toast(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_tabs(cx, &dir);
    focus_window(&mut h);
    h.send(attention("s1", AttentionReason::AwaitingInput));
    assert!(
        h.notified().is_empty(),
        "the owner is looking at s1 already"
    );
    assert!(h.sounds().is_empty(), "nor does it sound");
}

#[gpui::test]
fn no_sound_when_the_needs_you_notification_is_off(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    save_notifications(&dir, &serde_json::json!({ "awaiting_input": false }));
    let mut h = opened(cx, &dir);
    h.send(attention("s1", AttentionReason::AwaitingInput));
    assert!(h.notified().is_empty(), "the toggle is off");
    assert!(h.sounds().is_empty(), "and the sound with it");
}

#[gpui::test]
fn a_missing_custom_file_falls_back_to_the_default(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let missing = dir.path().join("no-such-chime.ogg");
    save_notifications(
        &dir,
        &serde_json::json!({ "needs_you_sound": { "custom": missing } }),
    );
    let mut h = opened(cx, &dir);
    h.send(attention("s1", AttentionReason::AwaitingInput));
    assert_eq!(
        h.sounds(),
        [ClipSource::Bundled(BundledSound::MalletRisingFifth)]
    );
}

/// Asserts the Needs you toast `s1` raises under `sound` is raised silent.
fn assert_needs_you_silent_under(cx: &mut TestAppContext, sound: &serde_json::Value, choice: &str) {
    let dir = TestDir::new();
    save_notifications(&dir, &serde_json::json!({ "needs_you_sound": sound }));
    let mut h = opened(cx, &dir);
    h.send(attention("s1", AttentionReason::AwaitingInput));
    assert_eq!(
        h.notified().len(),
        1,
        "Needs you is raised with the {choice} choice"
    );
    assert_eq!(
        h.notified_silent(),
        [true],
        "a Needs you toast is raised silent with the {choice} choice"
    );
}

#[gpui::test]
fn a_needs_you_toast_is_raised_silent_whatever_the_sound_choice(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let missing = dir.path().join("no-such-chime.ogg");
    assert_needs_you_silent_under(cx, &serde_json::json!("off"), "off");
    assert_needs_you_silent_under(cx, &serde_json::json!("default"), "default");
    assert_needs_you_silent_under(
        cx,
        &serde_json::json!({ "custom": missing }),
        "a missing custom file",
    );
}

#[gpui::test]
fn a_finished_toast_is_raised_silent_and_plays_the_finished_sound(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    turn(&mut h, "s2", "review");
    grace(&mut h);
    assert_eq!(h.notified().len(), 1, "Finished is raised");
    assert_eq!(
        h.notified_silent(),
        [true],
        "a Finished toast is raised silent"
    );
    assert_eq!(
        h.sounds(),
        [ClipSource::Bundled(BundledSound::MalletFallingFifth)]
    );
}

#[gpui::test]
fn no_finished_sound_when_its_choice_is_off(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    save_notifications(&dir, &serde_json::json!({ "finished_sound": "off" }));
    let mut h = opened(cx, &dir);
    turn(&mut h, "s2", "review");
    grace(&mut h);
    assert_eq!(h.notified().len(), 1, "the toast still fires");
    assert_eq!(h.notified_silent(), [true], "and is raised silent");
    assert!(h.sounds().is_empty(), "the sound is off");
}

#[gpui::test]
fn the_finished_and_needs_you_sounds_are_chosen_apart(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    save_notifications(
        &dir,
        &serde_json::json!({ "needs_you_sound": "off", "finished_sound": "default" }),
    );
    let mut h = opened(cx, &dir);
    h.send(attention("s1", AttentionReason::AwaitingInput));
    assert_eq!(h.notified().len(), 1, "Needs you is raised");
    assert!(h.sounds().is_empty(), "its sound is off");
    turn(&mut h, "s2", "review");
    grace(&mut h);
    assert_eq!(
        h.sounds(),
        [ClipSource::Bundled(BundledSound::MalletFallingFifth)],
        "Finished plays its own sound"
    );
}

#[gpui::test]
fn a_held_finished_toast_plays_the_finished_sound_when_it_expires(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.send(summarizer_status().build());
    turn(&mut h, "s2", "review");
    assert!(
        h.sounds().is_empty(),
        "nothing plays while an alert may still come"
    );
    h.advance(Duration::from_secs(12));
    assert_eq!(h.notified().len(), 1, "the fallback is raised");
    assert_eq!(h.notified_silent(), [true], "and is raised silent");
    assert_eq!(
        h.sounds(),
        [ClipSource::Bundled(BundledSound::MalletFallingFifth)]
    );
}

/// The Needs you row's sound dropdown.
const NEEDS_YOU_FIELD: &str = "settings-needs-you-sound-field";
/// The Finished row's sound dropdown.
const FINISHED_FIELD: &str = "settings-finished-sound-field";
/// The open sound list's last entry, which asks for a file.
const CUSTOM_OPTION: &str = "settings-sound-option-custom";

/// Opens `field`'s sound list and presses `option`, scrolled into view.
fn choose_sound(h: &mut Harness<'_>, field: &str, option: &str) {
    h.click_on(field);
    h.scroll_into_view("settings-sound-list", option);
    h.click_on(option);
    h.cx.run_until_parked();
}

/// Which row's sound list is open.
fn open_list(h: &mut Harness<'_>) -> Option<NotifyKind> {
    h.root(|root, _| root.open_sound_list())
}

/// Asserts the Needs you field shows `shown` when `sound` is saved.
fn assert_needs_you_field_shows(cx: &mut TestAppContext, sound: &serde_json::Value, shown: &str) {
    let dir = TestDir::new();
    save_notifications(&dir, &serde_json::json!({ "needs_you_sound": sound }));
    let mut h = on_notifications_tab(cx, &dir);
    assert!(
        h.bounds(&format!("{NEEDS_YOU_FIELD}-text={shown}"))
            .origin
            .x
            >= px(0.0),
        "{sound} shows as {shown}"
    );
}

#[gpui::test]
fn an_off_sound_shows_as_off(cx: &mut TestAppContext) {
    assert_needs_you_field_shows(cx, &serde_json::json!("off"), "Off");
}

#[gpui::test]
fn the_default_sound_shows_as_the_default_entry(cx: &mut TestAppContext) {
    assert_needs_you_field_shows(
        cx,
        &serde_json::json!("default"),
        "Mallet, rising fifth (default)",
    );
}

#[gpui::test]
fn the_rows_own_default_saved_as_a_sound_shows_as_the_default_entry(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    save_notifications(
        &dir,
        &serde_json::json!({ "needs_you_sound": { "bundled": "mallet_rising_fifth" } }),
    );
    let mut h = on_notifications_tab(cx, &dir);
    assert!(
        h.bounds("settings-needs-you-sound-field-text=Mallet, rising fifth (default)")
            .origin
            .x
            >= px(0.0),
        "the closed field shows the default entry"
    );
    h.click_on(NEEDS_YOU_FIELD);
    assert!(
        h.bounds("settings-sound-option-mallet_rising_fifth-text=Mallet, rising fifth (default)")
            .origin
            .x
            >= px(0.0),
        "the list marks the row's default"
    );
    assert!(
        h.bounds("settings-sound-option-mallet_rising_fifth-chosen")
            .origin
            .x
            >= px(0.0),
        "and that entry is the chosen one"
    );
}

#[gpui::test]
fn the_other_rows_default_shows_as_that_sound_with_no_suffix(cx: &mut TestAppContext) {
    assert_needs_you_field_shows(
        cx,
        &serde_json::json!({ "bundled": "mallet_falling_fifth" }),
        "Mallet, falling fifth",
    );
}

#[gpui::test]
fn a_custom_sound_shows_as_custom_file(cx: &mut TestAppContext) {
    assert_needs_you_field_shows(
        cx,
        &serde_json::json!({ "custom": "ding.wav" }),
        "Custom file…",
    );
}

#[gpui::test]
fn choosing_a_sound_writes_it_and_play_previews_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_notifications_tab(cx, &dir);
    choose_sound(
        &mut h,
        NEEDS_YOU_FIELD,
        "settings-sound-option-bell_rising_fifth",
    );
    assert_eq!(
        saved_ui(&dir)["notifications"]["needs_you_sound"],
        serde_json::json!({ "bundled": "bell_rising_fifth" })
    );
    assert_eq!(open_list(&mut h), None, "a choice closes the list");
    h.click_on("settings-needs-you-sound-play");
    assert_eq!(
        h.sounds(),
        [ClipSource::Bundled(BundledSound::BellRisingFifth)]
    );
}

#[gpui::test]
fn the_sound_list_is_walked_by_keys(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_notifications_tab(cx, &dir);
    h.keys("tab tab tab tab tab tab");
    assert_eq!(settings_focus(&mut h), Some(FINISHED_FIELD));
    h.keys("enter");
    assert_eq!(open_list(&mut h), Some(NotifyKind::Finished));
    h.keys("down enter");
    assert_eq!(
        saved_ui(&dir)["notifications"]["finished_sound"],
        serde_json::json!({ "bundled": "mallet_rising_third" }),
        "Down moves off the chosen entry and Enter chooses"
    );
    assert_eq!(open_list(&mut h), None);
    assert_eq!(
        settings_focus(&mut h),
        Some(FINISHED_FIELD),
        "the field keeps the keyboard"
    );
    h.keys("space up enter");
    assert_eq!(
        saved_ui(&dir)["notifications"]["finished_sound"],
        "default",
        "Space opens the list and Up moves back to the row's default"
    );
}

#[gpui::test]
fn choosing_the_default_entry_writes_default(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    save_notifications(
        &dir,
        &serde_json::json!({ "finished_sound": { "bundled": "bong" } }),
    );
    let mut h = on_notifications_tab(cx, &dir);
    choose_sound(
        &mut h,
        FINISHED_FIELD,
        "settings-sound-option-mallet_falling_fifth",
    );
    assert_eq!(saved_ui(&dir)["notifications"]["finished_sound"], "default");
    choose_sound(
        &mut h,
        NEEDS_YOU_FIELD,
        "settings-sound-option-mallet_falling_fifth",
    );
    assert_eq!(
        saved_ui(&dir)["notifications"]["needs_you_sound"],
        serde_json::json!({ "bundled": "mallet_falling_fifth" }),
        "the Finished default is an ordinary sound on the Needs you row"
    );
}

#[gpui::test]
fn esc_closes_the_sound_list_and_leaves_settings_open(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_notifications_tab(cx, &dir);
    h.click_on(NEEDS_YOU_FIELD);
    assert_eq!(open_list(&mut h), Some(NotifyKind::NeedsYou));
    h.keys("escape");
    assert_eq!(open_list(&mut h), None);
    assert!(
        h.root(|root, _| root.settings_open()),
        "Settings stays open"
    );
    h.keys("escape");
    assert!(
        !h.root(|root, _| root.settings_open()),
        "with the list closed, Esc closes Settings"
    );
}

#[gpui::test]
fn a_held_key_neither_opens_nor_closes_a_sound_list(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_notifications_tab(cx, &dir);
    h.keys("tab tab tab");
    assert_eq!(settings_focus(&mut h), Some(NEEDS_YOU_FIELD));
    h.held("enter");
    assert_eq!(open_list(&mut h), None, "a held Enter opens nothing");

    h.keys("enter");
    h.held("down");
    h.held("enter");
    assert_eq!(
        open_list(&mut h),
        Some(NotifyKind::NeedsYou),
        "a held Enter chooses nothing"
    );
    h.keys("enter");
    assert_eq!(
        saved_ui(&dir)["notifications"]["needs_you_sound"],
        serde_json::json!({ "bundled": "mallet_falling_fifth" }),
        "a held Down still moves, and only the press chose"
    );

    h.keys("enter");
    h.held("escape");
    assert_eq!(
        open_list(&mut h),
        Some(NotifyKind::NeedsYou),
        "a held Esc closes nothing"
    );
    h.keys("escape");
    assert_eq!(open_list(&mut h), None);
    h.held("escape");
    assert!(
        h.root(|root, _| root.settings_open()),
        "the repeat of the Esc that closed the list leaves Settings open"
    );
}

#[gpui::test]
fn leaving_the_notifications_tab_closes_the_sound_list(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    save_notifications(
        &dir,
        &serde_json::json!({ "needs_you_sound": { "bundled": "bong" } }),
    );
    let mut h = on_notifications_tab(cx, &dir);
    h.click_on(NEEDS_YOU_FIELD);
    assert_eq!(open_list(&mut h), Some(NotifyKind::NeedsYou));
    h.root_update(|root, window, cx| root.show_settings_tab("General", window, cx));
    assert_eq!(open_list(&mut h), None);
    h.keys("down enter");
    assert_eq!(
        h.root(|root, _| root.settings_tab()),
        "Notifications",
        "Down is the tab list's again"
    );
    assert_eq!(open_list(&mut h), None, "and no hidden list came back");
    assert_eq!(
        saved_ui(&dir)["notifications"]["needs_you_sound"],
        serde_json::json!({ "bundled": "bong" }),
        "Enter saved no hidden highlight"
    );
}

#[gpui::test]
fn tab_closes_the_sound_list_and_walks_on(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_notifications_tab(cx, &dir);
    h.keys("tab tab tab enter");
    assert_eq!(open_list(&mut h), Some(NotifyKind::NeedsYou));
    h.keys("tab");
    assert_eq!(open_list(&mut h), None);
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-needs-you-sound-play")
    );
}

#[gpui::test]
fn up_and_down_stop_at_the_ends_of_the_sound_list(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_notifications_tab(cx, &dir);
    h.keys("tab tab tab enter up up up enter");
    assert_eq!(
        saved_ui(&dir)["notifications"]["needs_you_sound"],
        "off",
        "Up stops on the first entry"
    );
    let chosen = dir.path().join("ding.wav");
    h.set_picked_file(chosen.to_str());
    h.keys("enter");
    h.keys("down ".repeat(20).trim_end());
    h.keys("enter");
    h.cx.run_until_parked();
    assert_eq!(
        saved_ui(&dir)["notifications"]["needs_you_sound"],
        serde_json::json!({ "custom": chosen }),
        "Down stops on the last entry"
    );
}

#[gpui::test]
fn a_short_window_keeps_the_whole_sound_list_on_screen(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_notifications_tab(cx, &dir);
    let height = px(460.0);
    h.cx.simulate_resize(gpui::size(px(1000.0), height));
    h.click_on(NEEDS_YOU_FIELD);
    let list = h.bounds("settings-sound-list");
    assert!(
        list.top() >= px(0.0) && list.bottom() <= height,
        "the list stays inside the window: {list:?}"
    );
    h.scroll_into_view("settings-sound-list", CUSTOM_OPTION);
    let last = h.bounds(CUSTOM_OPTION);
    assert!(
        last.top() >= px(0.0) && last.bottom() <= height,
        "the last row scrolls into the window: {last:?}"
    );
    let chosen = dir.path().join("ding.wav");
    h.set_picked_file(chosen.to_str());
    h.click_on(CUSTOM_OPTION);
    h.cx.run_until_parked();
    assert_eq!(
        saved_ui(&dir)["notifications"]["needs_you_sound"],
        serde_json::json!({ "custom": chosen }),
        "and a press reaches it"
    );
}

#[gpui::test]
fn opening_one_sound_list_closes_the_other(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_notifications_tab(cx, &dir);
    // The Needs you list opens over the Finished field, so the Finished list
    // opens first: its own list leaves the Needs you field to click.
    h.click_on(FINISHED_FIELD);
    assert_eq!(open_list(&mut h), Some(NotifyKind::Finished));
    h.click_on(NEEDS_YOU_FIELD);
    assert_eq!(open_list(&mut h), Some(NotifyKind::NeedsYou));
    h.click_on(NEEDS_YOU_FIELD);
    assert_eq!(open_list(&mut h), None, "a second click closes it");
    h.click_on(FINISHED_FIELD);
    h.click_on("settings-tab-notifications");
    assert_eq!(open_list(&mut h), None, "a press outside closes it");
}

#[gpui::test]
fn choosing_off_default_and_custom_writes_the_sound_key(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_notifications_tab(cx, &dir);
    choose_sound(&mut h, NEEDS_YOU_FIELD, "settings-sound-option-off");
    assert_eq!(saved_ui(&dir)["notifications"]["needs_you_sound"], "off");
    // gpui keeps a selector's last bounds once it stops being drawn, so the
    // Tab ring says whether Play is there.
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-notify-finished"),
        "no Play to preview while the sound is off"
    );

    choose_sound(
        &mut h,
        NEEDS_YOU_FIELD,
        "settings-sound-option-mallet_rising_fifth",
    );
    assert_eq!(
        saved_ui(&dir)["notifications"]["needs_you_sound"],
        "default"
    );

    let chosen = dir.path().join("ding.wav");
    h.set_picked_file(chosen.to_str());
    choose_sound(&mut h, NEEDS_YOU_FIELD, CUSTOM_OPTION);
    assert_eq!(
        h.file_filters()
            .last()
            .map(|filter| filter.extensions.clone()),
        Some(vec!["wav".to_owned(), "ogg".to_owned()]),
        "the picker offers .wav and .ogg"
    );
    assert_eq!(
        saved_ui(&dir)["notifications"]["needs_you_sound"],
        serde_json::json!({ "custom": chosen })
    );
    assert!(
        h.bounds("settings-needs-you-sound-path").origin.x >= px(0.0),
        "the chosen path shows"
    );
    let problem = h
        .root(|root, _| root.needs_you_sound_problem())
        .expect("a missing file is a problem");
    assert!(
        problem.contains("could not be read"),
        "the problem says why: {problem}"
    );
    assert!(h.bounds("settings-needs-you-sound-problem").origin.x >= px(0.0));

    h.set_picked_file(None);
    choose_sound(&mut h, NEEDS_YOU_FIELD, CUSTOM_OPTION);
    assert_eq!(
        saved_ui(&dir)["notifications"]["needs_you_sound"],
        serde_json::json!({ "custom": chosen }),
        "a cancelled pick keeps the choice"
    );
}

#[gpui::test]
fn play_previews_the_chosen_sound_once(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_notifications_tab(cx, &dir);
    assert!(h.sounds().is_empty(), "opening the tab plays nothing");
    h.click_on("settings-needs-you-sound-play");
    assert_eq!(
        h.sounds(),
        [ClipSource::Bundled(BundledSound::MalletRisingFifth)]
    );
}

#[gpui::test]
fn choosing_off_default_and_custom_writes_the_finished_sound_key(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_notifications_tab(cx, &dir);
    choose_sound(&mut h, FINISHED_FIELD, "settings-sound-option-off");
    assert_eq!(saved_ui(&dir)["notifications"]["finished_sound"], "off");
    // gpui keeps a selector's last bounds once it stops being drawn, so the
    // Tab ring says whether Play is there.
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-finished-grace-0"),
        "no Play to preview while the sound is off, so the grace row follows"
    );

    choose_sound(
        &mut h,
        FINISHED_FIELD,
        "settings-sound-option-mallet_falling_fifth",
    );
    assert_eq!(saved_ui(&dir)["notifications"]["finished_sound"], "default");

    let chosen = dir.path().join("done.wav");
    h.set_picked_file(chosen.to_str());
    choose_sound(&mut h, FINISHED_FIELD, CUSTOM_OPTION);
    assert_eq!(
        h.file_filters()
            .last()
            .map(|filter| filter.extensions.clone()),
        Some(vec!["wav".to_owned(), "ogg".to_owned()]),
        "the picker offers .wav and .ogg"
    );
    assert_eq!(
        saved_ui(&dir)["notifications"]["finished_sound"],
        serde_json::json!({ "custom": chosen })
    );
    assert!(
        h.bounds("settings-finished-sound-path").origin.x >= px(0.0),
        "the chosen path shows"
    );
    let problem = h
        .root(|root, _| root.finished_sound_problem())
        .expect("a missing file is a problem");
    assert!(
        problem.contains("could not be read"),
        "the problem says why: {problem}"
    );
    assert!(h.bounds("settings-finished-sound-problem").origin.x >= px(0.0));

    h.set_picked_file(None);
    choose_sound(&mut h, FINISHED_FIELD, CUSTOM_OPTION);
    assert_eq!(
        saved_ui(&dir)["notifications"]["finished_sound"],
        serde_json::json!({ "custom": chosen }),
        "a cancelled pick keeps the choice"
    );
}

#[gpui::test]
fn play_previews_the_finished_sound_once(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_notifications_tab(cx, &dir);
    assert!(h.sounds().is_empty(), "opening the tab plays nothing");
    h.click_on("settings-finished-sound-play");
    assert_eq!(
        h.sounds(),
        [ClipSource::Bundled(BundledSound::MalletFallingFifth)]
    );
}
