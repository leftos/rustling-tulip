//! A session's chat drawn in a pane: its rows in a scrolling column that
//! follows the bottom until the owner scrolls up, the Terminal / Chat
//! toggle's work (from the header, the pane menu or Ctrl+Shift+M) with each
//! session's remembered view, the `LoadChat` / `UnloadChat` bookkeeping, and the folds
//! of the daemon's chat messages, and the Tail boxes under tool cards with
//! their `TailFile` / `UntailFile` bookkeeping. The rows' model, which panes
//! show a chat and which chats are loaded live in [`crate::chat`]; the Tail
//! boxes' state in [`crate::chat_tail`].

use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap};
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use chrono::{DateTime, TimeDelta, Utc};
use std::rc::Rc;

use gpui::{
    AnyElement, App, Autocapitalize, Bounds, ClickEvent, Context, Div, ElementId, Entity,
    Focusable, Hsla, KeyDownEvent, KeyboardKind, Keystroke, MouseUpEvent, Pixels, Point,
    ScrollEvent, ScrollHandle, SharedString, Size, Stateful, Subscription, Task, TextInputAction,
    TextInputConfiguration, Window, WindowId, canvas, div, point, prelude::*, px, relative,
};
use protocol::error_codes::UNSUPPORTED_MESSAGE;
use protocol::{
    AgentDescriptor, CatalogEntry, ChatAnswerKind, ChatEntry, ChatPrompt, ChatPromptKind,
    ChatScreen, ClientMessage, ComposerState, DaemonMessage, FEATURE_CHAT_CATALOG,
    FEATURE_CHAT_CLEAR_DRAFT, FEATURE_CHAT_DRAFT, FEATURE_FILE_SEARCH, FEATURE_FILE_TAIL,
    FileMatch, SessionMode, SessionSnapshot, ToolOutcome,
};

use crate::agent_options::descriptor_for;
use crate::buttons::{ButtonSize, Layer, field_frame, outlined_button, primary_button};
use crate::chat::{
    self, COMMAND_TRIGGER, ChatLoads, ChatLog, ChatMode, CommandCatalog, ComposerLine,
    ComposerNote, DisplayRow, DraftClear, DraftPull, FILE_SEARCH_DEBOUNCE, FILE_SEARCH_LIMIT,
    FILE_TRIGGER, FileSearch, PaneView, PaneViews, PromptStack, RowShape, RunItem, RunSummary,
    RunningWork, ScriptRow, SendTracker, Strip, SuggestionChip, ViewKey,
};
use crate::chat_cards::{
    ANSWERED_ELSEWHERE, ButtonAnswer, CardBoxes, CardKey, CardLinks, CardState, CardView, GiveBack,
    KeyAction, OnPress, box_placeholder, card_answer_message, key_action, prompt_card,
    restored_box_text, typed_option,
};
use crate::chat_select::{self, Selection, TextPos, UnitSource, Units};
use crate::chat_tail::{ChatTails, NO_OUTPUT, Tail, TailCard, TailShows};
use crate::completion::{self, CompletionItem, CompletionState, Token};
use crate::connection::Connection;
use crate::driver_id::DriverId as _;
use crate::fonts::{self, DEFAULT_FAMILY, ui_px_at};
use crate::links::IssueContext;
use crate::phone::quick_replies::{self, ReplySet};
use crate::scrollbar::Scrollbar;
use crate::session_menu::{on_press, popover_frame};
use crate::shell_lines;
use crate::sidebar::{agent_up, runtime_label, session_view_key};
use crate::stretchtext;
use crate::summary::{SummaryLog, wanted_loads};
use crate::target::HostKey;
use crate::text_input::{CompletionEvent, ImagePasted, TextChanged, TextInput, TextInputEvent};
pub(crate) use crate::text_select::{SelectDraw, select_text};
use crate::text_select::{TextRegistry, align_shift, painted_mark};
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
/// The name of the file type Attach's file picker offers: every file.
const ATTACH_FILTER: &str = "All files";
/// How many rows the composer grows between.
const COMPOSER_MIN_ROWS: usize = 2;
const COMPOSER_MAX_ROWS: usize = 6;

/// What the phone's empty composer says.
const PHONE_COMPOSER_PLACEHOLDER: &str = "Message";
/// How many rows the phone's composer grows between.
const PHONE_COMPOSER_MIN_ROWS: usize = 1;
const PHONE_COMPOSER_MAX_ROWS: usize = 5;

/// The characters that open the desktop composer's completion popup.
const COMPLETION_TRIGGERS: &[char] = &['/', '@'];

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
/// How many lines tall a Tail box is, and its line height over its text
/// size.
const TAIL_ROWS: f32 = 12.0;
const TAIL_LINE_RATIO: f32 = 1.4;

/// Whether `session` offers its chat: an interactive agent session, not a
/// plain shell or a headless run.
pub(crate) fn offers_chat(session: &SessionSnapshot) -> bool {
    session.mode == SessionMode::Interactive && session.agent.is_some()
}

/// The tooltip of a Chat or Summary control drawn disabled while its
/// session's agent is not running.
pub(crate) const AGENT_DOWN_TIP: &str = "The agent isn't running";

/// Whether `session` can show its chat and summary now: it offers a chat
/// and its agent is up in the pane (see [`agent_up`]), or its host's agents
/// (`agents`) do not name its agent yet, so nothing says it is down.
pub(crate) fn chat_available(session: &SessionSnapshot, agents: &[AgentDescriptor]) -> bool {
    let Some(agent) = session.agent.as_ref().filter(|_| offers_chat(session)) else {
        return false;
    };
    descriptor_for(agents, agent).is_none()
        || agent_up(session, runtime_label(session, agents).as_deref(), agents).is_some()
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
    /// The Linear context its answers link issue identifiers against, as its
    /// host last gave it.
    issues: IssueContext,
    /// The work running beside the chat, for its strip; a `Chat` reply
    /// leaves it as it is.
    work: RunningWork,
    /// The model the session's main agent answers with, as the daemon's last
    /// `ChatModel` named it: the id a later switch of the model uses.
    model: Option<String>,
    /// That model in the form a person reads, as the agent plugin formatted
    /// it; none while the daemon sent no label.
    label: Option<String>,
    /// The effort the session's main agent answers with, as the daemon's
    /// last `ChatModel` named it: shown verbatim, mapped by nothing.
    effort: Option<String>,
    /// The text the chat sends to open the agent's own model switcher, as
    /// the daemon's last `ChatModel` named it; none when the agent has none,
    /// and the model pill is then read-only.
    switch_command: Option<String>,
    /// The screen's last composer state; none until the chat loaded.
    composer_state: Option<ComposerState>,
    /// Esc was sent to interrupt the running turn; held until the next
    /// screen reading so a second press cannot open the agent's Rewind.
    interrupt_sent: bool,
    /// The cards' busy and "Answered elsewhere" marks.
    cards: CardState,
    /// The card the text box belongs to, and the box text answers took.
    boxes: CardBoxes,
    /// The current card's text box, while that card has an option that
    /// takes typed text; it lives with its card.
    card_box: Option<CardBox>,
    /// The chat send in flight.
    sends: SendTracker,
    /// The pull of the prompt box's draft awaiting its answer.
    pull: DraftPull,
    /// The last clear of the prompt box's draft, until a refusal names it.
    clear: DraftClear,
    /// Whether the draft's Clear was pressed and awaits its Confirm clear.
    clear_armed: bool,
    /// The composer, made once the chat has loaded.
    composer: Option<Composer>,
    /// The daemon's reason for refusing the last send.
    send_error: Option<String>,
    /// The phone's selection of the chat's text; it goes with the chat.
    selection: Option<Selection>,
    /// The (start, end) of the word the phone's latest hold picked, which
    /// the held finger widens the selection from.
    hold: Option<(TextPos, TextPos)>,
    /// The selection handle's drag in progress on the phone, if any.
    handle_drag: Option<HandleDrag>,
}

/// What one daemon refusal ended in a chat: the text of a send it refused
/// (empty for a draft sent as it stands), the box text a card answer took
/// and where it goes back, the refused answer's card, and whether it ended
/// the pending draft pull or named the last draft clear.
struct ChatRefusal {
    text: Option<String>,
    boxed: Option<(String, GiveBack)>,
    card: Option<crate::chat_cards::AnswerRefusal>,
    pull: bool,
    clear: bool,
}

impl ChatRefusal {
    /// Whether the refusal ended anything of the chat's.
    fn is_ours(&self) -> bool {
        self.text.is_some()
            || self.boxed.is_some()
            || self.card.is_some()
            || self.pull
            || self.clear
    }
}

/// An answer's parsed document, with the text length, live flag, open
/// stretches and Linear issue context it was parsed at: a change to any of
/// them parses it again.
struct AnswerDoc {
    len: usize,
    live: bool,
    open: BTreeSet<String>,
    issues: IssueContext,
    doc: Arc<MdDoc>,
}

/// A loaded chat's composer, its completion popup and its subscriptions.
pub(crate) struct Composer {
    input: Entity<TextInput>,
    completion: CompletionState,
    /// Whether the completion is the phone's tap-only strip: the input never
    /// enters the popup's key context, so Return stays a line break and Up,
    /// Down, Tab and Esc keep their own behaviour.
    tap_only: bool,
    /// The search of the session's files for the `@` token.
    files: FileSearch,
    /// The debounce timer of the next file search.
    file_timer: Option<Task<()>>,
    /// The session's commands and skills for the `/` token.
    commands: CommandCatalog,
    _subscriptions: [Subscription; 4],
}

impl Composer {
    /// Tells the input whether the popup is open, so its keys go to it; the
    /// phone's tap-only strip never takes its keys.
    fn sync_completion(&self, cx: &mut App) {
        let open = self.completion.is_open() && !self.tap_only;
        self.input
            .update(cx, |input, cx| input.set_completion_open(open, cx));
    }

    /// The caret now sits in `token`, or none: an `@` token restarts the
    /// file search's debounce, which sends only on a host that offers file
    /// search (`searchable`); leaving it stops the timer, forgets the
    /// pending search and removes the rows it gave.
    fn follow_file_token(
        &mut self,
        session_id: &str,
        token: Option<&Token>,
        searchable: bool,
        cx: &mut Context<RootView>,
    ) {
        if token.is_some_and(|token| token.trigger == FILE_TRIGGER) {
            let generation = self.files.token_changed();
            if searchable {
                let id = session_id.to_owned();
                self.file_timer = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(FILE_SEARCH_DEBOUNCE).await;
                    // Fails only when the view is gone, and the composer with it.
                    this.update(cx, |this, _| this.send_file_search(&id, generation))
                        .ok();
                }));
            }
            return;
        }
        self.file_timer = None;
        if self.files.token_left() {
            self.completion.set_items(Vec::new());
        }
    }

    /// The caret now sits in `token`, or none: a `/` token on a host that
    /// offers the catalog (`offered`) shows the cached entries matching its
    /// query, or, with none cached and none awaited, returns the request id
    /// of the catalog request to send; leaving it removes the rows the
    /// catalog gave.
    fn follow_command_token(&mut self, token: Option<&Token>, offered: bool) -> Option<String> {
        let Some(token) = token.filter(|token| token.trigger == COMMAND_TRIGGER) else {
            if self.commands.token_left() {
                self.completion.set_items(Vec::new());
            }
            return None;
        };
        if !offered {
            return None;
        }
        if let Some(rows) = self.commands.rows_for(&token.query) {
            self.completion.set_items(rows);
            return None;
        }
        if !self.commands.wants_fetch() {
            return None;
        }
        let request_id = new_request_id();
        self.commands.sent(request_id.clone());
        Some(request_id)
    }
}

/// A card's single-line text box and its subscriptions.
pub(crate) struct CardBox {
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

    /// The label the chat's model pill shows: the model's label, else its
    /// id; none while no model is known.
    fn model_label(&self) -> Option<&str> {
        self.label.as_deref().or(self.model.as_deref())
    }

    /// The text the chat's model pill shows: the model's label, followed by
    /// the effort the main agent answered with when it named one; none while
    /// no model is known.
    fn pill_text(&self) -> Option<String> {
        let label = self.model_label()?;
        match self.effort.as_deref() {
            Some(effort) if !effort.is_empty() => Some(format!("{label} · {effort}")),
            _ => Some(label.to_owned()),
        }
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

    /// The daemon refused `request_id` with `message`: what it ends of this
    /// chat's sends, card answers and draft pull; none when it ends none.
    fn take_refusal(&mut self, request_id: &str, message: &str) -> Option<ChatRefusal> {
        let refusal = ChatRefusal {
            text: self.sends.refused(request_id),
            boxed: self.boxes.refused(request_id),
            card: self.cards.refused(request_id, message),
            pull: self.pull.answered(Some(request_id)),
            clear: self.clear.refused(request_id),
        };
        refusal.is_ours().then_some(refusal)
    }

    /// Settles `refusal`: box text a card answer took goes back to the card
    /// box while its card is up, and sent text goes back to the composer
    /// before what was typed since, with `message` under it. A draft sent
    /// as it stands gives back no text, so the composer and its caret stay
    /// as they are; it, a refused pull and a refused clear still show
    /// `message`.
    fn settle_refusal(&mut self, refusal: ChatRefusal, message: &str, cx: &mut App) {
        let mut text = refusal.text;
        match refusal.boxed {
            Some((taken, GiveBack::CardBox)) => {
                let current = self.card_box_text(cx);
                self.set_card_box_text(&restored_box_text(&taken, &current), cx);
            }
            Some((taken, GiveBack::Composer)) => text = Some(taken),
            None => {}
        }
        let shows_reason = text.is_some() || refusal.pull || refusal.clear;
        if let Some(text) = text.filter(|text| !text.is_empty()) {
            self.give_back(&text, cx);
        }
        if shows_reason {
            self.send_error = Some(message.to_owned());
        }
    }

    /// The card box's text, empty while the card has no box.
    fn card_box_text(&self, cx: &App) -> String {
        self.card_box
            .as_ref()
            .map(|card_box| card_box.input.read(cx).text().to_owned())
            .unwrap_or_default()
    }

    /// Puts `text` in the card box, the caret at its end.
    fn set_card_box_text(&self, text: &str, cx: &mut App) {
        if let Some(card_box) = &self.card_box {
            let text = text.to_owned();
            card_box
                .input
                .update(cx, |input, cx| input.set_text(text, cx));
        }
    }

    /// Whether the card box has `window`'s keyboard.
    fn card_box_has_keyboard(&self, window: &Window, cx: &App) -> bool {
        self.card_box
            .as_ref()
            .is_some_and(|card_box| card_box.input.read(cx).focus_handle(cx).is_focused(window))
    }

    /// Whether the card on screen was submitted and is still up.
    fn card_busy(&self) -> bool {
        self.stack
            .current()
            .is_some_and(|prompt| self.cards.is_busy(&prompt.id))
    }

    /// A screen reading: the card stack, the cards' marks, the send in
    /// flight and the composer state follow it; a box no longer holding a
    /// draft drops an armed Clear.
    fn take_screen(&mut self, screen: &ChatScreen, now: Instant) {
        if !matches!(screen.composer, ComposerState::Draft) {
            self.clear_armed = false;
        }
        let before = self.stack.current().map(|prompt| prompt.id.clone());
        self.stack.on_screen(screen, now);
        let after = screen.prompt.as_ref().map(|prompt| prompt.id.as_str());
        self.cards.on_screen(before.as_deref(), after);
        self.sends.on_composer(&screen.composer);
        self.composer_state = Some(screen.composer.clone());
        self.interrupt_sent = false;
    }

    /// Ends the phone's selection: it, the hold it grew from and any
    /// handle drag moving it.
    fn drop_selection(&mut self) {
        self.selection = None;
        self.hold = None;
        self.handle_drag = None;
    }

    /// Applies one chat message: a `Chat` that answers this chat's load or
    /// that the daemon sent unasked (a reset that keeps open cards open), an
    /// append, a screen change, the running work, or the session's model. A
    /// reply to an older load changes nothing. Returns whether it applied.
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
                    None => {
                        self.log.reset(entries, hidden);
                        // The chat started over: its agent's commands may
                        // have changed, so the next `/` asks again.
                        if let Some(composer) = self.composer.as_mut() {
                            composer.commands.forget();
                        }
                    }
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
            DaemonMessage::ChatModel {
                model,
                label,
                effort,
                switch_command,
                ..
            } => {
                self.model = model;
                self.label = label;
                self.effort = effort;
                self.switch_command = switch_command;
            }
            _ => return false,
        }
        true
    }

    /// Links its answers' issue identifiers against `context`, parsing every
    /// answer again when it is not the context they were parsed with.
    pub(crate) fn set_issue_context(&mut self, context: IssueContext) {
        if self.issues == context {
            return;
        }
        self.issues = context;
        self.refresh_docs();
    }

    /// Parses each answer whose text is new or changed length, whose live
    /// flag changed, whose stretches opened or closed, or whose issue context
    /// changed since its last parse, and forgets the answers that left.
    fn refresh_docs(&mut self) {
        let mut parsed = std::mem::take(&mut self.docs);
        let cancel = AtomicBool::new(false);
        let none = BTreeSet::new();
        let issues = self.issues.clone();
        for entry in self.log.entries() {
            let ChatEntry::Answer { id, text, live } = entry else {
                continue;
            };
            let open = self.log.open_stretchtext(id).unwrap_or(&none);
            let doc = parsed
                .remove(id)
                .filter(|doc| {
                    doc.len == text.len()
                        && doc.live == *live
                        && doc.open == *open
                        && doc.issues == issues
                })
                .unwrap_or_else(|| {
                    let composed = stretchtext::compose(text, open, *live);
                    AnswerDoc {
                        len: text.len(),
                        live: *live,
                        open: open.clone(),
                        issues: issues.clone(),
                        doc: Arc::new(prepare_answer(&composed, &issues, &cancel)),
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
    /// By session id, the composer text its chat held when it unloaded,
    /// given back to the composer when the chat loads again.
    drafts: HashMap<String, String>,
    /// The summaries this connection has loaded, and each one's lines.
    summary_loads: ChatLoads,
    pub(crate) summaries: HashMap<String, SummaryLog>,
    /// Each chat or summary pane's scroll.
    pub(crate) scrolls: HashMap<String, Scrollbar>,
    notes: HashMap<String, String>,
    /// The request ids of loads forgotten when the session's other load was
    /// refused, whose own refusal is taken without a word.
    refused_loads: BTreeSet<String>,
    /// The host of each session with a chat or summary loaded, so its
    /// requests reach that host even once its list has dropped the session.
    hosts: HashMap<String, HostKey>,
    /// The followed files the Tail boxes under the chats' tool cards and
    /// the follow tabs read, and each box's scroll by session id and path.
    pub(crate) tails: ChatTails,
    tail_scrolls: HashMap<(String, String), ScrollHandle>,
    wall: WallClock,
    /// Repaints the running-work strips while one shows, so their ages and
    /// countdowns keep counting.
    work_strip_timer: Option<Task<()>>,
    /// How many times that timer has repainted.
    work_strip_repaints: usize,
    /// The session the phone shows, and its host: its chat loads as a pane
    /// on it would ([`PHONE_PANE`]).
    phone: Option<(HostKey, String)>,
    /// The note the phone shows in place of its session's chat once the
    /// daemon refused to load it; it is not asked again until the phone
    /// shows a session again or its host reconnects.
    phone_refused: Option<String>,
    /// The phone chat's selectable texts as its last frame drew them.
    phone_text: Rc<RefCell<TextRegistry>>,
    /// The finger of the selection drag the phone has claimed, while one is
    /// under way: its session, its window point and what it drags. Only the
    /// edge scroll's tick reads it.
    drag_finger: Option<(String, Point<Pixels>, DragKind)>,
    /// Whether the edge scroll's tick runs ([`RootView::start_edge_tick`]).
    edge_ticking: bool,
    /// How many times the phone's chat body has been built, for specs.
    phone_chat_renders: Cell<usize>,
}

/// What a selection drag on the phone's chat moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DragKind {
    /// The held finger of a hold that picked a word.
    Hold,
    /// A selection handle.
    Handle,
}

/// How often the edge scroll's tick runs.
const EDGE_TICK: Duration = Duration::from_millis(16);

impl TextRegistry {
    /// Where the handle of the selection's end `pos` goes, relative to
    /// `origin`: the caret before byte `pos.byte` of its drawn text, snapped
    /// to whole pixels, and its line's height; `None` when that text was not
    /// drawn or the caret lies outside `viewport` (the chat's scroll area).
    fn spot(
        &self,
        pos: &TextPos,
        origin: Point<Pixels>,
        viewport: Option<Bounds<Pixels>>,
    ) -> Option<HandleSpot> {
        let text = self
            .texts
            .iter()
            .find(|text| text.entry == pos.entry && text.unit == pos.unit)?;
        let layout = &text.layout;
        let at = layout.position_for_index(pos.byte)?;
        // The middle of the caret's row picks the row whose offset it takes.
        let row_y = at.y + layout.line_height() / 2.0;
        let caret = point(at.x + align_shift(layout, text.align, row_y), at.y);
        if !viewport.is_none_or(|view| view.contains(&caret)) {
            return None;
        }
        Some(HandleSpot {
            caret: point((caret.x - origin.x).round(), (caret.y - origin.y).round()),
            line_height: layout.line_height(),
        })
    }
}

/// The side of a hit area that takes a finger: the phone's touch target
/// (`TOUCH_TARGET` in `phone/home_view.rs`).
const HANDLE_TARGET: f32 = 44.0;
/// A selection handle's stem width.
const HANDLE_STEM: f32 = 2.0;
/// A selection handle's knob diameter.
const HANDLE_KNOB: f32 = 10.0;

/// Which end of the phone chat's selection a handle moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HandleEnd {
    /// The selection's first end, in reading order; its knob sits above.
    Start,
    /// The selection's last end, in reading order; its knob sits below.
    End,
}

impl HandleEnd {
    /// The handle's driver id, also the key of its touch-drag state.
    pub(crate) fn key(self) -> &'static str {
        match self {
            HandleEnd::Start => "phone-chat-handle-start",
            HandleEnd::End => "phone-chat-handle-end",
        }
    }
}

/// Where one selection handle draws: its end's caret point (the top of the
/// caret's line, where the stem's left edge starts) and that line's height.
#[derive(Debug, Clone, Copy, PartialEq)]
struct HandleSpot {
    caret: Point<Pixels>,
    line_height: Pixels,
}

impl HandleSpot {
    /// The middle of the stem, where the handle aims.
    fn stem_middle(self) -> Point<Pixels> {
        point(
            self.caret.x + px(HANDLE_STEM / 2.0),
            self.caret.y + self.line_height / 2.0,
        )
    }

    /// The knob's centre: centred on the stem, half a knob above its top
    /// for the start, half a knob below its bottom for the end.
    fn knob_centre(self, end: HandleEnd) -> Point<Pixels> {
        let x = self.caret.x + px(HANDLE_STEM / 2.0);
        match end {
            HandleEnd::Start => point(x, self.caret.y - px(HANDLE_KNOB / 2.0)),
            HandleEnd::End => point(x, self.caret.y + self.line_height + px(HANDLE_KNOB / 2.0)),
        }
    }

    /// The vector from `grab`, where a finger took the handle (in the
    /// spot's own coordinates), to the stem's middle, which a drag adds to
    /// the finger to find the text it aims at: the end moves from where it
    /// is, wherever on the hit area the finger took it.
    fn aim_offset(self, grab: Point<Pixels>) -> Point<Pixels> {
        self.stem_middle() - grab
    }

    /// The top-left of the handle's square hit area, centred on its knob.
    fn hit_origin(self, end: HandleEnd) -> Point<Pixels> {
        let knob = self.knob_centre(end);
        let half = px(HANDLE_TARGET / 2.0);
        point(knob.x - half, knob.y - half)
    }
}

/// The selection handles a frame measured: each end's spot, relative to
/// `origin` (the window point of the rows' box the handles draw in), and
/// that box's size; an end is `None` when its caret was not drawn inside
/// the chat's scroll area.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct HandleSpots {
    origin: Point<Pixels>,
    layer: Size<Pixels>,
    start: Option<HandleSpot>,
    end: Option<HandleSpot>,
}

impl HandleSpots {
    /// End `end`'s spot.
    fn of(self, end: HandleEnd) -> Option<HandleSpot> {
        match end {
            HandleEnd::Start => self.start,
            HandleEnd::End => self.end,
        }
    }

    /// Whether window point `at` lies inside a drawn handle's square hit
    /// area, within the layer that clips it.
    fn covers(self, at: Point<Pixels>) -> bool {
        if !Bounds::new(self.origin, self.layer).contains(&at) {
            return false;
        }
        let side = px(HANDLE_TARGET);
        [HandleEnd::Start, HandleEnd::End].into_iter().any(|end| {
            self.of(end).is_some_and(|spot| {
                let origin = self.origin + spot.hit_origin(end);
                Bounds::new(origin, gpui::size(side, side)).contains(&at)
            })
        })
    }
}

/// Whether window point `at` lies inside a hit area of the handles
/// `spots` measured; never with no handles.
fn in_handle_area(spots: Option<HandleSpots>, at: Point<Pixels>) -> bool {
    spots.is_some_and(|spots| spots.covers(at))
}

/// The Copy button's label.
const COPY_LABEL: &str = "Copy";
/// The Copy button's horizontal padding.
const COPY_PAD: f32 = 14.0;
/// The Copy button's corner radius.
const COPY_RADIUS: f32 = 8.0;

/// The Copy button's size, a touch target tall and its label plus its
/// padding and border wide, and its label's text size.
#[expect(
    clippy::cast_precision_loss,
    reason = "the label is four characters, far inside f32's exact integer range"
)]
fn copy_button_size(cx: &App) -> (Size<Pixels>, Pixels) {
    let text_size = fonts::ui_px(cx, UI_TEXT_SIZE);
    let text = cx.text_system();
    let id = text.resolve_font(&gpui::font(fonts::ui_family(cx)));
    let measured: anyhow::Result<f32> = COPY_LABEL
        .chars()
        .map(|ch| {
            text.advance(id, text_size, ch)
                .map(|advance| advance.width / px(1.0))
        })
        .sum();
    // A face without one of the label's glyphs: a full em a letter.
    let label = measured.unwrap_or(COPY_LABEL.chars().count() as f32 * (text_size / px(1.0)));
    let width = label.ceil() + 2.0 * COPY_PAD + 2.0;
    (gpui::size(px(width), px(HANDLE_TARGET)), text_size)
}

/// The phone chat's Copy button at `origin` in the handle layer, of
/// `size`, its label at `text_size`; a tap on it runs `on_copy` and goes
/// no further.
fn copy_button(
    origin: Point<Pixels>,
    (size, text_size): (Size<Pixels>, Pixels),
    t: &UiTheme,
    on_copy: impl Fn(&mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .id("phone-chat-copy")
        .driver_id(|| "phone-chat-copy".to_owned())
        .absolute()
        .left(origin.x)
        .top(origin.y)
        .w(size.width)
        .h(size.height)
        .flex()
        .items_center()
        .justify_center()
        .px(px(COPY_PAD))
        .rounded(px(COPY_RADIUS))
        .bg(gpui::rgb(t.popover))
        .border_1()
        .border_color(gpui::rgb(t.popover_edge))
        .text_color(gpui::rgb(t.text))
        .text_size(text_size)
        .on_click(move |_: &ClickEvent, window, cx| {
            cx.stop_propagation();
            on_copy(window, cx);
        })
        .child(COPY_LABEL)
        .into_any_element()
}

/// The (start, end) spots of `spots` the Copy button is placed from: each
/// end whose caret lies inside `view` (the chat's scroll area), the one in
/// view standing in for the other when only one is; none when neither is.
/// A dragged handle's spot outside the view keeps its drag area but never
/// places the button.
fn copy_spots(
    spots: HandleSpots,
    view: Option<Bounds<Pixels>>,
) -> Option<(HandleSpot, HandleSpot)> {
    let shown = |spot: Option<HandleSpot>| {
        spot.filter(|spot| view.is_none_or(|view| view.contains(&(spots.origin + spot.caret))))
    };
    let (start, end) = (shown(spots.start), shown(spots.end));
    Some((start.or(end)?, end.or(start)?))
}

/// The gap between the Copy button and the handle hit area it sits beside.
const COPY_GAP: f32 = 4.0;

/// The top-left of the phone chat's Copy button, of size `button`, in a
/// handle layer of size `layer`, for a selection whose ends draw at
/// `start` and `end`: its bottom edge [`COPY_GAP`] above the top of the
/// start handle's hit area, centred on the start's caret and kept inside
/// the layer's width; when that would put its top above the layer's top,
/// its top edge [`COPY_GAP`] below the bottom of the end handle's hit
/// area instead, still centred on the start's caret; when it fits there
/// neither, its bottom on the layer's bottom, its top never above the
/// layer's top.
fn copy_button_origin(
    start: HandleSpot,
    end: HandleSpot,
    layer: Size<Pixels>,
    button: Size<Pixels>,
) -> Point<Pixels> {
    let unit = px(1.0);
    let (width, height) = (button.width / unit, button.height / unit);
    let right = (layer.width / unit - width).max(0.0);
    let x = (start.caret.x / unit - width / 2.0).clamp(0.0, right);
    let above = start.hit_origin(HandleEnd::Start).y / unit - COPY_GAP - height;
    let below = end.hit_origin(HandleEnd::End).y / unit + HANDLE_TARGET + COPY_GAP;
    let lowest = (layer.height / unit - height).max(0.0);
    let y = if above >= 0.0 {
        above
    } else {
        below.min(lowest)
    };
    point(px(x), px(y))
}

/// A selection handle's drag in progress: the handle the finger took, the
/// selection's end that stays, and the vector added to the finger to find
/// the end that moves.
struct HandleDrag {
    end: HandleEnd,
    fixed: TextPos,
    aim: Point<Pixels>,
}

/// The touch-drag areas of the two selection handles, which the phone's
/// session view makes (each over its handle's hit area).
pub(crate) struct HandleAreas {
    /// The start handle's area.
    pub(crate) start: AnyElement,
    /// The end handle's area.
    pub(crate) end: AnyElement,
}

/// A selection handle drawn at `spot` in `color`: a 44 px square hit area
/// centred on its knob, tagged with its driver id and holding `area`, with
/// the stem and the knob drawn inside it; they take no input.
fn handle_element(end: HandleEnd, spot: HandleSpot, color: Hsla, area: AnyElement) -> AnyElement {
    let origin = spot.hit_origin(end);
    let knob = spot.knob_centre(end) - origin;
    div()
        .driver_id(|| end.key().to_owned())
        .absolute()
        .left(origin.x)
        .top(origin.y)
        .size(px(HANDLE_TARGET))
        .child(
            div()
                .absolute()
                .left(spot.caret.x - origin.x)
                .top(spot.caret.y - origin.y)
                .w(px(HANDLE_STEM))
                .h(spot.line_height)
                .bg(color),
        )
        .child(
            div()
                .absolute()
                .left(knob.x - px(HANDLE_KNOB / 2.0))
                .top(knob.y - px(HANDLE_KNOB / 2.0))
                .size(px(HANDLE_KNOB))
                .rounded_full()
                .bg(color),
        )
        .child(area)
        .into_any_element()
}

/// The (start, end) spots of `spots` and the layer's size, as a frame
/// draws its handles and its Copy button from.
type SpotPair = (Option<HandleSpot>, Option<HandleSpot>, Size<Pixels>);

/// The [`SpotPair`] of `spots`.
fn spot_pair(spots: Option<HandleSpots>) -> Option<SpotPair> {
    spots.map(|spots| (spots.start, spots.end, spots.layer))
}

/// A canvas over the rows' box that measures, once the rows before it are
/// laid out, where the handles of the selection's ordered `ends` go, and
/// stores that in `registry`; when it differs from `drawn`, the spots this
/// frame drew its handles at, the window draws again, so frames settle.
/// The handle a finger drags (`dragged`) is measured wherever its caret
/// is, inside the chat's scroll area or not, so it keeps its drag area.
fn handle_measure(
    registry: Rc<RefCell<TextRegistry>>,
    ends: Option<(TextPos, TextPos)>,
    viewport: Option<ScrollHandle>,
    drawn: Option<HandleSpots>,
    dragged: Option<HandleEnd>,
) -> impl IntoElement {
    canvas(
        move |bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App| {
            let view = viewport.as_ref().map(ScrollHandle::bounds);
            let view_of = |end: HandleEnd| view.filter(|_| dragged != Some(end));
            let measured = ends.as_ref().map(|(start, end)| {
                let registry = registry.borrow();
                HandleSpots {
                    origin: bounds.origin,
                    layer: bounds.size,
                    start: registry.spot(start, bounds.origin, view_of(HandleEnd::Start)),
                    end: registry.spot(end, bounds.origin, view_of(HandleEnd::End)),
                }
            });
            registry.borrow_mut().handles = measured;
            if spot_pair(measured) != spot_pair(drawn) {
                // A refresh while drawing is ignored: ask once this frame is done.
                window.defer(cx, |window, _| window.refresh());
            }
        },
        |_, (), _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

/// The phone chat's selectable texts, in the order it draws them: Compact,
/// folding only runs of four or more calls.
fn phone_units(chat: &SessionChat) -> Units {
    let rows = chat::display_rows_folding(chat.log.entries(), ChatMode::Compact, PHONE_MIN_RUN);
    Units::build(
        &rows,
        |id| chat.docs.get(id).map(|answer| answer.doc.as_ref()),
        |id| chat.log.is_expanded(id),
        |id| chat.log.is_run_open(id),
    )
}

/// `selection` with both ends moved into `units` ([`Units::clamp`]); `None`
/// when either end's entry has no units any more.
fn clamped(units: &Units, selection: &Selection) -> Option<Selection> {
    Some(Selection {
        anchor: units.clamp(&selection.anchor)?,
        focus: units.clamp(&selection.focus)?,
    })
}

/// A position from a spec hook's plain data: (entry id, unit index, byte).
fn text_pos((entry, unit, byte): (String, u32, usize)) -> TextPos {
    TextPos { entry, unit, byte }
}

/// The pane id the phone's shown session's chat loads and scrolls under.
pub(crate) const PHONE_PANE: &str = "phone";
/// The fewest read, search and run calls in a row the phone folds into one
/// row.
const PHONE_MIN_RUN: usize = 4;

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
            drafts: HashMap::new(),
            summary_loads: ChatLoads::default(),
            summaries: HashMap::new(),
            scrolls: HashMap::new(),
            notes: HashMap::new(),
            refused_loads: BTreeSet::new(),
            hosts: HashMap::new(),
            tails: ChatTails::default(),
            tail_scrolls: HashMap::new(),
            wall,
            work_strip_timer: None,
            work_strip_repaints: 0,
            phone: None,
            phone_refused: None,
            phone_text: Rc::default(),
            drag_finger: None,
            edge_ticking: false,
            phone_chat_renders: Cell::new(0),
        }
    }

    /// Session `session_id`'s load was refused with `note`: when it is the
    /// phone's session, the phone shows `note` and asks no more.
    fn refuse_phone(&mut self, session_id: &str, note: &str) {
        if self.phone.as_ref().is_some_and(|(_, id)| id == session_id) {
            self.phone_refused = Some(note.to_owned());
        }
    }

    /// The session `session_id`'s loaded chat.
    pub(crate) fn session(&self, session_id: &str) -> Option<&SessionChat> {
        self.sessions.get(session_id)
    }

    /// The prompt card on screen in the session `session_id`'s loaded chat,
    /// when one is loaded and a card is up.
    pub(crate) fn held_prompt(&self, session_id: &str) -> Option<&ChatPrompt> {
        self.session(session_id)?.stack.current()
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

    /// `host`'s new connection has loaded no chat: every chat pane of that
    /// host asks again once its pane is bound to its session, its composer
    /// text kept as the draft, and so does each of its open Tail boxes once
    /// its chat loads again.
    pub(crate) fn reset_connection(&mut self, host: &HostKey, cx: &App) {
        self.tails.reset_host(host);
        if self
            .phone
            .as_ref()
            .is_some_and(|(phone_host, _)| phone_host == host)
        {
            self.phone_refused = None;
        }
        let gone: Vec<String> = self
            .hosts
            .iter()
            .filter(|(_, session_host)| *session_host == host)
            .map(|(session_id, _)| session_id.clone())
            .collect();
        for session_id in gone {
            self.stash_draft(&session_id, cx);
            self.forget_chat_load(&session_id);
            self.forget_summary_load(&session_id);
            self.hosts.remove(&session_id);
        }
        self.refused_loads.clear();
        if self.sessions.is_empty() {
            self.work_strip_timer = None;
        }
    }

    /// The host whose connection session `session_id`'s chat requests take.
    pub(crate) fn host_of(&self, session_id: &str) -> HostKey {
        self.hosts
            .get(session_id)
            .cloned()
            .unwrap_or(HostKey::Local)
    }

    /// Forgets the host of each session with nothing loaded any more.
    fn retain_loaded_hosts(&mut self) {
        let (sessions, summaries) = (&self.sessions, &self.summaries);
        self.hosts.retain(|session_id, _| {
            sessions.contains_key(session_id) || summaries.contains_key(session_id)
        });
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

    /// Keeps session `session_id`'s composer text as its draft, for the
    /// chat's next load; an empty composer leaves no draft. A chat whose
    /// load is still awaited has no composer yet, so the draft it would
    /// restore is kept as it is.
    fn stash_draft(&mut self, session_id: &str, cx: &App) {
        let Some(chat) = self.sessions.get(session_id) else {
            return;
        };
        if chat.composer.is_none() {
            return;
        }
        let text = chat.composer_text(cx);
        if text.is_empty() {
            self.drafts.remove(session_id);
        } else {
            self.drafts.insert(session_id.to_owned(), text);
        }
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
    /// Whether the session's host follows files, so its tool cards offer
    /// Tail buttons.
    pub(crate) can_tail: bool,
    /// The open Tail boxes, and each one's scroll.
    pub(crate) tails: &'a ChatTails,
    pub(crate) tail_scrolls: &'a HashMap<(String, String), ScrollHandle>,
    /// How the chat pane draws tool calls.
    pub(crate) mode: ChatMode,
    /// The widest the chat column draws, centred in a wider pane; `None`
    /// draws it at the pane's full width (a summary pane's look).
    pub(crate) max_width: Option<f32>,
    /// The answers that draw as their turn's final reply, in a box; none in
    /// a summary pane's look.
    pub(crate) finals: BTreeSet<String>,
    /// The fewest calls in a row Compact folds into one row; a shorter run
    /// draws each of its rows.
    pub(crate) min_run: usize,
    /// How the rows' selectable texts are drawn and registered; only the
    /// phone's chat has it.
    pub(crate) select: Option<&'a SelectDraw>,
    /// The links an answer follows: web, stretch and file links on the
    /// desktop, web and stretch links on the phone.
    pub(crate) link_route: LinkRoute,
}

/// The parts of a tool call a card draws.
struct ToolRow<'a> {
    id: &'a str,
    name: &'a str,
    summary: &'a str,
    input: &'a str,
    result: Option<&'a ToolOutcome>,
    /// The files it offers a Tail of; none where its card draws no Tail.
    tail_paths: &'a [String],
}

impl RootView {
    /// What pane `pane_id`, showing `session_id`, draws: the view it was
    /// switched to while the session can show its chat, else its terminal.
    /// The view it was switched to is kept, so the pane goes back to it when
    /// the session's agent starts again.
    pub(crate) fn view_shown(&self, pane_id: &str, session_id: Option<&str>) -> PaneView {
        // The phone's Terminal page shows the terminal whatever the chat.
        if self.is_phone_terminal(pane_id) {
            return PaneView::Terminal;
        }
        if session_id.is_some_and(|id| self.session_chat_available(id)) {
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

    /// The session whose chat the active tab's focused pane shows, when
    /// that pane shows its chat.
    pub(crate) fn focused_pane_chat_session(&self) -> Option<String> {
        let pane_id = self.focused_pane()?;
        if !self.pane_shows_chat(&pane_id) {
            return None;
        }
        self.panes
            .get(&pane_id)
            .and_then(|slot| slot.session())
            .map(str::to_owned)
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

    /// The label the `chat-model` pill draws for session `session_id`'s
    /// chat; none while no pane shows its chat or no model is known.
    #[must_use]
    pub fn chat_model_label(&self, session_id: &str) -> Option<String> {
        self.chat.sessions.get(session_id)?.pill_text()
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

    /// The lines the Tail box of `path` in session `session_id` draws; none
    /// while it is closed.
    #[must_use]
    pub fn chat_tail_lines(&self, session_id: &str, path: &str) -> Option<Vec<String>> {
        let tail = self.chat.tails.get(session_id, path)?;
        Some(match tail.shows() {
            TailShows::Nothing => vec![NO_OUTPUT.to_owned()],
            TailShows::Error(message) => vec![message.to_owned()],
            TailShows::Lines(lines) => lines.into_iter().map(str::to_owned).collect(),
        })
    }

    /// Whether the open Tail box of `path` in session `session_id` follows
    /// its bottom; none while it is closed.
    #[must_use]
    pub fn chat_tail_following(&self, session_id: &str, path: &str) -> Option<bool> {
        self.chat.tails.get(session_id, path).map(Tail::following)
    }

    /// How far, in pixels, chat pane `pane_id`'s rows are scrolled from
    /// their top; none while it shows no chat.
    #[must_use]
    pub fn chat_scroll_offset(&self, pane_id: &str) -> Option<f32> {
        let scroll = self.chat.scrolls.get(pane_id)?;
        Some(-scroll.handle().offset().y / px(1.0))
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
        self.sync_chat_loads(cx);
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
    /// session can show its chat now.
    fn press_view_key(
        &mut self,
        pane_id: &str,
        key: ViewKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.pane_chat_available(pane_id) {
            let next = self.chat.views.get(pane_id).after_key(key);
            self.set_pane_view(pane_id, next, window, cx);
        }
    }

    /// Ctrl+Shift+M or Ctrl+Shift+U in the main window: switches the
    /// active tab's focused pane, when its session can show its chat now
    /// (it offers one and its agent is running).
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
    /// pane it draws (a popped tab's focused pane) when its session can show
    /// its chat now (it offers one and its agent is running).
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

    /// The key `session_views` files the session pane `pane_id` shows under.
    fn pane_view_key(&self, pane_id: &str) -> String {
        self.panes.get(pane_id).map_or_else(
            || session_view_key(&HostKey::Local),
            |slot| session_view_key(slot.host()),
        )
    }

    /// Whether session `session_id` can show its chat and summary now, read
    /// against its own host's agents (see [`chat_available`]).
    pub(crate) fn session_chat_available(&self, session_id: &str) -> bool {
        self.sidebar
            .session(session_id)
            .is_some_and(|session| chat_available(session, self.sidebar.session_agents(session_id)))
    }

    /// Whether the session pane `pane_id` shows can show its chat and
    /// summary now.
    fn pane_chat_available(&self, pane_id: &str) -> bool {
        self.panes
            .get(pane_id)
            .and_then(|slot| slot.session())
            .is_some_and(|id| self.session_chat_available(id))
    }

    /// Why pane `pane_id` cannot switch to `view` now: Chat and Summary
    /// while its session's agent is not running. `None` when it can.
    #[must_use]
    pub fn pane_view_disabled_tip(&self, pane_id: &str, view: PaneView) -> Option<&'static str> {
        (view.hides_terminal() && !self.pane_chat_available(pane_id)).then_some(AGENT_DOWN_TIP)
    }

    /// A session snapshot or list came in: a pane whose session's agent
    /// stopped shows its terminal, which takes the keyboard when that pane
    /// held it, and one whose agent started again shows the view it was
    /// switched to, its terminal letting go of the keyboard, as a switch by
    /// hand does. The chats and summaries load or unload to match.
    pub(crate) fn follow_agent_runs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The last sync left one scroll for each pane that hid its terminal.
        let hid: BTreeSet<String> = self.chat.scrolls.keys().cloned().collect();
        self.sync_chat_loads(cx);
        self.arm_work_strip_repaint(cx);
        self.update_pane_roles(cx);
        let hides: BTreeSet<String> = self.chat.scrolls.keys().cloned().collect();
        for pane_id in hides.difference(&hid) {
            self.release_pane_keyboard(pane_id, window, cx);
        }
        for pane_id in hid.difference(&hides) {
            if self.pane_holds_focus(pane_id) {
                self.focus_pane_where_drawn(pane_id, window, cx);
            }
        }
    }

    /// Whether pane `pane_id` is the one its window's keys go to: a popped
    /// pane's window's pane, else the main window's focused pane.
    fn pane_holds_focus(&self, pane_id: &str) -> bool {
        match self.slot_window(pane_id) {
            Some(here) => self.window_pane(here).as_deref() == Some(pane_id),
            None => self.focused_pane().as_deref() == Some(pane_id),
        }
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
        let host = self.pane_view_key(pane_id);
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
        let host = self.pane_view_key(pane_id);
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
    fn blur_pane(&self, pane_id: &str, window: &mut Window, cx: &mut App) {
        let focused = self
            .panes
            .get(pane_id)
            .is_some_and(|slot| slot.view().read(cx).is_focused(window));
        if !focused {
            return;
        }
        if self.is_main_window(window) {
            self.root_focus.focus(window, cx);
        } else {
            window.blur(cx);
        }
    }

    /// Brings the loaded chats and summaries in line with the panes:
    /// `LoadChat` for a session the first chat or summary pane (in any
    /// window) now shows, `LoadSummary` for one the first summary pane now
    /// shows, and `UnloadChat` or `UnloadSummary` once the last such pane
    /// stopped showing it. A pane that is gone forgets its view.
    pub(crate) fn sync_chat_loads(&mut self, cx: &App) {
        let panes = &self.panes;
        self.chat
            .views
            .retain(|pane_id| panes.contains_key(pane_id));
        let shown = self.chats_shown();
        for (_, session_id, _, host) in &shown {
            self.chat.hosts.insert(session_id.clone(), host.clone());
        }
        let shown_panes: BTreeSet<&str> = shown
            .iter()
            .map(|(pane_id, _, _, _)| pane_id.as_str())
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
                .map(|(_, session_id, view, _)| (session_id.as_str(), *view)),
        );
        self.sync_summary_loads(wanted.summary);
        let changes = self.chat.loads.diff(wanted.chat);
        for session_id in changes.unload {
            self.chat.stash_draft(&session_id, cx);
            self.chat.sessions.remove(&session_id);
            let untails = self.chat.tails.close_session(&session_id);
            self.send_untails(untails);
            let host = self.chat.host_of(&session_id);
            self.send_to(&host, ClientMessage::UnloadChat { session_id });
        }
        for session_id in changes.load {
            let request_id = new_request_id();
            let host = self.chat.host_of(&session_id);
            let issues = self.issue_context_of(&host);
            let mut chat = SessionChat {
                request: Some(request_id.clone()),
                ..SessionChat::default()
            };
            chat.set_issue_context(issues);
            self.chat.sessions.insert(session_id.clone(), chat);
            self.send_to(
                &host,
                ClientMessage::LoadChat {
                    session_id,
                    request_id: Some(request_id),
                },
            );
        }
        self.sync_follow_tabs();
        let listed = self.follow_tab_sessions_ready();
        let ChatPanes {
            tails, sessions, ..
        } = &mut self.chat;
        let asks = tails.resend(
            |session_id| sessions.contains_key(session_id),
            |host, session_id| listed.contains(&(host.clone(), session_id.to_owned())),
            new_request_id,
        );
        for (host, msg) in asks {
            self.send_to(&host, msg);
        }
        self.chat.retain_loaded_hosts();
    }

    /// Hands `host`'s Linear issue context `context` to each of its loaded
    /// chats, so an answer parsed before the context arrived links its issue
    /// identifiers.
    pub(crate) fn set_chat_issue_context(&mut self, host: &HostKey, context: &IssueContext) {
        let sessions: Vec<String> = self
            .chat
            .sessions
            .keys()
            .filter(|session_id| self.chat.host_of(session_id) == *host)
            .cloned()
            .collect();
        for session_id in sessions {
            if let Some(chat) = self.chat.sessions.get_mut(&session_id) {
                chat.set_issue_context(context.clone());
            }
        }
    }

    /// Every pane drawing its chat or summary, as its pane id, its session,
    /// its view and its session's host; and, while the phone shows a session
    /// whose chat can show, that session's chat under [`PHONE_PANE`].
    fn chats_shown(&self) -> Vec<(String, String, PaneView, HostKey)> {
        let mut shown: Vec<(String, String, PaneView, HostKey)> = self
            .panes
            .iter()
            .filter_map(|(pane_id, slot)| {
                let session_id = slot.session()?;
                let view = self.view_shown(pane_id, Some(session_id));
                view.hides_terminal().then(|| {
                    let host = slot.host().clone();
                    (pane_id.clone(), session_id.to_owned(), view, host)
                })
            })
            .collect();
        if let Some((host, session_id)) = &self.chat.phone
            && self.chat.phone_refused.is_none()
            && self.session_chat_available(session_id)
        {
            let phone = PHONE_PANE.to_owned();
            shown.push((phone, session_id.clone(), PaneView::Chat, host.clone()));
        }
        shown
    }

    /// The phone now shows `shown`, a host's session, or no session: that
    /// session's chat loads while it can show, as a pane's would, and the
    /// last one shown unloads.
    pub(crate) fn show_phone_chat(&mut self, shown: Option<(HostKey, String)>, cx: &App) {
        // Another session, or none: the one shown before loses its selection.
        if let Some((_, before)) = self
            .chat
            .phone
            .as_ref()
            .filter(|before| shown.as_ref() != Some(*before))
            && let Some(chat) = self.chat.sessions.get_mut(before)
        {
            chat.drop_selection();
        }
        self.chat.phone = shown;
        self.chat.phone_refused = None;
        self.sync_chat_loads(cx);
    }

    /// The host's session the phone shows, while it shows one.
    pub(crate) fn phone_chat_shown(&self) -> Option<&(HostKey, String)> {
        self.chat.phone.as_ref()
    }

    /// The note the phone shows in place of its session's chat since the
    /// daemon refused to load it, while it does.
    #[must_use]
    pub fn phone_chat_note(&self) -> Option<&str> {
        self.chat.phone_refused.as_deref()
    }

    /// The rows the phone draws for session `session_id`'s chat, as plain
    /// data: Compact, folding only runs of four or more calls; none while it
    /// is not loaded.
    #[must_use]
    pub fn phone_chat_display_rows(&self, session_id: &str) -> Vec<RowShape> {
        self.chat
            .sessions
            .get(session_id)
            .map(|chat| {
                chat::display_rows_folding(chat.log.entries(), ChatMode::Compact, PHONE_MIN_RUN)
                    .iter()
                    .map(DisplayRow::shape)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Spec hook: the position in the phone chat's selectable text that
    /// window point `point` falls on, as plain data (entry id, unit index,
    /// byte): the text it is on, else the one nearest it above or below;
    /// none before the chat's texts have been painted.
    #[must_use]
    pub fn phone_chat_hit(&self, point: Point<Pixels>) -> Option<(String, u32, usize)> {
        self.chat
            .phone_text
            .borrow()
            .hit(point, self.phone_viewport())
    }

    /// The phone chat's scroll area, in window coordinates, once drawn.
    fn phone_viewport(&self) -> Option<Bounds<Pixels>> {
        self.chat
            .scrolls
            .get(PHONE_PANE)
            .map(|scroll| scroll.handle().bounds())
    }

    /// The phone draws no chat now (another page, or no session): its last
    /// frame's texts are forgotten, so no hit reads them.
    pub(crate) fn forget_phone_chat_text(&self) {
        self.chat.phone_text.borrow_mut().forget();
    }

    /// A hold at window point `point` on the phone's chat of session
    /// `session_id`: on a selectable text the selection becomes the word
    /// under the finger, which a held drag widens from; anywhere else (a
    /// button, a muted row, white space, empty space, or before the chat
    /// has painted) nothing is selected and the selection stays. Whether
    /// the hold selected a word.
    pub(crate) fn phone_select_start(
        &mut self,
        session_id: &str,
        point: Point<Pixels>,
        cx: &mut Context<Self>,
    ) -> bool {
        let hit = self
            .chat
            .phone_text
            .borrow()
            .hit_inside(point, self.phone_viewport());
        let word = self.chat.sessions.get(session_id).and_then(|chat| {
            let (entry, unit, byte) = hit?;
            phone_units(chat).word_around(&TextPos { entry, unit, byte })
        });
        let Some(chat) = self.chat.sessions.get_mut(session_id) else {
            return false;
        };
        chat.hold.clone_from(&word);
        let Some((start, end)) = word else {
            return false;
        };
        chat.selection = Some(Selection {
            anchor: start,
            focus: end,
        });
        cx.notify();
        true
    }

    /// The held finger of the hold that picked a word in session
    /// `session_id`'s phone chat is at window point `point`: the selection
    /// runs from that word to the text nearest the finger
    /// ([`chat_select::widen`]).
    pub(crate) fn phone_select_widen(
        &mut self,
        session_id: &str,
        point: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let held = self.chat.sessions.get(session_id);
        if held.is_some_and(|chat| chat.hold.is_some()) {
            self.track_drag_finger(session_id, point, DragKind::Hold, cx);
        }
        let hit = self
            .chat
            .phone_text
            .borrow()
            .hit(point, self.phone_viewport());
        let Some(chat) = self.chat.sessions.get_mut(session_id) else {
            return;
        };
        let (Some(word), Some((entry, unit, byte))) = (chat.hold.as_ref(), hit) else {
            return;
        };
        let focus = TextPos { entry, unit, byte };
        let widened = chat_select::widen(&phone_units(chat), word, focus);
        if widened.is_some() && widened != chat.selection {
            chat.selection = widened;
            cx.notify();
        }
    }

    /// A finger took selection handle `end` of session `session_id`'s
    /// phone chat at window point `grab`: the selection's other end stays
    /// where it is, and the moving end aims at the finger plus the vector
    /// from `grab` to the handle's stem's middle, as the handle stands now.
    pub(crate) fn phone_handle_start(
        &mut self,
        session_id: &str,
        end: HandleEnd,
        grab: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let aim = self
            .chat
            .phone_text
            .borrow()
            .handles
            .and_then(|spots| Some(spots.of(end)?.aim_offset(grab - spots.origin)));
        let Some(chat) = self.chat.sessions.get_mut(session_id) else {
            return;
        };
        let units = phone_units(chat);
        let ordered = chat
            .selection
            .as_ref()
            .and_then(|selection| clamped(&units, selection))
            .and_then(|selection| selection.ordered(&units));
        chat.handle_drag = aim.zip(ordered).map(|(aim, (start, last))| HandleDrag {
            end,
            fixed: match end {
                HandleEnd::Start => last,
                HandleEnd::End => start,
            },
            aim,
        });
        cx.notify();
    }

    /// The finger dragging a selection handle of session `session_id`'s
    /// phone chat is at window point `finger`: the selection runs from the
    /// end that stays to the text nearest the point the handle aims at, so
    /// dragging past the other end swaps the ends; an empty selection is
    /// allowed while dragging.
    pub(crate) fn phone_handle_move(
        &mut self,
        session_id: &str,
        finger: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let dragging = self.chat.sessions.get(session_id);
        if dragging.is_some_and(|chat| chat.handle_drag.is_some()) {
            self.track_drag_finger(session_id, finger, DragKind::Handle, cx);
        }
        let viewport = self.phone_viewport();
        let Some(chat) = self.chat.sessions.get_mut(session_id) else {
            return;
        };
        let Some(drag) = chat.handle_drag.as_ref() else {
            return;
        };
        let aimed = finger + drag.aim;
        let Some(hit) = self.chat.phone_text.borrow().hit(aimed, viewport) else {
            return;
        };
        let moved = Some(Selection {
            anchor: drag.fixed.clone(),
            focus: text_pos(hit),
        });
        if moved != chat.selection {
            chat.selection = moved;
            cx.notify();
        }
    }

    /// The finger dragging a selection handle of session `session_id`'s
    /// phone chat lifted or was cancelled: the drag ends, and the selection
    /// stays as it last moved.
    pub(crate) fn phone_handle_end(&mut self, session_id: &str, cx: &mut Context<Self>) {
        self.end_drag_finger(session_id);
        if let Some(chat) = self.chat.sessions.get_mut(session_id) {
            chat.handle_drag = None;
            cx.notify();
        }
    }

    /// The held finger of a hold on session `session_id`'s phone chat
    /// lifted or was cancelled: its drag ends, and the selection stays as
    /// it last widened.
    pub(crate) fn phone_select_hold_end(&mut self, session_id: &str) {
        self.end_drag_finger(session_id);
    }

    /// The finger of a selection drag the phone claimed on session
    /// `session_id`'s chat, dragging `kind`, is at window point `finger`:
    /// the edge scroll reads it, and its tick starts unless one runs.
    fn track_drag_finger(
        &mut self,
        session_id: &str,
        finger: Point<Pixels>,
        kind: DragKind,
        cx: &mut Context<Self>,
    ) {
        self.chat.drag_finger = Some((session_id.to_owned(), finger, kind));
        self.start_edge_tick(cx);
    }

    /// The selection drag on session `session_id`'s phone chat is over:
    /// the edge scroll has no finger to follow.
    fn end_drag_finger(&mut self, session_id: &str) {
        if self
            .chat
            .drag_finger
            .as_ref()
            .is_some_and(|(id, _, _)| id == session_id)
        {
            self.chat.drag_finger = None;
        }
    }

    /// Starts the edge scroll's tick unless one runs or no drag is under
    /// way: every [`EDGE_TICK`] it scrolls the phone chat by [`edge_step`]
    /// over the time since the last tick ([`Self::edge_tick`]), until that
    /// stops it or the view is gone.
    fn start_edge_tick(&mut self, cx: &mut Context<Self>) {
        if self.chat.edge_ticking || self.chat.drag_finger.is_none() {
            return;
        }
        self.chat.edge_ticking = true;
        cx.spawn(async move |this, cx| {
            let mut last = cx.background_executor().now();
            loop {
                cx.background_executor().timer(EDGE_TICK).await;
                let now = cx.background_executor().now();
                let dt = now.saturating_duration_since(last).as_secs_f32();
                last = now;
                match this.update(cx, |root, cx| root.edge_tick(dt, cx)) {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(err) => {
                        tracing::debug!("the edge scroll's root view has gone: {err:#}");
                        break;
                    }
                }
            }
        })
        .detach();
    }

    /// One tick of the edge scroll, `dt` seconds after the last: whether
    /// it goes on ([`Self::edge_scroll`]); when it does not, the tick ends.
    fn edge_tick(&mut self, dt: f32, cx: &mut Context<Self>) -> bool {
        let going = self.edge_scroll(dt, cx);
        if !going {
            self.chat.edge_ticking = false;
        }
        going
    }

    /// While a selection drag's finger is in the phone chat's edge band,
    /// with its session the one the phone shows: scrolls the chat by
    /// [`edge_step`] over `dt` seconds ([`leave_bottom`]), within its rows,
    /// and moves the drag again at the finger, so the selection follows the
    /// rows under it; a chat already at its end there neither scrolls nor
    /// redraws, and waits. Whether the tick goes on: no drag, another
    /// session, no chat drawn or a finger outside the bands stop it, and so
    /// does a selection gone or no longer in the chat's texts, which also
    /// ends the drag's finger.
    fn edge_scroll(&mut self, dt: f32, cx: &mut Context<Self>) -> bool {
        let Some((session_id, finger, kind)) = self.chat.drag_finger.clone() else {
            return false;
        };
        let Some((view, handle)) = self.phone_scroll_of(&session_id) else {
            return false;
        };
        if !self.phone_selection_resolves(&session_id) {
            self.chat.drag_finger = None;
            return false;
        }
        let unit = px(1.0);
        let step = edge_step(finger.y / unit, view.top() / unit, view.bottom() / unit, dt);
        if step == 0.0 {
            return false;
        }
        let offset = handle.offset();
        let max = (handle.max_offset().y / unit).max(0.0);
        let from = -offset.y / unit;
        let to = (from + leave_bottom(step, from, max)).clamp(0.0, max);
        if (to - from).abs() > f32::EPSILON {
            handle.set_offset(point(offset.x, px(-to)));
            match kind {
                DragKind::Handle => self.phone_handle_move(&session_id, finger, cx),
                DragKind::Hold => self.phone_select_widen(&session_id, finger, cx),
            }
            cx.notify();
        }
        true
    }

    /// The phone chat's scroll area, in window coordinates, and its scroll
    /// handle, while the phone shows session `session_id`'s chat.
    fn phone_scroll_of(&self, session_id: &str) -> Option<(Bounds<Pixels>, ScrollHandle)> {
        let shown = self
            .chat
            .phone
            .as_ref()
            .is_some_and(|(_, id)| id == session_id);
        let scroll = self.chat.scrolls.get(PHONE_PANE).filter(|_| shown)?;
        let handle = scroll.handle().clone();
        Some((handle.bounds(), handle))
    }

    /// Whether session `session_id`'s chat has a selection that still lies
    /// in its texts.
    fn phone_selection_resolves(&self, session_id: &str) -> bool {
        self.chat.sessions.get(session_id).is_some_and(|chat| {
            let units = phone_units(chat);
            chat.selection
                .as_ref()
                .and_then(|selection| clamped(&units, selection))
                .and_then(|selection| selection.ordered(&units))
                .and_then(|ordered| units.resolve(&ordered))
                .is_some()
        })
    }

    /// Spec hook: how many times the phone's chat body has been built.
    #[must_use]
    pub fn phone_chat_renders(&self) -> usize {
        self.chat.phone_chat_renders.get()
    }

    /// Spec hook: the window points the phone chat's start and end
    /// selection handles aim at (each stem's middle), as the latest frame
    /// measured them; `None` unless both handles are drawn.
    #[must_use]
    pub fn phone_chat_handle_aims(&self) -> Option<(Point<Pixels>, Point<Pixels>)> {
        let spots = self.chat.phone_text.borrow().handles?;
        let (start, end) = spots.start.zip(spots.end)?;
        Some((
            spots.origin + start.stem_middle(),
            spots.origin + end.stem_middle(),
        ))
    }

    /// Spec hook: selects session `session_id`'s chat text from `anchor` to
    /// `focus`, each as plain data (entry id, unit index, byte); the
    /// phone's chat draws it while that chat stays loaded.
    pub fn set_phone_chat_selection(
        &mut self,
        session_id: &str,
        anchor: (String, u32, usize),
        focus: (String, u32, usize),
        cx: &mut Context<Self>,
    ) {
        if let Some(chat) = self.chat.sessions.get_mut(session_id) {
            chat.selection = Some(Selection {
                anchor: text_pos(anchor),
                focus: text_pos(focus),
            });
            cx.notify();
        }
    }

    /// Ends session `session_id`'s phone chat selection, with the hold it
    /// grew from and any handle drag; specs call it too.
    pub fn clear_phone_chat_selection(&mut self, session_id: &str, cx: &mut Context<Self>) {
        self.end_drag_finger(session_id);
        if let Some(chat) = self.chat.sessions.get_mut(session_id) {
            chat.drop_selection();
            cx.notify();
        }
    }

    /// The Copy button's tap on session `session_id`'s phone chat: the
    /// selection goes on the clipboard, with the copied chip, and ends. An
    /// empty selection copies nothing, so the clipboard keeps what it held;
    /// specs call it too.
    pub fn phone_copy_selection(&mut self, session_id: &str, cx: &mut Context<Self>) {
        let text = self.phone_chat_selected_text(session_id);
        if !text.is_empty() {
            self.copy_to_clipboard(&text, cx);
        }
        self.clear_phone_chat_selection(session_id, cx);
    }

    /// Whether a release at window point `at` lies inside a selection
    /// handle's hit area, as the latest frame measured them: such a tap
    /// keeps the selection.
    pub(crate) fn phone_chat_release_on_handle(&self, at: Point<Pixels>) -> bool {
        in_handle_area(self.chat.phone_text.borrow().handles, at)
    }

    /// Spec hook: each (entry id, unit index, bytes) the phone chat's
    /// latest frame drew on the selection's colour, in drawing order.
    #[must_use]
    pub fn phone_chat_painted_selection(&self) -> Vec<(String, u32, Range<usize>)> {
        self.chat.phone_text.borrow().painted_ranges.clone()
    }

    /// Spec hook: the byte range the composer of session `session_id`
    /// selects, its caret when empty; none without a composer.
    #[must_use]
    pub fn chat_composer_selection(&self, session_id: &str, cx: &App) -> Option<Range<usize>> {
        let chat = self.chat.sessions.get(session_id)?;
        let composer = chat.composer.as_ref()?;
        Some(composer.input.read(cx).selected_range())
    }

    /// Spec hook: the text session `session_id`'s chat selection copies, as
    /// the phone draws the chat; empty without a selection.
    #[must_use]
    pub fn phone_chat_selected_text(&self, session_id: &str) -> String {
        let Some(chat) = self.chat.sessions.get(session_id) else {
            return String::new();
        };
        let units = phone_units(chat);
        chat.selection
            .as_ref()
            .and_then(|selection| clamped(&units, selection))
            .map(|selection| chat_select::selected_text(&units, &selection))
            .unwrap_or_default()
    }

    /// Sends each of `untails` to its host while that host's connection is
    /// open, and forgets the scroll of every closed Tail box.
    pub(crate) fn send_untails(&mut self, untails: Vec<(HostKey, ClientMessage)>) {
        for (host, msg) in untails {
            if self.conn_of(&host).is_some_and(Connection::is_open) {
                self.send_to(&host, msg);
            }
        }
        let ChatPanes {
            tails,
            tail_scrolls,
            ..
        } = &mut self.chat;
        tail_scrolls.retain(|(session_id, path), _| tails.is_open(session_id, path));
    }

    /// A press on a tool card's Tail button for `path`, the `index`th file
    /// of tool call `tool_id` in session `session_id`'s chat: opens its box
    /// under the card and asks the session's host to follow the file, or
    /// closes the open box and stops the follow. A follow tab reading the
    /// same file shares its follow: the box then asks for nothing and reads
    /// the lines so far, and its close leaves the tab's follow running.
    pub fn toggle_chat_tail(
        &mut self,
        session_id: &str,
        tool_id: &str,
        index: usize,
        path: &str,
        cx: &mut Context<Self>,
    ) {
        if self.chat.tails.is_open(session_id, path) {
            let untail = self.chat.tails.close(session_id, path);
            self.send_untails(untail.into_iter().collect());
        } else {
            let host = self.chat.host_of(session_id);
            let card = TailCard {
                tool_id: tool_id.to_owned(),
                index,
            };
            let ask = self
                .chat
                .tails
                .open(host.clone(), session_id, path, card, new_request_id());
            self.chat.tail_scrolls.insert(
                (session_id.to_owned(), path.to_owned()),
                ScrollHandle::new(),
            );
            if let Some(ask) = ask {
                self.send_to(&host, ask);
            }
        }
        cx.notify();
    }

    /// A `FileTail` from `host`: the follow of its file takes it for every
    /// reader, its Chat box and its follow tabs, else it is dropped.
    fn on_file_tail(&mut self, host: &HostKey, msg: &DaemonMessage, cx: &mut Context<Self>) {
        let DaemonMessage::FileTail {
            session_id,
            path,
            data_b64,
            reset,
            ..
        } = msg
        else {
            return;
        };
        if self
            .chat
            .tails
            .on_file_tail(host, session_id, path, data_b64, *reset)
        {
            cx.notify();
        } else {
            tracing::debug!(%session_id, path, reset, "FileTail for a file nothing reads, or queued before its follow's reset: dropped");
        }
    }

    /// The summaries some summary pane shows are now `wanted`: unloads the
    /// ones no pane shows any more and loads the new ones.
    fn sync_summary_loads(&mut self, wanted: BTreeSet<String>) {
        let changes = self.chat.summary_loads.diff(wanted);
        for session_id in changes.unload {
            self.chat.summaries.remove(&session_id);
            let host = self.chat.host_of(&session_id);
            self.send_to(&host, ClientMessage::UnloadSummary { session_id });
        }
        for session_id in changes.load {
            let request_id = new_request_id();
            self.chat
                .summaries
                .insert(session_id.clone(), SummaryLog::asked(request_id.clone()));
            let host = self.chat.host_of(&session_id);
            self.send_to(
                &host,
                ClientMessage::LoadSummary {
                    session_id,
                    request_id: Some(request_id),
                },
            );
        }
    }

    /// A message no earlier fold took: a chat message updates its chat, a
    /// summary message its summary, a file's tail its Tail box, a removed
    /// session closes its Tail boxes, and anything else goes to the agents'
    /// fold.
    pub(crate) fn fold_chat_or_agents(
        &mut self,
        host: &crate::target::HostKey,
        msg: DaemonMessage,
        cx: &mut Context<Self>,
    ) {
        match msg {
            DaemonMessage::Chat { .. }
            | DaemonMessage::ChatAppend { .. }
            | DaemonMessage::ChatScreenChanged { .. }
            | DaemonMessage::ChatWork { .. }
            | DaemonMessage::ChatModel { .. } => self.on_chat_message(msg, cx),
            msg @ DaemonMessage::ChatDraft { .. } => self.on_chat_draft(msg, cx),
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
            msg @ DaemonMessage::FileTail { .. } => self.on_file_tail(host, &msg, cx),
            DaemonMessage::FileMatches {
                session_id,
                matches,
                request_id,
                ..
            } => self.on_file_matches(&session_id, matches, request_id.as_deref(), cx),
            DaemonMessage::Catalog {
                session_id,
                entries,
                request_id,
            } => self.on_catalog(&session_id, entries, request_id.as_deref(), cx),
            DaemonMessage::SessionRemoved { session_id } => {
                let untails = self.chat.tails.close_session(&session_id);
                self.send_untails(untails);
                self.chat.tails.session_ended(&session_id);
                self.fold_agents(host, DaemonMessage::SessionRemoved { session_id }, cx);
            }
            other => self.fold_agents(host, other, cx),
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

    /// A `ChatDraft` answering its session's pending pull: the draft goes into
    /// the composer ahead of what was typed there, with a note when the
    /// terminal's box still holds some of it. One no pull awaits is
    /// dropped.
    fn on_chat_draft(&mut self, msg: DaemonMessage, cx: &mut Context<Self>) {
        let DaemonMessage::ChatDraft {
            session_id,
            request_id,
            text,
            cleared,
        } = msg
        else {
            return;
        };
        let Some(chat) = self
            .chat
            .sessions
            .get_mut(&session_id)
            .filter(|chat| chat.pull.is_pending())
        else {
            tracing::debug!(%session_id, "draft for a chat that awaits no pull: dropped");
            return;
        };
        if !chat.pull.answered(request_id.as_deref()) {
            tracing::debug!(%session_id, "draft answering another pull: dropped");
            return;
        }
        tracing::info!(%session_id, cleared, "pulled the prompt box's draft into the composer");
        chat.give_back(&text, cx);
        chat.send_error = chat::pulled_error(cleared);
        cx.notify();
    }

    /// A `Chat`, `ChatAppend`, `ChatScreenChanged`, `ChatWork` or
    /// `ChatModel`: it updates the chat of a session a chat pane shows; one
    /// for any other session, or a reply to an older load, is dropped.
    pub(crate) fn on_chat_message(&mut self, msg: DaemonMessage, cx: &mut Context<Self>) {
        let now = (self.now)();
        let session_id = match &msg {
            DaemonMessage::Chat { session_id, .. }
            | DaemonMessage::ChatAppend { session_id, .. }
            | DaemonMessage::ChatScreenChanged { session_id, .. }
            | DaemonMessage::ChatWork { session_id, .. }
            | DaemonMessage::ChatModel { session_id, .. } => session_id.clone(),
            _ => return,
        };
        let work = matches!(msg, DaemonMessage::ChatWork { .. });
        // Only an unasked reset says a card left: the reply to a load, after
        // a reconnect, may be empty while the transcript is read again.
        let full = matches!(
            msg,
            DaemonMessage::Chat {
                request_id: None,
                ..
            }
        );
        let applied = self
            .chat
            .sessions
            .get_mut(&session_id)
            .is_some_and(|chat| chat.apply(msg, now));
        if applied {
            if work {
                self.arm_work_strip_repaint(cx);
            }
            if full {
                self.close_tails_without_cards(&session_id);
            }
            self.follow_summary_panes(&session_id);
            self.ensure_composer(&session_id, cx);
            self.ensure_card_box(&session_id, cx);
            cx.notify();
        } else {
            tracing::debug!(%session_id, "chat message for a chat no pane shows, or for an older load: dropped");
        }
    }

    /// After a `Chat` that replaced session `session_id`'s rows: each Tail
    /// box whose tool card is no longer among them closes.
    fn close_tails_without_cards(&mut self, session_id: &str) {
        let ChatPanes {
            tails, sessions, ..
        } = &mut self.chat;
        let Some(chat) = sessions.get(session_id) else {
            return;
        };
        let untails = tails.close_missing_cards(session_id, |tool_id| {
            matches!(chat.log.get(tool_id), Some(ChatEntry::Tool { .. }))
        });
        self.send_untails(untails);
    }

    /// A daemon `Error` that answers one of this connection's pending
    /// `TailFile`s shows in that file's Tail box. One that answers a pending
    /// `LoadChat` or `LoadSummary`: every chat and summary pane of that
    /// session snaps back to its terminal with a note, nothing is unloaded
    /// and a later toggle asks again. The session's other load, still
    /// pending, is forgotten with it, and its own refusal is taken silently. An `unsupported_message` refusal says the session offers no
    /// chat; any other carries the daemon's `message`. Returns whether the
    /// error answered a `TailFile`, a `LoadChat` or one of the chat's own
    /// requests.
    pub(crate) fn on_chat_refused(
        &mut self,
        host: &HostKey,
        message: &str,
        request_id: Option<&str>,
        code: Option<&str>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(request_id) = request_id else {
            return false;
        };
        if self.chat.tails.on_refused(host, request_id, message) {
            tracing::info!(
                request_id,
                ?code,
                message,
                "the daemon refused to follow a file; its Tail box shows why"
            );
            cx.notify();
            return true;
        }
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
            if self.sidebar.remember_session_view(
                &session_view_key(host),
                &session_id,
                PaneView::Terminal,
            ) {
                self.save_ui();
            }
            UNSUPPORTED_NOTE.to_owned()
        } else {
            tracing::warn!(%session_id, ?code, message, "the daemon could not load this session's chat; its chat panes show the terminal");
            format!("{LOAD_FAILED_NOTE}{message}")
        };
        self.chat
            .forget_refused_load(&session_id, chat_refused.is_some());
        self.chat.refuse_phone(&session_id, &note);
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
        self.sync_chat_loads(cx);
        self.arm_work_strip_repaint(cx);
        self.update_pane_roles(cx);
        cx.notify();
        true
    }

    /// A daemon `Error` that answers one of our chat sends or card answers,
    /// shown inline with no toast. A refused send gives its text back to
    /// the composer before what was typed since, with the daemon's reason
    /// under the composer; a refused answer that took the card box's text
    /// gives it back to that box while its card is up, else to the
    /// composer with the reason under it; a refused answer to a card still
    /// up frees it with the reason on it. Returns whether the error was
    /// ours.
    fn on_chat_reply_refused(
        &mut self,
        message: &str,
        request_id: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        for (session_id, chat) in &mut self.chat.sessions {
            let Some(refusal) = chat.take_refusal(request_id, message) else {
                continue;
            };
            tracing::info!(%session_id, message, card = ?refusal.card, pull = refusal.pull, "the daemon refused a chat send, card answer or draft pull");
            chat.settle_refusal(refusal, message, cx);
            cx.notify();
            return true;
        }
        false
    }

    /// Makes session `session_id`'s composer once its chat has loaded:
    /// Ctrl+Enter and Esc in it go through [`Self::chat_key`], and every edit
    /// repaints the buttons beside it. The phone's sessions build it with the
    /// phone's values: its own placeholder, one to five rows and the soft
    /// keyboard's Send action key.
    fn ensure_composer(&mut self, session_id: &str, cx: &mut Context<Self>) {
        let wanted = self
            .chat
            .sessions
            .get(session_id)
            .is_some_and(|chat| chat.request.is_none() && chat.composer.is_none());
        if !wanted {
            return;
        }
        let draft = self.chat.drafts.remove(session_id).unwrap_or_default();
        let phone = self
            .chat
            .phone
            .as_ref()
            .is_some_and(|(_, id)| id == session_id);
        let input = cx.new(|cx| {
            let mut input = if phone {
                let mut input = TextInput::multi_line("", PHONE_COMPOSER_PLACEHOLDER, cx)
                    .with_rows(PHONE_COMPOSER_MIN_ROWS, PHONE_COMPOSER_MAX_ROWS)
                    .with_completion_triggers(COMPLETION_TRIGGERS)
                    .with_touch_editing();
                input.set_text_input_configuration(phone_composer_configuration(), cx);
                input
            } else {
                TextInput::multi_line("", COMPOSER_PLACEHOLDER, cx)
                    .with_rows(COMPOSER_MIN_ROWS, COMPOSER_MAX_ROWS)
                    .with_completion_triggers(COMPLETION_TRIGGERS)
                    .with_image_paste()
            };
            input.set_text(draft, cx);
            input
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
        let id = session_id.to_owned();
        let completions = cx.subscribe(&input, move |this, _, event: &CompletionEvent, cx| {
            this.on_completion_event(&id, event, cx);
        });
        let id = session_id.to_owned();
        let images = cx.subscribe(&input, move |this, _, event: &ImagePasted, cx| {
            this.on_chat_paste_image(&id, &event.name, &event.bytes, cx);
        });
        // The draft's set_text told its token before these subscriptions
        // were active, so the input's own token is folded as if it had just
        // changed.
        let token = input.read(cx).completion_token().cloned();
        if let Some(chat) = self.chat.sessions.get_mut(session_id) {
            chat.composer = Some(Composer {
                input,
                completion: CompletionState::default(),
                tap_only: phone,
                files: FileSearch::default(),
                file_timer: None,
                commands: CommandCatalog::default(),
                _subscriptions: [keys, edits, completions, images],
            });
        }
        if let Some(token) = token {
            self.on_completion_event(session_id, &CompletionEvent::TokenChanged(token), cx);
        }
    }

    /// Whether session `session_id`'s host offers file search.
    fn file_searchable(&self, session_id: &str) -> bool {
        self.conn_of(&self.chat.host_of(session_id))
            .is_some_and(|conn| conn.supports(FEATURE_FILE_SEARCH))
    }

    /// Folds a token change or clear from session `session_id`'s composer
    /// into its file search; any other event leaves it alone.
    fn fold_file_token(
        &mut self,
        session_id: &str,
        event: &CompletionEvent,
        cx: &mut Context<Self>,
    ) {
        let token = match event {
            CompletionEvent::TokenChanged(token) => Some(token),
            CompletionEvent::TokenCleared => None,
            _ => return,
        };
        let searchable = self.file_searchable(session_id);
        if let Some(composer) = self
            .chat
            .sessions
            .get_mut(session_id)
            .and_then(|chat| chat.composer.as_mut())
        {
            composer.follow_file_token(session_id, token, searchable, cx);
        }
    }

    /// Whether session `session_id`'s host offers its command catalog.
    fn catalog_offered(&self, session_id: &str) -> bool {
        self.conn_of(&self.chat.host_of(session_id))
            .is_some_and(|conn| conn.supports(FEATURE_CHAT_CATALOG))
    }

    /// Folds a token change or clear from session `session_id`'s composer
    /// into its command catalog, sending the one `ListCatalog` a first `/`
    /// token on a host that offers it asks for; any other event leaves it
    /// alone.
    fn fold_command_token(&mut self, session_id: &str, event: &CompletionEvent) {
        let token = match event {
            CompletionEvent::TokenChanged(token) => Some(token),
            CompletionEvent::TokenCleared => None,
            _ => return,
        };
        let offered = self.catalog_offered(session_id);
        let Some(request_id) = self
            .chat
            .sessions
            .get_mut(session_id)
            .and_then(|chat| chat.composer.as_mut())
            .and_then(|composer| composer.follow_command_token(token, offered))
        else {
            return;
        };
        let host = self.chat.host_of(session_id);
        self.send_to(
            &host,
            ClientMessage::ListCatalog {
                session_id: session_id.to_owned(),
                request_id: Some(request_id),
            },
        );
    }

    /// Folds what session `session_id`'s composer told of its completion
    /// token or the popup's keys into its [`CompletionState`]: Up and Down
    /// move the selection, Tab and Enter (`Accept`) put the pick in place of
    /// the token, Esc (`Close`) shuts the popup for this token.
    fn on_completion_event(
        &mut self,
        session_id: &str,
        event: &CompletionEvent,
        cx: &mut Context<Self>,
    ) {
        self.fold_file_token(session_id, event, cx);
        self.fold_command_token(session_id, event);
        let Some(composer) = self
            .chat
            .sessions
            .get_mut(session_id)
            .and_then(|chat| chat.composer.as_mut())
        else {
            return;
        };
        let state = &mut composer.completion;
        match event {
            CompletionEvent::TokenChanged(token) => state.token_changed(token.clone()),
            CompletionEvent::TokenCleared => state.token_cleared(),
            CompletionEvent::Up => state.select_previous(),
            CompletionEvent::Down => state.select_next(),
            CompletionEvent::Close => state.dismiss(),
            CompletionEvent::Accept => {
                if let Some((range, insert)) = state.pick() {
                    composer
                        .input
                        .update(cx, |input, cx| input.accept_completion(&range, &insert, cx));
                }
            }
        }
        composer.sync_completion(cx);
        cx.notify();
    }

    /// A click on row `index` of session `session_id`'s completion popup:
    /// it accepts that row and gives the composer the keyboard back.
    fn click_completion(
        &mut self,
        session_id: &str,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(composer) = self
            .chat
            .sessions
            .get_mut(session_id)
            .and_then(|chat| chat.composer.as_mut())
        else {
            return;
        };
        composer.completion.select(index);
        let input = composer.input.clone();
        input.read(cx).focus_handle(cx).focus(window, cx);
        self.on_completion_event(session_id, &CompletionEvent::Accept, cx);
    }

    /// A tap on row `index` of session `session_id`'s completion strip: it
    /// accepts that row and leaves the keyboard as it is, so the phone's
    /// field keeps its focus and the keyboard neither rises nor lowers.
    pub(crate) fn tap_completion(
        &mut self,
        session_id: &str,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(composer) = self
            .chat
            .sessions
            .get_mut(session_id)
            .and_then(|chat| chat.composer.as_mut())
        else {
            return;
        };
        composer.completion.select(index);
        self.on_completion_event(session_id, &CompletionEvent::Accept, cx);
    }

    /// Offers `items` in session `session_id`'s completion popup, the first
    /// selected; the popup shows them while the composer's caret sits in a
    /// `/` or `@` token. The specs give it a fixed list; the session's
    /// commands and files feed it from their own answers. Nothing happens
    /// before the composer is made.
    pub fn set_chat_completion_items(
        &mut self,
        session_id: &str,
        items: Vec<CompletionItem>,
        cx: &mut Context<Self>,
    ) {
        let Some(composer) = self
            .chat
            .sessions
            .get_mut(session_id)
            .and_then(|chat| chat.composer.as_mut())
        else {
            return;
        };
        composer.completion.set_items(items);
        composer.files.items_given();
        composer.commands.items_given();
        composer.sync_completion(cx);
        cx.notify();
    }

    /// The debounce timer of session `session_id`'s file search fired: while
    /// its `generation` is current, the caret sits in an `@` token and the
    /// session's host still offers file search, the token's query goes to
    /// that host, and its request id is the one an answer must carry.
    fn send_file_search(&mut self, session_id: &str, generation: u64) {
        if !self.file_searchable(session_id) {
            return;
        }
        let host = self.chat.host_of(session_id);
        let Some(composer) = self
            .chat
            .sessions
            .get_mut(session_id)
            .and_then(|chat| chat.composer.as_mut())
            .filter(|composer| composer.files.is_current(generation))
        else {
            return;
        };
        let Some(query) = composer
            .completion
            .token()
            .filter(|token| token.trigger == FILE_TRIGGER)
            .map(|token| token.query.clone())
        else {
            return;
        };
        let request_id = new_request_id();
        composer.files.sent(request_id.clone());
        self.send_to(
            &host,
            ClientMessage::SearchFiles {
                session_id: session_id.to_owned(),
                query,
                limit: Some(FILE_SEARCH_LIMIT),
                request_id: Some(request_id),
            },
        );
    }

    /// A `FileMatches` answering session `session_id`'s pending file search
    /// while the caret is still in its `@` token: its matches, in the
    /// daemon's order, become the popup's rows. Any other is dropped.
    fn on_file_matches(
        &mut self,
        session_id: &str,
        matches: Vec<FileMatch>,
        request_id: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        let Some(composer) = self
            .chat
            .sessions
            .get_mut(session_id)
            .and_then(|chat| chat.composer.as_mut())
        else {
            tracing::debug!(%session_id, "file matches for a chat with no composer: dropped");
            return;
        };
        if !composer.files.answered(request_id) {
            tracing::debug!(%session_id, "file matches for no pending search: dropped");
            return;
        }
        let items = matches
            .into_iter()
            .map(|file| CompletionItem::new(file.path, file.reference))
            .collect();
        composer.completion.set_items(items);
        composer.sync_completion(cx);
        cx.notify();
    }

    /// A `Catalog` answering session `session_id`'s pending catalog request:
    /// its entries are cached for the composer's `/` tokens, and while the
    /// caret sits in one, those matching its query become the popup's rows.
    /// Any other is dropped.
    fn on_catalog(
        &mut self,
        session_id: &str,
        entries: Vec<CatalogEntry>,
        request_id: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        let Some(composer) = self
            .chat
            .sessions
            .get_mut(session_id)
            .and_then(|chat| chat.composer.as_mut())
        else {
            tracing::debug!(%session_id, "command catalog for a chat with no composer: dropped");
            return;
        };
        let count = entries.len();
        if !composer.commands.answered(request_id, entries) {
            tracing::debug!(%session_id, "command catalog for no pending request: dropped");
            return;
        }
        tracing::debug!(%session_id, count, "command catalog cached for the composer");
        let query = composer
            .completion
            .token()
            .filter(|token| token.trigger == COMMAND_TRIGGER)
            .map(|token| token.query.clone());
        let Some(rows) = query.and_then(|query| composer.commands.rows_for(&query)) else {
            return;
        };
        composer.completion.set_items(rows);
        composer.sync_completion(cx);
        cx.notify();
    }

    /// Session `session_id`'s completion popup while it shows: its rows'
    /// labels and the selected row.
    #[must_use]
    pub fn chat_completion(&self, session_id: &str) -> Option<(Vec<String>, usize)> {
        let composer = self.chat.sessions.get(session_id)?.composer.as_ref()?;
        let state = &composer.completion;
        state.is_open().then(|| {
            let labels = state.items().iter().map(|item| item.label.clone());
            (labels.collect(), state.selected())
        })
    }

    /// Session `session_id`'s completion while it shows, as the phone's strip
    /// lists it: each item's short chip text, in the popup's order.
    #[must_use]
    pub fn chat_completion_chips(&self, session_id: &str) -> Option<Vec<String>> {
        let composer = self.chat.sessions.get(session_id)?.composer.as_ref()?;
        let state = &composer.completion;
        state
            .is_open()
            .then(|| state.items().iter().map(CompletionItem::chip).collect())
    }

    /// Session `session_id`'s completion popup above the composer field
    /// (`chat-completion`), while it is open: at most
    /// [`completion::MAX_ROWS`] rows (`chat-completion-{i}`), the selected
    /// one filled, a click accepting its row.
    fn completion_popup(
        session_id: &str,
        composer: &Composer,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let state = &composer.completion;
        if !state.is_open() {
            return None;
        }
        let (offset, t) = (fonts::ui_text_size(cx), theme(cx));
        let mut popup = popover_frame(offset, "chat-completion", &t)
            .flex()
            .flex_col()
            .py(px(4.0));
        for index in completion::visible_rows(state.items().len(), state.selected()) {
            let Some(item) = state.items().get(index) else {
                continue;
            };
            let selected = index == state.selected();
            let id = session_id.to_owned();
            let hover = t.popover_hover;
            popup = popup.child(
                div()
                    .id(ElementId::Name(format!("chat-completion-{index}").into()))
                    .driver_id(move || format!("chat-completion-{index}"))
                    .px(px(10.0))
                    .py(px(3.0))
                    .cursor_pointer()
                    .when(selected, |row| row.bg(gpui::rgb(t.selected_bg)))
                    .when(!selected, |row| {
                        row.hover(move |style| style.bg(gpui::rgb(hover)))
                    })
                    .child(item.label.clone())
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.click_completion(&id, index, window, cx);
                    })),
            );
        }
        Some(popup.into_any_element())
    }

    /// Gives session `session_id`'s current card its own empty text box
    /// when the card changed and has an option that takes typed text: the
    /// same card shown again keeps its box, a card without such an option
    /// has none. Enter in the box sends it through
    /// [`Self::submit_card_box`]; Esc gives the keyboard to the composer.
    fn ensure_card_box(&mut self, session_id: &str, cx: &mut Context<Self>) {
        let agent = self.agent_label(session_id);
        let Some(chat) = self.chat.sessions.get_mut(session_id) else {
            return;
        };
        if chat.request.is_some() {
            return;
        }
        let prompt = chat.stack.current();
        if !chat.boxes.show(prompt.map(|prompt| prompt.id.as_str())) {
            return;
        }
        chat.card_box = None;
        let Some(placeholder) = prompt.and_then(|prompt| box_placeholder(prompt, &agent)) else {
            return;
        };
        let input = cx.new(|cx| TextInput::new("", placeholder, cx));
        let id = session_id.to_owned();
        let keys = cx.subscribe(
            &input,
            move |this, _, event: &TextInputEvent, cx| match event {
                TextInputEvent::Submit => this.submit_card_box(&id, cx),
                TextInputEvent::Cancel => this.leave_card_box(&id, cx),
            },
        );
        let edits = cx.subscribe(&input, |_, _, _: &TextChanged, cx| cx.notify());
        if let Some(chat) = self.chat.sessions.get_mut(session_id) {
            chat.card_box = Some(CardBox {
                input,
                _subscriptions: [keys, edits],
            });
        }
    }

    /// Enter in session `session_id`'s card box: its text denies a
    /// permission card with the text alongside, and answers any other card
    /// as the user's own answer; a blank box sends nothing, and nothing is
    /// sent while a chat send is in flight.
    fn submit_card_box(&mut self, session_id: &str, cx: &mut Context<Self>) {
        let Some(chat) = self.chat.sessions.get(session_id) else {
            return;
        };
        if chat.sends.in_flight((self.now)()) {
            return;
        }
        let Some(prompt) = chat.stack.current().cloned() else {
            return;
        };
        let text = chat.card_box_text(cx);
        let answer = if prompt.kind == ChatPromptKind::Permission {
            chat::deny_with_text(&prompt, &text)
        } else {
            chat::type_something_answer(&text)
        };
        if let Some(answer) = answer {
            self.answer_card(session_id, &prompt, answer, Some(text), cx);
        }
    }

    /// Esc in session `session_id`'s card box: the composer takes the
    /// keyboard in the window the box had it in, and nothing is sent.
    fn leave_card_box(&self, session_id: &str, cx: &mut Context<Self>) {
        let Some(chat) = self.chat.sessions.get(session_id) else {
            return;
        };
        let (Some(card_box), Some(composer)) = (&chat.card_box, &chat.composer) else {
            return;
        };
        let from = card_box.input.read(cx).focus_handle(cx);
        let to = composer.input.read(cx).focus_handle(cx);
        cx.defer(move |cx| {
            for handle in cx.windows() {
                let moved = handle.update(cx, |_, window, cx| {
                    if from.is_focused(window) {
                        to.focus(window, cx);
                    }
                });
                if let Err(err) = moved {
                    tracing::debug!("moving the keyboard from a card box: {err:#}");
                }
            }
        });
    }

    /// A card key pressed in a pane showing session `session_id`'s chat,
    /// before any input there reads it. A digit while the card box has the
    /// keyboard is typed into it; the digit of the option that takes typed
    /// text presses that option, as its button does; any other key goes
    /// through [`Self::chat_key`]. Returns whether the key was taken.
    pub(crate) fn chat_pane_key(
        &mut self,
        session_id: &str,
        key: CardKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(chat) = self.chat.sessions.get(session_id) else {
            return false;
        };
        let CardKey::Digit(digit) = key else {
            return self.chat_key(session_id, key, cx);
        };
        if chat.card_box_has_keyboard(window, cx) {
            return false;
        }
        let typed = chat
            .stack
            .current()
            .filter(|prompt| typed_option(prompt) == Some(digit))
            .cloned();
        if let Some(prompt) = typed {
            let now = (self.now)();
            let working = chat.composer_state.as_ref() == Some(&ComposerState::Working);
            let text = chat.composer_text(cx);
            let action = key_action(&chat.stack, chat.card_busy(), key, &text, working, now);
            if matches!(action, KeyAction::Answer(_)) {
                self.press_card(session_id, &prompt, &ButtonAnswer::TypedText, window, cx);
                cx.stop_propagation();
                return true;
            }
        }
        self.chat_key(session_id, key, cx)
    }

    /// A key the chat of session `session_id` reads before its composer:
    /// it answers the current card, sends the composer's text, interrupts a
    /// running turn (dropping an armed one-click Stop first), or is
    /// swallowed; Ctrl+Enter does nothing while a send is in flight, and Esc
    /// interrupts once per screen reading. An Esc the chat would pass on
    /// drops a one-click arm (an armed Stop or draft Clear) when there is
    /// one, since the composer's own Esc binding keeps it from the window.
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
        let working = chat.composer_state.as_ref() == Some(&ComposerState::Working);
        let held = chat.interrupt_sent;
        match key_action(&chat.stack, chat.card_busy(), key, &text, working, now) {
            KeyAction::Answer(answer) => {
                if let Some(prompt) = current {
                    self.answer_card(session_id, &prompt, answer, None, cx);
                }
            }
            KeyAction::Send => self.send_composer(session_id, cx),
            KeyAction::Interrupt => {
                if self.disarm_escape_arms() {
                    cx.notify();
                } else if !held {
                    self.interrupt_turn(session_id);
                }
            }
            KeyAction::Swallow => {}
            // The composer binds Esc to its own action, so the window's Esc
            // never sees it: a one-click arm is dropped here instead.
            KeyAction::Pass => {
                if key != CardKey::Escape || !self.disarm_escape_arms() {
                    return false;
                }
                cx.notify();
            }
        }
        cx.stop_propagation();
        true
    }

    /// Sends Esc to the running turn of session `session_id` through its
    /// host, and holds Esc until the next screen reading so the agent's
    /// Rewind never opens on a second press.
    fn interrupt_turn(&mut self, session_id: &str) {
        let Some(chat) = self.chat.sessions.get_mut(session_id) else {
            return;
        };
        chat.interrupt_sent = true;
        let host = self.chat.host_of(session_id);
        self.send_to(
            &host,
            ClientMessage::SendInput {
                session_id: session_id.to_owned(),
                data_b64: B64.encode(b"\x1b"),
                automatic: false,
            },
        );
    }

    /// Sends `answer` to `prompt`, the card it was drawn or keyed for, under
    /// a new request id, unless that card is busy; a card gone since is
    /// answered all the same and the daemon refuses it. A toggle leaves the
    /// card free; any other answer makes it busy. `taken`, the card box's
    /// text the answer carries, is cleared from the box and kept by its
    /// request id to give back on a refusal.
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
            chat.boxes.took(request_id.clone(), &prompt.id, text);
            chat.set_card_box_text("", cx);
        }
        let host = self.chat.host_of(session_id);
        self.send_to(
            &host,
            card_answer_message(session_id, &prompt.id, answer, Some(request_id)),
        );
        cx.notify();
    }

    /// A button of session `session_id`'s card for `prompt` was pressed in
    /// `window`: a button that takes typed text sends the card box's text,
    /// waits while a chat send is in flight, and with the box blank gives it
    /// the keyboard instead.
    fn press_card(
        &mut self,
        session_id: &str,
        prompt: &ChatPrompt,
        answer: &ButtonAnswer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(chat) = self.chat.sessions.get(session_id) else {
            return;
        };
        let text = chat.card_box_text(cx);
        let Some(resolved) = answer.resolve(prompt, &text) else {
            if let Some(card_box) = chat.card_box.as_ref().filter(|_| answer.takes_text()) {
                card_box.input.read(cx).focus_handle(cx).focus(window, cx);
            }
            return;
        };
        if answer.takes_text() && chat.sends.in_flight((self.now)()) {
            return;
        }
        let taken = answer.takes_text().then_some(text);
        self.answer_card(session_id, prompt, resolved, taken, cx);
    }

    /// Sends `text` as session `session_id`'s chat message, tracked as the
    /// composer's Send is: its pending entry, a refusal giving it back. The
    /// composer's own text is left alone. A blank `text` and a send already
    /// in flight send nothing. Says whether the message went out.
    pub(crate) fn send_text(
        &mut self,
        session_id: &str,
        text: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let now = (self.now)();
        let request_id = new_request_id();
        let Some(chat) = self.chat.sessions.get_mut(session_id) else {
            return false;
        };
        if chat.sends.in_flight(now) {
            return false;
        }
        let Some(msg) = chat::send_message(session_id, text, request_id.clone()) else {
            return false;
        };
        chat.sends.sent(request_id, text.to_owned(), now);
        chat.clear_armed = false;
        chat.send_error = None;
        let host = self.chat.host_of(session_id);
        self.send_to(&host, msg);
        cx.notify();
        true
    }

    /// Sends session `session_id`'s composer text, clearing the composer;
    /// a refusal puts it back. Nothing happens while a send is in flight.
    pub(crate) fn send_composer(&mut self, session_id: &str, cx: &mut Context<Self>) {
        let Some(text) = self
            .chat
            .sessions
            .get(session_id)
            .map(|chat| chat.composer_text(cx))
        else {
            return;
        };
        if !self.send_text(session_id, &text, cx) {
            return;
        }
        if let Some(chat) = self.chat.sessions.get(session_id) {
            chat.set_composer_text("", cx);
        }
    }

    /// Asks session `session_id`'s host for the terminal prompt box's draft,
    /// which lands in the composer when it answers. Nothing happens while
    /// a pull is awaited.
    pub(crate) fn pull_draft(&mut self, session_id: &str, cx: &mut Context<Self>) {
        let request_id = new_request_id();
        let Some(chat) = self.chat.sessions.get_mut(session_id) else {
            return;
        };
        if chat.pull.is_pending() {
            return;
        }
        chat.pull.sent(request_id.clone());
        chat.clear_armed = false;
        chat.send_error = None;
        let host = self.chat.host_of(session_id);
        self.send_to(&host, chat::pull_draft_message(session_id, request_id));
        cx.notify();
    }

    /// Sends session `session_id`'s terminal prompt-box draft as it stands,
    /// tracked as a send with no text of ours. Nothing happens while a
    /// send is in flight.
    pub(crate) fn send_draft(&mut self, session_id: &str, cx: &mut Context<Self>) {
        let now = (self.now)();
        let request_id = new_request_id();
        let Some(chat) = self.chat.sessions.get_mut(session_id) else {
            return;
        };
        if chat.sends.in_flight(now) {
            return;
        }
        chat.sends.sent_draft(request_id.clone(), now);
        chat.clear_armed = false;
        chat.send_error = None;
        let host = self.chat.host_of(session_id);
        self.send_to(&host, chat::send_draft_message(session_id, request_id));
        cx.notify();
    }

    /// Asks session `session_id`'s host to clear the terminal prompt box's
    /// draft, tracked so a refusal shows under the composer; a clear that
    /// emptied the box is not answered.
    pub(crate) fn clear_draft(&mut self, session_id: &str, cx: &mut Context<Self>) {
        let request_id = new_request_id();
        let Some(chat) = self.chat.sessions.get_mut(session_id) else {
            return;
        };
        chat.clear.sent(request_id.clone());
        chat.clear_armed = false;
        chat.send_error = None;
        let host = self.chat.host_of(session_id);
        self.send_to(&host, chat::clear_draft_message(session_id, request_id));
        cx.notify();
    }

    /// Arms session `session_id`'s draft Clear, dropping any other
    /// session's, as pressing its Clear does.
    pub fn arm_draft_clear(&mut self, session_id: &str) {
        self.disarm_draft_clear();
        if let Some(chat) = self.chat.sessions.get_mut(session_id) {
            chat.clear_armed = true;
        }
    }

    /// Whether session `session_id`'s draft Clear is armed, its Confirm
    /// clear and Cancel showing.
    #[must_use]
    pub fn draft_clear_armed(&self, session_id: &str) -> bool {
        self.chat
            .sessions
            .get(session_id)
            .is_some_and(|chat| chat.clear_armed)
    }

    /// Drops an armed draft Clear whose composer line no longer offers
    /// Clear: the draft went, a pull or send took the row, or the host's
    /// connection dropped. Run after every network event, so the arm never
    /// shows again once its Clear went away.
    pub(crate) fn keep_draft_clear_arms(&mut self) {
        let gone: Vec<String> = self
            .chat
            .sessions
            .iter()
            .filter(|(session_id, chat)| {
                chat.clear_armed
                    && !self
                        .composer_line_of(session_id, chat)
                        .is_some_and(|line| line.offers_clear())
            })
            .map(|(session_id, _)| session_id.clone())
            .collect();
        for session_id in gone {
            if let Some(chat) = self.chat.sessions.get_mut(&session_id) {
                chat.clear_armed = false;
            }
        }
    }

    /// Drops every armed draft Clear; returns whether one was armed.
    pub(crate) fn disarm_draft_clear(&mut self) -> bool {
        let mut armed = false;
        for chat in self.chat.sessions.values_mut() {
            armed |= std::mem::take(&mut chat.clear_armed);
        }
        armed
    }

    /// Sends the agent's suggested next prompt `text` for session
    /// `session_id`, as the composer sends its own text but leaving that
    /// text alone. Nothing happens while a send is in flight.
    pub(crate) fn send_suggestion(&mut self, session_id: &str, text: &str, cx: &mut Context<Self>) {
        self.send_text(session_id, text, cx);
    }

    /// Puts the agent's suggested next prompt `text` into session
    /// `session_id`'s composer ahead of what is typed there, to edit; the
    /// daemon is not told.
    pub(crate) fn pull_suggestion(&mut self, session_id: &str, text: &str, cx: &mut Context<Self>) {
        if let Some(chat) = self.chat.sessions.get(session_id) {
            chat.give_back(text, cx);
            cx.notify();
        }
    }

    /// Types `bytes` into session `session_id` through its host, as the
    /// phone's Keys buttons and prompt sheet do.
    pub(crate) fn send_phone_input(&mut self, session_id: &str, bytes: &[u8]) {
        let host = self.chat.host_of(session_id);
        self.send_to(
            &host,
            ClientMessage::SendInput {
                session_id: session_id.to_owned(),
                data_b64: B64.encode(bytes),
                automatic: false,
            },
        );
    }

    /// Whether `chat`'s composer may send `text` now: not blank, no note
    /// that disables, no send in flight and no card up.
    fn send_enabled(&self, session_id: &str, chat: &SessionChat, text: &str) -> bool {
        let disabled = self
            .composer_note_of(session_id, chat)
            .as_ref()
            .is_some_and(ComposerNote::disables);
        !text.trim().is_empty()
            && !disabled
            && !chat.sends.in_flight((self.now)())
            && chat.stack.current().is_none()
    }

    /// The note `chat`'s composer shows: why it cannot send.
    fn composer_note_of(&self, session_id: &str, chat: &SessionChat) -> Option<ComposerNote> {
        let open = self
            .conn_of(&self.chat.host_of(session_id))
            .is_some_and(crate::connection::Connection::is_open);
        chat::composer_note(
            open,
            chat.composer_state.as_ref(),
            chat.sends.in_flight((self.now)()),
            chat.sends.owns_draft((self.now)()),
        )
    }

    /// The note row `chat`'s composer draws: its note, with the draft's
    /// Pull and Send, and Clear, on a host that offers them.
    fn composer_line_of(&self, session_id: &str, chat: &SessionChat) -> Option<ComposerLine> {
        let conn = self.conn_of(&self.chat.host_of(session_id));
        let draft_actions = conn.is_some_and(|conn| conn.supports(FEATURE_CHAT_DRAFT));
        let clear = conn.is_some_and(|conn| conn.supports(FEATURE_CHAT_CLEAR_DRAFT));
        chat::composer_line(
            self.composer_note_of(session_id, chat),
            draft_actions,
            chat.pull.is_pending(),
        )
        .map(|line| line.with_clear(clear))
    }

    /// The name people read for session `session_id`'s agent, for the
    /// labels that name it.
    fn agent_label(&self, session_id: &str) -> String {
        let agent = self
            .sidebar
            .session(session_id)
            .and_then(|session| session.agent.as_ref());
        self.sidebar
            .session_label_agents(session_id)
            .iter()
            .find(|descriptor| Some(&descriptor.id) == agent)
            .map_or_else(
                || GENERIC_AGENT.to_owned(),
                |descriptor| descriptor.label.clone(),
            )
    }

    /// Session `session_id`'s composer input, while its chat has one.
    #[must_use]
    pub(crate) fn phone_composer_input(&self, session_id: &str) -> Option<Entity<TextInput>> {
        let chat = self.chat.sessions.get(session_id)?;
        Some(chat.composer.as_ref()?.input.clone())
    }

    /// Whether session `session_id`'s composer may send now: not blank, no
    /// note that disables, no send in flight and no card up.
    #[must_use]
    pub fn phone_composer_send_enabled(&self, session_id: &str, cx: &App) -> bool {
        let Some(chat) = self.chat.sessions.get(session_id) else {
            return false;
        };
        let Some(composer) = chat.composer.as_ref() else {
            return false;
        };
        let text = composer.input.read(cx).text().to_owned();
        self.send_enabled(session_id, chat, &text)
    }

    /// Whether session `session_id`'s chat could send `text` now, as the
    /// composer's Send judges its own text: not blank, the host connected,
    /// no disabling note, no send in flight and no card up. A Quick reply is
    /// drawn live only while this holds for its own text.
    #[must_use]
    pub fn chat_send_enabled(&self, session_id: &str, text: &str) -> bool {
        let Some(chat) = self.chat.sessions.get(session_id) else {
            return false;
        };
        self.send_enabled(session_id, chat, text)
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

    /// The text in session `session_id`'s card box; none while its card
    /// has no box.
    #[must_use]
    pub fn chat_card_box_text(&self, session_id: &str, cx: &App) -> Option<String> {
        let chat = self.chat.sessions.get(session_id)?;
        chat.card_box.as_ref().map(|_| chat.card_box_text(cx))
    }

    /// The note session `session_id`'s composer shows, if any.
    #[must_use]
    pub fn chat_composer_note(&self, session_id: &str) -> Option<String> {
        self.chat_composer_line(session_id).map(|line| line.text())
    }

    /// The note row session `session_id`'s composer draws, with the
    /// draft's Pull and Send where the host offers them; none without a
    /// note.
    #[must_use]
    pub(crate) fn chat_composer_line(&self, session_id: &str) -> Option<ComposerLine> {
        let chat = self.chat.sessions.get(session_id)?;
        self.composer_line_of(session_id, chat)
    }

    /// The suggestion chip session `session_id`'s composer draws above its
    /// field, if any.
    #[must_use]
    pub(crate) fn chat_suggestion_chip(&self, session_id: &str) -> Option<SuggestionChip<'_>> {
        let chat = self.chat.sessions.get(session_id)?;
        self.suggestion_chip_of(chat)
    }

    /// The suggestion chip `chat`'s composer draws: one for the agent's
    /// suggested next prompt, while no card is up and no send is in flight.
    fn suggestion_chip_of<'a>(&self, chat: &'a SessionChat) -> Option<SuggestionChip<'a>> {
        chat::suggestion_chip(
            chat.composer_state.as_ref(),
            chat.stack.current().is_some(),
            chat.sends.in_flight((self.now)()),
        )
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

    /// The host session `session_id`'s chat is loaded from; none while it
    /// is not loaded (still loading or gone).
    pub(crate) fn loaded_chat_host(&self, session_id: &str) -> Option<HostKey> {
        let chat = self.chat.sessions.get(session_id)?;
        if chat.request.is_some() {
            return None;
        }
        self.chat.hosts.get(session_id).cloned()
    }

    /// Session `session_id`'s composer input, while its chat is loaded with
    /// one.
    pub(crate) fn chat_composer_input(&self, session_id: &str) -> Option<Entity<TextInput>> {
        let chat = self.chat.sessions.get(session_id)?;
        chat.composer
            .as_ref()
            .map(|composer| composer.input.clone())
    }

    /// Session `session_id`'s Attach button (`chat-attach`), beside Send:
    /// it asks this PC's file picker for a file to upload.
    fn attach_button(
        session_id: &str,
        enabled: bool,
        t: &UiTheme,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let id = session_id.to_owned();
        primary_button(
            fonts::ui_text_size(cx),
            crate::fonts::ui_weight_shift(cx),
            "chat-attach",
            ButtonSize::Compact,
            enabled,
            Layer::Base,
            t,
        )
        .child("Attach")
        .when(enabled, |attach| {
            attach.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.attach_to_chat(&id, cx);
            }))
        })
    }

    /// Attach: the file this PC's file picker returns uploads to session
    /// `session_id`'s host as a drop on its chat does. A cancelled pick does
    /// nothing.
    fn attach_to_chat(&mut self, session_id: &str, cx: &mut Context<Self>) {
        let picked = (self.pick_file)(cx, None, crate::FileFilter::new(ATTACH_FILTER, &[]));
        let id = session_id.to_owned();
        cx.spawn(async move |this, cx| {
            let Some(path) = picked.await else {
                tracing::debug!(session = %id, "chat attach: the pick was cancelled");
                return;
            };
            // Fails only when the root view is gone, and the chat with it.
            this.update(cx, |this, cx| this.drop_files_on_chat(&id, &[path], cx))
                .ok();
        })
        .detach();
    }

    /// The selectors of the buttons beside session `session_id`'s
    /// composer that send: Send. Attach (`chat-attach`) is not listed.
    #[must_use]
    pub fn chat_composer_actions(&self, session_id: &str) -> Vec<&'static str> {
        let Some(chat) = self.chat.sessions.get(session_id) else {
            return Vec::new();
        };
        if chat.composer.is_none() {
            return Vec::new();
        }
        vec!["chat-send"]
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
    pub fn toggle_chat_card(&mut self, session_id: &str, id: &str, cx: &mut Context<Self>) {
        if let Some(chat) = self.chat.sessions.get_mut(session_id) {
            chat.log.toggle_expanded(id);
            cx.notify();
        }
    }

    /// How chat panes draw tool calls.
    #[must_use]
    pub fn chat_mode(&self) -> ChatMode {
        self.sidebar.chat_mode()
    }

    /// The header's Compact toggle: switches every chat pane between
    /// Compact and Comfortable and saves the choice at once.
    pub fn toggle_chat_mode(&mut self, cx: &mut Context<Self>) {
        let mode = self.sidebar.chat_mode().toggled();
        if self.sidebar.set_chat_mode(mode) {
            self.save_ui();
        }
        cx.notify();
    }

    /// The rows a chat pane draws for session `session_id`'s chat in the
    /// current mode, as plain data; none while it is not loaded.
    #[must_use]
    pub fn chat_display_rows(&self, session_id: &str) -> Vec<RowShape> {
        self.chat
            .sessions
            .get(session_id)
            .map(|chat| {
                chat::display_rows(chat.log.entries(), self.sidebar.chat_mode())
                    .iter()
                    .map(DisplayRow::shape)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The ids of the answers in session `session_id`'s chat that draw as
    /// their turn's final reply, in id order; none while it is not loaded.
    #[must_use]
    pub fn chat_final_replies(&self, session_id: &str) -> Vec<String> {
        self.chat
            .sessions
            .get(session_id)
            .map(|chat| {
                chat::final_replies(chat.log.entries())
                    .into_iter()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Whether the Compact run that starts at call `first_id` of session
    /// `session_id`'s chat is open.
    #[must_use]
    pub fn chat_run_open(&self, session_id: &str, first_id: &str) -> bool {
        self.chat
            .sessions
            .get(session_id)
            .is_some_and(|chat| chat.log.is_run_open(first_id))
    }

    /// A click on a Compact run's line opens it to its cards, or closes it;
    /// every pane on the session draws it so.
    pub fn toggle_chat_run(&mut self, session_id: &str, first_id: &str, cx: &mut Context<Self>) {
        if let Some(chat) = self.chat.sessions.get_mut(session_id) {
            chat.log.toggle_run(first_id);
            cx.notify();
        }
    }

    /// The command statements tool card `tool_id` of session `session_id`'s
    /// chat lists, one per line, while it is open; none while it is folded
    /// or its tool runs no shell command.
    #[must_use]
    pub fn chat_tool_lines(&self, session_id: &str, tool_id: &str) -> Vec<String> {
        let Some(chat) = self.chat.sessions.get(session_id) else {
            return Vec::new();
        };
        if !chat.log.is_expanded(tool_id) {
            return Vec::new();
        }
        match chat.log.get(tool_id) {
            Some(ChatEntry::Tool {
                name,
                summary,
                input,
                ..
            }) => shell_lines::tool_statements(name, summary, input),
            _ => Vec::new(),
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

    /// The file links answer `answer_id` of session `session_id`'s desktop
    /// chat draws as links, in order: each one's path as the answer wrote it
    /// and its `#fragment` (lowercased, as a click passes it).
    #[must_use]
    pub fn chat_answer_file_links(
        &self,
        session_id: &str,
        answer_id: &str,
    ) -> Vec<(String, Option<String>)> {
        self.answer_doc(session_id, answer_id)
            .map(MdDoc::file_links)
            .unwrap_or_default()
    }

    /// Opens a file link tapped in session `session_id`'s phone chat as a
    /// tapped path: resolved as the desktop chat resolves it, then the host
    /// says whether it names a file, which the viewer opens, or a folder,
    /// which the browser opens. A `#fragment` is not passed.
    pub fn phone_chat_file_link(
        &mut self,
        session_id: &str,
        path: &std::path::Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let link = crate::link_menu_view::ChatFileLink {
            path,
            anchor: None,
            menu: false,
        };
        self.chat_file_link(session_id, link, window, cx);
    }

    /// Raises issue `identifier`, whose web address is `web_url`, tapped in
    /// session `session_id`'s phone chat: its host is asked for the issue
    /// once and its card shows as the phone's sheet, or, on a host that
    /// cannot answer, the link opens.
    pub fn phone_chat_issue_link(
        &mut self,
        session_id: &str,
        identifier: &str,
        web_url: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let host = self.chat.host_of(session_id);
        self.phone_raise_card(&host, identifier, web_url, window, cx);
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
                    .driver_id(|| name)
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
        let cap = self.sidebar.chat_max_width();
        let look = RowLook {
            max_width: (cap > 0).then(|| u16::try_from(cap).map_or(f32::from(u16::MAX), f32::from)),
            finals: chat
                .map(|chat| chat::final_replies(chat.log.entries()))
                .unwrap_or_default(),
            ..self.row_look(pane_id, session_id, cx)
        };
        let (main, strip) = self.rows_and_strip(&look, cx);
        let loaded = chat.filter(|chat| chat.request.is_none());
        let cards = loaded.and_then(|chat| self.card_area(session_id, chat, cx));
        let composer = loaded.and_then(|chat| self.composer_row(session_id, chat, cx));
        let keys_for = session_id.to_owned();
        let drop_for = session_id.to_owned();
        div()
            .driver_id(|| name)
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .size_full()
            // A pop-out's chat takes its own drops; in the main window the
            // grid's drop layer takes them first and `drop_files` sends a
            // chat pane's to the composer.
            .on_drop(
                cx.listener(move |this, paths: &gpui::ExternalPaths, _, cx| {
                    this.drop_files_on_chat(&drop_for, paths.paths(), cx);
                }),
            )
            .capture_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                if let Some(key) = card_key(&event.keystroke)
                    && this.chat_pane_key(&keys_for, key, window, cx)
                {
                    cx.stop_propagation();
                }
            }))
            .child(main)
            .children(cards)
            .children(strip)
            .children(composer)
            .into_any_element()
    }

    /// The look's session's chat as `look` draws it: a muted line while it
    /// loads or has no rows, else its rows in a column that follows the
    /// bottom until scrolled up (with the look's pane's scroll); and the
    /// running-work strip while work runs.
    fn rows_and_strip(
        &self,
        look: &RowLook<'_>,
        cx: &mut Context<Self>,
    ) -> (AnyElement, Option<AnyElement>) {
        let chat = self.chat.sessions.get(look.session_id);
        let strip = chat
            .and_then(|chat| chat.work.strip(self.wall_now()))
            .map(|strip| {
                strip_row(look.text_offset, &strip, &look.mono, &look.theme).into_any_element()
            });
        let rows = chat.filter(|chat| !chat.log.is_empty() || chat.log.hidden() > 0);
        let main = if let Some(chat) = rows {
            chat_column(look, chat, self.chat.scrolls.get(look.pane_id), cx)
        } else {
            let loading = chat.is_none_or(|chat| chat.request.is_some());
            let note = if loading { LOADING_NOTE } else { EMPTY_NOTE };
            chat_note(look.text_offset, note, &look.theme).into_any_element()
        };
        (main, strip)
    }

    /// Session `session_id`'s chat as the phone's session page draws it,
    /// tagged `phone-chat`: its rows (Compact, at the page's full width,
    /// with no final-reply box, folding only runs of four or more calls) or
    /// the loading or empty note, over the running-work strip while work
    /// runs. A release anywhere in it reaches `on_up` first, in the capture
    /// phase, before the row, link or button the tap hit acts, which is how
    /// the phone puts the keyboard away whatever a child does with the click.
    /// `hold`, an area over the rows (not the strip), takes their long
    /// presses; `handles` are the selection handles' drag areas, drawn over
    /// the rows while the chat has a selection
    /// ([`Self::phone_handle_layer`]).
    pub(crate) fn phone_chat_body(
        &self,
        session_id: &str,
        on_up: impl Fn(&MouseUpEvent, &mut Window, &mut App) + 'static,
        hold: impl IntoElement,
        handles: HandleAreas,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let renders = &self.chat.phone_chat_renders;
        renders.set(renders.get() + 1);
        let select = self.phone_select_draw(session_id, cx);
        let ends = select.as_ref().and_then(|select| select.ends.clone());
        let look = RowLook {
            mode: ChatMode::Compact,
            max_width: None,
            finals: BTreeSet::new(),
            min_run: PHONE_MIN_RUN,
            select: select.as_ref(),
            link_route: LinkRoute::Phone,
            ..self.row_look(PHONE_PANE, session_id, cx)
        };
        let (main, strip) = match &self.chat.phone_refused {
            Some(note) => {
                let note = chat_note(look.text_offset, note.clone(), &look.theme)
                    .driver_id(|| "phone-chat-note".to_owned())
                    .into_any_element();
                (note, None)
            }
            None => self.rows_and_strip(&look, cx),
        };
        div()
            .id(ElementId::Name(SharedString::from("phone-chat")))
            .driver_id(|| "phone-chat".to_owned())
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .size_full()
            .capture_any_mouse_up(on_up)
            .child(
                // The hold covers the rows only: the strip below keeps its
                // own long press, which shows a segment's tooltip.
                div()
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.0))
                    .w_full()
                    .child(main)
                    .child(hold)
                    .child(self.phone_handle_layer(session_id, ends, handles, cx)),
            )
            .children(strip)
            .child(painted_mark(self.chat.phone_text.clone()))
            .into_any_element()
    }

    /// The selection handles over the phone chat's rows: one at each of
    /// the selection's ordered `ends` whose caret the previous frame drew
    /// inside the chat's scroll area, and the handle a finger drags wherever
    /// its caret is (so its drag goes on), each holding its drag area from
    /// `areas`, then the canvas that measures where they go this frame
    /// ([`handle_measure`]). While a handle draws, so does the Copy button
    /// beside them ([`copy_button_origin`]), which copies session
    /// `session_id`'s selection. No selection draws no handle. The layer is
    /// clipped to the rows' box, so no handle reaches over the work strip,
    /// the composer or the header, nor takes a touch there.
    fn phone_handle_layer(
        &self,
        session_id: &str,
        ends: Option<(TextPos, TextPos)>,
        areas: HandleAreas,
        cx: &mut Context<Self>,
    ) -> Div {
        let drawn = ends
            .as_ref()
            .and_then(|_| self.chat.phone_text.borrow().handles);
        let color: Hsla = gpui::rgb(theme(cx).accent).into();
        let mut layer = Vec::new();
        for (end, area) in [(HandleEnd::Start, areas.start), (HandleEnd::End, areas.end)] {
            if let Some(spot) = drawn.and_then(|spots| spots.of(end)) {
                layer.push(handle_element(end, spot, color, area));
            }
        }
        // An empty selection keeps its handles but has nothing to copy.
        if ends.as_ref().is_some_and(|(start, end)| start != end) {
            let view = self.phone_viewport();
            layer.extend(Self::phone_copy_button(session_id, drawn, view, cx));
        }
        let viewport = self
            .chat
            .scrolls
            .get(PHONE_PANE)
            .map(|scroll| scroll.handle().clone());
        let registry = self.chat.phone_text.clone();
        let dragged = self
            .chat
            .sessions
            .get(session_id)
            .and_then(|chat| chat.handle_drag.as_ref())
            .map(|drag| drag.end);
        let measure = handle_measure(registry, ends, viewport, drawn, dragged);
        layer.push(measure.into_any_element());
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .overflow_hidden()
            .children(layer)
    }

    /// The Copy button of session `session_id`'s phone chat, placed from
    /// the handles `drawn`; none while no handle draws. With one end's
    /// handle off the chat, the button sits by the other's.
    fn phone_copy_button(
        session_id: &str,
        drawn: Option<HandleSpots>,
        view: Option<Bounds<Pixels>>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let spots = drawn?;
        let (start, end) = copy_spots(spots, view)?;
        let measured = copy_button_size(cx);
        let origin = copy_button_origin(start, end, spots.layer, measured.0);
        let root = cx.entity().downgrade();
        let id = session_id.to_owned();
        let on_copy = move |_: &mut Window, cx: &mut App| {
            // Fails only when the app's root is gone.
            root.update(cx, |root, cx| root.phone_copy_selection(&id, cx))
                .ok();
        };
        Some(copy_button(origin, measured, &theme(cx), on_copy))
    }

    /// What the phone draws session `session_id`'s selectable texts with,
    /// while its chat is loaded: its selection moved into the texts it now
    /// holds, in order; the registry is emptied first, for this frame's
    /// texts.
    fn phone_select_draw(&self, session_id: &str, cx: &App) -> Option<SelectDraw> {
        self.chat.phone_text.borrow_mut().forget();
        let chat = self.chat.sessions.get(session_id)?;
        let units = phone_units(chat);
        let ends = chat
            .selection
            .as_ref()
            .and_then(|selection| clamped(&units, selection))
            .and_then(|selection| selection.ordered(&units));
        let selected = ends.as_ref().and_then(|ordered| units.resolve(ordered));
        Some(SelectDraw {
            units: Rc::new(units),
            selected,
            ends,
            background: gpui::rgba(theme(cx).input_selection).into(),
            registry: self.chat.phone_text.clone(),
        })
    }

    /// Session `session_id`'s prompt card area as the desktop draws it
    /// ([`Self::card_area`]), while its chat is loaded; the phone's prompt
    /// sheet holds it.
    pub(crate) fn phone_card_area(
        &self,
        session_id: &str,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let chat = self
            .chat
            .sessions
            .get(session_id)
            .filter(|chat| chat.request.is_none())?;
        self.card_area_on(session_id, chat, LinkRoute::Phone, cx)
    }

    /// The id of session `session_id`'s current prompt card, only while its
    /// chat is loaded: the card [`Self::phone_card_area`] draws.
    pub(crate) fn phone_current_prompt(&self, session_id: &str) -> Option<&str> {
        self.chat
            .sessions
            .get(session_id)
            .filter(|chat| chat.request.is_none())?
            .stack
            .current()
            .map(|prompt| prompt.id.as_str())
    }

    /// What every row pane `pane_id` draws for session `session_id` is
    /// drawn with: the terminal's font for code, the UI text size and the
    /// theme, with no width cap and no final replies (the chat pane adds
    /// both).
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
            can_tail: self
                .conn_of(&self.chat.host_of(session_id))
                .is_some_and(|conn| conn.supports(FEATURE_FILE_TAIL)),
            tails: &self.chat.tails,
            tail_scrolls: &self.chat.tail_scrolls,
            mode: self.sidebar.chat_mode(),
            max_width: None,
            finals: BTreeSet::new(),
            min_run: 1,
            select: None,
            link_route: LinkRoute::Chat,
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
        self.card_area_on(session_id, chat, LinkRoute::Chat, cx)
    }

    /// [`Self::card_area`] whose card's links follow `route`: the desktop
    /// chat's, or the phone's.
    fn card_area_on(
        &self,
        session_id: &str,
        chat: &SessionChat,
        route: LinkRoute,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (offset, t) = (fonts::ui_text_size(cx), theme(cx));
        let elsewhere = chat.cards.answered_elsewhere().then(|| {
            muted_line(offset, ANSWERED_ELSEWHERE.to_owned(), &t)
                .driver_id(|| "chat-answered-elsewhere".to_owned())
        });
        let agent = self.agent_label(session_id);
        let card = chat.stack.current().map(|prompt| {
            let view = CardView {
                prompt,
                agent: &agent,
                busy: chat.cards.is_busy(&prompt.id),
                refusal: chat.cards.refusal(&prompt.id),
                ghosts: chat.stack.ghosts().len(),
                text_box: chat.card_box.as_ref().map(|card_box| &card_box.input),
                links: CardLinks { session_id, route },
            };
            let id = session_id.to_owned();
            let drawn = prompt.clone();
            let on_press: OnPress =
                Rc::new(cx.listener(move |this, answer: &ButtonAnswer, window, cx| {
                    this.press_card(&id, &drawn, answer, window, cx);
                }));
            prompt_card(
                offset,
                crate::fonts::ui_weight_shift(cx),
                &view,
                &on_press,
                &t,
            )
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
    /// send (with the draft's Pull and Send where the host offers them),
    /// the agent's suggested next prompt as a chip, the input (tagged
    /// `chat-composer`) with Send (`chat-send`), disabled while a card is
    /// up since a card's typed answers go in its own box, the Quick replies
    /// under it (`chat-quick`), and the daemon's reason for refusing the
    /// last send (`chat-composer-error`). Without a connection the input is
    /// disabled.
    pub(crate) fn composer_row(
        &self,
        session_id: &str,
        chat: &SessionChat,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let composer = chat.composer.as_ref()?;
        let (offset, t) = (fonts::ui_text_size(cx), theme(cx));
        let line = self.composer_line_of(session_id, chat);
        let disabled = composer_line_disables(line.as_ref());
        let armed = chat.clear_armed;
        let note = line.map(|line| Self::composer_note_row(session_id, &line, armed, cx));
        let chip = self
            .suggestion_chip_of(chat)
            .map(|chip| Self::suggestion_chip_row(session_id, &chip, !disabled, cx));
        let text = composer.input.read(cx).text().to_owned();
        let field = field_frame(false, Layer::Base, &t)
            .driver_id(|| "chat-composer".to_owned())
            .flex_1()
            .min_w(px(0.0))
            .py(px(6.0))
            .when(disabled, |field| field.opacity(DISABLED_OPACITY))
            .when(!disabled, |field| field.child(composer.input.clone()));
        let enabled = self.send_enabled(session_id, chat, &text);
        let quick = self.quick_row(session_id, chat, cx);
        let popup = (!disabled)
            .then(|| Self::completion_popup(session_id, composer, cx))
            .flatten();
        let id = session_id.to_owned();
        let send = primary_button(
            offset,
            crate::fonts::ui_weight_shift(cx),
            "chat-send",
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
        });
        let attach = Self::attach_button(session_id, !disabled, &t, cx);
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
                .children(note)
                .children(chip)
                .children(popup)
                .child(
                    div()
                        .flex()
                        .items_end()
                        .gap(px(8.0))
                        .child(field)
                        .child(attach)
                        .child(send),
                )
                .children(quick)
                .children(chat.send_error.clone().map(|reason| {
                    div()
                        .driver_id(|| "chat-composer-error".to_owned())
                        .text_size(ui_px_at(offset, SMALL_TEXT_SIZE))
                        .text_color(gpui::rgb(t.danger))
                        .child(reason)
                }))
                .into_any_element(),
        )
    }

    /// Session `session_id`'s Quick reply row under the composer field
    /// (`chat-quick`): the model pill (`chat-model`) first, a button sending
    /// the agent's model switch command when the agent names one and
    /// read-only otherwise, then the agent
    /// reply set's groups in order, a gap between groups, every reply a chip
    /// (`chat-quick-{group}-{reply}`) sending its own text as a chat message
    /// and leaving the composer's text alone. A chip is live whenever
    /// `chat`'s composer could send that reply's text: no disabling note, no
    /// send in flight and no card up. None when neither a model nor a reply
    /// is known.
    fn quick_row(
        &self,
        session_id: &str,
        chat: &SessionChat,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (offset, t) = (fonts::ui_text_size(cx), theme(cx));
        let shift = crate::fonts::ui_weight_shift(cx);
        let saved = &self.sidebar.ui_state().phone_agent_replies;
        let groups = quick_replies::groups(saved.as_deref(), ReplySet::Agent);
        let pill = chat.pill_text().map(|label| match &chat.switch_command {
            Some(command) => {
                let id = session_id.to_owned();
                let command = command.clone();
                let enabled = self.send_enabled(session_id, chat, &command);
                outlined_button(
                    offset,
                    shift,
                    "chat-model",
                    ButtonSize::Quick,
                    enabled,
                    Layer::Base,
                    &t,
                )
                .child(label)
                .when(enabled, |pill| {
                    pill.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.send_text(&id, &command, cx);
                    }))
                })
                .into_any_element()
            }
            None => model_pill(offset, &label, &t),
        });
        if groups.iter().all(|group| group.replies.is_empty()) && pill.is_none() {
            return None;
        }
        let mut row = div()
            .driver_id(|| "chat-quick".to_owned())
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(QUICK_GROUP_GAP))
            .children(pill);
        for (group_index, group) in groups
            .iter()
            .enumerate()
            .filter(|(_, group)| !group.replies.is_empty())
        {
            let mut section = div()
                .driver_id(move || format!("chat-quick-group-{group_index}"))
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(QUICK_CHIP_GAP));
            for (reply_index, text) in group.replies.iter().enumerate() {
                let id = session_id.to_owned();
                let reply = text.clone();
                let enabled = self.send_enabled(session_id, chat, text);
                let selector = format!("chat-quick-{group_index}-{reply_index}");
                section = section.child(
                    outlined_button(
                        offset,
                        shift,
                        &selector,
                        ButtonSize::Quick,
                        enabled,
                        Layer::Base,
                        &t,
                    )
                    .child(text.clone())
                    .when(enabled, |chip| {
                        chip.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.send_text(&id, &reply, cx);
                        }))
                    }),
                );
            }
            row = row.child(section);
        }
        Some(row.into_any_element())
    }

    /// The composer's note row (`chat-composer-note`): `line`'s text, and
    /// for a draft the host can pull or send, Pull (`chat-draft-pull`) and
    /// Send (`chat-draft-send`) beside it, then the draft's Clear where the
    /// host offers it (see [`Self::draft_clear_controls`]; `armed` once
    /// pressed).
    fn composer_note_row(
        session_id: &str,
        line: &ComposerLine,
        armed: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (offset, t) = (fonts::ui_text_size(cx), theme(cx));
        if !line.has_draft_actions() {
            return muted_line(offset, line.text(), &t)
                .driver_id(|| "chat-composer-note".to_owned())
                .into_any_element();
        }
        // One line beside the buttons, cut with `…` when the row is narrow.
        let note = div()
            .flex_1()
            .min_w(px(0.0))
            .text_size(ui_px_at(offset, SMALL_TEXT_SIZE))
            .text_color(gpui::rgb(t.muted))
            .child(crate::ellipsis::truncating(
                "chat-composer-note",
                gpui::StyledText::new(line.text()),
            ))
            .driver_id(|| "chat-composer-note".to_owned());
        let shift = crate::fonts::ui_weight_shift(cx);
        let (pull_id, send_id) = (session_id.to_owned(), session_id.to_owned());
        let pull = outlined_button(
            offset,
            shift,
            "chat-draft-pull",
            ButtonSize::Compact,
            true,
            Layer::Base,
            &t,
        )
        .child("Pull")
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.pull_draft(&pull_id, cx);
        }));
        let send = outlined_button(
            offset,
            shift,
            "chat-draft-send",
            ButtonSize::Compact,
            true,
            Layer::Base,
            &t,
        )
        .child("Send")
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.send_draft(&send_id, cx);
        }));
        let clear = if line.offers_clear() {
            Self::draft_clear_controls(session_id, armed, cx)
        } else {
            Vec::new()
        };
        div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(note)
            .child(pull)
            .child(send)
            .children(clear)
            .into_any_element()
    }

    /// The draft's Clear (`chat-draft-clear`), or once pressed (`armed`) its
    /// Confirm clear (`chat-draft-clear-confirm`) in the danger colour and
    /// Cancel (`chat-draft-clear-cancel`). They act on the press, which then
    /// goes no further, so the window's press elsewhere does not disarm them.
    fn draft_clear_controls(
        session_id: &str,
        armed: bool,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let (offset, t) = (fonts::ui_text_size(cx), theme(cx));
        let shift = crate::fonts::ui_weight_shift(cx);
        let compact = |selector: &str| {
            outlined_button(
                offset,
                shift,
                selector,
                ButtonSize::Compact,
                true,
                Layer::Base,
                &t,
            )
        };
        let id = session_id.to_owned();
        if !armed {
            let clear = compact("chat-draft-clear").child("Clear");
            return vec![
                on_press(clear, cx, move |this, _, _| this.arm_draft_clear(&id)).into_any_element(),
            ];
        }
        let confirm = compact("chat-draft-clear-confirm")
            .text_color(gpui::rgb(t.danger))
            .child("Confirm clear");
        let cancel = compact("chat-draft-clear-cancel").child("Cancel");
        vec![
            on_press(confirm, cx, move |this, _, cx| this.clear_draft(&id, cx)).into_any_element(),
            on_press(cancel, cx, |this, _, _| {
                this.disarm_draft_clear();
            })
            .into_any_element(),
        ]
    }

    /// The agent's suggested next prompt above the composer
    /// (`chat-suggestion`): its text, and unless it was cut, Send
    /// (`chat-suggestion-send`) and Pull (`chat-suggestion-pull`), live
    /// while `enabled`.
    fn suggestion_chip_row(
        session_id: &str,
        chip: &SuggestionChip<'_>,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (offset, t) = (fonts::ui_text_size(cx), theme(cx));
        let shift = crate::fonts::ui_weight_shift(cx);
        let text = muted_line(offset, chip.text.to_owned(), &t)
            .flex_1()
            .min_w(px(0.0));
        let row = field_frame(false, Layer::Base, &t)
            .driver_id(|| "chat-suggestion".to_owned())
            .flex()
            .items_center()
            .gap(px(8.0))
            .py(px(4.0))
            .child(text);
        if !chip.actions {
            return row.into_any_element();
        }
        let button = |selector: &str, label: &'static str| {
            outlined_button(
                offset,
                shift,
                selector,
                ButtonSize::Compact,
                enabled,
                Layer::Base,
                &t,
            )
            .child(label)
        };
        let (send_id, pull_id) = (session_id.to_owned(), session_id.to_owned());
        let (send_text, pull_text) = (chip.text.to_owned(), chip.text.to_owned());
        let send = button("chat-suggestion-send", "Send").when(enabled, |send| {
            send.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.send_suggestion(&send_id, &send_text, cx);
            }))
        });
        let pull = button("chat-suggestion-pull", "Pull").when(enabled, |pull| {
            pull.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.pull_suggestion(&pull_id, &pull_text, cx);
            }))
        });
        row.child(send).child(pull).into_any_element()
    }
}

/// How a disabled composer is dimmed.
const DISABLED_OPACITY: f32 = 0.5;

/// The gap between the composer's Quick reply groups, and between the chips
/// of one group.
const QUICK_GROUP_GAP: f32 = 8.0;
const QUICK_CHIP_GAP: f32 = 4.0;

/// The model pill's least height, side padding and label size, in px: a
/// Quick reply chip's, which is a quick button's.
const MODEL_PILL_HEIGHT: f32 = 28.0;
const MODEL_PILL_PAD_X: f32 = 7.0;
const MODEL_PILL_TEXT_SIZE: f32 = 11.0;

/// The chat's model pill: its label, read-only, muted and chip-sized, at
/// the start of the composer's Quick replies line.
fn model_pill(offset: i8, label: &str, t: &UiTheme) -> AnyElement {
    div()
        .driver_id(|| "chat-model".to_owned())
        .flex_none()
        .flex()
        .items_center()
        .h(ui_px_at(offset, MODEL_PILL_HEIGHT))
        .px(px(MODEL_PILL_PAD_X))
        .rounded(px(ButtonSize::Quick.radius()))
        .border_1()
        .border_color(gpui::rgb(t.line_strong))
        .text_size(ui_px_at(offset, MODEL_PILL_TEXT_SIZE))
        .text_color(gpui::rgb(t.muted))
        .child(label.to_owned())
        .into_any_element()
}

/// Whether the composer's note row `line` disables the composer, and with
/// it the suggestion chip's Send and Pull.
pub(crate) fn composer_line_disables(line: Option<&ComposerLine>) -> bool {
    matches!(line, Some(ComposerLine::Note(note)) if note.disables())
}

/// The soft keyboard the phone's composer asks for: Send as its action key,
/// with autocorrect and sentence autocapitalization on. Word suggestions
/// stay off: the platform's inline predictions arrive as marked text, which
/// a Send could carry.
fn phone_composer_configuration() -> TextInputConfiguration {
    TextInputConfiguration {
        autocorrect: true,
        suggestions: false,
        autocapitalize: Autocapitalize::Sentences,
        input_action: TextInputAction::Send,
        keyboard: KeyboardKind::Text,
    }
}

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

/// A loaded chat's rows in a column (tagged `chat-column-<pane>`) at most
/// the look's width cap wide and centred in a wider pane, that follows the
/// bottom until scrolled up, with its scrollbar at the pane's edge.
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
    for row in chat::display_rows_folding(chat.log.entries(), look.mode, look.min_run) {
        let drawn = match row {
            DisplayRow::Single(entry) => chat_row(look, chat, entry, CHAT_ROW, cx),
            DisplayRow::Script(script) => script_row(look, chat, &script, cx),
            DisplayRow::Run { entries, summary } => run_row(look, chat, &entries, &summary, cx),
        };
        rows.push(drawn);
    }
    let name = format!("chat-column-{}", look.pane_id);
    let rows = div()
        .driver_id(|| name)
        .w_full()
        .when_some(look.max_width, |rows, cap| rows.max_w(px(cap)))
        .mx_auto()
        .px(px(BODY_PAD_X))
        .py(px(BODY_PAD_Y))
        .flex()
        .flex_col()
        .gap(px(ROW_GAP))
        .children(rows);
    let column = div()
        .id(ElementId::Name(SharedString::from(format!(
            "chat-scroll-{}",
            look.pane_id
        ))))
        .size_full()
        .overflow_y_scroll()
        .text_size(ui_px_at(look.text_offset, UI_TEXT_SIZE))
        .text_color(gpui::rgb(look.theme.text))
        .child(rows);
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
            handle.max_offset().y / px(1.0),
        )
    });
    if chat::follows_bottom(laid_out) {
        handle.scroll_to_bottom();
    }
}

/// How far inside the phone chat's top or bottom edge, in pixels, a
/// selection drag's finger scrolls the chat.
const EDGE_BAND: f32 = 48.0;
/// The edge scroll's top speed, in pixels a second, with the finger at the
/// edge or past it.
const EDGE_SPEED: f32 = 600.0;

/// How far, in pixels, the phone chat scrolls over `dt` seconds while a
/// selection drag's finger is at window height `y` over a scroll area from
/// `top` to `bottom`: nothing more than [`EDGE_BAND`] inside both edges;
/// inside a band, [`EDGE_SPEED`] scaled by how deep in the band the finger
/// is (all of it at the edge and past it), negative toward earlier rows
/// near the top and positive toward later rows near the bottom.
pub(crate) fn edge_step(y: f32, top: f32, bottom: f32, dt: f32) -> f32 {
    let into_top = (EDGE_BAND - (y - top)).clamp(0.0, EDGE_BAND);
    let into_bottom = (EDGE_BAND - (bottom - y)).clamp(0.0, EDGE_BAND);
    (into_bottom - into_top) * EDGE_SPEED / EDGE_BAND * dt
}

/// The least step up, in pixels, from a chat at its bottom: one more than
/// the slack within which [`chat::follows_bottom`] keeps a chat at its
/// bottom, so the next layout does not snap the chat back.
const LEAVE_BOTTOM: f32 = 2.0;

/// The edge scroll's `step` from a chat scrolled `scrolled` of its
/// `max_scroll` pixels: a step up from a chat at its bottom is at least
/// [`LEAVE_BOTTOM`]; any other step stands.
fn leave_bottom(step: f32, scrolled: f32, max_scroll: f32) -> f32 {
    if step < 0.0 && chat::follows_bottom(Some((scrolled, max_scroll))) {
        step.min(-LEAVE_BOTTOM)
    } else {
        step
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

/// The chat pane header's Compact toggle's tooltip.
pub(crate) const COMPACT_TIP: &str =
    "Fold runs of read, search and run calls and Code Mode scripts into one line";

/// The chat pane header's Compact toggle, `base` being the header control it
/// is drawn on (tagged `chat-compact-<pane>`, no hover look of its own):
/// drawn on (accent fill, on-accent text) while Compact is the mode, and
/// still on under the pointer; off, it hovers as the header's buttons do. A
/// click switches the mode and saves it.
pub(crate) fn compact_toggle(
    base: gpui::Stateful<Div>,
    mode: ChatMode,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let t = theme(cx);
    let on = mode == ChatMode::Compact;
    base.w_auto()
        .px(px(8.0))
        .when(on, |toggle| {
            toggle
                .bg(gpui::rgb(t.accent))
                .text_color(gpui::rgb(t.on_accent))
        })
        .hover(move |style| {
            if on {
                style
                    .bg(gpui::rgb(t.accent))
                    .text_color(gpui::rgb(t.on_accent))
            } else {
                style.bg(gpui::rgb(t.hover)).text_color(gpui::rgb(t.text))
            }
        })
        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
            this.toggle_chat_mode(cx);
        }))
        .into_any_element()
}

/// A script's cards under its line: open, the script call's card, then each
/// inner call's card, in order; closed, only the cards of its inner calls
/// that tail files (`ScriptRow::shown_inner`).
fn script_cards(
    look: &RowLook<'_>,
    chat: &SessionChat,
    script: &ScriptRow<'_>,
    open: bool,
    cx: &mut Context<RootView>,
) -> Vec<AnyElement> {
    open.then_some(script.call)
        .into_iter()
        .chain(script.shown_inner(open))
        .map(|entry| chat_row(look, chat, entry, CHAT_ROW, cx))
        .collect()
}

/// A Code Mode script as one line (tagged `chat-script-<pane>-<id>`), a
/// failed script's error text under it, and its cards (`script_cards`, all
/// of them while the script call's card is open) under those, all in a
/// column tagged `chat-script-row-<pane>-<id>`. A click on the line opens
/// or closes it.
fn script_row(
    look: &RowLook<'_>,
    chat: &SessionChat,
    script: &ScriptRow<'_>,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let Some(id) = chat::entry_id(script.call) else {
        return div().into_any_element();
    };
    let mut line = chat::script_line(script);
    let error = line.error.take().map(|text| script_error(look, id, text));
    let cards = script_cards(look, chat, script, chat.log.is_expanded(id), cx);
    let name = format!("chat-script-row-{}-{id}", look.pane_id);
    div()
        .driver_id(|| name)
        .flex()
        .flex_col()
        .gap(px(ROW_GAP))
        .child(script_line_row(look, id, line, cx))
        .children(error)
        .children(cards)
        .into_any_element()
}

/// A script's line: the λ mark, its state mark (tagged
/// `chat-script-<running|done|failed>-<pane>-<id>`), its description, its
/// number and time (tagged `chat-script-meta-<pane>-<id>`) and its chips,
/// wrapping onto further lines (the meta and each chip whole) when the row
/// is too narrow; a click toggles the script call's card open.
fn script_line_row(
    look: &RowLook<'_>,
    id: &str,
    line: chat::ScriptLine,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let t = &look.theme;
    let pane = look.pane_id;
    let hover_text = t.text;
    let (state, mark, color) = match line.state {
        chat::ScriptState::Running => ("running", "\u{2026}", t.working),
        chat::ScriptState::Done => ("done", "\u{2713}", t.waiting),
        chat::ScriptState::Failed => ("failed", "\u{2715}", t.danger),
    };
    let name = format!("chat-script-{pane}-{id}");
    let mark_name = format!("chat-script-{state}-{pane}-{id}");
    let meta = line.meta.map(|text| {
        let meta_name = format!("chat-script-meta-{pane}-{id}");
        div().driver_id(|| meta_name).flex_none().child(text)
    });
    let chips: Vec<Div> = line
        .chips
        .into_iter()
        .enumerate()
        .map(|(n, chip)| script_chip(look, id, n, chip))
        .collect();
    let (session_id, call_id) = (look.session_id.to_owned(), id.to_owned());
    div()
        .id(ElementId::Name(SharedString::from(name.clone())))
        .driver_id(|| name)
        .flex()
        .flex_wrap()
        .items_center()
        .gap_x(px(8.0))
        .gap_y(px(2.0))
        .text_color(gpui::rgb(t.muted))
        .cursor_pointer()
        .hover(move |style| style.text_color(gpui::rgb(hover_text)))
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.toggle_chat_card(&session_id, &call_id, cx);
        }))
        .child(
            div()
                .flex_none()
                .text_color(gpui::rgb(t.lilac))
                .child("\u{3bb}"),
        )
        .child(
            div()
                .driver_id(|| mark_name)
                .flex_none()
                .text_color(gpui::rgb(color))
                .child(mark),
        )
        .child(
            div()
                .min_w(px(0.0))
                .text_color(gpui::rgb(t.text))
                .child(line.description),
        )
        .children(meta)
        .children(chips)
        .into_any_element()
}

/// Chip `n` of a script's line (tagged `chat-script-chip-<pane>-<id>-<n>`):
/// its count on the chip background, in danger when its calls failed.
fn script_chip(look: &RowLook<'_>, id: &str, n: usize, chip: chat::ScriptChip) -> Div {
    let t = &look.theme;
    let name = format!("chat-script-chip-{}-{id}-{n}", look.pane_id);
    div()
        .driver_id(|| name)
        .flex_none()
        .px(px(6.0))
        .rounded(px(4.0))
        .bg(gpui::rgb(t.chip))
        .when(chip.failed, |chip_box| {
            chip_box.text_color(gpui::rgb(t.danger))
        })
        .child(chip.text)
}

/// A failed script's error text under its line (tagged
/// `chat-script-error-<pane>-<id>`), indented and wrapping, in danger.
fn script_error(look: &RowLook<'_>, id: &str, text: String) -> Div {
    let name = format!("chat-script-error-{}-{id}", look.pane_id);
    div()
        .driver_id(|| name)
        .pl(px(16.0))
        .min_w(px(0.0))
        .text_color(gpui::rgb(look.theme.danger))
        .child(text)
}

/// A Compact run of read, search, run and script calls: one line tagged
/// `chat-run-<pane>-<first id>` saying what they did, with the running mark
/// (tagged `chat-run-running-<pane>-<first id>`) while one runs, and each of
/// its scripts' lines under it; a click opens it to the calls' cards as
/// Comfortable draws them (scripts as their lines), under the line.
fn run_row(
    look: &RowLook<'_>,
    chat: &SessionChat,
    entries: &[RunItem<'_>],
    summary: &RunSummary,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let Some(first) = entries.first().and_then(RunItem::id) else {
        return div().into_any_element();
    };
    let t = &look.theme;
    let hover_text = t.text;
    let open = chat.log.is_run_open(first);
    let name = format!("chat-run-{}-{first}", look.pane_id);
    let running = summary.running.then(|| {
        let mark = format!("chat-run-running-{}-{first}", look.pane_id);
        div()
            .driver_id(|| mark)
            .flex_none()
            .text_color(gpui::rgb(t.working))
            .child("running")
    });
    let (session_id, id) = (look.session_id.to_owned(), first.to_owned());
    let line = div()
        .id(ElementId::Name(SharedString::from(name.clone())))
        .driver_id(|| name)
        .flex()
        .items_center()
        .gap(px(8.0))
        .text_color(gpui::rgb(t.muted))
        .cursor_pointer()
        .hover(move |style| style.text_color(gpui::rgb(hover_text)))
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.toggle_chat_run(&session_id, &id, cx);
        }))
        .child(
            div()
                .flex_none()
                .child(if open { "\u{25be}" } else { "\u{25b8}" }),
        )
        .child(div().flex_1().min_w(px(0.0)).child(summary.text()))
        .children(running);
    // Closed, the run draws only its scripts' lines under its own; open,
    // every item in place.
    let mut cards: Vec<AnyElement> = Vec::new();
    for item in entries {
        match item {
            RunItem::Entry(entry) if open => cards.push(chat_row(look, chat, entry, CHAT_ROW, cx)),
            RunItem::Entry(_) => {}
            RunItem::Script(script) => cards.push(script_row(look, chat, script, cx)),
        }
    }
    div()
        .flex()
        .flex_col()
        .gap(px(ROW_GAP))
        .child(line)
        .children(cards)
        .into_any_element()
}

/// One muted line centred in the chat pane.
pub(crate) fn chat_note(offset: i8, text: impl Into<SharedString>, t: &UiTheme) -> Div {
    div()
        .flex()
        .flex_1()
        .size_full()
        .items_center()
        .justify_center()
        .text_size(ui_px_at(offset, UI_TEXT_SIZE))
        .text_color(gpui::rgb(t.muted))
        .child(text.into())
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
        ChatEntry::User { text, pending, .. } => user_row(look, id, text, *pending),
        ChatEntry::Answer { text, live, .. } => {
            let answer = answer_row(look, id, text, *live, chat.docs.get(id));
            if look.finals.contains(id) {
                final_reply_box(look, id, answer)
            } else {
                answer
            }
        }
        ChatEntry::Thinking { secs, .. } => muted(thinking_label(*secs)),
        ChatEntry::Tool {
            name,
            summary,
            input,
            result,
            tail_paths,
            ..
        } => {
            let tool = ToolRow {
                id,
                name,
                summary,
                input,
                result: result.as_ref(),
                tail_paths: if prefix == CHAT_ROW { tail_paths } else { &[] },
            };
            tool_card(look, &tool, chat.log.is_expanded(id), cx)
        }
        ChatEntry::System { text, .. } => system_pill(look, text),
        ChatEntry::Receipt {
            question, answer, ..
        } => receipt_row(look, id, &format!("{question} \u{2014} {answer}")),
        ChatEntry::TurnEnd { duration_ms, .. } => muted(turn_label(*duration_ms)),
        ChatEntry::Unknown => div(),
    };
    let name = format!("{prefix}-{}-{id}", look.pane_id);
    div()
        .driver_id(|| name)
        .flex()
        .flex_col()
        .child(row)
        .into_any_element()
}

/// An answered question's receipt, `text`, as a muted line whose paths and
/// web addresses are links the row's route follows.
fn receipt_row(look: &RowLook<'_>, id: &str, text: &str) -> Div {
    let name = format!("chat-receipt-{}-{id}", look.pane_id);
    let line = crate::viewer_md_view::linked_text(
        &name,
        text,
        look.link_route,
        look.session_id,
        &look.theme,
    );
    div()
        .text_size(ui_px_at(look.text_offset, SMALL_TEXT_SIZE))
        .text_color(gpui::rgb(look.theme.muted))
        .child(line)
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
fn user_row(look: &RowLook<'_>, id: &str, text: &str, pending: bool) -> Div {
    div().flex().justify_end().child(
        div()
            .max_w(relative(BUBBLE_MAX_SHARE))
            .px(px(BUBBLE_PAD_X))
            .py(px(BUBBLE_PAD_Y))
            .rounded(px(BUBBLE_RADIUS))
            .bg(gpui::rgb(look.theme.chip))
            .when(pending, |bubble| bubble.opacity(PENDING_OPACITY))
            .child(select_text(
                look.select,
                id,
                UnitSource::User,
                text.to_owned(),
                Vec::new(),
            )),
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
                look.link_route,
                (look.mono_family, look.mono_size),
                &look.theme,
            )
            .for_answer(look.session_id, id)
            .selectable(id, look.select);
            div()
                .flex()
                .flex_col()
                .children((0..doc.row_count()).map(|ix| render_row(doc, &painter, ix, 0.0, 0.0)))
        }
        None => div().child(select_text(
            look.select,
            id,
            UnitSource::AnswerPlain,
            text.to_owned(),
            Vec::new(),
        )),
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
        .driver_id(|| name)
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

/// A turn's final reply, `answer`, in a box tagged `chat-final-<pane>-<id>`:
/// the theme's strong line for its border, no fill of its own (inline code
/// keeps its `chip` background), rounded as the tool cards are.
fn final_reply_box(look: &RowLook<'_>, id: &str, answer: Div) -> Div {
    let name = format!("chat-final-{}-{id}", look.pane_id);
    div()
        .driver_id(|| name)
        .flex()
        .flex_col()
        .px(px(BUBBLE_PAD_X))
        .py(px(BUBBLE_PAD_Y))
        .rounded(px(CARD_RADIUS))
        .border_1()
        .border_color(gpui::rgb(look.theme.line_strong))
        .child(answer)
}

/// A tool call's card: its name, one-line summary and status mark (tagged
/// `chat-tool-<running|done|error>-<pane>-<id>`); a click opens it to a
/// shell tool's whole command, one statement per line, then its input and
/// result. On a host that follows files it offers a Tail button per file
/// the call names, and draws each file's open Tail box under it.
fn tool_card(
    look: &RowLook<'_>,
    tool: &ToolRow<'_>,
    open: bool,
    cx: &mut Context<RootView>,
) -> Div {
    let t = &look.theme;
    let header = tool_header(look, tool);
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
        .font_weight(crate::fonts::mono_weight())
        .text_size(ui_px_at(look.text_offset, CARD_TEXT_SIZE))
        .text_color(gpui::rgb(t.text_2))
        .cursor_pointer()
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.toggle_chat_card(&session_id, &id, cx);
        }))
        .child(header)
        .when(look.can_tail && !tool.tail_paths.is_empty(), |card| {
            card.child(tail_buttons(look, tool, cx))
        })
        .when(open, |card| {
            let statements = shell_lines::tool_statements(tool.name, tool.summary, tool.input);
            card.when(!statements.is_empty(), |card| {
                card.child(command_lines(look, tool.id, statements))
            })
            .child(tool_detail(
                look,
                "Input",
                tool.input,
                (tool.id, UnitSource::ToolInput),
            ))
            .children(tool.result.map(|outcome| {
                let label = if outcome.is_error { "Error" } else { "Result" };
                tool_detail(
                    look,
                    label,
                    &outcome.text,
                    (tool.id, UnitSource::ToolResult),
                )
            }))
        });
    let boxes: Vec<AnyElement> = tool
        .tail_paths
        .iter()
        .enumerate()
        .filter_map(|(index, path)| {
            let tail = look.tails.get(look.session_id, path)?;
            let card = tail.card()?;
            (card.tool_id == tool.id && card.index == index).then(|| {
                tail_box(
                    look,
                    &TailPlace {
                        tool: tool.id,
                        index,
                        path,
                    },
                    tail,
                    cx,
                )
            })
        })
        .collect();
    div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(card)
        .children(boxes)
}

/// A tool card's header line: the tool's name, its one-liner and its status
/// mark (tagged `chat-tool-<running|done|error>-<pane>-<id>`).
fn tool_header(look: &RowLook<'_>, tool: &ToolRow<'_>) -> Div {
    let t = &look.theme;
    let (state, mark, color) = match tool.result {
        None => ("running", "running", t.working),
        Some(outcome) if outcome.is_error => ("error", "failed", t.danger),
        Some(_) => ("done", "done", t.waiting),
    };
    let mark_name = format!("chat-tool-{state}-{}-{}", look.pane_id, tool.id);
    let one_liner = chat::tool_one_liner(tool.summary, tool.name);
    let summary_slot = div()
        .flex_1()
        .min_w(px(0.0))
        .when(!one_liner.is_empty(), |slot| {
            slot.child(select_text(
                look.select,
                tool.id,
                UnitSource::ToolSummary,
                one_liner,
                Vec::new(),
            ))
        });
    div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .child(
            div()
                .flex_none()
                .text_color(gpui::rgb(t.text))
                .child(tool.name.to_owned()),
        )
        .child(summary_slot)
        .child(
            div()
                .driver_id(|| mark_name)
                .flex_none()
                .text_color(gpui::rgb(color))
                .child(mark),
        )
}

/// The last component of `path`, which a Tail button names.
pub(crate) fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\'])
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(path)
}

/// A tool card's Tail buttons, one per file it names (each tagged
/// `chat-tail-<id>-<n>`), each drawn selected while its box is open; a press
/// opens or closes the box without opening the card.
fn tail_buttons(look: &RowLook<'_>, tool: &ToolRow<'_>, cx: &mut Context<RootView>) -> Div {
    let shift = fonts::ui_weight_shift(cx);
    let buttons: Vec<AnyElement> = tool
        .tail_paths
        .iter()
        .enumerate()
        .map(|(index, path)| {
            let selector = format!("chat-tail-{}-{index}", tool.id);
            let open = look.tails.is_open(look.session_id, path);
            let draw = if open {
                primary_button
            } else {
                outlined_button
            };
            let (session_id, tool_id, path) =
                (look.session_id.to_owned(), tool.id.to_owned(), path.clone());
            draw(
                look.text_offset,
                shift,
                &selector,
                ButtonSize::Compact,
                true,
                Layer::Base,
                &look.theme,
            )
            .child(format!("Tail {}", file_name(&path)))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                cx.stop_propagation();
                this.toggle_chat_tail(&session_id, &tool_id, index, &path, cx);
            }))
            .into_any_element()
        })
        .collect();
    div().flex().flex_wrap().gap(px(6.0)).children(buttons)
}

/// Where a Tail box sits: under tool call `tool`'s card, for the `index`th
/// file it names, `path`.
struct TailPlace<'a> {
    tool: &'a str,
    index: usize,
    path: &'a str,
}

/// An open Tail box (tagged `chat-tail-box-<id>-<n>`): the file's lines in
/// the terminal font at the chat's text size, twelve lines tall and
/// scrolling, following its bottom until scrolled up; "No output yet"
/// before any data, or the daemon's refusal in the error colour.
fn tail_box(
    look: &RowLook<'_>,
    place: &TailPlace<'_>,
    tail: &Tail,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let t = &look.theme;
    let size = ui_px_at(look.text_offset, UI_TEXT_SIZE);
    let line = (size / px(1.0) * TAIL_LINE_RATIO).round();
    let lines: Vec<Div> = match tail.shows() {
        TailShows::Nothing => vec![div().text_color(gpui::rgb(t.muted)).child(NO_OUTPUT)],
        TailShows::Error(message) => {
            vec![
                div()
                    .text_color(gpui::rgb(t.danger))
                    .child(message.to_owned()),
            ]
        }
        TailShows::Lines(lines) => lines
            .into_iter()
            .map(|text| div().child(text.to_owned()))
            .collect(),
    };
    let name = format!("chat-tail-box-{}-{}", place.tool, place.index);
    let column = div()
        .id(ElementId::Name(SharedString::from(name.clone())))
        .debug_selector(|| name)
        .h(px(line * TAIL_ROWS + 2.0 * CARD_PAD_Y + 2.0))
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .px(px(CARD_PAD_X))
        .py(px(CARD_PAD_Y))
        .rounded(px(CARD_RADIUS))
        .border_1()
        .border_color(gpui::rgb(t.line_strong))
        .bg(gpui::rgb(t.sunken))
        .font_family(look.mono.clone())
        .font_weight(crate::fonts::mono_weight())
        .text_size(size)
        .line_height(px(line))
        .text_color(gpui::rgb(t.text_2))
        .children(lines);
    let key = (look.session_id.to_owned(), place.path.to_owned());
    let Some(handle) = look.tail_scrolls.get(&key).cloned() else {
        return column.into_any_element();
    };
    if tail.following() {
        handle.scroll_to_bottom();
    }
    column
        .track_scroll(&handle)
        .on_scroll(cx.listener(move |_, _: &ScrollEvent, _, cx| {
            // The box has scrolled by now; the chat around it stays put.
            cx.stop_propagation();
            let (key, handle) = (key.clone(), handle.clone());
            // Read once the wheel has moved the box.
            let root = cx.weak_entity();
            cx.defer(move |cx| {
                let scrolled = -handle.offset().y / px(1.0);
                let max_scroll = handle.max_offset().y / px(1.0);
                let read = root.update(cx, |this, cx| {
                    this.chat
                        .tails
                        .on_scrolled(&key.0, &key.1, scrolled, max_scroll);
                    cx.notify();
                });
                if let Err(err) = read {
                    tracing::debug!("a chat scroll's root view has gone: {err:#}");
                }
            });
        }))
        .into_any_element()
}

/// An open shell tool card's command, one statement per line on the code
/// background (each tagged `chat-tool-line-<pane>-<id>-<n>`); a statement
/// longer than the card wraps.
fn command_lines(look: &RowLook<'_>, id: &str, statements: Vec<String>) -> Div {
    let t = &look.theme;
    div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .px(px(CARD_PAD_X))
        .py(px(CARD_PAD_Y))
        .rounded(px(CARD_RADIUS / 2.0))
        .bg(gpui::rgb(t.sunken))
        .text_color(gpui::rgb(t.text))
        .children(statements.into_iter().enumerate().map(|(n, statement)| {
            let name = format!("chat-tool-line-{}-{id}-{n}", look.pane_id);
            let source = UnitSource::ToolStatement { line: n };
            div().driver_id(|| name).child(select_text(
                look.select,
                id,
                source,
                statement,
                Vec::new(),
            ))
        }))
}

/// One labelled block of an open tool card, its text the `source` text of
/// entry `entry`.
fn tool_detail(
    look: &RowLook<'_>,
    label: &'static str,
    text: &str,
    (entry, source): (&str, UnitSource),
) -> Div {
    let t = &look.theme;
    div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .child(div().text_color(gpui::rgb(t.faint)).child(label))
        .child(div().child(select_text(
            look.select,
            entry,
            source,
            text.to_owned(),
            Vec::new(),
        )))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answer_reparses_when_the_issue_context_arrives() {
        let mut chat = SessionChat::default();
        chat.log.load(
            vec![ChatEntry::Answer {
                id: "a1".to_owned(),
                text: "see RT-12 for it".to_owned(),
                live: false,
            }],
            0,
        );
        chat.refresh_docs();
        let before = chat.docs["a1"].doc.clone();
        let context = IssueContext::new(&["RT".to_owned()], Some("ws"), true);
        chat.set_issue_context(context.clone());
        assert!(
            !Arc::ptr_eq(&before, &chat.docs["a1"].doc),
            "the answer parses again against the new context"
        );
        let reparsed = chat.docs["a1"].doc.clone();
        chat.set_issue_context(context);
        assert!(
            Arc::ptr_eq(&reparsed, &chat.docs["a1"].doc),
            "an unchanged context parses nothing"
        );
    }

    #[test]
    fn the_phone_composer_keyboard_has_autocorrect_and_sentence_caps_without_predictions() {
        let config = phone_composer_configuration();
        assert!(config.autocorrect, "autocorrect");
        assert!(
            !config.suggestions,
            "no predictive text: its inline predictions arrive as marked text"
        );
        assert_eq!(config.autocapitalize, Autocapitalize::Sentences);
        assert_eq!(config.input_action, TextInputAction::Send);
    }

    /// A scroll area from 100 to 700 px, as the edge scroll sees it.
    const TOP: f32 = 100.0;
    const BOTTOM: f32 = 700.0;

    #[test]
    fn edge_step_is_zero_in_the_middle() {
        assert_eq!(edge_step(400.0, TOP, BOTTOM, 0.5), 0.0);
    }

    #[test]
    fn edge_step_is_zero_at_the_bands_inner_line() {
        assert_eq!(
            edge_step(TOP + 48.0, TOP, BOTTOM, 0.5),
            0.0,
            "48 px under the top"
        );
        assert_eq!(
            edge_step(BOTTOM - 48.0, TOP, BOTTOM, 0.5),
            0.0,
            "48 px over the bottom"
        );
    }

    #[test]
    fn edge_step_scrolls_up_near_the_top_and_down_near_the_bottom() {
        assert!(
            edge_step(TOP + 20.0, TOP, BOTTOM, 0.5) < 0.0,
            "toward earlier rows"
        );
        assert!(
            edge_step(BOTTOM - 20.0, TOP, BOTTOM, 0.5) > 0.0,
            "toward later rows"
        );
    }

    #[test]
    fn edge_step_is_full_speed_at_the_edge_and_past_it() {
        assert_eq!(edge_step(TOP, TOP, BOTTOM, 0.5), -300.0, "at the top");
        assert_eq!(
            edge_step(TOP - 30.0, TOP, BOTTOM, 0.5),
            -300.0,
            "over the top"
        );
        assert_eq!(edge_step(BOTTOM, TOP, BOTTOM, 0.5), 300.0, "at the bottom");
        assert_eq!(
            edge_step(BOTTOM + 30.0, TOP, BOTTOM, 0.5),
            300.0,
            "under the bottom"
        );
    }

    #[test]
    fn edge_step_speed_grows_with_depth() {
        assert_eq!(
            edge_step(TOP + 36.0, TOP, BOTTOM, 1.0),
            -150.0,
            "12 px into the top band"
        );
        assert_eq!(
            edge_step(TOP + 12.0, TOP, BOTTOM, 1.0),
            -450.0,
            "36 px into the top band"
        );
        assert_eq!(
            edge_step(BOTTOM - 36.0, TOP, BOTTOM, 1.0),
            150.0,
            "12 px into the bottom band"
        );
        assert_eq!(
            edge_step(BOTTOM - 12.0, TOP, BOTTOM, 1.0),
            450.0,
            "36 px into the bottom band"
        );
    }

    #[test]
    fn edge_step_up_from_the_bottom_clears_the_follow_slack() {
        let max = 1000.0;
        let left = leave_bottom(-0.5, max, max);
        assert!(left <= -2.0, "at least the slack and one more: {left}");
        assert!(
            !chat::follows_bottom(Some((max + left, max))),
            "the next layout does not snap it back to the bottom"
        );
        let nearly = leave_bottom(-0.5, max - 0.5, max);
        assert!(
            nearly <= -2.0,
            "within the slack is at the bottom: {nearly}"
        );
        assert_eq!(leave_bottom(-0.5, 500.0, max), -0.5, "away from the bottom");
        assert_eq!(leave_bottom(3.0, max, max), 3.0, "a step down");
        assert_eq!(
            leave_bottom(-5.0, max, max),
            -5.0,
            "a step already past the slack"
        );
    }

    #[test]
    fn copy_spots_place_copy_by_the_start_when_the_dragged_end_is_below_the_rows() {
        let view = Bounds::new(point(px(0.0), px(100.0)), gpui::size(px(390.0), px(600.0)));
        let start = HandleSpot {
            caret: point(px(50.0), px(4.0)),
            line_height: px(20.0),
        };
        let below = HandleSpot {
            caret: point(px(80.0), px(700.0)),
            line_height: px(20.0),
        };
        let spots = HandleSpots {
            origin: view.origin,
            layer: view.size,
            start: Some(start),
            end: Some(below),
        };
        let pair = copy_spots(spots, Some(view));
        assert_eq!(
            pair,
            Some((start, start)),
            "the start stands in for the end"
        );
        let button = gpui::size(px(60.0), px(44.0));
        let origin = copy_button_origin(start, start, spots.layer, button);
        assert!(
            origin.y < px(100.0),
            "by the start near the top: {origin:?}"
        );
        let gone = HandleSpots {
            start: None,
            ..spots
        };
        assert_eq!(copy_spots(gone, Some(view)), None, "neither end in view");
    }

    #[test]
    fn edge_step_over_no_time_is_zero() {
        assert_eq!(edge_step(TOP, TOP, BOTTOM, 0.0), 0.0);
        assert_eq!(edge_step(BOTTOM, TOP, BOTTOM, 0.0), 0.0);
    }

    #[test]
    fn handle_knob_and_aim_for_a_caret() {
        let spot = HandleSpot {
            caret: point(px(100.0), px(200.0)),
            line_height: px(20.0),
        };
        assert_eq!(
            spot.caret.x,
            px(100.0),
            "the stem's left edge is the caret's x"
        );
        assert_eq!(spot.stem_middle(), point(px(101.0), px(210.0)));
        assert_eq!(
            spot.knob_centre(HandleEnd::Start),
            point(px(101.0), px(195.0)),
            "the start's knob sits above the stem"
        );
        assert_eq!(
            spot.knob_centre(HandleEnd::End),
            point(px(101.0), px(225.0)),
            "the end's knob sits below the stem"
        );
        let start_knob = spot.knob_centre(HandleEnd::Start);
        let end_knob = spot.knob_centre(HandleEnd::End);
        assert_eq!(spot.aim_offset(start_knob), point(px(0.0), px(15.0)));
        assert_eq!(spot.aim_offset(end_knob), point(px(0.0), px(-15.0)));
        assert_eq!(
            spot.aim_offset(point(px(90.0), px(180.0))),
            point(px(11.0), px(30.0)),
            "a grab off the knob aims from where the finger took the handle"
        );
    }

    #[test]
    fn handle_hit_area_is_centred_on_the_knob() {
        let spot = HandleSpot {
            caret: point(px(100.0), px(200.0)),
            line_height: px(20.0),
        };
        assert_eq!(
            spot.hit_origin(HandleEnd::Start),
            point(px(79.0), px(173.0))
        );
        assert_eq!(spot.hit_origin(HandleEnd::End), point(px(79.0), px(203.0)));
    }

    /// Handles measured at window origin (10, 10): the start's caret at
    /// (100, 200), the end's at (100, 300), each on a 20 px line.
    fn two_handles() -> HandleSpots {
        HandleSpots {
            origin: point(px(10.0), px(10.0)),
            layer: LAYER,
            start: Some(spot_at(100.0, 200.0)),
            end: Some(spot_at(100.0, 300.0)),
        }
    }

    #[test]
    fn handle_area_holds_a_release_inside_either_handle() {
        // The start's hit area spans (89, 183) to (133, 227) in the window,
        // the end's (89, 313) to (133, 357).
        let spots = Some(two_handles());
        assert!(
            in_handle_area(spots, point(px(90.0), px(184.0))),
            "inside the start's"
        );
        assert!(
            in_handle_area(spots, point(px(132.0), px(356.0))),
            "inside the end's"
        );
        assert!(
            !in_handle_area(spots, point(px(110.0), px(260.0))),
            "between them, outside both"
        );
        assert!(
            !in_handle_area(spots, point(px(140.0), px(200.0))),
            "beside the start's, outside both"
        );
    }

    #[test]
    fn handle_area_holds_nothing_without_handles() {
        assert!(!in_handle_area(None, point(px(110.0), px(200.0))));
        let none_drawn = HandleSpots {
            start: None,
            end: None,
            ..two_handles()
        };
        assert!(!in_handle_area(
            Some(none_drawn),
            point(px(110.0), px(200.0))
        ));
    }

    /// A spot whose caret is at (`x`, `y`) on a 20 px line.
    fn spot_at(x: f32, y: f32) -> HandleSpot {
        HandleSpot {
            caret: point(px(x), px(y)),
            line_height: px(20.0),
        }
    }

    const LAYER: Size<Pixels> = Size {
        width: px(390.0),
        height: px(600.0),
    };
    const BUTTON: Size<Pixels> = Size {
        width: px(60.0),
        height: px(44.0),
    };

    #[test]
    fn copy_button_sits_above_the_start_in_a_wide_layer() {
        // The start's hit area tops out at 300 - 5 - 22 = 273.
        let origin =
            copy_button_origin(spot_at(200.0, 300.0), spot_at(250.0, 340.0), LAYER, BUTTON);
        assert_eq!(
            origin,
            point(px(170.0), px(225.0)),
            "centred on the start's caret, its bottom 4 px above the hit area"
        );
    }

    #[test]
    fn copy_button_is_clamped_at_the_left_and_right_edges() {
        let end = spot_at(250.0, 340.0);
        assert_eq!(
            copy_button_origin(spot_at(10.0, 300.0), end, LAYER, BUTTON),
            point(px(0.0), px(225.0)),
            "kept inside the left edge"
        );
        assert_eq!(
            copy_button_origin(spot_at(380.0, 300.0), end, LAYER, BUTTON),
            point(px(330.0), px(225.0)),
            "kept inside the right edge"
        );
    }

    #[test]
    fn copy_button_goes_below_the_end_when_the_start_is_near_the_top() {
        // The start's hit area tops out at 30 - 27 = 3: no room above it.
        // The end's hit area runs from 60 + 20 + 5 - 22 = 63 to 107.
        let origin = copy_button_origin(spot_at(200.0, 30.0), spot_at(100.0, 60.0), LAYER, BUTTON);
        assert_eq!(
            origin,
            point(px(170.0), px(111.0)),
            "its top 4 px below the end's hit area, centred on the start"
        );
    }

    #[test]
    fn copy_button_is_clamped_inside_a_layer_with_no_room_above_or_below() {
        // No room above the start (as above); below the end's hit area it
        // would run from 111 to 155, past a 150 px layer's bottom.
        let short = Size {
            width: px(390.0),
            height: px(150.0),
        };
        let origin = copy_button_origin(spot_at(200.0, 30.0), spot_at(100.0, 60.0), short, BUTTON);
        assert_eq!(
            origin,
            point(px(170.0), px(106.0)),
            "its bottom on the layer's bottom, centred on the start"
        );
        let tiny = Size {
            width: px(390.0),
            height: px(30.0),
        };
        let origin = copy_button_origin(spot_at(200.0, 30.0), spot_at(100.0, 60.0), tiny, BUTTON);
        assert_eq!(
            origin.y,
            px(0.0),
            "never above the layer's top, even when it does not fit"
        );
    }

    #[test]
    fn handle_area_holds_no_release_outside_the_layer() {
        // The layer spans window y 10 to 340; the end's square runs from
        // 313 to 357.
        let spots = HandleSpots {
            layer: Size {
                width: px(390.0),
                height: px(330.0),
            },
            ..two_handles()
        };
        assert!(
            in_handle_area(Some(spots), point(px(110.0), px(320.0))),
            "inside the end's square and the layer"
        );
        assert!(
            !in_handle_area(Some(spots), point(px(110.0), px(350.0))),
            "inside the end's square but below the layer, where it is clipped"
        );
    }

    /// A `ChatModel` for `s1` naming `model` and `label`.
    fn model_report(model: Option<&str>, label: Option<&str>) -> DaemonMessage {
        DaemonMessage::ChatModel {
            session_id: "s1".to_owned(),
            model: model.map(str::to_owned),
            label: label.map(str::to_owned),
            effort: None,
            switch_command: None,
        }
    }

    #[test]
    fn a_chat_model_message_sets_the_sessions_model() {
        let mut chat = SessionChat::default();
        assert!(chat.apply(
            model_report(Some("claude-opus-5-5"), Some("Opus 5.5")),
            Instant::now(),
        ));
        assert_eq!(chat.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(chat.model_label(), Some("Opus 5.5"));
    }

    #[test]
    fn a_cleared_chat_model_clears_it() {
        let mut chat = SessionChat::default();
        assert!(chat.apply(
            model_report(Some("claude-opus-5-5"), Some("Opus 5.5")),
            Instant::now(),
        ));
        assert!(chat.apply(model_report(None, None), Instant::now()));
        assert_eq!(chat.model, None);
        assert_eq!(chat.model_label(), None, "the pill draws nothing again");
    }
}
