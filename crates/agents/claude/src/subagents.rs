//! The Claude plugin's subagent tree reader. It lists the `subagents/`
//! folder beside a conversation's main transcript on each read, follows each
//! `agent-<id>.jsonl` with its own incremental [`Transcript`], reads each
//! `agent-<id>.meta.json` sidecar, and joins every subagent to the launch
//! that started it.
//!
//! A sidecar's `toolUseId` joins a first-layer subagent to the main
//! transcript's subagent calls (to any call when the main reads began past
//! the file's start, where older launches are unknown), and its
//! `parentAgentId` hangs a nested one under its launcher. A transcript with
//! no sidecar, or a sidecar with no `toolUseId` (a background forked skill),
//! joins through the background launch that named its agent id. A
//! first-layer sidecar with neither is listed only while it runs and its file
//! was written since the agent process started. Any other subagent is left
//! out, and so is one whose sidecar does not parse.
//!
//! A subagent's state follows these rules, in order:
//!
//! 1. A transcript last written before the agent process started is
//!    finished.
//! 2. A sidecar marked `stoppedByUser`, or a task notification in the
//!    launcher's transcript that ended the task and is newer than the
//!    transcript's last line, ends it: `completed` as finished, any other
//!    end as failed.
//! 3. A last reply with `end_turn` and no tool call, followed only by
//!    attachments, finishes it.
//! 4. Otherwise it runs; lines after a finished state make it run again.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use agent_api::SubagentNode;
use chrono::{DateTime, Utc};
use protocol::AgentState;
use serde_json::Value;
use tracing::debug;

use crate::activity::{self, AgentCall, Notice, State, Transcript};

/// The start of a subagent file's name, before its agent id.
const FILE_PREFIX: &str = "agent-";
/// The end of a subagent transcript's file name.
const TRANSCRIPT_SUFFIX: &str = ".jsonl";
/// The end of a subagent sidecar's file name.
const SIDECAR_SUFFIX: &str = ".meta.json";
/// The end status of a subagent whose sidecar says the user stopped it.
const STOPPED_BY_USER: &str = "stoppedByUser";

/// The subagents of one conversation, as of one read.
#[derive(Debug, Default)]
pub(crate) struct SubagentTree {
    /// The first layer, each node holding the ones it launched.
    pub(crate) nodes: Vec<SubagentNode>,
    /// The tokens every listed subagent's replies read.
    pub(crate) tokens_in: u64,
    /// The tokens every listed subagent's replies wrote.
    pub(crate) tokens_out: u64,
    /// What every listed subagent's replies would cost, in US dollars.
    pub(crate) cost_usd: f64,
}

/// The reader of one conversation's `subagents/` folder.
pub(crate) struct SubagentReader {
    dir: PathBuf,
    /// Each subagent file the folder held at the last read, by agent id.
    files: HashMap<String, AgentFile>,
}

/// One subagent's transcript and sidecar, and what has been read of them.
struct AgentFile {
    transcript: Transcript,
    /// When the transcript was last modified, as of its last read.
    modified: Option<SystemTime>,
    sidecar: Sidecar,
    /// The sidecar's length and modified time when it was last read.
    sidecar_stamp: Option<(u64, Option<SystemTime>)>,
}

/// What a subagent's sidecar holds, as of its last read.
#[derive(Clone, Debug)]
enum Sidecar {
    Missing,
    /// It exists but does not parse: half-written, or not Claude Code's.
    Unreadable,
    Read(SidecarFields),
}

/// The sidecar fields the tree uses.
#[derive(Clone, Debug, Default)]
struct SidecarFields {
    agent_type: Option<String>,
    description: Option<String>,
    tool_use_id: Option<String>,
    parent_agent_id: Option<String>,
    stopped_by_user: bool,
}

/// A subagent joined to its launch.
struct Joined {
    id: String,
    /// The subagent that launched it; `None` for the main agent.
    parent: Option<String>,
    /// The launcher's newest notification for it.
    notice: Option<Notice>,
    /// What the launching call asked for.
    call: Option<AgentCall>,
    /// Whether a launch names it; `false` for a first-layer sidecar with no
    /// `toolUseId`.
    launched: bool,
}

/// A node waiting for its place in the tree, with its figures.
struct Pending {
    parent: Option<String>,
    node: SubagentNode,
    tokens_in: u64,
    tokens_out: u64,
    cost_usd: f64,
}

/// How a subagent's run stands.
struct Ending {
    state: AgentState,
    status: Option<String>,
    finished_at: Option<DateTime<Utc>>,
}

impl SubagentReader {
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            files: HashMap::new(),
        }
    }

    /// The subagent tree as of `now`, after reading what each file gained.
    /// `main` is the main transcript's state, already read.
    pub(crate) fn read(
        &mut self,
        main: &State,
        now: DateTime<Utc>,
        process_started_at: Option<DateTime<Utc>>,
    ) -> SubagentTree {
        self.refresh();
        let joined: Vec<Joined> = self
            .files
            .iter()
            .filter_map(|(id, file)| self.join(id, file, main))
            .collect();
        let mut pending = Vec::new();
        for joined in joined {
            let Some(file) = self.files.get_mut(&joined.id) else {
                continue;
            };
            let node = file.node(&joined, now, process_started_at);
            let listed = joined.launched
                || (node.state == AgentState::Running
                    && written_since(file.modified, process_started_at));
            if listed {
                let state = &file.transcript.state;
                pending.push(Pending {
                    parent: joined.parent,
                    node,
                    tokens_in: state.tokens_in,
                    tokens_out: state.tokens_out,
                    cost_usd: state.cost_usd,
                });
            }
        }
        let mut tree = SubagentTree::default();
        let nodes = attach(None, &mut pending, &mut tree);
        tree.nodes = nodes;
        tree
    }

    /// Lists the folder, forgetting the files it no longer holds, and reads
    /// what each file gained. A missing folder holds none; a folder that
    /// cannot be listed keeps the last reading.
    fn refresh(&mut self) {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.files.clear();
                return;
            }
            Err(error) => {
                debug!(dir = %self.dir.display(), %error, "activity: the subagents folder could not be listed");
                return;
            }
        };
        let ids: Vec<String> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| agent_id_of(entry.file_name().to_str()?).map(str::to_owned))
            .collect();
        self.files.retain(|id, _| ids.contains(id));
        for id in ids {
            let dir = &self.dir;
            let file = self.files.entry(id.clone()).or_insert_with(|| {
                AgentFile::new(dir.join(format!("{FILE_PREFIX}{id}{TRANSCRIPT_SUFFIX}")))
            });
            file.refresh(&dir.join(format!("{FILE_PREFIX}{id}{SIDECAR_SUFFIX}")));
        }
    }

    /// Subagent `id` joined to its launch; `None` when no launch the reader
    /// knows of names it or its sidecar does not parse.
    fn join(&self, id: &str, file: &AgentFile, main: &State) -> Option<Joined> {
        match &file.sidecar {
            Sidecar::Unreadable => None,
            Sidecar::Read(sidecar) => self.join_sidecar(id, sidecar, main),
            Sidecar::Missing => self.join_launch(id, main),
        }
    }

    /// Joins by the sidecar: a nested subagent to its parent's file; a
    /// first-layer one to the main transcript's subagent call, or to any call
    /// when the main reads began past the launches; one with no call to the
    /// background launch that named it, else unlaunched.
    fn join_sidecar(&self, id: &str, sidecar: &SidecarFields, main: &State) -> Option<Joined> {
        let call = sidecar.tool_use_id.as_deref();
        if let Some(parent) = &sidecar.parent_agent_id {
            let launcher = &self.files.get(parent)?.transcript.state;
            return Some(Joined::new(id, Some(parent.as_str()), launcher, call, true));
        }
        match call {
            Some(call)
                if main.began_mid_file()
                    || main.agent_call_ids.iter().any(|known| known == call) =>
            {
                Some(Joined::new(id, None, main, Some(call), true))
            }
            Some(_) => None,
            None => match main.agent_launches.get(id) {
                Some(launch) => Some(Joined::new(id, None, main, launch.as_deref(), true)),
                None => Some(Joined::new(id, None, main, None, false)),
            },
        }
    }

    /// Joins a subagent with no sidecar through the background launch that
    /// named its agent id, in the main transcript or a subagent's.
    fn join_launch(&self, id: &str, main: &State) -> Option<Joined> {
        if let Some(call) = main.agent_launches.get(id) {
            return Some(Joined::new(id, None, main, call.as_deref(), true));
        }
        self.files.iter().find_map(|(parent, file)| {
            let launcher = &file.transcript.state;
            let call = launcher.agent_launches.get(id)?;
            Some(Joined::new(
                id,
                Some(parent.as_str()),
                launcher,
                call.as_deref(),
                true,
            ))
        })
    }
}

impl Joined {
    /// Subagent `id`, launched by `call` in `launcher`'s transcript.
    fn new(
        id: &str,
        parent: Option<&str>,
        launcher: &State,
        call: Option<&str>,
        launched: bool,
    ) -> Self {
        Self {
            id: id.to_owned(),
            parent: parent.map(str::to_owned),
            notice: launcher.notices.get(id).cloned(),
            call: call
                .and_then(|call| launcher.agent_calls.get(call))
                .cloned(),
            launched,
        }
    }
}

impl AgentFile {
    fn new(path: PathBuf) -> Self {
        Self {
            transcript: Transcript::new(path),
            modified: None,
            sidecar: Sidecar::Missing,
            sidecar_stamp: None,
        }
    }

    /// Reads what the transcript gained and the sidecar at `sidecar_path`
    /// when it changed.
    fn refresh(&mut self, sidecar_path: &Path) {
        match self.transcript.refresh() {
            Ok(meta) => self.modified = meta.modified().ok(),
            Err(error) => {
                debug!(path = %self.transcript.path().display(), %error, "activity: a subagent transcript could not be read");
            }
        }
        self.refresh_sidecar(sidecar_path);
    }

    fn refresh_sidecar(&mut self, path: &Path) {
        let meta = match fs::metadata(path) {
            Ok(meta) => meta,
            Err(error) => {
                if error.kind() != io::ErrorKind::NotFound {
                    debug!(path = %path.display(), %error, "activity: a subagent sidecar could not be read");
                }
                self.sidecar = Sidecar::Missing;
                self.sidecar_stamp = None;
                return;
            }
        };
        let stamp = Some((meta.len(), meta.modified().ok()));
        if stamp == self.sidecar_stamp {
            return;
        }
        match fs::read(path) {
            Ok(bytes) => {
                self.sidecar = parse_sidecar(&bytes).map_or(Sidecar::Unreadable, Sidecar::Read);
                self.sidecar_stamp = stamp;
            }
            Err(error) => {
                debug!(path = %path.display(), %error, "activity: a subagent sidecar could not be read");
            }
        }
    }

    /// The subagent's node as of `now`, with no children yet.
    fn node(
        &mut self,
        joined: &Joined,
        now: DateTime<Utc>,
        process_started_at: Option<DateTime<Utc>>,
    ) -> SubagentNode {
        let sidecar = match &self.sidecar {
            Sidecar::Read(fields) => Some(fields),
            Sidecar::Missing | Sidecar::Unreadable => None,
        };
        let state = &mut self.transcript.state;
        let ending = ending(state, sidecar, joined.notice.as_ref(), process_started_at);
        let call = joined.call.as_ref();
        let first_line = state
            .first_prompt
            .as_deref()
            .and_then(|prompt| prompt.lines().next())
            .map(str::to_owned);
        SubagentNode {
            agent_id: joined.id.clone(),
            parent_agent_id: joined.parent.clone(),
            agent_type: sidecar
                .and_then(|fields| fields.agent_type.clone())
                .or_else(|| call.and_then(|call| call.agent_type.clone())),
            description: sidecar
                .and_then(|fields| fields.description.clone())
                .or_else(|| call.and_then(|call| call.description.clone()))
                .or(first_line),
            started_at: state.first_record_at,
            finished_at: ending.finished_at,
            state: ending.state,
            end_status: ending.status,
            open_call: state.newest_open_call(),
            last_text: state.last_assistant_text.clone(),
            failure_run: state.failure_run,
            activity: state.minute_counts(now),
            children: Vec::new(),
        }
    }
}

/// How a subagent's run stands, by the rules in this module's header.
fn ending(
    state: &State,
    sidecar: Option<&SidecarFields>,
    notice: Option<&Notice>,
    process_started_at: Option<DateTime<Utc>>,
) -> Ending {
    let last = state.last_record_at;
    let ended = |state, status: Option<&str>, finished_at| Ending {
        state,
        status: status.map(str::to_owned),
        finished_at,
    };
    if process_started_at.is_some_and(|start| last.is_some_and(|last| last < start)) {
        return ended(AgentState::Finished, None, last);
    }
    if sidecar.is_some_and(|fields| fields.stopped_by_user) {
        return ended(AgentState::Failed, Some(STOPPED_BY_USER), last);
    }
    if let Some(notice) = notice
        && let Some(end) = activity::ended_state(notice.status.as_deref())
        && notice
            .at
            .is_some_and(|at| last.is_none_or(|last| at > last))
    {
        return ended(end, notice.status.as_deref(), notice.at);
    }
    if state.run_ended {
        return ended(AgentState::Finished, None, last);
    }
    ended(AgentState::Running, None, None)
}

/// Takes the pending nodes whose parent is `parent` out of `pending`, each
/// with its own children, oldest first, adding their figures to `tree`. A
/// node whose parent is never reached stays behind and is left out.
fn attach(
    parent: Option<&str>,
    pending: &mut Vec<Pending>,
    tree: &mut SubagentTree,
) -> Vec<SubagentNode> {
    let (mine, rest): (Vec<Pending>, Vec<Pending>) = std::mem::take(pending)
        .into_iter()
        .partition(|waiting| waiting.parent.as_deref() == parent);
    *pending = rest;
    let mut nodes = Vec::with_capacity(mine.len());
    for waiting in mine {
        tree.tokens_in = tree.tokens_in.saturating_add(waiting.tokens_in);
        tree.tokens_out = tree.tokens_out.saturating_add(waiting.tokens_out);
        tree.cost_usd += waiting.cost_usd;
        let mut node = waiting.node;
        let id = node.agent_id.clone();
        node.children = attach(Some(&id), pending, tree);
        nodes.push(node);
    }
    nodes.sort_by(|left, right| {
        (left.started_at, &left.agent_id).cmp(&(right.started_at, &right.agent_id))
    });
    nodes
}

/// Whether a file last modified at `modified` was written after `start`;
/// `false` when either is unknown.
fn written_since(modified: Option<SystemTime>, start: Option<DateTime<Utc>>) -> bool {
    match (modified, start) {
        (Some(modified), Some(start)) => DateTime::<Utc>::from(modified) > start,
        _ => false,
    }
}

/// The agent id a subagent transcript's file name holds:
/// `agent-<id>.jsonl`.
fn agent_id_of(name: &str) -> Option<&str> {
    name.strip_prefix(FILE_PREFIX)?
        .strip_suffix(TRANSCRIPT_SUFFIX)
        .filter(|id| !id.is_empty())
}

/// The fields of a sidecar's JSON object; `None` when it is no JSON object.
fn parse_sidecar(bytes: &[u8]) -> Option<SidecarFields> {
    let value: Value = serde_json::from_slice(bytes).ok()?;
    if !value.is_object() {
        return None;
    }
    let text = |field: &str| {
        value[field]
            .as_str()
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    };
    Some(SidecarFields {
        agent_type: text("agentType"),
        description: text("description"),
        tool_use_id: text("toolUseId"),
        parent_agent_id: text("parentAgentId"),
        stopped_by_user: value["stoppedByUser"] == true,
    })
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test that cannot write its scratch conversation must fail loudly"
)]
mod tests {
    use crate::activity::tests::{Scratch, at, lines};
    use agent_api::{ActivityReader as _, MINUTE_COLUMNS, OpenCall, SubagentNode};
    use protocol::AgentState;

    const MAIN: &str = include_str!("../tests/fixtures/subagents/main.jsonl");
    const RUNNING: &str = include_str!("../tests/fixtures/subagents/running.jsonl");
    const RUNNING_META: &str = include_str!("../tests/fixtures/subagents/running.meta.json");
    const FINISHED: &str = include_str!("../tests/fixtures/subagents/finished.jsonl");
    const FINISHED_META: &str = include_str!("../tests/fixtures/subagents/finished.meta.json");
    const NESTED_PARENT: &str = include_str!("../tests/fixtures/subagents/nested_parent.jsonl");
    const NESTED_CHILD: &str = include_str!("../tests/fixtures/subagents/nested_child.jsonl");
    const NESTED_CHILD_META: &str =
        include_str!("../tests/fixtures/subagents/nested_child.meta.json");
    const NESTED_GRANDCHILD: &str =
        include_str!("../tests/fixtures/subagents/nested_grandchild.jsonl");
    const NESTED_GRANDCHILD_META: &str =
        include_str!("../tests/fixtures/subagents/nested_grandchild.meta.json");
    /// A foreground depth-1 sidecar with neither a description nor a launch.
    const NO_LAUNCH_META: &str = r#"{"agentType":"general-purpose","spawnDepth":1,"requestShape":"foreground","requestNonInteractive":false}"#;

    const NOW: &str = "2026-09-27T10:01:00Z";

    /// A scratch conversation whose main transcript is `main.jsonl` and whose
    /// subagents folder holds each `(agent id, transcript, sidecar)`.
    fn conversation(label: &str, agents: &[(&str, &str, Option<&str>)]) -> Scratch {
        let scratch = Scratch::new(label);
        scratch.write(MAIN);
        for (id, transcript, sidecar) in agents {
            scratch.put(&format!("agent-{id}.jsonl"), transcript);
            if let Some(sidecar) = sidecar {
                scratch.put(&format!("agent-{id}.meta.json"), sidecar);
            }
        }
        scratch
    }

    fn tree(scratch: &Scratch, process_started_at: Option<&str>, now: &str) -> Vec<SubagentNode> {
        scratch
            .reader(process_started_at.map(at))
            .read(at(now))
            .subagents
    }

    /// The launcher's queue-operation line that notifies task `id` ended
    /// with `status` at `when`.
    fn notification(id: &str, status: &str, when: &str) -> String {
        queue_operation("enqueue", id, status, when)
    }

    /// The launcher's queue-operation line of kind `operation` that carries
    /// the notification that task `id` ended with `status`, written at
    /// `when`.
    fn queue_operation(operation: &str, id: &str, status: &str, when: &str) -> String {
        format!(
            r#"{{"type":"queue-operation","operation":"{operation}","timestamp":"{when}","content":"<task-notification><task-id>{id}</task-id><tool-use-id>toolu_x</tool-use-id><output-file>o</output-file><status>{status}</status><summary>Agent ended</summary></task-notification>"}}"#
        ) + "\n"
    }

    fn last_minute(count: u16) -> [u16; MINUTE_COLUMNS] {
        let mut activity = [0; MINUTE_COLUMNS];
        activity[MINUTE_COLUMNS - 1] = count;
        activity
    }

    /// `running.jsonl` under `running.meta.json`, read at [`NOW`].
    fn running_node() -> SubagentNode {
        SubagentNode {
            agent_id: "a20".to_owned(),
            parent_agent_id: None,
            agent_type: Some("Explore".to_owned()),
            description: Some("Map the crate".to_owned()),
            started_at: Some(at("2026-09-27T10:00:01Z")),
            finished_at: None,
            state: AgentState::Running,
            end_status: None,
            open_call: Some(OpenCall {
                id: "toolu_s1".to_owned(),
                name: "Bash".to_owned(),
                command: Some("cargo test".to_owned()),
                started: at("2026-09-27T10:00:10Z"),
            }),
            last_text: None,
            failure_run: 0,
            activity: last_minute(1),
            children: Vec::new(),
        }
    }

    /// The first five lines of `finished.jsonl` under `finished.meta.json`,
    /// read at [`NOW`].
    fn finished_node() -> SubagentNode {
        SubagentNode {
            agent_id: "a21".to_owned(),
            parent_agent_id: None,
            agent_type: Some("reviewer".to_owned()),
            description: Some("Review the diff".to_owned()),
            started_at: Some(at("2026-09-27T10:00:03Z")),
            finished_at: Some(at("2026-09-27T10:00:09Z")),
            state: AgentState::Finished,
            end_status: None,
            open_call: None,
            last_text: Some("Found it.".to_owned()),
            failure_run: 0,
            activity: last_minute(1),
            children: Vec::new(),
        }
    }

    /// The one node of `nodes`; a test whose nodes are not one fails.
    fn only(nodes: &[SubagentNode]) -> &SubagentNode {
        assert_eq!(nodes.len(), 1, "one node expected: {nodes:?}");
        nodes.first().expect("one node")
    }

    fn ids(nodes: &[SubagentNode]) -> Vec<&str> {
        nodes.iter().map(|node| node.agent_id.as_str()).collect()
    }

    #[test]
    fn a_sidecar_with_no_end_turn_reply_is_a_running_subagent() {
        let scratch = conversation("running", &[("a20", RUNNING, Some(RUNNING_META))]);

        assert_eq!(tree(&scratch, None, NOW), [running_node()]);
    }

    #[test]
    fn an_end_turn_reply_followed_only_by_attachments_is_a_finished_subagent() {
        let transcript = lines(FINISHED, 0, 5);
        let scratch = conversation("finished", &[("a21", &transcript, Some(FINISHED_META))]);

        assert_eq!(tree(&scratch, None, NOW), [finished_node()]);
    }

    #[test]
    fn growth_after_a_finished_reply_makes_the_subagent_run_again() {
        let transcript = lines(FINISHED, 0, 5);
        let scratch = conversation("resumed", &[("a21", &transcript, Some(FINISHED_META))]);
        let mut reader = scratch.reader(None);
        assert_eq!(reader.read(at(NOW)).subagents, [finished_node()]);

        scratch.add("agent-a21.jsonl", &lines(FINISHED, 5, 7));
        let resumed = reader.read(at("2026-09-27T10:06:00Z")).subagents;

        let node = only(&resumed);
        assert_eq!(node.state, AgentState::Running);
        assert_eq!(node.finished_at, None);
        assert_eq!(node.end_status, None);
        assert_eq!(
            node.open_call.as_ref().map(|call| call.id.as_str()),
            Some("toolu_s3")
        );
    }

    #[test]
    fn a_killed_notification_newer_than_the_last_line_ends_the_subagent_as_failed() {
        let endings = [
            ("failed", AgentState::Failed),
            ("killed", AgentState::Failed),
            ("stopped", AgentState::Failed),
            ("completed", AgentState::Finished),
        ];
        for (status, state) in endings {
            let scratch = conversation(
                &format!("ended-{status}"),
                &[("a20", RUNNING, Some(RUNNING_META))],
            );
            scratch.append(&notification("a20", status, "2026-09-27T10:00:30.000Z"));

            let expected = SubagentNode {
                state,
                end_status: Some(status.to_owned()),
                finished_at: Some(at("2026-09-27T10:00:30Z")),
                ..running_node()
            };
            assert_eq!(tree(&scratch, None, NOW), [expected], "status {status}");
        }
    }

    #[test]
    fn a_stopped_by_user_sidecar_ends_the_subagent() {
        let sidecar =
            RUNNING_META.replace("\"spawnDepth\"", "\"stoppedByUser\":true,\"spawnDepth\"");
        let scratch = conversation("stopped", &[("a20", RUNNING, Some(&sidecar))]);

        let expected = SubagentNode {
            state: AgentState::Failed,
            end_status: Some("stoppedByUser".to_owned()),
            finished_at: Some(at("2026-09-27T10:00:10Z")),
            ..running_node()
        };
        assert_eq!(tree(&scratch, None, NOW), [expected]);
    }

    #[test]
    fn a_notification_older_than_the_transcripts_last_line_is_ignored() {
        let scratch = conversation("stale-notice", &[("a20", RUNNING, Some(RUNNING_META))]);
        scratch.append(&notification(
            "a20",
            "completed",
            "2026-09-27T10:00:05.000Z",
        ));

        assert_eq!(tree(&scratch, None, NOW), [running_node()]);
    }

    #[test]
    fn a_transcript_written_before_the_process_started_is_finished() {
        let scratch = conversation("old-process", &[("a20", RUNNING, Some(RUNNING_META))]);

        let nodes = tree(
            &scratch,
            Some("2026-09-27T10:30:00Z"),
            "2026-09-27T10:31:00Z",
        );

        let node = only(&nodes);
        assert_eq!(node.state, AgentState::Finished);
        assert_eq!(node.finished_at, Some(at("2026-09-27T10:00:10Z")));
        assert_eq!(node.end_status, None);
    }

    #[test]
    fn a_nested_subagent_hangs_under_its_parent_by_parent_agent_id_and_takes_its_notification_from_the_launcher_file()
     {
        let scratch = conversation(
            "nested",
            &[
                ("a20", NESTED_PARENT, Some(RUNNING_META)),
                ("a30", NESTED_CHILD, Some(NESTED_CHILD_META)),
                ("a40", NESTED_GRANDCHILD, Some(NESTED_GRANDCHILD_META)),
            ],
        );

        let nodes = tree(&scratch, None, "2026-09-27T10:03:00Z");

        let parent = only(&nodes);
        assert_eq!(parent.state, AgentState::Running);
        let child = only(&parent.children);
        assert_eq!(child.agent_id, "a30");
        assert_eq!(child.parent_agent_id.as_deref(), Some("a20"));
        assert_eq!(child.state, AgentState::Finished);
        assert_eq!(child.end_status.as_deref(), Some("completed"));
        assert_eq!(child.finished_at, Some(at("2026-09-27T10:02:00Z")));
        let grandchild = only(&child.children);
        assert_eq!(grandchild.agent_id, "a40");
        assert_eq!(grandchild.parent_agent_id.as_deref(), Some("a30"));
        assert_eq!(grandchild.state, AgentState::Finished);
        assert_eq!(grandchild.end_status, None);
        assert_eq!(grandchild.finished_at, Some(at("2026-09-27T10:00:30Z")));
        assert_eq!(grandchild.last_text.as_deref(), Some("It is short."));
        assert_eq!(grandchild.children, []);
    }

    #[test]
    fn a_sidecar_whose_launch_the_main_transcript_never_made_is_left_out() {
        let unlaunched = RUNNING_META.replace("toolu_20", "toolu_99");
        let finished = lines(FINISHED, 0, 5);
        let scratch = conversation(
            "unlaunched",
            &[
                ("a90", RUNNING, Some(&unlaunched)),
                ("a91", RUNNING, Some(NO_LAUNCH_META)),
                ("a92", &finished, Some(NO_LAUNCH_META)),
            ],
        );

        let after_start = tree(&scratch, Some("2026-09-27T10:00:00Z"), NOW);
        let no_start = tree(&scratch, None, NOW);

        assert_eq!(
            ids(&after_start),
            ["a91"],
            "a sidecar with no launch is listed only while it runs in this process"
        );
        assert_eq!(ids(&no_start), [] as [&str; 0]);
    }

    #[test]
    fn a_transcript_with_no_sidecar_takes_its_description_from_the_first_prompt() {
        let transcript = RUNNING.replace(
            "Map the crate\\nStart at lib.rs",
            "Summarise the logs\\nThen report",
        );
        let scratch = conversation("no-sidecar", &[("a22", &transcript, None)]);

        let expected = SubagentNode {
            agent_id: "a22".to_owned(),
            agent_type: Some("general-purpose".to_owned()),
            description: Some("Summarise the logs".to_owned()),
            ..running_node()
        };
        assert_eq!(tree(&scratch, None, NOW), [expected]);
    }

    #[test]
    fn garbage_lines_and_a_half_written_sidecar_are_skipped() {
        let transcript = format!(
            "not json at all\n{}{{\"type\":\n{}",
            lines(RUNNING, 0, 1),
            lines(RUNNING, 1, 2)
        );
        let finished = lines(FINISHED, 0, 5);
        let scratch = conversation(
            "garbage",
            &[
                ("a20", &transcript, Some(RUNNING_META)),
                ("a21", &finished, Some(r#"{"agentType":"revi"#)),
            ],
        );

        assert_eq!(tree(&scratch, None, NOW), [running_node()]);
    }

    #[test]
    fn a_missing_subagents_folder_means_no_nodes() {
        let scratch = Scratch::new("no-folder");
        scratch.write(MAIN);

        assert_eq!(tree(&scratch, None, NOW), []);
    }

    #[test]
    fn a_launch_before_the_main_transcripts_tail_still_joins_its_sidecar() {
        let mut main = MAIN.to_owned();
        let filler = "x".repeat(600);
        let padding = format!(
            r#"{{"type":"assistant","timestamp":"2026-09-27T10:00:06.000Z","message":{{"role":"assistant","content":[{{"type":"text","text":"{filler}"}}]}}}}"#
        );
        while main.len() <= 1_100_000 {
            main.push_str(&padding);
            main.push('\n');
        }
        let scratch = conversation("long-main", &[("a20", RUNNING, Some(RUNNING_META))]);
        scratch.write(&main);

        assert_eq!(tree(&scratch, None, NOW), [running_node()]);
    }

    #[test]
    fn a_background_forked_skill_sidecar_joins_by_its_launch() {
        let finished = lines(FINISHED, 0, 5);
        let sidecar = r#"{"agentType":"general-purpose","name":"review","spawnDepth":1,"requestShape":"background","requestNonInteractive":true}"#;
        let scratch = conversation("forked-skill", &[("a60", &finished, Some(sidecar))]);
        scratch.append(concat!(
            r#"{"type":"assistant","isSidechain":false,"timestamp":"2026-09-27T10:00:06.000Z","message":{"id":"msg_600","role":"assistant","model":"fake-model","content":[{"type":"tool_use","id":"toolu_60","name":"Skill","input":{"skill":"review"}}],"stop_reason":"tool_use","usage":{"input_tokens":1,"output_tokens":1}}}"#,
            "\n",
            r#"{"type":"user","isSidechain":false,"timestamp":"2026-09-27T10:00:07.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_60","content":"Skill forked","is_error":false}]},"toolUseResult":{"status":"forked","background":true,"agentId":"a60"}}"#,
            "\n"
        ));

        let nodes = tree(&scratch, None, NOW);

        let node = only(&nodes);
        assert_eq!(node.agent_id, "a60");
        assert_eq!(node.state, AgentState::Finished);
    }

    #[test]
    fn a_removed_queue_operation_does_not_move_a_subagents_finish_time() {
        let scratch = conversation("removed-op", &[("a20", RUNNING, Some(RUNNING_META))]);
        scratch.append(&queue_operation(
            "enqueue",
            "a20",
            "completed",
            "2026-09-27T10:00:30.000Z",
        ));
        scratch.append(&queue_operation(
            "remove",
            "a20",
            "completed",
            "2026-09-27T10:00:40.000Z",
        ));

        let node = only(&tree(&scratch, None, NOW)).clone();

        assert_eq!(node.state, AgentState::Finished);
        assert_eq!(node.finished_at, Some(at("2026-09-27T10:00:30Z")));
    }
}
