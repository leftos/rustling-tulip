//! The client side of a driven client's pipe: one request line out, the
//! reply line with the same id back, within [`REPLY_GUARD`].
//!
//! The reply and busy-pipe ceilings count load-adjusted time on the
//! process's one [`clock`], with a backstop of 5x their budget in wall time,
//! so other work loading the machine does not turn them into false hangs;
//! the poll sleeps between busy retries stay wall time.
//!
//! A reply that does not come is cancelled: on Windows the reader thread's
//! blocked read is cancelled (`CancelSynchronousIo`) until it hands the
//! connection back, a `cancel` of the request goes out on that connection,
//! its answer is awaited for [`CANCEL_REPLY_GUARD`], and the connection is
//! dropped, so the next call opens a fresh one.

use std::fs::File;
use std::io::{self, BufRead as _, BufReader, Write as _};
use std::sync::{Arc, OnceLock, mpsc};
use std::thread::JoinHandle;
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

/// How long a timed-out call keeps cancelling its reader's pending read, in
/// wall time: it bounds an OS call, not work the machine's load slows.
#[cfg(windows)]
const CANCEL_READ_GUARD: Duration = Duration::from_secs(5);

/// How long each cancel of a pending read waits for the reader's hand-back.
#[cfg(windows)]
const CANCEL_POLL: Duration = Duration::from_millis(50);

/// How long a timed-out call waits for the endpoint's answer to its cancel,
/// in load-adjusted time with a 5x wall backstop.
const CANCEL_REPLY_GUARD: Duration = Duration::from_secs(3);

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
    /// How long a call waits for its reply: [`REPLY_GUARD`] but in specs.
    reply_guard: Duration,
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
            reply_guard: REPLY_GUARD,
        }
    }

    /// A client whose calls wait `reply_guard` for their reply.
    #[cfg(all(test, windows))]
    fn with_reply_guard(path: String, reply_guard: Duration) -> Self {
        Self {
            reply_guard,
            ..Self::new(path)
        }
    }

    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Writes `line` and reads lines until the reply to `id` (or to the
    /// reserved id 0, a refusal of the line itself). A failure drops the
    /// connection, so a late reply is never read as the next call's; a reply
    /// that does not come is first cancelled on the connection, when the
    /// reader hands it back.
    fn exchange(&mut self, line: &str, id: u64) -> Result<Value, String> {
        let mut connection = match self.connection.take() {
            Some(connection) => connection,
            None => open(&self.path)?,
        };
        send_line(connection.get_mut(), line)
            .map_err(|err| format!("writing to {}: {err}", self.path))?;
        let deadline = Deadline::start(clock(), self.reply_guard);
        loop {
            connection = match read_line_by(connection, &deadline) {
                ReadEnd::Line(back, text) => match reply_to(&text, id)? {
                    Some(reply) => {
                        self.connection = Some(back);
                        return Ok(reply);
                    }
                    None => back,
                },
                ReadEnd::Timeout { connection, expiry } => {
                    if let Some(connection) = connection {
                        self.cancel(connection, id);
                    }
                    return Err(no_reply(&expiry));
                }
                ReadEnd::Failed(why) => return Err(why),
            };
        }
    }

    /// Asks the endpoint to cancel `request` on the connection it was sent
    /// on, waits up to [`CANCEL_REPLY_GUARD`] for the answer, and drops the
    /// connection, whose close cancels whatever is still pending there.
    fn cancel(&mut self, mut connection: BufReader<File>, request: u64) {
        let id = self.next_id;
        self.next_id += 1;
        let mut line =
            json!({"id": id, "method": "cancel", "params": {"request": request}}).to_string();
        line.push('\n');
        if let Err(err) = send_line(connection.get_mut(), &line) {
            tracing::debug!(request, %err, "writing the cancel of a timed-out request");
            return;
        }
        let deadline = Deadline::start(clock(), CANCEL_REPLY_GUARD);
        loop {
            connection = match read_line_by(connection, &deadline) {
                ReadEnd::Line(back, text) => match reply_to(&text, id) {
                    Ok(Some(reply)) => {
                        let cancelled = reply["result"]["cancelled"].as_bool();
                        tracing::debug!(request, ?cancelled, "the endpoint answered the cancel");
                        return;
                    }
                    Ok(None) => back,
                    Err(why) => {
                        tracing::debug!(request, why, "the answer to the cancel");
                        return;
                    }
                },
                ReadEnd::Timeout { .. } | ReadEnd::Failed(_) => {
                    tracing::debug!(request, "no answer to the cancel");
                    return;
                }
            };
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

/// Writes `line` whole and flushes it.
fn send_line(pipe: &mut File, line: &str) -> io::Result<()> {
    pipe.write_all(line.as_bytes())?;
    pipe.flush()
}

/// The reply in `text` when it answers `id` (or the reserved id 0), `None`
/// for a reply to another request, which is dropped.
fn reply_to(text: &str, id: u64) -> Result<Option<Value>, String> {
    let reply: Value = serde_json::from_str(text)
        .map_err(|err| format!("the endpoint's reply is not JSON ({err}): {text}"))?;
    match reply.get("id").and_then(Value::as_u64) {
        Some(got) if got == id || got == 0 => Ok(Some(reply)),
        other => {
            tracing::warn!(?other, id, "dropping a reply to another request");
            Ok(None)
        }
    }
}

/// What a reader thread hands back: the connection, the read's result and
/// the line it read.
type HandBack = (BufReader<File>, io::Result<usize>, String);

/// How a [`read_line_by`] ended.
enum ReadEnd {
    /// A line, and the connection to read the next one from.
    Line(BufReader<File>, String),
    /// No line by the deadline; on Windows the reader's read was cancelled
    /// and `connection` is the connection it handed back, if it did.
    Timeout {
        connection: Option<BufReader<File>>,
        expiry: Expiry,
    },
    /// The pipe closed or the read failed; the connection is gone.
    Failed(String),
}

/// One line read on a thread of its own, so the wait ends at `deadline`. On
/// Windows a read still pending then is cancelled and the connection comes
/// back in [`ReadEnd::Timeout`] (or, when the reply raced the cancel, as its
/// line); elsewhere the pending read keeps the connection, which is dropped.
fn read_line_by(mut connection: BufReader<File>, deadline: &Deadline) -> ReadEnd {
    let (send, receive) = mpsc::channel();
    let reader = std::thread::spawn(move || {
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
        Ok(Some((connection, Ok(read), line))) if read > 0 => ReadEnd::Line(connection, line),
        Ok(Some((_, Ok(_), _))) => ReadEnd::Failed("the pipe closed before the reply".to_owned()),
        Ok(Some((_, Err(err), _))) => ReadEnd::Failed(format!("reading the reply: {err}")),
        Ok(None) => ReadEnd::Failed("the reply reader ended without a line".to_owned()),
        Err(expiry) => reclaim(&reader, &receive, expiry),
    }
}

/// Cancels `reader`'s pending read until it hands the connection back, for
/// at most [`CANCEL_READ_GUARD`] of wall time: a cancel made before the
/// thread entered its read does nothing, so it is repeated. A line handed
/// back is the reply, which came before the cancel took effect.
#[cfg(windows)]
fn reclaim(reader: &JoinHandle<()>, receive: &mpsc::Receiver<HandBack>, expiry: Expiry) -> ReadEnd {
    let started = std::time::Instant::now();
    loop {
        cancel_read(reader);
        match receive.recv_timeout(CANCEL_POLL) {
            Ok((connection, Ok(read), line)) if read > 0 => return ReadEnd::Line(connection, line),
            Ok((connection, _, _)) => {
                return ReadEnd::Timeout {
                    connection: Some(connection),
                    expiry,
                };
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) if started.elapsed() >= CANCEL_READ_GUARD => {
                tracing::warn!(
                    "the reply reader kept its read past every cancel; dropping the connection"
                );
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
    ReadEnd::Timeout {
        connection: None,
        expiry,
    }
}

/// Off Windows the pending read keeps the connection, which is dropped.
#[cfg(not(windows))]
fn reclaim(
    _reader: &JoinHandle<()>,
    _receive: &mpsc::Receiver<HandBack>,
    expiry: Expiry,
) -> ReadEnd {
    ReadEnd::Timeout {
        connection: None,
        expiry,
    }
}

/// Cancels the synchronous read `reader` has pending, if it has one yet.
#[cfg(windows)]
fn cancel_read(reader: &JoinHandle<()>) {
    use std::os::windows::io::AsRawHandle as _;
    use windows::Win32::Foundation::{ERROR_NOT_FOUND, HANDLE};
    use windows::Win32::System::IO::CancelSynchronousIo;

    // SAFETY: the handle is the reader thread's, owned by `reader`, which the
    // caller holds across this call, so it is open; std opens a thread with
    // THREAD_TERMINATE access, which the call needs.
    let cancelled = unsafe { CancelSynchronousIo(HANDLE(reader.as_raw_handle())) };
    if let Err(err) = cancelled
        && err.code() != ERROR_NOT_FOUND.to_hresult()
    {
        tracing::debug!(%err, "cancelling the reply reader's read");
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

#[cfg(all(test, windows))]
#[expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the step it lost"
)]
mod cancel_tests {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

    use super::{CallError, PipeClient, Transport as _};

    /// How long a spec waits on the server's next event before it fails as hung.
    const HANG_GUARD: Duration = Duration::from_secs(30);

    /// The reply ceiling of a spec's client, so a call times out soon.
    const SHORT_REPLY_GUARD: Duration = Duration::from_millis(500);

    /// What the server saw, by connection index.
    #[derive(Debug, PartialEq)]
    enum Seen {
        Accepted(u32),
        Line(u32, Value),
        Eof(u32),
    }

    /// A pipe path no other spec uses.
    fn unique_path() -> String {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        format!(r"\\.\pipe\rt-driver-mcp-test-{}-{n}", std::process::id())
    }

    /// Serves a byte-mode pipe on a thread of its own and returns its path
    /// and what it sees. It answers `ping`, and `cancel` when
    /// `answer_cancel`, and nothing else.
    fn serve(answer_cancel: bool) -> (String, mpsc::Receiver<Seen>) {
        let path = unique_path();
        let (seen, events) = mpsc::channel();
        let (ready, is_ready) = mpsc::channel();
        let name = path.clone();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a runtime");
            runtime.block_on(accept(name, answer_cancel, seen, ready));
        });
        is_ready
            .recv_timeout(HANG_GUARD)
            .expect("hung: the server did not start");
        (path, events)
    }

    async fn accept(
        path: String,
        answer_cancel: bool,
        seen: mpsc::Sender<Seen>,
        ready: mpsc::Sender<()>,
    ) {
        let mut server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&path)
            .expect("the pipe is created");
        let _ = ready.send(());
        let mut index = 0;
        loop {
            server.connect().await.expect("a client connects");
            let next = ServerOptions::new()
                .create(&path)
                .expect("the next instance is created");
            let _ = seen.send(Seen::Accepted(index));
            tokio::spawn(answer(server, index, answer_cancel, seen.clone()));
            server = next;
            index += 1;
        }
    }

    /// Reports each line of one connection and its end, answering as
    /// [`serve`] says.
    async fn answer(
        pipe: NamedPipeServer,
        index: u32,
        answer_cancel: bool,
        seen: mpsc::Sender<Seen>,
    ) {
        let (read, mut write) = tokio::io::split(pipe);
        let mut lines = BufReader::new(read).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let request: Value = serde_json::from_str(&line).expect("a request line is JSON");
            let _ = seen.send(Seen::Line(index, request.clone()));
            let result = match request["method"].as_str() {
                Some("ping") => json!({"pong": true}),
                Some("cancel") if answer_cancel => json!({"cancelled": true}),
                _ => continue,
            };
            let mut reply = json!({"id": request["id"], "result": result}).to_string();
            reply.push('\n');
            if write.write_all(reply.as_bytes()).await.is_err() {
                break;
            }
        }
        let _ = seen.send(Seen::Eof(index));
    }

    fn next(events: &mpsc::Receiver<Seen>) -> Seen {
        events
            .recv_timeout(HANG_GUARD)
            .expect("hung: the server saw nothing more")
    }

    /// Calls `hold`, which the server never answers, and checks the call
    /// timed out and its cancel went out on the same connection.
    fn timed_out_hold(client: &mut PipeClient, events: &mpsc::Receiver<Seen>) {
        let got = client.call("hold", json!({}));
        assert!(
            matches!(&got, Err(CallError::Transport(why)) if why.starts_with("hung: no reply")),
            "{got:?}"
        );
        assert_eq!(next(events), Seen::Accepted(0));
        assert_eq!(
            next(events),
            Seen::Line(0, json!({"id": 1, "method": "hold", "params": {}}))
        );
        assert_eq!(
            next(events),
            Seen::Line(
                0,
                json!({"id": 2, "method": "cancel", "params": {"request": 1}})
            )
        );
    }

    #[test]
    fn a_timed_out_call_sends_its_cancel_on_the_same_connection() {
        let (path, events) = serve(false);
        let mut client = PipeClient::with_reply_guard(path, SHORT_REPLY_GUARD);
        timed_out_hold(&mut client, &events);
        assert_eq!(next(&events), Seen::Eof(0));
    }

    #[test]
    fn a_cancel_answer_is_read_and_the_connection_still_closes() {
        let (path, events) = serve(true);
        let mut client = PipeClient::with_reply_guard(path, SHORT_REPLY_GUARD);
        timed_out_hold(&mut client, &events);
        assert_eq!(next(&events), Seen::Eof(0));
        assert_eq!(client.call("ping", json!({})), Ok(json!({"pong": true})));
        assert_eq!(next(&events), Seen::Accepted(1));
        assert_eq!(
            next(&events),
            Seen::Line(1, json!({"id": 3, "method": "ping", "params": {}}))
        );
    }
}
