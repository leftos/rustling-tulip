//! Terminal links on the root: the link a pane's Ctrl+click picked, checked
//! and opened through the [`Opener`](crate::open::Opener) on a background
//! thread, the confirm before a file that runs code, and the toasts that say
//! what did not open.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{AnyElement, ClickEvent, Context, FontWeight, Keystroke, Window, div, prelude::*, px};

use crate::links::{LinkKind, TerminalLink, TerminalLinkCandidate};
use crate::notice_view::modal_panel;
use crate::notices::ToastKind;
use crate::open::{self, JobOutcome, OpenAction, OpenFailure, OpenJob, Resolution};
use crate::run_confirm::{RunButton, RunConfirm};
use crate::session_menu::{backdrop, dialog_button};
use crate::{MUTED, RootView};

/// The title of every toast about a link that did not open.
pub(crate) const COULD_NOT_OPEN: &str = "Couldn't open";

/// What happens once an open went through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Then {
    Nothing,
    /// The link asked for VS Code, which is not installed, so its file went
    /// to its default app instead: say so.
    SayVsCodeMissing,
}

impl RootView {
    /// The run confirm's title and detail while it is open.
    #[must_use]
    pub fn run_confirm_text(&self) -> Option<(String, String)> {
        let confirm = self.run_confirm.as_ref()?;
        Some((confirm.title(), confirm.detail()))
    }

    /// The selector of the run confirm's focused button while it is open.
    #[must_use]
    pub fn run_confirm_focus(&self) -> Option<String> {
        let confirm = self.run_confirm.as_ref()?;
        Some(confirm.focused().selector().to_owned())
    }

    /// A pane's Ctrl+click picked `link`; a path in it resolves against
    /// `base_dirs`, the pane's session's folders.
    pub(crate) fn open_link(
        &mut self,
        link: &TerminalLink,
        base_dirs: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match link.kind {
            LinkKind::Url => match open::validate_http_url(&link.target) {
                Ok(url) => {
                    self.dispatch_open(OpenJob::Url(url.to_owned()), Then::Nothing, window, cx);
                }
                Err(err) => {
                    tracing::warn!("refusing terminal link {}: {err}", link.target);
                    self.push_toast(
                        ToastKind::Error,
                        COULD_NOT_OPEN,
                        Some("Only http and https links open from the terminal.".to_owned()),
                        cx,
                    );
                }
            },
            LinkKind::Path => self.resolve_path(link.candidates.clone(), base_dirs, window, cx),
        }
    }

    /// Checks the path's readings on a background thread, then opens the
    /// first that exists.
    fn resolve_path(
        &mut self,
        candidates: Vec<TerminalLinkCandidate>,
        base_dirs: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let first = candidates
            .first()
            .map(|candidate| candidate.path.clone())
            .unwrap_or_default();
        let ui_dir = self.ui_dir.clone();
        let opener = Arc::clone(&self.opener);
        let resolve = cx.background_executor().spawn(async move {
            open::resolve_link(&candidates, &base_dirs, ui_dir.as_deref(), opener.as_ref())
        });
        cx.spawn_in(window, async move |this, cx| {
            let resolution = resolve.await;
            // Fails only when the view is gone, leaving nowhere to report.
            this.update_in(cx, |root, window, cx| {
                root.after_resolve(resolution, &first, window, cx);
            })
            .ok();
        })
        .detach();
    }

    fn after_resolve(
        &mut self,
        resolution: Resolution,
        first: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match resolution {
            Resolution::Open(OpenAction::VsCode { path, line, column }) => {
                let job = OpenJob::VsCode {
                    path,
                    line,
                    column: column.unwrap_or(1),
                };
                self.dispatch_open(job, Then::Nothing, window, cx);
            }
            Resolution::Open(OpenAction::DefaultApp(path)) => {
                self.open_in_default_app(path, Then::Nothing, window, cx);
            }
            Resolution::NotFound(err) => {
                tracing::warn!("no file found for terminal link {first}: {err}");
                self.push_toast(
                    ToastKind::Error,
                    COULD_NOT_OPEN,
                    Some(format!("No file found for {first}")),
                    cx,
                );
            }
            Resolution::Refused { host } => {
                tracing::warn!(
                    "terminal link {first} is on network host {host}, which is neither mapped nor listed"
                );
                self.push_toast(
                    ToastKind::Error,
                    &format!("Not opening a network path on {host}"),
                    Some("Add it to unc_hosts in native-ui.json to allow it.".to_owned()),
                    cx,
                );
            }
        }
    }

    /// Sends `path` to its default app; a file that runs code comes back
    /// from the background check unopened, and the user is asked first.
    fn open_in_default_app(
        &mut self,
        path: PathBuf,
        then: Then,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let job = OpenJob::DefaultApp {
            path,
            confirmed: false,
        };
        self.dispatch_open(job, then, window, cx);
    }

    /// A background check found that the link's file `path` runs code:
    /// opens the run confirm for it, unless the confirm or another dialog is
    /// already up, when the request is dropped and a toast says why.
    pub fn ask_to_run(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if self.run_confirm.is_some() || self.modal_open() {
            tracing::info!(
                path = %path.display(),
                "terminal link runs code, but a dialog is open; not asking"
            );
            self.push_toast(
                ToastKind::Error,
                COULD_NOT_OPEN,
                Some("Another dialog is open.".to_owned()),
                cx,
            );
            return;
        }
        tracing::info!(path = %path.display(), "terminal link runs code; asking first");
        self.run_confirm = Some(RunConfirm::new(path));
        self.run_focus.focus(window);
        cx.notify();
    }

    /// The exit, delete-worktree, spawn, Shell…, appearance, Settings or
    /// checkout dialog, or an action-failed notice, is open.
    fn modal_open(&self) -> bool {
        self.exit.is_some()
            || self.delete_dialog.is_some()
            || self.spawn_dialog.is_some()
            || self.shell_dialog.is_some()
            || self.appearance_editor.is_some()
            || self.notices.has_modal()
    }

    /// Hands `job` to the opener on a background thread and reports how it
    /// went.
    pub(crate) fn dispatch_open(
        &mut self,
        job: OpenJob,
        then: Then,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let opener = Arc::clone(&self.opener);
        let run = {
            let job = job.clone();
            cx.background_executor()
                .spawn(async move { job.run(opener.as_ref()) })
        };
        cx.spawn_in(window, async move |this, cx| {
            let result = run.await;
            // Fails only when the view is gone, leaving nowhere to report.
            this.update_in(cx, |root, window, cx| {
                root.after_open(job, then, result, window, cx);
            })
            .ok();
        })
        .detach();
    }

    fn after_open(
        &mut self,
        job: OpenJob,
        then: Then,
        result: Result<JobOutcome, OpenFailure>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match (result, job) {
            (Ok(JobOutcome::AskFirst(path)), _) => self.ask_to_run(path, window, cx),
            (Ok(JobOutcome::Opened), OpenJob::DefaultApp { path, .. })
                if then == Then::SayVsCodeMissing =>
            {
                self.push_toast(
                    ToastKind::Info,
                    "VS Code not found",
                    Some(format!("Opened {} in its default app.", file_name(&path))),
                    cx,
                );
            }
            (Ok(JobOutcome::Opened), _) => {}
            (Err(OpenFailure::VsCodeNotFound), OpenJob::VsCode { path, .. }) => {
                tracing::warn!(path = %path.display(), "VS Code not found; opening the file in its default app");
                self.open_in_default_app(path, Then::SayVsCodeMissing, window, cx);
            }
            (Err(OpenFailure::VsCodeNotFound), job) => {
                tracing::warn!(?job, "VS Code not found for a job that does not use it");
            }
            (Err(OpenFailure::Failed(err)), job) => {
                tracing::warn!(?job, "opening a terminal link failed: {err}");
                self.push_toast(ToastKind::Error, COULD_NOT_OPEN, Some(err), cx);
            }
        }
    }

    fn press_run_button(&mut self, button: RunButton, window: &mut Window, cx: &mut Context<Self>) {
        let Some(confirm) = self.run_confirm.take() else {
            return;
        };
        let path = confirm.path().to_path_buf();
        tracing::info!(?button, path = %path.display(), "run confirm answered");
        match button {
            RunButton::Cancel => {}
            RunButton::Reveal => {
                self.dispatch_open(OpenJob::Reveal(path), Then::Nothing, window, cx);
            }
            RunButton::Run => {
                let job = OpenJob::DefaultApp {
                    path,
                    confirmed: true,
                };
                self.dispatch_open(job, Then::Nothing, window, cx);
            }
        }
        self.after_notice_closed(window, cx);
    }

    /// A key while the run confirm is open; returns whether it was.
    pub(crate) fn on_run_confirm_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(confirm) = &mut self.run_confirm else {
            return false;
        };
        match confirm.key(keystroke.key.as_str(), keystroke.modifiers.shift) {
            Some(button) => self.press_run_button(button, window, cx),
            None => cx.notify(),
        }
        true
    }

    /// The run confirm over a backdrop that takes every click beneath it.
    pub(crate) fn run_confirm_layer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let confirm = self.run_confirm.as_ref()?;
        let buttons: Vec<AnyElement> = RunButton::ALL
            .into_iter()
            .map(|button| {
                dialog_button(
                    button.selector(),
                    button.label().to_owned(),
                    button == RunButton::Run,
                    confirm.focused() == button,
                )
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.press_run_button(button, window, cx);
                }))
                .into_any_element()
            })
            .collect();
        let panel = modal_panel("run-confirm-panel")
            .track_focus(&self.run_focus)
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(confirm.title()),
            )
            .child(div().text_color(gpui::rgb(MUTED)).child(confirm.detail()))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .justify_end()
                    .gap(px(6.0))
                    .children(buttons),
            );
        Some(backdrop("run-confirm-dialog", panel))
    }
}

/// `path`'s file name, or the whole path when it has none.
fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}
