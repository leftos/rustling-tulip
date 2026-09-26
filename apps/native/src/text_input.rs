//! A text input, trimmed from gpui's `examples/input.rs`: typing and IME,
//! backspace and delete, the arrows, Home and End, Shift selection,
//! select-all, copy, cut and paste. [`TextInput::new`] makes a single-line
//! input, where Enter emits [`TextInputEvent::Submit`].
//! [`TextInput::multi_line`] makes one that wraps to its width and grows from
//! a minimum to a maximum number of rows, scrolling past that; there Enter
//! inserts a line break, Ctrl+Enter submits, and Up and Down move between
//! rows. Esc emits [`TextInputEvent::Cancel`] in both. Every edit the user
//! makes emits [`TextChanged`], and [`TextInput::set_text`] replaces the text
//! without it. A read-only input ([`TextInput::set_read_only`]) ignores edits.

use std::ops::Range;

use gpui::{
    App, AvailableSpace, Bounds, ClipboardItem, ContentMask, Context, CursorStyle, ElementId,
    ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable,
    GlobalElementId, KeyBinding, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, PaintQuad, Pixels, Point, SharedString, Style, TextAlign, TextRun,
    UTF16Selection, UnderlineStyle, Window, WrappedLine, actions, div, fill, point, prelude::*, px,
    relative, rgb, rgba, size,
};
use unicode_segmentation::UnicodeSegmentation as _;

/// The key context a single-line input's bindings apply in.
const CONTEXT: &str = "TextInput";
/// The key context of a multi-line input, its own so Enter and the vertical
/// arrows bind differently there.
const MULTI_LINE_CONTEXT: &str = "TextInputMultiLine";
const DEFAULT_MIN_ROWS: usize = 2;
const DEFAULT_MAX_ROWS: usize = 6;
const TEXT_COLOR: u32 = 0x00cc_cccc;
const PLACEHOLDER_COLOR: u32 = 0x006a_6a6a;
const CURSOR_COLOR: u32 = 0x00cc_cccc;
const SELECTION_COLOR: u32 = 0x264f_78ff;

actions!(
    text_input,
    [
        Backspace,
        Delete,
        Left,
        Right,
        Up,
        Down,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        SelectAll,
        Home,
        End,
        Paste,
        Cut,
        Copy,
        Enter,
        Newline,
        Escape,
    ]
);

/// Binds the input's keys, active only while an input has the keyboard.
pub fn bind_keys(cx: &mut App) {
    for context in [CONTEXT, MULTI_LINE_CONTEXT] {
        let context = Some(context);
        cx.bind_keys([
            KeyBinding::new("backspace", Backspace, context),
            KeyBinding::new("delete", Delete, context),
            KeyBinding::new("left", Left, context),
            KeyBinding::new("right", Right, context),
            KeyBinding::new("shift-left", SelectLeft, context),
            KeyBinding::new("shift-right", SelectRight, context),
            KeyBinding::new("ctrl-a", SelectAll, context),
            KeyBinding::new("ctrl-v", Paste, context),
            KeyBinding::new("ctrl-c", Copy, context),
            KeyBinding::new("ctrl-x", Cut, context),
            KeyBinding::new("home", Home, context),
            KeyBinding::new("end", End, context),
            KeyBinding::new("escape", Escape, context),
        ]);
    }
    cx.bind_keys([KeyBinding::new("enter", Enter, Some(CONTEXT))]);
    let multi_line = Some(MULTI_LINE_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("enter", Newline, multi_line),
        KeyBinding::new("ctrl-enter", Enter, multi_line),
        KeyBinding::new("up", Up, multi_line),
        KeyBinding::new("down", Down, multi_line),
        KeyBinding::new("shift-up", SelectUp, multi_line),
        KeyBinding::new("shift-down", SelectDown, multi_line),
    ]);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextInputEvent {
    Submit,
    Cancel,
}

/// The user edited the text: typing, IME, deletion, cut or paste.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextChanged;

pub struct TextInput {
    focus_handle: FocusHandle,
    content: SharedString,
    placeholder: SharedString,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    last_layout: Option<TextLayout>,
    last_bounds: Option<Bounds<Pixels>>,
    is_selecting: bool,
    mode: Mode,
    read_only: bool,
    /// The x a run of Up and Down aims for, so a short row passed on the way
    /// does not pull the caret left for good.
    goal_x: Option<Pixels>,
    /// How far a multi-line input's rows are scrolled up to keep the caret's
    /// row in view.
    scroll_y: Pixels,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    SingleLine,
    /// Grows with its text from `min_rows` to `max_rows`, then scrolls.
    MultiLine {
        min_rows: usize,
        max_rows: usize,
    },
}

impl Mode {
    fn is_multi_line(self) -> bool {
        matches!(self, Self::MultiLine { .. })
    }

    /// The owner's text as this mode holds it: a multi-line input keeps
    /// only LF line breaks, and a single-line one takes the text as given.
    fn accept(self, text: SharedString) -> SharedString {
        if self.is_multi_line() && text.contains('\r') {
            normalize_newlines(&text).into()
        } else {
            text
        }
    }
}

impl EventEmitter<TextInputEvent> for TextInput {}

impl EventEmitter<TextChanged> for TextInput {}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl TextInput {
    /// A single-line input holding `content`, all of it selected.
    pub fn new(
        content: impl Into<SharedString>,
        placeholder: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::with_mode(content.into(), placeholder.into(), Mode::SingleLine, cx)
    }

    /// A multi-line input holding `content`, all of it selected, two to six
    /// rows tall until [`Self::with_rows`] says otherwise.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "no view has a multi-line input yet; the tests build one"
        )
    )]
    pub fn multi_line(
        content: impl Into<SharedString>,
        placeholder: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mode = Mode::MultiLine {
            min_rows: DEFAULT_MIN_ROWS,
            max_rows: DEFAULT_MAX_ROWS,
        };
        Self::with_mode(content.into(), placeholder.into(), mode, cx)
    }

    fn with_mode(
        content: SharedString,
        placeholder: SharedString,
        mode: Mode,
        cx: &mut Context<Self>,
    ) -> Self {
        let content = mode.accept(content);
        Self {
            focus_handle: cx.focus_handle(),
            selected_range: 0..content.len(),
            content,
            placeholder,
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
            is_selecting: false,
            mode,
            read_only: false,
            goal_x: None,
            scroll_y: px(0.),
        }
    }

    /// How many rows a multi-line input shows: it grows with its text from
    /// `min_rows` to `max_rows` and scrolls past that. `min_rows` is at least
    /// one and `max_rows` at least `min_rows`. A single-line input stays one
    /// row whatever this says.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "no view has a multi-line input yet; the tests build one"
        )
    )]
    #[must_use]
    pub fn with_rows(mut self, min_rows: usize, max_rows: usize) -> Self {
        if self.mode.is_multi_line() {
            let min_rows = min_rows.max(1);
            self.mode = Mode::MultiLine {
                min_rows,
                max_rows: max_rows.max(min_rows),
            };
        }
        self
    }

    /// Makes the input read-only, or editable again. A read-only input
    /// ignores typing, IME, paste, cut and deletion; the caret still moves,
    /// and selecting and copying still work.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "no view has a read-only input yet; the tests make one"
        )
    )]
    /// Going read-only mid-composition drops the half-composed text, a
    /// [`TextChanged`] like the composing was.
    pub fn set_read_only(&mut self, read_only: bool, cx: &mut Context<Self>) {
        if self.read_only == read_only {
            return;
        }
        if read_only && let Some(marked) = self.marked_range.take() {
            self.splice(&marked, "");
            self.selected_range = marked.start..marked.start;
            self.selection_reversed = false;
            cx.emit(TextChanged);
        }
        self.read_only = read_only;
        self.marked_range = None;
        cx.notify();
    }

    pub fn text(&self) -> &str {
        &self.content
    }

    /// Replaces the text, the cursor at its end, without [`TextChanged`]:
    /// the owner is setting it, not the user.
    pub fn set_text(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.content = self.mode.accept(text.into());
        let end = self.content.len();
        self.selected_range = end..end;
        self.selection_reversed = false;
        self.marked_range = None;
        self.goal_x = None;
        cx.notify();
    }

    /// Replaces the text shown while the input is empty.
    pub fn set_placeholder(
        &mut self,
        placeholder: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        let placeholder = placeholder.into();
        if placeholder != self.placeholder {
            self.placeholder = placeholder;
            cx.notify();
        }
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(previous_boundary(&self.content, self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx);
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(next_boundary(&self.content, self.selected_range.end), cx);
        } else {
            self.move_to(self.selected_range.end, cx);
        }
    }

    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        self.move_vertically(false, false, cx);
    }

    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        self.move_vertically(true, false, cx);
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(previous_boundary(&self.content, self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(next_boundary(&self.content, self.cursor_offset()), cx);
    }

    fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_vertically(false, true, cx);
    }

    fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        self.move_vertically(true, true, cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx);
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        let start = self.caret_row().map_or(0, |(start, _)| start);
        self.move_to(start, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        let end = self.caret_row().map_or(self.content.len(), |(_, end)| end);
        self.move_to(end, cx);
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        if self.selected_range.is_empty() {
            self.select_to(previous_boundary(&self.content, self.cursor_offset()), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        if self.selected_range.is_empty() {
            self.select_to(next_boundary(&self.content, self.cursor_offset()), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn enter(_: &mut Self, _: &Enter, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(TextInputEvent::Submit);
    }

    fn newline(&mut self, _: &Newline, window: &mut Window, cx: &mut Context<Self>) {
        self.replace_text_in_range(None, "\n", window, cx);
    }

    fn escape(_: &mut Self, _: &Escape, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(TextInputEvent::Cancel);
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_handle.focus(window);
        self.is_selecting = true;
        let index = self.index_for_mouse_position(event.position);
        if event.modifiers.shift {
            self.select_to(index, cx);
        } else {
            self.move_to(index, cx);
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            self.select_to(self.index_for_mouse_position(event.position), cx);
        }
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            let text = if self.mode.is_multi_line() {
                normalize_newlines(&text)
            } else {
                single_line(&text)
            };
            self.replace_text_in_range(None, &text, window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_owned(),
            ));
        }
    }

    /// Copies the selection and, unless the input is read-only, deletes it.
    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_owned(),
            ));
            if !self.read_only {
                self.replace_text_in_range(None, "", window, cx);
            }
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        self.goal_x = None;
        cx.notify();
    }

    /// Moves the caret, or with `select` the selection's moving end, one row
    /// up or down, aiming for the goal x. From the first row Up goes to the
    /// start of the text, and from the last row Down to its end.
    fn move_vertically(&mut self, down: bool, select: bool, cx: &mut Context<Self>) {
        let (target, goal_x) = self.vertical_target(down);
        if select {
            self.select_to(target, cx);
        } else {
            self.move_to(target, cx);
        }
        self.goal_x = goal_x;
    }

    fn vertical_target(&self, down: bool) -> (usize, Option<Pixels>) {
        let text_edge = if down { self.content.len() } else { 0 };
        let Some(layout) = self
            .last_layout
            .as_ref()
            .filter(|_| !self.content.is_empty())
        else {
            return (text_edge, None);
        };
        let cursor = self.cursor_offset();
        let row = layout.row_ix(cursor);
        let goal_x = self.goal_x.unwrap_or_else(|| layout.position(cursor).x);
        let target_row = if down {
            Some(row + 1).filter(|ix| *ix < layout.rows.len())
        } else {
            row.checked_sub(1)
        };
        let target = target_row.map_or(text_edge, |ix| {
            layout.closest_in_row(ix, goal_x, &self.content)
        });
        (self.clamp_offset(target), Some(goal_x))
    }

    /// The start of the caret's row and the last offset the caret can take on
    /// it, once the input has been drawn.
    fn caret_row(&self) -> Option<(usize, usize)> {
        if self.content.is_empty() {
            return None;
        }
        let layout = self.last_layout.as_ref()?;
        let row = layout.rows.get(layout.row_ix(self.cursor_offset()))?;
        let end = Row::caret_end(row, &self.content);
        Some((self.clamp_offset(row.start), self.clamp_offset(end)))
    }

    /// `offset` inside the text and on a character boundary: the drawn layout
    /// can trail an edit by a frame.
    fn clamp_offset(&self, offset: usize) -> usize {
        let offset = offset.min(self.content.len());
        if self.content.is_char_boundary(offset) {
            offset
        } else {
            previous_boundary(&self.content, offset)
        }
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        if self.content.is_empty() {
            return 0;
        }
        let (Some(bounds), Some(layout)) = (self.last_bounds.as_ref(), self.last_layout.as_ref())
        else {
            return 0;
        };
        if position.y < bounds.top() {
            return 0;
        }
        if position.y > bounds.bottom() {
            return self.content.len();
        }
        let row = layout.row_at_y(position.y - bounds.top() + self.scroll_y);
        self.clamp_offset(layout.closest_in_row(row, position.x - bounds.left(), &self.content))
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.selection_reversed {
            self.selected_range.start = offset;
        } else {
            self.selected_range.end = offset;
        }
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        self.goal_x = None;
        cx.notify();
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        offset_to_utf16(&self.content, range.start)..offset_to_utf16(&self.content, range.end)
    }

    fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        offset_from_utf16(&self.content, range_utf16.start)
            ..offset_from_utf16(&self.content, range_utf16.end)
    }

    /// The range an edit applies to: the one given, else the IME's marked
    /// text, else the selection.
    fn edit_range(&self, range_utf16: Option<&Range<usize>>) -> Range<usize> {
        range_utf16
            .map(|range| self.range_from_utf16(range))
            .or_else(|| self.marked_range.clone())
            .unwrap_or_else(|| self.selected_range.clone())
    }

    fn splice(&mut self, range: &Range<usize>, new_text: &str) {
        self.content =
            (self.content[..range.start].to_owned() + new_text + &self.content[range.end..]).into();
        self.goal_x = None;
    }
}

/// Pasted text as one line: every line break, CRLF included, becomes one
/// space.
pub fn single_line(text: &str) -> String {
    text.replace("\r\n", " ").replace(['\r', '\n'], " ")
}

/// Pasted text for a multi-line input: every CRLF and lone CR becomes one LF.
pub fn normalize_newlines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// The byte offset of the grapheme boundary before `offset`, or 0.
pub fn previous_boundary(text: &str, offset: usize) -> usize {
    text.grapheme_indices(true)
        .rev()
        .find_map(|(index, _)| (index < offset).then_some(index))
        .unwrap_or(0)
}

/// The byte offset of the grapheme boundary after `offset`, or the end.
pub fn next_boundary(text: &str, offset: usize) -> usize {
    text.grapheme_indices(true)
        .find_map(|(index, _)| (index > offset).then_some(index))
        .unwrap_or(text.len())
}

/// The byte offset of the UTF-16 offset `utf16` (the IME counts in UTF-16).
pub fn offset_from_utf16(text: &str, utf16: usize) -> usize {
    let mut utf8_offset = 0;
    let mut utf16_count = 0;
    for ch in text.chars() {
        if utf16_count >= utf16 {
            break;
        }
        utf16_count += ch.len_utf16();
        utf8_offset += ch.len_utf8();
    }
    utf8_offset
}

/// The UTF-16 offset of the byte offset `utf8`.
pub fn offset_to_utf16(text: &str, utf8: usize) -> usize {
    let mut utf16_offset = 0;
    let mut utf8_count = 0;
    for ch in text.chars() {
        if utf8_count >= utf8 {
            break;
        }
        utf8_count += ch.len_utf8();
        utf16_offset += ch.len_utf16();
    }
    utf16_offset
}

/// The scroll that keeps the caret's row in a view `view_height` tall, the
/// row's top `caret_y` below the first row's and the rows `content_height`
/// tall in all, moving as little as it can from `scroll_y`.
fn scroll_to_caret(
    scroll_y: Pixels,
    caret_y: Pixels,
    line_height: Pixels,
    view_height: Pixels,
    content_height: Pixels,
) -> Pixels {
    let mut scroll = scroll_y;
    if caret_y < scroll {
        scroll = caret_y;
    } else if caret_y + line_height > scroll + view_height {
        scroll = caret_y + line_height - view_height;
    }
    let max_scroll = content_height - view_height;
    if scroll > max_scroll {
        scroll = max_scroll;
    }
    if scroll < px(0.) {
        scroll = px(0.);
    }
    scroll
}

/// A drawn input's shaped text: one [`WrappedLine`] a hard line, and the
/// visual rows they wrap into. A single-line input's text is one row.
struct TextLayout {
    lines: Vec<WrappedLine>,
    rows: Vec<Row>,
    line_height: Pixels,
}

/// One visual row: the byte range `start..end` of the text, in hard line
/// `line`, which starts at byte `line_start`.
#[derive(Debug, Clone, Copy)]
struct Row {
    line: usize,
    line_start: usize,
    start: usize,
    end: usize,
    /// Where the row starts in its hard line's unwrapped layout.
    start_x: Pixels,
    /// The row ends at a soft wrap, not at a line break or the end of the
    /// text.
    wrapped: bool,
}

impl Row {
    /// The last offset the caret can take and still show on this row: at a
    /// soft wrap, the row's end is the next row's start.
    fn caret_end(&self, text: &str) -> usize {
        if self.wrapped && self.end > self.start {
            previous_boundary(text, self.end).max(self.start)
        } else {
            self.end
        }
    }
}

impl TextLayout {
    fn new(lines: Vec<WrappedLine>, line_height: Pixels) -> Self {
        let mut rows = Vec::new();
        let mut line_start = 0;
        for (line, wrapped_line) in lines.iter().enumerate() {
            let unwrapped = &wrapped_line.unwrapped_layout;
            let mut start = 0;
            let mut start_x = px(0.);
            for boundary in wrapped_line.wrap_boundaries() {
                let Some(glyph) = unwrapped
                    .runs
                    .get(boundary.run_ix)
                    .and_then(|run| run.glyphs.get(boundary.glyph_ix))
                else {
                    continue;
                };
                rows.push(Row {
                    line,
                    line_start,
                    start: line_start + start,
                    end: line_start + glyph.index,
                    start_x,
                    wrapped: true,
                });
                start = glyph.index;
                start_x = glyph.position.x;
            }
            rows.push(Row {
                line,
                line_start,
                start: line_start + start,
                end: line_start + wrapped_line.len(),
                start_x,
                wrapped: false,
            });
            line_start += wrapped_line.len() + 1;
        }
        Self {
            lines,
            rows,
            line_height,
        }
    }

    fn height(&self) -> Pixels {
        self.line_height * self.rows.len()
    }

    /// The row the caret at `offset` shows on: at a soft wrap, the later one.
    fn row_ix(&self, offset: usize) -> usize {
        self.rows
            .iter()
            .rposition(|row| row.start <= offset)
            .unwrap_or(0)
    }

    /// The row `y` below the first row's top falls in, clamped to the first
    /// and the last.
    fn row_at_y(&self, y: Pixels) -> usize {
        (0..self.rows.len())
            .rposition(|ix| self.line_height * ix <= y)
            .unwrap_or(0)
    }

    /// How far `offset` is from the left of its row `row`.
    fn x_in(&self, row: &Row, offset: usize) -> Pixels {
        self.lines.get(row.line).map_or(px(0.), |line| {
            line.unwrapped_layout
                .x_for_index(offset.saturating_sub(row.line_start))
                - row.start_x
        })
    }

    /// Where the caret at `offset` sits, from the first row's top left.
    fn position(&self, offset: usize) -> Point<Pixels> {
        let ix = self.row_ix(offset);
        self.rows.get(ix).map_or_else(Point::default, |row| {
            point(self.x_in(row, offset), self.line_height * ix)
        })
    }

    /// The caret offset in row `ix` closest to `x`.
    fn closest_in_row(&self, ix: usize, x: Pixels, text: &str) -> usize {
        let Some(row) = self.rows.get(ix) else {
            return text.len();
        };
        let Some(line) = self.lines.get(row.line) else {
            return row.start;
        };
        let offset = row.line_start + line.unwrapped_layout.closest_index_for_x(x + row.start_x);
        offset.clamp(row.start, row.caret_end(text))
    }

    /// The offset of the character under `x` in row `ix`, if there is one.
    fn index_in_row(&self, ix: usize, x: Pixels) -> Option<usize> {
        let row = self.rows.get(ix)?;
        let line = self.lines.get(row.line)?;
        let offset = row.line_start + line.unwrapped_layout.index_for_x(x + row.start_x)?;
        (row.start..=row.end).contains(&offset).then_some(offset)
    }

    /// One quad a row the selection `range` covers, `origin` the first row's
    /// top left. A row whose line break is selected shows a sliver past its
    /// end, so a selected empty line shows too.
    fn selection_quads(&self, range: &Range<usize>, origin: Point<Pixels>) -> Vec<PaintQuad> {
        let line_break_width = self.line_height / 4.;
        self.rows
            .iter()
            .enumerate()
            .filter_map(|(ix, row)| {
                if range.end < row.start || range.start > row.end {
                    return None;
                }
                let left = self.x_in(row, range.start.max(row.start));
                let mut right = self.x_in(row, range.end.min(row.end));
                if !row.wrapped && range.end > row.end {
                    right += line_break_width;
                }
                if right <= left {
                    return None;
                }
                let top = origin.y + self.line_height * ix;
                let corners = Bounds::from_corners(
                    point(origin.x + left, top),
                    point(origin.x + right, top + self.line_height),
                );
                Some(fill(corners, rgba(SELECTION_COLOR)))
            })
            .collect()
    }

    /// Paints each hard line at its first row, `origin` the first row's top
    /// left.
    fn paint(&self, origin: Point<Pixels>, window: &mut Window, cx: &mut App) {
        let mut painted = None;
        for (ix, row) in self.rows.iter().enumerate() {
            if painted == Some(row.line) {
                continue;
            }
            painted = Some(row.line);
            let Some(line) = self.lines.get(row.line) else {
                continue;
            };
            let at = point(origin.x, origin.y + self.line_height * ix);
            if let Err(err) = line.paint(at, self.line_height, TextAlign::Left, None, window, cx) {
                tracing::error!("painting a text input: {err:#}");
            }
        }
    }
}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_owned())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.read_only {
            return;
        }
        let range = self.edit_range(range_utf16.as_ref());
        self.splice(&range, new_text);
        let end = range.start + new_text.len();
        self.selected_range = end..end;
        self.marked_range = None;
        cx.emit(TextChanged);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.read_only {
            return;
        }
        let range = self.edit_range(range_utf16.as_ref());
        self.splice(&range, new_text);
        self.marked_range =
            (!new_text.is_empty()).then(|| range.start..range.start + new_text.len());
        let end = range.start + new_text.len();
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|selected| {
                offset_from_utf16(new_text, selected.start)
                    ..offset_from_utf16(new_text, selected.end)
            })
            .map_or(end..end, |selected| {
                range.start + selected.start..range.start + selected.end
            });
        cx.emit(TextChanged);
        cx.notify();
    }

    /// The range's box on the row it starts on: to its end when it ends on
    /// that row, else to the input's right edge.
    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let layout = self.last_layout.as_ref()?;
        let range = self.range_from_utf16(&range_utf16);
        let origin = point(bounds.left(), bounds.top() - self.scroll_y);
        let start = layout.position(range.start);
        let end = layout.position(range.end);
        let right = if end.y == start.y {
            origin.x + end.x
        } else {
            bounds.right()
        };
        Some(Bounds::from_corners(
            origin + start,
            point(right, origin.y + start.y + layout.line_height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        let local = self.last_bounds?.localize(&point)?;
        let layout = self.last_layout.as_ref()?;
        let row = layout.row_at_y(local.y + self.scroll_y);
        let utf8_index = layout.index_in_row(row, local.x)?;
        Some(offset_to_utf16(&self.content, utf8_index))
    }
}

impl Render for TextInput {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let context = if self.mode.is_multi_line() {
            MULTI_LINE_CONTEXT
        } else {
            CONTEXT
        };
        div()
            .flex()
            .w_full()
            .key_context(context)
            .track_focus(&self.focus_handle)
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::enter))
            .on_action(cx.listener(Self::newline))
            .on_action(cx.listener(Self::escape))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .child(TextElement { input: cx.entity() })
    }
}

/// Lays out, paints and takes IME input for a [`TextInput`].
struct TextElement {
    input: Entity<TextInput>,
}

struct PrepaintState {
    layout: Option<TextLayout>,
    /// The first row's top left: the element's, less the scroll.
    origin: Point<Pixels>,
    cursor: Option<PaintQuad>,
    selection: Vec<PaintQuad>,
}

impl IntoElement for TextElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

/// The runs of the displayed text: one, or three with the IME's marked text
/// underlined.
fn text_runs(run: TextRun, len: usize, marked: Option<&Range<usize>>) -> Vec<TextRun> {
    let Some(marked) = marked else {
        return vec![run];
    };
    let underline = UnderlineStyle {
        color: Some(run.color),
        thickness: px(1.0),
        wavy: false,
    };
    [
        TextRun {
            len: marked.start,
            ..run.clone()
        },
        TextRun {
            len: marked.end - marked.start,
            underline: Some(underline),
            ..run.clone()
        },
        TextRun {
            len: len - marked.end,
            ..run
        },
    ]
    .into_iter()
    .filter(|run| run.len > 0)
    .collect()
}

/// What an input shapes: its text, or the placeholder while it is empty, in
/// its runs and font size.
struct Shaping {
    text: SharedString,
    runs: Vec<TextRun>,
    font_size: Pixels,
}

impl Shaping {
    fn of(input: &TextInput, window: &Window) -> Self {
        let style = window.text_style();
        let (text, color) = if input.content.is_empty() {
            (input.placeholder.clone(), rgb(PLACEHOLDER_COLOR))
        } else {
            (input.content.clone(), rgb(TEXT_COLOR))
        };
        // A single-line input draws one row: a line break its owner set draws
        // as a space, byte for byte, so the offsets still hold.
        let text = if !input.mode.is_multi_line() && text.contains(['\r', '\n']) {
            text.replace(['\r', '\n'], " ").into()
        } else {
            text
        };
        let run = TextRun {
            len: text.len(),
            font: style.font(),
            color: color.into(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let runs = text_runs(run, text.len(), input.marked_range.as_ref());
        Self {
            font_size: style.font_size.to_pixels(window.rem_size()),
            text,
            runs,
        }
    }

    /// One [`WrappedLine`] a hard line, wrapped to `wrap_width` if given.
    fn shape(&self, window: &Window, wrap_width: Option<Pixels>) -> Option<Vec<WrappedLine>> {
        let shaped = window.text_system().shape_text(
            self.text.clone(),
            self.font_size,
            &self.runs,
            wrap_width,
            None,
        );
        match shaped {
            Ok(lines) => Some(lines.into_vec()),
            Err(err) => {
                tracing::error!("shaping a text input: {err:#}");
                None
            }
        }
    }
}

fn row_count(lines: &[WrappedLine]) -> usize {
    lines
        .iter()
        .map(|line| line.wrap_boundaries().len() + 1)
        .sum()
}

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    /// One line tall for a single-line input; for a multi-line one, as many
    /// rows as its text wraps into at the width it gets, within its bounds.
    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        let line_height = window.line_height();
        let input = self.input.read(cx);
        let Mode::MultiLine { min_rows, max_rows } = input.mode else {
            style.size.height = line_height.into();
            return (window.request_layout(style, [], cx), ());
        };
        let shaping = Shaping::of(input, window);
        let layout_id =
            window.request_measured_layout(style, move |known, available, window, _| {
                let width = known.width.or(match available.width {
                    AvailableSpace::Definite(width) => Some(width),
                    AvailableSpace::MinContent | AvailableSpace::MaxContent => None,
                });
                let rows = shaping
                    .shape(window, width)
                    .map_or(1, |lines| row_count(&lines));
                size(
                    known.width.unwrap_or(px(0.)),
                    line_height * rows.clamp(min_rows, max_rows),
                )
            });
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        (): &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let input = self.input.read(cx);
        let multi_line = input.mode.is_multi_line();
        let line_height = window.line_height();
        let wrap_width = multi_line.then_some(bounds.size.width);
        let Some(lines) = Shaping::of(input, window).shape(window, wrap_width) else {
            return PrepaintState {
                layout: None,
                origin: bounds.origin,
                cursor: None,
                selection: Vec::new(),
            };
        };
        let layout = TextLayout::new(lines, line_height);
        let caret = layout.position(input.cursor_offset());
        let selected = input.selected_range.clone();
        let scroll_y = if multi_line {
            let view_height = bounds.size.height;
            scroll_to_caret(
                input.scroll_y,
                caret.y,
                line_height,
                view_height,
                layout.height(),
            )
        } else {
            px(0.)
        };
        let origin = point(bounds.left(), bounds.top() - scroll_y);
        let (selection, cursor) = if selected.is_empty() {
            let caret = Bounds::new(origin + caret, size(px(2.), line_height));
            (Vec::new(), Some(fill(caret, rgb(CURSOR_COLOR))))
        } else {
            (layout.selection_quads(&selected, origin), None)
        };
        self.input.update(cx, |input, _| input.scroll_y = scroll_y);
        PrepaintState {
            layout: Some(layout),
            origin,
            cursor,
            selection,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        (): &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let input = self.input.read(cx);
        let focus_handle = input.focus_handle.clone();
        let mask = input.mode.is_multi_line().then_some(ContentMask { bounds });
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        let Some(layout) = prepaint.layout.take() else {
            return;
        };
        let focused = focus_handle.is_focused(window);
        window.with_content_mask(mask, |window| {
            for selection in prepaint.selection.drain(..) {
                window.paint_quad(selection);
            }
            layout.paint(prepaint.origin, window, cx);
            if focused && let Some(cursor) = prepaint.cursor.take() {
                window.paint_quad(cursor);
            }
        });
        self.input.update(cx, |input, _| {
            input.last_layout = Some(layout);
            input.last_bounds = Some(bounds);
        });
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use gpui::{ClipboardItem, Entity, EntityInputHandler as _, TestAppContext, VisualTestContext};

    use super::{
        TextChanged, TextInput, TextInputEvent, bind_keys, next_boundary, offset_from_utf16,
        offset_to_utf16, previous_boundary, single_line,
    };

    /// A focused, drawn input in its own window holding `text`, the cursor at
    /// its end.
    fn open<'a>(
        cx: &'a mut TestAppContext,
        build: impl FnOnce(&mut gpui::Context<TextInput>) -> TextInput,
        text: &str,
    ) -> (Entity<TextInput>, &'a mut VisualTestContext) {
        cx.update(bind_keys);
        let (input, cx) = cx.add_window_view(|_, cx| build(cx));
        cx.update(|window, cx| {
            input.update(cx, |input, cx| {
                input.set_text(text.to_owned(), cx);
                input.focus_handle.focus(window);
            });
        });
        cx.run_until_parked();
        (input, cx)
    }

    fn single(cx: &mut gpui::Context<TextInput>) -> TextInput {
        TextInput::new("", "", cx)
    }

    fn multi(cx: &mut gpui::Context<TextInput>) -> TextInput {
        TextInput::multi_line("", "", cx)
    }

    fn text_of(input: &Entity<TextInput>, cx: &mut VisualTestContext) -> String {
        cx.update(|_, cx| input.read(cx).text().to_owned())
    }

    /// The selection as (anchor, cursor).
    fn caret(input: &Entity<TextInput>, cx: &mut VisualTestContext) -> (usize, usize) {
        cx.update(|_, cx| {
            let input = input.read(cx);
            let range = input.selected_range.clone();
            if input.selection_reversed {
                (range.end, range.start)
            } else {
                (range.start, range.end)
            }
        })
    }

    /// Counts the [`TextInputEvent`]s the input emits.
    fn events(
        input: &Entity<TextInput>,
        cx: &mut VisualTestContext,
    ) -> Rc<std::cell::RefCell<Vec<TextInputEvent>>> {
        let seen = Rc::new(std::cell::RefCell::new(Vec::new()));
        let sink = Rc::clone(&seen);
        cx.update(|_, cx| {
            cx.subscribe(input, move |_, event: &TextInputEvent, _| {
                sink.borrow_mut().push(*event);
            })
            .detach();
        });
        seen
    }

    #[gpui::test]
    fn text_input_multi_line_paste_keeps_and_normalises_newlines(cx: &mut TestAppContext) {
        let (input, cx) = open(cx, multi, "");
        cx.write_to_clipboard(ClipboardItem::new_string("a\r\nb\rc\nd".to_owned()));
        cx.simulate_keystrokes("ctrl-v");
        assert_eq!(text_of(&input, cx), "a\nb\nc\nd");
    }

    #[gpui::test]
    fn text_input_single_line_paste_flattens_newlines(cx: &mut TestAppContext) {
        let (input, cx) = open(cx, single, "");
        cx.write_to_clipboard(ClipboardItem::new_string("a\r\nb\rc\nd".to_owned()));
        cx.simulate_keystrokes("ctrl-v");
        assert_eq!(text_of(&input, cx), "a b c d");
    }

    #[gpui::test]
    fn text_input_enter_inserts_a_newline_in_multi_line_mode(cx: &mut TestAppContext) {
        let (input, cx) = open(cx, multi, "ab");
        let seen = events(&input, cx);
        cx.simulate_keystrokes("enter");
        assert_eq!(text_of(&input, cx), "ab\n");
        assert!(seen.borrow().is_empty(), "Enter is not a submit here");
    }

    #[gpui::test]
    fn text_input_enter_submits_in_single_line_mode(cx: &mut TestAppContext) {
        let (input, cx) = open(cx, single, "ab");
        let seen = events(&input, cx);
        cx.simulate_keystrokes("enter");
        assert_eq!(text_of(&input, cx), "ab");
        assert_eq!(*seen.borrow(), [TextInputEvent::Submit]);
    }

    #[gpui::test]
    fn text_input_ctrl_enter_submits_in_multi_line_mode(cx: &mut TestAppContext) {
        let (input, cx) = open(cx, multi, "ab");
        let seen = events(&input, cx);
        cx.simulate_keystrokes("ctrl-enter");
        assert_eq!(text_of(&input, cx), "ab", "Ctrl+Enter inserts nothing");
        assert_eq!(*seen.borrow(), [TextInputEvent::Submit]);
        cx.simulate_keystrokes("escape");
        assert_eq!(
            *seen.borrow(),
            [TextInputEvent::Submit, TextInputEvent::Cancel]
        );
    }

    #[gpui::test]
    fn text_input_up_and_down_keep_the_goal_column(cx: &mut TestAppContext) {
        let (input, cx) = open(cx, multi, "abcd\nx\nabcd");
        cx.simulate_keystrokes("up");
        assert_eq!(caret(&input, cx), (6, 6), "the short row's end");
        cx.simulate_keystrokes("up");
        assert_eq!(caret(&input, cx), (4, 4), "back at the goal column");
        cx.simulate_keystrokes("up");
        assert_eq!(caret(&input, cx), (0, 0), "the first row goes to the start");
        cx.simulate_keystrokes("down down");
        assert_eq!(caret(&input, cx), (11, 11), "the goal column survives");
        cx.simulate_keystrokes("home down");
        assert_eq!(caret(&input, cx), (11, 11), "the last row goes to the end");
        cx.simulate_keystrokes("end shift-up");
        assert_eq!(caret(&input, cx), (11, 6), "Shift extends the selection");
    }

    #[gpui::test]
    fn text_input_home_and_end_go_to_the_row_boundaries(cx: &mut TestAppContext) {
        let (input, cx) = open(cx, multi, "abcd\nefgh");
        cx.simulate_keystrokes("home");
        assert_eq!(caret(&input, cx), (5, 5));
        cx.simulate_keystrokes("end");
        assert_eq!(caret(&input, cx), (9, 9));
        cx.simulate_keystrokes("up home");
        assert_eq!(caret(&input, cx), (0, 0));
        cx.simulate_keystrokes("end");
        assert_eq!(caret(&input, cx), (4, 4));
    }

    #[gpui::test]
    fn text_input_single_line_newlines_stay_on_one_row(cx: &mut TestAppContext) {
        let (input, cx) = open(cx, single, "ab\ncd");
        let rows = cx.update(|_, cx| input.read(cx).last_layout.as_ref().map(|l| l.rows.len()));
        assert_eq!(rows, Some(1), "a line break draws as a space");
        cx.simulate_keystrokes("home");
        assert_eq!(caret(&input, cx), (0, 0), "Home goes to the text's start");
        cx.simulate_keystrokes("end");
        assert_eq!(caret(&input, cx), (5, 5), "End goes to the text's end");
        assert_eq!(text_of(&input, cx), "ab\ncd", "the text itself is kept");
    }

    #[gpui::test]
    fn text_input_multi_line_normalises_newlines_it_is_given(cx: &mut TestAppContext) {
        let (built, vcx) = cx.add_window_view(|_, cx| TextInput::multi_line("x\r\ny", "", cx));
        assert_eq!(text_of(&built, vcx), "x\ny");
        assert_eq!(caret(&built, vcx), (0, 3), "all of it selected");
        let (input, cx) = open(cx, multi, "a\r\nb\rc");
        assert_eq!(text_of(&input, cx), "a\nb\nc");
        assert_eq!(caret(&input, cx), (5, 5), "the cursor at the end");
    }

    #[gpui::test]
    fn text_input_read_only_drops_the_composition(cx: &mut TestAppContext) {
        let (input, cx) = open(cx, multi, "ab");
        let changes = Rc::new(Cell::new(0));
        let counter = Rc::clone(&changes);
        cx.update(|window, cx| {
            cx.subscribe(&input, move |_, _: &TextChanged, _| {
                counter.set(counter.get() + 1);
            })
            .detach();
            input.update(cx, |input, cx| {
                input.replace_and_mark_text_in_range(None, "\u{304b}", None, window, cx);
            });
        });
        assert_eq!(text_of(&input, cx), "ab\u{304b}");
        cx.update(|_, cx| input.update(cx, |input, cx| input.set_read_only(true, cx)));
        cx.run_until_parked();
        assert_eq!(text_of(&input, cx), "ab", "the half-composed text is gone");
        assert_eq!(caret(&input, cx), (2, 2));
        let marked = cx.update(|_, cx| input.read(cx).marked_range.clone());
        assert_eq!(marked, None);
        assert_eq!(changes.get(), 2, "composing and dropping it are both edits");
    }

    #[gpui::test]
    fn text_input_read_only_ignores_edits_but_copies(cx: &mut TestAppContext) {
        let (input, cx) = open(cx, multi, "seed");
        cx.update(|_, cx| input.update(cx, |input, cx| input.set_read_only(true, cx)));
        cx.write_to_clipboard(ClipboardItem::new_string("zz".to_owned()));
        cx.simulate_input("x");
        cx.simulate_keystrokes("ctrl-v backspace enter");
        assert_eq!(text_of(&input, cx), "seed");
        cx.simulate_keystrokes("ctrl-a ctrl-c");
        assert_eq!(caret(&input, cx), (0, 4), "select-all still selects");
        let copied = cx.read_from_clipboard().and_then(|item| item.text());
        assert_eq!(copied.as_deref(), Some("seed"));
    }

    #[gpui::test]
    fn text_input_multi_line_height_clamps_between_min_and_max_rows(cx: &mut TestAppContext) {
        /// The drawn height in rows, if it is a whole number of them.
        fn rows(
            cx: &mut TestAppContext,
            build: fn(&mut gpui::Context<TextInput>) -> TextInput,
            text: &str,
        ) -> Option<usize> {
            let (input, cx) = open(cx, build, text);
            let line_height = cx.update(|window, _| window.line_height());
            let height = cx.update(|_, cx| input.read(cx).last_bounds)?.size.height;
            (0..20).find(|rows| line_height * *rows == height)
        }
        assert_eq!(rows(cx, multi, "one"), Some(2), "min_rows");
        assert_eq!(rows(cx, multi, "1\n2\n3"), Some(3));
        assert_eq!(
            rows(cx, multi, "1\n2\n3\n4\n5\n6\n7\n8"),
            Some(6),
            "max_rows"
        );
        assert_eq!(
            rows(
                cx,
                |cx| TextInput::multi_line("", "", cx).with_rows(1, 3),
                "a\nb\nc\nd"
            ),
            Some(3)
        );
        assert_eq!(rows(cx, single, "one"), Some(1), "single-line is one row");
    }

    #[gpui::test]
    fn text_input_set_text_is_silent_and_edits_emit_changed(cx: &mut TestAppContext) {
        let (input, cx) = cx.add_window_view(|_, cx| TextInput::new("seed", "", cx));
        let changes = Rc::new(Cell::new(0));
        let counter = Rc::clone(&changes);
        cx.update(|_, cx| {
            cx.subscribe(&input, move |_, _: &TextChanged, _| {
                counter.set(counter.get() + 1);
            })
            .detach();
        });

        cx.update(|_, cx| input.update(cx, |input, cx| input.set_text("wt/brave-fox", cx)));
        cx.run_until_parked();
        assert_eq!(changes.get(), 0, "set_text is the owner's, not the user's");
        let text = cx.update(|_, cx| input.read(cx).text().to_owned());
        assert_eq!(text, "wt/brave-fox");

        cx.update(|window, cx| {
            input.update(cx, |input, cx| {
                input.replace_text_in_range(None, "!", window, cx);
            });
        });
        cx.run_until_parked();
        assert_eq!(changes.get(), 1, "typing is an edit");
        let text = cx.update(|_, cx| input.read(cx).text().to_owned());
        assert_eq!(text, "wt/brave-fox!", "set_text left the cursor at the end");
    }

    /// "a", a thumbs-up with a skin tone (two chars, one grapheme), then an
    /// "e" with a combining acute (two chars, one grapheme), then "b".
    const TEXT: &str = "a\u{1f44d}\u{1f3fd}e\u{301}b";
    const THUMB: usize = 1;
    const E: usize = 1 + 4 + 4;
    const B: usize = E + 1 + 2;

    #[test]
    fn text_input_paste_is_one_line() {
        assert_eq!(single_line("a\r\nb\nc\rd"), "a b c d");
        assert_eq!(
            single_line("\r\n\r\n"),
            "  ",
            "each line break is one space"
        );
        assert_eq!(single_line("plain"), "plain");
    }

    #[test]
    fn text_input_boundaries_step_over_whole_graphemes() {
        assert_eq!(next_boundary(TEXT, 0), THUMB);
        assert_eq!(next_boundary(TEXT, THUMB), E);
        assert_eq!(next_boundary(TEXT, E), B);
        assert_eq!(next_boundary(TEXT, B), TEXT.len());
        assert_eq!(previous_boundary(TEXT, TEXT.len()), B);
        assert_eq!(previous_boundary(TEXT, B), E);
        assert_eq!(previous_boundary(TEXT, E), THUMB);
        assert_eq!(previous_boundary(TEXT, THUMB), 0);
        assert_eq!(previous_boundary(TEXT, 0), 0);
        assert_eq!(next_boundary("", 0), 0);
    }

    #[test]
    fn text_input_utf16_offsets_count_surrogate_pairs() {
        assert_eq!(offset_to_utf16(TEXT, THUMB), 1);
        assert_eq!(offset_to_utf16(TEXT, E), 5, "two surrogate pairs");
        assert_eq!(offset_to_utf16(TEXT, TEXT.len()), 8);
        assert_eq!(offset_from_utf16(TEXT, 5), E);
        assert_eq!(offset_from_utf16(TEXT, 8), TEXT.len());
        assert_eq!(offset_from_utf16(TEXT, 99), TEXT.len());
        for utf8 in [0, THUMB, E, B, TEXT.len()] {
            assert_eq!(offset_from_utf16(TEXT, offset_to_utf16(TEXT, utf8)), utf8);
        }
    }
}
