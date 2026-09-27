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
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    use uuid::Uuid;

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

        let second = acquire(scratch.path(), Duration::from_millis(200)).expect("second acquire");
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

        // The holder takes the lock, signals that it has it, holds it long
        // enough for the contending acquire to start waiting, then releases.
        let (held_tx, held_rx) = mpsc::channel::<()>();
        let holder_dir = dir.clone();
        let holder = std::thread::spawn(move || {
            let lock = acquire(&holder_dir, Duration::from_secs(3))
                .expect("holder acquire")
                .expect("holder takes the lock");
            held_tx.send(()).expect("signal lock held");
            std::thread::sleep(Duration::from_millis(300));
            drop(lock);
        });

        held_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("holder signalled it took the lock");
        let started = Instant::now();
        let acquired = acquire(&dir, Duration::from_secs(3)).expect("contended acquire");
        let waited = started.elapsed();

        assert!(acquired.is_some(), "the wait must outlast the holder");
        assert!(
            waited >= Duration::from_millis(200),
            "acquire returned after {waited:?}, before the holder released"
        );
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
