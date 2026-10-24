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
    self, FetchEntry, FetchRequest, MediaEnd, ProbeGroup, WriteOutcome, WriteStep,
    display_host_path, is_under,
};
use crate::html_media;
use crate::links::TerminalLink;
use crate::notices::{ToastKind, ToastSpec};
use crate::open::{OpenAction, OpenJob};
use crate::open_view::{COULD_NOT_OPEN, Origin, Route, Then};

/// The detail of the toast refusing a fetched file's open of a path outside
/// the folder for fetched files.
pub(crate) const OUTSIDE_FETCH_FOLDER: &str =
    "Only files in the folder for fetched files open here.";

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
        self.remote_files_root = Ok(absolute_root(root));
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
        let (host, target) = self.fetch_target(&request)?;
        let repo_id = request.repo_id.clone();
        let worktree_path = request.worktree_path.clone();
        let rel_path = request.rel_path.clone();
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
                    key: Some(fetching_toast_key(
                        &id,
                        self.fetches.get(&id).and_then(FetchEntry::group),
                    )),
                    sticky: false,
                    actions: Vec::new(),
                },
                cx,
            );
        }
        Ok(id)
    }

    /// The download folder of this host and where `request`'s file lands in
    /// it.
    ///
    /// # Errors
    /// The [`Refusal`] for the local daemon, a refused path or a missing
    /// download folder.
    fn fetch_target(&self, request: &FetchRequest) -> Result<(String, PathBuf), Refusal> {
        let name = file_fetch::file_name(&request.rel_path);
        let Some(host) = file_fetch::host_folder(self.conn.target()) else {
            tracing::warn!(rel = %request.rel_path, "refusing a file fetch from the local daemon");
            return Err(Refusal {
                kind: ToastKind::Warning,
                title: failed_title(name),
                detail: Some("Files are fetched only from a remote host.".to_owned()),
            });
        };
        let target = match &self.remote_files_root {
            Ok(root) => {
                file_fetch::download_path(root, &host, &request.repo_name, &request.rel_path)
            }
            Err(err) => Err(format!("No folder for fetched files: {err}")),
        };
        match target {
            Ok(target) => Ok((host, target)),
            Err(reason) => {
                tracing::warn!(rel = %request.rel_path, "refusing a file fetch: {reason}");
                Err(Refusal {
                    kind: ToastKind::Error,
                    title: failed_title(name),
                    detail: Some(reason),
                })
            }
        }
    }

    /// Starts the media fetch `request` for the page `page_id`, quietly: a
    /// refused path fails it, and a file already fetching counts as landed.
    fn start_child(&mut self, page_id: &str, request: FetchRequest) {
        let (host, target) = match self.fetch_target(&request) {
            Ok(found) => found,
            Err(refusal) => {
                tracing::warn!(
                    page = page_id,
                    rel = %request.rel_path,
                    "a page's media file is refused: {}",
                    refusal.message()
                );
                self.fetches.child_ended(page_id, MediaEnd::Failed);
                return;
            }
        };
        let repo_id = request.repo_id.clone();
        let worktree_path = request.worktree_path.clone();
        let rel_path = request.rel_path.clone();
        let Some(id) = self.fetches.insert_child(page_id, host, request, target) else {
            tracing::info!(page = page_id, rel = %rel_path, "a page's media file is already fetching");
            return;
        };
        tracing::info!(%id, page = page_id, rel = %rel_path, "fetching a page's media file");
        self.send(ClientMessage::FetchFile {
            id,
            repo_id,
            worktree_path,
            path: rel_path,
        });
    }

    /// Starts the page `page_id`'s next media fetches; opens the page once
    /// every one has ended, else updates its "Fetching" toast's count.
    fn pump_batch(&mut self, page_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .fetches
            .get(page_id)
            .is_some_and(FetchEntry::connection_lost)
        {
            tracing::debug!(page = page_id, "no media are fetched on a lost connection");
            return;
        }
        loop {
            let children = self.fetches.next_children(page_id);
            if children.is_empty() {
                break;
            }
            for request in children {
                self.start_child(page_id, request);
            }
        }
        if let Some(page) = self.fetches.take_finished_batch(page_id) {
            self.finish_batch(page_id, &page, window, cx);
            return;
        }
        let Some(page) = self.fetches.get(page_id) else {
            return;
        };
        let Some(batch) = page.batch() else {
            return;
        };
        let spec = ToastSpec {
            kind: ToastKind::Info,
            title: format!(
                "Fetching {} and its media ({} of {})",
                page.file_name(),
                batch.ended(),
                batch.total()
            ),
            detail: None,
            key: Some(fetching_toast_key(page_id, page.group())),
            sticky: false,
            actions: Vec::new(),
        };
        self.push_toast_spec(spec, cx);
    }

    /// Every media fetch of `page` ended: one toast says how many failed or
    /// were skipped, if any did, and the page opens.
    fn finish_batch(
        &mut self,
        page_id: &str,
        page: &FetchEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(batch) = page.batch() else {
            return;
        };
        tracing::info!(
            page = page_id,
            total = batch.total(),
            missed = batch.missed(),
            "a page's media fetches ended"
        );
        if batch.missed() > 0 {
            self.push_page_toast(
                page_id,
                page,
                ToastKind::Warning,
                format!("Some of {}'s media didn't load", page.file_name()),
                Some(format!(
                    "{} of {} files failed or were skipped.",
                    batch.missed(),
                    batch.total()
                )),
                cx,
            );
        } else {
            self.push_page_toast(
                page_id,
                page,
                ToastKind::Info,
                format!("Fetched {} and its media", page.file_name()),
                None,
                cx,
            );
        }
        self.open_fetched(batch.saved().to_path_buf(), page.request(), window, cx);
    }

    /// Shows a toast about the page `page_id` in place of its "Fetching"
    /// toast.
    fn push_page_toast(
        &mut self,
        page_id: &str,
        page: &FetchEntry,
        kind: ToastKind,
        title: String,
        detail: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let spec = ToastSpec {
            kind,
            title,
            detail,
            key: Some(fetching_toast_key(page_id, page.group())),
            sticky: false,
            actions: Vec::new(),
        };
        self.push_toast_spec(spec, cx);
    }

    /// The page `id`, saved at `path` after its connection closed, opens
    /// without its media, and a toast says so.
    fn open_page_without_media(
        &mut self,
        id: &str,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(page) = self.fetches.remove(id) else {
            return;
        };
        tracing::warn!(
            id,
            "a page saved after its connection closed opens without its media"
        );
        self.push_page_toast(
            id,
            &page,
            ToastKind::Warning,
            lost_media_title(page.file_name()),
            Some(LOST_MEDIA_DETAIL.to_owned()),
            cx,
        );
        self.open_fetched(path, page.request(), window, cx);
    }

    /// The page `id` is saved at `path`: it waits while its media are read
    /// off the UI thread, then fetches them.
    fn scan_saved_page(
        &mut self,
        id: &str,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(page) = self.fetches.get_mut(id) else {
            return;
        };
        page.await_media(path.clone());
        let rel = page.request().rel_path.clone();
        let read = cx
            .background_executor()
            .spawn(async move { read_media(&path, &rel, html_media::page_media) });
        let id = id.to_owned();
        cx.spawn_in(window, async move |this, cx| {
            let (media, skipped) = read.await;
            // Fails only when the view is gone, leaving nowhere to report.
            this.update_in(cx, |root, window, cx| {
                root.fetches.page_scanned(&id, media, skipped);
                root.pump_batch(&id, window, cx);
            })
            .ok();
        })
        .detach();
    }

    /// A media file of the page `page_id` is saved at `path`: a stylesheet
    /// the page names is read for its own media first.
    fn after_child_saved(
        &mut self,
        page_id: String,
        rel: &str,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !(html_media::is_stylesheet(rel) && self.fetches.named_by_page(&page_id, rel)) {
            self.fetches.child_ended(&page_id, MediaEnd::Landed);
            self.pump_batch(&page_id, window, cx);
            return;
        }
        let rel = rel.to_owned();
        let read = cx
            .background_executor()
            .spawn(async move { read_media(&path, &rel, html_media::stylesheet_media) });
        cx.spawn_in(window, async move |this, cx| {
            let (media, skipped) = read.await;
            // Fails only when the view is gone, leaving nowhere to report.
            this.update_in(cx, |root, window, cx| {
                root.fetches.add_media(&page_id, media, skipped);
                root.fetches.child_ended(&page_id, MediaEnd::Landed);
                root.pump_batch(&page_id, window, cx);
            })
            .ok();
        })
        .detach();
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
        if let Some(page_id) = entry.parent().map(str::to_owned)
            && !self.fetches.admit_child(&page_id, size)
        {
            tracing::info!(id, size, page = %page_id, "a page's media file is over a size cap; cancelling it");
            self.cancel_fetch(id);
            self.fetches.child_ended(&page_id, MediaEnd::Skipped);
            self.pump_batch(&page_id, window, cx);
            return;
        }
        let Some(entry) = self.fetches.get_mut(id) else {
            return;
        };
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
                let waits_on_media = self.fetches.get(id).is_some_and(|entry| {
                    entry.request().open_when_done
                        && html_media::is_html_page(&entry.request().rel_path)
                });
                if waits_on_media {
                    if self
                        .fetches
                        .get(id)
                        .is_some_and(FetchEntry::connection_lost)
                    {
                        self.open_page_without_media(id, path, window, cx);
                    } else {
                        self.scan_saved_page(id, path, window, cx);
                    }
                    return;
                }
                let Some(entry) = self.fetches.remove(id) else {
                    return;
                };
                if let Some(page_id) = entry.parent() {
                    let rel = &entry.request().rel_path;
                    self.after_child_saved(page_id.to_owned(), rel, path, window, cx);
                } else if entry.request().open_when_done {
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
        if let Some(page_id) = entry.parent() {
            self.fetches.child_ended(page_id, MediaEnd::Failed);
            self.pump_batch(page_id, window, cx);
            return;
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
    /// fetch says so in the dialog, which the connection overlay closes. A
    /// saved page waiting on its media does not open, and one toast says so
    /// for the page and its media fetches.
    pub(crate) fn end_fetches_on_disconnect(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for (id, entry) in self.fetches.take_unfinished() {
            let reason = format!("The connection to {} closed.", entry.host());
            tracing::warn!(%id, rel = %entry.request().rel_path, "fetching a file failed: {reason}");
            abort_writer(&id, &entry);
            if entry.parent().is_some() {
                continue;
            }
            if entry.batch().is_some() {
                self.push_page_toast(
                    &id,
                    &entry,
                    ToastKind::Error,
                    lost_media_title(entry.file_name()),
                    Some(LOST_MEDIA_DETAIL.to_owned()),
                    cx,
                );
                continue;
            }
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

    /// Opens the fetched copy at `path` the way `request` asked: a text
    /// file in the viewer at its line, anything else by the `:line` rule.
    fn open_fetched(
        &mut self,
        path: PathBuf,
        request: &FetchRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let action = match request.line {
            Some(line) => OpenAction::VsCode {
                path,
                line,
                column: request.column,
            },
            None => OpenAction::DefaultApp(path),
        };
        self.open_text_or_external(action, Origin::Fetched, window, cx);
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
                Some(OUTSIDE_FETCH_FOLDER.to_owned()),
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
    pub(crate) fn is_fetched_copy(&self, path: &Path) -> bool {
        self.remote_files_root
            .as_ref()
            .is_ok_and(|root| is_under(root, path))
    }
}

/// The download root `root` made absolute against the current folder, so
/// it compares with a viewer tab's absolute path; kept as given, with a
/// warning, when that fails.
pub(crate) fn absolute_root(root: PathBuf) -> PathBuf {
    match std::path::absolute(&root) {
        Ok(absolute) => absolute,
        Err(err) => {
            tracing::warn!(
                root = %root.display(),
                "the download root could not be made absolute, so it is used as given: {err}"
            );
            root
        }
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

/// The key of the "Fetching" toast of the fetch `id`: its link's `group`'s,
/// else its own id.
fn fetching_toast_key(id: &str, group: Option<&ProbeGroup>) -> String {
    group.map_or_else(|| id.to_owned(), |group| group.key().to_owned())
}

/// The detail of the toast about a page whose media the lost connection
/// stopped.
const LOST_MEDIA_DETAIL: &str = "The connection was lost.";

/// The title of the toast about a page whose media the lost connection
/// stopped.
fn lost_media_title(page_name: &str) -> String {
    format!("Couldn't fetch {page_name}'s media")
}

/// The media the saved file at `path` (repo-relative `rel`) names, by
/// `scan`; a file that cannot be read names none.
fn read_media(
    path: &Path,
    rel: &str,
    scan: fn(&str, &str) -> (Vec<String>, usize),
) -> (Vec<String>, usize) {
    match std::fs::read(path) {
        Ok(bytes) => scan(rel, &String::from_utf8_lossy(&bytes)),
        Err(err) => {
            tracing::warn!(path = %path.display(), "could not read a fetched file for its media: {err}");
            (Vec::new(), 0)
        }
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

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use std::path::PathBuf;

    use super::absolute_root;

    #[test]
    fn a_relative_remote_files_root_is_made_absolute() {
        let cwd = std::env::current_dir().expect("the test has a current folder");
        let root = absolute_root(PathBuf::from("downloads"));
        assert!(root.is_absolute(), "{}", root.display());
        assert_eq!(root, cwd.join("downloads"));

        let absolute = cwd.join("elsewhere");
        assert_eq!(absolute_root(absolute.clone()), absolute, "kept as given");
    }
}
