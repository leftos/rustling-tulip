//! Terminal state (`alacritty_terminal`) and the per-frame snapshot the view paints.

use alacritty_terminal::Term;
use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::term::cell::{Cell, Flags};
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
            if let Ok(row) = usize::try_from(indexed.point.line.0 + offset) {
                builder.push_cell(
                    row,
                    indexed.point.column.0,
                    indexed.cell,
                    colors,
                    background,
                );
            }
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
    /// Adds one grid cell. Spacer cells behind a wide glyph carry nothing to paint.
    fn push_cell(&mut self, row: usize, col: usize, cell: &Cell, colors: &Colors, background: Rgb) {
        if cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        {
            return;
        }
        let (mut fg, mut bg) = (cell.fg, cell.bg);
        if cell.flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        let bold = cell.flags.contains(Flags::BOLD);
        let fg = resolve(brighten(fg, bold), colors);
        let bg = resolve(bg, colors);
        let wide = cell.flags.contains(Flags::WIDE_CHAR);
        self.push_bg(row, col, if wide { 2 } else { 1 }, bg, background);
        let ch = if cell.flags.contains(Flags::HIDDEN) {
            ' '
        } else {
            cell.c
        };
        self.push_text(TextSpan {
            row,
            col,
            text: ch.to_string(),
            fg,
            flags: cell.flags & SPAN_STYLE,
        });
    }

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
            // Dim variants (DimBlack..DimWhite) are contiguous, in base-colour order.
            _ => {
                let base = (other as usize).checked_sub(NamedColor::DimBlack as usize);
                default_indexed(base.and_then(|i| u8::try_from(i).ok()).unwrap_or(0))
            }
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

#[cfg(test)]
#[expect(clippy::unreadable_literal, reason = "hex colors read as #rrggbb")]
mod tests {
    use alacritty_terminal::term::cell::Flags;
    use alacritty_terminal::term::color::Colors;
    use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};

    use super::{GridSize, SpanBuilder, Terminal, TextSpan, brighten, default_named, resolve, rgb};

    const BACKGROUND: Rgb = Rgb {
        r: 0x1e,
        g: 0x1e,
        b: 0x1e,
    };

    fn span(col: usize, text: &str, fg: u32, flags: Flags) -> TextSpan {
        TextSpan {
            row: 0,
            col,
            text: text.to_owned(),
            fg: rgb(fg),
            flags,
        }
    }

    fn fed(bytes: &[u8]) -> Terminal {
        let mut term = Terminal::new(GridSize { cols: 10, rows: 2 });
        term.feed(bytes);
        term
    }

    #[test]
    fn adjacent_cells_with_same_style_merge_into_one_span() {
        let mut builder = SpanBuilder::default();
        builder.push_text(span(0, "a", 0xd4d4d4, Flags::empty()));
        builder.push_text(span(1, "b", 0xd4d4d4, Flags::empty()));
        assert_eq!(builder.text.len(), 1);
        assert_eq!(builder.text[0].text, "ab");
    }

    #[test]
    fn style_change_starts_new_span() {
        let mut builder = SpanBuilder::default();
        builder.push_text(span(0, "a", 0xd4d4d4, Flags::empty()));
        builder.push_text(span(1, "b", 0xcd3131, Flags::empty()));
        builder.push_text(span(2, "c", 0xcd3131, Flags::BOLD));
        builder.push_text(span(4, "d", 0xcd3131, Flags::BOLD));
        let texts: Vec<_> = builder.text.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, ["a", "b", "c", "d"]);
    }

    #[test]
    fn wide_char_spacer_is_skipped() {
        let snap = fed("a中b".as_bytes()).snapshot();
        let row0: Vec<_> = snap.text.iter().filter(|s| s.row == 0).collect();
        assert_eq!((row0[0].col, row0[0].text.as_str()), (0, "a"));
        assert_eq!((row0[1].col, row0[1].text.as_str()), (1, "中"));
        assert!(row0[1].flags.contains(Flags::WIDE_CHAR));
        assert_eq!(row0[2].col, 3);
        assert!(row0[2].text.starts_with('b'));
    }

    #[test]
    fn wide_cell_background_spans_two_columns() {
        let snap = fed("\x1b[41m中".as_bytes()).snapshot();
        let bg: Vec<_> = snap
            .bg
            .iter()
            .map(|b| (b.row, b.col, b.len, b.color))
            .collect();
        assert_eq!(bg, [(0, 0, 2, rgb(0xcd3131))]);
    }

    #[test]
    fn background_runs_merge_and_default_background_is_skipped() {
        let mut builder = SpanBuilder::default();
        builder.push_bg(0, 0, 1, BACKGROUND, BACKGROUND);
        builder.push_bg(0, 1, 1, rgb(0xcd3131), BACKGROUND);
        builder.push_bg(0, 2, 2, rgb(0xcd3131), BACKGROUND);
        builder.push_bg(0, 4, 1, rgb(0x0dbc79), BACKGROUND);
        let runs: Vec<_> = builder.bg.iter().map(|b| (b.col, b.len)).collect();
        assert_eq!(runs, [(1, 3), (4, 1)]);
    }

    #[test]
    fn named_colour_resolves_from_palette() {
        let mut colors = Colors::default();
        assert_eq!(
            resolve(Color::Named(NamedColor::Red), &colors),
            rgb(0xcd3131)
        );
        colors[NamedColor::Red] = Some(rgb(0x123456));
        assert_eq!(
            resolve(Color::Named(NamedColor::Red), &colors),
            rgb(0x123456)
        );
    }

    #[test]
    fn special_named_colours_have_theme_values() {
        assert_eq!(default_named(NamedColor::Cursor), rgb(0xaeafad));
        assert_eq!(default_named(NamedColor::DimForeground), rgb(0x8a8a8a));
        assert_eq!(default_named(NamedColor::BrightForeground), rgb(0xd4d4d4));
    }

    #[test]
    fn dim_colours_map_to_their_base_hue() {
        assert_eq!(default_named(NamedColor::DimBlack), rgb(0x000000));
        assert_eq!(default_named(NamedColor::DimRed), rgb(0xcd3131));
        assert_eq!(default_named(NamedColor::DimBlue), rgb(0x2472c8));
        assert_eq!(default_named(NamedColor::DimWhite), rgb(0xe5e5e5));
    }

    #[test]
    fn indexed_colour_resolves() {
        let mut colors = Colors::default();
        let idx = |i: u8, colors: &Colors| resolve(Color::Indexed(i), colors);
        assert_eq!(idx(1, &colors), rgb(0xcd3131));
        assert_eq!(idx(15, &colors), rgb(0xffffff));
        assert_eq!(idx(16, &colors), rgb(0x000000));
        assert_eq!(idx(17, &colors), rgb(0x00005f));
        assert_eq!(idx(21, &colors), rgb(0x0000ff));
        assert_eq!(idx(196, &colors), rgb(0xff0000));
        assert_eq!(idx(231, &colors), rgb(0xffffff));
        assert_eq!(idx(232, &colors), rgb(0x080808));
        assert_eq!(idx(255, &colors), rgb(0xeeeeee));
        colors[42] = Some(rgb(0x123456));
        assert_eq!(idx(42, &colors), rgb(0x123456));
    }

    #[test]
    fn truecolor_passes_through() {
        let spec = rgb(0x123456);
        assert_eq!(resolve(Color::Spec(spec), &Colors::default()), spec);
    }

    #[test]
    fn bold_base_colour_brightens() {
        assert_eq!(
            brighten(Color::Named(NamedColor::Red), true),
            Color::Indexed(9)
        );
        assert_eq!(brighten(Color::Indexed(2), true), Color::Indexed(10));
        assert_eq!(brighten(Color::Indexed(2), false), Color::Indexed(2));
        assert_eq!(brighten(Color::Indexed(9), true), Color::Indexed(9));
        let fg = Color::Named(NamedColor::Foreground);
        assert_eq!(brighten(fg, true), fg);
    }

    #[test]
    fn inverse_swaps_fg_and_bg() {
        let snap = fed(b"\x1b[7mX").snapshot();
        let x = snap.text.iter().find(|s| s.text.starts_with('X'));
        assert_eq!(x.map(|s| s.fg), Some(BACKGROUND));
        let bg: Vec<_> = snap
            .bg
            .iter()
            .map(|b| (b.row, b.col, b.len, b.color))
            .collect();
        assert_eq!(bg, [(0, 0, 1, rgb(0xd4d4d4))]);
    }

    #[test]
    fn hidden_cell_renders_blank() {
        let snap = fed(b"\x1b[8mX").snapshot();
        assert!(snap.text.iter().all(|s| !s.text.contains('X')));
    }

    #[test]
    fn default_fg_bg_resolve_to_theme_defaults() {
        let colors = Colors::default();
        assert_eq!(
            resolve(Color::Named(NamedColor::Foreground), &colors),
            rgb(0xd4d4d4)
        );
        assert_eq!(
            resolve(Color::Named(NamedColor::Background), &colors),
            BACKGROUND
        );
        assert_eq!(fed(b"").snapshot().background, BACKGROUND);
    }

    #[test]
    fn cursor_is_reported_only_while_visible() {
        assert_eq!(fed(b"ab").snapshot().cursor, Some((0, 2)));
        assert_eq!(fed(b"ab\x1b[?25l").snapshot().cursor, None);
    }
}
