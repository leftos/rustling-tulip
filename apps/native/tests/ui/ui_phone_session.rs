//! The phone's session page: a header with Back, the session's label and
//! Home's status word, and at its right end a Terminal toggle shown only
//! for a running session whose chat is mirrored; the toggle flips between
//! the Chat and Terminal pages, and Back or a committed back gesture takes
//! one step back, from the Terminal page to the Chat page, else to Home.

use crate::support;

use daemon_client::device_tokens::DeviceTokenStore as _;
use gpui::{BackGesturePhase, Edges, Pixels, TestAppContext, WindowInsets, point, px};
use protocol::error_codes::CHAT_REFUSED;
use protocol::{
    ChatEntry, ChatScreen, ClientMessage, ComposerState, DaemonMessage, SessionSnapshot, ToolKind,
    ToolOutcome,
};
use rustling_tulip_native::chat::RowShape;
use rustling_tulip_native::phone::PhoneRoot;
use rustling_tulip_native::target::{HostKey, Target};
use rustling_tulip_native::{Connection, NetEvent};
use serde_json::json;
use support::{Harness, REMOTE, TestDir, repo, saved_remote, seed_ui, session};

/// The least height of a row or a target.
fn touch_target() -> Pixels {
    px(44.0)
}

/// The selector of the saved host's row for session `id`.
fn row(id: &str) -> String {
    let wire = HostKey::of(&saved_remote())
        .wire()
        .expect("a tailnet host has a wire key")
        .to_owned();
    format!("phone-home-row-{wire}::{id}")
}

/// Whether the last frame drew what `selector` tags.
fn drawn(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).size.width > px(0.0)
}

/// Reads the phone's root.
fn phone<R>(h: &mut Harness<'_>, read: impl FnOnce(&PhoneRoot, &gpui::App) -> R) -> R {
    h.cx.update(|window, cx| {
        let phone = window
            .root::<PhoneRoot>()
            .flatten()
            .expect("the window's root is the phone's");
        read(phone.read(cx), cx)
    })
}

/// The id of the session the phone shows in place of Home.
fn shown(h: &mut Harness<'_>) -> Option<String> {
    phone(h, |phone, _| {
        phone.shown_session().map(|(_, id)| id.to_owned())
    })
}

/// `a` working with a mirrored chat, `e` stopped, `n` with no agent and
/// so no chat, all in r1.
fn listed() -> Vec<SessionSnapshot> {
    vec![
        session("a")
            .label("Alpha")
            .in_repo("r1")
            .status("working")
            .build(),
        session("e")
            .label("Echo")
            .in_repo("r1")
            .status("stopped")
            .build(),
        session("n")
            .label("November")
            .in_repo("r1")
            .status("working")
            .no_agent()
            .build(),
    ]
}

/// A phone with the saved [`REMOTE`] open, listing [`listed`].
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
    h
}

/// A phone showing session `id`'s page.
fn opened<'a>(cx: &'a mut TestAppContext, dir: &TestDir, id: &str) -> Harness<'a> {
    let mut h = open_phone(cx, dir);
    h.click_on(&row(id));
    assert_eq!(shown(&mut h).as_deref(), Some(id));
    h
}

/// The request ids of the `LoadChat`s for session `id` among `sent`.
fn loads_of(sent: &[ClientMessage], id: &str) -> Vec<Option<String>> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::LoadChat {
                session_id,
                request_id,
            } if session_id == id => Some(request_id.clone()),
            _ => None,
        })
        .collect()
}

/// The daemon's answer to a load of session `id`'s chat: `entries`.
fn chat_reply(id: &str, request_id: Option<String>, entries: Vec<ChatEntry>) -> DaemonMessage {
    DaemonMessage::Chat {
        session_id: id.to_owned(),
        entries,
        hidden: 0,
        screen: ChatScreen {
            composer: ComposerState::Ready,
            prompt: None,
        },
        request_id,
    }
}

fn user(id: &str) -> ChatEntry {
    ChatEntry::User {
        id: id.to_owned(),
        text: format!("message {id}"),
        pending: false,
    }
}

fn answer(id: &str) -> ChatEntry {
    ChatEntry::Answer {
        id: id.to_owned(),
        text: format!("answer {id}"),
        live: false,
    }
}

/// A finished call `id` of tool `name`, of `kind`.
fn call(id: &str, name: &str, kind: ToolKind) -> ChatEntry {
    ChatEntry::Tool {
        id: id.to_owned(),
        name: name.to_owned(),
        summary: format!("{name} {id}"),
        input: "{}".to_owned(),
        result: Some(ToolOutcome {
            text: "out".to_owned(),
            is_error: false,
        }),
        parent_id: None,
        tail_paths: Vec::new(),
        kind,
    }
}

/// Session `a`'s page with its chat's load answered with `entries`.
fn chat_loaded<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    entries: Vec<ChatEntry>,
) -> Harness<'a> {
    let mut h = opened(cx, dir, "a");
    let sent = h.host(&saved_remote()).sent();
    let request = loads_of(&sent, "a")
        .pop()
        .expect("opening the page loads its chat");
    h.host(&saved_remote())
        .send(chat_reply("a", request, entries));
    h
}

/// The rows the phone draws for session `id`'s chat, as plain data.
fn phone_rows(h: &mut Harness<'_>, id: &str) -> Vec<RowShape> {
    h.root(|root, _| root.phone_chat_display_rows(id))
}

/// A back gesture in `phase` from the page's leading edge.
fn back_gesture(h: &mut Harness<'_>, phase: BackGesturePhase) {
    let page = h.bounds("phone-session");
    let at = point(page.origin.x + px(4.0), page.origin.y + px(200.0));
    h.back_gesture(phase, at);
}

#[gpui::test]
fn the_header_shows_back_label_and_status(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir, "a");

    assert!(drawn(&mut h, "phone-session-back"));
    assert!(h.bounds("phone-session-back").size.height >= touch_target());
    assert!(drawn(&mut h, "phone-session-label"));
    assert!(drawn(&mut h, "phone-session-status"));
    let (label, status) = phone(&mut h, |phone, cx| {
        (phone.session_label(cx), phone.session_status(cx))
    });
    assert_eq!(label.as_deref(), Some("Alpha"));
    assert_eq!(status, Some("Working"), "Home's word for the row");
}

#[gpui::test]
fn the_terminal_toggle_shows_for_a_live_session_with_a_mirrored_chat(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir, "a");

    assert!(drawn(&mut h, "phone-session-toggle"));
    assert!(h.bounds("phone-session-toggle").size.height >= touch_target());
    assert!(
        h.bounds("phone-session-toggle").origin.x > h.bounds("phone-session-label").origin.x,
        "the toggle sits right of the label"
    );
    let toggle = phone(&mut h, PhoneRoot::session_toggle);
    assert_eq!(toggle, Some("Terminal"));
    assert!(drawn(&mut h, "phone-session-chat"), "it opens on the chat");
}

#[gpui::test]
fn the_terminal_toggle_is_hidden_for_an_ended_session(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir, "e");

    assert!(!drawn(&mut h, "phone-session-toggle"));
    assert_eq!(phone(&mut h, PhoneRoot::session_toggle), None);
    assert_eq!(phone(&mut h, PhoneRoot::session_status), Some("Stopped"));
}

#[gpui::test]
fn the_terminal_toggle_is_hidden_without_a_mirrored_chat(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir, "n");

    assert!(!drawn(&mut h, "phone-session-toggle"));
    assert!(
        drawn(&mut h, "phone-session-terminal"),
        "the terminal is its main view"
    );
    assert!(!drawn(&mut h, "phone-session-chat"));
}

#[gpui::test]
fn the_toggle_switches_to_the_terminal_page_and_back(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir, "a");

    h.click_on("phone-session-toggle");

    assert!(drawn(&mut h, "phone-session-terminal"));
    assert!(!drawn(&mut h, "phone-session-chat"));
    assert_eq!(phone(&mut h, PhoneRoot::session_toggle), Some("Chat"));

    h.click_on("phone-session-toggle");

    assert!(drawn(&mut h, "phone-session-chat"));
    assert!(!drawn(&mut h, "phone-session-terminal"));
    assert_eq!(phone(&mut h, PhoneRoot::session_toggle), Some("Terminal"));
}

#[gpui::test]
fn the_back_button_returns_to_home(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir, "a");
    h.host(&saved_remote()).sent();

    h.click_on("phone-session-back");

    assert_eq!(shown(&mut h), None);
    assert!(drawn(&mut h, "phone-home"));
    assert!(!drawn(&mut h, "phone-session"));
    let sent = h.host(&saved_remote()).sent();
    assert!(
        matches!(
            sent.as_slice(),
            [ClientMessage::UnloadChat { session_id }] if session_id == "a"
        ),
        "Back sends only its chat's unload: {sent:?}"
    );
}

#[gpui::test]
fn opening_a_session_loads_its_chat(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir, "a");

    let sent = h.host(&saved_remote()).sent();
    let loads = loads_of(&sent, "a");
    assert_eq!(loads.len(), 1, "one load, to the session's host: {sent:?}");
    assert!(loads[0].is_some(), "under a request id");
}

#[gpui::test]
fn the_chat_rows_draw_on_the_phone(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded(
        cx,
        &dir,
        vec![user("u1"), answer("a1"), call("b1", "Bash", ToolKind::Run)],
    );

    assert!(drawn(&mut h, "phone-chat"));
    let chat = h.bounds("phone-chat");
    for row in [
        "chat-row-phone-u1",
        "chat-row-phone-a1",
        "chat-row-phone-b1",
    ] {
        let bounds = h.bounds(row);
        assert!(bounds.size.width > px(0.0), "{row} is drawn");
        assert!(
            chat.contains(&bounds.origin),
            "{row} sits in the phone's chat"
        );
    }
    assert_eq!(
        phone_rows(&mut h, "a"),
        [
            RowShape::Entry("u1".to_owned()),
            RowShape::Entry("a1".to_owned()),
            RowShape::Entry("b1".to_owned()),
        ]
    );
}

#[gpui::test]
fn closing_the_session_unloads_its_chat(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded(cx, &dir, vec![user("u1")]);

    h.click_on("phone-session-back");

    let sent = h.host(&saved_remote()).sent();
    let unloads = sent
        .iter()
        .filter(|msg| matches!(msg, ClientMessage::UnloadChat { session_id } if session_id == "a"))
        .count();
    assert_eq!(unloads, 1, "Back unloads the chat: {sent:?}");
}

#[gpui::test]
fn a_reconnect_loads_the_chat_again(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded(cx, &dir, vec![user("u1")]);
    let target: Target = saved_remote();
    let mut conn = Connection::for_target(target.clone());
    conn.on_connecting();
    conn.on_socket_open(8787);
    let _ = conn.on_closed("dropped".to_owned());
    h.host(&target).event(NetEvent::State(conn));

    let mut host = h.host(&target);
    host.connect();
    host.send(DaemonMessage::Sessions { sessions: listed() });

    let sent = h.host(&target).sent();
    assert_eq!(
        loads_of(&sent, "a").len(),
        1,
        "the new connection loads the chat: {sent:?}"
    );
}

/// Session `a`'s page with its chat's load refused as an agent offering no
/// chat refuses it.
fn chat_refused<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = opened(cx, dir, "a");
    let sent = h.host(&saved_remote()).sent();
    let request = loads_of(&sent, "a")
        .pop()
        .expect("opening the page loads its chat");
    h.host(&saved_remote()).send(DaemonMessage::Error {
        message: "chat is not offered for agent `other`".to_owned(),
        request_id: request,
        code: Some("unsupported_message".to_owned()),
    });
    h
}

#[gpui::test]
fn a_refused_phone_chat_load_is_not_asked_again(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_refused(cx, &dir);

    let sent = h.host(&saved_remote()).sent();
    assert!(
        loads_of(&sent, "a").is_empty(),
        "a refused load is not asked again: {sent:?}"
    );
    assert!(drawn(&mut h, "phone-chat"));
    assert!(
        drawn(&mut h, "phone-chat-note"),
        "the phone's chat shows the refusal's note"
    );
    assert_eq!(
        h.root(|root, _| root.phone_chat_note().map(str::to_owned))
            .as_deref(),
        Some("This session offers no chat, so it shows its terminal")
    );
}

#[gpui::test]
fn a_reconnect_asks_again_after_a_refusal(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_refused(cx, &dir);
    h.host(&saved_remote()).sent();
    let target: Target = saved_remote();
    let mut conn = Connection::for_target(target.clone());
    conn.on_connecting();
    conn.on_socket_open(8787);
    let _ = conn.on_closed("dropped".to_owned());
    h.host(&target).event(NetEvent::State(conn));

    let mut host = h.host(&target);
    host.connect();
    host.send(DaemonMessage::Sessions { sessions: listed() });

    let sent = h.host(&target).sent();
    assert_eq!(
        loads_of(&sent, "a").len(),
        1,
        "the new connection asks once more: {sent:?}"
    );
}

#[gpui::test]
fn three_calls_in_a_row_draw_as_three_rows_and_four_fold(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded(
        cx,
        &dir,
        vec![
            call("r1", "Read", ToolKind::Read),
            call("r2", "Read", ToolKind::Read),
            call("r3", "Read", ToolKind::Read),
        ],
    );
    assert_eq!(
        phone_rows(&mut h, "a"),
        [
            RowShape::Entry("r1".to_owned()),
            RowShape::Entry("r2".to_owned()),
            RowShape::Entry("r3".to_owned()),
        ]
    );
    assert!(drawn(&mut h, "chat-row-phone-r2"));

    h.host(&saved_remote()).send(DaemonMessage::ChatAppend {
        session_id: "a".to_owned(),
        entries: vec![call("g1", "Grep", ToolKind::Search)],
        removed: Vec::new(),
    });

    assert_eq!(
        phone_rows(&mut h, "a"),
        [RowShape::Run {
            ids: ["r1", "r2", "r3", "g1"].map(str::to_owned).to_vec(),
            line: "3 read \u{b7} 1 searched".to_owned(),
            running: false,
        }]
    );
    assert!(drawn(&mut h, "chat-run-phone-r1"));
}

#[gpui::test]
fn a_chat_message_for_another_session_changes_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded(cx, &dir, vec![user("u1")]);
    let before = phone_rows(&mut h, "a");

    h.host(&saved_remote())
        .send(chat_reply("e", None, vec![user("x1"), answer("x2")]));

    assert_eq!(phone_rows(&mut h, "a"), before);
    assert!(phone_rows(&mut h, "e").is_empty(), "nothing loaded for e");
    assert!(!drawn(&mut h, "chat-row-phone-x1"));
    assert!(
        h.host(&saved_remote()).sent().is_empty(),
        "and nothing is sent"
    );
}

#[gpui::test]
fn back_from_the_terminal_page_returns_to_the_chat_page(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir, "a");
    h.click_on("phone-session-toggle");

    h.click_on("phone-session-back");

    assert_eq!(shown(&mut h).as_deref(), Some("a"), "the page stays");
    assert!(drawn(&mut h, "phone-session-chat"));

    h.click_on("phone-session-back");

    assert_eq!(shown(&mut h), None);
    assert!(drawn(&mut h, "phone-home"));
}

#[gpui::test]
fn a_committed_back_gesture_returns_to_home(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir, "a");

    back_gesture(&mut h, BackGesturePhase::Started);
    back_gesture(&mut h, BackGesturePhase::Progressed);
    assert_eq!(shown(&mut h).as_deref(), Some("a"), "not before it commits");
    back_gesture(&mut h, BackGesturePhase::Committed);

    assert_eq!(shown(&mut h), None);
    assert!(drawn(&mut h, "phone-home"));
}

#[gpui::test]
fn a_back_gesture_starting_in_the_top_inset_returns_to_home(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir, "a");
    // The harness's window reports no insets: draw a phone's 62 pt top one.
    let insets = WindowInsets {
        safe_area: Edges {
            top: px(62.0),
            bottom: px(34.0),
            ..Edges::default()
        },
        ..WindowInsets::default()
    };
    h.cx.update(|window, cx| {
        let phone = window
            .root::<PhoneRoot>()
            .flatten()
            .expect("the window's root is the phone's");
        phone.update(cx, |phone, cx| phone.set_insets_override(Some(insets), cx));
    });
    h.cx.run_until_parked();
    assert!(
        h.bounds("phone-session").origin.y >= px(62.0),
        "the page sits under the inset"
    );
    let at = point(px(4.0), px(20.0));

    h.back_gesture(BackGesturePhase::Started, at);
    h.back_gesture(BackGesturePhase::Committed, at);

    assert_eq!(shown(&mut h), None);
    assert!(drawn(&mut h, "phone-home"));
}

#[gpui::test]
fn the_page_shows_the_terminal_when_the_chat_stops_being_mirrored(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir, "a");
    assert!(drawn(&mut h, "phone-session-chat"));

    // The agent exited to the shell it ran in: the session runs on, its
    // chat is no longer mirrored.
    let mut sessions = listed();
    sessions[0] = session("a")
        .label("Alpha")
        .in_repo("r1")
        .status("working")
        .program_name("pwsh")
        .build();
    h.host(&saved_remote())
        .send(DaemonMessage::Sessions { sessions });
    h.cx.run_until_parked();

    assert_eq!(shown(&mut h).as_deref(), Some("a"), "the page stays");
    assert!(drawn(&mut h, "phone-session-terminal"));
    assert!(!drawn(&mut h, "phone-session-chat"));
    assert!(!drawn(&mut h, "phone-session-toggle"));

    h.host(&saved_remote())
        .send(DaemonMessage::Sessions { sessions: listed() });
    h.cx.run_until_parked();

    assert!(
        drawn(&mut h, "phone-session-chat"),
        "the chosen chat returns"
    );
    assert!(drawn(&mut h, "phone-session-toggle"));
}

#[gpui::test]
fn a_cancelled_back_gesture_stays(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir, "a");

    back_gesture(&mut h, BackGesturePhase::Started);
    back_gesture(&mut h, BackGesturePhase::Progressed);
    back_gesture(&mut h, BackGesturePhase::Cancelled);

    assert_eq!(shown(&mut h).as_deref(), Some("a"));
    assert!(drawn(&mut h, "phone-session"));
    assert!(drawn(&mut h, "phone-session-chat"));
}

#[gpui::test]
fn the_page_stays_while_its_host_is_disconnected(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir, "a");
    let target: Target = saved_remote();
    let mut conn = Connection::for_target(target.clone());
    conn.on_connecting();
    conn.on_socket_open(8787);
    let _ = conn.on_closed("dropped".to_owned());

    h.host(&target).event(NetEvent::State(conn));
    h.cx.run_until_parked();

    assert_eq!(shown(&mut h).as_deref(), Some("a"), "the page stays");
    assert!(drawn(&mut h, "phone-session-label"));
    assert!(drawn(&mut h, "phone-session-status"));
}

#[gpui::test]
fn the_page_closes_when_its_session_is_gone(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir, "a");

    let rest: Vec<SessionSnapshot> = listed().into_iter().skip(1).collect();
    h.host(&saved_remote())
        .send(DaemonMessage::Sessions { sessions: rest });
    h.cx.run_until_parked();

    assert_eq!(shown(&mut h), None);
    assert!(drawn(&mut h, "phone-home"));
    assert!(!drawn(&mut h, "phone-session"));
}

// ── The composer: Send, Paste, Quick and Keys ──────────────────────────────

/// Session `a`'s page with its chat loaded, `text` typed into its composer.
fn typed_composer<'a>(cx: &'a mut TestAppContext, dir: &TestDir, text: &str) -> Harness<'a> {
    let mut h = chat_loaded(cx, dir, vec![user("u1")]);
    h.click_on("phone-composer-field");
    let keys: Vec<String> = text
        .chars()
        .map(|c| {
            if c == ' ' {
                "space".to_owned()
            } else {
                c.to_string()
            }
        })
        .collect();
    h.keys(&keys.join(" "));
    h
}

/// The text in session `a`'s composer.
fn composer_text(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, cx| root.chat_composer_text("a", cx))
}

/// The note session `a`'s composer shows, while it shows one.
fn composer_note(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.chat_composer_note("a"))
}

/// Whether session `a`'s composer may send now.
fn send_enabled(h: &mut Harness<'_>) -> bool {
    h.root(|root, cx| root.phone_composer_send_enabled("a", cx))
}

/// The `ChatSend`s to session `a` since the last drain, as text and
/// request id.
fn chat_sends(h: &mut Harness<'_>) -> Vec<(String, Option<String>)> {
    h.host(&saved_remote())
        .sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::ChatSend {
                session_id,
                text,
                request_id,
            } if session_id == "a" => Some((text, request_id)),
            _ => None,
        })
        .collect()
}

#[gpui::test]
fn send_sends_the_text_and_clears_the_box(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = typed_composer(cx, &dir, "hello there");
    assert!(drawn(&mut h, "phone-composer"));
    assert_eq!(composer_text(&mut h).as_deref(), Some("hello there"));
    assert!(send_enabled(&mut h), "a full box can send");
    h.host(&saved_remote()).sent();

    h.click_on("phone-composer-send");

    let sends = chat_sends(&mut h);
    assert_eq!(sends.len(), 1, "one send: {sends:?}");
    assert_eq!(sends[0].0, "hello there");
    assert_eq!(
        composer_text(&mut h).as_deref(),
        Some(""),
        "the send clears it"
    );
}

#[gpui::test]
fn return_makes_a_line_break_and_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = typed_composer(cx, &dir, "hi");
    h.host(&saved_remote()).sent();

    h.keys("enter");

    assert_eq!(composer_text(&mut h).as_deref(), Some("hi\n"));
    let sends = chat_sends(&mut h);
    assert!(sends.is_empty(), "Return sends nothing: {sends:?}");
}

#[gpui::test]
fn send_is_off_for_a_blank_box(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded(cx, &dir, vec![user("u1")]);
    h.host(&saved_remote()).sent();

    assert!(!send_enabled(&mut h), "an empty box cannot send");
    h.click_on("phone-composer-send");

    assert!(
        chat_sends(&mut h).is_empty(),
        "and a tap on Send sends nothing"
    );
}

#[gpui::test]
fn send_is_off_while_the_host_is_disconnected(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = typed_composer(cx, &dir, "hello");
    let target: Target = saved_remote();
    let mut conn = Connection::for_target(target.clone());
    conn.on_connecting();
    conn.on_socket_open(8787);
    let _ = conn.on_closed("dropped".to_owned());
    h.host(&target).event(NetEvent::State(conn));
    h.cx.run_until_parked();

    assert!(!send_enabled(&mut h), "no connection, no send");
    assert_eq!(composer_note(&mut h).as_deref(), Some("Not connected"));
    assert!(drawn(&mut h, "phone-composer-note"), "the note says why");
}

#[gpui::test]
fn paste_inserts_the_clipboard_text(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded(cx, &dir, vec![user("u1")]);
    h.set_clipboard("pasted text");

    h.click_on("phone-composer-paste");

    assert_eq!(composer_text(&mut h).as_deref(), Some("pasted text"));
}

#[gpui::test]
fn quick_types_its_reply_with_return(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded(cx, &dir, vec![user("u1")]);
    h.host(&saved_remote()).sent();

    h.click_on("phone-composer-quick");
    assert!(drawn(&mut h, "phone-dock-quick"), "the dock opens");
    h.click_on("phone-quick-common-0");

    assert_eq!(
        h.host(&saved_remote()).sent_input("a"),
        b"continue\r",
        "a quick reply types its text and Return"
    );
    assert!(
        drawn(&mut h, "phone-dock-quick"),
        "the dock stays open after a tap"
    );
}

#[gpui::test]
fn a_shell_session_offers_only_y_and_n(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir, "n");

    assert_eq!(phone(&mut h, PhoneRoot::quick_reply_items), ["y", "n"]);
}

#[gpui::test]
fn keys_types_each_key(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded(cx, &dir, vec![user("u1")]);
    h.host(&saved_remote()).sent();
    h.click_on("phone-composer-keys");
    assert!(drawn(&mut h, "phone-dock-keys"), "the tray opens");

    h.click_on("phone-key-0");
    assert_eq!(h.host(&saved_remote()).sent_input("a"), b"\x1b", "Esc");

    h.click_on("phone-key-6");
    assert_eq!(h.host(&saved_remote()).sent_input("a"), b"\x03", "Ctrl+C");
}

#[gpui::test]
fn only_one_dock_shows_at_a_time(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded(cx, &dir, vec![user("u1")]);

    h.click_on("phone-composer-quick");
    assert!(drawn(&mut h, "phone-dock-quick"));
    assert!(!drawn(&mut h, "phone-dock-keys"));

    h.click_on("phone-composer-keys");
    assert!(!drawn(&mut h, "phone-dock-quick"));
    assert!(drawn(&mut h, "phone-dock-keys"));
}

#[gpui::test]
fn a_refused_send_gives_its_text_back(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = typed_composer(cx, &dir, "hello there");
    h.host(&saved_remote()).sent();
    h.click_on("phone-composer-send");
    let sent = chat_sends(&mut h);
    assert_eq!(sent.len(), 1, "one send to refuse: {sent:?}");

    h.host(&saved_remote()).send(DaemonMessage::Error {
        message: "the prompt box holds a draft".to_owned(),
        request_id: sent[0].1.clone(),
        code: Some(CHAT_REFUSED.to_owned()),
    });

    assert_eq!(
        composer_text(&mut h).as_deref(),
        Some("hello there"),
        "the refusal gives the text back"
    );
    assert_eq!(
        h.root(|root, _| root.chat_composer_error("a")).as_deref(),
        Some("the prompt box holds a draft"),
        "and the daemon's reason is kept"
    );
    assert!(
        drawn(&mut h, "phone-composer-error"),
        "the reason shows under the field"
    );
}

#[gpui::test]
fn back_closes_an_open_dock_and_stays(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded(cx, &dir, vec![user("u1")]);
    h.click_on("phone-composer-keys");
    assert!(drawn(&mut h, "phone-dock-keys"));

    h.click_on("phone-session-back");

    assert!(!drawn(&mut h, "phone-dock-keys"), "the dock closes first");
    assert_eq!(shown(&mut h).as_deref(), Some("a"), "and the page stays");
    assert!(drawn(&mut h, "phone-session-chat"));
}

#[gpui::test]
fn back_from_the_terminal_page_after_opening_keys_returns_to_the_chat_page(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let mut h = chat_loaded(cx, &dir, vec![user("u1")]);
    h.click_on("phone-composer-keys");
    assert!(drawn(&mut h, "phone-dock-keys"));

    h.click_on("phone-session-toggle");
    assert!(
        drawn(&mut h, "phone-session-terminal"),
        "the toggle leaves the chat page"
    );

    h.click_on("phone-session-back");

    assert!(
        drawn(&mut h, "phone-session-chat"),
        "one Back steps off the terminal page, not onto a dock nobody sees"
    );
    assert_eq!(shown(&mut h).as_deref(), Some("a"), "the page stays");
}

#[gpui::test]
fn the_composer_buttons_are_44_px_tall(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded(cx, &dir, vec![user("u1")]);

    for selector in [
        "phone-composer-send",
        "phone-composer-paste",
        "phone-composer-quick",
        "phone-composer-keys",
    ] {
        let bounds = h.bounds(selector);
        assert!(
            bounds.size.height >= touch_target(),
            "{selector} is {} tall",
            bounds.size.height
        );
    }
}
