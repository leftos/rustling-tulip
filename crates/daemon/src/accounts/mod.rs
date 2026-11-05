//! The accounts handle the hub holds: the plugin's account service, the
//! last snapshot its poll read (mapped for the wire, so `ListAccounts`
//! answers without touching the store), and the driver that polls it.
//!
//! The cache is written by the driver alone; every reader takes the read
//! lock for a copy and never calls the service's poll.
pub(crate) mod driver;
pub(crate) mod snapshot;

use crate::server::StateEvent;
use agent_api::AccountsService;
use snapshot::Wire;
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;
use tokio::sync::{Notify, broadcast};
use tokio::task::JoinHandle;
use tracing::warn;

/// How long the daemon's exit waits for the driver to drain and end.
pub(crate) const DRIVER_JOIN: Duration = Duration::from_secs(20);

/// The account service as the hub holds it: shared, with no lock around it,
/// so a read answers while a poll is in flight.
pub(crate) type SharedAccounts = Arc<dyn AccountsService>;

/// The plugin's account service, its cached wire snapshot and its driver.
pub(crate) struct Accounts {
    service: SharedAccounts,
    /// The mapped snapshot of the driver's last successful poll; `None`
    /// before the first.
    cache: RwLock<Option<Wire>>,
    /// Interest in the accounts: wakes a driver parked because the store
    /// held no account.
    pub(crate) wake: Notify,
    /// The driver's task, once started; taken by [`Accounts::join_driver`].
    driver: Mutex<Option<JoinHandle<()>>>,
}

impl Accounts {
    /// The handle over `service`, with nothing cached and no driver.
    pub(crate) fn new(service: SharedAccounts) -> Arc<Self> {
        Arc::new(Self {
            service,
            cache: RwLock::new(None),
            wake: Notify::new(),
            driver: Mutex::new(None),
        })
    }

    /// Start the driver, which broadcasts on `events`; one per hub.
    pub(crate) fn start_driver(
        self: &Arc<Self>,
        events: broadcast::Sender<StateEvent>,
        watches: driver::Watches,
    ) {
        let task = driver::spawn(Arc::clone(self), events, watches);
        if let Some(earlier) = crate::sync::lock(&self.driver).replace(task) {
            warn!("the accounts driver was started twice; stopping the earlier one");
            earlier.abort();
        }
    }

    /// Wait up to `bound` for the driver to drain and end, once the shutdown
    /// watch has flipped.
    pub(crate) async fn join_driver(&self, bound: Duration) {
        let Some(task) = crate::sync::lock(&self.driver).take() else {
            return;
        };
        match tokio::time::timeout(bound, task).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => warn!(%err, "the accounts driver stopped abnormally"),
            Err(_) => warn!(?bound, "the accounts driver did not end in time"),
        }
    }

    /// The account service.
    pub(crate) fn service(&self) -> &SharedAccounts {
        &self.service
    }

    /// A copy of the cached snapshot; `None` before the first poll.
    pub(crate) fn cached(&self) -> Option<Wire> {
        self.cache
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Cache `wire` when it differs from the cached snapshot, or nothing is
    /// cached yet; returns whether it did. The driver's alone to call.
    pub(crate) fn store_if_changed(&self, wire: Wire) -> bool {
        let mut cache = self.cache.write().unwrap_or_else(PoisonError::into_inner);
        if cache.as_ref() == Some(&wire) {
            return false;
        }
        *cache = Some(wire);
        true
    }
}
