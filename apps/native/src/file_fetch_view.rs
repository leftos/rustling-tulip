//! Fetching a file from a remote host on the root: the request, the daemon's
//! replies fed to a writer off the UI thread, the toasts that say how it
//! went, and opening the saved copy.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use futures::channel::mpsc::unbounded;
use gpui::{Context, Window};
use protocol::{ClientMessage, DaemonMessage};

use crate::RootView;
use crate::fetch_link;
use crate::file_fetch::{
    self, FetchEntry, FetchRequest, ProbeGroup, WriteOutcome, WriteStep, display_host_path,
    is_under,
};
use crate::links::TerminalLink;
use crate::notices::{ToastKind, ToastSpec};
use crate::open::OpenJob;
use crate::open_view::{COULD_NOT_OPEN, Origin, Route, Then};

/// The title of the toast about a fetch that failed.
fn failed_title(file_name: &str) -> String {
    format!("Couldn't fetch {file_name}")
}

/// Why a fetch failed, after the host's path of the file once the daemon
/// named it.
fn failure_detail(entry: &FetchEntry, reason: &str) -> String {
    match entry.resolved_path() {
        Some(resolved) => format!("{}: {reason}", display_host_path(resolved)),
        None => reason.to_owned(),
    }
}

impl RootView {
    /// Saves fetched files under `root` instead of the data folder.
    pub fn set_remote_files_root(&mut self, root: PathBuf) {
        self.remote_files_root = Ok(root);
    }

    /// Asks the remote host for `request`'s file, saved under this host's
    /// download folder; returns the fetch's id, or `None` when the fetch is
    /// refused (the local daemon, a refused path, the same file already
    /// fetching), which a toast explains.
    pub fn fetch_remote_file(
        &mut self,
        request: FetchRequest,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        self.start_fetch(request, None, cx)
    }

    /// A Ctrl+click on a path link on a remote host: fetches the first of
    /// the link's readings under `session`'s repos and worktrees that the
    /// host has, trying them one at a time.
    pub(crate) fn fetch_path_link(
        &mut self,
        link: &TerminalLink,
        session: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        let mut readings = session
            .and_then(|id| self.sidebar.session(id))
            .map(|session| {
                fetch_link::session_readings(&link.candidates, session, self.sidebar.repos())
            })
            .unwrap_or_default();
        if readings.is_empty() {
            tracing::warn!(
                target = %link.target,
                session = session.unwrap_or("none"),
                "no repo or worktree of the session holds the terminal path link"
            );
            self.push_toast(
                ToastKind::Warning,
                &failed_title(file_fetch::file_name(&link.target)),
                Some("Not inside a repo or worktree of this session.".to_owned()),
                cx,
            );
            return;
        }
        let first = readings.remove(0);
        let group = self.fetches.new_group(readings);
        self.start_fetch(first, Some(group), cx);
    }

    /// [`Self::fetch_remote_file`], as one reading of `group`'s link when
    /// given: its "Fetching" toast is the group's, updated in place. A
    /// refusal is a toast.
    fn start_fetch(
        &mut self,
        request: FetchRequest,
        group: Option<ProbeGroup>,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        match self.try_start_fetch(request, group, false, cx) {
            Ok(id) => Some(id),
            Err(refusal) => {
                self.push_toast(refusal.kind, &refusal.title, refusal.detail, cx);
                None
            }
        }
    }

    /// Asks the host for `request`'s file and returns the fetch's id. A
    /// fetch for the Fetch file… dialog (`popup`) pushes no "Fetching"
    /// toast, since the dialog shows its progress.
    ///
    /// # Errors
    /// The [`Refusal`] when the fetch is refused: the local daemon, a
    /// refused path or download folder, the same file already fetching.
    pub(crate) fn try_start_fetch(
        &mut self,
        request: FetchRequest,
        group: Option<ProbeGroup>,
        popup: bool,
        cx: &mut Context<Self>,
    ) -> Result<String, Refusal> {
        let name = file_fetch::file_name(&request.rel_path).to_owned();
        let Some(host) = file_fetch::host_folder(self.conn.target()) else {
            tracing::warn!(rel = %request.rel_path, "refusing a file fetch from the local daemon");
            return Err(Refusal {
                kind: ToastKind::Warning,
                title: failed_title(&name),
                detail: Some("Files are fetched only from a remote host.".to_owned()),
            });
        };
        let target = match &self.remote_files_root {
            Ok(root) => {
                file_fetch::download_path(root, &host, &request.repo_name, &request.rel_path)
            }
            Err(err) => Err(format!("No folder for fetched files: {err}")),
        };
        let target = match target {
            Ok(target) => target,
            Err(reason) => {
                tracing::warn!(rel = %request.rel_path, "refusing a file fetch: {reason}");
                return Err(Refusal {
                    kind: ToastKind::Error,
                    title: failed_title(&name),
                    detail: Some(reason),
                });
            }
        };
        let repo_id = request.repo_id.clone();
        let worktree_path = request.worktree_path.clone();
        let rel_path = request.rel_path.clone();
        let toast_key = group.as_ref().map(|group| group.key().to_owned());
        let mut entry = FetchEntry::new(host.clone(), request, target);
        if let Some(group) = group {
            entry = entry.in_group(group);
        }
        if popup {
            entry = entry.for_popup();
        }
        let Ok(id) = self.fetches.insert(entry) else {
            tracing::info!(rel = %rel_path, "the file is already being fetched");
            return Err(Refusal {
                kind: ToastKind::Info,
                title: format!("Already fetching {name}"),
                detail: None,
            });
        };
        tracing::info!(%id, %host, rel = %rel_path, popup, "fetching a file");
        self.send(ClientMessage::FetchFile {
            id: id.clone(),
            repo_id,
            worktree_path,
            path: rel_path,
        });
        if !popup {
            self.push_toast_spec(
                ToastSpec {
                    kind: ToastKind::Info,
                    title: format!("Fetching {name} from {host}"),
                    detail: None,
                    key: toast_key,
                    sticky: false,
                    actions: Vec::new(),
                },
                cx,
            );
        }
        Ok(id)
    }

    /// The user cancelled the fetch `id`: the daemon is told to stop and
    /// the writer deletes the part file, with no toast. Returns `false`,
    /// leaving it running, once its last chunk reached the writer, which
    /// then saves it.
    pub(crate) fn cancel_fetch(&mut self, id: &str) -> bool {
        if self.fetches.get(id).is_some_and(FetchEntry::is_finishing) {
            tracing::info!(id, "a finishing fetch is not cancelled");
            return false;
        }
        let Some(entry) = self.fetches.remove(id) else {
            return true;
        };
        tracing::info!(id, rel = %entry.request().rel_path, "cancelled a file fetch");
        abort_writer(id, &entry);
        self.send(ClientMessage::CancelFetch { id: id.to_owned() });
        true
    }

    /// Redraws the Fetch file… dialog when the fetch `id` is the one it
    /// shows the progress of.
    fn redraw_popup_progress(&self, id: &str, cx: &mut Context<Self>) {
        if self
            .fetches
            .get(id)
            .is_some_and(FetchEntry::reports_to_popup)
        {
            cx.notify();
        }
    }

    /// The daemon could not fetch `id`: a reading of a link that never
    /// started moves on to the link's next reading; anything else fails.
    fn on_fetch_error(
        &mut self,
        id: &str,
        error: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.fetches.next_reading(id) {
            Some((next, group)) => {
                tracing::info!(id, next = %next.rel_path, "a link's reading is not on the host: {error}");
                self.start_fetch(next, Some(group), cx);
            }
            None => self.end_fetch(id, error, false, window, cx),
        }
    }

    /// Folds in a fetch's reply; returns whether it was one.
    pub(crate) fn on_fetch_message(
        &mut self,
        msg: &DaemonMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match msg {
            DaemonMessage::FileFetchStarted {
                id,
                resolved_path,
                size,
            } => {
                self.on_fetch_started(id, resolved_path, *size, window, cx);
                self.redraw_popup_progress(id, cx);
            }
            DaemonMessage::FileChunk { id, seq, data_b64 } => {
                self.on_fetch_chunk(id, *seq, data_b64, window, cx);
                self.redraw_popup_progress(id, cx);
            }
            DaemonMessage::FileFetchDone { id } => {
                self.on_fetch_done(id, window, cx);
                self.redraw_popup_progress(id, cx);
            }
            DaemonMessage::FileFetchError { id, error } => {
                self.on_fetch_error(id, error, window, cx);
            }
            _ => return false,
        }
        true
    }

    /// Starts the fetch's writer on a background thread.
    fn on_fetch_started(
        &mut self,
        id: &str,
        resolved_path: &str,
        size: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.fetches.get_mut(id) else {
            tracing::debug!(id, "a fetch the client no longer follows started");
            return;
        };
        if entry.writer().is_some() {
            self.end_fetch(id, "the transfer started twice", true, window, cx);
            return;
        }
        entry.on_started(size, resolved_path.to_owned());
        let (writer, steps) = unbounded();
        entry.set_writer(writer);
        let target = entry.target().to_path_buf();
        let write = cx.background_executor().spawn(file_fetch::run_writer(
            target,
            id.to_owned(),
            size,
            steps,
        ));
        let id = id.to_owned();
        cx.spawn_in(window, async move |this, cx| {
            let outcome = write.await;
            // Fails only when the view is gone, leaving nowhere to report.
            this.update_in(cx, |root, window, cx| {
                root.after_write(&id, outcome, window, cx);
            })
            .ok();
        })
        .detach();
    }

    fn on_fetch_chunk(
        &mut self,
        id: &str,
        seq: u64,
        data_b64: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.fetches.get_mut(id) else {
            tracing::debug!(id, seq, "a chunk for a fetch the client no longer follows");
            return;
        };
        let data = match base64::engine::general_purpose::STANDARD.decode(data_b64) {
            Ok(data) => data,
            Err(err) => {
                let reason = format!("chunk {seq} is not valid base64: {err}");
                self.end_fetch(id, &reason, true, window, cx);
                return;
            }
        };
        entry.on_chunk(data.len() as u64);
        let Some(writer) = entry.writer() else {
            let reason = "a chunk arrived before the transfer started";
            self.end_fetch(id, reason, true, window, cx);
            return;
        };
        if writer
            .unbounded_send(WriteStep::Chunk { seq, data })
            .is_err()
        {
            tracing::debug!(id, seq, "the fetch's writer already ended");
        }
    }

    fn on_fetch_done(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.fetches.get_mut(id) else {
            tracing::debug!(id, "a fetch the client no longer follows is done");
            return;
        };
        let Some(writer) = entry.writer() else {
            let reason = "the transfer ended before it started";
            self.end_fetch(id, reason, false, window, cx);
            return;
        };
        if writer.unbounded_send(WriteStep::Done).is_err() {
            tracing::debug!(id, "the fetch's writer already ended");
            return;
        }
        entry.mark_finishing();
    }

    /// The writer of the fetch `id` ended.
    fn after_write(
        &mut self,
        id: &str,
        outcome: WriteOutcome,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match outcome {
            WriteOutcome::Saved(path) => {
                tracing::info!(id, path = %path.display(), "fetched a file");
                // The dialog closes first, so a run confirm the open asks
                // for is not refused as a dialog over a dialog.
                if self.fetch_dialog_follows(id) {
                    self.close_fetch_dialog(window, cx);
                }
                let Some(entry) = self.fetches.remove(id) else {
                    return;
                };
                if entry.request().open_when_done {
                    self.open_fetched(path, entry.request(), window, cx);
                }
            }
            WriteOutcome::Failed(reason) => self.end_fetch(id, &reason, true, window, cx),
            WriteOutcome::Aborted => {}
        }
    }

    /// Ends the fetch `id` as failed for `reason`: its writer deletes the
    /// part file, the daemon is told to stop when `cancel` is set, and the
    /// Fetch file… dialog showing it, else a toast, says why.
    fn end_fetch(
        &mut self,
        id: &str,
        reason: &str,
        cancel: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.fetches.remove(id) else {
            tracing::debug!(id, "a fetch the client no longer follows failed: {reason}");
            return;
        };
        tracing::warn!(id, rel = %entry.request().rel_path, "fetching a file failed: {reason}");
        abort_writer(id, &entry);
        if cancel {
            self.send(ClientMessage::CancelFetch { id: id.to_owned() });
        }
        let detail = failure_detail(&entry, reason);
        if entry.reports_to_popup() && self.fetch_dialog_follows(id) {
            self.fail_fetch_dialog(detail, window, cx);
            return;
        }
        self.push_toast(
            ToastKind::Error,
            &failed_title(entry.file_name()),
            Some(detail),
            cx,
        );
    }

    /// The connection closed or moved to another daemon: every fetch still
    /// receiving ends, its part file deleted; one whose `Done` reached its
    /// writer ends by the writer's result. The Fetch file… dialog's own
    /// fetch says so in the dialog, which the connection overlay closes.
    pub(crate) fn end_fetches_on_disconnect(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for (id, entry) in self.fetches.take_unfinished() {
            let reason = format!("The connection to {} closed.", entry.host());
            tracing::warn!(%id, rel = %entry.request().rel_path, "fetching a file failed: {reason}");
            abort_writer(&id, &entry);
            if entry.reports_to_popup() && self.fetch_dialog_follows(&id) {
                self.fail_fetch_dialog(failure_detail(&entry, &reason), window, cx);
                continue;
            }
            self.push_toast(
                ToastKind::Error,
                &failed_title(entry.file_name()),
                Some(failure_detail(&entry, &reason)),
                cx,
            );
        }
    }

    /// Opens the fetched copy at `path` the way `request` asked.
    fn open_fetched(
        &mut self,
        path: PathBuf,
        request: &FetchRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let job = file_fetch::open_job(path, request.line, request.column);
        self.open_fetched_job(job, Then::Nothing, window, cx);
    }

    /// Opens `job`, which a fetch started, without the remote host's refusal
    /// of local opens; refuses a job that names no path under the folder for
    /// fetched files. The run confirm and the VS Code-missing fallback it
    /// leads to come back here.
    pub(crate) fn open_fetched_job(
        &mut self,
        job: OpenJob,
        then: Then,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = match &job {
            OpenJob::DefaultApp { path, .. }
            | OpenJob::VsCode { path, .. }
            | OpenJob::Reveal(path) => Some(path.as_path()),
            OpenJob::Url(_)
            | OpenJob::Folder(_)
            | OpenJob::VsCodeFolder(_)
            | OpenJob::VsCodeWorkspace(_) => None,
        };
        if !path.is_some_and(|path| self.is_fetched_copy(path)) {
            tracing::warn!(
                ?job,
                "refusing to open a fetched file outside the download folder"
            );
            self.push_toast(
                ToastKind::Error,
                COULD_NOT_OPEN,
                Some("Only files in the folder for fetched files open here.".to_owned()),
                cx,
            );
            return;
        }
        let route = Route {
            then,
            origin: Origin::Fetched,
        };
        self.run_open(job, route, window, cx);
    }

    /// Whether `path` is under the folder fetched files are saved in.
    fn is_fetched_copy(&self, path: &Path) -> bool {
        self.remote_files_root
            .as_ref()
            .is_ok_and(|root| is_under(root, path))
    }
}

/// Why a fetch was not started, as the toast that says so.
#[derive(Debug)]
pub(crate) struct Refusal {
    kind: ToastKind,
    title: String,
    detail: Option<String>,
}

impl Refusal {
    /// The refusal as one line, for the Fetch file… dialog.
    pub(crate) fn message(&self) -> &str {
        self.detail.as_deref().unwrap_or(&self.title)
    }
}

/// Tells the fetch's writer, if it started, to stop and delete its part.
fn abort_writer(id: &str, entry: &FetchEntry) {
    if let Some(writer) = entry.writer()
        && writer.unbounded_send(WriteStep::Abort).is_err()
    {
        tracing::debug!(id, "the fetch's writer already ended");
    }
}
