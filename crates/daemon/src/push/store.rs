//! One push subscription per device, kept in `push/subscriptions.json`
//! keyed by the device id `devices.json` records, so a re-subscribe from the
//! same device replaces its channel and `devices.json` is never rewritten.

use std::collections::{BTreeMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, anyhow};
use serde::{Deserialize, Serialize};

/// The subscriptions file's name inside the push folder.
pub(crate) const SUBSCRIPTIONS_FILE: &str = "subscriptions.json";

/// A subscription written without the at-desk hold switch is held.
fn default_true() -> bool {
    true
}

/// The APNs environment a stored token belongs to. Only the two values APNs
/// has are read, so an entry naming anything else fails that entry's decode
/// and is kept raw rather than rewritten as an unknown.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ApnsEnvironment {
    /// The App Store's push service.
    Production,
    /// APNs' sandbox service, which a development build's token names.
    Sandbox,
}

impl std::fmt::Debug for ApnsEnvironment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Production => "production",
            Self::Sandbox => "sandbox",
        })
    }
}

/// Where a subscription's pushes go, written flattened beside its switches
/// and tagged by `kind`: a Web Push endpoint, or the token the push relay
/// addresses. Its `Debug` redacts the address and both keys, `auth` being
/// the RFC 8291 secret, so no log carries them.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Channel {
    /// A Web Push endpoint in the phone app's service worker.
    WebPush {
        /// The push service endpoint the POST goes to.
        endpoint: String,
        /// The device's public key, base64url (RFC 8291 `p256dh`).
        p256dh: String,
        /// The device's auth secret, base64url (RFC 8291 `auth`).
        auth: String,
    },
    /// An APNs device token, sent through the push relay.
    Apns {
        /// The device token the relay addresses.
        token: String,
        /// Which APNs environment the token belongs to.
        environment: ApnsEnvironment,
        /// The device's public key, base64url (RFC 8291 `p256dh`).
        p256dh: String,
        /// The device's auth secret, base64url (RFC 8291 `auth`).
        auth: String,
    },
    /// An FCM registration token, sent through the push relay.
    Fcm {
        /// The registration token the relay addresses.
        token: String,
        /// The device's public key, base64url (RFC 8291 `p256dh`).
        p256dh: String,
        /// The device's auth secret, base64url (RFC 8291 `auth`).
        auth: String,
    },
}

impl Channel {
    /// The kind as `subscriptions.json` writes it.
    #[must_use]
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::WebPush { .. } => "web_push",
            Self::Apns { .. } => "apns",
            Self::Fcm { .. } => "fcm",
        }
    }

    /// The address the subscription is reached at: the endpoint, or the
    /// token. It is what a late answer for a dropped subscription is
    /// compared against, never a secret the log may show.
    #[must_use]
    pub(crate) fn address(&self) -> &str {
        match self {
            Self::WebPush { endpoint, .. } => endpoint,
            Self::Apns { token, .. } | Self::Fcm { token, .. } => token,
        }
    }
}

impl std::fmt::Debug for Channel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        const REDACTED: &str = "<redacted>";
        match self {
            Self::WebPush { .. } => f
                .debug_struct("WebPush")
                .field("endpoint", &REDACTED)
                .field("p256dh", &REDACTED)
                .field("auth", &REDACTED)
                .finish(),
            Self::Apns { environment, .. } => f
                .debug_struct("Apns")
                .field("token", &REDACTED)
                .field("environment", environment)
                .field("p256dh", &REDACTED)
                .field("auth", &REDACTED)
                .finish(),
            Self::Fcm { .. } => f
                .debug_struct("Fcm")
                .field("token", &REDACTED)
                .field("p256dh", &REDACTED)
                .field("auth", &REDACTED)
                .finish(),
        }
    }
}

/// One device's subscription: where its pushes go, and the two switches the
/// phone app sets. Its `Debug` redacts the channel's address and keys, so no
/// log carries them.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Subscription {
    /// Where the device's pushes go, written beside the switches.
    #[serde(flatten)]
    pub channel: Channel,
    /// The device's Needs you switch.
    pub needs_you: bool,
    /// The device's Finished switch.
    pub finished: bool,
    /// Whether this subscription's pushes are held while the owner is at the
    /// desk; on unless the phone turns it off.
    #[serde(default = "default_true")]
    pub hold_at_desk: bool,
    /// When the subscription was stored, RFC 3339 UTC; the caller reads the
    /// clock, never this store.
    pub created_at: String,
}

impl std::fmt::Debug for Subscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscription")
            .field("channel", &self.channel)
            .field("needs_you", &self.needs_you)
            .field("finished", &self.finished)
            .field("hold_at_desk", &self.hold_at_desk)
            .field("created_at", &self.created_at)
            .finish()
    }
}

/// One device's entry in `subscriptions.json`: the subscription this build
/// reads, or the raw JSON of one it cannot (a newer build's channel kind or
/// environment), kept so a save writes it back as it was read.
#[derive(Clone)]
enum Entry {
    Known(Subscription),
    Raw(serde_json::Value),
}

impl Entry {
    fn known(&self) -> Option<&Subscription> {
        match self {
            Self::Known(subscription) => Some(subscription),
            Self::Raw(_) => None,
        }
    }

    fn to_value(&self) -> anyhow::Result<serde_json::Value> {
        match self {
            Self::Known(subscription) => {
                serde_json::to_value(subscription).context("serializing a subscription")
            }
            Self::Raw(value) => Ok(value.clone()),
        }
    }
}

/// The `kind` an entry this build could not decode names, for the load
/// warning; values are never logged.
fn entry_kind(value: &serde_json::Value) -> &str {
    value
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("<absent>")
}

/// Decode one device's entry, keeping a value this build cannot read as it
/// was with a warning that names the device and its `kind` only.
fn decode_entry(device_id: &str, value: serde_json::Value) -> Entry {
    let Ok(subscription) = serde_json::from_value::<Subscription>(value.clone()) else {
        tracing::warn!(
            %device_id,
            kind = entry_kind(&value),
            "push: subscriptions.json holds an entry this build cannot read; it is kept as it is and served no pushes"
        );
        return Entry::Raw(value);
    };
    Entry::Known(subscription)
}

/// `subscriptions.json`: each device's entry by device id. The values are
/// left undecoded here, so one entry this build cannot read neither loses
/// the rest nor gets rewritten.
#[derive(Serialize, Deserialize)]
struct SubscriptionsFile {
    devices: BTreeMap<String, serde_json::Value>,
}

/// The subscriptions, loaded from and saved to `subscriptions.json`.
pub(crate) struct SubscriptionStore {
    path: PathBuf,
    devices: BTreeMap<String, Entry>,
    /// The file could not be read or decoded: nothing is served and nothing is
    /// written over it.
    corrupt: bool,
}

impl SubscriptionStore {
    /// Load the store at `path`.
    ///
    /// An absent file is an empty store that will be created on the first
    /// write. Any other read failure, and any file that is not a JSON object
    /// of device entries, leaves a store with no subscriptions that refuses
    /// to save, so a file that exists but cannot be read is preserved instead
    /// of being overwritten. Each device's entry is decoded on its own: one
    /// this build cannot read is kept as it was read, served no pushes, and
    /// written back unchanged on every save.
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
                devices: file
                    .devices
                    .into_iter()
                    .map(|(device_id, value)| {
                        let entry = decode_entry(&device_id, value);
                        (device_id, entry)
                    })
                    .collect(),
                corrupt: false,
            },
            Err(err) => {
                tracing::warn!(
                    at = %crate::logging::json_error_at(&err),
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
        self.devices.get(device_id).and_then(Entry::known)
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
        devices.insert(device_id.to_owned(), Entry::Known(subscription));
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

    /// Delete `device_id`'s subscription only while its address is still
    /// `address`, and save; `Ok(false)` when the device has none or has
    /// since registered another address, so a push service's late "gone" for
    /// the old endpoint or token never deletes the new one.
    ///
    /// # Errors
    ///
    /// The store is corrupt, or saving the file failed.
    pub(crate) fn remove_if_address(
        &mut self,
        device_id: &str,
        address: &str,
    ) -> anyhow::Result<bool> {
        if self
            .devices
            .get(device_id)
            .and_then(Entry::known)
            .is_none_or(|stored| stored.channel.address() != address)
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

    /// Every subscription this build can read, by device id; a device whose
    /// entry it cannot read is left out.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&str, &Subscription)> {
        self.devices.iter().filter_map(|(device_id, entry)| {
            entry
                .known()
                .map(|subscription| (device_id.as_str(), subscription))
        })
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

    fn save(&self, devices: &BTreeMap<String, Entry>) -> anyhow::Result<()> {
        self.refuse_if_corrupt()?;
        let devices = devices
            .iter()
            .map(|(device_id, entry)| Ok((device_id.clone(), entry.to_value()?)))
            .collect::<anyhow::Result<BTreeMap<String, serde_json::Value>>>()
            .with_context(|| format!("serializing {}", self.path.display()))?;
        let bytes = serde_json::to_vec_pretty(&SubscriptionsFile { devices })
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

    use super::{
        ApnsEnvironment, Channel, SUBSCRIPTIONS_FILE as FILE, Subscription, SubscriptionStore,
    };

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
            channel: Channel::WebPush {
                endpoint: endpoint.to_owned(),
                p256dh: "p256dh-key".to_owned(),
                auth: "auth-secret".to_owned(),
            },
            needs_you,
            finished,
            hold_at_desk: true,
            created_at: "2026-11-07T00:00:00Z".to_owned(),
        }
    }

    fn apns(token: &str, environment: ApnsEnvironment) -> Subscription {
        Subscription {
            channel: Channel::Apns {
                token: token.to_owned(),
                environment,
                p256dh: "p256dh-key".to_owned(),
                auth: "auth-secret".to_owned(),
            },
            needs_you: true,
            finished: false,
            hold_at_desk: true,
            created_at: "2026-11-07T00:00:00Z".to_owned(),
        }
    }

    fn fcm(token: &str) -> Subscription {
        Subscription {
            channel: Channel::Fcm {
                token: token.to_owned(),
                p256dh: "p256dh-key".to_owned(),
                auth: "auth-secret".to_owned(),
            },
            needs_you: true,
            finished: false,
            hold_at_desk: true,
            created_at: "2026-11-07T00:00:00Z".to_owned(),
        }
    }

    fn ids(store: &SubscriptionStore) -> Vec<String> {
        store.iter().map(|(id, _)| id.to_owned()).collect()
    }

    #[test]
    fn damaged_subscriptions_log_their_position_not_their_values() {
        use tracing_subscriber::layer::SubscriberExt;

        let scratch = Scratch::new("damaged-position");
        std::fs::write(scratch.file(), br#"{"devices":"sk-live-123"}"#)
            .expect("seed a damaged subscriptions.json");
        let capture = crate::logging::tests::Capture::default();
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_writer(capture.clone())
                .with_ansi(false),
        );
        tracing::subscriber::with_default(subscriber, || {
            let store = scratch.store();
            assert!(
                store.is_corrupt(),
                "a damaged subscriptions.json reads as unusable"
            );
        });
        let text = capture.text();
        assert!(
            !text.contains("sk-live-123"),
            "the log names where the file stops parsing, not the value: {text}"
        );
        assert!(
            text.contains("line 1 column"),
            "the log names the position: {text}"
        );
    }

    #[test]
    fn a_subscription_without_hold_at_desk_reads_true() {
        let scratch = Scratch::new("hold-default");
        std::fs::write(
            scratch.file(),
            r#"{"devices":{"dev-1":{"kind":"web_push","endpoint":"https://push.example/one","p256dh":"p256dh-key","auth":"auth-secret","needs_you":true,"finished":false,"created_at":"2026-11-07T00:00:00Z"}}}"#,
        )
        .expect("write the subscriptions file");

        let store = scratch.store();

        let stored = store.get("dev-1").expect("the device's subscription");
        assert!(
            stored.hold_at_desk,
            "a subscription written without the switch is held at the desk"
        );
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
        assert_eq!(stored.channel.address(), "https://push.example/one");
        assert_eq!(stored.channel.kind(), "web_push");
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
        assert_eq!(stored.channel.address(), "https://push.example/two");
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
    fn remove_if_address_deletes_the_address_that_failed() {
        let scratch = Scratch::new("remove-if-address");
        let mut store = scratch.store();
        store
            .put(
                "dev-1",
                subscription("https://push.example/one", true, false),
            )
            .expect("store the subscription");

        assert!(
            store
                .remove_if_address("dev-1", "https://push.example/one")
                .expect("remove it")
        );
        assert!(
            scratch.store().get("dev-1").is_none(),
            "the removal is saved"
        );
        assert!(
            !store
                .remove_if_address("dev-1", "https://push.example/one")
                .expect("remove again"),
            "a second removal has nothing to delete"
        );
    }

    #[test]
    fn remove_if_address_keeps_a_re_registered_address() {
        let scratch = Scratch::new("remove-if-address-stale");
        let mut store = scratch.store();
        store
            .put(
                "dev-1",
                subscription("https://push.example/two", true, false),
            )
            .expect("store the re-registered subscription");

        assert!(
            !store
                .remove_if_address("dev-1", "https://push.example/one")
                .expect("compare"),
            "a stale address deletes nothing"
        );
        let kept = scratch.store();
        assert_eq!(
            kept.get("dev-1").expect("still stored").channel.address(),
            "https://push.example/two"
        );
        assert!(
            !store
                .remove_if_address("dev-2", "https://push.example/two")
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
        assert_eq!(kept.channel.address(), "https://push.example/one");

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
        let mut stored = subscription("https://web.push.apple.com/one", true, false);
        stored.channel = Channel::WebPush {
            endpoint: "https://web.push.apple.com/send/capability".to_owned(),
            p256dh: "BPublicKeyValue".to_owned(),
            auth: "AuthSecretValue".to_owned(),
        };

        let shown = format!("{stored:?}");

        for secret in ["send/capability", "BPublicKeyValue", "AuthSecretValue"] {
            assert!(!shown.contains(secret), "{shown} shows {secret}");
        }
        assert!(shown.contains("needs_you: true"), "{shown}");
    }

    #[test]
    fn a_v0_12_web_push_file_loads_and_saves_back_in_its_shape() {
        let scratch = Scratch::new("v0-12-shape");
        let released = serde_json::json!({
            "kind": "web_push",
            "endpoint": "https://web.push.apple.com/x",
            "p256dh": "BK",
            "auth": "au",
            "needs_you": true,
            "finished": false,
            "hold_at_desk": true,
            "created_at": "2026-10-01T00:00:00Z",
        });
        std::fs::write(
            scratch.file(),
            serde_json::json!({ "devices": { "d1": released.clone() } }).to_string(),
        )
        .expect("write a released subscriptions file");

        let mut store = scratch.store();

        let stored = store.get("d1").expect("the device's subscription");
        assert!(
            !store.is_corrupt(),
            "a file the released daemon wrote is not corrupt"
        );
        assert_eq!(stored.channel.kind(), "web_push");
        assert_eq!(stored.channel.address(), "https://web.push.apple.com/x");
        assert_eq!(
            stored.channel,
            Channel::WebPush {
                endpoint: "https://web.push.apple.com/x".to_owned(),
                p256dh: "BK".to_owned(),
                auth: "au".to_owned(),
            }
        );
        assert!(stored.needs_you);
        assert!(!stored.finished);
        assert!(stored.hold_at_desk);
        assert_eq!(stored.created_at, "2026-10-01T00:00:00Z");

        store
            .put("d2", subscription("https://push.example/two", true, false))
            .expect("store another device's subscription");

        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(scratch.file()).expect("read the file back"))
                .expect("the saved file is JSON");
        assert_eq!(
            saved["devices"]["d1"], released,
            "a released entry is written back in its own shape"
        );
    }

    #[test]
    fn an_apns_subscription_round_trips() {
        let scratch = Scratch::new("apns-round-trip");
        let mut store = scratch.store();
        store
            .put("dev-1", apns("aa11bb22", ApnsEnvironment::Sandbox))
            .expect("store the apns subscription");

        let reloaded = scratch.store();

        let stored = reloaded.get("dev-1").expect("the device's subscription");
        assert_eq!(stored.channel.kind(), "apns");
        assert_eq!(stored.channel.address(), "aa11bb22");
        assert_eq!(
            stored.channel,
            Channel::Apns {
                token: "aa11bb22".to_owned(),
                environment: ApnsEnvironment::Sandbox,
                p256dh: "p256dh-key".to_owned(),
                auth: "auth-secret".to_owned(),
            }
        );

        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(scratch.file()).expect("read the file back"))
                .expect("the saved file is JSON");
        let entry = &saved["devices"]["dev-1"];
        assert_eq!(entry["kind"], "apns", "{entry}");
        assert_eq!(entry["token"], "aa11bb22", "{entry}");
        assert_eq!(entry["environment"], "sandbox", "{entry}");
        assert!(
            entry.get("channel").is_none(),
            "the channel is flattened: {entry}"
        );
    }

    #[test]
    fn an_fcm_subscription_round_trips() {
        let scratch = Scratch::new("fcm-round-trip");
        let mut store = scratch.store();
        store
            .put("dev-1", fcm("fcm-token"))
            .expect("store the fcm subscription");

        let stored = scratch.store();
        let stored = stored.get("dev-1").expect("the device's subscription");
        assert_eq!(stored.channel.kind(), "fcm");
        assert_eq!(stored.channel.address(), "fcm-token");

        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(scratch.file()).expect("read the file back"))
                .expect("the saved file is JSON");
        let entry = &saved["devices"]["dev-1"];
        assert_eq!(entry["kind"], "fcm", "{entry}");
        assert_eq!(entry["token"], "fcm-token", "{entry}");
        assert!(entry.get("channel").is_none(), "{entry}");
    }

    #[test]
    fn an_entry_from_a_newer_build_is_kept_and_written_back() {
        let scratch = Scratch::new("newer-entry");
        let web_push = serde_json::json!({
            "kind": "web_push",
            "endpoint": "https://web.push.apple.com/x",
            "p256dh": "BK",
            "auth": "au",
            "needs_you": true,
            "finished": false,
            "hold_at_desk": true,
            "created_at": "2026-10-01T00:00:00Z",
        });
        let pigeon = serde_json::json!({
            "kind": "carrier_pigeon",
            "perch": "north",
            "needs_you": true,
            "finished": false,
            "created_at": "2026-10-01T00:00:00Z",
        });
        let staging = serde_json::json!({
            "kind": "apns",
            "token": "aa11bb22",
            "environment": "staging",
            "p256dh": "BK",
            "auth": "au",
            "needs_you": true,
            "finished": false,
            "hold_at_desk": true,
            "created_at": "2026-10-01T00:00:00Z",
        });
        std::fs::write(
            scratch.file(),
            serde_json::json!({
                "devices": { "d1": web_push, "d2": pigeon.clone(), "d3": staging.clone() },
            })
            .to_string(),
        )
        .expect("write a newer build's subscriptions file");

        let mut store = scratch.store();

        assert!(
            !store.is_corrupt(),
            "one entry this build cannot read does not make the file corrupt"
        );
        assert_eq!(
            ids(&store),
            vec!["d1".to_owned()],
            "only the entry this build reads is served"
        );
        assert!(
            store.get("d2").is_none(),
            "a device whose entry this build cannot read is served no pushes"
        );

        store
            .put("d4", subscription("https://push.example/two", true, false))
            .expect("store another device's subscription");

        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(scratch.file()).expect("read the file back"))
                .expect("the saved file is JSON");
        assert_eq!(
            saved["devices"]["d2"], pigeon,
            "an unknown kind is written back as it was read"
        );
        assert_eq!(
            saved["devices"]["d3"], staging,
            "an unknown environment is written back as it was read"
        );
    }

    #[test]
    fn a_late_gone_for_an_old_token_never_deletes_a_new_one() {
        let scratch = Scratch::new("late-gone-token");
        let mut store = scratch.store();
        store
            .put("dev-1", apns("aa", ApnsEnvironment::Production))
            .expect("store the old token");
        store
            .put("dev-1", apns("bb", ApnsEnvironment::Production))
            .expect("store the replacement token");

        assert!(
            !store.remove_if_address("dev-1", "aa").expect("compare"),
            "a late answer for the old token deletes nothing"
        );
        assert_eq!(
            scratch
                .store()
                .get("dev-1")
                .expect("the replacement is still stored")
                .channel
                .address(),
            "bb",
            "the replacement token stays"
        );
        assert!(
            store.remove_if_address("dev-1", "bb").expect("remove it"),
            "the current token's answer deletes the subscription"
        );
        assert!(
            scratch.store().get("dev-1").is_none(),
            "the removal is saved"
        );
    }

    #[test]
    fn debug_shows_no_token() {
        let held = Subscription {
            channel: Channel::Apns {
                token: "TokenValue".to_owned(),
                environment: ApnsEnvironment::Sandbox,
                p256dh: "BPublicKeyValue".to_owned(),
                auth: "AuthSecretValue".to_owned(),
            },
            needs_you: true,
            finished: false,
            hold_at_desk: true,
            created_at: "2026-11-07T00:00:00Z".to_owned(),
        };

        let shown = format!("{held:?}");

        for secret in ["TokenValue", "BPublicKeyValue", "AuthSecretValue"] {
            assert!(!shown.contains(secret), "{shown} shows {secret}");
        }
        assert!(shown.contains("sandbox"), "{shown} names the environment");
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
