//! Fetching a file from a remote host on the root: the request, the daemon's
//! replies fed to a writer off the UI thread, the toasts that say how it
//! went, and opening the saved copy.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use base64::Engine as _;
use futures::channel::mpsc::unbounded;
use gpui::{Context, Window};
use protocol::{ClientMessage, DaemonMessage, PathKind, SessionFolder};

use crate::RootView;
use crate::fetch_link;
use crate::file_fetch::{
    self, DoneAction, FetchEntry, FetchRequest, MediaEnd, ProbeGroup, WriteOutcome, WriteStep,
    Writer, display_host_path, is_under_resolved,
};
use crate::folder_unpack::{self, Announced};
use crate::html_media;
use crate::links::TerminalLink;
use crate::notices::{ToastAction, ToastKind, ToastSpec};
use crate::open::{LinkAction, OpenAction, OpenJob};
use crate::open_view::{COULD_NOT_OPEN, Opening, Origin, Route, Then};
use crate::spawn_form::human_size;
use crate::target::HostKey;

/// The detail of the toast refusing a fetched file's open of a path outside
/// the folder for fetched files.
pub(crate) const OUTSIDE_FETCH_FOLDER: &str =
    "Only files in the folder for fetched files open here.";

/// The Cancel on a file's or folder fetch's toast.
pub(crate) const FETCH_CANCEL: ToastAction = ToastAction {
    id: "cancel",
    label: "Cancel",
    danger: false,
};

/// The Fetch whole folder on a fetched file's done toast.
pub(crate) const FETCH_WHOLE_FOLDER: ToastAction = ToastAction {
    id: "whole-folder",
    label: "Fetch whole folder",
    danger: false,
};

/// The Reveal on a fetched folder's done toast.
pub(crate) const FETCH_REVEAL: ToastAction = ToastAction {
    id: "reveal",
    label: "Reveal",
    danger: false,
};

/// The detail of the toast refusing a link's fetch, outside every repo or
/// worktree of its session.
const NOT_INSIDE: &str = "Not inside a repo or worktree of this session.";

/// How often a finishing folder fetch's toast shows the unpacked bytes.
const FINISHING_TICK: Duration = Duration::from_millis(250);

/// The title of the toast about a fetch that failed.
fn failed_title(file_name: &str) -> String {
    format!("Couldn't fetch {file_name}")
}

/// The detail of the toast about a folder the host `host` is too old to
/// send.
fn too_old_detail(host: &str) -> String {
    format!(
        "{host} runs an older rustling-tulip that cannot send folders. Update it to fetch folders."
    )
}

/// Why a fetch failed, after the host's path of the file once the daemon
/// named it.
fn failure_detail(entry: &FetchEntry, reason: &str) -> String {
    match entry.resolved_path() {
        Some(resolved) => format!("{}: {reason}", display_host_path(resolved)),
        None => reason.to_owned(),
    }
}

/// Where a held folder unpacker waits: before it starts unpacking, and
/// after it ended, before it reports how.
#[derive(Debug, Default)]
pub(crate) struct UnpackGate {
    released: Mutex<Released>,
    changed: Condvar,
}

/// Which of a held unpacker's two waits are released.
#[derive(Debug, Default, Clone, Copy)]
struct Released {
    start: bool,
    outcome: bool,
}

impl UnpackGate {
    /// Releases the waits `set` marks, waking every waiting unpacker.
    fn release(&self, set: impl FnOnce(&mut Released)) {
        let mut released = self.released.lock().unwrap_or_else(PoisonError::into_inner);
        set(&mut released);
        self.changed.notify_all();
    }

    /// Blocks the unpacker's thread until `passes` holds.
    fn wait(&self, passes: impl Fn(Released) -> bool) {
        let mut released = self.released.lock().unwrap_or_else(PoisonError::into_inner);
        while !passes(*released) {
            released = self
                .changed
                .wait(released)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

/// A spec's hold on the folder unpackers started while it lives: each waits
/// before it starts until [`Self::release_start`], and before it reports
/// until [`Self::release_outcome`]. Dropping it releases both, so no
/// unpacker is left waiting.
#[doc(hidden)]
#[derive(Debug)]
pub struct UnpackHold(Arc<UnpackGate>);

impl UnpackHold {
    /// Lets the held unpackers start.
    pub fn release_start(&self) {
        self.0.release(|released| released.start = true);
    }

    /// Lets the held unpackers report how they ended.
    pub fn release_outcome(&self) {
        self.0.release(|released| released.outcome = true);
    }
}

impl Drop for UnpackHold {
    fn drop(&mut self) {
        self.0.release(|released| {
            released.start = true;
            released.outcome = true;
        });
    }
}

impl RootView {
    /// Saves fetched files under `root` instead of the data folder.
    pub fn set_remote_files_root(&mut self, root: PathBuf) {
        self.remote_files_root = Ok(absolute_root(root));
    }

    /// Holds every folder unpacker started from now on, for a spec to look
    /// at a fetch between its last step and its outcome.
    #[doc(hidden)]
    pub fn hold_folder_unpackers(&mut self) -> UnpackHold {
        let gate = Arc::new(UnpackGate::default());
        self.fetches.hold_unpackers(Arc::clone(&gate));
        UnpackHold(gate)
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

    /// Asks the remote host for `request`'s folder, unpacked under this
    /// host's download folder, without asking first what the path names;
    /// returns the fetch's id, or `None` when the fetch is refused (the
    /// local daemon, a refused path, the same folder already fetching),
    /// which a toast explains.
    pub fn fetch_remote_folder(
        &mut self,
        request: FetchRequest,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        match self.try_start_folder_fetch(request, cx) {
            Ok(id) => Some(id),
            Err(refusal) => {
                self.push_toast(refusal.kind, &refusal.title, refusal.detail, cx);
                None
            }
        }
    }

    /// Fetch whole folder on the done toast keyed `key`: the toast goes and
    /// the fetched file's folder is fetched, or, when its host no longer
    /// sends folders, a warning says the host is too old.
    pub(crate) fn fetch_whole_folder(&mut self, key: &str, cx: &mut Context<Self>) {
        let Some(DoneAction::WholeFolder(request)) = self.fetches.done_action(key).cloned() else {
            return;
        };
        self.dismiss_toast_key(key, cx);
        let name = file_fetch::fetch_name(&request).to_owned();
        let conn = self.conn_of(&request.host);
        if !conn.is_some_and(crate::connection::Connection::is_open) {
            let host = conn.map_or_else(
                || "The host".to_owned(),
                |conn| file_fetch::host_label(conn.target()),
            );
            tracing::info!(host = ?request.host, "the folder's host is offline");
            self.push_toast(
                ToastKind::Warning,
                &failed_title(&name),
                Some(format!("{host} is offline.")),
                cx,
            );
            return;
        }
        if !self.host_sends_folders(&request.host) {
            tracing::info!(host = ?request.host, "the host no longer sends folders");
            self.push_too_old(&request.host, &name, cx);
            return;
        }
        self.fetch_remote_folder(request, cx);
    }

    /// Reveal on the done toast keyed `key`: the fetched folder shows in
    /// Explorer, and the toast stays.
    pub(crate) fn reveal_fetched_folder(
        &mut self,
        key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(DoneAction::Reveal(path)) = self.fetches.done_action(key).cloned() else {
            return;
        };
        self.open_fetched_job(OpenJob::Reveal(path), Then::Nothing, window, cx);
    }

    /// A Ctrl+click on a path link in a pane of remote host `host`: fetches
    /// the first of the link's readings under the repos and worktrees of
    /// `session` as that host lists it, trying them one at a time; the copy
    /// opens as `action` asks. A link that may lie outside those folders is
    /// first asked of the host, which lists every folder
    /// `git worktree list` names for them.
    pub(crate) fn fetch_path_link(
        &mut self,
        link: &TerminalLink,
        host: &HostKey,
        session: Option<&str>,
        action: LinkAction,
        cx: &mut Context<Self>,
    ) {
        let found =
            session.and_then(|id| self.sidebar.sessions_of(host).iter().find(|s| s.id == id));
        let cwd = found.and_then(|session| session.current_cwd.clone());
        let mut readings = found
            .map(|session| {
                fetch_link::session_readings(
                    host,
                    &link.candidates,
                    session,
                    self.sidebar.repos_of(host),
                )
            })
            .unwrap_or_default();
        let own_roots = found
            .map(|session| {
                fetch_link::roots(
                    &session.members,
                    cwd.as_deref(),
                    self.sidebar.repos_of(host),
                )
            })
            .unwrap_or_default();
        let asks = found.is_some_and(|_| {
            self.conn_of(host)
                .is_some_and(|conn| conn.supports(protocol::FEATURE_SESSION_FOLDERS))
        });
        let leaves =
            asks && fetch_link::link_leaves_roots(&link.candidates, cwd.as_deref(), &own_roots);
        if leaves && let Some(session) = session {
            self.ask_folders(
                file_fetch::PendingAsk {
                    host: host.clone(),
                    candidates: link.candidates.clone(),
                    target: link.target.clone(),
                    cwd,
                    own_roots,
                    action,
                },
                session,
                cx,
            );
            return;
        }
        if readings.is_empty() {
            tracing::warn!(
                target = %link.target,
                session = session.unwrap_or("none"),
                "no repo or worktree of the session holds the terminal path link"
            );
            self.push_toast(
                ToastKind::Warning,
                &failed_title(file_fetch::file_name(&link.target)),
                Some(NOT_INSIDE.to_owned()),
                cx,
            );
            return;
        }
        for reading in &mut readings {
            reading.action = action;
        }
        self.start_link_readings(readings, host, cx);
    }

    /// Records a link's ask `ask` under a new id, asks `host` for its
    /// session's folders, and shows the fetch's toast keyed by the ask; its
    /// reply is [`Self::on_session_folders`]'s.
    fn ask_folders(&mut self, ask: file_fetch::PendingAsk, session: &str, cx: &mut Context<Self>) {
        let name = file_fetch::file_name(&ask.target).to_owned();
        let host = ask.host.clone();
        let id = self.fetches.begin_folder_ask(ask);
        self.send_to(
            &host,
            ClientMessage::ListSessionFolders {
                id: id.clone(),
                session_id: session.to_owned(),
            },
        );
        let title = format!("Fetching {name} from {}", self.host_label_of(&host));
        self.push_toast_spec(
            ToastSpec {
                kind: ToastKind::Info,
                title,
                detail: None,
                key: Some(id),
                sticky: false,
                actions: Vec::new(),
            },
            cx,
        );
    }

    /// Fetches the link whose `readings` these are, on `host`: a host that
    /// sends folders is asked about the first reading, and an older one
    /// fetches it as a file or is told the folder it names is beyond it.
    fn start_link_readings(
        &mut self,
        readings: Vec<FetchRequest>,
        host: &HostKey,
        cx: &mut Context<Self>,
    ) {
        if self.host_sends_folders(host) {
            let Some((id, first)) = self.fetches.begin_stat(host.clone(), readings) else {
                return;
            };
            // The toast shows at once, keyed by the stat: the file fetch
            // updates it in place, and a folder's toast or a warning
            // replaces it.
            let title = format!(
                "Fetching {} from {}",
                file_fetch::fetch_name(&first),
                self.host_label_of(host)
            );
            self.push_toast_spec(
                ToastSpec {
                    kind: ToastKind::Info,
                    title,
                    detail: None,
                    key: Some(id.clone()),
                    sticky: false,
                    actions: Vec::new(),
                },
                cx,
            );
            self.send_stat(host, id, &first);
            return;
        }
        let Some(root) = readings.first().map(|r| r.repo_name.clone()) else {
            return;
        };
        if !self.start_file_flow(readings, None, cx) {
            tracing::info!(host = ?host, "the host cannot send the folder a link names");
            self.push_too_old(host, &root, cx);
        }
    }

    /// The host's answer to a link's ask for its session's folders: the
    /// link's readings over every folder it lists, fetched as the direct
    /// route would. Returns whether `id` was an ask still waiting; a reply
    /// that arrives after the connection closed finds none.
    fn on_session_folders(
        &mut self,
        id: &str,
        folders: &[SessionFolder],
        error: Option<&str>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(ask) = self.fetches.take_folder_ask(id) else {
            return false;
        };
        self.dismiss_toast_key(id, cx);
        let roots: Vec<fetch_link::Root> = folders.iter().map(fetch_link::Root::from).collect();
        let mut readings = fetch_link::readings(
            &ask.host,
            &ask.candidates,
            ask.cwd.as_deref(),
            &roots,
            &ask.own_roots,
        );
        if readings.is_empty() {
            let label = self.host_label_of(&ask.host);
            let detail = file_fetch::outside_detail(
                folders,
                error,
                &label,
                &file_fetch::display_host_path(&ask.target),
            );
            tracing::warn!(
                target = %ask.target,
                listing_error = error.unwrap_or("none"),
                "no folder the host lists holds the terminal path link"
            );
            self.push_toast(
                ToastKind::Warning,
                &failed_title(file_fetch::file_name(&ask.target)),
                Some(detail),
                cx,
            );
            return true;
        }
        for reading in &mut readings {
            reading.action = ask.action;
        }
        self.start_link_readings(readings, &ask.host, cx);
        true
    }

    /// `host` as fetch toasts name it ([`file_fetch::host_label`]); "The
    /// host" when it has no connection.
    fn host_label_of(&self, host: &HostKey) -> String {
        self.conn_of(host).map_or_else(
            || "The host".to_owned(),
            |conn| file_fetch::host_label(conn.target()),
        )
    }

    /// Whether `host`'s daemon fetches folders.
    fn host_sends_folders(&self, host: &HostKey) -> bool {
        self.conn_of(host)
            .is_some_and(|conn| conn.supports(protocol::FEATURE_FOLDER_FETCH))
    }

    /// Says that `host` is too old to send the folder `name`.
    fn push_too_old(&mut self, host: &HostKey, name: &str, cx: &mut Context<Self>) {
        let label = self.host_label_of(host);
        self.push_toast(
            ToastKind::Warning,
            &failed_title(name),
            Some(too_old_detail(&label)),
            cx,
        );
    }

    /// Fetches the first of a link's `readings` that names a file, trying
    /// the ones after it one at a time; a reading of the root itself is
    /// never fetched as a file. `false` when no reading names a file. With
    /// `key`, the link's stat whose toast already shows, the fetch updates
    /// that toast in place, and a refused fetch takes it away.
    fn start_file_flow(
        &mut self,
        readings: Vec<FetchRequest>,
        key: Option<&str>,
        cx: &mut Context<Self>,
    ) -> bool {
        let mut files: Vec<FetchRequest> = readings
            .into_iter()
            .filter(|reading| !reading.rel_path.is_empty())
            .collect();
        if files.is_empty() {
            return false;
        }
        let first = files.remove(0);
        let group = match key {
            Some(key) => self.fetches.new_group_keyed(key.to_owned(), files),
            None => self.fetches.new_group(files),
        };
        if self.start_fetch(first, Some(group), cx).is_none()
            && let Some(key) = key
        {
            self.dismiss_toast_key(key, cx);
        }
        true
    }

    /// Asks `host` what `reading` names, for the stat `id`.
    fn send_stat(&self, host: &HostKey, id: String, reading: &FetchRequest) {
        tracing::info!(%id, rel = %reading.rel_path, "asking the host what a link names");
        self.send_to(
            host,
            ClientMessage::StatPath {
                id,
                repo_id: reading.repo_id.clone(),
                worktree_path: reading.worktree_path.clone(),
                path: reading.rel_path.clone(),
            },
        );
    }

    /// The host said what the stat `id`'s reading names: a folder is
    /// fetched as one, a missing path moves on to the next reading, and
    /// anything else is fetched as a file. Returns whether `id` was a stat.
    fn on_path_stat(&mut self, id: &str, kind: PathKind, cx: &mut Context<Self>) -> bool {
        let Some(stat) = self.fetches.get_stat(id) else {
            return false;
        };
        let host = stat.host().clone();
        match kind {
            PathKind::Dir => {
                let Some(stat) = self.fetches.take_stat(id) else {
                    return true;
                };
                // The folder's own toast, or its refusal, takes over.
                self.dismiss_toast_key(id, cx);
                if let Err(refusal) = self.try_start_folder_fetch(stat.reading().clone(), cx) {
                    self.push_toast(refusal.kind, &refusal.title, refusal.detail, cx);
                }
            }
            PathKind::Missing => {
                if let Some(next) = self.fetches.next_stat(id) {
                    self.send_stat(&host, id.to_owned(), &next);
                    return true;
                }
                let Some(stat) = self.fetches.take_stat(id) else {
                    return true;
                };
                tracing::info!(
                    id,
                    "no reading of the link is on the host; fetching as files"
                );
                self.start_files_or_warn(stat.readings().to_vec(), id, cx);
            }
            PathKind::File | PathKind::Other | PathKind::Unknown => {
                let Some(stat) = self.fetches.take_stat(id) else {
                    return true;
                };
                let mut readings = vec![stat.reading().clone()];
                readings.extend(stat.rest().iter().cloned());
                self.start_files_or_warn(readings, id, cx);
            }
        }
        true
    }

    /// [`Self::start_file_flow`] over `readings`; when none names a file, a
    /// warning titled by the first reading says why: a link that reads only
    /// as repo or worktree roots names a folder missing on the host, and
    /// any other link is outside the session's repos and worktrees. The
    /// toast of the link's stat `key` becomes the fetch's, or gives way to
    /// the warning.
    fn start_files_or_warn(
        &mut self,
        readings: Vec<FetchRequest>,
        key: &str,
        cx: &mut Context<Self>,
    ) {
        let name = readings
            .first()
            .map(file_fetch::fetch_name)
            .unwrap_or_default()
            .to_owned();
        let root_host = readings
            .iter()
            .all(|reading| reading.rel_path.is_empty())
            .then(|| readings.first().map(|reading| reading.host.clone()))
            .flatten();
        if !self.start_file_flow(readings, Some(key), cx) {
            self.dismiss_toast_key(key, cx);
            let detail = match root_host {
                Some(host) => format!("{name} is not on {}.", self.host_label_of(&host)),
                None => NOT_INSIDE.to_owned(),
            };
            self.push_toast(ToastKind::Warning, &failed_title(&name), Some(detail), cx);
        }
    }

    /// Asks the host for `request`'s folder and returns the fetch's id; its
    /// toast shows the progress and offers Cancel.
    ///
    /// # Errors
    /// The [`Refusal`] when the fetch is refused: the local daemon, a
    /// refused path or download folder, the same folder already fetching.
    fn try_start_folder_fetch(
        &mut self,
        request: FetchRequest,
        cx: &mut Context<Self>,
    ) -> Result<String, Refusal> {
        let name = file_fetch::fetch_name(&request).to_owned();
        let (host_key, host, target) =
            self.fetch_target_with(&request, file_fetch::folder_download_path)?;
        let repo_id = request.repo_id.clone();
        let worktree_path = request.worktree_path.clone();
        let rel_path = request.rel_path.clone();
        let entry = FetchEntry::new(host_key.clone(), host.clone(), request, target).for_folder();
        let Ok(id) = self.fetches.insert(entry) else {
            tracing::info!(rel = %rel_path, "the folder is already being fetched");
            return Err(Refusal {
                kind: ToastKind::Info,
                title: format!("Already fetching {name}"),
                detail: None,
            });
        };
        tracing::info!(%id, %host, rel = %rel_path, "fetching a folder");
        self.send_to(
            &host_key,
            ClientMessage::FetchFolder {
                id: id.clone(),
                repo_id,
                worktree_path,
                path: rel_path,
            },
        );
        self.push_toast_spec(
            ToastSpec {
                kind: ToastKind::Info,
                title: format!("Fetching folder {name} from {host}"),
                detail: None,
                key: Some(id.clone()),
                sticky: true,
                actions: vec![FETCH_CANCEL],
            },
            cx,
        );
        Ok(id)
    }

    /// Shows the folder fetch `id`'s counts and unpacked bytes on its
    /// toast; with `on_change`, only when the shown bytes changed. Once the
    /// user closed that toast it stays closed.
    fn show_folder_progress(&mut self, id: &str, on_change: bool, cx: &mut Context<Self>) {
        let Some(entry) = self.fetches.get_mut(id) else {
            return;
        };
        if entry.progress_dismissed() {
            return;
        }
        let Some(announced) = entry.announced() else {
            return;
        };
        let done = human_size(entry.bytes_received());
        let size = human_size(announced.bytes);
        let finishing = entry.is_finishing();
        let detail = if finishing {
            format!("Finishing: {done} of {size}")
        } else {
            format!("{done} of {size}")
        };
        if !entry.note_progress(&detail) && on_change {
            return;
        }
        let spec = ToastSpec {
            kind: ToastKind::Info,
            title: format!(
                "Fetching folder {} from {} ({} files, {size})",
                entry.file_name(),
                entry.host_label(),
                announced.files
            ),
            detail: Some(detail),
            key: Some(id.to_owned()),
            sticky: true,
            // A finishing fetch is not cancelled, so its toast offers no Cancel.
            actions: if finishing {
                Vec::new()
            } else {
                vec![FETCH_CANCEL]
            },
        };
        self.push_toast_spec(spec, cx);
    }

    /// Shows the file fetch `id`'s received bytes on its toast, once the
    /// host said the file's size; with `on_change`, only when the shown
    /// text changed. Once the user closed that toast it stays closed, and a
    /// fetch that reports in the Fetch file… dialog, a page's media and a
    /// folder's fetch show no such toast.
    fn show_file_progress(&mut self, id: &str, on_change: bool, cx: &mut Context<Self>) {
        let Some(entry) = self.fetches.get_mut(id) else {
            return;
        };
        if entry.is_folder()
            || entry.reports_to_popup()
            || entry.parent().is_some()
            || entry.batch().is_some()
            || entry.progress_dismissed()
        {
            return;
        }
        let Some(size) = entry.size() else {
            return;
        };
        let detail = crate::fetch_dialog::progress_text(
            entry.bytes_received(),
            Some(size),
            entry.host_label(),
        );
        if !entry.note_progress(&detail) && on_change {
            return;
        }
        let finishing = entry.is_finishing();
        let spec = ToastSpec {
            kind: ToastKind::Info,
            title: format!("Fetching {} from {}", entry.file_name(), entry.host_label()),
            detail: Some(detail),
            key: Some(fetching_toast_key(id, entry.group())),
            // Only a fetch that can still be cancelled is held open; a
            // finishing one is timed and expires.
            sticky: !finishing,
            actions: if finishing {
                Vec::new()
            } else {
                vec![FETCH_CANCEL]
            },
        };
        self.push_toast_spec(spec, cx);
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
        let (host_key, host, target) = self.fetch_target(&request)?;
        let repo_id = request.repo_id.clone();
        let worktree_path = request.worktree_path.clone();
        let rel_path = request.rel_path.clone();
        let mut entry = FetchEntry::new(host_key.clone(), host.clone(), request, target);
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
        self.send_to(
            &host_key,
            ClientMessage::FetchFile {
                id: id.clone(),
                repo_id,
                worktree_path,
                path: rel_path,
            },
        );
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
                    sticky: true,
                    actions: vec![FETCH_CANCEL],
                },
                cx,
            );
        }
        Ok(id)
    }

    /// The host `request` names, its label for toasts ([`file_fetch::host_label`])
    /// and where `request`'s file lands under its download folder.
    ///
    /// # Errors
    /// The [`Refusal`] for the local daemon, a refused path or a missing
    /// download folder.
    fn fetch_target(&self, request: &FetchRequest) -> Result<(HostKey, String, PathBuf), Refusal> {
        self.fetch_target_with(request, file_fetch::download_path)
    }

    /// [`Self::fetch_target`], with `place` saying where the fetch lands.
    ///
    /// # Errors
    /// As [`Self::fetch_target`].
    fn fetch_target_with(
        &self,
        request: &FetchRequest,
        place: fn(&Path, &str, &str, &str) -> Result<PathBuf, String>,
    ) -> Result<(HostKey, String, PathBuf), Refusal> {
        let name = file_fetch::fetch_name(request);
        let host_key = request.host.clone();
        let conn = self.conn_of(&host_key).unwrap_or_else(|| self.conn());
        let Some(folder) = file_fetch::host_folder(conn.target()) else {
            tracing::warn!(rel = %request.rel_path, "refusing a file fetch from the local daemon");
            return Err(Refusal {
                kind: ToastKind::Warning,
                title: failed_title(name),
                detail: Some("Files are fetched only from a remote host.".to_owned()),
            });
        };
        let target = match &self.remote_files_root {
            Ok(root) => place(root, &folder, &request.repo_name, &request.rel_path),
            Err(err) => Err(format!("No folder for fetched files: {err}")),
        };
        match target {
            Ok(target) => Ok((host_key, file_fetch::host_label(conn.target()), target)),
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
        let (host_key, host, target) = match self.fetch_target(&request) {
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
        let Some(id) = self
            .fetches
            .insert_child(page_id, host_key.clone(), host, request, target)
        else {
            tracing::info!(page = page_id, rel = %rel_path, "a page's media file is already fetching");
            return;
        };
        tracing::info!(%id, page = page_id, rel = %rel_path, "fetching a page's media file");
        self.send_to(
            &host_key,
            ClientMessage::FetchFile {
                id,
                repo_id,
                worktree_path,
                path: rel_path,
            },
        );
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
            self.dismiss_toast_key(&fetching_toast_key(page_id, page.group()), cx);
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
        self.send_to(
            entry.host_key(),
            ClientMessage::CancelFetch { id: id.to_owned() },
        );
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

    /// The daemon could not fetch `id`: a link's reading that is a folder
    /// on a host too old to send one says so and ends the link; a reading
    /// that never started moves on to the link's next reading; anything
    /// else fails.
    fn on_fetch_error(
        &mut self,
        id: &str,
        error: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_old_hosts_folder(id, error) {
            let Some(entry) = self.fetches.remove(id) else {
                return;
            };
            tracing::warn!(id, rel = %entry.request().rel_path, "the host cannot send a folder a link names: {error}");
            abort_writer(id, &entry);
            let host = entry.host_key().clone();
            self.dismiss_toast_key(&fetching_toast_key(id, entry.group()), cx);
            self.push_too_old(&host, entry.file_name(), cx);
            return;
        }
        match self.fetches.next_reading(id) {
            Some((next, group)) => {
                tracing::info!(id, next = %next.rel_path, "a link's reading is not on the host: {error}");
                let key = group.key().to_owned();
                // A refused retry shows its own toast, and the link's
                // "Fetching" toast, keyed by the group, goes with it.
                if self.start_fetch(next, Some(group), cx).is_none() {
                    self.dismiss_toast_key(&key, cx);
                }
            }
            None => self.end_fetch(id, error, false, window, cx),
        }
    }

    /// Whether `error` ends the link reading `id` on a host too old to send
    /// folders, whose daemon refuses a folder as a file that is a
    /// directory.
    fn is_old_hosts_folder(&self, id: &str, error: &str) -> bool {
        error.ends_with(" is a directory")
            && self.fetches.get(id).is_some_and(|entry| {
                entry.group().is_some()
                    && !entry.is_folder()
                    && !self.host_sends_folders(entry.host_key())
            })
    }

    /// Folds in a fetch's reply; returns whether it was one.
    pub(crate) fn on_fetch_message(
        &mut self,
        _host: &HostKey,
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
            DaemonMessage::FolderFetchStarted {
                id,
                resolved_path,
                files,
                bytes,
            } => {
                let announced = Announced {
                    files: *files,
                    bytes: *bytes,
                };
                self.on_folder_fetch_started(id, resolved_path, announced, window, cx);
            }
            DaemonMessage::PathStat { id, kind, .. } => return self.on_path_stat(id, *kind, cx),
            DaemonMessage::SessionFolders { id, folders, error } => {
                return self.on_session_folders(id, folders, error.as_deref(), cx);
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
        if entry.is_folder() {
            self.end_fetch(id, "the host sent a file for a folder", true, window, cx);
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
        entry.set_writer(Writer::File(writer));
        let target = entry.target().to_path_buf();
        self.show_file_progress(id, false, cx);
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

    /// Starts the folder fetch's unpacker on a thread of its own, since it
    /// blocks on the disk and on its steps.
    fn on_folder_fetch_started(
        &mut self,
        id: &str,
        resolved_path: &str,
        announced: Announced,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let hold = self.fetches.unpack_hold();
        let Some(entry) = self.fetches.get_mut(id) else {
            tracing::debug!(id, "a folder fetch the client no longer follows started");
            return;
        };
        if entry.writer().is_some() {
            self.end_fetch(id, "the transfer started twice", true, window, cx);
            return;
        }
        if !entry.is_folder() {
            self.end_fetch(id, "the host sent a folder for a file", true, window, cx);
            return;
        }
        let Some(unpacked) = entry.unpacked_counter() else {
            return;
        };
        entry.on_folder_started(announced, resolved_path.to_owned());
        let (writer, steps) = std::sync::mpsc::channel();
        entry.set_writer(Writer::Folder(writer));
        let target = entry.target().to_path_buf();
        let (report, outcome) = futures::channel::oneshot::channel();
        let thread_id = id.to_owned();
        let spawned = std::thread::Builder::new()
            .name(format!("unpack {id}"))
            .spawn(move || {
                if let Some(gate) = &hold {
                    gate.wait(|released| released.start);
                }
                let ended =
                    folder_unpack::run_unpacker(&target, &thread_id, announced, steps, &mut |n| {
                        unpacked.store(n, Ordering::Relaxed);
                    });
                if let Some(gate) = &hold {
                    gate.wait(|released| released.outcome);
                }
                if report.send(ended).is_err() {
                    tracing::debug!(id = %thread_id, "nobody waits on the folder's unpacker");
                }
            });
        if let Err(err) = spawned {
            let reason = format!("starting the folder unpacker: {err}");
            self.end_fetch(id, &reason, true, window, cx);
            return;
        }
        tracing::info!(
            id,
            files = announced.files,
            bytes = announced.bytes,
            "a folder fetch started"
        );
        self.show_folder_progress(id, false, cx);
        let id = id.to_owned();
        cx.spawn_in(window, async move |this, cx| {
            let ended = outcome
                .await
                .unwrap_or_else(|_| WriteOutcome::Failed("the folder unpacker stopped".to_owned()));
            // Fails only when the view is gone, leaving nowhere to report.
            this.update_in(cx, |root, window, cx| {
                root.after_write(&id, ended, window, cx);
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
        if !writer.send(WriteStep::Chunk { seq, data }) {
            tracing::debug!(id, seq, "the fetch's writer already ended");
        }
        if entry.is_folder() {
            self.show_folder_progress(id, true, cx);
        } else {
            self.show_file_progress(id, true, cx);
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
        if !writer.send(WriteStep::Done) {
            tracing::debug!(id, "the fetch's writer already ended");
            return;
        }
        let was_finishing = entry.is_finishing();
        entry.mark_finishing();
        if entry.is_folder() {
            self.show_folder_progress(id, false, cx);
            if !was_finishing {
                Self::tick_finishing_folder(id.to_owned(), cx);
            }
        } else {
            self.show_file_progress(id, false, cx);
        }
    }

    /// Nothing else redraws the finishing folder fetch `id`'s toast after
    /// the host's last chunk, so a timer started at `Done` shows what the
    /// unpacker did since, until the fetch ends; before `Done` no timer
    /// runs.
    fn tick_finishing_folder(id: String, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(FINISHING_TICK).await;
                let ticking = this.update(cx, |root, cx| {
                    if !root.fetches.get(&id).is_some_and(FetchEntry::is_finishing) {
                        return false;
                    }
                    root.show_folder_progress(&id, true, cx);
                    true
                });
                // The fetch ended, or the view is gone with it.
                if !ticking.unwrap_or(false) {
                    return;
                }
            }
        })
        .detach();
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
            WriteOutcome::Saved(path)
                if self.fetches.get(id).is_some_and(FetchEntry::is_folder) =>
            {
                tracing::info!(id, path = %path.display(), "fetched a folder");
                if let Some(entry) = self.fetches.remove(id) {
                    self.push_folder_fetched(id, &entry, path, cx);
                }
            }
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
                    return;
                }
                if !entry.reports_to_popup() && self.host_sends_folders(entry.host_key()) {
                    self.push_file_fetched(id, &entry, cx);
                }
                if entry.request().open_when_done {
                    self.open_fetched(path, entry.request(), window, cx);
                }
            }
            WriteOutcome::Failed(reason) => self.end_fetch(id, &reason, true, window, cx),
            WriteOutcome::Aborted => {}
        }
    }

    /// The plain file fetch `id` is saved: its done toast, in place of its
    /// Fetching toast, offers to fetch the file's whole folder.
    fn push_file_fetched(&mut self, id: &str, entry: &FetchEntry, cx: &mut Context<Self>) {
        let key = fetching_toast_key(id, entry.group());
        let spec = ToastSpec {
            kind: ToastKind::Info,
            title: format!("Fetched {}", file_fetch::fetch_name(entry.request())),
            detail: Some(format!("from {}", entry.host_label())),
            key: Some(key.clone()),
            sticky: false,
            actions: vec![FETCH_WHOLE_FOLDER],
        };
        self.push_toast_spec(spec, cx);
        self.fetches.note_done(
            &key,
            DoneAction::WholeFolder(file_fetch::parent_folder_request(entry.request())),
        );
    }

    /// The folder fetch `id` is saved at `path`: its toast says so, with how
    /// many files came, and offers to reveal it.
    fn push_folder_fetched(
        &mut self,
        id: &str,
        entry: &FetchEntry,
        path: PathBuf,
        cx: &mut Context<Self>,
    ) {
        let files = entry.announced().map_or(0, |announced| announced.files);
        let spec = ToastSpec {
            kind: ToastKind::Info,
            title: format!("Fetched folder {}", entry.file_name()),
            detail: Some(format!("{files} files from {}", entry.host_label())),
            key: Some(id.to_owned()),
            sticky: false,
            actions: vec![FETCH_REVEAL],
        };
        self.push_toast_spec(spec, cx);
        self.fetches.note_done(id, DoneAction::Reveal(path));
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
            self.send_to(
                entry.host_key(),
                ClientMessage::CancelFetch { id: id.to_owned() },
            );
        }
        if let Some(page_id) = entry.parent() {
            self.fetches.child_ended(page_id, MediaEnd::Failed);
            self.pump_batch(page_id, window, cx);
            return;
        }
        self.dismiss_toast_key(&fetching_toast_key(id, entry.group()), cx);
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

    /// `host`'s connection closed or moved to another daemon: every fetch
    /// from it still receiving ends, its part file deleted; one whose `Done`
    /// reached its writer ends by the writer's result. The Fetch file…
    /// dialog's own fetch says so in the dialog, which the connection
    /// overlay closes. A saved page waiting on its media does not open, and
    /// one toast says so for the page and its media fetches. Other hosts'
    /// fetches go on.
    pub(crate) fn end_fetches_on_disconnect(
        &mut self,
        host: &HostKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for id in self.fetches.drop_stats_of(host) {
            self.dismiss_toast_key(&id, cx);
        }
        for (id, entry) in self.fetches.take_unfinished(host) {
            let reason = format!("The connection to {} closed.", entry.host_label());
            tracing::warn!(%id, rel = %entry.request().rel_path, "fetching a file failed: {reason}");
            abort_writer(&id, &entry);
            if entry.parent().is_some() {
                continue;
            }
            self.dismiss_toast_key(&fetching_toast_key(&id, entry.group()), cx);
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

    /// Opens the fetched copy at `path` the way `request` asked: for the
    /// viewer, a text file in the viewer at its line; for its folder,
    /// revealed there; anything else by the `:line` rule.
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
        let opening = Opening {
            origin: Origin::Fetched,
            action: request.action,
        };
        self.open_text_or_external(action, opening, window, cx);
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
            | OpenJob::Page { path, .. }
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
            .is_ok_and(|root| is_under_resolved(root, path))
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
        && !writer.send(WriteStep::Abort)
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
