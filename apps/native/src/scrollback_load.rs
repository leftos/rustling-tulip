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

/// Twice the daemon's scrollback ring, so a whole replay and the output
/// behind it always fit.
pub const PRE_LAYOUT_CAP_BYTES: usize = 4 * 1024 * 1024;

/// What a terminal pane wrote before its first layout, kept so the first
/// layout can rebuild the terminal at the pane's own size by replaying it,
/// rather than show it reflowed from the placeholder's.
#[derive(Debug, Default)]
pub struct PreLayoutLog {
    payloads: Vec<Vec<u8>>,
    bytes: usize,
    finished: bool,
}

impl PreLayoutLog {
    /// Whether writes are being kept for the replay.
    pub fn recording(&self) -> bool {
        !self.finished
    }

    /// Keeps a copy of `bytes` for the replay. Past [`PRE_LAYOUT_CAP_BYTES`]
    /// the log drops what it kept and finishes, so the pane keeps what the
    /// placeholder showed, as it would with no log at all.
    pub fn record(&mut self, bytes: &[u8]) {
        if !self.recording() || bytes.is_empty() {
            return;
        }
        let total = self.bytes.saturating_add(bytes.len());
        if total > PRE_LAYOUT_CAP_BYTES {
            self.payloads = Vec::new();
            self.bytes = 0;
            self.finished = true;
            return;
        }
        self.bytes = total;
        self.payloads.push(bytes.to_vec());
    }

    /// The kept payloads, in the order they were written, or `None` when
    /// nothing was kept. Finishes the log either way.
    pub fn take(&mut self) -> Option<Vec<Vec<u8>>> {
        self.finished = true;
        self.bytes = 0;
        let payloads = std::mem::take(&mut self.payloads);
        (!payloads.is_empty()).then_some(payloads)
    }
}

/// What became of a scrollback reply.
#[derive(Debug, PartialEq, Eq)]
pub enum ReplyVerdict {
    /// It answers the load; the steps write it.
    Accepted(Vec<Step>),
    /// It answers an earlier request than the latest one, and is dropped.
    Stale,
    /// No load is in progress, so it is dropped.
    NotLoading,
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
    /// The id of the latest `LoadScrollback` sent; only its reply is written.
    request_id: Option<String>,
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
            request_id: None,
        }
    }

    /// A `LoadScrollback` went out under `request_id`: replies to earlier
    /// requests are dropped from now on.
    pub fn expect_reply_to(&mut self, request_id: String) {
        self.request_id = Some(request_id);
    }

    /// The id of the latest `LoadScrollback` sent, once known.
    pub fn request_id(&self) -> Option<&str> {
        self.request_id.as_deref()
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

    /// A scrollback reply, written if it is the first one while loading and
    /// answers the latest request. A reply without an id is never stale: an
    /// id-less `Scrollback` is the daemon's resync, which the pane handles
    /// before it reaches the load, starting a fresh load for it. `history` is
    /// only called, to decode the reply, once it is accepted; its error
    /// leaves the load as it was.
    ///
    /// When the daemon says `forwarder_restarted`, the output held back so
    /// far came from the stream it stopped and is already in the history: it
    /// is dropped. Otherwise it follows the history.
    pub fn on_reply<E>(
        &mut self,
        request_id: Option<&str>,
        forwarder_restarted: bool,
        truncated: bool,
        history: impl FnOnce() -> Result<Vec<u8>, E>,
    ) -> Result<ReplyVerdict, E> {
        if !matches!(self.state(), State::Loading(_)) {
            return Ok(ReplyVerdict::NotLoading);
        }
        if request_id.is_some_and(|id| self.request_id.as_deref() != Some(id)) {
            return Ok(ReplyVerdict::Stale);
        }
        let history = history()?;
        if forwarder_restarted {
            self.buffer.clear();
        }
        self.phase = Phase::Loaded;
        let mut steps = vec![Step::Status(CLEAR_LINE.to_owned())];
        if !history.is_empty() {
            if truncated {
                steps.push(Step::Status(TRUNCATED_BANNER.to_owned()));
            }
            steps.push(Step::History(history));
        }
        Ok(ReplyVerdict::Accepted(self.finish(steps)))
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
    use std::convert::Infallible;
    use std::time::Instant;

    use super::{
        CLEAR_LINE, FAILED_BANNER, PRE_LAYOUT_CAP_BYTES, PreLayoutLog, REQUEST_TIMEOUT,
        RETRY_DELAYS, ReplyVerdict, ScrollbackLoad, State, Step, TRUNCATED_BANNER, retry_line,
    };

    fn clear() -> Step {
        Step::Status(CLEAR_LINE.to_owned())
    }

    /// Hands the load an untruncated reply.
    fn reply(
        load: &mut ScrollbackLoad,
        history: &[u8],
        request_id: Option<&str>,
        forwarder_restarted: bool,
    ) -> ReplyVerdict {
        load.on_reply(request_id, forwarder_restarted, false, || {
            Ok::<_, Infallible>(history.to_vec())
        })
        .unwrap_or_else(|never| match never {})
    }

    #[test]
    fn reply_before_the_timeout_loads_and_drains_the_buffer_in_order() {
        let mut load = ScrollbackLoad::start(Instant::now());
        assert_eq!(load.on_output(b"a".to_vec()), None);
        assert_eq!(load.on_output(b"b".to_vec()), None);
        assert_eq!(
            reply(&mut load, b"history", None, false),
            ReplyVerdict::Accepted(vec![
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
        assert_eq!(
            load.tick(t0 + REQUEST_TIMEOUT / 2),
            [] as [crate::scrollback_load::Step; 0]
        );
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
            reply(&mut load, b"h", None, false),
            ReplyVerdict::Accepted(vec![clear(), Step::History(b"h".to_vec()), Step::Resize])
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
        assert_eq!(
            reply(&mut load, b"late", None, false),
            ReplyVerdict::NotLoading
        );
        assert_eq!(load.on_output(b"x".to_vec()), Some(b"x".to_vec()));
    }

    #[test]
    fn a_late_reply_to_an_earlier_request_is_dropped() {
        let t0 = Instant::now();
        let mut load = ScrollbackLoad::start(t0);
        load.expect_reply_to("first".to_owned());
        load.tick(t0 + REQUEST_TIMEOUT);
        let retry_at = t0 + REQUEST_TIMEOUT + RETRY_DELAYS[0];
        assert_eq!(load.tick(retry_at), vec![Step::Request]);
        load.expect_reply_to("second".to_owned());

        assert_eq!(
            reply(&mut load, b"stale", Some("first"), true),
            ReplyVerdict::Stale
        );
        assert_eq!(load.state(), State::Loading(2));
        assert_eq!(load.next_deadline(), Some(retry_at + REQUEST_TIMEOUT));

        assert_eq!(
            reply(&mut load, b"fresh", Some("second"), true),
            ReplyVerdict::Accepted(vec![
                clear(),
                Step::History(b"fresh".to_vec()),
                Step::Resize
            ])
        );
        assert_eq!(load.state(), State::Loaded);
        assert_eq!(
            reply(&mut load, b"fresh", Some("second"), true),
            ReplyVerdict::NotLoading
        );
    }

    #[test]
    fn a_reply_before_any_request_id_is_known_is_dropped() {
        let mut load = ScrollbackLoad::start(Instant::now());
        assert_eq!(
            reply(&mut load, b"h", Some("other"), true),
            ReplyVerdict::Stale
        );
        assert_eq!(load.state(), State::Loading(1));
    }

    #[test]
    fn a_matching_reply_discards_the_output_held_back() {
        let mut load = ScrollbackLoad::start(Instant::now());
        load.expect_reply_to("r1".to_owned());
        assert_eq!(load.on_output(b"X".to_vec()), None);
        assert_eq!(
            reply(&mut load, b"H", Some("r1"), true),
            ReplyVerdict::Accepted(vec![clear(), Step::History(b"H".to_vec()), Step::Resize])
        );
        assert_eq!(load.on_output(b"live".to_vec()), Some(b"live".to_vec()));
    }

    #[test]
    fn matching_reply_without_restart_keeps_the_buffer() {
        let mut load = ScrollbackLoad::start(Instant::now());
        load.expect_reply_to("r1".to_owned());
        assert_eq!(load.on_output(b"X".to_vec()), None);
        assert_eq!(
            reply(&mut load, b"H", Some("r1"), false),
            ReplyVerdict::Accepted(vec![
                clear(),
                Step::History(b"H".to_vec()),
                Step::Live(b"X".to_vec()),
                Step::Resize,
            ])
        );
        assert_eq!(load.state(), State::Loaded);
    }

    #[test]
    fn an_id_less_reply_after_a_retry_is_accepted_once_with_the_output_held_back() {
        let t0 = Instant::now();
        let mut load = ScrollbackLoad::start(t0);
        load.expect_reply_to("first".to_owned());
        load.tick(t0 + REQUEST_TIMEOUT);
        let retried = load.tick(t0 + REQUEST_TIMEOUT + RETRY_DELAYS[0]);
        assert_eq!(retried, vec![Step::Request]);
        load.expect_reply_to("second".to_owned());
        assert_eq!(load.on_output(b"X".to_vec()), None);
        assert_eq!(
            reply(&mut load, b"h", None, false),
            ReplyVerdict::Accepted(vec![
                clear(),
                Step::History(b"h".to_vec()),
                Step::Live(b"X".to_vec()),
                Step::Resize,
            ])
        );
        assert_eq!(load.state(), State::Loaded);
        assert_eq!(
            reply(&mut load, b"h", None, false),
            ReplyVerdict::NotLoading
        );
    }

    #[test]
    fn a_reply_during_the_pause_before_a_retry_is_accepted() {
        let t0 = Instant::now();
        let mut load = ScrollbackLoad::start(t0);
        load.tick(t0 + REQUEST_TIMEOUT);
        assert!(matches!(
            reply(&mut load, b"", None, false),
            ReplyVerdict::Accepted(_)
        ));
        assert_eq!(load.state(), State::Loaded);
    }

    #[test]
    fn truncated_history_gets_the_banner() {
        let mut load = ScrollbackLoad::start(Instant::now());
        let verdict = load
            .on_reply(None, false, true, || Ok::<_, Infallible>(b"h".to_vec()))
            .unwrap_or_else(|never| match never {});
        assert_eq!(
            verdict,
            ReplyVerdict::Accepted(vec![
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
        reply(&mut load, b"", None, false);
        assert_eq!(load.on_output(b"x".to_vec()), Some(b"x".to_vec()));
    }

    #[test]
    fn a_default_records_and_take_returns_the_payloads_in_order() {
        let mut log = PreLayoutLog::default();
        assert!(log.recording(), "a pane records until its first layout");
        log.record(b"one");
        log.record(b"two");
        assert!(log.recording(), "under the cap it keeps recording");
        assert_eq!(log.take(), Some(vec![b"one".to_vec(), b"two".to_vec()]));
        assert!(!log.recording(), "a take finishes the log");
        assert_eq!(log.take(), None, "a finished log has nothing to give");
    }

    #[test]
    fn an_empty_write_records_nothing() {
        let mut log = PreLayoutLog::default();
        log.record(b"");
        assert_eq!(log.take(), None, "nothing was written");
    }

    #[test]
    fn a_total_exactly_at_the_cap_still_replays() {
        let mut log = PreLayoutLog::default();
        let first = vec![b'x'; PRE_LAYOUT_CAP_BYTES - 1];
        log.record(&first);
        log.record(b"y");
        assert_eq!(log.take(), Some(vec![first, b"y".to_vec()]));
    }

    #[test]
    fn a_payload_past_the_cap_drops_the_log() {
        let mut log = PreLayoutLog::default();
        log.record(&vec![b'x'; PRE_LAYOUT_CAP_BYTES - 1]);
        log.record(b"yz");
        assert!(!log.recording(), "the cap finishes the log");
        assert_eq!(log.take(), None, "nothing kept past the cap is replayed");
    }
}
