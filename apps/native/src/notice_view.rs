//! Spawning, the toasts, the action-failed notice and the checkout prompt on
//! screen: rendering, keys and clicks, forwarded to [`crate::notices`] and
//! [`crate::spawns`].

use gpui::{
    AnyElement, ClickEvent, Context, Div, ElementId, FontWeight, Keystroke, SharedString, Stateful,
    Window, div, prelude::*, px,
};
use protocol::{SessionSnapshot, SpawnRequest};

use crate::notices::{
    ActionFailedNotice, CHECKOUT_TITLE, CheckoutChoice, CheckoutPrompt, Toast, ToastKind,
};
use crate::session_menu::{BACKDROP_TINT, dialog_button};
use crate::spawns::OpenIn;
use crate::{BORDER, DANGER, FOOTER_HEIGHT, MUTED, PANEL_BG, RootView, TEXT, UI_TEXT_SIZE};

const TOAST_WIDTH: f32 = 320.0;
const MODAL_WIDTH: f32 = 440.0;
const SPAWNING_TITLE: &str = "Spawning session…";
const SPAWNING_DETAIL: &str = "Worktree creation may take a few seconds.";
const DAEMON_ERROR_TITLE: &str = "Daemon error";

impl RootView {
    /// Asks the daemon for a new session and places it by `open_in` when
    /// its reply arrives. A "Spawning session…" toast shows meanwhile.
    pub fn spawn(&mut self, request: SpawnRequest, open_in: OpenIn, cx: &mut Context<Self>) {
        let request_id = crate::new_request_id();
        let msg = self.spawns.start(request, request_id, open_in);
        self.send(msg);
        self.push_spawning_toast(cx);
    }

    /// The toasts on screen, oldest first.
    #[must_use]
    pub fn toasts(&self) -> &[Toast] {
        self.notices.toasts()
    }

    /// The refused action's notice, while shown.
    #[must_use]
    pub fn action_failed(&self) -> Option<&ActionFailedNotice> {
        self.notices.action_failed()
    }

    /// The in-place checkout prompt, while shown.
    #[must_use]
    pub fn checkout_prompt(&self) -> Option<&CheckoutPrompt> {
        self.notices.checkout()
    }

    /// The selector of the checkout prompt's focused button.
    #[must_use]
    pub fn checkout_focus(&self) -> Option<&'static str> {
        self.notices
            .checkout()
            .map(|prompt| prompt.focused().selector())
    }

    /// Whether a modal notice holds the keyboard.
    #[must_use]
    pub fn notice_focused(&self, window: &Window) -> bool {
        self.notice_focus.is_focused(window)
    }

    fn push_spawning_toast(&mut self, cx: &mut Context<Self>) {
        let detail = Some(SPAWNING_DETAIL.to_owned());
        self.push_toast(ToastKind::Info, SPAWNING_TITLE, detail, cx);
    }

    fn push_toast(
        &mut self,
        kind: ToastKind,
        title: &str,
        detail: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.notices.push(kind, title, detail, (self.now)());
        self.schedule_toast_expiry(cx);
        cx.notify();
    }

    fn dismiss_toast(&mut self, id: u64, cx: &mut Context<Self>) {
        if self.notices.dismiss(id) {
            self.schedule_toast_expiry(cx);
            cx.notify();
        }
    }

    /// Arms a timer for the next toast to go; replacing the timer cancels
    /// the one before.
    fn schedule_toast_expiry(&mut self, cx: &mut Context<Self>) {
        let now = (self.now)();
        self.toast_timer = self.notices.next_expiry().map(|deadline| {
            let delay = deadline.saturating_duration_since(now);
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(delay).await;
                // Fails only when the view is gone, and the toasts with it.
                this.update(cx, Self::expire_toasts).ok();
            })
        });
    }

    /// The timer fired: drop the toasts whose time is up by the clock, which
    /// may lag the timer, and wait for the next.
    fn expire_toasts(&mut self, cx: &mut Context<Self>) {
        if self.notices.expire((self.now)()) {
            cx.notify();
        }
        self.schedule_toast_expiry(cx);
    }

    /// A spawn or duplicate the daemon refused is no longer on the way.
    fn fail_request(&mut self, request_id: Option<&str>) {
        self.fail_duplicate(request_id);
        if let Some(id) = request_id {
            self.spawns.fail(id);
        }
    }

    /// `Error`: a toast, and the request it answers fails.
    pub(crate) fn on_daemon_error(
        &mut self,
        message: String,
        request_id: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        self.fail_request(request_id);
        self.push_toast(ToastKind::Error, DAEMON_ERROR_TITLE, Some(message), cx);
    }

    /// `ActionFailed`: the modal notice takes the keyboard, and the request
    /// it answers fails.
    pub(crate) fn on_action_failed(
        &mut self,
        notice: ActionFailedNotice,
        request_id: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.fail_request(request_id);
        let ActionFailedNotice {
            title,
            detail,
            hint,
        } = notice;
        self.notices.show_action_failed(title, detail, hint);
        self.notice_focus.focus(window);
        cx.notify();
    }

    /// `CheckoutConfirmRequired`: the prompt takes the keyboard, Cancel
    /// focused.
    pub(crate) fn on_checkout_confirm(
        &mut self,
        repo_id: String,
        branch: String,
        dirty_count: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let request_id = self.spawns.resolve_checkout(&repo_id, &branch);
        if request_id.is_none() {
            tracing::warn!(
                repo_id,
                branch,
                "checkout prompt matches no pending in-place spawn; its answer will send nothing"
            );
        }
        self.notices
            .ask_checkout(repo_id, branch, dirty_count, request_id);
        self.notice_focus.focus(window);
        cx.notify();
    }

    /// A session that answers one of this client's spawns goes where the
    /// spawn asked for it.
    pub(crate) fn place_spawn(
        &mut self,
        request_id: Option<&str>,
        session: &SessionSnapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(request_id) = request_id else {
            return;
        };
        let sessions = self.sidebar.sessions();
        let Some(placed) = self
            .spawns
            .place(request_id, session, &mut self.tabs, sessions)
        else {
            return;
        };
        self.confirm.disarm();
        self.send(placed.message);
        if placed.relayout {
            self.after_tabs_change(window, cx);
        }
    }

    /// Forgets every spawn and closes both modal notices, as a new or lost
    /// connection must.
    pub(crate) fn reset_notices(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.spawns.clear();
        if self.notices.close_modals() {
            self.after_notice_closed(window, cx);
        }
    }

    /// A key while a modal notice is open; returns whether one was. The
    /// action-failed notice closes on Esc, Enter or Space. The checkout
    /// prompt cancels on Esc, presses its focused button on Enter or Space,
    /// and moves the focus on Tab and Shift+Tab.
    pub(crate) fn on_notice_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let key = keystroke.key.as_str();
        if self.notices.action_failed().is_some() {
            if matches!(key, "escape" | "enter" | "space") {
                self.close_action_failed(window, cx);
            }
            return true;
        }
        let Some(prompt) = self.notices.checkout_mut() else {
            return false;
        };
        match key {
            "escape" => self.answer_checkout(CheckoutChoice::Cancel, window, cx),
            "enter" | "space" => {
                let choice = prompt.focused();
                self.answer_checkout(choice, window, cx);
            }
            "tab" => {
                prompt.move_focus(!keystroke.modifiers.shift);
                cx.notify();
            }
            _ => {}
        }
        true
    }

    fn close_action_failed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.notices.close_action_failed() {
            self.after_notice_closed(window, cx);
        }
    }

    /// The user's answer to the checkout prompt, for the spawn it resolved
    /// to: Cancel forgets that spawn; Stash or Carry resends it with that
    /// strategy and the same request id. A prompt that matched no spawn only
    /// closes.
    fn answer_checkout(
        &mut self,
        choice: CheckoutChoice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(prompt) = self.notices.close_checkout() else {
            return;
        };
        match (prompt.request_id, choice.strategy()) {
            (None, _) => {}
            (Some(request_id), None) => {
                self.spawns.cancel_checkout(&request_id);
            }
            (Some(request_id), Some(strategy)) => {
                if let Some(msg) = self.spawns.retry_checkout(&request_id, strategy) {
                    self.send(msg);
                    self.push_spawning_toast(cx);
                }
            }
        }
        self.after_notice_closed(window, cx);
    }

    /// The keyboard goes to the modal still open, else back to the spawn
    /// dialog's focused control, the delete-worktree confirm or the active
    /// pane.
    fn after_notice_closed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.notices.has_modal() {
            self.notice_focus.focus(window);
        } else if self.spawn_dialog.is_some() {
            self.apply_spawn_focus(window, cx);
        } else if self.delete_dialog.is_some() {
            self.dialog_focus.focus(window);
        } else {
            self.focus_active_pane(window, cx);
        }
        cx.notify();
    }

    /// The toasts, stacked in the bottom-right corner above the footer.
    pub(crate) fn toast_layer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let toasts = self.notices.toasts();
        if toasts.is_empty() {
            return None;
        }
        let cards: Vec<AnyElement> = toasts.iter().map(|toast| toast_card(toast, cx)).collect();
        Some(
            div()
                .absolute()
                .right(px(8.0))
                .bottom(px(FOOTER_HEIGHT + 8.0))
                .w(px(TOAST_WIDTH))
                .flex()
                .flex_col()
                .gap(px(6.0))
                .children(cards)
                .into_any_element(),
        )
    }

    /// The modal notices, the action-failed one on top; the top one holds
    /// the keyboard focus.
    pub(crate) fn notice_layers(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let failed = self.notices.action_failed();
        let checkout = self
            .notices
            .checkout()
            .map(|prompt| self.checkout_layer(prompt, failed.is_none(), cx));
        let failed = failed.map(|notice| self.action_failed_layer(notice, cx));
        checkout.into_iter().chain(failed).collect()
    }

    fn action_failed_layer(
        &self,
        notice: &ActionFailedNotice,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let ok = dialog_button("action-failed-dismiss", "OK".to_owned(), false, true).on_click(
            cx.listener(|this, _: &ClickEvent, window, cx| this.close_action_failed(window, cx)),
        );
        let detail = notice.detail.split('\n').map(|line| {
            // An empty line keeps its height.
            div().child(if line.is_empty() { " " } else { line }.to_owned())
        });
        let hint = notice
            .hint
            .clone()
            .map(|hint| div().text_color(gpui::rgb(MUTED)).child(hint));
        let panel = modal_panel("action-failed-panel")
            .track_focus(&self.notice_focus)
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(notice.title.clone()),
            )
            .child(div().flex().flex_col().children(detail))
            .children(hint)
            .child(div().flex().justify_end().child(ok));
        backdrop("action-failed-modal", panel)
    }

    fn checkout_layer(
        &self,
        prompt: &CheckoutPrompt,
        focused_layer: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let close = dialog_button("checkout-close", "✕".to_owned(), false, false).on_click(
            cx.listener(|this, _: &ClickEvent, window, cx| {
                this.answer_checkout(CheckoutChoice::Cancel, window, cx);
            }),
        );
        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(CHECKOUT_TITLE),
            )
            .child(close);
        let notes = prompt
            .choice_notes()
            .map(|note| div().text_color(gpui::rgb(MUTED)).child(note));
        let buttons: Vec<AnyElement> = CheckoutChoice::ALL
            .into_iter()
            .map(|choice| {
                dialog_button(
                    choice.selector(),
                    choice.label().to_owned(),
                    false,
                    prompt.focused() == choice,
                )
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.answer_checkout(choice, window, cx);
                }))
                .into_any_element()
            })
            .collect();
        let panel = modal_panel("checkout-panel")
            .when(focused_layer, |panel| panel.track_focus(&self.notice_focus))
            .child(header)
            .child(div().child(prompt.message()))
            .children(notes)
            .child(div().flex().justify_end().gap(px(6.0)).children(buttons));
        backdrop("checkout-confirm", panel)
    }
}

/// A toast: its title, its muted detail and its dismiss button; an error's
/// border is red.
fn toast_card(toast: &Toast, cx: &mut Context<RootView>) -> AnyElement {
    let id = toast.id;
    let close = dialog_button(&format!("toast-close-{id}"), "✕".to_owned(), false, false)
        .flex_none()
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.dismiss_toast(id, cx);
        }));
    let accent = match toast.kind {
        ToastKind::Error => DANGER,
        ToastKind::Info => BORDER,
    };
    let detail = toast
        .detail
        .clone()
        .map(|detail| div().text_color(gpui::rgb(MUTED)).child(detail));
    let body = div()
        .flex()
        .flex_col()
        .flex_1()
        .gap(px(2.0))
        .child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .child(toast.title.clone()),
        )
        .children(detail);
    let name = format!("toast-{id}");
    div()
        .id(ElementId::Name(SharedString::from(name.clone())))
        .debug_selector(|| name)
        .flex()
        .items_start()
        .gap(px(8.0))
        .p(px(10.0))
        .bg(gpui::rgb(PANEL_BG))
        .border_1()
        .border_color(gpui::rgb(accent))
        .rounded(px(6.0))
        .text_size(px(UI_TEXT_SIZE))
        .text_color(gpui::rgb(TEXT))
        .occlude()
        .child(body)
        .child(close)
        .into_any_element()
}

/// A modal's card.
fn modal_panel(id: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_col()
        .gap(px(10.0))
        .w(px(MODAL_WIDTH))
        .p(px(14.0))
        .bg(gpui::rgb(PANEL_BG))
        .border_1()
        .border_color(gpui::rgb(BORDER))
        .rounded(px(6.0))
        .text_size(px(UI_TEXT_SIZE))
        .text_color(gpui::rgb(TEXT))
}

/// `panel` centred over a backdrop that takes every click beneath it; a
/// click on the backdrop itself does nothing.
fn backdrop(selector: &'static str, panel: Stateful<Div>) -> AnyElement {
    div()
        .id(selector)
        .debug_selector(|| selector.to_owned())
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(gpui::rgba(BACKDROP_TINT))
        .occlude()
        .child(panel)
        .into_any_element()
}
