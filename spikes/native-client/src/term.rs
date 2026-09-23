//! Terminal state (`alacritty_terminal`) and the per-frame snapshot the view paints.

use alacritty_terminal::Term;
use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::{Config, TermMode};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor, Rgb};

/// Terminal replies (`Event::PtyWrite`, e.g. cursor-position reports) are dropped:
/// the Tauri app attached to the same session already answers them, and two
/// answers would reach the child.
pub struct Listener;
impl EventListener for Listener {}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct GridSize {
    pub cols: usize,
    pub rows: usize,
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

pub struct Terminal {
    term: Term<Listener>,
    parser: Processor,
    size: GridSize,
}

/// A horizontal run of cells sharing one style, painted as one shaped line.
pub struct TextSpan {
    pub row: usize,
    pub col: usize,
    pub text: String,
    pub fg: Rgb,
    /// Cell flags masked to [`SPAN_STYLE`]. A `WIDE_CHAR` span holds one
    /// two-cell glyph, shaped without the per-cell width lock.
    pub flags: Flags,
}

pub const SPAN_STYLE: Flags = Flags::BOLD
    .union(Flags::ITALIC)
    .union(Flags::UNDERLINE)
    .union(Flags::WIDE_CHAR);

pub struct BgSpan {
    pub row: usize,
    pub col: usize,
    pub len: usize,
    pub color: Rgb,
}

pub struct Snapshot {
    pub text: Vec<TextSpan>,
    pub bg: Vec<BgSpan>,
    pub cursor: Option<(usize, usize)>,
    pub background: Rgb,
}

impl Terminal {
    pub fn new(size: GridSize) -> Self {
        Self {
            term: Term::new(Config::default(), &size, Listener),
            parser: Processor::new(),
            size,
        }
    }

    pub fn size(&self) -> GridSize {
        self.size
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    pub fn resize(&mut self, size: GridSize) {
        self.size = size;
        self.term.resize(size);
    }

    pub fn scroll(&mut self, lines: i32) {
        self.term.scroll_display(Scroll::Delta(lines));
    }

    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
    }

    pub fn app_cursor(&self) -> bool {
        self.term.mode().contains(TermMode::APP_CURSOR)
    }

    pub fn bracketed_paste(&self) -> bool {
        self.term.mode().contains(TermMode::BRACKETED_PASTE)
    }

    pub fn snapshot(&self) -> Snapshot {
        let content = self.term.renderable_content();
        let colors = content.colors;
        let background = resolve(Color::Named(NamedColor::Background), colors);
        let offset = i32::try_from(content.display_offset).unwrap_or(i32::MAX);
        let mut builder = SpanBuilder::default();

        for indexed in content.display_iter {
            let Ok(row) = usize::try_from(indexed.point.line.0 + offset) else {
                continue;
            };
            let cell = indexed.cell;
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            let (mut fg, mut bg) = (cell.fg, cell.bg);
            if cell.flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            let bold = cell.flags.contains(Flags::BOLD);
            let fg = resolve(brighten(fg, bold), colors);
            let bg = resolve(bg, colors);
            let col = indexed.point.column.0;
            let wide = cell.flags.contains(Flags::WIDE_CHAR);
            builder.push_bg(row, col, if wide { 2 } else { 1 }, bg, background);
            let ch = if cell.flags.contains(Flags::HIDDEN) {
                ' '
            } else {
                cell.c
            };
            builder.push_text(TextSpan {
                row,
                col,
                text: ch.to_string(),
                fg,
                flags: cell.flags & SPAN_STYLE,
            });
        }

        let cursor_visible = content.mode.contains(TermMode::SHOW_CURSOR);
        let cursor = usize::try_from(content.cursor.point.line.0 + offset)
            .ok()
            .filter(|_| cursor_visible)
            .map(|row| (row, content.cursor.point.column.0));

        Snapshot {
            text: builder.text,
            bg: builder.bg,
            cursor,
            background,
        }
    }
}

#[derive(Default)]
struct SpanBuilder {
    text: Vec<TextSpan>,
    bg: Vec<BgSpan>,
}

impl SpanBuilder {
    fn push_text(&mut self, span: TextSpan) {
        if let Some(last) = self.text.last_mut()
            && !span.flags.contains(Flags::WIDE_CHAR)
            && last.flags == span.flags
            && last.row == span.row
            && last.col + last.text.chars().count() == span.col
            && last.fg == span.fg
        {
            last.text.push_str(&span.text);
            return;
        }
        self.text.push(span);
    }

    fn push_bg(&mut self, row: usize, col: usize, len: usize, color: Rgb, background: Rgb) {
        if color == background {
            return;
        }
        if let Some(last) = self.bg.last_mut()
            && last.row == row
            && last.col + last.len == col
            && last.color == color
        {
            last.len += len;
            return;
        }
        self.bg.push(BgSpan {
            row,
            col,
            len,
            color,
        });
    }
}

/// Bold text in the eight base colors renders in the bright variant, as xterm does.
fn brighten(color: Color, bold: bool) -> Color {
    match color {
        Color::Named(named) if bold => match u8::try_from(named as usize) {
            Ok(i) if i < 8 => Color::Indexed(i + 8),
            _ => color,
        },
        Color::Indexed(i) if bold && i < 8 => Color::Indexed(i + 8),
        other => other,
    }
}

fn resolve(color: Color, overrides: &Colors) -> Rgb {
    match color {
        Color::Spec(rgb) => rgb,
        Color::Indexed(i) => overrides[usize::from(i)].unwrap_or_else(|| default_indexed(i)),
        Color::Named(named) => overrides[named].unwrap_or_else(|| default_named(named)),
    }
}

#[expect(clippy::unreadable_literal, reason = "hex colors read as #rrggbb")]
fn default_named(named: NamedColor) -> Rgb {
    match named {
        NamedColor::Foreground | NamedColor::BrightForeground => rgb(0xd4d4d4),
        NamedColor::Background => rgb(0x1e1e1e),
        NamedColor::Cursor => rgb(0xaeafad),
        NamedColor::DimForeground => rgb(0x8a8a8a),
        other => match u8::try_from(other as usize) {
            Ok(i) if i < 16 => default_indexed(i),
            // Dim variants (DimBlack..DimWhite) follow the 16 base colors.
            _ => default_indexed(u8::try_from(other as usize % 8).unwrap_or(0)),
        },
    }
}

#[expect(clippy::unreadable_literal, reason = "hex colors read as #rrggbb")]
const BASE16: [u32; 16] = [
    0x000000, 0xcd3131, 0x0dbc79, 0xe5e510, 0x2472c8, 0xbc3fbc, 0x11a8cd, 0xe5e5e5, 0x666666,
    0xf14c4c, 0x23d18b, 0xf5f543, 0x3b8eea, 0xd670d6, 0x29b8db, 0xffffff,
];

fn default_indexed(i: u8) -> Rgb {
    match i {
        0..16 => rgb(BASE16[usize::from(i)]),
        16..232 => {
            let i = i - 16;
            let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            Rgb {
                r: level(i / 36),
                g: level((i / 6) % 6),
                b: level(i % 6),
            }
        }
        _ => {
            let v = 8 + (i - 232) * 10;
            Rgb { r: v, g: v, b: v }
        }
    }
}

fn rgb(hex: u32) -> Rgb {
    let [_, r, g, b] = hex.to_be_bytes();
    Rgb { r, g, b }
}
