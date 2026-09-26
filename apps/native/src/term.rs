//! Terminal state (`alacritty_terminal`) and the per-frame snapshot the view paints.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use alacritty_terminal::Term;
use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::{ClipboardType, Config, Osc52, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorShape, CursorStyle, NamedColor, Processor, Rgb};

use crate::links::{self, TerminalLink, TerminalRow};
use crate::theme::{self, Theme};

/// Collects what the terminal asks of its client: the replies to the child
/// (`Event::PtyWrite`: cursor-position reports, device attributes), and the
/// texts it wants on the clipboard (`Event::ClipboardStore`, OSC 52).
#[derive(Clone, Default)]
pub struct Listener {
    replies: Rc<RefCell<Vec<u8>>>,
    copies: Rc<RefCell<Vec<String>>>,
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        match event {
            Event::PtyWrite(text) => self.replies.borrow_mut().extend_from_slice(text.as_bytes()),
            // Only the clipboard target reaches the system clipboard: Windows
            // has no primary selection, and a program that keeps both in step
            // sends the same text to `c` and to `p`/`s`.
            Event::ClipboardStore(ClipboardType::Clipboard, text) if !text.is_empty() => {
                self.copies.borrow_mut().push(text);
            }
            // The program read the clipboard: the client keeps none in the
            // terminal, so it answers with an empty one.
            Event::ClipboardLoad(_, formatter) => {
                self.replies
                    .borrow_mut()
                    .extend_from_slice(formatter("").as_bytes());
            }
            _ => {}
        }
    }
}

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

/// How many lines of scrollback a session keeps.
pub const SCROLLBACK_LINES: usize = 5000;

/// How long a program's synchronized update (`DEC 2026`) may hold the bytes
/// it wrote back before the client writes them anyway.
pub const SYNC_TIMEOUT: Duration = Duration::from_millis(150);

pub struct Terminal {
    term: Term<Listener>,
    parser: Processor,
    size: GridSize,
    listener: Listener,
    /// The colours cells resolve against. A program's own palette changes
    /// (`OSC 4`, `OSC 10`) still win over it.
    theme: Theme,
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
    /// Where the cursor is (row, column), whether the program shows it or
    /// hides it.
    pub cursor_point: Option<(usize, usize)>,
    pub cursor_shape: CursorShape,
    pub background: Rgb,
    /// The default text colour, after the program's palette changes.
    pub foreground: Rgb,
    /// The colour the pane paints the caret in.
    pub caret: Rgb,
}

impl Terminal {
    /// A blank terminal whose cursor is `cursor` until the program sets its own.
    pub fn new(size: GridSize, cursor: CursorShape) -> Self {
        let config = Config {
            scrolling_history: SCROLLBACK_LINES,
            default_cursor_style: CursorStyle {
                shape: cursor,
                blinking: false,
            },
            // Both directions: a program may put text on the clipboard, and
            // read it back, which the pane answers with an empty one.
            osc52: Osc52::CopyPaste,
            ..Config::default()
        };
        let listener = Listener::default();
        Self {
            term: Term::new(config, &size, listener.clone()),
            parser: Processor::new(),
            size,
            listener,
            theme: Theme::default(),
        }
    }

    pub fn size(&self) -> GridSize {
        self.size
    }

    /// Rebuilds the theme for a new pane background.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "a pane rebuilds its theme when its background changes"
        )
    )]
    pub fn set_background(&mut self, background: Rgb) {
        self.theme = theme::build_theme(background);
    }

    /// Feeds live output. Replies it provokes wait in [`Self::take_replies`].
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    /// Feeds replayed history. The queries in it were answered when they were
    /// first made, so the replies they provoke now are discarded, and so are
    /// the clipboard stores: the copy happened when the line was written. A
    /// synchronized update the history left open is ended here, before its
    /// bytes could run later as if they had just arrived.
    pub fn feed_history(&mut self, bytes: &[u8]) {
        let queued = self.listener.replies.borrow().len();
        let stored = self.listener.copies.borrow().len();
        self.feed(bytes);
        if self.sync_pending() {
            self.parser.stop_sync(&mut self.term);
        }
        self.listener.replies.borrow_mut().truncate(queued);
        self.listener.copies.borrow_mut().truncate(stored);
    }

    /// The replies queued since the last call, to send to the child.
    pub fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut *self.listener.replies.borrow_mut())
    }

    /// The clipboard texts the program stored since the last call, oldest
    /// first.
    pub fn take_copies(&mut self) -> Vec<String> {
        std::mem::take(&mut *self.listener.copies.borrow_mut())
    }

    /// Whether a synchronized update (`DEC 2026`) is pending, holding the
    /// bytes written since it began back until it ends or the client times it
    /// out. The deadline vte records is on a real clock, which a test's clock
    /// cannot move, so the pane times the update itself.
    pub fn sync_pending(&self) -> bool {
        self.parser.sync_timeout().sync_timeout().is_some()
    }

    /// Ends a synchronized update whose timeout has passed, writing what it
    /// buffered to the grid.
    pub fn stop_sync(&mut self) {
        self.parser.stop_sync(&mut self.term);
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

    pub fn mode(&self) -> TermMode {
        *self.term.mode()
    }

    /// Lines scrolled back into history; 0 on the live screen.
    pub fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    pub fn start_selection(&mut self, ty: SelectionType, point: Point, side: Side) {
        self.term.selection = Some(Selection::new(ty, point, side));
    }

    pub fn update_selection(&mut self, point: Point, side: Side) {
        if let Some(selection) = self.term.selection.as_mut() {
            selection.update(point, side);
        }
    }

    pub fn clear_selection(&mut self) {
        self.term.selection = None;
    }

    pub fn has_selection(&self) -> bool {
        self.term.selection.as_ref().is_some_and(|s| !s.is_empty())
    }

    /// The selected text, or `None` when nothing is selected.
    pub fn selection_text(&self) -> Option<String> {
        self.term
            .selection_to_string()
            .filter(|text| !text.is_empty())
    }

    pub fn snapshot(&self) -> Snapshot {
        let content = self.term.renderable_content();
        let colors = content.colors;
        let theme = &self.theme;
        let background = resolve(Color::Named(NamedColor::Background), colors, theme);
        let offset = i32::try_from(content.display_offset).unwrap_or(i32::MAX);
        let palette = Palette {
            colors,
            theme,
            background,
        };
        let selection = content.selection;
        let mut builder = SpanBuilder::default();

        for indexed in content.display_iter {
            if let Ok(row) = usize::try_from(indexed.point.line.0 + offset) {
                let selected = selection.is_some_and(|range| range.contains(indexed.point));
                builder.push_cell(
                    row,
                    indexed.point.column.0,
                    indexed.cell,
                    &palette,
                    selected,
                );
            }
        }

        let cursor_visible = content.mode.contains(TermMode::SHOW_CURSOR);
        let cursor_point = usize::try_from(content.cursor.point.line.0 + offset)
            .ok()
            .map(|row| (row, content.cursor.point.column.0));
        let cursor = cursor_point.filter(|_| cursor_visible);

        Snapshot {
            text: builder.text,
            bg: builder.bg,
            cursor,
            cursor_point,
            cursor_shape: content.cursor.shape,
            background,
            foreground: resolve(Color::Named(NamedColor::Foreground), colors, theme),
            caret: resolve(Color::Named(NamedColor::Cursor), colors, theme),
        }
    }
}

/// The row reader the link detector works on. Every method here hands out
/// grid lines rather than screen rows, so a line above the viewport is a
/// negative one.
#[cfg_attr(not(test), expect(dead_code, reason = "only the tests call these"))]
impl Terminal {
    /// How far the walk for a hovered line reaches, in rows each way.
    const LINK_ROW_WINDOW: i32 = 64;

    /// The grid lines that exist: the oldest line of the history and the
    /// bottom line of the screen.
    fn line_bounds(&self) -> (i32, i32) {
        let grid = self.term.grid();
        (grid.topmost_line().0, grid.bottommost_line().0)
    }

    /// The links spanning grid line `line`, stitched from the rows around it.
    ///
    /// The walk reaches [`Self::LINK_ROW_WINDOW`] rows back from `line`, and
    /// as many forward, independently, while each step is a soft wrap or a
    /// hard stitch. A link touching a window edge the walk only stopped at
    /// because it ran out of window is dropped rather than offered cut short.
    /// The rows of what comes back are grid lines, so a link in the history
    /// carries a negative one.
    #[must_use]
    pub fn detect_links_near(&self, line: i32) -> Vec<TerminalLink> {
        self.links_near(line, Self::LINK_ROW_WINDOW)
    }

    /// [`Self::detect_links_near`] with the window cap named, so that a test
    /// can reach a window edge on a small grid.
    fn links_near(&self, line: i32, cap: i32) -> Vec<TerminalLink> {
        let (top, bottom) = self.line_bounds();
        if line < top || line > bottom {
            return Vec::new();
        }
        let cols = self.size.cols;
        let mut above = vec![self.read_row(line)];
        let mut first = line;
        while first > top && line - first < cap {
            let previous = self.read_row(first - 1);
            let Some(current) = above.last() else {
                break;
            };
            if !current.is_wrapped && !links::can_stitch(&previous, current, cols) {
                break;
            }
            above.push(previous);
            first -= 1;
        }
        let cut_above = first > top && line - first >= cap;

        let mut below = Vec::new();
        let mut last = line;
        while last < bottom && last - line < cap {
            let next = self.read_row(last + 1);
            let Some(current) = below.last().or_else(|| above.last()) else {
                break;
            };
            if !next.is_wrapped && !links::can_stitch(current, &next, cols) {
                break;
            }
            below.push(next);
            last += 1;
        }
        let cut_below = last < bottom && last - line >= cap;

        above.reverse();
        let hovered = line - first;
        let edge = i32::try_from(above.len() + below.len())
            .unwrap_or(i32::MAX)
            .saturating_sub(1);
        let rows: Vec<TerminalRow> = above.into_iter().chain(below).collect();
        let mut links = links::detect_row_links(&rows, cols);
        links.retain(|link| {
            let spanning = link.start_row <= hovered && link.end_row >= hovered;
            let cut = (cut_above && link.start_row == 0) || (cut_below && link.end_row == edge);
            spanning && !cut
        });
        for link in &mut links {
            link.start_row += first;
            link.end_row += first;
        }
        links
    }

    /// One grid line as a [`TerminalRow`]: the cell of every column holding a
    /// glyph, in column order, with the spacer cell behind a wide glyph left
    /// out and every cell's column kept in the row's map.
    fn read_row(&self, line: i32) -> TerminalRow {
        let grid = self.term.grid();
        let mut row = TerminalRow {
            text: String::with_capacity(grid.columns()),
            columns: Vec::with_capacity(grid.columns()),
            is_wrapped: self.wraps(line),
        };
        for column in 0..grid.columns() {
            let cell = &grid[Line(line)][Column(column)];
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            row.text.push(cell.c);
            row.columns.push(links::column_limit(column));
        }
        row
    }

    /// Whether `line` continues the line above it. The grid marks a wrap on
    /// the last cell of the row that wrapped.
    fn wraps(&self, line: i32) -> bool {
        let (top, _) = self.line_bounds();
        if line <= top {
            return false;
        }
        let grid = self.term.grid();
        grid[Line(line - 1)][grid.last_column()]
            .flags
            .contains(Flags::WRAPLINE)
    }
}

/// `tint` at `alpha` over `base`, each channel rounded.
fn blend(base: Rgb, tint: Rgb, alpha: f64) -> Rgb {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "alpha is a small non-negative fraction"
    )]
    let weight = (alpha * 1000.0).round() as u32;
    let channel = |b: u8, t: u8| {
        let v = (u32::from(b) * (1000 - weight) + u32::from(t) * weight + 500) / 1000;
        u8::try_from(v).unwrap_or(u8::MAX)
    };
    Rgb {
        r: channel(base.r, tint.r),
        g: channel(base.g, tint.g),
        b: channel(base.b, tint.b),
    }
}

/// The colours cells resolve against.
struct Palette<'a> {
    colors: &'a Colors,
    theme: &'a Theme,
    /// The background `snapshot()` resolved, a program's `OSC 11` included: a
    /// cell painted its colour needs no span of its own.
    background: Rgb,
}

#[derive(Default)]
struct SpanBuilder {
    text: Vec<TextSpan>,
    bg: Vec<BgSpan>,
}

impl SpanBuilder {
    /// Adds one grid cell. Spacer cells behind a wide glyph carry nothing to paint.
    fn push_cell(
        &mut self,
        row: usize,
        col: usize,
        cell: &Cell,
        palette: &Palette,
        selected: bool,
    ) {
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
        let (fg, bg) = if selected {
            (
                palette.theme.selection_fg,
                blend(
                    resolve(bg, palette.colors, palette.theme),
                    palette.theme.selection,
                    palette.theme.selection_alpha,
                ),
            )
        } else {
            (
                resolve(brighten(fg, bold), palette.colors, palette.theme),
                resolve(bg, palette.colors, palette.theme),
            )
        };
        let wide = cell.flags.contains(Flags::WIDE_CHAR);
        self.push_bg(row, col, if wide { 2 } else { 1 }, bg, palette.background);
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

fn resolve(color: Color, overrides: &Colors, theme: &Theme) -> Rgb {
    match color {
        Color::Spec(rgb) => rgb,
        Color::Indexed(i) => overrides[usize::from(i)].unwrap_or_else(|| default_indexed(i, theme)),
        Color::Named(named) => overrides[named].unwrap_or_else(|| default_named(named, theme)),
    }
}

fn default_named(named: NamedColor, theme: &Theme) -> Rgb {
    match named {
        NamedColor::Foreground | NamedColor::BrightForeground => theme.fg,
        NamedColor::Background => theme.bg,
        NamedColor::Cursor => theme.caret,
        // Dim text sits between the foreground and the background.
        NamedColor::DimForeground => theme::mix(theme.fg, theme.bg, 0.35),
        other => match u8::try_from(other as usize) {
            Ok(i) if i < 16 => default_indexed(i, theme),
            // Dim variants (DimBlack..DimWhite) are contiguous, in base-colour order.
            _ => {
                let base = (other as usize).checked_sub(NamedColor::DimBlack as usize);
                default_indexed(base.and_then(|i| u8::try_from(i).ok()).unwrap_or(0), theme)
            }
        },
    }
}

fn default_indexed(i: u8, theme: &Theme) -> Rgb {
    match i {
        0..16 => theme.ansi[usize::from(i)],
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

/// A `#rrggbb` literal as a colour, for the tests' expected values.
#[cfg(test)]
fn rgb(hex: u32) -> Rgb {
    let [_, r, g, b] = hex.to_be_bytes();
    Rgb { r, g, b }
}

#[cfg(test)]
#[expect(clippy::unreadable_literal, reason = "hex colors read as #rrggbb")]
mod tests {
    use alacritty_terminal::index::{Column, Line, Point, Side};
    use alacritty_terminal::selection::SelectionType;
    use alacritty_terminal::term::cell::Flags;
    use alacritty_terminal::term::color::Colors;
    use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor};

    use crate::theme::Theme;

    use super::{GridSize, SpanBuilder, Terminal, TextSpan, brighten, default_named, resolve, rgb};

    fn default_theme() -> Theme {
        Theme::default()
    }

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
        let mut term = Terminal::new(GridSize { cols: 10, rows: 2 }, CursorShape::Block);
        term.feed(bytes);
        term
    }

    fn sized(bytes: &[u8], cols: usize, rows: usize) -> Terminal {
        let mut term = Terminal::new(GridSize { cols, rows }, CursorShape::Block);
        term.feed(bytes);
        term
    }

    /// The text of grid line `line`, trailing blanks trimmed.
    fn row_text(term: &Terminal, line: i32) -> String {
        term.read_row(line).text.trim_end().to_owned()
    }

    #[test]
    fn scrollback_keeps_five_thousand_lines() {
        let mut term = Terminal::new(GridSize { cols: 10, rows: 3 }, CursorShape::Block);
        term.feed("x\r\n".repeat(6000).as_bytes());
        assert_eq!(term.line_bounds(), (-5000, 2), "history is capped");
    }

    #[test]
    fn a_utf8_sequence_split_across_feeds_still_lands() {
        let mut term = fed(b"");
        let bytes = "中".as_bytes();
        term.feed(&bytes[..2]);
        term.feed(&bytes[2..]);
        assert_eq!(row_text(&term, 0), "中");
    }

    #[test]
    fn a_utf8_sequence_split_between_history_and_live_still_lands() {
        let mut term = fed(b"");
        let bytes = "中".as_bytes();
        term.feed_history(&bytes[..1]);
        term.feed(&bytes[1..]);
        assert_eq!(row_text(&term, 0), "中");
    }

    #[test]
    fn a_pending_sync_holds_output_until_it_is_stopped() {
        let mut term = fed(b"\x1b[?2026hhello");
        assert_eq!(row_text(&term, 0), "", "held back");
        assert!(term.sync_pending(), "a sync is pending");
        term.stop_sync();
        assert_eq!(row_text(&term, 0), "hello");
        assert!(!term.sync_pending());
    }

    #[test]
    fn an_ended_sync_writes_its_bytes_and_leaves_none_pending() {
        let term = fed(b"\x1b[?2026hhello\x1b[?2026l");
        assert!(!term.sync_pending());
        assert_eq!(row_text(&term, 0), "hello");
    }

    #[test]
    fn a_sync_left_open_by_history_never_fires_later() {
        let mut term = fed(b"");
        term.feed_history(b"\x1b[?2026h\x1b]52;c;aGk=\x07\x1b[c");
        assert!(term.take_copies().is_empty(), "the history copies nothing");
        assert!(
            term.take_replies().is_empty(),
            "the history answers nothing"
        );
        assert!(!term.sync_pending(), "the history leaves no sync pending");

        // The queries and stores it held back do not run when a later
        // synchronized update writes its own bytes.
        term.feed(b"\x1b[?2026hx\x1b[?2026l");
        assert!(term.take_copies().is_empty(), "no copy ran later");
        assert!(term.take_replies().is_empty(), "no reply ran later");
        assert_eq!(row_text(&term, 0), "x");
    }

    #[test]
    fn osc52_store_to_the_selection_target_is_ignored() {
        let mut term = fed(b"\x1b]52;p;aGk=\x07");
        assert!(term.take_copies().is_empty(), "only `c` reaches the chip");
        assert!(term.take_replies().is_empty());
    }

    #[test]
    fn osc52_store_is_collected() {
        let mut term = fed(b"\x1b]52;c;aGk=\x07");
        assert_eq!(term.take_copies(), ["hi".to_owned()]);
        assert!(term.take_copies().is_empty());
        assert!(term.take_replies().is_empty(), "a store provokes no reply");
    }

    #[test]
    fn osc52_store_in_history_is_discarded() {
        let mut term = fed(b"");
        term.feed_history(b"\x1b]52;c;aGk=\x07");
        assert!(term.take_copies().is_empty());
        term.feed(b"\x1b]52;c;aGk=\x07");
        assert_eq!(term.take_copies(), ["hi".to_owned()]);
    }

    #[test]
    fn osc52_load_is_answered_with_an_empty_clipboard() {
        let mut term = fed(b"\x1b]52;c;?\x07");
        assert_eq!(term.take_replies(), b"\x1b]52;c;\x07".to_vec());
        assert!(term.take_copies().is_empty());
    }

    #[test]
    fn selected_cells_are_highlighted() {
        let theme = default_theme();
        let mut term = fed(b"abcd");
        term.start_selection(
            SelectionType::Simple,
            Point::new(Line(0), Column(0)),
            Side::Left,
        );
        term.update_selection(Point::new(Line(0), Column(1)), Side::Right);
        let snap = term.snapshot();
        let bg: Vec<_> = snap
            .bg
            .iter()
            .map(|b| (b.row, b.col, b.len, b.color))
            .collect();
        // The selection tint over the default background at 30%: each channel
        // of #08090b moved 30% of the way to #5b9bff, rounded.
        assert_eq!(bg, [(0, 0, 2, rgb(0x213554))]);
        let texts: Vec<_> = snap
            .text
            .iter()
            .map(|s| (s.col, s.text.as_str(), s.fg))
            .collect();
        // Selected text is drawn in the theme's foreground — the selection's
        // own foreground is that same colour — so the row is one span and the
        // tint asserted above is what marks the selection.
        assert_eq!(theme.selection_fg, theme.fg);
        assert_eq!(texts[0], (0, "abcd      ", theme.fg));
        assert_eq!(term.selection_text().as_deref(), Some("ab"));
    }

    #[test]
    fn cursor_position_query_queues_a_reply() {
        let mut term = fed(b"ab\x1b[6n");
        assert_eq!(term.take_replies(), b"\x1b[1;3R".to_vec());
        assert!(term.take_replies().is_empty());
    }

    #[test]
    fn replies_to_replayed_history_are_discarded() {
        let mut term = fed(b"");
        term.feed_history(b"\x1b[6n");
        assert!(term.take_replies().is_empty());
        term.feed(b"\x1b[6n");
        assert_eq!(term.take_replies(), b"\x1b[1;1R".to_vec());
    }

    #[test]
    fn default_cursor_shape_yields_to_the_program() {
        let term = Terminal::new(GridSize { cols: 10, rows: 2 }, CursorShape::Beam);
        assert_eq!(term.snapshot().cursor_shape, CursorShape::Beam);
        assert_eq!(
            fed(b"\x1b[4 q").snapshot().cursor_shape,
            CursorShape::Underline
        );
    }

    #[test]
    fn adjacent_cells_with_same_style_merge_into_one_span() {
        let mut builder = SpanBuilder::default();
        builder.push_text(span(0, "a", 0xe5e6e8, Flags::empty()));
        builder.push_text(span(1, "b", 0xe5e6e8, Flags::empty()));
        assert_eq!(builder.text.len(), 1);
        assert_eq!(builder.text[0].text, "ab");
    }

    #[test]
    fn style_change_starts_new_span() {
        let mut builder = SpanBuilder::default();
        builder.push_text(span(0, "a", 0xe5e6e8, Flags::empty()));
        builder.push_text(span(1, "b", 0xef5c5c, Flags::empty()));
        builder.push_text(span(2, "c", 0xef5c5c, Flags::BOLD));
        builder.push_text(span(4, "d", 0xef5c5c, Flags::BOLD));
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
        assert_eq!(bg, [(0, 0, 2, default_theme().ansi[1])]);
    }

    #[test]
    fn background_runs_merge_and_default_background_is_skipped() {
        let theme = default_theme();
        let mut builder = SpanBuilder::default();
        builder.push_bg(0, 0, 1, theme.bg, theme.bg);
        builder.push_bg(0, 1, 1, theme.ansi[1], theme.bg);
        builder.push_bg(0, 2, 2, theme.ansi[1], theme.bg);
        builder.push_bg(0, 4, 1, theme.ansi[2], theme.bg);
        let runs: Vec<_> = builder.bg.iter().map(|b| (b.col, b.len)).collect();
        assert_eq!(runs, [(1, 3), (4, 1)]);
    }

    #[test]
    fn named_colour_resolves_from_palette() {
        let theme = default_theme();
        let mut colors = Colors::default();
        assert_eq!(
            resolve(Color::Named(NamedColor::Red), &colors, &theme),
            theme.ansi[1]
        );
        colors[NamedColor::Red] = Some(rgb(0x123456));
        assert_eq!(
            resolve(Color::Named(NamedColor::Red), &colors, &theme),
            rgb(0x123456)
        );
    }

    #[test]
    fn special_named_colours_have_theme_values() {
        let theme = default_theme();
        assert_eq!(default_named(NamedColor::Cursor, &theme), theme.caret);
        assert_eq!(
            default_named(NamedColor::BrightForeground, &theme),
            theme.fg
        );
        // Dim text sits 35% of the way from the foreground to the background.
        assert_eq!(
            default_named(NamedColor::DimForeground, &theme),
            rgb(0x98999b)
        );
    }

    #[test]
    fn dim_colours_map_to_their_base_hue() {
        let theme = default_theme();
        assert_eq!(default_named(NamedColor::DimBlack, &theme), theme.ansi[0]);
        assert_eq!(default_named(NamedColor::DimRed, &theme), theme.ansi[1]);
        assert_eq!(default_named(NamedColor::DimBlue, &theme), theme.ansi[4]);
        assert_eq!(default_named(NamedColor::DimWhite, &theme), theme.ansi[7]);
    }

    #[test]
    fn indexed_colour_resolves() {
        let theme = default_theme();
        let mut colors = Colors::default();
        let idx = |i: u8, colors: &Colors| resolve(Color::Indexed(i), colors, &theme);
        assert_eq!(idx(1, &colors), theme.ansi[1]);
        assert_eq!(idx(15, &colors), theme.ansi[15]);
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
        assert_eq!(
            resolve(Color::Spec(spec), &Colors::default(), &default_theme()),
            spec
        );
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
        let theme = default_theme();
        let snap = fed(b"\x1b[7mX").snapshot();
        let x = snap.text.iter().find(|s| s.text.starts_with('X'));
        assert_eq!(x.map(|s| s.fg), Some(theme.bg));
        let bg: Vec<_> = snap
            .bg
            .iter()
            .map(|b| (b.row, b.col, b.len, b.color))
            .collect();
        assert_eq!(bg, [(0, 0, 1, theme.fg)]);
    }

    #[test]
    fn hidden_cell_renders_blank() {
        let snap = fed(b"\x1b[8mX").snapshot();
        assert!(snap.text.iter().all(|s| !s.text.contains('X')));
    }

    #[test]
    fn default_fg_bg_resolve_to_theme_defaults() {
        let theme = default_theme();
        let colors = Colors::default();
        assert_eq!(
            resolve(Color::Named(NamedColor::Foreground), &colors, &theme),
            theme.fg
        );
        assert_eq!(
            resolve(Color::Named(NamedColor::Background), &colors, &theme),
            theme.bg
        );
        assert_eq!(theme.bg, rgb(0x08090b));
        assert_eq!(fed(b"").snapshot().background, theme.bg);
        assert_eq!(fed(b"").snapshot().caret, theme.caret);
    }

    #[test]
    fn a_new_background_rebuilds_the_theme() {
        let mut term = fed(b"\x1b[47mX");
        assert_eq!(term.snapshot().bg[0].color, rgb(0xe5e6e8));
        term.set_background(rgb(0xf6f4ef));
        let snap = term.snapshot();
        // The Paper theme lowers the base white to clear its contrast
        // minimum, and would not be reached if the theme were not rebuilt.
        assert_eq!(snap.bg[0].color, rgb(0x87888a));
        assert_eq!(snap.background, rgb(0xf6f4ef));
        assert_eq!(snap.foreground, rgb(0x1a1c22));
        assert_eq!(snap.caret, rgb(0x7f7f7f));
    }

    #[test]
    fn a_program_background_resolves_over_the_theme_for_every_cell() {
        // `OSC 11` repaints the pane, so a plain cell needs no span to sit on
        // it.
        let plain = fed(b"\x1b]11;#ffffff\x07ab").snapshot();
        assert_eq!(plain.background, rgb(0xffffff));
        assert!(plain.bg.is_empty());
        // A cell painted the theme's own background is still a span against
        // the program's.
        let painted = fed(b"\x1b]11;#ffffff\x07\x1b[48;2;8;9;11mX").snapshot();
        let bg: Vec<_> = painted
            .bg
            .iter()
            .map(|b| (b.row, b.col, b.len, b.color))
            .collect();
        assert_eq!(bg, [(0, 0, 1, rgb(0x08090b))]);
    }

    #[test]
    fn a_program_cursor_colour_wins_over_the_theme() {
        assert_eq!(fed(b"\x1b]12;#ff0000\x07").snapshot().caret, rgb(0xff0000));
    }

    #[test]
    fn cursor_is_reported_only_while_visible() {
        assert_eq!(fed(b"ab").snapshot().cursor, Some((0, 2)));
        assert_eq!(fed(b"ab\x1b[?25l").snapshot().cursor, None);
    }

    #[test]
    fn a_soft_wrap_is_marked_on_the_continuation() {
        let term = sized(b"X:/dev/project/long/name/file.ts\r\none\r\ntwo", 20, 3);
        assert_eq!(term.line_bounds(), (-1, 2));
        let head = term.read_row(-1);
        assert!(!head.is_wrapped);
        assert_eq!(head.text, "X:/dev/project/long/");
        let tail = term.read_row(0);
        assert!(tail.is_wrapped);
        assert_eq!(tail.text, "name/file.ts        ");
        let links = term.detect_links_near(-1);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target, "X:/dev/project/long/name/file.ts");
        assert_eq!(links[0].start_row, -1);
        assert_eq!(links[0].end_row, 0);
    }

    #[test]
    fn links_are_found_from_either_row_of_a_wrap() {
        let term = sized(b"X:/dev/project/long/name/file.ts", 20, 3);
        let target = "X:/dev/project/long/name/file.ts";
        let first = term.detect_links_near(0);
        let second = term.detect_links_near(1);
        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        assert_eq!(first[0].target, target);
        assert_eq!(second[0].target, target);
        assert_eq!(first[0].start_row, 0);
        assert_eq!(first[0].end_row, 1);
        assert_eq!(second[0].start_row, 0);
        assert_eq!(second[0].end_row, 1);
        assert_eq!(first[0].start_column, 0);
        assert_eq!(first[0].end_column, 12);
    }

    #[test]
    fn a_wide_glyph_before_a_path_keeps_its_column() {
        let term = sized("中 X:/a/b.rs:3".as_bytes(), 20, 3);
        let row = term.read_row(0);
        assert_eq!(row.text, "中 X:/a/b.rs:3      ");
        assert_eq!(row.columns[2], 3);
        let links = term.detect_links_near(0);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target, "X:/a/b.rs");
        assert_eq!(links[0].line, Some(3));
        assert_eq!(links[0].start_row, 0);
        assert_eq!(links[0].end_row, 0);
        assert_eq!(links[0].start_column, 3);
        assert_eq!(links[0].end_column, 14);
    }

    #[test]
    fn a_wide_glyph_wrapping_to_the_next_row_adds_no_phantom_char() {
        let term = sized("X:/dir/aaaaaaaaaaaa中.rs".as_bytes(), 20, 4);
        let head = term.read_row(0);
        assert!(!head.is_wrapped);
        assert_eq!(head.text, "X:/dir/aaaaaaaaaaaa");
        let tail = term.read_row(1);
        assert!(tail.is_wrapped);
        assert_eq!(tail.text, format!("中.rs{}", " ".repeat(15)));
        let links = term.detect_links_near(0);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target, "X:/dir/aaaaaaaaaaaa中.rs");
        assert_eq!(links[0].start_row, 0);
        assert_eq!(links[0].end_row, 1);
        assert_eq!(links[0].start_column, 0);
        assert_eq!(links[0].end_column, 5);
    }

    #[test]
    fn a_line_scrolled_into_history_is_readable() {
        let term = sized(b"X:/a/b.rs\r\nalpha\r\nbravo\r\ncharlie", 20, 3);
        assert_eq!(term.line_bounds(), (-1, 2));
        assert!(term.detect_links_near(9).is_empty());
        let row = term.read_row(-1);
        assert_eq!(row.text, "X:/a/b.rs           ");
        let links = term.detect_links_near(-1);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target, "X:/a/b.rs");
        assert_eq!(links[0].start_row, -1);
        assert_eq!(links[0].end_row, -1);
    }

    #[test]
    fn a_link_cut_by_the_window_cap_is_not_offered() {
        let term = sized(format!("X:/dir/{}", "a".repeat(73)).as_bytes(), 20, 5);
        let whole = term.links_near(0, 64);
        assert_eq!(whole.len(), 1);
        assert_eq!(whole[0].start_row, 0);
        assert_eq!(whole[0].end_row, 3);
        assert!(term.links_near(0, 2).is_empty());
    }
}
