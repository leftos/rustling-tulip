//! End-to-end spec for session recovery: a real daemon isolated under
//! `.tmp/native-e2e/`, its real tracers and `tools/e2e/fake-claude`. A
//! session's tracer is killed, the daemon records the session as lost, and
//! the history recovers it as Claude resuming its conversation and as a
//! plain shell typing `claude --resume`, which runs the harness's `claude`
//! stub. Ignored by default; run by `.\rt.ps1 native-e2e`.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

#[expect(dead_code, reason = "each spec file uses its own share of the helper")]
mod support;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use base64::Engine as _;
use gpui::TestAppContext;
use protocol::{
    ClientMessage, DaemonMessage, RecoverAs, RecoverItem, RecoverItemResult, SessionEnd,
    SessionHistoryItem, SessionMode, SessionSnapshot,
};
use serde_json::{Value, json};
use support::Harness;
use support::live::{
    CLAUDE_STUB_MARKER, LiveClient, LiveDaemon, kill_tree, processes_under, spawn_claude_in_place,
    spawn_shell,
};

const CONNECT: Duration = Duration::from_secs(20);
const SPAWN: Duration = Duration::from_secs(30);
/// How long the daemon gets to notice a killed tracer and write history.
const ENDED: Duration = Duration::from_secs(30);
/// How long a reply to one request may take.
const REPLY: Duration = Duration::from_secs(10);
/// How long a recovered shell gets to run `claude --resume`: its injector
/// types once the prompt has printed, well inside this.
const ECHO: Duration = Duration::from_secs(10);
/// The conversation the recovered shell resumes.
const SHELL_CONVERSATION: &str = "abc-123";
/// `DSR 6`: the pseudo console asks where the cursor is.
const CURSOR_POSITION_QUERY: &str = "\x1b[6n";
/// The answer to [`CURSOR_POSITION_QUERY`]: row 1, column 1.
const CURSOR_AT_ORIGIN: &[u8] = b"\x1b[1;1R";
/// The line the tracer logs with the program and args it spawns.
const SPAWN_LINE: &str = "about to spawn child";

/// Waits until the view's latest state is open on `daemon`'s port with no
/// connecting overlay.
fn wait_connected(h: &mut Harness<'_>, client: &LiveClient, daemon: &LiveDaemon) {
    let port = daemon.handshake().port;
    h.wait_until("the footer to show the daemon's port", CONNECT, |_| {
        client
            .latest()
            .is_some_and(|(open_port, overlay)| open_port == Some(port) && overlay.is_none())
    });
}

/// The newest snapshot the daemon sent of a session `pick` accepts.
fn snapshot(
    client: &LiveClient,
    pick: impl Fn(&SessionSnapshot) -> bool,
) -> Option<SessionSnapshot> {
    client.find_message(|msg| match msg {
        DaemonMessage::SessionUpdated { session, .. } => pick(session).then(|| session.clone()),
        DaemonMessage::Sessions { sessions } => sessions.iter().find(|s| pick(s)).cloned(),
        _ => None,
    })
}

/// Waits for a snapshot `pick` accepts and returns it.
fn wait_snapshot(
    h: &mut Harness<'_>,
    client: &LiveClient,
    what: &str,
    pick: impl Fn(&SessionSnapshot) -> bool,
) -> SessionSnapshot {
    let mut found = None;
    h.wait_until(what, SPAWN, |_| {
        found = snapshot(client, &pick);
        found.is_some()
    });
    found.expect("the wait just found it")
}

/// The tracer's spawn line for `session_id`, once its log has one.
fn spawn_line(daemon: &LiveDaemon, session_id: &str) -> Option<String> {
    let log = daemon
        .config_dir()
        .join("logs")
        .join(format!("tracer-{session_id}.log"));
    let text = std::fs::read_to_string(log).ok()?;
    text.lines()
        .find(|line| line.contains(SPAWN_LINE))
        .map(str::to_owned)
}

fn wait_spawn_line(h: &mut Harness<'_>, daemon: &LiveDaemon, session_id: &str) -> String {
    let mut line = None;
    h.wait_until("the tracer's spawn line", SPAWN, |_| {
        line = spawn_line(daemon, session_id);
        line.is_some()
    });
    line.expect("the wait just found it")
}

/// The daemon's history entry for `session_id`, as it wrote it.
fn history_entry(daemon: &LiveDaemon, session_id: &str) -> Option<Value> {
    let path = daemon
        .config_dir()
        .join("history")
        .join(format!("{session_id}.json"));
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn wait_tracer_lost(h: &mut Harness<'_>, daemon: &LiveDaemon, session_id: &str) -> Value {
    let mut entry = None;
    h.wait_until("a tracer_lost history entry", ENDED, |_| {
        entry =
            history_entry(daemon, session_id).filter(|entry| entry["end"]["type"] == "tracer_lost");
        entry.is_some()
    });
    entry.expect("the wait just found it")
}

/// The folder the daemon recovers `entry` in: its last reported folder, else
/// the one it started in, else its first member's.
fn entry_folder(entry: &Value) -> String {
    ["current_cwd", "primary_cwd"]
        .iter()
        .find_map(|key| entry[key].as_str())
        .or_else(|| entry["members"][0]["worktree_path"].as_str())
        .filter(|folder| !folder.is_empty())
        .expect("the history entry names a folder")
        .to_owned()
}

/// Kills `session_id`'s tracer, found by the pid its sidecar names among the
/// processes running from this daemon's own binaries dir.
fn kill_session_tracer(h: &mut Harness<'_>, daemon: &LiveDaemon, session_id: &str) {
    let meta = daemon
        .config_dir()
        .join("sessions")
        .join(session_id)
        .join("meta.json");
    let binaries = daemon.binaries_dir();
    let mut target: Option<(u32, PathBuf)> = None;
    h.wait_until(
        "the session's tracer in its sidecar and running",
        SPAWN,
        |_| {
            let pid = std::fs::read_to_string(&meta)
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                .and_then(|meta| meta["tracer_pid"].as_u64())
                .and_then(|pid| u32::try_from(pid).ok());
            target = pid.and_then(|pid| {
                processes_under(&binaries)
                    .into_iter()
                    .find(|(running, _)| *running == pid)
            });
            target.is_some()
        },
    );
    let (pid, image) = target.expect("the wait just found it");
    let name = image
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    assert!(
        image.starts_with(&binaries) && name.starts_with("rt-tracer"),
        "pid {pid} runs {}, not a tracer from {}",
        image.display(),
        binaries.display()
    );
    kill_tree(pid);
}

/// The folder name Claude Code files `cwd`'s transcripts under.
fn encode_project_dir(cwd: &str) -> String {
    cwd.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// A two-line transcript of conversation `id` recorded in `cwd`.
fn write_transcript(daemon: &LiveDaemon, cwd: &str, id: &str) {
    let dir = daemon
        .claude_config_dir()
        .join("projects")
        .join(encode_project_dir(cwd));
    std::fs::create_dir_all(&dir).expect("create the transcript's project folder");
    let head = json!({ "type": "system", "cwd": cwd, "sessionId": id });
    let user = json!({
        "type": "user",
        "cwd": cwd,
        "sessionId": id,
        "message": { "role": "user", "content": "recover this conversation" },
    });
    std::fs::write(dir.join(format!("{id}.jsonl")), format!("{head}\n{user}\n"))
        .expect("write the transcript");
}

/// Asks for the history and returns the item for `session_id`.
fn list_history(
    h: &mut Harness<'_>,
    client: &LiveClient,
    request_id: &str,
    session_id: &str,
) -> SessionHistoryItem {
    client.send(ClientMessage::ListSessionHistory {
        request_id: Some(request_id.to_owned()),
    });
    let mut items = None;
    h.wait_until("the session history reply", REPLY, |_| {
        items = client.find_message(|msg| match msg {
            DaemonMessage::SessionHistory {
                request_id: Some(id),
                items,
            } if id == request_id => Some(items.clone()),
            _ => None,
        });
        items.is_some()
    });
    items
        .expect("the wait just found it")
        .into_iter()
        .find(|item| item.entry.session_id == session_id)
        .expect("the history lists the ended session")
}

/// Sends one recovery and returns the daemon's results for it.
fn recover(
    h: &mut Harness<'_>,
    client: &LiveClient,
    request_id: &str,
    item: RecoverItem,
) -> Vec<RecoverItemResult> {
    client.send(ClientMessage::RecoverSessions {
        request_id: Some(request_id.to_owned()),
        items: vec![item],
    });
    let mut results = None;
    h.wait_until("the recover result", SPAWN, |_| {
        results = client.find_message(|msg| match msg {
            DaemonMessage::RecoverResult {
                request_id: Some(id),
                results,
            } if id == request_id => Some(results.clone()),
            _ => None,
        });
        results.is_some()
    });
    results.expect("the wait just found it")
}

/// The one new session a recovery reports, asserting it succeeded.
fn recovered_session(results: &[RecoverItemResult], history_id: &str) -> String {
    assert_eq!(results.len(), 1, "one result per item: {results:?}");
    let result = &results[0];
    assert_eq!(result.history_id, history_id);
    assert_eq!(result.error, None, "the recovery failed: {result:?}");
    let new_id = result.session_id.clone().expect("a recovered session id");
    assert_ne!(new_id, history_id, "the recovery is a new session");
    new_id
}

/// The session's persisted scrollback, as the daemon replies to one
/// `LoadScrollback`.
fn scrollback(
    h: &mut Harness<'_>,
    client: &LiveClient,
    session_id: &str,
    request_id: &str,
) -> String {
    client.send(ClientMessage::LoadScrollback {
        session_id: session_id.to_owned(),
        request_id: Some(request_id.to_owned()),
    });
    let mut data = None;
    h.wait_until("the scrollback reply", REPLY, |_| {
        data = client.find_message(|msg| match msg {
            DaemonMessage::Scrollback {
                request_id: Some(id),
                data_b64,
                ..
            } if id == request_id => Some(data_b64.clone()),
            _ => None,
        });
        data.is_some()
    });
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.expect("the wait just found it"))
        .expect("scrollback is base64");
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Polls the session's scrollback until it contains `want`, for [`ECHO`].
fn wait_scrollback_contains(
    h: &mut Harness<'_>,
    client: &LiveClient,
    session_id: &str,
    want: &str,
) {
    let deadline = Instant::now() + ECHO;
    let mut answered = false;
    for attempt in 0.. {
        let text = scrollback(h, client, session_id, &format!("scrollback-{attempt}"));
        if text.contains(want) {
            return;
        }
        // ConPTY asks where the cursor is and holds the shell's output until
        // a terminal answers; no pane shows this session, so answer here.
        if !answered && text.contains(CURSOR_POSITION_QUERY) {
            client.send(ClientMessage::SendInput {
                session_id: session_id.to_owned(),
                data_b64: base64::engine::general_purpose::STANDARD.encode(CURSOR_AT_ORIGIN),
            });
            answered = true;
        }
        assert!(
            Instant::now() < deadline,
            "timed out after {ECHO:?} waiting for {want:?} in the scrollback; it holds {text:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Registers `repo` with the daemon and returns its id.
fn register_repo(
    h: &mut Harness<'_>,
    client: &LiveClient,
    daemon: &LiveDaemon,
    repo: &Path,
) -> String {
    client.send(ClientMessage::AddRepo {
        path: repo.to_string_lossy().into_owned(),
        name: None,
    });
    let mut repo_id = None;
    h.wait_until("the fixture repo in state.json", SPAWN, |_| {
        repo_id = daemon.repo_id(repo);
        repo_id.is_some()
    });
    repo_id.expect("the repo was registered")
}

/// Steps 2–7: a Claude session loses its tracer and is recovered as Claude
/// resuming its conversation, once.
fn recover_claude_session(
    h: &mut Harness<'_>,
    client: &LiveClient,
    daemon: &LiveDaemon,
    repo_id: &str,
) {
    client.send(spawn_claude_in_place(repo_id));
    let session = wait_snapshot(h, client, "the claude session's snapshot", |s| {
        s.mode == SessionMode::Interactive
    });
    let conversation = session
        .claude_session_id
        .clone()
        .expect("a spawned claude session carries its conversation id");
    let line = wait_spawn_line(h, daemon, &session.id);
    assert!(
        line.contains(&format!("\"--session-id\", \"{conversation}\"")),
        "the spawn passes --session-id {conversation}: {line}"
    );
    let cwd = &session
        .members
        .first()
        .expect("an in-place session has a member")
        .worktree_path;
    write_transcript(daemon, cwd, &conversation);

    kill_session_tracer(h, daemon, &session.id);
    wait_tracer_lost(h, daemon, &session.id);
    let item = list_history(h, client, "history-claude", &session.id);
    assert_eq!(item.entry.end, SessionEnd::TracerLost);
    let candidates: Vec<&str> = item.candidates.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(
        candidates,
        [conversation.as_str()],
        "the one known conversation"
    );

    let recover_item = RecoverItem {
        history_id: session.id.clone(),
        conversation_id: Some(conversation.clone()),
        how: RecoverAs::Claude,
    };
    let results = recover(h, client, "recover-claude", recover_item.clone());
    let new_id = recovered_session(&results, &session.id);
    let line = wait_spawn_line(h, daemon, &new_id);
    assert!(
        line.contains(&format!("\"--resume\", \"{conversation}\"")),
        "the recovered spawn passes --resume {conversation}: {line}"
    );
    assert!(
        !line.contains("--session-id"),
        "the recovered spawn takes no fresh --session-id: {line}"
    );
    h.wait_until("recovered_at on the history entry", REPLY, |_| {
        history_entry(daemon, &session.id).is_some_and(|entry| entry["recovered_at"].is_string())
    });
    let again = recover(h, client, "recover-claude-again", recover_item);
    assert_eq!(again.len(), 1, "one result per item: {again:?}");
    assert_eq!(again[0].error.as_deref(), Some("already recovered"));
    assert_eq!(again[0].session_id, None);
}

/// Step 8: a plain shell loses its tracer and is recovered as a shell that
/// types `claude --resume <conversation>`.
fn recover_shell_session(
    h: &mut Harness<'_>,
    client: &LiveClient,
    daemon: &LiveDaemon,
    folder: &Path,
) {
    client.send(spawn_shell(folder));
    let session = wait_snapshot(h, client, "the shell's snapshot", |s| {
        s.mode == SessionMode::PlainShell
    });
    kill_session_tracer(h, daemon, &session.id);
    let entry = wait_tracer_lost(h, daemon, &session.id);
    write_transcript(daemon, &entry_folder(&entry), SHELL_CONVERSATION);

    let results = recover(
        h,
        client,
        "recover-shell",
        RecoverItem {
            history_id: session.id.clone(),
            conversation_id: Some(SHELL_CONVERSATION.to_owned()),
            how: RecoverAs::Shell,
        },
    );
    let new_id = recovered_session(&results, &session.id);
    wait_snapshot(h, client, "the recovered shell's snapshot", |s| {
        s.id == new_id && s.mode == SessionMode::PlainShell
    });
    // The stub prints its marker and arguments: the typed command ran, and
    // ran the stub rather than the user's `claude`.
    let ran = format!("{CLAUDE_STUB_MARKER} --resume {SHELL_CONVERSATION}");
    wait_scrollback_contains(h, client, &new_id, &ran);
}

#[gpui::test]
#[ignore = "e2e: run via .\\rt.ps1 native-e2e"]
fn live_recovers_sessions_whose_tracer_died(cx: &mut TestAppContext) {
    let node = Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success());
    assert!(node, "node not on PATH; fake-claude needs it");
    let daemon = LiveDaemon::start("recover");
    let repo = daemon.git_fixture();
    let (mut h, client) = Harness::open_live(cx, &daemon);
    wait_connected(&mut h, &client, &daemon);

    let repo_id = register_repo(&mut h, &client, &daemon, &repo);
    recover_claude_session(&mut h, &client, &daemon, &repo_id);
    recover_shell_session(&mut h, &client, &daemon, &repo);
}
