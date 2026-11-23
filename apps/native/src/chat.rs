//! The chat model: a session's conversation rows by id with the display cap,
//! tool-card expansion, the one-line tool summary, the answer builders, the
//! prompt card stack with its digit grace, which panes show a chat, which
//! sessions' chats this connection has loaded, when the chat follows its
//! bottom, and the running-work strip's text.
//!
//! It is pure data: it takes the clock as a parameter and depends on
//! `protocol` and the wait wording alone.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use crate::completion::CompletionItem;
use crate::stretchtext;

use chrono::{DateTime, TimeDelta, Utc};
use protocol::{
    CatalogEntry, ChatAnswerKind, ChatEntry, ChatPrompt, ChatPromptKind, ChatScreen, ClientMessage,
    ComposerState, ScriptFacts, ToolKind, WorkShell, WorkSubagent,
};

/// How many rows the client keeps: a load reply carries the newest this
/// many, and the rows past it drop off the top with the count kept hidden.
pub const DISPLAY_CAP: usize = 500;

/// How long after a prompt card appears its digits may answer it.
pub const DIGIT_GRACE: Duration = Duration::from_millis(400);

/// How many prompt cards the stack keeps behind the current one.
const GHOST_CAP: usize = 3;

/// The longest one-line tool summary, the closing ellipsis included.
const ONE_LINER_CAP: usize = 80;

/// How many ids that left a log (trimmed off its top or removed) it
/// remembers, so a late update for one is not taken for a new row.
const GONE_CAP: usize = 4 * DISPLAY_CAP;

/// How near its bottom, in pixels, a chat still counts as at it.
const BOTTOM_SLACK: f32 = 1.0;

/// The id a chat entry is known by, or `None` for an entry kind this build
/// does not know.
#[must_use]
pub fn entry_id(entry: &ChatEntry) -> Option<&str> {
    match entry {
        ChatEntry::User { id, .. }
        | ChatEntry::Answer { id, .. }
        | ChatEntry::Thinking { id, .. }
        | ChatEntry::Tool { id, .. }
        | ChatEntry::System { id, .. }
        | ChatEntry::Agent { id, .. }
        | ChatEntry::Receipt { id, .. }
        | ChatEntry::TurnEnd { id, .. } => Some(id),
        ChatEntry::Unknown => None,
    }
}

/// The newest ids that left a log, oldest first, at most [`GONE_CAP`].
#[derive(Debug, Default)]
struct GoneIds {
    order: VecDeque<String>,
    known: HashSet<String>,
}

impl GoneIds {
    fn insert(&mut self, id: &str) {
        if !self.known.insert(id.to_owned()) {
            return;
        }
        self.order.push_back(id.to_owned());
        if self.order.len() > GONE_CAP
            && let Some(oldest) = self.order.pop_front()
        {
            self.known.remove(&oldest);
        }
    }

    fn remove(&mut self, id: &str) {
        if self.known.remove(id) {
            self.order.retain(|known| known != id);
        }
    }

    fn contains(&self, id: &str) -> bool {
        self.known.contains(id)
    }

    fn clear(&mut self) {
        self.order.clear();
        self.known.clear();
    }
}

/// One session's chat rows in arrival order, the tool cards the user opened,
/// the Compact tool runs the user opened (by their first call's id), each
/// answer's open stretches, the count of older rows the display cap or a
/// load left out, and the ids that left since the last load.
#[derive(Debug, Default)]
pub struct ChatLog {
    entries: Vec<ChatEntry>,
    expanded: HashSet<String>,
    open_runs: HashSet<String>,
    stretchtext: HashMap<String, BTreeSet<String>>,
    hidden: u32,
    gone: GoneIds,
}

impl ChatLog {
    /// Replaces every row with a load reply's rows, taking that reply's count
    /// of the older rows it left out; every card and stretch closes, and an
    /// id that left before is accepted again.
    pub fn load(&mut self, entries: Vec<ChatEntry>, hidden: u32) {
        self.entries = entries
            .into_iter()
            .filter(|entry| !matches!(entry, ChatEntry::Unknown))
            .collect();
        self.expanded.clear();
        self.open_runs.clear();
        self.stretchtext.clear();
        self.gone.clear();
        self.hidden = hidden;
    }

    /// A load the daemon sent unasked: it replaces every row as a load does,
    /// but the cards and stretches the user opened stay open when their rows
    /// survive it.
    pub fn reset(&mut self, entries: Vec<ChatEntry>, hidden: u32) {
        let expanded = std::mem::take(&mut self.expanded);
        let open_runs = std::mem::take(&mut self.open_runs);
        let stretchtext = std::mem::take(&mut self.stretchtext);
        self.load(entries, hidden);
        self.expanded = expanded
            .into_iter()
            .filter(|id| self.get(id).is_some())
            .collect();
        self.open_runs = open_runs
            .into_iter()
            .filter(|id| self.get(id).is_some())
            .collect();
        self.stretchtext = stretchtext
            .into_iter()
            .filter(|(id, _)| self.get(id).is_some())
            .collect();
    }

    /// Drops the rows `ids` names and forgets their expansion and open
    /// stretches; a later update for one of them is ignored until the next
    /// load.
    pub fn remove(&mut self, ids: &[String]) {
        for id in ids {
            self.entries
                .retain(|entry| entry_id(entry) != Some(id.as_str()));
            self.expanded.remove(id);
            self.open_runs.remove(id);
            self.stretchtext.remove(id);
            self.gone.insert(id);
        }
    }

    /// Applies one append: the `removed` ids leave first, then `entries` are
    /// upserted, so an id both removed and carried comes back at the bottom.
    /// A removed live answer's open stretches move to the new finished
    /// answer that replaces it: the removed live answers and the new finished
    /// answers the append takes pair up in order, each live answer by its own
    /// slot whether or not it had stretches open.
    pub fn append(&mut self, entries: Vec<ChatEntry>, removed: &[String]) {
        let moved: Vec<Option<BTreeSet<String>>> = removed
            .iter()
            .filter(|id| matches!(self.get(id), Some(ChatEntry::Answer { live: true, .. })))
            .map(|id| self.stretchtext.get(id).cloned())
            .collect();
        let finals: Vec<String> = entries
            .iter()
            .filter(|entry| matches!(entry, ChatEntry::Answer { live: false, .. }))
            .filter_map(entry_id)
            .filter(|id| self.get(id).is_none())
            .filter(|id| !self.gone.contains(id) || removed.iter().any(|gone| gone == id))
            .map(ToOwned::to_owned)
            .collect();
        self.remove(removed);
        for entry in &entries {
            if let Some(id) = entry_id(entry)
                && removed.iter().any(|gone| gone == id)
            {
                self.gone.remove(id);
            }
        }
        self.upsert(entries);
        for (id, open) in finals.into_iter().zip(moved) {
            if let Some(open) = open
                && self.get(&id).is_some()
            {
                self.stretchtext.insert(id, open);
            }
        }
    }

    /// Applies appended rows: a known id is replaced in its place, a new id
    /// is pushed, an id that left (trimmed off the top or removed) and an
    /// entry kind this build does not know are skipped, and the rows past
    /// [`DISPLAY_CAP`] drop off the top, counted into `hidden` with their
    /// expansion forgotten. A prompt still waiting on the running turn draws
    /// after every other row, so the turn's own rows landing later never sit
    /// above it.
    pub fn upsert(&mut self, entries: Vec<ChatEntry>) {
        for entry in entries {
            let Some(id) = entry_id(&entry).map(ToOwned::to_owned) else {
                continue;
            };
            if self.gone.contains(&id) {
                continue;
            }
            match self
                .entries
                .iter()
                .position(|known| entry_id(known) == Some(id.as_str()))
            {
                Some(index) => self.entries[index] = entry,
                None => self.entries.push(entry),
            }
        }
        self.entries
            .sort_by_key(|entry| matches!(entry, ChatEntry::User { pending: true, .. }));
        self.trim();
    }

    /// The rows in display order.
    pub fn entries(&self) -> impl Iterator<Item = &ChatEntry> {
        self.entries.iter()
    }

    /// The row known by `id`, while the log still holds it.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&ChatEntry> {
        self.entries
            .iter()
            .find(|entry| entry_id(entry) == Some(id))
    }

    /// How many rows the log holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the log holds no rows at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// How many older rows the display cap or the load reply left out.
    #[must_use]
    pub fn hidden(&self) -> u32 {
        self.hidden
    }

    /// Opens or closes the tool card `id`, returning its new state. A row
    /// that is not a tool call, or an id the log does not hold, changes
    /// nothing and stays closed.
    pub fn toggle_expanded(&mut self, id: &str) -> bool {
        if !matches!(self.get(id), Some(ChatEntry::Tool { .. })) {
            return false;
        }
        if self.expanded.remove(id) {
            return false;
        }
        self.expanded.insert(id.to_owned());
        true
    }

    /// Whether the tool card `id` is open.
    #[must_use]
    pub fn is_expanded(&self, id: &str) -> bool {
        self.expanded.contains(id)
    }

    /// Opens or closes the Compact tool run whose first call is `first_id`,
    /// returning its new state. An id that is not a tool call the log
    /// holds changes nothing and stays closed.
    pub fn toggle_run(&mut self, first_id: &str) -> bool {
        if !matches!(self.get(first_id), Some(ChatEntry::Tool { .. })) {
            return false;
        }
        if self.open_runs.remove(first_id) {
            return false;
        }
        self.open_runs.insert(first_id.to_owned());
        true
    }

    /// Whether the Compact tool run whose first call is `first_id` is open.
    #[must_use]
    pub fn is_run_open(&self, first_id: &str) -> bool {
        self.open_runs.contains(first_id)
    }

    /// Opens the stretch `key` of answer `id`, or closes it and every
    /// stretch nested in it when it is open, returning whether it is open
    /// now. A row that is not an answer, or an id the log does not hold,
    /// changes nothing.
    pub fn toggle_stretchtext(&mut self, id: &str, key: &str) -> bool {
        if !matches!(self.get(id), Some(ChatEntry::Answer { .. })) {
            return false;
        }
        let open = self.stretchtext.entry(id.to_owned()).or_default();
        if open.contains(key) {
            stretchtext::close(open, key);
            return false;
        }
        open.insert(key.to_owned());
        true
    }

    /// The keys of answer `id`'s open stretches, when any were opened.
    #[must_use]
    pub fn open_stretchtext(&self, id: &str) -> Option<&BTreeSet<String>> {
        self.stretchtext.get(id)
    }

    /// Drops the rows past [`DISPLAY_CAP`], counting them hidden and
    /// forgetting their expansion.
    fn trim(&mut self) {
        let excess = self.entries.len().saturating_sub(DISPLAY_CAP);
        if excess == 0 {
            return;
        }
        for entry in self.entries.drain(..excess) {
            if let Some(id) = entry_id(&entry) {
                self.expanded.remove(id);
                self.open_runs.remove(id);
                self.stretchtext.remove(id);
                self.gone.insert(id);
            }
        }
        self.hidden = self
            .hidden
            .saturating_add(u32::try_from(excess).unwrap_or(u32::MAX));
    }
}

/// How a chat pane draws tool calls: Compact folds each run of consecutive
/// read, search, run and script calls into one line; Comfortable draws one
/// card per call. In both, a script call is one row holding the calls it ran.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ChatMode {
    #[default]
    Compact,
    Comfortable,
}

impl ChatMode {
    /// The name `native-ui.json` keeps the mode under.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Comfortable => "comfortable",
        }
    }

    /// The mode a saved name stands for, when it names one.
    #[must_use]
    pub fn of_name(name: &str) -> Option<Self> {
        match name {
            "compact" => Some(Self::Compact),
            "comfortable" => Some(Self::Comfortable),
            _ => None,
        }
    }

    /// The other mode, which the header's Compact toggle switches to.
    #[must_use]
    pub fn toggled(self) -> Self {
        match self {
            Self::Compact => Self::Comfortable,
            Self::Comfortable => Self::Compact,
        }
    }
}

/// What one Compact run of tool calls did: how many read, searched and ran,
/// how many scripts it holds, how many of them failed, and whether one
/// still runs. A script's inner calls are not counted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunSummary {
    pub read: usize,
    pub searched: usize,
    pub ran: usize,
    pub failed: usize,
    pub scripts: usize,
    pub running: bool,
}

impl RunSummary {
    /// The run's line in the terminal's words, such as
    /// `1 read · 2 searched · 2 ran · 1 failed · 1 script`: the counts read,
    /// searched, ran with the zero ones left out, then the failed count when
    /// one failed, then the script count (`1 script`, `2 scripts`) when the
    /// run holds one.
    #[must_use]
    pub fn text(&self) -> String {
        let mut parts: Vec<String> = [
            (self.read, "read"),
            (self.searched, "searched"),
            (self.ran, "ran"),
        ]
        .into_iter()
        .filter(|(count, _)| *count > 0)
        .map(|(count, verb)| format!("{count} {verb}"))
        .collect();
        if self.failed > 0 {
            parts.push(format!("{} failed", self.failed));
        }
        match self.scripts {
            0 => {}
            1 => parts.push("1 script".to_owned()),
            many => parts.push(format!("{many} scripts")),
        }
        parts.join(" \u{b7} ")
    }

    fn add(&mut self, kind: ToolKind, failed: bool, running: bool) {
        match kind {
            ToolKind::Read => self.read += 1,
            ToolKind::Search => self.searched += 1,
            ToolKind::Run => self.ran += 1,
            ToolKind::Script => self.scripts += 1,
            ToolKind::Other | ToolKind::Unknown => {}
        }
        self.failed += usize::from(failed);
        self.running |= running;
    }
}

/// A script call (a tool call of the Script kind) and the calls it ran (the
/// tool entries whose `parent_id` is its id), in log order, with whether it
/// still runs: it has no result, and no turn end or user message follows it
/// unless it runs in the background (it has an eval number).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptRow<'a> {
    pub call: &'a ChatEntry,
    pub inner: Vec<&'a ChatEntry>,
    pub running: bool,
}

impl ScriptRow<'_> {
    /// The script as plain data, for tests and the spec hooks.
    #[must_use]
    pub fn shape(&self) -> RowShape {
        RowShape::Script {
            id: entry_id(self.call).unwrap_or_default().to_owned(),
            inner: self
                .inner
                .iter()
                .filter_map(|entry| entry_id(entry))
                .map(ToOwned::to_owned)
                .collect(),
            running: self.running,
        }
    }

    /// Ends the script's run when its turn is over, unless it runs in the
    /// background.
    fn settle(&mut self) {
        self.running &= runs_in_background(self.call);
    }
}

impl<'a> ScriptRow<'a> {
    /// The inner calls whose cards draw under the script's line: every one
    /// while the script call's card is `open`; closed, only those that name
    /// files to tail, so their Tail buttons and boxes stay in sight.
    pub fn shown_inner(&self, open: bool) -> impl Iterator<Item = &'a ChatEntry> + '_ {
        self.inner
            .iter()
            .copied()
            .filter(move |call| open || tails_files(call))
    }
}

/// One item of a Compact run: an entry (a read, search or run call, or a
/// thinking row between them), or a script with its inner calls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunItem<'a> {
    Entry(&'a ChatEntry),
    Script(ScriptRow<'a>),
}

impl RunItem<'_> {
    /// The item's id: the entry's, or the script call's.
    #[must_use]
    pub fn id(&self) -> Option<&str> {
        match self {
            Self::Entry(entry) => entry_id(entry),
            Self::Script(script) => entry_id(script.call),
        }
    }
}

/// One row a chat pane draws: an entry on its own, a script with the calls
/// it ran, or (in Compact) a run of consecutive read, search, run and script
/// calls drawn as one line, its items in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisplayRow<'a> {
    Single(&'a ChatEntry),
    Script(ScriptRow<'a>),
    Run {
        entries: Vec<RunItem<'a>>,
        summary: RunSummary,
    },
}

impl<'a> From<RunItem<'a>> for DisplayRow<'a> {
    /// A run's item standing on its own: an entry as a single row, a script
    /// as its script row.
    fn from(item: RunItem<'a>) -> Self {
        match item {
            RunItem::Entry(entry) => Self::Single(entry),
            RunItem::Script(script) => Self::Script(script),
        }
    }
}

impl DisplayRow<'_> {
    /// The row's id: the entry's, the script call's, or the run's first
    /// item's.
    #[must_use]
    pub fn id(&self) -> Option<&str> {
        match self {
            Self::Single(entry) => entry_id(entry),
            Self::Script(script) => entry_id(script.call),
            Self::Run { entries, .. } => entries.first().and_then(RunItem::id),
        }
    }

    /// The row as plain data, for tests and the spec hooks.
    #[must_use]
    pub fn shape(&self) -> RowShape {
        match self {
            Self::Single(entry) => RowShape::Entry(entry_id(entry).unwrap_or_default().to_owned()),
            Self::Script(script) => script.shape(),
            Self::Run { entries, summary } => RowShape::Run {
                ids: entries
                    .iter()
                    .filter_map(RunItem::id)
                    .map(ToOwned::to_owned)
                    .collect(),
                line: summary.text(),
                running: summary.running,
            },
        }
    }
}

/// A [`DisplayRow`] as plain data: an entry's id; a script's id, its inner
/// calls' ids and whether it still runs; or a run's item ids (a script in it
/// by its own id), its line and whether a call in it still runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowShape {
    Entry(String),
    Script {
        id: String,
        inner: Vec<String>,
        running: bool,
    },
    Run {
        ids: Vec<String>,
        line: String,
        running: bool,
    },
}

/// Where a script stands on its line: still running, done, or failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptState {
    Running,
    Done,
    Failed,
}

/// One chip of a script's line: a count of its finished inner calls of one
/// kind and outcome, and whether they failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptChip {
    pub text: String,
    pub failed: bool,
}

/// What a script's one line draws: its state, its description, its number
/// and time when known, its inner calls as chips, and a failed script's
/// error text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptLine {
    pub state: ScriptState,
    pub description: String,
    pub meta: Option<String>,
    pub chips: Vec<ScriptChip>,
    pub error: Option<String>,
}

/// The line `row` draws: running while it runs, else failed when its result
/// is an error or its facts carry one, else done; the first non-empty line
/// of its description, else its one-liner, else its tool's name; `#<eval>`
/// and (once it has stopped) its time, joined by a middle dot; one chip per
/// unbroken run of finished inner calls of one word and outcome; and the
/// facts' error text when it failed.
#[must_use]
pub fn script_line(row: &ScriptRow<'_>) -> ScriptLine {
    let chips = script_chips(&row.inner);
    let ChatEntry::Tool {
        name,
        summary,
        result,
        script,
        ..
    } = row.call
    else {
        let state = if row.running {
            ScriptState::Running
        } else {
            ScriptState::Done
        };
        return ScriptLine {
            state,
            description: String::new(),
            meta: None,
            chips,
            error: None,
        };
    };
    let facts = script.as_ref();
    let failed = result.as_ref().is_some_and(|outcome| outcome.is_error)
        || facts.is_some_and(|facts| facts.error.is_some());
    let state = if row.running {
        ScriptState::Running
    } else if failed {
        ScriptState::Failed
    } else {
        ScriptState::Done
    };
    let description = facts
        .and_then(|facts| {
            facts
                .description
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
        })
        .map_or_else(
            || {
                let one_liner = tool_one_liner(summary, name);
                if one_liner.is_empty() {
                    name.clone()
                } else {
                    one_liner
                }
            },
            ToOwned::to_owned,
        );
    let error = facts
        .and_then(|facts| facts.error.clone())
        .filter(|_| state == ScriptState::Failed);
    ScriptLine {
        state,
        description,
        meta: script_meta(facts, row.running),
        chips,
        error,
    }
}

/// A script's number and, once it has stopped, its time, joined by a middle
/// dot; either alone when only one is known; `None` when neither is.
fn script_meta(facts: Option<&ScriptFacts>, running: bool) -> Option<String> {
    let facts = facts?;
    let number = facts.eval.map(|eval| format!("#{eval}"));
    let time = facts.ms.filter(|_| !running).map(format_ms);
    match (number, time) {
        (Some(number), Some(time)) => Some(format!("{number} \u{b7} {time}")),
        (number, time) => number.or(time),
    }
}

/// A run of a script's finished inner calls one chip counts: their word,
/// whether it is a tool's name, and whether they failed.
struct ChipRun<'a> {
    word: &'a str,
    named: bool,
    failed: bool,
    count: usize,
}

impl ChipRun<'_> {
    fn chip(&self) -> ScriptChip {
        let (count, word) = (self.count, self.word);
        let text = match (self.failed, self.named) {
            (false, _) => format!("{count} {word}"),
            (true, false) => format!("{count} failed"),
            (true, true) => format!("{count} {word} failed"),
        };
        ScriptChip {
            text,
            failed: self.failed,
        }
    }
}

/// The chips of a script's inner calls: one per unbroken run, in order, of
/// finished calls with the same word and outcome; a call still running
/// makes no chip and breaks no run.
fn script_chips(inner: &[&ChatEntry]) -> Vec<ScriptChip> {
    let mut runs: Vec<ChipRun<'_>> = Vec::new();
    for entry in inner {
        let ChatEntry::Tool {
            name,
            kind,
            result: Some(outcome),
            ..
        } = entry
        else {
            continue;
        };
        let (word, named) = match kind {
            ToolKind::Read => ("read", false),
            ToolKind::Search => ("searched", false),
            ToolKind::Run => ("ran", false),
            ToolKind::Script | ToolKind::Other | ToolKind::Unknown => (name.as_str(), true),
        };
        let failed = outcome.is_error;
        match runs.last_mut() {
            Some(last) if last.word == word && last.named == named && last.failed == failed => {
                last.count += 1;
            }
            _ => runs.push(ChipRun {
                word,
                named,
                failed,
                count: 1,
            }),
        }
    }
    runs.iter().map(ChipRun::chip).collect()
}

/// A script's run time as ui-themes writes it: tenths of a second (floored)
/// under 10 s, whole seconds under a minute, else minutes and seconds.
#[must_use]
pub fn format_ms(ms: u64) -> String {
    if ms < 10_000 {
        format!("{}.{}s", ms / 1000, (ms / 100) % 10)
    } else if ms < 60_000 {
        format!("{}s", ms / 1000)
    } else {
        format!("{}m {}s", ms / 60_000, (ms % 60_000) / 1000)
    }
}

/// Each script call's inner calls, by the script's id. Only calls of the
/// Script kind take inner calls, and a script is never another script's
/// inner call.
type ScriptCalls<'a> = HashMap<&'a str, Vec<&'a ChatEntry>>;

/// The inner calls of every script among `entries`, in log order wherever
/// in the log the script stands.
fn script_calls<'a>(entries: &[&'a ChatEntry]) -> ScriptCalls<'a> {
    let mut scripts: ScriptCalls<'a> = entries
        .iter()
        .filter(|entry| is_script(entry))
        .filter_map(|entry| entry_id(entry))
        .map(|id| (id, Vec::new()))
        .collect();
    for &entry in entries {
        if let ChatEntry::Tool {
            parent_id: Some(parent),
            kind,
            ..
        } = entry
            && *kind != ToolKind::Script
            && let Some(calls) = scripts.get_mut(parent.as_str())
        {
            calls.push(entry);
        }
    }
    scripts
}

/// Whether `entry` is a call a script in `scripts` ran, which draws inside
/// that script's row and not on its own.
fn is_inner(entry: &ChatEntry, scripts: &ScriptCalls<'_>) -> bool {
    matches!(
        entry,
        ChatEntry::Tool { parent_id: Some(parent), kind, .. }
            if *kind != ToolKind::Script && scripts.contains_key(parent.as_str())
    )
}

/// Whether `entry` is a script call: a tool call of the Script kind.
fn is_script(entry: &ChatEntry) -> bool {
    matches!(
        entry,
        ChatEntry::Tool {
            kind: ToolKind::Script,
            ..
        }
    )
}

/// Whether call `entry` names files to tail, which keeps its card (or its
/// script's row) in sight for the Tail button.
fn tails_files(entry: &ChatEntry) -> bool {
    matches!(entry, ChatEntry::Tool { tail_paths, .. } if !tail_paths.is_empty())
}

/// Whether script call `entry` failed: its result is an error, or it carries
/// an error line.
fn script_failed(entry: &ChatEntry) -> bool {
    let ChatEntry::Tool { result, script, .. } = entry else {
        return false;
    };
    result.as_ref().is_some_and(|outcome| outcome.is_error)
        || script.as_ref().is_some_and(|facts| facts.error.is_some())
}

/// Whether script call `entry` runs in the background: it has an eval
/// number and no result yet.
fn runs_in_background(entry: &ChatEntry) -> bool {
    matches!(
        entry,
        ChatEntry::Tool { result: None, script: Some(facts), .. } if facts.eval.is_some()
    )
}

/// The row of script call `call`, holding its inner calls from `scripts`.
fn script_row<'a>(call: &'a ChatEntry, scripts: &ScriptCalls<'a>) -> ScriptRow<'a> {
    let inner = entry_id(call)
        .and_then(|id| scripts.get(id))
        .cloned()
        .unwrap_or_default();
    ScriptRow {
        call,
        inner,
        running: matches!(call, ChatEntry::Tool { result: None, .. }),
    }
}

/// The row `entry` draws as on its own: its script row for a script call,
/// else a single row.
fn standing_row<'a>(entry: &'a ChatEntry, scripts: &ScriptCalls<'a>) -> DisplayRow<'a> {
    if is_script(entry) {
        DisplayRow::Script(script_row(entry, scripts))
    } else {
        DisplayRow::Single(entry)
    }
}

/// The read, search or run kind a Compact run takes `entry` into, with
/// whether it failed and whether it has no result yet; `None` for any other
/// entry. A subagent's call (one with a `parent_id`) and a call that names
/// files to tail are never grouped, so each keeps its card.
fn run_kind(entry: &ChatEntry) -> Option<(ToolKind, bool, bool)> {
    let ChatEntry::Tool {
        kind,
        result,
        parent_id: None,
        tail_paths,
        ..
    } = entry
    else {
        return None;
    };
    (tail_paths.is_empty() && matches!(kind, ToolKind::Read | ToolKind::Search | ToolKind::Run))
        .then(|| {
            let failed = result.as_ref().is_some_and(|outcome| outcome.is_error);
            (*kind, failed, result.is_none())
        })
}

/// A Compact run being gathered: its items so far, what its calls did, and
/// the thinking rows after its last call, which join it only when another
/// call follows them.
#[derive(Default)]
struct OpenRun<'a> {
    entries: Vec<RunItem<'a>>,
    summary: RunSummary,
    thinking: Vec<&'a ChatEntry>,
    /// How many calls (scripts included) its items hold.
    calls: usize,
}

impl<'a> OpenRun<'a> {
    /// Takes `entry` into the run when it joins one: a read, search or run
    /// call, a script that no other call ran and none of whose inner calls
    /// names files to tail, or a thinking row after a call
    /// of the run. Says whether it took it.
    fn take(&mut self, entry: &'a ChatEntry, scripts: &ScriptCalls<'a>) -> bool {
        if let Some((kind, failed, running)) = run_kind(entry) {
            self.summary.add(kind, failed, running);
            self.push(RunItem::Entry(entry));
            return true;
        }
        if is_script(entry)
            && matches!(
                entry,
                ChatEntry::Tool {
                    parent_id: None,
                    ..
                }
            )
        {
            let script = script_row(entry, scripts);
            if script.inner.iter().any(|call| tails_files(call)) {
                return false;
            }
            self.summary
                .add(ToolKind::Script, script_failed(entry), script.running);
            self.push(RunItem::Script(script));
            return true;
        }
        if matches!(entry, ChatEntry::Thinking { .. }) && !self.entries.is_empty() {
            self.thinking.push(entry);
            return true;
        }
        false
    }

    /// Adds call `item`, after the thinking rows that came before it.
    fn push(&mut self, item: RunItem<'a>) {
        self.entries
            .extend(self.thinking.drain(..).map(RunItem::Entry));
        self.entries.push(item);
        self.calls += 1;
    }

    /// Closes the run: pushes it, or its items one by one in order when it
    /// holds fewer than `min_run` calls (an entry as a single row, a script
    /// as its script row), then the thinking rows after its last call on
    /// their own.
    fn flush(&mut self, rows: &mut Vec<DisplayRow<'a>>, min_run: usize) {
        let entries = std::mem::take(&mut self.entries);
        let summary = std::mem::take(&mut self.summary);
        if self.calls >= min_run && !entries.is_empty() {
            rows.push(DisplayRow::Run { entries, summary });
        } else {
            rows.extend(entries.into_iter().map(DisplayRow::from));
        }
        self.calls = 0;
        rows.extend(self.thinking.drain(..).map(DisplayRow::Single));
    }
}

/// The rows a chat pane draws for `entries` in `mode`. In both modes a
/// script call is one [`DisplayRow::Script`] holding the calls it ran, which
/// draw nowhere else; a call whose script is not among `entries` stands as
/// any other. In Comfortable every other entry is its own row; in Compact,
/// each run of consecutive read, search, run and script calls (a run of one
/// included, thinking rows between its calls folded in) becomes one
/// [`DisplayRow::Run`], and every other entry stands alone and ends the run
/// before it. A call counts as running only while it has no result and no
/// turn end or user message follows it (a message still queued does not
/// count); a script running in the background runs on past its turn's end.
#[must_use]
pub fn display_rows<'a>(
    entries: impl IntoIterator<Item = &'a ChatEntry>,
    mode: ChatMode,
) -> Vec<DisplayRow<'a>> {
    display_rows_folding(entries, mode, 1)
}

/// The rows [`display_rows`] draws, except that in Compact a run holding
/// fewer than `min_run` calls stands as its items in order: each entry its
/// own [`DisplayRow::Single`] (the thinking rows folded into it included),
/// each script its own [`DisplayRow::Script`], so no minimum hides a script.
#[must_use]
pub fn display_rows_folding<'a>(
    entries: impl IntoIterator<Item = &'a ChatEntry>,
    mode: ChatMode,
    min_run: usize,
) -> Vec<DisplayRow<'a>> {
    let entries: Vec<&'a ChatEntry> = entries.into_iter().collect();
    let scripts = script_calls(&entries);
    let mut rows = Vec::new();
    let mut run = OpenRun::default();
    for entry in entries {
        if is_inner(entry, &scripts) {
            continue;
        }
        if mode == ChatMode::Compact && run.take(entry, &scripts) {
            continue;
        }
        run.flush(&mut rows, min_run);
        rows.push(standing_row(entry, &scripts));
    }
    run.flush(&mut rows, min_run);
    settle_finished_runs(&mut rows);
    rows
}

/// Clears the running mark of every run and script an entry that
/// [`ends_turn`] comes after: that turn is over, so a call it left without
/// a result runs no more, except a script running in the background, which
/// keeps its run running too.
fn settle_finished_runs(rows: &mut [DisplayRow<'_>]) {
    let mut ended = false;
    for row in rows.iter_mut().rev() {
        match row {
            DisplayRow::Single(entry) if ends_turn(entry) => ended = true,
            DisplayRow::Script(script) if ended => script.settle(),
            DisplayRow::Run { entries, summary } if ended => {
                summary.running = false;
                for item in entries.iter_mut() {
                    if let RunItem::Script(script) = item {
                        script.settle();
                        summary.running |= script.running;
                    }
                }
            }
            _ => {}
        }
    }
}

/// Whether `entry` ends the turn before it: a turn end, a user message the
/// transcript has, or a message another agent sent. A message still pending
/// (queued while the agent works) does not.
fn ends_turn(entry: &ChatEntry) -> bool {
    matches!(
        entry,
        ChatEntry::TurnEnd { .. }
            | ChatEntry::User { pending: false, .. }
            | ChatEntry::Agent { .. }
    )
}

/// The ids of the answers among `entries` (in display order) that are
/// their turn's final reply: the last answer before a turn end, or before
/// the next user message the transcript has. A turn still running has none
/// yet, so its latest answer is not one, a message queued meanwhile
/// included. Answers never join a Compact run, so each id stands
/// for a row [`display_rows`] draws on its own in either mode.
#[must_use]
pub fn final_replies<'a>(entries: impl IntoIterator<Item = &'a ChatEntry>) -> BTreeSet<String> {
    let mut finals = BTreeSet::new();
    let mut last_answer: Option<&str> = None;
    for entry in entries {
        if let ChatEntry::Answer { id, .. } = entry {
            last_answer = Some(id);
        } else if ends_turn(entry)
            && let Some(id) = last_answer.take()
        {
            finals.insert(id.to_owned());
        }
    }
    finals
}

/// What a pane draws for its session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PaneView {
    /// The session's terminal.
    #[default]
    Terminal,
    /// The session's chat, its terminal kept fed but not drawn.
    Chat,
    /// The session's summary lines, its terminal kept fed but not drawn.
    Summary,
}

/// A key that switches a pane's view: Ctrl+Shift+M (`Chat`) or
/// Ctrl+Shift+U (`Summary`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewKey {
    Chat,
    Summary,
}

impl PaneView {
    /// What a pane showing `self` shows after `key`: Ctrl+Shift+M flips
    /// terminal and chat and leaves the summary for the terminal; Ctrl+Shift+U
    /// opens the summary and leaves it for the terminal.
    #[must_use]
    pub fn after_key(self, key: ViewKey) -> PaneView {
        match (key, self) {
            (ViewKey::Chat, PaneView::Terminal) => PaneView::Chat,
            (ViewKey::Summary, PaneView::Terminal | PaneView::Chat) => PaneView::Summary,
            (ViewKey::Chat, PaneView::Chat | PaneView::Summary)
            | (ViewKey::Summary, PaneView::Summary) => PaneView::Terminal,
        }
    }

    /// Whether a pane showing `self` hides its terminal.
    #[must_use]
    pub fn hides_terminal(self) -> bool {
        self != PaneView::Terminal
    }
}

/// The view each pane shows, by pane id; a pane not named shows its
/// terminal.
#[derive(Debug, Default)]
pub struct PaneViews {
    views: HashMap<String, PaneView>,
}

impl PaneViews {
    /// What pane `pane_id` shows.
    #[must_use]
    pub fn get(&self, pane_id: &str) -> PaneView {
        self.views.get(pane_id).copied().unwrap_or_default()
    }

    /// Whether pane `pane_id` shows its session's chat.
    #[must_use]
    pub fn is_chat(&self, pane_id: &str) -> bool {
        self.get(pane_id) == PaneView::Chat
    }

    /// Pane `pane_id` shows `view` from now on.
    pub fn set(&mut self, pane_id: &str, view: PaneView) {
        match view {
            PaneView::Terminal => {
                self.views.remove(pane_id);
            }
            PaneView::Chat | PaneView::Summary => {
                self.views.insert(pane_id.to_owned(), view);
            }
        }
    }

    /// The pane known as `old` is now `new`, and keeps its view.
    pub fn rekey(&mut self, old: &str, new: &str) {
        let view = self.get(old);
        self.views.remove(old);
        self.set(new, view);
    }

    /// Forgets the view of every pane `keep` refuses.
    pub fn retain(&mut self, mut keep: impl FnMut(&str) -> bool) {
        self.views.retain(|pane_id, _| keep(pane_id));
    }
}

/// The chats a connection starts and stops loading after a change.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct LoadChanges {
    /// The sessions to send `LoadChat` for.
    pub load: Vec<String>,
    /// The sessions to send `UnloadChat` for.
    pub unload: Vec<String>,
}

/// The sessions whose chat this connection has loaded.
#[derive(Debug, Default)]
pub struct ChatLoads {
    loaded: BTreeSet<String>,
}

impl ChatLoads {
    /// The sessions some pane wants the chat of are now `wanted`: returns
    /// the ones to load and the ones to unload, and counts them loaded.
    pub fn diff(&mut self, wanted: BTreeSet<String>) -> LoadChanges {
        let load = wanted.difference(&self.loaded).cloned().collect();
        let unload = self.loaded.difference(&wanted).cloned().collect();
        self.loaded = wanted;
        LoadChanges { load, unload }
    }

    /// A new connection has loaded nothing, and needs no unload either.
    pub fn clear(&mut self) {
        self.loaded.clear();
    }

    /// The daemon refused session `session_id`'s load, so it holds nothing
    /// to unload.
    pub fn forget(&mut self, session_id: &str) {
        self.loaded.remove(session_id);
    }
}

/// Whether a chat follows its bottom, from how far it was scrolled down and
/// how far it could scroll at its last layout: at the bottom (within a
/// pixel) it follows, scrolled up off it it stays put; a chat never laid out
/// starts at the bottom.
#[must_use]
pub fn follows_bottom(laid_out: Option<(f32, f32)>) -> bool {
    laid_out.is_none_or(|(scrolled, max_scroll)| scrolled >= max_scroll - BOTTOM_SLACK)
}

/// The one line a collapsed tool card shows: the summary's first line,
/// trimmed and cut to at most 80 characters with a closing ellipsis; nothing
/// when that line is empty or only repeats the tool's name, so the card names
/// the tool once.
#[must_use]
pub fn tool_one_liner(summary: &str, name: &str) -> String {
    let line = summary.lines().next().unwrap_or_default().trim();
    if line.is_empty() || line == name {
        return String::new();
    }
    if line.chars().count() <= ONE_LINER_CAP {
        return line.to_owned();
    }
    let mut cut: String = line.chars().take(ONE_LINER_CAP - 1).collect();
    cut.push('\u{2026}');
    cut
}

/// The prompt card a session shows, the cards it replaced, and when the
/// current one appeared.
#[derive(Debug, Default)]
pub struct PromptStack {
    current: Option<PromptCard>,
    /// The cards the current one replaced, newest first.
    ghosts: Vec<ChatPrompt>,
}

/// The card on screen and when it first appeared, for the digit grace.
#[derive(Debug)]
struct PromptCard {
    prompt: ChatPrompt,
    shown_at: Instant,
}

impl PromptStack {
    /// Reads a screen reading: a prompt id the stack does not show becomes
    /// the current card, stamped `now`, with the card before it pushed behind
    /// as a ghost and any ghost of the same id dropped; a reading of the id
    /// the stack already shows takes its new content and keeps its stamp; no
    /// prompt clears the card and every ghost.
    pub fn on_screen(&mut self, screen: &ChatScreen, now: Instant) {
        let Some(prompt) = screen.prompt.as_ref() else {
            self.current = None;
            self.ghosts.clear();
            return;
        };
        if let Some(card) = self
            .current
            .as_mut()
            .filter(|card| card.prompt.id == prompt.id)
        {
            card.prompt = prompt.clone();
            return;
        }
        if let Some(card) = self.current.take() {
            self.ghosts.insert(0, card.prompt);
            self.ghosts.truncate(GHOST_CAP);
        }
        self.ghosts.retain(|ghost| ghost.id != prompt.id);
        self.current = Some(PromptCard {
            prompt: prompt.clone(),
            shown_at: now,
        });
    }

    /// The card on screen.
    #[must_use]
    pub fn current(&self) -> Option<&ChatPrompt> {
        self.current.as_ref().map(|card| &card.prompt)
    }

    /// The cards the current one replaced, newest first, at most three.
    #[must_use]
    pub fn ghosts(&self) -> &[ChatPrompt] {
        &self.ghosts
    }

    /// When the current card's digit grace ends, for the timer that wakes the
    /// view.
    #[must_use]
    pub fn grace_ends_at(&self) -> Option<Instant> {
        self.current
            .as_ref()
            .map(|card| card.shown_at + DIGIT_GRACE)
    }

    /// The answer `digit` picks on the current card when it may: only from an
    /// empty composer, only once the grace has ended, and only for a number
    /// the prompt lists. A digit inside the grace is dropped, never queued.
    /// `multi` says the prompt is a multi-select, whose digits flip boxes
    /// instead of picking.
    #[must_use]
    pub fn digit_answer(
        &self,
        digit: u32,
        composer_empty: bool,
        multi: bool,
        now: Instant,
    ) -> Option<ChatAnswerKind> {
        if !composer_empty {
            return None;
        }
        let card = self.current.as_ref()?;
        if now < card.shown_at + DIGIT_GRACE {
            return None;
        }
        number_listed(&card.prompt, digit).then_some(if multi {
            ChatAnswerKind::Toggle { number: digit }
        } else {
            ChatAnswerKind::Option { number: digit }
        })
    }
}

/// Whether the prompt lists `number` among its options.
fn number_listed(prompt: &ChatPrompt, number: u32) -> bool {
    prompt.options.iter().any(|option| option.number == number)
}

/// Whether `text` holds anything but whitespace.
fn non_blank(text: &str) -> bool {
    !text.trim().is_empty()
}

/// The answer that picks numbered option `number`, when the prompt lists it.
#[must_use]
pub fn option_answer(prompt: &ChatPrompt, number: u32) -> Option<ChatAnswerKind> {
    number_listed(prompt, number).then_some(ChatAnswerKind::Option { number })
}

/// The answer that flips numbered checkbox `number`, when the prompt lists
/// it.
#[must_use]
pub fn toggle_answer(prompt: &ChatPrompt, number: u32) -> Option<ChatAnswerKind> {
    number_listed(prompt, number).then_some(ChatAnswerKind::Toggle { number })
}

/// The answer that submits a multi-select question.
#[must_use]
pub const fn submit_answer() -> ChatAnswerKind {
    ChatAnswerKind::Submit
}

/// The answer that goes back to the previous question.
#[must_use]
pub const fn back_answer() -> ChatAnswerKind {
    ChatAnswerKind::Back
}

/// The answer that skips this question.
#[must_use]
pub const fn skip_answer() -> ChatAnswerKind {
    ChatAnswerKind::Skip
}

/// The answer that brings a clipped Heads up's choices on screen.
#[must_use]
pub const fn reveal_answer() -> ChatAnswerKind {
    ChatAnswerKind::Reveal
}

/// The answer carrying `text` as the user's own answer, when `text` is not
/// blank; the text is passed on as typed.
#[must_use]
pub fn type_something_answer(text: &str) -> Option<ChatAnswerKind> {
    non_blank(text).then_some(ChatAnswerKind::TypeSomething {
        text: text.to_owned(),
    })
}

/// The answer that allows the call and tells the agent `text` alongside, when
/// the prompt takes an amendment and `text` is not blank.
#[must_use]
pub fn allow_with_text(prompt: &ChatPrompt, text: &str) -> Option<ChatAnswerKind> {
    (prompt.can_amend && non_blank(text)).then_some(ChatAnswerKind::AllowWithText {
        text: text.to_owned(),
    })
}

/// The answer that denies the call and tells the agent `text` alongside, when
/// the prompt is a permission dialog and `text` is not blank.
#[must_use]
pub fn deny_with_text(prompt: &ChatPrompt, text: &str) -> Option<ChatAnswerKind> {
    (prompt.kind == ChatPromptKind::Permission && non_blank(text)).then_some(
        ChatAnswerKind::DenyWithText {
            text: text.to_owned(),
        },
    )
}

/// The longest command or description a lone shell shows on the strip,
/// before its closing ellipsis.
const STRIP_COMMAND_CAP: usize = 24;

/// How often the strip repaints while an age or countdown it shows reads in
/// seconds, and at the longest once every one reads in minutes.
const STRIP_FAST_REPAINT: Duration = Duration::from_secs(1);
const STRIP_SLOW_REPAINT: Duration = Duration::from_secs(30);

/// What separates the strip's parts.
pub const STRIP_SEPARATOR: &str = " \u{b7} ";

/// The tooltip of an item only the screen's count shows.
pub const SCREEN_READ_TIP: &str = "Counted from the screen; no details reported yet";

/// The work still running beside a session's chat, as the daemon's last
/// `ChatWork` reported it: its subagents, its background shells and its
/// earliest pending wakeup.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunningWork {
    subagents: Vec<WorkSubagent>,
    shells: Vec<WorkShell>,
    wakeup_at: Option<DateTime<Utc>>,
}

/// The running-work strip at one moment: its text, the parts that text
/// joins, and one item per subagent, shell and wakeup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Strip {
    /// The parts joined with [`STRIP_SEPARATOR`].
    pub text: String,
    pub parts: Vec<String>,
    pub items: Vec<StripItem>,
}

/// What a strip item stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StripKind {
    Subagent,
    Shell,
    Wakeup,
}

/// One subagent, shell or wakeup on the strip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripItem {
    pub kind: StripKind,
    /// Its short name: a subagent's description or type, a shell's command
    /// or description as the strip cuts it, the wakeup's part.
    pub label: String,
    pub tooltip: Option<String>,
    /// Whether a click on it does anything; nothing does yet.
    pub tappable: bool,
    /// The index of the part of [`Strip::parts`] it is counted in.
    pub part: usize,
}

impl RunningWork {
    /// Takes the work a `ChatWork` reported, replacing what was known.
    pub fn set(
        &mut self,
        subagents: Vec<WorkSubagent>,
        shells: Vec<WorkShell>,
        wakeup_at: Option<DateTime<Utc>>,
    ) {
        self.subagents = subagents;
        self.shells = shells;
        self.wakeup_at = wakeup_at;
    }

    /// Whether nothing runs and no wakeup is pending: the strip is hidden.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.subagents.is_empty() && self.shells.is_empty() && self.wakeup_at.is_none()
    }

    /// The strip as of `now`, or `None` when nothing runs.
    #[must_use]
    pub fn strip(&self, now: DateTime<Utc>) -> Option<Strip> {
        let mut parts = Vec::new();
        let mut items = Vec::new();
        if !self.subagents.is_empty() {
            let part = parts.len();
            parts.push(counted(self.subagents.len(), "subagent"));
            items.extend(
                self.subagents
                    .iter()
                    .map(|agent| subagent_item(agent, part)),
            );
        }
        if !self.shells.is_empty() {
            let part = parts.len();
            parts.push(self.shells_text(now));
            items.extend(self.shells.iter().map(|shell| shell_item(shell, part)));
        }
        if let Some(at) = self.wakeup_at {
            let text = wakeup_text(now, at);
            items.push(StripItem {
                kind: StripKind::Wakeup,
                label: text.clone(),
                tooltip: None,
                tappable: false,
                part: parts.len(),
            });
            parts.push(text);
        }
        if parts.is_empty() {
            return None;
        }
        Some(Strip {
            text: parts.join(STRIP_SEPARATOR),
            parts,
            items,
        })
    }

    /// How long after `now` the strip next needs a repaint, or `None` when
    /// it is hidden: a second while an age or countdown it shows reads in
    /// seconds, else 30 seconds or the moment the soonest wakeup's
    /// countdown starts reading in seconds, whichever comes first.
    #[must_use]
    pub fn next_repaint(&self, now: DateTime<Utc>) -> Option<Duration> {
        if self.is_empty() {
            return None;
        }
        if self.counts_seconds(now) {
            return Some(STRIP_FAST_REPAINT);
        }
        let until_seconds = self
            .wakeup_at
            .filter(|at| *at > now)
            .and_then(|at| (at - now - TimeDelta::seconds(60)).to_std().ok())
            .map(|lead| lead.max(STRIP_FAST_REPAINT));
        Some(until_seconds.map_or(STRIP_SLOW_REPAINT, |lead| lead.min(STRIP_SLOW_REPAINT)))
    }

    /// Whether some age or countdown the strip shows as of `now` reads in
    /// seconds.
    #[must_use]
    pub fn counts_seconds(&self, now: DateTime<Utc>) -> bool {
        let young_shell = self
            .oldest_shell()
            .is_some_and(|since| (now - since).num_seconds() < 60);
        let near_wakeup = self
            .wakeup_at
            .is_some_and(|at| at > now && countdown_seconds(now, at) < 60);
        young_shell || near_wakeup
    }

    /// "1 background shell (cmd, 3m)", "1 background shell" for one only
    /// the screen counts, or "N background shells (oldest 3m)".
    fn shells_text(&self, now: DateTime<Utc>) -> String {
        if let [shell] = self.shells.as_slice() {
            let Some(since) = shell.since else {
                return "1 background shell".to_owned();
            };
            let age = crate::needs_you::waited(now, since);
            return match shell_name(shell) {
                Some(name) => format!("1 background shell ({}, {age})", cut(name)),
                None => format!("1 background shell ({age})"),
            };
        }
        let count = counted(self.shells.len(), "background shell");
        match self.oldest_shell() {
            Some(since) => format!("{count} (oldest {})", crate::needs_you::waited(now, since)),
            None => count,
        }
    }

    /// When the longest-running reported shell started.
    fn oldest_shell(&self) -> Option<DateTime<Utc>> {
        self.shells.iter().filter_map(|shell| shell.since).min()
    }
}

/// "1 subagent", "2 subagents".
fn counted(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

/// `text` cut to [`STRIP_COMMAND_CAP`] characters, with an ellipsis when
/// it was longer.
fn cut(text: &str) -> String {
    if text.chars().count() <= STRIP_COMMAND_CAP {
        return text.to_owned();
    }
    let mut short: String = text.chars().take(STRIP_COMMAND_CAP).collect();
    short.push('\u{2026}');
    short
}

/// A shell's command, else its description; `None` when it has neither.
fn shell_name(shell: &WorkShell) -> Option<&str> {
    shell
        .command
        .as_deref()
        .filter(|command| !command.is_empty())
        .or_else(|| Some(shell.description.as_str()).filter(|text| !text.is_empty()))
}

fn shell_item(shell: &WorkShell, part: usize) -> StripItem {
    let name = shell_name(shell);
    let tooltip = if shell.from_screen {
        Some(SCREEN_READ_TIP.to_owned())
    } else {
        name.map(str::to_owned)
    };
    StripItem {
        kind: StripKind::Shell,
        label: name.map_or_else(|| "background shell".to_owned(), cut),
        tooltip,
        tappable: false,
        part,
    }
}

fn subagent_item(agent: &WorkSubagent, part: usize) -> StripItem {
    let kind = agent.agent_type.as_deref().filter(|kind| !kind.is_empty());
    let description = Some(agent.description.as_str()).filter(|text| !text.is_empty());
    let tooltip = if agent.from_screen {
        Some(SCREEN_READ_TIP.to_owned())
    } else {
        match (kind, description) {
            (Some(kind), Some(description)) => Some(format!("{kind}: {description}")),
            (kind, description) => description.or(kind).map(str::to_owned),
        }
    };
    StripItem {
        kind: StripKind::Subagent,
        label: description.or(kind).unwrap_or("subagent").to_owned(),
        tooltip,
        tappable: false,
        part,
    }
}

/// The whole seconds until `at`, rounded up so a wakeup 3m 59.5s off
/// still reads "4m".
fn countdown_seconds(now: DateTime<Utc>, at: DateTime<Utc>) -> i64 {
    let millis = (at - now).num_milliseconds().max(0);
    (millis + 999) / 1000
}

/// "wakeup in 4m" while `at` is ahead of `now`, else "wakeup due".
fn wakeup_text(now: DateTime<Utc>, at: DateTime<Utc>) -> String {
    if at <= now {
        return "wakeup due".to_owned();
    }
    let later = now + TimeDelta::seconds(countdown_seconds(now, at));
    format!("wakeup in {}", crate::needs_you::waited(later, now))
}

/// The message that types `text` into session `session_id`'s prompt box,
/// when `text` is not blank; the text is passed on as typed.
#[must_use]
pub fn send_message(session_id: &str, text: &str, request_id: String) -> Option<ClientMessage> {
    non_blank(text).then(|| ClientMessage::ChatSend {
        session_id: session_id.to_owned(),
        text: text.to_owned(),
        request_id: Some(request_id),
    })
}

/// The message that pulls session `session_id`'s terminal prompt-box draft
/// into the chat.
#[must_use]
pub fn pull_draft_message(session_id: &str, request_id: String) -> ClientMessage {
    ClientMessage::ChatPullDraft {
        session_id: session_id.to_owned(),
        request_id: Some(request_id),
    }
}

/// The message that sends session `session_id`'s terminal prompt-box
/// draft as it stands.
#[must_use]
pub fn send_draft_message(session_id: &str, request_id: String) -> ClientMessage {
    ClientMessage::ChatSendDraft {
        session_id: session_id.to_owned(),
        request_id: Some(request_id),
    }
}

/// The message that clears session `session_id`'s terminal prompt-box
/// draft without reading it out.
#[must_use]
pub fn clear_draft_message(session_id: &str, request_id: String) -> ClientMessage {
    ClientMessage::ChatClearDraft {
        session_id: session_id.to_owned(),
        request_id: Some(request_id),
    }
}

/// The text a send carried, as the daemon's pending entry shows it.
fn sent_text(text: &str) -> String {
    text.replace("\r\n", "\n").trim().to_owned()
}

/// One send the daemon has not settled: its request id, its text, and
/// whether the screen has shown its paste in the prompt box yet.
#[derive(Debug, Clone)]
struct Send {
    request_id: String,
    text: String,
    saw_draft: bool,
    at: Instant,
}

/// The prompt box's draft a send still owns once it has settled: the send's
/// own paste can stay on screen after its entry arrives and before the turn
/// starts, and it is not a draft of the owner's.
#[derive(Debug, Clone)]
struct Owner {
    /// The screen has shown the send's paste as a draft.
    saw_draft: bool,
    /// The send's pending user entry has arrived.
    entry_arrived: bool,
    /// When the send went out, for the backstop.
    at: Instant,
}

/// A session's chat send in flight. The daemon sends no success reply: a
/// send is in flight from `ChatSend` until its pending user entry arrives,
/// the screen's composer leaves the draft its paste made, or the daemon
/// refuses it. While it is in flight its paste is not a draft of the
/// owner's. The last settled send is kept, so a refusal that comes after
/// its entry still gives its text back.
///
/// A send that settled can still own the prompt box's draft for a moment
/// (`owns_draft`): its paste may stay on screen after its entry arrived
/// and before the turn starts, so a `Draft` shows no note until the
/// composer moves past it, the turn starts, a refusal gives the text back,
/// or the backstop passes.
#[derive(Debug, Default)]
pub struct SendTracker {
    in_flight: Option<Send>,
    /// The send whose paste the prompt box may still show.
    owner: Option<Owner>,
    settled: Option<Send>,
}

impl SendTracker {
    /// A send of `text` went out under `request_id`.
    pub fn sent(&mut self, request_id: String, text: String, now: Instant) {
        self.in_flight = Some(Send {
            request_id,
            text,
            saw_draft: false,
            at: now,
        });
        self.owner = Some(Owner {
            saw_draft: false,
            entry_arrived: false,
            at: now,
        });
    }

    /// The prompt box's draft was sent as it stands under `request_id`: a
    /// send with no text of ours, whose draft is already on screen, so it
    /// settles once the composer leaves that draft or the turn starts, and
    /// a refusal gives nothing back.
    pub fn sent_draft(&mut self, request_id: String, now: Instant) {
        self.sent(request_id, String::new(), now);
        if let Some(send) = self.in_flight.as_mut() {
            send.saw_draft = true;
        }
        if let Some(owner) = self.owner.as_mut() {
            owner.saw_draft = true;
        }
    }

    /// Whether the prompt box's draft may still be the send's own paste:
    /// from the send going out until the composer moves past it, the turn
    /// starts, or the backstop passes.
    #[must_use]
    pub fn owns_draft(&self, now: Instant) -> bool {
        self.owner
            .as_ref()
            .is_some_and(|owner| now < owner.at + SEND_BACKSTOP)
    }

    /// Whether a send is in flight.
    #[must_use]
    pub fn in_flight(&self, now: Instant) -> bool {
        self.in_flight
            .as_ref()
            .is_some_and(|send| now < send.at + SEND_BACKSTOP)
    }

    /// Appended rows: a pending user entry with the send's text settles it.
    pub fn on_entries(&mut self, entries: &[ChatEntry]) {
        let Some(send) = self.in_flight.as_ref() else {
            return;
        };
        let ours = sent_text(&send.text);
        let arrived = entries.iter().any(|entry| {
            matches!(entry, ChatEntry::User { text, pending: true, .. } if sent_text(text) == ours)
        });
        if arrived {
            self.settle();
            if let Some(owner) = self.owner.as_mut() {
                owner.entry_arrived = true;
            }
        }
    }

    /// The screen's composer now reads `state`: once it has shown the
    /// send's paste as a draft, leaving the draft settles the send. The
    /// ownership of that draft ends when the composer moves past it, the
    /// turn starts, or a `Ready` (or a `Suggestion`, an empty box but for
    /// the agent's suggested next prompt) follows the send's entry.
    pub fn on_composer(&mut self, state: &ComposerState) {
        if let Some(send) = self.in_flight.as_mut() {
            if *state == ComposerState::Draft {
                send.saw_draft = true;
            } else if send.saw_draft || *state == ComposerState::Working {
                self.settle();
            }
        }

        let ends = match self.owner.as_mut() {
            Some(owner) if *state == ComposerState::Draft => {
                owner.saw_draft = true;
                false
            }
            Some(owner) => {
                *state == ComposerState::Working
                    || owner.saw_draft
                    || (matches!(
                        state,
                        ComposerState::Ready | ComposerState::Suggestion { .. }
                    ) && owner.entry_arrived)
            }
            None => false,
        };
        if ends {
            self.owner = None;
        }
    }

    /// The daemon refused `request_id`: the text of the send it names, in
    /// flight or the last settled one, which ends with it. A refusal of the
    /// in-flight or the last settled send also ends its ownership of the
    /// draft, since the text goes back to the composer and its paste is gone.
    pub fn refused(&mut self, request_id: &str) -> Option<String> {
        if self
            .in_flight
            .as_ref()
            .is_some_and(|send| send.request_id == request_id)
        {
            self.owner = None;
            return self.in_flight.take().map(|send| send.text);
        }
        if self
            .settled
            .as_ref()
            .is_some_and(|send| send.request_id == request_id)
        {
            self.owner = None;
            return self.settled.take().map(|send| send.text);
        }
        None
    }

    fn settle(&mut self) {
        self.settled = self.in_flight.take();
    }
}

/// How long a send stays in flight at most when nothing settles it sooner.
pub const SEND_BACKSTOP: Duration = Duration::from_secs(10);

/// The composer's text after a refusal gives `refused` back while it holds
/// `current`: the refused text alone, or before what was typed since. An
/// empty `refused` (a draft sent as it stands) leaves `current` as it is.
#[must_use]
pub fn restored_text(refused: &str, current: &str) -> String {
    if refused.is_empty() {
        current.to_owned()
    } else if current.is_empty() {
        refused.to_owned()
    } else {
        format!("{refused}\n{current}")
    }
}

/// Why the composer cannot send now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposerNote {
    /// No daemon connection.
    NotConnected,
    /// Our own send is in flight.
    Sending,
    /// The terminal's prompt box holds text of the owner's.
    Draft,
    /// A full-screen overlay is open in the terminal, named by its prompt's
    /// label when it shows one.
    Overlay(Option<String>),
    /// The screen's prompt box cannot be read.
    Unknown,
}

impl ComposerNote {
    /// What the composer says.
    #[must_use]
    pub fn text(&self) -> String {
        match self {
            Self::NotConnected => "Not connected".to_owned(),
            Self::Sending => "Sending\u{2026}".to_owned(),
            Self::Draft => {
                "The prompt box has a draft. Send or clear it in the terminal first.".to_owned()
            }
            Self::Overlay(Some(label)) => {
                format!("{label} is open in the terminal. Close it to send.")
            }
            Self::Overlay(None) => {
                "A full-screen view is open in the terminal. Close it there to send.".to_owned()
            }
            Self::Unknown => "Can't read the prompt box right now.".to_owned(),
        }
    }

    /// Whether the composer takes no input while it shows this.
    #[must_use]
    pub fn disables(&self) -> bool {
        matches!(self, Self::NotConnected | Self::Overlay(_))
    }
}

/// The note the composer shows: none while it can send. `connected` says
/// the daemon connection is open, `state` is the screen's last composer
/// state (none before the chat loaded), `sending` says our own send is in
/// flight, whose paste reads as a draft, `owns_draft` says a settled send
/// may still own the draft the screen shows, which then shows no note, and
/// `prompt` is the card the chat shows, whose subject names an overlay.
#[must_use]
pub fn composer_note(
    connected: bool,
    state: Option<&ComposerState>,
    sending: bool,
    owns_draft: bool,
    prompt: Option<&ChatPrompt>,
) -> Option<ComposerNote> {
    if !connected {
        return Some(ComposerNote::NotConnected);
    }
    if sending {
        return Some(ComposerNote::Sending);
    }
    match state? {
        ComposerState::Draft if owns_draft => None,
        ComposerState::Draft => Some(ComposerNote::Draft),
        ComposerState::Overlay { .. } => Some(ComposerNote::Overlay(
            prompt
                .filter(|prompt| prompt.kind == ChatPromptKind::Overlay)
                .map(|prompt| prompt.subject.clone()),
        )),
        ComposerState::Unknown => Some(ComposerNote::Unknown),
        _ => None,
    }
}

/// What the composer's note row says over a draft on a host that can pull
/// or send it, beside its Pull and Send buttons.
pub const DRAFT_ACTIONS_NOTE: &str = "The prompt box has a draft.";
/// What the note row says over a draft on a host that can also clear it,
/// beside its Pull, Send and Clear buttons.
pub const CLEAR_ACTIONS_NOTE: &str = "The prompt box has a draft. Pull, send or clear it first.";
/// What the note row says while a pull of the draft is awaited.
pub const PULLING_NOTE: &str = "Pulling the draft\u{2026}";
/// What the composer says when a pull left some of the draft in the box.
pub const PULL_PARTIAL: &str =
    "Pulled, but the terminal's prompt box still holds some of the draft.";

/// The composer's note row as drawn: a plain note, or the draft's line
/// with its Pull and Send buttons, or the line a pending pull shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposerLine {
    /// A note, drawn as [`ComposerNote::text`].
    Note(ComposerNote),
    /// The draft's line with its Pull and Send buttons.
    DraftActions,
    /// The draft's line with its Pull, Send and Clear buttons.
    DraftClearActions,
    /// A pull of the draft is awaited; no buttons.
    Pulling,
}

impl ComposerLine {
    /// What the row says.
    #[must_use]
    pub fn text(&self) -> String {
        match self {
            Self::Note(note) => note.text(),
            Self::DraftActions => DRAFT_ACTIONS_NOTE.to_owned(),
            Self::DraftClearActions => CLEAR_ACTIONS_NOTE.to_owned(),
            Self::Pulling => PULLING_NOTE.to_owned(),
        }
    }

    /// Whether the row draws the draft's Pull and Send buttons.
    #[must_use]
    pub fn has_draft_actions(&self) -> bool {
        matches!(self, Self::DraftActions | Self::DraftClearActions)
    }

    /// Whether the row draws the draft's Clear beside Pull and Send.
    #[must_use]
    pub fn offers_clear(&self) -> bool {
        matches!(self, Self::DraftClearActions)
    }

    /// This line on a host that can clear the draft (`clear`): the draft's
    /// buttons gain Clear; any other line stays as it is.
    #[must_use]
    pub fn with_clear(self, clear: bool) -> Self {
        match self {
            Self::DraftActions if clear => Self::DraftClearActions,
            line => line,
        }
    }
}

/// The note row for `note`: a draft note on a host that can pull or send
/// the draft (`draft_actions`) gets the draft's buttons, or the pulling
/// line while a pull is awaited (`pulling`); any other note stays as it is.
#[must_use]
pub fn composer_line(
    note: Option<ComposerNote>,
    draft_actions: bool,
    pulling: bool,
) -> Option<ComposerLine> {
    match note? {
        ComposerNote::Draft if draft_actions && pulling => Some(ComposerLine::Pulling),
        ComposerNote::Draft if draft_actions => Some(ComposerLine::DraftActions),
        note => Some(ComposerLine::Note(note)),
    }
}

/// A session's pull of its prompt-box draft awaiting its answer: the pull's
/// request id, from the Pull click until a `ChatDraft` or a refusal with
/// that id ends it.
#[derive(Debug, Default)]
pub struct DraftPull {
    pending: Option<String>,
}

impl DraftPull {
    /// A pull went out under `request_id`.
    pub fn sent(&mut self, request_id: String) {
        self.pending = Some(request_id);
    }

    /// Whether a pull is awaited.
    #[must_use]
    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// An answer or refusal carrying `request_id` arrived: whether it ends
    /// the pending pull, which it then does. Anything else is not ours.
    pub fn answered(&mut self, request_id: Option<&str>) -> bool {
        let ours = request_id.is_some() && self.pending.as_deref() == request_id;
        if ours {
            self.pending = None;
        }
        ours
    }
}

/// A session's last clear of its prompt-box draft that may still be
/// refused: its request id, from the confirmed Clear until a refusal with
/// that id or the next clear. A clear that emptied the box is never answered.
#[derive(Debug, Default)]
pub struct DraftClear {
    last: Option<String>,
}

impl DraftClear {
    /// A clear went out under `request_id`.
    pub fn sent(&mut self, request_id: String) {
        self.last = Some(request_id);
    }

    /// A refusal carrying `request_id` arrived: whether it names the last
    /// clear, which it then ends.
    pub fn refused(&mut self, request_id: &str) -> bool {
        let ours = self.last.as_deref() == Some(request_id);
        if ours {
            self.last = None;
        }
        ours
    }
}

/// The completion trigger whose token searches the session's files.
pub const FILE_TRIGGER: char = '@';

/// How long after the `@` token last changed its file search goes out.
pub const FILE_SEARCH_DEBOUNCE: Duration = Duration::from_millis(150);

/// How many matches a file search asks for.
pub const FILE_SEARCH_LIMIT: u32 = 50;

/// One composer's search of its session's files for the `@` token the
/// caret sits in: the generation each change of that token bumps (a
/// debounce timer sends only while its generation is current), the request
/// id of the search awaiting its answer, and whether the popup's items are
/// that search's answer.
#[derive(Debug, Default)]
pub struct FileSearch {
    generation: u64,
    pending: Option<String>,
    in_token: bool,
    showing: bool,
}

impl FileSearch {
    /// The caret now sits in an `@` token, changed: the generation the new
    /// debounce timer carries, which makes every earlier one stale.
    pub fn token_changed(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.in_token = true;
        self.generation
    }

    /// The caret left the `@` token, or sits in another trigger's: every
    /// timer and the pending search go stale. Returns whether the popup's
    /// items came from the search, and so must go.
    pub fn token_left(&mut self) -> bool {
        self.generation = self.generation.wrapping_add(1);
        self.pending = None;
        self.in_token = false;
        std::mem::take(&mut self.showing)
    }

    /// Whether a debounce timer carrying `generation` may still send.
    #[must_use]
    pub fn is_current(&self, generation: u64) -> bool {
        self.in_token && generation == self.generation
    }

    /// A search went out under `request_id`; an answer to any earlier one
    /// no longer applies.
    pub fn sent(&mut self, request_id: String) {
        self.pending = Some(request_id);
    }

    /// An answer carrying `request_id` arrived: whether it answers the
    /// pending search while the caret is still in its token, in which case
    /// its matches become the popup's items.
    pub fn answered(&mut self, request_id: Option<&str>) -> bool {
        let ours = self.in_token && request_id.is_some() && self.pending.as_deref() == request_id;
        if ours {
            self.pending = None;
            self.showing = true;
        }
        ours
    }

    /// The popup's items were given from elsewhere: leaving the token no
    /// longer removes them.
    pub fn items_given(&mut self) {
        self.showing = false;
    }
}

/// The completion trigger whose token lists the session's commands and
/// skills.
pub const COMMAND_TRIGGER: char = '/';

/// How many rows the `/` filter gives at most.
pub const COMMAND_ROW_CAP: usize = 50;

/// What a `/` row says of an entry that opens an overlay or dialog in the
/// terminal.
pub const OPENS_TERMINAL_NOTE: &str = "opens in the terminal";

/// One composer's copy of its session's command catalog: the entries the
/// host answered with, fetched once and filtered locally for each `/`
/// token, the request id of the catalog request awaiting its answer, and
/// whether the popup's items are the catalog's rows.
#[derive(Debug, Default)]
pub struct CommandCatalog {
    entries: Option<Vec<CatalogEntry>>,
    pending: Option<String>,
    showing: bool,
}

impl CommandCatalog {
    /// Whether a `/` token must ask the host for the catalog: none is
    /// cached and none is awaited.
    #[must_use]
    pub fn wants_fetch(&self) -> bool {
        self.entries.is_none() && self.pending.is_none()
    }

    /// A catalog request went out under `request_id`; an answer to any
    /// other no longer applies.
    pub fn sent(&mut self, request_id: String) {
        self.pending = Some(request_id);
    }

    /// An answer carrying `request_id` arrived with `entries`: whether it
    /// answers the pending request, in which case its entries are cached.
    pub fn answered(&mut self, request_id: Option<&str>, entries: Vec<CatalogEntry>) -> bool {
        let ours = request_id.is_some() && self.pending.as_deref() == request_id;
        if ours {
            self.pending = None;
            self.entries = Some(entries);
        }
        ours
    }

    /// The rows for a `/` token holding `query`, from the cached catalog;
    /// None while nothing is cached. Rows given make the popup's items the
    /// catalog's.
    pub fn rows_for(&mut self, query: &str) -> Option<Vec<CompletionItem>> {
        let rows = slash_rows(self.entries.as_deref()?, query);
        self.showing = true;
        Some(rows)
    }

    /// The caret left the `/` token, or sits in another trigger's: returns
    /// whether the popup's items came from the catalog, and so must go.
    pub fn token_left(&mut self) -> bool {
        std::mem::take(&mut self.showing)
    }

    /// The popup's items were given from elsewhere: leaving the token no
    /// longer removes them.
    pub fn items_given(&mut self) {
        self.showing = false;
    }

    /// The chat started over: the cached catalog and the pending request
    /// are dropped, so the next `/` token asks again. Rows already shown
    /// stay until the caret leaves their token.
    pub fn forget(&mut self) {
        self.entries = None;
        self.pending = None;
    }
}

/// The `/` popup's rows for `query` over `entries`: those whose name starts
/// with it (ignoring case), then those that only contain it, each group in
/// catalog order, at most [`COMMAND_ROW_CAP`]. A row shows `/name`, the
/// entry's description and [`OPENS_TERMINAL_NOTE`] when they apply, and
/// inserts `/name`.
#[must_use]
pub fn slash_rows(entries: &[CatalogEntry], query: &str) -> Vec<CompletionItem> {
    let query = query.to_lowercase();
    let lowered: Vec<String> = entries
        .iter()
        .map(|entry| entry.name.to_lowercase())
        .collect();
    let prefixed = entries
        .iter()
        .zip(&lowered)
        .filter(|(_, name)| name.starts_with(&query));
    let contained = entries
        .iter()
        .zip(&lowered)
        .filter(|(_, name)| !name.starts_with(&query) && name.contains(&query));
    prefixed
        .chain(contained)
        .take(COMMAND_ROW_CAP)
        .map(|(entry, _)| slash_row(entry))
        .collect()
}

/// One entry's `/` row: `/name`, then its description and
/// [`OPENS_TERMINAL_NOTE`] when they apply; it inserts `/name`.
fn slash_row(entry: &CatalogEntry) -> CompletionItem {
    let insert = format!("{COMMAND_TRIGGER}{}", entry.name);
    let mut label = insert.clone();
    if let Some(description) = entry.description.as_deref() {
        label.push_str(STRIP_SEPARATOR);
        label.push_str(description);
    }
    if entry.opens_terminal {
        label.push_str(STRIP_SEPARATOR);
        label.push_str(OPENS_TERMINAL_NOTE);
    }
    CompletionItem::new(label, insert)
}

/// What the composer says after a pull answered with `cleared`: none when
/// the box was cleared, else that some of the draft stayed in it.
#[must_use]
pub fn pulled_error(cleared: bool) -> Option<String> {
    (!cleared).then(|| PULL_PARTIAL.to_owned())
}

/// Whether a suggested next prompt was cut short on screen: it ends in an
/// ellipsis, so sending or pulling it would take half a prompt.
#[must_use]
pub fn suggestion_is_cut(text: &str) -> bool {
    text.ends_with('\u{2026}')
}

/// The suggestion chip above the composer: the suggested prompt, and
/// whether its Send and Pull buttons show (not for a cut suggestion).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuggestionChip<'a> {
    pub text: &'a str,
    pub actions: bool,
}

/// The chip the composer draws for `state`: one for a suggestion, while no
/// card is up (`card_up`) and no send is in flight (`sending`).
#[must_use]
pub fn suggestion_chip(
    state: Option<&ComposerState>,
    card_up: bool,
    sending: bool,
) -> Option<SuggestionChip<'_>> {
    let Some(ComposerState::Suggestion { text }) = state else {
        return None;
    };
    (!card_up && !sending).then(|| SuggestionChip {
        text,
        actions: !suggestion_is_cut(text),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::time::{Duration, Instant};

    use protocol::{
        ChatAnswerKind, ChatEntry, ChatOption, ChatPrompt, ChatPromptKind, ChatScreen,
        ComposerState, ToolOutcome,
    };

    use super::{
        ChatLoads, ChatLog, DIGIT_GRACE, DISPLAY_CAP, LoadChanges, PaneView, PaneViews,
        PromptStack, ViewKey, allow_with_text, back_answer, deny_with_text, entry_id,
        follows_bottom, option_answer, reveal_answer, skip_answer, submit_answer, toggle_answer,
        tool_one_liner, type_something_answer,
    };

    /// A message row, the simplest entry with an id.
    fn user(id: &str) -> ChatEntry {
        ChatEntry::User {
            id: id.to_owned(),
            text: format!("message {id}"),
            pending: false,
        }
    }

    /// One answer row, live or complete.
    fn answer(id: &str, live: bool) -> ChatEntry {
        ChatEntry::Answer {
            id: id.to_owned(),
            text: format!("answer {id}"),
            live,
        }
    }

    #[test]
    fn only_the_live_answer_with_open_stretches_passes_them_on() {
        let mut log = ChatLog::default();
        log.load(
            vec![
                answer("a0", false),
                user("u1"),
                answer("live:m1", true),
                answer("live:m2", true),
            ],
            0,
        );
        log.remove(&["a0".to_owned()]);
        assert!(log.toggle_stretchtext("live:m2", "k"));
        log.append(
            vec![
                answer("a0", false),
                answer("a1", false),
                answer("a2", false),
            ],
            &["live:m1".to_owned(), "live:m2".to_owned()],
        );
        assert_eq!(log.open_stretchtext("a0"), None, "a gone id stays gone");
        assert_eq!(
            log.open_stretchtext("a1"),
            None,
            "the first live answer had nothing open"
        );
        assert_eq!(
            log.open_stretchtext("a2"),
            Some(&BTreeSet::from(["k".to_owned()])),
            "the second final takes the second live answer's stretches"
        );
    }

    /// One tool-call row, with what the call returned once it has.
    fn tool(id: &str, result: Option<ToolOutcome>) -> ChatEntry {
        ChatEntry::Tool {
            id: id.to_owned(),
            name: "Read".to_owned(),
            summary: "src/chat.rs".to_owned(),
            input: "{}".to_owned(),
            result,
            parent_id: None,
            tail_paths: Vec::new(),
            kind: protocol::ToolKind::Other,
            script: None,
        }
    }

    /// The ids of the log's rows, in display order.
    fn ids(log: &ChatLog) -> Vec<String> {
        log.entries()
            .filter_map(entry_id)
            .map(ToOwned::to_owned)
            .collect()
    }

    /// A prompt of `kind` listing the numbered options `numbers`.
    fn test_prompt(id: &str, kind: ChatPromptKind, can_amend: bool, numbers: &[u32]) -> ChatPrompt {
        ChatPrompt {
            id: id.to_owned(),
            kind,
            subject: "Approve the edit".to_owned(),
            body: String::new(),
            options: numbers
                .iter()
                .map(|&number| ChatOption {
                    number,
                    label: format!("option {number}"),
                    description: None,
                    selected: false,
                })
                .collect(),
            questions: Vec::new(),
            can_amend,
            always_allow_tip: None,
            multi: false,
            cursor: None,
            adjust: None,
            preview: None,
        }
    }

    /// A dialog screen showing `prompt`, or none.
    fn screen(prompt: Option<ChatPrompt>) -> ChatScreen {
        ChatScreen {
            composer: ComposerState::Dialog,
            prompt,
        }
    }

    /// A user row the daemon echoed for a prompt still waiting on the
    /// running turn.
    fn pending_user(id: &str) -> ChatEntry {
        ChatEntry::User {
            id: id.to_owned(),
            text: format!("message {id}"),
            pending: true,
        }
    }

    #[test]
    fn a_pending_prompt_stays_below_entries_that_arrive_after_it() {
        let mut log = ChatLog::default();
        log.upsert(vec![user("u-1")]);
        log.upsert(vec![pending_user("pending:1")]);
        log.upsert(vec![answer("a-1", false), tool("t-1", None)]);
        assert_eq!(ids(&log), ["u-1", "a-1", "t-1", "pending:1"]);
    }

    #[test]
    fn two_pending_prompts_keep_their_order() {
        let mut log = ChatLog::default();
        log.upsert(vec![pending_user("pending:1"), pending_user("pending:2")]);
        log.upsert(vec![answer("a-1", false)]);
        assert_eq!(ids(&log), ["a-1", "pending:1", "pending:2"]);
    }

    #[test]
    fn a_live_answer_lands_above_a_pending_prompt() {
        let mut log = ChatLog::default();
        log.upsert(vec![pending_user("pending:1")]);
        log.upsert(vec![answer("live:m1", true)]);
        assert_eq!(ids(&log), ["live:m1", "pending:1"]);
    }

    #[test]
    fn a_removed_pending_prompt_leaves_the_next_one_waiting() {
        let mut log = ChatLog::default();
        log.upsert(vec![
            user("u-1"),
            pending_user("pending:1"),
            pending_user("pending:2"),
        ]);
        log.append(vec![user("u-2")], &["pending:1".to_owned()]);
        assert_eq!(ids(&log), ["u-1", "u-2", "pending:2"]);
    }

    /// A log holding one row past the display cap: `row0` dropped off the
    /// top, `row1` to `row500` kept.
    fn over_cap() -> ChatLog {
        let mut log = ChatLog::default();
        log.upsert(
            (0..=DISPLAY_CAP)
                .map(|n| tool(&format!("row{n}"), None))
                .collect(),
        );
        log
    }

    fn done(text: &str) -> ToolOutcome {
        ToolOutcome {
            text: text.to_owned(),
            is_error: false,
        }
    }

    fn sessions(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|id| (*id).to_owned()).collect()
    }

    fn changes(load: &[&str], unload: &[&str]) -> LoadChanges {
        LoadChanges {
            load: load.iter().map(|id| (*id).to_owned()).collect(),
            unload: unload.iter().map(|id| (*id).to_owned()).collect(),
        }
    }

    #[test]
    fn a_late_update_for_a_row_trimmed_off_the_top_is_ignored() {
        let mut log = over_cap();
        assert_eq!(log.hidden(), 1, "the oldest row dropped off the top");
        assert!(log.get("row0").is_none());
        log.upsert(vec![tool("row0", Some(done("late result")))]);
        assert_eq!(log.len(), DISPLAY_CAP, "the late update adds no row");
        assert!(log.get("row0").is_none(), "the trimmed row stays gone");
        assert_eq!(log.hidden(), 1, "nothing more is counted hidden");
        let kept = ids(&log);
        assert_eq!(
            kept.first().map(String::as_str),
            Some("row1"),
            "no other row dropped"
        );
        let newest = format!("row{DISPLAY_CAP}");
        assert_eq!(kept.last(), Some(&newest), "the newest row stays the last");
    }

    #[test]
    fn an_update_for_a_row_still_held_replaces_it_past_the_cap() {
        let mut log = over_cap();
        log.upsert(vec![tool("row7", Some(done("result")))]);
        assert!(matches!(
            log.get("row7"),
            Some(ChatEntry::Tool {
                result: Some(_),
                ..
            })
        ));
        assert_eq!(log.len(), DISPLAY_CAP);
    }

    #[test]
    fn a_load_accepts_an_id_trimmed_or_removed_before_it() {
        let mut log = over_cap();
        log.upsert(vec![user("gone")]);
        log.remove(&["gone".to_owned()]);
        log.load(Vec::new(), 0);
        log.upsert(vec![user("row0"), user("gone")]);
        assert_eq!(ids(&log), ["row0", "gone"]);
    }

    #[test]
    fn remove_drops_rows_and_their_expansion_and_keeps_them_gone() {
        let mut log = ChatLog::default();
        log.upsert(vec![tool("t1", None), user("u1"), tool("t2", None)]);
        assert!(log.toggle_expanded("t1"));
        log.remove(&["t1".to_owned(), "unknown".to_owned()]);
        assert_eq!(ids(&log), ["u1", "t2"]);
        assert!(!log.is_expanded("t1"));
        assert_eq!(log.hidden(), 0, "a removed row is not hidden");
        log.upsert(vec![tool("t1", Some(done("late")))]);
        assert_eq!(ids(&log), ["u1", "t2"], "a late update does not revive it");
    }

    #[test]
    fn an_append_removes_before_it_upserts() {
        let mut log = ChatLog::default();
        log.upsert(vec![user("u1"), answer("live:m1", true), user("u2")]);
        log.append(vec![answer("a1", false)], &["live:m1".to_owned()]);
        assert_eq!(
            ids(&log),
            ["u1", "u2", "a1"],
            "the transcript answer replaces the live one"
        );
        log.append(vec![answer("u2", false)], &["u2".to_owned()]);
        assert_eq!(
            ids(&log),
            ["u1", "a1", "u2"],
            "an id removed and carried in one append comes back at the bottom"
        );
    }

    #[test]
    fn a_reset_keeps_the_expansion_of_the_cards_that_survive_it() {
        let mut log = ChatLog::default();
        log.upsert(vec![tool("t1", None), tool("t2", None), user("gone")]);
        log.toggle_expanded("t1");
        log.toggle_expanded("t2");
        log.remove(&["gone".to_owned()]);
        log.reset(vec![tool("t1", Some(done("ok"))), user("u1")], 3);
        assert_eq!(ids(&log), ["t1", "u1"]);
        assert_eq!(log.hidden(), 3);
        assert!(log.is_expanded("t1"), "a surviving card stays open");
        assert!(
            !log.is_expanded("t2"),
            "a card the reset dropped is forgotten"
        );
        log.upsert(vec![tool("t2", None), user("gone")]);
        assert_eq!(ids(&log), ["t1", "u1", "t2", "gone"]);
        assert!(
            !log.is_expanded("t2"),
            "a card that comes back starts closed"
        );
    }

    /// Presses `key` on pane `pane_id`, returning what it shows now.
    fn press(views: &mut PaneViews, pane_id: &str, key: ViewKey) -> PaneView {
        let next = views.get(pane_id).after_key(key);
        views.set(pane_id, next);
        next
    }

    #[test]
    fn a_pane_shows_its_terminal_until_toggled() {
        let mut views = PaneViews::default();
        assert_eq!(views.get("p1"), PaneView::Terminal);
        assert_eq!(press(&mut views, "p1", ViewKey::Chat), PaneView::Chat);
        assert!(views.is_chat("p1"));
        assert!(!views.is_chat("p2"), "another pane keeps its terminal");
        assert_eq!(press(&mut views, "p1", ViewKey::Chat), PaneView::Terminal);
        assert!(!views.is_chat("p1"));
        views.set("p2", PaneView::Chat);
        assert!(views.is_chat("p2"));
    }

    #[test]
    fn ctrl_shift_m_flips_terminal_and_chat_and_leaves_the_summary_for_the_terminal() {
        assert_eq!(PaneView::Terminal.after_key(ViewKey::Chat), PaneView::Chat);
        assert_eq!(PaneView::Chat.after_key(ViewKey::Chat), PaneView::Terminal);
        assert_eq!(
            PaneView::Summary.after_key(ViewKey::Chat),
            PaneView::Terminal
        );
    }

    #[test]
    fn ctrl_shift_u_opens_the_summary_and_leaves_it_for_the_terminal() {
        assert_eq!(
            PaneView::Terminal.after_key(ViewKey::Summary),
            PaneView::Summary
        );
        assert_eq!(
            PaneView::Chat.after_key(ViewKey::Summary),
            PaneView::Summary
        );
        assert_eq!(
            PaneView::Summary.after_key(ViewKey::Summary),
            PaneView::Terminal
        );
    }

    #[test]
    fn a_summary_pane_keeps_its_view_and_is_no_chat() {
        let mut views = PaneViews::default();
        views.set("p1", PaneView::Summary);
        assert_eq!(views.get("p1"), PaneView::Summary);
        assert!(!views.is_chat("p1"));
        views.rekey("p1", "p9");
        assert_eq!(views.get("p9"), PaneView::Summary);
        assert_eq!(views.get("p1"), PaneView::Terminal);
        views.set("p9", PaneView::Terminal);
        assert_eq!(views.get("p9"), PaneView::Terminal);
    }

    #[test]
    fn a_rekeyed_pane_keeps_its_view() {
        let mut views = PaneViews::default();
        views.set("old", PaneView::Chat);
        views.rekey("old", "new");
        assert!(views.is_chat("new"));
        assert!(!views.is_chat("old"));
        views.rekey("terminal", "moved");
        assert!(
            !views.is_chat("moved"),
            "a terminal pane moves as a terminal"
        );
    }

    #[test]
    fn a_pane_that_is_gone_forgets_its_view() {
        let mut views = PaneViews::default();
        views.set("p1", PaneView::Chat);
        views.set("p2", PaneView::Chat);
        views.retain(|pane_id| pane_id == "p2");
        assert!(!views.is_chat("p1"));
        assert!(views.is_chat("p2"));
    }

    #[test]
    fn the_first_chat_pane_of_a_session_loads_it_and_the_last_unloads_it() {
        let mut loads = ChatLoads::default();
        assert_eq!(loads.diff(sessions(&["s1"])), changes(&["s1"], &[]));
        assert_eq!(
            loads.diff(sessions(&["s1"])),
            changes(&[], &[]),
            "a session already loaded is not asked for again"
        );
        assert_eq!(loads.diff(sessions(&["s1", "s2"])), changes(&["s2"], &[]));
        assert_eq!(loads.diff(sessions(&["s2"])), changes(&[], &["s1"]));
        assert_eq!(loads.diff(BTreeSet::new()), changes(&[], &["s2"]));
    }

    #[test]
    fn a_new_connection_asks_for_every_wanted_chat_again_and_unloads_none() {
        let mut loads = ChatLoads::default();
        loads.diff(sessions(&["s1", "s2"]));
        loads.clear();
        assert_eq!(loads.diff(sessions(&["s1"])), changes(&["s1"], &[]));
        loads.clear();
        assert_eq!(loads.diff(BTreeSet::new()), changes(&[], &[]));
    }

    #[test]
    fn a_forgotten_load_is_never_unloaded() {
        let mut loads = ChatLoads::default();
        loads.diff(sessions(&["s1"]));
        loads.forget("s1");
        assert_eq!(loads.diff(BTreeSet::new()), changes(&[], &[]));
    }

    #[test]
    fn the_chat_follows_its_bottom_until_scrolled_up() {
        assert!(
            follows_bottom(None),
            "a chat not laid out yet starts at the bottom"
        );
        assert!(follows_bottom(Some((0.0, 0.0))), "content that fits");
        assert!(follows_bottom(Some((400.0, 400.0))), "at the bottom");
        assert!(follows_bottom(Some((399.5, 400.0))), "within a pixel of it");
        assert!(!follows_bottom(Some((300.0, 400.0))), "scrolled up");
        assert!(
            !follows_bottom(Some((0.0, 400.0))),
            "at the top of a long chat"
        );
    }

    #[test]
    fn an_upsert_appends_new_ids_in_arrival_order() {
        let mut log = ChatLog::default();
        assert!(log.is_empty(), "a new log holds no rows");
        log.upsert(vec![user("a"), user("b")]);
        assert!(!log.is_empty());
        log.upsert(vec![user("c")]);
        assert_eq!(ids(&log), ["a", "b", "c"]);
        assert_eq!(log.len(), 3);
        assert_eq!(log.hidden(), 0);
    }

    #[test]
    fn an_upsert_replaces_a_known_id_in_its_place() {
        let mut log = ChatLog::default();
        log.upsert(vec![user("a"), answer("b", true), user("c")]);
        log.upsert(vec![answer("b", false)]);
        assert_eq!(
            ids(&log),
            ["a", "b", "c"],
            "the replaced row keeps its place"
        );
        assert_eq!(log.len(), 3, "the replacement adds no row");
        assert!(
            matches!(log.get("b"), Some(ChatEntry::Answer { live: false, .. })),
            "the transcript's record replaces the live answer"
        );
    }

    #[test]
    fn an_upsert_keeps_a_tool_card_expanded() {
        let mut log = ChatLog::default();
        log.upsert(vec![tool("t", None)]);
        assert!(log.toggle_expanded("t"));
        log.upsert(vec![tool(
            "t",
            Some(ToolOutcome {
                text: "ok".to_owned(),
                is_error: false,
            }),
        )]);
        assert!(
            log.is_expanded("t"),
            "the result lands on a card already open"
        );
        assert!(matches!(
            log.get("t"),
            Some(ChatEntry::Tool {
                result: Some(_),
                ..
            })
        ));
    }

    #[test]
    fn an_upsert_skips_an_unknown_entry() {
        let mut log = ChatLog::default();
        log.upsert(vec![user("a"), ChatEntry::Unknown]);
        assert_eq!(ids(&log), ["a"]);
        assert_eq!(log.len(), 1);
        assert_eq!(entry_id(&ChatEntry::Unknown), None);
    }

    #[test]
    fn the_cap_drops_the_oldest_rows_and_counts_them_hidden() {
        let mut log = ChatLog::default();
        let rows: Vec<ChatEntry> = (0..DISPLAY_CAP + 2)
            .map(|n| user(&format!("row{n}")))
            .collect();
        log.upsert(rows);
        assert_eq!(log.len(), DISPLAY_CAP);
        assert_eq!(log.hidden(), 2);
        assert!(log.get("row0").is_none(), "the oldest row is past the cap");
        assert_eq!(
            ids(&log).first().map(String::as_str),
            Some("row2"),
            "the rows past the cap went first"
        );
        let newest = format!("row{}", DISPLAY_CAP + 1);
        assert_eq!(ids(&log).last().map(String::as_str), Some(newest.as_str()));
    }

    #[test]
    fn a_load_replaces_everything_and_takes_its_hidden_count() {
        let mut log = ChatLog::default();
        log.upsert(vec![user("old")]);
        log.load(vec![user("a"), user("b")], 40);
        assert_eq!(ids(&log), ["a", "b"]);
        assert_eq!(log.hidden(), 40, "the reply's count is the log's");
        assert!(log.get("old").is_none());
    }

    #[test]
    fn a_dropped_rows_expansion_is_forgotten() {
        let mut log = ChatLog::default();
        log.upsert(vec![tool("t", None)]);
        assert!(log.toggle_expanded("t"));
        let rows: Vec<ChatEntry> = (0..DISPLAY_CAP).map(|n| user(&format!("row{n}"))).collect();
        log.upsert(rows);
        assert!(log.get("t").is_none(), "the cap dropped the card");
        assert!(!log.is_expanded("t"), "its expansion went with it");
        assert_eq!(log.hidden(), 1);
    }

    #[test]
    fn an_expansion_follows_only_tool_ids() {
        let mut log = ChatLog::default();
        log.upsert(vec![user("u"), tool("t", None)]);
        assert!(
            !log.toggle_expanded("u"),
            "a message row has nothing to open"
        );
        assert!(!log.is_expanded("u"));
        assert!(
            !log.toggle_expanded("missing"),
            "an id the log does not hold has nothing to open"
        );
        assert!(log.toggle_expanded("t"), "the first click opens the card");
        assert!(log.is_expanded("t"));
        assert!(!log.toggle_expanded("t"), "the second closes it");
        assert!(!log.is_expanded("t"));
    }

    #[test]
    fn a_one_liner_is_the_first_line_of_the_summary() {
        assert_eq!(tool_one_liner("src/chat.rs", "Read"), "src/chat.rs");
        assert_eq!(
            tool_one_liner("cargo nextest run -p protocol", "Bash"),
            "cargo nextest run -p protocol"
        );
        assert_eq!(tool_one_liner(r"fn\s+main", "Grep"), r"fn\s+main");
        assert_eq!(tool_one_liner("  src/a.rs  ", "Read"), "src/a.rs");
        assert_eq!(
            tool_one_liner("src/a.rs\nsecond line", "Read"),
            "src/a.rs",
            "only the first line shows"
        );
        assert_eq!(
            tool_one_liner("", "Read"),
            "",
            "an empty summary says nothing"
        );
        assert_eq!(
            tool_one_liner("ExitPlanMode", "ExitPlanMode"),
            "",
            "a summary that is only the name is drawn once"
        );
        assert_eq!(tool_one_liner("  ToolSearch  ", "ToolSearch"), "");
    }

    #[test]
    fn a_long_one_liner_ends_in_an_ellipsis_at_the_cap() {
        let long = "x".repeat(100);
        let cut = tool_one_liner(&long, "Read");
        assert_eq!(
            cut.chars().count(),
            80,
            "the ellipsis counts toward the cap"
        );
        assert!(cut.ends_with('\u{2026}'));
        assert_eq!(cut.chars().take(79).collect::<String>(), "x".repeat(79));
        let exact = "y".repeat(80);
        assert_eq!(tool_one_liner(&exact, "Read"), exact, "80 characters fit");
        let wide = "\u{20ac}".repeat(100);
        let cut = tool_one_liner(&wide, "Read");
        assert_eq!(cut.chars().count(), 80, "characters, not bytes");
        assert_eq!(
            cut.chars().take(79).collect::<String>(),
            "\u{20ac}".repeat(79)
        );
    }

    #[test]
    fn an_empty_summary_draws_no_one_liner() {
        assert_eq!(tool_one_liner("", "Read"), "");
        assert_eq!(tool_one_liner("   ", "Bash"), "");
    }

    #[test]
    fn the_builders_pick_the_kind_the_prompt_takes() {
        let permission = test_prompt("p", ChatPromptKind::Permission, true, &[1, 2]);
        let question = test_prompt("q", ChatPromptKind::Question, true, &[1]);
        assert_eq!(
            option_answer(&permission, 2),
            Some(ChatAnswerKind::Option { number: 2 })
        );
        assert_eq!(
            toggle_answer(&permission, 1),
            Some(ChatAnswerKind::Toggle { number: 1 })
        );
        assert_eq!(submit_answer(), ChatAnswerKind::Submit);
        assert_eq!(back_answer(), ChatAnswerKind::Back);
        assert_eq!(skip_answer(), ChatAnswerKind::Skip);
        assert_eq!(reveal_answer(), ChatAnswerKind::Reveal);
        assert_eq!(
            type_something_answer("a hint"),
            Some(ChatAnswerKind::TypeSomething {
                text: "a hint".to_owned()
            })
        );
        assert_eq!(
            allow_with_text(&permission, "only src"),
            Some(ChatAnswerKind::AllowWithText {
                text: "only src".to_owned()
            })
        );
        assert_eq!(
            allow_with_text(&question, "a note"),
            Some(ChatAnswerKind::AllowWithText {
                text: "a note".to_owned()
            }),
            "an amendable question takes a note too"
        );
        assert_eq!(
            deny_with_text(&permission, "not that file"),
            Some(ChatAnswerKind::DenyWithText {
                text: "not that file".to_owned()
            })
        );
        let pasted = "  first line\n  second line  ";
        assert_eq!(
            type_something_answer(pasted),
            Some(ChatAnswerKind::TypeSomething {
                text: pasted.to_owned()
            }),
            "the text is kept as typed"
        );
    }

    #[test]
    fn the_builders_refuse_what_the_prompt_cannot_take() {
        let permission = test_prompt("p", ChatPromptKind::Permission, true, &[1, 2]);
        let fixed = test_prompt("f", ChatPromptKind::Question, false, &[1]);
        let open_question = test_prompt("q", ChatPromptKind::Question, true, &[1]);
        assert_eq!(
            option_answer(&permission, 3),
            None,
            "the prompt lists no option 3"
        );
        assert_eq!(toggle_answer(&permission, 0), None);
        assert_eq!(
            type_something_answer("   "),
            None,
            "blank text is no answer"
        );
        assert_eq!(type_something_answer("\n\t "), None);
        assert_eq!(
            allow_with_text(&fixed, "a note"),
            None,
            "the prompt takes no amendment"
        );
        assert_eq!(allow_with_text(&permission, "  "), None);
        assert_eq!(
            deny_with_text(&open_question, "a note"),
            None,
            "only a permission prompt takes a deny note"
        );
        assert_eq!(deny_with_text(&permission, " "), None);
    }

    #[test]
    fn a_digit_answers_from_an_empty_composer_only() {
        let t0 = Instant::now();
        let mut stack = PromptStack::default();
        stack.on_screen(
            &screen(Some(test_prompt(
                "p1",
                ChatPromptKind::Permission,
                false,
                &[1, 2],
            ))),
            t0,
        );
        let after = t0 + DIGIT_GRACE;
        assert_eq!(
            stack.digit_answer(2, true, false, after),
            Some(ChatAnswerKind::Option { number: 2 }),
            "a single choice is an option"
        );
        assert_eq!(
            stack.digit_answer(1, true, true, after),
            Some(ChatAnswerKind::Toggle { number: 1 }),
            "a multi-select digit flips a box"
        );
        assert_eq!(
            stack.digit_answer(2, false, false, after),
            None,
            "a composer holding text keeps its digits"
        );
        assert_eq!(
            stack.digit_answer(3, true, false, after),
            None,
            "the prompt lists no option 3"
        );
    }

    #[test]
    fn a_digit_without_a_prompt_is_no_answer() {
        let stack = PromptStack::default();
        assert_eq!(stack.digit_answer(1, true, false, Instant::now()), None);
        assert_eq!(stack.grace_ends_at(), None);
        assert!(stack.current().is_none());
        assert!(stack.ghosts().is_empty(), "no ghost is left");
    }

    #[test]
    fn a_digit_inside_the_grace_is_dropped() {
        let t0 = Instant::now();
        let mut stack = PromptStack::default();
        stack.on_screen(
            &screen(Some(test_prompt(
                "p1",
                ChatPromptKind::Permission,
                false,
                &[1],
            ))),
            t0,
        );
        assert_eq!(stack.grace_ends_at(), Some(t0 + DIGIT_GRACE));
        assert_eq!(
            stack.digit_answer(1, true, false, t0 + Duration::from_millis(399)),
            None,
            "399 ms is still inside the grace"
        );
        assert_eq!(
            stack.digit_answer(1, true, false, t0 + DIGIT_GRACE),
            Some(ChatAnswerKind::Option { number: 1 }),
            "the grace ends at 400 ms"
        );
    }

    #[test]
    fn the_grace_restarts_only_for_a_new_prompt_id() {
        let t0 = Instant::now();
        let mut stack = PromptStack::default();
        let first = screen(Some(test_prompt(
            "p1",
            ChatPromptKind::Permission,
            false,
            &[1],
        )));
        stack.on_screen(&first, t0);
        stack.on_screen(&first, t0 + Duration::from_millis(300));
        assert_eq!(
            stack.grace_ends_at(),
            Some(t0 + DIGIT_GRACE),
            "the same prompt re-sent keeps its stamp"
        );
        let later = t0 + Duration::from_millis(300);
        stack.on_screen(
            &screen(Some(test_prompt(
                "p2",
                ChatPromptKind::Permission,
                false,
                &[1],
            ))),
            later,
        );
        assert_eq!(
            stack.grace_ends_at(),
            Some(later + DIGIT_GRACE),
            "a new prompt starts its own grace"
        );
        assert_eq!(
            stack.digit_answer(1, true, false, later + Duration::from_millis(399)),
            None
        );
        assert_eq!(
            stack.digit_answer(1, true, false, later + DIGIT_GRACE),
            Some(ChatAnswerKind::Option { number: 1 })
        );
    }

    #[test]
    fn a_rereading_of_the_same_prompt_updates_its_content_and_keeps_its_grace() {
        let t0 = Instant::now();
        let mut stack = PromptStack::default();
        let unticked = test_prompt("p1", ChatPromptKind::Permission, false, &[1, 2]);
        stack.on_screen(&screen(Some(unticked)), t0);
        let mut ticked = test_prompt("p1", ChatPromptKind::Permission, false, &[1, 2]);
        ticked.options[0].selected = true;
        stack.on_screen(&screen(Some(ticked)), t0 + Duration::from_millis(500));
        assert!(
            stack
                .current()
                .is_some_and(|prompt| prompt.options[0].selected),
            "the re-read card shows the ticked option"
        );
        assert_eq!(
            stack.grace_ends_at(),
            Some(t0 + DIGIT_GRACE),
            "the re-read keeps the card's first stamp"
        );
    }

    #[test]
    fn a_new_prompt_stacks_the_one_before_it_behind() {
        let t0 = Instant::now();
        let mut stack = PromptStack::default();
        stack.on_screen(
            &screen(Some(test_prompt(
                "p1",
                ChatPromptKind::Permission,
                false,
                &[1],
            ))),
            t0,
        );
        stack.on_screen(
            &screen(Some(test_prompt(
                "p2",
                ChatPromptKind::Question,
                false,
                &[1],
            ))),
            t0 + Duration::from_millis(500),
        );
        assert_eq!(stack.current().map(|prompt| prompt.id.as_str()), Some("p2"));
        assert_eq!(
            stack
                .ghosts()
                .iter()
                .map(|prompt| prompt.id.as_str())
                .collect::<Vec<_>>(),
            ["p1"],
            "the card before it is the ghost"
        );
        let mut at = t0 + Duration::from_millis(500);
        for id in ["p3", "p4", "p5", "p6"] {
            at += Duration::from_millis(500);
            stack.on_screen(
                &screen(Some(test_prompt(id, ChatPromptKind::Question, false, &[1]))),
                at,
            );
        }
        assert_eq!(
            stack
                .ghosts()
                .iter()
                .map(|prompt| prompt.id.as_str())
                .collect::<Vec<_>>(),
            ["p5", "p4", "p3"],
            "at most three ghosts, newest first"
        );
    }

    #[test]
    fn a_prompt_that_returns_leaves_the_ghosts() {
        let t0 = Instant::now();
        let mut stack = PromptStack::default();
        let first = screen(Some(test_prompt(
            "p1",
            ChatPromptKind::Permission,
            false,
            &[1],
        )));
        let middle = screen(Some(test_prompt(
            "p2",
            ChatPromptKind::Question,
            false,
            &[1],
        )));
        stack.on_screen(&first, t0);
        stack.on_screen(&middle, t0 + Duration::from_millis(200));
        stack.on_screen(&first, t0 + Duration::from_millis(400));
        assert_eq!(stack.current().map(|prompt| prompt.id.as_str()), Some("p1"));
        assert_eq!(
            stack
                .ghosts()
                .iter()
                .map(|prompt| prompt.id.as_str())
                .collect::<Vec<_>>(),
            ["p2"],
            "the returning card is not behind itself"
        );
    }

    #[test]
    fn no_prompt_clears_the_card_and_every_ghost() {
        let t0 = Instant::now();
        let mut stack = PromptStack::default();
        stack.on_screen(
            &screen(Some(test_prompt(
                "p1",
                ChatPromptKind::Permission,
                false,
                &[1],
            ))),
            t0,
        );
        stack.on_screen(
            &screen(Some(test_prompt(
                "p2",
                ChatPromptKind::Permission,
                false,
                &[1],
            ))),
            t0,
        );
        stack.on_screen(&screen(None), t0);
        assert!(stack.current().is_none());
        assert!(stack.ghosts().is_empty(), "no ghost is left");
        assert_eq!(stack.grace_ends_at(), None);
        assert_eq!(stack.digit_answer(1, true, false, t0 + DIGIT_GRACE), None);
    }
}

#[cfg(test)]
mod strip_tests {
    use std::time::{Duration, Instant};

    use chrono::{DateTime, TimeDelta, TimeZone, Utc};
    use protocol::{
        ChatEntry, ChatOption, ChatPrompt, ChatPromptKind, ClientMessage, ComposerState, WorkShell,
        WorkSubagent,
    };

    use super::{RunningWork, SCREEN_READ_TIP, Strip, StripKind};

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 30, 12, 0, 0)
            .single()
            .unwrap_or_default()
    }

    fn minutes(n: i64) -> TimeDelta {
        TimeDelta::minutes(n)
    }

    fn shell(command: Option<&str>, description: &str, age: Option<TimeDelta>) -> WorkShell {
        WorkShell {
            id: "b1".to_owned(),
            description: description.to_owned(),
            command: command.map(str::to_owned),
            since: age.map(|age| now() - age),
            from_screen: false,
        }
    }

    fn screen_shell() -> WorkShell {
        WorkShell {
            id: String::new(),
            description: String::new(),
            command: None,
            since: None,
            from_screen: true,
        }
    }

    fn subagent(description: &str, agent_type: Option<&str>) -> WorkSubagent {
        WorkSubagent {
            id: "a1".to_owned(),
            description: description.to_owned(),
            agent_type: agent_type.map(str::to_owned),
            since: Some(now() - minutes(2)),
            from_screen: false,
        }
    }

    fn strip(
        subagents: Vec<WorkSubagent>,
        shells: Vec<WorkShell>,
        wakeup_at: Option<DateTime<Utc>>,
    ) -> Option<Strip> {
        let mut work = RunningWork::default();
        work.set(subagents, shells, wakeup_at);
        work.strip(now())
    }

    fn text(
        subagents: Vec<WorkSubagent>,
        shells: Vec<WorkShell>,
        wakeup_at: Option<DateTime<Utc>>,
    ) -> Option<String> {
        strip(subagents, shells, wakeup_at).map(|strip| strip.text)
    }

    #[test]
    fn one_shell_reads_its_command_and_age() {
        let shells = vec![shell(Some("cargo test"), "Run the tests", Some(minutes(3)))];
        assert_eq!(
            text(vec![], shells, None).as_deref(),
            Some("1 background shell (cargo test, 3m)")
        );
    }

    #[test]
    fn a_long_command_is_cut_to_24_characters() {
        let command = "cargo nextest run --workspace --cargo-profile test-release";
        let shells = vec![shell(Some(command), "", Some(minutes(3)))];
        assert_eq!(
            text(vec![], shells, None).as_deref(),
            Some("1 background shell (cargo nextest run --work\u{2026}, 3m)")
        );
        let exact = vec![shell(
            Some("cargo nextest run footer"),
            "",
            Some(minutes(3)),
        )];
        assert_eq!(
            text(vec![], exact, None).as_deref(),
            Some("1 background shell (cargo nextest run footer, 3m)"),
            "24 characters stay whole"
        );
    }

    #[test]
    fn a_shell_without_a_command_reads_its_description() {
        for command in [None, Some("")] {
            let shells = vec![shell(command, "Watch the build", Some(minutes(1)))];
            assert_eq!(
                text(vec![], shells, None).as_deref(),
                Some("1 background shell (Watch the build, 1m)")
            );
        }
        let nameless = vec![shell(None, "", Some(TimeDelta::seconds(45)))];
        assert_eq!(
            text(vec![], nameless, None).as_deref(),
            Some("1 background shell (45s)")
        );
    }

    #[test]
    fn a_screen_read_shell_reads_only_its_count() {
        assert_eq!(
            text(vec![], vec![screen_shell()], None).as_deref(),
            Some("1 background shell")
        );
    }

    #[test]
    fn several_shells_read_the_oldest_age() {
        let shells = vec![
            shell(Some("a"), "", Some(minutes(3))),
            shell(Some("b"), "", Some(minutes(65))),
            screen_shell(),
        ];
        assert_eq!(
            text(vec![], shells, None).as_deref(),
            Some("3 background shells (oldest 1h 5m)")
        );
        assert_eq!(
            text(vec![], vec![screen_shell(), screen_shell()], None).as_deref(),
            Some("2 background shells"),
            "no reported start, no oldest age"
        );
    }

    #[test]
    fn subagents_count_in_the_singular_and_the_plural() {
        assert_eq!(
            text(vec![subagent("Sleeper A", None)], vec![], None).as_deref(),
            Some("1 subagent")
        );
        let two = vec![subagent("Sleeper A", None), subagent("Sleeper B", None)];
        assert_eq!(text(two, vec![], None).as_deref(), Some("2 subagents"));
    }

    #[test]
    fn a_wakeup_ahead_counts_down() {
        assert_eq!(
            text(vec![], vec![], Some(now() + minutes(4))).as_deref(),
            Some("wakeup in 4m"),
            "exactly 4 minutes ahead"
        );
        let almost = now() + minutes(4) - TimeDelta::milliseconds(500);
        assert_eq!(
            text(vec![], vec![], Some(almost)).as_deref(),
            Some("wakeup in 4m"),
            "the countdown rounds up to the second"
        );
        assert_eq!(
            text(vec![], vec![], Some(now() + minutes(65))).as_deref(),
            Some("wakeup in 1h 5m")
        );
        assert_eq!(
            text(vec![], vec![], Some(now() + TimeDelta::seconds(59))).as_deref(),
            Some("wakeup in 59s")
        );
    }

    #[test]
    fn a_passed_wakeup_is_due() {
        for at in [now(), now() - minutes(2)] {
            assert_eq!(
                text(vec![], vec![], Some(at)).as_deref(),
                Some("wakeup due")
            );
        }
    }

    #[test]
    fn nothing_running_gives_no_strip() {
        assert_eq!(strip(vec![], vec![], None), None);
        assert!(RunningWork::default().is_empty());
    }

    #[test]
    fn the_parts_join_in_order_and_each_item_names_its_part() {
        let shot = strip(
            vec![subagent("Sleeper A", None), subagent("Sleeper B", None)],
            vec![shell(
                Some("cargo nextest run footer"),
                "",
                Some(minutes(3)),
            )],
            Some(now() + minutes(4)),
        );
        assert_eq!(
            shot.as_ref().map(|shot| shot.text.as_str()),
            Some(
                "2 subagents \u{b7} 1 background shell (cargo nextest run footer, 3m) \u{b7} wakeup in 4m"
            )
        );
        assert_eq!(shot.as_ref().map(|shot| shot.parts.len()), Some(3));
        let parts: Vec<(StripKind, usize)> = shot
            .iter()
            .flat_map(|shot| &shot.items)
            .map(|item| (item.kind, item.part))
            .collect();
        assert_eq!(
            parts,
            [
                (StripKind::Subagent, 0),
                (StripKind::Subagent, 0),
                (StripKind::Shell, 1),
                (StripKind::Wakeup, 2)
            ]
        );
    }

    #[test]
    fn every_tooltip_rule() {
        let screen_agent = WorkSubagent {
            from_screen: true,
            since: None,
            ..subagent("", None)
        };
        let shot = strip(
            vec![
                subagent("Sleeper A", Some("general-purpose")),
                subagent("Sleeper B", None),
                screen_agent,
            ],
            vec![
                shell(
                    Some("cargo nextest run --workspace --cargo-profile test-release"),
                    "Run all",
                    Some(minutes(3)),
                ),
                shell(None, "Watch the build", Some(minutes(1))),
                screen_shell(),
            ],
            None,
        );
        let tips: Vec<Option<String>> = shot
            .map(|strip| strip.items.into_iter().map(|item| item.tooltip).collect())
            .unwrap_or_default();
        let tip = |text: &str| Some(text.to_owned());
        assert_eq!(
            tips,
            [
                tip("general-purpose: Sleeper A"),
                tip("Sleeper B"),
                tip(SCREEN_READ_TIP),
                tip("cargo nextest run --workspace --cargo-profile test-release"),
                tip("Watch the build"),
                tip(SCREEN_READ_TIP),
            ]
        );
    }

    #[test]
    fn no_item_is_tappable() {
        let shot = strip(
            vec![subagent("Sleeper A", Some("general-purpose"))],
            vec![
                shell(Some("cargo test"), "", Some(minutes(3))),
                screen_shell(),
            ],
            Some(now() + minutes(4)),
        );
        let items = shot.map(|strip| strip.items).unwrap_or_default();
        assert_eq!(items.len(), 4);
        assert!(items.iter().all(|item| !item.tappable));
    }

    fn next_repaint(
        shells: Vec<WorkShell>,
        wakeup_at: Option<DateTime<Utc>>,
    ) -> Option<std::time::Duration> {
        let mut work = RunningWork::default();
        work.set(vec![], shells, wakeup_at);
        work.next_repaint(now())
    }

    #[test]
    fn a_wakeup_75_seconds_off_repaints_when_it_reads_in_seconds() {
        assert_eq!(
            next_repaint(vec![], Some(now() + TimeDelta::seconds(75))),
            Some(std::time::Duration::from_secs(15))
        );
    }

    #[test]
    fn an_age_under_a_minute_repaints_each_second() {
        let young = vec![shell(Some("a"), "", Some(TimeDelta::seconds(40)))];
        assert_eq!(
            next_repaint(young, None),
            Some(std::time::Duration::from_secs(1))
        );
    }

    #[test]
    fn nothing_timed_repaints_every_30_seconds() {
        assert_eq!(
            next_repaint(vec![screen_shell()], None),
            Some(std::time::Duration::from_secs(30))
        );
        assert_eq!(
            next_repaint(vec![], Some(now() + minutes(10))),
            Some(std::time::Duration::from_secs(30)),
            "a wakeup far off waits the 30 s"
        );
    }

    #[test]
    fn an_empty_strip_needs_no_repaint() {
        assert_eq!(next_repaint(vec![], None), None);
    }

    #[test]
    fn ages_and_countdowns_under_a_minute_count_seconds() {
        let mut work = RunningWork::default();
        work.set(vec![], vec![shell(Some("a"), "", Some(minutes(3)))], None);
        assert!(!work.counts_seconds(now()));
        work.set(vec![], vec![], Some(now() + TimeDelta::seconds(30)));
        assert!(work.counts_seconds(now()));
        work.set(
            vec![],
            vec![shell(Some("a"), "", Some(TimeDelta::seconds(10)))],
            None,
        );
        assert!(work.counts_seconds(now()));
    }

    fn pending_user(id: &str, text: &str) -> ChatEntry {
        ChatEntry::User {
            id: id.to_owned(),
            text: text.to_owned(),
            pending: true,
        }
    }

    #[test]
    fn a_send_message_names_the_session_and_carries_the_text() {
        let sent = super::send_message("s1", "hello", "r1".to_owned());
        assert!(
            matches!(
                &sent,
                Some(ClientMessage::ChatSend { session_id, text, request_id })
                    if session_id == "s1" && text == "hello" && request_id.as_deref() == Some("r1")
            ),
            "{sent:?}"
        );
        assert!(super::send_message("s1", "  \n ", "r2".to_owned()).is_none());
    }

    #[test]
    fn a_send_is_in_flight_until_its_pending_entry_arrives() {
        let mut sends = super::SendTracker::default();
        sends.sent("r1".to_owned(), "hello".to_owned(), Instant::now());
        assert!(sends.in_flight(Instant::now()));
        sends.on_entries(&[pending_user("pending:1", "something else")]);
        assert!(
            sends.in_flight(Instant::now()),
            "another message's entry is not ours"
        );
        sends.on_entries(&[pending_user("pending:2", "hello\n")]);
        assert!(!sends.in_flight(Instant::now()));
    }

    #[test]
    fn a_send_is_in_flight_until_the_composer_leaves_draft() {
        let mut sends = super::SendTracker::default();
        sends.sent("r1".to_owned(), "hello".to_owned(), Instant::now());
        sends.on_composer(&ComposerState::Ready);
        assert!(
            sends.in_flight(Instant::now()),
            "the paste has not shown yet"
        );
        sends.on_composer(&ComposerState::Draft);
        assert!(sends.in_flight(Instant::now()));
        sends.on_composer(&ComposerState::Working);
        assert!(!sends.in_flight(Instant::now()));
    }

    #[test]
    fn a_refused_send_gives_its_text_back_and_ends_the_flight() {
        let mut sends = super::SendTracker::default();
        sends.sent("r1".to_owned(), "hello".to_owned(), Instant::now());
        assert_eq!(sends.refused("other"), None);
        assert!(sends.in_flight(Instant::now()));
        assert_eq!(sends.refused("r1"), Some("hello".to_owned()));
        assert!(!sends.in_flight(Instant::now()));
        assert_eq!(sends.refused("r1"), None, "a refusal is taken once");
    }

    #[test]
    fn a_refusal_after_the_pending_entry_still_gives_the_text_back() {
        let mut sends = super::SendTracker::default();
        sends.sent("r1".to_owned(), "hello".to_owned(), Instant::now());
        sends.on_entries(&[pending_user("pending:1", "hello")]);
        assert_eq!(sends.refused("r1"), Some("hello".to_owned()));
    }

    #[test]
    fn a_send_settles_ten_seconds_after_it_went() {
        let at = Instant::now();
        let mut sends = super::SendTracker::default();
        sends.sent("r1".to_owned(), "/compact".to_owned(), at);
        assert!(sends.in_flight(at + Duration::from_millis(9_999)));
        assert!(!sends.in_flight(at + Duration::from_secs(10)));
    }

    #[test]
    fn a_send_settles_when_the_turn_starts() {
        let at = Instant::now();
        let mut sends = super::SendTracker::default();
        sends.sent("r1".to_owned(), "/compact".to_owned(), at);
        sends.on_composer(&ComposerState::Working);
        assert!(!sends.in_flight(at));
    }

    #[test]
    fn refused_text_goes_back_before_what_was_typed_since() {
        assert_eq!(super::restored_text("hello", ""), "hello");
        assert_eq!(super::restored_text("hello", "more"), "hello\nmore");
    }

    #[test]
    fn giving_back_nothing_leaves_the_composer_as_it_is() {
        assert_eq!(super::restored_text("", "more"), "more");
        assert_eq!(super::restored_text("", ""), "");
    }

    #[test]
    fn a_refused_draft_send_gives_nothing_back() {
        let at = Instant::now();
        let mut sends = super::SendTracker::default();
        sends.sent_draft("r1".to_owned(), at);
        assert!(sends.in_flight(at));
        assert_eq!(sends.refused("r1").as_deref(), Some(""));
        assert_eq!(super::restored_text("", "typed"), "typed");
    }

    #[test]
    fn a_draft_send_settles_when_the_composer_leaves_the_draft() {
        let at = Instant::now();
        let mut sends = super::SendTracker::default();
        sends.sent_draft("r1".to_owned(), at);
        sends.on_composer(&ComposerState::Draft);
        assert!(sends.in_flight(at), "the draft still on screen");
        sends.on_composer(&ComposerState::Ready);
        assert!(!sends.in_flight(at));
        assert!(!sends.owns_draft(at));
    }

    #[test]
    fn the_note_row_offers_the_drafts_actions_on_a_host_that_has_them() {
        use super::{ComposerLine, ComposerNote, composer_line};
        assert_eq!(
            composer_line(Some(ComposerNote::Draft), false, false),
            Some(ComposerLine::Note(ComposerNote::Draft)),
            "a host without the feature keeps the old note"
        );
        let actions = composer_line(Some(ComposerNote::Draft), true, false);
        assert_eq!(actions, Some(ComposerLine::DraftActions));
        assert_eq!(
            actions.as_ref().map(ComposerLine::text).as_deref(),
            Some("The prompt box has a draft.")
        );
        assert!(actions.is_some_and(|line| line.has_draft_actions()));
        let pulling = composer_line(Some(ComposerNote::Draft), true, true);
        assert_eq!(pulling, Some(ComposerLine::Pulling));
        assert_eq!(
            pulling.as_ref().map(ComposerLine::text).as_deref(),
            Some("Pulling the draft\u{2026}")
        );
        assert!(!pulling.is_some_and(|line| line.has_draft_actions()));
        assert_eq!(
            composer_line(Some(ComposerNote::Sending), true, false),
            Some(ComposerLine::Note(ComposerNote::Sending)),
            "our send in flight shows Sending, no buttons"
        );
        assert_eq!(
            composer_line(None, true, false),
            None,
            "a draft our settled send owns shows no note"
        );
    }

    #[test]
    fn a_pull_waits_for_its_own_answer() {
        let mut pull = super::DraftPull::default();
        assert!(!pull.is_pending());
        assert!(!pull.answered(Some("r1")), "no pull asked for");
        pull.sent("r1".to_owned());
        assert!(pull.is_pending());
        assert!(!pull.answered(None));
        assert!(!pull.answered(Some("r2")), "another request's answer");
        assert!(pull.is_pending());
        assert!(pull.answered(Some("r1")));
        assert!(!pull.is_pending());
        assert!(!pull.answered(Some("r1")), "answered once");
    }

    #[test]
    fn a_partial_pull_says_so() {
        assert_eq!(super::pulled_error(true), None);
        assert_eq!(
            super::pulled_error(false).as_deref(),
            Some("Pulled, but the terminal's prompt box still holds some of the draft.")
        );
    }

    #[test]
    fn a_suggestion_ending_in_an_ellipsis_is_cut() {
        assert!(super::suggestion_is_cut("write a long sugg\u{2026}"));
        assert!(!super::suggestion_is_cut("write todo.py"));
        assert!(!super::suggestion_is_cut(""));
        assert!(
            !super::suggestion_is_cut("wait..."),
            "three dots are unmeasured"
        );
    }

    #[test]
    fn the_suggestion_chip_shows_while_the_composer_can_send() {
        use super::{SuggestionChip, suggestion_chip};
        let whole = ComposerState::Suggestion {
            text: "write todo.py".to_owned(),
        };
        assert_eq!(
            suggestion_chip(Some(&whole), false, false),
            Some(SuggestionChip {
                text: "write todo.py",
                actions: true
            })
        );
        assert_eq!(
            suggestion_chip(Some(&whole), true, false),
            None,
            "a card is up"
        );
        assert_eq!(
            suggestion_chip(Some(&whole), false, true),
            None,
            "a send is in flight"
        );
        let cut = ComposerState::Suggestion {
            text: "write a long sugg\u{2026}".to_owned(),
        };
        assert_eq!(
            suggestion_chip(Some(&cut), false, false),
            Some(SuggestionChip {
                text: "write a long sugg\u{2026}",
                actions: false
            })
        );
        assert_eq!(
            suggestion_chip(Some(&ComposerState::Ready), false, false),
            None
        );
        assert_eq!(suggestion_chip(None, false, false), None);
    }

    #[test]
    fn the_draft_messages_carry_their_request_id() {
        use protocol::ClientMessage;
        assert!(matches!(
            super::pull_draft_message("s1", "r1".to_owned()),
            ClientMessage::ChatPullDraft { session_id, request_id }
                if session_id == "s1" && request_id.as_deref() == Some("r1")
        ));
        assert!(matches!(
            super::send_draft_message("s1", "r2".to_owned()),
            ClientMessage::ChatSendDraft { session_id, request_id }
                if session_id == "s1" && request_id.as_deref() == Some("r2")
        ));
        assert!(matches!(
            super::clear_draft_message("s1", "r3".to_owned()),
            ClientMessage::ChatClearDraft { session_id, request_id }
                if session_id == "s1" && request_id.as_deref() == Some("r3")
        ));
    }

    #[test]
    fn the_composer_note_says_why_a_send_cannot_go() {
        use super::{ComposerNote, composer_note};
        assert_eq!(
            composer_note(true, Some(&ComposerState::Ready), false, false, None),
            None
        );
        assert_eq!(
            composer_note(true, Some(&ComposerState::Working), false, false, None),
            None
        );
        assert_eq!(
            composer_note(true, Some(&ComposerState::Dialog), false, false, None),
            None
        );
        assert_eq!(
            composer_note(true, Some(&ComposerState::Draft), false, false, None),
            Some(ComposerNote::Draft)
        );
        assert_eq!(
            composer_note(true, Some(&ComposerState::Draft), true, false, None),
            Some(ComposerNote::Sending),
            "our own send in flight reads as sending"
        );
        let overlay = ComposerState::Overlay {
            name: "background".to_owned(),
        };
        assert_eq!(
            composer_note(true, Some(&overlay), false, false, None).map(|note| note.text()),
            Some("A full-screen view is open in the terminal. Close it there to send.".to_owned()),
            "an overlay with no Close prompt is named by no label"
        );
        assert_eq!(
            composer_note(true, Some(&ComposerState::Unknown), false, false, None)
                .map(|note| note.text()),
            Some("Can't read the prompt box right now.".to_owned())
        );
        assert_eq!(
            ComposerNote::Draft.text(),
            "The prompt box has a draft. Send or clear it in the terminal first."
        );
        assert_eq!(
            composer_note(false, Some(&ComposerState::Ready), false, false, None)
                .map(|note| note.text()),
            Some("Not connected".to_owned())
        );
        assert!(ComposerNote::NotConnected.disables());
        assert!(!ComposerNote::Draft.disables());
        assert_eq!(composer_note(true, None, false, false, None), None);
        assert_eq!(
            composer_note(true, Some(&ComposerState::Draft), false, true, None),
            None,
            "a draft the send owns shows no note"
        );
        assert_eq!(
            composer_note(true, Some(&ComposerState::Draft), true, true, None),
            Some(ComposerNote::Sending),
            "sending still wins over an owned draft"
        );
    }

    /// An overlay's Close prompt with `subject`.
    fn close_prompt(subject: &str) -> ChatPrompt {
        ChatPrompt {
            id: "o1".to_owned(),
            kind: ChatPromptKind::Overlay,
            subject: subject.to_owned(),
            body: String::new(),
            options: vec![ChatOption {
                number: 1,
                label: "Close".to_owned(),
                description: None,
                selected: false,
            }],
            questions: Vec::new(),
            can_amend: false,
            always_allow_tip: None,
            multi: false,
            cursor: None,
            adjust: None,
            preview: None,
        }
    }

    #[test]
    fn the_composer_note_names_an_overlay_by_its_prompt_and_disables() {
        use super::{ComposerNote, composer_note};
        let rewind = ComposerState::Overlay {
            name: "rewind".to_owned(),
        };
        let close = close_prompt("Rewind");
        let shown = composer_note(true, Some(&rewind), false, false, Some(&close));
        assert_eq!(
            shown.as_ref().map(ComposerNote::text),
            Some("Rewind is open in the terminal. Close it to send.".to_owned())
        );
        assert!(
            shown.is_some_and(|note| note.disables()),
            "an overlay disables the composer"
        );
        let other = ChatPrompt {
            kind: ChatPromptKind::Permission,
            ..close
        };
        let fallback = composer_note(true, Some(&rewind), false, false, Some(&other));
        assert_eq!(
            fallback.as_ref().map(ComposerNote::text),
            Some("A full-screen view is open in the terminal. Close it there to send.".to_owned()),
            "only an overlay prompt names the overlay"
        );
        assert!(fallback.is_some_and(|note| note.disables()));
        assert_eq!(
            composer_note(
                false,
                Some(&rewind),
                false,
                false,
                Some(&close_prompt("Rewind"))
            ),
            Some(ComposerNote::NotConnected),
            "no connection still wins"
        );
    }

    /// The note the composer shows with `state`, after `sends`' last step.
    fn composer_note_shown(
        sends: &super::SendTracker,
        state: &ComposerState,
        now: Instant,
    ) -> Option<super::ComposerNote> {
        super::composer_note(
            true,
            Some(state),
            sends.in_flight(now),
            sends.owns_draft(now),
            None,
        )
    }

    #[test]
    fn a_draft_after_the_pending_entry_is_still_ours() {
        let at = Instant::now();
        let mut sends = super::SendTracker::default();
        sends.sent("r1".to_owned(), "hello".to_owned(), at);
        sends.on_entries(&[pending_user("pending:1", "hello")]);
        assert_eq!(composer_note_shown(&sends, &ComposerState::Ready, at), None);
        sends.on_composer(&ComposerState::Draft);
        assert_eq!(
            composer_note_shown(&sends, &ComposerState::Draft, at),
            None,
            "the send's own paste is not the owner's draft"
        );
        sends.on_composer(&ComposerState::Working);
        assert_eq!(
            composer_note_shown(&sends, &ComposerState::Working, at),
            None
        );
        assert!(
            !sends.owns_draft(at),
            "the turn starting ends the ownership"
        );
    }

    #[test]
    fn a_draft_before_the_pending_entry_reads_sending_then_nothing() {
        let at = Instant::now();
        let mut sends = super::SendTracker::default();
        sends.sent("r1".to_owned(), "hello".to_owned(), at);
        sends.on_composer(&ComposerState::Draft);
        assert_eq!(
            composer_note_shown(&sends, &ComposerState::Draft, at),
            Some(super::ComposerNote::Sending)
        );
        sends.on_entries(&[pending_user("pending:1", "hello")]);
        assert_eq!(composer_note_shown(&sends, &ComposerState::Draft, at), None);
        sends.on_composer(&ComposerState::Working);
        assert_eq!(
            composer_note_shown(&sends, &ComposerState::Working, at),
            None
        );
    }

    #[test]
    fn ready_after_the_pending_entry_ends_the_ownership() {
        let at = Instant::now();
        let mut sends = super::SendTracker::default();
        sends.sent("r1".to_owned(), "hello".to_owned(), at);
        sends.on_entries(&[pending_user("pending:1", "hello")]);
        assert_eq!(composer_note_shown(&sends, &ComposerState::Ready, at), None);
        sends.on_composer(&ComposerState::Ready);
        assert_eq!(composer_note_shown(&sends, &ComposerState::Ready, at), None);
        sends.on_composer(&ComposerState::Draft);
        assert_eq!(
            composer_note_shown(&sends, &ComposerState::Draft, at),
            Some(super::ComposerNote::Draft),
            "a draft after the send settled is the owner's"
        );
    }

    #[test]
    fn a_suggestion_after_the_pending_entry_ends_the_ownership_as_ready_does() {
        let at = Instant::now();
        let suggestion = ComposerState::Suggestion {
            text: "write todo.py".to_owned(),
        };
        let mut sends = super::SendTracker::default();
        sends.sent("r1".to_owned(), "hello".to_owned(), at);
        sends.on_composer(&suggestion);
        assert!(sends.in_flight(at), "the paste has not shown yet");
        sends.on_entries(&[pending_user("pending:1", "hello")]);
        sends.on_composer(&suggestion);
        assert_eq!(composer_note_shown(&sends, &suggestion, at), None);
        sends.on_composer(&ComposerState::Draft);
        assert_eq!(
            composer_note_shown(&sends, &ComposerState::Draft, at),
            Some(super::ComposerNote::Draft),
            "a draft after the send settled is the owner's"
        );
    }

    #[test]
    fn leaving_the_draft_ends_the_ownership() {
        let at = Instant::now();
        let mut sends = super::SendTracker::default();
        sends.sent("r1".to_owned(), "hello".to_owned(), at);
        sends.on_composer(&ComposerState::Draft);
        assert_eq!(
            composer_note_shown(&sends, &ComposerState::Draft, at),
            Some(super::ComposerNote::Sending)
        );
        sends.on_composer(&ComposerState::Ready);
        assert_eq!(composer_note_shown(&sends, &ComposerState::Ready, at), None);
        sends.on_composer(&ComposerState::Draft);
        assert_eq!(
            composer_note_shown(&sends, &ComposerState::Draft, at),
            Some(super::ComposerNote::Draft)
        );
    }

    #[test]
    fn ownership_ends_at_the_backstop() {
        let at = Instant::now();
        let mut sends = super::SendTracker::default();
        sends.sent("r1".to_owned(), "hello".to_owned(), at);
        sends.on_entries(&[pending_user("pending:1", "hello")]);
        sends.on_composer(&ComposerState::Draft);
        assert_eq!(
            composer_note_shown(
                &sends,
                &ComposerState::Draft,
                at + Duration::from_millis(9_999)
            ),
            None
        );
        assert_eq!(
            composer_note_shown(&sends, &ComposerState::Draft, at + super::SEND_BACKSTOP),
            Some(super::ComposerNote::Draft),
            "the backstop gives the box back to the owner"
        );
    }

    #[test]
    fn a_refusal_ends_the_ownership() {
        let at = Instant::now();
        let mut sends = super::SendTracker::default();
        sends.sent("r1".to_owned(), "hello".to_owned(), at);
        sends.on_composer(&ComposerState::Draft);
        assert_eq!(sends.refused("r1"), Some("hello".to_owned()));
        assert_eq!(
            composer_note_shown(&sends, &ComposerState::Draft, at),
            Some(super::ComposerNote::Draft)
        );
    }

    #[test]
    fn a_refusal_after_the_pending_entry_ends_the_ownership() {
        let at = Instant::now();
        let mut sends = super::SendTracker::default();
        sends.sent("r1".to_owned(), "hello".to_owned(), at);
        sends.on_entries(&[pending_user("pending:1", "hello")]);
        sends.on_composer(&ComposerState::Draft);
        assert_eq!(sends.refused("r1"), Some("hello".to_owned()));
        assert_eq!(
            composer_note_shown(&sends, &ComposerState::Draft, at),
            Some(super::ComposerNote::Draft),
            "the settled send's refusal gives the draft back to the owner"
        );
    }
}

#[cfg(test)]
mod run_tests {
    use protocol::{ChatEntry, ScriptFacts, ToolKind, ToolOutcome};

    use super::{
        ChatLog, ChatMode, DisplayRow, RowShape, RunItem, RunSummary, ScriptChip, ScriptLine,
        ScriptRow, ScriptState, display_rows, display_rows_folding, format_ms, script_line,
    };

    /// Script call `id` with `summary`, carrying `facts`, with no result
    /// yet (`None`) or a result that failed or not.
    fn script_call(
        id: &str,
        summary: &str,
        facts: Option<ScriptFacts>,
        failed: Option<bool>,
    ) -> ChatEntry {
        ChatEntry::Tool {
            id: id.to_owned(),
            name: "REPL".to_owned(),
            summary: summary.to_owned(),
            input: "{}".to_owned(),
            result: failed.map(|is_error| ToolOutcome {
                text: "out".to_owned(),
                is_error,
            }),
            parent_id: None,
            tail_paths: Vec::new(),
            kind: ToolKind::Script,
            script: facts,
        }
    }

    fn facts(description: &str, eval: Option<u32>, ms: Option<u64>) -> ScriptFacts {
        ScriptFacts {
            description: description.to_owned(),
            eval,
            ms,
            error: None,
        }
    }

    fn chip(text: &str, failed: bool) -> ScriptChip {
        ScriptChip {
            text: text.to_owned(),
            failed,
        }
    }

    /// The line of script `call` running `inner`, still running or not.
    fn line_of(call: &ChatEntry, inner: &[ChatEntry], running: bool) -> ScriptLine {
        script_line(&ScriptRow {
            call,
            inner: inner.iter().collect(),
            running,
        })
    }

    #[test]
    fn script_line_of_a_done_script_with_eval_time_and_chips() {
        let script = script_call(
            "s1",
            "REPL s1",
            Some(facts(
                "\n  Read the footer and its spec  \nthen sum them",
                Some(12),
                Some(3249),
            )),
            Some(false),
        );
        let inner = [
            call("r1", "Read", ToolKind::Read),
            call("r2", "Read", ToolKind::Read),
            call("b1", "Bash", ToolKind::Run),
            with_result("r3", "Read", ToolKind::Read, Some(true)),
        ];
        assert_eq!(
            line_of(&script, &inner, false),
            ScriptLine {
                state: ScriptState::Done,
                description: "Read the footer and its spec".to_owned(),
                meta: Some("#12 \u{b7} 3.2s".to_owned()),
                chips: vec![
                    chip("2 read", false),
                    chip("1 ran", false),
                    chip("1 failed", true)
                ],
                error: None,
            }
        );
    }

    #[test]
    fn script_line_of_a_running_script_has_no_time() {
        let script = script_call("s1", "", Some(facts("Sum", Some(4), Some(800))), None);
        let inner = [
            call("r1", "Read", ToolKind::Read),
            with_result("r2", "Read", ToolKind::Read, None),
            call("r3", "Read", ToolKind::Read),
            with_result("b1", "Bash", ToolKind::Run, None),
        ];
        let line = line_of(&script, &inner, true);
        assert_eq!(line.state, ScriptState::Running);
        assert_eq!(line.meta.as_deref(), Some("#4"));
        assert_eq!(line.chips, [chip("2 read", false)]);
        assert_eq!(line.error, None);
    }

    #[test]
    fn script_line_of_a_failed_script_carries_its_error() {
        let failing = ScriptFacts {
            error: Some("TypeError: x is undefined".to_owned()),
            ..facts("Sum", Some(5), Some(900))
        };
        let by_facts = script_call("s1", "", Some(failing.clone()), Some(false));
        let line = line_of(&by_facts, &[], false);
        assert_eq!(line.state, ScriptState::Failed);
        assert_eq!(line.error.as_deref(), Some("TypeError: x is undefined"));
        assert_eq!(line.meta.as_deref(), Some("#5 \u{b7} 0.9s"));
        let by_result = script_call("s2", "REPL s2", None, Some(true));
        let line = line_of(&by_result, &[], false);
        assert_eq!(line.state, ScriptState::Failed);
        assert_eq!(line.error, None, "no facts, no error row");
        let still_running = script_call("s3", "", Some(failing), None);
        let line = line_of(&still_running, &[], true);
        assert_eq!(line.state, ScriptState::Running);
        assert_eq!(line.error, None, "a running script draws no error row");
    }

    #[test]
    fn script_line_without_facts_uses_the_one_liner_then_the_name() {
        let summarised = script_call("s1", "sum the sizes\nmore", None, Some(false));
        let line = line_of(&summarised, &[], false);
        assert_eq!(line.description, "sum the sizes");
        assert_eq!(line.meta, None);
        let blank = script_call("s2", "", Some(facts("  \n ", None, None)), Some(false));
        assert_eq!(line_of(&blank, &[], false).description, "REPL");
        let named = script_call("s3", "REPL", None, Some(false));
        assert_eq!(line_of(&named, &[], false).description, "REPL");
    }

    #[test]
    fn script_line_with_only_a_time_has_no_number() {
        let timed = script_call("s1", "", Some(facts("Sum", None, Some(61_000))), None);
        let line = line_of(&timed, &[], false);
        assert_eq!(
            line.state,
            ScriptState::Done,
            "not running, no result, no error"
        );
        assert_eq!(line.meta.as_deref(), Some("1m 1s"));
        let numbered = script_call("s2", "", Some(facts("Sum", Some(7), None)), Some(false));
        assert_eq!(line_of(&numbered, &[], false).meta.as_deref(), Some("#7"));
        let neither = script_call("s3", "", Some(facts("Sum", None, None)), Some(false));
        assert_eq!(line_of(&neither, &[], false).meta, None);
    }

    #[test]
    fn script_line_chips_name_other_tools() {
        let script = script_call("s1", "", Some(facts("Fetch", None, None)), Some(false));
        let inner = [
            call("w1", "WebFetch", ToolKind::Other),
            with_result("w2", "WebFetch", ToolKind::Other, Some(true)),
            call("r1", "Read", ToolKind::Read),
            call("w3", "WebFetch", ToolKind::Unknown),
        ];
        assert_eq!(
            line_of(&script, &inner, false).chips,
            [
                chip("1 WebFetch", false),
                chip("1 WebFetch failed", true),
                chip("1 read", false),
                chip("1 WebFetch", false),
            ]
        );
    }

    #[test]
    fn script_line_shows_only_tailing_inner_calls_while_closed() {
        let script = script_call("s1", "", Some(facts("Build", None, None)), Some(false));
        let mut tailing = call("b1", "Bash", ToolKind::Run);
        if let ChatEntry::Tool { tail_paths, .. } = &mut tailing {
            tail_paths.push("build.log".to_owned());
        }
        let inner = [
            call("r1", "Read", ToolKind::Read),
            tailing,
            call("r2", "Read", ToolKind::Read),
        ];
        let row = ScriptRow {
            call: &script,
            inner: inner.iter().collect(),
            running: false,
        };
        let ids = |open: bool| -> Vec<&str> {
            row.shown_inner(open).filter_map(super::entry_id).collect()
        };
        assert_eq!(ids(false), ["b1"], "closed: the tailing call only");
        assert_eq!(ids(true), ["r1", "b1", "r2"], "open: every inner call");
    }

    #[test]
    fn format_ms_matches_ui_themes() {
        for (ms, text) in [
            (0, "0.0s"),
            (3249, "3.2s"),
            (9999, "9.9s"),
            (10_000, "10s"),
            (59_999, "59s"),
            (60_000, "1m 0s"),
            (61_000, "1m 1s"),
        ] {
            assert_eq!(format_ms(ms), text, "{ms} ms");
        }
    }

    fn answer(id: &str) -> ChatEntry {
        ChatEntry::Answer {
            id: id.to_owned(),
            text: format!("answer {id}"),
            live: false,
        }
    }

    /// A finished call `id` of tool `name`, of `kind`.
    fn call(id: &str, name: &str, kind: ToolKind) -> ChatEntry {
        with_result(id, name, kind, Some(false))
    }

    /// Call `id` with no result yet (`None`), or a result that failed or not.
    fn with_result(id: &str, name: &str, kind: ToolKind, failed: Option<bool>) -> ChatEntry {
        ChatEntry::Tool {
            id: id.to_owned(),
            name: name.to_owned(),
            summary: format!("{name} {id}"),
            input: "{}".to_owned(),
            result: failed.map(|is_error| ToolOutcome {
                text: "out".to_owned(),
                is_error,
            }),
            parent_id: None,
            tail_paths: Vec::new(),
            kind,
            script: None,
        }
    }

    fn shapes(entries: &[ChatEntry], mode: ChatMode) -> Vec<RowShape> {
        display_rows(entries, mode)
            .iter()
            .map(super::DisplayRow::shape)
            .collect()
    }

    fn entry(id: &str) -> RowShape {
        RowShape::Entry(id.to_owned())
    }

    fn line(ids: &[&str], text: &str) -> RowShape {
        RowShape::Run {
            ids: ids.iter().map(|id| (*id).to_owned()).collect(),
            line: text.to_owned(),
            running: false,
        }
    }

    #[test]
    fn a_read_a_search_and_a_run_between_answers_fold_into_one_line() {
        let entries = [
            answer("a1"),
            call("r1", "Read", ToolKind::Read),
            call("g1", "Grep", ToolKind::Search),
            call("b1", "Bash", ToolKind::Run),
            answer("a2"),
        ];
        assert_eq!(
            shapes(&entries, ChatMode::Compact),
            [
                entry("a1"),
                line(&["r1", "g1", "b1"], "1 read \u{b7} 1 searched \u{b7} 1 ran"),
                entry("a2"),
            ]
        );
    }

    #[test]
    fn an_edit_ends_a_run_and_keeps_its_card() {
        let entries = [
            call("r1", "Read", ToolKind::Read),
            call("e1", "Edit", ToolKind::Other),
            call("r2", "Read", ToolKind::Read),
        ];
        assert_eq!(
            shapes(&entries, ChatMode::Compact),
            [
                line(&["r1"], "1 read"),
                entry("e1"),
                line(&["r2"], "1 read")
            ]
        );
    }

    #[test]
    fn a_lone_run_call_is_a_line() {
        let entries = [call("b1", "Bash", ToolKind::Run)];
        assert_eq!(
            shapes(&entries, ChatMode::Compact),
            [line(&["b1"], "1 ran")]
        );
    }

    #[test]
    fn a_call_of_unknown_kind_from_an_older_daemon_keeps_its_card() {
        let entries = [call("r1", "Read", ToolKind::Unknown)];
        assert_eq!(shapes(&entries, ChatMode::Compact), [entry("r1")]);
    }

    #[test]
    fn a_failed_call_adds_the_failed_count() {
        let entries = [
            call("r1", "Read", ToolKind::Read),
            with_result("r2", "Read", ToolKind::Read, Some(true)),
        ];
        assert_eq!(
            shapes(&entries, ChatMode::Compact),
            [line(&["r1", "r2"], "2 read \u{b7} 1 failed")]
        );
    }

    #[test]
    fn a_call_without_a_result_marks_its_run_running() {
        let entries = [with_result("g1", "Grep", ToolKind::Search, None)];
        assert_eq!(
            shapes(&entries, ChatMode::Compact),
            [RowShape::Run {
                ids: vec!["g1".to_owned()],
                line: "1 searched".to_owned(),
                running: true,
            }]
        );
    }

    fn thinking(id: &str) -> ChatEntry {
        ChatEntry::Thinking {
            id: id.to_owned(),
            secs: Some(3),
        }
    }

    #[test]
    fn thinking_between_calls_stays_in_the_run() {
        let entries = [
            call("r1", "Read", ToolKind::Read),
            thinking("k1"),
            call("g1", "Grep", ToolKind::Search),
        ];
        assert_eq!(
            shapes(&entries, ChatMode::Compact),
            [line(&["r1", "k1", "g1"], "1 read \u{b7} 1 searched")]
        );
    }

    fn folding_shapes(entries: &[ChatEntry], min_run: usize) -> Vec<RowShape> {
        display_rows_folding(entries, ChatMode::Compact, min_run)
            .iter()
            .map(super::DisplayRow::shape)
            .collect()
    }

    #[test]
    fn a_run_shorter_than_the_minimum_stands_as_single_rows() {
        let entries = [
            call("r1", "Read", ToolKind::Read),
            call("r2", "Read", ToolKind::Read),
            call("r3", "Read", ToolKind::Read),
        ];
        assert_eq!(
            folding_shapes(&entries, 4),
            [entry("r1"), entry("r2"), entry("r3")]
        );
    }

    #[test]
    fn a_run_of_the_minimum_folds() {
        let entries = [
            call("r1", "Read", ToolKind::Read),
            call("r2", "Read", ToolKind::Read),
            call("g1", "Grep", ToolKind::Search),
            call("b1", "Bash", ToolKind::Run),
        ];
        assert_eq!(
            folding_shapes(&entries, 4),
            [line(
                &["r1", "r2", "g1", "b1"],
                "2 read \u{b7} 1 searched \u{b7} 1 ran"
            )]
        );
    }

    #[test]
    fn thinking_inside_a_short_run_stays_in_order() {
        let entries = [
            call("r1", "Read", ToolKind::Read),
            thinking("k1"),
            call("g1", "Grep", ToolKind::Search),
        ];
        assert_eq!(
            folding_shapes(&entries, 4),
            [entry("r1"), entry("k1"), entry("g1")]
        );
    }

    #[test]
    fn the_desktop_folds_a_run_of_one() {
        let entries = [call("r1", "Read", ToolKind::Read)];
        assert_eq!(
            shapes(&entries, ChatMode::Compact),
            [line(&["r1"], "1 read")]
        );
    }

    #[test]
    fn thinking_before_a_run_stays_outside() {
        let entries = [
            thinking("k0"),
            call("r1", "Read", ToolKind::Read),
            thinking("k1"),
            call("e1", "Edit", ToolKind::Other),
            call("r2", "Read", ToolKind::Read),
            thinking("k2"),
        ];
        assert_eq!(
            shapes(&entries, ChatMode::Compact),
            [
                entry("k0"),
                line(&["r1"], "1 read"),
                entry("k1"),
                entry("e1"),
                line(&["r2"], "1 read"),
                entry("k2"),
            ]
        );
    }

    #[test]
    fn a_call_with_tail_paths_keeps_its_card() {
        let mut tailing = call("b1", "Bash", ToolKind::Run);
        if let ChatEntry::Tool { tail_paths, .. } = &mut tailing {
            tail_paths.push("C:/work/build.log".to_owned());
        }
        let entries = [
            call("r1", "Read", ToolKind::Read),
            tailing,
            call("r2", "Read", ToolKind::Read),
        ];
        assert_eq!(
            shapes(&entries, ChatMode::Compact),
            [
                line(&["r1"], "1 read"),
                entry("b1"),
                line(&["r2"], "1 read")
            ]
        );
    }

    #[test]
    fn a_queued_message_keeps_a_run_running() {
        let unanswered = with_result("g1", "Grep", ToolKind::Search, None);
        let queued = ChatEntry::User {
            id: "u1".to_owned(),
            text: "next".to_owned(),
            pending: true,
        };
        assert_eq!(
            shapes(&[unanswered, queued], ChatMode::Compact),
            [
                RowShape::Run {
                    ids: vec!["g1".to_owned()],
                    line: "1 searched".to_owned(),
                    running: true,
                },
                entry("u1")
            ],
            "a message queued while the turn runs does not end it"
        );
    }

    #[test]
    fn an_unanswered_call_after_turn_end_is_not_running() {
        let unanswered = with_result("g1", "Grep", ToolKind::Search, None);
        let turn_end = ChatEntry::TurnEnd {
            id: "e1".to_owned(),
            duration_ms: Some(1_000),
        };
        let user = ChatEntry::User {
            id: "u1".to_owned(),
            text: "next".to_owned(),
            pending: false,
        };
        assert_eq!(
            shapes(&[unanswered.clone(), turn_end], ChatMode::Compact),
            [line(&["g1"], "1 searched"), entry("e1")],
            "a turn that ended runs nothing"
        );
        assert_eq!(
            shapes(
                &[unanswered.clone(), thinking("k1"), user],
                ChatMode::Compact
            ),
            [line(&["g1"], "1 searched"), entry("k1"), entry("u1")],
            "a later message ends it too"
        );
        assert!(
            matches!(
                shapes(&[unanswered, thinking("k2")], ChatMode::Compact).first(),
                Some(RowShape::Run { running: true, .. })
            ),
            "a thinking row after it is not the turn's end"
        );
    }

    #[test]
    fn a_subagent_task_and_its_child_call_draw_as_comfortable_does() {
        let mut child = call("r1", "Read", ToolKind::Read);
        if let ChatEntry::Tool { parent_id, .. } = &mut child {
            *parent_id = Some("task1".to_owned());
        }
        let entries = [call("task1", "Task", ToolKind::Other), child];
        assert_eq!(
            shapes(&entries, ChatMode::Compact),
            shapes(&entries, ChatMode::Comfortable)
        );
        assert_eq!(
            shapes(&entries, ChatMode::Compact),
            [entry("task1"), entry("r1")]
        );
    }

    /// Script call `id` with no result yet (`None`), or a result that failed
    /// or not.
    fn script(id: &str, failed: Option<bool>) -> ChatEntry {
        let mut entry = with_result(id, "REPL", ToolKind::Script, failed);
        if let ChatEntry::Tool { script, .. } = &mut entry {
            *script = Some(ScriptFacts {
                description: format!("script {id}"),
                ..ScriptFacts::default()
            });
        }
        entry
    }

    /// Script call `id` with `facts` changed by `change`.
    fn script_with(id: &str, failed: Option<bool>, change: impl Fn(&mut ScriptFacts)) -> ChatEntry {
        let mut entry = script(id, failed);
        if let ChatEntry::Tool {
            script: Some(facts),
            ..
        } = &mut entry
        {
            change(facts);
        }
        entry
    }

    /// Finished call `id` of tool `name`, of `kind`, that the call `parent`
    /// (a script or a subagent's task) ran.
    fn inner(id: &str, name: &str, kind: ToolKind, parent: &str) -> ChatEntry {
        let mut entry = call(id, name, kind);
        if let ChatEntry::Tool { parent_id, .. } = &mut entry {
            *parent_id = Some(parent.to_owned());
        }
        entry
    }

    fn script_shape(id: &str, calls: &[&str], running: bool) -> RowShape {
        RowShape::Script {
            id: id.to_owned(),
            inner: calls.iter().map(|call| (*call).to_owned()).collect(),
            running,
        }
    }

    /// The shapes of the scripts inside the Compact runs of `entries`.
    fn scripts_in_runs(entries: &[ChatEntry], min_run: usize) -> Vec<RowShape> {
        display_rows_folding(entries, ChatMode::Compact, min_run)
            .iter()
            .flat_map(|row| match row {
                DisplayRow::Run { entries, .. } => entries
                    .iter()
                    .filter_map(|item| match item {
                        RunItem::Script(script) => Some(script.shape()),
                        RunItem::Entry(_) => None,
                    })
                    .collect(),
                DisplayRow::Single(_) | DisplayRow::Script(_) => Vec::new(),
            })
            .collect()
    }

    fn turn_end(id: &str) -> ChatEntry {
        ChatEntry::TurnEnd {
            id: id.to_owned(),
            duration_ms: Some(1_000),
        }
    }

    #[test]
    fn a_group_with_one_script_and_two_inner_calls() {
        let entries = [
            answer("a1"),
            script("s1", Some(false)),
            inner("r1", "Read", ToolKind::Read, "s1"),
            inner("b1", "Bash", ToolKind::Run, "s1"),
            answer("a2"),
        ];
        assert_eq!(
            shapes(&entries, ChatMode::Compact),
            [entry("a1"), line(&["s1"], "1 script"), entry("a2")],
            "Compact folds the script into a group and its inner calls are not rows"
        );
        assert_eq!(
            scripts_in_runs(&entries, 1),
            [script_shape("s1", &["r1", "b1"], false)]
        );
        assert_eq!(
            shapes(&entries, ChatMode::Comfortable),
            [
                entry("a1"),
                script_shape("s1", &["r1", "b1"], false),
                entry("a2")
            ],
            "Comfortable draws the script as one row holding its inner calls"
        );
    }

    #[test]
    fn a_script_with_nothing_but_its_call_is_one_row() {
        let entries = [script("s1", None), turn_end("e1")];
        assert_eq!(
            shapes(&entries, ChatMode::Comfortable),
            [script_shape("s1", &[], false), entry("e1")]
        );
        assert_eq!(
            folding_shapes(&entries, 4),
            [script_shape("s1", &[], false), entry("e1")],
            "a group too short to fold still draws its script as a script row"
        );
        assert_eq!(
            shapes(&entries, ChatMode::Compact),
            [line(&["s1"], "1 script"), entry("e1")]
        );
        assert_eq!(
            scripts_in_runs(&entries, 1),
            [script_shape("s1", &[], false)]
        );
    }

    #[test]
    fn an_orphan_inner_call_stays_an_ordinary_row() {
        let entries = [inner("r1", "Read", ToolKind::Read, "gone"), answer("a1")];
        for mode in [ChatMode::Compact, ChatMode::Comfortable] {
            assert_eq!(shapes(&entries, mode), [entry("r1"), entry("a1")]);
        }
    }

    #[test]
    fn a_subagents_calls_are_not_taken_by_a_script() {
        let entries = [
            call("task1", "Task", ToolKind::Other),
            script("s1", Some(false)),
            inner("r1", "Read", ToolKind::Read, "task1"),
        ];
        assert_eq!(
            shapes(&entries, ChatMode::Comfortable),
            [entry("task1"), script_shape("s1", &[], false), entry("r1")]
        );
        assert_eq!(
            shapes(&entries, ChatMode::Compact),
            [entry("task1"), line(&["s1"], "1 script"), entry("r1")]
        );
    }

    #[test]
    fn bash_script_bash_in_compact_folds_into_one_run_with_the_script_inside() {
        let entries = [
            call("b1", "Bash", ToolKind::Run),
            script("s1", Some(false)),
            inner("r1", "Read", ToolKind::Read, "s1"),
            call("b2", "Bash", ToolKind::Run),
        ];
        assert_eq!(
            shapes(&entries, ChatMode::Compact),
            [line(&["b1", "s1", "b2"], "2 ran \u{b7} 1 script")]
        );
        assert_eq!(
            scripts_in_runs(&entries, 1),
            [script_shape("s1", &["r1"], false)]
        );
        assert_eq!(
            folding_shapes(&entries, 4),
            [entry("b1"), script_shape("s1", &["r1"], false), entry("b2")],
            "under the minimum the script stays a script row and the Bashes stand alone"
        );
    }

    #[test]
    fn a_script_whose_inner_call_tails_a_file_stands_alone() {
        let mut tailing = inner("b2", "Bash", ToolKind::Run, "s1");
        if let ChatEntry::Tool { tail_paths, .. } = &mut tailing {
            tail_paths.push("C:/work/build.log".to_owned());
        }
        let entries = [
            call("b1", "Bash", ToolKind::Run),
            script("s1", Some(false)),
            tailing,
            call("b3", "Bash", ToolKind::Run),
        ];
        assert_eq!(
            shapes(&entries, ChatMode::Compact),
            [
                line(&["b1"], "1 ran"),
                script_shape("s1", &["b2"], false),
                line(&["b3"], "1 ran")
            ],
            "the script stays in sight so its inner call's Tail button does"
        );
    }

    #[test]
    fn the_run_summary_counts_scripts() {
        let entries = [
            call("b1", "Bash", ToolKind::Run),
            script("s1", Some(false)),
            inner("r1", "Read", ToolKind::Read, "s1"),
            inner("g1", "Grep", ToolKind::Search, "s1"),
            script("s2", Some(false)),
            call("b2", "Bash", ToolKind::Run),
        ];
        assert_eq!(
            shapes(&entries, ChatMode::Compact),
            [line(&["b1", "s1", "s2", "b2"], "2 ran \u{b7} 2 scripts")],
            "inner calls are not counted into read, searched or ran"
        );
        let one = RunSummary {
            ran: 2,
            scripts: 1,
            ..RunSummary::default()
        };
        assert_eq!(one.text(), "2 ran \u{b7} 1 script");
        let failed = RunSummary {
            ran: 1,
            failed: 1,
            scripts: 1,
            ..RunSummary::default()
        };
        assert_eq!(failed.text(), "1 ran \u{b7} 1 failed \u{b7} 1 script");
    }

    #[test]
    fn a_background_script_stays_running_after_its_turn_ends() {
        let background = script_with("s1", None, |facts| facts.eval = Some(3));
        let entries = [background, script("s2", None), turn_end("e1")];
        assert_eq!(
            shapes(&entries, ChatMode::Comfortable),
            [
                script_shape("s1", &[], true),
                script_shape("s2", &[], false),
                entry("e1")
            ],
            "a background script runs on; a script its ended turn left runs no more"
        );
        assert_eq!(
            shapes(&entries, ChatMode::Compact),
            [
                RowShape::Run {
                    ids: vec!["s1".to_owned(), "s2".to_owned()],
                    line: "2 scripts".to_owned(),
                    running: true,
                },
                entry("e1")
            ]
        );
        assert_eq!(
            shapes(&[script("s2", None), turn_end("e1")], ChatMode::Compact),
            [line(&["s2"], "1 script"), entry("e1")]
        );
    }

    #[test]
    fn a_failed_script_marks_its_run_failed() {
        let errored = script_with("s2", Some(false), |facts| {
            facts.error = Some("boom".to_owned());
        });
        let entries = [script("s1", Some(true)), errored];
        assert_eq!(
            shapes(&entries, ChatMode::Compact),
            [line(&["s1", "s2"], "2 failed \u{b7} 2 scripts")]
        );
    }

    #[test]
    fn a_scripts_update_keeps_its_inner_calls_and_its_place() {
        let mut log = ChatLog::default();
        log.upsert(vec![
            answer("a1"),
            script("s1", None),
            inner("r1", "Read", ToolKind::Read, "s1"),
            answer("a2"),
        ]);
        log.upsert(vec![script("s1", Some(false))]);
        let rows: Vec<RowShape> = display_rows(log.entries(), ChatMode::Comfortable)
            .iter()
            .map(DisplayRow::shape)
            .collect();
        assert_eq!(
            rows,
            [entry("a1"), script_shape("s1", &["r1"], false), entry("a2")]
        );
    }

    #[test]
    fn comfortable_draws_one_row_per_entry() {
        let entries = [
            answer("a1"),
            call("r1", "Read", ToolKind::Read),
            call("g1", "Grep", ToolKind::Search),
            call("e1", "Edit", ToolKind::Other),
            with_result("b1", "Bash", ToolKind::Run, None),
        ];
        assert_eq!(
            shapes(&entries, ChatMode::Comfortable),
            [
                entry("a1"),
                entry("r1"),
                entry("g1"),
                entry("e1"),
                entry("b1")
            ]
        );
    }

    #[test]
    fn a_summary_lists_its_counts_in_order_and_leaves_out_zeros() {
        let summary = RunSummary {
            read: 1,
            searched: 2,
            ran: 2,
            failed: 0,
            scripts: 0,
            running: false,
        };
        assert_eq!(summary.text(), "1 read \u{b7} 2 searched \u{b7} 2 ran");
        let ran_only = RunSummary {
            ran: 3,
            failed: 1,
            ..RunSummary::default()
        };
        assert_eq!(ran_only.text(), "3 ran \u{b7} 1 failed");
    }

    #[test]
    fn a_run_opens_and_closes_by_its_first_call_and_a_load_closes_it() {
        let mut log = ChatLog::default();
        log.upsert(vec![call("r1", "Read", ToolKind::Read), answer("a1")]);
        assert!(!log.toggle_run("a1"), "an answer is not a run");
        assert!(!log.toggle_run("nope"), "an id the log does not hold");
        assert!(log.toggle_run("r1"));
        assert!(log.is_run_open("r1"));
        log.reset(vec![call("r1", "Read", ToolKind::Read)], 0);
        assert!(log.is_run_open("r1"), "an unasked reset keeps it open");
        assert!(!log.toggle_run("r1"));
        assert!(!log.is_run_open("r1"));
        log.toggle_run("r1");
        log.load(vec![call("r1", "Read", ToolKind::Read)], 0);
        assert!(!log.is_run_open("r1"), "a load closes every run");
        log.toggle_run("r1");
        log.remove(&["r1".to_owned()]);
        assert!(!log.is_run_open("r1"), "a removed call forgets its run");
    }

    #[test]
    fn the_mode_names_round_trip_and_the_toggle_flips() {
        for mode in [ChatMode::Compact, ChatMode::Comfortable] {
            assert_eq!(ChatMode::of_name(mode.name()), Some(mode));
            assert_eq!(mode.toggled().toggled(), mode);
        }
        assert_eq!(ChatMode::default(), ChatMode::Compact);
        assert_eq!(ChatMode::of_name("dense"), None);
    }
}

#[cfg(test)]
mod final_reply_tests {
    use protocol::{ChatEntry, ToolKind, ToolOutcome};

    use super::{ChatMode, DisplayRow, display_rows, final_replies};

    fn user(id: &str) -> ChatEntry {
        ChatEntry::User {
            id: id.to_owned(),
            text: format!("message {id}"),
            pending: false,
        }
    }

    fn answer(id: &str, live: bool) -> ChatEntry {
        ChatEntry::Answer {
            id: id.to_owned(),
            text: format!("answer {id}"),
            live,
        }
    }

    fn tool(id: &str) -> ChatEntry {
        ChatEntry::Tool {
            id: id.to_owned(),
            name: "Read".to_owned(),
            summary: format!("Read {id}"),
            input: "{}".to_owned(),
            result: Some(ToolOutcome {
                text: "out".to_owned(),
                is_error: false,
            }),
            parent_id: None,
            tail_paths: Vec::new(),
            kind: ToolKind::Read,
            script: None,
        }
    }

    fn turn_end(id: &str) -> ChatEntry {
        ChatEntry::TurnEnd {
            id: id.to_owned(),
            duration_ms: Some(1_000),
        }
    }

    fn finals(entries: &[ChatEntry]) -> Vec<String> {
        final_replies(entries).into_iter().collect()
    }

    #[test]
    fn the_last_answer_before_the_turn_end_is_the_final_reply() {
        let entries = [
            user("u1"),
            answer("a1", false),
            tool("t1"),
            answer("a2", false),
            turn_end("e1"),
        ];
        assert_eq!(finals(&entries), ["a2"]);
        for mode in [ChatMode::Compact, ChatMode::Comfortable] {
            let singles: Vec<&str> = display_rows(&entries, mode)
                .iter()
                .filter_map(|row| match row {
                    DisplayRow::Single(entry) => super::entry_id(entry),
                    DisplayRow::Script(_) | DisplayRow::Run { .. } => None,
                })
                .collect();
            assert!(
                singles.contains(&"a2"),
                "{mode:?}: the final reply draws as its own row"
            );
        }
    }

    #[test]
    fn each_finished_turn_has_its_own_final_reply() {
        let entries = [
            user("u1"),
            answer("a1", false),
            turn_end("e1"),
            user("u2"),
            answer("a2", false),
            turn_end("e2"),
        ];
        assert_eq!(finals(&entries), ["a1", "a2"]);
    }

    #[test]
    fn a_turn_without_a_turn_end_yet_has_no_final_reply() {
        let entries = [
            user("u1"),
            answer("a1", false),
            tool("t1"),
            answer("a2", false),
        ];
        assert_eq!(finals(&entries), Vec::<String>::new());
    }

    #[test]
    fn the_next_user_message_ends_the_turn_without_a_turn_end() {
        let entries = [user("u1"), answer("a1", false), user("u2")];
        assert_eq!(finals(&entries), ["a1"]);
    }

    #[test]
    fn a_queued_message_does_not_box_the_running_answer() {
        let queued = ChatEntry::User {
            id: "u2".to_owned(),
            text: "message u2".to_owned(),
            pending: true,
        };
        let entries = [user("u1"), answer("a1", false), queued.clone()];
        assert_eq!(finals(&entries), Vec::<String>::new());
        let ended = [user("u1"), answer("a1", false), queued, turn_end("e1")];
        assert_eq!(finals(&ended), ["a1"], "the turn's end still settles it");
    }

    #[test]
    fn a_turn_without_an_answer_has_no_final_reply() {
        let entries = [user("u1"), tool("t1"), turn_end("e1")];
        assert_eq!(finals(&entries), Vec::<String>::new());
    }

    #[test]
    fn a_live_answer_before_the_turn_end_is_the_final_reply() {
        let entries = [user("u1"), answer("a1", true), turn_end("e1")];
        assert_eq!(finals(&entries), ["a1"]);
    }
}

#[cfg(test)]
mod file_search_tests {
    use super::FileSearch;

    #[test]
    fn only_the_latest_token_change_keeps_its_timer_current() {
        let mut search = FileSearch::default();
        let first = search.token_changed();
        let second = search.token_changed();
        assert_ne!(first, second);
        assert!(
            !search.is_current(first),
            "a later keystroke makes it stale"
        );
        assert!(search.is_current(second));
        search.token_left();
        assert!(
            !search.is_current(second),
            "leaving the token makes it stale"
        );
    }

    #[test]
    fn an_answer_applies_only_for_the_pending_request() {
        let mut search = FileSearch::default();
        search.token_changed();
        search.sent("r1".to_owned());
        search.sent("r2".to_owned());
        assert!(!search.answered(Some("r1")), "an older search's answer");
        assert!(!search.answered(None), "an answer naming no search");
        assert!(search.answered(Some("r2")));
        assert!(!search.answered(Some("r2")), "an answer applies once");
    }

    #[test]
    fn an_answer_after_the_caret_left_the_token_is_dropped() {
        let mut search = FileSearch::default();
        search.token_changed();
        search.sent("r1".to_owned());
        assert!(!search.token_left(), "no items came from the search yet");
        assert!(!search.answered(Some("r1")));
    }

    #[test]
    fn leaving_the_token_removes_only_the_search_s_items() {
        let mut search = FileSearch::default();
        search.token_changed();
        search.sent("r1".to_owned());
        assert!(search.answered(Some("r1")));
        search.token_changed();
        assert!(search.token_left(), "the popup shows the search's answer");
        assert!(!search.token_left(), "they went the first time");

        search.token_changed();
        search.sent("r2".to_owned());
        assert!(search.answered(Some("r2")));
        search.items_given();
        assert!(!search.token_left(), "items given from elsewhere stay");
    }
}

#[cfg(test)]
mod command_catalog_tests {
    use super::{CommandCatalog, slash_rows};
    use crate::completion::CompletionItem;
    use protocol::{CatalogEntry, CatalogEntryKind, CatalogEntrySource};

    fn entry(name: &str, description: Option<&str>, opens_terminal: bool) -> CatalogEntry {
        CatalogEntry {
            name: name.to_owned(),
            kind: CatalogEntryKind::Command,
            description: description.map(str::to_owned),
            source: CatalogEntrySource::BuiltIn,
            opens_terminal,
        }
    }

    fn named(name: &str) -> CatalogEntry {
        entry(name, None, false)
    }

    fn inserts(rows: &[CompletionItem]) -> Vec<&str> {
        rows.iter().map(|row| row.insert.as_str()).collect()
    }

    #[test]
    fn the_slash_filter_puts_prefix_matches_before_substring_ones() {
        let entries = [
            named("decompose"),
            entry("code-review", Some("Review the branch"), false),
            named("help"),
            entry("Config", Some("Open the settings"), true),
            named("recon"),
            entry("compact", None, true),
        ];
        let rows = slash_rows(&entries, "cO");
        assert_eq!(
            inserts(&rows),
            [
                "/code-review",
                "/Config",
                "/compact",
                "/decompose",
                "/recon"
            ],
            "prefix matches first, then substring ones, each in catalog order"
        );
        let labels: Vec<&str> = rows.iter().map(|row| row.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "/code-review \u{b7} Review the branch",
                "/Config \u{b7} Open the settings \u{b7} opens in the terminal",
                "/compact \u{b7} opens in the terminal",
                "/decompose",
                "/recon",
            ]
        );
        assert_eq!(slash_rows(&entries, "").len(), entries.len(), "every entry");
        assert_eq!(slash_rows(&entries, "zz"), Vec::new());
    }

    #[test]
    fn the_slash_filter_is_capped_at_fifty() {
        let entries: Vec<CatalogEntry> = (0..30)
            .map(|i| named(&format!("x-cmd{i:02}")))
            .chain((0..40).map(|i| named(&format!("cmd{i:02}"))))
            .collect();
        let rows = slash_rows(&entries, "cmd");
        assert_eq!(rows.len(), 50);
        assert_eq!(rows[0].insert, "/cmd00", "the prefix matches lead");
        assert_eq!(rows[39].insert, "/cmd39");
        assert_eq!(rows[40].insert, "/x-cmd00", "then the substring ones");
        assert_eq!(rows[49].insert, "/x-cmd09");
    }

    #[test]
    fn a_catalog_answer_applies_only_for_the_pending_request() {
        let mut catalog = CommandCatalog::default();
        assert!(catalog.wants_fetch());
        assert_eq!(catalog.rows_for("co"), None, "nothing cached yet");
        catalog.sent("r1".to_owned());
        assert!(!catalog.wants_fetch(), "one request in flight");
        assert!(!catalog.answered(Some("r0"), vec![named("compact")]));
        assert!(!catalog.answered(None, vec![named("compact")]));
        assert!(catalog.answered(Some("r1"), vec![named("compact")]));
        assert!(
            !catalog.answered(Some("r1"), vec![named("clear")]),
            "an answer applies once"
        );
        assert!(!catalog.wants_fetch(), "the answer is cached");
        let rows = catalog.rows_for("co").unwrap_or_default();
        assert_eq!(inserts(&rows), ["/compact"]);
        assert!(catalog.token_left(), "the popup showed the catalog's rows");

        catalog.forget();
        assert!(catalog.wants_fetch(), "a forgotten catalog is asked again");
        assert_eq!(catalog.rows_for("co"), None);
        catalog.sent("r2".to_owned());
        catalog.forget();
        assert!(
            !catalog.answered(Some("r2"), vec![named("compact")]),
            "the pending request is forgotten with it"
        );
    }
}
