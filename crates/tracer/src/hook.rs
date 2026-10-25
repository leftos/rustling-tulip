//! `rt-tracer hook`: the shim an agent's injected command hooks run.
//!
//! An interactive agent session is spawned with a `--settings` file whose
//! hooks are exec-form command hooks running
//! `<cached rt-tracer.exe> hook --session <id> --config-dir <dir>`. One
//! invocation reads the event JSON from stdin, looks the daemon's current port
//! and auth token up in `daemon.json`, makes the listener on that port prove
//! with `GET /proof` that it holds this run's daemon token, then POSTs the
//! body verbatim to `/hook/<session id>` on the same connection, and exits 0
//! whatever happens: a hook must never fail the agent's turn. A listener that
//! cannot prove it is the daemon is sent nothing more.
//!
//! The tracer's `main` dispatches here on a leading `hook` argument before it
//! builds the tokio runtime or parses the tracer's own CLI, so the daemon's
//! own invocation is unchanged, a hook event never pays for either, and this
//! path stays plain synchronous std code.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The shim's whole-run budget, covering reading stdin, the connect, the
/// proof round trip, the writes and the status line: one deadline, not a
/// timeout per step.
pub(crate) const BUDGET: Duration = Duration::from_secs(2);

/// How much of stdin the scanner pulls per read.
const SCAN_CHUNK: usize = 8 * 1024;

/// The first byte of the UTF-8 byte-order mark.
const BOM_FIRST: u8 = 0xEF;

/// What one shim run did. `main` ignores it; the tests assert on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The daemon answered `2xx`.
    Posted,
    /// The daemon answered, but not `2xx`.
    Rejected,
    /// Nothing usable came back: no full proof answer head or no status line,
    /// a read error, a closed connection, or the deadline.
    NoResponse,
    /// A listener answered the proof request but did not prove it is the
    /// daemon; nothing else was sent.
    Unproven,
    /// The request was never sent: no usable stdin, a bad invocation or
    /// handshake, an unsafe header value, no random challenge, or a connect
    /// or write that failed.
    Skipped,
}

/// The most of the proof answer's head the shim reads; a longer head is not
/// the daemon's.
const PROOF_HEAD_LIMIT: usize = 8 * 1024;

/// What the listener made of the proof request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Proof {
    /// It answered with this run's daemon proof.
    Proven,
    /// A head arrived and failed a check.
    Unproven,
    /// No full head arrived.
    NoResponse,
}

/// The `hook` subcommand's arguments.
struct Args {
    session_id: String,
    config_dir: PathBuf,
}

impl Args {
    /// Parses `--session <id> --config-dir <dir>`. Anything else, a missing
    /// argument, or a session id outside `[A-Za-z0-9_-]` is `None`, which the
    /// caller turns into a silent exit 0.
    fn parse(argv: &[OsString]) -> Option<Self> {
        let mut session_id: Option<String> = None;
        let mut config_dir: Option<PathBuf> = None;
        let mut rest = argv.iter();
        while let Some(arg) = rest.next() {
            match arg.to_str()? {
                "--session" => {
                    let id = rest.next()?.to_str()?;
                    if !is_safe_session_id(id) {
                        return None;
                    }
                    session_id = Some(id.to_owned());
                }
                // A config dir is a path, so it keeps its `OsString` bytes.
                "--config-dir" => config_dir = Some(PathBuf::from(rest.next()?)),
                _ => return None,
            }
        }
        Some(Self {
            session_id: session_id?,
            config_dir: config_dir?,
        })
    }
}

/// Whether `session_id` is safe in a request path: the daemon's ids are hex,
/// dashes and underscores, and any other byte could carry a CRLF or a space
/// into the request line.
fn is_safe_session_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// Whether `token` is safe in a header value: visible ASCII, so no byte of it
/// can end this header or start another.
fn is_safe_token(token: &str) -> bool {
    !token.is_empty() && token.bytes().all(|byte| (b'!'..=b'~').contains(&byte))
}

/// The daemon's current loopback port and auth token.
///
/// The port changes on every daemon start and a tracer-backed session outlives
/// the daemon, so the shim reads both per call rather than taking them from
/// the spawn.
struct Daemon {
    port: u16,
    token: String,
}

/// Runs the `hook` subcommand end to end. Never fails: every path is exit 0.
pub(crate) fn run_from_env(started_nanos: u128) {
    let argv: Vec<OsString> = std::env::args_os().skip(2).collect();
    let _ = run_invocation(&argv, std::io::stdin(), started_nanos, BUDGET);
}

/// One `hook` invocation for an argv tail, apart from the process's own argv
/// and stdin.
pub(crate) fn run_invocation(
    argv: &[OsString],
    stdin: impl Read + Send + 'static,
    started_nanos: u128,
    budget: Duration,
) -> Outcome {
    let Some(args) = Args::parse(argv) else {
        // Drain stdin even for an unusable invocation: the agent must not see
        // a broken pipe mid-write.
        let _ = read_input(stdin, budget);
        return Outcome::Skipped;
    };
    run(
        stdin,
        &args.config_dir,
        &args.session_id,
        started_nanos,
        budget,
    )
}

/// One shim run: read the event from `stdin`, then POST it to the daemon that
/// `config_dir`'s `daemon.json` names.
///
/// `started_nanos` is the wall clock taken when the shim started, sent as
/// `X-RT-Hook-Time` so the daemon can order events from async hooks, which
/// finish out of order. `budget` bounds the whole run.
pub(crate) fn run(
    stdin: impl Read + Send + 'static,
    config_dir: &Path,
    session_id: &str,
    started_nanos: u128,
    budget: Duration,
) -> Outcome {
    let deadline = Instant::now() + budget;
    let Some(left) = left_until(deadline) else {
        return Outcome::Skipped;
    };

    // stdin first, and always: whatever happens next, the agent must not see
    // a broken pipe mid-write.
    let Some(body) = read_input(stdin, left) else {
        return Outcome::Skipped;
    };
    let Some(daemon) = read_daemon(config_dir) else {
        return Outcome::Skipped;
    };
    post(&daemon, session_id, &body, started_nanos, deadline)
}

/// The time left before `deadline`, or `None` once it is spent.
fn left_until(deadline: Instant) -> Option<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    (!left.is_zero()).then_some(left)
}

/// Arms `stream`'s write timeout with the time left before `deadline`.
fn arm_write(stream: &TcpStream, deadline: Instant) -> bool {
    left_until(deadline).is_some_and(|left| stream.set_write_timeout(Some(left)).is_ok())
}

/// Arms `stream`'s read timeout with the time left before `deadline`.
fn arm_read(stream: &TcpStream, deadline: Instant) -> bool {
    left_until(deadline).is_some_and(|left| stream.set_read_timeout(Some(left)).is_ok())
}

/// The daemon's handshake, read fresh on every call.
fn read_daemon(config_dir: &Path) -> Option<Daemon> {
    let text = std::fs::read_to_string(config_dir.join("daemon.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let port = u16::try_from(value.get("port")?.as_u64()?).ok()?;
    let token = value.get("auth_token")?.as_str()?.to_owned();
    Some(Daemon { port, token })
}

/// Reads the first complete JSON value from `reader`, or `None`.
///
/// The read runs on its own thread because stdin has no read timeout: the
/// caller waits `budget` for it and leaves the thread behind when it does not
/// answer, which is fine because the process is about to exit.
fn read_input(reader: impl Read + Send + 'static, budget: Duration) -> Option<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::with_capacity(SCAN_CHUNK, reader);
        let (body, drain) = match scan(&mut reader) {
            Scanned::Value(value) => (Some(value), false),
            Scanned::Nothing => (None, false),
            // The input can never hold a value: report that at once, then keep
            // reading so a writer still mid-write sees no broken pipe.
            Scanned::Unusable => (None, true),
        };
        let _ = tx.send(body);
        if drain {
            drain_to_eof(&mut reader);
        }
    });
    rx.recv_timeout(budget).unwrap_or(None)
}

/// Reads the rest of `reader`, discarding it: the caller has already been
/// answered and the process is about to exit.
fn drain_to_eof(reader: &mut impl Read) {
    let mut sink = [0u8; SCAN_CHUNK];
    while let Ok(read) = reader.read(&mut sink) {
        if read == 0 {
            break;
        }
    }
}

/// What the scanner made of the input.
#[derive(Debug, PartialEq, Eq)]
enum Scanned {
    /// The first complete JSON value, byte for byte, with no leading
    /// whitespace or byte-order mark.
    Value(Vec<u8>),
    /// The input ended without a value.
    Nothing,
    /// The input can never hold a value.
    Unusable,
}

/// What one byte did to the scan.
#[derive(Debug, Clone, Copy)]
enum Step {
    /// Keep reading.
    More,
    /// The value is complete.
    Done,
    /// The input can never hold a value.
    Bail,
}

/// The streaming state of the one JSON value being read.
///
/// The prefix is skipped rather than collected, so what the scanner keeps is
/// the value's own bytes and the posted body is byte-identical to the agent's.
#[derive(Default)]
struct ScanState {
    value: Vec<u8>,
    started: bool,
    depth: i32,
    in_string: bool,
    escaped: bool,
    bom: u8,
}

impl ScanState {
    /// Feeds one byte, whichever chunk boundary it arrived on.
    fn push(&mut self, byte: u8) -> Step {
        if self.bom > 0 {
            let expected = if self.bom == 1 { 0xBB } else { 0xBF };
            if byte != expected {
                return Step::Bail;
            }
            self.bom = if self.bom == 1 { 2 } else { 0 };
            return Step::More;
        }
        if !self.started {
            if byte == BOM_FIRST {
                self.bom = 1;
                return Step::More;
            }
            if byte.is_ascii_whitespace() {
                return Step::More;
            }
            if byte != b'{' && byte != b'[' {
                return Step::Bail;
            }
            self.started = true;
        }
        self.value.push(byte);
        if self.in_string {
            match byte {
                b'\\' if !self.escaped => self.escaped = true,
                b'"' if !self.escaped => self.in_string = false,
                _ => self.escaped = false,
            }
            return Step::More;
        }
        match byte {
            b'"' => self.in_string = true,
            b'{' | b'[' => self.depth += 1,
            b'}' | b']' => {
                self.depth -= 1;
                if self.depth == 0 {
                    // The value closes here; whatever follows it is the next.
                    return if serde_json::from_slice::<serde_json::Value>(&self.value).is_ok() {
                        Step::Done
                    } else {
                        Step::Bail
                    };
                }
            }
            _ => {}
        }
        Step::More
    }
}

/// Reads the first complete JSON value off `reader`.
///
/// Chunks come from a `BufReader`, so a multi-megabyte event costs a handful
/// of reads, while the scan still stops at the value's last byte instead of
/// waiting for an EOF some agents never send. Only a top-level object or array
/// is a value: every event the injected hooks send is one.
fn scan(reader: &mut impl BufRead) -> Scanned {
    let mut state = ScanState::default();
    loop {
        let (used, step) = {
            let Ok(chunk) = reader.fill_buf() else {
                return Scanned::Nothing;
            };
            if chunk.is_empty() {
                return Scanned::Nothing;
            }
            let mut used = 0;
            let mut step = Step::More;
            for &byte in chunk {
                used += 1;
                step = state.push(byte);
                if !matches!(step, Step::More) {
                    break;
                }
            }
            (used, step)
        };
        match step {
            Step::Done => return Scanned::Value(state.value),
            Step::Bail => return Scanned::Unusable,
            Step::More => reader.consume(used),
        }
    }
}

/// POSTs `body` verbatim to the daemon's `/hook/<session id>` route, once the
/// listener has proved on the same connection that it holds the daemon token.
fn post(
    daemon: &Daemon,
    session_id: &str,
    body: &[u8],
    started_nanos: u128,
    deadline: Instant,
) -> Outcome {
    if !is_safe_session_id(session_id) || !is_safe_token(&daemon.token) {
        return Outcome::Skipped;
    }
    let mut challenge = [0u8; 32];
    if getrandom::fill(&mut challenge).is_err() {
        return Outcome::Skipped;
    }
    let addr = SocketAddr::from(([127, 0, 0, 1], daemon.port));
    let Some(left) = left_until(deadline) else {
        return Outcome::Skipped;
    };
    let Ok(stream) = TcpStream::connect_timeout(&addr, left) else {
        return Outcome::Skipped;
    };
    // One reader for the whole connection, so no byte it buffers is lost
    // between the proof answer and the status line.
    let mut reader = BufReader::new(&stream);
    if let Err(outcome) = prove(daemon, &mut reader, &stream, &challenge, deadline) {
        return outcome;
    }
    let mut writer = &stream;
    let request = format!(
        "POST /hook/{session_id} HTTP/1.1\r\n\
         Host: 127.0.0.1:{port}\r\n\
         Authorization: Bearer {token}\r\n\
         X-RT-Hook-Time: {started_nanos}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {length}\r\n\
         Connection: close\r\n\
         \r\n",
        port = daemon.port,
        token = daemon.token,
        length = body.len(),
    );
    // Each step gets the time actually left, so the whole run cannot spend the
    // budget once per socket operation.
    if !arm_write(&stream, deadline) || writer.write_all(request.as_bytes()).is_err() {
        return Outcome::Skipped;
    }
    if !arm_write(&stream, deadline) || writer.write_all(body).is_err() {
        return Outcome::Skipped;
    }
    let _ = writer.flush();
    if !arm_read(&stream, deadline) {
        return Outcome::NoResponse;
    }
    // The daemon answers `204` at once, and its body says nothing the shim
    // needs, so the status line is all it reads.
    let mut status_line = String::new();
    if reader.read_line(&mut status_line).is_err() {
        return Outcome::NoResponse;
    }
    status_outcome(&status_line)
}

/// Sends the proof request for `challenge` and checks the answer; `Err` is
/// the run's outcome when the listener did not prove it is the daemon.
fn prove(
    daemon: &Daemon,
    reader: &mut BufReader<&TcpStream>,
    stream: &TcpStream,
    challenge: &[u8; 32],
    deadline: Instant,
) -> Result<(), Outcome> {
    let proof_request = format!(
        "GET /proof HTTP/1.1\r\n\
         Host: 127.0.0.1:{port}\r\n\
         X-RT-Challenge: {challenge}\r\n\
         \r\n",
        port = daemon.port,
        challenge = tracer_protocol::to_lower_hex(challenge),
    );
    let mut writer = stream;
    if !arm_write(stream, deadline) || writer.write_all(proof_request.as_bytes()).is_err() {
        return Err(Outcome::Skipped);
    }
    let _ = writer.flush();
    let proof_key = tracer_protocol::daemon_proof_key(&daemon.token);
    match read_proof(reader, stream, deadline, &proof_key, challenge) {
        Proof::Proven => Ok(()),
        Proof::Unproven => Err(Outcome::Unproven),
        Proof::NoResponse => Err(Outcome::NoResponse),
    }
}

/// Reads the listener's answer to the proof request and checks it.
fn read_proof(
    reader: &mut BufReader<&TcpStream>,
    stream: &TcpStream,
    deadline: Instant,
    proof_key: &[u8; 32],
    challenge: &[u8; 32],
) -> Proof {
    match read_response_head(reader, stream, deadline) {
        Head::Complete(head) => {
            let proven = std::str::from_utf8(&head)
                .ok()
                .and_then(tracer_protocol::parse_daemon_proof_head)
                .is_some_and(|proof| {
                    tracer_protocol::verify_daemon_proof(proof_key, challenge, &proof)
                });
            if proven {
                Proof::Proven
            } else {
                Proof::Unproven
            }
        }
        Head::TooLarge => Proof::Unproven,
        Head::Missing => Proof::NoResponse,
    }
}

/// What came back as a response head.
enum Head {
    /// The head through its blank line, without the blank line.
    Complete(Vec<u8>),
    /// The head ran past [`PROOF_HEAD_LIMIT`] bytes, complete or not.
    TooLarge,
    /// A read error, a closed connection or the deadline came first.
    Missing,
}

/// Reads a response head up to its `\r\n\r\n`, arming each read with the time
/// left, and consumes exactly the head so what follows stays in `reader`.
fn read_response_head(
    reader: &mut BufReader<&TcpStream>,
    stream: &TcpStream,
    deadline: Instant,
) -> Head {
    let mut head: Vec<u8> = Vec::new();
    loop {
        if !arm_read(stream, deadline) {
            return Head::Missing;
        }
        let Ok(chunk) = reader.fill_buf() else {
            return Head::Missing;
        };
        if chunk.is_empty() {
            return Head::Missing;
        }
        let scanned_from = head.len().saturating_sub(3);
        let before = head.len();
        head.extend_from_slice(chunk);
        let end = head[scanned_from..]
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|at| scanned_from + at);
        if let Some(end) = end {
            if end > PROOF_HEAD_LIMIT {
                return Head::TooLarge;
            }
            reader.consume(end + 4 - before);
            head.truncate(end);
            return Head::Complete(head);
        }
        let used = head.len() - before;
        reader.consume(used);
        if head.len() > PROOF_HEAD_LIMIT {
            return Head::TooLarge;
        }
    }
}

/// The status code a status line carries, if it is one.
fn status_code(status_line: &str) -> Option<u16> {
    status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
}

/// The status line's code as an outcome: `2xx` is `Posted`, any other code is
/// `Rejected`, and anything that is not a status line is `NoResponse`.
fn status_outcome(status_line: &str) -> Outcome {
    match status_code(status_line) {
        Some(200..=299) => Outcome::Posted,
        Some(_) => Outcome::Rejected,
        None => Outcome::NoResponse,
    }
}

/// Nanoseconds since the Unix epoch.
pub(crate) fn unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos())
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests fail loudly on setup errors")]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    /// The body the shim must post byte for byte: whitespace inside it and a
    /// non-ASCII character, and no trailing whitespace (the shim stops at the
    /// value's closing brace).
    const INPUT: &str =
        "{\n  \"hook_event_name\": \"PreToolUse\",\n  \"prompt\": \"héllo → wörld\"\n}";
    const SESSION: &str = "1f0a4c3e-7b2d-4e5a-9c8b-2d6f0a1b3c4d";
    const TOKEN: &str = "tok-6f1c";
    /// A hang guard, never a must-beat bound: a passing run returns in
    /// milliseconds and never reaches it, while the tens of seconds a busy
    /// machine stalls a `BelowNormal` command for cannot turn a correct run red.
    const GUARD: Duration = Duration::from_secs(120);
    /// The budget the passing cases pass. Only a run that waits for an EOF
    /// that never comes reaches it.
    const LONG_BUDGET: Duration = Duration::from_secs(120);
    /// A budget long enough that waiting it out blows `GUARD`, so a case that
    /// must decide at once is red when it does not.
    const HELD_OPEN_BUDGET: Duration = Duration::from_secs(600);

    #[test]
    fn posts_the_body_verbatim_with_token_and_time() {
        let fake = FakeDaemon::answering();
        let dir = scratch_dir("verbatim");
        write_daemon_json(&dir, fake.port, TOKEN);
        let started = unix_nanos();

        let outcome = run(
            Cursor::new(INPUT.as_bytes().to_vec()),
            &dir,
            SESSION,
            started,
            LONG_BUDGET,
        );

        assert_eq!(outcome, Outcome::Posted);
        let request = fake.finish();
        let head = String::from_utf8_lossy(&request).to_string();
        assert!(
            head.starts_with(&format!("POST /hook/{SESSION} HTTP/1.1\r\n")),
            "request line: {head}"
        );
        assert!(
            head.contains(&format!("Authorization: Bearer {TOKEN}\r\n")),
            "auth header: {head}"
        );
        assert!(
            head.contains(&format!("X-RT-Hook-Time: {started}\r\n")),
            "hook time header: {head}"
        );
        assert!(
            head.contains("Content-Type: application/json\r\n"),
            "content type: {head}"
        );
        assert!(request.ends_with(INPUT.as_bytes()), "body: {head}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stops_at_the_first_json_value() {
        let fake = FakeDaemon::answering();
        let dir = scratch_dir("first-value");
        write_daemon_json(&dir, fake.port, TOKEN);
        let started = Instant::now();

        let outcome = run(
            BlockingAfter::new(INPUT.as_bytes().to_vec()),
            &dir,
            SESSION,
            unix_nanos(),
            LONG_BUDGET,
        );

        assert!(
            started.elapsed() < GUARD,
            "the shim hung on a blocking reader"
        );
        assert_eq!(outcome, Outcome::Posted, "the reader never sent EOF");
        let request = fake.finish();
        assert!(request.ends_with(INPUT.as_bytes()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_daemon_json_drains_stdin_and_returns() {
        // The directory exists but holds no `daemon.json`.
        let dir = scratch_dir("missing-daemon");
        let consumed = Arc::new(AtomicUsize::new(0));
        let started = Instant::now();

        let outcome = run(
            CountingReader::new(INPUT.as_bytes().to_vec(), Arc::clone(&consumed)),
            &dir,
            SESSION,
            unix_nanos(),
            LONG_BUDGET,
        );

        assert!(started.elapsed() < GUARD);
        assert_eq!(outcome, Outcome::Skipped);
        assert_eq!(
            consumed.load(Ordering::SeqCst),
            INPUT.len(),
            "stdin must be read before the handshake lookup"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_connect_returns() {
        // Port 0 is not a port anything can listen on, so the connect fails
        // however the rest of the machine is loaded.
        let dir = scratch_dir("failed-connect");
        write_daemon_json(&dir, 0, TOKEN);
        let started = Instant::now();

        let outcome = run(
            Cursor::new(INPUT.as_bytes().to_vec()),
            &dir,
            SESSION,
            unix_nanos(),
            LONG_BUDGET,
        );

        assert!(started.elapsed() < GUARD, "the connect must not hang");
        assert_eq!(outcome, Outcome::Skipped);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_silent_listener_is_no_response() {
        let (port, head_rx) = silent_daemon();
        let dir = scratch_dir("silent");
        write_daemon_json(&dir, port, TOKEN);
        let started = Instant::now();

        let outcome = run(
            Cursor::new(INPUT.as_bytes().to_vec()),
            &dir,
            SESSION,
            unix_nanos(),
            // The product's own short budget, through the seam: this test
            // pins the give-up path and asserts nothing about how long it took.
            Duration::from_millis(200),
        );

        assert!(
            started.elapsed() < GUARD,
            "the shim hung on a silent listener"
        );
        assert_eq!(
            outcome,
            Outcome::NoResponse,
            "the proof request was sent and never answered"
        );
        let head = head_rx
            .recv_timeout(GUARD)
            .expect("the proof request reached the listener");
        let head = String::from_utf8_lossy(&head).to_string();
        assert!(
            head.starts_with("GET /proof HTTP/1.1\r\n"),
            "request line: {head}"
        );
        assert!(
            challenge_of(head.as_bytes()).is_some(),
            "a challenge header: {head}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn posts_after_a_real_proof_exchange() {
        let fake = FakeDaemon::answering();
        let dir = scratch_dir("proof-exchange");
        write_daemon_json(&dir, fake.port, TOKEN);

        let outcome = run(
            Cursor::new(INPUT.as_bytes().to_vec()),
            &dir,
            SESSION,
            unix_nanos(),
            LONG_BUDGET,
        );

        assert_eq!(outcome, Outcome::Posted);
        let port = fake.port;
        // The fake accepts one connection, so the POST rode the proved one.
        let exchange = fake.finish_exchange();
        let proof_head = String::from_utf8_lossy(&exchange.proof_head).to_string();
        assert!(
            proof_head.starts_with(&format!(
                "GET /proof HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nX-RT-Challenge: "
            )),
            "proof request: {proof_head}"
        );
        assert!(
            challenge_of(&exchange.proof_head).is_some(),
            "a 64 lowercase hex challenge: {proof_head}"
        );
        assert!(
            !proof_head.contains("Connection: close"),
            "the proof request keeps the connection open: {proof_head}"
        );
        let request = String::from_utf8_lossy(&exchange.request).to_string();
        assert!(
            request.starts_with(&format!("POST /hook/{SESSION} HTTP/1.1\r\n")),
            "request line: {request}"
        );
        assert!(exchange.request.ends_with(INPUT.as_bytes()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sends_nothing_to_a_listener_without_the_proof() {
        let fake = UnprovenFake::answering(|_| "HTTP/1.1 204 No Content\r\n\r\n".to_owned());
        assert_unproven(fake, "no-proof");
    }

    #[test]
    fn sends_nothing_to_a_listener_with_a_wrong_proof() {
        let fake = UnprovenFake::answering(|challenge| {
            let wrong = tracer_protocol::daemon_proof(
                &tracer_protocol::daemon_proof_key("not-the-token"),
                challenge,
            );
            proof_answer(&wrong, "")
        });
        assert_unproven(fake, "wrong-proof");
    }

    #[test]
    fn a_proof_answer_with_a_body_is_unproven() {
        let fake = UnprovenFake::answering(|challenge| {
            let mut answer =
                proof_answer(&correct_proof(TOKEN, challenge), "Content-Length: 5\r\n");
            answer.push_str("hello");
            answer
        });
        assert_unproven(fake, "proof-body");
    }

    #[test]
    fn a_proof_answer_with_two_proof_headers_is_unproven() {
        let fake = UnprovenFake::answering(|challenge| {
            let proof = tracer_protocol::to_lower_hex(&correct_proof(TOKEN, challenge));
            proof_answer(
                &correct_proof(TOKEN, challenge),
                &format!("X-RT-Proof: {proof}\r\n"),
            )
        });
        assert_unproven(fake, "two-proofs");
    }

    #[test]
    fn a_proof_head_over_the_limit_is_unproven() {
        // A correct proof whose head, terminator included, comes in one write
        // and runs past the limit.
        let fake = UnprovenFake::answering(|challenge| {
            let padding = format!("X-Padding: {}\r\n", "a".repeat(PROOF_HEAD_LIMIT));
            proof_answer(&correct_proof(TOKEN, challenge), &padding)
        });
        assert_unproven(fake, "proof-head-limit");
    }

    /// Runs the shim against `fake` and asserts it is `Unproven` and that the
    /// fake saw nothing after the proof request.
    fn assert_unproven(fake: UnprovenFake, name: &str) {
        let dir = scratch_dir(name);
        write_daemon_json(&dir, fake.port, TOKEN);

        let outcome = run(
            Cursor::new(INPUT.as_bytes().to_vec()),
            &dir,
            SESSION,
            unix_nanos(),
            LONG_BUDGET,
        );

        assert_eq!(outcome, Outcome::Unproven);
        let after = fake.handle.join().expect("the fake daemon thread");
        assert_eq!(
            after, 0,
            "the shim sent {after} bytes after an unproven answer"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_a_non_2xx_answer() {
        let fake = FakeDaemon::replying("HTTP/1.1 401 Unauthorized\r\nConnection: close\r\n\r\n");
        let dir = scratch_dir("rejected");
        write_daemon_json(&dir, fake.port, TOKEN);

        let outcome = run(
            Cursor::new(INPUT.as_bytes().to_vec()),
            &dir,
            SESSION,
            unix_nanos(),
            LONG_BUDGET,
        );

        assert_eq!(outcome, Outcome::Rejected);
        let request = fake.finish();
        assert!(request.ends_with(INPUT.as_bytes()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_an_unsafe_token() {
        // The fake proves itself for this token and would accept the event, so
        // a request that got through would be `Posted`.
        let unsafe_token = "ok\r\nX-Injected: yes";
        let fake = FakeDaemon::answering_for(unsafe_token);
        let dir = scratch_dir("unsafe-token");
        write_daemon_json(&dir, fake.port, unsafe_token);
        let daemon = read_daemon(&dir).expect("the daemon.json parses");
        assert_eq!(daemon.token, unsafe_token, "only the token is unsafe");

        let outcome = run(
            Cursor::new(INPUT.as_bytes().to_vec()),
            &dir,
            SESSION,
            unix_nanos(),
            LONG_BUDGET,
        );

        assert_eq!(outcome, Outcome::Skipped);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unusable_arguments_skip_and_drain_stdin() {
        // The handshake is there and the fake would answer, so a case that got
        // as far as a request would be `Posted`: only the argument check can
        // make these `Skipped`.
        let fake = FakeDaemon::answering();
        let dir = scratch_dir("bad-args");
        write_daemon_json(&dir, fake.port, TOKEN);
        let dir_arg = dir.clone().into_os_string();
        let cases = [
            vec![],
            vec![OsString::from("--nonsense")],
            vec![OsString::from("--session")],
            vec![
                OsString::from("--session"),
                OsString::from(""),
                OsString::from("--config-dir"),
                dir_arg.clone(),
            ],
            vec![
                OsString::from("--session"),
                OsString::from("bad\r\nid"),
                OsString::from("--config-dir"),
                dir_arg.clone(),
            ],
            vec![
                OsString::from("--session"),
                OsString::from("bad id"),
                OsString::from("--config-dir"),
                dir_arg.clone(),
            ],
            vec![OsString::from("--session"), OsString::from(SESSION)],
        ];

        for argv in cases {
            let consumed = Arc::new(AtomicUsize::new(0));
            let outcome = run_invocation(
                &argv,
                CountingReader::new(INPUT.as_bytes().to_vec(), Arc::clone(&consumed)),
                unix_nanos(),
                LONG_BUDGET,
            );
            assert_eq!(outcome, Outcome::Skipped, "argv: {argv:?}");
            assert_eq!(
                consumed.load(Ordering::SeqCst),
                INPUT.len(),
                "stdin must be drained for argv: {argv:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn skips_leading_whitespace_and_a_bom() {
        let value = INPUT.as_bytes().to_vec();

        let mut padded = b"\n \t\r\n  ".to_vec();
        padded.extend_from_slice(&value);
        assert_eq!(
            scan(&mut BufReader::new(Cursor::new(padded))),
            Scanned::Value(value.clone())
        );

        let mut bom = vec![0xEF, 0xBB, 0xBF];
        bom.extend_from_slice(&value);
        assert_eq!(
            scan(&mut BufReader::new(Cursor::new(bom))),
            Scanned::Value(value.clone())
        );

        // One byte per read, so the prefix spans chunk boundaries.
        let mut dripped = vec![0xEF, 0xBB, 0xBF, b'\n'];
        dripped.extend_from_slice(&value);
        assert_eq!(
            scan(&mut BufReader::new(DripReader::new(dripped))),
            Scanned::Value(value)
        );
    }

    #[test]
    fn scans_escapes_inside_strings() {
        // A `}` inside a string, and `\"` and `\\` escapes, must not end the
        // value early.
        let input = br#"{"quote": "he said \"hi\"", "slash": "a\\b", "brace": "}"}"#;
        assert_eq!(
            scan(&mut BufReader::new(Cursor::new(input.to_vec()))),
            Scanned::Value(input.to_vec())
        );
    }

    #[test]
    fn an_unusable_prefix_is_reported_at_once() {
        for input in [&b"] nope"[..], &b"{\"broken\": }"[..], &[0xEF, 0x00]] {
            assert_eq!(
                scan(&mut BufReader::new(Cursor::new(input.to_vec()))),
                Scanned::Unusable,
                "input: {input:?}"
            );
        }
    }

    #[test]
    fn blocked_stdin_with_an_unusable_prefix_returns_at_once() {
        // The input can never become a value, so the shim must not wait for
        // the EOF this reader never sends: `HELD_OPEN_BUDGET` is long enough
        // that waiting it out blows the guard.
        let dir = scratch_dir("held-open-useless");
        let started = Instant::now();

        let outcome = run(
            BlockingAfter::new(b"]".to_vec()),
            &dir,
            SESSION,
            unix_nanos(),
            HELD_OPEN_BUDGET,
        );

        assert!(
            started.elapsed() < GUARD,
            "the shim waited for an EOF that never comes"
        );
        assert_eq!(outcome, Outcome::Skipped);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn drains_the_rest_of_an_unusable_body() {
        // A body the scanner rejects, then data the writer keeps sending,
        // then a close, with no `daemon.json`: the whole input is consumed,
        // so the writer never sees a broken pipe.
        let dir = scratch_dir("drain");
        let input = b"{\"broken\": } and the writer keeps going".to_vec();
        let consumed = Arc::new(AtomicUsize::new(0));

        let outcome = run(
            CountingReader::new(input.clone(), Arc::clone(&consumed)),
            &dir,
            SESSION,
            unix_nanos(),
            LONG_BUDGET,
        );

        assert_eq!(outcome, Outcome::Skipped);
        wait_for_consumed(&consumed, input.len());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The answer a fake gives the hook POST when it accepts the event.
    const NO_CONTENT: &str = "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n";

    /// What a fake daemon read off its one connection.
    struct Exchange {
        /// The proof request's head.
        proof_head: Vec<u8>,
        /// The hook request that followed the proof, head and body.
        request: Vec<u8>,
    }

    /// A one-shot fake daemon on a free loopback port: it proves itself for
    /// the token it was given, reads the hook request on the same connection,
    /// answers it and hands the raw bytes back.
    struct FakeDaemon {
        port: u16,
        handle: thread::JoinHandle<Exchange>,
    }

    impl FakeDaemon {
        /// One that proves itself for [`TOKEN`] and answers `204 No Content`.
        fn answering() -> Self {
            Self::answering_for(TOKEN)
        }

        /// One that proves itself for `token` and answers `204 No Content`.
        fn answering_for(token: &str) -> Self {
            Self::serving(token, NO_CONTENT)
        }

        /// One that proves itself for [`TOKEN`] and answers the hook request
        /// with `response` verbatim.
        fn replying(response: &'static str) -> Self {
            Self::serving(TOKEN, response)
        }

        fn serving(token: &str, response: &'static str) -> Self {
            let proof_key = tracer_protocol::daemon_proof_key(token);
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind the fake daemon");
            let port = listener.local_addr().expect("fake daemon addr").port();
            let handle = thread::spawn(move || {
                let (mut stream, _) = listener.accept().expect("accept the shim");
                let proof_head = read_head(&mut stream);
                let challenge = challenge_of(&proof_head).expect("the shim sent a challenge");
                let proof = tracer_protocol::daemon_proof(&proof_key, &challenge);
                let _ = stream.write_all(proof_answer(&proof, "").as_bytes());
                let _ = stream.flush();
                let request = read_request(&mut stream);
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
                Exchange {
                    proof_head,
                    request,
                }
            });
            Self { port, handle }
        }

        /// The hook request the fake read.
        fn finish(self) -> Vec<u8> {
            self.finish_exchange().request
        }

        fn finish_exchange(self) -> Exchange {
            self.handle.join().expect("the fake daemon thread")
        }
    }

    /// A fake listener that answers the proof request with whatever its
    /// `answer` builds from the challenge, then counts the bytes the shim
    /// sends after it: a shim that drops the stream sends none.
    struct UnprovenFake {
        port: u16,
        handle: thread::JoinHandle<usize>,
    }

    impl UnprovenFake {
        fn answering(answer: fn(&[u8; 32]) -> String) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind the fake listener");
            let port = listener.local_addr().expect("fake listener addr").port();
            let handle = thread::spawn(move || {
                let (mut stream, _) = listener.accept().expect("accept the shim");
                let head = read_head(&mut stream);
                let challenge = challenge_of(&head).expect("the shim sent a challenge");
                let _ = stream.write_all(answer(&challenge).as_bytes());
                let _ = stream.flush();
                // EOF, or a reset when the shim closed with our answer still
                // unread: either way no second request arrived.
                let mut after = [0u8; 1024];
                stream.read(&mut after).unwrap_or(0)
            });
            Self { port, handle }
        }
    }

    /// The `204` proof answer carrying `proof`, with `extra` header lines.
    fn proof_answer(proof: &[u8; 32], extra: &str) -> String {
        format!(
            "HTTP/1.1 204 No Content\r\nx-rt-proof: {}\r\n{extra}\r\n",
            tracer_protocol::to_lower_hex(proof)
        )
    }

    /// The proof a daemon holding `token` gives for `challenge`.
    fn correct_proof(token: &str, challenge: &[u8; 32]) -> [u8; 32] {
        tracer_protocol::daemon_proof(&tracer_protocol::daemon_proof_key(token), challenge)
    }

    /// The challenge a proof request head carries, when it is 64 lowercase hex.
    fn challenge_of(head: &[u8]) -> Option<[u8; 32]> {
        String::from_utf8_lossy(head)
            .lines()
            .find_map(|line| line.strip_prefix("X-RT-Challenge: "))
            .and_then(tracer_protocol::parse_lower_hex32)
    }

    /// A fake daemon that reads the request head and then never answers, on a
    /// standard-library channel so no sleep or poll is involved.
    fn silent_daemon() -> (u16, mpsc::Receiver<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the silent daemon");
        let port = listener.local_addr().expect("silent daemon addr").port();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept the shim");
            let head = read_head(&mut stream);
            let _ = tx.send(head);
            // Hold the connection open without answering: the shim must give
            // up on its own budget, not on this server.
            let (_keep, held) = mpsc::channel::<()>();
            let _ = held.recv();
        });
        (port, rx)
    }

    /// Reads a request line and its headers.
    fn read_head(stream: &mut std::net::TcpStream) -> Vec<u8> {
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if stream.read(&mut byte).expect("read the request head") == 0 {
                break;
            }
            head.push(byte[0]);
        }
        head
    }

    /// The head plus exactly the number of body bytes its `Content-Length`
    /// names, so a test can compare the whole request byte for byte.
    fn read_request(stream: &mut std::net::TcpStream) -> Vec<u8> {
        let mut request = read_head(stream);
        let head = String::from_utf8_lossy(&request).to_string();
        let length: usize = head
            .lines()
            .find_map(|line| line.strip_prefix("Content-Length: "))
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or(0);
        let mut body = vec![0u8; length];
        stream.read_exact(&mut body).expect("read the request body");
        request.extend_from_slice(&body);
        request
    }

    /// Writes the `daemon.json` a daemon would have left in its config dir.
    fn write_daemon_json(dir: &Path, port: u16, token: &str) {
        let json = serde_json::json!({
            "protocol_version": 24,
            "port": port,
            "auth_token": token,
            "pid": 1,
        });
        std::fs::write(dir.join("daemon.json"), json.to_string()).expect("write daemon.json");
    }

    /// A scratch directory under the worktree's `.tmp`, unique per test.
    fn scratch_dir(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.tmp/hook-shim-tests")
            .join(format!("{name}-{}-{}", std::process::id(), unix_nanos()));
        std::fs::create_dir_all(&dir).expect("create the scratch dir");
        dir
    }

    /// A reader that counts the bytes it hands out, so a test can tell stdin
    /// was read even when nothing was posted.
    struct CountingReader {
        inner: Cursor<Vec<u8>>,
        consumed: Arc<AtomicUsize>,
    }

    impl CountingReader {
        fn new(input: Vec<u8>, consumed: Arc<AtomicUsize>) -> Self {
            Self {
                inner: Cursor::new(input),
                consumed,
            }
        }
    }

    impl Read for CountingReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let read = self.inner.read(buf)?;
            self.consumed.fetch_add(read, Ordering::SeqCst);
            Ok(read)
        }
    }

    /// A reader that yields `input` and then blocks forever: an agent that
    /// keeps stdin open after its hook event.
    struct BlockingAfter {
        inner: Cursor<Vec<u8>>,
        held: mpsc::Receiver<()>,
        _never_sent: mpsc::Sender<()>,
    }

    impl BlockingAfter {
        fn new(input: Vec<u8>) -> Self {
            let (never_sent, held) = mpsc::channel();
            Self {
                inner: Cursor::new(input),
                held,
                _never_sent: never_sent,
            }
        }
    }

    impl Read for BlockingAfter {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let read = self.inner.read(buf)?;
            if read == 0 {
                let _ = self.held.recv();
            }
            Ok(read)
        }
    }

    /// A reader that hands out one byte per call: the worst case for a scanner
    /// that assumes its chunk holds a whole prefix.
    struct DripReader {
        inner: Cursor<Vec<u8>>,
    }

    impl DripReader {
        fn new(input: Vec<u8>) -> Self {
            Self {
                inner: Cursor::new(input),
            }
        }
    }

    impl Read for DripReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let mut one = [0u8; 1];
            let read = self.inner.read(&mut one)?;
            if read == 1 && !buf.is_empty() {
                buf[0] = one[0];
            }
            Ok(read)
        }
    }

    /// Waits for the drain that runs after the shim has already answered to
    /// reach the input's end: a wait on the counter, with the hang guard as
    /// its only deadline.
    fn wait_for_consumed(consumed: &AtomicUsize, expected: usize) {
        let deadline = Instant::now() + GUARD;
        while consumed.load(Ordering::SeqCst) < expected && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            consumed.load(Ordering::SeqCst),
            expected,
            "stdin was not drained"
        );
    }
}
