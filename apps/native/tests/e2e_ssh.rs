//! End-to-end specs for the SSH transport: the native client runs
//! `tools/e2e/fake-ssh` as its `ssh`, which checks the argv and runs the real
//! `rustling-tulipd bridge` on this machine against a real daemon isolated
//! under `.tmp/`. They need that daemon build beside the test binary, so they
//! are ignored by default and run by `.\rt.ps1 native-e2e`.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

#[expect(dead_code, reason = "each spec file uses its own share of the helper")]
mod support;

use std::sync::Arc;

use daemon_client::device_tokens::MemoryTokens;
use gpui::TestAppContext;
use protocol::{ClientMessage, DaemonMessage, InitLayoutKind};
use rustling_tulip_native::Connection;
use rustling_tulip_native::file_fetch::FetchRequest;
use rustling_tulip_native::target::Target;
use support::live::{
    FakeSsh, LiveClient, LiveDaemon, commit_all, kill_tree, processes_under, spawn_shell,
};
use support::{Harness, Opened};

/// The saved host's string: the fake ssh ignores it, and the fetched-file
/// folder is named after it.
const HOST: &str = "devbox";
/// The refusal a denied login writes to stderr.
const DENIED: &str = "me@devbox: Permission denied (publickey).";

/// The argv the client runs `ssh` with for [`HOST`] and the default remote
/// command (`apps/native/src/ssh.rs`).
const EXPECTED_ARGV: [&str; 14] = [
    "-T",
    "-o",
    "BatchMode=yes",
    "-o",
    "ServerAliveInterval=15",
    "-o",
    "ServerAliveCountMax=3",
    "-o",
    "ConnectTimeout=10",
    "-o",
    "Compression=no",
    "--",
    HOST,
    "rustling-tulipd bridge",
];

/// A session the host's loopback client spawned shows in a client connected
/// over SSH, and the fake ssh was run with the documented argv.
#[gpui::test]
#[ignore = "e2e: run via .\\rt.ps1 native-e2e"]
fn an_ssh_host_connects_and_lists_its_sessions(
    host_cx: &mut TestAppContext,
    ssh_cx: &mut TestAppContext,
) {
    let daemon = LiveDaemon::start("ssh-list");
    let fake = use_fake_ssh(&daemon, None);
    let (mut host, host_client) = Harness::open_live(host_cx, &daemon);
    wait_open(&mut host, &host_client, "the loopback client to connect");
    host_client.send(spawn_shell(daemon.dir()));
    wait_for_session(&mut host, "the host's session in its sidebar");

    let (mut h, client) = open_ssh(ssh_cx, &daemon);
    wait_open(&mut h, &client, "the client to connect over ssh");
    wait_for_session(&mut h, "the host's session listed over ssh");

    let runs = fake.runs();
    assert_eq!(runs.len(), 1, "one ssh run connected: {runs:?}");
    assert_eq!(
        runs[0].argv, EXPECTED_ARGV,
        "ssh ran with the documented options, `--`, the host and the bridge command"
    );
}

/// A plain shell spawned over SSH echoes what is typed into it.
#[gpui::test]
#[ignore = "e2e: run via .\\rt.ps1 native-e2e"]
fn a_session_over_ssh_echoes_typed_input(cx: &mut TestAppContext) {
    let daemon = LiveDaemon::start("ssh-echo");
    let _fake = use_fake_ssh(&daemon, None);
    let (mut h, client) = open_ssh(cx, &daemon);
    wait_open(&mut h, &client, "the client to connect over ssh");

    client.send(spawn_shell(daemon.dir()));
    let pane = place_first_session(&mut h, &client);
    h.wait_for_row(&pane, "the shell's prompt", |row| {
        row.trim_end().ends_with('>')
    });
    let marker = format!("rt-ssh-marker-{}", std::process::id());
    h.type_into(&pane, &format!("echo {marker}\n"));
    h.wait_for_row(&pane, "the echoed marker on a line of its own", |row| {
        row.trim() == marker
    });
}

/// A file fetched from an SSH host lands under the host's `ssh-` folder and
/// the repo's, and opens.
#[gpui::test]
#[ignore = "e2e: run via .\\rt.ps1 native-e2e"]
fn a_file_fetched_over_ssh_lands_under_its_ssh_folder(cx: &mut TestAppContext) {
    const NOTES: &[u8] = b"rt-e2e notes fetched over ssh\n";
    let daemon = LiveDaemon::start("ssh-fetch");
    let repo = daemon.git_fixture();
    std::fs::create_dir_all(repo.join("docs")).expect("create the fixture's docs dir");
    std::fs::write(repo.join("docs").join("notes.txt"), NOTES).expect("write the notes");
    commit_all(&repo, "notes");
    // Text files open externally, so the fetched file's open is recorded.
    std::fs::write(
        daemon.dir().join("ui").join("native-ui.json"),
        serde_json::json!({ "general": { "open_text_externally": true } }).to_string(),
    )
    .expect("seed native-ui.json");
    let _fake = use_fake_ssh(&daemon, None);
    let (mut h, client) = open_ssh(cx, &daemon);
    wait_open(&mut h, &client, "the client to connect over ssh");

    client.send(ClientMessage::AddRepo {
        path: repo.to_string_lossy().into_owned(),
        name: None,
    });
    let mut repo_id = None;
    h.wait_until("the fixture repo in state.json", |_| {
        repo_id = daemon.repo_id(&repo);
        repo_id.is_some()
    });
    let repo_name = repo
        .file_name()
        .expect("the fixture dir has a name")
        .to_string_lossy()
        .into_owned();
    let root = daemon.dir().join("remote-files");
    let set = root.clone();
    h.root_update(move |view, _, _| view.set_remote_files_root(set));

    let request = FetchRequest {
        repo_id: repo_id.expect("the wait just found it"),
        repo_name: repo_name.clone(),
        worktree_path: None,
        rel_path: "docs/notes.txt".to_owned(),
        line: None,
        column: None,
        open_when_done: true,
    };
    h.root_update(move |view, _, cx| view.fetch_remote_file(request, cx))
        .expect("a fetch on the ssh host starts");
    h.wait_until("the fetched file to open", |h| !h.opened().is_empty());

    let landed = root
        .join(format!("ssh-{HOST}"))
        .join(&repo_name)
        .join("docs")
        .join("notes.txt");
    assert_eq!(h.opened(), [Opened::DefaultApp(landed.clone())]);
    assert_eq!(
        std::fs::read(&landed).expect("the file landed"),
        NOTES,
        "{}",
        landed.display()
    );
}

/// Killing the running ssh's process tree closes the connection; the client
/// runs ssh again, reconnects and is sent the session list again.
#[gpui::test]
#[ignore = "e2e: run via .\\rt.ps1 native-e2e"]
fn a_killed_ssh_reconnects(cx: &mut TestAppContext) {
    let daemon = LiveDaemon::start("ssh-reconnect");
    let fake = use_fake_ssh(&daemon, None);
    let (mut h, client) = open_ssh(cx, &daemon);
    wait_open(&mut h, &client, "the client to connect over ssh");
    client.send(spawn_shell(daemon.dir()));
    wait_for_session(&mut h, "the session in the sidebar");

    let first = fake.runs();
    assert_eq!(first.len(), 1, "one ssh run so far: {first:?}");
    let states_from = client.states().len();
    let from = client.message_count();
    kill_tree(first[0].pid);

    h.wait_until("the client to reconnect through a new ssh", |_| {
        let states = client.states();
        let after = &states[states_from..];
        let closed = after.iter().any(|conn| !conn.is_open());
        closed && after.last().is_some_and(Connection::is_open) && fake.runs().len() >= 2
    });
    h.wait_until("the session list on the new connection", |_| {
        client
            .find_message_from(from, |msg| match msg {
                DaemonMessage::Sessions { sessions } => (!sessions.is_empty()).then_some(()),
                _ => None,
            })
            .is_some()
    });
    let runs = fake.runs();
    assert_ne!(runs[1].pid, runs[0].pid, "the reconnect ran ssh anew");
    assert_eq!(runs[1].argv, EXPECTED_ARGV, "the reconnect's ssh argv");
}

/// With the host's daemon stopped, connecting over SSH has the bridge start
/// one in the same isolated dirs, from the isolated binaries dir.
#[gpui::test]
#[ignore = "e2e: run via .\\rt.ps1 native-e2e"]
fn the_bridge_starts_a_daemon_that_is_not_running(cx: &mut TestAppContext) {
    let mut daemon = LiveDaemon::start("ssh-cold");
    let stopped_pid = daemon.handshake().pid;
    daemon.shut_down_by_cli();
    let _fake = use_fake_ssh(&daemon, None);

    let (mut h, client) = open_ssh(cx, &daemon);
    wait_open(&mut h, &client, "the client to connect over ssh");
    let started = daemon_client::read_handshake_in(&daemon.config_dir())
        .expect("the started daemon wrote its daemon.json in the isolated config dir");
    assert_ne!(
        started.pid, stopped_pid,
        "a new daemon answers, not the stopped one"
    );
    let isolated = processes_under(&daemon.binaries_dir());
    assert!(
        isolated.iter().any(|(pid, _)| *pid == started.pid),
        "the started daemon runs from the isolated binaries dir {}: {isolated:?}",
        daemon.binaries_dir().display()
    );

    client.send(spawn_shell(daemon.dir()));
    wait_for_session(&mut h, "a session on the started daemon");
}

/// A login ssh refuses fails the connection with ssh's own stderr at the
/// end of the reason, and the client never shows as connected.
#[gpui::test]
#[ignore = "e2e: run via .\\rt.ps1 native-e2e"]
fn a_refused_ssh_login_shows_its_stderr(cx: &mut TestAppContext) {
    let daemon = LiveDaemon::start("ssh-refused");
    let fake = use_fake_ssh(&daemon, Some((255, DENIED)));
    let (mut h, client) = open_ssh(cx, &daemon);

    // The first attempt's reason may be a Hello write that failed before
    // ssh's stderr was read; an attempt that read it ends with it.
    let mut reason = None;
    h.wait_until("a close reason ending with ssh's stderr", |_| {
        reason = client.states().iter().find_map(|conn| {
            conn.reason()
                .filter(|reason| reason.ends_with("Permission denied (publickey)."))
                .map(str::to_owned)
        });
        reason.is_some()
    });
    let reason = reason.expect("the wait just found it");
    assert!(
        reason.ends_with(DENIED),
        "the whole stderr line is kept: {reason:?}"
    );
    assert!(
        !client.states().iter().any(Connection::is_open),
        "a refused login is never connected: {:?}",
        client.footer_labels()
    );
    assert_eq!(
        fake.runs().first().map(|run| run.argv.clone()),
        Some(EXPECTED_ARGV.map(str::to_owned).to_vec()),
        "the refused run had the documented argv"
    );
}

/// Sets up the fake ssh for `daemon`, refusing the login with `fail`, and
/// points this process's client at it.
fn use_fake_ssh(daemon: &LiveDaemon, fail: Option<(i32, &str)>) -> FakeSsh {
    let fake = daemon.fake_ssh(fail);
    fake.use_for_this_process();
    fake
}

/// A client of `daemon` over SSH to [`HOST`], through the fake ssh.
fn open_ssh<'a>(cx: &'a mut TestAppContext, daemon: &LiveDaemon) -> (Harness<'a>, LiveClient) {
    let target = Target::parse_ssh(HOST, None).expect("devbox is a valid ssh host");
    Harness::open_live_on(cx, daemon, target, None, Arc::new(MemoryTokens::default()))
}

/// Waits until the view's latest state is open, bounded only by
/// [`Harness::wait_until`]'s hang guard.
fn wait_open(h: &mut Harness<'_>, client: &LiveClient, what: &str) {
    h.wait_until(what, |_| {
        client.states().last().is_some_and(Connection::is_open)
    });
}

/// Waits for a session in the sidebar.
fn wait_for_session(h: &mut Harness<'_>, what: &str) {
    h.wait_until(what, |h| {
        h.root(|root, _| {
            root.sidebar_containers()
                .iter()
                .any(|c| !c.leaves.is_empty())
        })
    });
}

/// Waits for a session in the sidebar, has the daemon lay out every session
/// for this client, and returns the pane it lands in.
fn place_first_session(h: &mut Harness<'_>, client: &LiveClient) -> String {
    wait_for_session(h, "a session in the sidebar");
    client.send(ClientMessage::InitLayout {
        kind: InitLayoutKind::AllSessions,
    });
    h.wait_until("a pane for the session", |h| h.first_pane().is_some());
    h.first_pane().expect("a pane was just laid out")
}
