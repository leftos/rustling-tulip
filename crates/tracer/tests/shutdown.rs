//! The tracer exits promptly after `TracerRequest::Stop`, even when a
//! grandchild still holds the child's console open.
//!
//! The child is `cmd.exe`, which starts a background `ping` on the same
//! console before printing a marker and running a long foreground `ping`.
//! `Stop` kills `cmd.exe` but not the background `ping`, which keeps the
//! pseudoconsole — and with it the PTY output pipe — alive. A tracer that
//! waits for PTY EOF before closing the pseudoconsole never exits.

#![cfg(windows)]

use std::os::windows::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, bail, ensure};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use interprocess::local_socket::GenericNamespaced;
use interprocess::local_socket::tokio::Stream;
use interprocess::local_socket::tokio::prelude::*;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tracer_protocol::{
    InboundTracerResponse, SUPPORTED_TRACER_VERSIONS, TRACER_VERSION, TracerHello, TracerRequest,
    TracerResponse,
};

/// Same flag the daemon spawns the tracer with, so the tracer has no console
/// of its own.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// The hang guard of every wait in this test: the socket, the ready marker,
/// the exit after Stop and the cleanup. A passing run never reaches it, so a
/// busy machine only makes the run slower; reaching it means a hang.
const HANG_GUARD: Duration = Duration::from_secs(120);
const READY_MARKER: &str = "RT_TRACER_READY";
/// `ConPTY`'s startup cursor-position query, which the tracer answers and
/// removes.
const CURSOR_QUERY: &str = "\x1b[6n";
/// The file the no-daemon child creates, in its working directory.
const STARTED_FILE: &str = "started.marker";
const CDB_PATH: &str = r"C:\Program Files (x86)\Windows Kits\10\Debuggers\x64\cdb.exe";
/// How long the stack capture of an already hung tracer may run; it only
/// shapes the failure report.
const CDB_TIMEOUT: Duration = Duration::from_secs(30);

/// Kills the tracer when the test ends, so a failing run leaves no stray
/// process behind. Its kill-on-close job object takes the child tree with it.
struct TracerProcess(Child);

impl Drop for TracerProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{}-{nanos}", std::process::id())
}

/// The names one test's tracer goes by, and its own directory.
struct Case {
    session_id: String,
    pipe_name: String,
    dir: PathBuf,
}

impl Case {
    fn new() -> anyhow::Result<Self> {
        let suffix = unique_suffix();
        let session_id = format!("shutdown-test-{suffix}");
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(&session_id);
        std::fs::create_dir_all(&dir).context("creating test dir")?;
        Ok(Self {
            session_id,
            pipe_name: format!("rt-tracer-test-{suffix}"),
            dir,
        })
    }

    /// Starts the tracer with `script` as its child's `cmd.exe /c` command
    /// line.
    fn spawn_tracer(&self, script: &str) -> anyhow::Result<TracerProcess> {
        spawn_tracer(&self.session_id, &self.pipe_name, &self.dir, script)
    }
}

fn spawn_tracer(
    session_id: &str,
    pipe_name: &str,
    dir: &Path,
    script: &str,
) -> anyhow::Result<TracerProcess> {
    let child = Command::new(env!("CARGO_BIN_EXE_rt-tracer"))
        .arg("--session-id")
        .arg(session_id)
        .arg("--pipe-name")
        .arg(pipe_name)
        .arg("--cwd")
        .arg(dir)
        .arg("cmd.exe")
        .arg("/c")
        .arg(script)
        .env("RUSTLING_TULIP_TRACER_LOG", dir.join("tracer.log"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .context("spawning rt-tracer")?;
    Ok(TracerProcess(child))
}

async fn connect(pipe_name: &str) -> anyhow::Result<Stream> {
    let started = Instant::now();
    loop {
        let name = pipe_name.to_ns_name::<GenericNamespaced>()?;
        match Stream::connect(name).await {
            Ok(stream) => return Ok(stream),
            Err(err) if started.elapsed() >= HANG_GUARD => {
                return Err(err).context(format!(
                    "hung: the tracer socket was not connectable within {HANG_GUARD:?}"
                ));
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
}

async fn write_line<W, T>(writer: &mut W, msg: &T) -> anyhow::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
    T: serde::Serialize,
{
    let mut line = serde_json::to_string(msg)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    Ok(())
}

/// Read Output frames until the child has printed [`READY_MARKER`], failing at
/// once on the frame that shows [`CURSOR_QUERY`]. Sends nothing, so the child
/// starts only if the tracer answered the query itself.
async fn read_to_ready<R>(reader: &mut R) -> anyhow::Result<()>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut output = String::new();
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            bail!("tracer closed the socket before the child was ready; output {output:?}");
        }
        if let InboundTracerResponse::Known(TracerResponse::Output { data_b64 }) =
            InboundTracerResponse::from_json_str(line.trim_end())?
        {
            output.push_str(&String::from_utf8_lossy(&B64.decode(data_b64)?));
            ensure!(
                !output.contains(CURSOR_QUERY),
                "the startup query reached the daemon: {output:?}"
            );
            if output.contains(READY_MARKER) {
                return Ok(());
            }
        }
    }
}

/// Connects to the tracer and completes the handshake.
async fn attach(
    pipe_name: &str,
) -> anyhow::Result<(
    BufReader<tokio::io::ReadHalf<Stream>>,
    tokio::io::WriteHalf<Stream>,
)> {
    let stream = connect(pipe_name).await?;
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);

    let hello = TracerHello {
        version: TRACER_VERSION,
        supported: SUPPORTED_TRACER_VERSIONS.to_vec(),
    };
    write_line(&mut write_half, &hello).await?;
    let mut welcome = String::new();
    ensure!(
        reader.read_line(&mut welcome).await? > 0,
        "tracer closed the socket before Welcome"
    );
    Ok((reader, write_half))
}

/// Waits for the child to create `path`, failing at once if the tracer exits.
async fn wait_for_file(path: &Path, tracer: &mut TracerProcess) -> anyhow::Result<()> {
    let started = Instant::now();
    while !path.exists() {
        if let Some(status) = tracer.0.try_wait()? {
            bail!(
                "the tracer exited ({status}) before {} appeared",
                path.display()
            );
        }
        ensure!(
            started.elapsed() < HANG_GUARD,
            "hung: {} did not appear within {HANG_GUARD:?}",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}

async fn wait_for_exit(tracer: &mut TracerProcess) -> anyhow::Result<Option<i32>> {
    let started = Instant::now();
    while started.elapsed() < HANG_GUARD {
        if let Some(status) = tracer.0.try_wait()? {
            return Ok(Some(status.code().unwrap_or(-1)));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(None)
}

#[tokio::test]
async fn tracer_exits_after_stop_while_a_grandchild_holds_the_console() -> anyhow::Result<()> {
    let case = Case::new()?;
    let script = format!(
        "start /b ping -n 60 127.0.0.1 >nul & echo {READY_MARKER} & ping -n 60 127.0.0.1 >nul"
    );
    let mut tracer = case.spawn_tracer(&script)?;
    let (mut reader, mut write_half) = attach(&case.pipe_name).await?;

    tokio::time::timeout(HANG_GUARD, read_to_ready(&mut reader))
        .await
        .context(format!(
            "hung: the child printed no ready marker within {HANG_GUARD:?}"
        ))??;

    write_line(&mut write_half, &TracerRequest::Stop).await?;

    let exited = wait_for_exit(&mut tracer).await?;
    if exited.is_none() {
        let stacks = capture_stacks(&tracer, &case.dir).await;
        bail!(
            "hung: the tracer did not exit within {HANG_GUARD:?} after Stop (log: {}, {stacks})",
            case.dir.join("tracer.log").display()
        );
    }
    drop(tracer);
    remove_test_dir(&case.dir).await
}

#[tokio::test]
async fn the_startup_query_never_reaches_the_daemon() -> anyhow::Result<()> {
    let case = Case::new()?;
    let script = format!("echo {READY_MARKER} & ping -n 600 127.0.0.1 >nul");
    let tracer = case.spawn_tracer(&script)?;
    let (mut reader, _write_half) = attach(&case.pipe_name).await?;

    tokio::time::timeout(HANG_GUARD, read_to_ready(&mut reader))
        .await
        .context(format!(
            "hung: the child printed no ready marker within {HANG_GUARD:?}"
        ))??;

    drop(tracer);
    remove_test_dir(&case.dir).await
}

#[tokio::test]
async fn a_session_starts_with_no_daemon_attached() -> anyhow::Result<()> {
    let case = Case::new()?;
    let script =
        format!("echo {READY_MARKER} & type nul > {STARTED_FILE} & ping -n 600 127.0.0.1 >nul");
    let mut tracer = case.spawn_tracer(&script)?;

    wait_for_file(&case.dir.join(STARTED_FILE), &mut tracer).await?;

    let (mut reader, _write_half) = attach(&case.pipe_name).await?;
    tokio::time::timeout(HANG_GUARD, read_to_ready(&mut reader))
        .await
        .context(format!(
            "hung: the replay showed no ready marker within {HANG_GUARD:?}"
        ))??;

    drop(tracer);
    remove_test_dir(&case.dir).await
}

/// Dump every thread's stack of the hung tracer with cdb, non-invasively, into
/// `stacks.txt` in the test dir. Returns the phrase naming the result for the
/// failure message.
async fn capture_stacks(tracer: &TracerProcess, dir: &Path) -> String {
    let cdb = Path::new(CDB_PATH);
    if !cdb.exists() {
        return "no cdb; no stacks captured".to_owned();
    }
    let stacks = dir.join("stacks.txt");
    match run_cdb(cdb, tracer.0.id(), &stacks).await {
        Ok(()) => format!("stacks: {}", stacks.display()),
        Err(err) => format!("stacks: {} (cdb failed: {err:#})", stacks.display()),
    }
}

async fn run_cdb(cdb: &Path, pid: u32, stacks: &Path) -> anyhow::Result<()> {
    let out = std::fs::File::create(stacks).context("creating stacks.txt")?;
    let err = out.try_clone().context("cloning the stacks.txt handle")?;
    let mut child = Command::new(cdb)
        .arg("-pv")
        .arg("-p")
        .arg(pid.to_string())
        .arg("-c")
        .arg("~*k; q")
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .context("spawning cdb")?;
    let started = Instant::now();
    while child.try_wait()?.is_none() {
        if started.elapsed() >= CDB_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            bail!("cdb did not finish within {CDB_TIMEOUT:?}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(())
}

/// Remove the test dir after a passing run. The job object takes the
/// background `ping` (whose cwd is this dir) down as the tracer exits, which
/// can lag the tracer's own exit, so removal is retried up to the hang guard.
async fn remove_test_dir(dir: &Path) -> anyhow::Result<()> {
    let started = Instant::now();
    loop {
        match std::fs::remove_dir_all(dir) {
            Ok(()) => return Ok(()),
            Err(err) if started.elapsed() >= HANG_GUARD => {
                return Err(err).with_context(|| {
                    format!(
                        "hung: {} not removable within {HANG_GUARD:?}",
                        dir.display()
                    )
                });
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}
