//! The client's buttons: one look for each kind and size, and the keyboard
//! focus ring drawn just outside a focused control's border.
//!
//! [`button_look`] decides the look as plain values; [`button`] draws it.
//! A button carries no label, tooltip or handler: the caller adds them.

use gpui::{
    Div, ElementId, FontWeight, MouseButton, Pixels, SharedString, Stateful, div, prelude::*, px,
};

use crate::driver_id::DriverId as _;
use crate::fonts::ui_px_at;
use crate::ui_theme::UiTheme;

/// The opacity of a disabled button.
const DISABLED_OPACITY: f32 = 0.45;
/// The width of the border every button and field draws.
const CONTROL_BORDER: f32 = 1.0;
/// The width of the keyboard focus ring.
const FOCUS_RING_WIDTH: f32 = 2.0;
/// The padding a scrolling list leaves inside its edge, so a focused
/// child's ring shows whole.
pub(crate) const RING_ROOM: f32 = CONTROL_BORDER + FOCUS_RING_WIDTH;
/// A button's padding above and below its label; the least height keeps a
/// one-line label centred.
const BUTTON_PAD_Y: f32 = 4.0;

/// What a button is for, and so how it is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ButtonKind {
    /// Transparent, with a strong outline and ordinary text.
    Outlined,
    /// Filled with the accent, with dark bold text: the action a surface
    /// offers first.
    Primary,
    /// Outlined, with the danger colour's text: an action that destroys
    /// something.
    Danger,
}

/// How big a button is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ButtonSize {
    /// 32 px tall: a dialog's buttons.
    Regular,
    /// 28 px tall: a toast's, an empty pane's and an overlay's buttons.
    Compact,
    /// 36 px tall: a wide dialog's footer.
    Large,
    /// 30 px tall: the sessions panel's toolbar.
    Toolbar,
}

impl ButtonSize {
    /// The least outer height, the side padding, the outer corner radius and
    /// the label's size, in px.
    const fn metrics(self) -> (f32, f32, f32, f32) {
        match self {
            Self::Regular => (32.0, 14.0, 7.0, 12.5),
            Self::Compact => (28.0, 12.0, 6.0, 12.0),
            Self::Large => (36.0, 16.0, 8.0, 13.0),
            Self::Toolbar => (30.0, 10.0, 6.0, 12.5),
        }
    }

    /// The outer corner radius, in px.
    pub(crate) const fn radius(self) -> f32 {
        self.metrics().2
    }
}

/// How a button is drawn: its box, its fills, its border and its text.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ButtonLook {
    /// The least outer height, border included, at the UI text offset; a
    /// wrapped label grows the button past it.
    pub height: Pixels,
    /// The padding at each side, in px.
    pub pad_x: f32,
    /// The outer corner radius, in px.
    pub radius: f32,
    /// The label's size, at the UI text offset.
    pub text_size: Pixels,
    /// The fill at rest, as `0xRRGGBB`; none is transparent.
    pub fill: Option<u32>,
    /// The fill under the pointer; none leaves the fill alone.
    pub hover_fill: Option<u32>,
    /// The fill while pressed; none leaves the fill alone.
    pub pressed_fill: Option<u32>,
    /// The 1 px border's colour; none draws it transparent, so every kind
    /// keeps the same outer size.
    pub border: Option<u32>,
    /// The label's colour, as `0xRRGGBB`.
    pub text: u32,
    /// The label's weight, at the UI weight shift.
    pub weight: FontWeight,
    /// The whole button's opacity.
    pub opacity: f32,
    /// Whether the button takes a press.
    pub enabled: bool,
}

/// The layer a button is drawn on, which picks its hover and press fills
/// and its danger text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Layer {
    /// The dark surfaces: the ground, the sidebar, a panel, a row card.
    Base,
    /// A modal, a popover, a menu or a toast.
    Floating,
}

impl Layer {
    /// The fill of a hovered row or button on this layer in `t`, as
    /// `0xRRGGBB`.
    pub(crate) fn hover(self, t: &UiTheme) -> u32 {
        match self {
            Self::Base => t.hover,
            Self::Floating => t.popover_hover,
        }
    }

    /// The fill of a pressed button on this layer in `t`, as `0xRRGGBB`.
    pub(crate) fn pressed(self, t: &UiTheme) -> u32 {
        match self {
            Self::Base => t.pressed,
            Self::Floating => t.popover_pressed,
        }
    }

    /// Danger text on this layer in `t`, as `0xRRGGBB`.
    pub(crate) fn danger_text(self, t: &UiTheme) -> u32 {
        match self {
            Self::Base => t.danger,
            Self::Floating => t.danger_text,
        }
    }
}

/// The look of a `kind` button of `size` at UI text `offset` and UI weight
/// `shift`, drawn on `layer` in `t`; a disabled one is dimmed and neither hover nor press changes it.
pub(crate) fn button_look(
    offset: i8,
    shift: i8,
    kind: ButtonKind,
    size: ButtonSize,
    enabled: bool,
    layer: Layer,
    t: &UiTheme,
) -> ButtonLook {
    let (height, pad_x, radius, text_size) = size.metrics();
    // Outlined and danger take their layer's hover and press fills; the
    // primary lightens on hover and darkens on press.
    let (fill, hover, pressed, border, text, weight) = match kind {
        ButtonKind::Outlined => (
            None,
            layer.hover(t),
            layer.pressed(t),
            Some(t.line_strong),
            t.text,
            FontWeight::NORMAL,
        ),
        ButtonKind::Danger => (
            None,
            layer.hover(t),
            layer.pressed(t),
            Some(t.line_strong),
            layer.danger_text(t),
            FontWeight::NORMAL,
        ),
        ButtonKind::Primary => (
            Some(t.primary),
            t.primary_hover,
            t.primary_pressed,
            None,
            t.on_accent,
            FontWeight::BOLD,
        ),
    };
    ButtonLook {
        height: ui_px_at(offset, height),
        pad_x,
        radius,
        text_size: ui_px_at(offset, text_size),
        fill,
        hover_fill: enabled.then_some(hover),
        pressed_fill: enabled.then_some(pressed),
        border,
        text,
        weight: crate::fonts::ui_weight_at(shift, weight),
        opacity: if enabled { 1.0 } else { DISABLED_OPACITY },
        enabled,
    }
}

/// A button drawn as `look` (see [`button_look`]), tagged `selector`, with
/// no label, tooltip or handler. A disabled one has the default cursor and
/// its left press stops at it; its caller attaches no handler, so a click
/// reaches nothing.
pub(crate) fn button(selector: &str, look: ButtonLook) -> Stateful<Div> {
    let name = selector.to_owned();
    let base = div()
        .id(ElementId::Name(SharedString::from(name.clone())))
        .driver_id(|| name)
        .flex()
        .items_center()
        .justify_center()
        .min_h(look.height)
        .px(px(look.pad_x))
        .py(px(BUTTON_PAD_Y))
        .rounded(px(look.radius))
        .border_1()
        .border_color(
            look.border
                .map_or_else(gpui::transparent_black, |border| gpui::rgb(border).into()),
        )
        .text_size(look.text_size)
        .font_weight(look.weight)
        .text_color(gpui::rgb(look.text))
        .when_some(look.fill, |button, fill| button.bg(gpui::rgb(fill)))
        .when_some(look.hover_fill, |button, fill| {
            button.hover(move |style| style.bg(gpui::rgb(fill)))
        })
        .when_some(look.pressed_fill, |button, fill| {
            button.active(move |style| style.bg(gpui::rgb(fill)))
        });
    if look.enabled {
        base.cursor_pointer()
    } else {
        base.opacity(look.opacity)
            .cursor_default()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
    }
}

/// An [`ButtonKind::Outlined`] button in `t`; see [`button`].
pub(crate) fn outlined_button(
    offset: i8,
    shift: i8,
    selector: &str,
    size: ButtonSize,
    enabled: bool,
    layer: Layer,
    t: &UiTheme,
) -> Stateful<Div> {
    let look = button_look(offset, shift, ButtonKind::Outlined, size, enabled, layer, t);
    button(selector, look)
}

/// A [`ButtonKind::Primary`] button in `t`; see [`button`].
pub(crate) fn primary_button(
    offset: i8,
    shift: i8,
    selector: &str,
    size: ButtonSize,
    enabled: bool,
    layer: Layer,
    t: &UiTheme,
) -> Stateful<Div> {
    let look = button_look(offset, shift, ButtonKind::Primary, size, enabled, layer, t);
    button(selector, look)
}

/// The keyboard focus ring of a control whose outer corners have `radius`:
/// a 2 px ring in `t`'s accent just outside its 1 px border. The control
/// adds it as a child; it takes no room in the control's layout.
pub(crate) fn focus_ring(radius: f32, t: &UiTheme) -> Div {
    let inset = px(-(CONTROL_BORDER + FOCUS_RING_WIDTH));
    div()
        .absolute()
        .top(inset)
        .left(inset)
        .right(inset)
        .bottom(inset)
        .border_2()
        .border_color(gpui::rgb(t.accent))
        .rounded(px(radius + FOCUS_RING_WIDTH))
}

/// How a text field's frame is drawn.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct FieldLook {
    /// The least outer height, border included, in px.
    pub min_height: f32,
    /// The padding at each side, in px.
    pub pad_x: f32,
    /// The outer corner radius, in px.
    pub radius: f32,
    /// The fill, as `0xRRGGBB`.
    pub fill: u32,
    /// The 1 px border's colour, as `0xRRGGBB`.
    pub border: u32,
    /// Whether the focus ring shows.
    pub ring: bool,
}

/// The look of a text field's frame in `t`, ringed while `focused`.
pub(crate) fn field_look(focused: bool, t: &UiTheme) -> FieldLook {
    FieldLook {
        min_height: 34.0,
        pad_x: 11.0,
        radius: 7.0,
        fill: t.ground,
        border: t.line_strong,
        ring: focused,
    }
}

/// The frame of a text field in `t`, ringed while `focused`; the caller
/// adds the input as its child.
pub(crate) fn field_frame(focused: bool, t: &UiTheme) -> Div {
    framed_field(focused, None, t)
}

/// [`field_frame`] whose focus ring, while drawn, is tagged `ring_selector`.
pub(crate) fn field_frame_with_ring(focused: bool, ring_selector: String, t: &UiTheme) -> Div {
    framed_field(focused, Some(ring_selector), t)
}

fn framed_field(focused: bool, ring_selector: Option<String>, t: &UiTheme) -> Div {
    let look = field_look(focused, t);
    let ring = focus_ring(look.radius, t);
    let ring = match ring_selector {
        Some(selector) => ring.driver_id(move || selector),
        None => ring,
    };
    div()
        .flex()
        .items_center()
        .min_h(px(look.min_height))
        .px(px(look.pad_x))
        .bg(gpui::rgb(look.fill))
        .border_1()
        .border_color(gpui::rgb(look.border))
        .rounded(px(look.radius))
        .when(look.ring, |field| field.child(ring))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::{
        DANGER, DANGER_TEXT, GROUND, HOVER, LINE_STRONG, ON_ACCENT, POPOVER_HOVER, POPOVER_PRESSED,
        PRESSED, PRIMARY, PRIMARY_HOVER, PRIMARY_PRESSED, TEXT,
    };

    const T: &UiTheme = &UiTheme::CONTRAST;

    #[test]
    fn field_frame_is_ground_with_line_strong_at_34_px() {
        let look = field_look(false, T);
        assert_eq!(
            (look.min_height, look.pad_x, look.radius),
            (34.0, 11.0, 7.0)
        );
        assert_eq!(look.fill, GROUND);
        assert_eq!(look.border, LINE_STRONG);
        assert!(!look.ring, "an unfocused field has no ring");
    }

    #[test]
    fn focused_field_wears_the_ring() {
        assert_eq!(
            field_look(true, T),
            FieldLook {
                ring: true,
                ..field_look(false, T)
            }
        );
    }

    #[test]
    fn size_radius_matches_the_look() {
        for size in SIZES {
            assert!(
                (size.radius()
                    - button_look(0, 0, ButtonKind::Outlined, size, true, Layer::Base, T).radius)
                    .abs()
                    < f32::EPSILON
            );
        }
    }

    const SIZES: [ButtonSize; 4] = [
        ButtonSize::Regular,
        ButtonSize::Compact,
        ButtonSize::Large,
        ButtonSize::Toolbar,
    ];

    #[test]
    fn outlined_button_is_line_strong_on_transparent() {
        let look = button_look(
            0,
            0,
            ButtonKind::Outlined,
            ButtonSize::Regular,
            true,
            Layer::Base,
            T,
        );
        assert_eq!(look.fill, None);
        assert_eq!(look.border, Some(LINE_STRONG));
        assert_eq!(look.text, TEXT);
        assert_eq!(look.weight, FontWeight::NORMAL);
        assert_eq!(look.hover_fill, Some(HOVER));
        assert_eq!(look.pressed_fill, Some(PRESSED));
        let floating = button_look(
            0,
            0,
            ButtonKind::Outlined,
            ButtonSize::Regular,
            true,
            Layer::Floating,
            T,
        );
        assert_eq!(floating.hover_fill, Some(POPOVER_HOVER));
        assert_eq!(floating.pressed_fill, Some(POPOVER_PRESSED));
    }

    #[test]
    fn primary_button_is_light_fill_with_on_accent_bold() {
        let look = button_look(
            0,
            0,
            ButtonKind::Primary,
            ButtonSize::Compact,
            true,
            Layer::Base,
            T,
        );
        assert_eq!(look.fill, Some(PRIMARY));
        assert_eq!(look.text, ON_ACCENT);
        assert_eq!(look.weight, FontWeight::BOLD);
        assert_eq!(look.border, None, "no visible border");
        assert_eq!(look.hover_fill, Some(PRIMARY_HOVER));
        assert_eq!(look.pressed_fill, Some(PRIMARY_PRESSED));
        let outlined = button_look(
            0,
            0,
            ButtonKind::Outlined,
            ButtonSize::Compact,
            true,
            Layer::Base,
            T,
        );
        assert_eq!(
            (look.height, look.pad_x),
            (outlined.height, outlined.pad_x),
            "the same outer size as an outlined button"
        );
    }

    #[test]
    fn danger_button_is_outlined_with_danger_text() {
        let look = button_look(
            0,
            0,
            ButtonKind::Danger,
            ButtonSize::Regular,
            true,
            Layer::Base,
            T,
        );
        let outlined = button_look(
            0,
            0,
            ButtonKind::Outlined,
            ButtonSize::Regular,
            true,
            Layer::Base,
            T,
        );
        assert_eq!(look.text, DANGER);
        assert_eq!(ButtonLook { text: TEXT, ..look }, outlined);
        let floating = button_look(
            0,
            0,
            ButtonKind::Danger,
            ButtonSize::Regular,
            true,
            Layer::Floating,
            T,
        );
        assert_eq!(
            floating.text, DANGER_TEXT,
            "a lighter danger on a floating layer"
        );
    }

    #[test]
    fn button_sizes_follow_the_boards() {
        let boxes: Vec<(f32, f32, f32, f32)> = SIZES
            .iter()
            .map(|&size| {
                let look = button_look(0, 0, ButtonKind::Outlined, size, true, Layer::Base, T);
                (
                    f32::from(look.height),
                    look.pad_x,
                    look.radius,
                    f32::from(look.text_size),
                )
            })
            .collect();
        assert_eq!(
            boxes,
            [
                (32.0, 14.0, 7.0, 12.5),
                (28.0, 12.0, 6.0, 12.0),
                (36.0, 16.0, 8.0, 13.0),
                (30.0, 10.0, 6.0, 12.5),
            ]
        );
    }

    #[test]
    fn button_look_grows_with_the_offset() {
        for size in SIZES {
            let look =
                |offset| button_look(offset, 0, ButtonKind::Outlined, size, true, Layer::Base, T);
            let base = look(0);
            for (offset, by) in [(2, px(2.0)), (-2, px(-2.0))] {
                let scaled = look(offset);
                assert_eq!(
                    scaled.height,
                    base.height + by,
                    "{size:?} height at {offset}"
                );
                assert_eq!(
                    scaled.text_size,
                    base.text_size + by,
                    "{size:?} text at {offset}"
                );
                assert_eq!(
                    (scaled.pad_x, scaled.radius),
                    (base.pad_x, base.radius),
                    "{size:?} keeps its padding and corners"
                );
            }
        }
    }

    #[test]
    fn button_look_moves_its_weight_by_the_shift() {
        let weight = |shift, kind| {
            button_look(0, shift, kind, ButtonSize::Regular, true, Layer::Base, T).weight
        };
        assert_eq!(weight(1, ButtonKind::Outlined), FontWeight::MEDIUM);
        assert_eq!(weight(2, ButtonKind::Danger), FontWeight::SEMIBOLD);
        assert_eq!(
            weight(2, ButtonKind::Primary),
            FontWeight::BOLD,
            "already at the cap"
        );
    }

    #[gpui::test]
    fn a_shifted_weight_reaches_a_button(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::fonts::set_ui_weight(600, cx);
            let look = button_look(
                crate::fonts::ui_text_size(cx),
                crate::fonts::ui_weight_shift(cx),
                ButtonKind::Outlined,
                ButtonSize::Regular,
                true,
                Layer::Floating,
                T,
            );
            assert_eq!(look.weight, FontWeight::SEMIBOLD);
        });
    }

    #[test]
    fn disabled_button_dims_to_45_percent() {
        for kind in [
            ButtonKind::Outlined,
            ButtonKind::Primary,
            ButtonKind::Danger,
        ] {
            for size in SIZES {
                let look = button_look(0, 0, kind, size, false, Layer::Base, T);
                assert!(
                    (look.opacity - 0.45).abs() < f32::EPSILON,
                    "{kind:?} {size:?}"
                );
                assert_eq!(look.hover_fill, None, "{kind:?} {size:?} takes no hover");
                assert_eq!(look.pressed_fill, None, "{kind:?} {size:?} takes no press");
                let enabled = button_look(0, 0, kind, size, true, Layer::Base, T);
                assert!((enabled.opacity - 1.0).abs() < f32::EPSILON);
            }
        }
    }
}
