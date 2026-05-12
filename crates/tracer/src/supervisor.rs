//! Top-level supervisor loop. Spawns the child via PTY, hosts the named-pipe
//! server, multiplexes child output → ring/pipe and pipe → child stdin.
//!
//! Phase C.2 skeleton: the PTY is spawned, the ring is allocated, and the
//! output-reader thread copies child bytes into the ring. The named-pipe
//! server is stubbed pending C.1's empirical answer to "`interprocess` vs
//! `tokio::net::windows::named_pipe` vs raw `windows` crate". The `run` entry
//! point compiles and the read-loop exercises the ring; it's a contract for
//! C.3 to wire up, not a working binary.

use crate::ring::OutputRing;
use anyhow::Context as _;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tracing::{info, warn};

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

/// Boot the supervisor: spawn the PTY child, set up the ring, host the pipe.
/// Returns when the child exits.
pub async fn run(cfg: Config) -> anyhow::Result<()> {
    info!(
        session_id = %cfg.session_id,
        pipe = %cfg.pipe_name,
        program = %cfg.program,
        "supervisor: starting"
    );

    let ring: Arc<Mutex<OutputRing>> = Arc::new(Mutex::new(OutputRing::new()));

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: cfg.rows,
            cols: cfg.cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("opening PTY")?;

    let mut cmd = CommandBuilder::new(&cfg.program);
    for arg in &cfg.args {
        cmd.arg(arg);
    }
    cmd.cwd(cfg.cwd.as_path());

    let mut child = pair.slave.spawn_command(cmd).context("spawning child")?;
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .context("cloning PTY master reader")?;
    let ring_for_reader = Arc::clone(&ring);
    let reader_handle = std::thread::spawn(move || -> anyhow::Result<()> {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if let Ok(mut ring) = ring_for_reader.lock() {
                        ring.push(&buf[..n]);
                    }
                }
                Err(err) => {
                    warn!(?err, "PTY reader: read failed; exiting");
                    break;
                }
            }
        }
        Ok(())
    });

    // TODO(C.3): wire up the actual pipe server here. Per docs/tracer-abi.md
    // the design intent is:
    //   1. Server-side named pipe at `cfg.pipe_name`
    //   2. tokio task: read from ring → write to pipe (with daemon
    //      reconnect handling)
    //   3. tokio task: read from pipe → parse `InboundTracerRequest` →
    //      dispatch (Input/Resize/Status/Stop)
    //   4. On daemon disconnect, keep ring filling; on reconnect, drain
    //      the ring to the new connection before resuming the live stream.
    //
    // The implementation is gated on the C.1 empirical spike. Until those
    // answers land, the skeleton just blocks on the child and returns
    // without serving anything — a misconfigured daemon spawning this
    // binary exits cleanly rather than leaving an orphan tracer behind.
    warn!(
        session_id = %cfg.session_id,
        "supervisor: pipe server not yet implemented (C.3 pending). Waiting for child exit."
    );

    // Wait for the child to exit (blocking call inside a tokio task is fine
    // here — this binary is single-purpose and the wait is the whole job).
    let status = tokio::task::spawn_blocking(move || child.wait())
        .await
        .context("joining child wait task")?
        .context("waiting on child")?;

    let _ = reader_handle.join();

    let snapshot_len = ring.lock().map(|r| r.len()).unwrap_or(0);
    let overflowed = ring.lock().map(|r| r.overflowed()).unwrap_or(false);
    info!(
        session_id = %cfg.session_id,
        ?status,
        ring_bytes = snapshot_len,
        overflowed,
        "supervisor: child exited"
    );
    Ok(())
}
