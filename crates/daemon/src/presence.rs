//! Whether the owner is at the desk: the live connections whose client last
//! reported its window focused on a machine with recent input. Nothing is
//! stored, and a connection that closes stops counting (RT-1160).

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use tokio::sync::watch;

/// The connections reporting the owner active. A leaf lock: nothing else is
/// held while it is taken.
pub(crate) struct Presence {
    /// The connections whose last report said active.
    active: Mutex<HashSet<u64>>,
    /// The desk's answer, flipped only when `active` goes between empty and
    /// non-empty, so the push task can deliver what it held.
    at_desk: watch::Sender<bool>,
}

impl Default for Presence {
    fn default() -> Self {
        Self {
            active: Mutex::default(),
            at_desk: watch::channel(false).0,
        }
    }
}

impl Presence {
    /// Record `connection`'s latest answer: it counts while `active`, and
    /// stops counting as soon as it reports otherwise.
    pub(crate) fn report(&self, connection: u64, active: bool) {
        let (was, now) = {
            let mut connections = crate::sync::lock(&self.active);
            let was = !connections.is_empty();
            if active {
                connections.insert(connection);
            } else {
                connections.remove(&connection);
            }
            (was, !connections.is_empty())
        };
        self.publish(was, now);
    }

    /// Stop counting `connection`, which ended.
    pub(crate) fn forget(&self, connection: u64) {
        let (was, now) = {
            let mut connections = crate::sync::lock(&self.active);
            let was = !connections.is_empty();
            connections.remove(&connection);
            (was, !connections.is_empty())
        };
        self.publish(was, now);
    }

    /// Whether any live connection last reported the owner active.
    #[must_use]
    pub(crate) fn at_desk(&self) -> bool {
        !crate::sync::lock(&self.active).is_empty()
    }

    /// The desk's answer as it changes; the value is what [`Presence::at_desk`]
    /// says.
    pub(crate) fn subscribe(&self) -> watch::Receiver<bool> {
        self.at_desk.subscribe()
    }

    /// Tell subscribers when the answer changed; sends only then, with the
    /// lock released.
    fn publish(&self, was: bool, now: bool) {
        if was != now {
            let _previous = self.at_desk.send_replace(now);
        }
    }
}

/// Forgets `connection` when dropped, so every way out of a connection, panic
/// included, stops it counting as at the desk.
pub(crate) struct PresenceGuard {
    presence: Arc<Presence>,
    connection: u64,
}

impl PresenceGuard {
    pub(crate) fn new(presence: Arc<Presence>, connection: u64) -> Self {
        Self {
            presence,
            connection,
        }
    }
}

impl Drop for PresenceGuard {
    fn drop(&mut self) {
        self.presence.forget(self.connection);
    }
}

#[cfg(test)]
mod tests {
    use super::Presence;

    #[test]
    fn at_desk_while_any_connection_is_active() {
        let presence = Presence::default();
        assert!(!presence.at_desk(), "no connection has reported");

        presence.report(1, true);
        presence.report(2, true);
        presence.report(1, true);

        assert!(presence.at_desk(), "one active connection is enough");
    }

    #[test]
    fn a_connection_reporting_inactive_stops_counting() {
        let presence = Presence::default();
        presence.report(1, true);
        presence.report(2, true);

        presence.report(1, false);

        assert!(presence.at_desk(), "the other connection still counts");
        presence.report(2, false);
        assert!(!presence.at_desk(), "no connection is active any more");
    }

    #[test]
    fn a_forgotten_connection_stops_counting() {
        let presence = Presence::default();
        presence.report(1, true);

        presence.forget(1);

        assert!(!presence.at_desk(), "the ended connection is gone");
        presence.forget(7);
        assert!(
            !presence.at_desk(),
            "forgetting an unknown id changes nothing"
        );
    }

    #[tokio::test]
    async fn the_watch_turns_false_when_the_last_active_connection_goes() {
        let presence = Presence::default();
        let mut at_desk = presence.subscribe();
        assert!(!*at_desk.borrow_and_update());

        presence.report(1, true);
        assert!(*at_desk.borrow_and_update(), "the first active connection");

        presence.report(2, true);
        presence.forget(1);
        assert!(
            *at_desk.borrow_and_update(),
            "another connection still reports active"
        );

        presence.forget(2);
        assert!(!*at_desk.borrow_and_update(), "the last one ended");

        presence.report(3, true);
        let changed = at_desk.changed().await;
        assert!(changed.is_ok(), "the sender lives");
        assert!(
            *at_desk.borrow_and_update(),
            "a new report reaches a watcher"
        );
    }
}
