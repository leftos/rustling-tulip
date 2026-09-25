//! The terminal pane: one daemon session rendered with `alacritty_terminal`,
//! with its input, resize, scroll and scrollback handling.

use std::ops::Range;
use std::time::Instant;

use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::CursorShape;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use futures::channel::mpsc::UnboundedSender;
use gpui::{
    App, BorderStyle, Bounds, ClipboardItem, Context, DispatchPhase, ElementInputHandler,
    EntityInputHandler, EventEmitter, FocusHandle, Font, FontStyle, FontWeight, KeyDownEvent,
    Keystroke, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point,
    Rgba, ScrollWheelEvent, SharedString, Subscription, Task, TextRun, UTF16Selection,
    UnderlineStyle, Window, canvas, div, fill, font, outline, point, prelude::*, px, size,
};
use protocol::{ClientMessage, SessionSnapshot};

use crate::Clock;
use crate::mouse::{self, COPY_ON_SELECT, CellSize, Gesture, Tracker, ViewportCell};
use crate::net::NetCommand;
use crate::scrollback_load::{self, ScrollbackLoad, State as LoadState, Step};
use crate::term::{BgSpan, GridSize, Snapshot, Terminal, TextSpan};
use crate::term_input::{self, DeadKeyFate, KeyAction, SessionContext};
use crate::text_input::{offset_from_utf16, offset_to_utf16};

const FONT_FAMILY: &str = "Cascadia Mono";
const FONT_SIZE: f32 = 14.0;
const LINE_HEIGHT: f32 = 18.0;
/// `DSR 6`: the program asks where the cursor is and waits for the reply.
const CURSOR_POSITION_QUERY: &[u8] = b"\x1b[6n";

pub struct TerminalPane {
    term: Terminal,
    focus: FocusHandle,
    tx: UnboundedSender<NetCommand>,
    /// The time the scrollback load's timeouts and retries count from.
    now: Clock,
    attachment: Attachment,
    session: Option<SessionContext>,
    /// Wakes the scrollback load at its next timeout or retry.
    load_timer: Option<Task<()>>,
    scroll_accum: f32,
    /// The grid's top-left corner in the window and its cell size, from the
    /// last layout; mouse positions map to cells through it.
    layout: Option<(Point<Pixels>, CellSize)>,
    /// The mouse gesture in progress (report or select) and its buttons.
    tracker: Tracker,
    /// The last cell a motion report named, so a move inside a cell sends
    /// nothing.
    last_motion: Option<(usize, usize)>,
    /// Whether this pane may size the session's PTY yet.
    size_gate: SizeGate,
    /// Whether this pane answers its session's terminal queries: one pane
    /// per session does, so the child gets each answer once.
    answers_queries: bool,
    /// The IME composition in progress (or a pending dead key), drawn at
    /// the cursor and never sent: only the text it commits is.
    marked: Option<String>,
    /// Whether `marked` is a dead key's accent rather than an IME
    /// composition: keys still reach [`Self::on_key`] while it is pending.
    dead_key: bool,
    /// The character of the last key-down left to the text input, which a
    /// dead key's mark repeats.
    last_key_char: Option<String>,
    /// Drops the composition when the pane loses focus.
    blur: Option<Subscription>,
}

/// The session the pane shows, and the load of its scrollback. Only the
/// first scrollback reply after an attach is written: a later one answers an
/// earlier attach of the same session and would replay its history over the
/// live screen.
#[derive(Debug, Default)]
struct Attachment {
    session_id: Option<String>,
    load: Option<ScrollbackLoad>,
}

impl Attachment {
    fn attach(&mut self, session_id: String, now: Instant) {
        self.session_id = Some(session_id);
        self.load = Some(ScrollbackLoad::start(now));
    }

    fn take(&mut self) -> Option<String> {
        self.load = None;
        self.session_id.take()
    }
}

/// Whether the pane may size its session's PTY: only once a layout pass has
/// measured its real size, and only while it drives the session's size.
#[derive(Debug, Default, Clone, Copy)]
struct SizeGate {
    measured: bool,
    driving: bool,
}

impl SizeGate {
    /// A layout pass measured the grid; returns whether to send the size.
    /// The first measure always does, since nothing was sent before it.
    fn measure(&mut self, changed: bool) -> bool {
        let first = !self.measured;
        self.measured = true;
        self.driving && (first || changed)
    }

    /// Returns whether to send the size now: when the pane takes over with
    /// a measured size.
    fn set_driving(&mut self, driving: bool) -> bool {
        let takes_over = driving && !self.driving;
        self.driving = driving;
        takes_over && self.measured
    }

    fn allows(self) -> bool {
        self.measured && self.driving
    }
}

/// What a pane asks of the root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneEvent {
    /// The scrollback request timed out and retry `attempt` is due.
    ScrollbackRetry { session_id: String, attempt: usize },
}

impl EventEmitter<PaneEvent> for TerminalPane {}

impl TerminalPane {
    pub fn new(tx: UnboundedSender<NetCommand>, now: Clock, cx: &mut Context<Self>) -> Self {
        Self {
            term: Terminal::new(GridSize { cols: 80, rows: 24 }, CursorShape::Block),
            focus: cx.focus_handle(),
            tx,
            now,
            attachment: Attachment::default(),
            session: None,
            load_timer: None,
            scroll_accum: 0.0,
            layout: None,
            tracker: Tracker::default(),
            last_motion: None,
            size_gate: SizeGate::default(),
            answers_queries: false,
            marked: None,
            dead_key: false,
            last_key_char: None,
            blur: None,
        }
    }

    pub fn session_id(&self) -> Option<&str> {
        self.attachment.session_id.as_deref()
    }

    /// The visible screen, one string per row with trailing blanks trimmed.
    pub fn grid_text(&self) -> Vec<String> {
        let snapshot = self.term.snapshot();
        let mut rows = vec![String::new(); self.term.size().rows];
        for span in &snapshot.text {
            let Some(row) = rows.get_mut(span.row) else {
                continue;
            };
            let filled = row.chars().count();
            if filled < span.col {
                row.extend(std::iter::repeat_n(' ', span.col - filled));
            }
            row.push_str(&span.text);
        }
        rows.iter().map(|row| row.trim_end().to_owned()).collect()
    }

    /// The marked text drawn at the cursor, if any.
    pub fn preedit(&self) -> Option<&str> {
        self.marked.as_deref()
    }

    /// The cursor shape the next paint draws.
    pub fn cursor_shape(&self) -> CursorShape {
        self.term.snapshot().cursor_shape
    }

    /// The window position of the centre of cell (`col`, `row`), from the
    /// last layout.
    pub fn cell_center(&self, col: usize, row: usize) -> Option<Point<Pixels>> {
        let (origin, cell) = self.layout?;
        #[expect(clippy::cast_precision_loss, reason = "grid coordinates are small")]
        let (x, y) = (
            (col as f32 + 0.5) * cell.width,
            (row as f32 + 0.5) * cell.height,
        );
        Some(point(origin.x + px(x), origin.y + px(y)))
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    pub fn focus(&self, window: &mut Window) {
        self.focus.focus(window);
    }

    pub fn is_focused(&self, window: &Window) -> bool {
        self.focus.is_focused(window)
    }

    /// Whether this pane drives its session's PTY size: the focused pane
    /// showing it, else the first on screen. Taking over sends the size.
    pub fn set_drives_size(&mut self, drives: bool) {
        if self.size_gate.set_driving(drives) {
            self.send_resize();
        }
    }

    /// Whether this pane answers its session's terminal queries, from
    /// history and live output alike. The others drop their answers.
    pub fn set_answers_queries(&mut self, answers: bool) {
        self.answers_queries = answers;
    }

    /// Show `session` in a fresh terminal waiting for its scrollback, then
    /// size its PTY to this pane. The caller asks for the scrollback once
    /// for every pane showing the session, through
    /// [`Self::request_scrollback`].
    pub fn attach(&mut self, session: &SessionSnapshot, cx: &mut Context<Self>) {
        let context = SessionContext::of(session);
        self.fresh_terminal(context.default_cursor_shape());
        self.session = Some(context);
        self.attachment.attach(session.id.clone(), (self.now)());
        self.send_resize();
        self.schedule_load_tick(cx);
        cx.notify();
    }

    /// Replaces the terminal, dropping any selection or mouse gesture on the
    /// old one.
    fn fresh_terminal(&mut self, cursor: CursorShape) {
        self.term = Terminal::new(self.term.size(), cursor);
        self.tracker = Tracker::default();
        self.last_motion = None;
    }

    pub fn request_scrollback(&self) {
        if let Some(session_id) = self.attachment.session_id.clone() {
            self.send(ClientMessage::LoadScrollback { session_id });
        }
    }

    /// Arms a timer for the scrollback load's next timeout or retry, or
    /// disarms it when the load has none.
    fn schedule_load_tick(&mut self, cx: &mut Context<Self>) {
        let deadline = self
            .attachment
            .load
            .as_ref()
            .and_then(ScrollbackLoad::next_deadline);
        self.load_timer = deadline.map(|deadline| {
            let delay = deadline.saturating_duration_since((self.now)());
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(delay).await;
                // Fails only when the pane is gone, leaving nothing to load.
                this.update(cx, TerminalPane::tick_load).ok();
            })
        });
    }

    fn tick_load(&mut self, cx: &mut Context<Self>) {
        let Some(load) = self.attachment.load.as_mut() else {
            return;
        };
        let steps = load.tick((self.now)());
        if load.state() == LoadState::Failed {
            tracing::warn!(
                "scrollback for session {:?} failed after {} retries",
                self.attachment.session_id,
                scrollback_load::RETRY_DELAYS.len()
            );
        }
        if self.run_load_steps(steps) {
            self.emit_retry(cx);
        }
        self.schedule_load_tick(cx);
        cx.notify();
    }

    /// Runs the load's steps; returns whether a retry is due. The root
    /// sends it, once for every pane showing the session.
    fn run_load_steps(&mut self, steps: Vec<Step>) -> bool {
        let mut retry = false;
        // The load's live output always comes after its history.
        let output_follows = steps
            .iter()
            .any(|step| matches!(step, Step::Live(bytes) if !bytes.is_empty()));
        for step in steps {
            match step {
                Step::Status(text) => self.term.feed(text.as_bytes()),
                Step::Request => retry = true,
                Step::History(bytes) => self.feed_history(&bytes, output_follows),
                Step::Live(bytes) => self.feed_live(&bytes),
                Step::Resize => self.send_resize(),
            }
        }
        retry
    }

    fn emit_retry(&self, cx: &mut Context<Self>) {
        let attempt = self.attachment.load.as_ref().map(ScrollbackLoad::state);
        if let (Some(session_id), Some(LoadState::Loading(attempt))) =
            (self.attachment.session_id.clone(), attempt)
        {
            cx.emit(PaneEvent::ScrollbackRetry {
                session_id,
                attempt,
            });
        }
    }

    /// Take in a newer snapshot of the attached session (its status).
    pub fn update_session(&mut self, session: &SessionSnapshot) {
        if self.session_id() == Some(session.id.as_str()) {
            self.session = Some(SessionContext::of(session));
            self.drop_marked_unless_accepting();
        }
    }

    /// Take in a full session list: the attached session's entry, if listed.
    pub fn refresh_sessions(&mut self, sessions: &[SessionSnapshot]) {
        if let Some(context) = self
            .session_id()
            .and_then(|id| SessionContext::find(sessions, id))
        {
            self.session = Some(context);
            self.drop_marked_unless_accepting();
        }
    }

    /// Forget the attached session. The caller sends `Detach` once no pane
    /// shows it.
    pub fn release(&mut self) {
        self.attachment.take();
        self.fresh_terminal(CursorShape::Block);
        self.session = None;
        self.marked = None;
        self.load_timer = None;
    }

    /// Forget the attachment for a new connection, which starts unattached.
    pub fn reset_for_reconnect(&mut self) {
        if self.attachment.take().is_some() {
            self.fresh_terminal(CursorShape::Block);
        }
        self.session = None;
        self.marked = None;
        self.load_timer = None;
    }

    /// Whether the attached session takes input; a composition is only
    /// held while it does.
    fn accepts_input(&self) -> bool {
        self.session.is_some_and(SessionContext::accepts_input)
    }

    fn drop_marked_unless_accepting(&mut self) {
        if !self.accepts_input() {
            self.marked = None;
        }
    }

    /// Drops the composition in progress without sending it.
    fn drop_marked(&mut self, cx: &mut Context<Self>) {
        if self.marked.take().is_some() {
            cx.notify();
        }
    }

    /// A scrollback reply: the history, then the output held back while it
    /// loaded. Ignored unless it is the first reply since the attach.
    pub fn on_scrollback(
        &mut self,
        data_b64: &str,
        truncated: bool,
    ) -> Result<(), base64::DecodeError> {
        let history = B64.decode(data_b64)?;
        let steps = self
            .attachment
            .load
            .as_mut()
            .and_then(|load| load.on_reply(history, truncated));
        if let Some(steps) = steps {
            self.load_timer = None;
            self.run_load_steps(steps);
        }
        Ok(())
    }

    pub fn on_pty_output(&mut self, data_b64: &str) -> Result<(), base64::DecodeError> {
        let bytes = B64.decode(data_b64)?;
        let live = self
            .attachment
            .load
            .as_mut()
            .and_then(|load| load.on_output(bytes));
        if let Some(bytes) = live {
            self.feed_live(&bytes);
        }
        Ok(())
    }

    /// Feeds replayed history. A cursor-position query at its very end is
    /// still waiting for its answer (`ConPTY` sends one at startup and writes
    /// nothing until it is answered), so it is answered as live output is,
    /// unless `output_follows`: output after it means another client has
    /// answered it already. Every earlier query was answered when it was
    /// first made.
    ///
    /// Known limit: the query is answered a second time when another client
    /// already answered it and the child has printed nothing since, or when
    /// the program got its answer and now waits in silence.
    fn feed_history(&mut self, bytes: &[u8], output_follows: bool) {
        match bytes.strip_suffix(CURSOR_POSITION_QUERY) {
            Some(answered) if !output_follows => {
                self.term.feed_history(answered);
                self.feed_live(CURSOR_POSITION_QUERY);
            }
            _ => self.term.feed_history(bytes),
        }
    }

    /// Feeds live output and answers the queries in it when this pane
    /// answers for its session.
    fn feed_live(&mut self, bytes: &[u8]) {
        self.term.feed(bytes);
        let replies = self.term.take_replies();
        if self.answers_queries && !replies.is_empty() {
            self.send_to_child(&replies);
        }
    }

    fn send(&self, msg: ClientMessage) {
        // Fails only once the network thread has exited, which it does only
        // after every sender has dropped.
        let _ = self.tx.unbounded_send(NetCommand::Send(Box::new(msg)));
    }

    /// Sends what the user typed or pasted, and scrolls to the live screen.
    fn send_input(&mut self, bytes: &[u8]) {
        if self.send_to_child(bytes) {
            self.term.scroll_to_bottom();
        }
    }

    /// Sends `bytes` as input unless nothing is attached or the session has
    /// stopped. Returns whether it sent.
    fn send_to_child(&self, bytes: &[u8]) -> bool {
        let Some(session_id) = self.attachment.session_id.clone() else {
            return false;
        };
        if !self.session.is_some_and(SessionContext::accepts_input) {
            return false;
        }
        self.send(ClientMessage::SendInput {
            session_id,
            data_b64: B64.encode(bytes),
        });
        true
    }

    fn send_resize(&self) {
        if !self.size_gate.allows() {
            return;
        }
        let Some(session_id) = self.attachment.session_id.clone() else {
            return;
        };
        let GridSize { cols, rows } = self.term.size();
        let (Ok(cols), Ok(rows)) = (u16::try_from(cols), u16::try_from(rows)) else {
            return;
        };
        self.send(ClientMessage::Resize {
            session_id,
            cols,
            rows,
        });
    }

    fn ensure_size(&mut self, size: GridSize) {
        let changed = size != self.term.size();
        if changed {
            self.term.resize(size);
        }
        if self.size_gate.measure(changed) {
            self.send_resize();
        }
    }

    fn on_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.session else {
            return;
        };
        let ks = &event.keystroke;
        let action = term_input::key_action(
            ks,
            self.term.app_cursor(),
            self.term.has_selection(),
            session,
        );
        let Some(action) = action else {
            self.last_key_char = ks.key_char.clone().filter(|text| text.chars().count() == 1);
            return;
        };
        self.last_key_char = None;
        if self.end_dead_key(ks) {
            match action {
                KeyAction::Send(bytes) => {
                    self.term.clear_selection();
                    self.send_input(&bytes);
                }
                KeyAction::Copy { clear_selection } => self.copy_selection(clear_selection, cx),
                KeyAction::Paste => self.paste(cx),
                KeyAction::Consume => {}
            }
        }
        cx.stop_propagation();
        cx.notify();
    }

    /// Ends a pending dead key when the terminal handles `ks` itself: the
    /// accent is cancelled by Backspace, dropped for Escape and sent ahead
    /// of any other key. Returns whether `ks` still does its own action.
    fn end_dead_key(&mut self, ks: &Keystroke) -> bool {
        if !self.dead_key {
            return true;
        }
        let Some(accent) = self.marked.take() else {
            return true;
        };
        flush_dead_key();
        match term_input::dead_key_fate(ks) {
            DeadKeyFate::Cancel => false,
            DeadKeyFate::Drop => true,
            DeadKeyFate::SendFirst => {
                self.send_input(accent.as_bytes());
                true
            }
            DeadKeyFate::SendInstead => {
                self.send_input(accent.as_bytes());
                false
            }
        }
    }

    fn copy_selection(&mut self, clear_selection: bool, cx: &mut Context<Self>) {
        if let Some(text) = self.term.selection_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
        if clear_selection {
            self.term.clear_selection();
        }
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let bytes = term_input::paste_bytes(&text, self.term.bracketed_paste());
        self.send_input(&bytes);
    }

    fn on_scroll(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let line_height = px(LINE_HEIGHT);
        self.scroll_accum += event.delta.pixel_delta(line_height).y / line_height;
        let lines = self.scroll_accum.trunc();
        if lines != 0.0 {
            self.scroll_accum -= lines;
            // Saturating cast: a single wheel event never scrolls i32::MAX lines.
            #[expect(
                clippy::cast_possible_truncation,
                reason = "whole-line count from a wheel delta"
            )]
            self.wheel(lines as i32, event.position, event.modifiers);
            cx.notify();
        }
    }

    /// Scrolls `lines` (positive is up): as wheel reports when the child
    /// asked for the mouse, as arrow keys on an alternate screen that asked
    /// for them, and through the history otherwise.
    fn wheel(&mut self, lines: i32, position: Point<Pixels>, modifiers: Modifiers) {
        let mode = self.term.mode();
        if mouse::reports(mode, modifiers.shift) {
            let Some(cell) = self.cell_at(position) else {
                return;
            };
            let button = if lines > 0 {
                mouse::Button::WheelUp
            } else {
                mouse::Button::WheelDown
            };
            for _ in 0..lines.unsigned_abs() {
                self.report(button, mouse::ReportKind::Press, modifiers, cell);
            }
        } else if mouse::wheel_sends_arrows(mode) {
            self.send_to_child(&mouse::wheel_arrows(lines, self.term.app_cursor()));
        } else {
            self.term.scroll(lines);
        }
    }

    fn cell_at(&self, position: Point<Pixels>) -> Option<ViewportCell> {
        let (origin, cell) = self.layout?;
        let x = (position.x - origin.x) / px(1.0);
        let y = (position.y - origin.y) / px(1.0);
        Some(ViewportCell::at(x, y, cell, self.term.size()))
    }

    fn report(
        &self,
        button: mouse::Button,
        kind: mouse::ReportKind,
        modifiers: Modifiers,
        cell: ViewportCell,
    ) {
        let report = mouse::Report {
            button,
            kind,
            mods: mouse::Mods {
                shift: modifiers.shift,
                alt: modifiers.alt,
                ctrl: modifiers.control,
            },
            cell,
        };
        let encoding = mouse::Encoding::of(self.term.mode());
        self.send_to_child(&mouse::encode(&report, encoding));
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus.focus(window);
        let (Some(cell), Some(button)) =
            (self.cell_at(event.position), report_button(event.button))
        else {
            return;
        };
        match self
            .tracker
            .down(button, self.term.mode(), event.modifiers.shift)
        {
            Some(Gesture::Report) => {
                self.report(button, mouse::ReportKind::Press, event.modifiers, cell);
            }
            Some(Gesture::Select) => {
                let point = cell.to_point(self.term.display_offset());
                let ty = mouse::selection_type(event.click_count);
                self.term.start_selection(ty, point, cell.side);
            }
            None => return,
        }
        cx.notify();
    }

    /// A move over the pane with no button down: a hover report when the
    /// child asked for all motion. Moves during a gesture go through
    /// [`Self::on_gesture_move`].
    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, _: &mut Context<Self>) {
        let mode = self.term.mode();
        if self.tracker.moving().is_some()
            || !mouse::reports(mode, event.modifiers.shift)
            || !mouse::reports_motion(mode, false)
        {
            return;
        }
        if let Some(cell) = self.cell_at(event.position) {
            self.report_motion(mouse::Button::None, event.modifiers, cell);
        }
    }

    /// A move anywhere in the window during a gesture, clamped to the grid:
    /// a drag report, or the selection extends.
    fn on_gesture_move(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        let (Some(gesture), Some(cell)) = (self.tracker.moving(), self.cell_at(event.position))
        else {
            return;
        };
        match gesture {
            Gesture::Report => {
                if mouse::reports_motion(self.term.mode(), true) {
                    let button = self.tracker.held().unwrap_or(mouse::Button::None);
                    self.report_motion(button, event.modifiers, cell);
                }
            }
            Gesture::Select => {
                let point = cell.to_point(self.term.display_offset());
                self.term.update_selection(point, cell.side);
                cx.notify();
            }
        }
    }

    /// Reports motion, once per cell entered.
    fn report_motion(&mut self, button: mouse::Button, modifiers: Modifiers, cell: ViewportCell) {
        if self.last_motion == Some((cell.row, cell.col)) {
            return;
        }
        self.last_motion = Some((cell.row, cell.col));
        self.report(button, mouse::ReportKind::Motion, modifiers, cell);
    }

    /// Ends a reported press, or a selection: a click that selected nothing
    /// clears it, and a real selection is copied when copy-on-select is on.
    fn on_mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(button) = report_button(event.button) else {
            return;
        };
        match self.tracker.up(button) {
            Some(Gesture::Report) => {
                if let Some(cell) = self.cell_at(event.position) {
                    self.report(button, mouse::ReportKind::Release, event.modifiers, cell);
                }
                return;
            }
            Some(Gesture::Select) => {}
            None => return,
        }
        if !self.term.has_selection() {
            self.term.clear_selection();
        } else if let Some(text) = mouse::copy_on_select(COPY_ON_SELECT, self.term.selection_text())
        {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
        cx.notify();
    }
}

/// The byte range of the UTF-16 range `range` in `text`, clamped to it.
fn utf8_range(text: &str, range: &Range<usize>) -> Range<usize> {
    let end = offset_from_utf16(text, range.end);
    offset_from_utf16(text, range.start).min(end)..end
}

fn utf16_len(text: &str) -> usize {
    offset_to_utf16(text, text.len())
}

/// The cell the IME anchors to and the composition is drawn from: the
/// cursor's, shown or hidden, or the grid's top-left cell without one.
fn anchor_cell(snap: &Snapshot) -> (usize, usize) {
    snap.cursor_point.unwrap_or_default()
}

/// Clears the dead key Windows still holds after the pane resolved its
/// accent, so the next letter is not composed with it: translating a Space
/// consumes a pending dead key. The key state is all up, so a held modifier
/// cannot change the translation. A negative length means a dead key is
/// still pending, so the translation runs once more.
#[cfg(windows)]
fn flush_dead_key() {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetKeyboardLayout, MAPVK_VK_TO_VSC, MapVirtualKeyW, ToUnicodeEx, VK_SPACE,
    };
    let state = [0u8; 256];
    let space = u32::from(VK_SPACE.0);
    // SAFETY: plain lookups for this thread's layout and a key's scan code.
    let (layout, scan) = unsafe { (GetKeyboardLayout(0), MapVirtualKeyW(space, MAPVK_VK_TO_VSC)) };
    let mut out = [0u16; 8];
    for _ in 0..2 {
        // SAFETY: translates into a local buffer; flags 0 let the call
        // consume the pending dead key, and its output is discarded.
        let len = unsafe { ToUnicodeEx(space, scan, &state, &mut out, 0, Some(layout)) };
        if len >= 0 {
            break;
        }
    }
}

#[cfg(not(windows))]
fn flush_dead_key() {}

/// Text from the platform: typed characters and the IME. Only committed
/// text is sent; the composition before it is held and drawn at the cursor.
impl EntityInputHandler for TerminalPane {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let marked = self.marked.as_deref()?;
        let range = utf8_range(marked, &range_utf16);
        adjusted_range
            .replace(offset_to_utf16(marked, range.start)..offset_to_utf16(marked, range.end));
        marked.get(range).map(str::to_owned)
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let end = self.marked.as_deref().map_or(0, utf16_len);
        Some(UTF16Selection {
            range: end..end,
            reversed: false,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked
            .as_deref()
            .filter(|_| !self.dead_key)
            .map(|marked| 0..utf16_len(marked))
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.drop_marked(cx);
    }

    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked = None;
        self.last_key_char = None;
        if !text.is_empty() {
            self.term.clear_selection();
            self.send_input(text.as_bytes());
        }
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let last_key_char = self.last_key_char.take();
        let composing = self.marked.is_some();
        let mut marked = self.marked.take().unwrap_or_default();
        let range = range_utf16.map_or(0..marked.len(), |range| utf8_range(&marked, &range));
        marked.replace_range(range, new_text);
        self.dead_key = !composing && last_key_char.as_deref() == Some(marked.as_str());
        self.marked = (!marked.is_empty() && self.accepts_input()).then_some(marked);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let (origin, cell) = self.layout?;
        let (row, col) = anchor_cell(&self.term.snapshot());
        #[expect(clippy::cast_precision_loss, reason = "grid coordinates are small")]
        let at = point(
            origin.x + px(col as f32 * cell.width),
            origin.y + px(row as f32 * cell.height),
        );
        Some(Bounds::new(at, size(px(cell.width), px(cell.height))))
    }

    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

fn report_button(button: MouseButton) -> Option<mouse::Button> {
    match button {
        MouseButton::Left => Some(mouse::Button::Left),
        MouseButton::Middle => Some(mouse::Button::Middle),
        MouseButton::Right => Some(mouse::Button::Right),
        MouseButton::Navigate(_) => None,
    }
}

impl Render for TerminalPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.blur.is_none() {
            let blur = cx.on_blur(&self.focus, window, |pane, _, cx| pane.drop_marked(cx));
            self.blur = Some(blur);
        }
        let view = cx.entity();
        let gesture_view = view.clone();
        let input_view = view.clone();
        let focus = self.focus.clone();
        let background = to_rgba(self.term.snapshot().background);
        let grid = canvas(
            move |bounds, window, cx| {
                let m = metrics(window);
                let (snap, marked) = view.update(cx, |v, _| {
                    v.ensure_size(grid_size(bounds, &m));
                    v.layout = Some((bounds.origin, cell_size(&m)));
                    (v.term.snapshot(), v.marked.clone())
                });
                (m, snap, marked)
            },
            move |bounds, (m, snap, marked), window, cx| {
                window.handle_input(&focus, ElementInputHandler::new(bounds, input_view), cx);
                // Window-wide, so a drag keeps going once the pointer leaves the pane.
                window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                    if phase == DispatchPhase::Bubble {
                        gesture_view.update(cx, |pane, cx| pane.on_gesture_move(event, cx));
                    }
                });
                paint_grid(bounds, &snap, &m, window, cx);
                if let Some(text) = marked {
                    paint_preedit(bounds.origin, &snap, &text, &m, window, cx);
                }
            },
        )
        .size_full();
        let mut pane = div()
            .size_full()
            .p(px(crate::PADDING))
            .bg(background)
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .on_any_mouse_down(cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move));
        for button in [MouseButton::Left, MouseButton::Middle, MouseButton::Right] {
            pane = pane
                .on_mouse_up(button, cx.listener(Self::on_mouse_up))
                .on_mouse_up_out(button, cx.listener(Self::on_mouse_up));
        }
        pane.child(grid)
    }
}

fn cell_size(m: &Metrics) -> CellSize {
    CellSize {
        width: m.cell_width / px(1.0),
        height: m.line_height / px(1.0),
    }
}

struct Metrics {
    cell_width: Pixels,
    line_height: Pixels,
    font: Font,
}

fn metrics(window: &Window) -> Metrics {
    let font = font(FONT_FAMILY);
    let text = window.text_system();
    let font_id = text.resolve_font(&font);
    let cell_width = text
        .advance(font_id, px(FONT_SIZE), 'm')
        .map_or(px(FONT_SIZE * 0.6), |s| s.width);
    Metrics {
        cell_width,
        line_height: px(LINE_HEIGHT),
        font,
    }
}

fn grid_size(bounds: Bounds<Pixels>, m: &Metrics) -> GridSize {
    let fit = |avail: Pixels, unit: Pixels| {
        let n = (avail / unit).floor();
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "clamped, small count"
        )]
        let n = n.max(1.0) as usize;
        n
    };
    GridSize {
        cols: fit(bounds.size.width, m.cell_width),
        rows: fit(bounds.size.height, m.line_height),
    }
}

fn to_rgba(c: alacritty_terminal::vte::ansi::Rgb) -> Rgba {
    Rgba {
        r: f32::from(c.r) / 255.0,
        g: f32::from(c.g) / 255.0,
        b: f32::from(c.b) / 255.0,
        a: 1.0,
    }
}

fn cell_origin(origin: Point<Pixels>, m: &Metrics, row: usize, col: usize) -> Point<Pixels> {
    #[expect(clippy::cast_precision_loss, reason = "grid coordinates are small")]
    point(
        origin.x + m.cell_width * col as f32,
        origin.y + m.line_height * row as f32,
    )
}

fn paint_grid(
    bounds: Bounds<Pixels>,
    snap: &Snapshot,
    m: &Metrics,
    window: &mut Window,
    cx: &mut App,
) {
    paint_backgrounds(bounds.origin, &snap.bg, m, window);
    if let Some((row, col)) = snap.cursor {
        paint_cursor(
            cell_origin(bounds.origin, m, row, col),
            snap.cursor_shape,
            m,
            window,
        );
    }
    for span in &snap.text {
        if !span.text.trim().is_empty() {
            paint_span(bounds.origin, span, m, window, cx);
        }
    }
}

fn paint_backgrounds(origin: Point<Pixels>, spans: &[BgSpan], m: &Metrics, window: &mut Window) {
    for bg in spans {
        #[expect(clippy::cast_precision_loss, reason = "grid coordinates are small")]
        let width = m.cell_width * bg.len as f32;
        window.paint_quad(fill(
            Bounds::new(
                cell_origin(origin, m, bg.row, bg.col),
                size(width, m.line_height),
            ),
            to_rgba(bg.color),
        ));
    }
}

/// Paints the cursor in `shape`. The block is translucent so the glyph under
/// it stays readable; the thin shapes are opaque.
fn paint_cursor(at: Point<Pixels>, shape: CursorShape, m: &Metrics, window: &mut Window) {
    const THICKNESS: f32 = 2.0;
    let color = to_rgba(alacritty_terminal::vte::ansi::Rgb {
        r: 0xae,
        g: 0xaf,
        b: 0xad,
    });
    let cell = Bounds::new(at, size(m.cell_width, m.line_height));
    let quad = match shape {
        CursorShape::Block => fill(cell, Rgba { a: 0.55, ..color }),
        CursorShape::Beam => fill(Bounds::new(at, size(px(THICKNESS), m.line_height)), color),
        CursorShape::Underline => fill(
            Bounds::new(
                point(at.x, at.y + m.line_height - px(THICKNESS)),
                size(m.cell_width, px(THICKNESS)),
            ),
            color,
        ),
        CursorShape::HollowBlock => outline(cell, color, BorderStyle::Solid),
        CursorShape::Hidden => return,
    };
    window.paint_quad(quad);
}

fn text_run(span: &TextSpan, m: &Metrics) -> TextRun {
    let color = to_rgba(span.fg);
    TextRun {
        len: span.text.len(),
        font: Font {
            weight: if span.flags.contains(Flags::BOLD) {
                FontWeight::BOLD
            } else {
                FontWeight::NORMAL
            },
            style: if span.flags.contains(Flags::ITALIC) {
                FontStyle::Italic
            } else {
                FontStyle::Normal
            },
            ..m.font.clone()
        },
        color: color.into(),
        background_color: None,
        underline: span
            .flags
            .contains(Flags::UNDERLINE)
            .then(|| UnderlineStyle {
                color: Some(color.into()),
                thickness: px(1.0),
                wavy: false,
            }),
        strikethrough: None,
    }
}

/// Paints the composition from the anchor cell rightwards: underlined, in the
/// terminal's font and foreground, over its background so the cells beneath
/// do not show through.
fn paint_preedit(
    origin: Point<Pixels>,
    snap: &Snapshot,
    text: &str,
    m: &Metrics,
    window: &mut Window,
    cx: &mut App,
) {
    let color = to_rgba(snap.foreground);
    let run = TextRun {
        len: text.len(),
        font: m.font.clone(),
        color: color.into(),
        background_color: None,
        underline: Some(UnderlineStyle {
            color: Some(color.into()),
            thickness: px(1.0),
            wavy: false,
        }),
        strikethrough: None,
    };
    let line = window.text_system().shape_line(
        SharedString::from(text.to_owned()),
        px(FONT_SIZE),
        &[run],
        None,
    );
    let (row, col) = anchor_cell(snap);
    let at = cell_origin(origin, m, row, col);
    let width = line.width.max(m.cell_width);
    window.paint_quad(fill(
        Bounds::new(at, size(width, m.line_height)),
        to_rgba(snap.background),
    ));
    if let Err(err) = line.paint(at, m.line_height, window, cx) {
        tracing::error!("painting the IME composition: {err:#}");
    }
}

fn paint_span(
    origin: Point<Pixels>,
    span: &TextSpan,
    m: &Metrics,
    window: &mut Window,
    cx: &mut App,
) {
    let force_width = (!span.flags.contains(Flags::WIDE_CHAR)).then_some(m.cell_width);
    let line = window.text_system().shape_line(
        SharedString::from(span.text.clone()),
        px(FONT_SIZE),
        &[text_run(span, m)],
        force_width,
    );
    let at = cell_origin(origin, m, span.row, span.col);
    if let Err(err) = line.paint(at, m.line_height, window, cx) {
        tracing::error!("painting row {}: {err:#}", span.row);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::{Attachment, SizeGate};

    fn attach(attachment: &mut Attachment, id: &str) {
        attachment.attach(id.to_owned(), Instant::now());
    }

    /// Whether a scrollback reply arriving now would be written.
    fn accept_scrollback(attachment: &mut Attachment) -> bool {
        attachment
            .load
            .as_mut()
            .and_then(|load| load.on_reply(Vec::new(), false))
            .is_some()
    }

    #[test]
    fn size_gate_waits_for_the_first_measure() {
        let mut gate = SizeGate::default();
        assert!(!gate.set_driving(true), "nothing measured yet");
        assert!(!gate.allows());
        assert!(
            gate.measure(false),
            "the first measure sends even at the old size"
        );
        assert!(gate.allows());
        assert!(!gate.measure(false));
        assert!(gate.measure(true));
    }

    #[test]
    fn size_gate_only_the_driver_sends() {
        let mut gate = SizeGate::default();
        assert!(!gate.measure(true), "not driving");
        assert!(!gate.allows());
        assert!(
            gate.set_driving(true),
            "taking over sends the measured size"
        );
        assert!(!gate.set_driving(true));
        assert!(!gate.set_driving(false));
        assert!(!gate.measure(true));
    }

    #[test]
    fn second_scrollback_for_the_same_attach_is_ignored() {
        let mut attachment = Attachment::default();
        attach(&mut attachment, "a");
        assert!(accept_scrollback(&mut attachment));
        assert!(!accept_scrollback(&mut attachment));
    }

    #[test]
    fn scrollback_after_reattach_is_accepted() {
        let mut attachment = Attachment::default();
        attach(&mut attachment, "a");
        assert!(accept_scrollback(&mut attachment));
        attachment.take();
        attach(&mut attachment, "b");
        attachment.take();
        attach(&mut attachment, "a");
        assert!(accept_scrollback(&mut attachment));
    }

    #[test]
    fn scrollback_without_an_attachment_is_ignored() {
        let mut attachment = Attachment::default();
        assert!(!accept_scrollback(&mut attachment));
        attach(&mut attachment, "a");
        attachment.take();
        assert!(!accept_scrollback(&mut attachment));
    }
}
