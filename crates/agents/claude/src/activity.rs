//! The Claude plugin's transcript activity reader: it follows one
//! conversation's main transcript, parsing only the lines appended since the
//! last read, and reports what the session is doing for the Dashboard, with
//! the subagent tree [`crate::subagents`] reads beside it.
//!
//! A first read of a file longer than [`TAIL_BYTES`] starts at the first
//! whole line of its last [`TAIL_BYTES`], and the first prompt is then looked
//! for in the file's first [`HEAD_BYTES`]. A line still being written (no
//! `\n` yet) is left for the next read. A file whose length and creation time
//! are unchanged is not read; one that shrank or was replaced is read again
//! from scratch.

use std::collections::{HashMap, VecDeque};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use agent_api::{
    ActivityReader, ActivitySnapshot, ActivitySource, BackgroundTask, ContextReading, EndedTask,
    MINUTE_COLUMNS, OpenCall,
};
use chrono::{DateTime, Utc};
use protocol::AgentState;
use serde_json::Value;
use tracing::debug;

use crate::pricing::{self, Usage};
use crate::records::{self, LaunchedTask, NOTIFICATION_TAG, TaskNotification};
use crate::subagents::SubagentReader;
use crate::transcripts;

/// How much of a file's end its first read takes.
const TAIL_BYTES: u64 = 1024 * 1024;
/// How much of a file's start is searched for the first prompt when the first
/// read began past it.
const HEAD_BYTES: u64 = 256 * 1024;
/// The longest first prompt kept, in bytes.
const PROMPT_BYTES: usize = 2048;
/// The longest last assistant text kept, in bytes.
const ASSISTANT_TEXT_BYTES: usize = 8 * 1024;
/// How many recent message ids and tool-call ids are remembered, so a
/// streamed message is counted once and a call is counted once.
const RECENT_IDS: usize = 256;
/// One minute, in milliseconds.
const MINUTE_MS: i64 = 60_000;
/// The tool that starts a subagent.
const AGENT_TOOL: &str = "Agent";
/// The model Claude Code writes on a reply it made up itself (an API error).
const SYNTHETIC_MODEL: &str = "<synthetic>";
/// The slash command that starts the conversation over; it is no prompt.
const CLEAR_COMMAND: &str = "/clear";
/// The prefixes of a local command's own output, which Claude Code records as
/// a user message although the user typed none of it.
const LOCAL_COMMAND_OUTPUT: [&str; 3] = [
    "<local-command-stdout>",
    "<local-command-stderr>",
    "<local-command-caveat>",
];

/// A file's identity as of a read: its length and creation time.
type Identity = (u64, Option<SystemTime>);

/// The reader of the main transcript `source` names; `None` when the
/// conversation id is no plain file name or the session has no Claude home.
pub(crate) fn reader(source: &ActivitySource) -> Option<Box<dyn ActivityReader>> {
    let path = transcripts::transcript_path(&source.folder, &source.conversation_id, &source.rows)?;
    Some(Box::new(TranscriptReader::new(
        path,
        source.process_started_at,
    )))
}

/// One conversation's reader: its main transcript, what it has read so far
/// and its subagents.
pub(crate) struct TranscriptReader {
    main: Transcript,
    process_started_at: Option<DateTime<Utc>>,
    /// Whether the file's absence has been logged since it was last read.
    missing_logged: bool,
    subagents: SubagentReader,
}

impl ActivityReader for TranscriptReader {
    fn read(&mut self, now: DateTime<Utc>) -> ActivitySnapshot {
        if let Err(error) = self.refresh() {
            self.log_unreadable(&error);
        }
        let mut snapshot = self.main.state.snapshot(now, self.process_started_at);
        let tree = self
            .subagents
            .read(&self.main.state, now, self.process_started_at);
        snapshot.subagents = tree.nodes;
        snapshot.tokens_in = snapshot.tokens_in.saturating_add(tree.tokens_in);
        snapshot.tokens_out = snapshot.tokens_out.saturating_add(tree.tokens_out);
        snapshot.cost_usd += tree.cost_usd;
        snapshot
    }
}

impl TranscriptReader {
    pub(crate) fn new(path: PathBuf, process_started_at: Option<DateTime<Utc>>) -> Self {
        Self {
            subagents: SubagentReader::new(transcripts::subagents_dir(&path)),
            main: Transcript::new(path),
            process_started_at,
            missing_logged: false,
        }
    }

    fn refresh(&mut self) -> io::Result<()> {
        self.main.refresh()?;
        self.missing_logged = false;
        Ok(())
    }

    /// Logs a transcript that cannot be read: a missing one once until it
    /// appears, any other failure each time.
    fn log_unreadable(&mut self, error: &io::Error) {
        let path = self.main.path();
        if error.kind() != io::ErrorKind::NotFound {
            debug!(path = %path.display(), %error, "activity: the transcript could not be read");
        } else if !self.missing_logged {
            debug!(path = %path.display(), "activity: no transcript yet");
            self.missing_logged = true;
        }
    }
}

/// One transcript file and what has been read of it.
pub(crate) struct Transcript {
    path: PathBuf,
    pub(crate) state: State,
}

impl Transcript {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path,
            state: State::default(),
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Reads what the file gained, starting over when it shrank or was
    /// replaced since the last read and reading nothing when its length and
    /// creation time are unchanged. Returns the file's metadata.
    pub(crate) fn refresh(&mut self) -> io::Result<fs::Metadata> {
        let meta = fs::metadata(&self.path)?;
        let identity = (meta.len(), meta.created().ok());
        if self.state.read_once {
            if identity == self.state.identity {
                return Ok(meta);
            }
            if identity.0 < self.state.identity.0 || identity.1 != self.state.identity.1 {
                self.state = State::default();
            }
        }
        self.state.read_new(&self.path, identity)?;
        Ok(meta)
    }
}

/// The newest task notification a transcript holds for one task.
#[derive(Clone, Debug)]
pub(crate) struct Notice {
    /// Its status word.
    pub(crate) status: Option<String>,
    /// When the record holding it was written.
    pub(crate) at: Option<DateTime<Utc>>,
}

/// What a subagent tool call asked for.
#[derive(Clone, Debug)]
pub(crate) struct AgentCall {
    pub(crate) description: Option<String>,
    pub(crate) agent_type: Option<String>,
}

/// What a reader has gathered from the lines it read.
#[derive(Default)]
pub(crate) struct State {
    /// The byte just past the last whole line read.
    offset: u64,
    read_once: bool,
    identity: Identity,
    /// Whether the reads began at the file's start, so the first prompt is
    /// the first one they meet.
    prompt_from_lines: bool,
    open: HashMap<String, OpenCall>,
    /// The recent tool-call ids already counted.
    counted_calls: VecDeque<String>,
    pub(crate) agent_call_ids: Vec<String>,
    /// What each subagent tool call asked for, by call id.
    pub(crate) agent_calls: HashMap<String, AgentCall>,
    /// The id of the call that launched each background subagent, by agent
    /// id.
    pub(crate) agent_launches: HashMap<String, Option<String>>,
    background: Vec<BackgroundTask>,
    /// The background shells a notification ended, in that order.
    ended: Vec<EndedTask>,
    /// The newest notification for each task id.
    pub(crate) notices: HashMap<String, Notice>,
    /// When each call of the last ten minutes started.
    calls: Vec<DateTime<Utc>>,
    /// The largest usage seen of each recent message id.
    usage_seen: VecDeque<(String, Usage)>,
    pub(crate) tokens_in: u64,
    pub(crate) tokens_out: u64,
    pub(crate) cost_usd: f64,
    /// The newest main-thread reply's model and context tokens.
    context: Option<(String, u64)>,
    pub(crate) failure_run: u32,
    pub(crate) first_prompt: Option<String>,
    pub(crate) first_record_at: Option<DateTime<Utc>>,
    pub(crate) last_record_at: Option<DateTime<Utc>>,
    pub(crate) last_assistant_text: Option<String>,
    /// Whether the newest record other than an attachment is a reply that
    /// ended a run: `end_turn` with no tool call.
    pub(crate) run_ended: bool,
}

impl State {
    /// Reads the whole lines past [`Self::offset`] (past the cut
    /// [`TAIL_BYTES`] before the end, on the first read of a longer file),
    /// merging each one that parses. `identity` is the file's as of now.
    fn read_new(&mut self, path: &Path, identity: Identity) -> io::Result<()> {
        let mut file = File::open(path)?;
        if !self.read_once {
            if identity.0 > TAIL_BYTES {
                self.offset = next_line_start(&mut file, identity.0 - TAIL_BYTES - 1)?;
                self.first_prompt = first_prompt_in_head(&mut file)?;
            } else {
                self.prompt_from_lines = true;
            }
            self.read_once = true;
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let mut reader = BufReader::new(file);
        let mut line = Vec::new();
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line)? == 0 || line.last() != Some(&b'\n') {
                break;
            }
            self.offset += u64::try_from(line.len()).unwrap_or(u64::MAX);
            if let Ok(record) = serde_json::from_slice::<Value>(&line) {
                let notifies = line
                    .windows(NOTIFICATION_TAG.len())
                    .any(|window| window == NOTIFICATION_TAG.as_bytes());
                self.merge(&record, notifies);
            }
        }
        self.identity = identity;
        Ok(())
    }

    /// Adds one record; `notifies` says whether its line holds a task
    /// notification.
    fn merge(&mut self, record: &Value, notifies: bool) {
        let at = timestamp(record);
        if let Some(at) = at {
            self.last_record_at = Some(self.last_record_at.map_or(at, |last| last.max(at)));
            self.first_record_at = Some(self.first_record_at.map_or(at, |first| first.min(at)));
        }
        self.track_run_end(record);
        match record["type"].as_str() {
            Some("assistant") => self.merge_assistant(record),
            Some("user") => self.merge_user(record, notifies, at),
            Some("attachment") if notifies && record["attachment"]["type"] == "queued_command" => {
                if let Some(text) = record["attachment"]["prompt"].as_str() {
                    self.finish_tasks(text, at);
                }
            }
            Some("queue-operation") if notifies && record["operation"] == "enqueue" => {
                if let Some(text) = record["content"].as_str() {
                    self.finish_tasks(text, at);
                }
            }
            _ => {}
        }
    }

    /// Notes whether `record` ends a run: an assistant reply with
    /// `end_turn` and no tool call does, an attachment changes nothing, and
    /// any other record starts the run again.
    fn track_run_end(&mut self, record: &Value) {
        match record["type"].as_str() {
            Some("attachment") => {}
            Some("assistant") => {
                let message = &record["message"];
                let calls_a_tool = message["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|block| block["type"] == "tool_use");
                self.run_ended = message["stop_reason"] == "end_turn" && !calls_a_tool;
            }
            _ => self.run_ended = false,
        }
    }

    /// Adds an assistant record: its calls, its usage and its last text,
    /// unless it is Claude Code's API error message.
    fn merge_assistant(&mut self, record: &Value) {
        self.merge_calls(record);
        self.merge_usage(record);
        if record["isApiErrorMessage"] != true
            && let Some(text) = last_text_block(&record["message"]["content"])
        {
            self.last_assistant_text = Some(cut(text, ASSISTANT_TEXT_BYTES));
        }
    }

    /// Adds the `tool_use` blocks of an assistant record that carries a time
    /// as open calls, each id counted once.
    fn merge_calls(&mut self, record: &Value) {
        let Some(started) = timestamp(record) else {
            return;
        };
        let blocks = record["message"]["content"]
            .as_array()
            .into_iter()
            .flatten();
        for block in blocks.filter(|block| block["type"] == "tool_use") {
            let Some(id) = block["id"].as_str().filter(|id| !id.is_empty()) else {
                continue;
            };
            if self.counted_calls.iter().any(|seen| seen == id) {
                continue;
            }
            remember(&mut self.counted_calls, id.to_owned());
            let name = block["name"].as_str().unwrap_or_default().to_owned();
            if name == AGENT_TOOL {
                self.agent_call_ids.push(id.to_owned());
                let input = &block["input"];
                let text = |field: &str| input[field].as_str().map(str::to_owned);
                let call = AgentCall {
                    description: text("description"),
                    agent_type: text("subagent_type"),
                };
                self.agent_calls.insert(id.to_owned(), call);
            }
            self.calls.push(started);
            let command = block["input"]["command"].as_str().map(str::to_owned);
            let call = OpenCall {
                id: id.to_owned(),
                name,
                command,
                started,
            };
            self.open.insert(id.to_owned(), call);
        }
    }

    /// Adds an assistant record's usage to the tokens and cost, a message id
    /// already seen adding only what its largest usage grew by; a main-thread
    /// reply (no sidechain, no made-up model) also becomes the context
    /// reading.
    fn merge_usage(&mut self, record: &Value) {
        let message = &record["message"];
        let Some(usage) = usage_of(&message["usage"]) else {
            return;
        };
        let model = message["model"].as_str();
        if let Some(model) = model.filter(|model| *model != SYNTHETIC_MODEL)
            && record["isSidechain"] != true
        {
            self.context = Some((model.to_owned(), context_tokens(&usage)));
        }
        let (before, after) = self.grow_usage(message["id"].as_str(), usage);
        self.tokens_in = self
            .tokens_in
            .saturating_add(after.tokens_in().saturating_sub(before.tokens_in()));
        self.tokens_out = self
            .tokens_out
            .saturating_add(after.output.saturating_sub(before.output));
        if let Some(model) = model
            && let (Some(after), Some(before)) = (
                pricing::cost_usd(model, &after),
                pricing::cost_usd(model, &before),
            )
        {
            self.cost_usd += after - before;
        }
    }

    /// The usage message `id` had counted before this record and has now:
    /// the largest of each field seen. A record with no id counts whole.
    fn grow_usage(&mut self, id: Option<&str>, usage: Usage) -> (Usage, Usage) {
        let Some(id) = id else {
            return (Usage::default(), usage);
        };
        if let Some((_, seen)) = self.usage_seen.iter_mut().find(|(seen, _)| seen == id) {
            let before = *seen;
            *seen = before.largest(usage);
            return (before, *seen);
        }
        self.usage_seen.push_back((id.to_owned(), usage));
        while self.usage_seen.len() > RECENT_IDS {
            self.usage_seen.pop_front();
        }
        (Usage::default(), usage)
    }

    /// Adds a `user` record: the first prompt, when the reads began at the
    /// file's start and none was found yet; the results it holds; the
    /// background task it launches; and, when `notifies`, the tasks its text
    /// names as finished. `at` is the record's time.
    fn merge_user(&mut self, record: &Value, notifies: bool, at: Option<DateTime<Utc>>) {
        if self.prompt_from_lines && self.first_prompt.is_none() {
            self.first_prompt = prompt_of(record);
        }
        let content = &record["message"]["content"];
        let mut launching_call = None;
        let blocks = content.as_array().into_iter().flatten();
        for block in blocks.filter(|block| block["type"] == "tool_result") {
            if let Some(id) = block["tool_use_id"].as_str() {
                let call = self.open.remove(id);
                launching_call.get_or_insert((id, call));
            }
            self.count_result(block);
        }
        if let Some(task) = records::launched_task(&record["toolUseResult"]) {
            self.launch(task, launching_call, at);
        }
        if notifies && let Some(text) = content.as_str() {
            self.finish_tasks(text, at);
        }
    }

    /// Adds a background task the result recorded at `at` launched, with the
    /// id and open call of the tool call it answers; a background subagent
    /// also keeps its launching call's id.
    fn launch(
        &mut self,
        task: LaunchedTask<'_>,
        call: Option<(&str, Option<OpenCall>)>,
        at: Option<DateTime<Utc>>,
    ) {
        let (tool_use_id, open) =
            call.map_or((None, None), |(id, open)| (Some(id.to_owned()), open));
        if task.is_agent {
            self.agent_launches
                .insert(task.id.to_owned(), tool_use_id.clone());
        }
        if self.background.iter().any(|running| running.id == task.id) {
            return;
        }
        self.background.push(BackgroundTask {
            id: task.id.to_owned(),
            tool_use_id,
            started: open.as_ref().map(|call| call.started).or(at),
            command: open.and_then(|call| call.command),
        });
    }

    /// Counts one `tool_result` towards the run of failures: a failed result
    /// extends it, a successful one ends it, and a denied or interrupted one
    /// leaves it as it was.
    fn count_result(&mut self, block: &Value) {
        if records::result_text(block).is_some_and(records::is_denied_or_interrupted) {
            return;
        }
        if block["is_error"] == true {
            self.failure_run = self.failure_run.saturating_add(1);
        } else {
            self.failure_run = 0;
        }
    }

    /// Takes each task notification of `text`, recorded at `at`: it becomes
    /// its task's newest notice, and the running background task it names
    /// ends, a shell moving to the ended list.
    fn finish_tasks(&mut self, text: &str, at: Option<DateTime<Utc>>) {
        for notification in records::task_notifications(text) {
            self.note(&notification, at);
            let Some(index) = self
                .background
                .iter()
                .position(|task| task.id == notification.task_id)
            else {
                continue;
            };
            let mut task = self.background.remove(index);
            if self.agent_launches.contains_key(&task.id) {
                continue;
            }
            if task.tool_use_id.is_none() {
                task.tool_use_id = notification.tool_use_id.map(str::to_owned);
            }
            self.ended.push(EndedTask {
                task,
                state: ended_state(notification.status).unwrap_or(AgentState::Unknown),
                status: notification.status.map(str::to_owned),
                summary: notification.summary.map(str::to_owned),
                ended_at: at,
            });
        }
    }

    /// Keeps `notification` as its task's notice unless a newer one is kept.
    fn note(&mut self, notification: &TaskNotification<'_>, at: Option<DateTime<Utc>>) {
        let newer = self
            .notices
            .get(notification.task_id)
            .is_none_or(|kept| kept.at <= at);
        if newer {
            let notice = Notice {
                status: notification.status.map(str::to_owned),
                at,
            };
            self.notices.insert(notification.task_id.to_owned(), notice);
        }
    }

    /// Whether the reads began past the file's start, so launches older than
    /// the first read's tail are unknown.
    pub(crate) fn began_mid_file(&self) -> bool {
        self.read_once && !self.prompt_from_lines
    }

    /// The newest tool call with no result yet.
    pub(crate) fn newest_open_call(&self) -> Option<OpenCall> {
        self.open
            .values()
            .max_by(|left, right| (left.started, &left.id).cmp(&(right.started, &right.id)))
            .cloned()
    }

    /// Tool calls started in each of the ten minutes up to `now`, oldest
    /// first, dropping the call times ten minutes or more before it; a call
    /// stamped after `now` counts in the last column.
    pub(crate) fn minute_counts(&mut self, now: DateTime<Utc>) -> [u16; MINUTE_COLUMNS] {
        let window = i64::try_from(MINUTE_COLUMNS)
            .unwrap_or(i64::MAX)
            .saturating_mul(MINUTE_MS);
        let age = |at: &DateTime<Utc>| now.signed_duration_since(*at).num_milliseconds();
        self.calls.retain(|at| age(at) < window);
        let mut minute_counts = [0_u16; MINUTE_COLUMNS];
        for at in &self.calls {
            let minutes_ago = usize::try_from(age(at).max(0) / MINUTE_MS).unwrap_or(0);
            if let Some(count) = (MINUTE_COLUMNS - 1)
                .checked_sub(minutes_ago)
                .and_then(|column| minute_counts.get_mut(column))
            {
                *count = count.saturating_add(1);
            }
        }
        minute_counts
    }

    /// The state as of `now`, with the main transcript's figures only.
    fn snapshot(
        &mut self,
        now: DateTime<Utc>,
        process_started_at: Option<DateTime<Utc>>,
    ) -> ActivitySnapshot {
        let minute_counts = self.minute_counts(now);
        let mut open_calls: Vec<OpenCall> = self
            .open
            .values()
            .filter(|call| process_started_at.is_none_or(|start| call.started >= start))
            .cloned()
            .collect();
        open_calls.sort_by(|left, right| (left.started, &left.id).cmp(&(right.started, &right.id)));
        ActivitySnapshot {
            open_calls,
            agent_call_ids: self.agent_call_ids.clone(),
            background: self.background.clone(),
            ended_background: self.ended.clone(),
            subagents: Vec::new(),
            context: self.context.as_ref().map(|(model, tokens)| ContextReading {
                model: model.clone(),
                tokens: *tokens,
                percent: pricing::context_window(model)
                    .map(|window| pricing::context_percent(*tokens, window)),
            }),
            model: self.context.as_ref().map(|(model, _)| model.clone()),
            tokens_in: self.tokens_in,
            tokens_out: self.tokens_out,
            cost_usd: self.cost_usd,
            minute_counts,
            failure_run: self.failure_run,
            first_prompt: self.first_prompt.clone(),
            last_record_at: self.last_record_at,
            last_assistant_text: self.last_assistant_text.clone(),
        }
    }
}

/// The state a task notification's `status` ends its task in: `Finished`
/// for `completed`; `Failed` for `failed`, `killed` and `stopped`; `None`
/// for any other status, which names no known end.
pub(crate) fn ended_state(status: Option<&str>) -> Option<AgentState> {
    match status? {
        "completed" => Some(AgentState::Finished),
        "failed" | "killed" | "stopped" => Some(AgentState::Failed),
        _ => None,
    }
}

/// Adds `id` to the recent ids, forgetting the oldest past [`RECENT_IDS`].
fn remember(recent: &mut VecDeque<String>, id: String) {
    recent.push_back(id);
    while recent.len() > RECENT_IDS {
        recent.pop_front();
    }
}

/// The time a record carries, when it parses.
fn timestamp(record: &Value) -> Option<DateTime<Utc>> {
    record["timestamp"].as_str()?.parse().ok()
}

/// A `message.usage` object's figures, each missing one counting 0; `None`
/// when there is no usage object.
fn usage_of(usage: &Value) -> Option<Usage> {
    let fields = usage.as_object()?;
    let count = |field: &str| fields.get(field).and_then(Value::as_u64).unwrap_or(0);
    let split = usage["cache_creation"].as_object().map(|split| {
        let count = |field: &str| split.get(field).and_then(Value::as_u64).unwrap_or(0);
        (
            count("ephemeral_5m_input_tokens"),
            count("ephemeral_1h_input_tokens"),
        )
    });
    Some(Usage {
        input: count("input_tokens"),
        output: count("output_tokens"),
        cache_read: count("cache_read_input_tokens"),
        cache_creation: count("cache_creation_input_tokens"),
        cache_split: split,
    })
}

/// The tokens a reply's context held: its input, cache reads and cache
/// writes, not its output.
fn context_tokens(usage: &Usage) -> u64 {
    usage.tokens_in()
}

/// The last `text` block of an assistant message's `content` whose text is
/// not blank.
fn last_text_block(content: &Value) -> Option<&str> {
    content
        .as_array()?
        .iter()
        .rev()
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str())
        .find(|text| !text.trim().is_empty())
}

/// The prompt a `user` record gives: the slash command it runs as its
/// command line (`/nextup RT-483`), or its own text; cut to
/// [`PROMPT_BYTES`]. `None` for a record marked `isMeta`, one with no text, a
/// `/clear` and a local command's own output.
fn prompt_of(record: &Value) -> Option<String> {
    if transcripts::is_meta(record) {
        return None;
    }
    let text = records::result_text(&record["message"])?;
    let trimmed = text.trim_start();
    if LOCAL_COMMAND_OUTPUT
        .iter()
        .any(|prefix| trimmed.starts_with(prefix))
    {
        return None;
    }
    let command = records::tag_values(text, "<command-name>", "</command-name>")
        .next()
        .filter(|name| !name.is_empty());
    let prompt = match command {
        Some(CLEAR_COMMAND) => return None,
        Some(name) => match records::tag_values(text, "<command-args>", "</command-args>").next() {
            Some(args) if !args.is_empty() => format!("{name} {args}"),
            _ => name.to_owned(),
        },
        None if text.trim().is_empty() => return None,
        None => text.to_owned(),
    };
    Some(cut(&prompt, PROMPT_BYTES))
}

/// The first prompt among the `user` records within the file's first
/// [`HEAD_BYTES`]; lines that do not parse are skipped.
fn first_prompt_in_head(file: &mut File) -> io::Result<Option<String>> {
    file.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(file.by_ref().take(HEAD_BYTES));
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            return Ok(None);
        }
        let Ok(record) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        if record["type"] == "user"
            && let Some(prompt) = prompt_of(&record)
        {
            return Ok(Some(prompt));
        }
    }
}

/// `text` cut to its longest prefix of at most `max` bytes that ends on a
/// character boundary.
fn cut(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// The offset just past the first `\n` at or after `position`, or the file's
/// end when there is none.
fn next_line_start(file: &mut File, position: u64) -> io::Result<u64> {
    file.seek(SeekFrom::Start(position))?;
    let mut skipped = Vec::new();
    let read = BufReader::new(file.by_ref()).read_until(b'\n', &mut skipped)?;
    Ok(position + u64::try_from(read).unwrap_or(u64::MAX))
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test that cannot write its scratch transcript must fail loudly"
)]
pub(crate) mod tests {
    use super::TranscriptReader;
    use agent_api::{ActivityReader, ActivitySnapshot, BackgroundTask, EndedTask, OpenCall};
    use chrono::{DateTime, Utc};
    use protocol::AgentState;
    use std::fs::{self, OpenOptions};
    use std::io::Write as _;
    use std::path::{Path, PathBuf};
    use uuid::Uuid;

    const SHELLS: &str = include_str!("../tests/fixtures/subagents/shells.jsonl");
    const SUBAGENT_MAIN: &str = include_str!("../tests/fixtures/subagents/main.jsonl");
    const RUNNING: &str = include_str!("../tests/fixtures/subagents/running.jsonl");
    const RUNNING_META: &str = include_str!("../tests/fixtures/subagents/running.meta.json");
    const CALLS: &str = include_str!("../tests/fixtures/activity/calls.jsonl");
    const BACKGROUND: &str = include_str!("../tests/fixtures/activity/background.jsonl");
    const USAGE: &str = include_str!("../tests/fixtures/activity/usage.jsonl");
    const FAILURES: &str = include_str!("../tests/fixtures/activity/failures.jsonl");
    /// Its `/nextup` record is hand-made: no captured transcript in the repo
    /// runs a slash command, so it follows the `<command-name>` shape
    /// `transcripts.rs`'s `is_command_noise` drops.
    const PROMPT: &str = include_str!("../tests/fixtures/activity/prompt.jsonl");

    /// A scratch transcript under the OS temp root, with its subagents folder
    /// beside it as Claude Code lays it out; drop removes it.
    pub(crate) struct Scratch {
        dir: PathBuf,
    }

    impl Scratch {
        pub(crate) fn new(label: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("rt-activity-{label}-{}", Uuid::new_v4().simple()));
            fs::create_dir_all(&dir).expect("create scratch dir");
            Self { dir }
        }

        fn path(&self) -> PathBuf {
            self.dir.join("transcript.jsonl")
        }

        pub(crate) fn write(&self, body: &str) {
            fs::write(self.path(), body).expect("write transcript");
        }

        pub(crate) fn append(&self, body: &str) {
            append_to(&self.path(), body);
        }

        /// Writes `name` in the subagents folder, creating the folder.
        pub(crate) fn put(&self, name: &str, body: &str) {
            let folder = self.dir.join("transcript").join("subagents");
            fs::create_dir_all(&folder).expect("create subagents folder");
            fs::write(folder.join(name), body).expect("write subagent file");
        }

        /// Appends to `name` in the subagents folder.
        pub(crate) fn add(&self, name: &str, body: &str) {
            append_to(
                &self.dir.join("transcript").join("subagents").join(name),
                body,
            );
        }

        pub(crate) fn reader(&self, process_started_at: Option<DateTime<Utc>>) -> TranscriptReader {
            TranscriptReader::new(self.path(), process_started_at)
        }
    }

    fn append_to(path: &Path, body: &str) {
        OpenOptions::new()
            .append(true)
            .open(path)
            .expect("open file to append")
            .write_all(body.as_bytes())
            .expect("append to file");
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            if let Err(error) = fs::remove_dir_all(&self.dir) {
                tracing::debug!(%error, "could not remove a scratch transcript");
            }
        }
    }

    pub(crate) fn at(text: &str) -> DateTime<Utc> {
        text.parse().expect("a test time")
    }

    /// Lines `from..to` of a fixture, each ending in `\n`.
    pub(crate) fn lines(fixture: &str, from: usize, to: usize) -> String {
        fixture
            .lines()
            .skip(from)
            .take(to - from)
            .flat_map(|line| [line, "\n"])
            .collect()
    }

    fn read_whole(label: &str, body: &str, now: &str) -> ActivitySnapshot {
        let scratch = Scratch::new(label);
        scratch.write(body);
        scratch.reader(None).read(at(now))
    }

    fn call(id: &str, name: &str, command: Option<&str>, started: &str) -> OpenCall {
        OpenCall {
            id: id.to_owned(),
            name: name.to_owned(),
            command: command.map(str::to_owned),
            started: at(started),
        }
    }

    #[test]
    fn an_unanswered_call_is_open_unless_stamped_before_the_process_started() {
        let scratch = Scratch::new("open");
        scratch.write(CALLS);
        let now = at("2026-09-27T10:03:30Z");

        let current = scratch.reader(Some(at("2026-09-27T10:01:00Z"))).read(now);
        let every = scratch.reader(None).read(now);

        let bash = call(
            "toolu_01",
            "Bash",
            Some("cargo test"),
            "2026-09-27T10:02:00Z",
        );
        let agent = call("toolu_03", "Agent", None, "2026-09-27T10:03:00Z");
        let read = call("toolu_00", "Read", None, "2026-09-27T10:00:00Z");
        assert_eq!(current.open_calls, [bash.clone(), agent.clone()]);
        assert_eq!(every.open_calls, [read, bash, agent]);
        assert_eq!(current.agent_call_ids, ["toolu_03"]);
        assert_eq!(current.last_record_at, Some(at("2026-09-27T10:03:00Z")));
        assert_eq!(current.minute_counts.iter().sum::<u16>(), 4, "{current:?}");
    }

    #[test]
    fn a_background_task_runs_until_a_notification_names_it() {
        let scratch = Scratch::new("background");
        scratch.write(&lines(BACKGROUND, 0, 4));
        let mut reader = scratch.reader(None);
        let now = at("2026-09-27T10:01:00Z");
        let shell = task(
            "b1",
            "toolu_10",
            "2026-09-27T10:00:00Z",
            Some("cargo build"),
        );
        let agent = task("a1", "toolu_11", "2026-09-27T10:00:02Z", None);

        assert_eq!(reader.read(now).background, [shell, agent.clone()]);

        scratch.append(&lines(BACKGROUND, 4, 5));
        assert_eq!(reader.read(now).background, [agent]);

        scratch.append(&lines(BACKGROUND, 5, 6));
        assert_eq!(reader.read(now).background, []);
    }

    fn task(id: &str, call: &str, started: &str, command: Option<&str>) -> BackgroundTask {
        BackgroundTask {
            id: id.to_owned(),
            tool_use_id: Some(call.to_owned()),
            started: Some(at(started)),
            command: command.map(str::to_owned),
        }
    }

    fn ended(
        task: BackgroundTask,
        state: AgentState,
        status: &str,
        summary: &str,
        ended_at: &str,
    ) -> EndedTask {
        EndedTask {
            task,
            state,
            status: Some(status.to_owned()),
            summary: Some(summary.to_owned()),
            ended_at: Some(at(ended_at)),
        }
    }

    fn shell_tasks() -> [BackgroundTask; 4] {
        [
            task(
                "b50",
                "toolu_50",
                "2026-09-27T10:00:00Z",
                Some("cargo build"),
            ),
            task("b51", "toolu_51", "2026-09-27T10:00:02Z", Some("npm test")),
            task("b52", "toolu_52", "2026-09-27T10:00:04Z", Some("sleep 600")),
            task("a53", "toolu_53", "2026-09-27T10:00:06Z", None),
        ]
    }

    #[test]
    fn a_running_background_shell_is_a_running_shell_node() {
        let snapshot = read_whole(
            "shells-running",
            &lines(SHELLS, 0, 8),
            "2026-09-27T10:00:30Z",
        );

        assert_eq!(snapshot.background, shell_tasks());
        assert_eq!(snapshot.ended_background, []);
    }

    #[test]
    fn a_background_shell_ended_by_its_notification_is_a_finished_shell_node() {
        let snapshot = read_whole("shells-ended", SHELLS, "2026-09-27T10:05:00Z");

        let [build, test, sleep, _agent] = shell_tasks();
        assert_eq!(snapshot.background, []);
        assert_eq!(
            snapshot.ended_background,
            [
                ended(
                    build,
                    AgentState::Finished,
                    "completed",
                    "Background command \"cargo build\" completed (exit code 0)",
                    "2026-09-27T10:01:00Z"
                ),
                ended(
                    test,
                    AgentState::Failed,
                    "failed",
                    "Background command \"npm test\" failed with exit code 1",
                    "2026-09-27T10:02:00Z"
                ),
                ended(
                    sleep,
                    AgentState::Failed,
                    "killed",
                    "Background command \"sleep 600\" was stopped",
                    "2026-09-27T10:03:00Z"
                ),
            ]
        );
    }

    #[test]
    fn a_background_task_whose_id_is_a_subagent_id_is_not_an_ended_shell() {
        let scratch = Scratch::new("shell-agent");
        scratch.write(SHELLS);
        scratch.put("agent-a53.jsonl", RUNNING);
        scratch.put(
            "agent-a53.meta.json",
            &RUNNING_META.replace("toolu_20", "toolu_53"),
        );

        let snapshot = scratch.reader(None).read(at("2026-09-27T10:05:00Z"));

        let subagents: Vec<&str> = snapshot
            .subagents
            .iter()
            .map(|node| node.agent_id.as_str())
            .collect();
        let ended: Vec<&str> = snapshot
            .ended_background
            .iter()
            .map(|ended| ended.task.id.as_str())
            .collect();
        assert_eq!(subagents, ["a53"]);
        assert_eq!(ended, ["b50", "b51", "b52"]);
    }

    #[test]
    fn a_subagents_tokens_and_cost_add_to_the_session_total() {
        let scratch = Scratch::new("subagent-cost");
        scratch.write(SUBAGENT_MAIN);
        let now = at("2026-09-27T10:01:00Z");
        let alone = scratch.reader(None).read(now);

        scratch.put("agent-a20.jsonl", RUNNING);
        scratch.put("agent-a20.meta.json", RUNNING_META);
        let with = scratch.reader(None).read(now);

        assert_eq!(with.subagents.len(), 1);
        assert_eq!(with.tokens_in, alone.tokens_in + 1_000);
        assert_eq!(with.tokens_out, alone.tokens_out + 100);
        let subagent_cost = (1_000.0 * 4.0 + 100.0 * 20.0) / 1_000_000.0;
        assert!(
            (with.cost_usd - alone.cost_usd - subagent_cost).abs() < 1e-12,
            "cost {} != {} + {subagent_cost}",
            with.cost_usd,
            alone.cost_usd
        );
        assert_eq!(
            with.context, alone.context,
            "a subagent's reply is no context reading"
        );
    }

    #[test]
    fn a_streamed_message_is_priced_once_by_its_growth() {
        let snapshot = read_whole("usage", USAGE, "2026-09-27T10:01:00Z");

        let expected_cost = (10.0 * 4.0
            + 50.0 * 20.0
            + 1000.0 * 1.25 * 4.0
            + 100.0 * 0.2
            + 20.0 * 4.0
            + 10.0 * 20.0
            + 404_000.0 * 0.2)
            / 1_000_000.0;
        assert_eq!(snapshot.tokens_in, 1_110 + 404_020);
        assert_eq!(snapshot.tokens_out, 60);
        assert!(
            (snapshot.cost_usd - expected_cost).abs() < 1e-12,
            "cost {} != {expected_cost}",
            snapshot.cost_usd
        );
        let context = snapshot.context.expect("a context reading");
        assert_eq!(context.model, "claude-opus-5-5");
        assert_eq!(context.tokens, 404_020);
        assert_eq!(context.percent, Some(40));
        assert_eq!(snapshot.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(snapshot.last_assistant_text.as_deref(), Some("Done."));
    }

    #[test]
    fn a_model_with_no_window_has_no_context_percent() {
        let snapshot = read_whole("no-window", CALLS, "2026-09-27T10:03:30Z");

        let context = snapshot.context.expect("a context reading");
        assert_eq!(context.model, "fake-model");
        assert_eq!(context.percent, None);
        assert!(snapshot.cost_usd.abs() < f64::EPSILON);
        assert_eq!(snapshot.tokens_in, 4);
    }

    #[test]
    fn minute_counts_slide_with_now() {
        // Calls start at 10:00:00, 10:02:00, 10:02:30 and 10:03:00.
        let before_the_window_ends = read_whole("minutes-a", CALLS, "2026-09-27T10:09:59Z");
        let the_first_dropped = read_whole("minutes-b", CALLS, "2026-09-27T10:10:00Z");
        let three_in_the_future = read_whole("minutes-c", CALLS, "2026-09-27T10:01:00Z");

        assert_eq!(
            before_the_window_ends.minute_counts,
            [1, 0, 2, 1, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(
            the_first_dropped.minute_counts,
            [0, 1, 2, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(
            three_in_the_future.minute_counts,
            [0, 0, 0, 0, 0, 0, 0, 0, 1, 3]
        );
    }

    #[test]
    fn denials_and_interrupts_neither_extend_nor_end_the_failure_run() {
        let scratch = Scratch::new("failures");
        let mut reader = scratch.reader(None);
        let now = at("2026-09-27T10:01:00Z");

        scratch.write(&lines(FAILURES, 0, 2));
        assert_eq!(reader.read(now).failure_run, 2);

        scratch.append(&lines(FAILURES, 2, 5));
        assert_eq!(reader.read(now).failure_run, 2);

        scratch.append(&lines(FAILURES, 5, 6));
        assert_eq!(reader.read(now).failure_run, 3);

        scratch.append(&lines(FAILURES, 6, 7));
        assert_eq!(reader.read(now).failure_run, 0);
    }

    #[test]
    fn the_first_prompt_is_a_slash_command_line_found_past_a_long_tail() {
        let short = read_whole("prompt-short", PROMPT, "2026-09-27T10:01:00Z");
        let plain = read_whole("prompt-plain", CALLS, "2026-09-27T10:01:00Z");

        let mut long = PROMPT.to_owned();
        let filler = "x".repeat(600);
        let padding = format!(
            r#"{{"type":"assistant","timestamp":"2026-09-27T10:00:05.000Z","message":{{"role":"assistant","content":[{{"type":"text","text":"{filler}"}}]}}}}"#
        );
        while long.len() <= 1_100_000 {
            long.push_str(&padding);
            long.push('\n');
        }
        let past_the_tail = read_whole("prompt-long", &long, "2026-09-27T10:01:00Z");

        assert_eq!(short.first_prompt.as_deref(), Some("/nextup RT-483"));
        assert_eq!(plain.first_prompt.as_deref(), Some("Fix the build"));
        assert_eq!(
            past_the_tail.first_prompt.as_deref(),
            Some("/nextup RT-483")
        );
        assert_eq!(
            past_the_tail.last_assistant_text.as_deref(),
            Some(filler.as_str())
        );
    }

    #[test]
    fn a_shrunk_file_is_read_again_from_scratch() {
        let scratch = Scratch::new("shrunk");
        scratch.write(CALLS);
        let mut reader = scratch.reader(Some(at("2026-09-27T10:01:00Z")));
        let now = at("2026-09-27T10:03:30Z");
        assert_eq!(reader.read(now).open_calls.len(), 2);

        scratch.write(&lines(CALLS, 0, 3));
        let again = reader.read(now);

        assert_eq!(
            again.open_calls,
            [call(
                "toolu_01",
                "Bash",
                Some("cargo test"),
                "2026-09-27T10:02:00Z"
            )]
        );
        assert_eq!(again.agent_call_ids, [] as [&str; 0]);
        assert_eq!(again.minute_counts.iter().sum::<u16>(), 2);
        assert_eq!(again.first_prompt.as_deref(), Some("Fix the build"));
    }
}
