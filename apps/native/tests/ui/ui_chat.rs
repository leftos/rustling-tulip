//! Chat pane specs: a pane header's Chat toggle loads the session's chat
//! from the daemon and draws it in place of the terminal; the daemon's
//! appends, live answers and unasked resets update it; the last chat pane
//! going unloads it; a reconnect loads it again; and a session whose agent
//! offers no chat snaps back to its terminal with a note. The pane menu and
//! Ctrl+Shift+M switch the view too; each session's last view is saved in
//! `native-ui.json` and a new pane on it opens there, a new session on its
//! terminal, and a pop-out keeps its view. The running-work strip under the
//! chat shows the daemon's `ChatWork`, counts down and takes no click. A
//! tool card's Tail button, on a daemon that follows files, opens a box
//! under the card showing the file's chunks from the follow's first
//! (`reset`) one; a second press, the card leaving the chat or the chat
//! unloading closes it with `UntailFile`, a reconnect asks again, and a
//! refusal shows in the box.

use crate::support;

use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use chrono::{DateTime, TimeDelta, Utc};
use gpui::{Modifiers, ScrollDelta, ScrollEvent, TestAppContext, TouchPhase, point, px};
use protocol::error_codes::{CHAT_REFUSED, INVALID_REQUEST};
use protocol::{
    CatalogEntry, CatalogEntryKind, CatalogEntrySource, ChatAnswerKind, ChatEntry, ChatOption,
    ChatPrompt, ChatPromptKind, ChatQuestionTab, ChatScreen, ClientMessage, ComposerState,
    DaemonMessage, FEATURE_AGENT_COMMANDS, FEATURE_CHAT_CATALOG, FEATURE_CHAT_CLEAR_DRAFT,
    FEATURE_CHAT_DRAFT, FEATURE_CHAT_TOOL_RESULT, FEATURE_FILE_SEARCH, FEATURE_FILE_TAIL,
    FileMatch, GridNode, ScriptFacts, SplitDirection, ToolKind, ToolOutcome, WorkShell,
    WorkSubagent,
};
use rustling_tulip_native::chat::{self, ChatMode, PaneView, RowShape};
use rustling_tulip_native::completion::CompletionItem;
use rustling_tulip_native::target::Target;
use rustling_tulip_native::ui_theme::UiTheme;
use rustling_tulip_native::{Connection, NetEvent, OpenOutcome, PopoutKey};
use support::{Fixture, Harness, Opened, TestDir, facts, facts_message, pane, repo, session, tab};

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

/// A `ChatModel` for `session_id`, the daemon's report of the model its
/// main agent answers with and the effort it answered at.
fn chat_model(
    session_id: &str,
    model: Option<&str>,
    label: Option<&str>,
    effort: Option<&str>,
) -> DaemonMessage {
    DaemonMessage::ChatModel {
        session_id: session_id.to_owned(),
        model: model.map(str::to_owned),
        label: label.map(str::to_owned),
        effort: effort.map(str::to_owned),
        switch_command: None,
    }
}

/// A `ChatModel` for `session_id` naming `label` as the model its main agent
/// answers with, and `switch` as the text that opens the agent's model
/// picker.
fn chat_model_with_switch(session_id: &str, label: &str, switch: &str) -> DaemonMessage {
    DaemonMessage::ChatModel {
        session_id: session_id.to_owned(),
        model: Some("claude-opus-5-5".to_owned()),
        label: Some(label.to_owned()),
        effort: None,
        switch_command: Some(switch.to_owned()),
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
        tail_paths: Vec::new(),
        kind: ToolKind::Other,
        script: None,
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
            | ChatEntry::Agent { id, .. }
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

/// The text `s1`'s chat-model pill draws; none while it draws nothing.
fn pill_label(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.chat_model_label("s1"))
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
fn an_agent_message_draws_as_an_agent_row(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    let message = ChatEntry::Agent {
        id: "g1".to_owned(),
        from: "yaat-3a".to_owned(),
        text: "The fix is on main.".to_owned(),
    };
    h.send(reply(Some(&id), vec![message.clone()]));
    assert_eq!(h.root(|root, _| root.chat_rows("s1")), [message]);
    assert!(painted(&mut h, "chat-row-p1-g1"), "the row is drawn");
    assert!(painted(&mut h, "chat-agent-g1"), "as an agent bubble");
    assert!(
        painted(&mut h, "chat-agent-from-g1"),
        "with its sender line"
    );
    let row = h.bounds("chat-row-p1-g1");
    let bubble = h.bounds("chat-agent-g1");
    let sender = h.bounds("chat-agent-from-g1");
    assert!(
        bubble.origin.x - row.origin.x < px(1.0) && bubble.right() < row.right() - px(1.0),
        "on the left, not a user bubble on the right: {bubble:?} in {row:?}"
    );
    assert!(
        sender.origin.y < bubble.bottom() && sender.bottom() < bubble.bottom(),
        "the sender line is above the text inside the bubble: {sender:?} in {bubble:?}"
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

/// A finished call `id` of tool `name` the plugin classes as `kind`, or one
/// still running when `done` is false.
fn kinded(id: &str, name: &str, kind: ToolKind, done: bool) -> ChatEntry {
    ChatEntry::Tool {
        id: id.to_owned(),
        name: name.to_owned(),
        summary: format!("{name} {id}"),
        input: "{}".to_owned(),
        result: done.then(|| ToolOutcome {
            text: "ok".to_owned(),
            is_error: false,
        }),
        parent_id: None,
        tail_paths: Vec::new(),
        kind,
        script: None,
    }
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

fn display_rows(h: &mut Harness<'_>) -> Vec<RowShape> {
    h.root(|root, _| root.chat_display_rows("s1"))
}

fn run_shape(ids: &[&str], line: &str, running: bool) -> RowShape {
    RowShape::Run {
        ids: ids.iter().map(|id| (*id).to_owned()).collect(),
        line: line.to_owned(),
        running,
    }
}

fn saved_chat_mode(dir: &TestDir) -> serde_json::Value {
    let text = std::fs::read_to_string(dir.path().join("native-ui.json")).expect("a saved layout");
    let file: serde_json::Value = serde_json::from_str(&text).expect("native-ui.json is JSON");
    file["chat_mode"].clone()
}

#[gpui::test]
fn a_code_mode_script_draws_as_one_line_under_its_run_and_opens_to_its_cards(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    let facts = |eval: u32, ms: Option<u64>, error: Option<&str>| ScriptFacts {
        description: "Read the footer".to_owned(),
        eval: Some(eval),
        ms,
        error: error.map(str::to_owned),
    };
    let mut failed = scripted(
        kinded("s2", "REPL", ToolKind::Script, true),
        Some(facts(4, Some(300), Some("TypeError: x is undefined"))),
        None,
    );
    if let ChatEntry::Tool {
        result: Some(outcome),
        ..
    } = &mut failed
    {
        outcome.is_error = true;
    }
    h.send(reply(
        Some(&id),
        vec![
            user("u1"),
            scripted(
                kinded("s1", "REPL", ToolKind::Script, true),
                Some(facts(3, Some(1200), None)),
                None,
            ),
            scripted(kinded("i1", "Read", ToolKind::Read, true), None, Some("s1")),
            scripted(kinded("i2", "Read", ToolKind::Read, true), None, Some("s1")),
            failed,
            scripted(
                kinded("s3", "REPL", ToolKind::Script, false),
                Some(facts(5, None, None)),
                None,
            ),
        ],
    ));
    // Compact folds the three scripts into one run line, which draws each
    // script's own line under it while closed, and still does open.
    for selector in [
        "chat-run-p1-s1",
        "chat-script-p1-s1",
        "chat-script-p1-s2",
        "chat-script-p1-s3",
    ] {
        assert!(
            painted(&mut h, selector),
            "{selector} is drawn, the run closed"
        );
    }
    h.click_on("chat-run-p1-s1");
    for selector in [
        "chat-script-p1-s1",
        "chat-script-done-p1-s1",
        "chat-script-meta-p1-s1",
        "chat-script-chip-p1-s1-0",
        "chat-script-failed-p1-s2",
        "chat-script-error-p1-s2",
        "chat-script-running-p1-s3",
    ] {
        assert!(painted(&mut h, selector), "{selector} is drawn");
    }
    assert!(
        !drawn_now(&mut h, "chat-row-p1-i1"),
        "a closed script draws no inner card"
    );
    h.click_on("chat-script-p1-s1");
    assert!(painted(&mut h, "chat-row-p1-s1"), "the script's own card");
    assert!(painted(&mut h, "chat-row-p1-i1"), "its inner call's card");
    h.click_on("chat-script-p1-s1");
    assert!(
        !drawn_now(&mut h, "chat-row-p1-i1"),
        "a second click closes it"
    );
}

/// A done script call `id`, with a description but no number or time.
fn done_script(id: &str) -> ChatEntry {
    scripted(
        kinded(id, "REPL", ToolKind::Script, true),
        Some(ScriptFacts {
            description: format!("script {id}"),
            ..ScriptFacts::default()
        }),
        None,
    )
}

#[gpui::test]
fn a_standing_script_draws_as_its_line_in_comfortable(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    h.click_on("chat-compact-p1");
    assert_eq!(h.root(|root, _| root.chat_mode()), ChatMode::Comfortable);
    h.send(reply(
        Some(&id),
        vec![
            user("u1"),
            done_script("s1"),
            scripted(kinded("i1", "Read", ToolKind::Read, true), None, Some("s1")),
        ],
    ));
    assert!(painted(&mut h, "chat-script-p1-s1"), "the script's line");
    assert!(
        !drawn_now(&mut h, "chat-row-p1-i1"),
        "a closed script draws no inner card"
    );
    h.click_on("chat-script-p1-s1");
    assert!(painted(&mut h, "chat-row-p1-s1"), "the script's own card");
    assert!(painted(&mut h, "chat-row-p1-i1"), "its inner call's card");
}

#[gpui::test]
fn a_closed_script_keeps_its_tailing_inner_calls_card_in_sight(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    let mut tailing = scripted(kinded("b1", "Bash", ToolKind::Run, true), None, Some("s1"));
    if let ChatEntry::Tool { tail_paths, .. } = &mut tailing {
        tail_paths.push("build.log".to_owned());
    }
    h.send(reply(
        Some(&id),
        vec![
            user("u1"),
            done_script("s1"),
            tailing,
            scripted(kinded("r1", "Read", ToolKind::Read, true), None, Some("s1")),
        ],
    ));
    assert!(painted(&mut h, "chat-script-p1-s1"), "the script's line");
    assert!(
        painted(&mut h, "chat-row-p1-b1"),
        "the tailing call's card stays in sight"
    );
    assert!(
        !drawn_now(&mut h, "chat-row-p1-r1"),
        "the other inner call's card waits for the script to open"
    );
}

#[gpui::test]
fn compact_draws_a_tool_run_as_one_line(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    assert_eq!(h.root(|root, _| root.chat_mode()), ChatMode::Compact);
    h.send(reply(
        Some(&id),
        vec![
            user("u1"),
            kinded("r1", "Read", ToolKind::Read, true),
            kinded("g1", "Grep", ToolKind::Search, true),
            kinded("b1", "Bash", ToolKind::Run, false),
            answer("a1", "Done.", false),
        ],
    ));
    assert_eq!(
        display_rows(&mut h),
        [
            RowShape::Entry("u1".to_owned()),
            run_shape(
                &["r1", "g1", "b1"],
                "1 read \u{b7} 1 searched \u{b7} 1 ran",
                true
            ),
            RowShape::Entry("a1".to_owned()),
        ]
    );
    assert!(painted(&mut h, "chat-run-p1-r1"), "the run's line is drawn");
    assert!(
        painted(&mut h, "chat-run-running-p1-r1"),
        "a run with a call still running says so"
    );
    assert!(painted(&mut h, "chat-row-p1-a1"));
}

#[gpui::test]
fn clicking_a_run_line_shows_its_cards(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    h.send(reply(
        Some(&id),
        vec![
            kinded("r1", "Read", ToolKind::Read, true),
            kinded("g1", "Grep", ToolKind::Search, true),
        ],
    ));
    assert!(!h.root(|root, _| root.chat_run_open("s1", "r1")));
    h.click_on("chat-run-p1-r1");
    assert!(
        h.root(|root, _| root.chat_run_open("s1", "r1")),
        "the click opens the run"
    );
    for card in ["chat-tool-done-p1-r1", "chat-tool-done-p1-g1"] {
        assert!(painted(&mut h, card), "{card} is drawn under the line");
    }
    h.click_on("chat-run-p1-r1");
    assert!(
        !h.root(|root, _| root.chat_run_open("s1", "r1")),
        "a second click closes it"
    );
}

#[gpui::test]
fn the_compact_toggle_switches_to_one_card_per_call_and_saves(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    h.send(reply(
        Some(&id),
        vec![
            kinded("r1", "Read", ToolKind::Read, true),
            kinded("g1", "Grep", ToolKind::Search, true),
        ],
    ));
    assert!(
        painted(&mut h, "chat-compact-p1"),
        "the header offers the toggle"
    );
    h.click_on("chat-compact-p1");
    assert_eq!(h.root(|root, _| root.chat_mode()), ChatMode::Comfortable);
    assert_eq!(
        display_rows(&mut h),
        [
            RowShape::Entry("r1".to_owned()),
            RowShape::Entry("g1".to_owned())
        ]
    );
    for card in ["chat-tool-done-p1-r1", "chat-tool-done-p1-g1"] {
        assert!(painted(&mut h, card), "{card} is drawn as its own card");
    }
    assert_eq!(
        saved_chat_mode(&dir),
        "comfortable",
        "the switch is saved at once"
    );
    h.click_on("chat-compact-p1");
    assert_eq!(h.root(|root, _| root.chat_mode()), ChatMode::Compact);
    assert_eq!(saved_chat_mode(&dir), "compact");
    assert_eq!(
        display_rows(&mut h),
        [run_shape(&["r1", "g1"], "1 read \u{b7} 1 searched", false)]
    );
}

/// The widest the chat column draws by default, in logical px.
const CHAT_CAP: f32 = 960.0;

/// `p1`'s chat showing one finished turn, in a window `width` px wide.
fn chat_at_width<'a>(cx: &'a mut TestAppContext, dir: &TestDir, width: f32) -> Harness<'a> {
    let (mut h, id) = opened(cx, dir);
    h.send(reply(
        Some(&id),
        vec![
            user("u1"),
            answer("a1", "Done.", false),
            ChatEntry::TurnEnd {
                id: "e1".to_owned(),
                duration_ms: Some(1_000),
            },
        ],
    ));
    let height = h.window_height();
    h.cx.simulate_resize(gpui::size(px(width), px(height)));
    h.cx.run_until_parked();
    h
}

#[gpui::test]
fn a_wide_pane_caps_and_centres_the_chat_column(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_at_width(cx, &dir, 2400.0);
    let pane = h.bounds("pane-chat-p1");
    assert!(
        pane.size.width / px(1.0) > CHAT_CAP + 100.0,
        "the pane is wider than the cap: {pane:?}"
    );
    let column = h.bounds("chat-column-p1");
    assert!(
        (column.size.width / px(1.0) - CHAT_CAP).abs() < 0.5,
        "the column is capped: {column:?}"
    );
    let pane_centre = pane.origin.x / px(1.0) + pane.size.width / px(1.0) / 2.0;
    let column_centre = column.origin.x / px(1.0) + column.size.width / px(1.0) / 2.0;
    assert!(
        (pane_centre - column_centre).abs() < 0.5,
        "the column is centred: pane {pane:?}, column {column:?}"
    );
}

#[gpui::test]
fn a_narrow_pane_keeps_the_full_width(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_at_width(cx, &dir, 900.0);
    let pane = h.bounds("pane-chat-p1");
    assert!(
        pane.size.width / px(1.0) < CHAT_CAP,
        "the pane is narrower than the cap: {pane:?}"
    );
    let column = h.bounds("chat-column-p1");
    assert!(
        column.size.width > px(0.0),
        "the column is drawn: {column:?}"
    );
    assert!(
        (column.size.width / px(1.0) - pane.size.width / px(1.0)).abs() < 0.5
            && (column.origin.x / px(1.0) - pane.origin.x / px(1.0)).abs() < 0.5,
        "the column fills the pane: pane {pane:?}, column {column:?}"
    );
}

#[gpui::test]
fn a_long_paste_leaves_the_rows_after_it_in_reach(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    let height = h.window_height();
    h.cx.simulate_resize(gpui::size(px(1600.0), px(height)));
    h.cx.run_until_parked();
    let paste: String = (0..150)
        .map(|n| {
            format!(
                "Error (active)    CS0246    The type or namespace name 'Windows' could not be found \
                 (are you missing a using directive or an assembly reference?)    \
                 WorktreeSweep.Core    X:\\dev\\worktree-sweep\\src\\Core\\File{n}.cs    7"
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    h.send(reply(
        Some(&id),
        vec![
            ChatEntry::User {
                id: "u1".to_owned(),
                text: paste,
                pending: false,
            },
            answer("a1", "Looking.", false),
            user("u2"),
            answer("a2", "Done.", false),
        ],
    ));
    let pane = h.bounds("pane-chat-p1");
    assert!(
        pane.size.width / px(1.0) > CHAT_CAP + 100.0,
        "the pane is wider than the column's cap: {pane:?}"
    );
    // The chat follows its bottom; a long wheel down makes sure of it.
    h.cx.simulate_event(ScrollEvent {
        position: point(pane.center().x, pane.origin.y + px(120.0)),
        delta: ScrollDelta::Pixels(point(px(0.0), px(-1_000_000.0))),
        modifiers: Modifiers::none(),
        touch_phase: TouchPhase::Moved,
        source: gpui::ScrollSource::Pointer,
    });
    h.cx.run_until_parked();
    let column = h.bounds("chat-column-p1");
    let last = h.bounds("chat-row-p1-a2");
    assert!(
        last.bottom() <= column.bottom(),
        "the column's box holds its last row: column {column:?}, last row {last:?}"
    );
    assert!(
        last.bottom() <= pane.bottom(),
        "at the bottom of the chat the last row is in view: pane {pane:?}, last row {last:?}"
    );
}

#[gpui::test]
fn the_final_reply_draws_in_a_box(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    h.send(reply(
        Some(&id),
        vec![
            user("u1"),
            answer("a1", "Looking.", false),
            tool("t1", Some("fn main() {}")),
            answer("a2", "Done: **two** files.", false),
            ChatEntry::TurnEnd {
                id: "e1".to_owned(),
                duration_ms: Some(12_000),
            },
        ],
    ));
    assert_eq!(h.root(|root, _| root.chat_final_replies("s1")), ["a2"]);
    assert!(
        painted(&mut h, "chat-final-p1-a2"),
        "the turn's last answer draws in its box"
    );
    assert!(
        painted(&mut h, "chat-row-p1-a1"),
        "the earlier answer still draws"
    );
}

#[gpui::test]
fn a_running_turns_latest_answer_has_no_box(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    h.send(reply(
        Some(&id),
        vec![
            user("u1"),
            answer("a1", "Looking.", false),
            tool("t1", None),
            answer("a2", "Still going", true),
        ],
    ));
    assert!(painted(&mut h, "chat-row-p1-a2"), "the live answer draws");
    assert_eq!(
        h.root(|root, _| root.chat_final_replies("s1")),
        Vec::<String>::new(),
        "a turn still running has no final reply"
    );
    h.send(append(
        vec![ChatEntry::TurnEnd {
            id: "e1".to_owned(),
            duration_ms: Some(3_000),
        }],
        &[],
    ));
    assert_eq!(
        h.root(|root, _| root.chat_final_replies("s1")),
        ["a2"],
        "the turn's end makes its latest answer the final reply"
    );
    assert!(painted(&mut h, "chat-final-p1-a2"));
}

#[gpui::test]
fn a_call_with_tail_paths_keeps_its_card_in_compact(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    file_tail_daemon(&mut h);
    let mut tailing = tailing_tool("t1");
    if let ChatEntry::Tool { kind, .. } = &mut tailing {
        *kind = ToolKind::Run;
    }
    h.send(reply(
        Some(&id),
        vec![
            kinded("r1", "Read", ToolKind::Read, true),
            tailing,
            kinded("r2", "Read", ToolKind::Read, true),
        ],
    ));
    assert_eq!(
        display_rows(&mut h),
        [
            run_shape(&["r1"], "1 read", false),
            RowShape::Entry("t1".to_owned()),
            run_shape(&["r2"], "1 read", false),
        ]
    );
    assert!(
        painted(&mut h, "chat-tool-running-p1-t1"),
        "the call keeps its card"
    );
    assert!(painted(&mut h, TAIL_BUTTON), "and its Tail button");
}

#[gpui::test]
fn an_edit_ends_a_run_and_keeps_its_card(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    h.send(reply(
        Some(&id),
        vec![
            kinded("r1", "Read", ToolKind::Read, true),
            kinded("e1", "Edit", ToolKind::Other, true),
            kinded("r2", "Read", ToolKind::Read, true),
        ],
    ));
    assert_eq!(
        display_rows(&mut h),
        [
            run_shape(&["r1"], "1 read", false),
            RowShape::Entry("e1".to_owned()),
            run_shape(&["r2"], "1 read", false),
        ]
    );
    assert!(painted(&mut h, "chat-run-p1-r1"));
    assert!(
        painted(&mut h, "chat-tool-done-p1-e1"),
        "the edit keeps its card"
    );
    assert!(painted(&mut h, "chat-run-p1-r2"));
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
fn restart_keeps_the_pane_on_chat(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir);
    h.click_on(TO_CHAT);
    let loaded = load_request(&h.sent()).expect("the toggle asks for the chat");
    h.send(reply(Some(&loaded), vec![user("u1")]));
    h.sent();

    h.send(DaemonMessage::SessionUpdated {
        session: session("s1").exited(0).build(),
        request_id: None,
    });
    h.sent();
    h.click_on("exited-restart-p1");
    let restart = h
        .sent()
        .into_iter()
        .find_map(|msg| match msg {
            ClientMessage::DuplicateSession {
                session_id,
                request_id,
                ..
            } if session_id == "s1" => request_id,
            _ => None,
        })
        .expect("a restart of s1 with a request id");
    h.send(DaemonMessage::SessionUpdated {
        session: session("s1b").build(),
        request_id: Some(restart),
    });
    h.sent();

    h.send(DaemonMessage::TabUpdated {
        tab: tab("t1", &pane("p1", Some("s1b"))),
    });
    let sent = h.sent();
    assert!(
        h.root(|root, _| root.pane_shows_chat("p1")),
        "the restarted session's pane opens on its chat"
    );
    assert!(
        sent.iter().any(|msg| matches!(
            msg,
            ClientMessage::LoadChat { session_id, .. } if session_id == "s1b"
        )),
        "the copy's chat is asked for: {sent:?}"
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
        adjust: None,
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
    h.root(|root, _| root.chat_composer_actions("s1"))
}

/// The current card's prompt id and whether it is busy.
fn card(h: &mut Harness<'_>) -> Option<(String, bool)> {
    h.root(|root, _| root.chat_card("s1"))
}

/// The text in the current card's box; none while the card has no box.
fn box_text(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, cx| root.chat_card_box_text("s1", cx))
}

/// A single-select question whose third option takes typed text.
fn type_something(id: &str) -> ChatPrompt {
    prompt(
        id,
        ChatPromptKind::Question,
        &[(1, "sub"), (2, "div"), (3, "Type something")],
    )
}

fn typed(text: &str) -> ChatAnswerKind {
    ChatAnswerKind::TypeSomething {
        text: text.to_owned(),
    }
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
fn an_mcp_server_card_offers_its_three_options_and_sends_the_pick(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    let options = [
        (1, "Use this MCP server"),
        (2, "Use this and all future MCP servers in this project"),
        (3, "Continue without using this MCP server"),
    ];
    for (id, button) in [
        ("m1", "chat-card-1"),
        ("m2", "chat-card-2"),
        ("m3", "chat-card-3"),
    ] {
        show(&mut h, prompt(id, ChatPromptKind::McpServer, &options));
        h.click_on(button);
    }
    assert_eq!(answer_kinds(&mut h), [opt(1), opt(2), opt(3)]);
}

#[gpui::test]
fn a_model_picker_card_sends_the_picked_row(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    let mut picker = prompt(
        "m1",
        ChatPromptKind::ModelPicker,
        &[
            (1, "Default (recommended)"),
            (2, "Opus 5.5"),
            (3, "Fable 5.2"),
            (4, "Sonnet 5.5"),
            (5, "Haiku 5.5"),
            (6, "More models"),
        ],
    );
    picker.subject = "Select model".to_owned();
    picker.cursor = Some(1);
    picker.options[0].description =
        Some("Current · Opus 5.5 · Best for everyday, complex tasks".to_owned());
    picker.options[1].description = Some("For complex work and everyday tasks".to_owned());
    show(&mut h, picker);
    for button in [
        "chat-card-1",
        "chat-card-2",
        "chat-card-3",
        "chat-card-4",
        "chat-card-5",
    ] {
        assert!(painted(&mut h, button), "{button}");
    }
    assert!(
        !painted(&mut h, "chat-card-6"),
        "More models gets no button"
    );
    h.click_on("chat-card-4");
    assert_eq!(answer_kinds(&mut h), [opt(4)]);
}

#[gpui::test]
fn an_overlay_card_closes_it_and_disables_the_composer(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    h.click_on("chat-composer");
    type_text(&mut h, "careful");
    let mut close = prompt("o1", ChatPromptKind::Overlay, &[(1, "Close")]);
    close.subject = "Rewind".to_owned();
    close.body = String::new();
    close.can_amend = false;
    show_screen(
        &mut h,
        ComposerState::Overlay {
            name: "rewind".to_owned(),
        },
        Some(close),
    );
    assert!(painted(&mut h, "chat-card-1"), "the Close button");
    assert!(!painted(&mut h, "chat-card-2"), "Close is the only button");
    assert_eq!(
        note(&mut h).as_deref(),
        Some("Rewind is open in the terminal. Close it to send.")
    );
    h.sent();
    h.click_on("chat-send");
    h.keys("ctrl-enter");
    assert!(
        sends(&mut h).is_empty(),
        "Send is disabled while the overlay is up"
    );
    h.click_on("chat-card-1");
    assert_eq!(answer_kinds(&mut h), [opt(1)]);
}

#[gpui::test]
fn the_effort_buttons_send_adjust_answers(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    let mut picker = prompt(
        "m1",
        ChatPromptKind::ModelPicker,
        &[
            (1, "Default (recommended)"),
            (2, "Opus 5.5"),
            (3, "More models"),
        ],
    );
    picker.subject = "Select model".to_owned();
    picker.cursor = Some(1);
    picker.options[0].selected = true;
    picker.adjust = Some("High effort".to_owned());
    show(&mut h, picker);
    assert!(painted(&mut h, "chat-card-adjust"), "the effort label");
    h.click_on("chat-card-adjust-up");
    assert_eq!(
        answer_kinds(&mut h),
        [ChatAnswerKind::Adjust { raise: true }]
    );
    h.click_on("chat-card-adjust-down");
    assert_eq!(
        answer_kinds(&mut h),
        [ChatAnswerKind::Adjust { raise: false }]
    );
}

#[gpui::test]
fn a_switch_model_card_sends_no_go_back(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    show(
        &mut h,
        prompt(
            "m2",
            ChatPromptKind::Confirm,
            &[(1, "Yes, switch to Opus 5.5"), (2, "No, go back")],
        ),
    );
    h.click_on("chat-card-2");
    assert_eq!(answer_kinds(&mut h), [opt(2)]);
}

#[gpui::test]
fn an_auto_mode_card_offers_its_two_options_and_sends_the_pick(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    let options = [
        (1, "Yes, set auto mode as my default permission mode"),
        (2, "No, keep manual mode"),
    ];
    for (id, button) in [("a1", "chat-card-1"), ("a2", "chat-card-2")] {
        show(&mut h, prompt(id, ChatPromptKind::Confirm, &options));
        h.click_on(button);
    }
    assert_eq!(answer_kinds(&mut h), [opt(1), opt(2)]);
}

#[gpui::test]
fn an_mcp_servers_card_toggles_each_server_and_enables_the_selected(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    let mut servers = prompt(
        "s1",
        ChatPromptKind::McpServer,
        &[(1, "rt-driver"), (2, "alpha-notes"), (3, "beta-search")],
    );
    servers.multi = true;
    for (id, button) in [
        ("s1", "chat-card-1"),
        ("s2", "chat-card-2"),
        ("s3", "chat-card-3"),
        ("s4", "chat-card-submit"),
    ] {
        servers.id = id.to_owned();
        show(&mut h, servers.clone());
        h.click_on(button);
    }
    assert_eq!(
        answer_kinds(&mut h),
        [
            ChatAnswerKind::Toggle { number: 1 },
            ChatAnswerKind::Toggle { number: 2 },
            ChatAnswerKind::Toggle { number: 3 },
            ChatAnswerKind::Submit,
        ]
    );
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
    h.click_on("chat-card-box");
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
fn a_question_cards_box_sends_its_typed_answer(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    show(&mut h, type_something("q1"));
    h.advance(Duration::from_millis(500));
    h.click_on("chat-card-box");
    type_text(&mut h, "use 2 lines");
    assert_eq!(
        box_text(&mut h).as_deref(),
        Some("use 2 lines"),
        "a digit typed into the box types it"
    );
    h.keys("enter");
    assert_eq!(answer_kinds(&mut h), [typed("use 2 lines")]);
    assert_eq!(
        box_text(&mut h).as_deref(),
        Some(""),
        "the text leaves the box"
    );
    assert_eq!(composer_text(&mut h).as_deref(), Some(""));
}

#[gpui::test]
fn a_blank_box_option_focuses_the_box(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    show(&mut h, type_something("q1"));
    h.click_on("chat-card-3");
    assert_eq!(answer_kinds(&mut h), [], "a blank box sends nothing");
    type_text(&mut h, "mul");
    assert_eq!(
        box_text(&mut h).as_deref(),
        Some("mul"),
        "the press gave the box the keyboard"
    );
    assert_eq!(composer_text(&mut h).as_deref(), Some(""));
    h.advance(Duration::from_millis(500));
    h.click_on("chat-composer");
    h.keys("3");
    assert_eq!(
        answer_kinds(&mut h),
        [typed("mul")],
        "the option's digit sends the box's text"
    );
}

#[gpui::test]
fn a_cards_box_holds_its_answer_while_a_send_is_in_flight(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    h.click_on("chat-composer");
    type_text(&mut h, "first");
    h.keys("ctrl-enter");
    assert_eq!(sends(&mut h).len(), 1);
    show(&mut h, type_something("q1"));
    h.advance(Duration::from_millis(500));
    h.click_on("chat-card-box");
    type_text(&mut h, "mul");
    h.keys("enter");
    assert_eq!(answer_kinds(&mut h), [], "Enter waits for the send");
    assert_eq!(
        box_text(&mut h).as_deref(),
        Some("mul"),
        "the box keeps its text"
    );
    h.click_on("chat-card-3");
    assert_eq!(
        answer_kinds(&mut h),
        [],
        "the option's button waits for the send"
    );
    h.click_on("chat-composer");
    h.keys("3");
    assert_eq!(
        answer_kinds(&mut h),
        [],
        "the option's digit waits for the send"
    );
    assert_eq!(
        box_text(&mut h).as_deref(),
        Some("mul"),
        "the box still keeps its text"
    );
    show_screen(&mut h, ComposerState::Working, Some(type_something("q1")));
    h.click_on("chat-card-box");
    h.keys("enter");
    assert_eq!(
        answer_kinds(&mut h),
        [typed("mul")],
        "the answer goes once the send settled"
    );
}

#[gpui::test]
fn escape_in_a_card_box_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    show(&mut h, type_something("q1"));
    h.click_on("chat-card-box");
    type_text(&mut h, "mul");
    h.sent();
    h.keys("escape");
    assert!(h.sent().is_empty(), "Esc in the box sends nothing");
    type_text(&mut h, "x");
    assert_eq!(
        composer_text(&mut h).as_deref(),
        Some("x"),
        "the composer has the keyboard"
    );
    assert_eq!(
        box_text(&mut h).as_deref(),
        Some("mul"),
        "the box keeps its text"
    );
}

#[gpui::test]
fn a_refused_typed_answer_goes_back_to_its_box(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    show(&mut h, type_something("q1"));
    h.click_on("chat-card-box");
    type_text(&mut h, "mul");
    h.keys("enter");
    let request_id = answers(&mut h)[0].2.clone();
    type_text(&mut h, "more");
    refused(&mut h, request_id, "the dialog moved");
    assert_eq!(
        box_text(&mut h).as_deref(),
        Some("mul more"),
        "the text goes back before what was typed since"
    );
    assert_eq!(
        composer_text(&mut h).as_deref(),
        Some(""),
        "the composer is left alone"
    );
    assert_eq!(
        h.root(|root, _| root.chat_card_refusal("s1")).as_deref(),
        Some("the dialog moved")
    );
}

#[gpui::test]
fn a_new_card_starts_with_an_empty_box(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    show(&mut h, type_something("q1"));
    h.click_on("chat-card-box");
    type_text(&mut h, "mul");
    show(&mut h, type_something("q1"));
    assert_eq!(
        box_text(&mut h).as_deref(),
        Some("mul"),
        "the same card redrawn keeps its box"
    );
    show(&mut h, type_something("q2"));
    assert_eq!(
        box_text(&mut h).as_deref(),
        Some(""),
        "a new card starts empty"
    );
}

#[gpui::test]
fn a_plan_cards_box_sends_its_feedback(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
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
    assert!(
        painted(&mut h, "chat-card-box"),
        "the feedback row has a box"
    );
    h.click_on("chat-card-box");
    type_text(&mut h, "split it");
    h.keys("enter");
    assert_eq!(answer_kinds(&mut h), [typed("split it")]);
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

/// The one Esc byte an interrupt sends to a session.
fn interrupt_byte() -> Vec<u8> {
    b"\x1b".to_vec()
}

#[gpui::test]
fn escape_interrupts_a_running_turn(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    h.click_on("chat-composer");
    show_screen(&mut h, ComposerState::Working, None);
    h.sent();
    h.keys("escape");
    assert_eq!(
        h.sent_input("s1"),
        interrupt_byte(),
        "Esc in the composer interrupts the turn"
    );
}

#[gpui::test]
fn escape_on_a_focused_chat_pane_interrupts(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    show_screen(&mut h, ComposerState::Working, None);
    h.sent();
    h.keys("escape");
    assert_eq!(
        h.sent_input("s1"),
        interrupt_byte(),
        "the pane holds the keys with the composer unfocused"
    );
}

#[gpui::test]
fn a_second_escape_waits_for_the_screen(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    show_screen(&mut h, ComposerState::Working, None);
    h.sent();
    h.keys("escape");
    h.keys("escape");
    assert_eq!(h.sent_input("s1"), interrupt_byte(), "one Esc per turn");
    show_screen(&mut h, ComposerState::Working, None);
    h.keys("escape");
    assert_eq!(
        h.sent_input("s1"),
        interrupt_byte(),
        "a new screen reading lets the next Esc through"
    );
}

#[gpui::test]
fn escape_while_ready_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    h.click_on("chat-composer");
    h.sent();
    h.keys("escape");
    assert!(h.sent_input("s1").is_empty(), "no turn runs to interrupt");
}

#[gpui::test]
fn escape_spent_on_an_armed_stop_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    show_screen(&mut h, ComposerState::Working, None);
    h.sent();
    h.click_on("pane-stop-p1");
    assert!(h.in_model("pane-stop-confirm-p1"), "one click arms Stop");
    h.keys("escape");
    assert!(
        h.sent_input("s1").is_empty(),
        "the Esc was spent on the arm"
    );
    assert!(!h.in_model("pane-stop-confirm-p1"), "the arm is disarmed");
}

#[gpui::test]
fn escape_in_the_composer_disarms_a_stop_first(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    show_screen(&mut h, ComposerState::Working, None);
    h.sent();
    h.click_on("chat-composer");
    h.click_on("pane-stop-p1");
    assert!(h.in_model("pane-stop-confirm-p1"), "one click arms Stop");
    h.keys("escape");
    assert!(
        h.sent_input("s1").is_empty(),
        "the Esc dropped the armed Stop instead"
    );
    assert!(!h.in_model("pane-stop-confirm-p1"), "the arm is disarmed");
}

#[gpui::test]
fn escape_that_closes_a_menu_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    show_screen(&mut h, ComposerState::Working, None);
    h.sent();
    assert_eq!(menu_label(&mut h).as_deref(), Some("Show terminal"));
    h.keys("escape");
    assert!(
        h.sent_input("s1").is_empty(),
        "the Esc closed the pane menu"
    );
    assert!(
        h.root(|root, _| root.session_menu().is_none()),
        "the menu is gone"
    );
}

/// Whether the Source Control panel holds the window's keyboard.
fn source_control_focused(h: &mut Harness<'_>) -> bool {
    let root = h.root.clone();
    h.cx.update(|window, cx| root.read(cx).sidebar_focused(window))
}

#[gpui::test]
fn escape_outside_the_chat_pane_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached_in_a_repo(cx, &dir);
    h.click_on(TO_CHAT);
    let id = load_request(&h.sent()).expect("the toggle asks for the chat");
    h.send(reply(Some(&id), vec![]));
    show_screen(&mut h, ComposerState::Working, None);
    h.click_on("activity-source-control");
    h.click_on("sc-panel");
    assert!(
        source_control_focused(&mut h),
        "the Source Control panel took the keyboard"
    );
    h.sent();
    h.keys("escape");
    assert!(
        h.sent_input("s1").is_empty(),
        "the keyboard is in the Source Control panel"
    );
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

// The composer's completion popup, over a fixed list.

/// `p1`'s chat loaded, its composer offering `clear` and `compact` and
/// holding the keyboard.
fn completing<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = loaded(cx, dir);
    h.root_update(|root, _, cx| {
        root.set_chat_completion_items(
            "s1",
            vec![
                CompletionItem::new("clear", "/clear"),
                CompletionItem::new("compact", "/compact"),
            ],
            cx,
        );
    });
    h.click_on("chat-composer");
    h.sent();
    h
}

/// The popup's rows and selected row, while it shows.
fn completion(h: &mut Harness<'_>) -> Option<(Vec<String>, usize)> {
    h.root(|root, _| root.chat_completion("s1"))
}

fn both_rows(selected: usize) -> (Vec<String>, usize) {
    (vec!["clear".to_owned(), "compact".to_owned()], selected)
}

#[gpui::test]
fn typing_a_slash_opens_the_popup_and_tab_inserts_the_pick(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = completing(cx, &dir);
    assert!(
        !painted(&mut h, "chat-completion"),
        "no popup before a token"
    );
    type_text(&mut h, "/");
    assert_eq!(completion(&mut h), Some(both_rows(0)));
    assert!(painted(&mut h, "chat-completion"));
    assert!(painted(&mut h, "chat-completion-1"));
    h.keys("down");
    assert_eq!(completion(&mut h), Some(both_rows(1)));
    h.keys("down");
    assert_eq!(
        completion(&mut h),
        Some(both_rows(0)),
        "Down wraps to the first"
    );
    h.keys("up");
    assert_eq!(
        completion(&mut h),
        Some(both_rows(1)),
        "Up wraps to the last"
    );
    h.keys("tab");
    assert_eq!(composer_text(&mut h).as_deref(), Some("/compact "));
    assert_eq!(completion(&mut h), None, "the pick closes it");
    assert!(!painted(&mut h, "chat-completion"));
    assert_eq!(sends(&mut h), Vec::new());
}

#[gpui::test]
fn enter_accepts_the_pick_and_does_not_send(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = completing(cx, &dir);
    type_text(&mut h, "/c");
    h.keys("enter");
    assert_eq!(composer_text(&mut h).as_deref(), Some("/clear "));
    assert_eq!(sends(&mut h), Vec::new(), "Enter accepts, it does not send");
    h.keys("enter");
    assert_eq!(
        composer_text(&mut h).as_deref(),
        Some("/clear \n"),
        "closed, Enter breaks the line"
    );
    type_text(&mut h, "/");
    h.click_on("chat-completion-1");
    assert_eq!(
        composer_text(&mut h).as_deref(),
        Some("/clear \n/compact "),
        "a click accepts its row, the text before the token kept"
    );
    assert_eq!(completion(&mut h), None);
    type_text(&mut h, "x");
    assert_eq!(
        composer_text(&mut h).as_deref(),
        Some("/clear \n/compact x"),
        "the composer keeps the keyboard after the click"
    );
}

#[gpui::test]
fn esc_closes_the_popup_and_keeps_the_text(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = completing(cx, &dir);
    type_text(&mut h, "/co");
    assert_eq!(completion(&mut h), Some(both_rows(0)));
    h.keys("escape");
    assert_eq!(completion(&mut h), None);
    assert_eq!(composer_text(&mut h).as_deref(), Some("/co"));
    type_text(&mut h, "m");
    assert_eq!(completion(&mut h), None, "it stays shut in the same token");
    h.keys("enter");
    assert_eq!(composer_text(&mut h).as_deref(), Some("/com\n"));
    type_text(&mut h, "/");
    assert_eq!(
        completion(&mut h),
        Some(both_rows(0)),
        "a new token opens it again"
    );
    assert_eq!(sends(&mut h), Vec::new());
}

#[gpui::test]
fn ctrl_enter_sends_while_the_popup_is_open(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = completing(cx, &dir);
    type_text(&mut h, "/co");
    assert_eq!(completion(&mut h), Some(both_rows(0)));
    h.keys("ctrl-enter");
    let sent = sends(&mut h);
    assert_eq!(sent.len(), 1, "one send: {sent:?}");
    assert_eq!(sent[0].0, "/co");
    assert_eq!(composer_text(&mut h).as_deref(), Some(""));
    assert_eq!(
        completion(&mut h),
        None,
        "the cleared composer has no token"
    );
}

#[gpui::test]
fn a_restored_draft_ending_in_a_token_opens_the_popup(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    h.click_on("chat-composer");
    type_text(&mut h, "/co");
    h.click_on(TO_TERMINAL);
    h.click_on(TO_CHAT);
    let id = load_request(&h.sent()).expect("the toggle asks for the chat again");
    h.send(reply(Some(&id), vec![]));
    assert_eq!(
        composer_text(&mut h).as_deref(),
        Some("/co"),
        "the draft is back"
    );
    h.root_update(|root, _, cx| {
        root.set_chat_completion_items(
            "s1",
            vec![
                CompletionItem::new("clear", "/clear"),
                CompletionItem::new("compact", "/compact"),
            ],
            cx,
        );
    });
    assert_eq!(
        completion(&mut h),
        Some(both_rows(0)),
        "the restored draft's token opens the popup"
    );
    assert!(painted(&mut h, "chat-completion"));
}

#[gpui::test]
fn an_at_after_a_word_opens_no_popup(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = completing(cx, &dir);
    type_text(&mut h, "me@sr");
    assert_eq!(completion(&mut h), None, "an email address is no token");
    assert!(!painted(&mut h, "chat-completion"));
    type_text(&mut h, " @s");
    assert_eq!(
        completion(&mut h),
        Some(both_rows(0)),
        "an @ after a space is"
    );
}

// The composer's `@` popup, over the session's files.

/// `s1`'s chat loaded on a host offering file search, its composer
/// holding the keyboard, everything sent so far drained.
fn file_searching<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = loaded_listing(cx, dir, &[FEATURE_FILE_SEARCH]);
    h.click_on("chat-composer");
    h.sent();
    h
}

/// Moves the test clock on by `ms` and lets what it woke run.
fn advance(h: &mut Harness<'_>, ms: u64) {
    h.cx.executor().advance_clock(Duration::from_millis(ms));
    h.cx.run_until_parked();
}

/// Moves the test clock past the file search's debounce.
fn past_debounce(h: &mut Harness<'_>) {
    let ms = u64::try_from(chat::FILE_SEARCH_DEBOUNCE.as_millis()).unwrap_or(u64::MAX);
    advance(h, ms + 10);
}

/// The query, limit and request id of each `SearchFiles` for `s1` sent
/// since the last drain.
fn searches(h: &mut Harness<'_>) -> Vec<(String, Option<u32>, Option<String>)> {
    h.sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::SearchFiles {
                session_id,
                query,
                limit,
                request_id,
            } if session_id == "s1" => Some((query, limit, request_id)),
            _ => None,
        })
        .collect()
}

/// The one search sent since the last drain: its query and request id.
fn one_search(h: &mut Harness<'_>) -> (String, Option<String>) {
    let sent = searches(h);
    assert_eq!(sent.len(), 1, "one search: {sent:?}");
    let (query, limit, request_id) = sent[0].clone();
    assert_eq!(limit, Some(50));
    (query, request_id)
}

/// The daemon answers the search `request_id` with `files`, each a path
/// and its reference.
fn matched(h: &mut Harness<'_>, request_id: Option<String>, files: &[(&str, &str)]) {
    h.send(DaemonMessage::FileMatches {
        session_id: "s1".to_owned(),
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

/// The popup showing `labels`, the first selected.
fn rows(labels: &[&str]) -> (Vec<String>, usize) {
    (labels.iter().map(|label| (*label).to_owned()).collect(), 0)
}

#[gpui::test]
fn at_popup_lists_matching_files_and_inserts_the_reference(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = file_searching(cx, &dir);
    type_text(&mut h, "@src/ma");
    assert_eq!(searches(&mut h), Vec::new(), "nothing inside the debounce");
    past_debounce(&mut h);
    let (query, id) = one_search(&mut h);
    assert_eq!(query, "src/ma");
    matched(
        &mut h,
        id,
        &[
            ("src/main.rs", "@./src/main.rs"),
            ("src/map.rs", "@./src/map.rs"),
        ],
    );
    assert_eq!(
        completion(&mut h),
        Some(rows(&["src/main.rs", "src/map.rs"]))
    );
    assert!(painted(&mut h, "chat-completion"));
    h.keys("enter");
    assert_eq!(
        composer_text(&mut h).as_deref(),
        Some("@./src/main.rs "),
        "the pick inserts the daemon's reference"
    );
    assert_eq!(sends(&mut h), Vec::new());
}

#[gpui::test]
fn typing_within_the_debounce_sends_one_search(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = file_searching(cx, &dir);
    type_text(&mut h, "@");
    advance(&mut h, 50);
    type_text(&mut h, "s");
    advance(&mut h, 50);
    type_text(&mut h, "r");
    assert_eq!(searches(&mut h), Vec::new(), "nothing inside the debounce");
    past_debounce(&mut h);
    let (query, _) = one_search(&mut h);
    assert_eq!(query, "sr", "the last query alone");
    past_debounce(&mut h);
    assert_eq!(searches(&mut h), Vec::new(), "no second search");
}

#[gpui::test]
fn a_stale_file_answer_is_ignored(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = file_searching(cx, &dir);
    type_text(&mut h, "@s");
    past_debounce(&mut h);
    let (_, older) = one_search(&mut h);
    type_text(&mut h, "r");
    past_debounce(&mut h);
    let (_, newer) = one_search(&mut h);
    matched(&mut h, older, &[("src/old.rs", "@./src/old.rs")]);
    assert_eq!(completion(&mut h), None, "an older search's answer");
    matched(&mut h, newer, &[("src/lib.rs", "@./src/lib.rs")]);
    assert_eq!(completion(&mut h), Some(rows(&["src/lib.rs"])));

    type_text(&mut h, " @x");
    past_debounce(&mut h);
    let (_, left) = one_search(&mut h);
    type_text(&mut h, " ");
    matched(&mut h, left, &[("x.rs", "@./x.rs")]);
    type_text(&mut h, "@");
    assert_eq!(
        completion(&mut h),
        None,
        "an answer after the caret left its token"
    );
    assert_eq!(composer_text(&mut h).as_deref(), Some("@sr @x @"));
}

#[gpui::test]
fn a_host_without_file_search_opens_no_popup_and_sends_no_request(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded_listing(cx, &dir, &[]);
    h.click_on("chat-composer");
    h.sent();
    type_text(&mut h, "@src");
    past_debounce(&mut h);
    assert_eq!(searches(&mut h), Vec::new());
    assert_eq!(completion(&mut h), None);
    assert!(!painted(&mut h, "chat-completion"));
}

#[gpui::test]
fn a_restored_draft_in_an_at_token_searches(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = file_searching(cx, &dir);
    type_text(&mut h, "@src");
    h.click_on(TO_TERMINAL);
    h.click_on(TO_CHAT);
    let id = load_request(&h.sent()).expect("the toggle asks for the chat again");
    h.send(reply(Some(&id), vec![]));
    assert_eq!(
        composer_text(&mut h).as_deref(),
        Some("@src"),
        "the draft is back"
    );
    past_debounce(&mut h);
    let (query, _) = one_search(&mut h);
    assert_eq!(query, "src");
}

#[gpui::test]
fn a_search_due_after_the_host_lost_file_search_is_not_sent(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = file_searching(cx, &dir);
    type_text(&mut h, "@src");
    h.reconnect_with_features(&[]);
    past_debounce(&mut h);
    assert_eq!(searches(&mut h), Vec::new());
}

#[gpui::test]
fn leaving_the_at_token_drops_the_file_rows(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = file_searching(cx, &dir);
    type_text(&mut h, "@ma");
    past_debounce(&mut h);
    let (_, id) = one_search(&mut h);
    matched(&mut h, id, &[("main.rs", "@./main.rs")]);
    assert_eq!(completion(&mut h), Some(rows(&["main.rs"])));
    type_text(&mut h, " ");
    assert_eq!(completion(&mut h), None);
    h.keys("backspace");
    assert_eq!(
        completion(&mut h),
        None,
        "back in the token, the file rows are gone"
    );
    assert!(!painted(&mut h, "chat-completion"));

    h.root_update(|root, _, cx| {
        root.set_chat_completion_items("s1", vec![CompletionItem::new("clear", "/clear")], cx);
    });
    assert_eq!(completion(&mut h), Some(rows(&["clear"])));
    type_text(&mut h, " ");
    type_text(&mut h, "@");
    assert_eq!(
        completion(&mut h),
        Some(rows(&["clear"])),
        "items given from elsewhere stay"
    );
}

// The composer's `/` popup, over the session's commands and skills.

/// `s1`'s chat loaded on a host offering the command catalog, its
/// composer holding the keyboard, everything sent so far drained.
fn cataloging<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = loaded_listing(cx, dir, &[FEATURE_CHAT_CATALOG]);
    h.click_on("chat-composer");
    h.sent();
    h
}

/// The request ids of the `ListCatalog`s for `s1` sent since the last
/// drain.
fn catalog_requests(h: &mut Harness<'_>) -> Vec<Option<String>> {
    h.sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::ListCatalog {
                session_id,
                request_id,
            } if session_id == "s1" => Some(request_id),
            _ => None,
        })
        .collect()
}

/// The one `ListCatalog` sent since the last drain: its request id.
fn one_catalog_request(h: &mut Harness<'_>) -> Option<String> {
    let sent = catalog_requests(h);
    assert_eq!(sent.len(), 1, "one catalog request: {sent:?}");
    assert!(sent[0].is_some(), "it carries a request id");
    sent[0].clone()
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

/// The host's commands: `compact`, `config` (an overlay) and
/// `code-review`, in that order.
fn host_commands() -> Vec<CatalogEntry> {
    vec![
        command("compact", Some("Compress the conversation"), false),
        command("config", None, true),
        command("code-review", Some("Review the branch"), false),
    ]
}

/// The popup's rows for [`host_commands`] on `/co`.
fn host_command_rows() -> (Vec<String>, usize) {
    rows(&[
        "/compact \u{b7} Compress the conversation",
        "/config \u{b7} opens in the terminal",
        "/code-review \u{b7} Review the branch",
    ])
}

/// The daemon answers the catalog request `request_id` with `entries`.
fn cataloged(h: &mut Harness<'_>, request_id: Option<String>, entries: Vec<CatalogEntry>) {
    h.send(DaemonMessage::Catalog {
        session_id: "s1".to_owned(),
        entries,
        request_id,
    });
}

#[gpui::test]
fn slash_popup_lists_the_hosts_commands_and_inserts_the_pick(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = cataloging(cx, &dir);
    type_text(&mut h, "/co");
    let id = one_catalog_request(&mut h);
    assert_eq!(completion(&mut h), None, "nothing before the answer");
    cataloged(&mut h, id, host_commands());
    assert_eq!(completion(&mut h), Some(host_command_rows()));
    assert!(painted(&mut h, "chat-completion"));
    h.keys("enter");
    assert_eq!(
        composer_text(&mut h).as_deref(),
        Some("/compact "),
        "the pick inserts the command's name"
    );
    assert_eq!(sends(&mut h), Vec::new(), "a pick only inserts");
}

#[gpui::test]
fn a_second_slash_uses_the_cached_catalog(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = cataloging(cx, &dir);
    type_text(&mut h, "/co");
    let id = one_catalog_request(&mut h);
    cataloged(&mut h, id, host_commands());
    h.keys("enter");
    h.keys("enter");
    type_text(&mut h, "/conf");
    assert_eq!(catalog_requests(&mut h), Vec::new(), "no second request");
    assert_eq!(
        completion(&mut h),
        Some(rows(&["/config \u{b7} opens in the terminal"]))
    );
    assert_eq!(composer_text(&mut h).as_deref(), Some("/compact \n/conf"));
}

#[gpui::test]
fn a_host_without_the_catalog_opens_no_popup_and_sends_no_request(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded_listing(cx, &dir, &[]);
    h.click_on("chat-composer");
    h.sent();
    type_text(&mut h, "/co");
    assert_eq!(catalog_requests(&mut h), Vec::new());
    assert_eq!(completion(&mut h), None);
    assert!(!painted(&mut h, "chat-completion"));
}

#[gpui::test]
fn a_stale_catalog_answer_is_ignored(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = cataloging(cx, &dir);
    type_text(&mut h, "/co");
    let id = one_catalog_request(&mut h);
    cataloged(&mut h, Some("another-request".to_owned()), host_commands());
    assert_eq!(completion(&mut h), None, "another request's answer");
    cataloged(&mut h, None, host_commands());
    assert_eq!(completion(&mut h), None, "an answer naming no request");
    cataloged(&mut h, id.clone(), host_commands());
    assert_eq!(completion(&mut h), Some(host_command_rows()));
    cataloged(&mut h, id, vec![command("clear", None, false)]);
    assert_eq!(
        completion(&mut h),
        Some(host_command_rows()),
        "an answer applies once"
    );
}

#[gpui::test]
fn a_restarted_chat_refetches_the_catalog(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = cataloging(cx, &dir);
    type_text(&mut h, "/co");
    let first = one_catalog_request(&mut h);
    cataloged(&mut h, first.clone(), host_commands());
    assert_eq!(completion(&mut h), Some(host_command_rows()));
    h.send(reply(None, vec![]));
    type_text(&mut h, " ");
    assert_eq!(completion(&mut h), None, "leaving the token drops the rows");
    h.keys("enter");
    type_text(&mut h, "/co");
    let second = one_catalog_request(&mut h);
    assert_ne!(second, first);
    cataloged(&mut h, first, host_commands());
    assert_eq!(completion(&mut h), None, "the old request's answer");
    cataloged(&mut h, second, vec![command("compact", None, false)]);
    assert_eq!(completion(&mut h), Some(rows(&["/compact"])));
}

/// Options whose third field is the description drawn under the label.
fn described(list: &[(u32, &str, Option<&str>)]) -> Vec<ChatOption> {
    list.iter()
        .map(|(number, label, description)| ChatOption {
            number: *number,
            label: (*label).to_owned(),
            description: description.map(str::to_owned),
            selected: false,
        })
        .collect()
}

/// One tab per label; the tab at `current` is the shown question and those
/// before it are answered.
fn tabs(labels: &[&str], current: usize) -> Vec<ChatQuestionTab> {
    labels
        .iter()
        .enumerate()
        .map(|(index, label)| ChatQuestionTab {
            label: (*label).to_owned(),
            answered: index < current,
            current: index == current,
        })
        .collect()
}

/// A single question `q1` whose subject wraps over several lines, `sub`
/// described and `div` not.
fn described_question() -> ChatPrompt {
    let mut question = ChatPrompt {
        subject: "Which of these functions should the new tests cover first, \
            given that the release goes out to the users on the beta channel tomorrow \
            morning and the integer helpers changed the most since the last one, \
            while the floating point helpers only moved files and kept their bodies?"
            .to_owned(),
        body: String::new(),
        ..prompt("q1", ChatPromptKind::Question, &[])
    };
    question.options = described(&[
        (
            1,
            "sub",
            Some("Subtraction: returns a - b, and raises on overflow"),
        ),
        (2, "div", None),
    ]);
    question.questions = tabs(&["Op"], 0);
    question
}

#[gpui::test]
fn a_question_card_shows_the_whole_question_option_descriptions_and_the_header_chip(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    show(&mut h, described_question());
    assert!(
        painted(&mut h, "chat-card-header"),
        "the header chip is drawn"
    );
    let (subject, chip) = (h.bounds("chat-card-subject"), h.bounds("chat-card-header"));
    assert!(
        subject.size.height > chip.size.height * 1.5,
        "the whole question wraps over several lines: {subject:?} against {chip:?}"
    );
    assert!(
        painted(&mut h, "chat-card-option-desc-1"),
        "sub's description is drawn"
    );
    let (row, description) = (h.bounds("chat-card-1"), h.bounds("chat-card-option-desc-1"));
    assert!(
        row.contains(&description.center()) && description.origin.y > row.origin.y,
        "under its label, inside its row: {description:?} in {row:?}"
    );
    assert!(
        !painted(&mut h, "chat-card-option-desc-2"),
        "an option without a description draws none"
    );
}

#[gpui::test]
fn clicking_an_option_row_sends_its_answer(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    show(&mut h, described_question());
    h.click_on("chat-card-option-desc-1");
    assert_eq!(
        answer_kinds(&mut h),
        [opt(1)],
        "a click on the description answers"
    );
}

/// A file in `dir` a card names, by its absolute path.
fn card_file(dir: &TestDir) -> std::path::PathBuf {
    let path = dir.path().join("notes.txt");
    std::fs::write(&path, "x\n").expect("write the file");
    let resolved = std::fs::canonicalize(&path).expect("canonicalize the file");
    let text = resolved.to_string_lossy();
    std::path::PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(&text))
}

/// A click on the first character of the text `selector` tags.
fn click_first_char(h: &mut Harness<'_>, selector: &str) {
    let bounds = h.bounds(selector);
    h.click(
        point(bounds.left() + px(2.0), bounds.top() + px(5.0)),
        Modifiers::none(),
    );
}

#[gpui::test]
fn a_path_in_a_question_cards_body_opens_without_answering(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let file = card_file(&dir);
    let mut h = loaded(cx, &dir);
    let mut question = described_question();
    question.body = format!("{} holds the notes.", file.display());
    show(&mut h, question);

    click_first_char(&mut h, "chat-card-body");

    assert_eq!(
        h.opened(),
        [Opened::DefaultApp(file)],
        "the body's path is a link"
    );
    assert_eq!(answer_kinds(&mut h), [], "the click answers nothing");
}

#[gpui::test]
fn a_path_link_in_an_option_description_does_not_pick_the_option(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let file = card_file(&dir);
    let mut h = loaded(cx, &dir);
    let mut question = described_question();
    let description = format!("{} is the one to keep", file.display());
    question.options = described(&[(1, "sub", Some(description.as_str())), (2, "div", None)]);
    show(&mut h, question);

    click_first_char(&mut h, "chat-card-option-desc-1");

    assert_eq!(
        h.opened(),
        [Opened::DefaultApp(file)],
        "the description's path is a link"
    );
    assert_eq!(answer_kinds(&mut h), [], "the option is not picked");
}

#[gpui::test]
fn a_multi_question_card_shows_its_tabs_and_marks_the_current_one(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    let mut question = described_question();
    question.questions = tabs(&["Op", "Base", "Mode"], 1);
    show(&mut h, question);
    for tab in ["chat-card-tab-1", "chat-card-tab-2", "chat-card-tab-3"] {
        assert!(painted(&mut h, tab), "{tab} is drawn");
    }
    let chip = h.bounds("chat-card-header").center();
    assert!(
        h.bounds("chat-card-tab-2").contains(&chip),
        "the current tab carries the header chip"
    );
    for other in ["chat-card-tab-1", "chat-card-tab-3"] {
        assert!(!h.bounds(other).contains(&chip), "{other} is not marked");
    }
}

#[gpui::test]
fn a_composer_draft_survives_leaving_the_chat_view(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    h.click_on("chat-composer");
    type_text(&mut h, "keep me");
    h.click_on(TO_TERMINAL);
    assert_eq!(unloads(&h.sent()), 1, "the chat unloads");
    h.click_on(TO_CHAT);
    let id = load_request(&h.sent()).expect("the toggle asks for the chat again");
    h.send(reply(Some(&id), vec![]));
    assert_eq!(composer_text(&mut h).as_deref(), Some("keep me"));
}

#[gpui::test]
fn a_composer_draft_survives_a_reconnect(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    h.click_on("chat-composer");
    type_text(&mut h, "keep me");
    h.send_welcome(support::PROTOCOL);
    h.load(&Fixture::single(session("s1").build()));
    let id = load_request(&h.sent()).expect("the new connection asks for the chat again");
    h.send(reply(Some(&id), vec![]));
    assert_eq!(composer_text(&mut h).as_deref(), Some("keep me"));
}

#[gpui::test]
fn a_composer_draft_survives_two_reconnects_before_the_chat_reloads(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    h.click_on("chat-composer");
    type_text(&mut h, "keep me");
    h.send_welcome(support::PROTOCOL);
    h.load(&Fixture::single(session("s1").build()));
    assert!(
        load_request(&h.sent()).is_some(),
        "the first new connection asks for the chat"
    );
    h.send_welcome(support::PROTOCOL);
    h.load(&Fixture::single(session("s1").build()));
    let id = load_request(&h.sent()).expect("the second new connection asks again");
    h.send(reply(Some(&id), vec![]));
    assert_eq!(composer_text(&mut h).as_deref(), Some("keep me"));
}

#[gpui::test]
fn a_remote_composer_draft_survives_its_hosts_reconnect(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h =
        support::remote_in_pane(cx, &dir, &serde_json::json!({}), &[], session("b1").build());
    h.click_on("pane-view-chat-p2");
    let id = remote_load_request(&mut h).expect("the pane asks its host for the chat");
    h.host(&support::saved_remote()).send(remote_reply(&id));
    h.click_on("chat-composer");
    type_text(&mut h, "keep me");
    h.host(&support::saved_remote()).connect();
    support::remote_lists(&mut h, &["b1"]);
    let again = remote_load_request(&mut h).expect("the host's new connection asks again");
    h.host(&support::saved_remote()).send(remote_reply(&again));
    assert_eq!(
        h.root(|root, cx| root.chat_composer_text("b1", cx))
            .as_deref(),
        Some("keep me")
    );
}

/// The request id of the last `LoadChat` for `b1` sent to the saved host.
fn remote_load_request(h: &mut Harness<'_>) -> Option<String> {
    h.host(&support::saved_remote())
        .sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::LoadChat {
                session_id,
                request_id,
            } if session_id == "b1" => request_id,
            _ => None,
        })
        .next_back()
}

/// `b1`'s chat loaded with no rows, the reply echoing `request_id`.
fn remote_reply(request_id: &str) -> DaemonMessage {
    DaemonMessage::Chat {
        session_id: "b1".to_owned(),
        entries: Vec::new(),
        hidden: 0,
        screen: screen(),
        request_id: Some(request_id.to_owned()),
    }
}

#[gpui::test]
fn while_a_dialog_is_up_the_composer_offers_only_a_disabled_send(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    h.click_on("chat-composer");
    type_text(&mut h, "careful");
    assert_eq!(actions(&mut h), ["chat-send"]);
    assert!(painted(&mut h, "chat-send"));
    show(&mut h, permission("p1"));
    assert_eq!(
        actions(&mut h),
        ["chat-send"],
        "the composer offers only Send while a card is up"
    );
    h.sent();
    h.click_on("chat-send");
    h.keys("ctrl-enter");
    let sent = h.sent();
    assert!(
        !sent.iter().any(|msg| matches!(
            msg,
            ClientMessage::ChatSend { .. } | ClientMessage::ChatAnswer { .. }
        )),
        "neither Send nor Ctrl+Enter sends the composer's text while a card is up"
    );
    assert_eq!(composer_text(&mut h).as_deref(), Some("careful"));
}

#[gpui::test]
fn a_quick_reply_sends_its_text_as_a_chat_message(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    assert!(
        painted(&mut h, "chat-quick-1-0"),
        "the common group's first reply is drawn"
    );
    h.sent();

    h.click_on("chat-quick-1-0");

    let sent = sends(&mut h);
    assert_eq!(sent.len(), 1, "one send: {sent:?}");
    assert_eq!(sent[0].0, "continue");
    assert_eq!(
        composer_text(&mut h).as_deref(),
        Some(""),
        "the composer keeps its own text, still empty"
    );
}

#[gpui::test]
fn a_quick_reply_leaves_typed_text_in_the_composer(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    h.click_on("chat-composer");
    type_text(&mut h, "hi");
    h.sent();

    h.click_on("chat-quick-1-0");

    let sent = sends(&mut h);
    assert_eq!(sent.len(), 1, "one send: {sent:?}");
    assert_eq!(sent[0].0, "continue");
    assert_eq!(
        composer_text(&mut h).as_deref(),
        Some("hi"),
        "the composer keeps its own text"
    );
}

#[gpui::test]
fn a_quick_reply_is_disabled_under_a_dialog(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    h.click_on("chat-composer");
    type_text(&mut h, "careful");
    show(&mut h, permission("p1"));
    assert!(
        painted(&mut h, "chat-quick-1-0"),
        "the chip is drawn, disabled"
    );
    h.sent();

    h.click_on("chat-quick-1-0");

    let sent = h.sent();
    assert!(
        !sent
            .iter()
            .any(|msg| matches!(msg, ClientMessage::ChatSend { .. })),
        "a disabled chip sends nothing: {sent:?}"
    );
    assert_eq!(composer_text(&mut h).as_deref(), Some("careful"));
}

#[gpui::test]
fn an_empty_quick_reply_group_draws_no_section(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    support::seed_ui(
        &dir,
        &serde_json::json!({
            "phone_agent_replies": [
                { "title": "more", "replies": [] },
                { "title": "common", "replies": ["continue"] },
            ],
        }),
    );
    let (mut h, id) = opened(cx, &dir);
    h.send(reply(Some(&id), vec![]));
    h.sent();

    assert!(
        !painted(&mut h, "chat-quick-group-0"),
        "an empty group draws no section"
    );
    assert!(
        painted(&mut h, "chat-quick-group-1"),
        "the group with a reply draws its section"
    );
    assert!(painted(&mut h, "chat-quick-1-0"), "with its reply");
}

#[gpui::test]
fn the_chat_shows_the_sessions_model_as_a_pill(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    h.send(reply(Some(&id), vec![]));
    h.send(chat_model(
        "s1",
        Some("claude-opus-5-5"),
        Some("Opus 5.5"),
        None,
    ));

    assert!(painted(&mut h, "chat-model"), "the pill is drawn");
    assert_eq!(pill_label(&mut h).as_deref(), Some("Opus 5.5"));
}

#[gpui::test]
fn the_pill_shows_the_effort_after_the_model(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    h.send(reply(Some(&id), vec![]));
    h.send(chat_model(
        "s1",
        Some("claude-opus-5-5"),
        Some("Opus 5.5"),
        Some("high"),
    ));

    assert!(painted(&mut h, "chat-model"), "the pill is drawn");
    assert_eq!(pill_label(&mut h).as_deref(), Some("Opus 5.5 · high"));
}

#[gpui::test]
fn an_effort_alone_draws_no_pill(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    h.send(reply(Some(&id), vec![]));
    h.send(chat_model("s1", None, None, Some("high")));

    assert!(!painted(&mut h, "chat-model"), "no model, no pill");
    assert_eq!(pill_label(&mut h), None);
}

#[gpui::test]
fn a_model_change_updates_the_pill(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    h.send(reply(Some(&id), vec![]));
    h.send(chat_model(
        "s1",
        Some("claude-opus-5-5"),
        Some("Opus 5.5"),
        None,
    ));
    h.send(chat_model(
        "s1",
        Some("claude-sonnet-9"),
        Some("Sonnet 9"),
        None,
    ));

    assert!(painted(&mut h, "chat-model"), "the pill is still drawn");
    assert_eq!(pill_label(&mut h).as_deref(), Some("Sonnet 9"));
}

#[gpui::test]
fn a_cleared_model_removes_the_pill(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    h.send(reply(Some(&id), vec![]));
    h.send(chat_model(
        "s1",
        Some("claude-opus-5-5"),
        Some("Opus 5.5"),
        None,
    ));
    assert!(painted(&mut h, "chat-model"));

    h.send(chat_model("s1", None, None, None));

    assert!(!drawn_now(&mut h, "chat-model"), "the pill is gone");
    assert_eq!(pill_label(&mut h), None);
}

#[gpui::test]
fn a_chat_with_no_model_draws_no_pill(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);

    assert!(!painted(&mut h, "chat-model"), "no model, no pill");
    assert_eq!(pill_label(&mut h), None);
}

#[gpui::test]
fn clicking_the_pill_sends_the_switch_command(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    h.send(reply(Some(&id), vec![]));
    h.send(chat_model_with_switch("s1", "Opus 5.5", "/model"));
    assert!(painted(&mut h, "chat-model"), "the pill is drawn");
    h.sent();

    h.click_on("chat-model");

    let sent = sends(&mut h);
    assert_eq!(sent.len(), 1, "one send: {sent:?}");
    assert_eq!(sent[0].0, "/model");
}

#[gpui::test]
fn the_pill_without_a_switch_command_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    h.send(reply(Some(&id), vec![]));
    h.send(chat_model(
        "s1",
        Some("claude-opus-5-5"),
        Some("Opus 5.5"),
        None,
    ));
    assert!(painted(&mut h, "chat-model"), "the pill is drawn");
    h.sent();

    h.click_on("chat-model");

    let sent = sends(&mut h);
    assert!(sent.is_empty(), "a read-only pill sends nothing: {sent:?}");
}

#[gpui::test]
fn the_pill_sends_nothing_while_a_card_is_up(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    h.send(reply(Some(&id), vec![]));
    h.send(chat_model_with_switch("s1", "Opus 5.5", "/model"));
    show(&mut h, permission("p1"));
    assert!(painted(&mut h, "chat-model"), "the pill is drawn, disabled");
    h.sent();

    h.click_on("chat-model");

    let sent = sends(&mut h);
    assert!(sent.is_empty(), "a disabled pill sends nothing: {sent:?}");
}

#[gpui::test]
fn a_chat_model_for_another_session_is_dropped(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    h.send(reply(Some(&id), vec![]));
    h.send(chat_model(
        "s2",
        Some("claude-opus-5-5"),
        Some("Opus 5.5"),
        None,
    ));

    assert!(!painted(&mut h, "chat-model"), "s1 shows no pill");
    assert_eq!(pill_label(&mut h), None);
}

#[gpui::test]
fn the_pill_shows_with_no_quick_replies(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    support::seed_ui(&dir, &serde_json::json!({ "phone_agent_replies": [] }));
    let (mut h, id) = opened(cx, &dir);
    h.send(reply(Some(&id), vec![]));
    h.send(chat_model(
        "s1",
        Some("claude-opus-5-5"),
        Some("Opus 5.5"),
        None,
    ));

    assert!(painted(&mut h, "chat-model"), "the line holds the pill");
    assert_eq!(pill_label(&mut h).as_deref(), Some("Opus 5.5"));
    assert!(
        !painted(&mut h, "chat-quick-group-0"),
        "and no quick reply chip"
    );
}

#[gpui::test]
fn quick_replies_stay_on_one_line_in_a_narrow_pane(cx: &mut TestAppContext) {
    // The harness's fixed-advance glyphs run wider than Inter, so a 680 px
    // pane here stands for the 640 px pane with real text.
    const NARROW: f32 = 680.0;
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    h.send(reply(Some(&id), vec![]));
    h.send(chat_model(
        "s1",
        Some("claude-opus-5-5"),
        Some("Opus 5.5"),
        Some("high"),
    ));
    assert!(painted(&mut h, "chat-model"), "the pill is drawn");
    let pane = h.bounds("pane-chat-p1").size.width / px(1.0);
    let (width, height) = (h.window_width(), h.window_height());
    h.cx.simulate_resize(gpui::size(px(width + NARROW - pane), px(height)));
    h.cx.run_until_parked();

    let pane = h.bounds("pane-chat-p1").size.width / px(1.0);
    assert!(
        (pane - NARROW).abs() <= 1.0,
        "the pane is {NARROW} px wide: {pane}"
    );
    // Within a pixel: the read-only pill's box and a chip's may round
    // differently, while a wrapped chip sits a whole line lower.
    let row = (
        h.bounds("chat-quick"),
        h.bounds("chat-model"),
        h.bounds("chat-quick-group-0"),
        h.bounds("chat-quick-group-1"),
    );
    let top = h.bounds("chat-model").top() / px(1.0);
    let chips = (0..2)
        .map(|reply| (0, reply))
        .chain((0..5).map(|reply| (1, reply)));
    for (group, reply) in chips {
        let chip = format!("chat-quick-{group}-{reply}");
        let chip_top = h.bounds(&chip).top() / px(1.0);
        assert!(
            (chip_top - top).abs() <= 1.0,
            "{chip} is on the pill's line: {chip_top} against {top}; row, pill, groups: {row:?}"
        );
    }
}

#[gpui::test]
fn a_permission_cards_box_allows_or_denies_with_its_text(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    h.click_on("chat-composer");
    type_text(&mut h, "careful");
    let mut fixed = permission("p2");
    fixed.can_amend = false;
    show(&mut h, fixed.clone());
    assert!(
        painted(&mut h, "chat-card-box"),
        "a permission card has a box"
    );
    assert!(painted(&mut h, "chat-card-deny-text"));
    assert!(
        !painted(&mut h, "chat-card-allow-text"),
        "no amendment, no Allow with text"
    );
    h.click_on("chat-card-deny-text");
    assert_eq!(answer_kinds(&mut h), [], "a blank box sends nothing");
    h.click_on("chat-card-box");
    type_text(&mut h, "no");
    h.keys("escape");
    assert_eq!(answer_kinds(&mut h), [], "Esc in the box denies nothing");
    h.click_on("chat-card-box");
    h.keys("end enter");
    assert_eq!(
        answer_kinds(&mut h),
        [ChatAnswerKind::DenyWithText {
            text: "no".to_owned()
        }],
        "Enter in the box denies with its text"
    );

    fixed.id = "p3".to_owned();
    show(&mut h, fixed);
    h.click_on("chat-card-box");
    type_text(&mut h, "stop");
    h.click_on("chat-card-deny-text");
    assert_eq!(
        answer_kinds(&mut h),
        [ChatAnswerKind::DenyWithText {
            text: "stop".to_owned()
        }]
    );

    show(&mut h, permission("p1"));
    assert!(painted(&mut h, "chat-card-allow-text"));
    h.click_on("chat-card-allow-text");
    assert_eq!(answer_kinds(&mut h), [], "a blank box sends nothing");
    h.click_on("chat-card-box");
    type_text(&mut h, "use 3");
    h.click_on("chat-card-allow-text");
    assert_eq!(
        answer_kinds(&mut h),
        [ChatAnswerKind::AllowWithText {
            text: "use 3".to_owned()
        }]
    );
    assert_eq!(box_text(&mut h).as_deref(), Some(""));
    assert_eq!(
        composer_text(&mut h).as_deref(),
        Some("careful"),
        "the composer keeps its own text"
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
        Some("The prompt box has a draft. Send or clear it in the terminal first."),
        "a host without chat_draft keeps the old note"
    );
    assert!(painted(&mut h, "chat-composer-note"));
    assert!(!painted(&mut h, "chat-draft-pull"));
    assert!(!painted(&mut h, "chat-draft-send"));
}

/// `s1`'s chat loaded on a daemon listing `chat_draft`, everything sent so
/// far drained.
fn loaded_on_a_draft_host<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    loaded_listing(cx, dir, &[FEATURE_CHAT_DRAFT])
}

/// `s1`'s chat loaded on a daemon listing `chat_draft` and
/// `chat_clear_draft`, the box holding a draft, everything sent so far
/// drained.
fn on_a_clear_draft_host<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = loaded_listing(cx, dir, &[FEATURE_CHAT_DRAFT, FEATURE_CHAT_CLEAR_DRAFT]);
    show_screen(&mut h, ComposerState::Draft, None);
    h.sent();
    h
}

/// `s1`'s chat loaded on a daemon listing `features`, everything sent so
/// far drained.
fn loaded_listing<'a>(cx: &'a mut TestAppContext, dir: &TestDir, features: &[&str]) -> Harness<'a> {
    let fixture = Fixture::single(session("s1").build());
    let mut h = Harness::with(cx, dir, &fixture);
    h.answer_scrollback("s1", b"");
    h.reconnect_with_features(features);
    h.load(&fixture);
    h.answer_scrollback("s1", b"");
    h.sent();
    h.click_on(TO_CHAT);
    let id = load_request(&h.sent()).expect("the toggle asks for the chat");
    h.send(reply(Some(&id), vec![]));
    h.sent();
    h
}

/// The request ids of the `ChatPullDraft`s for `s1` sent since the last
/// drain.
fn pulls(h: &mut Harness<'_>) -> Vec<Option<String>> {
    h.sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::ChatPullDraft {
                session_id,
                request_id,
            } if session_id == "s1" => Some(request_id),
            _ => None,
        })
        .collect()
}

/// The daemon answers a pull with `text`, the box `cleared` or not.
fn pulled(h: &mut Harness<'_>, request_id: Option<String>, text: &str, cleared: bool) {
    h.send(DaemonMessage::ChatDraft {
        session_id: "s1".to_owned(),
        request_id,
        text: text.to_owned(),
        cleared,
    });
}

fn composer_error(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.chat_composer_error("s1"))
}

/// Whether anything sent since the last drain asks the chat to send, pull
/// or clear.
fn sent_chat_work(h: &mut Harness<'_>) -> bool {
    h.sent().iter().any(|msg| {
        matches!(
            msg,
            ClientMessage::ChatSend { .. }
                | ClientMessage::ChatPullDraft { .. }
                | ClientMessage::ChatSendDraft { .. }
                | ClientMessage::ChatClearDraft { .. }
        )
    })
}

/// The request ids of the `ChatClearDraft`s for `s1` sent since the last
/// drain.
fn clears(h: &mut Harness<'_>) -> Vec<Option<String>> {
    h.sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::ChatClearDraft {
                session_id,
                request_id,
            } if session_id == "s1" => Some(request_id),
            _ => None,
        })
        .collect()
}

#[gpui::test]
fn a_draft_on_a_chat_draft_host_offers_pull_and_send(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded_on_a_draft_host(cx, &dir);
    show_screen(&mut h, ComposerState::Draft, None);
    assert_eq!(note(&mut h).as_deref(), Some("The prompt box has a draft."));
    assert!(painted(&mut h, "chat-composer-note"));
    assert!(painted(&mut h, "chat-draft-pull"));
    assert!(painted(&mut h, "chat-draft-send"));
    assert!(
        !painted(&mut h, "chat-draft-clear"),
        "a host without chat_clear_draft shows no Clear"
    );
}

#[gpui::test]
fn a_draft_on_a_clear_draft_host_offers_pull_send_and_clear(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_a_clear_draft_host(cx, &dir);
    assert_eq!(
        note(&mut h).as_deref(),
        Some("The prompt box has a draft. Pull, send or clear it first.")
    );
    let pull = h.bounds("chat-draft-pull");
    let send = h.bounds("chat-draft-send");
    let clear = h.bounds("chat-draft-clear");
    assert!(clear.size.height > px(0.0), "Clear shows");
    assert!(
        pull.origin.x < send.origin.x && send.origin.x < clear.origin.x,
        "Pull, Send, Clear in that order"
    );
    assert!(!painted(&mut h, "chat-draft-clear-confirm"));
}

#[gpui::test]
fn a_draft_clear_arms_and_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_a_clear_draft_host(cx, &dir);
    h.click_on("chat-draft-clear");
    assert!(!sent_chat_work(&mut h), "a press only arms");
    assert!(drawn_now(&mut h, "chat-draft-clear-confirm"));
    assert!(drawn_now(&mut h, "chat-draft-clear-cancel"));
    assert!(
        !drawn_now(&mut h, "chat-draft-clear"),
        "Confirm clear takes its place"
    );
    assert!(drawn_now(&mut h, "chat-draft-pull"), "Pull and Send stay");
    assert!(drawn_now(&mut h, "chat-draft-send"));
}

#[gpui::test]
fn confirm_clear_sends_one_draft_clear_and_disarms(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_a_clear_draft_host(cx, &dir);
    h.click_on("chat-draft-clear");
    h.click_on("chat-draft-clear-confirm");
    let sent = clears(&mut h);
    assert_eq!(sent.len(), 1, "one clear goes out: {sent:?}");
    assert!(sent[0].is_some(), "under a request id");
    assert!(!drawn_now(&mut h, "chat-draft-clear-confirm"), "disarmed");
    assert!(drawn_now(&mut h, "chat-draft-clear"));
    assert_eq!(composer_error(&mut h), None);
}

#[gpui::test]
fn an_armed_draft_clear_drops_on_cancel_escape_or_a_press_elsewhere(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_a_clear_draft_host(cx, &dir);
    for (how, drop) in [
        ("Cancel", "cancel"),
        ("Esc", "escape"),
        ("a press elsewhere", "press"),
    ] {
        h.click_on("chat-draft-clear");
        assert!(
            drawn_now(&mut h, "chat-draft-clear-confirm"),
            "{how}: armed"
        );
        match drop {
            "cancel" => h.click_on("chat-draft-clear-cancel"),
            "escape" => h.keys("escape"),
            _ => h.click_on("chat-composer-note"),
        }
        assert!(
            !drawn_now(&mut h, "chat-draft-clear-confirm"),
            "{how} disarms"
        );
        assert!(
            drawn_now(&mut h, "chat-draft-clear"),
            "{how}: Clear is back"
        );
        assert!(!sent_chat_work(&mut h), "{how}: nothing is sent");
    }
}

#[gpui::test]
fn an_armed_draft_clear_drops_when_the_draft_goes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_a_clear_draft_host(cx, &dir);
    h.click_on("chat-draft-clear");
    show_screen(&mut h, ComposerState::Ready, None);
    show_screen(&mut h, ComposerState::Draft, None);
    assert!(
        !drawn_now(&mut h, "chat-draft-clear-confirm"),
        "the arm went with the draft"
    );
    assert!(drawn_now(&mut h, "chat-draft-clear"));
    assert!(!sent_chat_work(&mut h), "nothing is sent");
}

#[gpui::test]
fn escape_in_the_composer_disarms_a_draft_clear(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_a_clear_draft_host(cx, &dir);
    h.click_on("chat-composer");
    h.click_on("chat-draft-clear");
    assert!(drawn_now(&mut h, "chat-draft-clear-confirm"), "armed");
    h.keys("escape");
    assert!(
        !drawn_now(&mut h, "chat-draft-clear-confirm"),
        "the composer's Esc disarms Clear"
    );
    assert!(drawn_now(&mut h, "chat-draft-clear"), "Clear is back");
    assert!(!sent_chat_work(&mut h), "nothing is sent");
    assert!(h.sent_input("s1").is_empty(), "no Esc reached the terminal");
}

#[gpui::test]
fn an_armed_draft_clear_drops_while_a_send_takes_the_row(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_a_clear_draft_host(cx, &dir);
    h.click_on("chat-composer");
    h.click_on("chat-draft-clear");
    assert!(drawn_now(&mut h, "chat-draft-clear-confirm"), "armed");
    type_text(&mut h, "hello");
    h.keys("ctrl-enter");
    let sends: Vec<Option<String>> = h
        .sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::ChatSend { request_id, .. } => Some(request_id),
            _ => None,
        })
        .collect();
    assert_eq!(sends.len(), 1, "the keyboard send went out: {sends:?}");
    assert_eq!(note(&mut h).as_deref(), Some("Sending\u{2026}"));
    refused(&mut h, sends[0].clone(), "the prompt box is busy");
    assert_eq!(
        note(&mut h).as_deref(),
        Some("The prompt box has a draft. Pull, send or clear it first."),
        "the draft row is back"
    );
    assert!(
        !drawn_now(&mut h, "chat-draft-clear-confirm"),
        "the arm went while the row showed no Clear"
    );
    assert!(drawn_now(&mut h, "chat-draft-clear"), "Clear shows");
}

#[gpui::test]
fn an_armed_draft_clear_drops_on_a_dropped_connection(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_a_clear_draft_host(cx, &dir);
    h.click_on("chat-draft-clear");
    assert!(drawn_now(&mut h, "chat-draft-clear-confirm"), "armed");
    h.lose_connection();
    assert_eq!(note(&mut h).as_deref(), Some("Not connected"));
    h.reconnect_with_features(&[FEATURE_CHAT_DRAFT, FEATURE_CHAT_CLEAR_DRAFT]);
    h.load(&Fixture::single(session("s1").build()));
    h.answer_scrollback("s1", b"");
    if let Some(id) = load_request(&h.sent()) {
        h.send(reply(Some(&id), vec![]));
    }
    show_screen(&mut h, ComposerState::Draft, None);
    h.sent();
    assert_eq!(
        note(&mut h).as_deref(),
        Some("The prompt box has a draft. Pull, send or clear it first."),
        "the draft row is back"
    );
    assert!(drawn_now(&mut h, "chat-draft-clear"), "Clear shows again");
    assert!(
        !drawn_now(&mut h, "chat-draft-clear-confirm"),
        "the arm went with the connection"
    );
    assert!(!sent_chat_work(&mut h), "nothing is sent");
}

#[gpui::test]
fn a_refused_draft_clear_shows_why(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_a_clear_draft_host(cx, &dir);
    h.click_on("chat-draft-clear");
    h.click_on("chat-draft-clear-confirm");
    let sent = clears(&mut h);
    let partial =
        "part of the draft is still in the terminal's box; clear the rest in the terminal";
    refused(&mut h, sent[0].clone(), partial);
    assert_eq!(composer_error(&mut h).as_deref(), Some(partial));
    assert!(painted(&mut h, "chat-composer-error"));
}

#[gpui::test]
fn pull_sends_a_pull_and_lands_the_draft_ahead_of_typed_text(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded_on_a_draft_host(cx, &dir);
    show_screen(&mut h, ComposerState::Draft, None);
    h.click_on("chat-composer");
    type_text(&mut h, "and test");
    h.click_on("chat-draft-pull");
    let sent = pulls(&mut h);
    assert_eq!(sent.len(), 1, "one pull goes out");
    assert!(sent[0].is_some(), "under a request id");
    assert_eq!(note(&mut h).as_deref(), Some("Pulling the draft\u{2026}"));
    assert!(
        !drawn_now(&mut h, "chat-draft-pull"),
        "no buttons while pulling"
    );
    pulled(&mut h, sent[0].clone(), "fix the bug", true);
    assert_eq!(
        composer_text(&mut h).as_deref(),
        Some("fix the bug\nand test")
    );
    assert_eq!(composer_error(&mut h), None);
    assert_eq!(note(&mut h).as_deref(), Some("The prompt box has a draft."));
}

#[gpui::test]
fn a_partial_pull_says_the_box_still_holds_some(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded_on_a_draft_host(cx, &dir);
    show_screen(&mut h, ComposerState::Draft, None);
    h.click_on("chat-draft-pull");
    let sent = pulls(&mut h);
    pulled(&mut h, sent[0].clone(), "fix the bug", false);
    assert_eq!(composer_text(&mut h).as_deref(), Some("fix the bug"));
    assert_eq!(
        composer_error(&mut h).as_deref(),
        Some("Pulled, but the terminal's prompt box still holds some of the draft.")
    );
}

#[gpui::test]
fn a_refused_pull_shows_why(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded_on_a_draft_host(cx, &dir);
    show_screen(&mut h, ComposerState::Draft, None);
    h.click_on("chat-composer");
    type_text(&mut h, "mine");
    h.click_on("chat-draft-pull");
    let sent = pulls(&mut h);
    refused(&mut h, sent[0].clone(), "the draft holds pasted text");
    assert_eq!(composer_text(&mut h).as_deref(), Some("mine"));
    assert_eq!(
        composer_error(&mut h).as_deref(),
        Some("the draft holds pasted text")
    );
    assert!(painted(&mut h, "chat-composer-error"));
    assert_eq!(note(&mut h).as_deref(), Some("The prompt box has a draft."));
    assert!(drawn_now(&mut h, "chat-draft-pull"), "the buttons are back");
}

#[gpui::test]
fn send_as_is_sends_a_send_draft(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded_on_a_draft_host(cx, &dir);
    show_screen(&mut h, ComposerState::Draft, None);
    h.click_on("chat-draft-send");
    let sent: Vec<Option<String>> = h
        .sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::ChatSendDraft {
                session_id,
                request_id,
            } if session_id == "s1" => Some(request_id),
            _ => None,
        })
        .collect();
    assert_eq!(sent.len(), 1, "one send-draft goes out");
    assert!(sent[0].is_some(), "under a request id");
    assert_eq!(note(&mut h).as_deref(), Some("Sending\u{2026}"));
}

#[gpui::test]
fn a_refused_send_as_is_keeps_the_caret(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded_on_a_draft_host(cx, &dir);
    show_screen(&mut h, ComposerState::Draft, None);
    h.click_on("chat-composer");
    type_text(&mut h, "abcd");
    h.keys("left left");
    h.click_on("chat-draft-send");
    let sent: Vec<Option<String>> = h
        .sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::ChatSendDraft { request_id, .. } => Some(request_id),
            _ => None,
        })
        .collect();
    assert_eq!(sent.len(), 1, "one send-draft goes out");
    refused(&mut h, sent[0].clone(), "the prompt box is busy");
    assert_eq!(
        composer_error(&mut h).as_deref(),
        Some("the prompt box is busy")
    );
    assert_eq!(composer_text(&mut h).as_deref(), Some("abcd"));
    assert_eq!(
        h.root(|root, cx| root.chat_composer_selection("s1", cx)),
        Some(2..2),
        "the caret stays where it was"
    );
}

#[gpui::test]
fn a_suggestion_shows_a_chip_that_sends_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    h.click_on("chat-composer");
    type_text(&mut h, "mine");
    show_screen(
        &mut h,
        ComposerState::Suggestion {
            text: "write todo.py".to_owned(),
        },
        None,
    );
    assert!(painted(&mut h, "chat-suggestion"));
    assert_eq!(note(&mut h), None);
    h.click_on("chat-suggestion-send");
    let sent = sends(&mut h);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, "write todo.py");
    assert!(sent[0].1.is_some(), "under a request id");
    assert_eq!(
        composer_text(&mut h).as_deref(),
        Some("mine"),
        "the composer keeps its own text"
    );
}

#[gpui::test]
fn a_suggestion_chip_pulls_into_the_composer(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    show_screen(
        &mut h,
        ComposerState::Suggestion {
            text: "write todo.py".to_owned(),
        },
        None,
    );
    h.click_on("chat-suggestion-pull");
    assert_eq!(composer_text(&mut h).as_deref(), Some("write todo.py"));
    assert!(!sent_chat_work(&mut h), "nothing is sent");
}

#[gpui::test]
fn a_cut_suggestion_has_no_actions(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    show_screen(
        &mut h,
        ComposerState::Suggestion {
            text: "write a long sugg\u{2026}".to_owned(),
        },
        None,
    );
    assert!(painted(&mut h, "chat-suggestion"));
    assert!(!painted(&mut h, "chat-suggestion-send"));
    assert!(!painted(&mut h, "chat-suggestion-pull"));
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
        Some("A full-screen view is open in the terminal. Close it there to send."),
        "an overlay with no Close prompt names no label"
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
fn a_refused_typed_answer_after_the_dialog_went_goes_to_the_composer(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir);
    show(&mut h, permission("p1"));
    h.click_on("chat-card-box");
    type_text(&mut h, "careful");
    h.click_on("chat-card-deny-text");
    let request_id = answers(&mut h)[0].2.clone();
    assert_eq!(box_text(&mut h).as_deref(), Some(""));
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

// Tool cards that run a shell command.

/// A Bash command of four statements, a pipe inside the first.
const COMMAND: &str =
    "cargo build 2>&1 | tail -n 3; git add docs/a.md && git commit -F .tmp/msg.txt; echo rc=$?";
const STATEMENTS: [&str; 4] = [
    "cargo build 2>&1 | tail -n 3;",
    "git add docs/a.md &&",
    "git commit -F .tmp/msg.txt;",
    "echo rc=$?",
];

/// A Bash call running `command`, as the daemon sends it: the summary its
/// first line, the input the pretty-printed JSON.
fn bash(id: &str, command: &str) -> ChatEntry {
    let input = serde_json::json!({ "command": command, "description": "Commit the docs" });
    ChatEntry::Tool {
        id: id.to_owned(),
        name: "Bash".to_owned(),
        summary: command.lines().next().unwrap_or_default().to_owned(),
        input: format!("{input:#}"),
        result: None,
        parent_id: None,
        tail_paths: Vec::new(),
        kind: ToolKind::Other,
        script: None,
    }
}

fn tool_lines(h: &mut Harness<'_>, id: &str) -> Vec<String> {
    h.root(|root, _| root.chat_tool_lines("s1", id))
}

fn line(n: usize) -> String {
    format!("chat-tool-line-p1-t1-{n}")
}

#[gpui::test]
fn a_long_bash_command_shows_one_statement_per_line(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    h.send(reply(Some(&id), vec![bash("t1", COMMAND)]));
    h.click_on("chat-row-p1-t1");
    assert_eq!(tool_lines(&mut h, "t1"), STATEMENTS);
    let card = h.bounds("chat-row-p1-t1");
    let rows: Vec<_> = (0..STATEMENTS.len()).map(|n| h.bounds(&line(n))).collect();
    for (n, row) in rows.iter().enumerate() {
        assert!(row.size.height > px(0.0), "statement {n} is drawn");
        assert!(
            card.contains(&row.origin),
            "statement {n} is inside its card"
        );
    }
    for pair in rows.windows(2) {
        assert!(
            pair[1].top() >= pair[0].bottom(),
            "each statement starts its own line: {pair:?}"
        );
    }
}

#[gpui::test]
fn a_folded_bash_card_keeps_one_line_and_a_click_opens_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    h.send(reply(
        Some(&id),
        vec![bash("t1", COMMAND), tool("t2", Some("ok"))],
    ));
    let none: [&str; 0] = [];
    assert_eq!(
        tool_lines(&mut h, "t1"),
        none,
        "a folded card lists no statements"
    );
    assert!(
        !painted(&mut h, &line(0)),
        "a folded card draws no statement"
    );
    let folded = h.bounds("chat-row-p1-t1").size.height;
    assert_eq!(
        folded,
        h.bounds("chat-row-p1-t2").size.height,
        "the folded card is one line, as any folded tool card"
    );
    h.click_on("chat-row-p1-t1");
    assert!(
        painted(&mut h, &line(STATEMENTS.len() - 1)),
        "the open card draws every statement"
    );
    assert!(
        h.bounds("chat-row-p1-t1").size.height > folded,
        "the card grew"
    );
    h.click_on("chat-row-p1-t1");
    assert_eq!(tool_lines(&mut h, "t1"), none, "a second click folds it");
    assert!(
        !drawn_now(&mut h, &line(0)),
        "the folded card draws no statement"
    );
}

#[gpui::test]
fn a_command_cut_by_the_input_cap_falls_back_to_its_summary(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    let mut entry = bash("t1", COMMAND);
    if let ChatEntry::Tool { summary, input, .. } = &mut entry {
        *summary = "cargo build 2>&1 | tail -n 3; git add docs/a.md && git c\u{2026}".to_owned();
        let cut: String = input.chars().take(40).collect();
        *input = format!("{cut}\u{2026}");
    }
    h.send(reply(Some(&id), vec![entry]));
    h.click_on("chat-row-p1-t1");
    assert_eq!(
        tool_lines(&mut h, "t1"),
        [
            "cargo build 2>&1 | tail -n 3;",
            "git add docs/a.md &&",
            "git c\u{2026}"
        ],
        "the summary's statements, one per line"
    );
    assert!(
        painted(&mut h, &line(2)),
        "the summary's last statement is drawn"
    );
    assert!(
        !painted(&mut h, &line(3)),
        "nothing past the summary is drawn"
    );
}

/// What a disabled Chat or Summary control says while `s1`'s agent is down.
const AGENT_DOWN: &str = "The agent isn't running";

/// The daemon's snapshot of `s1` with `program` up in its pane: the shell's
/// `pwsh` once its hosted agent ended, `claude` once it started again.
fn running(h: &mut Harness<'_>, program: &str) {
    h.send(DaemonMessage::SessionUpdated {
        session: session("s1").program_name(program).build(),
        request_id: None,
    });
}

/// Why `p1`'s header cannot switch to `view` now; `None` when it can.
fn header_tip(h: &mut Harness<'_>, view: PaneView) -> Option<&'static str> {
    h.root(|root, _| root.pane_view_disabled_tip("p1", view))
}

fn summary_loads(sent: &[ClientMessage]) -> usize {
    sent.iter()
        .filter(|msg| matches!(msg, ClientMessage::LoadSummary { session_id, .. } if session_id == "s1"))
        .count()
}

#[gpui::test]
fn a_pane_on_its_chat_falls_back_to_the_terminal_when_the_hosted_agent_exits(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    h.send(reply(Some(&id), vec![user("u1")]));
    h.sent();
    running(&mut h, "pwsh");
    assert_eq!(
        h.root(|root, _| root.pane_view("p1")),
        PaneView::Terminal,
        "the pane shows its terminal"
    );
    assert_eq!(header_tip(&mut h, PaneView::Chat), Some(AGENT_DOWN));
    assert_eq!(header_tip(&mut h, PaneView::Summary), Some(AGENT_DOWN));
    assert_eq!(header_tip(&mut h, PaneView::Terminal), None);
    h.click_on(TO_CHAT);
    assert_eq!(
        load_request(&h.sent()),
        None,
        "the disabled Chat segment asks for nothing"
    );
    assert!(!shows_chat(&mut h), "and switches nothing");
}

#[gpui::test]
fn the_chat_unloads_when_the_agent_exits(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    h.send(reply(Some(&id), vec![user("u1")]));
    h.sent();
    running(&mut h, "pwsh");
    let sent = h.sent();
    assert_eq!(unloads(&sent), 1, "the chat unloads: {sent:?}");
    assert_eq!(
        h.root(|root, _| root.chat_rows("s1")),
        Vec::new(),
        "its rows are gone"
    );
}

#[gpui::test]
fn the_chat_comes_back_when_the_agent_starts_again(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    h.send(reply(Some(&id), vec![user("u1")]));
    running(&mut h, "pwsh");
    h.sent();
    running(&mut h, "claude");
    assert!(shows_chat(&mut h), "the pane is back on its chat");
    assert!(
        load_request(&h.sent()).is_some(),
        "and asks for the chat again"
    );
    assert_eq!(header_tip(&mut h, PaneView::Chat), None);
    assert_eq!(header_tip(&mut h, PaneView::Summary), None);
}

#[gpui::test]
fn a_reconnect_onto_a_shell_without_its_agent_shows_the_terminal(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    h.send(reply(Some(&id), vec![user("u1")]));
    h.send_welcome(support::PROTOCOL);
    h.load(&Fixture::single(session("s1").program_name("pwsh").build()));
    let sent = h.sent();
    assert_eq!(load_request(&sent), None, "no chat is asked for: {sent:?}");
    assert!(!shows_chat(&mut h), "the pane shows its terminal");
    assert_eq!(header_tip(&mut h, PaneView::Chat), Some(AGENT_DOWN));
}

#[gpui::test]
fn the_view_key_does_nothing_while_the_agent_is_down(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir);
    running(&mut h, "pwsh");
    h.sent();
    h.keys("ctrl-shift-m");
    h.keys("ctrl-shift-u");
    let sent = h.sent();
    assert_eq!(load_request(&sent), None, "no chat is asked for: {sent:?}");
    assert_eq!(summary_loads(&sent), 0, "nor a summary: {sent:?}");
    running(&mut h, "claude");
    assert_eq!(
        h.root(|root, _| root.pane_view("p1")),
        PaneView::Terminal,
        "the keys chose no view to come back to"
    );
}

#[gpui::test]
fn the_remembered_view_is_kept_while_the_agent_is_down(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir);
    h.click_on(TO_CHAT);
    running(&mut h, "pwsh");
    assert!(!shows_chat(&mut h));
    assert_eq!(
        saved_views(&dir),
        serde_json::json!({ "local": { "s1": "chat" } }),
        "the session's chosen view stays saved"
    );
    h.send(DaemonMessage::Tabs {
        tabs: vec![tab("t1", &split_with("s1"))],
    });
    running(&mut h, "claude");
    assert!(shows_chat(&mut h), "p1 returns to its chat");
    assert!(
        h.root(|root, _| root.pane_shows_chat("p2")),
        "a pane opened while the agent was down opens on the chat too"
    );
}

/// Whether `p1`'s terminal has the main window's keyboard.
fn terminal_focused(h: &mut Harness<'_>) -> bool {
    let root = h.root.clone();
    h.cx.update(|window, cx| root.read(cx).pane_terminal_focused("p1", window, cx))
}

/// `s1` of `r1` alone in `p1`, attached, its hosted agent down and its
/// chat the view last chosen for it, so `p1` shows its terminal.
fn down_on_a_remembered_chat<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    std::fs::write(
        dir.path().join("native-ui.json"),
        r#"{ "session_views": { "local": { "s1": "chat" } } }"#,
    )
    .expect("seed native-ui.json");
    let mut fixture = Fixture::single(session("s1").in_repo("r1").program_name("pwsh").build());
    fixture.repos = vec![repo("r1", "D:/src/r1")];
    let mut h = Harness::with(cx, dir, &fixture);
    h.answer_scrollback("s1", b"");
    h.sent();
    assert!(!shows_chat(&mut h), "p1 starts on its terminal");
    h
}

#[gpui::test]
fn the_terminal_takes_the_keyboard_when_the_agent_exits(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    h.send(reply(Some(&id), vec![user("u1")]));
    assert!(
        !terminal_focused(&mut h),
        "the chat pane's terminal has no keys"
    );
    running(&mut h, "pwsh");
    assert!(
        terminal_focused(&mut h),
        "the focused pane's terminal takes them"
    );
}

#[gpui::test]
fn the_terminal_takes_a_tab_pop_outs_keyboard_when_the_agent_exits(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached_in_a_repo(cx, &dir);
    let key = PopoutKey::Tab {
        tab_id: "t1".to_owned(),
    };
    let outcome = h.root_update(move |root, window, cx| root.open_popout(key, window, cx));
    assert_eq!(outcome, OpenOutcome::Opened);
    h.root_update(|root, window, cx| root.toggle_pane_view("p1", window, cx));
    assert!(!focused_in_popout(&mut h));
    running(&mut h, "pwsh");
    assert!(
        focused_in_popout(&mut h),
        "the popped tab's focused pane's terminal takes the pop-out's keyboard"
    );
}

#[gpui::test]
fn the_terminal_lets_go_of_the_keyboard_when_the_agent_starts_again(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = down_on_a_remembered_chat(cx, &dir);
    h.click_on("leaf-s1");
    assert!(terminal_focused(&mut h), "the terminal has the keys");
    running(&mut h, "claude");
    assert!(shows_chat(&mut h), "p1 goes back to its chat");
    assert!(
        !terminal_focused(&mut h),
        "the hidden terminal lets go of the keys"
    );
}

#[gpui::test]
fn the_terminal_lets_go_of_the_pop_outs_keyboard_when_the_agent_starts_again(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let mut h = down_on_a_remembered_chat(cx, &dir);
    pop_out_p1(&mut h);
    assert!(
        focused_in_popout(&mut h),
        "the popped terminal has the keys"
    );
    running(&mut h, "claude");
    assert!(shows_chat(&mut h), "p1 goes back to its chat");
    assert!(
        !focused_in_popout(&mut h),
        "the hidden terminal lets go of the pop-out's keyboard"
    );
}

#[gpui::test]
fn the_pane_menu_disables_chat_and_summary_while_the_agent_is_down(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir);
    running(&mut h, "pwsh");
    h.sent();
    h.right_click_on("pane-header-p1");
    assert_eq!(
        h.root(|root, _| root.pane_view_menu_labels()),
        ["Show chat", "Show summary"],
        "both rows are drawn"
    );
    for selector in [MENU_ROW, "session-menu-pane-view-summary"] {
        assert_eq!(
            h.root(|root, _| root.pane_view_menu_disabled_tip(selector)),
            Some(AGENT_DOWN),
            "{selector} is disabled"
        );
    }
    h.click_on(MENU_ROW);
    let sent = h.sent();
    assert_eq!(
        load_request(&sent),
        None,
        "the row asks for nothing: {sent:?}"
    );
    assert!(!shows_chat(&mut h));
}

/// The file the tailing tool call writes.
const LOG: &str = "C:/work/build.log";
const TAIL_BUTTON: &str = "chat-tail-t1-0";
const TAIL_BOX: &str = "chat-tail-box-t1-0";

/// A shell call that writes `LOG`.
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

/// Reports the connection as a daemon that follows files.
fn file_tail_daemon(h: &mut Harness<'_>) {
    let mut conn = Connection::for_target(Target::Local);
    conn.on_connecting();
    conn.on_socket_open(4242);
    let features = vec![
        FEATURE_AGENT_COMMANDS.to_owned(),
        FEATURE_FILE_TAIL.to_owned(),
    ];
    conn.on_welcome(support::PROTOCOL, features, None);
    h.event(NetEvent::State(conn));
}

/// `p1` on its chat over a daemon that follows files, the chat holding a
/// tool card that writes `LOG`.
fn tail_offered<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let (mut h, id) = opened(cx, dir);
    file_tail_daemon(&mut h);
    h.send(reply(Some(&id), vec![user("u1"), tailing_tool("t1")]));
    h.sent();
    h
}

/// The request ids of the `TailFile`s for `LOG` among `sent`.
fn tail_requests(sent: &[ClientMessage]) -> Vec<Option<String>> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::TailFile {
                session_id,
                path,
                request_id,
            } if session_id == "s1" && path == LOG => Some(request_id.clone()),
            _ => None,
        })
        .collect()
}

fn untails(sent: &[ClientMessage]) -> usize {
    sent.iter()
        .filter(|msg| {
            matches!(msg, ClientMessage::UntailFile { session_id, path }
                if session_id == "s1" && path == LOG)
        })
        .count()
}

/// The Tail button pressed: the request id its `TailFile` went out under.
fn press_tail(h: &mut Harness<'_>) -> String {
    h.click_on(TAIL_BUTTON);
    let sent = h.sent();
    let requests = tail_requests(&sent);
    assert_eq!(requests.len(), 1, "one TailFile: {sent:?}");
    requests.into_iter().flatten().next().unwrap_or_default()
}

fn file_tail(data: &[u8], reset: bool) -> DaemonMessage {
    DaemonMessage::FileTail {
        session_id: "s1".to_owned(),
        path: LOG.to_owned(),
        data_b64: B64.encode(data),
        offset: 0,
        reset,
    }
}

/// The lines `LOG`'s box draws; none while it is closed.
fn tail_lines(h: &mut Harness<'_>) -> Option<Vec<String>> {
    h.root(|root, _| root.chat_tail_lines("s1", LOG))
}

#[gpui::test]
fn pressing_tail_sends_tail_file_and_shows_the_chunks(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = tail_offered(cx, &dir);
    assert!(painted(&mut h, TAIL_BUTTON), "the card offers its file");
    assert_eq!(tail_lines(&mut h), None, "no box before the press");
    let request = press_tail(&mut h);
    assert!(!request.is_empty(), "the TailFile carries a request id");
    assert_eq!(tail_lines(&mut h), Some(vec!["No output yet".to_owned()]));
    assert!(painted(&mut h, TAIL_BOX), "the box opens under the card");
    h.send(file_tail(b"\x1b[32mone\x1b[0m\r\ntwo\n", true));
    h.send(file_tail(b"three\n", false));
    assert_eq!(
        tail_lines(&mut h),
        Some(vec!["one".to_owned(), "two".to_owned(), "three".to_owned()])
    );
    assert!(
        h.root(|root, _| root.chat_tool_lines("s1", "t1"))
            .is_empty(),
        "the press leaves the card folded"
    );
}

#[gpui::test]
fn a_second_press_closes_the_box_and_sends_untail_file(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = tail_offered(cx, &dir);
    press_tail(&mut h);
    h.send(file_tail(b"one\n", true));
    assert!(painted(&mut h, TAIL_BOX));
    h.click_on(TAIL_BUTTON);
    let sent = h.sent();
    assert_eq!(untails(&sent), 1, "the follow stops: {sent:?}");
    assert_eq!(tail_lines(&mut h), None);
    assert!(!drawn_now(&mut h, TAIL_BOX), "the box is gone");
}

#[gpui::test]
fn chunks_before_a_reset_are_dropped(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = tail_offered(cx, &dir);
    press_tail(&mut h);
    h.send(file_tail(b"queued before the follow\n", false));
    assert_eq!(tail_lines(&mut h), Some(vec!["No output yet".to_owned()]));
    h.send(file_tail(b"fresh\n", true));
    assert_eq!(tail_lines(&mut h), Some(vec!["fresh".to_owned()]));
}

#[gpui::test]
fn a_reconnect_asks_for_open_tails_again(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = tail_offered(cx, &dir);
    let first = press_tail(&mut h);
    h.send(file_tail(b"before\n", true));
    h.send_welcome(support::PROTOCOL);
    h.load(&Fixture::single(session("s1").build()));
    let sent = h.sent();
    let load = load_request(&sent).expect("the new connection asks for the chat");
    let again: Vec<String> = tail_requests(&sent).into_iter().flatten().collect();
    assert_eq!(again.len(), 1, "the open box asks again: {sent:?}");
    assert!(
        again.iter().all(|id| *id != first),
        "under a fresh request id"
    );
    assert_eq!(untails(&sent), 0, "a new connection untails nothing");
    h.send(reply(Some(&load), vec![user("u1"), tailing_tool("t1")]));
    h.send(file_tail(b"late from the old follow\n", false));
    assert_eq!(tail_lines(&mut h), Some(vec!["before".to_owned()]));
    h.send(file_tail(b"after\n", true));
    assert_eq!(tail_lines(&mut h), Some(vec!["after".to_owned()]));
}

#[gpui::test]
fn no_tail_button_without_the_file_tail_feature(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    h.send(reply(Some(&id), vec![user("u1"), tailing_tool("t1")]));
    assert!(painted(&mut h, "chat-row-p1-t1"), "the card is drawn");
    assert!(!painted(&mut h, TAIL_BUTTON), "but offers no Tail");
}

#[gpui::test]
fn a_refused_tail_shows_the_error_in_its_box(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = tail_offered(cx, &dir);
    let request = press_tail(&mut h);
    let message = "refusing to follow \"C:/work/build.log\": outside the session's folders";
    h.send(DaemonMessage::Error {
        message: message.to_owned(),
        code: Some(INVALID_REQUEST.to_owned()),
        request_id: Some(request),
    });
    assert_eq!(tail_lines(&mut h), Some(vec![message.to_owned()]));
    assert!(painted(&mut h, TAIL_BOX), "the box stays open");
    assert!(
        tail_requests(&h.sent()).is_empty(),
        "nothing is asked again"
    );
}

#[gpui::test]
fn the_box_closes_when_its_card_leaves_the_chat(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = tail_offered(cx, &dir);
    press_tail(&mut h);
    h.send(file_tail(b"one\n", true));
    h.send(reply(None, vec![user("u2")]));
    assert_eq!(tail_lines(&mut h), None, "the box closed with its card");
    assert_eq!(untails(&h.sent()), 1, "and its follow stopped");
}

#[gpui::test]
fn toggling_to_the_terminal_closes_the_box(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = tail_offered(cx, &dir);
    press_tail(&mut h);
    h.click_on(TO_TERMINAL);
    let sent = h.sent();
    assert_eq!(unloads(&sent), 1);
    assert_eq!(
        untails(&sent),
        1,
        "the unloaded chat's box closes: {sent:?}"
    );
    assert_eq!(tail_lines(&mut h), None);
}

#[gpui::test]
fn a_reconnect_keeps_an_open_tail_when_the_reload_reply_is_empty(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = tail_offered(cx, &dir);
    press_tail(&mut h);
    h.send(file_tail(b"before\n", true));
    h.send_welcome(support::PROTOCOL);
    h.load(&Fixture::single(session("s1").build()));
    let load = load_request(&h.sent()).expect("the new connection asks for the chat");
    h.send(reply(Some(&load), Vec::new()));
    assert_eq!(untails(&h.sent()), 0, "the follow just asked for stays");
    assert!(tail_lines(&mut h).is_some(), "the box stays open");
    h.send(reply(None, vec![user("u1"), tailing_tool("t1")]));
    h.send(file_tail(b"after\n", true));
    assert_eq!(tail_lines(&mut h), Some(vec!["after".to_owned()]));
}

#[gpui::test]
fn a_wheel_over_a_tail_box_leaves_the_chat_still(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = opened(cx, &dir);
    file_tail_daemon(&mut h);
    let mut rows: Vec<ChatEntry> = (0..30).map(|n| user(&format!("u{n}"))).collect();
    rows.push(tailing_tool("t1"));
    h.send(reply(Some(&id), rows));
    press_tail(&mut h);
    let lines: Vec<String> = (0..40).map(|n| format!("line {n}")).collect();
    let log = lines.join("\n") + "\n";
    h.send(file_tail(log.as_bytes(), true));
    let chat_before = h.root(|root, _| root.chat_scroll_offset("p1"));
    assert_eq!(
        h.root(|root, _| root.chat_tail_following("s1", LOG)),
        Some(true)
    );
    let at = h.bounds(TAIL_BOX).center();
    h.cx.simulate_event(ScrollEvent {
        position: at,
        delta: ScrollDelta::Pixels(point(px(0.0), px(60.0))),
        modifiers: Modifiers::none(),
        touch_phase: TouchPhase::Moved,
        source: gpui::ScrollSource::Pointer,
    });
    h.cx.run_until_parked();
    assert_eq!(
        h.root(|root, _| root.chat_tail_following("s1", LOG)),
        Some(false),
        "the box stops following"
    );
    assert_eq!(
        h.root(|root, _| root.chat_scroll_offset("p1")),
        chat_before,
        "the chat keeps its place"
    );
}

/// Whether `sent` holds a `WatchDashboard` turning the watch `on`.
fn watch_sent(sent: &[ClientMessage], on: bool) -> bool {
    sent.iter()
        .any(|msg| matches!(msg, ClientMessage::WatchDashboard { on: was } if *was == on))
}

#[gpui::test]
fn a_shown_chat_watches_its_hosts_facts_and_hiding_it_unwatches(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir);
    assert!(
        !watch_sent(&h.sent(), true),
        "a terminal pane watches nothing"
    );

    h.click_on(TO_CHAT);
    assert!(
        watch_sent(&h.sent(), true),
        "the chat's host is watched with no Dashboard tab"
    );

    h.click_on(TO_TERMINAL);
    assert!(
        watch_sent(&h.sent(), false),
        "hiding the chat ends the watch"
    );
}

#[gpui::test]
fn a_chat_pane_header_shows_its_context_and_a_terminal_pane_none(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        sessions: vec![session("s1").build()],
        tabs: vec![tab("t1", &split_with("s1"))],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.cx.simulate_resize(gpui::size(px(1920.0), px(1200.0)));
    h.answer_scrollback("s1", b"");
    h.click_on(TO_CHAT);
    h.send(facts_message(vec![facts("s1").context(41).build()]));

    assert!(painted(&mut h, "pane-p1-context=ctx 41%"));
    assert!(!painted(&mut h, "pane-p1-context-hot"), "41% is not hot");
    assert!(
        !painted(&mut h, "pane-p2-context"),
        "a pane drawing its terminal draws no label"
    );
}

/// The least width a pane header leaves the session name, as
/// `pane_header_fit::TITLE_MIN` has it.
const TITLE_MIN: f32 = 120.0;
/// A header button's width, as `grid_view::HEADER_BUTTON_SIZE` has it.
const HEADER_BUTTON: f32 = 26.0;
/// The header row's padding, its left and right sides together.
const HEADER_PAD: f32 = 20.0;
/// The gap a header row puts between two of its items.
const HEADER_GAP: f32 = 8.0;

/// The width the element `selector` tags was painted at.
fn width_of(h: &mut Harness<'_>, selector: &str) -> f32 {
    h.bounds(selector).size.width / px(1.0)
}

#[gpui::test]
fn an_armed_stop_hides_the_context_label_in_a_collapsed_header(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        sessions: vec![session("s1").build()],
        tabs: vec![tab("t1", &split_with("s1"))],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.cx.simulate_resize(gpui::size(px(1920.0), px(1200.0)));
    h.answer_scrollback("s1", b"");
    h.click_on(TO_CHAT);
    h.send(facts_message(vec![facts("s1").context(41).build()]));

    // The collapsed row's width with Stop unarmed, as `header_fit` measures
    // it: the pads, the status glyph, the title at its least width, the
    // runtime chip, the context label, ⋯ and Close, a gap between each two.
    let unarmed = HEADER_PAD
        + width_of(&mut h, "pane-glyph-p1")
        + TITLE_MIN
        + width_of(&mut h, "pane-chip-p1-runtime")
        + width_of(&mut h, "pane-p1-context")
        + 2.0 * HEADER_BUTTON
        + 6.0 * HEADER_GAP;
    // Armed, Confirm stop and Cancel stay drawn beside ⋯: two more items and
    // their gaps.
    h.click_on("pane-stop-p1");
    let armed_extra = width_of(&mut h, "pane-stop-confirm-p1")
        + width_of(&mut h, "pane-stop-cancel-p1")
        + 2.0 * HEADER_GAP;
    h.click_on("pane-stop-cancel-p1");

    // A card between the two widths: the label fits beside ⋯ alone, and not
    // beside the armed Stop's buttons too.
    let wanted = unarmed + armed_extra / 2.0;
    let wide = width_of(&mut h, "pane-frame-p1");
    let (window, height) = (h.window_width(), h.window_height());
    h.cx.simulate_resize(gpui::size(px(window - 2.0 * (wide - wanted)), px(height)));
    h.cx.run_until_parked();

    let card = width_of(&mut h, "pane-frame-p1");
    assert!(
        (card - wanted).abs() < 3.0,
        "the window narrows p1's card to about {wanted} px: {card}"
    );
    assert!(
        h.root(|root, cx| root.pane_header_collapsed("p1", cx)),
        "a {card} px card moves p1's buttons into ⋯"
    );
    let frame = h.bounds("pane-frame-p1");
    let label = h.bounds("pane-p1-context");
    assert!(
        label.size.height > px(0.0)
            && label.left() >= frame.left()
            && label.right() <= frame.right() + px(0.5),
        "with Stop unarmed the collapsed header paints the label in its card: label {label:?}, card {frame:?}"
    );

    h.click_on("pane-overflow-p1");
    h.click_on("pane-overflow-p1-stop");

    assert!(
        h.in_model("pane-stop-confirm-p1"),
        "the Stop row arms the stop"
    );
    assert!(
        h.root(|root, cx| root.pane_header_collapsed("p1", cx)),
        "the header stays collapsed"
    );
    assert!(
        !painted(&mut h, "pane-p1-context"),
        "Confirm stop and Cancel take the label's room, so the header drops it"
    );
}

/// A tool result the agent cut at its cap: its marker on the last line.
const CUT_RESULT: &str = "line one\nline two\n\u{2026} 400 more lines";
/// The whole text of a tool result, which Show all fetches.
const WHOLE_RESULT: &str = "line one\nline two\nline three";
const SHOW_ALL: &str = "chat-show-all-t1";
const SHOW_ALL_MISSING: &str = "chat-show-all-missing-t1";

/// `p1` on its chat over a host listing `features`, holding the cut tool
/// card `t1` open, everything sent so far drained.
fn cut_result<'a>(cx: &'a mut TestAppContext, dir: &TestDir, features: &[&str]) -> Harness<'a> {
    let mut h = loaded_listing(cx, dir, features);
    h.send(append(vec![user("u1"), tool("t1", Some(CUT_RESULT))], &[]));
    h.sent();
    h.click_on("chat-tool-done-p1-t1");
    h
}

/// The request ids of the `GetToolResult`s for `t1` sent since the last
/// drain.
fn show_all_requests(h: &mut Harness<'_>) -> Vec<Option<String>> {
    h.sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::GetToolResult {
                session_id,
                tool_id,
                request_id,
            } if session_id == "s1" && tool_id == "t1" => Some(request_id),
            _ => None,
        })
        .collect()
}

/// The Show all button pressed: the request id its `GetToolResult` went out
/// under.
fn press_show_all(h: &mut Harness<'_>) -> String {
    h.click_on(SHOW_ALL);
    let requests = show_all_requests(h);
    assert_eq!(requests.len(), 1, "one GetToolResult: {requests:?}");
    requests.into_iter().flatten().next().unwrap_or_default()
}

/// The daemon's answer to the card's Show all: `text`, and no whole text at
/// all when it is `None`.
fn tool_result(request_id: &str, text: Option<&str>) -> DaemonMessage {
    DaemonMessage::ToolResult {
        session_id: "s1".to_owned(),
        tool_id: "t1".to_owned(),
        text: text.map(str::to_owned),
        truncated: false,
        request_id: Some(request_id.to_owned()),
    }
}

/// The result text the card draws for `t1`.
fn result_text(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.chat_tool_result_text("s1", "t1"))
}

#[gpui::test]
fn show_all_appears_on_a_cut_result_and_swaps_in_the_full_text(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = cut_result(cx, &dir, &[FEATURE_CHAT_TOOL_RESULT]);
    assert!(painted(&mut h, SHOW_ALL), "the cut result offers the rest");
    assert_eq!(result_text(&mut h).as_deref(), Some(CUT_RESULT));
    let request_id = press_show_all(&mut h);
    assert!(!request_id.is_empty(), "the request carries an id");
    h.send(tool_result(&request_id, Some(WHOLE_RESULT)));
    assert_eq!(
        result_text(&mut h).as_deref(),
        Some(WHOLE_RESULT),
        "the whole text takes the cut one's place"
    );
    assert!(
        !drawn_now(&mut h, SHOW_ALL),
        "nothing more is left to fetch"
    );
}

#[gpui::test]
fn show_all_is_not_offered_on_a_host_without_the_feature(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = cut_result(cx, &dir, &[]);
    assert!(
        !painted(&mut h, SHOW_ALL),
        "a host that answers for no whole result offers none"
    );
}

#[gpui::test]
fn show_all_is_not_offered_on_an_uncut_result(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded_listing(cx, &dir, &[FEATURE_CHAT_TOOL_RESULT]);
    h.send(append(
        vec![user("u1"), tool("t1", Some(WHOLE_RESULT))],
        &[],
    ));
    h.sent();
    h.click_on("chat-tool-done-p1-t1");
    assert!(
        !painted(&mut h, SHOW_ALL),
        "nothing of an uncut result is missing"
    );
}

/// A script call `id` whose card holds `result` as its text.
fn script_tool(id: &str, result: &str) -> ChatEntry {
    ChatEntry::Tool {
        id: id.to_owned(),
        name: "REPL".to_owned(),
        summary: "eval".to_owned(),
        input: "{}".to_owned(),
        result: Some(ToolOutcome {
            text: result.to_owned(),
            is_error: false,
        }),
        parent_id: None,
        tail_paths: Vec::new(),
        kind: ToolKind::Script,
        script: Some(ScriptFacts {
            description: "eval".to_owned(),
            ..ScriptFacts::default()
        }),
    }
}

#[gpui::test]
fn show_all_is_not_offered_on_a_script_card(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded_listing(cx, &dir, &[FEATURE_CHAT_TOOL_RESULT]);
    h.send(append(vec![user("u1"), script_tool("t1", CUT_RESULT)], &[]));
    h.sent();
    assert!(
        painted(&mut h, "chat-script-done-p1-t1"),
        "a script call draws its own line, not a tool card"
    );
    h.click_on("chat-script-done-p1-t1");
    assert!(
        !painted(&mut h, SHOW_ALL),
        "a script's result comes from a poll event, not the call's own"
    );
}

#[gpui::test]
fn a_missing_result_says_the_rest_is_not_in_the_transcript(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = cut_result(cx, &dir, &[FEATURE_CHAT_TOOL_RESULT]);
    let request_id = press_show_all(&mut h);
    h.send(tool_result(&request_id, None));
    assert!(
        painted(&mut h, SHOW_ALL_MISSING),
        "the card says the transcript holds no whole text"
    );
    assert!(
        !drawn_now(&mut h, SHOW_ALL),
        "no button stands beside the line"
    );
}

#[gpui::test]
fn a_second_show_all_press_while_loading_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = cut_result(cx, &dir, &[FEATURE_CHAT_TOOL_RESULT]);
    press_show_all(&mut h);
    assert!(painted(&mut h, SHOW_ALL), "the button stays while loading");
    h.click_on(SHOW_ALL);
    assert!(
        show_all_requests(&mut h).is_empty(),
        "one request for a result at a time"
    );
}

#[gpui::test]
fn show_all_works_again_after_a_reconnect_while_loading(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = cut_result(cx, &dir, &[FEATURE_CHAT_TOOL_RESULT]);
    let first = press_show_all(&mut h);
    h.send_welcome(support::PROTOCOL);
    h.load(&Fixture::single(session("s1").build()));
    let sent = h.sent();
    let load = load_request(&sent).expect("the new connection asks for the chat again");
    h.send(reply(
        Some(&load),
        vec![user("u1"), tool("t1", Some(CUT_RESULT))],
    ));
    h.sent();
    h.click_on("chat-tool-done-p1-t1");
    assert!(
        painted(&mut h, SHOW_ALL),
        "the card offers the rest once more"
    );
    let again = press_show_all(&mut h);
    assert_ne!(again, first, "under a request id of its own");
}
