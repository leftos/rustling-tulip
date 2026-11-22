//! The client side of a driven client's pipe: one request line out, the
//! reply line with the same id back, within [`REPLY_GUARD`].
//!
//! The reply and busy-pipe ceilings count load-adjusted time on the
//! process's one [`clock`], with a backstop of 5x their budget in wall time,
//! so other work loading the machine does not turn them into false hangs;
//! the poll sleeps between busy retries stay wall time.

use std::fs::File;
use std::io::{BufRead as _, BufReader, Write as _};
use std::sync::{Arc, OnceLock, mpsc};
use std::time::Duration;

use load_clock::{Deadline, Expiry, LoadClock};
use serde_json::{Value, json};

/// How long a call waits for its reply, in load-adjusted time with a 5x
/// wall backstop (350 s): longer than the endpoint's longest wait (60 s),
/// and a deadline's adjusted time never passes its wall time, so the
/// transport never cuts a full-length wait short.
pub const REPLY_GUARD: Duration = Duration::from_secs(70);

/// How long opening the pipe retries while every instance is busy, in
/// load-adjusted time with a 5x wall backstop.
const BUSY_GUARD: Duration = Duration::from_secs(5);

/// The longest a reply wait blocks before checking its deadline again.
const SLICE: Duration = Duration::from_secs(1);

/// The process's one load-adjusted clock, which every ceiling counts on.
#[must_use]
pub fn clock() -> Arc<LoadClock> {
    static CLOCK: OnceLock<Arc<LoadClock>> = OnceLock::new();
    Arc::clone(CLOCK.get_or_init(LoadClock::system))
}

/// `ERROR_PIPE_BUSY`: every instance of the pipe is serving a client.
const ERROR_PIPE_BUSY: i32 = 231;

/// Why a call to the endpoint brought no result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError {
    /// The endpoint answered with an error.
    Endpoint {
        code: String,
        message: String,
        hint: Option<String>,
    },
    /// No answer: the pipe could not be opened, closed, or the reply did
    /// not come within [`REPLY_GUARD`].
    Transport(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Endpoint {
                code,
                message,
                hint,
            } => {
                write!(f, "{code}: {message}")?;
                match hint {
                    Some(hint) => write!(f, "\nhint: {hint}"),
                    None => Ok(()),
                }
            }
            Self::Transport(why) => write!(f, "transport: {why}"),
        }
    }
}

/// One call at a time to a driver endpoint.
pub trait Transport {
    /// Calls `method` with `params` and returns the endpoint's `result`.
    fn call(&mut self, method: &str, params: Value) -> Result<Value, CallError>;
}

/// A driven client's pipe, opened on first use and again after a call that
/// ended uncleanly.
pub struct PipeClient {
    path: String,
    connection: Option<BufReader<File>>,
    next_id: u64,
}

impl PipeClient {
    /// The client of the pipe at `path` (`\\.\pipe\rt-driver-<pid>`);
    /// nothing is opened yet.
    #[must_use]
    pub fn new(path: String) -> Self {
        Self {
            path,
            connection: None,
            next_id: 1,
        }
    }

    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Writes `line` and reads lines until the reply to `id` (or to the
    /// reserved id 0, a refusal of the line itself). A failure drops the
    /// connection, so a late reply is never read as the next call's.
    fn exchange(&mut self, line: &str, id: u64) -> Result<Value, String> {
        let mut connection = match self.connection.take() {
            Some(connection) => connection,
            None => open(&self.path)?,
        };
        let pipe = connection.get_mut();
        pipe.write_all(line.as_bytes())
            .and_then(|()| pipe.flush())
            .map_err(|err| format!("writing to {}: {err}", self.path))?;
        let deadline = Deadline::start(clock(), REPLY_GUARD);
        loop {
            let (back, text) = read_line_by(connection, &deadline)?;
            connection = back;
            let reply: Value = serde_json::from_str(&text)
                .map_err(|err| format!("the endpoint's reply is not JSON ({err}): {text}"))?;
            match reply.get("id").and_then(Value::as_u64) {
                Some(got) if got == id || got == 0 => {
                    self.connection = Some(connection);
                    return Ok(reply);
                }
                other => tracing::warn!(?other, id, "dropping a reply to another request"),
            }
        }
    }
}

impl Transport for PipeClient {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, CallError> {
        let id = self.next_id;
        self.next_id += 1;
        let mut line = json!({"id": id, "method": method, "params": params}).to_string();
        line.push('\n');
        let reply = self.exchange(&line, id).map_err(CallError::Transport)?;
        outcome(reply)
    }
}

/// The `result` of an endpoint reply, or its `error`.
fn outcome(mut reply: Value) -> Result<Value, CallError> {
    if let Some(result) = reply.get_mut("result") {
        return Ok(result.take());
    }
    let error = &reply["error"];
    let text = |key: &str| error[key].as_str().map(str::to_owned);
    match (text("code"), text("message")) {
        (Some(code), Some(message)) => Err(CallError::Endpoint {
            code,
            message,
            hint: text("hint"),
        }),
        _ => Err(CallError::Transport(format!(
            "the endpoint's reply has neither a result nor an error: {reply}"
        ))),
    }
}

/// Opens the pipe, retrying while every instance is busy.
fn open(path: &str) -> Result<BufReader<File>, String> {
    let deadline = Deadline::start(clock(), BUSY_GUARD);
    loop {
        match File::options().read(true).write(true).open(path) {
            Ok(file) => return Ok(BufReader::new(file)),
            Err(err) if err.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                if let Some(expiry) = deadline.expired() {
                    return Err(all_busy(path, &expiry));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(err) => return Err(format!("opening {path}: {err}")),
        }
    }
}

/// One line read on a thread of its own, so the wait ends at `deadline`; a
/// read still pending then keeps the connection, which is dropped.
fn read_line_by(
    mut connection: BufReader<File>,
    deadline: &Deadline,
) -> Result<(BufReader<File>, String), String> {
    let (send, receive) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let read = connection.read_line(&mut line);
        let _ = send.send((connection, read, line));
    });
    // A reader that ended without sending answers at once, never as a timeout.
    let wait = |slice| match receive.recv_timeout(slice) {
        Ok(got) => Some(Some(got)),
        Err(mpsc::RecvTimeoutError::Timeout) => None,
        Err(mpsc::RecvTimeoutError::Disconnected) => Some(None),
    };
    match await_reply(deadline, wait) {
        Ok(Some((connection, Ok(read), line))) if read > 0 => Ok((connection, line)),
        Ok(Some((_, Ok(_), _))) => Err("the pipe closed before the reply".to_owned()),
        Ok(Some((_, Err(err), _))) => Err(format!("reading the reply: {err}")),
        Ok(None) => Err("the reply reader ended without a line".to_owned()),
        Err(expiry) => Err(no_reply(&expiry)),
    }
}

/// Waits in slices of at most [`SLICE`] until `wait` brings something, or
/// `deadline` expires.
fn await_reply<T>(
    deadline: &Deadline,
    mut wait: impl FnMut(Duration) -> Option<T>,
) -> Result<T, Expiry> {
    loop {
        if let Some(got) = wait(deadline.wait_slice(SLICE)) {
            return Ok(got);
        }
        if let Some(expiry) = deadline.expired() {
            return Err(expiry);
        }
    }
}

/// The error of a reply that did not come.
fn no_reply(expiry: &Expiry) -> String {
    format!("hung: no reply: {expiry}")
}

/// The error of a pipe whose every instance stayed busy.
fn all_busy(path: &str, expiry: &Expiry) -> String {
    format!("opening {path}: every pipe instance stayed busy: {expiry}")
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::{Arc, Mutex, PoisonError};
    use std::time::{Duration, Instant};

    use load_clock::{
        CpuCounters, Deadline, Expiry, ExpiryKind, Figures, LoadClock, LoadSource, Ticker,
    };

    use super::{BUSY_GUARD, REPLY_GUARD, SLICE, all_busy, await_reply, no_reply};
    use crate::launch::{discovery_target, no_discovery, stalled};

    /// A ticker at a fixed base instant plus an offset the test moves.
    pub(crate) struct ManualTicker {
        base: Instant,
        offset: Mutex<Duration>,
    }

    impl Ticker for ManualTicker {
        fn now(&self) -> Instant {
            self.base + *self.offset.lock().unwrap_or_else(PoisonError::into_inner)
        }
    }

    /// A one-processor machine whose counters the test moves.
    struct ManualSource(Arc<Mutex<CpuCounters>>);

    impl LoadSource for ManualSource {
        fn machine(&self) -> Option<CpuCounters> {
            Some(*self.0.lock().unwrap_or_else(PoisonError::into_inner))
        }

        fn own_cpu(&self) -> Option<Duration> {
            Some(Duration::ZERO)
        }
    }

    /// A load clock over a manual source and ticker, so a wait runs without sleeping.
    pub(crate) struct Rig {
        pub(crate) ticker: Arc<ManualTicker>,
        counters: Arc<Mutex<CpuCounters>>,
        pub(crate) clock: Arc<LoadClock>,
    }

    impl Rig {
        pub(crate) fn new() -> Rig {
            let ticker = Arc::new(ManualTicker {
                base: Instant::now(),
                offset: Mutex::new(Duration::ZERO),
            });
            let counters = Arc::new(Mutex::new(CpuCounters {
                busy: Duration::ZERO,
                total: Duration::ZERO,
            }));
            let clock = LoadClock::new(
                Box::new(ManualSource(Arc::clone(&counters))),
                ticker.clone(),
            );
            Rig {
                ticker,
                counters,
                clock,
            }
        }

        /// Moves wall time by `span` with other work leaving `share` of the machine free.
        pub(crate) fn advance(&self, span: Duration, share: f64) {
            *self
                .ticker
                .offset
                .lock()
                .unwrap_or_else(PoisonError::into_inner) += span;
            let mut counters = self.counters.lock().unwrap_or_else(PoisonError::into_inner);
            counters.total += span;
            counters.busy += span.mul_f64(1.0 - share);
        }

        /// Wall time the test has moved so far.
        pub(crate) fn wall(&self) -> Duration {
            *self
                .ticker
                .offset
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
        }
    }

    pub(crate) fn assert_about(actual: Duration, expected: Duration) {
        assert!(
            actual.abs_diff(expected) <= Duration::from_secs(1),
            "expected about {expected:?}, got {actual:?}"
        );
    }

    /// A reply wait that never gets its reply, at a machine free by `share`.
    fn wait_unanswered(rig: &Rig, share: f64) -> Expiry {
        let deadline = Deadline::start(Arc::clone(&rig.clock), REPLY_GUARD);
        let got: Result<(), Expiry> = await_reply(&deadline, |slice| {
            assert!(slice <= SLICE, "a slice of {slice:?}");
            rig.advance(slice, share);
            None
        });
        match got {
            Err(expiry) => expiry,
            Ok(()) => unreachable!("no reply was given"),
        }
    }

    #[test]
    fn a_reply_wait_on_a_loaded_machine_outlasts_its_wall_budget() {
        let rig = Rig::new();
        let expiry = wait_unanswered(&rig, 0.5);
        assert_eq!(expiry.kind, ExpiryKind::Ceiling);
        assert!(expiry.figures.wall > Duration::from_secs(100), "{expiry}");
        assert_about(expiry.figures.wall, Duration::from_secs(140));
    }

    #[test]
    fn a_reply_wait_on_an_idle_machine_ends_at_its_budget() {
        let rig = Rig::new();
        let expiry = wait_unanswered(&rig, 1.0);
        assert_eq!(expiry.kind, ExpiryKind::Ceiling);
        assert_about(expiry.figures.wall, REPLY_GUARD);
    }

    #[test]
    fn a_reply_wait_ends_at_its_backstop_on_a_saturated_machine() {
        let rig = Rig::new();
        let expiry = wait_unanswered(&rig, 0.05);
        assert_eq!(expiry.kind, ExpiryKind::Backstop);
        assert_about(expiry.figures.wall, Duration::from_secs(350));
    }

    #[test]
    fn a_reply_that_comes_ends_the_wait() {
        let rig = Rig::new();
        let deadline = Deadline::start(Arc::clone(&rig.clock), REPLY_GUARD);
        let mut slices = 0;
        let got = await_reply(&deadline, |slice| {
            rig.advance(slice, 1.0);
            slices += 1;
            (slices == 3).then_some("the reply")
        });
        assert_eq!(got, Ok("the reply"));
        assert_eq!(slices, 3);
        assert_about(rig.wall(), 3 * SLICE);
    }

    fn expiry(kind: ExpiryKind, budget: u64, wall: u64, adjusted: u64) -> Expiry {
        Expiry {
            kind,
            figures: Figures {
                budget: Duration::from_secs(budget),
                wall: Duration::from_secs(wall),
                adjusted: Duration::from_secs(adjusted),
            },
        }
    }

    #[test]
    fn each_error_names_what_fired() {
        assert_eq!(
            no_reply(&expiry(ExpiryKind::Ceiling, 70, 140, 70)),
            "hung: no reply: did not finish within 70 s of load-adjusted time (wall 140 s, machine free 50% on average)"
        );
        assert_eq!(BUSY_GUARD, Duration::from_secs(5));
        assert_eq!(
            all_busy(
                r"\\.\pipe\rt-driver-42",
                &expiry(ExpiryKind::Ceiling, 5, 10, 5)
            ),
            r"opening \\.\pipe\rt-driver-42: every pipe instance stayed busy: did not finish within 5 s of load-adjusted time (wall 10 s, machine free 50% on average)"
        );
        let target = discovery_target(42, std::path::Path::new(r"C:\temp\rustling-tulip-driver"));
        assert_eq!(
            no_discovery(&target, &expiry(ExpiryKind::Ceiling, 60, 90, 60)),
            r"hung: no discovery file for pid 42 in C:\temp\rustling-tulip-driver: did not finish within 60 s of load-adjusted time (wall 90 s, machine free 67% on average)"
        );
        assert_eq!(
            stalled(&expiry(ExpiryKind::Stall, 120, 120, 120)),
            "the client stalled: no log line and no CPU for 120 s"
        );
    }
}
