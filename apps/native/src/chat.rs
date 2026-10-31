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

use crate::stretchtext;

use chrono::{DateTime, TimeDelta, Utc};
use protocol::{
    ChatAnswerKind, ChatEntry, ChatPrompt, ChatPromptKind, ChatScreen, ClientMessage,
    ComposerState, WorkShell, WorkSubagent,
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
/// each answer's open stretches, the count of older rows the display cap or
/// a load left out, and the ids that left since the last load.
#[derive(Debug, Default)]
pub struct ChatLog {
    entries: Vec<ChatEntry>,
    expanded: HashSet<String>,
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
        self.stretchtext.clear();
        self.gone.clear();
        self.hidden = hidden;
    }

    /// A load the daemon sent unasked: it replaces every row as a load does,
    /// but the cards and stretches the user opened stay open when their rows
    /// survive it.
    pub fn reset(&mut self, entries: Vec<ChatEntry>, hidden: u32) {
        let expanded = std::mem::take(&mut self.expanded);
        let stretchtext = std::mem::take(&mut self.stretchtext);
        self.load(entries, hidden);
        self.expanded = expanded
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
    /// expansion forgotten.
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
                self.stretchtext.remove(id);
                self.gone.insert(id);
            }
        }
        self.hidden = self
            .hidden
            .saturating_add(u32::try_from(excess).unwrap_or(u32::MAX));
    }
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
/// trimmed and cut to at most 80 characters with a closing ellipsis, or the
/// tool's name when the summary is empty.
#[must_use]
pub fn tool_one_liner(summary: &str, name: &str) -> String {
    let line = summary.lines().next().unwrap_or_default().trim();
    if line.is_empty() {
        return name.to_owned();
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

/// A session's chat send in flight. The daemon sends no success reply: a
/// send is in flight from `ChatSend` until its pending user entry arrives,
/// the screen's composer leaves the draft its paste made, or the daemon
/// refuses it. While it is in flight its paste is not a draft of the
/// owner's. The last settled send is kept, so a refusal that comes after
/// its entry still gives its text back.
#[derive(Debug, Default)]
pub struct SendTracker {
    in_flight: Option<Send>,
    settled: Option<Send>,
    /// The recent card answers that took the composer's text, by request
    /// id, so a refusal gives the text back even after the card left.
    taken: VecDeque<(String, String)>,
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
    }

    /// A card answer under `request_id` took `text` from the composer.
    pub fn took(&mut self, request_id: String, text: String) {
        self.taken.push_back((request_id, text));
        if self.taken.len() > TAKEN_CAP {
            self.taken.pop_front();
        }
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
        }
    }

    /// The screen's composer now reads `state`: once it has shown the
    /// send's paste as a draft, leaving the draft settles the send.
    pub fn on_composer(&mut self, state: &ComposerState) {
        let Some(send) = self.in_flight.as_mut() else {
            return;
        };
        if *state == ComposerState::Draft {
            send.saw_draft = true;
        } else if send.saw_draft || *state == ComposerState::Working {
            self.settle();
        }
    }

    /// The daemon refused `request_id`: the text of the send it names, in
    /// flight or the last settled one, which ends with it.
    pub fn refused(&mut self, request_id: &str) -> Option<String> {
        if self
            .in_flight
            .as_ref()
            .is_some_and(|send| send.request_id == request_id)
        {
            return self.in_flight.take().map(|send| send.text);
        }
        if self
            .settled
            .as_ref()
            .is_some_and(|send| send.request_id == request_id)
        {
            return self.settled.take().map(|send| send.text);
        }
        let taken = self.taken.iter().position(|(sent, _)| sent == request_id)?;
        self.taken.remove(taken).map(|(_, text)| text)
    }

    fn settle(&mut self) {
        self.settled = self.in_flight.take();
    }
}

/// How long a send stays in flight at most when nothing settles it sooner.
pub const SEND_BACKSTOP: Duration = Duration::from_secs(10);

/// How many card answers that took composer text are remembered.
const TAKEN_CAP: usize = 16;

/// The composer's text after a refusal gives `refused` back while it holds
/// `current`: the refused text alone, or before what was typed since.
#[must_use]
pub fn restored_text(refused: &str, current: &str) -> String {
    if current.is_empty() {
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
    /// The named overlay is open in the terminal.
    Overlay(String),
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
            Self::Overlay(name) => {
                format!("{name} is open in the terminal. Close it there to send.")
            }
            Self::Unknown => "Can't read the prompt box right now.".to_owned(),
        }
    }

    /// Whether the composer takes no input while it shows this.
    #[must_use]
    pub fn disables(&self) -> bool {
        matches!(self, Self::NotConnected)
    }
}

/// The note the composer shows: none while it can send. `connected` says
/// the daemon connection is open, `state` is the screen's last composer
/// state (none before the chat loaded), and `sending` says our own send is
/// in flight, whose paste reads as a draft.
#[must_use]
pub fn composer_note(
    connected: bool,
    state: Option<&ComposerState>,
    sending: bool,
) -> Option<ComposerNote> {
    if !connected {
        return Some(ComposerNote::NotConnected);
    }
    if sending {
        return Some(ComposerNote::Sending);
    }
    match state? {
        ComposerState::Draft => Some(ComposerNote::Draft),
        ComposerState::Overlay { name } => Some(ComposerNote::Overlay(name.clone())),
        ComposerState::Unknown => Some(ComposerNote::Unknown),
        _ => None,
    }
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
        follows_bottom, option_answer, skip_answer, submit_answer, toggle_answer, tool_one_liner,
        type_something_answer,
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
        }
    }

    /// A dialog screen showing `prompt`, or none.
    fn screen(prompt: Option<ChatPrompt>) -> ChatScreen {
        ChatScreen {
            composer: ComposerState::Dialog,
            prompt,
        }
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
    fn an_empty_summary_names_the_tool() {
        assert_eq!(tool_one_liner("", "Read"), "Read");
        assert_eq!(tool_one_liner("   ", "Bash"), "Bash");
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
    use protocol::{ChatEntry, ClientMessage, ComposerState, WorkShell, WorkSubagent};

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
    fn an_answer_that_took_text_gives_it_back_when_refused() {
        let mut sends = super::SendTracker::default();
        sends.took("r1".to_owned(), "careful".to_owned());
        assert_eq!(sends.refused("r2"), None);
        assert_eq!(sends.refused("r1"), Some("careful".to_owned()));
        assert_eq!(sends.refused("r1"), None);
    }

    #[test]
    fn refused_text_goes_back_before_what_was_typed_since() {
        assert_eq!(super::restored_text("hello", ""), "hello");
        assert_eq!(super::restored_text("hello", "more"), "hello\nmore");
    }

    #[test]
    fn the_composer_note_says_why_a_send_cannot_go() {
        use super::{ComposerNote, composer_note};
        assert_eq!(
            composer_note(true, Some(&ComposerState::Ready), false),
            None
        );
        assert_eq!(
            composer_note(true, Some(&ComposerState::Working), false),
            None
        );
        assert_eq!(
            composer_note(true, Some(&ComposerState::Dialog), false),
            None
        );
        assert_eq!(
            composer_note(true, Some(&ComposerState::Draft), false),
            Some(ComposerNote::Draft)
        );
        assert_eq!(
            composer_note(true, Some(&ComposerState::Draft), true),
            Some(ComposerNote::Sending),
            "our own send in flight reads as sending"
        );
        let overlay = ComposerState::Overlay {
            name: "/model".to_owned(),
        };
        assert_eq!(
            composer_note(true, Some(&overlay), false).map(|note| note.text()),
            Some("/model is open in the terminal. Close it there to send.".to_owned())
        );
        assert_eq!(
            composer_note(true, Some(&ComposerState::Unknown), false).map(|note| note.text()),
            Some("Can't read the prompt box right now.".to_owned())
        );
        assert_eq!(
            ComposerNote::Draft.text(),
            "The prompt box has a draft. Send or clear it in the terminal first."
        );
        assert_eq!(
            composer_note(false, Some(&ComposerState::Ready), false).map(|note| note.text()),
            Some("Not connected".to_owned())
        );
        assert!(ComposerNote::NotConnected.disables());
        assert!(!ComposerNote::Draft.disables());
        assert_eq!(composer_note(true, None, false), None);
    }
}
