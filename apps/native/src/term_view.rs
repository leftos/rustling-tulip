//! The terminal pane: one daemon session rendered with `alacritty_terminal`,
//! with its input, resize, scroll and scrollback handling.

use alacritty_terminal::term::cell::Flags;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use futures::channel::mpsc::UnboundedSender;
use gpui::{
    App, Bounds, Context, FocusHandle, Font, FontStyle, FontWeight, KeyDownEvent, Pixels, Point,
    Rgba, ScrollWheelEvent, SharedString, TextRun, UnderlineStyle, Window, canvas, div, fill, font,
    point, prelude::*, px, size,
};
use protocol::ClientMessage;

use crate::keys;
use crate::net::NetCommand;
use crate::term::{BgSpan, GridSize, Snapshot, Terminal, TextSpan};

const FONT_FAMILY: &str = "Cascadia Mono";
const FONT_SIZE: f32 = 14.0;
const LINE_HEIGHT: f32 = 18.0;

pub struct TerminalPane {
    term: Terminal,
    focus: FocusHandle,
    tx: UnboundedSender<NetCommand>,
    attachment: Attachment,
    /// PTY output that arrived before the scrollback reply; fed after it.
    pending: Vec<Vec<u8>>,
    scroll_accum: f32,
}

/// The session the pane shows, and whether its scrollback has been fed.
#[derive(Debug, Default)]
struct Attachment {
    session_id: Option<String>,
    scrollback_loaded: bool,
}

impl Attachment {
    fn attach(&mut self, session_id: String) {
        self.session_id = Some(session_id);
        self.scrollback_loaded = false;
    }

    fn take(&mut self) -> Option<String> {
        self.scrollback_loaded = false;
        self.session_id.take()
    }

    /// Whether to feed a scrollback reply for the attached session: only the
    /// first one after an attach. A later one answers an earlier attach of
    /// the same session and would replay its history over the live screen.
    fn accept_scrollback(&mut self) -> bool {
        if self.session_id.is_none() || self.scrollback_loaded {
            return false;
        }
        self.scrollback_loaded = true;
        true
    }
}

impl TerminalPane {
    pub fn new(
        tx: UnboundedSender<NetCommand>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        focus.focus(window);
        Self {
            term: Terminal::new(GridSize { cols: 80, rows: 24 }),
            focus,
            tx,
            attachment: Attachment::default(),
            pending: Vec::new(),
            scroll_accum: 0.0,
        }
    }

    pub fn session_id(&self) -> Option<&str> {
        self.attachment.session_id.as_deref()
    }

    pub fn focus(&self, window: &mut Window) {
        self.focus.focus(window);
    }

    pub fn is_focused(&self, window: &Window) -> bool {
        self.focus.is_focused(window)
    }

    /// Show `session_id` in a fresh terminal: load its scrollback, then size
    /// its PTY to this pane.
    pub fn attach(&mut self, session_id: String) {
        self.term = Terminal::new(self.term.size());
        self.pending.clear();
        self.attachment.attach(session_id.clone());
        self.send(ClientMessage::LoadScrollback { session_id });
        self.send_resize();
    }

    /// Stop receiving the attached session's output, if one is attached.
    pub fn detach(&mut self) {
        if let Some(session_id) = self.attachment.take() {
            self.send(ClientMessage::Detach { session_id });
        }
        self.pending.clear();
    }

    /// Forget the attachment for a new connection, which starts unattached,
    /// and return the session that was attached.
    pub fn reset_for_reconnect(&mut self) -> Option<String> {
        let previous = self.attachment.take();
        if previous.is_some() {
            self.term = Terminal::new(self.term.size());
        }
        self.pending.clear();
        previous
    }

    pub fn on_scrollback(&mut self, data_b64: &str) -> Result<(), base64::DecodeError> {
        if !self.attachment.accept_scrollback() {
            return Ok(());
        }
        let fed = self.feed_b64(data_b64);
        for chunk in std::mem::take(&mut self.pending) {
            self.term.feed(&chunk);
        }
        fed
    }

    pub fn on_pty_output(&mut self, data_b64: &str) -> Result<(), base64::DecodeError> {
        if self.attachment.scrollback_loaded {
            self.feed_b64(data_b64)
        } else {
            if let Ok(bytes) = B64.decode(data_b64) {
                self.pending.push(bytes);
            }
            Ok(())
        }
    }

    fn feed_b64(&mut self, data_b64: &str) -> Result<(), base64::DecodeError> {
        let bytes = B64.decode(data_b64)?;
        self.term.feed(&bytes);
        Ok(())
    }

    fn send(&self, msg: ClientMessage) {
        // Fails only once the network thread has exited, which it does only
        // after every sender has dropped.
        let _ = self.tx.unbounded_send(NetCommand::Send(Box::new(msg)));
    }

    fn send_input(&mut self, bytes: &[u8]) {
        let Some(session_id) = self.attachment.session_id.clone() else {
            return;
        };
        self.term.scroll_to_bottom();
        self.send(ClientMessage::SendInput {
            session_id,
            data_b64: B64.encode(bytes),
        });
    }

    fn send_resize(&self) {
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

impl Render for TerminalPane {
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
            .p(px(crate::PADDING))
            .bg(background)
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .child(grid)
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

#[cfg(test)]
mod tests {
    use super::Attachment;

    #[test]
    fn second_scrollback_for_the_same_attach_is_ignored() {
        let mut attachment = Attachment::default();
        attachment.attach("a".to_owned());
        assert!(attachment.accept_scrollback());
        assert!(!attachment.accept_scrollback());
    }

    #[test]
    fn scrollback_after_reattach_is_accepted() {
        let mut attachment = Attachment::default();
        attachment.attach("a".to_owned());
        assert!(attachment.accept_scrollback());
        attachment.take();
        attachment.attach("b".to_owned());
        attachment.take();
        attachment.attach("a".to_owned());
        assert!(attachment.accept_scrollback());
    }

    #[test]
    fn scrollback_without_an_attachment_is_ignored() {
        let mut attachment = Attachment::default();
        assert!(!attachment.accept_scrollback());
        attachment.attach("a".to_owned());
        attachment.take();
        assert!(!attachment.accept_scrollback());
    }
}
