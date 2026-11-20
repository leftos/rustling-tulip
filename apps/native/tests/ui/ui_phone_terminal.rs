//! The phone's terminal slot: one session's live terminal held by the root
//! under the session's pop-out slot id, attached with a scrollback load,
//! claiming the PTY size, drawn at the phone's own font size, and detached
//! when it closes, with no tab, layout or pop-out window.

use crate::support;

use daemon_client::device_tokens::DeviceTokenStore as _;
use gpui::{
    Edges, Modifiers, Pixels, Point, ScrollDelta, ScrollEvent, TestAppContext, TouchPhase,
    WindowInsets, WindowOptions, point, px,
};
use protocol::{
    ChatEntry, ChatOption, ChatPrompt, ChatPromptKind, ChatScreen, ClientMessage, ComposerState,
    DaemonMessage, SessionSnapshot,
};
use rustling_tulip_native::chat::PaneView;
use rustling_tulip_native::keyboard::{KeyboardCall, clear_keyboard_calls, keyboard_calls};
use rustling_tulip_native::phone::PhoneRoot;
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

/// The rows of the first `Resize` for session `id` in `sent`, if any.
fn first_resize_rows(sent: &[ClientMessage], id: &str) -> Option<u16> {
    sent.iter().find_map(|msg| match msg {
        ClientMessage::Resize {
            session_id, rows, ..
        } if session_id == id => Some(*rows),
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

// ── The Terminal page ──────────────────────────────────────────────────────

/// The least height of a touch target.
fn touch_target() -> Pixels {
    px(44.0)
}

/// The key bar's keys' driver ids, in the order it draws them.
const BAR_KEYS: [&str; 9] = [
    "phone-bar-esc",
    "phone-bar-tab",
    "phone-bar-ctrl",
    "phone-bar-up",
    "phone-bar-down",
    "phone-bar-left",
    "phone-bar-right",
    "phone-bar-paste",
    "phone-bar-quick",
];

/// The selector of the saved host's Home row for session `id`.
fn row(id: &str) -> String {
    let wire = host()
        .wire()
        .expect("a tailnet host has a wire key")
        .to_owned();
    format!("phone-home-row-{wire}::{id}")
}

/// Whether the last frame drew what `selector` tags.
fn drawn(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).size.width > px(0.0)
}

/// Runs `drive` on the phone's root and lets the window settle.
fn phone_update(
    h: &mut Harness<'_>,
    drive: impl FnOnce(&mut PhoneRoot, &mut gpui::Context<PhoneRoot>),
) {
    h.cx.update(|window, cx| {
        let phone = window
            .root::<PhoneRoot>()
            .flatten()
            .expect("the window's root is the phone's");
        phone.update(cx, drive);
    });
    h.cx.run_until_parked();
}

/// A phone showing session `id`'s page, tapped open from Home, everything
/// sent so far drained.
fn page_of<'a>(cx: &'a mut TestAppContext, dir: &TestDir, id: &str) -> Harness<'a> {
    let mut h = open_phone(cx, dir);
    h.click_on(&row(id));
    h.host(&saved_remote()).sent();
    h
}

/// Shell session `n` on its Terminal page, its scrollback answered and
/// everything sent so far drained.
fn on_terminal<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = open_phone(cx, dir);
    h.click_on(&row("n"));
    h.host(&saved_remote()).pty("n", b"");
    h.cx.run_until_parked();
    h.host(&saved_remote()).sent();
    h
}

/// The insets a phone draws with a `keyboard`-tall on-screen keyboard up.
fn keyboard_insets(keyboard: Pixels) -> WindowInsets {
    WindowInsets {
        safe_area: Edges {
            top: px(62.0),
            bottom: px(34.0),
            ..Edges::default()
        },
        ime: Edges {
            bottom: keyboard,
            ..Edges::default()
        },
        ..WindowInsets::default()
    }
}

#[gpui::test]
fn the_terminal_page_draws_the_terminal_and_the_key_bar(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_terminal(cx, &dir);

    assert!(drawn(&mut h, "phone-session-terminal"));
    assert!(drawn(&mut h, "phone-key-bar"));
    let terminal = h.bounds("phone-session-terminal");
    let bar = h.bounds("phone-key-bar");
    assert!(
        bar.origin.y + px(1.0) >= terminal.bottom(),
        "the bar sits under the terminal: {bar:?}, terminal {terminal:?}"
    );
    let mut left = px(-1.0);
    for key in BAR_KEYS {
        assert!(drawn(&mut h, key), "{key} is drawn");
        let bounds = h.bounds(key);
        assert!(bounds.size.height >= touch_target(), "{key} is {bounds:?}");
        assert!(bounds.origin.x > left, "{key} follows the key before it");
        left = bounds.origin.x;
    }
    assert!(
        h.root(|root, _| root.phone_terminal_view()).is_some(),
        "the page holds the session's terminal"
    );
}

#[gpui::test]
fn the_toggle_attaches_and_detaches(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = page_of(cx, &dir, "a");
    assert!(h.root(|root, _| root.phone_terminal_view()).is_none());

    h.click_on("phone-session-toggle");

    let sent = h.host(&saved_remote()).sent();
    assert_eq!(
        scrollback_loads(&sent, "a"),
        1,
        "the Terminal page attaches: {sent:?}"
    );
    assert!(h.root(|root, _| root.phone_terminal_view()).is_some());

    h.click_on("phone-session-toggle");

    let sent = h.host(&saved_remote()).sent();
    assert_eq!(detaches(&sent, "a"), 1, "the Chat page detaches: {sent:?}");
    assert!(h.root(|root, _| root.phone_terminal_view()).is_none());
}

#[gpui::test]
fn back_from_the_terminal_detaches_and_returns_to_chat(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = page_of(cx, &dir, "a");
    h.click_on("phone-session-toggle");
    h.host(&saved_remote()).sent();

    h.click_on("phone-session-back");

    let sent = h.host(&saved_remote()).sent();
    assert_eq!(detaches(&sent, "a"), 1, "Back detaches: {sent:?}");
    assert!(
        drawn(&mut h, "phone-session-chat"),
        "Back steps to the chat"
    );
    assert!(h.root(|root, _| root.phone_terminal_view()).is_none());
}

#[gpui::test]
fn a_shell_session_opens_on_its_terminal(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir);

    h.click_on(&row("n"));

    let sent = h.host(&saved_remote()).sent();
    assert_eq!(
        scrollback_loads(&sent, "n"),
        1,
        "it loads scrollback: {sent:?}"
    );
    assert!(drawn(&mut h, "phone-session-terminal"));
    assert!(drawn(&mut h, "phone-key-bar"));
}

#[gpui::test]
fn another_session_shown_detaches_the_first(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_terminal(cx, &dir);

    phone_update(&mut h, |phone, cx| {
        assert!(phone.open_session_on("a", cx), "Home lists a");
    });

    let sent = h.host(&saved_remote()).sent();
    assert_eq!(detaches(&sent, "n"), 1, "n is detached: {sent:?}");
    assert_eq!(
        scrollback_loads(&sent, "a"),
        0,
        "a opens on its chat: {sent:?}"
    );
    assert!(h.root(|root, _| root.phone_terminal_view()).is_none());
}

#[gpui::test]
fn the_key_bar_arrows_send_cursor_keys(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_terminal(cx, &dir);

    h.click_on("phone-bar-up");
    assert_eq!(h.host(&saved_remote()).sent_input("n"), b"\x1b[A");

    h.host(&saved_remote()).pty("n", b"\x1b[?1h");
    h.cx.run_until_parked();
    h.host(&saved_remote()).sent();
    h.click_on("phone-bar-up");
    assert_eq!(
        h.host(&saved_remote()).sent_input("n"),
        b"\x1bOA",
        "application-cursor mode"
    );
}

/// The key bar's keys' driver ids: the whole grid in its order.
const GRID_BAR_KEYS: [&str; 21] = [
    "phone-bar-esc",
    "phone-bar-tab",
    "phone-bar-ctrl",
    "phone-bar-up",
    "phone-bar-down",
    "phone-bar-left",
    "phone-bar-right",
    "phone-bar-paste",
    "phone-bar-quick",
    "phone-bar-shift-tab",
    "phone-bar-enter",
    "phone-bar-ctrl-c",
    "phone-bar-1",
    "phone-bar-2",
    "phone-bar-3",
    "phone-bar-4",
    "phone-bar-5",
    "phone-bar-6",
    "phone-bar-7",
    "phone-bar-8",
    "phone-bar-9",
];

/// Resizes the phone to the window it draws at on a phone, narrow enough for
/// the key bar's keys to run off its right edge.
fn phone_width(h: &mut Harness<'_>) {
    h.cx.simulate_resize(gpui::size(px(390.0), px(844.0)));
}

/// Scrolls the key bar sideways until what `selector` tags lies inside its
/// scroller, as a finger's pan does.
fn scroll_bar_to(h: &mut Harness<'_>, selector: &str) {
    let at = h.center("phone-key-bar-scroll");
    for _ in 0..50 {
        let (outer, inner) = (h.bounds("phone-key-bar-scroll"), h.bounds(selector));
        let dx = if inner.right() > outer.right() {
            outer.right() - inner.right()
        } else if inner.left() < outer.left() {
            outer.left() - inner.left()
        } else {
            return;
        };
        h.cx.simulate_event(ScrollEvent {
            position: at,
            delta: ScrollDelta::Pixels(point(dx, px(0.0))),
            modifiers: Modifiers::none(),
            touch_phase: TouchPhase::Moved,
            source: gpui::ScrollSource::Pointer,
        });
        h.cx.run_until_parked();
    }
    let (outer, inner) = (h.bounds("phone-key-bar-scroll"), h.bounds(selector));
    assert!(
        inner.left() >= outer.left() && inner.right() <= outer.right() + px(1.0),
        "{selector} does not show in the key bar after 50 scrolls: {inner:?} in {outer:?}"
    );
}

#[gpui::test]
fn the_key_bar_is_one_row_of_the_grid_in_order(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_terminal(cx, &dir);
    phone_width(&mut h);

    let mut row: Option<Pixels> = None;
    let mut left = px(-1.0);
    for key in GRID_BAR_KEYS {
        let bounds = h.bounds(key);
        assert!(bounds.size.width > px(0.0), "{key} is drawn: {bounds:?}");
        let y = bounds.origin.y;
        assert_eq!(
            *row.get_or_insert(y),
            y,
            "{key} sits on the bar's one row: {bounds:?}"
        );
        assert!(
            bounds.origin.x > left,
            "{key} follows the key before it: {bounds:?}"
        );
        left = bounds.origin.x;
    }
}

#[gpui::test]
fn the_key_bar_scrolls_sideways_to_its_last_key(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_terminal(cx, &dir);
    phone_width(&mut h);
    let bar = h.bounds("phone-key-bar-scroll");
    let last = h.bounds("phone-bar-9");
    assert!(
        last.right() > bar.right(),
        "the last key starts off the bar: {last:?} in {bar:?}"
    );

    scroll_bar_to(&mut h, "phone-bar-9");

    let (bar, last) = (h.bounds("phone-key-bar-scroll"), h.bounds("phone-bar-9"));
    assert!(
        last.left() >= bar.left() && last.right() <= bar.right() + px(1.0),
        "the sideways scroll brings the last key inside the bar: {last:?} in {bar:?}"
    );
}

#[gpui::test]
fn the_key_bar_stays_one_row_tall(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_terminal(cx, &dir);
    phone_width(&mut h);
    let bar = h.bounds("phone-key-bar");
    let key = h.bounds("phone-bar-esc");
    assert!(
        bar.size.height <= key.size.height + px(8.0),
        "the bar is one key tall: {bar:?}, a key {key:?}"
    );

    draw_keyboard(&mut h, true);

    assert_eq!(
        h.bounds("phone-key-bar").size.height,
        bar.size.height,
        "Hide stays on the bar's row"
    );
}

#[gpui::test]
fn the_key_bar_keeps_hide_pinned_at_its_right_end(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_terminal(cx, &dir);
    phone_width(&mut h);
    draw_keyboard(&mut h, true);
    let hide = h.bounds("phone-bar-hide");

    scroll_bar_to(&mut h, "phone-bar-9");

    let scrolled = h.bounds("phone-bar-hide");
    assert_eq!(
        scrolled.origin.x, hide.origin.x,
        "Hide stays put while the keys scroll under it: {scrolled:?} was {hide:?}"
    );
    let last = h.bounds("phone-bar-9");
    assert!(
        last.right() <= scrolled.left() + px(1.0),
        "the scrolled keys end before Hide: {last:?}, Hide {scrolled:?}"
    );
}

/// The point `dx` points past the centre of cell (`col`, 0) of the terminal
/// `pane` holds, as the desktop's own selection specs take theirs.
fn cell_edge(h: &mut Harness<'_>, pane: &str, col: usize, dx: f32) -> Point<Pixels> {
    let at = h
        .root(|root, cx| root.pane_cell_center(pane, col, 0, cx))
        .expect("the terminal pane is laid out");
    point(at.x + px(dx), at.y)
}

/// Whether the phone's terminal slot `pane` holds a selection.
fn selected(h: &mut Harness<'_>, pane: &str) -> bool {
    h.root(|root, cx| {
        root.pane_terminal(pane)
            .is_some_and(|view| view.read(cx).has_selection())
    })
}

#[gpui::test]
fn the_bar_ctrl_c_key_interrupts_with_text_selected(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_terminal(cx, &dir);
    // At its final width first: a resize reflows the terminal's columns, and
    // that clears any selection the drag had made.
    phone_width(&mut h);
    h.host(&saved_remote()).pty("n", b"hello world");
    h.cx.run_until_parked();
    let slot = slot_id("n");
    let (from, to) = (
        cell_edge(&mut h, &slot, 0, -2.0),
        cell_edge(&mut h, &slot, 4, 2.0),
    );
    let terminal = h.bounds("phone-session-terminal");
    assert!(
        terminal.contains(&from) && terminal.contains(&to),
        "the selection drags inside the terminal: {from:?} to {to:?} in {terminal:?}"
    );
    // A selection made as `ui_terminal`'s copy specs make one. The phone
    // drops the `Copied` its release emits (`drops_phone_event`), so the
    // selection is checked through the pane, not the clipboard.
    h.drag(from, to, [Modifiers::none(); 2]);
    assert!(selected(&mut h, &slot), "the drag selects text");
    h.set_clipboard("other");

    scroll_bar_to(&mut h, "phone-bar-ctrl-c");
    h.click_on("phone-bar-ctrl-c");

    assert!(
        !selected(&mut h, &slot),
        "the bar's send clears the selection, as every key typed into the terminal does"
    );
    assert_eq!(
        h.host(&saved_remote()).sent_input("n"),
        [0x03],
        "Ctrl+C interrupts a terminal with a selection instead of copying"
    );
    assert_eq!(
        h.clipboard().as_deref(),
        Some("other"),
        "and the bar's Ctrl+C copies nothing"
    );
}

#[gpui::test]
fn the_bar_enter_and_digit_keys_send_their_bytes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_terminal(cx, &dir);
    phone_width(&mut h);

    scroll_bar_to(&mut h, "phone-bar-enter");
    h.click_on("phone-bar-enter");
    assert_eq!(h.host(&saved_remote()).sent_input("n"), b"\r");

    scroll_bar_to(&mut h, "phone-bar-1");
    h.click_on("phone-bar-1");
    assert_eq!(h.host(&saved_remote()).sent_input("n"), b"1");
}

#[gpui::test]
fn armed_ctrl_is_left_armed_by_the_bar_ctrl_c_chord(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_terminal(cx, &dir);
    phone_width(&mut h);

    h.click_on("phone-bar-ctrl");
    scroll_bar_to(&mut h, "phone-bar-ctrl-c");
    h.click_on("phone-bar-ctrl-c");
    assert_eq!(
        h.host(&saved_remote()).sent_input("n"),
        [0x03],
        "Ctrl+C is a chord of its own"
    );

    scroll_bar_to(&mut h, "phone-bar-up");
    h.click_on("phone-bar-up");
    assert_eq!(
        h.host(&saved_remote()).sent_input("n"),
        b"\x1b[1;5A",
        "the armed Ctrl still waits for the next plain key"
    );
}

#[gpui::test]
fn ctrl_then_a_typed_c_sends_etx(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_terminal(cx, &dir);

    h.click_on("phone-bar-ctrl");
    h.commit(&slot_id("n"), "c");
    assert_eq!(h.host(&saved_remote()).sent_input("n"), [0x03]);

    h.commit(&slot_id("n"), "c");
    assert_eq!(
        h.host(&saved_remote()).sent_input("n"),
        b"c",
        "Ctrl is no longer armed"
    );
}

#[gpui::test]
fn a_second_ctrl_tap_disarms(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_terminal(cx, &dir);

    h.click_on("phone-bar-ctrl");
    h.click_on("phone-bar-ctrl");
    h.commit(&slot_id("n"), "c");

    assert_eq!(h.host(&saved_remote()).sent_input("n"), b"c");
}

/// A key as `native-ui.json` stores it.
fn grid_key(id: &str, label: &str, action: &serde_json::Value) -> serde_json::Value {
    json!({ "id": id, "label": label, "action": action })
}

/// A send action of one press of `key` with the modifiers given.
fn press(ctrl: bool, alt: bool, shift: bool, key: &serde_json::Value) -> serde_json::Value {
    json!({
        "kind": "send",
        "steps": [{ "ctrl": ctrl, "alt": alt, "shift": shift, "key": key }],
    })
}

/// A key of one press of the named key `name`.
fn named_key(id: &str, label: &str, name: &str) -> serde_json::Value {
    grid_key(
        id,
        label,
        &press(
            false,
            false,
            false,
            &json!({ "kind": "named", "name": name }),
        ),
    )
}

/// A phone showing session `n`'s Terminal page over `ui` saved beside its
/// host, everything sent so far drained.
fn on_terminal_saved<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    ui: &serde_json::Value,
) -> Harness<'a> {
    let mut file = ui.as_object().cloned().unwrap_or_default();
    file.insert(
        "saved_hosts".to_owned(),
        json!([{ "name": REMOTE, "port": 8787 }]),
    );
    seed_ui(dir, &serde_json::Value::Object(file));
    let mut h = Harness::phone(cx, dir);
    h.tokens().set(REMOTE, "t1").expect("seed a token");
    let mut host = h.host(&saved_remote());
    host.connect();
    host.send(DaemonMessage::Repos {
        repos: vec![repo("r1", "C:/r1")],
    });
    host.send(DaemonMessage::Sessions { sessions: listed() });
    h.host(&saved_remote()).sent();
    h.click_on(&row("n"));
    h.host(&saved_remote()).pty("n", b"");
    h.cx.run_until_parked();
    h.host(&saved_remote()).sent();
    h
}

#[gpui::test]
fn saved_grid_orders_the_bar(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let ui = json!({ "phone_keys": [
        named_key("enter", "Enter", "enter"),
        named_key("esc", "Esc", "esc"),
    ] });
    let mut h = on_terminal_saved(cx, &dir, &ui);
    phone_width(&mut h);

    let mut left = px(-1.0);
    for key in [
        "phone-bar-enter",
        "phone-bar-esc",
        "phone-bar-ctrl",
        "phone-bar-paste",
        "phone-bar-quick",
    ] {
        let bounds = h.bounds(key);
        assert!(bounds.size.width > px(0.0), "{key} is drawn: {bounds:?}");
        assert!(
            bounds.origin.x > left,
            "{key} follows the key before it: {bounds:?}"
        );
        left = bounds.origin.x;
    }
    assert!(
        !drawn(&mut h, "phone-bar-tab"),
        "a key the saved grid does not hold is not drawn"
    );
}

#[gpui::test]
fn saved_grid_without_paste_still_draws_paste(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let ui = json!({ "phone_keys": [named_key("esc", "Esc", "esc")] });
    let mut h = on_terminal_saved(cx, &dir, &ui);
    phone_width(&mut h);

    assert!(
        drawn(&mut h, "phone-bar-paste"),
        "a saved grid with no Paste still draws it"
    );
}

#[gpui::test]
fn saved_grid_unknown_key_sends_nothing_and_keeps_ctrl(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let ui = json!({ "phone_keys": [
        named_key("esc", "Esc", "esc"),
        grid_key("macro", "Macro", &json!({ "kind": "macro" })),
        named_key("up", "↑", "up"),
    ] });
    let mut h = on_terminal_saved(cx, &dir, &ui);
    phone_width(&mut h);

    assert!(
        drawn(&mut h, "phone-bar-macro"),
        "a key of a kind this build does not know is drawn"
    );
    h.click_on("phone-bar-ctrl");
    h.click_on("phone-bar-macro");
    assert!(
        h.host(&saved_remote()).sent_input("n").is_empty(),
        "an inert key sends nothing"
    );
    scroll_bar_to(&mut h, "phone-bar-up");
    h.click_on("phone-bar-up");
    assert_eq!(
        h.host(&saved_remote()).sent_input("n"),
        b"\x1b[1;5A",
        "the armed Ctrl is still armed for the next plain key"
    );
}

#[gpui::test]
fn saved_grid_chord_sends_its_bytes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let ui = json!({ "phone_keys": [
        grid_key(
            "ctrl-b",
            "Ctrl+B",
            &press(true, false, false, &json!({ "kind": "char", "value": "b" })),
        ),
    ] });
    let mut h = on_terminal_saved(cx, &dir, &ui);
    phone_width(&mut h);

    h.click_on("phone-bar-ctrl-b");
    assert_eq!(
        h.host(&saved_remote()).sent_input("n"),
        [0x02],
        "Ctrl+B is a chord of its own"
    );
}

#[gpui::test]
fn a_shift_enter_key_sends_backslash_return_to_a_shell(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let ui = json!({ "phone_keys": [
        grid_key(
            "shift-enter",
            "Shift+Enter",
            &press(false, false, true, &json!({ "kind": "named", "name": "enter" })),
        ),
    ] });
    let mut h = on_terminal_saved(cx, &dir, &ui);
    phone_width(&mut h);

    h.click_on("phone-bar-shift-enter");
    assert_eq!(
        h.host(&saved_remote()).sent_input("n"),
        b"\\\r",
        "a shell takes `\\` + CR for Shift+Enter"
    );
}

#[gpui::test]
fn paste_sends_the_clipboard_text_bracketed_when_asked(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_terminal(cx, &dir);
    h.host(&saved_remote()).pty("n", b"\x1b[?2004h");
    h.cx.run_until_parked();
    h.host(&saved_remote()).sent();
    h.set_clipboard("ls -la");

    h.click_on("phone-bar-paste");

    assert_eq!(
        h.host(&saved_remote()).sent_input("n"),
        b"\x1b[200~ls -la\x1b[201~"
    );
}

#[gpui::test]
fn paste_with_an_empty_clipboard_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_terminal(cx, &dir);
    h.set_clipboard("");

    h.click_on("phone-bar-paste");

    let sent = h.host(&saved_remote()).sent();
    assert!(
        !sent
            .iter()
            .any(|msg| matches!(msg, ClientMessage::SendInput { .. })),
        "no input: {sent:?}"
    );
    assert!(drawn(&mut h, "phone-bar-paste"), "the bar is drawn");
}

#[gpui::test]
fn quick_types_into_the_terminal(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_terminal(cx, &dir);

    h.click_on("phone-bar-quick");
    assert!(drawn(&mut h, "phone-terminal-quick"), "the dock opens");
    h.click_on("phone-terminal-quick-confirm-0");

    assert_eq!(h.host(&saved_remote()).sent_input("n"), b"y\r");
    assert!(!drawn(&mut h, "phone-terminal-quick"), "the dock closes");
}

#[gpui::test]
fn a_rising_keyboard_keeps_the_size(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_terminal(cx, &dir);
    phone_update(&mut h, |phone, cx| {
        phone.set_insets_override(Some(keyboard_insets(px(0.0))), cx);
    });
    h.host(&saved_remote()).sent();

    phone_update(&mut h, |phone, cx| {
        phone.set_insets_override(Some(keyboard_insets(px(336.0))), cx);
    });

    let sent = h.host(&saved_remote()).sent();
    assert_eq!(
        first_resize_claim(&sent, "n"),
        None,
        "the keyboard sends no resize: {sent:?}"
    );
}

#[gpui::test]
fn a_falling_keyboard_keeps_the_size(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_terminal(cx, &dir);
    phone_update(&mut h, |phone, cx| {
        phone.set_insets_override(Some(keyboard_insets(px(336.0))), cx);
    });
    h.host(&saved_remote()).sent();

    phone_update(&mut h, |phone, cx| {
        phone.set_insets_override(Some(keyboard_insets(px(0.0))), cx);
    });

    let sent = h.host(&saved_remote()).sent();
    assert_eq!(
        first_resize_claim(&sent, "n"),
        None,
        "the keyboard sends no resize: {sent:?}"
    );
}

/// Shell session `n`'s Terminal page opened under a `keyboard`-tall
/// on-screen keyboard, everything sent drained, with the rows of the first
/// `Resize` it sent.
fn opened_under<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    keyboard: Pixels,
) -> (Harness<'a>, Option<u16>) {
    let mut h = open_phone(cx, dir);
    phone_update(&mut h, |phone, cx| {
        phone.set_insets_override(Some(keyboard_insets(keyboard)), cx);
    });
    h.click_on(&row("n"));
    h.host(&saved_remote()).pty("n", b"");
    h.cx.run_until_parked();
    let sent = h.host(&saved_remote()).sent();
    (h, first_resize_rows(&sent, "n"))
}

/// The rows of the first `Resize` shell session `n` sends when its
/// Terminal page opens under a `keyboard`-tall on-screen keyboard.
fn rows_opened_under(cx: &mut TestAppContext, keyboard: Pixels) -> Option<u16> {
    let dir = TestDir::new();
    opened_under(cx, &dir, keyboard).1
}

/// Numbered lines `line 0` up to `line {count - 1}`, each ended, so the
/// cursor ends on the row under the last.
fn numbered_lines(count: usize) -> Vec<u8> {
    (0..count)
        .flat_map(|index| format!("line {index}\r\n").into_bytes())
        .collect()
}

/// Whether row `row` of the phone's terminal for `n` shows: its first
/// cell's centre lies inside the terminal's visible area.
fn shows_row(h: &mut Harness<'_>, row: usize) -> bool {
    let area = h.bounds("phone-session-terminal");
    let slot = slot_id("n");
    let at = h
        .root(|root, cx| root.pane_cell_center(&slot, 0, row, cx))
        .expect("the terminal has been laid out");
    area.contains(&at)
}

/// A vertical touch pan of `dy` over the phone's terminal, then the lift.
fn pan_terminal(h: &mut Harness<'_>, dy: Pixels) {
    let at = h.bounds("phone-session-terminal").center();
    let pan = |delta: Pixels, touch_phase: TouchPhase| ScrollEvent {
        position: at,
        delta: ScrollDelta::Pixels(point(px(0.0), delta)),
        modifiers: Modifiers::none(),
        touch_phase,
        source: gpui::ScrollSource::Touch,
    };
    h.cx.simulate_event(pan(px(0.0), TouchPhase::Started));
    h.cx.simulate_event(pan(dy, TouchPhase::Moved));
    h.cx.simulate_event(pan(px(0.0), TouchPhase::Ended));
    h.cx.run_until_parked();
}

/// The text the phone's terminal for `n` shows.
fn terminal_text(h: &mut Harness<'_>) -> String {
    h.root(|root, cx| {
        root.phone_terminal_view()
            .map(|pane| pane.read(cx).visible_text())
    })
    .expect("the terminal is open")
}

/// Whether `sent` holds any `Resize` or `SendInput` for session `id`.
fn sends_size_or_input(sent: &[ClientMessage], id: &str) -> bool {
    sent.iter().any(|msg| match msg {
        ClientMessage::Resize { session_id, .. } | ClientMessage::SendInput { session_id, .. } => {
            session_id == id
        }
        _ => false,
    })
}

#[gpui::test]
fn the_cursor_row_shows_with_the_keyboard_up(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, rows) = opened_under(cx, &dir, px(336.0));
    let rows = usize::from(rows.expect("the page claims a size"));
    assert!(shows_row(&mut h, 0), "a fresh screen's cursor row shows");

    h.host(&saved_remote()).pty("n", &numbered_lines(rows + 5));
    h.cx.run_until_parked();

    assert!(shows_row(&mut h, rows - 1), "the cursor's last row shows");
    assert!(!shows_row(&mut h, 0), "row 0 is under the keyboard's cover");
}

#[gpui::test]
fn a_drag_over_the_terminal_pans_without_sending(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, rows) = opened_under(cx, &dir, px(336.0));
    let rows = usize::from(rows.expect("the page claims a size"));
    h.host(&saved_remote()).pty("n", &numbered_lines(rows + 20));
    h.cx.run_until_parked();
    h.host(&saved_remote()).sent();
    let text = terminal_text(&mut h);
    assert!(!shows_row(&mut h, 0), "row 0 starts hidden");

    pan_terminal(&mut h, px(302.0));

    assert!(shows_row(&mut h, 0), "the drag pans row 0 into sight");
    assert_eq!(terminal_text(&mut h), text, "the pan scrolls no history");
    let sent = h.host(&saved_remote()).sent();
    assert!(
        !sends_size_or_input(&sent, "n"),
        "a pan sends nothing: {sent:?}"
    );

    draw_keyboard(&mut h, false);
    pan_terminal(&mut h, px(302.0));

    assert_ne!(
        terminal_text(&mut h),
        text,
        "with the keyboard down the drag scrolls the history"
    );
}

#[gpui::test]
fn a_terminal_opened_under_the_keyboard_takes_the_keyboard_down_rows(
    down: &mut TestAppContext,
    up: &mut TestAppContext,
) {
    let down_rows = rows_opened_under(down, px(0.0));
    let up_rows = rows_opened_under(up, px(336.0));

    assert!(down_rows.is_some(), "the page claims a size");
    assert_eq!(up_rows, down_rows, "the keyboard-down rows");
}

/// A pending single-select question `id` with two options.
fn question_prompt(id: &str) -> ChatPrompt {
    ChatPrompt {
        id: id.to_owned(),
        kind: ChatPromptKind::Question,
        subject: "calc.py".to_owned(),
        body: "Which one?".to_owned(),
        options: [(1, "sub"), (2, "div")]
            .iter()
            .map(|(number, label)| ChatOption {
                number: *number,
                label: (*label).to_owned(),
                description: None,
                selected: false,
            })
            .collect(),
        questions: Vec::new(),
        can_amend: true,
        always_allow_tip: None,
        multi: false,
        cursor: None,
        adjust: None,
    }
}

#[gpui::test]
fn the_prompt_sheet_draws_over_the_terminal_page(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = sheet_over_terminal(cx, &dir);

    assert!(drawn(&mut h, "phone-prompt-sheet"));
    assert!(drawn(&mut h, "phone-session-terminal"), "over the terminal");
    assert!(h.root(|root, _| root.phone_terminal_view()).is_some());
}

#[gpui::test]
fn a_drag_on_the_prompt_sheet_puts_it_away_with_the_keyboard_up(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = sheet_over_terminal(cx, &dir);
    draw_keyboard(&mut h, true);
    h.host(&saved_remote()).sent();
    assert!(drawn(&mut h, "phone-prompt-sheet"), "the sheet is up");

    drag_sheet(&mut h, px(60.0), 5);

    assert!(
        !drawn(&mut h, "phone-prompt-sheet"),
        "the drag puts it away"
    );
    let sent = h.host(&saved_remote()).sent();
    let claimed_or_typed = sent.iter().any(|msg| match msg {
        ClientMessage::Resize {
            session_id, claim, ..
        } => session_id == "a" && *claim,
        ClientMessage::SendInput { session_id, .. } => session_id == "a",
        _ => false,
    });
    assert!(
        !claimed_or_typed,
        "the drag types and claims nothing: {sent:?}"
    );
}

/// A downward pan over the prompt sheet's handle: `steps` movements of
/// `dy`, then the lift.
fn drag_sheet(h: &mut Harness<'_>, dy: Pixels, steps: usize) {
    let at = h.bounds("phone-prompt-handle").center();
    let pan = |delta: Pixels, touch_phase: TouchPhase| ScrollEvent {
        position: at,
        delta: ScrollDelta::Pixels(point(px(0.0), delta)),
        modifiers: Modifiers::none(),
        touch_phase,
        source: gpui::ScrollSource::Touch,
    };
    h.cx.simulate_event(pan(px(0.0), TouchPhase::Started));
    for _ in 0..steps {
        h.cx.simulate_event(pan(dy, TouchPhase::Moved));
    }
    h.cx.simulate_event(pan(px(0.0), TouchPhase::Ended));
    h.cx.run_until_parked();
}

/// Session `a` on its Terminal page with a pending question's prompt sheet
/// drawn over it.
fn sheet_over_terminal<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = open_phone(cx, dir);
    h.click_on(&row("a"));
    let sent = h.host(&saved_remote()).sent();
    let request = sent.iter().find_map(|msg| match msg {
        ClientMessage::LoadChat {
            session_id,
            request_id,
        } if session_id == "a" => Some(request_id.clone()),
        _ => None,
    });
    h.host(&saved_remote()).send(DaemonMessage::Chat {
        session_id: "a".to_owned(),
        entries: vec![ChatEntry::User {
            id: "u1".to_owned(),
            text: "message u1".to_owned(),
            pending: false,
        }],
        hidden: 0,
        screen: ChatScreen {
            composer: ComposerState::Ready,
            prompt: None,
        },
        request_id: request.flatten(),
    });
    h.click_on("phone-session-toggle");
    assert!(drawn(&mut h, "phone-session-terminal"));

    h.host(&saved_remote())
        .send(DaemonMessage::ChatScreenChanged {
            session_id: "a".to_owned(),
            screen: ChatScreen {
                composer: ComposerState::Dialog,
                prompt: Some(question_prompt("q1")),
            },
        });
    h.cx.run_until_parked();
    h
}

// ── Putting the keyboard away ──────────────────────────────────────────────

/// Draws the phone with the on-screen keyboard up or down.
fn draw_keyboard(h: &mut Harness<'_>, up: bool) {
    let keyboard = if up { px(336.0) } else { px(0.0) };
    phone_update(h, |phone, cx| {
        phone.set_insets_override(Some(keyboard_insets(keyboard)), cx);
    });
}

/// The last call the app made on the on-screen keyboard, if any.
fn last_keyboard_call() -> Option<KeyboardCall> {
    keyboard_calls().last().copied()
}

#[gpui::test]
fn the_key_bar_shows_hide_only_while_the_keyboard_is_up(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_terminal(cx, &dir);
    draw_keyboard(&mut h, false);
    assert!(
        !drawn(&mut h, "phone-bar-hide"),
        "no Hide key with the keyboard down"
    );

    draw_keyboard(&mut h, true);

    assert!(
        drawn(&mut h, "phone-bar-hide"),
        "Hide shows at the bar's end with the keyboard up"
    );
    let hide = h.bounds("phone-bar-hide");
    let quick = h.bounds("phone-bar-quick");
    assert!(
        hide.origin.x > quick.origin.x,
        "Hide is the bar's last key: {hide:?} after {quick:?}"
    );
    assert!(
        hide.size.height >= touch_target(),
        "Hide is a touch target: {hide:?}"
    );
}

#[gpui::test]
fn the_hide_key_lowers_the_keyboard(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_terminal(cx, &dir);
    draw_keyboard(&mut h, true);
    clear_keyboard_calls();

    h.tap_on("phone-bar-hide");

    assert_eq!(
        last_keyboard_call(),
        Some(KeyboardCall::Lower),
        "the bar's Hide key puts the keyboard away"
    );
}

#[gpui::test]
fn a_tap_on_the_terminal_with_the_keyboard_up_lowers_it_and_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_terminal(cx, &dir);
    draw_keyboard(&mut h, true);
    h.host(&saved_remote()).sent();
    clear_keyboard_calls();

    h.tap_on("phone-session-terminal");

    assert_eq!(
        last_keyboard_call(),
        Some(KeyboardCall::Lower),
        "the tap puts the keyboard away"
    );
    let sent = h.host(&saved_remote()).sent();
    assert!(
        !sent
            .iter()
            .any(|msg| matches!(msg, ClientMessage::SendInput { .. })),
        "the tap reaches no program: {sent:?}"
    );
}

#[gpui::test]
fn a_tap_on_the_terminal_with_the_keyboard_down_raises_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_terminal(cx, &dir);
    draw_keyboard(&mut h, false);
    clear_keyboard_calls();

    h.tap_on("phone-session-terminal");

    assert_eq!(
        last_keyboard_call(),
        Some(KeyboardCall::Raise),
        "the tap brings the keyboard up"
    );
}

/// The program in session `n` turns on mouse reports in SGR encoding, as a
/// TUI with clickable parts does.
fn program_reports_the_mouse(h: &mut Harness<'_>) {
    h.host(&saved_remote()).pty("n", b"\x1b[?1000h\x1b[?1006h");
    h.cx.run_until_parked();
    h.host(&saved_remote()).sent();
}

#[gpui::test]
fn a_tap_on_a_terminal_whose_program_reports_the_mouse_clicks_it_and_leaves_the_keyboard_down(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let mut h = on_terminal(cx, &dir);
    program_reports_the_mouse(&mut h);
    draw_keyboard(&mut h, false);
    clear_keyboard_calls();

    h.tap_on("phone-session-terminal");

    assert_eq!(keyboard_calls(), [], "the tap is the program's click");
    let sent = h.host(&saved_remote()).sent();
    assert!(
        sent.iter().any(
            |msg| matches!(msg, ClientMessage::SendInput { session_id, .. } if session_id == "n")
        ),
        "the click reaches the program: {sent:?}"
    );
}

#[gpui::test]
fn the_key_bar_raises_the_keyboard_only_while_a_tap_cannot(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_terminal(cx, &dir);
    draw_keyboard(&mut h, false);
    assert!(
        !drawn(&mut h, "phone-bar-keyboard"),
        "no Keyboard key while a tap raises the keyboard"
    );

    program_reports_the_mouse(&mut h);
    assert!(
        drawn(&mut h, "phone-bar-keyboard"),
        "a Keyboard key while the program takes the taps"
    );
    clear_keyboard_calls();
    h.click_on("phone-bar-keyboard");
    assert_eq!(
        last_keyboard_call(),
        Some(KeyboardCall::Raise),
        "the Keyboard key brings the keyboard up"
    );

    draw_keyboard(&mut h, true);
    assert!(
        !drawn(&mut h, "phone-bar-keyboard"),
        "Hide takes its place with the keyboard up"
    );
    assert!(drawn(&mut h, "phone-bar-hide"), "Hide shows");
}

#[gpui::test]
fn the_key_bar_fits_a_375_point_phone_with_the_keyboard_up(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_terminal(cx, &dir);
    h.cx.simulate_resize(gpui::size(px(375.0), px(812.0)));
    draw_keyboard(&mut h, true);

    let width = h.window_width();
    let hide = h.bounds("phone-bar-hide");
    assert!(
        f32::from(hide.right()) <= width - 16.0 + 1.0,
        "the bar ends inside the phone: {hide:?} in {width} px"
    );
    for key in BAR_KEYS.into_iter().chain(["phone-bar-hide"]) {
        let bounds = h.bounds(key);
        assert!(
            bounds.size.height >= touch_target(),
            "{key} is {bounds:?}, under a touch target"
        );
    }
}

// ── Tapped paths ───────────────────────────────────────────────────────────

/// A file path on the host.
const NOTES: &str = "/home/me/src/notes.txt";

/// A phone on session `id`'s page over a host that serves host paths,
/// browsing and folder fetches, everything sent so far drained.
fn on_paths_page<'a>(cx: &'a mut TestAppContext, dir: &TestDir, id: &str) -> Harness<'a> {
    seed_ui(
        dir,
        &json!({ "saved_hosts": [{ "name": REMOTE, "port": 8787 }] }),
    );
    let mut h = Harness::phone(cx, dir);
    h.tokens().set(REMOTE, "t1").expect("seed a token");
    let copies = dir.path().join("copies");
    h.root_update(move |root, _, _| root.set_remote_files_root(copies));
    let mut host = h.host(&saved_remote());
    host.connect_with_features(&[
        protocol::FEATURE_HOST_PATHS,
        protocol::FEATURE_HOST_BROWSE,
        protocol::FEATURE_FOLDER_FETCH,
    ]);
    host.send(DaemonMessage::Repos {
        repos: vec![repo("r1", REPO_ROOT)],
    });
    // `n` works in its repo's root, which its relative paths read against.
    let mut sessions = listed();
    sessions[1] = session("n")
        .label("November")
        .in_repo("r1")
        .status("working")
        .no_agent()
        .cwd(REPO_ROOT)
        .build();
    host.send(DaemonMessage::Sessions { sessions });
    h.click_on(&row(id));
    h.host(&saved_remote()).sent();
    h
}

/// The phone on session `n`'s terminal, whose output is `lines`, one per
/// row from the top.
fn terminal_showing<'a>(cx: &'a mut TestAppContext, dir: &TestDir, lines: &[&str]) -> Harness<'a> {
    let mut h = on_paths_page(cx, dir, "n");
    let output = lines.iter().fold(String::new(), |mut output, line| {
        output.push_str(line);
        output.push_str("\r\n");
        output
    });
    h.host(&saved_remote()).pty("n", output.as_bytes());
    h.cx.run_until_parked();
    h.host(&saved_remote()).sent();
    // A frame lays the grid out, so its cells have places.
    h.bounds("phone-session-terminal");
    h
}

/// The window point of the terminal's cell at `col`, `row`.
fn terminal_cell(h: &mut Harness<'_>, col: usize, row: usize) -> Point<Pixels> {
    h.root(|root, cx| {
        root.phone_terminal_view()
            .and_then(|view| view.read(cx).cell_center(col, row))
    })
    .expect("the terminal's grid is laid out")
}

/// A finger down and straight up on the terminal's cell at `col`, `row`.
fn tap_cell(h: &mut Harness<'_>, col: usize, row: usize) {
    let at = terminal_cell(h, col, row);
    h.touch(TouchPhase::Started, at);
    h.touch(TouchPhase::Ended, at);
}

/// The stats among `sent`: id and path.
fn stats(sent: &[ClientMessage]) -> Vec<(String, String)> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::StatHostPath { id, path } => Some((id.clone(), path.clone())),
            _ => None,
        })
        .collect()
}

/// The paths of the file and folder fetches among `sent`.
fn fetches(sent: &[ClientMessage]) -> Vec<String> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::FetchHostFile { path, .. }
            | ClientMessage::FetchHostFolder { path, .. } => Some(path.clone()),
            _ => None,
        })
        .collect()
}

/// The host answers the stat `id` with `kind`.
fn answer_stat(h: &mut Harness<'_>, id: &str, kind: protocol::PathKind) {
    h.host(&saved_remote()).send(DaemonMessage::PathStat {
        id: id.to_owned(),
        kind,
        size: 5,
        modified: Some(1_700_000_000_000),
    });
    h.cx.run_until_parked();
}

/// The one stat the phone sent since the last call, about `path`, answered
/// `kind`; then everything sent after it.
fn stat_answered(h: &mut Harness<'_>, path: &str, kind: protocol::PathKind) -> Vec<ClientMessage> {
    let sent = h.host(&saved_remote()).sent();
    let asked = stats(&sent);
    assert_eq!(asked.len(), 1, "one stat: {sent:?}");
    assert_eq!(asked[0].1, path, "the tapped path is asked about");
    answer_stat(h, &asked[0].0, kind);
    h.host(&saved_remote()).sent()
}

/// The fetch flow saved [`NOTES`]'s copy, as it hands it to the phone.
fn notes_saved(h: &mut Harness<'_>, dir: &TestDir) {
    let saved = rustling_tulip_native::PhoneFetched {
        host: REMOTE.to_owned(),
        host_path: NOTES.to_owned(),
        local_path: format!("{REMOTE}/root/home/me/src/notes.txt"),
        size: 5,
        host_label: REMOTE.to_owned(),
    };
    let path = dir.path().join("copies").join(&saved.local_path);
    std::fs::create_dir_all(path.parent().expect("a copy has a folder")).expect("make its folder");
    std::fs::write(&path, "text\n").expect("write the copy");
    h.root_update(|root, _, cx| root.phone_file_saved(saved, cx));
}

/// The phone's not-on-host line, while it shows.
fn path_note(h: &mut Harness<'_>) -> Option<String> {
    let mut note = None;
    phone_update(h, |phone, _| note = phone.path_note().map(str::to_owned));
    note
}

#[gpui::test]
fn a_tap_on_a_file_path_in_the_terminal_opens_it_in_the_viewer(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = terminal_showing(cx, &dir, &[NOTES]);
    clear_keyboard_calls();

    tap_cell(&mut h, 3, 0);

    assert_eq!(keyboard_calls(), [], "a tap on a link leaves the keyboard");
    let after = stat_answered(&mut h, NOTES, protocol::PathKind::File);
    assert_eq!(fetches(&after), [NOTES], "the file is fetched: {after:?}");
    notes_saved(&mut h, &dir);
    assert!(drawn(&mut h, "phone-viewer"), "the viewer opens on it");
}

#[gpui::test]
fn a_tap_on_a_folder_path_in_the_terminal_opens_the_browser_there(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = terminal_showing(cx, &dir, &["/home/me/src"]);

    tap_cell(&mut h, 3, 0);

    let after = stat_answered(&mut h, "/home/me/src", protocol::PathKind::Dir);
    assert_eq!(
        fetches(&after),
        Vec::<String>::new(),
        "nothing is fetched: {after:?}"
    );
    assert!(drawn(&mut h, "phone-browser"), "the browser opens");
    let mut path = None;
    phone_update(&mut h, |phone, _| {
        path = phone.browser_path().map(str::to_owned);
    });
    assert_eq!(
        path.as_deref(),
        Some("/home/me/src"),
        "at the tapped folder"
    );
}

#[gpui::test]
fn a_tap_on_a_path_not_on_the_host_says_so_on_the_session_page(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let gone = "/home/me/src/gone.txt";
    let mut h = terminal_showing(cx, &dir, &[gone, "", NOTES]);

    tap_cell(&mut h, 3, 0);
    let mut sent = h.host(&saved_remote()).sent();
    let mut all = Vec::new();
    while let Some((id, _)) = stats(&sent).first().cloned() {
        answer_stat(&mut h, &id, protocol::PathKind::Missing);
        all.extend(sent);
        sent = h.host(&saved_remote()).sent();
    }
    all.extend(sent);

    assert_eq!(
        fetches(&all),
        Vec::<String>::new(),
        "nothing is fetched: {all:?}"
    );
    let note = format!("Not on {REMOTE}: {gone}");
    assert_eq!(path_note(&mut h).as_deref(), Some(note.as_str()));
    assert!(drawn(&mut h, "phone-path-note"), "the line shows");

    clear_keyboard_calls();
    tap_cell(&mut h, 3, 1);
    assert_eq!(
        last_keyboard_call(),
        Some(KeyboardCall::Raise),
        "a tap off every link raises the keyboard as before"
    );

    tap_cell(&mut h, 3, 2);
    stat_answered(&mut h, NOTES, protocol::PathKind::File);
    notes_saved(&mut h, &dir);
    assert_eq!(path_note(&mut h), None, "a tap that opens clears the line");
}

#[gpui::test]
fn the_not_on_host_line_clears_when_the_session_page_closes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let gone = "/home/me/src/gone.txt";
    let mut h = terminal_showing(cx, &dir, &[gone]);
    tap_cell(&mut h, 3, 0);
    stat_answered(&mut h, gone, protocol::PathKind::Missing);
    assert!(path_note(&mut h).is_some(), "the line shows");

    h.click_on("phone-session-back");

    assert_eq!(path_note(&mut h), None, "closing the page clears the line");
}

#[gpui::test]
fn a_tapped_file_link_in_the_phone_chat_asks_the_host_about_its_path(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_paths_page(cx, &dir, "a");

    h.root_update(|root, window, cx| {
        root.phone_chat_file_link("a", std::path::Path::new(NOTES), window, cx);
    });

    let sent = h.host(&saved_remote()).sent();
    let asked: Vec<String> = stats(&sent).into_iter().map(|(_, path)| path).collect();
    assert_eq!(asked, [NOTES], "the link's path is asked about: {sent:?}");
}

/// Where the specs' repo `r1` lives on the host.
const REPO_ROOT: &str = "/home/me/r1";

/// Answers every stat the phone sends, starting with those in `sent`, as
/// missing, until it sends none; returns everything it sent meanwhile.
fn all_missing(h: &mut Harness<'_>, mut sent: Vec<ClientMessage>) -> Vec<ClientMessage> {
    let mut all = Vec::new();
    while let Some((id, _)) = stats(&sent).first().cloned() {
        answer_stat(h, &id, protocol::PathKind::Missing);
        all.extend(sent);
        sent = h.host(&saved_remote()).sent();
    }
    all.extend(sent);
    all
}

/// Opens session `id`'s page as a notification's tap does.
fn open_session_on(h: &mut Harness<'_>, id: &str) {
    let mut opened = false;
    phone_update(h, |phone, cx| opened = phone.open_session_on(id, cx));
    assert!(opened, "Home lists {id}");
}

#[gpui::test]
fn a_missing_path_still_resolving_names_itself_not_a_later_tap(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let gone = "docs/gone.md";
    let mut h = terminal_showing(cx, &dir, &[gone, "", NOTES]);

    tap_cell(&mut h, 2, 0);
    let first = h.host(&saved_remote()).sent();
    assert!(
        !stats(&first).is_empty(),
        "the first tap is asked about: {first:?}"
    );
    tap_cell(&mut h, 3, 2);
    stat_answered(&mut h, NOTES, protocol::PathKind::File);
    all_missing(&mut h, first);

    let note = path_note(&mut h);
    assert_eq!(note, Some(format!("Not on {REMOTE}: {gone}")));
    assert!(
        !note.is_some_and(|note| note.contains(NOTES)),
        "the line never names the later tap"
    );
}

#[gpui::test]
fn a_not_on_host_line_stays_on_its_own_session_page(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let gone = "/home/me/src/gone.txt";
    let mut h = terminal_showing(cx, &dir, &[gone]);
    tap_cell(&mut h, 3, 0);
    stat_answered(&mut h, gone, protocol::PathKind::Missing);
    assert!(path_note(&mut h).is_some(), "the line shows on n's page");

    open_session_on(&mut h, "a");

    assert_eq!(
        path_note(&mut h),
        None,
        "another session's page has no line"
    );
    assert!(!drawn(&mut h, "phone-path-note"));
}

#[gpui::test]
fn a_missing_answer_after_leaving_the_page_shows_no_line(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let gone = "/home/me/src/gone.txt";
    let mut h = terminal_showing(cx, &dir, &[gone]);
    tap_cell(&mut h, 3, 0);
    let pending = h.host(&saved_remote()).sent();
    h.click_on("phone-session-back");
    open_session_on(&mut h, "a");

    all_missing(&mut h, pending);

    assert_eq!(
        path_note(&mut h),
        None,
        "the next session's page has no line"
    );
    assert!(!drawn(&mut h, "phone-path-note"));
}

#[gpui::test]
fn a_tapped_folder_in_the_sessions_repo_opens_the_browser_and_fetches_nothing(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let mut h = terminal_showing(cx, &dir, &["src/"]);

    tap_cell(&mut h, 1, 0);
    let sent = h.host(&saved_remote()).sent();
    let asked = stats(&sent);
    assert!(!asked.is_empty(), "the folder is asked about: {sent:?}");
    answer_stat(&mut h, &asked[0].0, protocol::PathKind::Dir);
    let after = h.host(&saved_remote()).sent();

    assert_eq!(
        fetches(&after),
        Vec::<String>::new(),
        "nothing is fetched: {after:?}"
    );
    assert!(
        !after
            .iter()
            .any(|msg| wire_types(std::slice::from_ref(msg))[0].starts_with("fetch")),
        "no fetch of any kind: {after:?}"
    );
    let mut path = None;
    phone_update(&mut h, |phone, _| {
        path = phone.browser_path().map(str::to_owned);
    });
    assert_eq!(
        path,
        Some(format!("{REPO_ROOT}/src")),
        "the browser opens at the folder"
    );
}
