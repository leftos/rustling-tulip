//! Spawning, the toasts, the action-failed notice and the checkout prompt on
//! screen: rendering, keys and clicks, forwarded to [`crate::notices`] and
//! [`crate::spawns`].

use crate::fonts::{ui_px, ui_px_at};
use gpui::{
    AnyElement, BoxShadow, ClickEvent, Context, Div, ElementId, FontWeight, Keystroke,
    SharedString, Stateful, Window, WindowId, div, point, prelude::*, px, relative,
};
use protocol::{ClientMessage, SessionSnapshot, SpawnRequest, SpawnTarget};

use crate::buttons::{ButtonSize, Layer, outlined_button};
use crate::notices::{
    ActionFailedNotice, CLOSE_SESSION, CheckoutAsk, CheckoutChoice, CheckoutPrompt, Toast,
    ToastKind, ToastSpec, agent_exit_session_of, spawning_key, toast_action_selector,
};
use crate::preset_launch::job_id_of;
use crate::session_menu::{backdrop, dialog_button, dialog_look};
use crate::spawns::OpenIn;
use crate::target::HostKey;
use crate::ui_theme::{UiTheme, theme};
use crate::{FOOTER_HEIGHT, RootView, UI_TEXT_SIZE};

const TOAST_WIDTH: f32 = 320.0;
const MODAL_WIDTH: f32 = 440.0;
/// The corner radius of a modal's card.
pub(crate) const MODAL_RADIUS: f32 = 14.0;
/// The corner radius of a toast's card.
const TOAST_RADIUS: f32 = 10.0;
/// The side of a toast's and an undo entry's square close button.
pub(crate) const CLOSE_SIZE: f32 = 28.0;
const SPAWNING_TITLE: &str = "Spawning session…";
const SPAWNING_DETAIL: &str = "Worktree creation may take a few seconds.";
const SHELL_SPAWNING_DETAIL: &str = "Shell startup may take a few seconds.";
const DAEMON_ERROR_TITLE: &str = "Daemon error";

impl RootView {
    /// Asks the daemon for a new session and places it by `open_in` when
    /// its reply arrives, under the request id it went out with. A
    /// "Spawning session…" toast shows meanwhile: it clears when the daemon
    /// answers or the spawn otherwise ends, and else expires or is pushed
    /// out as any other timed toast is.
    pub fn spawn(
        &mut self,
        request: SpawnRequest,
        open_in: OpenIn,
        cx: &mut Context<Self>,
    ) -> String {
        let request_id = crate::new_request_id();
        let detail = spawning_detail(&request);
        let msg = self.spawns.start(request, request_id.clone(), open_in);
        self.send_to(&self.host_of_request(&msg), msg);
        self.push_spawning_toast(detail, &request_id, cx);
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

    /// The "Spawning session…" toast of spawn `request_id`, keyed by it so a
    /// resend updates it in place and the daemon's answer clears it.
    fn push_spawning_toast(&mut self, detail: &str, request_id: &str, cx: &mut Context<Self>) {
        let spec = ToastSpec {
            kind: ToastKind::Info,
            title: SPAWNING_TITLE.to_owned(),
            detail: Some(detail.to_owned()),
            key: Some(spawning_key(request_id)),
            sticky: false,
            actions: Vec::new(),
        };
        self.push_toast_spec(spec, cx);
    }

    /// The daemon answered spawn `request_id`: its toast goes.
    fn clear_spawning_toast(&mut self, request_id: &str, cx: &mut Context<Self>) {
        if self.notices.dismiss_key(&spawning_key(request_id)) {
            self.forget_closed_jobs();
            self.schedule_toast_expiry(cx);
            cx.notify();
        }
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

    pub(crate) fn push_toast(
        &mut self,
        kind: ToastKind,
        title: &str,
        detail: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.notices.push(kind, title, detail, (self.now)());
        self.place_toasts();
        self.forget_closed_jobs();
        self.schedule_toast_expiry(cx);
        cx.notify();
    }

    /// Shows `spec`, updating the toast its key matches in place; returns the
    /// toast's id.
    pub fn push_toast_spec(&mut self, spec: ToastSpec, cx: &mut Context<Self>) -> u64 {
        let id = self.notices.push_spec(spec, (self.now)());
        self.place_toasts();
        self.forget_closed_jobs();
        self.schedule_toast_expiry(cx);
        cx.notify();
        id
    }

    /// A new toast shows in the window the user last pressed a mouse
    /// button or a key in, else the main one; a shown one stays put.
    fn place_toasts(&mut self) {
        self.overlays
            .place_toasts(self.notices.toasts().iter().map(|toast| toast.id));
    }

    /// Drops the ended launch jobs whose toast went: closed, timed out or
    /// pushed out by a newer one.
    fn forget_closed_jobs(&mut self) {
        let toasts = self.notices.toasts();
        self.preset_launches
            .retain_shown(|key| toasts.iter().any(|toast| toast.key.as_deref() == Some(key)));
    }

    /// Button `action` on toast `id`: an agent-exit toast's closes its
    /// session, a preset job's goes to its registry.
    fn press_toast_action(
        &mut self,
        id: u64,
        action: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = self
            .notices
            .toasts()
            .iter()
            .find(|toast| toast.id == id)
            .and_then(|toast| toast.key.clone());
        let Some(key) = key else {
            return;
        };
        if let Some(session_id) = agent_exit_session_of(&key) {
            if action == CLOSE_SESSION.id {
                self.close_session_after_agent_exit(session_id);
                self.dismiss_toast(id, cx);
            }
            return;
        }
        if job_id_of(&key).is_some() {
            self.press_preset_toast_action(&key, action, window, cx);
        }
    }

    /// The × on toast `id`; a preset job's progress toast stays closed.
    fn dismiss_toast(&mut self, id: u64, cx: &mut Context<Self>) {
        let key = self
            .notices
            .toasts()
            .iter()
            .find(|toast| toast.id == id)
            .and_then(|toast| toast.key.clone());
        if let Some(key) = key {
            self.preset_launches.dismiss(&key);
        }
        if self.notices.dismiss(id) {
            self.forget_closed_jobs();
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
            self.forget_closed_jobs();
            cx.notify();
        }
        self.schedule_toast_expiry(cx);
    }

    /// A spawn or duplicate the daemon refused is no longer on the way, nor
    /// is its spawning toast or any checkout prompt for it, and a new tab it
    /// refused activates nothing when another tab arrives. A refused split,
    /// move or extract is no longer in flight, and a tab its send marked
    /// closing is not.
    fn fail_request(
        &mut self,
        request_id: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.fail_duplicate(request_id);
        if let Some(id) = request_id {
            self.tabs.disarm_create(id);
            self.tabs.fail_layout_request(id);
            self.tabs.fail_reorder(id);
            self.end_spawn(id, cx);
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
        self.pump_checkout(cx);
        if held && !self.notices.has_modal() {
            self.after_notice_closed(window, cx);
        }
    }

    /// When no checkout prompt is shown and none is being asked again, the
    /// oldest waiting prompt's spawn is sent again, unchanged, so the daemon
    /// answers with a freshly measured prompt or with the session itself. A
    /// waiting spawn that cannot be asked again ends, its toast with it. No
    /// new toast: the spawn showed one when it started.
    fn pump_checkout(&mut self, cx: &mut Context<Self>) {
        let spawns = &mut self.spawns;
        let mut dropped = Vec::new();
        let msg = self.notices.pump(|id| {
            let msg = spawns.reask(id);
            if msg.is_none() {
                dropped.push(id.to_owned());
            }
            msg
        });
        for request_id in dropped {
            self.end_spawn(&request_id, cx);
        }
        if let Some(msg) = msg {
            self.send_to(&self.host_of_request(&msg), msg);
        }
    }

    /// Spawn `request_id` ends without a session: it is no longer pending,
    /// and its spawning toast goes.
    fn end_spawn(&mut self, request_id: &str, cx: &mut Context<Self>) {
        self.spawns.fail(request_id);
        self.clear_spawning_toast(request_id, cx);
    }

    /// `Error`: a toast, and the request it answers fails.
    pub(crate) fn on_daemon_error(
        &mut self,
        message: String,
        request_id: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.refuse_appearance(request_id, &message, cx) {
            return;
        }
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
        if self.refuse_appearance(request_id, &notice.detail, cx) {
            return;
        }
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

    /// A session that answers one of this client's spawns clears its
    /// spawning toast and goes where the spawn asked for it.
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
        self.clear_spawning_toast(request_id, cx);
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
            self.tabs.note_layout_request(&msg);
            self.send_to(&HostKey::Local, msg);
        }
        self.settle_checkout(request_id, window, cx);
        if placed.relayout {
            self.after_tabs_change(window, cx);
        }
    }

    /// Forgets every spawn and its spawning toast and closes both modal
    /// notices, as a new or lost connection must.
    pub(crate) fn reset_notices(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.spawns.clear();
        self.quick_shell_saves.clear();
        if self.notices.dismiss_spawning() {
            self.forget_closed_jobs();
            self.schedule_toast_expiry(cx);
            cx.notify();
        }
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
                self.end_spawn(&request_id, cx);
            }
            (Some(request_id), strategy @ Some(_)) => {
                if let Some(msg) = self.spawns.resend(&request_id, strategy) {
                    let detail = spawn_detail_of(&msg);
                    self.send_to(&self.host_of_request(&msg), msg);
                    self.push_spawning_toast(detail, &request_id, cx);
                }
            }
        }
        self.pump_checkout(cx);
        self.after_notice_closed(window, cx);
    }

    /// The keyboard goes to the modal still open, else back to the spawn
    /// dialog's focused control, the Themes modal, Settings, the
    /// delete-worktree confirm, the layout chooser or the import modal, or
    /// the active pane.
    pub(crate) fn after_notice_closed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.notices.has_modal() {
            self.notice_focus.focus(window);
        } else if self.spawn_dialog.is_some() {
            self.apply_spawn_focus(window, cx);
        } else if self.shell_dialog.is_some() {
            self.apply_shell_focus(window, cx);
        } else if self.fetch_dialog.is_some() {
            self.apply_fetch_focus(window, cx);
        } else if self.themes_modal.is_some() {
            self.themes_focus.focus(window);
        } else if self.appearance_editor.is_some() {
            self.appearance_focus.focus(window);
        } else if self.delete_dialog.is_some() {
            self.dialog_focus.focus(window);
        } else if self.layout_chooser.is_some() || self.import_modal.is_some() {
            self.chooser_focus.focus(window);
        } else {
            self.focus_active_pane(window, cx);
        }
        cx.notify();
    }

    /// Window `here`'s toasts, stacked in its bottom-right corner, above the
    /// footer in the main window.
    pub(crate) fn toast_layer(&self, here: WindowId, cx: &mut Context<Self>) -> Option<AnyElement> {
        let toasts: Vec<&Toast> = self
            .notices
            .toasts()
            .iter()
            .filter(|toast| self.overlays.toast_owner(toast.id) == here)
            .collect();
        if toasts.is_empty() {
            return None;
        }
        let bottom = if here == self.layouts.main() {
            ui_px(cx, FOOTER_HEIGHT) + px(8.0)
        } else {
            px(8.0)
        };
        let cards: Vec<AnyElement> = toasts
            .into_iter()
            .map(|toast| {
                let progress = toast
                    .key
                    .as_deref()
                    .and_then(|key| self.preset_launches.progress(key));
                toast_card(toast, progress, cx)
            })
            .collect();
        Some(
            div()
                .absolute()
                .right(px(8.0))
                .bottom(bottom)
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
        let t = theme(cx);
        let ok = dialog_button(
            "action-failed-dismiss",
            "OK".to_owned(),
            dialog_look(
                crate::fonts::ui_text_size(cx),
                crate::fonts::ui_weight_shift(cx),
                false,
                true,
                &theme(cx),
            ),
            true,
        )
        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
            this.close_action_failed(window, cx);
        }));
        let detail = notice.detail.split('\n').map(|line| {
            // An empty line keeps its height.
            div().child(if line.is_empty() { " " } else { line }.to_owned())
        });
        let hint = notice
            .hint
            .clone()
            .map(|hint| div().text_color(gpui::rgb(t.muted)).child(hint));
        let panel = modal_panel(crate::fonts::ui_text_size(cx), "action-failed-panel", &t)
            .track_focus(&self.notice_focus)
            .child(
                div()
                    .font_weight(crate::fonts::ui_weight(cx, FontWeight::SEMIBOLD))
                    .child(notice.title.clone()),
            )
            .child(div().flex().flex_col().children(detail))
            .children(hint)
            .child(div().flex().justify_end().child(ok));
        backdrop("action-failed-modal", panel, &theme(cx))
    }

    fn checkout_layer(
        &self,
        prompt: &CheckoutPrompt,
        title: &str,
        focused_layer: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = theme(cx);
        let close = dialog_button(
            "checkout-close",
            "✕".to_owned(),
            dialog_look(
                crate::fonts::ui_text_size(cx),
                crate::fonts::ui_weight_shift(cx),
                false,
                true,
                &theme(cx),
            ),
            false,
        )
        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
            this.answer_checkout(CheckoutChoice::Cancel, window, cx);
        }));
        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .child(
                div()
                    .font_weight(crate::fonts::ui_weight(cx, FontWeight::SEMIBOLD))
                    .child(title.to_owned()),
            )
            .child(close);
        let notes = prompt
            .choice_notes()
            .map(|note| div().text_color(gpui::rgb(t.muted)).child(note));
        let buttons: Vec<AnyElement> = CheckoutChoice::ALL
            .into_iter()
            .map(|choice| {
                dialog_button(
                    choice.selector(),
                    choice.label().to_owned(),
                    dialog_look(
                        crate::fonts::ui_text_size(cx),
                        crate::fonts::ui_weight_shift(cx),
                        false,
                        true,
                        &theme(cx),
                    ),
                    prompt.focused() == choice,
                )
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.answer_checkout(choice, window, cx);
                }))
                .into_any_element()
            })
            .collect();
        let panel = modal_panel(crate::fonts::ui_text_size(cx), "checkout-panel", &t)
            .when(focused_layer, |panel| panel.track_focus(&self.notice_focus))
            .child(header)
            .child(div().child(prompt.message()))
            .children(notes)
            .child(div().flex().justify_end().gap(px(6.0)).children(buttons));
        backdrop("checkout-confirm", panel, &theme(cx))
    }
}

/// A toast: its title, its muted detail, a thin bar under them when it
/// reports `progress` (0 to 1), its action buttons, and its dismiss button;
/// an error's border is red.
fn toast_card(toast: &Toast, progress: Option<f32>, cx: &mut Context<RootView>) -> AnyElement {
    let t = theme(cx);
    let id = toast.id;
    let close = outlined_button(
        crate::fonts::ui_text_size(cx),
        crate::fonts::ui_weight_shift(cx),
        &format!("toast-close-{id}"),
        ButtonSize::Compact,
        true,
        Layer::Floating,
        &t,
    )
    .flex_none()
    .w(px(CLOSE_SIZE))
    .px(px(0.0))
    .child("✕")
    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
        this.dismiss_toast(id, cx);
    }));
    let accent = match toast.kind {
        ToastKind::Error => Some(t.danger),
        ToastKind::Warning => Some(t.warning),
        ToastKind::Info => None,
    };
    let detail = toast
        .detail
        .clone()
        .map(|detail| div().text_color(gpui::rgb(t.muted)).child(detail));
    let body = div()
        .flex()
        .flex_col()
        .flex_1()
        .gap(px(2.0))
        .child(
            div()
                .font_weight(crate::fonts::ui_weight(cx, FontWeight::SEMIBOLD))
                .child(toast.title.clone()),
        )
        .children(detail)
        .children(progress.map(|fraction| progress_bar(id, fraction, &t)))
        .children(toast_actions(toast, cx));
    let name = format!("toast-{id}");
    toast_frame(
        crate::fonts::ui_text_size(cx),
        ElementId::Name(SharedString::from(name.clone())),
        &t,
    )
    .debug_selector(|| name)
    .flex()
    .items_start()
    .gap(px(8.0))
    .when_some(accent, |card, accent| card.border_color(gpui::rgb(accent)))
    .occlude()
    .child(body)
    .child(close)
    .into_any_element()
}

/// A thin bar in `t` filled `fraction` (0 to 1) of its width, under toast
/// `id`'s detail.
fn progress_bar(id: u64, fraction: f32, t: &UiTheme) -> Div {
    let selector = format!("toast-progress-{id}");
    div()
        .debug_selector(move || selector)
        .mt(px(4.0))
        .h(px(3.0))
        .w_full()
        .rounded(px(2.0))
        .bg(gpui::rgb(t.line_strong))
        .child(
            div()
                .debug_selector(move || format!("toast-progress-fill-{id}"))
                .h_full()
                .rounded(px(2.0))
                .bg(gpui::rgb(t.accent))
                .w(relative(fraction.clamp(0.0, 1.0))),
        )
}

/// Toast `toast`'s action buttons in a row, a destructive one's label red;
/// None without actions.
fn toast_actions(toast: &Toast, cx: &mut Context<RootView>) -> Option<Div> {
    if toast.actions.is_empty() {
        return None;
    }
    let t = theme(cx);
    let id = toast.id;
    let buttons = toast.actions.iter().map(|action| {
        let action_id = action.id;
        outlined_button(
            crate::fonts::ui_text_size(cx),
            crate::fonts::ui_weight_shift(cx),
            &toast_action_selector(id, action_id),
            ButtonSize::Compact,
            true,
            Layer::Floating,
            &t,
        )
        .when(action.danger, |button| {
            button.text_color(gpui::rgb(t.danger_text))
        })
        .child(action.label)
        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
            this.press_toast_action(id, action_id, window, cx);
        }))
    });
    Some(div().flex().gap(px(6.0)).mt(px(6.0)).children(buttons))
}

/// The card a toast, an undo entry and the copied chip sit in, tagged `id`:
/// the popover fill with its edge in `t`, which an error or a warning
/// recolours.
pub(crate) fn toast_frame(offset: i8, id: impl Into<ElementId>, t: &UiTheme) -> Stateful<Div> {
    div()
        .id(id)
        .px(px(14.0))
        .py(px(12.0))
        .bg(gpui::rgb(t.popover))
        .border_1()
        .border_color(gpui::rgb(t.popover_edge))
        .rounded(px(TOAST_RADIUS))
        .shadow(vec![drop_shadow(t.toast_shadow, 18.0, 40.0)])
        .text_size(ui_px_at(offset, UI_TEXT_SIZE))
        .text_color(gpui::rgb(t.text))
}

/// A soft shadow of `color` (`0xRRGGBBAA`) cast `drop` px down and blurred
/// `blur` px.
pub(crate) fn drop_shadow(color: u32, drop: f32, blur: f32) -> BoxShadow {
    BoxShadow {
        color: gpui::rgba(color).into(),
        offset: point(px(0.0), px(drop)),
        blur_radius: px(blur),
        spread_radius: px(0.0),
    }
}

/// A modal's card in `t`.
pub(crate) fn modal_panel(offset: i8, id: &'static str, t: &UiTheme) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_col()
        .gap(px(10.0))
        .w(px(MODAL_WIDTH))
        .p(px(14.0))
        .bg(gpui::rgb(t.modal))
        .border_1()
        .border_color(gpui::rgb(t.modal_edge))
        .rounded(px(MODAL_RADIUS))
        .shadow(vec![drop_shadow(t.dialog_shadow, 30.0, 80.0)])
        .text_size(ui_px_at(offset, UI_TEXT_SIZE))
        .text_color(gpui::rgb(t.text))
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
