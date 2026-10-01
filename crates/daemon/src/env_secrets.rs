//! The store for secret environment-row values.
//!
//! A spawn dialog row under a secret key holds a literal only until the daemon
//! seals it: [`seal`] moves the value into Windows Credential Manager and
//! returns the id of a `${secret:<id>}` reference that takes its place in every
//! stored file and every echo. [`open`] reads a value back at spawn.
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
use chrono::{DateTime, Utc};
use keyring_core::{Entry, Error as KeyringError};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
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
    if value.encode_utf16().count() > MAX_VALUE_UNITS {
        return Err(SpawnFailure {
            title: "Could not save a secret".to_owned(),
            detail: format!(
                "{key}'s value is too long to save securely; use ${{env:{key}}} instead."
            ),
            hint: None,
        });
    }
    seal_value(key, value).map_err(|err| SpawnFailure {
        title: "Could not save a secret".to_owned(),
        detail: format!("{key}'s value could not be saved: {err:#}"),
        hint: Some(format!(
            "Try again, or set {key} in your environment and use ${{env:{key}}}."
        )),
    })
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
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::sync::{Arc, Mutex, OnceLock, PoisonError};

    /// Serialises the tests that point `RUSTLING_TULIP_CONFIG_DIR` somewhere:
    /// env vars are process-global and tests run on parallel threads.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// A value no index file may ever contain.
    const VALUE_SENTINEL: &str = "sentinel-value-8c41f0d2";

    /// A config dir under the temp root that `RUSTLING_TULIP_CONFIG_DIR` points
    /// at while the guard lives. Drop restores the var's prior value (unsetting
    /// it only when it was unset) and removes the dir.
    struct ScratchConfigDir {
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
    fn mock_store() -> Arc<keyring_core::mock::Store> {
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

    fn scratch(label: &str) -> (std::sync::MutexGuard<'static, ()>, ScratchConfigDir) {
        let lock = ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        mock_store();
        (lock, ScratchConfigDir::new(label))
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
}
