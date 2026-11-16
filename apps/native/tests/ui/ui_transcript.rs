//! A pane that follows a dispatch run draws the run's transcript the way a
//! terminal shows Claude Code: it follows the run's transcript, draws the
//! reply in place of the recent actions, folds appends by id, ignores a stale
//! reply, unfollows once no pane shows the run, and follows again after a
//! reconnect. A path or URL in a transcript line opens as in a terminal:
//! Ctrl+hover names its gestures, Ctrl+click opens it, Ctrl+Shift+click on
//! a path opens the link menu.

use crate::support;

use std::path::{Path, PathBuf};

use gpui::{
    FontWeight, Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, Pixels, Point,
    TestAppContext, point, px,
};
use protocol::{
    AppearanceOverrides, ChatEntry, ClientMessage, DaemonMessage, GridNode, PaneFollow,
    SessionSnapshot, SplitDirection, ToolKind, ToolOutcome, TranscriptSource,
};
use rustling_tulip_native::target::Target;
use support::{
    DispatchRunBuilder, Fixture, Harness, Opened, REMOTE, TestDir, dispatch_run, follow_pane,
    follow_subagent_pane, pane, repo, session, split, subagent, tab,
};

/// Repo `r1`; tab `t1`: session `s1` in `p1` and a pane `f2` following run
/// `r1` on This PC.
fn fixture() -> Fixture {
    Fixture {
        repos: vec![repo("r1", "C:/r1")],
        sessions: vec![session("s1").in_repo("r1").build()],
        tabs: vec![tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                pane("p1", Some("s1")),
                follow_pane("f2", "r1"),
            ),
        )],
        ..Fixture::default()
    }
}

/// Run `r1`, working, with two recent actions.
fn r1() -> DispatchRunBuilder {
    dispatch_run("r1").actions(&["read a.rs", "edit b.rs"])
}

/// The fixture on a connection with dispatch runs, `r1` listed.
fn listed<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = Harness::with(cx, dir, &fixture());
    h.reconnect_with_features(&[protocol::FEATURE_DISPATCH_RUNS]);
    h.send(DaemonMessage::DispatchRuns {
        runs: vec![r1().build()],
    });
    h
}

fn source() -> TranscriptSource {
    TranscriptSource::DispatchRun {
        run_id: "r1".to_owned(),
    }
}

/// The request id of each `FollowTranscript` of `r1` in `sent`.
fn follows(sent: &[ClientMessage]) -> Vec<Option<String>> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::FollowTranscript {
                source: s,
                request_id,
            } if *s == source() => Some(request_id.clone()),
            _ => None,
        })
        .collect()
}

/// How many `UnfollowTranscript` of `r1` `sent` holds.
fn unfollows(sent: &[ClientMessage]) -> usize {
    sent.iter()
        .filter(
            |msg| matches!(msg, ClientMessage::UnfollowTranscript { source: s } if *s == source()),
        )
        .count()
}

/// The request id of the one follow sent since the last read.
fn follow_id(h: &mut Harness<'_>) -> String {
    let sent = follows(&h.sent());
    assert!(
        matches!(sent.as_slice(), [Some(_)]),
        "one follow with a request id was sent: {sent:?}"
    );
    sent.into_iter().flatten().next().unwrap_or_default()
}

fn answer(id: &str, text: &str) -> ChatEntry {
    ChatEntry::Answer {
        id: id.to_owned(),
        text: text.to_owned(),
        live: false,
    }
}

fn tool(id: &str, summary: &str, result: Option<&str>) -> ChatEntry {
    ChatEntry::Tool {
        id: id.to_owned(),
        name: "Bash".to_owned(),
        summary: summary.to_owned(),
        input: String::new(),
        result: result.map(|text| ToolOutcome {
            text: text.to_owned(),
            is_error: false,
        }),
        parent_id: None,
        tail_paths: Vec::new(),
        kind: ToolKind::Run,
        script: None,
    }
}

fn reply(entries: Vec<ChatEntry>, request_id: Option<String>) -> DaemonMessage {
    DaemonMessage::Transcript {
        source: source(),
        entries,
        hidden: 0,
        request_id,
    }
}

fn lines(h: &mut Harness<'_>) -> Vec<String> {
    h.root(|root, _| root.dispatch_view_lines("f2"))
}

#[gpui::test]
fn following_a_run_sends_follow_transcript(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    h.sent();

    h.reconnect_with_features(&[protocol::FEATURE_DISPATCH_RUNS]);

    let sent = follows(&h.sent());
    assert!(
        matches!(sent.as_slice(), [Some(_)]),
        "one follow of r1 with a request id: {sent:?}"
    );
}

#[gpui::test]
fn the_reply_draws_the_transcript_in_place_of_recent_actions(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = listed(cx, &dir);
    let id = follow_id(&mut h);

    h.send(reply(
        vec![answer("a1", "Looking."), tool("t1", "ls", Some("a.rs"))],
        Some(id),
    ));

    assert_eq!(lines(&mut h), ["● Looking.", "● Bash(ls)", "  ⎿  a.rs"]);
    assert!(h.in_model("dispatch-log-f2"), "the log is drawn");
}

#[gpui::test]
fn before_a_reply_the_pane_shows_recent_actions(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = listed(cx, &dir);

    assert_eq!(lines(&mut h), ["read a.rs", "edit b.rs"]);
}

#[gpui::test]
fn an_append_adds_lines_and_a_result_fills_its_call(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = listed(cx, &dir);
    let id = follow_id(&mut h);
    h.send(reply(vec![tool("t1", "ls", None)], Some(id)));
    assert_eq!(lines(&mut h), ["● Bash(ls)", "  ⎿  Running…"]);

    h.send(DaemonMessage::TranscriptAppend {
        source: source(),
        entries: vec![tool("t1", "ls", Some("a.rs")), answer("a2", "Done.")],
        ended: false,
    });

    assert_eq!(lines(&mut h), ["● Bash(ls)", "  ⎿  a.rs", "● Done."]);
}

#[gpui::test]
fn a_stale_reply_is_ignored(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = listed(cx, &dir);
    let id = follow_id(&mut h);

    h.send(reply(vec![answer("a1", "Old.")], Some("stale".to_owned())));
    assert_eq!(
        lines(&mut h),
        ["read a.rs", "edit b.rs"],
        "a reply to another follow draws nothing"
    );

    h.send(reply(vec![answer("a1", "New.")], Some(id)));
    assert_eq!(lines(&mut h), ["● New."]);
}

#[gpui::test]
fn an_unsolicited_transcript_replaces_what_is_shown(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = listed(cx, &dir);
    let id = follow_id(&mut h);
    h.send(reply(vec![answer("a1", "Before.")], Some(id)));

    h.send(reply(vec![answer("a9", "After.")], None));

    assert_eq!(lines(&mut h), ["● After."]);
}

#[gpui::test]
fn closing_the_last_pane_on_a_run_sends_unfollow(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = listed(cx, &dir);
    h.send(DaemonMessage::Tabs {
        tabs: vec![tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                follow_pane("f1", "r1"),
                follow_pane("f2", "r1"),
            ),
        )],
    });
    let sent = h.sent();
    assert_eq!(
        unfollows(&sent),
        0,
        "a second pane on the run follows it once"
    );

    h.send(DaemonMessage::Tabs {
        tabs: vec![tab("t1", &follow_pane("f1", "r1"))],
    });
    assert_eq!(unfollows(&h.sent()), 0, "a pane still shows the run");

    h.send(DaemonMessage::Tabs {
        tabs: vec![tab("t1", &pane("p1", Some("s1")))],
    });
    assert_eq!(unfollows(&h.sent()), 1, "no pane shows the run");
}

#[gpui::test]
fn a_reconnect_follows_again_and_the_reply_replaces_the_lines(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = listed(cx, &dir);
    let first = follow_id(&mut h);
    h.send(reply(vec![answer("a1", "One.")], Some(first.clone())));

    h.reconnect_with_features(&[protocol::FEATURE_DISPATCH_RUNS]);
    let second = follow_id(&mut h);
    assert_ne!(
        second, first,
        "the follow goes again with a fresh request id"
    );
    assert_eq!(
        lines(&mut h),
        ["● One."],
        "the held transcript draws until the reply"
    );

    h.send(reply(vec![answer("a1", "Stale.")], Some(first)));
    assert_eq!(lines(&mut h), ["● One."], "the old follow's reply is stale");
    h.send(reply(
        vec![answer("a1", "One."), answer("a2", "Two.")],
        Some(second),
    ));

    assert_eq!(lines(&mut h), ["● One.", "● Two."], "replaced, not merged");
}

/// The fixture holding `● One.` from a first follow, reconnected, with the
/// re-follow's request id.
fn held_then_reconnected<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> (Harness<'a>, String) {
    let mut h = listed(cx, dir);
    let first = follow_id(&mut h);
    h.send(reply(vec![answer("a1", "One.")], Some(first)));
    h.reconnect_with_features(&[protocol::FEATURE_DISPATCH_RUNS]);
    h.send(DaemonMessage::DispatchRuns {
        runs: vec![r1().build()],
    });
    let second = follow_id(&mut h);
    (h, second)
}

fn refusal(code: &str, request_id: String) -> DaemonMessage {
    DaemonMessage::Error {
        code: Some(code.to_owned()),
        message: "refused".to_owned(),
        request_id: Some(request_id),
    }
}

#[gpui::test]
fn an_unsupported_follow_refusal_drops_the_transcript_and_shows_recent_actions(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let (mut h, id) = held_then_reconnected(cx, &dir);
    assert_eq!(lines(&mut h), ["● One."]);

    h.send(refusal(protocol::error_codes::UNSUPPORTED_MESSAGE, id));

    assert_eq!(lines(&mut h), ["read a.rs", "edit b.rs"]);
}

#[gpui::test]
fn a_not_found_refusal_keeps_the_last_transcript(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = held_then_reconnected(cx, &dir);

    h.send(refusal(protocol::error_codes::NOT_FOUND, id));

    assert_eq!(lines(&mut h), ["● One."]);
}

/// [`fixture`] with session `s1`'s own appearance set by `appearance`.
fn with_appearance(appearance: impl FnOnce(&mut AppearanceOverrides)) -> Fixture {
    let mut s = session("s1").in_repo("r1").build();
    appearance(&mut s.appearance);
    Fixture {
        repos: vec![repo("r1", "C:/r1")],
        sessions: vec![s],
        tabs: vec![tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                pane("p1", Some("s1")),
                follow_pane("f2", "r1"),
            ),
        )],
        ..Fixture::default()
    }
}

/// `fixture` on a connection with dispatch runs, `r1` listed as `s1`'s run
/// and its one answer held.
fn answered<'a>(cx: &'a mut TestAppContext, dir: &TestDir, fixture: &Fixture) -> Harness<'a> {
    let mut h = Harness::with(cx, dir, fixture);
    h.reconnect_with_features(&[protocol::FEATURE_DISPATCH_RUNS]);
    h.send(DaemonMessage::DispatchRuns {
        runs: vec![r1().session_id("s1").build()],
    });
    let id = follow_id(&mut h);
    h.send(reply(vec![answer("a1", "Looking.")], Some(id)));
    h
}

#[gpui::test]
fn a_run_transcript_draws_at_the_terminal_font_size_and_line_height(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = with_appearance(|appearance| appearance.terminal_font_size = Some(18));
    let mut h = answered(cx, &dir, &fixture);

    let (_, _, size, line_height) = h
        .root(|root, cx| root.transcript_font_of("f2", cx))
        .expect("the pane follows a run");
    assert_eq!(size, 18.0, "the terminal's size, not the UI text size");
    assert_eq!(
        h.bounds("dispatch-line-f2-0").size.height,
        px(line_height),
        "the column draws the line height as well as the size"
    );
}

#[gpui::test]
fn a_run_transcript_draws_in_the_tab_terminals_face(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = with_appearance(|appearance| {
        appearance.terminal_font_family = Some("Cascadia Code".to_owned());
        appearance.terminal_font_weight = Some(600);
    });
    let mut h = answered(cx, &dir, &fixture);

    let (family, weight, _, _) = h
        .root(|root, cx| root.transcript_font_of("f2", cx))
        .expect("the pane follows a run");
    let terminal = h
        .root(|root, cx| root.pane_font("p1", cx))
        .expect("a terminal pane showing s1");

    assert_eq!(
        Some(family.as_str()),
        terminal.family.as_deref(),
        "the family the terminal beside it draws"
    );
    assert_eq!(
        FontWeight(f32::from(terminal.weight)),
        weight,
        "and its weight"
    );
}

#[gpui::test]
fn clicking_a_tool_line_expands_and_collapses_its_output(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = listed(cx, &dir);
    let id = follow_id(&mut h);
    h.send(reply(
        vec![tool("t1", "ls", Some("l1\nl2\nl3\nl4\nl5\nl6\nl7"))],
        Some(id),
    ));
    assert_eq!(lines(&mut h).len(), 7, "five lines and a count");

    h.click_on("dispatch-line-f2-0");
    assert_eq!(lines(&mut h).len(), 8, "the whole output");
    assert_eq!(lines(&mut h).last().map(String::as_str), Some("     l7"));

    h.click_on("dispatch-line-f2-3");
    assert_eq!(
        lines(&mut h).len(),
        7,
        "a click on an output line collapses it"
    );
}

// ── Selecting and copying ──────────────────────────────────────────────────

/// The answer the selection specs draw over three lines.
const THREE: &str = "alpha bravo\ncharlie delta\necho foxtrot";

/// The entries of [`selectable`]'s transcript.
fn selectable_entries() -> Vec<ChatEntry> {
    vec![answer("a1", THREE), tool("t1", "ls", Some("a.rs"))]
}

/// [`listed`] with run `r1`'s transcript held: [`THREE`] (lines 0 to 2,
/// units 0 to 2 of `a1`), then a tool call (lines 3 and 4, `t1`).
fn selectable<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = listed(cx, dir);
    let id = follow_id(&mut h);
    h.send(reply(selectable_entries(), Some(id)));
    h
}

/// [`listed`] with run `r1`'s transcript holding one tool call whose seven
/// output lines draw collapsed: the call, five lines and a count.
fn with_tool<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = listed(cx, dir);
    let id = follow_id(&mut h);
    h.send(reply(
        vec![tool("t1", "ls", Some("l1\nl2\nl3\nl4\nl5\nl6\nl7"))],
        Some(id),
    ));
    h
}

/// The first point, from the left edge of line `n` at its middle height,
/// that pane `f2` maps to a byte in `bytes` of unit `unit` of entry
/// `entry`, with that byte.
fn on_line(
    h: &mut Harness<'_>,
    n: usize,
    unit: (&str, u32),
    bytes: std::ops::Range<usize>,
) -> (Point<Pixels>, usize) {
    on_text(h, &format!("dispatch-line-f2-{n}"), unit, bytes)
}

/// [`on_line`] for the line tagged `line` in pane `f2`, a run's or a
/// subagent's.
fn on_text(
    h: &mut Harness<'_>,
    line: &str,
    (entry, unit): (&str, u32),
    bytes: std::ops::Range<usize>,
) -> (Point<Pixels>, usize) {
    h.cx.run_until_parked();
    let bounds = h.bounds(line);
    let y = bounds.top() + bounds.size.height / 2.0;
    let mut x = bounds.left();
    let mut found = None;
    while found.is_none() && x < bounds.right() {
        let at = point(x, y);
        let hit = h.root(|root, _| root.transcript_hit("f2", at));
        if let Some((hit_entry, hit_unit, byte)) = hit
            && hit_entry == entry
            && hit_unit == unit
            && bytes.contains(&byte)
        {
            found = Some((at, byte));
        }
        x += px(1.0);
    }
    found.expect("the line draws those bytes")
}

/// The text pane `f2`'s transcript selection copies.
fn selected(h: &mut Harness<'_>) -> String {
    h.cx.run_until_parked();
    h.root(|root, _| root.transcript_selected_text("f2"))
}

/// `count` clicks at `at` as the platform sends them: a left press and
/// release with click count 1, then 2, up to `count`.
fn clicks(h: &mut Harness<'_>, at: Point<Pixels>, count: usize) {
    for click_count in 1..=count {
        h.cx.simulate_event(MouseDownEvent {
            position: at,
            modifiers: Modifiers::none(),
            button: MouseButton::Left,
            click_count,
            first_mouse: false,
        });
        h.cx.simulate_event(MouseUpEvent {
            position: at,
            modifiers: Modifiers::none(),
            button: MouseButton::Left,
            click_count,
        });
        h.cx.run_until_parked();
    }
}

/// A drag from the middle of line 0 ("bravo") to the middle of line 2
/// ("foxtrot"), and the text it selects.
fn drag_across(h: &mut Harness<'_>) -> String {
    let (from, start) = on_line(h, 0, ("a1", 0), 10..12);
    let (to, end) = on_line(h, 2, ("a1", 2), 8..10);
    h.drag(from, to, [Modifiers::none(); 2]);
    let lines = lines(h);
    format!("{}\n{}\n{}", &lines[0][start..], lines[1], &lines[2][..end])
}

/// Ctrl+C over a cleared clipboard, and what it then holds.
fn ctrl_c(h: &mut Harness<'_>) -> Option<String> {
    h.set_clipboard("other");
    h.keys("ctrl-c");
    h.clipboard()
}

#[gpui::test]
fn a_drag_across_lines_then_ctrl_c_copies_exactly_those_characters(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = selectable(cx, &dir);
    let expected = drag_across(&mut h);
    assert_eq!(selected(&mut h), expected);

    assert_eq!(ctrl_c(&mut h), Some(expected));
}

#[gpui::test]
fn with_copy_on_select_on_the_mouse_up_copies(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = selectable(cx, &dir);
    h.set_clipboard("other");

    let expected = drag_across(&mut h);

    assert_eq!(h.clipboard(), Some(expected), "on by default, no Ctrl+C");
}

#[gpui::test]
fn one_click_on_an_answer_line_selects_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = selectable(cx, &dir);
    let (at, _) = on_line(&mut h, 1, ("a1", 1), 4..6);

    clicks(&mut h, at, 1);

    assert_eq!(selected(&mut h), "");
    assert_eq!(lines(&mut h).len(), 5, "nothing toggles");
}

#[gpui::test]
fn a_double_click_selects_one_word(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = selectable(cx, &dir);
    let (at, _) = on_line(&mut h, 1, ("a1", 1), 4..6);

    clicks(&mut h, at, 2);

    assert_eq!(selected(&mut h), "charlie");
}

#[gpui::test]
fn a_triple_click_selects_one_line(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = selectable(cx, &dir);
    let (at, _) = on_line(&mut h, 2, ("a1", 2), 4..6);

    clicks(&mut h, at, 3);

    assert_eq!(selected(&mut h), "  echo foxtrot");
}

#[gpui::test]
fn a_shift_click_extends_the_selection(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = selectable(cx, &dir);
    let (from, start) = on_line(&mut h, 0, ("a1", 0), 10..12);
    let (to, _) = on_line(&mut h, 1, ("a1", 1), 4..6);
    h.drag(from, to, [Modifiers::none(); 2]);
    let (further, end) = on_line(&mut h, 2, ("a1", 2), 8..10);

    h.click(further, Modifiers::shift());

    let lines = lines(&mut h);
    assert_eq!(
        selected(&mut h),
        format!("{}\n{}\n{}", &lines[0][start..], lines[1], &lines[2][..end])
    );
}

#[gpui::test]
fn a_plain_click_clears_the_selection(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = selectable(cx, &dir);
    drag_across(&mut h);
    let (at, _) = on_line(&mut h, 1, ("a1", 1), 4..6);

    clicks(&mut h, at, 1);

    assert_eq!(selected(&mut h), "");
}

#[gpui::test]
fn one_click_on_a_tool_line_toggles_it_and_selects_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = with_tool(cx, &dir);
    let (at, _) = on_line(&mut h, 0, ("t1", 0), 4..6);

    clicks(&mut h, at, 1);

    assert_eq!(lines(&mut h).len(), 8, "the whole output");
    assert_eq!(selected(&mut h), "");
}

#[gpui::test]
fn a_double_click_on_a_tool_line_does_not_toggle_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = with_tool(cx, &dir);
    let (at, _) = on_line(&mut h, 0, ("t1", 0), 4..6);

    clicks(&mut h, at, 2);

    assert_eq!(lines(&mut h).len(), 7, "still five lines and a count");
    assert_eq!(selected(&mut h), "Bash");
}

#[gpui::test]
fn a_triple_click_on_a_tool_line_does_not_toggle_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = with_tool(cx, &dir);
    let (at, _) = on_line(&mut h, 0, ("t1", 0), 4..6);

    clicks(&mut h, at, 3);

    assert_eq!(lines(&mut h).len(), 7, "still five lines and a count");
    assert_eq!(selected(&mut h), "● Bash(ls)");
}

#[gpui::test]
fn an_update_that_keeps_the_lines_keeps_the_selection(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = selectable(cx, &dir);
    let (at, _) = on_line(&mut h, 1, ("a1", 1), 4..6);
    clicks(&mut h, at, 2);

    h.send(DaemonMessage::TranscriptAppend {
        source: source(),
        entries: vec![answer("a2", "More.")],
        ended: false,
    });

    assert_eq!(lines(&mut h).len(), 6, "the new answer is drawn");
    assert_eq!(selected(&mut h), "charlie");
}

#[gpui::test]
fn a_hidden_note_drawn_above_keeps_the_selection_on_its_word(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = selectable(cx, &dir);
    let (at, _) = on_line(&mut h, 1, ("a1", 1), 4..6);
    clicks(&mut h, at, 2);

    h.send(DaemonMessage::Transcript {
        source: source(),
        entries: selectable_entries(),
        hidden: 1,
        request_id: None,
    });

    assert_eq!(lines(&mut h).len(), 6, "the note is drawn above");
    assert_eq!(ctrl_c(&mut h).as_deref(), Some("charlie"));
}

#[gpui::test]
fn dropping_the_oldest_answer_keeps_a_selection_on_a_later_one(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = listed(cx, &dir);
    let id = follow_id(&mut h);
    let mut entries = vec![answer("a0", "old\nfirst")];
    entries.extend((1..500).map(|n| answer(&format!("a{n}"), "zulu yankee")));
    h.send(reply(entries, Some(id)));
    let (at, _) = on_line(&mut h, 500, ("a499", 0), 10..12);
    clicks(&mut h, at, 2);
    assert_eq!(selected(&mut h), "yankee");

    h.send(DaemonMessage::TranscriptAppend {
        source: source(),
        entries: vec![answer("a500", "tail")],
        ended: false,
    });

    let lines = lines(&mut h);
    assert_eq!(
        lines.first().map(String::as_str),
        Some("… 1 earlier entries not shown"),
        "a0 went past the cap"
    );
    assert_eq!(ctrl_c(&mut h).as_deref(), Some("yankee"));
}

#[gpui::test]
fn a_drag_released_outside_the_log_copies_and_ends_the_press(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = selectable(cx, &dir);
    h.set_clipboard("other");
    let (from, start) = on_line(&mut h, 0, ("a1", 0), 10..12);
    let log = h.bounds("dispatch-log-f2");
    let line = h.bounds("dispatch-line-f2-1");
    let outside = point(log.left() - px(40.0), line.center().y);

    h.drag(from, outside, [Modifiers::none(); 2]);

    let copied = selected(&mut h);
    assert!(
        copied.starts_with(&lines(&mut h)[0][start..]),
        "the selection runs from the press: {copied:?}"
    );
    assert_eq!(h.clipboard(), Some(copied.clone()), "the release copied it");
    let elsewhere = point(
        log.left() - px(40.0),
        h.bounds("dispatch-line-f2-4").center().y,
    );
    h.cx.simulate_mouse_move(elsewhere, MouseButton::Left, Modifiers::none());
    assert_eq!(selected(&mut h), copied, "no press is left to move it");
}

// ── Opening links ──────────────────────────────────────────────────────────

const URL: &str = "https://example.com/docs";
const NOTES: &str = "docs/notes.md";
const BOTH_GESTURES: &str = "pane-link-tip-f2=Ctrl+click to open · Ctrl+Shift+click for options";

fn ctrl() -> Modifiers {
    support::secondary_key()
}

fn ctrl_shift() -> Modifiers {
    Modifiers {
        shift: true,
        ..support::secondary_key()
    }
}

/// Writes `rel` under `root`; returns the path a link to it resolves to.
fn write_file(root: &Path, rel: &str) -> PathBuf {
    let path = root.join(rel);
    let parent = path.parent().expect("a file has a parent folder");
    std::fs::create_dir_all(parent).expect("create the file's folder");
    std::fs::write(&path, "x\n").expect("write the file");
    let resolved = std::fs::canonicalize(&path).expect("canonicalize the file");
    let text = resolved.to_string_lossy();
    PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(&text))
}

/// Whether an element tagged `selector` was painted.
fn drawn(h: &mut Harness<'_>, selector: &'static str) -> bool {
    h.cx.run_until_parked();
    h.cx.debug_bounds(selector).is_some()
}

/// [`fixture`] on a connection with dispatch runs, `r1` listed working in
/// `worktree` with main checkout `main` and no dispatching session, and its
/// transcript holding `entries`.
fn run_in<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    (worktree, main): (&Path, &Path),
    entries: Vec<ChatEntry>,
) -> Harness<'a> {
    let mut h = Harness::with(cx, dir, &fixture());
    h.reconnect_with_features(&[protocol::FEATURE_DISPATCH_RUNS]);
    let run = r1()
        .worktree(&worktree.to_string_lossy())
        .main_checkout(&main.to_string_lossy());
    h.send(DaemonMessage::DispatchRuns {
        runs: vec![run.build()],
    });
    let id = follow_id(&mut h);
    h.send(reply(entries, Some(id)));
    h
}

/// [`run_in`] with `docs/notes.md` written in both the worktree and the
/// main checkout under `dir`; also returns the worktree's file.
fn run_with_notes<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    entries: Vec<ChatEntry>,
) -> (Harness<'a>, PathBuf) {
    let (worktree, main) = (dir.path().join("wt"), dir.path().join("main"));
    let file = write_file(&worktree, NOTES);
    write_file(&main, NOTES);
    (run_in(cx, dir, (&worktree, &main), entries), file)
}

/// The answer line `● wrote docs/notes.md`, whose path takes bytes 10 to 23.
fn wrote_notes() -> ChatEntry {
    answer("a1", "wrote docs/notes.md")
}

#[gpui::test]
fn ctrl_click_on_a_path_in_a_subagent_transcript_opens_it_as_the_terminal_does(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let file = write_file(dir.path(), NOTES);
    let s1 = session("s1")
        .shell(&dir.path().to_string_lossy())
        .subagents(vec![subagent("a1")])
        .build();
    let fixture = Fixture {
        sessions: vec![s1],
        tabs: vec![tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                pane("p1", Some("s1")),
                follow_subagent_pane("f2", "s1", "a1"),
            ),
        )],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.sent();
    h.reconnect_with_features(&[]);
    let of = TranscriptSource::Subagent {
        session_id: "s1".to_owned(),
        agent_id: "a1".to_owned(),
    };
    let id = h.sent().into_iter().find_map(|msg| match msg {
        ClientMessage::FollowTranscript { source, request_id } if source == of => request_id,
        _ => None,
    });
    h.send(DaemonMessage::Transcript {
        source: of,
        entries: vec![wrote_notes()],
        hidden: 0,
        request_id: id,
    });
    let (at, _) = on_text(&mut h, "subagent-line-f2-0", ("a1", 0), 12..14);
    h.click(at, ctrl());

    // A terminal's Ctrl+click on this path in `s1` opens the same
    // (`ui_links::ctrl_click_on_a_text_path_opens_the_default_app`).
    assert_eq!(
        h.opened(),
        [Opened::DefaultApp(file)],
        "the path resolved against s1's folder, in its default app"
    );
}

#[gpui::test]
fn ctrl_shift_click_on_a_path_in_a_transcript_opens_the_link_menu(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = run_with_notes(cx, &dir, vec![wrote_notes()]);
    let (at, _) = on_line(&mut h, 0, ("a1", 0), 12..14);

    h.click(at, ctrl_shift());

    assert!(
        h.root(|root, _| root.link_menu_open()),
        "the link menu opens"
    );
    assert_eq!(h.opened(), [], "and nothing opens yet");
    assert_eq!(selected(&mut h), "", "nor is anything selected");
}

#[gpui::test]
fn ctrl_click_on_a_url_in_a_transcript_opens_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let entries = vec![answer("a1", &format!("see {URL} here"))];
    let (mut h, _) = run_with_notes(cx, &dir, entries);
    let (at, _) = on_line(&mut h, 0, ("a1", 0), 12..20);

    h.click(at, ctrl());

    assert!(
        !h.root(|root, _| root.link_menu_open()),
        "a URL opens no menu"
    );
    assert_eq!(h.opened(), [Opened::Url(URL.to_owned())]);
}

#[gpui::test]
fn a_runs_relative_path_resolves_against_its_worktree(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, file) = run_with_notes(cx, &dir, vec![wrote_notes()]);
    let (at, _) = on_line(&mut h, 0, ("a1", 0), 12..14);

    h.click(at, ctrl());

    assert_eq!(
        h.opened(),
        [Opened::DefaultApp(file)],
        "the worktree's file, not the main checkout's"
    );
}

#[gpui::test]
fn a_ctrl_drag_off_any_link_selects_as_a_plain_drag_does(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = selectable(cx, &dir);
    let (from, start) = on_line(&mut h, 0, ("a1", 0), 10..12);
    let (to, end) = on_line(&mut h, 2, ("a1", 2), 8..10);

    h.drag(from, to, [ctrl(); 2]);

    let lines = lines(&mut h);
    assert_eq!(
        selected(&mut h),
        format!("{}\n{}\n{}", &lines[0][start..], lines[1], &lines[2][..end])
    );
    assert_eq!(h.opened(), [], "nothing opens");
}

#[gpui::test]
fn ctrl_click_on_a_link_in_a_tool_line_opens_it_and_neither_selects_nor_toggles(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let output = Some("l1\nl2\nl3\nl4\nl5\nl6\nl7");
    let entries = vec![tool("t1", "cat docs/notes.md", output)];
    let (mut h, file) = run_with_notes(cx, &dir, entries);
    assert_eq!(lines(&mut h)[0], "● Bash(cat docs/notes.md)");
    let (at, _) = on_line(&mut h, 0, ("t1", 0), 15..18);

    h.click(at, ctrl());

    assert_eq!(h.opened(), [Opened::DefaultApp(file)]);
    assert_eq!(lines(&mut h).len(), 7, "still five lines and a count");
    assert_eq!(selected(&mut h), "");
}

#[gpui::test]
fn ctrl_hover_over_a_link_in_a_transcript_shows_the_tip(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = run_with_notes(cx, &dir, vec![wrote_notes()]);
    let (at, _) = on_line(&mut h, 0, ("a1", 0), 12..14);

    h.hover(at, Modifiers::none());
    assert!(!drawn(&mut h, BOTH_GESTURES), "no tip without Ctrl");

    h.hover(at, ctrl());
    assert!(
        drawn(&mut h, BOTH_GESTURES),
        "the terminal's tip for a path"
    );
}

/// The paths of the `OpenViewerTab`s sent since the last look.
fn viewer_opens(h: &mut Harness<'_>) -> Vec<PathBuf> {
    h.sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::OpenViewerTab { path, .. } => Some(PathBuf::from(path)),
            _ => None,
        })
        .collect()
}

#[gpui::test]
fn down_then_enter_in_a_transcripts_link_menu_picks_the_viewer(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, file) = run_with_notes(cx, &dir, vec![wrote_notes()]);
    let (at, _) = on_line(&mut h, 0, ("a1", 0), 12..14);
    h.click(at, ctrl_shift());
    h.sent();

    h.keys("down");
    h.keys("enter");

    assert_eq!(viewer_opens(&mut h), [file], "the menu kept the keyboard");
    assert_eq!(h.opened(), [], "no default app");
}

#[gpui::test]
fn releasing_ctrl_removes_a_transcript_links_tip(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = run_with_notes(cx, &dir, vec![wrote_notes()]);
    // Modifier changes reach the window through its focus, so a click off
    // the link focuses the pane first, as `ui_links::attached` does.
    let (off_link, _) = on_line(&mut h, 0, ("a1", 0), 4..8);
    clicks(&mut h, off_link, 1);
    let (at, _) = on_line(&mut h, 0, ("a1", 0), 12..14);
    h.hover(at, ctrl());
    assert!(drawn(&mut h, BOTH_GESTURES), "Ctrl+hover shows the tip");

    h.set_modifiers(Modifiers::none());
    assert!(
        !drawn(&mut h, BOTH_GESTURES),
        "releasing Ctrl hides it with no pointer move"
    );

    h.set_modifiers(ctrl());
    assert!(
        drawn(&mut h, BOTH_GESTURES),
        "pressing Ctrl again over a still mouse brings it back"
    );
}

#[gpui::test]
fn collapsing_a_tool_call_under_a_ctrl_hovered_link_draws(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let long = format!("docs/{}.md", "a".repeat(24));
    let output = format!("l1\nl2\nl3\nl4\nl5\n{long}\nl7");
    let (mut h, _) = run_with_notes(cx, &dir, vec![tool("t1", "ls", Some(&output))]);
    h.click_on("dispatch-line-f2-0");
    assert_eq!(lines(&mut h)[6], format!("     {long}"), "expanded");
    let (on_link, _) = on_line(&mut h, 6, ("t1", 6), 10..14);
    h.hover(on_link, ctrl());
    assert!(drawn(&mut h, BOTH_GESTURES), "the long path is hovered");

    // Ctrl stays down and the pointer does not move, so the hover still
    // names line 6, which collapsing makes the shorter count line.
    let (head, _) = on_line(&mut h, 0, ("t1", 0), 0..3);
    h.click(head, ctrl());

    assert_eq!(lines(&mut h).len(), 7, "collapsed: five lines and a count");
    assert_eq!(lines(&mut h)[6], "     … +2 lines");
    assert!(drawn(&mut h, "dispatch-line-f2-6"), "the count line draws");
}

#[gpui::test]
fn ctrl_click_on_a_links_last_character_opens_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, file) = run_with_notes(cx, &dir, vec![wrote_notes()]);
    // `●` takes three bytes, so the path's last character is byte 22 but
    // character 20.
    let (at, byte) = on_line(&mut h, 0, ("a1", 0), 22..23);
    assert_eq!(byte, 22);

    h.click(at, ctrl());

    assert_eq!(h.opened(), [Opened::DefaultApp(file)]);
}

/// Where run `r1` works on [`REMOTE`] in the remote-run specs.
const RUN_WORKTREE: &str = "/home/dev/wt/run";
/// Its main checkout there.
const RUN_MAIN: &str = "/home/dev/main";

/// The features of a host that sends folders, lists a session's folders
/// and serves any absolute path.
const SERVING: [&str; 4] = [
    protocol::FEATURE_DISPATCH_RUNS,
    protocol::FEATURE_FOLDER_FETCH,
    protocol::FEATURE_SESSION_FOLDERS,
    protocol::FEATURE_HOST_PATHS,
];
/// The features of an older host, which follows runs but serves no host
/// path and sends no folder.
const OLDER: [&str; 1] = [protocol::FEATURE_DISPATCH_RUNS];

/// [`REMOTE`] as the remote-run specs dial it.
fn remote_target() -> Target {
    Target::tailnet(REMOTE, None)
}

/// Pane `f2` following `follow` on [`REMOTE`] beside This PC's `s1`, that
/// host's daemon listing `features` beside the fake daemon's and the repo
/// `rp` at `/home/dev/repo`. What the client sent on `Welcome`, the follow
/// among it, is left to read.
fn remote_pane_following<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    follow: PaneFollow,
    features: &[&str],
) -> Harness<'a> {
    let pane_node = GridNode::Pane {
        pane_id: "f2".to_owned(),
        session_id: None,
        host: Some(REMOTE.to_owned()),
        follow: Some(follow),
    };
    let fixture = Fixture {
        repos: vec![repo("r1", "C:/r1")],
        sessions: vec![session("s1").in_repo("r1").build()],
        tabs: vec![tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                pane("p1", Some("s1")),
                pane_node,
            ),
        )],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, dir, &fixture);
    let mut listed = support::daemon_features();
    listed.extend(features.iter().map(|feature| (*feature).to_owned()));
    h.host(&remote_target()).connect_keeping_sent_with(listed);
    h.host(&remote_target()).send(DaemonMessage::Repos {
        repos: vec![repo("rp", "/home/dev/repo")],
    });
    h
}

/// Answers [`REMOTE`]'s last follow of `of` with `entries`, and drains
/// what This PC was sent.
fn answer_remote_follow(h: &mut Harness<'_>, of: &TranscriptSource, entries: Vec<ChatEntry>) {
    let id = h
        .host(&remote_target())
        .sent()
        .into_iter()
        .rev()
        .find_map(|msg| match msg {
            ClientMessage::FollowTranscript { source, request_id } if source == *of => request_id,
            _ => None,
        })
        .expect("the pane follows on its host");
    h.host(&remote_target()).send(DaemonMessage::Transcript {
        source: of.clone(),
        entries,
        hidden: 0,
        request_id: Some(id),
    });
    h.sent();
}

/// Pane `f2` following `run` (as run `r1`) on [`REMOTE`] with `features`,
/// that host listing `sessions`, and its transcript holding
/// `● wrote docs/notes.md`.
fn remote_run_on<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    (run, sessions): (DispatchRunBuilder, Vec<SessionSnapshot>),
    features: &[&str],
) -> Harness<'a> {
    let follow = PaneFollow::DispatchRun {
        run_id: "r1".to_owned(),
    };
    let mut h = remote_pane_following(cx, dir, follow, features);
    h.host(&remote_target())
        .send(DaemonMessage::Sessions { sessions });
    h.host(&remote_target()).send(DaemonMessage::DispatchRuns {
        runs: vec![run.build()],
    });
    answer_remote_follow(&mut h, &source(), vec![wrote_notes()]);
    h
}

/// [`remote_run_on`] a host serving host paths, `run` working in
/// [`RUN_WORKTREE`] with main checkout [`RUN_MAIN`].
fn remote_run<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    run: DispatchRunBuilder,
    sessions: Vec<SessionSnapshot>,
) -> Harness<'a> {
    let run = run.worktree(RUN_WORKTREE).main_checkout(RUN_MAIN);
    remote_run_on(cx, dir, (run, sessions), &SERVING)
}

/// Ctrl+clicks `docs/notes.md` in [`remote_run`]'s transcript; returns the
/// host paths sent to [`REMOTE`] to stat, and whether it was asked for a
/// session's folders.
fn ctrl_click_remote_notes(h: &mut Harness<'_>) -> (Vec<String>, bool) {
    let (at, _) = on_line(h, 0, ("a1", 0), 12..14);
    h.click(at, ctrl());
    let sent = h.host(&Target::tailnet(REMOTE, None)).sent();
    let stats = sent
        .iter()
        .filter_map(|msg| match msg {
            ClientMessage::StatHostPath { path, .. } => Some(path.clone()),
            _ => None,
        })
        .collect();
    let asked = sent
        .iter()
        .any(|msg| matches!(msg, ClientMessage::ListSessionFolders { .. }));
    (stats, asked)
}

#[gpui::test]
fn a_remote_runs_relative_path_with_no_session_is_fetched_from_its_worktree(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let mut h = remote_run(cx, &dir, r1(), Vec::new());

    let (stats, asked) = ctrl_click_remote_notes(&mut h);

    assert_eq!(stats, ["/home/dev/wt/run/docs/notes.md"]);
    assert!(!asked, "a run asks for no session's folders");
}

#[gpui::test]
fn a_remote_runs_relative_path_is_fetched_from_its_worktree_not_its_sessions_folder(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let s9 = session("s9").shell("/home/dev/other").build();
    let mut h = remote_run(cx, &dir, r1().session_id("s9"), vec![s9]);

    let (stats, asked) = ctrl_click_remote_notes(&mut h);

    assert_eq!(stats, ["/home/dev/wt/run/docs/notes.md"]);
    assert!(!asked, "a run asks for no session's folders");
}

/// The `FetchFile`s sent to [`REMOTE`] since the last look, as `(repo,
/// worktree, path)`.
fn remote_file_fetches(h: &mut Harness<'_>) -> Vec<(String, Option<String>, String)> {
    h.host(&remote_target())
        .sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::FetchFile {
                repo_id,
                worktree_path,
                path,
                ..
            } => Some((repo_id, worktree_path, path)),
            _ => None,
        })
        .collect()
}

/// [`remote_run_on`] an older host, run `r1` working in `worktree` with
/// main checkout `/home/dev/repo`, the repo `rp` that host registers;
/// Ctrl+clicks `docs/notes.md` and returns the files fetched.
fn older_host_fetches(
    cx: &mut TestAppContext,
    worktree: &str,
) -> Vec<(String, Option<String>, String)> {
    let dir = TestDir::new();
    let run = r1().worktree(worktree).main_checkout("/home/dev/repo");
    let mut h = remote_run_on(cx, &dir, (run, Vec::new()), &OLDER);
    let (at, _) = on_line(&mut h, 0, ("a1", 0), 12..14);
    h.click(at, ctrl());
    remote_file_fetches(&mut h)
}

#[gpui::test]
fn on_an_older_host_a_runs_path_is_fetched_from_its_worktree_beside_the_repo(
    cx: &mut TestAppContext,
) {
    let fetches = older_host_fetches(cx, "/home/dev/repo.wt/feat");

    assert_eq!(
        fetches,
        [(
            "rp".to_owned(),
            Some("/home/dev/repo.wt/feat".to_owned()),
            "docs/notes.md".to_owned()
        )],
        "the worktree of the repo registered at the main checkout"
    );
}

#[gpui::test]
fn on_an_older_host_a_run_with_no_worktree_fetches_from_its_main_checkout(cx: &mut TestAppContext) {
    let fetches = older_host_fetches(cx, "");

    assert_eq!(
        fetches,
        [(
            "rp".to_owned(),
            Some("/home/dev/repo".to_owned()),
            "docs/notes.md".to_owned()
        )]
    );
}

#[gpui::test]
fn ctrl_click_on_a_transcript_link_drops_an_armed_stop(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = run_with_notes(cx, &dir, vec![wrote_notes()]);
    h.click_on("dispatch-stop-f2");
    assert!(
        h.root(|root, _| root.dispatch_stop_armed("f2")),
        "Stop is armed"
    );
    let (at, _) = on_line(&mut h, 0, ("a1", 0), 12..14);

    h.click(at, ctrl());

    assert!(
        !h.root(|root, _| root.dispatch_stop_armed("f2")),
        "the press dropped the armed Stop"
    );
}

#[gpui::test]
fn a_gone_runs_subagent_still_fetches_an_absolute_path(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let follow = PaneFollow::RunSubagent {
        run_id: "r1".to_owned(),
        agent_id: "a9".to_owned(),
    };
    let mut h = remote_pane_following(cx, &dir, follow, &SERVING);
    let of = TranscriptSource::RunSubagent {
        run_id: "r1".to_owned(),
        agent_id: "a9".to_owned(),
    };
    let entries = vec![answer("a1", "see /home/dev/build.log")];
    answer_remote_follow(&mut h, &of, entries);
    let (at, _) = on_text(&mut h, "subagent-line-f2-0", ("a1", 0), 12..16);

    h.click(at, ctrl());

    let sent = h.host(&remote_target()).sent();
    let stats: Vec<&str> = sent
        .iter()
        .filter_map(|msg| match msg {
            ClientMessage::StatHostPath { path, .. } => Some(path.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(stats, ["/home/dev/build.log"], "fetched by its host path");
}

/// The tip pane `f3` draws over a path.
const F3_TIP: &str = "pane-link-tip-f3=Ctrl+click to open · Ctrl+Shift+click for options";

#[gpui::test]
fn ctrl_down_underlines_no_link_in_a_background_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        repos: vec![repo("r1", "C:/r1")],
        tabs: vec![
            tab("t1", &follow_pane("f2", "r1")),
            tab("t2", &follow_pane("f3", "r1")),
        ],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.reconnect_with_features(&[protocol::FEATURE_DISPATCH_RUNS]);
    h.send(DaemonMessage::DispatchRuns {
        runs: vec![r1().build()],
    });
    let id = follows(&h.sent()).into_iter().flatten().last();
    h.send(reply(vec![wrote_notes()], id));
    let (on_link, _) = on_line(&mut h, 0, ("a1", 0), 12..14);
    let (off_link, _) = on_line(&mut h, 0, ("a1", 0), 4..8);
    // `f3` draws once, at the same place as `f2`, then its tab goes behind.
    h.click_on("tab-t2");
    assert!(
        drawn(&mut h, "dispatch-line-f3-0"),
        "f3 drew its transcript"
    );
    h.click_on("tab-t1");
    clicks(&mut h, off_link, 1);
    h.hover(on_link, Modifiers::none());

    h.set_modifiers(ctrl());

    assert!(drawn(&mut h, BOTH_GESTURES), "the shown pane underlines");
    assert!(
        !h.root(|root, _| root.transcript_link_hovered("f3")),
        "the background tab's pane does not"
    );
    assert!(!drawn(&mut h, F3_TIP));
}
