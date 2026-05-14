//! Handle to a PTY-bound child process.
//!
//! Pre-Phase-C this module also owned the spawn pipeline (direct portable-pty
//! / `ConPTY`). Phase C.3 moved that responsibility into `tracer_client`, which
//! shells out to `rt-tracer.exe` per session and reattaches across daemon
//! restarts. The handle surface (`output`/`write_input`/`resize`/`take_exit`/
//! `kill`/`pid`) is unchanged — call sites stay agnostic to the spawn path.

use portable_pty::ChildKiller;
use std::path::PathBuf;
use std::sync::Mutex;
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{debug, warn};

const MAX_PTY_INPUT_CHUNK_BYTES: usize = 2048;

#[derive(Debug, Clone)]
pub struct PtySpawnSpec {
    /// Session id used to derive the tracer pipe name and the orphan sidecar
    /// path. Required even for plain-shell sessions because every PTY now
    /// flows through a tracer.
    pub session_id: String,
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    pub cols: u16,
    pub rows: u16,
}

/// All the channel ends needed to construct a [`PtyHandle`]. The tracer-client
/// path builds one of these from pipe IO; legacy direct-PTY code paths would
/// build one from `portable_pty` if they existed (they don't, post-C.3).
pub struct PtyHandleParts {
    pub output: broadcast::Sender<Vec<u8>>,
    pub input_tx: mpsc::UnboundedSender<Vec<u8>>,
    pub resize_tx: mpsc::UnboundedSender<(u16, u16)>,
    pub exit_rx: oneshot::Receiver<i32>,
    pub killer: Box<dyn ChildKiller + Send + Sync>,
    /// Process id of the PTY child (claude / codex / shell). `None` when the
    /// tracer hasn't yet reported one back. The orphan sidecar's `pid` field
    /// gets this value for legacy compatibility; the `tracer_pid` field carries
    /// the rt-tracer process id used for liveness checks post-C.3.
    pub pid: Option<u32>,
}

pub struct PtyHandle {
    /// Broadcast of raw output bytes from the child. Re-subscribe per-client.
    pub output: broadcast::Sender<Vec<u8>>,
    input_tx: mpsc::UnboundedSender<Vec<u8>>,
    resize_tx: mpsc::UnboundedSender<(u16, u16)>,
    /// Receives the child's exit status once.
    exit_rx: Mutex<Option<oneshot::Receiver<i32>>>,
    /// Master kill switch. Dropping closes input/resize channels and the
    /// reader task observes EOF.
    killer: Mutex<Option<Box<dyn ChildKiller + Send + Sync>>>,
    pid: Option<u32>,
}

impl PtyHandle {
    #[must_use]
    pub fn from_parts(parts: PtyHandleParts) -> Self {
        Self {
            output: parts.output,
            input_tx: parts.input_tx,
            resize_tx: parts.resize_tx,
            exit_rx: Mutex::new(Some(parts.exit_rx)),
            killer: Mutex::new(Some(parts.killer)),
            pid: parts.pid,
        }
    }

    pub fn write_input(&self, data: Vec<u8>) {
        if data.len() <= MAX_PTY_INPUT_CHUNK_BYTES {
            let _ = self.input_tx.send(data);
            return;
        }
        for chunk in data.chunks(MAX_PTY_INPUT_CHUNK_BYTES) {
            let _ = self.input_tx.send(chunk.to_vec());
        }
    }

    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        let _ = self.resize_tx.send((cols, rows));
    }

    pub fn take_exit(&self) -> Option<oneshot::Receiver<i32>> {
        self.exit_rx.lock().ok().and_then(|mut g| g.take())
    }

    pub fn kill(&self) {
        if let Ok(mut guard) = self.killer.lock()
            && let Some(mut k) = guard.take()
            && let Err(err) = k.kill()
        {
            // Windows quirk: portable-pty's killer wraps TerminateProcess.
            // When the child has already exited (race with the wait task)
            // the OS returns ERROR_SUCCESS (0) but the kill call still
            // surfaces an Err. The kill is effectively a no-op in that
            // case — the child is already dead — so demote that specific
            // shape to debug rather than warn.
            if err.raw_os_error() == Some(0) {
                debug!(?err, "PTY kill: child already gone");
            } else {
                warn!(?err, "failed to kill PTY child");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct NoopKiller;

    impl ChildKiller for NoopKiller {
        fn kill(&mut self) -> std::io::Result<()> {
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
            Box::new(NoopKiller)
        }
    }

    #[test]
    fn write_input_splits_large_payloads_into_bounded_chunks() {
        let (output, _) = broadcast::channel(1);
        let (input_tx, mut input_rx) = mpsc::unbounded_channel();
        let (resize_tx, _) = mpsc::unbounded_channel();
        let (_exit_tx, exit_rx) = oneshot::channel();
        let handle = PtyHandle::from_parts(PtyHandleParts {
            output,
            input_tx,
            resize_tx,
            exit_rx,
            killer: Box::new(NoopKiller),
            pid: None,
        });
        let input = vec![b'x'; (MAX_PTY_INPUT_CHUNK_BYTES * 2) + 17];

        handle.write_input(input.clone());

        let mut chunks = Vec::new();
        while let Ok(chunk) = input_rx.try_recv() {
            chunks.push(chunk);
        }
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].len(), MAX_PTY_INPUT_CHUNK_BYTES);
        assert_eq!(chunks[1].len(), MAX_PTY_INPUT_CHUNK_BYTES);
        assert_eq!(chunks[2].len(), 17);
        assert_eq!(chunks.concat(), input);
    }

    #[test]
    fn write_input_keeps_small_payloads_as_single_writes() {
        let (output, _) = broadcast::channel(1);
        let (input_tx, mut input_rx) = mpsc::unbounded_channel();
        let (resize_tx, _) = mpsc::unbounded_channel();
        let (_exit_tx, exit_rx) = oneshot::channel();
        let handle = PtyHandle::from_parts(PtyHandleParts {
            output,
            input_tx,
            resize_tx,
            exit_rx,
            killer: Box::new(NoopKiller),
            pid: None,
        });
        let input = vec![b'y'; MAX_PTY_INPUT_CHUNK_BYTES];

        handle.write_input(input.clone());

        let mut chunks = Vec::new();
        while let Ok(chunk) = input_rx.try_recv() {
            chunks.push(chunk);
        }
        assert_eq!(chunks, vec![input]);
    }
}
