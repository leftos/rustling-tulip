//! The one-byte file lock and Claude Code's directory locks: contention, stale takeover, the toucher and release.

use std::fs::{self, OpenOptions};
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use claude_accounts::{
    CONFIG_STALENESS, CREDENTIALS_STALENESS, ClaudeCodeLocks, ClaudePaths, DirLock, DirLockGuard,
    FileLock, LockError, LockTiming,
};

use crate::support::Scratch;

/// `FILE_FLAG_BACKUP_SEMANTICS`: the flag that lets `CreateFileW` open a directory.
const BACKUP_SEMANTICS: u32 = 0x0200_0000;
/// `FILE_WRITE_ATTRIBUTES`: enough access to set a directory's times.
const WRITE_ATTRIBUTES: u32 = 0x0100;
/// `FILE_READ_ATTRIBUTES`: enough access to read a directory's times.
const READ_ATTRIBUTES: u32 = 0x0080;

fn set_dir_mtime(dir: &Path, ago: Duration) {
    let handle = OpenOptions::new()
        .access_mode(WRITE_ATTRIBUTES)
        .custom_flags(BACKUP_SEMANTICS)
        .open(dir)
        .expect("the lock directory opens for its times");
    handle
        .set_modified(SystemTime::now() - ago)
        .expect("the lock directory's mtime can be set");
}

fn mtime_age(dir: &Path) -> Duration {
    let modified = fs::metadata(dir)
        .expect("the lock directory exists")
        .modified()
        .expect("the mtime reads");
    SystemTime::now()
        .duration_since(modified)
        .unwrap_or_default()
}

fn timing(timeout_ms: u64) -> LockTiming {
    LockTiming::new(Duration::from_millis(timeout_ms), Duration::from_secs(3))
}

#[test]
fn lock_contention_with_held_lock() {
    let scratch = Scratch::empty("lock_contention_with_held_lock");
    let path = scratch.dir.join("nested").join(".lock");
    std::fs::create_dir(scratch.dir.join("nested")).expect("the lock's folder can be made");
    let timeout = Duration::from_millis(300);

    let first = FileLock::acquire(&path, timeout).expect("a free lock is taken");
    let started = Instant::now();
    let second = FileLock::acquire(&path, timeout);
    assert!(
        matches!(second, Err(LockError::Timeout)),
        "a held lock times out: {second:?}"
    );
    assert!(
        started.elapsed() >= timeout,
        "the second handle waited out its timeout"
    );

    drop(first);
    let third = FileLock::acquire(&path, timeout).expect("a released lock is taken");
    drop(third);
}

#[test]
fn dir_lock_stale_takeover() {
    let scratch = Scratch::empty("dir_lock_stale_takeover");
    let cases = [
        ("credentials-stale", CREDENTIALS_STALENESS, 61, true),
        ("credentials-live", CREDENTIALS_STALENESS, 59, false),
        ("config-stale", CONFIG_STALENESS, 11, true),
        ("config-live", CONFIG_STALENESS, 9, false),
    ];
    for (name, staleness, age_secs, taken) in cases {
        let dir = scratch.dir.join(format!("{name}.lock"));
        fs::create_dir(&dir).expect("the held lock directory is made");
        set_dir_mtime(&dir, Duration::from_secs(age_secs));

        let result = DirLock::acquire(&dir, staleness, timing(700));
        if taken {
            let guard = result.expect("a stale lock is taken over");
            assert!(
                mtime_age(&dir) < Duration::from_secs(5),
                "{name}: the retaken lock is fresh"
            );
            drop(guard);
            assert!(!dir.exists(), "{name}: the retaken lock is released");
        } else {
            assert!(
                matches!(result, Err(LockError::Timeout)),
                "{name}: a live lock is never stolen: {result:?}"
            );
            assert!(dir.exists(), "{name}: the live holder's lock is left alone");
        }
    }
}

#[test]
fn dir_lock_touched_while_held() {
    let scratch = Scratch::empty("dir_lock_touched_while_held");
    let dir = scratch.dir.join(".oauth_refresh.lock");
    let quick = LockTiming::new(Duration::from_secs(1), Duration::from_millis(50));
    let guard = DirLock::acquire(&dir, CREDENTIALS_STALENESS, quick).expect("a free lock is taken");
    set_dir_mtime(&dir, Duration::from_secs(30));
    assert!(mtime_age(&dir) >= Duration::from_secs(29));

    // A loaded machine can starve the toucher thread, so poll for the refresh rather than assume one interval is enough.
    let deadline = Instant::now() + Duration::from_secs(10);
    while mtime_age(&dir) >= Duration::from_secs(5) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(25));
    }
    assert!(
        mtime_age(&dir) < Duration::from_secs(5),
        "the toucher refreshed the mtime"
    );
    drop(guard);
}

#[test]
fn dir_lock_removed_on_drop() {
    let scratch = Scratch::empty("dir_lock_removed_on_drop");
    let dir = scratch.dir.join("parent").join(".claude.json.lock");
    let guard = DirLock::acquire(&dir, CONFIG_STALENESS, timing(500))
        .expect("a free lock is taken, its parent made");
    assert!(dir.is_dir());
    assert_eq!(guard.path(), dir);
    let started = Instant::now();
    drop(guard);
    assert!(!dir.exists(), "the lock directory is removed on drop");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the toucher stops promptly"
    );

    let guard = DirLock::acquire(&dir, CONFIG_STALENESS, timing(500))
        .expect("a released lock is free again");
    fs::remove_dir(&dir).expect("the lock can be taken away while held");
    drop(guard);
    assert!(
        !dir.exists(),
        "a lock that vanished while held drops quietly"
    );
}

/// Another taker's stale takeover of the lock at `dir`: it removes the directory and makes its own in its place.
fn take_over(dir: &Path) {
    fs::remove_dir(dir).expect("the other taker removes the lock directory");
    fs::create_dir(dir).expect("the other taker makes its own lock directory");
}

#[test]
fn drop_removes_its_own_lock() {
    let scratch = Scratch::empty("drop_removes_its_own_lock");
    let dir = scratch.dir.join(".claude.lock");
    let guard =
        DirLock::acquire(&dir, CREDENTIALS_STALENESS, timing(500)).expect("a free lock is taken");
    assert!(guard.still_owned(), "a lock just taken is ours");
    drop(guard);
    assert!(!dir.exists(), "our own lock is removed on drop");
}

#[test]
fn drop_leaves_a_lock_taken_over_as_stale() {
    let scratch = Scratch::empty("drop_leaves_a_lock_taken_over_as_stale");
    let dir = scratch.dir.join(".claude.lock");
    let guard =
        DirLock::acquire(&dir, CREDENTIALS_STALENESS, timing(500)).expect("a free lock is taken");
    take_over(&dir);
    drop(guard);
    assert!(dir.is_dir(), "the other taker's lock is left alone");
}

#[test]
fn still_owned_is_false_after_takeover() {
    let scratch = Scratch::empty("still_owned_is_false_after_takeover");
    let dir = scratch.dir.join(".oauth_refresh.lock");
    let guard =
        DirLock::acquire(&dir, CREDENTIALS_STALENESS, timing(500)).expect("a free lock is taken");
    assert!(guard.still_owned(), "a lock just taken is ours");
    take_over(&dir);
    assert!(
        !guard.still_owned(),
        "a lock re-made by another taker is not ours"
    );
    drop(guard);
    assert!(dir.is_dir(), "the other taker's lock is left alone");
}

#[test]
fn still_owned_is_false_when_the_toucher_failed() {
    let scratch = Scratch::empty("still_owned_is_false_when_the_toucher_failed");
    let dir = scratch.dir.join(".claude.json.lock");
    let quick = LockTiming::new(Duration::from_secs(1), Duration::from_millis(50));
    let guard = DirLock::acquire(&dir, CONFIG_STALENESS, quick).expect("a free lock is taken");
    assert!(guard.still_owned(), "a lock just taken is ours");

    // The directory goes away under the guard, so the toucher's next touch fails.
    fs::remove_dir(&dir).expect("the lock can be taken away while held");

    assert!(
        wait_until_lost(&guard),
        "a lock whose toucher failed is no longer counted as ours"
    );
    drop(guard);
    assert!(
        !dir.exists(),
        "a drop makes nothing in place of a lock that is gone"
    );
}

/// Polls `guard.still_owned()` every 20 ms for up to 5 s; whether it turned false.
fn wait_until_lost(guard: &DirLockGuard) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while guard.still_owned() {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(20));
    }
    true
}

/// Denies everyone the right to set `dir`'s attributes and times, so a touch fails while the directory stays as it was.
fn deny_touch(dir: &Path) {
    let status = Command::new("icacls")
        .arg(dir)
        .args(["/deny", "*S-1-1-0:(WA)"])
        .stdout(Stdio::null())
        .status()
        .expect("icacls runs");
    assert!(status.success(), "icacls denies the touch: {status}");
}

#[test]
fn still_owned_is_false_once_the_lock_is_stale() {
    let scratch = Scratch::empty("still_owned_is_false_once_the_lock_is_stale");
    let dir = scratch.dir.join(".claude.json.lock");
    let guard =
        DirLock::acquire(&dir, CONFIG_STALENESS, timing(500)).expect("a free lock is taken");
    assert!(guard.still_owned(), "a lock just taken is ours");

    // The machine slept past the staleness while the lock was held: another taker may take it over now.
    set_dir_mtime(&dir, CONFIG_STALENESS + Duration::from_secs(1));

    assert!(
        !guard.still_owned(),
        "a lock gone stale is no longer counted as ours"
    );
    drop(guard);
    assert!(
        !dir.exists(),
        "a stale lock still ours by identity is removed on drop"
    );
}

#[test]
fn drop_removes_its_own_lock_after_a_failed_touch() {
    let scratch = Scratch::empty("drop_removes_its_own_lock_after_a_failed_touch");
    let dir = scratch.dir.join(".oauth_refresh.lock");
    let quick = LockTiming::new(Duration::from_secs(1), Duration::from_millis(50));
    // A failed touch loses the lock only once it would be stale, so the lock is taken with a staleness the test outlives.
    let guard =
        DirLock::acquire(&dir, Duration::from_millis(300), quick).expect("a free lock is taken");

    deny_touch(&dir);

    assert!(
        wait_until_lost(&guard),
        "a lock whose toucher failed is no longer counted as ours"
    );
    assert!(dir.is_dir(), "the directory itself is still the one taken");
    drop(guard);
    assert!(
        !dir.exists(),
        "our own directory is removed on drop even after a failed touch"
    );
}

/// Takes back [`deny_touch`]'s denial.
fn allow_touch(dir: &Path) {
    let status = Command::new("icacls")
        .arg(dir)
        .args(["/remove:d", "*S-1-1-0"])
        .stdout(Stdio::null())
        .status()
        .expect("icacls runs");
    assert!(status.success(), "icacls takes back the denial: {status}");
}

#[test]
fn a_dropped_guards_directory_can_be_recreated_while_another_handle_is_open() {
    let scratch =
        Scratch::empty("a_dropped_guards_directory_can_be_recreated_while_another_handle_is_open");
    let dir = scratch.dir.join(".claude.json.lock");
    let guard =
        DirLock::acquire(&dir, CONFIG_STALENESS, timing(500)).expect("a free lock is taken");
    // Another process (an indexer, antivirus) has the directory open, sharing everything.
    let other = OpenOptions::new()
        .access_mode(READ_ATTRIBUTES)
        .custom_flags(BACKUP_SEMANTICS)
        .open(&dir)
        .expect("the lock directory opens for another reader");
    drop(guard);
    let made = fs::create_dir(&dir);
    drop(other);
    assert!(
        made.is_ok(),
        "Claude Code's next mkdir succeeds while another handle to the removed directory is still open: {made:?}"
    );
}

#[test]
fn the_toucher_does_not_touch_a_directory_that_replaced_ours() {
    let scratch = Scratch::empty("the_toucher_does_not_touch_a_directory_that_replaced_ours");
    let dir = scratch.dir.join(".oauth_refresh.lock");
    let quick = LockTiming::new(Duration::from_secs(1), Duration::from_millis(50));
    let guard = DirLock::acquire(&dir, CREDENTIALS_STALENESS, quick).expect("a free lock is taken");
    take_over(&dir);
    set_dir_mtime(&dir, Duration::from_secs(30));

    // Eight touch intervals: a toucher still touching by path would have refreshed the new directory by now.
    thread::sleep(Duration::from_millis(400));
    assert!(
        mtime_age(&dir) >= Duration::from_secs(29),
        "the other holder's directory keeps the mtime it was given"
    );
    assert!(
        !guard.still_owned(),
        "a lock whose directory was replaced is reported lost"
    );
    drop(guard);
    assert!(
        dir.is_dir(),
        "the other holder's directory is left in place"
    );
}

#[test]
fn a_toucher_that_cannot_touch_until_stale_marks_the_lock_lost() {
    let scratch = Scratch::empty("a_toucher_that_cannot_touch_until_stale_marks_the_lock_lost");
    let dir = scratch.dir.join(".claude.lock");
    let quick = LockTiming::new(Duration::from_secs(1), Duration::from_millis(50));
    let guard =
        DirLock::acquire(&dir, Duration::from_millis(300), quick).expect("a free lock is taken");
    // Opened before the denial, which is checked only at open: the test keeps the mtime fresh through it, so only
    // the toucher giving up can make the lock lost.
    let keeper = OpenOptions::new()
        .access_mode(WRITE_ATTRIBUTES)
        .custom_flags(BACKUP_SEMANTICS)
        .open(&dir)
        .expect("the lock directory opens for its times");
    deny_touch(&dir);

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut lost = false;
    while Instant::now() < deadline {
        keeper
            .set_modified(SystemTime::now())
            .expect("the handle opened before the denial still sets the mtime");
        if !guard.still_owned() {
            lost = true;
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    drop(keeper);
    assert!(
        lost,
        "a toucher that failed to touch until the lock would be stale marks it lost"
    );
    drop(guard);
    assert!(!dir.exists(), "our own directory is removed on drop");
}

#[test]
fn a_transient_touch_failure_retries_instead_of_losing_the_lock() {
    let scratch = Scratch::empty("a_transient_touch_failure_retries_instead_of_losing_the_lock");
    let dir = scratch.dir.join(".oauth_refresh.lock");
    let quick = LockTiming::new(Duration::from_secs(1), Duration::from_millis(50));
    let guard = DirLock::acquire(&dir, CREDENTIALS_STALENESS, quick).expect("a free lock is taken");

    deny_touch(&dir);
    // Ten touch intervals, every touch failing, while the lock stays far from stale.
    thread::sleep(Duration::from_millis(500));
    assert!(
        guard.still_owned(),
        "touches that fail while the lock is fresh do not lose it"
    );

    allow_touch(&dir);
    set_dir_mtime(&dir, Duration::from_secs(30));
    let deadline = Instant::now() + Duration::from_secs(5);
    while mtime_age(&dir) >= Duration::from_secs(5) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        mtime_age(&dir) < Duration::from_secs(5),
        "the toucher kept going and refreshed the lock"
    );
    assert!(
        guard.still_owned(),
        "the lock is still ours once touches succeed again"
    );
    drop(guard);
    assert!(!dir.exists(), "our own lock is removed on drop");
}

#[test]
fn credentials_lock_order() {
    let scratch = Scratch::empty("credentials_lock_order");
    let config_home = scratch.dir.join(".claude");
    let paths = ClaudePaths::new(config_home.clone(), scratch.dir.join(".claude.json"));
    assert_eq!(
        paths.oauth_refresh_lock(),
        config_home.join(".oauth_refresh.lock")
    );
    assert_eq!(paths.credentials_lock(), scratch.dir.join(".claude.lock"));
    assert_eq!(paths.config_lock(), scratch.dir.join(".claude.json.lock"));

    let both =
        ClaudeCodeLocks::credentials_with(&paths, timing(700)).expect("both free locks are taken");
    assert!(paths.oauth_refresh_lock().is_dir() && paths.credentials_lock().is_dir());
    drop(both);
    assert!(!paths.oauth_refresh_lock().exists() && !paths.credentials_lock().exists());

    fs::create_dir(paths.credentials_lock()).expect("Claude Code's legacy lock is held");
    let waiter_paths = paths.clone();
    let waiter = thread::spawn(move || {
        ClaudeCodeLocks::credentials_with(&waiter_paths, timing(1500)).map(drop)
    });
    thread::sleep(Duration::from_millis(500));
    assert!(
        paths.oauth_refresh_lock().is_dir(),
        "the primary lock is taken first, then the legacy one waited on"
    );
    let result = waiter.join().expect("the waiter finishes");
    assert!(
        matches!(result, Err(LockError::Timeout)),
        "a held legacy lock times out: {result:?}"
    );
    assert!(
        !paths.oauth_refresh_lock().exists(),
        "the primary lock is released when the legacy one is not had"
    );
    assert!(
        paths.credentials_lock().is_dir(),
        "the other holder's lock is left alone"
    );

    let config =
        ClaudeCodeLocks::config_with(&paths, timing(500)).expect("the config lock is free");
    assert_eq!(config.path(), paths.config_lock());
}
