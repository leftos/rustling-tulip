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
    Agent, ClientMessage, DaemonMessage, RecoverAs, RecoverItem, RecoverItemResult, SessionEnd,
    SessionHistoryItem, SessionKind, SessionMode, SessionSnapshot,
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
/// The branch the fixture repo switches to after its Claude session ends.
const MOVED_ON: &str = "moved-on";
/// The Codex thread id `fake-claude`'s codex mode writes into its rollout,
/// and so the id the daemon records for the session.
const CODEX_THREAD_ID: &str = "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b";
/// The Cursor chat id `fake-claude`'s cursor mode prints for `create-chat`.
const CURSOR_CHAT_ID: &str = "3f1c2b9e-7a4d-4e8f-9b2a-6c5d4e3f2a1b";
/// A key the daemon seals by its name: it has a `KEY` segment.
const SECRET_KEY: &str = "RT_TEST_API_KEY";
/// The service the daemon files secrets under; an isolated daemon's carries a
/// `:<hash>` suffix after it.
const SECRET_SERVICE: &str = "rustling-tulip";
/// How many hex characters of the config dir's hash an isolated service
/// carries, as `crates/daemon/src/env_secrets.rs` cuts it.
const SERVICE_HASH_LEN: usize = 8;

/// Asserts the shim's runtime is on `PATH`: `fake-claude` runs under `node`.
fn require_node() {
    let node = Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success());
    assert!(node, "node not on PATH; fake-claude needs it");
}

/// Runs `git -C repo <args>`, asserting it succeeded, and returns its
/// trimmed stdout.
fn git_out(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

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

    // The repo moves on after the session ended: recovery must run on the
    // branch it has now, not check the recorded `main` out again.
    let repo = Path::new(cwd);
    git_out(repo, &["switch", "-q", "-c", MOVED_ON]);
    let status_before = git_out(repo, &["status", "--porcelain"]);

    let recover_item = RecoverItem {
        history_id: session.id.clone(),
        conversation_id: Some(conversation.clone()),
        how: RecoverAs::Claude,
    };
    let results = recover(h, client, "recover-claude", recover_item.clone());
    let new_id = recovered_session(&results, &session.id);
    let recovered = wait_snapshot(h, client, "the recovered claude's snapshot", |s| {
        s.id == new_id
    });
    assert_eq!(recovered.kind, SessionKind::Single, "{recovered:?}");
    let member = recovered
        .members
        .first()
        .expect("a single session has a member");
    assert_eq!(member.repo_id, repo_id, "recovered under the fixture repo");
    assert_eq!(
        member.branch, MOVED_ON,
        "labelled with the branch it runs on"
    );
    assert!(
        recovered.label.contains(MOVED_ON),
        "the label names the branch it runs on: {}",
        recovered.label
    );
    assert_eq!(git_out(repo, &["branch", "--show-current"]), MOVED_ON);
    assert_eq!(git_out(repo, &["status", "--porcelain"]), status_before);
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

/// An interactive spawn of the agent `agent_options` selects, in place in
/// `repo_id`'s checkout, with `extra_env` rows on it.
fn spawn_agent_in_place(
    repo_id: &str,
    agent_options: &Value,
    extra_env: &[(&str, &str)],
) -> ClientMessage {
    serde_json::from_value(json!({
        "type": "spawn_session",
        "label": null,
        "target": {
            "kind": "single",
            "repo_id": repo_id,
            "branch_name": "main",
            "base_branch": null,
            "use_worktree": false,
        },
        "mode": "interactive",
        "initial_prompt": null,
        "dangerously_skip_permissions": false,
        "agent_options": agent_options,
        "model": null,
        "extra_env": extra_env,
    }))
    .expect("spawn request fixture")
}

/// Types `line` into `session_id` as the client would, with a carriage return
/// that ends it.
fn type_line(client: &LiveClient, session_id: &str, line: &str) {
    client.send(ClientMessage::SendInput {
        session_id: session_id.to_owned(),
        data_b64: base64::engine::general_purpose::STANDARD.encode(format!("{line}\r")),
    });
}

/// The conversation id the daemon recorded for `session_id`, from its own
/// sidecar.
fn sidecar_conversation_id(daemon: &LiveDaemon, session_id: &str) -> Option<String> {
    let text = std::fs::read_to_string(
        daemon
            .config_dir()
            .join("sessions")
            .join(session_id)
            .join("meta.json"),
    )
    .ok()?;
    serde_json::from_str::<Value>(&text)
        .ok()?
        .get("agent_conversation_id")?
        .as_str()
        .map(str::to_owned)
}

/// A Codex session writes a rollout, loses its tracer, and recovers as Codex
/// resuming the thread the daemon read out of that rollout.
fn recover_codex_session(
    h: &mut Harness<'_>,
    client: &LiveClient,
    daemon: &LiveDaemon,
    repo_id: &str,
) {
    let codex_home = daemon.dir().join("codex-home");
    std::fs::create_dir_all(&codex_home).expect("create the codex home");
    let codex_home = codex_home.to_string_lossy().into_owned();
    client.send(spawn_agent_in_place(
        repo_id,
        &json!({ "kind": "codex" }),
        &[("CODEX_HOME", codex_home.as_str()), ("FAKE_AGENT", "codex")],
    ));
    let session = wait_snapshot(h, client, "the codex session's snapshot", |s| {
        s.agent == Agent::Codex && s.mode == SessionMode::Interactive
    });
    wait_scrollback_contains(h, client, &session.id, "[fake-codex] ready");

    // Codex writes its thread's rollout on the first message; the daemon's
    // watcher reads the thread id out of it while the session runs.
    type_line(client, &session.id, "hello");
    h.wait_until("the daemon to record the rollout id", ENDED, |_| {
        sidecar_conversation_id(daemon, &session.id).as_deref() == Some(CODEX_THREAD_ID)
    });

    kill_session_tracer(h, daemon, &session.id);
    let entry = wait_tracer_lost(h, daemon, &session.id);
    assert_eq!(
        entry["agent_conversation_id"].as_str(),
        Some(CODEX_THREAD_ID),
        "the entry carries the rollout's thread id: {entry}"
    );
    let item = list_history(h, client, "history-codex", &session.id);
    assert!(
        item.candidates.is_empty(),
        "a Codex entry is offered no Claude conversation: {item:?}"
    );
    assert!(item.own_agent_resumable, "its rollout is on disk");

    let results = recover(
        h,
        client,
        "recover-codex",
        RecoverItem {
            history_id: session.id.clone(),
            conversation_id: Some(CODEX_THREAD_ID.to_owned()),
            how: RecoverAs::OwnAgent,
        },
    );
    let new_id = recovered_session(&results, &session.id);
    let recovered = wait_snapshot(h, client, "the recovered codex's snapshot", |s| {
        s.id == new_id
    });
    assert_eq!(recovered.agent, Agent::Codex, "{recovered:?}");
    wait_scrollback_contains(h, client, &new_id, &format!("RT_RESUMED {CODEX_THREAD_ID}"));
}

/// A Cursor session loses its tracer and recovers as Cursor resuming the chat
/// the daemon pre-created for it.
fn recover_cursor_session(
    h: &mut Harness<'_>,
    client: &LiveClient,
    daemon: &LiveDaemon,
    repo_id: &str,
) {
    client.send(spawn_agent_in_place(
        repo_id,
        &json!({ "kind": "cursor" }),
        &[("FAKE_AGENT", "cursor")],
    ));
    let session = wait_snapshot(h, client, "the cursor session's snapshot", |s| {
        s.agent == Agent::Cursor && s.mode == SessionMode::Interactive
    });
    h.wait_until("the pre-created chat on the session", ENDED, |_| {
        sidecar_conversation_id(daemon, &session.id).as_deref() == Some(CURSOR_CHAT_ID)
    });
    // The shim's banner: the PTY child is up, so the tracer is settled before
    // it is killed.
    wait_scrollback_contains(h, client, &session.id, "[fake-cursor] ready");

    kill_session_tracer(h, daemon, &session.id);
    let entry = wait_tracer_lost(h, daemon, &session.id);
    assert_eq!(
        entry["agent_conversation_id"].as_str(),
        Some(CURSOR_CHAT_ID),
        "the entry carries the chat the daemon pre-created: {entry}"
    );
    let item = list_history(h, client, "history-cursor", &session.id);
    assert!(
        item.candidates.is_empty(),
        "a Cursor entry is offered no Claude conversation: {item:?}"
    );
    assert!(item.own_agent_resumable, "a recorded chat is resumable");

    let results = recover(
        h,
        client,
        "recover-cursor",
        RecoverItem {
            history_id: session.id.clone(),
            conversation_id: Some(CURSOR_CHAT_ID.to_owned()),
            how: RecoverAs::OwnAgent,
        },
    );
    let new_id = recovered_session(&results, &session.id);
    let recovered = wait_snapshot(h, client, "the recovered cursor's snapshot", |s| {
        s.id == new_id
    });
    assert_eq!(recovered.agent, Agent::Cursor, "{recovered:?}");
    wait_scrollback_contains(h, client, &new_id, &format!("RT_RESUMED {CURSOR_CHAT_ID}"));
}

/// The lowercase hex SHA-256 of `bytes`.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    use std::fmt::Write as _;
    let mut hex = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// The Credential Manager service `daemon` files its secrets under:
/// `rustling-tulip:` and the first hex of the SHA-256 of its config dir, made
/// absolute as the daemon's `paths::config_dir` makes it.
fn isolated_service(daemon: &LiveDaemon) -> String {
    let config = std::path::absolute(daemon.config_dir()).expect("the config dir made absolute");
    let mut hex = sha256_hex(config.to_string_lossy().as_bytes());
    hex.truncate(SERVICE_HASH_LEN);
    format!("{SECRET_SERVICE}:{hex}")
}

/// The generic credential `windows-native-keyring-store`'s default store files
/// (`service`, `user`) under: `{user}.{service}`, the daemon's user being
/// `env/<key>/<id>`.
fn credential_target(service: &str, key: &str, id: &str) -> String {
    format!("env/{key}/{id}.{service}")
}

/// The (key, id) of every value the daemon under `config` indexed as sealed.
fn indexed_secrets(config: &Path) -> Vec<(String, String)> {
    let Ok(text) = std::fs::read_to_string(config.join("env-secrets.json")) else {
        return Vec::new();
    };
    let Ok(Value::Array(entries)) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| {
            let key = entry["key"].as_str()?;
            let id = entry["id"].as_str()?;
            Some((key.to_owned(), id.to_owned()))
        })
        .collect()
}

/// Whether Credential Manager lists a credential named `target`.
fn credential_listed(target: &str) -> bool {
    Command::new("cmdkey")
        .arg(format!("/list:{target}"))
        .output()
        .is_ok_and(|out| {
            String::from_utf8_lossy(&out.stdout).contains(&format!("Target: {target}"))
        })
}

/// Every generic credential target Credential Manager lists whose service part
/// is exactly `service`: the target ends in `.{service}`.
fn targets_of_service(service: &str) -> Vec<String> {
    let Ok(out) = Command::new("cmdkey").arg("/list").output() else {
        return Vec::new();
    };
    let suffix = format!(".{service}");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.trim().strip_prefix("Target: "))
        .map(|target| {
            target
                .strip_prefix("LegacyGeneric:target=")
                .unwrap_or(target)
        })
        .filter(|target| target.ends_with(&suffix))
        .map(str::to_owned)
        .collect()
}

/// Deletes, on drop, every Credential Manager entry filed under this run's
/// isolated service, so a failed spec leaves none behind. A target it cannot
/// delete fails the spec, or, when the spec is already failing, is named in
/// `leftover-credentials.txt` in the run's kept dir.
struct SealedSecretsCleanup {
    root: PathBuf,
    service: String,
}

impl SealedSecretsCleanup {
    fn new(daemon: &LiveDaemon) -> Self {
        let isolated = daemon
            .envs()
            .iter()
            .any(|(key, value)| *key == "RUSTLING_TULIP_CONFIG_DIR" && !value.is_empty());
        assert!(
            isolated,
            "the live daemon runs without RUSTLING_TULIP_CONFIG_DIR, so it files secrets \
             under the user's own service; this spec never touches that store"
        );
        Self {
            root: daemon.dir().to_path_buf(),
            service: isolated_service(daemon),
        }
    }
}

impl Drop for SealedSecretsCleanup {
    fn drop(&mut self) {
        for target in targets_of_service(&self.service) {
            // A failure shows as the target still listed below.
            let _ = Command::new("cmdkey")
                .arg(format!("/delete:{target}"))
                .output();
        }
        let leftovers = targets_of_service(&self.service);
        if leftovers.is_empty() {
            return;
        }
        if std::thread::panicking() {
            let mut names = leftovers.join("\n");
            names.push('\n');
            let _ = std::fs::write(self.root.join("leftover-credentials.txt"), names);
        } else {
            assert!(
                leftovers.is_empty(),
                "the teardown could not delete these test credentials: {leftovers:?}"
            );
        }
    }
}

/// How many times a file the daemon is rewriting is read before the walk
/// gives up on it.
const READ_ATTEMPTS: u32 = 5;

/// Reads `path`, retrying a `PermissionDenied` (the daemon replacing the file)
/// up to [`READ_ATTEMPTS`] times, 100 ms apart. `None` when the file is gone.
fn read_settled(path: &Path) -> Option<Vec<u8>> {
    let mut attempt = 1;
    let read = loop {
        match std::fs::read(path) {
            Err(err)
                if err.kind() == std::io::ErrorKind::PermissionDenied
                    && attempt < READ_ATTEMPTS =>
            {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(100));
            }
            read => break read,
        }
    };
    match read {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        read => Some(
            read.map_err(|err| format!("{}: {err}", path.display()))
                .expect("read every file under the config dir"),
        ),
    }
}

/// Asserts no file under `dir` holds `needle`, naming the first that does.
/// `daemon.lock` is skipped: the running daemon holds it locked, and it carries
/// no spawn data. A file or folder the daemon removed mid-walk is skipped.
fn assert_no_file_holds(dir: &Path, needle: &[u8]) {
    let mut pending = vec![dir.to_path_buf()];
    while let Some(folder) = pending.pop() {
        let entries = match std::fs::read_dir(&folder) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            listed => listed
                .map_err(|err| format!("{}: {err}", folder.display()))
                .expect("list a folder under the config dir"),
        };
        for entry in entries {
            let path = entry.expect("read a config dir entry").path();
            if path.is_dir() {
                pending.push(path);
                continue;
            }
            if path.file_name().is_some_and(|name| name == "daemon.lock") {
                continue;
            }
            let Some(bytes) = read_settled(&path) else {
                continue;
            };
            assert!(
                !bytes.windows(needle.len()).any(|window| window == needle),
                "{} holds the sealed secret's value",
                path.display()
            );
        }
    }
}

/// A Claude session spawned with a secret row sees its value, loses its
/// tracer and, recovered as Claude, sees it again. Returns the ended session's
/// id, which names its history entry.
fn recover_sealed_session(
    h: &mut Harness<'_>,
    client: &LiveClient,
    daemon: &LiveDaemon,
    repo_id: &str,
    sentinel: &str,
) -> String {
    let hash_line = format!(
        "RT_ENV {SECRET_KEY} sha256={}",
        sha256_hex(sentinel.as_bytes())
    );
    let ask = format!("/env {SECRET_KEY}");
    client.send(spawn_agent_in_place(
        repo_id,
        &json!({ "kind": "claude" }),
        &[(SECRET_KEY, sentinel)],
    ));
    let session = wait_snapshot(h, client, "the claude session's snapshot", |s| {
        s.mode == SessionMode::Interactive
    });
    let conversation = session
        .claude_session_id
        .clone()
        .expect("a spawned claude session carries its conversation id");
    let cwd = &session
        .members
        .first()
        .expect("an in-place session has a member")
        .worktree_path;
    write_transcript(daemon, cwd, &conversation);
    wait_scrollback_contains(h, client, &session.id, "[fake-claude] ready");
    type_line(client, &session.id, &ask);
    wait_scrollback_contains(h, client, &session.id, &hash_line);

    kill_session_tracer(h, daemon, &session.id);
    wait_tracer_lost(h, daemon, &session.id);
    let results = recover(
        h,
        client,
        "recover-sealed",
        RecoverItem {
            history_id: session.id.clone(),
            conversation_id: Some(conversation),
            how: RecoverAs::Claude,
        },
    );
    let new_id = recovered_session(&results, &session.id);
    wait_snapshot(h, client, "the recovered claude's snapshot", |s| {
        s.id == new_id
    });
    wait_scrollback_contains(h, client, &new_id, "[fake-claude] ready");
    type_line(client, &new_id, &ask);
    wait_scrollback_contains(h, client, &new_id, &hash_line);
    session.id
}

/// Asserts the sealed value lives in Credential Manager and in no file under
/// the config dir, whose stored files carry its reference instead.
fn assert_sealed_on_disk(daemon: &LiveDaemon, history_id: &str, sentinel: &str) {
    let config = daemon.config_dir();
    assert_no_file_holds(&config, sentinel.as_bytes());
    assert!(
        config.join("env-secrets.json").is_file(),
        "the daemon indexed the sealed value"
    );
    let entry_path = config.join("history").join(format!("{history_id}.json"));
    let entry = std::fs::read_to_string(&entry_path).expect("read the history entry");
    assert!(
        entry.contains("${secret:"),
        "{} carries a secret reference: {entry}",
        entry_path.display()
    );
    let service = isolated_service(daemon);
    let secrets = indexed_secrets(&config);
    assert!(!secrets.is_empty(), "the index lists the sealed value");
    for (key, id) in &secrets {
        let target = credential_target(&service, key, id);
        assert!(
            credential_listed(&target),
            "Credential Manager lists no {target}: the spec's service hash or target format \
             is wrong"
        );
    }
}

#[gpui::test]
#[ignore = "e2e: run via .\\rt.ps1 native-e2e"]
fn live_a_sealed_secret_reaches_the_child_and_no_file(cx: &mut TestAppContext) {
    require_node();
    let daemon = LiveDaemon::start("sealed-secret");
    let _cleanup = SealedSecretsCleanup::new(&daemon);
    let repo = daemon.git_fixture();
    let (mut h, client) = Harness::open_live(cx, &daemon);
    wait_connected(&mut h, &client, &daemon);

    let repo_id = register_repo(&mut h, &client, &daemon, &repo);
    let sentinel = format!("rt-sentinel-{}", uuid::Uuid::new_v4().simple());
    let history_id = recover_sealed_session(&mut h, &client, &daemon, &repo_id, &sentinel);
    assert_sealed_on_disk(&daemon, &history_id, &sentinel);
}

#[gpui::test]
#[ignore = "e2e: run via .\\rt.ps1 native-e2e"]
fn live_recovers_sessions_whose_tracer_died(cx: &mut TestAppContext) {
    require_node();
    let daemon = LiveDaemon::start("recover");
    let repo = daemon.git_fixture();
    let (mut h, client) = Harness::open_live(cx, &daemon);
    wait_connected(&mut h, &client, &daemon);

    let repo_id = register_repo(&mut h, &client, &daemon, &repo);
    recover_claude_session(&mut h, &client, &daemon, &repo_id);
    recover_shell_session(&mut h, &client, &daemon, &repo);
}

#[gpui::test]
#[ignore = "e2e: run via .\\rt.ps1 native-e2e"]
fn live_recovers_a_codex_session_as_codex(cx: &mut TestAppContext) {
    require_node();
    let daemon = LiveDaemon::start("recover-codex");
    let repo = daemon.git_fixture();
    let (mut h, client) = Harness::open_live(cx, &daemon);
    wait_connected(&mut h, &client, &daemon);

    let repo_id = register_repo(&mut h, &client, &daemon, &repo);
    recover_codex_session(&mut h, &client, &daemon, &repo_id);
}

#[gpui::test]
#[ignore = "e2e: run via .\\rt.ps1 native-e2e"]
fn live_recovers_a_cursor_session_as_cursor(cx: &mut TestAppContext) {
    require_node();
    let daemon = LiveDaemon::start("recover-cursor");
    let repo = daemon.git_fixture();
    let (mut h, client) = Harness::open_live(cx, &daemon);
    wait_connected(&mut h, &client, &daemon);

    let repo_id = register_repo(&mut h, &client, &daemon, &repo);
    recover_cursor_session(&mut h, &client, &daemon, &repo_id);
}
