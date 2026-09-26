//! Spawning, the toasts, the action-failed notice and the checkout prompt on
//! screen: rendering, keys and clicks, forwarded to [`crate::notices`] and
//! [`crate::spawns`].

use gpui::{
    AnyElement, ClickEvent, Context, Div, ElementId, FontWeight, Keystroke, SharedString, Stateful,
    Window, div, prelude::*, px,
};
use protocol::{ClientMessage, SessionSnapshot, SpawnRequest, SpawnTarget};

use crate::notices::{
    ActionFailedNotice, CheckoutAsk, CheckoutChoice, CheckoutPrompt, Toast, ToastKind,
};
use crate::session_menu::{backdrop, dialog_button};
use crate::spawns::OpenIn;
use crate::{BORDER, DANGER, FOOTER_HEIGHT, MUTED, PANEL_BG, RootView, TEXT, UI_TEXT_SIZE};

const TOAST_WIDTH: f32 = 320.0;
const MODAL_WIDTH: f32 = 440.0;
const SPAWNING_TITLE: &str = "Spawning session…";
const SPAWNING_DETAIL: &str = "Worktree creation may take a few seconds.";
const SHELL_SPAWNING_DETAIL: &str = "Shell startup may take a few seconds.";
const DAEMON_ERROR_TITLE: &str = "Daemon error";

impl RootView {
    /// Asks the daemon for a new session and places it by `open_in` when
    /// its reply arrives, under the request id it went out with. A
    /// "Spawning session…" toast shows meanwhile.
    pub fn spawn(
        &mut self,
        request: SpawnRequest,
        open_in: OpenIn,
        cx: &mut Context<Self>,
    ) -> String {
        let request_id = crate::new_request_id();
        let detail = spawning_detail(&request);
        let msg = self.spawns.start(request, request_id.clone(), open_in);
        self.send(msg);
        self.push_spawning_toast(detail, cx);
        request_id
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

    /// The checkout prompt's heading, counting the prompts waiting behind it.
    #[must_use]
    pub fn checkout_title(&self) -> Option<String> {
        self.notices.checkout_title()
    }

    /// Whether a modal notice holds the keyboard.
    #[must_use]
    pub fn notice_focused(&self, window: &Window) -> bool {
        self.notice_focus.is_focused(window)
    }

    fn push_spawning_toast(&mut self, detail: &str, cx: &mut Context<Self>) {
        let detail = Some(detail.to_owned());
        self.push_toast(ToastKind::Info, SPAWNING_TITLE, detail, cx);
    }

    /// A spawn that landed: the quick-shell folder it came with, if one was
    /// waiting on it, becomes the remembered one.
    fn remember_quick_shell(&mut self, request_id: &str) {
        if let Some(folder) = self.quick_shell_saves.succeed(request_id)
            && self.sidebar.set_quick_shell_dir(Some(&folder))
        {
            self.save_ui();
        }
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

    /// A spawn or duplicate the daemon refused is no longer on the way, nor
    /// is any checkout prompt for it.
    fn fail_request(
        &mut self,
        request_id: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.fail_duplicate(request_id);
        if let Some(id) = request_id {
            self.spawns.fail(id);
            self.quick_shell_saves.fail(id);
            self.settle_checkout(id, window, cx);
        }
    }

    /// The spawn `request_id` was placed or failed: its prompt waiting, or
    /// its re-ask holding the slot, goes, and the next prompt gets its turn.
    /// A re-ask that held the keyboard with nothing to follow it hands the
    /// keyboard back.
    fn settle_checkout(&mut self, request_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let held = self.notices.reasking();
        self.notices.spawn_settled(request_id);
        self.pump_checkout();
        if held && !self.notices.has_modal() {
            self.after_notice_closed(window, cx);
        }
    }

    /// When no checkout prompt is shown and none is being asked again, the
    /// oldest waiting prompt's spawn is sent again, unchanged, so the daemon
    /// answers with a freshly measured prompt or with the session itself. A
    /// waiting spawn that cannot be asked again is forgotten. No toast: the
    /// spawn showed one when it started.
    fn pump_checkout(&mut self) {
        let spawns = &mut self.spawns;
        if let Some(msg) = self.notices.pump(|id| spawns.reask(id)) {
            self.send(msg);
        }
    }

    /// `Error`: a toast, and the request it answers fails.
    pub(crate) fn on_daemon_error(
        &mut self,
        message: String,
        request_id: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.fail_request(request_id, window, cx);
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
        self.fail_request(request_id, window, cx);
        let ActionFailedNotice {
            title,
            detail,
            hint,
        } = notice;
        self.notices.show_action_failed(title, detail, hint);
        self.notice_focus.focus(window);
        cx.notify();
    }

    /// `CheckoutConfirmRequired`, resolved to the pending spawn its
    /// `request_id` names (by repo and branch when an older daemon sent
    /// none). Shown at once, holding the keyboard with Cancel focused, when
    /// no prompt is shown and none is being asked again, or when it answers
    /// the spawn being asked again; otherwise it waits its turn, to be asked
    /// again then.
    pub(crate) fn on_checkout_confirm(
        &mut self,
        mut ask: CheckoutAsk,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let sent_id = ask.request_id.take();
        ask.request_id =
            self.spawns
                .resolve_checkout(sent_id.as_deref(), &ask.repo_id, &ask.branch);
        let unresolved = ask
            .request_id
            .is_none()
            .then(|| (ask.repo_id.clone(), ask.branch.clone()));
        let shown = self.notices.ask_checkout(ask);
        if let Some((repo_id, branch)) = unresolved {
            let request_id = sent_id.as_deref();
            if shown {
                tracing::warn!(
                    repo_id,
                    branch,
                    request_id,
                    "checkout prompt matches no pending in-place spawn; its answer will send nothing"
                );
            } else {
                tracing::warn!(
                    repo_id,
                    branch,
                    request_id,
                    "checkout prompt dropped: no pending spawn and another prompt is shown"
                );
            }
        }
        if shown {
            self.notice_focus.focus(window);
        }
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
        self.remember_quick_shell(request_id);
        self.confirm.disarm();
        for msg in placed.messages {
            self.send(msg);
        }
        self.settle_checkout(request_id, window, cx);
        if placed.relayout {
            self.after_tabs_change(window, cx);
        }
    }

    /// Forgets every spawn and closes both modal notices, as a new or lost
    /// connection must.
    pub(crate) fn reset_notices(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.spawns.clear();
        self.quick_shell_saves.clear();
        if self.notices.close_modals() {
            self.after_notice_closed(window, cx);
        }
    }

    /// A key while a modal notice is open or a re-ask holds the checkout
    /// prompt's place; returns whether either was. The
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
            // A re-ask holds the prompt's place: every key, Esc included,
            // goes nowhere until its answer comes back.
            return self.notices.reasking();
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

    /// The user's answer to the shown checkout prompt, for the spawn it
    /// resolved to: Cancel forgets that spawn; Stash or Carry resends it with
    /// that strategy and the same request id. A prompt that matched no spawn
    /// only closes. The prompt waiting next, if any, is not shown from the
    /// numbers the daemon measured earlier: its spawn is asked again.
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
                self.spawns.fail(&request_id);
            }
            (Some(request_id), strategy @ Some(_)) => {
                if let Some(msg) = self.spawns.resend(&request_id, strategy) {
                    let detail = spawn_detail_of(&msg);
                    self.send(msg);
                    self.push_spawning_toast(detail, cx);
                }
            }
        }
        self.pump_checkout();
        self.after_notice_closed(window, cx);
    }

    /// The keyboard goes to the modal still open, else back to the spawn
    /// dialog's focused control, the delete-worktree confirm or the active
    /// pane.
    pub(crate) fn after_notice_closed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.notices.has_modal() {
            self.notice_focus.focus(window);
        } else if self.spawn_dialog.is_some() {
            self.apply_spawn_focus(window, cx);
        } else if self.shell_dialog.is_some() {
            self.apply_shell_focus(window, cx);
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
            .zip(self.notices.checkout_title())
            .map(|(prompt, title)| self.checkout_layer(prompt, &title, failed.is_none(), cx));
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
        title: &str,
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
                    .child(title.to_owned()),
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
pub(crate) fn modal_panel(id: &'static str) -> Stateful<Div> {
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

/// What the "Spawning session…" toast says under its title, which is what
/// the spawn's target makes the wait: a shell starts sooner than a worktree.
fn spawning_detail(request: &SpawnRequest) -> &'static str {
    match request.target {
        SpawnTarget::Standalone { .. } => SHELL_SPAWNING_DETAIL,
        SpawnTarget::Single { .. } | SpawnTarget::Workspace { .. } => SPAWNING_DETAIL,
    }
}

/// What the toast says for spawn `msg`, going out again as it first did.
fn spawn_detail_of(msg: &ClientMessage) -> &'static str {
    match msg {
        ClientMessage::SpawnSession(request) => spawning_detail(request),
        // No other message a spawn goes out as; a worktree is the safe wait
        // to name.
        _ => SPAWNING_DETAIL,
    }
}
