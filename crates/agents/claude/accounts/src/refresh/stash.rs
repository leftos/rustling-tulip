//! The unclaimed stash: a credential a refresh could not persist, or a switch displaced, held for a later pass.
//! `credentials/.unclaimed-<id>.cred` holds one sealed with [`SealPurpose::StashEntry`], bound to the config slot its
//! manifest row names (an account's number and the organization UUID the row records, or `unmanaged` with no
//! organization); `credentials/.unclaimed-manifest.json` maps each id to its row (`createdAt`, `reason`, `configSlot`,
//! `consumedFp`, `fingerprint`, then `organization`, the one the entry is sealed with), and every manifest rewrite runs
//! under `credentials/.unclaimed-manifest.lock`. The stash reads nothing of `accounts.json` but its `schemaVersion`,
//! without the account lock, so that a write before a grant is spent refuses a newer store ([`StashWrite`]): a writer
//! passes the organization it already read, and a reader opens an entry with the one its row records. A manifest of a
//! newer `schemaVersion` is read and never written.

use std::fs::{self, File};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

use super::{GrantError, Slot, StashReason, credential_fingerprint, fingerprint};
use crate::drain::Drain;
use crate::guard::{self, GuardErrorKind};
use crate::locks::{
    ACCOUNT_LOCK_TIMEOUT, AccountLock, AccountLockGuard, ConsumeLock, LockError, acquire_in_root,
};
use crate::paths::AccountsRoot;
use crate::seal::{self, AssociatedData, SealPurpose, SealedBytes};
use crate::secret::CredentialBlob;
use crate::store::{self, Store, StoreErrorKind, read_optional_unlinked, write_json_atomic};

const MANIFEST: &str = ".unclaimed-manifest.json";
const MANIFEST_LOCK: &str = ".unclaimed-manifest.lock";
const ENTRY_PREFIX: &str = ".unclaimed-";
const ENTRY_SUFFIX: &str = ".cred";
/// The manifest's `schemaVersion` this version writes; a manifest carrying another is read and never written.
const MANIFEST_SCHEMA_VERSION: u64 = 1;
/// How many temp names a write tries when each one it takes already exists.
const TEMP_NAME_ATTEMPTS: u32 = 16;

/// Makes entry ids unique within the process, beside the time and pid that make them unique across processes.
static NONCE_SERIAL: AtomicU64 = AtomicU64::new(0);
/// Numbers an entry write's temp files, so two writes never share one.
static TEMP_SERIAL: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
thread_local! {
    /// A test's step run between a write's or retire's early refusal check and the manifest lock, so a test can turn the
    /// manifest or the store newer where nothing else can.
    static BEFORE_LOCK: std::cell::RefCell<Option<Box<dyn Fn()>>> = const { std::cell::RefCell::new(None) };
}

/// Runs this thread's [`BEFORE_LOCK`] step, when a test set one.
#[cfg(test)]
fn run_before_lock() {
    BEFORE_LOCK.with(|hook| {
        if let Some(hook) = hook.borrow().as_ref() {
            hook();
        }
    });
}

/// Whose a stash row is (`configSlot`): a managed account's number, or `unmanaged` for a live login no managed
/// account has. Only an account's rows are ever adopted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConfigSlot {
    /// Managed account `n`.
    Account(u32),
    /// No managed account.
    Unmanaged,
}

impl ConfigSlot {
    /// The manifest's `configSlot` string: the number, or `unmanaged`.
    fn field(self) -> String {
        match self {
            Self::Account(number) => number.to_string(),
            Self::Unmanaged => "unmanaged".to_owned(),
        }
    }

    /// The associated data an entry is sealed under: its slot and, for an account, the organization UUID in
    /// `organization` (empty for `unmanaged`, which has no account).
    fn associated_data(self, organization: &str) -> AssociatedData {
        match self {
            Self::Account(number) => AssociatedData::new(number.to_string(), organization),
            Self::Unmanaged => AssociatedData::new(Self::Unmanaged.field(), ""),
        }
    }
}

/// A stash row's fields besides `createdAt`, which the write stamps.
pub(super) struct Row<'a> {
    pub reason: &'static str,
    pub config_slot: ConfigSlot,
    pub consumed_fp: Option<&'a str>,
    pub fingerprint: Option<String>,
    /// The account's organization UUID as the caller read it, empty when it has none or the roster could not be read;
    /// ignored for `unmanaged`.
    pub organization: &'a str,
}

impl Row<'_> {
    /// The organization the entry is sealed with and its row records: empty for `unmanaged`.
    fn sealed_organization(&self) -> &str {
        match self.config_slot {
            ConfigSlot::Account(_) => self.organization,
            ConfigSlot::Unmanaged => "",
        }
    }
}

/// A stash write or manifest rewrite failed: an entry or the manifest could not be written, the manifest lock stayed
/// held, or the manifest exists and cannot be read (rewriting it from an empty read would orphan every row it maps).
#[derive(Debug)]
pub(crate) struct StashFailed;

/// Why [`write_unclaimed`] left no manifest row naming its entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unrecorded {
    /// The write failed or was refused ([`StashFailed`]): no copy of the credential was kept on purpose.
    Failed,
    /// A [`StashWrite::Rescue`] beside a manifest of a newer version: its entry's bytes were kept, and no row names
    /// them. Stashing the same credential again would only add a second unnamed copy.
    Kept,
}

impl From<StashFailed> for Unrecorded {
    fn from(_: StashFailed) -> Self {
        Self::Failed
    }
}

/// When a stash write happens, which decides what refuses it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StashWrite {
    /// Before anything is spent (a switch's or the active restore's displaced live login): a newer store or a newer
    /// manifest refuses it, and nothing is written.
    BeforeSpend,
    /// A successor after its grant was spent: never refused on the store's version, since refusing it would lose the
    /// account; a newer manifest keeps the entry's bytes without a row.
    Rescue,
}

/// Why a stash write was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Refused {
    /// `accounts.json` refuses writes ([`store::version_refusal`]).
    Store,
    /// The manifest is of a newer `schemaVersion`.
    NewerManifest,
}

/// What reading the manifest established.
enum Verdict {
    /// Read, or absent: the rows are known.
    Known,
    /// The bytes exist and could not be read.
    Unreadable,
    /// The bytes were read and are not a manifest.
    Corrupt,
    /// A manifest whose `schemaVersion` is present and not [`MANIFEST_SCHEMA_VERSION`]: its rows are read, and it is
    /// never written.
    Newer,
}

/// Why a manifest rewrite did not happen: refused, the manifest lock not taken, or failed.
enum MutateFailed {
    Refused(Refused),
    Lock(LockError),
    Failed,
}

fn entry_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{ENTRY_PREFIX}{id}{ENTRY_SUFFIX}"))
}

/// The manifest's rows and what the read established.
///
/// It holds no secret (`createdAt`, `reason`, `configSlot`, `consumedFp`, `fingerprint` and `organization`; credential
/// bytes are separate entry files), so the parsed tree is dropped without scrubbing.
fn read_manifest(dir: &Path) -> (Map<String, Value>, Verdict) {
    let bytes = match read_optional_unlinked(&dir.join(MANIFEST)) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return (Map::new(), Verdict::Known),
        Err(_) => return (Map::new(), Verdict::Unreadable),
    };
    let Ok(Value::Object(mut document)) = serde_json::from_slice::<Value>(&bytes) else {
        return (Map::new(), Verdict::Corrupt);
    };
    let newer = document
        .get("schemaVersion")
        .is_some_and(|version| version.as_u64() != Some(MANIFEST_SCHEMA_VERSION));
    match (document.remove("entries"), newer) {
        (Some(Value::Object(entries)), true) => (entries, Verdict::Newer),
        (_, true) => (Map::new(), Verdict::Newer),
        (Some(Value::Object(entries)), false) => (entries, Verdict::Known),
        (Some(_) | None, false) => (Map::new(), Verdict::Corrupt),
    }
}

/// Why a stash write of `kind` into `dir`, the stash of `root`, is refused, `None` when it may go ahead: for
/// [`StashWrite::BeforeSpend`] a store that refuses writes ([`store::version_refusal`], read without the account lock,
/// which some callers hold on this thread and others do not, so taking it here could wait on this thread's own lock),
/// and for both kinds a manifest of a newer version. It runs before anything is made, written, deleted or set aside.
fn refusal(root: &AccountsRoot, dir: &Path, kind: StashWrite) -> Option<Refused> {
    let (_, verdict) = read_manifest(dir);
    refusal_given(root, &verdict, kind)
}

/// [`refusal`] on a manifest already read as `verdict`.
fn refusal_given(root: &AccountsRoot, verdict: &Verdict, kind: StashWrite) -> Option<Refused> {
    if kind == StashWrite::BeforeSpend && store::version_refusal(root).is_some() {
        return Some(Refused::Store);
    }
    matches!(verdict, Verdict::Newer).then_some(Refused::NewerManifest)
}

/// Whether any stash entry's bytes are on disk. A folder that cannot be listed answers true: it is no evidence that
/// nothing is at risk. A missing folder answers false.
fn entry_files_exist(dir: &Path) -> bool {
    let listing = match fs::read_dir(dir) {
        Ok(listing) => listing,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return false,
        Err(_) => return true,
    };
    for entry in listing {
        let Ok(entry) = entry else {
            return true;
        };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(ENTRY_PREFIX) && name.ends_with(ENTRY_SUFFIX) {
            return true;
        }
    }
    false
}

/// Rewrites the manifest with `entries` at [`MANIFEST_SCHEMA_VERSION`], first setting a manifest that does not parse
/// aside as `.unclaimed-manifest.json.corrupt-<epoch seconds>`. Only
/// [`mutate`] calls it, once its refusal checks passed.
fn write_manifest(
    dir: &Path,
    entries: &Map<String, Value>,
    now: jiff::Timestamp,
) -> io::Result<()> {
    let path = dir.join(MANIFEST);
    if let Ok(Some(bytes)) = read_optional_unlinked(&path)
        && serde_json::from_slice::<Value>(&bytes).is_err()
    {
        // A set-aside that fails is only logged; the rewrite below goes ahead either way.
        let _set_aside = fs::rename(
            &path,
            dir.join(format!("{MANIFEST}.corrupt-{}", now.as_second())),
        );
    }
    write_json_atomic(
        &path,
        &json!({ "schemaVersion": MANIFEST_SCHEMA_VERSION, "entries": entries }),
    )
}

/// Applies `mutate` to the manifest's rows of `root`'s stash as one step under the manifest lock, once the folders on
/// the way are checked ([`AccountsRoot::check_folders`]) and, under the lock, the manifest as read and the store pass
/// [`refusal`] for a write of `kind`: a manifest or store that turned newer meanwhile is caught there.
/// The manifest lock is waited for up to `wait`.
fn mutate(
    root: &AccountsRoot,
    now: jiff::Timestamp,
    kind: StashWrite,
    wait: Duration,
    mutate: impl FnOnce(&mut Map<String, Value>),
) -> Result<(), MutateFailed> {
    let dir = root.credentials_dir();
    root.check_folders(&dir.join(MANIFEST_LOCK))
        .map_err(|_| MutateFailed::Failed)?;
    make_folder(root, &dir).map_err(|_| MutateFailed::Failed)?;
    let _lock =
        acquire_in_root(root, &dir.join(MANIFEST_LOCK), wait).map_err(MutateFailed::Lock)?;
    let (mut entries, verdict) = read_manifest(&dir);
    if matches!(verdict, Verdict::Unreadable) {
        return Err(MutateFailed::Failed);
    }
    if let Some(refused) = refusal_given(root, &verdict, kind) {
        return Err(MutateFailed::Refused(refused));
    }
    mutate(&mut entries);
    write_manifest(&dir, &entries, now).map_err(|_| MutateFailed::Failed)
}

/// An entry id: `<UTC time %Y%m%dT%H%M%S>-<first 12 hex of the credential's SHA-256>-<6 hex nonce>`.
fn entry_id(blob: &CredentialBlob, now: jiff::Timestamp) -> String {
    let digest = fingerprint("", blob.expose().as_bytes());
    let serial = NONCE_SERIAL.fetch_add(1, Ordering::Relaxed);
    let wall = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    let nonce = fingerprint(
        "",
        format!("{}:{serial}:{wall}", std::process::id()).as_bytes(),
    );
    format!(
        "{}-{}-{}",
        now.strftime("%Y%m%dT%H%M%S"),
        &digest[..12],
        &nonce[..6]
    )
}

/// Makes the stash's folder, and a missing account store root, owner-only, as the store's own writes make theirs, once
/// the root and its parent are verified ([`store::check_root`]): nothing is made or written under a root that is a link
/// or open to another principal.
fn make_folder(root: &AccountsRoot, dir: &Path) -> Result<(), StashFailed> {
    store::ensure_root(root.root()).map_err(|_| StashFailed)?;
    store::check_root(root).map_err(|_| StashFailed)?;
    guard::create_folder(root.root(), dir).map_err(|_| StashFailed)
}

/// Writes `blob` as a new entry sealed for `row`'s slot and organization under the id `next_id` names, never over an
/// existing entry: an id already taken is replaced by a second one once, and a second collision fails the stash.
/// Returns the id written.
fn write_new_entry(
    root: &AccountsRoot,
    dir: &Path,
    blob: &CredentialBlob,
    row: &Row<'_>,
    mut next_id: impl FnMut() -> String,
) -> Result<String, StashFailed> {
    let ad = row.config_slot.associated_data(row.sealed_organization());
    let sealed = seal::seal(SealPurpose::StashEntry, &ad, blob).map_err(|_| StashFailed)?;
    let written = Written {
        sealed: &sealed,
        ad: &ad,
        blob,
    };
    for _attempt in 0..2 {
        let id = next_id();
        match write_sealed_entry(root, &entry_path(dir, &id), &written) {
            Ok(()) => return Ok(id),
            Err(EntryWrite::Taken) => {}
            Err(EntryWrite::Failed) => return Err(StashFailed),
        }
    }
    Err(StashFailed)
}

/// Whether a new entry's name was taken by another writer, or its write failed for another reason.
enum EntryWrite {
    Taken,
    Failed,
}

/// What one entry write puts on disk: the sealed bytes, the associated data they were sealed under, and the credential
/// they must open to.
struct Written<'w> {
    sealed: &'w SealedBytes,
    ad: &'w AssociatedData,
    blob: &'w CredentialBlob,
}

impl Written<'_> {
    /// Whether `read_back` is the sealed bytes written, and opens under their associated data to the credential.
    fn landed(&self, read_back: &SealedBytes) -> bool {
        read_back.as_bytes() == self.sealed.as_bytes()
            && seal::open(SealPurpose::StashEntry, self.ad, read_back)
                .is_ok_and(|opened| opened.expose() == self.blob.expose())
    }
}

/// Writes a new sealed entry through a guard-made temp file, reads it back (its ciphertext, and the plaintext it opens
/// to), reserves the final name with `CREATE_NEW` so an entry already there is never replaced, then renames the temp
/// onto it.
fn write_sealed_entry(
    root: &AccountsRoot,
    target: &Path,
    entry: &Written<'_>,
) -> Result<(), EntryWrite> {
    let Some((temp, mut file)) = create_temp(root, target) else {
        return Err(EntryWrite::Failed);
    };
    let written = file
        .write_all(entry.sealed.as_bytes())
        .and_then(|()| file.sync_all());
    drop(file);
    if written.is_err() {
        let _removed = fs::remove_file(&temp);
        return Err(EntryWrite::Failed);
    }
    let read_back = guard::read_file(root.root(), &temp).map_err(|_| {
        let _removed = fs::remove_file(&temp);
        EntryWrite::Failed
    })?;
    if !entry.landed(&read_back) {
        let _removed = fs::remove_file(&temp);
        return Err(EntryWrite::Failed);
    }
    match guard::create_file(root.root(), target) {
        Ok(reserved) => drop(reserved),
        Err(error) if store::is_already_exists(&error) => {
            let _removed = fs::remove_file(&temp);
            return Err(EntryWrite::Taken);
        }
        Err(_) => {
            let _removed = fs::remove_file(&temp);
            return Err(EntryWrite::Failed);
        }
    }
    if store::retry_transient(|| fs::rename(&temp, target)).is_err() {
        let _removed = fs::remove_file(&temp);
        // The empty reservation is this write's own, so it is removed with the temp file.
        let _removed = fs::remove_file(target);
        return Err(EntryWrite::Failed);
    }
    Ok(())
}

/// A new temp file beside an entry, created by the guard: one that already exists, a link included, is never opened.
fn create_temp(root: &AccountsRoot, target: &Path) -> Option<(PathBuf, File)> {
    for _attempt in 0..TEMP_NAME_ATTEMPTS {
        let temp = target.with_extension(format!(
            "{}.{}.tmp",
            std::process::id(),
            TEMP_SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        match guard::create_file(root.root(), &temp) {
            Ok(file) => return Some((temp, file)),
            Err(error) if store::is_already_exists(&error) => {}
            Err(_) => return None,
        }
    }
    None
}

/// Stashes `blob` under a new id: the entry's bytes first, then its manifest row, so a failure never leaves a row
/// without bytes. When the row cannot be written the bytes are removed again, since no reader finds an entry no row
/// names. Nothing is written when a folder on the way is a link ([`AccountsRoot::check_folders`]).
///
/// A write [`StashWrite::BeforeSpend`] that [`refusal`] refuses writes nothing (no folder, entry or temp file), and one
/// refused under the manifest lock removes its entry again. A [`StashWrite::Rescue`] beside a manifest of a newer
/// version keeps its entry's bytes, leaves the manifest untouched and logs a warning: the spent grant's successor is
/// never deleted.
///
/// # Errors
///
/// [`Unrecorded`] when no manifest row names the entry: [`Unrecorded::Failed`] when the write failed or was refused,
/// [`Unrecorded::Kept`] for a kept rescue.
pub(super) fn write_unclaimed(
    root: &AccountsRoot,
    blob: &CredentialBlob,
    row: &Row<'_>,
    now: jiff::Timestamp,
    kind: StashWrite,
) -> Result<(), Unrecorded> {
    let dir = root.credentials_dir();
    root.check_folders(&dir.join(MANIFEST))
        .map_err(|_| Unrecorded::Failed)?;
    let refused = refusal(root, &dir, kind);
    if let Some(refused) = refused
        && kind == StashWrite::BeforeSpend
    {
        tracing::warn!(
            ?refused,
            "a stash write before any grant is spent was refused; nothing was written"
        );
        return Err(Unrecorded::Failed);
    }
    #[cfg(test)]
    run_before_lock();
    make_folder(root, &dir)?;
    let id = write_new_entry(root, &dir, blob, row, || entry_id(blob, now))?;
    let entry = entry_path(&dir, &id);
    if refused.is_some() {
        keep_rescued(&entry);
        return Err(Unrecorded::Kept);
    }
    let fields = row_fields(row, now);
    match mutate(root, now, kind, ACCOUNT_LOCK_TIMEOUT, |entries| {
        entries.insert(id, Value::Object(fields));
    }) {
        Ok(()) => Ok(()),
        Err(MutateFailed::Refused(Refused::NewerManifest)) if kind == StashWrite::Rescue => {
            keep_rescued(&entry);
            Err(Unrecorded::Kept)
        }
        Err(MutateFailed::Refused(_) | MutateFailed::Lock(_) | MutateFailed::Failed) => {
            drop_unrecorded(&entry);
            Err(Unrecorded::Failed)
        }
    }
}

/// A rescued entry beside a manifest of a newer version stays on disk without a row; only its file name, which
/// carries its id, is logged, never its path or the credential's bytes.
fn keep_rescued(entry: &Path) {
    let name = entry.file_name().unwrap_or_default().to_string_lossy();
    tracing::warn!(
        entry = %name,
        "a rescued credential was kept in the stash beside a manifest of a newer version; no row names it"
    );
}

/// Removes an entry whose manifest row could not be written, since no reader finds an entry no row names.
fn drop_unrecorded(entry: &Path) {
    if let Err(error) = fs::remove_file(entry) {
        // Only the entry's file name, which carries its id, is logged: never its path or the credential's bytes.
        let name = entry.file_name().unwrap_or_default().to_string_lossy();
        tracing::warn!(entry = %name, error = %error.kind(), "a stash entry whose manifest row could not be written was left behind");
    }
}

/// A new manifest row's fields for `row`, stamped `createdAt` `now`.
fn row_fields(row: &Row<'_>, now: jiff::Timestamp) -> Map<String, Value> {
    let mut fields = Map::new();
    fields.insert(
        "createdAt".to_owned(),
        Value::String(now.strftime("%Y-%m-%dT%H:%M:%SZ").to_string()),
    );
    fields.insert("reason".to_owned(), Value::String(row.reason.to_owned()));
    fields.insert(
        "configSlot".to_owned(),
        Value::String(row.config_slot.field()),
    );
    fields.insert(
        "consumedFp".to_owned(),
        row.consumed_fp
            .map_or(Value::Null, |fp| Value::String(fp.to_owned())),
    );
    fields.insert(
        "fingerprint".to_owned(),
        row.fingerprint.clone().map_or(Value::Null, Value::String),
    );
    fields.insert(
        "organization".to_owned(),
        Value::String(row.sealed_organization().to_owned()),
    );
    fields
}

/// One entry's credential, keeping absent apart from unreadable: `Ok(None)` is a file that is not there, `Err` a file
/// that exists and could not be read through [`guard::read_file`] (a link, a cap breach, another user's, a folder) or
/// opened under `ad`, its row's slot and organization ([`ReadFailure`]). Only `Ok(None)` may retire a row or be
/// discarded.
fn read_entry(
    root: &AccountsRoot,
    id: &str,
    ad: &AssociatedData,
) -> Result<Option<CredentialBlob>, ReadFailure> {
    let path = entry_path(&root.credentials_dir(), id);
    let sealed = match guard::read_file(root.root(), &path) {
        Ok(sealed) => sealed,
        Err(error) if error.kind() == GuardErrorKind::Missing => return Ok(None),
        Err(_) => return Err(ReadFailure),
    };
    seal::open(SealPurpose::StashEntry, ad, &sealed)
        .map(Some)
        .map_err(|_| ReadFailure)
}

/// Drops every stash entry of account `number`, which is being removed, and its manifest row: once the account is gone
/// no pass may adopt one and spend it for the number. Each entry's bytes go first, as a retire removes them
/// ([`remove_entry_file`]), and then its row, under the manifest lock once [`mutate`]'s checks passed, waiting up to
/// `wait` for it. A row whose id is not of the entry form names no file and is dropped alone. A manifest with no row
/// of the account is left untouched, lock and all.
///
/// # Errors
///
/// When a folder on the way is a link, the manifest cannot be read, is corrupt or was written by a newer version, the
/// store refuses writes, the manifest lock is not taken, an entry's bytes cannot be deleted (its row is kept), or the
/// manifest cannot be rewritten. The caller removes nothing else then.
pub(crate) fn drop_account_entries(
    root: &AccountsRoot,
    number: u32,
    wait: Duration,
    now: jiff::Timestamp,
) -> io::Result<()> {
    let dir = root.credentials_dir();
    root.check_folders(&dir.join(MANIFEST))?;
    let key = ConfigSlot::Account(number).field();
    let of_account = |entries: &Map<String, Value>| -> Vec<String> {
        entries
            .iter()
            .filter(|(_, row)| field(row, "configSlot") == Some(key.as_str()))
            .map(|(id, _)| id.clone())
            .collect()
    };
    let (entries, verdict) = read_manifest(&dir);
    match verdict {
        Verdict::Known => {}
        Verdict::Unreadable | Verdict::Corrupt | Verdict::Newer => {
            return Err(io::Error::other(
                "the stash's manifest cannot be read or was written by a newer version",
            ));
        }
    }
    if of_account(&entries).is_empty() {
        return Ok(());
    }
    let mut kept = false;
    let rewritten = mutate(root, now, StashWrite::BeforeSpend, wait, |entries| {
        for id in of_account(entries) {
            if valid_id(&id) && !remove_entry_file(&entry_path(&dir, &id)) {
                kept = true;
                continue;
            }
            entries.remove(&id);
        }
    });
    match rewritten {
        Ok(()) if !kept => Ok(()),
        Ok(()) => Err(io::Error::other(
            "a stash entry of the account could not be deleted; it and its row are kept",
        )),
        Err(MutateFailed::Lock(error)) => Err(io::Error::other(error)),
        Err(MutateFailed::Refused(_)) => Err(io::Error::other(
            "the store or the stash's manifest refuses writes",
        )),
        Err(MutateFailed::Failed) => Err(io::Error::other(
            "the stash's manifest could not be read or rewritten",
        )),
    }
}

/// The bytes of an entry exist and could not be read or opened as the slot's sealed entry, or opened to a credential
/// other than the one its row's `fingerprint` names. Nothing is known about the credential the row expects, so nothing
/// may be retired or overwritten on its account.
struct ReadFailure;

/// Drops one entry, its bytes then its row, as housekeeping, both under the manifest lock once [`mutate`]'s checks
/// passed: a failure leaves a row the next pass retires.
fn retire(root: &AccountsRoot, id: &str, now: jiff::Timestamp) {
    retire_when(root, id, now, |_| true);
}

/// [`retire`], only while `still` answers true on the rows read under the manifest lock. The bytes are removed as a
/// discard removes them ([`remove_entry_file`]: a link to a folder at the entry's name is removed itself), and a row
/// whose bytes could not be removed is kept, so no bytes are left without a row. Nothing is removed behind a linked
/// folder ([`AccountsRoot::check_folders`]), nor when [`refusal`] refuses a write before a spend (a newer store or
/// manifest), before the lock or under it: the bytes and the row both stay.
fn retire_when(
    root: &AccountsRoot,
    id: &str,
    now: jiff::Timestamp,
    still: impl FnOnce(&Map<String, Value>) -> bool,
) {
    let dir = root.credentials_dir();
    let path = entry_path(&dir, id);
    if root.check_folders(&path).is_err() {
        // The row stays; a later pass retires it once the folder is a folder again.
        return;
    }
    let refused = refusal(root, &dir, StashWrite::BeforeSpend).or_else(|| {
        #[cfg(test)]
        run_before_lock();
        let retired = mutate(
            root,
            now,
            StashWrite::BeforeSpend,
            ACCOUNT_LOCK_TIMEOUT,
            |entries| {
                if still(entries) && remove_entry_file(&path) {
                    entries.remove(id);
                }
            },
        );
        // A retire that fails otherwise leaves one stale row; the next pass retries it.
        match retired {
            Err(MutateFailed::Refused(refused)) => Some(refused),
            Ok(()) | Err(MutateFailed::Lock(_) | MutateFailed::Failed) => None,
        }
    });
    if let Some(refused) = refused {
        tracing::warn!(
            ?refused,
            "a stash entry was not retired: the store or the manifest refuses writes"
        );
    }
}

/// After a capture replaced account `number`'s stored credential, whose fingerprint was `old_fp`, with one whose
/// fingerprint is `new_fp`, retires the rows that held up the old generation ([`blocking_rows`]): they are keyed to a
/// credential the account no longer stores, so no pass would ever adopt them and no discard would list them. A row with
/// no `consumedFp` is never among them, and nothing is retired while the new credential has the old one's fingerprint,
/// since its rows still hold that generation up. Each row is judged again under the manifest lock, as a discard judges
/// it, and kept when another writer changed it meanwhile.
pub(crate) fn retire_superseded(
    root: &AccountsRoot,
    number: u32,
    old_fp: &str,
    new_fp: Option<&str>,
    now: jiff::Timestamp,
) {
    if new_fp == Some(old_fp) {
        return;
    }
    let Some(entries) = known_rows(root) else {
        return;
    };
    for held in blocking_rows(root, number, old_fp, &entries) {
        retire_when(root, &held.id, now, |entries| {
            still_held(root, number, old_fp, entries, &held.id)
        });
    }
}

/// Completes an earlier pass's failed persist: when the account still stores the
/// generation a row's `consumedFp` names, that row's credential is its live successor, written back and retired.
/// Rows of the account that can never match are retired on sight: a CAS conflict's, and one whose bytes are gone.
/// Each entry opens under the organization its row records, none for a row that records none; the roster is never
/// read for it.
///
/// # Errors
///
/// [`GrantError::StashUnreadable`] when the manifest cannot be read (or is corrupt while entry bytes exist), or the
/// only matching row's bytes cannot be read or open to a credential other than its row's `fingerprint`;
/// [`GrantError::StoreUnwritable`] when the store refuses the adopted credential's write, [`GrantError::Transient`] when that write fails otherwise.
pub(crate) fn adopt(
    slot: &Slot<'_, '_>,
    lock: &AccountLockGuard,
    current: Option<&CredentialBlob>,
) -> Result<Option<CredentialBlob>, GrantError> {
    let Some(current_fp) = current.and_then(credential_fingerprint) else {
        return Ok(None);
    };
    let root = slot.context.store.root();
    let now = (slot.context.clock)();
    let entries = readable_rows(root)?;
    let config_slot = ConfigSlot::Account(slot.number);
    let key = config_slot.field();
    let mut deferred = false;
    let rows = entries
        .iter()
        .filter(|(id, row)| valid_id(id) && field(row, "configSlot") == Some(key.as_str()));
    for (id, row) in rows {
        let ad = config_slot.associated_data(field(row, "organization").unwrap_or_default());
        if field(row, "consumedFp") != Some(current_fp.as_str()) {
            retire_if_dead(root, id, row, &ad, now);
            continue;
        }
        match try_adopt_row(root, id, row, &ad, now) {
            Ok(Some(successor)) => {
                slot.write(lock, &successor)
                    .map_err(|error| super::write_failure(&error))?;
                retire(root, id, now);
                return Ok(Some(successor));
            }
            Ok(None) => {}
            Err(ReadFailure) => deferred = true,
        }
    }
    if deferred {
        Err(GrantError::StashUnreadable)
    } else {
        Ok(None)
    }
}

/// The manifest's rows, or [`GrantError::StashUnreadable`] when it cannot be read, or is corrupt while entry bytes
/// exist that its rows would have mapped, or a folder on the way is a link ([`AccountsRoot::check_folders`]).
fn readable_rows(root: &AccountsRoot) -> Result<Map<String, Value>, GrantError> {
    let dir = root.credentials_dir();
    root.check_folders(&dir.join(MANIFEST))
        .map_err(|_| GrantError::StashUnreadable)?;
    let (entries, verdict) = read_manifest(&dir);
    match verdict {
        Verdict::Unreadable => Err(GrantError::StashUnreadable),
        Verdict::Corrupt if entry_files_exist(&dir) => Err(GrantError::StashUnreadable),
        Verdict::Known | Verdict::Corrupt | Verdict::Newer => Ok(entries),
    }
}

/// A row's `key` when it is a string.
fn field<'r>(row: &'r Value, key: &str) -> Option<&'r str> {
    row.get(key).and_then(Value::as_str)
}

/// Whether `id` has the form of an entry id, `^\d{8}T\d{6}-[0-9a-f]{12}-[0-9a-f]{6}$` with ASCII digits. An id
/// becomes a file name, so a row with any other id is never read, adopted, retired or deleted.
fn valid_id(id: &str) -> bool {
    let decimal = |text: &str, len: usize| {
        text.len() == len && text.bytes().all(|byte| byte.is_ascii_digit())
    };
    let hex = |text: &str, len: usize| {
        text.len() == len
            && text
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    };
    let mut parts = id.split('-');
    let (Some(stamp), Some(digest), Some(nonce), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    let Some((date, time)) = stamp.split_once('T') else {
        return false;
    };
    decimal(date, 8) && decimal(time, 6) && hex(digest, 12) && hex(nonce, 6)
}

/// Retires a row of the account keyed to another generation when it can never match: a CAS conflict's (its successor
/// lost to the racing writer's lineage), or one whose bytes are gone. A CAS conflict's entry is deleted without being
/// opened: the row's reason alone decides it, whatever the bytes hold. Any other row whose bytes
/// exist and cannot be read is kept: it is not evidence that the credential is gone.
fn retire_if_dead(
    root: &AccountsRoot,
    id: &str,
    row: &Value,
    ad: &AssociatedData,
    now: jiff::Timestamp,
) {
    let conflict = field(row, "reason") == Some(StashReason::CasConflict.kind());
    if conflict || matches!(read_entry(root, id, ad), Ok(None)) {
        retire(root, id, now);
    }
}

/// The successor a row keyed to the stored generation holds ([`judge_row`]). Bytes that are gone retire the row and
/// give `None`; a held entry gives [`ReadFailure`] and is left in place.
fn try_adopt_row(
    root: &AccountsRoot,
    id: &str,
    row: &Value,
    ad: &AssociatedData,
    now: jiff::Timestamp,
) -> Result<Option<CredentialBlob>, ReadFailure> {
    match judge_row(root, id, row, ad) {
        Judgement::Gone => {
            retire(root, id, now);
            Ok(None)
        }
        Judgement::Adoptable(successor) => Ok(Some(successor)),
        Judgement::Held(HeldReason::DoesNotOpen) => Err(ReadFailure),
        Judgement::Held(HeldReason::DoesNotMatchItsRow) => {
            let slot = field(row, "configSlot").unwrap_or_default();
            tracing::warn!(slot = %slot, entry = %id, "a stash entry does not match its manifest row's fingerprint; it is kept and not adopted");
            Err(ReadFailure)
        }
    }
}

/// What a row keyed to the stored generation comes to, judged without writing anything.
enum Judgement {
    /// Its entry's bytes are not there.
    Gone,
    /// Its entry opens to the credential its row's `fingerprint` names: the account's live successor.
    Adoptable(CredentialBlob),
    /// Its entry is kept and holds up the account's refresh.
    Held(HeldReason),
}

/// Judges the entry of row `id`, opened under `ad`: bytes that exist and cannot be read or opened do not open, and
/// bytes that open to a credential whose fingerprint is not the row's `fingerprint` (an entry copied onto another id of
/// the slot, or an older one restored under the row) do not match it, since the sealed entry binds only its slot and
/// organization. The opened credential is dropped scrubbed unless it is handed back as adoptable.
fn judge_row(root: &AccountsRoot, id: &str, row: &Value, ad: &AssociatedData) -> Judgement {
    let successor = match read_entry(root, id, ad) {
        Ok(Some(successor)) => successor,
        Ok(None) => return Judgement::Gone,
        Err(ReadFailure) => return Judgement::Held(HeldReason::DoesNotOpen),
    };
    let recorded = field(row, "fingerprint");
    if recorded.is_none() || credential_fingerprint(&successor).as_deref() != recorded {
        return Judgement::Held(HeldReason::DoesNotMatchItsRow);
    }
    Judgement::Adoptable(successor)
}

/// Why a held stash entry holds up its account's refresh.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum HeldReason {
    /// The entry's bytes exist and cannot be read, or do not open as the account's sealed entry.
    DoesNotOpen,
    /// The entry opens to a credential other than the one its manifest row's `fingerprint` names.
    DoesNotMatchItsRow,
}

impl HeldReason {
    /// The reason's wire name: `does_not_open` or `does_not_match_row`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DoesNotOpen => "does_not_open",
            Self::DoesNotMatchItsRow => "does_not_match_row",
        }
    }
}

/// A stash entry that holds up its account's refresh now: its row's `consumedFp` names the account's stored
/// credential, and its entry does not open or does not match its row. It carries no credential, path, fingerprint or
/// organization.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeldEntry {
    /// The entry's id, of the stash's id form.
    pub id: String,
    /// The account whose refresh it holds up.
    pub number: u32,
    /// When the entry was stashed, `None` when its row's `createdAt` is absent or not a time.
    pub created_at: Option<jiff::Timestamp>,
    /// Why it is held.
    pub reason: HeldReason,
}

/// Why [`discard_held`] discarded nothing, or did not finish.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DiscardError {
    /// The id does not have the stash's id form.
    #[error("that is not a stash entry id")]
    NotAnEntryId,
    /// No account has the number.
    #[error("no account has that number")]
    UnknownAccount,
    /// The account's consume lock, the account lock or the stash's manifest lock was not taken; nothing was discarded.
    #[error("the account's locks were not taken: {0}")]
    Locks(#[source] LockError),
    /// The stop drain has begun. Nothing was discarded.
    #[error("the server is stopping, so nothing was discarded")]
    Draining,
    /// The account store refuses writes, or could not be read.
    #[error("the account store refuses the discard ({0})")]
    Store(StoreErrorKind),
    /// The stash's manifest cannot be read, or a folder on the way is a link.
    #[error("the stash cannot be read, so nothing was discarded")]
    StashUnreadable,
    /// The stash's manifest does not parse, or under its lock it, or a folder on the way, could not be read before
    /// anything was deleted.
    #[error("the stash could not be read; nothing was discarded")]
    StashNotRead,
    /// The stash's manifest was written by a newer version, so it is never rewritten.
    #[error(
        "the stash was written by a newer version of the account store, so nothing was discarded"
    )]
    NewerManifest,
    /// Re-judged under the locks, the entry no longer holds up the account's refresh: it was adopted or retired, the
    /// stored credential changed, or it now opens and matches its row.
    #[error("that entry no longer holds up the account's refresh, so nothing was discarded")]
    NotHeld,
    /// The entry's file could not be deleted; the file and its row are kept.
    #[error("the entry's file could not be deleted; it and its row are kept")]
    RemoveFailed,
    /// The entry's file was deleted and the manifest could not be rewritten; the next pass retires its row.
    #[error("the stash's manifest could not be rewritten; the next pass retires the entry's row")]
    ManifestWriteFailed,
}

/// The stash entries that hold up an account's refresh now, judged as a refresh judges them and in id order per
/// account, read without a lock: an advisory list, which [`discard_held`] judges again under the locks. An account
/// with an adoptable row is not held, since its next pass adopts it. A manifest that cannot be read, is corrupt or was
/// written by a newer version, or a folder on the way that is a link, lists nothing; an account whose stored credential
/// cannot be read lists nothing, and so does one the roster does not name (which [`discard_held`] refuses), or every
/// account when the roster cannot be read. Each entry is opened only to compare fingerprints, and nothing is written or
/// logged.
#[must_use]
pub fn held_entries(store: &Store) -> Vec<HeldEntry> {
    let root = store.root();
    let Ok(Some(sequence)) = store.read_sequence() else {
        return Vec::new();
    };
    let Some(entries) = known_rows(root) else {
        return Vec::new();
    };
    let numbers: std::collections::BTreeSet<u32> = entries
        .values()
        .filter_map(|row| field(row, "configSlot").and_then(|slot| slot.parse().ok()))
        .filter(|number| sequence.account(*number).is_some())
        .collect();
    numbers
        .into_iter()
        .flat_map(|number| {
            let current = store.read_credentials(number).ok().flatten();
            current
                .as_ref()
                .and_then(credential_fingerprint)
                .map(|current_fp| blocking_rows(root, number, &current_fp, &entries))
                .unwrap_or_default()
        })
        .collect()
}

/// The manifest's rows when it reads as known, through no linked folder; `None` otherwise.
fn known_rows(root: &AccountsRoot) -> Option<Map<String, Value>> {
    let dir = root.credentials_dir();
    root.check_folders(&dir.join(MANIFEST)).ok()?;
    let (entries, verdict) = read_manifest(&dir);
    matches!(verdict, Verdict::Known).then_some(entries)
}

/// Account `number`'s rows keyed to `current_fp` whose entries are held, as [`adopt`] would meet them; none when one of
/// its rows is adoptable. Rows whose bytes are gone are skipped, never retired here.
fn blocking_rows(
    root: &AccountsRoot,
    number: u32,
    current_fp: &str,
    entries: &Map<String, Value>,
) -> Vec<HeldEntry> {
    let config_slot = ConfigSlot::Account(number);
    let key = config_slot.field();
    let mut held = Vec::new();
    let rows = entries.iter().filter(|(id, row)| {
        valid_id(id)
            && field(row, "configSlot") == Some(key.as_str())
            && field(row, "consumedFp") == Some(current_fp)
    });
    for (id, row) in rows {
        let ad = config_slot.associated_data(field(row, "organization").unwrap_or_default());
        match judge_row(root, id, row, &ad) {
            Judgement::Gone => {}
            Judgement::Adoptable(_) => return Vec::new(),
            Judgement::Held(reason) => held.push(HeldEntry {
                id: id.clone(),
                number,
                created_at: field(row, "createdAt").and_then(|text| text.parse().ok()),
                reason,
            }),
        }
    }
    held
}

/// Discards held entry `id` of account `number`: under the account's consume lock, then the account lock, it takes a
/// guard on `drain` (only now, past those lock waits, so a bounded drain never waits one out), judges the entry again,
/// and once more under the manifest lock once its checks passed, and only while it still holds up the account's refresh
/// deletes the entry's file and then its row; a file that cannot be deleted keeps its row. Each of the three locks is
/// waited for up to the store's lock wait, [`ACCOUNT_LOCK_TIMEOUT`] unless a test set another. The guard drops once
/// the file and row are gone; once the locks are released it clears the account's `stash-unreadable` failure and
/// backoff in the usage table, so its next pass refreshes it. An entry that opens and matches its row is never deleted, and nothing is
/// deleted while draining, while the store or the manifest refuses writes or cannot be trusted, or through a linked
/// folder. Nothing else is touched: not the stored credential, the account's other rows or other accounts'.
///
/// # Errors
///
/// [`DiscardError`]: before the locks, an id not of the stash's form or an unknown account; then a lock not taken
/// (the consume, account or manifest lock), a drain that has begun, a store that refuses writes, an unreadable, corrupt
/// or newer manifest, an entry that is no longer held, and a file removal or manifest rewrite that failed.
pub fn discard_held(
    store: &Store,
    drain: &Arc<Drain>,
    number: u32,
    id: &str,
    now: jiff::Timestamp,
) -> Result<(), DiscardError> {
    if !valid_id(id) {
        return Err(DiscardError::NotAnEntryId);
    }
    match store.read_sequence() {
        Ok(Some(sequence)) if sequence.account(number).is_some() => {}
        Ok(_) => return Err(DiscardError::UnknownAccount),
        Err(error) => return Err(DiscardError::Store(error.kind())),
    }
    let root = store.root();
    let consume = ConsumeLock::acquire_within(root, number, store.lock_wait())
        .map_err(DiscardError::Locks)?;
    let account =
        AccountLock::acquire_within(root, store.lock_wait()).map_err(DiscardError::Locks)?;
    // Taken only now, past the consume and account lock waits, as a switch takes its own.
    let Ok(work) = drain.begin() else {
        return Err(DiscardError::Draining);
    };
    let discarded = discard_locked(store, number, id, now);
    drop(work);
    drop(account);
    drop(consume);
    discarded?;
    if let Err(error) = crate::poll::clear_stash_failure(root, number) {
        tracing::warn!(account = number, error = %error, "a discarded stash entry's account keeps its refresh backoff");
    }
    Ok(())
}

/// [`discard_held`]'s judgement and deletion, under the account's consume lock and the account lock.
fn discard_locked(
    store: &Store,
    number: u32,
    id: &str,
    now: jiff::Timestamp,
) -> Result<(), DiscardError> {
    if let Some(kind) = store.write_refusal() {
        return Err(DiscardError::Store(kind));
    }
    let root = store.root();
    let dir = root.credentials_dir();
    let path = entry_path(&dir, id);
    root.check_folders(&path)
        .map_err(|_| DiscardError::StashUnreadable)?;
    let (entries, verdict) = read_manifest(&dir);
    match verdict {
        Verdict::Unreadable => return Err(DiscardError::StashUnreadable),
        Verdict::Newer => return Err(DiscardError::NewerManifest),
        Verdict::Corrupt => return Err(DiscardError::StashNotRead),
        Verdict::Known => {}
    }
    let current = store
        .read_credentials(number)
        .map_err(|error| DiscardError::Store(error.kind()))?;
    let Some(current_fp) = current.as_ref().and_then(credential_fingerprint) else {
        return Err(DiscardError::NotHeld);
    };
    if !still_held(root, number, &current_fp, &entries, id) {
        return Err(DiscardError::NotHeld);
    }
    #[cfg(test)]
    run_before_lock();
    // What the closure did once the manifest lock was held and its checks passed; `None` when they did not.
    let mut under_lock = None;
    let rewritten = mutate(
        root,
        now,
        StashWrite::BeforeSpend,
        store.lock_wait(),
        |entries| {
            // Judged again on the rows read under the lock: another writer may have changed them since the read above.
            if !still_held(root, number, &current_fp, entries, id) {
                under_lock = Some(UnderLock::NotHeld);
                return;
            }
            let deleted = remove_entry_file(&path);
            if deleted {
                entries.remove(id);
            }
            under_lock = Some(UnderLock::Removed(deleted));
        },
    );
    match (rewritten, under_lock) {
        (Ok(()), Some(UnderLock::Removed(true))) => Ok(()),
        (_, Some(UnderLock::Removed(false))) => Err(DiscardError::RemoveFailed),
        (Err(MutateFailed::Failed), Some(UnderLock::Removed(true))) => {
            Err(DiscardError::ManifestWriteFailed)
        }
        // Nothing was deleted; the unchanged rows were written back, or failed to be.
        (Ok(()) | Err(MutateFailed::Failed), Some(UnderLock::NotHeld)) => {
            Err(DiscardError::NotHeld)
        }
        (Err(MutateFailed::Lock(error)), _) => Err(DiscardError::Locks(error)),
        // `mutate` failed before its closure ran: nothing was deleted.
        (Ok(()) | Err(MutateFailed::Failed), None) => Err(DiscardError::StashNotRead),
        (Err(MutateFailed::Refused(Refused::Store)), _) => Err(DiscardError::Store(
            store::version_refusal(root).unwrap_or(StoreErrorKind::UnknownSchema),
        )),
        (Err(MutateFailed::Refused(Refused::NewerManifest)), _) => Err(DiscardError::NewerManifest),
    }
}

/// What [`discard_locked`]'s closure did under the manifest lock.
enum UnderLock {
    /// The entry no longer holds up the refresh: nothing was deleted.
    NotHeld,
    /// The entry was judged held, and its file deleted (`true`) or not.
    Removed(bool),
}

/// Whether row `id` is among account `number`'s rows in `entries` that hold up the refresh of `current_fp`.
fn still_held(
    root: &AccountsRoot,
    number: u32,
    current_fp: &str,
    entries: &Map<String, Value>,
    id: &str,
) -> bool {
    blocking_rows(root, number, current_fp, entries)
        .iter()
        .any(|held| held.id == id)
}

/// Deletes an entry's file and answers whether it is gone; one already gone counts as deleted, and a directory junction
/// or other link to a folder at the entry's name is removed itself, never what it points at. A failure is logged by the
/// file name, which carries the id, and the error's kind, never the path or the bytes.
fn remove_entry_file(path: &Path) -> bool {
    let error = match fs::remove_file(path) {
        Ok(()) => return true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return true,
        Err(error) => match remove_folder_link(path) {
            Some(Ok(())) => return true,
            Some(Err(link_error)) => link_error,
            None => error,
        },
    };
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    tracing::warn!(entry = %name, error = %error.kind(), "a stash entry could not be deleted; it and its row are kept");
    false
}

/// Removes `path` itself when it is a reparse point on a folder (a junction or directory symlink; another kind is
/// removed only when empty): `RemoveDirectoryW` removes the link and leaves its target as it is. `None` when `path` is
/// not one.
#[cfg(windows)]
fn remove_folder_link(path: &Path) -> Option<io::Result<()>> {
    use std::os::windows::fs::MetadataExt as _;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    };

    let folder_link = FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY;
    let attributes = fs::symlink_metadata(path).ok()?.file_attributes();
    (attributes & folder_link == folder_link).then(|| fs::remove_dir(path))
}

/// Off Windows a link at the entry's name is a symlink, which [`fs::remove_file`] already removes itself.
#[cfg(not(windows))]
fn remove_folder_link(_path: &Path) -> Option<io::Result<()>> {
    None
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test that cannot set up its folder or read its files must fail loudly"
)]
mod tests {
    use std::fs;
    use std::path::Path;

    #[cfg(accounts_locks)]
    use super::{
        BEFORE_LOCK, DiscardError, Unrecorded, credential_fingerprint, discard_locked,
        read_manifest, retire,
    };
    use super::{
        ConfigSlot, ENTRY_PREFIX, ENTRY_SUFFIX, MANIFEST, Row, StashWrite, entry_path,
        write_new_entry, write_unclaimed,
    };
    use crate::guard;
    #[cfg(accounts_locks)]
    use crate::locks::AccountLock;
    use crate::paths::AccountsRoot;
    use crate::seal::{self, AssociatedData, SealPurpose};
    use crate::secret::CredentialBlob;

    const TAKEN: &str = "20260921T141320-aaaaaaaaaaaa-aaaaaa";
    const FREE: &str = "20260921T141320-aaaaaaaaaaaa-bbbbbb";
    /// An entry already on disk; its bytes are never opened, only compared.
    const EARLIER: &[u8] = b"an earlier entry's bytes";

    /// A private scratch store with an empty `credentials/` folder.
    fn scratch() -> AccountsRoot {
        let dir =
            std::env::temp_dir().join(format!("claude-accounts-stash-unit-{}", std::process::id()));
        // A folder left by an earlier run is wiped first; one that does not exist is the usual case.
        let _wiped = fs::remove_dir_all(&dir);
        #[cfg(unix)]
        {
            // Every file a test writes is owner-only, as the private folder's DACL makes it on Windows.
            // SAFETY: umask only swaps the process's file-mode creation mask; it takes no pointer and cannot fail.
            let _previous = unsafe { libc::umask(0o077) };
        }
        fs::create_dir_all(&dir).expect("the test's folder can be made");
        private(&dir);
        let root = AccountsRoot::new(dir.join("backup"));
        fs::create_dir_all(root.credentials_dir()).expect("the store's folders can be made");
        private(root.root());
        private(&root.credentials_dir());
        root
    }

    /// Makes `dir` this user's alone: on Windows a DACL with no inherited ACEs and one for this user, off Windows
    /// mode 0700.
    #[cfg(windows)]
    fn private(dir: &Path) {
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
    fn private(dir: &Path) {
        use std::os::unix::fs::PermissionsExt as _;

        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
            .expect("the test's folder can be made private");
    }

    #[cfg(not(any(unix, windows)))]
    fn private(_dir: &Path) {}

    /// The store's own `schemaVersion`, and one a newer version writes.
    const CURRENT_STORE: u64 = 1;
    const NEWER_STORE: u64 = CURRENT_STORE + 1;

    /// Writes `accounts.json` of `root` with no accounts at `version`.
    fn accounts_at(root: &AccountsRoot, version: u64) {
        fs::write(
            root.accounts_json(),
            format!(r#"{{"schemaVersion": {version}}}"#),
        )
        .expect("accounts.json can be written");
    }

    /// A row of no generation for account 1, as a switch's displaced live login has.
    fn foreign() -> Row<'static> {
        Row {
            reason: "foreign",
            config_slot: ConfigSlot::Account(1),
            consumed_fp: None,
            fingerprint: None,
            organization: "",
        }
    }

    fn successor() -> CredentialBlob {
        CredentialBlob::new("successor".to_owned())
    }

    /// Removes a scratch store's test folder; litter in the temp folder is harmless, the next run wipes it.
    fn wipe(root: &AccountsRoot) {
        let _removed = fs::remove_dir_all(root.root().parent().expect("the store has a parent"));
    }

    /// A stash while this thread holds the account lock, as every persist-path stash does, never takes that lock again:
    /// a second acquire would wait on this thread's own lock and fail when it times out.
    #[cfg(accounts_locks)]
    #[test]
    fn a_stash_under_the_account_lock_never_takes_it_again() {
        let root = scratch();
        accounts_at(&root, CURRENT_STORE);
        let lock = AccountLock::acquire(&root).expect("the account lock is free");

        let stashed = write_unclaimed(
            &root,
            &successor(),
            &foreign(),
            jiff::Timestamp::UNIX_EPOCH,
            StashWrite::BeforeSpend,
        );

        drop(lock);
        wipe(&root);
        assert!(
            stashed.is_ok(),
            "the stash writes under the account lock without taking it again"
        );
    }

    /// A stash before a spend into a newer store writes nothing: no entry, no temp file, no corrupt manifest set aside,
    /// and no credentials folder made when there was none.
    #[test]
    fn a_refused_stash_leaves_no_entry_temp_or_set_aside() {
        let root = scratch();
        let dir = root.credentials_dir();
        accounts_at(&root, NEWER_STORE);
        fs::write(dir.join(MANIFEST), b"not json").expect("the corrupt manifest can be written");
        let before = tree(&dir);

        let stashed = write_unclaimed(
            &root,
            &successor(),
            &foreign(),
            jiff::Timestamp::UNIX_EPOCH,
            StashWrite::BeforeSpend,
        );

        let after = tree(&dir);
        fs::remove_dir_all(&dir).expect("the credentials folder can be removed");
        let folderless = write_unclaimed(
            &root,
            &successor(),
            &foreign(),
            jiff::Timestamp::UNIX_EPOCH,
            StashWrite::BeforeSpend,
        );
        let made = dir.exists();
        wipe(&root);
        assert!(
            stashed.is_err(),
            "a newer store refuses a stash before a spend"
        );
        assert_eq!(after, before, "the credentials folder is unchanged");
        assert!(
            folderless.is_err(),
            "a newer store refuses a stash with no credentials folder"
        );
        assert!(!made, "no credentials folder was made");
    }

    /// A manifest of a newer version is never rewritten: a stash before a spend writes nothing, and a rescue keeps its
    /// entry's bytes beside the untouched manifest.
    #[test]
    fn a_newer_manifest_is_never_rewritten() {
        const NEWER_MANIFEST: &[u8] = br#"{"schemaVersion": 2, "entries": {}}"#;
        let root = scratch();
        let dir = root.credentials_dir();
        accounts_at(&root, CURRENT_STORE);
        fs::write(dir.join(MANIFEST), NEWER_MANIFEST).expect("the newer manifest can be written");
        let before = tree(&dir);

        let refused = write_unclaimed(
            &root,
            &successor(),
            &foreign(),
            jiff::Timestamp::UNIX_EPOCH,
            StashWrite::BeforeSpend,
        );
        let after_refused = tree(&dir);
        let rescued = write_unclaimed(
            &root,
            &successor(),
            &foreign(),
            jiff::Timestamp::UNIX_EPOCH,
            StashWrite::Rescue,
        );
        let after_rescue = tree(&dir);
        let manifest = fs::read(dir.join(MANIFEST)).expect("the manifest stays");
        wipe(&root);

        assert!(
            refused.is_err(),
            "a newer manifest refuses a stash before a spend"
        );
        assert_eq!(after_refused, before, "the refused stash changed nothing");
        assert!(
            rescued.is_err(),
            "a rescue beside a newer manifest is not recorded"
        );
        assert_eq!(
            manifest, NEWER_MANIFEST,
            "the newer manifest keeps its bytes"
        );
        let added: Vec<String> = after_rescue
            .iter()
            .filter(|file| !before.contains(file))
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect();
        assert_eq!(added.len(), 1, "exactly one file was added: {added:?}");
        assert!(
            added[0].starts_with(ENTRY_PREFIX)
                && added[0].ends_with(ENTRY_SUFFIX)
                && added[0] != MANIFEST,
            "the added file is the rescued entry: {added:?}"
        );
    }

    /// A retire in a newer store deletes nothing: the entry's bytes and its manifest row stay.
    #[cfg(accounts_locks)]
    #[test]
    fn a_refused_retire_deletes_nothing() {
        let root = scratch();
        let dir = root.credentials_dir();
        accounts_at(&root, CURRENT_STORE);
        write_unclaimed(
            &root,
            &successor(),
            &foreign(),
            jiff::Timestamp::UNIX_EPOCH,
            StashWrite::BeforeSpend,
        )
        .expect("the entry is stashed");
        let (entries, _) = read_manifest(&dir);
        let id = entries
            .keys()
            .next()
            .expect("the manifest has the row")
            .clone();
        accounts_at(&root, NEWER_STORE);
        let before = tree(&dir);

        retire(&root, &id, jiff::Timestamp::UNIX_EPOCH);

        let after = tree(&dir);
        let kept = entry_path(&dir, &id).exists();
        wipe(&root);
        assert!(kept, "the entry file stays");
        assert_eq!(after, before, "the entry and the manifest are unchanged");
    }

    /// Runs `write` as this thread's step between the early refusal check and the manifest lock until the result is
    /// dropped.
    #[cfg(accounts_locks)]
    fn before_lock(write: impl Fn() + 'static) -> BeforeLock {
        BEFORE_LOCK.with(|hook| *hook.borrow_mut() = Some(Box::new(write)));
        BeforeLock
    }

    /// Clears this thread's step before the lock when dropped.
    #[cfg(accounts_locks)]
    struct BeforeLock;

    #[cfg(accounts_locks)]
    impl Drop for BeforeLock {
        fn drop(&mut self) {
            BEFORE_LOCK.with(|hook| *hook.borrow_mut() = None);
        }
    }

    /// The `.unclaimed-*.cred` entry files in `dir`.
    #[cfg(accounts_locks)]
    fn entry_files(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .expect("the folder lists")
            .map(|entry| {
                entry
                    .expect("an entry can be read")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .filter(|name| name.starts_with(ENTRY_PREFIX) && name.ends_with(ENTRY_SUFFIX))
            .collect()
    }

    /// A rescue whose manifest turns newer between the early check and the lock keeps its entry's bytes, leaves the
    /// manifest as it found it under the lock, and reports the kept rescue.
    #[cfg(accounts_locks)]
    #[test]
    fn a_rescue_write_that_meets_a_newer_manifest_under_the_lock_keeps_its_entry() {
        const NEWER_MANIFEST: &[u8] = br#"{"schemaVersion": 2, "entries": {}}"#;
        let root = scratch();
        let dir = root.credentials_dir();
        accounts_at(&root, CURRENT_STORE);
        let manifest = dir.join(MANIFEST);
        let written = manifest.clone();
        let hook = before_lock(move || {
            fs::write(&written, NEWER_MANIFEST).expect("the newer manifest can be written");
        });

        let rescued = write_unclaimed(
            &root,
            &successor(),
            &foreign(),
            jiff::Timestamp::UNIX_EPOCH,
            StashWrite::Rescue,
        );

        drop(hook);
        let entries = entry_files(&dir);
        let after = fs::read(&manifest).expect("the manifest stays");
        wipe(&root);
        assert_eq!(
            rescued,
            Err(Unrecorded::Kept),
            "the rescue is kept without a row"
        );
        assert_eq!(entries.len(), 1, "the rescued entry stays: {entries:?}");
        assert_eq!(
            after, NEWER_MANIFEST,
            "the newer manifest keeps the bytes it was given"
        );
    }

    /// A write before a spend whose store turns newer between the early check and the lock is refused under the lock,
    /// and its entry's bytes are removed again.
    #[cfg(accounts_locks)]
    #[test]
    fn a_before_spend_write_refused_under_the_lock_removes_its_entry() {
        let root = scratch();
        let dir = root.credentials_dir();
        accounts_at(&root, CURRENT_STORE);
        let accounts = root.accounts_json();
        let hook = before_lock(move || newer_store(&accounts));

        let stashed = write_unclaimed(
            &root,
            &successor(),
            &foreign(),
            jiff::Timestamp::UNIX_EPOCH,
            StashWrite::BeforeSpend,
        );

        drop(hook);
        let entries = entry_files(&dir);
        wipe(&root);
        assert_eq!(
            stashed,
            Err(Unrecorded::Failed),
            "the newer store refuses the write under the lock"
        );
        assert!(
            entries.is_empty(),
            "the refused write's entry was removed: {entries:?}"
        );
    }

    /// A retire whose store turns newer between the early check and the lock is refused under the lock: the entry's
    /// bytes and its manifest row stay.
    #[cfg(accounts_locks)]
    #[test]
    fn a_retire_refused_under_the_lock_changes_nothing() {
        let root = scratch();
        let dir = root.credentials_dir();
        accounts_at(&root, CURRENT_STORE);
        write_unclaimed(
            &root,
            &successor(),
            &foreign(),
            jiff::Timestamp::UNIX_EPOCH,
            StashWrite::BeforeSpend,
        )
        .expect("the entry is stashed");
        let (entries, _) = read_manifest(&dir);
        let id = entries
            .keys()
            .next()
            .expect("the manifest has the row")
            .clone();
        let before = tree(&dir);
        let accounts = root.accounts_json();
        let hook = before_lock(move || newer_store(&accounts));

        retire(&root, &id, jiff::Timestamp::UNIX_EPOCH);

        drop(hook);
        let after = tree(&dir);
        let kept = entry_path(&dir, &id).exists();
        wipe(&root);
        assert!(kept, "the entry file stays");
        assert_eq!(after, before, "the entry and the manifest are unchanged");
    }

    /// A discard whose entry stops holding up the refresh between its judgement and the manifest lock (its row gone, as
    /// another writer's retire would leave it) deletes nothing: the entry is judged again under the manifest lock.
    #[cfg(accounts_locks)]
    #[test]
    fn a_discard_whose_entry_stops_being_held_before_the_manifest_lock_deletes_nothing() {
        const HELD: &str = "20260921T140000-abcdefabcdef-a1b2c3";
        const ONE_ACCOUNT: &str = concat!(
            r#"{"schemaVersion": 1, "nextNumber": 2, "order": [1], "active": 1, "accounts": {"1": "#,
            r#"{"email": "one@example.test", "uuid": "00000000-0000-4000-8000-000000000011", "added": "2026-09-01T10:00:00Z"}}}"#,
        );
        let root = scratch();
        let dir = root.credentials_dir();
        fs::write(root.accounts_json(), ONE_ACCOUNT).expect("accounts.json can be written");
        let store = crate::store::Store::open(root.clone()).expect("the scratch store opens");
        let stored = CredentialBlob::new("account one's stored credential".to_owned());
        let lock = AccountLock::acquire(&root).expect("the account lock is free");
        store
            .write_credentials(&lock, 1, &stored)
            .expect("account 1's credential can be written");
        drop(lock);
        let entry = entry_path(&dir, HELD);
        fs::write(&entry, b"bytes that do not open").expect("the entry can be written");
        let row = serde_json::json!({
            "createdAt": "2026-09-21T14:00:00Z",
            "reason": "consume-gate-persist-failed",
            "configSlot": "1",
            "consumedFp": credential_fingerprint(&stored),
            "fingerprint": "sha256-full:another-credential",
            "organization": "",
        });
        let manifest = dir.join(MANIFEST);
        let held = serde_json::json!({ "schemaVersion": 1, "entries": { HELD: row } });
        fs::write(&manifest, held.to_string()).expect("the manifest can be written");
        let hook = before_lock(move || {
            fs::write(&manifest, r#"{"schemaVersion": 1, "entries": {}}"#)
                .expect("the manifest can be rewritten");
        });

        let discarded = discard_locked(&store, 1, HELD, jiff::Timestamp::UNIX_EPOCH);

        drop(hook);
        let kept = entry.exists();
        wipe(&root);
        assert!(
            matches!(discarded, Err(DiscardError::NotHeld)),
            "{discarded:?}"
        );
        assert!(
            kept,
            "an entry that no longer holds anything up is not deleted"
        );
    }

    /// Rewrites the `accounts.json` at `path` with no accounts at a newer `schemaVersion`.
    #[cfg(accounts_locks)]
    fn newer_store(path: &Path) {
        fs::write(path, format!(r#"{{"schemaVersion": {NEWER_STORE}}}"#))
            .expect("accounts.json can be rewritten");
    }

    #[test]
    fn stash_never_overwrites_an_entry() {
        let root = scratch();
        let dir = root.credentials_dir();
        fs::write(entry_path(&dir, TAKEN), EARLIER).expect("the earlier entry can be written");
        let blob = CredentialBlob::new("successor".to_owned());
        let row = Row {
            reason: "foreign",
            config_slot: ConfigSlot::Account(1),
            consumed_fp: None,
            fingerprint: None,
            organization: "",
        };

        let collided = write_new_entry(&root, &dir, &blob, &row, || TAKEN.to_owned());
        assert!(
            collided.is_err(),
            "both ids are taken: the stash fails rather than overwrite"
        );
        assert_eq!(
            fs::read(entry_path(&dir, TAKEN)).expect("the entry stays"),
            EARLIER,
            "the earlier entry is untouched"
        );

        let mut ids = [TAKEN, FREE].into_iter();
        let written = write_new_entry(&root, &dir, &blob, &row, || {
            ids.next().unwrap_or(FREE).to_owned()
        })
        .expect("the second id is free");
        assert_eq!(written, FREE, "a taken id is replaced once");
        assert_eq!(
            fs::read(entry_path(&dir, TAKEN)).expect("the entry stays"),
            EARLIER,
            "the earlier entry is untouched"
        );
        let sealed =
            guard::read_file(root.root(), &entry_path(&dir, FREE)).expect("the new entry reads");
        assert!(
            sealed.as_bytes().starts_with(b"RTSEAL"),
            "the entry is a sealed file, not base64"
        );
        let opened = seal::open(
            SealPurpose::StashEntry,
            &AssociatedData::new("1", ""),
            &sealed,
        )
        .expect("it opens under its slot");
        assert_eq!(opened.expose(), "successor");

        // Litter in the temp folder is harmless; the next run wipes it.
        let _removed = fs::remove_dir_all(root.root().parent().expect("the store has a parent"));
    }

    /// `credentials/` is a junction to a folder outside the root that holds a manifest: the stash write fails, and the
    /// folder behind the junction keeps exactly its one file and its bytes.
    #[cfg(windows)]
    #[test]
    fn a_junctioned_credentials_folder_is_never_stashed_into() {
        const PLANTED: &[u8] = br#"{"schemaVersion": 1, "entries": {}}"#;
        let dir = std::env::temp_dir().join(format!(
            "claude-accounts-stash-junction-unit-{}",
            std::process::id()
        ));
        // A folder left by an earlier run is wiped first; one that does not exist is the usual case.
        let _wiped = fs::remove_dir_all(&dir);
        let root = AccountsRoot::new(dir.join("backup"));
        let outside = dir.join("outside-credentials");
        fs::create_dir_all(root.root()).expect("the account store can be made");
        fs::create_dir_all(&outside).expect("the outside folder can be made");
        fs::write(outside.join(MANIFEST), PLANTED).expect("the planted manifest can be written");
        let made = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(root.credentials_dir())
            .arg(&outside)
            .output()
            .expect("cmd can be started");
        assert!(made.status.success(), "mklink /J failed");
        let row = Row {
            reason: "foreign",
            config_slot: ConfigSlot::Account(1),
            consumed_fp: None,
            fingerprint: None,
            organization: "",
        };

        let stashed = write_unclaimed(
            &root,
            &CredentialBlob::new("successor".to_owned()),
            &row,
            jiff::Timestamp::UNIX_EPOCH,
            StashWrite::BeforeSpend,
        );

        let names: Vec<String> = fs::read_dir(&outside)
            .expect("the outside folder lists")
            .map(|entry| {
                entry
                    .expect("an entry can be read")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        let manifest = fs::read(outside.join(MANIFEST)).expect("the planted manifest stays");
        // Removing the scratch folder removes the junction itself, never what it points at; litter is harmless.
        let _removed = fs::remove_dir_all(&dir);
        assert!(stashed.is_err(), "nothing is stashed through a junction");
        assert_eq!(names, [MANIFEST], "nothing was added behind the junction");
        assert_eq!(manifest, PLANTED, "the planted manifest keeps its bytes");
    }

    /// Every file and folder under `folder`, by its path relative to it, with each file's bytes (none for a folder).
    fn tree(folder: &Path) -> Vec<(std::path::PathBuf, Option<Vec<u8>>)> {
        let mut listing = Vec::new();
        let mut pending = vec![folder.to_path_buf()];
        while let Some(current) = pending.pop() {
            for entry in fs::read_dir(&current).expect("the folder lists") {
                let path = entry.expect("an entry can be read").path();
                let name = path
                    .strip_prefix(folder)
                    .expect("the entry is under the folder")
                    .to_path_buf();
                if path.is_dir() {
                    listing.push((name, None));
                    pending.push(path);
                } else {
                    listing.push((name, Some(fs::read(&path).expect("a file can be read"))));
                }
            }
        }
        listing.sort();
        listing
    }

    /// A stash into a root another principal can write, or into a root that is a junction to another folder, fails,
    /// and nothing under the root, or behind the junction, changes.
    #[cfg(windows)]
    #[test]
    fn a_stash_write_refuses_a_loose_or_junctioned_root() {
        let row = Row {
            reason: "foreign",
            config_slot: ConfigSlot::Account(1),
            consumed_fp: None,
            fingerprint: None,
            organization: "",
        };
        let blob = CredentialBlob::new("successor".to_owned());

        let loose = scratch();
        let granted = std::process::Command::new("icacls")
            .arg(loose.root())
            .args(["/grant", "*S-1-5-32-545:(W)"])
            .output()
            .expect("icacls can be started");
        assert!(granted.status.success(), "icacls /grant Users:(W) failed");
        let before = tree(loose.root());
        let stashed = write_unclaimed(
            &loose,
            &blob,
            &row,
            jiff::Timestamp::UNIX_EPOCH,
            StashWrite::BeforeSpend,
        );
        let after = tree(loose.root());
        // Litter in the temp folder is harmless; the next run wipes it.
        let _removed = fs::remove_dir_all(loose.root().parent().expect("the store has a parent"));
        assert!(
            stashed.is_err(),
            "nothing is stashed into a root another principal can write"
        );
        assert_eq!(after, before, "nothing under the loose root changed");

        let dir = std::env::temp_dir().join(format!(
            "claude-accounts-stash-root-junction-unit-{}",
            std::process::id()
        ));
        // A folder left by an earlier run is wiped first; one that does not exist is the usual case.
        let _wiped = fs::remove_dir_all(&dir);
        #[cfg(unix)]
        {
            // Every file a test writes is owner-only, as the private folder's DACL makes it on Windows.
            // SAFETY: umask only swaps the process's file-mode creation mask; it takes no pointer and cannot fail.
            let _previous = unsafe { libc::umask(0o077) };
        }
        fs::create_dir_all(&dir).expect("the test's folder can be made");
        private(&dir);
        let root = AccountsRoot::new(dir.join("backup"));
        let outside = dir.join("outside-root");
        fs::create_dir_all(outside.join("credentials")).expect("the outside folder can be made");
        fs::write(
            outside.join("credentials").join(MANIFEST),
            br#"{"schemaVersion": 1, "entries": {}}"#,
        )
        .expect("the planted manifest can be written");
        let made = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(root.root())
            .arg(&outside)
            .output()
            .expect("cmd can be started");
        assert!(made.status.success(), "mklink /J failed");
        let before = tree(&outside);
        let stashed = write_unclaimed(
            &root,
            &blob,
            &row,
            jiff::Timestamp::UNIX_EPOCH,
            StashWrite::BeforeSpend,
        );
        let after = tree(&outside);
        // Removing the scratch folder removes the junction itself, never what it points at; litter is harmless.
        let _removed = fs::remove_dir_all(&dir);
        assert!(
            stashed.is_err(),
            "nothing is stashed through a junctioned root"
        );
        assert_eq!(after, before, "the folder behind the junction is unchanged");
    }

    /// A stash into a root whose parent folder another principal can write fails before anything is made or written
    /// under the root: no `credentials/` folder, no entry bytes.
    #[cfg(windows)]
    #[test]
    fn a_stash_write_refuses_a_loose_root_parent() {
        let row = Row {
            reason: "foreign",
            config_slot: ConfigSlot::Account(1),
            consumed_fp: None,
            fingerprint: None,
            organization: "",
        };
        let root = scratch();
        fs::remove_dir(root.credentials_dir())
            .expect("the empty credentials folder can be removed");
        let parent = root
            .root()
            .parent()
            .expect("the store has a parent")
            .to_path_buf();
        let granted = std::process::Command::new("icacls")
            .arg(&parent)
            .args(["/grant", "*S-1-5-32-545:(W)"])
            .output()
            .expect("icacls can be started");
        assert!(granted.status.success(), "icacls /grant Users:(W) failed");
        let before = tree(root.root());

        let stashed = write_unclaimed(
            &root,
            &CredentialBlob::new("successor".to_owned()),
            &row,
            jiff::Timestamp::UNIX_EPOCH,
            StashWrite::BeforeSpend,
        );

        let after = tree(root.root());
        // Litter in the temp folder is harmless; the next run wipes it.
        let _removed = fs::remove_dir_all(&parent);
        assert!(
            stashed.is_err(),
            "nothing is stashed under a parent another principal can write"
        );
        assert_eq!(after, before, "nothing under the root was made or written");
    }
}
