//! Native (GPUI) rustling-tulip client: attaches to one session of a running
//! daemon and renders it with `alacritty_terminal`.
//!
//! Usage: `rustling-tulip-native [session-id]`. Without an id it attaches to the
//! first live interactive or plain-shell session the daemon lists. Logs go to
//! stderr, filtered by `RUST_LOG` (default `info`).

mod keys;
mod net;
mod term;

use alacritty_terminal::term::cell::Flags;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use futures::StreamExt as _;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use gpui::{
    App, Application, Bounds, Context, FocusHandle, Font, FontStyle, FontWeight, KeyDownEvent,
    Pixels, Point, Rgba, ScrollWheelEvent, SharedString, TextRun, UnderlineStyle, Window,
    WindowBounds, WindowOptions, canvas, div, fill, font, point, prelude::*, px, size,
};
use protocol::{ClientMessage, DaemonMessage, SessionMode, SessionSnapshot, SessionStatus};
use tracing_subscriber::EnvFilter;

use crate::net::NetEvent;
use crate::term::{BgSpan, GridSize, Snapshot, Terminal, TextSpan};

const FONT_FAMILY: &str = "Cascadia Mono";
const FONT_SIZE: f32 = 14.0;
const LINE_HEIGHT: f32 = 18.0;
const PADDING: f32 = 6.0;

struct TerminalView {
    term: Terminal,
    focus: FocusHandle,
    tx: UnboundedSender<ClientMessage>,
    wanted_session: Option<String>,
    session_id: Option<String>,
    /// PTY output that arrived before the scrollback reply; fed after it.
    pending: Vec<Vec<u8>>,
    scrollback_loaded: bool,
    status: String,
    scroll_accum: f32,
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
            wanted_session,
            session_id: None,
            pending: Vec::new(),
            scrollback_loaded: false,
            status: "connecting…".to_owned(),
            scroll_accum: 0.0,
        }
    }

    fn send(&self, msg: ClientMessage) {
        // Fails only once the network thread has exited; `on_net` already reported why.
        let _ = self.tx.unbounded_send(msg);
    }

    fn on_net(&mut self, event: NetEvent, cx: &mut Context<Self>) {
        match event {
            NetEvent::Closed(reason) => self.status = format!("disconnected: {reason}"),
            NetEvent::Message(msg) => self.on_message(*msg),
        }
        cx.notify();
    }

    fn on_message(&mut self, msg: DaemonMessage) {
        match msg {
            DaemonMessage::Welcome {
                protocol_version, ..
            } => {
                self.status = format!("connected (protocol v{protocol_version})");
                self.send(ClientMessage::ListSessions);
            }
            DaemonMessage::AuthFailed { reason } => self.status = format!("auth failed: {reason}"),
            DaemonMessage::Sessions { sessions } if self.session_id.is_none() => {
                self.pick_session(&sessions);
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
            DaemonMessage::SessionRemoved { session_id } if self.is_ours(&session_id) => {
                "session removed".clone_into(&mut self.status);
            }
            _ => {}
        }
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
        if ks.modifiers.control && ks.modifiers.shift && ks.key == "v" {
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

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(background)
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .child(
                div()
                    .px(px(PADDING))
                    .py(px(2.0))
                    .text_size(px(12.0))
                    .text_color(gpui::rgb(0x009a_9a9a))
                    .bg(gpui::rgb(0x0025_2526))
                    .child(self.status.clone()),
            )
            .child(div().flex_1().p(px(PADDING)).child(grid))
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

fn main() {
    init_tracing();
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
