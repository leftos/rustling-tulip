//! A scratch account store per test, copied from the usage fixture, and the fixture's fake values.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use claude_accounts::ClaudePaths;
use claude_accounts::{AccountsRoot, Identity, Store};

/// Account 1's email and organization in the fixture.
pub const ONE: &str = "one@example.test";
pub const ONE_ORG: &str = "00000000-0000-4000-8000-0000000000b1";
/// Account 2's email in the fixture; it has no organization.
pub const TWO: &str = "two@example.test";
/// When the fixture's account 1 was last fetched, in seconds since the epoch.
pub const FETCHED_AT: f64 = 1_790_000_000.0;
/// Every fake token string in the fixture: none may ever appear in a `Debug`.
pub const FAKE_TOKENS: [&str; 5] = [
    "fake-access-one",
    "fake-refresh-one",
    "fake-device-one",
    "fake-access-two",
    "fake-refresh-two",
];

/// A folder named after the test, wiped first, with a copy of the fixture's account store under it.
pub struct Scratch {
    pub dir: PathBuf,
}

impl Scratch {
    pub fn new(test: &str) -> Self {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("claude-accounts-usage")
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
        let scratch = Self { dir };
        copy_dir(&fixture_root(), &scratch.backup());
        let store = scratch.store();
        let lock = claude_accounts::AccountLock::acquire(&scratch.root())
            .expect("the account lock is free");
        for (number, text) in [(1, fixture_credential_one()), (2, fixture_credential_two())] {
            store
                .write_credentials(&lock, number, &claude_accounts::CredentialBlob::new(text))
                .expect("the fixture's credential is written");
        }
        scratch
    }

    pub fn backup(&self) -> PathBuf {
        self.dir.join("backup")
    }

    pub fn root(&self) -> AccountsRoot {
        AccountsRoot::new(self.backup())
    }

    pub fn store(&self) -> Store {
        Store::open(self.root()).expect("the scratch store opens")
    }

    pub fn write(&self, relative: &str, contents: &str) {
        let path = self.backup().join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("the file's folder can be made");
        }
        fs::write(path, contents).expect("a scratch file can be written");
    }

    /// Claude Code's files for this test: its config home is `claude/` beside the account store, empty until a test
    /// writes into it.
    pub fn claude(&self) -> ClaudePaths {
        ClaudePaths::new(self.dir.join("claude"), self.dir.join(".claude.json"))
    }

    /// Writes Claude Code's live credential file, `claude/.credentials.json`.
    pub fn write_live_credentials(&self, contents: &str) {
        let config_home = self.dir.join("claude");
        fs::create_dir_all(&config_home).expect("the config home can be made");
        fs::write(config_home.join(".credentials.json"), contents)
            .expect("the live credential file can be written");
    }

    /// Writes Claude Code's global config, `.claude.json` beside the account store.
    pub fn write_claude_json(&self, contents: &str) {
        fs::write(self.dir.join(".claude.json"), contents)
            .expect("the global config can be written");
    }

    pub fn read_json(&self, relative: &str) -> serde_json::Value {
        let bytes = fs::read(self.backup().join(relative)).expect("a scratch file can be read");
        serde_json::from_slice(&bytes).expect("a scratch file is JSON")
    }

    /// The `cache/usage.json` row of account `number`.
    pub fn usage_row(&self, number: u32) -> serde_json::Value {
        self.read_json("cache/usage.json")["accounts"][number.to_string()].clone()
    }
}

/// The fixture's two accounts as the usage table identifies them.
pub fn identities() -> BTreeMap<u32, Identity> {
    BTreeMap::from([
        (1, Identity::new(ONE, ONE_ORG)),
        (2, Identity::new(TWO, "")),
    ])
}

/// A timestamp `seconds` after the fixture's account 1 fetch.
pub fn after_fetch(seconds: i64) -> jiff::Timestamp {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the fixture's epoch is a whole number of seconds"
    )]
    let fetched = FETCHED_AT as i64;
    jiff::Timestamp::from_second(fetched + seconds).expect("the fixture's times are in range")
}

/// A timestamp as the store writes it: seconds since the epoch.
pub fn epoch_seconds(timestamp: jiff::Timestamp) -> f64 {
    timestamp.as_duration().as_secs_f64()
}

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("usage")
        .join("fixtures")
        .join("accounts-root")
}

/// Account 1's fixture credential: the same fake tokens the committed fixture held, written in test setup because a
/// DPAPI-sealed file cannot be committed.
pub fn fixture_credential_one() -> String {
    r#"{"claudeAiOauth":{"accessToken":"fake-access-one","refreshToken":"fake-refresh-one","expiresAt":4102444800000,"refreshTokenExpiresAt":4133980800000,"scopes":["user:inference","user:profile"],"subscriptionType":"max"},"trustedDeviceToken":"fake-device-one"}"#.to_owned()
}

/// Account 2's fixture credential, as [`fixture_credential_one`].
pub fn fixture_credential_two() -> String {
    r#"{"claudeAiOauth":{"accessToken":"fake-access-two","refreshToken":"fake-refresh-two","expiresAt":4102444800000,"scopes":["user:inference","user:profile"],"subscriptionType":"pro"}}"#.to_owned()
}

/// Makes `dir` this user's alone: a DACL with no inherited ACEs and one for this user.
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

/// Off Windows: mode 0700.
#[cfg(unix)]
fn make_private(dir: &Path) {
    use std::os::unix::fs::PermissionsExt as _;

    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        .expect("the test's folder can be made private");
}

/// Off Windows a copied fixture file keeps the checkout's mode: it is made owner-only (0600), as the store makes its
/// files. On Windows the folder's DACL already covers it.
#[cfg(windows)]
fn make_file_private(_file: &Path) {}

#[cfg(unix)]
fn make_file_private(file: &Path) {
    use std::os::unix::fs::PermissionsExt as _;

    fs::set_permissions(file, fs::Permissions::from_mode(0o600))
        .expect("a copied fixture file can be made private");
}

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
