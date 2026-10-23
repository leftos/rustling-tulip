//! Terminal links on the root: the link a pane's Ctrl+click picked, checked
//! and opened through the [`Opener`](crate::open::Opener) on a background
//! thread, the confirm before a file that runs code, and the toasts that say
//! what did not open.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{AnyElement, ClickEvent, Context, FontWeight, Keystroke, Window, div, prelude::*, px};

use crate::links::{LinkKind, TerminalLink, TerminalLinkCandidate};
use crate::notice_view::modal_panel;
use crate::notices::{ToastKind, ToastSpec};
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

/// Where an open came from, which decides the route it and its follow-ups
/// (the run confirm, the VS Code-missing fallback) take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Origin {
    /// A link, a menu or a button: refused on a remote host unless a URL.
    Link,
    /// A file fetched from the remote host onto this PC.
    Fetched,
}

/// How an open goes: what follows it and where it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Route {
    pub(crate) then: Then,
    pub(crate) origin: Origin,
}

impl Route {
    const LINK: Self = Self {
        then: Then::Nothing,
        origin: Origin::Link,
    };
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
    /// `base_dirs`, the folders of `session`, the pane's session. On a
    /// remote host the path is fetched from `session`'s repos instead.
    pub(crate) fn open_link(
        &mut self,
        link: &TerminalLink,
        base_dirs: Vec<String>,
        session: Option<&str>,
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
            // A host path is not on this PC; resolving it here could open
            // a local file of the same name, so it is fetched instead.
            LinkKind::Path if !self.capabilities().opens_local_files() => {
                self.fetch_path_link(link, session, cx);
            }
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
                self.open_in_default_app(path, Route::LINK, window, cx);
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
                self.push_toast_spec(
                    ToastSpec {
                        kind: ToastKind::Warning,
                        title: format!("Not opening a network path on {host}"),
                        detail: Some(
                            "Add it to unc_hosts in native-ui.json to allow it.".to_owned(),
                        ),
                        key: Some(format!("unc-refused:{}", host.to_lowercase())),
                        sticky: false,
                        actions: Vec::new(),
                    },
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
        route: Route,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let job = OpenJob::DefaultApp {
            path,
            confirmed: false,
        };
        self.open_by(job, route, window, cx);
    }

    /// Opens `job` by the route its origin takes: a link's through the
    /// remote host's refusal, a fetched file's through the check that it
    /// is one.
    fn open_by(&mut self, job: OpenJob, route: Route, window: &mut Window, cx: &mut Context<Self>) {
        match route.origin {
            Origin::Link => self.dispatch_open(job, route.then, window, cx),
            Origin::Fetched => self.open_fetched_job(job, route.then, window, cx),
        }
    }

    /// A background check found that the link's file `path` runs code:
    /// opens the run confirm for it, unless the confirm or another dialog is
    /// already up, when the request is dropped and a toast says why.
    pub fn ask_to_run(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.ask_to_run_from(path, Origin::Link, window, cx);
    }

    /// [`Self::ask_to_run`] for an open from `origin`, which the confirm's
    /// answer keeps.
    fn ask_to_run_from(
        &mut self,
        path: PathBuf,
        origin: Origin,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
        self.run_confirm_origin = origin;
        self.run_focus.focus(window);
        cx.notify();
    }

    /// The exit, delete-worktree, spawn, Shell…, appearance, Settings,
    /// checkout or discard dialog, the source-control file menu, or an
    /// action-failed notice, is open.
    fn modal_open(&self) -> bool {
        self.exit.is_some()
            || self.delete_dialog.is_some()
            || self.spawn_dialog.is_some()
            || self.shell_dialog.is_some()
            || self.appearance_editor.is_some()
            || self.changes.discard.is_some()
            || self.stash.drop.is_some()
            || self.changes.file_menu.is_some()
            || self.notices.has_modal()
    }

    /// Hands `job` to the opener on a background thread and reports how it
    /// went. On a remote host only a URL opens; any other job would open a
    /// host path on this PC, so it is refused with a toast.
    pub(crate) fn dispatch_open(
        &mut self,
        job: OpenJob,
        then: Then,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !matches!(job, OpenJob::Url(_))
            && let Some(reason) = self.capabilities().local_files_refusal()
        {
            tracing::warn!(?job, "refusing a local open on a remote host");
            self.push_toast(
                ToastKind::Warning,
                COULD_NOT_OPEN,
                Some(reason.to_owned()),
                cx,
            );
            return;
        }
        let route = Route {
            then,
            origin: Origin::Link,
        };
        self.run_open(job, route, window, cx);
    }

    /// Hands `job` to the opener on a background thread and reports how it
    /// went, with no check of where the connection is; its follow-ups keep
    /// `route`'s origin.
    pub(crate) fn run_open(
        &mut self,
        job: OpenJob,
        route: Route,
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
                root.after_open(job, route, result, window, cx);
            })
            .ok();
        })
        .detach();
    }

    fn after_open(
        &mut self,
        job: OpenJob,
        route: Route,
        result: Result<JobOutcome, OpenFailure>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let fallback = Route {
            then: Then::SayVsCodeMissing,
            origin: route.origin,
        };
        match (result, job) {
            (Ok(JobOutcome::AskFirst(path)), _) => {
                self.ask_to_run_from(path, route.origin, window, cx);
            }
            (Ok(JobOutcome::Opened), OpenJob::DefaultApp { path, .. })
                if route.then == Then::SayVsCodeMissing =>
            {
                self.push_toast_spec(
                    ToastSpec {
                        kind: ToastKind::Warning,
                        title: "VS Code not found".to_owned(),
                        detail: Some(format!("Opened {} in its default app.", file_name(&path))),
                        key: None,
                        sticky: false,
                        actions: Vec::new(),
                    },
                    cx,
                );
            }
            (Ok(JobOutcome::Opened), _) => {}
            (Err(OpenFailure::VsCodeNotFound), OpenJob::VsCode { path, .. }) => {
                tracing::warn!(path = %path.display(), "VS Code not found; opening the file in its default app");
                self.open_in_default_app(path, fallback, window, cx);
            }
            (Err(OpenFailure::VsCodeNotFound), OpenJob::VsCodeFolder(paths)) => {
                if let Some(path) = paths.first() {
                    tracing::warn!(path = %path.display(), "VS Code not found; opening the folder in the file manager");
                    self.open_in_default_app(path.clone(), fallback, window, cx);
                } else {
                    tracing::warn!("VS Code not found and the job names no folder");
                }
            }
            (Err(OpenFailure::VsCodeNotFound), OpenJob::VsCodeWorkspace(path)) => {
                tracing::warn!(path = %path.display(), "VS Code not found; opening the workspace file in its default app");
                self.open_in_default_app(path, fallback, window, cx);
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
        let route = Route {
            then: Then::Nothing,
            origin: self.run_confirm_origin,
        };
        match button {
            RunButton::Cancel => {}
            RunButton::Reveal => self.open_by(OpenJob::Reveal(path), route, window, cx),
            RunButton::Run => {
                let job = OpenJob::DefaultApp {
                    path,
                    confirmed: true,
                };
                self.open_by(job, route, window, cx);
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
                    true,
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
