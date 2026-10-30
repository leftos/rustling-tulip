//! The summarizer's per-session queue: one request runs per session at a time
//! and at most one more waits; a new request for the session replaces the
//! waiting one, which ends unrun, except that a working refresh never
//! replaces a waiting stop request: it ends unrun itself. A running request
//! is never cancelled.

use std::collections::HashMap;
use std::sync::Mutex;

use tokio::sync::oneshot;

use crate::summarizer::prompt::Trigger;

/// The sessions with a request running, each with the request waiting after
/// it, if any.
#[derive(Debug, Default)]
pub(crate) struct Queue {
    sessions: Mutex<HashMap<String, Slot>>,
}

#[derive(Debug, Default)]
struct Slot {
    /// The waiting request's trigger, and the sender told when the running
    /// request ends and it may run; dropped when a newer request replaces it.
    waiting: Option<(Trigger, oneshot::Sender<()>)>,
}

impl Slot {
    /// Whether a request for `trigger` may take the waiting place: always,
    /// unless it is a working refresh and a stop request still waits there.
    fn admits(&self, trigger: Trigger) -> bool {
        match &self.waiting {
            Some((Trigger::Stop, tx)) if !tx.is_closed() => trigger == Trigger::Stop,
            _ => true,
        }
    }
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
    /// Wait for `session_id`'s turn for a `trigger` request: at once when
    /// nothing runs for it, else once the running request ends. `None` when
    /// a newer request for the session replaced this one first, or at once
    /// for a working refresh while a stop request waits.
    pub(crate) async fn admit(&self, session_id: &str, trigger: Trigger) -> Option<Turn<'_>> {
        let rx = {
            let mut sessions = crate::sync::lock(&self.sessions);
            match sessions.get_mut(session_id) {
                None => {
                    sessions.insert(session_id.to_owned(), Slot::default());
                    return Some(self.turn(session_id));
                }
                Some(slot) if !slot.admits(trigger) => return None,
                Some(slot) => {
                    let (tx, rx) = oneshot::channel();
                    slot.waiting = Some((trigger, tx));
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
            .is_some_and(|(_, waiting)| waiting.send(()).is_ok());
        if !handed {
            sessions.remove(session_id);
        }
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests fail loudly on setup errors")]
mod tests {
    use super::*;

    const STOP: Trigger = Trigger::Stop;
    const WORKING: Trigger = Trigger::Working;

    #[tokio::test]
    async fn a_second_trigger_for_a_session_replaces_its_queued_request() {
        let queue = Queue::default();
        let running = queue
            .admit("s1", STOP)
            .await
            .expect("nothing runs, so s1 runs at once");
        let other = queue
            .admit("s2", STOP)
            .await
            .expect("another session runs beside it");

        let mut second = Box::pin(queue.admit("s1", STOP));
        assert!(futures::poll!(&mut second).is_pending(), "the second waits");
        let mut third = Box::pin(queue.admit("s1", STOP));
        assert!(futures::poll!(&mut third).is_pending(), "the third waits");
        assert!(second.await.is_none(), "the third replaced the second");

        drop(running);
        let turn = third.await.expect("the third runs once the first ends");
        drop(turn);
        drop(other);
        assert!(
            queue.admit("s1", STOP).await.is_some(),
            "a freed session runs at once"
        );
    }

    #[tokio::test]
    async fn a_working_request_never_replaces_a_queued_stop() {
        let queue = Queue::default();
        let running = queue.admit("s1", STOP).await.expect("s1 runs at once");
        let mut stop = Box::pin(queue.admit("s1", STOP));
        assert!(futures::poll!(&mut stop).is_pending(), "the stop waits");

        let mut working = Box::pin(queue.admit("s1", WORKING));
        assert!(
            matches!(futures::poll!(&mut working), std::task::Poll::Ready(None)),
            "the working request ends unrun at once"
        );
        drop(running);
        assert!(
            stop.await.is_some(),
            "the queued stop still runs once the first ends"
        );
    }

    #[tokio::test]
    async fn a_stop_request_replaces_a_queued_working_one() {
        let queue = Queue::default();
        let running = queue.admit("s1", WORKING).await.expect("s1 runs at once");
        let mut working = Box::pin(queue.admit("s1", WORKING));
        assert!(
            futures::poll!(&mut working).is_pending(),
            "the refresh waits"
        );
        let mut stop = Box::pin(queue.admit("s1", STOP));
        assert!(futures::poll!(&mut stop).is_pending(), "the stop waits");

        assert!(working.await.is_none(), "the stop replaced the refresh");
        drop(running);
        assert!(stop.await.is_some(), "the stop runs once the first ends");
    }
}
