//! The device tokens this client holds for remote hosts.
//!
//! A tailnet host's daemon serves this device only when its `Hello` carries
//! the token that host issued it at pairing. [`KeyringTokens`] keeps them in
//! the platform's credential store, Windows Credential Manager on Windows and
//! the login Keychain on macOS (a generic password), one item per host, under
//! the service the daemon files its env secrets under: `rustling-tulip`, or
//! `rustling-tulip:<first 8 hex of the SHA-256 of the config dir>` while
//! `RUSTLING_TULIP_CONFIG_DIR` is set, so an isolated client never shares
//! entries with the user's own. The item's user (the Keychain's account) is
//! `device/<lower-cased host name>`, with no port: one device identity per
//! host name. Both platforms use the same service and user names.
//! [`MemoryTokens`] holds them in memory, for tests.
//!
//! A token never appears in a log line, an error or a `Debug` string.

use anyhow::{Context as _, Result};
use keyring_core::{Entry, Error as KeyringError};
#[cfg(any(windows, target_os = "macos", test))]
use sha2::{Digest as _, Sha256};
use std::collections::HashMap;
use std::fmt;
#[cfg(any(windows, target_os = "macos", test))]
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};

/// The credential store service tokens are filed under, unless the config
/// dir is overridden.
#[cfg(any(windows, target_os = "macos", test))]
const SERVICE: &str = "rustling-tulip";

/// Setting this to a non-empty value switches the service to its
/// per-config-dir variant.
#[cfg(any(windows, target_os = "macos"))]
const CONFIG_DIR_VAR: &str = "RUSTLING_TULIP_CONFIG_DIR";

/// How many hex characters of the config dir's hash the isolated service name
/// carries.
#[cfg(any(windows, target_os = "macos", test))]
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
#[cfg(any(windows, target_os = "macos", test))]
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
#[cfg(any(windows, target_os = "macos"))]
fn service() -> Result<String> {
    let overridden = std::env::var(CONFIG_DIR_VAR).is_ok_and(|value| !value.is_empty());
    Ok(service_for(&crate::config_dir()?, overridden))
}

/// Install Windows Credential Manager as the process's credential store.
#[cfg(windows)]
fn install_store() -> Result<()> {
    let store = windows_native_keyring_store::Store::new()
        .context("creating the Windows credential store")?;
    keyring_core::set_default_store(store);
    Ok(())
}

/// Install the user's login Keychain as the process's credential store: one
/// generic password per (service, user) pair.
#[cfg(target_os = "macos")]
fn install_store() -> Result<()> {
    let store = apple_native_keyring_store::keychain::Store::new()
        .context("creating the macOS Keychain credential store")?;
    keyring_core::set_default_store(store);
    Ok(())
}

/// Device tokens in the platform's credential store: Windows Credential
/// Manager on Windows, the login Keychain on macOS.
#[derive(Debug)]
pub struct KeyringTokens {
    service: String,
}

impl KeyringTokens {
    /// A store filing tokens under `service`, for a test that must not touch
    /// the user's own items.
    #[cfg(all(test, target_os = "macos"))]
    fn with_service(service: String) -> Result<Self> {
        install_store()?;
        Ok(Self { service })
    }

    /// Install the platform's credential store (Windows Credential Manager,
    /// or the macOS login Keychain) as the process's credential store and
    /// file tokens under this config dir's service.
    ///
    /// # Errors
    ///
    /// Fails when the platform store cannot be created or the config dir
    /// cannot be resolved.
    #[cfg(any(windows, target_os = "macos"))]
    pub fn open() -> Result<Self> {
        install_store()?;
        Ok(Self {
            service: service()?,
        })
    }

    /// # Errors
    ///
    /// Always: the store is Windows Credential Manager or the macOS Keychain.
    #[cfg(not(any(windows, target_os = "macos")))]
    pub fn open() -> Result<Self> {
        anyhow::bail!(
            "device tokens live in Windows Credential Manager or the macOS Keychain; there is no store on this system"
        )
    }

    fn entry(&self, host: &str) -> Result<Entry> {
        Entry::new(&self.service, &user_for(host))
            .with_context(|| format!("opening the device token credential for {host}"))
    }
}

impl DeviceTokenStore for KeyringTokens {
    fn get(&self, host: &str) -> Result<Option<String>> {
        match self.entry(host)?.get_password() {
            Ok(token) => Ok(Some(token)),
            Err(KeyringError::NoEntry) => Ok(None),
            Err(err) => Err(err).with_context(|| format!("reading the device token for {host}")),
        }
    }

    fn set(&self, host: &str, token: &str) -> Result<()> {
        self.entry(host)?
            .set_password(token)
            .with_context(|| format!("saving the device token for {host} to the credential store"))
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

    /// The entry names v0.5.1-alpha filed a device token under. A change to
    /// either half orphans the token a released client holds, and the device
    /// then fails to pair with its host until it is approved again.
    #[test]
    fn device_tokens_entry_names_match_v0_5_1_alpha() {
        assert_eq!(
            service_for(Path::new(r"X:\rt\config"), false),
            "rustling-tulip",
            "the production service is the plain name"
        );
        assert_eq!(
            service_for(Path::new(r"X:\rt\config"), true),
            "rustling-tulip:f10321bb",
            "an isolated client files under the first 8 hex of its config dir"
        );
        assert_eq!(
            user_for("box.tail1234.ts.net"),
            "device/box.tail1234.ts.net",
            "a host's token is filed under device/ and its name"
        );
        assert_eq!(
            user_for("Box.Tail1234.TS.net:8787"),
            "device/box.tail1234.ts.net",
            "the user is lower-cased and carries no port"
        );
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

    /// A token saved to the login Keychain is read back by a fresh store on
    /// the same service, as a relaunched client builds, and is gone once
    /// deleted. The service is unique to the run, so the user's own items are
    /// never read or written.
    #[test]
    #[cfg(target_os = "macos")]
    #[ignore = "touches the login Keychain; run on the Mac builder"]
    fn keychain_store_round_trips_a_token() {
        use super::KeyringTokens;
        use std::fmt::Write as _;

        /// Deletes the test's item however the test ends.
        struct Cleanup<'a>(&'a KeyringTokens);
        impl Drop for Cleanup<'_> {
            fn drop(&mut self) {
                let _ = self.0.delete(HOST);
            }
        }

        const HOST: &str = "box.tail1234.ts.net:8787";
        const TOKEN: &str = "keychain-test-token";

        let mut bytes = [0_u8; 8];
        getrandom::fill(&mut bytes).expect("reads random bytes");
        let mut service = String::from("rustling-tulip-test:");
        for byte in bytes {
            let _ = write!(service, "{byte:02x}");
        }

        let store = KeyringTokens::with_service(service.clone()).expect("opens the Keychain");
        let _cleanup = Cleanup(&store);
        store.set(HOST, TOKEN).expect("saves the token");

        let relaunched = KeyringTokens::with_service(service).expect("opens the Keychain again");
        assert_eq!(
            relaunched.get(HOST).expect("reads").as_deref(),
            Some(TOKEN),
            "a fresh store reads the saved token back"
        );

        relaunched.delete(HOST).expect("deletes the token");
        assert_eq!(store.get(HOST).expect("reads"), None, "the token is gone");
    }
}
