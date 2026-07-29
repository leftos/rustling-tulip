//! Headless (`--print` / structured-JSON) driver.
//!
//! Generic over the agent: each [`crate::agents::AgentBackend`] owns its own
//! line parser via [`crate::agents::AgentBackend::handle_headless_line`].
//! `headless::spawn` wires stdout into that parser and stderr into
//! `recent_actions`; the rest is agent-agnostic.

use crate::orphan;
use crate::paths::Dirs;
use crate::scrollback;
use crate::session::{SessionRegistry, push_recent_action};
use anyhow::{Context as _, anyhow};
use protocol::{Agent, SessionStatus};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::process::Command;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::oneshot;
use tracing::warn;

/// Which arm of the exit waiter's `select!` fired first.
enum Woke {
    /// The child exited on its own; carries its status code.
    Exited(Option<i32>),
    /// The kill channel resolved. `true` when a kill was actually requested,
    /// `false` when the sender was merely dropped.
    KillRequested(bool),
}

#[derive(Debug, Clone)]
pub struct HeadlessSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    /// Agent kind. Drives which backend's `handle_headless_line` parses
    /// stdout — claude's stream-json, codex's exec-json, etc.
    pub agent: Agent,
}

pub struct HeadlessHandle {
    /// One-shot kill request handed to the exit-waiter task, which owns the
    /// `Child` outright. Deliberately *not* a mutex around the `Child`: the
    /// waiter has to hold the child across `wait().await` for the process's
    /// whole lifetime, so any lock it shared with `kill` would make a kill
    /// block until the child exited on its own — and by then the waiter has
    /// already reaped it, leaving nothing to signal.
    kill_tx: AsyncMutex<Option<oneshot::Sender<()>>>,
    pid: Option<u32>,
}

impl HeadlessHandle {
    /// Ask the exit waiter to terminate the child. Fire-and-forget, mirroring
    /// `PtyHandle::kill`: this returns as soon as the request is queued, and
    /// the waiter reports the resulting exit status through the registry.
    /// Repeat calls after the first are no-ops.
    pub async fn kill(&self) {
        let sender = self.kill_tx.lock().await.take();
        if let Some(tx) = sender {
            // `Err` means the waiter already finished — the child is gone and
            // there is nothing left to kill.
            let _ = tx.send(());
        }
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }
}

/// Spawn the headless child and wire its stdout into the registry.
/// Returns immediately; the parser runs in a background task. `dirs` is used
/// to clean up the orphan-meta sidecar when the child exits.
pub fn spawn(
    spec: &HeadlessSpec,
    registry: &Arc<SessionRegistry>,
    session_id: String,
    dirs: Dirs,
) -> anyhow::Result<Arc<HeadlessHandle>> {
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args)
        .current_dir(&spec.cwd)
        .env_clear()
        .envs(spec.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning {} for headless mode", spec.program))?;
    let pid = child.id();
    let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
    let stderr = child.stderr.take().ok_or_else(|| anyhow!("no stderr"))?;

    let (kill_tx, kill_rx) = oneshot::channel::<()>();
    let handle = Arc::new(HeadlessHandle {
        kill_tx: AsyncMutex::new(Some(kill_tx)),
        pid,
    });

    // stdout: parse the agent's headless output line by line. Also persist
    // each line to the scrollback file so the headless event log can be
    // replayed on reattach.
    let registry_for_stdout = Arc::clone(registry);
    let session_for_stdout = session_id.clone();
    let dirs_for_stdout = dirs.clone();
    let backend = crate::agents::backend_for(spec.agent);
    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout).lines();
        loop {
            match reader.next_line().await {
                Ok(Some(line)) => {
                    if line.trim().is_empty() {
                        continue;
                    }
                    let mut chunk = line.clone().into_bytes();
                    chunk.push(b'\n');
                    scrollback::append(&dirs_for_stdout, &session_for_stdout, &chunk);
                    backend.handle_headless_line(&registry_for_stdout, &session_for_stdout, &line);
                }
                Ok(None) => break,
                Err(err) => {
                    warn!(?err, "headless stdout read error");
                    break;
                }
            }
        }
    });

    // stderr: collect into recent_actions.
    let registry_for_stderr = Arc::clone(registry);
    let session_for_stderr = session_id.clone();
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            if !line.trim().is_empty() {
                registry_for_stderr.update(&session_for_stderr, |rec| {
                    push_recent_action(rec, format!("stderr: {line}"));
                });
            }
        }
    });

    // Exit waiter. Owns the `Child` outright and races its natural exit against
    // a kill request, so `HeadlessHandle::kill` never reaches for the child.
    let registry_for_exit = Arc::clone(registry);
    let session_for_exit = session_id;
    let dirs_for_exit = dirs;
    tokio::spawn(async move {
        let mut child = child;
        let mut kill_rx = kill_rx;
        let woke = tokio::select! {
            status = child.wait() => Woke::Exited(status.ok().and_then(|s| s.code())),
            // `Err` means the handle was dropped without ever requesting a
            // kill; that must not terminate the child, so only `Ok` kills.
            recv = &mut kill_rx => Woke::KillRequested(recv.is_ok()),
        };
        let exit = match woke {
            Woke::Exited(code) => code,
            Woke::KillRequested(requested) => {
                if requested
                    && let Err(err) = child.start_kill()
                {
                    warn!(?err, "failed to kill headless child");
                }
                child.wait().await.ok().and_then(|s| s.code())
            }
        };
        registry_for_exit.update(&session_for_exit, |rec| {
            rec.status = SessionStatus::Stopped;
            rec.exit_code = exit;
            push_recent_action(
                rec,
                format!(
                    "exited with code {}",
                    exit.map_or_else(|| "?".into(), |c| c.to_string())
                ),
            );
        });
        registry_for_exit
            .fan_out_attention(session_for_exit.clone(), protocol::AttentionReason::Stopped);
        orphan::try_delete_meta(&dirs_for_exit, &session_for_exit);
    });

    Ok(handle)
}

// stream-json parsing lives in `crate::agents::claude`. Other agents'
// headless parsers will live in their own backend modules.

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert preconditions with expect/panic; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{Duration, Instant};

    /// Per-test scratch config dir so sidecar writes don't collide.
    fn scratch_dirs() -> Dirs {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let tag = SEQ.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("rt-headless-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create scratch dir");
        Dirs {
            config: root.clone(),
            state_file: root.join("state.json"),
            handshake_file: root.join("daemon.json"),
            lan_config_file: root.join("lan.json"),
            lan_cert_file: root.join("lan-cert.pem"),
            lan_key_file: root.join("lan-key.pem"),
            sessions_dir: root.join("sessions"),
            worktrees_dir: root.join("worktrees"),
            binaries_dir: root.join("binaries"),
        }
    }

    /// A child that outlives the test by a wide margin, so "did `kill` return
    /// before the child would have exited anyway" is unambiguous. Must not
    /// depend on stdin: `spawn` gives the child `Stdio::null()`, which makes
    /// `cmd /c pause` and `timeout` exit immediately. `ping` against loopback
    /// sleeps ~1 s per echo and ignores stdin entirely.
    ///
    /// ~20 s: comfortably longer than the 10 s death poll below (so a surviving
    /// child is unambiguous) while keeping a regression from stalling the suite
    /// — the harness waits on the leaked child's stdout pipe before reporting.
    fn long_lived_spec() -> HeadlessSpec {
        let (program, args) = if cfg!(windows) {
            (
                "ping".to_string(),
                vec!["-n".to_string(), "20".to_string(), "127.0.0.1".to_string()],
            )
        } else {
            ("sleep".to_string(), vec!["20".to_string()])
        };
        HeadlessSpec {
            program,
            args,
            cwd: std::env::temp_dir(),
            env: Vec::new(),
            agent: Agent::Claude,
        }
    }

    /// Regression: the exit waiter used to hold a mutex around the `Child`
    /// across `wait().await`, so `kill` blocked for the child's entire
    /// remaining lifetime and then found the child already taken — a stop
    /// request neither returned promptly nor terminated anything.
    #[tokio::test]
    async fn kill_returns_promptly_and_terminates_the_child() {
        let dirs = scratch_dirs();
        let registry = SessionRegistry::new(dirs.clone());
        let handle = spawn(
            &long_lived_spec(),
            &registry,
            "headless-kill-test".to_string(),
            dirs,
        )
        .expect("spawn long-lived headless child");
        let pid = handle.pid().expect("child reports a pid");

        let started = Instant::now();
        handle.kill().await;
        let kill_elapsed = started.elapsed();
        assert!(
            kill_elapsed < Duration::from_secs(2),
            "kill() should not wait on the child; took {kill_elapsed:?}"
        );

        // The waiter does the actual terminate, so poll for the process to go.
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if !pid_is_alive(pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("child pid {pid} still alive after kill()");
    }

    /// Killing after the child has already exited must not panic, hang, or
    /// double-signal. Repeat calls are no-ops.
    #[tokio::test]
    async fn kill_is_idempotent_and_safe_after_exit() {
        let dirs = scratch_dirs();
        let registry = SessionRegistry::new(dirs.clone());
        let spec = HeadlessSpec {
            program: if cfg!(windows) { "cmd" } else { "true" }.to_string(),
            args: if cfg!(windows) {
                vec!["/c".to_string(), "exit".to_string()]
            } else {
                Vec::new()
            },
            cwd: std::env::temp_dir(),
            env: Vec::new(),
            agent: Agent::Claude,
        };
        let handle = spawn(&spec, &registry, "headless-idempotent".to_string(), dirs)
            .expect("spawn short-lived headless child");
        tokio::time::sleep(Duration::from_millis(300)).await;
        handle.kill().await;
        handle.kill().await;
    }

    fn pid_is_alive(pid: u32) -> bool {
        use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System};
        let mut sys =
            System::new_with_specifics(RefreshKind::new().with_processes(ProcessRefreshKind::new()));
        sys.refresh_processes(ProcessesToUpdate::All, true);
        sys.process(sysinfo::Pid::from_u32(pid)).is_some()
    }
}
