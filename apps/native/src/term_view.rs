//! The terminal pane: one daemon session rendered with `alacritty_terminal`,
//! with its input, resize, scroll and scrollback handling.

use std::cell::RefCell;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use alacritty_terminal::index::Side;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{CursorShape, Rgb};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use futures::channel::mpsc::UnboundedSender;
use gpui::{
    App, BorderStyle, Bounds, Context, CursorStyle, DispatchPhase, ElementInputHandler,
    EntityInputHandler, EventEmitter, FocusHandle, Font, FontStyle, FontWeight, HitboxBehavior,
    KeyDownEvent, Keystroke, Modifiers, MouseButton, MouseDownEvent, MouseExitEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Point, Rgba, ScrollWheelEvent, SharedString,
    Subscription, Task, TextRun, UTF16Selection, UnderlineStyle, Window, canvas, div, fill, font,
    outline, point, prelude::*, px, size,
};
use protocol::{ClientMessage, SessionMode, SessionSnapshot};

use crate::Clock;
use crate::fonts::{self, FontSettings};
use crate::links::TerminalLink;
use crate::mouse::{self, COPY_ON_SELECT, CellSize, Gesture, Tracker, ViewportCell};
use crate::net::NetCommand;
use crate::open;
use crate::scrollback_load::{self, ReplyVerdict, ScrollbackLoad, State as LoadState, Step};
use crate::shell_marks::{ShellDot, ShellStatus};
use crate::term::{BgSpan, GridSize, SYNC_TIMEOUT, Snapshot, Terminal, TextSpan};
use crate::term_input::{self, DeadKeyFate, KeyAction, SessionContext};
use crate::text_input::{offset_from_utf16, offset_to_utf16};
use crate::theme;

/// `DSR 6`: the program asks where the cursor is and waits for the reply.
const CURSOR_POSITION_QUERY: &[u8] = b"\x1b[6n";

/// The width of the gutter left of a plain shell's grid, where the command
/// dots sit.
const GUTTER: f32 = 14.0;
/// A command dot's diameter, and its left edge in the gutter: 10px left of
/// the text.
const DOT: f32 = 8.0;
const DOT_LEFT: f32 = GUTTER - 10.0;

pub struct TerminalPane {
    /// The grid pane this terminal fills, which names its dots.
    pane_id: String,
    term: Terminal,
    focus: FocusHandle,
    tx: UnboundedSender<NetCommand>,
    /// The time the scrollback load's timeouts and retries count from.
    now: Clock,
    attachment: Attachment,
    session: Option<SessionContext>,
    /// Wakes the scrollback load at its next timeout or retry.
    load_timer: Option<Task<()>>,
    /// Wakes the pane when a synchronized update's timeout passes.
    sync_timer: Option<Task<()>>,
    /// Our deadline for the synchronized update that is pending, on the
    /// injected clock; `None` when none is pending.
    sync_deadline: Option<Instant>,
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
    /// The font the pane draws with.
    font: FontSettings,
    /// `font`'s family resolved against the installed fonts, once measured.
    family: Option<SharedString>,
    /// The folders the session's relative link paths resolve against.
    base_dirs: Vec<String>,
    /// Link mode and the link under the mouse.
    links: LinkHover,
    /// Bumped whenever the terminal's content may have changed: output,
    /// history, a resize or a fresh terminal. Keys the hover's link cache.
    content_generation: u64,
    /// The terminal's background, which every fresh terminal is given.
    background: Rgb,
    /// The padding's colour around the terminal; `None` follows the
    /// terminal's background.
    frame: Option<Rgb>,
}

/// Ctrl+hover state: whether Ctrl is held, the viewport cell (row, column)
/// under the mouse while it is over this pane's grid, and the last link
/// detection.
#[derive(Debug, Default)]
struct LinkHover {
    active: bool,
    cell: Option<(usize, usize)>,
    cache: RefCell<Option<CachedLinks>>,
}

/// The links around one grid line, kept while the terminal's content stays
/// the same.
#[derive(Debug)]
struct CachedLinks {
    /// The pane's content generation and the hovered grid line.
    key: (u64, i32),
    links: Vec<TerminalLink>,
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

/// A daemon scrollback reply, which the root hands to every pane showing the
/// session it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrollbackReply {
    /// The session the history belongs to.
    pub session_id: String,
    /// The history, base64-encoded.
    pub data_b64: String,
    /// Whether the daemon's scrollback ring had dropped bytes before it.
    pub truncated: bool,
    /// The `LoadScrollback` request the reply answers, when it names one.
    pub request_id: Option<String>,
    /// The daemon restarted the session's output forwarder with this reply.
    pub forwarder_restarted: bool,
}

/// What a pane asks of the root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneEvent {
    /// The scrollback request `request_id` timed out and retry `attempt` is
    /// due.
    ScrollbackRetry {
        session_id: String,
        attempt: usize,
        request_id: Option<String>,
    },
    /// The pane made a copy, or the program stored one (OSC 52): the root
    /// puts it on the clipboard and shows the chip.
    Copied { text: String },
    /// Ctrl+click picked `link`; a path in it resolves against
    /// `base_dirs`, the session's folders.
    OpenLink {
        link: TerminalLink,
        base_dirs: Vec<String>,
    },
}

impl EventEmitter<PaneEvent> for TerminalPane {}

impl TerminalPane {
    pub fn new(
        pane_id: String,
        tx: UnboundedSender<NetCommand>,
        now: Clock,
        font: FontSettings,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            pane_id,
            term: Terminal::new(GridSize { cols: 80, rows: 24 }, CursorShape::Block),
            focus: cx.focus_handle(),
            tx,
            now,
            attachment: Attachment::default(),
            session: None,
            load_timer: None,
            sync_timer: None,
            sync_deadline: None,
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
            font: font.normalized(),
            family: None,
            base_dirs: Vec::new(),
            links: LinkHover::default(),
            content_generation: 0,
            background: theme::DEFAULT_BACKGROUND,
            frame: None,
        }
    }

    /// Paints the terminal on `background`, whose theme it rebuilds, and the
    /// padding around it in `frame`, or in the terminal's background when
    /// `frame` is `None`.
    pub fn set_colors(&mut self, background: Rgb, frame: Option<Rgb>, cx: &mut Context<Self>) {
        if background == self.background && frame == self.frame {
            return;
        }
        if background != self.background {
            self.background = background;
            self.term.set_background(background);
        }
        self.frame = frame;
        cx.notify();
    }

    /// What the pane fills: the terminal's area with the terminal's
    /// background (a program's `OSC 11` included), and the padding ring
    /// around it with the frame, which follows that background when unset.
    pub fn fills(&self) -> (Rgb, Rgb) {
        let background = self.term.background();
        (background, self.frame.unwrap_or(background))
    }

    /// The terminal's background and default text colour as it paints them.
    pub fn terminal_colors(&self) -> (Rgb, Rgb) {
        let snapshot = self.term.snapshot();
        (snapshot.background, snapshot.foreground)
    }

    pub fn session_id(&self) -> Option<&str> {
        self.attachment.session_id.as_deref()
    }

    /// The font the pane draws with.
    pub fn font(&self) -> &FontSettings {
        &self.font
    }

    /// Draws with `settings` from the next frame, which re-measures the grid
    /// and resizes the PTY to fit.
    pub fn set_font(&mut self, settings: FontSettings, cx: &mut Context<Self>) {
        let settings = settings.normalized();
        if settings == self.font {
            return;
        }
        self.font = settings;
        self.family = None;
        cx.notify();
    }

    /// The family the pane's font resolves to, resolved once per font.
    fn resolved_family(&mut self, window: &Window) -> SharedString {
        if let Some(family) = &self.family {
            return family.clone();
        }
        let available = fonts::available_families(window.text_system());
        let requested = self.font.family.as_deref().unwrap_or(fonts::DEFAULT_FAMILY);
        let family = fonts::resolve_family(Some(requested), &available);
        if !family.eq_ignore_ascii_case(requested) {
            tracing::warn!("terminal font {requested:?} is not installed; using {family:?}");
        }
        self.family = Some(family.clone());
        family
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

    /// The dots the gutter draws: the finished commands whose prompt row is
    /// on screen, top first.
    pub fn shell_dots(&self) -> Vec<ShellDot> {
        self.term.shell_dots()
    }

    /// Whether the pane keeps a gutter for command dots: only a plain
    /// shell's does.
    fn has_gutter(&self) -> bool {
        self.session
            .is_some_and(|session| session.mode == SessionMode::PlainShell)
    }

    /// Whether `position` is in the gutter left of the grid.
    fn in_gutter(&self, position: Point<Pixels>) -> bool {
        self.has_gutter()
            && self
                .layout
                .is_some_and(|(origin, _)| (origin.x - px(GUTTER)..origin.x).contains(&position.x))
    }

    /// The gutter and its dots, each centred on its prompt row, from the
    /// last layout's row height.
    fn gutter(&self) -> impl IntoElement {
        let dots: Vec<_> = self
            .layout
            .map(|(_, cell)| {
                self.shell_dots()
                    .into_iter()
                    .enumerate()
                    .map(|(n, dot)| gutter_dot(&self.pane_id, n, dot, cell.height))
                    .collect()
            })
            .unwrap_or_default();
        div()
            .flex_none()
            .w(px(GUTTER))
            .h_full()
            .relative()
            .children(dots)
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
    /// for every pane showing the session, and names the request through
    /// [`Self::expect_scrollback`].
    pub fn attach(&mut self, session: &SessionSnapshot, cx: &mut Context<Self>) {
        let context = SessionContext::of(session);
        self.fresh_terminal(
            context.default_cursor_shape(),
            context.mode == SessionMode::PlainShell,
        );
        self.session = Some(context);
        self.base_dirs = open::base_dirs(session);
        self.attachment.attach(session.id.clone(), (self.now)());
        self.send_resize();
        self.schedule_load_tick(cx);
        cx.notify();
    }

    /// Replaces the terminal, dropping any selection or mouse gesture on the
    /// old one. A plain shell's terminal keeps the commands the shell marks.
    fn fresh_terminal(&mut self, cursor: CursorShape, plain_shell: bool) {
        let size = self.term.size();
        self.term = if plain_shell {
            Terminal::with_shell_marks(size, cursor, self.now.clone())
        } else {
            Terminal::new(size, cursor)
        };
        self.term.set_background(self.background);
        self.content_changed();
        self.tracker = Tracker::default();
        self.last_motion = None;
        self.sync_timer = None;
        self.sync_deadline = None;
    }

    /// The terminal's content may read differently now.
    fn content_changed(&mut self) {
        self.content_generation = self.content_generation.wrapping_add(1);
    }

    /// A `LoadScrollback` for the attached session went out under
    /// `request_id`: only its reply is written.
    pub fn expect_scrollback(&mut self, request_id: &str) {
        if let Some(load) = self.attachment.load.as_mut() {
            load.expect_reply_to(request_id.to_owned());
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
        if self.run_load_steps(steps, cx) {
            self.emit_retry(cx);
        }
        self.schedule_load_tick(cx);
        cx.notify();
    }

    /// Runs the load's steps; returns whether a retry is due. The root
    /// sends it, once for every pane showing the session.
    fn run_load_steps(&mut self, steps: Vec<Step>, cx: &mut Context<Self>) -> bool {
        let mut retry = false;
        // The load's live output always comes after its history.
        let output_follows = steps
            .iter()
            .any(|step| matches!(step, Step::Live(bytes) if !bytes.is_empty()));
        for step in steps {
            match step {
                Step::Status(text) => {
                    self.term.feed(text.as_bytes());
                    self.content_changed();
                    self.service_term(cx);
                }
                Step::Request => retry = true,
                Step::History(bytes) => self.feed_history(&bytes, output_follows, cx),
                Step::Live(bytes) => self.feed_live(&bytes, cx),
                Step::Resize => self.send_resize(),
            }
        }
        retry
    }

    fn emit_retry(&self, cx: &mut Context<Self>) {
        let (Some(session_id), Some(load)) = (
            self.attachment.session_id.clone(),
            self.attachment.load.as_ref(),
        ) else {
            return;
        };
        if let LoadState::Loading(attempt) = load.state() {
            cx.emit(PaneEvent::ScrollbackRetry {
                session_id,
                attempt,
                request_id: load.request_id().map(str::to_owned),
            });
        }
    }

    /// Take in a newer snapshot of the attached session (its status).
    pub fn update_session(&mut self, session: &SessionSnapshot) {
        if self.session_id() == Some(session.id.as_str()) {
            self.session = Some(SessionContext::of(session));
            self.base_dirs = open::base_dirs(session);
            self.drop_marked_unless_accepting();
        }
    }

    /// Take in a full session list: the attached session's entry, if listed.
    pub fn refresh_sessions(&mut self, sessions: &[SessionSnapshot]) {
        let Some(session) = self
            .session_id()
            .and_then(|id| sessions.iter().find(|session| session.id == id))
        else {
            return;
        };
        self.session = Some(SessionContext::of(session));
        self.base_dirs = open::base_dirs(session);
        self.drop_marked_unless_accepting();
    }

    /// Forget the attached session. The caller sends `Detach` once no pane
    /// shows it.
    pub fn release(&mut self) {
        self.attachment.take();
        self.fresh_terminal(CursorShape::Block, false);
        self.session = None;
        self.base_dirs.clear();
        self.marked = None;
        self.load_timer = None;
        self.sync_timer = None;
        self.sync_deadline = None;
    }

    /// Forget the attachment for a new connection, which starts unattached.
    pub fn reset_for_reconnect(&mut self) {
        if self.attachment.take().is_some() {
            self.fresh_terminal(CursorShape::Block, false);
        }
        self.session = None;
        self.base_dirs.clear();
        self.marked = None;
        self.load_timer = None;
        self.sync_timer = None;
        self.sync_deadline = None;
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

    /// A scrollback reply: the history, then any output held back while it
    /// loaded unless the daemon restarted the forwarder for it. Ignored,
    /// and not decoded, unless it is the first reply since the attach and
    /// answers the latest request.
    pub fn on_scrollback(
        &mut self,
        reply: &ScrollbackReply,
        cx: &mut Context<Self>,
    ) -> Result<(), base64::DecodeError> {
        let Some(load) = self.attachment.load.as_mut() else {
            return Ok(());
        };
        let verdict = load.on_reply(
            reply.request_id.as_deref(),
            reply.forwarder_restarted,
            reply.truncated,
            || B64.decode(&reply.data_b64),
        )?;
        match verdict {
            ReplyVerdict::Accepted(steps) => {
                self.load_timer = None;
                self.run_load_steps(steps, cx);
            }
            ReplyVerdict::Stale => tracing::debug!(
                "dropping a scrollback reply for session {:?} to an earlier request {:?}",
                reply.session_id,
                reply.request_id
            ),
            ReplyVerdict::NotLoading => {}
        }
        Ok(())
    }

    pub fn on_pty_output(
        &mut self,
        data_b64: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), base64::DecodeError> {
        let bytes = B64.decode(data_b64)?;
        let live = self
            .attachment
            .load
            .as_mut()
            .and_then(|load| load.on_output(bytes));
        if let Some(bytes) = live {
            self.feed_live(&bytes, cx);
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
    fn feed_history(&mut self, bytes: &[u8], output_follows: bool, cx: &mut Context<Self>) {
        self.content_changed();
        match bytes.strip_suffix(CURSOR_POSITION_QUERY) {
            Some(answered) if !output_follows => {
                self.term.feed_history(answered);
                self.feed_live(CURSOR_POSITION_QUERY, cx);
            }
            _ => {
                self.term.feed_history(bytes);
                self.service_term(cx);
            }
        }
    }

    /// Feeds live output and answers what it asks for when this pane
    /// answers for its session.
    fn feed_live(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        self.term.feed(bytes);
        self.content_changed();
        self.service_term(cx);
    }

    /// Answers what the terminal asked for while it advanced: the replies it
    /// queues for the child and the texts the program stored on the clipboard
    /// (OSC 52), both only from the pane that answers for the session, and
    /// then the check for a synchronized update still pending.
    fn service_term(&mut self, cx: &mut Context<Self>) {
        let replies = self.term.take_replies();
        if self.answers_queries && !replies.is_empty() {
            self.send_to_child(&replies);
        }
        let copies = self.term.take_copies();
        if self.answers_queries {
            for text in copies {
                cx.emit(PaneEvent::Copied { text });
            }
        }
        self.track_sync(cx);
    }

    /// Follows the terminal's synchronized update (`DEC 2026`): records the
    /// pane's own deadline for one that has just become pending, drops it
    /// when none is, and arms the timer for it — only when the deadline
    /// moved, so a chunk of output does not restart an armed timer.
    fn track_sync(&mut self, cx: &mut Context<Self>) {
        let deadline = if self.term.sync_pending() {
            match self.sync_deadline {
                Some(deadline) => Some(deadline),
                // vte times the update on a real clock, which the injected
                // one cannot move, so the pane keeps its own.
                None => Some((self.now)() + SYNC_TIMEOUT),
            }
        } else {
            None
        };
        if deadline != self.sync_deadline {
            self.sync_deadline = deadline;
            self.arm_sync_tick(cx);
        }
    }

    /// Arms the timer that wakes the pane at the pending synchronized
    /// update's deadline, or disarms it when none is pending.
    fn arm_sync_tick(&mut self, cx: &mut Context<Self>) {
        self.sync_timer = self.sync_deadline.map(|deadline| {
            let delay = deadline.saturating_duration_since((self.now)());
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(delay).await;
                // Fails only when the pane is gone, and its terminal with it.
                this.update(cx, TerminalPane::tick_sync).ok();
            })
        });
    }

    /// The timer fired: end the synchronized update whose deadline has
    /// passed, writing what it buffered to the grid. One the timer ran ahead
    /// of waits for its deadline again.
    fn tick_sync(&mut self, cx: &mut Context<Self>) {
        let Some(deadline) = self.sync_deadline else {
            return;
        };
        if deadline <= (self.now)() {
            self.term.stop_sync();
            self.service_term(cx);
            cx.notify();
        } else {
            self.arm_sync_tick(cx);
        }
    }

    fn send(&self, msg: ClientMessage) {
        // Fails only once the network thread has exited, which it does only
        // after every sender has dropped.
        let _ = self.tx.unbounded_send(NetCommand::Send(Box::new(msg)));
    }

    /// Sends what the user typed or pasted, and scrolls to the live screen.
    /// Returns the session it went to, or `None` when it sent nothing.
    fn send_input(&mut self, bytes: &[u8]) -> Option<String> {
        let sent_to = self.send_to_child(bytes);
        if sent_to.is_some() {
            self.term.scroll_to_bottom();
        }
        sent_to
    }

    /// Sends `bytes` as input unless nothing is attached or the session has
    /// stopped. Returns the session it sent to, or `None` when it sent
    /// nothing.
    fn send_to_child(&self, bytes: &[u8]) -> Option<String> {
        let session_id = self.attachment.session_id.clone()?;
        if !self.session.is_some_and(SessionContext::accepts_input) {
            return None;
        }
        self.send(ClientMessage::SendInput {
            session_id: session_id.clone(),
            data_b64: B64.encode(bytes),
        });
        Some(session_id)
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

    /// Sizes the terminal to `size`, telling the session when the size gate
    /// allows. Returns whether the terminal was resized.
    fn ensure_size(&mut self, size: GridSize) -> bool {
        let changed = size != self.term.size();
        if changed {
            self.term.resize(size);
            self.content_changed();
        }
        if self.size_gate.measure(changed) {
            self.send_resize();
        }
        changed
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
            cx.emit(PaneEvent::Copied { text });
        }
        if clear_selection {
            self.term.clear_selection();
        }
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let bracketed = self.term.bracketed_paste();
        let bytes = term_input::paste_bytes(&text, bracketed);
        // A paste no session took is not logged: nothing saw it.
        if let Some(session_id) = self.send_input(&bytes) {
            tracing::info!(
                "{}",
                paste_log_line(
                    next_paste_number(),
                    &session_id,
                    text.chars().count(),
                    bracketed,
                    bytes.len(),
                )
            );
        }
    }

    fn on_scroll(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let line_height = self
            .layout
            .map_or(px(self.font.size * 1.2), |(_, cell)| px(cell.height));
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
        // The gutter is the dots' own: a press there starts no selection
        // and reaches no program.
        if self.in_gutter(event.position) {
            cx.stop_propagation();
            return;
        }
        // Ctrl+click on a link opens it, ahead of a selection or a mouse
        // report; its release then finds no gesture to end. Only the first
        // press of a multi-click opens it; the later ones do nothing.
        if event.button == MouseButton::Left
            && event.modifiers.secondary()
            && let Some(link) = self
                .cell_within(event.position)
                .and_then(|cell| self.link_at(cell.row, cell.col))
        {
            if event.click_count == 1 {
                cx.emit(PaneEvent::OpenLink {
                    link,
                    base_dirs: self.base_dirs.clone(),
                });
            }
            cx.notify();
            return;
        }
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
            || self.in_gutter(event.position)
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

    /// A move anywhere in the window: the cell under the mouse while it is
    /// over this pane's grid (`over`), and whether Ctrl is held.
    fn on_hover_move(&mut self, event: &MouseMoveEvent, over: bool, cx: &mut Context<Self>) {
        let cell = over
            .then(|| self.cell_within(event.position))
            .flatten()
            .map(|cell| (cell.row, cell.col));
        let active = event.modifiers.secondary();
        let changed = (active, cell) != (self.links.active, self.links.cell);
        let shown = active || self.links.active;
        self.links.active = active;
        self.links.cell = cell;
        if changed && shown {
            cx.notify();
        }
    }

    /// Holds link mode while Ctrl is down, in this pane whether it has the
    /// focus or not.
    /// Only a pane with a hovered cell repaints, since only it can underline.
    pub fn set_link_mode(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.links.active != on {
            self.links.active = on;
            if self.links.cell.is_some() {
                cx.notify();
            }
        }
    }

    /// The pointer left the window: no cell is hovered, so nothing is
    /// underlined.
    fn clear_hover(&mut self, cx: &mut Context<Self>) {
        if self.links.cell.take().is_some() {
            cx.notify();
        }
    }

    /// The link Ctrl+hover underlines: the one under the mouse while Ctrl is
    /// held.
    pub fn hovered_link(&self) -> Option<TerminalLink> {
        let (row, col) = self.links.cell.filter(|_| self.links.active)?;
        self.link_at(row, col)
    }

    /// The link whose own characters take viewport cell (`row`, `col`). The
    /// links around a grid line are detected again only once the terminal's
    /// content changed or another line is hovered.
    fn link_at(&self, row: usize, col: usize) -> Option<TerminalLink> {
        let cell = ViewportCell {
            row,
            col,
            side: Side::Left,
        };
        let point = cell.to_point(self.term.display_offset());
        let (line, col) = (point.line.0, point.column.0);
        let key = (self.content_generation, line);
        let mut cache = self.links.cache.borrow_mut();
        if cache.as_ref().is_none_or(|cached| cached.key != key) {
            let links = self
                .term
                .link_window(line)
                .map(|window| window.links())
                .unwrap_or_default();
            *cache = Some(CachedLinks { key, links });
        }
        cache
            .as_ref()?
            .links
            .iter()
            .find(|link| link.covers(line, col))
            .cloned()
    }

    /// The cell under `position` when it lies on the grid itself rather
    /// than its padding.
    fn cell_within(&self, position: Point<Pixels>) -> Option<ViewportCell> {
        let (origin, cell) = self.layout?;
        let x = (position.x - origin.x) / px(1.0);
        let y = (position.y - origin.y) / px(1.0);
        let size = self.term.size();
        #[expect(clippy::cast_precision_loss, reason = "grid dimensions are small")]
        let (width, height) = (
            size.cols as f32 * cell.width,
            size.rows as f32 * cell.height,
        );
        let inside = (0.0..width).contains(&x) && (0.0..height).contains(&y);
        if inside { self.cell_at(position) } else { None }
    }

    /// The viewport cells the hovered link's own characters take, row by
    /// row.
    fn hovered_link_cells(&self) -> Option<Vec<(usize, Range<usize>)>> {
        let link = self.hovered_link()?;
        Some(link_cells(
            &link,
            self.term.display_offset(),
            self.term.size(),
        ))
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
            cx.emit(PaneEvent::Copied { text });
        }
        cx.notify();
    }
}

/// The number of pastes this process has logged, shared by every pane, so a
/// paste's number names exactly one of them.
static PASTES: AtomicU64 = AtomicU64::new(0);

/// The next paste's number, counted from one for the process.
fn next_paste_number() -> u64 {
    PASTES.fetch_add(1, Ordering::Relaxed) + 1
}

/// The line a paste logs: what was sent and how big it was, never the text.
fn paste_log_line(
    n: u64,
    session: &str,
    chars: usize,
    bracketed: bool,
    sent_bytes: usize,
) -> String {
    format!(
        "paste#{n} session={session} chars={chars} bracketed={bracketed} sent_bytes={sent_bytes}"
    )
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
        let family = self.resolved_family(window);
        let font_settings = self.font.clone();
        let view = cx.entity();
        let gesture_view = view.clone();
        let exit_view = view.clone();
        let input_view = view.clone();
        let focus = self.focus.clone();
        let (grid_fill, ring_fill) = self.fills();
        let grid = canvas(
            move |bounds, window, cx| {
                let m = metrics(window, family, &font_settings);
                let (snap, marked, link, relaid) = view.update(cx, |v, _| {
                    let resized = v.ensure_size(grid_size(bounds, &m));
                    let cell = cell_size(&m);
                    let relaid = resized || v.layout.is_none_or(|(_, old)| old != cell);
                    v.layout = Some((bounds.origin, cell));
                    let snap = v.term.snapshot();
                    (snap, v.marked.clone(), v.hovered_link_cells(), relaid)
                });
                // The gutter was built before this layout, from the last one:
                // a resize moves the anchors and a new cell height the rows,
                // so the dots are rebuilt in another frame. A notify made
                // while the frame draws is dropped, so it waits for the draw.
                if relaid {
                    let pane = view.clone();
                    window.defer(cx, move |_, cx| pane.update(cx, |_, cx| cx.notify()));
                }
                let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
                (m, snap, marked, link, hitbox)
            },
            move |bounds, (m, snap, marked, link, hitbox), window, cx| {
                window.handle_input(&focus, ElementInputHandler::new(bounds, input_view), cx);
                if link.is_some() {
                    window.set_cursor_style(CursorStyle::PointingHand, &hitbox);
                }
                // Window-wide, so a drag keeps going once the pointer leaves
                // the pane, and a hovered link ends when it does.
                window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                    if phase == DispatchPhase::Bubble {
                        let over = hitbox.is_hovered(window);
                        gesture_view.update(cx, |pane, cx| {
                            pane.on_gesture_move(event, cx);
                            pane.on_hover_move(event, over, cx);
                        });
                    }
                });
                // Leaving the window sends no move, so the hovered cell
                // would otherwise outlive the pointer.
                window.on_mouse_event(move |_: &MouseExitEvent, phase, _, cx| {
                    if phase == DispatchPhase::Bubble {
                        exit_view.update(cx, TerminalPane::clear_hover);
                    }
                });
                window.paint_quad(fill(bounds, to_rgba(grid_fill)));
                paint_grid(
                    bounds,
                    &snap,
                    link.as_deref().unwrap_or_default(),
                    &m,
                    window,
                    cx,
                );
                if let Some(text) = marked {
                    paint_preedit(bounds.origin, &snap, &text, &m, window, cx);
                }
            },
        );
        let gutter = self.has_gutter().then(|| self.gutter());
        let mut pane = div()
            .size_full()
            .p(px(crate::PADDING))
            .bg(to_rgba(ring_fill))
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
        match gutter {
            Some(gutter) => pane
                .flex()
                .flex_row()
                .child(gutter)
                .child(grid.flex_1().h_full()),
            None => pane.child(grid.size_full()),
        }
    }
}

/// The dot of a finished command whose prompt is on viewport row
/// `dot.row`, with its exit and duration as its tooltip. A click on it is
/// taken by the gutter.
fn gutter_dot(pane_id: &str, n: usize, dot: ShellDot, line_height: f32) -> impl IntoElement {
    let selector = format!("shell-dot-{pane_id}-{n}");
    #[expect(clippy::cast_precision_loss, reason = "a viewport row is small")]
    let top = dot.row as f32 * line_height + (line_height - DOT) / 2.0;
    div()
        .id(SharedString::from(selector.clone()))
        .debug_selector(move || selector.clone())
        .absolute()
        .left(px(DOT_LEFT))
        .top(px(top))
        .size(px(DOT))
        .rounded_full()
        .bg(dot_color(dot.status))
        .border_1()
        .border_color(gpui::rgba(0x0000_0059))
        .tooltip(crate::tooltip(dot.tooltip))
}

/// Green for a command that exited 0, red for any other code, muted for one
/// the shell gave no code for.
fn dot_color(status: ShellStatus) -> Rgba {
    gpui::rgb(match status {
        ShellStatus::Ok => 0x004e_c9b0,
        ShellStatus::Fail => 0x00f4_8771,
        ShellStatus::Unknown => crate::MUTED,
    })
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
    font_size: Pixels,
    /// The font normal text draws in: the settings' family, at the bold
    /// weight when the settings ask for it.
    font: Font,
}

fn metrics(window: &Window, family: SharedString, settings: &FontSettings) -> Metrics {
    let font = Font {
        weight: if settings.bold {
            FontWeight::BOLD
        } else {
            FontWeight::NORMAL
        },
        ..font(family)
    };
    let text = window.text_system();
    let font_id = text.resolve_font(&font);
    let font_size = px(settings.size);
    let cell_width = text
        .advance(font_id, font_size, 'm')
        .map_or(px(settings.size * 0.6), |s| s.width);
    let ascent = text.ascent(font_id, font_size) / px(1.0);
    let descent = text.descent(font_id, font_size) / px(1.0);
    Metrics {
        cell_width,
        line_height: px(fonts::line_height(ascent, descent, settings.size)),
        font_size,
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

/// The viewport cells `link`'s own characters take, row by row. Rows out of
/// view are left out.
fn link_cells(
    link: &TerminalLink,
    display_offset: usize,
    size: GridSize,
) -> Vec<(usize, Range<usize>)> {
    let offset = i64::try_from(display_offset).unwrap_or(i64::MAX);
    link.segments
        .iter()
        .filter_map(|segment| {
            let row = usize::try_from(i64::from(segment.row).saturating_add(offset))
                .ok()
                .filter(|row| *row < size.rows)?;
            let end = segment.end_column.min(size.cols);
            (segment.start_column < end).then_some((row, segment.start_column..end))
        })
        .collect()
}

/// `link` is the hovered link's cells, underlined after the text.
fn paint_grid(
    bounds: Bounds<Pixels>,
    snap: &Snapshot,
    link: &[(usize, Range<usize>)],
    m: &Metrics,
    window: &mut Window,
    cx: &mut App,
) {
    paint_backgrounds(bounds.origin, &snap.bg, m, window);
    if let Some((row, col)) = snap.cursor {
        paint_cursor(
            cell_origin(bounds.origin, m, row, col),
            snap.cursor_shape,
            snap.caret,
            m,
            window,
        );
    }
    for span in &snap.text {
        if !span.text.trim().is_empty() {
            paint_span(bounds.origin, span, m, window, cx);
        }
    }
    paint_link_underline(bounds.origin, link, snap, m, window);
}

/// A 1px line along the bottom of the link's cells, in its text's colour.
fn paint_link_underline(
    origin: Point<Pixels>,
    cells: &[(usize, Range<usize>)],
    snap: &Snapshot,
    m: &Metrics,
    window: &mut Window,
) {
    for (row, cols) in cells {
        let at = cell_origin(origin, m, *row, cols.start);
        #[expect(clippy::cast_precision_loss, reason = "grid coordinates are small")]
        let width = m.cell_width * cols.len() as f32;
        window.paint_quad(fill(
            Bounds::new(
                point(at.x, at.y + m.line_height - px(1.0)),
                size(width, px(1.0)),
            ),
            to_rgba(text_color_at(snap, *row, cols.start)),
        ));
    }
}

/// The colour of the text in cell (`row`, `col`); the default foreground
/// over a blank.
fn text_color_at(snap: &Snapshot, row: usize, col: usize) -> alacritty_terminal::vte::ansi::Rgb {
    snap.text
        .iter()
        .find(|span| {
            span.row == row && span.col <= col && col < span.col + span.text.chars().count()
        })
        .map_or(snap.foreground, |span| span.fg)
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
fn paint_cursor(
    at: Point<Pixels>,
    shape: CursorShape,
    caret: alacritty_terminal::vte::ansi::Rgb,
    m: &Metrics,
    window: &mut Window,
) {
    const THICKNESS: f32 = 2.0;
    let color = to_rgba(caret);
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
                m.font.weight
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
        m.font_size,
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
        m.font_size,
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
    use std::convert::Infallible;
    use std::time::Instant;

    use super::{Attachment, ReplyVerdict, ShellStatus, SizeGate, dot_color, paste_log_line};
    use crate::term_input;

    fn attach(attachment: &mut Attachment, id: &str) {
        attachment.attach(id.to_owned(), Instant::now());
    }

    /// Whether a scrollback reply arriving now would be written.
    fn accept_scrollback(attachment: &mut Attachment) -> bool {
        attachment.load.as_mut().is_some_and(|load| {
            let verdict = load.on_reply(None, false, false, || Ok::<_, Infallible>(Vec::new()));
            matches!(verdict, Ok(ReplyVerdict::Accepted(_)))
        })
    }

    #[test]
    fn the_paste_log_line_carries_sizes_and_never_the_text() {
        assert_eq!(
            paste_log_line(3, "s1", 5, false, 5),
            "paste#3 session=s1 chars=5 bracketed=false sent_bytes=5"
        );

        let secret = "hunter2";
        let bracketed = term_input::paste_bytes(secret, true);
        assert_eq!(bracketed.len(), 19, "the wrapped paste, got {bracketed:?}");
        let line = paste_log_line(1, "s1", secret.chars().count(), true, bracketed.len());
        assert_eq!(
            line,
            "paste#1 session=s1 chars=7 bracketed=true sent_bytes=19"
        );
        assert!(!line.contains(secret), "never the text, got {line}");
    }

    #[test]
    fn dots_are_green_for_success_red_for_failure_and_muted_without_a_code() {
        assert_eq!(dot_color(ShellStatus::Ok), gpui::rgb(0x004e_c9b0));
        assert_eq!(dot_color(ShellStatus::Fail), gpui::rgb(0x00f4_8771));
        assert_eq!(dot_color(ShellStatus::Unknown), gpui::rgb(crate::MUTED));
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
