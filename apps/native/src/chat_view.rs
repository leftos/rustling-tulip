//! A session's chat drawn in a pane: its rows in a scrolling column that
//! follows the bottom until the owner scrolls up, the Terminal / Chat
//! toggle's work (from the header, the pane menu or Ctrl+Shift+M) with each
//! session's remembered view, the `LoadChat` / `UnloadChat` bookkeeping, and the folds
//! of the daemon's chat messages. The rows' model, which panes show a chat
//! and which chats are loaded live in [`crate::chat`].

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use gpui::{
    AnyElement, App, ClickEvent, Context, Div, ElementId, ScrollHandle, SharedString, Window,
    WindowId, div, prelude::*, px, relative,
};
use protocol::error_codes::UNSUPPORTED_MESSAGE;
use protocol::{
    ChatEntry, ClientMessage, DaemonMessage, SessionMode, SessionSnapshot, ToolOutcome,
};

use crate::chat::{self, ChatLoads, ChatLog, PaneView, PaneViews, PromptStack};
use crate::fonts::DEFAULT_FAMILY;
use crate::palette::{CHIP, DANGER, FAINT, LINE_STRONG, SUBTLE, TEXT_2, WAITING, WORKING};
use crate::scrollbar::Scrollbar;
use crate::sidebar::LOCAL_HOST_KEY;
use crate::viewer_md_view::{LinkRoute, MdDoc, Painter, prepare, render_row};
use crate::{MUTED, RootView, TEXT, UI_TEXT_SIZE, new_request_id};

/// What a pane whose chat the daemon refused says above its terminal.
pub(crate) const UNSUPPORTED_NOTE: &str = "This session offers no chat, so it shows its terminal";
/// What a pane whose chat failed to load says above its terminal, before
/// the daemon's message.
const LOAD_FAILED_NOTE: &str = "Could not load the chat: ";
/// What a chat pane says while its first rows are on their way.
const LOADING_NOTE: &str = "Loading the chat…";
/// What a chat pane says when the conversation has no rows yet.
const EMPTY_NOTE: &str = "No messages yet";

const TOGGLE_TO_CHAT_TIP: &str = "Show this session's chat";
const TOGGLE_TO_TERMINAL_TIP: &str = "Show this session's terminal";

/// The room between rows, and around the column.
const ROW_GAP: f32 = 8.0;
const BODY_PAD_X: f32 = 14.0;
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
const SMALL_TEXT_SIZE: f32 = 12.0;
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
    /// By answer id, the text length it was parsed at and the document.
    docs: HashMap<String, (usize, Arc<MdDoc>)>,
}

impl SessionChat {
    /// Applies one chat message: a `Chat` that answers this chat's load or
    /// that the daemon sent unasked (a reset that keeps open cards open), an
    /// append, or a screen change. A reply to an older load changes nothing.
    /// Returns whether it applied.
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
                self.stack.on_screen(&screen, now);
                self.refresh_docs();
            }
            DaemonMessage::ChatAppend {
                entries, removed, ..
            } => {
                self.log.append(entries, &removed);
                self.refresh_docs();
            }
            DaemonMessage::ChatScreenChanged { screen, .. } => self.stack.on_screen(&screen, now),
            _ => return false,
        }
        true
    }

    /// Parses each answer whose text is new or changed length since its
    /// last parse, and forgets the answers that left.
    fn refresh_docs(&mut self) {
        let mut parsed = std::mem::take(&mut self.docs);
        let cancel = AtomicBool::new(false);
        for entry in self.log.entries() {
            let ChatEntry::Answer { id, text, .. } = entry else {
                continue;
            };
            let doc = parsed
                .remove(id)
                .filter(|(len, _)| *len == text.len())
                .unwrap_or_else(|| (text.len(), Arc::new(prepare(text, Path::new(""), &cancel))));
            self.docs.insert(id.clone(), doc);
        }
    }
}

/// The chat side of the panes: what each pane shows, the chats this
/// connection has loaded, each loaded session's chat, each chat pane's
/// scroll, and the note a refused chat leaves on its pane.
#[derive(Default)]
pub(crate) struct ChatPanes {
    views: PaneViews,
    loads: ChatLoads,
    sessions: HashMap<String, SessionChat>,
    scrolls: HashMap<String, Scrollbar>,
    notes: HashMap<String, String>,
}

impl ChatPanes {
    /// Whether pane `pane_id` was switched to its chat.
    pub(crate) fn is_chat(&self, pane_id: &str) -> bool {
        self.views.is_chat(pane_id)
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
    }
}

/// What every row of one chat pane is drawn with.
struct RowLook<'a> {
    pane_id: &'a str,
    session_id: &'a str,
    mono: SharedString,
    mono_family: Option<&'a str>,
    mono_size: f32,
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
    /// Whether pane `pane_id`, showing `session_id`, draws its session's
    /// chat: it was switched to it, and the session offers one.
    pub(crate) fn chat_shown(&self, pane_id: &str, session_id: Option<&str>) -> bool {
        self.chat.is_chat(pane_id)
            && session_id
                .and_then(|id| self.sidebar.session(id))
                .is_some_and(offers_chat)
    }

    /// Whether pane `pane_id` draws its session's chat in place of its
    /// terminal.
    #[must_use]
    pub fn pane_shows_chat(&self, pane_id: &str) -> bool {
        let session = self.panes.get(pane_id).and_then(|slot| slot.session());
        self.chat_shown(pane_id, session)
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

    /// Switches pane `pane_id` between its terminal and its chat. The chat
    /// takes no keys, so a terminal that had the keyboard of the window
    /// drawing it, the main one or a pop-out, lets go of it; switching back
    /// gives the terminal the keyboard again.
    /// The view chosen becomes its session's remembered one, saved at once.
    pub fn toggle_pane_view(&mut self, pane_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.chat.notes.remove(pane_id);
        let shown = self.chat.views.toggle(pane_id);
        self.remember_pane_view(pane_id, shown);
        self.sync_chat_loads();
        self.update_pane_roles(cx);
        match shown {
            PaneView::Chat => self.release_pane_keyboard(pane_id, window, cx),
            PaneView::Terminal => self.focus_pane_where_drawn(pane_id, window, cx),
        }
        cx.notify();
    }

    /// Ctrl+Shift+M in the main window: switches the active tab's focused
    /// pane between its terminal and its chat, when its session offers one.
    pub(crate) fn toggle_focused_pane_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pane_id) = self.focused_pane() else {
            return;
        };
        if self.pane_offers_chat(&pane_id) {
            self.toggle_pane_view(&pane_id, window, cx);
        }
    }

    /// Ctrl+Shift+M in pop-out window `here`: switches the pane it draws (a
    /// popped tab's focused pane) when its session offers a chat.
    pub(crate) fn toggle_window_pane_view(
        &mut self,
        here: WindowId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pane_id) = self.window_pane(here) else {
            return;
        };
        if self.pane_offers_chat(&pane_id) {
            self.toggle_pane_view(&pane_id, window, cx);
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

    /// Brings the loaded chats in line with the panes: `LoadChat` for a
    /// session the first chat pane (in any window) now shows, `UnloadChat`
    /// for one the last chat pane stopped showing. A pane that is gone
    /// forgets its view.
    pub(crate) fn sync_chat_loads(&mut self) {
        let panes = &self.panes;
        self.chat
            .views
            .retain(|pane_id| panes.contains_key(pane_id));
        let shown: Vec<(String, String)> = self
            .panes
            .iter()
            .filter_map(|(pane_id, slot)| Some((pane_id.clone(), slot.session()?.to_owned())))
            .filter(|(pane_id, session_id)| self.chat_shown(pane_id, Some(session_id)))
            .collect();
        let chat_panes: BTreeSet<&str> =
            shown.iter().map(|(pane_id, _)| pane_id.as_str()).collect();
        self.chat
            .scrolls
            .retain(|pane_id, _| chat_panes.contains(pane_id.as_str()));
        for pane_id in chat_panes {
            self.chat.scrolls.entry(pane_id.to_owned()).or_default();
        }
        let wanted = shown
            .into_iter()
            .map(|(_, session_id)| session_id)
            .collect();
        let changes = self.chat.loads.diff(wanted);
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

    /// A message no earlier fold took: a chat message updates its chat,
    /// anything else goes to the agents' fold.
    pub(crate) fn fold_chat_or_agents(&mut self, msg: DaemonMessage, cx: &mut Context<Self>) {
        match msg {
            DaemonMessage::Chat { .. }
            | DaemonMessage::ChatAppend { .. }
            | DaemonMessage::ChatScreenChanged { .. } => self.on_chat_message(msg, cx),
            other => self.fold_agents(other, cx),
        }
    }

    /// A `Chat`, `ChatAppend` or `ChatScreenChanged`: it updates the chat
    /// of a session a chat pane shows; one for any other session, or a
    /// reply to an older load, is dropped.
    pub(crate) fn on_chat_message(&mut self, msg: DaemonMessage, cx: &mut Context<Self>) {
        let now = (self.now)();
        let session_id = match &msg {
            DaemonMessage::Chat { session_id, .. }
            | DaemonMessage::ChatAppend { session_id, .. }
            | DaemonMessage::ChatScreenChanged { session_id, .. } => session_id.clone(),
            _ => return,
        };
        let applied = self
            .chat
            .sessions
            .get_mut(&session_id)
            .is_some_and(|chat| chat.apply(msg, now));
        if applied {
            cx.notify();
        } else {
            tracing::debug!(%session_id, "chat message for a chat no pane shows, or for an older load: dropped");
        }
    }

    /// A daemon `Error` that answers one of this connection's pending
    /// `LoadChat`s: every chat pane of that session snaps back to its
    /// terminal with a note, nothing is unloaded and a later toggle asks
    /// again. An `unsupported_message` refusal says the session offers no
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
        let Some(session_id) = self
            .chat
            .sessions
            .iter()
            .find(|(_, chat)| chat.request.as_deref() == Some(request_id))
            .map(|(session_id, _)| session_id.clone())
        else {
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
        self.chat.sessions.remove(&session_id);
        self.chat.loads.forget(&session_id);
        let panes: Vec<String> = self
            .panes
            .iter()
            .filter(|(pane_id, slot)| {
                slot.session() == Some(session_id.as_str()) && self.chat.is_chat(pane_id)
            })
            .map(|(pane_id, _)| pane_id.clone())
            .collect();
        for pane_id in panes {
            self.chat.views.set(&pane_id, PaneView::Terminal);
            self.chat.notes.insert(pane_id, note.clone());
        }
        self.sync_chat_loads();
        self.update_pane_roles(cx);
        cx.notify();
        true
    }

    /// A click on a tool card of session `session_id`'s chat opens or
    /// closes it.
    fn toggle_chat_card(&mut self, session_id: &str, id: &str, cx: &mut Context<Self>) {
        if let Some(chat) = self.chat.sessions.get_mut(session_id) {
            chat.log.toggle_expanded(id);
            cx.notify();
        }
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
        if self.chat_shown(pane_id, Some(session_id)) {
            return self.chat_body(pane_id, session_id, cx);
        }
        let Some(note) = self.chat.notes.get(pane_id).cloned() else {
            return terminal.into_any_element();
        };
        let name = format!("pane-chat-note-{pane_id}");
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
                    .text_size(px(SMALL_TEXT_SIZE))
                    .text_color(gpui::rgb(MUTED))
                    .child(note),
            )
            .child(terminal)
            .into_any_element()
    }

    /// Session `session_id`'s chat as pane `pane_id` draws it: a muted line
    /// while it loads or has no rows, else its rows in a column that follows
    /// the bottom until scrolled up.
    fn chat_body(&self, pane_id: &str, session_id: &str, cx: &mut Context<Self>) -> AnyElement {
        let name = format!("pane-chat-{pane_id}");
        let chat = self.chat.sessions.get(session_id);
        let Some(chat) = chat.filter(|chat| !chat.log.is_empty() || chat.log.hidden() > 0) else {
            let loading = chat.is_none_or(|chat| chat.request.is_some());
            let note = if loading { LOADING_NOTE } else { EMPTY_NOTE };
            return chat_note(note).debug_selector(|| name).into_any_element();
        };
        let font = &self.sidebar.ui_state().terminal_font;
        let family = font.family.as_deref();
        let look = RowLook {
            pane_id,
            session_id,
            mono: SharedString::from(family.unwrap_or(DEFAULT_FAMILY).to_owned()),
            mono_family: family,
            mono_size: font.size,
        };
        let mut rows: Vec<AnyElement> = Vec::with_capacity(chat.log.len() + 1);
        if chat.log.hidden() > 0 {
            rows.push(hidden_row(chat.log.hidden()));
        }
        rows.extend(
            chat.log
                .entries()
                .map(|entry| chat_row(&look, chat, entry, cx)),
        );
        let column = div()
            .id(ElementId::Name(SharedString::from(format!(
                "chat-scroll-{pane_id}"
            ))))
            .size_full()
            .overflow_y_scroll()
            .px(px(BODY_PAD_X))
            .py(px(BODY_PAD_Y))
            .flex()
            .flex_col()
            .gap(px(ROW_GAP))
            .text_size(px(UI_TEXT_SIZE))
            .text_color(gpui::rgb(TEXT))
            .children(rows);
        let scroll = self.chat.scrolls.get(pane_id);
        let column = match scroll {
            Some(scroll) => {
                follow_bottom(scroll.handle());
                column.track_scroll(scroll.handle())
            }
            None => column,
        };
        div()
            .debug_selector(|| name)
            .relative()
            .flex_1()
            .min_h(px(0.0))
            .size_full()
            .child(column)
            .children(scroll.map(Scrollbar::overlay))
            .into_any_element()
    }
}

/// Keeps a chat at its bottom while it was there at its last layout, so new
/// rows scroll into view; a chat scrolled up stays where it is.
fn follow_bottom(handle: &ScrollHandle) {
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

/// The label and tooltip of a pane header's Terminal / Chat toggle: the
/// view a click switches to.
pub(crate) fn toggle_label(chat_shown: bool) -> (&'static str, &'static str) {
    if chat_shown {
        ("Terminal", TOGGLE_TO_TERMINAL_TIP)
    } else {
        ("Chat", TOGGLE_TO_CHAT_TIP)
    }
}

/// One muted line centred in the chat pane.
fn chat_note(text: &'static str) -> Div {
    div()
        .flex()
        .flex_1()
        .size_full()
        .items_center()
        .justify_center()
        .text_size(px(UI_TEXT_SIZE))
        .text_color(gpui::rgb(MUTED))
        .child(text)
}

/// One row of the chat, tagged `chat-row-<pane>-<id>`.
fn chat_row(
    look: &RowLook<'_>,
    chat: &SessionChat,
    entry: &ChatEntry,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let Some(id) = chat::entry_id(entry) else {
        return div().into_any_element();
    };
    let row = match entry {
        ChatEntry::User { text, pending, .. } => user_row(text, *pending),
        ChatEntry::Answer { text, live, .. } => {
            answer_row(look, id, text, *live, chat.docs.get(id))
        }
        ChatEntry::Thinking { secs, .. } => muted_line(thinking_label(*secs)),
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
        ChatEntry::System { text, .. } => system_pill(text),
        ChatEntry::Receipt {
            question, answer, ..
        } => muted_line(format!("{question} \u{2014} {answer}")),
        ChatEntry::TurnEnd { duration_ms, .. } => muted_line(turn_label(*duration_ms)),
        ChatEntry::Unknown => div(),
    };
    let name = format!("chat-row-{}-{id}", look.pane_id);
    div()
        .debug_selector(|| name)
        .flex()
        .flex_col()
        .child(row)
        .into_any_element()
}

/// The muted first row of a chat whose older rows were left out.
fn hidden_row(hidden: u32) -> AnyElement {
    let noun = if hidden == 1 { "message" } else { "messages" };
    div()
        .flex()
        .justify_center()
        .child(muted_line(format!("{hidden} earlier {noun}")))
        .into_any_element()
}

/// A message the user sent: a bubble on the right, dimmed until the
/// transcript has it.
fn user_row(text: &str, pending: bool) -> Div {
    div().flex().justify_end().child(
        div()
            .max_w(relative(BUBBLE_MAX_SHARE))
            .px(px(BUBBLE_PAD_X))
            .py(px(BUBBLE_PAD_Y))
            .rounded(px(BUBBLE_RADIUS))
            .bg(gpui::rgb(CHIP))
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
    doc: Option<&(usize, Arc<MdDoc>)>,
) -> Div {
    let body = match doc {
        Some((_, doc)) => {
            let painter = Painter::new(
                format!("chat-{id}"),
                LinkRoute::WebOnly,
                look.mono_family,
                look.mono_size,
            );
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
    let cursor = div()
        .w(px(CURSOR_WIDTH))
        .h(px(CURSOR_HEIGHT))
        .bg(gpui::rgb(WORKING));
    let tag = div()
        .text_size(px(TAG_TEXT_SIZE))
        .text_color(gpui::rgb(FAINT))
        .child(writing_tag(text.lines().count()));
    div()
        .debug_selector(|| name)
        .flex()
        .flex_col()
        .border_l_2()
        .border_color(gpui::rgb(WORKING))
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
    let (state, mark, color) = match tool.result {
        None => ("running", "running", WORKING),
        Some(outcome) if outcome.is_error => ("error", "failed", DANGER),
        Some(_) => ("done", "done", WAITING),
    };
    let mark_name = format!("chat-tool-{state}-{}-{}", look.pane_id, tool.id);
    let header = div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .child(
            div()
                .flex_none()
                .text_color(gpui::rgb(TEXT))
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
        .border_color(gpui::rgb(LINE_STRONG))
        .font_family(look.mono.clone())
        .text_size(px(CARD_TEXT_SIZE))
        .text_color(gpui::rgb(TEXT_2))
        .cursor_pointer()
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.toggle_chat_card(&session_id, &id, cx);
        }))
        .child(header)
        .when(open, |card| {
            card.child(tool_detail("Input", tool.input))
                .children(tool.result.map(|outcome| {
                    let label = if outcome.is_error { "Error" } else { "Result" };
                    tool_detail(label, &outcome.text)
                }))
        });
    div().child(card)
}

/// One labelled block of an open tool card.
fn tool_detail(label: &'static str, text: &str) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .child(div().text_color(gpui::rgb(FAINT)).child(label))
        .child(div().child(text.to_owned()))
}

/// A row the daemon wrote: a muted pill centred in the column.
fn system_pill(text: &str) -> Div {
    div().flex().justify_center().child(
        div()
            .px(px(PILL_PAD_X))
            .py(px(PILL_PAD_Y))
            .rounded_full()
            .border_1()
            .border_dashed()
            .border_color(gpui::rgb(LINE_STRONG))
            .text_size(px(SMALL_TEXT_SIZE))
            .text_color(gpui::rgb(SUBTLE))
            .child(text.to_owned()),
    )
}

/// One small muted line.
fn muted_line(text: String) -> Div {
    div()
        .text_size(px(SMALL_TEXT_SIZE))
        .text_color(gpui::rgb(MUTED))
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
