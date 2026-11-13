#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert their fixture preconditions and fail on an unexpected message"
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use protocol::{
    ChatEntry, DaemonMessage, SessionMode, SpawnConfig, SpawnTarget, TranscriptSource, error_codes,
};
use tokio::sync::mpsc;

use super::{Key, Owner, SubagentTails};
use crate::dispatch_runs::DispatchRuns;
use crate::server::Hub;
use crate::server::test_support::{ScratchDir, enable_dispatch_runs, idle_record, test_hub};
use crate::transcript_tail::TailEvent;

/// A passing test never reaches this: it fails only when the product is
/// broken, so it is sized by the slow path it rules out (the tail's poll).
const HANG: Duration = Duration::from_secs(60);

/// The session the fixture holds.
pub(crate) const SESSION: &str = "s1";
/// The conversation the fixture's session names.
pub(crate) const CONVERSATION: &str = "conv-1";
/// A subagent of the fixture's session.
pub(crate) const AGENT: &str = "a1b2";
/// The dispatch run a run fixture lists.
const RUN: &str = "run-1";
/// The run's own conversation, from its stream.
const RUN_CONVERSATION: &str = "run-conv-1";

pub(crate) type Inbox = mpsc::UnboundedReceiver<DaemonMessage>;

/// The session record a fixture inserts, and the folder and env rows its
/// subagent paths are built from.
struct SessionSetup {
    folder: PathBuf,
    extra_env: Vec<(String, String)>,
    record: crate::session::SessionRecord,
}

/// The record holding [`SESSION`] on [`CONVERSATION`] in a project folder of
/// `scratch`, whose agent home is in the scratch dir too.
fn session_setup(scratch: &ScratchDir) -> SessionSetup {
    let folder = scratch.path().join("proj");
    std::fs::create_dir_all(&folder).expect("create the project folder");
    let home = scratch.path().join("claude-home");
    let extra_env = vec![("CLAUDE_CONFIG_DIR".to_owned(), home.display().to_string())];
    let mut record = idle_record(SESSION);
    record.conversation_id = Some(CONVERSATION.to_owned());
    record.spawn_config = Some(SpawnConfig {
        target: SpawnTarget::Standalone {
            cwd: Some(folder.display().to_string()),
            add_dirs: Vec::new(),
        },
        mode: SessionMode::Interactive,
        dangerously_skip_permissions: false,
        agent: None,
        extra_args: Vec::new(),
        extra_env: extra_env.clone(),
    });
    SessionSetup {
        folder,
        extra_env,
        record,
    }
}

/// A hub with one client connected, so tails read, holding session
/// [`SESSION`] on [`CONVERSATION`] in a scratch project folder whose agent
/// home is in the scratch dir too.
pub(crate) struct Fixture {
    pub(crate) hub: Hub,
    folder: PathBuf,
    extra_env: Vec<(String, String)>,
    _scratch: ScratchDir,
}

impl Fixture {
    pub(crate) fn new(tag: &str) -> Self {
        let (hub, scratch) = test_hub(tag);
        hub.client_count.send_replace(1);
        let setup = session_setup(&scratch);
        hub.sessions.insert(setup.record);
        Self {
            hub,
            folder: setup.folder,
            extra_env: setup.extra_env,
            _scratch: scratch,
        }
    }

    pub(crate) fn tails(&self) -> &SubagentTails {
        &self.hub.subagent_tails
    }

    /// Where the plugin keeps `agent`'s transcript; its folder is not made.
    pub(crate) fn path(&self, agent: &str) -> PathBuf {
        let plugin = agent_registry::plugin(&agent_registry::default_agent())
            .expect("the default agent is registered");
        let rows = crate::conversations::lookup_rows(&self.extra_env);
        plugin
            .subagent_transcript_path(&self.folder, CONVERSATION, agent, &rows)
            .expect("the plugin keeps subagent transcripts")
    }

    /// Append `lines` to `agent`'s transcript, its folder made.
    pub(crate) fn append(&self, agent: &str, lines: &[String]) {
        append_lines(&self.path(agent), lines);
    }

    /// Make `connection` follow `agent` on a fresh queue, returning its inbox.
    fn follow(&self, agent: &str, connection: u64, request_id: Option<&str>) -> Inbox {
        follow_on(self.tails(), &key(agent), connection, request_id)
    }
}

/// A hub with one client connected and a dispatch run listed, holding the
/// same session as [`Fixture`] and listing [`RUN`] on [`RUN_CONVERSATION`] in
/// a worktree of the scratch dir.
struct RunFixture {
    hub: Hub,
    runs: Arc<DispatchRuns>,
    worktree: PathBuf,
    extra_env: Vec<(String, String)>,
    _scratch: ScratchDir,
}

impl RunFixture {
    fn new(tag: &str) -> Self {
        let (mut hub, scratch) = test_hub(tag);
        hub.client_count.send_replace(1);
        let setup = session_setup(&scratch);
        hub.sessions.insert(setup.record);
        let runs = enable_dispatch_runs(&mut hub, scratch.path().join("live"));
        let worktree = scratch.path().join("run-worktree");
        std::fs::create_dir_all(&worktree).expect("create the run's worktree");
        runs.list_streaming_for_test(
            RUN,
            &worktree.display().to_string(),
            Some(CONVERSATION),
            Some(RUN_CONVERSATION),
        );
        Self {
            hub,
            runs,
            worktree,
            extra_env: setup.extra_env,
            _scratch: scratch,
        }
    }

    fn tails(&self) -> &SubagentTails {
        &self.hub.subagent_tails
    }

    /// Where the plugin keeps `agent`'s transcript under the run's worktree.
    fn path(&self, agent: &str) -> PathBuf {
        let plugin = agent_registry::plugin(&agent_registry::default_agent())
            .expect("the default agent is registered");
        let rows = crate::conversations::lookup_rows(&self.extra_env);
        plugin
            .subagent_transcript_path(&self.worktree, RUN_CONVERSATION, agent, &rows)
            .expect("the plugin keeps subagent transcripts")
    }

    /// Append `lines` to `agent`'s transcript, its folder made.
    fn append(&self, agent: &str, lines: &[String]) {
        append_lines(&self.path(agent), lines);
    }

    /// Make `connection` follow `agent` of the run on a fresh queue.
    fn follow(&self, agent: &str, connection: u64, request_id: Option<&str>) -> Inbox {
        follow_on(self.tails(), &run_key(agent), connection, request_id)
    }
}

/// Append `lines` to the transcript at `path`, its folder made.
fn append_lines(path: &Path, lines: &[String]) {
    use std::io::Write as _;
    std::fs::create_dir_all(path.parent().expect("a transcript has a folder"))
        .expect("create the transcript's folder");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open the transcript");
    for line in lines {
        writeln!(file, "{line}").expect("append to the transcript");
    }
}

/// Make `connection` follow `key` on a fresh queue, returning its inbox.
fn follow_on(tails: &SubagentTails, key: &Key, connection: u64, request_id: Option<&str>) -> Inbox {
    let (outbox, inbox) = mpsc::unbounded_channel();
    tails.follow(key, request_id.map(str::to_owned), connection, &outbox);
    inbox
}

pub(crate) fn key(agent: &str) -> Key {
    (Owner::Session(SESSION.to_owned()), agent.to_owned())
}

/// The key of `agent` under [`RUN`].
fn run_key(agent: &str) -> Key {
    (Owner::Run(RUN.to_owned()), agent.to_owned())
}

pub(crate) fn source(agent: &str) -> TranscriptSource {
    TranscriptSource::Subagent {
        session_id: SESSION.to_owned(),
        agent_id: agent.to_owned(),
    }
}

/// The wire source of `agent` under [`RUN`].
fn run_source(agent: &str) -> TranscriptSource {
    TranscriptSource::RunSubagent {
        run_id: RUN.to_owned(),
        agent_id: agent.to_owned(),
    }
}

/// An assistant record answering `text` under its id `uuid`.
pub(crate) fn text_line(uuid: &str, text: &str) -> String {
    format!(
        r#"{{"type":"assistant","uuid":"{uuid}","message":{{"content":[{{"type":"text","text":"{text}"}}]}}}}"#
    )
}

/// The next message on `inbox`, failing as "hung" past [`HANG`].
pub(crate) async fn next(inbox: &mut Inbox) -> DaemonMessage {
    tokio::time::timeout(HANG, inbox.recv())
        .await
        .expect("hung: no message within the hang guard")
        .expect("the inbox closed")
}

fn ids(entries: &[ChatEntry]) -> Vec<String> {
    entries
        .iter()
        .filter_map(crate::chat_mirror::entry_id)
        .map(str::to_owned)
        .collect()
}

/// The next message, which must be a backfill of `expected` answering
/// `request_id`; returns its entries' ids.
async fn backfill_of(
    inbox: &mut Inbox,
    expected: &TranscriptSource,
    request_id: Option<&str>,
) -> Vec<String> {
    match next(inbox).await {
        DaemonMessage::Transcript {
            source: got,
            entries,
            hidden,
            request_id: got_request,
        } => {
            assert_eq!(&got, expected);
            assert_eq!(hidden, 0);
            assert_eq!(got_request.as_deref(), request_id);
            ids(&entries)
        }
        other => panic!("expected a backfill, got {other:?}"),
    }
}

/// The next message, which must be `agent`'s backfill answering
/// `request_id`; returns its entries' ids.
pub(crate) async fn backfill(
    inbox: &mut Inbox,
    agent: &str,
    request_id: Option<&str>,
) -> Vec<String> {
    backfill_of(inbox, &source(agent), request_id).await
}

/// The next message, which must be a backfill of `agent` under [`RUN`].
async fn run_backfill(inbox: &mut Inbox, agent: &str, request_id: Option<&str>) -> Vec<String> {
    backfill_of(inbox, &run_source(agent), request_id).await
}

/// The next message, which must be an append of `expected`; returns its
/// entries' ids and whether it ended the transcript.
async fn append_of(inbox: &mut Inbox, expected: &TranscriptSource) -> (Vec<String>, bool) {
    match next(inbox).await {
        DaemonMessage::TranscriptAppend {
            source: got,
            entries,
            ended,
        } => {
            assert_eq!(&got, expected);
            (ids(&entries), ended)
        }
        other => panic!("expected an append, got {other:?}"),
    }
}

/// The next message, which must be an append of `agent`'s; returns its
/// entries' ids and whether it ended the transcript.
pub(crate) async fn append(inbox: &mut Inbox, agent: &str) -> (Vec<String>, bool) {
    append_of(inbox, &source(agent)).await
}

/// The next message, which must be an append of `agent`'s under [`RUN`].
async fn run_append(inbox: &mut Inbox, agent: &str) -> (Vec<String>, bool) {
    append_of(inbox, &run_source(agent)).await
}

/// The code, message and request id of an `Error` reply.
fn refusal(message: DaemonMessage) -> (Option<String>, String, Option<String>) {
    match message {
        DaemonMessage::Error {
            message,
            request_id,
            code,
        } => (code, message, request_id),
        other => panic!("expected an Error, got {other:?}"),
    }
}

fn strings(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|id| (*id).to_owned()).collect()
}

fn nothing_on(inbox: &mut Inbox) -> bool {
    inbox.try_recv().is_err()
}

#[tokio::test]
async fn a_follow_answers_the_backfill_with_its_request_id() {
    let fixture = Fixture::new("subagent-tail-backfill");
    fixture.append(
        AGENT,
        &[
            text_line("m1", "one"),
            String::new(),
            text_line("m2", "two"),
        ],
    );

    let mut first = fixture.follow(AGENT, 1, Some("r1"));
    assert_eq!(
        backfill(&mut first, AGENT, Some("r1")).await,
        Vec::<String>::new()
    );
    // The first read folds in one append, the blank line yielding nothing.
    assert_eq!(
        append(&mut first, AGENT).await,
        (strings(&["m1", "m2"]), false)
    );

    let mut second = fixture.follow(AGENT, 2, Some("r2"));
    assert_eq!(
        backfill(&mut second, AGENT, Some("r2")).await,
        strings(&["m1", "m2"])
    );
    assert_eq!(
        fixture.tails().tail_count(),
        1,
        "the second follow joins the tail"
    );
}

#[tokio::test]
async fn appended_lines_reach_each_follower_once() {
    let fixture = Fixture::new("subagent-tail-twice");
    fixture.append(AGENT, &[text_line("m1", "one")]);
    let mut first = fixture.follow(AGENT, 1, None);
    backfill(&mut first, AGENT, None).await;
    assert_eq!(append(&mut first, AGENT).await, (strings(&["m1"]), false));
    let mut second = fixture.follow(AGENT, 2, None);
    assert_eq!(backfill(&mut second, AGENT, None).await, strings(&["m1"]));
    let (_third_out, mut third) = mpsc::unbounded_channel::<DaemonMessage>();

    fixture.append(AGENT, &[text_line("m2", "two")]);
    assert_eq!(append(&mut first, AGENT).await, (strings(&["m2"]), false));
    assert_eq!(append(&mut second, AGENT).await, (strings(&["m2"]), false));
    // A repeat of m2 would arrive before m3.
    fixture.append(AGENT, &[text_line("m3", "three")]);
    assert_eq!(append(&mut first, AGENT).await, (strings(&["m3"]), false));
    assert_eq!(append(&mut second, AGENT).await, (strings(&["m3"]), false));
    assert!(
        nothing_on(&mut third),
        "a connection that does not follow gets nothing"
    );
}

#[tokio::test]
async fn a_follow_before_the_file_exists_gets_an_empty_backfill_then_the_lines() {
    let fixture = Fixture::new("subagent-tail-missing");
    let mut inbox = fixture.follow(AGENT, 1, Some("r1"));
    assert_eq!(
        backfill(&mut inbox, AGENT, Some("r1")).await,
        Vec::<String>::new()
    );

    fixture.append(AGENT, &[text_line("m1", "one")]);
    assert_eq!(append(&mut inbox, AGENT).await, (strings(&["m1"]), false));
}

#[tokio::test]
async fn the_last_unfollow_stops_the_tail() {
    let fixture = Fixture::new("subagent-tail-unfollow");
    fixture.append(AGENT, &[text_line("m1", "one")]);
    let mut first = fixture.follow(AGENT, 1, None);
    backfill(&mut first, AGENT, None).await;
    append(&mut first, AGENT).await;
    let mut second = fixture.follow(AGENT, 2, None);
    backfill(&mut second, AGENT, None).await;

    fixture.tails().unfollow(&key(AGENT), 1);
    assert_eq!(fixture.tails().tail_count(), 1, "a follower is left");
    fixture.tails().unfollow(&key(AGENT), 2);
    assert_eq!(
        fixture.tails().tail_count(),
        0,
        "the last unfollow drops the entry"
    );

    fixture.append(AGENT, &[text_line("m2", "two")]);
    // A fresh follow reads the file from its start on a tail of its own.
    let mut third = fixture.follow(AGENT, 3, None);
    assert_eq!(
        backfill(&mut third, AGENT, None).await,
        Vec::<String>::new()
    );
    assert_eq!(
        append(&mut third, AGENT).await,
        (strings(&["m1", "m2"]), false)
    );
    assert!(
        nothing_on(&mut first),
        "an unfollowed connection gets no append"
    );
    assert!(
        nothing_on(&mut second),
        "an unfollowed connection gets no append"
    );
}

#[tokio::test]
async fn a_closed_connection_stops_its_tails() {
    let fixture = Fixture::new("subagent-tail-guard");
    let guard = fixture.tails().follow_guard(1);
    let mut first = fixture.follow(AGENT, 1, None);
    let mut other = fixture.follow("c3d4", 1, None);
    let mut kept = fixture.follow(AGENT, 2, None);
    backfill(&mut first, AGENT, None).await;
    backfill(&mut other, "c3d4", None).await;
    backfill(&mut kept, AGENT, None).await;
    assert_eq!(fixture.tails().tail_count(), 2);

    drop(guard);
    assert_eq!(
        fixture.tails().tail_count(),
        1,
        "the closed connection's only tail stops; the shared one stays"
    );
    fixture.append(AGENT, &[text_line("m1", "one")]);
    assert_eq!(append(&mut kept, AGENT).await, (strings(&["m1"]), false));
    assert!(nothing_on(&mut first), "a closed connection gets no append");
}

#[tokio::test]
async fn removing_the_session_sends_ended_and_stops_the_tail() {
    let fixture = Fixture::new("subagent-tail-removed");
    let _watcher = fixture.tails().spawn_removal_watcher();
    fixture.append(AGENT, &[text_line("m1", "one")]);
    let mut inbox = fixture.follow(AGENT, 1, None);
    backfill(&mut inbox, AGENT, None).await;
    append(&mut inbox, AGENT).await;

    fixture.hub.sessions.remove(SESSION);
    assert_eq!(append(&mut inbox, AGENT).await, (Vec::new(), true));
    assert_eq!(fixture.tails().tail_count(), 0);
}

#[tokio::test]
async fn a_removal_between_the_record_read_and_the_lock_still_ends_the_follow() {
    let fixture = Fixture::new("subagent-tail-race");
    let sessions = std::sync::Arc::clone(&fixture.hub.sessions);
    fixture
        .tails()
        .before_tail_lock(move || sessions.remove(SESSION));
    let mut inbox = fixture.follow(AGENT, 1, Some("r1"));

    assert_eq!(
        backfill(&mut inbox, AGENT, Some("r1")).await,
        Vec::<String>::new()
    );
    assert_eq!(append(&mut inbox, AGENT).await, (Vec::new(), true));
    assert_eq!(fixture.tails().tail_count(), 0);
}

#[tokio::test]
async fn a_stopped_tails_late_line_is_not_folded_into_a_new_tail() {
    let fixture = Fixture::new("subagent-tail-generation");
    fixture.append(AGENT, &[text_line("m1", "one")]);
    let mut first = fixture.follow(AGENT, 1, None);
    backfill(&mut first, AGENT, None).await;
    append(&mut first, AGENT).await;
    let stale = fixture
        .tails()
        .generation(&key(AGENT))
        .expect("a tail runs");
    fixture.tails().unfollow(&key(AGENT), 1);

    let mut second = fixture.follow(AGENT, 2, None);
    backfill(&mut second, AGENT, None).await;
    assert_eq!(append(&mut second, AGENT).await, (strings(&["m1"]), false));
    assert_ne!(fixture.tails().generation(&key(AGENT)), Some(stale));

    // The stopped tail's read was already under way when it stopped.
    fixture.tails().deliver(
        &key(AGENT),
        stale,
        TailEvent::Line(text_line("late", "late")),
    );
    fixture.append(AGENT, &[text_line("m2", "two")]);
    assert_eq!(
        append(&mut second, AGENT).await,
        (strings(&["m2"]), false),
        "the stale line is dropped, so the next append is the real one"
    );
}

#[tokio::test]
async fn a_run_subagent_follow_answers_the_backfill_with_its_request_id() {
    let fixture = RunFixture::new("run-tail-backfill");
    fixture.append(AGENT, &[text_line("m1", "one"), text_line("m2", "two")]);

    let mut first = fixture.follow(AGENT, 1, None);
    assert_eq!(
        run_backfill(&mut first, AGENT, None).await,
        Vec::<String>::new()
    );
    assert_eq!(
        run_append(&mut first, AGENT).await,
        (strings(&["m1", "m2"]), false)
    );

    let mut second = fixture.follow(AGENT, 2, Some("r1"));
    assert_eq!(
        run_backfill(&mut second, AGENT, Some("r1")).await,
        strings(&["m1", "m2"]),
        "the reply is the run subagent's backfill answering r1"
    );
}

#[tokio::test]
async fn a_run_subagents_appended_lines_reach_its_follower_once() {
    let fixture = RunFixture::new("run-tail-once");
    fixture.append(AGENT, &[text_line("m1", "one")]);
    let mut inbox = fixture.follow(AGENT, 1, None);
    run_backfill(&mut inbox, AGENT, None).await;
    assert_eq!(
        run_append(&mut inbox, AGENT).await,
        (strings(&["m1"]), false)
    );

    fixture.append(AGENT, &[text_line("m2", "two")]);
    assert_eq!(
        run_append(&mut inbox, AGENT).await,
        (strings(&["m2"]), false)
    );
    fixture.append(AGENT, &[text_line("m3", "three")]);
    assert_eq!(
        run_append(&mut inbox, AGENT).await,
        (strings(&["m3"]), false)
    );
    assert!(
        nothing_on(&mut inbox),
        "each appended line arrives once and nothing else is queued"
    );
}

#[tokio::test]
async fn an_unknown_run_answers_not_found() {
    let fixture = RunFixture::new("run-tail-unknown");
    let unknown = (Owner::Run("nope".to_owned()), AGENT.to_owned());
    let mut inbox = follow_on(fixture.tails(), &unknown, 1, Some("r1"));

    assert_eq!(
        refusal(next(&mut inbox).await),
        (
            Some(error_codes::NOT_FOUND.to_owned()),
            "unknown subagent: nope/a1b2".to_owned(),
            Some("r1".to_owned())
        )
    );
    assert_eq!(fixture.tails().tail_count(), 0);
}

#[tokio::test]
async fn a_refused_run_agent_id_answers_not_found() {
    let fixture = RunFixture::new("run-tail-refused-agent");
    let mut inbox = fixture.follow("../x", 1, Some("r1"));

    assert_eq!(
        refusal(next(&mut inbox).await),
        (
            Some(error_codes::NOT_FOUND.to_owned()),
            format!("unknown subagent: {RUN}/../x"),
            Some("r1".to_owned())
        )
    );
    assert_eq!(fixture.tails().tail_count(), 0);
}

#[tokio::test]
async fn a_run_without_its_conversation_answers_not_found() {
    let fixture = RunFixture::new("run-tail-no-conversation");
    assert!(
        fixture.runs.relist_for_test(RUN, None).is_some(),
        "the run is listed again without a conversation"
    );
    let mut inbox = fixture.follow(AGENT, 1, Some("r1"));

    assert_eq!(
        refusal(next(&mut inbox).await),
        (
            Some(error_codes::NOT_FOUND.to_owned()),
            format!("unknown subagent: {RUN}/{AGENT}"),
            Some("r1".to_owned())
        )
    );
    assert_eq!(fixture.tails().tail_count(), 0);
}

#[tokio::test]
async fn removing_the_run_sends_ended_and_stops_the_tail() {
    let fixture = RunFixture::new("run-tail-removed");
    let _watcher = fixture.tails().spawn_removal_watcher();
    fixture.append(AGENT, &[text_line("m1", "one")]);
    let mut inbox = fixture.follow(AGENT, 1, None);
    run_backfill(&mut inbox, AGENT, None).await;
    run_append(&mut inbox, AGENT).await;

    assert!(fixture.runs.remove_for_test(RUN), "the run was listed");
    assert_eq!(run_append(&mut inbox, AGENT).await, (Vec::new(), true));
    assert_eq!(fixture.tails().tail_count(), 0);
}

#[tokio::test]
async fn a_run_removal_between_the_resolve_and_the_lock_still_ends_the_follow() {
    let fixture = RunFixture::new("run-tail-race");
    let runs = Arc::clone(&fixture.runs);
    fixture.tails().before_tail_lock(move || {
        runs.remove_for_test(RUN);
    });
    let mut inbox = fixture.follow(AGENT, 1, Some("r1"));

    assert_eq!(
        run_backfill(&mut inbox, AGENT, Some("r1")).await,
        Vec::<String>::new()
    );
    assert_eq!(run_append(&mut inbox, AGENT).await, (Vec::new(), true));
    assert_eq!(fixture.tails().tail_count(), 0);
}
