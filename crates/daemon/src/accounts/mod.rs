//! The accounts handle the hub holds: the plugin's account service, the
//! last snapshot its poll read (mapped for the wire, so `ListAccounts`
//! answers without touching the store), and the driver that polls it.
//!
//! The cache is written by the driver alone; every reader takes the read
//! lock for a copy and never calls the service's poll. A write that changed
//! the store asks the driver to re-read it through the refresh lane
//! ([`Accounts::request_refresh`]) rather than caching what it read itself,
//! so a poll in flight cannot overwrite the write's snapshot with an older
//! one.
//!
//! Beside the driver runs the engine task ([`engine`]), which ticks the
//! service's auto-switch engine whether or not a client is connected; a
//! settings write wakes it ([`Accounts::engine_wake`]).
pub(crate) mod driver;
pub(crate) mod engine;
pub(crate) mod snapshot;

use crate::server::StateEvent;
use agent_api::AccountsService;
use snapshot::Wire;
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;
use tokio::sync::{Notify, broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::warn;

/// How long the daemon's exit waits for the driver to drain and the engine
/// task to end, both at once.
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
    /// A change to the auto-switch settings: wakes the engine task for a
    /// tick now. A wake while a tick runs is kept for after it.
    pub(crate) engine_wake: Notify,
    /// The driver's task, once started; taken by [`Accounts::join_driver`].
    driver: Mutex<Option<JoinHandle<()>>>,
    /// The engine task, once started; taken by [`Accounts::join_driver`].
    engine: Mutex<Option<JoinHandle<()>>>,
    /// The refresh lane's sending end.
    refresh_tx: mpsc::UnboundedSender<RefreshAck>,
    /// The refresh lane's receiving end until the driver takes it.
    refresh_rx: Mutex<Option<mpsc::UnboundedReceiver<RefreshAck>>>,
}

impl Accounts {
    /// The handle over `service`, with nothing cached and no driver.
    pub(crate) fn new(service: SharedAccounts) -> Arc<Self> {
        let (refresh_tx, refresh_rx) = mpsc::unbounded_channel();
        Arc::new(Self {
            service,
            cache: RwLock::new(None),
            wake: Notify::new(),
            engine_wake: Notify::new(),
            driver: Mutex::new(None),
            engine: Mutex::new(None),
            refresh_tx,
            refresh_rx: Mutex::new(Some(refresh_rx)),
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

    /// Start the engine task, which broadcasts the engine's events on
    /// `events` and ends when `shutdown` flips; one per hub.
    pub(crate) fn start_engine(
        self: &Arc<Self>,
        events: broadcast::Sender<StateEvent>,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        let task = engine::spawn(Arc::clone(self), events, shutdown);
        if let Some(earlier) = crate::sync::lock(&self.engine).replace(task) {
            warn!("the accounts engine task was started twice; stopping the earlier one");
            earlier.abort();
        }
    }

    /// Wait up to `bound`, once the shutdown watch has flipped, for the
    /// driver to drain and end and for the engine task to end, both at once.
    pub(crate) async fn join_driver(&self, bound: Duration) {
        let driver = crate::sync::lock(&self.driver).take();
        let engine = crate::sync::lock(&self.engine).take();
        let deadline = tokio::time::Instant::now() + bound;
        tokio::join!(
            join_task(driver, "the accounts driver", deadline, bound),
            join_task(engine, "the accounts engine task", deadline, bound),
        );
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
    /// cached yet; returns whether it did. The driver is its only caller, for
    /// a poll and for a refresh; anyone else asks for a refresh through the
    /// lane ([`Accounts::request_refresh`]).
    pub(crate) fn store_if_changed(&self, wire: Wire) -> bool {
        let mut cache = self.cache.write().unwrap_or_else(PoisonError::into_inner);
        if cache.as_ref() == Some(&wire) {
            return false;
        }
        *cache = Some(wire);
        true
    }

    /// Ask the driver to re-read the store (no network), cache what it read
    /// and broadcast it when it changed. The answer resolves once the cache
    /// holds that read; a refresh asked while a poll runs is served after
    /// the poll. `None` when the lane is closed: no driver ever started, or
    /// it has ended. The answer's sender is dropped, never sent, when the
    /// driver ends before serving it.
    pub(crate) fn request_refresh(&self) -> Option<oneshot::Receiver<()>> {
        if crate::sync::lock(&self.refresh_rx).is_some() {
            return None;
        }
        let (ack, done) = oneshot::channel();
        self.refresh_tx.send(ack).ok()?;
        Some(done)
    }

    /// The refresh lane's receiving end, for the driver that starts; `None`
    /// once an earlier driver took it.
    pub(crate) fn take_refreshes(&self) -> Option<mpsc::UnboundedReceiver<RefreshAck>> {
        crate::sync::lock(&self.refresh_rx).take()
    }
}

/// Wait until `deadline` for `task`, named `what` in the log, to end.
async fn join_task(
    task: Option<JoinHandle<()>>,
    what: &'static str,
    deadline: tokio::time::Instant,
    bound: Duration,
) {
    let Some(task) = task else {
        return;
    };
    match tokio::time::timeout_at(deadline, task).await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => warn!(%err, what, "an accounts task stopped abnormally"),
        Err(_) => warn!(?bound, what, "an accounts task did not end in time"),
    }
}

/// What a refresh request carries: the driver sends on it once the cache
/// holds the snapshot it read.
pub(crate) type RefreshAck = oneshot::Sender<()>;
