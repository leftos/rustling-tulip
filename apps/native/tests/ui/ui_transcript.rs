//! A pane that follows a dispatch run draws the run's transcript the way a
//! terminal shows Claude Code: it follows the run's transcript, draws the
//! reply in place of the recent actions, folds appends by id, ignores a stale
//! reply, unfollows once no pane shows the run, and follows again after a
//! reconnect.

use crate::support;

use gpui::TestAppContext;
use protocol::{
    ChatEntry, ClientMessage, DaemonMessage, SplitDirection, ToolKind, ToolOutcome,
    TranscriptSource,
};
use support::{
    DispatchRunBuilder, Fixture, Harness, TestDir, dispatch_run, follow_pane, pane, repo, session,
    split, tab,
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
