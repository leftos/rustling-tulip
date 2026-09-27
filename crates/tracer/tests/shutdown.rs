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
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READY_TIMEOUT: Duration = Duration::from_secs(15);
const EXIT_TIMEOUT: Duration = Duration::from_secs(10);
const READY_MARKER: &str = "RT_TRACER_READY";
const CDB_PATH: &str = r"C:\Program Files (x86)\Windows Kits\10\Debuggers\x64\cdb.exe";
const CDB_TIMEOUT: Duration = Duration::from_secs(30);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

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

fn spawn_tracer(session_id: &str, pipe_name: &str, dir: &Path) -> anyhow::Result<TracerProcess> {
    let script = format!(
        "start /b ping -n 60 127.0.0.1 >nul & echo {READY_MARKER} & ping -n 60 127.0.0.1 >nul"
    );
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
            Err(err) if started.elapsed() >= CONNECT_TIMEOUT => {
                return Err(err).context("tracer socket never became connectable");
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

/// Read Output frames until the child has printed [`READY_MARKER`], which it
/// does only after the background `ping` is running. `ConPTY` opens with a
/// cursor-position query and renders nothing until it is answered, so the
/// query gets a reply the way a terminal would give one.
async fn wait_for_ready<R, W>(reader: &mut R, writer: &mut W) -> anyhow::Result<()>
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut output = String::new();
    let mut answered_cursor_query = false;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            bail!("tracer closed the socket before the child was ready");
        }
        if let InboundTracerResponse::Known(TracerResponse::Output { data_b64 }) =
            InboundTracerResponse::from_json_str(line.trim_end())?
        {
            output.push_str(&String::from_utf8_lossy(&B64.decode(data_b64)?));
            if !answered_cursor_query && output.contains("\x1b[6n") {
                answered_cursor_query = true;
                let reply = TracerRequest::Input {
                    data_b64: B64.encode("\x1b[1;1R"),
                };
                write_line(writer, &reply).await?;
            }
            if output.contains(READY_MARKER) {
                return Ok(());
            }
        }
    }
}

async fn wait_for_exit(tracer: &mut TracerProcess) -> anyhow::Result<Option<i32>> {
    let started = Instant::now();
    while started.elapsed() < EXIT_TIMEOUT {
        if let Some(status) = tracer.0.try_wait()? {
            return Ok(Some(status.code().unwrap_or(-1)));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(None)
}

#[tokio::test]
async fn tracer_exits_after_stop_while_a_grandchild_holds_the_console() -> anyhow::Result<()> {
    let suffix = unique_suffix();
    let session_id = format!("shutdown-test-{suffix}");
    let pipe_name = format!("rt-tracer-test-{suffix}");
    let dir: PathBuf = Path::new(env!("CARGO_TARGET_TMPDIR")).join(&session_id);
    std::fs::create_dir_all(&dir).context("creating test dir")?;

    let mut tracer = spawn_tracer(&session_id, &pipe_name, &dir)?;
    let stream = connect(&pipe_name).await?;
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

    tokio::time::timeout(READY_TIMEOUT, wait_for_ready(&mut reader, &mut write_half))
        .await
        .context("child never printed the ready marker")??;

    write_line(&mut write_half, &TracerRequest::Stop).await?;

    let exited = wait_for_exit(&mut tracer).await?;
    if exited.is_none() {
        let stacks = capture_stacks(&tracer, &dir).await;
        bail!(
            "tracer did not exit within {EXIT_TIMEOUT:?} after Stop (log: {}, {stacks})",
            dir.join("tracer.log").display()
        );
    }
    drop(tracer);
    remove_test_dir(&dir).await
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
/// can lag the tracer's own exit by a moment, so removal is retried briefly.
async fn remove_test_dir(dir: &Path) -> anyhow::Result<()> {
    let started = Instant::now();
    loop {
        match std::fs::remove_dir_all(dir) {
            Ok(()) => return Ok(()),
            Err(err) if started.elapsed() >= CLEANUP_TIMEOUT => {
                return Err(err).with_context(|| format!("removing {}", dir.display()));
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}
