//! What an app's own toast draws: a rounded card with a stripe by kind, its
//! title and body, the prompt's command and answers, `Go to pane` and ×, and
//! the view that holds one card in a toast window of its own. The rules are
//! in `docs/native-client.md` "The app's own toasts".

use std::rc::Rc;

use chrono::{DateTime, Utc};
use gpui::{
    AnyElement, App, ClickEvent, Context, Div, IntoElement, MouseMoveEvent, Render, WeakEntity,
    Window, div, prelude::*, px,
};
use protocol::PromptChoice;

use crate::buttons::{ButtonSize, Layer, outlined_button, primary_button};
use crate::dashboard_rows::{AskBlock, AskChoice, GO_TO_PANE};
use crate::driver_id::DriverId as _;
use crate::fonts::{self, DEFAULT_FAMILY, ui_px};
use crate::toast_stack::{AppToast, AppToastKind, SessionKey};
use crate::ui_theme::theme;
use crate::{RootView, UI_TEXT_SIZE};

/// How tall a toast with no answers is, in logical px.
const CARD_HEIGHT: f32 = 116.0;

/// What the `Go to pane` row adds to a toast's height, in logical px: a
/// compact button's height. An account toast draws no such row.
const GO_ROW_HEIGHT: f32 = 28.0;

/// How tall a toast with answers is, in logical px.
const ANSWERS_HEIGHT: f32 = 152.0;

/// What a permission's command adds to a toast's height, in logical px.
const COMMAND_HEIGHT: f32 = 30.0;

/// What a second row of answers adds to a toast's height, in logical px.
const SECOND_ROW_HEIGHT: f32 = 34.0;

/// How many answers share a row.
const ANSWERS_PER_ROW: usize = 2;

/// How wide the card's accent stripe is, in logical px.
const STRIPE_WIDTH: f32 = 3.0;

/// The command box's text size, as the Dashboard's ask block draws it.
const SMALL_TEXT: f32 = 11.5;

/// The key hint's text size, as the Dashboard's ask block draws it.
const CAPS_TEXT: f32 = 10.5;

/// What an answered toast draws in its choices' place.
const SENT: &str = "Sent";

/// How tall `toast`'s card is, in logical px: the height it is placed at.
#[must_use]
pub fn card_height(toast: &AppToast) -> f32 {
    let Some(ask) = &toast.ask else {
        return match toast.kind {
            AppToastKind::Account => CARD_HEIGHT - GO_ROW_HEIGHT,
            AppToastKind::NeedsYou { .. } | AppToastKind::Finished => CARD_HEIGHT,
        };
    };
    let choices = ask.choices.len();
    let base = if choices == 0 {
        CARD_HEIGHT
    } else {
        ANSWERS_HEIGHT
    };
    let command = if ask.command.is_some() {
        COMMAND_HEIGHT
    } else {
        0.0
    };
    let second_row = if choices > ANSWERS_PER_ROW {
        SECOND_ROW_HEIGHT
    } else {
        0.0
    };
    base + command + second_row
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
    let picked = choice.choice;
    make(
        offset,
        shift,
        &format!("toast-answer-{index}"),
        ButtonSize::Compact,
        true,
        Layer::Base,
        &t,
    )
    .flex_1()
    .min_w_0()
    .gap(px(6.0))
    .child(
        div()
            .min_w_0()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis()
            .child(choice.label.clone()),
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
    .on_click(move |_, _, cx| answer(since, picked, cx))
    .into_any_element()
}

/// `ask`'s command box, its choices two to a row, or, once an answer is
/// `sent`, a disabled `Sent` in their place.
fn answer_block(ask: &AskBlock, sent: bool, answer: &AnswerPress, cx: &App) -> Div {
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
        let numbered: Vec<(usize, &AskChoice)> = ask.choices.iter().enumerate().collect();
        for row in numbered.chunks(ANSWERS_PER_ROW) {
            let buttons = row
                .iter()
                .map(|(index, choice)| answer_button(*index, choice, since, answer, cx));
            rows.push(
                div()
                    .flex()
                    .gap(px(6.0))
                    .children(buttons)
                    .into_any_element(),
            );
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
                    .child(crate::ellipsis::truncating(
                        "toast-command-text",
                        gpui::StyledText::new(command),
                    ))
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
/// toast's pane.
fn head_block(toast: &AppToast, press: Option<SharedPress>, cx: &App) -> AnyElement {
    let t = theme(cx);
    let head = div()
        .flex()
        .flex_col()
        .gap(px(4.0))
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
/// session.
pub fn toast_card(toast: &AppToast, sent: bool, presses: ToastPresses, cx: &App) -> Div {
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
    let answers = toast
        .ask
        .as_ref()
        .map(|ask| answer_block(ask, sent, &presses.answer, cx));
    let words = div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .gap(px(4.0))
        .p(px(10.0))
        .child(head_block(toast, head_press, cx))
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
        }
    }

    /// Shows `toast` as the stack now holds it, `sent` when an answer is out
    /// for its session. The root view is not read while drawing: it is
    /// mid-update when a toast window opens.
    pub(crate) fn show(&mut self, toast: AppToast, sent: bool, cx: &mut Context<Self>) {
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
        let presses = ToastPresses {
            answer: Rc::new(move |since, choice, cx| {
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
        let card = toast_card(&self.toast, self.sent, presses, cx);
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

    use super::{card_height, releases_hover};
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

    /// A toast waiting on `choices` answers, with a command when `command`.
    fn asking(choices: u16, command: bool) -> AppToast {
        let choice = |index| AskChoice {
            label: format!("choice {index}"),
            hint: None,
            choice: PromptChoice::Option { index },
        };
        toast().waiting_on(
            Some(AskBlock {
                session_id: "s1".to_owned(),
                label: "Permission ·".to_owned(),
                text: "a prompt".to_owned(),
                command: command.then(|| "gh pr merge".to_owned()),
                since: None,
                choices: (0..choices).map(choice).collect(),
            }),
            None,
        )
    }

    #[test]
    fn card_height_follows_what_the_card_draws() {
        let cases = [
            (toast(), 116.0, "no ask"),
            (asking(0, false), 116.0, "no choices"),
            (asking(2, false), 152.0, "one row of choices"),
            (asking(1, false), 152.0, "one choice"),
            (asking(2, true), 182.0, "choices and a command"),
            (asking(0, true), 146.0, "a command and no choices"),
            (asking(3, false), 186.0, "a second row of choices"),
            (asking(3, true), 216.0, "a second row and a command"),
            (account(), 88.0, "an account toast draws no Go to pane row"),
        ];
        for (toast, height, why) in cases {
            assert!((card_height(&toast) - height).abs() < f32::EPSILON, "{why}");
        }
    }
}
