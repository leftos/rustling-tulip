//! Chat pane specs: a pane header's Chat toggle loads the session's chat
//! from the daemon and draws it in place of the terminal; the daemon's
//! appends, live answers and unasked resets update it; the last chat pane
//! going unloads it; a reconnect loads it again; and a session whose agent
//! offers no chat snaps back to its terminal with a note. The pane menu and
//! Ctrl+Shift+M switch the view too; each session's last view is saved in
//! `native-ui.json` and a new pane on it opens there, a new session on its
//! terminal, and a pop-out keeps its view. The running-work strip under the
//! chat shows the daemon's `ChatWork`, counts down and takes no click.

use crate::support;

use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use gpui::{TestAppContext, px};
use protocol::error_codes::CHAT_REFUSED;
use protocol::{
    ChatAnswerKind, ChatEntry, ChatOption, ChatPrompt, ChatPromptKind, ChatQuestionTab, ChatScreen,
    ClientMessage, ComposerState, DaemonMessage, GridNode, SplitDirection, ToolOutcome, WorkShell,
    WorkSubagent,
};
use rustling_tulip_native::ui_theme::UiTheme;
use rustling_tulip_native::{OpenOutcome, PopoutKey};
use support::{Fixture, Harness, TestDir, pane, repo, session, tab};

const TO_TERMINAL: &str = "pane-view-terminal-p1";
const TO_CHAT: &str = "pane-view-chat-p1";

/// `s1` alone in pane `p1`, its scrollback answered and everything sent so
/// far drained.
fn attached<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = Harness::with(cx, dir, &Fixture::single(session("s1").build()));
    h.answer_scrollback("s1", b"");
    h.sent();
    h
}

/// The request id of the `LoadChat` for `s1` among `sent`.
fn load_request(sent: &[ClientMessage]) -> Option<String> {
    sent.iter().find_map(|msg| match msg {
        ClientMessage::LoadChat {
            session_id,
            request_id,
        } if session_id == "s1" => request_id.clone(),
        _ => None,
    })
}

fn unloads(sent: &[ClientMessage]) -> usize {
    sent.iter()
        .filter(|msg| matches!(msg, ClientMessage::UnloadChat { session_id } if session_id == "s1"))
        .count()
}

/// `p1` switched to its chat, with the request id its `LoadChat` went out
/// under.
fn opened<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> (Harness<'a>, String) {
    let mut h = attached(cx, dir);
    h.click_on(TO_CHAT);
    let id = load_request(&h.sent()).expect("the toggle asks for the chat");
    (h, id)
}

fn screen() -> ChatScreen {
    ChatScreen {
        composer: ComposerState::Ready,
        prompt: None,
    }
}

fn reply(request_id: Option<&str>, entries: Vec<ChatEntry>) -> DaemonMessage {
    DaemonMessage::Chat {
        session_id: "s1".to_owned(),
        entries,
        hidden: 0,
        screen: screen(),
        request_id: request_id.map(str::to_owned),
    }
}

fn append(entries: Vec<ChatEntry>, removed: &[&str]) -> DaemonMessage {
    DaemonMessage::ChatAppend {
        session_id: "s1".to_owned(),
        entries,
        removed: removed.iter().map(|id| (*id).to_owned()).collect(),
    }
}

fn user(id: &str) -> ChatEntry {
    ChatEntry::User {
        id: id.to_owned(),
        text: format!("message {id}"),
        pending: false,
    }
}

fn answer(id: &str, text: &str, live: bool) -> ChatEntry {
    ChatEntry::Answer {
        id: id.to_owned(),
        text: text.to_owned(),
        live,
    }
}

fn tool(id: &str, result: Option<&str>) -> ChatEntry {
    ChatEntry::Tool {
        id: id.to_owned(),
        name: "Read".to_owned(),
        summary: "apps/native/src/chat.rs".to_owned(),
        input: "{\"file_path\": \"apps/native/src/chat.rs\"}".to_owned(),
        result: result.map(|text| ToolOutcome {
            text: text.to_owned(),
            is_error: false,
        }),
        parent_id: None,
    }
}

/// The ids of `s1`'s chat rows, in display order.
fn row_ids(h: &mut Harness<'_>) -> Vec<String> {
    h.root(|root, _| root.chat_rows("s1"))
        .iter()
        .filter_map(|entry| match entry {
            ChatEntry::User { id, .. }
            | ChatEntry::Answer { id, .. }
            | ChatEntry::Thinking { id, .. }
            | ChatEntry::Tool { id, .. }
            | ChatEntry::System { id, .. }
            | ChatEntry::Receipt { id, .. }
            | ChatEntry::TurnEnd { id, .. } => Some(id.clone()),
            ChatEntry::Unknown => None,
        })
        .collect()
}

/// Whether the element `selector` tags has been painted with a size.
fn painted(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).size.height > px(0.0)
}

/// Whether the element `selector` tags is drawn now. gpui keeps an
/// element's last bounds after it stops drawing, so the window is narrowed
/// first: a drawn element is laid out again at the new width, one no longer
/// drawn keeps its old bounds. The window gets its width back after.
fn drawn_now(h: &mut Harness<'_>, selector: &str) -> bool {
    let before = h.bounds(selector);
    if before.size.height <= px(0.0) {
        return false;
    }
    let (width, height) = (h.window_width(), h.window_height());
    h.cx.simulate_resize(gpui::size(px(width - 100.0), px(height)));
    let after = h.bounds(selector);
    h.cx.simulate_resize(gpui::size(px(width), px(height)));
    h.cx.run_until_parked();
    after != before
}

fn shows_chat(h: &mut Harness<'_>) -> bool {
    h.root(|root, _| root.pane_shows_chat("p1"))
}

#[gpui::test]
fn the_toggle_asks_for_the_chat_and_draws_it_in_place_of_the_terminal(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir);
    assert!(!shows_chat(&mut h), "a pane opens on its terminal");
    assert!(painted(&mut h, "pane-grid-p1"));
    h.click_on(TO_CHAT);
    let sent = h.sent();
    assert!(
        load_request(&sent).is_some(),
        "the toggle sends LoadChat: {sent:?}"
    );
    assert!(shows_chat(&mut h));
    assert!(painted(&mut h, "pane-chat-p1"), "the chat body is drawn");
    assert!(
        !drawn_now(&mut h, "pane-grid-p1"),
        "the terminal is not drawn beside it"
    );
}

#[gpui::test]
fn the_reply_draws_its_rows(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    h.send(reply(
        Some(&id),
        vec![
            user("u1"),
            ChatEntry::Thinking {
                id: "k1".to_owned(),
                secs: Some(14),
            },
            tool("t1", None),
            answer("a1", "Done: **two** files.", false),
            ChatEntry::TurnEnd {
                id: "e1".to_owned(),
                duration_ms: Some(12_000),
            },
        ],
    ));
    assert_eq!(row_ids(&mut h), ["u1", "k1", "t1", "a1", "e1"]);
    for row in ["u1", "k1", "t1", "a1", "e1"] {
        assert!(
            painted(&mut h, &format!("chat-row-p1-{row}")),
            "row {row} is drawn"
        );
    }
    assert!(
        painted(&mut h, "chat-tool-running-p1-t1"),
        "a call without a result runs"
    );
}

#[gpui::test]
fn an_append_updates_a_tool_card_in_place(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    h.send(reply(Some(&id), vec![user("u1"), tool("t1", None)]));
    h.send(append(vec![tool("t1", Some("fn main() {}"))], &[]));
    assert_eq!(row_ids(&mut h), ["u1", "t1"], "the card keeps its place");
    assert!(
        painted(&mut h, "chat-tool-done-p1-t1"),
        "the card shows its result"
    );
}

#[gpui::test]
fn a_live_answer_grows_then_gives_way_to_the_transcript_answer(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    h.send(reply(Some(&id), vec![user("u1")]));
    h.send(append(vec![answer("live:m1", "one", true)], &[]));
    let short = h.bounds("chat-live-p1-live:m1").size.height;
    assert!(short > px(0.0), "the live answer is drawn");
    h.send(append(
        vec![answer("live:m1", "one\n\ntwo\n\nthree", true)],
        &[],
    ));
    let tall = h.bounds("chat-live-p1-live:m1").size.height;
    assert!(
        tall > short,
        "the live answer grows: {short:?} then {tall:?}"
    );
    h.send(append(
        vec![answer("a1", "one two three", false)],
        &["live:m1"],
    ));
    assert_eq!(
        row_ids(&mut h),
        ["u1", "a1"],
        "the transcript answer replaces it"
    );
    assert!(painted(&mut h, "chat-row-p1-a1"));
}

#[gpui::test]
fn an_unasked_chat_from_the_daemon_replaces_every_row(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    h.send(reply(Some(&id), vec![user("u1"), user("u2")]));
    h.send(reply(
        None,
        vec![
            ChatEntry::System {
                id: "cleared:c2".to_owned(),
                text: "Conversation cleared".to_owned(),
            },
            user("u3"),
        ],
    ));
    assert_eq!(row_ids(&mut h), ["cleared:c2", "u3"]);
    assert!(painted(&mut h, "chat-row-p1-u3"));
}

#[gpui::test]
fn toggling_back_unloads_the_chat_and_draws_the_terminal(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    h.send(reply(Some(&id), vec![user("u1")]));
    h.click_on(TO_TERMINAL);
    assert_eq!(unloads(&h.sent()), 1, "the last chat pane going unloads");
    assert!(!shows_chat(&mut h));
    assert!(painted(&mut h, "pane-grid-p1"));
    assert!(!drawn_now(&mut h, "pane-chat-p1"), "the chat body is gone");
}

#[gpui::test]
fn closing_the_last_chat_pane_unloads_the_chat(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let split = GridNode::Split {
        direction: SplitDirection::Horizontal,
        ratio: 0.5,
        first: Box::new(pane("p1", Some("s1"))),
        second: Box::new(pane("p2", Some("s1"))),
    };
    let fixture = Fixture {
        sessions: vec![session("s1").build()],
        tabs: vec![tab("t1", &split)],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.answer_scrollback("s1", b"");
    h.sent();
    h.click_on(TO_CHAT);
    assert!(load_request(&h.sent()).is_some());
    h.send(DaemonMessage::Tabs {
        tabs: vec![tab("t1", &pane("p2", Some("s1")))],
    });
    let sent = h.sent();
    assert_eq!(unloads(&sent), 1, "closing the chat pane unloads: {sent:?}");
}

#[gpui::test]
fn a_reconnect_asks_for_the_chat_again(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    h.send(reply(Some(&id), vec![user("u1")]));
    h.send_welcome(support::PROTOCOL);
    h.load(&Fixture::single(session("s1").build()));
    let sent = h.sent();
    let again = load_request(&sent).expect("the new connection asks for the chat");
    assert_ne!(again, id, "under a fresh request id");
    assert_eq!(unloads(&sent), 0, "a new connection unloads nothing");
    assert!(shows_chat(&mut h), "the pane stays on its chat");
}

#[gpui::test]
fn an_agent_without_a_chat_snaps_the_pane_back_with_a_note(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    h.send(DaemonMessage::Error {
        message: "chat is not offered for agent `other`".to_owned(),
        request_id: Some(id),
        code: Some("unsupported_message".to_owned()),
    });
    assert!(!shows_chat(&mut h), "the pane is back on its terminal");
    assert_eq!(
        h.root(|root, _| root.pane_chat_note("p1").map(str::to_owned))
            .as_deref(),
        Some("This session offers no chat, so it shows its terminal")
    );
    assert!(painted(&mut h, "pane-chat-note-p1"), "the note is drawn");
    assert!(painted(&mut h, "pane-grid-p1"), "over the terminal");
    assert_eq!(
        unloads(&h.sent()),
        0,
        "a refused load has nothing to unload"
    );
}

#[gpui::test]
fn a_failed_load_shows_the_terminal_with_the_daemons_message(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    h.send(DaemonMessage::Error {
        message: "session `s1` not found".to_owned(),
        request_id: Some(id),
        code: Some("not_found".to_owned()),
    });
    assert!(!shows_chat(&mut h), "the pane is back on its terminal");
    assert_eq!(
        h.root(|root, _| root.pane_chat_note("p1").map(str::to_owned))
            .as_deref(),
        Some("Could not load the chat: session `s1` not found")
    );
    assert!(painted(&mut h, "pane-chat-note-p1"), "the note is drawn");
    assert!(painted(&mut h, "pane-grid-p1"), "over the terminal");
    assert!(
        h.root(|root, _| root.toasts().is_empty()),
        "the note is the only word of it"
    );
    h.click_on(TO_CHAT);
    assert!(
        load_request(&h.sent()).is_some(),
        "a later toggle asks for the chat again"
    );
}

#[gpui::test]
fn a_reconnect_onto_a_plain_shell_gives_the_pane_its_terminal_back(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    h.send(reply(Some(&id), vec![user("u1")]));
    h.send_welcome(support::PROTOCOL);
    h.load(&Fixture::single(session("s2").shell("D:/src").build()));
    h.answer_scrollback("s2", b"");
    let sent = h.sent();
    assert!(
        sent.iter().any(|msg| matches!(
            msg,
            ClientMessage::Resize { session_id, .. } if session_id == "s2"
        )),
        "the pane drives the shell's PTY size: {sent:?}"
    );
    h.click_on("leaf-s2");
    let root = h.root.clone();
    let focused =
        h.cx.update(|window, cx| root.read(cx).pane_terminal_focused("p1", window, cx));
    assert!(focused, "the pane's terminal takes the keyboard");
}

/// `s1` of `r1` alone in `p1`, attached: with a repo registered, Ctrl+N
/// opens the spawn dialog itself.
fn attached_in_a_repo<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut fixture = Fixture::single(session("s1").in_repo("r1").build());
    fixture.repos = vec![repo("r1", "D:/src/r1")];
    let mut h = Harness::with(cx, dir, &fixture);
    h.answer_scrollback("s1", b"");
    h.sent();
    h
}

fn pop_out_p1(h: &mut Harness<'_>) {
    let key = PopoutKey::Pane {
        tab_id: "t1".to_owned(),
        pane_id: "p1".to_owned(),
    };
    let outcome = h.root_update(move |root, window, cx| root.open_popout(key, window, cx));
    assert_eq!(outcome, OpenOutcome::Opened);
}

/// Whether `p1`'s terminal has the pop-out window's keyboard.
fn focused_in_popout(h: &mut Harness<'_>) -> bool {
    let root = h.root.clone();
    let mut popout = h.popout_window();
    popout.update(|window, cx| root.read(cx).pane_terminal_focused("p1", window, cx))
}

#[gpui::test]
fn a_popped_out_chat_pane_gives_its_hidden_terminal_no_keys(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached_in_a_repo(cx, &dir);
    h.click_on(TO_CHAT);
    assert!(shows_chat(&mut h));
    pop_out_p1(&mut h);
    assert!(
        !focused_in_popout(&mut h),
        "the pop-out focuses no hidden terminal"
    );
    h.keys("ctrl-n");
    assert!(
        h.root(|root, _| root.spawn_dialog_open()),
        "Ctrl+N outside the terminals opens the spawn dialog"
    );
}

/// The pane menu's Terminal / Chat row.
const MENU_ROW: &str = "session-menu-pane-view";

/// The label of `p1`'s menu row after a right-click on its header.
fn menu_label(h: &mut Harness<'_>) -> Option<String> {
    h.right_click_on("pane-header-p1");
    h.root(|root, _| root.pane_view_menu_label().map(str::to_owned))
}

/// `native-ui.json` in the spec's ui dir, as it stands on disk.
fn saved_views(dir: &TestDir) -> serde_json::Value {
    let text = std::fs::read_to_string(dir.path().join("native-ui.json")).expect("a saved layout");
    let file: serde_json::Value = serde_json::from_str(&text).expect("native-ui.json is JSON");
    file["session_views"].clone()
}

/// A split of `p1` showing `s1` beside `p2` showing `second`.
fn split_with(second: &str) -> GridNode {
    GridNode::Split {
        direction: SplitDirection::Horizontal,
        ratio: 0.5,
        first: Box::new(pane("p1", Some("s1"))),
        second: Box::new(pane("p2", Some(second))),
    }
}

#[gpui::test]
fn the_pane_menu_switches_the_view(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir);
    assert_eq!(menu_label(&mut h).as_deref(), Some("Show chat"));
    h.click_on(MENU_ROW);
    assert!(load_request(&h.sent()).is_some(), "the row sends LoadChat");
    assert!(shows_chat(&mut h));
    assert!(
        h.root(|root, _| root.session_menu().is_none()),
        "the menu closes"
    );
    assert_eq!(menu_label(&mut h).as_deref(), Some("Show terminal"));
    h.click_on(MENU_ROW);
    assert_eq!(unloads(&h.sent()), 1, "the row sends UnloadChat");
    assert!(!shows_chat(&mut h));
}

#[gpui::test]
fn the_pane_menu_offers_no_view_for_a_plain_shell(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(
        cx,
        &dir,
        &Fixture::single(session("s1").shell("D:/src").build()),
    );
    h.answer_scrollback("s1", b"");
    assert_eq!(menu_label(&mut h), None);
}

#[gpui::test]
fn the_key_switches_the_focused_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir);
    h.keys("ctrl-shift-m");
    assert!(load_request(&h.sent()).is_some(), "the key sends LoadChat");
    assert!(shows_chat(&mut h));
    h.keys("ctrl-shift-m");
    assert_eq!(unloads(&h.sent()), 1, "the key sends UnloadChat");
    assert!(!shows_chat(&mut h));
    assert!(h.sent_input("s1").is_empty(), "the terminal never saw it");
}

#[gpui::test]
fn a_new_pane_on_the_session_opens_in_its_remembered_view(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir);
    h.click_on(TO_CHAT);
    h.sent();
    h.send(DaemonMessage::Tabs {
        tabs: vec![tab("t1", &split_with("s1"))],
    });
    assert!(
        h.root(|root, _| root.pane_shows_chat("p2")),
        "p2 opens on the chat"
    );
    h.click_on(TO_TERMINAL);
    h.send(DaemonMessage::Tabs {
        tabs: vec![tab("t1", &pane("p1", Some("s1")))],
    });
    h.send(DaemonMessage::Tabs {
        tabs: vec![tab("t1", &split_with("s1"))],
    });
    assert!(
        !h.root(|root, _| root.pane_shows_chat("p2")),
        "the last view chosen was the terminal"
    );
}

#[gpui::test]
fn a_new_session_opens_on_its_terminal(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir);
    h.click_on(TO_CHAT);
    h.sent();
    h.send(DaemonMessage::Sessions {
        sessions: vec![session("s1").build(), session("s2").build()],
    });
    h.send(DaemonMessage::Tabs {
        tabs: vec![tab("t1", &split_with("s2"))],
    });
    assert!(shows_chat(&mut h), "p1 keeps its chat");
    assert!(!h.root(|root, _| root.pane_shows_chat("p2")));
    let sent = h.sent();
    assert!(
        !sent.iter().any(|msg| matches!(
            msg,
            ClientMessage::LoadChat { session_id, .. } if session_id == "s2"
        )),
        "nothing asks for the new session's chat: {sent:?}"
    );
}

#[gpui::test]
fn a_popped_out_pane_keeps_its_view(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached_in_a_repo(cx, &dir);
    h.click_on(TO_CHAT);
    h.sent();
    pop_out_p1(&mut h);
    assert!(shows_chat(&mut h), "the popped pane still shows its chat");
    let sent = h.sent();
    assert_eq!(unloads(&sent), 0, "nothing unloads: {sent:?}");
    assert_eq!(load_request(&sent), None, "nor loads again");
}

#[gpui::test]
fn the_remembered_view_is_saved_with_only_live_sessions(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    std::fs::write(
        dir.path().join("native-ui.json"),
        r#"{ "session_views": {
            "local": { "s1": "chat", "gone": "chat" },
            "ssh:dev": { "gone": "chat" }
        } }"#,
    )
    .expect("seed native-ui.json");
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    assert!(
        shows_chat(&mut h),
        "a restarted client opens s1 on its chat"
    );
    assert!(load_request(&h.sent()).is_some(), "and asks for it");
    let other_host = serde_json::json!({ "gone": "chat" });
    h.click_on(TO_TERMINAL);
    assert_eq!(
        saved_views(&dir),
        serde_json::json!({ "ssh:dev": other_host }),
        "this PC's ended session goes, another host's stays"
    );
    h.click_on(TO_CHAT);
    assert_eq!(
        saved_views(&dir),
        serde_json::json!({ "local": { "s1": "chat" }, "ssh:dev": other_host })
    );
}

#[gpui::test]
fn the_key_in_a_pop_out_switches_its_pane_and_never_reaches_the_terminal(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached_in_a_repo(cx, &dir);
    pop_out_p1(&mut h);
    assert!(
        focused_in_popout(&mut h),
        "the popped terminal has the keys"
    );
    h.popout_window().simulate_keystrokes("ctrl-shift-m");
    h.cx.run_until_parked();
    assert!(shows_chat(&mut h), "the popped pane shows its chat");
    assert!(load_request(&h.sent()).is_some(), "the key sends LoadChat");
    assert!(h.sent_input("s1").is_empty(), "the terminal never saw it");
}

#[gpui::test]
fn switching_a_popped_pane_to_its_chat_releases_its_terminals_keys(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached_in_a_repo(cx, &dir);
    pop_out_p1(&mut h);
    assert!(
        focused_in_popout(&mut h),
        "the popped terminal has the pop-out's keyboard"
    );
    h.root_update(|root, window, cx| root.toggle_pane_view("p1", window, cx));
    assert!(shows_chat(&mut h));
    assert!(
        !focused_in_popout(&mut h),
        "the pop-out's keyboard left the hidden terminal"
    );
}

/// A `ChatWork` for `s1`.
fn work(
    subagents: Vec<WorkSubagent>,
    shells: Vec<WorkShell>,
    wakeup_at: Option<DateTime<Utc>>,
) -> DaemonMessage {
    DaemonMessage::ChatWork {
        session_id: "s1".to_owned(),
        subagents,
        shells,
        wakeup_at,
    }
}

fn sleeper(now: DateTime<Utc>, id: &str, description: &str) -> WorkSubagent {
    WorkSubagent {
        id: id.to_owned(),
        description: description.to_owned(),
        agent_type: Some("general-purpose".to_owned()),
        since: Some(now - TimeDelta::minutes(2)),
        from_screen: false,
    }
}

/// Sends two subagents, a shell started 3 minutes ago and a wakeup 4
/// minutes off, stamped from the view's wall clock.
fn send_full_work(h: &mut Harness<'_>) {
    let now = h.root(|root, _| root.wall_now());
    let shell = WorkShell {
        id: "b1".to_owned(),
        description: "Run the footer specs".to_owned(),
        command: Some("cargo nextest run footer".to_owned()),
        since: Some(now - TimeDelta::minutes(3)),
        from_screen: false,
    };
    h.send(work(
        vec![
            sleeper(now, "a1", "Sleeper A"),
            sleeper(now, "a2", "Sleeper B"),
        ],
        vec![shell],
        Some(now + TimeDelta::minutes(4)),
    ));
}

fn strip_text(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.chat_strip("s1").map(|strip| strip.text))
}

fn work_repaints(h: &mut Harness<'_>) -> usize {
    h.root(|root, _| root.chat_work_repaints())
}

#[gpui::test]
fn the_strip_shows_running_work(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    h.send(reply(Some(&id), vec![user("u1")]));
    send_full_work(&mut h);
    assert_eq!(
        strip_text(&mut h).as_deref(),
        Some(
            "2 subagents \u{b7} 1 background shell (cargo nextest run footer, 3m) \u{b7} wakeup in 4m"
        )
    );
    assert!(drawn_now(&mut h, "work-strip"), "the strip is drawn");
    assert!(painted(&mut h, "chat-row-p1-u1"), "under the rows");
}

#[gpui::test]
fn an_empty_strip_hides(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    h.send(reply(Some(&id), vec![user("u1")]));
    send_full_work(&mut h);
    // `painted`, not `drawn_now`: a narrowed frame's bounds would outlive
    // the strip and read as a strip still drawn.
    assert!(painted(&mut h, "work-strip"));
    h.send(work(vec![], vec![], None));
    assert_eq!(strip_text(&mut h), None);
    assert!(!drawn_now(&mut h, "work-strip"), "nothing runs, no strip");
}

#[gpui::test]
fn the_countdown_ticks(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    h.send(reply(Some(&id), vec![user("u1")]));
    send_full_work(&mut h);
    let before = work_repaints(&mut h);
    h.advance(std::time::Duration::from_secs(60));
    let text = strip_text(&mut h).unwrap_or_default();
    assert!(text.ends_with("wakeup in 3m"), "a minute on: {text}");
    assert!(
        work_repaints(&mut h) > before,
        "the strip's timer repainted it"
    );
}

#[gpui::test]
fn a_screen_read_count_has_its_tooltip(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    h.send(reply(Some(&id), vec![user("u1")]));
    let counted = WorkShell {
        id: String::new(),
        description: String::new(),
        command: None,
        since: None,
        from_screen: true,
    };
    h.send(work(vec![], vec![counted], None));
    let strip = h
        .root(|root, _| root.chat_strip("s1"))
        .expect("a counted shell shows the strip");
    assert_eq!(strip.text, "1 background shell");
    let tips: Vec<Option<String>> = strip.items.into_iter().map(|item| item.tooltip).collect();
    assert_eq!(
        tips,
        [Some(
            "Counted from the screen; no details reported yet".to_owned()
        )]
    );
    assert!(painted(&mut h, "work-item-0"), "its segment is drawn");
}

#[gpui::test]
fn a_subagent_item_is_not_tappable(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    h.send(reply(Some(&id), vec![user("u1")]));
    send_full_work(&mut h);
    let strip = h.root(|root, _| root.chat_strip("s1"));
    assert!(
        strip.is_some_and(|strip| strip.items.iter().all(|item| !item.tappable)),
        "no item takes a click"
    );
    h.sent();
    h.click_on("work-item-0");
    let sent = h.sent();
    assert!(
        sent.is_empty(),
        "a click on a subagent sends nothing: {sent:?}"
    );
}

#[gpui::test]
fn the_strip_shows_while_the_chat_is_empty(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    h.send(reply(Some(&id), vec![]));
    send_full_work(&mut h);
    assert!(strip_text(&mut h).is_some());
    assert!(
        drawn_now(&mut h, "work-strip"),
        "the strip is drawn under the empty note"
    );
}

// Cards and the composer.

/// `p1` on its chat, the chat loaded with no rows and its composer up.
fn loaded<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let (mut h, id) = opened(cx, dir);
    h.send(reply(Some(&id), vec![]));
    h.sent();
    h
}

fn options(list: &[(u32, &str)]) -> Vec<ChatOption> {
    list.iter()
        .map(|(number, label)| ChatOption {
            number: *number,
            label: (*label).to_owned(),
            description: None,
            selected: false,
        })
        .collect()
}

fn prompt(id: &str, kind: ChatPromptKind, list: &[(u32, &str)]) -> ChatPrompt {
    ChatPrompt {
        id: id.to_owned(),
        kind,
        subject: "calc.py".to_owned(),
        body: "Do you want to make this edit?".to_owned(),
        options: options(list),
        questions: Vec::new(),
        can_amend: true,
        always_allow_tip: None,
        multi: false,
        cursor: None,
    }
}

fn permission(id: &str) -> ChatPrompt {
    prompt(
        id,
        ChatPromptKind::Permission,
        &[
            (1, "Yes"),
            (2, "Yes, and don't ask again for: python *"),
            (3, "No"),
        ],
    )
}

/// The screen now shows `prompt` (or none) with the composer `state`.
fn show_screen(h: &mut Harness<'_>, state: ComposerState, prompt: Option<ChatPrompt>) {
    h.send(DaemonMessage::ChatScreenChanged {
        session_id: "s1".to_owned(),
        screen: ChatScreen {
            composer: state,
            prompt,
        },
    });
}

fn show(h: &mut Harness<'_>, prompt: ChatPrompt) {
    show_screen(h, ComposerState::Dialog, Some(prompt));
}

/// The `ChatAnswer`s sent since the last drain: prompt id, answer and
/// request id.
fn answers(h: &mut Harness<'_>) -> Vec<(String, ChatAnswerKind, Option<String>)> {
    h.sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::ChatAnswer {
                session_id,
                prompt_id,
                answer,
                request_id,
            } if session_id == "s1" => Some((prompt_id, answer, request_id)),
            _ => None,
        })
        .collect()
}

fn answer_kinds(h: &mut Harness<'_>) -> Vec<ChatAnswerKind> {
    answers(h)
        .into_iter()
        .map(|(_, answer, _)| answer)
        .collect()
}

/// The `ChatSend`s sent since the last drain: text and request id.
fn sends(h: &mut Harness<'_>) -> Vec<(String, Option<String>)> {
    h.sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::ChatSend {
                session_id,
                text,
                request_id,
            } if session_id == "s1" => Some((text, request_id)),
            _ => None,
        })
        .collect()
}

fn type_text(h: &mut Harness<'_>, text: &str) {
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

fn composer_text(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, cx| root.chat_composer_text("s1", cx))
}

fn note(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.chat_composer_note("s1"))
}

fn refused(h: &mut Harness<'_>, request_id: Option<String>, message: &str) {
    h.send(DaemonMessage::Error {
        message: message.to_owned(),
        request_id,
        code: Some(CHAT_REFUSED.to_owned()),
    });
}

fn opt(number: u32) -> ChatAnswerKind {
    ChatAnswerKind::Option { number }
}

/// The selectors of the buttons beside the composer.
fn actions(h: &mut Harness<'_>) -> Vec<&'static str> {
    h.root(|root, cx| root.chat_composer_actions("s1", cx))
}

/// The current card's prompt id and whether it is busy.
fn card(h: &mut Harness<'_>) -> Option<(String, bool)> {
    h.root(|root, _| root.chat_card("s1"))
}

#[gpui::test]
fn each_permission_button_sends_its_chat_answer(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    for (id, button, number) in [
        ("p1", "chat-card-1", 1),
        ("p2", "chat-card-2", 2),
        ("p3", "chat-card-3", 3),
    ] {
        show(&mut h, permission(id));
        h.click_on(button);
        assert_eq!(answer_kinds(&mut h), [opt(number)], "{button}");
    }
}

#[gpui::test]
fn question_review_plan_trust_and_heads_up_cards_send_their_answers(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    let single = prompt(
        "q1",
        ChatPromptKind::Question,
        &[(1, "sub"), (2, "div"), (3, "Type something")],
    );
    show(&mut h, single.clone());
    h.click_on("chat-card-2");
    assert_eq!(answer_kinds(&mut h), [opt(2)]);

    let mut multi = single.clone();
    multi.id = "q2".to_owned();
    multi.multi = true;
    multi.questions = ["Op", "Base"]
        .iter()
        .enumerate()
        .map(|(index, label)| ChatQuestionTab {
            label: (*label).to_owned(),
            answered: false,
            current: index == 0,
        })
        .collect();
    show(&mut h, multi.clone());
    h.click_on("chat-card-1");
    h.click_on("chat-card-back");
    assert_eq!(
        answer_kinds(&mut h),
        [ChatAnswerKind::Toggle { number: 1 }, ChatAnswerKind::Back]
    );
    multi.id = "q3".to_owned();
    show(&mut h, multi.clone());
    h.click_on("chat-card-skip");
    multi.id = "q4".to_owned();
    show(&mut h, multi);
    h.click_on("chat-card-submit");
    assert_eq!(
        answer_kinds(&mut h),
        [ChatAnswerKind::Skip, ChatAnswerKind::Submit]
    );

    let mut typed = single;
    typed.id = "q5".to_owned();
    show(&mut h, typed);
    h.click_on("chat-composer");
    type_text(&mut h, "use mul");
    h.click_on("chat-card-3");
    assert_eq!(
        answer_kinds(&mut h),
        [ChatAnswerKind::TypeSomething {
            text: "use mul".to_owned()
        }]
    );

    show(
        &mut h,
        prompt(
            "r1",
            ChatPromptKind::Review,
            &[(1, "Submit answers"), (2, "Cancel")],
        ),
    );
    h.click_on("chat-card-2");
    show(
        &mut h,
        prompt(
            "pl1",
            ChatPromptKind::Plan,
            &[
                (1, "Yes, auto"),
                (2, "Yes, manual"),
                (3, "No, keep planning"),
            ],
        ),
    );
    h.click_on("chat-card-1");
    show(
        &mut h,
        prompt(
            "t1",
            ChatPromptKind::Trust,
            &[(1, "Yes, proceed"), (2, "No, exit")],
        ),
    );
    h.click_on("chat-card-2");
    show(
        &mut h,
        prompt(
            "h1",
            ChatPromptKind::HeadsUp,
            &[(1, "Learn more"), (2, "Knew this already"), (0, "Dismiss")],
        ),
    );
    h.click_on("chat-card-0");
    assert_eq!(answer_kinds(&mut h), [opt(2), opt(1), opt(2), opt(0)]);
}

#[gpui::test]
fn a_card_answer_carries_the_prompt_id_and_a_request_id(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    show(&mut h, permission("p7"));
    h.click_on("chat-card-1");
    let sent = answers(&mut h);
    assert_eq!(sent.len(), 1);
    let (prompt_id, answer, request_id) = &sent[0];
    assert_eq!(prompt_id, "p7");
    assert_eq!(*answer, opt(1));
    assert!(request_id.as_deref().is_some_and(|id| !id.is_empty()));
}

#[gpui::test]
fn a_digit_inside_the_grace_is_dropped_and_after_it_answers(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    h.click_on("chat-composer");
    show(&mut h, permission("p1"));
    h.advance(Duration::from_millis(399));
    h.keys("1");
    assert_eq!(answer_kinds(&mut h), [], "inside the grace nothing answers");
    assert_eq!(
        composer_text(&mut h).as_deref(),
        Some(""),
        "and nothing is typed"
    );
    h.advance(Duration::from_millis(1));
    h.keys("1");
    assert_eq!(answer_kinds(&mut h), [opt(1)]);
    assert_eq!(composer_text(&mut h).as_deref(), Some(""));
}

#[gpui::test]
fn digits_type_into_a_non_empty_composer(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    h.click_on("chat-composer");
    show(&mut h, permission("p1"));
    h.advance(Duration::from_millis(500));
    type_text(&mut h, "a1");
    assert_eq!(answer_kinds(&mut h), []);
    assert_eq!(composer_text(&mut h).as_deref(), Some("a1"));
}

#[gpui::test]
fn esc_denies_ctrl_enter_allows_once_and_ctrl_shift_enter_always_allows(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    h.click_on("chat-composer");
    show(&mut h, permission("p1"));
    h.keys("escape");
    show(&mut h, permission("p2"));
    h.keys("ctrl-enter");
    show(&mut h, permission("p3"));
    h.keys("ctrl-shift-enter");
    assert_eq!(answer_kinds(&mut h), [opt(3), opt(1), opt(2)]);
}

#[gpui::test]
fn the_composer_sends_with_ctrl_enter_and_keeps_its_text_when_refused(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    h.click_on("chat-composer");
    type_text(&mut h, "hello there");
    h.keys("ctrl-enter");
    let sent = sends(&mut h);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, "hello there");
    assert_eq!(
        composer_text(&mut h).as_deref(),
        Some(""),
        "the send clears it"
    );
    refused(&mut h, sent[0].1.clone(), "the prompt box holds a draft");
    assert_eq!(composer_text(&mut h).as_deref(), Some("hello there"));
    assert_eq!(
        h.root(|root, _| root.chat_composer_error("s1")).as_deref(),
        Some("the prompt box holds a draft")
    );
    assert!(painted(&mut h, "chat-composer-error"));
}

#[gpui::test]
fn while_a_dialog_is_up_the_composer_offers_the_two_ride_on_answers(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    h.click_on("chat-composer");
    type_text(&mut h, "careful");
    assert_eq!(actions(&mut h), ["chat-send"]);
    assert!(painted(&mut h, "chat-send"));
    show(&mut h, permission("p1"));
    assert_eq!(
        actions(&mut h),
        ["chat-ride-yes", "chat-ride-no"],
        "Send gives way to the ride-on answers"
    );
    assert!(painted(&mut h, "chat-ride-yes"));
    assert!(painted(&mut h, "chat-ride-no"));
    h.keys("ctrl-enter");
    assert_eq!(
        answer_kinds(&mut h),
        [],
        "Ctrl+Enter leaves the choice to the buttons"
    );
    h.click_on("chat-ride-yes");
    assert_eq!(
        answer_kinds(&mut h),
        [ChatAnswerKind::AllowWithText {
            text: "careful".to_owned()
        }]
    );
    assert_eq!(composer_text(&mut h).as_deref(), Some(""));

    let mut fixed = permission("p2");
    fixed.can_amend = false;
    show(&mut h, fixed);
    h.click_on("chat-composer");
    type_text(&mut h, "no");
    assert_eq!(actions(&mut h), ["chat-ride-no"], "no amendment, no Yes");
    h.click_on("chat-ride-no");
    assert_eq!(
        answer_kinds(&mut h),
        [ChatAnswerKind::DenyWithText {
            text: "no".to_owned()
        }]
    );
}

#[gpui::test]
fn a_draft_composer_shows_why_it_cannot_send(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    assert_eq!(note(&mut h), None);
    show_screen(&mut h, ComposerState::Draft, None);
    assert_eq!(
        note(&mut h).as_deref(),
        Some("The prompt box has a draft. Send or clear it in the terminal first.")
    );
    assert!(painted(&mut h, "chat-composer-note"));
}

#[gpui::test]
fn an_overlay_composer_names_the_overlay(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    show_screen(
        &mut h,
        ComposerState::Overlay {
            name: "/model".to_owned(),
        },
        None,
    );
    assert_eq!(
        note(&mut h).as_deref(),
        Some("/model is open in the terminal. Close it there to send.")
    );
}

#[gpui::test]
fn a_draft_from_our_own_send_is_not_a_refusal(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    h.click_on("chat-composer");
    type_text(&mut h, "hello");
    h.keys("ctrl-enter");
    show_screen(&mut h, ComposerState::Draft, None);
    assert_eq!(
        note(&mut h).as_deref(),
        Some("Sending…"),
        "our own paste is not a draft"
    );
    show_screen(&mut h, ComposerState::Working, None);
    show_screen(&mut h, ComposerState::Draft, None);
    assert!(note(&mut h).is_some(), "a later draft is the owner's");
}

#[gpui::test]
fn a_submitted_card_stays_busy_until_the_dialog_goes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    show(&mut h, permission("p1"));
    h.click_on("chat-card-1");
    assert_eq!(card(&mut h), Some(("p1".to_owned(), true)));
    assert!(painted(&mut h, "chat-card-busy"));
    h.click_on("chat-card-2");
    assert_eq!(
        answers(&mut h).len(),
        1,
        "a busy card takes no second answer"
    );
    show(&mut h, permission("p1"));
    assert_eq!(card(&mut h), Some(("p1".to_owned(), true)));
    show_screen(&mut h, ComposerState::Ready, None);
    h.send(append(
        vec![ChatEntry::Receipt {
            id: "rc1".to_owned(),
            question: "Edit calc.py".to_owned(),
            answer: "Yes".to_owned(),
        }],
        &[],
    ));
    assert_eq!(card(&mut h), None);
    assert!(!h.root(|root, _| root.chat_answered_elsewhere("s1")));
    assert!(painted(&mut h, "chat-row-p1-rc1"));
}

#[gpui::test]
fn a_card_closed_without_our_answer_reads_answered_elsewhere(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    show(&mut h, permission("p1"));
    show_screen(&mut h, ComposerState::Ready, None);
    assert!(h.root(|root, _| root.chat_answered_elsewhere("s1")));
    assert!(painted(&mut h, "chat-answered-elsewhere"));
    h.send(append(vec![user("u1")], &[]));
    assert!(!h.root(|root, _| root.chat_answered_elsewhere("s1")));
}

#[gpui::test]
fn a_refused_answer_frees_the_card_and_shows_the_daemons_reason(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    show(&mut h, permission("p1"));
    h.click_on("chat-card-1");
    let request_id = answers(&mut h)[0].2.clone();
    refused(&mut h, request_id, "the terminal did not respond");
    assert_eq!(card(&mut h), Some(("p1".to_owned(), false)));
    assert_eq!(
        h.root(|root, _| root.chat_card_refusal("s1")).as_deref(),
        Some("the terminal did not respond")
    );
    assert!(painted(&mut h, "chat-card-refusal"));
    h.click_on("chat-card-1");
    assert_eq!(
        answer_kinds(&mut h),
        [opt(1)],
        "the freed card answers again"
    );
}

#[gpui::test]
fn the_heads_up_notice_is_pinned_over_the_composer(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    h.click_on("chat-composer");
    show(
        &mut h,
        prompt(
            "h1",
            ChatPromptKind::HeadsUp,
            &[(1, "Learn more"), (2, "Knew this already"), (0, "Dismiss")],
        ),
    );
    let card = h.bounds("chat-card");
    let composer = h.bounds("chat-composer");
    assert!(card.size.height > px(0.0));
    assert!(
        card.bottom() <= composer.top(),
        "{card:?} over {composer:?}"
    );
    h.advance(Duration::from_millis(400));
    h.keys("0");
    assert_eq!(answer_kinds(&mut h), [opt(0)]);
}

#[gpui::test]
fn an_unreadable_dialog_says_to_open_the_terminal(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    show(&mut h, prompt("u1", ChatPromptKind::Unknown, &[(1, "a")]));
    assert!(painted(&mut h, "chat-card-terminal"));
    assert!(!painted(&mut h, "chat-card-1"));
}

#[gpui::test]
fn the_composer_sits_under_the_running_work_strip(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    send_full_work(&mut h);
    let strip = h.bounds("work-strip");
    let composer = h.bounds("chat-composer");
    assert!(strip.size.height > px(0.0));
    assert!(composer.size.height > px(0.0));
    assert!(
        strip.bottom() <= composer.top(),
        "{strip:?} over {composer:?}"
    );
}

#[gpui::test]
fn a_refused_ride_on_after_the_dialog_went_gives_the_text_back(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    h.click_on("chat-composer");
    show(&mut h, permission("p1"));
    type_text(&mut h, "careful");
    h.click_on("chat-ride-no");
    let request_id = answers(&mut h)[0].2.clone();
    assert_eq!(composer_text(&mut h).as_deref(), Some(""));
    show_screen(&mut h, ComposerState::Ready, None);
    refused(
        &mut h,
        request_id,
        "the denial was typed, but the message was not sent",
    );
    assert_eq!(composer_text(&mut h).as_deref(), Some("careful"));
    assert_eq!(
        h.root(|root, _| root.chat_composer_error("s1")).as_deref(),
        Some("the denial was typed, but the message was not sent")
    );
}

#[gpui::test]
fn a_refusal_keeps_what_was_typed_since(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    h.click_on("chat-composer");
    type_text(&mut h, "hello");
    h.keys("ctrl-enter");
    let request_id = sends(&mut h)[0].1.clone();
    type_text(&mut h, "more");
    refused(&mut h, request_id, "the prompt box holds a draft");
    assert_eq!(composer_text(&mut h).as_deref(), Some("hello\nmore"));
}

#[gpui::test]
fn a_second_send_waits_for_the_first(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    h.click_on("chat-composer");
    type_text(&mut h, "one");
    h.keys("ctrl-enter");
    type_text(&mut h, "two");
    h.keys("ctrl-enter");
    let sent: Vec<String> = sends(&mut h).into_iter().map(|(text, _)| text).collect();
    assert_eq!(sent, ["one"]);
    assert_eq!(composer_text(&mut h).as_deref(), Some("two"));
    assert_eq!(note(&mut h).as_deref(), Some("Sending\u{2026}"));
    h.click_on("chat-send");
    assert!(
        sends(&mut h).is_empty(),
        "Send is disabled while one is in flight"
    );
}

#[gpui::test]
fn a_send_with_no_pending_entry_settles_when_the_turn_starts(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    h.click_on("chat-composer");
    type_text(&mut h, "/compact");
    h.keys("ctrl-enter");
    assert_eq!(sends(&mut h).len(), 1);
    show_screen(&mut h, ComposerState::Working, None);
    assert_eq!(note(&mut h), None);
    type_text(&mut h, "next");
    h.keys("ctrl-enter");
    assert_eq!(sends(&mut h).len(), 1, "the next send goes");
}

/// An answer with a stretch whose long form holds a nested stretch.
const STRETCHED: &str = "The build {{failed|failed on a cold cache.\n\nThe {{cache key|key is the lockfile hash}} changed.}} today.";
const OUTER: &str = "failed";
const INNER: &str = "failed \u{203A} cache key";

fn drawn_text(h: &mut Harness<'_>, answer_id: &str) -> Option<String> {
    let answer_id = answer_id.to_owned();
    h.root(move |root, _| root.chat_answer_text("s1", &answer_id))
}

fn stretch_links(h: &mut Harness<'_>, answer_id: &str) -> Vec<(String, String)> {
    let answer_id = answer_id.to_owned();
    h.root(move |root, _| root.chat_answer_stretchtext("s1", &answer_id))
}

fn link(text: &str, key: &str) -> (String, String) {
    (text.to_owned(), key.to_owned())
}

/// Clicks the short form or the closer of stretch `key` in answer
/// `answer_id`.
fn toggle(h: &mut Harness<'_>, answer_id: &str, key: &str) {
    let (answer_id, key) = (answer_id.to_owned(), key.to_owned());
    h.root_update(move |root, _, cx| root.toggle_stretchtext("s1", &answer_id, &key, cx));
}

/// `p1` on its chat with `entries` loaded.
fn chat_with<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    entries: Vec<ChatEntry>,
) -> Harness<'a> {
    let (mut h, id) = opened(cx, dir);
    h.send(reply(Some(&id), entries));
    h
}

#[gpui::test]
fn a_stretch_draws_its_short_form_until_clicked(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_with(cx, &dir, vec![user("u1"), answer("a1", STRETCHED, false)]);
    assert_eq!(
        drawn_text(&mut h, "a1").as_deref(),
        Some("The build failed today.")
    );
    assert_eq!(stretch_links(&mut h, "a1"), [link("failed", OUTER)]);
    let closed = h.bounds("chat-row-p1-a1").size.height;
    assert!(closed > px(0.0), "the answer is drawn");
    toggle(&mut h, "a1", OUTER);
    let open = h.bounds("chat-row-p1-a1").size.height;
    assert!(
        open > closed,
        "the long form takes its place: {closed:?} then {open:?}"
    );
}

#[gpui::test]
fn clicking_a_stretch_opens_one_level(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_with(cx, &dir, vec![answer("a1", STRETCHED, false)]);
    toggle(&mut h, "a1", OUTER);
    assert_eq!(
        drawn_text(&mut h, "a1").as_deref(),
        Some("The build failed on a cold cache.\nThe cache key changed. \u{2039} today.")
    );
    assert_eq!(
        stretch_links(&mut h, "a1"),
        [link("cache key", INNER), link("\u{2039}", OUTER)],
        "the nested stretch is a link of its own, still closed"
    );
}

#[gpui::test]
fn the_closer_closes_a_stretch_and_its_nested_ones(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_with(cx, &dir, vec![answer("a1", STRETCHED, false)]);
    toggle(&mut h, "a1", OUTER);
    toggle(&mut h, "a1", INNER);
    assert_eq!(
        drawn_text(&mut h, "a1").as_deref(),
        Some(
            "The build failed on a cold cache.\nThe key is the lockfile hash \u{2039} changed. \u{2039} today."
        )
    );
    assert_eq!(
        stretch_links(&mut h, "a1"),
        [link("\u{2039}", INNER), link("\u{2039}", OUTER)]
    );
    toggle(&mut h, "a1", OUTER);
    assert_eq!(
        drawn_text(&mut h, "a1").as_deref(),
        Some("The build failed today.")
    );
    toggle(&mut h, "a1", OUTER);
    assert_eq!(
        stretch_links(&mut h, "a1"),
        [link("cache key", INNER), link("\u{2039}", OUTER)],
        "opened again, the nested stretch starts closed"
    );
}

#[gpui::test]
fn an_open_stretch_survives_the_live_answer_becoming_final(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_with(cx, &dir, vec![user("u1")]);
    h.send(append(vec![answer("live:m1", STRETCHED, true)], &[]));
    assert_eq!(stretch_links(&mut h, "live:m1"), [link("failed", OUTER)]);
    toggle(&mut h, "live:m1", OUTER);
    h.send(append(vec![answer("a1", STRETCHED, false)], &["live:m1"]));
    assert_eq!(row_ids(&mut h), ["u1", "a1"]);
    assert_eq!(
        stretch_links(&mut h, "a1"),
        [link("cache key", INNER), link("\u{2039}", OUTER)],
        "the transcript answer opens where the live one was open"
    );
}

#[gpui::test]
fn a_popped_out_chat_pane_shows_the_same_open_stretches(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached_in_a_repo(cx, &dir);
    h.click_on(TO_CHAT);
    let id = load_request(&h.sent()).expect("the toggle asks for the chat");
    h.send(reply(Some(&id), vec![answer("a1", STRETCHED, false)]));
    toggle(&mut h, "a1", OUTER);
    pop_out_p1(&mut h);
    let mut popout = h.popout_window();
    popout.run_until_parked();
    let open = popout
        .debug_bounds("chat-row-p1-a1")
        .expect("the pop-out draws the answer")
        .size
        .height;
    toggle(&mut h, "a1", OUTER);
    let mut popout = h.popout_window();
    popout.run_until_parked();
    let closed = popout
        .debug_bounds("chat-row-p1-a1")
        .expect("the pop-out still draws the answer")
        .size
        .height;
    assert!(
        open > closed,
        "the pop-out drew the stretch the main window opened: {open:?} then {closed:?}"
    );
}

#[gpui::test]
fn a_stretch_in_a_code_block_stays_literal(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_with(
        cx,
        &dir,
        vec![answer(
            "a1",
            "Write `{{a|b}}` like so:\n\n```\n{{name|value}}\n```",
            false,
        )],
    );
    assert_eq!(
        drawn_text(&mut h, "a1").as_deref(),
        Some("Write {{a|b}} like so:\n{{name|value}}")
    );
    assert_eq!(stretch_links(&mut h, "a1"), []);
}

#[gpui::test]
fn the_work_strip_grows_with_the_ui_text_size(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    h.send(reply(Some(&id), vec![user("u1")]));
    send_full_work(&mut h);
    let base = h.bounds("work-strip").size.height;

    h.root_update(|root, _, cx| root.set_ui_text_size(3, cx));
    let strip = h.bounds("work-strip");
    assert_eq!(
        strip.size.height,
        base + px(3.0),
        "three pixels taller at +3"
    );
    let text = h.bounds("work-item-0");
    assert!(
        text.top() >= strip.top() && text.bottom() <= strip.bottom(),
        "its text stays inside the strip: {text:?} in {strip:?}"
    );
}

#[gpui::test]
fn a_chat_pane_redraws_in_a_new_theme(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    h.send(reply(Some(&id), vec![user("u1")]));
    let mut mine = UiTheme::CONTRAST;
    mine.ground = 0x0010_2030;

    h.root_update(|root, _, cx| root.set_theme(mine, cx));
    assert_eq!(
        h.root(|root, cx| root.chat_row_theme("p1", "s1", cx).ground),
        mine.ground,
        "the chat's rows are drawn in the new theme"
    );
    assert!(
        painted(&mut h, "chat-row-p1-u1"),
        "the chat's rows are still drawn"
    );
}

#[gpui::test]
fn a_chat_row_grows_with_the_ui_text_size(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_with(
        cx,
        &dir,
        vec![user("u1"), answer("a1", "#### Title", false)],
    );
    let question = h.bounds("chat-row-p1-u1").size.height;
    let heading = h.bounds("chat-row-p1-a1").size.height;

    h.root_update(|root, _, cx| root.set_ui_text_size(4, cx));
    assert!(
        h.bounds("chat-row-p1-u1").size.height > question,
        "a question grows at +4"
    );
    assert!(
        h.bounds("chat-row-p1-a1").size.height > heading,
        "an answer's heading grows at +4"
    );
}

#[gpui::test]
fn a_prompt_card_grows_with_the_ui_text_size(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    show(&mut h, permission("p1"));
    let card = h.bounds("chat-card").size.height;
    let button = h.bounds("chat-card-1").size.height;

    h.root_update(|root, _, cx| root.set_ui_text_size(4, cx));
    let grown = h.bounds("chat-card-1").size.height;
    assert!(
        grown >= button + px(4.0),
        "its buttons are at least four pixels taller at +4: {button:?} to {grown:?}"
    );
    assert!(
        h.bounds("chat-card").size.height > card,
        "the card grows around its text and buttons"
    );
}

#[gpui::test]
fn the_composer_grows_with_the_ui_text_size(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    let field = h.bounds("chat-composer").size.height;

    h.root_update(|root, _, cx| root.set_ui_text_size(4, cx));
    assert!(
        h.bounds("chat-composer").size.height > field,
        "the input's text grows at +4"
    );
}

#[gpui::test]
fn a_chat_heading_stays_above_its_paragraph_at_every_offset(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_with(
        cx,
        &dir,
        vec![answer("a1", "#### Title\n\nBody\n\nMore", false)],
    );

    for offset in [4, -2] {
        h.root_update(|root, _, cx| root.set_ui_text_size(offset, cx));
        let heading = h.bounds("viewer-md-block-chat-a1-0").size.height;
        let paragraph = h.bounds("viewer-md-block-chat-a1-1").size.height;
        assert!(
            heading > paragraph,
            "at {offset:+}: heading {heading:?}, paragraph {paragraph:?}"
        );
    }
}
