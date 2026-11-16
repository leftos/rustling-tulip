//! The tab-close dialog: its modal, its keys, and what each answer sends.
//! The model is [`crate::tab_close`].

use gpui::{
    AnyElement, ClickEvent, Context, FontWeight, Keystroke, Window, WindowId, div, prelude::*, px,
};

use crate::RootView;
use crate::connection::Connection;
use crate::driver_id::DriverId as _;
use crate::notice_view::modal_panel;
use crate::session_menu::{backdrop, dialog_button, dialog_look};
use protocol::TabEntry;

use crate::tab_close::{Control, KeyResult, LiveSession, Scope, TabClose, live_sessions_in};
use crate::target::HostKey;
use crate::ui_theme::theme;

impl RootView {
    /// Whether the tab-close dialog is open.
    #[must_use]
    pub fn tab_close_open(&self) -> bool {
        self.tab_close.is_some()
    }

    /// The open dialog's buttons in the keyboard's order: selector and
    /// label.
    #[must_use]
    pub fn tab_close_controls(&self) -> Vec<(String, String)> {
        let Some(dialog) = &self.tab_close else {
            return Vec::new();
        };
        TabClose::controls()
            .into_iter()
            .map(|control| (control.selector().to_owned(), dialog.label(control)))
            .collect()
    }

    /// The selector of the open dialog's focused button.
    #[must_use]
    pub fn tab_close_focus(&self) -> Option<String> {
        self.tab_close
            .as_ref()
            .map(|dialog| dialog.focused().selector().to_owned())
    }

    /// The open dialog's title and the line under it.
    #[must_use]
    pub fn tab_close_texts(&self) -> Option<(String, String)> {
        self.tab_close
            .as_ref()
            .map(|dialog| (dialog.title().to_owned(), dialog.body()))
    }

    /// The live sessions of `tab_id` (or, by `scope`, of every other tab) as
    /// they stand now, by the sidebar's statuses and the hosts'
    /// connections, with the first one's label as the sidebar shows it;
    /// none when the daemon no longer lists `tab_id`.
    fn tab_live_sessions(&self, scope: Scope, tab_id: &str) -> (Vec<LiveSession>, String) {
        let Some(tab) = self.tabs.tab(tab_id) else {
            return (Vec::new(), String::new());
        };
        let tabs: Vec<&TabEntry> = match scope {
            Scope::Tab => vec![tab],
            Scope::OtherTabs => self.tabs.tabs().iter().filter(|t| t.id != tab_id).collect(),
        };
        let status_of =
            |host: &HostKey, id: &str| self.sidebar.session_on(host, id).map(|s| s.status);
        let host_open = |host: &HostKey| self.conn_of(host).is_some_and(Connection::is_open);
        let sessions = live_sessions_in(&tabs, &status_of, &host_open);
        let label = sessions
            .first()
            .map(|session| self.session_label(&session.session_id))
            .unwrap_or_default();
        (sessions, label)
    }

    /// Opens the dialog closing `tab_id` (or, by `scope`, every other tab)
    /// when what it closes shows a live session; returns whether it
    /// opened. The tab menu gives way to it.
    pub(crate) fn open_tab_close(
        &mut self,
        scope: Scope,
        tab_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let (sessions, label) = self.tab_live_sessions(scope, tab_id);
        let Some(dialog) = TabClose::new(scope, tab_id, sessions, label) else {
            return false;
        };
        self.tabs.close_confirm.disarm();
        self.close_tab_menu(window, cx);
        self.tab_close = Some(dialog);
        self.tab_close_dialog_focus.focus(window, cx);
        cx.notify();
        true
    }

    /// A key while the dialog is open; returns whether it was taken, which
    /// every key is while the dialog is open.
    pub(crate) fn on_tab_close_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(dialog) = &mut self.tab_close else {
            return false;
        };
        match dialog.key(keystroke.key.as_str(), keystroke.modifiers.shift) {
            KeyResult::Moved => cx.notify(),
            KeyResult::Press(control) => self.press_tab_close(control, window, cx),
            KeyResult::Ignored => {}
        }
        true
    }

    /// Presses `control`: Cancel and ✕ close the dialog, Keep running
    /// closes what the dialog closes, and Stop stops each session live
    /// there at this moment on its host before it closes them (with none
    /// left it closes nothing). The keyboard goes where a closed dialog
    /// sends it.
    fn press_tab_close(&mut self, control: Control, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut dialog) = self.tab_close.take() else {
            return;
        };
        match control {
            Control::Cancel | Control::Dismiss => {}
            Control::Keep => {
                tracing::info!(
                    tab = %dialog.tab_id(),
                    scope = ?dialog.scope(),
                    "closing, the sessions keep running"
                );
                self.close_by_scope(&dialog, None, cx);
            }
            Control::Stop => self.stop_and_close_tab(&mut dialog, cx),
        }
        self.after_notice_closed(window, cx);
    }

    /// Stop's answer: recomputes `dialog`'s live sessions, stops exactly
    /// those, each on its host, then closes what it closes.
    fn stop_and_close_tab(&mut self, dialog: &mut TabClose, cx: &mut Context<Self>) {
        let (sessions, label) = self.tab_live_sessions(dialog.scope(), dialog.tab_id());
        if !dialog.refresh(sessions, label) {
            tracing::info!(tab = %dialog.tab_id(), "no live session is left to stop; closing nothing");
            return;
        }
        tracing::info!(
            tab = %dialog.tab_id(),
            scope = ?dialog.scope(),
            sessions = dialog.count(),
            "stopping the sessions and closing"
        );
        for (host, msg) in dialog.stop_messages() {
            self.send_to(&host, msg);
        }
        self.close_by_scope(dialog, Some(dialog.count()), cx);
    }

    /// Closes `dialog`'s tab, its undo entry telling of the `stopped`
    /// sessions, or every other tab, with no undo as Close other tabs has
    /// none.
    fn close_by_scope(
        &mut self,
        dialog: &TabClose,
        stopped: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        match dialog.scope() {
            Scope::Tab => self.close_tab_now(dialog.tab_id(), stopped, cx),
            Scope::OtherTabs => self.close_other_tabs_now(dialog.tab_id()),
        }
    }

    /// Recomputes the open dialog's live sessions after a tab, session or
    /// connection change, so its body and Stop label follow the count; with
    /// none left (or its tab gone) it closes without an answer. Returns
    /// whether it closed.
    pub(crate) fn refresh_tab_close(&mut self) -> bool {
        let Some((scope, tab_id)) = self
            .tab_close
            .as_ref()
            .map(|d| (d.scope(), d.tab_id().to_owned()))
        else {
            return false;
        };
        let (sessions, label) = self.tab_live_sessions(scope, &tab_id);
        let kept = self
            .tab_close
            .as_mut()
            .is_some_and(|dialog| dialog.refresh(sessions, label));
        if !kept {
            tracing::info!(tab = %tab_id, "the tab shows no live session any more; closing its dialog");
            self.tab_close = None;
        }
        !kept
    }

    /// [`Self::refresh_tab_close`] after a network event, handing the
    /// keyboard on when the dialog closed.
    pub(crate) fn refresh_tab_close_after_event(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.refresh_tab_close() {
            self.after_notice_closed(window, cx);
        }
    }

    /// The main window's close dialogs: the pane-close dialog while window
    /// `here` draws it, then the tab-close dialog.
    pub(crate) fn close_dialog_layers(
        &self,
        here: WindowId,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let pane = self.pane_close_layer(here, cx);
        pane.into_iter().chain(self.tab_close_layer(cx)).collect()
    }

    /// The dialog over a backdrop that takes every click beneath it. Only
    /// the main window, whose strip asked, draws it.
    fn tab_close_layer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let dialog = self.tab_close.as_ref()?;
        let mut button = |control: Control| {
            dialog_button(
                control.selector(),
                dialog.label(control),
                dialog_look(
                    crate::fonts::ui_text_size(cx),
                    crate::fonts::ui_weight_shift(cx),
                    control.danger(),
                    true,
                    &theme(cx),
                ),
                control == dialog.focused(),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.press_tab_close(control, window, cx);
            }))
        };
        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .child(
                div()
                    .font_weight(crate::fonts::ui_weight(cx, FontWeight::SEMIBOLD))
                    .child(dialog.title()),
            )
            .child(button(Control::Dismiss));
        let footer: Vec<_> = TabClose::controls()
            .into_iter()
            .filter(|control| *control != Control::Dismiss)
            .map(&mut button)
            .collect();
        let panel = modal_panel(
            crate::fonts::ui_text_size(cx),
            "tab-close-panel",
            &theme(cx),
        )
        .driver_id(|| "tab-close-panel".to_owned())
        .track_focus(&self.tab_close_dialog_focus)
        .child(header)
        .child(div().child(dialog.body()))
        .child(
            div()
                .flex()
                .flex_wrap()
                .justify_end()
                .gap(px(8.0))
                .children(footer),
        );
        Some(backdrop("tab-close", panel, &theme(cx)))
    }
}
