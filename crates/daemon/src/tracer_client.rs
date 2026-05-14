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

use crate::binary_cache;
use crate::paths::Dirs;
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
const OUTPUT_SUBSCRIBER_WAIT_TIMEOUT: Duration = Duration::from_secs(5);
const OUTPUT_SUBSCRIBER_WAIT_INTERVAL: Duration = Duration::from_millis(5);
const TRACER_PIPE_PREFIX_ENV: &str = "RUSTLING_TULIP_TRACER_PIPE_PREFIX";

/// The result of spawning (or reattaching to) a tracer for a single session.
pub struct TracerSpawn {
    /// Handle the rest of the daemon uses as if it were a direct PTY.
    pub handle: Arc<PtyHandle>,
    /// OS pid of the `rt-tracer.exe` process. Persisted to the orphan sidecar
    /// so a future daemon can detect a still-live tracer to reattach to.
    pub tracer_pid: u32,
    /// Pipe name persisted to the sidecar; not derivable from session id alone
    /// when a launcher supplies a per-instance pipe namespace.
    pub pipe_name: String,
    /// Absolute path of the cached `rt-tracer.exe` this supervisor was spawned
    /// from (see [`crate::binary_cache`]). Persisted to the sidecar so the
    /// daemon's startup GC sweep can tell which cache entries are still in
    /// use.
    pub tracer_exe_path: PathBuf,
}

/// Spawn `rt-tracer.exe` for this session and wire up its pipe. The tracer
/// inherits no console (Windows: `CREATE_NO_WINDOW`) and is detached from the
/// daemon's lifetime — when the daemon dies the tracer keeps owning its PTY
/// child until either a new daemon reconnects or it receives `Stop`.
///
/// `dirs` is consulted to copy the shipped `rt-tracer` template into the
/// content-addressed binary cache; the supervisor is launched from the cached
/// copy rather than the template itself so a rebuild or reinstall can replace
/// the template without disturbing live sessions.
pub async fn spawn(
    dirs: &Dirs,
    spec: PtySpawnSpec,
    expected_output_subscribers: usize,
) -> anyhow::Result<TracerSpawn> {
    let tracer_path = locate_tracer_exe(dirs)?;
    let pipe = tracer_pipe_name(&spec.session_id);
    debug!(
        tracer = %tracer_path.display(),
        pipe = %pipe,
        program = %spec.program,
        "tracer_client: spawning tracer"
    );

    let tracer_pid = spawn_tracer_process(&tracer_path, &spec, &pipe)?;
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
        tracer_exe_path: tracer_path,
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

/// Resolve the cached `rt-tracer` path, copying the shipped template into
/// the binary cache on first use. Returns the cached path the tracer should
/// be spawned from. Each rebuild of the template lands at a new hash, so
/// existing tracer processes keep running their own private copies.
fn locate_tracer_exe(dirs: &Dirs) -> anyhow::Result<PathBuf> {
    let template = locate_tracer_template()?;
    let stem = template
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("rt-tracer");
    binary_cache::ensure_cached(&template, &dirs.binaries_dir, stem)
}

fn tracer_pipe_name(session_id: &str) -> String {
    let prefix = std::env::var(TRACER_PIPE_PREFIX_ENV).ok();
    pipe_name_with_prefix(session_id, prefix.as_deref())
}

fn pipe_name_with_prefix(session_id: &str, prefix: Option<&str>) -> String {
    if let Some(prefix) = prefix.and_then(sanitize_pipe_prefix) {
        return format!(r"\\.\pipe\{prefix}-{session_id}");
    }
    pipe_name(session_id)
}

fn sanitize_pipe_prefix(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len().min(80));
    for ch in raw.chars() {
        if out.len() >= 80 {
            break;
        }
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
            out.push(ch);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn locate_tracer_template() -> anyhow::Result<PathBuf> {
    // Preferred: the directory Tauri's supervisor told us holds the original
    // templates. Required when the daemon is running from
    // `<binaries_dir>/rustling-tulipd-<hash>.exe` (the post-cache layout),
    // because the tracer template lives in the install dir / target dir, not
    // next to the cached daemon.
    if let Ok(value) = std::env::var("RUSTLING_TULIP_BIN_TEMPLATES")
        && !value.is_empty()
    {
        let candidate = PathBuf::from(value).join(tracer_exe_name());
        if candidate.is_file() {
            return Ok(candidate);
        }
        return Err(anyhow!(
            "rt-tracer not found at RUSTLING_TULIP_BIN_TEMPLATES location: {}",
            candidate.display()
        ));
    }

    // Fallback: direct `cargo run -p daemon` (no supervisor). The daemon was
    // launched from `target/<profile>/` and rt-tracer.exe sits next to it.
    let current = std::env::current_exe().context("locating daemon exe")?;
    let dir = current
        .parent()
        .ok_or_else(|| anyhow!("daemon exe has no parent dir"))?;
    let candidate = dir.join(tracer_exe_name());
    if candidate.is_file() {
        return Ok(candidate);
    }
    Err(anyhow!(
        "rt-tracer binary not found (no RUSTLING_TULIP_BIN_TEMPLATES, sibling lookup missed): {}",
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
fn spawn_tracer_process(
    tracer: &Path,
    spec: &PtySpawnSpec,
    pipe_name: &str,
) -> anyhow::Result<u32> {
    use std::os::windows::process::CommandExt;
    /// `CREATE_NO_WINDOW` — suppresses the console window that would otherwise
    /// flash for the tracer process. The tracer's tracing output goes to
    /// stderr; with `RUSTLING_TULIP_TRACER_LOG` set the tracer routes its
    /// `tracing_subscriber` output to a file instead.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let mut cmd = std::process::Command::new(tracer);
    cmd.arg("--session-id")
        .arg(&spec.session_id)
        .arg("--pipe-name")
        .arg(pipe_name)
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
    let dir = if let Ok(value) = std::env::var("RUSTLING_TULIP_CONFIG_DIR")
        && !value.is_empty()
    {
        PathBuf::from(value).join("logs")
    } else {
        let pd = directories::ProjectDirs::from("dev", "leftos", "rustling-tulip")?;
        pd.config_dir().join("logs")
    };
    if std::fs::create_dir_all(&dir).is_err() {
        return None;
    }
    Some(dir.join(format!("tracer-{session_id}.log")))
}

#[cfg(not(windows))]
fn spawn_tracer_process(
    tracer: &Path,
    spec: &PtySpawnSpec,
    pipe_name: &str,
) -> anyhow::Result<u32> {
    let mut cmd = std::process::Command::new(tracer);
    cmd.arg("--session-id")
        .arg(&spec.session_id)
        .arg("--pipe-name")
        .arg(pipe_name)
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
    enum PendingPipeState {
        Missing,
        Busy,
    }

    let started = Instant::now();
    let mut pending: PendingPipeState;
    loop {
        match ClientOptions::new().open(pipe) {
            Ok(c) => return Ok(c),
            Err(err) if err.raw_os_error() == Some(2) => {
                // ERROR_FILE_NOT_FOUND — tracer hasn't created the pipe yet.
                // Retry until the timeout.
                pending = PendingPipeState::Missing;
            }
            Err(err) if err.raw_os_error() == Some(231) => {
                // ERROR_PIPE_BUSY — a concurrent daemon restart can briefly
                // claim the pipe before the surviving daemon settles.
                pending = PendingPipeState::Busy;
            }
            Err(err) => return Err(err).context(format!("opening tracer pipe {pipe}")),
        }
        if started.elapsed() >= PIPE_CONNECT_TIMEOUT {
            return match pending {
                PendingPipeState::Missing => Err(anyhow!(
                    "tracer pipe {pipe} did not appear within {PIPE_CONNECT_TIMEOUT:?}"
                )),
                PendingPipeState::Busy => Err(anyhow!(
                    "tracer pipe {pipe} stayed busy for {PIPE_CONNECT_TIMEOUT:?}"
                )),
            };
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
        let started = Instant::now();
        let mut waits = 0_u32;
        while output_for_reader.receiver_count() < expected_output_subscribers {
            if started.elapsed() >= OUTPUT_SUBSCRIBER_WAIT_TIMEOUT {
                // Avoid deadlocking the session if a caller regresses and
                // forgets to install one of the expected watchers.
                tracing::warn!(
                    session_id = %session_id_for_reader,
                    expected_output_subscribers,
                    subscriber_count = output_for_reader.receiver_count(),
                    elapsed = ?started.elapsed(),
                    "tracer_client: missing expected subscribers before timeout; starting reader anyway",
                );
                break;
            }
            tokio::time::sleep(OUTPUT_SUBSCRIBER_WAIT_INTERVAL).await;
            waits += 1;
        }
        tracing::debug!(
            session_id = %session_id_for_reader,
            waits,
            subscriber_count = output_for_reader.receiver_count(),
            elapsed = ?started.elapsed(),
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

#[cfg(test)]
mod tests {
    use super::{pipe_name_with_prefix, sanitize_pipe_prefix};

    #[test]
    fn pipe_prefix_is_sanitized() {
        assert_eq!(
            pipe_name_with_prefix("session-1", Some("rt/e2e:test")),
            r"\\.\pipe\rt-e2e-test-session-1",
        );
    }

    #[test]
    fn empty_pipe_prefix_uses_default_protocol_name() {
        assert_eq!(
            pipe_name_with_prefix("session-1", Some("///")),
            tracer_protocol::pipe_name("session-1"),
        );
    }

    #[test]
    fn sanitize_pipe_prefix_limits_length() {
        assert_eq!(
            sanitize_pipe_prefix(&"a".repeat(100))
                .as_deref()
                .map(str::len),
            Some(80),
        );
    }
}
