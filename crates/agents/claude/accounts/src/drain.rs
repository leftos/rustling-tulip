//! The drain a stop runs before it kills the server: once started it refuses new grant spends and switches, and
//! [`Drain::wait_idle`] waits for the ones already in flight.
//!
//! A token refresh runs between Anthropic accepting a one-time refresh token and the new token reaching the store; a
//! stop in that window loses the account. A caller that spends a grant takes a [`WorkGuard`] with [`Drain::begin`]
//! first, so a stop arriving before the spend is refused and one arriving after it waits for the guard to drop. The
//! drain is held in an [`Arc`], which [`Drain::begin`] takes, so a guard owns a handle on it and can move into a
//! `'static` task or thread.

use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// The refusal [`Drain::begin`] gives once the drain has started.
#[derive(Debug, thiserror::Error)]
#[error("draining: no new grant spends or switches")]
pub struct Draining;

/// The drain's state: whether it has started, and how many [`WorkGuard`]s are held now.
#[derive(Debug, Default)]
struct State {
    draining: bool,
    held: usize,
}

/// Refuses new grant spends and switches once started, and lets a stop wait for the ones already in flight.
#[derive(Debug, Default)]
pub struct Drain {
    state: Mutex<State>,
    /// Notified whenever a guard drops, so [`Drain::wait_idle`] re-checks `held`.
    idle: Condvar,
}

impl Drain {
    /// A drain that has not started: [`Drain::begin`] hands out guards freely.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Takes a guard for one grant spend or switch, unless the drain has started. Takes the [`Arc`] so the guard owns a
    /// handle on the drain and can move into a `'static` task or thread.
    ///
    /// # Errors
    ///
    /// [`Draining`] once [`Drain::start`] has run.
    pub fn begin(self: &Arc<Self>) -> Result<WorkGuard, Draining> {
        let mut state = lock(&self.state);
        if state.draining {
            return Err(Draining);
        }
        state.held += 1;
        Ok(WorkGuard {
            drain: Arc::clone(self),
        })
    }

    /// Marks the drain started and returns the number of guards held now. Every later [`Drain::begin`] is refused.
    #[must_use]
    pub fn start(&self) -> usize {
        let mut state = lock(&self.state);
        state.draining = true;
        state.held
    }

    /// Waits until no guard is held and returns true, or until `bound` has passed and returns false.
    #[must_use]
    pub fn wait_idle(&self, bound: Duration) -> bool {
        let deadline = Instant::now() + bound;
        let mut state = lock(&self.state);
        while state.held > 0 {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            state = self
                .idle
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        true
    }

    /// Whether [`Drain::start`] has run.
    #[must_use]
    pub fn is_draining(&self) -> bool {
        lock(&self.state).draining
    }
}

/// One in-flight grant spend or switch, dropped when it finishes; it owns a handle on the drain, so it can be moved
/// into a `'static` task or thread. The drop wakes [`Drain::wait_idle`].
#[derive(Debug)]
pub struct WorkGuard {
    drain: Arc<Drain>,
}

impl Drop for WorkGuard {
    fn drop(&mut self) {
        let mut state = lock(&self.drain.state);
        state.held = state.held.saturating_sub(1);
        drop(state);
        self.drain.idle.notify_all();
    }
}

/// The drain's state, ignoring a poisoned lock: it holds a flag and a count, which a panic inside the guard's own drop
/// cannot leave inconsistent.
fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test that cannot take a guard or join its thread must fail loudly"
)]
mod tests {
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use super::Drain;

    #[test]
    fn begin_after_start_is_refused() {
        let drain = Arc::new(Drain::new());

        assert_eq!(drain.start(), 0, "no guard was held");
        assert!(drain.begin().is_err(), "begin after start must be refused");
    }

    #[test]
    fn start_counts_held_guards() {
        let drain = Arc::new(Drain::new());
        let _first = drain.begin().expect("the drain has not started");
        let _second = drain.begin().expect("the drain has not started");

        assert_eq!(drain.start(), 2, "start must count the guards held then");
        assert!(drain.is_draining(), "start must mark the drain started");
    }

    #[test]
    fn guard_moves_into_a_static_thread() {
        let drain = Arc::new(Drain::new());
        let guard = drain.begin().expect("the drain has not started");
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            drop(guard);
        });

        let started = Instant::now();
        assert!(
            drain.wait_idle(Duration::from_secs(2)),
            "the guard was dropped inside the bound"
        );
        let waited = started.elapsed();
        assert!(
            waited >= Duration::from_millis(50),
            "wait_idle returned after only {waited:?}"
        );

        handle.join().expect("the holder thread finishes");
    }

    #[test]
    fn wait_idle_blocks_while_a_guard_is_held() {
        let drain = Arc::new(Drain::new());
        let (ready_tx, ready_rx) = mpsc::channel();
        let holder = Arc::clone(&drain);
        let handle = std::thread::spawn(move || {
            let guard = holder.begin().expect("the drain has not started");
            ready_tx
                .send(())
                .expect("the main thread is waiting for the guard");
            std::thread::sleep(Duration::from_millis(200));
            drop(guard);
        });
        ready_rx.recv().expect("the holder thread takes the guard");

        let started = Instant::now();
        assert!(
            drain.wait_idle(Duration::from_secs(2)),
            "the guard was dropped inside the bound"
        );
        let waited = started.elapsed();
        assert!(
            waited >= Duration::from_millis(150),
            "wait_idle returned after only {waited:?}"
        );

        handle.join().expect("the holder thread finishes");
    }

    #[test]
    fn wait_idle_times_out() {
        let drain = Arc::new(Drain::new());
        let guard = drain.begin().expect("the drain has not started");

        let started = Instant::now();
        assert!(
            !drain.wait_idle(Duration::from_millis(100)),
            "a held guard must time out"
        );
        let waited = started.elapsed();
        assert!(
            waited >= Duration::from_millis(100),
            "wait_idle returned after only {waited:?}"
        );

        drop(guard);
    }
}
