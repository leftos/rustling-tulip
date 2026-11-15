//! The phone's terminal slot: one session's live terminal held by the root
//! under the session's pop-out slot id, attached with a scrollback load,
//! claiming the PTY size, drawn at the phone's own font size, and detached
//! when it closes, with no tab, layout or pop-out window.

use crate::support;

use daemon_client::device_tokens::DeviceTokenStore as _;
use gpui::{Edges, Pixels, TestAppContext, WindowInsets, WindowOptions, px};
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
fn a_rising_keyboard_claims_the_size_again(cx: &mut TestAppContext) {
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
        Some(true),
        "the shorter terminal claims the size: {sent:?}"
    );
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
    }
}

#[gpui::test]
fn the_prompt_sheet_draws_over_the_terminal_page(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir);
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

    assert!(drawn(&mut h, "phone-prompt-sheet"));
    assert!(drawn(&mut h, "phone-session-terminal"), "over the terminal");
    assert!(h.root(|root, _| root.phone_terminal_view()).is_some());
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
