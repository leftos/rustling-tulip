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
use crate::open_view::{COULD_NOT_OPEN, LinkFolders, LinkSource, Opening, Origin, Route, Then};
use crate::phone::browser::{Answer, FolderEnd, HostFacts};
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

/// Why the phone says a saved fetch failed when it landed outside the
/// copies folder, so the phone cannot keep it.
const PHONE_FOLDER_NOT_KEPT: &str = "it was saved outside the copies folder";

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

/// A count of files, in the singular for one.
fn files_count(files: u64) -> String {
    if files == 1 {
        "1 file".to_owned()
    } else {
        format!("{files} files")
    }
}

/// The detail of a fetched folder's toast: how many files `host` sent.
fn folder_files_detail(files: u64, host: &str) -> String {
    format!("{} from {host}", files_count(files))
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
        self.start_fetch(request, None, None, cx)
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
            self.push_too_old(&request.host, &name, None, cx);
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

    /// A Ctrl+click on a path link in a pane of the remote host `source`
    /// names: fetched from the source's folders, its session's
    /// ([`Self::fetch_session_link`]) or a dispatch run's own
    /// ([`Self::fetch_run_link`]); the copy opens as the source's action
    /// asks.
    pub(crate) fn fetch_path_link(
        &mut self,
        link: &TerminalLink,
        source: &LinkSource<'_>,
        cx: &mut Context<Self>,
    ) {
        // On the phone the tap travels with its own fetch, so its outcome
        // names its own path and session page.
        let tap = (self.shape == crate::RootShape::Phone).then(|| file_fetch::PhoneTap {
            session: source.session.map(str::to_owned),
            path: link.target.clone(),
        });
        match source.folders {
            LinkFolders::Session => self.fetch_session_link(link, source, tap, cx),
            LinkFolders::Run(dirs) => self.fetch_run_link(link, source, dirs, tap, cx),
        }
    }

    /// A path link of a dispatch run on the remote host `source` names:
    /// fetches the first of its readings under the run's folders `dirs`
    /// ([`Self::run_link_readings`]), trying them one at a time, and never
    /// asks the host for a session's folders.
    fn fetch_run_link(
        &mut self,
        link: &TerminalLink,
        source: &LinkSource<'_>,
        dirs: &[String],
        tap: Option<file_fetch::PhoneTap>,
        cx: &mut Context<Self>,
    ) {
        let mut readings = self.run_link_readings(source.host, &link.candidates, dirs);
        if readings.is_empty() {
            tracing::warn!(
                target = %link.target,
                folders = ?dirs,
                "no folder of the run holds the transcript path link"
            );
            self.warn_tap(
                source.host,
                tap.as_ref(),
                &failed_title(file_fetch::file_name(&link.target)),
                Some(NOT_INSIDE.to_owned()),
                cx,
            );
            return;
        }
        for reading in &mut readings {
            reading.action = source.action;
        }
        self.start_link_readings(readings, source.host, tap, cx);
    }

    /// The readings of a link's `candidates` on `host` under a dispatch
    /// run's folders `dirs`, its worktree first. On a host that serves any
    /// absolute path, each candidate as a host path joined to each folder
    /// in turn, each path once; on an older host, repo-relative
    /// ([`Self::older_run_readings`]).
    pub(crate) fn run_link_readings(
        &self,
        host: &HostKey,
        candidates: &[crate::links::TerminalLinkCandidate],
        dirs: &[String],
    ) -> Vec<FetchRequest> {
        if !self.host_serves_paths(host) {
            return self.older_run_readings(host, candidates, dirs);
        }
        let mut found: Vec<FetchRequest> = Vec::new();
        for dir in dirs {
            for reading in fetch_link::host_readings(host, candidates, Some(dir)) {
                if !found.contains(&reading) {
                    found.push(reading);
                }
            }
        }
        found
    }

    /// A run's link readings on `host`, which serves only repo-relative
    /// paths: `dirs` is the run's worktree then its main checkout, or the
    /// one of them it has. The last is read as the main checkout, whose
    /// registered repo (the deepest holding it) gives the roots: the
    /// worktree as a worktree of that repo, as a session member on it
    /// would be, then the repo itself; relative paths join the first.
    fn older_run_readings(
        &self,
        host: &HostKey,
        candidates: &[crate::links::TerminalLinkCandidate],
        dirs: &[String],
    ) -> Vec<FetchRequest> {
        let (Some(first), Some(main)) = (dirs.first(), dirs.last()) else {
            return Vec::new();
        };
        let repos = self.sidebar.repos_of(host);
        let main_roots = fetch_link::roots(&[], Some(main), repos);
        let mut roots = Vec::new();
        if let (Some(repo), [worktree, _]) = (main_roots.first(), dirs) {
            let member = protocol::SessionMember {
                repo_id: repo.repo_id.clone(),
                repo_name: repo.repo_name.clone(),
                branch: String::new(),
                worktree_path: worktree.clone(),
            };
            roots.extend(fetch_link::roots(&[member], None, repos));
        }
        roots.extend(main_roots);
        fetch_link::readings(host, candidates, Some(first), &roots, &roots)
    }

    /// A path link of a pane of the remote host `source` names: fetches the
    /// first of the link's readings under the repos and worktrees of the
    /// source's session as that host lists it, trying them one at a time.
    /// A link that may lie outside those folders is first asked of the
    /// host, which lists every folder `git worktree list` names for them.
    fn fetch_session_link(
        &mut self,
        link: &TerminalLink,
        source: &LinkSource<'_>,
        tap: Option<file_fetch::PhoneTap>,
        cx: &mut Context<Self>,
    ) {
        let (host, session, action) = (source.host, source.session, source.action);
        let found =
            session.and_then(|id| self.sidebar.sessions_of(host).iter().find(|s| s.id == id));
        let cwd = found.and_then(|session| session.current_cwd.clone());
        let mut readings = found
            .map(|session| self.link_readings(host, &link.candidates, session))
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
                    tap,
                },
                session,
                cx,
            );
            return;
        }
        if readings.is_empty() {
            let outside = self.host_link_readings(host, &link.candidates, cwd.as_deref(), action);
            if !outside.is_empty() {
                self.start_link_readings(outside, host, tap, cx);
                return;
            }
            tracing::warn!(
                target = %link.target,
                session = session.unwrap_or("none"),
                "no repo or worktree of the session holds the terminal path link"
            );
            self.warn_tap(
                host,
                tap.as_ref(),
                &failed_title(file_fetch::file_name(&link.target)),
                Some(NOT_INSIDE.to_owned()),
                cx,
            );
            return;
        }
        for reading in &mut readings {
            reading.action = action;
        }
        self.start_link_readings(readings, host, tap, cx);
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
        tap: Option<file_fetch::PhoneTap>,
        cx: &mut Context<Self>,
    ) {
        if self.host_sends_folders(host) {
            let Some((id, first)) = self.fetches.begin_stat(host.clone(), readings) else {
                return;
            };
            self.fetches.tap_stat(&id, tap);
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
        if !self.start_file_flow(readings, None, tap.as_ref(), cx) {
            tracing::info!(host = ?host, "the host cannot send the folder a link names");
            self.push_too_old(host, &root, tap.as_ref(), cx);
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
        let read = if self.host_serves_paths(&ask.host) {
            fetch_link::host_path_readings
        } else {
            fetch_link::readings
        };
        let mut readings = read(
            &ask.host,
            &ask.candidates,
            ask.cwd.as_deref(),
            &roots,
            &ask.own_roots,
        );
        if readings.is_empty() {
            let (host, cwd) = (ask.host.clone(), ask.cwd.as_deref());
            let outside = self.host_link_readings(&host, &ask.candidates, cwd, ask.action);
            if !outside.is_empty() {
                self.start_link_readings(outside, &host, ask.tap, cx);
                return true;
            }
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
            self.warn_tap(
                &ask.host,
                ask.tap.as_ref(),
                &failed_title(file_fetch::file_name(&ask.target)),
                Some(detail),
                cx,
            );
            return true;
        }
        for reading in &mut readings {
            reading.action = ask.action;
        }
        self.start_link_readings(readings, &ask.host, ask.tap, cx);
        true
    }

    /// A link's readings on `host` under `session`'s own folders: on a host
    /// that serves any path, a place outside them reads as its host path in
    /// its turn ([`fetch_link::host_path_readings`]).
    fn link_readings(
        &self,
        host: &HostKey,
        candidates: &[crate::links::TerminalLinkCandidate],
        session: &protocol::SessionSnapshot,
    ) -> Vec<FetchRequest> {
        let repos = self.sidebar.repos_of(host);
        if !self.host_serves_paths(host) {
            return fetch_link::session_readings(host, candidates, session, repos);
        }
        let cwd = session.current_cwd.as_deref();
        let own = fetch_link::roots(&session.members, cwd, repos);
        fetch_link::host_path_readings(host, candidates, cwd, &own, &own)
    }

    /// A link outside every session folder on `host`: when the host serves
    /// absolute paths, the link's host-path readings, opening as `action`
    /// asks. Empty when the host does not serve them or the link has no
    /// such reading, which leaves the refusal to the caller.
    fn host_link_readings(
        &self,
        host: &HostKey,
        candidates: &[crate::links::TerminalLinkCandidate],
        cwd: Option<&str>,
        action: LinkAction,
    ) -> Vec<FetchRequest> {
        if !self.host_serves_paths(host) {
            return Vec::new();
        }
        let mut readings = fetch_link::host_readings(host, candidates, cwd);
        for reading in &mut readings {
            reading.action = action;
        }
        if !readings.is_empty() {
            tracing::info!(host = ?host, "fetching a link outside every session folder by its host path");
        }
        readings
    }

    /// Whether `host`'s daemon fetches any absolute path.
    pub(crate) fn host_serves_paths(&self, host: &HostKey) -> bool {
        self.conn_of(host)
            .is_some_and(|conn| conn.supports(protocol::FEATURE_HOST_PATHS))
    }

    /// Whether `host`'s daemon lists its folders and says what it runs.
    pub(crate) fn host_serves_browse(&self, host: &HostKey) -> bool {
        self.conn_of(host)
            .is_some_and(|conn| conn.supports(protocol::FEATURE_HOST_BROWSE))
    }

    /// Hands `host`'s answer to the phone browser's request `id` to the
    /// phone, from a host that offers browsing; returns whether the phone
    /// took it.
    fn on_browse_answer(
        &mut self,
        host: &HostKey,
        id: &str,
        answer: Answer,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.host_serves_browse(host) || !self.phone_browse.answer(host, id, answer) {
            return false;
        }
        cx.notify();
        true
    }

    /// `host` as fetch toasts name it ([`file_fetch::host_label`]); "The
    /// host" when it has no connection.
    pub(crate) fn host_label_of(&self, host: &HostKey) -> String {
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

    /// Says that `host` is too old to send the folder `name`: on the phone,
    /// on the page of the session `tap` was tapped in.
    fn push_too_old(
        &mut self,
        host: &HostKey,
        name: &str,
        tap: Option<&file_fetch::PhoneTap>,
        cx: &mut Context<Self>,
    ) {
        let label = self.host_label_of(host);
        self.warn_tap(
            host,
            tap,
            &failed_title(name),
            Some(too_old_detail(&label)),
            cx,
        );
    }

    /// A warning titled `title` about a link on `host`: on the phone, for a
    /// path tapped there (`tap`), the page of the session it was tapped in
    /// says why (`detail`, else `title`), since the phone draws no toasts;
    /// otherwise a warning toast.
    fn warn_tap(
        &mut self,
        host: &HostKey,
        tap: Option<&file_fetch::PhoneTap>,
        title: &str,
        detail: Option<String>,
        cx: &mut Context<Self>,
    ) {
        match tap.filter(|_| self.shape == crate::RootShape::Phone) {
            Some(tap) => {
                let note = detail.unwrap_or_else(|| title.to_owned());
                tracing::info!(path = %tap.path, %note, "phone: a tapped path could not open");
                let open = crate::PhoneOpen::Missing {
                    host: host.clone(),
                    session: tap.session.clone(),
                    note,
                };
                self.phone_open(open, cx);
            }
            None => self.push_toast(ToastKind::Warning, title, detail, cx),
        }
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
        tap: Option<&file_fetch::PhoneTap>,
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
        if self.start_fetch(first, Some(group), tap, cx).is_none()
            && let Some(key) = key
        {
            self.dismiss_toast_key(key, cx);
        }
        true
    }

    /// Asks `host` what `reading` names, for the stat `id`.
    fn send_stat(&self, host: &HostKey, id: String, reading: &FetchRequest) {
        tracing::info!(%id, rel = %reading.rel_path, host_path = reading.host_path, "asking the host what a link names");
        self.send_to(host, file_fetch::stat_message(id, reading));
    }

    /// The host said what the stat `id`'s reading names: a folder is
    /// fetched as one (on the phone, a host path opens the browser at it), a
    /// missing path moves on to the next reading (on the phone, the last one
    /// says the path is not on the host), and anything else is fetched as a
    /// file. Returns whether `id` was a stat.
    fn on_path_stat(&mut self, id: &str, kind: PathKind, cx: &mut Context<Self>) -> bool {
        let Some(stat) = self.fetches.get_stat(id) else {
            return false;
        };
        let host = stat.host().clone();
        match kind {
            PathKind::Dir => self.on_stat_dir(id, &host, cx),
            PathKind::Missing => self.on_stat_missing(id, &host, cx),
            PathKind::File | PathKind::Other | PathKind::Unknown => {
                let Some(stat) = self.fetches.take_stat(id) else {
                    return true;
                };
                let mut readings = vec![stat.reading().clone()];
                readings.extend(stat.rest().iter().cloned());
                self.start_files_or_warn(readings, id, stat.tap(), cx);
            }
        }
        true
    }

    /// The host said the stat `id`'s reading on `host` is a folder: the
    /// phone opens its browser there ([`Self::phone_folder_stat`]); the
    /// desktop fetches it as one.
    fn on_stat_dir(&mut self, id: &str, host: &HostKey, cx: &mut Context<Self>) {
        let Some(stat) = self.fetches.take_stat(id) else {
            return;
        };
        // The folder's own toast, or its refusal, takes over.
        self.dismiss_toast_key(id, cx);
        if self.phone_folder_stat(host, &stat, cx) {
            return;
        }
        if let Err(refusal) = self.try_start_folder_fetch(stat.reading().clone(), cx) {
            self.push_toast(refusal.kind, &refusal.title, refusal.detail, cx);
        }
    }

    /// The host said the stat `id`'s reading on `host` is missing: the next
    /// reading is asked about; with none left, the phone says the path is
    /// not on the host, and the desktop fetches the readings as files.
    fn on_stat_missing(&mut self, id: &str, host: &HostKey, cx: &mut Context<Self>) {
        if let Some(next) = self.fetches.next_stat(id) {
            self.send_stat(host, id.to_owned(), &next);
            return;
        }
        let Some(stat) = self.fetches.take_stat(id) else {
            return;
        };
        if self.shape == crate::RootShape::Phone {
            self.dismiss_toast_key(id, cx);
            self.phone_not_on_host(host, &stat, cx);
            return;
        }
        tracing::info!(
            id,
            "no reading of the link is on the host; fetching as files"
        );
        self.start_files_or_warn(stat.readings().to_vec(), id, stat.tap(), cx);
    }

    /// On the phone, the host says the reading `stat` asked about is a
    /// folder: the browser opens at its absolute path on the host (a
    /// session folder's reading joined to its root), and when none can be
    /// built the session page says the path is not on the host. A tap never
    /// fetches a folder whole. Returns whether the phone took the answer;
    /// `false` on the desktop.
    fn phone_folder_stat(
        &mut self,
        host: &HostKey,
        stat: &file_fetch::PendingStat,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.shape != crate::RootShape::Phone {
            return false;
        }
        let Some(path) = self.reading_host_path(host, stat.reading()) else {
            self.phone_not_on_host(host, stat, cx);
            return true;
        };
        tracing::debug!(%path, "phone: a tapped path is a folder; browsing it");
        let open = crate::PhoneOpen::Folder {
            host: host.clone(),
            session: stat.tap().and_then(|tap| tap.session.clone()),
            path,
        };
        self.phone_open(open, cx);
        true
    }

    /// The absolute path on `host` that `reading` names: a host path as it
    /// is, else its rel path joined to its worktree, or to its registered
    /// repo's root. `None` when its repo is not one `host` lists.
    fn reading_host_path(&self, host: &HostKey, reading: &FetchRequest) -> Option<String> {
        if reading.host_path {
            return Some(reading.rel_path.clone());
        }
        let root = match reading
            .worktree_path
            .as_ref()
            .filter(|path| !path.is_empty())
        {
            Some(worktree) => worktree.clone(),
            None => self
                .sidebar
                .repos_of(host)
                .iter()
                .find(|repo| repo.id == reading.repo_id)?
                .path
                .clone(),
        };
        let path = reading
            .rel_path
            .split(['/', '\\'])
            .filter(|segment| !segment.is_empty())
            .fold(root, |path, segment| {
                crate::phone::browser::child_of(None, &path, segment)
            });
        Some(path)
    }

    /// On the phone, no reading of the tapped path `stat` asked about is on
    /// `host`: nothing is fetched, and the page of the session it was tapped
    /// in says so, naming the path as tapped (else the first reading).
    fn phone_not_on_host(
        &mut self,
        host: &HostKey,
        stat: &file_fetch::PendingStat,
        cx: &mut Context<Self>,
    ) {
        let tap = stat.tap();
        let path = tap
            .map(|tap| tap.path.clone())
            .or_else(|| {
                stat.readings()
                    .first()
                    .map(|reading| reading.rel_path.clone())
            })
            .unwrap_or_default();
        tracing::info!(%path, "phone: no reading of a tapped path is on the host");
        let open = crate::PhoneOpen::Missing {
            host: host.clone(),
            session: tap.and_then(|tap| tap.session.clone()),
            note: format!("Not on {}: {path}", self.host_label_of(host)),
        };
        self.phone_open(open, cx);
    }

    /// [`Self::start_file_flow`] over `readings`; when none names a file, a
    /// warning titled by the first reading says why: a link that reads only
    /// as repo or worktree roots names a folder missing on the host, and
    /// any other link is outside the session's repos and worktrees. The
    /// toast of the link's stat `key` becomes the fetch's, or gives way to
    /// the warning, which on the phone is the note of the session `tap` was
    /// tapped in.
    fn start_files_or_warn(
        &mut self,
        readings: Vec<FetchRequest>,
        key: &str,
        tap: Option<&file_fetch::PhoneTap>,
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
        let host = readings.first().map(|reading| reading.host.clone());
        if !self.start_file_flow(readings, Some(key), tap, cx) {
            self.dismiss_toast_key(key, cx);
            let detail = match root_host {
                Some(host) => format!("{name} is not on {}.", self.host_label_of(&host)),
                None => NOT_INSIDE.to_owned(),
            };
            match host {
                Some(host) => self.warn_tap(&host, tap, &failed_title(&name), Some(detail), cx),
                None => self.push_toast(ToastKind::Warning, &failed_title(&name), Some(detail), cx),
            }
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
        let rel_path = request.rel_path.clone();
        let sent = request.clone();
        let entry = FetchEntry::new(host_key.clone(), host.clone(), request, target).for_folder();
        let Ok(id) = self.fetches.insert(entry) else {
            tracing::info!(rel = %rel_path, "the folder is already being fetched");
            return Err(Refusal {
                kind: ToastKind::Info,
                title: format!("Already fetching {name}"),
                detail: None,
            });
        };
        tracing::info!(%id, %host, rel = %rel_path, host_path = sent.host_path, "fetching a folder");
        self.send_to(&host_key, file_fetch::folder_message(id.clone(), &sent));
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
                "Fetching folder {} from {} ({}, {size})",
                entry.file_name(),
                entry.host_label(),
                files_count(announced.files)
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
    /// given: its "Fetching" toast is the group's, updated in place. On the
    /// phone, `tap` is the tap this fetch came from, which its later
    /// failures name on its session's page; a refusal is a toast, or that
    /// page's note.
    fn start_fetch(
        &mut self,
        request: FetchRequest,
        group: Option<ProbeGroup>,
        tap: Option<&file_fetch::PhoneTap>,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let host = request.host.clone();
        match self.try_start_fetch(request, group, false, cx) {
            Ok(id) => {
                self.fetches.set_tap(&id, tap.cloned());
                Some(id)
            }
            Err(refusal) => {
                match tap.filter(|_| self.shape == crate::RootShape::Phone) {
                    Some(tap) => {
                        self.warn_tap(&host, Some(tap), &refusal.title, refusal.detail, cx);
                    }
                    None => self.push_toast(refusal.kind, &refusal.title, refusal.detail, cx),
                }
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
        let rel_path = request.rel_path.clone();
        let sent = request.clone();
        let mut entry = FetchEntry::new(host_key.clone(), host.clone(), request, target);
        if let Some(group) = group {
            entry = entry.in_group(group);
        }
        if popup {
            entry = entry.for_popup();
        }
        let folder = self
            .fetches
            .folder_overlapping(&entry)
            .map(|folder| folder.file_name().to_owned());
        let Ok(id) = self.fetches.insert(entry) else {
            tracing::info!(rel = %rel_path, ?folder, "the file is already being fetched");
            let title = folder.map_or_else(
                || format!("Already fetching {name}"),
                |folder| format!("Already fetching folder {folder}"),
            );
            return Err(Refusal {
                kind: ToastKind::Info,
                title,
                detail: None,
            });
        };
        tracing::info!(%id, %host, rel = %rel_path, popup, host_path = sent.host_path, "fetching a file");
        self.send_to(&host_key, file_fetch::file_message(id.clone(), &sent));
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
            Ok(root) if request.host_path => {
                file_fetch::host_download_path(root, &folder, &request.rel_path)
            }
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
        let rel_path = request.rel_path.clone();
        let sent = request.clone();
        let Some(id) = self
            .fetches
            .insert_child(page_id, host_key.clone(), host, request, target)
        else {
            tracing::info!(page = page_id, rel = %rel_path, "a page's media file is already fetching");
            return;
        };
        tracing::info!(%id, page = page_id, rel = %rel_path, "fetching a page's media file");
        self.send_to(&host_key, file_fetch::file_message(id, &sent));
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
            // Fails only when the view is gone or this task's window has closed,
            // leaving nowhere to report.
            RootView::update_in_task_window(&this, cx, |root, window, cx| {
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
            // Fails only when the view is gone or this task's window has closed,
            // leaving nowhere to report.
            RootView::update_in_task_window(&this, cx, |root, window, cx| {
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
            self.push_too_old(&host, entry.file_name(), None, cx);
            return;
        }
        // The retried reading is the same tap's fetch on the phone.
        let tap = self.fetches.get(id).and_then(FetchEntry::tap).cloned();
        match self.fetches.next_reading(id) {
            Some((next, group)) => {
                tracing::info!(id, next = %next.rel_path, "a link's reading is not on the host: {error}");
                let key = group.key().to_owned();
                // A refused retry shows its own toast, and the link's
                // "Fetching" toast, keyed by the group, goes with it.
                if self
                    .start_fetch(next, Some(group), tap.as_ref(), cx)
                    .is_none()
                {
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
        host: &HostKey,
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
            DaemonMessage::PathStat {
                id,
                kind,
                size,
                modified,
            } => {
                let answer = crate::PhoneStatAnswer::Stat {
                    kind: *kind,
                    size: *size,
                    modified: *modified,
                };
                // A phone stat's answer goes to the phone, never to the
                // desktop's link stats.
                if self.phone_stats.answer(host, id, answer) {
                    cx.notify();
                    return true;
                }
                return self.on_path_stat(id, *kind, cx);
            }
            DaemonMessage::SessionFolders { id, folders, error } => {
                return self.on_session_folders(id, folders, error.as_deref(), cx);
            }
            DaemonMessage::HostInfo { id, os, roots, .. } => {
                let facts = HostFacts {
                    os: *os,
                    roots: roots.clone(),
                };
                return self.on_browse_answer(host, id, Answer::Info(facts), cx);
            }
            DaemonMessage::DirListing {
                id,
                entries,
                next_cursor,
                error,
                ..
            } => {
                let answer = Answer::Listing {
                    entries: entries.clone(),
                    next_cursor: next_cursor.clone(),
                    error: error.clone(),
                };
                return self.on_browse_answer(host, id, answer, cx);
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
            // Fails only when the view is gone or this task's window has closed,
            // leaving nowhere to report.
            RootView::update_in_task_window(&this, cx, |root, window, cx| {
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
            // Fails only when the view is gone or this task's window has closed,
            // leaving nowhere to report.
            RootView::update_in_task_window(&this, cx, |root, window, cx| {
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
                self.after_folder_saved(id, path, window, cx);
            }
            WriteOutcome::Saved(path) => self.after_file_written(id, path, window, cx),
            WriteOutcome::Failed(reason) => self.end_fetch(id, &reason, true, window, cx),
            WriteOutcome::Aborted => {}
        }
    }

    /// The file fetch `id` is saved at `path`: its dialog closes, a page
    /// waits for its media, a child reports to its parent, and anything
    /// else opens as its request asks.
    fn after_file_written(
        &mut self,
        id: &str,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        tracing::info!(id, path = %path.display(), "fetched a file");
        // The dialog closes first, so a run confirm the open asks
        // for is not refused as a dialog over a dialog.
        if self.fetch_dialog_follows(id) {
            self.close_fetch_dialog(window, cx);
        }
        if self.waits_on_media(id) {
            self.after_write_page_media(id, path, window, cx);
            return;
        }
        let Some(entry) = self.fetches.remove(id) else {
            return;
        };
        self.after_write_removed(id, &entry, path, window, cx);
    }

    /// The saved page fetch `id` opens without its media when its connection
    /// was lost, and scans for them otherwise.
    fn after_write_page_media(
        &mut self,
        id: &str,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .fetches
            .get(id)
            .is_some_and(FetchEntry::connection_lost)
        {
            self.open_page_without_media(id, path, window, cx);
        } else {
            self.scan_saved_page(id, path, window, cx);
        }
    }

    /// The saved fetch `id`, removed from the list as `entry`, reports to its
    /// parent page when it is a batch child, and opens otherwise.
    fn after_write_removed(
        &mut self,
        id: &str,
        entry: &FetchEntry,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(page_id) = entry.parent() {
            let rel = &entry.request().rel_path;
            self.after_child_saved(page_id.to_owned(), rel, path, window, cx);
            return;
        }
        self.after_file_saved(id, entry, path, window, cx);
    }

    /// Whether the saved file fetch `id` is a page that opens once its
    /// media are fetched. Open containing folder on a host that sends
    /// folders fetches the page's whole folder instead, which brings them.
    /// A page fetched by its host path opens without its media, and so does
    /// a page on the phone, which hands its copy to the share sheet alone.
    fn waits_on_media(&self, id: &str) -> bool {
        if self.shape == crate::RootShape::Phone {
            return false;
        }
        self.fetches.get(id).is_some_and(|entry| {
            let request = entry.request();
            let folder_brings_media =
                request.action == LinkAction::Folder && self.host_sends_folders(entry.host_key());
            request.open_when_done
                && !request.host_path
                && html_media::is_html_page(&request.rel_path)
                && !folder_brings_media
        })
    }

    /// Reveals the saved file a failed folder fetch of `entry` noted, when
    /// Open containing folder started it: the copy outlives the failure.
    fn reveal_noted_copy(
        &mut self,
        entry: &FetchEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(file) = entry.reveal_on_done() {
            let job = OpenJob::Reveal(file.to_path_buf());
            self.open_fetched_job(job, Then::Nothing, window, cx);
        }
    }

    /// The folder fetch `id` is saved at `path`: its done toast shows, and
    /// when Open containing folder started it, the file it noted shows in
    /// Explorer, or the folder when that file did not come.
    fn after_folder_saved(
        &mut self,
        id: &str,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        tracing::info!(id, path = %path.display(), "fetched a folder");
        let Some(entry) = self.fetches.remove(id) else {
            return;
        };
        if self.shape == crate::RootShape::Phone {
            // The phone draws no toast: its browser's line says how it went.
            self.dismiss_toast_key(id, cx);
            self.phone_folder_saved(&entry, &path, cx);
            return;
        }
        let reveal = entry.reveal_on_done().map(Path::to_path_buf);
        self.push_folder_fetched(id, &entry, path.clone(), cx);
        if let Some(file) = reveal {
            let shown = if file.exists() { file } else { path };
            self.open_fetched_job(OpenJob::Reveal(shown), Then::Nothing, window, cx);
        }
    }

    /// The plain file fetch `id` of `entry` is saved at `path`: its done
    /// toast shows when its host sends folders, and it opens as its request
    /// asks. Open containing folder on such a host fetches the file's whole
    /// folder first ([`Self::reveal_after_folder`]).
    fn after_file_saved(
        &mut self,
        id: &str,
        entry: &FetchEntry,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.shape == crate::RootShape::Phone {
            match self.phone_fetched_of(entry, &path) {
                Some(saved) => self.phone_file_saved(saved, cx),
                None => self.warn_tap(
                    entry.host_key(),
                    entry.tap(),
                    &failed_title(entry.file_name()),
                    Some(PHONE_FOLDER_NOT_KEPT.to_owned()),
                    cx,
                ),
            }
            return;
        }
        let request = entry.request();
        let sends_folders = self.host_sends_folders(entry.host_key());
        if request.open_when_done && request.action == LinkAction::Folder && sends_folders {
            self.reveal_after_folder(id, entry, path, window, cx);
            return;
        }
        if !entry.reports_to_popup() && sends_folders {
            self.push_file_fetched(id, entry, cx);
        }
        if request.open_when_done {
            self.open_fetched(path, request, window, cx);
        }
    }

    /// The phone's folder fetch `entry` is saved at `path`: the phone keeps
    /// it as one folder, its size the bytes the host announced, and its
    /// browser says so. A folder not under the copies folder is not kept,
    /// and the browser says it failed.
    fn phone_folder_saved(&mut self, entry: &FetchEntry, path: &Path, cx: &mut Context<Self>) {
        let host = entry.host_key().clone();
        let asked = entry.request().rel_path.clone();
        let end = match self.phone_fetched_of(entry, path) {
            Some(mut saved) => {
                saved.size = entry
                    .announced()
                    .map_or_else(|| entry.bytes_received(), |announced| announced.bytes);
                FolderEnd::Saved { host, asked, saved }
            }
            None => FolderEnd::Failed {
                host,
                asked,
                reason: PHONE_FOLDER_NOT_KEPT.to_owned(),
            },
        };
        self.phone_browse.folder_ended(end);
        cx.notify();
    }

    /// The phone's folder fetch `entry` ended without its folder, for
    /// `reason`: the phone's browser says why. A file fetch, or any fetch
    /// on the desktop, tells nothing.
    fn phone_folder_failed(&mut self, entry: &FetchEntry, reason: &str, cx: &mut Context<Self>) {
        if self.shape != crate::RootShape::Phone || !entry.is_folder() {
            return;
        }
        self.phone_browse.folder_ended(FolderEnd::Failed {
            host: entry.host_key().clone(),
            asked: entry.request().rel_path.clone(),
            reason: reason.to_owned(),
        });
        cx.notify();
    }

    /// Asks the host for the folder `request` names, for the phone's
    /// browser, which says why when the fetch is refused; returns the
    /// fetch's id.
    ///
    /// # Errors
    /// Why the fetch is refused, as one line: the same folder already
    /// fetching, a refused path or copies folder.
    pub(crate) fn phone_fetch_folder(
        &mut self,
        request: FetchRequest,
        cx: &mut Context<Self>,
    ) -> Result<String, String> {
        self.try_start_folder_fetch(request, cx)
            .map_err(|refusal| refusal.message().to_owned())
    }

    /// The phone's record of `entry`, saved at `path`: its host is the
    /// folder the copy sits in under the copies folder. `None`, with a
    /// warning, when `path` is not under that folder.
    fn phone_fetched_of(&self, entry: &FetchEntry, path: &Path) -> Option<crate::PhoneFetched> {
        let relative = self
            .remote_files_root
            .as_ref()
            .ok()
            .and_then(|root| path.strip_prefix(root).ok());
        let parts: Option<Vec<&str>> = relative.and_then(|relative| {
            relative
                .components()
                .map(|part| match part {
                    std::path::Component::Normal(part) => part.to_str(),
                    _ => None,
                })
                .collect()
        });
        let Some(parts) = parts.filter(|parts| parts.len() > 1) else {
            tracing::warn!(
                path = %path.display(),
                "phone: a saved fetch is not under the copies folder; not kept"
            );
            return None;
        };
        // The path on the host, so the copy can be stated and fetched again
        // whichever way it was fetched.
        let request = entry.request();
        let host_path = match entry.resolved_path() {
            // The form the host's path check accepts: a Windows host's
            // verbatim `\\?\` prefix is dropped.
            Some(resolved) => display_host_path(resolved),
            None if request.host_path => request.rel_path.clone(),
            None => {
                tracing::warn!(
                    path = %path.display(),
                    "phone: a saved fetch has no host path; not kept"
                );
                return None;
            }
        };
        Some(crate::PhoneFetched {
            host: parts[0].to_owned(),
            host_path,
            local_path: parts.join("/"),
            size: entry.size().unwrap_or_else(|| entry.bytes_received()),
            host_label: entry.host_label().to_owned(),
        })
    }

    /// Open containing folder on the file fetch `id` of `entry`, saved at
    /// `path`: its Fetching toast goes and its whole folder is fetched,
    /// which reveals the file once saved; when that fetch is refused, the
    /// refusal shows and the saved copy is revealed now.
    fn reveal_after_folder(
        &mut self,
        id: &str,
        entry: &FetchEntry,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dismiss_toast_key(&fetching_toast_key(id, entry.group()), cx);
        let folder = file_fetch::parent_folder_request(entry.request());
        match self.try_start_folder_fetch(folder, cx) {
            Ok(folder_id) => {
                if let Some(folder) = self.fetches.get_mut(&folder_id) {
                    folder.reveal_when_done(path);
                }
            }
            Err(refusal) => {
                self.push_toast(refusal.kind, &refusal.title, refusal.detail, cx);
                self.open_fetched(path, entry.request(), window, cx);
            }
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
            detail: Some(folder_files_detail(files, entry.host_label())),
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
        self.phone_folder_failed(&entry, reason, cx);
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
        if self.shape == crate::RootShape::Phone
            && let Some(tap) = entry.tap()
        {
            self.warn_tap(
                entry.host_key(),
                Some(tap),
                &failed_title(entry.file_name()),
                Some(detail),
                cx,
            );
            return;
        }
        self.push_toast(
            ToastKind::Error,
            &failed_title(entry.file_name()),
            Some(detail),
            cx,
        );
        self.reveal_noted_copy(&entry, window, cx);
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
            self.phone_folder_failed(&entry, &reason, cx);
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
            if self.shape == crate::RootShape::Phone
                && let Some(tap) = entry.tap()
            {
                self.warn_tap(
                    entry.host_key(),
                    Some(tap),
                    &failed_title(entry.file_name()),
                    Some(failure_detail(&entry, &reason)),
                    cx,
                );
                continue;
            }
            self.push_toast(
                ToastKind::Error,
                &failed_title(entry.file_name()),
                Some(failure_detail(&entry, &reason)),
                cx,
            );
            self.reveal_noted_copy(&entry, window, cx);
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
            at: None,
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
            | OpenJob::VsCodeWorkspace(_)
            | OpenJob::LinearIssue { .. } => None,
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

    use super::{absolute_root, files_count, folder_files_detail};

    #[test]
    fn folder_files_detail_uses_the_singular_for_one_file() {
        assert_eq!(folder_files_detail(1, "devbox"), "1 file from devbox");
        assert_eq!(folder_files_detail(2, "devbox"), "2 files from devbox");
        assert_eq!(folder_files_detail(0, "devbox"), "0 files from devbox");
    }

    #[test]
    fn files_count_uses_the_singular_for_one_file() {
        assert_eq!(files_count(1), "1 file");
        assert_eq!(files_count(3), "3 files");
    }

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
