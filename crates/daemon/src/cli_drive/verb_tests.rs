//! Socket tests of the session-driving verbs: the real loopback router over a
//! test hub, the only wall-clock bound being the hang guard.
//!
//! A test hub has no tracer to start a session with and always welcomes with
//! its features, so what it cannot say comes from [`scripted_daemon`].

#![expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]

use super::tests::{HANG_GUARD, handshake_file, run_verb, run_verb_into};
use super::*;
use crate::history::test_support::{insert_live, record, recording_pty};
use crate::screen::Screen;
use crate::server::test_support::{ScratchDir, serve_loopback, test_hub};
use futures::{SinkExt as _, StreamExt as _};
use protocol::{SessionMode, SessionStatus};
use std::sync::{Arc, Mutex};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

fn welcome(features: &[&str]) -> DaemonMessage {
    DaemonMessage::Welcome {
        protocol_version: 1,
        supported_versions: vec![1],
        features: features.iter().map(|&feature| feature.to_owned()).collect(),
        daemon_id: None,
    }
}

fn frame(message: &DaemonMessage) -> Message {
    Message::text(serde_json::to_string(message).expect("serialize a daemon message"))
}

/// A listener that plays a daemon on one connection: it sends `opening` after
/// the `Hello`, then `answer` after the next client message, and hands back
/// every message it read. The `/proof` the client asks for goes to the hub on
/// `hub_port`, which holds the handshake's token.
async fn scripted_daemon(
    hub_port: u16,
    opening: Vec<DaemonMessage>,
    answer: Option<DaemonMessage>,
) -> (u16, tokio::task::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind the scripted daemon");
    let port = listener.local_addr().expect("the bound address").port();
    let script = tokio::spawn(async move {
        let (client, _) = listener.accept().await.expect("the client's connection");
        let (mut proof, _) = listener.accept().await.expect("the proof's connection");
        tokio::spawn(async move {
            let mut hub = TcpStream::connect(("127.0.0.1", hub_port))
                .await
                .expect("reach the hub for the proof");
            let _ = tokio::io::copy_bidirectional(&mut proof, &mut hub).await;
        });
        let mut socket = tokio_tungstenite::accept_async(client)
            .await
            .expect("upgrade the client");
        let mut read = Vec::new();
        let hello = socket.next().await.expect("a Hello").expect("a frame");
        read.push(hello.to_string());
        for message in &opening {
            socket.send(frame(message)).await.expect("send the opening");
        }
        if let Some(Ok(Message::Text(text))) = socket.next().await {
            read.push(text.to_string());
            if let Some(answer) = &answer {
                socket.send(frame(answer)).await.expect("send the answer");
            }
            while let Some(Ok(message)) = socket.next().await {
                if message.is_close() {
                    break;
                }
            }
        }
        read
    });
    (port, script)
}

/// Writes each write to `lines`, so a test reads a verb's output as it comes.
struct LineSink(mpsc::UnboundedSender<String>);

impl std::io::Write for LineSink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let text = String::from_utf8_lossy(bytes).into_owned();
        self.0.send(text).map_err(std::io::Error::other)?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// `wait` on session `s1` for `limit`.
async fn wait_limited(
    connection: &mut Connection,
    welcome: &Welcome,
    until: &args::Until,
    limit: Duration,
) -> Result<String, Failure> {
    let request = verbs::WaitRequest {
        session_id: "s1",
        until,
        limit,
        json: false,
    };
    verbs::wait(connection, welcome, &request).await
}

#[tokio::test]
async fn spawn_prints_the_new_session() {
    let (hub, scratch) = test_hub("cli-drive-spawn");
    let hub_port = serve_loopback(&hub).await;
    let opening = vec![
        welcome(&[]),
        DaemonMessage::Sessions {
            sessions: Vec::new(),
        },
    ];
    let reply = DaemonMessage::SessionUpdated {
        session: record("new-1", SessionMode::PlainShell).snapshot(),
        request_id: Some(REQUEST_ID.to_owned()),
    };
    let (port, script) = scripted_daemon(hub_port, opening.clone(), Some(reply.clone())).await;
    let cwd = scratch.path().to_string_lossy().into_owned();

    let text = run_verb(
        &handshake_file(&scratch, port),
        &["spawn", "--cwd", &cwd, "--label", "mine"],
    )
    .await;

    assert_eq!(text.exit, EXIT_OK, "{}", text.err);
    assert_eq!(text.out, "new-1\n");
    let read = script.await.expect("the script finished");
    let spawn: serde_json::Value = serde_json::from_str(&read[1]).expect("valid JSON");
    assert_eq!(spawn["type"], "spawn_session", "{spawn}");
    assert_eq!(spawn["mode"], "plain_shell", "{spawn}");
    assert_eq!(spawn["label"], "mine", "{spawn}");
    assert_eq!(spawn["agent"], serde_json::Value::Null, "{spawn}");
    assert_eq!(spawn["target"]["kind"], "standalone", "{spawn}");
    assert_eq!(spawn["target"]["cwd"], cwd.as_str(), "{spawn}");

    let (port, _script) = scripted_daemon(hub_port, opening, Some(reply)).await;
    let json = run_verb(
        &handshake_file(&scratch, port),
        &["spawn", "--cwd", &cwd, "--json"],
    )
    .await;
    let value: serde_json::Value = serde_json::from_str(&json.out).expect("valid JSON");
    assert_eq!(
        value,
        serde_json::json!({"id": "new-1", "label": "new-1", "status": "idle"})
    );
}

#[tokio::test]
async fn spawn_refuses_an_unknown_agent() {
    let (hub, scratch) = test_hub("cli-drive-spawn-agent");
    let port = serve_loopback(&hub).await;
    let file = handshake_file(&scratch, port);

    let json = run_verb(
        &file,
        &["spawn", "--agent", "ghost", "--cwd", ".", "--json"],
    )
    .await;

    assert_eq!(json.exit, EXIT_FAILED);
    let value: serde_json::Value = serde_json::from_str(&json.out).expect("valid JSON");
    assert_eq!(value["error"]["code"], protocol::error_codes::NOT_FOUND);
    assert_eq!(value["error"]["message"], "unknown agent: ghost");
    assert_eq!(hub.sessions.snapshots().len(), 0);
}

#[tokio::test]
async fn send_reaches_the_barrier() {
    let (hub, scratch) = test_hub("cli-drive-send");
    let (pty, mut input, _exit) = recording_pty();
    insert_live(
        &hub.sessions,
        &hub.dirs,
        record("s1", SessionMode::PlainShell),
        &pty,
    );
    let port = serve_loopback(&hub).await;
    let file = handshake_file(&scratch, port);

    let sent = run_verb(&file, &["send", "s1", "--", "--echo"]).await;

    assert_eq!(sent.exit, EXIT_OK, "{}", sent.err);
    assert_eq!(sent.out, "");
    let typed = input.try_recv().expect("the daemon took the input first");
    assert_eq!(typed, b"--echo\r");

    let keys = run_verb(&file, &["send", "s1", "--keys", "<C-c>", "--json"]).await;
    assert_eq!(keys.out, "{\"id\":\"s1\"}\n");
    let typed = input.try_recv().expect("the daemon took the keys first");
    assert_eq!(typed, [3]);

    let unknown = run_verb(&file, &["send", "ghost", "text"]).await;
    assert_eq!(unknown.exit, EXIT_FAILED);
    assert_eq!(
        unknown.err,
        "rustling-tulipd send: unknown session: ghost\n"
    );
}

#[tokio::test]
async fn kill_stops_a_session() {
    let (hub, scratch) = test_hub("cli-drive-kill");
    hub.sessions
        .insert_pending(record("s1", SessionMode::PlainShell))
        .publish_from(None);
    let port = serve_loopback(&hub).await;
    let file = handshake_file(&scratch, port);

    let killed = run_verb(&file, &["kill", "s1"]).await;

    assert_eq!(killed.exit, EXIT_OK, "{}", killed.err);
    assert_eq!(killed.out, "");
    let sessions = hub.sessions.snapshots();
    assert_eq!(
        sessions[0].status,
        SessionStatus::Stopped,
        "the record stays"
    );

    let again = run_verb(&file, &["kill", "s1", "--json"]).await;
    assert_eq!(again.exit, EXIT_OK, "{}", again.out);
    assert_eq!(again.out, "{\"id\":\"s1\"}\n");

    let unknown = run_verb(&file, &["kill", "ghost"]).await;
    assert_eq!(unknown.exit, EXIT_FAILED);
    assert_eq!(
        unknown.err,
        "rustling-tulipd kill: unknown session: ghost\n"
    );
}

#[tokio::test]
async fn wait_ends_when_the_hub_changes_status() {
    let (hub, scratch) = test_hub("cli-drive-wait");
    hub.sessions
        .insert_pending(record("s1", SessionMode::PlainShell))
        .publish_from(None);
    let port = serve_loopback(&hub).await;
    let file = handshake_file(&scratch, port);
    let mut clients = hub.client_count.subscribe();

    let change = async {
        clients
            .wait_for(|count| *count > 0)
            .await
            .expect("the hub lives on");
        hub.sessions
            .update("s1", |session| session.status = SessionStatus::Working);
    };
    let (waited, ()) = tokio::join!(
        run_verb(&file, &["wait", "s1", "--status", "working", "--json"]),
        change
    );

    assert_eq!(waited.exit, EXIT_OK, "{}", waited.out);
    assert_eq!(waited.out, "{\"id\":\"s1\",\"status\":\"working\"}\n");
}

#[tokio::test]
async fn watch_streams_a_status_change() {
    let (hub, scratch) = test_hub("cli-drive-watch");
    hub.sessions
        .insert_pending(record("s1", SessionMode::PlainShell))
        .publish_from(None);
    let port = serve_loopback(&hub).await;
    let file = handshake_file(&scratch, port);
    let (lines_tx, mut lines) = mpsc::unbounded_channel::<String>();

    let change = async {
        let first = lines.recv().await.expect("the current state");
        assert!(first.contains(" idle - seq="), "{first:?}");
        hub.sessions
            .update("s1", |session| session.status = SessionStatus::Working);
        let second = lines.recv().await.expect("the change");
        assert!(second.contains(" working - seq="), "{second:?}");
        hub.sessions.remove("s1");
    };
    let (watched, ()) = tokio::time::timeout(HANG_GUARD, async {
        tokio::join!(
            run_verb_into(&file, &["watch", "s1"], LineSink(lines_tx)),
            change
        )
    })
    .await
    .expect("hung: the watch did not end");

    assert_eq!(watched.exit, EXIT_OK, "{}", watched.err);
    assert_eq!(watched.out, "");
}

#[tokio::test]
async fn wait_times_out_with_exit_four() {
    let (hub, scratch) = test_hub("cli-drive-wait-timeout");
    hub.sessions
        .insert_pending(record("s1", SessionMode::PlainShell))
        .publish_from(None);
    let port = serve_loopback(&hub).await;
    let file = handshake_file(&scratch, port);
    let (mut connection, welcome) = Connection::open(&file, "test", HANG_GUARD)
        .await
        .expect("connect to the hub");
    tokio::time::pause();

    let until = args::Until::Status(SessionStatus::Working);
    let failure = wait_limited(&mut connection, &welcome, &until, Duration::from_secs(1))
        .await
        .expect_err("the status never comes");

    assert_eq!(failure.exit, EXIT_TIMED_OUT);
    assert_eq!(failure.code, CODE_TIMED_OUT);
}

#[tokio::test]
async fn a_usage_error_exits_two() {
    let scratch = ScratchDir::new("cli-drive-usage");
    let file = scratch.path().join("daemon.json");

    let text = run_verb(&file, &["spawn"]).await;
    assert_eq!(text.exit, EXIT_USAGE);
    assert_eq!(text.out, "");
    assert!(
        text.err
            .starts_with("rustling-tulipd spawn: give one of --repo and --cwd; usage: "),
        "{:?}",
        text.err
    );

    let json = run_verb(&file, &["wait", "s1", "--status", "busy", "--json"]).await;
    assert_eq!(json.exit, EXIT_USAGE);
    let value: serde_json::Value = serde_json::from_str(&json.out).expect("valid JSON");
    assert_eq!(value["error"]["code"], CODE_USAGE);
}

#[tokio::test]
async fn read_prints_the_screen() {
    let (hub, scratch) = test_hub("cli-drive-read");
    let mut session = record("s1", SessionMode::PlainShell);
    let mut screen = Screen::new(20, 3);
    screen.feed(b"hello");
    session.screen = Some(Arc::new(Mutex::new(screen)));
    hub.sessions.insert_pending(session).publish_from(None);
    let port = serve_loopback(&hub).await;
    let file = handshake_file(&scratch, port);

    let text = run_verb(&file, &["read", "s1"]).await;
    assert_eq!(text.exit, EXIT_OK, "{}", text.err);
    assert!(text.out.starts_with("hello\n"), "{:?}", text.out);

    let json = run_verb(&file, &["read", "s1", "--json"]).await;
    let value: serde_json::Value = serde_json::from_str(&json.out).expect("valid JSON");
    assert_eq!(value["cols"], 20, "{value}");
    assert_eq!(value["lines"][0], "hello", "{value}");
}

#[tokio::test]
async fn read_refuses_a_daemon_without_the_screen_dump() {
    let (hub, scratch) = test_hub("cli-drive-read-older");
    let hub_port = serve_loopback(&hub).await;
    let (port, script) = scripted_daemon(hub_port, vec![welcome(&[])], None).await;

    let outcome = run_verb(&handshake_file(&scratch, port), &["read", "s1"]).await;

    assert_eq!(outcome.exit, EXIT_FAILED);
    assert_eq!(outcome.out, "");
    assert!(
        outcome.err.starts_with("rustling-tulipd read: "),
        "{:?}",
        outcome.err
    );
    assert!(
        outcome.err.contains("cannot dump a screen"),
        "{:?}",
        outcome.err
    );
    let read = script.await.expect("the script finished");
    assert_eq!(read.len(), 1, "nothing follows the Hello: {read:?}");
}

#[tokio::test]
async fn send_to_a_stopped_session_fails_ended() {
    let (hub, scratch) = test_hub("cli-drive-send-stopped");
    let mut session = record("s1", SessionMode::PlainShell);
    session.status = SessionStatus::Stopped;
    hub.sessions.insert_pending(session).publish_from(None);
    let port = serve_loopback(&hub).await;
    let file = handshake_file(&scratch, port);

    for list in [
        &["send", "s1", "text", "--json"][..],
        &["send", "s1", "text", "--raw", "--json"][..],
        &["send", "s1", "--keys", "<Enter>", "--json"][..],
    ] {
        let outcome = run_verb(&file, list).await;
        assert_eq!(outcome.exit, EXIT_FAILED, "{list:?}");
        let value: serde_json::Value = serde_json::from_str(&outcome.out).expect("valid JSON");
        assert_eq!(value["error"]["code"], CODE_ENDED, "{list:?}");
    }
}

#[tokio::test]
async fn wait_status_fails_when_the_session_stops() {
    let (hub, scratch) = test_hub("cli-drive-wait-stops");
    hub.sessions
        .insert_pending(record("s1", SessionMode::PlainShell))
        .publish_from(None);
    let port = serve_loopback(&hub).await;
    let file = handshake_file(&scratch, port);
    let mut clients = hub.client_count.subscribe();

    let change = async {
        clients
            .wait_for(|count| *count > 0)
            .await
            .expect("the hub lives on");
        hub.sessions
            .update("s1", |session| session.status = SessionStatus::Stopped);
    };
    let (waited, ()) = tokio::join!(
        run_verb(&file, &["wait", "s1", "--status", "working", "--json"]),
        change
    );

    assert_eq!(waited.exit, EXIT_FAILED, "{}", waited.out);
    let value: serde_json::Value = serde_json::from_str(&waited.out).expect("valid JSON");
    assert_eq!(value["error"]["code"], CODE_ENDED);
}

#[tokio::test]
async fn wait_text_matches_a_final_screen_then_fails_ended() {
    let (hub, scratch) = test_hub("cli-drive-wait-final");
    let mut session = record("s1", SessionMode::PlainShell);
    session.status = SessionStatus::Stopped;
    let mut screen = Screen::new(20, 3);
    screen.feed(b"finished");
    session.screen = Some(Arc::new(Mutex::new(screen)));
    hub.sessions.insert_pending(session).publish_from(None);
    let port = serve_loopback(&hub).await;
    let file = handshake_file(&scratch, port);

    let matched = run_verb(&file, &["wait", "s1", "--text", "finished"]).await;
    assert_eq!(matched.exit, EXIT_OK, "{}", matched.err);
    assert_eq!(matched.out, "finished\n");

    let absent = run_verb(&file, &["wait", "s1", "--text", "absent", "--json"]).await;
    assert_eq!(absent.exit, EXIT_FAILED, "{}", absent.out);
    let value: serde_json::Value = serde_json::from_str(&absent.out).expect("valid JSON");
    assert_eq!(value["error"]["code"], CODE_ENDED);
}

#[tokio::test]
async fn a_quiet_screen_is_read_again_on_the_tick() {
    let (hub, scratch) = test_hub("cli-drive-wait-tick");
    let mut session = record("s1", SessionMode::PlainShell);
    let screen = Arc::new(Mutex::new(Screen::new(20, 3)));
    session.screen = Some(Arc::clone(&screen));
    hub.sessions.insert_pending(session).publish_from(None);
    let port = serve_loopback(&hub).await;
    let file = handshake_file(&scratch, port);
    let (mut connection, welcome) = Connection::open(&file, "test", HANG_GUARD)
        .await
        .expect("connect to the hub");
    tokio::time::pause();
    let until = args::Until::Text(regex::Regex::new("quiet").expect("a valid regex"));

    let change = async {
        tokio::time::sleep(Duration::from_secs(60)).await;
        crate::sync::lock(&screen).feed(b"quiet");
    };
    let limit = Duration::from_hours(24);
    let (found, ()) = tokio::join!(
        wait_limited(&mut connection, &welcome, &until, limit),
        change
    );

    assert_eq!(found.expect("the tick reads the screen"), "quiet\n");
}
