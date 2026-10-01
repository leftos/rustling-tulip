//! Notification specs: what an attention event notifies, the per-reason
//! toggles, a Stop this client sent staying quiet, and the Settings
//! modal's Notifications tab.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use gpui::{TestAppContext, px};
use protocol::{AttentionReason, BranchFate, ClientMessage, DaemonMessage, MemberBranchFate};
use rustling_tulip_native::NotifyState;
use support::{Fixture, Harness, Opened, TestDir, session};

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
fn awaiting_input_attention_notifies_with_label(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.send(attention("s1", AttentionReason::AwaitingInput));
    assert_eq!(
        h.notified(),
        [shown("Claude is awaiting input", "fix login")]
    );
}

#[gpui::test]
fn stopped_attention_respects_toggle(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    std::fs::write(
        dir.path().join("native-ui.json"),
        r#"{ "notifications": { "stopped": false } }"#,
    )
    .expect("write native-ui.json");
    let mut h = opened(cx, &dir);
    h.send(attention("s2", AttentionReason::Stopped));
    assert!(h.notified().is_empty(), "Stopped is off");
    h.send(attention("s2", AttentionReason::AwaitingInput));
    assert_eq!(h.notified(), [shown("Claude is awaiting input", "review")]);
}

#[gpui::test]
fn error_attention_notifies(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.send(attention("s2", AttentionReason::Error));
    assert_eq!(h.notified(), [shown("Claude session errored", "review")]);
}

#[gpui::test]
fn own_stop_does_not_notify(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.click_on("pane-stop-p1");
    h.click_on("pane-stop-confirm-p1");
    assert_eq!(h.sent().len(), 1, "the stop went out");

    // The daemon answers the requester, then broadcasts on the exit.
    h.send(attention("s1", AttentionReason::Stopped));
    h.send(updated(session("s1").user_label("fix login").exited(0)));
    h.send(attention("s1", AttentionReason::Stopped));
    assert!(h.notified().is_empty(), "this client stopped s1");

    h.send(attention("s2", AttentionReason::Stopped));
    assert_eq!(h.notified(), [shown("Claude session stopped", "review")]);
}

fn updated(session: support::SessionBuilder) -> DaemonMessage {
    DaemonMessage::SessionUpdated {
        session: session.build(),
        request_id: None,
    }
}

#[gpui::test]
fn own_stop_clears_when_session_runs_again(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.click_on("pane-stop-p1");
    h.click_on("pane-stop-confirm-p1");
    h.sent();
    h.send(updated(
        session("s1").user_label("fix login").status("working"),
    ));
    h.send(attention("s1", AttentionReason::Stopped));
    assert!(
        h.notified().is_empty(),
        "running before the stop lands keeps it quiet"
    );

    h.send(updated(session("s1").user_label("fix login").exited(0)));
    h.send(updated(
        session("s1").user_label("fix login").status("working"),
    ));
    h.send(attention("s1", AttentionReason::Stopped));
    assert_eq!(
        h.notified(),
        [shown("Claude session stopped", "fix login")],
        "resumed after the stop, its next stop is not this client's"
    );
}

#[gpui::test]
fn quit_stop_does_not_notify(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture::single(session("s1").worktree("r1").build());
    let mut h = Harness::with(cx, &dir, &fixture);
    h.answer_scrollback("s1", b"");
    assert!(!h.cx.simulate_close(), "the exit dialog asks first");
    h.cx.run_until_parked();
    h.click_on("exit-stop-remove-worktrees");
    h.send(DaemonMessage::DiscardPreview {
        session_id: "s1".to_owned(),
        members: vec![MemberBranchFate {
            repo_id: "r1".to_owned(),
            repo_name: "r1".to_owned(),
            branch: "wt/x".to_owned(),
            fate: BranchFate::KeptByDefault {
                unique_commits: Some(1),
                checked_against: vec!["main".to_owned()],
            },
        }],
    });
    h.click_on("delete-worktree-keep-branch");
    let stops = h
        .commands()
        .iter()
        .filter_map(|command| match command {
            rustling_tulip_native::NetCommand::Shutdown { before, .. } => Some(
                before
                    .iter()
                    .filter(|msg| matches!(msg, ClientMessage::StopSession { .. }))
                    .count(),
            ),
            _ => None,
        })
        .sum::<usize>();

    assert_eq!(stops, 1, "the shutdown carries the stop");
    h.send(attention("s1", AttentionReason::Stopped));
    h.send(attention("s1", AttentionReason::Stopped));
    assert!(h.notified().is_empty(), "this client stopped s1");
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
fn notifications_tab_toggles_saved(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_notifications_tab(cx, &dir);
    for selector in [
        "settings-notify-awaiting-input",
        "settings-notify-stopped",
        "settings-notify-error",
    ] {
        assert!(
            h.bounds(selector).origin.x >= px(0.0),
            "{selector} is painted"
        );
    }

    h.click_on("settings-notify-stopped");
    let saved = saved_ui(&dir);
    assert_eq!(saved["notifications"]["stopped"], false);
    assert_eq!(saved["notifications"]["awaiting_input"], true);
    assert_eq!(saved["notifications"]["error"], true);

    h.send(attention("s2", AttentionReason::Stopped));
    assert!(h.notified().is_empty(), "the toggle took effect at once");

    assert_eq!(settings_focus(&mut h), Some("settings-notify-stopped"));
    h.keys("tab tab tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-open-windows-notifications"),
        "past Errored and the tab list, round to the link"
    );
    h.keys("tab tab space");
    assert_eq!(settings_focus(&mut h), Some("settings-notify-stopped"));
    assert_eq!(saved_ui(&dir)["notifications"]["stopped"], true);
}

fn settings_focus(h: &mut Harness<'_>) -> Option<&'static str> {
    let root = h.root.clone();
    h.cx.update(|window, cx| root.read(cx).settings_focus(window))
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
    h.send(attention("ghost", AttentionReason::Error));
    assert_eq!(
        h.notified(),
        [
            shown("Claude is awaiting input", "rustling-tulip"),
            shown("Claude session errored", "rustling-tulip"),
        ]
    );
}
