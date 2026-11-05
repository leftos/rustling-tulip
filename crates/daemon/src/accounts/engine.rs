//! The accounts engine task: one task per hub, beside the poll driver, that
//! ticks the account service's auto-switch engine whether or not a client is
//! connected.
//!
//! Each tick runs on a blocking thread and is awaited before the next, so one
//! tick runs at a time. A tick that ran broadcasts its events as
//! `AccountEvent`s, asks the driver for a refresh (so `AccountsUpdated`
//! carries the new auto-switch state), and waits the delay it reported. A
//! tick whose engine halted waits for a wake ([`super::Accounts::engine_wake`],
//! a settings write) instead; a tick that found auto-switch off, and one that
//! failed, wait the poll floor, so auto-switch turned on outside this daemon
//! starts within it. Every wait also ends on a wake, and a
//! wake that came while a tick ran is kept for after it.
//!
//! When the shutdown watch flips the task ends, a tick in flight included:
//! the driver's drain ([`agent_api::AccountsService::begin_shutdown`]) waits
//! for it and releases the engine's lease.

use super::Accounts;
use super::driver::POLL_FLOOR;
use crate::server::StateEvent;
use agent_api::AccountsError;
use agent_api::accounts::{AccountEventRow, EngineTick};
use protocol::AccountEventKind;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, watch};
use tracing::{info, warn};

/// `row` on the wire.
pub(crate) fn account_event(row: AccountEventRow) -> AccountEventKind {
    match row {
        AccountEventRow::Switched { from, to, trigger } => {
            AccountEventKind::Switched { from, to, trigger }
        }
        AccountEventRow::AllExhausted { earliest_reset_at } => {
            AccountEventKind::AllExhausted { earliest_reset_at }
        }
        AccountEventRow::Quarantined { number } => AccountEventKind::Quarantined { number },
        AccountEventRow::Unquarantined { number } => AccountEventKind::Unquarantined { number },
        AccountEventRow::Halted { message } => AccountEventKind::Halted { message },
    }
}

/// Start the engine task of `accounts` for the daemon's life; it broadcasts
/// on `events` and ends when `shutdown` flips.
pub(crate) fn spawn(
    accounts: Arc<Accounts>,
    events: broadcast::Sender<StateEvent>,
    shutdown: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(run(accounts, events, shutdown))
}

/// What the loop waits for after a tick, beside a wake and shutdown.
enum Next {
    /// Nothing but a wake.
    Wake,
    /// This long, or a wake.
    Sleep(Duration),
}

/// Resolves when `next`'s wait is over; never for [`Next::Wake`].
async fn waited(next: Next) {
    match next {
        Next::Wake => std::future::pending().await,
        Next::Sleep(delay) => tokio::time::sleep(delay).await,
    }
}

async fn run(
    accounts: Arc<Accounts>,
    events: broadcast::Sender<StateEvent>,
    mut shutdown: watch::Receiver<bool>,
) {
    // Whether the last tick failed, so a run of failures warns once.
    let mut failing = false;
    // Whether the engine is halted, so a halt is logged once.
    let mut halted = false;
    loop {
        if *shutdown.borrow_and_update() {
            break;
        }
        let Some(ticked) = tick(&accounts, &mut shutdown).await else {
            info!("the auto-switch tick in flight at shutdown is left to the account drain");
            break;
        };
        let next = match ticked {
            // Turned on outside this daemon (a hand edit, another client
            // of the store) is seen within the floor.
            Ok(EngineTick::Off) => {
                failing = false;
                halted = false;
                Next::Sleep(POLL_FLOOR)
            }
            Ok(EngineTick::Ran {
                events: rows,
                delay,
                halted: halt,
                ..
            }) => {
                if failing {
                    info!("the auto-switch engine ticks again");
                    failing = false;
                }
                for row in rows {
                    let _ = events.send(StateEvent::AccountEvent {
                        event: account_event(row),
                    });
                }
                // Not awaited: the driver serves it, and the next wait must
                // not hang on a driver that has ended.
                drop(accounts.request_refresh());
                if let Some(message) = halt {
                    if !halted {
                        warn!(%message, "the auto-switch engine halted; it waits for a settings change or a restart");
                        halted = true;
                    }
                    Next::Wake
                } else {
                    halted = false;
                    Next::Sleep(delay)
                }
            }
            Err(err) => {
                if !failing {
                    warn!(%err, "the auto-switch tick failed; retrying");
                    failing = true;
                }
                Next::Sleep(POLL_FLOOR)
            }
        };
        tokio::select! {
            biased;
            _ = shutdown.wait_for(|stop| *stop) => break,
            () = accounts.engine_wake.notified() => {}
            () = waited(next) => {}
        }
    }
}

/// One tick on a blocking thread; `None` when `shutdown` flipped first,
/// leaving the tick to finish on its own.
async fn tick(
    accounts: &Accounts,
    shutdown: &mut watch::Receiver<bool>,
) -> Option<Result<EngineTick, AccountsError>> {
    let service = Arc::clone(accounts.service());
    let task = tokio::task::spawn_blocking(move || service.engine_tick());
    // The watch's guard is dropped inside the wait.
    let stopped = async {
        let _ = shutdown.wait_for(|stop| *stop).await;
    };
    tokio::select! {
        joined = task => Some(joined.unwrap_or_else(|err| {
            Err(AccountsError::new(format!("the auto-switch tick stopped: {err}")))
        })),
        () = stopped => None,
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test fails with the message of the precondition it lost"
)]
mod tests {
    use super::*;
    use crate::accounts::driver::Watches;
    use agent_api::{AccountsService, AccountsSnapshot, AutoState, PollReport, PollScope};
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Mutex, PoisonError};
    use tokio::sync::mpsc;

    /// A step-count guard on a paused clock, far past any passing wait.
    const GUARD: Duration = Duration::from_hours(7 * 24);

    /// A wait far past any tick delay the tests schedule.
    const A_DAY: Duration = Duration::from_hours(24);

    /// A hang guard for the tests on the real clock.
    const HANG: Duration = Duration::from_mins(1);

    /// An account service whose engine ticks answer the queued results in
    /// order, then [`EngineTick::Off`]; each tick is signalled, and with a
    /// gate each blocks until the gate releases it. A tick that ran paused
    /// puts its reason in the snapshot's auto-switch state, as the plugin's
    /// snapshot shows it.
    struct Engined {
        ticks: Mutex<VecDeque<Result<EngineTick, AccountsError>>>,
        count: AtomicUsize,
        signal: mpsc::UnboundedSender<()>,
        gate: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
        snapshot: Mutex<AccountsSnapshot>,
    }

    impl AccountsService for Engined {
        fn snapshot(&self) -> Result<AccountsSnapshot, AccountsError> {
            Ok(self
                .snapshot
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone())
        }

        fn poll(&self, _scope: PollScope) -> Result<PollReport, AccountsError> {
            Ok(PollReport {
                snapshot: self.snapshot()?,
                next_poll_in: None,
            })
        }

        fn begin_shutdown(&self, _bound: Duration) -> bool {
            true
        }

        fn engine_tick(&self) -> Result<EngineTick, AccountsError> {
            let answer = self
                .ticks
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop_front()
                .unwrap_or(Ok(EngineTick::Off));
            if let Ok(EngineTick::Ran { paused, .. }) = &answer {
                self.snapshot
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .auto = Some(auto_paused(paused.clone()));
            }
            self.count.fetch_add(1, Ordering::SeqCst);
            let _ = self.signal.send(());
            if let Some(gate) = &*self.gate.lock().unwrap_or_else(PoisonError::into_inner) {
                let _ = gate.recv();
            }
            answer
        }
    }

    /// An auto-switch state that is on, paused for `reason`.
    fn auto_paused(reason: Option<String>) -> AutoState {
        AutoState {
            enabled: true,
            threshold_pct: 80.0,
            hysteresis_pct: 5.0,
            cooldown_secs: 300,
            strategy: protocol::AutoSwitchStrategy::Best,
            rate_window_minutes: None,
            paused_reason: reason,
            warm_up_done: true,
            last_decision: None,
            fleet: None,
        }
    }

    /// A tick that ran, with `events`, asking for the next in `delay`.
    fn ran(events: Vec<AccountEventRow>, delay: Duration) -> EngineTick {
        EngineTick::Ran {
            events,
            delay,
            paused: None,
            halted: None,
        }
    }

    struct Rig {
        accounts: Arc<Accounts>,
        fake: Arc<Engined>,
        ticked: mpsc::UnboundedReceiver<()>,
        events: broadcast::Receiver<StateEvent>,
        _events_tx: broadcast::Sender<StateEvent>,
        /// The attached-client count the poll driver reads: always 0.
        _clients: watch::Sender<usize>,
        shutdown: watch::Sender<bool>,
        task: tokio::task::JoinHandle<()>,
    }

    /// The engine task over the queued `ticks`, beside a poll driver (which
    /// serves the refreshes) with no client connected.
    fn rig(
        ticks: Vec<Result<EngineTick, AccountsError>>,
        gate: Option<std::sync::mpsc::Receiver<()>>,
    ) -> Rig {
        let (signal, ticked) = mpsc::unbounded_channel();
        let fake = Arc::new(Engined {
            ticks: Mutex::new(ticks.into()),
            count: AtomicUsize::new(0),
            signal,
            gate: Mutex::new(gate),
            snapshot: Mutex::default(),
        });
        let accounts = Accounts::new(Arc::clone(&fake) as Arc<dyn AccountsService>);
        let (events_tx, events) = broadcast::channel(16);
        let (clients, clients_rx) = watch::channel(0_usize);
        let (shutdown, shutdown_rx) = watch::channel(false);
        accounts.start_driver(
            events_tx.clone(),
            Watches {
                clients: clients_rx,
                shutdown: shutdown.subscribe(),
            },
        );
        let task = spawn(Arc::clone(&accounts), events_tx.clone(), shutdown_rx);
        Rig {
            accounts,
            fake,
            ticked,
            events,
            _events_tx: events_tx,
            _clients: clients,
            shutdown,
            task,
        }
    }

    impl Rig {
        fn ticks(&self) -> usize {
            self.fake.count.load(Ordering::SeqCst)
        }

        /// Wait up to `guard` for the next tick's signal.
        async fn next_tick_within(&mut self, guard: Duration) {
            tokio::time::timeout(guard, self.ticked.recv())
                .await
                .expect("a tick came")
                .expect("the fake is alive");
        }

        async fn next_tick(&mut self) {
            self.next_tick_within(GUARD).await;
        }

        /// The next broadcast `pick` answers for, skipping the others.
        async fn next_matching<T>(&mut self, pick: impl Fn(StateEvent) -> Option<T>) -> T {
            loop {
                let event = tokio::time::timeout(GUARD, self.events.recv())
                    .await
                    .expect("a broadcast came")
                    .expect("the channel is open");
                if let Some(found) = pick(event) {
                    return found;
                }
            }
        }

        async fn stop(self) {
            self.shutdown.send_replace(true);
            self.accounts.join_driver(GUARD).await;
            self.task.abort();
        }
    }

    /// Let the tasks run; yields only, so a passing run takes no wall time.
    async fn settle() {
        for _ in 0..1_000 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn no_tick_runs_while_auto_switch_is_off() {
        // Every call answers Off: the service ran no tick.
        let mut rig = rig(Vec::new(), None);
        rig.next_tick().await;

        tokio::time::sleep(A_DAY).await;
        settle().await;

        assert!(
            matches!(
                rig.events.try_recv(),
                Err(broadcast::error::TryRecvError::Empty)
            ),
            "an engine that is off broadcasts nothing and asks no refresh"
        );
        rig.stop().await;
    }

    #[tokio::test(start_paused = true)]
    async fn an_off_engine_rechecks_after_the_poll_floor() {
        let mut rig = rig(Vec::new(), None);
        rig.next_tick().await;

        tokio::time::sleep(POLL_FLOOR.saturating_sub(Duration::from_secs(1))).await;
        settle().await;
        assert_eq!(rig.ticks(), 1, "not before the poll floor");

        tokio::time::sleep(Duration::from_secs(2)).await;
        rig.next_tick().await;
        assert_eq!(rig.ticks(), 2, "an off engine asks again after the floor");
        rig.stop().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_settings_write_wakes_the_engine() {
        let mut rig = rig(vec![Ok(EngineTick::Off), Ok(ran(Vec::new(), A_DAY))], None);
        rig.next_tick().await;
        settle().await;
        assert_eq!(rig.ticks(), 1);

        rig.accounts.engine_wake.notify_one();
        rig.next_tick().await;

        assert_eq!(rig.ticks(), 2, "the wake ticked at once");
        rig.stop().await;
    }

    #[tokio::test(start_paused = true)]
    async fn the_loop_sleeps_the_ticks_delay() {
        let delay = Duration::from_secs(300);
        let mut rig = rig(
            vec![Ok(ran(Vec::new(), delay)), Ok(ran(Vec::new(), A_DAY))],
            None,
        );
        rig.next_tick().await;

        tokio::time::sleep(delay.saturating_sub(Duration::from_secs(1))).await;
        settle().await;
        assert_eq!(rig.ticks(), 1, "not before the delay the tick asked for");

        tokio::time::sleep(Duration::from_secs(2)).await;
        rig.next_tick().await;
        assert_eq!(rig.ticks(), 2);
        rig.stop().await;
    }

    #[tokio::test(start_paused = true)]
    async fn events_are_broadcast_as_account_events() {
        let rows = vec![
            AccountEventRow::Switched {
                from: Some(1),
                to: 2,
                trigger: "at-limit".to_owned(),
            },
            AccountEventRow::Quarantined { number: 3 },
        ];
        let mut rig = rig(vec![Ok(ran(rows, A_DAY))], None);
        rig.next_tick().await;

        let pick = |event| match event {
            StateEvent::AccountEvent { event } => Some(event),
            _ => None,
        };
        let first = rig.next_matching(pick).await;
        let second = rig.next_matching(pick).await;

        assert_eq!(
            first,
            AccountEventKind::Switched {
                from: Some(1),
                to: 2,
                trigger: "at-limit".to_owned(),
            }
        );
        assert_eq!(second, AccountEventKind::Quarantined { number: 3 });
        rig.stop().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_paused_reason_reaches_the_snapshot_refresh() {
        let reason = "another engine holds the lease";
        let mut rig = rig(
            vec![Ok(EngineTick::Ran {
                events: Vec::new(),
                delay: A_DAY,
                paused: Some(reason.to_owned()),
                halted: None,
            })],
            None,
        );
        rig.next_tick().await;

        let paused = rig
            .next_matching(|event| match event {
                StateEvent::Accounts {
                    auto: Some(auto), ..
                } => Some(auto.paused_reason),
                _ => None,
            })
            .await;

        assert_eq!(paused.as_deref(), Some(reason));
        let cached = rig.accounts.cached().expect("the refresh is cached");
        assert_eq!(
            cached.auto.and_then(|auto| auto.paused_reason).as_deref(),
            Some(reason)
        );
        rig.stop().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_halted_tick_waits_for_a_wake() {
        let message = "a failed rollback halted auto-switch";
        let mut rig = rig(
            vec![
                Ok(EngineTick::Ran {
                    events: vec![AccountEventRow::Halted {
                        message: message.to_owned(),
                    }],
                    delay: Duration::from_secs(60),
                    paused: Some(message.to_owned()),
                    halted: Some(message.to_owned()),
                }),
                Ok(ran(Vec::new(), A_DAY)),
            ],
            None,
        );
        rig.next_tick().await;
        let halted = rig
            .next_matching(|event| match event {
                StateEvent::AccountEvent { event } => Some(event),
                _ => None,
            })
            .await;
        assert_eq!(
            halted,
            AccountEventKind::Halted {
                message: message.to_owned()
            }
        );

        tokio::time::sleep(A_DAY).await;
        settle().await;
        assert_eq!(rig.ticks(), 1, "a halted engine is not ticked on its delay");

        rig.accounts.engine_wake.notify_one();
        rig.next_tick().await;
        assert_eq!(rig.ticks(), 2, "a wake ticks it again");
        rig.stop().await;
    }

    #[tokio::test(start_paused = true)]
    async fn the_engine_ticks_with_no_client_connected() {
        let delay = Duration::from_secs(60);
        let mut rig = rig(
            vec![Ok(ran(Vec::new(), delay)), Ok(ran(Vec::new(), delay))],
            None,
        );

        rig.next_tick().await;
        rig.next_tick().await;
        rig.next_tick().await;

        assert_eq!(rig.ticks(), 3, "no client is connected, and it ticks");
        rig.stop().await;
    }

    /// On the real clock: the tick blocks a real thread, and the waits are
    /// hang guards on its events.
    #[tokio::test]
    async fn shutdown_ends_the_loop_within_the_join_bound() {
        let (release, gate) = std::sync::mpsc::channel();
        let mut rig = rig(vec![Ok(ran(Vec::new(), A_DAY))], Some(gate));
        rig.next_tick_within(HANG).await;

        rig.shutdown.send_replace(true);
        tokio::time::timeout(HANG, &mut rig.task)
            .await
            .expect("hung: the engine task did not end while its tick was in flight")
            .expect("the engine task did not panic");

        // The gate still holds the tick: the task ended without waiting
        // for it.
        release
            .send(())
            .expect("the tick was still held when the task ended");
        rig.accounts.join_driver(HANG).await;
        assert_eq!(rig.ticks(), 1, "no tick after shutdown");
    }

    /// On the real clock: the tick blocks a real thread, and the waits are
    /// hang guards on its events, far inside the day the tick asks to wait.
    #[tokio::test]
    async fn a_wake_during_a_tick_is_not_lost() {
        let (release, gate) = std::sync::mpsc::channel();
        let mut rig = rig(
            vec![Ok(ran(Vec::new(), A_DAY)), Ok(ran(Vec::new(), A_DAY))],
            Some(gate),
        );
        rig.next_tick_within(HANG).await;

        rig.accounts.engine_wake.notify_one();
        release.send(()).expect("the first tick is waiting");
        rig.next_tick_within(HANG).await;

        assert_eq!(
            rig.ticks(),
            2,
            "the wake during the first tick ticked again"
        );
        release.send(()).expect("the second tick is waiting");
        rig.stop().await;
    }
}
