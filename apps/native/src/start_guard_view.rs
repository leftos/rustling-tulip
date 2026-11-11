//! A safe start on the app's side: what it does with This PC's first tab list,
//! the prompt that asks whether to open the tabs, and the timer that calls the
//! run stable. The model is [`crate::start_guard`].

use std::path::{Path, PathBuf};

use chrono::Utc;
use gpui::{AnyElement, App, ClickEvent, Context, FontWeight, Keystroke, Window, div, prelude::*};
use protocol::{ClientMessage, DaemonMessage, TabEntry};

use crate::RootView;
use crate::buttons::{ButtonKind, ButtonSize, Layer, button, button_look, focus_ring};
use crate::notice_view::modal_panel;
use crate::notices::ToastKind;
use crate::offscreen;
use crate::quit_view::QuitFn;
use crate::session_menu::{backdrop, dialog_button, dialog_look};
use crate::sidebar::UiState;
use crate::start_guard::{
    self, LAYOUT_COPY_PREFIX, PROMPT_OPEN, PROMPT_TITLE, PROMPT_WITHOUT, STARTED_WITHOUT_TITLE,
    StartMode, StartPrompt, UI_RESET_TITLE,
};
use crate::target::HostKey;
use crate::ui_theme::theme;

/// The prompt a start after a crashed one raises, and This PC's newest tab
/// list, held undrawn until the user chooses.
#[derive(Debug)]
pub(crate) struct HeldStart {
    prompt: StartPrompt,
    tabs: Vec<TabEntry>,
}

/// Drops what restores views from `ui`, keeping the fonts, the theme and every
/// other setting.
pub(crate) fn drop_restored_views(ui: &mut UiState) {
    let defaults = UiState::default();
    ui.active_tab_id = None;
    ui.popouts.clear();
    ui.session_views = defaults.session_views;
    ui.dashboard.view = defaults.dashboard.view;
}

/// `quit`, first forgetting the failed starts in `dir` and clearing the run's
/// mark: a clean quit means the start was good, and nothing crashed. A safe
/// start that has not acted yet keeps its count and marks the run as asking,
/// since the next start still has to carry it out.
pub(crate) fn stable_on_quit(quit: QuitFn, dir: Option<PathBuf>) -> QuitFn {
    Box::new(move |cx: &mut App| {
        if let Some(dir) = &dir {
            if !start_guard::safe_start_pending() {
                start_guard::mark_stable(dir);
            }
            start_guard::mark_clean_end(dir);
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
    /// Whether the prompt asking about the tabs is up.
    #[must_use]
    pub fn start_prompt_open(&self) -> bool {
        self.start_notice.is_some()
    }

    /// The lines the prompt shows under its title.
    #[must_use]
    pub fn start_prompt_lines(&self) -> Option<Vec<String>> {
        self.start_notice.as_ref().map(|held| held.prompt.lines())
    }

    /// This PC's tab messages, once they concern the start: a tab the safe
    /// start closed is never drawn, and a tab list arriving later has its
    /// closed tabs taken out and closed again. This PC's first tab list or
    /// layout question also starts the stable timer on a normal start; after
    /// a crashed start, a first list with tabs is used up before anything
    /// reads it and held while the prompt asks what to do with it, and the
    /// timer waits for the choice. A start whose host never connects keeps
    /// its mode and asks nothing.
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
        if self.start_notice.is_some() {
            return self.hold_tab_message(msg);
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
        if self.pending_safe_start == StartMode::Normal {
            self.start_stable_timer(cx);
            return false;
        }
        let mode = std::mem::take(&mut self.pending_safe_start);
        match msg {
            DaemonMessage::Tabs { tabs } if !tabs.is_empty() => {
                // The stable window waits for the choice, so a prompt left up
                // never counts the run stable.
                let tabs = std::mem::take(tabs);
                self.ask_about_tabs(&mode, tabs, window, cx);
                true
            }
            _ => {
                self.start_stable_timer(cx);
                self.start_with_no_tabs(&mode, cx);
                false
            }
        }
    }

    /// A This PC tab message while the prompt is up: nothing draws, and the
    /// newest tab list replaces the held one. A list is used up whole, so the
    /// tab model stays unloaded and no saved pop-out reopens before the
    /// choice. Returns whether `msg` is used up.
    fn hold_tab_message(&mut self, msg: &mut DaemonMessage) -> bool {
        let Some(held) = self.start_notice.as_mut() else {
            return false;
        };
        match msg {
            DaemonMessage::Tabs { tabs } => {
                held.tabs = std::mem::take(tabs);
                true
            }
            DaemonMessage::TabRemoved { tab_id } => {
                held.tabs.retain(|tab| tab.id != *tab_id);
                false
            }
            DaemonMessage::TabUpdated { .. } | DaemonMessage::TabsReordered { .. } => true,
            _ => false,
        }
    }

    /// Holds `tabs` undrawn and raises the prompt.
    fn ask_about_tabs(
        &mut self,
        mode: &StartMode,
        tabs: Vec<TabEntry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        tracing::warn!(?mode, "the last start crashed; asking about the tabs");
        self.start_notice = Some(HeldStart {
            prompt: StartPrompt::new(mode),
            tabs,
        });
        self.start_notice_focus.focus(window);
        cx.notify();
    }

    /// A start after a crashed one with no tabs to ask about: nothing to
    /// close, and a reset of the settings drops the restored views and is
    /// told in a toast.
    fn start_with_no_tabs(&mut self, mode: &StartMode, cx: &mut Context<Self>) {
        start_guard::set_safe_start_pending(false);
        tracing::warn!(?mode, "the last start crashed; no tabs to ask about");
        if matches!(mode, StartMode::AskAndResetUi { .. }) {
            self.sidebar.drop_restored_views();
            // The restored views are gone from the saved file too, or the
            // next normal start would bring them back.
            self.save_ui();
        }
        if let Some(copy) = mode.kept_ui() {
            let detail = start_guard::ui_reset_detail(copy);
            self.push_toast(ToastKind::Info, UI_RESET_TITLE, Some(detail), cx);
        }
        cx.notify();
    }

    /// Open tabs: the prompt closes, the stable window starts, and This PC is
    /// asked for its tab list again, which draws as normal with the saved
    /// views. Nothing is closed or saved.
    fn open_held_tabs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.start_notice.take().is_none() {
            return;
        }
        start_guard::set_safe_start_pending(false);
        tracing::info!("opening the tabs after a crashed start");
        self.start_stable_timer(cx);
        self.send_to(&HostKey::Local, ClientMessage::ListTabs);
        self.after_start_prompt(window, cx);
    }

    /// Start without tabs: the held tabs are saved to a dated copy and
    /// closed, the restored views go, the stable window starts, and This PC
    /// is asked for its tab list again so a tab made meanwhile draws.
    fn start_without_tabs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(held) = self.start_notice.take() else {
            return;
        };
        start_guard::set_safe_start_pending(false);
        tracing::warn!("starting without the tabs after a crashed start");
        self.start_stable_timer(cx);
        let copy = self.close_restored_tabs(&held.tabs);
        self.send_to(&HostKey::Local, ClientMessage::ListTabs);
        self.sidebar.drop_restored_views();
        // The restored views are gone from the saved file too, or the next
        // normal start would bring them back.
        self.save_ui();
        if let Some(copy) = copy {
            let detail = start_guard::started_without_detail(&copy);
            self.push_toast(ToastKind::Info, STARTED_WITHOUT_TITLE, Some(detail), cx);
        }
        self.after_start_prompt(window, cx);
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

    /// A key while the prompt is up; returns whether it was. Enter and Esc
    /// start without the tabs, and every other key does nothing.
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
            self.start_without_tabs(window, cx);
        }
        true
    }

    /// After the prompt closed: the keyboard goes back to what it covered.
    fn after_start_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.cleanup_failed.is_empty() {
            self.cleanup_focus.focus(window);
        } else if self.layout_chooser.is_some() {
            self.chooser_focus.focus(window);
        } else {
            self.after_notice_closed(window, cx);
        }
        cx.notify();
    }

    /// The prompt over a backdrop that takes every click beneath it.
    pub(crate) fn start_notice_layer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let held = self.start_notice.as_ref()?;
        let t = theme(cx);
        let title_weight = crate::fonts::ui_weight(cx, FontWeight::SEMIBOLD);
        let (offset, shift) = (
            crate::fonts::ui_text_size(cx),
            crate::fonts::ui_weight_shift(cx),
        );
        let open = dialog_button(
            "start-prompt-open",
            PROMPT_OPEN.to_owned(),
            dialog_look(offset, shift, false, true, &t),
            false,
        )
        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
            this.open_held_tabs(window, cx);
        }));
        let size = ButtonSize::Regular;
        let primary = button_look(
            offset,
            shift,
            ButtonKind::Primary,
            size,
            true,
            Layer::Floating,
            &t,
        );
        // Starting without the tabs is the safe answer, so it holds the focus.
        let without = button("start-prompt-without", primary)
            .child(focus_ring(size.radius(), &t))
            .child(PROMPT_WITHOUT)
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.start_without_tabs(window, cx);
            }));
        let panel = modal_panel(crate::fonts::ui_text_size(cx), "start-prompt-panel", &t)
            .track_focus(&self.start_notice_focus)
            .child(div().font_weight(title_weight).child(PROMPT_TITLE))
            .children(
                held.prompt
                    .lines()
                    .into_iter()
                    .map(|line| div().child(line)),
            )
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(open)
                    .child(without),
            );
        Some(backdrop("start-prompt", panel, &t))
    }
}
