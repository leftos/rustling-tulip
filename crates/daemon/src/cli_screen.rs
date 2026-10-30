//! The daemon's `screen` subcommand: print one session's rendered screen and
//! how its screen rules read it.
//!
//! `rustling-tulipd screen <session id> [--fixture <path>]` asks the running
//! daemon for the session's grid and the rule trace over it, prints the trace
//! (one line per rule), the winning rule and the grid, and with `--fixture`
//! also writes the grid's rows to a new file at that path, ready to become a
//! screen-rule test fixture.

use anyhow::anyhow;
use daemon_client::ProvenConnectError;
use futures::{SinkExt as _, StreamExt as _};
use protocol::{
    ClientMessage, DaemonHandshake, DaemonMessage, InboundDaemonMessage,
    SUPPORTED_PROTOCOL_VERSIONS, ScreenExplanation,
};
use std::ffi::OsString;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// The whole exchange — reading the handshake, connecting, dumping the
/// screen — must finish inside this, or the subcommand gives up.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(10);

/// The client name the daemon logs this connection under.
const CLIENT_NAME: &str = "rustling-tulipd screen";

/// The `Welcome.features` entry a daemon lists once it can answer a dump.
const FEATURE_SCREEN_DUMP: &str = "screen_dump";

/// The error for a daemon too old to know `DumpScreen`.
const OLDER_DAEMON: &str = "the running daemon is older and cannot dump a screen; restart it";

/// The request id the dump is asked with, so its reply is told apart.
const REQUEST_ID: &str = "screen";

/// How the subcommand is called.
const USAGE: &str = "usage: rustling-tulipd screen <session id> [--fixture <path>]";

/// One client connection to the daemon's loopback listener.
type ScreenSocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// What the subcommand was asked to do.
#[derive(Debug, PartialEq, Eq)]
struct Options {
    session_id: String,
    fixture: Option<PathBuf>,
}

/// The parts of a [`DaemonMessage::ScreenDump`] the subcommand prints.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ScreenDump {
    cols: u16,
    rows: u16,
    grid: Vec<String>,
    title: Option<String>,
    explanation: ScreenExplanation,
}

/// True when the daemon's arguments ask for `screen`: the first argument is
/// exactly `screen`.
pub fn requested(args: impl IntoIterator<Item = OsString>) -> bool {
    args.into_iter().next().is_some_and(|arg| arg == "screen")
}

/// [`run`] on this process's stdout, its failure as one stderr line
/// `rustling-tulipd screen: <error>`: the exit code is 0 on success, 1 on any
/// failure.
pub async fn run_and_report(
    handshake_file: &Path,
    args: impl IntoIterator<Item = OsString>,
) -> std::process::ExitCode {
    match run(handshake_file, args, &mut std::io::stdout().lock()).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            let _ = writeln!(std::io::stderr(), "rustling-tulipd screen: {err:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Run the subcommand with `args`, the arguments after `screen`: dump the
/// session's screen from the daemon named by `handshake_file` and print it to
/// `out`.
async fn run(
    handshake_file: &Path,
    args: impl IntoIterator<Item = OsString>,
    out: &mut impl std::io::Write,
) -> anyhow::Result<()> {
    let options = parse(args)?;
    if let Some(path) = &options.fixture
        && path.exists()
    {
        return Err(anyhow!("{} already exists", path.display()));
    }
    let timeout_secs = EXCHANGE_TIMEOUT.as_secs();
    let dump = tokio::time::timeout(
        EXCHANGE_TIMEOUT,
        exchange(handshake_file, &options.session_id),
    )
    .await
    .map_err(|_| anyhow!("rustling-tulipd sent no screen dump within {timeout_secs} s"))??;
    out.write_all(render_dump(&dump).as_bytes())?;
    if let Some(path) = &options.fixture {
        write_fixture(path, &dump.grid)?;
        writeln!(out, "wrote {}", path.display())?;
    }
    Ok(())
}

/// The options `args` (the arguments after `screen`) ask for.
fn parse(args: impl IntoIterator<Item = OsString>) -> anyhow::Result<Options> {
    let mut session_id: Option<String> = None;
    let mut fixture: Option<PathBuf> = None;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--fixture" {
            let path = args
                .next()
                .ok_or_else(|| anyhow!("--fixture needs a path; {USAGE}"))?;
            fixture = Some(PathBuf::from(path));
        } else if arg.to_string_lossy().starts_with("--") || session_id.is_some() {
            return Err(anyhow!("unexpected argument {}; {USAGE}", arg.display()));
        } else {
            session_id = Some(arg.to_string_lossy().into_owned());
        }
    }
    let session_id = session_id.ok_or_else(|| anyhow!("no session id; {USAGE}"))?;
    Ok(Options {
        session_id,
        fixture,
    })
}

/// The trace, the winner and the grid, as the subcommand prints them.
fn render_dump(dump: &ScreenDump) -> String {
    let explanation = &dump.explanation;
    let mut lines: Vec<String> = Vec::new();
    for rule in &explanation.rules {
        let outcome = match (rule.matched, rule.vetoed_by) {
            (true, _) => "matched".to_owned(),
            (false, Some(guard)) => format!("vetoed by guard {guard}"),
            (false, None) => "no match".to_owned(),
        };
        lines.push(format!(
            "{:>5}  {}  {}  {outcome}  {}",
            rule.priority, rule.name, rule.region, rule.preview
        ));
    }
    let winner = explanation.winner.as_deref().unwrap_or("none");
    lines.push(format!("winner: {winner} ({})", explanation.verdict));
    for rejected in &explanation.rejected {
        lines.push(format!("rejected: {rejected}"));
    }
    if let Some(title) = &dump.title {
        lines.push(format!("title: {title}"));
    }
    lines.push(format!("grid {}x{}:", dump.cols, dump.rows));
    lines.extend(dump.grid.iter().cloned());
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

/// Write `grid` to `path`, one row per LF-ended line with trailing spaces
/// trimmed, refusing a path that already exists.
fn write_fixture(path: &Path, grid: &[String]) -> anyhow::Result<()> {
    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(anyhow!("{} already exists", path.display()));
        }
        Err(err) => return Err(anyhow!("could not create {}: {err}", path.display())),
    };
    let mut text = String::new();
    for row in grid {
        text.push_str(row.trim_end());
        text.push('\n');
    }
    file.write_all(text.as_bytes())
        .map_err(|err| anyhow!("could not write {}: {err}", path.display()))
}

/// Read the handshake file and dump `session_id`'s screen over one loopback
/// connection, opened only once the listener proves it holds the token.
async fn exchange(handshake_file: &Path, session_id: &str) -> anyhow::Result<ScreenDump> {
    let handshake = read_handshake(handshake_file)?;
    let port = handshake.port;
    let stream = match daemon_client::connect_proven(
        port,
        &handshake.auth_token,
        daemon_client::PROOF_TIMEOUT,
    )
    .await
    {
        Ok(stream) => stream,
        Err(ProvenConnectError::NotProven(proof)) => {
            return Err(anyhow!(
                "{}",
                crate::cli_pair::not_proven_reason(port, handshake_file, proof)
            ));
        }
        Err(ProvenConnectError::Connect(err)) => {
            return Err(anyhow!(
                "could not reach rustling-tulipd on 127.0.0.1:{port}: {err:#}"
            ));
        }
    };
    let (mut socket, _) = tokio_tungstenite::client_async(
        format!("ws://127.0.0.1:{port}/ws"),
        MaybeTlsStream::Plain(stream),
    )
    .await
    .map_err(|err| anyhow!("could not reach rustling-tulipd on 127.0.0.1:{port}: {err}"))?;
    send(&mut socket, &hello(&handshake)).await?;
    let dump = read_until_dump(&mut socket, session_id).await?;
    let _ = socket.close(None).await;
    Ok(dump)
}

/// The `daemon.json` the running daemon wrote, or the error that says why
/// there is no daemon to reach.
fn read_handshake(path: &Path) -> anyhow::Result<DaemonHandshake> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(anyhow!(
                "no rustling-tulipd is running: {} does not exist",
                path.display()
            ));
        }
        Err(err) => return Err(anyhow!("could not read {}: {err}", path.display())),
    };
    serde_json::from_slice(&bytes)
        .map_err(|err| anyhow!("could not read {}: {err}", path.display()))
}

/// The `Hello` that opens the connection: the daemon's own token, every
/// protocol version this build speaks, and no client identity.
fn hello(handshake: &DaemonHandshake) -> ClientMessage {
    ClientMessage::Hello {
        protocol_version: SUPPORTED_PROTOCOL_VERSIONS
            .iter()
            .copied()
            .max()
            .unwrap_or_default(),
        protocol_versions: SUPPORTED_PROTOCOL_VERSIONS.to_vec(),
        auth_token: handshake.auth_token.clone(),
        client_id: None,
        client_name: Some(CLIENT_NAME.to_owned()),
    }
}

/// What one daemon frame asks the subcommand to do.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    /// Nothing this subcommand acts on: keep reading.
    Skip,
    /// The daemon welcomed the connection; `screen_dump` says whether it lists
    /// [`FEATURE_SCREEN_DUMP`], and so can answer a dump at all.
    Welcome { screen_dump: bool },
    /// The dump arrived.
    Dump(ScreenDump),
    /// The daemon refused; the string is the whole error message.
    Refused(String),
}

/// Read one frame's text as the daemon's answer. Anything before the ask is
/// the initial state the daemon pushes, so only an answer after it counts.
fn classify(text: &str, asked: bool) -> Step {
    let Ok(InboundDaemonMessage::Known(message)) = InboundDaemonMessage::from_json_str(text) else {
        return Step::Skip;
    };
    match *message {
        DaemonMessage::Welcome { features, .. } if !asked => Step::Welcome {
            screen_dump: supports_screen_dump(&features),
        },
        DaemonMessage::ScreenDump {
            request_id,
            cols,
            rows,
            grid,
            title,
            explanation,
            ..
        } if asked && request_id.as_deref() == Some(REQUEST_ID) => Step::Dump(ScreenDump {
            cols,
            rows,
            grid,
            title,
            explanation,
        }),
        DaemonMessage::AuthFailed { reason } => {
            Step::Refused(format!("rustling-tulipd refused the handshake: {reason}"))
        }
        DaemonMessage::Error {
            message,
            request_id,
            ..
        } if asked && request_id.as_deref().is_none_or(|id| id == REQUEST_ID) => {
            Step::Refused(message)
        }
        _ => Step::Skip,
    }
}

/// Whether the daemon's `Welcome.features` show it can answer a dump.
fn supports_screen_dump(features: &[String]) -> bool {
    features
        .iter()
        .any(|feature| feature == FEATURE_SCREEN_DUMP)
}

/// Refuse a daemon that lists no [`FEATURE_SCREEN_DUMP`]: it decodes no
/// `DumpScreen`, so waiting for a reply could only run out the exchange.
fn check_welcome(screen_dump: bool) -> anyhow::Result<()> {
    if screen_dump {
        Ok(())
    } else {
        Err(anyhow!("{OLDER_DAEMON}"))
    }
}

/// Ask for the dump once the daemon welcomes the connection, and read on past
/// the initial state it pushes until the dump, a refusal or a close.
async fn read_until_dump(
    socket: &mut ScreenSocket,
    session_id: &str,
) -> anyhow::Result<ScreenDump> {
    let mut asked = false;
    loop {
        let Some(text) = next_text(socket).await else {
            return Err(anyhow!(
                "rustling-tulipd closed the connection before sending the screen"
            ));
        };
        match classify(&text, asked) {
            Step::Skip => {}
            Step::Welcome { screen_dump } => {
                check_welcome(screen_dump)?;
                asked = true;
                let ask = ClientMessage::DumpScreen {
                    session_id: session_id.to_owned(),
                    request_id: Some(REQUEST_ID.to_owned()),
                };
                send(socket, &ask).await?;
            }
            Step::Dump(dump) => return Ok(dump),
            Step::Refused(reason) => return Err(anyhow!("{reason}")),
        }
    }
}

/// The next text frame, or `None` once the daemon closed the connection.
async fn next_text(socket: &mut ScreenSocket) -> Option<String> {
    loop {
        match socket.next().await {
            None | Some(Err(_) | Ok(Message::Close(_))) => return None,
            Some(Ok(Message::Text(text))) => return Some(text.to_string()),
            Some(Ok(_)) => {}
        }
    }
}

async fn send(socket: &mut ScreenSocket, message: &ClientMessage) -> anyhow::Result<()> {
    let json = serde_json::to_string(message)
        .map_err(|err| anyhow!("could not serialize a client message: {err}"))?;
    socket
        .send(Message::text(json))
        .await
        .map_err(|err| anyhow!("could not send to rustling-tulipd: {err}"))
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use crate::server::test_support::ScratchDir;
    use protocol::ScreenRuleTrace;

    fn trace(name: &str, priority: u16, matched: bool, vetoed_by: Option<u32>) -> ScreenRuleTrace {
        ScreenRuleTrace {
            name: name.to_owned(),
            priority,
            region: "bottom_non_empty(4)".to_owned(),
            verdict: "working".to_owned(),
            matched,
            vetoed_by,
            preview: "BUSY | done".to_owned(),
        }
    }

    #[test]
    fn render_dump_prints_the_trace_the_winner_and_the_grid() {
        let dump = ScreenDump {
            cols: 20,
            rows: 2,
            grid: vec!["BUSY".to_owned(), "done".to_owned()],
            title: Some("my title".to_owned()),
            explanation: ScreenExplanation {
                verdict: "working".to_owned(),
                winner: Some("busy".to_owned()),
                from_title: false,
                rules: vec![
                    trace("guarded", 900, false, Some(1)),
                    trace("absent", 800, false, None),
                    trace("busy", 100, true, None),
                ],
                rejected: Vec::new(),
            },
        };
        assert_eq!(
            render_dump(&dump),
            "  900  guarded  bottom_non_empty(4)  vetoed by guard 1  BUSY | done\n\
             \x20 800  absent  bottom_non_empty(4)  no match  BUSY | done\n\
             \x20 100  busy  bottom_non_empty(4)  matched  BUSY | done\n\
             winner: busy (working)\n\
             title: my title\n\
             grid 20x2:\n\
             BUSY\n\
             done\n"
        );
    }

    #[test]
    fn render_dump_names_no_winner_and_every_rejection() {
        let dump = ScreenDump {
            cols: 4,
            rows: 1,
            grid: vec![String::new()],
            title: None,
            explanation: ScreenExplanation {
                verdict: "no_match".to_owned(),
                winner: None,
                from_title: false,
                rules: Vec::new(),
                rejected: vec!["screen rule \"x\": bad".to_owned()],
            },
        };
        assert_eq!(
            render_dump(&dump),
            "winner: none (no_match)\nrejected: screen rule \"x\": bad\ngrid 4x1:\n\n"
        );
    }

    #[test]
    fn welcome_without_the_feature_refuses_an_older_daemon() {
        let welcome = |features: Vec<String>| {
            serde_json::to_string(&DaemonMessage::Welcome {
                protocol_version: 22,
                supported_versions: vec![22],
                features,
                daemon_id: None,
            })
            .expect("the welcome serializes")
        };
        let older = welcome(vec!["agent_commands".to_owned()]);
        assert_eq!(
            classify(&older, false),
            Step::Welcome { screen_dump: false }
        );
        let err = check_welcome(false).expect_err("an older daemon is refused");
        assert_eq!(err.to_string(), OLDER_DAEMON);
        let newer = welcome(vec![FEATURE_SCREEN_DUMP.to_owned()]);
        assert_eq!(classify(&newer, false), Step::Welcome { screen_dump: true });
        check_welcome(true).expect("a daemon that can dump is welcomed");
    }

    #[test]
    fn fixture_writes_lf_rows_with_a_trailing_newline() {
        let scratch = ScratchDir::new("cli-screen-fixture");
        let path = scratch.path().join("capture.txt");
        let grid = vec!["one  ".to_owned(), "two".to_owned(), String::new()];
        write_fixture(&path, &grid).expect("the fixture is written");
        let bytes = std::fs::read(&path).expect("read the fixture");
        assert_eq!(bytes, b"one\ntwo\n\n");
    }

    #[test]
    fn fixture_refuses_an_existing_path() {
        let scratch = ScratchDir::new("cli-screen-fixture-exists");
        let path = scratch.path().join("capture.txt");
        std::fs::write(&path, "kept").expect("write the existing file");
        let err = write_fixture(&path, &["new".to_owned()]).expect_err("an existing file is kept");
        assert_eq!(
            err.to_string(),
            format!("{} already exists", path.display())
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("read it back"),
            "kept"
        );
    }
}
