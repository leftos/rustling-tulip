//! Draws the Linear hover card from its [`Card`]: a popover anchored just
//! below the hovered identifier, with no backdrop, holding the identifier,
//! priority and state, the title, the fields, the notice and the Ctrl tip's
//! line at its foot.

use std::rc::Rc;

use gpui::{
    AnyElement, AnyView, App, Context, Div, FontWeight, MouseButton, MouseDownEvent, Pixels, Point,
    Render, SharedString, Stateful, WeakEntity, Window, anchored, deferred, div, prelude::*, px,
};

use crate::driver_id::DriverId as _;
use crate::fonts::{self, DEFAULT_FAMILY, ui_px_at};
use crate::linear_card::{CHIP_FOOT, Card, Notice, OPEN_IN_LINEAR, TitleChip};
use crate::notice_view::drop_shadow;
use crate::session_menu::popover_frame;
use crate::ui_theme::{self, UiTheme};
use crate::{RootView, UI_TEXT_SIZE};

/// The card's width, as the mock's.
const CARD_WIDTH: f32 = 300.0;
/// The corner radius of a title chip's card, as a popover's.
const PASSIVE_RADIUS: f32 = 10.0;
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
    let hover = actions.hover;
    let frame = card_frame(pane_id, card, Some(&actions.open), cx)
        .on_hover(move |over: &bool, window, cx| hover(*over, window, cx));
    let card = anchored().position(at).snap_to_window().child(frame);
    deferred(card).with_priority(1).into_any_element()
}

/// The gap added under a title chip's height before its card, in px: the card,
/// a tooltip at the pointer, starts this far plus the chip's height below it.
pub(crate) const CARD_GAP: f32 = 4.0;

/// The card of title chip `chip`, tagged `owner`, as the chip's tooltip:
/// the same card, drawn where gpui draws a tooltip and read from `root`'s
/// cache at each render, so an open card follows a reply. A transparent
/// spacer `below` tall (the chip's height and [`CARD_GAP`]) tops it, so
/// the card starts under the chip rather than at the pointer. It takes no
/// pointer: a press on it reaches the chip under it.
pub(crate) fn card_tooltip(
    owner: String,
    root: WeakEntity<RootView>,
    chip: TitleChip,
    below: Pixels,
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    move |_, cx| {
        let tip = CardTip {
            owner: owner.clone(),
            root: root.clone(),
            chip: chip.clone(),
            below,
        };
        cx.new(|cx| {
            if let Some(root) = tip.root.upgrade() {
                cx.observe(&root, |_, _, cx| cx.notify()).detach();
            }
            tip
        })
        .into()
    }
}

/// A title chip's card, drawn as its tooltip under a spacer `below` tall.
struct CardTip {
    owner: String,
    root: WeakEntity<RootView>,
    chip: TitleChip,
    below: Pixels,
}

impl Render for CardTip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let card = self.root.upgrade().map_or_else(
            || self.chip.card.clone(),
            |root| root.read(cx).chip_card(&self.chip),
        );
        div()
            .pt(self.below)
            .child(card_frame(&self.owner, &card, None, cx))
    }
}

/// The card's box, tagged by `owner` (its pane, or its title chip): the
/// header, the title, the fields, the notice and the foot. With `open` it
/// is the terminal's card, which takes the pointer and whose title and
/// button open the issue; without, a title chip's passive tooltip, whose
/// foot names the chip's plain click.
fn card_frame(owner: &str, card: &Card, open: Option<&OpenCard>, cx: &App) -> Stateful<Div> {
    let t = ui_theme::theme(cx);
    let offset = fonts::ui_text_size(cx);
    let pane = owner.to_owned();
    let foot_text = if open.is_some() {
        Card::foot()
    } else {
        CHIP_FOOT
    };
    let foot = div()
        .text_color(gpui::rgb(t.faint))
        .driver_id(move || format!("linear-card-foot-{pane}={foot_text}"))
        .child(foot_text);
    let frame = if open.is_some() {
        popover_frame(offset, "linear-card", &t)
    } else {
        passive_frame(offset, &t)
    };
    frame
        .w(px(CARD_WIDTH))
        .px(px(12.0))
        .py(px(10.0))
        .flex()
        .flex_col()
        .gap(px(4.0))
        .font_family(fonts::ui_family(cx))
        .child(header(owner, card, &t))
        .children(title(owner, card, open))
        .children(
            card.fields
                .iter()
                .map(|(label, value)| field(label, value, &t)),
        )
        .children(
            card.notice
                .as_ref()
                .map(|notice| notice_row(owner, card, notice, open, &t)),
        )
        .child(foot)
}

/// The card's box as a tooltip: the popover's look, without taking the
/// pointer, so a press on it goes through to what lies under it.
fn passive_frame(offset: i8, t: &UiTheme) -> Stateful<Div> {
    div()
        .id("linear-card")
        .driver_id(|| "linear-card".to_owned())
        .bg(gpui::rgb(t.popover))
        .border_1()
        .border_color(gpui::rgb(t.popover_edge))
        .rounded(px(PASSIVE_RADIUS))
        .shadow(vec![drop_shadow(t.popover_shadow, 8.0, 24.0)])
        .text_size(ui_px_at(offset, UI_TEXT_SIZE))
        .text_color(gpui::rgb(t.text))
}

/// The identifier, then the priority and state pills at the right; the
/// identifier and the state are tagged by `owner`.
fn header(owner: &str, card: &Card, t: &UiTheme) -> Div {
    let (id_owner, state_owner) = (owner.to_owned(), owner.to_owned());
    let identifier = card.identifier.clone();
    div()
        .flex()
        .items_center()
        .gap(px(6.0))
        .child(
            div()
                .font_family(DEFAULT_FAMILY)
                .text_color(gpui::rgb(t.subtle))
                .driver_id(move || format!("linear-card-id-{id_owner}={identifier}"))
                .child(card.identifier.clone()),
        )
        .child(div().flex_1())
        .children(card.priority.clone().map(|label| pill(t.faint, label, t)))
        .children(card.state.as_ref().map(|state| {
            let name = state.name.clone();
            pill(state.color.unwrap_or(t.faint), state.name.clone(), t)
                .driver_id(move || format!("linear-card-state-{state_owner}={name}"))
        }))
}

/// A session title chip's 6 px dot: Linear's colour for its issue's state,
/// else the theme's muted text colour.
pub(crate) fn issue_dot(color: Option<u32>, t: &UiTheme) -> Div {
    div()
        .flex_none()
        .size(px(6.0))
        .rounded_full()
        .bg(gpui::rgb(color.unwrap_or(t.muted)))
}

/// A Linear title chip's press target, tagged `selector`, which also owns
/// its card: the state dot, then the identifier, tagged
/// `<selector>=<IDENTIFIER>`. Hovering it asks the chip's host for its
/// issue and shows the card; a left press opens the issue and goes no
/// further, so the row under it neither selects nor opens anything.
pub(crate) fn issue_target(
    selector: String,
    issue: &TitleChip,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    let t = ui_theme::theme(cx);
    let (tag, text_tag) = (selector.clone(), format!("{selector}={}", issue.identifier));
    let (url, host) = (issue.open_url().to_owned(), issue.host.clone());
    let root = cx.entity().downgrade();
    let below =
        crate::sidebar_view::ChipSize::Row.drawn_height(fonts::ui_text_size(cx)) + px(CARD_GAP);
    div()
        .id(SharedString::from(selector.clone()))
        .driver_id(move || tag)
        .flex()
        .flex_none()
        .items_center()
        .gap(px(5.0))
        .cursor_pointer()
        .on_hover(RootView::ask_on_chip_hover(issue, cx))
        .tooltip(card_tooltip(selector, root, issue.clone(), below))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                this.on_any_mouse_down(event, window, cx);
                cx.stop_propagation();
                this.open_issue_link(&url, &host, window, cx);
            }),
        )
        .child(issue_dot(issue.dot, &t))
        .child(
            div()
                .font_family(DEFAULT_FAMILY)
                .driver_id(move || text_tag)
                .child(issue.identifier.clone()),
        )
}

/// [`issue_target`] in the sidebar leaf chip's outlined frame, as a
/// Dashboard session title leads with it.
pub(crate) fn framed_issue_chip(
    selector: String,
    issue: &TitleChip,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let t = ui_theme::theme(cx);
    let offset = fonts::ui_text_size(cx);
    let target = issue_target(selector, issue, cx);
    crate::sidebar_view::chip(offset, target, crate::sidebar_view::ChipSize::Row, &t)
        .into_any_element()
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

/// The issue's title, which with `open` opens the issue when clicked;
/// without, plain text tagged by `owner`.
fn title(owner: &str, card: &Card, open: Option<&OpenCard>) -> Option<Div> {
    let title = card.title.clone()?;
    let Some(open) = open else {
        let tag = format!("linear-card-title-{owner}={title}");
        return Some(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .driver_id(|| tag)
                .child(title),
        );
    };
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

/// The notice's line, tagged by `owner`, in the asking colour when it
/// warns, with Open in Linear under it when it offers one and the card has
/// `open`.
fn notice_row(
    owner: &str,
    card: &Card,
    notice: &Notice,
    open: Option<&OpenCard>,
    t: &UiTheme,
) -> Div {
    let color = if notice.warns() { t.asking } else { t.subtle };
    let text = notice.text(&card.identifier);
    let tag = format!("linear-card-notice-{owner}={text}");
    let button = open.filter(|_| notice.offers_open()).map(|open| {
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
                .driver_id(|| tag)
                .child(text),
        )
        .children(button)
}
