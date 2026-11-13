//! The performance sample service: the machine's CPU and memory, read every
//! [`INTERVAL`] while at least one connection subscribes, one sample per
//! tick sent to every subscribed connection.
//!
//! A connection is a watcher while it is open and its last `WatchPerf` said
//! on. With no watcher the ticker parks and reads nothing; the first watcher
//! gets a read at once, whose sample has no CPU figure, since CPU is
//! measured between two reads. The previous read is forgotten when the last
//! watcher leaves, so no figure spans an idle gap.

pub(crate) mod sample;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use protocol::DaemonMessage;
use tokio::sync::watch;
use tracing::warn;

use self::sample::{RawTable, build_sample};
use crate::sync::lock;
use crate::transcript_log::Outbox;

/// How often the machine is read while a connection subscribes.
pub(crate) const INTERVAL: Duration = Duration::from_secs(2);

/// Where the service reads the machine from.
pub(crate) trait PerfSource: Send + Sync {
    /// One read of the machine, `None` when it failed: the tick sends
    /// nothing and the previous read stays. It blocks, so the service calls
    /// it on the blocking pool.
    fn read(&self) -> Option<RawTable>;
}

/// The performance sample service, shared by the hub, every connection and
/// the ticker.
#[derive(Clone)]
pub(crate) struct Perf {
    inner: Arc<Inner>,
}

struct Inner {
    /// `None` when this host has no source: subscribing is accepted, but
    /// nothing ticks and nothing is sent.
    source: Option<Arc<dyn PerfSource>>,
    state: Mutex<State>,
    /// The number of watchers, which wakes and parks the ticker; changed
    /// only under [`Inner::state`]'s lock.
    watcher_count: watch::Sender<usize>,
}

#[derive(Default)]
struct State {
    watchers: HashMap<u64, Outbox>,
    /// The last read that made a sample, which the next one measures CPU
    /// against.
    prev: Option<RawTable>,
}

impl Perf {
    /// The service reading `source`, or none.
    pub(crate) fn new(source: Option<Arc<dyn PerfSource>>) -> Self {
        Self {
            inner: Arc::new(Inner {
                source,
                state: Mutex::new(State::default()),
                watcher_count: watch::Sender::new(0),
            }),
        }
    }

    /// Make `connection` a watcher sending on `outbox`; a connection that
    /// already is keeps one subscription, on the newer outbox.
    pub(crate) fn subscribe(&self, connection: u64, outbox: Outbox) {
        let mut state = lock(&self.inner.state);
        state.watchers.insert(connection, outbox);
        self.watchers_changed(&mut state);
    }

    /// Stop `connection` watching; a connection that was not is left alone.
    pub(crate) fn unsubscribe(&self, connection: u64) {
        let mut state = lock(&self.inner.state);
        if state.watchers.remove(&connection).is_some() {
            self.watchers_changed(&mut state);
        }
    }

    /// A guard that stops `connection` watching when dropped, so a closed
    /// connection is never left a watcher.
    pub(crate) fn watch_guard(&self, connection: u64) -> WatchGuard {
        WatchGuard {
            perf: self.clone(),
            connection,
        }
    }

    /// The number of watchers, as the ticker sees it.
    #[cfg(test)]
    pub(crate) fn watcher_count(&self) -> watch::Receiver<usize> {
        self.inner.watcher_count.subscribe()
    }

    /// Start the ticker for the daemon's life: it parks while no connection
    /// watches, reads at once when one starts, then every [`INTERVAL`].
    /// `None`, starting nothing, when the service has no source.
    pub(crate) fn spawn_ticker(&self) -> Option<tokio::task::JoinHandle<()>> {
        let source = self.inner.source.clone()?;
        let perf = self.clone();
        let mut count = self.inner.watcher_count.subscribe();
        Some(tokio::spawn(async move {
            loop {
                if count.wait_for(|watchers| *watchers > 0).await.is_err() {
                    return;
                }
                let mut interval = tokio::time::interval(INTERVAL);
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                // The first tick completes at once: the first watcher's read.
                loop {
                    tokio::select! {
                        biased;
                        changed = count.changed() => {
                            if changed.is_err() {
                                return;
                            }
                            if *count.borrow_and_update() == 0 {
                                break;
                            }
                        }
                        _ = interval.tick() => perf.tick(&source).await,
                    }
                }
            }
        }))
    }

    /// One tick: read the machine on the blocking pool, with no lock held,
    /// and send each watcher the one sample built from it; a watcher whose
    /// connection is gone is dropped. Reads nothing with no watcher.
    async fn tick(&self, source: &Arc<dyn PerfSource>) {
        if lock(&self.inner.state).watchers.is_empty() {
            return;
        }
        let source = Arc::clone(source);
        let table = match tokio::task::spawn_blocking(move || source.read()).await {
            Ok(Some(table)) => table,
            Ok(None) => return,
            Err(error) => {
                warn!(%error, "perf: the read stopped; skipping this tick");
                return;
            }
        };
        let mut state = lock(&self.inner.state);
        // The last watcher may have left while the read ran.
        if state.watchers.is_empty() {
            return;
        }
        let interval_ms = u32::try_from(INTERVAL.as_millis()).unwrap_or(u32::MAX);
        let sample = build_sample(&table, state.prev.as_ref(), Utc::now(), interval_ms);
        state.prev = Some(table);
        state.watchers.retain(|connection, outbox| {
            let message = DaemonMessage::Perf {
                sample: sample.clone(),
            };
            let sent = outbox.send(message).is_ok();
            if !sent {
                warn!(
                    connection,
                    "perf: a watcher's connection is gone; it stops watching"
                );
            }
            sent
        });
        self.watchers_changed(&mut state);
    }

    /// Publish the watcher count after the watchers changed. With none left
    /// the previous read is forgotten, so the next first sample has no CPU.
    fn watchers_changed(&self, state: &mut State) {
        if state.watchers.is_empty() {
            state.prev = None;
        }
        let watchers = state.watchers.len();
        self.inner.watcher_count.send_if_modified(|count| {
            let changed = *count != watchers;
            *count = watchers;
            changed
        });
    }
}

/// Stops its connection watching when dropped.
pub(crate) struct WatchGuard {
    perf: Perf,
    connection: u64,
}

impl Drop for WatchGuard {
    fn drop(&mut self) {
        self.perf.unsubscribe(self.connection);
    }
}
