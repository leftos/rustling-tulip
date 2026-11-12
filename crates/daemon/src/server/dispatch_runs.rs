//! Stopping a live dispatch run (`docs/plans/dispatch-follow.md` sections 8
//! and 10). A tailnet client may stop one too, as it may stop a session.
//!
//! The order is the design's: read the run, open its process guard and hold
//! it, mark the run `Stopping`, write `<run_id>.stop`, then kill the child's
//! tree. The marker lands before the kill, so the dispatch tool never sees its
//! child exit without it and never resumes a stopped run.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use protocol::{ClientMessage, DaemonMessage, error_codes};
use tracing::{debug, info, warn};

use super::Hub;
use super::connection::ConnCtx;
use crate::dispatch_runs::{DispatchRuns, DispatchRunsHub, MarkStopping, StopTarget, guard};

/// The `ActionFailed` title for a run that is gone or is no longer its process.
const ALREADY_ENDED: &str = "The run already ended";
/// The `ActionFailed` title for a kill that failed.
const KILL_FAILED: &str = "Could not stop the run";

/// What a Stop came to.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// The run was marked, its marker written and its tree killed.
    Stopped,
    /// Another Stop already marked the run.
    NoOp,
    /// The run is gone, or its process is not the one its file recorded.
    Ended,
    /// The kill failed, with what failed; the run is no longer stopping.
    KillFailed(String),
}

/// Handle the dispatch-run messages `dispatch` routes here. The Stop runs on
/// a task of its own, since it opens a process, writes a file and waits on a
/// kill.
pub(super) fn handle(hub: &Hub, msg: ClientMessage, ctx: &ConnCtx<'_>) {
    match msg {
        ClientMessage::FollowTranscript { source, .. } => {
            debug!(?source, "transcript follow received");
        }
        ClientMessage::UnfollowTranscript { source } => {
            debug!(?source, "transcript unfollow received");
        }
        ClientMessage::StopDispatchRun { run_id, request_id } => {
            stop_run(hub, ctx, run_id, request_id);
        }
        other => warn!(
            ?other,
            "dispatch_runs::handle: routed a message it does not own"
        ),
    }
}

/// Stop the run, answering the requester's reply when nothing was stopped.
fn stop_run(hub: &Hub, ctx: &ConnCtx<'_>, run_id: String, request_id: Option<String>) {
    let Some(dispatch) = hub.dispatch_runs.clone() else {
        let _ = ctx.out_tx.send(DaemonMessage::Error {
            message: "dispatch runs are not followed by this daemon".to_owned(),
            request_id,
            code: Some(error_codes::UNSUPPORTED_MESSAGE.to_owned()),
        });
        return;
    };
    let out_tx = ctx.out_tx.clone();
    tokio::spawn(async move {
        if let Some(reply) = stop(&dispatch, &run_id, request_id).await {
            let _ = out_tx.send(reply);
        }
    });
}

/// Stop the run `run_id` and answer the requester's reply, if any: an
/// `ActionFailed` carrying `request_id` when nothing was stopped, `None` on
/// success (the broadcast `Stopping` and removal answer it) or when another
/// Stop is already under way.
async fn stop(
    dispatch: &DispatchRunsHub,
    run_id: &str,
    request_id: Option<String>,
) -> Option<DaemonMessage> {
    let Some(target) = dispatch.runs.stop_target(run_id) else {
        return Some(already_ended(run_id, request_id));
    };
    if target.stopping {
        return None;
    }
    let runs = Arc::clone(&dispatch.runs);
    let owned_id = run_id.to_owned();
    let marker = marker_path(&dispatch.folder, run_id);
    let outcome =
        tokio::task::spawn_blocking(move || stop_process(&runs, &owned_id, target, &marker))
            .await
            .unwrap_or_else(|failure| {
                warn!(error = %failure, "dispatch_runs: the stop task ended early");
                Outcome::KillFailed(format!("the stop did not finish: {failure}"))
            });
    match outcome {
        Outcome::Stopped | Outcome::NoOp => None,
        Outcome::Ended => Some(already_ended(run_id, request_id)),
        Outcome::KillFailed(detail) => Some(action_failed(KILL_FAILED, detail, request_id)),
    }
}

/// The stop marker of `run_id` in the live-run folder. The id is a listed
/// run's, which the watcher took from a file name in that folder.
fn marker_path(folder: &Path, run_id: &str) -> PathBuf {
    folder.join(format!("{run_id}.stop"))
}

/// Steps 2 to 6 on Windows: open the guard and hold its handle until the
/// tree kill returns, so the pid cannot be reused in between.
#[cfg(windows)]
fn stop_process(runs: &DispatchRuns, run_id: &str, target: StopTarget, marker: &Path) -> Outcome {
    let Some(held) = guard::open(target.child_pid, target.child_start, target.started_at) else {
        return Outcome::Ended;
    };
    if let Some(early) = mark(runs, run_id, target.generation) {
        return early;
    }
    if let Err(detail) = write_marker(marker) {
        return settle(runs, run_id, target.generation, Err(detail));
    }
    let killed = kill_verdict(kill_tree(target.child_pid), || held.current_exit_code());
    drop(held);
    settle(runs, run_id, target.generation, killed)
}

/// Steps 2 to 6 off Windows: check the process again, then kill the single
/// matched process through the `sysinfo` object that was checked. A window
/// between the check and the kill remains, as for a session's kill.
#[cfg(not(windows))]
fn stop_process(runs: &DispatchRuns, run_id: &str, target: StopTarget, marker: &Path) -> Outcome {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    if guard::liveness(target.child_pid, target.child_start, target.started_at)
        != guard::Liveness::Alive
    {
        return Outcome::Ended;
    }
    let pid = Pid::from_u32(target.child_pid);
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::new(),
    );
    let Some(process) = sys
        .process(pid)
        .filter(|process| guard::start_within_window(process.start_time(), target.started_at))
    else {
        return Outcome::Ended;
    };
    if let Some(early) = mark(runs, run_id, target.generation) {
        return early;
    }
    if let Err(detail) = write_marker(marker) {
        return settle(runs, run_id, target.generation, Err(detail));
    }
    let killed = if process.kill() {
        Ok(())
    } else {
        Err("the run's process could not be signalled".to_owned())
    };
    settle(runs, run_id, target.generation, killed)
}

/// Mark the run stopping; `None` when this Stop marked it and goes on, else
/// the outcome that ends it.
fn mark(runs: &DispatchRuns, run_id: &str, generation: u64) -> Option<Outcome> {
    match runs.mark_stopping(run_id, generation) {
        MarkStopping::Marked => None,
        MarkStopping::AlreadyStopping => Some(Outcome::NoOp),
        MarkStopping::Gone => Some(Outcome::Ended),
    }
}

/// Write the empty stop marker. A failure stops the Stop before any kill: a
/// child killed without its marker would be resumed by the dispatch tool.
fn write_marker(marker: &Path) -> Result<(), String> {
    std::fs::write(marker, b"").map_err(|err| {
        warn!(path = %marker.display(), %err, "dispatch_runs: the stop marker could not be written; nothing is killed");
        format!("the stop marker could not be written: {err}")
    })
}

/// Kill the child's process tree with `taskkill`, the tree kill the dispatch
/// tool uses on a timeout, and answer its exit status.
#[cfg(windows)]
fn kill_tree(pid: u32) -> Result<std::process::ExitStatus, String> {
    use std::os::windows::process::CommandExt as _;
    use std::process::{Command, Stdio};
    Command::new("taskkill")
        .args(["/T", "/F", "/PID", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(windows::Win32::System::Threading::CREATE_NO_WINDOW.0)
        .status()
        .map_err(|err| format!("taskkill could not start: {err}"))
}

/// Whether `taskkill` stopped the run. A non-zero exit still counts as
/// stopped when the child, read through the held guard by `child_exit`, has
/// exited: `taskkill /T /F` fails when a tree member exits between its
/// listing and its kill, while the tree is gone. A child still running, or
/// one whose exit code cannot be read, is a failed kill.
#[cfg(windows)]
fn kill_verdict(
    taskkill: Result<std::process::ExitStatus, String>,
    child_exit: impl FnOnce() -> Option<u32>,
) -> Result<(), String> {
    let status = taskkill?;
    if status.success() {
        return Ok(());
    }
    match child_exit() {
        Some(code) if code != guard::STILL_ACTIVE => {
            info!(%status, code, "dispatch_runs: taskkill failed but the run's child has exited");
            Ok(())
        }
        _ => Err(format!("taskkill exited with {status}")),
    }
}

/// The outcome of a stop after the run was marked: a failed one clears the
/// run's stopping mark, so it shows its stream's status again; a marker
/// already written stays for the tool to remove.
fn settle(
    runs: &DispatchRuns,
    run_id: &str,
    generation: u64,
    killed: Result<(), String>,
) -> Outcome {
    match killed {
        Ok(()) => {
            info!(run_id, "dispatch_runs: stopped the run");
            Outcome::Stopped
        }
        Err(detail) => {
            warn!(run_id, %detail, "dispatch_runs: the run could not be stopped");
            runs.clear_stopping(run_id, generation);
            Outcome::KillFailed(detail)
        }
    }
}

/// The reply to a Stop of a run that is gone or no longer its process.
fn already_ended(run_id: &str, request_id: Option<String>) -> DaemonMessage {
    action_failed(
        ALREADY_ENDED,
        format!("The dispatch run {run_id} is no longer running, so there was nothing to stop."),
        request_id,
    )
}

/// An `ActionFailed` for the requester alone.
fn action_failed(title: &str, detail: String, request_id: Option<String>) -> DaemonMessage {
    DaemonMessage::ActionFailed {
        title: title.to_owned(),
        detail,
        hint: None,
        request_id,
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test asserts its preconditions and a wrong reply fails it loudly"
)]
mod tests {
    use super::*;
    use crate::server::accounts::fake::{dispatch_local, next_reply};
    use crate::server::test_support::{ScratchDir, enable_dispatch_runs, test_hub};
    use tokio::sync::mpsc;

    /// A hub following an empty live-run folder `live` in its scratch dir.
    fn dispatch_hub(tag: &str) -> (Hub, ScratchDir, Arc<DispatchRuns>, PathBuf) {
        let (mut hub, scratch) = test_hub(tag);
        let folder = scratch.path().join("live");
        std::fs::create_dir_all(&folder).expect("the live folder is created");
        let runs = enable_dispatch_runs(&mut hub, folder.clone());
        (hub, scratch, runs, folder)
    }

    /// A Stop of `run_id` carrying `request_id` `id`.
    fn stop_message(run_id: &str, id: &str) -> ClientMessage {
        ClientMessage::StopDispatchRun {
            run_id: run_id.to_owned(),
            request_id: Some(id.to_owned()),
        }
    }

    /// The title and request id of an `ActionFailed` reply; `None` for any
    /// other reply.
    fn failure_of(reply: &DaemonMessage) -> Option<(&str, Option<&str>)> {
        match reply {
            DaemonMessage::ActionFailed {
                title, request_id, ..
            } => Some((title.as_str(), request_id.as_deref())),
            _ => None,
        }
    }

    #[tokio::test]
    async fn stop_dispatch_run_on_a_removed_run_fails_and_kills_nothing() {
        let (hub, _scratch, _runs, folder) = dispatch_hub("stop-removed-run");
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();

        dispatch_local(&hub, stop_message("gone", "req-1"), &out_tx).await;
        let reply = next_reply(&mut out_rx).await;
        assert_eq!(
            failure_of(&reply),
            Some((ALREADY_ENDED, Some("req-1"))),
            "{reply:?}"
        );
        assert!(
            !marker_path(&folder, "gone").exists(),
            "no marker is written"
        );
    }

    #[tokio::test]
    async fn second_stop_while_stopping_is_a_no_op() {
        let (hub, _scratch, runs, folder) = dispatch_hub("stop-second");
        let generation = runs.list_for_test("run-s", 4242, Some(7), chrono::Utc::now());
        assert_eq!(
            runs.mark_stopping("run-s", generation),
            MarkStopping::Marked
        );
        let dispatch = hub
            .dispatch_runs
            .clone()
            .expect("the hub follows dispatch runs");

        let reply = stop(&dispatch, "run-s", Some("req-2".to_owned())).await;
        assert!(reply.is_none(), "a second Stop sends nothing: {reply:?}");
        assert!(
            !marker_path(&folder, "run-s").exists(),
            "a second Stop writes no marker"
        );
        assert_eq!(
            runs.mark_stopping("run-s", generation),
            MarkStopping::AlreadyStopping
        );
    }

    #[cfg(windows)]
    mod windows {
        use super::*;
        use crate::dispatch_runs::guard::{Liveness, creation_filetime, liveness};
        use crate::server::connection::spawn_dispatch_run_forwarder;
        use crate::server::test_support::HANG_GUARD;
        use chrono::Utc;
        use protocol::DispatchRunStatus;
        use std::process::{Child, Command, Stdio};
        use std::time::Duration;

        /// A `cmd` parent running a long `ping`, its tree killed and the
        /// parent reaped when dropped.
        struct Tree(Child);

        impl Tree {
            fn spawn() -> Self {
                let child = Command::new("cmd")
                    .args(["/C", "ping", "-n", "120", "127.0.0.1"])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("the cmd parent starts");
                Self(child)
            }

            fn pid(&self) -> u32 {
                self.0.id()
            }
        }

        impl Drop for Tree {
            fn drop(&mut self) {
                let _ = Command::new("taskkill")
                    .args(["/T", "/F", "/PID", &self.pid().to_string()])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        /// Wait until `done` holds, polling the observation, failing as hung
        /// at the guard.
        async fn wait_until(what: &str, done: impl Fn() -> bool) {
            let wait = async {
                while !done() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            };
            tokio::time::timeout(HANG_GUARD, wait)
                .await
                .unwrap_or_else(|_| unreachable!("hung waiting until {what}"));
        }

        /// The pid of `parent`'s child process, once it has one.
        async fn child_of(parent: u32) -> u32 {
            use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
            let find = || {
                let mut sys = System::new();
                sys.refresh_processes_specifics(
                    ProcessesToUpdate::All,
                    true,
                    ProcessRefreshKind::new(),
                );
                sys.processes()
                    .iter()
                    .find(|(_, process)| process.parent() == Some(Pid::from_u32(parent)))
                    .map(|(pid, _)| pid.as_u32())
            };
            wait_until("the parent starts its child", || find().is_some()).await;
            find().expect("the child is listed")
        }

        #[tokio::test]
        async fn stop_dispatch_run_kills_the_tree_and_writes_the_marker() {
            let (hub, _scratch, runs, folder) = dispatch_hub("stop-kills-tree");
            let tree = Tree::spawn();
            let parent = tree.pid();
            let parent_start = creation_filetime(parent).expect("the parent's start is read");
            let grandchild = child_of(parent).await;
            let grandchild_start =
                creation_filetime(grandchild).expect("the grandchild's start is read");
            runs.list_for_test("run-k", parent, Some(parent_start), Utc::now());
            let (out_tx, mut out_rx) = mpsc::unbounded_channel();
            let forwarder = spawn_dispatch_run_forwarder(Arc::clone(&runs), out_tx);
            let dispatch = hub
                .dispatch_runs
                .clone()
                .expect("the hub follows dispatch runs");

            let reply = stop(&dispatch, "run-k", Some("req-3".to_owned())).await;
            assert!(
                reply.is_none(),
                "a successful Stop sends no reply: {reply:?}"
            );
            wait_until("the parent and the grandchild are gone", || {
                liveness(parent, Some(parent_start), Utc::now()) == Liveness::Dead
                    && liveness(grandchild, Some(grandchild_start), Utc::now()) == Liveness::Dead
            })
            .await;
            assert!(
                marker_path(&folder, "run-k").exists(),
                "the marker is written"
            );

            let first = next_reply(&mut out_rx).await;
            let status = match &first {
                DaemonMessage::DispatchRunUpdated { run } if run.run_id == "run-k" => {
                    Some(run.status)
                }
                _ => None,
            };
            assert_eq!(status, Some(DispatchRunStatus::Stopping), "{first:?}");
            while let Ok(message) = out_rx.try_recv() {
                assert!(failure_of(&message).is_none(), "{message:?}");
            }
            forwarder.abort();
        }

        #[tokio::test]
        async fn stop_with_an_unwritable_marker_kills_nothing() {
            let (mut hub, scratch) = test_hub("stop-unwritable-marker");
            // A file where the live-run folder should be: no marker fits in it.
            let folder = scratch.path().join("live");
            std::fs::write(&folder, b"").expect("the file in the folder's place is written");
            let runs = enable_dispatch_runs(&mut hub, folder.clone());
            let tree = Tree::spawn();
            let start = creation_filetime(tree.pid()).expect("the parent's start is read");
            runs.list_for_test("run-w", tree.pid(), Some(start), Utc::now());
            let dispatch = hub
                .dispatch_runs
                .clone()
                .expect("the hub follows dispatch runs");

            let reply = stop(&dispatch, "run-w", Some("req-5".to_owned()))
                .await
                .expect("a Stop that wrote no marker answers");
            assert_eq!(
                failure_of(&reply),
                Some((KILL_FAILED, Some("req-5"))),
                "{reply:?}"
            );
            let DaemonMessage::ActionFailed { detail, .. } = &reply else {
                unreachable!("checked above");
            };
            assert!(
                detail.starts_with("the stop marker could not be written: "),
                "{detail}"
            );
            assert_eq!(
                liveness(tree.pid(), Some(start), Utc::now()),
                Liveness::Alive,
                "nothing was killed"
            );
            assert!(
                runs.snapshots()
                    .iter()
                    .all(|run| run.status != DispatchRunStatus::Stopping),
                "the run is no longer stopping"
            );
        }

        #[test]
        fn taskkill_failure_with_a_dead_child_counts_as_stopped() {
            use std::os::windows::process::ExitStatusExt as _;
            use std::process::ExitStatus;
            let failed = || Ok(ExitStatus::from_raw(128));
            assert_eq!(kill_verdict(Ok(ExitStatus::from_raw(0)), || None), Ok(()));
            assert_eq!(
                kill_verdict(failed(), || Some(1)),
                Ok(()),
                "a non-zero exit with the child gone is a stop"
            );
            assert_eq!(
                kill_verdict(failed(), || Some(guard::STILL_ACTIVE)),
                Err("taskkill exited with exit code: 128".to_owned()),
                "a non-zero exit with the child running is a failure"
            );
            assert_eq!(
                kill_verdict(failed(), || None),
                Err("taskkill exited with exit code: 128".to_owned()),
                "a non-zero exit with the child unreadable is a failure"
            );
            assert_eq!(
                kill_verdict(Err("taskkill could not start: x".to_owned()), || Some(1)),
                Err("taskkill could not start: x".to_owned())
            );
        }

        #[tokio::test]
        async fn stop_dispatch_run_with_a_failed_guard_fails() {
            let (hub, _scratch, runs, folder) = dispatch_hub("stop-failed-guard");
            let tree = Tree::spawn();
            let start = creation_filetime(tree.pid()).expect("the parent's start is read");
            runs.list_for_test("run-g", tree.pid(), Some(start + 1), Utc::now());
            let mut events = runs.subscribe();
            let (out_tx, mut out_rx) = mpsc::unbounded_channel();

            dispatch_local(&hub, stop_message("run-g", "req-4"), &out_tx).await;
            let reply = next_reply(&mut out_rx).await;
            assert_eq!(
                failure_of(&reply),
                Some((ALREADY_ENDED, Some("req-4"))),
                "{reply:?}"
            );
            assert!(
                !marker_path(&folder, "run-g").exists(),
                "no marker is written"
            );
            assert!(events.try_recv().is_err(), "the run never showed Stopping");
            assert!(
                runs.snapshots()
                    .iter()
                    .all(|run| run.status != DispatchRunStatus::Stopping)
            );
            assert_eq!(
                liveness(tree.pid(), Some(start), Utc::now()),
                Liveness::Alive,
                "nothing was killed"
            );
        }
    }
}
