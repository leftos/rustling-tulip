//! The session-driving verbs: `sessions`, `read`, `spawn`, `send`, `kill`,
//! `wait` and `watch`, thin subcommands over the protocol every client speaks.
//!
//! They act on whichever daemon the active config dir's `daemon.json` names.
//! They send no `Resize` and no client identity, so the user's panes and
//! layouts stay as they are.

mod args;
mod keys;
mod render;
mod scrollback;
#[cfg(test)]
mod verb_tests;
mod verbs;
mod waiting;

use crate::cli_client::{Connection, NoDaemon, Welcome};
use anyhow::anyhow;
use args::{Command, Verb};
use base64::Engine as _;
use protocol::{ClientMessage, DaemonMessage, SessionSnapshot};
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

/// The whole exchange must finish inside this, or the verb gives up.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(10);

/// A spawn must answer inside this: it may check out and start a process.
const SPAWN_TIMEOUT: Duration = Duration::from_secs(30);

/// The request id a verb asks with, so its reply is told apart.
const REQUEST_ID: &str = "drive";

const EXIT_OK: u8 = 0;
const EXIT_FAILED: u8 = 1;
const EXIT_USAGE: u8 = 2;
const EXIT_TIMED_OUT: u8 = 4;
const EXIT_NO_DAEMON: u8 = 5;

const CODE_FAILED: &str = "failed";
const CODE_USAGE: &str = "usage";
const CODE_NO_DAEMON: &str = "no_daemon";
const CODE_TIMED_OUT: &str = "timed_out";
const CODE_ENDED: &str = "ended";
const CODE_CHECKOUT_CONFIRM: &str = "checkout_confirm";

/// The words that name a verb.
const VERBS: [&str; 7] = ["sessions", "read", "spawn", "send", "kill", "wait", "watch"];

/// The waits a verb allows.
#[derive(Debug, Clone, Copy)]
struct Budgets {
    /// How long the listener has to prove it holds the token.
    proof: Duration,
    /// How long the connection has to open, and a verb that takes a bounded
    /// exchange has to finish it.
    exchange: Duration,
    /// How long `spawn` has.
    spawn: Duration,
}

const BUDGETS: Budgets = Budgets {
    proof: daemon_client::PROOF_TIMEOUT,
    exchange: EXCHANGE_TIMEOUT,
    spawn: SPAWN_TIMEOUT,
};

/// Why a verb failed: what it prints, and the process's exit code.
#[derive(Debug, PartialEq, Eq)]
struct Failure {
    code: String,
    message: String,
    exit: u8,
}

impl Failure {
    fn failed(message: impl Into<String>) -> Self {
        Self {
            code: CODE_FAILED.to_owned(),
            message: message.into(),
            exit: EXIT_FAILED,
        }
    }

    fn usage(message: String) -> Self {
        Self {
            code: CODE_USAGE.to_owned(),
            message,
            exit: EXIT_USAGE,
        }
    }

    /// A refusal the daemon sent, under its own code when it gave one.
    fn refused(code: Option<String>, message: String) -> Self {
        Self {
            code: code.unwrap_or_else(|| CODE_FAILED.to_owned()),
            message,
            exit: EXIT_FAILED,
        }
    }

    fn unknown_session(session_id: &str) -> Self {
        Self::refused(
            Some(protocol::error_codes::NOT_FOUND.to_owned()),
            format!("unknown session: {session_id}"),
        )
    }

    fn ended(session_id: &str) -> Self {
        Self::refused(
            Some(CODE_ENDED.to_owned()),
            format!("session {session_id} ended"),
        )
    }

    fn checkout_confirm(message: String) -> Self {
        Self::refused(Some(CODE_CHECKOUT_CONFIRM.to_owned()), message)
    }

    fn timed_out(message: String) -> Self {
        Self {
            code: CODE_TIMED_OUT.to_owned(),
            message,
            exit: EXIT_TIMED_OUT,
        }
    }

    /// The failure a connection or protocol error stands for.
    fn from_error(err: &anyhow::Error) -> Self {
        let message = format!("{err:#}");
        if err.downcast_ref::<NoDaemon>().is_some() {
            Self {
                code: CODE_NO_DAEMON.to_owned(),
                message,
                exit: EXIT_NO_DAEMON,
            }
        } else {
            Self::failed(message)
        }
    }
}

/// True when the first argument names one of these verbs.
pub fn requested(args: impl IntoIterator<Item = OsString>) -> bool {
    args.into_iter()
        .next()
        .is_some_and(|arg| VERBS.iter().any(|verb| arg == *verb))
}

/// Run the verb in `args`, the daemon's arguments from the verb on, against
/// the active config dir's daemon, on this process's stdout and stderr.
pub async fn run_and_report(args: impl IntoIterator<Item = OsString>) -> std::process::ExitCode {
    let args: Vec<OsString> = args.into_iter().collect();
    let mut stdout = std::io::stdout();
    let result = execute(daemon_client::handshake_file, &args, BUDGETS, &mut stdout).await;
    let code = report(
        &args,
        result,
        &mut std::io::stdout().lock(),
        &mut std::io::stderr().lock(),
    );
    std::process::ExitCode::from(code)
}

/// Print `result` and answer the exit code. Output reaches `out` only on
/// success, and a failure is one stderr line, or with `--json` one JSON line
/// on `out` in its place, so a usage error honours the flag too.
fn report(
    args: &[OsString],
    result: Result<String, Failure>,
    out: &mut impl std::io::Write,
    err: &mut impl std::io::Write,
) -> u8 {
    let failure = match result {
        Ok(text) => match out.write_all(text.as_bytes()) {
            Ok(()) => return EXIT_OK,
            Err(write_err) => Failure::failed(format!("could not write to stdout: {write_err}")),
        },
        Err(failure) => failure,
    };
    if args::json_errors(args) {
        let line = render::error_json(&failure.code, &failure.message);
        let _ = out.write_all(line.as_bytes());
    } else {
        let verb = args.first().map(|arg| arg.to_string_lossy());
        let verb = verb.as_deref().unwrap_or_default();
        let _ = writeln!(err, "rustling-tulipd {verb}: {}", failure.message);
    }
    failure.exit
}

/// Parse `args`, find the handshake file through `handshake_file`, open one
/// connection and run the verb over it, answering what is left to print. A
/// verb that follows a session writes its lines to `out` as they come.
/// Closing the connection also ends any forwarder the verb's request started.
async fn execute(
    handshake_file: impl FnOnce() -> anyhow::Result<PathBuf>,
    args: &[OsString],
    budgets: Budgets,
    out: &mut impl std::io::Write,
) -> Result<String, Failure> {
    let command = args::parse(args.iter().cloned()).map_err(Failure::usage)?;
    let handshake_file = handshake_file().map_err(|err| Failure::from_error(&err))?;
    let client_name = format!("rustling-tulipd {}", command.verb.name());
    let opened = Connection::open(&handshake_file, &client_name, budgets.proof);
    let (mut connection, welcome) = within(budgets.exchange, opened)
        .await?
        .map_err(|err| Failure::from_error(&err))?;
    let verb = run_verb(&mut connection, &welcome, &command, out);
    let printed = match verb_budget(&command.verb, budgets) {
        Some(budget) => within(budget, verb).await.and_then(|printed| printed),
        None => verb.await,
    };
    connection.close().await;
    printed
}

/// Await `work`, or fail once `budget` has passed.
async fn within<T>(budget: Duration, work: impl Future<Output = T>) -> Result<T, Failure> {
    tokio::time::timeout(budget, work).await.map_err(|_| {
        let seconds = budget.as_secs();
        Failure::failed(format!("rustling-tulipd did not answer within {seconds} s"))
    })
}

/// The time `verb` has; `wait` and `watch` keep their own.
fn verb_budget(verb: &Verb, budgets: Budgets) -> Option<Duration> {
    match verb {
        Verb::Spawn(_) => Some(budgets.spawn),
        Verb::Wait { .. } | Verb::Watch { .. } => None,
        Verb::Sessions | Verb::Read { .. } | Verb::Send { .. } | Verb::Kill { .. } => {
            Some(budgets.exchange)
        }
    }
}

async fn run_verb(
    connection: &mut Connection,
    welcome: &Welcome,
    command: &Command,
    out: &mut impl std::io::Write,
) -> Result<String, Failure> {
    let json = command.json;
    match &command.verb {
        Verb::Sessions => sessions(connection, json).await,
        Verb::Read {
            session_id,
            scrollback: false,
        } => read_screen(connection, welcome, session_id, json).await,
        Verb::Read {
            session_id,
            scrollback: true,
        } => read_scrollback(connection, session_id, json).await,
        Verb::Spawn(options) => verbs::spawn(connection, options, json).await,
        Verb::Send {
            session_id,
            payload,
        } => verbs::send(connection, welcome, session_id, payload, json).await,
        Verb::Kill { session_id } => verbs::kill(connection, session_id, json).await,
        Verb::Wait {
            session_id,
            until,
            timeout,
        } => {
            let request = verbs::WaitRequest {
                session_id,
                until,
                limit: *timeout,
                json,
            };
            verbs::wait(connection, welcome, &request).await
        }
        Verb::Watch {
            session_id,
            timeout,
        } => verbs::watch(connection, session_id, *timeout, json, out).await,
    }
}

/// The `Sessions` message the daemon pushes after its `Welcome`.
async fn initial_sessions(connection: &mut Connection) -> Result<Vec<SessionSnapshot>, Failure> {
    loop {
        match connection.next_message().await {
            None => return Err(closed_early("the sessions")),
            Some(DaemonMessage::Sessions { sessions }) => return Ok(sessions),
            Some(_) => {}
        }
    }
}

async fn sessions(connection: &mut Connection, json: bool) -> Result<String, Failure> {
    let sessions = initial_sessions(connection).await?;
    Ok(if json {
        render::sessions_json(&sessions)
    } else {
        render::sessions_text(&sessions)
    })
}

async fn read_screen(
    connection: &mut Connection,
    welcome: &Welcome,
    session_id: &str,
    json: bool,
) -> Result<String, Failure> {
    crate::cli_screen::check_welcome(crate::cli_screen::supports_screen_dump(&welcome.features))
        .map_err(|err| Failure::from_error(&err))?;
    ask(
        connection,
        &ClientMessage::DumpScreen {
            session_id: session_id.to_owned(),
            request_id: Some(REQUEST_ID.to_owned()),
        },
    )
    .await?;
    await_reply(connection, "the screen", |message| match message {
        DaemonMessage::ScreenDump {
            request_id,
            cols,
            rows,
            grid,
            title,
            ..
        } if is_mine(request_id.as_deref()) => Some(if json {
            render::screen_json(session_id, (cols, rows), title.as_deref(), &grid)
        } else {
            render::screen_text(&grid)
        }),
        _ => None,
    })
    .await
}

/// A session's scrollback as text. `LoadScrollback` answers an unknown id with
/// an empty reply, so the id is first checked against the sessions pushed
/// after the `Welcome`.
async fn read_scrollback(
    connection: &mut Connection,
    session_id: &str,
    json: bool,
) -> Result<String, Failure> {
    let sessions = initial_sessions(connection).await?;
    if !sessions.iter().any(|session| session.id == session_id) {
        return Err(Failure::unknown_session(session_id));
    }
    ask(
        connection,
        &ClientMessage::LoadScrollback {
            session_id: session_id.to_owned(),
            request_id: Some(REQUEST_ID.to_owned()),
        },
    )
    .await?;
    let (data_b64, truncated) =
        await_reply(connection, "the scrollback", |message| match message {
            DaemonMessage::Scrollback {
                request_id,
                data_b64,
                truncated,
                ..
            } if is_mine(request_id.as_deref()) => Some((data_b64, truncated)),
            _ => None,
        })
        .await?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data_b64)
        .map_err(|err| Failure::from_error(&anyhow!("the daemon sent bad scrollback: {err}")))?;
    let text = scrollback::plain_text(&bytes);
    Ok(if json {
        render::scrollback_json(session_id, truncated, &text)
    } else {
        render::scrollback_text(&text)
    })
}

async fn ask(connection: &mut Connection, message: &ClientMessage) -> Result<(), Failure> {
    connection
        .send(message)
        .await
        .map_err(|err| Failure::from_error(&err))
}

fn is_mine(request_id: Option<&str>) -> bool {
    request_id == Some(REQUEST_ID)
}

/// Read on until `pick` takes a message, the daemon refuses this request, or
/// it closes the connection.
async fn await_reply<T>(
    connection: &mut Connection,
    what: &str,
    mut pick: impl FnMut(DaemonMessage) -> Option<T>,
) -> Result<T, Failure> {
    loop {
        match connection.next_message().await {
            None => return Err(closed_early(what)),
            Some(DaemonMessage::Error {
                message,
                request_id,
                code,
            }) if is_mine(request_id.as_deref()) => {
                return Err(Failure::refused(code, message));
            }
            Some(message) => {
                if let Some(reply) = pick(message) {
                    return Ok(reply);
                }
            }
        }
    }
}

fn closed_early(what: &str) -> Failure {
    Failure::failed(format!(
        "rustling-tulipd closed the connection before sending {what}"
    ))
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
pub(super) mod tests {
    use super::*;
    use crate::history::test_support::record;
    use crate::server::test_support::{ScratchDir, serve_loopback, test_hub, unproven_listener};
    use protocol::{DaemonHandshake, SUPPORTED_PROTOCOL_VERSIONS, SessionMode};
    use std::path::Path;

    /// The only bound on a test's waits: a hang guard.
    pub(in crate::cli_drive) const HANG_GUARD: Duration = Duration::from_secs(120);

    const TEST_BUDGETS: Budgets = Budgets {
        proof: HANG_GUARD,
        exchange: HANG_GUARD,
        spawn: HANG_GUARD,
    };

    /// What a finished verb left behind.
    pub(in crate::cli_drive) struct Outcome {
        pub(in crate::cli_drive) exit: u8,
        pub(in crate::cli_drive) out: String,
        pub(in crate::cli_drive) err: String,
    }

    pub(in crate::cli_drive) async fn run_verb(handshake_file: &Path, list: &[&str]) -> Outcome {
        run_verb_into(handshake_file, list, Vec::new()).await
    }

    /// [`run_verb`] with the lines the verb streams going to `lines`. The
    /// answer it prints at the end lands in the outcome.
    pub(in crate::cli_drive) async fn run_verb_into(
        handshake_file: &Path,
        list: &[&str],
        mut lines: impl std::io::Write,
    ) -> Outcome {
        let args: Vec<OsString> = list.iter().map(OsString::from).collect();
        let result = tokio::time::timeout(
            HANG_GUARD,
            execute(
                || Ok(handshake_file.to_owned()),
                &args,
                TEST_BUDGETS,
                &mut lines,
            ),
        )
        .await
        .expect("hung: the verb did not finish");
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let exit = report(&args, result, &mut out, &mut err);
        Outcome {
            exit,
            out: String::from_utf8(out).expect("stdout is UTF-8"),
            err: String::from_utf8(err).expect("stderr is UTF-8"),
        }
    }

    /// A handshake file for the test hub listening on `port`.
    pub(in crate::cli_drive) fn handshake_file(scratch: &ScratchDir, port: u16) -> PathBuf {
        let handshake = DaemonHandshake {
            protocol_version: SUPPORTED_PROTOCOL_VERSIONS
                .iter()
                .copied()
                .max()
                .unwrap_or_default(),
            port,
            auth_token: "test-token".to_owned(),
            pid: 1,
            supported_versions: SUPPORTED_PROTOCOL_VERSIONS.to_vec(),
            build_version: String::new(),
        };
        let path = scratch.path().join("cli-daemon.json");
        let json = serde_json::to_vec(&handshake).expect("serialize the handshake");
        std::fs::write(&path, json).expect("write the handshake file");
        path
    }

    #[tokio::test]
    async fn sessions_lists_a_session_the_hub_holds() {
        let (hub, scratch) = test_hub("cli-drive-sessions");
        hub.sessions
            .insert_pending(record("s1", SessionMode::PlainShell))
            .publish_from(None);
        let port = serve_loopback(&hub).await;
        let file = handshake_file(&scratch, port);

        let text = run_verb(&file, &["sessions"]).await;
        assert_eq!(text.exit, EXIT_OK, "{}", text.err);
        assert!(text.out.starts_with("s1\tidle\t-\t"), "{:?}", text.out);

        let json = run_verb(&file, &["sessions", "--json"]).await;
        let rows: serde_json::Value = serde_json::from_str(&json.out).expect("valid JSON");
        assert_eq!(rows[0]["id"], "s1", "{rows}");
    }

    #[tokio::test]
    async fn read_of_an_unknown_id_fails_with_not_found() {
        let (hub, scratch) = test_hub("cli-drive-read-unknown");
        let port = serve_loopback(&hub).await;
        let file = handshake_file(&scratch, port);

        for list in [
            &["read", "ghost"][..],
            &["read", "ghost", "--scrollback"][..],
        ] {
            let outcome = run_verb(&file, list).await;
            assert_eq!(outcome.exit, EXIT_FAILED, "{list:?}");
            assert_eq!(outcome.out, "", "{list:?}");
            assert_eq!(
                outcome.err, "rustling-tulipd read: unknown session: ghost\n",
                "{list:?}"
            );
        }

        let json = run_verb(&file, &["read", "ghost", "--json"]).await;
        assert_eq!(json.exit, EXIT_FAILED);
        assert_eq!(json.err, "");
        let value: serde_json::Value = serde_json::from_str(&json.out).expect("valid JSON");
        assert_eq!(value["error"]["code"], protocol::error_codes::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_missing_handshake_file_exits_five() {
        let scratch = ScratchDir::new("cli-drive-no-daemon");
        let file = scratch.path().join("daemon.json");

        let text = run_verb(&file, &["sessions"]).await;
        assert_eq!(text.exit, EXIT_NO_DAEMON);
        assert_eq!(text.out, "");
        assert!(
            text.err
                .starts_with("rustling-tulipd sessions: no rustling-tulipd is running: "),
            "{:?}",
            text.err
        );

        let json = run_verb(&file, &["read", "s1", "--json"]).await;
        assert_eq!(json.exit, EXIT_NO_DAEMON);
        let value: serde_json::Value = serde_json::from_str(&json.out).expect("valid JSON");
        assert_eq!(value["error"]["code"], CODE_NO_DAEMON);
    }

    #[tokio::test]
    async fn a_handshake_path_failure_reports_as_a_failure() {
        let args: Vec<OsString> = ["sessions", "--json"].iter().map(OsString::from).collect();
        let result = execute(
            || Err(anyhow!("no config dir")),
            &args,
            TEST_BUDGETS,
            &mut Vec::new(),
        )
        .await;
        let (mut out, mut err) = (Vec::new(), Vec::new());

        let exit = report(&args, result, &mut out, &mut err);

        assert_eq!(exit, EXIT_FAILED);
        assert_eq!(err, b"");
        let value: serde_json::Value = serde_json::from_slice(&out).expect("valid JSON");
        assert_eq!(value["error"]["code"], CODE_FAILED);
        assert_eq!(value["error"]["message"], "no config dir");
    }

    #[tokio::test]
    async fn a_port_nothing_listens_on_exits_five_and_an_unproven_listener_one() {
        let scratch = ScratchDir::new("cli-drive-dead-port");
        let bound = tokio::net::TcpSocket::new_v4().expect("a socket");
        bound
            .bind("127.0.0.1:0".parse().expect("a loopback address"))
            .expect("hold a port without listening on it");
        let dead_port = bound.local_addr().expect("the bound address").port();

        let json = run_verb(
            &handshake_file(&scratch, dead_port),
            &["sessions", "--json"],
        )
        .await;
        assert_eq!(json.exit, EXIT_NO_DAEMON, "{}", json.out);
        let value: serde_json::Value = serde_json::from_str(&json.out).expect("valid JSON");
        assert_eq!(value["error"]["code"], CODE_NO_DAEMON);

        let (port, _received) = unproven_listener();
        let unproven = run_verb(&handshake_file(&scratch, port), &["sessions"]).await;
        assert_eq!(unproven.exit, EXIT_FAILED, "{}", unproven.err);
    }
}
