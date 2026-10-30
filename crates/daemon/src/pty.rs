//! Handle to a PTY-bound child process.
//!
//! `tracer_client` spawns each session under `rt-tracer.exe` and reattaches
//! across daemon restarts. Its reader task owns the session's
//! [`PtyProducer`]: every output chunk, then the exit, goes down one lossless
//! queue to the session's lifecycle task (`session::attach_lifecycle`), so
//! the exit is handled only once every chunk before it has reached clients
//! and scrollback. The [`PtyHandle`] carries the watcher broadcast, input,
//! resize and kill.

use portable_pty::ChildKiller;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, warn};

use crate::typing_lane::TypingLane;

const MAX_PTY_INPUT_CHUNK_BYTES: usize = 2048;

/// Items the lifecycle queue holds. Bounded so a slow lifecycle task makes
/// the reader wait; the tracer's own 4 MB ring absorbs what it can't hand
/// over.
pub const LIFECYCLE_OUTPUT_CAPACITY: usize = 256;

/// One item on a session's lifecycle queue.
#[derive(Debug)]
pub enum LifecycleItem {
    /// A chunk of the child's output.
    Chunk(Vec<u8>),
    /// How the session ended; always the queue's last item. A queue that
    /// closes without one lost its reader, which is [`PtyExit::TracerLost`].
    Exited(PtyExit),
}

/// How a PTY session's process ended, as its tracer connection saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PtyExit {
    /// The tracer reported that the child exited with this code.
    Code(i32),
    /// The tracer pipe closed or failed before the tracer reported an exit:
    /// the tracer was killed or crashed, and the child's fate is unknown.
    TracerLost,
}

impl PtyExit {
    /// The exit code surfaced on the session: the child's own, or `-1` when
    /// the tracer was lost.
    #[must_use]
    pub fn code(self) -> i32 {
        match self {
            Self::Code(code) => code,
            Self::TracerLost => -1,
        }
    }
}

#[derive(Clone)]
pub struct PtySpawnSpec {
    /// Session id used to derive the tracer pipe name and the orphan sidecar
    /// path. Required even for plain-shell sessions because every PTY now
    /// flows through a tracer.
    pub session_id: String,
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    /// Variables the tracer, and so the child, must not inherit from the
    /// daemon's environment (a provider's removed variables).
    pub env_remove: Vec<String>,
    pub cols: u16,
    pub rows: u16,
    /// The agent a shell typed it into runs, handed to the tracer so its log
    /// names it; `None` for every other spawn.
    pub agent_note: Option<crate::tracer_log::AgentNote>,
}

/// Shows the env's keys only: its values can hold resolved secrets.
impl std::fmt::Debug for PtySpawnSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let env_keys: Vec<&str> = self.env.iter().map(|(k, _)| k.as_str()).collect();
        f.debug_struct("PtySpawnSpec")
            .field("session_id", &self.session_id)
            .field("program", &self.program)
            .field("args", &self.args)
            .field("cwd", &self.cwd)
            .field("env_keys", &env_keys)
            .field("env_remove", &self.env_remove)
            .field("cols", &self.cols)
            .field("rows", &self.rows)
            .field("agent_note", &self.agent_note)
            .finish()
    }
}

/// The watchers' side of a PTY session's output: a bounded broadcast for the
/// status, title and injector watchers, which may lag and skip.
#[derive(Clone)]
pub struct PtyOutput {
    watchers: broadcast::Sender<Vec<u8>>,
}

impl PtyOutput {
    /// A watcher's receiver, starting at the next chunk sent.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Vec<u8>> {
        self.watchers.subscribe()
    }

    /// How many watchers are subscribed; the lifecycle queue is not counted.
    #[must_use]
    pub fn receiver_count(&self) -> usize {
        self.watchers.receiver_count()
    }
}

/// The sending side of a PTY session, owned by its reader. Every chunk goes
/// to two places: a bounded queue read only by the session's lifecycle task
/// (`session::attach_lifecycle`), which feeds clients and scrollback and so
/// must never skip a chunk (a full queue makes the sender wait), and the
/// watchers' [`PtyOutput`]. [`Self::finish`] ends the queue with the exit.
pub struct PtyProducer {
    lifecycle: mpsc::Sender<LifecycleItem>,
    output: PtyOutput,
}

impl PtyProducer {
    /// A new producer whose watcher broadcast holds `capacity` chunks, and
    /// the receiver of its lifecycle queue, which goes into
    /// [`PtyHandleParts`] with [`Self::output`].
    #[must_use]
    pub fn channel(capacity: usize) -> (Self, mpsc::Receiver<LifecycleItem>) {
        let (lifecycle, lifecycle_rx) = mpsc::channel(LIFECYCLE_OUTPUT_CAPACITY);
        let (watchers, _) = broadcast::channel(capacity);
        (
            Self {
                lifecycle,
                output: PtyOutput { watchers },
            },
            lifecycle_rx,
        )
    }

    /// The watchers' side of this output.
    #[must_use]
    pub fn output(&self) -> PtyOutput {
        self.output.clone()
    }

    /// Sends one chunk to the lifecycle queue, waiting while it is full, then
    /// to the watchers. Neither having no receiver is an error: a session
    /// nobody reads drops its output.
    pub async fn send(&self, bytes: Vec<u8>) {
        let _ = self
            .lifecycle
            .send(LifecycleItem::Chunk(bytes.clone()))
            .await;
        let _ = self.output.watchers.send(bytes);
    }

    /// [`Self::send`] from a thread outside the runtime, for tests that feed
    /// a session from a plain thread.
    #[cfg(test)]
    pub fn blocking_send(&self, bytes: Vec<u8>) {
        let _ = self
            .lifecycle
            .blocking_send(LifecycleItem::Chunk(bytes.clone()));
        let _ = self.output.watchers.send(bytes);
    }

    /// Ends the lifecycle queue with `exit`, after every chunk sent before
    /// it, waiting while the queue is full. A lifecycle task that is gone
    /// has nobody left to tell.
    pub async fn finish(self, exit: PtyExit) {
        let _ = self.lifecycle.send(LifecycleItem::Exited(exit)).await;
    }
}

/// All the channel ends needed to construct a [`PtyHandle`]. The tracer-client
/// path builds one of these from pipe IO.
pub struct PtyHandleParts {
    /// [`PtyProducer::output`] of the session's producer.
    pub output: PtyOutput,
    /// The receiver [`PtyProducer::channel`] returned with the producer.
    pub lifecycle_output: mpsc::Receiver<LifecycleItem>,
    pub input_tx: mpsc::UnboundedSender<Vec<u8>>,
    pub resize_tx: mpsc::UnboundedSender<(u16, u16)>,
    pub killer: Box<dyn ChildKiller + Send + Sync>,
    /// Process id of the PTY child (agent / shell). `None` when the
    /// tracer hasn't yet reported one back. The orphan sidecar's `pid` field
    /// gets this value for legacy compatibility; the `tracer_pid` field carries
    /// the rt-tracer process id used for liveness checks post-C.3.
    pub pid: Option<u32>,
}

pub struct PtyHandle {
    /// Raw output bytes from the child. Watchers subscribe to it; the
    /// lifecycle task takes its lossless queue with
    /// [`Self::take_lifecycle_output`].
    pub output: PtyOutput,
    /// The lifecycle task's lossless queue, taken once.
    lifecycle_output: Mutex<Option<mpsc::Receiver<LifecycleItem>>>,
    input_tx: mpsc::UnboundedSender<Vec<u8>>,
    resize_tx: mpsc::UnboundedSender<(u16, u16)>,
    /// Master kill switch. Dropping closes input/resize channels and the
    /// reader task observes EOF.
    killer: Mutex<Option<Box<dyn ChildKiller + Send + Sync>>>,
    pid: Option<u32>,
    /// One typist at a time on this PTY.
    typing: Arc<TypingLane>,
}

impl PtyHandle {
    #[must_use]
    pub fn from_parts(parts: PtyHandleParts) -> Self {
        Self {
            output: parts.output,
            lifecycle_output: Mutex::new(Some(parts.lifecycle_output)),
            input_tx: parts.input_tx,
            resize_tx: parts.resize_tx,
            killer: Mutex::new(Some(parts.killer)),
            pid: parts.pid,
            typing: Arc::new(TypingLane::default()),
        }
    }

    /// The lane a run of keys takes so no other run interleaves with it.
    #[must_use]
    pub fn typing(&self) -> &Arc<TypingLane> {
        &self.typing
    }

    pub fn write_input(&self, data: Vec<u8>) {
        // Paste-fidelity instrumentation (pairs with the native client's
        // `paste#N ... sent_bytes=` log line in `term_view.rs`). `chunks` records how many tracer Input
        // frames this write was split into. Kept at `debug` now that the send
        // path is proven byte-exact end to end and the ConPTY-overrun loss is
        // paced against in the tracer writer thread (see supervisor.rs); the
        // trail stays available for re-diagnosis without flooding daemon.log.
        let total = data.len();
        if total <= MAX_PTY_INPUT_CHUNK_BYTES {
            if total > 16 {
                debug!(bytes = total, chunks = 1, "write_input paste-sized");
            }
            let _ = self.input_tx.send(data);
            return;
        }
        let chunk_count = total.div_ceil(MAX_PTY_INPUT_CHUNK_BYTES);
        debug!(
            bytes = total,
            chunks = chunk_count,
            "write_input paste-sized (split)"
        );
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

    /// The lifecycle task's queue: every chunk, in order, none skipped, then
    /// the exit. `None` once taken.
    pub fn take_lifecycle_output(&self) -> Option<mpsc::Receiver<LifecycleItem>> {
        self.lifecycle_output.lock().ok().and_then(|mut g| g.take())
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
    fn spawn_spec_debug_shows_env_keys_but_not_values() {
        let spec = PtySpawnSpec {
            session_id: "s1".to_owned(),
            program: "claude".to_owned(),
            args: Vec::new(),
            cwd: PathBuf::from("."),
            env: vec![(
                "EXAMPLE_API_KEY".to_owned(),
                "sk-example-hunter2".to_owned(),
            )],
            env_remove: Vec::new(),
            cols: 80,
            rows: 24,
            agent_note: None,
        };
        let shown = format!("{spec:?}");
        assert!(shown.contains("EXAMPLE_API_KEY"), "{shown}");
        assert!(!shown.contains("hunter2"), "{shown}");
    }

    #[test]
    fn write_input_splits_large_payloads_into_bounded_chunks() {
        let (producer, lifecycle_output) = PtyProducer::channel(1);
        let (input_tx, mut input_rx) = mpsc::unbounded_channel();
        let (resize_tx, _) = mpsc::unbounded_channel();
        let handle = PtyHandle::from_parts(PtyHandleParts {
            output: producer.output(),
            lifecycle_output,
            input_tx,
            resize_tx,
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
        let (producer, lifecycle_output) = PtyProducer::channel(1);
        let (input_tx, mut input_rx) = mpsc::unbounded_channel();
        let (resize_tx, _) = mpsc::unbounded_channel();
        let handle = PtyHandle::from_parts(PtyHandleParts {
            output: producer.output(),
            lifecycle_output,
            input_tx,
            resize_tx,
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
