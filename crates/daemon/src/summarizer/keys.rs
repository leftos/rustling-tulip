//! The summarizer's provider keys: a key saved in Windows Credential Manager,
//! one entry per provider under the config dir's service (user
//! `summarizer/<provider id>`), or an environment variable read at call time.
//! A key the provider refused is remembered in memory only, as a hash, until
//! the key changes.
//!
//! The store calls block; an async caller runs them on a blocking thread.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Mutex, MutexGuard, PoisonError};

use anyhow::Context as _;
use keyring_core::{Entry, Error as KeyringError};
use protocol::{KeyState, SummarizerKeySource};
use sha2::{Digest as _, Sha256};
use tracing::warn;

use crate::env_secrets;
use crate::user_env::{self, Secret};

/// Where a provider's key comes from, as this build reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KeySource {
    /// The key saved with [`set`].
    Stored,
    /// The named environment variable, read through [`user_env::resolve`].
    Env(String),
}

impl KeySource {
    /// The wire's source, or `None` for a source this build does not know,
    /// whose key then reads as missing rather than as the stored one.
    pub(crate) fn from_wire(source: &SummarizerKeySource) -> Option<Self> {
        match source {
            SummarizerKeySource::Stored => Some(Self::Stored),
            SummarizerKeySource::Env { variable } => Some(Self::Env(variable.clone())),
            SummarizerKeySource::Unknown => None,
        }
    }
}

/// Save `key` as `provider`'s key, replacing any saved before.
///
/// # Errors
///
/// The key is blank or over the store's size limit, the config dir cannot be
/// resolved, or the store refused the write. No message carries the key.
pub(crate) fn set(provider: &str, key: &Secret) -> anyhow::Result<()> {
    let text = key.expose();
    if text.trim().is_empty() {
        anyhow::bail!("the {provider} key is empty; paste the whole key");
    }
    let units = text.encode_utf16().count();
    if units > env_secrets::MAX_VALUE_UNITS {
        anyhow::bail!(
            "the {provider} key is {units} UTF-16 units long; Windows Credential Manager holds at most {}",
            env_secrets::MAX_VALUE_UNITS
        );
    }
    entry(provider)?
        .set_password(text)
        .with_context(|| format!("saving the {provider} key to Windows Credential Manager"))
}

/// The saved key, or `None` when there is none or the store cannot be read
/// (logged, without the key).
fn read_stored(provider: &str) -> Option<Secret> {
    let read = entry(provider).and_then(|entry| entry.get_password().map_err(anyhow::Error::from));
    match read {
        Ok(text) => Some(Secret::new(text)),
        Err(err) if matches!(err.downcast_ref(), Some(KeyringError::NoEntry)) => None,
        Err(err) => {
            warn!(provider, error = %format!("{err:#}"), "summarizer: could not read the saved key; treating it as missing");
            None
        }
    }
}

/// `provider`'s key from `source`, or `None` when there is none. A store that
/// cannot be read is logged and reads as no key.
pub(crate) fn get(provider: &str, source: &KeySource) -> Option<Secret> {
    match source {
        KeySource::Stored => read_stored(provider),
        KeySource::Env(variable) => user_env::resolve(variable).map(|(key, _)| key),
    }
}

/// Delete `provider`'s saved key. Deleting a key that is not saved succeeds.
///
/// # Errors
///
/// The config dir cannot be resolved or the store refused the delete.
pub(crate) fn delete(provider: &str) -> anyhow::Result<()> {
    match entry(provider)?.delete_credential() {
        Ok(()) | Err(KeyringError::NoEntry) => Ok(()),
        Err(err) => Err(err).with_context(|| {
            format!("deleting the {provider} key from Windows Credential Manager")
        }),
    }
}

/// The state of `provider`'s key from `source`; `None`, a source this build
/// does not know, reads as [`KeyState::Missing`].
pub(crate) fn state(
    provider: &str,
    source: Option<&KeySource>,
    rejections: &Rejections,
) -> KeyState {
    let Some(source) = source else {
        return KeyState::Missing;
    };
    match (source, get(provider, source)) {
        (_, Some(key)) if rejections.is_rejected(provider, &key) => KeyState::Rejected,
        (KeySource::Stored, Some(_)) => KeyState::Stored,
        (KeySource::Stored, None) => KeyState::Missing,
        (KeySource::Env(_), Some(_)) => KeyState::EnvFound,
        (KeySource::Env(_), None) => KeyState::EnvMissing,
    }
}

/// The keys each provider refused, as SHA-256 hashes, in memory only. A
/// provider's record holds while its current key hashes the same; a changed
/// key clears it, so a restart or a new key tries again.
#[derive(Default)]
pub(crate) struct Rejections {
    by_provider: Mutex<HashMap<String, [u8; 32]>>,
}

impl Rejections {
    /// Record that `provider` refused `key`.
    pub(crate) fn reject(&self, provider: &str, key: &Secret) {
        self.records().insert(provider.to_owned(), digest(key));
    }

    /// Whether `provider` refused `key`. A record for a different key is
    /// cleared.
    pub(crate) fn is_rejected(&self, provider: &str, key: &Secret) -> bool {
        let mut records = self.records();
        match records.get(provider) {
            None => false,
            Some(refused) if *refused == digest(key) => true,
            Some(_) => {
                records.remove(provider);
                false
            }
        }
    }

    /// The map, also after a thread panicked while holding it: each write is
    /// one insert or remove, so it is never left half-changed.
    fn records(&self) -> MutexGuard<'_, HashMap<String, [u8; 32]>> {
        self.by_provider
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl fmt::Debug for Rejections {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let records = self.records();
        let mut providers: Vec<&String> = records.keys().collect();
        providers.sort();
        f.debug_struct("Rejections")
            .field("providers", &providers)
            .finish()
    }
}

fn digest(key: &Secret) -> [u8; 32] {
    Sha256::digest(key.expose().as_bytes()).into()
}

/// The credential's user string, so a person reading Credential Manager can
/// tell what the entry is.
fn user(provider: &str) -> String {
    format!("summarizer/{provider}")
}

fn entry(provider: &str) -> anyhow::Result<Entry> {
    let service = env_secrets::service()?;
    Entry::new(&service, &user(provider))
        .with_context(|| format!("opening the credential entry for the {provider} key"))
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests fail loudly on setup errors")]
mod tests {
    use super::*;
    use crate::env_secrets::test_support::{EnvVarGuard, mock_store, scratch};
    use chrono::{TimeDelta, Utc};
    use std::collections::HashSet;
    use std::sync::Arc;

    /// A key no message, log or `Debug` output may ever contain.
    const KEY_SENTINEL: &str = "sentinel-key-5e2a9b71";
    const PROVIDER: &str = "test-provider";
    /// Set only while a test holds the env lock.
    const SET_VAR: &str = "RT_SUMMARIZER_KEYS_TEST_SET_VAR";
    /// Never set in any scope.
    const ABSENT_VAR: &str = "RT_SUMMARIZER_KEYS_TEST_ABSENT_VAR_4d1c";

    fn key(text: &str) -> Secret {
        Secret::new(text.to_owned())
    }

    fn stored(provider: &str) -> Option<String> {
        get(provider, &KeySource::Stored).map(|key| key.expose().to_owned())
    }

    /// Make the next call on `provider`'s credential fail, the way a busy or
    /// locked store would. The credential must already exist in the mock.
    fn inject_store_error(provider: &str) {
        let store = mock_store();
        let specifiers = (
            env_secrets::service().expect("the config dir resolves"),
            user(provider),
        );
        let mut guard = store.inner.lock().expect("the mock store is not poisoned");
        let cred = Arc::clone(
            guard
                .get_mut()
                .iter()
                .find(|cred| cred.specifiers == specifiers)
                .expect("the credential was built"),
        );
        cred.set_error(KeyringError::Invalid(
            "injected".to_owned(),
            "the store refused".to_owned(),
        ));
    }

    #[test]
    fn a_stored_key_reads_back() {
        let _scratch = scratch("summ-keys");
        set(PROVIDER, &key(KEY_SENTINEL)).expect("the key saves");
        assert_eq!(stored(PROVIDER).as_deref(), Some(KEY_SENTINEL));
        let rejections = Rejections::default();
        assert_eq!(
            state(PROVIDER, Some(&KeySource::Stored), &rejections),
            KeyState::Stored
        );
    }

    #[test]
    fn setting_again_replaces_the_key() {
        let _scratch = scratch("summ-keys");
        set(PROVIDER, &key("first-key")).expect("the first key saves");
        set(PROVIDER, &key("second-key")).expect("the second key saves");
        assert_eq!(stored(PROVIDER).as_deref(), Some("second-key"));
    }

    #[test]
    fn delete_removes_the_key_and_the_state_is_missing() {
        let _scratch = scratch("summ-keys");
        let rejections = Rejections::default();
        assert_eq!(
            state(PROVIDER, Some(&KeySource::Stored), &rejections),
            KeyState::Missing
        );
        set(PROVIDER, &key(KEY_SENTINEL)).expect("the key saves");
        delete(PROVIDER).expect("the key deletes");
        assert_eq!(stored(PROVIDER), None);
        assert_eq!(
            state(PROVIDER, Some(&KeySource::Stored), &rejections),
            KeyState::Missing
        );
        delete(PROVIDER).expect("deleting a key that is not saved succeeds");
    }

    #[test]
    fn keys_for_two_providers_do_not_collide() {
        let _scratch = scratch("summ-keys");
        set("provider-a", &key("key-a")).expect("a saves");
        set("provider-b", &key("key-b")).expect("b saves");
        delete("provider-a").expect("a deletes");
        assert_eq!(stored("provider-a"), None);
        assert_eq!(stored("provider-b").as_deref(), Some("key-b"));
    }

    #[test]
    fn the_entry_is_filed_under_summarizer_slash_provider_and_the_config_dir_service() {
        let _scratch = scratch("summ-keys");
        set(PROVIDER, &key(KEY_SENTINEL)).expect("the key saves");
        let service = env_secrets::service().expect("the config dir resolves");
        assert!(service.starts_with("rustling-tulip:"), "{service}");
        let entry = Entry::new(&service, "summarizer/test-provider").expect("the entry opens");
        assert_eq!(
            entry.get_password().expect("the entry holds the key"),
            KEY_SENTINEL
        );
    }

    #[test]
    fn an_env_source_reads_the_variable() {
        let _scratch = scratch("summ-keys");
        let _var = EnvVarGuard::set(SET_VAR, KEY_SENTINEL);
        let source = KeySource::Env(SET_VAR.to_owned());
        let found = get(PROVIDER, &source).expect("the variable is read");
        assert_eq!(found.expose(), KEY_SENTINEL);
        assert_eq!(
            state(PROVIDER, Some(&source), &Rejections::default()),
            KeyState::EnvFound
        );
    }

    #[test]
    fn a_missing_variable_is_env_missing() {
        let _scratch = scratch("summ-keys");
        set(PROVIDER, &key(KEY_SENTINEL)).expect("a stored key is not read for an env source");
        let source = KeySource::Env(ABSENT_VAR.to_owned());
        assert!(get(PROVIDER, &source).is_none());
        assert_eq!(
            state(PROVIDER, Some(&source), &Rejections::default()),
            KeyState::EnvMissing
        );
    }

    #[test]
    fn an_unknown_source_is_missing() {
        let _scratch = scratch("summ-keys");
        set(PROVIDER, &key(KEY_SENTINEL)).expect("the key saves");
        let source = KeySource::from_wire(&SummarizerKeySource::Unknown);
        assert_eq!(source, None);
        assert_eq!(
            state(PROVIDER, source.as_ref(), &Rejections::default()),
            KeyState::Missing
        );
    }

    #[test]
    fn the_wire_sources_map_to_key_sources() {
        assert_eq!(
            KeySource::from_wire(&SummarizerKeySource::Stored),
            Some(KeySource::Stored)
        );
        assert_eq!(
            KeySource::from_wire(&SummarizerKeySource::Env {
                variable: SET_VAR.to_owned()
            }),
            Some(KeySource::Env(SET_VAR.to_owned()))
        );
    }

    #[test]
    fn an_empty_or_over_long_key_is_refused_without_echoing_it() {
        let _scratch = scratch("summ-keys");
        for blank in ["", "   "] {
            let err = set(PROVIDER, &key(blank)).expect_err("a blank key is refused");
            assert!(format!("{err:#}").contains("empty"), "{err:#}");
        }
        let long = KEY_SENTINEL.repeat(env_secrets::MAX_VALUE_UNITS / KEY_SENTINEL.len() + 1);
        let err = set(PROVIDER, &key(&long)).expect_err("an over-long key is refused");
        assert!(!format!("{err:#}").contains(KEY_SENTINEL), "{err:#}");
        assert!(!format!("{err:?}").contains(KEY_SENTINEL), "{err:?}");
        assert_eq!(stored(PROVIDER), None);
    }

    #[test]
    fn a_store_failure_does_not_carry_the_key_text() {
        let _scratch = scratch("summ-keys");
        set(PROVIDER, &key("earlier-key")).expect("the first key saves");
        inject_store_error(PROVIDER);
        let err = set(PROVIDER, &key(KEY_SENTINEL)).expect_err("the store refuses");
        assert!(!format!("{err:#}").contains(KEY_SENTINEL), "{err:#}");
        assert!(!format!("{err:?}").contains(KEY_SENTINEL), "{err:?}");
    }

    #[test]
    fn debug_of_a_key_and_of_a_struct_holding_it_shows_no_key_text() {
        let _scratch = scratch("summ-keys");
        set(PROVIDER, &key(KEY_SENTINEL)).expect("the key saves");
        let found = get(PROVIDER, &KeySource::Stored).expect("the key reads back");
        let rejections = Rejections::default();
        rejections.reject(PROVIDER, &found);
        let held = (KeySource::Stored, Some(found));
        for text in [
            format!("{:?}", held.1),
            format!("{held:?}"),
            format!("{held:#?}"),
            format!("{rejections:?}"),
        ] {
            assert!(!text.contains(KEY_SENTINEL), "{text}");
        }
        assert!(format!("{held:?}").contains("<redacted>"));
    }

    #[test]
    fn a_rejected_key_reads_rejected_until_the_key_changes() {
        let _scratch = scratch("summ-keys");
        let rejections = Rejections::default();
        set(PROVIDER, &key("refused-key")).expect("the key saves");
        rejections.reject(PROVIDER, &key("refused-key"));
        assert_eq!(
            state(PROVIDER, Some(&KeySource::Stored), &rejections),
            KeyState::Rejected
        );
        set(PROVIDER, &key("new-key")).expect("the new key saves");
        assert_eq!(
            state(PROVIDER, Some(&KeySource::Stored), &rejections),
            KeyState::Stored
        );
        set(PROVIDER, &key("refused-key")).expect("the old key saves again");
        assert_eq!(
            state(PROVIDER, Some(&KeySource::Stored), &rejections),
            KeyState::Stored,
            "a changed key cleared the record, so the old key is tried again"
        );
    }

    #[test]
    fn a_rejected_variable_clears_when_its_value_changes() {
        let _scratch = scratch("summ-keys");
        let rejections = Rejections::default();
        let source = KeySource::Env(SET_VAR.to_owned());
        {
            let _var = EnvVarGuard::set(SET_VAR, "refused-key");
            rejections.reject(PROVIDER, &key("refused-key"));
            assert_eq!(
                state(PROVIDER, Some(&source), &rejections),
                KeyState::Rejected
            );
        }
        let _var = EnvVarGuard::set(SET_VAR, "new-key");
        assert_eq!(
            state(PROVIDER, Some(&source), &rejections),
            KeyState::EnvFound
        );
    }

    #[test]
    fn a_rejection_is_per_provider() {
        let rejections = Rejections::default();
        rejections.reject("provider-a", &key("shared-key"));
        assert!(rejections.is_rejected("provider-a", &key("shared-key")));
        assert!(!rejections.is_rejected("provider-b", &key("shared-key")));
    }

    #[test]
    fn the_unreferenced_sweep_leaves_a_summarizer_key_alone() {
        let _scratch = scratch("summ-keys");
        set(PROVIDER, &key(KEY_SENTINEL)).expect("the key saves");
        let later = Utc::now() + TimeDelta::hours(2);
        let _ = env_secrets::collect_unreferenced(&HashSet::new(), later);
        assert_eq!(stored(PROVIDER).as_deref(), Some(KEY_SENTINEL));
    }
}
