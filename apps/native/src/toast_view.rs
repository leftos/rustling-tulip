//! What an app's own toast draws: a rounded card with a stripe by kind, its
//! title and body, `Go to pane` and ×, and the view that holds one card in a
//! toast window of its own. The rules are in `docs/plans/custom-toasts.md`.

use gpui::{
    App, ClickEvent, Context, Div, IntoElement, MouseMoveEvent, Render, WeakEntity, Window, div,
    prelude::*, px,
};

use crate::buttons::{ButtonSize, Layer, outlined_button};
use crate::dashboard_rows::GO_TO_PANE;
use crate::driver_id::DriverId as _;
use crate::fonts::{self, ui_px};
use crate::toast_stack::{AppToast, AppToastKind};
use crate::ui_theme::theme;
use crate::{RootView, UI_TEXT_SIZE};

/// How tall a toast is, in logical px.
const CARD_HEIGHT: f32 = 116.0;

/// How wide the card's accent stripe is, in logical px.
const STRIPE_WIDTH: f32 = 3.0;

/// How tall `toast`'s card is, in logical px: the height it is placed at.
#[must_use]
pub fn card_height(_toast: &AppToast) -> f32 {
    CARD_HEIGHT
}

/// `toast`'s card, filling its parent: `on_go` is `Go to pane`'s press and
/// `on_dismiss` is ×'s.
pub fn toast_card(
    toast: &AppToast,
    on_go: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_dismiss: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> Div {
    let t = theme(cx);
    let offset = crate::fonts::ui_text_size(cx);
    let shift = crate::fonts::ui_weight_shift(cx);
    let accent = match toast.kind {
        AppToastKind::NeedsYou { .. } => t.asking,
        AppToastKind::Finished => t.muted,
    };
    let go = outlined_button(
        offset,
        shift,
        "toast-go-to-pane",
        ButtonSize::Compact,
        true,
        Layer::Base,
        &t,
    )
    .text_color(gpui::rgb(t.text_2))
    .child(GO_TO_PANE)
    .on_click(on_go);
    let dismiss = div()
        .id("toast-dismiss")
        .driver_id(|| "toast-dismiss".to_owned())
        .absolute()
        .top(px(6.0))
        .right(px(8.0))
        .cursor_pointer()
        .text_color(gpui::rgb(t.muted))
        .child("×")
        .on_click(on_dismiss);
    let words = div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .gap(px(4.0))
        .p(px(10.0))
        .child(
            crate::ellipsis::truncating(
                &format!("toast-title-{}", toast.key.1),
                gpui::StyledText::new(toast.title.clone()),
            )
            .pr(px(16.0))
            .text_color(gpui::rgb(t.text)),
        )
        .child(
            div()
                .overflow_hidden()
                .text_ellipsis()
                .line_clamp(2)
                .text_color(gpui::rgb(t.text_2))
                .child(toast.body.clone()),
        )
        .child(div().mt_auto().flex().child(go));
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
    /// Whether the pointer rests on the toast, as last told to the root view.
    hovering: bool,
    /// Whether the window reported the pointer at its last move over the
    /// toast. Only such a window can report it leaving.
    window_saw_pointer: bool,
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
            hovering: false,
            window_saw_pointer: false,
        }
    }

    /// A later toast for the session takes this one's place.
    pub(crate) fn replace(&mut self, toast: AppToast, cx: &mut Context<Self>) {
        self.toast = toast;
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

impl Render for ToastView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.follow_window_hover(window, cx);
        let (go_root, dismiss_root) = (self.root.clone(), self.root.clone());
        let (go_key, dismiss_key) = (self.toast.key.clone(), self.toast.key.clone());
        let card = toast_card(
            &self.toast,
            move |_, _, cx| RootView::toast_go_to_pane(&go_root, &go_key, cx),
            move |_, _, cx| {
                let dismissed = dismiss_root.update(cx, |root, cx| {
                    root.dismiss_app_toast(&dismiss_key, cx);
                });
                if let Err(err) = dismissed {
                    tracing::debug!("toast dismissed after the root view went: {err:#}");
                }
            },
            cx,
        );
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

    use super::{CARD_HEIGHT, card_height, releases_hover};
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

    #[test]
    fn card_height_is_the_placed_height() {
        let toast = AppToast::new(
            key_of(&HostKey::Local, "s1"),
            AppToastKind::Finished,
            "a title".to_owned(),
            "a body".to_owned(),
            Instant::now(),
        );
        assert!((card_height(&toast) - CARD_HEIGHT).abs() < f32::EPSILON);
        assert!((CARD_HEIGHT - 116.0).abs() < f32::EPSILON);
    }
}
