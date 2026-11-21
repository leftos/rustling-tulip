//! Settings › Linear specs: the tab a host listing `linear_settings`
//! offers, its status rows, the switch Apply sends, the key's Save and
//! Clear, the lock a remote origin puts on the key, Test and its result,
//! and a refused key shown under the field.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use std::time::Duration;

use gpui::{TestAppContext, px};
use protocol::{
    ClientMessage, ConnectionOrigin, DaemonMessage, FEATURE_LINEAR_SETTINGS, LinearKeyState,
    error_codes,
};
use rustling_tulip_native::target::Target;
use rustling_tulip_native::{Connection, NetEvent, RootView};
use support::{Fixture, Harness, PROTOCOL, TestDir, daemon_features, session};

/// A window on one session, its pane drawn, nothing sent yet.
fn opened<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = Harness::with(cx, dir, &Fixture::single(session("s1").build()));
    h.answer_scrollback("s1", b"");
    h.advance(Duration::from_millis(400));
    h.sent();
    h
}

/// Whether the element tagged `selector` was drawn at some point.
fn drawn(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).origin.x >= px(0.0)
}

fn press(h: &mut Harness<'_>, selector: &str) {
    h.scroll_into_view("settings-content", selector);
    h.click_on(selector);
}

/// The Settings stop that has the keyboard.
fn settings_focus(h: &mut Harness<'_>) -> Option<&'static str> {
    let root = h.root.clone();
    h.cx.update(|window, cx| root.read(cx).settings_focus(window, cx))
}

fn type_text(h: &mut Harness<'_>, text: &str) {
    let keys: Vec<String> = text.chars().map(|c| c.to_string()).collect();
    h.keys(&keys.join(" "));
}

/// A `LinearStatus` with Linear on, a saved key, the workspace `Leftos`
/// and its teams RT and YAAT.
fn status() -> DaemonMessage {
    DaemonMessage::LinearStatus {
        enabled: true,
        key: LinearKeyState::Stored,
        workspace_name: Some("Leftos".to_owned()),
        team_keys: vec!["RT".to_owned(), "YAAT".to_owned()],
        last_error: None,
        key_saved: true,
    }
}

/// A `LinearStatus` with Linear on and the key in use refused; `key_saved`
/// says whether it is the saved key.
fn rejected(key_saved: bool) -> DaemonMessage {
    DaemonMessage::LinearStatus {
        enabled: true,
        key: LinearKeyState::Rejected,
        workspace_name: None,
        team_keys: Vec::new(),
        last_error: Some("Linear refused the key (HTTP 401).".to_owned()),
        key_saved,
    }
}

/// This PC reconnected to a daemon listing `linear_settings` that sent
/// `status`, then Settings open on the Linear tab, nothing sent yet.
fn page<'a>(cx: &'a mut TestAppContext, dir: &TestDir, status: DaemonMessage) -> Harness<'a> {
    let mut h = opened(cx, dir);
    h.reconnect_with_features(&[FEATURE_LINEAR_SETTINGS]);
    h.send(status);
    on_page(&mut h);
    h
}

fn on_page(h: &mut Harness<'_>) {
    h.click_on("settings-open");
    h.click_on("settings-tab-linear");
    assert_eq!(h.root(|root, _| root.settings_tab()), "Linear");
    h.sent();
}

/// The `(key, request id)` of every `SetLinearKey` among `sent`.
fn key_sets(sent: &[ClientMessage]) -> Vec<(Option<String>, Option<String>)> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::SetLinearKey { key, request_id } => {
                Some((key.as_ref().map(|key| key.0.clone()), request_id.clone()))
            }
            _ => None,
        })
        .collect()
}

/// The request ids of the `TestLinear` messages among `sent`.
fn test_ids(sent: &[ClientMessage]) -> Vec<Option<String>> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::TestLinear { request_id } => Some(request_id.clone()),
            _ => None,
        })
        .collect()
}

#[gpui::test]
fn the_tab_follows_dashboard_only_on_a_host_listing_linear_settings(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.keys("ctrl-,");
    let rows = h.root(|root, _| root.settings_tab_rows());
    assert!(
        !rows.contains(&"Linear"),
        "no linear_settings, no tab: {rows:?}"
    );

    h.reconnect_with_features(&[FEATURE_LINEAR_SETTINGS]);
    let rows = h.root(|root, _| root.settings_tab_rows());
    let at = rows
        .iter()
        .position(|row| *row == "Dashboard")
        .expect("Dashboard is listed");
    assert_eq!(rows.get(at + 1).copied(), Some("Linear"), "{rows:?}");
}

#[gpui::test]
fn the_status_rows_show_the_key_state_workspace_and_teams(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = page(cx, &dir, status());
    assert!(drawn(&mut h, "settings-linear-key-state=Key saved"));
    assert!(drawn(&mut h, "settings-linear-workspace=Leftos"));
    assert!(drawn(&mut h, "settings-linear-teams=RT, YAAT"));
    assert!(drawn(&mut h, "settings-linear-enabled"), "the switch");

    h.send(rejected(true));
    assert!(drawn(
        &mut h,
        "settings-linear-key-state=Linear refused the saved key"
    ));
    assert!(drawn(&mut h, "settings-linear-workspace=—"), "no workspace");
    assert!(drawn(&mut h, "settings-linear-teams=—"), "no teams");
    assert!(drawn(
        &mut h,
        "settings-linear-last-error=Linear refused the key (HTTP 401)."
    ));
}

#[gpui::test]
fn the_switch_waits_for_apply_then_sends_set_linear_enabled(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = page(cx, &dir, status());
    press(&mut h, "settings-linear-enabled");
    let early = h.sent();
    assert!(
        !early
            .iter()
            .any(|msg| matches!(msg, ClientMessage::SetLinearEnabled { .. })),
        "the switch waits for Apply: {early:?}"
    );
    assert!(h.root(|root, _| root.settings_apply_enabled()));
    h.click_on("settings-apply");
    let sets: Vec<(bool, bool)> = h
        .sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::SetLinearEnabled {
                enabled,
                request_id,
            } => Some((enabled, request_id.is_some())),
            _ => None,
        })
        .collect();
    assert_eq!(sets, [(false, true)], "off, with a request id");
}

#[gpui::test]
fn save_sends_the_typed_key_and_empties_the_field(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = page(cx, &dir, status());
    press(&mut h, "settings-linear-key");
    type_text(&mut h, "lin123");
    assert_eq!(key_sets(&h.sent()), [], "typing sends nothing");
    press(&mut h, "settings-linear-key-save");
    let sets = key_sets(&h.sent());
    assert_eq!(sets.len(), 1, "{sets:?}");
    assert_eq!(sets[0].0.as_deref(), Some("lin123"));
    assert!(sets[0].1.is_some(), "with a request id");
    assert_eq!(
        h.root(RootView::linear_key_draft).as_deref(),
        Some(""),
        "Save empties the field"
    );
}

#[gpui::test]
fn clear_sends_no_key(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = page(cx, &dir, status());
    press(&mut h, "settings-linear-key-clear");
    let sets = key_sets(&h.sent());
    assert_eq!(sets.len(), 1, "{sets:?}");
    assert_eq!(sets[0].0, None, "Clear deletes the saved key");
}

#[gpui::test]
fn clear_is_offered_only_for_a_saved_key(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = page(cx, &dir, rejected(false));
    assert!(drawn(
        &mut h,
        "settings-linear-key-state=Linear refused LINEAR_API_KEY"
    ));
    press(&mut h, "settings-linear-key-clear");
    assert_eq!(
        key_sets(&h.sent()),
        [],
        "LINEAR_API_KEY is not saved, so Clear has nothing to delete"
    );

    h.send(rejected(true));
    press(&mut h, "settings-linear-key-clear");
    let sets = key_sets(&h.sent());
    assert_eq!(sets.len(), 1, "{sets:?}");
    assert_eq!(sets[0].0, None, "Clear deletes the refused saved key");
}

#[gpui::test]
fn a_remote_origin_locks_the_key_while_test_stays_open(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    let mut features = daemon_features();
    features.push(FEATURE_LINEAR_SETTINGS.to_owned());
    let mut conn = Connection::for_target(Target::Local);
    conn.on_connecting();
    conn.on_socket_open(4242);
    conn.on_welcome(PROTOCOL, features, Some(ConnectionOrigin::Remote));
    h.event(NetEvent::State(conn));
    h.send_welcome(PROTOCOL);
    h.send(status());
    on_page(&mut h);

    let note = h
        .root(|root, _| root.linear_key_note())
        .expect("the locked key has its note");
    assert!(
        note.starts_with("Set the key on ") && note.ends_with(" itself."),
        "{note}"
    );
    assert!(drawn(&mut h, &format!("settings-linear-key-locked={note}")));
    press(&mut h, "settings-linear-key");
    assert_ne!(
        settings_focus(&mut h),
        Some("settings-linear-key"),
        "a locked key field takes no keyboard"
    );
    type_text(&mut h, "lin123");
    h.set_clipboard("lin_pasted");
    h.keys("secondary-v");
    let draft = h.root(RootView::linear_key_draft);
    assert!(
        draft.as_deref().is_none_or(str::is_empty),
        "a locked key field takes no text: {draft:?}"
    );
    press(&mut h, "settings-linear-key-save");
    press(&mut h, "settings-linear-key-clear");
    let sent = h.sent();
    assert_eq!(key_sets(&sent), [], "the key is locked: {sent:?}");

    press(&mut h, "settings-linear-test");
    assert_eq!(test_ids(&h.sent()).len(), 1, "Test is open to every origin");
}

#[gpui::test]
fn test_shows_testing_then_the_replys_text(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = page(cx, &dir, status());
    press(&mut h, "settings-linear-test");
    let ids = test_ids(&h.sent());
    assert_eq!(ids.len(), 1, "{ids:?}");
    let id = ids[0].clone().expect("the Test carries a request id");
    assert_eq!(
        h.root(|root, _| root.linear_test_result()).as_deref(),
        Some("Testing…")
    );
    assert!(drawn(&mut h, "settings-linear-test-result=Testing…"));

    h.send(DaemonMessage::LinearTest {
        workspace_name: Some("Leftos".to_owned()),
        team_keys: vec!["RT".to_owned(), "YAAT".to_owned()],
        error: None,
        request_id: Some(id),
    });
    assert_eq!(
        h.root(|root, _| root.linear_test_result()).as_deref(),
        Some("Reached Leftos · teams RT, YAAT")
    );
    assert!(drawn(
        &mut h,
        "settings-linear-test-result=Reached Leftos · teams RT, YAAT"
    ));
}

#[gpui::test]
fn a_refused_key_shows_its_error_under_the_field(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = page(cx, &dir, status());
    press(&mut h, "settings-linear-key");
    type_text(&mut h, "lin123");
    press(&mut h, "settings-linear-key-save");
    let sets = key_sets(&h.sent());
    let id = sets
        .first()
        .and_then(|(_, id)| id.clone())
        .expect("the save carries a request id");
    h.send(DaemonMessage::Error {
        message: "The secure store is out of reach.".to_owned(),
        request_id: Some(id),
        code: Some(error_codes::LINEAR_REFUSED.to_owned()),
    });
    assert!(drawn(
        &mut h,
        "settings-linear-key-error=The secure store is out of reach."
    ));
}
