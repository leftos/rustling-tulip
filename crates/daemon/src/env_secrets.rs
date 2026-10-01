//! The store for secret environment-row values.
//!
//! A spawn dialog row under a secret key holds a literal only until the daemon
//! seals it: [`seal_rows`] moves a spawn's secret values into Windows
//! Credential Manager and leaves the id of a `${secret:<id>}` reference in
//! their place in every stored file and every echo. [`open`] reads a value back
//! at spawn.
//!
//! Values live in Credential Manager rather than in a file so a backup, or
//! another user of the machine, never sees them. What this module keeps in the
//! config dir is `<config dir>/env-secrets.json`: one `{id, key, created_at}`
//! line per stored value, never the value. The index is how the daemon knows
//! which ids exist, since the store is configured without its search feature.
//!
//! Credentials are filed under the service `rustling-tulip`, or
//! `rustling-tulip:<first 8 hex of the SHA-256 of the config dir>` while
//! `RUSTLING_TULIP_CONFIG_DIR` is set, so an isolated daemon (the e2e tier)
//! never shares entries with the user's own.

use crate::paths;
use crate::secret;
use crate::spawn_plan::SpawnFailure;
use crate::user_env::Secret;
use anyhow::Context as _;
use chrono::{DateTime, TimeDelta, Utc};
use keyring_core::{Entry, Error as KeyringError};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use tracing::warn;

/// The Credential Manager service entries are filed under, unless the config
/// dir is overridden.
const SERVICE: &str = "rustling-tulip";

/// Setting this to a non-empty value switches the service to its per-config-dir
/// variant.
const CONFIG_DIR_VAR: &str = "RUSTLING_TULIP_CONFIG_DIR";

/// How many hex characters of the config dir's hash the isolated service name
/// carries.
const SERVICE_HASH_LEN: usize = 8;

/// The index file's name in the config dir.
const INDEX_FILE: &str = "env-secrets.json";

/// Serialises every read-modify-write of the index.
///
/// Each client connection is served on its own task, so two spawns can seal
/// their rows at the same time, and a cleanup can delete while a spawn seals.
/// Without this lock the later write replaces the whole file from a copy read
/// before the earlier line was added: that line is gone, so the value behind it
/// can no longer be opened, and nothing can find it to delete it. The two
/// writers would also share the one temp file the write goes through.
static INDEX_LOCK: Mutex<()> = Mutex::new(());

/// The index lock, held across one read-modify-write of `<config>/env-secrets.json`.
/// A thread that panicked while holding it left the index consistent, so a
/// poisoned lock is not a reason to refuse every later spawn.
fn index_lock() -> MutexGuard<'static, ()> {
    INDEX_LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The most UTF-16 code units Windows Credential Manager takes in one generic
/// credential's blob: `CRED_MAX_CREDENTIAL_BLOB_SIZE` (2,560) bytes of UTF-16,
/// which `windows-native-keyring-store` encodes the value as.
const MAX_VALUE_UNITS: usize = 1280;

/// How long an unreferenced secret is kept before a startup cleanup may delete
/// it: long enough to cover a spawn sealed by an older daemon instance moments
/// before a restart, so a value a config still needs is never swept.
const UNREFERENCED_GRACE: TimeDelta = TimeDelta::hours(1);

/// Install Windows Credential Manager as the process's credential store.
///
/// Meant for startup, before any spawn seals a value.
///
/// # Errors
///
/// Fails when the platform store cannot be created, or off Windows, where this
/// module has no store to install.
#[cfg(windows)]
pub fn init() -> anyhow::Result<()> {
    let store = windows_native_keyring_store::Store::new()
        .context("creating the Windows credential store")?;
    keyring_core::set_default_store(store);
    Ok(())
}

/// # Errors
///
/// Always: the store is Windows Credential Manager.
#[cfg(not(windows))]
pub fn init() -> anyhow::Result<()> {
    anyhow::bail!("the secret store is Windows Credential Manager; there is no store off Windows")
}

/// Save `value` under `key` and return the id standing for it: an indexed id
/// whose key and stored value already match is reused, else a fresh id is
/// stored and indexed.
///
/// # Errors
///
/// A [`SpawnFailure`] refusing the spawn: the value is over the store's size
/// limit, or the store or the index could not be written. Neither message
/// carries the value.
pub fn seal(key: &str, value: &str) -> Result<String, SpawnFailure> {
    check_sealable_value(key, value)?;
    seal_value(key, value).map_err(|err| SpawnFailure {
        title: "Could not save a secret".to_owned(),
        detail: format!("{key}'s value could not be saved: {err:#}"),
        hint: Some(format!(
            "Try again, or set {key} in your environment and use ${{env:{key}}}."
        )),
    })
}

/// Seal every row in `extra_env` that carries a secret literal, replacing its
/// value with the `${secret:<id>}` reference standing for it.
///
/// A row is sealed when its value is a literal (never a `${env:…}` or
/// `${secret:…}` reference), non-empty, and its key either names a credential
/// ([`protocol::env_rows::is_secret_key`]) or the client listed it in
/// `secret_keys` (the dialog's per-row Secret toggle). Every other row is left
/// exactly as sent.
///
/// # Errors
///
/// The [`seal`] refusal: the value is over the store's size limit, or the
/// store or the index could not be written. The refusal names the key, never
/// the value.
pub fn seal_rows(
    extra_env: &mut [(String, String)],
    secret_keys: &[String],
) -> Result<(), SpawnFailure> {
    for (key, value) in extra_env.iter_mut() {
        if !is_secret_row(key, value, secret_keys) {
            continue;
        }
        let id = seal(key, value)?;
        *value = format!("${{secret:{id}}}");
    }
    Ok(())
}

/// Refuse a spawn whose secret rows could not be sealed, without sealing
/// anything: the selection and the refusals [`seal_rows`] applies, for a caller
/// that must decide before it does work it cannot undo — the dispatcher's
/// pre-check, which runs before the user is asked to confirm a checkout, and
/// recovery, which runs before it registers a repo.
///
/// # Errors
///
/// The [`seal`] refusal for the first row that would be sealed: the value is
/// over the store's size limit, or the store is not available. Neither message
/// carries the value.
pub fn check_sealable(
    extra_env: &[(String, String)],
    secret_keys: &[String],
) -> Result<(), SpawnFailure> {
    for (key, value) in extra_env {
        if is_secret_row(key, value, secret_keys) {
            check_sealable_value(key, value)?;
        }
    }
    Ok(())
}

/// Whether `value` is a secret literal under `key`: non-empty, not already a
/// reference, and either the key names a credential or `secret_keys` lists it.
fn is_secret_row(key: &str, value: &str, secret_keys: &[String]) -> bool {
    !value.is_empty()
        && protocol::env_rows::reference(value).is_none()
        && (protocol::env_rows::is_secret_key(key)
            || secret_keys.iter().any(|listed| listed == key))
}

/// What sealing one stored file's spawn-config rows at startup did.
pub(crate) enum SealOutcome {
    /// No row needed sealing; the file is left as it is.
    Unchanged,
    /// The rows, with each secret literal the store took replaced by the
    /// `${secret:<id>}` reference standing for it.
    Sealed(Vec<(String, String)>),
}

/// Seal every secret literal row of `rows` — one stored file's spawn config —
/// so an older file's plain-text secrets move into the store at startup.
///
/// Only the name pattern applies: a file written before the dialog's per-row
/// toggle carries no toggle, so a row is sealed when
/// [`protocol::env_rows::is_secret_key`] matches its key.
///
/// One row at a time: a value the store refuses stays in plain text with a
/// warning naming `file` and the key, never the value, and every other row is
/// still sealed.
pub(crate) fn seal_stored_rows(rows: &[(String, String)], file: &Path) -> SealOutcome {
    let mut sealed = rows.to_vec();
    let mut changed = false;
    for (key, value) in &mut sealed {
        if !is_secret_row(key, value, &[]) {
            continue;
        }
        match seal(key, value) {
            Ok(id) => {
                *value = format!("${{secret:{id}}}");
                changed = true;
            }
            Err(failure) => warn!(
                file = %file.display(),
                key = %key,
                error = %failure.detail,
                "env_secrets: could not seal a secret row; leaving it in plain text"
            ),
        }
    }
    if changed {
        SealOutcome::Sealed(sealed)
    } else {
        SealOutcome::Unchanged
    }
}

/// Add every id `rows` references through `${secret:<id>}` to `into`.
pub(crate) fn referenced_ids(rows: &[(String, String)], into: &mut HashSet<String>) {
    for (_, value) in rows {
        if let Some(protocol::env_rows::Reference::Secret(id)) =
            protocol::env_rows::reference(value)
        {
            into.insert(id);
        }
    }
}

/// The refusals a secret value must pass before it can be stored: it fits the
/// store's size limit, and the store is installed (see [`init`]). Shared by
/// [`seal`] and [`check_sealable`], so a row is refused for the same reasons
/// whether the daemon checks it early or seals it.
fn check_sealable_value(key: &str, value: &str) -> Result<(), SpawnFailure> {
    if value.encode_utf16().count() > MAX_VALUE_UNITS {
        return Err(SpawnFailure {
            title: "Could not save a secret".to_owned(),
            detail: format!(
                "{key}'s value is too long to save securely; use ${{env:{key}}} instead."
            ),
            hint: None,
        });
    }
    if keyring_core::get_default_store().is_none() {
        return Err(SpawnFailure {
            title: "Could not save a secret".to_owned(),
            detail: format!(
                "{key}'s value can't be saved right now: the credential store is not available."
            ),
            hint: Some(format!(
                "Restart the daemon, or set {key} in your environment and use ${{env:{key}}}."
            )),
        });
    }
    Ok(())
}

/// Read the value saved under `id`. `None` when the index has no such id, or
/// the store no longer holds it — a value the user deleted in Credential
/// Manager reads as missing rather than as an error.
#[must_use]
pub fn open(id: &str) -> Option<Secret> {
    let context = match context() {
        Ok(context) => context,
        Err(err) => {
            warn!(error = %err, "env_secrets: no config dir to read the index from");
            return None;
        }
    };
    let entries = read_index(&context.index).unwrap_or_else(|err| {
        warn!(error = %err, "env_secrets: unreadable index; treating it as empty");
        Vec::new()
    });
    let entry = entries.iter().find(|entry| entry.id == id)?;
    stored_value(&context.service, &entry.key, id).map(Secret::new)
}

/// Remove the value saved under `id` from the store and its line from the
/// index. An id the index doesn't know is a no-op.
///
/// # Errors
///
/// Fails when the index cannot be read or rewritten, or when the store refuses
/// to remove the value. A refusal leaves the line in place: the value is still
/// in the store, so it has to stay findable for a later cleanup to retry.
pub fn delete(id: &str) -> anyhow::Result<()> {
    let _guard = index_lock();
    let context = context()?;
    let mut entries = read_index(&context.index)?;
    let Some(position) = entries.iter().position(|entry| entry.id == id) else {
        return Ok(());
    };
    let key = entries[position].key.clone();
    // The store first: a value already gone from it counts as removed, and a
    // store that refuses keeps the line.
    remove_stored(&context.service, &key, id)?;
    entries.remove(position);
    write_index(&context.index, &entries)
}

/// Delete every indexed value `referenced` does not name and that was saved
/// more than [`UNREFERENCED_GRACE`] before `now`, from the store and the
/// index. Returns how many were deleted.
///
/// A spawn that failed after sealing, a history entry pruned since it was
/// written, and a `last_spawn_config` a later spawn replaced all leave an id
/// nothing points at; this is the startup sweep that removes them. Each
/// removal goes through [`delete`], so a store refusal keeps that id's index
/// line for the next start and an id whose credential the user already
/// cleared loses its line. Call it before the daemon serves connections —
/// `delete` takes the index lock per id, not across the whole sweep.
#[must_use]
pub fn collect_unreferenced(referenced: &HashSet<String>, now: DateTime<Utc>) -> usize {
    let context = match context() {
        Ok(context) => context,
        Err(err) => {
            warn!(error = %format!("{err:#}"), "env_secrets: no config dir to clean up from");
            return 0;
        }
    };
    let entries = match read_index(&context.index) {
        Ok(entries) => entries,
        Err(err) => {
            warn!(error = %format!("{err:#}"), "env_secrets: unreadable index; skipping cleanup");
            return 0;
        }
    };
    let mut removed = 0_usize;
    for entry in entries {
        if referenced.contains(&entry.id) || now - entry.created_at <= UNREFERENCED_GRACE {
            continue;
        }
        match delete(&entry.id) {
            Ok(()) => removed += 1,
            Err(err) => warn!(
                key = %entry.key,
                id = %entry.id,
                error = %format!("{err:#}"),
                "env_secrets: could not remove an unreferenced secret; keeping it for the next start"
            ),
        }
    }
    removed
}

/// The service name and index path every operation here needs.
struct Context {
    service: String,
    index: PathBuf,
}

fn context() -> anyhow::Result<Context> {
    let config = paths::config_dir()?;
    Ok(Context {
        service: service_for(&config),
        index: config.join(INDEX_FILE),
    })
}

/// The service entries are filed under for `config_dir`: the plain name, or its
/// per-config-dir variant while the override is set.
fn service_for(config_dir: &Path) -> String {
    if !std::env::var(CONFIG_DIR_VAR).is_ok_and(|value| !value.is_empty()) {
        return SERVICE.to_owned();
    }
    let mut hex = String::with_capacity(64);
    for byte in Sha256::digest(config_dir.to_string_lossy().as_bytes()) {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex.truncate(SERVICE_HASH_LEN);
    format!("{SERVICE}:{hex}")
}

fn seal_value(key: &str, value: &str) -> anyhow::Result<String> {
    let _guard = index_lock();
    let context = context()?;
    let mut entries = read_index(&context.index)?;
    for entry in &entries {
        if entry.key == key
            && stored_value(&context.service, key, &entry.id).as_deref() == Some(value)
        {
            return Ok(entry.id.clone());
        }
    }
    let id = uuid::Uuid::new_v4().simple().to_string();
    entries.push(IndexEntry {
        id: id.clone(),
        key: key.to_owned(),
        created_at: Utc::now(),
    });
    write_index(&context.index, &entries)?;
    if let Err(err) = store_value(&context.service, key, &id, value) {
        entries.pop();
        if let Err(rollback) = write_index(&context.index, &entries) {
            warn!(error = %rollback, "env_secrets: could not undo an index line after a failed save");
        }
        return Err(err);
    }
    Ok(id)
}

fn stored_value(service: &str, key: &str, id: &str) -> Option<String> {
    match Entry::new(service, &user(key, id)).and_then(|entry| entry.get_password()) {
        Ok(value) => Some(value),
        Err(KeyringError::NoEntry) => None,
        Err(err) => {
            warn!(key, id, error = %err, "env_secrets: could not read a saved secret");
            None
        }
    }
}

fn store_value(service: &str, key: &str, id: &str, value: &str) -> anyhow::Result<()> {
    let entry = Entry::new(service, &user(key, id))
        .with_context(|| format!("opening the credential entry for {key}"))?;
    entry
        .set_password(value)
        .with_context(|| format!("saving {key} to Windows Credential Manager"))?;
    Ok(())
}

fn remove_stored(service: &str, key: &str, id: &str) -> anyhow::Result<()> {
    let entry = Entry::new(service, &user(key, id))
        .with_context(|| format!("opening the credential entry for {key}"))?;
    match entry.delete_credential() {
        Ok(()) | Err(KeyringError::NoEntry) => Ok(()),
        Err(err) => Err(err).with_context(|| format!("deleting the saved secret for {key}")),
    }
}

/// The credential's user string, so a person reading Credential Manager can
/// tell what an entry is and which row it came from.
fn user(key: &str, id: &str) -> String {
    format!("env/{key}/{id}")
}

/// One stored value's index line: its id, the row key it was sealed under, and
/// when that happened. Never the value.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct IndexEntry {
    id: String,
    key: String,
    created_at: DateTime<Utc>,
}

impl fmt::Debug for IndexEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IndexEntry")
            .field("id", &self.id)
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

fn read_index(path: &Path) -> anyhow::Result<Vec<IndexEntry>> {
    match std::fs::read(path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
    }
}

fn write_index(path: &Path, entries: &[IndexEntry]) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec_pretty(entries).context("serializing the secret index")?;
    let tmp = path.with_extension("json.tmp");
    secret::write_private(&tmp, &bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests fail loudly on setup errors")]
pub(crate) mod test_support {
    use super::CONFIG_DIR_VAR;
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::sync::{Arc, OnceLock};

    /// Serialises the tests that point `RUSTLING_TULIP_CONFIG_DIR` somewhere:
    /// env vars are process-global and tests run on parallel threads. An async
    /// mutex, so an async test can hold it across its awaits.
    pub(crate) static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// A config dir under the temp root that `RUSTLING_TULIP_CONFIG_DIR` points
    /// at while the guard lives. Drop restores the var's prior value (unsetting
    /// it only when it was unset) and removes the dir.
    pub(crate) struct ScratchConfigDir {
        path: PathBuf,
        prior: Option<OsString>,
    }

    impl ScratchConfigDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "rt-env-secrets-{label}-{}",
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(&path).expect("create scratch config dir");
            let prior = std::env::var_os(CONFIG_DIR_VAR);
            // SAFETY: callers hold ENV_LOCK for the guard's lifetime.
            unsafe { std::env::set_var(CONFIG_DIR_VAR, &path) };
            Self { path, prior }
        }
    }

    impl Drop for ScratchConfigDir {
        fn drop(&mut self) {
            // SAFETY: as in `new` — ENV_LOCK is still held by the caller.
            unsafe {
                match &self.prior {
                    Some(value) => std::env::set_var(CONFIG_DIR_VAR, value),
                    None => std::env::remove_var(CONFIG_DIR_VAR),
                }
            }
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// The in-memory store, installed as the process default on first use. Ids
    /// are unique per test, so the tests can share it.
    pub(crate) fn mock_store() -> Arc<keyring_core::mock::Store> {
        static STORE: OnceLock<Arc<keyring_core::mock::Store>> = OnceLock::new();
        STORE
            .get_or_init(|| {
                let store = keyring_core::mock::Store::new().expect("the mock store builds");
                let installed: Arc<keyring_core::CredentialStore> = store.clone();
                keyring_core::set_default_store(installed);
                store
            })
            .clone()
    }

    /// The env lock, the mock store and a scratch config dir, for a
    /// synchronous test. The guard must outlive the test body.
    pub(crate) fn scratch(label: &str) -> (tokio::sync::MutexGuard<'static, ()>, ScratchConfigDir) {
        let guard = ENV_LOCK.blocking_lock();
        mock_store();
        (guard, ScratchConfigDir::new(label))
    }

    /// The same as [`scratch`], for an async test: the async lock may be held
    /// across awaits, where the std one would be `await_holding_lock`.
    pub(crate) async fn scratch_async(
        label: &str,
    ) -> (tokio::sync::MutexGuard<'static, ()>, ScratchConfigDir) {
        let guard = ENV_LOCK.lock().await;
        mock_store();
        (guard, ScratchConfigDir::new(label))
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests fail loudly on setup errors")]
mod tests {
    use super::test_support::{mock_store, scratch};
    use super::*;
    use crate::{history, orphan};
    use std::sync::Arc;

    /// A value no index file may ever contain.
    const VALUE_SENTINEL: &str = "sentinel-value-8c41f0d2";

    /// Make the next call on `key`'s credential for `id` fail with `err`, the
    /// way a credential store that is busy or locked would. The mock clears the
    /// error once it has returned it, so the following call goes through.
    fn inject_store_error(key: &str, id: &str, err: KeyringError) {
        let store = mock_store();
        let specifiers = (
            service_for(&paths::config_dir().expect("the config dir resolves")),
            user(key, id),
        );
        let mut guard = store.inner.lock().expect("the mock store is not poisoned");
        let cred = Arc::clone(
            guard
                .get_mut()
                .iter()
                .find(|cred| cred.specifiers == specifiers)
                .expect("the credential was built"),
        );
        cred.set_error(err);
    }

    fn index_text() -> String {
        let path = context().expect("the config dir resolves").index;
        std::fs::read_to_string(&path).expect("the index was written")
    }

    #[test]
    fn a_sealed_value_opens() {
        let (_lock, _dir) = scratch("open");
        let id = seal("ANTHROPIC_API_KEY", VALUE_SENTINEL).expect("seals");
        assert_eq!(id.len(), 32, "the id is a 32-character UUID: {id}");
        assert_eq!(
            open(&id).expect("the sealed value opens").expose(),
            VALUE_SENTINEL
        );
    }

    #[test]
    fn the_same_key_and_value_reuse_the_id() {
        let (_lock, _dir) = scratch("reuse");
        let first = seal("ANTHROPIC_API_KEY", VALUE_SENTINEL).expect("seals");
        let second = seal("ANTHROPIC_API_KEY", VALUE_SENTINEL).expect("re-seals");
        assert_eq!(first, second, "one entry per distinct value");
    }

    #[test]
    fn another_value_gets_a_new_id() {
        let (_lock, _dir) = scratch("distinct");
        let first = seal("ANTHROPIC_API_KEY", VALUE_SENTINEL).expect("seals");
        let second = seal("ANTHROPIC_API_KEY", "another-value").expect("seals");
        assert_ne!(first, second);
    }

    #[test]
    fn delete_removes_the_entry_and_its_index_line() {
        let (_lock, _dir) = scratch("delete");
        let id = seal("ANTHROPIC_API_KEY", VALUE_SENTINEL).expect("seals");
        delete(&id).expect("deletes");
        assert!(open(&id).is_none(), "the store no longer holds it");
        let index = index_text();
        assert!(!index.contains(&id), "{index}");
    }

    #[test]
    fn an_unknown_id_opens_none() {
        let (_lock, _dir) = scratch("unknown");
        assert!(open("0123456789abcdef0123456789abcdef").is_none());
    }

    #[test]
    fn the_index_file_bytes_never_contain_the_value() {
        let (_lock, _dir) = scratch("index");
        let id = seal("ANTHROPIC_API_KEY", VALUE_SENTINEL).expect("seals");
        let index = index_text();
        assert!(!index.contains(VALUE_SENTINEL), "{index}");
        assert!(index.contains(&id), "{index}");
        assert!(index.contains("ANTHROPIC_API_KEY"), "{index}");
    }

    #[test]
    fn an_over_long_value_is_refused_and_names_the_key() {
        let (_lock, _dir) = scratch("too-long");
        // 2,560 bytes of UTF-16 is the store's blob limit: 1,280 code units.
        let just_fits = "a".repeat(MAX_VALUE_UNITS);
        assert!(seal("RT_TEST_LONG_KEY", &just_fits).is_ok());

        let too_long = "a".repeat(MAX_VALUE_UNITS + 1);
        let refusal = seal("ANTHROPIC_API_KEY", &too_long).expect_err("refuses the value");
        assert!(refusal.detail.contains("ANTHROPIC_API_KEY"), "{refusal:?}");
        assert!(refusal.detail.contains("too long"), "{refusal:?}");
        assert!(
            !refusal.detail.contains(&too_long),
            "the refusal never carries the value: {refusal:?}"
        );
        let index = index_text();
        assert!(!index.contains("ANTHROPIC_API_KEY"), "{index}");
    }

    /// The pre-check selects and refuses exactly what sealing does, and writes
    /// nothing: no index line, no store entry.
    #[test]
    fn check_sealable_selects_and_refuses_without_writing_anything() {
        let (_lock, _dir) = scratch("check-sealable");
        let index = context().expect("the config dir resolves").index;
        let stored = mock_store()
            .inner
            .lock()
            .expect("the mock store is not poisoned")
            .borrow()
            .len();

        let rows = vec![("ANTHROPIC_API_KEY".to_owned(), VALUE_SENTINEL.to_owned())];
        check_sealable(&rows, &[]).expect("a sealable row passes");
        // The same selection `seal_rows` applies: a plain row, a reference and
        // an empty value are none of its business.
        let untouched = vec![
            ("RUST_LOG".to_owned(), "debug".to_owned()),
            ("K".to_owned(), "${env:HOME}".to_owned()),
            ("EMPTY".to_owned(), String::new()),
        ];
        check_sealable(&untouched, &[]).expect("nothing to seal");
        assert!(!index.exists(), "the check writes no index line");
        assert_eq!(
            mock_store()
                .inner
                .lock()
                .expect("the mock store is not poisoned")
                .borrow()
                .len(),
            stored,
            "the check stores no value"
        );

        let too_long = "a".repeat(MAX_VALUE_UNITS + 1);
        let refusal = check_sealable(&[("ANTHROPIC_API_KEY".to_owned(), too_long)], &[])
            .expect_err("an over-long value refuses");
        assert!(refusal.detail.contains("ANTHROPIC_API_KEY"), "{refusal:?}");
        assert!(refusal.detail.contains("too long"), "{refusal:?}");
        assert!(refusal.hint.is_none(), "{refusal:?}");
        assert!(!index.exists(), "a refusal writes nothing either");

        // A key the dialog toggled is checked even though its name is plain.
        let toggled = vec![("GH_PAT".to_owned(), VALUE_SENTINEL.to_owned())];
        check_sealable(&toggled, &["GH_PAT".to_owned()]).expect("a toggled row passes");
        assert!(!index.exists(), "a toggled row still writes nothing");
    }

    #[test]
    fn debug_output_holds_no_value() {
        let (_lock, _dir) = scratch("debug");
        let id = seal("ANTHROPIC_API_KEY", VALUE_SENTINEL).expect("seals");
        let entries = read_index(&context().expect("resolves").index).expect("the index reads");
        let shown = format!("{entries:?}");
        assert!(!shown.contains(VALUE_SENTINEL), "{shown}");
        assert!(shown.contains(&id), "{shown}");
        assert!(shown.contains("ANTHROPIC_API_KEY"), "{shown}");
    }

    #[test]
    fn the_service_name_differs_when_the_config_dir_is_overridden() {
        let (_lock, _dir) = scratch("service");
        let overridden = service_for(&paths::config_dir().expect("resolves"));
        assert!(overridden.starts_with("rustling-tulip:"), "{overridden}");
        let hashed = overridden.trim_start_matches("rustling-tulip:");
        assert_eq!(hashed.len(), SERVICE_HASH_LEN);
        assert!(
            hashed.chars().all(|c| c.is_ascii_hexdigit()),
            "{overridden}"
        );

        // SAFETY: ENV_LOCK is held, so nothing else reads the variable.
        unsafe { std::env::remove_var(CONFIG_DIR_VAR) };
        assert_eq!(
            service_for(&paths::config_dir().expect("resolves")),
            SERVICE,
            "the production service is the plain name"
        );
    }

    /// Two client connections spawn at once, each sealing its own rows. Every
    /// id a `seal` reported has to be in the index afterwards: a line lost to
    /// another seal's write is a value nothing can open or delete.
    #[test]
    fn concurrent_seals_all_land_in_the_index() {
        const ROUNDS: usize = 10;
        const THREADS: usize = 16;
        let (_lock, _dir) = scratch("concurrent");

        let mut ids: Vec<String> = Vec::new();
        for round in 0..ROUNDS {
            let sealed: Vec<String> = std::thread::scope(|scope| {
                let handles: Vec<_> = (0..THREADS)
                    .map(|n| {
                        scope.spawn(move || {
                            seal(
                                &format!("RT_TEST_KEY_{round}_{n}"),
                                &format!("value-{round}-{n}"),
                            )
                            .expect("concurrent seals succeed")
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|handle| handle.join().expect("the sealing thread joins"))
                    .collect()
            });
            ids.extend(sealed);
        }

        let entries = read_index(&context().expect("the config dir resolves").index)
            .expect("the index reads");
        assert_eq!(
            entries.len(),
            ROUNDS * THREADS,
            "every sealed value keeps its line"
        );
        for id in &ids {
            assert!(
                entries.iter().any(|entry| &entry.id == id),
                "{id} was dropped from the index"
            );
            assert!(open(id).is_some(), "{id} does not open");
        }
    }

    /// A store that refuses the removal must leave the line, so a later cleanup
    /// can find the value again.
    #[test]
    fn a_failed_store_delete_keeps_the_index_line() {
        let (_lock, _dir) = scratch("delete-failure");
        let id = seal("ANTHROPIC_API_KEY", VALUE_SENTINEL).expect("seals");
        inject_store_error(
            "ANTHROPIC_API_KEY",
            &id,
            KeyringError::Invalid("injected".to_owned(), "the store refused".to_owned()),
        );

        let refusal = delete(&id).expect_err("the store refused the removal");
        assert!(
            format!("{refusal:#}").contains("ANTHROPIC_API_KEY"),
            "{refusal:#}"
        );
        assert!(index_text().contains(&id), "the line survives for a retry");
        assert!(open(&id).is_some(), "the value is still in the store");

        // The mock clears its injected error once it has returned it, so the
        // next cleanup finds the value and the retry goes through.
        delete(&id).expect("the retry deletes");
        assert!(!index_text().contains(&id));
    }

    /// A value the user already cleared in Credential Manager still counts as
    /// removed: there is nothing left to retry, so the line goes.
    #[test]
    fn delete_clears_a_line_whose_credential_is_gone() {
        let (_lock, _dir) = scratch("delete-gone");
        let id = seal("ANTHROPIC_API_KEY", VALUE_SENTINEL).expect("seals");
        let service = service_for(&paths::config_dir().expect("the config dir resolves"));
        Entry::new(&service, &user("ANTHROPIC_API_KEY", &id))
            .expect("the credential is built")
            .delete_credential()
            .expect("the store removes it");

        delete(&id).expect("an already-gone value counts as removed");
        assert!(!index_text().contains(&id));
    }

    // --- The startup passes over stored files (ES.4, ES.5) -------------------

    /// The `${secret:<id>}` reference standing for `id`.
    fn secret_ref(id: &str) -> String {
        format!("${{secret:{id}}}")
    }

    /// A spawn config whose env rows are `rows`, for a fixture file.
    fn fixture_config(rows: &[(&str, &str)]) -> protocol::SpawnConfig {
        protocol::SpawnConfig {
            target: protocol::SpawnTarget::Standalone {
                cwd: None,
                add_dirs: Vec::new(),
            },
            mode: protocol::SessionMode::Interactive,
            dangerously_skip_permissions: false,
            agent_options: protocol::AgentOptions::Claude {
                permission_mode: None,
            },
            model: None,
            extra_env: rows
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect(),
        }
    }

    /// A registered repo whose last spawn config carries `config`.
    fn repo_with_config(id: &str, config: protocol::SpawnConfig) -> protocol::RepoEntry {
        protocol::RepoEntry {
            id: id.to_owned(),
            name: id.to_owned(),
            path: format!("C:/fixture/{id}"),
            default_branch: None,
            default_use_worktree: false,
            appearance: protocol::AppearanceOverrides::default(),
            last_agent: None,
            last_spawn_config: Some(config),
        }
    }

    /// A `Dirs` rooted at `root`, with its sessions and history dirs created.
    fn fixture_dirs(root: &std::path::Path) -> paths::Dirs {
        let config = root.join("config");
        let dirs = paths::Dirs {
            config: config.clone(),
            state_file: config.join("state.json"),
            handshake_file: config.join("daemon.json"),
            lan_config_file: config.join("lan.json"),
            lan_cert_file: config.join("lan-cert.pem"),
            lan_key_file: config.join("lan-key.pem"),
            sessions_dir: config.join("sessions"),
            worktrees_dir: root.join("worktrees"),
            binaries_dir: root.join("binaries"),
        };
        std::fs::create_dir_all(&dirs.sessions_dir).expect("create the sessions dir");
        std::fs::create_dir_all(dirs.history_dir()).expect("create the history dir");
        dirs
    }

    /// A scratch root unique to `label`.
    fn fixture_root(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "rt-startup-{label}-{}",
            uuid::Uuid::new_v4().simple()
        ))
    }

    /// Write the sidecar a spawn would leave: `id`'s meta carrying `config`.
    fn write_sidecar(dirs: &paths::Dirs, id: &str, config: &protocol::SpawnConfig) {
        let config = serde_json::to_value(config).expect("serialize the fixture config");
        let meta: orphan::OrphanMeta = serde_json::from_value(serde_json::json!({
            "session_id": id,
            "pid": 4242,
            "label": id,
            "kind": "standalone",
            "mode": "interactive",
            "members": [],
            "started_at": "2026-01-01T00:00:00Z",
            "spawn_config": config,
        }))
        .expect("the sidecar fixture parses");
        orphan::write_meta(dirs, &meta).expect("write the fixture sidecar");
    }

    /// Write the history entry a stop would leave: `id`'s entry carrying
    /// `config`, ended just now so the startup prune keeps it.
    fn write_history(dirs: &paths::Dirs, id: &str, config: &protocol::SpawnConfig) {
        write_history_at(dirs, id, Utc::now(), config);
    }

    /// The same, with an explicit end time, so a test can put an entry past the
    /// retention window.
    fn write_history_at(
        dirs: &paths::Dirs,
        id: &str,
        ended_at: DateTime<Utc>,
        config: &protocol::SpawnConfig,
    ) {
        let config = serde_json::to_value(config).expect("serialize the fixture config");
        let entry: protocol::HistoryEntry = serde_json::from_value(serde_json::json!({
            "session_id": id,
            "label": id,
            "kind": "standalone",
            "mode": "interactive",
            "agent": "claude",
            "members": [],
            "ended_at": ended_at.to_rfc3339(),
            "end": { "type": "stopped_by_user" },
            "source": "record",
            "spawn_config": config,
        }))
        .expect("the history fixture parses");
        history::write_if_absent(dirs, &entry).expect("write the fixture history entry");
    }

    /// `state.json`'s repo, a sidecar and a history entry whose
    /// `ANTHROPIC_API_KEY` row holds the sentinel literal, each beside a plain
    /// `RUST_LOG=debug` row. Returns the loaded state.
    fn seed_startup_fixtures(dirs: &paths::Dirs) -> crate::state::AppState {
        let state = crate::state::AppState::load_or_default(dirs).expect("load the fixture state");
        let rows = [("ANTHROPIC_API_KEY", VALUE_SENTINEL), ("RUST_LOG", "debug")];
        state
            .mutate(|persisted| {
                persisted
                    .repos
                    .push(repo_with_config("r1", fixture_config(&rows)));
            })
            .expect("seed state.json");
        write_sidecar(dirs, "sidecar-seal", &fixture_config(&rows));
        write_history(dirs, "history-seal", &fixture_config(&rows));
        state
    }

    /// Startup's file passes in order: `state.json` and the sidecars before
    /// orphan recovery reads them, then the history pass after its prune.
    fn run_startup_secret_passes(dirs: &paths::Dirs, state: &crate::state::AppState) {
        crate::seal_stored_secrets(dirs, state);
        crate::prune_and_seal_history(dirs, Utc::now());
    }

    /// The three fixture files a startup pass visits.
    fn startup_files(dirs: &paths::Dirs) -> [std::path::PathBuf; 3] {
        [
            dirs.state_file.clone(),
            dirs.sessions_dir.join("sidecar-seal").join("meta.json"),
            dirs.history_dir().join("history-seal.json"),
        ]
    }

    /// `text` holds no copy of the sentinel and does carry a reference.
    fn assert_sealed(text: &str, what: &str) {
        assert!(!text.contains(VALUE_SENTINEL), "{what}: {text}");
        assert!(text.contains("${secret:"), "{what}: {text}");
    }

    /// The id of the first `${secret:<id>}` reference in `text`.
    fn sealed_id(text: &str) -> String {
        let rest = text
            .split("${secret:")
            .nth(1)
            .expect("a reference is present");
        rest.split('}')
            .next()
            .expect("the reference closes")
            .to_owned()
    }

    fn read_text(path: &std::path::Path) -> String {
        std::fs::read_to_string(path).expect("the file reads")
    }

    /// `path`'s modification time.
    fn modified(path: &std::path::Path) -> std::time::SystemTime {
        std::fs::metadata(path)
            .expect("the file exists")
            .modified()
            .expect("the file has a modification time")
    }

    /// How many values the index holds.
    fn index_len() -> usize {
        read_index(&context().expect("the config dir resolves").index)
            .expect("the index reads")
            .len()
    }

    /// Captures the process's `tracing` output into a buffer, so a test can
    /// assert what a warning line does and does not carry.
    #[derive(Clone, Default)]
    struct LogCapture(Arc<Mutex<Vec<u8>>>);

    impl LogCapture {
        fn text(&self) -> String {
            let bytes = self.0.lock().expect("the log buffer is not poisoned");
            String::from_utf8_lossy(&bytes).into_owned()
        }
    }

    impl std::io::Write for LogCapture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("the log buffer is not poisoned")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl tracing_subscriber::fmt::MakeWriter<'_> for LogCapture {
        type Writer = Self;

        fn make_writer(&self) -> Self::Writer {
            self.clone()
        }
    }

    /// Leaves the credential store unset for the guard's lifetime and restores
    /// the mock store on drop, so a panicking assertion cannot strand every
    /// other test without one.
    struct StoreUnset;

    impl StoreUnset {
        fn unset() -> Self {
            keyring_core::unset_default_store();
            Self
        }
    }

    impl Drop for StoreUnset {
        fn drop(&mut self) {
            let store: Arc<keyring_core::CredentialStore> = mock_store();
            keyring_core::set_default_store(store);
        }
    }

    #[test]
    fn the_startup_pass_seals_state_sidecar_and_history_files() {
        let (_lock, _config_dir) = scratch("startup-seal");
        let dirs = fixture_dirs(&fixture_root("seal"));
        let state = seed_startup_fixtures(&dirs);

        run_startup_secret_passes(&dirs, &state);

        let state_text = read_text(&dirs.state_file);
        assert_sealed(&state_text, "state.json");
        assert!(
            state_text.contains("RUST_LOG"),
            "the plain row stays: {state_text}"
        );
        assert!(
            state_text.contains("debug"),
            "the plain value stays: {state_text}"
        );
        let sidecar_text = read_text(&dirs.sessions_dir.join("sidecar-seal").join("meta.json"));
        assert_sealed(&sidecar_text, "the sidecar");
        assert!(
            sidecar_text.contains("debug"),
            "the plain row stays: {sidecar_text}"
        );
        let history_text = read_text(&dirs.history_dir().join("history-seal.json"));
        assert_sealed(&history_text, "the history entry");
        assert!(
            history_text.contains("debug"),
            "the plain row stays: {history_text}"
        );

        let id = sealed_id(&state_text);
        assert_eq!(
            open(&id).expect("the sealed value opens").expose(),
            VALUE_SENTINEL,
            "the store holds the literal the file used to carry"
        );
    }

    #[test]
    fn a_second_startup_pass_rewrites_nothing() {
        let (_lock, _config_dir) = scratch("startup-again");
        let dirs = fixture_dirs(&fixture_root("again"));
        let state = seed_startup_fixtures(&dirs);
        run_startup_secret_passes(&dirs, &state);

        let files = startup_files(&dirs);
        let before: Vec<(std::time::SystemTime, Vec<u8>)> = files
            .iter()
            .map(|path| {
                let modified = std::fs::metadata(path)
                    .expect("the file exists")
                    .modified()
                    .expect("the file has a modification time");
                (modified, std::fs::read(path).expect("the file reads"))
            })
            .collect();

        run_startup_secret_passes(&dirs, &state);

        for (path, (modified, bytes)) in files.iter().zip(before) {
            let meta = std::fs::metadata(path).expect("the file exists");
            assert_eq!(
                meta.modified().expect("the file has a modification time"),
                modified,
                "{} was rewritten",
                path.display()
            );
            assert_eq!(
                std::fs::read(path).expect("the file reads"),
                bytes,
                "{} changed",
                path.display()
            );
        }
    }

    #[test]
    fn a_store_that_refuses_leaves_every_file_untouched_and_logs_no_value() {
        let (_lock, _config_dir) = scratch("startup-refused");
        let dirs = fixture_dirs(&fixture_root("refused"));
        let state = seed_startup_fixtures(&dirs);

        let files = startup_files(&dirs);
        let before: Vec<Vec<u8>> = files
            .iter()
            .map(|path| std::fs::read(path).expect("the file reads"))
            .collect();

        let capture = LogCapture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(capture.clone())
            .with_ansi(false)
            .finish();
        let _store = StoreUnset::unset();
        tracing::subscriber::with_default(subscriber, || {
            run_startup_secret_passes(&dirs, &state);
        });

        for (path, bytes) in files.iter().zip(before) {
            assert_eq!(
                std::fs::read(path).expect("the file reads"),
                bytes,
                "{} was rewritten after the store refused",
                path.display()
            );
        }
        let logs = capture.text();
        assert!(
            !logs.contains(VALUE_SENTINEL),
            "the warning line carries the value: {logs}"
        );
        assert!(
            logs.contains("ANTHROPIC_API_KEY"),
            "no warning named the key: {logs}"
        );
    }

    #[test]
    fn an_unreadable_history_file_is_skipped() {
        let (_lock, _config_dir) = scratch("startup-broken");
        let dirs = fixture_dirs(&fixture_root("broken"));
        let state = crate::state::AppState::load_or_default(&dirs).expect("load state");
        let broken = dirs.history_dir().join("broken.json");
        std::fs::write(&broken, b"{ this is not json").expect("write the broken entry");
        let before = std::fs::read(&broken).expect("the file reads");

        run_startup_secret_passes(&dirs, &state);

        assert_eq!(
            std::fs::read(&broken).expect("the file reads"),
            before,
            "the unreadable file is left as it is"
        );
    }

    #[test]
    fn an_id_a_history_entry_references_is_kept() {
        let (_lock, _config_dir) = scratch("collect-history");
        let dirs = fixture_dirs(&fixture_root("collect-history"));
        let state = crate::state::AppState::load_or_default(&dirs).expect("load state");
        let id = seal("ANTHROPIC_API_KEY", VALUE_SENTINEL).expect("seals");
        write_history(
            &dirs,
            "kept",
            &fixture_config(&[("ANTHROPIC_API_KEY", &secret_ref(&id))]),
        );

        let referenced = crate::referenced_secret_ids(&dirs, &state).expect("every file reads");
        assert!(referenced.contains(&id), "{referenced:?}");
        assert_eq!(
            collect_unreferenced(&referenced, Utc::now() + TimeDelta::hours(2)),
            0,
            "a referenced value is never swept"
        );
        assert!(open(&id).is_some(), "the value survives the sweep");
    }

    #[test]
    fn an_id_a_sidecar_or_last_spawn_config_references_is_kept() {
        let (_lock, _config_dir) = scratch("collect-files");
        let dirs = fixture_dirs(&fixture_root("collect-files"));
        let state = crate::state::AppState::load_or_default(&dirs).expect("load state");
        let sidecar_id = seal("GH_TOKEN", VALUE_SENTINEL).expect("seals");
        let state_id = seal("MY_SECRET", "another-value").expect("seals");
        write_sidecar(
            &dirs,
            "kept",
            &fixture_config(&[("GH_TOKEN", &secret_ref(&sidecar_id))]),
        );
        state
            .mutate(|persisted| {
                persisted.repos.push(repo_with_config(
                    "r1",
                    fixture_config(&[("MY_SECRET", &secret_ref(&state_id))]),
                ));
            })
            .expect("seed state.json");

        let referenced = crate::referenced_secret_ids(&dirs, &state).expect("every file reads");
        assert!(
            referenced.contains(&sidecar_id) && referenced.contains(&state_id),
            "{referenced:?}"
        );
        assert_eq!(
            collect_unreferenced(&referenced, Utc::now() + TimeDelta::hours(2)),
            0
        );
        assert!(open(&sidecar_id).is_some() && open(&state_id).is_some());
    }

    #[test]
    fn an_unreferenced_id_two_hours_old_is_deleted_from_the_store_and_index() {
        let (_lock, _dir) = scratch("collect-old");
        let id = seal("ANTHROPIC_API_KEY", VALUE_SENTINEL).expect("seals");

        let removed = collect_unreferenced(&HashSet::new(), Utc::now() + TimeDelta::hours(2));

        assert_eq!(removed, 1);
        assert!(open(&id).is_none(), "the value is gone from the store");
        assert!(!index_text().contains(&id), "and its line from the index");
    }

    #[test]
    fn an_unreferenced_id_five_minutes_old_is_kept() {
        let (_lock, _dir) = scratch("collect-young");
        let id = seal("ANTHROPIC_API_KEY", VALUE_SENTINEL).expect("seals");

        assert_eq!(
            collect_unreferenced(&HashSet::new(), Utc::now() + TimeDelta::minutes(5)),
            0,
            "the grace period covers a spawn sealed moments before a restart"
        );
        assert!(open(&id).is_some(), "the value is still in the store");
        assert!(index_text().contains(&id), "and its line in the index");
    }

    #[test]
    fn collect_unreferenced_drops_a_line_whose_credential_is_gone() {
        let (_lock, _dir) = scratch("collect-gone");
        let id = seal("ANTHROPIC_API_KEY", VALUE_SENTINEL).expect("seals");
        let service = service_for(&paths::config_dir().expect("the config dir resolves"));
        Entry::new(&service, &user("ANTHROPIC_API_KEY", &id))
            .expect("the credential is built")
            .delete_credential()
            .expect("the store removes it");

        assert_eq!(
            collect_unreferenced(&HashSet::new(), Utc::now() + TimeDelta::hours(2)),
            1,
            "a line whose credential is already gone is dropped"
        );
        assert!(!index_text().contains(&id));
    }

    #[test]
    fn collect_unreferenced_keeps_a_line_whose_removal_fails() {
        let (_lock, _dir) = scratch("collect-failure");
        let id = seal("ANTHROPIC_API_KEY", VALUE_SENTINEL).expect("seals");
        inject_store_error(
            "ANTHROPIC_API_KEY",
            &id,
            KeyringError::Invalid("injected".to_owned(), "the store refused".to_owned()),
        );
        let later = Utc::now() + TimeDelta::hours(2);

        assert_eq!(collect_unreferenced(&HashSet::new(), later), 0);
        assert!(
            index_text().contains(&id),
            "the line survives for the next start"
        );
        assert!(open(&id).is_some(), "and so does the value");

        // The mock clears its injected error once it has returned it, so the
        // next start's sweep finds the value and the removal goes through.
        assert_eq!(collect_unreferenced(&HashSet::new(), later), 1);
        assert!(!index_text().contains(&id));
    }

    /// A sweep with every stored file readable still deletes what nothing
    /// references: the guard is about an unread file, not about never acting.
    #[test]
    fn a_sweep_with_every_file_readable_still_deletes() {
        let (_lock, _config_dir) = scratch("sweep-clean");
        let dirs = fixture_dirs(&fixture_root("sweep-clean"));
        let state = crate::state::AppState::load_or_default(&dirs).expect("load state");
        let id = seal("ANTHROPIC_API_KEY", VALUE_SENTINEL).expect("seals");

        crate::sweep_unreferenced_secrets(&dirs, &state, Utc::now() + TimeDelta::hours(2));

        assert!(open(&id).is_none(), "an unreferenced id is swept");
        assert!(!index_text().contains(&id));
    }

    /// A history file the loader cannot parse may hold the only reference to
    /// an id, so the sweep cannot prove it unreferenced and deletes nothing.
    #[test]
    fn an_id_only_an_unparseable_history_file_references_is_kept() {
        let (_lock, _config_dir) = scratch("sweep-broken-history");
        let dirs = fixture_dirs(&fixture_root("sweep-broken-history"));
        let state = crate::state::AppState::load_or_default(&dirs).expect("load state");
        let id = seal("ANTHROPIC_API_KEY", VALUE_SENTINEL).expect("seals");
        // Invalid JSON (the object is never closed) that still holds the
        // reference text, so only a parse can tell what it pointed at.
        std::fs::write(
            dirs.history_dir().join("broken.json"),
            format!(
                "{{\"spawn_config\": {{\"extra_env\": [[\"K\", \"{}\"]]}}",
                secret_ref(&id)
            ),
        )
        .expect("write the broken entry");

        crate::sweep_unreferenced_secrets(&dirs, &state, Utc::now() + TimeDelta::hours(2));

        assert!(
            open(&id).is_some(),
            "an id survives a sweep that cannot read every file"
        );
    }

    /// A sidecar from a newer daemon is left in place on purpose, so the id it
    /// alone references must survive the sweep.
    #[test]
    fn an_id_only_a_future_version_sidecar_references_is_kept() {
        let (_lock, _config_dir) = scratch("sweep-future-sidecar");
        let dirs = fixture_dirs(&fixture_root("sweep-future-sidecar"));
        let state = crate::state::AppState::load_or_default(&dirs).expect("load state");
        let id = seal("ANTHROPIC_API_KEY", VALUE_SENTINEL).expect("seals");
        let dir = dirs.sessions_dir.join("future");
        std::fs::create_dir_all(&dir).expect("create the session dir");
        let config =
            serde_json::to_value(fixture_config(&[("ANTHROPIC_API_KEY", &secret_ref(&id))]))
                .expect("serialize the fixture config");
        std::fs::write(
            dir.join("meta.json"),
            serde_json::json!({
                "on_disk_version": orphan::MAX_KNOWN_SIDECAR_VERSION + 1,
                "session_id": "future",
                "pid": 1,
                "label": "future",
                "kind": "standalone",
                "mode": "interactive",
                "members": [],
                "started_at": "2026-01-01T00:00:00Z",
                "spawn_config": config,
            })
            .to_string(),
        )
        .expect("write the future sidecar");

        crate::sweep_unreferenced_secrets(&dirs, &state, Utc::now() + TimeDelta::hours(2));

        assert!(
            open(&id).is_some(),
            "an id survives a sweep that skipped a sidecar"
        );
    }

    /// A folder the walk cannot list hides whatever its files referenced, so
    /// the sweep deletes nothing. The sessions folder is replaced by a file of
    /// the same name, which cannot be listed.
    #[test]
    fn a_sweep_after_a_folder_read_error_deletes_nothing() {
        let (_lock, _config_dir) = scratch("sweep-folder");
        let dirs = fixture_dirs(&fixture_root("sweep-folder"));
        let state = crate::state::AppState::load_or_default(&dirs).expect("load state");
        let id = seal("ANTHROPIC_API_KEY", VALUE_SENTINEL).expect("seals");
        std::fs::remove_dir_all(&dirs.sessions_dir).expect("remove the sessions dir");
        std::fs::write(&dirs.sessions_dir, b"not a folder").expect("replace it with a file");

        crate::sweep_unreferenced_secrets(&dirs, &state, Utc::now() + TimeDelta::hours(2));

        assert!(
            open(&id).is_some(),
            "nothing is deleted when a folder cannot be read"
        );
    }

    /// A `state.json` that failed to parse leaves the walk with no
    /// `last_spawn_config`s to read, so the sweep deletes nothing either.
    #[test]
    fn a_sweep_after_a_corrupt_state_file_deletes_nothing() {
        let (_lock, _config_dir) = scratch("sweep-corrupt-state");
        let dirs = fixture_dirs(&fixture_root("sweep-corrupt-state"));
        std::fs::write(&dirs.state_file, b"{ this is not json").expect("write a corrupt state");
        let state = crate::state::AppState::load_or_default(&dirs).expect("load state");
        assert!(
            state.state_file_corrupt(),
            "the fixture loaded as a default"
        );
        let id = seal("ANTHROPIC_API_KEY", VALUE_SENTINEL).expect("seals");

        crate::sweep_unreferenced_secrets(&dirs, &state, Utc::now() + TimeDelta::hours(2));

        assert!(
            open(&id).is_some(),
            "nothing is deleted when state.json could not be read"
        );
    }

    /// An entry past the retention window is deleted before it is sealed, so
    /// the value it alone named never reaches the credential store.
    #[test]
    fn an_entry_past_retention_is_never_sealed() {
        let (_lock, _config_dir) = scratch("startup-pruned");
        let dirs = fixture_dirs(&fixture_root("pruned"));
        let state = crate::state::AppState::load_or_default(&dirs).expect("load state");
        write_history_at(
            &dirs,
            "expired",
            Utc::now() - TimeDelta::days(8),
            &fixture_config(&[("ANTHROPIC_API_KEY", VALUE_SENTINEL)]),
        );

        run_startup_secret_passes(&dirs, &state);

        assert!(
            !dirs.history_dir().join("expired.json").exists(),
            "the expired entry was pruned"
        );
        assert_eq!(index_len(), 0, "nothing was sealed for a pruned entry");
    }

    /// One refused row does not cost the whole file: the rows the store takes
    /// are sealed, and only the refused one stays in plain text.
    #[test]
    fn a_refused_row_in_state_json_leaves_the_other_rows_sealed() {
        let (_lock, _config_dir) = scratch("state-one-bad-row");
        let dirs = fixture_dirs(&fixture_root("one-bad-row"));
        let state = crate::state::AppState::load_or_default(&dirs).expect("load state");
        let too_long = "a".repeat(MAX_VALUE_UNITS + 1);
        state
            .mutate(|persisted| {
                persisted.repos.push(repo_with_config(
                    "r-long",
                    fixture_config(&[("RT_TEST_LONG_TOKEN", &too_long)]),
                ));
                persisted.repos.push(repo_with_config(
                    "r-good",
                    fixture_config(&[("ANTHROPIC_API_KEY", VALUE_SENTINEL)]),
                ));
            })
            .expect("seed state.json");

        crate::seal_stored_secrets(&dirs, &state);

        let text = read_text(&dirs.state_file);
        assert!(
            text.contains("${secret:"),
            "the sealable row was sealed: {text}"
        );
        assert!(
            !text.contains(VALUE_SENTINEL),
            "the sealed row's literal is gone: {text}"
        );
        assert!(
            text.contains(&too_long),
            "the refused row stays in plain text: {text}"
        );
        assert_eq!(index_len(), 1, "only the sealable row took an id");

        // A later start seals nothing more: the refused value never reaches the
        // store, and state.json is not rewritten.
        let before = modified(&dirs.state_file);
        let second = crate::state::AppState::load_or_default(&dirs).expect("reload state");
        crate::seal_stored_secrets(&dirs, &second);

        assert_eq!(index_len(), 1, "no id was added for the refused value");
        assert_eq!(
            modified(&dirs.state_file),
            before,
            "state.json was rewritten for a row that changed nothing"
        );
        assert_eq!(read_text(&dirs.state_file), text);
    }
}
