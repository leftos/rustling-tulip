//! One Web Push subscription per device, kept in `push/subscriptions.json`
//! keyed by the device id `devices.json` records, so a re-subscribe from the
//! same device replaces its endpoint and `devices.json` is never rewritten.

use std::collections::{BTreeMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, anyhow};
use serde::{Deserialize, Serialize};

/// The subscriptions file's name inside the push folder.
pub(crate) const SUBSCRIPTIONS_FILE: &str = "subscriptions.json";

/// One device's subscription and the two switches the phone app sets. Its
/// `Debug` redacts the endpoint (a capability URL) and both keys, `auth`
/// being the RFC 8291 secret, so no log carries them.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Subscription {
    /// The push service's kind; `web_push` is the only one written today.
    pub kind: String,
    /// The push service endpoint the POST goes to.
    pub endpoint: String,
    /// The device's public key, base64url (RFC 8291 `p256dh`).
    pub p256dh: String,
    /// The device's auth secret, base64url (RFC 8291 `auth`).
    pub auth: String,
    /// The device's Needs you switch.
    pub needs_you: bool,
    /// The device's Finished switch.
    pub finished: bool,
    /// When the subscription was stored, RFC 3339 UTC; the caller reads the
    /// clock, never this store.
    pub created_at: String,
}

impl std::fmt::Debug for Subscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        const REDACTED: &str = "<redacted>";
        f.debug_struct("Subscription")
            .field("kind", &self.kind)
            .field("endpoint", &REDACTED)
            .field("p256dh", &REDACTED)
            .field("auth", &REDACTED)
            .field("needs_you", &self.needs_you)
            .field("finished", &self.finished)
            .field("created_at", &self.created_at)
            .finish()
    }
}

/// `subscriptions.json`: the subscriptions by device id.
#[derive(Serialize, Deserialize)]
struct SubscriptionsFile {
    devices: BTreeMap<String, Subscription>,
}

/// The subscriptions, loaded from and saved to `subscriptions.json`.
pub(crate) struct SubscriptionStore {
    path: PathBuf,
    devices: BTreeMap<String, Subscription>,
    /// The file could not be read or decoded: nothing is served and nothing is
    /// written over it.
    corrupt: bool,
}

impl SubscriptionStore {
    /// Load the store at `path`.
    ///
    /// An absent file is an empty store that will be created on the first
    /// write. Any other read failure, and any undecodable file, leaves a store
    /// with no subscriptions that refuses to save, so a file that exists but
    /// cannot be read is preserved instead of being overwritten.
    #[must_use]
    pub(crate) fn load(path: &Path) -> Self {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Self {
                    path: path.to_path_buf(),
                    devices: BTreeMap::new(),
                    corrupt: false,
                };
            }
            Err(err) => {
                tracing::warn!(
                    ?err,
                    path = %path.display(),
                    "could not read subscriptions.json; no device has a push subscription"
                );
                return Self::unusable(path);
            }
        };
        match serde_json::from_slice::<SubscriptionsFile>(&bytes) {
            Ok(file) => Self {
                path: path.to_path_buf(),
                devices: file.devices,
                corrupt: false,
            },
            Err(err) => {
                tracing::warn!(
                    ?err,
                    path = %path.display(),
                    "subscriptions.json is corrupt; no device has a push subscription and the file is left as it is"
                );
                Self::unusable(path)
            }
        }
    }

    fn unusable(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            devices: BTreeMap::new(),
            corrupt: true,
        }
    }

    /// Whether the file could not be read or decoded.
    #[must_use]
    pub(crate) fn is_corrupt(&self) -> bool {
        self.corrupt
    }

    /// The subscription stored for `device_id`.
    #[must_use]
    pub(crate) fn get(&self, device_id: &str) -> Option<&Subscription> {
        self.devices.get(device_id)
    }

    /// One subscription per device: `subscription` replaces whatever the
    /// device had, then the file is saved.
    ///
    /// # Errors
    ///
    /// The store is corrupt, or saving the file failed.
    pub(crate) fn put(
        &mut self,
        device_id: &str,
        subscription: Subscription,
    ) -> anyhow::Result<()> {
        let mut devices = self.devices.clone();
        devices.insert(device_id.to_owned(), subscription);
        self.save(&devices)?;
        self.devices = devices;
        Ok(())
    }

    /// Delete the subscription stored for `device_id` and save; `Ok(false)`
    /// when the device had none.
    ///
    /// # Errors
    ///
    /// The store is corrupt, or saving the file failed.
    pub(crate) fn remove(&mut self, device_id: &str) -> anyhow::Result<bool> {
        let mut devices = self.devices.clone();
        if devices.remove(device_id).is_none() {
            return Ok(false);
        }
        self.save(&devices)?;
        self.devices = devices;
        Ok(true)
    }

    /// Delete `device_id`'s subscription only while its endpoint is still
    /// `endpoint`, and save; `Ok(false)` when the device has none or has
    /// since registered another endpoint, so a push service's late "gone"
    /// for the old endpoint never deletes the new one.
    ///
    /// # Errors
    ///
    /// The store is corrupt, or saving the file failed.
    pub(crate) fn remove_if_endpoint(
        &mut self,
        device_id: &str,
        endpoint: &str,
    ) -> anyhow::Result<bool> {
        if self
            .devices
            .get(device_id)
            .is_none_or(|stored| stored.endpoint != endpoint)
        {
            return Ok(false);
        }
        self.remove(device_id)
    }

    /// Drop every subscription whose device id is not in `known`, save when
    /// any was dropped, and return how many.
    ///
    /// # Errors
    ///
    /// The store is corrupt, or saving the file failed.
    pub(crate) fn retain_devices(&mut self, known: &HashSet<String>) -> anyhow::Result<usize> {
        let mut devices = self.devices.clone();
        let before = devices.len();
        devices.retain(|device_id, _| known.contains(device_id));
        let dropped = before - devices.len();
        if dropped > 0 {
            self.save(&devices)?;
            self.devices = devices;
        }
        Ok(dropped)
    }

    /// Every subscription, by device id.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&str, &Subscription)> {
        self.devices
            .iter()
            .map(|(device_id, subscription)| (device_id.as_str(), subscription))
    }

    fn refuse_if_corrupt(&self) -> anyhow::Result<()> {
        if self.corrupt {
            return Err(anyhow!(
                "{} is corrupt; the file is left as it is",
                self.path.display()
            ));
        }
        Ok(())
    }

    fn save(&self, devices: &BTreeMap<String, Subscription>) -> anyhow::Result<()> {
        self.refuse_if_corrupt()?;
        let bytes = serde_json::to_vec_pretty(&SubscriptionsFile {
            devices: devices.clone(),
        })
        .with_context(|| format!("serializing {}", self.path.display()))?;
        crate::atomic_file::write_atomic(&self.path, &bytes)
            .with_context(|| format!("writing {}", self.path.display()))
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert preconditions with expect and panic"
)]
mod tests {
    use std::collections::HashSet;
    use std::path::PathBuf;

    use super::{SUBSCRIPTIONS_FILE as FILE, Subscription, SubscriptionStore};

    /// A scratch push folder holding one `subscriptions.json`, removed on
    /// drop.
    struct Scratch {
        dir: PathBuf,
    }

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("rt-push-store-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create scratch dir");
            Self { dir }
        }

        fn file(&self) -> PathBuf {
            self.dir.join(FILE)
        }

        fn store(&self) -> SubscriptionStore {
            SubscriptionStore::load(&self.file())
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn subscription(endpoint: &str, needs_you: bool, finished: bool) -> Subscription {
        Subscription {
            kind: "web_push".to_owned(),
            endpoint: endpoint.to_owned(),
            p256dh: "p256dh-key".to_owned(),
            auth: "auth-secret".to_owned(),
            needs_you,
            finished,
            created_at: "2026-11-07T00:00:00Z".to_owned(),
        }
    }

    fn ids(store: &SubscriptionStore) -> Vec<String> {
        store.iter().map(|(id, _)| id.to_owned()).collect()
    }

    #[test]
    fn a_put_subscription_survives_a_reload() {
        let scratch = Scratch::new("reload");
        let mut store = scratch.store();
        store
            .put(
                "dev-1",
                subscription("https://push.example/one", true, false),
            )
            .expect("store the subscription");

        let reloaded = scratch.store();

        let stored = reloaded.get("dev-1").expect("the device's subscription");
        assert_eq!(stored.endpoint, "https://push.example/one");
        assert_eq!(stored.kind, "web_push");
        assert!(stored.needs_you);
        assert!(!stored.finished);
    }

    #[test]
    fn a_second_put_for_one_device_replaces_its_endpoint() {
        let scratch = Scratch::new("replace");
        let mut store = scratch.store();
        store
            .put(
                "dev-1",
                subscription("https://push.example/one", true, false),
            )
            .expect("store the first subscription");
        store
            .put(
                "dev-1",
                subscription("https://push.example/two", false, true),
            )
            .expect("store the replacement");

        let reloaded = scratch.store();

        assert_eq!(
            ids(&reloaded),
            vec!["dev-1".to_owned()],
            "one entry per device"
        );
        let stored = reloaded.get("dev-1").expect("the device's subscription");
        assert_eq!(stored.endpoint, "https://push.example/two");
        assert!(
            stored.finished,
            "the switches come from the replacement too"
        );
    }

    #[test]
    fn two_devices_keep_separate_switches() {
        let scratch = Scratch::new("switches");
        let mut store = scratch.store();
        store
            .put(
                "dev-1",
                subscription("https://push.example/one", true, false),
            )
            .expect("store the phone's subscription");
        store
            .put(
                "dev-2",
                subscription("https://push.example/two", false, true),
            )
            .expect("store the second device's subscription");

        let reloaded = scratch.store();

        assert_eq!(ids(&reloaded), vec!["dev-1".to_owned(), "dev-2".to_owned()]);
        let phone = reloaded.get("dev-1").expect("the phone's subscription");
        assert!(phone.needs_you);
        assert!(!phone.finished);
        let other = reloaded
            .get("dev-2")
            .expect("the second device's subscription");
        assert!(!other.needs_you);
        assert!(other.finished);
    }

    #[test]
    fn remove_deletes_the_subscription_and_saves() {
        let scratch = Scratch::new("remove");
        let mut store = scratch.store();
        store
            .put(
                "dev-1",
                subscription("https://push.example/one", true, false),
            )
            .expect("store the subscription");

        assert!(store.remove("dev-1").expect("remove it"));
        assert!(
            scratch.store().get("dev-1").is_none(),
            "the removal is saved"
        );
        assert!(
            !store.remove("dev-1").expect("remove again"),
            "a second removal has nothing to delete"
        );
    }

    #[test]
    fn remove_if_endpoint_deletes_the_endpoint_that_failed() {
        let scratch = Scratch::new("remove-if-endpoint");
        let mut store = scratch.store();
        store
            .put(
                "dev-1",
                subscription("https://push.example/one", true, false),
            )
            .expect("store the subscription");

        assert!(
            store
                .remove_if_endpoint("dev-1", "https://push.example/one")
                .expect("remove it")
        );
        assert!(
            scratch.store().get("dev-1").is_none(),
            "the removal is saved"
        );
        assert!(
            !store
                .remove_if_endpoint("dev-1", "https://push.example/one")
                .expect("remove again"),
            "a second removal has nothing to delete"
        );
    }

    #[test]
    fn remove_if_endpoint_keeps_a_re_registered_endpoint() {
        let scratch = Scratch::new("remove-if-endpoint-stale");
        let mut store = scratch.store();
        store
            .put(
                "dev-1",
                subscription("https://push.example/two", true, false),
            )
            .expect("store the re-registered subscription");

        assert!(
            !store
                .remove_if_endpoint("dev-1", "https://push.example/one")
                .expect("compare"),
            "a stale endpoint deletes nothing"
        );
        let kept = scratch.store();
        assert_eq!(
            kept.get("dev-1").expect("still stored").endpoint,
            "https://push.example/two"
        );
        assert!(
            !store
                .remove_if_endpoint("dev-2", "https://push.example/two")
                .expect("compare"),
            "a device with no subscription deletes nothing"
        );
    }

    #[test]
    fn retain_devices_drops_an_orphan_and_keeps_a_known_device() {
        let scratch = Scratch::new("retain");
        let mut store = scratch.store();
        store
            .put(
                "dev-1",
                subscription("https://push.example/one", true, false),
            )
            .expect("store the known device's subscription");
        store
            .put(
                "dev-2",
                subscription("https://push.example/two", true, false),
            )
            .expect("store the orphan's subscription");
        let known: HashSet<String> = ["dev-1".to_owned()].into_iter().collect();

        assert_eq!(
            store.retain_devices(&known).expect("drop the orphan"),
            1,
            "one subscription is dropped"
        );

        let reloaded = scratch.store();
        assert!(reloaded.get("dev-1").is_some(), "the known device stays");
        assert!(
            reloaded.get("dev-2").is_none(),
            "the dropped subscription stays dropped after a reload"
        );
        assert_eq!(
            store.retain_devices(&known).expect("nothing left to drop"),
            0
        );
    }

    #[test]
    fn a_failed_save_leaves_the_store_as_it_was() {
        let scratch = Scratch::new("save-failure");
        let mut store = scratch.store();
        store
            .put(
                "dev-1",
                subscription("https://push.example/one", true, false),
            )
            .expect("store the subscription");
        // A folder where the file belongs makes every save fail.
        std::fs::remove_file(scratch.file()).expect("remove the file");
        std::fs::create_dir(scratch.file()).expect("make a folder in its place");

        let Err(err) = store.put(
            "dev-1",
            subscription("https://push.example/two", false, true),
        ) else {
            panic!("a save that cannot write the file is an error");
        };
        assert!(
            !format!("{err:#}").is_empty(),
            "the error says why the save failed"
        );
        let kept = store
            .get("dev-1")
            .expect("the earlier subscription is still served");
        assert_eq!(kept.endpoint, "https://push.example/one");

        let Err(_) = store.put(
            "dev-2",
            subscription("https://push.example/two", true, false),
        ) else {
            panic!("a save that cannot write the file is an error");
        };
        assert!(
            store.get("dev-2").is_none(),
            "a new device's failed put leaves nothing behind"
        );
    }

    #[test]
    fn debug_shows_no_endpoint_and_no_key() {
        let mut stored = subscription("https://web.push.apple.com/send/capability", true, false);
        stored.p256dh = "BPublicKeyValue".to_owned();
        stored.auth = "AuthSecretValue".to_owned();

        let shown = format!("{stored:?}");

        for secret in ["send/capability", "BPublicKeyValue", "AuthSecretValue"] {
            assert!(!shown.contains(secret), "{shown} shows {secret}");
        }
        assert!(shown.contains("needs_you: true"), "{shown}");
    }

    #[test]
    fn a_corrupt_file_loads_empty_and_refuses_to_save() {
        let scratch = Scratch::new("corrupt");
        let corrupted = b"{ not the subscriptions file";
        std::fs::write(scratch.file(), corrupted).expect("write a corrupt subscriptions.json");

        let mut store = scratch.store();

        assert!(store.is_corrupt(), "the undecodable file is flagged");
        assert_eq!(ids(&store), Vec::<String>::new(), "nothing is served");
        let Err(err) = store.put(
            "dev-1",
            subscription("https://push.example/one", true, false),
        ) else {
            panic!("a corrupt store refuses to save");
        };
        assert!(
            format!("{err:#}").contains(&scratch.file().display().to_string()),
            "the error names the file"
        );
        assert_eq!(
            std::fs::read(scratch.file()).expect("read the file back"),
            corrupted,
            "the corrupt file is never overwritten"
        );
    }
}
