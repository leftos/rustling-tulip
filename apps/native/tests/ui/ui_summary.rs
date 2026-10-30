//! Summary pane specs: the header's Summary segment, the pane menu and
//! Ctrl+Shift+U switch a pane to its session's summary, which loads the
//! summary and the chat; the summary draws one line per stretch under the
//! pinned question, each line opening into its chat entries, with the
//! running-work strip and the composer under the lines. The view is
//! remembered per session, an unasked `Summary` resets the lines, and a
//! reply to an older load is dropped.

use crate::support;

use chrono::{DateTime, TimeDelta, Utc};
use gpui::{TestAppContext, px};
use protocol::{
    ChatEntry, ChatOption, ChatPrompt, ChatPromptKind, ChatScreen, ClientMessage, ComposerState,
    DaemonMessage, GridNode, SplitDirection, StretchLine, SummarySource, WorkShell, WorkSubagent,
};
use rustling_tulip_native::chat::PaneView;
use support::{Fixture, Harness, TestDir, pane, session, tab};

const TO_TERMINAL: &str = "pane-view-terminal-p1";
const TO_CHAT: &str = "pane-view-chat-p1";
const TO_SUMMARY: &str = "pane-view-summary-p1";

/// `s1` alone in pane `p1`, its scrollback answered and everything sent so
/// far drained.
fn attached<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = Harness::with(cx, dir, &Fixture::single(session("s1").build()));
    h.answer_scrollback("s1", b"");
    h.sent();
    h
}

/// The request ids of the `LoadChat` and the `LoadSummary` for `s1` among
/// `sent`.
fn load_requests(sent: &[ClientMessage]) -> (Option<String>, Option<String>) {
    let chat = sent.iter().find_map(|msg| match msg {
        ClientMessage::LoadChat {
            session_id,
            request_id,
        } if session_id == "s1" => request_id.clone(),
        _ => None,
    });
    let summary = sent.iter().find_map(|msg| match msg {
        ClientMessage::LoadSummary {
            session_id,
            request_id,
        } if session_id == "s1" => request_id.clone(),
        _ => None,
    });
    (chat, summary)
}

/// How many `UnloadChat` and `UnloadSummary` for `s1` are among `sent`.
fn unloads(sent: &[ClientMessage]) -> (usize, usize) {
    let chat = sent
        .iter()
        .filter(|msg| matches!(msg, ClientMessage::UnloadChat { session_id } if session_id == "s1"))
        .count();
    let summary = sent
        .iter()
        .filter(
            |msg| matches!(msg, ClientMessage::UnloadSummary { session_id } if session_id == "s1"),
        )
        .count();
    (chat, summary)
}

/// `p1` switched to its summary, with the request ids its `LoadChat` and
/// `LoadSummary` went out under.
fn opened<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> (Harness<'a>, String, String) {
    let mut h = attached(cx, dir);
    h.click_on(TO_SUMMARY);
    let (chat, summary) = load_requests(&h.sent());
    let chat = chat.expect("the summary pane asks for the chat");
    let summary = summary.expect("and for the summary");
    (h, chat, summary)
}

fn chat_reply(request_id: &str, entries: Vec<ChatEntry>) -> DaemonMessage {
    DaemonMessage::Chat {
        session_id: "s1".to_owned(),
        entries,
        hidden: 0,
        screen: ChatScreen {
            composer: ComposerState::Ready,
            prompt: None,
        },
        request_id: Some(request_id.to_owned()),
    }
}

fn summary_reply(request_id: Option<&str>, lines: Vec<StretchLine>) -> DaemonMessage {
    DaemonMessage::Summary {
        session_id: "s1".to_owned(),
        lines,
        request_id: request_id.map(str::to_owned),
    }
}

fn summary_append(lines: Vec<StretchLine>) -> DaemonMessage {
    DaemonMessage::SummaryAppend {
        session_id: "s1".to_owned(),
        lines,
    }
}

fn line(id: &str, from: &str, to: &str, text: &str, source: SummarySource) -> StretchLine {
    StretchLine {
        id: id.to_owned(),
        from_entry: from.to_owned(),
        to_entry: to.to_owned(),
        started_at: None,
        ended_at: None,
        text: text.to_owned(),
        source,
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

/// The chat `u1 t1 a1 e1 u2 a2` and its two stretches, `u1` to `e1` and
/// `u2` to `a2`, loaded into `p1`'s summary.
fn two_stretches(h: &mut Harness<'_>, chat: &str, summary: &str) {
    h.send(chat_reply(
        chat,
        ["u1", "t1", "a1", "e1", "u2", "a2"]
            .iter()
            .map(|id| {
                if id.starts_with('u') {
                    user(id)
                } else {
                    answer(id)
                }
            })
            .collect(),
    ));
    h.send(summary_reply(
        Some(summary),
        vec![
            line(
                "u1",
                "u1",
                "e1",
                "Fix the login bug (2m, 1 tool)",
                SummarySource::Template,
            ),
            line(
                "u2",
                "u2",
                "a2",
                "Explained the retry loop",
                SummarySource::Model,
            ),
        ],
    ));
}

fn line_ids(h: &mut Harness<'_>) -> Vec<String> {
    h.root(|root, _| root.summary_lines("s1"))
        .into_iter()
        .map(|line| line.id)
        .collect()
}

fn painted(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).size.height > px(0.0)
}

/// Whether the element `selector` tags is drawn now: gpui keeps an
/// element's last bounds after it stops drawing, so the window is narrowed
/// and the element must be laid out again.
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

fn view(h: &mut Harness<'_>, pane_id: &str) -> PaneView {
    let pane_id = pane_id.to_owned();
    h.root(move |root, _| root.pane_view(&pane_id))
}

fn saved_views(dir: &TestDir) -> serde_json::Value {
    let text = std::fs::read_to_string(dir.path().join("native-ui.json")).expect("a saved layout");
    let file: serde_json::Value = serde_json::from_str(&text).expect("native-ui.json is JSON");
    file["session_views"].clone()
}

fn split_with(second: &str) -> GridNode {
    GridNode::Split {
        direction: SplitDirection::Horizontal,
        ratio: 0.5,
        first: Box::new(pane("p1", Some("s1"))),
        second: Box::new(pane("p2", Some(second))),
    }
}

fn permission(id: &str) -> ChatPrompt {
    ChatPrompt {
        id: id.to_owned(),
        kind: ChatPromptKind::Permission,
        subject: "calc.py".to_owned(),
        body: "Do you want to make this edit?".to_owned(),
        options: [(1, "Yes"), (2, "No")]
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
fn the_summary_view_lists_one_line_per_stretch(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, chat, summary) = opened(cx, &dir);
    assert_eq!(view(&mut h, "p1"), PaneView::Summary);
    assert!(
        painted(&mut h, "pane-summary-p1"),
        "the summary body is drawn"
    );
    assert!(painted(&mut h, "summary-loading"), "a note while it loads");
    assert!(
        !drawn_now(&mut h, "pane-grid-p1"),
        "the terminal is not drawn beside it"
    );
    two_stretches(&mut h, &chat, &summary);
    assert_eq!(line_ids(&mut h), ["u1", "u2"]);
    let first = h.bounds("summary-line-p1-u1");
    let second = h.bounds("summary-line-p1-u2");
    assert!(first.size.height > px(0.0) && second.size.height > px(0.0));
    assert!(
        first.bottom() <= second.top(),
        "the lines keep the daemon's order"
    );
    assert!(
        !painted(&mut h, "summary-row-p1-u1"),
        "a closed line draws none of its entries"
    );
}

#[gpui::test]
fn the_pinned_question_sits_on_top(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, chat, summary) = opened(cx, &dir);
    two_stretches(&mut h, &chat, &summary);
    h.send(DaemonMessage::ChatScreenChanged {
        session_id: "s1".to_owned(),
        screen: ChatScreen {
            composer: ComposerState::Dialog,
            prompt: Some(permission("p-1")),
        },
    });
    let card = h.bounds("chat-card");
    assert!(card.size.height > px(0.0), "the card is pinned");
    assert!(
        card.bottom() <= h.bounds("summary-line-p1-u1").top(),
        "above the lines"
    );
    h.sent();
    h.click_on("chat-card-1");
    let answered = h.sent().into_iter().any(|msg| {
        matches!(msg, ClientMessage::ChatAnswer { session_id, prompt_id, .. }
            if session_id == "s1" && prompt_id == "p-1")
    });
    assert!(answered, "the card answers as in the chat");
}

#[gpui::test]
fn a_line_expands_into_its_entries(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, chat, summary) = opened(cx, &dir);
    two_stretches(&mut h, &chat, &summary);
    h.click_on("summary-line-p1-u1");
    assert!(h.root(|root, _| root.summary_line_open("s1", "u1")));
    for id in ["u1", "t1", "a1", "e1"] {
        assert!(painted(&mut h, &format!("summary-row-p1-{id}")), "{id}");
    }
    for id in ["u2", "a2"] {
        assert!(!painted(&mut h, &format!("summary-row-p1-{id}")), "{id}");
    }
    assert!(
        h.bounds("summary-row-p1-e1").bottom() <= h.bounds("summary-line-p1-u2").top(),
        "the entries sit under their line"
    );
    assert!(!painted(&mut h, "summary-earlier-p1-u1"));
    h.click_on("summary-line-p1-u1");
    assert!(!h.root(|root, _| root.summary_line_open("s1", "u1")));
    assert!(
        !drawn_now(&mut h, "summary-row-p1-t1"),
        "a second click closes it"
    );
}

#[gpui::test]
fn an_expanded_line_whose_start_is_gone_says_earlier_messages_are_not_loaded(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let (mut h, chat, summary) = opened(cx, &dir);
    h.send(chat_reply(
        &chat,
        vec![answer("t1"), answer("e1"), user("u2")],
    ));
    h.send(summary_reply(
        Some(&summary),
        vec![line("u1", "u1", "e1", "Long turn", SummarySource::Template)],
    ));
    h.click_on("summary-line-p1-u1");
    let note = h.bounds("summary-earlier-p1-u1");
    assert!(note.size.height > px(0.0), "the note is drawn");
    assert!(
        note.bottom() <= h.bounds("summary-row-p1-t1").top(),
        "over the entries"
    );
    assert!(painted(&mut h, "summary-row-p1-e1"));
    assert!(!painted(&mut h, "summary-row-p1-u2"));
}

#[gpui::test]
fn the_strip_and_composer_show_under_the_lines(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, chat, summary) = opened(cx, &dir);
    two_stretches(&mut h, &chat, &summary);
    let now: DateTime<Utc> = h.root(|root, _| root.wall_now());
    h.send(DaemonMessage::ChatWork {
        session_id: "s1".to_owned(),
        subagents: vec![WorkSubagent {
            id: "a1".to_owned(),
            description: "Sleeper".to_owned(),
            agent_type: Some("general-purpose".to_owned()),
            since: Some(now - TimeDelta::minutes(2)),
            from_screen: false,
        }],
        shells: vec![WorkShell {
            id: "b1".to_owned(),
            description: "Run the specs".to_owned(),
            command: Some("cargo nextest run".to_owned()),
            since: Some(now - TimeDelta::minutes(3)),
            from_screen: false,
        }],
        wakeup_at: None,
    });
    let strip = h.bounds("work-strip");
    let composer = h.bounds("chat-composer");
    assert!(strip.size.height > px(0.0), "the strip shows");
    assert!(composer.size.height > px(0.0), "the composer shows");
    assert!(h.bounds("summary-line-p1-u2").bottom() <= strip.top());
    assert!(
        strip.bottom() <= composer.top(),
        "the composer is under the strip"
    );
}

#[gpui::test]
fn the_header_switches_between_terminal_chat_and_summary(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir);
    for selector in [TO_TERMINAL, TO_CHAT, TO_SUMMARY] {
        assert!(painted(&mut h, selector), "{selector} is in the header");
    }
    h.click_on(TO_CHAT);
    assert_eq!(view(&mut h, "p1"), PaneView::Chat);
    assert_eq!(
        load_requests(&h.sent()).1,
        None,
        "a chat pane loads no summary"
    );
    h.click_on(TO_SUMMARY);
    assert_eq!(view(&mut h, "p1"), PaneView::Summary);
    let sent = h.sent();
    let (chat, summary) = load_requests(&sent);
    assert!(summary.is_some(), "the summary loads: {sent:?}");
    assert_eq!(chat, None, "the chat stays loaded");
    assert_eq!(unloads(&sent), (0, 0));
    h.right_click_on("pane-header-p1");
    assert_eq!(
        h.root(|root, _| root.pane_view_menu_labels()),
        ["Show terminal", "Show chat"],
        "the menu leaves out the view shown"
    );
    h.click_on("session-menu-pane-view");
    assert_eq!(view(&mut h, "p1"), PaneView::Terminal);
    assert_eq!(unloads(&h.sent()), (1, 1), "both unload with the last pane");
    h.right_click_on("pane-header-p1");
    assert_eq!(
        h.root(|root, _| root.pane_view_menu_labels()),
        ["Show chat", "Show summary"]
    );
    h.click_on("session-menu-pane-view-summary");
    assert_eq!(view(&mut h, "p1"), PaneView::Summary);
    h.click_on(TO_TERMINAL);
    assert_eq!(view(&mut h, "p1"), PaneView::Terminal);
}

#[gpui::test]
fn ctrl_shift_u_opens_and_leaves_the_summary(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir);
    h.keys("ctrl-shift-u");
    assert_eq!(view(&mut h, "p1"), PaneView::Summary);
    assert!(
        load_requests(&h.sent()).1.is_some(),
        "the key loads the summary"
    );
    h.keys("ctrl-shift-u");
    assert_eq!(view(&mut h, "p1"), PaneView::Terminal);
    h.keys("ctrl-shift-m");
    assert_eq!(view(&mut h, "p1"), PaneView::Chat);
    h.keys("ctrl-shift-u");
    assert_eq!(view(&mut h, "p1"), PaneView::Summary, "from the chat too");
    h.keys("ctrl-shift-m");
    assert_eq!(
        view(&mut h, "p1"),
        PaneView::Terminal,
        "Ctrl+Shift+M leaves the summary for the terminal"
    );
    assert!(h.sent_input("s1").is_empty(), "the terminal never saw them");
}

#[gpui::test]
fn the_summary_view_is_remembered_per_session(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    std::fs::write(
        dir.path().join("native-ui.json"),
        r#"{ "session_views": { "local": { "s1": "summary" } } }"#,
    )
    .expect("seed native-ui.json");
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    assert_eq!(
        view(&mut h, "p1"),
        PaneView::Summary,
        "a restarted client opens s1 on its summary"
    );
    assert!(load_requests(&h.sent()).1.is_some(), "and asks for it");
    h.send(DaemonMessage::Tabs {
        tabs: vec![tab("t1", &split_with("s1"))],
    });
    assert_eq!(
        view(&mut h, "p2"),
        PaneView::Summary,
        "a new pane opens there"
    );
    h.click_on(TO_CHAT);
    assert_eq!(
        saved_views(&dir),
        serde_json::json!({ "local": { "s1": "chat" } })
    );
    h.click_on(TO_SUMMARY);
    assert_eq!(
        saved_views(&dir),
        serde_json::json!({ "local": { "s1": "summary" } })
    );
    h.send(DaemonMessage::Sessions {
        sessions: vec![session("s1").build(), session("s2").build()],
    });
    h.send(DaemonMessage::Tabs {
        tabs: vec![tab("t1", &split_with("s2"))],
    });
    assert_eq!(
        view(&mut h, "p2"),
        PaneView::Terminal,
        "a new session opens on its terminal"
    );
}

#[gpui::test]
fn a_summary_pane_loads_the_chat_and_unloads_it_with_the_last_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        sessions: vec![session("s1").build()],
        tabs: vec![tab("t1", &split_with("s1"))],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.answer_scrollback("s1", b"");
    h.sent();
    h.click_on(TO_SUMMARY);
    let (chat, summary) = load_requests(&h.sent());
    assert!(chat.is_some() && summary.is_some(), "both load");
    h.click_on("pane-view-chat-p2");
    let sent = h.sent();
    assert_eq!(
        load_requests(&sent),
        (None, None),
        "nothing more loads: {sent:?}"
    );
    h.click_on(TO_TERMINAL);
    assert_eq!(unloads(&h.sent()), (0, 1), "the chat pane keeps the chat");
    h.click_on("pane-view-terminal-p2");
    assert_eq!(unloads(&h.sent()), (1, 0), "the last pane unloads the chat");
}

#[gpui::test]
fn an_unsolicited_summary_resets_the_lines_and_drops_gone_open_ids(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, chat, summary) = opened(cx, &dir);
    two_stretches(&mut h, &chat, &summary);
    h.click_on("summary-line-p1-u1");
    h.click_on("summary-line-p1-u2");
    h.send(summary_reply(
        None,
        vec![
            line("u2", "u2", "a2", "Explained it", SummarySource::Model),
            line("u3", "u3", "a3", "New turn", SummarySource::Template),
        ],
    ));
    assert_eq!(line_ids(&mut h), ["u2", "u3"]);
    assert!(
        h.root(|root, _| root.summary_line_open("s1", "u2")),
        "a surviving line stays open"
    );
    assert!(
        !h.root(|root, _| root.summary_line_open("s1", "u1")),
        "a gone line is forgotten"
    );
    h.send(summary_append(vec![
        line("u3", "u3", "a4", "New turn, longer", SummarySource::Model),
        line("u4", "u4", "a5", "Another", SummarySource::Template),
    ]));
    assert_eq!(line_ids(&mut h), ["u2", "u3", "u4"]);
    assert!(painted(&mut h, "summary-line-p1-u4"));
}

#[gpui::test]
fn a_stale_summary_reply_is_dropped(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, chat, summary) = opened(cx, &dir);
    h.send(chat_reply(&chat, vec![user("u1")]));
    h.send(summary_reply(
        Some("older"),
        vec![line("u1", "u1", "u1", "Stale", SummarySource::Template)],
    ));
    assert!(
        line_ids(&mut h).is_empty(),
        "an older load's reply is dropped"
    );
    assert!(
        painted(&mut h, "summary-loading"),
        "the load is still awaited"
    );
    h.send(summary_reply(
        Some(&summary),
        vec![line("u1", "u1", "u1", "Fresh", SummarySource::Template)],
    ));
    assert_eq!(line_ids(&mut h), ["u1"]);
}

/// The chat `u1 a1 … a60` and its one stretch, taller than the pane once
/// open, loaded into `p1`'s summary.
fn one_long_stretch(h: &mut Harness<'_>, chat: &str, summary: &str) {
    let mut entries = vec![user("u1")];
    entries.extend((1..=60).map(|n| answer(&format!("a{n}"))));
    h.send(chat_reply(chat, entries));
    h.send(summary_reply(
        Some(summary),
        vec![line(
            "u1",
            "u1",
            "a60",
            "A long turn",
            SummarySource::Template,
        )],
    ));
}

/// Whether what `selector` tags is drawn wholly inside what `container`
/// tags.
fn inside(h: &mut Harness<'_>, selector: &str, container: &str) -> bool {
    let (outer, inner) = (h.bounds(container), h.bounds(selector));
    inner.size.height > px(0.0) && inner.top() >= outer.top() && inner.bottom() <= outer.bottom()
}

#[gpui::test]
fn a_refused_summary_load_shows_the_terminal_with_one_note_and_no_toast(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, chat, summary) = opened(cx, &dir);
    for request_id in [summary, chat] {
        h.send(DaemonMessage::Error {
            message: "session `s1` not found".to_owned(),
            request_id: Some(request_id),
            code: Some("not_found".to_owned()),
        });
    }
    assert_eq!(view(&mut h, "p1"), PaneView::Terminal);
    assert_eq!(
        h.root(|root, _| root.pane_chat_note("p1").map(str::to_owned))
            .as_deref(),
        Some("Could not load the chat: session `s1` not found")
    );
    assert!(painted(&mut h, "pane-chat-note-p1"), "the note is drawn");
    assert!(
        h.root(|root, _| root.toasts().is_empty()),
        "the note is the only word of it"
    );
    assert_eq!(
        unloads(&h.sent()).0,
        0,
        "a refused load has no chat to unload"
    );
}

#[gpui::test]
fn opening_a_line_keeps_it_in_view(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, chat, summary) = opened(cx, &dir);
    one_long_stretch(&mut h, &chat, &summary);
    h.click_on("summary-line-p1-u1");
    assert!(painted(&mut h, "summary-row-p1-a60"), "the line opened");
    assert!(
        inside(&mut h, "summary-line-p1-u1", "pane-summary-p1"),
        "the opened line stays in view"
    );
}

#[gpui::test]
fn switching_from_a_scrolled_summary_to_chat_starts_the_chat_at_its_bottom(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let (mut h, chat, summary) = opened(cx, &dir);
    one_long_stretch(&mut h, &chat, &summary);
    h.click_on("summary-line-p1-u1");
    h.scroll_into_view("pane-summary-p1", "summary-row-p1-a60");
    h.scroll_into_view("pane-summary-p1", "summary-line-p1-u1");
    h.click_on(TO_CHAT);
    assert_eq!(view(&mut h, "p1"), PaneView::Chat);
    assert!(
        inside(&mut h, "chat-row-p1-a60", "pane-chat-p1"),
        "the chat starts at its newest row"
    );
}

#[gpui::test]
fn a_line_opened_before_the_chat_arrives_says_loading_then_shows_its_entries(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let (mut h, chat, summary) = opened(cx, &dir);
    h.send(summary_reply(
        Some(&summary),
        vec![line("u1", "u1", "a1", "Fix it", SummarySource::Template)],
    ));
    h.click_on("summary-line-p1-u1");
    assert!(
        painted(&mut h, "summary-line-loading-p1-u1"),
        "the open line says the chat is loading"
    );
    assert!(
        !painted(&mut h, "summary-earlier-p1-u1"),
        "not that its messages are gone"
    );
    h.send(chat_reply(&chat, vec![user("u1"), answer("a1")]));
    for id in ["u1", "a1"] {
        assert!(painted(&mut h, &format!("summary-row-p1-{id}")), "{id}");
    }
    assert!(
        !drawn_now(&mut h, "summary-line-loading-p1-u1"),
        "the loading note goes once the chat is in"
    );
}
