//! Top-level supervisor loop. Spawns the child via PTY, hosts the named-pipe
//! server, and multiplexes child output → ring/pipe and pipe → child stdin.
//!
//! Design notes:
//! - The PTY reader runs in a dedicated `std::thread`; output bytes are pushed
//!   to a bounded ring and, when a daemon is attached, forwarded over the pipe
//!   with backpressure. A slow daemon therefore slows the child rather than
//!   ballooning memory.
//! - Only one daemon can attach at a time (`max_instances(1)`). If a second
//!   daemon races, the OS returns `ERROR_PIPE_BUSY` (231) — see the C.1 spike
//!   write-up at `docs/spikes/c1-tracer-spike.md` Q5.
//! - On reconnect, the freshly-attached daemon receives a single ring-snapshot
//!   Output frame as the first message after `Welcome` so it can replay what
//!   it missed during the outage. After the snapshot, live output is forwarded
//!   as it arrives.

use crate::ring::OutputRing;
use anyhow::Context as _;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt as _, AsyncWrite, AsyncWriteExt as _, BufReader};
use tokio::net::windows::named_pipe::{NamedPipeServer, PipeMode, ServerOptions};
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

#[derive(Debug, Clone)]
pub struct Config {
    pub session_id: String,
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
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => {
                    debug!("supervisor: PTY reader EOF");
                    break;
                }
                Ok(n) => {
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
                    warn!(?err, "supervisor: PTY reader error; exiting");
                    break;
                }
            }
        }
    });

    // PTY writer thread: serializes input writes onto the master.
    let (pty_input_tx, mut pty_input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let writer_handle = std::thread::spawn(move || {
        let mut writer = writer;
        while let Some(bytes) = block_on_recv(&mut pty_input_rx) {
            if let Err(err) = std::io::Write::write_all(&mut writer, &bytes) {
                warn!(?err, "supervisor: PTY writer error; exiting");
                break;
            }
            let _ = std::io::Write::flush(&mut writer);
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
        let status = child.wait();
        let code = match status {
            Ok(s) => i32::try_from(s.exit_code()).unwrap_or(-1),
            Err(err) => {
                warn!(?err, "supervisor: child wait failed");
                -1
            }
        };
        let _ = exit_broadcast_for_child.send(code);
        code
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

    // Pipe accept loop.
    let pipe_cfg = PipeAcceptConfig {
        pipe_name: cfg.pipe_name.clone(),
        session_id: cfg.session_id.clone(),
        child_pid,
        shared: Arc::clone(&shared),
        pty_input_tx,
        resize_tx,
        stop_tx,
        exit_broadcast: exit_broadcast_tx,
    };
    let pipe_accept = tokio::spawn(pipe_accept_loop(pipe_cfg));

    // Block on child exit. Once the child is gone we let the pipe accept loop
    // finish whatever its current connection is doing (with the Exited frame
    // already broadcast), then abort it.
    let exit_code = child_wait.await.unwrap_or(-1);
    info!(exit_code, "supervisor: child exited; shutting down");

    // Brief grace period so the pipe handler can flush the Exited frame.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    pipe_accept.abort();
    stop_handler.abort();

    let _ = reader_handle.join();

    let ring_bytes = shared.lock().map(|s| s.ring.len()).unwrap_or(0);
    let overflowed = shared.lock().map(|s| s.ring.overflowed()).unwrap_or(false);
    info!(
        session_id = %cfg.session_id,
        exit_code,
        ring_bytes,
        overflowed,
        "supervisor: exiting"
    );
    Ok(())
}

struct PipeAcceptConfig {
    pipe_name: String,
    session_id: String,
    child_pid: Option<u32>,
    shared: Arc<Mutex<SharedOutput>>,
    pty_input_tx: mpsc::UnboundedSender<Vec<u8>>,
    resize_tx: mpsc::UnboundedSender<(u16, u16)>,
    stop_tx: mpsc::UnboundedSender<()>,
    exit_broadcast: broadcast::Sender<i32>,
}

async fn pipe_accept_loop(cfg: PipeAcceptConfig) {
    let mut first = true;
    loop {
        let server = if first {
            ServerOptions::new()
                .first_pipe_instance(true)
                .pipe_mode(PipeMode::Byte)
                .max_instances(1)
                .create(&cfg.pipe_name)
        } else {
            ServerOptions::new()
                .pipe_mode(PipeMode::Byte)
                .max_instances(1)
                .create(&cfg.pipe_name)
        };
        let server = match server {
            Ok(s) => s,
            Err(err) => {
                error!(
                    ?err,
                    pipe = %cfg.pipe_name,
                    "supervisor: pipe create failed; ending accept loop"
                );
                return;
            }
        };
        first = false;

        info!(pipe = %cfg.pipe_name, "supervisor: awaiting client");
        if let Err(err) = server.connect().await {
            warn!(?err, "supervisor: pipe connect failed; retrying");
            continue;
        }
        info!(session_id = %cfg.session_id, "supervisor: client connected");

        let res = handle_client(
            server,
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

        match res {
            Ok(()) => info!("supervisor: client disconnected cleanly"),
            Err(err) => warn!(?err, "supervisor: client handler ended with error"),
        }
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "handle_client runs handshake + input dispatch + output forwarding in one task; \
              breaking it up means routing a handle's worth of channels through every helper"
)]
async fn handle_client(
    server: NamedPipeServer,
    shared: Arc<Mutex<SharedOutput>>,
    pty_input_tx: mpsc::UnboundedSender<Vec<u8>>,
    resize_tx: mpsc::UnboundedSender<(u16, u16)>,
    stop_tx: mpsc::UnboundedSender<()>,
    mut exit_rx: broadcast::Receiver<i32>,
    child_pid: Option<u32>,
) -> anyhow::Result<()> {
    let (reader, writer) = tokio::io::split(server);
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
    let input_task = tokio::spawn(async move {
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => {
                    debug!("supervisor: pipe input EOF");
                    return Ok::<(), anyhow::Error>(());
                }
                Ok(_) => {}
                Err(err) => {
                    debug!(?err, "supervisor: pipe input read error");
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
    // closing.
    loop {
        tokio::select! {
            biased;
            chunk = pipe_out_rx.recv() => {
                if let Some(bytes) = chunk {
                    let frame = TracerResponse::Output { data_b64: B64.encode(&bytes) };
                    if let Err(err) = write_frame(&mut writer, &frame).await {
                        debug!(?err, "supervisor: pipe write failed; client likely gone");
                        break;
                    }
                } else {
                    debug!("supervisor: output channel closed");
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
            // Periodic ping so write errors during long idle periods surface
            // quickly enough that the attached-state clears for the next
            // daemon to attach.
            () = tokio::time::sleep(std::time::Duration::from_secs(30)) => {
                let frame = TracerResponse::Status {
                    child_pid,
                    child_alive: true,
                    ring_bytes: shared.lock().map(|s| s.ring.len()).unwrap_or(0),
                };
                if let Err(err) = write_frame(&mut writer, &frame).await {
                    debug!(?err, "supervisor: keepalive write failed; client gone");
                    break;
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
