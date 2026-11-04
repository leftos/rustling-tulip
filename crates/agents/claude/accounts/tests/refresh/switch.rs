//! Switching the live login to a managed account: the target's credential and `oauthAccount` made live under cswap's
//! and Claude Code's locks, the outgoing credential and config backed up or stashed as the profile oracle classifies
//! them, and a failed write rolled back byte for byte.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use claude_accounts::AutoSwitchSettings;
use claude_accounts::{
    AccessToken, AccountLock, CredentialBlob, FileLock, LockError, LockTiming, LoginState,
    OutgoingProbe, RawUsage, Store, SwitchError, SwitchErrorKind, SwitchOutcome,
    SwitchPrecondition, TransportError, UsageTransport, credential_fingerprint,
    poll_once_with_refresh, probe_outgoing, switch,
};
use serde::Serialize as _;
use serde_json::{Value, json};

use crate::links::symlink_or_skip;
use crate::support::{ONE, ONE_ORG, Rig, Scratch, credential, ms_after, profile};
use crate::windows_files::{hold_open, temp_files};

/// Account 1's uuid in the usage fixture.
const ONE_UUID: &str = "00000000-0000-4000-8000-000000000011";
const TWO: &str = "two@example.test";
/// A uuid, email and organization no managed account has.
const STRANGER_UUID: &str = "00000000-0000-4000-8000-0000000000ff";
const STRANGER: &str = "stranger@example.test";
/// `FILE_SHARE_READ`: a handle that lets others read the file, but not replace it.
const FILE_SHARE_READ: u32 = 0x1;
/// Every fake token the suite writes: none may appear in an outcome's or an error's `Debug` or text.
const FAKE_TOKENS: [&str; 3] = ["fake-access", "fake-refresh", "fake-device"];
/// What a refusal of a live `primaryApiKey` no managed account backs up says.
const API_KEY_MESSAGE: &str = "the live login holds an API key no managed account backs up, which the switch would drop; nothing was written";

fn fresh(access: &str, refresh: &str) -> String {
    credential(access, Some(refresh), ms_after(3600))
}

/// Account 2's backed-up `oauthAccount`.
fn two_oauth_account() -> Value {
    json!({
        "emailAddress": TWO,
        "organizationUuid": "",
        "accountUuid": "00000000-0000-4000-8000-000000000012",
        "displayName": "Two",
    })
}

/// `~/.claude.json` logged in as `email` in `org`, with keys either side of `oauthAccount` that a switch keeps in place.
fn global_config(email: &str, org: &str) -> Value {
    json!({
        "numStartups": 7,
        "oauthAccount": { "emailAddress": email, "organizationUuid": org },
        "projects": { "X:/dev": { "allowedTools": [] } },
        "zLast": true,
    })
}

/// A rig logged in as account 1 whose live file holds `live`; account 1's backup holds `one_backup`, account 2's holds
/// `two_backup` and its config backup [`two_oauth_account`].
fn rig(test: &str, live: &str, one_backup: &str, two_backup: &str) -> Rig {
    let scratch = Scratch::new(test);
    crate::support::put_credentials(&scratch.root(), one_backup);
    let store = Store::open(scratch.root()).expect("the scratch store opens");
    let lock = AccountLock::acquire(&scratch.root()).expect("the account lock is free");
    store
        .write_credentials(&lock, 2, &CredentialBlob::new(two_backup.to_owned()))
        .expect("account 2's credential can be written");
    drop(lock);
    let config_backup = scratch.root().config_path(2);
    fs::create_dir_all(
        config_backup
            .parent()
            .expect("a config backup has a folder"),
    )
    .expect("the configs folder can be made");
    fs::write(
        &config_backup,
        json!({ "oauthAccount": two_oauth_account() }).to_string(),
    )
    .expect("account 2's config backup can be written");
    write_global(&scratch.dir, &global_config(ONE, ONE_ORG));
    let claude = scratch.claude();
    fs::create_dir_all(scratch.dir.join("claude")).expect("Claude Code's folder can be made");
    fs::write(claude.credentials_file(), live).expect("the live credential can be written");
    Rig::new(scratch, crate::support::FakeEndpoint::answering(Vec::new()))
}

fn write_global(dir: &Path, config: &Value) {
    let text = serde_json::to_string_pretty(config).expect("the global config serializes");
    fs::write(dir.join(".claude.json"), text).expect("the global config can be written");
}

/// `value` as no serde writer writes it: four-space indents and trailing whitespace.
fn non_canonical(value: &Value) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut serializer = serde_json::Serializer::with_formatter(
        &mut bytes,
        serde_json::ser::PrettyFormatter::with_indent(b"    "),
    );
    value
        .serialize(&mut serializer)
        .expect("the value serializes");
    bytes.extend_from_slice(b"  \n\n");
    bytes
}

/// Rewrites the live credential and `~/.claude.json` with non-canonical bytes, so only a byte-exact rollback restores
/// them; returns those bytes.
fn make_non_canonical(rig: &Rig) -> (Vec<u8>, Vec<u8>) {
    let live = non_canonical(&json_of(&live(rig)));
    let config = non_canonical(&global(rig));
    fs::write(rig.claude.credentials_file(), &live).expect("the live credential can be written");
    fs::write(rig.claude.global_config(), &config).expect("the global config can be written");
    (live, config)
}

/// Changes `accounts.json` as `edit` says.
fn edit_sequence(rig: &Rig, edit: impl FnOnce(&mut Value)) {
    let path = rig.scratch.backup().join("accounts.json");
    let mut sequence = json_of(&fs::read_to_string(&path).expect("accounts.json can be read"));
    edit(&mut sequence);
    fs::write(&path, sequence.to_string()).expect("accounts.json can be written");
}

/// A switch to `number` with a probe that asked nothing.
fn switch_to(rig: &Rig, number: u32) -> Result<SwitchOutcome, SwitchError> {
    switch(&rig.context(), number, &OutgoingProbe::unresolved(), None)
}

/// A switch to `number` after asking the oracle about the live credential.
fn probed_switch_to(rig: &Rig, number: u32) -> Result<SwitchOutcome, SwitchError> {
    let probe = probe_outgoing(&rig.context());
    switch(&rig.context(), number, &probe, None)
}

fn live(rig: &Rig) -> String {
    fs::read_to_string(rig.claude.credentials_file()).expect("the live credential can be read")
}

fn json_of(text: &str) -> Value {
    serde_json::from_str(text).expect("the text is JSON")
}

fn global(rig: &Rig) -> Value {
    json_of(&fs::read_to_string(rig.claude.global_config()).expect("the global config can be read"))
}

fn keys(value: &Value) -> Vec<String> {
    value
        .as_object()
        .expect("the value is an object")
        .keys()
        .cloned()
        .collect()
}

fn stored(rig: &Rig, number: u32) -> Option<String> {
    rig.store
        .read_credentials(number)
        .expect("the stored credential can be read")
        .map(|blob| blob.expose().to_owned())
}

fn active(rig: &Rig) -> Option<u32> {
    rig.store
        .read_sequence()
        .expect("the roster reads")
        .expect("the roster exists")
        .active()
}

/// Account 1's config backup, `None` when there is none.
fn one_config_backup(rig: &Rig) -> Option<Vec<u8>> {
    fs::read(rig.scratch.root().config_path(1)).ok()
}

/// Every file under the rig's folder with its bytes, lock files aside: taking a lock creates its file.
fn files(rig: &Rig) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut found = BTreeMap::new();
    collect(&rig.scratch.dir, &mut found);
    found
}

fn collect(dir: &Path, found: &mut BTreeMap<PathBuf, Vec<u8>>) {
    for entry in fs::read_dir(dir).expect("a scratch folder can be listed") {
        let path = entry.expect("an entry can be read").path();
        if path.is_dir() {
            collect(&path, found);
        } else if path.extension().is_none_or(|extension| extension != "lock") {
            found.insert(
                path.clone(),
                fs::read(&path).expect("a scratch file can be read"),
            );
        }
    }
}

/// Account 1's credential files: its backup and the `.prev` generation kept beside it.
fn one_credential_files(rig: &Rig) -> BTreeMap<PathBuf, Vec<u8>> {
    files(rig)
        .into_iter()
        .filter(|(path, _)| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("1.cred"))
        })
        .collect()
}

fn stash_rows(rig: &Rig) -> Vec<(String, Value)> {
    if !rig.scratch.manifest_path().exists() {
        return Vec::new();
    }
    let manifest = rig.scratch.manifest();
    let entries = manifest["entries"]
        .as_object()
        .expect("the manifest has entries");
    entries
        .iter()
        .map(|(id, row)| (id.clone(), row.clone()))
        .collect()
}

/// Opens `path` sharing only reads: it can be read, but no write replaces it until the handle drops.
fn readable_only(path: &Path) -> File {
    OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(path)
        .expect("the file opens sharing reads")
}

fn account_of(account: Option<&claude_accounts::SwitchAccount>) -> Option<(Option<u32>, String)> {
    account.map(|account| (account.number, account.email.as_str().to_owned()))
}

/// A rig whose live login is [`STRANGER`], no managed account's, with `live` in the live file and `target` as account
/// 2's backup; account 1's backup holds another credential.
fn stranger_rig(test: &str, live: &str, target: &str) -> Rig {
    let rig = rig(
        test,
        live,
        &fresh("fake-access-a", "fake-refresh-a"),
        target,
    );
    write_global(&rig.scratch.dir, &global_config(STRANGER, "org-stranger"));
    rig
}

/// Every file under the account store but `accounts.json` and the stash's: what an outgoing backup would change.
fn backups(rig: &Rig) -> BTreeMap<PathBuf, Vec<u8>> {
    let backup = rig.scratch.backup();
    files(rig)
        .into_iter()
        .filter(|(path, _)| {
            let name = path.to_string_lossy();
            path.starts_with(&backup)
                && !name.ends_with("accounts.json")
                && !name.contains(".unclaimed-")
        })
        .collect()
}

/// The oracle's answer naming an account no slot has.
fn stranger_profile() -> Result<claude_accounts::RawProfile, TransportError> {
    profile(STRANGER_UUID, STRANGER, STRANGER_UUID)
}

/// A usage endpoint that answers every token with a reading.
struct Readings;

impl UsageTransport for Readings {
    fn get_usage(&self, _token: &AccessToken) -> Result<RawUsage, TransportError> {
        Ok(RawUsage {
            status: 200,
            body: json!({ "five_hour": { "utilization": 10.0 }, "seven_day": { "utilization": 20.0 } }),
            retry_after: None,
        })
    }
}

#[test]
fn switch_writes_target_credentials_and_splices_oauth_account_keeping_other_keys() {
    let own = fresh("fake-access-a", "fake-refresh-a");
    let target = fresh("fake-access-two", "fake-refresh-two");
    let rig = rig("switch_writes_target", &own, &own, &target);
    let one_before = one_credential_files(&rig);

    let outcome = switch_to(&rig, 2).expect("the switch succeeds");

    assert!(outcome.switched);
    assert_eq!(outcome.reason, "switched");
    assert_eq!(outcome.strategy, "direct");
    assert_eq!(
        account_of(outcome.from.as_ref()),
        Some((Some(1), ONE.to_owned()))
    );
    assert_eq!(
        account_of(Some(&outcome.to)),
        Some((Some(2), TWO.to_owned()))
    );
    assert_eq!(outcome.message, "Switched to Account-2 (two@example.test)");
    assert!(outcome.warnings.is_empty(), "got {:?}", outcome.warnings);
    assert_eq!(
        json_of(&live(&rig))["claudeAiOauth"],
        json_of(&target)["claudeAiOauth"],
        "the target's login is live"
    );
    let config = global(&rig);
    assert_eq!(
        keys(&config),
        ["numStartups", "oauthAccount", "projects", "zLast"],
        "every key keeps its place"
    );
    assert_eq!(config["oauthAccount"], two_oauth_account());
    let before = global_config(ONE, ONE_ORG);
    for key in ["numStartups", "projects", "zLast"] {
        assert_eq!(config[key], before[key], "{key} is kept");
    }
    assert_eq!(active(&rig), Some(2));
    assert_eq!(
        one_credential_files(&rig),
        one_before,
        "an unchanged outgoing credential is not rewritten, and no .prev appears"
    );
    assert_eq!(
        stored(&rig, 2),
        Some(target),
        "the target's backup is left as it was"
    );
}

#[test]
fn switch_clears_primary_api_key() {
    let own = fresh("fake-access-a", "fake-refresh-a");
    let rig = rig(
        "switch_clears_primary_key",
        &own,
        &own,
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    let mut config = global_config(ONE, ONE_ORG);
    config["primaryApiKey"] = json!("sk-ant-fake-key");
    write_global(&rig.scratch.dir, &config);

    switch_to(&rig, 2).expect("the switch succeeds");

    let config = global(&rig);
    assert!(
        config.get("primaryApiKey").is_none(),
        "a managed API key would be billed instead of the login"
    );
    assert_eq!(
        keys(&config),
        ["numStartups", "oauthAccount", "projects", "zLast"]
    );
}

#[test]
fn switch_refreshes_the_outgoing_accounts_config_backup() {
    let own = fresh("fake-access-a", "fake-refresh-a");
    let rig = rig(
        "switch_refreshes_config_backup",
        &own,
        &own,
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    let (_, config_before) = make_non_canonical(&rig);
    assert_eq!(
        one_config_backup(&rig),
        None,
        "the fixture has no config backup for account 1"
    );

    switch_to(&rig, 2).expect("the switch succeeds");

    let backup: Value = serde_json::from_slice(
        &one_config_backup(&rig).expect("the outgoing account's config backup is written"),
    )
    .expect("it is JSON");
    let live_config: Value =
        serde_json::from_slice(&config_before).expect("the live config is JSON");
    assert_eq!(
        backup,
        json!({"oauthAccount": live_config["oauthAccount"]}),
        "the outgoing account's identity alone, not the whole ~/.claude.json"
    );
}

#[test]
fn switch_merges_live_shared_keys_and_drops_the_targets() {
    let live_text = json!({
        "claudeAiOauth": { "accessToken": "fake-access-a", "refreshToken": "fake-refresh-a", "expiresAt": ms_after(3600) },
        "pluginSecrets": { "live": "p" },
        "mcpOAuth": { "live": "m" },
        "trustedDeviceToken": "fake-device-one",
        "mcpOAuthClientConfig": { "live": "c" },
        "mcpXaaIdpConfig": { "live": "x" },
    })
    .to_string();
    let target = json!({
        "claudeAiOauth": { "accessToken": "fake-access-two", "refreshToken": "fake-refresh-two", "expiresAt": ms_after(3600) },
        "mcpOAuth": { "stale": "m" },
        "trustedDeviceToken": "fake-device-two",
        "mcpXaaIdp": { "stale": "i" },
        "pluginSecrets": { "stale": "p" },
        "zTarget": 1,
    })
    .to_string();
    let rig = rig("switch_merges_shared_keys", &live_text, &live_text, &target);

    switch_to(&rig, 2).expect("the switch succeeds");

    let written = json_of(&live(&rig));
    assert_eq!(
        keys(&written),
        [
            "claudeAiOauth",
            "trustedDeviceToken",
            "zTarget",
            "pluginSecrets",
            "mcpOAuth",
            "mcpOAuthClientConfig",
            "mcpXaaIdpConfig"
        ],
        "the target's own keys keep their order, the live shared keys follow in theirs"
    );
    let (live_value, target_value) = (json_of(&live_text), json_of(&target));
    assert_eq!(written["claudeAiOauth"], target_value["claudeAiOauth"]);
    assert_eq!(
        written["trustedDeviceToken"], "fake-device-two",
        "an account-bound key stays the target's"
    );
    assert_eq!(
        written["zTarget"], 1,
        "a key cswap does not know stays the target's"
    );
    for key in [
        "pluginSecrets",
        "mcpOAuth",
        "mcpOAuthClientConfig",
        "mcpXaaIdpConfig",
    ] {
        assert_eq!(written[key], live_value[key], "{key} is the live file's");
    }
    assert!(
        written.get("mcpXaaIdp").is_none(),
        "a shared key the live file lacks is not resurrected from the target"
    );
}

#[test]
fn switch_backs_up_an_own_rotated_outgoing_credential() {
    let rotated = credential("fake-access-b", Some("fake-refresh-b"), ms_after(7200));
    let old = fresh("fake-access-a", "fake-refresh-a");
    let mut rig = rig(
        "switch_backs_up_own_rotated",
        &rotated,
        &old,
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    rig.profile = crate::support::FakeProfile::answering(vec![profile(ONE_UUID, ONE, ONE_ORG)]);

    let outcome = probed_switch_to(&rig, 2).expect("the switch succeeds");

    assert!(outcome.switched);
    assert_eq!(
        rig.profile.asked(),
        ["fake-access-b"],
        "the oracle is asked about the live token, before the switch"
    );
    assert_eq!(
        stored(&rig, 1),
        Some(rotated),
        "the rotated credential is account 1's backup"
    );
    assert!(stash_rows(&rig).is_empty(), "nothing is stashed");
    assert!(outcome.warnings.is_empty(), "got {:?}", outcome.warnings);
}

#[test]
fn switch_backfills_the_outgoing_accounts_uuid() {
    let rotated = credential("fake-access-b", Some("fake-refresh-b"), ms_after(7200));
    let old = fresh("fake-access-a", "fake-refresh-a");
    let mut rig = rig(
        "switch_backfills_uuid",
        &rotated,
        &old,
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    edit_sequence(&rig, |sequence| {
        sequence["accounts"]["1"]["uuid"] = json!("");
    });
    rig.profile = crate::support::FakeProfile::answering(vec![profile(ONE_UUID, ONE, ONE_ORG)]);

    probed_switch_to(&rig, 2).expect("the switch succeeds");

    let sequence = rig
        .store
        .read_sequence()
        .expect("the roster reads")
        .expect("the roster exists");
    assert_eq!(
        sequence.account(1).expect("account 1 is kept").uuid,
        ONE_UUID,
        "the oracle's uuid is recorded"
    );
    assert_eq!(sequence.active(), Some(2));
    assert_eq!(stored(&rig, 1), Some(rotated));
}

#[test]
fn switch_keeps_a_newer_stored_backup_over_an_older_own_rotated_outgoing_credential() {
    let older = credential("fake-access-b", Some("fake-refresh-b"), ms_after(60));
    let newer = fresh("fake-access-a", "fake-refresh-a");
    let mut rig = rig(
        "switch_keeps_newer_own_rotated",
        &older,
        &newer,
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    rig.profile = crate::support::FakeProfile::answering(vec![profile(ONE_UUID, ONE, ONE_ORG)]);

    let outcome = probed_switch_to(&rig, 2).expect("the switch succeeds");

    assert!(outcome.switched);
    assert_eq!(
        stored(&rig, 1),
        Some(newer),
        "the newer stored backup is kept"
    );
    assert!(stash_rows(&rig).is_empty(), "nothing is stashed");
    assert!(outcome.warnings.is_empty(), "got {:?}", outcome.warnings);
}

#[test]
fn switch_keeps_a_newer_stored_backup_over_an_older_unresolved_outgoing_credential() {
    let older = credential("fake-access-b", Some("fake-refresh-b"), ms_after(60));
    let newer = fresh("fake-access-a", "fake-refresh-a");
    let rig = rig(
        "switch_keeps_newer_unresolved",
        &older,
        &newer,
        &fresh("fake-access-two", "fake-refresh-two"),
    );

    let outcome = switch_to(&rig, 2).expect("the switch succeeds");

    assert!(outcome.switched);
    assert_eq!(
        stored(&rig, 1),
        Some(newer),
        "the newer stored backup is kept"
    );
    assert!(stash_rows(&rig).is_empty(), "nothing is stashed");
}

#[test]
fn switch_keeps_a_stored_backup_with_no_expiry_over_another_lineage() {
    let outgoing = fresh("fake-access-b", "fake-refresh-b");
    let successor =
        r#"{"claudeAiOauth":{"accessToken":"fake-access-a","refreshToken":"fake-refresh-a"}}"#
            .to_owned();
    let mut rig = rig(
        "switch_keeps_no_expiry",
        &outgoing,
        &successor,
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    rig.profile = crate::support::FakeProfile::answering(vec![profile(ONE_UUID, ONE, ONE_ORG)]);

    let outcome = probed_switch_to(&rig, 2).expect("the switch succeeds");

    assert!(outcome.switched);
    assert_eq!(
        stored(&rig, 1),
        Some(successor),
        "a stored copy whose age cannot be told is kept"
    );
    assert!(stash_rows(&rig).is_empty(), "nothing is stashed");
}

#[test]
fn switch_still_writes_the_config_backup_and_uuid_when_it_keeps_the_stored_credential() {
    let older = credential("fake-access-b", Some("fake-refresh-b"), ms_after(60));
    let newer = fresh("fake-access-a", "fake-refresh-a");
    let mut rig = rig(
        "switch_keeps_but_backs_up_config",
        &older,
        &newer,
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    edit_sequence(&rig, |sequence| {
        sequence["accounts"]["1"]["uuid"] = json!("");
    });
    rig.profile = crate::support::FakeProfile::answering(vec![profile(ONE_UUID, ONE, ONE_ORG)]);

    probed_switch_to(&rig, 2).expect("the switch succeeds");

    assert_eq!(
        stored(&rig, 1),
        Some(newer),
        "the newer stored backup is kept"
    );
    let sequence = rig
        .store
        .read_sequence()
        .expect("the roster reads")
        .expect("the roster exists");
    assert_eq!(
        sequence.account(1).expect("account 1 is kept").uuid,
        ONE_UUID,
        "the oracle's uuid is recorded"
    );
    let config: Value = serde_json::from_slice(
        &one_config_backup(&rig).expect("account 1's config backup is written"),
    )
    .expect("it is JSON");
    assert_eq!(
        config["oauthAccount"]["emailAddress"], ONE,
        "the config backup is account 1's login"
    );
}

#[test]
fn switch_stashes_a_foreign_outgoing_credential_and_does_not_write_the_backup() {
    let foreign = fresh("fake-access-x", "fake-refresh-x");
    let old = fresh("fake-access-a", "fake-refresh-a");
    let target = fresh("fake-access-two", "fake-refresh-two");
    let mut rig = rig("switch_stashes_foreign", &foreign, &old, &target);
    rig.profile = crate::support::FakeProfile::answering(vec![stranger_profile()]);

    let outcome = probed_switch_to(&rig, 2).expect("the switch succeeds");

    assert!(outcome.switched);
    assert_eq!(
        stored(&rig, 1),
        Some(old),
        "account 1's backup is not overwritten"
    );
    assert_eq!(
        one_config_backup(&rig),
        None,
        "nor is its config backup written"
    );
    let rows = stash_rows(&rig);
    assert_eq!(rows.len(), 1, "the foreign credential is stashed once");
    let (id, row) = &rows[0];
    assert_eq!(row["reason"], "foreign");
    assert_eq!(row["configSlot"], "1");
    assert_eq!(
        rig.scratch.stash_entry(id).as_deref(),
        Some(foreign.as_str()),
        "the overwritten bytes are kept"
    );
    assert_eq!(
        outcome.warnings.len(),
        1,
        "the stash is reported: {:?}",
        outcome.warnings
    );
    assert_eq!(
        json_of(&live(&rig))["claudeAiOauth"],
        json_of(&target)["claudeAiOauth"],
        "the target is live"
    );
}

/// An outgoing backup that exists and does not open is not an absent one: the switch stops before stashing or writing
/// anything, even where the oracle would classify the live credential as another login's.
#[test]
fn an_unopenable_outgoing_backup_stops_the_switch_before_a_stash_or_write() {
    let foreign = fresh("fake-access-x", "fake-refresh-x");
    let old = fresh("fake-access-a", "fake-refresh-a");
    let mut rig = rig(
        "switch_outgoing_unreadable",
        &foreign,
        &old,
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    rig.profile = crate::support::FakeProfile::answering(vec![stranger_profile()]);
    let backup = rig.scratch.root().credential_path(1);
    let mut corrupted = fs::read(&backup).expect("account 1's backup can be read");
    let last = corrupted.len() - 1;
    corrupted[last] ^= 0x01;
    fs::write(&backup, &corrupted).expect("account 1's backup can be damaged");
    let before = backups(&rig);
    let live_before = live_files(&rig);

    let error =
        probed_switch_to(&rig, 2).expect_err("an unreadable outgoing backup stops the switch");

    assert_eq!(
        rig.profile.asked(),
        ["fake-access-x"],
        "the oracle is still asked: the bytes cannot be compared"
    );
    assert_eq!(error.kind(), SwitchErrorKind::OutgoingBackupFailed);
    assert_eq!(error.kind().as_str(), "outgoing_backup_failed");
    assert_eq!(
        backups(&rig),
        before,
        "the unreadable backup is left byte for byte"
    );
    assert_eq!(
        live_files(&rig),
        live_before,
        "both live files are byte-unchanged"
    );
    assert!(stash_rows(&rig).is_empty(), "nothing is stashed");
}

/// A target credential that exists and does not open refuses the switch before the outgoing account's backup is
/// written: an absent target backup and an unreadable one are not the same.
#[test]
fn an_unopenable_target_credential_is_refused_before_the_outgoing_backup_is_written() {
    let rotated = credential("fake-access-b", Some("fake-refresh-b"), ms_after(7200));
    let old = fresh("fake-access-a", "fake-refresh-a");
    let rig = rig(
        "switch_target_credential_unreadable",
        &rotated,
        &old,
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    let target = rig.scratch.root().credential_path(2);
    let mut corrupted = fs::read(&target).expect("account 2's credential can be read");
    let last = corrupted.len() - 1;
    corrupted[last] ^= 0x01;
    fs::write(&target, &corrupted).expect("account 2's credential can be damaged");
    let before = backups(&rig);
    let live_before = live_files(&rig);

    let error = switch_to(&rig, 2).expect_err("an unreadable target credential is refused");

    assert_eq!(error.kind(), SwitchErrorKind::TargetBackupUnreadable);
    assert_eq!(error.kind().as_str(), "target_backup_unreadable");
    assert_eq!(
        backups(&rig),
        before,
        "the outgoing backup is not written before the refusal"
    );
    assert_eq!(
        live_files(&rig),
        live_before,
        "both live files are byte-unchanged"
    );
}

#[test]
fn a_lineage_known_as_foreign_is_stashed() {
    let foreign = fresh("fake-access-x", "fake-refresh-x");
    let old = fresh("fake-access-a", "fake-refresh-a");
    let mut rig = rig(
        "switch_known_foreign",
        &foreign,
        &old,
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    fs::remove_file(rig.scratch.backup().join("cache").join("usage.json"))
        .expect("the usage table can be removed");
    rig.profile = crate::support::FakeProfile::answering(vec![stranger_profile()]);
    // A poll serves the live token and the oracle names another account: the verdict is remembered.
    poll_once_with_refresh(&Readings, &AutoSwitchSettings::default(), &rig.context())
        .expect("the poll runs");
    assert_eq!(rig.profile.asked(), ["fake-access-x"]);

    let outcome = switch_to(&rig, 2).expect("the switch succeeds without asking the oracle");

    assert_eq!(
        stored(&rig, 1),
        Some(old),
        "account 1's backup is not overwritten"
    );
    let rows = stash_rows(&rig);
    assert_eq!(rows.len(), 1, "the credential is stashed once");
    assert_eq!(rows[0].1["reason"], "foreign");
    assert_eq!(outcome.warnings.len(), 1, "got {:?}", outcome.warnings);
    assert!(
        outcome.warnings[0].contains("previously identified"),
        "got {:?}",
        outcome.warnings
    );
}

#[test]
fn a_wiped_outgoing_credential_is_not_backed_up() {
    let wiped = credential("", Some(""), ms_after(-3600));
    let old = fresh("fake-access-a", "fake-refresh-a");
    let rig = rig(
        "switch_wiped",
        &wiped,
        &old,
        &fresh("fake-access-two", "fake-refresh-two"),
    );

    let outcome = switch_to(&rig, 2).expect("the switch succeeds");

    assert!(outcome.switched);
    assert_eq!(
        stored(&rig, 1),
        Some(old),
        "the backup keeps the account's only refresh token"
    );
    assert!(stash_rows(&rig).is_empty(), "nothing is stashed");
    assert!(
        one_config_backup(&rig).is_some(),
        "the config backup is still written"
    );
    assert_eq!(outcome.warnings.len(), 1, "got {:?}", outcome.warnings);
    assert!(
        outcome.warnings[0].contains("wiped"),
        "got {:?}",
        outcome.warnings
    );
}

#[test]
fn a_failed_stash_aborts_the_switch_with_nothing_live_written() {
    let foreign = fresh("fake-access-x", "fake-refresh-x");
    let old = fresh("fake-access-a", "fake-refresh-a");
    let mut rig = rig(
        "switch_stash_fails",
        &foreign,
        &old,
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    rig.profile = crate::support::FakeProfile::answering(vec![stranger_profile()]);
    let manifest = rig.scratch.manifest_path();
    fs::create_dir_all(manifest.parent().expect("the manifest has a folder"))
        .expect("the stash folder can be made");
    fs::write(
        &manifest,
        json!({ "schemaVersion": 1, "entries": {} }).to_string(),
    )
    .expect("the manifest can be written");
    let before = files(&rig);
    let probe = probe_outgoing(&rig.context());

    let held = hold_open(&manifest);
    let error =
        switch(&rig.context(), 2, &probe, None).expect_err("a stash that fails stops the switch");
    drop(held);

    assert_eq!(error.kind(), SwitchErrorKind::OutgoingBackupFailed);
    assert_eq!(error.kind().as_str(), "outgoing_backup_failed");
    assert_eq!(
        files(&rig),
        before,
        "no live file, stash entry, backup or accounts.json was written"
    );
}

/// A foreign outgoing credential is stashed and no config backup is written, so the splice would drop the live
/// `primaryApiKey` with no copy anywhere: the switch stops first.
#[test]
fn a_managed_switch_from_a_foreign_login_holding_an_api_key_is_refused_with_nothing_written() {
    let foreign = fresh("fake-access-x", "fake-refresh-x");
    let old = fresh("fake-access-a", "fake-refresh-a");
    let mut rig = rig(
        "switch_foreign_api_key",
        &foreign,
        &old,
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    rig.profile = crate::support::FakeProfile::answering(vec![stranger_profile()]);
    let mut config = global_config(ONE, ONE_ORG);
    config["primaryApiKey"] = json!("sk-ant-fake-key");
    write_global(&rig.scratch.dir, &config);
    let before = files(&rig);

    let error = probed_switch_to(&rig, 2)
        .expect_err("a live API key that no file would keep is not dropped");

    assert_eq!(error.kind(), SwitchErrorKind::ApiKeyAccount);
    assert_eq!(error.kind().to_string(), API_KEY_MESSAGE);
    assert_eq!(error.login_state(), None);
    assert_eq!(
        files(&rig),
        before,
        "no stash row, backup, live file or accounts.json was written"
    );
}

/// An API-key record is no account the switch knows: refused as unknown before any lock, with the live files, every
/// backup and `accounts.json` left byte for byte.
#[test]
fn a_switch_to_an_api_key_account_is_refused_as_unknown_with_nothing_written() {
    let own = fresh("fake-access-a", "fake-refresh-a");
    let rig = rig(
        "switch_api_key",
        &own,
        &own,
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    edit_sequence(&rig, |sequence| {
        sequence["accounts"]["2"]["kind"] = json!("api_key");
    });
    let before = files(&rig);

    let error = switch_to(&rig, 2).expect_err("a switch to an API-key account is refused");

    assert_eq!(error.kind(), SwitchErrorKind::UnknownAccount);
    assert_eq!(error.kind().as_str(), "unknown_account");
    assert_eq!(error.account(), 2);
    assert_eq!(
        files(&rig),
        before,
        "no live file, backup or accounts.json was written"
    );
    assert!(
        !rig.scratch.root().consume_lock(2).exists(),
        "no lock is taken"
    );
}

/// A live login that is an API-key account is no managed account's, so the switch activates the target directly, and
/// the `primaryApiKey` cswap made live beside it, which the splice would drop with no copy kept, stops it.
#[test]
fn a_switch_from_a_live_api_key_account_holding_its_key_is_refused_with_nothing_written() {
    let own = fresh("fake-access-a", "fake-refresh-a");
    let rig = rig(
        "switch_from_api_key",
        &own,
        &own,
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    edit_sequence(&rig, |sequence| {
        sequence["accounts"]["1"]["kind"] = json!("api_key");
    });
    let mut config = global_config(ONE, ONE_ORG);
    config["primaryApiKey"] = json!("sk-ant-fake-key");
    write_global(&rig.scratch.dir, &config);
    let before = files(&rig);

    let error =
        switch_to(&rig, 2).expect_err("a live API key that no file would keep is not dropped");

    assert_eq!(error.kind(), SwitchErrorKind::ApiKeyAccount);
    assert_eq!(error.kind().to_string(), API_KEY_MESSAGE);
    assert_eq!(
        files(&rig),
        before,
        "no stash row, backup, live file or accounts.json was written"
    );
}

/// A live login that is an API-key account, with no `primaryApiKey` beside it, is displaced as no managed account's:
/// its credential is stashed and never written into the API-key account's backup.
#[test]
fn a_switch_from_a_live_api_key_account_without_its_key_stashes_and_activates() {
    let own = fresh("fake-access-a", "fake-refresh-a");
    let rig = rig(
        "switch_from_api_key_no_key",
        &own,
        &own,
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    edit_sequence(&rig, |sequence| {
        sequence["accounts"]["1"]["kind"] = json!("api_key");
    });
    write_global(&rig.scratch.dir, &global_config(ONE, ONE_ORG));
    let credential_backup = one_credential_files(&rig);
    let config_backup = one_config_backup(&rig);

    let outcome = switch_to(&rig, 2).expect("the target is activated directly");

    assert!(outcome.switched);
    assert_eq!(
        account_of(outcome.from.as_ref()),
        Some((None, ONE.to_owned())),
        "the login is named, as no account's"
    );
    let rows = stash_rows(&rig);
    assert_eq!(rows.len(), 1, "the displaced credential is stashed once");
    let (id, row) = &rows[0];
    assert_eq!(row["reason"], "displaced-live-login");
    assert_eq!(row["configSlot"], "unmanaged");
    assert_eq!(rig.scratch.stash_entry(id).as_deref(), Some(own.as_str()));
    assert_eq!(
        one_credential_files(&rig),
        credential_backup,
        "the API-key account's credential backup is untouched"
    );
    assert_eq!(
        one_config_backup(&rig),
        config_backup,
        "the API-key account's config backup is untouched"
    );
    assert_eq!(active(&rig), Some(2));
}

#[test]
fn switch_to_the_active_account_is_a_no_op() {
    let own = fresh("fake-access-a", "fake-refresh-a");
    let rig = rig(
        "switch_no_op",
        &own,
        &own,
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    let before = files(&rig);

    let outcome = switch_to(&rig, 1).expect("a switch to the active account answers");

    assert!(!outcome.switched);
    assert_eq!(outcome.reason, "already-active");
    assert_eq!(
        account_of(outcome.from.as_ref()),
        Some((Some(1), ONE.to_owned()))
    );
    assert_eq!(
        account_of(Some(&outcome.to)),
        Some((Some(1), ONE.to_owned()))
    );
    assert_eq!(outcome.message, "Already on Account-1 (one@example.test)");
    assert_eq!(files(&rig), before, "no file's bytes change");
    assert!(
        !rig.scratch.root().consume_lock(1).exists(),
        "no lock is taken"
    );
    assert!(
        !rig.claude.oauth_refresh_lock().exists(),
        "no Claude Code lock is taken"
    );
}

#[test]
fn switch_to_a_disabled_account_is_not_refused() {
    let own = fresh("fake-access-a", "fake-refresh-a");
    let rig = rig(
        "switch_to_disabled",
        &own,
        &own,
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    let lock = AccountLock::acquire(&rig.scratch.root()).expect("the account lock is free");
    assert!(
        rig.store
            .set_disabled(&lock, 2, true)
            .expect("account 2 can be disabled")
    );
    drop(lock);

    let outcome = switch_to(&rig, 2).expect("an explicit switch to a disabled account goes ahead");

    assert!(outcome.switched);
    assert_eq!(active(&rig), Some(2));
}

#[test]
fn switch_to_an_unknown_account_is_refused_with_nothing_written() {
    let own = fresh("fake-access-a", "fake-refresh-a");
    let rig = rig(
        "switch_unknown_account",
        &own,
        &own,
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    let before = files(&rig);

    let error = switch_to(&rig, 9).expect_err("account 9 does not exist");

    assert_eq!(error.kind(), SwitchErrorKind::UnknownAccount);
    assert_eq!(error.kind().as_str(), "unknown_account");
    assert_eq!(error.account(), 9);
    assert_eq!(files(&rig), before, "nothing was written");
}

#[test]
fn unmanaged_live_login_is_stashed_then_target_activated() {
    let displaced = fresh("fake-access-x", "fake-refresh-x");
    let target = fresh("fake-access-two", "fake-refresh-two");
    let rig = stranger_rig("direct_stashes_then_activates", &displaced, &target);

    let outcome = switch_to(&rig, 2).expect("the target is activated directly");

    assert!(outcome.switched);
    assert_eq!(outcome.reason, "switched");
    assert_eq!(outcome.message, "Switched to Account-2 (two@example.test)");
    assert_eq!(
        account_of(outcome.from.as_ref()),
        Some((None, STRANGER.to_owned())),
        "the login left is the stranger's, with no account number"
    );
    assert_eq!(
        account_of(Some(&outcome.to)),
        Some((Some(2), TWO.to_owned()))
    );
    assert!(outcome.warnings.is_empty(), "got {:?}", outcome.warnings);
    let rows = stash_rows(&rig);
    assert_eq!(rows.len(), 1, "the displaced credential is stashed once");
    let (id, row) = &rows[0];
    assert_eq!(row["reason"], "displaced-live-login");
    assert_eq!(row["configSlot"], "unmanaged");
    assert_eq!(row["consumedFp"], Value::Null, "keyed to no generation");
    assert_eq!(
        row["fingerprint"],
        json!(credential_fingerprint(&CredentialBlob::new(
            displaced.clone()
        )))
    );
    assert_eq!(
        rig.scratch.stash_entry(id).as_deref(),
        Some(displaced.as_str()),
        "the displaced bytes are kept"
    );
    assert_eq!(
        json_of(&live(&rig))["claudeAiOauth"],
        json_of(&target)["claudeAiOauth"],
        "the target's login is live"
    );
    let config = global(&rig);
    assert_eq!(
        keys(&config),
        ["numStartups", "oauthAccount", "projects", "zLast"],
        "every key keeps its place"
    );
    assert_eq!(config["oauthAccount"], two_oauth_account());
    let before = global_config(STRANGER, "org-stranger");
    for key in ["numStartups", "projects", "zLast"] {
        assert_eq!(config[key], before[key], "{key} is kept");
    }
    assert_eq!(active(&rig), Some(2));
}

#[test]
fn direct_activation_merges_shared_keys() {
    let live_text = json!({
        "claudeAiOauth": { "accessToken": "fake-access-x", "refreshToken": "fake-refresh-x", "expiresAt": ms_after(3600) },
        "pluginSecrets": { "live": "p" },
        "mcpOAuth": { "live": "m" },
    })
    .to_string();
    let target = json!({
        "claudeAiOauth": { "accessToken": "fake-access-two", "refreshToken": "fake-refresh-two", "expiresAt": ms_after(3600) },
        "mcpOAuth": { "stale": "m" },
        "trustedDeviceToken": "fake-device-two",
        "mcpXaaIdp": { "stale": "i" },
    })
    .to_string();
    let rig = stranger_rig("direct_merges_shared_keys", &live_text, &target);

    switch_to(&rig, 2).expect("the target is activated directly");

    let written = json_of(&live(&rig));
    assert_eq!(
        keys(&written),
        [
            "claudeAiOauth",
            "trustedDeviceToken",
            "pluginSecrets",
            "mcpOAuth"
        ],
        "the target's own keys keep their order, the live shared keys follow in theirs"
    );
    assert_eq!(written["claudeAiOauth"], json_of(&target)["claudeAiOauth"]);
    assert_eq!(
        written["trustedDeviceToken"], "fake-device-two",
        "an account-bound key stays the target's"
    );
    let live_value = json_of(&live_text);
    for key in ["pluginSecrets", "mcpOAuth"] {
        assert_eq!(written[key], live_value[key], "{key} is the live file's");
    }
    assert!(
        written.get("mcpXaaIdp").is_none(),
        "a shared key the live file lacks is not resurrected from the target"
    );
    // The primaryApiKey removal is pinned by missing_global_config_is_written_from_the_target_backup.
    let config = global(&rig);
    assert_eq!(
        keys(&config),
        ["numStartups", "oauthAccount", "projects", "zLast"]
    );
}

#[test]
fn direct_activation_writes_no_outgoing_backups() {
    let rig = stranger_rig(
        "direct_no_outgoing_backups",
        &fresh("fake-access-x", "fake-refresh-x"),
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    let before = backups(&rig);

    switch_to(&rig, 2).expect("the target is activated directly");

    assert_eq!(
        backups(&rig),
        before,
        "no credential or config backup was written for any account"
    );
    assert_eq!(one_config_backup(&rig), None);
}

#[test]
fn a_failed_stash_aborts_direct_activation_with_nothing_live_written() {
    let rig = stranger_rig(
        "direct_stash_fails",
        &fresh("fake-access-x", "fake-refresh-x"),
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    let manifest = rig.scratch.manifest_path();
    fs::create_dir_all(manifest.parent().expect("the manifest has a folder"))
        .expect("the stash folder can be made");
    fs::write(
        &manifest,
        json!({ "schemaVersion": 1, "entries": {} }).to_string(),
    )
    .expect("the manifest can be written");
    let before = files(&rig);

    let held = hold_open(&manifest);
    let error = switch_to(&rig, 2).expect_err("a stash that fails stops the switch");
    drop(held);

    assert_eq!(error.kind(), SwitchErrorKind::OutgoingBackupFailed);
    assert_eq!(error.login_state(), None, "no live file was written");
    assert_eq!(
        files(&rig),
        before,
        "no live file, stash entry, backup or accounts.json was written"
    );
}

#[test]
fn a_live_credential_equal_to_the_targets_is_not_stashed() {
    let target = fresh("fake-access-two", "fake-refresh-two");
    let rig = stranger_rig("direct_live_equals_target", &target, &target);

    let outcome = switch_to(&rig, 2).expect("the target is activated directly");

    assert!(outcome.switched);
    assert!(
        stash_rows(&rig).is_empty(),
        "the target's own bytes are not stashed"
    );
    assert_eq!(global(&rig)["oauthAccount"], two_oauth_account());
    assert_eq!(active(&rig), Some(2));
}

#[test]
fn direct_activation_rolls_back_byte_exact_on_a_failed_config_write() {
    let rig = stranger_rig(
        "direct_config_write_fails",
        &fresh("fake-access-x", "fake-refresh-x"),
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    let before = make_non_canonical(&rig);
    let held = readable_only(rig.claude.global_config());

    let error = switch_to(&rig, 2).expect_err("the config write fails");
    drop(held);

    assert_eq!(error.kind(), SwitchErrorKind::WriteFailed);
    assert_eq!(error.login_state(), Some(LoginState::Source));
    assert_eq!(
        live_files(&rig),
        before,
        "both live files are back, byte for byte"
    );
    assert_eq!(active(&rig), Some(1), "the active account is unchanged");
    assert!(
        temp_files(&rig.scratch.dir).is_empty(),
        "no temp file is left beside the config"
    );
    assert!(
        temp_files(&rig.scratch.dir.join("claude")).is_empty(),
        "no temp file is left beside the live file"
    );
}

#[test]
fn direct_activation_rolls_back_byte_exact_on_a_failed_sequence_write() {
    let rig = stranger_rig(
        "direct_sequence_write_fails",
        &fresh("fake-access-x", "fake-refresh-x"),
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    let before = make_non_canonical(&rig);
    let held = readable_only(&rig.scratch.backup().join("accounts.json"));

    let error = switch_to(&rig, 2).expect_err("the sequence write fails");
    drop(held);

    assert_eq!(error.kind(), SwitchErrorKind::WriteFailed);
    assert_eq!(error.login_state(), Some(LoginState::Source));
    assert_eq!(
        live_files(&rig),
        before,
        "both live files are back, byte for byte"
    );
    assert_eq!(
        global(&rig)["oauthAccount"]["emailAddress"],
        STRANGER,
        "the stranger is the login again"
    );
    assert_eq!(active(&rig), Some(1));
}

#[test]
fn direct_activation_rollback_removes_the_live_files_it_created() {
    let rig = rig(
        "direct_rollback_removes_created",
        &fresh("fake-access-x", "fake-refresh-x"),
        &fresh("fake-access-a", "fake-refresh-a"),
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    fs::remove_file(rig.claude.credentials_file()).expect("the live credential can be removed");
    fs::remove_file(rig.claude.global_config()).expect("the global config can be removed");
    let held = readable_only(&rig.scratch.backup().join("accounts.json"));

    let error = switch_to(&rig, 2).expect_err("the sequence write fails");
    drop(held);

    assert_eq!(error.kind(), SwitchErrorKind::WriteFailed);
    assert_eq!(error.login_state(), Some(LoginState::Source));
    assert!(
        !rig.claude.credentials_file().exists(),
        "the live credential the switch made is removed"
    );
    assert!(
        !rig.claude.global_config().exists(),
        "the global config the switch made is removed"
    );
    assert_eq!(active(&rig), Some(1));
}

#[test]
fn direct_activation_with_missing_target_backup_writes_nothing() {
    let rig = stranger_rig(
        "direct_target_missing",
        &fresh("fake-access-x", "fake-refresh-x"),
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    fs::remove_file(rig.scratch.root().config_path(2))
        .expect("account 2's config backup can be removed");
    let before = files(&rig);

    let error = switch_to(&rig, 2).expect_err("a target without a config backup is refused");

    assert_eq!(error.kind(), SwitchErrorKind::TargetBackupMissing);
    assert_eq!(
        files(&rig),
        before,
        "no stash row, backup or live file was written"
    );
}

#[test]
fn no_live_credential_activates_without_a_stash() {
    for empty in [false, true] {
        let target = fresh("fake-access-two", "fake-refresh-two");
        let rig = stranger_rig(&format!("direct_no_live_credential_{empty}"), "", &target);
        if !empty {
            fs::remove_file(rig.claude.credentials_file())
                .expect("the live credential can be removed");
        }

        let outcome = switch_to(&rig, 2).expect("the target is activated directly");

        assert!(outcome.switched, "empty file: {empty}");
        assert!(
            stash_rows(&rig).is_empty(),
            "nothing to stash; empty file: {empty}"
        );
        assert_eq!(
            live(&rig),
            target,
            "the target's stored credential, as stored; empty file: {empty}"
        );
        assert_eq!(global(&rig)["oauthAccount"], two_oauth_account());
        assert_eq!(active(&rig), Some(2));
    }
}

#[test]
fn missing_global_config_is_written_from_the_target_backup() {
    let rig = rig(
        "direct_missing_global_config",
        &fresh("fake-access-x", "fake-refresh-x"),
        &fresh("fake-access-a", "fake-refresh-a"),
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    fs::remove_file(rig.claude.global_config()).expect("the global config can be removed");
    let backup = json!({
        "numStartups": 3,
        "oauthAccount": two_oauth_account(),
        "primaryApiKey": "sk-ant-fake-key",
        "zKeep": 1,
    });
    fs::write(rig.scratch.root().config_path(2), backup.to_string())
        .expect("account 2's config backup can be written");

    let outcome = switch_to(&rig, 2).expect("the target is activated directly");

    assert!(outcome.switched);
    assert_eq!(outcome.from, None, "no live identity was displaced");
    let config = global(&rig);
    assert_eq!(
        keys(&config),
        ["oauthAccount"],
        "the target's identity alone: a config backup holds no other key of the live file"
    );
    assert_eq!(config["oauthAccount"], two_oauth_account());
    assert_eq!(active(&rig), Some(2));
}

#[test]
fn unreadable_global_config_is_refused_with_nothing_written() {
    let rig = rig(
        "direct_unreadable_global_config",
        &fresh("fake-access-x", "fake-refresh-x"),
        &fresh("fake-access-a", "fake-refresh-a"),
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    fs::write(rig.claude.global_config(), "not json").expect("the global config can be written");
    let before = files(&rig);

    let error = switch_to(&rig, 2).expect_err("an unparsable global config is refused");

    assert_eq!(error.kind(), SwitchErrorKind::ConfigUnreadable);
    assert_eq!(error.login_state(), None);
    assert_eq!(files(&rig), before, "nothing was written");
}

#[test]
fn no_oauth_account_stashes_under_the_active_number() {
    let displaced = fresh("fake-access-x", "fake-refresh-x");
    let rig = rig(
        "direct_no_oauth_account",
        &displaced,
        &fresh("fake-access-a", "fake-refresh-a"),
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    write_global(
        &rig.scratch.dir,
        &json!({ "numStartups": 7, "zLast": true }),
    );

    let outcome = switch_to(&rig, 2).expect("the target is activated directly");

    assert_eq!(outcome.from, None, "no live identity was displaced");
    let rows = stash_rows(&rig);
    assert_eq!(rows.len(), 1, "the displaced credential is stashed once");
    let (id, row) = &rows[0];
    assert_eq!(row["reason"], "displaced-live-login");
    assert_eq!(
        row["configSlot"], "1",
        "filed under accounts.json's active account"
    );
    assert_eq!(
        rig.scratch.stash_entry(id).as_deref(),
        Some(displaced.as_str())
    );
    assert_eq!(
        keys(&global(&rig)),
        ["numStartups", "zLast", "oauthAccount"]
    );
    assert_eq!(active(&rig), Some(2));
}

#[test]
fn an_unmanaged_api_key_login_is_refused_with_nothing_written() {
    let rig = stranger_rig(
        "direct_unmanaged_api_key",
        &fresh("fake-access-x", "fake-refresh-x"),
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    let mut config = global_config(STRANGER, "org-stranger");
    config["primaryApiKey"] = json!("sk-ant-fake-key");
    write_global(&rig.scratch.dir, &config);
    let before = files(&rig);

    let error = switch_to(&rig, 2).expect_err("a live API key with no backup is not dropped");

    assert_eq!(error.kind(), SwitchErrorKind::ApiKeyAccount);
    assert_eq!(error.kind().to_string(), API_KEY_MESSAGE);
    assert_eq!(error.login_state(), None);
    assert_eq!(
        files(&rig),
        before,
        "no stash row, backup or live file was written"
    );
}

/// `~/.claude.json` names two logins under one key: the first read of the switch (serde's struct, which refuses the
/// duplicate) finds no login, the snapshot (the last value wins) finds account 1. The switch stops rather than stash a
/// managed account's credential as no account's.
#[test]
fn a_snapshot_naming_a_managed_account_is_refused_with_nothing_written() {
    let rig = stranger_rig(
        "direct_snapshot_names_managed",
        &fresh("fake-access-a", "fake-refresh-a"),
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    let first =
        format!(r#"{{"oauthAccount": {{"emailAddress": "{STRANGER}", "organizationUuid": ""}}, "#);
    let text = format!(
        r#"{first}"oauthAccount": {{"emailAddress": "{ONE}", "organizationUuid": "{ONE_ORG}"}}}}"#
    );
    fs::write(rig.claude.global_config(), text).expect("the global config can be written");
    let before = files(&rig);

    let error = switch_to(&rig, 2).expect_err("the live login is a managed account's after all");

    assert_eq!(error.kind(), SwitchErrorKind::LocksBusy);
    assert_eq!(error.login_state(), None);
    assert_eq!(
        files(&rig),
        before,
        "no stash row, backup or live file was written"
    );
}

#[test]
fn a_live_email_no_account_could_have_is_displaced_with_no_from() {
    let displaced = fresh("fake-access-x", "fake-refresh-x");
    let rig = stranger_rig(
        "direct_unsafe_live_email",
        &displaced,
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    write_global(
        &rig.scratch.dir,
        &global_config("a/b@example.test", "org-stranger"),
    );

    let outcome = switch_to(&rig, 2).expect("the target is activated directly");

    assert!(outcome.switched);
    assert_eq!(
        outcome.from, None,
        "an email no account could have is not named"
    );
    let rows = stash_rows(&rig);
    assert_eq!(rows.len(), 1, "the displaced credential is stashed once");
    assert_eq!(rows[0].1["configSlot"], "unmanaged");
    assert_eq!(
        rig.scratch.stash_entry(&rows[0].0).as_deref(),
        Some(displaced.as_str())
    );
}

#[test]
fn an_unreadable_live_credential_is_refused_by_direct_activation() {
    let rig = stranger_rig(
        "direct_live_unreadable",
        &fresh("fake-access-x", "fake-refresh-x"),
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    fs::write(rig.claude.credentials_file(), [0xff, 0xfe, 0x00])
        .expect("the live credential can be written");
    let before = files(&rig);

    let error = switch_to(&rig, 2).expect_err("a live credential that is not UTF-8 is refused");

    assert_eq!(error.kind(), SwitchErrorKind::LiveUnreadable);
    assert_eq!(error.login_state(), None);
    assert_eq!(
        files(&rig),
        before,
        "no stash row, backup or live file was written"
    );
}

#[test]
fn an_empty_live_credential_is_restored_empty_on_rollback() {
    let rig = stranger_rig(
        "direct_empty_live_rollback",
        "",
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    let held = readable_only(&rig.scratch.backup().join("accounts.json"));

    let error = switch_to(&rig, 2).expect_err("the sequence write fails");
    drop(held);

    assert_eq!(error.kind(), SwitchErrorKind::WriteFailed);
    assert_eq!(error.login_state(), Some(LoginState::Source));
    assert_eq!(
        fs::read(rig.claude.credentials_file()).expect("the live credential file is still there"),
        Vec::<u8>::new(),
        "the empty file is put back empty"
    );
    assert_eq!(global(&rig)["oauthAccount"]["emailAddress"], STRANGER);
}

#[test]
fn an_unmanaged_login_to_an_api_key_target_is_refused_as_unknown() {
    let rig = stranger_rig(
        "direct_api_key_target",
        &fresh("fake-access-x", "fake-refresh-x"),
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    edit_sequence(&rig, |sequence| {
        sequence["accounts"]["2"]["kind"] = json!("api_key");
    });
    let before = files(&rig);

    let error = switch_to(&rig, 2).expect_err("a switch to an API-key account is refused");

    assert_eq!(error.kind(), SwitchErrorKind::UnknownAccount);
    assert_eq!(files(&rig), before, "nothing was written");
}

#[test]
fn failed_config_write_rolls_back_credentials() {
    let own = fresh("fake-access-a", "fake-refresh-a");
    let rig = rig(
        "switch_rolls_back",
        &own,
        &own,
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    let (live_before, config_before) = make_non_canonical(&rig);
    let held = readable_only(rig.claude.global_config());

    let error = switch_to(&rig, 2).expect_err("the config write fails");
    drop(held);

    assert_eq!(error.kind(), SwitchErrorKind::WriteFailed);
    assert_eq!(error.account(), 2);
    assert_eq!(error.login_state(), Some(LoginState::Source));
    assert_eq!(
        fs::read(rig.claude.credentials_file()).expect("the live credential can be read"),
        live_before
    );
    assert_eq!(
        fs::read(rig.claude.global_config()).expect("the global config can be read"),
        config_before
    );
    assert_eq!(active(&rig), Some(1), "the active account is unchanged");
    assert!(
        temp_files(&rig.scratch.dir).is_empty(),
        "no temp file is left beside the config"
    );
    assert!(
        temp_files(&rig.scratch.dir.join("claude")).is_empty(),
        "no temp file is left beside the live file"
    );
}

#[test]
fn failed_sequence_write_rolls_back_config_and_credentials() {
    let own = fresh("fake-access-a", "fake-refresh-a");
    let rig = rig(
        "switch_sequence_fails",
        &own,
        &own,
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    let (live_before, config_before) = make_non_canonical(&rig);
    let held = readable_only(&rig.scratch.backup().join("accounts.json"));

    let error = switch_to(&rig, 2).expect_err("the sequence write fails");
    drop(held);

    assert_eq!(error.kind(), SwitchErrorKind::WriteFailed);
    assert_eq!(error.login_state(), Some(LoginState::Source));
    assert_eq!(
        fs::read(rig.claude.credentials_file()).expect("the live credential can be read"),
        live_before
    );
    assert_eq!(
        fs::read(rig.claude.global_config()).expect("the global config can be read"),
        config_before
    );
    assert_eq!(
        global(&rig)["oauthAccount"]["emailAddress"],
        ONE,
        "account 1 is the login again"
    );
    assert_eq!(active(&rig), Some(1));
    for folder in [
        rig.scratch.dir.clone(),
        rig.scratch.dir.join("claude"),
        rig.scratch.backup(),
    ] {
        assert!(
            temp_files(&folder).is_empty(),
            "no temp file is left in {}",
            folder.display()
        );
    }
}

#[test]
fn an_oracle_failure_does_not_fail_the_switch() {
    let rotated = credential("fake-access-b", Some("fake-refresh-b"), ms_after(7200));
    let old = fresh("fake-access-a", "fake-refresh-a");
    let rig = rig(
        "switch_oracle_fails",
        &rotated,
        &old,
        &fresh("fake-access-two", "fake-refresh-two"),
    );

    let outcome = probed_switch_to(&rig, 2).expect("the switch succeeds without the oracle");

    assert_eq!(
        rig.profile.asked(),
        ["fake-access-b"],
        "the oracle was asked and failed"
    );
    assert!(outcome.switched);
    assert_eq!(
        stored(&rig, 1),
        Some(rotated),
        "an unresolved credential is backed up, as cswap's fail-open does"
    );
    assert!(stash_rows(&rig).is_empty(), "nothing is stashed");
}

#[test]
fn no_switch_error_or_outcome_holds_a_token() {
    let rotated = fresh("fake-access-b", "fake-refresh-b");
    let old = fresh("fake-access-a", "fake-refresh-a");
    let mut rig = rig(
        "switch_prints_no_token",
        &rotated,
        &old,
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    rig.profile = crate::support::FakeProfile::answering(vec![profile(ONE_UUID, ONE, ONE_ORG)]);
    let mut printed = Vec::new();

    let probe = probe_outgoing(&rig.context());
    printed.push(format!("{probe:?}"));
    let unknown = switch(&rig.context(), 9, &probe, None).expect_err("account 9 does not exist");
    printed.extend([format!("{unknown:?}"), unknown.to_string()]);
    let held = readable_only(rig.claude.global_config());
    let failed = switch(&rig.context(), 2, &probe, None).expect_err("the config write fails");
    drop(held);
    printed.extend([format!("{failed:?}"), failed.to_string()]);
    let outcome = switch(&rig.context(), 2, &probe, None).expect("the switch succeeds");
    printed.push(format!("{outcome:?}"));
    write_global(&rig.scratch.dir, &global_config(STRANGER, "org-stranger"));
    let held = readable_only(rig.claude.global_config());
    let direct_failed = switch_to(&rig, 1).expect_err("the direct activation's config write fails");
    drop(held);
    printed.extend([format!("{direct_failed:?}"), direct_failed.to_string()]);
    let direct = switch_to(&rig, 1).expect("a live login no account owns is stashed and replaced");
    printed.push(format!("{direct:?}"));

    let folder = rig.scratch.dir.to_string_lossy().into_owned();
    for text in printed {
        for token in FAKE_TOKENS {
            assert!(!text.contains(token), "{text} carries {token}");
        }
        assert!(!text.contains(&folder), "{text} carries a path");
    }
}

/// The live credential's and `~/.claude.json`'s bytes.
fn live_files(rig: &Rig) -> (Vec<u8>, Vec<u8>) {
    (
        fs::read(rig.claude.credentials_file()).expect("the live credential can be read"),
        fs::read(rig.claude.global_config()).expect("the global config can be read"),
    )
}

#[test]
fn a_failed_outgoing_credential_backup_aborts_with_nothing_live_written() {
    let rotated = credential("fake-access-b", Some("fake-refresh-b"), ms_after(7200));
    let old = fresh("fake-access-a", "fake-refresh-a");
    let rig = rig(
        "switch_credential_backup_fails",
        &rotated,
        &old,
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    let before = live_files(&rig);
    let held = readable_only(&rig.scratch.root().credential_path(1));

    let error = switch_to(&rig, 2).expect_err("the outgoing backup cannot be replaced");
    drop(held);

    assert_eq!(error.kind(), SwitchErrorKind::OutgoingBackupFailed);
    assert_eq!(error.login_state(), None, "no live file was written");
    assert_eq!(
        live_files(&rig),
        before,
        "both live files are byte-unchanged"
    );
    assert_eq!(stored(&rig, 1), Some(old));
    assert_eq!(active(&rig), Some(1));
}

#[test]
fn a_failed_outgoing_config_backup_aborts_with_nothing_live_written() {
    let own = fresh("fake-access-a", "fake-refresh-a");
    let rig = rig(
        "switch_config_backup_fails",
        &own,
        &own,
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    let config_backup = rig.scratch.root().config_path(1);
    fs::write(&config_backup, b"{}").expect("account 1's config backup can be written");
    let before = live_files(&rig);
    let held = readable_only(&config_backup);

    let error = switch_to(&rig, 2).expect_err("the outgoing config backup cannot be replaced");
    drop(held);

    assert_eq!(error.kind(), SwitchErrorKind::OutgoingBackupFailed);
    assert_eq!(
        live_files(&rig),
        before,
        "both live files are byte-unchanged"
    );
    assert_eq!(
        one_config_backup(&rig).as_deref(),
        Some(b"{}".as_slice()),
        "the old config backup is kept"
    );
    assert_eq!(active(&rig), Some(1));
}

#[test]
fn a_prev_copy_that_cannot_be_kept_is_a_warning() {
    let rotated = credential("fake-access-b", Some("fake-refresh-b"), ms_after(7200));
    let old = fresh("fake-access-a", "fake-refresh-a");
    let rig = rig(
        "switch_prev_not_kept",
        &rotated,
        &old,
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    let mut prev = rig.scratch.root().credential_path(1).into_os_string();
    prev.push(".prev");
    let prev = PathBuf::from(prev);
    if prev.exists() {
        fs::remove_file(&prev).expect("the old .prev copy can be removed");
    }
    fs::create_dir(&prev).expect("a folder stands where the .prev copy goes");

    let outcome = switch_to(&rig, 2).expect("the switch goes ahead without the .prev copy");

    assert!(outcome.switched);
    assert_eq!(stored(&rig, 1), Some(rotated), "the new backup was written");
    assert_eq!(outcome.warnings.len(), 1, "got {:?}", outcome.warnings);
    assert!(
        outcome.warnings[0].contains(".prev"),
        "got {:?}",
        outcome.warnings
    );
}

#[test]
fn a_target_without_a_backup_is_refused_before_anything_is_written() {
    let rotated = credential("fake-access-b", Some("fake-refresh-b"), ms_after(7200));
    let old = fresh("fake-access-a", "fake-refresh-a");
    let rig = rig(
        "switch_target_missing",
        &rotated,
        &old,
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    fs::remove_file(rig.scratch.root().config_path(2))
        .expect("account 2's config backup can be removed");
    let before = files(&rig);

    let error = switch_to(&rig, 2).expect_err("a target without a config backup is refused");

    assert_eq!(error.kind(), SwitchErrorKind::TargetBackupMissing);
    assert_eq!(error.kind().as_str(), "target_backup_missing");
    assert_eq!(
        files(&rig),
        before,
        "no backup, .prev or stash row was written"
    );
}

/// The account number is all that ties a record to its files: a record re-identified as another login, its files left
/// in place, is refused before anything is written, from a managed login and from an unmanaged one.
#[test]
fn a_target_whose_files_are_another_logins_is_refused_before_anything_is_written() {
    let managed = rig(
        "switch_target_identity_mismatch",
        &fresh("fake-access-b", "fake-refresh-b"),
        &fresh("fake-access-a", "fake-refresh-a"),
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    let unmanaged = stranger_rig(
        "direct_target_identity_mismatch",
        &fresh("fake-access-x", "fake-refresh-x"),
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    for rig in [managed, unmanaged] {
        rig.scratch
            .set_account(2, "email", json!("other@example.test"));
        let accounts_json = rig.scratch.backup().join("accounts.json");
        let roster = fs::read(&accounts_json).expect("accounts.json can be read");
        let before = files(&rig);
        let live_before = live_files(&rig);

        let error =
            switch_to(&rig, 2).expect_err("a target whose config names another login is refused");

        assert_eq!(error.kind(), SwitchErrorKind::TargetIdentityMismatch);
        assert_eq!(error.kind().as_str(), "target_identity_mismatch");
        assert_eq!(
            files(&rig),
            before,
            "no backup, .prev or stash row was written"
        );
        assert_eq!(
            live_files(&rig),
            live_before,
            "the live files are untouched"
        );
        assert_eq!(
            fs::read(&accounts_json).expect("accounts.json can be read"),
            roster,
            "accounts.json is untouched"
        );
    }
}

#[test]
fn an_unreadable_target_backup_is_its_own_kind() {
    let own = fresh("fake-access-a", "fake-refresh-a");
    let rig = rig(
        "switch_target_unreadable",
        &own,
        &own,
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    fs::write(rig.scratch.root().config_path(2), "not json")
        .expect("account 2's config backup can be written");
    let before = live_files(&rig);

    let error = switch_to(&rig, 2).expect_err("a corrupt target config backup is refused");

    assert_eq!(error.kind(), SwitchErrorKind::TargetBackupUnreadable);
    assert_eq!(error.kind().as_str(), "target_backup_unreadable");
    assert!(
        !error.kind().to_string().contains("add it again"),
        "a read error does not advise re-adding"
    );
    assert_eq!(
        live_files(&rig),
        before,
        "both live files are byte-unchanged"
    );
}

/// A target config whose `oauthAccount` is an empty object names no login: it is a mismatch, not a missing backup,
/// and the reply advises re-adding the account.
#[test]
fn a_target_config_with_an_empty_identity_is_a_mismatch() {
    let own = fresh("fake-access-a", "fake-refresh-a");
    let rig = rig(
        "switch_target_empty_identity",
        &own,
        &own,
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    fs::write(
        rig.scratch.root().config_path(2),
        json!({ "oauthAccount": {} }).to_string(),
    )
    .expect("account 2's config backup can be written");
    let before = live_files(&rig);

    let error = switch_to(&rig, 2).expect_err("an empty identity is not the target's login");

    assert_eq!(error.kind(), SwitchErrorKind::TargetIdentityMismatch);
    assert_eq!(error.kind().as_str(), "target_identity_mismatch");
    assert_eq!(
        live_files(&rig),
        before,
        "both live files are byte-unchanged"
    );
}

/// A target config with no `oauthAccount` object cannot be read as the account's identity: it is the unreadable kind,
/// which does not advise re-adding.
#[test]
fn a_target_config_with_no_identity_object_is_unreadable() {
    let own = fresh("fake-access-a", "fake-refresh-a");
    let rig = rig(
        "switch_target_no_identity",
        &own,
        &own,
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    fs::write(
        rig.scratch.root().config_path(2),
        json!({ "mcpServers": {} }).to_string(),
    )
    .expect("account 2's config backup can be written");
    let before = live_files(&rig);

    let error =
        switch_to(&rig, 2).expect_err("a config without an identity is not a usable backup");

    assert_eq!(error.kind(), SwitchErrorKind::TargetBackupUnreadable);
    assert_eq!(error.kind().as_str(), "target_backup_unreadable");
    assert!(
        !error.kind().to_string().contains("add it again"),
        "a read error does not advise re-adding"
    );
    assert_eq!(
        live_files(&rig),
        before,
        "both live files are byte-unchanged"
    );
}

/// A store whose canary fails refuses the switch before its first lock: the consume lock is never made.
#[test]
fn a_store_that_refuses_writes_stops_the_switch_before_a_lock() {
    let own = fresh("fake-access-a", "fake-refresh-a");
    let mut rig = rig(
        "switch_store_read_only",
        &own,
        &own,
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    rig.store =
        Store::open_with_canary(rig.scratch.root(), || false).expect("the scratch store opens");
    let live_before = live_files(&rig);

    let error = switch_to(&rig, 2).expect_err("a store that takes no write refuses the switch");

    assert_eq!(error.kind(), SwitchErrorKind::StoreReadOnly);
    assert_eq!(error.kind().as_str(), "store_read_only");
    assert!(
        !rig.scratch.root().consume_lock(2).exists(),
        "the first lock of the order is never taken"
    );
    assert_eq!(
        live_files(&rig),
        live_before,
        "both live files are byte-unchanged"
    );
}

/// How many times [`sealing_until_the_locks`] was asked.
static CANARY_CALLS: AtomicU32 = AtomicU32::new(0);

/// A canary whose first answer says sealing works and every later one says it does not: the switch's check before its
/// locks passes, and a refusal appears during the lock wait.
fn sealing_until_the_locks() -> bool {
    CANARY_CALLS.fetch_add(1, Ordering::SeqCst) == 0
}

/// A store refusal that appears while the switch waits on its locks is seen under them, before any write: from a
/// managed login and from an unmanaged one, the switch stops as the store refusal with no live file written (none
/// written and then rolled back).
#[test]
fn a_store_refusal_that_appears_during_the_lock_wait_stops_the_switch_before_any_write() {
    let managed = rig(
        "switch_refusal_during_lock_wait",
        &fresh("fake-access-b", "fake-refresh-b"),
        &fresh("fake-access-a", "fake-refresh-a"),
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    let unmanaged = stranger_rig(
        "direct_refusal_during_lock_wait",
        &fresh("fake-access-x", "fake-refresh-x"),
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    for mut rig in [managed, unmanaged] {
        rig.store = Store::open_with_canary(rig.scratch.root(), sealing_until_the_locks)
            .expect("the scratch store opens");
        CANARY_CALLS.store(0, Ordering::SeqCst);
        let before = files(&rig);

        let error = switch_to(&rig, 2).expect_err("a refusal under the locks stops the switch");

        assert_eq!(error.kind(), SwitchErrorKind::StoreReadOnly);
        assert_eq!(error.login_state(), None, "no live file was written");
        assert_eq!(
            CANARY_CALLS.load(Ordering::SeqCst),
            2,
            "the store is asked before the locks and again under them"
        );
        assert_eq!(
            files(&rig),
            before,
            "no live file, backup, stash row or roster was written"
        );
    }
}

/// An `accounts.json` of a later schema refuses the switch before its first lock.
#[test]
fn a_later_schema_accounts_json_stops_the_switch_before_a_lock() {
    let own = fresh("fake-access-a", "fake-refresh-a");
    let rig = rig("switch_store_later_schema", &own, &own, &two_backup());
    edit_sequence(&rig, |sequence| sequence["schemaVersion"] = json!(99));
    let before = files(&rig);

    let error = switch_to(&rig, 2).expect_err("a later store's schema refuses the switch");

    assert_eq!(error.kind(), SwitchErrorKind::UnknownSchema);
    assert!(
        !rig.scratch.root().consume_lock(2).exists(),
        "the first lock of the order is never taken"
    );
    assert_eq!(files(&rig), before, "nothing was written");
}

/// `CLAUDE_SECURESTORAGE_CONFIG_DIR` set refuses the switch before its first lock.
#[test]
fn a_securestorage_dir_stops_the_switch_before_a_lock() {
    let own = fresh("fake-access-a", "fake-refresh-a");
    let mut rig = rig("switch_store_unmirrored", &own, &own, &two_backup());
    rig.securestorage_dir_set = true;
    let before = files(&rig);

    let error =
        switch_to(&rig, 2).expect_err("a store Claude Code does not mirror refuses the switch");

    assert_eq!(error.kind(), SwitchErrorKind::StoreUnmirrored);
    assert!(
        !rig.scratch.root().consume_lock(2).exists(),
        "the first lock of the order is never taken"
    );
    assert_eq!(files(&rig), before, "nothing was written");
}

/// Account 2's backup in the store-refusal and live-config tests.
fn two_backup() -> String {
    fresh("fake-access-two", "fake-refresh-two")
}

fn access_of_live(rig: &Rig) -> String {
    json_of(&live(rig))["claudeAiOauth"]["accessToken"]
        .as_str()
        .expect("the live credential has an access token")
        .to_owned()
}

/// The live `~/.claude.json` is the user's own file, which they may link on purpose: a switch still reads it.
#[test]
fn a_symlinked_live_global_config_is_still_read() {
    let own = fresh("fake-access-a", "fake-refresh-a");
    let rig = rig("switch_symlinked_live_config", &own, &own, &two_backup());
    let config = rig.scratch.dir.join(".claude.json");
    let elsewhere = rig.scratch.dir.join("elsewhere-claude.json");
    fs::rename(&config, &elsewhere).expect("the global config can be moved aside");
    if !symlink_or_skip(&config, &elsewhere) {
        return;
    }

    let outcome = switch_to(&rig, 2).expect("a linked live config is read");

    assert!(outcome.switched);
    assert_eq!(access_of_live(&rig), "fake-access-two");
    assert_eq!(global(&rig)["oauthAccount"]["emailAddress"], TWO);
}

/// A started drain refuses the switch once it holds its locks, with nothing written.
#[test]
fn a_switch_refuses_while_draining() {
    let own = fresh("fake-access-a", "fake-refresh-a");
    let rig = rig("switch_draining", &own, &own, &two_backup());
    let _in_flight = rig.drain.start();
    let before = files(&rig);

    let error = switch_to(&rig, 2).expect_err("a draining server switches nothing");

    assert_eq!(error.kind(), SwitchErrorKind::Draining);
    assert_eq!(files(&rig), before, "nothing was written");
}

/// `accounts.json` names the target active while `~/.claude.json` is still account 1's, as a crash can leave it: the
/// switch is made, not answered as already active.
#[test]
fn a_target_active_only_in_accounts_json_is_still_switched_to() {
    let own = fresh("fake-access-a", "fake-refresh-a");
    let rig = rig("switch_sequence_only_active", &own, &own, &two_backup());
    rig.scratch.set_active(2);

    let outcome = switch_to(&rig, 2).expect("the switch goes ahead");

    assert!(outcome.switched);
    assert_eq!(outcome.reason, "switched");
    assert_eq!(access_of_live(&rig), "fake-access-two");
}

/// How long the rig waits for each of Claude Code's locks.
const LOCK_WAIT: Duration = Duration::from_millis(700);

/// Spins until `path` exists, for at most 5 s.
fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        thread::sleep(Duration::from_millis(1));
    }
}

/// Whether another handle holds the file lock at `path`.
fn held_elsewhere(path: &Path) -> bool {
    matches!(
        FileLock::acquire(path, Duration::ZERO),
        Err(LockError::Timeout)
    )
}

#[test]
fn held_claude_code_lock_fails_the_switch_with_nothing_written() {
    let own = fresh("fake-access-a", "fake-refresh-a");
    let mut rig = rig(
        "switch_lock_held",
        &own,
        &own,
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    rig.claude_locks = LockTiming::new(LOCK_WAIT, Duration::from_secs(3));
    let held = rig.claude.credentials_lock();
    fs::create_dir(&held).expect("the lock directory is made");
    let before = files(&rig);

    let started = Instant::now();
    let error = switch_to(&rig, 2).expect_err("a held lock fails the switch");
    let waited = started.elapsed();

    assert_eq!(error.kind(), SwitchErrorKind::LocksBusy);
    assert_eq!(error.kind().as_str(), "locks_busy");
    assert_eq!(error.account(), 2);
    assert!(
        waited >= LOCK_WAIT,
        "the switch waited out the lock timeout, waited {waited:?}"
    );
    assert!(held.is_dir(), "the holder keeps its lock");
    assert!(
        !rig.claude.oauth_refresh_lock().exists(),
        "the primary lock is released"
    );
    assert_eq!(files(&rig), before, "nothing was written");
}

#[test]
fn lock_order_is_consume_account_credentials_config() {
    let own = fresh("fake-access-a", "fake-refresh-a");
    let mut rig = rig(
        "switch_lock_order",
        &own,
        &own,
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    rig.claude_locks = LockTiming::new(Duration::from_secs(5), Duration::from_secs(3));
    let root = rig.scratch.root();
    let account = AccountLock::acquire(&root).expect("the account lock is free");
    let (primary, legacy, config) = (
        rig.claude.oauth_refresh_lock(),
        rig.claude.credentials_lock(),
        rig.claude.config_lock(),
    );
    fs::create_dir(&legacy).expect("the legacy lock directory is made");
    fs::create_dir(&config).expect("the config lock directory is made");
    let consume = root.consume_lock(2);

    let outcome = thread::scope(|scope| {
        let switching = scope.spawn(|| switch_to(&rig, 2));
        // The consume lock is taken while the account lock is still held here.
        wait_for(&consume);
        assert!(
            !primary.exists(),
            "no Claude Code lock is taken before the account lock"
        );
        drop(account);
        // Past the account lock, the switch holds both of cswap's locks while it waits on `~/.claude.lock`.
        wait_for(&primary);
        assert!(held_elsewhere(&consume), "the consume lock is held");
        assert!(
            held_elsewhere(&root.lock_file()),
            "the account lock is held"
        );
        fs::remove_dir(&legacy).expect("the legacy lock is released");
        // The config lock comes last: the switch takes `~/.claude.lock` and waits on it.
        wait_for(&legacy);
        assert!(config.is_dir(), "the config lock is still this test's");
        fs::remove_dir(&config).expect("the config lock is released");
        switching.join().expect("the switch thread finishes")
    });

    assert!(outcome.expect("the switch completes").switched);
    assert!(
        !primary.exists() && !legacy.exists() && !config.exists(),
        "every Claude Code lock is released"
    );
}

/// The tests whose assertions race a write's sub-second retry window: a loaded machine can exhaust the retries before
/// the racing thread acts, so they run apart in the gate's critical slot (`cargo nextest run -P timing`).
mod timing {
    use std::fs;
    use std::path::Path;
    use std::thread;
    use std::time::{Duration, Instant};

    use claude_accounts::{LoginState, SwitchErrorKind};

    use super::{
        FAKE_TOKENS, TWO, fresh, global, live, readable_only, rig, stranger_rig, switch_to,
    };
    use crate::windows_files::{pin_first_temp, temp_files};

    #[test]
    fn a_failed_config_restore_leaves_the_credentials_on_the_target() {
        let own = fresh("fake-access-a", "fake-refresh-a");
        let rig = rig(
            "switch_config_restore_fails",
            &own,
            &own,
            &fresh("fake-access-two", "fake-refresh-two"),
        );
        let held_sequence = readable_only(&rig.scratch.backup().join("accounts.json"));
        let config_path = rig.claude.global_config().to_path_buf();

        let (error, held_config) = thread::scope(|scope| {
            // Once the forward config write lands, and while the sequence write is still retrying, pin the new config
            // so the rollback cannot replace it.
            let pinner = scope.spawn(|| {
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    if fs::read_to_string(&config_path).is_ok_and(|text| text.contains(TWO)) {
                        return readable_only(&config_path);
                    }
                    assert!(
                        Instant::now() < deadline,
                        "the forward config write never landed"
                    );
                    thread::sleep(Duration::from_millis(1));
                }
            });
            let error = switch_to(&rig, 2).expect_err("the sequence write fails");
            (error, pinner.join().expect("the pinner finishes"))
        });
        drop(held_config);
        drop(held_sequence);

        assert_eq!(error.kind(), SwitchErrorKind::RollbackFailed);
        assert_eq!(
            error.login_state(),
            Some(LoginState::Target),
            "the live pair is the target's, whole"
        );
        assert!(
            live(&rig).contains("fake-access-two"),
            "the credentials stay on the target"
        );
        assert!(
            fs::read_to_string(&config_path)
                .expect("the config reads")
                .contains(TWO)
        );
    }

    /// The forward credential write is held off, retrying, until a thread that sees its temp file has made a
    /// `~/.claude.json` the snapshot did not find: the direct activation leaves that file alone and rolls back.
    #[test]
    fn a_global_config_that_appears_after_the_snapshot_is_not_overwritten() {
        let own = fresh("fake-access-x", "fake-refresh-x");
        let rig = rig(
            "direct_config_appears",
            &own,
            &fresh("fake-access-a", "fake-refresh-a"),
            &fresh("fake-access-two", "fake-refresh-two"),
        );
        let config_path = rig.claude.global_config().to_path_buf();
        fs::remove_file(&config_path).expect("the global config can be removed");
        let appeared: &[u8] = br#"{"primaryApiKey": "sk-ant-fake-key"}"#;
        let held = readable_only(&rig.claude.credentials_file());
        let (claude_dir, writes_to) = (rig.scratch.dir.join("claude"), config_path.clone());

        let error = thread::scope(|scope| {
            scope.spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(5);
                while temp_files(&claude_dir).is_empty() {
                    assert!(
                        Instant::now() < deadline,
                        "the forward credential write never started"
                    );
                    thread::sleep(Duration::from_millis(1));
                }
                fs::write(&writes_to, appeared).expect("the global config can be written");
                drop(held);
            });
            switch_to(&rig, 2).expect_err("the global config appeared after the snapshot")
        });

        assert_eq!(error.kind(), SwitchErrorKind::WriteFailed);
        assert_eq!(error.login_state(), Some(LoginState::Source));
        assert_eq!(
            fs::read(&config_path).expect("the global config is still there"),
            appeared,
            "a file the switch did not make is neither overwritten nor removed"
        );
        assert_eq!(live(&rig), own, "the live credential is back");
    }

    /// `rig`'s `~/.claude.json` with a `primaryApiKey` added, as another writer would rewrite it.
    fn with_api_key(rig: &crate::support::Rig) -> Vec<u8> {
        let mut config = global(rig);
        config
            .as_object_mut()
            .expect("the global config is an object")
            .insert("primaryApiKey".to_owned(), "sk-ant-fake-key".into());
        serde_json::to_vec_pretty(&config).expect("the global config serializes")
    }

    /// A switch to account 2 whose forward credential write is held off, retrying, until a thread that sees its temp
    /// file has written `changed` over `~/.claude.json`. Under heavy load the credential write can exhaust its retries
    /// before the thread writes, which still satisfies the callers' assertions without the expected-bytes check
    /// running, as in `a_global_config_that_appears_after_the_snapshot_is_not_overwritten`: hence the critical slot.
    fn switch_after_config_change(
        rig: &crate::support::Rig,
        changed: &[u8],
    ) -> claude_accounts::SwitchError {
        let held = readable_only(&rig.claude.credentials_file());
        let (claude_dir, writes_to) = (
            rig.scratch.dir.join("claude"),
            rig.claude.global_config().to_path_buf(),
        );
        thread::scope(|scope| {
            scope.spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(5);
                while temp_files(&claude_dir).is_empty() {
                    assert!(
                        Instant::now() < deadline,
                        "the forward credential write never started"
                    );
                    thread::sleep(Duration::from_millis(1));
                }
                fs::write(&writes_to, changed).expect("the global config can be written");
                drop(held);
            });
            switch_to(rig, 2).expect_err("the global config changed after the snapshot")
        })
    }

    #[test]
    fn a_config_changed_after_the_snapshot_fails_the_managed_switch() {
        let own = fresh("fake-access-a", "fake-refresh-a");
        let rig = rig(
            "managed_config_changes",
            &own,
            &own,
            &fresh("fake-access-two", "fake-refresh-two"),
        );
        let changed = with_api_key(&rig);

        let error = switch_after_config_change(&rig, &changed);

        assert_eq!(error.kind(), SwitchErrorKind::WriteFailed);
        assert_eq!(error.login_state(), Some(LoginState::Source));
        assert_eq!(
            fs::read(rig.claude.global_config()).expect("the global config is still there"),
            changed,
            "a config changed since the snapshot is left byte for byte, its key kept"
        );
        assert_eq!(live(&rig), own, "the live credential is back");
    }

    #[test]
    fn a_config_changed_after_the_snapshot_fails_the_direct_activation() {
        let own = fresh("fake-access-x", "fake-refresh-x");
        let rig = stranger_rig(
            "direct_config_changes",
            &own,
            &fresh("fake-access-two", "fake-refresh-two"),
        );
        let changed = with_api_key(&rig);

        let error = switch_after_config_change(&rig, &changed);

        assert_eq!(error.kind(), SwitchErrorKind::WriteFailed);
        assert_eq!(error.login_state(), Some(LoginState::Source));
        assert_eq!(
            fs::read(rig.claude.global_config()).expect("the global config is still there"),
            changed,
            "a config changed since the snapshot is left byte for byte, its key kept"
        );
        assert_eq!(live(&rig), own, "the live credential is back");
    }

    /// A switch to account 2 whose last write before the live files, a store write onto `destination` through a temp
    /// file beside it, is held off, retrying, until a thread that pinned that temp file has written `racer` over the
    /// live credential file. `destination` must exist: it is held open sharing only reads, so the rename onto it keeps
    /// failing and the temp file stays for the whole retry window, not the microseconds a free rename takes. Under heavy
    /// load the held write can exhaust its retries before the thread writes, which fails the switch before any live
    /// write and the callers' assertions with it: hence the critical slot.
    fn switch_after_credential_change(
        rig: &crate::support::Rig,
        destination: &Path,
        racer: &str,
    ) -> claude_accounts::SwitchError {
        let held = readable_only(destination);
        let pinner = pin_first_temp(
            destination
                .parent()
                .expect("the held write's destination has a folder")
                .to_path_buf(),
        );
        let live_path = rig.claude.credentials_file();
        thread::scope(|scope| {
            scope.spawn(move || {
                let pinned = pinner
                    .join()
                    .expect("the held write's temp file was pinned");
                fs::write(&live_path, racer).expect("the live credential can be written");
                drop(pinned);
                drop(held);
            });
            switch_to(rig, 2).expect_err("the live credential changed after the snapshot")
        })
    }

    #[test]
    fn a_credential_changed_after_the_snapshot_fails_the_managed_switch() {
        let own = fresh("fake-access-a", "fake-refresh-a");
        let rig = rig(
            "managed_credential_changes",
            &own,
            &own,
            &fresh("fake-access-two", "fake-refresh-two"),
        );
        let racer = fresh("fake-access-y", "fake-refresh-y");
        let config_before = fs::read(rig.claude.global_config()).expect("the global config reads");

        // The outgoing account's config backup is the managed switch's last write before the live files. The rig has
        // none, so the one an earlier switch would have left is put there for the switch to overwrite.
        let backup = rig.scratch.root().config_path(1);
        fs::write(&backup, &config_before).expect("account 1's config backup can be written");
        let error = switch_after_credential_change(&rig, &backup, &racer);

        assert_eq!(error.kind(), SwitchErrorKind::WriteFailed);
        assert_eq!(error.login_state(), Some(LoginState::Source));
        for token in FAKE_TOKENS {
            assert!(
                !format!("{error:?}").contains(token),
                "the error's Debug carries {token}"
            );
            assert!(
                !error.to_string().contains(token),
                "the error's message carries {token}"
            );
        }
        assert_eq!(
            fs::read(rig.claude.credentials_file()).expect("the live credential reads"),
            racer.as_bytes(),
            "a credential changed since the snapshot is left byte for byte"
        );
        assert_eq!(
            fs::read(rig.claude.global_config()).expect("the global config reads"),
            config_before,
            "the global config is unchanged"
        );
    }

    #[test]
    fn a_credential_changed_after_the_snapshot_fails_the_direct_activation() {
        let own = fresh("fake-access-x", "fake-refresh-x");
        let rig = stranger_rig(
            "direct_credential_changes",
            &own,
            &fresh("fake-access-two", "fake-refresh-two"),
        );
        let racer = fresh("fake-access-y", "fake-refresh-y");
        let config_before = fs::read(rig.claude.global_config()).expect("the global config reads");

        // The displaced credential's stash, whose manifest is written through a temp file in the store's credential
        // folder, is the direct activation's last write before the live files. The rig has no manifest, so an empty one,
        // which the stash reads as it reads a missing one, is put there for the stash to rewrite.
        let manifest = rig.scratch.manifest_path();
        fs::write(&manifest, r#"{"schemaVersion": 1, "entries": {}}"#)
            .expect("an empty stash manifest can be written");
        let error = switch_after_credential_change(&rig, &manifest, &racer);

        assert_eq!(error.kind(), SwitchErrorKind::WriteFailed);
        assert_eq!(error.login_state(), Some(LoginState::Source));
        for token in FAKE_TOKENS {
            assert!(
                !format!("{error:?}").contains(token),
                "the error's Debug carries {token}"
            );
            assert!(
                !error.to_string().contains(token),
                "the error's message carries {token}"
            );
        }
        assert_eq!(
            fs::read(rig.claude.credentials_file()).expect("the live credential reads"),
            racer.as_bytes(),
            "a credential changed since the snapshot is left byte for byte"
        );
        assert_eq!(
            fs::read(rig.claude.global_config()).expect("the global config reads"),
            config_before,
            "the global config is unchanged"
        );
    }

    /// Spins until `path` holds `marker`, a switch's forward write, for at most 5 s, then writes `other` over it, as
    /// another writer would while the switch's sequence write is still retrying.
    fn rewrite_once_written(path: &Path, marker: &str, other: &[u8]) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !fs::read_to_string(path).is_ok_and(|text| text.contains(marker)) {
            assert!(Instant::now() < deadline, "the forward write never landed");
            thread::sleep(Duration::from_millis(1));
        }
        fs::write(path, other).expect("the file can be rewritten");
    }

    #[test]
    fn a_created_config_rewritten_before_rollback_is_left_in_place() {
        let rig = rig(
            "rollback_keeps_rewritten_config",
            &fresh("fake-access-x", "fake-refresh-x"),
            &fresh("fake-access-a", "fake-refresh-a"),
            &fresh("fake-access-two", "fake-refresh-two"),
        );
        let config_path = rig.claude.global_config().to_path_buf();
        fs::remove_file(&config_path).expect("the global config can be removed");
        let rewritten: &[u8] = br#"{"numStartups": 99}"#;
        let held_sequence = readable_only(&rig.scratch.backup().join("accounts.json"));

        let error = thread::scope(|scope| {
            scope.spawn(|| rewrite_once_written(&config_path, TWO, rewritten));
            switch_to(&rig, 2).expect_err("the sequence write fails")
        });
        drop(held_sequence);

        assert_eq!(error.kind(), SwitchErrorKind::RollbackFailed);
        assert_eq!(
            error.login_state(),
            Some(LoginState::Target),
            "the config was not put back, so the credentials stay on the target"
        );
        assert_eq!(
            fs::read(&config_path).expect("the config is still there"),
            rewritten,
            "a config rewritten since the switch created it is left byte for byte"
        );
        assert!(
            live(&rig).contains("fake-access-two"),
            "the credentials stay on the target"
        );
    }

    #[test]
    fn a_created_credentials_file_rewritten_before_rollback_is_left_in_place() {
        let rig = stranger_rig(
            "rollback_keeps_rewritten_credentials",
            &fresh("fake-access-x", "fake-refresh-x"),
            &fresh("fake-access-two", "fake-refresh-two"),
        );
        let live_path = rig.claude.credentials_file();
        fs::remove_file(&live_path).expect("the live credential can be removed");
        let rewritten = fresh("fake-access-y", "fake-refresh-y");
        let held_sequence = readable_only(&rig.scratch.backup().join("accounts.json"));

        let error = thread::scope(|scope| {
            scope.spawn(|| {
                rewrite_once_written(&live_path, "fake-access-two", rewritten.as_bytes());
            });
            switch_to(&rig, 2).expect_err("the sequence write fails")
        });
        drop(held_sequence);

        assert_eq!(error.kind(), SwitchErrorKind::RollbackFailed);
        assert_eq!(
            error.login_state(),
            Some(LoginState::Mixed),
            "the config is back, the credential file is not"
        );
        assert_eq!(
            live(&rig),
            rewritten,
            "a credential file rewritten since the switch created it is left byte for byte"
        );
        let config = fs::read_to_string(rig.claude.global_config()).expect("the config reads");
        assert!(!config.contains(TWO), "the config is the stranger's again");
    }

    #[test]
    fn a_restored_config_rewritten_before_rollback_is_left_in_place() {
        let own = fresh("fake-access-a", "fake-refresh-a");
        let rig = rig(
            "rollback_keeps_rewritten_restored_config",
            &own,
            &own,
            &fresh("fake-access-two", "fake-refresh-two"),
        );
        let config_path = rig.claude.global_config().to_path_buf();
        let rewritten: &[u8] = br#"{"numStartups": 99}"#;
        let held_sequence = readable_only(&rig.scratch.backup().join("accounts.json"));

        let error = thread::scope(|scope| {
            scope.spawn(|| rewrite_once_written(&config_path, TWO, rewritten));
            switch_to(&rig, 2).expect_err("the sequence write fails")
        });
        drop(held_sequence);

        assert_eq!(error.kind(), SwitchErrorKind::RollbackFailed);
        assert_eq!(
            error.login_state(),
            Some(LoginState::Target),
            "the config was not put back, so the credentials stay on the target"
        );
        assert_eq!(
            fs::read(&config_path).expect("the config is still there"),
            rewritten,
            "a config rewritten since the switch wrote it is not restored over"
        );
        assert!(
            live(&rig).contains("fake-access-two"),
            "the credentials stay on the target"
        );
    }

    #[test]
    fn a_restored_credential_file_rewritten_before_rollback_is_left_in_place() {
        let own = fresh("fake-access-a", "fake-refresh-a");
        let rig = rig(
            "rollback_keeps_rewritten_restored_credentials",
            &own,
            &own,
            &fresh("fake-access-two", "fake-refresh-two"),
        );
        let live_path = rig.claude.credentials_file();
        let rewritten = fresh("fake-access-y", "fake-refresh-y");
        let held_sequence = readable_only(&rig.scratch.backup().join("accounts.json"));

        let error = thread::scope(|scope| {
            scope.spawn(|| {
                rewrite_once_written(&live_path, "fake-access-two", rewritten.as_bytes());
            });
            switch_to(&rig, 2).expect_err("the sequence write fails")
        });
        drop(held_sequence);

        assert_eq!(error.kind(), SwitchErrorKind::RollbackFailed);
        assert_eq!(
            error.login_state(),
            Some(LoginState::Mixed),
            "the config is back, the credential file is not"
        );
        assert_eq!(
            live(&rig),
            rewritten,
            "a credential file rewritten since the switch wrote it is not restored over"
        );
        let config = fs::read_to_string(rig.claude.global_config()).expect("the config reads");
        assert!(!config.contains(TWO), "the config is account 1's again");
    }

    #[test]
    fn a_failed_credential_restore_reports_a_mixed_login() {
        let own = fresh("fake-access-a", "fake-refresh-a");
        let rig = rig(
            "switch_credential_restore_fails",
            &own,
            &own,
            &fresh("fake-access-two", "fake-refresh-two"),
        );
        let held_sequence = readable_only(&rig.scratch.backup().join("accounts.json"));
        let live_path = rig.claude.credentials_file();

        let (error, held_live) = thread::scope(|scope| {
            // Once the forward credential write lands, pin the new credential file so the rollback cannot replace it;
            // the sequence write, still held, fails after its retries.
            let pinner = scope.spawn(|| {
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    if fs::read_to_string(&live_path)
                        .is_ok_and(|text| text.contains("fake-access-two"))
                    {
                        return readable_only(&live_path);
                    }
                    assert!(
                        Instant::now() < deadline,
                        "the forward credential write never landed"
                    );
                    thread::sleep(Duration::from_millis(1));
                }
            });
            let error = switch_to(&rig, 2).expect_err("the sequence write fails");
            (error, pinner.join().expect("the pinner finishes"))
        });
        drop(held_live);
        drop(held_sequence);

        assert_eq!(error.kind(), SwitchErrorKind::RollbackFailed);
        assert_eq!(
            error.login_state(),
            Some(LoginState::Mixed),
            "the config is back, the credentials are the target's"
        );
        assert!(
            live(&rig).contains("fake-access-two"),
            "the credentials stay on the target"
        );
        let config = fs::read_to_string(rig.claude.global_config()).expect("the config reads");
        assert!(!config.contains(TWO), "the config is account 1's again");
    }

    #[test]
    fn a_live_write_that_leaves_its_temp_reports_it() {
        let own = fresh("fake-access-a", "fake-refresh-a");
        let rig = rig(
            "switch_temp_left",
            &own,
            &own,
            &fresh("fake-access-two", "fake-refresh-two"),
        );
        let claude_dir = rig.scratch.dir.join("claude");
        let held = readable_only(&rig.claude.credentials_file());
        let pinner = pin_first_temp(claude_dir.clone());

        let error = switch_to(&rig, 2).expect_err("the live write fails");
        drop(held);
        let pinned = pinner
            .join()
            .expect("the live write's temp file was pinned");

        assert_eq!(error.kind(), SwitchErrorKind::WriteFailed);
        assert_eq!(error.login_state(), Some(LoginState::Source));
        assert!(
            error.temps_left().credentials,
            "the credential write's temp file is reported"
        );
        assert!(!error.temps_left().config);
        assert_eq!(
            temp_files(&claude_dir).len(),
            1,
            "the temp file is still beside the live file"
        );
        assert_eq!(live(&rig), own, "the live file is unchanged");
        drop(pinned);
    }
}

type Hook = Box<dyn FnOnce() + Send>;

/// A write that changes the world between the switch's checks before its locks and the ones under them, set on the
/// rig's store ([`claude_accounts::Store::on_switch_locks`]) just before the switch is called.
struct BeforeLocks(std::sync::Mutex<Option<Hook>>);

impl BeforeLocks {
    fn new(hook: impl FnOnce() + Send + 'static) -> Self {
        Self(std::sync::Mutex::new(Some(Box::new(hook))))
    }

    /// Sets the write on `rig`'s store, to run once in its next switch.
    fn arm(&self, rig: &Rig) {
        if let Some(hook) = self
            .0
            .lock()
            .expect("no test panics holding the hook")
            .take()
        {
            rig.store.on_switch_locks(hook);
        }
    }
}

/// A paid move from account 1 to account 2.
fn paid_move() -> SwitchPrecondition {
    SwitchPrecondition {
        expected_outgoing: 1,
        target_must_be_paid_opted_in: true,
        outgoing_must_be_opted_out: false,
    }
}

/// A paid move to account 2, with `before` written between the checks before the locks and the ones under them.
fn paid_switch_with(rig: &Rig, before: &BeforeLocks) -> Result<SwitchOutcome, SwitchError> {
    before.arm(rig);
    switch(
        &rig.context(),
        2,
        &OutgoingProbe::unresolved(),
        Some(paid_move()),
    )
}

fn opt_in_two(rig: &Rig) {
    edit_sequence(rig, |sequence| {
        sequence["accounts"]["2"]["maySpendCredits"] = json!(true);
    });
}

/// The live login moves to another login after the paid move was decided and before the switch holds its locks: the
/// re-check under the locks refuses it, and nothing but the move itself is written.
#[test]
fn a_login_moved_before_the_switch_locks_refuses_the_paid_move() {
    let own = fresh("fake-access-a", "fake-refresh-a");
    let rig = rig(
        "paid_login_moved",
        &own,
        &own,
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    opt_in_two(&rig);
    let global_path = rig.claude.global_config().to_path_buf();
    let moved = serde_json::to_string_pretty(&global_config(STRANGER, ""))
        .expect("the global config serializes");
    let mut expected = files(&rig);
    expected.insert(global_path.clone(), moved.clone().into_bytes());
    let before_locks = BeforeLocks::new(move || {
        fs::write(&global_path, moved).expect("the global config can be written");
    });

    let error =
        paid_switch_with(&rig, &before_locks).expect_err("a moved login refuses the paid move");

    assert_eq!(error.kind(), SwitchErrorKind::PreconditionFailed);
    assert_eq!(
        files(&rig),
        expected,
        "nothing is written but the login's own move"
    );
    assert_eq!(active(&rig), Some(1));

    let error = switch(
        &rig.context(),
        2,
        &OutgoingProbe::unresolved(),
        Some(paid_move()),
    )
    .expect_err("still not account 1");
    assert_eq!(
        error.kind(),
        SwitchErrorKind::PreconditionFailed,
        "the check before the locks refuses it too"
    );
}

/// The target's opt-in is cleared after the paid move was decided and before the switch holds its locks: the re-check
/// under the locks refuses it with nothing written. A target never opted in is refused before the locks, and one still
/// opted in switches.
#[test]
fn a_target_whose_flag_was_cleared_refuses_the_paid_move() {
    let own = fresh("fake-access-a", "fake-refresh-a");
    let target = fresh("fake-access-two", "fake-refresh-two");
    let flagged = rig("paid_flag_cleared", &own, &own, &target);
    let error = switch(
        &flagged.context(),
        2,
        &OutgoingProbe::unresolved(),
        Some(paid_move()),
    )
    .expect_err("account 2 is not opted in");
    assert_eq!(error.kind(), SwitchErrorKind::PreconditionFailed);

    opt_in_two(&flagged);
    let sequence_path = flagged.scratch.backup().join("accounts.json");
    let before = files(&flagged);
    let mut cleared =
        json_of(&fs::read_to_string(&sequence_path).expect("accounts.json can be read"));
    cleared["accounts"]["2"]
        .as_object_mut()
        .expect("account 2 is an object")
        .shift_remove("maySpendCredits");
    let cleared = cleared.to_string();
    let mut expected = before;
    expected.insert(sequence_path.clone(), cleared.clone().into_bytes());
    let before_locks = BeforeLocks::new(move || {
        fs::write(&sequence_path, cleared).expect("accounts.json can be written");
    });

    let error = paid_switch_with(&flagged, &before_locks)
        .expect_err("a cleared flag refuses the paid move");

    assert_eq!(error.kind(), SwitchErrorKind::PreconditionFailed);
    assert_eq!(
        files(&flagged),
        expected,
        "nothing is written but the flag's own change"
    );
    assert_eq!(active(&flagged), Some(1));

    let disabled = rig("paid_target_disabled", &own, &own, &target);
    opt_in_two(&disabled);
    edit_sequence(&disabled, |sequence| {
        sequence["accounts"]["2"]["disabled"] = json!(true);
    });
    let error = switch(
        &disabled.context(),
        2,
        &OutgoingProbe::unresolved(),
        Some(paid_move()),
    )
    .expect_err("a disabled target");
    assert_eq!(error.kind(), SwitchErrorKind::PreconditionFailed);

    let kept = rig("paid_flag_kept", &own, &own, &target);
    opt_in_two(&kept);
    let outcome = switch(
        &kept.context(),
        2,
        &OutgoingProbe::unresolved(),
        Some(paid_move()),
    )
    .expect("the precondition holds");
    assert!(outcome.switched);
    assert_eq!(active(&kept), Some(2));
}

/// The auto-switch engine's leave off account 1 once its "May spend credits" is off, to account 2.
fn opted_out_leave() -> SwitchPrecondition {
    SwitchPrecondition {
        expected_outgoing: 1,
        target_must_be_paid_opted_in: false,
        outgoing_must_be_opted_out: true,
    }
}

/// The outgoing account is opted in again after the leave was decided and before the switch holds its locks: the
/// re-check under the locks refuses the leave with nothing written but the flag's own change. An outgoing account
/// opted in already is refused before the locks, and one still opted out switches.
#[test]
fn an_opted_out_clause_refuses_once_the_outgoing_account_is_opted_in() {
    let own = fresh("fake-access-a", "fake-refresh-a");
    let target = fresh("fake-access-two", "fake-refresh-two");
    let opted_in = rig("leave_outgoing_opted_in", &own, &own, &target);
    edit_sequence(&opted_in, |sequence| {
        sequence["accounts"]["1"]["maySpendCredits"] = json!(true);
    });
    let error = switch(
        &opted_in.context(),
        2,
        &OutgoingProbe::unresolved(),
        Some(opted_out_leave()),
    )
    .expect_err("account 1 is opted in");
    assert_eq!(error.kind(), SwitchErrorKind::PreconditionFailed);

    let late = rig("leave_outgoing_opted_in_late", &own, &own, &target);
    let sequence_path = late.scratch.backup().join("accounts.json");
    let before = files(&late);
    let mut turned_on =
        json_of(&fs::read_to_string(&sequence_path).expect("accounts.json can be read"));
    turned_on["accounts"]["1"]["maySpendCredits"] = json!(true);
    let turned_on = turned_on.to_string();
    let mut expected = before;
    expected.insert(sequence_path.clone(), turned_on.clone().into_bytes());
    BeforeLocks::new(move || {
        fs::write(&sequence_path, turned_on).expect("accounts.json can be written");
    })
    .arm(&late);
    let error = switch(
        &late.context(),
        2,
        &OutgoingProbe::unresolved(),
        Some(opted_out_leave()),
    )
    .expect_err("a flag turned back on refuses the leave");
    assert_eq!(error.kind(), SwitchErrorKind::PreconditionFailed);
    assert_eq!(
        files(&late),
        expected,
        "nothing is written but the flag's own change"
    );
    assert_eq!(active(&late), Some(1));

    let kept = rig("leave_outgoing_opted_out", &own, &own, &target);
    let outcome = switch(
        &kept.context(),
        2,
        &OutgoingProbe::unresolved(),
        Some(opted_out_leave()),
    )
    .expect("the precondition holds");
    assert!(outcome.switched);
    assert_eq!(active(&kept), Some(2));
}

/// The target passes the check before the locks, then is disabled before the switch holds them: the check under the
/// locks refuses the paid move, with nothing written but the disabling itself.
#[test]
fn the_in_lock_check_refuses_a_target_disabled_after_the_early_check() {
    let own = fresh("fake-access-a", "fake-refresh-a");
    let rig = rig(
        "paid_disabled_in_lock",
        &own,
        &own,
        &fresh("fake-access-two", "fake-refresh-two"),
    );
    opt_in_two(&rig);
    let sequence_path = rig.scratch.backup().join("accounts.json");
    let mut disabled =
        json_of(&fs::read_to_string(&sequence_path).expect("accounts.json can be read"));
    disabled["accounts"]["2"]["disabled"] = json!(true);
    let disabled = disabled.to_string();
    let mut expected = files(&rig);
    expected.insert(sequence_path.clone(), disabled.clone().into_bytes());
    let before_locks = BeforeLocks::new(move || {
        fs::write(&sequence_path, disabled).expect("accounts.json can be written");
    });

    let error = paid_switch_with(&rig, &before_locks)
        .expect_err("a target disabled under the locks refuses the paid move");

    assert_eq!(error.kind(), SwitchErrorKind::PreconditionFailed);
    assert_eq!(
        files(&rig),
        expected,
        "nothing is written but the disabling"
    );
    assert_eq!(active(&rig), Some(1));
}
