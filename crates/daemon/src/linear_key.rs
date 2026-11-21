//! The host's Linear API key: one entry in the secret store under the config
//! dir's service (user `linear/api-key`), else `LINEAR_API_KEY`, and which of
//! them the daemon uses. The entry has no index entry, so the unreferenced
//! secret sweep never removes it.
//!
//! The store calls block; an async caller runs them on a blocking thread.

use anyhow::Context as _;
use keyring_core::{Entry, Error as KeyringError};
use protocol::LinearKeyState;
use tracing::warn;

use crate::env_secrets::{self, VaultUnreachable};
use crate::user_env::Secret;

/// The provider id the key's rejection is recorded under in a
/// [`crate::summarizer::keys::Rejections`].
pub(crate) const REJECTION_ID: &str = "linear";

/// Save `key` as the Linear key, replacing any saved before.
///
/// # Errors
///
/// The key is blank or over the store's size limit, the config dir cannot be
/// resolved, or the store refused the write ([`VaultUnreachable`] when the
/// secret store is out of reach). No message carries the key.
pub(crate) fn set(key: &Secret) -> anyhow::Result<()> {
    let text = key.expose();
    if text.trim().is_empty() {
        anyhow::bail!("the Linear API key is empty; paste the whole key");
    }
    let units = text.encode_utf16().count();
    if units > env_secrets::MAX_VALUE_UNITS {
        anyhow::bail!(
            "the Linear API key is {units} UTF-16 units long; the secret store holds at most {}",
            env_secrets::MAX_VALUE_UNITS
        );
    }
    match entry()?.set_password(text) {
        Ok(()) => Ok(()),
        Err(err) if env_secrets::is_store_out_of_reach(&err) => Err(VaultUnreachable.into()),
        Err(err) => Err(err).context("saving the Linear API key to the secret store"),
    }
}

/// The saved key, or `None` when there is none or the store fails to read it
/// (logged, without the key).
///
/// # Errors
///
/// [`VaultUnreachable`] when the secret store is out of reach of this
/// daemon's logon session, so the key reads as unreachable, not as missing.
pub(crate) fn read_stored() -> Result<Option<Secret>, VaultUnreachable> {
    let read = entry().and_then(|entry| entry.get_password().map_err(anyhow::Error::from));
    match read {
        Ok(text) => Ok(Some(Secret::new(text))),
        Err(err) => match err.downcast_ref::<KeyringError>() {
            Some(KeyringError::NoEntry) => Ok(None),
            Some(store_err) if env_secrets::is_store_out_of_reach(store_err) => {
                Err(VaultUnreachable)
            }
            _ => {
                warn!(error = %format!("{err:#}"), "linear: could not read the saved key; treating it as missing");
                Ok(None)
            }
        },
    }
}

/// Delete the saved key. Deleting a key that is not saved succeeds.
///
/// # Errors
///
/// The config dir cannot be resolved or the store refused the delete
/// ([`VaultUnreachable`] when the secret store is out of reach).
pub(crate) fn delete() -> anyhow::Result<()> {
    match entry()?.delete_credential() {
        Ok(()) | Err(KeyringError::NoEntry) => Ok(()),
        Err(err) if env_secrets::is_store_out_of_reach(&err) => Err(VaultUnreachable.into()),
        Err(err) => Err(err).context("deleting the Linear API key from the secret store"),
    }
}

/// The key to use and its state: none while `offline`; else the saved key;
/// else `env`. A store out of reach counts as no saved key, and reads as
/// [`LinearKeyState::Unreachable`] only when `env` is unset too.
pub(crate) fn choose(
    offline: bool,
    stored: Result<Option<Secret>, VaultUnreachable>,
    env: Option<Secret>,
) -> (Option<Secret>, LinearKeyState) {
    if offline {
        return (None, LinearKeyState::Missing);
    }
    match (stored, env) {
        (Ok(Some(key)), _) => (Some(key), LinearKeyState::Stored),
        (Ok(None) | Err(VaultUnreachable), Some(env)) => (Some(env), LinearKeyState::Environment),
        (Ok(None), None) => (None, LinearKeyState::Missing),
        (Err(VaultUnreachable), None) => (None, LinearKeyState::Unreachable),
    }
}

/// The key the daemon uses now, read from the store then `LINEAR_API_KEY`;
/// with `RUSTLING_TULIP_LINEAR_OFFLINE` exactly `1`, nothing is read at all.
/// Blocks on the store.
pub(crate) fn current() -> (Option<Secret>, LinearKeyState) {
    let offline = crate::linear::is_offline(
        std::env::var(crate::linear::OFFLINE_VARIABLE)
            .ok()
            .as_deref(),
    );
    if offline {
        return choose(true, Ok(None), None);
    }
    choose(false, read_stored(), crate::linear::configured_key())
}

/// The credential's user string, so a person reading the secret store can
/// tell what the entry is.
fn user() -> &'static str {
    "linear/api-key"
}

fn entry() -> anyhow::Result<Entry> {
    let service = env_secrets::service()?;
    Entry::new(&service, user()).context("opening the credential entry for the Linear API key")
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests fail loudly on setup errors")]
mod tests {
    use super::*;
    use crate::env_secrets::test_support::scratch;

    /// A key no message may ever contain.
    const KEY_SENTINEL: &str = "lin_api_sentinel_7c41e0";

    fn key(text: &str) -> Secret {
        Secret::new(text.to_owned())
    }

    fn exposed(chosen: &(Option<Secret>, LinearKeyState)) -> (Option<&str>, LinearKeyState) {
        (chosen.0.as_ref().map(Secret::expose), chosen.1)
    }

    /// The secret store user name the key is filed under. A change to it
    /// orphans every key a user saved.
    #[test]
    fn the_linear_key_user_name_is_pinned() {
        assert_eq!(user(), "linear/api-key");
    }

    #[test]
    fn offline_chooses_no_key() {
        let chosen = choose(true, Ok(Some(key("saved"))), Some(key("env")));
        assert_eq!(exposed(&chosen), (None, LinearKeyState::Missing));
    }

    #[test]
    fn a_saved_key_wins_over_the_variable() {
        let chosen = choose(false, Ok(Some(key("saved"))), Some(key("env")));
        assert_eq!(exposed(&chosen), (Some("saved"), LinearKeyState::Stored));
    }

    #[test]
    fn no_saved_key_falls_back_to_the_variable() {
        let chosen = choose(false, Ok(None), Some(key("env")));
        assert_eq!(exposed(&chosen), (Some("env"), LinearKeyState::Environment));
    }

    #[test]
    fn no_saved_key_and_no_variable_is_missing() {
        let chosen = choose(false, Ok(None), None);
        assert_eq!(exposed(&chosen), (None, LinearKeyState::Missing));
    }

    #[test]
    fn an_unreachable_store_falls_back_to_the_variable() {
        let chosen = choose(false, Err(VaultUnreachable), Some(key("env")));
        assert_eq!(exposed(&chosen), (Some("env"), LinearKeyState::Environment));
    }

    #[test]
    fn an_unreachable_store_and_no_variable_is_unreachable() {
        let chosen = choose(false, Err(VaultUnreachable), None);
        assert_eq!(exposed(&chosen), (None, LinearKeyState::Unreachable));
    }

    #[test]
    fn a_blank_or_over_long_key_is_refused_without_its_text() {
        let _scratch = scratch("linear-key-refuse");
        let blank = set(&key("   ")).expect_err("a blank key is refused");
        assert!(blank.to_string().contains("empty"), "{blank:#}");

        let long = format!("{KEY_SENTINEL}{}", "x".repeat(env_secrets::MAX_VALUE_UNITS));
        let refusal = set(&key(&long)).expect_err("an over-long key is refused");
        let text = format!("{refusal:#} {refusal:?}");
        assert!(!text.contains(KEY_SENTINEL), "{text}");
        assert!(text.contains("the secret store holds at most"), "{text}");
        assert!(
            read_stored().expect("the store is within reach").is_none(),
            "nothing is saved"
        );
    }

    #[test]
    fn a_saved_key_reads_back_and_deletes() {
        let _scratch = scratch("linear-key-round-trip");
        assert!(read_stored().expect("the store is within reach").is_none());
        set(&key(KEY_SENTINEL)).expect("the key saves");
        let read = read_stored().expect("the store is within reach");
        assert_eq!(read.as_ref().map(Secret::expose), Some(KEY_SENTINEL));
        set(&key("second-key")).expect("the key saves again");
        let read = read_stored().expect("the store is within reach");
        assert_eq!(read.as_ref().map(Secret::expose), Some("second-key"));
        delete().expect("the key deletes");
        assert!(read_stored().expect("the store is within reach").is_none());
        delete().expect("deleting a key that is not saved succeeds");
    }
}
