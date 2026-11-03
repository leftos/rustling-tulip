//! The driver's requests (feature `driver`), answered in-process against the
//! harness window: version, state, the registry's names and keys.
#![cfg(feature = "driver")]

use crate::support;

use gpui::{Modifiers, TestAppContext};
use rustling_tulip_native::driver::handlers::{handle, wait_read};
use rustling_tulip_native::driver::protocol::{ErrorCode, Failure, Outcome, Reply, Request};
use rustling_tulip_native::driver::wait::parse_predicate;
use serde_json::{Value, json};
use support::{Fixture, Harness, TestDir, pane, repo, session, tab};

/// Repos `r1` and `r2`, one session of `r1` alone in pane `p1` of tab `t1`.
fn two_repos() -> Fixture {
    let mut fixture = Fixture::single(session("s1").in_repo("r1").build());
    fixture.repos = vec![repo("r1", "D:/src/r1"), repo("r2", "D:/src/r2")];
    fixture
}

/// Sessions `s1` and `s2` in panes `p1` of tab `t1` and `p2` of tab `t2`.
fn two_tabs() -> Fixture {
    Fixture {
        sessions: vec![session("s1").build(), session("s2").build()],
        tabs: vec![
            tab("t1", &pane("p1", Some("s1"))),
            tab("t2", &pane("p2", Some("s2"))),
        ],
        ..Fixture::default()
    }
}

/// `two_repos`, `s1`'s scrollback answered, its pane clicked so it holds
/// the keyboard, and everything sent so far drained.
fn focused_pane<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = Harness::with(cx, dir, &two_repos());
    h.answer_scrollback("s1", b"");
    let at = h.cell_center("p1", 0, 0);
    h.click(at, Modifiers::none());
    h.sent();
    h
}

/// The reply to `method` with `params`, sent once the window has settled.
fn call(h: &mut Harness<'_>, method: &str, params: Value) -> Reply {
    h.cx.run_until_parked();
    let request = Request {
        id: 7,
        method: method.to_owned(),
        params,
    };
    let root = h.root.clone();
    let reply =
        h.cx.update(|window, cx| handle(&request, &root, window, cx));
    h.cx.run_until_parked();
    assert_eq!(reply.id, 7, "the reply answers the request's id");
    reply
}

/// The result of `method`, failing the spec with the error it got instead.
#[expect(
    clippy::panic,
    reason = "an error reply fails the spec with the failure it carries"
)]
fn result(h: &mut Harness<'_>, method: &str, params: Value) -> Value {
    match call(h, method, params).outcome {
        Outcome::Result(result) => result,
        Outcome::Error(failure) => panic!("{method} failed: {failure:?}"),
    }
}

/// The failure of `method`, failing the spec when it succeeded instead.
#[expect(
    clippy::panic,
    reason = "a result fails the spec with the result it carries"
)]
fn failure(h: &mut Harness<'_>, method: &str, params: Value) -> Failure {
    match call(h, method, params).outcome {
        Outcome::Error(failure) => failure,
        Outcome::Result(result) => panic!("{method} succeeded: {result}"),
    }
}

#[gpui::test]
fn driver_ping_answers_the_version(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());

    assert_eq!(
        result(&mut h, "ping", Value::Null),
        json!({"version": env!("CARGO_PKG_VERSION")})
    );
}

#[gpui::test]
fn driver_unknown_method_lists_methods(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());

    let failed = failure(&mut h, "nope", json!({}));
    assert_eq!(failed.code, ErrorCode::UnknownMethod);
    assert_eq!(failed.message, "unknown method: nope");
    assert_eq!(
        failed.hint.as_deref(),
        Some("methods: action, find, key, pane_text, ping, screenshot, state, type_text, wait_for")
    );
}

#[gpui::test]
fn driver_key_takes_one_keystroke_per_request(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());

    let failed = failure(&mut h, "key", json!({"keys": "ctrl-, escape"}));

    assert_eq!(failed.code, ErrorCode::BadParams);
    assert_eq!(failed.message, "send one keystroke per request");
    assert_eq!(
        failed.hint.as_deref(),
        Some("send ctrl-k, then ctrl-s, as two requests")
    );
    assert!(
        !h.root(|root, _| root.settings_open()),
        "a refused request presses nothing"
    );
}

#[gpui::test]
fn driver_key_ctrl_comma_opens_settings(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    assert_eq!(result(&mut h, "state", json!({}))["overlay"], Value::Null);

    result(&mut h, "key", json!({"keys": "ctrl-,"}));

    assert!(
        h.root(|root, _| root.settings_open()),
        "Ctrl+, opened Settings"
    );
    assert_eq!(
        result(&mut h, "state", json!({}))["overlay"],
        json!("settings")
    );
}

#[gpui::test]
fn driver_find_lists_names_with_a_prefix(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());

    let found = result(&mut h, "find", json!({"prefix": "container-repo:"}));
    let rows = found.as_array().expect("find answers a list");
    let names: Vec<&str> = rows
        .iter()
        .map(|row| row["name"].as_str().expect("a name"))
        .collect();
    assert!(
        names.iter().all(|name| name.starts_with("container-repo:")),
        "every name has the prefix: {names:?}"
    );
    assert!(names.is_sorted(), "the names come sorted: {names:?}");
    for row in ["container-repo:r1", "container-repo:r2"] {
        assert!(names.contains(&row), "{row} is listed: {names:?}");
    }
    let r1 = rows
        .iter()
        .find(|row| row["name"] == json!("container-repo:r1"))
        .expect("r1's row");

    let bounds = h.bounds("container-repo:r1");
    let expected = json!({
        "x": f32::from(bounds.origin.x),
        "y": f32::from(bounds.origin.y),
        "w": f32::from(bounds.size.width),
        "h": f32::from(bounds.size.height),
    });
    assert_eq!(r1["bounds"], expected, "the row's bounds");
    assert_eq!(r1["visible"], expected, "the row shows whole");
}

#[gpui::test]
fn driver_state_lists_tabs_and_sessions(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());

    let state = result(&mut h, "state", json!({}));

    assert_eq!(state["active_tab"], json!("t1"));
    assert_eq!(state["tabs"], json!([{"id": "t1", "title": "t1"}]));
    assert_eq!(state["panes"], json!([{"id": "p1", "session_id": "s1"}]));
    let sessions = state["sessions"].as_array().expect("a session list");
    assert_eq!(sessions.len(), 1, "one session: {sessions:?}");
    assert_eq!(sessions[0]["id"], json!("s1"));
    assert_eq!(sessions[0]["label"], json!("s1"));
    assert!(sessions[0]["status"].is_string(), "{sessions:?}");
    assert_eq!(state["overlay"], Value::Null);
    let window = &state["window"];
    assert!(
        window["width"].as_f64().is_some_and(|width| width > 0.0)
            && window["height"].as_f64().is_some_and(|height| height > 0.0)
            && window["scale"].as_f64().is_some_and(|scale| scale > 0.0),
        "the window's logical size and scale: {window}"
    );
}

#[gpui::test]
fn driver_type_text_reaches_the_focused_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = focused_pane(cx, &dir);

    let typed = result(&mut h, "type_text", json!({"text": "echo hi"}));

    assert_eq!(typed, json!({"typed": 7}));
    let sent = h.sent_input("s1");
    assert!(
        sent.ends_with(b"echo hi"),
        "the pane's session got the text: {:?}",
        String::from_utf8_lossy(&sent)
    );
}

#[gpui::test]
fn driver_type_text_refuses_control_characters_and_types_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = focused_pane(cx, &dir);

    let failed = failure(&mut h, "type_text", json!({"text": "a\nb"}));

    assert_eq!(failed.code, ErrorCode::BadParams);
    assert_eq!(
        failed.hint.as_deref(),
        Some("type_text takes printable text; press Enter, Tab or Escape with key")
    );
    assert!(
        h.sent_input("s1").is_empty(),
        "a refused text types nothing"
    );
}

#[gpui::test]
fn driver_pane_text_reads_the_panes_rows(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    h.pty("s1", b"hello\r\nworld");
    h.cx.run_until_parked();

    let read = result(&mut h, "pane_text", json!({"pane": "p1"}));

    let rows = h.grid_text("p1");
    assert_eq!(rows.first().map(String::as_str), Some("hello"), "{rows:?}");
    assert_eq!(read["pane"], json!("p1"));
    assert_eq!(read["lines"], json!(rows));
    assert_eq!(read["text"], json!(rows.join("\n")));
    assert_eq!(
        result(&mut h, "pane_text", json!({})),
        read,
        "the focused pane is read by default"
    );
}

#[gpui::test]
fn driver_pane_text_unknown_pane_lists_panes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());

    let failed = failure(&mut h, "pane_text", json!({"pane": "nope"}));

    assert_eq!(failed.code, ErrorCode::NotFound);
    assert_eq!(failed.hint.as_deref(), Some("panes: p1"));
}

#[gpui::test]
fn driver_action_open_settings_opens_settings(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());

    let done = result(&mut h, "action", json!({"name": "open_settings"}));

    assert_eq!(done, json!({"action": "open_settings"}));
    assert_eq!(
        result(&mut h, "state", json!({}))["overlay"],
        json!("settings")
    );
}

#[gpui::test]
fn driver_action_unknown_name_lists_actions(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());

    let failed = failure(&mut h, "action", json!({"name": "nope"}));

    assert_eq!(failed.code, ErrorCode::BadParams);
    assert_eq!(
        failed.hint.as_deref(),
        Some(
            "actions: open_settings, new_shell, open_spawn, toggle_sidebar, select_tab, \
             focus_pane, show_activity, set_hover"
        )
    );
}

#[gpui::test]
fn driver_action_select_tab_switches_the_active_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_tabs());
    assert_eq!(
        result(&mut h, "state", json!({}))["active_tab"],
        json!("t1")
    );

    result(
        &mut h,
        "action",
        json!({"name": "select_tab", "args": {"id": "t2"}}),
    );

    assert_eq!(
        result(&mut h, "state", json!({}))["active_tab"],
        json!("t2")
    );
}

#[gpui::test]
fn driver_action_select_tab_unknown_id_is_not_found(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_tabs());

    let failed = failure(
        &mut h,
        "action",
        json!({"name": "select_tab", "args": {"id": "t9"}}),
    );

    assert_eq!(failed.code, ErrorCode::NotFound);
    assert_eq!(failed.hint.as_deref(), Some("tabs: t1, t2"));
    assert_eq!(
        result(&mut h, "state", json!({}))["active_tab"],
        json!("t1")
    );
}

#[gpui::test]
#[expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]
fn driver_wait_read_sees_the_settings_overlay(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    let predicate = parse_predicate(&json!({"state": "overlay", "equals": "settings"}))
        .expect("a well-formed predicate");
    let root = h.root.clone();
    let before =
        h.cx.update(|window, cx| wait_read(&predicate, &root, window, cx));
    assert_eq!(before, (false, Value::Null), "no overlay yet");

    result(&mut h, "key", json!({"keys": "ctrl-,"}));

    let after =
        h.cx.update(|window, cx| wait_read(&predicate, &root, window, cx));
    assert_eq!(after, (true, json!("settings")));
}

#[gpui::test]
fn driver_wait_for_rejects_a_malformed_predicate(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());

    let failed = failure(
        &mut h,
        "wait_for",
        json!({"until": {"state": "overlay", "equals": "settings", "exists": true}}),
    );

    assert_eq!(failed.code, ErrorCode::BadParams);
    assert!(
        failed.message.contains("give equals or exists, not both"),
        "{}",
        failed.message
    );
}
