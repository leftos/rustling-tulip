#![expect(clippy::expect_used, reason = "tests assert preconditions with expect")]

use std::sync::Arc;

use agent_api::{ActivitySnapshot, SubagentNode};
use chrono::{DateTime, Duration, TimeZone as _, Utc};
use protocol::{AgentState, SessionMember, SessionMode, SessionStatus};
use tokio::sync::{broadcast, watch};

use super::{Job, SubagentList, TICK, live_sessions};
use crate::dashboard::test_support::Script;
use crate::server::test_support::{ScratchDir, idle_record, scratch_dirs};
use crate::session::{SessionEvent, SessionRegistry};

/// The host session's conversation, which the script reads by.
const CONVERSATION: &str = "conv1";

fn at(minute: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 12, 9, 0, 0)
        .single()
        .expect("a valid time")
        + Duration::minutes(minute)
}

/// A subagent `id` in `state`, started at minute 0 or before, and finished at
/// `finished` (a minute) when that is given.
fn node(id: &str, state: AgentState, finished: Option<i64>) -> SubagentNode {
    SubagentNode {
        agent_id: id.to_owned(),
        parent_agent_id: None,
        agent_type: Some("Explore".to_owned()),
        description: Some(format!("look at {id}")),
        model: Some("Opus 5.5".to_owned()),
        effort: None,
        started_at: Some(at(finished.map_or(0, |minute| minute - 1))),
        finished_at: finished.map(at),
        state,
        end_status: None,
        open_call: None,
        last_text: None,
        failure_run: 0,
        activity: [0; agent_api::MINUTE_COLUMNS],
        children: Vec::new(),
    }
}

fn running(id: &str) -> SubagentNode {
    node(id, AgentState::Running, None)
}

fn finished(id: &str, minute: i64) -> SubagentNode {
    node(id, AgentState::Finished, Some(minute))
}

/// A reading holding `subagents`, its newest typed prompt at `prompt` (a
/// minute) when that is given.
fn reading(subagents: Vec<SubagentNode>, prompt: Option<i64>) -> ActivitySnapshot {
    ActivitySnapshot {
        subagents,
        last_typed_prompt_at: prompt.map(at),
        ..ActivitySnapshot::default()
    }
}

/// A registry holding one live agent session `s1` whose process started at
/// minute 0, a list service reading it through a script, and the registry's
/// events from after the session was added.
struct Fixture {
    _scratch: ScratchDir,
    sessions: Arc<SessionRegistry>,
    script: Script,
    list: SubagentList,
    events: broadcast::Receiver<SessionEvent>,
}

fn fixture(tag: &str) -> Fixture {
    let scratch = ScratchDir::new(tag);
    let sessions = SessionRegistry::new(scratch_dirs(scratch.path(), None));
    let mut record = idle_record("s1");
    record.conversation_id = Some(CONVERSATION.to_owned());
    record.started_at = at(0);
    record.members = vec![SessionMember {
        repo_id: "r1".to_owned(),
        repo_name: "repo".to_owned(),
        branch: "main".to_owned(),
        worktree_path: "C:/work/repo".to_owned(),
    }];
    sessions.insert(record);
    let script = Script::default();
    let list = SubagentList::with_reader_factory(Arc::clone(&sessions), script.factory());
    let events = sessions.subscribe();
    Fixture {
        _scratch: scratch,
        sessions,
        script,
        list,
        events,
    }
}

impl Fixture {
    /// The session's list as each `SessionUpdated` since the last call
    /// carried it, as `(agent id, depth, state)`.
    fn updates(&mut self) -> Vec<Vec<(String, u8, AgentState)>> {
        let mut updates = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            if let SessionEvent::Updated(snapshot, _) = event
                && snapshot.id == "s1"
            {
                updates.push(rows(&snapshot.subagents));
            }
        }
        updates
    }

    /// The session's list as its snapshot carries it now.
    fn listed(&self) -> Vec<(String, u8, AgentState)> {
        let snapshot = self
            .sessions
            .snapshots()
            .into_iter()
            .find(|snapshot| snapshot.id == "s1")
            .expect("the session is in the registry");
        rows(&snapshot.subagents)
    }

    fn ids(&self) -> Vec<String> {
        self.listed().into_iter().map(|(id, _, _)| id).collect()
    }
}

fn rows(list: &[protocol::SubagentSnapshot]) -> Vec<(String, u8, AgentState)> {
    list.iter()
        .map(|subagent| (subagent.agent_id.clone(), subagent.depth, subagent.state))
        .collect()
}

fn row(id: &str, depth: u8, state: AgentState) -> (String, u8, AgentState) {
    (id.to_owned(), depth, state)
}

/// The job a pass would build for the fixture's one live session, reading
/// `snapshot`.
fn job_for(fx: &Fixture, snapshot: ActivitySnapshot) -> Job {
    let mut live = live_sessions(&fx.sessions, &fx.sessions.snapshots());
    assert_eq!(live.len(), 1, "the fixture's one session is live");
    Job {
        live: live.pop().expect("one live session"),
        reader: None,
        snapshot: Some(snapshot),
    }
}

#[tokio::test]
async fn a_running_subagent_reaches_the_snapshot_with_one_session_updated() {
    let mut fx = fixture("subagents-running");
    fx.script
        .set(CONVERSATION, reading(vec![running("a1")], None));

    fx.list.pass().await;

    let expected = vec![row("a1", 1, AgentState::Running)];
    assert_eq!(fx.updates(), vec![expected.clone()]);
    assert_eq!(fx.listed(), expected);
    let snapshot = fx
        .sessions
        .snapshots()
        .into_iter()
        .find(|snapshot| snapshot.id == "s1")
        .expect("the session is in the registry");
    let subagent = &snapshot.subagents[0];
    assert_eq!(subagent.description.as_deref(), Some("look at a1"));
    assert_eq!(subagent.agent_type.as_deref(), Some("Explore"));
    assert_eq!(subagent.model.as_deref(), Some("Opus 5.5"));
    assert_eq!(subagent.parent_agent_id, None);
    assert_eq!(subagent.started_at, Some(at(0)));
}

#[tokio::test]
async fn an_unchanged_pass_sends_no_update() {
    let mut fx = fixture("subagents-unchanged");
    fx.script
        .set(CONVERSATION, reading(vec![running("a1")], None));
    fx.list.pass().await;
    assert_eq!(fx.updates().len(), 1);

    fx.list.pass().await;

    assert_eq!(fx.script.reads(), 2, "the second pass read");
    assert_eq!(fx.script.builds(), 1, "with the same reader");
    assert_eq!(fx.updates(), Vec::<Vec<_>>::new());
}

#[tokio::test]
async fn a_finished_subagent_stays_until_a_newer_typed_prompt() {
    let mut fx = fixture("subagents-prompt");
    fx.script
        .set(CONVERSATION, reading(vec![finished("a1", 5)], Some(2)));
    fx.list.pass().await;
    assert_eq!(fx.listed(), vec![row("a1", 1, AgentState::Finished)]);

    fx.script
        .set(CONVERSATION, reading(vec![finished("a1", 5)], Some(4)));
    fx.list.pass().await;
    assert_eq!(
        fx.ids(),
        vec!["a1"],
        "a prompt older than its finish keeps it"
    );

    fx.updates();
    fx.script
        .set(CONVERSATION, reading(vec![finished("a1", 5)], Some(6)));
    fx.list.pass().await;
    assert_eq!(fx.updates(), vec![Vec::new()]);
}

#[tokio::test]
async fn a_subagent_finished_before_the_process_start_is_never_listed() {
    let mut fx = fixture("subagents-before-start");
    fx.script.set(
        CONVERSATION,
        reading(vec![finished("old", -5), finished("new", 3)], None),
    );

    fx.list.pass().await;

    assert_eq!(fx.ids(), vec!["new"]);
    assert_eq!(fx.updates().len(), 1);
}

#[tokio::test]
async fn a_sixth_finish_drops_the_oldest() {
    let fx = fixture("subagents-sixth");
    let five: Vec<SubagentNode> = (1..=5)
        .map(|minute| finished(&format!("a{minute}"), minute))
        .collect();
    fx.script.set(CONVERSATION, reading(five.clone(), Some(0)));
    fx.list.pass().await;
    assert_eq!(fx.ids(), vec!["a1", "a2", "a3", "a4", "a5"]);

    let mut six = five;
    six.push(finished("a6", 6));
    fx.script.set(CONVERSATION, reading(six, Some(0)));
    fx.list.pass().await;

    assert_eq!(fx.ids(), vec!["a2", "a3", "a4", "a5", "a6"]);
}

#[tokio::test]
async fn a_nested_running_child_keeps_its_finished_parent_listed() {
    let fx = fixture("subagents-nested");
    let mut parent = finished("a1", 2);
    let mut child = running("a2");
    child.parent_agent_id = Some("a1".to_owned());
    parent.children = vec![child];
    fx.script.set(
        CONVERSATION,
        reading(vec![parent, finished("a3", 2)], Some(3)),
    );

    fx.list.pass().await;

    assert_eq!(
        fx.listed(),
        vec![
            row("a1", 1, AgentState::Finished),
            row("a2", 2, AgentState::Running),
        ]
    );
}

#[tokio::test]
async fn a_stopped_sessions_list_empties_once() {
    let mut fx = fixture("subagents-stopped");
    fx.script
        .set(CONVERSATION, reading(vec![running("a1")], None));
    fx.list.pass().await;
    assert_eq!(fx.ids(), vec!["a1"]);
    fx.sessions
        .update("s1", |record| record.status = SessionStatus::Stopped);
    fx.updates();

    fx.list.pass().await;
    assert_eq!(fx.updates(), vec![Vec::new()]);
    fx.list.pass().await;

    assert_eq!(fx.updates(), Vec::<Vec<_>>::new());
}

#[tokio::test]
async fn an_agent_ended_in_its_shell_empties_the_list() {
    let mut fx = fixture("subagents-agent-ended");
    fx.script
        .set(CONVERSATION, reading(vec![running("a1")], None));
    fx.list.pass().await;
    assert_eq!(fx.ids(), vec!["a1"]);

    fx.sessions.update("s1", |record| {
        record.mode = SessionMode::PlainShell;
        record.agent_in_shell = true;
        record.shell_label = Some("pwsh".to_owned());
        record.program_name = Some("pwsh".to_owned());
        record.status = SessionStatus::Idle;
    });
    fx.updates();

    fx.list.pass().await;
    assert_eq!(
        fx.updates(),
        vec![Vec::new()],
        "the ended agent's list empties"
    );
    fx.list.pass().await;

    assert_eq!(fx.updates(), Vec::<Vec<_>>::new());
}

#[tokio::test]
async fn a_reading_for_a_stopped_session_is_not_written() {
    let mut fx = fixture("subagents-stopped-reading");
    fx.script
        .set(CONVERSATION, reading(vec![running("a1")], None));
    fx.list.pass().await;
    assert_eq!(fx.ids(), vec!["a1"]);

    let job = job_for(&fx, reading(vec![running("a2")], None));
    fx.sessions
        .update("s1", |record| record.status = SessionStatus::Stopped);
    fx.updates();

    fx.list.apply(job);

    assert_eq!(fx.updates(), Vec::<Vec<_>>::new(), "no update is sent");
    assert_eq!(fx.ids(), vec!["a1"], "the last live reading stays");
}

#[tokio::test]
async fn a_reading_for_an_old_conversation_is_not_written() {
    let mut fx = fixture("subagents-old-conversation");
    fx.script
        .set(CONVERSATION, reading(vec![running("a1")], None));
    fx.list.pass().await;
    assert_eq!(fx.ids(), vec!["a1"]);

    let job = job_for(&fx, reading(vec![running("a2")], None));
    fx.sessions.update("s1", |record| {
        record.conversation_id = Some("conv2".to_owned());
    });
    fx.updates();

    fx.list.apply(job);

    assert_eq!(fx.updates(), Vec::<Vec<_>>::new(), "no update is sent");
    assert_eq!(fx.ids(), vec!["a1"], "the old conversation's list stays");
}

#[tokio::test(start_paused = true)]
async fn at_no_clients_no_reader_is_called() {
    let fx = fixture("subagents-no-clients");
    fx.script
        .set(CONVERSATION, reading(vec![running("a1")], None));
    let (clients, clients_rx) = watch::channel(0usize);
    let task = fx.list.spawn(clients_rx);

    tokio::time::sleep(TICK * 10).await;
    assert_eq!(fx.script.reads(), 0, "no pass runs with no client");

    clients.send_replace(1);
    tokio::time::sleep(TICK / 2).await;
    assert_eq!(
        fx.script.reads(),
        1,
        "a returning client gets a catch-up pass"
    );
    assert_eq!(fx.ids(), vec!["a1"]);
    tokio::time::sleep(TICK * 3).await;
    assert!(
        fx.script.reads() >= 3,
        "the tick reads while a client is connected: {}",
        fx.script.reads()
    );

    clients.send_replace(0);
    // A pass already under way when the last client left may still finish.
    tokio::time::sleep(TICK).await;
    let settled = fx.script.reads();
    tokio::time::sleep(TICK * 10).await;
    assert_eq!(fx.script.reads(), settled, "no pass runs once none is left");
    task.abort();
}

#[tokio::test(start_paused = true)]
async fn a_tick_after_the_last_client_left_reads_nothing() {
    let fx = fixture("subagents-tick-after-last-client");
    fx.script
        .set(CONVERSATION, reading(vec![running("a1")], None));
    let (clients, clients_rx) = watch::channel(1usize);
    let task = fx.list.spawn(clients_rx);
    // Stops between two ticks, with every pass of a connected client done.
    tokio::time::sleep(TICK * 3 + TICK / 2).await;
    let reads = fx.script.reads();
    assert!(
        reads > 0,
        "passes ran while a client was connected: {reads}"
    );

    clients.send_replace(0);
    tokio::time::sleep(TICK * 10).await;

    assert_eq!(
        fx.script.reads(),
        reads,
        "the tick due after the last client left reads nothing"
    );
    task.abort();
}

#[tokio::test]
async fn no_typed_prompt_keeps_finished_subagents_listed_up_to_five() {
    let fx = fixture("subagents-no-prompt");
    let six: Vec<SubagentNode> = (1..=6)
        .map(|minute| finished(&format!("a{minute}"), minute))
        .collect();
    fx.script.set(CONVERSATION, reading(six, None));

    fx.list.pass().await;

    assert_eq!(fx.ids(), vec!["a2", "a3", "a4", "a5", "a6"]);
}

#[tokio::test]
async fn newer_transcript_lines_make_a_finished_flagged_subagent_running() {
    let fx = fixture("subagents-resumed");
    fx.script
        .set(CONVERSATION, reading(vec![finished("a1", 2)], Some(3)));
    fx.list.pass().await;
    assert_eq!(fx.ids(), Vec::<String>::new(), "finished before the prompt");

    let mut resumed = running("a1");
    resumed.started_at = Some(at(1));
    fx.script.set(CONVERSATION, reading(vec![resumed], Some(3)));
    fx.list.pass().await;

    assert_eq!(fx.listed(), vec![row("a1", 1, AgentState::Running)]);
}
