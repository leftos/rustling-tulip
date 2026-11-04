//! The account store's locks and Claude Code's, each taken the way its owner takes it, so any process taking the same lock the same way is excluded.
//!
//! The store's locks ([`FileLock`]: the account lock `.lock` and each account's consume lock) hold byte 0 of a lock file,
//! the byte `msvcrt.locking` takes. Claude Code's locks ([`DirLock`]: its two credential locks and its config
//! lock) are directories in the `proper-lockfile` style: creating the directory is the mutex, a holder touches its mtime
//! while it lives, and a directory older than the lock's staleness belongs to a dead holder and may be taken over.
//! Off 64-bit Windows every lock returns [`LockError::Unsupported`].

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(all(windows, target_pointer_width = "64"))]
use std::sync::mpsc;
#[cfg(all(windows, target_pointer_width = "64"))]
use std::thread::JoinHandle;
use std::time::Duration;

use crate::paths::{AccountsRoot, ClaudePaths};

/// How long the account lock is waited for before giving up.
pub const ACCOUNT_LOCK_TIMEOUT: Duration = Duration::from_secs(10);
/// How long each of Claude Code's directory locks is waited for. The credentials pair waits up to twice this.
pub const CLAUDE_LOCK_TIMEOUT: Duration = Duration::from_secs(9);
/// The age past which a credential lock directory's holder counts as dead: Claude Code's own 60 s.
pub const CREDENTIALS_STALENESS: Duration = Duration::from_secs(60);
/// The age past which the config lock directory's holder counts as dead: `proper-lockfile`'s default 10 s.
pub const CONFIG_STALENESS: Duration = Duration::from_secs(10);
/// How often a held directory lock's mtime is touched; Claude Code touches every 5 s.
pub const TOUCH_INTERVAL: Duration = Duration::from_secs(3);

/// How often a held file lock is tried again.
#[cfg(all(windows, target_pointer_width = "64"))]
const FILE_LOCK_POLL: Duration = Duration::from_millis(100);
/// How long a dropped directory lock waits for its toucher to stop.
#[cfg(all(windows, target_pointer_width = "64"))]
const TOUCHER_JOIN: Duration = Duration::from_secs(1);

/// Why a lock was not taken.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LockError {
    /// Another holder kept the lock past the timeout.
    #[error("the lock stayed held by another process past the timeout; retry in a few seconds")]
    Timeout,
    /// The lock file or directory could not be made or opened.
    #[error("the lock could not be made or opened")]
    Io(#[source] std::io::Error),
    /// These locks are implemented for 64-bit Windows only.
    #[error("account locks are implemented for 64-bit Windows only")]
    Unsupported,
}

/// The store's file lock: byte 0 of a lock file, held exclusively.
pub struct FileLock;

/// A held [`FileLock`]; dropping it releases the byte and closes the file.
#[derive(Debug)]
pub struct FileLockGuard {
    #[cfg_attr(
        not(all(windows, target_pointer_width = "64")),
        expect(dead_code, reason = "no lock is ever taken off Windows")
    )]
    file: File,
}

impl FileLock {
    /// Opens or creates the lock file at `path` and takes byte 0, trying every 0.1 s until `timeout`. The file's folder
    /// must exist: a lock never makes one.
    ///
    /// # Errors
    ///
    /// [`LockError::Timeout`] when another handle holds the byte past `timeout`, [`LockError::Io`] when the file
    /// cannot be made or locked (its folder missing included), [`LockError::Unsupported`] off 64-bit Windows.
    pub fn acquire(path: &Path, timeout: Duration) -> Result<FileLockGuard, LockError> {
        refuse_link(path).map_err(LockError::Io)?;
        acquire_file(path, timeout)
    }
}

/// Takes the lock file at `path` in `root`'s account store, waiting up to `timeout`, once the root is verified: a
/// missing root (and its parent) is made owner-only, a root another principal owns or can write, or one that is a link,
/// is refused, the folders between the root and the lock are checked ([`AccountsRoot::check_folders`]), and the lock's
/// own folder, when it is not the root, is made owner-only. Every lock file in the root is taken here.
///
/// # Errors
///
/// [`LockError::Io`] when the root is refused or a folder cannot be checked or made; otherwise as
/// [`FileLock::acquire`].
#[cfg(all(windows, target_pointer_width = "64"))]
pub(crate) fn acquire_in_root(
    root: &AccountsRoot,
    path: &Path,
    timeout: Duration,
) -> Result<FileLockGuard, LockError> {
    // The lock is often the first writer a new store sees: it makes the root owner-only, never with the DACL of
    // whatever folder it was asked to live under.
    crate::store::ensure_root(root.root()).map_err(LockError::Io)?;
    check_store_root(root)?;
    root.check_folders(path).map_err(LockError::Io)?;
    if let Some(parent) = path.parent().filter(|parent| *parent != root.root()) {
        crate::guard::create_folder(root.root(), parent)
            .map_err(|error| LockError::Io(io::Error::other(error)))?;
    }
    FileLock::acquire(path, timeout)
}

/// Off 64-bit Windows no lock can be taken, so the root is left exactly as it was found: neither a missing store root,
/// nor a lock's folder, nor anything else is made for a lock that cannot be had.
///
/// # Errors
///
/// Always [`LockError::Unsupported`], the error [`FileLock::acquire`] gives there.
#[cfg(not(all(windows, target_pointer_width = "64")))]
pub(crate) fn acquire_in_root(
    _root: &AccountsRoot,
    _path: &Path,
    _timeout: Duration,
) -> Result<FileLockGuard, LockError> {
    Err(LockError::Unsupported)
}

/// Refuses a store root (and the folder above it) that is a link, owned by another user, or writable by another
/// principal, as the store's own opens do.
#[cfg(all(windows, target_pointer_width = "64"))]
fn check_store_root(root: &AccountsRoot) -> Result<(), LockError> {
    crate::store::check_root(root)
        .map_err(|error| LockError::Io(io::Error::new(io::ErrorKind::InvalidInput, error)))
}

/// Refuses a lock file that is a symbolic link, a junction or another reparse point, or, off Windows, a file more than
/// one name reaches: opening it would lock another file. A name that does not exist yet is fine.
fn refuse_link(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt as _;

                if metadata.file_attributes()
                    & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
                    != 0
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "the lock file is a link or reparse point, which is not followed",
                    ));
                }
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt as _;

                if metadata.file_type().is_symlink() || metadata.nlink() != 1 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "the lock file is a link, which is not followed",
                    ));
                }
            }
            let _ = &metadata;
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(all(windows, target_pointer_width = "64"))]
fn acquire_file(path: &Path, timeout: Duration) -> Result<FileLockGuard, LockError> {
    use std::fs::OpenOptions;
    use std::thread;
    use std::time::Instant;

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(LockError::Io)?;
    let started = Instant::now();
    loop {
        if crate::win32::try_lock_first_byte(&file).map_err(LockError::Io)? {
            return Ok(FileLockGuard { file });
        }
        if started.elapsed() > timeout {
            return Err(LockError::Timeout);
        }
        thread::sleep(FILE_LOCK_POLL);
    }
}

#[cfg(not(all(windows, target_pointer_width = "64")))]
fn acquire_file(_path: &Path, _timeout: Duration) -> Result<FileLockGuard, LockError> {
    Err(LockError::Unsupported)
}

#[cfg(all(windows, target_pointer_width = "64"))]
impl Drop for FileLockGuard {
    fn drop(&mut self) {
        // Closing the handle as the guard drops releases the byte even when the unlock fails, so its error says nothing
        // a caller could act on.
        let _unlocked = crate::win32::unlock_first_byte(&self.file);
    }
}

/// The account lock, `.lock` in the account store, held around every store write and switch.
pub struct AccountLock;

/// A held [`AccountLock`]: the store's writes take one, so none runs unlocked.
#[derive(Debug)]
pub struct AccountLockGuard {
    root: AccountsRoot,
    _lock: FileLockGuard,
}

impl AccountLock {
    /// Takes the account lock of `root`, waiting up to [`ACCOUNT_LOCK_TIMEOUT`].
    ///
    /// # Errors
    ///
    /// As [`FileLock::acquire`], and [`LockError::Io`] when the root is refused: the lock makes a missing root
    /// owner-only and refuses one another principal owns or can write, as every lock in the root does.
    pub fn acquire(root: &AccountsRoot) -> Result<AccountLockGuard, LockError> {
        Self::acquire_within(root, ACCOUNT_LOCK_TIMEOUT)
    }

    /// [`AccountLock::acquire`], waiting up to `wait`.
    pub(crate) fn acquire_within(
        root: &AccountsRoot,
        wait: Duration,
    ) -> Result<AccountLockGuard, LockError> {
        let lock = acquire_in_root(root, &root.lock_file(), wait)?;
        Ok(AccountLockGuard {
            root: root.clone(),
            _lock: lock,
        })
    }
}

impl AccountLockGuard {
    /// The account store whose lock this is.
    pub(crate) fn root(&self) -> &AccountsRoot {
        &self.root
    }
}

/// An account's consume lock, `credentials/.consume-<n>.lock`: one token refresh at a time per account, so a refresh
/// grant is never spent twice.
pub struct ConsumeLock;

/// A held [`ConsumeLock`].
#[derive(Debug)]
pub struct ConsumeLockGuard {
    number: u32,
    _lock: FileLockGuard,
}

impl ConsumeLock {
    /// Takes account `number`'s consume lock, waiting up to [`ACCOUNT_LOCK_TIMEOUT`].
    ///
    /// # Errors
    ///
    /// As [`AccountLock::acquire`].
    pub fn acquire(root: &AccountsRoot, number: u32) -> Result<ConsumeLockGuard, LockError> {
        Self::acquire_within(root, number, ACCOUNT_LOCK_TIMEOUT)
    }

    /// [`ConsumeLock::acquire`], waiting up to `wait`.
    pub(crate) fn acquire_within(
        root: &AccountsRoot,
        number: u32,
        wait: Duration,
    ) -> Result<ConsumeLockGuard, LockError> {
        let lock = acquire_in_root(root, &root.consume_lock(number), wait)?;
        Ok(ConsumeLockGuard {
            number,
            _lock: lock,
        })
    }
}

impl ConsumeLockGuard {
    /// The account whose consume lock this is.
    #[must_use]
    pub fn number(&self) -> u32 {
        self.number
    }
}

/// How long a directory lock is waited for, and how often it is touched while held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct LockTiming {
    /// How long a held lock is waited for.
    pub timeout: Duration,
    /// How often the held lock's mtime is touched.
    pub touch_interval: Duration,
}

impl LockTiming {
    /// The timing for Claude Code's locks: [`CLAUDE_LOCK_TIMEOUT`] and [`TOUCH_INTERVAL`].
    pub const CLAUDE: Self = Self::new(CLAUDE_LOCK_TIMEOUT, TOUCH_INTERVAL);

    /// A timing that waits up to `timeout` and touches every `touch_interval`.
    #[must_use]
    pub const fn new(timeout: Duration, touch_interval: Duration) -> Self {
        Self {
            timeout,
            touch_interval,
        }
    }
}

/// A directory lock in Claude Code's `proper-lockfile` style.
pub struct DirLock;

/// A held [`DirLock`]. A thread touches the directory's mtime while it lives, and only while it is still the directory
/// the guard made; a touch that fails is tried again at the next one, until the lock would be stale. Dropping the guard
/// stops the thread, waiting up to a second for it, and removes the directory when it is still the one the guard made
/// (by identity, even if it went stale or a touch failed). Each touch and the drop open the directory, check its
/// identity and act through that one handle, then close it, so a directory another holder made in its place is never
/// touched or removed; the drop leaves it alone with a warning. The guard is `Send` but not `Sync`: share it by moving
/// it, never by reference across threads.
#[derive(Debug)]
pub struct DirLockGuard {
    path: PathBuf,
    /// The identity (volume serial number and file ID) of the directory this guard made: a directory there with another
    /// one is another taker's.
    identity: DirIdentity,
    /// The age past which the lock's directory counts as a dead holder's, as it was taken with.
    staleness: Duration,
    /// Set by the toucher when it stopped keeping the lock fresh: the directory went or was replaced, or touches failed
    /// until the lock would be stale.
    lost: Arc<AtomicBool>,
    #[cfg(all(windows, target_pointer_width = "64"))]
    toucher: Option<Toucher>,
}

/// The thread that keeps a held directory lock fresh: dropping `stop` ends it, and `done` disconnects when it has.
#[cfg(all(windows, target_pointer_width = "64"))]
#[derive(Debug)]
struct Toucher {
    stop: mpsc::Sender<()>,
    done: mpsc::Receiver<()>,
    thread: JoinHandle<()>,
}

impl DirLock {
    /// Takes the lock directory at `path`, making its parent folder first. A directory already there whose mtime is
    /// older than `staleness` is removed and taken over; a live one is retried every 0.25 to 0.5 s until
    /// `timing.timeout`.
    ///
    /// # Errors
    ///
    /// [`LockError::Timeout`] when a live holder keeps the lock past the timeout, [`LockError::Io`] when the directory
    /// or the toucher thread cannot be made, [`LockError::Unsupported`] off 64-bit Windows.
    pub fn acquire(
        path: &Path,
        staleness: Duration,
        timing: LockTiming,
    ) -> Result<DirLockGuard, LockError> {
        acquire_dir(path, staleness, timing)
    }
}

impl DirLockGuard {
    /// The lock directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether the lock is still this guard's to write under: the directory exists, it is the one this guard made (its
    /// volume serial number and file ID are the ones recorded when it was taken), its mtime is younger than the
    /// staleness it was taken with, and the toucher has not given up keeping it fresh. A lock that went stale while held
    /// (the machine slept, or touching stopped) may be taken over by another process at any moment; a write under a
    /// lock that is not ours must not happen.
    #[must_use]
    pub fn still_owned(&self) -> bool {
        !self.lost.load(Ordering::Acquire)
            && look(&self.path).is_ok_and(|(identity, metadata)| {
                identity == self.identity && !is_stale(&metadata, self.staleness)
            })
    }
}

/// A directory's volume serial number and 128-bit file ID.
type DirIdentity = (u64, [u8; 16]);

#[cfg(all(windows, target_pointer_width = "64"))]
fn dir_identity(path: &Path) -> std::io::Result<DirIdentity> {
    use crate::win32::{DirAccess, DirHandle};

    DirHandle::open(path, DirAccess::Read)?.identity()
}

/// The identity and metadata of the lock directory at `path`, read through one handle.
#[cfg(all(windows, target_pointer_width = "64"))]
fn look(path: &Path) -> std::io::Result<(DirIdentity, std::fs::Metadata)> {
    use crate::win32::{DirAccess, DirHandle};

    let dir = DirHandle::open(path, DirAccess::Read)?;
    Ok((dir.identity()?, dir.metadata()?))
}

#[cfg(not(all(windows, target_pointer_width = "64")))]
fn look(_path: &Path) -> std::io::Result<(DirIdentity, std::fs::Metadata)> {
    Err(std::io::ErrorKind::Unsupported.into())
}

/// Touches the lock directory at `path` if it is still the one with `identity`, checking and touching through one
/// handle. `Ok(false)` when another directory is there.
#[cfg(all(windows, target_pointer_width = "64"))]
fn touch_if_ours(path: &Path, identity: DirIdentity) -> std::io::Result<bool> {
    use crate::win32::{DirAccess, DirHandle};

    let dir = DirHandle::open(path, DirAccess::Touch)?;
    if dir.identity()? != identity {
        return Ok(false);
    }
    dir.touch()?;
    Ok(true)
}

/// Removes the lock directory at `path` if it is still the one with `identity`, checking and removing through one
/// handle. `Ok(false)` when another directory is there.
#[cfg(all(windows, target_pointer_width = "64"))]
fn remove_if_ours(path: &Path, identity: DirIdentity) -> std::io::Result<bool> {
    use crate::win32::{DirAccess, DirHandle};

    let dir = DirHandle::open(path, DirAccess::Remove)?;
    if dir.identity()? != identity {
        return Ok(false);
    }
    dir.remove()?;
    Ok(true)
}

/// Whether a create or a look at a lock directory failed because another holder has it: it exists, or it is being
/// deleted and refuses access until its last handle closes.
#[cfg(all(windows, target_pointer_width = "64"))]
fn held_elsewhere(error: &std::io::Error) -> bool {
    use std::io::ErrorKind;

    [ErrorKind::AlreadyExists, ErrorKind::PermissionDenied].contains(&error.kind())
}

#[cfg(all(windows, target_pointer_width = "64"))]
fn acquire_dir(
    path: &Path,
    staleness: Duration,
    timing: LockTiming,
) -> Result<DirLockGuard, LockError> {
    use std::fs;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(LockError::Io)?;
    }
    create_lock_dir(path, staleness, timing.timeout)?;
    let identity = match dir_identity(path) {
        Ok(identity) => identity,
        Err(error) => {
            // A lock whose ownership could never be checked is given back rather than held.
            let _released = fs::remove_dir(path);
            return Err(LockError::Io(error));
        }
    };
    let lost = Arc::new(AtomicBool::new(false));
    let touching = Touching {
        path: path.to_path_buf(),
        identity,
        staleness,
        interval: timing.touch_interval,
        lost: Arc::clone(&lost),
    };
    match start_toucher(touching) {
        Ok(toucher) => Ok(DirLockGuard {
            path: path.to_path_buf(),
            identity,
            staleness,
            lost,
            toucher: Some(toucher),
        }),
        Err(error) => {
            // The lock was taken but cannot be kept alive: give it back rather than hold it unrefreshed.
            let _released = fs::remove_dir(path);
            Err(LockError::Io(error))
        }
    }
}

/// Creates the lock directory at `path`, waiting on another holder's until it is released or goes stale, for at most
/// `timeout`.
#[cfg(all(windows, target_pointer_width = "64"))]
fn create_lock_dir(path: &Path, staleness: Duration, timeout: Duration) -> Result<(), LockError> {
    use std::fs;
    use std::time::Instant;

    let started = Instant::now();
    loop {
        match fs::create_dir(path) {
            Ok(()) => return Ok(()),
            Err(error) if held_elsewhere(&error) => {}
            Err(error) => return Err(LockError::Io(error)),
        }
        if started.elapsed() > timeout {
            return Err(LockError::Timeout);
        }
        wait_on_holder(path, staleness);
    }
}

/// One waiting turn on the lock directory another holder has at `path`: looks at it, takes it over when stale, and
/// otherwise sleeps before the next create.
#[cfg(all(windows, target_pointer_width = "64"))]
fn wait_on_holder(path: &Path, staleness: Duration) {
    use std::io::ErrorKind;
    use std::thread;

    let (seen, metadata) = match look(path) {
        Ok(seen) => seen,
        // The holder released it between the create and this look: try again at once.
        Err(error) if error.kind() == ErrorKind::NotFound => return,
        // A directory being deleted cannot be looked at, and a sharing violation or antivirus passes: wait and
        // look again, until the timeout.
        Err(_) => {
            thread::sleep(retry_jitter());
            return;
        }
    };
    if is_stale(&metadata, staleness) {
        // A dead holder's lock. Losing the race to take it over to another waiter only means another turn.
        if !remove_if_still_stale(path, seen, staleness) {
            thread::sleep(Duration::from_millis(50));
        }
        return;
    }
    thread::sleep(retry_jitter());
}

#[cfg(not(all(windows, target_pointer_width = "64")))]
fn acquire_dir(
    _path: &Path,
    _staleness: Duration,
    _timing: LockTiming,
) -> Result<DirLockGuard, LockError> {
    Err(LockError::Unsupported)
}

/// Whether a lock directory's mtime is older than `staleness`.
fn is_stale(metadata: &std::fs::Metadata, staleness: Duration) -> bool {
    use std::time::SystemTime;

    metadata.modified().is_ok_and(|modified| {
        SystemTime::now()
            .duration_since(modified)
            .is_ok_and(|age| age > staleness)
    })
}

/// Removes the stale lock directory at `path`, but only if it is still the one seen with identity `seen` (its volume
/// serial number and file ID, which a directory made again in its place never shares, though its creation time may come
/// back) and still stale. Another waiter may have taken it over and made a fresh one since it was seen; that one is
/// never removed. The check and the removal go through one handle. Returns whether the directory was removed.
#[cfg(all(windows, target_pointer_width = "64"))]
fn remove_if_still_stale(path: &Path, seen: DirIdentity, staleness: Duration) -> bool {
    use crate::win32::{DirAccess, DirHandle};

    let Ok(dir) = DirHandle::open(path, DirAccess::Remove) else {
        return false;
    };
    let still_seen = dir.identity().is_ok_and(|identity| identity == seen);
    if !still_seen
        || !dir
            .metadata()
            .is_ok_and(|current| is_stale(&current, staleness))
    {
        return false;
    }
    dir.remove().is_ok()
}

/// A fixed description of the lock directory at `path` for the log, so no path under the user's profile is ever
/// logged.
#[cfg(all(windows, target_pointer_width = "64"))]
fn lock_description(path: &Path) -> &'static str {
    match path.file_name().and_then(|name| name.to_str()) {
        Some(".oauth_refresh.lock") => "Claude Code's OAuth refresh lock",
        Some(".claude.lock") => "Claude Code's credentials lock",
        Some(".claude.json.lock") => "Claude Code's config lock",
        _ => "the lock directory",
    }
}

/// A wait between 0.25 s and 0.5 s, so waiters that collide spread out.
#[cfg(all(windows, target_pointer_width = "64"))]
fn retry_jitter() -> Duration {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};

    let random = RandomState::new().build_hasher().finish();
    Duration::from_millis(250 + random % 251)
}

/// What a toucher keeps fresh: the lock directory with `identity` at `path`, touched every `interval`.
#[cfg(all(windows, target_pointer_width = "64"))]
struct Touching {
    path: PathBuf,
    identity: DirIdentity,
    /// The age past which the lock counts as a dead holder's: touches that fail for this long lose it.
    staleness: Duration,
    interval: Duration,
    lost: Arc<AtomicBool>,
}

#[cfg(all(windows, target_pointer_width = "64"))]
impl Touching {
    /// Touches the lock directory every `interval` until `stop` disconnects or the lock is lost: the directory went,
    /// another one took its place, or touches kept failing until the lock would be stale since the last one that took.
    /// Any other failed touch (antivirus, a sharing violation) is tried again at the next interval.
    fn run(&self, stop: &mpsc::Receiver<()>) {
        use std::io::ErrorKind;
        use std::sync::mpsc::RecvTimeoutError;
        use std::time::SystemTime;

        let mut last_touched = SystemTime::now();
        // The failed touches in a row: the first is warned of, the rest logged at debug.
        let mut failures: u32 = 0;
        while let Err(RecvTimeoutError::Timeout) = stop.recv_timeout(self.interval) {
            let attempted = SystemTime::now();
            let error = match touch_if_ours(&self.path, self.identity) {
                Ok(true) => {
                    last_touched = attempted;
                    failures = 0;
                    continue;
                }
                Ok(false) => {
                    self.lose();
                    tracing::warn!(
                        lock = lock_description(&self.path),
                        "a held lock directory was replaced by another holder's; it is no longer touched and counts as lost"
                    );
                    break;
                }
                Err(error) => error,
            };
            if error.kind() == ErrorKind::NotFound {
                self.lose();
                tracing::warn!(
                    lock = lock_description(&self.path),
                    "a held lock directory is gone; it counts as lost"
                );
                break;
            }
            if SystemTime::now()
                .duration_since(last_touched)
                .is_ok_and(|age| age > self.staleness)
            {
                self.lose();
                tracing::warn!(
                    lock = lock_description(&self.path),
                    error = %error.kind(),
                    "a held lock directory could not be touched until it went stale; it is no longer kept fresh and counts as lost"
                );
                break;
            }
            failures = failures.saturating_add(1);
            if failures == 1 {
                tracing::warn!(
                    lock = lock_description(&self.path),
                    error = %error.kind(),
                    "a held lock directory could not be touched; trying again at the next touch"
                );
            } else {
                tracing::debug!(
                    lock = lock_description(&self.path),
                    error = %error.kind(),
                    failures,
                    "a held lock directory could still not be touched; trying again at the next touch"
                );
            }
        }
    }

    fn lose(&self) {
        self.lost.store(true, Ordering::Release);
    }
}

#[cfg(all(windows, target_pointer_width = "64"))]
fn start_toucher(touching: Touching) -> std::io::Result<Toucher> {
    use std::thread;

    let (stop, stop_signal) = mpsc::channel::<()>();
    let (done_signal, done) = mpsc::channel::<()>();
    let thread = thread::Builder::new()
        .name("claude-lock-toucher".to_owned())
        .spawn(move || {
            let _done = done_signal;
            touching.run(&stop_signal);
        })?;
    Ok(Toucher { stop, done, thread })
}

#[cfg(all(windows, target_pointer_width = "64"))]
impl Drop for DirLockGuard {
    fn drop(&mut self) {
        use std::sync::mpsc::RecvTimeoutError;

        if let Some(toucher) = self.toucher.take() {
            drop(toucher.stop);
            match toucher.done.recv_timeout(TOUCHER_JOIN) {
                Err(RecvTimeoutError::Disconnected) | Ok(()) => {
                    // The toucher only sleeps and touches, so it has nothing to report on joining.
                    let _joined = toucher.thread.join();
                }
                // A toucher stuck in a slow touch is left to finish on its own; it touches nothing after this.
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
        // The drop decides on identity alone: a directory of ours that went stale, or whose toucher failed, is still
        // ours to remove. A removal that fails leaves a directory that turns stale and is taken over, and a drop has
        // no caller to tell but the log.
        match remove_if_ours(&self.path, self.identity) {
            // Removed, or the directory vanished while held and there is nothing to remove.
            Ok(true) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Ok(false) => tracing::warn!(
                lock = lock_description(&self.path),
                "a lock directory was taken over by another holder while held; it was left in place"
            ),
            Err(error) => tracing::warn!(
                lock = lock_description(&self.path),
                error = %error.kind(),
                "a lock directory could not be checked or removed when its guard dropped; it was left in place"
            ),
        }
    }
}

/// Claude Code's locks around its credentials and its global config.
pub struct ClaudeCodeLocks;

/// Both of Claude Code's credential locks, held. Dropping it releases the legacy lock, then the primary one. Like
/// [`DirLockGuard`], it is `Send` but not `Sync`.
#[derive(Debug)]
pub struct ClaudeCredentialsGuard {
    legacy: DirLockGuard,
    primary: DirLockGuard,
}

impl ClaudeCredentialsGuard {
    /// Whether these are the credential locks of `paths`.
    pub(crate) fn guards(&self, paths: &ClaudePaths) -> bool {
        self.primary.path() == paths.oauth_refresh_lock()
            && self.legacy.path() == paths.credentials_lock()
    }

    /// Whether both credential locks are still this guard's, as [`DirLockGuard::still_owned`] says for each.
    #[must_use]
    pub fn still_owned(&self) -> bool {
        self.primary.still_owned() && self.legacy.still_owned()
    }
}

impl ClaudeCodeLocks {
    /// Takes Claude Code's credential locks in its own order, `.oauth_refresh.lock` then `~/.claude.lock`, each with
    /// [`CREDENTIALS_STALENESS`] and [`LockTiming::CLAUDE`].
    ///
    /// # Errors
    ///
    /// As [`DirLock::acquire`]; the first lock is released when the second is not had.
    pub fn credentials(paths: &ClaudePaths) -> Result<ClaudeCredentialsGuard, LockError> {
        Self::credentials_with(paths, LockTiming::CLAUDE)
    }

    /// [`ClaudeCodeLocks::credentials`] with its own timing.
    ///
    /// # Errors
    ///
    /// As [`DirLock::acquire`]; the first lock is released when the second is not had.
    pub fn credentials_with(
        paths: &ClaudePaths,
        timing: LockTiming,
    ) -> Result<ClaudeCredentialsGuard, LockError> {
        let primary = DirLock::acquire(&paths.oauth_refresh_lock(), CREDENTIALS_STALENESS, timing)?;
        let legacy = DirLock::acquire(&paths.credentials_lock(), CREDENTIALS_STALENESS, timing)?;
        Ok(ClaudeCredentialsGuard { legacy, primary })
    }

    /// Takes Claude Code's global config lock, `~/.claude.json.lock`, with [`CONFIG_STALENESS`] and
    /// [`LockTiming::CLAUDE`].
    ///
    /// # Errors
    ///
    /// As [`DirLock::acquire`].
    pub fn config(paths: &ClaudePaths) -> Result<DirLockGuard, LockError> {
        Self::config_with(paths, LockTiming::CLAUDE)
    }

    /// [`ClaudeCodeLocks::config`] with its own timing.
    ///
    /// # Errors
    ///
    /// As [`DirLock::acquire`].
    pub fn config_with(paths: &ClaudePaths, timing: LockTiming) -> Result<DirLockGuard, LockError> {
        DirLock::acquire(&paths.config_lock(), CONFIG_STALENESS, timing)
    }
}

#[cfg(all(test, windows, target_pointer_width = "64"))]
#[expect(
    clippy::expect_used,
    reason = "a test that cannot set up its lock directory must fail loudly"
)]
mod tests {
    use std::fs::{self, FileTimes, OpenOptions};
    use std::os::windows::fs::{FileTimesExt, OpenOptionsExt};
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime};

    use super::{CREDENTIALS_STALENESS, dir_identity, remove_if_still_stale};

    /// `FILE_FLAG_BACKUP_SEMANTICS` and `FILE_WRITE_ATTRIBUTES`: enough to open a directory and set its times.
    const BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const WRITE_ATTRIBUTES: u32 = 0x0100;

    /// A fresh scratch folder under the system temp folder: unit tests have no `CARGO_TARGET_TMPDIR`.
    fn scratch(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("claude-accounts-unit").join(test);
        if dir.exists() {
            fs::remove_dir_all(&dir).expect("the test's old folder can be removed");
        }
        fs::create_dir_all(&dir).expect("the test's folder can be made");
        dir
    }

    fn age(dir: &Path, ago: Duration) {
        let handle = OpenOptions::new()
            .access_mode(WRITE_ATTRIBUTES)
            .custom_flags(BACKUP_SEMANTICS)
            .open(dir)
            .expect("the lock directory opens for its times");
        handle
            .set_modified(SystemTime::now() - ago)
            .expect("the lock directory's mtime can be set");
    }

    #[test]
    fn stale_takeover_spares_a_lock_retaken_meanwhile() {
        let lock = scratch("stale_takeover_spares_a_lock_retaken_meanwhile").join(".claude.lock");
        fs::create_dir(&lock).expect("a dead holder's lock is made");
        age(&lock, Duration::from_secs(61));
        let seen = dir_identity(&lock).expect("the stale lock is seen");

        // Another waiter takes the stale lock over between this waiter's look and its remove.
        fs::remove_dir(&lock).expect("the other waiter removes the stale lock");
        fs::create_dir(&lock).expect("the other waiter makes its own");
        assert!(
            !remove_if_still_stale(&lock, seen, CREDENTIALS_STALENESS),
            "a fresh lock is never removed"
        );
        assert!(lock.is_dir(), "the other waiter keeps its lock");

        age(&lock, Duration::from_secs(61));
        let seen = dir_identity(&lock).expect("the lock is seen stale again");
        assert!(
            remove_if_still_stale(&lock, seen, CREDENTIALS_STALENESS),
            "a lock still stale as seen is removed"
        );
        assert!(!lock.exists());
    }

    #[test]
    fn stale_removal_compares_file_identity_not_creation_time() {
        let lock =
            scratch("stale_removal_compares_file_identity_not_creation_time").join(".claude.lock");
        fs::create_dir(&lock).expect("a dead holder's lock is made");
        age(&lock, Duration::from_secs(61));
        let seen = dir_identity(&lock).expect("the stale lock is seen");
        let created = fs::metadata(&lock)
            .and_then(|metadata| metadata.created())
            .expect("the stale lock's creation time reads");

        // Another taker replaces it, and the new directory carries the old one's creation time (as NTFS tunnelling
        // would) and has gone stale too.
        fs::remove_dir(&lock).expect("the other taker removes the stale lock");
        fs::create_dir(&lock).expect("the other taker makes its own");
        OpenOptions::new()
            .access_mode(WRITE_ATTRIBUTES)
            .custom_flags(BACKUP_SEMANTICS)
            .open(&lock)
            .expect("the replacement opens for its times")
            .set_times(
                FileTimes::new()
                    .set_created(created)
                    .set_modified(SystemTime::now() - Duration::from_secs(61)),
            )
            .expect("the replacement's times can be set");

        assert!(
            !remove_if_still_stale(&lock, seen, CREDENTIALS_STALENESS),
            "a replacement with the seen creation time is still not the directory seen"
        );
        assert!(lock.is_dir(), "the replacement is left in place");
    }

    /// A refused store root keeps the check's own error as the lock error's source.
    #[test]
    fn a_refused_lock_root_keeps_the_check_error_as_its_source() {
        let folder = scratch("a_refused_lock_root_keeps_the_check_error_as_its_source");
        let root = crate::paths::AccountsRoot::new(folder.join("store"));
        fs::write(root.root(), b"not a folder")
            .expect("the test's file stands where the store root would be");
        let refusal =
            crate::store::check_root(&root).expect_err("a file at the store root is refused");
        let text = refusal.to_string();

        let error =
            super::check_store_root(&root).expect_err("the lock layer refuses the same root");

        let io_error = std::error::Error::source(&error)
            .and_then(|source| source.downcast_ref::<std::io::Error>())
            .expect("the lock error carries the check's I/O error");
        assert_eq!(
            io_error.kind(),
            std::io::ErrorKind::InvalidInput,
            "the I/O kind is unchanged"
        );
        assert_eq!(
            io_error.to_string(),
            text,
            "the refusal's text is unchanged"
        );
        let kept = io_error
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<crate::store::StoreError>())
            .expect("the check's own error is kept as the source");
        assert_eq!(
            kept.kind(),
            crate::store::StoreErrorKind::RootNotDirectory,
            "the source is the check's own error"
        );
    }
}
