//! A pane that follows a subagent draws the subagent's transcript read-only:
//! it follows the transcript on the pane's host, draws a header row of the
//! subagent's last snapshot over the transcript, keeps both once the
//! subagent is no longer listed, notes an ended session or a subagent no
//! longer available, unfollows once no pane shows it, and follows again
//! after a reconnect.

use crate::support;

use gpui::{TestAppContext, px};
use protocol::{
    ChatEntry, ClientMessage, DaemonMessage, GridNode, PaneFollow, SplitDirection, ToolKind,
    ToolOutcome, TranscriptSource,
};
use rustling_tulip_native::target::Target;
use serde_json::Value;
use support::{
    Fixture, Harness, REMOTE, TestDir, dispatch_run, follow_pane, follow_subagent_pane, pane, repo,
    session, split, subagent_with, tab,
};

/// Subagent `a1` of `s1`: `task a1`, an `explore` agent on `model-x`.
fn a1(state: &str) -> Value {
    subagent_with("a1", "explore", "model-x", state)
}

/// Repo `r1`; session `s1` listing `subagents`; tab `t1`: `s1` in `p1`
/// and a pane `f2` following subagent `a1` of `s1` on This PC.
fn fixture_with(subagents: Vec<Value>) -> Fixture {
    Fixture {
        repos: vec![repo("r1", "C:/r1")],
        sessions: vec![session("s1").in_repo("r1").subagents(subagents).build()],
        tabs: vec![tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                pane("p1", Some("s1")),
                follow_subagent_pane("f2", "s1", "a1"),
            ),
        )],
        ..Fixture::default()
    }
}

fn fixture() -> Fixture {
    fixture_with(vec![a1("running")])
}

/// `fixture` on a fresh connection, nothing sent left unread but the
/// follow.
fn connected<'a>(cx: &'a mut TestAppContext, dir: &TestDir, fixture: &Fixture) -> Harness<'a> {
    let mut h = Harness::with(cx, dir, fixture);
    h.sent();
    h.reconnect_with_features(&[]);
    h
}

fn source_of(session_id: &str, agent_id: &str) -> TranscriptSource {
    TranscriptSource::Subagent {
        session_id: session_id.to_owned(),
        agent_id: agent_id.to_owned(),
    }
}

fn source() -> TranscriptSource {
    source_of("s1", "a1")
}

/// The request id of each `FollowTranscript` of `of` in `sent`.
fn follows_of(sent: &[ClientMessage], of: &TranscriptSource) -> Vec<Option<String>> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::FollowTranscript {
                source: s,
                request_id,
            } if s == of => Some(request_id.clone()),
            _ => None,
        })
        .collect()
}

/// How many `UnfollowTranscript` of `of` `sent` holds.
fn unfollows_of(sent: &[ClientMessage], of: &TranscriptSource) -> usize {
    sent.iter()
        .filter(|msg| matches!(msg, ClientMessage::UnfollowTranscript { source: s } if s == of))
        .count()
}

/// The request id of the one follow of `a1` sent since the last read.
fn follow_id(h: &mut Harness<'_>) -> String {
    let sent = follows_of(&h.sent(), &source());
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

fn append(source: TranscriptSource, entries: Vec<ChatEntry>, ended: bool) -> DaemonMessage {
    DaemonMessage::TranscriptAppend {
        source,
        entries,
        ended,
    }
}

fn refusal(code: &str, request_id: String) -> DaemonMessage {
    DaemonMessage::Error {
        code: Some(code.to_owned()),
        message: "refused".to_owned(),
        request_id: Some(request_id),
    }
}

/// `s1` updated to list `subagents`.
fn s1_lists(subagents: Vec<Value>) -> DaemonMessage {
    DaemonMessage::SessionUpdated {
        session: session("s1").in_repo("r1").subagents(subagents).build(),
        request_id: None,
    }
}

fn lines(h: &mut Harness<'_>) -> Vec<String> {
    h.root(|root, _| root.subagent_view_lines("f2"))
}

fn header(h: &mut Harness<'_>) -> Option<(String, String, String)> {
    h.root(|root, _| root.subagent_view_header("f2"))
}

fn note(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.subagent_view_note("f2"))
}

/// Whether the element tagged `selector` is laid out on screen.
fn drawn(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).origin.x >= px(0.0)
}

/// The header `a1` draws in `state`.
fn a1_header(state: &str) -> (String, String, String) {
    (
        "task a1".to_owned(),
        "SA · explore · model-x".to_owned(),
        state.to_owned(),
    )
}

#[gpui::test]
fn a_subagent_pane_follows_its_transcript_on_the_panes_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = fixture();
    let remote_pane = GridNode::Pane {
        pane_id: "f3".to_owned(),
        session_id: None,
        host: Some(REMOTE.to_owned()),
        follow: Some(PaneFollow::Subagent {
            session_id: "s9".to_owned(),
            agent_id: "a9".to_owned(),
        }),
    };
    fixture.tabs = vec![tab(
        "t1",
        &split(
            SplitDirection::Horizontal,
            pane("p1", Some("s1")),
            split(
                SplitDirection::Vertical,
                follow_subagent_pane("f2", "s1", "a1"),
                remote_pane,
            ),
        ),
    )];
    let mut h = Harness::with(cx, &dir, &fixture);
    h.sent();
    h.reconnect_with_features(&[]);
    let target = Target::tailnet(REMOTE, None);
    h.host(&target).connect_keeping_sent_with(Vec::new());

    let local = h.sent();
    let remote = h.host(&target).sent();

    let local_follows = follows_of(&local, &source());
    assert!(
        matches!(local_follows.as_slice(), [Some(_)]),
        "This PC follows a1 once, with a request id: {local_follows:?}"
    );
    assert!(
        follows_of(&local, &source_of("s9", "a9")).is_empty(),
        "This PC is not asked for the remote pane's subagent"
    );
    let remote_follows = follows_of(&remote, &source_of("s9", "a9"));
    assert!(
        matches!(remote_follows.as_slice(), [Some(_)]),
        "the remote pane's host follows a9: {remote_follows:?}"
    );
    assert!(
        follows_of(&remote, &source()).is_empty(),
        "the remote host is not asked for This PC's subagent"
    );
}

#[gpui::test]
fn the_reply_draws_the_answer_and_tool_card(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = connected(cx, &dir, &fixture());
    let id = follow_id(&mut h);

    h.send(reply(
        vec![answer("x1", "Looking."), tool("t1", "ls", Some("a.rs"))],
        Some(id),
    ));

    assert_eq!(lines(&mut h), ["● Looking.", "● Bash(ls)", "  ⎿  a.rs"]);
    assert!(drawn(&mut h, "subagent-log-f2"), "the log is drawn");
    assert!(
        drawn(&mut h, "subagent-line-f2-1"),
        "the tool card is drawn"
    );
    assert!(drawn(&mut h, "subagent-header-f2"), "the header is drawn");
}

#[gpui::test]
fn an_append_adds_a_row(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = connected(cx, &dir, &fixture());
    let id = follow_id(&mut h);
    h.send(reply(vec![tool("t1", "ls", None)], Some(id)));
    assert_eq!(lines(&mut h), ["● Bash(ls)", "  ⎿  Running…"]);

    h.send(append(
        source(),
        vec![tool("t1", "ls", Some("a.rs")), answer("x2", "Done.")],
        false,
    ));

    assert_eq!(lines(&mut h), ["● Bash(ls)", "  ⎿  a.rs", "● Done."]);
    assert!(drawn(&mut h, "subagent-line-f2-2"), "the new row is drawn");
}

#[gpui::test]
fn the_header_follows_the_snapshots_state(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = connected(cx, &dir, &fixture());
    assert_eq!(header(&mut h), Some(a1_header("running")));

    h.send(s1_lists(vec![a1("finished")]));
    assert_eq!(header(&mut h), Some(a1_header("finished")));

    let mut failed = a1("failed");
    failed["end_status"] = serde_json::json!("stopped by the user");
    h.send(s1_lists(vec![failed]));
    assert_eq!(
        header(&mut h),
        Some(a1_header("failed · stopped by the user"))
    );
    assert!(drawn(&mut h, "subagent-header-f2"), "the header is drawn");
}

#[gpui::test]
fn a_dropped_subagent_keeps_its_transcript_and_last_header(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = connected(cx, &dir, &fixture());
    let id = follow_id(&mut h);
    h.send(reply(vec![answer("x1", "Found it.")], Some(id)));
    h.send(s1_lists(vec![a1("finished")]));

    h.send(s1_lists(Vec::new()));
    assert_eq!(
        header(&mut h),
        Some(a1_header("finished")),
        "the last header stays"
    );
    assert_eq!(lines(&mut h), ["● Found it."], "the transcript stays");

    h.send(DaemonMessage::SessionRemoved {
        session_id: "s1".to_owned(),
    });
    assert_eq!(
        header(&mut h),
        Some(a1_header("finished")),
        "a removed session keeps the header"
    );
    assert_eq!(lines(&mut h), ["● Found it."]);
    assert_eq!(note(&mut h), None, "no note without an ended append");
}

#[gpui::test]
fn an_ended_session_shows_the_note(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = connected(cx, &dir, &fixture());
    let id = follow_id(&mut h);
    h.send(reply(vec![answer("x1", "Working.")], Some(id)));
    assert_eq!(note(&mut h), None, "precondition: no note");
    h.send(append(source(), vec![answer("x2", "More.")], false));
    assert_eq!(note(&mut h), None, "an append that does not end adds none");

    h.send(append(source(), Vec::new(), true));

    assert_eq!(note(&mut h).as_deref(), Some("Its session has ended"));
    assert!(drawn(&mut h, "subagent-note-f2"), "the note is drawn");
    assert_eq!(
        lines(&mut h),
        ["● Working.", "● More."],
        "the transcript stays"
    );
}

#[gpui::test]
fn closing_the_pane_unfollows(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = connected(cx, &dir, &fixture());
    follow_id(&mut h);

    h.send(DaemonMessage::Tabs {
        tabs: vec![tab("t1", &pane("p1", Some("s1")))],
    });

    let sent = h.sent();
    assert_eq!(unfollows_of(&sent, &source()), 1, "no pane shows a1");
    assert!(
        follows_of(&sent, &source()).is_empty(),
        "nothing re-follows"
    );
}

#[gpui::test]
fn a_welcome_refollows_and_ignores_the_stale_reply(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = connected(cx, &dir, &fixture());
    let first = follow_id(&mut h);
    h.send(reply(vec![answer("x1", "One.")], Some(first.clone())));

    h.reconnect_with_features(&[]);
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

    h.send(reply(vec![answer("x1", "Stale.")], Some(first)));
    assert_eq!(lines(&mut h), ["● One."], "the old follow's reply is stale");
    h.send(reply(
        vec![answer("x1", "One."), answer("x2", "Two.")],
        Some(second),
    ));

    assert_eq!(lines(&mut h), ["● One.", "● Two."], "replaced, not merged");
}

#[gpui::test]
fn a_refusal_with_nothing_kept_reads_no_longer_available(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = connected(cx, &dir, &fixture_with(Vec::new()));
    let id = follow_id(&mut h);
    assert_eq!(header(&mut h), None, "precondition: no snapshot of a1");
    assert_eq!(note(&mut h), None, "no note before the refusal");

    h.send(refusal(protocol::error_codes::NOT_FOUND, id));

    assert_eq!(
        note(&mut h).as_deref(),
        Some("This subagent is no longer available")
    );
    assert!(drawn(&mut h, "subagent-note-f2"), "the note is drawn");
    assert_eq!(lines(&mut h), Vec::<String>::new());

    h.send(reply(vec![answer("x1", "Back.")], None));
    assert_eq!(note(&mut h), None, "a later reply clears the note");
    assert_eq!(lines(&mut h), ["● Back."]);
}

#[gpui::test]
fn a_refusal_keeps_a_held_transcript(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = connected(cx, &dir, &fixture());
    let first = follow_id(&mut h);

    h.send(refusal(protocol::error_codes::UNSUPPORTED_MESSAGE, first));
    assert_eq!(
        header(&mut h),
        Some(a1_header("running")),
        "a snapshot alone keeps the header"
    );
    assert!(lines(&mut h).is_empty(), "with an empty body");
    assert_eq!(note(&mut h), None, "and no note");

    h.reconnect_with_features(&[]);
    let second = follow_id(&mut h);
    h.send(reply(vec![answer("x1", "Kept.")], Some(second)));
    h.reconnect_with_features(&[]);
    let third = follow_id(&mut h);

    h.send(refusal(protocol::error_codes::NOT_FOUND, third));

    assert_eq!(lines(&mut h), ["● Kept."], "the held transcript stays");
    assert_eq!(note(&mut h), None);
}

#[gpui::test]
fn a_non_not_found_refusal_keeps_a_held_transcript(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = connected(cx, &dir, &fixture());
    let first = follow_id(&mut h);
    h.send(reply(vec![answer("x1", "Kept.")], Some(first)));
    h.reconnect_with_features(&[]);
    let second = follow_id(&mut h);

    h.send(refusal(protocol::error_codes::UNSUPPORTED_MESSAGE, second));

    assert_eq!(lines(&mut h), ["● Kept."], "the held transcript stays");
    assert_eq!(note(&mut h), None);
}

#[gpui::test]
fn closing_the_last_pane_drops_the_kept_snapshot(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = connected(cx, &dir, &fixture());
    h.send(s1_lists(vec![a1("finished")]));
    h.send(s1_lists(Vec::new()));
    assert_eq!(
        header(&mut h),
        Some(a1_header("finished")),
        "precondition: the kept header draws"
    );

    h.send(DaemonMessage::Tabs {
        tabs: vec![tab("t1", &pane("p1", Some("s1")))],
    });
    h.send(DaemonMessage::Tabs {
        tabs: vec![tab("t1", &follow_subagent_pane("f2", "s1", "a1"))],
    });

    assert_eq!(header(&mut h), None, "no stale header once reopened");
}

#[gpui::test]
fn rebinding_the_pane_to_another_subagent_moves_the_follow(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = connected(
        cx,
        &dir,
        &fixture_with(vec![a1("running"), subagent_with("a2", "", "", "queued")]),
    );
    let id = follow_id(&mut h);
    h.send(reply(vec![answer("x1", "One.")], Some(id)));

    h.send(DaemonMessage::Tabs {
        tabs: vec![tab("t1", &follow_subagent_pane("f2", "s1", "a2"))],
    });

    let sent = h.sent();
    assert_eq!(unfollows_of(&sent, &source()), 1, "a1 is unfollowed");
    let a2 = follows_of(&sent, &source_of("s1", "a2"));
    assert!(matches!(a2.as_slice(), [Some(_)]), "a2 is followed: {a2:?}");
    assert!(
        lines(&mut h).is_empty(),
        "a1's transcript is not shown for a2"
    );
    assert_eq!(
        header(&mut h),
        Some(("task a2".to_owned(), "SA".to_owned(), "queued".to_owned()))
    );
}

#[gpui::test]
fn a_resumed_host_moves_the_follow_without_the_ended_note(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = connected(cx, &dir, &fixture());
    let id = follow_id(&mut h);
    h.send(reply(vec![answer("x1", "One.")], Some(id)));
    h.send(s1_lists(vec![a1("finished")]));

    h.send(DaemonMessage::Tabs {
        tabs: vec![tab("t1", &follow_subagent_pane("f2", "s2", "a1"))],
    });
    let sent = h.sent();
    assert_eq!(
        unfollows_of(&sent, &source()),
        1,
        "the old session's a1 is unfollowed"
    );
    let moved = follows_of(&sent, &source_of("s2", "a1"));
    assert!(
        matches!(moved.as_slice(), [Some(_)]),
        "the new session's a1 is followed: {moved:?}"
    );
    assert_eq!(
        header(&mut h),
        Some(a1_header("finished")),
        "the moved pane keeps a1's last header"
    );
    let moved_id = moved.into_iter().flatten().next().unwrap_or_default();
    h.send(DaemonMessage::Error {
        code: Some(protocol::error_codes::NOT_FOUND.to_owned()),
        message: "refused".to_owned(),
        request_id: Some(moved_id),
    });
    assert_eq!(
        note(&mut h),
        None,
        "the kept snapshot is held, so the subagent is not unavailable"
    );
    assert_eq!(header(&mut h), Some(a1_header("finished")));

    h.send(append(source(), Vec::new(), true));

    assert_eq!(
        note(&mut h),
        None,
        "the old session's end is not this pane's"
    );
}

#[gpui::test]
fn a_run_pane_beside_it_still_follows_its_run(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = fixture();
    fixture.tabs = vec![tab(
        "t1",
        &split(
            SplitDirection::Horizontal,
            follow_pane("f3", "r1"),
            follow_subagent_pane("f2", "s1", "a1"),
        ),
    )];
    let mut h = Harness::with(cx, &dir, &fixture);
    h.sent();
    h.reconnect_with_features(&[protocol::FEATURE_DISPATCH_RUNS]);
    h.send(DaemonMessage::DispatchRuns {
        runs: vec![dispatch_run("r1").actions(&["read a.rs"]).build()],
    });
    let sent = h.sent();
    let run_source = TranscriptSource::DispatchRun {
        run_id: "r1".to_owned(),
    };
    let run_id = follows_of(&sent, &run_source).into_iter().flatten().next();
    let sub_id = follows_of(&sent, &source()).into_iter().flatten().next();
    assert!(
        run_id.is_some() && sub_id.is_some(),
        "both the run and the subagent are followed: {sent:?}"
    );
    let (run_id, sub_id) = (run_id.unwrap_or_default(), sub_id.unwrap_or_default());

    h.send(DaemonMessage::Transcript {
        source: run_source,
        entries: vec![answer("y1", "Run says.")],
        hidden: 0,
        request_id: Some(run_id),
    });
    h.send(reply(vec![answer("x1", "Subagent says.")], Some(sub_id)));

    assert_eq!(
        h.root(|root, _| root.dispatch_view_lines("f3")),
        ["● Run says."]
    );
    assert_eq!(lines(&mut h), ["● Subagent says."]);
    assert!(
        drawn(&mut h, "dispatch-log-f3"),
        "the run's log keeps its id"
    );
    assert!(
        drawn(&mut h, "subagent-log-f2"),
        "the subagent's log has its own"
    );
}
