//! The driver's requests (feature `driver`), answered in-process against the
//! harness window: version, state, the registry's names and keys.
#![cfg(feature = "driver")]

use crate::support;

use gpui::TestAppContext;
use rustling_tulip_native::driver::handlers::handle;
use rustling_tulip_native::driver::protocol::{ErrorCode, Failure, Outcome, Reply, Request};
use serde_json::{Value, json};
use support::{Fixture, Harness, TestDir, repo, session};

/// Repos `r1` and `r2`, one session of `r1` alone in pane `p1` of tab `t1`.
fn two_repos() -> Fixture {
    let mut fixture = Fixture::single(session("s1").in_repo("r1").build());
    fixture.repos = vec![repo("r1", "D:/src/r1"), repo("r2", "D:/src/r2")];
    fixture
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
        Some("methods: find, key, ping, screenshot, state")
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
