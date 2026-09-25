//! Loading an attached session's scrollback before its live output: the
//! request, its timeout and retries, and the live output held back meanwhile.
//! Time comes in as `now`, so the schedule is testable without a clock.

use std::time::{Duration, Instant};

/// How long one `LoadScrollback` request waits for its reply.
pub const REQUEST_TIMEOUT: Duration = Duration::from_millis(8000);

/// The wait before each retry. Once the last retry times out, loading fails.
pub const RETRY_DELAYS: [Duration; 2] = [Duration::from_millis(2000), Duration::from_millis(4000)];

/// Wipes the in-place retry line.
const CLEAR_LINE: &str = "\r\x1b[2K";

pub const TRUNCATED_BANNER: &str = "\x1b[33m[earlier output discarded]\x1b[0m\r\n";

pub const FAILED_BANNER: &str = "\x1b[33m[could not load earlier output — the daemon is not responding. \
     The session itself may still be running; reopen this pane to retry.]\x1b[0m\r\n";

fn retry_line(attempt: usize) -> String {
    format!(
        "{CLEAR_LINE}\x1b[33m[daemon is slow to respond — retrying ({attempt}/{})…]\x1b[0m",
        RETRY_DELAYS.len()
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Waiting on request number `attempt` (1-based), or on the pause before
    /// the next one.
    Loading(usize),
    Loaded,
    Failed,
}

/// What the pane does next, in order.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    /// Text written into the terminal.
    Status(String),
    /// Send `LoadScrollback` again.
    Request,
    /// The session's history. Replies it provokes are not sent.
    History(Vec<u8>),
    /// Live output.
    Live(Vec<u8>),
    /// Size the session's PTY to the pane.
    Resize,
}

#[derive(Debug)]
enum Phase {
    Awaiting { attempt: usize, deadline: Instant },
    Backoff { attempt: usize, until: Instant },
    Loaded,
    Failed,
}

#[derive(Debug)]
pub struct ScrollbackLoad {
    phase: Phase,
    /// Live output held back until the history is written.
    buffer: Vec<Vec<u8>>,
}

impl ScrollbackLoad {
    /// Starts loading; the caller has just sent the first `LoadScrollback`.
    pub fn start(now: Instant) -> Self {
        Self {
            phase: Phase::Awaiting {
                attempt: 1,
                deadline: now + REQUEST_TIMEOUT,
            },
            buffer: Vec::new(),
        }
    }

    pub fn state(&self) -> State {
        match self.phase {
            Phase::Awaiting { attempt, .. } | Phase::Backoff { attempt, .. } => {
                State::Loading(attempt)
            }
            Phase::Loaded => State::Loaded,
            Phase::Failed => State::Failed,
        }
    }

    /// When [`Self::tick`] next has something to do.
    pub fn next_deadline(&self) -> Option<Instant> {
        match self.phase {
            Phase::Awaiting { deadline, .. } => Some(deadline),
            Phase::Backoff { until, .. } => Some(until),
            Phase::Loaded | Phase::Failed => None,
        }
    }

    /// Live output: returned to be fed now, or held back while loading.
    pub fn on_output(&mut self, bytes: Vec<u8>) -> Option<Vec<u8>> {
        match self.state() {
            State::Loading(_) => {
                self.buffer.push(bytes);
                None
            }
            State::Loaded | State::Failed => Some(bytes),
        }
    }

    /// A scrollback reply. The first one while loading is written, whichever
    /// request it answers; any later one returns `None`.
    pub fn on_reply(&mut self, history: Vec<u8>, truncated: bool) -> Option<Vec<Step>> {
        if !matches!(self.state(), State::Loading(_)) {
            return None;
        }
        self.phase = Phase::Loaded;
        let mut steps = vec![Step::Status(CLEAR_LINE.to_owned())];
        if !history.is_empty() {
            if truncated {
                steps.push(Step::Status(TRUNCATED_BANNER.to_owned()));
            }
            steps.push(Step::History(history));
        }
        Some(self.finish(steps))
    }

    /// Advances the timeout and retry schedule to `now`.
    pub fn tick(&mut self, now: Instant) -> Vec<Step> {
        let mut steps = Vec::new();
        while let Some(step) = self.advance(now) {
            steps.extend(step);
        }
        steps
    }

    /// Takes the one transition due at `now`, if any.
    fn advance(&mut self, now: Instant) -> Option<Vec<Step>> {
        match self.phase {
            Phase::Awaiting { attempt, deadline } if now >= deadline => {
                let Some(delay) = RETRY_DELAYS.get(attempt - 1) else {
                    self.phase = Phase::Failed;
                    let banner = vec![
                        Step::Status(CLEAR_LINE.to_owned()),
                        Step::Status(FAILED_BANNER.to_owned()),
                    ];
                    return Some(self.finish(banner));
                };
                self.phase = Phase::Backoff {
                    attempt,
                    until: now + *delay,
                };
                Some(vec![Step::Status(retry_line(attempt))])
            }
            Phase::Backoff { attempt, until } if now >= until => {
                // The retry's reply carries everything held back so far.
                self.buffer.clear();
                self.phase = Phase::Awaiting {
                    attempt: attempt + 1,
                    deadline: now + REQUEST_TIMEOUT,
                };
                Some(vec![Step::Request])
            }
            _ => None,
        }
    }

    /// Appends the held-back output and the resize that end every load.
    fn finish(&mut self, mut steps: Vec<Step>) -> Vec<Step> {
        steps.extend(self.buffer.drain(..).map(Step::Live));
        steps.push(Step::Resize);
        steps
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::{
        CLEAR_LINE, FAILED_BANNER, REQUEST_TIMEOUT, RETRY_DELAYS, ScrollbackLoad, State, Step,
        TRUNCATED_BANNER, retry_line,
    };

    fn clear() -> Step {
        Step::Status(CLEAR_LINE.to_owned())
    }

    #[test]
    fn reply_before_the_timeout_loads_and_drains_the_buffer_in_order() {
        let mut load = ScrollbackLoad::start(Instant::now());
        assert_eq!(load.on_output(b"a".to_vec()), None);
        assert_eq!(load.on_output(b"b".to_vec()), None);
        assert_eq!(
            load.on_reply(b"history".to_vec(), false),
            Some(vec![
                clear(),
                Step::History(b"history".to_vec()),
                Step::Live(b"a".to_vec()),
                Step::Live(b"b".to_vec()),
                Step::Resize,
            ])
        );
        assert_eq!(load.state(), State::Loaded);
        assert_eq!(load.next_deadline(), None);
    }

    #[test]
    fn timeout_retries_clears_the_buffer_and_schedules_the_next_wait() {
        let t0 = Instant::now();
        let mut load = ScrollbackLoad::start(t0);
        assert!(load.tick(t0 + REQUEST_TIMEOUT / 2).is_empty());
        assert_eq!(load.on_output(b"stale".to_vec()), None);

        let timed_out = t0 + REQUEST_TIMEOUT;
        assert_eq!(load.tick(timed_out), vec![Step::Status(retry_line(1))]);
        assert_eq!(load.state(), State::Loading(1));
        let retry_at = timed_out + RETRY_DELAYS[0];
        assert_eq!(load.next_deadline(), Some(retry_at));

        assert_eq!(load.tick(retry_at), vec![Step::Request]);
        assert_eq!(load.state(), State::Loading(2));
        assert_eq!(load.next_deadline(), Some(retry_at + REQUEST_TIMEOUT));
        assert_eq!(
            load.on_reply(b"h".to_vec(), false),
            Some(vec![clear(), Step::History(b"h".to_vec()), Step::Resize])
        );
    }

    #[test]
    fn the_full_retry_schedule_ends_in_failed() {
        let t0 = Instant::now();
        let mut load = ScrollbackLoad::start(t0);
        assert_eq!(load.on_output(b"live".to_vec()), None);
        let mut steps = Vec::new();
        let mut now = t0;
        while let Some(deadline) = load.next_deadline() {
            now = deadline;
            steps.extend(load.tick(now));
        }
        assert_eq!(load.state(), State::Failed);
        assert_eq!(
            now - t0,
            REQUEST_TIMEOUT * 3 + RETRY_DELAYS[0] + RETRY_DELAYS[1]
        );
        assert_eq!(
            steps,
            vec![
                Step::Status(retry_line(1)),
                Step::Request,
                Step::Status(retry_line(2)),
                Step::Request,
                clear(),
                Step::Status(FAILED_BANNER.to_owned()),
                Step::Resize,
            ]
        );
        assert_eq!(load.on_reply(b"late".to_vec(), false), None);
        assert_eq!(load.on_output(b"x".to_vec()), Some(b"x".to_vec()));
    }

    #[test]
    fn a_late_reply_after_a_retry_is_accepted_once() {
        let t0 = Instant::now();
        let mut load = ScrollbackLoad::start(t0);
        load.tick(t0 + REQUEST_TIMEOUT);
        let retried = load.tick(t0 + REQUEST_TIMEOUT + RETRY_DELAYS[0]);
        assert_eq!(retried, vec![Step::Request]);
        assert!(load.on_reply(b"h".to_vec(), false).is_some());
        assert_eq!(load.state(), State::Loaded);
        assert_eq!(load.on_reply(b"h".to_vec(), false), None);
    }

    #[test]
    fn a_reply_during_the_pause_before_a_retry_is_accepted() {
        let t0 = Instant::now();
        let mut load = ScrollbackLoad::start(t0);
        load.tick(t0 + REQUEST_TIMEOUT);
        assert!(load.on_reply(Vec::new(), false).is_some());
        assert_eq!(load.state(), State::Loaded);
    }

    #[test]
    fn truncated_history_gets_the_banner() {
        let mut load = ScrollbackLoad::start(Instant::now());
        assert_eq!(
            load.on_reply(b"h".to_vec(), true),
            Some(vec![
                clear(),
                Step::Status(TRUNCATED_BANNER.to_owned()),
                Step::History(b"h".to_vec()),
                Step::Resize,
            ])
        );
    }

    #[test]
    fn output_after_loaded_is_fed_directly() {
        let mut load = ScrollbackLoad::start(Instant::now());
        load.on_reply(Vec::new(), false);
        assert_eq!(load.on_output(b"x".to_vec()), Some(b"x".to_vec()));
    }
}
