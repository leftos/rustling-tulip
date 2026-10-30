//! The daemon's `bridge` subcommand: one client's protocol over stdio.
//!
//! `rustling-tulipd bridge` is what a client on another machine runs on this
//! host over SSH (`ssh -T -o BatchMode=yes -- <host> rustling-tulipd bridge`).
//! It starts the daemon when none runs, opens the loopback `/ws` and relays one
//! compact JSON message per line: each stdin line to the daemon, each daemon
//! text frame to stdout. It writes the daemon's token into the client's first
//! `Hello` itself, so the token never leaves the host, and nothing but protocol
//! lines ever reaches stdout.

use crate::cli_pair::not_proven_reason;
use anyhow::anyhow;
use daemon_client::ProvenConnectError;
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt as _, StreamExt as _};
use protocol::{DaemonHandshake, SUPPORTED_PROTOCOL_VERSIONS};
use std::ffi::OsString;
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// The bridge's one connection to the daemon's loopback listener.
type BridgeSocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// How many stdin chunks the reader thread may hold before the relay takes them.
const STDIN_CHUNKS: usize = 16;

/// How long the bridge keeps relaying the daemon's frames after stdin ended,
/// waiting for the daemon's Close.
const DRAIN_LIMIT: Duration = Duration::from_secs(5);

/// The waits one bridge run allows.
#[derive(Debug, Clone, Copy)]
struct Budgets {
    /// How long the daemon has to prove it holds the token.
    proof: Duration,
    /// How long replies may still arrive after stdin ended: [`DRAIN_LIMIT`].
    drain: Duration,
}

/// True when the daemon's arguments ask for `bridge`: the first argument is
/// exactly `bridge`.
pub fn requested(args: impl IntoIterator<Item = OsString>) -> bool {
    args.into_iter().next().is_some_and(|arg| arg == "bridge")
}

/// Start the daemon if none runs and relay this process's stdin and stdout to
/// it until either end closes.
pub async fn run() -> anyhow::Result<()> {
    stop_inheriting_stdio();
    run_with(
        async || daemon_client::ensure_running(SUPPORTED_PROTOCOL_VERSIONS).await,
        stdin_reader(),
        tokio::io::stdout(),
        Budgets {
            proof: daemon_client::PROOF_TIMEOUT,
            drain: DRAIN_LIMIT,
        },
    )
    .await
}

/// Keep this process's standard handles out of every child it starts.
///
/// sshd hands the bridge inheritable pipes, and a daemon `ensure_running`
/// starts would otherwise inherit them and hold sshd's output pipe open for
/// its whole life, so the ssh session would never end after the bridge exits.
#[cfg(windows)]
fn stop_inheriting_stdio() {
    use windows::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    let handles: Vec<_> = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE]
        .into_iter()
        // SAFETY: reads one of this process's own standard handles.
        .filter_map(|which| unsafe { GetStdHandle(which) }.ok())
        .collect();
    stop_inheriting(&handles);
}

/// Nothing to do: on other platforms the daemon is started without the
/// caller's standard handles holding the session open.
#[cfg(not(windows))]
fn stop_inheriting_stdio() {}

/// Clear the inherit flag on each of `handles`, skipping a null or invalid
/// one. A failure is reported on stderr and leaves the rest as they are.
#[cfg(windows)]
fn stop_inheriting(handles: &[windows::Win32::Foundation::HANDLE]) {
    use std::io::Write as _;
    use windows::Win32::Foundation::{HANDLE_FLAG_INHERIT, HANDLE_FLAGS, SetHandleInformation};
    for &handle in handles.iter().filter(|handle| !handle.is_invalid()) {
        // SAFETY: changes only the inherit flag of a handle this process holds.
        let cleared =
            unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0)) };
        if let Err(err) = cleared {
            let _ = writeln!(
                std::io::stderr(),
                "rustling-tulipd bridge: could not keep a standard handle from a started daemon: {err}"
            );
            return;
        }
    }
}

/// Standard input, read on a thread of its own.
///
/// `tokio::io::stdin` reads on a runtime blocking thread whose read cannot be
/// cancelled, so the runtime's shutdown would wait for the client's next line
/// after the daemon closed. A plain thread does not hold the process open.
fn stdin_reader() -> impl AsyncBufRead + Unpin {
    let (mut chunks_tx, chunks) =
        futures::channel::mpsc::channel::<std::io::Result<std::io::Cursor<Vec<u8>>>>(STDIN_CHUNKS);
    std::thread::spawn(move || {
        use std::io::Read as _;
        let mut stdin = std::io::stdin().lock();
        let mut buffer = vec![0u8; 8 * 1024];
        loop {
            let chunk = match stdin.read(&mut buffer) {
                Ok(0) => return,
                Ok(read) => Ok(std::io::Cursor::new(buffer[..read].to_vec())),
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(err) => Err(err),
            };
            let failed = chunk.is_err();
            // A closed channel means the relay has ended: nothing reads on.
            if futures::executor::block_on(chunks_tx.send(chunk)).is_err() || failed {
                return;
            }
        }
    });
    tokio_util::io::StreamReader::new(chunks)
}

/// [`run`] over the given stdio, starting or finding the daemon with `ensure`
/// and waiting within `budgets`.
///
/// When stdin ends first, the daemon's frames are still relayed until it
/// closes, for at most `budgets.drain`, and only then does the bridge send its
/// own Close. The Close waits because the daemon drops every reply still
/// queued for a connection once it reads that connection's Close, so a piped
/// one-shot (`printf '<hello>\n' | ssh <host> rustling-tulipd bridge`) would
/// otherwise lose its `Welcome`.
async fn run_with(
    ensure: impl AsyncFnOnce() -> anyhow::Result<DaemonHandshake>,
    stdin: impl AsyncBufRead + Unpin,
    stdout: impl AsyncWrite + Unpin,
    budgets: Budgets,
) -> anyhow::Result<()> {
    let handshake = ensure().await?;
    let socket = connect(&handshake, budgets.proof).await?;
    let (sink, stream) = socket.split();
    let downstream = downstream(stream, stdout);
    tokio::pin!(downstream);
    tokio::select! {
        ended = upstream(stdin, sink, &handshake.auth_token) => {
            let mut sink = ended?;
            let drained = tokio::time::timeout(budgets.drain, &mut downstream).await;
            // The client is gone either way: a daemon that already closed
            // needs no Close, so a failed one changes nothing.
            let _ = sink.send(Message::Close(None)).await;
            // How the daemon's side ended no longer matters either: a reset,
            // or a daemon still open when the drain is over, ends the run
            // cleanly. Only a stdout error still fails it.
            match drained {
                Ok(drained) => drained.map(|_| ()),
                Err(_) => Ok(()),
            }
        }
        ended = &mut downstream => ended?.while_open(),
    }
}

/// Open the loopback `/ws` named by `handshake`, only once the listener proves
/// it holds the token within `proof_budget`.
async fn connect(
    handshake: &DaemonHandshake,
    proof_budget: Duration,
) -> anyhow::Result<BridgeSocket> {
    let port = handshake.port;
    let stream =
        match daemon_client::connect_proven(port, &handshake.auth_token, proof_budget).await {
            Ok(stream) => stream,
            Err(ProvenConnectError::NotProven(proof)) => {
                let path = daemon_client::handshake_file()?;
                return Err(anyhow!("{}", not_proven_reason(port, &path, proof)));
            }
            Err(ProvenConnectError::Connect(err)) => {
                return Err(anyhow!(
                    "could not reach rustling-tulipd on 127.0.0.1:{port}: {err:#}"
                ));
            }
        };
    let (socket, _) = tokio_tungstenite::client_async(
        format!("ws://127.0.0.1:{port}/ws"),
        MaybeTlsStream::Plain(stream),
    )
    .await
    .map_err(|err| anyhow!("could not reach rustling-tulipd on 127.0.0.1:{port}: {err}"))?;
    Ok(socket)
}

/// Send each non-empty stdin line to the daemon as one text frame, the first
/// one carrying the host's token when it is a `Hello`. At stdin's end, hand
/// the sink back unclosed.
async fn upstream(
    stdin: impl AsyncBufRead + Unpin,
    mut sink: SplitSink<BridgeSocket, Message>,
    token: &str,
) -> anyhow::Result<SplitSink<BridgeSocket, Message>> {
    let mut lines = stdin.lines();
    let mut first = true;
    while let Some(line) = lines
        .next_line()
        .await
        .map_err(|err| anyhow!("could not read stdin: {err}"))?
    {
        if line.is_empty() {
            continue;
        }
        let text = if first {
            with_host_token(line, token)
        } else {
            line
        };
        first = false;
        sink.send(Message::text(text))
            .await
            .map_err(|err| anyhow!("could not send to rustling-tulipd: {err}"))?;
    }
    Ok(sink)
}

/// `line` with its `auth_token` set to `token` when it is a `Hello` object;
/// any other line, unchanged. Every other field survives as sent.
fn with_host_token(line: String, token: &str) -> String {
    let Ok(serde_json::Value::Object(mut fields)) = serde_json::from_str(&line) else {
        return line;
    };
    if fields.get("type").and_then(serde_json::Value::as_str) != Some("hello") {
        return line;
    }
    fields.insert(
        "auth_token".to_owned(),
        serde_json::Value::String(token.to_owned()),
    );
    serde_json::Value::Object(fields).to_string()
}

/// How the daemon's side of the connection ended.
enum Ended {
    /// The daemon closed it, or the stream ended.
    Closed,
    /// The connection failed before the daemon closed it.
    Lost(tungstenite::Error),
}

impl Ended {
    /// The run's result when the daemon's side ended while the client was
    /// still there: a lost connection fails it.
    fn while_open(self) -> anyhow::Result<()> {
        match self {
            Self::Closed => Ok(()),
            Self::Lost(err) => Err(anyhow!("lost the connection to rustling-tulipd: {err}")),
        }
    }
}

/// Write each daemon text frame to `stdout` as one line until the daemon
/// closes the connection or it fails. Only a stdout error is an `Err`.
async fn downstream(
    mut stream: SplitStream<BridgeSocket>,
    mut stdout: impl AsyncWrite + Unpin,
) -> anyhow::Result<Ended> {
    let mut ended = Ended::Closed;
    while let Some(frame) = stream.next().await {
        match frame {
            Ok(Message::Text(text)) => write_line(&mut stdout, text.as_str()).await?,
            Ok(Message::Close(_)) | Err(tungstenite::Error::ConnectionClosed) => break,
            Ok(_) => {}
            Err(err) => {
                ended = Ended::Lost(err);
                break;
            }
        }
    }
    stdout.flush().await.map_err(|err| stdout_error(&err))?;
    Ok(ended)
}

/// Write `text` and a newline to `stdout` and flush it.
async fn write_line(stdout: &mut (impl AsyncWrite + Unpin), text: &str) -> anyhow::Result<()> {
    let written = async {
        stdout.write_all(text.as_bytes()).await?;
        stdout.write_all(b"\n").await?;
        stdout.flush().await
    };
    written.await.map_err(|err| stdout_error(&err))
}

fn stdout_error(err: &std::io::Error) -> anyhow::Error {
    anyhow!("could not write to stdout: {err}")
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use crate::server::test_support::{serve_loopback, test_hub, unproven_listener};
    use protocol::{ClientMessage, DaemonMessage, InboundDaemonMessage};
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll};
    use tokio::io::{AsyncReadExt as _, BufReader, DuplexStream};
    use tokio::task::JoinHandle;

    /// The proof budget and the bound on every wait: a hang guard only.
    const HANG_GUARD: Duration = Duration::from_secs(120);

    /// Every wait a test bridge allows: hang guards only, so no test depends
    /// on the production proof timeout or drain limit.
    const TEST_BUDGETS: Budgets = Budgets {
        proof: HANG_GUARD,
        drain: HANG_GUARD,
    };

    /// [`TEST_BUDGETS`] with no drain, for a test whose replies have all
    /// arrived before stdin ends: the bridge closes as soon as it ends.
    const NO_DRAIN: Budgets = Budgets {
        proof: HANG_GUARD,
        drain: Duration::ZERO,
    };

    /// The token every test hub holds.
    const TOKEN: &str = "test-token";

    /// What the duplex pipes standing for stdin and stdout may hold.
    const PIPE: usize = 1024 * 1024;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    fn handshake_for(port: u16) -> DaemonHandshake {
        DaemonHandshake {
            protocol_version: SUPPORTED_PROTOCOL_VERSIONS
                .iter()
                .copied()
                .max()
                .unwrap_or_default(),
            port,
            auth_token: TOKEN.to_owned(),
            pid: 1,
            supported_versions: SUPPORTED_PROTOCOL_VERSIONS.to_vec(),
        }
    }

    /// A client's `Hello` line with an empty token.
    fn hello_line() -> String {
        let hello = ClientMessage::Hello {
            protocol_version: SUPPORTED_PROTOCOL_VERSIONS
                .iter()
                .copied()
                .max()
                .unwrap_or_default(),
            protocol_versions: SUPPORTED_PROTOCOL_VERSIONS.to_vec(),
            auth_token: String::new(),
            client_id: None,
            client_name: Some("bridge test".to_owned()),
        };
        let json = serde_json::to_string(&hello).expect("serialize the hello");
        assert!(json.contains(r#""auth_token":"""#), "{json}");
        format!("{json}\n")
    }

    /// A bridge running against the daemon on `port`: the client's ends of
    /// its stdin and stdout, and the bridge's task.
    struct Bridge {
        stdin: DuplexStream,
        stdout: BufReader<DuplexStream>,
        task: JoinHandle<anyhow::Result<()>>,
    }

    impl Bridge {
        fn start(port: u16, budgets: Budgets) -> Self {
            let (stdin, bridge_stdin) = tokio::io::duplex(PIPE);
            let (bridge_stdout, stdout) = tokio::io::duplex(PIPE);
            let task = tokio::spawn(run_with(
                async move || Ok(handshake_for(port)),
                BufReader::new(bridge_stdin),
                bridge_stdout,
                budgets,
            ));
            Self {
                stdin,
                stdout: BufReader::new(stdout),
                task,
            }
        }

        async fn send(&mut self, line: &str) {
            self.stdin
                .write_all(line.as_bytes())
                .await
                .expect("write to the bridge's stdin");
        }

        /// The next stdout line with its newline, or empty at stdout's end.
        async fn next_raw(&mut self) -> Vec<u8> {
            let mut line = Vec::new();
            tokio::time::timeout(HANG_GUARD, self.stdout.read_until(b'\n', &mut line))
                .await
                .expect("hung waiting for a stdout line")
                .expect("read the bridge's stdout");
            line
        }

        async fn next_message(&mut self) -> DaemonMessage {
            let line = self.next_raw().await;
            assert!(
                line.ends_with(b"\n"),
                "{:?}",
                String::from_utf8_lossy(&line)
            );
            known(&line)
        }
    }

    fn known(line: &[u8]) -> DaemonMessage {
        let text = std::str::from_utf8(line).expect("a stdout line is UTF-8");
        let message = match InboundDaemonMessage::from_json_str(text.trim_end_matches('\n')) {
            Ok(InboundDaemonMessage::Known(message)) => Some(*message),
            _ => None,
        };
        assert!(message.is_some(), "not a known daemon message: {text:?}");
        message.expect("a known daemon message")
    }

    /// Read stdout until the `EnvNames` reply to `request_id`, returning every
    /// line read on the way, newlines included.
    async fn read_until_env_names(bridge: &mut Bridge, request_id: &str) -> Vec<Vec<u8>> {
        let mut lines = Vec::new();
        loop {
            let line = bridge.next_raw().await;
            assert!(!line.is_empty(), "stdout ended before the EnvNames reply");
            let message = known(&line);
            lines.push(line);
            if matches!(message, DaemonMessage::EnvNames { request_id: Some(id), .. } if id == request_id)
            {
                return lines;
            }
        }
    }

    /// A whole session through the bridge: hello, one request and its reply,
    /// then stdin's end. Every stdout byte, once the bridge ended.
    async fn session_bytes(tag: &str) -> Vec<u8> {
        let (hub, _scratch) = test_hub(tag);
        let port = serve_loopback(&hub).await;
        let mut bridge = Bridge::start(port, NO_DRAIN);
        bridge.send(&hello_line()).await;
        bridge
            .send("{\"type\":\"list_env_names\",\"request_id\":\"bridge-1\"}\r\n")
            .await;
        let mut bytes = read_until_env_names(&mut bridge, "bridge-1").await.concat();
        let Bridge {
            stdin,
            mut stdout,
            task,
        } = bridge;
        drop(stdin);
        tokio::time::timeout(HANG_GUARD, stdout.read_to_end(&mut bytes))
            .await
            .expect("hung waiting for stdout's end")
            .expect("read the rest of stdout");
        tokio::time::timeout(HANG_GUARD, task)
            .await
            .expect("hung: the bridge did not end")
            .expect("the bridge task ran to its end")
            .expect("the bridge ends cleanly at stdin's end");
        bytes
    }

    #[test]
    fn bridge_is_requested_only_as_the_first_argument() {
        assert!(requested(args(&["bridge"])));
        let empty: &[&str] = &[];
        for list in [
            empty,
            &["--detach"],
            &["pair", "bridge"],
            &["bridges"],
            &["Bridge"],
        ] {
            assert!(!requested(args(list)), "{list:?}");
        }
    }

    #[tokio::test]
    async fn bridge_puts_the_host_token_into_the_hello() {
        let (hub, _scratch) = test_hub("cli-bridge-hello");
        let port = serve_loopback(&hub).await;
        let mut bridge = Bridge::start(port, TEST_BUDGETS);

        bridge.send(&hello_line()).await;

        let first = bridge.next_message().await;
        assert!(matches!(first, DaemonMessage::Welcome { .. }), "{first:?}");
    }

    #[tokio::test]
    async fn bridge_relays_lines_both_ways() {
        let (hub, _scratch) = test_hub("cli-bridge-relay");
        let port = serve_loopback(&hub).await;
        let mut bridge = Bridge::start(port, TEST_BUDGETS);
        bridge.send(&hello_line()).await;
        let first = bridge.next_message().await;
        assert!(matches!(first, DaemonMessage::Welcome { .. }), "{first:?}");

        bridge
            .send("\n{\"type\":\"list_env_names\",\"request_id\":\"bridge-2\"}\n")
            .await;

        read_until_env_names(&mut bridge, "bridge-2").await;
    }

    #[tokio::test]
    async fn bridge_writes_only_protocol_lines() {
        let bytes = session_bytes("cli-bridge-lines").await;

        assert!(bytes.ends_with(b"\n"), "stdout ends mid-line");
        for line in bytes.split_inclusive(|byte| *byte == b'\n') {
            assert!(!line[..line.len() - 1].contains(&b'\n'));
            assert!(
                !line.contains(&b'\r'),
                "{:?}",
                String::from_utf8_lossy(line)
            );
            known(line);
        }
    }

    #[tokio::test]
    async fn bridge_never_writes_the_token() {
        let bytes = session_bytes("cli-bridge-token").await;

        assert!(
            !bytes
                .windows(TOKEN.len())
                .any(|window| window == TOKEN.as_bytes()),
            "the token reached stdout"
        );
    }

    #[tokio::test]
    async fn bridge_exits_on_stdin_eof() {
        let (hub, _scratch) = test_hub("cli-bridge-eof");
        let port = serve_loopback(&hub).await;
        let mut bridge = Bridge::start(port, NO_DRAIN);
        bridge.send(&hello_line()).await;
        let first = bridge.next_message().await;
        assert!(matches!(first, DaemonMessage::Welcome { .. }), "{first:?}");

        let Bridge {
            stdin,
            stdout: _stdout,
            task,
        } = bridge;
        drop(stdin);

        tokio::time::timeout(HANG_GUARD, task)
            .await
            .expect("hung: the bridge did not end at stdin's end")
            .expect("the bridge task ran to its end")
            .expect("stdin's end ends the bridge cleanly");
    }

    #[tokio::test]
    async fn bridge_relays_replies_still_in_flight_at_stdin_eof() {
        let (hub, _scratch) = test_hub("cli-bridge-drain");
        let port = serve_loopback(&hub).await;
        let mut bridge = Bridge::start(port, TEST_BUDGETS);

        // The hello and stdin's end at once, as a piped one-shot sends them.
        bridge.send(&hello_line()).await;
        let Bridge {
            stdin,
            mut stdout,
            task,
        } = bridge;
        drop(stdin);

        let mut first = Vec::new();
        tokio::time::timeout(HANG_GUARD, stdout.read_until(b'\n', &mut first))
            .await
            .expect("hung waiting for the first stdout line")
            .expect("read the bridge's stdout");
        assert!(!task.is_finished(), "the bridge ended before its drain did");
        let first = known(&first);
        assert!(matches!(first, DaemonMessage::Welcome { .. }), "{first:?}");

        // The daemon closing ends the drain, and with it the run.
        hub.shutdown_tx
            .send(true)
            .expect("the hub's shutdown has a receiver");
        tokio::time::timeout(HANG_GUARD, task)
            .await
            .expect("hung: the bridge did not end on the daemon's close")
            .expect("the bridge task ran to its end")
            .expect("stdin's end ends the bridge cleanly");
    }

    #[cfg(windows)]
    #[test]
    fn stdio_handles_stop_being_inherited() {
        use windows::Win32::Foundation::{
            CloseHandle, GetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT,
        };
        use windows::Win32::Security::SECURITY_ATTRIBUTES;
        use windows::Win32::System::Pipes::CreatePipe;

        let attributes = SECURITY_ATTRIBUTES {
            nLength: u32::try_from(std::mem::size_of::<SECURITY_ATTRIBUTES>())
                .expect("the attributes' size fits a u32"),
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: true.into(),
        };
        let (mut read, mut write) = (HANDLE::default(), HANDLE::default());
        // SAFETY: creates an anonymous pipe into two handles this test owns.
        unsafe {
            CreatePipe(
                &raw mut read,
                &raw mut write,
                Some(&raw const attributes),
                0,
            )
        }
        .expect("create an inheritable pipe");
        let inherited = |handle: HANDLE| {
            let mut flags = 0u32;
            // SAFETY: reads the flags of a handle this test owns.
            unsafe { GetHandleInformation(handle, &raw mut flags) }
                .expect("read the handle's flags");
            flags & HANDLE_FLAG_INHERIT.0 != 0
        };
        assert!(
            inherited(read) && inherited(write),
            "the pipe starts inheritable"
        );

        stop_inheriting(&[read, HANDLE::default(), write]);

        assert!(!inherited(read), "the read end is still inherited");
        assert!(!inherited(write), "the write end is still inherited");
        for handle in [read, write] {
            // SAFETY: closes a handle this test owns, once.
            unsafe { CloseHandle(handle) }.expect("close the pipe end");
        }
    }

    #[tokio::test]
    async fn bridge_exits_when_the_daemon_closes() {
        let (hub, _scratch) = test_hub("cli-bridge-closed");
        let port = serve_loopback(&hub).await;
        let mut bridge = Bridge::start(port, TEST_BUDGETS);

        // Not a Hello: the daemon refuses the handshake and closes.
        bridge.send("{\"type\":\"list_sessions\"}\n").await;

        let first = bridge.next_message().await;
        assert!(
            matches!(first, DaemonMessage::AuthFailed { .. }),
            "{first:?}"
        );
        assert_eq!(bridge.next_raw().await, b"", "stdout ends after the close");
        // The client's stdin stays open: the daemon's close alone ends the run.
        let stdin_held = bridge.stdin;
        tokio::time::timeout(HANG_GUARD, bridge.task)
            .await
            .expect("hung: the bridge did not end on the daemon's close")
            .expect("the bridge task ran to its end")
            .expect("the daemon's close ends the bridge cleanly");
        drop(stdin_held);
    }

    /// A stdout whose every write fails.
    struct BrokenStdout;

    impl AsyncWrite for BrokenStdout {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Err(std::io::Error::other("the pipe is gone")))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Err(std::io::Error::other("the pipe is gone")))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn bridge_fails_on_a_stdout_write_error() {
        let (hub, _scratch) = test_hub("cli-bridge-broken-stdout");
        let port = serve_loopback(&hub).await;
        let (mut stdin, bridge_stdin) = tokio::io::duplex(PIPE);
        stdin
            .write_all(hello_line().as_bytes())
            .await
            .expect("write the hello");

        let result = tokio::time::timeout(
            HANG_GUARD,
            run_with(
                async move || Ok(handshake_for(port)),
                BufReader::new(bridge_stdin),
                BrokenStdout,
                TEST_BUDGETS,
            ),
        )
        .await
        .expect("hung: the bridge did not end on a stdout error");

        let err = result.expect_err("a stdout write error fails the bridge");
        assert_eq!(
            err.to_string(),
            "could not write to stdout: the pipe is gone"
        );
        drop(stdin);
    }

    #[tokio::test]
    async fn bridge_connects_to_the_daemon_ensure_returns() {
        let (hub, _scratch) = test_hub("cli-bridge-ensure");
        let port = serve_loopback(&hub).await;
        let ensured = Arc::new(AtomicUsize::new(0));
        let calls = Arc::clone(&ensured);
        let (mut stdin, bridge_stdin) = tokio::io::duplex(PIPE);
        let (bridge_stdout, stdout) = tokio::io::duplex(PIPE);
        let task = tokio::spawn(run_with(
            async move || {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(handshake_for(port))
            },
            BufReader::new(bridge_stdin),
            bridge_stdout,
            NO_DRAIN,
        ));
        let mut bridge = Bridge {
            stdin: {
                stdin
                    .write_all(hello_line().as_bytes())
                    .await
                    .expect("write the hello");
                stdin
            },
            stdout: BufReader::new(stdout),
            task,
        };

        let first = bridge.next_message().await;

        assert!(matches!(first, DaemonMessage::Welcome { .. }), "{first:?}");
        assert_eq!(ensured.load(Ordering::SeqCst), 1);
        drop(bridge.stdin);
        bridge
            .task
            .await
            .expect("the bridge task ran to its end")
            .expect("stdin's end ends the bridge cleanly");
    }

    #[tokio::test]
    async fn bridge_reports_a_failed_start() {
        let mut stdout = Vec::new();

        let result = tokio::time::timeout(
            HANG_GUARD,
            run_with(
                async || Err(anyhow!("could not start rustling-tulipd: no daemon binary")),
                hello_line().as_bytes(),
                &mut stdout,
                TEST_BUDGETS,
            ),
        )
        .await
        .expect("hung: a failed start did not end the bridge");

        let err = result.expect_err("a failed start fails the bridge");
        assert_eq!(
            err.to_string(),
            "could not start rustling-tulipd: no daemon binary"
        );
        assert!(stdout.is_empty(), "{:?}", String::from_utf8_lossy(&stdout));
    }

    #[tokio::test]
    async fn bridge_sends_nothing_to_a_listener_without_the_proof() {
        let (port, received) = unproven_listener();
        let mut stdout = Vec::new();

        let result = tokio::time::timeout(
            HANG_GUARD,
            run_with(
                async move || Ok(handshake_for(port)),
                hello_line().as_bytes(),
                &mut stdout,
                TEST_BUDGETS,
            ),
        )
        .await
        .expect("hung: an unproven listener did not end the bridge");

        let text = result
            .expect_err("an unproven listener gets no relay")
            .to_string();
        assert!(
            text.starts_with(&format!("the listener on 127.0.0.1:{port} named by ")),
            "{text}"
        );
        assert!(
            text.ends_with("did not prove it is rustling-tulipd; nothing was sent"),
            "{text}"
        );
        assert!(stdout.is_empty(), "{:?}", String::from_utf8_lossy(&stdout));
        let first = received
            .recv_timeout(HANG_GUARD)
            .expect("the connection opened before the proof was closed");
        for bytes in std::iter::once(first).chain(received.try_iter()) {
            assert!(
                bytes.is_empty(),
                "the unproven listener was sent {:?}",
                String::from_utf8_lossy(&bytes)
            );
        }
    }
}
