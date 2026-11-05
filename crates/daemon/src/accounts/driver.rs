//! The accounts driver: one task per hub that polls the account service's
//! usage while a client is connected, caches the mapped snapshot and
//! broadcasts it when it changed.
//!
//! The next poll is the wait the poll reported, never less than
//! [`POLL_FLOOR`]; a poll that failed waits the floor. The driver parks
//! while no client is connected, and while the store holds no account
//! until something wakes it ([`super::Accounts::wake`]). When the shutdown
//! watch flips it refuses new credential spends, waits up to
//! [`SHUTDOWN_DRAIN`] for the ones in flight, and ends.
//!
//! Every wait is one `select!` over the next poll, the client count, the
//! wake, the refresh lane and the shutdown watch, so a later engine tick
//! joins the same loop.
//!
//! A refresh ([`super::Accounts::request_refresh`]) re-reads the store with
//! no network, caches and broadcasts what changed, then acks; it is served
//! whether or not a client is connected. A poll is awaited outside the
//! `select!`, so a refresh asked while one runs waits for it and reads after
//! the poll's snapshot was cached. Once shutdown begins the lane is dropped,
//! with every refresh still queued in it, before the drain: the requester
//! sees its ack dropped rather than waiting.

use super::snapshot::{Wire, wire};
use super::{Accounts, RefreshAck};
use crate::server::StateEvent;
use agent_api::{AccountsError, PollReport, PollScope};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::time::Instant;
use tracing::{info, warn};

/// The least wait between two polls, and the wait after a failed one.
pub(crate) const POLL_FLOOR: Duration = Duration::from_secs(60);

/// How long shutdown waits for the credential spends in flight.
pub(crate) const SHUTDOWN_DRAIN: Duration = Duration::from_secs(15);

/// What the driver reads beside the handle: the attached-client count and
/// the daemon's shutdown watch.
pub(crate) struct Watches {
    pub(crate) clients: watch::Receiver<usize>,
    pub(crate) shutdown: watch::Receiver<bool>,
}

/// Start the driver of `accounts` for the daemon's life; it broadcasts on
/// `events`.
pub(crate) fn spawn(
    accounts: Arc<Accounts>,
    events: broadcast::Sender<StateEvent>,
    watches: Watches,
) -> tokio::task::JoinHandle<()> {
    // Taken before the task starts, so a refresh asked right after the start
    // queues for this driver instead of finding the lane closed.
    let refreshes = accounts.take_refreshes();
    if refreshes.is_none() {
        warn!("the accounts refresh lane was already taken; this driver serves no refresh");
    }
    tokio::spawn(run(accounts, events, watches, refreshes))
}

/// The refresh lane; `None` once shutdown dropped it (or a driver started
/// without it).
type Lane = Option<mpsc::UnboundedReceiver<RefreshAck>>;

/// The next refresh asked on `lane`; never resolves without one.
async fn next_refresh(lane: &mut Lane) -> Option<RefreshAck> {
    match lane {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

async fn run(
    accounts: Arc<Accounts>,
    events: broadcast::Sender<StateEvent>,
    watches: Watches,
    mut lane: Lane,
) {
    let Watches {
        mut clients,
        mut shutdown,
    } = watches;
    // When the next poll is due; `None` while the store holds no account.
    let mut due = Some(Instant::now());
    // Whether the last poll failed, so a run of failures warns once.
    let mut failing = false;
    loop {
        if *shutdown.borrow_and_update() {
            break;
        }
        let count = *clients.borrow_and_update();
        let connected = count > 0;
        if connected && due.is_some_and(|at| at <= Instant::now()) {
            match poll(&accounts, &events, &mut failing, &mut shutdown, &mut lane).await {
                Polled::Next(wait) => due = wait.map(|wait| Instant::now() + wait),
                // The poll was cut by shutdown, which already drained.
                Polled::ShutDown => return,
            }
            continue;
        }
        // A refresh is served after the `select!`, whose output may hold the
        // shutdown watch's guard, which no await may cross.
        let asked = tokio::select! {
            biased;
            _ = shutdown.wait_for(|stop| *stop) => break,
            changed = clients.changed() => {
                if changed.is_err() {
                    break;
                }
                // A client that connects is interest, as a wake is: a
                // driver parked with no account polls again.
                if due.is_none() && *clients.borrow_and_update() > count {
                    due = Some(Instant::now());
                }
                None
            }
            () = accounts.wake.notified(), if connected && due.is_none() => {
                due = Some(Instant::now());
                None
            }
            Some(ack) = next_refresh(&mut lane) => Some(ack),
            () = tokio::time::sleep_until(due.unwrap_or_else(Instant::now)),
                if connected && due.is_some() => None,
        };
        if let Some(ack) = asked {
            refresh(&accounts, &events).await;
            let _ = ack.send(());
        }
    }
    drop(lane);
    drain(&accounts).await;
}

/// Cache `wire` and broadcast it when it changed.
fn cache_and_broadcast(accounts: &Accounts, events: &broadcast::Sender<StateEvent>, wire: Wire) {
    if accounts.store_if_changed(wire.clone()) {
        let _ = events.send(StateEvent::Accounts {
            accounts: wire.accounts,
            auto: wire.auto,
        });
    }
}

/// Re-read the store on a blocking thread, with no network, and cache and
/// broadcast what changed; a read that fails keeps the cache.
async fn refresh(accounts: &Accounts, events: &broadcast::Sender<StateEvent>) {
    let service = Arc::clone(accounts.service());
    match tokio::task::spawn_blocking(move || service.snapshot()).await {
        Ok(Ok(snapshot)) => cache_and_broadcast(accounts, events, wire(&snapshot)),
        Ok(Err(err)) => {
            warn!(%err, "the account refresh could not read the store; keeping the cache");
        }
        Err(err) => warn!(%err, "the account refresh stopped; keeping the cache"),
    }
}

/// How a poll ended.
enum Polled {
    /// It answered or failed: the wait until the next poll, `None` when the
    /// store holds no account.
    Next(Option<Duration>),
    /// Shutdown came while it ran; the service is drained.
    ShutDown,
}

/// Poll once on a blocking thread, cache and broadcast what changed, and
/// return the wait until the next poll. When `shutdown` flips while the
/// poll runs, the service is drained while the poll is still in flight (the
/// drain refuses its new credential spends and waits for the ones it
/// started), the poll is given what is left of [`SHUTDOWN_DRAIN`] to return,
/// and nothing it read is cached or broadcast; the refresh `lane` is dropped
/// before the drain.
async fn poll(
    accounts: &Accounts,
    events: &broadcast::Sender<StateEvent>,
    failing: &mut bool,
    shutdown: &mut watch::Receiver<bool>,
    lane: &mut Lane,
) -> Polled {
    let service = Arc::clone(accounts.service());
    let mut task = tokio::task::spawn_blocking(move || service.poll(PollScope::OnDemand));
    // The watch's guard is dropped inside the wait, never held across the
    // drain's await.
    let stopped = async {
        let _ = shutdown.wait_for(|stop| *stop).await;
    };
    let joined = tokio::select! {
        joined = &mut task => Some(joined),
        () = stopped => None,
    };
    let Some(joined) = joined else {
        let started = Instant::now();
        drop(lane.take());
        drain(accounts).await;
        let left = SHUTDOWN_DRAIN.saturating_sub(started.elapsed());
        if tokio::time::timeout(left, task).await.is_err() {
            warn!(bound = ?SHUTDOWN_DRAIN, "the account poll did not end in time; leaving it");
        }
        return Polled::ShutDown;
    };
    let polled =
        joined.unwrap_or_else(|err| Err(AccountsError::new(format!("the poll stopped: {err}"))));
    Polled::Next(match polled {
        Ok(PollReport {
            snapshot,
            next_poll_in,
        }) => {
            if *failing {
                info!("the account poll answers again");
                *failing = false;
            }
            cache_and_broadcast(accounts, events, wire(&snapshot));
            next_poll_in.map(|wait| wait.max(POLL_FLOOR))
        }
        Err(err) => {
            if !*failing {
                warn!(%err, "the account poll failed; keeping the last snapshot and retrying");
                *failing = true;
            }
            Some(POLL_FLOOR)
        }
    })
}

/// Refuse new credential spends and wait, on a blocking thread, for the ones
/// in flight.
async fn drain(accounts: &Accounts) {
    let service = Arc::clone(accounts.service());
    match tokio::task::spawn_blocking(move || service.begin_shutdown(SHUTDOWN_DRAIN)).await {
        Ok(true) => info!("the account service drained"),
        Ok(false) => warn!(
            bound = ?SHUTDOWN_DRAIN,
            "the account service still had credential spends in flight at shutdown"
        ),
        Err(err) => warn!(%err, "the account service's drain stopped"),
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test fails with the message of the precondition it lost"
)]
mod tests {
    use super::*;
    use crate::accounts::snapshot::test_support::row;
    use agent_api::{AccountsService, AccountsSnapshot};
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Mutex, PoisonError};
    use tokio::sync::mpsc;

    /// A step-count guard on a paused clock, far past any passing wait.
    const GUARD: Duration = Duration::from_hours(7 * 24);

    /// A wait far past any poll the tests schedule.
    const A_DAY: Duration = Duration::from_hours(24);

    /// An account service whose polls answer the queued reports in order,
    /// then the last snapshot with no next poll; each poll and each drain is
    /// signalled, and with a gate each poll blocks until the gate releases.
    struct Scripted {
        reports: Mutex<VecDeque<Result<PollReport, AccountsError>>>,
        last: Mutex<AccountsSnapshot>,
        polls: AtomicUsize,
        signal: mpsc::UnboundedSender<()>,
        drains: Mutex<Vec<Duration>>,
        drain_signal: mpsc::UnboundedSender<()>,
        gate: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
        /// What happened, in order: `poll`, `drain`, `poll returned`.
        order: Mutex<Vec<&'static str>>,
    }

    impl Scripted {
        fn record(&self, step: &'static str) {
            self.order
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(step);
        }

        fn order(&self) -> Vec<&'static str> {
            self.order
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }

        /// What the store holds from now on, as a write would leave it: the
        /// next `snapshot()` answers it.
        fn set_snapshot(&self, snapshot: AccountsSnapshot) {
            *self.last.lock().unwrap_or_else(PoisonError::into_inner) = snapshot;
        }
    }

    impl AccountsService for Scripted {
        fn snapshot(&self) -> Result<AccountsSnapshot, AccountsError> {
            Ok(self
                .last
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone())
        }

        fn poll(&self, _scope: PollScope) -> Result<PollReport, AccountsError> {
            let queued = self
                .reports
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop_front();
            let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
            let answer = queued.unwrap_or_else(|| {
                Ok(PollReport {
                    snapshot: last.clone(),
                    next_poll_in: None,
                })
            });
            if let Ok(report) = &answer {
                last.clone_from(&report.snapshot);
            }
            drop(last);
            self.record("poll");
            self.polls.fetch_add(1, Ordering::SeqCst);
            let _ = self.signal.send(());
            if let Some(gate) = &*self.gate.lock().unwrap_or_else(PoisonError::into_inner) {
                let _ = gate.recv();
            }
            self.record("poll returned");
            answer
        }

        fn begin_shutdown(&self, bound: Duration) -> bool {
            self.drains
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(bound);
            self.record("drain");
            let _ = self.drain_signal.send(());
            true
        }
    }

    /// A poll answering one account at `pct` and asking for the next poll in
    /// `next`.
    fn report(pct: f64, next: Option<Duration>) -> PollReport {
        PollReport {
            snapshot: AccountsSnapshot {
                accounts: vec![row(1, pct)],
                auto: None,
            },
            next_poll_in: next,
        }
    }

    struct Rig {
        accounts: Arc<Accounts>,
        fake: Arc<Scripted>,
        calls: mpsc::UnboundedReceiver<()>,
        drained: mpsc::UnboundedReceiver<()>,
        events: broadcast::Receiver<StateEvent>,
        _events_tx: broadcast::Sender<StateEvent>,
        clients: watch::Sender<usize>,
        shutdown: watch::Sender<bool>,
        task: tokio::task::JoinHandle<()>,
    }

    /// A driver over the queued `reports` with `clients` connected.
    fn rig(reports: Vec<Result<PollReport, AccountsError>>, clients: usize) -> Rig {
        rig_with(reports, clients, None)
    }

    /// [`rig`] whose polls block until `gate` releases each one.
    fn rig_with(
        reports: Vec<Result<PollReport, AccountsError>>,
        clients: usize,
        gate: Option<std::sync::mpsc::Receiver<()>>,
    ) -> Rig {
        let (signal, calls) = mpsc::unbounded_channel();
        let (drain_signal, drained) = mpsc::unbounded_channel();
        let fake = Arc::new(Scripted {
            reports: Mutex::new(reports.into()),
            last: Mutex::default(),
            polls: AtomicUsize::new(0),
            signal,
            drains: Mutex::default(),
            drain_signal,
            gate: Mutex::new(gate),
            order: Mutex::default(),
        });
        let accounts = Accounts::new(Arc::clone(&fake) as Arc<dyn AccountsService>);
        let (events_tx, events) = broadcast::channel(16);
        let (clients, clients_rx) = watch::channel(clients);
        let (shutdown, shutdown_rx) = watch::channel(false);
        let task = spawn(
            Arc::clone(&accounts),
            events_tx.clone(),
            Watches {
                clients: clients_rx,
                shutdown: shutdown_rx,
            },
        );
        Rig {
            accounts,
            fake,
            calls,
            drained,
            events,
            _events_tx: events_tx,
            clients,
            shutdown,
            task,
        }
    }

    impl Rig {
        fn polls(&self) -> usize {
            self.fake.polls.load(Ordering::SeqCst)
        }

        /// Wait for the next poll's signal.
        async fn next_call(&mut self) {
            tokio::time::timeout(GUARD, self.calls.recv())
                .await
                .expect("a poll came")
                .expect("the fake is alive");
        }

        /// Wait for the next broadcast: the accounts it carries.
        async fn next_event(&mut self) -> Vec<protocol::AccountSnapshot> {
            let event = tokio::time::timeout(GUARD, self.events.recv())
                .await
                .expect("a broadcast came")
                .expect("the channel is open");
            let StateEvent::Accounts { accounts, .. } = event else {
                return Vec::new();
            };
            accounts
        }

        fn no_event(&mut self) -> bool {
            matches!(
                self.events.try_recv(),
                Err(broadcast::error::TryRecvError::Empty)
            )
        }
    }

    /// Let the tasks run; yields only, so a passing run takes no wall time.
    async fn settle() {
        for _ in 0..1_000 {
            tokio::task::yield_now().await;
        }
    }

    fn pct_of(accounts: &[protocol::AccountSnapshot]) -> Option<f64> {
        accounts
            .first()
            .and_then(|account| account.five_hour.as_ref())
            .map(|window| window.pct)
    }

    #[tokio::test(start_paused = true)]
    async fn no_poll_while_no_client_is_connected() {
        let mut rig = rig(vec![Ok(report(10.0, None))], 0);

        tokio::time::sleep(A_DAY).await;
        settle().await;
        assert_eq!(rig.polls(), 0, "no client, no poll");

        rig.clients.send_replace(1);
        rig.next_call().await;
        assert_eq!(
            rig.polls(),
            1,
            "the parked driver polls once a client comes"
        );
        rig.task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn first_client_starts_a_poll_and_broadcasts() {
        let mut rig = rig(vec![Ok(report(40.0, None))], 0);

        rig.clients.send_replace(1);
        rig.next_call().await;
        let accounts = rig.next_event().await;

        assert_eq!(pct_of(&accounts), Some(40.0));
        let cached = rig.accounts.cached().expect("the poll is cached");
        assert_eq!(cached.accounts, accounts, "the cache holds what was sent");
        rig.task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn an_unchanged_snapshot_is_not_broadcast() {
        let mut rig = rig(
            vec![
                Ok(report(10.0, Some(POLL_FLOOR))),
                Ok(report(10.0, Some(POLL_FLOOR))),
                Ok(report(20.0, None)),
            ],
            1,
        );
        rig.next_call().await;
        assert_eq!(pct_of(&rig.next_event().await), Some(10.0));

        rig.next_call().await;
        rig.next_call().await;

        assert_eq!(
            pct_of(&rig.next_event().await),
            Some(20.0),
            "the unchanged second poll sent nothing; the changed third did"
        );
        rig.task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn next_poll_waits_for_next_poll_in() {
        let wait = Duration::from_secs(300);
        let mut rig = rig(
            vec![Ok(report(10.0, Some(wait))), Ok(report(10.0, None))],
            1,
        );
        rig.next_call().await;

        tokio::time::sleep(wait.saturating_sub(Duration::from_secs(1))).await;
        settle().await;
        assert_eq!(rig.polls(), 1, "not before the wait the poll asked for");

        tokio::time::sleep(Duration::from_secs(2)).await;
        rig.next_call().await;
        assert_eq!(rig.polls(), 2);
        rig.task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn no_account_parks_until_woken() {
        let mut rig = rig(vec![Ok(report(10.0, None)), Ok(report(10.0, None))], 1);
        rig.next_call().await;

        tokio::time::sleep(A_DAY).await;
        settle().await;
        assert_eq!(rig.polls(), 1, "no account, no next poll");

        rig.accounts.wake.notify_one();
        rig.next_call().await;
        assert_eq!(rig.polls(), 2, "a wake polls again");
        rig.task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn a_poll_error_keeps_the_cache_and_backs_off_to_the_floor() {
        let mut rig = rig(
            vec![
                Ok(report(10.0, Some(POLL_FLOOR))),
                Err(AccountsError::new("the store is unreadable")),
                Ok(report(20.0, None)),
            ],
            1,
        );
        rig.next_call().await;
        assert_eq!(pct_of(&rig.next_event().await), Some(10.0));
        rig.next_call().await;

        tokio::time::sleep(POLL_FLOOR.saturating_sub(Duration::from_secs(1))).await;
        settle().await;
        assert_eq!(rig.polls(), 2, "the failed poll waits the floor");
        assert!(rig.no_event(), "a failed poll broadcasts nothing");
        let cached = rig.accounts.cached().expect("the cache stays");
        assert_eq!(pct_of(&cached.accounts), Some(10.0));

        tokio::time::sleep(Duration::from_secs(2)).await;
        rig.next_call().await;
        assert_eq!(pct_of(&rig.next_event().await), Some(20.0));
        rig.task.abort();
    }

    /// On the real clock: the poll blocks a real thread, and the waits are
    /// hang guards on its events.
    #[tokio::test]
    async fn shutdown_during_a_poll_drains_before_the_poll_returns() {
        const HANG: Duration = Duration::from_mins(1);
        let (release, gate) = std::sync::mpsc::channel();
        let mut rig = rig_with(vec![Ok(report(10.0, Some(POLL_FLOOR)))], 1, Some(gate));
        tokio::time::timeout(HANG, rig.calls.recv())
            .await
            .expect("the poll started")
            .expect("the fake is alive");

        rig.shutdown.send_replace(true);
        tokio::time::timeout(HANG, rig.drained.recv())
            .await
            .expect("the drain ran while the poll was blocked")
            .expect("the fake is alive");
        assert_eq!(rig.fake.order(), ["poll", "drain"], "drained mid-poll");

        release.send(()).expect("the poll is still waiting");
        tokio::time::timeout(HANG, &mut rig.task)
            .await
            .expect("the driver ended")
            .expect("the driver did not panic");
        assert_eq!(rig.fake.order(), ["poll", "drain", "poll returned"]);
        assert!(rig.no_event(), "a poll cut by shutdown broadcasts nothing");
        assert_eq!(rig.accounts.cached(), None, "nor caches");
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_client_restarts_polling_after_a_no_account_park() {
        let mut rig = rig(vec![Ok(report(10.0, None)), Ok(report(10.0, None))], 1);
        rig.next_call().await;

        rig.clients.send_replace(0);
        settle().await;
        rig.clients.send_replace(1);
        rig.next_call().await;

        assert_eq!(rig.polls(), 2, "the client that came polls again");
        rig.task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_drains_once_and_ends_the_loop() {
        let mut rig = rig(vec![Ok(report(10.0, None))], 1);
        rig.next_call().await;

        rig.shutdown.send_replace(true);
        tokio::time::timeout(GUARD, &mut rig.task)
            .await
            .expect("the driver ended")
            .expect("the driver did not panic");

        let drains = rig
            .fake
            .drains
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        assert_eq!(drains, [SHUTDOWN_DRAIN], "one drain, with the bound");
        assert_eq!(rig.polls(), 1);
    }

    /// Ask for a refresh and wait for its ack.
    async fn refreshed(accounts: &Accounts, guard: Duration) {
        let ack = accounts.request_refresh().expect("the lane is open");
        tokio::time::timeout(guard, ack)
            .await
            .expect("the refresh was acked")
            .expect("the driver acked, not dropped");
    }

    #[tokio::test(start_paused = true)]
    async fn a_refresh_caches_and_broadcasts_the_new_snapshot() {
        let mut rig = rig(vec![Ok(report(10.0, None))], 1);
        rig.next_call().await;
        assert_eq!(pct_of(&rig.next_event().await), Some(10.0));

        rig.fake.set_snapshot(report(30.0, None).snapshot);
        refreshed(&rig.accounts, GUARD).await;

        assert_eq!(pct_of(&rig.next_event().await), Some(30.0));
        let cached = rig.accounts.cached().expect("the refresh is cached");
        assert_eq!(pct_of(&cached.accounts), Some(30.0));
        assert_eq!(rig.polls(), 1, "a refresh is not a poll");
        rig.task.abort();
    }

    /// On the real clock: the poll blocks a real thread, and the waits are
    /// hang guards on its events.
    #[tokio::test]
    async fn a_refresh_waits_for_a_poll_in_flight_and_wins() {
        const HANG: Duration = Duration::from_mins(1);
        let (release, gate) = std::sync::mpsc::channel();
        let mut rig = rig_with(vec![Ok(report(10.0, None))], 1, Some(gate));
        tokio::time::timeout(HANG, rig.calls.recv())
            .await
            .expect("the poll started")
            .expect("the fake is alive");

        let ack = rig.accounts.request_refresh().expect("the lane is open");
        rig.fake.set_snapshot(report(50.0, None).snapshot);
        release.send(()).expect("the poll is still waiting");
        tokio::time::timeout(HANG, ack)
            .await
            .expect("the refresh was acked")
            .expect("the driver acked, not dropped");

        let cached = rig.accounts.cached().expect("something is cached");
        assert_eq!(
            pct_of(&cached.accounts),
            Some(50.0),
            "the refresh, not the poll, holds the cache"
        );
        let mut last = None;
        while let Ok(event) = rig.events.try_recv() {
            if let StateEvent::Accounts { accounts, .. } = event {
                last = pct_of(&accounts);
            }
        }
        assert_eq!(last, Some(50.0), "the last broadcast carries the refresh");
        rig.task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn a_refresh_with_nothing_changed_acks_without_a_broadcast() {
        let mut rig = rig(vec![Ok(report(10.0, None))], 1);
        rig.next_call().await;
        assert_eq!(pct_of(&rig.next_event().await), Some(10.0));

        refreshed(&rig.accounts, GUARD).await;

        assert!(rig.no_event(), "an unchanged refresh broadcasts nothing");
        let cached = rig.accounts.cached().expect("the cache stays");
        assert_eq!(pct_of(&cached.accounts), Some(10.0));
        rig.task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn a_refresh_after_shutdown_is_dropped_not_hung() {
        // Far inside the handler's ten-second refresh wait.
        const PROMPT: Duration = Duration::from_secs(1);
        let (release, gate) = std::sync::mpsc::channel();
        let mut rig = rig_with(vec![Ok(report(10.0, Some(POLL_FLOOR)))], 1, Some(gate));
        rig.next_call().await;

        // Queued behind the gated poll, so the driver has not served it.
        let ack = rig.accounts.request_refresh().expect("the lane is open");
        rig.shutdown.send_replace(true);

        let answered = tokio::time::timeout(PROMPT, ack)
            .await
            .expect("the queued refresh was dropped at shutdown, not left to wait");
        assert!(answered.is_err(), "the ack's sender was dropped, not sent");
        assert!(
            rig.accounts.request_refresh().is_none(),
            "a refresh asked after shutdown finds the lane closed"
        );

        release.send(()).expect("the poll is still waiting");
        tokio::time::timeout(GUARD, &mut rig.task)
            .await
            .expect("the driver ended")
            .expect("the driver did not panic");
        assert_eq!(rig.accounts.cached(), None, "the refresh never ran");
    }
}
