//! End-to-end spec of a dispatch run: the root view on the real network
//! thread, against a real daemon isolated under `.tmp/native-e2e/` whose
//! live-run folder gets a run file for a fake CLI child that streams
//! stream-json lines slowly (`crates/agents/claude/fake`,
//! `FAKE_CLAUDE_STREAM_JSON`). It needs the daemon and tracer built beside
//! the test binary, so it is ignored by default and run by
//! `.\rt.ps1 native-e2e`.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

#[expect(dead_code, reason = "each spec file uses its own share of the helper")]
mod support;

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use gpui::{TestAppContext, px};
use protocol::{ClientMessage, DaemonMessage, DispatchRunSnapshot};
use serde_json::json;
use support::Harness;
use support::live::{LiveClient, LiveDaemon, fake_claude_script, kill_tree};

/// The run the spec lists; its file is `<live-run folder>/e2e-run.json`.
const RUN_ID: &str = "e2e-run";
/// The sidebar leaf of [`RUN_ID`].
const LEAF: &str = "leaf-e2e-run";
/// The fake CLI's milliseconds between stream-json lines.
const STREAM_INTERVAL_MS: &str = "300";
/// The session id of the fake CLI's stream-json `init` line.
const FAKE_SESSION_ID: &str = "00000000-0000-4000-8000-00000000f00d";
/// The line the daemon logs once a Stop wrote the run's stop marker and
/// killed its child (`crates/daemon/src/server/dispatch_runs.rs`).
const STOPPED_LOG: &str = "dispatch_runs: stopped the run";

/// The fake CLI child the run file names; dropping it kills its tree and
/// reaps it, so a failing spec leaves no `node` behind.
struct RunChild(Child);

impl Drop for RunChild {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            kill_tree(self.0.id());
        }
        let _ = self.0.wait();
    }
}

/// Waits until the view's latest state is open on `daemon`'s port with no
/// connecting overlay.
fn wait_connected(h: &mut Harness<'_>, client: &LiveClient, daemon: &LiveDaemon) {
    let port = daemon.handshake().port;
    h.wait_until("the footer to show the daemon's port", |_| {
        client
            .latest()
            .is_some_and(|(open_port, overlay)| open_port == Some(port) && overlay.is_none())
    });
}

/// Registers the daemon's git fixture as a repo, waits for its id and
/// returns the fixture's path, which the run names as its worktree.
fn register_fixture(h: &mut Harness<'_>, client: &LiveClient, daemon: &LiveDaemon) -> PathBuf {
    let repo = daemon.git_fixture();
    client.send(ClientMessage::AddRepo {
        path: repo.to_string_lossy().into_owned(),
        name: None,
    });
    h.wait_until("the fixture repo in state.json", |_| {
        daemon.repo_id(&repo).is_some()
    });
    repo
}

/// Starts the fake CLI in stream-json mode with its stdout in `stream`.
fn spawn_streaming_child(stream: &Path) -> RunChild {
    let out = File::create(stream).expect("create the run's stream file");
    let mut command = Command::new("node");
    command
        .arg(fake_claude_script())
        .env("FAKE_CLAUDE_STREAM_JSON", STREAM_INTERVAL_MS)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::null());
    dev_daemon::no_console(&mut command);
    RunChild(command.spawn().expect("spawn the fake CLI under node"))
}

/// Writes the run file for `child` into the live-run folder, through a temp
/// file renamed over, so the watcher never reads half of it. Written after
/// the spawn, so the child's start falls inside the guard's window around
/// `started_at`.
fn write_run_file(
    daemon: &LiveDaemon,
    worktree: &Path,
    run_dir: &Path,
    child: &RunChild,
) -> PathBuf {
    let file = json!({
        "schema": 1,
        "run_id": RUN_ID,
        "title": "e2e dispatch run",
        "backend": "backend-e2e",
        "tier": 2,
        "worktree": worktree,
        "run_dir": run_dir,
        "stream": run_dir.join("stream.jsonl"),
        "child_pid": child.0.id(),
        "child_start": null,
        "runner_pid": std::process::id(),
        "runner_start": null,
        "started_at": chrono::Utc::now().to_rfc3339(),
        "session_id": null,
        "resumed_from": null,
        "auto_resume": false,
    });
    let path = daemon.dispatch_dir().join(format!("{RUN_ID}.json"));
    let staged = path.with_extension("json.tmp");
    std::fs::write(&staged, file.to_string()).expect("write the staged run file");
    std::fs::rename(&staged, &path).expect("move the run file into the live-run folder");
    path
}

/// Starts the fake CLI streaming into a run folder under `daemon`'s scratch
/// folder and lists it with a run file; returns the child and the run file.
fn start_streaming_run(daemon: &LiveDaemon, worktree: &Path) -> (RunChild, PathBuf) {
    let run_dir = daemon.dir().join("run");
    std::fs::create_dir_all(&run_dir).expect("create the run folder");
    let child = spawn_streaming_child(&run_dir.join("stream.jsonl"));
    let run_file = write_run_file(daemon, worktree, &run_dir, &child);
    (child, run_file)
}

/// Fails the spec unless `node`, which runs the fake CLI, is on the PATH.
fn assert_node() {
    let node = Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success());
    assert!(node, "node not on PATH; the fake CLI needs it");
}

/// Waits until the run's leaf is laid out in the sidebar with the `DS` tag.
fn wait_dispatch_leaf(h: &mut Harness<'_>) {
    h.wait_until("the run's leaf laid out in the sidebar", |h| {
        h.in_model(LEAF) && h.bounds(LEAF).size.height > px(0.0)
    });
    let tagged = h.root(|root, _| {
        root.sidebar_containers()
            .iter()
            .flat_map(|c| c.leaves.iter())
            .any(|leaf| leaf.id == RUN_ID && leaf.runtime.as_deref() == Some("DS"))
    });
    assert!(tagged, "the run's leaf carries the DS tag");
}

/// The transcript lines pane `pane` draws for the run it follows.
fn dispatch_lines(h: &mut Harness<'_>, pane: &str) -> Vec<String> {
    h.root(|root, _| root.dispatch_view_lines(pane))
}

/// The highest `n` of the `● step <n>` lines in `lines`; 0 before the
/// first. The transcript is capped and drops the oldest entries, so the
/// step count, unlike the line count, keeps rising for as long as the child
/// streams.
fn newest_dispatch_step(lines: &[String]) -> u64 {
    lines
        .iter()
        .filter_map(|line| line.strip_prefix("● step "))
        .filter_map(|n| n.parse().ok())
        .max()
        .unwrap_or(0)
}

/// Waits until the daemon has sent this client the run with the session id
/// of the fake CLI's `init` line: the stream's start reached the daemon.
/// Read from the message log, since the `init` action rolls off the capped
/// action list once the child has streamed long enough.
fn wait_dispatch_init(h: &mut Harness<'_>, client: &LiveClient) {
    let has_init = |run: &DispatchRunSnapshot| {
        run.run_id == RUN_ID && run.agent_session_id.as_deref() == Some(FAKE_SESSION_ID)
    };
    h.wait_until("the run's init session id from the daemon", |_| {
        client
            .find_message(|msg| match msg {
                DaemonMessage::DispatchRuns { runs } => runs.iter().any(has_init).then_some(()),
                DaemonMessage::DispatchRunUpdated { run } => has_init(run).then_some(()),
                _ => None,
            })
            .is_some()
    });
}

/// Clicks the run's leaf, waits for the pane that follows it to show a
/// `Read`, then until a later step than the newest one it shows arrives as
/// the child streams; returns the pane.
fn follow_dispatch_run(h: &mut Harness<'_>, client: &LiveClient) -> String {
    wait_dispatch_init(h, client);
    h.click_on(LEAF);
    let mut pane = None;
    h.wait_until("a pane following the run with a Read", |h| {
        pane = h.first_pane();
        let Some(id) = pane.as_deref() else {
            return false;
        };
        dispatch_lines(h, id)
            .iter()
            .any(|line| line.starts_with("● Read"))
    });
    let pane = pane.expect("the wait just found the pane");
    let seen = newest_dispatch_step(&dispatch_lines(h, &pane));
    h.wait_until("a later step as the child streams", |h| {
        newest_dispatch_step(&dispatch_lines(h, &pane)) > seen
    });
    pane
}

/// Waits until the daemon's log says it stopped the run, which it logs only
/// after the stop marker was written and the kill returned.
fn wait_dispatch_stopped_logged(h: &mut Harness<'_>, daemon: &LiveDaemon) {
    h.wait_until("the daemon to log the run stopped", |_| {
        daemon
            .daemon_log()
            .lines()
            .any(|line| line.contains(STOPPED_LOG) && line.contains(RUN_ID))
    });
}

/// Stops the run from `pane`'s header: Stop arms, its confirm sends.
fn stop_dispatch_from_header(h: &mut Harness<'_>, pane: &str) {
    h.click_on(&format!("dispatch-stop-{pane}"));
    let armed = h.root(|root, _| root.dispatch_stop_armed(pane));
    assert!(armed, "the first Stop press arms the confirm");
    h.click_on(&format!("dispatch-stop-confirm-{pane}"));
}

/// Waits until the daemon has removed the run: the client was sent
/// `DispatchRunRemoved`.
fn wait_dispatch_removed(h: &mut Harness<'_>, client: &LiveClient) {
    h.wait_until("the daemon to remove the run", |_| {
        client
            .find_message(|msg| match msg {
                DaemonMessage::DispatchRunRemoved { run_id } if run_id == RUN_ID => Some(()),
                _ => None,
            })
            .is_some()
    });
}

/// The run's runner (this test process) is alive, so the daemon kept the
/// run file and the stop marker beside it for the runner to read; deletes
/// both, as the dispatch tool does once it has read the marker.
fn finish_as_the_runner(run_file: &Path) {
    let marker = run_file.with_extension("stop");
    assert!(
        run_file.exists(),
        "the daemon keeps the run file while its runner lives"
    );
    assert!(
        marker.exists(),
        "the daemon keeps the stop marker while its runner lives"
    );
    std::fs::remove_file(run_file).expect("delete the run file as the runner");
    std::fs::remove_file(&marker).expect("delete the stop marker as the runner");
}

#[gpui::test]
#[ignore = "e2e: run via .\\rt.ps1 native-e2e"]
fn live_dispatch_run_streams_follows_and_stops(cx: &mut TestAppContext) {
    assert_node();
    let daemon = LiveDaemon::start("dispatch");
    let (mut h, client) = Harness::open_live(cx, &daemon);
    wait_connected(&mut h, &client, &daemon);
    let worktree = register_fixture(&mut h, &client, &daemon);

    let (mut child, run_file) = start_streaming_run(&daemon, &worktree);

    wait_dispatch_leaf(&mut h);
    let pane = follow_dispatch_run(&mut h, &client);
    stop_dispatch_from_header(&mut h, &pane);

    h.wait_until("the stopped child to exit", |_| {
        matches!(child.0.try_wait(), Ok(Some(_)))
    });
    wait_dispatch_stopped_logged(&mut h, &daemon);

    wait_dispatch_removed(&mut h, &client);
    finish_as_the_runner(&run_file);
    h.wait_until("the pane to show the run ended", |h| {
        h.root(|root, _| root.dispatch_view_note(&pane))
            .is_some_and(|note| note.starts_with("Ended. Run folder:"))
    });
    h.wait_until("the run's leaf to leave the sidebar", |h| !h.in_model(LEAF));
}

/// A tool call's head line in a followed run's transcript.
fn is_tool_call(line: &str) -> bool {
    line.starts_with("● Read") || line.starts_with("● Edit")
}

/// A tool's first output line, not the `Running…` of a call still waiting.
fn is_tool_output(line: &str) -> bool {
    line.starts_with("  ⎿  ") && line != "  ⎿  Running…"
}

#[gpui::test]
#[ignore = "e2e: run via .\\rt.ps1 native-e2e"]
fn a_followed_run_shows_its_transcript(cx: &mut TestAppContext) {
    assert_node();
    let daemon = LiveDaemon::start("dispatch-transcript");
    let (mut h, client) = Harness::open_live(cx, &daemon);
    wait_connected(&mut h, &client, &daemon);
    let worktree = register_fixture(&mut h, &client, &daemon);
    let (_child, _run_file) = start_streaming_run(&daemon, &worktree);
    wait_dispatch_leaf(&mut h);
    wait_dispatch_init(&mut h, &client);

    h.click_on(LEAF);

    let mut lines = Vec::new();
    h.wait_until(
        "a pane following the run with a tool call and its output",
        |h| {
            let Some(pane) = h.first_pane() else {
                return false;
            };
            lines = dispatch_lines(h, &pane);
            lines.iter().any(|line| is_tool_call(line))
                && lines.iter().any(|line| is_tool_output(line))
        },
    );
    assert!(
        lines.iter().any(|line| is_tool_call(line)),
        "a tool call names its tool: {lines:?}"
    );
    assert!(
        lines.iter().any(|line| is_tool_output(line)),
        "a tool's output sits under its call: {lines:?}"
    );
    assert!(
        !lines.iter().any(|line| line.starts_with("system:")),
        "the transcript draws no recent action: {lines:?}"
    );
}
