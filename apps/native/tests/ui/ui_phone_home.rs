//! The phone's Home: every saved host's sessions under their repos, rows
//! that never move and are at least 44 pt tall, a header per host while
//! more than one is listed, and a tap that shows a session page without
//! sending anything.

use crate::support;

use daemon_client::device_tokens::DeviceTokenStore as _;
use gpui::{Bounds, Pixels, TestAppContext, px};
use protocol::{ClientMessage, DEVICE_NOT_APPROVED, DaemonMessage, SessionSnapshot};
use rustling_tulip_native::phone::PhoneRoot;
use rustling_tulip_native::target::{HostKey, Target};
use rustling_tulip_native::{Connection, NetEvent};
use serde_json::json;
use support::{Harness, REMOTE, TestDir, repo, saved_remote, seed_ui, session};

/// The second tailnet host the two-host specs save.
const DEN: &str = "den.tail1234.ts.net";

/// The sessions each spec host lists, in Home's order.
const IDS: [&str; 3] = ["a", "b", "c"];

/// The least height of a row or a target.
fn touch_target() -> Pixels {
    px(44.0)
}

fn den() -> Target {
    Target::tailnet(DEN, Some(8787))
}

/// The key `target`'s header and rows are tagged with.
fn wire(target: &Target) -> String {
    HostKey::of(target)
        .wire()
        .expect("a tailnet host has a wire key")
        .to_owned()
}

/// The selector of `target`'s row for session `id`.
fn row(target: &Target, id: &str) -> String {
    format!("phone-home-row-{}::{id}", wire(target))
}

/// Whether the last frame drew what `selector` tags.
fn drawn(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).size.width > px(0.0)
}

/// The id of the session the phone shows in place of Home.
fn shown(h: &mut Harness<'_>) -> Option<String> {
    h.cx.update(|window, cx| {
        window
            .root::<PhoneRoot>()
            .flatten()
            .and_then(|phone| phone.read(cx).shown_session().map(|(_, id)| id.to_owned()))
    })
}

/// Session `c` in r2: idle, or waiting on a question.
fn session_c(asking: bool) -> SessionSnapshot {
    let c = session("c").label("Cleanup").in_repo("r2");
    if !asking {
        return c.status("idle").build();
    }
    c.status("awaiting_input")
        .pending_input(json!({
            "kind": "question",
            "questions": [{
                "question": "keep newest or merge?",
                "header": "Migration",
                "options": [],
                "multi_select": false,
            }],
        }))
        .build()
}

fn listed() -> Vec<SessionSnapshot> {
    vec![
        session("a")
            .label("Alpha")
            .in_repo("r1")
            .status("working")
            .build(),
        session("b")
            .label("Bravo")
            .in_repo("r2")
            .status("working")
            .build(),
        session_c(false),
    ]
}

/// Opens `target` listing repos r1 and r2: `a` working in r1, `b` working
/// and `c` idle in r2.
fn open_host(h: &mut Harness<'_>, target: &Target) {
    let mut host = h.host(target);
    host.connect();
    host.send(DaemonMessage::Repos {
        repos: vec![repo("r1", "C:/r1"), repo("r2", "C:/r2")],
    });
    host.send(DaemonMessage::Sessions { sessions: listed() });
}

/// A phone with the saved [`REMOTE`] open as [`open_host`] lists it.
fn open_phone<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    seed_ui(
        dir,
        &json!({ "saved_hosts": [{ "name": REMOTE, "port": 8787 }] }),
    );
    let mut h = Harness::phone(cx, dir);
    h.tokens().set(REMOTE, "t1").expect("seed a token");
    open_host(&mut h, &saved_remote());
    h
}

/// A phone with [`REMOTE`] and [`DEN`] saved, each holding a token.
fn two_host_phone<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    seed_ui(
        dir,
        &json!({
            "saved_hosts": [
                { "name": REMOTE, "port": 8787 },
                { "name": DEN, "port": 8787 },
            ]
        }),
    );
    let h = Harness::phone(cx, dir);
    h.tokens().set(REMOTE, "t1").expect("seed a token");
    h.tokens().set(DEN, "t2").expect("seed a token");
    h
}

/// `target`'s connection dropped after it was open.
fn drop_connection(h: &mut Harness<'_>, target: &Target) {
    let mut conn = Connection::for_target(target.clone());
    conn.on_connecting();
    conn.on_socket_open(8787);
    let _ = conn.on_closed("dropped".to_owned());
    h.host(target).event(NetEvent::State(conn));
    h.cx.run_until_parked();
}

fn row_bounds(h: &mut Harness<'_>, targets: &[Target]) -> Vec<Bounds<Pixels>> {
    targets
        .iter()
        .flat_map(|target| IDS.map(|id| row(target, id)))
        .map(|selector| h.bounds(&selector))
        .collect()
}

#[gpui::test]
fn home_draws_every_row_at_least_44_high(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir);

    assert!(drawn(&mut h, "phone-home"));
    for id in IDS {
        let selector = row(&saved_remote(), id);
        assert!(drawn(&mut h, &selector), "{selector} is drawn");
        let height = h.bounds(&selector).size.height;
        assert!(height >= touch_target(), "{selector} is {height:?} high");
    }
}

#[gpui::test]
fn rows_do_not_move_when_a_session_starts_needing_you(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir);
    let before = row_bounds(&mut h, &[saved_remote()]);

    h.host(&saved_remote()).send(DaemonMessage::SessionUpdated {
        session: session_c(true),
        request_id: None,
    });
    h.cx.run_until_parked();

    assert_eq!(
        row_bounds(&mut h, &[saved_remote()]),
        before,
        "every row keeps its place"
    );
}

#[gpui::test]
fn a_tap_shows_the_session_page_and_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir);
    h.host(&saved_remote()).sent();

    h.click_on(&row(&saved_remote(), "b"));

    assert_eq!(shown(&mut h).as_deref(), Some("b"));
    assert!(drawn(&mut h, "phone-session"), "the session page shows");
    assert!(drawn(&mut h, "phone-session-label"));
    assert!(drawn(&mut h, "phone-session-back"));
    assert!(
        h.bounds("phone-session-back").size.height >= touch_target(),
        "Back is a 44 pt target"
    );
    assert!(!drawn(&mut h, "phone-home"), "the page replaces Home");
    let sent: Vec<ClientMessage> = h.host(&saved_remote()).sent();
    assert!(sent.is_empty(), "a tap sends nothing: {sent:?}");
}

#[gpui::test]
fn back_returns_to_home(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir);
    h.click_on(&row(&saved_remote(), "a"));
    h.host(&saved_remote()).sent();

    h.click_on("phone-session-back");

    assert_eq!(shown(&mut h), None);
    assert!(drawn(&mut h, "phone-home"), "Home shows again");
    assert!(!drawn(&mut h, "phone-session"));
    assert!(
        h.host(&saved_remote()).sent().is_empty(),
        "Back sends nothing"
    );
}

#[gpui::test]
fn a_shown_session_the_host_removes_returns_to_home(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir);
    h.click_on(&row(&saved_remote(), "c"));
    assert_eq!(shown(&mut h).as_deref(), Some("c"));

    let mut remaining = listed();
    remaining.retain(|s| s.id != "c");
    h.host(&saved_remote()).send(DaemonMessage::Sessions {
        sessions: remaining,
    });
    h.cx.run_until_parked();

    assert_eq!(shown(&mut h), None, "the gone session is no longer shown");
    assert!(drawn(&mut h, "phone-home"), "Home shows again");
    assert!(!drawn(&mut h, &row(&saved_remote(), "c")));
}

#[gpui::test]
fn a_host_refusing_the_token_shows_its_header_with_pair(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_host_phone(cx, &dir);
    open_host(&mut h, &saved_remote());
    let mut refused = Connection::for_target(den());
    refused.on_connecting();
    refused.on_socket_open(8787);
    refused.on_not_approved(DEVICE_NOT_APPROVED.to_owned(), true);
    h.host(&den()).event(NetEvent::State(refused));

    let den_wire = wire(&den());
    assert!(drawn(
        &mut h,
        &format!("phone-home-host-{}", wire(&saved_remote()))
    ));
    assert!(drawn(&mut h, &format!("phone-home-host-{den_wire}")));
    assert!(drawn(&mut h, &format!("phone-home-host-status-{den_wire}")));
    let pair = format!("phone-home-host-pair-{den_wire}");
    assert!(drawn(&mut h, &pair), "the refusing host offers Pair");
    assert!(
        h.bounds(&pair).size.height >= touch_target(),
        "Pair is a 44 pt target"
    );
    assert!(!h.root(|root, _| root.device_pairing_open()));

    h.click_on(&pair);

    assert!(
        h.root(|root, _| root.device_pairing_open()),
        "Pair opens the pairing flow"
    );
}

#[gpui::test]
fn rows_do_not_move_when_a_host_disconnects(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_host_phone(cx, &dir);
    open_host(&mut h, &saved_remote());
    open_host(&mut h, &den());
    let hosts = [saved_remote(), den()];
    let before = row_bounds(&mut h, &hosts);
    let status = format!("phone-home-host-status-{}", wire(&den()));
    assert!(
        !drawn(&mut h, &status),
        "an open host's header says nothing"
    );

    drop_connection(&mut h, &den());

    assert_eq!(
        row_bounds(&mut h, &hosts),
        before,
        "both hosts' rows keep their place"
    );
    assert!(drawn(&mut h, &status), "the dropped host's header says so");
}

#[gpui::test]
fn the_session_page_stays_while_its_host_disconnects(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_host_phone(cx, &dir);
    open_host(&mut h, &saved_remote());
    open_host(&mut h, &den());
    h.click_on(&row(&den(), "b"));
    assert_eq!(shown(&mut h).as_deref(), Some("b"));

    drop_connection(&mut h, &den());

    assert_eq!(shown(&mut h).as_deref(), Some("b"), "the page stays");
    assert!(drawn(&mut h, "phone-session"));
    assert!(drawn(&mut h, "phone-session-label"), "with its label");

    h.click_on("phone-session-back");

    assert_eq!(shown(&mut h), None);
    assert!(drawn(&mut h, "phone-home"), "Back returns to Home");
}
