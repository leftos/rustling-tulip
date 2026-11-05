//! A scratch account store per test, copied from the hand-written fixture, and the fixture's fake values. The fixture
//! carries no credential file: a DPAPI-sealed file belongs to one Windows user, so the test writes it through [`Store`]
//! in its setup.

use std::fs;
use std::path::{Path, PathBuf};

use claude_accounts::AccountsRoot;
#[cfg(accounts_locks)]
use claude_accounts::{AccountLock, AccountLockGuard, CredentialBlob, Store};

/// Account 1's email in the fixture.
#[cfg(accounts_locks)]
pub const ALICE: &str = "alice@example.com";
/// Account 2's email in the fixture.
#[cfg(accounts_locks)]
pub const BOB: &str = "bob@example.com";
/// The credential text test setup writes for account 1.
#[cfg(accounts_locks)]
pub const FIXTURE_CREDENTIALS: &str = r#"{"claudeAiOauth":{"accessToken":"fake-access","refreshToken":"fake-refresh"},"trustedDeviceToken":"fake-device"}"#;
/// Every fake token string in the fixture: none may ever appear in a `Debug` or an error's text.
#[cfg(accounts_locks)]
pub const FAKE_TOKENS: [&str; 3] = ["fake-access", "fake-refresh", "fake-device"];
/// Account 1's credential file, relative to the store's root.
#[cfg(accounts_locks)]
pub const ALICE_CRED: &str = "credentials/1.cred";
/// The generation account 1's previous credential write is kept as, relative to the store's root.
#[cfg(accounts_locks)]
pub const ALICE_PREV: &str = "credentials/1.cred.prev";

/// A canary that says sealing does not work, for a store that must refuse every write.
#[cfg(accounts_locks)]
pub fn failing_canary() -> bool {
    false
}

/// A folder named after the test, wiped first, with an account store under it and account 1's credentials written
/// through [`Store`].
pub struct Scratch {
    pub dir: PathBuf,
}

impl Scratch {
    /// A scratch folder whose account store does not exist yet.
    pub fn empty(test: &str) -> Self {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("claude-accounts")
            .join(test);
        if dir.exists() {
            fs::remove_dir_all(&dir).expect("the test's old folder can be removed");
        }
        fs::create_dir_all(&dir).expect("the test's folder can be made");
        make_private(&dir);
        #[cfg(unix)]
        {
            // Every file and folder a test makes is owner-only, as the scratch folder's DACL makes them on Windows.
            // SAFETY: umask only swaps the process's file-mode creation mask; it takes no pointer and cannot fail.
            let _previous = unsafe { libc::umask(0o077) };
        }
        // A Unix temp folder can sit under a link (macOS's `/var` is `/private/var`), which the store refuses as a
        // root's parent: the scratch folder is named by its real path.
        #[cfg(unix)]
        let dir = fs::canonicalize(&dir).expect("the test's folder has a real path");
        Self { dir }
    }

    pub fn backup(&self) -> PathBuf {
        self.dir.join("backup")
    }

    pub fn root(&self) -> AccountsRoot {
        AccountsRoot::new(self.backup())
    }
}

#[cfg(accounts_locks)]
impl Scratch {
    /// A scratch folder whose account store is a copy of the fixture, its credentials written in test setup.
    pub fn new(test: &str) -> Self {
        let scratch = Self::empty(test);
        copy_dir(&fixture_root(), &scratch.backup());
        let store = scratch.store();
        let lock = scratch.lock();
        store
            .write_credentials(
                &lock,
                1,
                &CredentialBlob::new(FIXTURE_CREDENTIALS.to_owned()),
            )
            .expect("account 1's fixture credential is written");
        scratch
    }

    pub fn store(&self) -> Store {
        Store::open(self.root()).expect("the scratch store opens")
    }

    /// The same store with a canary that says sealing does not work.
    pub fn store_without_sealing(&self) -> Store {
        Store::open_with_canary(self.root(), failing_canary).expect("the scratch store opens")
    }

    pub fn lock(&self) -> AccountLockGuard {
        AccountLock::acquire(&self.root()).expect("the account lock is free")
    }

    /// A file under the store's root.
    pub fn path(&self, relative: &str) -> PathBuf {
        self.backup().join(relative)
    }

    pub fn write(&self, relative: &str, contents: &str) {
        self.write_bytes(relative, contents.as_bytes());
    }

    pub fn write_bytes(&self, relative: &str, contents: &[u8]) {
        let path = self.path(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("the file's folder can be made");
        }
        fs::write(&path, contents).expect("a scratch file can be written");
    }

    pub fn read(&self, relative: &str) -> Vec<u8> {
        fs::read(self.path(relative)).expect("a scratch file can be read")
    }

    pub fn read_json(&self, relative: &str) -> serde_json::Value {
        serde_json::from_slice(&self.read(relative)).expect("a scratch file is JSON")
    }

    /// Rewrites the JSON file at `relative` with `edit`.
    pub fn edit_json(&self, relative: &str, edit: impl FnOnce(&mut serde_json::Value)) {
        let mut value = self.read_json(relative);
        edit(&mut value);
        self.write(
            relative,
            &serde_json::to_string_pretty(&value).expect("a JSON value serializes"),
        );
    }

    pub fn exists(&self, relative: &str) -> bool {
        self.path(relative).exists()
    }
}

#[cfg(accounts_locks)]
pub fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("store")
        .join("fixtures")
        .join("accounts-root")
}

#[cfg(accounts_locks)]
fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("a scratch folder can be made");
    // On Windows the copy inherits the scratch folder's private DACL; off Windows a made folder is 0755 under umask.
    #[cfg(unix)]
    make_private(to);
    for entry in fs::read_dir(from).expect("a fixture folder can be listed") {
        let entry = entry.expect("a fixture entry can be read");
        let target = to.join(entry.file_name());
        if entry
            .file_type()
            .expect("a fixture entry has a type")
            .is_dir()
        {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("a fixture file can be copied");
            make_file_private(&target);
        }
    }
}

/// Off Windows a copied fixture file keeps the checkout's mode: it is made owner-only (0600), as the store makes its
/// files. On Windows the folder's DACL already covers it.
#[cfg(all(windows, target_pointer_width = "64"))]
fn make_file_private(_file: &Path) {}

#[cfg(unix)]
fn make_file_private(file: &Path) {
    use std::os::unix::fs::PermissionsExt as _;

    fs::set_permissions(file, fs::Permissions::from_mode(0o600))
        .expect("a copied fixture file can be made private");
}

/// Makes `dir` this user's alone: on Windows a DACL with no inherited ACEs and one for this user, off Windows mode
/// 0700. Everything copied under it inherits the first.
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

/// The DACL of `path` holds one ACE, not inherited, naming the current user, as the store makes its root, folders and
/// files. Off Windows: owned by the effective uid, with no group or other permission bit.
#[cfg(unix)]
pub fn assert_owner_only(path: &Path) {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = fs::metadata(path).expect("the path's metadata reads");
    // SAFETY: geteuid takes no arguments, cannot fail and touches no memory of ours.
    let me = unsafe { libc::geteuid() };
    assert_eq!(metadata.uid(), me, "{} is this user's", path.display());
    assert_eq!(
        metadata.mode() & 0o077,
        0,
        "{} has no group or other bit: {:o}",
        path.display(),
        metadata.mode()
    );
}

/// The DACL of `path` holds one ACE, not inherited, naming the current user, as the store makes its root, folders and
/// files.
#[cfg(all(windows, target_pointer_width = "64"))]
pub fn assert_owner_only(path: &Path) {
    let output = std::process::Command::new("icacls")
        .arg(path)
        .output()
        .expect("icacls can be started");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{}: {text}", path.display());
    let more_aces: Vec<&str> = text
        .lines()
        .skip(1)
        .filter(|line| !line.trim().is_empty() && !line.starts_with("Successfully"))
        .collect();
    assert!(
        more_aces.is_empty(),
        "one ACE on {}: {text}",
        path.display()
    );
    assert!(
        !text.contains("(I)"),
        "no inherited ACE on {}: {text}",
        path.display()
    );
    assert!(
        !text.contains("BUILTIN\\")
            && !text.contains("NT AUTHORITY\\")
            && !text.contains("Everyone"),
        "the only ACE names the current user on {}: {text}",
        path.display()
    );
}

/// Off Windows: adds the group write bit to `path`.
#[cfg(unix)]
pub fn grant_users_write(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;

    let mode = fs::metadata(path)
        .expect("the path's metadata reads")
        .permissions()
        .mode();
    fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o020))
        .expect("the group write bit can be added");
}

/// Adds a write grant for the built-in `Users` group to `path`: a principal that is neither this user nor an admin.
#[cfg(all(windows, target_pointer_width = "64"))]
pub fn grant_users_write(path: &Path) {
    let output = std::process::Command::new("icacls")
        .arg(path)
        .args(["/grant", "*S-1-5-32-545:(W)"])
        .output()
        .expect("icacls can be started");
    assert!(
        output.status.success(),
        "icacls /grant Users:(W) on {} failed: {}",
        path.display(),
        String::from_utf8_lossy(&output.stdout)
    );
}
