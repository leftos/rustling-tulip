//! The quit flow on screen: the main window's close request, the exit
//! dialog with its keys and clicks, the branch-fate walk through the
//! delete-worktree confirm, and the wait for the daemon's answer, forwarded
//! to [`crate::quit`].

use gpui::{
    AnyElement, App, ClickEvent, Context, FontWeight, Keystroke, Task, Window, div, prelude::*, px,
};
use protocol::{BranchCleanup, ClientMessage};
use std::collections::HashMap;

use crate::connection::State;
use crate::net::NetCommand;
use crate::notice_view::modal_panel;
use crate::quit::{BODY, Counts, ExitButton, ExitDialog, TITLE, Walk, covered, removal_messages};
use crate::session_menu::backdrop;
use crate::session_menu::{ConfirmOwner, dialog_button};
use crate::{DANGER, MUTED, RootView};

/// Quits the app: `cx.quit()` in the binary, a recorder in the specs.
pub type QuitFn = Box<dyn Fn(&mut App)>;

/// The app's quit, asked for at most once.
pub(crate) struct Quitter {
    quit: QuitFn,
    requested: bool,
}

impl Quitter {
    pub(crate) fn new(quit: QuitFn) -> Self {
        Self {
            quit,
            requested: false,
        }
    }

    /// The app has asked to quit; the window may close.
    fn requested(&self) -> bool {
        self.requested
    }

    /// Quits the app, the first time only.
    fn request(&mut self, cx: &mut App) {
        if !self.requested {
            self.requested = true;
            (self.quit)(cx);
        }
    }
}

/// The open exit dialog.
pub(crate) struct ExitView {
    model: ExitDialog,
    /// Wakes the dialog when the wait for the daemon gives up; replacing or
    /// dropping it cancels the wake-up.
    timer: Option<Task<()>>,
}

impl RootView {
    /// Whether the exit dialog is open.
    #[must_use]
    pub fn exit_dialog_open(&self) -> bool {
        self.exit.is_some()
    }

    /// The exit dialog's text: title, body, the active count, the orphan
    /// and worktree notes when they apply, and the stuck warning.
    #[must_use]
    pub fn exit_dialog_text(&self) -> Vec<String> {
        let Some(exit) = &self.exit else {
            return Vec::new();
        };
        let counts = self.exit_counts();
        let mut lines = vec![TITLE.to_owned(), BODY.to_owned(), counts.active_line()];
        lines.extend(counts.orphan_note());
        lines.extend(counts.worktree_note());
        lines.extend(exit.model.warning());
        lines
    }

    /// The exit dialog's footer: selector, label and whether it takes a
    /// press.
    #[must_use]
    pub fn exit_dialog_buttons(&self) -> Vec<(String, String, bool)> {
        let Some(exit) = &self.exit else {
            return Vec::new();
        };
        let counts = self.exit_counts();
        let connected = self.conn.is_open();
        exit.model
            .buttons(counts)
            .into_iter()
            .map(|button| {
                (
                    button.selector().to_owned(),
                    exit.model.label(button, counts),
                    exit.model.enabled(button, connected),
                )
            })
            .collect()
    }

    /// The selector of the exit dialog's focused button; `None` while no
    /// button takes a press.
    #[must_use]
    pub fn exit_dialog_focus(&self) -> Option<String> {
        let exit = self.exit.as_ref()?;
        let focused = exit
            .model
            .focused(self.exit_counts(), self.conn.is_open())?;
        Some(focused.selector().to_owned())
    }

    fn exit_counts(&self) -> Counts {
        Counts::of(self.sidebar.sessions())
    }

    /// The main window asked to close. With nothing the daemon could stop,
    /// or no connection or session list to judge by, the app quits at once;
    /// otherwise the exit dialog opens and the window stays. Returns whether
    /// the window may close.
    pub(crate) fn on_close_requested(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.quitter.requested() {
            return true;
        }
        if self.exit.is_some() {
            return false;
        }
        let active = self.exit_counts().active;
        let connected = self.conn.is_open();
        if !connected || !self.sessions_loaded || active == 0 {
            tracing::info!(
                connected,
                loaded = self.sessions_loaded,
                active,
                "window close: nothing to ask about; quitting"
            );
            self.request_quit(cx);
            return true;
        }
        tracing::info!(active, "window close: asking what to do with the sessions");
        self.close_session_menu(window, cx);
        self.close_shell_menu(window, cx);
        self.close_tab_menu(window, cx);
        // The menu's delete confirm and the spawn and Shell… dialogs have
        // sent nothing yet, so they can go.
        self.close_delete_dialog(window, cx);
        self.close_spawn_dialog(window, cx);
        self.close_shell_dialog(window, cx);
        self.close_flyout();
        self.exit = Some(ExitView {
            model: ExitDialog::new(),
            timer: None,
        });
        self.quit_focus.focus(window);
        cx.notify();
        false
    }

    /// Quits the app, once, after writing the layout change a font step
    /// left pending.
    fn request_quit(&mut self, cx: &mut Context<Self>) {
        self.flush_font_save();
        self.quitter.request(cx);
    }

    /// Cancel: the dialog closes and the keyboard goes back where it was.
    fn close_exit_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        tracing::info!("exit dialog: cancelled");
        self.exit = None;
        self.after_notice_closed(window, cx);
    }

    fn press_exit_button(
        &mut self,
        button: ExitButton,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let connected = self.conn.is_open();
        if !self
            .exit
            .as_ref()
            .is_some_and(|exit| exit.model.enabled(button, connected))
        {
            return;
        }
        match button {
            ExitButton::Cancel => self.close_exit_dialog(window, cx),
            ExitButton::KeepRunning => {
                tracing::info!("exit dialog: keep sessions running in background");
                self.request_quit(cx);
            }
            ExitButton::ForceQuit => {
                tracing::warn!("exit dialog: force quit (the daemon did not answer)");
                self.request_quit(cx);
            }
            ExitButton::StopKeep => self.begin_shutdown(button, true, Vec::new(), cx),
            ExitButton::Abandon => self.begin_shutdown(button, false, Vec::new(), cx),
            ExitButton::StopRemove => self.start_exit_walk(window, cx),
        }
        cx.notify();
    }

    /// Hands `before` and the shutdown to the network thread as one command
    /// and waits for the daemon.
    fn begin_shutdown(
        &mut self,
        pressed: ExitButton,
        drain: bool,
        before: Vec<ClientMessage>,
        cx: &mut Context<Self>,
    ) {
        tracing::info!(
            ?pressed,
            drain,
            before = before.len(),
            "exit dialog: shutting the daemon down"
        );
        self.command(NetCommand::Shutdown { before, drain });
        let now = (self.now)();
        if let Some(exit) = &mut self.exit {
            exit.model.start_stopping(pressed, now);
        }
        self.schedule_exit_tick(cx);
        cx.notify();
    }

    /// Arms a timer for the rest of the wait for the daemon, or disarms it
    /// once the wait is over.
    fn schedule_exit_tick(&mut self, cx: &mut Context<Self>) {
        let now = (self.now)();
        let Some(exit) = &mut self.exit else {
            return;
        };
        exit.timer = exit.model.deadline().map(|deadline| {
            let delay = deadline.saturating_duration_since(now);
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(delay).await;
                // Fails only when the view is gone, and the dialog with it.
                this.update(cx, Self::tick_exit).ok();
            })
        });
    }

    /// The timer fired: offer Force quit when the deadline has passed, else
    /// wait out the rest, as the clock may lag the timer.
    fn tick_exit(&mut self, cx: &mut Context<Self>) {
        let now = (self.now)();
        let Some(exit) = &mut self.exit else {
            return;
        };
        if exit.model.tick(now) {
            tracing::warn!("exit dialog: no answer from the daemon; offering force quit");
            cx.notify();
        }
        self.schedule_exit_tick(cx);
    }

    /// "Stop sessions, remove worktrees": the walk asks each worktree
    /// session's branch fate first; with none to ask about the removal goes
    /// out at once.
    fn start_exit_walk(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(exit) = &mut self.exit else {
            return;
        };
        if let Some(walk) = Walk::start(self.sidebar.sessions()) {
            tracing::info!(
                sessions = walk.progress().1,
                "exit dialog: asking branch fates"
            );
            exit.model.start_walk(walk);
            self.open_walk_confirm(window, cx);
        } else {
            let live = self.sidebar.sessions();
            let messages = removal_messages(&covered(live), &HashMap::new(), live);
            self.begin_shutdown(ExitButton::StopRemove, false, messages, cx);
        }
    }

    /// Opens the confirm for the walk's current session, or, once none is
    /// left, sends the removal.
    fn open_walk_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(walk) = self.exit.as_ref().and_then(|exit| exit.model.walk()) else {
            return;
        };
        let (index, total) = walk.progress();
        match walk.current().map(str::to_owned) {
            Some(session_id) => {
                let owner = ConfirmOwner::ExitWalk { index, total };
                self.open_delete_dialog(&session_id, owner, window, cx);
            }
            None => self.finish_exit_walk(window, cx),
        }
    }

    /// Every answer is in: the stops and discards for the sessions the walk
    /// started with, then the shutdown.
    fn finish_exit_walk(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let live = self.sidebar.sessions();
        let Some(messages) = self
            .exit
            .as_ref()
            .and_then(|exit| exit.model.walk())
            .map(|walk| removal_messages(walk.covered(), walk.choices(), live))
        else {
            return;
        };
        self.delete_dialog = None;
        self.begin_shutdown(ExitButton::StopRemove, false, messages, cx);
        self.quit_focus.focus(window);
    }

    /// The walk's confirm answered `branch` for `session_id`: record it and
    /// ask about the next session.
    pub(crate) fn answer_exit_walk(
        &mut self,
        session_id: &str,
        branch: BranchCleanup,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let sidebar = &self.sidebar;
        let Some(walk) = self.exit.as_mut().and_then(|exit| exit.model.walk_mut()) else {
            return;
        };
        tracing::info!(session_id, ?branch, "exit dialog: branch fate chosen");
        walk.record(session_id, branch);
        walk.skip_vanished(|id| sidebar.session(id).is_some());
        self.open_walk_confirm(window, cx);
    }

    /// The walk was cancelled: nothing is sent, and the exit dialog takes
    /// the keyboard back.
    pub(crate) fn cancel_exit_walk(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(exit) = &mut self.exit else {
            return;
        };
        tracing::info!("exit dialog: branch-fate walk cancelled; nothing sent");
        exit.model.cancel_walk();
        self.quit_focus.focus(window);
        cx.notify();
    }

    /// After a connection change or a session-list change while the dialog
    /// is open: a shutdown that went out quits once the connection leaves
    /// open and connecting; a walk ends when the connection goes and skips
    /// the sessions the daemon no longer lists; the dialog (or its walk)
    /// keeps the keyboard.
    pub(crate) fn after_exit_event(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(exit) = &mut self.exit else {
            return;
        };
        if exit.model.in_flight() {
            // A close before the network thread sent the shutdown predates
            // it: `ShutdownFailed` follows and the dialog stays.
            if exit.model.is_sent()
                && !matches!(self.conn.state(), State::Open { .. } | State::Connecting)
            {
                tracing::info!(state = ?self.conn.state(), "exit dialog: the connection closed; quitting");
                self.request_quit(cx);
            }
            return;
        }
        if exit.model.walk().is_some() {
            if !self.conn.is_open() {
                self.delete_dialog = None;
                self.cancel_exit_walk(window, cx);
                return;
            }
            self.skip_vanished_in_walk(window, cx);
        }
        if self.exit_walk_confirm_open() {
            self.dialog_focus.focus(window);
        } else {
            self.quit_focus.focus(window);
        }
    }

    /// Drops the sessions the daemon no longer lists from the walk; only
    /// when the one being asked about went does the confirm move on.
    fn skip_vanished_in_walk(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let sidebar = &self.sidebar;
        let Some(walk) = self.exit.as_mut().and_then(|exit| exit.model.walk_mut()) else {
            return;
        };
        let asking = walk.current().map(str::to_owned);
        if !walk.skip_vanished(|id| sidebar.session(id).is_some()) {
            return;
        }
        tracing::info!("exit dialog: a session in the walk went; skipping it");
        if walk.current() != asking.as_deref() {
            self.open_walk_confirm(window, cx);
        }
    }

    /// The network thread sent the shutdown: a close now ends the wait.
    pub(crate) fn on_shutdown_sent(&mut self) {
        if let Some(exit) = &mut self.exit {
            exit.model.mark_sent();
        }
    }

    /// The shutdown found no connection and sent nothing: the dialog goes
    /// back to the choice with the error line, and the app stays.
    pub(crate) fn on_shutdown_failed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(exit) = &mut self.exit else {
            return;
        };
        tracing::warn!("exit dialog: the connection dropped before the shutdown went out");
        exit.model.fail_shutdown();
        exit.timer = None;
        self.quit_focus.focus(window);
        cx.notify();
    }

    /// `ShutdownAck`: the daemon is done; quit.
    pub(crate) fn on_shutdown_ack(&mut self, cx: &mut Context<Self>) {
        if self
            .exit
            .as_ref()
            .is_some_and(|exit| exit.model.in_flight())
        {
            tracing::info!("exit dialog: the daemon acknowledged the shutdown; quitting");
            self.request_quit(cx);
        }
    }

    /// A key while the exit dialog is open; returns whether it was. The
    /// walk's confirm on top takes the keys. Otherwise Esc cancels unless a
    /// shutdown is on the way, Enter and Space press the focused button, and
    /// Tab and Shift+Tab move the focus over the enabled buttons.
    pub(crate) fn on_exit_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.exit.is_none() {
            return false;
        }
        if self.exit_walk_confirm_open() {
            self.on_delete_dialog_key(keystroke, window, cx);
            return true;
        }
        let counts = self.exit_counts();
        let connected = self.conn.is_open();
        let Some(exit) = &mut self.exit else {
            return true;
        };
        match keystroke.key.as_str() {
            "escape" if !exit.model.in_flight() => self.close_exit_dialog(window, cx),
            "enter" | "space" => {
                if let Some(button) = exit.model.focused(counts, connected) {
                    self.press_exit_button(button, window, cx);
                }
            }
            "tab" => {
                exit.model
                    .move_focus(!keystroke.modifiers.shift, counts, connected);
                cx.notify();
            }
            _ => {}
        }
        true
    }

    /// The exit dialog over a backdrop that takes every click beneath it.
    pub(crate) fn exit_layer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let model = &self.exit.as_ref()?.model;
        let counts = self.exit_counts();
        let connected = self.conn.is_open();
        let focused = model.focused(counts, connected);
        let buttons: Vec<AnyElement> = model
            .buttons(counts)
            .into_iter()
            .map(|button| {
                let enabled = model.enabled(button, connected);
                let element = dialog_button(
                    button.selector(),
                    model.label(button, counts),
                    button.is_danger(),
                    focused == Some(button),
                );
                if enabled {
                    element
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.press_exit_button(button, window, cx);
                        }))
                        .into_any_element()
                } else {
                    element.opacity(0.5).cursor_default().into_any_element()
                }
            })
            .collect();
        let active = div()
            .flex()
            .flex_wrap()
            .gap(px(4.0))
            .child(counts.active_line())
            .children(
                counts
                    .orphan_note()
                    .map(|note| div().text_color(gpui::rgb(MUTED)).child(note)),
            );
        let panel = modal_panel("exit-confirm-panel")
            .track_focus(&self.quit_focus)
            .child(div().font_weight(FontWeight::SEMIBOLD).child(TITLE))
            .child(div().child(BODY))
            .child(active)
            .children(
                counts
                    .worktree_note()
                    .map(|note| div().text_color(gpui::rgb(MUTED)).child(note)),
            )
            .children(
                model
                    .warning()
                    .map(|warning| div().text_color(gpui::rgb(DANGER)).child(warning)),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .justify_end()
                    .gap(px(6.0))
                    .children(buttons),
            );
        Some(backdrop("exit-confirm-dialog", panel))
    }
}
