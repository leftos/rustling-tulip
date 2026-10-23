//! Viewer tabs on the root view: opening one, building their state from the
//! tab list, the daemon's answers, the keyboard, and the placeholder body
//! that names the file. The state is [`crate::viewer_tab`].

use std::path::{Path, PathBuf};

use gpui::{AnyElement, Context, Window, div, prelude::*, px};
use protocol::{ClientMessage, DaemonMessage, TabContent};

use crate::notices::ToastKind;
use crate::viewer_tab::{self, Activation, OPEN_FAILED_TITLE, ViewerTab};
use crate::{MUTED, RootView, UI_TEXT_SIZE};

impl RootView {
    /// Asks the daemon to open, or focus, a viewer tab of `path`, a file of
    /// this PC; the tab it names is activated and its target moves to
    /// `line`. The path is trimmed and made absolute first; an empty one
    /// sends nothing.
    pub fn open_viewer(&mut self, path: &str, line: Option<u32>) {
        let normalised = match viewer_tab::normalise_path(path) {
            Ok(normalised) => normalised,
            Err(err) => {
                tracing::warn!("no viewer tab opened for {path:?}: {err}");
                return;
            }
        };
        let id = self.viewer_opens.open_id(line);
        self.send(ClientMessage::OpenViewerTab {
            id,
            path: normalised,
        });
    }

    /// Gives every viewer tab its state, deriving where its file comes from,
    /// and drops the state of tabs that are gone. Reads no file.
    pub(crate) fn reconcile_viewer_tabs(&mut self) {
        let paths: Vec<(String, String)> = self
            .tabs
            .tabs()
            .iter()
            .filter_map(|tab| match &tab.content {
                TabContent::Viewer { path } => Some((tab.id.clone(), path.clone())),
                TabContent::Grid { .. } | TabContent::Diff { .. } => None,
            })
            .collect();
        self.viewer_tabs
            .retain(|tab_id, _| paths.iter().any(|(id, _)| id == tab_id));
        let opens_local_files = self.capabilities().opens_local_files();
        let download_root = self.remote_files_root.as_ref().ok().map(PathBuf::as_path);
        for (tab_id, path) in paths {
            self.viewer_tabs.entry(tab_id).or_insert_with(|| {
                let origin =
                    viewer_tab::derive_origin(Path::new(&path), download_root, opens_local_files);
                ViewerTab::new(path, origin)
            });
        }
    }

    /// Folds a viewer message in; returns whether nothing else is owed it.
    pub(crate) fn on_viewer_message(
        &mut self,
        msg: &DaemonMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match msg {
            DaemonMessage::Welcome { .. } => {
                self.viewer_opens.reset();
                false
            }
            DaemonMessage::ViewerTabOpened { id, tab_id } => {
                let known = self.tabs.tab(tab_id).is_some();
                if let Some(activation) = self.viewer_opens.opened(id, tab_id, known, (self.now)())
                {
                    // Re-opening the active tab focuses it again too.
                    self.last_active_tab = None;
                    self.activate_viewer(&activation);
                    self.after_tabs_change(window, cx);
                }
                true
            }
            DaemonMessage::Error {
                message,
                request_id: Some(id),
            } if self.viewer_opens.failed(id) => {
                tracing::warn!("the daemon could not open a viewer tab: {message}");
                self.push_toast(
                    ToastKind::Error,
                    OPEN_FAILED_TITLE,
                    Some(message.clone()),
                    cx,
                );
                true
            }
            _ => false,
        }
    }

    /// Activates the tab an open named and moves its target line.
    fn activate_viewer(&mut self, activation: &Activation) {
        if let Some(tab) = self.viewer_tabs.get_mut(&activation.tab_id) {
            tab.set_line_target(activation.line);
        }
        self.tabs.activate(&activation.tab_id);
    }

    /// Activates the tab an `OpenViewerTab` named before the tab list held
    /// it, once it does.
    pub(crate) fn activate_pending_viewer_tab(&mut self) {
        let now = (self.now)();
        let tabs = &self.tabs;
        let ready = self
            .viewer_opens
            .take_ready(|tab_id| tabs.tab(tab_id).is_some(), now);
        if let Some(activation) = ready {
            self.activate_viewer(&activation);
        }
    }

    /// Gives the keyboard to the active tab when it is a viewer tab; returns
    /// whether it was.
    pub(crate) fn focus_active_viewer_tab(&self, window: &mut Window) -> bool {
        let active_viewer = self
            .tabs
            .active_id()
            .is_some_and(|id| self.viewer_tabs.contains_key(id));
        if active_viewer {
            self.viewer_focus.focus(window);
        }
        active_viewer
    }

    /// Viewer tab `tab_id`'s state.
    #[must_use]
    pub fn viewer_tab(&self, tab_id: &str) -> Option<&ViewerTab> {
        self.viewer_tabs.get(tab_id)
    }

    /// Whether viewer tab `tab_id` is the tab shown and holds the keyboard.
    #[must_use]
    pub fn viewer_tab_focused(&self, tab_id: &str, window: &Window) -> bool {
        self.tabs.active_id() == Some(tab_id)
            && self.viewer_tabs.contains_key(tab_id)
            && self.viewer_focus.is_focused(window)
    }

    /// Viewer tab `tab_id`'s body: the file's name, and under it why the
    /// file is not read when it is not.
    pub(crate) fn viewer_tab_element(&self, tab_id: &str) -> Option<AnyElement> {
        let tab = self.viewer_tabs.get(tab_id)?;
        let body = format!("viewer-placeholder-{tab_id}");
        let note = tab.note().map(|note| {
            let selector = format!("viewer-note-{tab_id}");
            div().debug_selector(move || selector).child(note)
        });
        Some(
            div()
                .debug_selector(move || body)
                .track_focus(&self.viewer_focus)
                .flex()
                .flex_col()
                .flex_1()
                .size_full()
                .items_center()
                .justify_center()
                .gap(px(6.0))
                .text_size(px(UI_TEXT_SIZE))
                .text_color(gpui::rgb(MUTED))
                .child(tab.name().to_owned())
                .children(note)
                .into_any_element(),
        )
    }
}
