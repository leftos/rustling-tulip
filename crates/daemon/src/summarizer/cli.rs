//! The summarizer's call through an agent's own CLI: the agent's program runs
//! the arguments its plugin builds, with the prompt on stdin, in the
//! summarizer folder, and with only the environment the plugin lists. The
//! plugin reads the reply's text and cost from the program's stdout, and
//! [`reply::parse`] reads the summary from that text.
//!
//! One call runs at a time. Every process a call starts, the program's own
//! children included, is killed when the call ends, however it ends: the
//! program's exit, the timeout, too much output, or the call's future being
//! dropped. On Windows the program is put in a job object that kills them all
//! when its guard drops; the child itself is also spawned with
//! `kill_on_drop`, which is all that stops it elsewhere.

use std::future::Future;
use std::io;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use agent_api::{AgentPlugin, SummarizerRequest, SummarizerSpec};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::Semaphore;
use tracing::{debug, warn};

#[cfg(windows)]
use crate::conversations::job::{self, JobGuard};
use crate::server::resolve_agent_program;
use crate::summarizer::reply::{self, ReplyError, SUMMARY_SCHEMA, Summary};

/// How long a call may run, from its program's start, before the whole
/// process tree is killed.
pub(crate) const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a killed program is waited for before the call gives up reaping
/// it.
const REAP_TIMEOUT: Duration = Duration::from_secs(2);

/// The most stdout a call reads; past it the call fails.
const STDOUT_CAP: usize = 256 * 1024;

/// How much of the end of stderr a call keeps for its failure.
const STDERR_TAIL_BYTES: usize = 4096;

/// How many of stderr's last lines a failure quotes.
const STDERR_TAIL_LINES: usize = 8;

/// How much stderr one read takes.
const READ_CHUNK: usize = 4096;

/// How long a call may take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CliConfig {
    /// The most time a call may run, from its program's start.
    pub(crate) timeout: Duration,
}

impl Default for CliConfig {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

/// A summary and what the call cost.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CliReply {
    pub(crate) summary: Summary,
    /// US dollars, as the program reported it; 0 when it reported none.
    pub(crate) cost_usd: f64,
}

/// Why a call gave no summary. A failure after the program started quotes
/// the last lines of its stderr.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CliError {
    /// The agent's plugin offers no summarizer.
    #[error("the agent offers no summarizer")]
    NoSummarizer,
    /// The program could not be started, for one when it is not installed.
    #[error("could not start the agent's program {program}")]
    Start {
        program: String,
        #[source]
        source: io::Error,
    },
    /// Writing the prompt or reading the reply failed.
    #[error("talking to the agent's program failed{}", stderr_note(.stderr))]
    Io {
        #[source]
        source: io::Error,
        stderr: String,
    },
    /// The program printed more than the cap.
    #[error("the agent's program printed more than {cap} bytes")]
    TooMuchOutput { cap: usize },
    /// The program ran past the timeout; its whole tree was killed.
    #[error("the agent's program ran past its time limit and was killed{}", stderr_note(.stderr))]
    TimedOut { stderr: String },
    /// The program exited with a failure status.
    #[error("the agent's program exited with {status}{}", stderr_note(.stderr))]
    Exit { status: ExitStatus, stderr: String },
    /// The plugin read a failure from the program's output.
    #[error("the agent's summarizer failed: {message}{}", stderr_note(.stderr))]
    Output { message: String, stderr: String },
    /// The reply's text held no usable summary; it was still billed.
    #[error("the reply held no summary")]
    Reply {
        #[source]
        source: ReplyError,
        cost_usd: f64,
    },
    /// The runner is shutting down.
    #[error("the summarizer is shutting down")]
    Closed,
}

/// `stderr` as the end of a failure's message, or nothing when it is empty.
fn stderr_note(stderr: &str) -> String {
    if stderr.is_empty() {
        String::new()
    } else {
        format!("; stderr: {stderr}")
    }
}

/// Runs summarizer calls through an agent's CLI, one at a time.
pub(crate) struct CliRunner {
    config: CliConfig,
    permits: Semaphore,
}

/// One call: the resolved program with its leading arguments, the plugin's
/// spec, and the folder it runs in.
struct CliCall<'a> {
    program: String,
    lead_args: Vec<String>,
    spec: SummarizerSpec,
    folder: &'a Path,
}

impl CliRunner {
    /// A runner whose calls each run for at most `config`'s timeout.
    pub(crate) fn new(config: CliConfig) -> Self {
        Self {
            config,
            permits: Semaphore::new(1),
        }
    }

    /// Summarize `prompt` through `plugin`'s CLI, run in `folder`. The
    /// program is the plugin's own (its override variable, else its default
    /// name), resolved as an interactive spawn resolves it; the host's launch
    /// command never applies, since extra arguments could change the reply.
    /// The plugin's environment allow-list reads the daemon's environment.
    ///
    /// # Errors
    ///
    /// See [`CliError`].
    pub(crate) async fn summarize(
        &self,
        plugin: &dyn AgentPlugin,
        folder: &Path,
        prompt: &str,
    ) -> Result<CliReply, CliError> {
        // The request borrows a reader that is not `Sync`, so it is dropped
        // before the first await and the call's future stays `Send`.
        let spec = {
            let read_env = |name: &str| std::env::var(name).ok();
            let request = SummarizerRequest {
                schema: SUMMARY_SCHEMA,
                read_env: &read_env,
            };
            plugin.summarizer(&request)
        }
        .ok_or(CliError::NoSummarizer)?;
        let (program, lead_args) = resolve_agent_program(&plugin.resolve_program());
        let call = CliCall {
            program,
            lead_args,
            spec,
            folder,
        };
        self.run(&call, prompt).await
    }

    /// Run `call` with the configured timeout, which starts once the call has
    /// its turn.
    async fn run(&self, call: &CliCall<'_>, prompt: &str) -> Result<CliReply, CliError> {
        let timeout = self.config.timeout;
        self.run_until(
            call,
            prompt,
            async move { tokio::time::sleep(timeout).await },
        )
        .await
    }

    /// [`Self::run`] with its deadline as the future `expire`, first polled
    /// once the program has started: the call fails once it resolves.
    async fn run_until(
        &self,
        call: &CliCall<'_>,
        prompt: &str,
        expire: impl Future<Output = ()>,
    ) -> Result<CliReply, CliError> {
        let Ok(_permit) = self.permits.acquire().await else {
            return Err(CliError::Closed);
        };
        let mut run = Run::start(call)?;
        let mut stderr_tail = Vec::new();
        let ended = tokio::select! {
            ended = run.exchange(prompt, &mut stderr_tail) => ended,
            () = expire => Err(Failure::TimedOut),
        };
        run.finish().await;
        let stderr = last_lines(&stderr_tail);
        let stdout = match ended {
            Ok((status, _)) if !status.success() => return Err(CliError::Exit { status, stderr }),
            Ok((_, stdout)) => stdout,
            Err(failure) => return Err(failure.into_error(stderr)),
        };
        read_reply(call, &stdout, stderr)
    }
}

/// The summary in `stdout`: the plugin reads the reply's text and cost, and
/// [`reply::parse`] reads the summary from the text.
fn read_reply(call: &CliCall<'_>, stdout: &[u8], stderr: String) -> Result<CliReply, CliError> {
    let output = (call.spec.parse_output)(&String::from_utf8_lossy(stdout))
        .map_err(|message| CliError::Output { message, stderr })?;
    let summary = reply::parse(&output.text).map_err(|source| CliError::Reply {
        source,
        cost_usd: output.cost_usd,
    })?;
    Ok(CliReply {
        summary,
        cost_usd: output.cost_usd,
    })
}

/// How a started program's exchange failed, before its stderr is known.
enum Failure {
    TimedOut,
    TooMuchOutput,
    Io(io::Error),
}

impl Failure {
    fn into_error(self, stderr: String) -> CliError {
        match self {
            Self::TimedOut => CliError::TimedOut { stderr },
            Self::TooMuchOutput => CliError::TooMuchOutput { cap: STDOUT_CAP },
            Self::Io(source) => CliError::Io { source, stderr },
        }
    }
}

/// The command for `call`: the spec's environment and nothing else, every
/// stream piped, no window.
fn command(call: &CliCall<'_>) -> Command {
    let mut cmd = Command::new(&call.program);
    cmd.args(&call.lead_args)
        .args(&call.spec.args)
        .current_dir(call.folder)
        .env_clear()
        .envs(call.spec.env.iter().map(|(name, value)| (name, value)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
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
                "summarizer: could not put the agent's program in a job; only the program itself is killed"
            );
            None
        }
    }
}

/// A started program. Its fields drop in order, so dropping it closes the
/// job, killing the whole tree, and then kills the child.
struct Run {
    #[cfg(windows)]
    job: Option<JobGuard>,
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: ChildStdout,
    stderr: ChildStderr,
}

impl Run {
    fn start(call: &CliCall<'_>) -> Result<Self, CliError> {
        let mut child = command(call).spawn().map_err(|source| CliError::Start {
            program: call.program.clone(),
            source,
        })?;
        #[cfg(windows)]
        let job = contain(&child);
        let pipes = (child.stdin.take(), child.stdout.take(), child.stderr.take());
        let (stdin, Some(stdout), Some(stderr)) = pipes else {
            return Err(CliError::Io {
                source: io::Error::other("the agent's program has no stdout or stderr pipe"),
                stderr: String::new(),
            });
        };
        Ok(Self {
            #[cfg(windows)]
            job,
            child,
            stdin,
            stdout,
            stderr,
        })
    }

    /// Write the prompt and close stdin, read stdout and stderr to their
    /// ends, then wait for the program's exit. The end of stderr goes to
    /// `stderr_tail` as it is read, so a call cut short still has it.
    async fn exchange(
        &mut self,
        prompt: &str,
        stderr_tail: &mut Vec<u8>,
    ) -> Result<(ExitStatus, Vec<u8>), Failure> {
        let write = write_prompt(self.stdin.take(), prompt);
        let read_out = read_capped(&mut self.stdout);
        let read_err = read_tail(&mut self.stderr, stderr_tail);
        let ((), stdout, ()) = tokio::try_join!(write, read_out, read_err)?;
        let status = self.child.wait().await.map_err(Failure::Io)?;
        debug!(%status, "summarizer: the agent's program exited");
        Ok((status, stdout))
    }

    /// Kill the whole tree and reap the program, giving up after
    /// [`REAP_TIMEOUT`].
    async fn finish(mut self) {
        #[cfg(windows)]
        drop(self.job.take());
        if let Err(err) = self.child.start_kill() {
            debug!(error = %err, "summarizer: killing the agent's program failed");
        }
        match tokio::time::timeout(REAP_TIMEOUT, self.child.wait()).await {
            Ok(Ok(status)) => debug!(%status, "summarizer: the agent's program was reaped"),
            Ok(Err(err)) => warn!(error = %err, "summarizer: reaping the agent's program failed"),
            Err(_) => warn!(
                timeout_ms = REAP_TIMEOUT.as_millis(),
                "summarizer: the agent's program was not reaped in time"
            ),
        }
    }
}

/// Write `prompt` to the program's stdin and close it. A program that exits
/// without reading it all is judged by its exit and output, so a failed
/// write is logged, not a failure.
async fn write_prompt(stdin: Option<ChildStdin>, prompt: &str) -> Result<(), Failure> {
    let Some(mut stdin) = stdin else {
        warn!("summarizer: the agent's program has no stdin pipe; the prompt was not sent");
        return Ok(());
    };
    if let Err(err) = stdin.write_all(prompt.as_bytes()).await {
        warn!(error = %err, "summarizer: writing the prompt to the agent's program failed");
        return Ok(());
    }
    if let Err(err) = stdin.shutdown().await {
        warn!(error = %err, "summarizer: closing the agent's program's stdin failed");
    }
    Ok(())
}

/// All of stdout, failing once it passes [`STDOUT_CAP`].
async fn read_capped(stdout: &mut ChildStdout) -> Result<Vec<u8>, Failure> {
    let mut buf = Vec::new();
    let room = u64::try_from(STDOUT_CAP + 1).unwrap_or(u64::MAX);
    stdout
        .take(room)
        .read_to_end(&mut buf)
        .await
        .map_err(Failure::Io)?;
    if buf.len() > STDOUT_CAP {
        return Err(Failure::TooMuchOutput);
    }
    Ok(buf)
}

/// Read stderr to its end, keeping its last [`STDERR_TAIL_BYTES`] in `tail`.
async fn read_tail(stderr: &mut ChildStderr, tail: &mut Vec<u8>) -> Result<(), Failure> {
    let mut chunk = [0u8; READ_CHUNK];
    loop {
        let n = stderr.read(&mut chunk).await.map_err(Failure::Io)?;
        if n == 0 {
            return Ok(());
        }
        tail.extend_from_slice(&chunk[..n]);
        let excess = tail.len().saturating_sub(STDERR_TAIL_BYTES);
        tail.drain(..excess);
    }
}

/// The last [`STDERR_TAIL_LINES`] non-blank lines of `bytes`, joined by
/// newlines.
fn last_lines(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .collect();
    let start = lines.len().saturating_sub(STDERR_TAIL_LINES);
    lines[start..].join("\n")
}

#[cfg(all(test, windows))]
#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert preconditions with expect and fail on the wrong error with panic; failure messages aid debugging"
)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{Duration, Instant};

    use agent_api::{AgentPlugin, SummarizerRequest, SummarizerSpec, SummaryOutput};
    use protocol::AlertKind;
    use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };

    use super::{CliCall, CliConfig, CliError, CliRunner, DEFAULT_TIMEOUT, STDOUT_CAP};
    use crate::summarizer::reply::{ReplyError, SUMMARY_SCHEMA};

    static NEXT_DIR: AtomicU32 = AtomicU32::new(0);

    /// How long a process the call started may take to die once it returns.
    const DEATH_CEILING_MS: u32 = 30_000;

    /// How long the child may take to write its pid file.
    const PID_FILE_CEILING: Duration = Duration::from_secs(60);

    /// A PowerShell function printing the claude CLI's JSON reply with a
    /// `structured_output` of `kind`, `headline` and `notification` "n".
    const REPLY_FN: &str = "function Reply($kind, $headline, $cost) { \
        $o = @{ kind = $kind; headline = $headline; notification = 'n' }; \
        $e = @{ type = 'result'; is_error = $false; structured_output = $o; total_cost_usd = $cost }; \
        [Console]::Out.Write(($e | ConvertTo-Json -Compress -Depth 5)) }";

    /// A fresh directory under the worktree's `.tmp/`, removed on drop.
    struct TestDir(PathBuf);

    impl TestDir {
        fn new(tag: &str) -> Self {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join(".tmp")
                .join(format!(
                    "summarizer-cli-{tag}-{}-{}",
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
                .expect("open a process the call started");
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

    /// The claude plugin's reader of its CLI's reply, from the registry.
    fn claude_parse() -> fn(&str) -> Result<SummaryOutput, String> {
        let plugin = agent_registry::plugin(&agent_registry::default_agent())
            .expect("the default agent is registered");
        plugin
            .summarizer(&SummarizerRequest {
                schema: SUMMARY_SCHEMA,
                read_env: &|_| None,
            })
            .expect("the claude plugin offers a summarizer")
            .parse_output
    }

    /// What PowerShell needs to start, from the test's own environment.
    fn base_env() -> Vec<(String, String)> {
        ["SystemRoot", "TEMP", "TMP", "USERPROFILE", "PATH"]
            .iter()
            .filter_map(|name| {
                std::env::var(name)
                    .ok()
                    .map(|value| ((*name).to_owned(), value))
            })
            .collect()
    }

    /// A call running `script` in PowerShell in `folder`, its stdout read as
    /// the claude CLI's reply.
    fn powershell<'a>(script: &str, folder: &'a Path) -> CliCall<'a> {
        CliCall {
            program: "powershell.exe".to_owned(),
            lead_args: vec![
                "-NoProfile".to_owned(),
                "-NonInteractive".to_owned(),
                "-Command".to_owned(),
                format!("{REPLY_FN}; {script}"),
            ],
            spec: SummarizerSpec {
                args: Vec::new(),
                env: base_env(),
                label: "test".to_owned(),
                privacy: "test".to_owned(),
                parse_output: claude_parse(),
            },
            folder,
        }
    }

    fn runner() -> CliRunner {
        CliRunner::new(CliConfig::default())
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
            assert!(process.exited(), "a process the call started outlived it");
        }
    }

    #[test]
    fn the_default_timeout_is_60_seconds() {
        assert_eq!(DEFAULT_TIMEOUT, Duration::from_secs(60));
        assert_eq!(CliConfig::default().timeout, DEFAULT_TIMEOUT);
    }

    #[tokio::test]
    async fn a_fake_program_reads_the_prompt_from_stdin_and_its_reply_parses() {
        let dir = TestDir::new("stdin");
        let call = powershell(
            "$p = [Console]::In.ReadToEnd(); Reply 'needs_answer' $p.Trim() 0.0029",
            &dir.0,
        );
        let reply = runner()
            .run(&call, "Pick a branch name")
            .await
            .expect("the call answers");
        assert_eq!(reply.summary.kind, AlertKind::NeedsAnswer);
        assert_eq!(reply.summary.headline, "Pick a branch name");
        assert_eq!(reply.summary.notification, "n");
        assert!((reply.cost_usd - 0.0029).abs() < 1e-9, "{}", reply.cost_usd);
    }

    #[tokio::test]
    async fn the_program_runs_in_the_summarizer_folder() {
        let dir = TestDir::new("cwd");
        let folder = dir.file("summarizer");
        std::fs::create_dir_all(&folder).expect("create the summarizer folder");
        let call = powershell(
            "Reply 'done_waiting' (Split-Path -Leaf (Get-Location).Path) 0",
            &folder,
        );
        let reply = runner().run(&call, "p").await.expect("the call answers");
        assert_eq!(reply.summary.headline, "summarizer");
    }

    #[tokio::test]
    async fn the_program_gets_only_the_specs_environment() {
        assert!(
            std::env::var_os("COMPUTERNAME").is_some(),
            "the test needs a variable the daemon has and the spec does not"
        );
        let dir = TestDir::new("env");
        let mut call = powershell(
            "Reply 'done_waiting' ($env:RT_SUMMARIZER_SET + '-' + [bool]$env:COMPUTERNAME) 0",
            &dir.0,
        );
        call.spec
            .env
            .push(("RT_SUMMARIZER_SET".to_owned(), "yes".to_owned()));
        let reply = runner().run(&call, "p").await.expect("the call answers");
        assert_eq!(reply.summary.headline, "yes-False");
    }

    #[tokio::test]
    async fn a_non_zero_exit_is_a_failure_quoting_stderr() {
        let dir = TestDir::new("exit");
        let call = powershell(
            "[Console]::Error.WriteLine('first line'); \
             [Console]::Error.WriteLine('the login expired'); exit 3",
            &dir.0,
        );
        let err = runner().run(&call, "p").await.expect_err("exit 3 fails");
        let CliError::Exit { status, stderr } = &err else {
            panic!("expected an exit failure, got {err:?}");
        };
        assert_eq!(status.code(), Some(3));
        assert!(stderr.contains("the login expired"), "{stderr}");
        assert!(err.to_string().contains("the login expired"), "{err}");
    }

    #[tokio::test]
    async fn is_error_in_the_reply_is_a_failure() {
        let dir = TestDir::new("is-error");
        let call = powershell(
            r#"[Console]::Out.Write('{"type":"result","is_error":true,"result":"API Error: 529"}')"#,
            &dir.0,
        );
        let err = runner().run(&call, "p").await.expect_err("is_error fails");
        let CliError::Output { message, .. } = &err else {
            panic!("expected an output failure, got {err:?}");
        };
        assert!(message.contains("API Error: 529"), "{message}");
    }

    #[tokio::test]
    async fn a_reply_with_an_unknown_kind_is_a_failure_that_keeps_its_cost() {
        let dir = TestDir::new("unknown-kind");
        let call = powershell("Reply 'celebrate' 'h' 0.5", &dir.0);
        let err = runner()
            .run(&call, "p")
            .await
            .expect_err("an unknown kind fails");
        let CliError::Reply { source, cost_usd } = err else {
            panic!("expected a reply failure, got {err:?}");
        };
        assert_eq!(source, ReplyError::UnknownKind);
        assert!((cost_usd - 0.5).abs() < 1e-9, "{cost_usd}");
    }

    #[tokio::test]
    async fn a_missing_program_is_a_failure() {
        let dir = TestDir::new("missing");
        let mut call = powershell("", &dir.0);
        call.program = "rt-summarizer-no-such-program.exe".to_owned();
        let err = runner()
            .run(&call, "p")
            .await
            .expect_err("a missing program fails");
        assert!(matches!(err, CliError::Start { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn output_past_the_cap_is_a_failure() {
        let dir = TestDir::new("past-cap");
        let call = powershell(
            "$line = 'x' * 1023; for ($i = 0; $i -lt 300; $i++) { [Console]::Out.WriteLine($line) }",
            &dir.0,
        );
        let err = runner()
            .run(&call, "p")
            .await
            .expect_err("output past the cap fails");
        assert!(
            matches!(err, CliError::TooMuchOutput { cap } if cap == STDOUT_CAP),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn the_timeout_kills_the_whole_process_tree() {
        let dir = TestDir::new("timeout");
        let call = powershell(&tree_script(&dir, "Start-Sleep 600"), &dir.0);
        let pids = dir.file("pids");
        let mut tree = Vec::new();
        let err = runner()
            .run_until(&call, "p", async {
                tree = wait_for_tree(&pids).await;
            })
            .await
            .expect_err("the deadline fails the call");
        assert!(matches!(err, CliError::TimedOut { .. }), "{err:?}");
        assert_all_exited(&tree);
    }

    #[tokio::test]
    async fn a_short_configured_timeout_ends_the_call() {
        let dir = TestDir::new("short-timeout");
        let call = powershell("Start-Sleep 600", &dir.0);
        let runner = CliRunner::new(CliConfig {
            timeout: Duration::from_millis(500),
        });
        let err = runner
            .run(&call, "p")
            .await
            .expect_err("the timeout fails the call");
        assert!(matches!(err, CliError::TimedOut { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn a_dropped_call_kills_the_process_tree() {
        let dir = TestDir::new("dropped");
        let call = powershell(&tree_script(&dir, "Start-Sleep 600"), &dir.0);
        let pids = dir.file("pids");
        let runner = runner();
        let mut run = Box::pin(runner.run_until(&call, "p", std::future::pending()));
        let tree = tokio::select! {
            ended = &mut run => Err(ended),
            tree = wait_for_tree(&pids) => Ok(tree),
        }
        .expect("the call ended before its child wrote the pid file");
        drop(run);
        assert_all_exited(&tree);
    }

    #[tokio::test]
    async fn two_calls_run_one_at_a_time() {
        let dir = TestDir::new("one-at-a-time");
        let busy = dir.file("busy");
        let overlap = dir.file("overlap");
        let script = format!(
            "try {{ New-Item -ItemType File -Path '{busy}' -ErrorAction Stop | Out-Null }} \
             catch {{ Set-Content -LiteralPath '{overlap}' -Value x }}; \
             Start-Sleep -Milliseconds 400; \
             Remove-Item -LiteralPath '{busy}' -ErrorAction SilentlyContinue; \
             Reply 'done_waiting' 'h' 0",
            busy = busy.display(),
            overlap = overlap.display()
        );
        let call = powershell(&script, &dir.0);
        let runner = runner();
        let (first, second) = tokio::join!(runner.run(&call, "a"), runner.run(&call, "b"));
        first.expect("the first call answers");
        second.expect("the second call answers");
        assert!(!overlap.exists(), "the two calls' programs ran at once");
    }

    /// Runs the real CLI on this machine's login; spends a few tenths of a
    /// cent.
    #[tokio::test]
    #[ignore = "runs the real claude CLI and spends money on the user's login"]
    async fn a_real_cli_call_returns_a_summary() {
        let plugin: &dyn AgentPlugin = agent_registry::plugin(&agent_registry::default_agent())
            .expect("the default agent is registered");
        let folder = crate::paths::resolve_summarizer_dir().expect("the summarizer folder");
        let prompt = "Summarize this coding session for a dashboard. The agent asked: \
                      'Should I name the branch fix-login or login-fix?' and is waiting.";
        let reply = runner()
            .summarize(plugin, &folder, prompt)
            .await
            .expect("the real CLI answers a summary");
        assert_eq!(reply.summary.kind, AlertKind::NeedsAnswer);
        assert_ne!(reply.summary.headline, "");
        assert!(reply.cost_usd > 0.0, "{}", reply.cost_usd);
    }
}
