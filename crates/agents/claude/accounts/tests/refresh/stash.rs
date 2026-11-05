//! The unclaimed stash: a successor the gate could not persist is parked as a sealed `.cred` entry with a manifest row,
//! and the next pass adopts it.

use std::collections::BTreeMap;
use std::fs;
#[cfg(windows)]
use std::fs::{File, OpenOptions};
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(windows)]
use claude_accounts::guard;
use claude_accounts::seal::{self, AssociatedData, SealPurpose};
use claude_accounts::{
    AccountLock, ConsumeLock, CredentialBlob, DiscardError, Drain, FileLock, GrantError,
    HeldReason, LockError, StashReason, Store, StoreErrorKind, credential_fingerprint,
    discard_held, held_entries,
};
use serde_json::{Value, json};

use crate::links::{junction, snapshot};
use crate::support::{
    FakeEndpoint, ONE_ORG, Rig, Scratch, access_of, credential, granted, ms_after, now,
    put_credentials, stored,
};

/// `FILE_SHARE_READ`: a handle opened with it lets others read the file but not replace or delete it.
#[cfg(windows)]
const FILE_SHARE_READ: u32 = 1;

fn expired() -> String {
    credential("fake-access-one", Some("fake-refresh-one"), ms_after(-3600))
}

fn fingerprint(text: &str) -> Option<String> {
    credential_fingerprint(&CredentialBlob::new(text.to_owned()))
}

fn grant() -> FakeEndpoint {
    FakeEndpoint::answering(vec![granted(
        "fake-access-new",
        Some("fake-refresh-new"),
        3600,
    )])
}

/// The manifest's one row: its id and its fields.
fn only_row(scratch: &Scratch) -> (String, Value) {
    let manifest = scratch.manifest();
    assert_eq!(manifest["schemaVersion"], 1);
    let entries = manifest["entries"]
        .as_object()
        .expect("the manifest has entries");
    assert_eq!(entries.len(), 1, "one row was stashed");
    let (id, row) = entries.iter().next().expect("one row");
    (id.clone(), row.clone())
}

/// Checks the stashed row carries the row fields for `reason`, and its bytes are the successor.
fn assert_stashed(scratch: &Scratch, reason: StashReason) {
    let (id, row) = only_row(scratch);
    let keys: Vec<&String> = row
        .as_object()
        .expect("a row is an object")
        .keys()
        .collect();
    assert_eq!(
        keys,
        [
            "createdAt",
            "reason",
            "configSlot",
            "consumedFp",
            "fingerprint",
            "organization"
        ],
        "the row fields, in order, then the organization the entry is sealed with"
    );
    assert_eq!(row["createdAt"], "2026-09-21T14:13:20Z");
    assert_eq!(row["reason"], reason.kind());
    assert_eq!(row["configSlot"], "1");
    assert_eq!(row["organization"], ONE_ORG);
    assert_eq!(
        row["consumedFp"],
        json!(fingerprint(&expired())),
        "keyed to the generation the POST consumed"
    );
    let successor = scratch
        .stash_entry(&id)
        .expect("the entry's bytes were written");
    assert_eq!(access_of(&successor), "fake-access-new");
    assert_eq!(row["fingerprint"], json!(fingerprint(&successor)));
    assert!(
        id.starts_with("20260921T141320-"),
        "the id starts with the UTC timestamp: {id}"
    );
}

fn rig_with(test: &str, hook: impl FnOnce(&Scratch) -> Box<dyn FnOnce() + Send>) -> Rig {
    let scratch = Scratch::new(test);
    scratch.set_active(2);
    put_credentials(&scratch.root(), &expired());
    let during = hook(&scratch);
    Rig::new(scratch, grant().during_post(during))
}

#[test]
fn cas_conflict_stashes_successor_and_adopts_the_store() {
    let other = credential(
        "fake-access-other",
        Some("fake-refresh-other"),
        ms_after(7200),
    );
    let written = other.clone();
    let rig = rig_with("cas_conflict", |scratch| {
        let root = scratch.root();
        Box::new(move || put_credentials(&root, &written))
    });

    let outcome = rig.consume(&expired());

    assert_eq!(
        outcome.error, None,
        "a CAS conflict leaves the slot freshened"
    );
    assert_eq!(outcome.stash, Some(StashReason::CasConflict));
    assert_eq!(
        outcome
            .credentials
            .expect("the store's credential is adopted")
            .expose(),
        other
    );
    assert_eq!(
        rig.stored().as_deref(),
        Some(other.as_str()),
        "the racing writer's credential stays"
    );
    assert_stashed(&rig.scratch, StashReason::CasConflict);
}

#[test]
fn slot_removed_during_the_post_stashes_without_writing() {
    let rig = rig_with("slot_removed", |scratch| {
        let cred = scratch.credential_file();
        Box::new(move || fs::remove_file(cred).expect("the credential file can be removed"))
    });

    let outcome = rig.consume(&expired());

    assert_eq!(outcome.error, None, "a removed slot is not demoted");
    assert_eq!(outcome.stash, Some(StashReason::SlotRemoved));
    assert_eq!(rig.stored(), None, "the removed slot is not resurrected");
    assert_stashed(&rig.scratch, StashReason::SlotRemoved);
}

#[test]
fn store_unreadable_after_the_post_stashes_and_demotes() {
    let rig = rig_with("store_unreadable", |scratch| {
        let cred = scratch.credential_file();
        Box::new(move || {
            fs::remove_file(&cred).expect("the credential file can be removed");
            fs::create_dir(&cred).expect("a folder can stand where the file was");
        })
    });

    let outcome = rig.consume(&expired());

    assert_eq!(
        outcome.error,
        Some(GrantError::Transient),
        "the slot still holds the spent generation"
    );
    assert_eq!(outcome.stash, Some(StashReason::StoreUnreadable));
    let credentials = outcome.credentials.expect("the successor rides along");
    assert_eq!(access_of(credentials.expose()), "fake-access-new");
    assert_stashed(&rig.scratch, StashReason::StoreUnreadable);
}

#[cfg(windows)]
#[test]
fn persist_failure_stashes_and_demotes() {
    let holder: Arc<Mutex<Option<File>>> = Arc::new(Mutex::new(None));
    let parked = Arc::clone(&holder);
    let rig = rig_with("persist_failed", |scratch| {
        let cred = scratch.credential_file();
        Box::new(move || {
            let file = OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ)
                .open(cred)
                .expect("the credential file can be opened");
            *parked.lock().expect("the test holds no lock") = Some(file);
        })
    });

    let outcome = rig.consume(&expired());
    drop(holder.lock().expect("the gate holds no lock").take());

    assert_eq!(
        outcome.error,
        Some(GrantError::Transient),
        "the slot still holds the spent generation"
    );
    assert_eq!(outcome.stash, Some(StashReason::PersistFailed));
    assert_eq!(
        rig.stored().as_deref(),
        Some(expired().as_str()),
        "the write could not replace the file"
    );
    assert_stashed(&rig.scratch, StashReason::PersistFailed);
}

/// Every file under `folder` but lock files, by its path relative to it, with its bytes.
fn files_but_locks(folder: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut pending = vec![folder.to_path_buf()];
    while let Some(current) = pending.pop() {
        for entry in fs::read_dir(&current).expect("the folder lists") {
            let path = entry.expect("a folder entry can be read").path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_none_or(|extension| extension != "lock") {
                let name = path
                    .strip_prefix(folder)
                    .expect("the file is under the folder")
                    .to_string_lossy()
                    .into_owned();
                files.insert(name, fs::read(&path).expect("a file can be read"));
            }
        }
    }
    files
}

/// `accounts.json` turns newer while the token endpoint spends the grant: the successor is never refused on the store's
/// version, so it lands in the stash under a manifest row and opens, and nothing else in the store changes.
#[test]
fn a_successor_spent_while_the_store_turned_newer_is_kept() {
    let mut bumped = Vec::new();
    let mut before = BTreeMap::new();
    let rig = rig_with("successor_kept_in_newer_store", |scratch| {
        let path = scratch.root().accounts_json();
        let mut accounts: Value =
            serde_json::from_slice(&fs::read(&path).expect("accounts.json can be read"))
                .expect("it is JSON");
        accounts["schemaVersion"] = json!(2);
        bumped = accounts.to_string().into_bytes();
        before = files_but_locks(scratch.root().root());
        let written = bumped.clone();
        Box::new(move || fs::write(path, written).expect("accounts.json can be rewritten"))
    });

    let outcome = rig.consume(&expired());

    assert_eq!(
        outcome.error,
        Some(GrantError::Transient),
        "the slot still holds the spent generation"
    );
    assert_eq!(
        outcome.stash,
        Some(StashReason::PersistFailed),
        "the store refused the write, so the successor is stashed"
    );
    assert_stashed(&rig.scratch, StashReason::PersistFailed);
    let (id, _) = only_row(&rig.scratch);
    let mut after = files_but_locks(rig.scratch.root().root());
    let entry = format!(
        "credentials{}.unclaimed-{id}.cred",
        std::path::MAIN_SEPARATOR
    );
    let manifest = format!(
        "credentials{}.unclaimed-manifest.json",
        std::path::MAIN_SEPARATOR
    );
    assert!(after.remove(&entry).is_some(), "the entry is in the store");
    assert!(
        after.remove(&manifest).is_some(),
        "the manifest is in the store"
    );
    before.insert("accounts.json".to_owned(), bumped);
    assert_eq!(after, before, "nothing else in the store changed");
}

/// A successor the persist path stashes beside a manifest of a newer version is kept once, without a row, and the
/// fallback stash does not park the same successor a second time.
#[test]
fn a_kept_rescue_is_stashed_once() {
    let newer = json!({ "schemaVersion": 2, "entries": {} }).to_string();
    let rig = rig_with("kept_rescue_once", |scratch| {
        fs::write(scratch.manifest_path(), &newer).expect("the newer manifest can be written");
        let cred = scratch.credential_file();
        Box::new(move || fs::remove_file(cred).expect("the credential file can be removed"))
    });

    let outcome = rig.consume(&expired());

    assert_eq!(
        outcome.stash,
        Some(StashReason::Unpersisted),
        "no row names the kept successor"
    );
    let credentials = outcome.credentials.expect("the successor rides along");
    assert_eq!(access_of(credentials.expose()), "fake-access-new");
    let kept = entry_files(&rig.scratch);
    assert_eq!(
        kept.len(),
        1,
        "the successor is kept exactly once: {kept:?}"
    );
    let manifest = fs::read_to_string(rig.scratch.manifest_path()).expect("the manifest stays");
    assert_eq!(manifest, newer, "the newer manifest keeps its bytes");
}

/// `credentials/` swapped for a junction to a folder outside the root that holds the spent generation: the pass stops at
/// the consume lock, so nothing is locked, read or written through it, and the folder behind it is unchanged. The swap
/// is made before the pass, since the consume lock held open inside the folder keeps it from being renamed during one.
/// The stash write itself refusing the junction is the unit test `a_junctioned_credentials_folder_is_never_stashed_into`
/// in `src/refresh/stash.rs`.
#[test]
fn a_junctioned_credentials_folder_stops_the_refresh_before_any_write() {
    let mut planted = None;
    let rig = rig_with("stash_junctioned_credentials", |scratch| {
        let credentials = scratch.root().credentials_dir();
        let elsewhere = scratch.dir.join("outside-credentials");
        fs::rename(&credentials, &elsewhere).expect("the credentials folder can be moved aside");
        junction(&credentials, &elsewhere);
        let before = snapshot(&elsewhere);
        planted = Some((elsewhere, before));
        Box::new(|| {})
    });

    let outcome = rig.consume(&expired());

    assert_eq!(
        outcome.error,
        Some(GrantError::Transient),
        "the consume lock is not taken through the junction"
    );
    assert_eq!(
        outcome.stash, None,
        "nothing was spent, so nothing is stashed"
    );
    let (elsewhere, before) = planted.expect("the junction was planted");
    assert_eq!(
        snapshot(&elsewhere),
        before,
        "the folder behind the junction is unchanged"
    );
}

/// The `.unclaimed-*.cred` files in the stash's folder, whether or not a row names them.
fn entry_files(scratch: &Scratch) -> Vec<String> {
    let manifest = scratch.manifest_path();
    let dir = manifest
        .parent()
        .expect("the manifest sits in the credentials folder");
    fs::read_dir(dir)
        .expect("the credentials folder can be listed")
        .map(|entry| {
            entry
                .expect("a folder entry can be read")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|name| {
            name.starts_with(".unclaimed-")
                && Path::new(name)
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("cred"))
        })
        .collect()
}

/// A stash whose manifest cannot be read takes its entry's bytes back: no `.unclaimed-*.cred` is left that no row
/// names and nothing reads or sweeps.
#[test]
fn unreadable_manifest_leaves_no_unnamed_entry() {
    let rig = rig_with("manifest_unreadable", |scratch| {
        let cred = scratch.credential_file();
        let manifest = scratch.manifest_path();
        Box::new(move || {
            fs::remove_file(cred).expect("the credential file can be removed");
            fs::create_dir(manifest).expect("a folder can stand where the manifest goes");
        })
    });

    let outcome = rig.consume(&expired());

    assert_eq!(
        outcome.stash,
        Some(StashReason::Unpersisted),
        "neither stash could write its row"
    );
    let left = entry_files(&rig.scratch);
    assert!(
        left.is_empty(),
        "entry bytes no row names were left behind: {left:?}"
    );
}

#[cfg(windows)]
/// A stash whose manifest reads but cannot be replaced takes its entry's bytes back, as an unreadable one does.
#[test]
fn unwritable_manifest_leaves_no_unnamed_entry() {
    let holder: Arc<Mutex<Option<File>>> = Arc::new(Mutex::new(None));
    let parked = Arc::clone(&holder);
    let rig = rig_with("manifest_unwritable", |scratch| {
        let empty = json!({ "schemaVersion": 1, "entries": {} });
        fs::write(scratch.manifest_path(), empty.to_string()).expect("the manifest can be written");
        let cred = scratch.credential_file();
        let manifest = scratch.manifest_path();
        Box::new(move || {
            fs::remove_file(cred).expect("the credential file can be removed");
            let file = OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ)
                .open(manifest)
                .expect("the manifest can be opened");
            *parked.lock().expect("the test holds no lock") = Some(file);
        })
    });

    let outcome = rig.consume(&expired());
    drop(holder.lock().expect("the gate holds no lock").take());

    assert_eq!(
        outcome.stash,
        Some(StashReason::Unpersisted),
        "neither stash could write its row"
    );
    assert_eq!(
        rig.scratch.manifest()["entries"],
        json!({}),
        "the manifest kept no row"
    );
    let left = entry_files(&rig.scratch);
    assert!(
        left.is_empty(),
        "entry bytes no row names were left behind: {left:?}"
    );
}

#[test]
fn stashed_successor_is_adopted_without_a_post() {
    let scratch = Scratch::new("adopt_stash");
    scratch.set_active(2);
    put_credentials(&scratch.root(), &expired());
    let successor = credential(
        "fake-access-stashed",
        Some("fake-refresh-stashed"),
        ms_after(3600),
    );
    let id = "20260921T140000-abcdefabcdef-a1b2c3";
    scratch.write_stash_entry(id, &successor);
    let row = json!({
        "createdAt": "2026-09-21T14:00:00Z",
        "reason": "consume-gate-persist-failed",
        "configSlot": "1",
        "consumedFp": fingerprint(&expired()),
        "fingerprint": fingerprint(&successor),
        "organization": ONE_ORG,
    });
    let manifest = json!({ "schemaVersion": 1, "entries": { id: row } });
    fs::write(scratch.manifest_path(), manifest.to_string()).expect("the manifest can be written");
    let rig = Rig::new(scratch, grant());

    let outcome = rig.consume(&expired());

    assert!(rig.endpoint.bodies().is_empty(), "nothing was posted");
    assert_eq!(outcome.error, None);
    assert_eq!(
        outcome
            .credentials
            .expect("the adopted successor is handed back")
            .expose(),
        successor
    );
    assert_eq!(
        stored(&rig.scratch.root()).as_deref(),
        Some(successor.as_str()),
        "the pending persist is done"
    );
    assert_eq!(
        rig.scratch.manifest()["entries"],
        json!({}),
        "the adopted row is retired"
    );
    assert_eq!(
        rig.scratch.stash_bytes(id),
        None,
        "the adopted entry's file is removed"
    );
}

/// A row a switch filed for a live login no account owns is never adopted, even keyed to the stored generation.
#[test]
fn an_unmanaged_row_is_never_adopted() {
    let scratch = Scratch::new("unmanaged_row");
    scratch.set_active(2);
    put_credentials(&scratch.root(), &expired());
    let displaced = credential(
        "fake-access-stashed",
        Some("fake-refresh-stashed"),
        ms_after(3600),
    );
    let id = "20260921T140000-abcdefabcdef-a1b2c3";
    scratch.write_stash_entry_for(id, &displaced, "unmanaged", "");
    let row = json!({
        "createdAt": "2026-09-21T14:00:00Z",
        "reason": "displaced-live-login",
        "configSlot": "unmanaged",
        "consumedFp": fingerprint(&expired()),
        "fingerprint": fingerprint(&displaced),
        "organization": "",
    });
    let manifest = json!({ "schemaVersion": 1, "entries": { id: row } });
    fs::write(scratch.manifest_path(), manifest.to_string()).expect("the manifest can be written");
    let rig = Rig::new(scratch, grant());

    let outcome = rig.consume(&expired());

    assert_eq!(
        rig.endpoint.bodies().len(),
        1,
        "the grant is spent: the row was not adopted"
    );
    assert_eq!(outcome.error, None);
    assert_eq!(
        rig.scratch.manifest()["entries"][id],
        row,
        "the row is left alone"
    );
    assert_eq!(
        rig.scratch.stash_entry(id).as_deref(),
        Some(displaced.as_str()),
        "its bytes are kept"
    );
}

#[cfg(windows)]
/// What the stash writes is a sealed file bound to its slot: it opens under the slot's associated data alone, its bytes
/// carry no plaintext, and the next pass adopts it.
#[test]
fn a_stashed_entry_is_sealed_for_its_slot_and_opens_only_with_it() {
    let holder: Arc<Mutex<Option<File>>> = Arc::new(Mutex::new(None));
    let parked = Arc::clone(&holder);
    let rig = rig_with("stash_entry_sealed", |scratch| {
        let cred = scratch.credential_file();
        Box::new(move || {
            let file = OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ)
                .open(cred)
                .expect("the credential file can be opened");
            *parked.lock().expect("the test holds no lock") = Some(file);
        })
    });

    let outcome = rig.consume(&expired());
    drop(holder.lock().expect("the gate holds no lock").take());

    assert_eq!(
        outcome.stash,
        Some(StashReason::PersistFailed),
        "the successor was stashed"
    );
    let successor = outcome
        .credentials
        .expect("the successor rides along")
        .expose()
        .to_owned();
    let (id, _row) = only_row(&rig.scratch);
    let bytes = rig
        .scratch
        .stash_bytes(&id)
        .expect("the entry's file was written");
    assert!(
        bytes.starts_with(b"RTSEAL"),
        "the entry is a sealed file, not base64: {bytes:?}"
    );
    assert!(
        !holds(&bytes, successor.as_bytes()),
        "the sealed entry carries no plaintext"
    );
    assert!(
        !holds(&bytes, b"fake-access-new"),
        "the sealed entry carries no token"
    );
    let sealed = guard::read_file(&rig.scratch.backup(), &rig.scratch.stash_path(&id))
        .expect("the guard reads the entry");
    assert!(
        seal::open(
            SealPurpose::StashEntry,
            &AssociatedData::new("2", ""),
            &sealed
        )
        .is_err(),
        "another slot's associated data does not open it"
    );
    assert_eq!(
        seal::open(
            SealPurpose::StashEntry,
            &AssociatedData::new("1", ONE_ORG),
            &sealed
        )
        .expect("its own slot opens it")
        .expose(),
        successor,
        "the entry round-trips"
    );

    let adopted = rig.consume(&expired());

    assert_eq!(
        rig.endpoint.bodies().len(),
        1,
        "the sealed entry is adopted without a second post"
    );
    assert_eq!(adopted.error, None);
    assert_eq!(
        rig.scratch.stash_bytes(&id),
        None,
        "the adopted entry's file is removed"
    );
}

/// Whether `needle` occurs anywhere in `bytes`. Only the DPAPI-sealed entries on Windows are checked for tokens.
#[cfg(windows)]
fn holds(bytes: &[u8], needle: &[u8]) -> bool {
    bytes.windows(needle.len()).any(|window| window == needle)
}

/// `accounts.json` cannot be read after the POST: the successor is still stashed, sealed under the organization the gate
/// read before the POST and recorded on its row, and the next pass adopts it once the roster reads again.
#[test]
fn a_successor_stashed_while_the_roster_is_unreadable_is_adopted_once_it_reads_again() {
    let saved: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let kept = Arc::clone(&saved);
    let rig = rig_with("stash_roster_unreadable", |scratch| {
        let roster = scratch.backup().join("accounts.json");
        Box::new(move || {
            *kept.lock().expect("the test holds no lock") =
                fs::read(&roster).expect("accounts.json can be read");
            fs::write(&roster, b"{ not an account list").expect("accounts.json can be damaged");
        })
    });

    let outcome = rig.consume(&expired());

    assert_eq!(
        outcome.stash,
        Some(StashReason::StoreUnreadable),
        "the successor was stashed"
    );
    assert_eq!(
        outcome.error,
        Some(GrantError::Transient),
        "the slot still holds the spent generation"
    );
    let (id, row) = only_row(&rig.scratch);
    assert_eq!(
        row["organization"], ONE_ORG,
        "the row records the organization its entry is sealed with"
    );
    let bytes = rig
        .scratch
        .stash_bytes(&id)
        .expect("the entry's file was written");
    assert!(
        bytes.starts_with(b"RTSEAL"),
        "the entry is a sealed file: {bytes:?}"
    );
    // DPAPI seals the entry; off Windows the store is owner-only and unsealed (RT-916 decision 4).
    #[cfg(windows)]
    assert!(
        !holds(&bytes, b"fake-access-new"),
        "the sealed entry carries no token"
    );

    let roster = saved.lock().expect("the gate holds no lock").clone();
    fs::write(rig.scratch.backup().join("accounts.json"), roster)
        .expect("accounts.json can be put back");
    let adopted = rig.consume(&expired());

    assert_eq!(
        rig.endpoint.bodies().len(),
        1,
        "the stashed successor is adopted without a second post"
    );
    assert_eq!(adopted.error, None);
    let credentials = adopted
        .credentials
        .expect("the adopted successor is handed back");
    assert_eq!(access_of(credentials.expose()), "fake-access-new");
    assert_eq!(
        rig.stored().as_deref().map(access_of).as_deref(),
        Some("fake-access-new"),
        "the successor is the stored credential"
    );
    assert_eq!(
        rig.scratch.stash_bytes(&id),
        None,
        "the adopted entry's file is removed"
    );
}

/// An entry sealed for another slot cannot be adopted: it defers, and neither its bytes nor its row are deleted.
#[test]
fn an_entry_sealed_for_another_slot_is_refused() {
    let scratch = Scratch::new("stash_entry_other_slot");
    scratch.set_active(2);
    put_credentials(&scratch.root(), &expired());
    let successor = credential(
        "fake-access-stashed",
        Some("fake-refresh-stashed"),
        ms_after(3600),
    );
    let id = "20260921T140000-abcdefabcdef-a1b2c3";
    scratch.write_stash_entry_for(id, &successor, "2", "");
    let row = json!({
        "createdAt": "2026-09-21T14:00:00Z",
        "reason": "consume-gate-persist-failed",
        "configSlot": "1",
        "consumedFp": fingerprint(&expired()),
        "fingerprint": fingerprint(&successor),
        "organization": ONE_ORG,
    });
    let manifest = json!({ "schemaVersion": 1, "entries": { id: row } });
    fs::write(scratch.manifest_path(), manifest.to_string()).expect("the manifest can be written");
    let rig = Rig::new(scratch, grant());
    let before = rig.scratch.stash_bytes(id).expect("the entry exists");

    let outcome = rig.consume(&expired());

    assert_eq!(outcome.error, Some(GrantError::StashUnreadable));
    assert!(rig.endpoint.bodies().is_empty(), "nothing was posted");
    assert_eq!(
        rig.scratch.stash_bytes(id),
        Some(before),
        "the entry's bytes stay"
    );
    assert_eq!(rig.scratch.manifest()["entries"][id], row, "the row stays");
    assert_eq!(
        rig.stored().as_deref(),
        Some(expired().as_str()),
        "the store is untouched"
    );
}

/// Two entries of the same slot swapped between their ids each open, but neither is the credential its row's
/// `fingerprint` names: neither is adopted, nothing is written to the slot, and both entries and rows stay.
#[test]
fn an_entry_swapped_under_a_row_of_the_same_slot_is_not_adopted() {
    let scratch = Scratch::new("stash_entry_swapped");
    scratch.set_active(2);
    put_credentials(&scratch.root(), &expired());
    let first = credential(
        "fake-access-first",
        Some("fake-refresh-first"),
        ms_after(3600),
    );
    let second = credential(
        "fake-access-second",
        Some("fake-refresh-second"),
        ms_after(3600),
    );
    let first_id = "20260921T140000-abcdefabcdef-a1b2c3";
    let second_id = "20260921T140001-abcdefabcdef-d4e5f6";
    scratch.write_stash_entry(first_id, &first);
    scratch.write_stash_entry(second_id, &second);
    let first_sealed = fs::read(scratch.stash_path(first_id)).expect("the first entry reads");
    let second_sealed = fs::read(scratch.stash_path(second_id)).expect("the second entry reads");
    fs::write(scratch.stash_path(first_id), second_sealed)
        .expect("the second entry's bytes go under the first id");
    fs::write(scratch.stash_path(second_id), first_sealed)
        .expect("the first entry's bytes go under the second id");
    let row = |successor: &str| {
        json!({
            "createdAt": "2026-09-21T14:00:00Z",
            "reason": "consume-gate-persist-failed",
            "configSlot": "1",
            "consumedFp": fingerprint(&expired()),
            "fingerprint": fingerprint(successor),
            "organization": ONE_ORG,
        })
    };
    let entries = json!({ first_id: row(&first), second_id: row(&second) });
    let manifest = json!({ "schemaVersion": 1, "entries": entries });
    fs::write(scratch.manifest_path(), manifest.to_string()).expect("the manifest can be written");
    let rig = Rig::new(scratch, grant());
    let first_bytes = rig
        .scratch
        .stash_bytes(first_id)
        .expect("the first entry exists");
    let second_bytes = rig
        .scratch
        .stash_bytes(second_id)
        .expect("the second entry exists");

    let outcome = rig.consume(&expired());

    assert_eq!(
        outcome.error,
        Some(GrantError::StashUnreadable),
        "a mismatched entry defers as one that does not open"
    );
    assert!(rig.endpoint.bodies().is_empty(), "nothing was posted");
    assert_eq!(
        rig.stored().as_deref(),
        Some(expired().as_str()),
        "nothing was written to the slot"
    );
    assert_eq!(
        rig.scratch.stash_bytes(first_id),
        Some(first_bytes),
        "the first entry's bytes stay"
    );
    assert_eq!(
        rig.scratch.stash_bytes(second_id),
        Some(second_bytes),
        "the second entry's bytes stay"
    );
    assert_eq!(rig.scratch.manifest()["entries"], entries, "both rows stay");
}

/// A base64 entry left by the old format is not a sealed file: it defers, and nothing is deleted.
#[test]
fn a_base64_entry_left_by_the_old_format_reads_as_unreadable() {
    let scratch = Scratch::new("stash_entry_old_format");
    scratch.set_active(2);
    put_credentials(&scratch.root(), &expired());
    let successor = credential(
        "fake-access-stashed",
        Some("fake-refresh-stashed"),
        ms_after(3600),
    );
    let id = "20260921T140000-abcdefabcdef-a1b2c3";
    scratch.write_stash_raw(id, b"ZWFybGllcg==");
    let row = json!({
        "createdAt": "2026-09-21T14:00:00Z",
        "reason": "consume-gate-persist-failed",
        "configSlot": "1",
        "consumedFp": fingerprint(&expired()),
        "fingerprint": fingerprint(&successor),
        "organization": ONE_ORG,
    });
    let manifest = json!({ "schemaVersion": 1, "entries": { id: row } });
    fs::write(scratch.manifest_path(), manifest.to_string()).expect("the manifest can be written");
    let rig = Rig::new(scratch, grant());

    let outcome = rig.consume(&expired());

    assert_eq!(outcome.error, Some(GrantError::StashUnreadable));
    assert!(rig.endpoint.bodies().is_empty(), "nothing was posted");
    assert_eq!(
        rig.scratch.stash_bytes(id).as_deref(),
        Some(b"ZWFybGllcg==".as_slice()),
        "the old bytes stay"
    );
    assert_eq!(rig.scratch.manifest()["entries"][id], row, "the row stays");
}

/// A row keyed to another generation is left alone when its entry exists but does not open: an entry that cannot be read
/// is not an entry that is gone.
#[test]
fn an_undecodable_entry_does_not_retire_its_manifest_row() {
    let scratch = Scratch::new("stash_entry_undecodable_kept");
    scratch.set_active(2);
    put_credentials(&scratch.root(), &expired());
    let id = "20260921T140000-abcdefabcdef-a1b2c3";
    scratch.write_stash_raw(id, b"ZWFybGllcg==");
    let row = json!({
        "createdAt": "2026-09-21T14:00:00Z",
        "reason": "consume-gate-persist-failed",
        "configSlot": "1",
        "consumedFp": "sha256-full:another-generation",
        "fingerprint": null,
        "organization": ONE_ORG,
    });
    let manifest = json!({ "schemaVersion": 1, "entries": { id: row } });
    fs::write(scratch.manifest_path(), manifest.to_string()).expect("the manifest can be written");
    let rig = Rig::new(scratch, grant());

    let outcome = rig.consume(&expired());

    assert_eq!(
        outcome.error, None,
        "the unreadable row does not stop the grant"
    );
    assert_eq!(rig.endpoint.bodies().len(), 1, "the grant is spent once");
    assert_eq!(
        rig.scratch.manifest()["entries"][id],
        row,
        "the row is kept"
    );
    assert_eq!(
        rig.scratch.stash_bytes(id).as_deref(),
        Some(b"ZWFybGllcg==".as_slice()),
        "the bytes are kept"
    );
}

/// A row keyed to another generation whose entry is absent is retired, as it always was.
#[test]
fn an_absent_entry_retires_its_manifest_row() {
    let scratch = Scratch::new("stash_entry_absent_retired");
    scratch.set_active(2);
    put_credentials(&scratch.root(), &expired());
    let id = "20260921T140000-abcdefabcdef-a1b2c3";
    let row = json!({
        "createdAt": "2026-09-21T14:00:00Z",
        "reason": "consume-gate-persist-failed",
        "configSlot": "1",
        "consumedFp": "sha256-full:another-generation",
        "fingerprint": null,
        "organization": ONE_ORG,
    });
    let manifest = json!({ "schemaVersion": 1, "entries": { id: row } });
    fs::write(scratch.manifest_path(), manifest.to_string()).expect("the manifest can be written");
    let rig = Rig::new(scratch, grant());

    let outcome = rig.consume(&expired());

    assert_eq!(outcome.error, None);
    assert_eq!(rig.endpoint.bodies().len(), 1, "the grant is spent once");
    assert_eq!(
        rig.scratch.manifest()["entries"],
        json!({}),
        "the absent row is retired"
    );
}

#[test]
fn unreadable_manifest_defers_with_stash_unreadable() {
    let scratch = Scratch::new("unreadable_manifest");
    scratch.set_active(2);
    put_credentials(&scratch.root(), &expired());
    fs::create_dir(scratch.manifest_path()).expect("a folder can stand where the manifest goes");
    let rig = Rig::new(scratch, grant());

    let outcome = rig.consume(&expired());

    assert_eq!(outcome.error, Some(GrantError::StashUnreadable));
    assert_eq!(GrantError::StashUnreadable.kind(), "stash-unreadable");
    assert!(rig.endpoint.bodies().is_empty(), "nothing was posted");
    assert_eq!(
        rig.stored().as_deref(),
        Some(expired().as_str()),
        "the store is untouched"
    );
}

/// A row whose id climbs out of the stash's name pattern, to another file in the stash's folder, is never followed: a
/// CAS-conflict row of the account would otherwise be retired, and its "bytes" deleted.
#[test]
fn stash_row_with_a_path_id_is_ignored() {
    let scratch = Scratch::new("path_id");
    scratch.set_active(2);
    put_credentials(&scratch.root(), &expired());
    let other = scratch.backup().join("credentials").join("2.cred");
    // Another account's credential file: the row's path resolves to it and must stay unread and in place.
    fs::write(&other, "b3RoZXI=").expect("the other account's credential can be written");
    let id = r"x\..\2";
    let row = json!({
        "createdAt": "2026-09-21T14:00:00Z",
        "reason": StashReason::CasConflict.kind(),
        "configSlot": "1",
        "consumedFp": "sha256-full:another-generation",
        "fingerprint": null,
        "organization": ONE_ORG,
    });
    let manifest = json!({ "schemaVersion": 1, "entries": { id: row } });
    fs::write(scratch.manifest_path(), manifest.to_string()).expect("the manifest can be written");
    let rig = Rig::new(scratch, grant());

    let outcome = rig.consume(&expired());

    assert!(
        other.exists(),
        "the other account's credential file is never deleted"
    );
    assert_eq!(
        outcome.error, None,
        "the row is skipped and the grant spent"
    );
    assert_eq!(
        rig.scratch.manifest()["entries"][id],
        row,
        "the row is left alone"
    );
}

/// A stashed successor of the stored generation, its manifest row written by hand.
fn pending_row(scratch: &Scratch, id: &str, successor: &str, organization: Option<&str>) -> Value {
    let mut row = json!({
        "createdAt": "2026-09-21T14:00:00Z",
        "reason": "consume-gate-persist-failed",
        "configSlot": "1",
        "consumedFp": fingerprint(&expired()),
        "fingerprint": fingerprint(successor),
    });
    if let Some(organization) = organization {
        row["organization"] = json!(organization);
    }
    let manifest = json!({ "schemaVersion": 1, "entries": { id: row } });
    fs::write(scratch.manifest_path(), manifest.to_string()).expect("the manifest can be written");
    row
}

/// The row's recorded organization opens the entry, not the account's: an entry sealed with none is adopted while the
/// roster names account 1's organization.
#[test]
fn an_entry_opens_with_the_organization_its_row_records() {
    let scratch = Scratch::new("stash_row_organization_empty");
    scratch.set_active(2);
    put_credentials(&scratch.root(), &expired());
    let successor = credential(
        "fake-access-stashed",
        Some("fake-refresh-stashed"),
        ms_after(3600),
    );
    let id = "20260921T140000-abcdefabcdef-a1b2c3";
    scratch.write_stash_entry_for(id, &successor, "1", "");
    pending_row(&scratch, id, &successor, Some(""));
    let rig = Rig::new(scratch, grant());

    let outcome = rig.consume(&expired());

    assert!(
        rig.endpoint.bodies().is_empty(),
        "the entry is adopted without a post"
    );
    assert_eq!(outcome.error, None);
    assert_eq!(
        outcome
            .credentials
            .expect("the adopted successor is handed back")
            .expose(),
        successor
    );
    assert_eq!(
        rig.scratch.stash_bytes(id),
        None,
        "the adopted entry's file is removed"
    );
}

/// A row that records no organization opens its entry with none, never with the roster's: an entry sealed with the
/// account's organization then does not open, and defers with its bytes and row kept.
#[test]
fn a_row_without_an_organization_never_borrows_the_accounts() {
    let scratch = Scratch::new("stash_row_organization_missing");
    scratch.set_active(2);
    put_credentials(&scratch.root(), &expired());
    let successor = credential(
        "fake-access-stashed",
        Some("fake-refresh-stashed"),
        ms_after(3600),
    );
    let id = "20260921T140000-abcdefabcdef-a1b2c3";
    scratch.write_stash_entry(id, &successor);
    let row = pending_row(&scratch, id, &successor, None);
    let rig = Rig::new(scratch, grant());
    let before = rig.scratch.stash_bytes(id).expect("the entry exists");

    let outcome = rig.consume(&expired());

    assert_eq!(outcome.error, Some(GrantError::StashUnreadable));
    assert!(rig.endpoint.bodies().is_empty(), "nothing was posted");
    assert_eq!(
        rig.scratch.stash_bytes(id),
        Some(before),
        "the entry's bytes stay"
    );
    assert_eq!(rig.scratch.manifest()["entries"][id], row, "the row stays");
}

/// The ids the held-entry tests stash under.
const HELD_ID: &str = "20260921T140000-abcdefabcdef-a1b2c3";
const OTHER_ID: &str = "20260921T140001-abcdefabcdef-d4e5f6";

/// A successor stashed for account 1, whose tokens the held-entry tests look for in what they are handed.
fn held_successor() -> String {
    credential(
        "fake-access-held",
        Some("fake-refresh-held"),
        ms_after(3600),
    )
}

/// A row of account 1 keyed to the stored generation, recording `fingerprint_field`.
fn held_row(fingerprint_field: &Value) -> Value {
    json!({
        "createdAt": "2026-09-21T14:00:00Z",
        "reason": "consume-gate-persist-failed",
        "configSlot": "1",
        "consumedFp": fingerprint(&expired()),
        "fingerprint": fingerprint_field,
        "organization": ONE_ORG,
    })
}

fn write_manifest(scratch: &Scratch, entries: &Value) {
    let manifest = json!({ "schemaVersion": 1, "entries": entries });
    fs::write(scratch.manifest_path(), manifest.to_string()).expect("the manifest can be written");
}

/// A scratch store whose account 1 stores [`expired`] and holds one entry under [`HELD_ID`] sealed for slot 2, so it does
/// not open as account 1's.
fn held_scratch(test: &str) -> (Scratch, Value) {
    let scratch = Scratch::new(test);
    scratch.set_active(2);
    put_credentials(&scratch.root(), &expired());
    scratch.write_stash_entry_for(HELD_ID, &held_successor(), "2", ONE_ORG);
    let row = held_row(&json!(fingerprint(&held_successor())));
    write_manifest(&scratch, &json!({ HELD_ID: row }));
    (scratch, row)
}

fn store_of(scratch: &Scratch) -> Store {
    Store::open(scratch.root()).expect("the scratch store opens")
}

/// Each held entry's id, account and reason.
fn held_summary(scratch: &Scratch) -> Vec<(String, u32, HeldReason)> {
    held_entries(&store_of(scratch))
        .into_iter()
        .map(|held| (held.id, held.number, held.reason))
        .collect()
}

#[test]
fn held_entries_lists_an_entry_sealed_for_another_slot_as_does_not_open() {
    let (scratch, _) = held_scratch("held_other_slot");

    let held = held_entries(&store_of(&scratch));

    assert_eq!(held.len(), 1, "one entry is held: {held:?}");
    assert_eq!(held[0].id, HELD_ID);
    assert_eq!(held[0].number, 1);
    assert_eq!(held[0].reason, HeldReason::DoesNotOpen);
    assert_eq!(
        held[0].created_at,
        Some("2026-09-21T14:00:00Z".parse().expect("a time"))
    );
}

#[test]
fn held_entries_lists_a_swapped_entry_as_does_not_match_its_row() {
    let scratch = Scratch::new("held_swapped");
    scratch.set_active(2);
    put_credentials(&scratch.root(), &expired());
    let other = credential(
        "fake-access-other",
        Some("fake-refresh-other"),
        ms_after(3600),
    );
    scratch.write_stash_entry(HELD_ID, &other);
    write_manifest(
        &scratch,
        &json!({ HELD_ID: held_row(&json!(fingerprint(&held_successor()))) }),
    );

    assert_eq!(
        held_summary(&scratch),
        [(HELD_ID.to_owned(), 1, HeldReason::DoesNotMatchItsRow)]
    );
}

#[test]
fn held_entries_lists_a_row_with_no_fingerprint_as_does_not_match_its_row() {
    let scratch = Scratch::new("held_no_fingerprint");
    scratch.set_active(2);
    put_credentials(&scratch.root(), &expired());
    scratch.write_stash_entry(HELD_ID, &held_successor());
    write_manifest(&scratch, &json!({ HELD_ID: held_row(&Value::Null) }));

    assert_eq!(
        held_summary(&scratch),
        [(HELD_ID.to_owned(), 1, HeldReason::DoesNotMatchItsRow)]
    );
}

#[test]
fn held_entries_skips_a_row_whose_consumed_fp_is_not_the_stored_one() {
    let (scratch, mut row) = held_scratch("held_other_generation");
    row["consumedFp"] = json!("sha256-full:another-generation");
    write_manifest(&scratch, &json!({ HELD_ID: row }));

    assert!(
        held_summary(&scratch).is_empty(),
        "a dormant row holds nothing up"
    );
}

#[test]
fn held_entries_skips_foreign_and_unmanaged_rows() {
    let (scratch, row) = held_scratch("held_foreign_unmanaged");
    let mut foreign = row.clone();
    foreign["reason"] = json!("foreign");
    foreign["consumedFp"] = Value::Null;
    let mut unmanaged = row;
    unmanaged["configSlot"] = json!("unmanaged");
    scratch.write_stash_entry_for(OTHER_ID, &held_successor(), "2", ONE_ORG);
    write_manifest(&scratch, &json!({ HELD_ID: foreign, OTHER_ID: unmanaged }));

    assert!(
        held_summary(&scratch).is_empty(),
        "neither row is keyed to a stored generation"
    );
}

#[test]
fn held_entries_skips_a_row_with_a_malformed_id() {
    let (scratch, row) = held_scratch("held_malformed_id");
    write_manifest(&scratch, &json!({ r"x\..\1": row }));

    assert!(
        held_summary(&scratch).is_empty(),
        "an id not of the stash's form is never read"
    );
}

#[test]
fn held_entries_skips_a_row_whose_bytes_are_gone() {
    let (scratch, _) = held_scratch("held_bytes_gone");
    fs::remove_file(scratch.stash_path(HELD_ID)).expect("the entry can be removed");

    assert!(
        held_summary(&scratch).is_empty(),
        "a row without bytes is retired by the next pass, not held"
    );
    assert!(
        scratch.manifest()["entries"].get(HELD_ID).is_some(),
        "the list retires nothing"
    );
}

#[test]
fn held_entries_skips_an_account_with_an_adoptable_row() {
    let (scratch, row) = held_scratch("held_beside_adoptable");
    scratch.write_stash_entry(OTHER_ID, &held_successor());
    write_manifest(&scratch, &json!({ HELD_ID: row.clone(), OTHER_ID: row }));

    assert!(
        held_summary(&scratch).is_empty(),
        "the next pass adopts the matching row"
    );
}

#[test]
fn held_entries_never_returns_credential_bytes() {
    let (scratch, _) = held_scratch("held_no_bytes");

    let shown = format!("{:?}", held_entries(&store_of(&scratch)));

    assert!(shown.contains(HELD_ID), "the entry is listed: {shown}");
    for secret in [
        "fake-access-held",
        "fake-refresh-held",
        "fake-access-one",
        "fake-refresh-one",
        ONE_ORG,
        ".cred",
        "credentials",
    ] {
        assert!(
            !shown.contains(secret),
            "the list carries {secret}: {shown}"
        );
    }
    let fp = fingerprint(&held_successor()).expect("a fingerprint");
    assert!(
        !shown.contains(&fp),
        "the list carries the entry's fingerprint: {shown}"
    );
}

#[test]
fn held_entries_skips_an_account_the_roster_no_longer_names() {
    let (scratch, _) = held_scratch("held_account_left_the_roster");
    let accounts = scratch.backup().join("accounts.json");
    let mut sequence: Value =
        serde_json::from_slice(&fs::read(&accounts).expect("accounts.json reads"))
            .expect("it is JSON");
    sequence["accounts"]
        .as_object_mut()
        .expect("the roster has accounts")
        .remove("1");
    sequence["order"] = json!([2]);
    fs::write(&accounts, sequence.to_string()).expect("accounts.json can be written");
    // Sealed under the empty binding an account outside the roster is read with, so its credential still opens and only
    // the roster keeps the entry off the list.
    let sealed = seal::seal(
        SealPurpose::Credentials,
        &AssociatedData::credentials(1, "", 1, ""),
        &CredentialBlob::new(expired()),
    )
    .expect("the credential seals");
    fs::write(scratch.credential_file(), sealed.as_bytes()).expect("the credential can be written");
    assert_eq!(
        store_of(&scratch)
            .read_credentials(1)
            .ok()
            .flatten()
            .map(|blob| blob.expose().to_owned()),
        Some(expired()),
        "the stored credential opens without the roster"
    );

    assert!(
        held_summary(&scratch).is_empty(),
        "an entry of an account no discard can name is not offered"
    );
}

#[test]
fn held_entries_reports_nothing_beside_a_newer_manifest() {
    let (scratch, row) = held_scratch("held_newer_manifest");
    let manifest = json!({ "schemaVersion": 2, "entries": { HELD_ID: row } });
    fs::write(scratch.manifest_path(), manifest.to_string()).expect("the manifest can be written");

    assert!(
        held_summary(&scratch).is_empty(),
        "a newer manifest is never written, so nothing is offered"
    );
}

/// Writes account 1's usage row as a `stash-unreadable` failure in backoff.
fn usage_in_stash_backoff(scratch: &Scratch) {
    let root = scratch.root();
    fs::create_dir_all(root.cache_dir()).expect("the cache folder can be made");
    let row = json!({ "lastError": "stash-unreadable", "consecutiveFailures": 3, "backoffUntil": 4_000_000_000.0_f64, "fetchedAt": 1.0 });
    let table = json!({ "schemaVersion": 3, "accounts": { "1": row } });
    fs::write(root.usage_json(), table.to_string()).expect("the usage table can be written");
}

fn usage_row(scratch: &Scratch) -> Value {
    let bytes = fs::read(scratch.root().usage_json()).expect("the usage table reads");
    let table: Value = serde_json::from_slice(&bytes).expect("the usage table is JSON");
    table["accounts"]["1"].clone()
}

#[test]
fn discard_deletes_the_entry_and_its_row_and_releases_the_refresh() {
    let (scratch, _) = held_scratch("discard_releases");
    usage_in_stash_backoff(&scratch);
    let store = store_of(&scratch);

    discard_held(&store, &idle_drain(), 1, HELD_ID, now()).expect("the held entry is discarded");

    assert_eq!(
        scratch.stash_bytes(HELD_ID),
        None,
        "the entry's file is deleted"
    );
    assert_eq!(
        scratch.manifest()["entries"],
        json!({}),
        "its row is removed"
    );
    let usage = usage_row(&scratch);
    assert_eq!(
        usage["lastError"],
        Value::Null,
        "the stash failure is cleared"
    );
    assert_eq!(usage["backoffUntil"], Value::Null, "the backoff is lifted");
    assert_eq!(usage["consecutiveFailures"], 0);
    assert_eq!(usage["fetchedAt"], 1.0, "the row's other fields stay");
    let rig = Rig::new(scratch, grant());
    let outcome = rig.consume(&expired());
    assert_eq!(outcome.error, None, "the refresh is no longer held");
    assert_eq!(rig.endpoint.bodies().len(), 1, "the grant is spent");
}

#[test]
fn discard_leaves_other_rows_of_the_account_and_other_accounts_untouched() {
    let (scratch, row) = held_scratch("discard_leaves_others");
    let mut other_account = row.clone();
    other_account["configSlot"] = json!("2");
    scratch.write_stash_entry_for(OTHER_ID, &held_successor(), "3", ONE_ORG);
    let third = "20260921T140002-abcdefabcdef-0a0b0c";
    scratch.write_stash_entry_for(third, &held_successor(), "3", ONE_ORG);
    write_manifest(
        &scratch,
        &json!({ HELD_ID: row.clone(), OTHER_ID: row.clone(), third: other_account.clone() }),
    );
    let other_bytes = scratch.stash_bytes(OTHER_ID);
    let third_bytes = scratch.stash_bytes(third);

    discard_held(&store_of(&scratch), &idle_drain(), 1, HELD_ID, now())
        .expect("the held entry is discarded");

    assert_eq!(
        scratch.manifest()["entries"],
        json!({ OTHER_ID: row, third: other_account }),
        "only its row goes"
    );
    assert_eq!(
        scratch.stash_bytes(OTHER_ID),
        other_bytes,
        "the account's other entry stays"
    );
    assert_eq!(
        scratch.stash_bytes(third),
        third_bytes,
        "another account's entry stays"
    );
    assert_eq!(
        stored(&scratch.root()).as_deref(),
        Some(expired().as_str()),
        "the stored credential is untouched"
    );
    assert_eq!(
        held_summary(&scratch),
        [(OTHER_ID.to_owned(), 1, HeldReason::DoesNotOpen)],
        "the other row still holds"
    );
}

#[test]
fn discard_refuses_an_entry_that_opens_and_matches() {
    let scratch = Scratch::new("discard_adoptable");
    scratch.set_active(2);
    put_credentials(&scratch.root(), &expired());
    scratch.write_stash_entry(HELD_ID, &held_successor());
    let row = held_row(&json!(fingerprint(&held_successor())));
    write_manifest(&scratch, &json!({ HELD_ID: row.clone() }));
    let before = scratch.stash_bytes(HELD_ID);

    let discarded = discard_held(&store_of(&scratch), &idle_drain(), 1, HELD_ID, now());

    assert!(
        matches!(discarded, Err(DiscardError::NotHeld)),
        "{discarded:?}"
    );
    assert_eq!(
        scratch.stash_bytes(HELD_ID),
        before,
        "an adoptable entry is never deleted"
    );
    assert_eq!(scratch.manifest()["entries"][HELD_ID], row);
}

#[test]
fn discard_refuses_when_the_stored_credential_changed_since_the_list() {
    let (scratch, row) = held_scratch("discard_after_capture");
    assert_eq!(held_summary(&scratch).len(), 1, "the entry is listed first");
    put_credentials(
        &scratch.root(),
        &credential(
            "fake-access-captured",
            Some("fake-refresh-captured"),
            ms_after(3600),
        ),
    );
    let before = scratch.stash_bytes(HELD_ID);

    let discarded = discard_held(&store_of(&scratch), &idle_drain(), 1, HELD_ID, now());

    assert!(
        matches!(discarded, Err(DiscardError::NotHeld)),
        "{discarded:?}"
    );
    assert_eq!(
        scratch.stash_bytes(HELD_ID),
        before,
        "the dormant entry stays"
    );
    assert_eq!(scratch.manifest()["entries"][HELD_ID], row);
}

#[test]
fn discard_refuses_a_newer_store_or_a_newer_manifest_and_deletes_nothing() {
    let (scratch, row) = held_scratch("discard_newer");
    let credentials = scratch.root().credentials_dir();
    let newer_manifest = json!({ "schemaVersion": 2, "entries": { HELD_ID: row } }).to_string();
    fs::write(scratch.manifest_path(), &newer_manifest).expect("the manifest can be written");
    let before = files_but_locks(&credentials);

    let manifest_refused = discard_held(&store_of(&scratch), &idle_drain(), 1, HELD_ID, now());

    assert!(
        matches!(manifest_refused, Err(DiscardError::NewerManifest)),
        "{manifest_refused:?}"
    );
    assert_eq!(
        files_but_locks(&credentials),
        before,
        "nothing changed beside the newer manifest"
    );

    write_manifest(
        &scratch,
        &json!({ HELD_ID: held_row(&json!(fingerprint(&held_successor()))) }),
    );
    let accounts = scratch.backup().join("accounts.json");
    let mut sequence: Value =
        serde_json::from_slice(&fs::read(&accounts).expect("accounts.json reads"))
            .expect("it is JSON");
    sequence["schemaVersion"] = json!(99);
    fs::write(&accounts, sequence.to_string()).expect("accounts.json can be written");
    let before = files_but_locks(&credentials);

    let store_refused = discard_held(&store_of(&scratch), &idle_drain(), 1, HELD_ID, now());

    assert!(
        matches!(store_refused, Err(DiscardError::Store(_))),
        "{store_refused:?}"
    );
    assert_eq!(
        files_but_locks(&credentials),
        before,
        "nothing changed in a newer store"
    );
}

#[cfg(windows)]
#[test]
fn discard_keeps_the_row_when_the_file_cannot_be_removed() {
    let (scratch, row) = held_scratch("discard_unremovable");
    let path = scratch.stash_path(HELD_ID);
    let handle = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(&path)
        .expect("the entry can be held open");

    let discarded = discard_held(&store_of(&scratch), &idle_drain(), 1, HELD_ID, now());

    drop(handle);
    assert!(
        matches!(discarded, Err(DiscardError::RemoveFailed)),
        "{discarded:?}"
    );
    assert!(path.exists(), "the entry's file stays");
    assert_eq!(scratch.manifest()["entries"][HELD_ID], row, "its row stays");
}

#[test]
fn discard_refuses_an_id_that_is_not_an_entry_id() {
    let (scratch, _) = held_scratch("discard_path_id");
    let other = scratch.root().credentials_dir().join("2.cred");
    let before = fs::read(&other).ok();

    let discarded = discard_held(&store_of(&scratch), &idle_drain(), 1, r"x\..\2", now());

    assert!(
        matches!(discarded, Err(DiscardError::NotAnEntryId)),
        "{discarded:?}"
    );
    assert_eq!(
        fs::read(&other).ok(),
        before,
        "no file the id names is touched"
    );
    assert!(
        scratch.stash_bytes(HELD_ID).is_some(),
        "the held entry stays"
    );
}

#[test]
fn discard_of_an_unknown_account_is_refused() {
    let (scratch, _) = held_scratch("discard_unknown_account");

    let discarded = discard_held(&store_of(&scratch), &idle_drain(), 99, HELD_ID, now());

    assert!(
        matches!(discarded, Err(DiscardError::UnknownAccount)),
        "{discarded:?}"
    );
    assert!(
        scratch.stash_bytes(HELD_ID).is_some(),
        "the held entry stays"
    );
}

#[test]
fn discard_through_a_linked_credentials_folder_deletes_nothing() {
    let (scratch, _) = held_scratch("discard_junctioned");
    let credentials = scratch.root().credentials_dir();
    let elsewhere = scratch.dir.join("outside-credentials");
    fs::rename(&credentials, &elsewhere).expect("the credentials folder can be moved aside");
    junction(&credentials, &elsewhere);
    let before = snapshot(&elsewhere);

    let discarded = discard_held(&store_of(&scratch), &idle_drain(), 1, HELD_ID, now());

    assert!(
        discarded.is_err(),
        "nothing is discarded through a junction"
    );
    assert_eq!(
        snapshot(&elsewhere),
        before,
        "the folder behind the junction is unchanged"
    );
}

#[test]
fn discard_removes_an_entry_that_is_a_directory_junction_and_never_what_it_points_at() {
    let scratch = Scratch::new("discard_junction_entry");
    scratch.set_active(2);
    put_credentials(&scratch.root(), &expired());
    let outside = scratch.dir.join("outside-entry");
    fs::create_dir_all(&outside).expect("the outside folder can be made");
    fs::write(outside.join("planted.txt"), b"not the stash's")
        .expect("the planted file can be written");
    junction(&scratch.stash_path(HELD_ID), &outside);
    write_manifest(
        &scratch,
        &json!({ HELD_ID: held_row(&json!(fingerprint(&held_successor()))) }),
    );
    let before = snapshot(&outside);
    assert_eq!(
        held_summary(&scratch),
        [(HELD_ID.to_owned(), 1, HeldReason::DoesNotOpen)],
        "a junction at the entry's name is listed as an entry that does not open"
    );

    let discarded = discard_held(&store_of(&scratch), &idle_drain(), 1, HELD_ID, now());

    assert!(discarded.is_ok(), "{discarded:?}");
    assert!(
        fs::symlink_metadata(scratch.stash_path(HELD_ID)).is_err(),
        "the junction itself is removed"
    );
    assert_eq!(
        scratch.manifest()["entries"],
        json!({}),
        "its row is removed"
    );
    assert_eq!(
        snapshot(&outside),
        before,
        "the folder the junction pointed at keeps its file"
    );
}

/// A discard's lock wait when a test means it to time out: far short of the real [`claude_accounts::ACCOUNT_LOCK_TIMEOUT`].
const SHORT_WAIT: Duration = Duration::from_millis(300);
/// A discard's lock wait when a test releases the lock it waits on: far longer than any test takes to see it waiting.
const LONG_WAIT: Duration = Duration::from_secs(60);
/// How long a lock-order test looks for the discard holding the earlier locks before it fails.
const HANG_GUARD: Duration = Duration::from_secs(30);

/// Whether another handle holds the file lock at `path`.
fn held_elsewhere(path: &Path) -> bool {
    matches!(
        FileLock::acquire(path, Duration::ZERO),
        Err(LockError::Timeout)
    )
}

/// Whether every lock at `paths` is seen held by another handle at once within [`HANG_GUARD`].
fn seen_held(paths: &[PathBuf]) -> bool {
    let deadline = Instant::now() + HANG_GUARD;
    while !paths.iter().all(|path| held_elsewhere(path)) {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(1));
    }
    true
}

#[test]
fn discard_takes_the_consume_lock_before_the_account_lock() {
    let (scratch, _) = held_scratch("discard_order_consume_account");
    let root = scratch.root();
    let account = AccountLock::acquire(&root).expect("the account lock is free");
    let store = store_of(&scratch).with_lock_wait(LONG_WAIT);
    let drain = idle_drain();

    let (seen, discarded) = thread::scope(|scope| {
        let discard = scope.spawn(|| discard_held(&store, &drain, 1, HELD_ID, now()));
        let seen = seen_held(&[root.consume_lock(1)]);
        drop(account);
        (seen, discard.join().expect("the discard does not panic"))
    });

    assert!(
        seen,
        "the discard held the consume lock while it waited for the account lock"
    );
    assert!(
        discarded.is_ok(),
        "the discard went ahead once the account lock was released: {discarded:?}"
    );
    assert_eq!(scratch.stash_bytes(HELD_ID), None, "the entry is discarded");
}

#[test]
fn discard_takes_the_account_lock_before_the_manifest_lock() {
    let (scratch, _) = held_scratch("discard_order_account_manifest");
    let root = scratch.root();
    let manifest_lock = FileLock::acquire(
        &root.credentials_dir().join(".unclaimed-manifest.lock"),
        Duration::ZERO,
    )
    .expect("the manifest lock is free");
    let store = store_of(&scratch).with_lock_wait(LONG_WAIT);
    let drain = idle_drain();

    let (seen, discarded) = thread::scope(|scope| {
        let discard = scope.spawn(|| discard_held(&store, &drain, 1, HELD_ID, now()));
        let seen = seen_held(&[root.consume_lock(1), root.lock_file()]);
        drop(manifest_lock);
        (seen, discard.join().expect("the discard does not panic"))
    });

    assert!(
        seen,
        "the discard held the consume and account locks while it waited for the manifest lock"
    );
    assert!(
        discarded.is_ok(),
        "the discard went ahead once the manifest lock was released: {discarded:?}"
    );
    assert_eq!(scratch.stash_bytes(HELD_ID), None, "the entry is discarded");
}

#[test]
fn discard_succeeds_and_only_warns_when_the_usage_cache_cannot_be_cleared() {
    const NEWER_USAGE: &[u8] = br#"{"schemaVersion": 99, "accounts": {}}"#;
    let (scratch, _) = held_scratch("discard_usage_cache_newer");
    let root = scratch.root();
    fs::create_dir_all(root.cache_dir()).expect("the cache folder can be made");
    fs::write(root.usage_json(), NEWER_USAGE).expect("the usage table can be written");

    let discarded = discard_held(&store_of(&scratch), &idle_drain(), 1, HELD_ID, now());

    assert!(
        discarded.is_ok(),
        "a usage cache that cannot be cleared does not fail the discard: {discarded:?}"
    );
    assert_eq!(
        scratch.stash_bytes(HELD_ID),
        None,
        "the entry's file is deleted"
    );
    assert_eq!(
        scratch.manifest()["entries"],
        json!({}),
        "its row is removed"
    );
    assert_eq!(
        fs::read(root.usage_json()).expect("the usage table reads"),
        NEWER_USAGE,
        "the newer usage table is left as it was"
    );
}

#[cfg(windows)]
#[test]
fn discard_that_deleted_the_file_but_could_not_rewrite_the_manifest_says_so_and_the_next_pass_retires_the_row()
 {
    let (scratch, row) = held_scratch("discard_manifest_unwritable");
    let manifest = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(scratch.manifest_path())
        .expect("the manifest can be held open");

    let discarded = discard_held(&store_of(&scratch), &idle_drain(), 1, HELD_ID, now());

    drop(manifest);
    assert!(
        matches!(discarded, Err(DiscardError::ManifestWriteFailed)),
        "{discarded:?}"
    );
    assert_eq!(
        scratch.stash_bytes(HELD_ID),
        None,
        "the entry's file was deleted"
    );
    assert_eq!(
        scratch.manifest()["entries"][HELD_ID],
        row,
        "its row stays until the next pass"
    );
    let rig = Rig::new(scratch, grant());
    let outcome = rig.consume(&expired());
    assert_eq!(outcome.error, None, "the refresh is no longer held");
    assert!(
        rig.scratch.manifest()["entries"].get(HELD_ID).is_none(),
        "the next pass retired the row whose file is gone"
    );
}

#[test]
fn discard_in_a_store_that_cannot_seal_is_refused_and_deletes_nothing() {
    let (scratch, _) = held_scratch("discard_cannot_seal");
    let credentials = scratch.root().credentials_dir();
    let before = files_but_locks(&credentials);
    let store = Store::open_with_canary(scratch.root(), || false).expect("the scratch store opens");

    let discarded = discard_held(&store, &idle_drain(), 1, HELD_ID, now());

    assert!(
        matches!(
            discarded,
            Err(DiscardError::Store(StoreErrorKind::StoreReadOnly))
        ),
        "{discarded:?}"
    );
    assert_eq!(
        files_but_locks(&credentials),
        before,
        "nothing changed in a store that cannot seal"
    );
}

#[test]
fn discard_waits_for_the_consume_lock_and_answers_busy_past_the_timeout() {
    let (scratch, row) = held_scratch("discard_consume_busy");
    let consume = ConsumeLock::acquire(&scratch.root(), 1).expect("the consume lock is free");

    let discarded = discard_held(
        &store_of(&scratch).with_lock_wait(SHORT_WAIT),
        &idle_drain(),
        1,
        HELD_ID,
        now(),
    );

    drop(consume);
    assert!(
        matches!(discarded, Err(DiscardError::Locks(LockError::Timeout))),
        "{discarded:?}"
    );
    assert!(
        scratch.stash_bytes(HELD_ID).is_some(),
        "the held entry stays"
    );
    assert_eq!(scratch.manifest()["entries"][HELD_ID], row);
}

/// A stop drain that has not begun.
fn idle_drain() -> Arc<Drain> {
    Arc::new(Drain::new())
}

#[test]
fn discard_while_draining_deletes_nothing() {
    let (scratch, row) = held_scratch("discard_draining");
    let drain = idle_drain();
    assert_eq!(drain.start(), 0, "no guard is held before the discard");

    let discarded = discard_held(&store_of(&scratch), &drain, 1, HELD_ID, now());

    assert!(
        matches!(discarded, Err(DiscardError::Draining)),
        "{discarded:?}"
    );
    assert!(
        scratch.stash_bytes(HELD_ID).is_some(),
        "the held entry stays"
    );
    assert_eq!(scratch.manifest()["entries"][HELD_ID], row, "its row stays");
}

#[test]
fn discard_with_the_manifest_lock_busy_deletes_nothing_and_says_busy() {
    let (scratch, row) = held_scratch("discard_manifest_busy");
    let lock = scratch
        .root()
        .credentials_dir()
        .join(".unclaimed-manifest.lock");
    let manifest_lock =
        FileLock::acquire(&lock, Duration::ZERO).expect("the manifest lock is free");

    let discarded = discard_held(
        &store_of(&scratch).with_lock_wait(SHORT_WAIT),
        &idle_drain(),
        1,
        HELD_ID,
        now(),
    );

    drop(manifest_lock);
    assert!(
        matches!(discarded, Err(DiscardError::Locks(LockError::Timeout))),
        "{discarded:?}"
    );
    assert!(
        scratch.stash_bytes(HELD_ID).is_some(),
        "the held entry stays"
    );
    assert_eq!(scratch.manifest()["entries"][HELD_ID], row, "its row stays");
}

#[test]
fn discard_with_a_corrupt_manifest_is_refused_and_deletes_nothing() {
    let (scratch, _) = held_scratch("discard_corrupt_manifest");
    fs::write(scratch.manifest_path(), b"{ not a manifest").expect("the manifest can be written");
    let credentials = scratch.root().credentials_dir();
    let before = files_but_locks(&credentials);

    let discarded = discard_held(&store_of(&scratch), &idle_drain(), 1, HELD_ID, now());

    assert!(
        matches!(discarded, Err(DiscardError::StashNotRead)),
        "{discarded:?}"
    );
    assert_eq!(
        files_but_locks(&credentials),
        before,
        "the entry and the corrupt manifest stay as they were"
    );
}
