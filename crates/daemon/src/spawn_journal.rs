//! A bounded, in-memory journal of the spawn requests each device sent.
//!
//! A phone on a flaky link resends a `SpawnSession` it never heard back
//! about. The journal keeps each request id's outcome for ten minutes, so a
//! repeat makes no second session and is answered with the first one's
//! outcome instead. Nothing is persisted: a daemon restart forgets every
//! entry, and a request sent before it is spawned again.
//!
//! A repeat can arrive while the first send still runs. It is not answered
//! then and there: the journal holds its connection as a waiter on the entry,
//! and the connection that finishes the first spawn answers every waiter with
//! the outcome it recorded.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use protocol::DaemonMessage;
use tokio::sync::mpsc::{UnboundedSender, WeakUnboundedSender};

/// How long an entry answers repeats for, from when it was recorded.
pub(crate) const TTL: Duration = Duration::from_mins(10);

/// The most entries one device keeps; the oldest goes first when full.
pub(crate) const PER_DEVICE: usize = 256;

/// What a journalled request came to.
#[derive(Clone, Debug)]
pub(crate) enum Outcome {
    /// The first request is still running. A repeat is answered with nothing
    /// now: its connection waits on the entry, and hears the outcome when the
    /// spawn finishes.
    Pending,
    /// The request made `session_id`.
    Spawned { session_id: String },
    /// The request failed with `error`, which answers its repeats unchanged.
    Failed { error: Arc<anyhow::Error> },
}

/// Whether [`SpawnJournal::begin`] recorded a new request or found a repeat.
pub(crate) enum Begin {
    New,
    Repeat(Outcome),
}

/// One journalled request.
struct Entry {
    request_id: String,
    recorded_at: Instant,
    outcome: Outcome,
    /// Every connection that repeated this request while it ran, in the order
    /// they did; each is answered when the outcome is recorded. Held weakly:
    /// a waiter must never keep its connection's send pump open, so a
    /// connection that drops while the spawn still runs closes at once, and
    /// the journal simply finds nothing to upgrade for it at the finish.
    waiters: Vec<WeakUnboundedSender<DaemonMessage>>,
}

/// Register `waiter` on `entry` when the request it repeats still runs; a
/// repeat of a finished request waits on nothing.
fn await_outcome(entry: &mut Entry, outcome: &Outcome, waiter: &UnboundedSender<DaemonMessage>) {
    if matches!(outcome, Outcome::Pending) {
        entry.waiters.push(waiter.downgrade());
    }
}

/// One device's entries, oldest first.
#[derive(Default)]
struct DeviceJournal {
    entries: VecDeque<Entry>,
}

impl DeviceJournal {
    /// Drop the entries that outlived [`TTL`] as of `now`, answering whether
    /// any remain.
    fn expire(&mut self, now: Instant) -> bool {
        while let Some(front) = self.entries.front() {
            if now.saturating_duration_since(front.recorded_at) < TTL {
                break;
            }
            self.entries.pop_front();
        }
        !self.entries.is_empty()
    }

    fn entry_mut(&mut self, request_id: &str) -> Option<&mut Entry> {
        self.entries
            .iter_mut()
            .find(|entry| entry.request_id == request_id)
    }
}

/// The spawn requests each device sent, by request id, bounded by [`TTL`] and
/// [`PER_DEVICE`].
#[derive(Default)]
pub(crate) struct SpawnJournal {
    devices: Mutex<HashMap<String, DeviceJournal>>,
}

impl SpawnJournal {
    /// Record `device`'s request `request_id` as running, or answer a repeat
    /// with what its first send came to. The lookup and the record happen in
    /// one lock hold, so two concurrent sends of one request id make one
    /// [`Begin::New`]. A repeat that finds the first send still running is
    /// registered as `waiter`, which [`Self::finish`] hands back to be
    /// answered; a new entry holds no waiter, since the connection that
    /// began it hears its own spawn on its own connection.
    #[must_use]
    pub(crate) fn begin(
        &self,
        device: &str,
        request_id: &str,
        now: Instant,
        waiter: &UnboundedSender<DaemonMessage>,
    ) -> Begin {
        let mut devices = crate::sync::lock(&self.devices);
        let journal = devices.entry(device.to_owned()).or_default();
        journal.expire(now);
        if let Some(entry) = journal.entry_mut(request_id) {
            let outcome = entry.outcome.clone();
            await_outcome(entry, &outcome, waiter);
            return Begin::Repeat(outcome);
        }
        if journal.entries.len() >= PER_DEVICE {
            journal.entries.pop_front();
        }
        journal.entries.push_back(Entry {
            request_id: request_id.to_owned(),
            recorded_at: now,
            outcome: Outcome::Pending,
            waiters: Vec::new(),
        });
        Begin::New
    }

    /// What `device`'s request `request_id` came to, if the journal holds it.
    /// The entry expires exactly as [`Self::begin`]'s does, and a request
    /// whose first send still runs registers `waiter` as [`Self::begin`] does
    /// for a repeat; nothing is recorded for a request the journal does not
    /// hold.
    #[must_use]
    pub(crate) fn peek(
        &self,
        device: &str,
        request_id: &str,
        now: Instant,
        waiter: &UnboundedSender<DaemonMessage>,
    ) -> Option<Outcome> {
        let mut devices = crate::sync::lock(&self.devices);
        let journal = devices.get_mut(device)?;
        journal.expire(now);
        let entry = journal.entry_mut(request_id)?;
        let outcome = entry.outcome.clone();
        await_outcome(entry, &outcome, waiter);
        Some(outcome)
    }

    /// Record what `device`'s request `request_id` came to and hand back the
    /// connections that repeated it while it ran, drained in the same lock
    /// hold that records the outcome: a repeat lands either before this drain,
    /// and is answered from the returned senders, or after it, and reads the
    /// outcome itself. Waiters are held weakly, so one whose connection has
    /// gone is skipped rather than answered. An entry that already expired, or
    /// one [`Self::begin`] never made, is ignored and yields no waiters.
    pub(crate) fn finish(
        &self,
        device: &str,
        request_id: &str,
        outcome: Outcome,
        now: Instant,
    ) -> Vec<UnboundedSender<DaemonMessage>> {
        let mut devices = crate::sync::lock(&self.devices);
        let mut gone = false;
        let mut waiters = Vec::new();
        if let Some(journal) = devices.get_mut(device) {
            if !journal.expire(now) {
                gone = true;
            } else if let Some(entry) = journal.entry_mut(request_id) {
                entry.outcome = outcome;
                waiters = std::mem::take(&mut entry.waiters)
                    .into_iter()
                    .filter_map(|waiter| waiter.upgrade())
                    .collect();
            }
        }
        // An expiring sweep is the only way a device's list goes empty, so
        // drop the device with it and keep the map bounded too.
        if gone {
            devices.remove(device);
        }
        waiters
    }

    /// How many entries `device` holds, expired ones included.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn entry_count(&self, device: &str) -> usize {
        crate::sync::lock(&self.devices)
            .get(device)
            .map_or(0, |journal| journal.entries.len())
    }

    /// What `device`'s request `request_id` was recorded as, if at all.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn outcome_of(&self, device: &str, request_id: &str) -> Option<Outcome> {
        let mut devices = crate::sync::lock(&self.devices);
        devices
            .get_mut(device)
            .and_then(|journal| journal.entry_mut(request_id))
            .map(|entry| entry.outcome.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sender whose receiver goes at once: enough for a `begin` called where
    /// the caller expects to register no waiter.
    fn throwaway() -> UnboundedSender<DaemonMessage> {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        tx
    }

    #[test]
    fn a_new_request_is_new() {
        let journal = SpawnJournal::default();
        let now = Instant::now();
        assert!(matches!(
            journal.begin("d", "rq-1", now, &throwaway()),
            Begin::New
        ));
        assert!(matches!(
            journal.outcome_of("d", "rq-1"),
            Some(Outcome::Pending)
        ));
    }

    #[test]
    fn a_repeat_while_pending_is_pending() {
        let journal = SpawnJournal::default();
        let now = Instant::now();
        assert!(matches!(
            journal.begin("d", "rq-1", now, &throwaway()),
            Begin::New
        ));
        assert!(matches!(
            journal.begin("d", "rq-1", now, &throwaway()),
            Begin::Repeat(Outcome::Pending)
        ));
        assert_eq!(journal.entry_count("d"), 1, "a repeat records nothing");
    }

    #[test]
    fn a_repeat_after_spawn_returns_the_session() {
        let journal = SpawnJournal::default();
        let now = Instant::now();
        assert!(matches!(
            journal.begin("d", "rq-1", now, &throwaway()),
            Begin::New
        ));
        journal.finish(
            "d",
            "rq-1",
            Outcome::Spawned {
                session_id: "s1".to_owned(),
            },
            now,
        );
        assert!(matches!(
            journal.begin("d", "rq-1", now, &throwaway()),
            Begin::Repeat(Outcome::Spawned { session_id }) if session_id == "s1"
        ));
    }

    #[test]
    fn a_repeat_after_a_failure_returns_the_failure() {
        let journal = SpawnJournal::default();
        let now = Instant::now();
        assert!(matches!(
            journal.begin("d", "rq-1", now, &throwaway()),
            Begin::New
        ));
        journal.finish(
            "d",
            "rq-1",
            Outcome::Failed {
                error: Arc::new(anyhow::anyhow!("the tracer did not start")),
            },
            now,
        );
        assert!(matches!(
            journal.begin("d", "rq-1", now, &throwaway()),
            Begin::Repeat(Outcome::Failed { error })
                if format!("{error:#}") == "the tracer did not start"
        ));
    }

    #[test]
    fn an_entry_expires_after_ten_minutes() {
        let journal = SpawnJournal::default();
        let start = Instant::now();
        assert!(matches!(
            journal.begin("d", "rq-1", start, &throwaway()),
            Begin::New
        ));
        let nine_minutes = start + Duration::from_mins(9);
        assert!(matches!(
            journal.begin("d", "rq-1", nine_minutes, &throwaway()),
            Begin::Repeat(Outcome::Pending)
        ));
        let ten_minutes = start + TTL;
        assert!(
            matches!(
                journal.begin("d", "rq-1", ten_minutes, &throwaway()),
                Begin::New
            ),
            "ten minutes after it was recorded it is gone"
        );
        assert_eq!(
            journal.entry_count("d"),
            1,
            "the expired entry gave way to the new one"
        );
    }

    #[test]
    fn devices_do_not_share_entries() {
        let journal = SpawnJournal::default();
        let now = Instant::now();
        assert!(matches!(
            journal.begin("a", "rq-1", now, &throwaway()),
            Begin::New
        ));
        assert!(matches!(
            journal.begin("b", "rq-1", now, &throwaway()),
            Begin::New
        ));
        journal.finish(
            "a",
            "rq-1",
            Outcome::Spawned {
                session_id: "s1".to_owned(),
            },
            now,
        );
        assert!(matches!(
            journal.begin("b", "rq-1", now, &throwaway()),
            Begin::Repeat(Outcome::Pending)
        ));
    }

    #[test]
    fn the_oldest_entry_goes_when_a_device_is_full() {
        let journal = SpawnJournal::default();
        let now = Instant::now();
        for index in 0..PER_DEVICE {
            assert!(matches!(
                journal.begin("d", &format!("rq-{index}"), now, &throwaway()),
                Begin::New
            ));
        }
        assert!(matches!(
            journal.begin("d", "rq-last", now, &throwaway()),
            Begin::New
        ));
        assert_eq!(journal.entry_count("d"), PER_DEVICE, "the cap holds");
        assert!(
            matches!(
                journal.begin("d", "rq-1", now, &throwaway()),
                Begin::Repeat(_)
            ),
            "the second-oldest is still held"
        );
        assert!(
            matches!(journal.begin("d", "rq-0", now, &throwaway()), Begin::New),
            "the oldest went"
        );
    }

    #[test]
    fn a_repeat_while_pending_is_handed_the_outcome_when_it_finishes() {
        let journal = SpawnJournal::default();
        let now = Instant::now();
        let (first_tx, mut first_rx) = tokio::sync::mpsc::unbounded_channel();
        let (repeat_tx, mut repeat_rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(matches!(
            journal.begin("d", "rq-1", now, &first_tx),
            Begin::New
        ));
        assert!(matches!(
            journal.begin("d", "rq-1", now, &repeat_tx),
            Begin::Repeat(Outcome::Pending)
        ));
        let waiters = journal.finish(
            "d",
            "rq-1",
            Outcome::Spawned {
                session_id: "s1".to_owned(),
            },
            now,
        );
        assert_eq!(waiters.len(), 1, "the repeat waits on the outcome");
        let sent = waiters[0].send(DaemonMessage::Error {
            message: "done".to_owned(),
            request_id: None,
            code: None,
        });
        assert!(sent.is_ok(), "the repeat's connection is still open");
        assert!(matches!(
            repeat_rx.try_recv(),
            Ok(DaemonMessage::Error { message, .. }) if message == "done"
        ));
        assert!(
            first_rx.try_recv().is_err(),
            "the first sender is no waiter of its own entry"
        );
    }

    #[test]
    fn a_waiter_does_not_keep_its_connection_open() {
        let journal = SpawnJournal::default();
        let now = Instant::now();
        let (first_tx, _first_rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(matches!(
            journal.begin("d", "rq-1", now, &first_tx),
            Begin::New
        ));
        let (repeat_tx, mut repeat_rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(matches!(
            journal.begin("d", "rq-1", now, &repeat_tx),
            Begin::Repeat(Outcome::Pending)
        ));
        drop(repeat_tx);
        assert!(
            matches!(
                repeat_rx.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
            ),
            "the journal holds the waiter weakly, so a dropped connection closes"
        );
        assert!(
            journal
                .finish(
                    "d",
                    "rq-1",
                    Outcome::Spawned {
                        session_id: "s1".to_owned(),
                    },
                    now,
                )
                .is_empty(),
            "a waiter whose connection is gone is not answered"
        );
    }

    #[test]
    fn a_repeat_after_the_outcome_registers_no_waiter() {
        let journal = SpawnJournal::default();
        let now = Instant::now();
        assert!(matches!(
            journal.begin("d", "rq-1", now, &throwaway()),
            Begin::New
        ));
        journal.finish(
            "d",
            "rq-1",
            Outcome::Spawned {
                session_id: "s1".to_owned(),
            },
            now,
        );
        assert!(matches!(
            journal.begin("d", "rq-1", now, &throwaway()),
            Begin::Repeat(Outcome::Spawned { session_id }) if session_id == "s1"
        ));
        assert!(
            journal
                .finish(
                    "d",
                    "rq-1",
                    Outcome::Spawned {
                        session_id: "s1".to_owned(),
                    },
                    now,
                )
                .is_empty(),
            "a repeat of a finished request waits on nothing"
        );
    }

    #[test]
    fn peek_of_an_unknown_request_records_nothing() {
        let journal = SpawnJournal::default();
        let now = Instant::now();
        assert!(journal.peek("d", "rq-9", now, &throwaway()).is_none());
        assert_eq!(journal.entry_count("d"), 0, "a peek records nothing");
    }

    #[test]
    fn peek_of_a_pending_request_registers_its_waiter() {
        let journal = SpawnJournal::default();
        let now = Instant::now();
        assert!(matches!(
            journal.begin("d", "rq-1", now, &throwaway()),
            Begin::New
        ));
        let (repeat_tx, mut repeat_rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(matches!(
            journal.peek("d", "rq-1", now, &repeat_tx),
            Some(Outcome::Pending)
        ));
        let waiters = journal.finish(
            "d",
            "rq-1",
            Outcome::Spawned {
                session_id: "s1".to_owned(),
            },
            now,
        );
        assert_eq!(waiters.len(), 1, "the peek registered its waiter");
        let sent = waiters[0].send(DaemonMessage::Error {
            message: "done".to_owned(),
            request_id: None,
            code: None,
        });
        assert!(sent.is_ok(), "the peek's connection is still open");
        assert!(matches!(
            repeat_rx.try_recv(),
            Ok(DaemonMessage::Error { message, .. }) if message == "done"
        ));
    }

    #[test]
    fn finish_of_an_expired_entry_returns_no_waiters() {
        let journal = SpawnJournal::default();
        let start = Instant::now();
        assert!(matches!(
            journal.begin("d", "rq-1", start, &throwaway()),
            Begin::New
        ));
        assert!(matches!(
            journal.begin("d", "rq-1", start, &throwaway()),
            Begin::Repeat(Outcome::Pending)
        ));
        assert!(
            journal
                .finish(
                    "d",
                    "rq-1",
                    Outcome::Spawned {
                        session_id: "s1".to_owned(),
                    },
                    start + TTL,
                )
                .is_empty(),
            "an expired entry has no waiters to answer"
        );
        assert_eq!(journal.entry_count("d"), 0, "the entry expired away");
    }
}
