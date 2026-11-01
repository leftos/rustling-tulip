//! Following a file inside a session's folders for `TailFile`: which paths a
//! session may follow, and the follow itself, which sends the file's last
//! [`TAIL_CHUNK`] bytes, then each new stretch as it arrives, read on a
//! `notify` wake or a [`POLL`] tick.
//!
//! A requested path is judged twice. [`check`] reads only its text and
//! refuses network and device paths, over-long or over-deep paths and
//! anything outside the session's folders before the disk is touched; the
//! disk work after it ([`prepare`], each follow's read) runs on a blocking
//! thread, bounded by [`STEP_BOUND`] and stopped early by the follow's
//! cancel token, and checks where the opened handle really is.

use crate::session::SessionRecord;
use base64::Engine as _;
use notify::{RecommendedWatcher, RecursiveMode, Watcher as _};
use protocol::{DaemonMessage, SpawnTarget};
use std::fs::File;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Component, Path, PathBuf, Prefix};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

/// The most bytes one [`DaemonMessage::FileTail`] carries, how much of the
/// file's end the first one sends, and the largest gap a follow reads
/// through rather than jumping to the file's end.
pub const TAIL_CHUNK: usize = 64 * 1024;
/// The most files one connection follows at once.
pub const MAX_FOLLOWS: usize = 4;
/// How often a follow looks at its file when no watcher event woke it, for
/// file systems and folders a watcher does not cover.
const POLL: Duration = Duration::from_secs(1);
/// The longest path a follow takes, in characters.
const MAX_PATH_CHARS: usize = 1024;
/// The most components a followed path may have.
const MAX_PATH_DEPTH: usize = 64;
/// How long one blocking step (resolving, opening, reading) is waited on
/// before the follow gives up on it for that poll.
pub const STEP_BOUND: Duration = Duration::from_secs(5);
/// How many bytes before the offset are compared with what was sent, to
/// tell a file rewritten in place.
const CHECK_BYTES: usize = 64;

/// The folders a session's follows may read: its working folder first, then
/// its other members' folders, its worktrees and its `--add-dir` folders.
#[must_use]
pub fn session_folders(record: &SessionRecord) -> Vec<PathBuf> {
    let members = record
        .members
        .iter()
        .map(|member| member.worktree_path.as_str());
    let worktrees = record.worktree_paths.iter().map(String::as_str);
    let standalone: Vec<&str> = match record.spawn_config.as_ref().map(|config| &config.target) {
        Some(SpawnTarget::Standalone { cwd, add_dirs }) => {
            cwd.iter().chain(add_dirs).map(String::as_str).collect()
        }
        _ => Vec::new(),
    };
    let mut folders: Vec<PathBuf> = Vec::new();
    for folder in members.chain(worktrees).chain(standalone) {
        let folder = PathBuf::from(folder);
        if !folder.as_os_str().is_empty() && !folders.contains(&folder) {
            folders.push(folder);
        }
    }
    folders
}

/// The path `path` names, absolute and with `.` and `..` applied, judged
/// from its text alone: a relative `path` is taken from the first of
/// `listed`, and the result must sit inside one of `listed` (compared
/// without case on Windows). Touches no disk.
///
/// # Errors
/// Why the path is refused: empty, longer than [`MAX_PATH_CHARS`], deeper
/// than [`MAX_PATH_DEPTH`], a network or device path, drive- or
/// root-relative, climbing above its root, or outside every folder.
pub fn check(listed: &[PathBuf], path: &str) -> Result<PathBuf, String> {
    check_shape(path)?;
    let named = Path::new(path);
    let joined = if named.is_absolute() {
        named.to_path_buf()
    } else {
        let Some(working) = listed.first() else {
            return Err("the session has no folder".to_owned());
        };
        working.join(named)
    };
    let normal = normalize(&joined).ok_or_else(|| format!("{path:?} climbs above its root"))?;
    if listed
        .iter()
        .any(|folder| lexically_inside(&normal, folder))
    {
        Ok(normal)
    } else {
        Err(format!("{path:?} is outside the session's folders"))
    }
}

/// Refuse a path by its shape: empty, too long, too deep, naming a network
/// share or a device, or relative to a drive or root.
fn check_shape(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("the path is empty".to_owned());
    }
    if path.chars().count() > MAX_PATH_CHARS {
        return Err(format!(
            "the path is longer than {MAX_PATH_CHARS} characters"
        ));
    }
    if is_network_or_device(path) {
        return Err(format!("{path:?} names a network or device path"));
    }
    let named = Path::new(path);
    if named.components().count() > MAX_PATH_DEPTH {
        return Err(format!(
            "the path is more than {MAX_PATH_DEPTH} components deep"
        ));
    }
    let starts_bare = matches!(named.components().next(), Some(Component::Prefix(_)));
    if !named.is_absolute() && (named.has_root() || starts_bare) {
        return Err(format!(
            "{path:?} is relative to a drive or root, not a folder"
        ));
    }
    Ok(())
}

/// Whether `path` opens with two separators (a UNC share, `\\?\`, `\\.\`)
/// or a Windows prefix other than a drive letter.
fn is_network_or_device(path: &str) -> bool {
    let mut chars = path.chars();
    let two_separators = matches!(
        (chars.next(), chars.next()),
        (Some('\\' | '/'), Some('\\' | '/'))
    );
    let odd_prefix = matches!(
        Path::new(path).components().next(),
        Some(Component::Prefix(prefix)) if !matches!(prefix.kind(), Prefix::Disk(_))
    );
    two_separators || odd_prefix
}

/// Whether `path` starts with every component of `folder`, by text.
fn lexically_inside(path: &Path, folder: &Path) -> bool {
    let folder = normalize(folder).unwrap_or_else(|| folder.to_path_buf());
    let mut parts = path.components();
    folder.components().all(|part| {
        parts
            .next()
            .is_some_and(|other| same_component(part, other))
    })
}

/// Two path components alike, without case on Windows.
fn same_component(one: Component<'_>, other: Component<'_>) -> bool {
    if cfg!(windows) {
        one.as_os_str().to_string_lossy().to_lowercase()
            == other.as_os_str().to_string_lossy().to_lowercase()
    } else {
        one == other
    }
}

/// `path` with `.` dropped and each `..` taking off the component before it;
/// `None` when a `..` would climb above the root.
fn normalize(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    return None;
                }
                out.pop();
            }
            other => out.push(other),
        }
    }
    Some(out)
}

/// A session's folders as recorded, and their canonical forms, taken once
/// when a follow starts.
#[derive(Clone, Debug)]
pub struct Folders {
    canonical: Vec<PathBuf>,
}

impl Folders {
    /// Canonicalize `listed`. Disk work: run it on a blocking thread. A
    /// folder that does not resolve is left out, so nothing under it is
    /// followed.
    #[must_use]
    pub fn canonicalize(listed: &[PathBuf]) -> Self {
        let canonical = listed
            .iter()
            .filter_map(|folder| {
                std::fs::canonicalize(folder)
                    .inspect_err(|err| {
                        debug!(folder = %folder.display(), error = %err, "file_tail: a session folder does not resolve");
                    })
                    .ok()
            })
            .collect();
        Self { canonical }
    }

    fn holds(&self, resolved: &Path) -> bool {
        self.canonical
            .iter()
            .any(|folder| resolved.starts_with(folder))
    }
}

/// Canonicalize `listed` and [`locate`] `normal` on a blocking thread, giving
/// up after [`STEP_BOUND`]: the folders a follow of `normal` checks against.
///
/// # Errors
/// Why the path is refused, or that resolving it did not return in time.
pub async fn prepare(listed: Vec<PathBuf>, normal: PathBuf) -> Result<Folders, String> {
    let cancel = CancellationToken::new();
    let step_cancel = cancel.clone();
    let step = tokio::task::spawn_blocking(move || {
        let folders = Folders::canonicalize(&listed);
        locate(&folders, &normal, &step_cancel).map(|_| folders)
    });
    match tokio::time::timeout(STEP_BOUND, step).await {
        Ok(Ok(found)) => found,
        Ok(Err(err)) => Err(format!("resolving the path failed: {err}")),
        Err(_) => {
            cancel.cancel();
            Err(format!(
                "the path did not resolve within {} s",
                STEP_BOUND.as_secs()
            ))
        }
    }
}

/// The file `normal` names with every link followed, when it sits inside
/// `folders`. A path whose end does not exist yet is placed under its
/// nearest existing ancestor; a link whose target is missing, or a part that
/// fails to resolve for any reason but absence, is refused. Disk work: run
/// it on a blocking thread.
///
/// # Errors
/// Why the path is refused.
pub fn locate(
    folders: &Folders,
    normal: &Path,
    cancel: &CancellationToken,
) -> Result<PathBuf, String> {
    let resolved = canonical_allowing_missing(normal, cancel)?;
    if folders.holds(&resolved) {
        Ok(resolved)
    } else {
        Err(format!(
            "it resolves outside the session's folders (to {})",
            resolved.display()
        ))
    }
}

/// The canonical form of `path`'s nearest existing ancestor, with the rest
/// of `path` after it.
fn canonical_allowing_missing(path: &Path, cancel: &CancellationToken) -> Result<PathBuf, String> {
    let mut existing = path;
    loop {
        if cancel.is_cancelled() {
            return Err(STOPPED.to_owned());
        }
        match std::fs::canonicalize(existing) {
            Ok(canonical) => {
                let rest = path.strip_prefix(existing).map_err(|err| {
                    format!(
                        "{} is not under {}: {err}",
                        path.display(),
                        existing.display()
                    )
                })?;
                return Ok(if rest.as_os_str().is_empty() {
                    canonical
                } else {
                    canonical.join(rest)
                });
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                if std::fs::symlink_metadata(existing).is_ok() {
                    return Err(format!(
                        "{} is a link whose target does not resolve",
                        existing.display()
                    ));
                }
                existing = existing
                    .parent()
                    .ok_or_else(|| format!("no part of {} exists", path.display()))?;
            }
            Err(err) => return Err(format!("{} does not resolve: {err}", existing.display())),
        }
    }
}

/// Why a blocking step gave up when its follow was stopped.
const STOPPED: &str = "the follow was stopped";

/// Why a read found nothing to send.
#[derive(Debug)]
enum StepError {
    /// The file does not exist (yet).
    Missing,
    /// The path was refused or the file could not be read.
    Failed(String),
}

/// Open `resolved` and check where the opened handle really is, which also
/// catches a link swapped in after [`locate`].
fn open_inside(folders: &Folders, resolved: &Path) -> Result<File, StepError> {
    let file = File::open(resolved).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            StepError::Missing
        } else {
            StepError::Failed(format!("opening {}: {err}", resolved.display()))
        }
    })?;
    let opened = final_path(&file, resolved).map_err(|err| {
        StepError::Failed(format!(
            "cannot tell where {} opened: {err}",
            resolved.display()
        ))
    })?;
    if folders.holds(&opened) {
        Ok(file)
    } else {
        Err(StepError::Failed(format!(
            "{} opened outside the session's folders (at {})",
            resolved.display(),
            opened.display()
        )))
    }
}

/// The path the open handle `file` refers to, in the form
/// `std::fs::canonicalize` gives.
#[cfg(windows)]
fn final_path(file: &File, _opened_as: &Path) -> std::io::Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt as _;
    use std::os::windows::io::AsRawHandle as _;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        FILE_NAME_NORMALIZED, GETFINALPATHNAMEBYHANDLE_FLAGS, GetFinalPathNameByHandleW,
        VOLUME_NAME_DOS,
    };
    let handle = HANDLE(file.as_raw_handle());
    let flags = GETFINALPATHNAMEBYHANDLE_FLAGS(FILE_NAME_NORMALIZED.0 | VOLUME_NAME_DOS.0);
    let mut buf = vec![0u16; 512];
    loop {
        // SAFETY: `handle` stays open while `file` lives, and the call
        // writes at most `buf.len()` units into `buf`.
        let len = unsafe { GetFinalPathNameByHandleW(handle, &mut buf, flags) } as usize;
        if len == 0 {
            return Err(std::io::Error::last_os_error());
        }
        if len < buf.len() {
            buf.truncate(len);
            return Ok(PathBuf::from(std::ffi::OsString::from_wide(&buf)));
        }
        // Too small: `len` is the size needed, its terminating null included.
        buf.resize(len, 0);
    }
}

/// The path the open handle `file` refers to: its `/proc/self/fd` link
/// where there is one, else `opened_as` canonicalized again.
#[cfg(not(windows))]
fn final_path(file: &File, opened_as: &Path) -> std::io::Result<PathBuf> {
    use std::os::fd::AsRawFd as _;
    std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
        .or_else(|_| std::fs::canonicalize(opened_as))
}

/// What tells one file from another at the same path: the volume and file
/// index on Windows.
#[cfg(windows)]
fn file_id(file: &File) -> std::io::Result<(u64, u64)> {
    use std::os::windows::io::AsRawHandle as _;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the handle stays open while `file` lives, and `info` is a
    // valid place for the call to write.
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &raw mut info) }
        .map_err(std::io::Error::from)?;
    let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
    Ok((u64::from(info.dwVolumeSerialNumber), index))
}

/// What tells one file from another at the same path: the device and inode.
#[cfg(not(windows))]
fn file_id(file: &File) -> std::io::Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt as _;
    let meta = file.metadata()?;
    Ok((meta.dev(), meta.ino()))
}

/// What one follow sends and may read.
pub struct Follow {
    pub session_id: String,
    /// The path as the request named it, echoed on every message.
    pub path: String,
    /// The path [`check`] made of it.
    pub normal: PathBuf,
    /// The session's folders when the follow began.
    pub folders: Folders,
    /// Whether this follow replaces an earlier one of the same file, so its
    /// first message carries `reset`.
    pub restart: bool,
}

/// Follow `spec`'s file until `cancel` fires or `tx` closes. The path is
/// resolved again before every read, so a link planted later is refused
/// like one there at the start. A missing file is waited for. Each read
/// runs on a blocking thread, one at a time: a read that has not returned
/// within [`STEP_BOUND`] is left running and waited for again on the next
/// poll, and `cancel` ends the follow without waiting for it.
pub async fn follow(spec: Follow, tx: mpsc::Sender<DaemonMessage>, cancel: CancellationToken) {
    let (wake_tx, mut wake) = mpsc::unbounded_channel();
    let mut watcher: Option<RecommendedWatcher> = None;
    let mut cursor = Cursor {
        sent: spec.restart,
        ..Cursor::default()
    };
    let mut poll = tokio::time::interval(POLL);
    let mut pending: Option<JoinHandle<Step>> = None;
    loop {
        let mut step = pending.take().unwrap_or_else(|| {
            let wake = watcher.is_none().then(|| wake_tx.clone());
            start_step(&spec, &cursor, wake, &cancel)
        });
        let finished = tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            joined = &mut step => Some(joined),
            () = tokio::time::sleep(STEP_BOUND) => None,
        };
        match finished {
            None => {
                let why = format!(
                    "a read has not returned in {} s; waiting for it",
                    STEP_BOUND.as_secs()
                );
                cursor.note(&spec.path, &why);
                pending = Some(step);
            }
            Some(Err(err)) => cursor.note(&spec.path, &format!("a read failed: {err}")),
            Some(Ok(found)) => {
                if found.watcher.is_some() {
                    watcher = found.watcher;
                }
                let sink = Sink {
                    spec: &spec,
                    tx: &tx,
                    cancel: &cancel,
                };
                if !cursor.apply(sink, found.read).await {
                    return;
                }
            }
        }
        continue_after_wait(&cancel, &mut wake, &mut poll).await;
        if cancel.is_cancelled() {
            return;
        }
    }
}

/// What one blocking step found, and the watcher it started when asked.
struct Step {
    read: Result<Read, StepError>,
    watcher: Option<RecommendedWatcher>,
}

/// Bytes read from `offset`; `reset` when the client starts over with them.
struct Read {
    data: Vec<u8>,
    offset: u64,
    reset: bool,
    file_id: (u64, u64),
}

/// Resolve, open and read the file on a blocking thread, starting a watcher
/// on its folder when `wake` is given.
fn start_step(
    spec: &Follow,
    cursor: &Cursor,
    wake: Option<mpsc::UnboundedSender<()>>,
    cancel: &CancellationToken,
) -> JoinHandle<Step> {
    let folders = spec.folders.clone();
    let normal = spec.normal.clone();
    let cursor = cursor.clone();
    let cancel = cancel.clone();
    tokio::task::spawn_blocking(move || {
        let resolved = match locate(&folders, &normal, &cancel) {
            Ok(resolved) => resolved,
            Err(why) => {
                return Step {
                    read: Err(StepError::Failed(why)),
                    watcher: None,
                };
            }
        };
        let watcher = wake.and_then(|wake| start_watcher(&resolved, wake));
        Step {
            read: read_at(&folders, &resolved, &cursor, &cancel),
            watcher,
        }
    })
}

/// Read what `cursor` has not sent of the file at `resolved`: at most
/// [`TAIL_CHUNK`] bytes.
fn read_at(
    folders: &Folders,
    resolved: &Path,
    cursor: &Cursor,
    cancel: &CancellationToken,
) -> Result<Read, StepError> {
    #[cfg(test)]
    hooks::before_open(resolved);
    if cancel.is_cancelled() {
        return Err(StepError::Failed(STOPPED.to_owned()));
    }
    let failed =
        |err: std::io::Error| StepError::Failed(format!("reading {}: {err}", resolved.display()));
    let mut file = open_inside(folders, resolved)?;
    let meta = file.metadata().map_err(failed)?;
    if !meta.is_file() {
        return Err(StepError::Failed("the path is not a file".to_owned()));
    }
    let file_id = file_id(&file).map_err(failed)?;
    let len = meta.len();
    let (start, reset) = cursor.plan(&mut file, len, file_id).map_err(failed)?;
    let end = len.min(start.saturating_add(TAIL_CHUNK as u64));
    let mut data = Vec::new();
    if start < end {
        file.seek(SeekFrom::Start(start)).map_err(failed)?;
        (&mut file)
            .take(end - start)
            .read_to_end(&mut data)
            .map_err(failed)?;
    }
    Ok(Read {
        data,
        offset: start,
        reset,
        file_id,
    })
}

/// Wait for a watcher wake, a poll tick or `cancel`; a burst of wakes
/// counts once.
async fn continue_after_wait(
    cancel: &CancellationToken,
    wake: &mut mpsc::UnboundedReceiver<()>,
    poll: &mut tokio::time::Interval,
) {
    tokio::select! {
        biased;
        () = cancel.cancelled() => {}
        Some(()) = wake.recv() => while wake.try_recv().is_ok() {},
        _ = poll.tick() => {}
    }
}

/// Watch `path`'s folder, waking the follow for any event naming a file of
/// `path`'s name. No folder yet, or a watcher that cannot start, leaves the
/// follow on its poll. Disk work: run it on a blocking thread.
fn start_watcher(path: &Path, wake: mpsc::UnboundedSender<()>) -> Option<RecommendedWatcher> {
    let folder = path.parent().filter(|folder| folder.is_dir())?;
    let name = path.file_name()?.to_owned();
    let mut watcher = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
        if let Ok(event) = result
            && event
                .paths
                .iter()
                .any(|seen| seen.file_name() == Some(&name))
        {
            let _ = wake.send(());
        }
    })
    .inspect_err(|err| debug!(error = %err, "file_tail: no watcher; polling only"))
    .ok()?;
    if let Err(err) = watcher.watch(folder, RecursiveMode::NonRecursive) {
        debug!(folder = %folder.display(), error = %err, "file_tail: cannot watch the folder; polling only");
        return None;
    }
    Some(watcher)
}

/// Where a follow's messages go.
#[derive(Clone, Copy)]
struct Sink<'a> {
    spec: &'a Follow,
    tx: &'a mpsc::Sender<DaemonMessage>,
    cancel: &'a CancellationToken,
}

/// Where a follow is in its file.
#[derive(Clone, Default)]
struct Cursor {
    /// The offset after the last byte sent; `None` before the file was
    /// first read and while it is missing.
    offset: Option<u64>,
    /// The file's id when last read, to tell a replaced file.
    file_id: Option<(u64, u64)>,
    /// The last [`CHECK_BYTES`] bytes sent, ending at `offset`, to tell a
    /// file rewritten in place.
    last: Vec<u8>,
    /// Whether anything was sent, so a file that comes back starts over;
    /// set from the start for a restarted follow.
    sent: bool,
    /// The last refusal or read failure logged, so a repeat is not.
    noted: Option<String>,
}

impl Cursor {
    fn note(&mut self, path: &str, why: &str) {
        if self.noted.as_deref() != Some(why) {
            warn!(%path, %why, "file_tail: cannot read the followed file; waiting");
            self.noted = Some(why.to_owned());
        }
    }

    /// Where to read from in a file of `len` bytes and id `file_id`, and
    /// whether the client starts over there: the file's end when it is new
    /// to the follow, shrank, was replaced, was rewritten before the offset
    /// or gained more than [`TAIL_CHUNK`] bytes since the last read.
    fn plan(&self, file: &mut File, len: u64, file_id: (u64, u64)) -> std::io::Result<(u64, bool)> {
        let tail_start = len.saturating_sub(TAIL_CHUNK as u64);
        let Some(offset) = self.offset else {
            return Ok((tail_start, self.sent));
        };
        let changed =
            len < offset || self.file_id != Some(file_id) || !self.ends_with_last(file, offset)?;
        if changed || len - offset > TAIL_CHUNK as u64 {
            return Ok((tail_start, true));
        }
        Ok((offset, false))
    }

    /// Whether the bytes before `offset` are still the ones last sent.
    fn ends_with_last(&self, file: &mut File, offset: u64) -> std::io::Result<bool> {
        if self.last.is_empty() {
            return Ok(true);
        }
        let mut held = vec![0u8; self.last.len()];
        file.seek(SeekFrom::Start(offset - self.last.len() as u64))?;
        file.read_exact(&mut held)?;
        Ok(held == self.last)
    }

    /// Send what a read found, if anything. Returns false when the follow
    /// should end: `tx` closed or `cancel` fired.
    async fn apply(&mut self, sink: Sink<'_>, read: Result<Read, StepError>) -> bool {
        let read = match read {
            Ok(read) => read,
            Err(StepError::Missing) => {
                self.offset = None;
                return true;
            }
            Err(StepError::Failed(why)) => {
                self.note(&sink.spec.path, &why);
                return true;
            }
        };
        self.file_id = Some(read.file_id);
        if read.data.is_empty() && !read.reset {
            self.offset = Some(read.offset);
            return true;
        }
        let message = DaemonMessage::FileTail {
            session_id: sink.spec.session_id.clone(),
            path: sink.spec.path.clone(),
            data_b64: base64::engine::general_purpose::STANDARD.encode(&read.data),
            offset: read.offset,
            reset: read.reset,
        };
        if !send_unless_cancelled(sink.tx, sink.cancel, message).await {
            return false;
        }
        self.advance(&read);
        true
    }

    /// Move past `read`, which was sent.
    fn advance(&mut self, read: &Read) {
        if read.reset {
            self.last.clear();
        }
        self.last.extend_from_slice(&read.data);
        let surplus = self.last.len().saturating_sub(CHECK_BYTES);
        self.last.drain(..surplus);
        self.offset = Some(read.offset + read.data.len() as u64);
        self.sent = true;
    }
}

/// Send `message` unless `cancel` fires first: whether it was sent.
async fn send_unless_cancelled(
    tx: &mpsc::Sender<DaemonMessage>,
    cancel: &CancellationToken,
    message: DaemonMessage,
) -> bool {
    tokio::select! {
        biased;
        () = cancel.cancelled() => false,
        sent = tx.send(message) => sent.is_ok(),
    }
}

/// Holding a follow's open from a test, to stand for an open that hangs.
#[cfg(test)]
pub(crate) mod hooks {
    use crate::sync::lock;
    use std::ffi::OsString;
    use std::path::Path;
    use std::sync::{Arc, Condvar, Mutex, PoisonError};
    use tokio::sync::Notify;

    #[derive(Default)]
    struct Hold {
        name: OsString,
        entered: Notify,
        released: Mutex<bool>,
        wake: Condvar,
    }

    static HOLDS: Mutex<Vec<Arc<Hold>>> = Mutex::new(Vec::new());

    /// Holds every open of a file of one name until dropped.
    pub(crate) struct OpenHold(Arc<Hold>);

    impl OpenHold {
        /// Hold every open of a file named `name`.
        pub(crate) fn new(name: &str) -> Self {
            let hold = Arc::new(Hold {
                name: name.into(),
                ..Hold::default()
            });
            lock(&HOLDS).push(Arc::clone(&hold));
            Self(hold)
        }

        /// Resolves once an open is being held.
        pub(crate) async fn entered(&self) {
            self.0.entered.notified().await;
        }
    }

    impl Drop for OpenHold {
        fn drop(&mut self) {
            lock(&HOLDS).retain(|hold| !Arc::ptr_eq(hold, &self.0));
            *lock(&self.0.released) = true;
            self.0.wake.notify_all();
        }
    }

    /// Make `link` a link to the folder `target`: a symlink, else a junction
    /// on Windows (a symlink needs developer mode or elevation there).
    /// Whether one was made.
    pub(crate) fn make_dir_link(link: &Path, target: &Path) -> bool {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link).is_ok()
        }
        #[cfg(windows)]
        {
            if std::os::windows::fs::symlink_dir(target, link).is_ok() {
                return true;
            }
            std::process::Command::new("cmd")
                .arg("/C")
                .arg("mklink")
                .arg("/J")
                .arg(link)
                .arg(target)
                .output()
                .is_ok_and(|out| out.status.success())
        }
    }

    /// Block while a hold names `path`'s file.
    pub(super) fn before_open(path: &Path) {
        let held = lock(&HOLDS)
            .iter()
            .find(|hold| path.file_name() == Some(hold.name.as_os_str()))
            .cloned();
        let Some(hold) = held else {
            return;
        };
        hold.entered.notify_one();
        let mut released = lock(&hold.released);
        while !*released {
            released = hold
                .wake
                .wait(released)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; panic messages aid debugging"
)]
mod tests {
    use super::*;
    use crate::file_fetch::test_support::TestDir;

    fn folder_with_sibling(tag: &str) -> (TestDir, PathBuf) {
        let base = TestDir::new(tag);
        let folder = base.path().join("work");
        std::fs::create_dir_all(folder.join("logs")).expect("create folder");
        std::fs::write(base.path().join("outside.log"), "secret").expect("write sibling");
        (base, folder)
    }

    /// [`check`], then [`locate`] against the folders canonicalized.
    fn resolve(folders: &[PathBuf], path: &str) -> Result<PathBuf, String> {
        let normal = check(folders, path)?;
        locate(
            &Folders::canonicalize(folders),
            &normal,
            &CancellationToken::new(),
        )
    }

    #[test]
    fn a_relative_path_resolves_against_the_working_folder() {
        let (_base, folder) = folder_with_sibling("relative");
        let resolved = resolve(std::slice::from_ref(&folder), "logs/./a.log").expect("inside");
        let expected = std::fs::canonicalize(folder.join("logs"))
            .expect("canonical")
            .join("a.log");
        assert_eq!(resolved, expected, "a missing file sits under its folder");
    }

    #[test]
    fn dot_dot_stays_inside_or_is_refused() {
        let (_base, folder) = folder_with_sibling("dotdot");
        let folders = [folder.clone()];
        assert!(resolve(&folders, "logs/../b.log").is_ok());
        let err = resolve(&folders, "../outside.log").expect_err("outside");
        assert!(err.contains("outside the session's folders"), "{err}");
        let deep = format!("{}/logs/../../outside.log", folder.display());
        assert!(resolve(&folders, &deep).is_err());
        assert!(resolve(&folders, "").is_err());
        assert!(
            resolve(&[], "a.log").is_err(),
            "no folder, no relative path"
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_drive_or_root_relative_path_is_refused() {
        let (_base, folder) = folder_with_sibling("drive");
        let folders = [folder];
        let drive_relative = resolve(&folders, "C:a.log").expect_err("drive-relative");
        assert!(
            drive_relative.contains("relative to a drive or root"),
            "{drive_relative}"
        );
        assert!(resolve(&folders, r"\a.log").is_err());
    }

    #[test]
    fn the_opened_handle_is_checked_against_the_folders() {
        let (base, folder) = folder_with_sibling("handle");
        std::fs::write(folder.join("logs").join("in.log"), "in").expect("write log");
        let folders = Folders::canonicalize(std::slice::from_ref(&folder));
        assert!(open_inside(&folders, &folder.join("logs").join("in.log")).is_ok());
        // Opened past every path check, as a link swapped in after them is.
        let outside = base.path().join("outside.log");
        let err = open_inside(&folders, &outside).expect_err("opened outside");
        assert!(
            matches!(&err, StepError::Failed(why) if why.contains("opened outside")),
            "{err:?}"
        );
        // No link without developer mode, elevation or `cmd`: the rest needs one.
        let elsewhere = base.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).expect("create folder");
        std::fs::write(elsewhere.join("secret.log"), "secret").expect("write secret");
        if hooks::make_dir_link(&folder.join("link"), &elsewhere) {
            let err = open_inside(&folders, &folder.join("link").join("secret.log"))
                .expect_err("through a link");
            assert!(
                matches!(&err, StepError::Failed(why) if why.contains("opened outside")),
                "{err:?}"
            );
        }
    }
}
