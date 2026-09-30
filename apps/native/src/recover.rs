//! The Recover dialog's state: the ended-session history the daemon keeps
//! (and the rail badge counting its unrecovered losses), the history grouped
//! by when sessions were lost, each row's conversation and "Recover as"
//! choice, the ticked rows, keyboard focus, and the `RecoverSessions`
//! request with its answer. Plain data; the view draws it.

use std::collections::{HashMap, HashSet};
use std::fmt::Display;
use std::time::{Duration, Instant};

use chrono::{DateTime, TimeZone, Utc};
use protocol::{
    Agent, ClientMessage, ConversationCandidate, DaemonMessage, HistoryEntry, RecoverAs,
    RecoverItem, RecoverItemResult, SessionEnd, SessionHistoryItem, SessionMode,
};

use crate::spawn_form::human_relative_time;

/// Unexpected ends this close to the previous one in a burst join it.
const BURST_GAP_SECS: i64 = 60;

/// How long a `RecoverSessions` request may go unanswered.
pub const RECOVER_TIMEOUT: Duration = Duration::from_secs(120);

/// The ended-session history the daemon last sent.
#[derive(Debug, Default)]
pub struct SessionHistory {
    items: Vec<SessionHistoryItem>,
}

impl SessionHistory {
    /// Takes a `SessionHistory` reply or broadcast (any request id) as the
    /// whole list. Returns whether `msg` was one.
    pub fn apply(&mut self, msg: &DaemonMessage) -> bool {
        let DaemonMessage::SessionHistory { items, .. } = msg else {
            return false;
        };
        self.items.clone_from(items);
        true
    }

    pub fn items(&self) -> &[SessionHistoryItem] {
        &self.items
    }

    /// The rail badge: sessions lost unexpectedly and not yet recovered.
    pub fn badge_count(&self) -> usize {
        self.items.iter().filter(|item| counts(&item.entry)).count()
    }
}

/// Whether `entry` counts toward the badge.
fn counts(entry: &HistoryEntry) -> bool {
    entry.end.is_unexpected() && entry.recovered_at.is_none()
}

/// What a group of rows holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupKind {
    /// Unexpected ends whose time could not be found.
    UnknownTime,
    /// Unexpected ends that came together.
    Burst,
    /// Every other ended session, shown collapsed.
    Other,
}

/// A titled group of rows, newest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub kind: GroupKind,
    /// A burst's latest end, as its title shows it.
    clock: String,
    /// History ids.
    pub rows: Vec<String>,
}

impl Group {
    /// "Lost at an unknown time", "N sessions lost at HH:MM" (counting the
    /// rows it lists now) or "Other recent sessions".
    pub fn title(&self) -> String {
        match self.kind {
            GroupKind::UnknownTime => "Lost at an unknown time".to_owned(),
            GroupKind::Burst => {
                let count = self.rows.len();
                let noun = if count == 1 { "session" } else { "sessions" };
                format!("{count} {noun} lost at {}", self.clock)
            }
            GroupKind::Other => "Other recent sessions".to_owned(),
        }
    }
}

/// The history grouped for the dialog: the unknown-time losses, then the
/// bursts of losses (newest first), then everything else.
pub fn groups<Tz: TimeZone>(items: &[SessionHistoryItem], now: &DateTime<Tz>) -> Vec<Group>
where
    Tz::Offset: Display,
{
    let mut entries: Vec<&HistoryEntry> = items.iter().map(|item| &item.entry).collect();
    entries.sort_by(|a, b| {
        b.ended_at
            .cmp(&a.ended_at)
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    let ids = |list: &[&HistoryEntry]| -> Vec<String> {
        list.iter().map(|e| e.session_id.clone()).collect()
    };
    let (lost, other): (Vec<&HistoryEntry>, Vec<&HistoryEntry>) =
        entries.into_iter().partition(|e| e.end.is_unexpected());
    let (unknown, known): (Vec<&HistoryEntry>, Vec<&HistoryEntry>) =
        lost.into_iter().partition(|e| !e.end_time_known);
    let mut out = Vec::new();
    if !unknown.is_empty() {
        out.push(Group {
            kind: GroupKind::UnknownTime,
            clock: String::new(),
            rows: ids(&unknown),
        });
    }
    for burst in bursts(&known) {
        let Some(latest) = burst.first() else {
            continue;
        };
        out.push(Group {
            kind: GroupKind::Burst,
            clock: clock(latest.ended_at, now),
            rows: ids(&burst),
        });
    }
    if !other.is_empty() {
        out.push(Group {
            kind: GroupKind::Other,
            clock: String::new(),
            rows: ids(&other),
        });
    }
    out
}

/// `newest_first` split where an end comes more than a minute before the
/// one after it.
fn bursts<'a>(newest_first: &[&'a HistoryEntry]) -> Vec<Vec<&'a HistoryEntry>> {
    let mut out: Vec<Vec<&HistoryEntry>> = Vec::new();
    for &entry in newest_first {
        let joins = out
            .last()
            .and_then(|burst| burst.last())
            .is_some_and(|prev| (prev.ended_at - entry.ended_at).num_seconds() <= BURST_GAP_SECS);
        match out.last_mut() {
            Some(burst) if joins => burst.push(entry),
            _ => out.push(vec![entry]),
        }
    }
    out
}

/// The collapsed "other sessions" toggle's text.
pub fn other_toggle_label(count: usize, expanded: bool) -> String {
    if expanded {
        "Hide other recent sessions".to_owned()
    } else if count == 1 {
        "Show 1 other recent session".to_owned()
    } else {
        format!("Show {count} other recent sessions")
    }
}

/// `at` as HH:MM in `now`'s time zone, after "Mon D " when it is not
/// `now`'s day.
fn clock<Tz: TimeZone>(at: DateTime<Utc>, now: &DateTime<Tz>) -> String
where
    Tz::Offset: Display,
{
    let local = at.with_timezone(&now.timezone());
    if local.date_naive() == now.date_naive() {
        local.format("%H:%M").to_string()
    } else {
        local.format("%b %-d %H:%M").to_string()
    }
}

/// When and how the session ended: "HH:MM · lost", "HH:MM · exited (code
/// N)", "HH:MM · stopped", "HH:MM · daemon shut down".
pub fn ended_text<Tz: TimeZone>(entry: &HistoryEntry, now: &DateTime<Tz>) -> String
where
    Tz::Offset: Display,
{
    let how = match entry.end {
        SessionEnd::TracerLost => "lost".to_owned(),
        SessionEnd::Exited { code } => format!("exited (code {code})"),
        SessionEnd::StoppedByUser => "stopped".to_owned(),
        SessionEnd::DaemonShutdown => "daemon shut down".to_owned(),
        SessionEnd::Unknown => "ended".to_owned(),
    };
    let when = if entry.end_time_known {
        clock(entry.ended_at, now)
    } else {
        "time unknown".to_owned()
    };
    format!("{when} · {how}")
}

/// The folder a row names: the session's last folder, else the one it
/// started in.
fn folder(entry: &HistoryEntry) -> Option<&str> {
    entry
        .current_cwd
        .as_deref()
        .or(entry.primary_cwd.as_deref())
}

/// The row's name: its label, else (an imported row) its folder.
fn row_label(entry: &HistoryEntry) -> String {
    if !entry.label.is_empty() {
        return entry.label.clone();
    }
    entry
        .primary_cwd
        .as_deref()
        .or(entry.current_cwd.as_deref())
        .unwrap_or(&entry.session_id)
        .to_owned()
}

/// The names the client knows for the registry's ids.
#[derive(Debug, Default)]
pub struct Names {
    /// Workspace id to name.
    pub workspaces: HashMap<String, String>,
    /// Repo id to name.
    pub repos: HashMap<String, String>,
}

/// Where the row ran: its workspace, else its repo, else its folder.
fn row_where(entry: &HistoryEntry, names: &Names) -> String {
    if let Some(name) = entry
        .workspace_id
        .as_ref()
        .and_then(|id| names.workspaces.get(id))
    {
        return name.clone();
    }
    if let Some(member) = entry.members.first() {
        return member.repo_name.clone();
    }
    folder(entry).unwrap_or_default().to_owned()
}

/// A conversation choice's text: "<title or 'untitled'> · 3h ago".
pub fn conversation_label(candidate: &ConversationCandidate, now: DateTime<Utc>) -> String {
    let title = candidate
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or("untitled");
    let ago = human_relative_time(now.timestamp(), candidate.last_active.timestamp());
    format!("{title} · {ago}")
}

/// The conversation a row starts on: the session's own, else the newest.
fn default_conversation(item: &SessionHistoryItem) -> Option<usize> {
    let known = item.entry.claude_session_id.as_deref();
    if let Some(index) = item
        .candidates
        .iter()
        .position(|c| Some(c.id.as_str()) == known)
    {
        return Some(index);
    }
    item.candidates
        .iter()
        .enumerate()
        .max_by_key(|(index, c)| (c.last_active, std::cmp::Reverse(*index)))
        .map(|(index, _)| index)
}

/// One way to recover a row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoverOption {
    pub how: RecoverAs,
    pub label: String,
    /// Whether it resumes the chosen conversation. False for a plain shell
    /// and for an own-agent row that starts a fresh run.
    pub resumes: bool,
}

/// Whether the row picks how it recovers: a shell, or a session known only
/// by its folder.
fn chooses_how(entry: &HistoryEntry) -> bool {
    entry.mode == SessionMode::PlainShell || entry.spawn_config.is_none()
}

/// The CLI an entry's own agent runs, as its rows name it.
fn agent_name(agent: Agent) -> &'static str {
    match agent {
        Agent::Claude => "Claude",
        Agent::Codex => "Codex",
        Agent::Cursor => "Cursor",
    }
}

/// Whether an own-agent entry resumes its conversation: the daemon says it
/// can and the id to resume with was recorded.
fn own_agent_resumes(item: &SessionHistoryItem) -> bool {
    item.own_agent_resumable && item.entry.agent_conversation_id.is_some()
}

/// The text an own-agent row shows where a Claude row shows nothing: whether
/// it resumes its own conversation or starts a fresh run. None for a Claude
/// row, for an own-agent row the daemon refuses for want of spawn settings,
/// and for an entry already recovered, which shows only its stamp.
fn own_agent_text(item: &SessionHistoryItem) -> Option<String> {
    let entry = &item.entry;
    if entry.agent == Agent::Claude || entry.spawn_config.is_none() || entry.recovered_at.is_some()
    {
        return None;
    }
    let run = if own_agent_resumes(item) {
        "resumes its conversation"
    } else if entry.agent_conversation_id.is_some() {
        "fresh run: its conversation is gone"
    } else {
        "fresh run: no conversation recorded"
    };
    Some(format!("{} session, {run}", agent_name(entry.agent)))
}

/// The ways `item` can be recovered, the default first-chosen index among
/// them. Empty when nothing can be offered.
fn recover_options(item: &SessionHistoryItem, names: &Names) -> (Vec<RecoverOption>, usize) {
    let entry = &item.entry;
    if entry.agent != Agent::Claude {
        let options = if entry.spawn_config.is_some() {
            vec![RecoverOption {
                how: RecoverAs::OwnAgent,
                label: format!("{} session", agent_name(entry.agent)),
                resumes: own_agent_resumes(item),
            }]
        } else {
            Vec::new()
        };
        return (options, 0);
    }
    let has_candidate = !item.candidates.is_empty();
    if !chooses_how(entry) {
        let options = if has_candidate {
            vec![RecoverOption {
                how: RecoverAs::Claude,
                label: "Claude session".to_owned(),
                resumes: true,
            }]
        } else {
            Vec::new()
        };
        return (options, 0);
    }
    if !has_candidate {
        let options = if item.entry.mode == SessionMode::PlainShell {
            vec![RecoverOption {
                how: RecoverAs::Shell,
                label: "Plain shell".to_owned(),
                resumes: false,
            }]
        } else {
            Vec::new()
        };
        return (options, 0);
    }
    let mut options = Vec::new();
    let path = folder(&item.entry).unwrap_or_default();
    if let Some(repo_id) = &item.folder_repo_id {
        let repo = names
            .repos
            .get(repo_id)
            .cloned()
            .unwrap_or_else(|| last_segment(path).to_owned());
        options.push(RecoverOption {
            how: RecoverAs::Claude,
            label: format!("Claude session in {repo}"),
            resumes: true,
        });
    } else if item.folder_is_git_repo && !path.is_empty() {
        options.push(RecoverOption {
            how: RecoverAs::RegisterRepoThenClaude {
                path: path.to_owned(),
            },
            label: format!("Register {path} as a repo, then Claude session"),
            resumes: true,
        });
    }
    let default = if item.folder_repo_id.is_some() {
        0
    } else {
        options.len()
    };
    options.push(RecoverOption {
        how: RecoverAs::Shell,
        label: "Shell running claude --resume".to_owned(),
        resumes: true,
    });
    (options, default)
}

/// Whether a row joins the pre-tick: a loss the daemon may recover, and not
/// an own-agent row that would start a fresh run.
fn pre_ticked(item: &SessionHistoryItem) -> bool {
    item.entry.agent == Agent::Claude || own_agent_resumes(item)
}

/// The last component of a path, either separator.
fn last_segment(path: &str) -> &str {
    path.trim_end_matches(['\\', '/'])
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(path)
}

/// One row of the dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// The history id.
    pub id: String,
    pub label: String,
    pub place: String,
    pub ended: String,
    /// Why the row cannot be recovered, when it cannot.
    pub disabled: Option<String>,
    /// `(conversation id, choice text)`, newest first.
    pub conversations: Vec<(String, String)>,
    pub conversation: Option<usize>,
    /// The one way an own-agent row recovers, where a Claude row shows its
    /// "Recover as" choice. None for a Claude row.
    pub fixed_how: Option<String>,
    pub options: Vec<RecoverOption>,
    pub option: usize,
    /// Whether the row shows its "Recover as" choice.
    pub shows_recover_as: bool,
    /// The last attempt's failure.
    pub error: Option<String>,
}

impl Row {
    fn new<Tz: TimeZone>(item: &SessionHistoryItem, names: &Names, now: &DateTime<Tz>) -> Self
    where
        Tz::Offset: Display,
    {
        let entry = &item.entry;
        let (options, option) = recover_options(item, names);
        let disabled = match entry.recovered_at {
            Some(at) => Some(format!("recovered {}", clock(at, now))),
            None if options.is_empty() => Some(
                match entry.agent {
                    Agent::Claude => "no conversation found",
                    Agent::Codex | Agent::Cursor => "no spawn settings recorded",
                }
                .to_owned(),
            ),
            None => None,
        };
        let now_utc = now.with_timezone(&Utc);
        let own = entry.agent != Agent::Claude;
        let conversations: Vec<(String, String)> = if own && own_agent_resumes(item) {
            entry
                .agent_conversation_id
                .iter()
                .map(|id| (id.clone(), "its own conversation".to_owned()))
                .collect()
        } else if own {
            Vec::new()
        } else {
            item.candidates
                .iter()
                .map(|c| (c.id.clone(), conversation_label(c, now_utc)))
                .collect()
        };
        let conversation = if own {
            (!conversations.is_empty()).then_some(0)
        } else {
            default_conversation(item)
        };
        Self {
            id: entry.session_id.clone(),
            label: row_label(entry),
            place: row_where(entry, names),
            ended: ended_text(entry, now),
            disabled,
            conversations,
            conversation,
            fixed_how: own_agent_text(item),
            shows_recover_as: !own && chooses_how(entry) && !options.is_empty(),
            options,
            option,
            error: None,
        }
    }

    fn enabled(&self) -> bool {
        self.disabled.is_none()
    }

    /// The chosen way to recover.
    pub fn chosen(&self) -> Option<&RecoverOption> {
        self.options.get(self.option)
    }

    fn item(&self) -> Option<RecoverItem> {
        let option = self.chosen()?;
        let conversation_id = if option.resumes {
            self.conversation
                .and_then(|index| self.conversations.get(index))
                .map(|(id, _)| id.clone())
        } else {
            None
        };
        Some(RecoverItem {
            history_id: self.id.clone(),
            conversation_id,
            how: option.how.clone(),
        })
    }
}

/// A control of the dialog that can hold the keyboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Control {
    Row(String),
    Conversation(String),
    RecoverAs(String),
    OtherToggle,
    SelectAll,
    Cancel,
    Recover,
}

impl Control {
    fn row(&self) -> Option<&str> {
        match self {
            Self::Row(id) | Self::Conversation(id) | Self::RecoverAs(id) => Some(id),
            _ => None,
        }
    }
}

/// A focus key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusMove {
    /// Up arrow: the row above.
    Up,
    /// Down arrow: the row below.
    Down,
    /// Tab: the next control.
    Next,
    /// Shift-Tab: the previous control.
    Prev,
}

/// What activating the focused control asks of the view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activation {
    /// Handled inside the dialog.
    Handled,
    Cancel,
    Recover,
}

/// What a recovery's answer does to the dialog.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    /// Every item recovered: the dialog closes.
    pub close: bool,
    /// The new sessions, in request order, to place like spawns.
    pub recovered_session_ids: Vec<String>,
}

/// The request waiting for its answer.
#[derive(Debug)]
struct InFlight {
    request_id: String,
    sent_at: Instant,
    /// The history ids it asked for; the answer is judged against these.
    requested: HashSet<String>,
    /// Cleared when the request timed out; a late answer still applies.
    busy: bool,
}

/// The Recover dialog's state.
#[derive(Debug)]
pub struct RecoverDialog {
    groups: Vec<Group>,
    rows: HashMap<String, Row>,
    ticked: HashSet<String>,
    /// The rows whose tick or untick the user made, so a refresh leaves them
    /// as the user left them and applies the pre-tick rule only to the rest.
    user_set: HashSet<String>,
    other_expanded: bool,
    focus: Option<Control>,
    in_flight: Option<InFlight>,
    error: Option<String>,
    /// Only the rows a recovery failed on are listed, so a fresh history
    /// adds none.
    failed_only: bool,
}

impl RecoverDialog {
    /// The dialog over `items`: the unrecovered losses ticked, except an
    /// own-agent row that would start a fresh run.
    pub fn new<Tz: TimeZone>(
        items: &[SessionHistoryItem],
        names: &Names,
        now: &DateTime<Tz>,
    ) -> Self
    where
        Tz::Offset: Display,
    {
        let rows: HashMap<String, Row> = items
            .iter()
            .map(|item| (item.entry.session_id.clone(), Row::new(item, names, now)))
            .collect();
        let ticked: HashSet<String> = items
            .iter()
            .filter(|item| counts(&item.entry) && pre_ticked(item))
            .map(|item| item.entry.session_id.clone())
            .filter(|id| rows.get(id).is_some_and(Row::enabled))
            .collect();
        let mut dialog = Self {
            groups: groups(items, now),
            rows,
            user_set: HashSet::new(),
            ticked,
            other_expanded: false,
            focus: None,
            in_flight: None,
            error: None,
            failed_only: false,
        };
        dialog.focus = dialog.focus_order().into_iter().next();
        dialog
    }

    /// Takes a fresh history while open. A row still listed keeps its
    /// conversation, "Recover as" choice and error, and its tick while it
    /// can be recovered; a row gone from `items` drops. New rows join, the
    /// losses ticked, unless only a recovery's failed rows are listed. A
    /// tick the pre-tick rule made follows the rule again, so an own-agent
    /// row that can no longer resume loses it; a tick or untick the user
    /// made stands.
    pub fn refresh<Tz: TimeZone>(
        &mut self,
        items: &[SessionHistoryItem],
        names: &Names,
        now: &DateTime<Tz>,
    ) where
        Tz::Offset: Display,
    {
        let items: Vec<SessionHistoryItem> = items
            .iter()
            .filter(|item| !self.failed_only || self.rows.contains_key(&item.entry.session_id))
            .cloned()
            .collect();
        let mut fresh = Self::new(&items, names, now);
        for (id, row) in &mut fresh.rows {
            if let Some(old) = self.rows.get(id) {
                keep_choices(old, row);
            }
        }
        let ticked: HashSet<String> = fresh
            .rows
            .iter()
            .filter(|(id, row)| {
                row.enabled()
                    && if self.user_set.contains(*id) {
                        self.ticked.contains(*id)
                    } else {
                        fresh.ticked.contains(*id)
                    }
            })
            .map(|(id, _)| id.clone())
            .collect();
        fresh.user_set = self
            .user_set
            .iter()
            .filter(|id| fresh.rows.contains_key(*id))
            .cloned()
            .collect();
        fresh.ticked = ticked;
        fresh.other_expanded = self.other_expanded;
        fresh.in_flight = self.in_flight.take();
        fresh.error = self.error.take();
        fresh.failed_only = self.failed_only;
        let order = fresh.focus_order();
        fresh.focus = self
            .focus
            .take()
            .filter(|focus| order.contains(focus))
            .or_else(|| order.into_iter().next());
        *self = fresh;
    }

    pub fn groups(&self) -> &[Group] {
        &self.groups
    }

    pub fn row(&self, id: &str) -> Option<&Row> {
        self.rows.get(id)
    }

    pub fn is_ticked(&self, id: &str) -> bool {
        self.ticked.contains(id)
    }

    pub fn focus(&self) -> Option<&Control> {
        self.focus.as_ref()
    }

    pub fn other_expanded(&self) -> bool {
        self.other_expanded
    }

    /// The dialog-wide error line.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The text shown instead of the list when it is empty.
    pub fn empty_text(&self) -> Option<&'static str> {
        self.rows
            .is_empty()
            .then_some("No ended sessions to recover.")
    }

    /// The row ids shown, in display order: the collapsed group's rows are
    /// hidden.
    pub fn visible_rows(&self) -> Vec<&str> {
        self.groups
            .iter()
            .filter(|g| g.kind != GroupKind::Other || self.other_expanded)
            .flat_map(|g| g.rows.iter().map(String::as_str))
            .collect()
    }

    /// Every row id in display order, shown or not.
    fn ordered_rows(&self) -> impl Iterator<Item = &str> {
        self.groups
            .iter()
            .flat_map(|g| g.rows.iter().map(String::as_str))
    }

    fn has_other(&self) -> bool {
        self.groups.iter().any(|g| g.kind == GroupKind::Other)
    }

    pub fn is_busy(&self) -> bool {
        self.in_flight.as_ref().is_some_and(|f| f.busy)
    }

    fn ticked_count(&self) -> usize {
        self.ordered_rows()
            .filter(|id| self.ticked.contains(*id))
            .count()
    }

    /// The Recover button's text.
    pub fn recover_label(&self) -> String {
        if self.is_busy() {
            "Recovering…".to_owned()
        } else {
            format!("Recover {}", self.ticked_count())
        }
    }

    pub fn recover_enabled(&self) -> bool {
        !self.is_busy() && self.ticked_count() > 0
    }

    /// Ticks or unticks row `id`, when it can be recovered.
    pub fn toggle(&mut self, id: &str) {
        if self.is_busy() || !self.rows.get(id).is_some_and(Row::enabled) {
            return;
        }
        if !self.ticked.remove(id) {
            self.ticked.insert(id.to_owned());
        }
        self.user_set.insert(id.to_owned());
    }

    /// Ticks every shown row that can be recovered.
    pub fn select_all(&mut self) {
        if self.is_busy() {
            return;
        }
        let ids: Vec<String> = self
            .visible_rows()
            .into_iter()
            .filter(|id| self.rows.get(*id).is_some_and(Row::enabled))
            .map(str::to_owned)
            .collect();
        for id in &ids {
            self.user_set.insert(id.clone());
        }
        self.ticked.extend(ids);
    }

    /// Shows or hides the other recent sessions. Focus on a row that hides
    /// moves to the toggle.
    pub fn toggle_other(&mut self) {
        self.other_expanded = !self.other_expanded;
        if !self.other_expanded
            && let Some(row) = self.focus.as_ref().and_then(Control::row)
            && !self.visible_rows().contains(&row)
        {
            self.focus = Some(Control::OtherToggle);
        }
    }

    /// Moves row `id`'s conversation choice to the next (or previous) one.
    pub fn cycle(&mut self, id: &str, forward: bool) {
        if self.is_busy() {
            return;
        }
        if let Some(row) = self.rows.get_mut(id)
            && let Some(current) = row.conversation
        {
            row.conversation = Some(step(current, row.conversations.len(), forward));
        }
    }

    /// Moves row `id`'s "Recover as" choice to the next (or previous) one.
    pub fn cycle_recover_as(&mut self, id: &str, forward: bool) {
        if self.is_busy() {
            return;
        }
        if let Some(row) = self.rows.get_mut(id)
            && row.shows_recover_as
        {
            row.option = step(row.option, row.options.len(), forward);
        }
    }

    /// The controls that take the keyboard, in Tab order.
    pub fn focus_order(&self) -> Vec<Control> {
        let mut out = Vec::new();
        for group in &self.groups {
            if group.kind == GroupKind::Other {
                out.push(Control::OtherToggle);
                if !self.other_expanded {
                    continue;
                }
            }
            for id in &group.rows {
                self.push_row_controls(id, &mut out);
            }
        }
        out.push(Control::SelectAll);
        out.push(Control::Cancel);
        if self.recover_enabled() {
            out.push(Control::Recover);
        }
        out
    }

    fn push_row_controls(&self, id: &str, out: &mut Vec<Control>) {
        out.push(Control::Row(id.to_owned()));
        let Some(row) = self.rows.get(id).filter(|row| row.enabled()) else {
            return;
        };
        if !row.conversations.is_empty() && row.chosen().is_some_and(|o| o.resumes) {
            out.push(Control::Conversation(id.to_owned()));
        }
        if row.shows_recover_as {
            out.push(Control::RecoverAs(id.to_owned()));
        }
    }

    /// Moves the keyboard: Up and Down between rows, Tab and Shift-Tab
    /// across every control, wrapping.
    pub fn move_focus(&mut self, key: FocusMove) {
        match key {
            FocusMove::Up | FocusMove::Down => self.move_row(key == FocusMove::Down),
            FocusMove::Next | FocusMove::Prev => {
                let order = self.focus_order();
                let len = order.len();
                let at = self
                    .focus
                    .as_ref()
                    .and_then(|f| order.iter().position(|c| c == f));
                let next = match at {
                    Some(index) => step(index, len, key == FocusMove::Next),
                    None => 0,
                };
                self.focus = order.into_iter().nth(next);
            }
        }
    }

    fn move_row(&mut self, down: bool) {
        let rows = self.visible_rows();
        let Some(last) = rows.len().checked_sub(1) else {
            return;
        };
        let at = self
            .focus
            .as_ref()
            .and_then(Control::row)
            .and_then(|id| rows.iter().position(|r| *r == id));
        let next = match at {
            Some(index) if down => (index + 1).min(last),
            Some(index) => index.saturating_sub(1),
            None if down => 0,
            None => last,
        };
        self.focus = rows.get(next).map(|id| Control::Row((*id).to_owned()));
    }

    /// Gives the keyboard to `control`, as a click on it does.
    pub fn set_focus(&mut self, control: Control) {
        self.focus = Some(control);
    }

    /// Space or Enter on the focused control.
    pub fn activate(&mut self) -> Activation {
        match self.focus.clone() {
            Some(Control::Row(id)) => self.toggle(&id),
            Some(Control::Conversation(id)) => self.cycle(&id, true),
            Some(Control::RecoverAs(id)) => self.cycle_recover_as(&id, true),
            Some(Control::OtherToggle) => self.toggle_other(),
            Some(Control::SelectAll) => self.select_all(),
            Some(Control::Cancel) => return Activation::Cancel,
            Some(Control::Recover) => return Activation::Recover,
            None => {}
        }
        Activation::Handled
    }

    /// The request recovering the ticked rows in display order, sent as
    /// `request_id` at `now`. None while busy or with nothing ticked.
    pub fn recover_message(&mut self, request_id: &str, now: Instant) -> Option<ClientMessage> {
        if !self.recover_enabled() {
            return None;
        }
        let items: Vec<RecoverItem> = self
            .ordered_rows()
            .filter(|id| self.ticked.contains(*id))
            .filter_map(|id| self.rows.get(id).and_then(Row::item))
            .collect();
        if items.is_empty() {
            return None;
        }
        self.error = None;
        for row in self.rows.values_mut() {
            row.error = None;
        }
        self.in_flight = Some(InFlight {
            request_id: request_id.to_owned(),
            sent_at: now,
            requested: items.iter().map(|i| i.history_id.clone()).collect(),
            busy: true,
        });
        Some(ClientMessage::RecoverSessions {
            request_id: Some(request_id.to_owned()),
            items,
        })
    }

    /// The daemon's answer to `request_id`. All recovered: the dialog
    /// closes. Otherwise only the failed rows stay, ticked, each with its
    /// error. An answer to another request changes nothing.
    pub fn on_result(
        &mut self,
        request_id: Option<&str>,
        results: &[RecoverItemResult],
    ) -> Outcome {
        let ours = self
            .in_flight
            .as_ref()
            .is_some_and(|f| Some(f.request_id.as_str()) == request_id);
        if !ours {
            return Outcome::default();
        }
        let requested = self
            .in_flight
            .take()
            .map(|flight| flight.requested)
            .unwrap_or_default();
        let recovered_session_ids: Vec<String> = results
            .iter()
            .filter_map(|r| r.session_id.clone())
            .collect();
        let mut failed: HashMap<String, String> = HashMap::new();
        for result in results {
            if result.session_id.is_none() || result.error.is_some() {
                let error = result
                    .error
                    .clone()
                    .unwrap_or_else(|| "No session was started".to_owned());
                failed.insert(result.history_id.clone(), error);
            }
        }
        let answered: HashSet<&str> = results.iter().map(|r| r.history_id.as_str()).collect();
        for id in &requested {
            if !answered.contains(id.as_str()) {
                failed.insert(
                    id.clone(),
                    "Recovery did not answer for this session".to_owned(),
                );
            }
        }
        if failed.is_empty() {
            return Outcome {
                close: true,
                recovered_session_ids,
            };
        }
        self.keep_only(failed);
        Outcome {
            close: false,
            recovered_session_ids,
        }
    }

    /// Lists only the `failed` rows, ticked, each showing its error.
    fn keep_only(&mut self, failed: HashMap<String, String>) {
        self.rows.retain(|id, _| failed.contains_key(id));
        for (id, error) in failed {
            if let Some(row) = self.rows.get_mut(&id) {
                row.error = Some(error);
            }
        }
        for group in &mut self.groups {
            group.rows.retain(|id| self.rows.contains_key(id));
        }
        self.groups.retain(|g| !g.rows.is_empty());
        self.ticked = self.rows.keys().cloned().collect();
        self.user_set = self.rows.keys().cloned().collect();
        self.failed_only = true;
        self.other_expanded = self.other_expanded || self.has_other();
        self.focus = self.focus_order().into_iter().next();
    }

    /// Ends the wait once the request has gone unanswered for
    /// [`RECOVER_TIMEOUT`] at `now`: the controls work again and the error
    /// line says so. Returns whether it timed out now.
    pub fn check_timeout(&mut self, now: Instant) -> bool {
        let Some(flight) = self.in_flight.as_mut() else {
            return false;
        };
        if !flight.busy || now.saturating_duration_since(flight.sent_at) < RECOVER_TIMEOUT {
            return false;
        }
        flight.busy = false;
        self.error = Some("Recovery did not answer".to_owned());
        true
    }
}

/// Carries `old`'s conversation and "Recover as" choice (where `row` still
/// offers them) and its error over to `row`.
fn keep_choices(old: &Row, row: &mut Row) {
    let conversation = old
        .conversation
        .and_then(|index| old.conversations.get(index))
        .and_then(|(id, _)| row.conversations.iter().position(|(c, _)| c == id));
    if conversation.is_some() {
        row.conversation = conversation;
    }
    if let Some(index) = old
        .chosen()
        .and_then(|chosen| row.options.iter().position(|o| o.how == chosen.how))
    {
        row.option = index;
    }
    row.error.clone_from(&old.error);
}

/// `index` moved one step through `len` items, wrapping.
fn step(index: usize, len: usize, forward: bool) -> usize {
    if len == 0 {
        return 0;
    }
    if forward {
        (index + 1) % len
    } else {
        (index + len - 1) % len
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert preconditions with expect and panic; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use chrono::FixedOffset;
    use protocol::{AgentOptions, SpawnConfig, SpawnTarget, WorktreeReusePolicy};
    use serde_json::json;

    /// 2026-09-28 12:00 at UTC+2.
    fn now() -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339("2026-09-28T12:00:00+02:00").expect("now")
    }

    fn utc(at: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(at)
            .expect("time")
            .with_timezone(&Utc)
    }

    fn spawn_config() -> SpawnConfig {
        SpawnConfig {
            target: SpawnTarget::Single {
                repo_id: "r1".to_owned(),
                branch_name: "main".to_owned(),
                base_branch: None,
                use_worktree: true,
                checkout_strategy: None,
                worktree_reuse: WorktreeReusePolicy::default(),
                existing_worktree: None,
            },
            mode: SessionMode::Interactive,
            dangerously_skip_permissions: false,
            agent_options: AgentOptions::Claude {
                permission_mode: None,
            },
            model: None,
            extra_env: Vec::new(),
        }
    }

    /// A Claude session in repo `r1` that ended at `ended` (UTC) for `end`,
    /// with one conversation.
    fn item(id: &str, end: &serde_json::Value, ended: &str) -> SessionHistoryItem {
        let mut item: SessionHistoryItem = serde_json::from_value(json!({
            "entry": {
                "session_id": id, "label": format!("label-{id}"), "kind": "single",
                "mode": "interactive", "agent": "claude", "spawn_config": null,
                "members": [{"repo_id": "r1", "repo_name": "repo-one", "branch": "main", "worktree_path": "D:\\wt"}],
                "workspace_id": null, "primary_cwd": "D:\\wt", "current_cwd": null,
                "program_name": "claude", "started_at": null, "ended_at": ended, "end": end,
                "claude_session_id": null, "source": "record", "recovered_at": null
            },
            "candidates": [{"id": format!("c-{id}"), "last_active": ended, "title": "Fix it"}],
            "folder_is_git_repo": true, "folder_repo_id": null
        }))
        .expect("history item fixture");
        item.entry.spawn_config = Some(spawn_config());
        item
    }

    fn lost(id: &str, ended: &str) -> SessionHistoryItem {
        item(id, &json!({"type": "tracer_lost"}), ended)
    }

    fn stopped(id: &str, ended: &str) -> SessionHistoryItem {
        item(id, &json!({"type": "stopped_by_user"}), ended)
    }

    /// A plain shell in `D:\shell`, with no conversation.
    fn shell(id: &str, ended: &str) -> SessionHistoryItem {
        let mut item = lost(id, ended);
        item.entry.mode = SessionMode::PlainShell;
        item.entry.members.clear();
        item.entry.primary_cwd = Some("D:\\shell".to_owned());
        item.candidates.clear();
        item.folder_is_git_repo = false;
        item
    }

    fn candidate(id: &str, at: &str, title: Option<&str>) -> ConversationCandidate {
        ConversationCandidate {
            id: id.to_owned(),
            last_active: utc(at),
            title: title.map(str::to_owned),
        }
    }

    fn dialog(items: &[SessionHistoryItem]) -> RecoverDialog {
        RecoverDialog::new(items, &Names::default(), &now())
    }

    /// Each group as "<title> [<row ids>]".
    fn titles(groups: &[Group]) -> Vec<String> {
        groups
            .iter()
            .map(|g| format!("{} [{}]", g.title(), g.rows.join(" ")))
            .collect()
    }

    fn ok(id: &str) -> RecoverItemResult {
        RecoverItemResult {
            history_id: id.to_owned(),
            session_id: Some(format!("new-{id}")),
            error: None,
        }
    }

    fn failed(id: &str, error: &str) -> RecoverItemResult {
        RecoverItemResult {
            history_id: id.to_owned(),
            session_id: None,
            error: Some(error.to_owned()),
        }
    }

    #[test]
    fn badge_counts_unrecovered_tracer_lost_only() {
        let mut recovered = lost("b", "2026-09-28T09:00:00Z");
        recovered.entry.recovered_at = Some(utc("2026-09-28T09:10:00Z"));
        let exited = item(
            "d",
            &json!({"type": "exited", "code": 1}),
            "2026-09-28T09:00:00Z",
        );
        let mut history = SessionHistory::default();
        assert!(history.apply(&DaemonMessage::SessionHistory {
            request_id: None,
            items: vec![
                lost("a", "2026-09-28T09:00:00Z"),
                recovered,
                stopped("c", "2026-09-28T09:00:00Z"),
                exited,
            ],
        }));
        assert_eq!(history.badge_count(), 1);
        assert_eq!(history.items().len(), 4);
        assert!(history.apply(&DaemonMessage::SessionHistory {
            request_id: Some("r1".to_owned()),
            items: Vec::new(),
        }));
        assert_eq!(history.badge_count(), 0, "a reply replaces the list");
        assert!(!history.apply(&DaemonMessage::SessionRemoved {
            session_id: "a".to_owned()
        }));
    }

    #[test]
    fn bursts_within_60s_group_together() {
        let items = [
            lost("a", "2026-09-28T09:29:00Z"),
            lost("b", "2026-09-28T09:28:10Z"),
            lost("c", "2026-09-28T09:27:20Z"),
            lost("d", "2026-09-28T09:20:00Z"),
            lost("e", "2026-09-27T09:20:00Z"),
        ];
        let groups = groups(&items, &now());
        assert_eq!(
            titles(&groups),
            [
                "3 sessions lost at 11:29 [a b c]",
                "1 session lost at 11:20 [d]",
                "1 session lost at Sep 27 11:20 [e]",
            ],
            "chained within a minute of the previous, newest first, local time"
        );
        assert!(groups.iter().all(|g| g.kind == GroupKind::Burst));
    }

    #[test]
    fn unknown_time_group_comes_first() {
        let mut unknown = lost("u", "2026-09-20T09:00:00Z");
        unknown.entry.end_time_known = false;
        let items = [
            stopped("s", "2026-09-28T09:40:00Z"),
            lost("a", "2026-09-28T09:29:00Z"),
            unknown,
        ];
        let groups = groups(&items, &now());
        assert_eq!(
            titles(&groups),
            [
                "Lost at an unknown time [u]",
                "1 session lost at 11:29 [a]",
                "Other recent sessions [s]",
            ]
        );
        assert_eq!(groups[0].kind, GroupKind::UnknownTime);
    }

    #[test]
    fn other_sessions_group_collapsed_label() {
        let items = [
            stopped("s1", "2026-09-28T09:40:00Z"),
            item(
                "x",
                &json!({"type": "exited", "code": 0}),
                "2026-09-28T09:50:00Z",
            ),
            item(
                "d",
                &json!({"type": "daemon_shutdown"}),
                "2026-09-28T09:30:00Z",
            ),
        ];
        let mut dialog = dialog(&items);
        assert_eq!(titles(dialog.groups()), ["Other recent sessions [x s1 d]"]);
        let row = dialog.row("x").expect("row");
        assert_eq!(
            (row.label.as_str(), row.place.as_str(), row.ended.as_str()),
            ("label-x", "repo-one", "11:50 · exited (code 0)")
        );
        assert!(!dialog.other_expanded());
        assert!(dialog.visible_rows().is_empty(), "collapsed");
        assert_eq!(other_toggle_label(3, false), "Show 3 other recent sessions");
        assert_eq!(other_toggle_label(1, false), "Show 1 other recent session");
        dialog.toggle_other();
        assert_eq!(dialog.visible_rows(), ["x", "s1", "d"]);
        assert_eq!(other_toggle_label(3, true), "Hide other recent sessions");
        assert_eq!(dialog.ticked_count(), 0, "user ends are listed unticked");
    }

    #[test]
    fn ended_text_formats_each_end_and_dates_other_days() {
        let text =
            |end: serde_json::Value, at: &str| ended_text(&item("a", &end, at).entry, &now());
        let today = "2026-09-28T09:05:00Z";
        assert_eq!(text(json!({"type": "tracer_lost"}), today), "11:05 · lost");
        assert_eq!(
            text(json!({"type": "exited", "code": 3}), today),
            "11:05 · exited (code 3)"
        );
        assert_eq!(
            text(json!({"type": "stopped_by_user"}), today),
            "11:05 · stopped"
        );
        assert_eq!(
            text(json!({"type": "daemon_shutdown"}), today),
            "11:05 · daemon shut down"
        );
        assert_eq!(
            text(json!({"type": "tracer_lost"}), "2026-09-27T21:30:00Z"),
            "Sep 27 23:30 · lost",
            "another local day carries its date"
        );
        assert_eq!(
            text(json!({"type": "tracer_lost"}), "2026-09-27T22:30:00Z"),
            "00:30 · lost",
            "the day is the local one, not UTC's"
        );
        let mut unknown = lost("u", today).entry;
        unknown.end_time_known = false;
        assert_eq!(ended_text(&unknown, &now()), "time unknown · lost");
    }

    #[test]
    fn pre_ticks_unrecovered_lost_rows() {
        let mut recovered = lost("r", "2026-09-28T09:00:00Z");
        recovered.entry.recovered_at = Some(utc("2026-09-28T09:10:00Z"));
        let mut no_conversation = lost("n", "2026-09-28T09:00:00Z");
        no_conversation.candidates.clear();
        let items = [
            lost("a", "2026-09-28T09:00:00Z"),
            recovered,
            no_conversation,
            stopped("s", "2026-09-28T09:00:00Z"),
            shell("sh", "2026-09-28T09:00:00Z"),
        ];
        let dialog = dialog(&items);
        let ticked: Vec<&str> = ["a", "r", "n", "s", "sh"]
            .into_iter()
            .filter(|id| dialog.is_ticked(id))
            .collect();
        assert_eq!(ticked, ["a", "sh"]);
        assert_eq!(dialog.recover_label(), "Recover 2");
    }

    #[test]
    fn recovered_and_candidate_less_rows_disabled_with_reason() {
        let mut recovered = lost("r", "2026-09-28T09:00:00Z");
        recovered.entry.recovered_at = Some(utc("2026-09-28T09:10:00Z"));
        let mut earlier = lost("e", "2026-09-26T09:00:00Z");
        earlier.entry.recovered_at = Some(utc("2026-09-26T09:10:00Z"));
        let mut none = lost("n", "2026-09-28T09:00:00Z");
        none.candidates.clear();
        let mut dialog = dialog(&[recovered, earlier, none, lost("a", "2026-09-28T09:00:00Z")]);
        let reason = |d: &RecoverDialog, id: &str| d.row(id).expect("row").disabled.clone();
        assert_eq!(reason(&dialog, "r").as_deref(), Some("recovered 11:10"));
        assert_eq!(
            reason(&dialog, "e").as_deref(),
            Some("recovered Sep 26 11:10")
        );
        assert_eq!(
            reason(&dialog, "n").as_deref(),
            Some("no conversation found")
        );
        assert_eq!(reason(&dialog, "a"), None);
        dialog.toggle("n");
        dialog.select_all();
        assert!(!dialog.is_ticked("n"), "a disabled row never ticks");
        assert!(!dialog.is_ticked("r"));
    }

    #[test]
    fn recover_as_options_follow_folder_flags() {
        let options = |item: &SessionHistoryItem, names: &Names| {
            let (options, default) = recover_options(item, names);
            let labels: Vec<String> = options.iter().map(|o| o.label.clone()).collect();
            (labels, options.get(default).map(|o| o.how.clone()))
        };
        let mut folder_only = lost("f", "2026-09-28T09:00:00Z");
        folder_only.entry.spawn_config = None;
        folder_only.entry.current_cwd = Some("D:\\proj".to_owned());
        let mut names = Names::default();
        names.repos.insert("r9".to_owned(), "proj".to_owned());

        let mut registered = folder_only.clone();
        registered.folder_repo_id = Some("r9".to_owned());
        assert_eq!(
            options(&registered, &names),
            (
                vec![
                    "Claude session in proj".to_owned(),
                    "Shell running claude --resume".to_owned()
                ],
                Some(RecoverAs::Claude)
            ),
            "registered: Claude first and chosen"
        );

        assert_eq!(
            options(&folder_only, &names),
            (
                vec![
                    "Register D:\\proj as a repo, then Claude session".to_owned(),
                    "Shell running claude --resume".to_owned()
                ],
                Some(RecoverAs::Shell)
            ),
            "an unregistered git folder: register offered, shell chosen"
        );

        let mut plain = folder_only.clone();
        plain.folder_is_git_repo = false;
        assert_eq!(
            options(&plain, &names),
            (
                vec!["Shell running claude --resume".to_owned()],
                Some(RecoverAs::Shell)
            )
        );

        let claude = lost("c", "2026-09-28T09:00:00Z");
        let dialog = dialog(std::slice::from_ref(&claude));
        let row = dialog.row("c").expect("row");
        assert!(!row.shows_recover_as, "a Claude session recovers as itself");
        assert_eq!(row.chosen().map(|o| o.how.clone()), Some(RecoverAs::Claude));
    }

    #[test]
    fn plain_shell_without_candidate_offers_plain_shell() {
        let mut dialog = dialog(&[shell("sh", "2026-09-28T09:00:00Z")]);
        let row = dialog.row("sh").expect("row");
        assert_eq!(row.disabled, None, "stays enabled");
        let labels: Vec<&str> = row.options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(labels, ["Plain shell"]);
        let msg = dialog
            .recover_message("q1", Instant::now())
            .expect("a request");
        let ClientMessage::RecoverSessions { items, .. } = msg else {
            panic!("expected recover_sessions");
        };
        assert_eq!(
            items,
            [RecoverItem {
                history_id: "sh".to_owned(),
                conversation_id: None,
                how: RecoverAs::Shell,
            }]
        );
    }

    #[test]
    fn default_conversation_known_else_newest() {
        let mut known = lost("k", "2026-09-28T09:00:00Z");
        known.candidates = vec![
            candidate("new", "2026-09-28T09:00:00Z", Some("Newest")),
            candidate("mine", "2026-09-28T08:00:00Z", None),
        ];
        known.entry.claude_session_id = Some("mine".to_owned());
        let mut unknown = known.clone();
        unknown.entry.session_id = "u".to_owned();
        unknown.entry.claude_session_id = None;
        unknown.candidates.reverse();
        let mut dialog = dialog(&[known, unknown]);

        let chosen = |d: &RecoverDialog, id: &str| {
            let row = d.row(id).expect("row");
            row.conversation
                .and_then(|i| row.conversations.get(i))
                .map(|(id, label)| (id.clone(), label.clone()))
        };
        assert_eq!(
            chosen(&dialog, "k"),
            Some(("mine".to_owned(), "untitled · 2h ago".to_owned()))
        );
        assert_eq!(
            chosen(&dialog, "u"),
            Some(("new".to_owned(), "Newest · 1h ago".to_owned()))
        );
        dialog.cycle("k", true);
        assert_eq!(chosen(&dialog, "k").map(|c| c.0).as_deref(), Some("new"));
        dialog.cycle("k", true);
        assert_eq!(chosen(&dialog, "k").map(|c| c.0).as_deref(), Some("mine"));
        dialog.cycle("k", false);
        assert_eq!(chosen(&dialog, "k").map(|c| c.0).as_deref(), Some("new"));
    }

    #[test]
    fn recover_message_carries_ticked_items_in_order() {
        let items = [
            lost("old", "2026-09-28T08:00:00Z"),
            lost("new", "2026-09-28T09:00:00Z"),
            stopped("s", "2026-09-28T09:30:00Z"),
        ];
        let mut dialog = dialog(&items);
        dialog.toggle_other();
        dialog.toggle("s");
        let msg = dialog
            .recover_message("q1", Instant::now())
            .expect("a request");
        let ClientMessage::RecoverSessions { request_id, items } = msg else {
            panic!("expected recover_sessions");
        };
        assert_eq!(request_id.as_deref(), Some("q1"));
        let sent: Vec<(&str, Option<&str>)> = items
            .iter()
            .map(|i| (i.history_id.as_str(), i.conversation_id.as_deref()))
            .collect();
        assert_eq!(
            sent,
            [
                ("new", Some("c-new")),
                ("old", Some("c-old")),
                ("s", Some("c-s"))
            ],
            "display order: newest burst first, the other sessions last"
        );
        assert!(dialog.is_busy());
        assert_eq!(dialog.recover_label(), "Recovering…");
        assert!(
            dialog.recover_message("q2", Instant::now()).is_none(),
            "one request at a time"
        );
    }

    #[test]
    fn result_with_failures_keeps_failed_rows_reticked() {
        let items = [
            lost("a", "2026-09-28T09:00:00Z"),
            lost("b", "2026-09-28T09:00:30Z"),
            stopped("s", "2026-09-28T09:30:00Z"),
        ];
        let mut dialog = dialog(&items);
        dialog
            .recover_message("q1", Instant::now())
            .expect("a request");
        let outcome = dialog.on_result(Some("q1"), &[ok("b"), failed("a", "worktree gone")]);
        assert_eq!(
            outcome,
            Outcome {
                close: false,
                recovered_session_ids: vec!["new-b".to_owned()],
            }
        );
        assert!(!dialog.is_busy());
        assert_eq!(dialog.visible_rows(), ["a"], "only the failed row stays");
        assert_eq!(
            titles(dialog.groups()),
            ["1 session lost at 11:00 [a]"],
            "the title counts the rows left"
        );
        assert!(dialog.row("b").is_none() && dialog.row("s").is_none());
        assert!(dialog.is_ticked("a"));
        assert_eq!(
            dialog.row("a").expect("row").error.as_deref(),
            Some("worktree gone")
        );
        assert_eq!(dialog.recover_label(), "Recover 1");
    }

    #[test]
    fn all_ok_result_closes() {
        let mut dialog = dialog(&[
            lost("a", "2026-09-28T09:00:00Z"),
            lost("b", "2026-09-28T09:00:30Z"),
        ]);
        dialog
            .recover_message("q1", Instant::now())
            .expect("a request");
        let outcome = dialog.on_result(Some("q1"), &[ok("b"), ok("a")]);
        assert_eq!(
            outcome,
            Outcome {
                close: true,
                recovered_session_ids: vec!["new-b".to_owned(), "new-a".to_owned()],
            }
        );
    }

    #[test]
    fn foreign_request_result_ignored() {
        let mut dialog = dialog(&[lost("a", "2026-09-28T09:00:00Z")]);
        assert_eq!(dialog.on_result(Some("q1"), &[ok("a")]), Outcome::default());
        dialog
            .recover_message("q1", Instant::now())
            .expect("a request");
        assert_eq!(
            dialog.on_result(Some("other"), &[ok("a")]),
            Outcome::default()
        );
        assert_eq!(dialog.on_result(None, &[ok("a")]), Outcome::default());
        assert!(dialog.is_busy(), "still waiting for its own answer");
        assert!(dialog.row("a").is_some());
    }

    #[test]
    fn timeout_clears_busy_with_message() {
        let mut dialog = dialog(&[lost("a", "2026-09-28T09:00:00Z")]);
        let sent = Instant::now();
        dialog.recover_message("q1", sent).expect("a request");
        assert!(!dialog.check_timeout(sent + Duration::from_secs(119)));
        assert!(dialog.is_busy());
        assert!(dialog.check_timeout(sent + RECOVER_TIMEOUT));
        assert!(!dialog.is_busy());
        assert_eq!(dialog.error(), Some("Recovery did not answer"));
        assert_eq!(dialog.recover_label(), "Recover 1", "controls work again");
        assert!(
            !dialog.check_timeout(sent + Duration::from_secs(500)),
            "times out once"
        );
        let late = dialog.on_result(Some("q1"), &[ok("a")]);
        assert!(late.close, "a late answer still applies");
    }

    #[test]
    fn rows_ticked_after_the_request_are_not_failed() {
        let mut dialog = dialog(&[
            lost("a", "2026-09-28T09:00:00Z"),
            stopped("s", "2026-09-28T09:30:00Z"),
        ]);
        let sent = Instant::now();
        dialog.recover_message("q1", sent).expect("a request");
        assert!(dialog.check_timeout(sent + RECOVER_TIMEOUT));
        dialog.toggle_other();
        dialog.toggle("s");
        assert!(dialog.is_ticked("s"), "ticked after the request");
        let outcome = dialog.on_result(Some("q1"), &[ok("a")]);
        assert!(outcome.close, "s was never asked for, so nothing failed");
    }

    #[test]
    fn empty_history_says_so() {
        let dialog = dialog(&[]);
        assert_eq!(dialog.empty_text(), Some("No ended sessions to recover."));
        assert!(!dialog.recover_enabled());
        assert_eq!(dialog.recover_label(), "Recover 0");
    }

    #[test]
    fn focus_moves_rows_and_controls() {
        let mut none = lost("n", "2026-09-28T08:00:00Z");
        none.candidates.clear();
        let mut folder_only = lost("f", "2026-09-28T09:00:00Z");
        folder_only.entry.spawn_config = None;
        let items = [folder_only, none, stopped("s", "2026-09-28T09:30:00Z")];
        let mut dialog = dialog(&items);
        let row = |id: &str| Control::Row(id.to_owned());
        assert_eq!(dialog.focus(), Some(&row("f")), "the first row");
        assert_eq!(
            dialog.focus_order(),
            [
                row("f"),
                Control::Conversation("f".to_owned()),
                Control::RecoverAs("f".to_owned()),
                row("n"),
                Control::OtherToggle,
                Control::SelectAll,
                Control::Cancel,
                Control::Recover,
            ],
            "a disabled row has no choices; the collapsed rows are skipped"
        );
        dialog.move_focus(FocusMove::Down);
        assert_eq!(dialog.focus(), Some(&row("n")));
        dialog.move_focus(FocusMove::Down);
        assert_eq!(dialog.focus(), Some(&row("n")), "hidden rows are skipped");
        dialog.move_focus(FocusMove::Up);
        dialog.move_focus(FocusMove::Next);
        assert_eq!(dialog.focus(), Some(&Control::Conversation("f".to_owned())));
        dialog.move_focus(FocusMove::Down);
        assert_eq!(dialog.focus(), Some(&row("n")), "down from a row's choice");
        dialog.move_focus(FocusMove::Prev);
        dialog.move_focus(FocusMove::Prev);
        dialog.move_focus(FocusMove::Prev);
        dialog.move_focus(FocusMove::Prev);
        assert_eq!(dialog.focus(), Some(&Control::Recover), "Shift-Tab wraps");
        assert_eq!(dialog.activate(), Activation::Recover);

        dialog.move_focus(FocusMove::Next);
        assert_eq!(dialog.focus(), Some(&row("f")), "Tab wraps");
        assert_eq!(dialog.activate(), Activation::Handled);
        assert!(!dialog.is_ticked("f"), "space unticks the row");

        dialog.toggle_other();
        dialog.move_focus(FocusMove::Up);
        dialog.move_focus(FocusMove::Up);
        for _ in 0..3 {
            dialog.move_focus(FocusMove::Down);
        }
        assert_eq!(dialog.focus(), Some(&row("s")), "expanded rows take focus");
        dialog.toggle_other();
        assert_eq!(
            dialog.focus(),
            Some(&Control::OtherToggle),
            "a hidden row gives focus to its toggle"
        );
    }

    /// A Codex or Cursor session in repo `r1`, lost at `ended`, with no
    /// recorded conversation id.
    fn own_agent(id: &str, agent: Agent, ended: &str) -> SessionHistoryItem {
        let mut item = lost(id, ended);
        item.entry.agent = agent;
        item
    }

    /// The one item a dialog of one row sends.
    fn sent(dialog: &mut RecoverDialog) -> Vec<RecoverItem> {
        let msg = dialog
            .recover_message("q1", Instant::now())
            .expect("a request");
        let ClientMessage::RecoverSessions { items, .. } = msg else {
            panic!("expected recover_sessions");
        };
        items
    }

    #[test]
    fn codex_resumable_row_sends_own_agent_with_its_id() {
        let mut codex = own_agent("x", Agent::Codex, "2026-09-28T09:00:00Z");
        codex.entry.agent_conversation_id = Some("rollout-7".to_owned());
        codex.own_agent_resumable = true;
        let mut dialog = dialog(&[codex]);
        let row = dialog.row("x").expect("row");
        assert_eq!(
            row.options,
            [RecoverOption {
                how: RecoverAs::OwnAgent,
                label: "Codex session".to_owned(),
                resumes: true,
            }]
        );
        assert_eq!(
            row.conversations,
            [("rollout-7".to_owned(), "its own conversation".to_owned())]
        );
        assert_eq!(
            row.fixed_how.as_deref(),
            Some("Codex session, resumes its conversation")
        );
        assert!(row.disabled.is_none());
        assert!(!row.shows_recover_as, "one choice needs no Recover as");
        assert!(dialog.is_ticked("x"), "a resumable loss is pre-ticked");
        assert_eq!(
            sent(&mut dialog),
            [RecoverItem {
                history_id: "x".to_owned(),
                conversation_id: Some("rollout-7".to_owned()),
                how: RecoverAs::OwnAgent,
            }]
        );
    }

    #[test]
    fn codex_fresh_row_sends_own_agent_without_id() {
        let codex = own_agent("x", Agent::Codex, "2026-09-28T09:00:00Z");
        let mut dialog = dialog(&[codex]);
        let row = dialog.row("x").expect("row");
        assert!(row.conversations.is_empty(), "no conversation to send");
        assert!(!row.chosen().expect("chosen").resumes);
        assert_eq!(
            row.fixed_how.as_deref(),
            Some("Codex session, fresh run: no conversation recorded")
        );
        dialog.toggle("x");
        assert_eq!(
            sent(&mut dialog),
            [RecoverItem {
                history_id: "x".to_owned(),
                conversation_id: None,
                how: RecoverAs::OwnAgent,
            }]
        );
    }

    #[test]
    fn codex_row_with_claude_candidates_offers_no_claude_choice() {
        let mut codex = own_agent("x", Agent::Codex, "2026-09-28T09:00:00Z");
        codex.entry.agent_conversation_id = Some("rollout-7".to_owned());
        codex.own_agent_resumable = true;
        codex.candidates = vec![candidate("c-x", "2026-09-28T09:00:00Z", Some("Fix it"))];
        let dialog = dialog(&[codex]);
        let row = dialog.row("x").expect("row");
        let labels: Vec<&str> = row.options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(
            labels,
            ["Codex session"],
            "a Codex entry never offers a Claude conversation"
        );
        assert_eq!(
            row.conversations,
            [("rollout-7".to_owned(), "its own conversation".to_owned())],
            "its own id, not the Claude candidate"
        );
    }

    #[test]
    fn codex_row_without_spawn_config_is_disabled_with_reason() {
        let mut codex = own_agent("x", Agent::Codex, "2026-09-28T09:00:00Z");
        codex.entry.spawn_config = None;
        codex.entry.agent_conversation_id = Some("rollout-7".to_owned());
        codex.own_agent_resumable = true;
        let mut dialog = dialog(&[codex]);
        let row = dialog.row("x").expect("row");
        assert!(row.options.is_empty());
        assert_eq!(
            row.disabled.as_deref(),
            Some("no spawn settings recorded"),
            "not the Claude reason"
        );
        assert!(row.fixed_how.is_none(), "no choice to describe");
        dialog.select_all();
        assert!(!dialog.is_ticked("x"), "a disabled row never ticks");
    }

    #[test]
    fn cursor_row_is_labelled_cursor_session() {
        let mut cursor = own_agent("x", Agent::Cursor, "2026-09-28T09:00:00Z");
        cursor.entry.agent_conversation_id = Some("chat-9".to_owned());
        cursor.own_agent_resumable = true;
        let dialog = dialog(&[cursor]);
        let row = dialog.row("x").expect("row");
        assert_eq!(row.chosen().expect("chosen").label, "Cursor session");
        assert_eq!(
            row.conversations,
            [("chat-9".to_owned(), "its own conversation".to_owned())]
        );
        assert_eq!(
            row.fixed_how.as_deref(),
            Some("Cursor session, resumes its conversation")
        );
    }

    #[test]
    fn codex_gone_conversation_says_so() {
        let mut codex = own_agent("x", Agent::Codex, "2026-09-28T09:00:00Z");
        codex.entry.agent_conversation_id = Some("rollout-7".to_owned());
        let dialog = dialog(&[codex]);
        let row = dialog.row("x").expect("row");
        assert_eq!(
            row.fixed_how.as_deref(),
            Some("Codex session, fresh run: its conversation is gone")
        );
        assert!(!row.chosen().expect("chosen").resumes);
        assert!(row.conversations.is_empty());
    }

    #[test]
    fn fresh_run_own_agent_row_is_not_preticked() {
        let mut resumable = own_agent("r", Agent::Codex, "2026-09-28T09:00:00Z");
        resumable.entry.agent_conversation_id = Some("rollout-7".to_owned());
        resumable.own_agent_resumable = true;
        let fresh = own_agent("f", Agent::Cursor, "2026-09-28T09:00:30Z");
        let dialog = dialog(&[resumable, fresh, lost("c", "2026-09-28T09:00:10Z")]);
        let ticked: Vec<&str> = ["r", "f", "c"]
            .into_iter()
            .filter(|id| dialog.is_ticked(id))
            .collect();
        assert_eq!(
            ticked,
            ["r", "c"],
            "a fresh own-agent run is left out, a resumable one joins the Claude losses"
        );
    }

    #[test]
    fn claude_row_is_unchanged() {
        let mut claude = lost("c", "2026-09-28T09:00:00Z");
        claude.entry.agent_conversation_id = Some("not-a-claude-id".to_owned());
        let mut dialog = dialog(&[claude]);
        let row = dialog.row("c").expect("row");
        assert_eq!(row.chosen().expect("chosen").label, "Claude session");
        assert!(row.fixed_how.is_none(), "a Claude row describes nothing");
        assert_eq!(
            row.conversations,
            [("c-c".to_owned(), "Fix it · 1h ago".to_owned())]
        );
        assert!(!row.shows_recover_as);
        assert!(dialog.is_ticked("c"));
        assert_eq!(
            sent(&mut dialog),
            [RecoverItem {
                history_id: "c".to_owned(),
                conversation_id: Some("c-c".to_owned()),
                how: RecoverAs::Claude,
            }]
        );
    }

    #[test]
    fn resumable_flag_without_id_is_a_fresh_run() {
        let mut codex = own_agent("x", Agent::Codex, "2026-09-28T09:00:00Z");
        codex.own_agent_resumable = true;
        let mut dialog = dialog(&[codex]);
        let row = dialog.row("x").expect("row");
        assert!(!row.chosen().expect("chosen").resumes, "no id to resume");
        assert!(row.conversations.is_empty());
        assert_eq!(
            row.fixed_how.as_deref(),
            Some("Codex session, fresh run: no conversation recorded")
        );
        assert!(!dialog.is_ticked("x"), "a fresh run is not pre-ticked");
        dialog.toggle("x");
        assert_eq!(
            sent(&mut dialog),
            [RecoverItem {
                history_id: "x".to_owned(),
                conversation_id: None,
                how: RecoverAs::OwnAgent,
            }]
        );
    }

    #[test]
    fn recovered_own_agent_row_has_no_fixed_how() {
        let mut codex = own_agent("x", Agent::Codex, "2026-09-28T09:00:00Z");
        codex.entry.agent_conversation_id = Some("rollout-7".to_owned());
        codex.own_agent_resumable = true;
        codex.entry.recovered_at = Some(utc("2026-09-28T09:10:00Z"));
        let dialog = dialog(&[codex]);
        let row = dialog.row("x").expect("row");
        assert_eq!(row.disabled.as_deref(), Some("recovered 11:10"));
        assert!(row.fixed_how.is_none(), "only the recovered stamp shows");
        assert!(!dialog.is_ticked("x"));
    }

    #[test]
    fn refresh_unticks_an_own_agent_row_that_can_no_longer_resume() {
        let mut codex = own_agent("x", Agent::Codex, "2026-09-28T09:00:00Z");
        codex.entry.agent_conversation_id = Some("rollout-7".to_owned());
        codex.own_agent_resumable = true;
        let mut dialog = dialog(std::slice::from_ref(&codex));
        assert!(dialog.is_ticked("x"), "pre-ticked while it can resume");
        let mut gone = codex;
        gone.own_agent_resumable = false;
        dialog.refresh(&[gone], &Names::default(), &now());
        assert!(dialog.row("x").is_some_and(Row::enabled));
        assert!(!dialog.is_ticked("x"), "the rule's tick is withdrawn");
        assert_eq!(dialog.recover_label(), "Recover 0");
    }

    #[test]
    fn refresh_keeps_a_user_tick_on_a_fresh_run_row() {
        let codex = own_agent("x", Agent::Codex, "2026-09-28T09:00:00Z");
        let mut dialog = dialog(std::slice::from_ref(&codex));
        assert!(!dialog.is_ticked("x"));
        dialog.toggle("x");
        dialog.refresh(&[codex], &Names::default(), &now());
        assert!(dialog.is_ticked("x"), "the user's tick stands");
    }

    #[test]
    fn refresh_keeps_a_user_untick_on_a_preticked_loss() {
        let loss = lost("b", "2026-09-28T09:00:00Z");
        let mut dialog = dialog(std::slice::from_ref(&loss));
        assert!(dialog.is_ticked("b"), "a loss is pre-ticked");
        dialog.toggle("b");
        dialog.refresh(&[loss], &Names::default(), &now());
        assert!(!dialog.is_ticked("b"), "the user's untick stands");
        assert_eq!(dialog.recover_label(), "Recover 0");
    }

    #[test]
    fn refresh_keeps_a_user_untick_on_a_failed_row() {
        let items = [
            lost("a", "2026-09-28T09:00:00Z"),
            lost("b", "2026-09-28T09:00:30Z"),
        ];
        let mut dialog = dialog(&items);
        dialog
            .recover_message("q1", Instant::now())
            .expect("a request");
        dialog.on_result(Some("q1"), &[ok("b"), failed("a", "worktree gone")]);
        assert!(dialog.is_ticked("a"), "the failed row is re-ticked");
        dialog.toggle("a");
        assert!(!dialog.is_ticked("a"), "the user unticked it");
        dialog.refresh(
            &[lost("a", "2026-09-28T09:00:00Z")],
            &Names::default(),
            &now(),
        );
        assert!(!dialog.is_ticked("a"), "the user's untick stands");
        assert_eq!(dialog.recover_label(), "Recover 0");
    }

    #[test]
    fn resumable_own_agent_row_with_an_expected_end_is_not_preticked() {
        let mut codex = own_agent("x", Agent::Codex, "2026-09-28T09:00:00Z");
        codex.entry.agent_conversation_id = Some("rollout-7".to_owned());
        codex.own_agent_resumable = true;
        codex.entry.end = SessionEnd::StoppedByUser;
        let dialog = dialog(&[codex]);
        assert!(!dialog.is_ticked("x"), "only a loss is pre-ticked");
        assert_eq!(dialog.recover_label(), "Recover 0");
    }

    #[test]
    fn resumable_own_agent_row_with_a_clean_end_is_not_preticked() {
        let mut codex = own_agent("x", Agent::Codex, "2026-09-28T09:00:00Z");
        codex.entry.agent_conversation_id = Some("rollout-7".to_owned());
        codex.own_agent_resumable = true;
        codex.entry.end = SessionEnd::Exited { code: 0 };
        let dialog = dialog(&[codex]);
        assert!(!dialog.is_ticked("x"), "only a loss is pre-ticked");
    }
}
