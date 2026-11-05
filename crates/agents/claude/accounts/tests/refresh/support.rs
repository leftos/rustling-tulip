//! Fakes for the refresh tests: a token endpoint that records each POST and answers from a queue, a fixed clock, fake
//! credentials, and (on 64-bit Windows, where the locks work) a session table and a scratch account store copied from the
//! usage fixture.

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard};

use claude_accounts::{RawToken, RefreshRequest, RefreshTransport, TransportError};
use serde_json::{Value, json};

#[cfg(accounts_locks)]
pub use disk::*;
#[cfg(accounts_locks)]
pub use oracle::*;

/// The clock every test runs at, in seconds since the epoch: 2026-09-21 14:13:20 UTC.
pub const NOW_S: i64 = 1_790_000_000;

/// The tests' clock: always [`NOW_S`].
pub fn now() -> jiff::Timestamp {
    jiff::Timestamp::from_second(NOW_S).expect("the test clock is in range")
}

/// Milliseconds since the epoch, `seconds` after [`now`].
pub fn ms_after(seconds: i64) -> i64 {
    (NOW_S + seconds) * 1000
}

/// A fake credential file as Claude Code writes it, with keys the refresh must keep: `mcpOAuth` beside the OAuth
/// block and `subscriptionType` inside it.
pub fn credential(access: &str, refresh: Option<&str>, expires_at_ms: i64) -> String {
    let mut oauth = json!({
        "accessToken": access,
        "expiresAt": expires_at_ms,
        "scopes": ["user:inference"],
        "subscriptionType": "max",
    });
    if let Some(refresh) = refresh {
        oauth["refreshToken"] = json!(refresh);
    }
    json!({ "claudeAiOauth": oauth, "mcpOAuth": { "keep": "me" } }).to_string()
}

/// A 200 from the token endpoint granting `access`, optionally a new refresh token, for `expires_in` seconds.
#[expect(
    clippy::unnecessary_wraps,
    reason = "the fake endpoint queues results, so a canned answer is one"
)]
pub fn granted(
    access: &str,
    refresh: Option<&str>,
    expires_in: i64,
) -> Result<RawToken, TransportError> {
    let mut body = json!({
        "access_token": access,
        "expires_in": expires_in,
        "scope": "user:inference user:profile",
        "token_type": "Bearer",
    });
    if let Some(refresh) = refresh {
        body["refresh_token"] = json!(refresh);
    }
    Ok(RawToken { status: 200, body })
}

/// `granted` with the token answer's `account` and `organization` objects set, as the endpoint names whose token it is.
pub fn with_account(
    granted: Result<RawToken, TransportError>,
    account: Value,
    organization: Value,
) -> Result<RawToken, TransportError> {
    granted.map(|mut token| {
        token.body["account"] = account;
        token.body["organization"] = organization;
        token
    })
}

/// An answer from the token endpoint with `status` and `body`.
#[expect(
    clippy::unnecessary_wraps,
    reason = "the fake endpoint queues results, so a canned answer is one"
)]
pub fn answer(status: u16, body: Value) -> Result<RawToken, TransportError> {
    Ok(RawToken { status, body })
}

type Hook = Box<dyn FnOnce() + Send>;

/// A token endpoint that records each POST's body and answers from a queue, `Connect` once it runs dry. A hook can
/// run inside the first POST, to change the store or block while the grant is in flight.
pub struct FakeEndpoint {
    answers: Mutex<VecDeque<Result<RawToken, TransportError>>>,
    bodies: Mutex<Vec<String>>,
    hook: Mutex<Option<Hook>>,
}

impl FakeEndpoint {
    pub fn answering(answers: Vec<Result<RawToken, TransportError>>) -> Self {
        Self {
            answers: Mutex::new(answers.into()),
            bodies: Mutex::new(Vec::new()),
            hook: Mutex::new(None),
        }
    }

    /// Runs `hook` inside the first POST, before it answers.
    #[cfg(accounts_locks)]
    pub fn during_post(self, hook: impl FnOnce() + Send + 'static) -> Self {
        *lock(&self.hook) = Some(Box::new(hook));
        self
    }

    /// Each POST's JSON body, verbatim.
    pub fn bodies(&self) -> Vec<String> {
        lock(&self.bodies).clone()
    }

    /// The fake refresh token each POST carried.
    #[cfg(accounts_locks)]
    pub fn posted_refresh_tokens(&self) -> Vec<String> {
        self.bodies()
            .iter()
            .map(|body| {
                let body: Value = serde_json::from_str(body).expect("a POST body is JSON");
                body["refresh_token"]
                    .as_str()
                    .expect("a POST body carries a refresh token")
                    .to_owned()
            })
            .collect()
    }
}

impl RefreshTransport for FakeEndpoint {
    fn post_token(&self, request: &RefreshRequest) -> Result<RawToken, TransportError> {
        lock(&self.bodies).push(request.json_body().expose().to_owned());
        let hook = lock(&self.hook).take();
        if let Some(hook) = hook {
            hook();
        }
        lock(&self.answers)
            .pop_front()
            .unwrap_or(Err(TransportError::Connect))
    }
}

/// The profile endpoint's fake, used only by the refreshing polls, which run where the locks work.
#[cfg(accounts_locks)]
mod oracle {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use claude_accounts::{AccessToken, ProfileTransport, RawProfile, TransportError};
    use serde_json::json;

    use super::lock;

    /// A profile endpoint that answers from a queue, `Connect` once it runs dry, and remembers the tokens it was asked
    /// about. They are fake tokens, so the tests compare them directly.
    pub struct FakeProfile {
        answers: Mutex<VecDeque<Result<RawProfile, TransportError>>>,
        asked: Mutex<Vec<String>>,
    }

    impl FakeProfile {
        pub fn answering(answers: Vec<Result<RawProfile, TransportError>>) -> Self {
            Self {
                answers: Mutex::new(answers.into()),
                asked: Mutex::new(Vec::new()),
            }
        }

        /// The access tokens the oracle was asked about, in order.
        pub fn asked(&self) -> Vec<String> {
            lock(&self.asked).clone()
        }
    }

    impl ProfileTransport for FakeProfile {
        fn get_profile(&self, access: &AccessToken) -> Result<RawProfile, TransportError> {
            lock(&self.asked).push(access.expose().to_owned());
            lock(&self.answers)
                .pop_front()
                .unwrap_or(Err(TransportError::Connect))
        }
    }

    /// A 200 from the profile endpoint naming account `uuid` with `email` in organization `org_uuid`.
    #[expect(
        clippy::unnecessary_wraps,
        reason = "the fake endpoint queues results, so a canned answer is one"
    )]
    pub fn profile(uuid: &str, email: &str, org_uuid: &str) -> Result<RawProfile, TransportError> {
        Ok(RawProfile {
            status: 200,
            body: json!({ "account": { "uuid": uuid, "email": email }, "organization": { "uuid": org_uuid } }),
        })
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .expect("no test thread panics holding the fake's lock")
}

#[cfg(accounts_locks)]
mod disk {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use claude_accounts::seal::{self, AssociatedData, SealPurpose};
    use claude_accounts::{
        AccountLock, AccountsRoot, ClaudePaths, CredentialBlob, Drain, Email, GrantOutcome,
        LockTiming, ProbeVerdicts, RefreshContext, Store, consume_backup_grant, guard,
    };
    use serde_json::{Value, json};

    use super::{FakeEndpoint, FakeProfile, now};

    /// Account 1's email and organization in the usage fixture.
    pub const ONE: &str = "one@example.test";
    pub const ONE_ORG: &str = "00000000-0000-4000-8000-0000000000b1";

    /// A folder named after the test, wiped first, with a copy of the usage fixture's account store under it.
    pub struct Scratch {
        pub dir: PathBuf,
    }

    impl Scratch {
        pub fn new(test: &str) -> Self {
            let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
                .join("claude-accounts-refresh")
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
            let scratch = Self { dir };
            copy_dir(&fixture_root(), &scratch.backup());
            restrict(&scratch.backup());
            put_credentials(&scratch.root(), &fixture_credential_one());
            write_credential(&scratch.root(), 2, &fixture_credential_two());
            scratch
        }

        pub fn backup(&self) -> PathBuf {
            self.dir.join("backup")
        }

        pub fn root(&self) -> AccountsRoot {
            AccountsRoot::new(self.backup())
        }

        /// Claude Code's files for the test: a `claude` folder and a `.claude.json` beside the account store, neither
        /// written unless the test writes it.
        pub fn claude(&self) -> ClaudePaths {
            ClaudePaths::new(self.dir.join("claude"), self.dir.join(".claude.json"))
        }

        /// Makes account `number` `accounts.json`'s active account. The fixture's is account 1, which the gate refuses.
        pub fn set_active(&self, number: u32) {
            let path = self.backup().join("accounts.json");
            let mut sequence: Value =
                serde_json::from_slice(&fs::read(&path).expect("accounts.json can be read"))
                    .expect("it is JSON");
            sequence["active"] = json!(number);
            fs::write(&path, sequence.to_string()).expect("accounts.json can be written");
        }

        /// Sets `key` of account `number`'s record in `accounts.json` to `value`.
        pub fn set_account(&self, number: u32, key: &str, value: Value) {
            let path = self.backup().join("accounts.json");
            let mut sequence: Value =
                serde_json::from_slice(&fs::read(&path).expect("accounts.json can be read"))
                    .expect("it is JSON");
            sequence["accounts"][number.to_string()][key] = value;
            fs::write(&path, sequence.to_string()).expect("accounts.json can be written");
        }

        /// Account 1's sealed credential file, `credentials/1.cred`.
        pub fn credential_file(&self) -> PathBuf {
            self.root().credential_path(1)
        }

        pub fn manifest_path(&self) -> PathBuf {
            self.backup()
                .join("credentials")
                .join(".unclaimed-manifest.json")
        }

        pub fn manifest(&self) -> Value {
            let bytes = fs::read(self.manifest_path()).expect("the stash manifest can be read");
            serde_json::from_slice(&bytes).expect("the stash manifest is JSON")
        }

        /// The stash entry's file.
        pub fn stash_path(&self, id: &str) -> PathBuf {
            self.backup()
                .join("credentials")
                .join(format!(".unclaimed-{id}.cred"))
        }

        /// The stash entry's bytes, as written.
        pub fn stash_bytes(&self, id: &str) -> Option<Vec<u8>> {
            fs::read(self.stash_path(id)).ok()
        }

        /// The stash entry's credential text, opened under the slot and organization its manifest row names (none for
        /// a row that records none).
        pub fn stash_entry(&self, id: &str) -> Option<String> {
            let row = self.manifest()["entries"].get(id)?.clone();
            let slot = row["configSlot"]
                .as_str()
                .expect("a row names its config slot");
            let sealed = guard::read_file(&self.backup(), &self.stash_path(id)).ok()?;
            let organization = row["organization"].as_str().unwrap_or_default();
            let ad = AssociatedData::new(slot, organization);
            seal::open(SealPurpose::StashEntry, &ad, &sealed)
                .ok()
                .map(|blob| blob.expose().to_owned())
        }

        /// Writes a stash entry sealed for account 1 and its organization.
        pub fn write_stash_entry(&self, id: &str, credentials: &str) {
            self.write_stash_entry_for(id, credentials, "1", ONE_ORG);
        }

        /// Writes a stash entry sealed for `slot` and `organization`, as the stash writes it.
        pub fn write_stash_entry_for(
            &self,
            id: &str,
            credentials: &str,
            slot: &str,
            organization: &str,
        ) {
            let sealed = seal::seal(
                SealPurpose::StashEntry,
                &AssociatedData::new(slot, organization),
                &CredentialBlob::new(credentials.to_owned()),
            )
            .expect("a stash entry can be sealed");
            self.write_stash_raw(id, sealed.as_bytes());
        }

        /// Writes raw bytes where a stash entry goes, as an older format or a damaged file would.
        pub fn write_stash_raw(&self, id: &str, bytes: &[u8]) {
            fs::write(self.stash_path(id), bytes).expect("a stash entry can be written");
        }
    }

    /// Account 1's email.
    pub fn email() -> Email {
        Email::parse(ONE).expect("the fixture's email is safe")
    }

    /// Replaces account `number`'s stored credential under the account lock.
    pub fn write_credential(root: &AccountsRoot, number: u32, text: &str) {
        let store = Store::open(root.clone()).expect("the scratch store opens");
        let lock = AccountLock::acquire(root).expect("the account lock is free");
        store
            .write_credentials(&lock, number, &CredentialBlob::new(text.to_owned()))
            .expect("the credential can be written");
    }

    /// Replaces account 1's stored credential under the account lock.
    pub fn put_credentials(root: &AccountsRoot, text: &str) {
        write_credential(root, 1, text);
    }

    /// Account 1's fixture credential: the fake tokens the committed fixture held, written in test setup because a
    /// DPAPI-sealed file cannot be committed.
    pub fn fixture_credential_one() -> String {
        r#"{"claudeAiOauth":{"accessToken":"fake-access-one","refreshToken":"fake-refresh-one","expiresAt":4102444800000,"refreshTokenExpiresAt":4133980800000,"scopes":["user:inference","user:profile"],"subscriptionType":"max"},"trustedDeviceToken":"fake-device-one"}"#.to_owned()
    }

    /// Account 2's fixture credential, as [`fixture_credential_one`].
    pub fn fixture_credential_two() -> String {
        r#"{"claudeAiOauth":{"accessToken":"fake-access-two","refreshToken":"fake-refresh-two","expiresAt":4102444800000,"scopes":["user:inference","user:profile"],"subscriptionType":"pro"}}"#.to_owned()
    }

    /// Account 1's stored credential text, `None` when absent.
    pub fn stored(root: &AccountsRoot) -> Option<String> {
        let store = Store::open(root.clone()).expect("the scratch store opens");
        store
            .read_credentials(1)
            .expect("the stored credential can be read")
            .map(|blob| blob.expose().to_owned())
    }

    /// Everything one consume needs: the scratch root, its store, the fake endpoint, and the drain.
    pub struct Rig {
        pub scratch: Scratch,
        pub store: Store,
        pub claude: ClaudePaths,
        pub endpoint: FakeEndpoint,
        pub drain: Arc<Drain>,
        pub securestorage_dir_set: bool,
        pub profile: FakeProfile,
        pub verdicts: ProbeVerdicts,
        pub claude_locks: LockTiming,
    }

    impl Rig {
        pub fn new(scratch: Scratch, endpoint: FakeEndpoint) -> Self {
            let store = Store::open(scratch.root()).expect("the scratch store opens");
            let claude = scratch.claude();
            Self {
                scratch,
                store,
                claude,
                endpoint,
                drain: Arc::new(Drain::new()),
                securestorage_dir_set: false,
                profile: FakeProfile::answering(Vec::new()),
                verdicts: ProbeVerdicts::new(),
                claude_locks: LockTiming::CLAUDE,
            }
        }

        /// The refresh context over the rig's fakes, at the tests' clock.
        pub fn context(&self) -> RefreshContext<'_> {
            RefreshContext {
                store: &self.store,
                claude: &self.claude,
                transport: &self.endpoint,
                drain: &self.drain,
                securestorage_dir_set: self.securestorage_dir_set,
                clock: &now,
                profile: &self.profile,
                verdicts: &self.verdicts,
                claude_locks: self.claude_locks,
            }
        }

        /// Spends account 1's grant through the gate, the caller holding `snapshot`.
        pub fn consume(&self, snapshot: &str) -> GrantOutcome {
            consume_backup_grant(
                &self.context(),
                1,
                &CredentialBlob::new(snapshot.to_owned()),
            )
        }

        pub fn stored(&self) -> Option<String> {
            stored(&self.scratch.root())
        }
    }

    /// The JSON text's `claudeAiOauth.accessToken`.
    pub fn access_of(text: &str) -> String {
        let value: Value = serde_json::from_str(text).expect("a credential is JSON");
        value["claudeAiOauth"]["accessToken"]
            .as_str()
            .expect("a credential has an access token")
            .to_owned()
    }

    fn fixture_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("usage")
            .join("fixtures")
            .join("accounts-root")
    }

    /// Makes `dir` this user's alone: on Windows a DACL with no inherited ACEs and one for this user, off Windows
    /// mode 0700.
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

    #[cfg(unix)]
    fn restrict(root: &Path) {
        use std::os::unix::fs::PermissionsExt as _;

        let _set = fs::set_permissions(root, fs::Permissions::from_mode(0o700));
        if let Ok(entries) = fs::read_dir(root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let _set = fs::set_permissions(&path, fs::Permissions::from_mode(0o700));
                    restrict(&path);
                } else {
                    let _set = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
                }
            }
        }
    }

    #[cfg(not(unix))]
    fn restrict(_root: &Path) {}

    /// Off Windows every file and folder a test makes is owner-only (umask 077), as the scratch folder's DACL makes
    /// them on Windows: the store refuses a file another user can read.
    #[cfg(unix)]
    fn private_umask() {
        // SAFETY: umask only swaps the process's file-mode creation mask; it takes no pointer and cannot fail.
        let _previous = unsafe { libc::umask(0o077) };
    }

    #[cfg(not(unix))]
    fn private_umask() {}

    fn copy_dir(from: &Path, to: &Path) {
        fs::create_dir_all(to).expect("a scratch folder can be made");
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
                fs::copy(entry.path(), target).expect("a fixture file can be copied");
            }
        }
    }
}
