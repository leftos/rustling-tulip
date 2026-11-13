//! The account store's locks and Claude Code's, each taken the way its owner takes it, so any process taking the same lock the same way is excluded.
//!
//! The store's locks ([`FileLock`]: the account lock `.lock` and each account's consume lock) hold byte 0 of a lock file,
//! the byte `msvcrt.locking` takes. Claude Code's locks ([`DirLock`]: its two credential locks and its config
//! lock) are directories in the `proper-lockfile` style: creating the directory is the mutex, a holder touches its mtime
//! while it lives, and a directory older than the lock's staleness belongs to a dead holder and may be taken over.
//! The locks work on 64-bit Windows and on macOS; on macOS a file lock is `flock(2)` on the whole lock file, which only
//! this crate takes. Everywhere else every lock returns [`LockError::Unsupported`].

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(accounts_locks)]
use std::sync::mpsc;
#[cfg(accounts_locks)]
use std::thread::JoinHandle;
use std::time::Duration;

#[cfg(all(unix, accounts_locks))]
use self::unix::{DirAccess, DirHandle, Stage, open_lock_file, try_lock_file, unlock_file};
#[cfg(unix)]
pub(crate) use self::unix::{aside_name, rename_exclusive};
use crate::paths::{AccountsRoot, ClaudePaths};
#[cfg(all(windows, target_pointer_width = "64"))]
use crate::win32::{DirAccess, DirHandle};

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
#[cfg(accounts_locks)]
const FILE_LOCK_POLL: Duration = Duration::from_millis(100);
/// How long a dropped directory lock waits for its toucher to stop.
#[cfg(accounts_locks)]
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
    /// These locks are implemented for 64-bit Windows and macOS only.
    #[error("account locks are implemented for 64-bit Windows and macOS only")]
    Unsupported,
}

/// The store's file lock: byte 0 of a lock file on Windows, the whole file (`flock`) on macOS, held exclusively.
pub struct FileLock;

/// A held [`FileLock`]; dropping it releases the lock and closes the file.
#[derive(Debug)]
pub struct FileLockGuard {
    #[cfg_attr(
        not(accounts_locks),
        expect(
            dead_code,
            reason = "no lock is ever taken off 64-bit Windows and macOS"
        )
    )]
    file: File,
}

impl FileLock {
    /// Opens or creates the lock file at `path` and takes its lock, trying every 0.1 s until `timeout`. The file's
    /// folder must exist: a lock never makes one. On macOS the file is made owner-only (mode 0600), and one that is a
    /// link, not a regular file, reached by more than one name, another user's or open to another user is refused
    /// before any wait.
    ///
    /// # Errors
    ///
    /// [`LockError::Timeout`] when another handle holds the lock past `timeout`, [`LockError::Io`] when the file
    /// cannot be made or locked (its folder missing included) or is refused, [`LockError::Unsupported`] off 64-bit
    /// Windows and macOS.
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
#[cfg(accounts_locks)]
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

/// Off 64-bit Windows and macOS no lock can be taken, so the root is left exactly as it was found: neither a missing
/// store root, nor a lock's folder, nor anything else is made for a lock that cannot be had.
///
/// # Errors
///
/// Always [`LockError::Unsupported`], the error [`FileLock::acquire`] gives there.
#[cfg(not(accounts_locks))]
pub(crate) fn acquire_in_root(
    _root: &AccountsRoot,
    _path: &Path,
    _timeout: Duration,
) -> Result<FileLockGuard, LockError> {
    Err(LockError::Unsupported)
}

/// Refuses a store root (and the folder above it) that is a link, owned by another user, or writable by another
/// principal, as the store's own opens do.
#[cfg(accounts_locks)]
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

#[cfg(accounts_locks)]
fn acquire_file(path: &Path, timeout: Duration) -> Result<FileLockGuard, LockError> {
    use std::thread;
    use std::time::Instant;

    let file = open_lock_file(path).map_err(LockError::Io)?;
    let started = Instant::now();
    loop {
        if try_lock_file(&file).map_err(LockError::Io)? {
            return Ok(FileLockGuard { file });
        }
        if started.elapsed() > timeout {
            return Err(LockError::Timeout);
        }
        thread::sleep(FILE_LOCK_POLL);
    }
}

#[cfg(not(accounts_locks))]
fn acquire_file(_path: &Path, _timeout: Duration) -> Result<FileLockGuard, LockError> {
    Err(LockError::Unsupported)
}

/// Opens or creates the lock file at `path`, never truncating it.
#[cfg(all(windows, target_pointer_width = "64"))]
fn open_lock_file(path: &Path) -> io::Result<File> {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

/// Takes byte 0 of `file` without waiting; `Ok(false)` when another handle holds it.
#[cfg(all(windows, target_pointer_width = "64"))]
fn try_lock_file(file: &File) -> io::Result<bool> {
    crate::win32::try_lock_first_byte(file)
}

/// Releases the lock [`try_lock_file`] took.
#[cfg(all(windows, target_pointer_width = "64"))]
fn unlock_file(file: &File) -> io::Result<()> {
    crate::win32::unlock_first_byte(file)
}

#[cfg(accounts_locks)]
impl Drop for FileLockGuard {
    fn drop(&mut self) {
        // Closing the handle as the guard drops releases the lock even when the unlock fails, so its error says nothing
        // a caller could act on.
        let _unlocked = unlock_file(&self.file);
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
/// identity and act through that one handle, then close it (on macOS the drop's removal sets the directory aside by
/// rename and checks it again there, as [`DirHandle::remove`] says), so a directory another holder made in its place
/// is never touched or removed; the drop leaves it alone with a warning. The guard is `Send` but not `Sync`: share it by moving
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
    #[cfg(accounts_locks)]
    toucher: Option<Toucher>,
}

/// The thread that keeps a held directory lock fresh: dropping `stop` ends it, and `done` disconnects when it has.
#[cfg(accounts_locks)]
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
    /// or the toucher thread cannot be made or, on macOS, when the lock directory is a link or cannot be looked at,
    /// [`LockError::Unsupported`] off 64-bit Windows and macOS.
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

/// A directory's volume serial number and 128-bit file ID; on macOS its device number and its inode number in the first
/// 8 bytes.
type DirIdentity = (u64, [u8; 16]);

#[cfg(accounts_locks)]
fn dir_identity(path: &Path) -> std::io::Result<DirIdentity> {
    DirHandle::open(path, DirAccess::Read)?.identity()
}

/// The identity and metadata of the lock directory at `path`, read through one handle.
#[cfg(accounts_locks)]
fn look(path: &Path) -> std::io::Result<(DirIdentity, std::fs::Metadata)> {
    let dir = DirHandle::open(path, DirAccess::Read)?;
    Ok((dir.identity()?, dir.metadata()?))
}

#[cfg(not(accounts_locks))]
fn look(_path: &Path) -> std::io::Result<(DirIdentity, std::fs::Metadata)> {
    Err(std::io::ErrorKind::Unsupported.into())
}

/// Touches the lock directory at `path` if it is still the one with `identity`, checking and touching through one
/// handle. `Ok(false)` when another directory is there.
#[cfg(accounts_locks)]
fn touch_if_ours(path: &Path, identity: DirIdentity) -> std::io::Result<bool> {
    let dir = DirHandle::open(path, DirAccess::Touch)?;
    if dir.identity()? != identity {
        return Ok(false);
    }
    dir.touch()?;
    Ok(true)
}

/// Removes the lock directory at `path` if it is still the one with `identity`, checking and removing through one
/// handle. `Ok(false)` when another directory is there.
#[cfg(accounts_locks)]
fn remove_if_ours(path: &Path, identity: DirIdentity) -> std::io::Result<bool> {
    #[cfg(windows)]
    {
        let dir = DirHandle::open(path, DirAccess::Remove)?;
        if dir.identity()? != identity {
            return Ok(false);
        }
        dir.remove()?;
        Ok(true)
    }
    #[cfg(unix)]
    {
        remove_if_ours_with(path, identity, &mut |_| ())
    }
}

/// As [`remove_if_ours`], calling `hook` at each [`Stage`] of the removal, so a test can act as a competing taker
/// between the check and the removal.
#[cfg(all(unix, accounts_locks))]
fn remove_if_ours_with(
    path: &Path,
    identity: DirIdentity,
    hook: &mut dyn FnMut(Stage),
) -> std::io::Result<bool> {
    let dir = DirHandle::open(path, DirAccess::Remove)?;
    if dir.identity()? != identity {
        return Ok(false);
    }
    dir.remove(None, hook)
}

/// Whether a create or a look at a lock directory failed because another holder has it: it exists, or, on Windows, it
/// is being deleted and refuses access until its last handle closes. A directory removed on macOS frees its name at
/// once, so there only an existing one is another holder's.
#[cfg(accounts_locks)]
fn held_elsewhere(error: &std::io::Error) -> bool {
    use std::io::ErrorKind;

    #[cfg(windows)]
    let kinds: &[ErrorKind] = &[ErrorKind::AlreadyExists, ErrorKind::PermissionDenied];
    #[cfg(unix)]
    let kinds: &[ErrorKind] = &[ErrorKind::AlreadyExists];
    kinds.contains(&error.kind())
}

/// Makes the lock directory at `path`; on macOS owner-only (mode 0700), its parent left as it is.
#[cfg(accounts_locks)]
fn make_lock_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        fs::create_dir(path)
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;

        fs::DirBuilder::new().mode(0o700).create(path)
    }
}

#[cfg(accounts_locks)]
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
            // A lock whose ownership could never be checked is not held. With no identity to check it by, a removal
            // by name could remove another holder's directory, so it is left for its staleness to clear.
            tracing::warn!(
                lock = lock_description(path),
                error = %error.kind(),
                "a lock directory just made could not be looked at; it was left for its staleness to clear"
            );
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
            // The lock was taken but cannot be kept alive: give it back rather than hold it unrefreshed, removing it
            // only while it is still the directory made. One that cannot be removed turns stale and is taken over.
            let _released = remove_if_ours(path, identity);
            Err(LockError::Io(error))
        }
    }
}

/// Creates the lock directory at `path`, waiting on another holder's until it is released or goes stale, for at most
/// `timeout`.
#[cfg(accounts_locks)]
fn create_lock_dir(path: &Path, staleness: Duration, timeout: Duration) -> Result<(), LockError> {
    use std::time::Instant;

    let started = Instant::now();
    loop {
        match make_lock_dir(path) {
            Ok(()) => return Ok(()),
            Err(error) if held_elsewhere(&error) => {}
            Err(error) => return Err(LockError::Io(error)),
        }
        if started.elapsed() > timeout {
            return Err(LockError::Timeout);
        }
        wait_on_holder(path, staleness)?;
    }
}

/// One waiting turn on the lock directory another holder has at `path`: looks at it, takes it over when stale, and
/// otherwise sleeps before the next create.
///
/// # Errors
///
/// On macOS, [`LockError::Io`] when the lock directory is refused: a link there (the no-follow open fails with
/// `ELOOP`), a file or anything else that is not a directory (`ENOTDIR`), or one this user may not open (`EACCES`,
/// `EPERM`) is refused before any wait. Any other failed look is waited out, as on Windows.
#[cfg(accounts_locks)]
fn wait_on_holder(path: &Path, staleness: Duration) -> Result<(), LockError> {
    wait_on_holder_with(path, staleness, look)
}

/// As [`wait_on_holder`], looking at the lock directory with `look`, so a test can make the look fail as it chooses.
#[cfg(accounts_locks)]
#[cfg_attr(
    windows,
    expect(
        clippy::unnecessary_wraps,
        reason = "only the macOS arm refuses; Windows waits out every failed look"
    )
)]
fn wait_on_holder_with(
    path: &Path,
    staleness: Duration,
    look: impl FnOnce(&Path) -> std::io::Result<(DirIdentity, std::fs::Metadata)>,
) -> Result<(), LockError> {
    use std::io::ErrorKind;
    use std::thread;

    let (seen, metadata) = match look(path) {
        Ok(seen) => seen,
        // The holder released it between the create and this look: try again at once.
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        // On macOS a link, a non-directory or a lock this user may not open is refused at once.
        #[cfg(unix)]
        Err(error) if refuses_at_once(&error) => return Err(LockError::Io(error)),
        // Anything else is waited out until the timeout: on Windows a directory being deleted, a sharing violation or
        // antivirus; on macOS an interrupted call or no descriptor free (`EINTR`, `EMFILE`, `ENFILE`).
        Err(_) => {
            thread::sleep(retry_jitter());
            return Ok(());
        }
    };
    if is_stale(&metadata, staleness) {
        // A dead holder's lock. Losing the race to take it over to another waiter only means another turn.
        if !remove_if_still_stale(path, seen, staleness) {
            thread::sleep(Duration::from_millis(50));
        }
        return Ok(());
    }
    thread::sleep(retry_jitter());
    Ok(())
}

/// Whether a failed look at a lock directory on macOS refuses the lock rather than waits: a link (`ELOOP`), not a
/// directory (`ENOTDIR`), not this user's to open (`EACCES`, `EPERM`), or a refusal of this crate's own that carries no
/// OS error (a NUL in the path), which no wait changes.
#[cfg(all(unix, accounts_locks))]
fn refuses_at_once(error: &std::io::Error) -> bool {
    error.raw_os_error().is_none_or(|errno| {
        [libc::ELOOP, libc::ENOTDIR, libc::EACCES, libc::EPERM].contains(&errno)
    })
}

#[cfg(not(accounts_locks))]
fn acquire_dir(
    _path: &Path,
    _staleness: Duration,
    _timing: LockTiming,
) -> Result<DirLockGuard, LockError> {
    Err(LockError::Unsupported)
}

/// Whether a lock directory's mtime is older than `staleness`.
fn is_stale(metadata: &std::fs::Metadata, staleness: Duration) -> bool {
    metadata
        .modified()
        .is_ok_and(|modified| older_than(modified, staleness))
}

/// Whether a time `modified` is more than `staleness` ago.
fn older_than(modified: std::time::SystemTime, staleness: Duration) -> bool {
    std::time::SystemTime::now()
        .duration_since(modified)
        .is_ok_and(|age| age > staleness)
}

/// Removes the stale lock directory at `path`, but only if it is still the one seen with identity `seen` (its volume
/// serial number and file ID, which a directory made again in its place never shares, though its creation time may come
/// back) and still stale. Another waiter may have taken it over and made a fresh one since it was seen; that one is
/// never removed, and neither is one its holder touched since it was seen. On Windows the check and the removal go
/// through one handle; on macOS [`DirHandle::remove`] sets the directory aside by rename and checks the identity and
/// the staleness again on the entry it set aside, putting back one that fails either. Returns whether the directory
/// was removed.
#[cfg(accounts_locks)]
fn remove_if_still_stale(path: &Path, seen: DirIdentity, staleness: Duration) -> bool {
    #[cfg(windows)]
    {
        let Ok(dir) = DirHandle::open(path, DirAccess::Remove) else {
            return false;
        };
        if !still_stale_as_seen(&dir, seen, staleness) {
            return false;
        }
        dir.remove().is_ok()
    }
    #[cfg(unix)]
    {
        remove_if_still_stale_with(path, seen, staleness, &mut |_| ())
    }
}

/// As [`remove_if_still_stale`], calling `hook` at each [`Stage`] of the removal, so a test can act as a competing
/// taker or a touching holder between the check and the removal.
#[cfg(all(unix, accounts_locks))]
fn remove_if_still_stale_with(
    path: &Path,
    seen: DirIdentity,
    staleness: Duration,
    hook: &mut dyn FnMut(Stage),
) -> bool {
    let Ok(dir) = DirHandle::open(path, DirAccess::Remove) else {
        return false;
    };
    if !still_stale_as_seen(&dir, seen, staleness) {
        return false;
    }
    dir.remove(Some(staleness), hook)
        .is_ok_and(|removed| removed)
}

/// Whether the directory `dir` holds is still the one seen with identity `seen` and still stale.
#[cfg(accounts_locks)]
fn still_stale_as_seen(dir: &DirHandle, seen: DirIdentity, staleness: Duration) -> bool {
    dir.identity().is_ok_and(|identity| identity == seen)
        && dir
            .metadata()
            .is_ok_and(|current| is_stale(&current, staleness))
}

/// A fixed description of the lock directory at `path` for the log, so no path under the user's profile is ever
/// logged.
#[cfg(any(accounts_locks, target_os = "ios"))]
fn lock_description(path: &Path) -> &'static str {
    match path.file_name().and_then(|name| name.to_str()) {
        Some(".oauth_refresh.lock") => "Claude Code's OAuth refresh lock",
        Some(".claude.lock") => "Claude Code's credentials lock",
        Some(".claude.json.lock") => "Claude Code's config lock",
        _ => "the lock directory",
    }
}

/// A wait between 0.25 s and 0.5 s, so waiters that collide spread out.
#[cfg(accounts_locks)]
fn retry_jitter() -> Duration {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};

    let random = RandomState::new().build_hasher().finish();
    Duration::from_millis(250 + random % 251)
}

/// What a toucher keeps fresh: the lock directory with `identity` at `path`, touched every `interval`.
#[cfg(accounts_locks)]
struct Touching {
    path: PathBuf,
    identity: DirIdentity,
    /// The age past which the lock counts as a dead holder's: touches that fail for this long lose it.
    staleness: Duration,
    interval: Duration,
    lost: Arc<AtomicBool>,
}

#[cfg(accounts_locks)]
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

#[cfg(accounts_locks)]
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

#[cfg(accounts_locks)]
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

/// The Unix calls the locks need: a lock file opened without following a link and checked through its descriptor,
/// `flock(2)` on it, and a lock directory opened by descriptor so that its identity is read and its mtime set through
/// that one descriptor. Only macOS reaches them; Linux keeps every lock unsupported.
#[cfg(unix)]
#[cfg_attr(
    not(accounts_locks),
    expect(
        dead_code,
        reason = "off macOS no lock is taken, so nothing calls the Unix primitives"
    )
)]
mod unix {
    use std::ffi::{CStr, CString, OsStr, OsString};
    use std::fs::{File, Metadata};
    use std::io;
    use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::MetadataExt as _;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, SystemTime};

    use super::DirIdentity;
    use crate::guard::GuardErrorKind;

    /// `O_NOFOLLOW` makes the open fail with `ELOOP` when the lock file's own name is a symbolic link (open(2)), so a
    /// lock is never taken on the file a link points at. `O_NONBLOCK` keeps a FIFO or device planted at the name from
    /// blocking the open, so the regular-file check refuses it; it is cleared once the file passes.
    const LOCK_FILE_FLAGS: libc::c_int =
        libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;
    /// A lock directory, never through a link at its own name.
    const DIR_FLAGS: libc::c_int =
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    /// The folder a lock directory sits in, which is Claude Code's and may itself be reached through a link.
    const PARENT_FLAGS: libc::c_int = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;

    /// Opens or creates the lock file at `path` (mode 0600, never truncated, never through a link) and checks it
    /// through the open descriptor: a regular file with one name, owned by the effective uid, with no group or other
    /// permission bit.
    pub(super) fn open_lock_file(path: &Path) -> io::Result<File> {
        let name = c_string(path.as_os_str())?;
        let file = open_at(None, &name, LOCK_FILE_FLAGS).map_err(|error| {
            if error.raw_os_error() == Some(libc::ELOOP) {
                refusal("the lock file is a link, which is not followed")
            } else {
                error
            }
        })?;
        check_lock_file(&file.metadata()?)?;
        clear_nonblock(&file)?;
        Ok(file)
    }

    /// Clears `O_NONBLOCK` on `file`'s open file description, so it behaves as one opened without it.
    fn clear_nonblock(file: &File) -> io::Result<()> {
        // SAFETY: the descriptor is open for the life of `file`, and F_GETFL takes no further argument.
        let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the descriptor is open for the life of `file`, and F_SETFL takes the flags as its one argument.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn check_lock_file(metadata: &Metadata) -> io::Result<()> {
        if !metadata.is_file() {
            return Err(refusal("the lock file is not a regular file"));
        }
        if metadata.nlink() != 1 {
            return Err(refusal(
                "the lock file has more than one name, so another file would be locked",
            ));
        }
        // SAFETY: geteuid takes no arguments, cannot fail and touches no memory of ours.
        let current_uid = unsafe { libc::geteuid() };
        match crate::guard::unix_verdict(metadata.uid(), current_uid, metadata.mode()) {
            Ok(()) => Ok(()),
            Err(GuardErrorKind::NotOwned) => Err(refusal("the lock file is another user's")),
            Err(_) => Err(refusal(
                "the lock file can be read or written by another user",
            )),
        }
    }

    /// Takes an exclusive `flock` on `file` without waiting; `Ok(false)` when another open file holds it. A `flock`
    /// belongs to the open file description and is released when it closes (flock(2)), so a second open of the same
    /// file, in this process or another, is refused as a second handle is by the Windows byte lock.
    pub(super) fn try_lock_file(file: &File) -> io::Result<bool> {
        // SAFETY: the descriptor is open for the life of `file`.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EWOULDBLOCK)
            || error.kind() == io::ErrorKind::Interrupted
        {
            return Ok(false);
        }
        Err(error)
    }

    /// Releases the lock [`try_lock_file`] took.
    pub(super) fn unlock_file(file: &File) -> io::Result<()> {
        // SAFETY: the descriptor is open for the life of `file`.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// What a directory is opened to do. A descriptor opened for reading can read the identity, set the times (as the
    /// directory's owner) and name the directory for removal, so every access opens the same way.
    #[derive(Clone, Copy, Debug)]
    pub(super) enum DirAccess {
        Read,
        Touch,
        Remove,
    }

    /// An open descriptor of a lock directory, so that a check of its identity and the touch that follows bind to one
    /// directory even if the name is given to another meanwhile. It is held for one check and one action, never for a
    /// lock's life.
    #[derive(Debug)]
    pub(super) struct DirHandle {
        dir: File,
        path: PathBuf,
    }

    impl DirHandle {
        /// Opens the directory at `path`, never through a link at its name: a link there fails with `ELOOP`.
        pub(super) fn open(path: &Path, _access: DirAccess) -> io::Result<Self> {
            let dir = open_at(None, &c_string(path.as_os_str())?, DIR_FLAGS)?;
            Ok(Self {
                dir,
                path: path.to_path_buf(),
            })
        }

        /// The directory's device and inode numbers, read through the descriptor.
        pub(super) fn identity(&self) -> io::Result<DirIdentity> {
            Ok(identity_of(&self.dir.metadata()?))
        }

        /// The directory's metadata, read through the descriptor.
        pub(super) fn metadata(&self) -> io::Result<Metadata> {
            self.dir.metadata()
        }

        /// Sets the directory's mtime to now through the descriptor (`futimens`), leaving its access time alone.
        pub(super) fn touch(&self) -> io::Result<()> {
            self.dir.set_modified(SystemTime::now())
        }

        /// Removes the directory when the entry at its name is still this directory and, with `still_stale`, still
        /// older than that staleness; `Ok(false)` when another entry was there and was left. Unix has no removal
        /// through a descriptor, and a check by name followed by a removal by name would remove a directory another
        /// taker made in between, so the name is first renamed to a sibling only this removal knows
        /// (`<name>.rt-remove-<pid>-<n>`): from then on no other taker reaches the entry by the lock's name. The
        /// entry set aside is checked (`fstatat`, not following a link) and removed only when it is this directory
        /// and, with `still_stale`, still stale; otherwise it is put back with a rename that never replaces an entry
        /// made at the name meanwhile. A put-back that fails (another taker made a lock at the free name) leaves the
        /// entry set aside with a warning: its owner's toucher finds its lock gone and counts it lost, as after a
        /// crash. That is the one case in which another holder's directory leaves the name. Fails on a directory that
        /// is not empty, which is put back.
        pub(super) fn remove(
            self,
            still_stale: Option<Duration>,
            hook: &mut dyn FnMut(Stage),
        ) -> io::Result<bool> {
            let identity = self.identity()?;
            let Some(name) = self.path.file_name() else {
                return Err(refusal("the lock directory has no name"));
            };
            let parent_path = match self.path.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => parent,
                _ => Path::new("."),
            };
            let parent = open_at(None, &c_string(parent_path.as_os_str())?, PARENT_FLAGS)?;
            let aside_os = aside_name(name);
            let aside = c_string(&aside_os)?;
            let name = c_string(name)?;
            hook(Stage::BeforeRemove);
            // rename(2) of a directory within one file system is atomic: the name moves in one step, so the entry set
            // aside is whatever was at the name at that instant, and no later mkdir at the name can be it.
            rename_at(&parent, &name, &aside)?;
            let still_ours = stat_at(&parent, &aside).is_ok_and(|stat| {
                identity_of_stat(&stat) == identity
                    && still_stale.is_none_or(|staleness| {
                        modified_of(&stat)
                            .is_some_and(|modified| super::older_than(modified, staleness))
                    })
            });
            if still_ours {
                // SAFETY: `aside` is NUL-terminated and outlives the call, and `parent` holds an open folder
                // descriptor.
                if unsafe { libc::unlinkat(parent.as_raw_fd(), aside.as_ptr(), libc::AT_REMOVEDIR) }
                    == 0
                {
                    return Ok(true);
                }
                let error = io::Error::last_os_error();
                put_back(&parent, &aside, &name, &self.path);
                return Err(error);
            }
            hook(Stage::BeforePutBack);
            put_back(&parent, &aside, &name, &self.path);
            Ok(false)
        }
    }

    /// Numbers this process's set-aside names, so two removals never share one.
    static ASIDE_SERIAL: AtomicU64 = AtomicU64::new(0);

    /// A sibling name for `name` that only this removal uses: `<name>.rt-remove-<pid>-<n>`.
    pub(crate) fn aside_name(name: &OsStr) -> OsString {
        let mut aside = name.to_os_string();
        aside.push(format!(
            ".rt-remove-{}-{}",
            std::process::id(),
            ASIDE_SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        aside
    }

    /// Puts the entry set aside as `aside` back at `name`, both under `parent`, never replacing an entry made at
    /// `name` meanwhile; when that fails it is left set aside, with a warning naming the lock at `path` by its fixed
    /// description, as every log line here does, and the set-aside name's shape.
    fn put_back(parent: &File, aside: &CString, name: &CString, path: &Path) {
        if let Err(error) = rename_exclusive_at(parent.as_raw_fd(), aside, name) {
            tracing::warn!(
                lock = super::lock_description(path),
                set_aside_as = "<lock name>.rt-remove-<pid>-<n>, beside the lock",
                error = %error.kind(),
                "an entry set aside for removal was not the one to remove and could not be put back; it was left set aside"
            );
        }
    }

    /// Renames `from` to `to` under the folder `parent`, replacing whatever is at `to` (rename(2)).
    fn rename_at(parent: &File, from: &CString, to: &CString) -> io::Result<()> {
        // SAFETY: both names are NUL-terminated and outlive the call, and `parent` holds an open folder descriptor.
        if unsafe {
            libc::renameat(
                parent.as_raw_fd(),
                from.as_ptr(),
                parent.as_raw_fd(),
                to.as_ptr(),
            )
        } == 0
        {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// Renames `from` to `to`, both under the folder descriptor `dir` (`AT_FDCWD` for paths of their own), failing
    /// rather than replacing an entry at `to`: macOS's `renameatx_np(2)` with `RENAME_EXCL` fails with `EEXIST` when
    /// the destination exists.
    #[cfg(target_os = "macos")]
    pub(crate) fn rename_exclusive_at(dir: libc::c_int, from: &CStr, to: &CStr) -> io::Result<()> {
        // SAFETY: both names are NUL-terminated and outlive the call; `dir` is an open folder descriptor or AT_FDCWD.
        if unsafe { libc::renameatx_np(dir, from.as_ptr(), dir, to.as_ptr(), libc::RENAME_EXCL) }
            == 0
        {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// Off macOS no lock is taken, so no exclusive rename is ever needed.
    #[cfg(not(target_os = "macos"))]
    pub(crate) fn rename_exclusive_at(
        _dir: libc::c_int,
        _from: &CStr,
        _to: &CStr,
    ) -> io::Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }

    /// Renames the file `from` to `to`, never replacing an entry at `to`, as [`rename_exclusive_at`].
    pub(crate) fn rename_exclusive(from: &Path, to: &Path) -> io::Result<()> {
        rename_exclusive_at(
            libc::AT_FDCWD,
            &c_string(from.as_os_str())?,
            &c_string(to.as_os_str())?,
        )
    }

    /// The entry `name` under the folder `parent`, a link not followed (`fstatat(AT_SYMLINK_NOFOLLOW)`).
    fn stat_at(parent: &File, name: &CString) -> io::Result<libc::stat> {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: `name` is NUL-terminated and outlives the call, `parent` holds an open folder descriptor, and `stat`
        // is a buffer of the size fstatat writes.
        if unsafe {
            libc::fstatat(
                parent.as_raw_fd(),
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fstatat succeeded, so it filled the buffer.
        Ok(unsafe { stat.assume_init() })
    }

    /// [`identity_of`] for a raw `stat`, converted as [`MetadataExt::dev`](std::os::unix::fs::MetadataExt::dev)
    /// converts it, so the two compare.
    #[cfg_attr(
        target_os = "macos",
        expect(
            clippy::cast_sign_loss,
            reason = "macOS's dev_t is signed; std's MetadataExt::dev casts it to u64 the same way"
        )
    )]
    fn identity_of_stat(stat: &libc::stat) -> DirIdentity {
        let mut id = [0_u8; 16];
        id[..8].copy_from_slice(&stat.st_ino.to_le_bytes());
        (stat.st_dev as u64, id)
    }

    /// The modification time of a raw `stat`; `None` for one before the epoch.
    fn modified_of(stat: &libc::stat) -> Option<SystemTime> {
        let seconds = u64::try_from(stat.st_mtime).ok()?;
        let nanos = u32::try_from(stat.st_mtime_nsec).ok()?;
        SystemTime::UNIX_EPOCH.checked_add(Duration::new(seconds, nanos))
    }

    /// The points of a lock directory's removal at which a test hook runs, to act as a competing taker.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum Stage {
        /// The directory was checked and is about to be set aside for removal.
        BeforeRemove,
        /// The directory set aside was not the one to remove and is about to be put back.
        BeforePutBack,
    }

    fn identity_of(metadata: &Metadata) -> DirIdentity {
        let mut id = [0_u8; 16];
        id[..8].copy_from_slice(&metadata.ino().to_le_bytes());
        (metadata.dev(), id)
    }

    fn refusal(why: &'static str) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidInput, why)
    }

    fn c_string(name: &OsStr) -> io::Result<CString> {
        CString::new(name.as_bytes()).map_err(|_| refusal("a lock path holds a NUL byte"))
    }

    /// `name` opened with `flags` (and, when they create it, mode 0600) under the folder `parent`, or as a path of its
    /// own when there is none.
    fn open_at(parent: Option<&File>, name: &CString, flags: libc::c_int) -> io::Result<File> {
        let mode: libc::c_uint = 0o600;
        // SAFETY: `name` is NUL-terminated and outlives the call, `parent` holds an open folder descriptor, and the
        // mode is the one variadic argument `O_CREAT` reads (ignored without it).
        let fd = unsafe {
            match parent {
                Some(parent) => libc::openat(parent.as_raw_fd(), name.as_ptr(), flags, mode),
                None => libc::open(name.as_ptr(), flags, mode),
            }
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a fresh descriptor that nothing else owns.
        Ok(File::from(unsafe { OwnedFd::from_raw_fd(fd) }))
    }
}

#[cfg(all(test, accounts_locks))]
#[expect(
    clippy::expect_used,
    reason = "a test that cannot set up its lock directory must fail loudly"
)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime};

    use super::{CREDENTIALS_STALENESS, dir_identity, remove_if_still_stale};

    /// `FILE_FLAG_BACKUP_SEMANTICS` and `FILE_WRITE_ATTRIBUTES`: enough to open a directory and set its times.
    #[cfg(windows)]
    const BACKUP_SEMANTICS: u32 = 0x0200_0000;
    #[cfg(windows)]
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

    #[cfg(unix)]
    fn age(dir: &Path, ago: Duration) {
        fs::File::open(dir)
            .expect("the lock directory opens for its times")
            .set_modified(SystemTime::now() - ago)
            .expect("the lock directory's mtime can be set");
    }

    #[cfg(windows)]
    fn age(dir: &Path, ago: Duration) {
        use std::fs::OpenOptions;
        use std::os::windows::fs::OpenOptionsExt;

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

    /// Another taker's stale takeover of the lock at `lock`: it removes the directory and makes its own in its place.
    #[cfg(unix)]
    fn take_over(lock: &Path) {
        fs::remove_dir(lock).expect("the other taker removes the lock directory");
        fs::create_dir(lock).expect("the other taker makes its own lock directory");
    }

    /// The entries beside `lock` that a removal set aside and left.
    #[cfg(unix)]
    fn set_aside(lock: &Path) -> usize {
        let prefix = format!(
            "{}.rt-remove-",
            lock.file_name()
                .and_then(|name| name.to_str())
                .expect("the lock has a name")
        );
        fs::read_dir(lock.parent().expect("the lock has a folder"))
            .expect("the lock's folder lists")
            .filter(|entry| {
                entry
                    .as_ref()
                    .is_ok_and(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
            })
            .count()
    }

    #[cfg(unix)]
    #[test]
    fn a_guard_does_not_remove_a_directory_that_replaced_its_own() {
        use super::{Stage, remove_if_ours_with};

        let lock = scratch("a_guard_does_not_remove_a_directory_that_replaced_its_own")
            .join(".claude.json.lock");
        fs::create_dir(&lock).expect("the guard's lock is made");
        let ours = dir_identity(&lock).expect("the guard's lock is seen");

        // Another taker replaces the directory between the guard's check and its removal.
        let removed = remove_if_ours_with(&lock, ours, &mut |stage| {
            if stage == Stage::BeforeRemove {
                take_over(&lock);
            }
        })
        .expect("the removal runs");

        assert!(!removed, "the replacement is not reported removed");
        let there = dir_identity(&lock).expect("the other taker's lock is still there");
        assert_ne!(there, ours, "the directory left is the other taker's");
        assert_eq!(set_aside(&lock), 0, "nothing is left set aside");
    }

    #[cfg(unix)]
    #[test]
    fn a_stale_takeover_does_not_remove_a_lock_made_in_its_place() {
        use super::{Stage, remove_if_still_stale_with};

        let lock = scratch("a_stale_takeover_does_not_remove_a_lock_made_in_its_place")
            .join(".claude.lock");
        fs::create_dir(&lock).expect("a dead holder's lock is made");
        age(&lock, Duration::from_secs(61));
        let seen = dir_identity(&lock).expect("the stale lock is seen");

        // Another waiter takes the stale lock over between this waiter's check and its removal.
        let removed =
            remove_if_still_stale_with(&lock, seen, CREDENTIALS_STALENESS, &mut |stage| {
                if stage == Stage::BeforeRemove {
                    take_over(&lock);
                }
            });

        assert!(
            !removed,
            "a lock made in the stale one's place is never removed"
        );
        let there = dir_identity(&lock).expect("the other waiter's lock is still there");
        assert_ne!(there, seen, "the directory left is the other waiter's");
        assert_eq!(set_aside(&lock), 0, "nothing is left set aside");
    }

    #[cfg(unix)]
    #[test]
    fn a_stale_takeover_does_not_remove_a_lock_touched_meanwhile() {
        use super::{Stage, remove_if_still_stale_with};

        let lock = scratch("a_stale_takeover_does_not_remove_a_lock_touched_meanwhile")
            .join(".claude.lock");
        fs::create_dir(&lock).expect("a holder's lock is made");
        age(&lock, Duration::from_secs(61));
        let seen = dir_identity(&lock).expect("the stale lock is seen");

        // Its holder touches the lock between this waiter's check and its removal: it is live again.
        let removed =
            remove_if_still_stale_with(&lock, seen, CREDENTIALS_STALENESS, &mut |stage| {
                if stage == Stage::BeforeRemove {
                    age(&lock, Duration::ZERO);
                }
            });

        assert!(!removed, "a lock touched since its check is never removed");
        let there = dir_identity(&lock).expect("the holder's lock is still there");
        assert_eq!(there, seen, "the holder keeps its own directory");
        assert_eq!(set_aside(&lock), 0, "nothing is left set aside");
    }

    #[cfg(unix)]
    #[test]
    fn a_put_back_never_replaces_a_new_directory() {
        use super::{Stage, remove_if_ours_with};

        let lock = scratch("a_put_back_never_replaces_a_new_directory").join(".claude.lock");
        fs::create_dir(&lock).expect("the guard's lock is made");
        let ours = dir_identity(&lock).expect("the guard's lock is seen");
        let mut newest = None;

        // A second taker replaces the guard's directory before the removal, and a third makes its own at the free
        // name while the second's is set aside.
        let removed = remove_if_ours_with(&lock, ours, &mut |stage| match stage {
            Stage::BeforeRemove => take_over(&lock),
            Stage::BeforePutBack => {
                fs::create_dir(&lock).expect("the third taker makes its lock at the free name");
                newest = Some(dir_identity(&lock).expect("the third taker's lock is seen"));
            }
        })
        .expect("the removal runs");

        assert!(!removed, "neither taker's directory is reported removed");
        let there = dir_identity(&lock).expect("a lock directory is at the name");
        assert_eq!(
            Some(there),
            newest,
            "the put-back did not replace the third taker's directory"
        );
        assert_eq!(
            set_aside(&lock),
            1,
            "the second taker's directory is left set aside, not removed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_transient_look_error_waits_instead_of_refusing() {
        use super::{LockError, wait_on_holder_with};

        let lock = scratch("a_transient_look_error_waits_instead_of_refusing").join(".claude.lock");
        for errno in [libc::EINTR, libc::EMFILE, libc::ENFILE, libc::EIO] {
            let waited = wait_on_holder_with(&lock, CREDENTIALS_STALENESS, |_| {
                Err(std::io::Error::from_raw_os_error(errno))
            });
            assert!(
                waited.is_ok(),
                "errno {errno} is waited out, not refused: {waited:?}"
            );
        }
        for errno in [libc::ELOOP, libc::ENOTDIR, libc::EACCES, libc::EPERM] {
            let refused = wait_on_holder_with(&lock, CREDENTIALS_STALENESS, |_| {
                Err(std::io::Error::from_raw_os_error(errno))
            });
            assert!(
                matches!(refused, Err(LockError::Io(_))),
                "errno {errno} is refused at once: {refused:?}"
            );
        }
    }

    /// Windows alone carries a settable creation time that a replacement could share.
    #[cfg(windows)]
    #[test]
    fn stale_removal_compares_file_identity_not_creation_time() {
        use std::fs::{FileTimes, OpenOptions};
        use std::os::windows::fs::{FileTimesExt, OpenOptionsExt};

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
