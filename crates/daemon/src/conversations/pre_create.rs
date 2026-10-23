//! Creating an agent's conversation before its interactive spawn: the agent's
//! program runs with the plugin's pre-create arguments, and the new
//! conversation's id is read from its stdout with the plugin's parser.
//!
//! Every process the run starts, the program's own children included, is
//! killed when the run ends, however it ends: an id, the program's exit, the
//! deadline, too much output, or the run's future being dropped. On Windows
//! the program is put in a job object that kills them all when its guard
//! drops; the child itself is also spawned with `kill_on_drop`.

use std::ffi::{OsStr, OsString};
use std::future::Future;
use std::io;
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use agent_api::is_plain_id;
use tokio::io::AsyncReadExt as _;
use tokio::process::{Child, ChildStdout, Command};
use tracing::{debug, warn};

#[cfg(windows)]
use super::job::{self, JobGuard};
use crate::tracer_client::{PARENT_SESSION_VARS, is_env_name};

/// How long a pre-create run may take before it is killed and the spawn goes
/// ahead with no conversation.
pub(crate) const PRE_CREATE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a killed run's program is waited for before the run gives up
/// reaping it.
const REAP_TIMEOUT: Duration = Duration::from_secs(2);

/// The most stdout a run reads; past it the run ends with no id.
const STDOUT_CAP: usize = 64 * 1024;

/// How much stdout one read takes.
const READ_CHUNK: usize = 4096;

/// A plugin's parser: the conversation id in the output read so far, if any.
pub(crate) type Parse = fn(&str) -> Option<String>;

/// The process a pre-create run starts.
pub(crate) struct PreCreateSpec {
    /// The resolved program.
    pub(crate) program: String,
    /// The program's leading arguments, then the plugin's pre-create ones.
    pub(crate) args: Vec<String>,
    /// The session's primary folder.
    pub(crate) cwd: PathBuf,
    /// Variables set over the daemon's own environment.
    pub(crate) env_set: Vec<(String, String)>,
    /// Variables removed from it after `env_set` is applied.
    pub(crate) env_remove: Vec<String>,
}

/// Run `spec` and answer the plain conversation id `parse` reads from its
/// stdout, or `None` when the run gives none within [`PRE_CREATE_TIMEOUT`].
pub(crate) async fn pre_create(spec: PreCreateSpec, parse: Parse) -> Option<String> {
    run_until(spec, parse, tokio::time::sleep(PRE_CREATE_TIMEOUT)).await
}

/// [`pre_create`] with its deadline as the future `expire`: the run ends with
/// no id once `expire` resolves.
pub(crate) async fn run_until(
    spec: PreCreateSpec,
    parse: Parse,
    expire: impl Future<Output = ()>,
) -> Option<String> {
    let mut run = Run::start(&spec)?;
    let id = tokio::select! {
        id = run.read_id(parse) => id,
        () = expire => {
            warn!("pre-create: no conversation id before the deadline");
            None
        }
    };
    run.finish().await;
    id.filter(|id| {
        let plain = is_plain_id(id);
        if !plain {
            warn!(
                id_len = id.len(),
                "pre-create: the parsed conversation id is not a plain id"
            );
        }
        plain
    })
}

/// The pre-create child's environment: `base` (the daemon's own) without a
/// parent Claude session's markers, as every PTY spawn drops them, then each
/// variable in `set` applied, then each name in `remove` removed.
fn child_env(
    base: impl IntoIterator<Item = (OsString, OsString)>,
    set: &[(String, String)],
    remove: &[String],
) -> Vec<(OsString, OsString)> {
    let mut env: Vec<(OsString, OsString)> = base
        .into_iter()
        .filter(|(key, _)| !PARENT_SESSION_VARS.iter().any(|var| names(key, var)))
        .collect();
    for (name, value) in set {
        env.retain(|(key, _)| !names(key, name));
        env.push((OsString::from(name), OsString::from(value)));
    }
    env.retain(|(key, _)| !remove.iter().any(|name| names(key, name)));
    env
}

/// Whether the environment name `key` is `var`.
fn names(key: &OsStr, var: &str) -> bool {
    key.to_str().is_some_and(|key| is_env_name(key, var))
}

/// The command for `spec`: the environment [`child_env`] makes from the
/// daemon's, no stdin, piped stdout, no stderr, no window.
fn command(spec: &PreCreateSpec) -> Command {
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args)
        .current_dir(&spec.cwd)
        .env_clear()
        .envs(child_env(
            std::env::vars_os(),
            &spec.env_set,
            &spec.env_remove,
        ))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Put `child` in a job that kills it and everything it starts when the
/// guard drops. A failure leaves only the child itself to be killed.
#[cfg(windows)]
fn contain(child: &Child) -> Option<JobGuard> {
    let pid = child.id()?;
    match job::assign_kill_on_close(pid) {
        Ok(guard) => Some(guard),
        Err(err) => {
            warn!(
                error = %err,
                "pre-create: could not put the program in a job; only the program itself is killed"
            );
            None
        }
    }
}

/// A started run. Its fields drop in order, so dropping it closes the job,
/// killing the whole tree, and then kills the child.
struct Run {
    #[cfg(windows)]
    job: Option<JobGuard>,
    child: Child,
    stdout: ChildStdout,
}

impl Run {
    fn start(spec: &PreCreateSpec) -> Option<Self> {
        let mut child = match command(spec).spawn() {
            Ok(child) => child,
            Err(err) => {
                warn!(error = %err, "pre-create: could not start the agent's program");
                return None;
            }
        };
        #[cfg(windows)]
        let job = contain(&child);
        let Some(stdout) = child.stdout.take() else {
            warn!("pre-create: the agent's program has no stdout pipe");
            return None;
        };
        Some(Self {
            #[cfg(windows)]
            job,
            child,
            stdout,
        })
    }

    /// Read stdout until `parse` finds an id in it, the program exits or
    /// stdout ends.
    async fn read_id(&mut self, parse: Parse) -> Option<String> {
        let mut out = Output::default();
        let mut chunk = [0u8; READ_CHUNK];
        loop {
            tokio::select! {
                read = self.stdout.read(&mut chunk) => {
                    if let ControlFlow::Break(id) = out.after_read(read, &chunk, parse) {
                        return id;
                    }
                }
                status = self.child.wait() => return self.after_exit(status, out, parse).await,
            }
        }
    }

    /// The id in everything the exited program printed: the job is closed
    /// first, so nothing it started still holds stdout open.
    async fn after_exit(
        &mut self,
        status: io::Result<ExitStatus>,
        mut out: Output,
        parse: Parse,
    ) -> Option<String> {
        match status {
            Ok(status) => debug!(%status, "pre-create: the agent's program exited"),
            Err(err) => warn!(error = %err, "pre-create: waiting for the agent's program failed"),
        }
        #[cfg(windows)]
        drop(self.job.take());
        let room = u64::try_from(STDOUT_CAP.saturating_sub(out.buf.len()) + 1).unwrap_or(u64::MAX);
        let mut rest = Vec::new();
        if let Err(err) = (&mut self.stdout).take(room).read_to_end(&mut rest).await {
            warn!(error = %err, "pre-create: reading the program's output failed");
            return None;
        }
        out.buf.extend_from_slice(&rest);
        if out.buf.len() > STDOUT_CAP {
            warn!(
                cap = STDOUT_CAP,
                "pre-create: the program printed past the cap; parsing the output before it"
            );
            let cut = out.buf[..STDOUT_CAP]
                .iter()
                .rposition(|byte| *byte == b'\n')
                .map_or(STDOUT_CAP, |newline| newline + 1);
            out.buf.truncate(cut);
        }
        out.parse_all(parse)
    }

    /// Kill the whole tree and reap the program, giving up after
    /// [`REAP_TIMEOUT`].
    async fn finish(mut self) {
        #[cfg(windows)]
        drop(self.job.take());
        if let Err(err) = self.child.start_kill() {
            debug!(error = %err, "pre-create: killing the agent's program failed");
        }
        match tokio::time::timeout(REAP_TIMEOUT, self.child.wait()).await {
            Ok(Ok(status)) => debug!(%status, "pre-create: the agent's program was reaped"),
            Ok(Err(err)) => warn!(error = %err, "pre-create: reaping the agent's program failed"),
            Err(_) => warn!(
                timeout_ms = REAP_TIMEOUT.as_millis(),
                "pre-create: the agent's program was not reaped in time"
            ),
        }
    }
}

/// The stdout a run has read, at most [`STDOUT_CAP`] bytes.
#[derive(Default)]
struct Output {
    buf: Vec<u8>,
}

impl Output {
    /// Take one read's result: an id ends the run, as do the end of stdout,
    /// a read error and the cap.
    fn after_read(
        &mut self,
        read: io::Result<usize>,
        chunk: &[u8],
        parse: Parse,
    ) -> ControlFlow<Option<String>> {
        match read {
            Ok(0) => ControlFlow::Break(self.parse_all(parse)),
            Ok(n) => self.feed(&chunk[..n], parse),
            Err(err) => {
                warn!(error = %err, "pre-create: reading the program's output failed");
                ControlFlow::Break(None)
            }
        }
    }

    /// Append `bytes` up to the cap and parse everything read so far at the
    /// end of each new line.
    fn feed(&mut self, bytes: &[u8], parse: Parse) -> ControlFlow<Option<String>> {
        let room = STDOUT_CAP.saturating_sub(self.buf.len());
        let kept = &bytes[..bytes.len().min(room)];
        let start = self.buf.len();
        self.buf.extend_from_slice(kept);
        let line_ends = kept
            .iter()
            .enumerate()
            .filter(|(_, byte)| **byte == b'\n')
            .map(|(offset, _)| start + offset + 1);
        for end in line_ends {
            if let Some(id) = parse(&String::from_utf8_lossy(&self.buf[..end])) {
                return ControlFlow::Break(Some(id));
            }
        }
        if kept.len() < bytes.len() {
            warn!(
                cap = STDOUT_CAP,
                "pre-create: the program printed past the cap"
            );
            return ControlFlow::Break(None);
        }
        ControlFlow::Continue(())
    }

    fn parse_all(&self, parse: Parse) -> Option<String> {
        parse(&String::from_utf8_lossy(&self.buf))
    }
}

#[cfg(all(test, windows))]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{Duration, Instant};

    use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };

    use std::ffi::OsString;

    use super::{PreCreateSpec, child_env, run_until};

    static NEXT_DIR: AtomicU32 = AtomicU32::new(0);

    /// How long a process the run started may take to die once it returns.
    const DEATH_CEILING_MS: u32 = 30_000;

    /// How long the child may take to write its pid file.
    const PID_FILE_CEILING: Duration = Duration::from_secs(60);

    /// A fresh directory under the worktree's `.tmp/`, removed on drop.
    struct TestDir(PathBuf);

    impl TestDir {
        fn new(tag: &str) -> Self {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join(".tmp")
                .join(format!(
                    "pre-create-{tag}-{}-{}",
                    std::process::id(),
                    NEXT_DIR.fetch_add(1, Ordering::Relaxed)
                ));
            std::fs::create_dir_all(&path).expect("create the test dir");
            Self(path)
        }

        fn file(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A process opened for waiting on, so a reused pid cannot stand in for it.
    struct Process(HANDLE);

    impl Process {
        fn open(pid: u32) -> Self {
            // SAFETY: OpenProcess returns a handle the caller must close,
            // which Drop does.
            let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }
                .expect("open a process the run started");
            Self(handle)
        }

        fn exited(&self) -> bool {
            // SAFETY: the handle is open; self owns it.
            unsafe { WaitForSingleObject(self.0, DEATH_CEILING_MS) == WAIT_OBJECT_0 }
        }
    }

    impl Drop for Process {
        fn drop(&mut self) {
            // SAFETY: the handle came from OpenProcess and is not yet closed.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }

    /// The text after `id: ` on the first line that holds it.
    fn after_id(text: &str) -> Option<String> {
        text.lines()
            .find_map(|line| line.trim().strip_prefix("id: "))
            .map(|id| id.trim().to_owned())
    }

    fn powershell(script: &str, cwd: &Path) -> PreCreateSpec {
        PreCreateSpec {
            program: "powershell.exe".to_owned(),
            args: vec![
                "-NoProfile".to_owned(),
                "-Command".to_owned(),
                script.to_owned(),
            ],
            cwd: cwd.to_path_buf(),
            env_set: Vec::new(),
            env_remove: Vec::new(),
        }
    }

    /// A script that starts a grandchild, writes its own and the grandchild's
    /// pid to `dir`'s `pids` file, then runs `then`.
    fn tree_script(dir: &TestDir, then: &str) -> String {
        let written = dir.file("pids.tmp");
        let done = dir.file("pids");
        format!(
            "$g = Start-Process powershell -ArgumentList '-NoProfile','-Command','Start-Sleep 600' \
             -PassThru -WindowStyle Hidden; \
             Set-Content -LiteralPath '{}' -Value (($PID, $g.Id) -join ' '); \
             Move-Item -LiteralPath '{}' -Destination '{}'; {then}",
            written.display(),
            written.display(),
            done.display()
        )
    }

    /// The processes named in `file`, opened once it appears.
    async fn wait_for_tree(file: &Path) -> Vec<Process> {
        let started = Instant::now();
        while !file.exists() && started.elapsed() < PID_FILE_CEILING {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let text = std::fs::read_to_string(file).expect("the child wrote its pid file");
        text.split_whitespace()
            .map(|pid| Process::open(pid.parse().expect("a pid")))
            .collect()
    }

    fn assert_all_exited(tree: &[Process]) {
        assert_eq!(tree.len(), 2, "the child and the grandchild");
        for process in tree {
            assert!(process.exited(), "a process the run started outlived it");
        }
    }

    #[tokio::test]
    async fn pre_create_timeout_kills_the_whole_process_tree() {
        let dir = TestDir::new("timeout");
        let spec = powershell(&tree_script(&dir, "Start-Sleep 600"), &dir.0);
        let pids = dir.file("pids");
        let mut tree = Vec::new();
        let id = run_until(spec, after_id, async {
            tree = wait_for_tree(&pids).await;
        })
        .await;
        assert_eq!(id, None);
        assert_all_exited(&tree);
    }

    #[tokio::test]
    async fn pre_create_returns_the_first_parsed_id_and_kills_a_lingering_tree() {
        let dir = TestDir::new("first-id");
        let go = dir.file("go");
        let then = format!(
            "while (-not (Test-Path -LiteralPath '{}')) {{ Start-Sleep -Milliseconds 50 }}; \
             [Console]::Out.WriteLine('id: abc-123'); [Console]::Out.Flush(); Start-Sleep 600",
            go.display()
        );
        let spec = powershell(&tree_script(&dir, &then), &dir.0);
        let pids = dir.file("pids");
        let watch = async {
            let tree = wait_for_tree(&pids).await;
            std::fs::write(&go, b"").expect("write the go file");
            tree
        };
        let (id, tree) = tokio::join!(run_until(spec, after_id, std::future::pending()), watch);
        assert_eq!(id.as_deref(), Some("abc-123"));
        assert_all_exited(&tree);
    }

    #[tokio::test]
    async fn a_dropped_pre_create_kills_the_process_tree() {
        let dir = TestDir::new("dropped");
        let spec = powershell(&tree_script(&dir, "Start-Sleep 600"), &dir.0);
        let pids = dir.file("pids");
        let mut run = Box::pin(run_until(spec, after_id, std::future::pending()));
        let tree = tokio::select! {
            id = &mut run => Err(id),
            tree = wait_for_tree(&pids) => Ok(tree),
        }
        .expect("the run ended before its child wrote the pid file");
        drop(run);
        assert_all_exited(&tree);
    }

    #[tokio::test]
    async fn pre_create_that_prints_nothing_records_no_conversation() {
        let dir = TestDir::new("silent");
        let id = run_until(
            powershell("Start-Sleep 600", &dir.0),
            after_id,
            tokio::time::sleep(Duration::from_millis(1500)),
        )
        .await;
        assert_eq!(id, None);
    }

    #[tokio::test]
    async fn pre_create_rejects_a_non_plain_id_and_a_failed_exit() {
        let dir = TestDir::new("rejects");
        let non_plain = run_until(
            powershell("[Console]::Out.WriteLine('id: ../x')", &dir.0),
            after_id,
            std::future::pending(),
        )
        .await;
        assert_eq!(non_plain, None, "a non-plain id");

        let failed = run_until(
            powershell("exit 3", &dir.0),
            after_id,
            std::future::pending(),
        )
        .await;
        assert_eq!(failed, None, "a failed exit");

        let mut missing = powershell("", &dir.0);
        missing.program = "rt-pre-create-no-such-program.exe".to_owned();
        let missing = run_until(missing, after_id, std::future::pending()).await;
        assert_eq!(missing, None, "a missing program");
    }

    #[tokio::test]
    async fn pre_create_runs_in_the_session_folder_with_its_environment() {
        let dir = TestDir::new("env");
        let cwd = dir.file("pcwd");
        std::fs::create_dir_all(&cwd).expect("create the session folder");
        let mut spec = powershell(
            "[Console]::Out.WriteLine('id: ' + (Split-Path -Leaf (Get-Location).Path) + '-' \
             + $env:RT_SET + '-' + [bool]$env:TEMP)",
            &cwd,
        );
        spec.env_set = vec![("RT_SET".to_owned(), "yes".to_owned())];
        spec.env_remove = vec!["TEMP".to_owned()];
        let id = run_until(spec, after_id, std::future::pending()).await;
        assert_eq!(id.as_deref(), Some("pcwd-yes-False"));
    }

    #[test]
    fn pre_create_drops_the_parent_session_markers() {
        let base = [
            ("CLAUDECODE", "1"),
            ("CLAUDE_CODE_SESSION_ID", "parent"),
            ("PATH", "kept"),
        ]
        .map(|(name, value)| (OsString::from(name), OsString::from(value)));
        let env = child_env(base, &[("RT_SET".to_owned(), "yes".to_owned())], &[]);
        let has =
            |name: &str, value: &str| env.contains(&(OsString::from(name), OsString::from(value)));
        let named = |name: &str| env.iter().any(|(key, _)| key == name);
        assert!(!named("CLAUDECODE"), "{env:?}");
        assert!(!named("CLAUDE_CODE_SESSION_ID"), "{env:?}");
        assert!(has("RT_SET", "yes"), "{env:?}");
        assert!(has("PATH", "kept"), "{env:?}");
    }

    #[tokio::test]
    async fn an_id_before_the_cap_survives_output_past_it() {
        let dir = TestDir::new("past-cap");
        let script = "[Console]::Out.WriteLine('id: abc-1'); $line = 'x' * 99; \
                      for ($i = 0; $i -lt 1000; $i++) { [Console]::Out.WriteLine($line) }";
        let id = run_until(powershell(script, &dir.0), after_id, std::future::pending()).await;
        assert_eq!(id.as_deref(), Some("abc-1"));
    }
}
