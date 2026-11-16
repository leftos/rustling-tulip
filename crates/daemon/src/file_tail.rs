//! Following a file for `TailFile`: which paths a follow takes, and the
//! follow itself, which sends the file's last [`TAIL_CHUNK`] bytes, then
//! each new stretch as it arrives, read on a `notify` wake or a [`POLL`]
//! tick. Any regular file the daemon's user can read may be followed; a
//! relative path is taken from the session's working folder.
//!
//! A requested path is judged twice. [`check`] reads only its text and
//! refuses empty, network and device paths, over-long or over-deep paths,
//! drive- or root-relative paths and paths climbing above their root before
//! the disk is touched; the disk work after it ([`prepare`], each follow's
//! read) runs on a blocking thread, bounded by [`STEP_BOUND`] and stopped
//! early by the follow's cancel token, follows links to the file they name
//! (a share a link or a mapped drive reaches included), and reads only a
//! regular file. Network and device paths are refused by their text alone.
//!
//! [`open_inside`] and [`Folders`] are a folder fetch's: the same open,
//! confined to the fetched folder by where the opened handle really is.

use crate::file_fetch::{CanonicalRoot, send_unless_cancelled};
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

/// A session's folders: its working folder first, then its other members'
/// folders, its worktrees and its `--add-dir` folders. A follow takes a
/// relative path from the first.
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
/// from its text alone: a relative `path` is taken from `working`, the
/// session's working folder. Touches no disk.
///
/// # Errors
/// Why the path is refused: empty, longer than [`MAX_PATH_CHARS`], deeper
/// than [`MAX_PATH_DEPTH`], a network or device path, drive- or
/// root-relative, climbing above its root, or relative with no `working`.
pub fn check(working: Option<&Path>, path: &str) -> Result<PathBuf, String> {
    check_shape(path)?;
    let named = Path::new(path);
    let joined = if named.is_absolute() {
        named.to_path_buf()
    } else {
        let Some(working) = working else {
            return Err("the session has no folder".to_owned());
        };
        working.join(named)
    };
    normalize(&joined).ok_or_else(|| format!("{path:?} climbs above its root"))
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

/// The canonical folders a folder fetch's opens must land inside.
#[derive(Clone, Debug)]
pub struct Folders {
    canonical: Vec<PathBuf>,
}

impl Folders {
    /// The one folder `root`, already canonical: what a folder fetch checks
    /// each opened file against.
    pub(crate) fn of_root(root: &CanonicalRoot) -> Self {
        Self {
            canonical: vec![root.path().to_path_buf()],
        }
    }

    /// Whether `resolved`, a canonical path, sits inside one of the folders.
    pub(crate) fn holds(&self, resolved: &Path) -> bool {
        self.canonical
            .iter()
            .any(|folder| resolved.starts_with(folder))
    }
}

/// [`locate`] `normal` on a blocking thread, giving up after
/// [`STEP_BOUND`], so a path that cannot be followed is refused at the start.
///
/// # Errors
/// Why the path is refused, or that resolving it did not return in time.
pub async fn prepare(normal: PathBuf) -> Result<(), String> {
    let cancel = CancellationToken::new();
    let step_cancel = cancel.clone();
    let step = tokio::task::spawn_blocking(move || locate(&normal, &step_cancel).map(|_| ()));
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

/// The file `normal` names with every link followed, wherever it leads: a
/// share a link or a mapped drive reaches included, as any readable file
/// is. A path whose end does not exist yet is placed under its nearest
/// existing ancestor; a link whose target is missing, or a part that fails
/// to resolve for any reason but absence, is refused. Disk work: run it on
/// a blocking thread.
///
/// # Errors
/// Why the path is refused.
pub fn locate(normal: &Path, cancel: &CancellationToken) -> Result<PathBuf, String> {
    canonical_allowing_missing(normal, cancel)
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
pub(crate) enum StepError {
    /// The file does not exist (yet).
    Missing,
    /// The path was refused or the file could not be read.
    Failed(String),
}

/// Open `resolved` as [`open_regular`] does and check where the opened
/// handle really is: inside `folders`, which also catches a link swapped in
/// after the path was resolved. A folder fetch's open.
///
/// The handle's own path is read from the handle before anything is sent.
/// Linux and macOS are the supported Unix targets; on any other Unix the
/// handle's path cannot be read and every open is refused.
pub(crate) fn open_inside(folders: &Folders, resolved: &Path) -> Result<File, StepError> {
    let file = open_regular(resolved)?;
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

/// Open `resolved`, the path [`locate`] made, for reading: a follow's open.
///
/// On Unix the open itself refuses a link as the path's last component,
/// which only a link swapped in after [`locate`] can be (`O_NOFOLLOW`),
/// cannot make a terminal device the daemon's controlling terminal
/// (`O_NOCTTY`) and does not block on a FIFO (`O_NONBLOCK`); the handle's
/// own type is checked before anything is read, and `O_NONBLOCK` is then
/// cleared so a read that can report `EAGAIN` waits instead.
#[cfg(unix)]
pub(crate) fn open_regular(resolved: &Path) -> Result<File, StepError> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NOCTTY | libc::O_NONBLOCK)
        .open(resolved)
        .map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                StepError::Missing
            } else {
                StepError::Failed(format!("opening {}: {err}", resolved.display()))
            }
        })?;
    let meta = file
        .metadata()
        .map_err(|err| StepError::Failed(format!("reading {}: {err}", resolved.display())))?;
    if !meta.is_file() {
        return Err(StepError::Failed(format!(
            "{} is not a regular file",
            resolved.display()
        )));
    }
    clear_nonblock(&file).map_err(|err| {
        StepError::Failed(format!(
            "cannot make {} read blocking: {err}",
            resolved.display()
        ))
    })?;
    Ok(file)
}

/// Clear `O_NONBLOCK` from an opened regular file's descriptor, so a read
/// that can still report `EAGAIN` (mandatory locking, a FUSE file system)
/// waits instead of failing.
#[cfg(unix)]
fn clear_nonblock(file: &File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd as _;
    let fd = file.as_raw_fd();
    // SAFETY: `fd` stays open while `file` lives, and `F_GETFL` only reads
    // the descriptor's flags.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` stays open while `file` lives, and `F_SETFL` takes only
    // the flags to set, with no buffer.
    let cleared = unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) };
    if cleared == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Open `resolved`, the path [`locate`] made, for reading: a follow's open.
/// Whether it is a regular file is checked on the handle by its caller.
#[cfg(not(unix))]
pub(crate) fn open_regular(resolved: &Path) -> Result<File, StepError> {
    File::open(resolved).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            StepError::Missing
        } else {
            StepError::Failed(format!("opening {}: {err}", resolved.display()))
        }
    })
}

/// The path the open handle `file` refers to, in the form
/// `std::fs::canonicalize` gives.
#[cfg(windows)]
pub(crate) fn final_path(file: &File, _opened_as: &Path) -> std::io::Result<PathBuf> {
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

/// The path the open handle `file` refers to, read from the handle itself:
/// its `/proc/self/fd` link. Linux is the only Unix target here without
/// `F_GETPATH`; another Unix has no such link, so its read fails and the
/// open is refused. There is no fallback to the opened path, which a swap
/// could make stale.
#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn final_path(file: &File, _opened_as: &Path) -> std::io::Result<PathBuf> {
    use std::os::fd::AsRawFd as _;
    std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

/// The path the open handle `file` refers to, read from the handle itself
/// with `F_GETPATH`. A failed call refuses the open, rather than walking the
/// opened path again, which a swap could make stale.
#[cfg(target_os = "macos")]
pub(crate) fn final_path(file: &File, _opened_as: &Path) -> std::io::Result<PathBuf> {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::ffi::OsStrExt as _;
    let mut buf = [0u8; libc::PATH_MAX as usize];
    // SAFETY: `buf` holds `libc::PATH_MAX` bytes, the most `F_GETPATH`
    // writes, and `file` keeps its descriptor open while the call runs.
    let written = unsafe {
        libc::fcntl(
            file.as_raw_fd(),
            libc::F_GETPATH,
            buf.as_mut_ptr().cast::<libc::c_char>(),
        )
    };
    if written == -1 {
        return Err(std::io::Error::last_os_error());
    }
    let len = buf.iter().position(|byte| *byte == 0).unwrap_or(buf.len());
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(&buf[..len])))
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
}

/// Follow `spec`'s file until `cancel` fires or `tx` closes. The path is
/// resolved again before every read, so each read takes the file a link
/// names at that moment. A missing file is waited for. Each read
/// runs on a blocking thread, one at a time: a read that has not returned
/// within [`STEP_BOUND`] is left running and waited for again on the next
/// poll, and `cancel` ends the follow without waiting for it.
pub async fn follow(spec: Follow, tx: mpsc::Sender<DaemonMessage>, cancel: CancellationToken) {
    let (wake_tx, mut wake) = mpsc::unbounded_channel();
    let mut watcher: Option<RecommendedWatcher> = None;
    let mut cursor = Cursor::default();
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
    let normal = spec.normal.clone();
    let cursor = cursor.clone();
    let cancel = cancel.clone();
    tokio::task::spawn_blocking(move || {
        let resolved = match locate(&normal, &cancel) {
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
            read: read_at(&resolved, &cursor, &cancel),
            watcher,
        }
    })
}

/// Read what `cursor` has not sent of the file at `resolved`: at most
/// [`TAIL_CHUNK`] bytes.
fn read_at(
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
    let mut file = open_regular(resolved)?;
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
    /// to the follow (its first read, or its return after going missing),
    /// shrank, was replaced, was rewritten before the offset or gained more
    /// than [`TAIL_CHUNK`] bytes since the last read.
    fn plan(&self, file: &mut File, len: u64, file_id: (u64, u64)) -> std::io::Result<(u64, bool)> {
        let tail_start = len.saturating_sub(TAIL_CHUNK as u64);
        let Some(offset) = self.offset else {
            return Ok((tail_start, true));
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
            std::os::windows::fs::symlink_dir(target, link).is_ok() || make_junction(link, target)
        }
    }

    /// Make `path` a FIFO on Unix. Whether one was made.
    #[cfg(unix)]
    pub(crate) fn make_fifo(path: &Path) -> bool {
        use std::os::unix::ffi::OsStrExt as _;
        let Ok(raw) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
            return false;
        };
        // SAFETY: `raw` is a NUL-terminated path under the test's own folder,
        // and `mkfifo` only reads it.
        unsafe { libc::mkfifo(raw.as_ptr(), 0o600) == 0 }
    }

    /// Make `link` a junction to the folder `target` on Windows; elsewhere,
    /// which has no junctions, nothing. Whether one was made.
    pub(crate) fn make_junction(link: &Path, target: &Path) -> bool {
        #[cfg(windows)]
        {
            std::process::Command::new("cmd")
                .arg("/C")
                .arg("mklink")
                .arg("/J")
                .arg(link)
                .arg(target)
                .output()
                .is_ok_and(|out| out.status.success())
        }
        #[cfg(not(windows))]
        {
            let _ = (link, target);
            false
        }
    }

    /// Block while a hold names `path`'s file.
    pub(crate) fn before_open(path: &Path) {
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

    /// [`check`] against the first of `folders`, then [`locate`].
    fn resolve(folders: &[PathBuf], path: &str) -> Result<PathBuf, String> {
        let normal = check(folders.first().map(PathBuf::as_path), path)?;
        locate(&normal, &CancellationToken::new())
    }

    /// The one folder `folder`, canonicalized, as a folder fetch holds it.
    fn folders_of(folder: &Path) -> Folders {
        Folders {
            canonical: vec![std::fs::canonicalize(folder).expect("canonical folder")],
        }
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
    fn a_path_outside_the_folders_resolves_and_shapes_are_still_refused() {
        let (base, folder) = folder_with_sibling("dotdot");
        let folders = [folder.clone()];
        assert!(resolve(&folders, "logs/../b.log").is_ok());
        let outside = std::fs::canonicalize(base.path().join("outside.log")).expect("canonical");
        assert_eq!(resolve(&folders, "../outside.log"), Ok(outside.clone()));
        let deep = format!("{}/logs/../../outside.log", folder.display());
        assert_eq!(resolve(&folders, &deep), Ok(outside.clone()));
        let absolute = base.path().join("outside.log").display().to_string();
        assert_eq!(resolve(&folders, &absolute), Ok(outside));
        assert_eq!(resolve(&folders, ""), Err("the path is empty".to_owned()));
        assert_eq!(
            resolve(&[], "a.log"),
            Err("the session has no folder".to_owned()),
            "no folder, no relative path"
        );
        let climb = if cfg!(windows) {
            r"C:\..\x.log"
        } else {
            "/../x.log"
        };
        let err = resolve(&folders, climb).expect_err("climbs");
        assert!(err.contains("climbs above its root"), "{err}");
    }

    #[test]
    fn a_link_out_of_the_folder_resolves_to_its_target() {
        let (base, folder) = folder_with_sibling("link-out");
        let elsewhere = base.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).expect("create folder");
        std::fs::write(elsewhere.join("secret.log"), "secret").expect("write secret");
        // No link without developer mode, elevation or `cmd`: nothing to test.
        if !hooks::make_dir_link(&folder.join("link"), &elsewhere) {
            return;
        }
        let target = std::fs::canonicalize(elsewhere.join("secret.log")).expect("canonical");
        let folders = [folder.clone()];
        assert_eq!(resolve(&folders, "link/secret.log"), Ok(target.clone()));
        let opened = open_regular(&target).expect("the target opens");
        drop(opened);
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

    /// A session on a mapped network drive or a UNC folder follows its own
    /// files: a path that resolves to `\\?\UNC\` is located like any other.
    #[cfg(windows)]
    #[test]
    fn a_path_that_resolves_to_a_share_is_located() {
        let (base, _folder) = folder_with_sibling("unc");
        let local = std::fs::canonicalize(base.path().join("outside.log")).expect("canonical");
        let text = local.display().to_string();
        let Some(rest) = text.strip_prefix(r"\\?\") else {
            return;
        };
        let mut chars = rest.chars();
        let (Some(drive), Some(':')) = (chars.next(), chars.next()) else {
            return;
        };
        let share = PathBuf::from(format!(r"\\localhost\{drive}${}", chars.as_str()));
        // The admin share needs an administrator and the Server service;
        // without them there is no share to test.
        let Ok(canonical) = std::fs::canonicalize(&share) else {
            return;
        };
        if !canonical.display().to_string().starts_with(r"\\?\UNC\") {
            return;
        }
        assert_eq!(locate(&share, &CancellationToken::new()), Ok(canonical));
    }

    #[test]
    fn the_opened_handle_is_checked_against_the_folders() {
        let (base, folder) = folder_with_sibling("handle");
        std::fs::write(folder.join("logs").join("in.log"), "in").expect("write log");
        let folders = folders_of(&folder);
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

    /// The bound every wait on an open takes here; passing it fails as hung.
    #[cfg(unix)]
    const OPEN_GUARD: std::time::Duration = std::time::Duration::from_secs(30);

    /// [`open_regular`] on a thread of its own, failing the test as hung when
    /// it has not returned within [`OPEN_GUARD`].
    #[cfg(unix)]
    fn open_guarded(path: &Path) -> Result<File, StepError> {
        let path = path.to_path_buf();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(open_regular(&path));
        });
        rx.recv_timeout(OPEN_GUARD)
            .expect("hung: the open never returned")
    }

    #[cfg(unix)]
    #[test]
    fn open_regular_refuses_a_fifo() {
        let (_base, folder) = folder_with_sibling("fifo");
        let fifo = folder.join("pipe");
        assert!(hooks::make_fifo(&fifo), "a FIFO could not be made");
        let err = open_guarded(&fifo).expect_err("a FIFO is not opened");
        assert!(
            matches!(&err, StepError::Failed(why) if why.contains("not a regular file")),
            "{err:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn open_regular_refuses_a_symlink_as_its_last_part() {
        let (_base, folder) = folder_with_sibling("open-link");
        let target = folder.join("logs").join("in.log");
        std::fs::write(&target, "in").expect("write log");
        let link = folder.join("link.log");
        std::os::unix::fs::symlink(&target, &link).expect("make the link");
        assert!(open_regular(&target).is_ok(), "the real file opens");
        let err = open_regular(&link).expect_err("a link is not opened");
        assert!(
            matches!(&err, StepError::Failed(why) if why.contains("opening")),
            "{err:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn open_regular_returns_a_blocking_handle() {
        use std::os::fd::AsRawFd as _;
        let (_base, folder) = folder_with_sibling("blocking");
        let path = folder.join("logs").join("in.log");
        std::fs::write(&path, "in").expect("write log");
        let file = open_regular(&path).expect("a regular file opens");
        // SAFETY: `file` stays open, and `F_GETFL` only reads the
        // descriptor's flags.
        let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
        assert_ne!(flags, -1, "F_GETFL");
        assert_eq!(flags & libc::O_NONBLOCK, 0, "the handle reads blocking");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn final_path_follows_a_rename_by_handle() {
        let (_base, folder) = folder_with_sibling("rename");
        let before = folder.join("logs").join("in.log");
        std::fs::write(&before, "in").expect("write log");
        let file = File::open(&before).expect("open the log");
        let after = folder.join("logs").join("renamed.log");
        std::fs::rename(&before, &after).expect("rename the log");
        let opened = final_path(&file, &before).expect("the handle's path");
        let expected = std::fs::canonicalize(&after).expect("the renamed path");
        assert_eq!(opened, expected, "the handle follows the rename");
    }
}
