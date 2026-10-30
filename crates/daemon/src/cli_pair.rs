//! The daemon's `pair` subcommand: print a one-time device pairing code.
//!
//! `rustling-tulipd pair` is the always-available fallback for an owner
//! reached over SSH: it asks the running daemon for a one-time code and prints
//! it, and the owner types that code on a device whose pairing request is
//! waiting.

use anyhow::anyhow;
use daemon_client::{Proof, ProvenConnectError};
use futures::{SinkExt as _, StreamExt as _};
use protocol::{
    ClientMessage, DaemonHandshake, DaemonMessage, InboundDaemonMessage,
    SUPPORTED_PROTOCOL_VERSIONS,
};
use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// The whole exchange — reading the handshake, connecting, minting a code —
/// must finish inside this, or the subcommand gives up.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(10);

/// The client name the daemon logs this connection under.
const CLIENT_NAME: &str = "rustling-tulipd pair";

/// One client connection to the daemon's loopback listener.
type PairSocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// A freshly minted pairing code and how long it stays redeemable.
#[derive(Debug, PartialEq, Eq)]
struct Minted {
    code: String,
    expires_in_secs: u64,
}

/// True when the daemon's arguments ask for `pair`: the first argument is
/// exactly `pair`.
pub fn requested(args: impl IntoIterator<Item = OsString>) -> bool {
    args.into_iter().next().is_some_and(|arg| arg == "pair")
}

/// Give the subcommand a console to print on.
///
/// Every non-test Windows build of the daemon is a GUI-subsystem exe, so a
/// `pair` run from cmd, PowerShell or an OpenSSH shell starts with no console
/// of its own and null standard handles: Rust's std treats a write to a null
/// handle as a success, so the code and any error text would vanish. Attaching
/// to the parent's console puts them back on the caller's terminal. A shell
/// does not wait for a GUI-subsystem exe either, so the two lines can land
/// after the prompt has already come back.
///
/// A failed attach means there was no parent console to join — the output was
/// redirected to a pipe or a file — and the inherited handles are left exactly
/// as they are, so `rustling-tulipd pair > code.txt` still captures both
/// lines.
#[cfg(windows)]
pub fn attach_parent_console() {
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
        SetStdHandle,
    };

    // SAFETY: attaches this process to whatever console the parent holds; a
    // failure changes nothing.
    if unsafe { AttachConsole(ATTACH_PARENT_PROCESS) }.is_err() {
        return;
    }
    // SAFETY: reads this process's own standard output handle. Only a real
    // handle counts: a null one means the launch gave this GUI-subsystem exe
    // none at all, which is what the console below replaces.
    if unsafe { GetStdHandle(STD_OUTPUT_HANDLE) }.is_ok_and(|handle| !handle.is_invalid()) {
        // A redirect survived the launch: its handle is the one to keep.
        return;
    }
    // The console device's name, NUL-terminated UTF-16 that outlives the call.
    let name: Vec<u16> = "CONOUT$\0".encode_utf16().collect();
    // SAFETY: opens the console device the attach just gave this process. The
    // handle lives as long as the process, which is the only writer.
    let Ok(console) = (unsafe {
        CreateFileW(
            windows::core::PCWSTR(name.as_ptr()),
            (FILE_GENERIC_READ | FILE_GENERIC_WRITE).0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }) else {
        return;
    };
    // SAFETY: installs the console handle as this process's output and error
    // stream; both setters only reject an invalid handle, which the open
    // above never yields.
    let _ = unsafe { SetStdHandle(STD_OUTPUT_HANDLE, console) };
    // SAFETY: as above, for the error stream.
    let _ = unsafe { SetStdHandle(STD_ERROR_HANDLE, console) };
}

/// Nothing to attach: on other platforms the binary keeps the caller's
/// standard handles.
#[cfg(not(windows))]
pub fn attach_parent_console() {}

/// Connect to the running daemon named by `handshake_file`, mint a one-time
/// pairing code and write it to `out` with how long it stays valid.
pub async fn run(handshake_file: &Path, out: &mut impl std::io::Write) -> anyhow::Result<()> {
    run_with(handshake_file, out, daemon_client::PROOF_TIMEOUT).await
}

/// [`run`], giving the daemon proof `proof_budget`.
async fn run_with(
    handshake_file: &Path,
    out: &mut impl std::io::Write,
    proof_budget: Duration,
) -> anyhow::Result<()> {
    let timeout_secs = EXCHANGE_TIMEOUT.as_secs();
    let minted = tokio::time::timeout(EXCHANGE_TIMEOUT, exchange(handshake_file, proof_budget))
        .await
        .map_err(|_| anyhow!("rustling-tulipd sent no pairing code within {timeout_secs} s"))??;
    let Minted {
        code,
        expires_in_secs,
    } = minted;
    writeln!(out, "Pairing code: {code}")?;
    writeln!(
        out,
        "Type it on the device's pairing screen within {}. It works once, for one device.",
        duration_text(expires_in_secs)
    )?;
    Ok(())
}

/// Read the handshake file and mint a code over one loopback connection,
/// opened only once the listener proves it holds the token within
/// `proof_budget`.
async fn exchange(handshake_file: &Path, proof_budget: Duration) -> anyhow::Result<Minted> {
    let handshake = read_handshake(handshake_file)?;
    let port = handshake.port;
    let stream =
        match daemon_client::connect_proven(port, &handshake.auth_token, proof_budget).await {
            Ok(stream) => stream,
            Err(ProvenConnectError::NotProven(proof)) => {
                return Err(anyhow!(
                    "{}",
                    not_proven_reason(port, handshake_file, proof)
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
    let minted = read_until_code(&mut socket).await?;
    let _ = socket.close(None).await;
    Ok(minted)
}

/// The error when the listener on `port`, named by `path`, did not prove it
/// holds the token, telling an impostor from a slow or vanished daemon.
pub(crate) fn not_proven_reason(port: u16, path: &Path, proof: Proof) -> String {
    let path = path.display();
    match proof {
        // `Proven` never reaches here; it reads as the impostor case.
        Proof::Proven | Proof::Unproven => format!(
            "the listener on 127.0.0.1:{port} named by {path} did not prove it is \
             rustling-tulipd; nothing was sent"
        ),
        Proof::NoResponse => format!(
            "the listener on 127.0.0.1:{port} named by {path} did not answer the \
             rustling-tulipd proof in time; nothing was sent"
        ),
        Proof::Refused => format!(
            "the listener on 127.0.0.1:{port} named by {path} closed before it could \
             prove it is rustling-tulipd; nothing was sent"
        ),
    }
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
/// protocol version this build speaks, and no client identity, so the
/// connection keys no tab layout.
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
    /// The daemon welcomed the connection: ask for the code.
    Mint,
    /// The code arrived.
    Code(Minted),
    /// The daemon refused; the string is the whole error message.
    Refused(String),
}

/// Read one frame's text as the daemon's answer.
///
/// A frame that does not decode, an unknown type, and anything the exchange
/// does not act on are all [`Step::Skip`]: the daemon pushes its whole initial
/// state first, and a code already minted must not be lost to a frame in
/// between. `DevicePairFailed` and `Error` refuse only once the mint was sent —
/// before that they belong to that initial state.
fn classify(text: &str, mint_sent: bool) -> Step {
    let Ok(InboundDaemonMessage::Known(message)) = InboundDaemonMessage::from_json_str(text) else {
        return Step::Skip;
    };
    match *message {
        DaemonMessage::Welcome { .. } if !mint_sent => Step::Mint,
        DaemonMessage::DevicePairCode {
            request_id: None,
            code,
            expires_in_secs,
        } if mint_sent => Step::Code(Minted {
            code: code.0,
            expires_in_secs,
        }),
        DaemonMessage::AuthFailed { reason } => {
            Step::Refused(format!("rustling-tulipd refused the handshake: {reason}"))
        }
        DaemonMessage::DevicePairFailed { reason } if mint_sent => Step::Refused(format!(
            "rustling-tulipd refused the pairing code: {reason}"
        )),
        DaemonMessage::Error { message, .. } if mint_sent => Step::Refused(format!(
            "rustling-tulipd refused the pairing code: {message}"
        )),
        _ => Step::Skip,
    }
}

/// Ask for the code as soon as the daemon welcomes the connection, and read
/// on past the initial state it pushes until the code, a refusal or a close.
async fn read_until_code(socket: &mut PairSocket) -> anyhow::Result<Minted> {
    let mut mint_sent = false;
    loop {
        let Some(text) = next_text(socket).await else {
            return Err(anyhow!(
                "rustling-tulipd closed the connection before sending a pairing code"
            ));
        };
        match classify(&text, mint_sent) {
            Step::Skip => {}
            Step::Mint => {
                mint_sent = true;
                send(socket, &ClientMessage::MintDevicePairCode).await?;
            }
            Step::Code(minted) => return Ok(minted),
            Step::Refused(reason) => return Err(anyhow!("{reason}")),
        }
    }
}

/// The next text frame, or `None` once the daemon closed the connection.
async fn next_text(socket: &mut PairSocket) -> Option<String> {
    loop {
        match socket.next().await {
            None | Some(Err(_) | Ok(Message::Close(_))) => return None,
            Some(Ok(Message::Text(text))) => return Some(text.to_string()),
            Some(Ok(_)) => {}
        }
    }
}

async fn send(socket: &mut PairSocket, message: &ClientMessage) -> anyhow::Result<()> {
    let json = serde_json::to_string(message)
        .map_err(|err| anyhow!("could not serialize a client message: {err}"))?;
    socket
        .send(Message::text(json))
        .await
        .map_err(|err| anyhow!("could not send to rustling-tulipd: {err}"))
}

/// A code lifetime in the words the subcommand prints.
fn duration_text(secs: u64) -> String {
    if secs == 60 {
        "1 minute".to_owned()
    } else if secs > 60 && secs.is_multiple_of(60) {
        format!("{} minutes", secs / 60)
    } else {
        format!("{secs} seconds")
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use crate::server::test_support::{ScratchDir, serve_loopback, test_hub, unproven_listener};
    use std::ffi::OsString;
    use std::time::Instant;

    /// The `daemon.json` a running daemon leaves in its config dir, written
    /// into `dir` for `run` to read.
    fn write_handshake(dir: &Path, port: u16, auth_token: &str) -> std::path::PathBuf {
        let handshake = DaemonHandshake {
            protocol_version: SUPPORTED_PROTOCOL_VERSIONS
                .iter()
                .copied()
                .max()
                .unwrap_or_default(),
            port,
            auth_token: auth_token.to_owned(),
            pid: 1,
            supported_versions: SUPPORTED_PROTOCOL_VERSIONS.to_vec(),
        };
        let path = dir.join("daemon.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&handshake).expect("serialize the handshake"),
        )
        .expect("write the handshake file");
        path
    }

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn pair_is_requested_only_as_the_first_argument() {
        assert!(requested(args(&["pair"])));
        let empty: &[&str] = &[];
        for list in [empty, &["--detach"], &["--detach", "pair"], &["pairs"]] {
            assert!(!requested(args(list)), "{list:?}");
        }
    }

    #[test]
    fn duration_reads_in_minutes_or_seconds() {
        assert_eq!(duration_text(120), "2 minutes");
        assert_eq!(duration_text(60), "1 minute");
        assert_eq!(duration_text(90), "90 seconds");
        assert_eq!(duration_text(30), "30 seconds");
    }

    fn frame(message: &DaemonMessage) -> String {
        serde_json::to_string(message).expect("serialize a daemon message")
    }

    #[test]
    fn a_frame_that_does_not_decode_is_skipped() {
        assert_eq!(classify("not json at all", false), Step::Skip);
        assert_eq!(classify("not json at all", true), Step::Skip);
        assert_eq!(
            classify(r#"{"type":"a_message_from_a_newer_daemon"}"#, true),
            Step::Skip,
            "an unknown message type is skipped"
        );
    }

    #[test]
    fn the_welcome_asks_for_the_mint_once() {
        let welcome = frame(&DaemonMessage::Welcome {
            protocol_version: protocol::PROTOCOL_VERSION,
            supported_versions: SUPPORTED_PROTOCOL_VERSIONS.to_vec(),
            features: Vec::new(),
        });
        assert_eq!(classify(&welcome, false), Step::Mint);
        assert_eq!(
            classify(&welcome, true),
            Step::Skip,
            "one mint per connection"
        );
    }

    #[test]
    fn a_refusal_counts_only_after_the_mint() {
        let error = frame(&DaemonMessage::Error {
            message: "boom".to_owned(),
            request_id: None,
            code: None,
        });
        assert_eq!(
            classify(&error, false),
            Step::Skip,
            "the initial state may carry an error before the mint"
        );
        assert_eq!(
            classify(&error, true),
            Step::Refused("rustling-tulipd refused the pairing code: boom".to_owned())
        );

        let failed = frame(&DaemonMessage::DevicePairFailed {
            reason: "no".to_owned(),
        });
        assert_eq!(classify(&failed, false), Step::Skip);
        assert_eq!(
            classify(&failed, true),
            Step::Refused("rustling-tulipd refused the pairing code: no".to_owned())
        );
    }

    #[test]
    fn the_code_ends_the_read() {
        let code = frame(&DaemonMessage::DevicePairCode {
            request_id: None,
            code: protocol::Secret("123456".to_owned()),
            expires_in_secs: 120,
        });
        assert_eq!(
            classify(&code, false),
            Step::Skip,
            "no code before the mint"
        );
        assert_eq!(
            classify(&code, true),
            Step::Code(Minted {
                code: "123456".to_owned(),
                expires_in_secs: 120,
            })
        );
    }

    #[tokio::test]
    async fn pair_prints_a_code_the_daemon_accepts() {
        let (hub, scratch) = test_hub("cli-pair");
        let port = serve_loopback(&hub).await;
        let path = write_handshake(scratch.path(), port, "test-token");

        let mut out = Vec::new();
        run_with(&path, &mut out, HANG_GUARD)
            .await
            .expect("the pairing exchange succeeds");

        let text = String::from_utf8(out).expect("the output is UTF-8");
        let mut lines = text.lines();
        let code = lines
            .next()
            .expect("the code line")
            .strip_prefix("Pairing code: ")
            .expect("the first line labels the code")
            .to_owned();
        assert_eq!(
            lines.next(),
            Some(
                "Type it on the device's pairing screen within 2 minutes. \
                 It works once, for one device."
            )
        );
        assert_eq!(lines.next(), None, "{text:?}");

        crate::sync::lock(&hub.pairing)
            .redeem_cli_code(&code, Instant::now())
            .expect("the daemon accepts the printed code");
        assert!(
            crate::sync::lock(&hub.pairing)
                .redeem_cli_code(&code, Instant::now())
                .is_err(),
            "a printed code works once"
        );
    }

    #[tokio::test]
    async fn pair_fails_when_no_daemon_runs() {
        let scratch = ScratchDir::new("cli-pair-absent");
        let path = scratch.path().join("daemon.json");

        let err = run_with(&path, &mut Vec::new(), HANG_GUARD)
            .await
            .expect_err("no daemon runs for an absent handshake file");

        assert_eq!(
            err.to_string(),
            format!(
                "no rustling-tulipd is running: {} does not exist",
                path.display()
            )
        );
    }

    #[tokio::test]
    async fn pair_fails_on_a_wrong_token() {
        let (hub, scratch) = test_hub("cli-pair-wrong-token");
        let port = serve_loopback(&hub).await;
        let path = write_handshake(scratch.path(), port, "not-the-daemons-token");

        let err = run_with(&path, &mut Vec::new(), HANG_GUARD)
            .await
            .expect_err("a wrong token never reaches the daemon");

        let text = err.to_string();
        assert!(text.starts_with("the listener on 127.0.0.1:"), "{err}");
        assert!(
            text.contains("did not prove it is rustling-tulipd; nothing was sent"),
            "{err}"
        );
    }

    /// The proof budget every test passes: a hang guard only, so no test
    /// depends on the production proof timeout.
    const HANG_GUARD: Duration = Duration::from_secs(120);

    #[test]
    fn not_proven_reason_names_an_impostor() {
        assert_eq!(
            not_proven_reason(4242, Path::new("daemon.json"), Proof::Unproven),
            "the listener on 127.0.0.1:4242 named by daemon.json did not prove it is \
             rustling-tulipd; nothing was sent"
        );
    }

    #[test]
    fn not_proven_reason_names_a_silent_listener() {
        assert_eq!(
            not_proven_reason(4242, Path::new("daemon.json"), Proof::NoResponse),
            "the listener on 127.0.0.1:4242 named by daemon.json did not answer the \
             rustling-tulipd proof in time; nothing was sent"
        );
    }

    #[test]
    fn not_proven_reason_names_a_vanished_listener() {
        assert_eq!(
            not_proven_reason(4242, Path::new("daemon.json"), Proof::Refused),
            "the listener on 127.0.0.1:4242 named by daemon.json closed before it could \
             prove it is rustling-tulipd; nothing was sent"
        );
    }

    #[test]
    fn not_proven_reason_names_proven_as_an_impostor() {
        let path = Path::new("daemon.json");
        assert_eq!(
            not_proven_reason(4242, path, Proof::Proven),
            not_proven_reason(4242, path, Proof::Unproven)
        );
    }

    #[tokio::test]
    async fn pair_sends_nothing_to_a_listener_without_the_proof() {
        let scratch = ScratchDir::new("cli-pair-unproven");
        let (port, received) = unproven_listener();
        let path = write_handshake(scratch.path(), port, "test-token");

        let err = run_with(&path, &mut Vec::new(), HANG_GUARD)
            .await
            .expect_err("an unproven listener gets no pairing exchange");

        assert_eq!(
            err.to_string(),
            format!(
                "the listener on 127.0.0.1:{port} named by {} did not prove it is \
                 rustling-tulipd; nothing was sent",
                path.display()
            )
        );
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
