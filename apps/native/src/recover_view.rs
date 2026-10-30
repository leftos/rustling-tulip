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
use crate::recover::{
    Activation, Control, FocusMove, Group, GroupKind, HostHistory, Names, RECOVER_TIMEOUT,
    RecoverDialog, Row, other_toggle_label,
};
use crate::session_menu::{backdrop, dialog_button, dialog_look};
use crate::target::HostKey;
use crate::ui_theme::{UiTheme, theme};
use crate::{RootView, new_request_id, spawns};

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

    /// The host names drawn over each host's rows, in order; none while
    /// only one host lists rows.
    #[must_use]
    pub fn recover_host_headers(&self) -> Vec<String> {
        let Some(dialog) = &self.recover else {
            return Vec::new();
        };
        dialog
            .groups()
            .iter()
            .filter_map(|group| group.header.clone())
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

    /// The names the dialog shows for the registry's repos and workspaces,
    /// with `host`'s agents.
    fn recover_names(&self, host: &HostKey) -> Names {
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
            agents: self.sidebar.agents(host).to_vec(),
        }
    }

    /// The name the dialog gives `host`: "This PC", else the remote host's
    /// short name.
    fn recover_host_label(&self, host: &HostKey) -> String {
        match host {
            HostKey::Local => "This PC".to_owned(),
            HostKey::Remote(key) => self
                .conn_of(host)
                .and_then(|conn| conn.target().short_name())
                .unwrap_or(key)
                .to_owned(),
        }
    }

    /// Every host the window holds, in host order, with its name and the
    /// names its rows use.
    fn recover_sources(&self) -> Vec<(HostKey, String, Names)> {
        self.hosts
            .keys()
            .into_iter()
            .map(|host| {
                let label = self.recover_host_label(&host);
                let names = self.recover_names(&host);
                (host, label, names)
            })
            .collect()
    }

    /// Each of `sources`' histories as the dialog lists it.
    fn host_histories<'a>(
        &'a self,
        sources: &'a [(HostKey, String, Names)],
    ) -> Vec<HostHistory<'a>> {
        sources
            .iter()
            .map(|(host, label, names)| HostHistory {
                host: host.clone(),
                label: label.clone(),
                items: self.session_history.items(host),
                names,
            })
            .collect()
    }

    /// Hands the open dialog every host's history as it now stands.
    pub(crate) fn refresh_recover(&mut self) {
        let Some(mut dialog) = self.recover.take() else {
            return;
        };
        let sources = self.recover_sources();
        dialog.refresh_over(&self.host_histories(&sources), &Local::now());
        self.recover = Some(dialog);
    }

    /// The rail button: opens the dialog, or closes it unless a recovery is
    /// on its way. With more than one host, each connected host is asked
    /// for its history afresh.
    pub(crate) fn toggle_recover(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.recover.is_some() {
            self.close_recover(window, cx);
            return;
        }
        let sources = self.recover_sources();
        let dialog = RecoverDialog::over(&self.host_histories(&sources), &Local::now());
        tracing::info!(
            hosts = sources.len(),
            entries = dialog.groups().iter().map(|g| g.rows.len()).sum::<usize>(),
            "recover dialog: opened"
        );
        self.recover = Some(dialog);
        if sources.len() > 1 {
            for (host, _, _) in &sources {
                if self.conn_of(host).is_some_and(crate::Connection::is_open) {
                    self.ask_history(host);
                }
            }
        }
        self.recover_focus.focus(window);
        cx.notify();
    }

    /// Asks `host`'s daemon for its session history.
    pub(crate) fn ask_history(&self, host: &HostKey) {
        self.send_to(
            host,
            ClientMessage::ListSessionHistory {
                request_id: Some(new_request_id()),
            },
        );
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
        self.ask_history(&HostKey::Local);
    }

    /// A lost connection: the dialog closes.
    pub(crate) fn close_recover_on_overlay(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.drop_recover(window, cx);
    }

    /// `host`'s history (a reply or a broadcast), a recovery's answer, and
    /// the snapshot of a recovered session still to be placed. Every other
    /// message is left alone.
    pub(crate) fn on_recover_message(
        &mut self,
        host: &HostKey,
        msg: &DaemonMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match msg {
            DaemonMessage::SessionHistory { .. } => {
                self.session_history.apply(host, msg);
                self.refresh_recover();
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
        let messages = spawns::place_several(recovered, &self.tabs, self.sidebar.local_sessions());
        tracing::info!(
            sessions = recovered.len(),
            requests = messages.len(),
            "recover dialog: placing the recovered sessions"
        );
        for msg in messages {
            self.send_to(&HostKey::Local, msg);
        }
    }

    /// Sends the ticked rows' recovery, one request to each row's host, and
    /// starts its timeout.
    fn submit_recover(&mut self, cx: &mut Context<Self>) {
        let now = (self.now)();
        let messages = self
            .recover
            .as_mut()
            .map(|dialog| dialog.recover_messages(new_request_id, now))
            .unwrap_or_default();
        if messages.is_empty() {
            return;
        }
        for (host, msg) in messages {
            if let ClientMessage::RecoverSessions {
                request_id: Some(request_id),
                items,
            } = &msg
            {
                tracing::info!(request = %request_id, ?host, "recover dialog: recovering");
                let abandoned: HashSet<String> = items
                    .iter()
                    .map(|item| item.history_id.clone())
                    .filter(|id| self.sidebar.session(id).is_some_and(|s| s.is_abandoned))
                    .collect();
                self.recover_requests.insert(request_id.clone(), abandoned);
            }
            self.send_to(&host, msg);
        }
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
        let t = theme(cx);
        let dialog = self.recover.as_ref()?;
        let mut toggle_drawn = false;
        let groups: Vec<Div> = dialog
            .groups()
            .iter()
            .map(|group| {
                let with_toggle = group.kind == GroupKind::Other && !toggle_drawn;
                toggle_drawn |= with_toggle;
                group_block(dialog, group, with_toggle, cx)
            })
            .collect();
        let list = div()
            .id("recover-list")
            .max_h(window.viewport_size().height * LIST_MAX_SHARE)
            .overflow_y_scroll()
            .p(px(RING_ROOM))
            .flex()
            .flex_col()
            .gap(px(10.0))
            .children(dialog.empty_text().map(|text| muted(text, &t)))
            .children(groups);
        let footer = div()
            .flex()
            .justify_end()
            .gap(px(8.0))
            .child(control_button(dialog, Control::SelectAll, cx))
            .child(control_button(dialog, Control::Cancel, cx))
            .child(control_button(dialog, Control::Recover, cx));
        let panel = modal_panel(crate::fonts::ui_text_size(cx), "recover-panel", &theme(cx))
            .track_focus(&self.recover_focus)
            .child(div().font_weight(FontWeight::SEMIBOLD).child(TITLE))
            .child(list)
            .children(dialog.error().map(|error| warning(error.to_owned(), &t)))
            .child(footer);
        Some(backdrop("recover", panel, &theme(cx)))
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
        dialog_look(
            crate::fonts::ui_text_size(cx),
            false,
            enabled(dialog, &control),
            &theme(cx),
        ),
        dialog.focus() == Some(&control),
    );
    if !enabled(dialog, &control) {
        return button;
    }
    button.on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
        this.press_recover_control(&control, window, cx);
    }))
}

/// A group: its host's name when it heads the host's rows, then its title
/// (the other sessions' behind the one toggle, drawn `with_toggle`) over
/// its shown rows.
fn group_block(
    dialog: &RecoverDialog,
    group: &Group,
    with_toggle: bool,
    cx: &mut Context<RootView>,
) -> Div {
    let block = div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .children(group.header.clone().map(muted));
    if group.kind == GroupKind::Other {
        let block = if with_toggle {
            block.child(
                div()
                    .flex()
                    .child(control_button(dialog, Control::OtherToggle, cx)),
            )
        } else {
            block
        };
        if !dialog.other_expanded() {
            return block;
        }
        return block.children(group.rows.iter().map(|id| row_block(dialog, id, cx)));
    }
    block
        .child(div().font_weight(FontWeight::SEMIBOLD).child(group.title()))
        .children(group.rows.iter().map(|id| row_block(dialog, id, cx)))
}

/// One row: its checkbox, label and end over where it ran and its choices,
/// then why it cannot be recovered or why it failed.
fn row_block(dialog: &RecoverDialog, id: &str, cx: &mut Context<RootView>) -> Div {
    let t = theme(cx);
    let Some(row) = dialog.row(id) else {
        return div();
    };
    let heading = div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .child(control_button(dialog, Control::Row(id.to_owned()), cx))
        .child(div().flex_1().min_w_0().child(row.label.clone()))
        .child(muted(row.ended.clone(), &t));
    let mut details = div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap(px(8.0))
        .pl(px(28.0))
        .child(muted(row.place.clone(), &t));
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
        .border_color(gpui::rgb(t.border))
        .rounded(px(4.0))
        .child(heading)
        .child(details)
        .children(
            row.disabled
                .clone()
                .map(|reason| muted(reason, &t).pl(px(28.0))),
        )
        .children(
            row.error
                .clone()
                .map(|error| warning(error, &t).pl(px(28.0))),
        )
}

/// The conversation a row resumes: a chip that cycles through several, or
/// plain text when there is one (outlined while it holds the focus).
fn conversation_choice(
    dialog: &RecoverDialog,
    row: &Row,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let t = theme(cx);
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
            gpui::rgb(t.text)
        } else {
            gpui::rgba(t.transparent)
        })
        .child(muted(chosen_conversation(row).unwrap_or_default(), &t))
        .into_any_element()
}

fn muted(text: impl Into<gpui::SharedString>, t: &UiTheme) -> Div {
    div().text_color(gpui::rgb(t.muted)).child(text.into())
}

fn warning(text: impl Into<gpui::SharedString>, t: &UiTheme) -> Div {
    div().text_color(gpui::rgb(t.warning)).child(text.into())
}
