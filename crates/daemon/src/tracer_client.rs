//! Daemon-side client for a per-session `rt-tracer.exe` supervisor.
//!
//! Replaces the direct `ConPTY` spawn that lived in `pty::spawn` before Phase C.
//! Every interactive and plain-shell PTY child now lives under a tracer that
//! owns the master handle. The tracer survives daemon restarts (Phase C's
//! headline goal); the daemon reconnects to its pipe on startup to resume
//! a live session via [`reattach`].
//!
//! The returned [`TracerSpawn`] hands back a [`PtyHandle`] whose surface is
//! identical to the pre-tracer world — call sites stay agnostic to whether
//! the bytes are coming from a local `ConPTY` or a remote pipe.

use crate::pty::{PtyHandle, PtyHandleParts, PtySpawnSpec};
use anyhow::{Context as _, anyhow};
use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use portable_pty::ChildKiller;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt as _, AsyncWrite, AsyncWriteExt as _, BufReader};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};
use tokio::sync::{broadcast, mpsc, oneshot};
use tracer_protocol::{
    InboundTracerResponse, SUPPORTED_TRACER_VERSIONS, TRACER_VERSION, TracerHello, TracerRequest,
    TracerResponse, TracerWelcome, negotiate, pipe_name,
};
use tracing::{debug, info, warn};

const OUTPUT_BROADCAST_CAPACITY: usize = 256;
const PIPE_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const PIPE_RETRY_INTERVAL: Duration = Duration::from_millis(50);

/// The result of spawning (or reattaching to) a tracer for a single session.
pub struct TracerSpawn {
    /// Handle the rest of the daemon uses as if it were a direct PTY.
    pub handle: Arc<PtyHandle>,
    /// OS pid of the `rt-tracer.exe` process. Persisted to the orphan sidecar
    /// so a future daemon can detect a still-live tracer to reattach to.
    pub tracer_pid: u32,
    /// Pipe name (matches `tracer_protocol::pipe_name(session_id)`). Persisted
    /// to the sidecar; not derivable from session id alone if the ABI ever
    /// changes the format.
    pub pipe_name: String,
}

/// Spawn `rt-tracer.exe` for this session and wire up its pipe. The tracer
/// inherits no console (Windows: `CREATE_NO_WINDOW`) and is detached from the
/// daemon's lifetime — when the daemon dies the tracer keeps owning its PTY
/// child until either a new daemon reconnects or it receives `Stop`.
pub async fn spawn(
    spec: PtySpawnSpec,
    expected_output_subscribers: usize,
) -> anyhow::Result<TracerSpawn> {
    let tracer_path = locate_tracer_exe()?;
    let pipe = pipe_name(&spec.session_id);
    debug!(
        tracer = %tracer_path.display(),
        pipe = %pipe,
        program = %spec.program,
        "tracer_client: spawning tracer"
    );

    let tracer_pid = spawn_tracer_process(&tracer_path, &spec)?;
    info!(tracer_pid, pipe = %pipe, "tracer_client: tracer process started");

    let client = connect_with_retry(&pipe).await?;
    let (handle, _negotiated_version) = handshake_and_wire(
        client,
        spec.session_id.clone(),
        Some(tracer_pid),
        expected_output_subscribers,
    )
    .await?;

    Ok(TracerSpawn {
        handle,
        tracer_pid,
        pipe_name: pipe,
    })
}

/// Reattach to an already-running tracer (orphan-recovery path). The tracer
/// pid is treated as authoritative input — caller is expected to have verified
/// liveness via `is_session_alive`-style check before calling this.
pub async fn reattach(
    session_id: &str,
    pipe: &str,
    tracer_pid: u32,
    expected_output_subscribers: usize,
) -> anyhow::Result<Arc<PtyHandle>> {
    let client = connect_with_retry(pipe).await?;
    let (handle, _negotiated_version) = handshake_and_wire(
        client,
        session_id.to_string(),
        Some(tracer_pid),
        expected_output_subscribers,
    )
    .await?;
    Ok(handle)
}

fn locate_tracer_exe() -> anyhow::Result<PathBuf> {
    let current = std::env::current_exe().context("locating daemon exe")?;
    let dir = current
        .parent()
        .ok_or_else(|| anyhow!("daemon exe has no parent dir"))?;
    let candidate = dir.join(tracer_exe_name());
    if candidate.is_file() {
        return Ok(candidate);
    }
    // Dev fallback: cargo target/<profile>/ — useful for `cargo run -p daemon`
    // before the installer is built. In production the side-by-side path above
    // hits.
    Err(anyhow!(
        "rt-tracer binary not found next to daemon: {}",
        candidate.display()
    ))
}

#[cfg(windows)]
const fn tracer_exe_name() -> &'static str {
    "rt-tracer.exe"
}

#[cfg(not(windows))]
const fn tracer_exe_name() -> &'static str {
    "rt-tracer"
}

#[cfg(windows)]
fn spawn_tracer_process(tracer: &Path, spec: &PtySpawnSpec) -> anyhow::Result<u32> {
    use std::os::windows::process::CommandExt;
    /// `CREATE_NO_WINDOW` — suppresses the console window that would otherwise
    /// flash for the tracer process. The tracer's tracing output goes to
    /// stderr; with `RUSTLING_TULIP_TRACER_LOG` set the tracer routes its
    /// `tracing_subscriber` output to a file instead.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let mut cmd = std::process::Command::new(tracer);
    cmd.arg("--session-id")
        .arg(&spec.session_id)
        .arg("--cwd")
        .arg(&spec.cwd)
        .arg("--cols")
        .arg(spec.cols.to_string())
        .arg("--rows")
        .arg(spec.rows.to_string());
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    // Per-session tracer log so we can debug spawn issues for things like
    // `.cmd` shims and shebang scripts that CreateProcess handles weirdly.
    // Best-effort: if the log dir can't be resolved (no APPDATA on a weird
    // host, locked filesystem, etc.) we silently skip — losing logs is not
    // a reason to fail a session spawn.
    if let Some(log_path) = tracer_log_path(&spec.session_id) {
        cmd.env("RUSTLING_TULIP_TRACER_LOG", &log_path);
    }
    // Trailing program-and-args: program first, then its args. portable-pty
    // expects this shape on the tracer side.
    cmd.arg(&spec.program);
    for arg in &spec.args {
        cmd.arg(arg);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW);

    let child = cmd
        .spawn()
        .with_context(|| format!("spawning tracer process {}", tracer.display()))?;
    Ok(child.id())
}

/// Build the per-session tracer log path under `<config>/logs/`. Uses the
/// same `directories` resolution as `paths::Dirs` so both processes see
/// the same root.
fn tracer_log_path(session_id: &str) -> Option<PathBuf> {
    let pd = directories::ProjectDirs::from("dev", "leftos", "rustling-tulip")?;
    let dir = pd.config_dir().join("logs");
    if std::fs::create_dir_all(&dir).is_err() {
        return None;
    }
    Some(dir.join(format!("tracer-{session_id}.log")))
}

#[cfg(not(windows))]
fn spawn_tracer_process(tracer: &Path, spec: &PtySpawnSpec) -> anyhow::Result<u32> {
    let mut cmd = std::process::Command::new(tracer);
    cmd.arg("--session-id")
        .arg(&spec.session_id)
        .arg("--cwd")
        .arg(&spec.cwd)
        .arg("--cols")
        .arg(spec.cols.to_string())
        .arg("--rows")
        .arg(spec.rows.to_string());
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    if let Some(log_path) = tracer_log_path(&spec.session_id) {
        cmd.env("RUSTLING_TULIP_TRACER_LOG", &log_path);
    }
    cmd.arg(&spec.program);
    for arg in &spec.args {
        cmd.arg(arg);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = cmd
        .spawn()
        .with_context(|| format!("spawning tracer process {}", tracer.display()))?;
    Ok(child.id())
}

async fn connect_with_retry(pipe: &str) -> anyhow::Result<NamedPipeClient> {
    let started = Instant::now();
    loop {
        match ClientOptions::new().open(pipe) {
            Ok(c) => return Ok(c),
            Err(err) if err.raw_os_error() == Some(2) => {
                // ERROR_FILE_NOT_FOUND — tracer hasn't created the pipe yet.
                // Retry until the timeout.
            }
            Err(err) if err.raw_os_error() == Some(231) => {
                // ERROR_PIPE_BUSY — another daemon attached first.
                return Err(anyhow!(
                    "tracer pipe {pipe} is already in use by another daemon (ERROR_PIPE_BUSY)"
                ));
            }
            Err(err) => return Err(err).context(format!("opening tracer pipe {pipe}")),
        }
        if started.elapsed() >= PIPE_CONNECT_TIMEOUT {
            return Err(anyhow!(
                "tracer pipe {pipe} did not appear within {PIPE_CONNECT_TIMEOUT:?}"
            ));
        }
        tokio::time::sleep(PIPE_RETRY_INTERVAL).await;
    }
}

async fn handshake_and_wire(
    client: NamedPipeClient,
    session_id: String,
    pid_for_handle: Option<u32>,
    expected_output_subscribers: usize,
) -> anyhow::Result<(Arc<PtyHandle>, u32)> {
    let (reader, writer) = tokio::io::split(client);
    let mut reader = BufReader::new(reader);
    let mut writer = writer;

    // Send TracerHello.
    let hello = TracerHello {
        version: TRACER_VERSION,
        supported: SUPPORTED_TRACER_VERSIONS.to_vec(),
    };
    write_struct(&mut writer, &hello).await?;

    // Read TracerWelcome.
    let mut welcome_line = String::new();
    let n = reader
        .read_line(&mut welcome_line)
        .await
        .context("reading TracerWelcome")?;
    if n == 0 {
        return Err(anyhow!("tracer closed pipe before Welcome"));
    }
    let welcome: TracerWelcome =
        serde_json::from_str(welcome_line.trim_end()).context("parsing TracerWelcome")?;
    let negotiated = negotiate(welcome.version, &welcome.supported)
        .or_else(|| negotiate(welcome.version, SUPPORTED_TRACER_VERSIONS))
        .ok_or_else(|| anyhow!("no mutually supported tracer version"))?;
    debug!(
        version = negotiated,
        "tracer_client: tracer handshake complete"
    );

    // Wire the four daemon-facing channels.
    let (output_tx, _) = broadcast::channel::<Vec<u8>>(OUTPUT_BROADCAST_CAPACITY);
    let (input_tx, input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (resize_tx, resize_rx) = mpsc::unbounded_channel::<(u16, u16)>();
    let (stop_tx, stop_rx) = mpsc::unbounded_channel::<()>();
    let (exit_tx, exit_rx) = oneshot::channel::<i32>();

    // Reader task: parse TracerResponse frames, route to output broadcast +
    // exit oneshot. On stream EOF or parse error, transition exit to -1.
    //
    // Critical: yield until every startup subscriber has called
    // `output_tx.subscribe()` before starting the pipe read loop. Otherwise
    // the first Output frame (the tracer's ring snapshot containing shell
    // startup bytes) can be consumed by an early watcher before
    // `attach_lifecycle` has subscribed, leaving scrollback empty and the
    // terminal blank until the child writes more output.
    let output_for_reader = output_tx.clone();
    let session_id_for_reader = session_id.clone();
    tokio::spawn(async move {
        let mut waits = 0_u32;
        while output_for_reader.receiver_count() < expected_output_subscribers {
            tokio::task::yield_now().await;
            waits += 1;
            if waits >= 1_000 {
                // Avoid deadlocking the session if a caller regresses and
                // forgets to install one of the expected watchers.
                tracing::warn!(
                    session_id = %session_id_for_reader,
                    expected_output_subscribers,
                    subscriber_count = output_for_reader.receiver_count(),
                    "tracer_client: missing expected subscribers after 1000 yields; starting reader anyway",
                );
                break;
            }
        }
        tracing::debug!(
            session_id = %session_id_for_reader,
            waits,
            subscriber_count = output_for_reader.receiver_count(),
            "tracer_client: reader starting"
        );
        let exit_code = read_loop(reader, output_for_reader, &session_id_for_reader).await;
        let _ = exit_tx.send(exit_code);
    });

    // Writer task: pump input/resize/stop into the pipe.
    tokio::spawn(async move {
        write_loop(writer, input_rx, resize_rx, stop_rx).await;
    });

    // Custom ChildKiller that sends Stop. Cloneable so PtyHandle's killer
    // can be inspected if anything ever calls clone_killer().
    let killer: Box<dyn ChildKiller + Send + Sync> = Box::new(TracerKiller { stop_tx });

    let handle = Arc::new(PtyHandle::from_parts(PtyHandleParts {
        output: output_tx,
        input_tx,
        resize_tx,
        exit_rx,
        killer,
        // Sidecar's `pid` field gets the tracer's pid so the legacy liveness
        // check (`pid` + `program_name`) still resolves to a live process post-
        // C.3. The explicit `tracer_pid` field carries the same value for
        // forward-compatible reattach.
        pid: pid_for_handle,
    }));
    Ok((handle, negotiated))
}

async fn read_loop(
    mut reader: BufReader<tokio::io::ReadHalf<NamedPipeClient>>,
    output_tx: broadcast::Sender<Vec<u8>>,
    session_id: &str,
) -> i32 {
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => {
                debug!(%session_id, "tracer_client: pipe EOF");
                return -1;
            }
            Ok(_) => {}
            Err(err) => {
                debug!(?err, %session_id, "tracer_client: pipe read error");
                return -1;
            }
        }
        let parsed = match InboundTracerResponse::from_json_str(line.trim_end()) {
            Ok(p) => p,
            Err(err) => {
                warn!(?err, line = %line.trim_end(), "tracer_client: unparseable frame");
                continue;
            }
        };
        match parsed {
            InboundTracerResponse::Known(TracerResponse::Output { data_b64 }) => {
                match B64.decode(&data_b64) {
                    Ok(bytes) => {
                        let _ = output_tx.send(bytes);
                    }
                    Err(err) => warn!(?err, "tracer_client: bad base64 in Output"),
                }
            }
            InboundTracerResponse::Known(TracerResponse::Status {
                child_pid,
                child_alive,
                ring_bytes,
            }) => {
                debug!(
                    ?child_pid,
                    child_alive, ring_bytes, "tracer_client: status frame"
                );
            }
            InboundTracerResponse::Known(TracerResponse::Exited { code }) => {
                info!(%session_id, code, "tracer_client: child exited");
                return code;
            }
            InboundTracerResponse::Known(TracerResponse::Error { message }) => {
                warn!(%session_id, %message, "tracer_client: tracer error frame");
            }
            InboundTracerResponse::Unknown { type_tag, .. } => {
                debug!(%type_tag, "tracer_client: unknown tracer frame");
            }
        }
    }
}

async fn write_loop(
    mut writer: tokio::io::WriteHalf<NamedPipeClient>,
    mut input_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    mut resize_rx: mpsc::UnboundedReceiver<(u16, u16)>,
    mut stop_rx: mpsc::UnboundedReceiver<()>,
) {
    loop {
        let req = tokio::select! {
            Some(bytes) = input_rx.recv() => TracerRequest::Input { data_b64: B64.encode(&bytes) },
            Some((cols, rows)) = resize_rx.recv() => TracerRequest::Resize { cols, rows },
            Some(()) = stop_rx.recv() => TracerRequest::Stop,
            else => return,
        };
        if let Err(err) = write_struct(&mut writer, &req).await {
            warn!(?err, "tracer_client: pipe write failed; ending write loop");
            return;
        }
    }
}

async fn write_struct<W, T>(w: &mut W, msg: &T) -> anyhow::Result<()>
where
    W: AsyncWrite + Unpin,
    T: serde::Serialize,
{
    let mut line = serde_json::to_string(msg).context("serializing tracer frame")?;
    line.push('\n');
    w.write_all(line.as_bytes())
        .await
        .context("writing tracer frame")?;
    Ok(())
}

#[derive(Debug)]
struct TracerKiller {
    stop_tx: mpsc::UnboundedSender<()>,
}

impl ChildKiller for TracerKiller {
    fn kill(&mut self) -> io::Result<()> {
        self.stop_tx
            .send(())
            .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e.to_string()))?;
        Ok(())
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(Self {
            stop_tx: self.stop_tx.clone(),
        })
    }
}
