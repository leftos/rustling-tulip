//! Fetching a file from a remote host on the root: the request, the daemon's
//! replies fed to a writer off the UI thread, the toasts that say how it
//! went, and opening the saved copy.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use futures::channel::mpsc::unbounded;
use gpui::{Context, Window};
use protocol::{ClientMessage, DaemonMessage};

use crate::RootView;
use crate::file_fetch::{
    self, FetchEntry, FetchRequest, WriteOutcome, WriteStep, display_host_path, is_under,
};
use crate::notices::ToastKind;
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
        let name = file_fetch::file_name(&request.rel_path).to_owned();
        let Some(host) = file_fetch::host_folder(self.conn.target()) else {
            tracing::warn!(rel = %request.rel_path, "refusing a file fetch from the local daemon");
            self.push_toast(
                ToastKind::Warning,
                &failed_title(&name),
                Some("Files are fetched only from a remote host.".to_owned()),
                cx,
            );
            return None;
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
                self.push_toast(ToastKind::Error, &failed_title(&name), Some(reason), cx);
                return None;
            }
        };
        let repo_id = request.repo_id.clone();
        let worktree_path = request.worktree_path.clone();
        let rel_path = request.rel_path.clone();
        let Ok(id) = self
            .fetches
            .insert(FetchEntry::new(host.clone(), request, target))
        else {
            tracing::info!(rel = %rel_path, "the file is already being fetched");
            self.push_toast(
                ToastKind::Info,
                &format!("Already fetching {name}"),
                None,
                cx,
            );
            return None;
        };
        tracing::info!(%id, %host, rel = %rel_path, "fetching a file");
        self.send(ClientMessage::FetchFile {
            id: id.clone(),
            repo_id,
            worktree_path,
            path: rel_path,
        });
        self.push_toast(
            ToastKind::Info,
            &format!("Fetching {name} from {host}"),
            None,
            cx,
        );
        Some(id)
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
            } => self.on_fetch_started(id, resolved_path, *size, window, cx),
            DaemonMessage::FileChunk { id, seq, data_b64 } => {
                self.on_fetch_chunk(id, *seq, data_b64, cx);
            }
            DaemonMessage::FileFetchDone { id } => self.on_fetch_done(id, cx),
            DaemonMessage::FileFetchError { id, error } => self.end_fetch(id, error, false, cx),
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
            self.end_fetch(id, "the transfer started twice", true, cx);
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

    fn on_fetch_chunk(&mut self, id: &str, seq: u64, data_b64: &str, cx: &mut Context<Self>) {
        let Some(entry) = self.fetches.get_mut(id) else {
            tracing::debug!(id, seq, "a chunk for a fetch the client no longer follows");
            return;
        };
        let data = match base64::engine::general_purpose::STANDARD.decode(data_b64) {
            Ok(data) => data,
            Err(err) => {
                self.end_fetch(
                    id,
                    &format!("chunk {seq} is not valid base64: {err}"),
                    true,
                    cx,
                );
                return;
            }
        };
        entry.on_chunk(data.len() as u64);
        let Some(writer) = entry.writer() else {
            self.end_fetch(id, "a chunk arrived before the transfer started", true, cx);
            return;
        };
        if writer
            .unbounded_send(WriteStep::Chunk { seq, data })
            .is_err()
        {
            tracing::debug!(id, seq, "the fetch's writer already ended");
        }
    }

    fn on_fetch_done(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(entry) = self.fetches.get_mut(id) else {
            tracing::debug!(id, "a fetch the client no longer follows is done");
            return;
        };
        let Some(writer) = entry.writer() else {
            self.end_fetch(id, "the transfer ended before it started", false, cx);
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
                let Some(entry) = self.fetches.remove(id) else {
                    return;
                };
                if entry.request().open_when_done {
                    self.open_fetched(path, entry.request(), window, cx);
                }
            }
            WriteOutcome::Failed(reason) => self.end_fetch(id, &reason, true, cx),
            WriteOutcome::Aborted => {}
        }
    }

    /// Ends the fetch `id` as failed for `reason`: its writer deletes the
    /// part file, the daemon is told to stop when `cancel` is set, and a
    /// toast says why.
    fn end_fetch(&mut self, id: &str, reason: &str, cancel: bool, cx: &mut Context<Self>) {
        let Some(entry) = self.fetches.remove(id) else {
            tracing::debug!(id, "a fetch the client no longer follows failed: {reason}");
            return;
        };
        tracing::warn!(id, rel = %entry.request().rel_path, "fetching a file failed: {reason}");
        abort_writer(id, &entry);
        if cancel {
            self.send(ClientMessage::CancelFetch { id: id.to_owned() });
        }
        self.push_toast(
            ToastKind::Error,
            &failed_title(entry.file_name()),
            Some(failure_detail(&entry, reason)),
            cx,
        );
    }

    /// The connection closed or moved to another daemon: every fetch still
    /// receiving ends, its part file deleted; one whose `Done` reached its
    /// writer ends by the writer's result.
    pub(crate) fn end_fetches_on_disconnect(&mut self, cx: &mut Context<Self>) {
        for (id, entry) in self.fetches.take_unfinished() {
            let reason = format!("The connection to {} closed.", entry.host());
            tracing::warn!(%id, rel = %entry.request().rel_path, "fetching a file failed: {reason}");
            abort_writer(&id, &entry);
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

/// Tells the fetch's writer, if it started, to stop and delete its part.
fn abort_writer(id: &str, entry: &FetchEntry) {
    if let Some(writer) = entry.writer()
        && writer.unbounded_send(WriteStep::Abort).is_err()
    {
        tracing::debug!(id, "the fetch's writer already ended");
    }
}
