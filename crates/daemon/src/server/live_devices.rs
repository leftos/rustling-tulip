//! The served tailnet connections of each approved device, by device id, so
//! blocking or forgetting a device cuts every one of them at once.
//!
//! One mutex guards the whole map. The handshake verifies a device token and
//! registers the connection while holding it, and a block sets the device's
//! flag and cuts its connections while holding it, so no connection can
//! verify before the flag and register after the cut. Whenever both are
//! held, this lock is taken before [`super::Hub::devices`].
//!
//! A pairing connection and a loopback connection are never registered.
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use tokio_util::sync::CancellationToken;

/// Each device's served connections: a key per connection and the token
/// that ends it.
#[derive(Default)]
struct Registry {
    next_key: u64,
    by_device: HashMap<String, Vec<(u64, CancellationToken)>>,
}

/// The live registry the hub holds; clones share one map.
#[derive(Clone, Default)]
pub struct LiveDevices {
    inner: Arc<Mutex<Registry>>,
}

impl LiveDevices {
    /// Take the registry's lock. Take it before [`super::Hub::devices`]
    /// when both are held, and drop no [`LiveConnection`] while holding it.
    pub fn lock(&self) -> LiveLock<'_> {
        LiveLock {
            registry: crate::sync::lock(&self.inner),
            inner: &self.inner,
        }
    }

    /// Whether some caller holds the lock right now.
    #[cfg(test)]
    pub fn is_locked(&self) -> bool {
        matches!(
            self.inner.try_lock(),
            Err(std::sync::TryLockError::WouldBlock)
        )
    }
}

/// The registry, locked.
pub struct LiveLock<'a> {
    registry: MutexGuard<'a, Registry>,
    inner: &'a Arc<Mutex<Registry>>,
}

impl LiveLock<'_> {
    /// Register a served connection of `device_id` that `life` ends. The
    /// connection stays registered until the returned guard drops or the
    /// device is cut.
    pub fn register(&mut self, device_id: &str, life: CancellationToken) -> LiveConnection {
        let key = self.registry.next_key;
        self.registry.next_key += 1;
        self.registry
            .by_device
            .entry(device_id.to_owned())
            .or_default()
            .push((key, life));
        LiveConnection {
            registry: Arc::clone(self.inner),
            device_id: device_id.to_owned(),
            key,
        }
    }

    /// End every served connection of `device_id` and forget them; returns
    /// how many there were.
    pub fn cut(&mut self, device_id: &str) -> usize {
        let Some(connections) = self.registry.by_device.remove(device_id) else {
            return 0;
        };
        for (_, life) in &connections {
            life.cancel();
        }
        connections.len()
    }

    /// How many served connections `device_id` has.
    pub fn connected(&self, device_id: &str) -> u32 {
        self.registry
            .by_device
            .get(device_id)
            .map_or(0, |connections| {
                u32::try_from(connections.len()).unwrap_or(u32::MAX)
            })
    }
}

/// One registered connection; dropping it deregisters the connection, on
/// every exit path a panic included.
pub struct LiveConnection {
    registry: Arc<Mutex<Registry>>,
    device_id: String,
    key: u64,
}

impl LiveConnection {
    /// The device this connection is served as.
    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    /// Whether a block, a forget or a re-pair cut this connection.
    pub fn is_cut(&self) -> bool {
        let registry = crate::sync::lock(&self.registry);
        registry
            .by_device
            .get(&self.device_id)
            .is_none_or(|connections| !connections.iter().any(|(key, _)| *key == self.key))
    }
}

impl Drop for LiveConnection {
    fn drop(&mut self) {
        let mut registry = crate::sync::lock(&self.registry);
        let Some(connections) = registry.by_device.get_mut(&self.device_id) else {
            return;
        };
        connections.retain(|(key, _)| *key != self.key);
        if connections.is_empty() {
            registry.by_device.remove(&self.device_id);
        }
    }
}

impl fmt::Debug for LiveConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LiveConnection")
            .field("device_id", &self.device_id)
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

/// Two guards are equal when they stand for the same registration.
impl PartialEq for LiveConnection {
    fn eq(&self, other: &Self) -> bool {
        self.device_id == other.device_id && self.key == other.key
    }
}

impl Eq for LiveConnection {}

#[cfg(test)]
mod tests {
    use super::LiveDevices;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn the_guard_deregisters_on_every_exit() {
        let live = LiveDevices::default();

        let first = live.lock().register("d1", CancellationToken::new());
        let second = live.lock().register("d1", CancellationToken::new());
        assert_eq!(live.lock().connected("d1"), 2);
        drop(first);
        assert_eq!(
            live.lock().connected("d1"),
            1,
            "a normal return deregisters"
        );

        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _held = second;
            std::panic::resume_unwind(Box::new("the connection task unwound"));
        }));
        assert!(unwound.is_err());
        assert_eq!(live.lock().connected("d1"), 0, "an unwind deregisters");

        let life = CancellationToken::new();
        let cut = live.lock().register("d1", life.clone());
        let other = live.lock().register("d2", CancellationToken::new());
        assert_eq!(live.lock().cut("d1"), 1);
        assert!(life.is_cancelled(), "a cut ends the connection");
        let again = live.lock().register("d1", CancellationToken::new());
        drop(cut);
        assert_eq!(
            live.lock().connected("d1"),
            1,
            "a cut connection's guard leaves a newer registration alone"
        );
        assert_eq!(
            live.lock().connected("d2"),
            1,
            "another device is untouched"
        );
        drop(again);
        drop(other);
        assert_eq!(live.lock().connected("d1"), 0);
        assert_eq!(live.lock().connected("d2"), 0);
    }
}
