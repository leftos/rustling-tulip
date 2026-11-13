//! The push state the hub holds behind one lock: the push folder, the
//! endpoint policy, and, once the first push request has loaded them, the
//! VAPID key and the subscription store. [`super::keys::load_or_create`] has
//! no lock of its own, so it and every subscription write run under this
//! one; the lock is a leaf below `Hub::devices` (see `server/pairing.rs`).

use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::anyhow;

use super::send::PushPolicy;
use super::store::{SUBSCRIPTIONS_FILE, SubscriptionStore};
use super::vapid::VapidKey;
use crate::devices::DeviceStore;

/// The key and the subscriptions, loaded on the first push request.
pub(crate) struct Loaded {
    /// The daemon's VAPID key pair, created on first use and never rotated.
    pub key: VapidKey,
    /// One subscription per device id.
    pub subs: SubscriptionStore,
}

/// The daemon's push state.
pub(crate) struct PushState {
    /// The push folder; `None` when it could not be resolved at start, so
    /// every push request fails.
    dir: Option<PathBuf>,
    /// Which endpoints a subscription may name.
    policy: PushPolicy,
    /// The key and the store, once loaded.
    loaded: Option<Loaded>,
}

impl PushState {
    /// A state over `dir`, loaded on the first push request.
    #[must_use]
    pub(crate) fn new(dir: Option<PathBuf>, policy: PushPolicy) -> Self {
        Self {
            dir,
            policy,
            loaded: None,
        }
    }

    /// Which endpoints a subscription may name.
    #[must_use]
    pub(crate) fn policy(&self) -> PushPolicy {
        self.policy
    }

    /// The key and the store, loading them on first use and then sweeping
    /// them against `devices`: [`Self::load_unswept`], then, on that first
    /// load only, [`Self::sweep`].
    ///
    /// # Errors
    ///
    /// The push folder is unresolved or could not be created, the key file
    /// could not be read, decoded or written, `subscriptions.json` is
    /// corrupt, or the sweep could not save it. Nothing is kept, so the next
    /// request tries again; the messages name the files, never an endpoint
    /// or a key.
    pub(crate) fn load(&mut self, devices: &DeviceStore) -> anyhow::Result<&mut Loaded> {
        let first = self.loaded.is_none();
        self.load_unswept()?;
        if first {
            self.sweep(devices)?;
        }
        self.loaded
            .as_mut()
            .ok_or_else(|| anyhow!("the push state did not load"))
    }

    /// The key and the store, loading them on first use without
    /// `Hub::devices`: the folder is created, the key is read or made, and
    /// the store is read. A first load keeps every subscription in the file,
    /// so the caller runs [`Self::sweep`] under `Hub::devices` before it
    /// lets go of this lock's work (a block that found the state unloaded
    /// relies on that sweep).
    ///
    /// # Errors
    ///
    /// The push folder is unresolved or could not be created, the key file
    /// could not be read, decoded or written, or `subscriptions.json` is
    /// corrupt. Nothing is kept, so the next request tries again.
    pub(crate) fn load_unswept(&mut self) -> anyhow::Result<&mut Loaded> {
        if self.loaded.is_none() {
            let dir = self
                .dir
                .as_deref()
                .ok_or_else(|| anyhow!("the push folder could not be resolved"))?;
            super::ensure_folder(dir)?;
            let key = super::keys::load_or_create(dir)?;
            let path = dir.join(SUBSCRIPTIONS_FILE);
            let subs = SubscriptionStore::load(&path);
            if subs.is_corrupt() {
                return Err(anyhow!(
                    "{} is corrupt; the file is left as it is",
                    path.display()
                ));
            }
            self.loaded = Some(Loaded { key, subs });
        }
        self.loaded
            .as_mut()
            .ok_or_else(|| anyhow!("the push state did not load"))
    }

    /// Drop every loaded subscription of a device that is not an unrevoked
    /// record in `devices`, in memory and, when one drops, in the file. An
    /// unloaded state, or a corrupt `devices` (so a corrupt `devices.json`
    /// wipes nothing), is left as it is.
    ///
    /// # Errors
    ///
    /// The sweep could not save the file; the state is then unloaded, so no
    /// dropped subscription stays in memory and the next load sweeps again.
    pub(crate) fn sweep(&mut self, devices: &DeviceStore) -> anyhow::Result<()> {
        let Some(loaded) = self.loaded.as_mut() else {
            return Ok(());
        };
        let Some(known) = unrevoked_ids(devices) else {
            return Ok(());
        };
        match loaded.subs.retain_devices(&known) {
            Ok(0) => Ok(()),
            Ok(dropped) => {
                tracing::info!(
                    dropped,
                    "push: dropped the subscriptions of devices no longer paired"
                );
                Ok(())
            }
            Err(err) => {
                self.loaded = None;
                Err(err)
            }
        }
    }

    /// Whether the key and the store are loaded.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn is_loaded(&self) -> bool {
        self.loaded.is_some()
    }

    /// Drop the loaded key and store, as a failed save does, so the next
    /// load reads the file and sweeps it again.
    pub(crate) fn unload(&mut self) {
        self.loaded = None;
    }

    /// Delete `device_id`'s subscription when the state is loaded and its
    /// address is still `address` (see
    /// [`SubscriptionStore::remove_if_address`]). An unloaded state does
    /// nothing, so the push trigger loads it first, holding `Hub::devices`
    /// then this lock. A save failure is logged and unloads the state, as
    /// [`Self::remove_device`] does.
    pub(crate) fn remove_if_address(&mut self, device_id: &str, address: &str) {
        let Some(loaded) = self.loaded.as_mut() else {
            return;
        };
        match loaded.subs.remove_if_address(device_id, address) {
            Ok(true) => {
                tracing::info!(%device_id, "push: the push service dropped the device's subscription; it was deleted");
            }
            Ok(false) => {}
            Err(err) => {
                tracing::warn!(
                    %device_id,
                    err = format!("{err:#}"),
                    "push: could not delete the device's dropped subscription; the push state is unloaded until the next request"
                );
                self.loaded = None;
            }
        }
    }

    /// Delete `device_id`'s subscription when the state is loaded; an
    /// unloaded state has nothing in memory, and its file is swept on load.
    /// A failure is logged, never returned: the caller's block, forget or
    /// pairing goes on. It also unloads the state, so the subscription does
    /// not stay in memory: the next request loads again and its sweep drops
    /// the device, or fails.
    pub(crate) fn remove_device(&mut self, device_id: &str) {
        let Some(loaded) = self.loaded.as_mut() else {
            return;
        };
        match loaded.subs.remove(device_id) {
            Ok(true) => tracing::info!(%device_id, "push: the device's subscription was deleted"),
            Ok(false) => {}
            Err(err) => {
                tracing::warn!(
                    %device_id,
                    err = format!("{err:#}"),
                    "push: could not delete the device's subscription; the push state is unloaded until the next request"
                );
                self.loaded = None;
            }
        }
    }
}

/// The ids of `devices`' unrevoked records, the devices a subscription may
/// belong to; `None` when `devices` is corrupt, so nothing is judged by it.
pub(crate) fn unrevoked_ids(devices: &DeviceStore) -> Option<HashSet<String>> {
    if devices.is_corrupt() {
        return None;
    }
    Some(
        devices
            .list()
            .iter()
            .filter(|record| !record.revoked)
            .map(|record| record.id.clone())
            .collect(),
    )
}
