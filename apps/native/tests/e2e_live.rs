//! End-to-end specs: the root view on the real network thread, against a
//! real daemon isolated under `.tmp/native-e2e/`, with plain shells and
//! `tools/e2e/fake-claude`. They need the daemon and tracer built beside the
//! test binary, so they are ignored by default and run by
//! `.\rt.ps1 native-e2e`.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

#[expect(dead_code, reason = "each spec file uses its own share of the helper")]
mod support;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use gpui::TestAppContext;
use protocol::{
    ClientMessage, DaemonMessage, InitLayoutKind, PendingInput, PendingQuestion, SessionSnapshot,
    SessionStatus,
};
use serde_json::{Value, json};
use support::Harness;
use support::live::{
    LiveClient, LiveDaemon, kill_tree, processes_under, spawn_claude_in_place, spawn_shell,
};

const CONNECT: Duration = Duration::from_secs(20);
const SPAWN: Duration = Duration::from_secs(30);
const ECHO: Duration = Duration::from_secs(15);
/// How long a force-killed process gets to leave the process list.
const EXIT: Duration = Duration::from_secs(5);

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

/// Waits for a session in the sidebar, has the daemon lay out every session
/// for this client, and returns the pane it lands in.
fn place_first_session(h: &mut Harness<'_>, client: &LiveClient) -> String {
    h.wait_until("a session in the sidebar", SPAWN, |h| {
        h.root(|root, _| {
            root.sidebar_containers()
                .iter()
                .any(|c| !c.leaves.is_empty())
        })
    });
    client.send(ClientMessage::InitLayout {
        kind: InitLayoutKind::AllSessions,
    });
    h.wait_until("a pane for the session", SPAWN, |h| {
        h.first_pane().is_some()
    });
    h.first_pane().expect("a pane was just laid out")
}

/// The newest snapshot the daemon sent of a session `pick` accepts.
fn snapshot(
    client: &LiveClient,
    pick: impl Fn(&SessionSnapshot) -> bool,
) -> Option<SessionSnapshot> {
    snapshot_from(client, 0, pick)
}

/// The newest snapshot of a session `pick` accepts among the messages from
/// index `from` on.
fn snapshot_from(
    client: &LiveClient,
    from: usize,
    pick: impl Fn(&SessionSnapshot) -> bool,
) -> Option<SessionSnapshot> {
    client.find_message_from(from, |msg| match msg {
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

/// Waits until a snapshot of `session_id` received at or after message `from`
/// reports `want`, and returns the newest such snapshot. Looking back from
/// the cue's cursor keeps a transient status observable even when a later
/// snapshot has already superseded it.
fn wait_status(
    h: &mut Harness<'_>,
    client: &LiveClient,
    from: usize,
    session_id: &str,
    want: SessionStatus,
) -> SessionSnapshot {
    let mut found = None;
    h.wait_until(
        &format!("session {session_id} to report {want:?}"),
        SPAWN,
        |_| {
            found = snapshot_from(client, from, |s| s.id == session_id && s.status == want);
            found.is_some()
        },
    );
    found.expect("the wait just found it")
}

/// Waits until fake-claude shows its prompt box in `pane`. A Claude session's
/// PTY child is its shell, which the daemon types the agent into, so a cue
/// typed before then reaches the shell.
fn wait_claude_prompt(h: &mut Harness<'_>, pane: &str) {
    h.wait_for_row(pane, "fake-claude's prompt box", SPAWN, |row| {
        row.trim_start().starts_with('\u{276f}')
    });
}

#[gpui::test]
#[ignore = "e2e: run via .\\rt.ps1 native-e2e"]
fn live_connects_and_reports_connected(cx: &mut TestAppContext) {
    let daemon = LiveDaemon::start("connects");
    let (mut h, client) = Harness::open_live(cx, &daemon);

    wait_connected(&mut h, &client, &daemon);

    let labels = client.footer_labels();
    let last = labels.last().expect("the view was sent a state");
    assert_eq!(last, "0 sessions", "footer labels: {labels:?}");
}

#[gpui::test]
#[ignore = "e2e: run via .\\rt.ps1 native-e2e"]
fn live_plain_shell_appears_in_pane_and_echoes_input(cx: &mut TestAppContext) {
    let daemon = LiveDaemon::start("shell");
    let (mut h, client) = Harness::open_live(cx, &daemon);
    wait_connected(&mut h, &client, &daemon);

    client.send(spawn_shell(daemon.dir()));
    let pane = place_first_session(&mut h, &client);
    h.wait_for_row(&pane, "the shell's prompt", SPAWN, |row| {
        row.trim_end().ends_with('>')
    });
    h.type_into(&pane, "echo rt-e2e-marker\n");

    h.wait_for_row(
        &pane,
        "the echoed marker on a line of its own",
        ECHO,
        |row| row.trim() == "rt-e2e-marker",
    );
}

#[gpui::test]
#[ignore = "e2e: run via .\\rt.ps1 native-e2e"]
fn live_fake_claude_streams_ready_banner(cx: &mut TestAppContext) {
    let node = Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success());
    assert!(node, "node not on PATH; fake-claude needs it");
    let daemon = LiveDaemon::start("fake-claude");
    let repo = daemon.git_fixture();
    let (mut h, client) = Harness::open_live(cx, &daemon);
    wait_connected(&mut h, &client, &daemon);

    client.send(ClientMessage::AddRepo {
        path: repo.to_string_lossy().into_owned(),
        name: None,
    });
    let mut repo_id = None;
    h.wait_until("the fixture repo in state.json", SPAWN, |_| {
        repo_id = daemon.repo_id(&repo);
        repo_id.is_some()
    });
    client.send(spawn_claude_in_place(
        &repo_id.expect("the repo was registered"),
    ));
    let pane = place_first_session(&mut h, &client);

    h.wait_for_row(&pane, "fake-claude's ready banner", SPAWN, |row| {
        row.contains("[fake-claude] ready")
    });
}

/// The hook chain end to end: `fake-claude` runs the hook commands the daemon
/// injected through `--settings`, and the session's snapshots follow
/// `Working` → `AwaitingInput` with the question → `Idle`.
#[gpui::test]
#[ignore = "e2e: run via .\\rt.ps1 native-e2e"]
fn live_hook_status_reports_working_asking_and_idle(cx: &mut TestAppContext) {
    let node = Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success());
    assert!(node, "node not on PATH; fake-claude needs it");
    let daemon = LiveDaemon::start("hook-status");
    let repo = daemon.git_fixture();
    let (mut h, client) = Harness::open_live(cx, &daemon);
    wait_connected(&mut h, &client, &daemon);

    client.send(ClientMessage::AddRepo {
        path: repo.to_string_lossy().into_owned(),
        name: None,
    });
    let mut repo_id = None;
    h.wait_until("the fixture repo in state.json", SPAWN, |_| {
        repo_id = daemon.repo_id(&repo);
        repo_id.is_some()
    });
    client.send(spawn_claude_in_place(
        &repo_id.expect("the repo was registered"),
    ));
    let pane = place_first_session(&mut h, &client);

    let session = wait_snapshot(&mut h, &client, "the claude session", |_| true);
    let id = session.id.clone();
    // The daemon writes the injected settings before it spawns the CLI, so a
    // spawn that lost them is named here instead of timing out cue by cue.
    let settings_path = daemon
        .config_dir()
        .join("sessions")
        .join(&id)
        .join("hook-settings.json");
    let text = std::fs::read_to_string(&settings_path)
        .map_err(|err| format!("reading {}: {err}", settings_path.display()))
        .expect("the hook settings file");
    let settings: Value = serde_json::from_str(&text).expect("the hook settings parse");
    let hook = &settings["hooks"]["PreToolUse"][0]["hooks"][0];
    let command = hook["command"].as_str().expect("the hook names a command");
    assert!(
        std::path::Path::new(command).extension().is_none_or(|ext| {
            ext.eq_ignore_ascii_case(std::env::consts::EXE_SUFFIX.trim_start_matches('.'))
        }),
        "the hook command is not the cached tracer: {command}"
    );
    assert_eq!(
        hook["args"],
        json!([
            "hook",
            "--session",
            id,
            "--config-dir",
            daemon.config_dir().to_string_lossy(),
        ]),
        "the hook args do not name this session and config dir"
    );

    // The spawn paints the idle box, which matches the seeded Idle.
    wait_claude_prompt(&mut h, &pane);
    let settled = wait_status(&mut h, &client, 0, &id, SessionStatus::Idle);
    assert_eq!(settled.pending_input, None, "nothing is pending at spawn");

    let from = client.message_count();
    h.type_into(&pane, "/rt-hook prompt\n");
    let working = wait_status(&mut h, &client, from, &id, SessionStatus::Working);
    assert_eq!(
        working.pending_input, None,
        "submitting a prompt leaves nothing pending"
    );
    std::thread::sleep(Duration::from_secs(4));
    let newest = snapshot_from(&client, from, |s| s.id == id).map(|s| s.status);
    assert_eq!(
        newest,
        Some(SessionStatus::Working),
        "a quiet screen showing the spinner left Working"
    );

    let from = client.message_count();
    h.type_into(&pane, "/rt-hook ask\n");
    let asking = wait_status(&mut h, &client, from, &id, SessionStatus::AwaitingInput);
    assert_eq!(
        asking.pending_input,
        Some(PendingInput::Question {
            questions: vec![PendingQuestion {
                question: "Do you prefer tea or coffee?".to_owned(),
                header: "Drink".to_owned(),
                options: vec!["Tea".to_owned(), "Coffee".to_owned()],
                multi_select: false,
            }],
        }),
        "the question the hook reported"
    );

    // The fake runs the Stop hook on its own timer. Between this cue and that
    // Stop the screen shows the spinner and then the question, so the only
    // Idle at or after this cursor is the Stop's, however late the
    // AwaitingInput wait returned.
    let stopped = wait_status(&mut h, &client, from, &id, SessionStatus::Idle);
    assert_eq!(stopped.pending_input, None, "Stop clears the pending input");
}

/// An Esc mid-turn fires no hook: the interrupt row on the screen alone ends
/// the turn, so the session drops from Working to Idle.
#[gpui::test]
#[ignore = "e2e: run via .\\rt.ps1 native-e2e"]
fn live_esc_mid_turn_drops_working_to_idle_from_the_screen(cx: &mut TestAppContext) {
    let node = Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success());
    assert!(node, "node not on PATH; fake-claude needs it");
    let daemon = LiveDaemon::start("esc-interrupt");
    let repo = daemon.git_fixture();
    let (mut h, client) = Harness::open_live(cx, &daemon);
    wait_connected(&mut h, &client, &daemon);

    client.send(ClientMessage::AddRepo {
        path: repo.to_string_lossy().into_owned(),
        name: None,
    });
    let mut repo_id = None;
    h.wait_until("the fixture repo in state.json", SPAWN, |_| {
        repo_id = daemon.repo_id(&repo);
        repo_id.is_some()
    });
    client.send(spawn_claude_in_place(
        &repo_id.expect("the repo was registered"),
    ));
    let pane = place_first_session(&mut h, &client);

    let session = wait_snapshot(&mut h, &client, "the claude session", |_| true);
    let id = session.id.clone();
    wait_claude_prompt(&mut h, &pane);
    let settled = wait_status(&mut h, &client, 0, &id, SessionStatus::Idle);
    assert_eq!(settled.pending_input, None, "nothing is pending at spawn");

    let from = client.message_count();
    h.type_into(&pane, "/rt-hook prompt\n");
    wait_status(&mut h, &client, from, &id, SessionStatus::Working);
    h.wait_for_row(&pane, "the turn's spinner", ECHO, |row| {
        row.contains("Cooking…")
    });
    let from = client.message_count();
    h.keys("escape");
    h.wait_for_row(&pane, "the interrupt row", ECHO, |row| {
        row.contains("Interrupted ·")
    });
    let idle = wait_status(&mut h, &client, from, &id, SessionStatus::Idle);
    assert_eq!(
        idle.pending_input, None,
        "an interrupt leaves nothing pending"
    );
}

/// A turn that ends with a background shell still running: the session is
/// `Idle` with the background flag set, and the next prompt clears it.
#[gpui::test]
#[ignore = "e2e: run via .\\rt.ps1 native-e2e"]
fn live_hook_status_reports_background_beside_idle(cx: &mut TestAppContext) {
    let node = Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success());
    assert!(node, "node not on PATH; fake-claude needs it");
    let daemon = LiveDaemon::start("hook-background");
    let repo = daemon.git_fixture();
    let (mut h, client) = Harness::open_live(cx, &daemon);
    wait_connected(&mut h, &client, &daemon);

    client.send(ClientMessage::AddRepo {
        path: repo.to_string_lossy().into_owned(),
        name: None,
    });
    let mut repo_id = None;
    h.wait_until("the fixture repo in state.json", SPAWN, |_| {
        repo_id = daemon.repo_id(&repo);
        repo_id.is_some()
    });
    client.send(spawn_claude_in_place(
        &repo_id.expect("the repo was registered"),
    ));
    let pane = place_first_session(&mut h, &client);

    let session = wait_snapshot(&mut h, &client, "the claude session", |_| true);
    let id = session.id.clone();

    // The ready banner is briefly classified `Working`; cue only once the
    // spawn has settled.
    wait_claude_prompt(&mut h, &pane);
    let settled = wait_status(&mut h, &client, 0, &id, SessionStatus::Idle);
    assert!(
        !settled.background,
        "nothing runs in the background at spawn"
    );

    let from = client.message_count();
    h.type_into(&pane, "/rt-hook bg\n");
    let mut found = None;
    h.wait_until(
        &format!("session {id} to report Idle with background work"),
        SPAWN,
        |_| {
            found = snapshot_from(&client, from, |s| {
                s.id == id && s.status == SessionStatus::Idle && s.background
            });
            found.is_some()
        },
    );
    let background = found.expect("a background snapshot was found");
    assert_eq!(background.status, SessionStatus::Idle);

    let from = client.message_count();
    h.type_into(&pane, "/rt-hook prompt\n");
    let working = wait_status(&mut h, &client, from, &id, SessionStatus::Working);
    assert!(!working.background, "a new turn clears the background flag");
}

/// The processes still running from under `dir` once they have had
/// [`EXIT`] to go.
fn survivors_under(dir: &Path) -> Vec<(u32, PathBuf)> {
    let deadline = Instant::now() + EXIT;
    loop {
        let survivors = processes_under(dir);
        if survivors.is_empty() || Instant::now() >= deadline {
            return survivors;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[gpui::test]
#[ignore = "e2e: run via .\\rt.ps1 native-e2e"]
fn live_kill_leaves_no_tracer_behind(cx: &mut TestAppContext) {
    let mut daemon = LiveDaemon::start("kill-tracers");
    let binaries = daemon.binaries_dir();
    let (mut h, client) = Harness::open_live(cx, &daemon);
    wait_connected(&mut h, &client, &daemon);
    client.send(spawn_shell(daemon.dir()));
    h.wait_until(
        "the shell in the sidebar and its tracer running",
        SPAWN,
        |h| {
            let listed = h.root(|root, _| {
                root.sidebar_containers()
                    .iter()
                    .any(|c| !c.leaves.is_empty())
            });
            listed && !processes_under(&binaries).is_empty()
        },
    );

    daemon.kill();
    drop(daemon);

    let survivors = survivors_under(&binaries);
    // Every survivor runs from this spec's own binaries dir, so it is ours.
    for (pid, _) in &survivors {
        kill_tree(*pid);
    }
    assert!(
        survivors.is_empty(),
        "processes from {} outlived the daemon's drop: {survivors:#?}",
        binaries.display()
    );
}

#[gpui::test]
#[ignore = "e2e: run via .\\rt.ps1 native-e2e"]
fn live_daemon_death_shows_reconnecting(cx: &mut TestAppContext) {
    let mut daemon = LiveDaemon::start("death");
    let (mut h, client) = Harness::open_live(cx, &daemon);
    wait_connected(&mut h, &client, &daemon);
    let before = client.footer_labels().len();

    daemon.kill();

    h.wait_until(
        "the footer to show the drop, then a reconnect attempt",
        CONNECT,
        |_| {
            let labels = client.footer_labels();
            let after = &labels[before..];
            after
                .iter()
                .position(|label| label == "disconnected")
                .is_some_and(|dropped| after[dropped..].iter().any(|label| label == "connecting…"))
        },
    );
    let (port, overlay) = client.latest().expect("the view was sent a state");
    assert_eq!(
        overlay, None,
        "no startup overlay once the client has connected"
    );
    assert_eq!(port, None, "no port while the daemon is gone");
}
