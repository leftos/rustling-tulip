//! The Recover dialog on screen: its grouped rows with their checkboxes and
//! choices, its keys, the `RecoverSessions` request with its timeout, and
//! the placing of the sessions it recovered. The model is
//! [`crate::recover`].

use std::collections::HashSet;
use std::time::Duration;

use chrono::Local;
use gpui::{
    AnyElement, ClickEvent, Context, Div, FontWeight, Keystroke, Stateful, Window, div, prelude::*,
    px,
};
use protocol::{ClientMessage, DaemonMessage, RecoverItemResult, SessionSnapshot};

use crate::activity_bar::badge_text;
use crate::buttons::RING_ROOM;
use crate::notice_view::modal_panel;
use crate::palette::TRANSPARENT;
use crate::recover::{
    Activation, Control, FocusMove, Group, GroupKind, Names, RECOVER_TIMEOUT, RecoverDialog, Row,
    other_toggle_label,
};
use crate::session_menu::{backdrop, dialog_button};
use crate::{BORDER, MUTED, RootView, TEXT, WARNING, new_request_id, spawns};

const TITLE: &str = "Recover sessions";
/// The most of the window's height the rows take before they scroll.
const LIST_MAX_SHARE: f32 = 0.6;
/// How soon a timeout check whose clock lagged its timer looks again.
const TIMEOUT_RECHECK: Duration = Duration::from_secs(1);

/// A row of the Recover dialog as it reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoverRow {
    /// The history id.
    pub id: String,
    pub label: String,
    /// Its workspace, repo or folder.
    pub place: String,
    /// When and how it ended.
    pub ended: String,
    pub ticked: bool,
    /// Why it cannot be recovered, when it cannot.
    pub disabled: Option<String>,
    /// The chosen conversation's text, while the chosen way resumes one.
    pub conversation: Option<String>,
    /// How many conversations there are to choose from.
    pub conversations: usize,
    /// The chosen "Recover as" text, when the row offers the choice.
    pub recover_as: Option<String>,
    /// The last recovery's failure.
    pub error: Option<String>,
}

impl RootView {
    /// Whether the Recover dialog is open.
    #[must_use]
    pub fn recover_open(&self) -> bool {
        self.recover.is_some()
    }

    /// The rail's Recover sessions badge as drawn, when it shows.
    #[must_use]
    pub fn recover_badge(&self) -> Option<String> {
        badge_text(self.session_history.badge_count())
    }

    /// The open dialog's groups: each title with the row ids it shows (none
    /// while the other sessions are collapsed).
    #[must_use]
    pub fn recover_groups(&self) -> Vec<(String, Vec<String>)> {
        let Some(dialog) = &self.recover else {
            return Vec::new();
        };
        dialog
            .groups()
            .iter()
            .map(|group| {
                let rows = if group.kind == GroupKind::Other && !dialog.other_expanded() {
                    Vec::new()
                } else {
                    group.rows.clone()
                };
                (group.title(), rows)
            })
            .collect()
    }

    /// The open dialog's shown rows, in order.
    #[must_use]
    pub fn recover_rows(&self) -> Vec<RecoverRow> {
        let Some(dialog) = &self.recover else {
            return Vec::new();
        };
        dialog
            .visible_rows()
            .into_iter()
            .filter_map(|id| dialog.row(id))
            .map(|row| RecoverRow {
                id: row.id.clone(),
                label: row.label.clone(),
                place: row.place.clone(),
                ended: row.ended.clone(),
                ticked: dialog.is_ticked(&row.id),
                disabled: row.disabled.clone(),
                conversation: resumes(row).then(|| chosen_conversation(row)).flatten(),
                conversations: row.conversations.len(),
                recover_as: row
                    .shows_recover_as
                    .then(|| row.chosen().map(|o| o.label.clone()))
                    .flatten(),
                error: row.error.clone(),
            })
            .collect()
    }

    /// The open dialog's controls as drawn, in the keyboard's order (a
    /// disabled Recover button last): selector and label.
    #[must_use]
    pub fn recover_controls(&self) -> Vec<(String, String)> {
        let Some(dialog) = &self.recover else {
            return Vec::new();
        };
        let mut controls = dialog.focus_order();
        if !controls.contains(&Control::Recover) {
            controls.push(Control::Recover);
        }
        controls
            .iter()
            .map(|control| (selector(control), control_label(dialog, control)))
            .collect()
    }

    /// The selector of the open dialog's focused control.
    #[must_use]
    pub fn recover_focus(&self) -> Option<String> {
        self.recover
            .as_ref()
            .and_then(RecoverDialog::focus)
            .map(selector)
    }

    /// The Recover button's text and whether it can be pressed.
    #[must_use]
    pub fn recover_button(&self) -> Option<(String, bool)> {
        let dialog = self.recover.as_ref()?;
        Some((dialog.recover_label(), dialog.recover_enabled()))
    }

    /// The error line under the list, when one shows.
    #[must_use]
    pub fn recover_error(&self) -> Option<String> {
        self.recover
            .as_ref()
            .and_then(RecoverDialog::error)
            .map(str::to_owned)
    }

    /// The text shown instead of an empty list.
    #[must_use]
    pub fn recover_empty_text(&self) -> Option<&'static str> {
        self.recover.as_ref().and_then(RecoverDialog::empty_text)
    }

    /// The names the dialog shows for the registry's repos and workspaces.
    fn recover_names(&self) -> Names {
        Names {
            workspaces: self
                .sidebar
                .workspaces()
                .iter()
                .map(|w| (w.id.clone(), w.name.clone()))
                .collect(),
            repos: self
                .sidebar
                .repos()
                .iter()
                .map(|r| (r.id.clone(), r.name.clone()))
                .collect(),
        }
    }

    /// The rail button: opens the dialog, or closes it unless a recovery is
    /// on its way.
    pub(crate) fn toggle_recover(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.recover.is_some() {
            self.close_recover(window, cx);
            return;
        }
        let dialog = RecoverDialog::new(
            self.session_history.items(),
            &self.recover_names(),
            &Local::now(),
        );
        tracing::info!(
            entries = self.session_history.items().len(),
            "recover dialog: opened"
        );
        self.recover = Some(dialog);
        self.recover_focus.focus(window);
        cx.notify();
    }

    /// Closes the dialog, unless a recovery is on its way.
    fn close_recover(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.recover.as_ref().is_some_and(RecoverDialog::is_busy) {
            return;
        }
        self.drop_recover(window, cx);
    }

    /// Closes the dialog whatever it waits for; the keyboard goes back to
    /// what it covered.
    fn drop_recover(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.recover_timer = None;
        if self.recover.take().is_some() {
            self.after_notice_closed(window, cx);
        }
    }

    /// A new connection: the dialog closes, nothing waits to be placed, and
    /// the history is asked for afresh.
    pub(crate) fn reset_recover(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.recover_requests.clear();
        self.recover_batches.clear();
        self.drop_recover(window, cx);
        self.send(protocol::ClientMessage::ListSessionHistory {
            request_id: Some(new_request_id()),
        });
    }

    /// A lost connection: the dialog closes.
    pub(crate) fn close_recover_on_overlay(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.drop_recover(window, cx);
    }

    /// The history (a reply or a broadcast), a recovery's answer, and the
    /// snapshot of a recovered session still to be placed. Every other
    /// message is left alone.
    pub(crate) fn on_recover_message(
        &mut self,
        msg: &DaemonMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match msg {
            DaemonMessage::SessionHistory { .. } => {
                self.session_history.apply(msg);
                let names = self.recover_names();
                if let Some(dialog) = &mut self.recover {
                    dialog.refresh(self.session_history.items(), &names, &Local::now());
                }
                cx.notify();
            }
            DaemonMessage::RecoverResult {
                request_id,
                results,
            } => self.on_recover_result(request_id.as_deref(), results, window, cx),
            DaemonMessage::SessionUpdated { .. } if !self.recover_batches.is_empty() => {
                self.place_ready_batches();
            }
            _ => {}
        }
    }

    fn on_recover_result(
        &mut self,
        request_id: Option<&str>,
        results: &[RecoverItemResult],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(abandoned) = request_id.and_then(|id| self.recover_requests.remove(id)) {
            // The daemon hands an abandoned session's panes to its recovery
            // itself; only the rest are placed here.
            let batch: Vec<String> = results
                .iter()
                .filter(|r| !abandoned.contains(&r.history_id))
                .filter_map(|r| r.session_id.clone())
                .collect();
            if !batch.is_empty() {
                self.recover_batches.push(batch);
                self.place_ready_batches();
            }
        } else {
            tracing::info!("recover dialog: an answer to no request of this connection");
        }
        let Some(dialog) = &mut self.recover else {
            return;
        };
        let outcome = dialog.on_result(request_id, results);
        if !dialog.is_busy() {
            self.recover_timer = None;
        }
        let failed = results.iter().filter(|r| r.session_id.is_none()).count();
        tracing::info!(
            recovered = outcome.recovered_session_ids.len(),
            failed,
            "recover dialog: answered"
        );
        if outcome.close {
            self.drop_recover(window, cx);
        }
        cx.notify();
    }

    /// Places every batch whose sessions' snapshots have all arrived, each
    /// in one go so no two of a batch take the same pane; the rest wait.
    fn place_ready_batches(&mut self) {
        for batch in std::mem::take(&mut self.recover_batches) {
            let snapshots: Option<Vec<SessionSnapshot>> = batch
                .iter()
                .map(|id| self.sidebar.session(id).cloned())
                .collect();
            match snapshots {
                Some(snapshots) => self.place_recovered(&snapshots),
                None => self.recover_batches.push(batch),
            }
        }
    }

    /// Places `recovered` where normal spawns would go, one after another.
    fn place_recovered(&mut self, recovered: &[SessionSnapshot]) {
        if recovered.is_empty() {
            return;
        }
        let messages = spawns::place_several(recovered, &self.tabs, self.sidebar.sessions());
        tracing::info!(
            sessions = recovered.len(),
            requests = messages.len(),
            "recover dialog: placing the recovered sessions"
        );
        for msg in messages {
            self.send(msg);
        }
    }

    /// Sends the ticked rows' recovery and starts its timeout.
    fn submit_recover(&mut self, cx: &mut Context<Self>) {
        let request_id = new_request_id();
        let now = (self.now)();
        let Some(msg) = self
            .recover
            .as_mut()
            .and_then(|dialog| dialog.recover_message(&request_id, now))
        else {
            return;
        };
        tracing::info!(request = %request_id, "recover dialog: recovering");
        let abandoned: HashSet<String> = match &msg {
            ClientMessage::RecoverSessions { items, .. } => items
                .iter()
                .map(|item| item.history_id.clone())
                .filter(|id| self.sidebar.session(id).is_some_and(|s| s.is_abandoned))
                .collect(),
            _ => HashSet::new(),
        };
        self.recover_requests.insert(request_id, abandoned);
        self.send(msg);
        self.arm_recover_timeout(RECOVER_TIMEOUT, cx);
        cx.notify();
    }

    fn arm_recover_timeout(&mut self, delay: Duration, cx: &mut Context<Self>) {
        self.recover_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            // Fails only when the view is gone, and the dialog with it.
            this.update(cx, Self::check_recover_timeout).ok();
        }));
    }

    /// The timeout fired: the wait ends once the clock, which may lag the
    /// timer, says so.
    fn check_recover_timeout(&mut self, cx: &mut Context<Self>) {
        let now = (self.now)();
        let Some(dialog) = &mut self.recover else {
            return;
        };
        if dialog.check_timeout(now) {
            tracing::warn!("recover dialog: the recovery did not answer");
            self.recover_timer = None;
            cx.notify();
        } else if dialog.is_busy() {
            self.arm_recover_timeout(TIMEOUT_RECHECK, cx);
        }
    }

    /// A key while the dialog is open; returns whether it was. Up and Down
    /// move between rows, Tab and Shift+Tab across controls, Left and Right
    /// cycle a focused choice, Space presses the focused control, Enter
    /// recovers unless a button has the focus (then presses it), and Esc
    /// closes. While a recovery is on its way every key does nothing.
    pub(crate) fn on_recover_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(dialog) = &mut self.recover else {
            return false;
        };
        if dialog.is_busy() {
            return true;
        }
        if let Some(key) = focus_move(keystroke) {
            dialog.move_focus(key);
            cx.notify();
            return true;
        }
        let key = keystroke.key.as_str();
        let activation = match key {
            "left" | "right" => {
                cycle_focused(dialog, key == "right");
                Activation::Handled
            }
            "space" => dialog.activate(),
            "enter" => enter(dialog),
            "escape" => Activation::Cancel,
            _ => Activation::Handled,
        };
        self.apply_recover_activation(activation, window, cx);
        true
    }

    fn press_recover_control(
        &mut self,
        control: &Control,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(dialog) = &mut self.recover else {
            return;
        };
        dialog.set_focus(control.clone());
        let activation = dialog.activate();
        self.apply_recover_activation(activation, window, cx);
    }

    fn apply_recover_activation(
        &mut self,
        activation: Activation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match activation {
            Activation::Handled => cx.notify(),
            Activation::Cancel => self.close_recover(window, cx),
            Activation::Recover => self.submit_recover(cx),
        }
    }

    /// The dialog over a backdrop that takes every click beneath it.
    pub(crate) fn recover_layer(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let dialog = self.recover.as_ref()?;
        let groups: Vec<Div> = dialog
            .groups()
            .iter()
            .map(|group| group_block(dialog, group, cx))
            .collect();
        let list = div()
            .id("recover-list")
            .max_h(window.viewport_size().height * LIST_MAX_SHARE)
            .overflow_y_scroll()
            .p(px(RING_ROOM))
            .flex()
            .flex_col()
            .gap(px(10.0))
            .children(dialog.empty_text().map(muted))
            .children(groups);
        let footer = div()
            .flex()
            .justify_end()
            .gap(px(8.0))
            .child(control_button(dialog, Control::SelectAll, cx))
            .child(control_button(dialog, Control::Cancel, cx))
            .child(control_button(dialog, Control::Recover, cx));
        let panel = modal_panel("recover-panel")
            .track_focus(&self.recover_focus)
            .child(div().font_weight(FontWeight::SEMIBOLD).child(TITLE))
            .child(list)
            .children(dialog.error().map(|error| warning(error.to_owned())))
            .child(footer);
        Some(backdrop("recover", panel))
    }
}

/// The focus move a key asks for: Up and Down between rows, Tab and
/// Shift+Tab across controls.
fn focus_move(keystroke: &Keystroke) -> Option<FocusMove> {
    match keystroke.key.as_str() {
        "up" => Some(FocusMove::Up),
        "down" => Some(FocusMove::Down),
        "tab" if keystroke.modifiers.shift => Some(FocusMove::Prev),
        "tab" => Some(FocusMove::Next),
        _ => None,
    }
}

/// Enter presses a focused button, else recovers.
fn enter(dialog: &mut RecoverDialog) -> Activation {
    if on_button(dialog.focus()) {
        dialog.activate()
    } else {
        Activation::Recover
    }
}

/// Left or Right on a focused choice moves it back or on.
fn cycle_focused(dialog: &mut RecoverDialog, forward: bool) {
    match dialog.focus().cloned() {
        Some(Control::Conversation(id)) => dialog.cycle(&id, forward),
        Some(Control::RecoverAs(id)) => dialog.cycle_recover_as(&id, forward),
        _ => {}
    }
}

/// Whether `focus` is one of the dialog's buttons, which Enter presses.
fn on_button(focus: Option<&Control>) -> bool {
    matches!(
        focus,
        Some(Control::OtherToggle | Control::SelectAll | Control::Cancel | Control::Recover)
    )
}

/// Whether `row`'s chosen way resumes a conversation it has.
fn resumes(row: &Row) -> bool {
    !row.conversations.is_empty() && row.chosen().is_some_and(|o| o.resumes)
}

fn chosen_conversation(row: &Row) -> Option<String> {
    row.conversation
        .and_then(|index| row.conversations.get(index))
        .map(|(_, text)| text.clone())
}

/// The selector a control is drawn under.
fn selector(control: &Control) -> String {
    match control {
        Control::Row(id) => format!("recover-row-{id}"),
        Control::Conversation(id) => format!("recover-conversation-{id}"),
        Control::RecoverAs(id) => format!("recover-as-{id}"),
        Control::OtherToggle => "recover-other-toggle".to_owned(),
        Control::SelectAll => "recover-select-all".to_owned(),
        Control::Cancel => "recover-cancel".to_owned(),
        Control::Recover => "recover-submit".to_owned(),
    }
}

fn other_count(dialog: &RecoverDialog) -> usize {
    dialog
        .groups()
        .iter()
        .filter(|group| group.kind == GroupKind::Other)
        .map(|group| group.rows.len())
        .sum()
}

/// A control's text: a row's checkbox mark, a choice's current value, a
/// button's label.
fn control_label(dialog: &RecoverDialog, control: &Control) -> String {
    let row = |id: &str| dialog.row(id);
    match control {
        Control::Row(id) => if dialog.is_ticked(id) { "☑" } else { "☐" }.to_owned(),
        Control::Conversation(id) => row(id).and_then(chosen_conversation).unwrap_or_default(),
        Control::RecoverAs(id) => row(id)
            .and_then(Row::chosen)
            .map(|o| format!("Recover as: {}", o.label))
            .unwrap_or_default(),
        Control::OtherToggle => other_toggle_label(other_count(dialog), dialog.other_expanded()),
        Control::SelectAll => "Select all".to_owned(),
        Control::Cancel => "Cancel".to_owned(),
        Control::Recover => dialog.recover_label(),
    }
}

/// Whether `control` takes a press now: nothing while a recovery is on its
/// way, a disabled row never, and Recover only with rows ticked.
fn enabled(dialog: &RecoverDialog, control: &Control) -> bool {
    if dialog.is_busy() {
        return false;
    }
    match control {
        Control::Row(id) => dialog.row(id).is_some_and(|row| row.disabled.is_none()),
        Control::Recover => dialog.recover_enabled(),
        _ => true,
    }
}

/// A control as a button, ringed while focused; a disabled one is dimmed
/// and inert.
fn control_button(
    dialog: &RecoverDialog,
    control: Control,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    let button = dialog_button(
        &selector(&control),
        control_label(dialog, &control),
        false,
        dialog.focus() == Some(&control),
        enabled(dialog, &control),
    );
    if !enabled(dialog, &control) {
        return button;
    }
    button.on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
        this.press_recover_control(&control, window, cx);
    }))
}

/// A group: its title (the other sessions' behind their toggle) over its
/// shown rows.
fn group_block(dialog: &RecoverDialog, group: &Group, cx: &mut Context<RootView>) -> Div {
    let block = div().flex().flex_col().gap(px(6.0));
    if group.kind == GroupKind::Other {
        let toggle = control_button(dialog, Control::OtherToggle, cx);
        if !dialog.other_expanded() {
            return block.child(div().flex().child(toggle));
        }
        return block
            .child(div().flex().child(toggle))
            .children(group.rows.iter().map(|id| row_block(dialog, id, cx)));
    }
    block
        .child(div().font_weight(FontWeight::SEMIBOLD).child(group.title()))
        .children(group.rows.iter().map(|id| row_block(dialog, id, cx)))
}

/// One row: its checkbox, label and end over where it ran and its choices,
/// then why it cannot be recovered or why it failed.
fn row_block(dialog: &RecoverDialog, id: &str, cx: &mut Context<RootView>) -> Div {
    let Some(row) = dialog.row(id) else {
        return div();
    };
    let heading = div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .child(control_button(dialog, Control::Row(id.to_owned()), cx))
        .child(div().flex_1().min_w_0().child(row.label.clone()))
        .child(muted(row.ended.clone()));
    let mut details = div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap(px(8.0))
        .pl(px(28.0))
        .child(muted(row.place.clone()));
    if row.disabled.is_none() {
        if resumes(row) {
            details = details.child(conversation_choice(dialog, row, cx));
        }
        if row.shows_recover_as {
            details = details.child(control_button(
                dialog,
                Control::RecoverAs(id.to_owned()),
                cx,
            ));
        }
    }
    div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .p(px(6.0))
        .border_1()
        .border_color(gpui::rgb(BORDER))
        .rounded(px(4.0))
        .child(heading)
        .child(details)
        .children(
            row.disabled
                .clone()
                .map(|reason| muted(reason).pl(px(28.0))),
        )
        .children(row.error.clone().map(|error| warning(error).pl(px(28.0))))
}

/// The conversation a row resumes: a chip that cycles through several, or
/// plain text when there is one (outlined while it holds the focus).
fn conversation_choice(
    dialog: &RecoverDialog,
    row: &Row,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let control = Control::Conversation(row.id.clone());
    if row.conversations.len() > 1 {
        return control_button(dialog, control, cx).into_any_element();
    }
    let focused = dialog.focus() == Some(&control);
    let name = selector(&control);
    div()
        .debug_selector(|| name)
        .px(px(10.0))
        .py(px(4.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(if focused {
            gpui::rgb(TEXT)
        } else {
            gpui::rgba(TRANSPARENT)
        })
        .child(muted(chosen_conversation(row).unwrap_or_default()))
        .into_any_element()
}

fn muted(text: impl Into<gpui::SharedString>) -> Div {
    div().text_color(gpui::rgb(MUTED)).child(text.into())
}

fn warning(text: impl Into<gpui::SharedString>) -> Div {
    div().text_color(gpui::rgb(WARNING)).child(text.into())
}
