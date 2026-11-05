//! Polling with refresh for the account Claude Code is logged in as: its live credential is refreshed under Claude
//! Code's own locks, a rotation Claude Code made is adopted without a POST, and a server-accepted lineage the profile
//! oracle attributes to the account is resynced into the backup.

use std::collections::BTreeSet;
#[cfg(windows)]
use std::fs::OpenOptions;
use std::fs::{self, File};
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use claude_accounts::AutoSwitchSettings;
use claude_accounts::seal::{self, AssociatedData, SealPurpose};
use claude_accounts::{
    AccessToken, AccountLock, CredentialBlob, DeferReason, FetchError, FetchMode, FetchScope,
    LockTiming, PollError, PollOutcome, PollResult, RawUsage, StashReason, Store, StoreErrorKind,
    TransportError, UsageTransport, credential_fingerprint, poll_once_with_refresh,
    poll_with_refresh, refresh_grant,
};
use serde_json::{Value, json};

use crate::support::{
    FakeEndpoint, FakeProfile, ONE, ONE_ORG, Rig, Scratch, access_of, answer, credential, granted,
    ms_after, now, profile,
};
#[cfg(windows)]
use crate::windows_files::temp_files;
#[cfg(windows)]
use crate::windows_files::{hold_open, pin_first_temp};

/// Account 1's uuid in the usage fixture.
const ONE_UUID: &str = "00000000-0000-4000-8000-000000000011";

/// A file a test wrote directly inherits its folder's owner-only DACL on Windows; off Windows it needs 0600.
fn private_file(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _set = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    let _ = path;
}
const TWO: &str = "two@example.test";
/// `FILE_FLAG_BACKUP_SEMANTICS` and `FILE_WRITE_ATTRIBUTES`: enough to open a directory and set its times.
#[cfg(windows)]
const BACKUP_SEMANTICS: u32 = 0x0200_0000;
#[cfg(windows)]
const WRITE_ATTRIBUTES: u32 = 0x0100;
/// How long the rig waits for each of Claude Code's locks.
const LOCK_WAIT: Duration = Duration::from_millis(700);
/// The hang guard for a test whose refresh only proceeds once another thread releases a held lock: a passing run
/// never waits it out, so its size is free.
const HANG_GUARD: Duration = Duration::from_secs(120);

/// A usage endpoint that answers 401 to the tokens in `refused` and a reading to any other, and remembers the tokens
/// it was sent. They are fake tokens, so the test compares them directly.
struct Usage {
    refused: Vec<&'static str>,
    sent: Mutex<Vec<String>>,
    /// The prepaid-credits endpoint's status: 200 answers a funded balance.
    prepaid_status: u16,
    /// Each balance request's token and organization UUID.
    prepaid_sent: Mutex<Vec<(String, String)>>,
}

impl Usage {
    fn refusing(refused: &[&'static str]) -> Self {
        Self {
            refused: refused.to_vec(),
            sent: Mutex::new(Vec::new()),
            prepaid_status: 200,
            prepaid_sent: Mutex::new(Vec::new()),
        }
    }

    /// The same endpoint, its balance request answering `status`.
    fn prepaid_answering(self, status: u16) -> Self {
        Self {
            prepaid_status: status,
            ..self
        }
    }

    fn prepaid_sent(&self) -> Vec<(String, String)> {
        self.prepaid_sent
            .lock()
            .expect("no test thread panics holding the fake's lock")
            .clone()
    }

    /// The tokens sent for account 1: every fake token but account 2's.
    fn sent_for_one(&self) -> Vec<String> {
        let sent = self
            .sent
            .lock()
            .expect("no test thread panics holding the fake's lock");
        sent.iter()
            .filter(|token| !token.contains("two"))
            .cloned()
            .collect()
    }
}

impl UsageTransport for Usage {
    fn get_usage(&self, token: &AccessToken) -> Result<RawUsage, TransportError> {
        let token = token.expose().to_owned();
        let refused = self.refused.contains(&token.as_str());
        self.sent
            .lock()
            .expect("no test thread panics holding the fake's lock")
            .push(token);
        if refused {
            return Ok(RawUsage {
                status: 401,
                body: Value::Null,
                retry_after: None,
            });
        }
        let body =
            json!({ "five_hour": { "utilization": 10.0 }, "seven_day": { "utilization": 20.0 } });
        Ok(RawUsage {
            status: 200,
            body,
            retry_after: None,
        })
    }

    fn get_prepaid(&self, token: &AccessToken, org_uuid: &str) -> Result<RawUsage, TransportError> {
        self.prepaid_sent
            .lock()
            .expect("no test thread panics holding the fake's lock")
            .push((token.expose().to_owned(), org_uuid.to_owned()));
        let body = if self.prepaid_status == 200 {
            json!({ "amount": 102_003, "currency": "USD", "balance": { "credits": { "amount_minor": 102_003, "exponent": 2 } } })
        } else {
            Value::Null
        };
        Ok(RawUsage {
            status: self.prepaid_status,
            body,
            retry_after: None,
        })
    }
}

fn expired(access: &str, refresh: &str) -> String {
    credential(access, Some(refresh), ms_after(-3600))
}

fn fresh(access: &str, refresh: &str) -> String {
    credential(access, Some(refresh), ms_after(3600))
}

/// `~/.claude.json` naming account 1.
fn logged_in_as_one() -> Value {
    json!({ "oauthAccount": { "emailAddress": ONE, "organizationUuid": ONE_ORG } })
}

/// A rig where Claude Code is logged in as account 1 (`config`), whose live file holds `live` and whose backup holds
/// `backup`; account 2 holds a fresh token, and the usage table starts empty, so every account is due. Claude Code's
/// locks are waited for [`LOCK_WAIT`].
fn rig(test: &str, live: &str, backup: &str, config: Option<&Value>) -> Rig {
    let scratch = Scratch::new(test);
    fs::remove_file(scratch.backup().join("cache").join("usage.json"))
        .expect("the fixture's usage table can be removed");
    crate::support::put_credentials(&scratch.root(), backup);
    let store = Store::open(scratch.root()).expect("the scratch store opens");
    let lock = AccountLock::acquire(&scratch.root()).expect("the account lock is free");
    let fresh_two = fresh("fake-access-two", "fake-refresh-two");
    store
        .write_credentials(&lock, 2, &CredentialBlob::new(fresh_two))
        .expect("account 2's credential can be written");
    drop(lock);
    if let Some(config) = config {
        fs::write(scratch.dir.join(".claude.json"), config.to_string())
            .expect("the global config can be written");
    }
    let claude = scratch.claude();
    fs::create_dir_all(scratch.dir.join("claude")).expect("Claude Code's folder can be made");
    fs::write(claude.credentials_file(), live).expect("the live credential can be written");
    let mut rig = Rig::new(
        scratch,
        FakeEndpoint::answering(vec![granted(
            "fake-access-new",
            Some("fake-refresh-new"),
            3600,
        )]),
    );
    rig.claude_locks = wait(LOCK_WAIT);
    rig
}

fn wait(timeout: Duration) -> LockTiming {
    LockTiming::new(timeout, Duration::from_secs(3))
}

/// Polls once with refresh and returns account 1's outcome.
fn poll(rig: &Rig, usage: &Usage) -> PollOutcome {
    let outcomes = poll_once_with_refresh(usage, &AutoSwitchSettings::default(), &rig.context())
        .expect("the poll runs");
    outcomes
        .into_iter()
        .find(|outcome| outcome.number == 1)
        .expect("account 1 has an outcome")
}

/// Empties the usage table again, so the next poll fetches every account.
fn make_due(rig: &Rig) {
    fs::remove_file(rig.scratch.backup().join("cache").join("usage.json"))
        .expect("the usage table can be removed");
}

fn live(rig: &Rig) -> String {
    fs::read_to_string(rig.claude.credentials_file()).expect("the live credential can be read")
}

fn write_live(rig: &Rig, text: &str) {
    fs::write(rig.claude.credentials_file(), text).expect("the live credential can be written");
}

fn backup(rig: &Rig) -> String {
    rig.stored().expect("account 1 holds a backup")
}

/// Claude Code's legacy credential lock, `~/.claude.lock`, made as a live holder would hold it.
fn hold_lock(rig: &Rig) -> PathBuf {
    let path = rig.claude.credentials_lock();
    fs::create_dir(&path).expect("the lock directory is made");
    path
}

/// Holds Claude Code's legacy credential lock, then, once the poller holds the primary lock `.oauth_refresh.lock` (so it
/// is waiting on the held one), runs `meanwhile` and releases the held lock.
fn release_later(rig: &Rig, meanwhile: impl FnOnce() + Send + 'static) -> JoinHandle<()> {
    let held = hold_lock(rig);
    let primary = rig.claude.oauth_refresh_lock();
    thread::spawn(move || {
        wait_for(&primary);
        meanwhile();
        fs::remove_dir(&held).expect("the held lock is released");
    })
}

/// Spins until `path` exists, until the hang guard fires.
fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(120);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "hung: {} never appeared",
            path.display()
        );
        thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(unix)]
fn set_dir_mtime(dir: &Path, ago: Duration) {
    File::open(dir)
        .expect("the lock directory opens for its times")
        .set_modified(SystemTime::now() - ago)
        .expect("the lock directory's mtime can be set");
}

#[cfg(windows)]
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

fn modified(path: &Path) -> SystemTime {
    fs::metadata(path)
        .expect("the path exists")
        .modified()
        .expect("the mtime reads")
}

fn fingerprint(text: &str) -> Option<String> {
    credential_fingerprint(&CredentialBlob::new(text.to_owned()))
}

/// `text` with its `mcpOAuth` replaced by `mcp`.
fn with_mcp(text: &str, mcp: &Value) -> String {
    let mut value: Value = serde_json::from_str(text).expect("a credential is JSON");
    value["mcpOAuth"] = mcp.clone();
    value.to_string()
}

/// The stash manifest's one row: its id and its fields.
fn only_stash_row(rig: &Rig) -> (String, Value) {
    let manifest = rig.scratch.manifest();
    let entries = manifest["entries"]
        .as_object()
        .expect("the manifest has entries");
    assert_eq!(entries.len(), 1, "one credential was stashed");
    let (id, row) = entries.iter().next().expect("one row");
    (id.clone(), row.clone())
}

/// How many rows the stash manifest holds; none when it was never written.
fn stash_rows(rig: &Rig) -> usize {
    if !rig.scratch.manifest_path().exists() {
        return 0;
    }
    rig.scratch.manifest()["entries"]
        .as_object()
        .map_or(0, serde_json::Map::len)
}

#[cfg(windows)]
/// Answers the rig's one POST with a successor while `paths` are opened with no sharing, so writes to them fail until
/// the returned handles are cleared.
fn hold_during_post(rig: &mut Rig, paths: Vec<PathBuf>) -> Arc<Mutex<Vec<File>>> {
    let holder: Arc<Mutex<Vec<File>>> = Arc::new(Mutex::new(Vec::new()));
    let during = Arc::clone(&holder);
    rig.endpoint = FakeEndpoint::answering(vec![granted(
        "fake-access-new",
        Some("fake-refresh-new"),
        3600,
    )])
    .during_post(move || {
        let held = paths.iter().map(|path| hold_open(path)).collect();
        *during
            .lock()
            .expect("no test thread panics holding the handles") = held;
    });
    holder
}

#[cfg(windows)]
fn release(holder: &Mutex<Vec<File>>) {
    holder
        .lock()
        .expect("no test thread panics holding the handles")
        .clear();
}

#[cfg(windows)]
/// Holds Claude Code's legacy credential lock and, once the poller waits on it (every read before the locks done),
/// opens `path` with no sharing and releases the lock: `path` cannot be read under the locks until the returned handles
/// are cleared.
fn unreadable_under_the_locks(
    rig: &mut Rig,
    path: PathBuf,
) -> (JoinHandle<()>, Arc<Mutex<Vec<File>>>) {
    rig.claude_locks = wait(HANG_GUARD);
    let holder: Arc<Mutex<Vec<File>>> = Arc::new(Mutex::new(Vec::new()));
    let during = Arc::clone(&holder);
    let releaser = release_later(rig, move || {
        during
            .lock()
            .expect("no test thread panics holding the handles")
            .push(hold_open(&path));
    });
    (releaser, holder)
}

/// Account 1's row in the usage table.
fn usage_row(rig: &Rig) -> Value {
    let table: Value = serde_json::from_slice(
        &fs::read(rig.scratch.backup().join("cache").join("usage.json"))
            .expect("the usage table can be read"),
    )
    .expect("the usage table is JSON");
    table["accounts"]["1"].clone()
}

/// The rig's one POST answered `invalid_grant`, running `during` while it is in flight.
fn refuse_the_grant(rig: &mut Rig, during: impl FnOnce() + Send + 'static) {
    rig.endpoint = FakeEndpoint::answering(vec![answer(400, json!({ "error": "invalid_grant" }))])
        .during_post(during);
}

#[test]
fn live_account_reads_the_balance_with_the_live_token() {
    let token = fresh("fake-access-a", "fake-refresh-a");
    let rig = rig(
        "active_prepaid_live",
        &token,
        &token,
        Some(&logged_in_as_one()),
    );
    let usage = Usage::refusing(&[]);

    let outcome = poll(&rig, &usage);

    assert!(
        matches!(outcome.result, PollResult::Fetched { .. }),
        "got {:?}",
        outcome.result
    );
    assert_eq!(
        usage.prepaid_sent(),
        [("fake-access-a".to_owned(), ONE_ORG.to_owned())],
        "the balance is read with the live token the usage read used; account 2 has no organization"
    );
    assert_eq!(usage_row(&rig)["prepaid"]["amountMinor"], json!(102_003));
}

#[test]
fn prepaid_401_spends_no_grant() {
    let token = fresh("fake-access-a", "fake-refresh-a");
    let rig = rig(
        "active_prepaid_401",
        &token,
        &token,
        Some(&logged_in_as_one()),
    );
    let usage = Usage::refusing(&[]).prepaid_answering(401);

    let outcome = poll(&rig, &usage);

    assert!(
        matches!(outcome.result, PollResult::Fetched { .. }),
        "got {:?}",
        outcome.result
    );
    assert!(
        rig.endpoint.bodies().is_empty(),
        "a refused balance read spends no grant"
    );
    assert_eq!(usage.prepaid_sent().len(), 1, "nor is it retried");
    let row = usage_row(&rig);
    assert_eq!(row["prepaid"]["error"], json!("http-401"), "{row}");
    assert_eq!(
        row["lastError"],
        Value::Null,
        "the usage row's failure state is untouched"
    );
    assert_eq!(
        access_of(&live(&rig)),
        "fake-access-a",
        "the live token is untouched"
    );
}

#[test]
fn live_after_recover_reads_the_balance_with_the_working_token() {
    let stale = expired("fake-access-a", "fake-refresh-a");
    let rig = rig(
        "active_prepaid_recovered",
        &stale,
        &stale,
        Some(&logged_in_as_one()),
    );
    let usage = Usage::refusing(&[]);

    let outcome = poll(&rig, &usage);

    assert!(
        matches!(outcome.result, PollResult::Fetched { .. }),
        "got {:?}",
        outcome.result
    );
    assert_eq!(
        rig.endpoint.posted_refresh_tokens(),
        ["fake-refresh-a"],
        "the live token was refreshed"
    );
    assert_eq!(
        usage.sent_for_one(),
        ["fake-access-new"],
        "usage is read with the successor"
    );
    assert_eq!(
        usage.prepaid_sent(),
        [("fake-access-new".to_owned(), ONE_ORG.to_owned())],
        "the balance is read with the successor the usage read used"
    );
    assert_eq!(usage_row(&rig)["prepaid"]["amountMinor"], json!(102_003));
}

#[test]
fn no_balance_read_beside_foreign_credential() {
    // The unexpired live token, of lineage C, serves a reading, but the oracle names lineage C as another account's.
    let foreign = fresh("fake-access-c1", "fake-refresh-c");
    let old_backup = expired("fake-access-a", "fake-refresh-a");
    let mut rig = rig(
        "active_prepaid_foreign",
        &foreign,
        &old_backup,
        Some(&logged_in_as_one()),
    );
    rig.profile = FakeProfile::answering(vec![profile(
        "00000000-0000-4000-8000-0000000000ff",
        "other@example.test",
        "org-other",
    )]);
    let usage = Usage::refusing(&[]);

    let served = poll(&rig, &usage);

    assert_eq!(served.result, PollResult::ForeignCredential);
    assert_eq!(
        usage.sent_for_one(),
        ["fake-access-c1"],
        "the foreign token's usage read ran"
    );
    assert_eq!(
        usage.prepaid_sent(),
        [],
        "no balance is read for a reading that is not the account's"
    );

    // The next pass reads an expired generation of lineage C; Claude Code rotates it while the refresh waits, and the
    // rotation is adopted under the remembered foreign verdict.
    make_due(&rig);
    write_live(&rig, &expired("fake-access-c0", "fake-refresh-c"));
    rig.claude_locks = wait(HANG_GUARD);
    let live_path = rig.claude.credentials_file();
    let releaser = release_later(&rig, move || {
        fs::write(live_path, fresh("fake-access-c2", "fake-refresh-c"))
            .expect("Claude Code rotates the live credential");
    });
    let usage = Usage::refusing(&[]);

    let adopted = poll(&rig, &usage);
    releaser.join().expect("the releaser finishes");

    assert_eq!(adopted.result, PollResult::ForeignCredential);
    assert!(rig.endpoint.bodies().is_empty(), "nothing was posted");
    assert_eq!(
        usage.prepaid_sent(),
        [],
        "no balance is read for a credential adopted as another account's"
    );
}

#[test]
fn held_claude_code_lock_makes_the_refresh_wait_then_defer() {
    let stale = expired("fake-access-a", "fake-refresh-a");
    let rig = rig(
        "active_lock_held",
        &stale,
        &stale,
        Some(&logged_in_as_one()),
    );
    let held = hold_lock(&rig);
    let held_since = modified(&held);

    let started = Instant::now();
    let outcome = poll(&rig, &Usage::refusing(&[]));
    let waited = started.elapsed();

    assert_eq!(outcome.result, PollResult::Deferred(DeferReason::LocksBusy));
    assert!(
        waited >= LOCK_WAIT,
        "the refresh waited out the lock timeout, waited {waited:?}"
    );
    assert!(rig.endpoint.bodies().is_empty(), "nothing was posted");
    assert!(held.is_dir(), "the holder keeps its lock");
    assert_eq!(modified(&held), held_since, "the held lock is untouched");
    assert!(
        !rig.claude.oauth_refresh_lock().exists(),
        "the primary lock is released"
    );
    assert_eq!(live(&rig), stale, "the live file is untouched");
}

#[test]
fn released_claude_code_lock_lets_the_refresh_proceed() {
    let stale = expired("fake-access-a", "fake-refresh-a");
    let mut rig = rig(
        "active_lock_released",
        &stale,
        &stale,
        Some(&logged_in_as_one()),
    );
    rig.claude_locks = wait(HANG_GUARD);
    let releaser = release_later(&rig, || {});

    let outcome = poll(&rig, &Usage::refusing(&[]));
    releaser.join().expect("the releaser finishes");

    assert!(
        matches!(outcome.result, PollResult::Fetched { .. }),
        "got {:?}",
        outcome.result
    );
    assert_eq!(
        rig.endpoint.posted_refresh_tokens(),
        ["fake-refresh-a"],
        "the grant is spent once"
    );
    assert_eq!(
        access_of(&live(&rig)),
        "fake-access-new",
        "the successor is live"
    );
    assert_eq!(
        access_of(&backup(&rig)),
        "fake-access-new",
        "the successor is the backup"
    );
}

#[test]
fn stale_claude_code_lock_at_61_s_is_taken_over() {
    let stale = expired("fake-access-a", "fake-refresh-a");
    let rig = rig(
        "active_lock_stale",
        &stale,
        &stale,
        Some(&logged_in_as_one()),
    );
    let held = hold_lock(&rig);
    set_dir_mtime(&held, Duration::from_secs(61));
    let usage = Usage::refusing(&[]);

    let outcome = poll(&rig, &usage);

    assert!(
        matches!(outcome.result, PollResult::Fetched { .. }),
        "got {:?}",
        outcome.result
    );
    assert_eq!(rig.endpoint.posted_refresh_tokens(), ["fake-refresh-a"]);
    assert_eq!(
        usage.sent_for_one(),
        ["fake-access-new"],
        "the expired token is never sent"
    );
    assert!(!held.exists(), "the taken-over lock is released");
}

#[test]
fn claude_code_lock_at_59_s_defers() {
    let stale = expired("fake-access-a", "fake-refresh-a");
    let rig = rig(
        "active_lock_live",
        &stale,
        &stale,
        Some(&logged_in_as_one()),
    );
    let held = hold_lock(&rig);
    set_dir_mtime(&held, Duration::from_secs(59));

    let outcome = poll(&rig, &Usage::refusing(&[]));

    assert_eq!(outcome.result, PollResult::Deferred(DeferReason::LocksBusy));
    assert!(rig.endpoint.bodies().is_empty(), "nothing was posted");
    assert!(held.is_dir(), "a live holder's lock is never stolen");
}

#[test]
fn drain_begun_while_waiting_on_claude_code_locks_defers_without_a_post() {
    let stale = expired("fake-access-a", "fake-refresh-a");
    let mut rig = rig(
        "active_drain_while_waiting",
        &stale,
        &stale,
        Some(&logged_in_as_one()),
    );
    rig.claude_locks = wait(HANG_GUARD);
    let drain = Arc::clone(&rig.drain);
    let releaser = release_later(&rig, move || {
        let _in_flight = drain.start();
    });

    let outcome = poll(&rig, &Usage::refusing(&[]));
    releaser.join().expect("the releaser finishes");

    assert_eq!(outcome.result, PollResult::Draining);
    assert!(rig.endpoint.bodies().is_empty(), "nothing was posted");
    assert_eq!(live(&rig), stale, "the live file is untouched");
    assert_eq!(backup(&rig), stale, "the backup is untouched");
}

#[test]
fn identity_mismatch_defers_without_a_post() {
    let stale = expired("fake-access-a", "fake-refresh-a");
    let mut rig = rig(
        "active_identity_moved",
        &stale,
        &stale,
        Some(&logged_in_as_one()),
    );
    rig.claude_locks = wait(HANG_GUARD);
    let config = rig.scratch.dir.join(".claude.json");
    let releaser = release_later(&rig, move || {
        let other = json!({ "oauthAccount": { "emailAddress": TWO } });
        fs::write(config, other.to_string()).expect("the global config can be rewritten");
    });

    let outcome = poll(&rig, &Usage::refusing(&[]));
    releaser.join().expect("the releaser finishes");

    assert_eq!(
        outcome.result,
        PollResult::Deferred(DeferReason::IdentityMismatch)
    );
    assert!(rig.endpoint.bodies().is_empty(), "nothing was posted");
    assert_eq!(live(&rig), stale, "the live file is untouched");
}

#[test]
fn credential_rotated_by_claude_code_is_adopted_without_a_post() {
    let lineage_b = fresh("fake-access-b1", "fake-refresh-b");
    let old_backup = expired("fake-access-a", "fake-refresh-a");
    let mut rig = rig(
        "active_adopt",
        &lineage_b,
        &old_backup,
        Some(&logged_in_as_one()),
    );
    rig.profile = FakeProfile::answering(vec![profile(ONE_UUID, ONE, ONE_ORG)]);

    // The first pass serves lineage B and the oracle attributes it, but Claude Code holds its lock, so the backup
    // resync is put off: the verdict stays remembered.
    let held = hold_lock(&rig);
    let first = poll(&rig, &Usage::refusing(&[]));
    assert!(
        matches!(first.result, PollResult::Fetched { .. }),
        "got {:?}",
        first.result
    );
    assert_eq!(
        backup(&rig),
        old_backup,
        "the resync could not take the lock"
    );
    fs::remove_dir(&held).expect("the held lock is released");

    // The next pass reads an expired generation of lineage B; Claude Code rotates it while the refresh waits.
    make_due(&rig);
    write_live(&rig, &expired("fake-access-b0", "fake-refresh-b"));
    rig.claude_locks = wait(HANG_GUARD);
    let rotated = fresh("fake-access-b2", "fake-refresh-b");
    let live_path = rig.claude.credentials_file();
    let written = rotated.clone();
    let releaser = release_later(&rig, move || {
        fs::write(live_path, written).expect("Claude Code rotates the live credential");
    });
    let usage = Usage::refusing(&[]);
    let second = poll(&rig, &usage);
    releaser.join().expect("the releaser finishes");

    assert!(
        matches!(second.result, PollResult::Fetched { .. }),
        "got {:?}",
        second.result
    );
    assert!(rig.endpoint.bodies().is_empty(), "nothing was posted");
    assert_eq!(
        usage.sent_for_one(),
        ["fake-access-b2"],
        "usage is read with the adopted token"
    );
    assert_eq!(
        backup(&rig),
        rotated,
        "the adopted credential is the backup"
    );
    assert_eq!(
        rig.profile.asked(),
        ["fake-access-b1"],
        "the remembered verdict is not asked again"
    );
}

#[cfg(windows)]
#[test]
fn failed_live_write_leaves_credentials_unchanged_and_backup_written() {
    let stale = expired("fake-access-a", "fake-refresh-a");
    let mut rig = rig(
        "active_live_write_fails",
        &stale,
        &stale,
        Some(&logged_in_as_one()),
    );
    let live_path = rig.claude.credentials_file();
    let holder: Arc<Mutex<Option<File>>> = Arc::new(Mutex::new(None));
    let during = Arc::clone(&holder);
    rig.endpoint = FakeEndpoint::answering(vec![granted(
        "fake-access-new",
        Some("fake-refresh-new"),
        3600,
    )])
    .during_post(move || {
        let file = OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(live_path)
            .expect("the live file opens with no sharing");
        *during
            .lock()
            .expect("no test thread panics holding the handle") = Some(file);
    });

    let outcome = poll(&rig, &Usage::refusing(&[]));
    drop(
        holder
            .lock()
            .expect("no test thread panics holding the handle")
            .take(),
    );

    assert_eq!(outcome.result, PollResult::LiveWriteFailed);
    assert_eq!(rig.endpoint.posted_refresh_tokens(), ["fake-refresh-a"]);
    assert_eq!(live(&rig), stale, "the live file is byte-identical");
    assert_eq!(
        access_of(&backup(&rig)),
        "fake-access-new",
        "the backup holds the successor"
    );
    assert_eq!(
        outcome.live_temp_left,
        Some(false),
        "the failed write's temp file was removed"
    );
}

#[test]
fn lock_lost_before_the_live_write_keeps_the_old_live_token() {
    let stale = expired("fake-access-a", "fake-refresh-a");
    let mut rig = rig(
        "active_lock_lost_before_live_write",
        &stale,
        &stale,
        Some(&logged_in_as_one()),
    );
    let lock = rig.claude.credentials_lock();
    let taken = lock.clone();
    // Claude Code takes the legacy lock over as stale while the POST is in flight.
    rig.endpoint = FakeEndpoint::answering(vec![granted(
        "fake-access-new",
        Some("fake-refresh-new"),
        3600,
    )])
    .during_post(move || {
        fs::remove_dir(&taken).expect("Claude Code removes the lock it found stale");
        fs::create_dir(&taken).expect("Claude Code makes its own lock");
    });

    let outcome = poll(&rig, &Usage::refusing(&[]));

    assert_eq!(outcome.result, PollResult::LiveWriteFailed);
    assert_eq!(rig.endpoint.posted_refresh_tokens(), ["fake-refresh-a"]);
    assert_eq!(live(&rig), stale, "the live file keeps the old token");
    assert_eq!(
        access_of(&backup(&rig)),
        "fake-access-new",
        "the backup holds the successor"
    );
    assert_eq!(stash_rows(&rig), 0, "a written backup needs no stash");
    assert!(lock.is_dir(), "Claude Code's lock is left alone");
}

#[cfg(windows)]
#[test]
fn failed_live_write_whose_temp_cannot_be_removed_reports_it() {
    let stale = expired("fake-access-a", "fake-refresh-a");
    let mut rig = rig(
        "active_live_temp_left",
        &stale,
        &stale,
        Some(&logged_in_as_one()),
    );
    let held_paths = vec![rig.claude.credentials_file()];
    let holder = hold_during_post(&mut rig, held_paths);
    let pinner = pin_first_temp(rig.scratch.dir.join("claude"));

    let outcome = poll(&rig, &Usage::refusing(&[]));
    release(&holder);
    let pinned = pinner
        .join()
        .expect("the live write's temp file was pinned");

    assert_eq!(outcome.result, PollResult::LiveWriteFailed);
    assert_eq!(
        outcome.live_temp_left,
        Some(true),
        "the temp file could not be removed"
    );
    assert_eq!(
        temp_files(&rig.scratch.dir.join("claude")).len(),
        1,
        "the temp file is still beside the live file"
    );
    assert_eq!(live(&rig), stale, "the live file is unchanged");
    drop(pinned);
}

#[cfg(windows)]
#[test]
fn successor_is_stashed_when_backup_and_live_writes_fail() {
    let stale = expired("fake-access-a", "fake-refresh-a");
    let mut rig = rig(
        "active_backup_and_live_fail",
        &stale,
        &stale,
        Some(&logged_in_as_one()),
    );
    let held_paths = [rig.scratch.credential_file(), rig.claude.credentials_file()];
    let holder: Arc<Mutex<Vec<File>>> = Arc::new(Mutex::new(Vec::new()));
    let during = Arc::clone(&holder);
    rig.endpoint = FakeEndpoint::answering(vec![granted(
        "fake-access-new",
        Some("fake-refresh-new"),
        3600,
    )])
    .during_post(move || {
        let held = held_paths.iter().map(|path| hold_open(path)).collect();
        *during
            .lock()
            .expect("no test thread panics holding the handles") = held;
    });

    let outcome = poll(&rig, &Usage::refusing(&[]));
    holder
        .lock()
        .expect("no test thread panics holding the handles")
        .clear();

    assert_eq!(outcome.result, PollResult::LiveWriteFailed);
    assert_eq!(rig.endpoint.posted_refresh_tokens(), ["fake-refresh-a"]);
    assert_eq!(live(&rig), stale, "the live file is unchanged");
    assert_eq!(backup(&rig), stale, "the backup could not be written");
    let (id, row) = only_stash_row(&rig);
    let successor = rig
        .scratch
        .stash_entry(&id)
        .expect("the entry's bytes were written");
    assert_eq!(
        access_of(&successor),
        "fake-access-new",
        "the stash holds the successor"
    );
    assert_eq!(row["reason"], StashReason::PersistFailed.kind());
    assert_eq!(
        row["consumedFp"],
        json!(fingerprint(&stale)),
        "keyed to the generation the POST spent"
    );
}

#[test]
fn a_failing_canary_defers_before_the_post() {
    let stale = expired("fake-access-a", "fake-refresh-a");
    let mut rig = rig(
        "active_failing_canary",
        &stale,
        &stale,
        Some(&logged_in_as_one()),
    );
    rig.store =
        Store::open_with_canary(rig.scratch.root(), || false).expect("the scratch store opens");

    let outcome = poll(&rig, &Usage::refusing(&[]));

    assert_eq!(
        outcome.result,
        PollResult::Deferred(DeferReason::StoreUnwritable)
    );
    assert!(rig.endpoint.bodies().is_empty(), "nothing was posted");
    assert_eq!(live(&rig), stale, "the live file is untouched");
}

#[test]
fn a_failing_canary_defers_before_stashing_a_foreign_live_credential() {
    let foreign = credential("fake-access-x", Some("fake-refresh-x"), ms_after(-7200));
    let older_backup = expired("fake-access-a", "fake-refresh-a");
    let mut rig = rig(
        "active_failing_canary_foreign",
        &foreign,
        &older_backup,
        Some(&logged_in_as_one()),
    );
    rig.store =
        Store::open_with_canary(rig.scratch.root(), || false).expect("the scratch store opens");

    let outcome = poll(&rig, &Usage::refusing(&[]));

    assert_eq!(
        outcome.result,
        PollResult::Deferred(DeferReason::StoreUnwritable)
    );
    assert!(rig.endpoint.bodies().is_empty(), "nothing was posted");
    assert_eq!(
        stash_rows(&rig),
        0,
        "a pass that spends nothing stashes nothing"
    );
    assert_eq!(live(&rig), foreign, "the live file is untouched");
}

/// Marks the rig's `accounts.json` as written by a later version (`schemaVersion` 2).
fn write_unknown_schema(rig: &Rig) {
    let path = rig.scratch.backup().join("accounts.json");
    let mut accounts: Value =
        serde_json::from_slice(&fs::read(&path).expect("accounts.json can be read"))
            .expect("it is JSON");
    accounts["schemaVersion"] = json!(2);
    fs::write(&path, accounts.to_string()).expect("accounts.json can be written");
}

#[test]
fn unknown_schema_refuses_the_poll_before_the_post() {
    let stale = expired("fake-access-a", "fake-refresh-a");
    let rig = rig(
        "active_unknown_schema",
        &stale,
        &stale,
        Some(&logged_in_as_one()),
    );
    write_unknown_schema(&rig);

    let polled = poll_once_with_refresh(
        &Usage::refusing(&[]),
        &AutoSwitchSettings::default(),
        &rig.context(),
    );

    assert!(
        matches!(
            polled,
            Err(PollError::StoreRefused(StoreErrorKind::UnknownSchema))
        ),
        "a newer store refuses the poll at its usage write: {polled:?}"
    );
    assert!(rig.endpoint.bodies().is_empty(), "nothing was posted");
    assert_eq!(stash_rows(&rig), 0, "nothing was stashed");
    assert_eq!(live(&rig), stale, "the live file is untouched");
}

#[test]
fn unknown_schema_refuses_the_poll_before_stashing_a_foreign_live_credential() {
    let foreign = credential("fake-access-x", Some("fake-refresh-x"), ms_after(-7200));
    let older_backup = expired("fake-access-a", "fake-refresh-a");
    let rig = rig(
        "active_unknown_schema_foreign",
        &foreign,
        &older_backup,
        Some(&logged_in_as_one()),
    );
    write_unknown_schema(&rig);

    let polled = poll_once_with_refresh(
        &Usage::refusing(&[]),
        &AutoSwitchSettings::default(),
        &rig.context(),
    );

    assert!(
        matches!(
            polled,
            Err(PollError::StoreRefused(StoreErrorKind::UnknownSchema))
        ),
        "a newer store refuses the poll at its usage write: {polled:?}"
    );
    assert!(rig.endpoint.bodies().is_empty(), "nothing was posted");
    assert_eq!(
        stash_rows(&rig),
        0,
        "a pass that spends nothing stashes nothing"
    );
    assert_eq!(live(&rig), foreign, "the live file is untouched");
}

#[test]
fn config_lock_held_does_not_fail_the_live_write() {
    let stale = expired("fake-access-a", "fake-refresh-a");
    let mut config = logged_in_as_one();
    config["primaryApiKey"] = json!("sk-ant-fake");
    let rig = rig("active_config_lock_held", &stale, &stale, Some(&config));
    let held = rig.claude.config_lock();
    fs::create_dir(&held).expect("the config lock directory is made");

    let outcome = poll(&rig, &Usage::refusing(&[]));

    assert!(
        matches!(outcome.result, PollResult::Fetched { .. }),
        "got {:?}",
        outcome.result
    );
    assert_eq!(
        outcome.primary_key_cleared,
        Some(false),
        "the key could not be cleared without the config lock"
    );
    assert_eq!(
        access_of(&live(&rig)),
        "fake-access-new",
        "the successor is live"
    );
    assert!(held.is_dir(), "the holder keeps its lock");
}

#[test]
fn restore_keeps_the_live_mcp_keys() {
    let live_text = with_mcp(
        &expired("fake-access-x", "fake-refresh-x"),
        &json!({ "server": "live" }),
    );
    let restored = fresh("fake-access-a", "fake-refresh-a");
    let rig = rig(
        "active_restore_keeps_mcp",
        &live_text,
        &restored,
        Some(&logged_in_as_one()),
    );

    let outcome = poll(&rig, &Usage::refusing(&[]));

    assert!(
        matches!(outcome.result, PollResult::Fetched { .. }),
        "got {:?}",
        outcome.result
    );
    assert!(
        rig.endpoint.bodies().is_empty(),
        "the backup is restored without a POST"
    );
    let written: Value = serde_json::from_str(&live(&rig)).expect("the live file is JSON");
    let backup: Value = serde_json::from_str(&restored).expect("the backup is JSON");
    assert_eq!(
        written["mcpOAuth"],
        json!({ "server": "live" }),
        "the live file keeps its own MCP keys"
    );
    assert_eq!(
        written["claudeAiOauth"], backup["claudeAiOauth"],
        "the OAuth block is the backup's"
    );
}

#[test]
fn successor_without_claude_ai_oauth_is_not_written_over_the_live_mcp_keys() {
    let stale = expired("fake-access-a", "fake-refresh-a");
    let live_text = with_mcp(&stale, &json!({ "server": "live" }));
    let rig = rig(
        "active_successor_without_oauth",
        &live_text,
        &stale,
        Some(&logged_in_as_one()),
    );
    let successor = json!({ "pluginSecrets": { "plugin": "stashed" } }).to_string();
    let id = "20260921T140000-abcdefabcdef-a1b2c3";
    rig.scratch.write_stash_entry(id, &successor);
    let row = json!({
        "createdAt": "2026-09-21T14:00:00Z",
        "reason": StashReason::PersistFailed.kind(),
        "configSlot": "1",
        "consumedFp": fingerprint(&stale),
        "fingerprint": fingerprint(&successor),
        "organization": ONE_ORG,
    });
    let manifest = json!({ "schemaVersion": 1, "entries": { id: row } });
    fs::write(rig.scratch.manifest_path(), manifest.to_string())
        .expect("the manifest can be written");

    let outcome = poll(&rig, &Usage::refusing(&[]));

    assert_eq!(outcome.result, PollResult::LiveWriteFailed);
    assert!(
        rig.endpoint.bodies().is_empty(),
        "the stashed successor is adopted without a POST"
    );
    assert_eq!(
        live(&rig),
        live_text,
        "the live file keeps its OAuth block and its MCP keys"
    );
}

#[test]
fn foreign_live_token_is_stashed_before_it_is_overwritten() {
    let foreign = expired("fake-access-x", "fake-refresh-x");
    let restored = fresh("fake-access-a", "fake-refresh-a");
    let rig = rig(
        "active_foreign_live_stashed",
        &foreign,
        &restored,
        Some(&logged_in_as_one()),
    );

    let outcome = poll(&rig, &Usage::refusing(&[]));

    assert!(
        matches!(outcome.result, PollResult::Fetched { .. }),
        "got {:?}",
        outcome.result
    );
    assert_eq!(
        access_of(&live(&rig)),
        "fake-access-a",
        "the backup is live"
    );
    let (id, row) = only_stash_row(&rig);
    assert_eq!(
        rig.scratch.stash_entry(&id).as_deref(),
        Some(foreign.as_str()),
        "the overwritten bytes are kept"
    );
    assert_eq!(row["reason"], "foreign");
    assert_eq!(row["configSlot"], "1");
    assert_eq!(
        row["consumedFp"],
        Value::Null,
        "no generation was spent, so no refresh adopts it"
    );
    assert_eq!(row["fingerprint"], json!(fingerprint(&foreign)));
}

#[cfg(windows)]
#[test]
fn foreign_live_stash_failure_defers_before_the_post() {
    let foreign = credential("fake-access-x", Some("fake-refresh-x"), ms_after(-7200));
    let older_backup = expired("fake-access-a", "fake-refresh-a");
    let rig = rig(
        "active_foreign_stash_fails",
        &foreign,
        &older_backup,
        Some(&logged_in_as_one()),
    );
    let manifest = rig.scratch.manifest_path();
    fs::create_dir_all(manifest.parent().expect("the manifest has a folder"))
        .expect("the stash folder can be made");
    fs::write(
        &manifest,
        json!({ "schemaVersion": 1, "entries": {} }).to_string(),
    )
    .expect("the manifest can be written");
    let held = hold_open(&manifest);

    let outcome = poll(&rig, &Usage::refusing(&[]));
    drop(held);

    assert_eq!(
        outcome.result,
        PollResult::Deferred(DeferReason::StashUnwritable)
    );
    assert!(
        rig.endpoint.bodies().is_empty(),
        "a grant whose successor could not be made live is not spent"
    );
    assert_eq!(live(&rig), foreign, "the live file is untouched");
    assert_eq!(backup(&rig), older_backup, "the backup is untouched");
}

#[test]
fn deferral_after_a_401_keeps_the_401() {
    let current = fresh("fake-access-a", "fake-refresh-a");
    let rig = rig(
        "active_401_deferred",
        &current,
        &current,
        Some(&logged_in_as_one()),
    );
    let _held = hold_lock(&rig);

    let outcome = poll(&rig, &Usage::refusing(&["fake-access-a"]));

    assert!(
        matches!(
            outcome.result,
            PollResult::Failed(FetchError::Http { status: 401, .. })
        ),
        "got {:?}",
        outcome.result
    );
    assert!(rig.endpoint.bodies().is_empty(), "nothing was posted");
}

#[test]
fn foreign_lineage_returns_foreign_credential() {
    let foreign = fresh("fake-access-b", "fake-refresh-b");
    let old_backup = expired("fake-access-a", "fake-refresh-a");
    let mut rig = rig(
        "active_foreign",
        &foreign,
        &old_backup,
        Some(&logged_in_as_one()),
    );
    rig.profile = FakeProfile::answering(vec![profile(
        "00000000-0000-4000-8000-0000000000ff",
        "other@example.test",
        "org-other",
    )]);

    let outcome = poll(&rig, &Usage::refusing(&[]));

    assert_eq!(outcome.result, PollResult::ForeignCredential);
    assert_eq!(
        backup(&rig),
        old_backup,
        "the backup keeps the account's own refresh token"
    );
    assert!(rig.endpoint.bodies().is_empty(), "nothing was posted");
}

#[test]
fn refresh_clears_primary_api_key() {
    let stale = expired("fake-access-a", "fake-refresh-a");
    let mut config = logged_in_as_one();
    config["primaryApiKey"] = json!("sk-ant-fake");
    config["numStartups"] = json!(7);
    config["projects"] = json!({ "X:/dev": { "allowedTools": [] } });
    let rig = rig("active_primary_key", &stale, &stale, Some(&config));

    let outcome = poll(&rig, &Usage::refusing(&[]));

    assert!(
        matches!(outcome.result, PollResult::Fetched { .. }),
        "got {:?}",
        outcome.result
    );
    assert_eq!(outcome.primary_key_cleared, Some(true));
    let written: Value = serde_json::from_slice(
        &fs::read(rig.scratch.dir.join(".claude.json")).expect("the global config can be read"),
    )
    .expect("it is JSON");
    let keys: Vec<&String> = written
        .as_object()
        .expect("the config is an object")
        .keys()
        .collect();
    assert_eq!(
        keys,
        ["oauthAccount", "numStartups", "projects"],
        "only primaryApiKey is gone"
    );
    assert_eq!(written["projects"], config["projects"]);
}

#[test]
fn inactive_sequence_active_account_not_logged_in_is_not_refreshed() {
    let stale = expired("fake-access-a", "fake-refresh-a");
    let rig = rig("active_not_logged_in", &stale, &stale, None);

    let outcome = poll(&rig, &Usage::refusing(&[]));

    assert_eq!(
        outcome.result,
        PollResult::TokenExpired,
        "accounts.json's active account keeps the read-only path"
    );
    assert!(rig.endpoint.bodies().is_empty(), "nothing was posted");
    assert_eq!(live(&rig), stale, "the live file is untouched");
    assert_eq!(outcome.primary_key_cleared, None);
}

#[test]
fn resync_writes_the_backup_on_a_true_verdict() {
    let lineage_b = fresh("fake-access-b", "fake-refresh-b");
    let old_backup = expired("fake-access-a", "fake-refresh-a");
    let mut rig = rig(
        "active_resync",
        &lineage_b,
        &old_backup,
        Some(&logged_in_as_one()),
    );
    rig.profile = FakeProfile::answering(vec![profile(ONE_UUID, ONE, ONE_ORG)]);

    let outcome = poll(&rig, &Usage::refusing(&[]));

    assert!(
        matches!(outcome.result, PollResult::Fetched { .. }),
        "got {:?}",
        outcome.result
    );
    assert_eq!(backup(&rig), lineage_b, "the rotated lineage is the backup");
    assert_eq!(rig.profile.asked(), ["fake-access-b"]);
    assert!(rig.endpoint.bodies().is_empty(), "nothing was posted");
}

#[test]
fn resync_skips_when_the_oracle_fails() {
    let lineage_b = fresh("fake-access-b", "fake-refresh-b");
    let old_backup = expired("fake-access-a", "fake-refresh-a");
    let rig = rig(
        "active_resync_oracle_fails",
        &lineage_b,
        &old_backup,
        Some(&logged_in_as_one()),
    );

    let first = poll(&rig, &Usage::refusing(&[]));
    make_due(&rig);
    let second = poll(&rig, &Usage::refusing(&[]));

    assert!(
        matches!(first.result, PollResult::Fetched { .. }),
        "got {:?}",
        first.result
    );
    assert!(
        matches!(second.result, PollResult::Fetched { .. }),
        "got {:?}",
        second.result
    );
    assert_eq!(backup(&rig), old_backup, "the backup is left alone");
    assert_eq!(
        rig.profile.asked(),
        ["fake-access-b", "fake-access-b"],
        "the failure was not remembered"
    );
}

#[cfg(windows)]
#[test]
fn stashed_successor_is_adopted_on_the_next_pass_without_a_post() {
    let stale = expired("fake-access-a", "fake-refresh-a");
    let mut rig = rig(
        "active_stash_adopted",
        &stale,
        &stale,
        Some(&logged_in_as_one()),
    );
    let held_paths = vec![rig.scratch.credential_file(), rig.claude.credentials_file()];
    let holder = hold_during_post(&mut rig, held_paths);
    let first = poll(&rig, &Usage::refusing(&[]));
    release(&holder);
    assert_eq!(first.result, PollResult::LiveWriteFailed);
    assert_eq!(stash_rows(&rig), 1, "the successor was stashed");

    make_due(&rig);
    let usage = Usage::refusing(&[]);
    let second = poll(&rig, &usage);

    assert!(
        matches!(second.result, PollResult::Fetched { .. }),
        "got {:?}",
        second.result
    );
    assert_eq!(
        rig.endpoint.posted_refresh_tokens(),
        ["fake-refresh-a"],
        "the spent grant is not posted again"
    );
    assert_eq!(
        access_of(&live(&rig)),
        "fake-access-new",
        "the stashed successor is live"
    );
    assert_eq!(
        access_of(&backup(&rig)),
        "fake-access-new",
        "the stashed successor is the backup"
    );
    assert_eq!(
        usage.sent_for_one(),
        ["fake-access-new"],
        "usage is read with the successor"
    );
    assert_eq!(stash_rows(&rig), 0, "the stash row is retired");
}

#[test]
fn backup_rewritten_between_read_and_lock_is_used() {
    let stale = expired("fake-access-a", "fake-refresh-a");
    let mut rig = rig(
        "active_backup_rewritten",
        &stale,
        &stale,
        Some(&logged_in_as_one()),
    );
    rig.claude_locks = wait(HANG_GUARD);
    let root = rig.scratch.root();
    let releaser = release_later(&rig, move || {
        // Another writer, holding the roster, seals a fresh generation over the account's backup.
        let rewritten = fresh("fake-access-c", "fake-refresh-c");
        let ad = AssociatedData::credentials(1, ONE_ORG, u64::MAX - 1, ONE);
        let sealed = seal::seal(
            SealPurpose::Credentials,
            &ad,
            &CredentialBlob::new(rewritten),
        )
        .expect("the rewrite seals");
        let path = root.credential_path(1);
        fs::write(&path, sealed.as_bytes()).expect("the backup can be rewritten");
        private_file(&path);
    });

    let outcome = poll(&rig, &Usage::refusing(&[]));
    releaser.join().expect("the releaser finishes");

    assert!(
        matches!(outcome.result, PollResult::Fetched { .. }),
        "got {:?}",
        outcome.result
    );
    assert!(
        rig.endpoint.bodies().is_empty(),
        "the grant of the backup read before the locks is not spent"
    );
    assert_eq!(
        access_of(&live(&rig)),
        "fake-access-c",
        "the backup as it stands under the locks is restored"
    );
}

#[cfg(windows)]
#[test]
fn unreadable_backup_defers_without_a_post() {
    let stale = expired("fake-access-a", "fake-refresh-a");
    let rig = rig(
        "active_backup_unreadable",
        &stale,
        &stale,
        Some(&logged_in_as_one()),
    );
    let held = hold_open(&rig.scratch.credential_file());

    let outcome = poll(&rig, &Usage::refusing(&[]));
    drop(held);

    assert_eq!(
        outcome.result,
        PollResult::Deferred(DeferReason::BackupUnreadable)
    );
    assert!(rig.endpoint.bodies().is_empty(), "nothing was posted");
    assert_eq!(live(&rig), stale, "the live file is untouched");
}

#[test]
fn drain_after_a_usage_401_returns_the_401() {
    let current = fresh("fake-access-a", "fake-refresh-a");
    let rig = rig(
        "active_401_draining",
        &current,
        &current,
        Some(&logged_in_as_one()),
    );
    let _in_flight = rig.drain.start();

    let outcome = poll(&rig, &Usage::refusing(&["fake-access-a"]));

    assert!(
        matches!(
            outcome.result,
            PollResult::Failed(FetchError::Http { status: 401, .. })
        ),
        "got {:?}",
        outcome.result
    );
    assert!(rig.endpoint.bodies().is_empty(), "nothing was posted");
}

#[cfg(windows)]
#[test]
fn backup_write_is_reported_on_the_outcome() {
    let stale = expired("fake-access-a", "fake-refresh-a");
    let written = rig(
        "active_backup_reported_written",
        &stale,
        &stale,
        Some(&logged_in_as_one()),
    );
    let outcome = poll(&written, &Usage::refusing(&[]));
    assert!(
        matches!(outcome.result, PollResult::Fetched { .. }),
        "got {:?}",
        outcome.result
    );
    assert_eq!(
        outcome.backup_written,
        Some(true),
        "the successor was written to the backup"
    );

    let mut failed = rig(
        "active_backup_reported_failed",
        &stale,
        &stale,
        Some(&logged_in_as_one()),
    );
    let held_paths = vec![failed.scratch.credential_file()];
    let holder = hold_during_post(&mut failed, held_paths);
    let outcome = poll(&failed, &Usage::refusing(&[]));
    release(&holder);
    assert!(
        matches!(outcome.result, PollResult::Fetched { .. }),
        "got {:?}",
        outcome.result
    );
    assert_eq!(
        access_of(&live(&failed)),
        "fake-access-new",
        "the successor is live"
    );
    assert_eq!(
        outcome.backup_written,
        Some(false),
        "the backup could not take the successor"
    );

    let restored = fresh("fake-access-a", "fake-refresh-a");
    let untouched = rig(
        "active_backup_reported_none",
        &expired("fake-access-x", "fake-refresh-x"),
        &restored,
        Some(&logged_in_as_one()),
    );
    let outcome = poll(&untouched, &Usage::refusing(&[]));
    assert!(
        matches!(outcome.result, PollResult::Fetched { .. }),
        "got {:?}",
        outcome.result
    );
    assert_eq!(outcome.backup_written, None, "a restore writes no backup");
}

#[cfg(windows)]
#[test]
fn restore_after_a_failed_live_write_stashes_nothing() {
    let stale = expired("fake-access-a", "fake-refresh-a");
    let mut rig = rig(
        "active_restore_after_live_fail",
        &stale,
        &stale,
        Some(&logged_in_as_one()),
    );
    let held_paths = vec![rig.claude.credentials_file()];
    let holder = hold_during_post(&mut rig, held_paths);
    let first = poll(&rig, &Usage::refusing(&[]));
    release(&holder);
    assert_eq!(first.result, PollResult::LiveWriteFailed);

    make_due(&rig);
    let second = poll(&rig, &Usage::refusing(&[]));

    assert!(
        matches!(second.result, PollResult::Fetched { .. }),
        "got {:?}",
        second.result
    );
    assert_eq!(
        rig.endpoint.posted_refresh_tokens(),
        ["fake-refresh-a"],
        "the backup is restored without a POST"
    );
    assert_eq!(
        access_of(&live(&rig)),
        "fake-access-new",
        "the restore writes the live file"
    );
    assert_eq!(
        stash_rows(&rig),
        0,
        "the spent generation it overwrites is the account's own"
    );
}

#[test]
fn cleared_live_file_restores_the_backup_without_a_post() {
    let stale = expired("fake-access-x", "fake-refresh-x");
    let restored = fresh("fake-access-a", "fake-refresh-a");
    let mut rig = rig(
        "active_live_cleared",
        &stale,
        &restored,
        Some(&logged_in_as_one()),
    );
    rig.claude_locks = wait(HANG_GUARD);
    let live_path = rig.claude.credentials_file();
    let releaser = release_later(&rig, move || {
        fs::write(live_path, "").expect("Claude Code clears the live credential");
    });
    let usage = Usage::refusing(&[]);

    let outcome = poll(&rig, &usage);
    releaser.join().expect("the releaser finishes");

    assert!(
        matches!(outcome.result, PollResult::Fetched { .. }),
        "got {:?}",
        outcome.result
    );
    assert!(
        rig.endpoint.bodies().is_empty(),
        "the backup is restored without a POST"
    );
    assert_eq!(
        live(&rig),
        restored,
        "the backup is written whole into the cleared live file"
    );
    assert_eq!(
        usage.sent_for_one(),
        ["fake-access-a"],
        "usage is read with the restored token"
    );
}

#[test]
fn newer_backup_is_restored_over_the_older_live_lineage() {
    let older = credential("fake-access-x", Some("fake-refresh-x"), ms_after(-7200));
    let newer = fresh("fake-access-a", "fake-refresh-a");
    let rig = rig(
        "active_backup_newer",
        &older,
        &newer,
        Some(&logged_in_as_one()),
    );
    let usage = Usage::refusing(&[]);

    let outcome = poll(&rig, &usage);

    assert!(
        matches!(outcome.result, PollResult::Fetched { .. }),
        "got {:?}",
        outcome.result
    );
    assert!(
        rig.endpoint.bodies().is_empty(),
        "the newer backup is restored without a POST"
    );
    assert_eq!(
        access_of(&live(&rig)),
        "fake-access-a",
        "the backup is live"
    );
    assert_eq!(
        usage.sent_for_one(),
        ["fake-access-a"],
        "usage is read with the restored token"
    );
}

#[test]
fn live_moved_to_another_lineage_under_the_locks_defers() {
    let stale = expired("fake-access-a", "fake-refresh-a");
    let moved = expired("fake-access-y", "fake-refresh-y");
    let mut rig = rig(
        "active_live_moved",
        &stale,
        &stale,
        Some(&logged_in_as_one()),
    );
    rig.claude_locks = wait(HANG_GUARD);
    let live_path = rig.claude.credentials_file();
    let written = moved.clone();
    let releaser = release_later(&rig, move || {
        fs::write(live_path, written).expect("another login replaces the live credential");
    });

    let outcome = poll(&rig, &Usage::refusing(&[]));
    releaser.join().expect("the releaser finishes");

    assert_eq!(outcome.result, PollResult::Deferred(DeferReason::LiveMoved));
    assert!(rig.endpoint.bodies().is_empty(), "nothing was posted");
    assert_eq!(
        live(&rig),
        moved,
        "the live file is left as the other login wrote it"
    );
    assert_eq!(backup(&rig), stale, "the backup is untouched");
}

#[test]
fn newer_live_lineage_without_a_verdict_defers_as_unattributable() {
    let newer = credential("fake-access-x", Some("fake-refresh-x"), ms_after(-60));
    let older = credential("fake-access-a", Some("fake-refresh-a"), ms_after(-3600));
    let rig = rig(
        "active_expiry_unattributed",
        &newer,
        &older,
        Some(&logged_in_as_one()),
    );

    let outcome = poll(&rig, &Usage::refusing(&[]));

    assert_eq!(
        outcome.result,
        PollResult::Deferred(DeferReason::Unattributable)
    );
    assert!(
        rig.endpoint.bodies().is_empty(),
        "a lineage nobody attributed is never spent"
    );
    assert_eq!(live(&rig), newer, "the live file is untouched");
    assert_eq!(backup(&rig), older, "the backup is untouched");
}

/// Account 1's backup holds `successor`, the generation a refresh stored, while the live file goes back to the spent
/// predecessor's lineage: a first pass serves a fresh copy of that lineage and the oracle attributes it (Claude Code
/// holds its lock, so the resync is put off and the verdict stays remembered), then the live file holds an expired
/// generation of it. The second pass must spend the successor's grant, never the predecessor's.
fn spent_predecessor_over(test: &str, successor: &str) -> Rig {
    let served = fresh("fake-access-p1", "fake-refresh-p");
    let mut rig = rig(test, &served, successor, Some(&logged_in_as_one()));
    rig.profile = FakeProfile::answering(vec![profile(ONE_UUID, ONE, ONE_ORG)]);
    let held = hold_lock(&rig);
    let first = poll(&rig, &Usage::refusing(&[]));
    assert!(
        matches!(first.result, PollResult::Fetched { .. }),
        "got {:?}",
        first.result
    );
    fs::remove_dir(&held).expect("the held lock is released");
    assert_eq!(
        backup(&rig),
        successor,
        "the first pass leaves the backup alone"
    );

    make_due(&rig);
    write_live(
        &rig,
        &credential("fake-access-p0", Some("fake-refresh-p"), ms_after(-60)),
    );
    rig
}

#[test]
fn stored_successor_with_no_expiry_is_not_replaced_by_the_spent_live_predecessor() {
    let successor =
        r#"{"claudeAiOauth":{"accessToken":"fake-access-s","refreshToken":"fake-refresh-s"}}"#;
    let rig = spent_predecessor_over("active_successor_no_expiry", successor);

    poll(&rig, &Usage::refusing(&[]));

    // A successor with no expiry reads as unexpired, so it is restored without a POST.
    assert!(
        rig.endpoint.bodies().is_empty(),
        "no grant is posted, the spent predecessor's least of all"
    );
    assert_eq!(
        access_of(&live(&rig)),
        "fake-access-s",
        "the stored successor is made live"
    );
    assert_eq!(
        backup(&rig),
        successor,
        "the backup still holds the successor"
    );
}

#[test]
fn stored_successor_with_zero_expiry_is_not_replaced_by_the_spent_live_predecessor() {
    let successor = credential("fake-access-s", Some("fake-refresh-s"), 0);
    let rig = spent_predecessor_over("active_successor_zero_expiry", &successor);

    poll(&rig, &Usage::refusing(&[]));

    assert_eq!(
        rig.endpoint.posted_refresh_tokens(),
        ["fake-refresh-s"],
        "the stored successor's grant is spent, not the predecessor's"
    );
}

/// A stored successor a degenerate grant left behind: no access token, its refresh token, and the stand-in expiry
/// `expires_at_ms`.
fn degenerate_successor(expires_at_ms: i64) -> String {
    credential("", Some("fake-refresh-s"), expires_at_ms)
}

#[test]
fn degenerate_stored_successor_over_a_spent_live_predecessor_is_spent_by_its_refresh_token() {
    let successor = degenerate_successor(ms_after(-30));
    let rig = spent_predecessor_over("active_degenerate_successor_spent", &successor);
    let usage = Usage::refusing(&[]);

    let outcome = poll(&rig, &usage);

    assert!(
        matches!(outcome.result, PollResult::Fetched { .. }),
        "got {:?}",
        outcome.result
    );
    assert_eq!(
        rig.endpoint.posted_refresh_tokens(),
        ["fake-refresh-s"],
        "the stored successor's grant is spent, not the predecessor's"
    );
    assert_eq!(
        access_of(&live(&rig)),
        "fake-access-new",
        "the successor's successor is live"
    );
    assert_eq!(
        usage.sent_for_one(),
        ["fake-access-new"],
        "usage is read with the new token"
    );
}

#[test]
fn degenerate_stored_successor_without_a_verdict_is_spent_and_the_predecessor_stashed_as_foreign() {
    let predecessor = credential("fake-access-p0", Some("fake-refresh-p"), ms_after(-60));
    let successor = degenerate_successor(ms_after(-30));
    let rig = rig(
        "active_degenerate_successor_no_verdict",
        &predecessor,
        &successor,
        Some(&logged_in_as_one()),
    );

    let outcome = poll(&rig, &Usage::refusing(&[]));

    assert!(
        matches!(outcome.result, PollResult::Fetched { .. }),
        "got {:?}",
        outcome.result
    );
    assert_eq!(
        rig.endpoint.posted_refresh_tokens(),
        ["fake-refresh-s"],
        "the stored successor's grant is spent, not the unattributed predecessor's"
    );
    let (id, row) = only_stash_row(&rig);
    assert_eq!(
        rig.scratch.stash_entry(&id).as_deref(),
        Some(predecessor.as_str()),
        "the overwritten predecessor is kept"
    );
    assert_eq!(row["reason"], "foreign");
    assert_eq!(row["fingerprint"], json!(fingerprint(&predecessor)));
}

#[test]
fn degenerate_stored_successor_is_not_replaced_by_the_spent_live_predecessor() {
    // A fresh predecessor the server still accepts and the oracle attributes, with Claude Code's locks free, so the
    // resync runs; the successor's stand-in expiry is at its closest, 1 ms after the predecessor's.
    let predecessor = credential("fake-access-p", Some("fake-refresh-p"), ms_after(3600));
    let successor = degenerate_successor(ms_after(3600) + 1);
    let mut rig = rig(
        "active_degenerate_successor_kept",
        &predecessor,
        &successor,
        Some(&logged_in_as_one()),
    );
    rig.profile = FakeProfile::answering(vec![profile(ONE_UUID, ONE, ONE_ORG)]);

    let outcome = poll(&rig, &Usage::refusing(&[]));

    assert!(
        matches!(outcome.result, PollResult::Fetched { .. }),
        "got {:?}",
        outcome.result
    );
    assert_eq!(
        rig.profile.asked(),
        ["fake-access-p"],
        "the oracle attributes the predecessor's lineage"
    );
    assert!(rig.endpoint.bodies().is_empty(), "nothing was posted");
    assert_eq!(
        backup(&rig),
        successor,
        "the spent predecessor is not resynced over the stored successor"
    );
}

#[test]
fn degenerate_successor_invalid_grant_strikes_the_successor_generation() {
    let successor = degenerate_successor(ms_after(-30));
    let mut rig = spent_predecessor_over("active_degenerate_successor_struck", &successor);
    refuse_the_grant(&mut rig, || {});

    let outcome = poll(&rig, &Usage::refusing(&[]));

    assert_eq!(outcome.result, PollResult::AuthDead);
    assert_eq!(
        rig.endpoint.posted_refresh_tokens(),
        ["fake-refresh-s"],
        "the successor's grant was posted"
    );
    let row = usage_row(&rig);
    assert_eq!(
        row["struckFingerprint"],
        json!(fingerprint(&successor)),
        "the successor's generation is struck"
    );
    assert_ne!(
        row["struckFingerprint"],
        json!(fingerprint(&expired("fake-access-p0", "fake-refresh-p"))),
        "the predecessor's generation is not struck"
    );

    lift_backoff(&rig);
    let second = poll(&rig, &Usage::refusing(&[]));

    assert_eq!(
        second.result,
        PollResult::AuthDead,
        "the strike on the successor's generation still applies"
    );
    assert_eq!(
        rig.endpoint.bodies().len(),
        1,
        "the dead grant is not posted again"
    );
}

#[test]
fn live_with_the_backups_refresh_token_is_still_spent_live() {
    // The live copy expires later than the backup and no verdict names it: only its shared refresh token, not its
    // generation, lets it be spent without deferring as unattributable.
    let stale = credential("fake-access-a", Some("fake-refresh-a"), ms_after(-10));
    let degenerate = credential("", Some("fake-refresh-a"), ms_after(-30));
    let rig = rig(
        "active_live_carries_backup_refresh",
        &stale,
        &degenerate,
        Some(&logged_in_as_one()),
    );

    let outcome = poll(&rig, &Usage::refusing(&[]));

    assert!(
        matches!(outcome.result, PollResult::Fetched { .. }),
        "got {:?}",
        outcome.result
    );
    assert_eq!(
        rig.endpoint.posted_refresh_tokens(),
        ["fake-refresh-a"],
        "the shared grant is spent once"
    );
    assert_eq!(
        access_of(&live(&rig)),
        "fake-access-new",
        "the successor is live"
    );
    assert_eq!(
        stash_rows(&rig),
        0,
        "the live file is the backup's lineage, so nothing is stashed"
    );
}

#[test]
fn cleared_live_file_spends_a_degenerate_backup_by_its_refresh_token() {
    let stale = expired("fake-access-x", "fake-refresh-x");
    let successor = degenerate_successor(ms_after(-30));
    let mut rig = rig(
        "active_live_cleared_degenerate",
        &stale,
        &successor,
        Some(&logged_in_as_one()),
    );
    rig.claude_locks = wait(HANG_GUARD);
    let live_path = rig.claude.credentials_file();
    let releaser = release_later(&rig, move || {
        fs::write(live_path, "").expect("Claude Code clears the live credential");
    });

    let outcome = poll(&rig, &Usage::refusing(&[]));
    releaser.join().expect("the releaser finishes");

    assert!(
        matches!(outcome.result, PollResult::Fetched { .. }),
        "got {:?}",
        outcome.result
    );
    assert_eq!(
        rig.endpoint.posted_refresh_tokens(),
        ["fake-refresh-s"],
        "the stored successor's grant is spent once"
    );
    assert_eq!(
        access_of(&live(&rig)),
        "fake-access-new",
        "the successor's successor is live"
    );
}

#[test]
fn degenerate_stored_successor_with_a_far_expiry_is_not_restored_live() {
    let successor = degenerate_successor(ms_after(3600));
    let rig = spent_predecessor_over("active_degenerate_successor_far_expiry", &successor);

    let outcome = poll(&rig, &Usage::refusing(&[]));

    assert!(
        matches!(outcome.result, PollResult::Fetched { .. }),
        "got {:?}",
        outcome.result
    );
    assert_eq!(
        rig.endpoint.posted_refresh_tokens(),
        ["fake-refresh-s"],
        "an unexpired successor with no access token is spent, not restored"
    );
    assert_eq!(
        access_of(&live(&rig)),
        "fake-access-new",
        "the empty access token is never written live"
    );
}

#[test]
fn active_poll_adopts_an_older_rotation_without_writing_it_over_the_backup() {
    let lineage_b = fresh("fake-access-b1", "fake-refresh-b");
    let newer_backup = credential("fake-access-a", Some("fake-refresh-a"), ms_after(7200));
    let mut rig = rig(
        "active_adopt_older",
        &lineage_b,
        &newer_backup,
        Some(&logged_in_as_one()),
    );
    rig.profile = FakeProfile::answering(vec![profile(ONE_UUID, ONE, ONE_ORG)]);

    // The first pass serves lineage B and the oracle attributes it, but Claude Code holds its lock, so the backup
    // resync is put off: the verdict stays remembered.
    let held = hold_lock(&rig);
    let first = poll(&rig, &Usage::refusing(&[]));
    assert!(
        matches!(first.result, PollResult::Fetched { .. }),
        "got {:?}",
        first.result
    );
    fs::remove_dir(&held).expect("the held lock is released");

    // The next pass reads an expired generation of lineage B; Claude Code rotates it, to an expiry earlier than the
    // backup's, while the refresh waits.
    make_due(&rig);
    write_live(&rig, &expired("fake-access-b0", "fake-refresh-b"));
    rig.claude_locks = wait(HANG_GUARD);
    let rotated = fresh("fake-access-b2", "fake-refresh-b");
    let live_path = rig.claude.credentials_file();
    let written = rotated.clone();
    let releaser = release_later(&rig, move || {
        fs::write(live_path, written).expect("Claude Code rotates the live credential");
    });
    let usage = Usage::refusing(&[]);
    let second = poll(&rig, &usage);
    releaser.join().expect("the releaser finishes");

    assert!(
        matches!(second.result, PollResult::Fetched { .. }),
        "got {:?}",
        second.result
    );
    assert!(rig.endpoint.bodies().is_empty(), "nothing was posted");
    assert_eq!(
        usage.sent_for_one(),
        ["fake-access-b2"],
        "usage is read with the adopted token"
    );
    assert_eq!(
        backup(&rig),
        newer_backup,
        "the older rotation is not written over the newer backup"
    );
}

#[cfg(windows)]
#[test]
fn resync_skips_when_the_backup_cannot_be_read_under_the_locks() {
    let lineage_b = credential("fake-access-b", Some("fake-refresh-b"), ms_after(7200));
    let old_backup = expired("fake-access-a", "fake-refresh-a");
    let mut rig = rig(
        "active_resync_backup_unreadable",
        &lineage_b,
        &old_backup,
        Some(&logged_in_as_one()),
    );
    rig.profile = FakeProfile::answering(vec![profile(ONE_UUID, ONE, ONE_ORG)]);
    let cred = rig.scratch.credential_file();
    let (releaser, holder) = unreadable_under_the_locks(&mut rig, cred);

    let outcome = poll(&rig, &Usage::refusing(&[]));
    releaser.join().expect("the releaser finishes");
    release(&holder);

    assert!(
        matches!(outcome.result, PollResult::Fetched { .. }),
        "got {:?}",
        outcome.result
    );
    assert_eq!(outcome.backup_written, None, "no backup write was tried");
    assert_eq!(backup(&rig), old_backup, "the backup is untouched");
}

#[test]
fn resync_replaces_a_successor_refreshed_without_an_expiry_with_a_later_live_successor() {
    let endpoint = FakeEndpoint::answering(vec![answer(
        200,
        json!({ "access_token": "fake-access-s", "refresh_token": "fake-refresh-s" }),
    )]);
    let spent = CredentialBlob::new(expired("fake-access-p", "fake-refresh-p"));
    let stored = refresh_grant(&endpoint, &spent, now())
        .expect("a new refresh token is always a successor")
        .credentials
        .expose()
        .to_owned();
    let later = fresh("fake-access-c", "fake-refresh-c");
    let mut rig = rig(
        "active_resync_over_unknown_expiry",
        &later,
        &stored,
        Some(&logged_in_as_one()),
    );
    rig.profile = FakeProfile::answering(vec![profile(ONE_UUID, ONE, ONE_ORG)]);

    let outcome = poll(&rig, &Usage::refusing(&[]));

    assert!(
        matches!(outcome.result, PollResult::Fetched { .. }),
        "got {:?}",
        outcome.result
    );
    assert_eq!(
        backup(&rig),
        later,
        "Claude Code's later successor replaces the stored one"
    );
}

#[test]
fn active_poll_resync_does_not_write_an_older_live_copy_over_the_backup() {
    // Unexpired past the five-minute buffer, so the poll fetches with it and then resyncs.
    let older = credential("fake-access-b", Some("fake-refresh-b"), ms_after(1800));
    let newer = fresh("fake-access-a", "fake-refresh-a");
    let mut rig = rig(
        "active_keeps_newer_backup",
        &older,
        &newer,
        Some(&logged_in_as_one()),
    );
    rig.profile = FakeProfile::answering(vec![profile(ONE_UUID, ONE, ONE_ORG)]);

    let outcome = poll(&rig, &Usage::refusing(&[]));

    assert!(
        matches!(outcome.result, PollResult::Fetched { .. }),
        "got {:?}",
        outcome.result
    );
    assert_eq!(
        rig.profile.asked(),
        ["fake-access-b"],
        "the oracle attributed the live lineage"
    );
    assert_eq!(
        backup(&rig),
        newer,
        "the newer backup is not replaced by the older live copy"
    );
    assert!(rig.endpoint.bodies().is_empty(), "nothing was posted");
}

#[test]
fn dead_lineage_whose_source_moved_is_refresh_failed() {
    // The live file holds a later generation of the backup's lineage, so the grant is spent from the live file.
    let stale = expired("fake-access-a1", "fake-refresh-a");
    let older = expired("fake-access-a0", "fake-refresh-a");
    let mut rig = rig(
        "active_dead_source_moved",
        &stale,
        &older,
        Some(&logged_in_as_one()),
    );
    let live_path = rig.claude.credentials_file();
    refuse_the_grant(&mut rig, move || {
        let rotated = expired("fake-access-b", "fake-refresh-b");
        fs::write(live_path, rotated).expect("a writer moves the live credential during the POST");
    });

    let outcome = poll(&rig, &Usage::refusing(&[]));

    assert_eq!(outcome.result, PollResult::TokenExpired);
    assert_eq!(rig.endpoint.posted_refresh_tokens(), ["fake-refresh-a"]);
    let row = usage_row(&rig);
    assert_eq!(
        row["lastError"], "refresh-failed",
        "a raced lineage is not declared dead"
    );
    assert_ne!(
        row["struckFingerprint"],
        json!(fingerprint(&stale)),
        "nothing is struck"
    );
}

#[test]
fn live_source_moved_during_an_invalid_grant_is_refresh_failed() {
    // The live file and the backup are byte-identical, so the grant is spent from the live file, which is re-read.
    let stale = expired("fake-access-a", "fake-refresh-a");
    let mut rig = rig(
        "active_dead_live_source_moved",
        &stale,
        &stale,
        Some(&logged_in_as_one()),
    );
    let live_path = rig.claude.credentials_file();
    refuse_the_grant(&mut rig, move || {
        let rotated = expired("fake-access-b", "fake-refresh-b");
        fs::write(live_path, rotated)
            .expect("Claude Code rotates the live credential during the POST");
    });

    let outcome = poll(&rig, &Usage::refusing(&[]));

    assert_eq!(outcome.result, PollResult::TokenExpired);
    assert_eq!(rig.endpoint.posted_refresh_tokens(), ["fake-refresh-a"]);
    let row = usage_row(&rig);
    assert_eq!(
        row["lastError"], "refresh-failed",
        "a raced live lineage is not declared dead"
    );
    assert_ne!(
        row["struckFingerprint"],
        json!(fingerprint(&stale)),
        "nothing is struck"
    );
}

#[test]
fn an_unreadable_source_after_an_invalid_grant_is_not_struck() {
    // The backup holds the later generation, so the grant spent is the backup's, and the source re-read is the backup.
    let stale = expired("fake-access-a", "fake-refresh-a");
    let older = credential("fake-access-x", Some("fake-refresh-x"), ms_after(-7200));
    let mut rig = rig(
        "active_dead_source_unreadable",
        &older,
        &stale,
        Some(&logged_in_as_one()),
    );
    let backup_path = rig.scratch.credential_file();
    refuse_the_grant(&mut rig, move || {
        fs::remove_file(&backup_path).expect("the backup can be removed");
        fs::create_dir(&backup_path).expect("a folder can stand where the backup was");
    });

    let outcome = poll(&rig, &Usage::refusing(&[]));

    assert_eq!(outcome.result, PollResult::TokenExpired);
    assert_eq!(rig.endpoint.posted_refresh_tokens(), ["fake-refresh-a"]);
    let row = usage_row(&rig);
    assert_eq!(
        row["lastError"], "refresh-failed",
        "a source that cannot be read is not declared dead"
    );
    assert_ne!(
        row["struckFingerprint"],
        json!(fingerprint(&stale)),
        "nothing is struck"
    );
}

#[test]
fn an_undecodable_live_source_after_an_invalid_grant_is_not_struck() {
    // The live file and the backup are byte-identical, so the grant is spent from the live file, which is re-read.
    let stale = expired("fake-access-a", "fake-refresh-a");
    let mut rig = rig(
        "active_dead_live_source_undecodable",
        &stale,
        &stale,
        Some(&logged_in_as_one()),
    );
    let live_path = rig.claude.credentials_file();
    refuse_the_grant(&mut rig, move || {
        fs::write(live_path, [0xff, 0xfe, 0xfd])
            .expect("the live file can be replaced with bytes that are not UTF-8");
    });

    let outcome = poll(&rig, &Usage::refusing(&[]));

    assert_eq!(outcome.result, PollResult::TokenExpired);
    assert_eq!(rig.endpoint.posted_refresh_tokens(), ["fake-refresh-a"]);
    let row = usage_row(&rig);
    assert_eq!(
        row["lastError"], "refresh-failed",
        "a live file that cannot be read is not declared dead"
    );
    assert_ne!(
        row["struckFingerprint"],
        json!(fingerprint(&stale)),
        "nothing is struck"
    );
}

/// Only a live file that is absent is absent: one removed during the POST has not moved, so the dead generation is
/// struck.
#[test]
fn an_absent_live_source_after_an_invalid_grant_is_struck() {
    // The live file and the backup are byte-identical, so the grant is spent from the live file, which is re-read.
    let stale = expired("fake-access-a", "fake-refresh-a");
    let mut rig = rig(
        "active_dead_live_source_absent",
        &stale,
        &stale,
        Some(&logged_in_as_one()),
    );
    let live_path = rig.claude.credentials_file();
    refuse_the_grant(&mut rig, move || {
        fs::remove_file(live_path).expect("the live file can be removed");
    });

    let outcome = poll(&rig, &Usage::refusing(&[]));

    assert_eq!(outcome.result, PollResult::AuthDead);
    assert_eq!(rig.endpoint.posted_refresh_tokens(), ["fake-refresh-a"]);
    let row = usage_row(&rig);
    assert_eq!(row["lastError"], "invalid_grant");
    assert_eq!(
        row["struckFingerprint"],
        json!(fingerprint(&stale)),
        "the generation posted is struck"
    );
}

#[test]
fn dead_lineage_whose_source_did_not_move_is_struck() {
    let stale = expired("fake-access-a", "fake-refresh-a");
    let mut rig = rig(
        "active_dead_source_kept",
        &stale,
        &stale,
        Some(&logged_in_as_one()),
    );
    refuse_the_grant(&mut rig, || {});

    let outcome = poll(&rig, &Usage::refusing(&[]));

    assert_eq!(outcome.result, PollResult::AuthDead);
    assert_eq!(rig.endpoint.posted_refresh_tokens(), ["fake-refresh-a"]);
    let row = usage_row(&rig);
    assert_eq!(row["lastError"], "invalid_grant");
    assert_eq!(
        row["struckFingerprint"],
        json!(fingerprint(&stale)),
        "the generation posted is struck"
    );
}

/// Lifts account 1's failure backoff by hand, keeping its strike: the test clock stands still, so the backoff a strike
/// sets would otherwise keep the account out of every later pass.
fn lift_backoff(rig: &Rig) {
    let path = rig.scratch.backup().join("cache").join("usage.json");
    let mut table: Value =
        serde_json::from_slice(&fs::read(&path).expect("the usage table can be read"))
            .expect("the usage table is JSON");
    table["accounts"]["1"]["backoffUntil"] = Value::Null;
    fs::write(&path, table.to_string()).expect("the usage table can be written");
}

/// A rig whose live file holds an older expired lineage than its backup, polled once with the backup's grant refused:
/// the poll spends the backup's grant, so the strike is bound to the backup's generation, not the live one. The
/// backoff is lifted, so only the strike can keep the account out of the next pass. Returns the rig and the backup.
fn struck_on_the_backup(test: &str) -> (Rig, String) {
    let older = credential("fake-access-x", Some("fake-refresh-x"), ms_after(-7200));
    let later = expired("fake-access-a", "fake-refresh-a");
    let mut rig = rig(test, &older, &later, Some(&logged_in_as_one()));
    refuse_the_grant(&mut rig, || {});

    let first = poll(&rig, &Usage::refusing(&[]));

    assert_eq!(first.result, PollResult::AuthDead);
    assert_eq!(
        rig.endpoint.posted_refresh_tokens(),
        ["fake-refresh-a"],
        "the backup's grant is spent"
    );
    let row = usage_row(&rig);
    assert_eq!(
        row["struckFingerprint"],
        json!(fingerprint(&later)),
        "the backup's generation is struck"
    );
    assert_ne!(
        fingerprint(&live(&rig)),
        fingerprint(&later),
        "the live file holds another generation"
    );
    lift_backoff(&rig);
    (rig, later)
}

#[test]
fn a_strike_on_the_backup_generation_keeps_the_live_account_dead() {
    let (rig, _) = struck_on_the_backup("active_strike_on_backup");

    let second = poll(&rig, &Usage::refusing(&[]));

    assert_eq!(
        second.result,
        PollResult::AuthDead,
        "the strike on the backup's generation still applies"
    );
    assert_eq!(
        rig.endpoint.bodies().len(),
        1,
        "the dead grant is not posted again"
    );
}

#[test]
fn a_strike_on_the_backup_generation_holds_in_force_mode() {
    let (rig, _) = struck_on_the_backup("active_strike_on_backup_force");
    let accounts = BTreeSet::from([1]);
    let force = FetchScope::Only {
        accounts: &accounts,
        mode: FetchMode::Force,
    };

    let outcomes = poll_with_refresh(
        &Usage::refusing(&[]),
        &AutoSwitchSettings::default(),
        &rig.context(),
        force,
    )
    .expect("the poll runs");
    let second = outcomes
        .into_iter()
        .find(|outcome| outcome.number == 1)
        .expect("account 1 has an outcome");

    assert_eq!(
        second.result,
        PollResult::AuthDead,
        "a forced fetch refuses the strike"
    );
    assert_eq!(
        rig.endpoint.bodies().len(),
        1,
        "the dead grant is not posted again"
    );
}

#[cfg(windows)]
#[test]
fn an_unreadable_backup_keeps_a_strike_on_the_live_account() {
    let (rig, later) = struck_on_the_backup("active_strike_backup_unreadable");
    let held = hold_open(&rig.scratch.credential_file());

    let second = poll(&rig, &Usage::refusing(&[]));
    drop(held);

    assert_eq!(
        second.result,
        PollResult::AuthDead,
        "an unseen backup cannot prove the strike lapsed"
    );
    assert_eq!(rig.endpoint.bodies().len(), 1, "nothing more was posted");
    let row = usage_row(&rig);
    assert_eq!(row["authDeadStrikes"], 1, "the strike is kept");
    assert_eq!(
        row["struckFingerprint"],
        json!(fingerprint(&later)),
        "the strike still names the backup's generation"
    );
}

#[test]
fn a_missing_backup_lets_a_strike_on_another_generation_lapse() {
    let (rig, _) = struck_on_the_backup("active_strike_backup_missing");
    fs::remove_file(rig.scratch.credential_file()).expect("the backup can be removed");

    let second = poll(&rig, &Usage::refusing(&[]));

    assert_eq!(
        second.result,
        PollResult::Deferred(DeferReason::Unattributable),
        "no stored source carries the struck generation, so the account is claimed again"
    );
    assert_eq!(
        rig.endpoint.bodies().len(),
        1,
        "the unattributed live grant is not spent"
    );
}

#[cfg(windows)]
#[test]
fn unreadable_live_file_under_the_locks_defers() {
    let stale = expired("fake-access-a", "fake-refresh-a");
    let mut rig = rig(
        "active_live_unreadable",
        &stale,
        &stale,
        Some(&logged_in_as_one()),
    );
    let live_path = rig.claude.credentials_file();
    let (releaser, holder) = unreadable_under_the_locks(&mut rig, live_path);

    let outcome = poll(&rig, &Usage::refusing(&[]));
    releaser.join().expect("the releaser finishes");
    release(&holder);

    assert_eq!(
        outcome.result,
        PollResult::Deferred(DeferReason::LiveUnreadable)
    );
    assert!(rig.endpoint.bodies().is_empty(), "nothing was posted");
    assert_eq!(live(&rig), stale, "the live file is untouched");
}

#[cfg(windows)]
#[test]
fn backup_unreadable_under_the_locks_defers() {
    let stale = expired("fake-access-a", "fake-refresh-a");
    let mut rig = rig(
        "active_backup_unreadable_locked",
        &stale,
        &stale,
        Some(&logged_in_as_one()),
    );
    let cred = rig.scratch.credential_file();
    let (releaser, holder) = unreadable_under_the_locks(&mut rig, cred);

    let outcome = poll(&rig, &Usage::refusing(&[]));
    releaser.join().expect("the releaser finishes");
    release(&holder);

    assert_eq!(
        outcome.result,
        PollResult::Deferred(DeferReason::BackupUnreadable)
    );
    assert!(rig.endpoint.bodies().is_empty(), "nothing was posted");
    assert_eq!(live(&rig), stale, "the live file is untouched");
}

#[cfg(windows)]
#[test]
fn unreadable_stash_defers_the_adoption() {
    let stale = expired("fake-access-a", "fake-refresh-a");
    let rig = rig(
        "active_stash_unreadable",
        &stale,
        &stale,
        Some(&logged_in_as_one()),
    );
    let manifest = rig.scratch.manifest_path();
    fs::create_dir_all(manifest.parent().expect("the manifest has a folder"))
        .expect("the stash folder can be made");
    fs::write(
        &manifest,
        json!({ "schemaVersion": 1, "entries": {} }).to_string(),
    )
    .expect("the manifest can be written");
    let held = hold_open(&manifest);

    let outcome = poll(&rig, &Usage::refusing(&[]));
    drop(held);

    assert_eq!(
        outcome.result,
        PollResult::Deferred(DeferReason::StashUnreadable)
    );
    assert!(
        rig.endpoint.bodies().is_empty(),
        "the live generation may be spent already, so nothing is posted"
    );
    assert_eq!(live(&rig), stale, "the live file is untouched");
}

#[cfg(windows)]
#[test]
fn refused_stash_adoption_defers_as_store_unwritable() {
    let stale = expired("fake-access-a", "fake-refresh-a");
    let mut rig = rig(
        "active_stash_refused",
        &stale,
        &stale,
        Some(&logged_in_as_one()),
    );
    let held_paths = vec![rig.scratch.credential_file(), rig.claude.credentials_file()];
    let holder = hold_during_post(&mut rig, held_paths);
    let first = poll(&rig, &Usage::refusing(&[]));
    release(&holder);
    assert_eq!(first.result, PollResult::LiveWriteFailed);
    assert_eq!(stash_rows(&rig), 1, "the successor was stashed");

    rig.store =
        Store::open_with_canary(rig.scratch.root(), || false).expect("the scratch store opens");
    make_due(&rig);
    let second = poll(&rig, &Usage::refusing(&[]));

    assert_eq!(
        second.result,
        PollResult::Deferred(DeferReason::StoreUnwritable)
    );
    assert_eq!(
        rig.endpoint.posted_refresh_tokens(),
        ["fake-refresh-a"],
        "the spent grant is not posted again"
    );
    assert_eq!(
        second.backup_written,
        Some(false),
        "the backup refused the stashed successor"
    );
    assert_eq!(stash_rows(&rig), 1, "the row stays for the next pass");
    assert_eq!(live(&rig), stale, "the live file is untouched");
}
