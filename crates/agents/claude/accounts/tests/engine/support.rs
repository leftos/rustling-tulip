//! A scratch account store per test and the tests' fixed clock.

use std::fs;
use std::path::{Path, PathBuf};

use claude_accounts::AccountsRoot;

/// The clock every test runs at, in seconds since the epoch: 2026-09-21 14:13:20 UTC.
pub const NOW_S: i64 = 1_790_000_000;

/// The tests' clock, moved `offset_ms` milliseconds from [`NOW_S`].
pub fn at(offset_ms: i64) -> jiff::Timestamp {
    jiff::Timestamp::from_millisecond(NOW_S * 1000 + offset_ms).expect("the test clock is in range")
}

/// A folder named after the test, wiped first, with an empty account store under it.
pub struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    pub fn new(test: &str) -> Self {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("claude-accounts-engine")
            .join(test);
        if dir.exists() {
            fs::remove_dir_all(&dir).expect("the test's old folder can be removed");
        }
        fs::create_dir_all(&dir).expect("the test's folder can be made");
        make_private(&dir);
        private_umask();
        // A Unix temp folder can sit under a link (macOS's `/var` is `/private/var`), which the store refuses as a
        // root's parent: the scratch folder is named by its real path.
        #[cfg(unix)]
        let dir = fs::canonicalize(&dir).expect("the test's folder has a real path");
        fs::create_dir(dir.join("backup")).expect("the test's account store can be made");
        make_private(&dir.join("backup"));
        Self { dir }
    }

    pub fn root(&self) -> AccountsRoot {
        AccountsRoot::new(self.dir.join("backup"))
    }
}

/// Off Windows every file and folder a test makes is owner-only (umask 077), as the scratch folder's DACL makes them on
/// Windows: the store refuses a file another user can read.
#[cfg(unix)]
fn private_umask() {
    // SAFETY: umask only swaps the process's file-mode creation mask; it takes no pointer and cannot fail.
    let _previous = unsafe { libc::umask(0o077) };
}

#[cfg(not(unix))]
fn private_umask() {}

/// Makes `dir` this user's alone: on Windows a DACL with no inherited ACEs and one for this user, off Windows mode
/// 0700.
#[cfg(windows)]
fn make_private(dir: &Path) {
    let whoami = std::process::Command::new("whoami")
        .output()
        .expect("whoami can be started");
    let user = String::from_utf8(whoami.stdout).expect("whoami prints text");
    let output = std::process::Command::new("icacls")
        .arg(dir)
        .args([
            "/inheritance:r",
            "/grant:r",
            &format!("{}:(OI)(CI)F", user.trim()),
        ])
        .output()
        .expect("icacls can be started");
    assert!(
        output.status.success(),
        "icacls failed: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[cfg(unix)]
fn make_private(dir: &Path) {
    use std::os::unix::fs::PermissionsExt as _;

    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        .expect("the test's folder can be made private");
}

#[cfg(not(any(unix, windows)))]
fn make_private(_dir: &Path) {}

pub fn write(path: &Path, contents: &str) {
    fs::write(path, contents).expect("a scratch file can be written");
}

pub fn read_json(path: &Path) -> serde_json::Value {
    let bytes = fs::read(path).expect("a scratch file can be read");
    serde_json::from_slice(&bytes).expect("a scratch file is JSON")
}
