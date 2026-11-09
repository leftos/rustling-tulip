//! A safe start on the app's side: what it does with This PC's first tab list,
//! the timer that calls the run stable, and the notice's modal. The model is
//! [`crate::start_guard`].

use std::path::{Path, PathBuf};

use chrono::Utc;
use gpui::{AnyElement, App, ClickEvent, Context, FontWeight, Keystroke, Window, div, prelude::*};
use protocol::{ClientMessage, DaemonMessage, TabEntry};

use crate::RootView;
use crate::notice_view::modal_panel;
use crate::offscreen;
use crate::quit_view::QuitFn;
use crate::session_menu::{backdrop, dialog_button, dialog_look};
use crate::sidebar::UiState;
use crate::start_guard::{
    self, LAYOUT_COPY_PREFIX, NOTICE_BUTTON, NOTICE_TITLE, StartMode, StartNotice,
};
use crate::target::HostKey;
use crate::ui_theme::theme;

/// Drops what restores views from `ui`, keeping the fonts, the theme and every
/// other setting.
pub(crate) fn drop_restored_views(ui: &mut UiState) {
    let defaults = UiState::default();
    ui.active_tab_id = None;
    ui.popouts.clear();
    ui.session_views = defaults.session_views;
    ui.dashboard.view = defaults.dashboard.view;
}

/// `quit`, first forgetting the failed starts in `dir`: a clean quit means the
/// start was good. A safe start that has not acted yet keeps its count, since
/// the next start still has to carry it out.
pub(crate) fn stable_on_quit(quit: QuitFn, dir: Option<PathBuf>) -> QuitFn {
    Box::new(move |cx: &mut App| {
        if let Some(dir) = &dir
            && !start_guard::safe_start_pending()
        {
            start_guard::mark_stable(dir);
        }
        quit(cx);
    })
}

/// Saves `tabs` to a dated copy in `dir`, returning its path; a failure logs.
fn save_closed_tabs(dir: &Path, tabs: &[TabEntry]) -> Option<PathBuf> {
    let bytes = serde_json::to_vec_pretty(tabs)
        .inspect_err(|err| tracing::warn!("serializing the closed tabs: {err}"))
        .ok()?;
    start_guard::keep_bytes(dir, LAYOUT_COPY_PREFIX, &bytes, Utc::now())
        .inspect_err(|err| tracing::warn!("saving the closed tabs: {err:#}"))
        .ok()
}

impl RootView {
    /// Whether the safe-start notice is up.
    #[must_use]
    pub fn start_notice_open(&self) -> bool {
        self.start_notice.is_some()
    }

    /// The lines the safe-start notice shows under its title.
    #[must_use]
    pub fn start_notice_lines(&self) -> Option<Vec<String>> {
        self.start_notice.as_ref().map(StartNotice::lines)
    }

    /// This PC's tab messages, once they concern the start: a tab the safe
    /// start closed is never drawn, and a tab list arriving later has its
    /// closed tabs taken out and closed again. This PC's first tab list or
    /// layout question also starts the stable timer and ends a safe start: a
    /// tab list is saved, closed and replaced with an empty one before
    /// anything reads it, so no restored tab is ever drawn. A start whose host
    /// never connects keeps its mode and shows no notice.
    ///
    /// Returns whether `msg` is used up and must draw nothing.
    pub(crate) fn on_safe_start_message(
        &mut self,
        host: &HostKey,
        msg: &mut DaemonMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if *host != HostKey::Local {
            return false;
        }
        match msg {
            DaemonMessage::Tabs { tabs } => {
                for tab_id in self.closed_by_safe_start.filter_tabs(tabs) {
                    self.send_to(host, ClientMessage::CloseTab { tab_id });
                }
            }
            DaemonMessage::TabUpdated { tab } if self.closed_by_safe_start.holds(&tab.id) => {
                return true;
            }
            DaemonMessage::TabRemoved { tab_id } => self.closed_by_safe_start.forget(tab_id),
            _ => {}
        }
        if !matches!(
            msg,
            DaemonMessage::Tabs { .. } | DaemonMessage::LayoutInitRequired { .. }
        ) {
            return false;
        }
        self.start_stable_timer(cx);
        if self.pending_safe_start == StartMode::Normal {
            return false;
        }
        let mode = std::mem::take(&mut self.pending_safe_start);
        start_guard::set_safe_start_pending(false);
        let layout_copy = if let DaemonMessage::Tabs { tabs } = msg {
            self.close_restored_tabs(&std::mem::take(tabs))
        } else {
            None
        };
        tracing::warn!(?mode, "opened with the tabs closed");
        self.start_notice = Some(StartNotice::new(&mode, layout_copy));
        self.start_notice_focus.focus(window);
        // The restored views are gone from the saved file too, or the next
        // normal start would bring them back.
        self.save_ui();
        cx.notify();
        false
    }

    /// Saves `tabs` to a dated copy and asks This PC to close each. Their
    /// sessions keep running. A copy that cannot be saved logs, and they close
    /// all the same: the start comes first.
    fn close_restored_tabs(&mut self, tabs: &[TabEntry]) -> Option<PathBuf> {
        if tabs.is_empty() {
            return None;
        }
        let copy = self
            .ui_dir
            .as_deref()
            .and_then(|dir| save_closed_tabs(dir, tabs));
        for tab in tabs {
            self.closed_by_safe_start.note(tab.id.clone());
            self.send_to(
                &HostKey::Local,
                ClientMessage::CloseTab {
                    tab_id: tab.id.clone(),
                },
            );
        }
        copy
    }

    /// Spawns, once, the wait after which this run counts as stable.
    fn start_stable_timer(&mut self, cx: &mut Context<Self>) {
        if std::mem::replace(&mut self.stable_timer_started, true) {
            return;
        }
        // Only a run's first tab list re-arms the window; a reconnect must not make a late panic count.
        start_guard::tabs_arrived();
        let Some(dir) = self.ui_dir.clone() else {
            return;
        };
        let timer = cx
            .background_executor()
            .timer(start_guard::stable_after(offscreen::requested()));
        cx.spawn(async move |_, _| {
            timer.await;
            start_guard::mark_stable(&dir);
        })
        .detach();
    }

    /// A key while the notice is up; returns whether it was. Enter and Esc
    /// dismiss it, and every other key does nothing.
    pub(crate) fn on_start_notice_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.start_notice.is_none() {
            return false;
        }
        if matches!(keystroke.key.as_str(), "enter" | "escape") {
            self.dismiss_start_notice(window, cx);
        }
        true
    }

    /// Closes the notice; the keyboard goes back to what it covered.
    fn dismiss_start_notice(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.start_notice = None;
        if !self.cleanup_failed.is_empty() {
            self.cleanup_focus.focus(window);
        } else if self.layout_chooser.is_some() {
            self.chooser_focus.focus(window);
        } else {
            self.after_notice_closed(window, cx);
        }
        cx.notify();
    }

    /// The notice over a backdrop that takes every click beneath it.
    pub(crate) fn start_notice_layer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let notice = self.start_notice.as_ref()?;
        let t = theme(cx);
        let title_weight = crate::fonts::ui_weight(cx, FontWeight::SEMIBOLD);
        let ok = dialog_button(
            "start-notice-ok",
            NOTICE_BUTTON.to_owned(),
            dialog_look(
                crate::fonts::ui_text_size(cx),
                crate::fonts::ui_weight_shift(cx),
                false,
                true,
                &t,
            ),
            true,
        )
        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
            this.dismiss_start_notice(window, cx);
        }));
        let panel = modal_panel(crate::fonts::ui_text_size(cx), "start-notice-panel", &t)
            .track_focus(&self.start_notice_focus)
            .child(div().font_weight(title_weight).child(NOTICE_TITLE))
            .children(notice.lines().into_iter().map(|line| div().child(line)))
            .child(div().flex().justify_end().child(ok));
        Some(backdrop("start-notice", panel, &t))
    }
}
