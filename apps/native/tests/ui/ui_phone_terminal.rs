//! The phone's terminal slot: one session's live terminal held by the root
//! under the session's pop-out slot id, attached with a scrollback load,
//! claiming the PTY size, drawn at the phone's own font size, and detached
//! when it closes, with no tab, layout or pop-out window.

use crate::support;

use daemon_client::device_tokens::DeviceTokenStore as _;
use gpui::{TestAppContext, WindowOptions};
use protocol::{ClientMessage, DaemonMessage, SessionSnapshot};
use rustling_tulip_native::chat::PaneView;
use rustling_tulip_native::target::{HostKey, Target};
use rustling_tulip_native::{Connection, NetEvent};
use serde_json::json;
use support::{Harness, REMOTE, TestDir, repo, saved_remote, seed_ui, session};

/// `a` working with a mirrored chat, `n` working with no agent, both in r1.
fn listed() -> Vec<SessionSnapshot> {
    vec![
        session("a")
            .label("Alpha")
            .in_repo("r1")
            .status("working")
            .build(),
        session("n")
            .label("November")
            .in_repo("r1")
            .status("working")
            .no_agent()
            .build(),
    ]
}

/// A phone with the saved [`REMOTE`] open, listing [`listed`], everything
/// sent so far drained.
fn open_phone<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    seed_ui(
        dir,
        &json!({ "saved_hosts": [{ "name": REMOTE, "port": 8787 }] }),
    );
    let mut h = Harness::phone(cx, dir);
    h.tokens().set(REMOTE, "t1").expect("seed a token");
    let mut host = h.host(&saved_remote());
    host.connect();
    host.send(DaemonMessage::Repos {
        repos: vec![repo("r1", "C:/r1")],
    });
    host.send(DaemonMessage::Sessions { sessions: listed() });
    h.host(&saved_remote()).sent();
    h
}

/// The saved host's key.
fn host() -> HostKey {
    HostKey::of(&saved_remote())
}

/// The slot id the phone's terminal for session `id` lives under: the
/// session's pop-out slot id.
fn slot_id(id: &str) -> String {
    let wire = host()
        .wire()
        .expect("a tailnet host has a wire key")
        .to_owned();
    format!("popout:{wire}:{id}")
}

/// Opens the phone's terminal for session `id`.
fn open_terminal(h: &mut Harness<'_>, id: &str) {
    let host = host();
    h.root_update(|root, window, cx| root.open_phone_terminal(&host, id, window, cx));
}

/// Closes the phone's terminal.
fn close_terminal(h: &mut Harness<'_>) {
    h.root_update(|root, _, cx| root.close_phone_terminal(cx));
}

/// A phone with session `id`'s terminal open, everything sent so far drained.
fn opened<'a>(cx: &'a mut TestAppContext, dir: &TestDir, id: &str) -> Harness<'a> {
    let mut h = open_phone(cx, dir);
    open_terminal(&mut h, id);
    h.host(&saved_remote()).sent();
    h
}

/// How many `LoadScrollback`s for session `id` `sent` holds.
fn scrollback_loads(sent: &[ClientMessage], id: &str) -> usize {
    sent.iter()
        .filter(|msg| {
            matches!(msg, ClientMessage::LoadScrollback { session_id, .. } if session_id == id)
        })
        .count()
}

/// How many `Detach`es for session `id` `sent` holds.
fn detaches(sent: &[ClientMessage], id: &str) -> usize {
    sent.iter()
        .filter(|msg| matches!(msg, ClientMessage::Detach { session_id } if session_id == id))
        .count()
}

/// The wire type names of `sent`.
fn wire_types(sent: &[ClientMessage]) -> Vec<String> {
    sent.iter()
        .map(|msg| {
            serde_json::to_value(msg).expect("a client message serializes")["type"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        })
        .collect()
}

/// The font size the phone's terminal draws at.
fn terminal_font_size(h: &mut Harness<'_>) -> Option<f32> {
    h.root(|root, cx| {
        root.phone_terminal_view()
            .map(|view| view.read(cx).font().size)
    })
}

/// The host's connection drops and comes back, saying `Welcome` and listing
/// its sessions again.
fn redial(h: &mut Harness<'_>) {
    let target: Target = saved_remote();
    let mut conn = Connection::for_target(target.clone());
    conn.on_connecting();
    conn.on_socket_open(8787);
    let _ = conn.on_closed("dropped".to_owned());
    h.host(&target).event(NetEvent::State(conn));
    let mut host = h.host(&target);
    host.connect();
    host.send(DaemonMessage::Sessions { sessions: listed() });
}

#[gpui::test]
fn opening_the_terminal_loads_scrollback_for_the_session(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir);

    open_terminal(&mut h, "a");

    let sent = h.host(&saved_remote()).sent();
    assert_eq!(scrollback_loads(&sent, "a"), 1, "one load: {sent:?}");
    assert!(
        !sent
            .iter()
            .any(|msg| matches!(msg, ClientMessage::Attach { .. })),
        "no Attach: {sent:?}"
    );
    assert!(h.root(|root, _| root.phone_terminal_view()).is_some());
}

/// Draws the phone's terminal in a window of its own, as the page that
/// draws it comes later.
fn draw_terminal(h: &mut Harness<'_>) {
    let pane = h
        .root(|root, _| root.phone_terminal_view())
        .expect("the terminal is open");
    h.cx.update(|_, cx| {
        cx.open_window(WindowOptions::default(), |_, _| pane)
            .expect("a window for the terminal");
    });
    h.cx.run_until_parked();
}

/// Whether the first `Resize` for session `id` in `sent` claims, if any.
fn first_resize_claim(sent: &[ClientMessage], id: &str) -> Option<bool> {
    sent.iter().find_map(|msg| match msg {
        ClientMessage::Resize {
            session_id, claim, ..
        } if session_id == id => Some(*claim),
        _ => None,
    })
}

#[gpui::test]
fn the_first_resize_claims_the_pty_size(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir, "a");

    draw_terminal(&mut h);

    let sent = h.host(&saved_remote()).sent();
    let first = first_resize_claim(&sent, "a");
    assert_eq!(first, Some(true), "the first resize claims: {sent:?}");
}

#[gpui::test]
fn a_redial_claims_the_size_again(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir, "a");
    draw_terminal(&mut h);
    let sent = h.host(&saved_remote()).sent();
    assert_eq!(first_resize_claim(&sent, "a"), Some(true), "{sent:?}");

    redial(&mut h);

    let sent = h.host(&saved_remote()).sent();
    assert_eq!(scrollback_loads(&sent, "a"), 1, "attached again: {sent:?}");
    assert_eq!(
        first_resize_claim(&sent, "a"),
        Some(true),
        "the re-attach claims the size: {sent:?}"
    );
}

#[gpui::test]
fn opening_before_the_first_list_still_claims(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({ "saved_hosts": [{ "name": REMOTE, "port": 8787 }] }),
    );
    let mut h = Harness::phone(cx, &dir);
    h.tokens().set(REMOTE, "t1").expect("seed a token");
    let mut host = h.host(&saved_remote());
    host.connect();
    host.send(DaemonMessage::Repos {
        repos: vec![repo("r1", "C:/r1")],
    });
    open_terminal(&mut h, "a");
    draw_terminal(&mut h);
    h.host(&saved_remote()).sent();

    h.host(&saved_remote())
        .send(DaemonMessage::Sessions { sessions: listed() });
    h.cx.run_until_parked();

    let sent = h.host(&saved_remote()).sent();
    assert_eq!(
        scrollback_loads(&sent, "a"),
        1,
        "the list attaches it: {sent:?}"
    );
    assert_eq!(
        first_resize_claim(&sent, "a"),
        Some(true),
        "its first resize claims: {sent:?}"
    );
}

#[gpui::test]
fn closing_the_terminal_sends_detach(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir, "a");

    close_terminal(&mut h);

    let sent = h.host(&saved_remote()).sent();
    assert_eq!(detaches(&sent, "a"), 1, "one detach: {sent:?}");
    assert!(h.root(|root, _| root.phone_terminal_view()).is_none());

    close_terminal(&mut h);
    let sent = h.host(&saved_remote()).sent();
    assert!(sent.is_empty(), "closing nothing sends nothing: {sent:?}");
}

#[gpui::test]
fn opening_another_session_closes_the_first(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir, "a");

    open_terminal(&mut h, "n");

    let sent = h.host(&saved_remote()).sent();
    assert_eq!(detaches(&sent, "a"), 1, "a is detached: {sent:?}");
    assert_eq!(scrollback_loads(&sent, "n"), 1, "n is loaded: {sent:?}");
    assert_eq!(scrollback_loads(&sent, "a"), 0, "a is not: {sent:?}");
    assert!(
        h.root(|root, _| root.pane_terminal(&slot_id("a")))
            .is_none(),
        "a's slot is gone"
    );
}

#[gpui::test]
fn a_redial_attaches_it_again(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir, "a");

    redial(&mut h);

    let sent = h.host(&saved_remote()).sent();
    assert_eq!(
        scrollback_loads(&sent, "a"),
        1,
        "the new connection loads it again: {sent:?}"
    );
    assert!(h.root(|root, _| root.phone_terminal_view()).is_some());
}

#[gpui::test]
fn a_tab_list_from_the_host_does_not_detach_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir, "a");

    h.host(&saved_remote())
        .send(DaemonMessage::Tabs { tabs: Vec::new() });
    h.cx.run_until_parked();

    let sent = h.host(&saved_remote()).sent();
    assert_eq!(detaches(&sent, "a"), 0, "no detach: {sent:?}");
    assert!(h.root(|root, _| root.phone_terminal_view()).is_some());
}

#[gpui::test]
fn no_tab_or_layout_message_is_sent(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir);

    open_terminal(&mut h, "a");
    close_terminal(&mut h);

    let sent = h.host(&saved_remote()).sent();
    let types = wire_types(&sent);
    assert!(
        !types
            .iter()
            .any(|kind| kind.contains("tab") || kind.contains("layout")),
        "no tab or layout message: {types:?}"
    );
}

#[gpui::test]
fn the_phone_terminal_shows_its_terminal_for_an_agent_session(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir, "a");
    let slot = slot_id("a");

    assert!(
        h.root(|root, _| root.pane_terminal(&slot)).is_some(),
        "the terminal lives in the session's pop-out slot"
    );
    assert_eq!(h.root(|root, _| root.pane_view(&slot)), PaneView::Terminal);
    assert!(!h.root(|root, _| root.pane_shows_chat(&slot)));
}

#[gpui::test]
fn the_phone_terminal_uses_the_phone_font(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir, "a");

    assert_eq!(terminal_font_size(&mut h), Some(11.0));

    h.host(&saved_remote())
        .send(DaemonMessage::Sessions { sessions: listed() });
    h.cx.run_until_parked();

    assert_eq!(
        terminal_font_size(&mut h),
        Some(11.0),
        "a sessions list keeps the phone's size"
    );
}
