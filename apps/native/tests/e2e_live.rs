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
use protocol::{ClientMessage, InitLayoutKind};
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
