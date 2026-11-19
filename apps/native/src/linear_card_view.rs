//! Draws the Linear hover card from its [`Card`]: a popover anchored just
//! below the hovered identifier, with no backdrop, holding the identifier,
//! priority and state, the title, the fields, the notice and the Ctrl tip's
//! line at its foot.

use std::rc::Rc;

use gpui::{
    AnyElement, App, Div, FontWeight, MouseButton, MouseDownEvent, Pixels, Point, Window, anchored,
    deferred, div, prelude::*, px,
};

use crate::driver_id::DriverId as _;
use crate::fonts::{self, DEFAULT_FAMILY};
use crate::linear_card::{Card, Notice, OPEN_IN_LINEAR};
use crate::session_menu::popover_frame;
use crate::ui_theme::{self, UiTheme};

/// The card's width, as the mock's.
const CARD_WIDTH: f32 = 300.0;
/// The width of a field's label column.
const LABEL_WIDTH: f32 = 64.0;

/// Opens the card's issue.
pub(crate) type OpenCard = Rc<dyn Fn(&mut Window, &mut App)>;
/// Follows the pointer onto the card (`true`) and off it.
pub(crate) type HoverCard = dyn Fn(bool, &mut Window, &mut App);

/// What the card's controls do: open the issue (Open in Linear, or a click
/// on the title), and follow the pointer onto and off the card.
pub(crate) struct CardActions {
    pub open: OpenCard,
    pub hover: Box<HoverCard>,
}

/// The card `card` of pane `pane_id` at window point `at`, kept inside the
/// window and drawn over everything as the link tip is.
pub(crate) fn card_at(
    pane_id: &str,
    card: &Card,
    at: Point<Pixels>,
    actions: CardActions,
    cx: &App,
) -> AnyElement {
    let t = ui_theme::theme(cx);
    let pane = pane_id.to_owned();
    let hover = actions.hover;
    let foot = div()
        .text_color(gpui::rgb(t.faint))
        .driver_id(move || format!("linear-card-foot-{pane}={}", Card::foot()))
        .child(Card::foot());
    let frame = popover_frame(fonts::ui_text_size(cx), "linear-card", &t)
        .w(px(CARD_WIDTH))
        .px(px(12.0))
        .py(px(10.0))
        .flex()
        .flex_col()
        .gap(px(4.0))
        .font_family(fonts::ui_family(cx))
        .on_hover(move |over: &bool, window, cx| hover(*over, window, cx))
        .child(header(card, &t))
        .children(title(card, &actions.open))
        .children(
            card.fields
                .iter()
                .map(|(label, value)| field(label, value, &t)),
        )
        .children(
            card.notice
                .as_ref()
                .map(|notice| notice_row(card, notice, &actions.open, &t)),
        )
        .child(foot);
    let card = anchored().position(at).snap_to_window().child(frame);
    deferred(card).with_priority(1).into_any_element()
}

/// The identifier, then the priority and state pills at the right.
fn header(card: &Card, t: &UiTheme) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(6.0))
        .child(
            div()
                .font_family(DEFAULT_FAMILY)
                .text_color(gpui::rgb(t.subtle))
                .child(card.identifier.clone()),
        )
        .child(div().flex_1())
        .children(card.priority.clone().map(|label| pill(t.faint, label, t)))
        .children(
            card.state
                .as_ref()
                .map(|state| pill(state.color.unwrap_or(t.faint), state.name.clone(), t)),
        )
}

/// A pill of `label` behind a dot of colour `dot`.
fn pill(dot: u32, label: String, t: &UiTheme) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(5.0))
        .px(px(7.0))
        .rounded(px(999.0))
        .bg(gpui::rgb(t.raised))
        .border_1()
        .border_color(gpui::rgb(t.line))
        .child(div().size(px(8.0)).rounded_full().bg(gpui::rgb(dot)))
        .child(label)
}

/// The issue's title, which opens the issue when clicked.
fn title(card: &Card, open: &OpenCard) -> Option<Div> {
    let title = card.title.clone()?;
    let open = Rc::clone(open);
    Some(
        div()
            .font_weight(FontWeight::SEMIBOLD)
            .cursor_pointer()
            .driver_id(|| "linear-card-title".to_owned())
            .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                open(window, cx);
            })
            .child(title),
    )
}

/// One field: its label, then its value.
fn field(label: &str, value: &str, t: &UiTheme) -> Div {
    div()
        .flex()
        .gap(px(10.0))
        .child(
            div()
                .w(px(LABEL_WIDTH))
                .flex_none()
                .text_color(gpui::rgb(t.faint))
                .child(label.to_owned()),
        )
        .child(div().child(value.to_owned()))
}

/// The notice's line, in the asking colour when it warns, with Open in
/// Linear under it when it offers one.
fn notice_row(card: &Card, notice: &Notice, open: &OpenCard, t: &UiTheme) -> Div {
    let color = if notice.warns() { t.asking } else { t.subtle };
    let button = notice.offers_open().then(|| {
        let open = Rc::clone(open);
        div()
            .mt(px(4.0))
            .self_start()
            .px(px(9.0))
            .py(px(3.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(gpui::rgb(t.accent))
            .text_color(gpui::rgb(t.accent))
            .cursor_pointer()
            .driver_id(|| "linear-card-open".to_owned())
            .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                open(window, cx);
            })
            .child(OPEN_IN_LINEAR)
    });
    div()
        .flex()
        .flex_col()
        .child(
            div()
                .text_color(gpui::rgb(color))
                .child(notice.text(&card.identifier)),
        )
        .children(button)
}
