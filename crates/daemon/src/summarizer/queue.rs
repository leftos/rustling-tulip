//! The summarizer's per-session queue: one request runs per session at a time
//! and at most one more waits; a new request for the session replaces the
//! waiting one, which ends unrun. A running request is never cancelled.

use std::collections::HashMap;
use std::sync::Mutex;

use tokio::sync::oneshot;

/// The sessions with a request running, each with the request waiting after
/// it, if any.
#[derive(Debug, Default)]
pub(crate) struct Queue {
    sessions: Mutex<HashMap<String, Slot>>,
}

#[derive(Debug, Default)]
struct Slot {
    /// Told when the running request ends and this one may run; dropped when
    /// a newer request replaces it.
    waiting: Option<oneshot::Sender<()>>,
}

/// A session's turn to run; the next waiting request runs once it drops.
#[derive(Debug)]
pub(crate) struct Turn<'a> {
    queue: &'a Queue,
    session_id: String,
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        self.queue.finish(&self.session_id);
    }
}

/// A waiting request. Dropped after its turn was handed over but before it
/// took it, it passes the turn on.
struct Waiting<'a> {
    queue: &'a Queue,
    session_id: String,
    rx: Option<oneshot::Receiver<()>>,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        if let Some(mut rx) = self.rx.take()
            && rx.try_recv().is_ok()
        {
            self.queue.finish(&self.session_id);
        }
    }
}

impl Queue {
    /// Wait for `session_id`'s turn: at once when nothing runs for it, else
    /// once the running request ends. `None` when a newer request for the
    /// session replaced this one first.
    pub(crate) async fn admit(&self, session_id: &str) -> Option<Turn<'_>> {
        let rx = {
            let mut sessions = crate::sync::lock(&self.sessions);
            match sessions.get_mut(session_id) {
                None => {
                    sessions.insert(session_id.to_owned(), Slot::default());
                    return Some(self.turn(session_id));
                }
                Some(slot) => {
                    let (tx, rx) = oneshot::channel();
                    slot.waiting = Some(tx);
                    rx
                }
            }
        };
        let mut waiting = Waiting {
            queue: self,
            session_id: session_id.to_owned(),
            rx: Some(rx),
        };
        let handed = match waiting.rx.as_mut() {
            Some(rx) => rx.await.is_ok(),
            None => false,
        };
        waiting.rx = None;
        handed.then(|| self.turn(session_id))
    }

    fn turn(&self, session_id: &str) -> Turn<'_> {
        Turn {
            queue: self,
            session_id: session_id.to_owned(),
        }
    }

    /// End `session_id`'s running request: hand the turn to the waiting
    /// request, or free the session when none waits.
    fn finish(&self, session_id: &str) {
        let mut sessions = crate::sync::lock(&self.sessions);
        let Some(slot) = sessions.get_mut(session_id) else {
            return;
        };
        let handed = slot
            .waiting
            .take()
            .is_some_and(|waiting| waiting.send(()).is_ok());
        if !handed {
            sessions.remove(session_id);
        }
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests fail loudly on setup errors")]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_second_trigger_for_a_session_replaces_its_queued_request() {
        let queue = Queue::default();
        let running = queue
            .admit("s1")
            .await
            .expect("nothing runs, so s1 runs at once");
        let other = queue
            .admit("s2")
            .await
            .expect("another session runs beside it");

        let mut second = Box::pin(queue.admit("s1"));
        assert!(futures::poll!(&mut second).is_pending(), "the second waits");
        let mut third = Box::pin(queue.admit("s1"));
        assert!(futures::poll!(&mut third).is_pending(), "the third waits");
        assert!(second.await.is_none(), "the third replaced the second");

        drop(running);
        let turn = third.await.expect("the third runs once the first ends");
        drop(turn);
        drop(other);
        assert!(
            queue.admit("s1").await.is_some(),
            "a freed session runs at once"
        );
    }
}
