//! The context menu of a dispatch run's sidebar leaf: Open, Stop (armed by
//! a first press, sent by the second) and Copy run folder.

use gpui::{
    AnyElement, ClickEvent, Context, MouseButton, MouseDownEvent, Pixels, Point, Window, anchored,
    deferred, div, prelude::*,
};
use protocol::{ClientMessage, DispatchRunStatus};

use crate::RootView;
use crate::session_menu::{MENU_MAX_SHARE, disabled_row, menu_frame, menu_item};
use crate::target::HostKey;
use crate::ui_theme::theme;

/// The open dispatch run menu: the run it acts on, where it opened and
/// whether its Stop row is armed.
pub(crate) struct DispatchMenu {
    pub(crate) host: HostKey,
    pub(crate) run_id: String,
    at: Point<Pixels>,
    max_height: Pixels,
    stop_armed: bool,
}

/// What a row of the menu does when pressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DispatchAction {
    Open,
    Stop,
    CopyFolder,
}

/// One row of the menu as drawn: its selector, its label, and the tooltip
/// saying why it cannot be pressed when it is disabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DispatchRow {
    pub selector: &'static str,
    pub label: &'static str,
    pub disabled_tip: Option<&'static str>,
    pub action: DispatchAction,
}

/// The menu's rows for a run with `status`, Open, Stop and Copy run folder
/// in that order; Stop reads its confirm text while `stop_armed`.
pub(crate) fn rows(status: DispatchRunStatus, stop_armed: bool) -> Vec<DispatchRow> {
    let stop_tip = stop_tip(status);
    let stop_label = if stop_armed && stop_tip.is_none() {
        STOP_CONFIRM_LABEL
    } else {
        STOP_LABEL
    };
    vec![
        DispatchRow {
            selector: OPEN_SELECTOR,
            label: "Open",
            disabled_tip: None,
            action: DispatchAction::Open,
        },
        DispatchRow {
            selector: STOP_SELECTOR,
            label: stop_label,
            disabled_tip: stop_tip,
            action: DispatchAction::Stop,
        },
        DispatchRow {
            selector: COPY_FOLDER_SELECTOR,
            label: "Copy run folder",
            disabled_tip: None,
            action: DispatchAction::CopyFolder,
        },
    ]
}

/// Why Stop cannot be pressed for a run with `status`; `None` while the
/// run can still be stopped.
fn stop_tip(status: DispatchRunStatus) -> Option<&'static str> {
    match status {
        DispatchRunStatus::Stopping => Some(STOPPING_TIP),
        DispatchRunStatus::Ended => Some(ENDED_TIP),
        DispatchRunStatus::Starting | DispatchRunStatus::Working | DispatchRunStatus::Unknown => {
            None
        }
    }
}

const OPEN_SELECTOR: &str = "dispatch-menu-open";
const STOP_SELECTOR: &str = "dispatch-menu-stop";
const COPY_FOLDER_SELECTOR: &str = "dispatch-menu-copy-folder";
const STOP_LABEL: &str = "Stop run";
const STOP_CONFIRM_LABEL: &str = "Confirm: stop run…";
const STOPPING_TIP: &str = "The run is already stopping";
const ENDED_TIP: &str = "The run has ended";

impl RootView {
    /// Opens the menu of `host`'s dispatch run `run_id` at `at`, taking the
    /// keyboard so Esc reaches it; any other menu gives way. Nothing opens
    /// for a run the host does not list.
    pub(crate) fn open_dispatch_menu(
        &mut self,
        host: &HostKey,
        run_id: &str,
        at: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.sidebar.run_on(host, run_id).is_none() {
            return;
        }
        self.close_session_menu(window, cx);
        self.close_container_menu(window, cx);
        self.close_shell_menu(window, cx);
        self.close_empty_pane_menu(window, cx);
        self.close_pane_overflow_menu(window, cx);
        self.close_tab_menu(window, cx);
        self.close_sc_picker(window, cx);
        self.close_sc_file_menu(window, cx);
        self.close_more_menu(window, cx);
        self.dispatch_menu = Some(DispatchMenu {
            host: host.clone(),
            run_id: run_id.to_owned(),
            at,
            max_height: window.viewport_size().height * MENU_MAX_SHARE,
            stop_armed: false,
        });
        self.menu_focus.focus(window);
        cx.notify();
    }

    /// Closes the dispatch run menu, dropping its Stop arm, and hands the
    /// keyboard back to the active pane.
    pub(crate) fn close_dispatch_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dispatch_menu.take().is_some() {
            self.focus_active_pane(window, cx);
            cx.notify();
        }
    }

    /// Whether a dispatch run menu is open.
    #[must_use]
    pub fn dispatch_menu_open(&self) -> bool {
        self.dispatch_menu.is_some()
    }

    /// The open dispatch run menu's row labels, in order; empty when no
    /// menu is open or its run is gone.
    #[must_use]
    pub fn dispatch_menu_labels(&self) -> Vec<String> {
        self.dispatch_menu_rows()
            .into_iter()
            .map(|row| row.label.to_owned())
            .collect()
    }

    /// The open menu's rows, read from the run as its host lists it now.
    fn dispatch_menu_rows(&self) -> Vec<DispatchRow> {
        let Some(menu) = self.dispatch_menu.as_ref() else {
            return Vec::new();
        };
        self.sidebar
            .run_on(&menu.host, &menu.run_id)
            .map(|run| rows(run.status, menu.stop_armed))
            .unwrap_or_default()
    }

    /// Runs the open menu's `action` on its run. Stop arms on its first
    /// press and sends on the second; every other row closes the menu. A
    /// run its host no longer lists only closes the menu.
    fn activate_dispatch_row(
        &mut self,
        action: DispatchAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(menu) = self.dispatch_menu.as_ref() else {
            return;
        };
        let (host, run_id) = (menu.host.clone(), menu.run_id.clone());
        let Some(run_dir) = self
            .sidebar
            .run_on(&host, &run_id)
            .map(|run| run.run_dir.clone())
        else {
            self.close_dispatch_menu(window, cx);
            return;
        };
        match action {
            DispatchAction::Open => {
                self.close_dispatch_menu(window, cx);
                self.open_dispatch_run(&host, &run_id, window, cx);
            }
            DispatchAction::Stop => self.press_dispatch_stop(window, cx),
            DispatchAction::CopyFolder => {
                self.close_dispatch_menu(window, cx);
                self.copy_to_clipboard(&run_dir, cx);
            }
        }
    }

    /// A press on the open menu's Stop row: the first arms it, the second
    /// asks the run's host to stop the run and closes the menu. A run that
    /// is stopping or has ended takes no press.
    fn press_dispatch_stop(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(menu) = self.dispatch_menu.as_mut() else {
            return;
        };
        let Some(status) = self
            .sidebar
            .run_on(&menu.host, &menu.run_id)
            .map(|run| run.status)
        else {
            return;
        };
        if stop_tip(status).is_some() {
            return;
        }
        if !menu.stop_armed {
            menu.stop_armed = true;
            cx.notify();
            return;
        }
        let (host, run_id) = (menu.host.clone(), menu.run_id.clone());
        self.send_to(
            &host,
            ClientMessage::StopDispatchRun {
                run_id,
                request_id: Some(crate::new_request_id()),
            },
        );
        self.close_dispatch_menu(window, cx);
    }

    /// One menu row as drawn: a disabled one says why in its tooltip, and
    /// Stop's press stays inside the menu so it can arm.
    fn dispatch_row_element(row: DispatchRow, cx: &mut Context<Self>) -> AnyElement {
        let t = theme(cx);
        if let Some(tip) = row.disabled_tip {
            return disabled_row(row.selector, row.label, tip, &t).into_any_element();
        }
        let stop = row.action == DispatchAction::Stop;
        let item = menu_item(row.selector, row.label, stop, &t);
        let item = if stop {
            item.on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _: &MouseDownEvent, _, cx| cx.stop_propagation()),
            )
        } else {
            item
        };
        let action = row.action;
        item.on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
            this.activate_dispatch_row(action, window, cx);
            if stop {
                cx.stop_propagation();
            }
        }))
        .into_any_element()
    }

    /// The layers of the sidebar's row menus, the container menu's then the
    /// dispatch run menu's, for whichever is open.
    pub(crate) fn sidebar_menu_layers(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut layers: Vec<AnyElement> = self
            .container_menu_layer(cx)
            .into_iter()
            .flatten()
            .collect();
        layers.extend(self.dispatch_menu_layer(cx).into_iter().flatten());
        layers
    }

    /// The open dispatch run menu over a layer that keeps a click outside it
    /// from reaching what lies beneath; nothing while its run is gone.
    fn dispatch_menu_layer(&self, cx: &mut Context<Self>) -> Option<[AnyElement; 2]> {
        let menu = self.dispatch_menu.as_ref()?;
        let (at, max_height) = (menu.at, menu.max_height);
        let rows = self.dispatch_menu_rows();
        if rows.is_empty() {
            return None;
        }
        let backdrop = div().absolute().top_0().left_0().size_full().occlude();
        let mut frame = menu_frame(
            crate::fonts::ui_text_size(cx),
            "dispatch-menu",
            &self.menu_focus,
            max_height,
            cx.listener(|this, _: &MouseDownEvent, window, cx| {
                this.close_dispatch_menu(window, cx);
                cx.stop_propagation();
            }),
            &theme(cx),
        );
        for row in rows {
            frame = frame.child(Self::dispatch_row_element(row, cx));
        }
        let panel = anchored().position(at).snap_to_window().child(frame);
        Some([
            backdrop.into_any_element(),
            deferred(panel).with_priority(1).into_any_element(),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(selector: &'static str, label: &'static str, tip: Option<&'static str>) -> DispatchRow {
        let action = match selector {
            "dispatch-menu-open" => DispatchAction::Open,
            "dispatch-menu-stop" => DispatchAction::Stop,
            _ => DispatchAction::CopyFolder,
        };
        DispatchRow {
            selector,
            label,
            disabled_tip: tip,
            action,
        }
    }

    fn stop_row(status: DispatchRunStatus, armed: bool) -> DispatchRow {
        rows(status, armed)
            .into_iter()
            .find(|row| row.action == DispatchAction::Stop)
            .unwrap_or_else(|| row("missing", "missing", None))
    }

    #[test]
    fn rows_offer_open_stop_and_copy_for_a_working_run() {
        assert_eq!(
            rows(DispatchRunStatus::Working, false),
            vec![
                row("dispatch-menu-open", "Open", None),
                row("dispatch-menu-stop", "Stop run", None),
                row("dispatch-menu-copy-folder", "Copy run folder", None),
            ]
        );
    }

    #[test]
    fn an_armed_stop_reads_confirm() {
        assert_eq!(
            stop_row(DispatchRunStatus::Working, true),
            row("dispatch-menu-stop", "Confirm: stop run…", None)
        );
    }

    #[test]
    fn stop_is_disabled_while_the_run_is_stopping() {
        assert_eq!(
            stop_row(DispatchRunStatus::Stopping, false),
            row(
                "dispatch-menu-stop",
                "Stop run",
                Some("The run is already stopping")
            )
        );
    }

    #[test]
    fn stop_is_disabled_once_the_run_has_ended() {
        assert_eq!(
            stop_row(DispatchRunStatus::Ended, false),
            row("dispatch-menu-stop", "Stop run", Some("The run has ended"))
        );
    }

    #[test]
    fn stop_is_enabled_for_a_starting_or_unknown_run() {
        for status in [DispatchRunStatus::Starting, DispatchRunStatus::Unknown] {
            assert_eq!(
                stop_row(status, false),
                row("dispatch-menu-stop", "Stop run", None),
                "{status:?}"
            );
        }
    }
}
