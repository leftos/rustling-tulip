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
    BackGesturePhase, Edges, Modifiers, Pixels, ScrollDelta, ScrollEvent, ScrollSource,
    TestAppContext, TouchPhase, WindowInsets, point, px,
};
use protocol::error_codes::CHAT_REFUSED;
use protocol::{
    ChatAnswerKind, ChatEntry, ChatOption, ChatPrompt, ChatPromptKind, ChatScreen, ClientMessage,
    ComposerState, DaemonMessage, SessionSnapshot, ToolKind, ToolOutcome,
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
