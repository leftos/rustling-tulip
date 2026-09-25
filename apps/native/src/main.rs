//! Native (GPUI) rustling-tulip client: attaches to one session of a running
//! daemon and renders it with `alacritty_terminal`.
//!
//! Usage: `rustling-tulip-native [session-id]`. Without an id it attaches to the
//! first live interactive or plain-shell session the daemon lists. It starts
//! the daemon when none is running and reconnects when the connection drops.
//! Logs go to stderr and to `<config dir>/logs/native.log` (the previous
//! launch's log kept as `native.log.old`), filtered by `RUST_LOG` (default
//! `info`).

mod connection;
mod footer;
mod keys;
mod net;
mod term;

use alacritty_terminal::term::cell::Flags;
use anyhow::Context as _;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use futures::StreamExt as _;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use gpui::{
    Animation, AnimationExt as _, AnyElement, AnyView, App, Application, Bounds, ClickEvent,
    ClipboardItem, Context, Div, FocusHandle, Font, FontStyle, FontWeight, KeyDownEvent,
    MouseDownEvent, Pixels, Point, Rgba, ScrollWheelEvent, SharedString, Stateful, TextRun,
    UnderlineStyle, Window, WindowBounds, WindowOptions, canvas, div, fill, font, point,
    prelude::*, pulsating_between, px, size,
};
use protocol::{ClientMessage, DaemonMessage, SessionMode, SessionSnapshot, SessionStatus};
use std::collections::HashSet;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

use crate::connection::{Connection, DotKind, Footer};
use crate::footer::{LogPaths, StopConfirm, flyout_rows, log_paths};
use crate::net::{HandshakeInfo, NetCommand, NetEvent};
use crate::term::{BgSpan, GridSize, Snapshot, Terminal, TextSpan};

const FONT_FAMILY: &str = "Cascadia Mono";
const FONT_SIZE: f32 = 14.0;
const LINE_HEIGHT: f32 = 18.0;
const PADDING: f32 = 6.0;
/// This client's log file, under `<config dir>/logs/`.
const LOG_FILE: &str = "native.log";

/// Text size of the footer, flyout and overlay.
const UI_TEXT_SIZE: f32 = 12.0;
const FOOTER_HEIGHT: f32 = 22.0;
const BAR_BG: u32 = 0x0025_2526;
const PANEL_BG: u32 = 0x000f_1014;
const OVERLAY_BG: u32 = 0x001e_1e1e;
const HOVER_BG: u32 = 0x002d_2f36;
const BORDER: u32 = 0x0020_222a;
const TEXT: u32 = 0x00cc_cccc;
const MUTED: u32 = 0x009a_9a9a;
const DANGER: u32 = 0x00ef_5c5c;
const DANGER_BG: u32 = 0x003a_1c1f;

struct TerminalView {
    term: Terminal,
    focus: FocusHandle,
    tx: UnboundedSender<NetCommand>,
    /// The connection state machine as the network thread last reported it.
    conn: Connection,
    handshake: Option<HandshakeInfo>,
    /// Every session the daemon knows, for the session count.
    session_ids: HashSet<String>,
    wanted_session: Option<String>,
    session_id: Option<String>,
    /// PTY output that arrived before the scrollback reply; fed after it.
    pending: Vec<Vec<u8>>,
    scrollback_loaded: bool,
    /// The attached session, or why none is.
    status: String,
    scroll_accum: f32,
    /// Whether the daemon troubleshooting flyout is open.
    flyout_open: bool,
    /// The flyout's two-click stop.
    stop: StopConfirm,
    /// The handshake path was copied since the flyout opened.
    copied: bool,
    /// The flyout's files, or why the config dir could not be resolved.
    paths: Result<LogPaths, String>,
}

impl TerminalView {
    fn new(wanted_session: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (out_tx, out_rx) = unbounded();
        let (in_tx, mut in_rx) = unbounded();
        net::spawn(out_rx, in_tx);
        cx.spawn(async move |this, cx| {
            while let Some(event) = in_rx.next().await {
                if this.update(cx, |view, cx| view.on_net(event, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        let focus = cx.focus_handle();
        focus.focus(window);
        Self {
            term: Terminal::new(GridSize { cols: 80, rows: 24 }),
            focus,
            tx: out_tx,
            conn: Connection::new(),
            handshake: None,
            session_ids: HashSet::new(),
            wanted_session,
            session_id: None,
            pending: Vec::new(),
            scrollback_loaded: false,
            status: String::new(),
            scroll_accum: 0.0,
            flyout_open: false,
            stop: StopConfirm::default(),
            copied: false,
            paths: daemon_client::config_dir()
                .map(|dir| log_paths(&dir))
                .map_err(|err| format!("config folder unavailable: {err:#}")),
        }
    }

    fn send(&self, msg: ClientMessage) {
        self.command(NetCommand::Send(Box::new(msg)));
    }

    fn command(&self, command: NetCommand) {
        // Fails only once the network thread has exited, which it does only
        // after this view drops its sender.
        let _ = self.tx.unbounded_send(command);
    }

    fn toggle_flyout(&mut self) {
        if self.flyout_open {
            self.close_flyout();
        } else {
            self.flyout_open = true;
        }
    }

    fn close_flyout(&mut self) {
        self.flyout_open = false;
        self.stop.reset();
        self.copied = false;
    }

    /// Restart the daemon. The old daemon's handshake is dropped so the
    /// flyout never shows its port, pid or protocol; the next ensure brings
    /// the new one.
    fn restart(&mut self) {
        self.command(NetCommand::Restart);
        self.handshake = None;
        self.close_flyout();
    }

    fn click_stop(&mut self) {
        if self.stop.click() {
            self.command(NetCommand::Stop);
            self.handshake = None;
            self.close_flyout();
        }
    }

    fn on_net(&mut self, event: NetEvent, cx: &mut Context<Self>) {
        match event {
            NetEvent::State(conn) => self.conn = conn,
            NetEvent::Handshake(info) => self.handshake = Some(info),
            NetEvent::Message(msg) => self.on_message(*msg),
        }
        // The overlay covers the footer; a flyout left open under it would
        // reappear (possibly armed) when the overlay goes.
        if self.conn.overlay().is_some() {
            self.close_flyout();
        }
        cx.notify();
    }

    fn on_message(&mut self, msg: DaemonMessage) {
        match msg {
            DaemonMessage::Welcome { .. } => self.reset_attachment(),
            DaemonMessage::Sessions { sessions } => {
                self.session_ids = sessions.iter().map(|s| s.id.clone()).collect();
                if self.session_id.is_none() {
                    self.pick_session(&sessions);
                }
            }
            DaemonMessage::SessionUpdated { session } => {
                self.session_ids.insert(session.id);
            }
            DaemonMessage::Scrollback {
                session_id,
                data_b64,
                ..
            } if self.is_ours(&session_id) => self.on_scrollback(&data_b64),
            DaemonMessage::PtyOutput {
                session_id,
                data_b64,
            } if self.is_ours(&session_id) => self.on_pty_output(&data_b64),
            DaemonMessage::SessionRemoved { session_id } => {
                self.session_ids.remove(&session_id);
                if self.is_ours(&session_id) {
                    "session removed".clone_into(&mut self.status);
                }
            }
            _ => {}
        }
    }

    /// A new connection starts unattached: the daemon pushes `Sessions` after
    /// `Welcome`, and the view picks again, preferring the session it had and
    /// replaying its scrollback into a fresh terminal.
    fn reset_attachment(&mut self) {
        if let Some(id) = self.session_id.take() {
            self.wanted_session = Some(id);
            self.term = Terminal::new(self.term.size());
        }
        self.pending.clear();
        self.scrollback_loaded = false;
        self.status.clear();
    }

    fn on_scrollback(&mut self, data_b64: &str) {
        self.feed_b64(data_b64);
        self.scrollback_loaded = true;
        for chunk in std::mem::take(&mut self.pending) {
            self.term.feed(&chunk);
        }
    }

    fn on_pty_output(&mut self, data_b64: &str) {
        if self.scrollback_loaded {
            self.feed_b64(data_b64);
        } else if let Ok(bytes) = B64.decode(data_b64) {
            self.pending.push(bytes);
        }
    }

    fn is_ours(&self, id: &str) -> bool {
        self.session_id.as_deref() == Some(id)
    }

    fn pick_session(&mut self, sessions: &[SessionSnapshot]) {
        let live = |s: &&SessionSnapshot| {
            matches!(s.mode, SessionMode::Interactive | SessionMode::PlainShell)
                && !matches!(s.status, SessionStatus::Stopped | SessionStatus::Error)
                && !s.is_orphan
                && !s.is_abandoned
        };
        let chosen = match &self.wanted_session {
            Some(id) => sessions.iter().find(|s| &s.id == id),
            None => sessions.iter().find(live),
        };
        let Some(session) = chosen else {
            self.status = match &self.wanted_session {
                Some(id) => format!("session {id} not found"),
                None => "no live interactive session to attach to".to_owned(),
            };
            return;
        };
        let label = session
            .user_label
            .clone()
            .unwrap_or_else(|| session.label.clone());
        self.status = format!("{label} · {}", session.id);
        self.session_id = Some(session.id.clone());
        self.send(ClientMessage::LoadScrollback {
            session_id: session.id.clone(),
        });
        self.send_resize();
    }

    fn feed_b64(&mut self, data_b64: &str) {
        match B64.decode(data_b64) {
            Ok(bytes) => self.term.feed(&bytes),
            Err(err) => self.status = format!("bad base64 from daemon: {err}"),
        }
    }

    fn send_input(&mut self, bytes: &[u8]) {
        let Some(session_id) = self.session_id.clone() else {
            return;
        };
        self.term.scroll_to_bottom();
        self.send(ClientMessage::SendInput {
            session_id,
            data_b64: B64.encode(bytes),
        });
    }

    fn send_resize(&self) {
        let Some(session_id) = self.session_id.clone() else {
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
        if size != self.term.size() {
            self.term.resize(size);
            self.send_resize();
        }
    }

    fn on_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let ks = &event.keystroke;
        if self.flyout_open && ks.key == "escape" {
            self.close_flyout();
        } else if ks.modifiers.control && ks.modifiers.shift && ks.key == "v" {
            self.paste(cx);
        } else if let Some(bytes) = keys::to_bytes(ks, self.term.app_cursor()) {
            self.send_input(&bytes);
        } else {
            return;
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        if self.term.bracketed_paste() {
            self.send_input(format!("\x1b[200~{text}\x1b[201~").as_bytes());
        } else {
            self.send_input(text.as_bytes());
        }
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
            self.term.scroll(lines as i32);
            cx.notify();
        }
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
        paint_cursor(cell_origin(bounds.origin, m, row, col), m, window);
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

fn paint_cursor(at: Point<Pixels>, m: &Metrics, window: &mut Window) {
    let cursor = Rgba {
        a: 0.55,
        ..to_rgba(alacritty_terminal::vte::ansi::Rgb {
            r: 0xae,
            g: 0xaf,
            b: 0xad,
        })
    };
    window.paint_quad(fill(
        Bounds::new(at, size(m.cell_width, m.line_height)),
        cursor,
    ));
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

impl Render for TerminalView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity();
        let background = to_rgba(self.term.snapshot().background);
        let grid = canvas(
            move |bounds, window, cx| {
                let m = metrics(window);
                let snap = view.update(cx, |v, _| {
                    v.ensure_size(grid_size(bounds, &m));
                    v.term.snapshot()
                });
                (m, snap)
            },
            |bounds, (m, snap), window, cx| paint_grid(bounds, &snap, &m, window, cx),
        )
        .size_full();
        let footer = self.conn.footer(self.session_ids.len());
        let flyout = self
            .flyout_open
            .then(|| [flyout_backdrop(cx).into_any_element(), self.flyout(cx)]);
        let overlay = self
            .conn
            .overlay()
            .map(|text| connecting_overlay(text, footer.dot, cx));

        div()
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(background)
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .child(div().flex_1().p(px(PADDING)).child(grid))
            .child(self.footer_bar(&footer, cx))
            .children(flyout.into_iter().flatten())
            .children(overlay)
    }
}

impl TerminalView {
    /// The bottom bar: the daemon pill, then the attached session's status.
    fn footer_bar(&self, footer: &Footer, cx: &mut Context<Self>) -> Div {
        let text = match footer.port {
            Some(port) => format!("daemon · {} · :{port}", footer.label),
            None => format!("daemon · {}", footer.label),
        };
        let pill = div()
            .id("daemon-pill")
            .flex()
            .items_center()
            .gap(px(6.0))
            .h_full()
            .px(px(8.0))
            .cursor_pointer()
            .hover(|style| style.bg(gpui::rgb(HOVER_BG)))
            .when(self.flyout_open, |pill| pill.bg(gpui::rgb(HOVER_BG)))
            .child(status_dot(footer.dot, "footer-dot"))
            .child(text)
            .tooltip(tooltip(footer.tooltip.clone()))
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.toggle_flyout();
                cx.notify();
            }));
        div()
            .flex()
            .flex_none()
            .items_center()
            .h(px(FOOTER_HEIGHT))
            .bg(gpui::rgb(BAR_BG))
            .border_t_1()
            .border_color(gpui::rgb(BORDER))
            .text_size(px(UI_TEXT_SIZE))
            .text_color(gpui::rgb(MUTED))
            .child(pill)
            .child(div().px(px(PADDING)).child(self.status.clone()))
    }

    /// The troubleshooting flyout above the pill: details, files, control.
    fn flyout(&self, cx: &mut Context<Self>) -> AnyElement {
        let rows = flyout_rows(&self.conn, self.handshake.as_ref(), self.session_ids.len());
        let details = div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .children(
                rows.into_iter()
                    .map(|(label, value)| detail_row(label, value)),
            )
            .child(self.handshake_row(cx));
        div()
            .id("daemon-flyout")
            .absolute()
            .left(px(PADDING))
            .bottom(px(FOOTER_HEIGHT + 4.0))
            .w(px(320.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .p(px(10.0))
            .bg(gpui::rgb(PANEL_BG))
            .border_1()
            .border_color(gpui::rgb(BORDER))
            .rounded(px(6.0))
            .text_size(px(UI_TEXT_SIZE))
            .text_color(gpui::rgb(TEXT))
            .occlude()
            .child(div().font_weight(FontWeight::SEMIBOLD).child("Daemon"))
            .child(details)
            .child(self.files_section())
            .child(self.control_section(cx))
            .into_any_element()
    }

    /// The "Handshake file" row with its copy button.
    fn handshake_row(&self, cx: &mut Context<Self>) -> Div {
        let label = if self.copied { "copied" } else { "copy" };
        let button = action_button("copy-handshake", label, TEXT, self.paths.is_ok());
        let button = match &self.paths {
            Ok(paths) => {
                let path = paths.handshake.display().to_string();
                button.tooltip(tooltip(path.clone())).on_click(cx.listener(
                    move |this, _: &ClickEvent, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(path.clone()));
                        this.copied = true;
                        cx.notify();
                    },
                ))
            }
            Err(_) => button,
        };
        detail_row("Handshake file", button)
    }

    /// "Logs & files": open the logs, reveal the config folder; disabled with
    /// the error when the config dir could not be resolved.
    fn files_section(&self) -> Div {
        let section = section("Logs & files");
        match &self.paths {
            Ok(paths) => section
                .child(open_button(
                    "open-daemon-log",
                    "Open daemon.log",
                    &paths.daemon_log,
                ))
                .child(open_button(
                    "open-native-log",
                    "Open native.log",
                    &paths.native_log,
                ))
                .child({
                    let dir = paths.config_dir.clone();
                    action_button("reveal-config", "Reveal config folder", TEXT, true)
                        .on_click(move |_, _, cx| cx.reveal_path(&dir))
                }),
            Err(err) => section
                .child(action_button(
                    "open-daemon-log",
                    "Open daemon.log",
                    TEXT,
                    false,
                ))
                .child(action_button(
                    "open-native-log",
                    "Open native.log",
                    TEXT,
                    false,
                ))
                .child(action_button(
                    "reveal-config",
                    "Reveal config folder",
                    TEXT,
                    false,
                ))
                .child(div().text_color(gpui::rgb(DANGER)).child(err.clone())),
        }
    }

    /// "Control": restart, and the two-click stop.
    fn control_section(&self, cx: &mut Context<Self>) -> Div {
        let armed = self.stop.armed;
        section("Control")
            .child(
                action_button("restart-daemon", "Restart daemon", TEXT, true).on_click(
                    cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.restart();
                        cx.notify();
                    }),
                ),
            )
            .child(
                action_button("stop-daemon", self.stop.label(), DANGER, true)
                    .when(armed, |button| button.bg(gpui::rgb(DANGER_BG)))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.click_stop();
                        cx.notify();
                    })),
            )
    }
}

/// A transparent full-window layer under the flyout; a click on it (the pill
/// included) closes the flyout.
fn flyout_backdrop(cx: &mut Context<TerminalView>) -> Stateful<Div> {
    div()
        .id("flyout-backdrop")
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .occlude()
        .on_any_mouse_down(cx.listener(|this, _: &MouseDownEvent, _, cx| {
            this.close_flyout();
            cx.stop_propagation();
            cx.notify();
        }))
}

/// The full-window card shown until the first connect, with a restart link.
fn connecting_overlay(
    text: &'static str,
    dot: DotKind,
    cx: &mut Context<TerminalView>,
) -> Stateful<Div> {
    let restart = div()
        .id("overlay-restart")
        .text_size(px(UI_TEXT_SIZE))
        .text_color(gpui::rgb(MUTED))
        .cursor_pointer()
        .hover(|style| style.text_color(gpui::rgb(TEXT)))
        .child("Restart daemon")
        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
            this.restart();
            cx.notify();
        }));
    let card = div()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(16.0))
        .px(px(40.0))
        .py(px(32.0))
        .min_w(px(280.0))
        .bg(gpui::rgb(PANEL_BG))
        .border_1()
        .border_color(gpui::rgb(BORDER))
        .rounded(px(6.0))
        .child(status_dot(dot, "overlay-dot"))
        .child(div().text_color(gpui::rgb(TEXT)).child(text))
        .child(restart);
    div()
        .id("connecting-overlay")
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(gpui::rgb(OVERLAY_BG))
        .occlude()
        .child(card)
}

/// The status dot: pulsing while pending, dimmed when stopped.
fn status_dot(dot: DotKind, id: &'static str) -> AnyElement {
    let base = div()
        .flex_none()
        .size(px(8.0))
        .rounded_full()
        .bg(gpui::rgb(dot_color(dot)));
    match dot {
        DotKind::Pending => base
            .with_animation(
                id,
                Animation::new(Duration::from_millis(1400))
                    .repeat()
                    .with_easing(pulsating_between(0.35, 1.0)),
                Styled::opacity,
            )
            .into_any_element(),
        DotKind::Stopped => base.opacity(0.55).into_any_element(),
        DotKind::Ok | DotKind::Idle | DotKind::Err => base.into_any_element(),
    }
}

/// The footer dot's colour, from the Tauri app's status tokens.
fn dot_color(dot: DotKind) -> u32 {
    match dot {
        DotKind::Ok => 0x003f_b96a,
        DotKind::Pending => 0x00e8_a531,
        DotKind::Idle | DotKind::Stopped => 0x0083_8a96,
        DotKind::Err => 0x00ef_5c5c,
    }
}

/// A flyout detail row: a muted label, the value on the right.
fn detail_row(label: &'static str, value: impl IntoElement) -> Div {
    div()
        .flex()
        .justify_between()
        .items_center()
        .gap(px(12.0))
        .child(div().text_color(gpui::rgb(MUTED)).child(label))
        .child(value)
}

/// A flyout section: a top border and a muted heading.
fn section(label: &'static str) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .pt(px(6.0))
        .border_t_1()
        .border_color(gpui::rgb(BORDER))
        .child(div().text_color(gpui::rgb(MUTED)).child(label))
}

/// A clickable text button; a disabled one is dimmed and takes no clicks.
fn action_button(
    id: &'static str,
    label: &'static str,
    color: u32,
    enabled: bool,
) -> Stateful<Div> {
    div()
        .id(id)
        .px(px(6.0))
        .py(px(3.0))
        .rounded(px(4.0))
        .text_color(gpui::rgb(color))
        .when(enabled, |button| {
            button
                .cursor_pointer()
                .hover(|style| style.bg(gpui::rgb(HOVER_BG)))
        })
        .when(!enabled, |button| button.opacity(0.5))
        .child(label)
}

/// A button that opens `path` with the system's default application.
fn open_button(id: &'static str, label: &'static str, path: &Path) -> Stateful<Div> {
    let path: PathBuf = path.to_path_buf();
    action_button(id, label, TEXT, true).on_click(move |_, _, cx| cx.open_with_system(&path))
}

/// A plain text tooltip.
struct Tip(SharedString);

impl Render for Tip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(6.0))
            .py(px(2.0))
            .bg(gpui::rgb(PANEL_BG))
            .border_1()
            .border_color(gpui::rgb(BORDER))
            .rounded(px(4.0))
            .text_size(px(UI_TEXT_SIZE))
            .text_color(gpui::rgb(TEXT))
            .child(self.0.clone())
    }
}

fn tooltip(text: impl Into<SharedString>) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let text = text.into();
    move |_, cx| cx.new(|_| Tip(text.clone())).into()
}

/// Move a non-empty `native.log` to `native.log.old`, replacing any older
/// generation, so each launch logs to a fresh file and the previous launch's
/// log survives one more launch. Mirrors the Tauri host's `app.log` rotation.
fn rotate_log(path: &Path) -> std::io::Result<()> {
    if std::fs::metadata(path).is_ok_and(|m| m.len() > 0) {
        let old = path.with_extension("log.old");
        // Windows rename fails when the target exists; drop the older
        // generation first (best-effort — rename reports the definitive
        // error).
        let _ = std::fs::remove_file(&old);
        std::fs::rename(path, &old)?;
    }
    Ok(())
}

/// Rotate and open `<config dir>/logs/native.log` for this launch.
fn open_log_file() -> anyhow::Result<File> {
    let dir = daemon_client::config_dir()?.join("logs");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(LOG_FILE);
    rotate_log(&path).with_context(|| format!("rotating {}", path.display()))?;
    std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))
}

/// Log to stderr and to `native.log`; stderr alone when the file cannot be
/// opened.
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let (file_layer, file_error) = match open_log_file() {
        Ok(file) => (
            Some(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(Mutex::new(file)),
            ),
            None,
        ),
        Err(err) => (None, Some(err)),
    };
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .with(file_layer)
        .init();
    if let Some(err) = file_error {
        tracing::warn!("logging to stderr only: {err:#}");
    }
}

/// Log every panic, the network thread's included, through tracing so it
/// reaches `native.log`, then run the default hook.
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        tracing::error!(
            thread = thread.name().unwrap_or("<unnamed>"),
            "panic: {info}"
        );
        default_hook(info);
    }));
}

fn main() {
    init_tracing();
    install_panic_hook();
    let wanted_session = std::env::args().nth(1);
    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1000.0), px(640.0)), cx);
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            move |window, cx| cx.new(|cx| TerminalView::new(wanted_session, window, cx)),
        );
        if let Err(err) = opened {
            tracing::error!("opening window: {err:#}");
            cx.quit();
            return;
        }
        cx.activate(true);
    });
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::rotate_log;
    use std::path::Path;

    #[test]
    fn rotate_moves_log_to_old() {
        let scratch = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("workspace root above apps/native")
            .join(".tmp")
            .join(format!("native-rotate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch).expect("create scratch dir");
        let log = scratch.join("native.log");
        let old = scratch.join("native.log.old");
        std::fs::write(&old, "two launches ago").expect("write older log");
        std::fs::write(&log, "last launch").expect("write log");

        rotate_log(&log).expect("rotate");

        assert!(!log.exists(), "native.log should have moved");
        let moved = std::fs::read_to_string(&old).expect("read native.log.old");
        let _ = std::fs::remove_dir_all(&scratch);
        assert_eq!(moved, "last launch");
    }
}
