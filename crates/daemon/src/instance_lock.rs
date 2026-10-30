//! Single-instance guard for the daemon.
//!
//! Exactly one `rustling-tulipd` may serve a given config dir. A second launch
//! against the same config dir — an autostart entry racing the daemon a client
//! spawned, say — would otherwise run its whole startup while the first daemon
//! is alive: rotate the live daemon's `daemon.log`, sweep its binary cache,
//! reap the orphan tracers, overwrite `state.json`, and finally replace
//! `daemon.json`, so every client handshake points at a port nothing listens on.
//! The guard is an exclusive lock on `<config_dir>/daemon.lock`; the losing
//! daemon exits without touching any of it.

use anyhow::Context as _;
use std::fs::{File, TryLockError};
use std::path::Path;
use std::time::{Duration, Instant};

/// Name of the lock file inside the config dir.
const LOCK_FILE: &str = "daemon.lock";

/// How long to wait between attempts while another process holds the lock.
const RETRY_INTERVAL: Duration = Duration::from_millis(100);

/// An exclusive lock on `<config_dir>/daemon.lock`, held for the lifetime of
/// the value. Dropping it — or the process exiting — releases the lock, so the
/// next daemon started against this config dir can take it.
pub struct InstanceLock {
    /// The open file the OS lock lives on. Held (never read) so the lock stays
    /// taken; closing the handle releases it.
    _file: File,
}

/// Take the daemon's single-instance lock in `config_dir`, waiting up to `wait`
/// for a lock another process holds to be released.
///
/// Returns `Ok(Some(lock))` when this process now holds the lock, `Ok(None)`
/// when another process held it for the whole wait, and `Err` for any other
/// I/O failure. A client that retires a healthy-but-incompatible daemon and
/// spawns a replacement leaves both processes alive for a moment, which is
/// what `wait` is for.
pub fn acquire(config_dir: &Path, wait: Duration) -> anyhow::Result<Option<InstanceLock>> {
    let path = config_dir.join(LOCK_FILE);
    // The lock lives on this handle, so the file is opened without truncating
    // or appending: neither daemon writes a byte to it.
    let file = File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening daemon lock file {}", path.display()))?;

    let deadline = Instant::now() + wait;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(Some(InstanceLock { _file: file })),
            Err(TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    return Ok(None);
                }
                std::thread::sleep(RETRY_INTERVAL);
            }
            Err(TryLockError::Error(err)) => {
                return Err(anyhow::Error::new(err))
                    .with_context(|| format!("locking daemon lock file {}", path.display()));
            }
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert scratch setup preconditions with expect for clear failure messages"
)]
mod tests {
    use super::{LOCK_FILE, acquire};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;
    use uuid::Uuid;

    /// The hang guard on a wait for the other thread, failing as "hung".
    const HANG_GUARD: Duration = Duration::from_secs(120);

    /// RAII scratch dir under the OS temp root. Drop removes the tree.
    struct Scratch {
        path: PathBuf,
    }

    impl Scratch {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "rt-instance-lock-{label}-{}",
                Uuid::new_v4().simple()
            ));
            fs::create_dir_all(&path).expect("create scratch dir");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn second_acquire_returns_none_while_first_is_held() {
        let scratch = Scratch::new("held");
        let first = acquire(scratch.path(), Duration::ZERO).expect("first acquire");
        assert!(first.is_some(), "the first acquire must take the lock");

        let second = acquire(scratch.path(), Duration::ZERO).expect("second acquire");
        assert!(
            second.is_none(),
            "a second acquire must report the lock as taken"
        );
    }

    #[test]
    fn acquire_succeeds_after_holder_drops() {
        let scratch = Scratch::new("dropped");
        let first = acquire(scratch.path(), Duration::ZERO)
            .expect("first acquire")
            .expect("first acquire holds the lock");
        drop(first);

        let again = acquire(scratch.path(), Duration::ZERO).expect("reacquire");
        assert!(again.is_some(), "the lock is free once the holder dropped");
    }

    #[test]
    fn acquire_waits_for_holder_released_within_wait() {
        let scratch = Scratch::new("waits");
        let dir = scratch.path().to_path_buf();

        // The holder takes the lock and signals that it has it; told to
        // release, it marks the release just before dropping the lock. The
        // release is asked for from a thread started just before the
        // contending acquire, which so most often meets the lock still held;
        // the mark it reads once it has the lock proves it never took the
        // lock before the holder dropped it, however the threads ran.
        let (held_tx, held_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let released = Arc::new(AtomicBool::new(false));
        let holder_dir = dir.clone();
        let holder_released = Arc::clone(&released);
        let holder = std::thread::spawn(move || {
            let lock = acquire(&holder_dir, Duration::ZERO)
                .expect("holder acquire")
                .expect("holder takes the lock");
            held_tx.send(()).expect("signal lock held");
            release_rx
                .recv_timeout(HANG_GUARD)
                .expect("hung: the test never asked for the release");
            holder_released.store(true, Ordering::SeqCst);
            drop(lock);
        });

        held_rx
            .recv_timeout(HANG_GUARD)
            .expect("hung: the holder never took the lock");
        let releaser = std::thread::spawn(move || release_tx.send(()));
        let acquired = acquire(&dir, HANG_GUARD).expect("contended acquire");

        assert!(
            acquired.is_some(),
            "hung: the lock was not free within the guard"
        );
        assert!(
            released.load(Ordering::SeqCst),
            "acquire returned the lock before the holder released it"
        );
        releaser
            .join()
            .expect("releaser thread finishes")
            .expect("ask the holder to release");
        holder.join().expect("holder thread finishes");
    }

    #[test]
    fn acquire_creates_missing_lock_file() {
        let scratch = Scratch::new("missing");
        let lock_path = scratch.path().join(LOCK_FILE);
        assert!(
            !lock_path.exists(),
            "scratch dir starts without a lock file"
        );

        let lock = acquire(scratch.path(), Duration::ZERO).expect("acquire");
        assert!(lock.is_some(), "a missing lock file is created and taken");
        assert!(lock_path.exists(), "acquire creates the lock file");
    }
}
