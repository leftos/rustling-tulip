//! A session's chat drawn in a pane: its rows in a scrolling column that
//! follows the bottom until the owner scrolls up, the Terminal / Chat
//! toggle's work (from the header, the pane menu or Ctrl+Shift+M) with each
//! session's remembered view, the `LoadChat` / `UnloadChat` bookkeeping, and the folds
//! of the daemon's chat messages. The rows' model, which panes show a chat
//! and which chats are loaded live in [`crate::chat`].

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use chrono::{DateTime, TimeDelta, Utc};
use std::rc::Rc;

use gpui::{
    AnyElement, App, ClickEvent, Context, Div, ElementId, Entity, KeyDownEvent, Keystroke,
    ScrollHandle, SharedString, Subscription, Task, Window, WindowId, div, prelude::*, px,
    relative,
};
use protocol::error_codes::UNSUPPORTED_MESSAGE;
use protocol::{
    ChatAnswerKind, ChatEntry, ChatPrompt, ChatScreen, ClientMessage, ComposerState, DaemonMessage,
    SessionMode, SessionSnapshot, ToolOutcome,
};

use crate::buttons::{ButtonSize, Layer, field_frame, outlined_button, primary_button};
use crate::chat::{
    self, ChatLoads, ChatLog, ComposerNote, PaneView, PaneViews, PromptStack, RunningWork,
    SendTracker, Strip, ViewKey,
};
use crate::chat_cards::{
    ANSWERED_ELSEWHERE, ButtonAnswer, CardKey, CardState, CardView, ComposerAction, KeyAction,
    OnPress, card_answer_message, composer_actions, key_action, prompt_card, ride_no_label,
    ride_yes_label,
};
use crate::fonts::{self, DEFAULT_FAMILY, ui_px_at};
use crate::scrollbar::Scrollbar;
use crate::sidebar::LOCAL_HOST_KEY;
use crate::stretchtext;
use crate::summary::{SummaryLog, wanted_loads};
use crate::text_input::{TextChanged, TextInput, TextInputEvent};
use crate::ui_theme::{UiTheme, theme};
use crate::viewer_md_view::{LinkRoute, MdDoc, Painter, prepare_answer, render_row};
use crate::work_strip::strip_row;
use crate::{RootView, UI_TEXT_SIZE, new_request_id};

/// What a pane whose chat the daemon refused says above its terminal.
pub(crate) const UNSUPPORTED_NOTE: &str = "This session offers no chat, so it shows its terminal";
/// What a pane whose chat failed to load says above its terminal, before
/// the daemon's message.
const LOAD_FAILED_NOTE: &str = "Could not load the chat: ";
/// What a chat pane says while its first rows are on their way.
pub(crate) const LOADING_NOTE: &str = "Loading the chat…";
/// What a chat pane says when the conversation has no rows yet.
const EMPTY_NOTE: &str = "No messages yet";

/// What an empty composer says.
const COMPOSER_PLACEHOLDER: &str = "Send a message (Ctrl+Enter)";
/// How many rows the composer grows between.
const COMPOSER_MIN_ROWS: usize = 2;
const COMPOSER_MAX_ROWS: usize = 6;
/// How a label names an agent the client has no descriptor for.
const GENERIC_AGENT: &str = "the agent";

const TOGGLE_TO_CHAT_TIP: &str = "Show this session's chat";
const TOGGLE_TO_TERMINAL_TIP: &str = "Show this session's terminal";
const TOGGLE_TO_SUMMARY_TIP: &str = "Show this session's summary";

/// The room between rows, and around the column.
const ROW_GAP: f32 = 8.0;
pub(crate) const BODY_PAD_X: f32 = 14.0;
const BODY_PAD_Y: f32 = 12.0;
/// A user message's bubble: its corners, padding and widest share of the
/// column.
const BUBBLE_RADIUS: f32 = 10.0;
const BUBBLE_PAD_X: f32 = 10.0;
const BUBBLE_PAD_Y: f32 = 6.0;
const BUBBLE_MAX_SHARE: f32 = 0.8;
/// How a message not yet in the transcript is dimmed.
const PENDING_OPACITY: f32 = 0.6;
/// A tool card's corners, padding and text size.
const CARD_RADIUS: f32 = 8.0;
const CARD_PAD_X: f32 = 8.0;
const CARD_PAD_Y: f32 = 5.0;
const CARD_TEXT_SIZE: f32 = 12.0;
/// The size of a muted row's text, and of the live answer's tag.
pub(crate) const SMALL_TEXT_SIZE: f32 = 12.0;
const TAG_TEXT_SIZE: f32 = 11.0;
/// The room right of a live answer's border, and its cursor block.
const LIVE_PAD: f32 = 8.0;
const CURSOR_WIDTH: f32 = 7.0;
const CURSOR_HEIGHT: f32 = 14.0;
/// A system row's pill padding.
const PILL_PAD_X: f32 = 10.0;
const PILL_PAD_Y: f32 = 2.0;

/// Whether `session` offers its chat: an interactive agent session, not a
/// plain shell or a headless run.
pub(crate) fn offers_chat(session: &SessionSnapshot) -> bool {
    session.mode == SessionMode::Interactive && session.agent.is_some()
}

/// One session's loaded chat: its rows, its prompt cards, the `LoadChat`
/// whose reply is still awaited, and each answer's parsed Markdown.
#[derive(Default)]
pub(crate) struct SessionChat {
    log: ChatLog,
    stack: PromptStack,
    request: Option<String>,
    /// By answer id, its parsed document and what it was parsed at.
    docs: HashMap<String, AnswerDoc>,
    /// The work running beside the chat, for its strip; a `Chat` reply
    /// leaves it as it is.
    work: RunningWork,
    /// The screen's last composer state; none until the chat loaded.
    composer_state: Option<ComposerState>,
    /// The cards' busy and "Answered elsewhere" marks.
    cards: CardState,
    /// The chat send in flight.
    sends: SendTracker,
    /// The composer, made once the chat has loaded.
    composer: Option<Composer>,
    /// The daemon's reason for refusing the last send.
    send_error: Option<String>,
}

/// An answer's parsed document, with the text length, live flag and open
/// stretches it was parsed at: a change to any of them parses it again.
struct AnswerDoc {
    len: usize,
    live: bool,
    open: BTreeSet<String>,
    doc: Arc<MdDoc>,
}

/// A loaded chat's composer and its subscriptions.
pub(crate) struct Composer {
    input: Entity<TextInput>,
    _subscriptions: [Subscription; 2],
}

impl SessionChat {
    /// The chat's rows.
    pub(crate) fn log(&self) -> &ChatLog {
        &self.log
    }

    /// Whether the reply to this chat's load is still awaited.
    pub(crate) fn is_loading(&self) -> bool {
        self.request.is_some()
    }

    /// The work running beside the chat.
    pub(crate) fn work(&self) -> &RunningWork {
        &self.work
    }

    /// The composer's text, empty before the composer is made.
    fn composer_text(&self, cx: &App) -> String {
        self.composer
            .as_ref()
            .map(|composer| composer.input.read(cx).text().to_owned())
            .unwrap_or_default()
    }

    /// Puts `text` in the composer, the caret at its end.
    fn set_composer_text(&self, text: &str, cx: &mut App) {
        if let Some(composer) = &self.composer {
            let text = text.to_owned();
            composer
                .input
                .update(cx, |input, cx| input.set_text(text, cx));
        }
    }

    /// Gives a refused send's or answer's `text` back to the composer,
    /// before whatever was typed since.
    fn give_back(&self, text: &str, cx: &mut App) {
        let current = self.composer_text(cx);
        self.set_composer_text(&chat::restored_text(text, &current), cx);
    }

    /// Whether the card on screen was submitted and is still up.
    fn card_busy(&self) -> bool {
        self.stack
            .current()
            .is_some_and(|prompt| self.cards.is_busy(&prompt.id))
    }

    /// A screen reading: the card stack, the cards' marks, the send in
    /// flight and the composer state follow it.
    fn take_screen(&mut self, screen: &ChatScreen, now: Instant) {
        let before = self.stack.current().map(|prompt| prompt.id.clone());
        self.stack.on_screen(screen, now);
        let after = screen.prompt.as_ref().map(|prompt| prompt.id.as_str());
        self.cards.on_screen(before.as_deref(), after);
        self.sends.on_composer(&screen.composer);
        self.composer_state = Some(screen.composer.clone());
    }

    /// Applies one chat message: a `Chat` that answers this chat's load or
    /// that the daemon sent unasked (a reset that keeps open cards open), an
    /// append, a screen change, or the running work. A reply to an older
    /// load changes nothing. Returns whether it applied.
    fn apply(&mut self, msg: DaemonMessage, now: Instant) -> bool {
        match msg {
            DaemonMessage::Chat {
                entries,
                hidden,
                screen,
                request_id,
                ..
            } => {
                match request_id {
                    Some(id) if self.request.as_deref() == Some(id.as_str()) => {
                        self.request = None;
                        self.log.load(entries, hidden);
                    }
                    Some(_) => return false,
                    None => self.log.reset(entries, hidden),
                }
                self.take_screen(&screen, now);
                self.refresh_docs();
            }
            DaemonMessage::ChatAppend {
                entries, removed, ..
            } => {
                let new_entry = entries
                    .iter()
                    .filter_map(chat::entry_id)
                    .any(|id| self.log.get(id).is_none());
                if new_entry {
                    self.cards.on_new_entry();
                }
                self.sends.on_entries(&entries);
                self.log.append(entries, &removed);
                self.refresh_docs();
            }
            DaemonMessage::ChatScreenChanged { screen, .. } => self.take_screen(&screen, now),
            DaemonMessage::ChatWork {
                subagents,
                shells,
                wakeup_at,
                ..
            } => self.work.set(subagents, shells, wakeup_at),
            _ => return false,
        }
        true
    }

    /// Parses each answer whose text is new or changed length, whose live
    /// flag changed or whose stretches opened or closed since its last
    /// parse, and forgets the answers that left.
    fn refresh_docs(&mut self) {
        let mut parsed = std::mem::take(&mut self.docs);
        let cancel = AtomicBool::new(false);
        let none = BTreeSet::new();
        for entry in self.log.entries() {
            let ChatEntry::Answer { id, text, live } = entry else {
                continue;
            };
            let open = self.log.open_stretchtext(id).unwrap_or(&none);
            let doc = parsed
                .remove(id)
                .filter(|doc| doc.len == text.len() && doc.live == *live && doc.open == *open)
                .unwrap_or_else(|| {
                    let composed = stretchtext::compose(text, open, *live);
                    AnswerDoc {
                        len: text.len(),
                        live: *live,
                        open: open.clone(),
                        doc: Arc::new(prepare_answer(&composed, &cancel)),
                    }
                });
            self.docs.insert(id.clone(), doc);
        }
    }
}

/// The chat side of the panes: what each pane shows, the chats this
/// connection has loaded, each loaded session's chat, each chat pane's
/// scroll, the note a refused chat leaves on its pane, and the wall clock
/// the running-work strips read.
pub(crate) struct ChatPanes {
    views: PaneViews,
    loads: ChatLoads,
    sessions: HashMap<String, SessionChat>,
    /// The summaries this connection has loaded, and each one's lines.
    summary_loads: ChatLoads,
    pub(crate) summaries: HashMap<String, SummaryLog>,
    /// Each chat or summary pane's scroll.
    pub(crate) scrolls: HashMap<String, Scrollbar>,
    notes: HashMap<String, String>,
    /// The request ids of loads forgotten when the session's other load was
    /// refused, whose own refusal is taken without a word.
    refused_loads: BTreeSet<String>,
    wall: WallClock,
    /// Repaints the running-work strips while one shows, so their ages and
    /// countdowns keep counting.
    work_strip_timer: Option<Task<()>>,
    /// How many times that timer has repainted.
    work_strip_repaints: usize,
}

/// The wall time and the view's clock reading taken together, so the wall
/// time moves only as the view's clock does: a spec's clock moves both.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WallClock {
    wall: DateTime<Utc>,
    instant: Instant,
}

impl WallClock {
    /// The wall time now, paired with `instant`, the view's clock now.
    pub(crate) fn starting_at(instant: Instant) -> Self {
        Self {
            wall: Utc::now(),
            instant,
        }
    }

    /// The wall time when the view's clock reads `instant`.
    fn at(self, instant: Instant) -> DateTime<Utc> {
        let elapsed = instant.saturating_duration_since(self.instant);
        // Only a clock moved past chrono's range (about 292 million years)
        // fails here; the base is the answer then.
        TimeDelta::from_std(elapsed)
            .ok()
            .and_then(|elapsed| self.wall.checked_add_signed(elapsed))
            .unwrap_or(self.wall)
    }
}

impl ChatPanes {
    /// No pane on its chat and nothing loaded, the strips reading `wall`.
    pub(crate) fn new(wall: WallClock) -> Self {
        Self {
            views: PaneViews::default(),
            loads: ChatLoads::default(),
            sessions: HashMap::new(),
            summary_loads: ChatLoads::default(),
            summaries: HashMap::new(),
            scrolls: HashMap::new(),
            notes: HashMap::new(),
            refused_loads: BTreeSet::new(),
            wall,
            work_strip_timer: None,
            work_strip_repaints: 0,
        }
    }

    /// The wall clock the strips read, for a fresh set of chat panes.
    pub(crate) fn wall_clock(&self) -> WallClock {
        self.wall
    }

    /// The session `session_id`'s loaded chat.
    pub(crate) fn session(&self, session_id: &str) -> Option<&SessionChat> {
        self.sessions.get(session_id)
    }

    /// Pane `pane_id` closed or now shows another session: it shows a
    /// terminal again, with no note.
    pub(crate) fn forget_pane(&mut self, pane_id: &str) {
        self.views.set(pane_id, PaneView::Terminal);
        self.scrolls.remove(pane_id);
        self.notes.remove(pane_id);
    }

    /// The pane known as `old` is now `new`, with its view, scroll and note.
    pub(crate) fn rekey_pane(&mut self, old: &str, new: &str) {
        self.views.rekey(old, new);
        if let Some(scroll) = self.scrolls.remove(old) {
            self.scrolls.insert(new.to_owned(), scroll);
        }
        if let Some(note) = self.notes.remove(old) {
            self.notes.insert(new.to_owned(), note);
        }
    }

    /// A new connection has loaded no chat: every chat pane asks again once
    /// its pane is bound to its session.
    pub(crate) fn reset_connection(&mut self) {
        self.loads.clear();
        self.sessions.clear();
        self.summary_loads.clear();
        self.summaries.clear();
        self.refused_loads.clear();
        self.work_strip_timer = None;
    }

    /// The daemon refused session `session_id`'s chat load (`chat_refused`)
    /// or its summary load: that load is forgotten with nothing to unload,
    /// and so is the session's other load while its reply is still awaited,
    /// its request id kept so its own refusal is taken without a word.
    fn forget_refused_load(&mut self, session_id: &str, chat_refused: bool) {
        let other_pending = if chat_refused {
            self.summaries
                .get(session_id)
                .is_some_and(SummaryLog::is_loading)
        } else {
            self.sessions
                .get(session_id)
                .is_some_and(SessionChat::is_loading)
        };
        let chat = (chat_refused || other_pending)
            .then(|| self.forget_chat_load(session_id))
            .flatten();
        let summary = (!chat_refused || other_pending)
            .then(|| self.forget_summary_load(session_id))
            .flatten();
        let other = if chat_refused { summary } else { chat };
        self.refused_loads.extend(other);
    }

    /// Forgets session `session_id`'s chat and its load, returning the
    /// request id its reply was awaited under.
    fn forget_chat_load(&mut self, session_id: &str) -> Option<String> {
        self.loads.forget(session_id);
        self.sessions.remove(session_id)?.request
    }

    /// Forgets session `session_id`'s summary and its load, returning the
    /// request id its reply was awaited under.
    fn forget_summary_load(&mut self, session_id: &str) -> Option<String> {
        self.summary_loads.forget(session_id);
        self.summaries
            .remove(session_id)?
            .request()
            .map(str::to_owned)
    }

    /// Whether some loaded chat shows a running-work strip.
    fn shows_work(&self) -> bool {
        self.sessions.values().any(|chat| !chat.work.is_empty())
    }
}

/// What every row of one chat pane is drawn with.
pub(crate) struct RowLook<'a> {
    pub(crate) pane_id: &'a str,
    pub(crate) session_id: &'a str,
    pub(crate) mono: SharedString,
    pub(crate) mono_family: Option<&'a str>,
    pub(crate) mono_size: f32,
    /// The whole pixels added to the UI text sizes.
    pub(crate) text_offset: i8,
    pub(crate) theme: UiTheme,
}

/// The parts of a tool call a card draws.
struct ToolRow<'a> {
    id: &'a str,
    name: &'a str,
    summary: &'a str,
    input: &'a str,
    result: Option<&'a ToolOutcome>,
}

impl RootView {
    /// What pane `pane_id`, showing `session_id`, draws: the view it was
    /// switched to while the session offers a chat, else its terminal.
    pub(crate) fn view_shown(&self, pane_id: &str, session_id: Option<&str>) -> PaneView {
        let offers = session_id
            .and_then(|id| self.sidebar.session(id))
            .is_some_and(offers_chat);
        if offers {
            self.chat.views.get(pane_id)
        } else {
            PaneView::Terminal
        }
    }

    /// Whether pane `pane_id`, showing `session_id`, draws its chat or its
    /// summary in place of its terminal.
    pub(crate) fn terminal_hidden(&self, pane_id: &str, session_id: Option<&str>) -> bool {
        self.view_shown(pane_id, session_id).hides_terminal()
    }

    /// Whether pane `pane_id` draws its session's chat in place of its
    /// terminal.
    #[must_use]
    pub fn pane_shows_chat(&self, pane_id: &str) -> bool {
        self.pane_view(pane_id) == PaneView::Chat
    }

    /// What pane `pane_id` draws for its session.
    #[must_use]
    pub fn pane_view(&self, pane_id: &str) -> PaneView {
        let session = self.panes.get(pane_id).and_then(|slot| slot.session());
        self.view_shown(pane_id, session)
    }

    /// The note pane `pane_id` shows above its terminal after its chat was
    /// refused.
    #[must_use]
    pub fn pane_chat_note(&self, pane_id: &str) -> Option<&str> {
        self.chat.notes.get(pane_id).map(String::as_str)
    }

    /// The rows of session `session_id`'s chat, in display order; none
    /// while no pane shows it.
    #[must_use]
    pub fn chat_rows(&self, session_id: &str) -> Vec<ChatEntry> {
        self.chat
            .sessions
            .get(session_id)
            .map(|chat| chat.log.entries().cloned().collect())
            .unwrap_or_default()
    }

    /// Session `session_id`'s running-work strip as drawn now; none while
    /// nothing runs or no pane shows its chat.
    #[must_use]
    pub fn chat_strip(&self, session_id: &str) -> Option<Strip> {
        self.chat
            .sessions
            .get(session_id)?
            .work
            .strip(self.wall_now())
    }

    /// How many times the running-work strips' timer has repainted them.
    #[must_use]
    pub fn chat_work_repaints(&self) -> usize {
        self.chat.work_strip_repaints
    }

    /// The wall time the running-work strips read: the time the view
    /// started, moved on as far as its clock has since.
    #[must_use]
    pub fn wall_now(&self) -> DateTime<Utc> {
        self.chat.wall.at((self.now)())
    }

    /// Starts the strips' repaint timer while some chat pane shows a strip,
    /// and drops it when none does. It fires when the soonest strip next
    /// changes how it reads (see [`RunningWork::next_repaint`]).
    pub(crate) fn arm_work_strip_repaint(&mut self, cx: &mut Context<Self>) {
        let now = self.wall_now();
        let Some(delay) = self
            .chat
            .sessions
            .values()
            .filter_map(|chat| chat.work.next_repaint(now))
            .min()
        else {
            self.chat.work_strip_timer = None;
            return;
        };
        self.chat.work_strip_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            // Fails only when the view is gone, and the strips with it.
            this.update(cx, Self::repaint_work_strips).ok();
        }));
    }

    /// The timer fired: repaint the strips and go again while one shows.
    fn repaint_work_strips(&mut self, cx: &mut Context<Self>) {
        if !self.chat.shows_work() {
            self.chat.work_strip_timer = None;
            return;
        }
        self.chat.work_strip_repaints += 1;
        cx.notify();
        self.arm_work_strip_repaint(cx);
    }

    /// Switches pane `pane_id` between its terminal and its chat, as
    /// Ctrl+Shift+M does (a summary pane goes to its terminal).
    pub fn toggle_pane_view(&mut self, pane_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let next = self.chat.views.get(pane_id).after_key(ViewKey::Chat);
        self.set_pane_view(pane_id, next, window, cx);
    }

    /// Pane `pane_id` shows `view` from now on. The chat and the summary
    /// take no keys, so a terminal that had the keyboard of the window
    /// drawing it, the main one or a pop-out, lets go of it; switching back
    /// gives the terminal the keyboard again.
    /// The view chosen becomes its session's remembered one, saved at once.
    /// A new view starts at its bottom.
    pub fn set_pane_view(
        &mut self,
        pane_id: &str,
        view: PaneView,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.chat.notes.remove(pane_id);
        if self.chat.views.get(pane_id) != view {
            self.chat.scrolls.remove(pane_id);
        }
        self.chat.views.set(pane_id, view);
        self.remember_pane_view(pane_id, view);
        self.sync_chat_loads();
        self.arm_work_strip_repaint(cx);
        self.update_pane_roles(cx);
        if view.hides_terminal() {
            self.release_pane_keyboard(pane_id, window, cx);
        } else {
            self.focus_pane_where_drawn(pane_id, window, cx);
        }
        cx.notify();
    }

    /// Ctrl+Shift+M or Ctrl+Shift+U (`key`) on pane `pane_id`, when its
    /// session offers a chat.
    fn press_view_key(
        &mut self,
        pane_id: &str,
        key: ViewKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.pane_offers_chat(pane_id) {
            let next = self.chat.views.get(pane_id).after_key(key);
            self.set_pane_view(pane_id, next, window, cx);
        }
    }

    /// Ctrl+Shift+M or Ctrl+Shift+U in the main window: switches the
    /// active tab's focused pane, when its session offers a chat.
    pub(crate) fn focused_pane_view_key(
        &mut self,
        key: ViewKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(pane_id) = self.focused_pane() {
            self.press_view_key(&pane_id, key, window, cx);
        }
    }

    /// Ctrl+Shift+M or Ctrl+Shift+U in pop-out window `here`: switches the
    /// pane it draws (a popped tab's focused pane) when its session offers a
    /// chat.
    pub(crate) fn window_pane_view_key(
        &mut self,
        here: WindowId,
        key: ViewKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(pane_id) = self.window_pane(here) {
            self.press_view_key(&pane_id, key, window, cx);
        }
    }

    /// The key `session_views` files the connected host's sessions under:
    /// [`LOCAL_HOST_KEY`] for this PC, else the host's session-count key.
    pub(crate) fn view_host(&self) -> String {
        self.conn
            .target()
            .session_count_key()
            .unwrap_or_else(|| LOCAL_HOST_KEY.to_owned())
    }

    /// Whether the session pane `pane_id` shows offers its chat, which is
    /// when the pane offers the Terminal / Chat switch.
    pub(crate) fn pane_offers_chat(&self, pane_id: &str) -> bool {
        self.panes
            .get(pane_id)
            .and_then(|slot| slot.session())
            .and_then(|id| self.sidebar.session(id))
            .is_some_and(offers_chat)
    }

    /// Keeps `view` as the last view of the session pane `pane_id` shows,
    /// and saves the layout when that changed it.
    fn remember_pane_view(&mut self, pane_id: &str, view: PaneView) {
        let host = self.view_host();
        let Some(session_id) = self.panes.get(pane_id).and_then(|slot| slot.session()) else {
            return;
        };
        if self.sidebar.remember_session_view(&host, session_id, view) {
            self.save_ui();
        }
    }

    /// Pane `pane_id` newly shows session `session_id`, or nothing: it opens
    /// in the view that session was last switched to, with no scroll or
    /// note of what it showed before.
    pub(crate) fn open_pane_view(&mut self, pane_id: &str, session_id: Option<&str>) {
        let host = self.view_host();
        let view = session_id.map_or(PaneView::Terminal, |id| {
            self.sidebar.session_view(&host, id)
        });
        self.chat.forget_pane(pane_id);
        self.chat.views.set(pane_id, view);
    }

    /// Takes the keyboard from pane `pane_id`'s terminal in the window that
    /// draws it, once this update is over when that is another window.
    fn release_pane_keyboard(&self, pane_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let drawn_in = self
            .slot_window(pane_id)
            .unwrap_or_else(|| self.layouts.main());
        if drawn_in == window.window_handle().window_id() {
            self.blur_pane(pane_id, window, cx);
            return;
        }
        let pane_id = pane_id.to_owned();
        Self::in_window(drawn_in, cx, move |root, window, cx| {
            root.blur_pane(&pane_id, window, cx);
        });
    }

    /// Takes `window`'s keyboard from pane `pane_id`'s terminal if it has
    /// it: the main window keeps the keyboard to itself, a pop-out focuses
    /// nothing.
    fn blur_pane(&self, pane_id: &str, window: &mut Window, cx: &App) {
        let focused = self
            .panes
            .get(pane_id)
            .is_some_and(|slot| slot.view().read(cx).is_focused(window));
        if !focused {
            return;
        }
        if self.is_main_window(window) {
            self.root_focus.focus(window);
        } else {
            window.blur();
        }
    }

    /// Brings the loaded chats and summaries in line with the panes:
    /// `LoadChat` for a session the first chat or summary pane (in any
    /// window) now shows, `LoadSummary` for one the first summary pane now
    /// shows, and `UnloadChat` or `UnloadSummary` once the last such pane
    /// stopped showing it. A pane that is gone forgets its view.
    pub(crate) fn sync_chat_loads(&mut self) {
        let panes = &self.panes;
        self.chat
            .views
            .retain(|pane_id| panes.contains_key(pane_id));
        let shown: Vec<(String, String, PaneView)> = self
            .panes
            .iter()
            .filter_map(|(pane_id, slot)| {
                let session_id = slot.session()?;
                let view = self.view_shown(pane_id, Some(session_id));
                view.hides_terminal()
                    .then(|| (pane_id.clone(), session_id.to_owned(), view))
            })
            .collect();
        let shown_panes: BTreeSet<&str> = shown
            .iter()
            .map(|(pane_id, _, _)| pane_id.as_str())
            .collect();
        self.chat
            .scrolls
            .retain(|pane_id, _| shown_panes.contains(pane_id.as_str()));
        for pane_id in shown_panes {
            self.chat
                .scrolls
                .entry(pane_id.to_owned())
                .or_insert_with(|| {
                    let scroll = Scrollbar::default();
                    scroll.handle().scroll_to_bottom();
                    scroll
                });
        }
        let wanted = wanted_loads(
            shown
                .iter()
                .map(|(_, session_id, view)| (session_id.as_str(), *view)),
        );
        self.sync_summary_loads(wanted.summary);
        let changes = self.chat.loads.diff(wanted.chat);
        for session_id in changes.unload {
            self.chat.sessions.remove(&session_id);
            self.send(ClientMessage::UnloadChat { session_id });
        }
        for session_id in changes.load {
            let request_id = new_request_id();
            let chat = SessionChat {
                request: Some(request_id.clone()),
                ..SessionChat::default()
            };
            self.chat.sessions.insert(session_id.clone(), chat);
            self.send(ClientMessage::LoadChat {
                session_id,
                request_id: Some(request_id),
            });
        }
    }

    /// The summaries some summary pane shows are now `wanted`: unloads the
    /// ones no pane shows any more and loads the new ones.
    fn sync_summary_loads(&mut self, wanted: BTreeSet<String>) {
        let changes = self.chat.summary_loads.diff(wanted);
        for session_id in changes.unload {
            self.chat.summaries.remove(&session_id);
            self.send(ClientMessage::UnloadSummary { session_id });
        }
        for session_id in changes.load {
            let request_id = new_request_id();
            self.chat
                .summaries
                .insert(session_id.clone(), SummaryLog::asked(request_id.clone()));
            self.send(ClientMessage::LoadSummary {
                session_id,
                request_id: Some(request_id),
            });
        }
    }

    /// A message no earlier fold took: a chat message updates its chat, a
    /// summary message its summary, anything else goes to the agents' fold.
    pub(crate) fn fold_chat_or_agents(&mut self, msg: DaemonMessage, cx: &mut Context<Self>) {
        match msg {
            DaemonMessage::Chat { .. }
            | DaemonMessage::ChatAppend { .. }
            | DaemonMessage::ChatScreenChanged { .. }
            | DaemonMessage::ChatWork { .. } => self.on_chat_message(msg, cx),
            DaemonMessage::Summary {
                session_id,
                lines,
                request_id,
            } => {
                let applied = self
                    .chat
                    .summaries
                    .get_mut(&session_id)
                    .is_some_and(|summary| summary.apply_summary(lines, request_id.as_deref()));
                self.summary_folded(&session_id, applied, cx);
            }
            DaemonMessage::SummaryAppend { session_id, lines } => {
                let summary = self.chat.summaries.get_mut(&session_id);
                let applied = summary.is_some();
                if let Some(summary) = summary {
                    summary.append(lines);
                }
                self.summary_folded(&session_id, applied, cx);
            }
            other => self.fold_agents(other, cx),
        }
    }

    /// A `Summary` or `SummaryAppend` for session `session_id` was folded
    /// (`applied`), or dropped: one for a summary no pane shows, or a reply
    /// to an older load.
    fn summary_folded(&self, session_id: &str, applied: bool, cx: &mut Context<Self>) {
        if applied {
            self.follow_summary_panes(session_id);
            cx.notify();
        } else {
            tracing::debug!(%session_id, "summary message for a summary no pane shows, or for an older load: dropped");
        }
    }

    /// A `Chat`, `ChatAppend`, `ChatScreenChanged` or `ChatWork`: it
    /// updates the chat of a session a chat pane shows; one for any other
    /// session, or a reply to an older load, is dropped.
    pub(crate) fn on_chat_message(&mut self, msg: DaemonMessage, cx: &mut Context<Self>) {
        let now = (self.now)();
        let session_id = match &msg {
            DaemonMessage::Chat { session_id, .. }
            | DaemonMessage::ChatAppend { session_id, .. }
            | DaemonMessage::ChatScreenChanged { session_id, .. }
            | DaemonMessage::ChatWork { session_id, .. } => session_id.clone(),
            _ => return,
        };
        let work = matches!(msg, DaemonMessage::ChatWork { .. });
        let applied = self
            .chat
            .sessions
            .get_mut(&session_id)
            .is_some_and(|chat| chat.apply(msg, now));
        if applied {
            if work {
                self.arm_work_strip_repaint(cx);
            }
            self.follow_summary_panes(&session_id);
            self.ensure_composer(&session_id, cx);
            cx.notify();
        } else {
            tracing::debug!(%session_id, "chat message for a chat no pane shows, or for an older load: dropped");
        }
    }

    /// A daemon `Error` that answers one of this connection's pending
    /// `LoadChat`s or `LoadSummary`s: every chat and summary pane of that
    /// session snaps back to its terminal with a note, nothing is unloaded
    /// and a later toggle asks again. The session's other load, still
    /// pending, is forgotten with it, and its own refusal is taken silently. An `unsupported_message` refusal says the session offers no
    /// chat; any other carries the daemon's `message`. Returns whether the
    /// error answered a `LoadChat`.
    pub(crate) fn on_chat_refused(
        &mut self,
        message: &str,
        request_id: Option<&str>,
        code: Option<&str>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(request_id) = request_id else {
            return false;
        };
        if self.chat.refused_loads.remove(request_id) {
            tracing::debug!(
                request_id,
                "refusal of a load forgotten with its session's other load: taken"
            );
            return true;
        }
        if self.on_chat_reply_refused(message, request_id, cx) {
            return true;
        }
        let chat_refused = self
            .chat
            .sessions
            .iter()
            .find(|(_, chat)| chat.request.as_deref() == Some(request_id))
            .map(|(session_id, _)| session_id.clone());
        let summary_refused = self
            .chat
            .summaries
            .iter()
            .find(|(_, summary)| summary.request() == Some(request_id))
            .map(|(session_id, _)| session_id.clone());
        let Some(session_id) = chat_refused.clone().or(summary_refused) else {
            return false;
        };
        let note = if code == Some(UNSUPPORTED_MESSAGE) {
            tracing::info!(%session_id, "the daemon offers no chat for this session; its chat panes show the terminal");
            let host = self.view_host();
            if self
                .sidebar
                .remember_session_view(&host, &session_id, PaneView::Terminal)
            {
                self.save_ui();
            }
            UNSUPPORTED_NOTE.to_owned()
        } else {
            tracing::warn!(%session_id, ?code, message, "the daemon could not load this session's chat; its chat panes show the terminal");
            format!("{LOAD_FAILED_NOTE}{message}")
        };
        self.chat
            .forget_refused_load(&session_id, chat_refused.is_some());
        let panes: Vec<String> = self
            .panes
            .iter()
            .filter(|(pane_id, slot)| {
                slot.session() == Some(session_id.as_str())
                    && self.chat.views.get(pane_id).hides_terminal()
            })
            .map(|(pane_id, _)| pane_id.clone())
            .collect();
        for pane_id in panes {
            self.chat.views.set(&pane_id, PaneView::Terminal);
            self.chat.notes.insert(pane_id, note.clone());
        }
        self.sync_chat_loads();
        self.arm_work_strip_repaint(cx);
        self.update_pane_roles(cx);
        cx.notify();
        true
    }

    /// A daemon `Error` that answers one of our chat sends or card answers,
    /// shown inline with no toast. A refused send, or a refused answer that
    /// took composer text (even after its card left), gives the text back
    /// before what was typed since, with the daemon's reason under the
    /// composer; a refused answer to a card still up frees it with the
    /// reason on it. Returns whether the error was ours.
    fn on_chat_reply_refused(
        &mut self,
        message: &str,
        request_id: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        for (session_id, chat) in &mut self.chat.sessions {
            let text = chat.sends.refused(request_id);
            let card = chat.cards.refused(request_id, message);
            if text.is_none() && card.is_none() {
                continue;
            }
            tracing::info!(%session_id, message, ?card, "the daemon refused a chat send or card answer");
            if let Some(text) = text {
                chat.give_back(&text, cx);
                chat.send_error = Some(message.to_owned());
            }
            cx.notify();
            return true;
        }
        false
    }

    /// Makes session `session_id`'s composer once its chat has loaded:
    /// Ctrl+Enter and Esc in it go through [`Self::chat_key`], and every edit
    /// repaints the buttons beside it.
    fn ensure_composer(&mut self, session_id: &str, cx: &mut Context<Self>) {
        let wanted = self
            .chat
            .sessions
            .get(session_id)
            .is_some_and(|chat| chat.request.is_none() && chat.composer.is_none());
        if !wanted {
            return;
        }
        let input = cx.new(|cx| {
            TextInput::multi_line("", COMPOSER_PLACEHOLDER, cx)
                .with_rows(COMPOSER_MIN_ROWS, COMPOSER_MAX_ROWS)
        });
        let id = session_id.to_owned();
        let keys = cx.subscribe(&input, move |this, _, event: &TextInputEvent, cx| {
            let key = match event {
                TextInputEvent::Submit => CardKey::CtrlEnter,
                TextInputEvent::Cancel => CardKey::Escape,
            };
            this.chat_key(&id, key, cx);
        });
        let edits = cx.subscribe(&input, |_, _, _: &TextChanged, cx| cx.notify());
        if let Some(chat) = self.chat.sessions.get_mut(session_id) {
            chat.composer = Some(Composer {
                input,
                _subscriptions: [keys, edits],
            });
        }
    }

    /// A key the chat of session `session_id` reads before its composer:
    /// it answers the current card, sends the composer's text, or is
    /// swallowed; Ctrl+Enter does nothing while a send is in flight.
    /// Returns whether the key was taken.
    pub(crate) fn chat_key(
        &mut self,
        session_id: &str,
        key: CardKey,
        cx: &mut Context<Self>,
    ) -> bool {
        let now = (self.now)();
        let Some(chat) = self.chat.sessions.get(session_id) else {
            return false;
        };
        if key == CardKey::CtrlEnter && chat.sends.in_flight(now) {
            return true;
        }
        let text = chat.composer_text(cx);
        let current = chat.stack.current().cloned();
        match key_action(&chat.stack, chat.card_busy(), key, &text, now) {
            KeyAction::Answer(answer) => {
                if let Some(prompt) = current {
                    self.answer_card(session_id, &prompt, answer, None, cx);
                }
            }
            KeyAction::Send => self.send_composer(session_id, cx),
            KeyAction::Swallow => {}
            KeyAction::Pass => return false,
        }
        true
    }

    /// Sends `answer` to `prompt`, the card it was drawn or keyed for, under
    /// a new request id, unless that card is busy; a card gone since is
    /// answered all the same and the daemon refuses it. A toggle leaves the
    /// card free; any other answer makes it busy. `taken`, the composer
    /// text the answer carries, is cleared from the composer and kept by
    /// its request id to give back on a refusal.
    fn answer_card(
        &mut self,
        session_id: &str,
        prompt: &ChatPrompt,
        answer: ChatAnswerKind,
        taken: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(chat) = self.chat.sessions.get_mut(session_id) else {
            return;
        };
        if chat.cards.is_busy(&prompt.id) {
            return;
        }
        let request_id = new_request_id();
        let current = chat.stack.current().map(|current| current.id.clone());
        chat.cards
            .clicked(&prompt.id, current.as_deref(), request_id.clone(), &answer);
        if let Some(text) = taken {
            chat.sends.took(request_id.clone(), text);
            chat.set_composer_text("", cx);
        }
        self.send(card_answer_message(
            session_id,
            &prompt.id,
            answer,
            Some(request_id),
        ));
        cx.notify();
    }

    /// A button of session `session_id`'s card for `prompt` was pressed.
    fn press_card(
        &mut self,
        session_id: &str,
        prompt: &ChatPrompt,
        answer: &ButtonAnswer,
        cx: &mut Context<Self>,
    ) {
        let Some(chat) = self.chat.sessions.get(session_id) else {
            return;
        };
        let text = chat.composer_text(cx);
        let Some(resolved) = answer.resolve(&text) else {
            return;
        };
        let taken = matches!(answer, ButtonAnswer::TypedText).then_some(text);
        self.answer_card(session_id, prompt, resolved, taken, cx);
    }

    /// A ride-on button of session `session_id` drawn for `prompt`: the
    /// composer's text rides on allowing (`allow`) or denying it. Nothing
    /// happens while a send is in flight.
    fn ride_on(
        &mut self,
        session_id: &str,
        prompt: &ChatPrompt,
        allow: bool,
        cx: &mut Context<Self>,
    ) {
        let now = (self.now)();
        let Some(chat) = self.chat.sessions.get(session_id) else {
            return;
        };
        if chat.sends.in_flight(now) {
            return;
        }
        let text = chat.composer_text(cx);
        let answer = if allow {
            chat::allow_with_text(prompt, &text)
        } else {
            chat::deny_with_text(prompt, &text)
        };
        if let Some(answer) = answer {
            self.answer_card(session_id, prompt, answer, Some(text), cx);
        }
    }

    /// Sends session `session_id`'s composer text, clearing the composer;
    /// a refusal puts it back. Nothing happens while a send is in flight.
    fn send_composer(&mut self, session_id: &str, cx: &mut Context<Self>) {
        let now = (self.now)();
        let request_id = new_request_id();
        let Some(chat) = self.chat.sessions.get_mut(session_id) else {
            return;
        };
        if chat.sends.in_flight(now) {
            return;
        }
        let text = chat.composer_text(cx);
        let Some(msg) = chat::send_message(session_id, &text, request_id.clone()) else {
            return;
        };
        chat.sends.sent(request_id, text, now);
        chat.send_error = None;
        chat.set_composer_text("", cx);
        self.send(msg);
        cx.notify();
    }

    /// The note `chat`'s composer shows: why it cannot send.
    fn composer_note_of(&self, chat: &SessionChat) -> Option<ComposerNote> {
        chat::composer_note(
            self.conn.is_open(),
            chat.composer_state.as_ref(),
            chat.sends.in_flight((self.now)()),
        )
    }

    /// The name people read for session `session_id`'s agent, for the
    /// labels that name it.
    fn agent_label(&self, session_id: &str) -> String {
        let agent = self
            .sidebar
            .session(session_id)
            .and_then(|session| session.agent.as_ref());
        self.sidebar
            .agents()
            .iter()
            .find(|descriptor| Some(&descriptor.id) == agent)
            .map_or_else(
                || GENERIC_AGENT.to_owned(),
                |descriptor| descriptor.label.clone(),
            )
    }

    /// The text of session `session_id`'s composer; none before its chat
    /// has loaded.
    #[must_use]
    pub fn chat_composer_text(&self, session_id: &str, cx: &App) -> Option<String> {
        let chat = self.chat.sessions.get(session_id)?;
        chat.composer
            .as_ref()
            .map(|composer| composer.input.read(cx).text().to_owned())
    }

    /// The note session `session_id`'s composer shows, if any.
    #[must_use]
    pub fn chat_composer_note(&self, session_id: &str) -> Option<String> {
        let chat = self.chat.sessions.get(session_id)?;
        self.composer_note_of(chat).map(|note| note.text())
    }

    /// The daemon's reason for refusing session `session_id`'s last send.
    #[must_use]
    pub fn chat_composer_error(&self, session_id: &str) -> Option<String> {
        self.chat.sessions.get(session_id)?.send_error.clone()
    }

    /// Session `session_id`'s card: its prompt id and whether it is busy.
    #[must_use]
    pub fn chat_card(&self, session_id: &str) -> Option<(String, bool)> {
        let chat = self.chat.sessions.get(session_id)?;
        let prompt = chat.stack.current()?;
        Some((prompt.id.clone(), chat.cards.is_busy(&prompt.id)))
    }

    /// The daemon's reason for refusing the last answer to session
    /// `session_id`'s card.
    #[must_use]
    pub fn chat_card_refusal(&self, session_id: &str) -> Option<String> {
        let chat = self.chat.sessions.get(session_id)?;
        let prompt = chat.stack.current()?;
        chat.cards.refusal(&prompt.id).map(ToOwned::to_owned)
    }

    /// The selectors of the buttons beside session `session_id`'s
    /// composer: Send, or the ride-on answers.
    #[must_use]
    pub fn chat_composer_actions(&self, session_id: &str, cx: &App) -> Vec<&'static str> {
        let Some(chat) = self.chat.sessions.get(session_id) else {
            return Vec::new();
        };
        if chat.composer.is_none() {
            return Vec::new();
        }
        composer_actions(chat.stack.current(), &chat.composer_text(cx))
            .into_iter()
            .map(ComposerAction::selector)
            .collect()
    }

    /// Whether session `session_id`'s chat shows "Answered elsewhere".
    #[must_use]
    pub fn chat_answered_elsewhere(&self, session_id: &str) -> bool {
        self.chat
            .sessions
            .get(session_id)
            .is_some_and(|chat| chat.cards.answered_elsewhere())
    }

    /// A click on a tool card of session `session_id`'s chat opens or
    /// closes it.
    fn toggle_chat_card(&mut self, session_id: &str, id: &str, cx: &mut Context<Self>) {
        if let Some(chat) = self.chat.sessions.get_mut(session_id) {
            chat.log.toggle_expanded(id);
            cx.notify();
        }
    }

    /// A click on a stretch's short form or closer in answer `answer_id` of
    /// session `session_id`'s chat opens it, or closes it and the stretches
    /// nested in it; every pane on the session draws it so.
    pub fn toggle_stretchtext(
        &mut self,
        session_id: &str,
        answer_id: &str,
        key: &str,
        cx: &mut Context<Self>,
    ) {
        if let Some(chat) = self.chat.sessions.get_mut(session_id) {
            chat.log.toggle_stretchtext(answer_id, key);
            chat.refresh_docs();
            cx.notify();
        }
    }

    /// The text answer `answer_id` of session `session_id`'s chat draws, a
    /// line per drawn row; none while its chat is not loaded.
    #[must_use]
    pub fn chat_answer_text(&self, session_id: &str, answer_id: &str) -> Option<String> {
        self.answer_doc(session_id, answer_id)
            .map(MdDoc::drawn_text)
    }

    /// The stretchtext links answer `answer_id` of session `session_id`'s
    /// chat draws, in order: each one's drawn text (a short form, or `‹` for
    /// a closer) and its stretch's key.
    #[must_use]
    pub fn chat_answer_stretchtext(
        &self,
        session_id: &str,
        answer_id: &str,
    ) -> Vec<(String, String)> {
        self.answer_doc(session_id, answer_id)
            .map(MdDoc::stretchtext_links)
            .unwrap_or_default()
    }

    fn answer_doc(&self, session_id: &str, answer_id: &str) -> Option<&MdDoc> {
        let chat = self.chat.sessions.get(session_id)?;
        chat.docs.get(answer_id).map(|answer| answer.doc.as_ref())
    }

    /// What pane `pane_id`, showing `session_id` with its terminal
    /// attached, draws: its chat when switched to it, else `terminal`, under
    /// the note a refused chat left.
    pub(crate) fn attached_body(
        &self,
        pane_id: &str,
        session_id: &str,
        terminal: Div,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match self.view_shown(pane_id, Some(session_id)) {
            PaneView::Chat => return self.chat_body(pane_id, session_id, cx),
            PaneView::Summary => return self.summary_body(pane_id, session_id, cx),
            PaneView::Terminal => {}
        }
        let Some(note) = self.chat.notes.get(pane_id).cloned() else {
            return terminal.into_any_element();
        };
        let name = format!("pane-chat-note-{pane_id}");
        let muted = theme(cx).muted;
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .size_full()
            .child(
                div()
                    .debug_selector(|| name)
                    .flex_none()
                    .px(px(BODY_PAD_X))
                    .py(px(4.0))
                    .text_size(fonts::ui_px(cx, SMALL_TEXT_SIZE))
                    .text_color(gpui::rgb(muted))
                    .child(note),
            )
            .child(terminal)
            .into_any_element()
    }

    /// Session `session_id`'s chat as pane `pane_id` draws it: a muted line
    /// while it loads or has no rows, else its rows in a column that follows
    /// the bottom until scrolled up; under either, the running-work strip
    /// while work runs.
    fn chat_body(&self, pane_id: &str, session_id: &str, cx: &mut Context<Self>) -> AnyElement {
        let name = format!("pane-chat-{pane_id}");
        let chat = self.chat.sessions.get(session_id);
        let look = self.row_look(pane_id, session_id, cx);
        let strip = chat.and_then(|chat| chat.work.strip(self.wall_now()));
        let rows = chat.filter(|chat| !chat.log.is_empty() || chat.log.hidden() > 0);
        let main = if let Some(chat) = rows {
            chat_column(&look, chat, self.chat.scrolls.get(pane_id), cx)
        } else {
            let loading = chat.is_none_or(|chat| chat.request.is_some());
            let note = if loading { LOADING_NOTE } else { EMPTY_NOTE };
            chat_note(look.text_offset, note, &look.theme).into_any_element()
        };
        let loaded = chat.filter(|chat| chat.request.is_none());
        let cards = loaded.and_then(|chat| self.card_area(session_id, chat, cx));
        let composer = loaded.and_then(|chat| self.composer_row(session_id, chat, cx));
        let keys_for = session_id.to_owned();
        div()
            .debug_selector(|| name)
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .size_full()
            .capture_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                if let Some(key) = card_key(&event.keystroke)
                    && this.chat_key(&keys_for, key, cx)
                {
                    cx.stop_propagation();
                }
            }))
            .child(main)
            .children(cards)
            .children(
                strip.map(|strip| strip_row(look.text_offset, &strip, &look.mono, &look.theme)),
            )
            .children(composer)
            .into_any_element()
    }

    /// What every row pane `pane_id` draws for session `session_id` is
    /// drawn with: the terminal's font for code, the UI text size and the
    /// theme.
    pub(crate) fn row_look<'a>(
        &'a self,
        pane_id: &'a str,
        session_id: &'a str,
        cx: &App,
    ) -> RowLook<'a> {
        let font = &self.sidebar.ui_state().terminal_font;
        let family = font.family.as_deref();
        RowLook {
            pane_id,
            session_id,
            mono: SharedString::from(family.unwrap_or(DEFAULT_FAMILY).to_owned()),
            mono_family: family,
            mono_size: font.size,
            text_offset: fonts::ui_text_size(cx),
            theme: theme(cx),
        }
    }

    /// The theme pane `pane_id` draws session `session_id`'s rows in.
    #[must_use]
    pub fn chat_row_theme(&self, pane_id: &str, session_id: &str, cx: &App) -> UiTheme {
        self.row_look(pane_id, session_id, cx).theme
    }

    /// Session `session_id`'s card and the cards behind it, under an
    /// "Answered elsewhere" line while one shows; none while neither does.
    pub(crate) fn card_area(
        &self,
        session_id: &str,
        chat: &SessionChat,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (offset, t) = (fonts::ui_text_size(cx), theme(cx));
        let elsewhere = chat.cards.answered_elsewhere().then(|| {
            muted_line(offset, ANSWERED_ELSEWHERE.to_owned(), &t)
                .debug_selector(|| "chat-answered-elsewhere".to_owned())
        });
        let agent = self.agent_label(session_id);
        let card = chat.stack.current().map(|prompt| {
            let view = CardView {
                prompt,
                agent: &agent,
                busy: chat.cards.is_busy(&prompt.id),
                refusal: chat.cards.refusal(&prompt.id),
                ghosts: chat.stack.ghosts().len(),
            };
            let id = session_id.to_owned();
            let drawn = prompt.clone();
            let on_press: OnPress =
                Rc::new(cx.listener(move |this, answer: &ButtonAnswer, _, cx| {
                    this.press_card(&id, &drawn, answer, cx);
                }));
            prompt_card(offset, &view, &on_press, &t)
        });
        if elsewhere.is_none() && card.is_none() {
            return None;
        }
        Some(
            div()
                .flex_none()
                .flex()
                .flex_col()
                .gap(px(6.0))
                .px(px(BODY_PAD_X))
                .pb(px(6.0))
                .children(elsewhere)
                .children(card)
                .into_any_element(),
        )
    }

    /// Session `session_id`'s composer row: the note saying why it cannot
    /// send, the input (tagged `chat-composer`) with Send (`chat-send`) or,
    /// with text while a card is up, the ride-on answers (`chat-ride-yes`,
    /// `chat-ride-no`), and the daemon's reason for refusing the last send
    /// (`chat-composer-error`). Without a connection the input is
    /// disabled.
    pub(crate) fn composer_row(
        &self,
        session_id: &str,
        chat: &SessionChat,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let composer = chat.composer.as_ref()?;
        let (offset, t) = (fonts::ui_text_size(cx), theme(cx));
        let note = self.composer_note_of(chat);
        let disabled = note.as_ref().is_some_and(ComposerNote::disables);
        let text = composer.input.read(cx).text().to_owned();
        let blank = text.trim().is_empty();
        let field = field_frame(false, &t)
            .debug_selector(|| "chat-composer".to_owned())
            .flex_1()
            .min_w(px(0.0))
            .py(px(6.0))
            .when(disabled, |field| field.opacity(DISABLED_OPACITY))
            .when(!disabled, |field| field.child(composer.input.clone()));
        let prompt = chat.stack.current();
        let busy = prompt.is_some_and(|prompt| chat.cards.is_busy(&prompt.id));
        let sending = chat.sends.in_flight((self.now)());
        let agent = self.agent_label(session_id);
        let actions: Vec<AnyElement> = composer_actions(prompt, &text)
            .into_iter()
            .map(|action| match action {
                ComposerAction::Send => {
                    let enabled = !blank && !disabled && !sending;
                    let id = session_id.to_owned();
                    primary_button(
                        offset,
                        action.selector(),
                        ButtonSize::Compact,
                        enabled,
                        Layer::Base,
                        &t,
                    )
                    .child("Send")
                    .when(enabled, |send| {
                        send.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.send_composer(&id, cx);
                        }))
                    })
                    .into_any_element()
                }
                ComposerAction::RideYes | ComposerAction::RideNo => {
                    let allow = action == ComposerAction::RideYes;
                    let label = if allow {
                        ride_yes_label(&agent)
                    } else {
                        ride_no_label(&agent)
                    };
                    let ride = Ride {
                        selector: action.selector(),
                        label,
                        allow,
                        busy: busy || sending,
                        prompt: prompt.cloned(),
                    };
                    Self::ride_button(session_id, ride, cx)
                }
            })
            .collect();
        Some(
            div()
                .flex_none()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .px(px(BODY_PAD_X))
                .pt(px(6.0))
                .pb(px(BODY_PAD_Y))
                .text_size(ui_px_at(offset, UI_TEXT_SIZE))
                .children(note.map(|note| {
                    muted_line(offset, note.text(), &t)
                        .debug_selector(|| "chat-composer-note".to_owned())
                }))
                .child(
                    div()
                        .flex()
                        .items_end()
                        .gap(px(8.0))
                        .child(field)
                        .children(actions),
                )
                .children(chat.send_error.clone().map(|reason| {
                    div()
                        .debug_selector(|| "chat-composer-error".to_owned())
                        .text_size(ui_px_at(offset, SMALL_TEXT_SIZE))
                        .text_color(gpui::rgb(t.danger))
                        .child(reason)
                }))
                .into_any_element(),
        )
    }

    /// One ride-on button: the composer's text rides on allowing (`allow`)
    /// or denying session `session_id`'s card; a busy card's takes no click.
    fn ride_button(session_id: &str, ride: Ride, cx: &mut Context<Self>) -> AnyElement {
        let id = session_id.to_owned();
        let Ride {
            selector,
            label,
            allow,
            busy,
            prompt,
        } = ride;
        outlined_button(
            fonts::ui_text_size(cx),
            selector,
            ButtonSize::Compact,
            !busy,
            Layer::Base,
            &theme(cx),
        )
        .child(label)
        .when(!busy, |button| {
            button.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                if let Some(prompt) = &prompt {
                    this.ride_on(&id, prompt, allow, cx);
                }
            }))
        })
        .into_any_element()
    }
}

/// One ride-on button: its selector, its label, whether it allows or
/// denies, and whether the card is busy.
struct Ride {
    selector: &'static str,
    label: String,
    allow: bool,
    busy: bool,
    prompt: Option<ChatPrompt>,
}

/// How a disabled composer is dimmed.
const DISABLED_OPACITY: f32 = 0.5;

/// The card key `keystroke` is, when the chat reads it before its
/// composer: a bare digit, or Ctrl+Shift+Enter. Ctrl+Enter and Esc reach
/// the chat through the composer's own Submit and Cancel.
pub(crate) fn card_key(keystroke: &Keystroke) -> Option<CardKey> {
    let mods = &keystroke.modifiers;
    if keystroke.key == "enter" && mods.control && mods.shift && !mods.alt && !mods.platform {
        return Some(CardKey::CtrlShiftEnter);
    }
    if mods.control || mods.alt || mods.shift || mods.platform || mods.function {
        return None;
    }
    let mut chars = keystroke.key.chars();
    let digit = chars.next()?.to_digit(10)?;
    chars.next().is_none().then_some(CardKey::Digit(digit))
}

/// A loaded chat's rows in a column that follows the bottom until scrolled
/// up, with its scrollbar.
fn chat_column(
    look: &RowLook<'_>,
    chat: &SessionChat,
    scroll: Option<&Scrollbar>,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let mut rows: Vec<AnyElement> = Vec::with_capacity(chat.log.len() + 1);
    if chat.log.hidden() > 0 {
        rows.push(hidden_row(look, chat.log.hidden()));
    }
    rows.extend(
        chat.log
            .entries()
            .map(|entry| chat_row(look, chat, entry, CHAT_ROW, cx)),
    );
    let column = div()
        .id(ElementId::Name(SharedString::from(format!(
            "chat-scroll-{}",
            look.pane_id
        ))))
        .size_full()
        .overflow_y_scroll()
        .px(px(BODY_PAD_X))
        .py(px(BODY_PAD_Y))
        .flex()
        .flex_col()
        .gap(px(ROW_GAP))
        .text_size(ui_px_at(look.text_offset, UI_TEXT_SIZE))
        .text_color(gpui::rgb(look.theme.text))
        .children(rows);
    let column = match scroll {
        Some(scroll) => {
            follow_bottom(scroll.handle());
            column.track_scroll(scroll.handle())
        }
        None => column,
    };
    div()
        .relative()
        .flex_1()
        .min_h(px(0.0))
        .w_full()
        .child(column)
        .children(scroll.map(Scrollbar::overlay))
        .into_any_element()
}

/// Keeps a chat at its bottom while it was there at its last layout, so new
/// rows scroll into view; a chat scrolled up stays where it is.
pub(crate) fn follow_bottom(handle: &ScrollHandle) {
    let viewport = handle.bounds().size.height / px(1.0);
    let laid_out = (viewport > 0.0).then(|| {
        (
            -handle.offset().y / px(1.0),
            handle.max_offset().height / px(1.0),
        )
    });
    if chat::follows_bottom(laid_out) {
        handle.scroll_to_bottom();
    }
}

/// The label and tooltip of a pane header's segment for `view`.
pub(crate) fn view_segment(view: PaneView) -> (&'static str, &'static str) {
    match view {
        PaneView::Terminal => ("Terminal", TOGGLE_TO_TERMINAL_TIP),
        PaneView::Chat => ("Chat", TOGGLE_TO_CHAT_TIP),
        PaneView::Summary => ("Summary", TOGGLE_TO_SUMMARY_TIP),
    }
}

/// The selector prefix of a chat pane's rows.
const CHAT_ROW: &str = "chat-row";

/// One muted line centred in the chat pane.
pub(crate) fn chat_note(offset: i8, text: &'static str, t: &UiTheme) -> Div {
    div()
        .flex()
        .flex_1()
        .size_full()
        .items_center()
        .justify_center()
        .text_size(ui_px_at(offset, UI_TEXT_SIZE))
        .text_color(gpui::rgb(t.muted))
        .child(text)
}

/// One row of the chat, tagged `<prefix>-<pane>-<id>`: `chat-row` in a
/// chat pane, `summary-row` in a summary pane.
pub(crate) fn chat_row(
    look: &RowLook<'_>,
    chat: &SessionChat,
    entry: &ChatEntry,
    prefix: &str,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let Some(id) = chat::entry_id(entry) else {
        return div().into_any_element();
    };
    let muted = |text: String| muted_line(look.text_offset, text, &look.theme);
    let row = match entry {
        ChatEntry::User { text, pending, .. } => user_row(look, text, *pending),
        ChatEntry::Answer { text, live, .. } => {
            answer_row(look, id, text, *live, chat.docs.get(id))
        }
        ChatEntry::Thinking { secs, .. } => muted(thinking_label(*secs)),
        ChatEntry::Tool {
            name,
            summary,
            input,
            result,
            ..
        } => {
            let tool = ToolRow {
                id,
                name,
                summary,
                input,
                result: result.as_ref(),
            };
            tool_card(look, &tool, chat.log.is_expanded(id), cx)
        }
        ChatEntry::System { text, .. } => system_pill(look, text),
        ChatEntry::Receipt {
            question, answer, ..
        } => muted(format!("{question} \u{2014} {answer}")),
        ChatEntry::TurnEnd { duration_ms, .. } => muted(turn_label(*duration_ms)),
        ChatEntry::Unknown => div(),
    };
    let name = format!("{prefix}-{}-{id}", look.pane_id);
    div()
        .debug_selector(|| name)
        .flex()
        .flex_col()
        .child(row)
        .into_any_element()
}

/// The muted first row of a chat whose older rows were left out.
fn hidden_row(look: &RowLook<'_>, hidden: u32) -> AnyElement {
    let noun = if hidden == 1 { "message" } else { "messages" };
    div()
        .flex()
        .justify_center()
        .child(muted_line(
            look.text_offset,
            format!("{hidden} earlier {noun}"),
            &look.theme,
        ))
        .into_any_element()
}

/// A message the user sent: a bubble on the right, dimmed until the
/// transcript has it.
fn user_row(look: &RowLook<'_>, text: &str, pending: bool) -> Div {
    div().flex().justify_end().child(
        div()
            .max_w(relative(BUBBLE_MAX_SHARE))
            .px(px(BUBBLE_PAD_X))
            .py(px(BUBBLE_PAD_Y))
            .rounded(px(BUBBLE_RADIUS))
            .bg(gpui::rgb(look.theme.chip))
            .when(pending, |bubble| bubble.opacity(PENDING_OPACITY))
            .child(text.to_owned()),
    )
}

/// An answer as Markdown; a live one carries a blue left border, a cursor
/// and a "writing" tag, tagged `chat-live-<pane>-<id>`.
fn answer_row(
    look: &RowLook<'_>,
    id: &str,
    text: &str,
    live: bool,
    doc: Option<&AnswerDoc>,
) -> Div {
    let body = match doc {
        Some(AnswerDoc { doc, .. }) => {
            let painter = Painter::new(
                look.text_offset,
                format!("chat-{id}"),
                LinkRoute::WebOnly,
                (look.mono_family, look.mono_size),
                &look.theme,
            )
            .for_answer(look.session_id, id);
            div()
                .flex()
                .flex_col()
                .children((0..doc.row_count()).map(|ix| render_row(doc, &painter, ix, 0.0, 0.0)))
        }
        None => div().child(text.to_owned()),
    };
    if !live {
        return body;
    }
    let name = format!("chat-live-{}-{id}", look.pane_id);
    let t = &look.theme;
    let cursor = div()
        .w(px(CURSOR_WIDTH))
        .h(ui_px_at(look.text_offset, CURSOR_HEIGHT))
        .bg(gpui::rgb(t.working));
    let tag = div()
        .text_size(ui_px_at(look.text_offset, TAG_TEXT_SIZE))
        .text_color(gpui::rgb(t.faint))
        .child(writing_tag(text.lines().count()));
    div()
        .debug_selector(|| name)
        .flex()
        .flex_col()
        .border_l_2()
        .border_color(gpui::rgb(t.working))
        .pl(px(LIVE_PAD))
        .child(body)
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .child(cursor)
                .child(tag),
        )
}

/// A tool call's card: its name, one-line summary and status mark (tagged
/// `chat-tool-<running|done|error>-<pane>-<id>`); a click opens it to its
/// input and result.
fn tool_card(
    look: &RowLook<'_>,
    tool: &ToolRow<'_>,
    open: bool,
    cx: &mut Context<RootView>,
) -> Div {
    let t = &look.theme;
    let (state, mark, color) = match tool.result {
        None => ("running", "running", t.working),
        Some(outcome) if outcome.is_error => ("error", "failed", t.danger),
        Some(_) => ("done", "done", t.waiting),
    };
    let mark_name = format!("chat-tool-{state}-{}-{}", look.pane_id, tool.id);
    let header = div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .child(
            div()
                .flex_none()
                .text_color(gpui::rgb(t.text))
                .child(tool.name.to_owned()),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .child(chat::tool_one_liner(tool.summary, tool.name)),
        )
        .child(
            div()
                .debug_selector(|| mark_name)
                .flex_none()
                .text_color(gpui::rgb(color))
                .child(mark),
        );
    let (session_id, id) = (look.session_id.to_owned(), tool.id.to_owned());
    let card_id = format!("chat-card-{}-{}", look.pane_id, tool.id);
    let card = div()
        .id(ElementId::Name(SharedString::from(card_id)))
        .flex()
        .flex_col()
        .gap(px(6.0))
        .px(px(CARD_PAD_X))
        .py(px(CARD_PAD_Y))
        .rounded(px(CARD_RADIUS))
        .border_1()
        .border_color(gpui::rgb(t.line_strong))
        .font_family(look.mono.clone())
        .text_size(ui_px_at(look.text_offset, CARD_TEXT_SIZE))
        .text_color(gpui::rgb(t.text_2))
        .cursor_pointer()
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.toggle_chat_card(&session_id, &id, cx);
        }))
        .child(header)
        .when(open, |card| {
            card.child(tool_detail("Input", tool.input, t))
                .children(tool.result.map(|outcome| {
                    let label = if outcome.is_error { "Error" } else { "Result" };
                    tool_detail(label, &outcome.text, t)
                }))
        });
    div().child(card)
}

/// One labelled block of an open tool card.
fn tool_detail(label: &'static str, text: &str, t: &UiTheme) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .child(div().text_color(gpui::rgb(t.faint)).child(label))
        .child(div().child(text.to_owned()))
}

/// A row the daemon wrote: a muted pill centred in the column.
fn system_pill(look: &RowLook<'_>, text: &str) -> Div {
    div().flex().justify_center().child(
        div()
            .px(px(PILL_PAD_X))
            .py(px(PILL_PAD_Y))
            .rounded_full()
            .border_1()
            .border_dashed()
            .border_color(gpui::rgb(look.theme.line_strong))
            .text_size(ui_px_at(look.text_offset, SMALL_TEXT_SIZE))
            .text_color(gpui::rgb(look.theme.subtle))
            .child(text.to_owned()),
    )
}

/// One small muted line.
pub(crate) fn muted_line(offset: i8, text: String, t: &UiTheme) -> Div {
    div()
        .text_size(ui_px_at(offset, SMALL_TEXT_SIZE))
        .text_color(gpui::rgb(t.muted))
        .child(text)
}

/// "writing · N lines" under a live answer.
fn writing_tag(lines: usize) -> String {
    let noun = if lines == 1 { "line" } else { "lines" };
    format!("writing \u{b7} {lines} {noun}")
}

/// "Thought for 14s", or "Thought" while its length is unknown.
fn thinking_label(secs: Option<u32>) -> String {
    secs.map_or_else(
        || "Thought".to_owned(),
        |secs| format!("Thought for {}", span(u64::from(secs))),
    )
}

/// "Worked for 12s" under a turn's last answer, or "Worked" without a
/// duration.
fn turn_label(duration_ms: Option<u64>) -> String {
    duration_ms.map_or_else(
        || "Worked".to_owned(),
        |ms| format!("Worked for {}", span(ms / 1000)),
    )
}

/// A length of time as "14s", "2m 5s" or "1h 3m".
fn span(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m {}s", secs / 60, secs % 60),
        _ => format!("{}h {}m", secs / 3600, secs % 3600 / 60),
    }
}
