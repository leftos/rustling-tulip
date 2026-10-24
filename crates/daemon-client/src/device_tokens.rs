//! The device tokens this client holds for remote hosts.
//!
//! A tailnet host's daemon serves this device only when its `Hello` carries
//! the token that host issued it at pairing. [`CredentialManagerTokens`] keeps
//! them in Windows Credential Manager, one credential per host, under the
//! service the daemon files its env secrets under: `rustling-tulip`, or
//! `rustling-tulip:<first 8 hex of the SHA-256 of the config dir>` while
//! `RUSTLING_TULIP_CONFIG_DIR` is set, so an isolated client never shares
//! entries with the user's own. The credential's user is
//! `device/<lower-cased host name>`, with no port: one device identity per
//! host name. [`MemoryTokens`] holds them in memory, for tests.
//!
//! A token never appears in a log line, an error or a `Debug` string.

use anyhow::{Context as _, Result};
use keyring_core::{Entry, Error as KeyringError};
use sha2::{Digest as _, Sha256};
use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};

/// The Credential Manager service tokens are filed under, unless the config
/// dir is overridden.
const SERVICE: &str = "rustling-tulip";

/// Setting this to a non-empty value switches the service to its
/// per-config-dir variant.
const CONFIG_DIR_VAR: &str = "RUSTLING_TULIP_CONFIG_DIR";

/// How many hex characters of the config dir's hash the isolated service name
/// carries.
const SERVICE_HASH_LEN: usize = 8;

/// What every device token's credential user starts with.
const USER_PREFIX: &str = "device/";

/// Where the device tokens are kept, one per host.
pub trait DeviceTokenStore: Send + Sync {
    /// The token held for `host`; `None` when none is held.
    ///
    /// # Errors
    ///
    /// Fails when the store cannot be read.
    fn get(&self, host: &str) -> Result<Option<String>>;

    /// Hold `token` for `host`, replacing any token held for it before.
    ///
    /// # Errors
    ///
    /// Fails when the store cannot be written.
    fn set(&self, host: &str, token: &str) -> Result<()>;

    /// Forget the token held for `host`; holding none is not an error.
    ///
    /// # Errors
    ///
    /// Fails when the store cannot be written.
    fn delete(&self, host: &str) -> Result<()>;
}

/// The credential user `host`'s token is filed under: `device/` and the host
/// name lower-cased, without a `:<port>` suffix.
#[must_use]
pub fn user_for(host: &str) -> String {
    let name = match host.rsplit_once(':') {
        Some((name, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => name,
        _ => host,
    };
    format!("{USER_PREFIX}{}", name.to_lowercase())
}

/// The service tokens are filed under for `config_dir`: the plain name, or
/// its per-config-dir variant when `overridden` (the config dir comes from
/// `RUSTLING_TULIP_CONFIG_DIR`). The daemon's env secrets use the same rule
/// (`crates/daemon/src/env_secrets.rs`).
fn service_for(config_dir: &Path, overridden: bool) -> String {
    if !overridden {
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

/// The service for this process's config dir.
#[cfg(windows)]
fn service() -> Result<String> {
    let overridden = std::env::var(CONFIG_DIR_VAR).is_ok_and(|value| !value.is_empty());
    Ok(service_for(&crate::config_dir()?, overridden))
}

/// Device tokens in Windows Credential Manager.
#[derive(Debug)]
pub struct CredentialManagerTokens {
    service: String,
}

impl CredentialManagerTokens {
    /// Install Windows Credential Manager as the process's credential store
    /// and file tokens under this config dir's service.
    ///
    /// # Errors
    ///
    /// Fails when the platform store cannot be created or the config dir
    /// cannot be resolved.
    #[cfg(windows)]
    pub fn open() -> Result<Self> {
        let store = windows_native_keyring_store::Store::new()
            .context("creating the Windows credential store")?;
        keyring_core::set_default_store(store);
        Ok(Self {
            service: service()?,
        })
    }

    /// # Errors
    ///
    /// Always: the store is Windows Credential Manager.
    #[cfg(not(windows))]
    pub fn open() -> Result<Self> {
        anyhow::bail!(
            "device tokens live in Windows Credential Manager; there is no store off Windows"
        )
    }

    fn entry(&self, host: &str) -> Result<Entry> {
        Entry::new(&self.service, &user_for(host))
            .with_context(|| format!("opening the device token credential for {host}"))
    }
}

impl DeviceTokenStore for CredentialManagerTokens {
    fn get(&self, host: &str) -> Result<Option<String>> {
        match self.entry(host)?.get_password() {
            Ok(token) => Ok(Some(token)),
            Err(KeyringError::NoEntry) => Ok(None),
            Err(err) => Err(err).with_context(|| format!("reading the device token for {host}")),
        }
    }

    fn set(&self, host: &str, token: &str) -> Result<()> {
        self.entry(host)?.set_password(token).with_context(|| {
            format!("saving the device token for {host} to Windows Credential Manager")
        })
    }

    fn delete(&self, host: &str) -> Result<()> {
        match self.entry(host)?.delete_credential() {
            Ok(()) | Err(KeyringError::NoEntry) => Ok(()),
            Err(err) => Err(err).with_context(|| format!("deleting the device token for {host}")),
        }
    }
}

/// Device tokens held in memory, for tests. Keyed like the credentials, so a
/// host's case and port do not matter.
#[derive(Default)]
pub struct MemoryTokens {
    tokens: Mutex<HashMap<String, String>>,
}

impl MemoryTokens {
    /// A store holding `token` for `host`.
    #[must_use]
    pub fn with(host: &str, token: &str) -> Self {
        let store = Self::default();
        store.lock().insert(user_for(host), token.to_owned());
        store
    }

    /// The map, also after a thread panicked holding it: every write is a
    /// single insert or remove, so it is never left half-done.
    fn lock(&self) -> MutexGuard<'_, HashMap<String, String>> {
        self.tokens.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl fmt::Debug for MemoryTokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut hosts: Vec<String> = self.lock().keys().cloned().collect();
        hosts.sort();
        f.debug_struct("MemoryTokens")
            .field("hosts", &hosts)
            .finish_non_exhaustive()
    }
}

impl DeviceTokenStore for MemoryTokens {
    fn get(&self, host: &str) -> Result<Option<String>> {
        Ok(self.lock().get(&user_for(host)).cloned())
    }

    fn set(&self, host: &str, token: &str) -> Result<()> {
        self.lock().insert(user_for(host), token.to_owned());
        Ok(())
    }

    fn delete(&self, host: &str) -> Result<()> {
        self.lock().remove(&user_for(host));
        Ok(())
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::{DeviceTokenStore as _, MemoryTokens, service_for, user_for};
    use std::path::Path;

    #[test]
    fn service_for_hashes_config_dir_like_env_secrets() {
        // The first 8 hex of `printf '%s' <dir> | sha256sum`: the daemon's
        // rule for its env secrets (`crates/daemon/src/env_secrets.rs`).
        assert_eq!(
            service_for(Path::new(r"X:\rt\config"), true),
            "rustling-tulip:f10321bb"
        );
        assert_eq!(
            service_for(Path::new("/tmp/rt-config"), true),
            "rustling-tulip:29c49caf"
        );
        assert_eq!(
            service_for(Path::new(r"X:\rt\config"), false),
            "rustling-tulip",
            "without the override the service is the plain name"
        );
    }

    #[test]
    fn user_key_is_the_lower_cased_host_without_port() {
        assert_eq!(
            user_for("box.tail1234.ts.net"),
            "device/box.tail1234.ts.net"
        );
        assert_eq!(
            user_for("Box.Tail1234.TS.net:8787"),
            "device/box.tail1234.ts.net"
        );
        assert_eq!(user_for("BOX"), "device/box");
    }

    #[test]
    fn memory_store_round_trips_and_deletes() {
        let store = MemoryTokens::default();
        assert_eq!(store.get("box").expect("reads"), None);

        store.set("box", "first").expect("writes");
        store.set("other", "second").expect("writes");
        assert_eq!(store.get("box").expect("reads").as_deref(), Some("first"));
        assert_eq!(
            store.get("BOX:8787").expect("reads").as_deref(),
            Some("first"),
            "the key is the lower-cased host without its port"
        );

        store.set("box", "replaced").expect("writes");
        assert_eq!(
            store.get("box").expect("reads").as_deref(),
            Some("replaced")
        );

        store.delete("box").expect("deletes");
        assert_eq!(store.get("box").expect("reads"), None);
        assert_eq!(
            store.get("other").expect("reads").as_deref(),
            Some("second"),
            "a delete leaves the other hosts"
        );
        store
            .delete("box")
            .expect("deleting a token that is not held is not an error");

        let seeded = MemoryTokens::with("seeded", "token");
        assert_eq!(
            seeded.get("seeded").expect("reads").as_deref(),
            Some("token")
        );
    }

    #[test]
    fn token_is_not_in_debug_output() {
        const TOKEN: &str = "s3cret-device-token";
        let store = MemoryTokens::with("box.tail1234.ts.net", TOKEN);
        let shown = format!("{store:?}");
        assert!(!shown.contains(TOKEN), "{shown}");
        assert!(shown.contains("box.tail1234.ts.net"), "{shown}");
    }
}
