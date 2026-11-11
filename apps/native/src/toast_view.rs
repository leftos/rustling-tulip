//! What an app's own toast draws: a rounded card with a stripe by kind, its
//! title and body, the prompt's command and answers, `Go to pane` and ×, and
//! the view that holds one card in a toast window of its own. The rules are
//! in `docs/native-client.md` "The app's own toasts".

use std::rc::Rc;

use chrono::{DateTime, Utc};
use gpui::{
    AnyElement, App, ClickEvent, Context, Div, Entity, Focusable as _, IntoElement, MouseButton,
    MouseDownEvent, MouseMoveEvent, Render, Subscription, WeakEntity, Window, div, prelude::*, px,
};
use protocol::PromptChoice;

use crate::buttons::{ButtonSize, Layer, field_frame, outlined_button, primary_button};
use crate::dashboard_rows::{AskBlock, AskBox, AskChoice, GO_TO_PANE};
use crate::driver_id::DriverId as _;
use crate::fonts::{self, DEFAULT_FAMILY, ui_px};
use crate::text_input::{TextInput, TextInputEvent};
use crate::toast_stack::{AppToast, AppToastKind, SessionKey, WIDTH};
use crate::ui_theme::theme;
use crate::{RootView, UI_TEXT_SIZE};

/// The card's 1 px border, above and below, in logical px.
const CARD_FRAME: f32 = 2.0;

/// The padding round the card's words, in logical px: each side's.
const WORDS_PAD: f32 = 10.0;

/// The gap under the title, and between the head and the rows, in logical px.
const HEAD_GAP: f32 = 4.0;

/// The gap between two rows (the command, each answer, `Go to pane`), in
/// logical px.
const ROW_GAP: f32 = 6.0;

/// The least lines the body is given room for: the card keeps the size it
/// had when its body was cut at two lines, so a short toast does not shrink
/// to a sliver beside the others in the stack.
const BODY_LEAST_LINES: usize = 2;

/// A compact button's least height, in logical px.
const BUTTON_LEAST: f32 = 28.0;

/// A compact button's padding and border, above and below together, in
/// logical px.
const BUTTON_FRAME: f32 = 10.0;

/// A compact button's padding and border, left and right together, in
/// logical px.
const BUTTON_SIDES: f32 = 26.0;

/// A compact button's label size, as the buttons draw it.
const BUTTON_TEXT: f32 = 12.0;

/// The command box's padding and border, above and below together, in
/// logical px.
const COMMAND_FRAME: f32 = 8.0;

/// The command box's sides, in logical px: its 8 px padding and 1 px
/// border each side, which the command wraps inside.
const COMMAND_SIDES: f32 = 18.0;

/// The room a label leaves for its key hint, in logical px: the gap and a
/// digit at the hint's size, with some to spare.
const HINT_ROOM: f32 = 16.0;

/// How wide the card's accent stripe is, in logical px.
const STRIPE_WIDTH: f32 = 3.0;

/// How wide the card's words are, in logical px: the toast less its border,
/// its stripe and the words' padding.
const WORDS_WIDTH: f32 = WIDTH - CARD_FRAME - STRIPE_WIDTH - 2.0 * WORDS_PAD;

/// The command box's text size, as the Dashboard's ask block draws it.
const SMALL_TEXT: f32 = 11.5;

/// The key hint's text size, as the Dashboard's ask block draws it.
const CAPS_TEXT: f32 = 10.5;

/// An answer's description's text size: the UI text size less 1.5 px, as
/// the key hint's is.
const DESCRIPTION_TEXT: f32 = UI_TEXT_SIZE - 1.5;

/// What an answered toast draws in its choices' place.
const SENT: &str = "Sent";

/// A text the card draws, which sets the size it is drawn at and the width
/// it wraps at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextRole {
    /// The title and the body (the question), at the UI text size across
    /// the card's words.
    Body,
    /// A permission's command, wrapped in the mono box.
    Command,
    /// An answer's label, at a compact button's text size across the
    /// button, less its key hint's room when `hinted`; `bold` for the first,
    /// primary, answer.
    Answer {
        /// Whether it is drawn bold.
        bold: bool,
        /// Whether a key hint sits beside it.
        hinted: bool,
    },
    /// An answer's description, under its label at the description's
    /// smaller size, across the label's width.
    Description {
        /// Whether a key hint sits beside the label.
        hinted: bool,
    },
}

/// The slack, in logical px, between the height a card was placed at and
/// its window's: a physical pixel's rounding, which must not cut its body.
const PLACED_SLACK: f32 = 1.0;

/// How the card's texts measure, so its height fits what it draws.
pub trait TextMeasure {
    /// How many lines `text` wraps to, drawn as `role`.
    fn lines(&self, text: &str, role: TextRole) -> usize;
    /// How tall one line of `role` is, in logical px.
    fn line_height(&self, role: TextRole) -> f32;
    /// The UI text size offset the card is drawn at, which sizes a
    /// button's least height as the buttons draw it.
    fn offset(&self) -> i8;
}

/// The card's texts measured by GPUI's line wrapper, at the font, size and
/// width the card draws them at.
pub struct CardText<'a> {
    cx: &'a App,
}

impl<'a> CardText<'a> {
    /// The measure of the card's texts in `cx`.
    #[must_use]
    pub fn new(cx: &'a App) -> Self {
        Self { cx }
    }

    /// The font, size and wrap width `role` is drawn at.
    fn font(&self, role: TextRole) -> (gpui::Font, gpui::Pixels, f32) {
        let offset = fonts::ui_text_size(self.cx);
        let shift = fonts::ui_weight_shift(self.cx);
        let ui = |weight| gpui::Font {
            weight: fonts::ui_weight_at(shift, weight),
            ..gpui::font(fonts::ui_family(self.cx))
        };
        match role {
            TextRole::Body => (
                ui(gpui::FontWeight::NORMAL),
                fonts::ui_px_at(offset, UI_TEXT_SIZE),
                WORDS_WIDTH,
            ),
            TextRole::Command => (
                gpui::Font {
                    weight: fonts::mono_weight(),
                    ..gpui::font(DEFAULT_FAMILY)
                },
                fonts::ui_px_at(offset, SMALL_TEXT),
                WORDS_WIDTH - COMMAND_SIDES,
            ),
            TextRole::Answer { bold, hinted } => {
                let weight = if bold {
                    gpui::FontWeight::BOLD
                } else {
                    gpui::FontWeight::NORMAL
                };
                let hint = if hinted { HINT_ROOM } else { 0.0 };
                (
                    ui(weight),
                    fonts::ui_px_at(offset, BUTTON_TEXT),
                    WORDS_WIDTH - BUTTON_SIDES - hint,
                )
            }
            TextRole::Description { hinted } => {
                let hint = if hinted { HINT_ROOM } else { 0.0 };
                (
                    ui(gpui::FontWeight::NORMAL),
                    fonts::ui_px_at(offset, DESCRIPTION_TEXT),
                    WORDS_WIDTH - BUTTON_SIDES - hint,
                )
            }
        }
    }
}

impl TextMeasure for CardText<'_> {
    fn lines(&self, text: &str, role: TextRole) -> usize {
        let (font, size, width) = self.font(role);
        let mut wrapper = self.cx.text_system().line_wrapper(font, size);
        // The wrapper skips a line break; the text breaks there first.
        text.split('\n')
            .map(|line| {
                wrapper
                    .wrap_line(&[gpui::LineFragment::text(line)], px(width))
                    .count()
                    + 1
            })
            .sum()
    }

    fn line_height(&self, role: TextRole) -> f32 {
        let (_, size, _) = self.font(role);
        // GPUI's default line height, `phi()` times the text size, rounded
        // as GPUI rounds a text style's line height. The rem size only
        // matters to a rem length, which `phi()` is not.
        let height = gpui::phi().to_pixels(size.into(), size);
        (height / px(1.0)).round()
    }

    fn offset(&self) -> i8 {
        fonts::ui_text_size(self.cx)
    }
}

/// How `card_layout` lays a card out.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CardLayout {
    /// The card's height, in logical px: the height it is placed at.
    pub height: f32,
    /// The lines of the body (the question) the card shows.
    pub body_lines: usize,
    /// Whether the body is cut to `body_lines` for the rows to fit.
    pub clamped: bool,
    /// What the card takes besides the body's lines, in logical px: its
    /// frame, title, gaps and rows.
    pub fixed: f32,
}

/// How tall `toast`'s card is, in logical px, its texts measured by
/// `measure`: the height it is placed at. The card grows to fit what it
/// draws, up to `cap`, the work area's room: a toast taller than the screen
/// cannot be shown, so the card is cut there.
#[must_use]
pub fn card_height(toast: &AppToast, sent: bool, measure: &dyn TextMeasure, cap: f32) -> f32 {
    card_layout(toast, sent, measure, cap).height
}

/// How `toast`'s card lays out, `sent` when an answer is out for its
/// session, its texts measured by `measure`, within `cap`.
#[must_use]
pub fn card_layout(
    toast: &AppToast,
    sent: bool,
    measure: &dyn TextMeasure,
    cap: f32,
) -> CardLayout {
    let line = measure.line_height(TextRole::Body);
    let measured = measure
        .lines(&toast.body, TextRole::Body)
        .max(BODY_LEAST_LINES);
    // A compact button's least height, as the buttons draw it at the UI
    // text offset.
    let least = fonts::ui_px_at(measure.offset(), BUTTON_LEAST) / px(1.0);
    let button = |texts: &[(TextRole, usize)]| {
        let words: f32 = texts
            .iter()
            .map(|(role, lines)| count(*lines) * measure.line_height(*role))
            .sum();
        (BUTTON_FRAME + words).max(least)
    };
    let plain = TextRole::Answer {
        bold: false,
        hinted: false,
    };
    // The rows `answer_block` draws, branch for branch.
    let mut rows: Vec<f32> = Vec::new();
    if let Some(ask) = &toast.ask {
        if let Some(command) = &ask.command {
            rows.push(
                COMMAND_FRAME
                    + count(measure.lines(command, TextRole::Command))
                        * measure.line_height(TextRole::Command),
            );
        }
        if sent && !ask.choices.is_empty() {
            rows.push(button(&[(plain, 1)]));
        } else if ask.since.is_some() {
            for (index, choice) in ask.choices.iter().enumerate() {
                let hinted = choice.hint.is_some();
                let label = TextRole::Answer {
                    bold: index == 0,
                    hinted,
                };
                let mut texts = vec![(label, measure.lines(&choice.label, label))];
                if let Some(description) = described(choice) {
                    let role = TextRole::Description { hinted };
                    texts.push((role, measure.lines(description, role)));
                }
                rows.push(button(&texts));
            }
            if drawn_box(ask).is_some() {
                rows.push(typed_row_height(measure));
            }
        }
    }
    if toast.kind != AppToastKind::Account {
        rows.push(button(&[(plain, 1)]));
    }
    let gaps = count(rows.len().saturating_sub(1)) * ROW_GAP;
    let fixed =
        CARD_FRAME + 2.0 * WORDS_PAD + line + 2.0 * HEAD_GAP + rows.iter().sum::<f32>() + gaps;
    let height = fixed + count(measured) * line;
    if height <= cap {
        return CardLayout {
            height,
            body_lines: measured,
            clamped: false,
            fixed,
        };
    }
    // Too tall for the work area: the body is cut to the lines that fit, so
    // the command, the answers and `Go to pane` stay on the card. A line of
    // it always shows.
    let fit = (1..measured)
        .rev()
        .find(|lines| fixed + count(*lines) * line <= cap)
        .unwrap_or(1);
    CardLayout {
        height: cap,
        body_lines: fit,
        clamped: true,
        fixed,
    }
}

/// The text box row's height, in logical px: a one-line compact button's.
fn typed_row_height(measure: &dyn TextMeasure) -> f32 {
    let least = fonts::ui_px_at(measure.offset(), BUTTON_LEAST) / px(1.0);
    let plain = TextRole::Answer {
        bold: false,
        hinted: false,
    };
    (BUTTON_FRAME + measure.line_height(plain)).max(least)
}

/// The text box `ask` draws under its choices. On macOS none is drawn: a
/// toast there is a panel that never becomes key (RT-1243).
#[cfg(windows)]
fn drawn_box(ask: &AskBlock) -> Option<&AskBox> {
    ask.typed_box()
}

/// The text box `ask` draws under its choices. On macOS none is drawn: a
/// toast there is a panel that never becomes key (RT-1243).
#[cfg(not(windows))]
fn drawn_box(_ask: &AskBlock) -> Option<&AskBox> {
    None
}

/// `choice`'s description, when it has one to draw.
fn described(choice: &AskChoice) -> Option<&str> {
    choice
        .description
        .as_deref()
        .filter(|description| !description.is_empty())
}

/// `n` as a height multiplier.
#[expect(
    clippy::cast_precision_loss,
    reason = "a card's lines and rows are few"
)]
fn count(n: usize) -> f32 {
    n as f32
}

/// An answer button's press: the wait it answers and the choice made.
type AnswerPress = Rc<dyn Fn(DateTime<Utc>, PromptChoice, &mut App)>;

/// A click on a control of the card, or on several of them at once: a Needs
/// you or Finished toast's title and body run the same press as its
/// `Go to pane` button.
type SharedPress = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

/// What the card's controls do.
pub struct ToastPresses {
    /// An answer button's press.
    pub answer: AnswerPress,
    /// `Go to pane`'s press; on an account toast, which has no pane to go
    /// to, a press on the card's body.
    pub go: SharedPress,
    /// ×'s press.
    pub dismiss: SharedPress,
    /// The text box under the answers, when the toast holds one.
    pub typed: Option<TypedRow>,
}

/// A mouse-down on the card's text box.
type BoxPress = Rc<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>;

/// The card's text box: the input it draws and what its controls do.
pub struct TypedRow {
    /// The single-line input the box draws.
    pub input: Entity<TextInput>,
    /// The send button's press.
    pub send: SharedPress,
    /// A mouse-down in the box, which gives the toast the keys.
    pub take_keys: BoxPress,
}

/// The answer button at `index`, primary when it is the first.
fn answer_button(
    index: usize,
    choice: &AskChoice,
    since: DateTime<Utc>,
    answer: &AnswerPress,
    cx: &App,
) -> AnyElement {
    let t = theme(cx);
    let offset = fonts::ui_text_size(cx);
    let shift = fonts::ui_weight_shift(cx);
    let make = if index == 0 {
        primary_button
    } else {
        outlined_button
    };
    let answer = Rc::clone(answer);
    let picked = choice.choice.clone();
    // The description draws under the label, inside the button: a tooltip
    // is drawn inside the toast's window, which is only as tall as its
    // card, so a long one would be cut.
    let description = described(choice).map(|description| {
        div()
            .driver_id(|| format!("toast-answer-{index}-description"))
            .text_size(fonts::ui_px_at(offset, DESCRIPTION_TEXT))
            .font_weight(fonts::ui_weight_at(shift, gpui::FontWeight::NORMAL))
            // On the filled primary button the muted colour has no contrast
            // with the fill, so it keeps the button's own text colour.
            .when(index != 0, |text| text.text_color(gpui::rgb(t.muted)))
            .child(description.to_owned())
    });
    make(
        offset,
        shift,
        &format!("toast-answer-{index}"),
        ButtonSize::Compact,
        true,
        Layer::Base,
        &t,
    )
    .w_full()
    .gap(px(6.0))
    // The label and its description wrap and show whole; the card's height
    // measures them.
    .child(
        div()
            .flex()
            .flex_col()
            .min_w_0()
            .child(div().child(choice.label.clone()))
            .children(description),
    )
    .when_some(choice.hint.clone(), |button, hint| {
        button.child(
            div()
                .flex_none()
                .opacity(0.65)
                .text_size(fonts::ui_px_at(offset, CAPS_TEXT))
                .child(hint),
        )
    })
    .on_click(move |_, _, cx| answer(since, picked.clone(), cx))
    .into_any_element()
}

/// The text box `typed` under the answers, drawn with `row`'s input and
/// presses: the box, and its send button beside it.
fn typed_row(typed: &AskBox, row: &TypedRow, cx: &App) -> AnyElement {
    let t = theme(cx);
    let offset = fonts::ui_text_size(cx);
    let shift = fonts::ui_weight_shift(cx);
    let height = typed_row_height(&CardText::new(cx));
    let take = Rc::clone(&row.take_keys);
    let field = field_frame(false, Layer::Base, &t)
        .driver_id(|| "toast-typed-box".to_owned())
        .flex_1()
        .min_w(px(0.0))
        .h_full()
        .text_size(fonts::ui_px_at(offset, BUTTON_TEXT))
        .on_mouse_down(MouseButton::Left, move |event, window, cx| {
            take(event, window, cx);
        })
        .child(row.input.clone());
    let send = Rc::clone(&row.send);
    let button = outlined_button(
        offset,
        shift,
        "toast-typed-send",
        ButtonSize::Compact,
        true,
        Layer::Base,
        &t,
    )
    .flex_none()
    .child(typed.send_label.clone())
    .on_click(move |event, window, cx| (*send)(event, window, cx));
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .h(px(height))
        .child(field)
        .child(button)
        .into_any_element()
}

/// `ask`'s command box, its choices one to a row and its text box under
/// them, or, once an answer is `sent`, a disabled `Sent` in their place.
fn answer_block(
    ask: &AskBlock,
    sent: bool,
    answer: &AnswerPress,
    typed: Option<&TypedRow>,
    cx: &App,
) -> Div {
    let t = theme(cx);
    let offset = fonts::ui_text_size(cx);
    let shift = fonts::ui_weight_shift(cx);
    let mut rows: Vec<AnyElement> = Vec::new();
    if sent && !ask.choices.is_empty() {
        rows.push(
            outlined_button(
                offset,
                shift,
                "toast-sent",
                ButtonSize::Compact,
                false,
                Layer::Base,
                &t,
            )
            .child(SENT)
            .into_any_element(),
        );
    } else if let Some(since) = ask.since {
        for (index, choice) in ask.choices.iter().enumerate() {
            rows.push(answer_button(index, choice, since, answer, cx));
        }
        if let (Some(typed), Some(row)) = (drawn_box(ask), typed) {
            rows.push(typed_row(typed, row, cx));
        }
    }
    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .when_some(ask.command.clone(), |block, command| {
            block.child(
                div()
                    .flex_none()
                    .max_w_full()
                    .px(px(8.0))
                    .py(px(3.0))
                    .rounded(px(4.0))
                    .border_1()
                    .border_color(gpui::rgb(t.line))
                    .bg(gpui::rgb(t.surface))
                    .font_family(DEFAULT_FAMILY)
                    .font_weight(fonts::mono_weight())
                    .text_size(fonts::ui_px_at(offset, SMALL_TEXT))
                    .child(gpui::StyledText::new(command))
                    .driver_id(|| "toast-command".to_owned()),
            )
        })
        .children(rows)
}

/// The card's `Go to pane` button, running `press`.
fn go_to_pane_button(press: SharedPress, cx: &App) -> AnyElement {
    let t = theme(cx);
    outlined_button(
        fonts::ui_text_size(cx),
        fonts::ui_weight_shift(cx),
        "toast-go-to-pane",
        ButtonSize::Compact,
        true,
        Layer::Base,
        &t,
    )
    .text_color(gpui::rgb(t.text_2))
    .child(GO_TO_PANE)
    .on_click(move |event, window, cx| (*press)(event, window, cx))
    .into_any_element()
}

/// The card's title and body, taking `press` when a click on them goes to the
/// toast's pane; the body cut to `body_lines` lines, ending in `…`, when
/// given.
fn head_block(
    toast: &AppToast,
    body_lines: Option<usize>,
    press: Option<SharedPress>,
    cx: &App,
) -> AnyElement {
    let t = theme(cx);
    let head = div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        // The title as an invisible copy that sizes its box and a drawn copy
        // laid over it. A `truncating` title collapsed to a bare `…`: gpui
        // keeps one text layout per element, and taffy's layout cache can
        // replay the title's full-width layout after a narrower measure
        // (from a sibling's intrinsic pass) replaced it. The absolutely
        // placed copy is measured once, at the box's final width. The
        // margin, not a padding, keeps it clear of ×: an absolute child
        // spans its box's padding. The title stays one line: it is context,
        // not the question, and the card's height measures the body.
        .child(
            div()
                .relative()
                .min_w(px(0.0))
                .overflow_hidden()
                .whitespace_nowrap()
                .mr(px(16.0))
                .text_color(gpui::rgb(t.text))
                .driver_id(|| format!("toast-title-{}", toast.key.1))
                .children(crate::ellipsis::ellipsized(
                    &format!("toast-title-{}", toast.key.1),
                    toast.title.clone().into(),
                    gpui::StyledText::new(toast.title.clone()),
                )),
        )
        // The body (a question) wraps and shows whole; the card's height
        // measures it. On a card cut to the work area it ends at the lines
        // that fit, so the rows under it stay on the card.
        .child(
            div()
                .text_color(gpui::rgb(t.text_2))
                .when_some(body_lines, |body, lines| {
                    body.overflow_hidden().text_ellipsis().line_clamp(lines)
                })
                .child(toast.body.clone()),
        );
    match press {
        Some(press) => head
            .id("toast-body")
            .cursor_pointer()
            .on_click(move |event, window, cx| (*press)(event, window, cx))
            .into_any_element(),
        None => head.into_any_element(),
    }
}

/// `toast`'s card, filling its parent; `sent` when an answer is out for its
/// session; its body cut to `body_lines` lines when given (see
/// [`CardLayout::clamped`]).
pub fn toast_card(
    toast: &AppToast,
    sent: bool,
    body_lines: Option<usize>,
    presses: ToastPresses,
    cx: &App,
) -> Div {
    let t = theme(cx);
    let accent = match toast.kind {
        AppToastKind::NeedsYou { .. } => t.asking,
        AppToastKind::Finished | AppToastKind::Account => t.muted,
    };
    // An account toast has no pane to go to: its body takes the press. A
    // Needs you or Finished toast goes to its pane from `Go to pane` and
    // from its title and body, never from its answers or its ×.
    let (go_button, head_press, body_press) = match toast.kind {
        AppToastKind::NeedsYou { .. } | AppToastKind::Finished => (
            Some(go_to_pane_button(Rc::clone(&presses.go), cx)),
            Some(Rc::clone(&presses.go)),
            None,
        ),
        AppToastKind::Account => (None, None, Some(Rc::clone(&presses.go))),
    };
    let dismiss = div()
        .id("toast-dismiss")
        .driver_id(|| "toast-dismiss".to_owned())
        .absolute()
        .top(px(6.0))
        .right(px(8.0))
        .cursor_pointer()
        .text_color(gpui::rgb(t.muted))
        .child("×")
        // It sits over the title, whose press goes to the pane: the press
        // ends here, never running both.
        .on_click(move |event, window, cx| {
            (*presses.dismiss)(event, window, cx);
            cx.stop_propagation();
        });
    // An ask with no command and no answer rows (no choices, or no wait
    // stamp to answer against) draws no block, and takes no gap above
    // `Go to pane`, as `card_layout` counts it.
    let answers = toast
        .ask
        .as_ref()
        .filter(|ask| {
            ask.command.is_some() || (!ask.choices.is_empty() && (sent || ask.since.is_some()))
        })
        .map(|ask| answer_block(ask, sent, &presses.answer, presses.typed.as_ref(), cx));
    let words = div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .gap(px(4.0))
        .p(px(10.0))
        .child(head_block(toast, body_lines, head_press, cx))
        .child(
            div()
                .mt_auto()
                .flex()
                .flex_col()
                .gap(px(6.0))
                .children(answers)
                .children(go_button.map(|go| div().flex().child(go))),
        );
    let words = match body_press {
        Some(press) => words
            .id("toast-body")
            .cursor_pointer()
            .on_click(move |event, window, cx| (*press)(event, window, cx))
            .into_any_element(),
        None => words.into_any_element(),
    };
    div()
        .driver_id(|| "toast-card".to_owned())
        .relative()
        .flex()
        .size_full()
        .overflow_hidden()
        .rounded(px(8.0))
        .border_1()
        .border_color(gpui::rgb(t.border))
        .bg(gpui::rgb(t.panel_bg))
        .font_family(fonts::ui_family(cx))
        .font_weight(fonts::ui_weight(cx, gpui::FontWeight::NORMAL))
        .text_size(ui_px(cx, UI_TEXT_SIZE))
        .child(
            div()
                .flex_none()
                .w(px(STRIPE_WIDTH))
                .h_full()
                .bg(gpui::rgb(accent)),
        )
        .child(words)
        .child(dismiss)
}

/// The root view of one toast window.
pub(crate) struct ToastView {
    root: WeakEntity<RootView>,
    toast: AppToast,
    /// Whether an answer is out for the session, as the root view last said.
    sent: bool,
    /// Whether the pointer rests on the toast, as last told to the root view.
    hovering: bool,
    /// Whether the window reported the pointer at its last move over the
    /// toast. Only such a window can report it leaving.
    window_saw_pointer: bool,
    /// The text box's input, made once the toast first draws a box and kept
    /// while it lives, so a refused answer's text is there when the box
    /// comes back.
    typed: Option<Entity<TextInput>>,
    /// The input's Enter and Esc, and the release that gives the keys back
    /// when the toast closes.
    typed_subscriptions: Vec<Subscription>,
    /// While the toast holds the keys, its window's handle and the window
    /// that was foreground before, to give back.
    keys: Option<(usize, Option<usize>)>,
}

/// Whether a redraw ends the rest on a toast: the pointer was `hovering`,
/// the window saw it at its last move (`window_saw_pointer`), and the window
/// no longer has it (`window_hovered`). A window that never reported the
/// pointer has nothing to say about it leaving.
fn releases_hover(hovering: bool, window_saw_pointer: bool, window_hovered: bool) -> bool {
    hovering && window_saw_pointer && !window_hovered
}

impl ToastView {
    /// The view of `toast`, reporting to `root`.
    pub(crate) fn new(root: WeakEntity<RootView>, toast: AppToast) -> Self {
        Self {
            root,
            toast,
            sent: false,
            hovering: false,
            window_saw_pointer: false,
            typed: None,
            typed_subscriptions: Vec::new(),
            keys: None,
        }
    }

    /// The text box's input when the toast draws a box: made on the first
    /// such draw, its placeholder kept to the box's.
    fn ensure_typed(&mut self, cx: &mut Context<Self>) -> Option<Entity<TextInput>> {
        let placeholder = self
            .toast
            .ask
            .as_ref()
            .and_then(drawn_box)
            .map(|typed| typed.placeholder.clone())?;
        if let Some(input) = &self.typed {
            input.update(cx, |input, cx| input.set_placeholder(placeholder, cx));
            return Some(input.clone());
        }
        let input = cx.new(|cx| TextInput::new("", placeholder, cx));
        self.typed_subscriptions.push(cx.subscribe(
            &input,
            |this, _, event: &TextInputEvent, cx| match event {
                TextInputEvent::Submit => this.send_typed(cx),
                TextInputEvent::Cancel => this.give_back_keys(cx),
            },
        ));
        self.typed_subscriptions
            .push(cx.on_release(ToastView::give_back_keys_at_release));
        self.typed = Some(input.clone());
        Some(input)
    }

    /// Sends the text box's text as the box's typed answer, as an answer
    /// button sends its choice; a blank box, or an answer already out,
    /// sends nothing. The send runs once this update ends, as the root view
    /// redraws this toast when it records the answer.
    fn send_typed(&mut self, cx: &mut Context<Self>) {
        if self.sent {
            return;
        }
        let Some(ask) = self.toast.ask.as_ref() else {
            return;
        };
        let (Some(since), Some(typed), Some(input)) = (ask.since, drawn_box(ask), &self.typed)
        else {
            return;
        };
        let Some(choice) = typed.answer.choice(input.read(cx).text()) else {
            return;
        };
        let root = self.root.clone();
        let session_id = self.toast.key.1.clone();
        cx.defer(move |cx| {
            let answered = root.update(cx, |root, cx| {
                root.answer_from_dashboard(&session_id, since, choice, cx);
            });
            if let Err(err) = answered {
                tracing::debug!("toast answered after the root view went: {err:#}");
            }
        });
        self.give_back_keys(cx);
    }

    /// A mouse-down in the text box: the box takes the keyboard, and on
    /// Windows the toast becomes the foreground window so keys reach it.
    /// The foreground change runs on a later turn of the loop, outside the
    /// window's own input handling, which the activation it brings re-enters.
    fn take_keys(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(input) = &self.typed {
            input.read(cx).focus_handle(cx).focus(window);
        }
        #[cfg(windows)]
        {
            if self.keys.is_some() {
                return;
            }
            let key = self.toast.key.clone();
            let Some(address) = self
                .root
                .upgrade()
                .and_then(|root| root.read(cx).toast_window_address(&key))
            else {
                return;
            };
            cx.spawn(async move |this, cx| {
                let (taken, before) = crate::toast_window::take_foreground(address);
                let Some(keys) = crate::toast_window::recorded_keys(address, taken, before) else {
                    return;
                };
                if this.update(cx, |this, _| this.keys = Some(keys)).is_err() {
                    tracing::warn!("the toast closed while it took the foreground");
                    crate::toast_window::give_back_foreground(address, before, false);
                }
            })
            .detach();
        }
    }

    /// Takes the toast's hold of the keys, when it has one: its window's
    /// handle and the window that was foreground before, for the caller to
    /// give back.
    pub(crate) fn release_keys(&mut self) -> Option<(usize, Option<usize>)> {
        self.keys.take()
    }

    /// Gives the keys back, when the toast holds them: its
    /// `WS_EX_NOACTIVATE` back on, and the window that was foreground
    /// before made foreground again while the toast still is (see
    /// [`crate::toast_window::foreground_to_restore`]), on a later turn of
    /// the loop.
    fn give_back_keys(&mut self, cx: &mut App) {
        self.give_back_keys_when(false, cx);
    }

    /// The last resort, as the view is released: gives the keys back as
    /// [`ToastView::give_back_keys`] does, but with the toast's window
    /// perhaps gone, the window that was foreground before is also made
    /// foreground again over a null foreground or one of this process's
    /// windows, never another app's.
    fn give_back_keys_at_release(&mut self, cx: &mut App) {
        self.give_back_keys_when(true, cx);
    }

    /// Gives the keys back, `released` when the view is.
    fn give_back_keys_when(&mut self, released: bool, cx: &mut App) {
        let Some((address, before)) = self.release_keys() else {
            return;
        };
        #[cfg(windows)]
        cx.spawn(async move |_| {
            crate::toast_window::give_back_foreground(address, before, released);
        })
        .detach();
        #[cfg(not(windows))]
        let _ = (address, before, released, cx);
    }

    /// Shows `toast` as the stack now holds it, `sent` when an answer is out
    /// for its session. The root view is not read while drawing: it is
    /// mid-update when a toast window opens. A different prompt empties the
    /// text box; the same one redrawn keeps its text.
    pub(crate) fn show(&mut self, toast: AppToast, sent: bool, cx: &mut Context<Self>) {
        if another_prompt(self.toast.ask.as_ref(), toast.ask.as_ref())
            && let Some(input) = &self.typed
        {
            input.update(cx, |input, cx| input.set_text("", cx));
        }
        self.toast = toast;
        self.sent = sent;
        cx.notify();
    }

    /// The pointer came to rest on the toast or left it: every countdown
    /// holds or runs on.
    fn set_hovering(&mut self, hovering: bool, cx: &mut Context<Self>) {
        if self.hovering == hovering {
            return;
        }
        self.hovering = hovering;
        self.window_saw_pointer = false;
        let key = self.toast.key.clone();
        let told = self
            .root
            .update(cx, |root, cx| root.hover_toast(&key, hovering, cx));
        if let Err(err) = told {
            tracing::debug!("toast hovered after the root view went: {err:#}");
        }
    }

    /// A pointer that leaves the window sends no move event for `on_hover`
    /// to see, only a redraw with the window no longer hovered.
    fn follow_window_hover(&mut self, window: &Window, cx: &mut Context<Self>) {
        let window_hovered = window.is_window_hovered();
        if releases_hover(self.hovering, self.window_saw_pointer, window_hovered) {
            let view = cx.weak_entity();
            cx.defer(move |cx| {
                // Fails only when the toast has closed, and the hold with it.
                view.update(cx, |view, cx| view.set_hovering(false, cx))
                    .ok();
            });
        }
    }
}

/// Whether the toast's ask went from the `drawn` prompt to another one: its
/// label, text, command, choices or box differ. The stamp is not compared,
/// so a new prompt under the same stamp counts.
fn another_prompt(drawn: Option<&AskBlock>, next: Option<&AskBlock>) -> bool {
    let (Some(drawn), Some(next)) = (drawn, next) else {
        return false;
    };
    (
        &drawn.label,
        &drawn.text,
        &drawn.command,
        &drawn.choices,
        &drawn.typed,
    ) != (
        &next.label,
        &next.text,
        &next.command,
        &next.choices,
        &next.typed,
    )
}

/// A press that takes `key`'s toast down through `root`.
fn dismiss_press(root: WeakEntity<RootView>, key: SessionKey) -> SharedPress {
    Rc::new(move |_, _, cx| {
        let dismissed = root.update(cx, |root, cx| root.dismiss_app_toast(&key, cx));
        if let Err(err) = dismissed {
            tracing::debug!("toast dismissed after the root view went: {err:#}");
        }
    })
}

impl Render for ToastView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.follow_window_hover(window, cx);
        let (go_root, dismiss_root) = (self.root.clone(), self.root.clone());
        let (go_key, dismiss_key) = (self.toast.key.clone(), self.toast.key.clone());
        let answer_root = self.root.clone();
        let session_id = self.toast.key.1.clone();
        let typed = self.ensure_typed(cx).map(|input| TypedRow {
            input,
            send: Rc::new(cx.listener(|this, _: &ClickEvent, _, cx| this.send_typed(cx))),
            take_keys: Rc::new(cx.listener(|this, _: &MouseDownEvent, window, cx| {
                this.take_keys(window, cx);
            })),
        });
        let view = cx.weak_entity();
        let presses = ToastPresses {
            typed,
            answer: Rc::new(move |since, choice, cx| {
                // An answer gives the keys back with it, as Send does. Fails
                // only when the toast has closed, and its keys with it.
                view.update(cx, |view, cx| view.give_back_keys(cx)).ok();
                let answered = answer_root.update(cx, |root, cx| {
                    root.answer_from_dashboard(&session_id, since, choice, cx);
                });
                if let Err(err) = answered {
                    tracing::debug!("toast answered after the root view went: {err:#}");
                }
            }),
            go: match self.toast.kind {
                AppToastKind::NeedsYou { .. } | AppToastKind::Finished => {
                    Rc::new(move |_, _, cx| RootView::toast_go_to_pane(&go_root, &go_key, cx))
                }
                AppToastKind::Account => dismiss_press(go_root, go_key),
            },
            dismiss: dismiss_press(dismiss_root, dismiss_key),
        };
        // The window is as tall as the card was placed: when that cut it to
        // the work area, the body is cut to the lines that fit.
        let placed = window.viewport_size().height / px(1.0) + PLACED_SLACK;
        let layout = card_layout(&self.toast, self.sent, &CardText::new(cx), placed);
        let body_lines = layout.clamped.then_some(layout.body_lines);
        let card = toast_card(&self.toast, self.sent, body_lines, presses, cx);
        div()
            .id("toast-window")
            .size_full()
            .on_mouse_move(cx.listener(|this, _: &MouseMoveEvent, window, cx| {
                this.set_hovering(true, cx);
                // Read at the event: the move and the leave can both land
                // between two frames, so no redraw may see the pointer here.
                this.window_saw_pointer = window.is_window_hovered();
            }))
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if !hovered {
                    this.set_hovering(false, cx);
                }
            }))
            .child(card)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use protocol::PromptChoice;

    use super::{TextMeasure, TextRole, card_height, card_layout, count, releases_hover};
    use crate::dashboard_rows::{AskBlock, AskChoice};
    use crate::target::HostKey;
    use crate::toast_stack::{AppToast, AppToastKind, key_of};

    #[test]
    fn hover_release_after_a_pass_between_frames() {
        let cases = [
            (
                (true, true, false),
                true,
                "the pass: seen at the move, gone by the frame",
            ),
            (
                (true, true, true),
                false,
                "the pointer is still on the toast",
            ),
            (
                (true, false, false),
                false,
                "a window that never reported the pointer",
            ),
            ((true, false, true), false, "no move has been read yet"),
            ((false, true, false), false, "nothing rests on the toast"),
            ((false, false, false), false, "nothing rests on the toast"),
        ];
        for ((hovering, saw, hovered), released, why) in cases {
            assert_eq!(releases_hover(hovering, saw, hovered), released, "{why}");
        }
    }

    fn toast() -> AppToast {
        AppToast::new(
            key_of(&HostKey::Local, "s1"),
            AppToastKind::Finished,
            "a title".to_owned(),
            "a body".to_owned(),
            Instant::now(),
        )
    }

    fn account() -> AppToast {
        AppToast::new(
            key_of(&HostKey::Local, "s1"),
            AppToastKind::Account,
            "a title".to_owned(),
            "a body".to_owned(),
            Instant::now(),
        )
    }

    /// A toast waiting on an answer per label, with a command when `command`,
    /// its buttons up (a wait stamp).
    fn asking(labels: &[&str], command: bool) -> AppToast {
        asking_described(labels, command, None)
    }

    /// As [`asking`], each answer described by `description` when given.
    fn asking_described(labels: &[&str], command: bool, description: Option<&str>) -> AppToast {
        let choices = labels
            .iter()
            .zip(0..)
            .map(|(label, index)| AskChoice {
                label: (*label).to_owned(),
                hint: Some("1".to_owned()),
                choice: PromptChoice::Option { index },
                description: description.map(str::to_owned),
            })
            .collect();
        toast().waiting_on(
            Some(AskBlock {
                session_id: "s1".to_owned(),
                label: "Permission ·".to_owned(),
                text: "a prompt".to_owned(),
                command: command.then(|| "gh pr merge".to_owned()),
                since: Some(chrono::Utc::now()),
                choices,
                typed: None,
            }),
            None,
        )
    }

    /// As [`asking`], with no wait stamp: no buttons are drawn.
    fn unstamped(labels: &[&str], command: bool) -> AppToast {
        let mut toast = asking(labels, command);
        if let Some(ask) = toast.ask.as_mut() {
            ask.since = None;
        }
        toast
    }

    /// A measure where a text starting `long` wraps to three lines, one
    /// starting `many` to twenty, and any other fits one, every line `line`
    /// px tall, at UI text size offset `offset`.
    struct Fake {
        line: f32,
        offset: i8,
    }

    /// The measure most cases use: 20 px lines at offset 0.
    const FAKE: Fake = Fake {
        line: 20.0,
        offset: 0,
    };

    impl TextMeasure for Fake {
        fn lines(&self, text: &str, _: TextRole) -> usize {
            if text.starts_with("long") {
                3
            } else if text.starts_with("many") {
                20
            } else {
                1
            }
        }

        fn line_height(&self, _: TextRole) -> f32 {
            self.line
        }

        fn offset(&self) -> i8 {
            self.offset
        }
    }

    #[test]
    fn card_height_follows_what_the_card_draws() {
        // At 20 px a line: the frame and padding 22, the title 20, its gap 4,
        // a body of at least two lines 40, the gap above the rows 4, then
        // each row (a one-line button 30, a three-line one 70, the command
        // 28, a described one-line button 50) with 6 between rows.
        let mut long_body = toast();
        long_body.body = "long body".to_owned();
        let cases = [
            (
                toast(),
                120.0,
                "no ask: the body's two lines and Go to pane",
            ),
            (
                long_body,
                140.0,
                "a body that wraps to three lines shows them all",
            ),
            (asking(&[], false), 120.0, "no choices"),
            (asking(&["a"], false), 156.0, "one choice"),
            (asking(&["a", "b"], false), 192.0, "a row per choice"),
            (asking(&["a", "b", "c"], false), 228.0, "three rows"),
            (
                asking(&["long a", "b"], false),
                232.0,
                "a wrapped answer grows its row",
            ),
            (asking(&["a", "b"], true), 226.0, "choices and a command"),
            (asking(&[], true), 154.0, "a command and no choices"),
            (account(), 90.0, "an account toast draws no Go to pane row"),
            (
                asking_described(&["a"], false, Some("what a does")),
                176.0,
                "a description draws under its label and grows its row",
            ),
            (
                unstamped(&["a", "b", "c"], false),
                120.0,
                "no wait stamp: no buttons, no rows",
            ),
            (
                unstamped(&["a", "b"], true),
                154.0,
                "no wait stamp: the command alone",
            ),
        ];
        for (toast, height, why) in cases {
            let drawn = card_height(&toast, false, &FAKE, 1000.0);
            assert!((drawn - height).abs() < f32::EPSILON, "{why}: {drawn}");
        }
        let capped = card_height(&asking(&["a", "b", "c"], false), false, &FAKE, 200.0);
        assert!(
            (capped - 200.0).abs() < f32::EPSILON,
            "a card taller than the work area is cut to it: {capped}"
        );
    }

    #[test]
    fn a_long_command_grows_the_card() {
        // The command box is the frame plus its lines: a command wrapping to
        // three lines adds two lines over the one-line box.
        let short = asking(&["Allow", "Deny"], true);
        let mut long = asking(&["Allow", "Deny"], true);
        long.ask = long.ask.map(|mut ask| {
            ask.command = Some("long command".to_owned());
            ask
        });
        assert_eq!(
            card_height(&long, false, &FAKE, 10_000.0),
            card_height(&short, false, &FAKE, 10_000.0) + 2.0 * FAKE.line,
            "wrapped lines grow the command's row"
        );
    }

    #[test]
    fn card_height_once_sent_counts_one_sent_row() {
        let cases = [
            (
                asking(&["long a", "long b", "c"], false),
                156.0,
                "one Sent row in the answers' place",
            ),
            (
                asking(&["a", "b"], true),
                190.0,
                "the command stays above the Sent row",
            ),
            (asking(&[], false), 120.0, "no choices: no Sent row"),
            (
                unstamped(&["a", "b"], false),
                156.0,
                "sent with choices draws Sent, stamp or none",
            ),
        ];
        for (toast, height, why) in cases {
            let drawn = card_height(&toast, true, &FAKE, 1000.0);
            assert!((drawn - height).abs() < f32::EPSILON, "{why}: {drawn}");
        }
    }

    #[test]
    fn card_height_clamps_the_body_so_the_answers_fit() {
        // The frame 22, the title 20 and its gap 4, the gap above the rows 4,
        // three answers and Go to pane at 30 each with 6 between: 188 px
        // before the body, so 112 px are left under a 300 px cap: 5 lines.
        let mut toast = asking(&["a", "b", "c"], false);
        toast.body = "many lines".to_owned();
        let layout = card_layout(&toast, false, &FAKE, 300.0);
        assert!(
            (layout.height - 300.0).abs() < f32::EPSILON,
            "the card fills the cap: {layout:?}"
        );
        assert!(layout.clamped, "the body is cut: {layout:?}");
        assert_eq!(layout.body_lines, 5, "the lines that fit: {layout:?}");
        assert!(layout.body_lines < 20, "fewer than measured: {layout:?}");
        assert!(
            (layout.fixed - 188.0).abs() < f32::EPSILON,
            "the rows are all counted: {layout:?}"
        );
        assert!(
            layout.fixed + count(layout.body_lines) * 20.0 <= 300.0,
            "the rows fit under the cut body: {layout:?}"
        );
        let whole = card_layout(&toast, false, &FAKE, 1000.0);
        assert!(!whole.clamped, "room for it all: {whole:?}");
        assert_eq!(whole.body_lines, 20, "{whole:?}");
    }

    #[test]
    fn card_height_least_button_follows_the_text_size_offset() {
        // At 10 px a line: the frame 22, the title 10 and its gap 4, the
        // body's two lines 20, the gap above the rows 4: 60; then a one-line
        // answer and Go to pane, 20 px of text and padding each, held to the
        // least height, with 6 between.
        let small = Fake {
            line: 10.0,
            offset: -4,
        };
        let drawn = card_height(&asking(&["a"], false), false, &small, 1000.0);
        assert!(
            (drawn - 114.0).abs() < f32::EPSILON,
            "24 px rows at offset -4: {drawn}"
        );
        let at_zero = Fake {
            line: 10.0,
            offset: 0,
        };
        let drawn = card_height(&asking(&["a"], false), false, &at_zero, 1000.0);
        assert!(
            (drawn - 122.0).abs() < f32::EPSILON,
            "28 px rows at offset 0: {drawn}"
        );
    }
}
