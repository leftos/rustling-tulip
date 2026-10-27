//! In-process fakes and scratch helpers for this crate's tests: loopback
//! listeners that stand in for a daemon (one that proves itself for a token,
//! one that answers `200` to everything, one that never answers), a scratch
//! config dir, `daemon.json` writers and a harmless stand-in process.
//!
//! Every listener records the head of each request it reads, so a test can
//! assert which requests (and which `Authorization` headers) reached it.

#![expect(
    clippy::expect_used,
    reason = "test helpers assert preconditions with expect; failure messages aid debugging"
)]

use protocol::DaemonHandshake;
use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, PoisonError, mpsc};
use std::thread;
use std::time::Duration;
use uuid::Uuid;

/// The answer a healthy listener that is not the daemon gives every request.
const OK: &str = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
const NOT_FOUND: &str = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
const BAD_REQUEST: &str =
    "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

/// How a [`FakeListener`] answers each connection.
#[derive(Clone)]
enum Behaviour {
    /// Proves itself with this proof key and accepts `POST /shutdown`.
    Proving([u8; 32]),
    /// Answers `200` to everything, first running the hook on each `/proof`
    /// request.
    Healthy(Option<Arc<dyn Fn() + Send + Sync>>),
    /// Reads the request head and never answers.
    Silent,
}

/// The request heads a [`FakeListener`] has read, each with the order its
/// connection was accepted in (0 first), and a signal for each new one.
#[derive(Default)]
struct Recorded {
    heads: Mutex<Vec<(usize, String)>>,
    changed: Condvar,
}

/// A loopback listener on a free port, serving each connection on its own
/// thread so a connection held open never blocks the next one.
pub(crate) struct FakeListener {
    pub(crate) port: u16,
    recorded: Arc<Recorded>,
}

impl FakeListener {
    /// One that proves it holds `token` and answers `POST /shutdown` with `200`.
    pub(crate) fn proving(token: &str) -> Self {
        Self::start(Behaviour::Proving(tracer_protocol::daemon_proof_key(token)))
    }

    /// One that answers `200 OK` to every request, `/proof` included.
    pub(crate) fn healthy() -> Self {
        Self::start(Behaviour::Healthy(None))
    }

    /// As [`Self::healthy`], also sending on the receiver for each `/proof`
    /// request it reads.
    pub(crate) fn healthy_signalling() -> (Self, mpsc::Receiver<()>) {
        let (tx, rx) = mpsc::channel();
        let signal = move || {
            let _ = tx.send(());
        };
        (Self::healthy_then(signal), rx)
    }

    /// As [`Self::healthy`], running `on_proof` on each `/proof` request
    /// before it answers.
    pub(crate) fn healthy_then(on_proof: impl Fn() + Send + Sync + 'static) -> Self {
        Self::start(Behaviour::Healthy(Some(Arc::new(on_proof))))
    }

    /// One that reads each request head and never answers.
    pub(crate) fn silent() -> Self {
        Self::start(Behaviour::Silent)
    }

    fn start(behaviour: Behaviour) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the fake listener");
        let port = listener.local_addr().expect("fake listener addr").port();
        let recorded = Arc::new(Recorded::default());
        let serving = Arc::clone(&recorded);
        thread::spawn(move || {
            for (accepted, stream) in listener.incoming().flatten().enumerate() {
                let behaviour = behaviour.clone();
                let recorded = Arc::clone(&serving);
                thread::spawn(move || serve(stream, accepted, &behaviour, &recorded));
            }
        });
        Self { port, recorded }
    }

    /// Every request head read so far.
    pub(crate) fn heads(&self) -> Vec<String> {
        self.heads_by_accept()
            .into_iter()
            .map(|(_, head)| head)
            .collect()
    }

    /// Every request head read so far, in the order they were read, each with
    /// the order its connection was accepted in (0 first). A connection that
    /// closed with nothing sent records an empty head.
    pub(crate) fn heads_by_accept(&self) -> Vec<(usize, String)> {
        self.recorded
            .heads
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// [`Self::heads_by_accept`] once at least `count` heads are read, or
    /// whatever was read when `hang_guard` passes.
    pub(crate) fn heads_when(&self, count: usize, hang_guard: Duration) -> Vec<(usize, String)> {
        let heads = self
            .recorded
            .heads
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let (heads, _) = self
            .recorded
            .changed
            .wait_timeout_while(heads, hang_guard, |heads| heads.len() < count)
            .unwrap_or_else(PoisonError::into_inner);
        heads.clone()
    }

    /// Whether any request carried an `Authorization` header.
    pub(crate) fn saw_authorization(&self) -> bool {
        self.heads()
            .iter()
            .any(|head| head.to_ascii_lowercase().contains("\r\nauthorization:"))
    }
}

/// Reads one request head, records it, then answers as `behaviour` says.
fn serve(mut stream: TcpStream, accepted: usize, behaviour: &Behaviour, recorded: &Recorded) {
    let head = read_head(&mut stream);
    recorded
        .heads
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push((accepted, head.clone()));
    recorded.changed.notify_all();
    let answer = match behaviour {
        Behaviour::Proving(proof_key) => proving_answer(&head, proof_key),
        Behaviour::Healthy(on_proof) => {
            if head.starts_with("GET /proof")
                && let Some(on_proof) = on_proof
            {
                on_proof();
            }
            OK.to_owned()
        }
        Behaviour::Silent => {
            // Hold the connection open without answering: the client must
            // give up on its own budget.
            let (_keep, held) = mpsc::channel::<()>();
            let _ = held.recv();
            return;
        }
    };
    let _ = stream.write_all(answer.as_bytes());
    let _ = stream.flush();
}

/// What a daemon holding `proof_key` answers `head` with.
fn proving_answer(head: &str, proof_key: &[u8; 32]) -> String {
    if head.starts_with("GET /proof ") {
        return challenge_of(head).map_or_else(
            || BAD_REQUEST.to_owned(),
            |challenge| {
                let proof = tracer_protocol::daemon_proof(proof_key, &challenge);
                format!(
                    "HTTP/1.1 204 No Content\r\nx-rt-proof: {}\r\n\r\n",
                    tracer_protocol::to_lower_hex(&proof)
                )
            },
        );
    }
    if head.starts_with("POST /shutdown ") {
        return OK.to_owned();
    }
    NOT_FOUND.to_owned()
}

/// The challenge a proof request head carries, when it is 64 lowercase hex.
fn challenge_of(head: &str) -> Option<[u8; 32]> {
    head.lines()
        .find_map(|line| line.strip_prefix("X-RT-Challenge: "))
        .and_then(tracer_protocol::parse_lower_hex32)
}

/// Reads a request line and its headers, up to the blank line or EOF.
fn read_head(stream: &mut TcpStream) -> String {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte) {
            Ok(1) => head.push(byte[0]),
            _ => break,
        }
    }
    String::from_utf8_lossy(&head).into_owned()
}

/// A directory under `<workspace>/.tmp` that the `*_in` functions are pointed
/// at explicitly; the environment is left alone. Drop removes it.
pub(crate) struct ScratchDir(pub(crate) PathBuf);

impl ScratchDir {
    pub(crate) fn new(label: &str) -> Self {
        let path = crate::supervisor::dev_workspace_root()
            .expect("workspace root resolves")
            .join(".tmp")
            .join(format!("daemon-client-{label}-{}", Uuid::new_v4().simple()));
        std::fs::create_dir_all(&path).expect("create scratch dir");
        Self(path)
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A handshake naming `pid`, `port` and `token`, speaking `versions`.
pub(crate) fn handshake_naming(
    pid: u32,
    port: u16,
    token: &str,
    versions: &[u32],
) -> DaemonHandshake {
    DaemonHandshake {
        protocol_version: versions.first().copied().unwrap_or(7),
        port,
        auth_token: token.to_owned(),
        pid,
        supported_versions: versions.to_vec(),
    }
}

/// Writes `handshake` as `daemon.json` under `dir`.
pub(crate) fn write_daemon_json(dir: &Path, handshake: &DaemonHandshake) {
    let json = serde_json::to_vec(handshake).expect("encode handshake");
    std::fs::write(crate::handshake_file_in(dir), json).expect("write daemon.json");
}

/// A harmless process that stays alive for half a minute unless killed.
pub(crate) fn stand_in() -> std::process::Child {
    let mut command = if cfg!(windows) {
        let mut ping = std::process::Command::new("ping");
        ping.args(["-n", "30", "127.0.0.1"]);
        ping
    } else {
        let mut sleep = std::process::Command::new("sleep");
        sleep.arg("30");
        sleep
    };
    command
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("spawn a stand-in daemon")
}

/// Runs `future` to completion on a fresh current-thread runtime.
pub(crate) fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build a test runtime")
        .block_on(future)
}
