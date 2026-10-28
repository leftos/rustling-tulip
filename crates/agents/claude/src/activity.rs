//! The Claude plugin's transcript activity reader: it follows one
//! conversation's main transcript, parsing only the lines appended since the
//! last read, and reports what the session is doing for the Dashboard.
//!
//! A first read of a file longer than [`TAIL_BYTES`] starts at the first
//! whole line of its last [`TAIL_BYTES`], and the first prompt is then looked
//! for in the file's first [`HEAD_BYTES`]. A line still being written (no
//! `\n` yet) is left for the next read. A file that shrank or was replaced is
//! read again from scratch.

use std::collections::{HashMap, VecDeque};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use agent_api::{
    ActivityReader, ActivitySnapshot, ActivitySource, BackgroundTask, ContextReading,
    MINUTE_COLUMNS, OpenCall,
};
use chrono::{DateTime, Utc};
use serde_json::Value;
use tracing::debug;

use crate::pricing::{self, Usage};
use crate::records::{self, NOTIFICATION_TAG};
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

/// One transcript's reader: where it is and what it has read so far.
struct TranscriptReader {
    path: PathBuf,
    process_started_at: Option<DateTime<Utc>>,
    state: State,
    /// Whether the file's absence has been logged since it was last read.
    missing_logged: bool,
}

impl ActivityReader for TranscriptReader {
    fn read(&mut self, now: DateTime<Utc>) -> ActivitySnapshot {
        if let Err(error) = self.refresh() {
            self.log_unreadable(&error);
        }
        self.state.snapshot(now, self.process_started_at)
    }
}

impl TranscriptReader {
    fn new(path: PathBuf, process_started_at: Option<DateTime<Utc>>) -> Self {
        Self {
            path,
            process_started_at,
            state: State::default(),
            missing_logged: false,
        }
    }

    /// Reads what the file gained, starting over when it shrank or was
    /// replaced since the last read.
    fn refresh(&mut self) -> io::Result<()> {
        let meta = fs::metadata(&self.path)?;
        let identity = (meta.len(), meta.created().ok());
        if self.state.read_once
            && (identity.0 < self.state.identity.0 || identity.1 != self.state.identity.1)
        {
            self.state = State::default();
        }
        self.missing_logged = false;
        self.state.read_new(&self.path, identity)
    }

    /// Logs a transcript that cannot be read: a missing one once until it
    /// appears, any other failure each time.
    fn log_unreadable(&mut self, error: &io::Error) {
        if error.kind() != io::ErrorKind::NotFound {
            debug!(path = %self.path.display(), %error, "activity: the transcript could not be read");
        } else if !self.missing_logged {
            debug!(path = %self.path.display(), "activity: no transcript yet");
            self.missing_logged = true;
        }
    }
}

/// What a reader has gathered from the lines it read.
#[derive(Default)]
struct State {
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
    agent_call_ids: Vec<String>,
    background: Vec<BackgroundTask>,
    /// When each call of the last ten minutes started.
    calls: Vec<DateTime<Utc>>,
    /// The largest usage seen of each recent message id.
    usage_seen: VecDeque<(String, Usage)>,
    tokens_in: u64,
    tokens_out: u64,
    cost_usd: f64,
    /// The newest main-thread reply's model and context tokens.
    context: Option<(String, u64)>,
    failure_run: u32,
    first_prompt: Option<String>,
    last_record_at: Option<DateTime<Utc>>,
    last_assistant_text: Option<String>,
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
        if let Some(at) = timestamp(record) {
            self.last_record_at = Some(self.last_record_at.map_or(at, |last| last.max(at)));
        }
        match record["type"].as_str() {
            Some("assistant") => self.merge_assistant(record),
            Some("user") => self.merge_user(record, notifies),
            Some("attachment") if notifies && record["attachment"]["type"] == "queued_command" => {
                if let Some(text) = record["attachment"]["prompt"].as_str() {
                    self.finish_tasks(text);
                }
            }
            Some("queue-operation") if notifies => {
                if let Some(text) = record["content"].as_str() {
                    self.finish_tasks(text);
                }
            }
            _ => {}
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
    /// names as finished.
    fn merge_user(&mut self, record: &Value, notifies: bool) {
        if self.prompt_from_lines && self.first_prompt.is_none() {
            self.first_prompt = prompt_of(record);
        }
        let content = &record["message"]["content"];
        let mut launching_call = None;
        let blocks = content.as_array().into_iter().flatten();
        for block in blocks.filter(|block| block["type"] == "tool_result") {
            if let Some(id) = block["tool_use_id"].as_str() {
                self.open.remove(id);
                launching_call.get_or_insert(id);
            }
            self.count_result(block);
        }
        if let Some(task) = records::launched_task(&record["toolUseResult"])
            && !self.background.iter().any(|running| running.id == task)
        {
            self.background.push(BackgroundTask {
                id: task.to_owned(),
                tool_use_id: launching_call.map(str::to_owned),
            });
        }
        if notifies && let Some(text) = content.as_str() {
            self.finish_tasks(text);
        }
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

    /// Drops each background task a `<task-id>` of `text` names.
    fn finish_tasks(&mut self, text: &str) {
        let finished: Vec<&str> = records::task_ids_in(text).collect();
        self.background
            .retain(|task| !finished.contains(&task.id.as_str()));
    }

    /// The state as of `now`, dropping the call times ten minutes or more
    /// before it; a call stamped after `now` counts in the last column.
    fn snapshot(
        &mut self,
        now: DateTime<Utc>,
        process_started_at: Option<DateTime<Utc>>,
    ) -> ActivitySnapshot {
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
mod tests {
    use super::TranscriptReader;
    use agent_api::{ActivityReader, ActivitySnapshot, BackgroundTask, OpenCall};
    use chrono::{DateTime, Utc};
    use std::fs::{self, OpenOptions};
    use std::io::Write as _;
    use std::path::PathBuf;
    use uuid::Uuid;

    const CALLS: &str = include_str!("../tests/fixtures/activity/calls.jsonl");
    const BACKGROUND: &str = include_str!("../tests/fixtures/activity/background.jsonl");
    const USAGE: &str = include_str!("../tests/fixtures/activity/usage.jsonl");
    const FAILURES: &str = include_str!("../tests/fixtures/activity/failures.jsonl");
    /// Its `/nextup` record is hand-made: no captured transcript in the repo
    /// runs a slash command, so it follows the `<command-name>` shape
    /// `transcripts.rs`'s `is_command_noise` drops.
    const PROMPT: &str = include_str!("../tests/fixtures/activity/prompt.jsonl");

    /// A scratch transcript under the OS temp root; drop removes it.
    struct Scratch {
        dir: PathBuf,
    }

    impl Scratch {
        fn new(label: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("rt-activity-{label}-{}", Uuid::new_v4().simple()));
            fs::create_dir_all(&dir).expect("create scratch dir");
            Self { dir }
        }

        fn path(&self) -> PathBuf {
            self.dir.join("transcript.jsonl")
        }

        fn write(&self, body: &str) {
            fs::write(self.path(), body).expect("write transcript");
        }

        fn append(&self, body: &str) {
            OpenOptions::new()
                .append(true)
                .open(self.path())
                .expect("open transcript")
                .write_all(body.as_bytes())
                .expect("append to transcript");
        }

        fn reader(&self, process_started_at: Option<DateTime<Utc>>) -> TranscriptReader {
            TranscriptReader::new(self.path(), process_started_at)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            if let Err(error) = fs::remove_dir_all(&self.dir) {
                tracing::debug!(%error, "could not remove a scratch transcript");
            }
        }
    }

    fn at(text: &str) -> DateTime<Utc> {
        text.parse().expect("a test time")
    }

    /// Lines `from..to` of a fixture, each ending in `\n`.
    fn lines(fixture: &str, from: usize, to: usize) -> String {
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
        let shell = BackgroundTask {
            id: "b1".to_owned(),
            tool_use_id: Some("toolu_10".to_owned()),
        };
        let agent = BackgroundTask {
            id: "a1".to_owned(),
            tool_use_id: Some("toolu_11".to_owned()),
        };

        assert_eq!(reader.read(now).background, [shell, agent.clone()]);

        scratch.append(&lines(BACKGROUND, 4, 5));
        assert_eq!(reader.read(now).background, [agent]);

        scratch.append(&lines(BACKGROUND, 5, 6));
        assert_eq!(reader.read(now).background, []);
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
