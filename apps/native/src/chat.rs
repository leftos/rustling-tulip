//! The chat model: a session's conversation rows by id with the display cap,
//! tool-card expansion, the one-line tool summary, the answer builders, the
//! prompt card stack with its digit grace, which panes show a chat, which
//! sessions' chats this connection has loaded, and when the chat follows its
//! bottom.
//!
//! It is pure data: it takes the clock as a parameter and depends on
//! `protocol` alone, so it can move into a shared client core unchanged.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use protocol::{ChatAnswerKind, ChatEntry, ChatPrompt, ChatPromptKind, ChatScreen, ClientMessage};

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
/// the count of older rows the display cap or a load left out, and the ids
/// that left since the last load.
#[derive(Debug, Default)]
pub struct ChatLog {
    entries: Vec<ChatEntry>,
    expanded: HashSet<String>,
    hidden: u32,
    gone: GoneIds,
}

impl ChatLog {
    /// Replaces every row with a load reply's rows, taking that reply's count
    /// of the older rows it left out; every card closes, and an id that left
    /// before is accepted again.
    pub fn load(&mut self, entries: Vec<ChatEntry>, hidden: u32) {
        self.entries = entries
            .into_iter()
            .filter(|entry| !matches!(entry, ChatEntry::Unknown))
            .collect();
        self.expanded.clear();
        self.gone.clear();
        self.hidden = hidden;
    }

    /// A load the daemon sent unasked: it replaces every row as a load does,
    /// but the cards the user opened stay open when their rows survive it.
    pub fn reset(&mut self, entries: Vec<ChatEntry>, hidden: u32) {
        let expanded = std::mem::take(&mut self.expanded);
        self.load(entries, hidden);
        self.expanded = expanded
            .into_iter()
            .filter(|id| self.get(id).is_some())
            .collect();
    }

    /// Drops the rows `ids` names and forgets their expansion; a later
    /// update for one of them is ignored until the next load.
    pub fn remove(&mut self, ids: &[String]) {
        for id in ids {
            self.entries
                .retain(|entry| entry_id(entry) != Some(id.as_str()));
            self.expanded.remove(id);
            self.gone.insert(id);
        }
    }

    /// Applies one append: the `removed` ids leave first, then `entries` are
    /// upserted, so an id both removed and carried comes back at the bottom.
    pub fn append(&mut self, entries: Vec<ChatEntry>, removed: &[String]) {
        self.remove(removed);
        for entry in &entries {
            if let Some(id) = entry_id(entry)
                && removed.iter().any(|gone| gone == id)
            {
                self.gone.remove(id);
            }
        }
        self.upsert(entries);
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
            PaneView::Chat => {
                self.views.insert(pane_id.to_owned(), view);
            }
        }
    }

    /// Switches pane `pane_id` between its terminal and its chat, returning
    /// what it shows now.
    pub fn toggle(&mut self, pane_id: &str) -> PaneView {
        let next = match self.get(pane_id) {
            PaneView::Terminal => PaneView::Chat,
            PaneView::Chat => PaneView::Terminal,
        };
        self.set(pane_id, next);
        next
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

/// The message that answers `prompt` with `answer` for `session_id`.
#[must_use]
pub fn answer_message(
    session_id: &str,
    prompt: &ChatPrompt,
    answer: ChatAnswerKind,
    request_id: Option<String>,
) -> ClientMessage {
    ClientMessage::ChatAnswer {
        session_id: session_id.to_owned(),
        prompt_id: prompt.id.clone(),
        answer,
        request_id,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::time::{Duration, Instant};

    use protocol::{
        ChatAnswerKind, ChatEntry, ChatOption, ChatPrompt, ChatPromptKind, ChatScreen,
        ClientMessage, ComposerState, ToolOutcome,
    };

    use super::{
        ChatLoads, ChatLog, DIGIT_GRACE, DISPLAY_CAP, LoadChanges, PaneView, PaneViews,
        PromptStack, allow_with_text, answer_message, back_answer, deny_with_text, entry_id,
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

    #[test]
    fn a_pane_shows_its_terminal_until_toggled() {
        let mut views = PaneViews::default();
        assert_eq!(views.get("p1"), PaneView::Terminal);
        assert_eq!(views.toggle("p1"), PaneView::Chat);
        assert!(views.is_chat("p1"));
        assert!(!views.is_chat("p2"), "another pane keeps its terminal");
        assert_eq!(views.toggle("p1"), PaneView::Terminal);
        assert!(!views.is_chat("p1"));
        views.set("p2", PaneView::Chat);
        assert!(views.is_chat("p2"));
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

    #[test]
    fn an_answer_message_names_the_session_and_the_prompt() {
        let prompt = test_prompt("prompt-7", ChatPromptKind::Permission, false, &[1]);
        let message = answer_message(
            "session-3",
            &prompt,
            ChatAnswerKind::Option { number: 1 },
            Some("req-9".to_owned()),
        );
        assert!(
            matches!(message, ClientMessage::ChatAnswer { .. }),
            "an answer is a ChatAnswer message"
        );
        if let ClientMessage::ChatAnswer {
            session_id,
            prompt_id,
            answer,
            request_id,
        } = message
        {
            assert_eq!(session_id, "session-3");
            assert_eq!(prompt_id, "prompt-7");
            assert_eq!(answer, ChatAnswerKind::Option { number: 1 });
            assert_eq!(request_id.as_deref(), Some("req-9"));
        }
        let plain = answer_message("session-3", &prompt, ChatAnswerKind::Submit, None);
        assert!(
            matches!(
                plain,
                ClientMessage::ChatAnswer {
                    request_id: None,
                    ..
                }
            ),
            "a request without an id stays without one"
        );
    }
}
