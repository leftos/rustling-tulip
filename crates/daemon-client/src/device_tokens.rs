//! The device tokens this client holds for remote hosts.
//!
//! A tailnet host's daemon serves this device only when its `Hello` carries
//! the token that host issued it at pairing. [`KeyringTokens`] keeps them in
//! the platform's credential store: Windows Credential Manager on Windows,
//! the login Keychain on macOS and the iOS Keychain on iOS (a generic
//! password), one item per host, under
//! the service the daemon files its env secrets under: `rustling-tulip`, or
//! `rustling-tulip:<first 8 hex of the SHA-256 of the config dir>` while
//! `RUSTLING_TULIP_CONFIG_DIR` is set, so an isolated client never shares
//! entries with the user's own. The item's user (the Keychain's account) is
//! `device/<lower-cased host name>`, with no port: one device identity per
//! host name. All three platforms use the same service and user names.
//! [`MemoryTokens`] holds them in memory, for tests.
//!
//! On iOS an item is readable after the device's first unlock, on this device
//! only, so an app a push launches while the phone is locked still reads its
//! tokens. An item an older build wrote (readable only while unlocked) is
//! moved to that policy on its first read, once per host: the hosts moved are
//! listed in `device-token-policy` in the config dir.
//!
//! A token never appears in a log line, an error or a `Debug` string.

use anyhow::{Context as _, Result};
use keyring_core::{Entry, Error as KeyringError};
#[cfg(any(windows, target_os = "macos", target_os = "ios", test))]
use sha2::{Digest as _, Sha256};
use std::collections::HashMap;
#[cfg(any(target_os = "ios", test))]
use std::collections::HashSet;
use std::fmt;
#[cfg(any(windows, target_os = "macos", target_os = "ios", test))]
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};

/// The credential store service tokens are filed under, unless the config
/// dir is overridden.
#[cfg(any(windows, target_os = "macos", target_os = "ios", test))]
const SERVICE: &str = "rustling-tulip";

/// Setting this to a non-empty value switches the service to its
/// per-config-dir variant.
#[cfg(any(windows, target_os = "macos", target_os = "ios"))]
const CONFIG_DIR_VAR: &str = "RUSTLING_TULIP_CONFIG_DIR";

/// How many hex characters of the config dir's hash the isolated service name
/// carries.
#[cfg(any(windows, target_os = "macos", target_os = "ios", test))]
const SERVICE_HASH_LEN: usize = 8;

/// What every device token's credential user starts with.
const USER_PREFIX: &str = "device/";

/// The file in the config dir listing the credential users (one per line)
/// whose iOS Keychain item has been moved to [`IOS_ACCESS_POLICY`].
#[cfg(any(target_os = "ios", test))]
const POLICY_MARKER_FILE: &str = "device-token-policy";

/// The iOS protected store's access policy for device tokens: readable after
/// the first unlock, on this device only, as the push keys are.
#[cfg(target_os = "ios")]
const IOS_ACCESS_POLICY: &str = "AfterFirstUnlockThisDeviceOnly";

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
#[cfg(any(windows, target_os = "macos", target_os = "ios", test))]
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
#[cfg(any(windows, target_os = "macos", target_os = "ios"))]
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

/// Install the iOS Keychain as the process's credential store: the protected
/// store, iOS's one Keychain store, one generic password per (service, user)
/// pair.
#[cfg(target_os = "ios")]
fn install_store() -> Result<()> {
    let store = apple_native_keyring_store::protected::Store::new()
        .context("creating the iOS Keychain credential store")?;
    keyring_core::set_default_store(store);
    Ok(())
}

/// What moving a stored token to a new access policy needs, so the order of
/// [`read_and_move`] is the same on iOS and in its tests.
#[cfg(any(target_os = "ios", test))]
trait PolicyMove {
    /// The token held for `host`, read whatever the item's policy.
    fn read(&self, host: &str) -> Result<Option<String>>;

    /// Delete `host`'s item, whatever its policy.
    fn delete_old(&self, host: &str) -> Result<()>;

    /// Save `token` for `host` under the new policy.
    fn save_new(&self, host: &str, token: &str) -> Result<()>;

    /// Save `token` for `host` under the old policy, the one older builds
    /// wrote, so the item exists again after a move that could not finish.
    fn save_old(&self, host: &str, token: &str) -> Result<()>;

    /// Whether `host`'s item is already recorded as moved.
    fn is_moved(&self, host: &str) -> bool;

    /// Record `host`'s item as moved, for this launch and the next ones.
    fn record_moved(&self, host: &str) -> Result<()>;
}

/// Read `host`'s token and, the first time one is read, move its item to the
/// new policy: delete it, save the same token again and record the host. A
/// failed delete leaves the item as it was; a save that fails twice saves the
/// token under the old policy instead. Either way the host stays unrecorded,
/// so a later read tries again, and the token read is returned. A failed read
/// or no token moves nothing.
#[cfg(any(target_os = "ios", test))]
fn read_and_move(store: &impl PolicyMove, host: &str) -> Result<Option<String>> {
    let Some(token) = store.read(host)? else {
        return Ok(None);
    };
    if store.is_moved(host) {
        return Ok(Some(token));
    }
    if let Err(err) = store.delete_old(host) {
        tracing::warn!(
            host,
            "moving the device token to the after-first-unlock policy failed; trying again on the next read: {err:#}"
        );
        return Ok(Some(token));
    }
    let saved = store.save_new(host, &token).or_else(|err| {
        tracing::warn!(
            host,
            "saving the device token under the after-first-unlock policy failed; trying once more: {err:#}"
        );
        store.save_new(host, &token)
    });
    match saved {
        Ok(()) => record_logged(store, host),
        Err(err) => restore_old(store, host, &token, &err),
    }
    Ok(Some(token))
}

/// Save `token` for `host` under the new policy and record the host, so its
/// next read moves nothing; a failed record is logged, never returned.
///
/// # Errors
///
/// Fails when the save fails.
#[cfg(any(target_os = "ios", test))]
fn save_and_record(store: &impl PolicyMove, host: &str, token: &str) -> Result<()> {
    store.save_new(host, token)?;
    record_logged(store, host);
    Ok(())
}

/// Record `host` as moved, logging a failure: the next launch then moves the
/// item again, which is harmless.
#[cfg(any(target_os = "ios", test))]
fn record_logged(store: &impl PolicyMove, host: &str) {
    if let Err(err) = store.record_moved(host) {
        tracing::warn!(
            host,
            "recording the device token's after-first-unlock policy failed: {err:#}"
        );
    }
}

/// Save `token` for `host` again under the old policy after the new-policy
/// save failed with `err`, leaving the host unrecorded.
#[cfg(any(target_os = "ios", test))]
fn restore_old(store: &impl PolicyMove, host: &str, token: &str, err: &anyhow::Error) {
    tracing::warn!(
        host,
        "saving the device token under the after-first-unlock policy failed again; saving it as before: {err:#}"
    );
    if let Err(err) = store.save_old(host, token) {
        tracing::warn!(
            "the device token for {host} could not be saved again after its move; pair this host again if it shows Not paired: {err:#}"
        );
    }
}

/// The credential users listed in the marker file at `path`; none when the
/// file is missing or cannot be read.
#[cfg(any(target_os = "ios", test))]
fn load_moved(path: &Path) -> HashSet<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => HashSet::new(),
        Err(err) => {
            tracing::warn!(path = %path.display(), "reading the device token policy marker failed: {err}");
            HashSet::new()
        }
    }
}

/// Write `users` to the marker file at `path`, one per line in order,
/// through a temporary file renamed over it.
#[cfg(any(target_os = "ios", test))]
fn save_moved(path: &Path, users: &HashSet<String>) -> Result<()> {
    let mut lines: Vec<&str> = users.iter().map(String::as_str).collect();
    lines.sort_unstable();
    let mut text = lines.join("\n");
    text.push('\n');
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))
}

/// The hosts whose iOS Keychain item is moved to [`IOS_ACCESS_POLICY`]:
/// the marker file and its credential users, loaded when the store opens.
#[cfg(target_os = "ios")]
#[derive(Debug)]
struct MovedHosts {
    path: std::path::PathBuf,
    users: Mutex<HashSet<String>>,
}

#[cfg(target_os = "ios")]
impl MovedHosts {
    fn open(path: std::path::PathBuf) -> Self {
        let users = Mutex::new(load_moved(&path));
        Self { path, users }
    }

    fn lock(&self) -> MutexGuard<'_, HashSet<String>> {
        self.users.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Device tokens in the platform's credential store: Windows Credential
/// Manager on Windows, the login Keychain on macOS, the iOS Keychain on iOS.
#[derive(Debug)]
pub struct KeyringTokens {
    service: String,
    /// Held across each whole get (its read and any move), set and delete,
    /// so no call sees or undoes a move half done.
    calls: Mutex<()>,
    /// The hosts whose item has been moved to the after-first-unlock policy.
    #[cfg(target_os = "ios")]
    moved: MovedHosts,
}

impl KeyringTokens {
    /// A store filing tokens under `service`, for a test that must not touch
    /// the user's own items.
    #[cfg(all(test, target_os = "macos"))]
    fn with_service(service: String) -> Result<Self> {
        install_store()?;
        Ok(Self {
            service,
            calls: Mutex::new(()),
        })
    }

    /// Install the platform's credential store (Windows Credential Manager,
    /// the macOS login Keychain or the iOS Keychain) as the process's
    /// credential store and file tokens under this config dir's service.
    ///
    /// # Errors
    ///
    /// Fails when the platform store cannot be created or the config dir
    /// cannot be resolved.
    #[cfg(any(windows, target_os = "macos", target_os = "ios"))]
    pub fn open() -> Result<Self> {
        install_store()?;
        Ok(Self {
            service: service()?,
            calls: Mutex::new(()),
            #[cfg(target_os = "ios")]
            moved: MovedHosts::open(crate::config_dir()?.join(POLICY_MARKER_FILE)),
        })
    }

    /// # Errors
    ///
    /// Always: the store is Windows Credential Manager, the macOS Keychain or
    /// the iOS Keychain.
    #[cfg(not(any(windows, target_os = "macos", target_os = "ios")))]
    pub fn open() -> Result<Self> {
        anyhow::bail!(
            "no credential store on this platform: device tokens need Windows Credential Manager, the macOS Keychain or the iOS Keychain"
        )
    }

    /// The credential for `host`; on iOS one written readable after the
    /// first unlock, on this device only.
    fn entry(&self, host: &str) -> Result<Entry> {
        #[cfg(target_os = "ios")]
        let entry = Entry::new_with_modifiers(
            &self.service,
            &user_for(host),
            &HashMap::from([("access-policy", IOS_ACCESS_POLICY)]),
        );
        #[cfg(not(target_os = "ios"))]
        let entry = Entry::new(&self.service, &user_for(host));
        entry.with_context(|| format!("opening the device token credential for {host}"))
    }

    /// The credential for `host` under the store's default policy, the one
    /// builds before the after-first-unlock policy wrote.
    #[cfg(target_os = "ios")]
    fn old_entry(&self, host: &str) -> Result<Entry> {
        Entry::new(&self.service, &user_for(host))
            .with_context(|| format!("opening the device token credential for {host}"))
    }

    /// The call lock, also after a thread panicked holding it: it guards no
    /// data, only the order of the calls.
    fn lock_calls(&self) -> MutexGuard<'_, ()> {
        self.calls.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The token held for `host`, as the item stands.
    fn read_token(&self, host: &str) -> Result<Option<String>> {
        match self.entry(host)?.get_password() {
            Ok(token) => Ok(Some(token)),
            Err(KeyringError::NoEntry) => Ok(None),
            Err(err) => Err(err).with_context(|| format!("reading the device token for {host}")),
        }
    }

    /// Save `token` for `host` through `entry`.
    fn write_token(entry: Result<Entry>, host: &str, token: &str) -> Result<()> {
        entry?
            .set_password(token)
            .with_context(|| format!("saving the device token for {host} to the credential store"))
    }

    /// Delete `host`'s item; holding none is not an error.
    fn remove_token(&self, host: &str) -> Result<()> {
        match self.entry(host)?.delete_credential() {
            Ok(()) | Err(KeyringError::NoEntry) => Ok(()),
            Err(err) => Err(err).with_context(|| format!("deleting the device token for {host}")),
        }
    }
}

/// An item's read ignores its policy and its delete matches service and
/// account only, so an item of any policy is read, deleted and saved anew.
/// Called with the call lock held.
#[cfg(target_os = "ios")]
impl PolicyMove for KeyringTokens {
    fn read(&self, host: &str) -> Result<Option<String>> {
        self.read_token(host)
    }

    fn delete_old(&self, host: &str) -> Result<()> {
        self.remove_token(host)
    }

    fn save_new(&self, host: &str, token: &str) -> Result<()> {
        Self::write_token(self.entry(host), host, token)
    }

    fn save_old(&self, host: &str, token: &str) -> Result<()> {
        Self::write_token(self.old_entry(host), host, token)
    }

    fn is_moved(&self, host: &str) -> bool {
        self.moved.lock().contains(&user_for(host))
    }

    fn record_moved(&self, host: &str) -> Result<()> {
        let mut users = self.moved.lock();
        users.insert(user_for(host));
        save_moved(&self.moved.path, &users)
    }
}

impl DeviceTokenStore for KeyringTokens {
    fn get(&self, host: &str) -> Result<Option<String>> {
        let _calls = self.lock_calls();
        #[cfg(target_os = "ios")]
        return read_and_move(self, host);
        #[cfg(not(target_os = "ios"))]
        self.read_token(host)
    }

    /// On iOS the save writes the new policy, so the host is recorded as
    /// moved and its next read moves nothing.
    fn set(&self, host: &str, token: &str) -> Result<()> {
        let _calls = self.lock_calls();
        #[cfg(target_os = "ios")]
        return save_and_record(self, host, token);
        #[cfg(not(target_os = "ios"))]
        Self::write_token(self.entry(host), host, token)
    }

    fn delete(&self, host: &str) -> Result<()> {
        let _calls = self.lock_calls();
        self.remove_token(host)
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
    use super::{
        DeviceTokenStore as _, MemoryTokens, POLICY_MARKER_FILE, PolicyMove, load_moved,
        read_and_move, save_and_record, save_moved, service_for, user_for,
    };
    use crate::test_fakes::ScratchDir;
    use std::cell::{Cell, RefCell};
    use std::collections::HashSet;
    use std::path::Path;

    /// A Keychain stand-in for [`read_and_move`] that records each call, in
    /// order, and fails its delete or its saves on request. A delete forgets
    /// the token and a save holds it again, as the Keychain's item does.
    #[derive(Default)]
    struct FakeKeychain {
        token: RefCell<Option<String>>,
        moved: RefCell<HashSet<String>>,
        fail_delete: Cell<bool>,
        /// Every new-policy save fails.
        fail_save: Cell<bool>,
        /// How many of the next new-policy saves fail.
        save_failures: Cell<u32>,
        fail_save_old: Cell<bool>,
        calls: RefCell<Vec<String>>,
    }

    impl FakeKeychain {
        fn holding(token: &'static str) -> Self {
            Self {
                token: RefCell::new(Some(token.to_owned())),
                ..Self::default()
            }
        }

        /// The token the item holds now.
        fn held(&self) -> Option<String> {
            self.token.borrow().clone()
        }

        fn call(&self, call: String) {
            self.calls.borrow_mut().push(call);
        }

        /// The calls made so far, and forget them.
        fn take_calls(&self) -> Vec<String> {
            self.calls.take()
        }
    }

    impl PolicyMove for FakeKeychain {
        fn read(&self, host: &str) -> anyhow::Result<Option<String>> {
            self.call(format!("get {host}"));
            Ok(self.held())
        }

        fn delete_old(&self, host: &str) -> anyhow::Result<()> {
            self.call(format!("delete {host}"));
            if self.fail_delete.get() {
                anyhow::bail!("the Keychain refused the delete");
            }
            self.token.replace(None);
            Ok(())
        }

        fn save_new(&self, host: &str, token: &str) -> anyhow::Result<()> {
            self.call(format!("set {host} {token}"));
            let failures = self.save_failures.get();
            if failures > 0 {
                self.save_failures.set(failures - 1);
                anyhow::bail!("the Keychain refused the save");
            }
            if self.fail_save.get() {
                anyhow::bail!("the Keychain refused the save");
            }
            self.token.replace(Some(token.to_owned()));
            Ok(())
        }

        fn save_old(&self, host: &str, token: &str) -> anyhow::Result<()> {
            self.call(format!("set-old {host} {token}"));
            if self.fail_save_old.get() {
                anyhow::bail!("the Keychain refused the old-policy save");
            }
            self.token.replace(Some(token.to_owned()));
            Ok(())
        }

        fn is_moved(&self, host: &str) -> bool {
            self.moved.borrow().contains(host)
        }

        fn record_moved(&self, host: &str) -> anyhow::Result<()> {
            self.call(format!("record {host}"));
            self.moved.borrow_mut().insert(host.to_owned());
            Ok(())
        }
    }

    #[test]
    fn a_first_read_moves_the_item_to_the_new_policy() {
        let keychain = FakeKeychain::holding("tok");
        let token = read_and_move(&keychain, "box").expect("reads");
        assert_eq!(token.as_deref(), Some("tok"));
        assert_eq!(
            keychain.take_calls(),
            ["get box", "delete box", "set box tok", "record box"],
            "the item is read, deleted, saved again with the same token, then recorded"
        );
    }

    #[test]
    fn a_recorded_host_is_not_moved_again() {
        let keychain = FakeKeychain::holding("tok");
        keychain.moved.borrow_mut().insert("box".to_owned());
        let token = read_and_move(&keychain, "box").expect("reads");
        assert_eq!(token.as_deref(), Some("tok"));
        assert_eq!(
            keychain.take_calls(),
            ["get box"],
            "a recorded host was moved again"
        );
    }

    #[test]
    fn a_failed_move_still_returns_the_token_and_is_retried_next_launch() {
        let keychain = FakeKeychain::holding("tok");
        keychain.fail_delete.set(true);
        let token = read_and_move(&keychain, "box").expect("reads");
        assert_eq!(
            token.as_deref(),
            Some("tok"),
            "a failed delete lost the token"
        );
        assert_eq!(keychain.take_calls(), ["get box", "delete box"]);
        assert!(!keychain.is_moved("box"), "a failed delete was recorded");

        keychain.fail_delete.set(false);
        keychain.fail_save.set(true);
        let token = read_and_move(&keychain, "box").expect("reads");
        assert_eq!(
            token.as_deref(),
            Some("tok"),
            "a failed save lost the token"
        );
        assert_eq!(
            keychain.take_calls(),
            [
                "get box",
                "delete box",
                "set box tok",
                "set box tok",
                "set-old box tok"
            ],
            "the move was not tried again after a failed delete"
        );
        assert!(!keychain.is_moved("box"), "a failed save was recorded");

        keychain.fail_save.set(false);
        read_and_move(&keychain, "box").expect("reads");
        assert_eq!(
            keychain.take_calls(),
            ["get box", "delete box", "set box tok", "record box"],
            "the move was not tried again after a failed save"
        );
    }

    #[test]
    fn a_failed_new_policy_save_falls_back_to_the_old_policy() {
        let keychain = FakeKeychain::holding("tok");
        keychain.save_failures.set(2);
        let token = read_and_move(&keychain, "box").expect("reads");
        assert_eq!(token.as_deref(), Some("tok"));
        assert_eq!(
            keychain.take_calls(),
            [
                "get box",
                "delete box",
                "set box tok",
                "set box tok",
                "set-old box tok"
            ],
            "two failed saves did not save the same token under the old policy"
        );
        assert!(!keychain.is_moved("box"), "a fallen-back save was recorded");
        assert_eq!(
            read_and_move(&keychain, "box").expect("reads").as_deref(),
            Some("tok"),
            "a later read lost the token"
        );

        let unsaved = FakeKeychain::holding("tok");
        unsaved.fail_save.set(true);
        unsaved.fail_save_old.set(true);
        assert_eq!(
            read_and_move(&unsaved, "box").expect("reads").as_deref(),
            Some("tok"),
            "a fallback that failed too did not return the token read"
        );
        assert!(!unsaved.is_moved("box"));
    }

    #[test]
    fn a_failed_new_policy_save_is_retried_once() {
        let keychain = FakeKeychain::holding("tok");
        keychain.save_failures.set(1);
        let token = read_and_move(&keychain, "box").expect("reads");
        assert_eq!(token.as_deref(), Some("tok"));
        assert_eq!(
            keychain.take_calls(),
            [
                "get box",
                "delete box",
                "set box tok",
                "set box tok",
                "record box"
            ],
            "a save that failed once was not retried and recorded"
        );
        assert_eq!(keychain.held().as_deref(), Some("tok"));
    }

    #[test]
    fn set_records_the_host_so_its_next_read_moves_nothing() {
        let keychain = FakeKeychain::default();
        save_and_record(&keychain, "box", "tok").expect("saves");
        let token = read_and_move(&keychain, "box").expect("reads");
        assert_eq!(token.as_deref(), Some("tok"));
        assert_eq!(
            keychain.take_calls(),
            ["set box tok", "record box", "get box"],
            "a saved token's next read moved it again"
        );
    }

    #[test]
    fn no_token_moves_nothing() {
        let keychain = FakeKeychain::default();
        assert_eq!(read_and_move(&keychain, "box").expect("reads"), None);
        assert_eq!(keychain.take_calls(), ["get box"]);
        assert!(!keychain.is_moved("box"));
    }

    #[test]
    fn the_marker_file_round_trips_hosts() {
        let scratch = ScratchDir::new("token-policy");
        let path = scratch.0.join("config").join(POLICY_MARKER_FILE);
        assert!(
            load_moved(&path).is_empty(),
            "a missing marker file lists no host"
        );

        let users: HashSet<String> = [user_for("Box.Tail1234.TS.net:8787"), user_for("other")]
            .into_iter()
            .collect();
        save_moved(&path, &users).expect("writes the marker file");
        assert_eq!(load_moved(&path), users, "the hosts read back");
        assert_eq!(
            std::fs::read_to_string(&path).expect("reads the marker file"),
            "device/box.tail1234.ts.net\ndevice/other\n",
            "one credential user per line, in order"
        );

        let fewer: HashSet<String> = [user_for("other")].into_iter().collect();
        save_moved(&path, &fewer).expect("replaces the marker file");
        assert_eq!(load_moved(&path), fewer, "a save replaces the earlier list");
    }

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
