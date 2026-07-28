//! Top-level supervisor loop. Spawns the child via PTY, hosts the local-socket
//! server (`interprocess`: named pipe on Windows, Unix domain socket on
//! macOS/Linux), and multiplexes child output → ring/socket and socket → child
//! stdin.
//!
//! Design notes:
//! - The PTY reader runs in a dedicated `std::thread`; output bytes are pushed
//!   to a bounded ring and, when a daemon is attached, forwarded over the socket
//!   with backpressure. A slow daemon therefore slows the child rather than
//!   ballooning memory.
//! - Only one daemon attaches at a time. The accept loop is serialized — it
//!   runs `handle_client` to completion before accepting the next connection —
//!   so a racing daemon waits (in the OS backlog / connect-retry) until the
//!   current one drops. A daemon restart drops the old connection (EOF on the
//!   read half), `handle_client` returns, and the next accept picks up the new
//!   daemon.
//! - On reconnect, the freshly-attached daemon receives a single ring-snapshot
//!   Output frame as the first message after `Welcome` so it can replay what
//!   it missed during the outage. After the snapshot, live output is forwarded
//!   as it arrives.

use crate::ring::OutputRing;
use anyhow::Context as _;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use interprocess::local_socket::ListenerOptions;
use interprocess::local_socket::tokio::prelude::*;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt as _, AsyncRead, AsyncWrite, AsyncWriteExt as _, BufReader};
use tokio::sync::{broadcast, mpsc};
use tracer_protocol::{
    InboundTracerRequest, SUPPORTED_TRACER_VERSIONS, TRACER_VERSION, TracerHello, TracerRequest,
    TracerResponse, TracerWelcome, negotiate,
};
use tracing::{debug, error, info, warn};

/// Bound on the pipe-output channel. Once the daemon is connected the PTY
/// reader pushes chunks into this channel; if the daemon stops draining it,
/// the reader backpressures (slowing the child) rather than letting memory
/// grow unbounded.
const PIPE_OUTPUT_CHANNEL_CAPACITY: usize = 64;

/// Paste pacing (input path). Windows `ConPTY` translates input-pipe bytes into
/// console `INPUT_RECORD`s in the child's console input buffer, and that buffer
/// is bounded. A large paste shoved at the master in one fast burst overruns it
/// and `ConPTY` silently drops records in the middle ("top and bottom present,
/// middle gone"). Windows Terminal works around the same limitation by pacing
/// its paste feed; we do the same. Any single write larger than
/// `PACING_CHUNK_BYTES` is sub-chunked and each sub-chunk is followed by a short
/// sleep so the child can drain between writes. Writes at or below the threshold
/// (ordinary keystrokes, control sequences) go through in one shot with no sleep
/// so normal typing stays instant. These live here, at the deepest point of the
/// send path, so the pacing holds regardless of how the daemon frames input.
const PACING_CHUNK_BYTES: usize = 1024;
const PACING_DELAY: Duration = Duration::from_millis(10);

#[derive(Debug, Clone)]
pub struct Config {
    pub session_id: String,
    /// `interprocess` local-socket name (Windows namespaced name / Unix socket
    /// path). Field name kept as `pipe_name` to match the daemon's `--pipe-name`
    /// CLI arg and the sidecar's `tracer_pipe` field.
    pub pipe_name: String,
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub cols: u16,
    pub rows: u16,
}

/// Shared state between the PTY reader thread and the pipe handler.
struct SharedOutput {
    ring: OutputRing,
    /// Set by the pipe handler when a daemon attaches; cleared when it
    /// disconnects. The PTY reader uses it to forward live output.
    attached: Option<mpsc::Sender<Vec<u8>>>,
}

/// Boot the supervisor: spawn the PTY child, set up the ring, host the pipe.
/// Returns when the child exits.
#[expect(
    clippy::too_many_lines,
    reason = "supervisor::run wires up reader thread, writer thread, resize task, child waiter, \
              stop handler, pipe accept loop, and shutdown in one place; splitting them apart \
              would force every helper to grow its own channel signature for marginal clarity gain"
)]
pub async fn run(cfg: Config) -> anyhow::Result<()> {
    info!(
        session_id = %cfg.session_id,
        pipe = %cfg.pipe_name,
        program = %cfg.program,
        cols = cfg.cols,
        rows = cfg.rows,
        "supervisor: starting"
    );

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: cfg.rows.max(2),
            cols: cfg.cols.max(2),
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("opening PTY")?;

    info!(
        program = %cfg.program,
        argc = cfg.args.len(),
        args = ?cfg.args,
        cwd = %cfg.cwd.display(),
        "supervisor: about to spawn child"
    );

    let mut cmd = CommandBuilder::new(&cfg.program);
    for arg in &cfg.args {
        cmd.arg(arg);
    }
    cmd.cwd(cfg.cwd.as_path());
    // portable-pty's CommandBuilder::as_command() calls env_clear() before
    // applying the builder's env map, so the child receives ZERO inherited
    // vars unless we explicitly add them. Forward this process's env (which
    // the daemon already populated via tracer_client::spawn with the
    // passthrough keep-list: PATH, APPDATA, USERPROFILE, etc.). Skip the
    // tracer-internal log var so it doesn't leak into the child env.
    let mut forwarded_env_count = 0_usize;
    for (k, v) in std::env::vars_os() {
        if k == "RUSTLING_TULIP_TRACER_LOG" {
            continue;
        }
        cmd.env(&k, &v);
        forwarded_env_count += 1;
    }
    info!(
        forwarded_env_count,
        "supervisor: forwarded process env to child"
    );

    let mut child = match cmd_spawn(pair.slave.as_ref(), cmd) {
        Ok(c) => c,
        Err(err) => {
            // Logged here in addition to the `?` propagation so the
            // tracer log captures the actual program/arg shape that
            // failed — the daemon only sees the pipe-connect timeout.
            tracing::error!(
                ?err,
                program = %cfg.program,
                args = ?cfg.args,
                "supervisor: child spawn failed"
            );
            return Err(err);
        }
    };
    drop(pair.slave);
    let child_pid = child.process_id();
    info!(?child_pid, "supervisor: child spawned");

    // Bind the child (and every descendant) to a Job object with
    // KILL_ON_JOB_CLOSE so killing the PTY child — or the tracer just
    // exiting — takes out the entire process tree. Without this, claude
    // grandchildren (node.exe from pnpm install, cargo from a build,
    // dev servers, the user's editor opened through `code .`) survive
    // TerminateProcess on the PTY child and pin worktree files open,
    // breaking the worktree cleanup path. Best-effort: if anything
    // here fails we still proceed without the job (legacy behavior).
    // The job handle is kept alive in `_job_guard` for the lifetime of
    // this fn — when the tracer process exits, the OS closes the
    // handle, KILL_ON_JOB_CLOSE fires, and every descendant goes with
    // it.
    #[cfg(windows)]
    let _job_guard = if let Some(pid) = child_pid {
        match crate::job_object::assign_kill_on_close(pid) {
            Ok(guard) => Some(guard),
            Err(err) => {
                tracing::warn!(
                    ?err,
                    ?pid,
                    "supervisor: could not bind child to kill-on-close job; \
                     grandchildren may survive kill"
                );
                None
            }
        }
    } else {
        tracing::warn!(
            "supervisor: portable-pty returned no child pid; \
             skipping job-object grouping"
        );
        None
    };

    let mut reader = pair
        .master
        .try_clone_reader()
        .context("cloning PTY master reader")?;
    let writer = pair
        .master
        .take_writer()
        .context("taking PTY master writer")?;
    let master = Arc::new(Mutex::new(pair.master));

    let shared = Arc::new(Mutex::new(SharedOutput {
        ring: OutputRing::new(),
        attached: None,
    }));

    // PTY reader thread: push to ring; if a daemon is attached, forward bytes
    // through the bounded channel. `blocking_send` backpressures the reader
    // (and through it, the child) when the daemon is slow to drain.
    let shared_for_reader = Arc::clone(&shared);
    let reader_handle = std::thread::spawn(move || {
        debug!("supervisor: PTY reader thread started; about to enter read loop");
        let mut buf = [0u8; 4096];
        let mut total_bytes = 0u64;
        let mut read_count = 0u64;
        loop {
            match reader.read(&mut buf) {
                Ok(0) => {
                    debug!(
                        total_bytes,
                        read_count, "supervisor: PTY reader EOF; exiting"
                    );
                    break;
                }
                Ok(n) => {
                    read_count += 1;
                    total_bytes += n as u64;
                    if read_count == 1 {
                        let preview_len = n.min(64);
                        debug!(
                            n,
                            preview = %String::from_utf8_lossy(&buf[..preview_len]).escape_debug().to_string(),
                            "supervisor: first PTY read"
                        );
                    }
                    let chunk = buf[..n].to_vec();
                    let attached = {
                        let Ok(mut state) = shared_for_reader.lock() else {
                            warn!("supervisor: shared state poisoned; exiting reader");
                            break;
                        };
                        state.ring.push(&chunk);
                        state.attached.clone()
                    };
                    if let Some(tx) = attached
                        && tx.blocking_send(chunk).is_err()
                    {
                        // Receiver dropped; pipe handler will reset attached
                        // when it sees disconnect. Just continue — bytes still
                        // land in the ring for next reconnect.
                        debug!("supervisor: pipe sender closed; bytes buffered in ring");
                    }
                }
                Err(err) => {
                    warn!(
                        ?err,
                        total_bytes, read_count, "supervisor: PTY reader error; exiting"
                    );
                    break;
                }
            }
        }
    });

    // PTY writer thread: serializes input writes onto the master.
    let (pty_input_tx, mut pty_input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let writer_handle = std::thread::spawn(move || {
        debug!("supervisor: PTY writer thread started");
        let mut writer = writer;
        while let Some(bytes) = block_on_recv(&mut pty_input_rx) {
            // Deepest point in the send path: the actual write onto the ConPTY
            // master. Large writes are paced (sub-chunked + slept between
            // sub-chunks) to avoid overrunning ConPTY's console input buffer;
            // see `PACING_CHUNK_BYTES` / `PACING_DELAY`. `write_all` loops until
            // every byte is accepted onto the pipe, so a byte count that reaches
            // here intact but still shows loss downstream would implicate ConPTY
            // itself. Paste-sized writes only (kept at debug — the pacing fix is
            // in place, but the trail stays available for re-diagnosis).
            if bytes.len() > 16 {
                debug!(bytes = bytes.len(), "supervisor: writing to ConPTY master");
            }
            let paced = bytes.len() > PACING_CHUNK_BYTES;
            let mut write_err = None;
            for sub in bytes.chunks(PACING_CHUNK_BYTES) {
                if let Err(err) = std::io::Write::write_all(&mut writer, sub) {
                    write_err = Some(err);
                    break;
                }
                let _ = std::io::Write::flush(&mut writer);
                if paced {
                    std::thread::sleep(PACING_DELAY);
                }
            }
            if let Some(err) = write_err {
                warn!(?err, "supervisor: PTY writer error; exiting");
                break;
            }
        }
    });
    // Detached; exits when input_tx is dropped.
    drop(writer_handle);

    // Resize forwarding: pipe input → PTY master.
    let (resize_tx, mut resize_rx) = mpsc::unbounded_channel::<(u16, u16)>();
    let master_for_resize = Arc::clone(&master);
    tokio::spawn(async move {
        while let Some((cols, rows)) = resize_rx.recv().await {
            let m = Arc::clone(&master_for_resize);
            let _ = tokio::task::spawn_blocking(move || {
                let Ok(guard) = m.lock() else {
                    return;
                };
                if let Err(err) = guard.resize(PtySize {
                    rows: rows.max(2),
                    cols: cols.max(2),
                    pixel_width: 0,
                    pixel_height: 0,
                }) {
                    debug!(?err, "supervisor: resize failed");
                }
            })
            .await;
        }
    });

    // Child waiter: blocks on child.wait(), broadcasts the exit code so any
    // attached pipe handler can emit a final `Exited` frame before closing.
    let (exit_broadcast_tx, _exit_broadcast_rx) = broadcast::channel::<i32>(4);
    let exit_broadcast_for_child = exit_broadcast_tx.clone();
    let mut child_killer = child.clone_killer();
    let child_wait = tokio::task::spawn_blocking(move || {
        debug!("supervisor: child waiter blocking on child.wait()");
        let status = child.wait();
        let code = match status {
            Ok(s) => i32::try_from(s.exit_code()).unwrap_or(-1),
            Err(err) => {
                warn!(?err, "supervisor: child wait failed");
                -1
            }
        };
        debug!(code, "supervisor: child waiter observed exit");
        let _ = exit_broadcast_for_child.send(code);
        code
    });

    // Liveness heartbeat: surfaces "child spawned but never wrote a byte"
    // hangs without flooding the log during normal use. Polls every 5 s for
    // 30 s after spawn; if the ring is still empty by then, logs a single
    // WARN and gives up. Once any byte arrives, heartbeats fall silent.
    let shared_for_heartbeat = Arc::clone(&shared);
    let heartbeat_session = cfg.session_id.clone();
    let heartbeat = tokio::spawn(async move {
        for tick in 1..=6_u32 {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            let Ok(guard) = shared_for_heartbeat.lock() else {
                debug!("supervisor: heartbeat: shared state poisoned");
                return;
            };
            let ring_bytes = guard.ring.len();
            drop(guard);
            if ring_bytes > 0 {
                debug!(
                    session_id = %heartbeat_session,
                    tick,
                    ring_bytes,
                    "supervisor: heartbeat (child producing output, exiting heartbeat)"
                );
                return;
            }
            if tick == 6 {
                warn!(
                    session_id = %heartbeat_session,
                    "supervisor: 30 s elapsed and child has not produced any output; \
                     it may be hung on stdin (e.g. unanswered DSR query) or env-starved"
                );
            }
        }
    });

    // Stop signal: pipe Input/Resize/Stop dispatch sends a kill request here.
    // Kept separate from child.wait() so it can be invoked multiple times.
    let (stop_tx, mut stop_rx) = mpsc::unbounded_channel::<()>();
    let stop_handler = tokio::spawn(async move {
        while stop_rx.recv().await.is_some() {
            info!("supervisor: stop request received, killing child");
            if let Err(err) = child_killer.kill() {
                debug!(?err, "supervisor: child kill failed (likely already gone)");
            }
        }
    });

    // Local-socket accept loop.
    let accept_cfg = AcceptConfig {
        pipe_name: cfg.pipe_name.clone(),
        session_id: cfg.session_id.clone(),
        child_pid,
        shared: Arc::clone(&shared),
        pty_input_tx,
        resize_tx,
        stop_tx,
        exit_broadcast: exit_broadcast_tx,
    };
    let pipe_accept = tokio::spawn(accept_loop(accept_cfg));

    // Block on child exit. Once the child is gone we let the pipe accept loop
    // finish whatever its current connection is doing (with the Exited frame
    // already broadcast), then abort it.
    let exit_code = child_wait.await.unwrap_or(-1);
    info!(exit_code, "supervisor: child exited; shutting down");

    // Brief grace period so the socket handler can flush the Exited frame.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    pipe_accept.abort();
    stop_handler.abort();
    heartbeat.abort();

    // Unix mirror of the Windows KILL_ON_JOB_CLOSE job object: take out any
    // surviving descendants in the child's process group (dev servers, node,
    // etc.) so they don't pin worktree files open and wedge cleanup — and so a
    // grandchild holding the PTY slave open can't block the reader-thread join
    // below. macOS has no PR_SET_PDEATHSIG, so this is explicit on the shutdown
    // path; a tracer killed with SIGKILL still leaks survivors (same gap the
    // plan notes). Best-effort, mirroring the job-object contract.
    #[cfg(unix)]
    if let Some(pid) = child_pid {
        kill_process_group(pid);
    }

    // On Unix the bound socket leaves a file behind; remove it so a re-spawn
    // with the same name binds cleanly. (No-op on Windows named pipes.)
    #[cfg(unix)]
    remove_stale_socket(&cfg.pipe_name);

    let _ = reader_handle.join();

    let ring_bytes = shared.lock().map_or(0, |s| s.ring.len());
    let overflowed = shared.lock().is_ok_and(|s| s.ring.overflowed());
    info!(
        session_id = %cfg.session_id,
        exit_code,
        ring_bytes,
        overflowed,
        "supervisor: exiting"
    );
    Ok(())
}

/// Convert a stored socket-name string into an `interprocess` `Name`. The
/// daemon and tracer agree on the same string (passed via `--pipe-name`); this
/// just selects the right namespace per platform.
#[cfg(windows)]
fn to_local_name(s: &str) -> std::io::Result<interprocess::local_socket::Name<'_>> {
    use interprocess::local_socket::{GenericNamespaced, ToNsName as _};
    s.to_ns_name::<GenericNamespaced>()
}

#[cfg(not(windows))]
fn to_local_name(s: &str) -> std::io::Result<interprocess::local_socket::Name<'_>> {
    use interprocess::local_socket::{GenericFilePath, ToFsName as _};
    s.to_fs_name::<GenericFilePath>()
}

/// Remove a leftover Unix socket file so a fresh bind doesn't fail with
/// `EADDRINUSE`. `interprocess` reclaims the name by default; this is a
/// belt-and-suspenders guard plus the explicit shutdown cleanup. No-op when
/// the file is already gone.
#[cfg(unix)]
fn remove_stale_socket(path: &str) {
    match std::fs::remove_file(path) {
        Ok(()) => debug!(path, "supervisor: removed stale socket file"),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => warn!(?err, path, "supervisor: could not remove stale socket file"),
    }
}

/// Signal the child's process group on shutdown. `portable-pty` runs `setsid`
/// for the PTY child, so its PGID equals its PID and the whole descendant tree
/// shares the group; `killpg` therefore reaches grandchildren the way the
/// Windows job object does. If the child was somehow not a group leader,
/// `killpg` no-ops (`ESRCH`) rather than touching the tracer's own group.
/// SIGTERM first for a clean exit, then SIGKILL for anything still standing.
#[cfg(unix)]
fn kill_process_group(child_pid: u32) {
    let Ok(pgid) = i32::try_from(child_pid) else {
        warn!(
            child_pid,
            "supervisor: child pid too large for killpg; skipping group kill"
        );
        return;
    };
    // SAFETY: `killpg` only inspects the pgid + signal number; there are no
    // memory-safety preconditions. Return values are intentionally ignored —
    // this is best-effort cleanup, exactly like the Windows job-object path.
    unsafe {
        libc::killpg(pgid, libc::SIGTERM);
    }
    std::thread::sleep(std::time::Duration::from_millis(150));
    // SAFETY: see above.
    let killed = unsafe { libc::killpg(pgid, libc::SIGKILL) };
    debug!(
        child_pid,
        killed, "supervisor: signalled child process group on shutdown"
    );
}

struct AcceptConfig {
    pipe_name: String,
    session_id: String,
    child_pid: Option<u32>,
    shared: Arc<Mutex<SharedOutput>>,
    pty_input_tx: mpsc::UnboundedSender<Vec<u8>>,
    resize_tx: mpsc::UnboundedSender<(u16, u16)>,
    stop_tx: mpsc::UnboundedSender<()>,
    exit_broadcast: broadcast::Sender<i32>,
}

async fn accept_loop(cfg: AcceptConfig) {
    let name = match to_local_name(&cfg.pipe_name) {
        Ok(name) => name,
        Err(err) => {
            error!(
                ?err,
                pipe = %cfg.pipe_name,
                "supervisor: invalid socket name; ending accept loop"
            );
            return;
        }
    };

    #[cfg(unix)]
    remove_stale_socket(&cfg.pipe_name);

    let listener = match ListenerOptions::new().name(name).create_tokio() {
        Ok(listener) => listener,
        Err(err) => {
            error!(
                ?err,
                pipe = %cfg.pipe_name,
                "supervisor: failed to bind local socket; ending accept loop"
            );
            return;
        }
    };
    info!(pipe = %cfg.pipe_name, "supervisor: local socket bound; awaiting clients");

    // Serialized accept: only one daemon is serviced at a time. A racing daemon
    // waits in the OS backlog (Unix) or retries the connect (Windows named
    // pipe) until the current `handle_client` returns. A daemon restart drops
    // the old connection (EOF on the read half), `handle_client` returns, and
    // the next iteration accepts the new daemon — preserving the single-attach
    // + ring-replay contract the named-pipe `max_instances(1)` used to give us.
    let mut iteration: u32 = 0;
    loop {
        iteration = iteration.saturating_add(1);
        let conn = match listener.accept().await {
            Ok(conn) => conn,
            Err(err) => {
                warn!(?err, iteration, "supervisor: accept failed; retrying");
                continue;
            }
        };
        info!(
            iteration,
            session_id = %cfg.session_id,
            "supervisor: client connected"
        );

        let handle_started = Instant::now();
        let res = handle_client(
            conn,
            Arc::clone(&cfg.shared),
            cfg.pty_input_tx.clone(),
            cfg.resize_tx.clone(),
            cfg.stop_tx.clone(),
            cfg.exit_broadcast.subscribe(),
            cfg.child_pid,
        )
        .await;

        // Always clear the attached writer on disconnect so the reader stops
        // forwarding to a dead channel.
        if let Ok(mut state) = cfg.shared.lock() {
            state.attached = None;
        }

        let session_elapsed_ms =
            u64::try_from(handle_started.elapsed().as_millis()).unwrap_or(u64::MAX);
        match res {
            Ok(()) => info!(
                iteration,
                session_elapsed_ms, "supervisor: client disconnected cleanly"
            ),
            Err(err) => warn!(
                ?err,
                iteration, session_elapsed_ms, "supervisor: client handler ended with error"
            ),
        }
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "handle_client runs handshake + input dispatch + output forwarding in one task; \
              breaking it up means routing a handle's worth of channels through every helper"
)]
async fn handle_client<S>(
    stream: S,
    shared: Arc<Mutex<SharedOutput>>,
    pty_input_tx: mpsc::UnboundedSender<Vec<u8>>,
    resize_tx: mpsc::UnboundedSender<(u16, u16)>,
    stop_tx: mpsc::UnboundedSender<()>,
    mut exit_rx: broadcast::Receiver<i32>,
    child_pid: Option<u32>,
) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (reader, writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);
    let mut writer = writer;

    // Read TracerHello first.
    let mut hello_line = String::new();
    let n = reader
        .read_line(&mut hello_line)
        .await
        .context("reading TracerHello")?;
    if n == 0 {
        return Err(anyhow::anyhow!("client closed before Hello"));
    }
    let hello: TracerHello =
        serde_json::from_str(hello_line.trim_end()).context("parsing TracerHello")?;
    let negotiated = negotiate(hello.version, &hello.supported);
    let Some(version) = negotiated else {
        let err = TracerResponse::Error {
            message: format!(
                "no mutually supported tracer version: daemon advertises {} / {:?}, tracer supports {:?}",
                hello.version, hello.supported, SUPPORTED_TRACER_VERSIONS
            ),
        };
        write_frame(&mut writer, &err).await?;
        return Err(anyhow::anyhow!("version negotiation failed"));
    };
    let welcome = TracerWelcome {
        version,
        supported: SUPPORTED_TRACER_VERSIONS.to_vec(),
    };
    write_struct_frame(&mut writer, &welcome).await?;
    debug!(version, "supervisor: negotiated tracer version");

    // Install the output channel BEFORE we read the ring snapshot, so any
    // bytes that arrive between snapshot and "attached" land in the channel
    // and aren't lost.
    let (pipe_out_tx, mut pipe_out_rx) = mpsc::channel::<Vec<u8>>(PIPE_OUTPUT_CHANNEL_CAPACITY);
    let snapshot = {
        let Ok(mut state) = shared.lock() else {
            return Err(anyhow::anyhow!("shared state poisoned"));
        };
        let snap = state.ring.snapshot_bytes();
        state.attached = Some(pipe_out_tx);
        snap
    };

    if !snapshot.is_empty() {
        debug!(bytes = snapshot.len(), "supervisor: sending ring replay");
        write_frame(
            &mut writer,
            &TracerResponse::Output {
                data_b64: B64.encode(&snapshot),
            },
        )
        .await?;
    }

    // Input loop: read lines from the pipe, dispatch to PTY/resize/stop/status.
    let mut input_task = tokio::spawn(async move {
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => {
                    info!("supervisor: pipe input EOF — daemon disconnected");
                    return Ok::<(), anyhow::Error>(());
                }
                Ok(_) => {}
                Err(err) => {
                    info!(
                        ?err,
                        "supervisor: pipe input read error — exiting input loop"
                    );
                    return Err(err.into());
                }
            }
            let parsed = match InboundTracerRequest::from_json_str(line.trim_end()) {
                Ok(p) => p,
                Err(err) => {
                    warn!(?err, line = %line.trim_end(), "supervisor: unparseable pipe input");
                    continue;
                }
            };
            match parsed {
                InboundTracerRequest::Known(TracerRequest::Input { data_b64 }) => {
                    match B64.decode(&data_b64) {
                        Ok(bytes) => {
                            // Paste-fidelity instrumentation: log paste-sized
                            // Input frames as they arrive off the daemon pipe,
                            // before they hit the ConPTY writer thread. Pairs
                            // with the "writing to ConPTY master" line so a byte
                            // count that arrives here but shrinks on the write
                            // isolates loss to the ConPTY write itself. Kept at
                            // debug now that the send path is proven intact.
                            if bytes.len() > 16 {
                                debug!(bytes = bytes.len(), "supervisor: Input frame received");
                            }
                            let _ = pty_input_tx.send(bytes);
                        }
                        Err(err) => warn!(?err, "supervisor: bad base64 in Input"),
                    }
                }
                InboundTracerRequest::Known(TracerRequest::Resize { cols, rows }) => {
                    let _ = resize_tx.send((cols, rows));
                }
                InboundTracerRequest::Known(TracerRequest::Status) => {
                    // Status replies aren't routed through this task — we'd
                    // need bidirectional access to the writer. Send via the
                    // output channel by constructing a dedicated Status path.
                    // For MVP, the daemon doesn't request Status (it reads
                    // sidecar metadata instead); leave this as a no-op and
                    // log so we notice if someone wires it up.
                    debug!("supervisor: Status request received; not yet implemented");
                }
                InboundTracerRequest::Known(TracerRequest::Stop) => {
                    info!("supervisor: Stop request received");
                    let _ = stop_tx.send(());
                }
                InboundTracerRequest::Unknown { type_tag, .. } => {
                    warn!(%type_tag, "supervisor: unknown tracer request");
                }
            }
        }
    });

    // Output loop: forward bytes from the ring-fed channel as Output frames.
    // Also watch for child exit so we can emit a final Exited frame before
    // closing. The `input_task` JoinHandle is selected on so an EOF or read
    // error on the read half terminates the handler immediately — without
    // this branch, an idle session (no PTY output to forward) wouldn't
    // notice the daemon's disconnect until the next 30 s keepalive write
    // failed, leaving the pipe occupied long enough for the next daemon's
    // reattach attempt to time out (the "always exactly one abandoned
    // session" pattern observed in tracer logs).
    loop {
        tokio::select! {
            biased;
            chunk = pipe_out_rx.recv() => {
                if let Some(bytes) = chunk {
                    let frame = TracerResponse::Output { data_b64: B64.encode(&bytes) };
                    if let Err(err) = write_frame(&mut writer, &frame).await {
                        info!(
                            ?err,
                            "supervisor: pipe write failed on Output; client gone — exiting handler"
                        );
                        break;
                    }
                } else {
                    info!("supervisor: output channel closed; exiting handler");
                    break;
                }
            }
            exit_code = exit_rx.recv() => {
                if let Ok(code) = exit_code {
                    info!(code, "supervisor: forwarding child exit to client");
                    let _ = write_frame(
                        &mut writer,
                        &TracerResponse::Exited { code },
                    )
                    .await;
                }
                break;
            }
            // Input task ended — either EOF on the read half (daemon
            // disconnected) or a read error. Either way, this client is
            // gone; break so the accept loop can grab the next one.
            join_res = &mut input_task => {
                match join_res {
                    Ok(Ok(())) => info!(
                        "supervisor: input task ended cleanly (daemon disconnected); exiting handler"
                    ),
                    Ok(Err(err)) => info!(
                        ?err,
                        "supervisor: input task ended with error; exiting handler"
                    ),
                    Err(err) => warn!(
                        ?err,
                        "supervisor: input task join failed; exiting handler"
                    ),
                }
                break;
            }
            // Periodic ping so write errors during long idle periods surface
            // quickly enough that the attached-state clears for the next
            // daemon to attach. With the input_task branch above this is
            // mostly a belt-and-suspenders backstop, but still useful to
            // detect a half-closed pipe (read half alive but writes failing).
            () = tokio::time::sleep(std::time::Duration::from_secs(30)) => {
                let ring_bytes = shared.lock().map_or(0, |s| s.ring.len());
                let frame = TracerResponse::Status {
                    child_pid,
                    child_alive: true,
                    ring_bytes,
                };
                match write_frame(&mut writer, &frame).await {
                    Ok(()) => {
                        debug!(
                            ring_bytes,
                            child_pid = ?child_pid,
                            "supervisor: keepalive write succeeded"
                        );
                    }
                    Err(err) => {
                        info!(
                            ?err,
                            "supervisor: keepalive write failed; client gone — exiting handler"
                        );
                        break;
                    }
                }
            }
        }
    }

    input_task.abort();
    Ok(())
}

async fn write_frame<W>(w: &mut W, msg: &TracerResponse) -> anyhow::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let mut line = serde_json::to_string(msg).context("serializing tracer response")?;
    line.push('\n');
    w.write_all(line.as_bytes())
        .await
        .context("writing frame")?;
    Ok(())
}

async fn write_struct_frame<W, T>(w: &mut W, msg: &T) -> anyhow::Result<()>
where
    W: AsyncWrite + Unpin,
    T: serde::Serialize,
{
    let mut line = serde_json::to_string(msg).context("serializing handshake frame")?;
    line.push('\n');
    w.write_all(line.as_bytes())
        .await
        .context("writing frame")?;
    Ok(())
}

/// Spawn the child via portable-pty. Wrapper exists so the call site stays
/// concise; the `&mut child` flow is handled by the caller.
fn cmd_spawn(
    slave: &dyn portable_pty::SlavePty,
    cmd: CommandBuilder,
) -> anyhow::Result<Box<dyn portable_pty::Child + Send + Sync>> {
    slave.spawn_command(cmd).context("spawning child")
}

/// Block a sync thread on a tokio mpsc receiver. Uses
/// `blocking_recv` if available; falls back to a manual loop otherwise.
fn block_on_recv(rx: &mut mpsc::UnboundedReceiver<Vec<u8>>) -> Option<Vec<u8>> {
    rx.blocking_recv()
}

const _: () = {
    // Sanity: re-export the version we negotiate against.
    assert!(TRACER_VERSION > 0);
};
