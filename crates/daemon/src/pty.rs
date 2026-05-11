//! PTY supervision for a single child process.
//!
//! Spawns a child under a portable-pty, exposes a writer handle for input,
//! a broadcast channel for output bytes, and a oneshot for exit status.

use anyhow::Context as _;
use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc};
use tokio::task;
use tracing::{debug, warn};

const OUTPUT_CHUNK: usize = 4096;
const OUTPUT_BROADCAST_CAPACITY: usize = 256;

#[derive(Debug, Clone)]
pub struct PtySpawnSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    pub cols: u16,
    pub rows: u16,
}

pub struct PtyHandle {
    /// Broadcast of raw output bytes from the child. Re-subscribe per-client.
    pub output: broadcast::Sender<Vec<u8>>,
    /// Send raw bytes to be written into the PTY master.
    input_tx: mpsc::UnboundedSender<Vec<u8>>,
    /// Resize requests `(cols, rows)` forwarded to the PTY master.
    resize_tx: mpsc::UnboundedSender<(u16, u16)>,
    /// Receives the child's exit status once.
    exit_rx: Mutex<Option<tokio::sync::oneshot::Receiver<i32>>>,
    /// Master kill switch. Dropping closes input/resize channels and the
    /// reader task observes EOF.
    killer: Mutex<Option<Box<dyn portable_pty::ChildKiller + Send + Sync>>>,
    /// OS process id of the spawned child, captured at spawn time. Used to
    /// write orphan-recovery sidecar so daemon restarts can detect surviving
    /// children. `None` if the platform refused to expose one.
    pid: Option<u32>,
}

impl PtyHandle {
    pub fn write_input(&self, data: Vec<u8>) {
        let _ = self.input_tx.send(data);
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        let _ = self.resize_tx.send((cols, rows));
    }

    pub fn take_exit(&self) -> Option<tokio::sync::oneshot::Receiver<i32>> {
        self.exit_rx.lock().ok().and_then(|mut g| g.take())
    }

    pub fn kill(&self) {
        if let Ok(mut guard) = self.killer.lock()
            && let Some(mut k) = guard.take()
            && let Err(err) = k.kill()
        {
            warn!(?err, "failed to kill PTY child");
        }
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "PTY spawn wires up reader, writer, resize, waiter, and exit handles \
              in one go; splitting them apart would force every helper to grow its \
              own channel signature for marginal clarity gain"
)]
pub fn spawn(spec: PtySpawnSpec) -> anyhow::Result<Arc<PtyHandle>> {
    let started = std::time::Instant::now();
    let PtySpawnSpec {
        program,
        args,
        cwd,
        env,
        cols,
        rows,
    } = spec;

    let pty_system = NativePtySystem::default();
    let t_open = std::time::Instant::now();
    let pair = pty_system
        .openpty(PtySize {
            rows: rows.max(10),
            cols: cols.max(40),
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("openpty")?;
    debug!(
        elapsed_ms = u64::try_from(t_open.elapsed().as_millis()).unwrap_or(u64::MAX),
        "pty: openpty done"
    );

    let mut cmd = CommandBuilder::new(&program);
    cmd.args(&args);
    cmd.cwd(&cwd);
    for (k, v) in &env {
        cmd.env(k, v);
    }

    let t_child = std::time::Instant::now();
    let mut child = pair.slave.spawn_command(cmd).context("spawn child")?;
    tracing::info!(
        elapsed_ms = u64::try_from(t_child.elapsed().as_millis()).unwrap_or(u64::MAX),
        program,
        "pty: child spawned"
    );
    drop(pair.slave);

    let pid = child.process_id();
    let killer = child.clone_killer();

    let mut reader = pair.master.try_clone_reader().context("clone reader")?;
    let mut writer = pair.master.take_writer().context("take writer")?;
    let master = pair.master;

    let (output_tx, _) = broadcast::channel::<Vec<u8>>(OUTPUT_BROADCAST_CAPACITY);
    let output_for_reader = output_tx.clone();

    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (resize_tx, mut resize_rx) = mpsc::unbounded_channel::<(u16, u16)>();
    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel::<i32>();

    // Reader: blocking std::io::Read, run on a blocking thread.
    task::spawn_blocking(move || {
        let mut buf = vec![0u8; OUTPUT_CHUNK];
        loop {
            match std::io::Read::read(&mut reader, &mut buf) {
                Ok(0) => {
                    debug!("pty reader: EOF");
                    break;
                }
                Ok(n) => {
                    let _ = output_for_reader.send(buf[..n].to_vec());
                }
                Err(err) => {
                    debug!(?err, "pty reader error, ending");
                    break;
                }
            }
        }
    });

    // Writer: blocking std::io::Write driven by an async mpsc.
    let writer_thread = std::thread::spawn(move || {
        // Serialize all writes to the PTY master from a single thread.
        while let Some(bytes) = futures::executor::block_on(input_rx.recv()) {
            if let Err(err) = std::io::Write::write_all(&mut writer, &bytes) {
                debug!(?err, "pty writer error, ending");
                break;
            }
            let _ = std::io::Write::flush(&mut writer);
        }
    });
    // We don't join this thread on shutdown; it exits when input_tx closes.
    drop(writer_thread);

    // Resize task.
    let master_for_resize = Arc::new(Mutex::new(master));
    let master_for_waiter = Arc::clone(&master_for_resize);
    tokio::spawn(async move {
        while let Some((cols, rows)) = resize_rx.recv().await {
            let m = master_for_resize.clone();
            let _ = task::spawn_blocking(move || {
                if let Ok(guard) = m.lock() {
                    let _ = guard.resize(PtySize {
                        rows: rows.max(2),
                        cols: cols.max(2),
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                }
            })
            .await;
        }
    });

    // Waiter: blocks on child.wait(), reports exit code.
    task::spawn_blocking(move || {
        let status = child.wait();
        let code = match status {
            Ok(s) => i32::try_from(s.exit_code()).unwrap_or(-1),
            Err(_) => -1,
        };
        // Keep master alive until the child has exited so the reader thread
        // gets a clean EOF rather than a stale handle.
        drop(master_for_waiter);
        let _ = exit_tx.send(code);
    });

    debug!(
        elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        ?pid,
        "pty: spawn complete"
    );

    Ok(Arc::new(PtyHandle {
        output: output_tx,
        input_tx,
        resize_tx,
        exit_rx: Mutex::new(Some(exit_rx)),
        killer: Mutex::new(Some(killer)),
        pid,
    }))
}
