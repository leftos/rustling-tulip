//! A single-line text input, trimmed from gpui's `examples/input.rs`: typing
//! and IME, backspace and delete, the arrows, Home and End, Shift selection,
//! select-all, copy, cut and paste. Enter emits [`TextInputEvent::Submit`]
//! and Esc [`TextInputEvent::Cancel`]; every edit the user makes emits
//! [`TextChanged`], and [`TextInput::set_text`] replaces the text without it.

use std::ops::Range;

use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, ElementId, ElementInputHandler, Entity,
    EntityInputHandler, EventEmitter, FocusHandle, Focusable, GlobalElementId, KeyBinding,
    LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point,
    ShapedLine, SharedString, Style, TextRun, UTF16Selection, UnderlineStyle, Window, actions, div,
    fill, point, prelude::*, px, relative, rgb, rgba, size,
};
use unicode_segmentation::UnicodeSegmentation as _;

/// The key context the input's bindings apply in.
const CONTEXT: &str = "TextInput";
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
        SelectLeft,
        SelectRight,
        SelectAll,
        Home,
        End,
        Paste,
        Cut,
        Copy,
        Enter,
        Escape,
    ]
);

/// Binds the input's keys, active only while an input has the keyboard.
pub fn bind_keys(cx: &mut App) {
    let context = Some(CONTEXT);
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
        KeyBinding::new("enter", Enter, context),
        KeyBinding::new("escape", Escape, context),
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
    last_layout: Option<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
    is_selecting: bool,
}

impl EventEmitter<TextInputEvent> for TextInput {}

impl EventEmitter<TextChanged> for TextInput {}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl TextInput {
    /// An input holding `content`, all of it selected.
    pub fn new(
        content: impl Into<SharedString>,
        placeholder: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> Self {
        let content = content.into();
        Self {
            focus_handle: cx.focus_handle(),
            selected_range: 0..content.len(),
            content,
            placeholder: placeholder.into(),
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
            is_selecting: false,
        }
    }

    pub fn text(&self) -> &str {
        &self.content
    }

    /// Replaces the text, the cursor at its end, without [`TextChanged`]:
    /// the owner is setting it, not the user.
    pub fn set_text(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.content = text.into();
        let end = self.content.len();
        self.selected_range = end..end;
        self.selection_reversed = false;
        self.marked_range = None;
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

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(previous_boundary(&self.content, self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(next_boundary(&self.content, self.cursor_offset()), cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx);
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.content.len(), cx);
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(previous_boundary(&self.content, self.cursor_offset()), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(next_boundary(&self.content, self.cursor_offset()), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn enter(_: &mut Self, _: &Enter, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(TextInputEvent::Submit);
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
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.replace_text_in_range(None, &single_line(&text), window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_owned(),
            ));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_owned(),
            ));
            self.replace_text_in_range(None, "", window, cx);
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        cx.notify();
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
        let (Some(bounds), Some(line)) = (self.last_bounds.as_ref(), self.last_layout.as_ref())
        else {
            return 0;
        };
        if position.y < bounds.top() {
            return 0;
        }
        if position.y > bounds.bottom() {
            return self.content.len();
        }
        line.closest_index_for_x(position.x - bounds.left())
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
    }
}

/// Pasted text as one line: every line break, CRLF included, becomes one
/// space.
pub fn single_line(text: &str) -> String {
    text.replace("\r\n", " ").replace(['\r', '\n'], " ")
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

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let last_layout = self.last_layout.as_ref()?;
        let range = self.range_from_utf16(&range_utf16);
        Some(Bounds::from_corners(
            point(
                bounds.left() + last_layout.x_for_index(range.start),
                bounds.top(),
            ),
            point(
                bounds.left() + last_layout.x_for_index(range.end),
                bounds.bottom(),
            ),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        let line_point = self.last_bounds?.localize(&point)?;
        let last_layout = self.last_layout.as_ref()?;
        let utf8_index = last_layout.index_for_x(line_point.x)?;
        Some(offset_to_utf16(&self.content, utf8_index))
    }
}

impl Render for TextInput {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .w_full()
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::enter))
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
    line: Option<ShapedLine>,
    cursor: Option<PaintQuad>,
    selection: Option<PaintQuad>,
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

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], cx), ())
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
        let style = window.text_style();
        let (display_text, color) = if input.content.is_empty() {
            (input.placeholder.clone(), rgb(PLACEHOLDER_COLOR))
        } else {
            (input.content.clone(), rgb(TEXT_COLOR))
        };
        let run = TextRun {
            len: display_text.len(),
            font: style.font(),
            color: color.into(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let runs = text_runs(run, display_text.len(), input.marked_range.as_ref());
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line = window
            .text_system()
            .shape_line(display_text, font_size, &runs, None);
        let selected = input.selected_range.clone();
        let (selection, cursor) = if selected.is_empty() {
            let x = bounds.left() + line.x_for_index(input.cursor_offset());
            let caret = Bounds::new(point(x, bounds.top()), size(px(2.), bounds.size.height));
            (None, Some(fill(caret, rgb(CURSOR_COLOR))))
        } else {
            let from = point(
                bounds.left() + line.x_for_index(selected.start),
                bounds.top(),
            );
            let to = point(
                bounds.left() + line.x_for_index(selected.end),
                bounds.bottom(),
            );
            (
                Some(fill(Bounds::from_corners(from, to), rgba(SELECTION_COLOR))),
                None,
            )
        };
        PrepaintState {
            line: Some(line),
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
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        if let Some(selection) = prepaint.selection.take() {
            window.paint_quad(selection);
        }
        let Some(line) = prepaint.line.take() else {
            return;
        };
        if let Err(err) = line.paint(bounds.origin, window.line_height(), window, cx) {
            tracing::error!("painting a text input: {err:#}");
        }
        if focus_handle.is_focused(window)
            && let Some(cursor) = prepaint.cursor.take()
        {
            window.paint_quad(cursor);
        }
        self.input.update(cx, |input, _| {
            input.last_layout = Some(line);
            input.last_bounds = Some(bounds);
        });
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use gpui::{EntityInputHandler as _, TestAppContext};

    use super::{
        TextChanged, TextInput, next_boundary, offset_from_utf16, offset_to_utf16,
        previous_boundary, single_line,
    };

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
