//! The phone's session page: a header with Back, the session's label and
//! Home's status word, and at its right end a Terminal toggle shown only
//! for a running session whose chat is mirrored; the toggle flips between
//! the Chat and Terminal pages, and Back or a committed back gesture takes
//! one step back, from the Terminal page to the Chat page, else to Home.

use crate::support;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use daemon_client::device_tokens::DeviceTokenStore as _;
use gpui::{
    BackGesturePhase, Edges, ImageFormat, Modifiers, Pixels, Point, ScrollDelta, ScrollEvent,
    ScrollSource, TestAppContext, TouchPhase, WindowInsets, point, px,
};
use protocol::error_codes::CHAT_REFUSED;
use protocol::{
    CatalogEntry, CatalogEntryKind, CatalogEntrySource, ChatAnswerKind, ChatEntry, ChatOption,
    ChatPrompt, ChatPromptKind, ChatScreen, ClientMessage, ComposerState, DaemonMessage,
    FEATURE_CHAT_CATALOG, FEATURE_CHAT_CLEAR_DRAFT, FEATURE_CHAT_DRAFT, FEATURE_FILE_SEARCH,
    FEATURE_FILE_TAIL, FEATURE_UPLOAD_FILE, FileMatch, ScriptFacts, SessionSnapshot, ToolKind,
    ToolOutcome, WorkShell,
};
use rustling_tulip_native::chat::{self, RowShape};
use rustling_tulip_native::keyboard::{KeyboardCall, clear_keyboard_calls, keyboard_calls};
use rustling_tulip_native::phone::PhoneRoot;
use rustling_tulip_native::phone::pasteboard::FakePasteboard;
use rustling_tulip_native::phone::photos::FakePhotoPicker;
use rustling_tulip_native::phone::quick_replies::{ReplyGroup, ReplySet};
use rustling_tulip_native::target::{HostKey, Target};
use rustling_tulip_native::{Connection, NetEvent};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use support::{
    Fixture, Harness, REMOTE, TestDir, facts, facts_message, repo, saved_remote, saved_ui, seed_ui,
    session,
};

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
    open_phone_on(cx, dir, &[])
}

/// [`open_phone`] with [`REMOTE`]'s daemon listing `features` beside the
/// current daemon's.
fn open_phone_on<'a>(cx: &'a mut TestAppContext, dir: &TestDir, features: &[&str]) -> Harness<'a> {
    open_phone_saved(cx, dir, &json!({}), features)
}

/// [`open_phone`] with `ui`'s fields saved beside the host.
fn open_phone_saved<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    ui: &serde_json::Value,
    features: &[&str],
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
    host.connect_with_features(features);
    host.sent();
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
        script: None,
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
fn a_long_title_keeps_back_on_one_line(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir, "a");
    let single = h.bounds("phone-session-back");
    assert!(
        single.size.height >= touch_target(),
        "Back is a 44 pt target"
    );

    // A title far wider than the header: the label gives way, Back does not.
    let title = "Release plan check across every saved host, ".repeat(3);
    let mut sessions = listed();
    sessions[0] = session("a")
        .label(&title)
        .in_repo("r1")
        .status("working")
        .build();
    h.host(&saved_remote())
        .send(DaemonMessage::Sessions { sessions });
    h.cx.run_until_parked();

    assert_eq!(shown(&mut h).as_deref(), Some("a"), "the page stays");
    assert_eq!(
        h.bounds("phone-session-back"),
        single,
        "Back keeps its one line beside the long title"
    );
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
            [
                ClientMessage::UnloadChat { session_id },
                ClientMessage::WatchDashboard { on: false },
            ] if session_id == "a"
        ),
        "Back sends only its chat's unload and the end of the watch its context label read: {sent:?}"
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

// The prompt sheet: the chat's pending prompt card rises as a bottom sheet
// over the page, with Esc, up and down keys; a swipe down or a back step
// puts it away, `Show prompt` raises it again, and a different prompt rises
// on its own.

/// The selectors of the sheet's three keys.
const SHEET_KEYS: [&str; 3] = ["phone-prompt-esc", "phone-prompt-up", "phone-prompt-down"];

/// Prompt `id` of `kind` offering `list`.
fn sheet_prompt(id: &str, kind: ChatPromptKind, list: &[(u32, &str)]) -> ChatPrompt {
    ChatPrompt {
        id: id.to_owned(),
        kind,
        subject: "calc.py".to_owned(),
        body: "Do you want to make this edit?".to_owned(),
        options: list
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

/// A single-select question `id` with two numbered options.
fn question_prompt(id: &str) -> ChatPrompt {
    sheet_prompt(id, ChatPromptKind::Question, &[(1, "sub"), (2, "div")])
}

/// A permission prompt `id`: allow once, always allow and deny.
fn permission_prompt(id: &str) -> ChatPrompt {
    sheet_prompt(
        id,
        ChatPromptKind::Permission,
        &[
            (1, "Yes"),
            (2, "Yes, and don't ask again for: python *"),
            (3, "No"),
        ],
    )
}

/// Session `a`'s screen now shows `prompt`, or none.
fn screen_shows(h: &mut Harness<'_>, prompt: Option<ChatPrompt>) {
    h.host(&saved_remote())
        .send(DaemonMessage::ChatScreenChanged {
            session_id: "a".to_owned(),
            screen: ChatScreen {
                composer: ComposerState::Dialog,
                prompt,
            },
        });
    h.cx.run_until_parked();
}

/// Session `a`'s page with its chat loaded and `prompt` pending, everything
/// sent so far drained.
fn prompted<'a>(cx: &'a mut TestAppContext, dir: &TestDir, prompt: ChatPrompt) -> Harness<'a> {
    prompted_over(cx, dir, vec![user("u1")], prompt)
}

/// Session `a`'s page with its chat loaded with `entries` and `prompt`
/// pending, everything sent so far drained.
fn prompted_over<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    entries: Vec<ChatEntry>,
    prompt: ChatPrompt,
) -> Harness<'a> {
    let mut h = chat_loaded(cx, dir, entries);
    screen_shows(&mut h, Some(prompt));
    h.host(&saved_remote()).sent();
    h
}

/// The `ChatAnswer`s sent for session `a` since the last drain: prompt id,
/// answer and request id.
fn sheet_answers(h: &mut Harness<'_>) -> Vec<(String, ChatAnswerKind, Option<String>)> {
    h.host(&saved_remote())
        .sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::ChatAnswer {
                session_id,
                prompt_id,
                answer,
                request_id,
            } if session_id == "a" => Some((prompt_id, answer, request_id)),
            _ => None,
        })
        .collect()
}

/// A downward pan over the sheet: `steps` movements of `dy`, then the lift.
fn drag_sheet(h: &mut Harness<'_>, dy: Pixels, steps: usize) {
    let at = h.bounds("phone-prompt-handle").center();
    let pan = |delta: Pixels, touch_phase: TouchPhase| ScrollEvent {
        position: at,
        delta: ScrollDelta::Pixels(point(px(0.0), delta)),
        modifiers: Modifiers::none(),
        touch_phase,
        source: ScrollSource::Touch,
    };
    h.cx.simulate_event(pan(px(0.0), TouchPhase::Started));
    for _ in 0..steps {
        h.cx.simulate_event(pan(dy, TouchPhase::Moved));
    }
    h.cx.simulate_event(pan(px(0.0), TouchPhase::Ended));
    h.cx.run_until_parked();
}

/// Whether `selector` is drawn inside the sheet.
fn in_sheet(h: &mut Harness<'_>, selector: &str) -> bool {
    let sheet = h.bounds("phone-prompt-sheet");
    drawn(h, selector) && sheet.contains(&h.bounds(selector).center())
}

#[gpui::test]
fn a_pending_prompt_raises_the_sheet_over_the_chat(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir, "a");
    screen_shows(&mut h, Some(question_prompt("q1")));
    assert!(
        !drawn(&mut h, "phone-prompt-sheet"),
        "no sheet before the chat loads"
    );
    let request = loads_of(&h.host(&saved_remote()).sent(), "a")
        .pop()
        .expect("opening the page loads its chat");
    h.host(&saved_remote())
        .send(chat_reply("a", request, vec![user("u1")]));
    assert!(
        !drawn(&mut h, "phone-prompt-sheet"),
        "no sheet without a card"
    );

    screen_shows(&mut h, Some(question_prompt("q1")));

    assert!(drawn(&mut h, "phone-prompt-scrim"));
    assert!(drawn(&mut h, "phone-prompt-sheet"));
    assert!(drawn(&mut h, "phone-session-chat"), "over the chat");
    assert!(in_sheet(&mut h, "phone-prompt-handle"));
    assert!(in_sheet(&mut h, "chat-card-1"), "the card's options");
    assert!(in_sheet(&mut h, "chat-card-2"));
    let (sheet, scrim) = (
        h.bounds("phone-prompt-sheet"),
        h.bounds("phone-prompt-scrim"),
    );
    assert!(
        sheet.bottom() + px(1.0) >= scrim.bottom(),
        "anchored to the bottom: {sheet:?} in {scrim:?}"
    );
    assert!(
        h.bounds("phone-prompt-handle").origin.y < h.bounds("chat-card-1").origin.y,
        "the handle tops the sheet"
    );
    assert!(!drawn(&mut h, "phone-prompt-show"));

    screen_shows(&mut h, None);

    assert!(
        !drawn(&mut h, "phone-prompt-sheet"),
        "an answered prompt takes the sheet down"
    );
}

#[gpui::test]
fn an_option_button_sends_a_chat_answer_with_its_number(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = prompted(cx, &dir, question_prompt("q1"));

    h.click_on("chat-card-2");

    let answers = sheet_answers(&mut h);
    assert_eq!(answers.len(), 1, "one answer: {answers:?}");
    assert_eq!(answers[0].0, "q1");
    assert_eq!(answers[0].1, ChatAnswerKind::Option { number: 2 });
    assert!(answers[0].2.is_some(), "under a request id");
}

#[gpui::test]
fn a_permission_prompt_offers_its_card_buttons(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = prompted(cx, &dir, permission_prompt("p1"));

    for selector in [
        "chat-card-1",
        "chat-card-2",
        "chat-card-3",
        "chat-card-box",
        "chat-card-allow-text",
        "chat-card-deny-text",
    ] {
        assert!(in_sheet(&mut h, selector), "{selector} is in the sheet");
    }

    h.click_on("chat-card-3");

    let answers = sheet_answers(&mut h);
    assert_eq!(
        answers
            .into_iter()
            .map(|(_, answer, _)| answer)
            .collect::<Vec<_>>(),
        [ChatAnswerKind::Option { number: 3 }],
        "Deny types its number"
    );
}

#[gpui::test]
fn esc_up_and_down_type_their_keys(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = prompted(cx, &dir, question_prompt("q1"));

    for key in SHEET_KEYS {
        assert!(in_sheet(&mut h, key), "{key} is in the sheet");
        h.click_on(key);
    }

    let sent = h.host(&saved_remote()).sent();
    let typed: Vec<Vec<u8>> = sent
        .iter()
        .filter_map(|msg| match msg {
            ClientMessage::SendInput {
                session_id,
                data_b64,
                automatic: false,
            } if session_id == "a" => Some(B64.decode(data_b64).expect("base64 input")),
            _ => None,
        })
        .collect();
    assert_eq!(
        typed,
        [b"\x1b".to_vec(), b"\x1b[A".to_vec(), b"\x1b[B".to_vec()],
        "{sent:?}"
    );
    assert!(
        !sent
            .iter()
            .any(|msg| matches!(msg, ClientMessage::ChatAnswer { .. })),
        "the keys answer nothing"
    );
    assert!(drawn(&mut h, "phone-prompt-sheet"), "the sheet stays up");
}

#[gpui::test]
fn a_swipe_puts_the_sheet_away_and_show_prompt_raises_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = prompted(cx, &dir, question_prompt("q1"));

    drag_sheet(&mut h, px(4.0), 2);
    assert!(
        drawn(&mut h, "phone-prompt-sheet"),
        "a short slow drag snaps back"
    );

    drag_sheet(&mut h, px(60.0), 5);

    assert!(!drawn(&mut h, "phone-prompt-sheet"));
    assert!(!drawn(&mut h, "phone-prompt-scrim"));
    assert!(drawn(&mut h, "phone-prompt-show"));
    let (show, chat, composer) = (
        h.bounds("phone-prompt-show"),
        h.bounds("phone-chat"),
        h.bounds("phone-composer"),
    );
    assert!(drawn(&mut h, "phone-composer"));
    assert!(
        show.origin.y + px(1.0) >= chat.bottom() && show.bottom() <= composer.origin.y + px(1.0),
        "Show prompt sits between the chat and the composer: {show:?}, chat {chat:?}, composer {composer:?}"
    );
    assert!(show.size.height >= touch_target());
    assert!(
        h.host(&saved_remote()).sent().is_empty(),
        "a swipe sends nothing"
    );

    h.click_on("phone-prompt-show");

    assert!(drawn(&mut h, "phone-prompt-sheet"));
    assert!(!drawn(&mut h, "phone-prompt-show"));
}

#[gpui::test]
fn a_different_prompt_raises_it_again(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = prompted(cx, &dir, question_prompt("q1"));
    drag_sheet(&mut h, px(60.0), 5);
    assert!(!drawn(&mut h, "phone-prompt-sheet"));

    screen_shows(&mut h, Some(question_prompt("q1")));
    assert!(
        !drawn(&mut h, "phone-prompt-sheet"),
        "the same prompt stays down"
    );

    screen_shows(&mut h, Some(permission_prompt("p2")));

    assert!(drawn(&mut h, "phone-prompt-sheet"));
    assert!(
        in_sheet(&mut h, "chat-card-deny-text"),
        "the new prompt's card"
    );
    assert!(!drawn(&mut h, "phone-prompt-show"));
}

#[gpui::test]
fn back_with_the_sheet_up_dismisses_it_and_stays_on_the_page(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = prompted(cx, &dir, question_prompt("q1"));

    back_gesture(&mut h, BackGesturePhase::Started);
    back_gesture(&mut h, BackGesturePhase::Committed);

    assert_eq!(shown(&mut h).as_deref(), Some("a"), "the page stays");
    assert!(!drawn(&mut h, "phone-prompt-sheet"));
    assert!(drawn(&mut h, "phone-prompt-show"));
    assert!(drawn(&mut h, "phone-session-chat"));
    assert!(
        h.host(&saved_remote()).sent().is_empty(),
        "the chat stays loaded"
    );

    back_gesture(&mut h, BackGesturePhase::Started);
    back_gesture(&mut h, BackGesturePhase::Committed);

    assert_eq!(shown(&mut h), None, "the next back closes the page");
    assert!(drawn(&mut h, "phone-home"));
}

#[gpui::test]
fn the_sheet_shows_on_the_terminal_page_too(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded(cx, &dir, vec![user("u1")]);
    h.click_on("phone-session-toggle");
    assert!(drawn(&mut h, "phone-session-terminal"));

    screen_shows(&mut h, Some(question_prompt("q1")));

    assert!(drawn(&mut h, "phone-prompt-sheet"));
    assert!(in_sheet(&mut h, "chat-card-1"));
    assert!(drawn(&mut h, "phone-session-terminal"), "over the terminal");
}

#[gpui::test]
fn a_refused_answer_shows_its_reason(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = prompted(cx, &dir, permission_prompt("p1"));
    h.click_on("chat-card-1");
    let request_id = sheet_answers(&mut h).pop().expect("the press answers").2;

    h.host(&saved_remote()).send(DaemonMessage::Error {
        message: "the terminal did not respond".to_owned(),
        request_id,
        code: Some(CHAT_REFUSED.to_owned()),
    });
    h.cx.run_until_parked();

    assert_eq!(
        h.root(|root, _| root.chat_card_refusal("a")).as_deref(),
        Some("the terminal did not respond")
    );
    assert!(in_sheet(&mut h, "chat-card-refusal"));
    assert!(drawn(&mut h, "phone-prompt-sheet"), "the sheet stays up");
}

#[gpui::test]
fn the_sheet_keys_are_44_px_tall(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = prompted(cx, &dir, question_prompt("q1"));

    for key in SHEET_KEYS {
        assert!(
            h.bounds(key).size.height >= touch_target(),
            "{key} is a touch target tall"
        );
    }
}

/// One touch pan event at `at`, `dy` down, in `phase`; gpui sends every
/// event of a pan, its momentum included, at the point the pan began.
fn pan_at(h: &mut Harness<'_>, at: gpui::Point<Pixels>, dy: Pixels, phase: TouchPhase) {
    h.cx.simulate_event(ScrollEvent {
        position: at,
        delta: ScrollDelta::Pixels(point(px(0.0), dy)),
        modifiers: Modifiers::none(),
        touch_phase: phase,
        source: ScrollSource::Touch,
    });
    h.cx.run_until_parked();
}

/// How far the phone's chat is scrolled from its top.
fn chat_offset(h: &mut Harness<'_>) -> Option<f32> {
    h.root(|root, _| root.chat_scroll_offset("phone"))
}

#[gpui::test]
fn a_flick_that_dismisses_the_sheet_does_not_scroll_the_chat(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let entries: Vec<ChatEntry> = (0..40).map(|n| user(&format!("u{n}"))).collect();
    let mut h = prompted_over(cx, &dir, entries, question_prompt("q1"));
    let start = chat_offset(&mut h);
    assert!(
        start.is_some_and(|offset| offset > 0.0),
        "the long chat sits at its bottom: {start:?}"
    );
    let at = h.bounds("phone-prompt-handle").center();

    pan_at(&mut h, at, px(0.0), TouchPhase::Started);
    for _ in 0..5 {
        pan_at(&mut h, at, px(60.0), TouchPhase::Moved);
    }
    pan_at(&mut h, at, px(0.0), TouchPhase::Ended);
    assert!(!drawn(&mut h, "phone-prompt-sheet"), "the flick commits");
    // `Show prompt` now sits under the chat, which keeps its bottom in view.
    let before = chat_offset(&mut h);
    // The flick's momentum, at the point the pan began.
    for _ in 0..4 {
        pan_at(&mut h, at, px(40.0), TouchPhase::Moved);
    }
    pan_at(&mut h, at, px(10.0), TouchPhase::Ended);

    assert_eq!(chat_offset(&mut h), before, "the chat did not scroll");
    let (last, chat) = (h.bounds("chat-row-phone-u39"), h.bounds("phone-chat"));
    assert!(
        last.bottom() <= chat.bottom() + px(1.0),
        "the newest row stays in view: {last:?} in {chat:?}"
    );

    pan_at(&mut h, at, px(0.0), TouchPhase::Started);
    pan_at(&mut h, at, px(100.0), TouchPhase::Moved);
    pan_at(&mut h, at, px(0.0), TouchPhase::Ended);

    assert_ne!(
        chat_offset(&mut h),
        before,
        "the next pan is never swallowed"
    );
}

#[gpui::test]
fn back_while_the_chat_loads_does_not_dismiss_a_prompt(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir, "a");
    h.click_on("phone-session-toggle");
    let request = loads_of(&h.host(&saved_remote()).sent(), "a")
        .pop()
        .expect("opening the page loads its chat");
    screen_shows(&mut h, Some(question_prompt("q1")));
    assert!(
        h.root(|root, _| root.chat_card("a")).is_some(),
        "the prompt is on the stack while the load is out"
    );
    assert!(!drawn(&mut h, "phone-prompt-sheet"));

    back_gesture(&mut h, BackGesturePhase::Started);
    back_gesture(&mut h, BackGesturePhase::Committed);

    assert_eq!(shown(&mut h).as_deref(), Some("a"));
    assert!(
        drawn(&mut h, "phone-session-chat"),
        "back steps to the Chat page as with no sheet up"
    );

    h.host(&saved_remote()).send(DaemonMessage::Chat {
        session_id: "a".to_owned(),
        entries: vec![user("u1")],
        hidden: 0,
        screen: ChatScreen {
            composer: ComposerState::Dialog,
            prompt: Some(question_prompt("q1")),
        },
        request_id: request,
    });
    h.cx.run_until_parked();

    assert!(
        drawn(&mut h, "phone-prompt-sheet"),
        "the prompt rises once the load lands"
    );
    assert!(!drawn(&mut h, "phone-prompt-show"));
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
    type_into_composer(&mut h, text);
    h
}

/// Taps session `a`'s composer field and types `text` into it.
fn type_into_composer(h: &mut Harness<'_>, text: &str) {
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

/// Gives the phone a [`FakePasteboard`] holding `image` and, when `text`,
/// some text, and hands back the spec's handle on it.
fn fake_pasteboard(
    h: &mut Harness<'_>,
    image: Option<(Vec<u8>, ImageFormat)>,
    text: bool,
) -> Arc<FakePasteboard> {
    let board = Arc::new(FakePasteboard::new(image, text));
    let given = board.clone();
    h.cx.update(|window, cx| {
        let phone = window
            .root::<PhoneRoot>()
            .flatten()
            .expect("the window's root is the phone's");
        phone.update(cx, |phone, _| phone.set_pasteboard(given));
    });
    board
}

/// Gives the phone a [`FakePhotoPicker`] whose every pick answers `answer`,
/// and hands back the spec's handle on it.
fn fake_photos(
    h: &mut Harness<'_>,
    answer: Option<(Vec<u8>, ImageFormat)>,
) -> Arc<FakePhotoPicker> {
    let picker = Arc::new(FakePhotoPicker::answering(answer));
    let given = picker.clone();
    h.cx.update(|window, cx| {
        let phone = window
            .root::<PhoneRoot>()
            .flatten()
            .expect("the window's root is the phone's");
        phone.update(cx, |phone, _| phone.set_photos(given));
    });
    picker
}

/// An `UploadFile` the phone sent: its request id, session, name and bytes.
#[derive(Debug)]
struct Upload {
    request_id: String,
    session_id: String,
    name: String,
    bytes: Vec<u8>,
}

/// The `UploadFile`s [`REMOTE`] was sent since the last drain.
fn uploads(h: &mut Harness<'_>) -> Vec<Upload> {
    h.host(&saved_remote())
        .sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::UploadFile {
                request_id,
                session_id,
                name,
                data_b64,
            } => Some(Upload {
                request_id,
                session_id,
                name,
                bytes: B64.decode(data_b64).expect("upload base64"),
            }),
            _ => None,
        })
        .collect()
}

#[gpui::test]
fn paste_uploads_a_clipboard_image_that_holds_no_text(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded_on(cx, &dir, &[FEATURE_UPLOAD_FILE]);
    let png = vec![0x89, b'P', b'N', b'G', 1, 2, 3];
    let board = fake_pasteboard(&mut h, Some((png.clone(), ImageFormat::Png)), false);

    h.click_on("phone-composer-paste");

    assert_eq!(board.reads(), 1, "the image is read on the tap");
    let [upload]: [Upload; 1] = uploads(&mut h).try_into().expect("exactly one upload");
    assert_eq!(upload.session_id, "a");
    assert_eq!(upload.name, "paste.png", "the desktop paste's name");
    assert_eq!(upload.bytes, png, "the pasteboard's own bytes go up");
    assert_eq!(composer_text(&mut h).as_deref(), Some(""));

    h.host(&saved_remote()).send(DaemonMessage::FileUploaded {
        request_id: upload.request_id,
        session_id: upload.session_id,
        path: "/work/.tmp/rt-uploads/paste-1.png".to_owned(),
    });
    assert_eq!(
        composer_text(&mut h).as_deref(),
        Some("/work/.tmp/rt-uploads/paste-1.png "),
        "the host path and one space are inserted at the caret"
    );
}

#[gpui::test]
fn paste_with_text_pastes_the_text_and_uploads_nothing(cx: &mut TestAppContext) {
    for image in [None, Some((vec![1, 2, 3], ImageFormat::Png))] {
        let dir = TestDir::new();
        let mut h = chat_loaded_on(cx, &dir, &[FEATURE_UPLOAD_FILE]);
        let had_image = image.is_some();
        let board = fake_pasteboard(&mut h, image, true);
        h.set_clipboard("pasted text");

        h.click_on("phone-composer-paste");

        assert_eq!(
            composer_text(&mut h).as_deref(),
            Some("pasted text"),
            "text wins (image too: {had_image})"
        );
        assert_eq!(
            board.reads(),
            0,
            "no image is read (image too: {had_image})"
        );
        assert!(
            uploads(&mut h).is_empty(),
            "nothing goes up (image too: {had_image})"
        );
    }
}

#[gpui::test]
fn paste_never_reads_the_image_on_a_host_without_upload_file(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded_on(cx, &dir, &[]);
    let board = fake_pasteboard(&mut h, Some((vec![1, 2, 3], ImageFormat::Png)), false);

    h.click_on("phone-composer-paste");

    assert_eq!(
        board.reads(),
        0,
        "no image is read for a host that cannot take it"
    );
    assert!(uploads(&mut h).is_empty(), "nothing goes up");
}

#[gpui::test]
fn photos_is_drawn_only_when_the_host_takes_uploads(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded_on(cx, &dir, &[]);
    assert!(
        !drawn(&mut h, "phone-composer-photos"),
        "a host without upload_file draws no Photos"
    );

    let dir = TestDir::new();
    let mut h = chat_loaded_on(cx, &dir, &[FEATURE_UPLOAD_FILE]);
    assert!(
        drawn(&mut h, "phone-composer-photos"),
        "a host listing upload_file draws Photos"
    );
}

#[gpui::test]
fn a_picked_photo_uploads_and_inserts_its_path(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded_on(cx, &dir, &[FEATURE_UPLOAD_FILE]);
    let jpeg = vec![0xFF, 0xD8, 0xFF, 4, 5, 6];
    let picker = fake_photos(&mut h, Some((jpeg.clone(), ImageFormat::Jpeg)));

    h.click_on("phone-composer-photos");

    assert_eq!(picker.picks(), 1, "the tap opens the picker");
    let [upload]: [Upload; 1] = uploads(&mut h).try_into().expect("exactly one upload");
    assert_eq!(upload.session_id, "a");
    assert!(
        upload.name.starts_with("photo-")
            && std::path::Path::new(&upload.name)
                .extension()
                .is_some_and(|ext| ext == "jpg"),
        "a picked photo's name: {}",
        upload.name
    );
    assert_eq!(upload.bytes, jpeg, "the picked bytes go up");

    h.host(&saved_remote()).send(DaemonMessage::FileUploaded {
        request_id: upload.request_id,
        session_id: upload.session_id,
        path: "/work/.tmp/rt-uploads/photo.jpg".to_owned(),
    });
    assert_eq!(
        composer_text(&mut h).as_deref(),
        Some("/work/.tmp/rt-uploads/photo.jpg ")
    );
}

#[gpui::test]
fn a_cancelled_pick_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded_on(cx, &dir, &[FEATURE_UPLOAD_FILE]);
    let picker = fake_photos(&mut h, None);

    h.click_on("phone-composer-photos");

    assert_eq!(picker.picks(), 1, "the tap opens the picker");
    assert!(uploads(&mut h).is_empty(), "a cancelled pick sends nothing");
    assert_eq!(composer_text(&mut h).as_deref(), Some(""));
}

#[gpui::test]
fn a_composer_quick_reply_sends_a_chat_message(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded(cx, &dir, vec![user("u1")]);
    h.click_on("phone-composer-quick");
    assert!(drawn(&mut h, "phone-dock-quick"), "the dock opens");
    h.host(&saved_remote()).sent();

    h.click_on("phone-quick-common-0");

    let sent = h.host(&saved_remote()).sent();
    let chats: Vec<&str> = sent
        .iter()
        .filter_map(|msg| match msg {
            ClientMessage::ChatSend {
                session_id, text, ..
            } if session_id == "a" => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        chats,
        ["continue"],
        "a Quick reply sends its text as a chat message"
    );
    assert!(
        !sent
            .iter()
            .any(|msg| matches!(msg, ClientMessage::SendInput { .. })),
        "and not as raw input: {sent:?}"
    );
    assert!(
        drawn(&mut h, "phone-dock-quick"),
        "the dock stays open after a tap"
    );
}

#[gpui::test]
fn a_composer_quick_reply_is_disabled_while_the_host_is_disconnected(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded(cx, &dir, vec![user("u1")]);
    h.click_on("phone-composer-quick");
    let target: Target = saved_remote();
    let mut conn = Connection::for_target(target.clone());
    conn.on_connecting();
    conn.on_socket_open(8787);
    let _ = conn.on_closed("dropped".to_owned());
    h.host(&target).event(NetEvent::State(conn));
    h.cx.run_until_parked();
    h.host(&saved_remote()).sent();
    assert!(
        !h.root(|root, _| root.chat_send_enabled("a", "continue")),
        "no connection greys the reply"
    );

    h.click_on("phone-quick-common-0");

    let sent = h.host(&saved_remote()).sent();
    assert!(
        !sent.iter().any(|msg| matches!(
            msg,
            ClientMessage::ChatSend { .. } | ClientMessage::SendInput { .. }
        )),
        "a greyed reply sends nothing: {sent:?}"
    );

    let mut host = h.host(&target);
    host.connect();
    host.send(DaemonMessage::Sessions { sessions: listed() });
    h.cx.run_until_parked();
    assert!(
        h.root(|root, _| root.chat_send_enabled("a", "continue")),
        "the lost reply leaves no send in flight, so the reconnected host can send"
    );
}

#[gpui::test]
fn a_composer_quick_reply_is_disabled_under_a_dialog(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = prompted(cx, &dir, permission_prompt("p1"));
    assert!(
        !h.root(|root, _| root.chat_send_enabled("a", "continue")),
        "a card up greys the reply"
    );

    // The card's sheet occludes the composer's buttons, so the Quick dock
    // cannot even open under it: no reply of it is on screen to be tapped.
    h.click_on("phone-composer-quick");
    assert!(
        !drawn(&mut h, "phone-quick-common-0"),
        "no reply of the dock reaches the screen under a card"
    );
    let sent = h.host(&saved_remote()).sent();
    assert!(
        !sent
            .iter()
            .any(|msg| matches!(msg, ClientMessage::ChatSend { .. })),
        "and nothing is sent: {sent:?}"
    );
}

#[gpui::test]
fn a_refused_composer_quick_reply_shows_the_reason(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded(cx, &dir, vec![user("u1")]);
    h.click_on("phone-composer-quick");
    h.host(&saved_remote()).sent();
    h.click_on("phone-quick-common-0");
    let sent = chat_sends(&mut h);
    assert_eq!(sent.len(), 1, "one send to refuse: {sent:?}");

    h.host(&saved_remote()).send(DaemonMessage::Error {
        message: "the prompt box holds a draft".to_owned(),
        request_id: sent[0].1.clone(),
        code: Some(CHAT_REFUSED.to_owned()),
    });
    h.cx.run_until_parked();

    assert_eq!(
        h.root(|root, _| root.chat_composer_error("a")).as_deref(),
        Some("the prompt box holds a draft"),
        "the daemon's reason is kept"
    );
    assert!(
        drawn(&mut h, "phone-composer-error"),
        "the reason shows under the field"
    );
}

#[gpui::test]
fn a_shell_session_offers_only_y_and_n(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir, "n");

    assert_eq!(phone(&mut h, PhoneRoot::quick_reply_items), ["y", "n"]);
}

// ── Quick replies saved in native-ui.json ──────────────────────────────────

/// A saved reply group.
fn reply_group(title: &str, replies: &[&str]) -> serde_json::Value {
    json!({ "title": title, "replies": replies })
}

/// A phone with `ui` saved beside its host, session `id`'s page open and its
/// chat's load answered with one message.
fn saved_phone<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    ui: &serde_json::Value,
    id: &str,
) -> Harness<'a> {
    let mut h = open_phone_saved(cx, dir, ui, &[]);
    h.click_on(&row(id));
    let sent = h.host(&saved_remote()).sent();
    let request = loads_of(&sent, id)
        .pop()
        .expect("opening the page loads its chat");
    h.host(&saved_remote())
        .send(chat_reply(id, request, vec![user("u1")]));
    h
}

#[gpui::test]
fn saved_agent_replies_replace_the_default_groups(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let ui = json!({ "phone_agent_replies": [reply_group("mine", &["run the tests"])] });
    let mut h = saved_phone(cx, &dir, &ui, "a");

    h.click_on("phone-composer-quick");

    assert!(drawn(&mut h, "phone-dock-quick"), "the dock opens");
    assert!(drawn(&mut h, "phone-quick-mine-0"), "the saved reply shows");
    assert!(
        !drawn(&mut h, "phone-quick-confirm-0"),
        "the saved set replaces the defaults"
    );
}

#[gpui::test]
fn saved_shell_replies_show_for_a_shell(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let ui = json!({ "phone_shell_replies": [reply_group("mine", &["ls -la"])] });
    let mut h = open_phone_saved(cx, &dir, &ui, &[]);

    h.click_on(&row("n"));

    assert_eq!(phone(&mut h, PhoneRoot::quick_reply_items), ["ls -la"]);
}

#[gpui::test]
fn saved_reply_sends_its_text_as_a_chat_message(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let ui = json!({ "phone_agent_replies": [reply_group("mine", &["run the tests"])] });
    let mut h = saved_phone(cx, &dir, &ui, "a");
    h.host(&saved_remote()).sent();
    h.click_on("phone-composer-quick");

    h.click_on("phone-quick-mine-0");

    let sends = chat_sends(&mut h);
    assert_eq!(sends.len(), 1, "one send: {sends:?}");
    assert_eq!(
        sends[0].0, "run the tests",
        "a saved reply sends its text as a chat message"
    );
}

#[gpui::test]
fn saved_empty_set_shows_the_empty_line(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let ui = json!({ "phone_agent_replies": [] });
    let mut h = saved_phone(cx, &dir, &ui, "a");

    h.click_on("phone-composer-quick");

    assert!(drawn(&mut h, "phone-dock-quick"), "the dock opens");
    assert!(
        drawn(&mut h, "phone-quick-empty"),
        "an empty set says where to add replies"
    );
    assert!(
        !drawn(&mut h, "phone-quick-confirm-0"),
        "an empty set shows no defaults"
    );
}

#[gpui::test]
fn saved_group_with_no_replies_draws_no_section(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let ui = json!({
        "phone_agent_replies": [
            reply_group("more", &[]),
            reply_group("common", &["continue"]),
        ],
    });
    let mut h = saved_phone(cx, &dir, &ui, "a");

    h.click_on("phone-composer-quick");

    assert!(
        !drawn(&mut h, "phone-quick-group-more"),
        "a group with no replies draws no section"
    );
    assert!(
        drawn(&mut h, "phone-quick-group-common"),
        "a group with replies draws its section"
    );
    assert!(drawn(&mut h, "phone-quick-common-0"), "with its reply");
    assert!(
        !drawn(&mut h, "phone-quick-empty"),
        "a set with a reply says nothing about adding replies"
    );
}

#[gpui::test]
fn saved_set_of_only_empty_groups_shows_the_empty_line(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let ui = json!({ "phone_agent_replies": [reply_group("more", &[])] });
    let mut h = saved_phone(cx, &dir, &ui, "a");

    h.click_on("phone-composer-quick");

    assert!(
        !drawn(&mut h, "phone-quick-group-more"),
        "a group with no replies draws no section"
    );
    assert!(
        drawn(&mut h, "phone-quick-empty"),
        "a set of only empty groups says where replies are added"
    );
}

#[gpui::test]
fn saved_by_set_phone_replies_and_redrawn(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = saved_phone(cx, &dir, &json!({}), "a");
    h.click_on("phone-composer-quick");
    assert!(drawn(&mut h, "phone-quick-confirm-0"), "the defaults show");

    h.root_update(|root, _, cx| {
        let groups = vec![ReplyGroup {
            title: "mine".to_owned(),
            replies: vec!["run the tests".to_owned()],
        }];
        root.set_phone_replies(ReplySet::Agent, Some(groups), cx);
    });

    assert_eq!(
        saved_ui(&dir)["phone_agent_replies"],
        json!([reply_group("mine", &["run the tests"])]),
        "the set is saved"
    );
    assert!(
        drawn(&mut h, "phone-quick-mine-0"),
        "the open dock redraws with it"
    );
    assert!(
        !drawn(&mut h, "phone-quick-confirm-0"),
        "the defaults are gone"
    );
}

#[gpui::test]
fn keys_types_each_key(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded(cx, &dir, vec![user("u1")]);
    h.host(&saved_remote()).sent();
    h.click_on("phone-composer-keys");
    assert!(drawn(&mut h, "phone-dock-keys"), "the dock opens");

    h.click_on("phone-key-0");
    assert_eq!(h.host(&saved_remote()).sent_input("a"), b"\x1b", "Esc");

    h.click_on("phone-key-11");
    assert_eq!(h.host(&saved_remote()).sent_input("a"), b"\x03", "Ctrl+C");
}

#[gpui::test]
fn the_keys_dock_lists_the_grid_in_order(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded(cx, &dir, vec![user("u1")]);
    h.click_on("phone-composer-keys");

    for index in 0..=20 {
        assert!(
            drawn(&mut h, &format!("phone-key-{index}")),
            "phone-key-{index}"
        );
    }
    assert!(!drawn(&mut h, "phone-key-21"), "the grid holds 21 keys");
}

#[gpui::test]
fn the_keys_dock_opens_with_ctrl_unarmed(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded(cx, &dir, vec![user("u1")]);
    h.host(&saved_remote()).sent();
    h.click_on("phone-composer-keys");
    h.click_on("phone-key-2");
    h.click_on("phone-composer-keys");
    h.click_on("phone-composer-keys");

    h.click_on("phone-key-3");

    assert_eq!(
        h.host(&saved_remote()).sent_input("a"),
        b"\x1b[A",
        "a reopened dock leaves no armed Ctrl behind"
    );
}

#[gpui::test]
fn the_dock_ctrl_arms_the_next_key_once(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded(cx, &dir, vec![user("u1")]);
    h.host(&saved_remote()).sent();
    h.click_on("phone-composer-keys");

    h.click_on("phone-key-2");
    h.click_on("phone-key-3");
    assert_eq!(
        h.host(&saved_remote()).sent_input("a"),
        b"\x1b[1;5A",
        "armed Ctrl turns the next key into Ctrl+Up"
    );

    h.click_on("phone-key-3");
    assert_eq!(
        h.host(&saved_remote()).sent_input("a"),
        b"\x1b[A",
        "and disarms after one key"
    );
}

#[gpui::test]
fn the_dock_ctrl_leaves_a_chord_armed(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded(cx, &dir, vec![user("u1")]);
    h.host(&saved_remote()).sent();
    h.click_on("phone-composer-keys");

    h.click_on("phone-key-2");
    h.click_on("phone-key-11");
    assert_eq!(
        h.host(&saved_remote()).sent_input("a"),
        b"\x03",
        "Ctrl+C sends its control byte"
    );

    h.click_on("phone-key-3");
    assert_eq!(
        h.host(&saved_remote()).sent_input("a"),
        b"\x1b[1;5A",
        "a chord leaves Ctrl armed"
    );
}

#[gpui::test]
fn the_dock_paste_pastes_into_the_composer(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded(cx, &dir, vec![user("u1")]);
    h.set_clipboard("pasted text");
    h.click_on("phone-composer-keys");

    h.click_on("phone-key-7");

    assert_eq!(composer_text(&mut h).as_deref(), Some("pasted text"));
}

#[gpui::test]
fn the_dock_quick_switches_to_the_quick_dock(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded(cx, &dir, vec![user("u1")]);
    h.click_on("phone-composer-keys");

    h.click_on("phone-key-8");

    assert!(drawn(&mut h, "phone-dock-quick"), "the Quick dock shows");
    assert!(!drawn(&mut h, "phone-dock-keys"), "the Keys dock closed");
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

// ── The completion strip: the session's commands and files ─────────────────

/// Session `a`'s page with its chat loaded on a host that offers the command
/// catalog.
fn cataloging_phone<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    chat_loaded_on(cx, dir, &[FEATURE_CHAT_CATALOG])
}

/// Session `a`'s page with its chat loaded on a host that offers file search.
fn file_searching_phone<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    chat_loaded_on(cx, dir, &[FEATURE_FILE_SEARCH])
}

/// The chips session `a`'s completion strip offers, while it shows.
fn chips(h: &mut Harness<'_>) -> Option<Vec<String>> {
    h.root(|root, _| root.chat_completion_chips("a"))
}

/// A command named `name`, with its description and overlay flag.
fn command(name: &str, description: Option<&str>, opens_terminal: bool) -> CatalogEntry {
    CatalogEntry {
        name: name.to_owned(),
        kind: CatalogEntryKind::Command,
        description: description.map(str::to_owned),
        source: CatalogEntrySource::BuiltIn,
        opens_terminal,
    }
}

/// The host's commands: `compact`, `config` (an overlay) and `code-review`,
/// in that order.
fn host_commands() -> Vec<CatalogEntry> {
    vec![
        command("compact", Some("Compress the conversation"), false),
        command("config", None, true),
        command("code-review", Some("Review the branch"), false),
    ]
}

/// The request ids of the `ListCatalog`s for session `a` sent since the last
/// drain.
fn catalog_requests(h: &mut Harness<'_>) -> Vec<Option<String>> {
    h.host(&saved_remote())
        .sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::ListCatalog {
                session_id,
                request_id,
            } if session_id == "a" => Some(request_id),
            _ => None,
        })
        .collect()
}

/// The one `ListCatalog` for session `a` sent since the last drain: its
/// request id.
fn one_catalog_request(h: &mut Harness<'_>) -> Option<String> {
    let sent = catalog_requests(h);
    assert_eq!(sent.len(), 1, "one catalog request: {sent:?}");
    assert!(sent[0].is_some(), "it carries a request id");
    sent[0].clone()
}

/// The daemon answers the catalog request `request_id` for session `a` with
/// `entries`.
fn cataloged(h: &mut Harness<'_>, request_id: Option<String>, entries: Vec<CatalogEntry>) {
    h.host(&saved_remote()).send(DaemonMessage::Catalog {
        session_id: "a".to_owned(),
        entries,
        request_id,
    });
}

/// Session `a`'s page holding `/co` with the host's commands answered, so
/// its strip shows their chips, everything sent so far drained.
fn on_the_command_strip<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = cataloging_phone(cx, dir);
    h.host(&saved_remote()).sent();
    type_into_composer(&mut h, "/co");
    let id = one_catalog_request(&mut h);
    cataloged(&mut h, id, host_commands());
    h.host(&saved_remote()).sent();
    h
}

/// Moves the test clock past the file search's debounce.
fn past_debounce(h: &mut Harness<'_>) {
    let ms = u64::try_from(chat::FILE_SEARCH_DEBOUNCE.as_millis()).unwrap_or(u64::MAX);
    h.cx.executor()
        .advance_clock(Duration::from_millis(ms + 10));
    h.cx.run_until_parked();
}

/// The query, limit and request id of each `SearchFiles` for session `a`
/// sent since the last drain.
fn file_searches(h: &mut Harness<'_>) -> Vec<(String, Option<u32>, Option<String>)> {
    h.host(&saved_remote())
        .sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::SearchFiles {
                session_id,
                query,
                limit,
                request_id,
            } if session_id == "a" => Some((query, limit, request_id)),
            _ => None,
        })
        .collect()
}

/// The one file search for session `a` sent since the last drain: its
/// request id, its query `ma` and the desktop's limit.
fn one_file_search(h: &mut Harness<'_>) -> Option<String> {
    let sent = file_searches(h);
    assert_eq!(sent.len(), 1, "one search: {sent:?}");
    let (query, limit, request_id) = sent[0].clone();
    assert_eq!(query, "ma");
    assert_eq!(limit, Some(50), "the desktop's limit");
    request_id
}

/// The daemon answers the search `request_id` for session `a` with `files`,
/// each a path and its reference.
fn files_matched(h: &mut Harness<'_>, request_id: Option<String>, files: &[(&str, &str)]) {
    h.host(&saved_remote()).send(DaemonMessage::FileMatches {
        session_id: "a".to_owned(),
        matches: files
            .iter()
            .map(|(path, reference)| FileMatch {
                path: (*path).to_owned(),
                reference: (*reference).to_owned(),
            })
            .collect(),
        truncated: false,
        request_id,
    });
}

#[gpui::test]
fn typing_a_slash_asks_for_the_catalog_and_the_strip_lists_its_chips(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = cataloging_phone(cx, &dir);
    assert!(
        !drawn(&mut h, "phone-completion-strip"),
        "no token, no strip"
    );
    h.host(&saved_remote()).sent();

    type_into_composer(&mut h, "/co");

    let id = one_catalog_request(&mut h);
    assert_eq!(chips(&mut h), None, "nothing before the answer");
    cataloged(&mut h, id, host_commands());
    assert_eq!(
        chips(&mut h),
        Some(vec![
            "/compact".to_owned(),
            "/config \u{b7} terminal".to_owned(),
            "/code-review".to_owned(),
        ]),
        "the strip lists the catalog's chips, the overlay flagged"
    );
    assert!(drawn(&mut h, "phone-completion-strip"));
}

#[gpui::test]
fn a_chip_tap_inserts_the_pick_and_leaves_the_keyboard_alone(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_the_command_strip(cx, &dir);
    clear_keyboard_calls();

    h.tap_on("phone-completion-1");

    assert_eq!(
        composer_text(&mut h).as_deref(),
        Some("/config "),
        "the pick and one space"
    );
    assert_eq!(chat_sends(&mut h), Vec::new(), "a pick only inserts");
    assert_eq!(
        keyboard_calls(),
        [],
        "the tap neither raises nor lowers the keyboard"
    );
    assert_eq!(chips(&mut h), None, "the pick ends the token");
    assert!(!drawn(&mut h, "phone-completion-strip"));
}

#[gpui::test]
fn typing_an_at_lists_the_matching_files_chips(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = file_searching_phone(cx, &dir);
    h.host(&saved_remote()).sent();

    type_into_composer(&mut h, "@ma");
    assert_eq!(
        file_searches(&mut h),
        Vec::new(),
        "nothing inside the debounce"
    );
    past_debounce(&mut h);
    let id = one_file_search(&mut h);
    files_matched(
        &mut h,
        id,
        &[
            ("src/a/b.rs", "@./src/a/b.rs"),
            ("src/map.rs", "@./src/map.rs"),
        ],
    );

    assert_eq!(
        chips(&mut h),
        Some(vec!["b.rs".to_owned(), "map.rs".to_owned()]),
        "each chip is the file's last path part"
    );
    assert!(drawn(&mut h, "phone-completion-strip"));
    h.host(&saved_remote()).sent();

    h.tap_on("phone-completion-0");

    assert_eq!(
        composer_text(&mut h).as_deref(),
        Some("@./src/a/b.rs "),
        "the pick inserts the daemon's reference"
    );
    assert_eq!(chat_sends(&mut h), Vec::new());
}

#[gpui::test]
fn return_with_the_strip_showing_inserts_a_line_break(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_the_command_strip(cx, &dir);
    assert!(drawn(&mut h, "phone-completion-strip"));

    h.keys("enter");

    assert_eq!(
        composer_text(&mut h).as_deref(),
        Some("/co\n"),
        "Return breaks the line"
    );
    assert_eq!(chat_sends(&mut h), Vec::new(), "and sends nothing");
}

#[gpui::test]
fn the_strips_chips_are_44_px_tall(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_the_command_strip(cx, &dir);

    for index in 0..3 {
        let selector = format!("phone-completion-{index}");
        let bounds = h.bounds(&selector);
        assert!(
            bounds.size.height >= touch_target(),
            "{selector} is {} tall",
            bounds.size.height
        );
    }
}

#[gpui::test]
fn a_space_ends_the_token_and_the_strip(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_the_command_strip(cx, &dir);
    assert!(drawn(&mut h, "phone-completion-strip"));

    h.keys("space");

    assert_eq!(composer_text(&mut h).as_deref(), Some("/co "));
    assert_eq!(chips(&mut h), None, "a space ends the token");
    assert!(!drawn(&mut h, "phone-completion-strip"));
}

// ── The prompt box's draft and the suggested next prompt ───────────────────

/// Session `a`'s page with its chat loaded, on a host that lists
/// `features`.
fn chat_loaded_on<'a>(cx: &'a mut TestAppContext, dir: &TestDir, features: &[&str]) -> Harness<'a> {
    let mut h = open_phone_on(cx, dir, features);
    h.click_on(&row("a"));
    assert_eq!(shown(&mut h).as_deref(), Some("a"));
    let sent = h.host(&saved_remote()).sent();
    let request = loads_of(&sent, "a")
        .pop()
        .expect("opening the page loads its chat");
    h.host(&saved_remote())
        .send(chat_reply("a", request, vec![user("u1")]));
    h
}

/// Session `a`'s terminal now shows its prompt box in `state`.
fn composer_shows(h: &mut Harness<'_>, state: ComposerState) {
    h.host(&saved_remote())
        .send(DaemonMessage::ChatScreenChanged {
            session_id: "a".to_owned(),
            screen: ChatScreen {
                composer: state,
                prompt: None,
            },
        });
}

/// Session `a`'s page on a host that can pull or send the prompt box's
/// draft, the box holding one.
fn on_a_draft<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = chat_loaded_on(cx, dir, &[FEATURE_CHAT_DRAFT]);
    composer_shows(&mut h, ComposerState::Draft);
    h.host(&saved_remote()).sent();
    h
}

/// Session `a`'s page with the agent suggesting `text` as the next prompt.
fn on_a_suggestion<'a>(cx: &'a mut TestAppContext, dir: &TestDir, text: &str) -> Harness<'a> {
    let mut h = chat_loaded(cx, dir, vec![user("u1")]);
    composer_shows(
        &mut h,
        ComposerState::Suggestion {
            text: text.to_owned(),
        },
    );
    h.host(&saved_remote()).sent();
    h
}

#[gpui::test]
fn a_phone_draft_offers_pull_and_send(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_a_draft(cx, &dir);

    assert_eq!(
        composer_note(&mut h).as_deref(),
        Some("The prompt box has a draft.")
    );
    assert!(drawn(&mut h, "phone-composer-note"));
    for selector in ["phone-draft-pull", "phone-draft-send"] {
        assert!(drawn(&mut h, selector), "{selector} shows");
        let height = h.bounds(selector).size.height;
        assert!(height >= touch_target(), "{selector} is {height} tall");
    }
    assert!(
        !drawn(&mut h, "phone-draft-clear"),
        "a host without chat_clear_draft shows no Clear"
    );
}

/// Session `a`'s page on a host that can also clear the prompt box's
/// draft, the box holding one.
fn on_a_clear_draft<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = chat_loaded_on(cx, dir, &[FEATURE_CHAT_DRAFT, FEATURE_CHAT_CLEAR_DRAFT]);
    composer_shows(&mut h, ComposerState::Draft);
    h.host(&saved_remote()).sent();
    h
}

/// The request ids of the `ChatClearDraft`s for `a` sent since the last
/// drain.
fn phone_clears(h: &mut Harness<'_>) -> Vec<Option<String>> {
    h.host(&saved_remote())
        .sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::ChatClearDraft {
                session_id,
                request_id,
            } if session_id == "a" => Some(request_id),
            _ => None,
        })
        .collect()
}

#[gpui::test]
fn a_phone_draft_on_a_clear_draft_host_offers_clear_too(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_a_clear_draft(cx, &dir);

    assert_eq!(
        composer_note(&mut h).as_deref(),
        Some("The prompt box has a draft. Pull, send or clear it first.")
    );
    for selector in ["phone-draft-pull", "phone-draft-send", "phone-draft-clear"] {
        assert!(drawn(&mut h, selector), "{selector} shows");
        let height = h.bounds(selector).size.height;
        assert!(height >= touch_target(), "{selector} is {height} tall");
    }
    let send = h.bounds("phone-draft-send");
    let clear = h.bounds("phone-draft-clear");
    assert!(send.origin.x < clear.origin.x, "Clear comes after Send");
}

#[gpui::test]
fn tapping_clear_asks_then_its_clear_sends_one_draft_clear(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_a_clear_draft(cx, &dir);

    h.tap_on("phone-draft-clear");

    assert!(phone_clears(&mut h).is_empty(), "a tap only asks");
    assert!(drawn(&mut h, "phone-draft-clear-ask"), "the question shows");
    assert!(drawn(&mut h, "phone-draft-clear-confirm"));
    assert!(drawn(&mut h, "phone-draft-clear-cancel"));
    assert!(!drawn(&mut h, "phone-draft-pull"), "the row asks in place");

    h.tap_on("phone-draft-clear-confirm");

    let sent = phone_clears(&mut h);
    assert_eq!(sent.len(), 1, "one clear goes out: {sent:?}");
    assert!(sent[0].is_some(), "under a request id");
    assert!(
        !drawn(&mut h, "phone-draft-clear-ask"),
        "the row drops back"
    );
    assert!(drawn(&mut h, "phone-draft-clear"));
}

#[gpui::test]
fn cancelling_a_phone_draft_clear_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_a_clear_draft(cx, &dir);

    h.tap_on("phone-draft-clear");
    h.tap_on("phone-draft-clear-cancel");

    let sent = h.host(&saved_remote()).sent();
    assert!(
        !sent.iter().any(|msg| matches!(
            msg,
            ClientMessage::ChatSend { .. }
                | ClientMessage::ChatPullDraft { .. }
                | ClientMessage::ChatSendDraft { .. }
                | ClientMessage::ChatClearDraft { .. }
        )),
        "nothing is sent: {sent:?}"
    );
    assert!(!drawn(&mut h, "phone-draft-clear-ask"));
    for selector in ["phone-draft-pull", "phone-draft-send", "phone-draft-clear"] {
        assert!(drawn(&mut h, selector), "{selector} is back");
    }
}

#[gpui::test]
fn a_phone_draft_clear_question_drops_when_the_draft_goes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_a_clear_draft(cx, &dir);

    h.tap_on("phone-draft-clear");
    composer_shows(&mut h, ComposerState::Ready);
    composer_shows(&mut h, ComposerState::Draft);

    assert!(!drawn(&mut h, "phone-draft-clear-ask"), "the question went");
    assert!(drawn(&mut h, "phone-draft-clear"));
    assert!(phone_clears(&mut h).is_empty(), "nothing is sent");
}

#[gpui::test]
fn tapping_pull_sends_a_pull_and_the_draft_lands_in_the_composer(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_a_draft(cx, &dir);
    type_into_composer(&mut h, "and test");
    h.host(&saved_remote()).sent();

    h.tap_on("phone-draft-pull");

    let pulls: Vec<Option<String>> = h
        .host(&saved_remote())
        .sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::ChatPullDraft {
                session_id,
                request_id,
            } if session_id == "a" => Some(request_id),
            _ => None,
        })
        .collect();
    assert_eq!(pulls.len(), 1, "one pull goes out: {pulls:?}");
    assert!(pulls[0].is_some(), "under a request id");
    assert_eq!(
        composer_note(&mut h).as_deref(),
        Some("Pulling the draft\u{2026}")
    );
    assert!(
        !drawn(&mut h, "phone-draft-pull"),
        "no buttons while pulling"
    );
    assert!(!drawn(&mut h, "phone-draft-send"));

    h.host(&saved_remote()).send(DaemonMessage::ChatDraft {
        session_id: "a".to_owned(),
        request_id: pulls[0].clone(),
        text: "fix the bug".to_owned(),
        cleared: true,
    });

    assert_eq!(
        composer_text(&mut h).as_deref(),
        Some("fix the bug\nand test"),
        "the draft lands ahead of the typed text"
    );
    assert_eq!(h.root(|root, _| root.chat_composer_error("a")), None);
    assert!(drawn(&mut h, "phone-draft-pull"), "the buttons are back");
}

#[gpui::test]
fn tapping_send_sends_the_draft_as_it_stands(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_a_draft(cx, &dir);

    h.tap_on("phone-draft-send");

    let sent: Vec<Option<String>> = h
        .host(&saved_remote())
        .sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::ChatSendDraft {
                session_id,
                request_id,
            } if session_id == "a" => Some(request_id),
            _ => None,
        })
        .collect();
    assert_eq!(sent.len(), 1, "one send-draft goes out: {sent:?}");
    assert!(sent[0].is_some(), "under a request id");
    assert_eq!(composer_note(&mut h).as_deref(), Some("Sending\u{2026}"));
}

#[gpui::test]
fn a_host_without_chat_draft_shows_no_draft_buttons(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded(cx, &dir, vec![user("u1")]);

    composer_shows(&mut h, ComposerState::Draft);

    assert_eq!(
        composer_note(&mut h).as_deref(),
        Some("The prompt box has a draft. Send or clear it in the terminal first.")
    );
    assert!(drawn(&mut h, "phone-composer-note"));
    assert!(!drawn(&mut h, "phone-draft-pull"));
    assert!(!drawn(&mut h, "phone-draft-send"));
    assert!(!drawn(&mut h, "phone-draft-clear"));
}

#[gpui::test]
fn a_phone_suggestion_chip_sends_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = typed_composer(cx, &dir, "mine");
    composer_shows(
        &mut h,
        ComposerState::Suggestion {
            text: "write todo.py".to_owned(),
        },
    );
    h.host(&saved_remote()).sent();

    assert!(drawn(&mut h, "phone-suggestion"));
    assert_eq!(composer_note(&mut h), None);
    for selector in ["phone-suggestion-send", "phone-suggestion-pull"] {
        let height = h.bounds(selector).size.height;
        assert!(height >= touch_target(), "{selector} is {height} tall");
    }
    h.tap_on("phone-suggestion-send");

    let sends = chat_sends(&mut h);
    assert_eq!(sends.len(), 1, "one send: {sends:?}");
    assert_eq!(sends[0].0, "write todo.py");
    assert!(sends[0].1.is_some(), "under a request id");
    assert_eq!(
        composer_text(&mut h).as_deref(),
        Some("mine"),
        "the composer keeps its own text"
    );
}

#[gpui::test]
fn a_phone_suggestion_chip_pulls_it_into_the_composer(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_a_suggestion(cx, &dir, "write todo.py");

    h.tap_on("phone-suggestion-pull");

    assert_eq!(composer_text(&mut h).as_deref(), Some("write todo.py"));
    let sent = h.host(&saved_remote()).sent();
    assert!(
        !sent.iter().any(|msg| matches!(
            msg,
            ClientMessage::ChatSend { .. }
                | ClientMessage::ChatPullDraft { .. }
                | ClientMessage::ChatSendDraft { .. }
                | ClientMessage::ChatClearDraft { .. }
        )),
        "nothing is sent: {sent:?}"
    );
}

#[gpui::test]
fn the_suggestion_chips_buttons_split_its_width(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_a_suggestion(cx, &dir, "write todo.py");

    // A field's frame: 11 px of padding inside a 1 px border on each side.
    let inset = 12.0;
    let chip = h.bounds("phone-suggestion");
    let send = h.bounds("phone-suggestion-send");
    let pull = h.bounds("phone-suggestion-pull");
    let left = f32::from(send.left() - chip.left());
    let right = f32::from(chip.right() - pull.right());
    assert!(
        (left - inset).abs() <= 1.0 && (right - inset).abs() <= 1.0,
        "Send and Pull span the chip's inner width: {left} px in from the left, {right} px from the right"
    );
    for (selector, bounds) in [
        ("phone-suggestion-send", send),
        ("phone-suggestion-pull", pull),
    ] {
        let height = bounds.size.height;
        assert!(height >= touch_target(), "{selector} is {height} tall");
    }
}

#[gpui::test]
fn a_cut_phone_suggestion_has_no_buttons(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_a_suggestion(cx, &dir, "write a long sugg\u{2026}");

    assert!(drawn(&mut h, "phone-suggestion"));
    assert!(!drawn(&mut h, "phone-suggestion-send"));
    assert!(!drawn(&mut h, "phone-suggestion-pull"));
}

// ── The keyboard ───────────────────────────────────────────────────────────

/// The insets a phone draws with a `keyboard`-tall on-screen keyboard up,
/// its safe area the one an iPhone reports.
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

/// Draws `insets` around the phone's content in place of the window's.
fn draw_insets(h: &mut Harness<'_>, insets: Option<WindowInsets>) {
    h.cx.update(|window, cx| {
        let phone = window
            .root::<PhoneRoot>()
            .flatten()
            .expect("the window's root is the phone's");
        phone.update(cx, |phone, cx| phone.set_insets_override(insets, cx));
    });
    h.cx.run_until_parked();
}

#[gpui::test]
fn the_composer_sits_above_the_keyboard(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded(cx, &dir, vec![user("u1")]);
    draw_insets(&mut h, Some(keyboard_insets(px(336.0))));

    assert!(drawn(&mut h, "phone-composer"));
    let height = h.window_height();
    let composer = h.bounds("phone-composer");
    assert!(
        composer.bottom() <= px(height - 336.0) + px(1.0),
        "the composer sits above the keyboard: {composer:?} in a {height} px window"
    );
}

#[gpui::test]
fn the_prompt_sheet_keys_sit_above_the_keyboard(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = prompted(cx, &dir, question_prompt("q1"));
    draw_insets(&mut h, Some(keyboard_insets(px(336.0))));

    assert!(drawn(&mut h, "phone-prompt-esc"));
    let height = h.window_height();
    let keys = h.bounds("phone-prompt-esc");
    assert!(
        keys.bottom() <= px(height - 336.0) + px(1.0),
        "the sheet's keys sit above the keyboard: {keys:?} in a {height} px window"
    );
}

/// An option whose description is drawn under its label.
fn described_option(number: u32, label: &str, description: &str) -> ChatOption {
    ChatOption {
        number,
        label: label.to_owned(),
        description: Some(description.to_owned()),
        selected: false,
    }
}

/// A question `q1` whose card overruns the room a phone leaves it: a
/// subject and a body over several lines, and four described options.
fn tall_question_prompt() -> ChatPrompt {
    ChatPrompt {
        subject: "Which of these functions should the new tests cover first, \
            given that the release goes out to the users on the beta channel tomorrow \
            morning and the integer helpers changed the most since the last one, \
            while the floating point helpers only moved files and kept their bodies?"
            .to_owned(),
        body: "Nothing is answered until you pick one, and the card keeps whatever \
            you type into its box until the answer is sent, so a long question has \
            to stay readable while the keyboard is up."
            .to_owned(),
        options: vec![
            described_option(
                1,
                "sub",
                "Subtraction: returns a - b, and raises on overflow, so every caller \
                 that adds a check on the sign has to keep working unchanged.",
            ),
            described_option(
                2,
                "div",
                "Division: returns a / b, and raises on a zero divisor and on the \
                 overflow of the smallest integer divided by minus one.",
            ),
            described_option(
                3,
                "abs",
                "Absolute value: returns the distance from zero, and raises on the \
                 smallest integer, whose magnitude no integer can hold.",
            ),
            described_option(
                4,
                "pow",
                "Power: multiplies the base by itself, and raises when the exponent \
                 is negative or when the result would leave the integer's range.",
            ),
        ],
        ..sheet_prompt("q1", ChatPromptKind::Question, &[])
    }
}

#[gpui::test]
fn a_tall_prompt_card_keeps_the_sheet_on_screen_above_the_keyboard(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = prompted(cx, &dir, tall_question_prompt());
    // A phone's screen, the island's band and the keyboard.
    h.cx.simulate_resize(gpui::size(px(390.0), px(874.0)));
    draw_insets(&mut h, Some(keyboard_insets(px(336.0))));

    let height = h.window_height();
    let handle = h.bounds("phone-prompt-handle");
    assert!(
        handle.origin.y >= px(62.0),
        "the handle stays below the island: {handle:?} in a {height} px window"
    );
    let card = h.bounds("phone-prompt-card");
    assert!(
        card.size.height > px(300.0),
        "the card keeps its room: {card:?} in a {height} px window"
    );
    let esc = h.bounds("phone-prompt-esc");
    assert!(
        esc.bottom() <= px(height - 336.0) + px(1.0),
        "the keys stay above the keyboard: {esc:?} in a {height} px window"
    );

    // The keyboard hides: the room it held comes back and the card draws whole.
    draw_insets(&mut h, Some(keyboard_insets(px(0.0))));

    let card = h.bounds("phone-prompt-card");
    assert!(
        card.contains(&h.bounds("chat-card-4").center()),
        "the whole card draws when it fits: {card:?}"
    );
}

#[gpui::test]
fn the_composer_drops_back_when_the_keyboard_hides(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded(cx, &dir, vec![user("u1")]);
    draw_insets(&mut h, Some(keyboard_insets(px(336.0))));
    let height = h.window_height();
    assert!(
        h.bounds("phone-composer").bottom() <= px(height - 336.0) + px(1.0),
        "the composer rises with the keyboard"
    );

    draw_insets(&mut h, Some(keyboard_insets(px(0.0))));

    let composer = h.bounds("phone-composer");
    assert!(
        (f32::from(composer.bottom()) - (height - 34.0)).abs() <= 1.0,
        "the composer returns to the safe area's bottom: {composer:?} in a {height} px window"
    );
}

/// Answer `id`, whose first paragraph is one stretch's short form long
/// enough to wrap over most of the row, so a point at the row's middle
/// lands on it.
fn stretched(id: &str) -> ChatEntry {
    let short = "tap here to open the long form and read it all ".repeat(12);
    ChatEntry::Answer {
        id: id.to_owned(),
        text: format!("{{{{{}|the long form, opened.}}}} today.", short.trim_end()),
        live: false,
    }
}

/// Answer `a1`'s stretchtext links: each one's drawn text and its key.
fn stretch_links(h: &mut Harness<'_>) -> Vec<(String, String)> {
    h.root(|root, _| root.chat_answer_stretchtext("a", "a1"))
}

#[gpui::test]
fn a_tap_on_a_stretch_opens_its_long_form(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded(cx, &dir, vec![stretched("a1")]);
    let [(_, key)]: [(String, String); 1] = stretch_links(&mut h)
        .try_into()
        .expect("the answer draws one closed stretch");
    let answer = h.bounds("chat-row-phone-a1");
    let at = point(
        answer.origin.x + px(30.0),
        answer.origin.y + answer.size.height / 2.0,
    );

    h.touch(TouchPhase::Started, at);
    h.touch(TouchPhase::Ended, at);

    assert_eq!(
        stretch_links(&mut h),
        [("\u{2039}".to_owned(), key)],
        "one tap opens the stretch, leaving its closer as the answer's only link"
    );
}

// ── Putting the keyboard away ──────────────────────────────────────────────

/// The last call the app made on the on-screen keyboard, if any.
fn last_keyboard_call() -> Option<KeyboardCall> {
    keyboard_calls().last().copied()
}

#[gpui::test]
fn a_tap_on_the_chat_lowers_the_keyboard(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded(cx, &dir, vec![user("u1")]);
    draw_insets(&mut h, Some(keyboard_insets(px(336.0))));
    clear_keyboard_calls();

    h.tap_on("chat-row-phone-u1");

    assert_eq!(
        last_keyboard_call(),
        Some(KeyboardCall::Lower),
        "a tap anywhere on the chat puts the keyboard away"
    );
}

#[gpui::test]
fn a_tap_on_a_stretch_with_the_keyboard_up_opens_it_and_lowers_the_keyboard(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let mut h = chat_loaded(cx, &dir, vec![stretched("a1")]);
    draw_insets(&mut h, Some(keyboard_insets(px(336.0))));
    let [(_, key)]: [(String, String); 1] = stretch_links(&mut h)
        .try_into()
        .expect("the answer draws one closed stretch");
    let answer = h.bounds("chat-row-phone-a1");
    let at = point(
        answer.origin.x + px(30.0),
        answer.origin.y + answer.size.height / 2.0,
    );
    clear_keyboard_calls();

    h.touch(TouchPhase::Started, at);
    h.touch(TouchPhase::Ended, at);

    assert_eq!(
        stretch_links(&mut h),
        [("\u{2039}".to_owned(), key)],
        "the stretch still opens on the one tap"
    );
    assert_eq!(
        last_keyboard_call(),
        Some(KeyboardCall::Lower),
        "and the keyboard goes away under it"
    );
}

#[gpui::test]
fn a_tap_on_the_chat_with_the_keyboard_down_calls_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded(cx, &dir, vec![user("u1")]);
    draw_insets(&mut h, Some(keyboard_insets(px(0.0))));
    clear_keyboard_calls();

    h.tap_on("chat-row-phone-u1");

    assert_eq!(
        keyboard_calls(),
        [],
        "a tap on the chat with no keyboard up calls nothing"
    );
}

#[gpui::test]
fn the_composer_shows_hide_keyboard_only_while_the_keyboard_is_up(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded(cx, &dir, vec![user("u1")]);
    assert!(
        !drawn(&mut h, "phone-composer-hide"),
        "no Hide keyboard button with the keyboard down"
    );

    draw_insets(&mut h, Some(keyboard_insets(px(336.0))));

    assert!(
        drawn(&mut h, "phone-composer-hide"),
        "Hide keyboard shows with the keyboard up"
    );
    let hide = h.bounds("phone-composer-hide");
    let keys = h.bounds("phone-composer-keys");
    assert!(
        hide.origin.x > keys.origin.x,
        "Hide follows Keys: {hide:?} after {keys:?}"
    );
    assert!(
        hide.size.height >= touch_target(),
        "Hide is a touch target: {hide:?}"
    );
}

#[gpui::test]
fn hide_keyboard_lowers_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded(cx, &dir, vec![user("u1")]);
    draw_insets(&mut h, Some(keyboard_insets(px(336.0))));
    clear_keyboard_calls();

    h.tap_on("phone-composer-hide");

    assert_eq!(
        last_keyboard_call(),
        Some(KeyboardCall::Lower),
        "the composer's Hide keyboard button puts the keyboard away"
    );
}

#[gpui::test]
fn the_composer_row_keeps_hide_inside_it_with_the_keyboard_up(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded_on(cx, &dir, &[FEATURE_UPLOAD_FILE]);
    // The iPhone 17's width, with its keyboard up: five buttons in the row.
    h.cx.simulate_resize(gpui::size(px(402.0), px(874.0)));
    draw_insets(&mut h, Some(keyboard_insets(px(445.0))));

    assert!(
        drawn(&mut h, "phone-composer-photos"),
        "a host that takes uploads adds Photos, so the row holds five"
    );
    let row = h.bounds("phone-composer-buttons");
    let hide = h.bounds("phone-composer-hide");
    assert!(
        hide.size.width > px(0.0),
        "Hide keyboard is drawn while the keyboard is up"
    );
    assert!(
        hide.right() <= row.right() + px(1.0),
        "Hide stays inside its row: {hide:?} in {row:?}"
    );
}

// ── The chat's own buttons ─────────────────────────────────────────────────

/// The file `tailing_tool`'s call writes.
const LOG: &str = "C:/work/build.log";

/// A shell call `id` that writes [`LOG`], offering its Tail button.
fn tailing_tool(id: &str) -> ChatEntry {
    ChatEntry::Tool {
        id: id.to_owned(),
        name: "Bash".to_owned(),
        summary: "cargo build > build.log".to_owned(),
        input: "{\"command\": \"cargo build > build.log\"}".to_owned(),
        result: None,
        parent_id: None,
        tail_paths: vec![LOG.to_owned()],
        kind: ToolKind::Other,
        script: None,
    }
}

/// Session `a`'s page over a daemon that follows files, its chat's load
/// answered with `entries`.
fn chat_loaded_tailing<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    entries: Vec<ChatEntry>,
) -> Harness<'a> {
    seed_ui(
        dir,
        &json!({ "saved_hosts": [{ "name": REMOTE, "port": 8787 }] }),
    );
    let mut h = Harness::phone(cx, dir);
    h.tokens().set(REMOTE, "t1").expect("seed a token");
    let mut host = h.host(&saved_remote());
    host.connect_with_features(&[FEATURE_FILE_TAIL]);
    host.send(DaemonMessage::Repos {
        repos: vec![repo("r1", "C:/r1")],
    });
    host.send(DaemonMessage::Sessions { sessions: listed() });
    h.click_on(&row("a"));
    let sent = h.host(&saved_remote()).sent();
    let request = loads_of(&sent, "a")
        .pop()
        .expect("opening the page loads its chat");
    h.host(&saved_remote())
        .send(chat_reply("a", request, entries));
    h
}

#[gpui::test]
fn a_tap_on_a_tail_button_with_the_keyboard_up_opens_its_box_and_lowers_the_keyboard(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let mut h = chat_loaded_tailing(cx, &dir, vec![tailing_tool("t1")]);
    draw_insets(&mut h, Some(keyboard_insets(px(336.0))));
    assert!(drawn(&mut h, "chat-tail-t1-0"), "the card offers its file");
    clear_keyboard_calls();

    h.tap_on("chat-tail-t1-0");

    assert!(
        drawn(&mut h, "chat-tail-box-t1-0"),
        "the box opens under the card, whose handler stops the click"
    );
    assert_eq!(
        last_keyboard_call(),
        Some(KeyboardCall::Lower),
        "and the keyboard goes away whatever the card's button stopped"
    );
}

/// Where the phone's chat maps window point `at` to, as (entry, unit,
/// byte), once the frames in flight have drawn.
fn hit(h: &mut Harness<'_>, at: Point<Pixels>) -> Option<(String, u32, usize)> {
    h.cx.run_until_parked();
    h.root(|root, _| root.phone_chat_hit(at))
}

#[gpui::test]
fn a_hit_test_across_two_rows_returns_the_expected_positions(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir, "a");
    let anywhere = point(px(100.0), px(200.0));
    assert_eq!(hit(&mut h, anywhere), None, "no chat text is drawn yet");
    let sent = h.host(&saved_remote()).sent();
    let request = loads_of(&sent, "a")
        .pop()
        .expect("opening the page loads its chat");
    h.host(&saved_remote())
        .send(chat_reply("a", request, vec![user("u1"), answer("a1")]));
    let bubble = h.bounds("chat-row-phone-u1");
    let reply = h.bounds("chat-row-phone-a1");
    let at = |entry: &str, byte: usize| Some((entry.to_owned(), 0, byte));
    // The bubble's text ends at its 10 px padding, at the row's right edge.
    let bubble_line = bubble.top() + px(12.0);
    assert_eq!(
        hit(&mut h, point(bubble.right() - px(12.0), bubble_line)),
        at("u1", 9),
        "on the bubble's last letter"
    );
    assert_eq!(
        hit(&mut h, point(bubble.right() - px(3.0), bubble_line)),
        at("u1", 10),
        "in the bubble right of its text"
    );
    let left = reply.left() + px(1.0);
    assert_eq!(
        hit(&mut h, point(left, reply.top() + px(4.0))),
        at("a1", 0),
        "on the answer's first letter"
    );
    assert_eq!(
        hit(
            &mut h,
            point(reply.left() + px(40.0), reply.top() - px(2.0))
        ),
        at("a1", 0),
        "the gap between the rows, nearer the answer's top"
    );
    assert_eq!(
        hit(&mut h, point(left, bubble.top() - px(4.0))),
        at("u1", 0),
        "above the first text"
    );
    assert_eq!(
        hit(&mut h, point(left, reply.bottom() + px(4.0))),
        at("a1", "answer a1".len()),
        "below the last text"
    );
}

/// Where the body cell "42" of a two-column, two-row table drawn as answer
/// `a1` sits: its text line's y, and the left and right edges of the
/// second column's text room (inside the row's 1 px border and the cell's
/// 8 px padding).
fn second_cell(h: &mut Harness<'_>) -> (Pixels, Pixels, Pixels) {
    let reply = h.bounds("chat-row-phone-a1");
    // The body row is the lower of two equal rows, whatever gap follows.
    let line = reply.top() + reply.size.height * 0.75;
    let inner_left = reply.left() + px(1.0);
    let inner_width = reply.size.width - px(2.0);
    let pad_x = px(8.0);
    let left = inner_left + inner_width / 2.0 + pad_x;
    let right = inner_left + inner_width - pad_x;
    (line, left, right)
}

#[gpui::test]
fn a_tap_on_a_right_aligned_cell_hits_its_own_bytes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let table = "| k | n |\n| --- | ---: |\n| a | 42 |\n";
    let mut h = chat_loaded(cx, &dir, vec![answer_with("a1", table)]);
    let (line, left, right) = second_cell(&mut h);

    assert_eq!(
        hit(&mut h, point(right - px(2.0), line)),
        Some(pos("a1", 3, 1)),
        "over the 2, drawn at the cell's right edge"
    );
    let mut x = right - px(2.0);
    while x > left && hit(&mut h, point(x, line)) != Some(pos("a1", 3, 0)) {
        x -= px(1.0);
    }
    assert!(
        right - x < px(24.0),
        "the 4 is one glyph left of the 2, not at the cell's left edge"
    );
    assert_eq!(
        hit(&mut h, point(left + px(2.0), line)),
        Some(pos("a1", 3, 0)),
        "the cell's empty left part is before its first byte"
    );
}

#[gpui::test]
fn a_tap_on_a_centred_cell_hits_its_own_bytes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let table = "| k | n |\n| --- | :---: |\n| a | 42 |\n";
    let mut h = chat_loaded(cx, &dir, vec![answer_with("a1", table)]);
    let (line, left, right) = second_cell(&mut h);
    let middle = (left + right) / 2.0;

    assert_eq!(
        hit(&mut h, point(middle - px(2.0), line)),
        Some(pos("a1", 3, 0)),
        "over the 4, left of the cell's middle"
    );
    assert_eq!(
        hit(&mut h, point(middle + px(2.0), line)),
        Some(pos("a1", 3, 1)),
        "over the 2, right of the cell's middle"
    );
    assert_eq!(
        hit(&mut h, point(left + px(2.0), line)),
        Some(pos("a1", 3, 0)),
        "the cell's empty left part is before its first byte"
    );
}

/// An answer `id` whose text is `text`.
fn answer_with(id: &str, text: &str) -> ChatEntry {
    ChatEntry::Answer {
        id: id.to_owned(),
        text: text.to_owned(),
        live: false,
    }
}

/// A position as the selection hooks take it.
type Pos = (String, u32, usize);

fn pos(entry: &str, unit: u32, byte: usize) -> Pos {
    (entry.to_owned(), unit, byte)
}

/// Seeds session `session_id`'s chat selection from `anchor` to `focus`.
fn select(h: &mut Harness<'_>, session_id: &str, anchor: Pos, focus: Pos) {
    let id = session_id.to_owned();
    h.root_update(move |root, _, cx| root.set_phone_chat_selection(&id, anchor, focus, cx));
}

/// The (entry, unit, bytes) the phone chat's latest frame drew selected.
fn painted_selection(h: &mut Harness<'_>) -> Vec<(String, u32, std::ops::Range<usize>)> {
    h.cx.run_until_parked();
    h.root(|root, _| root.phone_chat_painted_selection())
}

#[gpui::test]
fn a_seeded_selection_draws_its_highlight(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded(cx, &dir, vec![user("u1"), answer("a1")]);
    let rows = ["chat-row-phone-u1", "chat-row-phone-a1"];
    let before: Vec<_> = rows.iter().map(|row| h.bounds(row)).collect();
    assert!(
        painted_selection(&mut h).is_empty(),
        "no selection, no highlight"
    );

    select(&mut h, "a", pos("u1", 0, 3), pos("a1", 0, 4));

    assert_eq!(
        painted_selection(&mut h),
        vec![("u1".to_owned(), 0, 3..10), ("a1".to_owned(), 0, 0..4)],
        "the user text to its end, the answer up to the byte"
    );
    let after: Vec<_> = rows.iter().map(|row| h.bounds(row)).collect();
    assert_eq!(after, before, "the highlight moves and resizes no row");
}

#[gpui::test]
fn a_selection_inside_a_table_cell_highlights_only_that_cell(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let table = "| c1 | c2 |\n| --- | --- |\n| v1 | v2 |\n";
    let mut h = chat_loaded(cx, &dir, vec![answer_with("a1", table)]);

    select(&mut h, "a", pos("a1", 1, 0), pos("a1", 1, 2));

    assert_eq!(painted_selection(&mut h), vec![("a1".to_owned(), 1, 0..2)]);
}

#[gpui::test]
fn a_selection_in_a_code_line_highlights_only_that_line(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let code = "```\nfn a() {}\nlet b = 1;\n```";
    let mut h = chat_loaded(cx, &dir, vec![answer_with("a1", code)]);

    select(&mut h, "a", pos("a1", 1, 4), pos("a1", 1, 5));

    assert_eq!(painted_selection(&mut h), vec![("a1".to_owned(), 1, 4..5)]);
}

#[gpui::test]
fn desktop_chat_draws_no_selection(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    h.answer_scrollback("s1", b"");
    h.sent();
    h.click_on("pane-view-chat-p1");
    let request = loads_of(&h.sent(), "s1")
        .pop()
        .expect("the toggle asks for the chat");
    h.send(chat_reply("s1", request, vec![user("u1"), answer("a1")]));
    assert!(
        drawn(&mut h, "chat-row-p1-u1"),
        "the desktop draws the chat"
    );

    select(&mut h, "s1", pos("u1", 0, 0), pos("a1", 0, 4));

    assert!(
        painted_selection(&mut h).is_empty(),
        "nothing drawn selected"
    );
    let on_row = h.center("chat-row-p1-a1");
    assert_eq!(hit(&mut h, on_row), None, "and no text registered");
}

#[gpui::test]
fn a_seed_past_a_shortened_text_is_clamped(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded(cx, &dir, vec![user("u1"), answer("a1")]);
    select(&mut h, "a", pos("u1", 0, 2), pos("a1", 7, 0));
    assert_eq!(
        painted_selection(&mut h),
        vec![("u1".to_owned(), 0, 2..10), ("a1".to_owned(), 0, 0..9)],
        "a unit past the answer's last is its last unit's end"
    );
    let short = ChatEntry::User {
        id: "u1".to_owned(),
        text: "msg".to_owned(),
        pending: false,
    };

    h.host(&saved_remote())
        .send(chat_reply("a", None, vec![short, answer("a1")]));

    assert_eq!(
        painted_selection(&mut h),
        vec![("u1".to_owned(), 0, 2..3), ("a1".to_owned(), 0, 0..9)],
        "an end past the shortened text is its end"
    );
}

#[gpui::test]
fn the_selected_text_hook_copies_the_seeded_range(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded(cx, &dir, vec![user("u1"), answer("a1")]);

    select(&mut h, "a", pos("u1", 0, 8), pos("a1", 0, 6));

    assert_eq!(
        h.root(|root, _| root.phone_chat_selected_text("a")),
        "u1\n\nanswer"
    );
    h.root_update(|root, _, cx| root.clear_phone_chat_selection("a", cx));
    assert!(painted_selection(&mut h).is_empty(), "a cleared selection");
    assert_eq!(h.root(|root, _| root.phone_chat_selected_text("a")), "");
}

// ── Selecting with a hold ──────────────────────────────────────────────────

/// The first point, from the left edge of what `row` tags at its middle
/// height, that the phone's chat maps to a byte in `bytes` of entry
/// `entry`'s first unit.
fn on_bytes(
    h: &mut Harness<'_>,
    row: &str,
    entry: &str,
    bytes: std::ops::Range<usize>,
) -> Point<Pixels> {
    let bounds = h.bounds(row);
    let y = bounds.top() + bounds.size.height / 2.0;
    let mut x = bounds.left();
    let mut found = None;
    while found.is_none() && x < bounds.right() {
        let at = point(x, y);
        if let Some((hit_entry, 0, byte)) = hit(h, at)
            && hit_entry == entry
            && bytes.contains(&byte)
        {
            found = Some(at);
        }
        x += px(1.0);
    }
    found.expect("the row draws those bytes")
}

/// The text session `a`'s phone chat selection copies.
fn selected_text(h: &mut Harness<'_>) -> String {
    h.cx.run_until_parked();
    h.root(|root, _| root.phone_chat_selected_text("a"))
}

#[gpui::test]
fn a_long_press_on_chat_text_selects_the_word(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded(cx, &dir, vec![answer_with("a1", "alpha bravo charlie")]);
    let at = on_bytes(&mut h, "chat-row-phone-a1", "a1", 7..10);

    h.long_press_at(at);

    assert_eq!(selected_text(&mut h), "bravo");
    assert_eq!(
        painted_selection(&mut h),
        vec![("a1".to_owned(), 0, 6..11)],
        "the word's bytes are drawn selected"
    );
}

#[gpui::test]
fn a_long_press_on_a_cards_one_liner_selects_without_toggling_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded(cx, &dir, vec![tailing_tool("t1")]);
    let closed = h.bounds("chat-row-phone-t1");
    // Its one-liner is "cargo build > build.log": over the "log".
    let at = on_bytes(&mut h, "chat-row-phone-t1", "t1", 21..23);

    h.long_press_at(at);

    assert_eq!(selected_text(&mut h), "log");
    assert_eq!(
        h.bounds("chat-row-phone-t1"),
        closed,
        "the card stays closed"
    );
    h.touch(TouchPhase::Started, at);
    h.touch(TouchPhase::Ended, at);
    assert!(
        h.bounds("chat-row-phone-t1").size.height > closed.size.height,
        "where a tap opens it"
    );
}

#[gpui::test]
fn a_long_press_on_a_button_selects_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded_tailing(cx, &dir, vec![tailing_tool("t1")]);
    assert!(drawn(&mut h, "chat-tail-t1-0"), "the card offers its file");
    select(&mut h, "a", pos("t1", 0, 0), pos("t1", 0, 5));
    assert_eq!(selected_text(&mut h), "cargo");

    h.long_press_on("chat-tail-t1-0");

    assert_eq!(
        selected_text(&mut h),
        "cargo",
        "a hold on a button keeps the selection"
    );
    assert!(
        !drawn(&mut h, "chat-tail-box-t1-0"),
        "and does not press the button"
    );
    let card = h.bounds("chat-row-phone-t1");
    let below = point(card.left() + px(20.0), card.bottom() + px(40.0));
    assert!(
        h.bounds("phone-chat").contains(&below),
        "the chat has room below its rows"
    );

    h.long_press_at(below);

    assert_eq!(
        selected_text(&mut h),
        "cargo",
        "nor does a hold on the empty space below the rows"
    );
}

#[gpui::test]
fn a_tap_still_opens_a_stretch(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded(cx, &dir, vec![stretched("a1")]);
    let links = stretch_links(&mut h);
    let [(_, key)]: [(String, String); 1] = links
        .clone()
        .try_into()
        .expect("the answer draws one closed stretch");
    let answer = h.bounds("chat-row-phone-a1");
    let at = point(
        answer.origin.x + px(30.0),
        answer.origin.y + answer.size.height / 2.0,
    );

    // The hold goes on a letter of the short form, right of the tap's point.
    let drawn_text = h
        .root(|root, _| root.chat_answer_text("a", "a1"))
        .expect("the answer is drawn");
    let mut on_letter = at;
    while let Some((_, 0, byte)) = hit(&mut h, on_letter)
        && !drawn_text[byte..].starts_with(char::is_alphabetic)
        && on_letter.x < answer.right()
    {
        on_letter.x += px(1.0);
    }

    h.long_press_at(on_letter);

    assert_eq!(stretch_links(&mut h), links, "a hold opens nothing");
    let word = selected_text(&mut h);
    assert!(
        "tap here to open the long form and read it all"
            .split(' ')
            .any(|part| part == word),
        "it selects a word of the short form, not {word:?}"
    );

    h.touch(TouchPhase::Started, at);
    h.touch(TouchPhase::Ended, at);

    assert_eq!(
        stretch_links(&mut h),
        [("\u{2039}".to_owned(), key)],
        "a tap after the hold still opens the stretch"
    );
}

#[gpui::test]
fn a_hold_drag_widens_the_selection_across_rows(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let second = "delta echo foxtrot";
    let entries = vec![answer_with("a1", "alpha bravo"), answer_with("a2", second)];
    let mut h = chat_loaded(cx, &dir, entries);
    let from = on_bytes(&mut h, "chat-row-phone-a1", "a1", 7..10);
    let to = on_bytes(&mut h, "chat-row-phone-a2", "a2", 7..9);
    let (_, _, byte) = hit(&mut h, to).expect("the finger ends on a2");

    h.long_press_drag(from, to);

    assert_eq!(
        selected_text(&mut h),
        format!("bravo\n\n{}", &second[..byte]),
        "from the held word's start to the finger"
    );
}

#[gpui::test]
fn a_hold_dragged_back_keeps_the_first_word(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let text = "alpha bravo charlie";
    let mut h = chat_loaded(cx, &dir, vec![answer_with("a1", text)]);
    let from = on_bytes(&mut h, "chat-row-phone-a1", "a1", 13..18);
    let to = on_bytes(&mut h, "chat-row-phone-a1", "a1", 1..4);
    let (_, _, byte) = hit(&mut h, to).expect("the finger ends on a1");

    h.long_press_drag(from, to);

    assert_eq!(
        selected_text(&mut h),
        &text[byte..],
        "from the finger to the held word's end"
    );
}

#[gpui::test]
fn a_hold_with_a_small_move_keeps_the_word(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded(cx, &dir, vec![answer_with("a1", "alpha bravo charlie")]);
    let at = on_bytes(&mut h, "chat-row-phone-a1", "a1", 7..10);

    h.long_press_drag(at, point(at.x + px(2.0), at.y));

    assert_eq!(
        selected_text(&mut h),
        "bravo",
        "a move inside the held word keeps the whole word"
    );
}

// ── Dragging the selection's handles ───────────────────────────────────────

/// The window points the phone chat's start and end selection handles aim
/// at, once the frames in flight have drawn.
fn handle_aims(h: &mut Harness<'_>) -> (Point<Pixels>, Point<Pixels>) {
    h.cx.run_until_parked();
    h.root(|root, _| root.phone_chat_handle_aims())
        .expect("both handles are drawn")
}

/// Drags the selection handle tagged `phone-chat-handle-{end}` from its
/// knob by what puts its aim on window point `to`, and lifts it there.
fn drag_handle(h: &mut Harness<'_>, end: &str, to: Point<Pixels>) {
    let (start_aim, end_aim) = handle_aims(h);
    let aim = if end == "start" { start_aim } else { end_aim };
    let from = h.center(&format!("phone-chat-handle-{end}"));
    let finger = to - (aim - from);
    h.touch(TouchPhase::Started, from);
    h.touch(TouchPhase::Moved, finger);
    h.touch(TouchPhase::Ended, finger);
}

#[gpui::test]
fn a_selection_shows_a_handle_at_each_end(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded(cx, &dir, vec![answer_with("a1", "alpha bravo")]);
    assert!(
        !drawn(&mut h, "phone-chat-handle-start"),
        "no selection, no start handle"
    );
    assert!(
        !drawn(&mut h, "phone-chat-handle-end"),
        "no selection, no end handle"
    );

    select(&mut h, "a", pos("a1", 0, 6), pos("a1", 0, 11));

    assert!(
        drawn(&mut h, "phone-chat-handle-start"),
        "the start's handle"
    );
    assert!(drawn(&mut h, "phone-chat-handle-end"), "the end's handle");
    let (start, end) = handle_aims(&mut h);
    assert_eq!(
        hit(&mut h, start),
        Some(pos("a1", 0, 6)),
        "the start handle aims at the selection's start"
    );
    assert_eq!(
        hit(&mut h, end),
        Some(pos("a1", 0, 11)),
        "the end handle aims at the selection's end"
    );
}

#[gpui::test]
fn a_tap_on_a_handle_reaches_the_card_under_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded(cx, &dir, vec![tailing_tool("t1")]);
    let closed = h.bounds("chat-row-phone-t1");
    // Its one-liner is "cargo build > build.log": the "log".
    select(&mut h, "a", pos("t1", 0, 21), pos("t1", 0, 24));
    let (start, _) = handle_aims(&mut h);
    assert!(
        h.bounds("phone-chat-handle-start").contains(&start),
        "the tap lands on the start handle's hit area"
    );
    // A finger's jitter inside the tap slop leaves it a tap.
    let jittered = point(start.x, start.y + px(2.0));

    h.touch(TouchPhase::Started, start);
    h.touch(TouchPhase::Moved, jittered);
    h.touch(TouchPhase::Ended, jittered);

    assert!(
        h.bounds("chat-row-phone-t1").size.height > closed.size.height,
        "the tap opens the card under the handle"
    );
}

#[gpui::test]
fn a_tap_on_a_handle_that_jitters_keeps_the_selection(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded(cx, &dir, vec![answer_with("a1", "alpha bravo charlie")]);
    select(&mut h, "a", pos("a1", 0, 6), pos("a1", 0, 11));
    let start = h.center("phone-chat-handle-start");
    let jittered = point(start.x, start.y + px(2.0));

    h.touch(TouchPhase::Started, start);
    h.touch(TouchPhase::Moved, jittered);
    h.touch(TouchPhase::Ended, jittered);

    assert_eq!(
        selected_text(&mut h),
        "bravo",
        "a move inside the tap slop moves no end"
    );
}

#[gpui::test]
fn a_touch_under_the_rows_is_not_a_handles(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut entries: Vec<ChatEntry> = (0..40).map(|n| user(&format!("u{n}"))).collect();
    entries.push(answer_with("a1", "alpha bravo"));
    let mut h = chat_loaded(cx, &dir, entries);
    select(&mut h, "a", pos("a1", 0, 6), pos("a1", 0, 11));
    assert!(drawn(&mut h, "phone-chat-handle-end"), "the end's handle");
    let square = h.bounds("phone-chat-handle-end");
    let rows = h.bounds("phone-chat");
    let at = point(square.center().x, rows.bottom() + px(2.0));
    assert!(
        square.contains(&at),
        "the end handle's square would reach under the rows: {square:?} over {rows:?}"
    );

    h.touch(TouchPhase::Started, at);
    h.touch(TouchPhase::Moved, point(at.x, at.y - px(30.0)));
    h.touch(TouchPhase::Ended, point(at.x, at.y - px(30.0)));

    assert_eq!(
        selected_text(&mut h),
        "bravo",
        "a touch outside the rows is never a handle's"
    );
}

#[gpui::test]
fn dragging_the_end_handle_widens_to_the_next_row(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let second = "delta echo foxtrot";
    let entries = vec![answer_with("a1", "alpha bravo"), answer_with("a2", second)];
    let mut h = chat_loaded(cx, &dir, entries);
    select(&mut h, "a", pos("a1", 0, 6), pos("a1", 0, 11));
    let to = on_bytes(&mut h, "chat-row-phone-a2", "a2", 7..9);
    let (_, _, byte) = hit(&mut h, to).expect("the handle aims at a2");

    drag_handle(&mut h, "end", to);

    assert_eq!(
        selected_text(&mut h),
        format!("bravo\n\n{}", &second[..byte]),
        "from the start that stayed to where the end handle aims"
    );
}

#[gpui::test]
fn dragging_the_end_handle_past_the_start_swaps_the_ends(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let text = "alpha bravo charlie";
    let mut h = chat_loaded(cx, &dir, vec![answer_with("a1", text)]);
    select(&mut h, "a", pos("a1", 0, 12), pos("a1", 0, 19));
    let to = on_bytes(&mut h, "chat-row-phone-a1", "a1", 1..4);
    let (_, _, byte) = hit(&mut h, to).expect("the handle aims at a1");

    drag_handle(&mut h, "end", to);

    assert_eq!(
        selected_text(&mut h),
        &text[byte..12],
        "from where the end handle aims to the old start"
    );
}

#[gpui::test]
fn dragging_the_start_handle_moves_only_the_start(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let text = "alpha bravo charlie";
    let mut h = chat_loaded(cx, &dir, vec![answer_with("a1", text)]);
    select(&mut h, "a", pos("a1", 0, 6), pos("a1", 0, 11));
    let to = on_bytes(&mut h, "chat-row-phone-a1", "a1", 1..4);
    let (_, _, byte) = hit(&mut h, to).expect("the handle aims at a1");

    drag_handle(&mut h, "start", to);

    assert_eq!(
        selected_text(&mut h),
        &text[byte..11],
        "from where the start handle aims to the end that stayed"
    );
}

#[gpui::test]
fn a_pan_off_the_handles_scrolls_and_keeps_the_selection(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut entries: Vec<ChatEntry> = (0..40).map(|n| user(&format!("u{n}"))).collect();
    entries.push(answer_with("a1", "alpha bravo"));
    let mut h = chat_loaded(cx, &dir, entries);
    select(&mut h, "a", pos("a1", 0, 6), pos("a1", 0, 11));
    assert!(drawn(&mut h, "phone-chat-handle-end"), "the end's handle");
    let before = chat_offset(&mut h);
    let at = h.bounds("chat-row-phone-u37").center();

    pan_at(&mut h, at, px(0.0), TouchPhase::Started);
    pan_at(&mut h, at, px(100.0), TouchPhase::Moved);
    pan_at(&mut h, at, px(0.0), TouchPhase::Ended);

    assert_ne!(chat_offset(&mut h), before, "the pan scrolls the chat");
    assert_eq!(selected_text(&mut h), "bravo", "the selection stays");
}

#[gpui::test]
fn a_hold_beside_a_wrapped_line_selects_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    // Long words, of uneven lengths, so the first line ends well short of
    // the row's right edge.
    let words: Vec<String> = ('a'..='t')
        .zip([23, 31, 27, 35, 25].into_iter().cycle())
        .map(|(letter, len)| letter.to_string().repeat(len))
        .collect();
    let text = words.join(" ");
    let text = text.as_str();
    let mut h = chat_loaded(cx, &dir, vec![answer_with("a1", text)]);
    let reply = h.bounds("chat-row-phone-a1");
    let line = reply.top() + px(4.0);
    let edge = reply.right() - px(1.0);
    let (_, _, wrap) = hit(&mut h, point(edge, line)).expect("the first line is hit");
    assert!(wrap > 0 && wrap < text.len(), "the answer wraps at {wrap}");
    // Leftwards from the edge, the blank keeps the wrap's byte until the
    // line's last glyph.
    let mut x = edge;
    while x > reply.left() && hit(&mut h, point(x, line)).is_some_and(|(_, _, byte)| byte == wrap) {
        x -= px(1.0);
    }
    assert!(edge - x >= px(4.0), "the first line ends short of the edge");
    let blank = point((x + edge) / 2.0, line);

    h.long_press_at(blank);

    assert_eq!(
        selected_text(&mut h),
        "",
        "no glyph under the finger, no word"
    );
}

#[gpui::test]
fn a_hold_left_of_a_right_aligned_cell_selects_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let table = "| k | n |\n| --- | ---: |\n| a | 42 |\n";
    let mut h = chat_loaded(cx, &dir, vec![answer_with("a1", table)]);
    let (line, left, _) = second_cell(&mut h);

    h.long_press_at(point(left + px(2.0), line));

    assert_eq!(
        selected_text(&mut h),
        "",
        "the cell's blank left part has no glyph"
    );
}

#[gpui::test]
fn a_hold_on_the_work_strip_shows_its_tooltip(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded(cx, &dir, vec![user("u1")]);
    let counted = WorkShell {
        id: String::new(),
        description: String::new(),
        command: None,
        since: None,
        from_screen: true,
    };
    h.host(&saved_remote()).send(DaemonMessage::ChatWork {
        session_id: "a".to_owned(),
        subagents: Vec::new(),
        shells: vec![counted],
        wakeup_at: None,
    });
    assert!(drawn(&mut h, "work-item-0"), "the strip shows the shell");
    let tip = "tooltip=Counted from the screen; no details reported yet";
    let at = h.center("work-item-0");

    h.touch(TouchPhase::Started, at);
    h.advance(
        gpui::GestureTuning::default().long_press_duration + std::time::Duration::from_millis(1),
    );

    assert!(drawn(&mut h, tip), "the held segment shows its tooltip");
    h.touch(TouchPhase::Ended, at);
    assert_eq!(selected_text(&mut h), "", "and the chat selects nothing");
}

#[gpui::test]
fn the_chat_registry_is_forgotten_off_the_chat_page(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded(cx, &dir, vec![user("u1"), answer("a1")]);
    let on_text = on_bytes(&mut h, "chat-row-phone-a1", "a1", 1..5);

    h.click_on("phone-session-toggle");

    assert!(drawn(&mut h, "phone-session-terminal"));
    assert_eq!(
        hit(&mut h, on_text),
        None,
        "no text is hit once the chat is not drawn"
    );
}

/// Call `entry` with its `parent_id` set to `parent`, and, when `facts` is
/// given, made a Code Mode script call carrying them.
fn scripted(entry: ChatEntry, facts: Option<ScriptFacts>, parent: Option<&str>) -> ChatEntry {
    let ChatEntry::Tool {
        id,
        name,
        summary,
        input,
        result,
        tail_paths,
        kind,
        ..
    } = entry
    else {
        return entry;
    };
    ChatEntry::Tool {
        id,
        name,
        summary,
        input,
        result,
        parent_id: parent.map(str::to_owned),
        tail_paths,
        kind: if facts.is_some() {
            ToolKind::Script
        } else {
            kind
        },
        script: facts,
    }
}

/// A user row, a done script `s1` with a long description and four inner
/// calls of four tools (so four chips), and a failed script `s2` with a
/// long error.
fn wide_scripts() -> Vec<ChatEntry> {
    let description = "Read the footer, its spec and the pill gap rule, then grep every \
                       caller of pill_gap and fetch the design note it cites";
    let error = "TypeError: undefined is not an object (evaluating 'footer.pills[3].width') \
                 at sumPillWidths (repl:4:21) while measuring the footer's pill row widths";
    let mut failed = scripted(
        call("s2", "REPL", ToolKind::Script),
        Some(ScriptFacts {
            description: "Sum the pill widths".to_owned(),
            eval: Some(4),
            ms: Some(300),
            error: Some(error.to_owned()),
        }),
        None,
    );
    if let ChatEntry::Tool {
        result: Some(outcome),
        ..
    } = &mut failed
    {
        outcome.is_error = true;
    }
    vec![
        user("u1"),
        scripted(
            call("s1", "REPL", ToolKind::Script),
            Some(ScriptFacts {
                description: description.to_owned(),
                eval: Some(3),
                ms: Some(1200),
                error: None,
            }),
            None,
        ),
        scripted(call("i1", "Read", ToolKind::Read), None, Some("s1")),
        scripted(call("i2", "Grep", ToolKind::Search), None, Some("s1")),
        scripted(call("i3", "Bash", ToolKind::Run), None, Some("s1")),
        scripted(call("i4", "WebFetch", ToolKind::Other), None, Some("s1")),
        failed,
    ]
}

#[gpui::test]
fn a_script_draws_as_one_wrapping_line_on_the_phone(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded(cx, &dir, wide_scripts());
    // A phone's screen.
    h.cx.simulate_resize(gpui::size(px(390.0), px(874.0)));

    let scripts: Vec<String> = phone_rows(&mut h, "a")
        .into_iter()
        .filter_map(|row| match row {
            RowShape::Script { id, .. } => Some(id),
            _ => None,
        })
        .collect();
    assert_eq!(scripts, ["s1", "s2"], "each script is one row");
    assert!(drawn(&mut h, "chat-script-phone-s1"), "s1's line");
    assert!(drawn(&mut h, "chat-script-phone-s2"), "s2's line");
    let right = h.bounds("phone-chat").right() + px(1.0);
    let mut parts: Vec<String> = (0..4)
        .map(|n| format!("chat-script-chip-phone-s1-{n}"))
        .collect();
    parts.push("chat-script-meta-phone-s1".to_owned());
    for part in &parts {
        assert!(drawn(&mut h, part), "{part} is drawn");
        assert!(
            h.bounds(part).right() <= right,
            "{part} stays within the phone's chat"
        );
    }
    let (first, last) = (
        h.bounds("chat-script-chip-phone-s1-0"),
        h.bounds("chat-script-chip-phone-s1-3"),
    );
    assert!(
        last.top() > first.top(),
        "the chips wrap onto another line at the phone's width: line {:?}, meta {:?}, \
         first chip {first:?}, last chip {last:?}",
        h.bounds("chat-script-phone-s1"),
        h.bounds("chat-script-meta-phone-s1"),
    );
    let error = h.bounds("chat-script-error-phone-s2");
    assert!(
        error.size.height > h.bounds("chat-script-phone-s2").size.height,
        "the error wraps over more than one line"
    );
    assert!(error.right() <= right, "the error stays within the chat");
}

// ── Copying and ending the selection ───────────────────────────────────────

/// The copied chip's driver id, its label (`copied::CHIP_LABEL`).
const COPIED_CHIP: &str = "\u{2713} copied";

/// Session `a`'s page on a phone-sized window, its chat's load answered
/// with `entries`.
fn chat_loaded_on_a_phone<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    entries: Vec<ChatEntry>,
) -> Harness<'a> {
    let h = chat_loaded(cx, dir, entries);
    h.cx.simulate_resize(gpui::size(px(390.0), px(874.0)));
    h.cx.run_until_parked();
    h
}

#[gpui::test]
fn copy_puts_the_selection_on_the_clipboard_and_clears_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut entries: Vec<ChatEntry> = (0..4).map(|n| user(&format!("u{n}"))).collect();
    entries.push(answer_with("a1", "alpha bravo charlie"));
    let mut h = chat_loaded_on_a_phone(cx, &dir, entries);
    select(&mut h, "a", pos("a1", 0, 6), pos("a1", 0, 11));
    h.cx.run_until_parked();
    assert!(
        drawn(&mut h, "phone-chat-copy"),
        "Copy shows with the handles"
    );
    assert!(!drawn(&mut h, COPIED_CHIP), "no chip before the copy");
    let copy = h.bounds("phone-chat-copy");
    let start = h.bounds("phone-chat-handle-start");
    assert!(
        copy.bottom() <= start.top(),
        "Copy sits above the start handle: {copy:?} over {start:?}"
    );
    assert!(copy.size.height >= touch_target(), "a touch target tall");

    h.tap_on("phone-chat-copy");
    h.cx.run_until_parked();

    assert_eq!(h.clipboard().as_deref(), Some("bravo"));
    assert!(drawn(&mut h, COPIED_CHIP), "the copied chip shows");
    assert_eq!(
        h.root(|root, _| root.copied_chip()),
        Some(5),
        "and reports the copy"
    );
    assert_eq!(selected_text(&mut h), "", "the copy ends the selection");
    for gone in [
        "phone-chat-handle-start",
        "phone-chat-handle-end",
        "phone-chat-copy",
    ] {
        assert!(!drawn(&mut h, gone), "{gone} is gone with the selection");
    }
}

#[gpui::test]
fn copy_sits_below_the_end_when_the_start_is_at_the_top(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded_on_a_phone(cx, &dir, vec![answer_with("a1", "alpha bravo charlie")]);
    let rows = h.bounds("phone-chat");
    assert!(
        h.bounds("chat-row-phone-a1").top() < rows.top() + px(40.0),
        "the answer is the first row, at the top of the rows"
    );

    select(&mut h, "a", pos("a1", 0, 6), pos("a1", 0, 11));
    h.cx.run_until_parked();

    assert!(drawn(&mut h, "phone-chat-copy"), "Copy shows");
    let copy = h.bounds("phone-chat-copy");
    let end = h.bounds("phone-chat-handle-end");
    assert!(
        copy.top() >= end.bottom(),
        "no room above: Copy sits below the end handle: {copy:?} under {end:?}"
    );
}

#[gpui::test]
fn a_tap_elsewhere_clears_the_selection_and_still_acts(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let entries = vec![
        answer_with("a0", "alpha bravo charlie"),
        stretched("a1"),
        answer_with("a2", "delta echo foxtrot"),
    ];
    let mut h = chat_loaded_on_a_phone(cx, &dir, entries);
    let [(_, key)]: [(String, String); 1] = stretch_links(&mut h)
        .try_into()
        .expect("the answer draws one closed stretch");
    select(&mut h, "a", pos("a0", 0, 6), pos("a0", 0, 11));
    assert_eq!(selected_text(&mut h), "bravo");
    let answer = h.bounds("chat-row-phone-a1");
    let at = point(
        answer.origin.x + px(30.0),
        answer.origin.y + answer.size.height / 2.0,
    );

    h.touch(TouchPhase::Started, at);
    h.touch(TouchPhase::Ended, at);

    assert_eq!(
        stretch_links(&mut h),
        [("\u{2039}".to_owned(), key)],
        "the tap still opens the stretch"
    );
    assert_eq!(selected_text(&mut h), "", "and ends the selection");

    select(&mut h, "a", pos("a0", 0, 6), pos("a0", 0, 11));
    assert_eq!(selected_text(&mut h), "bravo");
    draw_insets(&mut h, Some(keyboard_insets(px(336.0))));
    let plain = on_bytes(&mut h, "chat-row-phone-a2", "a2", 1..4);
    clear_keyboard_calls();

    h.touch(TouchPhase::Started, plain);
    h.touch(TouchPhase::Ended, plain);

    assert_eq!(selected_text(&mut h), "", "a tap on plain text ends it");
    assert_eq!(
        last_keyboard_call(),
        Some(KeyboardCall::Lower),
        "and still puts the keyboard away"
    );
}

#[gpui::test]
fn a_pan_keeps_the_selection(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut entries: Vec<ChatEntry> = (0..40).map(|n| user(&format!("u{n}"))).collect();
    entries.push(answer_with("a1", "alpha bravo"));
    let mut h = chat_loaded(cx, &dir, entries);
    select(&mut h, "a", pos("a1", 0, 6), pos("a1", 0, 11));
    let before = chat_offset(&mut h);
    let at = h.bounds("chat-row-phone-u37").center();

    pan_at(&mut h, at, px(0.0), TouchPhase::Started);
    pan_at(&mut h, at, px(100.0), TouchPhase::Moved);
    pan_at(&mut h, at, px(0.0), TouchPhase::Ended);

    assert_ne!(chat_offset(&mut h), before, "the pan scrolls the chat");
    assert_eq!(selected_text(&mut h), "bravo", "and keeps the selection");
}

#[gpui::test]
fn leaving_the_chat_page_clears_the_selection(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded(cx, &dir, vec![answer_with("a1", "alpha bravo")]);
    select(&mut h, "a", pos("a1", 0, 6), pos("a1", 0, 11));
    assert_eq!(selected_text(&mut h), "bravo");

    h.click_on("phone-session-toggle");
    assert!(drawn(&mut h, "phone-session-terminal"));
    h.click_on("phone-session-toggle");

    assert!(drawn(&mut h, "phone-session-chat"), "back on the Chat page");
    assert_eq!(selected_text(&mut h), "", "the selection ended");
}

#[gpui::test]
fn back_to_home_clears_the_selection(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded(cx, &dir, vec![answer_with("a1", "alpha bravo")]);
    select(&mut h, "a", pos("a1", 0, 6), pos("a1", 0, 11));
    assert_eq!(selected_text(&mut h), "bravo");

    h.click_on("phone-session-back");
    assert_eq!(shown(&mut h), None);
    h.click_on(&row("a"));
    let request = loads_of(&h.host(&saved_remote()).sent(), "a")
        .pop()
        .expect("opening the page again loads its chat");
    h.host(&saved_remote()).send(chat_reply(
        "a",
        request,
        vec![answer_with("a1", "alpha bravo")],
    ));

    assert_eq!(shown(&mut h).as_deref(), Some("a"));
    assert!(drawn(&mut h, "chat-row-phone-a1"), "the chat is back");
    assert_eq!(selected_text(&mut h), "", "the selection ended");
}

#[gpui::test]
fn an_empty_selection_has_no_copy(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_loaded(cx, &dir, vec![answer_with("a1", "alpha bravo charlie")]);
    select(&mut h, "a", pos("a1", 0, 6), pos("a1", 0, 11));
    assert!(drawn(&mut h, "phone-chat-copy"), "Copy shows for \"bravo\"");
    let (start, _) = handle_aims(&mut h);

    drag_handle(&mut h, "end", start);

    assert_eq!(selected_text(&mut h), "", "the end meets the start");
    assert!(drawn(&mut h, "phone-chat-handle-start"), "the handles stay");
    assert!(!drawn(&mut h, "phone-chat-copy"), "but Copy goes");

    h.set_clipboard("kept");
    h.root_update(|root, _, cx| root.phone_copy_selection("a", cx));

    assert_eq!(
        h.clipboard().as_deref(),
        Some("kept"),
        "copying nothing leaves the clipboard as it was"
    );
}

// ── Scrolling while a selection drag nears the chat's edge ─────────────────

/// How long the specs hold a finger in a band: about eighteen ticks.
const HELD: std::time::Duration = std::time::Duration::from_millis(300);

/// Session `a`'s page on a phone-sized window, its chat 40 user rows and
/// then answers, panned up from its bottom so it can scroll either way.
fn scrollable_chat<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut entries: Vec<ChatEntry> = (0..40).map(|n| user(&format!("u{n}"))).collect();
    entries.push(answer_with("a1", "alpha bravo charlie"));
    entries.push(answer_with("a2", "delta echo foxtrot"));
    let mut h = chat_loaded_on_a_phone(cx, dir, entries);
    let at = h.bounds("phone-chat").center();
    pan_at(&mut h, at, px(0.0), TouchPhase::Started);
    pan_at(&mut h, at, px(300.0), TouchPhase::Moved);
    pan_at(&mut h, at, px(0.0), TouchPhase::Ended);
    h
}

/// The driver id of the phone chat's row of entry `id`.
fn row_tag(id: &str) -> String {
    format!("chat-row-phone-{id}")
}

/// The id of the user row whose middle lies nearest the chat's middle.
fn middle_row(h: &mut Harness<'_>) -> String {
    let middle = h.bounds("phone-chat").center().y;
    let mut best: Option<(f32, String)> = None;
    for n in 0..40 {
        let id = format!("u{n}");
        let distance = ((h.bounds(&row_tag(&id)).center().y - middle) / px(1.0)).abs();
        if best.as_ref().is_none_or(|(least, _)| distance < *least) {
            best = Some((distance, id));
        }
    }
    best.expect("the chat draws its user rows").1
}

/// [`scrollable_chat`] with "message" of its middle row selected.
fn selected_in_the_middle<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = scrollable_chat(cx, dir);
    let id = middle_row(&mut h);
    select(&mut h, "a", pos(&id, 0, 0), pos(&id, 0, 7));
    assert_eq!(selected_text(&mut h), "message");
    h
}

/// Puts a finger on the centre of the selection handle tagged
/// `phone-chat-handle-{end}` and moves it `dy` down, past the tap slop, so
/// it drags; the finger stays down. Where it took the handle.
fn grab_handle(h: &mut Harness<'_>, end: &str, dy: Pixels) -> Point<Pixels> {
    h.cx.run_until_parked();
    let from = h.center(&format!("phone-chat-handle-{end}"));
    h.touch(TouchPhase::Started, from);
    h.touch(TouchPhase::Moved, point(from.x, from.y + dy));
    from
}

#[gpui::test]
fn dragging_the_end_handle_into_the_bottom_band_scrolls_and_follows(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = selected_in_the_middle(cx, &dir);
    let before = chat_offset(&mut h).expect("the chat is drawn");
    let from = grab_handle(&mut h, "end", px(12.0));
    let band = point(from.x, h.bounds("phone-chat").bottom() - px(10.0));
    h.touch(TouchPhase::Moved, band);
    let at_band = selected_text(&mut h);

    h.advance(HELD);

    let scrolled = chat_offset(&mut h).expect("the chat is drawn");
    assert!(
        scrolled > before,
        "the chat scrolls toward later rows: {before} to {scrolled}"
    );
    let followed = selected_text(&mut h);
    assert!(
        followed.contains('\n'),
        "the selection runs past its first row: {followed:?}"
    );
    assert!(
        followed.len() > at_band.len(),
        "the end follows the rows scrolling under the finger: {at_band:?} to {followed:?}"
    );

    h.touch(TouchPhase::Ended, band);
    h.advance(HELD);

    assert_eq!(
        chat_offset(&mut h),
        Some(scrolled),
        "the lift stops the scroll"
    );
}

#[gpui::test]
fn dragging_the_start_handle_into_the_top_band_scrolls_up(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = selected_in_the_middle(cx, &dir);
    let before = chat_offset(&mut h).expect("the chat is drawn");
    let from = grab_handle(&mut h, "start", px(-12.0));
    let band = point(from.x, h.bounds("phone-chat").top() + px(10.0));
    h.touch(TouchPhase::Moved, band);
    let at_band = selected_text(&mut h);

    h.advance(HELD);

    let scrolled = chat_offset(&mut h).expect("the chat is drawn");
    assert!(
        scrolled < before,
        "the chat scrolls toward earlier rows: {before} to {scrolled}"
    );
    let followed = selected_text(&mut h);
    assert!(
        followed.ends_with("message") && followed.contains('\n'),
        "the end stays and the start is on an earlier row: {followed:?}"
    );
    assert!(
        followed.len() > at_band.len(),
        "the start follows the rows scrolling under the finger: {at_band:?} to {followed:?}"
    );

    h.touch(TouchPhase::Ended, band);
    h.advance(HELD);

    assert_eq!(
        chat_offset(&mut h),
        Some(scrolled),
        "the lift stops the scroll"
    );
}

#[gpui::test]
fn a_hold_drag_into_the_bottom_band_scrolls(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = scrollable_chat(cx, &dir);
    let id = middle_row(&mut h);
    let at = on_bytes(&mut h, &row_tag(&id), &id, 1..6);
    let before = chat_offset(&mut h).expect("the chat is drawn");
    h.touch(TouchPhase::Started, at);
    h.advance(
        gpui::GestureTuning::default().long_press_duration + std::time::Duration::from_millis(1),
    );
    assert_eq!(selected_text(&mut h), "message", "the hold picks the word");
    let band = point(at.x, h.bounds("phone-chat").bottom() - px(10.0));
    h.touch(TouchPhase::Moved, band);
    let at_band = selected_text(&mut h);

    h.advance(HELD);

    let scrolled = chat_offset(&mut h).expect("the chat is drawn");
    assert!(
        scrolled > before,
        "the chat scrolls toward later rows: {before} to {scrolled}"
    );
    let widened = selected_text(&mut h);
    assert!(
        widened.len() > at_band.len(),
        "the selection widens with the rows scrolling under the finger: {at_band:?} to {widened:?}"
    );

    h.touch(TouchPhase::Ended, band);
    h.advance(HELD);

    assert_eq!(
        chat_offset(&mut h),
        Some(scrolled),
        "the lift stops the scroll"
    );
}

#[gpui::test]
fn a_handle_dragged_out_of_view_keeps_its_drag(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = selected_in_the_middle(cx, &dir);
    let from = grab_handle(&mut h, "end", px(12.0));
    let rows = h.bounds("phone-chat");
    // Under the rows: the end's caret goes past the chat's bottom edge.
    let past = point(from.x, rows.bottom() + px(40.0));
    h.touch(TouchPhase::Moved, past);
    h.advance(std::time::Duration::from_millis(200));
    let far = selected_text(&mut h);

    let back = point(from.x, rows.center().y);
    h.touch(TouchPhase::Moved, back);

    let moved = selected_text(&mut h);
    assert!(
        moved.len() < far.len(),
        "the drag still moves the end once its caret is back: {far:?} to {moved:?}"
    );
    h.touch(TouchPhase::Ended, back);
}

/// How many times the phone's chat body has been built.
fn chat_renders(h: &mut Harness<'_>) -> usize {
    h.cx.run_until_parked();
    h.root(|root, _| root.phone_chat_renders())
}

#[gpui::test]
fn a_handle_held_in_the_band_at_the_chats_end_does_not_redraw(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut entries: Vec<ChatEntry> = (0..40).map(|n| user(&format!("u{n}"))).collect();
    entries.push(answer_with("a1", "alpha bravo charlie"));
    // At its bottom, as it loads: it cannot scroll down any further.
    let mut h = chat_loaded_on_a_phone(cx, &dir, entries);
    let id = middle_row(&mut h);
    select(&mut h, "a", pos(&id, 0, 0), pos(&id, 0, 7));
    let before = chat_offset(&mut h);
    let from = grab_handle(&mut h, "end", px(12.0));
    let band = point(from.x, h.bounds("phone-chat").bottom() - px(10.0));
    h.touch(TouchPhase::Moved, band);
    let renders = chat_renders(&mut h);

    h.advance(HELD);

    let redrawn = chat_renders(&mut h) - renders;
    assert!(
        redrawn <= 2,
        "a chat at its end redraws for no tick: {redrawn} renders"
    );
    assert_eq!(chat_offset(&mut h), before, "nor scrolls");
    h.touch(TouchPhase::Ended, band);
}

#[gpui::test]
fn a_press_on_a_handle_in_the_band_does_not_scroll(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = scrollable_chat(cx, &dir);
    let id = middle_row(&mut h);
    select(&mut h, "a", pos(&id, 0, 0), pos(&id, 0, 7));
    h.cx.run_until_parked();
    // Pan its row down until the end handle sits 20 px over the bottom.
    let rows = h.bounds("phone-chat");
    let knob = h.center("phone-chat-handle-end");
    let at = h.bounds(&row_tag(&id)).center();
    pan_at(&mut h, at, px(0.0), TouchPhase::Started);
    pan_at(
        &mut h,
        at,
        rows.bottom() - px(20.0) - knob.y,
        TouchPhase::Moved,
    );
    pan_at(&mut h, at, px(0.0), TouchPhase::Ended);
    h.cx.run_until_parked();
    let knob = h.center("phone-chat-handle-end");
    assert!(
        knob.y > rows.bottom() - px(48.0) && knob.y < rows.bottom(),
        "the end handle sits in the bottom band: {knob:?} over {rows:?}"
    );
    let before = chat_offset(&mut h);

    h.touch(TouchPhase::Started, knob);
    h.advance(HELD);

    assert_eq!(chat_offset(&mut h), before, "a press holds still");
    assert_eq!(selected_text(&mut h), "message", "and moves no end");
    h.touch(TouchPhase::Ended, knob);
}

#[gpui::test]
fn a_finger_in_the_middle_does_not_scroll(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = selected_in_the_middle(cx, &dir);
    let before = chat_offset(&mut h);
    let from = grab_handle(&mut h, "end", px(12.0));

    h.advance(HELD);

    assert_eq!(chat_offset(&mut h), before, "no scroll away from the edges");
    h.touch(TouchPhase::Ended, point(from.x, from.y + px(12.0)));
}

/// Whether `sent` holds a `WatchDashboard` turning the watch `on`.
fn watch_sent(sent: &[ClientMessage], on: bool) -> bool {
    sent.iter()
        .any(|msg| matches!(msg, ClientMessage::WatchDashboard { on: was } if *was == on))
}

#[gpui::test]
fn the_header_shows_a_hot_context_label_beside_the_status(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir, "a");
    {
        let mut host = h.host(&saved_remote());
        assert!(
            watch_sent(&host.sent(), true),
            "the page's host is watched with no Dashboard tab"
        );
        host.send(facts_message(vec![facts("a").context(91).build()]));
    }

    assert!(drawn(&mut h, "phone-session-context=ctx 91%"));
    assert!(drawn(&mut h, "phone-session-context-hot"), "91% is hot");
    assert!(
        h.bounds("phone-session-context").origin.x > h.bounds("phone-session-status").origin.x,
        "the label sits right of the status word"
    );

    h.click_on("phone-session-back");
    let mut host = h.host(&saved_remote());
    assert!(
        watch_sent(&host.sent(), false),
        "leaving the page ends the watch"
    );
}

#[gpui::test]
fn a_session_without_a_context_figure_draws_no_label(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir, "a");
    h.host(&saved_remote())
        .send(facts_message(vec![facts("a").build()]));

    assert!(drawn(&mut h, "phone-session-status"));
    assert!(!drawn(&mut h, "phone-session-context"));
}
