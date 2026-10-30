//! Durable atomic file replacement.
//!
//! The daemon rewrites a handful of small files — session sidecars, `state.json`,
//! `devices.json`, the secret index, the daemon handshake — while a crash, a
//! power loss or a standby the machine never wakes from can strike at any
//! moment. A plain temp-write-and-rename leaves both the temp file's bytes and
//! the rename's directory entry unflushed, which is how a sidecar or
//! `daemon.json` ends up zero-length or torn.
//!
//! [`write_atomic`] closes the torn-file gap: the bytes go to a sibling temp
//! file that is flushed with `sync_all`, and only then does `std::fs::rename`
//! replace the target, so a reader sees either the old bytes or the new
//! complete bytes. A power cut can still roll a same-volume rename back to the
//! old bytes — the file is then stale, never torn.
//!
//! On Unix the rename is followed by a `sync_all` of the parent folder, so the
//! new name survives a power cut too; on Windows the parent folder is not
//! synced. A replace that fails with a sharing violation or an access-denied
//! refusal is retried a few times, since a reader still holding the old file
//! for a moment is not a failure.
//!
//! The temp file is created owner-only through [`crate::secret`], because some
//! of these files carry the auth token or a spawn config's secret rows.

use crate::secret;
use anyhow::Context as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tracing::warn;

/// How many times a replace a sharing violation or an access-denied refusal
/// turned down is attempted in all.
const MAX_ATTEMPTS: u32 = 5;

/// The pause between two attempts.
const RETRY_DELAY: Duration = Duration::from_millis(20);

/// Windows `ERROR_SHARING_VIOLATION`: another handle holds the target without
/// sharing it, which a concurrent reader does for a moment.
#[cfg(windows)]
const ERROR_SHARING_VIOLATION: i32 = 32;

/// Windows `ERROR_ACCESS_DENIED`, which a replace onto a target another handle
/// holds open without delete sharing also reports.
#[cfg(windows)]
const ERROR_ACCESS_DENIED: i32 = 5;

/// Write `bytes` to `path` through a flushed sibling temp file and a rename, so
/// a reader sees the old bytes or the new complete bytes, and a power loss
/// never leaves a torn or zero-length one.
///
/// Every call gets its own temp name (the target's file name plus this
/// process's id and a process-wide counter), so two writers of the same file
/// never write into or replace each other's temp file. The temp file is created
/// owner-only, and the replace preserves that mode onto the target.
///
/// # Errors
///
/// Creating, writing or flushing the temp file failed, or the replace did. The
/// temp file is removed and the target keeps its old bytes.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let tmp = temp_path(path);
    if let Err(err) = write_temp(&tmp, bytes) {
        remove_temp(&tmp);
        return Err(err).with_context(|| format!("writing {}", tmp.display()));
    }
    if let Err(err) = replace_with_retry(&tmp, path) {
        remove_temp(&tmp);
        return Err(err).with_context(|| format!("replacing {}", path.display()));
    }
    sync_parent_directory(path);
    Ok(())
}

/// The temp path a write to `path` lands in: the target's file name, this
/// process's id and a process-wide counter, so no two calls collide.
fn temp_path(path: &Path) -> PathBuf {
    static NEXT_TMP: AtomicU64 = AtomicU64::new(0);
    let n = NEXT_TMP.fetch_add(1, Ordering::Relaxed);
    let mut name = path.file_name().map_or_else(
        || std::ffi::OsString::from("write"),
        std::ffi::OsStr::to_os_string,
    );
    name.push(format!(".{}.{n}.tmp", std::process::id()));
    path.with_file_name(name)
}

fn write_temp(tmp: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = secret::create_private(tmp)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn remove_temp(tmp: &Path) {
    match std::fs::remove_file(tmp) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => warn!(?err, path = %tmp.display(), "failed to remove a temp file"),
    }
}

fn replace_with_retry(tmp: &Path, target: &Path) -> std::io::Result<()> {
    let mut attempt = 1;
    loop {
        match replace_file(tmp, target) {
            Ok(()) => return Ok(()),
            Err(err) => {
                notify_failed_attempt();
                if attempt >= MAX_ATTEMPTS || !is_transient(&err) {
                    return Err(err);
                }
                std::thread::sleep(RETRY_DELAY);
                attempt += 1;
            }
        }
    }
}

/// Whether `err` is a refusal worth another attempt: the target is held open
/// by someone else for the moment, which the next attempt can get past.
fn is_transient(err: &std::io::Error) -> bool {
    #[cfg(windows)]
    {
        matches!(
            err.raw_os_error(),
            Some(ERROR_SHARING_VIOLATION | ERROR_ACCESS_DENIED)
        )
    }
    #[cfg(not(windows))]
    {
        let _ = err;
        false
    }
}

/// Run the test-only hook a caller installed, if any. Tests use it to release a
/// handle they hold on the target at a known point instead of racing a sleep.
fn notify_failed_attempt() {
    #[cfg(test)]
    AFTER_FAILED_ATTEMPT.with(|slot| {
        if let Some(hook) = slot.borrow().as_ref() {
            hook();
        }
    });
}

#[cfg(test)]
thread_local! {
    /// Called after each replace attempt that failed.
    static AFTER_FAILED_ATTEMPT: std::cell::RefCell<Option<Box<dyn Fn()>>> =
        std::cell::RefCell::new(None);
}

#[cfg(test)]
fn set_failed_attempt_hook(hook: Option<Box<dyn Fn()>>) {
    AFTER_FAILED_ATTEMPT.with(|slot| *slot.borrow_mut() = hook);
}

/// Replace `target` with the already-flushed `tmp`.
///
/// `std::fs::rename` is the platform's best replace, not a bare
/// `MoveFileExW(REPLACE_EXISTING)`: on Windows it falls back to
/// `SetFileInformationByHandle(FileRenameInfoEx)` when the target is held open
/// by a handle that shares delete (a scan, the indexer, one of the daemon's own
/// readers), and it prefixes long paths.
fn replace_file(tmp: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::rename(tmp, target)
}

/// Flush the folder entry the rename touched, so the target's new name survives
/// a power loss.
///
/// A failure is logged rather than returned: the rename already succeeded and
/// the target holds the new bytes, so an error here would report a write that
/// happened as failed.
#[cfg(unix)]
fn sync_parent_directory(path: &Path) {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    if let Err(err) = std::fs::File::open(parent).and_then(|dir| dir.sync_all()) {
        warn!(?err, path = %path.display(), "failed to sync the folder of a written file");
    }
}

/// Windows never syncs the parent folder of a written file.
#[cfg(not(unix))]
fn sync_parent_directory(_path: &Path) {}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("rt-atomic-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create the scratch dir");
        root
    }

    fn dir_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("read the scratch dir")
            .map(|entry| {
                entry
                    .expect("a dir entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    #[test]
    fn write_atomic_replaces_the_target_and_leaves_no_temp_file() {
        let dir = scratch("replaces");
        let path = dir.join("meta.json");
        write_atomic(&path, b"first").expect("the first write");
        write_atomic(&path, b"second").expect("the second write");
        assert_eq!(std::fs::read(&path).expect("read the target"), b"second");
        assert_eq!(dir_names(&dir), vec!["meta.json"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn write_atomic_leaves_old_bytes_when_the_replace_fails() {
        use std::os::windows::fs::OpenOptionsExt as _;

        let dir = scratch("replace-fails");
        let path = dir.join("state.json");
        std::fs::write(&path, b"old").expect("seed the target");
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&path)
            .expect("hold the target open without sharing it");
        let err = write_atomic(&path, b"new").expect_err("every attempt sees a sharing violation");
        drop(held);
        assert!(
            format!("{err:#}").contains("state.json"),
            "the error names the target: {err:#}"
        );
        assert_eq!(std::fs::read(&path).expect("read the target"), b"old");
        assert_eq!(dir_names(&dir), vec!["state.json"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(not(windows))]
    #[test]
    fn write_atomic_leaves_old_bytes_when_the_replace_fails() {
        let dir = scratch("replace-fails");
        let path = dir.join("state.json");
        std::fs::create_dir_all(path.join("occupied")).expect("fill the target directory");
        let err = write_atomic(&path, b"new").expect_err("a file cannot replace a directory");
        assert!(
            format!("{err:#}").contains("state.json"),
            "the error names the target: {err:#}"
        );
        assert_eq!(dir_names(&dir), vec!["state.json"]);
        assert_eq!(
            dir_names(&path),
            vec!["occupied"],
            "the target keeps its old contents"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_atomic_replaces_a_target_held_open_for_reading() {
        let dir = scratch("held-for-reading");
        let path = dir.join("state.json");
        std::fs::write(&path, b"old").expect("seed the target");
        let reader = std::fs::File::open(&path).expect("open the target for reading");
        write_atomic(&path, b"new")
            .expect("a reader sharing the handle does not block the replace");
        drop(reader);
        assert_eq!(std::fs::read(&path).expect("read the target"), b"new");
        assert_eq!(dir_names(&dir), vec!["state.json"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn write_atomic_retries_a_sharing_violation_then_succeeds() {
        use std::cell::Cell;
        use std::os::windows::fs::OpenOptionsExt as _;
        use std::sync::mpsc;

        let dir = scratch("retry");
        let path = dir.join("devices.json");
        std::fs::write(&path, b"old").expect("seed the target");
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&path)
            .expect("hold the target open without sharing it");

        let (release_tx, release_rx) = mpsc::channel::<()>();
        let (dropped_tx, dropped_rx) = mpsc::channel::<()>();
        let holder = std::thread::spawn(move || {
            release_rx.recv().expect("the writer asks for the handle");
            drop(held);
            dropped_tx.send(()).expect("report the handle dropped");
        });

        let released = Cell::new(false);
        let guard = HookGuard::set(Box::new(move || {
            if released.get() {
                return;
            }
            released.set(true);
            release_tx
                .send(())
                .expect("ask the holder to release the target");
            dropped_rx
                .recv()
                .expect("wait for the handle to be gone before the next attempt");
        }));

        let result = write_atomic(&path, b"new");
        drop(guard);
        result.expect("the retry gets past the released handle");
        holder.join().expect("the holder thread finishes");
        assert_eq!(std::fs::read(&path).expect("read the target"), b"new");
        assert_eq!(dir_names(&dir), vec!["devices.json"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_atomic_names_are_unique_per_call() {
        let dir = scratch("names");
        let path = dir.join("meta.json");
        let first = temp_path(&path);
        let second = temp_path(&path);
        assert_ne!(first, second);
        assert_eq!(first.parent(), path.parent());
        let first_name = first.file_name().expect("a temp name").to_string_lossy();
        let second_name = second.file_name().expect("a temp name").to_string_lossy();
        assert!(
            first_name.starts_with("meta.json."),
            "the temp name extends the target's: {first_name}"
        );
        assert!(first_name.ends_with(".tmp"), "{first_name}");
        assert!(second_name.ends_with(".tmp"), "{second_name}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "measurement: run it with --run-ignored only and --no-capture"]
    #[expect(clippy::print_stderr, reason = "the measurement is the test's output")]
    fn sidecar_write_cost() {
        let dir = scratch("cost");
        let path = dir.join("meta.json");
        let bytes = vec![b'x'; 2048];
        let mut samples = Vec::with_capacity(200);
        for _ in 0..200 {
            let started = std::time::Instant::now();
            write_atomic(&path, &bytes).expect("the write");
            samples.push(started.elapsed());
        }
        samples.sort_unstable();
        let p50 = samples[samples.len() / 2];
        let p90 = samples[samples.len() * 9 / 10];
        eprintln!("sidecar_write_cost over 200 writes: p50={p50:?} p90={p90:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Installs the failed-attempt hook for as long as it lives.
    struct HookGuard;

    impl HookGuard {
        fn set(hook: Box<dyn Fn()>) -> Self {
            set_failed_attempt_hook(Some(hook));
            Self
        }
    }

    impl Drop for HookGuard {
        fn drop(&mut self) {
            set_failed_attempt_hook(None);
        }
    }
}
