//! Terminal state (`alacritty_terminal`) and the per-frame snapshot the view paints.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::RangeInclusive;
use std::rc::Rc;
use std::time::{Duration, Instant};

use alacritty_terminal::Term;
use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Grid, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::{ClipboardType, Config, Osc52, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorShape, CursorStyle, NamedColor, Processor, Rgb};

use crate::Clock;
use crate::links::{self, TerminalLink, TerminalRow};
use crate::shell_marks::{Record, Records, Scanner, ShellDot, Split};
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
    /// The shell integration marks, for a plain shell's pane only.
    shell: Option<Shell>,
}

/// The bytes a plain shell's output is fed in, the cap enforced after
/// each. Slicing bounds the rows the common case pushes into the history
/// between two checks well below the headroom above the cap; a slice can
/// still overrun it (`CSI n S` with a large `n`, a storm of `2J`), and that
/// saturation drops every record by design.
const FEED_SLICE: usize = 4096;

/// A plain shell's command records and what anchors them to rows.
///
/// A row's absolute number is `base + history_size + line`: `base` counts
/// the rows evicted from the top of the history. alacritty's own history
/// limit sits at twice the cap plus a screen, so while it is never reached
/// its history size counts every row pushed exactly; the cap is enforced
/// here after each slice, with the evicted rows added to `base`.
///
/// Only rows pushed into the history move the anchors. A scroll inside the
/// screen (a reverse index at the top row, `CSI T`, an insert or delete of
/// lines, a scroll region whose top is below the first row) moves text
/// without moving the anchors, so a dot there can sit beside another row.
struct Shell {
    scanner: Scanner,
    records: Records,
    base: u64,
    /// The history the pane keeps.
    cap: usize,
    /// The history alacritty may hold before it drops rows itself.
    limit: usize,
    /// When a live mark arrived.
    clock: Clock,
    /// The anchors' logical positions, taken as the alternate screen was
    /// entered: a resize while it is up reflows the primary screen unseen.
    alt_snapshot: Option<Vec<(u64, Logical)>>,
    alt_resized: bool,
}

/// Where a row sits among the logical (unwrapped) lines: the line's index
/// counted from the cursor's line (older lines positive), and the row
/// within it counted from its top. A column change reflows the rows but
/// keeps both.
#[derive(Clone, Copy, Debug)]
struct Logical {
    index: isize,
    row: usize,
}

impl Shell {
    fn in_alt(term: &Term<Listener>) -> bool {
        term.mode().contains(TermMode::ALT_SCREEN)
    }

    /// Acts on a split the scanner found before its final byte runs.
    fn before(&mut self, term: &mut Term<Listener>, split: &Split) {
        let alt = Self::in_alt(term);
        match split {
            Split::ClearHistory if !alt => {
                self.settle(term);
                self.base += count(term.grid().history_size());
                self.records.evict_below(self.base);
            }
            Split::Reset => {
                if !alt {
                    self.base += count(term.grid().history_size());
                }
                self.records.clear();
                self.alt_snapshot = None;
                self.alt_resized = false;
            }
            Split::AltEnter if !alt => {
                self.settle(term);
                let anchors = self.records.anchors();
                self.alt_snapshot = Some(logical_positions(term.grid(), self.base, &anchors));
                self.alt_resized = false;
            }
            _ => {}
        }
    }

    /// Acts on a split the scanner found, after its bytes have run.
    fn after(&mut self, term: &mut Term<Listener>, split: Split, at: Option<Instant>) {
        if Self::in_alt(term) {
            return;
        }
        match split {
            Split::Mark(mark) => {
                let grid = term.grid();
                let line = grid.cursor.point.line.0;
                let abs = abs_of(self.base, grid.history_size(), line);
                self.records
                    .apply(mark, abs, grid.cursor.point.column.0, at);
            }
            Split::AltExit => {
                let snapshot = self.alt_snapshot.take();
                if std::mem::take(&mut self.alt_resized)
                    && let Some(snapshot) = snapshot
                {
                    self.relocate(term.grid(), &snapshot);
                    self.trim(term);
                }
            }
            _ => {}
        }
    }

    /// Enforces the cap after output (see [`Self::trim`]). Output that
    /// filled the history to alacritty's own limit may have pushed rows out
    /// uncounted, so every record goes.
    fn settle(&mut self, term: &mut Term<Listener>) {
        if Self::in_alt(term) {
            return;
        }
        if term.grid().history_size() >= self.limit {
            self.records.clear();
        }
        self.trim(term);
    }

    /// Enforces the cap on the primary screen's history, counting the rows
    /// it drops into `base`. After a reflow, which can cut the history to
    /// alacritty's limit itself, the relocation has already dropped exactly
    /// the anchors on the rows cut and numbered the rest on the new grid.
    fn trim(&mut self, term: &mut Term<Listener>) {
        if Self::in_alt(term) {
            return;
        }
        let history = term.grid().history_size();
        if history > self.cap {
            term.grid_mut().update_history(self.cap);
            self.base += count(history - self.cap);
            self.records.evict_below(self.base);
        }
        self.limit = 2 * self.cap + term.screen_lines();
        term.grid_mut().update_history(self.limit);
    }

    /// Moves every anchor to the row its logical position now names.
    fn relocate(&mut self, grid: &Grid<Cell>, logical: &[(u64, Logical)]) {
        let moved = rows_of(grid, self.base, logical);
        self.records.remap(|row| moved.get(&row).copied());
    }
}

/// Ends a pending synchronized update (`DEC 2026`), so that a mark or a
/// split reads the grid after the bytes the update held rather than
/// before them. The frame may paint early there.
fn flush_sync(parser: &mut Processor, term: &mut Term<Listener>) {
    if parser.sync_timeout().sync_timeout().is_some() {
        parser.stop_sync(term);
    }
}

/// A row count as the absolute rows count them.
fn count(rows: usize) -> u64 {
    u64::try_from(rows).unwrap_or(u64::MAX)
}

/// The absolute number of grid line `line`.
fn abs_of(base: u64, history: usize, line: i32) -> u64 {
    let offset = i64::try_from(history)
        .unwrap_or(i64::MAX)
        .saturating_add(i64::from(line));
    base.saturating_add(u64::try_from(offset).unwrap_or(0))
}

/// The grid line of absolute row `abs`, if it is still on the grid.
fn line_of(abs: u64, base: u64, grid: &Grid<Cell>) -> Option<i32> {
    let history = i128::from(count(grid.history_size()));
    let line = i32::try_from(i128::from(abs) - i128::from(base) - history).ok()?;
    (grid.topmost_line().0..=grid.bottommost_line().0)
        .contains(&line)
        .then_some(line)
}

/// The grid line absolute row `abs` names, clamped into the grid: a row the
/// history no longer holds reads the nearest line that is left.
fn clamped_line(abs: u64, base: u64, grid: &Grid<Cell>) -> i32 {
    let line = i128::from(abs) - i128::from(base) - i128::from(count(grid.history_size()));
    let line = i32::try_from(line).unwrap_or(if line.is_negative() {
        i32::MIN
    } else {
        i32::MAX
    });
    line.clamp(grid.topmost_line().0, grid.bottommost_line().0)
}

/// Everything after the last `"$ "`, `"> "`, `"# "` or `"% "` in `line`,
/// the line unchanged when it holds none. The first separator present wins;
/// its last occurrence is the cut.
fn strip_prompt_prefix(line: &str) -> &str {
    for sep in ["$ ", "> ", "# ", "% "] {
        if let Some(at) = line.rfind(sep) {
            return &line[at + sep.len()..];
        }
    }
    line
}

/// Whether grid line `line` starts a logical line: the row above it did not
/// wrap into it.
fn starts_line(grid: &Grid<Cell>, line: i32) -> bool {
    line <= grid.topmost_line().0
        || !grid[Line(line - 1)][grid.last_column()]
            .flags
            .contains(Flags::WRAPLINE)
}

/// The grid's logical lines as (first line, last line), newest first, and
/// the index of the one the cursor is on. They are read from the bottom up
/// only until `enough` holds for the lines read so far and the cursor's
/// index, once read.
fn logical_lines(
    grid: &Grid<Cell>,
    enough: impl Fn(&[(i32, i32)], Option<usize>) -> bool,
) -> (Vec<(i32, i32)>, usize) {
    let cursor = grid.cursor.point.line.0;
    let mut lines = Vec::new();
    let mut at_cursor = None;
    let mut last = grid.bottommost_line().0;
    for line in (grid.topmost_line().0..=last).rev() {
        if !starts_line(grid, line) {
            continue;
        }
        if (line..=last).contains(&cursor) {
            at_cursor = Some(lines.len());
        }
        lines.push((line, last));
        last = line - 1;
        if enough(&lines, at_cursor) {
            break;
        }
    }
    (lines, at_cursor.unwrap_or(0))
}

/// The logical position of each anchor still on the grid, its line counted
/// from the cursor's: resizing keeps the cursor on its own line, while it
/// may add or drop blank rows below it. Only the lines from the oldest
/// anchor's down are read.
fn logical_positions(grid: &Grid<Cell>, base: u64, anchors: &[u64]) -> Vec<(u64, Logical)> {
    let on_grid: Vec<(u64, i32)> = anchors
        .iter()
        .filter_map(|&abs| Some((abs, line_of(abs, base, grid)?)))
        .collect();
    let Some(oldest) = on_grid.iter().map(|&(_, line)| line).min() else {
        return Vec::new();
    };
    let (lines, at_cursor) = logical_lines(grid, |lines, at_cursor| {
        at_cursor.is_some() && lines.last().is_some_and(|&(first, _)| first <= oldest)
    });
    on_grid
        .into_iter()
        .filter_map(|(abs, line)| {
            // Newest first, so the first lines are in descending order.
            let index = lines.partition_point(|&(first, _)| first > line);
            let (first, _) = lines.get(index)?;
            let logical = Logical {
                index: signed(index) - signed(at_cursor),
                row: usize::try_from(line - first).ok()?,
            };
            Some((abs, logical))
        })
        .collect()
}

fn signed(index: usize) -> isize {
    isize::try_from(index).unwrap_or(isize::MAX)
}

/// The new absolute row of each anchor's logical position, clamped to its
/// logical line's last row; one whose line is gone has none. Only the
/// lines down from the oldest position are read.
fn rows_of(grid: &Grid<Cell>, base: u64, logical: &[(u64, Logical)]) -> HashMap<u64, u64> {
    let Some(oldest) = logical.iter().map(|(_, at)| at.index).max() else {
        return HashMap::new();
    };
    let (lines, at_cursor) = logical_lines(grid, |lines, at_cursor| {
        at_cursor.is_some_and(|at_cursor| signed(lines.len()) > signed(at_cursor) + oldest)
    });
    let history = grid.history_size();
    logical
        .iter()
        .filter_map(|&(abs, at)| {
            let index = usize::try_from(signed(at_cursor) + at.index).ok()?;
            let &(first, last) = lines.get(index)?;
            let row = i32::try_from(at.row).unwrap_or(i32::MAX);
            let line = first.saturating_add(row).min(last);
            Some((abs, abs_of(base, history, line)))
        })
        .collect()
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

/// One finished command as its gutter dot's menu shows it: the header line
/// and the two texts the copy rows put on the clipboard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellCommand {
    /// `exit N · 1.23s`, the dot's tooltip.
    pub header: String,
    /// The command line, and `""` when the shell left none.
    pub command: String,
    /// The command's output, and `""` when there is none.
    pub output: String,
}

impl Terminal {
    /// A blank terminal whose cursor is `cursor` until the program sets its own.
    pub fn new(size: GridSize, cursor: CursorShape) -> Self {
        Self::build(size, cursor, SCROLLBACK_LINES, None)
    }

    /// A blank terminal for a plain shell: it keeps records of the commands
    /// the shell marks, their live marks stamped by `clock`.
    pub fn with_shell_marks(size: GridSize, cursor: CursorShape, clock: Clock) -> Self {
        Self::with_marks_capped(size, cursor, clock, SCROLLBACK_LINES)
    }

    /// [`Self::with_shell_marks`] keeping `cap` lines of history.
    fn with_marks_capped(size: GridSize, cursor: CursorShape, clock: Clock, cap: usize) -> Self {
        let limit = 2 * cap + size.rows;
        let shell = Shell {
            scanner: Scanner::default(),
            records: Records::default(),
            base: 0,
            cap,
            limit,
            clock,
            alt_snapshot: None,
            alt_resized: false,
        };
        Self::build(size, cursor, limit, Some(shell))
    }

    fn build(size: GridSize, cursor: CursorShape, history: usize, shell: Option<Shell>) -> Self {
        let config = Config {
            scrolling_history: history,
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
            shell,
        }
    }

    pub fn size(&self) -> GridSize {
        self.size
    }

    /// Rebuilds the theme for a new pane background.
    pub fn set_background(&mut self, background: Rgb) {
        self.theme = theme::build_theme(background);
    }

    /// Feeds live output. Replies it provokes wait in [`Self::take_replies`].
    pub fn feed(&mut self, bytes: &[u8]) {
        let at = self.shell.as_ref().map(|shell| (shell.clock)());
        self.feed_at(bytes, at);
    }

    /// Feeds `bytes`, a plain shell's split at each mark and at each
    /// sequence that moves the rows the marks are anchored to. The marks
    /// are stamped `at`; replayed ones are not.
    fn feed_at(&mut self, bytes: &[u8], at: Option<Instant>) {
        let Self {
            term,
            parser,
            shell,
            ..
        } = self;
        let Some(shell) = shell.as_mut() else {
            parser.advance(term, bytes);
            return;
        };
        for slice in bytes.chunks(FEED_SLICE) {
            let mut rest = slice;
            while !rest.is_empty() {
                let (read, split) = shell.scanner.next(rest);
                let Some(split) = split else {
                    parser.advance(term, rest);
                    break;
                };
                if split.before_final() {
                    let final_byte = read.saturating_sub(1);
                    parser.advance(term, &rest[..final_byte]);
                    flush_sync(parser, term);
                    shell.before(term, &split);
                    parser.advance(term, &rest[final_byte..read]);
                } else {
                    parser.advance(term, &rest[..read]);
                    flush_sync(parser, term);
                    shell.after(term, split, at);
                }
                rest = &rest[read..];
            }
            shell.settle(term);
        }
    }

    /// The dots of the finished commands whose prompt row is on screen, top
    /// first. None while the alternate screen is up.
    pub fn shell_dots(&self) -> Vec<ShellDot> {
        self.visible_records()
            .into_iter()
            .map(|(record, row)| ShellDot {
                row,
                status: record.status(),
                exit: record.exit,
                tooltip: record.tooltip(),
            })
            .collect()
    }

    /// The finished commands whose prompt row is on screen, with the
    /// viewport row of each, top first. Empty while the alternate screen is
    /// up.
    fn visible_records(&self) -> Vec<(&Record, usize)> {
        let Some(shell) = self.shell.as_ref() else {
            return Vec::new();
        };
        if Shell::in_alt(&self.term) {
            return Vec::new();
        }
        let grid = self.term.grid();
        let offset = i64::try_from(grid.display_offset()).unwrap_or(i64::MAX);
        let mut found: Vec<(&Record, usize)> = shell
            .records
            .iter()
            .filter_map(|record| {
                let line = line_of(record.prompt, shell.base, grid)?;
                let row = usize::try_from(i64::from(line).saturating_add(offset))
                    .ok()
                    .filter(|row| *row < self.size.rows)?;
                Some((record, row))
            })
            .collect();
        found.sort_by_key(|(_, row)| *row);
        found
    }

    /// The finished command the gutter dot at `index` (top first) stands
    /// for, as its menu shows it.
    pub fn shell_command(&self, index: usize) -> Option<ShellCommand> {
        let (record, _) = *self.visible_records().get(index)?;
        Some(ShellCommand {
            header: record.tooltip(),
            command: self.command_text(record),
            output: self.output_text(record),
        })
    }

    /// The command line of `record`: the shell's own `OSC 633;E` text when
    /// it sent one, else its rows from the prompt's through the last one
    /// before the output: the output start's row, or the row above it when
    /// the output starts at column 0. Those rows are one wrapped line, so
    /// they join with no separator; a row that wraps keeps its trailing
    /// blanks, the others are right-trimmed, and the first loses its prompt
    /// prefix.
    pub fn command_text(&self, record: &Record) -> String {
        if let Some(command) = &record.command {
            return command.clone();
        }
        let Some(base) = self.shell.as_ref().map(|shell| shell.base) else {
            return String::new();
        };
        let last = match record.output {
            None => Some(record.prompt),
            Some(start) if record.output_col == 0 => start.checked_sub(1),
            Some(start) => Some(start),
        };
        let Some(lines) = last.and_then(|last| self.lines_between(base, record.prompt, last))
        else {
            return String::new();
        };
        let grid = self.term.grid();
        let mut text = String::new();
        for line in lines {
            let row = self.read_row(line).text;
            let full = grid[Line(line)][grid.last_column()]
                .flags
                .contains(Flags::WRAPLINE);
            let part = if full { row.as_str() } else { row.trim_end() };
            text.push_str(if text.is_empty() {
                strip_prompt_prefix(part)
            } else {
                part
            });
        }
        text.trim_end().to_owned()
    }

    /// The output of `record`, one line a row, each right-trimmed, with its
    /// trailing blank rows dropped. It starts on the output start's row
    /// when that start is at column 0, else on the row after it, and ends
    /// on the row above the end's when the end is at column 0 (the next
    /// prompt's row), else on the end's row; with no end, on the cursor's
    /// row. Empty without an output start.
    pub fn output_text(&self, record: &Record) -> String {
        let (Some(base), Some(start)) =
            (self.shell.as_ref().map(|shell| shell.base), record.output)
        else {
            return String::new();
        };
        let grid = self.term.grid();
        let first = if record.output_col == 0 {
            start
        } else {
            start.saturating_add(1)
        };
        let last = match record.end {
            Some(end) if record.end_col == 0 => end.checked_sub(1),
            Some(end) => Some(end),
            None => Some(abs_of(base, grid.history_size(), grid.cursor.point.line.0)),
        };
        let Some(lines) = last.and_then(|last| self.lines_between(base, first, last)) else {
            return String::new();
        };
        let mut rows: Vec<String> = lines
            .map(|line| self.read_row(line).text.trim_end().to_owned())
            .collect();
        while rows.last().is_some_and(|row| row.trim().is_empty()) {
            rows.pop();
        }
        rows.join("\n")
    }

    /// The grid lines of absolute rows `first` through `last`, a `first`
    /// the history dropped read from the topmost line. `None` when `last`
    /// is above `first`, or the history dropped `last` too (the topmost
    /// line's absolute row is `base`): the whole span is gone.
    fn lines_between(&self, base: u64, first: u64, last: u64) -> Option<RangeInclusive<i32>> {
        if last < first || last < base {
            return None;
        }
        let grid = self.term.grid();
        Some(clamped_line(first, base, grid)..=clamped_line(last, base, grid))
    }

    /// Feeds replayed history. The queries in it were answered when they were
    /// first made, so the replies they provoke now are discarded, and so are
    /// the clipboard stores: the copy happened when the line was written. A
    /// synchronized update the history left open is ended here, before its
    /// bytes could run later as if they had just arrived.
    pub fn feed_history(&mut self, bytes: &[u8]) {
        let queued = self.listener.replies.borrow().len();
        let stored = self.listener.copies.borrow().len();
        self.feed_at(bytes, None);
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

    /// Resizes the grid. A plain shell's anchors follow their rows: a row
    /// count change keeps the bottom row, and a column change reflows the
    /// rows, which moves each anchor to its logical position's new row.
    pub fn resize(&mut self, size: GridSize) {
        let old = std::mem::replace(&mut self.size, size);
        let Some(shell) = self.shell.as_mut() else {
            self.term.resize(size);
            return;
        };
        if Shell::in_alt(&self.term) {
            self.term.resize(size);
            // The snapshot taken at entry places the anchors on the way out;
            // without one there is nothing to place them by.
            if shell.alt_snapshot.is_some() {
                shell.alt_resized = true;
            } else {
                shell.records.clear();
            }
            return;
        }
        shell.settle(&mut self.term);
        if size.cols == old.cols {
            self.term.resize(size);
        } else {
            let anchors = shell.records.anchors();
            let logical = logical_positions(self.term.grid(), shell.base, &anchors);
            self.term.resize(size);
            shell.relocate(self.term.grid(), &logical);
        }
        shell.trim(&mut self.term);
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

    /// The background the terminal paints: a program's `OSC 11`, else the
    /// configured one. Resolved as [`Self::snapshot`] resolves it, without
    /// walking the cells.
    pub fn background(&self) -> Rgb {
        resolve(
            Color::Named(NamedColor::Background),
            self.term.colors(),
            &self.theme,
        )
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

/// The rows around a hovered line that its links are read from, as the walk
/// in [`Terminal::link_window`] found them.
#[derive(Debug)]
pub struct LinkWindow {
    /// The grid line of the first row.
    first: i32,
    rows: Vec<TerminalRow>,
    cols: usize,
    /// The walk stopped at its cap rather than at a row that ends the line.
    cut_above: bool,
    cut_below: bool,
}

impl LinkWindow {
    /// Every link in the window, its rows in grid lines. A link touching a
    /// window edge the walk only stopped at because it ran out of window is
    /// dropped rather than offered cut short.
    #[must_use]
    pub fn links(&self) -> Vec<TerminalLink> {
        let edge = i32::try_from(self.rows.len())
            .unwrap_or(i32::MAX)
            .saturating_sub(1);
        let mut links = links::detect_row_links(&self.rows, self.cols);
        links.retain(|link| {
            let cut_short =
                (self.cut_above && link.start_row == 0) || (self.cut_below && link.end_row == edge);
            !cut_short
        });
        for link in &mut links {
            link.shift_rows(self.first);
        }
        links
    }
}

/// The row reader the link detector works on. Every method here hands out
/// grid lines rather than screen rows, so a line above the viewport is a
/// negative one.
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
    /// The view reads them through [`Self::link_window`], which it caches.
    #[cfg(test)]
    #[must_use]
    pub fn detect_links_near(&self, line: i32) -> Vec<TerminalLink> {
        self.links_near(line, Self::LINK_ROW_WINDOW)
    }

    /// [`Self::detect_links_near`] with the window cap named, so that a test
    /// can reach a window edge on a small grid.
    #[cfg(test)]
    fn links_near(&self, line: i32, cap: i32) -> Vec<TerminalLink> {
        let Some(window) = self.window_near(line, cap) else {
            return Vec::new();
        };
        let mut links = window.links();
        links.retain(|link| link.start_row <= line && link.end_row >= line);
        links
    }

    /// The rows the links spanning grid line `line` are read from.
    ///
    /// The walk reaches [`Self::LINK_ROW_WINDOW`] rows back from `line`, and
    /// as many forward, independently, while each step is a soft wrap or a
    /// hard stitch. `None` when `line` is not on the grid.
    #[must_use]
    pub fn link_window(&self, line: i32) -> Option<LinkWindow> {
        self.window_near(line, Self::LINK_ROW_WINDOW)
    }

    /// [`Self::link_window`] with the window cap named.
    fn window_near(&self, line: i32, cap: i32) -> Option<LinkWindow> {
        let (top, bottom) = self.line_bounds();
        if line < top || line > bottom {
            return None;
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

        let mut below: Vec<TerminalRow> = Vec::new();
        let mut last = line;
        while last < bottom && last - line < cap {
            let next = self.read_row(last + 1);
            // Each step reads the row it walked last against the next: the
            // hovered row first, whatever the walk back found above it.
            let Some(current) = below.last().or_else(|| above.first()) else {
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
        Some(LinkWindow {
            first,
            rows: above.into_iter().chain(below).collect(),
            cols,
            cut_above,
            cut_below,
        })
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
    fn a_stitched_path_is_the_same_link_from_each_of_its_rows() {
        let boxed = sized(
            "│ X:/dev/proj/notes-file │\r\n│ aaaaaaaaaaaaaaaaaaaa   │\r\n│ b.txt:7                │\r\nnext/thing"
                .as_bytes(),
            28,
            6,
        );
        let target = format!("X:/dev/proj/notes-file{}b.txt", "a".repeat(20));
        for line in 0..3 {
            let links = boxed.detect_links_near(line);
            assert_eq!(links.len(), 1, "hovered on row {line}: {links:?}");
            assert_eq!(links[0].target, target, "hovered on row {line}");
            assert_eq!((links[0].start_row, links[0].end_row), (0, 2));
        }
        let next = boxed.detect_links_near(3);
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].target, "next/thing", "the next line stands alone");
        assert_eq!((next[0].start_row, next[0].end_row), (3, 3));

        // A soft-wrapped lead-in above the path's first row must not decide
        // whether the row below the hovered one continues the path.
        let wrapped = sized(
            b"hello there friend, X:/dir/aaaaaaaaaaaaa\r\nbbbb.rs",
            20,
            4,
        );
        let target = format!("X:/dir/{}bbbb.rs", "a".repeat(13));
        for line in 1..3 {
            let links = wrapped.detect_links_near(line);
            assert_eq!(links.len(), 1, "hovered on row {line}: {links:?}");
            assert_eq!(links[0].target, target, "hovered on row {line}");
        }
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

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test fails with the message of the precondition it lost"
)]
mod shell_tests {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::index::{Column, Line};
    use alacritty_terminal::vte::ansi::CursorShape;

    use super::{GridSize, SCROLLBACK_LINES, Terminal, line_of};
    use crate::shell_marks::{Record, ShellStatus};

    type Now = Arc<Mutex<Instant>>;

    fn shell(cols: usize, rows: usize, cap: usize) -> (Terminal, Now) {
        let now: Now = Arc::new(Mutex::new(Instant::now()));
        let read = Arc::clone(&now);
        let clock = Arc::new(move || *read.lock().expect("clock"));
        let size = GridSize { cols, rows };
        let term = Terminal::with_marks_capped(size, CursorShape::Block, clock, cap);
        (term, now)
    }

    /// One command: the prompt row starts with `tag`, one line of output,
    /// and the end with `exit`, which leaves the cursor two rows down.
    fn cmd(tag: char, exit: i32) -> String {
        format!("\x1b]133;A\x07{tag}\r\n\x1b]133;C\x07out\r\n\x1b]133;D;{exit}\x07")
    }

    /// The character at column 0 of each record's prompt row, oldest first;
    /// `!` for a prompt row that is off the grid.
    fn tags(term: &Terminal) -> Vec<char> {
        let shell = term.shell.as_ref().expect("a shell terminal");
        let grid = term.term.grid();
        shell
            .records
            .iter()
            .map(|record| {
                line_of(record.prompt, shell.base, grid)
                    .map_or('!', |line| grid[Line(line)][Column(0)].c)
            })
            .collect()
    }

    /// Every row of the grid, oldest first, with each record's prompt row
    /// marked `>`, for a failure message.
    fn dump(term: &Terminal) -> Vec<String> {
        let shell = term.shell.as_ref().expect("a shell terminal");
        let grid = term.term.grid();
        let prompts: Vec<i32> = shell
            .records
            .iter()
            .filter_map(|record| line_of(record.prompt, shell.base, grid))
            .collect();
        let (top, bottom) = term.line_bounds();
        (top..=bottom)
            .map(|line| {
                let mark = if prompts.contains(&line) { '>' } else { ' ' };
                let wrap = if term.wraps(line) { '~' } else { ' ' };
                format!("{mark}{wrap}{line}:{}", term.read_row(line).text.trim_end())
            })
            .collect()
    }

    fn dot_rows(term: &Terminal) -> Vec<usize> {
        term.shell_dots().iter().map(|dot| dot.row).collect()
    }

    fn crlf(n: usize) -> Vec<u8> {
        "\r\n".repeat(n).into_bytes()
    }

    #[test]
    fn a_dot_tracks_its_prompt_as_output_scrolls_below_the_cap() {
        let (mut term, _) = shell(10, 4, 20);
        term.feed(cmd('a', 0).as_bytes());
        assert_eq!(dot_rows(&term), [0]);
        term.feed(&crlf(2));
        assert_eq!(dot_rows(&term), [] as [usize; 0], "scrolled off the screen");
        term.scroll(1);
        assert_eq!(dot_rows(&term), [0], "back in view in the history");
        assert_eq!(tags(&term), ['a']);
    }

    #[test]
    fn a_prompt_at_the_top_of_a_full_history_keeps_its_dot_until_evicted() {
        let (mut term, _) = shell(10, 4, 5);
        term.feed(cmd('a', 0).as_bytes());
        term.feed(&crlf(6));
        assert_eq!(term.line_bounds(), (-5, 3));
        term.scroll(5);
        assert_eq!(dot_rows(&term), [0], "the prompt is the oldest line");
        term.scroll_to_bottom();
        term.feed(&crlf(1));
        assert_eq!(term.line_bounds(), (-5, 3), "the cap holds");
        assert_eq!(tags(&term), [] as [char; 0], "evicted with its line");
    }

    #[test]
    fn anchors_stay_exact_across_feeds_that_straddle_the_cap() {
        let (mut term, _) = shell(10, 4, 5);
        term.feed(cmd('a', 0).as_bytes());
        term.feed(&crlf(3));
        term.feed(cmd('b', 1).as_bytes());
        term.feed(&crlf(1));
        assert_eq!(tags(&term), ['a', 'b']);
        term.feed(&crlf(1));
        assert_eq!(tags(&term), ['b']);
        term.feed(&crlf(2));
        assert_eq!(tags(&term), ['b']);
    }

    #[test]
    fn a_feed_that_overruns_the_headroom_drops_every_record() {
        let (mut term, _) = shell(10, 4, 5);
        term.feed(cmd('a', 0).as_bytes());
        term.feed(&crlf(30));
        assert_eq!(tags(&term), [] as [char; 0]);
        term.feed(cmd('c', 0).as_bytes());
        assert_eq!(tags(&term), ['c'], "the next mark is exact");
        assert_eq!(term.line_bounds(), (-5, 3));
    }

    #[test]
    fn scrolls_and_clears_move_or_drop_the_anchors() {
        let (mut term, _) = shell(10, 4, 20);
        term.feed(cmd('a', 0).as_bytes());
        term.feed(b"\x1b[2S");
        assert_eq!(tags(&term), ['a'], "CSI S scrolls into the history");
        term.feed(b"\x1b[2J");
        assert_eq!(tags(&term), ['a'], "2J pushes the screen into the history");
        term.feed(b"\x1b[H");
        term.feed(cmd('b', 0).as_bytes());
        term.feed(b"\x1b[3J");
        assert_eq!(tags(&term), ['b'], "3J erases the history, not the screen");
        term.feed(b"\x1b[4;1H\x1bM");
        assert_eq!(
            tags(&term),
            ['b'],
            "a reverse index off the top moves nothing"
        );
        term.feed(b"\x1b[3;4r\x1b[4;1H\n\n\n\x1b[r");
        assert_eq!(
            tags(&term),
            ['b'],
            "a scroll inside a lower region leaves it"
        );
        term.feed(b"\x1bc");
        assert_eq!(tags(&term), [] as [char; 0], "a reset drops every record");
        term.feed(cmd('c', 0).as_bytes());
        assert_eq!(tags(&term), ['c']);
    }

    #[test]
    fn marks_on_the_alternate_screen_are_ignored_and_dots_return_after_it() {
        let (mut term, _) = shell(10, 4, 20);
        term.feed(cmd('a', 0).as_bytes());
        term.feed(b"\x1b[?1049h\x1b]133;A\x07x\x1b]133;D;0\x07");
        assert_eq!(
            dot_rows(&term),
            [] as [usize; 0],
            "no dots over a full-screen program"
        );
        term.resize(GridSize { cols: 6, rows: 4 });
        term.feed(b"\x1b[?1049l");
        assert_eq!(
            tags(&term),
            ['a'],
            "the alternate screen's marks made nothing"
        );
        assert_eq!(dot_rows(&term), [0]);
        term.feed(b"\x1b[?1049h");
        term.feed(b"\x1b[?1049l");
        assert_eq!(tags(&term), ['a']);
    }

    #[test]
    fn a_row_resize_at_the_cap_keeps_or_evicts_whole_anchors() {
        let (mut term, _) = shell(10, 4, 5);
        term.feed(cmd('a', 0).as_bytes());
        term.feed(&crlf(2));
        term.resize(GridSize { cols: 10, rows: 2 });
        assert_eq!(tags(&term), ['a']);
        term.resize(GridSize { cols: 10, rows: 6 });
        assert_eq!(tags(&term), ['a']);

        // The prompt is the oldest line of a full history.
        let (mut term, _) = shell(10, 4, 5);
        term.feed(cmd('a', 0).as_bytes());
        term.feed(&crlf(6));
        term.resize(GridSize { cols: 10, rows: 6 });
        assert_eq!(tags(&term), ['a'], "a taller screen pulls rows back");
        term.resize(GridSize { cols: 10, rows: 4 });
        assert_eq!(tags(&term), ['a']);
        term.resize(GridSize { cols: 10, rows: 3 });
        assert_eq!(tags(&term), [] as [char; 0], "pushed past the cap");
    }

    #[test]
    fn a_column_resize_reflows_a_wrapped_prompt_and_keeps_its_anchor() {
        let (mut term, _) = shell(10, 6, 20);
        term.feed(b"\x1b]133;A\x07a123456789XYZ\r\n\x1b]133;C\x07out\r\n\x1b]133;D;0\x07");
        term.feed(cmd('b', 1).as_bytes());
        assert_eq!(tags(&term), ['a', 'b']);
        term.resize(GridSize { cols: 6, rows: 6 });
        assert_eq!(tags(&term), ['a', 'b']);
        term.resize(GridSize { cols: 12, rows: 6 });
        assert_eq!(tags(&term), ['a', 'b']);
        let shell = term.shell.as_ref().expect("a shell terminal");
        let ends: Vec<char> = shell
            .records
            .iter()
            .map(|record| {
                let line = record
                    .end
                    .and_then(|end| line_of(end, shell.base, term.term.grid()))
                    .expect("on the grid");
                term.term.grid()[Line(line - 1)][Column(0)].c
            })
            .collect();
        assert_eq!(ends, ['o', 'o'], "each end sits below its output");
    }

    #[test]
    fn live_commands_carry_a_duration_and_replayed_ones_do_not() {
        let (mut term, now) = shell(20, 6, 20);
        term.feed_history(format!("{}{}", cmd('a', 0), cmd('b', 3)).as_bytes());
        term.feed(b"\x1b]133;A\x07c\r\n\x1b]133;C\x07");
        *now.lock().expect("clock") += Duration::from_millis(1234);
        term.feed(b"\x1b]133;D\x07");
        let dots: Vec<(usize, ShellStatus, String)> = term
            .shell_dots()
            .into_iter()
            .map(|dot| (dot.row, dot.status, dot.tooltip))
            .collect();
        assert_eq!(
            dots,
            [
                (0, ShellStatus::Ok, "exit 0".to_owned()),
                (2, ShellStatus::Fail, "exit 3".to_owned()),
                (4, ShellStatus::Unknown, "exit ? · 1.23s".to_owned()),
            ]
        );
    }

    /// A small deterministic generator for the random walk.
    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            usize::try_from(self.0 % u64::try_from(n).expect("small")).expect("small")
        }
    }

    /// A random walk of output, resizes and scrolls over one terminal, with
    /// the tags of the commands it wrote on the primary screen, in order,
    /// and the ops so far for a failure message.
    struct Walk {
        term: Terminal,
        rng: Rng,
        next_tag: u32,
        written: Vec<char>,
        log: Vec<String>,
    }

    impl Walk {
        fn new(seed: u64) -> Self {
            let (term, _) = shell(8, 4, 12);
            Self {
                term,
                rng: Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15)),
                next_tag: 0,
                written: Vec::new(),
                log: Vec::new(),
            }
        }

        fn step(&mut self) {
            let op = self.rng.below(12);
            self.log.push(op.to_string());
            if op < 6 {
                self.write(op);
            } else {
                self.disturb(op);
            }
        }

        /// Text, line feeds, or a whole command with a fresh tag.
        fn write(&mut self, op: usize) {
            match op {
                0 | 1 => {
                    let text: String = (0..self.rng.below(20))
                        .map(|i| if i % 3 == 0 { 'x' } else { 'y' })
                        .collect();
                    self.term.feed(text.as_bytes());
                }
                2 | 3 => self.term.feed(&crlf(1 + self.rng.below(4))),
                _ => self.command(),
            }
        }

        fn command(&mut self) {
            let tag = char::from_u32(0x100 + self.next_tag % 0x180).expect("a letter");
            self.next_tag += 1;
            if !super::Shell::in_alt(&self.term.term) {
                self.written.push(tag);
            }
            let exit = i32::try_from(self.rng.below(3)).expect("small");
            self.term.feed(format!("\r\n{}", cmd(tag, exit)).as_bytes());
        }

        /// A scroll, a clear, a resize, the alternate screen, a reset or a
        /// scroll of the view.
        fn disturb(&mut self, op: usize) {
            match op {
                6 => {
                    let lines = 1 + self.rng.below(3);
                    self.term.feed(format!("\x1b[{lines}S").as_bytes());
                }
                7 => self.clear(),
                8 => self.resize(),
                9 => self.term.feed(b"\x1b[?1049hfull\r\nscreen"),
                10 => self.term.feed(b"\x1b[?1049l"),
                _ => self.reset_or_view(),
            }
        }

        fn clear(&mut self) {
            let clear: &[u8] = if self.rng.below(2) == 0 {
                b"\x1b[2J"
            } else {
                b"\x1b[3J"
            };
            self.term.feed(clear);
        }

        fn resize(&mut self) {
            let size = GridSize {
                cols: 4 + self.rng.below(10),
                rows: 2 + self.rng.below(6),
            };
            self.log.push(format!("{}x{}", size.cols, size.rows));
            self.term.resize(size);
        }

        fn reset_or_view(&mut self) {
            if self.rng.below(8) == 0 {
                self.term.feed(b"\x1bc");
            } else {
                let lines = i32::try_from(self.rng.below(5)).expect("small") - 2;
                self.term.scroll(lines);
            }
        }

        /// Asserts every live record sits on its tag and returns how many
        /// there are; none are read while the alternate screen is up.
        fn check(&self, seed: u64, step: usize) -> usize {
            if super::Shell::in_alt(&self.term.term) {
                return 0;
            }
            // Records only ever leave from the oldest end, so the live ones
            // are the newest commands written, each on its tag.
            let found = tags(&self.term);
            let written = &self.written;
            assert!(
                written.ends_with(&found),
                "seed {seed} step {step}: found {found:?}, written {:?}, last ops {:?}, grid {:?}",
                &written[written.len().saturating_sub(found.len() + 2)..],
                &self.log[self.log.len().saturating_sub(12)..],
                dump(&self.term)
            );
            found.len()
        }
    }

    #[test]
    fn random_output_never_moves_an_anchor_off_its_tag() {
        let mut kept = 0;
        for seed in 1..=40_u64 {
            let mut walk = Walk::new(seed);
            for step in 0..300 {
                walk.step();
                kept += walk.check(seed, step);
            }
        }
        assert!(kept > 1000, "the walk kept records to check: {kept}");
    }

    #[test]
    fn a_mark_inside_a_synchronized_update_reads_the_grid_after_it() {
        let (mut term, _) = shell(10, 4, 20);
        term.feed(cmd('a', 0).as_bytes());
        term.feed(&crlf(5));
        term.feed(b"zz");
        let history = term.term.grid().history_size();
        let held = format!("\x1b[?2026h\x1b[H\x1b[2J\x1b[3J{}\x1b[?2026l", cmd('b', 0));
        term.feed(held.as_bytes());
        let shell = term.shell.as_ref().expect("a shell terminal");
        assert_eq!(
            shell.base,
            u64::try_from(history + 4).expect("small"),
            "3J erased the history 2J had pushed the four rows down to `zz` into"
        );
        assert_eq!(tags(&term), ['b']);
        assert_eq!(dot_rows(&term), [0]);
    }

    #[test]
    fn a_column_shrink_that_cuts_the_history_keeps_the_recent_records() {
        let (mut term, _) = shell(150, 10, SCROLLBACK_LINES);
        let line = format!("{}\r\n", "x".repeat(150));
        term.feed(line.repeat(SCROLLBACK_LINES).as_bytes());
        term.feed(cmd('a', 0).as_bytes());
        term.feed(cmd('b', 1).as_bytes());
        term.resize(GridSize { cols: 60, rows: 10 });
        assert_eq!(tags(&term), ['a', 'b'], "each wide line is three rows now");
        assert_eq!(dot_rows(&term).len(), 2);
        assert_eq!(term.term.grid().history_size(), SCROLLBACK_LINES);
    }

    /// The one finished command a terminal keeps.
    fn record(term: &Terminal) -> Record {
        term.shell
            .as_ref()
            .expect("a shell terminal")
            .records
            .iter()
            .next()
            .cloned()
            .expect("a finished command")
    }

    const PROMPT: &str = "\x1b]133;A\x07";
    const TYPED: &str = "\x1b]133;B\x07";
    const OUTPUT: &str = "\x1b]133;C\x07";

    /// One command in the order bash, zsh and pwsh write it: the prompt,
    /// the typed command, the Enter's newline, the output mark at the start
    /// of the next row, `out`, the end at the start of the row after it,
    /// and the next prompt there.
    fn rows_command(prompt: &str, typed: &str, out: &str, exit: i32) -> String {
        format!(
            "{PROMPT}{prompt}{TYPED}{typed}\r\n{OUTPUT}{out}\r\n\x1b]133;D;{exit}\x07{PROMPT}$ "
        )
    }

    /// A finished command whose anchors are given, with no command line.
    fn anchored(prompt: u64, output: (u64, usize), end: (u64, usize)) -> Record {
        Record {
            prompt,
            output: Some(output.0),
            output_col: output.1,
            end: Some(end.0),
            end_col: end.1,
            exit: Some(0),
            command: None,
            prompt_at: None,
            output_at: None,
            end_at: None,
        }
    }

    #[test]
    fn the_shells_own_command_line_wins_over_the_rows() {
        let (mut term, _) = shell(24, 6, 20);
        term.feed(
            format!(
                "{PROMPT}$ {TYPED}ls\r\n\x1b]633;E;ls --color=auto\x07{OUTPUT}out\r\n\x1b]133;D;0\x07{PROMPT}$ "
            )
            .as_bytes(),
        );
        assert_eq!(term.command_text(&record(&term)), "ls --color=auto");
    }

    #[test]
    fn the_command_rows_strip_each_prompt_suffix() {
        for (prompt, typed, expected) in [
            ("$ ", "ls -la", "ls -la"),
            ("PS> ", "Get-Item", "Get-Item"),
            ("root# ", "reboot", "reboot"),
            ("% ", "echo hi", "echo hi"),
            ("", "ls", "ls"),
        ] {
            let (mut term, _) = shell(24, 6, 20);
            term.feed(rows_command(prompt, typed, "out", 0).as_bytes());
            assert_eq!(term.command_text(&record(&term)), expected, "{prompt}");
        }
    }

    #[test]
    fn the_command_rows_in_shell_order_stop_above_the_output() {
        let (mut term, _) = shell(24, 6, 20);
        term.feed(rows_command("$ ", "ls -la", "one", 0).as_bytes());
        assert_eq!(
            term.command_text(&record(&term)),
            "ls -la",
            "the output start's row is the output's first"
        );
    }

    #[test]
    fn a_wrapped_command_row_joins_the_next_with_nothing_between() {
        let (mut term, _) = shell(8, 6, 20);
        // `$ ls -l ` fills the first row, its blank included; `/tmp` wraps.
        term.feed(rows_command("$ ", "ls -l /tmp", "out", 0).as_bytes());
        assert_eq!(term.command_text(&record(&term)), "ls -l /tmp");
    }

    #[test]
    fn the_output_in_shell_order_is_exactly_the_output_lines() {
        let (mut term, _) = shell(24, 8, 20);
        term.feed(rows_command("$ ", "ls", "one\r\ntwo", 0).as_bytes());
        assert_eq!(
            term.output_text(&record(&term)),
            "one\ntwo",
            "the first line kept, the next prompt left out"
        );
    }

    #[test]
    fn an_output_mark_mid_row_starts_the_output_on_the_next_row() {
        let (mut term, _) = shell(24, 6, 20);
        term.feed(format!("{PROMPT}$ ls{OUTPUT}\r\none\r\n\x1b]133;D;0\x07{PROMPT}$ ").as_bytes());
        let record = record(&term);
        assert_eq!(record.output_col, 4);
        assert_eq!(term.output_text(&record), "one");
        assert_eq!(term.command_text(&record), "ls", "the output start's row");
    }

    #[test]
    fn an_end_mid_row_keeps_its_row_in_the_output() {
        let (mut term, _) = shell(24, 6, 20);
        term.feed(
            format!("{PROMPT}$ {TYPED}printf hi\r\n{OUTPUT}hi\x1b]133;D;0\x07\r\n{PROMPT}$ ")
                .as_bytes(),
        );
        assert_eq!(term.output_text(&record(&term)), "hi");
    }

    #[test]
    fn the_output_rows_are_trimmed_and_their_trailing_blanks_dropped() {
        let (mut term, _) = shell(24, 8, 20);
        term.feed(rows_command("$ ", "ls", "one  \r\ntwo\r\n\r\n", 0).as_bytes());
        assert_eq!(term.output_text(&record(&term)), "one\ntwo");
    }

    #[test]
    fn a_command_without_an_output_mark_has_no_output() {
        let (mut term, _) = shell(24, 6, 20);
        term.feed(
            format!("{PROMPT}$ {TYPED}true\r\n\x1b]633;E;true\x07\x1b]133;D;0\x07{PROMPT}$ ")
                .as_bytes(),
        );
        let record = record(&term);
        assert_eq!(term.output_text(&record), "");
        assert_eq!(term.command_text(&record), "true");
    }

    #[test]
    fn a_wrapped_output_row_is_a_line_of_its_own() {
        let (mut term, _) = shell(8, 8, 20);
        term.feed(rows_command("$ ", "ls", "abcdefghij", 0).as_bytes());
        assert_eq!(term.output_text(&record(&term)), "abcdefgh\nij");
    }

    /// A terminal whose history dropped its oldest row: absolute row 0
    /// (`AAAA`) is gone, and row 1 (`BBBB`) is the topmost line.
    fn dropped_one_row() -> Terminal {
        let (mut term, _) = shell(8, 4, 2);
        term.feed(b"AAAA\r\nBBBB\r\nCCCC\r\nDDDD\r\nEEEE\r\nFFFF\r\n");
        let base = term.shell.as_ref().expect("a shell terminal").base;
        assert_eq!(base, 1, "the cap dropped the oldest row");
        term
    }

    #[test]
    fn an_output_start_the_history_left_behind_reads_from_the_topmost_line() {
        let term = dropped_one_row();
        let record = anchored(0, (0, 0), (3, 0));
        assert_eq!(term.output_text(&record), "BBBB\nCCCC");
    }

    #[test]
    fn a_record_the_history_left_behind_has_no_output_and_no_command() {
        let term = dropped_one_row();
        // The first's output ends on absolute row 0, which is gone; the
        // second's command rows end on its output start's, row 0 too.
        for record in [anchored(0, (0, 0), (1, 0)), anchored(0, (0, 3), (0, 5))] {
            assert_eq!(term.output_text(&record), "", "{record:?}");
            assert_eq!(term.command_text(&record), "", "{record:?}");
        }
    }
}
