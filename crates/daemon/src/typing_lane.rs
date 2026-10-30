//! One typist at a time on a session's PTY.
//!
//! Every writer that types a run of keys the screen must answer in order
//! (the status watcher's view switch, the preset injector, a chat send or
//! answer) takes the session's [`TypingLane`] for its whole run, so two runs
//! never interleave their keys. A [`TypingTurn`] is the lane held: it types
//! with [`TypingTurn::press`] and gives the lane back when it is dropped,
//! however its run ends. The owner's own keys never take the lane; they only
//! bump its owner epoch, which a run reads to learn the owner typed under it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::OwnedMutexGuard;
use tokio::time::Instant;
use tracing::debug;

use crate::pty::PtyHandle;

/// Who holds a session's typing lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Holder {
    /// The status watcher, switching the terminal's view.
    ViewReturn,
    /// The preset injector, typing its chain.
    Injector,
    /// A chat message being typed.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "a chat send takes it in CV.8's second part")
    )]
    ChatSend,
    /// A chat answer to a dialog being typed.
    #[expect(dead_code, reason = "a chat answer takes it in CV.9")]
    ChatAnswer,
}

/// A session's typing lane: at most one [`TypingTurn`] at a time.
#[derive(Debug, Default)]
pub struct TypingLane {
    lock: Arc<tokio::sync::Mutex<()>>,
    /// Who holds the lane, for the refusal a waiting writer reports.
    holder: Mutex<Option<Holder>>,
    /// When any holder last typed through the lane.
    last_key: Mutex<Option<Instant>>,
    /// Bumped by each owner key.
    owner_epoch: AtomicU64,
}

impl TypingLane {
    /// The lane for `who` if nobody holds it now.
    pub fn try_take(self: &Arc<Self>, who: Holder) -> Option<TypingTurn> {
        let guard = Arc::clone(&self.lock).try_lock_owned().ok()?;
        Some(self.turn(guard, who))
    }

    /// The lane for `who`, waiting at most `wait` for its holder to give it
    /// back; `Err` carries the holder still in it then.
    pub async fn take(
        self: &Arc<Self>,
        who: Holder,
        wait: Duration,
    ) -> Result<TypingTurn, Option<Holder>> {
        match tokio::time::timeout(wait, Arc::clone(&self.lock).lock_owned()).await {
            Ok(guard) => Ok(self.turn(guard, who)),
            Err(_) => Err(self.holder()),
        }
    }

    fn turn(self: &Arc<Self>, guard: OwnedMutexGuard<()>, who: Holder) -> TypingTurn {
        *crate::sync::lock(&self.holder) = Some(who);
        TypingTurn {
            _guard: guard,
            lane: Arc::clone(self),
            who,
            epoch: self.owner_epoch(),
        }
    }

    /// Who holds the lane now.
    pub fn holder(&self) -> Option<Holder> {
        *crate::sync::lock(&self.holder)
    }

    /// The owner typed a key.
    pub fn owner_typed(&self) {
        self.owner_epoch.fetch_add(1, Ordering::SeqCst);
    }

    /// How many owner keys the session has seen.
    pub fn owner_epoch(&self) -> u64 {
        self.owner_epoch.load(Ordering::SeqCst)
    }

    /// When any holder last typed through the lane.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "a chat send takes it in CV.8's second part")
    )]
    pub fn last_key(&self) -> Option<Instant> {
        *crate::sync::lock(&self.last_key)
    }
}

/// The typing lane held by one writer; dropping it gives the lane back.
pub struct TypingTurn {
    _guard: OwnedMutexGuard<()>,
    lane: Arc<TypingLane>,
    who: Holder,
    /// The lane's owner epoch when the turn began.
    epoch: u64,
}

impl TypingTurn {
    /// Write `bytes` to `pty` and stamp the lane's last key.
    pub fn press(&self, pty: &PtyHandle, bytes: Vec<u8>) {
        pty.write_input(bytes);
        *crate::sync::lock(&self.lane.last_key) = Some(Instant::now());
    }

    /// The lane's owner epoch when the turn began.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "a chat send takes it in CV.8's second part")
    )]
    pub fn epoch_at_start(&self) -> u64 {
        self.epoch
    }

    /// Whether the owner typed a key since the turn began.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "a chat send takes it in CV.8's second part")
    )]
    pub fn owner_typed_since(&self) -> bool {
        self.lane.owner_epoch() != self.epoch
    }
}

impl Drop for TypingTurn {
    fn drop(&mut self) {
        *crate::sync::lock(&self.lane.holder) = None;
        debug!(holder = ?self.who, "typing lane released");
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use crate::history::test_support::recording_pty;

    fn lane() -> Arc<TypingLane> {
        Arc::new(TypingLane::default())
    }

    #[test]
    fn a_second_try_fails_while_held_and_names_the_holder() {
        let lane = lane();
        let turn = lane.try_take(Holder::ViewReturn).expect("a free lane");
        assert!(
            lane.try_take(Holder::Injector).is_none(),
            "the lane is held"
        );
        assert_eq!(lane.holder(), Some(Holder::ViewReturn));
        drop(turn);
        assert!(
            lane.try_take(Holder::Injector).is_some(),
            "the lane is free again"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn take_waits_then_gets_the_lane() {
        let lane = lane();
        let turn = lane.try_take(Holder::ViewReturn).expect("a free lane");
        let waiter = {
            let lane = Arc::clone(&lane);
            tokio::spawn(async move {
                lane.take(Holder::ChatSend, Duration::from_secs(5))
                    .await
                    .map(|turn| turn.who)
            })
        };
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(!waiter.is_finished(), "the lane is still held");
        drop(turn);
        assert_eq!(
            waiter.await.expect("the waiter ran"),
            Ok(Holder::ChatSend),
            "the waiter gets the lane once it is given back"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn take_gives_up_after_its_wait() {
        let lane = lane();
        let _turn = lane.try_take(Holder::ViewReturn).expect("a free lane");
        let taken = lane.take(Holder::ChatSend, Duration::from_secs(2)).await;
        assert_eq!(taken.err(), Some(Some(Holder::ViewReturn)));
    }

    #[tokio::test]
    #[expect(
        clippy::panic,
        reason = "the turn's task panics to show the unwind releases it"
    )]
    async fn a_dropped_turn_releases() {
        let lane = lane();
        drop(lane.try_take(Holder::Injector).expect("a free lane"));
        assert_eq!(lane.holder(), None, "dropping a turn clears its holder");
        assert!(
            lane.try_take(Holder::Injector).is_some(),
            "and frees the lane"
        );

        let turn = lane.try_take(Holder::Injector).expect("a free lane");
        let task = tokio::spawn(async move {
            let _turn = turn;
            std::panic::panic_any("the injector's task panicked");
        });
        let joined = task.await;
        assert!(
            joined.is_err_and(|err| err.is_panic()),
            "the task ended in a panic"
        );
        assert_eq!(lane.holder(), None, "the unwind released the lane");
        assert!(lane.try_take(Holder::ViewReturn).is_some());
    }

    #[test]
    fn owner_typed_since_tracks_the_epoch() {
        let lane = lane();
        lane.owner_typed();
        let turn = lane.try_take(Holder::ChatSend).expect("a free lane");
        assert_eq!(turn.epoch_at_start(), 1);
        assert!(
            !turn.owner_typed_since(),
            "no owner key since the turn began"
        );
        lane.owner_typed();
        assert_eq!(lane.owner_epoch(), 2);
        assert!(turn.owner_typed_since(), "the owner typed under the turn");
    }

    #[tokio::test(start_paused = true)]
    async fn press_stamps_last_key() {
        let (pty, mut input, _exit) = recording_pty();
        let lane = Arc::clone(pty.typing());
        assert_eq!(lane.last_key(), None);
        let turn = lane.try_take(Holder::ViewReturn).expect("a free lane");
        tokio::time::sleep(Duration::from_secs(1)).await;
        let at = Instant::now();
        turn.press(&pty, b"\x1b[B".to_vec());
        assert_eq!(
            input.try_recv().ok(),
            Some(b"\x1b[B".to_vec()),
            "the key is written"
        );
        assert_eq!(lane.last_key(), Some(at));
    }
}
