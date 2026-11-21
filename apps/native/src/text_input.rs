//! A text input, trimmed from gpui's `examples/input.rs`: typing and IME,
//! backspace and delete, the arrows, Home and End, Shift selection,
//! select-all, copy, cut and paste. [`TextInput::new`] makes a single-line
//! input, where Enter emits [`TextInputEvent::Submit`].
//! [`TextInput::multi_line`] makes one that wraps to its width and grows from
//! a minimum to a maximum number of rows, scrolling past that; there Enter
//! inserts a line break, Ctrl+Enter submits, and Up and Down move between
//! rows; a single-line input emits [`NavKey`] for them instead. Esc emits
//! [`TextInputEvent::Cancel`] in both. Every edit the user
//! makes emits [`TextChanged`], and [`TextInput::set_text`] replaces the text
//! without it. A read-only input ([`TextInput::set_read_only`]) ignores edits.
//! A masked input ([`TextInput::set_masked`]) draws a dot for each character
//! and refuses copy and cut.

use std::ops::Range;
use std::time::Instant;

use gpui::{
    AnyElement, App, AvailableSpace, Bounds, ClickEvent, ClipboardEntry, ClipboardItem,
    ContentMask, Context, CursorStyle, DispatchPhase, ElementId, ElementInputHandler, Entity,
    EntityInputHandler, EventEmitter, FocusHandle, Focusable, GestureTuning, GlobalElementId,
    Hitbox, HitboxBehavior, Image, ImageFormat, KeyBinding, KeyContext, LayoutId, LongPressEvent,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point,
    ScrollEvent, SharedString, Style, TextAlign, TextInputConfiguration, TextRun, TouchDragEvent,
    TouchPhase, UTF16Selection, UnderlineStyle, WeakEntity, Window, WrappedLine, actions, canvas,
    deferred, div, fill, point, prelude::*, px, relative, rgb, rgba, size,
};
use unicode_segmentation::UnicodeSegmentation as _;

use crate::buttons::{ButtonKind, ButtonSize, Layer, button, button_look};
use crate::completion::{Token, token_at};
use crate::driver_id::DriverId as _;
use crate::touch_edit::{
    BarItem, SelectionBox, bar_items, bar_origin, drag_handle, near_caret, word_range,
};
use crate::ui_theme::{UiTheme, theme};

/// The key context a single-line input's bindings apply in.
const CONTEXT: &str = "TextInput";
/// The key context of a multi-line input, its own so Enter and the vertical
/// arrows bind differently there.
const MULTI_LINE_CONTEXT: &str = "TextInputMultiLine";
const DEFAULT_MIN_ROWS: usize = 2;
const DEFAULT_MAX_ROWS: usize = 6;
/// What a masked input draws in place of each character.
const MASK: char = '•';

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
        CompletionUp,
        CompletionDown,
        CompletionAccept,
        CompletionClose,
    ]
);

/// The key context a multi-line input adds while its owner's completion
/// popup is open, so Up, Down, Tab, Enter and Esc bind to the popup.
const COMPLETION_OPEN_CONTEXT: &str = "CompletionOpen";

/// The platform's own modifier for a `secondary-` binding: gpui's keystroke parser rejects `secondary-`.
fn secondary_key(key: &'static str) -> &'static str {
    let apple = cfg!(any(target_os = "macos", target_os = "ios"));
    match (apple, key) {
        (true, "a") => "cmd-a",
        (true, "v") => "cmd-v",
        (true, "c") => "cmd-c",
        (true, _) => "cmd-x",
        (false, "a") => "ctrl-a",
        (false, "v") => "ctrl-v",
        (false, "c") => "ctrl-c",
        (false, _) => "ctrl-x",
    }
}

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
            KeyBinding::new(secondary_key("a"), SelectAll, context),
            KeyBinding::new(secondary_key("v"), Paste, context),
            KeyBinding::new(secondary_key("c"), Copy, context),
            KeyBinding::new(secondary_key("x"), Cut, context),
            KeyBinding::new("home", Home, context),
            KeyBinding::new("end", End, context),
            KeyBinding::new("escape", Escape, context),
        ]);
    }
    cx.bind_keys([
        KeyBinding::new("enter", Enter, Some(CONTEXT)),
        KeyBinding::new("up", Up, Some(CONTEXT)),
        KeyBinding::new("down", Down, Some(CONTEXT)),
    ]);
    let multi_line = Some(MULTI_LINE_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("enter", Newline, multi_line),
        KeyBinding::new("ctrl-enter", Enter, multi_line),
        KeyBinding::new("up", Up, multi_line),
        KeyBinding::new("down", Down, multi_line),
        KeyBinding::new("shift-up", SelectUp, multi_line),
        KeyBinding::new("shift-down", SelectDown, multi_line),
    ]);
    // Bound after the input's own keys, so they win over them while the popup is open.
    let open = Some("TextInputMultiLine && CompletionOpen");
    cx.bind_keys([
        KeyBinding::new("up", CompletionUp, open),
        KeyBinding::new("down", CompletionDown, open),
        KeyBinding::new("tab", CompletionAccept, open),
        KeyBinding::new("enter", CompletionAccept, open),
        KeyBinding::new("escape", CompletionClose, open),
    ]);
}

/// What a multi-line input with completion triggers
/// ([`TextInput::with_completion_triggers`]) tells its owner: the caret
/// entered or moved in a token, or left it (never during IME composition),
/// and, while the owner says its popup is open
/// ([`TextInput::set_completion_open`]), the popup's keys: Up, Down, Tab or
/// Enter (`Accept`) and Esc (`Close`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionEvent {
    TokenChanged(Token),
    TokenCleared,
    Up,
    Down,
    Accept,
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextInputEvent {
    Submit,
    Cancel,
}

/// An image pasted into an input with image paste on
/// ([`TextInput::with_image_paste`]): its bytes and the file name it goes
/// up under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImagePasted {
    pub name: String,
    pub bytes: Vec<u8>,
}

/// The image a paste of `item` takes: its first non-empty image entry that
/// is not a BMP, else its first non-empty BMP; none without an image.
fn clipboard_image_entry(item: &ClipboardItem) -> Option<&Image> {
    let images = || {
        item.entries().iter().filter_map(|entry| match entry {
            ClipboardEntry::Image(image) if !image.bytes.is_empty() => Some(image),
            _ => None,
        })
    };
    images()
        .find(|image| image.format != ImageFormat::Bmp)
        .or_else(|| images().next())
}

/// `image` as the file it goes up as, named as a terminal pane names a
/// pasted image (`paste.<extension>`): a BMP goes up as the PNG the app's
/// bitmap reader finds on the system clipboard, else as the BMP it is. A
/// failed bitmap read is logged.
fn image_upload(image: &Image, cx: &App) -> (String, Vec<u8>) {
    if image.format == ImageFormat::Bmp {
        match crate::clipboard_image::bitmap_reader(cx)() {
            Ok(Some(png)) => return ("paste.png".to_owned(), png),
            Ok(None) => {}
            Err(err) => tracing::warn!("reading the clipboard's bitmap failed: {err:#}"),
        }
    }
    (
        format!("paste.{}", crate::paste_upload::extension_of(image.format)),
        image.bytes.clone(),
    )
}

/// Up or Down in a single-line input, for an owner that navigates a list
/// with them; the key still reaches the input's parents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavKey {
    Up,
    Down,
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
    draws: Draws,
    /// The x a run of Up and Down aims for, so a short row passed on the way
    /// does not pull the caret left for good.
    goal_x: Option<Pixels>,
    /// How far a multi-line input's rows are scrolled up to keep the caret's
    /// row in view.
    scroll_y: Pixels,
    /// How far a single-line input's text is scrolled left to keep the
    /// caret in view.
    scroll_x: Pixels,
    /// How the platform's soft keyboard serves this input.
    input_config: TextInputConfiguration,
    /// What the app changed since the last draw, which the next draw reports
    /// to the platform's text input so its keyboard reads it anew.
    platform_report: PlatformReport,
    /// The completion hook: its triggers, the popup's open flag and the
    /// token last told to the owner.
    completion: CompletionHook,
    /// What a paste of an image does ([`Self::with_image_paste`]).
    image_paste: ImagePaste,
    /// Touch editing's state while it is on ([`Self::with_touch_editing`]);
    /// none keeps the input as a desktop one.
    touch: Option<TouchEditing>,
}

/// The side of a hit area that takes a finger, in px.
const TOUCH_TARGET: f32 = 44.0;
/// A selection handle's stem width, in px, as the phone chat's.
const HANDLE_STEM: f32 = 2.0;
/// A selection handle's knob diameter, in px, as the phone chat's.
const HANDLE_KNOB: f32 = 10.0;
/// The edit bar's padding around its buttons, and the gap between them, in px.
const BAR_PAD: f32 = 4.0;
/// The edit bar's corner radius, in px.
const BAR_RADIUS: f32 = 10.0;

/// An input's touch editing: the edit bar, the drag a finger holds and
/// what the last render drew.
#[derive(Debug, Default)]
struct TouchEditing {
    /// Whether the edit bar is open.
    bar_open: bool,
    /// The caret or handle drag a finger holds.
    drag: Option<TouchDrag>,
    /// The spots the last render drew the handles and the bar from.
    drawn: Option<TouchSpots>,
    /// Where the last render drew the edit bar, in window px.
    bar: Option<Bounds<Pixels>>,
    /// When and where the last tap landed, which a tap on the caret pairs
    /// with into a double tap.
    last_tap: Option<(Instant, Point<Pixels>)>,
}

/// What a finger's touch drag moves.
#[derive(Debug, Clone, Copy, PartialEq)]
enum DragKind {
    /// The caret, from a touch that started on it.
    Caret,
    /// A selection handle's end; `fixed` is the other end, and `aim` the
    /// vector added to the finger to find the text the end goes to.
    Handle { fixed: usize, aim: Point<Pixels> },
}

/// A touch drag the input holds.
#[derive(Debug, Clone, Copy, PartialEq)]
struct TouchDrag {
    kind: DragKind,
    /// Where the touch started, in window px.
    start: Point<Pixels>,
    /// Whether it has moved past the tap slop.
    strayed: bool,
}

/// Which end of the selection a handle marks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HandleSide {
    Start,
    End,
}

impl HandleSide {
    /// The handle's driver id.
    fn key(self) -> &'static str {
        match self {
            HandleSide::Start => "text-edit-handle-start",
            HandleSide::End => "text-edit-handle-end",
        }
    }
}

/// Where the selection's ends draw, in window px: each end's caret top-left,
/// the rows' height and the input's box.
#[derive(Debug, Clone, Copy, PartialEq)]
struct TouchSpots {
    start: Point<Pixels>,
    end: Point<Pixels>,
    line_height: Pixels,
    field: Bounds<Pixels>,
}

impl TouchSpots {
    /// The caret top-left of the `side` end.
    fn of(self, side: HandleSide) -> Point<Pixels> {
        match side {
            HandleSide::Start => self.start,
            HandleSide::End => self.end,
        }
    }

    /// Whether the row of the caret at `caret` shows inside the input.
    fn shows(self, caret: Point<Pixels>) -> bool {
        caret.y + self.line_height > self.field.top() && caret.y < self.field.bottom()
    }

    /// The selection the edit bar serves: its first row's top and its last
    /// row's bottom, kept inside the input, and the middle of a one-row
    /// selection, else the input's middle.
    fn selection_box(self) -> SelectionBox {
        let top = self.start.y.max(self.field.top());
        let bottom = (self.end.y + self.line_height)
            .min(self.field.bottom())
            .max(top);
        let x = if self.start.y == self.end.y {
            (self.start.x + self.end.x) / 2.0
        } else {
            self.field.center().x
        };
        SelectionBox { top, bottom, x }
    }
}

/// What a paste of a clipboard holding an image does in an input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImagePaste {
    /// Pastes the clipboard's text, as an input does by default.
    Text,
    /// Emits [`ImagePasted`] and inserts nothing.
    Emit,
}

/// A multi-line input's completion hook.
#[derive(Debug, Default)]
struct CompletionHook {
    /// The characters that start a token; none keeps the hook off.
    triggers: Vec<char>,
    /// The owner's popup is open, so its keys go to it.
    open: bool,
    /// The token last told to the owner.
    token: Option<Token>,
}

/// What the app (not the platform's own text input) changed since the last
/// draw. The order is the upgrade order: a text change also reports the
/// selection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
enum PlatformReport {
    /// Nothing to report.
    #[default]
    None,
    /// The selection or the caret moved.
    Selection,
    /// The text changed.
    Content,
}

/// What an input draws: its text, or while masked a [`MASK`] for each
/// character, refusing copy and cut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Draws {
    Text,
    Dots,
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

impl EventEmitter<NavKey> for TextInput {}

impl EventEmitter<CompletionEvent> for TextInput {}

impl EventEmitter<ImagePasted> for TextInput {}

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
            draws: Draws::Text,
            goal_x: None,
            scroll_y: px(0.),
            scroll_x: px(0.),
            input_config: TextInputConfiguration::default(),
            platform_report: PlatformReport::None,
            completion: CompletionHook::default(),
            image_paste: ImagePaste::Text,
            touch: None,
        }
    }

    /// Turns on touch editing, as a phone's text field edits: a double tap
    /// selects the word under it; a long press selects the word under it,
    /// or places the caret where there is none, and opens the edit bar
    /// (Cut, Copy, Paste, Select All); a selection draws a handle at each
    /// end that a finger drags; a touch drag starting on the caret moves it;
    /// a tap on the caret opens the bar. Off by default.
    #[must_use]
    pub fn with_touch_editing(mut self) -> Self {
        self.touch = Some(TouchEditing::default());
        self
    }

    /// Turns on image paste: a paste whose clipboard holds an image emits
    /// [`ImagePasted`] with its bytes and file name instead of inserting
    /// text. A clipboard with no image pastes its text as before.
    #[must_use]
    pub fn with_image_paste(mut self) -> Self {
        self.image_paste = ImagePaste::Emit;
        self
    }

    /// Turns on the completion hook of a multi-line input for `triggers`
    /// (see [`crate::completion::token_at`]): it emits [`CompletionEvent`]s
    /// as the caret enters, moves in and leaves a token. A single-line input
    /// keeps the hook off.
    #[must_use]
    pub fn with_completion_triggers(mut self, triggers: &[char]) -> Self {
        if self.mode.is_multi_line() {
            self.completion.triggers = triggers.to_vec();
        }
        self
    }

    /// The completion token the caret sits in, as last told to the owner;
    /// none while the hook is off. An owner whose subscription came after
    /// the input was built reads it here.
    #[must_use]
    pub fn completion_token(&self) -> Option<&Token> {
        self.completion.token.as_ref()
    }

    /// The owner's popup opened or closed: while open, Up, Down, Tab, Enter
    /// and Esc emit [`CompletionEvent`]s instead of acting; Ctrl+Enter still
    /// submits. An input without triggers ignores it.
    pub fn set_completion_open(&mut self, open: bool, cx: &mut Context<Self>) {
        let open = open && !self.completion.triggers.is_empty();
        if self.completion.open != open {
            self.completion.open = open;
            cx.notify();
        }
    }

    /// Accepts a completion pick: `range` (a token's, trigger included)
    /// becomes `insert` and one space, the caret after the space, as an
    /// edit ([`TextChanged`]). A range outside the text changes nothing, a
    /// read-only input takes none, and neither does one while an IME
    /// composition is marked: the composition is kept.
    pub fn accept_completion(
        &mut self,
        range: &Range<usize>,
        insert: &str,
        cx: &mut Context<Self>,
    ) {
        if self.read_only || self.marked_range.is_some() {
            return;
        }
        let Some((text, caret)) = crate::completion::apply_pick(&self.content, range, insert)
        else {
            return;
        };
        self.content = text.into();
        self.selected_range = caret..caret;
        self.selection_reversed = false;
        self.marked_range = None;
        self.goal_x = None;
        self.close_edit_bar();
        self.report_to_platform(PlatformReport::Content);
        cx.emit(TextChanged);
        self.check_token(cx);
        cx.notify();
    }

    /// Tells the owner when the caret's token changed: a token while the
    /// caret (an empty selection) sits in one, else its end. Nothing while
    /// the hook is off or an IME composition is marked.
    fn check_token(&mut self, cx: &mut Context<Self>) {
        if self.completion.triggers.is_empty() || self.marked_range.is_some() {
            return;
        }
        let token = if self.selected_range.is_empty() {
            token_at(
                &self.content,
                self.selected_range.end,
                &self.completion.triggers,
            )
        } else {
            None
        };
        if token == self.completion.token {
            return;
        }
        self.completion.token.clone_from(&token);
        cx.emit(token.map_or(CompletionEvent::TokenCleared, CompletionEvent::TokenChanged));
    }

    /// The key context the input's bindings apply in: a multi-line input's
    /// own, with [`COMPLETION_OPEN_CONTEXT`] while its popup is open.
    fn key_context(&self) -> KeyContext {
        let mut context = KeyContext::default();
        if self.mode.is_multi_line() {
            context.add(MULTI_LINE_CONTEXT);
            if self.completion.open {
                context.add(COMPLETION_OPEN_CONTEXT);
            }
        } else {
            context.add(CONTEXT);
        }
        context
    }

    fn completion_up(_: &mut Self, _: &CompletionUp, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(CompletionEvent::Up);
    }

    fn completion_down(_: &mut Self, _: &CompletionDown, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(CompletionEvent::Down);
    }

    fn completion_accept(
        _: &mut Self,
        _: &CompletionAccept,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.emit(CompletionEvent::Accept);
    }

    fn completion_close(_: &mut Self, _: &CompletionClose, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(CompletionEvent::Close);
    }

    /// How many rows a multi-line input shows: it grows with its text from
    /// `min_rows` to `max_rows` and scrolls past that. `min_rows` is at least
    /// one and `max_rows` at least `min_rows`. A single-line input stays one
    /// row whatever this says.
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
            self.report_to_platform(PlatformReport::Content);
            cx.emit(TextChanged);
        }
        self.read_only = read_only;
        self.marked_range = None;
        self.check_token(cx);
        cx.notify();
    }

    /// Sets how the platform's soft keyboard serves this input: its action
    /// key, autocorrect and autocapitalization.
    pub fn set_text_input_configuration(
        &mut self,
        config: TextInputConfiguration,
        cx: &mut Context<Self>,
    ) {
        if self.input_config == config {
            return;
        }
        self.input_config = config;
        cx.notify();
    }

    /// Masks the input, or shows its text again. A masked input draws a `•`
    /// for each character (a line break stays one), with the caret and the
    /// selection where the hidden text puts them; copy and cut do nothing
    /// while it is masked, and typing and paste still work.
    pub fn set_masked(&mut self, masked: bool, cx: &mut Context<Self>) {
        let draws = if masked { Draws::Dots } else { Draws::Text };
        if self.draws != draws {
            self.draws = draws;
            // The platform reads the dots or the text anew; the draw tells it only while focused.
            self.report_to_platform(PlatformReport::Content);
            cx.notify();
        }
    }

    /// Pastes the clipboard's text at the caret, or over the selection, as
    /// the [`Paste`] action does: newlines normalized in a multi-line input,
    /// one line in a single-line one. A clipboard with no text changes
    /// nothing, and a read-only input takes none.
    /// With image paste on ([`Self::with_image_paste`]), a clipboard holding
    /// an image and no non-empty text emits [`ImagePasted`] instead and
    /// inserts nothing; text on the clipboard always pastes as text.
    pub fn paste_clipboard(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        let item = cx.read_from_clipboard();
        let text = item
            .as_ref()
            .and_then(ClipboardItem::text)
            .filter(|text| !text.is_empty());
        if text.is_none()
            && self.image_paste == ImagePaste::Emit
            && let Some(image) = item.as_ref().and_then(clipboard_image_entry)
        {
            let (name, bytes) = image_upload(image, cx);
            cx.emit(ImagePasted { name, bytes });
            return;
        }
        if let Some(text) = text {
            let text = if self.mode.is_multi_line() {
                normalize_newlines(&text)
            } else {
                single_line(&text)
            };
            self.replace_from_app(&text, cx);
        }
    }

    #[must_use]
    pub fn is_masked(&self) -> bool {
        self.draws == Draws::Dots
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.content
    }

    /// The byte range the input selects, its caret when empty.
    #[must_use]
    pub fn selected_range(&self) -> Range<usize> {
        self.selected_range.clone()
    }

    /// The text as drawn: the text itself, or while masked a [`MASK`] for
    /// each character but a line break.
    fn shown(&self) -> SharedString {
        if self.is_masked() {
            self.content
                .chars()
                .map(masked_char)
                .collect::<String>()
                .into()
        } else {
            self.content.clone()
        }
    }

    /// The offset in the drawn text of the text's byte `offset`.
    fn to_shown(&self, offset: usize) -> usize {
        if !self.is_masked() {
            return offset;
        }
        self.content
            .char_indices()
            .take_while(|(index, _)| *index < offset)
            .map(|(_, c)| masked_char(c).len_utf8())
            .sum()
    }

    /// The text's byte offset of the drawn text's offset `shown`: the start
    /// of the character it falls in or after.
    fn offset_of_shown(&self, shown: usize) -> usize {
        if !self.is_masked() {
            return shown;
        }
        let mut at = 0;
        for (index, c) in self.content.char_indices() {
            if at >= shown {
                return index;
            }
            at += masked_char(c).len_utf8();
        }
        self.content.len()
    }

    /// The selection drawn while the input does (`focused`) or does not
    /// have the keyboard: none while it does not, as for the caret, and
    /// none for an empty selection. The selection itself stays, so a field
    /// built all selected still replaces its text once it takes the keyboard.
    fn painted_selection(&self, focused: bool) -> Option<Range<usize>> {
        (focused && !self.selected_range.is_empty()).then(|| self.selected_range.clone())
    }

    /// The drawn text's range of the text's byte `range`.
    fn range_to_shown(&self, range: &Range<usize>) -> Range<usize> {
        self.to_shown(range.start)..self.to_shown(range.end)
    }

    /// Replaces the text, the cursor at its end, without [`TextChanged`]:
    /// the owner is setting it, not the user. The next draw tells the
    /// platform's text input of the change while the input has the keyboard.
    pub fn set_text(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.content = self.mode.accept(text.into());
        let end = self.content.len();
        self.selected_range = end..end;
        self.selection_reversed = false;
        self.marked_range = None;
        self.goal_x = None;
        self.close_edit_bar();
        self.report_to_platform(PlatformReport::Content);
        self.check_token(cx);
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
        if self.mode.is_multi_line() {
            self.move_vertically(false, false, cx);
        } else {
            cx.emit(NavKey::Up);
            cx.propagate();
        }
    }

    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        if self.mode.is_multi_line() {
            self.move_vertically(true, false, cx);
        } else {
            cx.emit(NavKey::Down);
            cx.propagate();
        }
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

    fn backspace(&mut self, _: &Backspace, _: &mut Window, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        if self.selected_range.is_empty() {
            self.select_to(previous_boundary(&self.content, self.cursor_offset()), cx);
        }
        self.replace_from_app("", cx);
    }

    fn delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        if self.selected_range.is_empty() {
            self.select_to(next_boundary(&self.content, self.cursor_offset()), cx);
        }
        self.replace_from_app("", cx);
    }

    fn enter(_: &mut Self, _: &Enter, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(TextInputEvent::Submit);
    }

    fn newline(&mut self, _: &Newline, _: &mut Window, cx: &mut Context<Self>) {
        self.replace_from_app("\n", cx);
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
        let focused = self.focus_handle.is_focused(window);
        self.focus_handle.focus(window, cx);
        if !self.read_only {
            crate::keyboard::raise(window);
        }
        if self.touch.is_some() && self.touch_tap(event, focused, cx) {
            return;
        }
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
        self.paste_clipboard(window, cx);
    }

    /// Copies the selection, unless the input is masked.
    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.is_masked() && !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_owned(),
            ));
        }
    }

    /// Copies the selection and, unless the input is read-only, deletes it.
    /// A masked input does neither.
    fn cut(&mut self, _: &Cut, _: &mut Window, cx: &mut Context<Self>) {
        if !self.is_masked() && !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_owned(),
            ));
            if !self.read_only {
                self.replace_from_app("", cx);
            }
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        self.goal_x = None;
        self.report_to_platform(PlatformReport::Selection);
        self.check_token(cx);
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
        let cursor = self.to_shown(self.cursor_offset());
        let row = layout.row_ix(cursor);
        let goal_x = self.goal_x.unwrap_or_else(|| layout.position(cursor).x);
        let target_row = if down {
            Some(row + 1).filter(|ix| *ix < layout.rows.len())
        } else {
            row.checked_sub(1)
        };
        let shown = self.shown();
        let target = target_row.map_or(text_edge, |ix| {
            self.offset_of_shown(layout.closest_in_row(ix, goal_x, &shown))
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
        let row = layout
            .rows
            .get(layout.row_ix(self.to_shown(self.cursor_offset())))?;
        let end = Row::caret_end(row, &self.shown());
        Some((
            self.clamp_offset(self.offset_of_shown(row.start)),
            self.clamp_offset(self.offset_of_shown(end)),
        ))
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
        let (Some(bounds), Some(_)) = (self.last_bounds.as_ref(), self.last_layout.as_ref()) else {
            return 0;
        };
        if position.y < bounds.top() {
            return 0;
        }
        if position.y > bounds.bottom() {
            return self.content.len();
        }
        self.index_at(position)
    }

    /// The caret offset closest to window point `position` in the drawn
    /// rows, a point above or below the input taking the rows scrolled out
    /// past its edge, clamped to the first and the last.
    fn index_at(&self, position: Point<Pixels>) -> usize {
        let (Some(bounds), Some(layout)) = (self.last_bounds.as_ref(), self.last_layout.as_ref())
        else {
            return 0;
        };
        if self.content.is_empty() {
            return 0;
        }
        let row = layout.row_at_y(position.y - bounds.top() + self.scroll_y);
        let x = position.x - bounds.left() + self.scroll_x;
        let shown = layout.closest_in_row(row, x, &self.shown());
        self.clamp_offset(self.offset_of_shown(shown))
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
        self.report_to_platform(PlatformReport::Selection);
        self.check_token(cx);
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

    /// Applies the platform's replacement of `range_utf16` (else the marked
    /// text, else the selection) with `new_text`. Return with no range in a
    /// single-line input submits instead.
    ///
    /// A composition marked outside an explicitly named range is kept, moved
    /// past the edit: `UIKit` still holds it, and its next edit would
    /// otherwise insert the composed text a second time. An edit on the
    /// marked text itself, or one overlapping it, drops it.
    ///
    /// Args:
    ///     `range_utf16`: The UTF-16 range the platform names, if any.
    ///     `new_text`: The text that replaces it.
    ///     `cx`: The input's context, for its events.
    fn replace_from_platform(
        &mut self,
        range_utf16: Option<&Range<usize>>,
        new_text: &str,
        cx: &mut Context<Self>,
    ) {
        if self.read_only {
            return;
        }
        // iOS delivers Return as an inserted line break, never as a key event.
        // A ranged replacement (UIKit's `replaceRange:withText:`) is an edit.
        if new_text == "\n" && range_utf16.is_none() && !self.mode.is_multi_line() {
            self.marked_range = None;
            cx.emit(TextInputEvent::Submit);
            cx.notify();
            return;
        }
        let range = self.edit_range(range_utf16);
        let caret = self.caret_after_replace(range_utf16.is_some(), &range, new_text.len());
        let marked = if range_utf16.is_some() {
            self.marked_range
                .clone()
                .and_then(|marked| marked_after_replace(&marked, &range, new_text.len()))
        } else {
            None
        };
        self.splice(&range, new_text);
        self.selected_range = caret..caret;
        self.marked_range = marked;
        self.close_edit_bar();
        cx.emit(TextChanged);
        self.check_token(cx);
        cx.notify();
    }

    /// Applies the app's own replacement of the marked text, else the
    /// selection, with `new_text` (paste, cut and the deleting and newline
    /// keys), as the platform's would apply, and marks it for the next draw
    /// to report to the platform's text input.
    ///
    /// Args:
    ///     `new_text`: The text that replaces it.
    ///     `cx`: The input's context, for its events.
    pub(crate) fn replace_from_app(&mut self, new_text: &str, cx: &mut Context<Self>) {
        self.replace_from_platform(None, new_text, cx);
        if !self.read_only {
            self.report_to_platform(PlatformReport::Content);
        }
    }

    /// Marks an app change for the next draw to report to the platform's
    /// text input, keeping a larger one already marked.
    ///
    /// Args:
    ///     `report`: What the app changed.
    fn report_to_platform(&mut self, report: PlatformReport) {
        self.platform_report = self.platform_report.max(report);
    }

    /// Where the caret lands once `range` holds `new_len` bytes.
    ///
    /// Args:
    ///     `explicit`: Whether the platform named the range, rather than the
    ///         edit falling on the marked text or the selection.
    ///     `range`: The byte range being replaced.
    ///     `new_len`: The replacement's length in bytes.
    ///
    /// Returns:
    ///     For a caret at or after an explicit range's end (autocorrect
    ///     behind the caret), the caret's own place shifted by the change in
    ///     length; for a caret before it, the caret's own place; else the end
    ///     of the replacement.
    fn caret_after_replace(&self, explicit: bool, range: &Range<usize>, new_len: usize) -> usize {
        let old = &self.selected_range;
        if explicit && old.is_empty() && old.start >= range.end {
            old.start - range.len() + new_len
        } else if explicit && old.is_empty() && old.end <= range.start {
            old.start
        } else {
            range.start + new_len
        }
    }

    /// Logs one call from the platform's text input at debug, under the
    /// `text_input::platform` target, so a detailed log shows the sequence a
    /// soft keyboard sends.
    ///
    /// Args:
    ///     `call`: The input handler method the platform called.
    ///     `range_utf16`: The UTF-16 range the call named, if any.
    ///     `new_text`: The text the call carried, if any; only its length in
    ///         characters is logged, since a masked input holds secrets.
    fn log_platform_call(
        &self,
        call: &str,
        range_utf16: Option<&Range<usize>>,
        new_text: Option<&str>,
    ) {
        tracing::debug!(
            target: "text_input::platform",
            call,
            range = ?range_utf16,
            new_chars = ?new_text.map(|text| text.chars().count()),
            selection = ?self.range_to_utf16(&self.selected_range),
            "platform text input call"
        );
    }

    fn splice(&mut self, range: &Range<usize>, new_text: &str) {
        self.content =
            (self.content[..range.start].to_owned() + new_text + &self.content[range.end..]).into();
        self.goal_x = None;
    }
}

/// Touch editing ([`TextInput::with_touch_editing`]).
impl TextInput {
    /// Closes the edit bar, if touch editing has one open.
    fn close_edit_bar(&mut self) {
        if let Some(touch) = self.touch.as_mut() {
            touch.bar_open = false;
        }
    }

    /// Whether the edit bar is open.
    fn bar_open(&self) -> bool {
        self.touch.as_ref().is_some_and(|touch| touch.bar_open)
    }

    /// Opens or closes the edit bar and draws again.
    fn set_bar_open(&mut self, open: bool, cx: &mut Context<Self>) {
        if let Some(touch) = self.touch.as_mut()
            && touch.bar_open != open
        {
            touch.bar_open = open;
            cx.notify();
        }
    }

    /// Where the selection's ends draw, from the last drawn frame.
    fn touch_spots(&self) -> Option<TouchSpots> {
        let layout = self.last_layout.as_ref()?;
        let field = self.last_bounds?;
        let origin = point(field.left() - self.scroll_x, field.top() - self.scroll_y);
        let at = |offset| origin + layout.position(self.to_shown(offset));
        Some(TouchSpots {
            start: at(self.selected_range.start),
            end: at(self.selected_range.end),
            line_height: layout.line_height,
            field,
        })
    }

    /// Whether a touch at window point `at` lands on the caret: the
    /// selection is empty and `at` lies within reach of it.
    fn on_caret(&self, at: Point<Pixels>) -> bool {
        self.selected_range.is_empty()
            && self
                .touch_spots()
                .is_some_and(|spots| near_caret(spots.start, spots.line_height, at))
    }

    /// The byte offset of the character under window point `position`;
    /// none outside the input or past a row's end.
    fn char_under(&self, position: Point<Pixels>) -> Option<usize> {
        let local = self.last_bounds?.localize(&position)?;
        let layout = self.last_layout.as_ref()?;
        let row = layout.row_at_y(local.y + self.scroll_y);
        let shown = layout.index_in_row(row, local.x + self.scroll_x)?;
        Some(self.clamp_offset(self.offset_of_shown(shown)))
    }

    /// Selects the word ([`word_range`]) under window point `position`;
    /// false, changing nothing, when no word is there.
    fn select_word_at(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) -> bool {
        self.commit_composition(cx);
        let Some(word) = self
            .char_under(position)
            .and_then(|offset| word_range(&self.content, offset))
        else {
            return false;
        };
        self.selected_range = word;
        self.selection_reversed = false;
        self.goal_x = None;
        self.report_to_platform(PlatformReport::Selection);
        self.check_token(cx);
        cx.notify();
        true
    }

    /// Takes a tap (a left press) with touch editing on, before the caret
    /// moves to it: a double tap on a word selects it; a tap on the caret
    /// of the input that already had the keyboard (`focused`) opens the
    /// edit bar and leaves the caret; any other tap closes the bar. True
    /// when the tap is spent and the caret must not move.
    fn touch_tap(&mut self, event: &MouseDownEvent, focused: bool, cx: &mut Context<Self>) -> bool {
        let now = cx.background_executor().now();
        if let Some(touch) = self.touch.as_mut() {
            touch.last_tap = Some((now, event.position));
        }
        if event.click_count == 2 && self.select_word_at(event.position, cx) {
            self.set_bar_open(false, cx);
            return true;
        }
        let on_caret = focused && self.on_caret(event.position);
        self.set_bar_open(on_caret, cx);
        on_caret
    }

    /// A long press at window point `at`: the input takes the keyboard,
    /// the word under it is selected, or the caret placed there when there
    /// is none, and the edit bar opens.
    fn long_press(&mut self, at: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_composition(cx);
        self.focus_handle.focus(window, cx);
        if !self.read_only {
            crate::keyboard::raise(window);
        }
        if !self.select_word_at(at, cx) {
            self.move_to(self.index_for_mouse_position(at), cx);
        }
        if let Some(touch) = self.touch.as_mut() {
            touch.drag = None;
        }
        self.set_bar_open(true, cx);
    }

    /// Whether a touch drag starting at window point `at` takes the caret:
    /// touch editing is on, no drag is held and the touch lands on the
    /// caret.
    fn wants_caret_drag(&self, at: Point<Pixels>) -> bool {
        self.touch
            .as_ref()
            .is_some_and(|touch| touch.drag.is_none())
            && self.on_caret(at)
    }

    /// Commits an IME composition the platform still marks, as
    /// [`EntityInputHandler::unmark_text`] does, and tells the platform the
    /// text anew: a touch edit then acts on the selection the input shows,
    /// never on the marked range.
    fn commit_composition(&mut self, cx: &mut Context<Self>) {
        if self.marked_range.take().is_some() {
            self.report_to_platform(PlatformReport::Content);
            self.check_token(cx);
            cx.notify();
        }
    }

    /// A finger took the caret at window point `at`: a caret drag starts,
    /// and should the finger stay put for the long-press time, it becomes a
    /// long press there ([`Self::caret_held`]), since the drag keeps gpui
    /// from seeing one.
    fn start_caret_drag(&mut self, at: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        self.start_drag(DragKind::Caret, at);
        let delay = GestureTuning::default().long_press_duration;
        cx.spawn_in(window, async move |input, cx| {
            cx.background_executor().timer(delay).await;
            // Fails only when the input or its window is gone.
            cx.update(|window, cx| {
                input
                    .update(cx, |input, cx| input.caret_held(at, window, cx))
                    .ok();
            })
            .ok();
        })
        .detach();
    }

    /// The long-press time passed on a caret drag that started at window
    /// point `at`: while that drag is still held and never strayed past the
    /// slop, it is a long press there ([`Self::long_press`]), which ends it.
    fn caret_held(&mut self, at: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let held = self
            .touch
            .as_ref()
            .and_then(|touch| touch.drag)
            .is_some_and(|drag| drag.kind == DragKind::Caret && !drag.strayed && drag.start == at);
        if held {
            self.long_press(at, window, cx);
        }
    }

    /// Starts `kind`'s drag at window point `at`.
    fn start_drag(&mut self, kind: DragKind, at: Point<Pixels>) {
        if let Some(touch) = self.touch.as_mut() {
            touch.drag = Some(TouchDrag {
                kind,
                start: at,
                strayed: false,
            });
        }
    }

    /// A finger took the `side` handle at window point `at`: that end
    /// becomes the moving one (the caret end) and the edit bar hides until
    /// the drag ends.
    fn start_handle_drag(&mut self, side: HandleSide, at: Point<Pixels>, cx: &mut Context<Self>) {
        self.commit_composition(cx);
        let Some(spots) = self.touch_spots() else {
            return;
        };
        let caret = spots.of(side);
        let stem_middle = point(
            caret.x + px(HANDLE_STEM / 2.0),
            caret.y + spots.line_height / 2.0,
        );
        let fixed = match side {
            HandleSide::Start => self.selected_range.end,
            HandleSide::End => self.selected_range.start,
        };
        self.selection_reversed = side == HandleSide::Start;
        let aim = stem_middle - at;
        self.start_drag(DragKind::Handle { fixed, aim }, at);
        self.close_edit_bar();
        cx.notify();
    }

    /// The finger of the held drag moved to window point `at`: once past
    /// the tap slop, the caret or the dragged end follows it, through the
    /// handle rule ([`drag_handle`]); the input scrolls to keep it in view.
    fn touch_drag_move(&mut self, at: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(drag) = self.touch.as_mut().and_then(|touch| touch.drag.as_mut()) else {
            return;
        };
        if !drag.strayed {
            let (dx, dy) = (
                (at.x - drag.start.x) / px(1.0),
                (at.y - drag.start.y) / px(1.0),
            );
            if dx.hypot(dy) <= GestureTuning::default().touch_slop / px(1.0) {
                return;
            }
            drag.strayed = true;
        }
        let kind = drag.kind;
        match kind {
            DragKind::Caret => self.move_to(self.index_at(at), cx),
            DragKind::Handle { fixed, aim } => {
                let to = self.index_at(at + aim);
                let dragged = drag_handle(&self.content, fixed, to, !self.selection_reversed);
                self.selected_range = dragged.range;
                self.selection_reversed = dragged.reversed;
                self.goal_x = None;
                self.report_to_platform(PlatformReport::Selection);
                self.check_token(cx);
                cx.notify();
            }
        }
    }

    /// The held drag ended (`ended`) or was cancelled at window point `at`:
    /// a caret touch that ended without straying is a tap on the caret,
    /// which opens the edit bar, or with the tap before it a double tap,
    /// which selects the word under it; both ask for the soft keyboard, as
    /// a tap does. A handle drag opens the bar again.
    fn touch_drag_end(
        &mut self,
        at: Point<Pixels>,
        ended: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let now = cx.background_executor().now();
        let Some(touch) = self.touch.as_mut() else {
            return;
        };
        let Some(drag) = touch.drag.take() else {
            return;
        };
        let tuning = GestureTuning::default();
        let double = touch.last_tap.is_some_and(|(time, place)| {
            let (dx, dy) = ((at.x - place.x) / px(1.0), (at.y - place.y) / px(1.0));
            now.saturating_duration_since(time) <= tuning.multi_tap_interval
                && dx.hypot(dy) <= tuning.multi_tap_slop / px(1.0)
        });
        if !ended {
            return;
        }
        match drag.kind {
            DragKind::Caret if !drag.strayed => {
                touch.last_tap = Some((now, at));
                if !self.read_only {
                    crate::keyboard::raise(window);
                }
                let word = double && self.select_word_at(at, cx);
                self.set_bar_open(!word, cx);
            }
            DragKind::Caret => {}
            DragKind::Handle { .. } => self.set_bar_open(true, cx),
        }
    }

    /// Runs the edit bar's `item` through the input's own Cut, Copy, Paste
    /// and Select All; every item but Select All closes the bar, which
    /// stays open without its Select All.
    fn bar_action(&mut self, item: BarItem, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_composition(cx);
        match item {
            BarItem::Cut => self.cut(&Cut, window, cx),
            BarItem::Copy => self.copy(&Copy, window, cx),
            BarItem::Paste => self.paste_clipboard(window, cx),
            BarItem::SelectAll => self.select_all(&SelectAll, window, cx),
        }
        if item != BarItem::SelectAll {
            self.set_bar_open(false, cx);
        }
        cx.notify();
    }

    /// Whether window point `at` lies on the edit bar the last render drew.
    fn in_bar(&self, at: Point<Pixels>) -> bool {
        self.touch
            .as_ref()
            .and_then(|touch| touch.bar)
            .is_some_and(|bar| bar.contains(&at))
    }

    /// What touch editing draws over the input: the selection's handles
    /// and the edit bar, from the last drawn frame; none while the input
    /// does not have the keyboard, which also closes the bar and drops a
    /// drag.
    fn touch_overlay(&mut self, window: &Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let focused = self.focus_handle.is_focused(window);
        let spots = self.touch_spots();
        let Some(touch) = self.touch.as_mut() else {
            return Vec::new();
        };
        if !focused {
            touch.bar_open = false;
            touch.drag = None;
        }
        touch.drawn = spots;
        touch.bar = None;
        let bar_open = touch.bar_open;
        let Some(spots) = spots.filter(|_| focused) else {
            return Vec::new();
        };
        let mut overlay = Vec::new();
        if !self.selected_range.is_empty() {
            let color = gpui::rgb(theme(cx).accent).into();
            let input = cx.entity().downgrade();
            for side in [HandleSide::Start, HandleSide::End] {
                if spots.shows(spots.of(side)) {
                    overlay.push(handle_element(side, spots, color, input.clone()));
                }
            }
        }
        if bar_open {
            overlay.push(self.edit_bar(spots, window, cx));
        }
        overlay
    }

    /// The edit bar for the selection at `spots`, placed by [`bar_origin`]
    /// in the window, with [`bar_items`]' buttons in the phone's button
    /// style; it records where it draws.
    fn edit_bar(
        &mut self,
        spots: TouchSpots,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = theme(cx);
        let look = button_look(
            crate::fonts::ui_text_size(cx),
            crate::fonts::ui_weight_shift(cx),
            ButtonKind::Outlined,
            ButtonSize::Large,
            true,
            Layer::Floating,
            &t,
        );
        let items = bar_items(self.content.len(), &self.selected_range);
        let widths: Vec<Pixels> = items
            .iter()
            .map(|item| label_width(item.label(), look.text_size, cx) + px(2.0 * look.pad_x + 2.0))
            .collect();
        let inner = widths.iter().fold(px(0.), |sum, width| sum + *width)
            + px(BAR_PAD) * widths.len().saturating_sub(1);
        let bar = size(
            inner + px(2.0 * BAR_PAD + 2.0),
            px(TOUCH_TARGET + 2.0 * BAR_PAD + 2.0),
        );
        let origin = bar_origin(spots.selection_box(), bar, window.viewport_size());
        if let Some(touch) = self.touch.as_mut() {
            touch.bar = Some(Bounds::new(origin, bar));
        }
        let local = origin - spots.field.origin;
        let buttons = items.into_iter().zip(widths).map(|(item, width)| {
            button(item.selector(), look)
                .flex_none()
                .w(width)
                .h(px(TOUCH_TARGET))
                .child(item.label())
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    cx.stop_propagation();
                    this.bar_action(item, window, cx);
                }))
        });
        deferred(
            div()
                .id("text-edit-bar")
                .driver_id(|| "text-edit-bar".to_owned())
                .occlude()
                .absolute()
                .left(local.x)
                .top(local.y)
                .w(bar.width)
                .h(bar.height)
                .flex()
                .items_center()
                .gap(px(BAR_PAD))
                .p(px(BAR_PAD))
                .rounded(px(BAR_RADIUS))
                .bg(rgb(t.popover))
                .border_1()
                .border_color(rgb(t.popover_edge))
                .children(buttons),
        )
        .into_any_element()
    }
}

/// The width of `label` at `text_size` in the UI font; a face without one
/// of its glyphs counts a full em for each character.
#[expect(
    clippy::cast_precision_loss,
    reason = "a bar label is a few characters, far inside f32's exact integer range"
)]
fn label_width(label: &str, text_size: Pixels, cx: &App) -> Pixels {
    let text = cx.text_system();
    let id = text.resolve_font(&gpui::font(crate::fonts::ui_family(cx)));
    let measured: anyhow::Result<f32> = label
        .chars()
        .map(|ch| {
            text.advance(id, text_size, ch)
                .map(|advance| advance.width / px(1.0))
        })
        .sum();
    let width = measured.unwrap_or(label.chars().count() as f32 * (text_size / px(1.0)));
    // Room for the label's weight shift.
    px(width.ceil() + 4.0)
}

/// The `side` handle of the selection at `spots`, as the phone chat's
/// handles draw: a stem the caret's height and a round knob above the start
/// or below the end, in `color`, inside a touch-target square centred on
/// the knob that takes a finger's drag for `input`. Placed in the input's
/// own box and drawn over everything else.
fn handle_element(
    side: HandleSide,
    spots: TouchSpots,
    color: gpui::Hsla,
    input: WeakEntity<TextInput>,
) -> AnyElement {
    let caret = spots.of(side);
    let knob_x = caret.x + px(HANDLE_STEM / 2.0);
    let knob = match side {
        HandleSide::Start => point(knob_x, caret.y - px(HANDLE_KNOB / 2.0)),
        HandleSide::End => point(knob_x, caret.y + spots.line_height + px(HANDLE_KNOB / 2.0)),
    };
    let half = px(TOUCH_TARGET / 2.0);
    let hit = point(knob.x - half, knob.y - half);
    let local = hit - spots.field.origin;
    deferred(
        div()
            .id(side.key())
            .driver_id(move || side.key().to_owned())
            .occlude()
            .absolute()
            .left(local.x)
            .top(local.y)
            .size(px(TOUCH_TARGET))
            .child(
                div()
                    .absolute()
                    .left(caret.x - hit.x)
                    .top(caret.y - hit.y)
                    .w(px(HANDLE_STEM))
                    .h(spots.line_height)
                    .bg(color),
            )
            .child(
                div()
                    .absolute()
                    .left(knob.x - hit.x - px(HANDLE_KNOB / 2.0))
                    .top(knob.y - hit.y - px(HANDLE_KNOB / 2.0))
                    .size(px(HANDLE_KNOB))
                    .rounded_full()
                    .bg(color),
            )
            .child(handle_area(side, input)),
    )
    .into_any_element()
}

/// An area over a handle's whole square that claims a touch drag starting
/// on it for the `side` handle of `input`, so the touch neither pans nor
/// long-presses (as `phone/long_press.rs`'s `touch_drag_area` claims one).
fn handle_area(side: HandleSide, input: WeakEntity<TextInput>) -> impl IntoElement {
    canvas(
        |bounds: Bounds<Pixels>, window: &mut Window, _: &mut App| {
            window.insert_hitbox(bounds, HitboxBehavior::Normal)
        },
        move |_, hitbox: Hitbox, window: &mut Window, _: &mut App| {
            window.on_mouse_event(move |event: &TouchDragEvent, phase, window, cx| {
                let claims = event.phase == TouchPhase::Started
                    && phase.bubble()
                    && !window.default_prevented()
                    && hitbox.is_hovered(window);
                if claims {
                    window.prevent_default();
                    cx.stop_propagation();
                    let at = event.start_position;
                    // Fails only when the input is gone.
                    input
                        .update(cx, |input, cx| input.start_handle_drag(side, at, cx))
                        .ok();
                }
            });
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

/// Registers a touch-editing input's own touch listeners for the frame,
/// `hitbox` its box: a long press on it, a touch drag that starts on its
/// caret and the moves and end of a drag it holds, and while its edit bar
/// is open, a scroll or a press outside the input and the bar, which close
/// the bar.
fn touch_listeners(
    input: &Entity<TextInput>,
    hitbox: &Hitbox,
    bar_open: bool,
    window: &mut Window,
) {
    let (target, area) = (input.clone(), hitbox.clone());
    window.on_mouse_event(move |event: &LongPressEvent, phase, window, cx| {
        let claims = event.phase == TouchPhase::Started
            && phase.bubble()
            && !window.default_prevented()
            && area.is_hovered(window);
        if claims {
            window.prevent_default();
            cx.stop_propagation();
            let at = event.start_position;
            target.update(cx, |input, cx| input.long_press(at, window, cx));
        }
    });
    let (target, area) = (input.clone(), hitbox.clone());
    window.on_mouse_event(move |event: &TouchDragEvent, phase, window, cx| {
        if !phase.bubble() {
            return;
        }
        match event.phase {
            TouchPhase::Started => {
                let at = event.start_position;
                let claims = !window.default_prevented()
                    && area.is_hovered(window)
                    && target.read(cx).focus_handle.is_focused(window)
                    && target.read(cx).wants_caret_drag(at);
                if claims {
                    window.prevent_default();
                    cx.stop_propagation();
                    target.update(cx, |input, cx| input.start_caret_drag(at, window, cx));
                }
            }
            TouchPhase::Moved => {
                target.update(cx, |input, cx| input.touch_drag_move(event.position, cx));
            }
            TouchPhase::Ended | TouchPhase::Cancelled => {
                let ended = event.phase == TouchPhase::Ended;
                target.update(cx, |input, cx| {
                    input.touch_drag_end(event.position, ended, window, cx);
                });
            }
        }
    });
    if !bar_open {
        return;
    }
    let target = input.clone();
    window.on_mouse_event(move |_: &ScrollEvent, phase, _, cx| {
        if phase == DispatchPhase::Capture {
            target.update(cx, |input, cx| input.set_bar_open(false, cx));
        }
    });
    let (target, area) = (input.clone(), hitbox.clone());
    window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
        let outside = !area.is_hovered(window) && !target.read(cx).in_bar(event.position);
        if phase == DispatchPhase::Capture && outside {
            target.update(cx, |input, cx| input.set_bar_open(false, cx));
        }
    });
}

/// Where the IME's marked range lands once `replaced` holds `new_len` bytes in
/// its place.
///
/// Args:
///     `marked`: The marked range before the replacement.
///     `replaced`: The byte range being replaced.
///     `new_len`: The replacement's length in bytes.
///
/// Returns:
///     The marked range shifted by the change in length when it lies wholly
///     after `replaced`, unchanged when it lies wholly before it, and None
///     when the two overlap.
fn marked_after_replace(
    marked: &Range<usize>,
    replaced: &Range<usize>,
    new_len: usize,
) -> Option<Range<usize>> {
    if marked.start >= replaced.end {
        let start = replaced.start + new_len + (marked.start - replaced.end);
        let end = replaced.start + new_len + (marked.end - replaced.end);
        Some(start..end)
    } else if marked.end <= replaced.start {
        Some(marked.clone())
    } else {
        None
    }
}

/// What a masked input draws for `c`: a line break stays, anything else is
/// [`MASK`].
fn masked_char(c: char) -> char {
    if c == '\n' { '\n' } else { MASK }
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

/// The scroll that keeps a single-line input's caret, `caret_x` from the
/// text's left, in a view `width` wide over text `text_width` wide, moving
/// as little as it can from `scroll_x`. Text no wider than the view never
/// scrolls.
fn caret_scroll_x(scroll_x: Pixels, caret_x: Pixels, width: Pixels, text_width: Pixels) -> Pixels {
    let mut scroll = scroll_x;
    if caret_x < scroll {
        scroll = caret_x;
    } else if caret_x > scroll + width {
        scroll = caret_x - width;
    }
    let max_scroll = text_width - width;
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
    fn selection_quads(
        &self,
        range: &Range<usize>,
        origin: Point<Pixels>,
        t: &UiTheme,
    ) -> Vec<PaintQuad> {
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
                Some(fill(corners, rgba(t.input_selection)))
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
    fn accepts_text_input(&self, _: &mut Window, _: &mut Context<Self>) -> bool {
        !self.read_only
    }

    // The iOS text-input bridge treats a handler with no document length as ineligible for the keyboard.
    fn text_length_utf16(&mut self, _: &mut Window, _: &mut Context<Self>) -> Option<usize> {
        Some(self.content.encode_utf16().count())
    }

    fn text_input_configuration(
        &mut self,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> TextInputConfiguration {
        self.input_config.clone()
    }

    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        let text = &self.content[range];
        // A masked input's text never reaches the platform's input handler
        // (prediction, a touch keyboard's learning): it reads the dots, one
        // per UTF-16 unit so the string is as long as the range it serves.
        Some(if self.is_masked() {
            text.chars()
                .flat_map(|c| std::iter::repeat_n(masked_char(c), c.len_utf16()))
                .collect()
        } else {
            text.to_owned()
        })
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

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.marked_range = None;
        self.check_token(cx);
        self.log_platform_call("unmark_text", None, None);
    }

    // A ranged replacement leaves a caret after the range where it was, shifted by the change in length.
    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.replace_from_platform(range_utf16.as_ref(), new_text, cx);
        self.log_platform_call(
            "replace_text_in_range",
            range_utf16.as_ref(),
            Some(new_text),
        );
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.read_only {
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
            self.close_edit_bar();
            cx.emit(TextChanged);
            self.check_token(cx);
            cx.notify();
        }
        self.log_platform_call(
            "replace_and_mark_text_in_range",
            range_utf16.as_ref(),
            Some(new_text),
        );
    }

    // UIKit sets the selection before a soft-keyboard backspace and before
    // smart punctuation's delete; the edit that follows replaces it.
    fn set_selected_text_range(
        &mut self,
        range_utf16: Range<usize>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected_range = self.range_from_utf16(&range_utf16);
        self.selection_reversed = false;
        self.goal_x = None;
        self.check_token(cx);
        cx.notify();
        self.log_platform_call("set_selected_text_range", Some(&range_utf16), None);
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
        let origin = point(bounds.left() - self.scroll_x, bounds.top() - self.scroll_y);
        let start = layout.position(self.to_shown(range.start));
        let end = layout.position(self.to_shown(range.end));
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
        let utf8_index = self.offset_of_shown(layout.index_in_row(row, local.x + self.scroll_x)?);
        Some(offset_to_utf16(&self.content, utf8_index))
    }
}

impl Render for TextInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The app's own edits and caret moves, told to the platform's keyboard while this input has it.
        let report = std::mem::take(&mut self.platform_report);
        if self.focus_handle.is_focused(window) {
            match report {
                PlatformReport::Content => window.notify_text_input_changed(&self.focus_handle),
                PlatformReport::Selection => {
                    window.notify_text_selection_changed(&self.focus_handle);
                }
                PlatformReport::None => {}
            }
        }
        let overlay = self.touch_overlay(window, cx);
        let touch = self.touch.is_some();
        div()
            .when(touch, |input| {
                input.relative().driver_id(|| "text-edit-field".to_owned())
            })
            .flex()
            .w_full()
            .key_context(self.key_context())
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
            .on_action(cx.listener(Self::completion_up))
            .on_action(cx.listener(Self::completion_down))
            .on_action(cx.listener(Self::completion_accept))
            .on_action(cx.listener(Self::completion_close))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .child(TextElement { input: cx.entity() })
            .children(overlay)
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
    /// The input's box as a hitbox, for touch editing's gestures; none
    /// while touch editing is off.
    hitbox: Option<Hitbox>,
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

/// The font a field draws in: the inherited family and size at Regular, since
/// what the user types stays outside the UI weight.
fn input_font(style: &gpui::TextStyle) -> gpui::Font {
    gpui::Font {
        weight: gpui::FontWeight::NORMAL,
        ..style.font()
    }
}

impl Shaping {
    fn of(input: &TextInput, window: &Window, t: &UiTheme) -> Self {
        let style = window.text_style();
        let (text, color) = if input.content.is_empty() {
            (input.placeholder.clone(), rgb(t.input_placeholder))
        } else {
            (input.shown(), rgb(t.input_text))
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
            font: input_font(&style),
            color: color.into(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let marked = input
            .marked_range
            .as_ref()
            .map(|range| input.range_to_shown(range));
        let runs = text_runs(run, text.len(), marked.as_ref());
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
        let shaping = Shaping::of(input, window, &theme(cx));
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
        let t = theme(cx);
        let input = self.input.read(cx);
        let multi_line = input.mode.is_multi_line();
        let line_height = window.line_height();
        let wrap_width = multi_line.then_some(bounds.size.width);
        let Some(lines) = Shaping::of(input, window, &t).shape(window, wrap_width) else {
            return PrepaintState {
                layout: None,
                origin: bounds.origin,
                cursor: None,
                selection: Vec::new(),
                hitbox: None,
            };
        };
        let hitbox = input
            .touch
            .is_some()
            .then(|| window.insert_hitbox(bounds, HitboxBehavior::Normal));
        let layout = TextLayout::new(lines, line_height);
        let caret = layout.position(input.to_shown(input.cursor_offset()));
        let painted = input.painted_selection(input.focus_handle.is_focused(window));
        let caret_width = px(2.);
        let (scroll_x, scroll_y) = if multi_line {
            let view_height = bounds.size.height;
            let scroll_y = scroll_to_caret(
                input.scroll_y,
                caret.y,
                line_height,
                view_height,
                layout.height(),
            );
            (px(0.), scroll_y)
        } else {
            // The view leaves the caret's own width at its right, so a caret
            // at the text's end still draws inside the box.
            let text_width = layout
                .lines
                .first()
                .map_or(px(0.), |line| line.unwrapped_layout.width);
            let view_width = bounds.size.width - caret_width;
            let scroll_x = caret_scroll_x(input.scroll_x, caret.x, view_width, text_width);
            (scroll_x, px(0.))
        };
        let origin = point(bounds.left() - scroll_x, bounds.top() - scroll_y);
        let (selection, cursor) = if let Some(painted) = painted {
            let shown = input.range_to_shown(&painted);
            (layout.selection_quads(&shown, origin, &t), None)
        } else {
            let caret = Bounds::new(origin + caret, size(caret_width, line_height));
            (Vec::new(), Some(fill(caret, rgb(t.input_cursor))))
        };
        self.input.update(cx, |input, _| {
            input.scroll_x = scroll_x;
            input.scroll_y = scroll_y;
        });
        PrepaintState {
            layout: Some(layout),
            origin,
            cursor,
            selection,
            hitbox,
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
        let mask = Some(ContentMask { bounds });
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        if let Some(hitbox) = prepaint.hitbox.take() {
            touch_listeners(&self.input, &hitbox, input.bar_open(), window);
        }
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
        let moved = self.input.update(cx, |input, _| {
            input.last_layout = Some(layout);
            input.last_bounds = Some(bounds);
            input
                .touch
                .as_ref()
                .is_some_and(|touch| touch.drawn != input.touch_spots())
        });
        if moved {
            // The handles and the bar drew from the frame before: draw again
            // once this frame is done, as a refresh while drawing is ignored.
            window.defer(cx, |window, _| window.refresh());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::ops::Range;
    use std::rc::Rc;

    use gpui::{
        AppContext as _, Autocapitalize, ClipboardItem, Entity, EntityInputHandler as _,
        IntoElement, MouseButton, MouseDownEvent, ParentElement, Pixels, Render, Styled,
        TestAppContext, TextInputAction, TextInputConfiguration, TextInputStateChange,
        VisualTestContext, Window, div, point, px,
    };

    use super::{
        CompletionEvent, PlatformReport, TextChanged, TextInput, TextInputEvent, Token, bind_keys,
        caret_scroll_x, clipboard_image_entry, marked_after_replace, next_boundary,
        offset_from_utf16, offset_to_utf16, previous_boundary, single_line,
    };

    #[test]
    fn clipboard_image_entry_prefers_a_non_bmp_image_and_skips_empty_ones() {
        use gpui::{ClipboardEntry, Image, ImageFormat};

        let of = |item: &ClipboardItem| {
            clipboard_image_entry(item).map(|image| (image.format, image.bytes.clone()))
        };
        assert_eq!(of(&ClipboardItem::new_string("hi".to_owned())), None);
        let mixed = ClipboardItem {
            entries: vec![
                ClipboardEntry::Image(Image::from_bytes(ImageFormat::Png, Vec::new())),
                ClipboardEntry::Image(Image::from_bytes(ImageFormat::Bmp, vec![b'B'])),
                ClipboardEntry::Image(Image::from_bytes(ImageFormat::Png, vec![1])),
            ],
        };
        assert_eq!(of(&mixed), Some((ImageFormat::Png, vec![1])));
        let bmp = ClipboardItem::new_image(&Image::from_bytes(ImageFormat::Bmp, vec![b'B']));
        assert_eq!(of(&bmp), Some((ImageFormat::Bmp, vec![b'B'])));
    }

    #[test]
    fn text_narrower_than_the_box_never_scrolls() {
        assert_eq!(caret_scroll_x(px(20.), px(50.), px(100.), px(80.)), px(0.));
    }

    #[test]
    fn a_caret_past_the_right_edge_scrolls_to_sit_at_it() {
        assert_eq!(
            caret_scroll_x(px(0.), px(150.), px(100.), px(200.)),
            px(50.)
        );
    }

    #[test]
    fn a_caret_left_of_the_scroll_scrolls_to_the_caret() {
        assert_eq!(
            caret_scroll_x(px(80.), px(30.), px(100.), px(200.)),
            px(30.)
        );
    }

    #[test]
    fn a_caret_inside_the_view_keeps_the_scroll() {
        assert_eq!(
            caret_scroll_x(px(40.), px(90.), px(100.), px(200.)),
            px(40.)
        );
    }

    #[test]
    fn text_shrinking_under_the_scroll_clamps_it_down() {
        assert_eq!(
            caret_scroll_x(px(80.), px(120.), px(100.), px(150.)),
            px(50.)
        );
    }

    #[test]
    fn a_caret_at_the_start_scrolls_to_the_start() {
        assert_eq!(caret_scroll_x(px(60.), px(0.), px(100.), px(300.)), px(0.));
    }

    #[test]
    fn a_text_input_ignores_the_ui_weight() {
        let style = gpui::TextStyle {
            font_weight: gpui::FontWeight::SEMIBOLD,
            ..gpui::TextStyle::default()
        };
        assert_eq!(super::input_font(&style).weight, gpui::FontWeight::NORMAL);
    }

    #[test]
    fn a_marked_range_after_a_replacement_shifts_with_it() {
        assert_eq!(
            marked_after_replace(&(2..5), &(0..1), 2),
            Some(3..6),
            "growing before the composition moves it right"
        );
        assert_eq!(
            marked_after_replace(&(5..9), &(0..3), 1),
            Some(3..7),
            "shrinking before it moves it left"
        );
    }

    #[test]
    fn a_marked_range_before_a_replacement_stays() {
        assert_eq!(marked_after_replace(&(2..5), &(6..8), 1), Some(2..5));
    }

    #[test]
    fn a_marked_range_overlapping_a_replacement_is_dropped() {
        assert_eq!(marked_after_replace(&(2..5), &(4..6), 1), None);
        assert_eq!(
            marked_after_replace(&(2..5), &(2..5), 1),
            None,
            "the marked text itself is what an unmarked edit replaces"
        );
    }

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
                input.focus_handle.focus(window, cx);
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

    /// Records the [`CompletionEvent`]s the input emits.
    fn completion_events(
        input: &Entity<TextInput>,
        cx: &mut VisualTestContext,
    ) -> Rc<std::cell::RefCell<Vec<CompletionEvent>>> {
        let seen = Rc::new(std::cell::RefCell::new(Vec::new()));
        let sink = Rc::clone(&seen);
        cx.update(|_, cx| {
            cx.subscribe(input, move |_, event: &CompletionEvent, _| {
                sink.borrow_mut().push(event.clone());
            })
            .detach();
        });
        seen
    }

    fn with_triggers(cx: &mut gpui::Context<TextInput>) -> TextInput {
        TextInput::multi_line("", "", cx).with_completion_triggers(&['/', '@'])
    }

    fn set_open(input: &Entity<TextInput>, open: bool, cx: &mut VisualTestContext) {
        cx.update(|_, cx| input.update(cx, |input, cx| input.set_completion_open(open, cx)));
        cx.run_until_parked();
    }

    #[gpui::test]
    fn the_hook_is_off_unless_asked(cx: &mut TestAppContext) {
        let (input, cx) = open(cx, multi, "");
        let seen = completion_events(&input, cx);
        let keys = events(&input, cx);
        cx.simulate_input("/co");
        set_open(&input, true, cx);
        cx.simulate_keystrokes("enter escape");
        assert!(
            seen.borrow().is_empty(),
            "no completion event without triggers"
        );
        assert_eq!(text_of(&input, cx), "/co\n", "Enter still breaks the line");
        assert_eq!(
            *keys.borrow(),
            [TextInputEvent::Cancel],
            "Esc still cancels"
        );
    }

    #[gpui::test]
    fn keys_go_to_the_popup_only_while_it_is_open(cx: &mut TestAppContext) {
        let (input, cx) = open(cx, with_triggers, "");
        let seen = completion_events(&input, cx);
        let keys = events(&input, cx);
        cx.simulate_input("/c");
        let token = Token {
            trigger: '/',
            query: "c".to_owned(),
            range: 0..2,
        };
        assert_eq!(
            seen.borrow().last(),
            Some(&CompletionEvent::TokenChanged(token))
        );
        seen.borrow_mut().clear();

        set_open(&input, true, cx);
        cx.simulate_keystrokes("up down tab enter escape");
        assert_eq!(
            *seen.borrow(),
            [
                CompletionEvent::Up,
                CompletionEvent::Down,
                CompletionEvent::Accept,
                CompletionEvent::Accept,
                CompletionEvent::Close,
            ]
        );
        assert_eq!(text_of(&input, cx), "/c", "the popup's keys edit nothing");
        assert!(keys.borrow().is_empty(), "Esc is the popup's, not a cancel");
        cx.simulate_keystrokes("ctrl-enter");
        assert_eq!(
            *keys.borrow(),
            [TextInputEvent::Submit],
            "Ctrl+Enter still submits"
        );

        seen.borrow_mut().clear();
        set_open(&input, false, cx);
        cx.simulate_keystrokes("escape enter");
        assert_eq!(
            *keys.borrow(),
            [TextInputEvent::Submit, TextInputEvent::Cancel]
        );
        assert_eq!(text_of(&input, cx), "/c\n", "closed, Enter breaks the line");
        assert_eq!(
            *seen.borrow(),
            [CompletionEvent::TokenCleared],
            "the line break left the token"
        );
    }

    #[gpui::test]
    fn accept_completion_is_ignored_during_ime_composition(cx: &mut TestAppContext) {
        let (input, cx) = open(cx, with_triggers, "@s");
        cx.update(|window, cx| {
            input.update(cx, |input, cx| {
                input.replace_and_mark_text_in_range(None, "\u{ff4b}", None, window, cx);
            });
        });
        let marked = |input: &Entity<TextInput>, cx: &mut VisualTestContext| {
            cx.update(|_, cx| input.read(cx).marked_range.clone())
        };
        assert_eq!(marked(&input, cx), Some(2..5), "the composition is marked");
        cx.update(|_, cx| {
            input.update(cx, |input, cx| input.accept_completion(&(0..2), "@src", cx));
        });
        cx.run_until_parked();
        assert_eq!(text_of(&input, cx), "@s\u{ff4b}", "the pick is ignored");
        assert_eq!(marked(&input, cx), Some(2..5), "the composition is kept");
    }

    #[gpui::test]
    fn no_token_event_fires_during_ime_composition(cx: &mut TestAppContext) {
        let (input, cx) = open(cx, with_triggers, "");
        let seen = completion_events(&input, cx);
        cx.update(|window, cx| {
            input.update(cx, |input, cx| {
                input.replace_and_mark_text_in_range(None, "@s", None, window, cx);
            });
        });
        cx.run_until_parked();
        assert!(seen.borrow().is_empty(), "nothing while the text is marked");
        cx.update(|window, cx| {
            input.update(cx, |input, cx| {
                input.replace_text_in_range(None, "@sr", window, cx);
            });
        });
        cx.run_until_parked();
        assert_eq!(
            *seen.borrow(),
            [CompletionEvent::TokenChanged(Token {
                trigger: '@',
                query: "sr".to_owned(),
                range: 0..3,
            })],
            "the committed text is a token"
        );
    }

    #[gpui::test]
    fn text_input_multi_line_paste_keeps_and_normalises_newlines(cx: &mut TestAppContext) {
        let (input, cx) = open(cx, multi, "");
        cx.write_to_clipboard(ClipboardItem::new_string("a\r\nb\rc\nd".to_owned()));
        cx.simulate_keystrokes("secondary-v");
        assert_eq!(text_of(&input, cx), "a\nb\nc\nd");
    }

    #[gpui::test]
    fn text_input_single_line_paste_flattens_newlines(cx: &mut TestAppContext) {
        let (input, cx) = open(cx, single, "");
        cx.write_to_clipboard(ClipboardItem::new_string("a\r\nb\rc\nd".to_owned()));
        cx.simulate_keystrokes("secondary-v");
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
    fn a_lone_newline_from_the_platform_submits_a_single_line_input(cx: &mut TestAppContext) {
        let (input, cx) = open(cx, single, "ab");
        let seen = events(&input, cx);
        cx.simulate_input("\n");
        assert_eq!(text_of(&input, cx), "ab", "Return inserts nothing");
        assert_eq!(*seen.borrow(), [TextInputEvent::Submit]);
    }

    #[gpui::test]
    fn a_lone_newline_from_the_platform_breaks_a_multi_line_input(cx: &mut TestAppContext) {
        let (input, cx) = open(cx, multi, "ab");
        let seen = events(&input, cx);
        cx.simulate_input("\n");
        assert_eq!(text_of(&input, cx), "ab\n");
        assert!(seen.borrow().is_empty(), "a line break is not a submit");
    }

    #[gpui::test]
    fn a_platform_insert_holding_newlines_stays_text(cx: &mut TestAppContext) {
        let (input, cx) = open(cx, single, "");
        let seen = events(&input, cx);
        cx.update(|window, cx| {
            input.update(cx, |input, cx| {
                input.replace_text_in_range(None, "a\nb", window, cx);
            });
        });
        assert_eq!(text_of(&input, cx), "a\nb");
        assert!(seen.borrow().is_empty(), "only a lone newline is Return");
    }

    #[gpui::test]
    fn a_pasted_newline_in_a_single_line_input_stays_text(cx: &mut TestAppContext) {
        let (input, cx) = open(cx, single, "");
        let seen = events(&input, cx);
        cx.write_to_clipboard(ClipboardItem::new_string("\n".to_owned()));
        cx.simulate_keystrokes("secondary-v");
        assert_eq!(text_of(&input, cx), " ");
        assert!(seen.borrow().is_empty(), "a paste is not Return");
    }

    #[gpui::test]
    fn a_newline_during_a_composition_commits_it_and_submits(cx: &mut TestAppContext) {
        let (input, cx) = open(cx, single, "");
        let seen = events(&input, cx);
        cx.update(|window, cx| {
            input.update(cx, |input, cx| {
                input.replace_and_mark_text_in_range(None, "wor", None, window, cx);
            });
        });
        cx.simulate_input("\n");
        assert_eq!(
            text_of(&input, cx),
            "wor",
            "the composed text is kept as it stands"
        );
        let marked = cx.update(|_, cx| input.read(cx).marked_range.clone());
        assert_eq!(marked, None);
        assert_eq!(*seen.borrow(), [TextInputEvent::Submit]);
    }

    #[gpui::test]
    fn an_edit_bar_cut_during_a_composition_cuts_the_selected_word(cx: &mut TestAppContext) {
        let (input, cx) = open(cx, multi, "hello ");
        cx.update(|window, cx| {
            input.update(cx, |input, cx| {
                input.replace_and_mark_text_in_range(None, "wor", None, window, cx);
                input.selected_range = 0..5;
                input.bar_action(super::BarItem::Cut, window, cx);
            });
        });
        assert_eq!(text_of(&input, cx), " wor", "the selected word goes");
        let marked = cx.update(|_, cx| input.read(cx).marked_range.clone());
        assert_eq!(marked, None, "the composition is committed");
        assert_eq!(
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .as_deref(),
            Some("hello")
        );
    }

    #[gpui::test]
    fn a_ranged_newline_replacement_stays_text(cx: &mut TestAppContext) {
        let (input, cx) = open(cx, single, "ab");
        let seen = events(&input, cx);
        cx.update(|window, cx| {
            input.update(cx, |input, cx| {
                input.replace_text_in_range(Some(0..1), "\n", window, cx);
            });
        });
        assert_eq!(text_of(&input, cx), "\nb");
        assert!(
            seen.borrow().is_empty(),
            "a ranged replacement is not Return"
        );
    }

    #[gpui::test]
    fn a_composition_survives_a_replacement_before_it(cx: &mut TestAppContext) {
        let (input, cx) = open(cx, single, "ab");
        cx.update(|window, cx| {
            input.update(cx, |input, cx| {
                input.replace_and_mark_text_in_range(None, "か", None, window, cx);
            });
        });
        let marked = cx
            .update(|window, cx| input.update(cx, |input, cx| input.marked_text_range(window, cx)));
        assert_eq!(marked, Some(2..3), "the composition is marked at the end");
        cx.update(|window, cx| {
            input.update(cx, |input, cx| {
                input.replace_text_in_range(Some(0..1), "XY", window, cx);
            });
        });
        assert_eq!(text_of(&input, cx), "XYbか");
        let marked = cx
            .update(|window, cx| input.update(cx, |input, cx| input.marked_text_range(window, cx)));
        assert_eq!(
            marked,
            Some(3..4),
            "the composition shifts by the replacement's growth"
        );
    }

    #[gpui::test]
    fn text_length_counts_utf16_units(cx: &mut TestAppContext) {
        let (input, cx) = open(cx, single, "a😀");
        let length = cx
            .update(|window, cx| input.update(cx, |input, cx| input.text_length_utf16(window, cx)));
        assert_eq!(length, Some(3), "the emoji is two UTF-16 units");
    }

    #[gpui::test]
    fn a_read_only_input_accepts_no_text_input(cx: &mut TestAppContext) {
        let (input, cx) = open(cx, single, "ab");
        let accepts = cx.update(|window, cx| {
            input.update(cx, |input, cx| input.accepts_text_input(window, cx))
        });
        assert!(accepts, "an editable input takes text");
        cx.update(|_, cx| input.update(cx, |input, cx| input.set_read_only(true, cx)));
        let accepts = cx.update(|window, cx| {
            input.update(cx, |input, cx| input.accepts_text_input(window, cx))
        });
        assert!(!accepts, "a read-only input takes none");
    }

    #[gpui::test]
    fn the_keyboard_configuration_is_what_was_set(cx: &mut TestAppContext) {
        let (input, cx) = open(cx, single, "");
        let default = cx.update(|window, cx| {
            input.update(cx, |input, cx| input.text_input_configuration(window, cx))
        });
        assert_eq!(default, TextInputConfiguration::default());
        let wanted = TextInputConfiguration {
            autocorrect: true,
            autocapitalize: Autocapitalize::Sentences,
            input_action: TextInputAction::Send,
            ..TextInputConfiguration::default()
        };
        cx.update(|_, cx| {
            input.update(cx, |input, cx| {
                input.set_text_input_configuration(wanted.clone(), cx);
            });
        });
        let set = cx.update(|window, cx| {
            input.update(cx, |input, cx| input.text_input_configuration(window, cx))
        });
        assert_eq!(set, wanted);
    }

    #[gpui::test]
    fn paste_clipboard_inserts_the_clipboard_text(cx: &mut TestAppContext) {
        let (input, cx) = open(cx, multi, "");
        cx.update(|_, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string("pasted text".to_owned()));
        });
        cx.update(|window, cx| {
            input.update(cx, |input, cx| input.paste_clipboard(window, cx));
        });
        assert_eq!(
            cx.update(|_, cx| input.read(cx).text().to_owned()),
            "pasted text"
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
        cx.simulate_keystrokes("secondary-v backspace enter");
        assert_eq!(text_of(&input, cx), "seed");
        cx.simulate_keystrokes("secondary-a secondary-c");
        assert_eq!(caret(&input, cx), (0, 4), "select-all still selects");
        let copied = cx.read_from_clipboard().and_then(|item| item.text());
        assert_eq!(copied.as_deref(), Some("seed"));
    }

    #[gpui::test]
    fn masked_input_draws_dots_and_blocks_copy(cx: &mut TestAppContext) {
        let (input, cx) = open(cx, single, "sk-é1");
        cx.update(|_, cx| input.update(cx, |input, cx| input.set_masked(true, cx)));
        cx.run_until_parked();
        let (shown, drawn_end) = cx.update(|_, cx| {
            let input = input.read(cx);
            let end = input.last_layout.as_ref().map(|layout| layout.rows[0].end);
            (input.shown().to_string(), end)
        });
        assert_eq!(shown, "•••••", "one dot a character, é included");
        assert_eq!(drawn_end, Some("•••••".len()), "the dots are what is drawn");
        let offsets = cx.update(|_, cx| {
            let input = input.read(cx);
            (
                input.to_shown(5),
                input.offset_of_shown(12),
                input.offset_of_shown(13),
            )
        });
        assert_eq!(offsets, (12, 5, 6), "offsets follow the hidden text");

        cx.write_to_clipboard(ClipboardItem::new_string("zz".to_owned()));
        cx.simulate_keystrokes("secondary-a secondary-c");
        let copied = cx.read_from_clipboard().and_then(|item| item.text());
        assert_eq!(copied.as_deref(), Some("zz"), "copy leaves the clipboard");
        cx.simulate_keystrokes("secondary-x");
        let cut = cx.read_from_clipboard().and_then(|item| item.text());
        assert_eq!(cut.as_deref(), Some("zz"), "cut leaves the clipboard");
        assert_eq!(text_of(&input, cx), "sk-é1", "and the text");
        cx.simulate_keystrokes("end left");
        assert_eq!(caret(&input, cx), (5, 5), "the caret steps over é's bytes");
        cx.simulate_keystrokes("secondary-v");
        assert_eq!(text_of(&input, cx), "sk-ézz1", "paste still inserts");

        cx.update(|_, cx| input.update(cx, |input, cx| input.set_masked(false, cx)));
        cx.run_until_parked();
        let shown = cx.update(|_, cx| input.read(cx).shown().to_string());
        assert_eq!(shown, "sk-ézz1", "unmasked, the text shows");
        cx.simulate_keystrokes("secondary-a secondary-c");
        let copied = cx.read_from_clipboard().and_then(|item| item.text());
        assert_eq!(copied.as_deref(), Some("sk-ézz1"), "and copies");
    }

    #[gpui::test]
    fn masked_input_hides_text_from_the_input_handler(cx: &mut TestAppContext) {
        let (input, cx) = open(cx, single, "sk-é1");
        let handler_text = |input: &Entity<TextInput>, cx: &mut VisualTestContext| {
            cx.update(|window, cx| {
                input.update(cx, |input, cx| {
                    let whole = 0..offset_to_utf16(input.text(), input.text().len());
                    let mut actual = None;
                    let text = input.text_for_range(whole, &mut actual, window, cx);
                    (text, actual)
                })
            })
        };
        cx.update(|_, cx| input.update(cx, |input, cx| input.set_masked(true, cx)));
        let (text, actual) = handler_text(&input, cx);
        assert_eq!(text.as_deref(), Some("•••••"), "one dot a character");
        assert_eq!(actual, Some(0..5), "the range is the hidden text's");
        cx.simulate_input("x");
        cx.write_to_clipboard(ClipboardItem::new_string("yz".to_owned()));
        cx.simulate_keystrokes("secondary-v backspace");
        assert_eq!(
            text_of(&input, cx),
            "sk-é1xy",
            "typing, paste and backspace work"
        );
        let (text, _) = handler_text(&input, cx);
        assert_eq!(text.as_deref(), Some("•••••••"));
        cx.update(|_, cx| input.update(cx, |input, cx| input.set_masked(false, cx)));
        let (text, _) = handler_text(&input, cx);
        assert_eq!(text.as_deref(), Some("sk-é1xy"), "unmasked, the text");
    }

    #[gpui::test]
    fn masked_text_for_range_keeps_the_utf16_length(cx: &mut TestAppContext) {
        let (input, cx) = open(cx, single, "a😀b");
        let handler_text =
            |input: &Entity<TextInput>, cx: &mut VisualTestContext, range: Range<usize>| {
                cx.update(|window, cx| {
                    input.update(cx, |input, cx| {
                        let mut actual = None;
                        let text = input.text_for_range(range, &mut actual, window, cx);
                        (text, actual)
                    })
                })
            };
        cx.update(|_, cx| input.update(cx, |input, cx| input.set_masked(true, cx)));
        let whole = 0..4;
        let (text, actual) = handler_text(&input, cx, whole.clone());
        assert_eq!(
            text.as_deref(),
            Some("••••"),
            "an emoji is two units, so two dots"
        );
        assert_eq!(
            text.map(|text| text.encode_utf16().count()),
            Some(whole.len()),
            "the string is as long as the range the platform asked for"
        );
        assert_eq!(actual, Some(whole));
        let (text, _) = handler_text(&input, cx, 0..3);
        assert_eq!(
            text.as_deref(),
            Some("•••"),
            "up to the caret after the emoji"
        );
    }

    /// A window root holding one input, so the input keeps its own measured height (gpui stretches an auto-sized window root to the window).
    struct Holder(Entity<TextInput>);

    impl Render for Holder {
        fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
            div().flex().flex_col().size_full().child(self.0.clone())
        }
    }

    #[gpui::test]
    fn text_input_multi_line_height_clamps_between_min_and_max_rows(cx: &mut TestAppContext) {
        /// The drawn height in whole rows, within the device pixel the layout rounds it up to, and the input's vertical scroll.
        fn rows(
            cx: &mut TestAppContext,
            build: fn(&mut gpui::Context<TextInput>) -> TextInput,
            text: &str,
            scale: f32,
        ) -> Option<(usize, Pixels)> {
            cx.update(bind_keys);
            let (holder, cx) = cx.add_window_view(|_, cx| Holder(cx.new(build)));
            let input = cx.update(|_, cx| holder.read(cx).0.clone());
            cx.update(|window, cx| {
                input.update(cx, |input, cx| {
                    input.set_text(text.to_owned(), cx);
                    input.focus_handle.focus(window, cx);
                });
            });
            cx.run_until_parked();
            cx.simulate_scale_factor_change(scale);
            cx.run_until_parked();
            let line_height = cx.update(|window, _| window.line_height());
            let (height, scroll_y) = cx.update(|_, cx| {
                let input = input.read(cx);
                input
                    .last_bounds
                    .map(|bounds| (bounds.size.height, input.scroll_y))
            })?;
            let drawn = (0..20).find(|rows| {
                line_height * *rows <= height && height - line_height * *rows < px(1.0 / scale)
            })?;
            Some((drawn, scroll_y))
        }
        for scale in [1.0_f32, 1.5, 2.0] {
            let one = rows(cx, multi, "one", scale);
            assert_eq!(one.map(|r| r.0), Some(2), "min_rows at {scale}");
            let three = rows(cx, multi, "1\n2\n3", scale);
            assert_eq!(three.map(|r| r.0), Some(3), "three lines at {scale}");
            assert_eq!(
                rows(cx, multi, "1\n2\n3\n4\n5\n6\n7\n8", scale).map(|r| r.0),
                Some(6),
                "max_rows at {scale}"
            );
            assert_eq!(
                rows(
                    cx,
                    |cx| TextInput::multi_line("", "", cx).with_rows(1, 3),
                    "a\nb\nc\nd",
                    scale
                )
                .map(|r| r.0),
                Some(3),
                "with_rows(1, 3) at {scale}"
            );
            let single_one = rows(cx, single, "one", scale);
            assert_eq!(
                single_one.map(|r| r.0),
                Some(1),
                "single-line is one row at {scale}"
            );
            assert_eq!(
                one.map(|r| r.1),
                Some(px(0.)),
                "a fitting text is not scrolled at {scale}"
            );
            assert_eq!(
                three.map(|r| r.1),
                Some(px(0.)),
                "a fitting text is not scrolled at {scale}"
            );
            assert_eq!(
                single_one.map(|r| r.1),
                Some(px(0.)),
                "a fitting text is not scrolled at {scale}"
            );
        }
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

    /// Sets the selection as the platform does (`UIKit`'s `setSelectedTextRange:`).
    fn platform_select(
        input: &Entity<TextInput>,
        range_utf16: Range<usize>,
        cx: &mut VisualTestContext,
    ) {
        cx.update(|window, cx| {
            input.update(cx, |input, cx| {
                input.set_selected_text_range(range_utf16, window, cx);
            });
        });
    }

    /// Replaces text as the platform does (`UIKit`'s `insertText:` with no
    /// range, `replaceRange:withText:` with one).
    fn platform_replace(
        input: &Entity<TextInput>,
        range_utf16: Option<Range<usize>>,
        text: &str,
        cx: &mut VisualTestContext,
    ) {
        cx.update(|window, cx| {
            input.update(cx, |input, cx| {
                input.replace_text_in_range(range_utf16, text, window, cx);
            });
        });
    }

    /// The selection as the platform reads it, in UTF-16.
    fn platform_selection(
        input: &Entity<TextInput>,
        cx: &mut VisualTestContext,
    ) -> Option<Range<usize>> {
        cx.update(|window, cx| {
            input.update(cx, |input, cx| {
                input
                    .selected_text_range(false, window, cx)
                    .map(|selection| selection.range)
            })
        })
    }

    #[gpui::test]
    fn the_platform_selection_is_what_its_next_edit_replaces(cx: &mut TestAppContext) {
        let (input, cx) = open(cx, single, "Word ");
        platform_select(&input, 3..5, cx);
        platform_replace(&input, None, ".", cx);
        assert_eq!(text_of(&input, cx), "Wor.");
        assert_eq!(platform_selection(&input, cx), Some(4..4));
    }

    #[gpui::test]
    fn a_platform_delete_after_selecting_one_character_removes_it(cx: &mut TestAppContext) {
        let (input, cx) = open(cx, single, "Word ");
        platform_select(&input, 4..5, cx);
        platform_replace(&input, None, "", cx);
        assert_eq!(text_of(&input, cx), "Word");
        assert_eq!(platform_selection(&input, cx), Some(4..4));
    }

    #[gpui::test]
    fn a_ranged_replace_before_the_caret_keeps_the_caret_after_it(cx: &mut TestAppContext) {
        let (input, cx) = open(cx, single, "teh x");
        platform_replace(&input, Some(0..3), "the", cx);
        assert_eq!(text_of(&input, cx), "the x");
        assert_eq!(caret(&input, cx), (5, 5));

        cx.update(|_, cx| input.update(cx, |input, cx| input.set_text("teh", cx)));
        platform_replace(&input, Some(0..3), "The", cx);
        assert_eq!(text_of(&input, cx), "The");
        assert_eq!(caret(&input, cx), (3, 3));

        cx.update(|_, cx| input.update(cx, |input, cx| input.set_text("teh x", cx)));
        platform_replace(&input, Some(0..3), "these", cx);
        assert_eq!(text_of(&input, cx), "these x");
        assert_eq!(caret(&input, cx), (7, 7));
    }

    #[gpui::test]
    fn a_ranged_replace_around_the_caret_puts_it_after_the_replacement(cx: &mut TestAppContext) {
        let (input, cx) = open(cx, single, "abcd");
        cx.update(|_, cx| input.update(cx, |input, _| input.selected_range = 2..2));
        platform_replace(&input, Some(1..3), "XY", cx);
        assert_eq!(text_of(&input, cx), "aXYd");
        assert_eq!(caret(&input, cx), (3, 3));
    }

    /// A focused input reports an app's text change to the window as exactly
    /// one `ContentChanged`, and a blurred input's draw reports nothing. The
    /// mark the draw takes is read inside the update, since the update's end
    /// draws.
    #[gpui::test]
    fn an_app_set_text_reports_the_change_to_the_platform(cx: &mut TestAppContext) {
        let (input, cx) = open(cx, single, "draft");
        cx.run_until_parked();
        let before = platform_changes(cx);
        assert_eq!(
            report_after(&input, cx, |input, _, cx| input.set_text("", cx)),
            PlatformReport::Content,
            "set_text marks the text stale"
        );
        cx.run_until_parked();
        let after = platform_changes(cx);
        assert_eq!(
            &after[before.len()..],
            &[TextInputStateChange::ContentChanged],
            "the focused input's draw reports the text change to the window"
        );
        assert_eq!(
            report_of(&input, cx),
            PlatformReport::None,
            "the draw that reported it cleared the mark"
        );

        cx.update(Window::blur);
        cx.run_until_parked();
        let after_blur = platform_changes(cx);
        assert_eq!(
            report_after(&input, cx, |input, _, cx| input.set_text("sent", cx)),
            PlatformReport::Content
        );
        cx.run_until_parked();
        let after = platform_changes(cx);
        assert_eq!(
            &after[after_blur.len()..],
            &[],
            "an unfocused input's draw reports nothing to the platform"
        );
        assert_eq!(
            report_of(&input, cx),
            PlatformReport::None,
            "an unfocused input's draw clears the mark too"
        );
    }

    /// A caret move on a focused input reports exactly one `SelectionChanged`
    /// to the window, and nothing else.
    #[gpui::test]
    fn a_caret_move_reports_the_selection_to_the_platform(cx: &mut TestAppContext) {
        let (input, cx) = open(cx, single, "abc");
        cx.run_until_parked();
        let before = platform_changes(cx);
        cx.update(|_, cx| input.update(cx, |input, cx| input.move_to(0, cx)));
        cx.run_until_parked();
        let after = platform_changes(cx);
        assert_eq!(
            &after[before.len()..],
            &[TextInputStateChange::SelectionChanged],
            "the focused input's draw reports the caret move to the window"
        );
        assert_eq!(caret(&input, cx), (0, 0));
    }

    /// Every text input state change the input's window has reported, in
    /// order. The read itself draws, so a pending mark lands in the record
    /// before the caller's baseline is taken.
    fn platform_changes(cx: &mut VisualTestContext) -> Vec<TextInputStateChange> {
        let handle = cx.update(|window, _| window.window_handle());
        cx.text_input_state_changes(handle)
    }

    /// The report the input holds for its next draw.
    fn report_of(input: &Entity<TextInput>, cx: &mut VisualTestContext) -> PlatformReport {
        cx.update(|_, cx| input.read(cx).platform_report)
    }

    /// Runs `change` on the input and reads the report it leaves, in one
    /// update: the update's end draws, which takes the report.
    fn report_after(
        input: &Entity<TextInput>,
        cx: &mut VisualTestContext,
        change: impl FnOnce(&mut TextInput, &mut Window, &mut gpui::Context<TextInput>),
    ) -> PlatformReport {
        cx.update(|window, cx| {
            input.update(cx, |input, cx| {
                change(input, window, cx);
                input.platform_report
            })
        })
    }

    #[gpui::test]
    fn a_ranged_replace_after_the_caret_leaves_the_caret(cx: &mut TestAppContext) {
        let (input, cx) = open(cx, single, "abc def");
        cx.update(|_, cx| input.update(cx, |input, _| input.selected_range = 1..1));
        platform_replace(&input, Some(4..7), "DEF", cx);
        assert_eq!(text_of(&input, cx), "abc DEF");
        assert_eq!(caret(&input, cx), (1, 1));
    }

    #[gpui::test]
    fn an_app_paste_marks_the_platform_stale(cx: &mut TestAppContext) {
        let (input, cx) = open(cx, single, "a");
        cx.write_to_clipboard(ClipboardItem::new_string("b".to_owned()));
        let report = report_after(&input, cx, TextInput::paste_clipboard);
        assert_eq!(text_of(&input, cx), "ab");
        assert_eq!(report, PlatformReport::Content);
    }

    #[gpui::test]
    fn a_click_marks_the_platform_selection_stale(cx: &mut TestAppContext) {
        let (input, cx) = open(cx, single, "abc");
        let click = MouseDownEvent {
            button: MouseButton::Left,
            position: point(px(0.), px(0.)),
            ..MouseDownEvent::default()
        };
        let report = report_after(&input, cx, |input, window, cx| {
            input.on_mouse_down(&click, window, cx);
        });
        assert_eq!(report, PlatformReport::Selection);
    }

    #[gpui::test]
    fn a_platform_edit_does_not_mark_the_platform_stale(cx: &mut TestAppContext) {
        let (input, cx) = open(cx, single, "abc");
        let report = report_after(&input, cx, |input, window, cx| {
            input.replace_text_in_range(None, "x", window, cx);
            input.set_selected_text_range(0..0, window, cx);
        });
        assert_eq!(text_of(&input, cx), "abcx");
        assert_eq!(report, PlatformReport::None);
    }

    /// A new input seeded with `SEED`, all of it selected, never focused.
    fn seeded(cx: &mut TestAppContext) -> (Entity<TextInput>, &mut VisualTestContext) {
        cx.add_window_view(|_, cx| TextInput::new(SEED, "", cx))
    }

    const SEED: &str = "C:/old";

    #[gpui::test]
    fn an_unfocused_input_paints_no_selection(cx: &mut TestAppContext) {
        let (input, cx) = seeded(cx);
        assert_eq!(
            cx.update(|_, cx| input.read(cx).painted_selection(false)),
            None
        );
    }

    #[gpui::test]
    fn a_focused_input_paints_its_selection(cx: &mut TestAppContext) {
        let (input, cx) = seeded(cx);
        assert_eq!(
            cx.update(|_, cx| input.read(cx).painted_selection(true)),
            Some(0..SEED.len())
        );
    }
}
