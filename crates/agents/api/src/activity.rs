//! What an agent's transcript says a session is doing, for the Dashboard:
//! the source a plugin's [`ActivityReader`] follows and the
//! [`ActivitySnapshot`] it hands back.
//!
//! The types carry figures, not verdicts: the daemon decides from them
//! whether a session is stuck.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use protocol::AgentState;

use crate::ResolvedRows;

/// The number of one-minute columns in [`ActivitySnapshot::minute_counts`].
pub const MINUTE_COLUMNS: usize = 10;

/// The transcript the daemon asks a plugin to read: one session's
/// conversation.
#[derive(Clone)]
pub struct ActivitySource {
    /// The folder the session started in (its first member's worktree), the
    /// one the agent files the conversation under.
    pub folder: PathBuf,
    /// The agent's conversation id for the session.
    pub conversation_id: String,
    /// The session's resolved environment rows, which may move the agent's
    /// home.
    pub rows: ResolvedRows,
    /// When the session's current agent process started: a tool call stamped
    /// before it is not open. `None` keeps every call.
    pub process_started_at: Option<DateTime<Utc>>,
}

/// A plugin's incremental reader of one session's transcript. It keeps its
/// place between calls, so each call reads only what the file gained.
pub trait ActivityReader: Send {
    /// The session's activity as of `now`, after reading what the transcript
    /// gained since the last call. A missing or unreadable transcript leaves
    /// the last reading as it was.
    fn read(&mut self, now: DateTime<Utc>) -> ActivitySnapshot;
}

/// Everything a transcript says about a session, as of one read: the whole
/// current state, never a change since the last read.
///
/// Tokens and cost cover the session's main transcript and the subagents in
/// [`ActivitySnapshot::subagents`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ActivitySnapshot {
    /// The tool calls with no result yet, oldest first, leaving out those
    /// stamped before [`ActivitySource::process_started_at`].
    pub open_calls: Vec<OpenCall>,
    /// The ids of the subagent tool calls the transcript holds, in file
    /// order: the key a subagent's own files name their launch by.
    pub agent_call_ids: Vec<String>,
    /// The background tasks still running, in launch order.
    pub background: Vec<BackgroundTask>,
    /// The background shells a task notification ended, in the order the
    /// notifications came; a task the agent launched as a subagent is never
    /// one.
    pub ended_background: Vec<EndedTask>,
    /// The subagents the conversation launched, running or finished: the
    /// first layer, each holding the ones it launched.
    pub subagents: Vec<SubagentNode>,
    /// How full the context of the newest main-thread reply is.
    pub context: Option<ContextReading>,
    /// The model of the newest main-thread reply.
    pub model: Option<String>,
    /// The tokens every reply read: input, cache writes and cache reads.
    pub tokens_in: u64,
    /// The tokens every reply wrote.
    pub tokens_out: u64,
    /// What every reply would cost at the model's API list prices, in US
    /// dollars; a reply of a model with no price adds nothing.
    pub cost_usd: f64,
    /// Tool calls started in each of the last ten minutes, oldest first; the
    /// last column is the minute ending now.
    pub minute_counts: [u16; MINUTE_COLUMNS],
    /// How many tool results in a row failed; a denied or interrupted call
    /// neither adds to the run nor ends it.
    pub failure_run: u32,
    /// The first prompt the user gave the conversation, a slash command as its
    /// command line.
    pub first_prompt: Option<String>,
    /// The newest time any record carries.
    pub last_record_at: Option<DateTime<Utc>>,
    /// The last text the agent wrote in its newest reply that has any.
    pub last_assistant_text: Option<String>,
}

/// A tool call with no result yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenCall {
    /// The call's id.
    pub id: String,
    /// The tool it calls.
    pub name: String,
    /// The command line it runs, for a tool that takes one.
    pub command: Option<String>,
    /// When the reply holding it was written.
    pub started: DateTime<Utc>,
}

/// A task the agent left running in the background.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackgroundTask {
    /// The task's id.
    pub id: String,
    /// The id of the tool call that launched it, when the transcript names it.
    pub tool_use_id: Option<String>,
    /// When the call that launched it started, else when its launch was
    /// recorded.
    pub started: Option<DateTime<Utc>>,
    /// The command line it runs, for a tool that takes one.
    pub command: Option<String>,
}

/// A background task a task notification ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EndedTask {
    /// The task as it was launched.
    pub task: BackgroundTask,
    /// `Finished` for a `completed` status, `Failed` for `failed`, `killed`
    /// or `stopped`, `Unknown` for any other.
    pub state: AgentState,
    /// The notification's own status word.
    pub status: Option<String>,
    /// The notification's summary line.
    pub summary: Option<String>,
    /// When the notification was recorded.
    pub ended_at: Option<DateTime<Utc>>,
}

/// One subagent of the conversation and the ones it launched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubagentNode {
    /// The subagent's id, which names its transcript file.
    pub agent_id: String,
    /// The id of the subagent that launched it; `None` for one the main
    /// agent launched.
    pub parent_agent_id: Option<String>,
    /// Its agent type (`Explore`, `general-purpose`, …).
    pub agent_type: Option<String>,
    /// What it was asked to do: its launch's description, else its first
    /// prompt's first line.
    pub description: Option<String>,
    /// The time of the first record read from its transcript.
    pub started_at: Option<DateTime<Utc>>,
    /// When it ended; `None` while it runs.
    pub finished_at: Option<DateTime<Utc>>,
    /// `Running`, `Finished`, or `Failed` for a run that was killed, stopped
    /// or failed.
    pub state: AgentState,
    /// What ended it, as recorded: a task notification's status word or
    /// `stoppedByUser`; `None` while it runs or when it ended on its own
    /// final reply.
    pub end_status: Option<String>,
    /// Its newest tool call with no result yet.
    pub open_call: Option<OpenCall>,
    /// The last text it wrote in its newest reply that has any.
    pub last_text: Option<String>,
    /// How many of its tool results in a row failed.
    pub failure_run: u32,
    /// Its tool calls started in each of the last ten minutes, oldest first.
    pub activity: [u16; MINUTE_COLUMNS],
    /// The subagents it launched, oldest first.
    pub children: Vec<SubagentNode>,
}

/// How full a reply's context window was.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextReading {
    /// The model that replied.
    pub model: String,
    /// The tokens its context held.
    pub tokens: u64,
    /// The share of the model's window those tokens fill, as a whole percent
    /// capped at 100; `None` for a model whose window is unknown.
    pub percent: Option<u8>,
}
