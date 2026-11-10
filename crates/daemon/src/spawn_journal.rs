//! A bounded, in-memory journal of the spawn requests each device sent.
//!
//! A phone on a flaky link resends a `SpawnSession` it never heard back
//! about. The journal keeps each request id's outcome for ten minutes, so a
//! repeat makes no second session and is answered with the first one's
//! outcome instead. Nothing is persisted: a daemon restart forgets every
//! entry, and a request sent before it is spawned again.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long an entry answers repeats for, from when it was recorded.
pub(crate) const TTL: Duration = Duration::from_mins(10);

/// The most entries one device keeps; the oldest goes first when full.
pub(crate) const PER_DEVICE: usize = 256;

/// What a journalled request came to.
#[derive(Clone, Debug)]
pub(crate) enum Outcome {
    /// The first request is still running: a repeat is answered with nothing.
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

    fn entry(&self, request_id: &str) -> Option<&Entry> {
        self.entries
            .iter()
            .find(|entry| entry.request_id == request_id)
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
    /// [`Begin::New`].
    #[must_use]
    pub(crate) fn begin(&self, device: &str, request_id: &str, now: Instant) -> Begin {
        let mut devices = crate::sync::lock(&self.devices);
        let journal = devices.entry(device.to_owned()).or_default();
        journal.expire(now);
        if let Some(entry) = journal.entry(request_id) {
            return Begin::Repeat(entry.outcome.clone());
        }
        if journal.entries.len() >= PER_DEVICE {
            journal.entries.pop_front();
        }
        journal.entries.push_back(Entry {
            request_id: request_id.to_owned(),
            recorded_at: now,
            outcome: Outcome::Pending,
        });
        Begin::New
    }

    /// Record what `device`'s request `request_id` came to; an entry that
    /// already expired, or one [`Self::begin`] never made, is ignored.
    pub(crate) fn finish(&self, device: &str, request_id: &str, outcome: Outcome, now: Instant) {
        let mut devices = crate::sync::lock(&self.devices);
        let mut gone = false;
        if let Some(journal) = devices.get_mut(device) {
            if !journal.expire(now) {
                gone = true;
            } else if let Some(entry) = journal.entry_mut(request_id) {
                entry.outcome = outcome;
            }
        }
        // An expiring sweep is the only way a device's list goes empty, so
        // drop the device with it and keep the map bounded too.
        if gone {
            devices.remove(device);
        }
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
        crate::sync::lock(&self.devices)
            .get(device)
            .and_then(|journal| journal.entry(request_id))
            .map(|entry| entry.outcome.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_request_is_new() {
        let journal = SpawnJournal::default();
        let now = Instant::now();
        assert!(matches!(journal.begin("d", "rq-1", now), Begin::New));
        assert!(matches!(
            journal.outcome_of("d", "rq-1"),
            Some(Outcome::Pending)
        ));
    }

    #[test]
    fn a_repeat_while_pending_is_pending() {
        let journal = SpawnJournal::default();
        let now = Instant::now();
        assert!(matches!(journal.begin("d", "rq-1", now), Begin::New));
        assert!(matches!(
            journal.begin("d", "rq-1", now),
            Begin::Repeat(Outcome::Pending)
        ));
        assert_eq!(journal.entry_count("d"), 1, "a repeat records nothing");
    }

    #[test]
    fn a_repeat_after_spawn_returns_the_session() {
        let journal = SpawnJournal::default();
        let now = Instant::now();
        assert!(matches!(journal.begin("d", "rq-1", now), Begin::New));
        journal.finish(
            "d",
            "rq-1",
            Outcome::Spawned {
                session_id: "s1".to_owned(),
            },
            now,
        );
        assert!(matches!(
            journal.begin("d", "rq-1", now),
            Begin::Repeat(Outcome::Spawned { session_id }) if session_id == "s1"
        ));
    }

    #[test]
    fn a_repeat_after_a_failure_returns_the_failure() {
        let journal = SpawnJournal::default();
        let now = Instant::now();
        assert!(matches!(journal.begin("d", "rq-1", now), Begin::New));
        journal.finish(
            "d",
            "rq-1",
            Outcome::Failed {
                error: Arc::new(anyhow::anyhow!("the tracer did not start")),
            },
            now,
        );
        assert!(matches!(
            journal.begin("d", "rq-1", now),
            Begin::Repeat(Outcome::Failed { error })
                if format!("{error:#}") == "the tracer did not start"
        ));
    }

    #[test]
    fn an_entry_expires_after_ten_minutes() {
        let journal = SpawnJournal::default();
        let start = Instant::now();
        assert!(matches!(journal.begin("d", "rq-1", start), Begin::New));
        let nine_minutes = start + Duration::from_mins(9);
        assert!(matches!(
            journal.begin("d", "rq-1", nine_minutes),
            Begin::Repeat(Outcome::Pending)
        ));
        let ten_minutes = start + TTL;
        assert!(
            matches!(journal.begin("d", "rq-1", ten_minutes), Begin::New),
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
        assert!(matches!(journal.begin("a", "rq-1", now), Begin::New));
        assert!(matches!(journal.begin("b", "rq-1", now), Begin::New));
        journal.finish(
            "a",
            "rq-1",
            Outcome::Spawned {
                session_id: "s1".to_owned(),
            },
            now,
        );
        assert!(matches!(
            journal.begin("b", "rq-1", now),
            Begin::Repeat(Outcome::Pending)
        ));
    }

    #[test]
    fn the_oldest_entry_goes_when_a_device_is_full() {
        let journal = SpawnJournal::default();
        let now = Instant::now();
        for index in 0..PER_DEVICE {
            assert!(matches!(
                journal.begin("d", &format!("rq-{index}"), now),
                Begin::New
            ));
        }
        assert!(matches!(journal.begin("d", "rq-last", now), Begin::New));
        assert_eq!(journal.entry_count("d"), PER_DEVICE, "the cap holds");
        assert!(
            matches!(journal.begin("d", "rq-1", now), Begin::Repeat(_)),
            "the second-oldest is still held"
        );
        assert!(
            matches!(journal.begin("d", "rq-0", now), Begin::New),
            "the oldest went"
        );
    }
}
