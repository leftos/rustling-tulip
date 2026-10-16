//! Notification specs: what an attention event notifies, the Needs you
//! toggle, when a notification for a session on screen is skipped, and the
//! Settings modal's Notifications tab.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use gpui::{TestAppContext, px};
use protocol::{AttentionReason, DaemonMessage, TabEntry};
use rustling_tulip_native::NotifyState;
use support::{Fixture, Harness, Opened, TestDir, pane, repo, session, tab};

fn attention(id: &str, reason: AttentionReason) -> DaemonMessage {
    DaemonMessage::Attention {
        session_id: id.to_owned(),
        reason,
    }
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
    assert_eq!(h.notified(), [shown("Needs you", "fix login")]);
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
        [shown("Needs you", "dev server")],
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
fn notifications_tab_has_one_needs_you_toggle(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_notifications_tab(cx, &dir);
    assert!(
        h.bounds("settings-notify-needs-you").origin.x >= px(0.0),
        "the Needs you toggle is painted"
    );
    for gone in ["settings-notify-stopped", "settings-notify-error"] {
        assert!(
            h.bounds(gone).origin.x < px(0.0),
            "{gone} left the Notifications tab"
        );
    }

    h.click_on("settings-notify-needs-you");
    let saved = saved_ui(&dir);
    assert_eq!(saved["notifications"]["awaiting_input"], false);
    assert!(saved["notifications"].get("stopped").is_none());
    assert!(saved["notifications"].get("error").is_none());

    h.send(attention("s2", AttentionReason::AwaitingInput));
    assert!(h.notified().is_empty(), "the toggle took effect at once");

    assert_eq!(settings_focus(&mut h), Some("settings-notify-needs-you"));
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
        [shown("Needs you", "fix login")],
        "the Settings modal hides s1's pane"
    );
}

#[gpui::test]
fn focused_window_other_tab_still_notifies(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_tabs(cx, &dir);
    focus_window(&mut h);
    h.send(attention("s2", AttentionReason::AwaitingInput));
    assert_eq!(h.notified(), [shown("Needs you", "review")]);
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
    assert_eq!(h.notified(), [shown("Needs you", "fix login")]);
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
    assert_eq!(h.notified(), [shown("Needs you", "fix login")]);
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
        [shown("Needs you", "fix login")],
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
            shown("Needs you", "rustling-tulip"),
            shown("Needs you", "rustling-tulip"),
        ]
    );
}
