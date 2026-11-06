//! Tailing one file from a byte offset, for consumers that follow a file an
//! agent appends to (the chat mirror, the file-path index, a subagent's stream).
//!
//! [`spawn`] starts one task that owns all reading, a `notify` watcher on the
//! file's parent folder (the file itself may not exist yet) and a [`POLL`]
//! interval for the file systems where a watcher misses an append. Both only
//! wake the task, so two wakeups never read at once. A folder that does not
//! exist yet when the tail starts is watched on the first poll tick after it
//! appears.
//!
//! The task parks while no client is connected (`client_count` 0, the pattern
//! `git_watch` uses for its refreshers) and reads once as soon as one arrives,
//! catching up with everything appended while it was parked.

use notify::{RecommendedWatcher, RecursiveMode, Watcher as _};
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{debug, warn};

/// How often the tailer checks the file's size on its own. A watcher alone
/// misses appends on some file systems, so the poll is the backstop.
const POLL: Duration = Duration::from_millis(1500);

/// One thing the tailer saw in the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TailEvent {
    /// One complete line, without its `\n` and without a trailing `\r`. Bytes
    /// that are not UTF-8 are decoded lossily.
    Line(String),
    /// The file shrank, was replaced by a shorter one, or reappeared after it
    /// was missing: what a consumer built from the old content is stale, and
    /// the lines after this one start a fresh file.
    Restarted,
    /// The tail's first read, or a read that restarted, has delivered every
    /// complete line the file held: each line after this one was appended
    /// since. Sent even when that read found no line.
    CaughtUp,
}

/// What the handle and the tailing task share: the offset past the last
/// complete line, plus — in a test build only — the counters a spec polls to
/// know the task has observed a state before it moves the file on.
#[derive(Default)]
struct Shared {
    offset: AtomicU64,
    #[cfg(test)]
    reads: AtomicU64,
    #[cfg(test)]
    missing: AtomicU64,
    #[cfg(test)]
    parked: AtomicBool,
    #[cfg(test)]
    watching: AtomicBool,
}

impl Shared {
    fn new(offset: u64) -> Self {
        let shared = Self::default();
        shared.offset.store(offset, Ordering::SeqCst);
        shared
    }
}

/// A running tail. Dropping it (or calling [`TailHandle::stop`]) ends the task
/// and drops its watcher.
pub struct TailHandle {
    stop: Option<oneshot::Sender<()>>,
    shared: Arc<Shared>,
    _task: tokio::task::JoinHandle<()>,
}

impl TailHandle {
    /// The byte offset just past the last complete line delivered.
    #[expect(dead_code, reason = "no consumer resumes a tail from its offset yet")]
    #[must_use]
    pub fn offset(&self) -> u64 {
        self.shared.offset.load(Ordering::SeqCst)
    }

    /// How many reads the task has finished, which is how a spec knows the task
    /// has seen the file as it stands before the spec changes it again.
    #[cfg(test)]
    fn reads(&self) -> u64 {
        self.shared.reads.load(Ordering::SeqCst)
    }

    /// How many reads found the file missing.
    #[cfg(test)]
    fn missing_reads(&self) -> u64 {
        self.shared.missing.load(Ordering::SeqCst)
    }

    /// Whether the task is parked because no client is connected.
    #[cfg(test)]
    fn parked(&self) -> bool {
        self.shared.parked.load(Ordering::SeqCst)
    }

    /// Whether a folder watch is active, which a spec waits on to know the
    /// watch was made after its folder appeared.
    #[cfg(test)]
    fn watching(&self) -> bool {
        self.shared.watching.load(Ordering::SeqCst)
    }

    /// Stop the tail, ending its task and dropping its watcher.
    pub fn stop(mut self) {
        self.stop = None;
    }
}

impl Drop for TailHandle {
    fn drop(&mut self) {
        // Dropping the sender wakes the task's select, which ends the task and
        // drops the watcher it owns.
        self.stop = None;
    }
}

/// Start tailing `path` from `start_offset`.
///
/// One task owns all reading and `on_event` runs on it, one call per event.
pub fn spawn(
    path: PathBuf,
    start_offset: u64,
    client_count: watch::Receiver<usize>,
    on_event: impl FnMut(TailEvent) + Send + 'static,
) -> TailHandle {
    let shared = Arc::new(Shared::new(start_offset));
    let (stop_tx, stop_rx) = oneshot::channel();
    let (wake_tx, wake_rx) = mpsc::unbounded_channel();
    let watcher = FolderWatch::start(&path, wake_tx);
    debug!(path = %path.display(), start_offset, "transcript_tail: started");
    let task = tokio::spawn(tail_task(
        path,
        Arc::clone(&shared),
        client_count,
        on_event,
        wake_rx,
        stop_rx,
        watcher,
    ));
    TailHandle {
        stop: Some(stop_tx),
        shared,
        _task: task,
    }
}

/// Where the tailer stands in the file it follows.
struct Cursor {
    /// Offset of the first byte not yet consumed; it only ever sits just past a
    /// complete line.
    offset: u64,
    /// The bytes after the last `\n` read so far, already taken out of the file.
    partial: Vec<u8>,
    /// Whether a line was delivered since the last restart, which is what
    /// decides whether a restart sends [`TailEvent::Restarted`] at all.
    delivered: bool,
    /// The file was missing on the last wake: the next read starts over from 0.
    reset: bool,
    /// Whether a read has finished and sent [`TailEvent::CaughtUp`].
    caught_up: bool,
}

impl Cursor {
    /// Where the next read starts: past the complete lines, and past the held
    /// partial line, whose bytes are already in [`Cursor::partial`].
    fn read_position(&self) -> u64 {
        self.offset + self.partial.len() as u64
    }

    /// Start over from the beginning of the file. Returns whether the consumer
    /// had seen any of the old content, which is when it hears the restart.
    fn restart(&mut self) -> bool {
        let delivered = std::mem::take(&mut self.delivered);
        self.offset = 0;
        self.partial.clear();
        self.reset = false;
        delivered
    }
}

/// The words notify's Windows backend uses for a path that is neither a file
/// nor a directory, which is what a folder that does not exist yet looks like.
const NEITHER_FILE_NOR_DIRECTORY: &str = "Input watch path is neither a file nor a directory";

/// Whether `err` says the watched path does not exist yet: a platform's
/// not-found error, or Windows' refusal of a path that is neither a file nor a
/// directory.
fn is_missing_path(err: &notify::Error) -> bool {
    match &err.kind {
        notify::ErrorKind::PathNotFound => true,
        notify::ErrorKind::Io(io) => io.kind() == std::io::ErrorKind::NotFound,
        notify::ErrorKind::Generic(message) => message.starts_with(NEITHER_FILE_NOR_DIRECTORY),
        _ => false,
    }
}

/// The watcher on the transcript's folder (non-recursively, since the file may
/// not exist yet): made as soon as the folder exists, and made again on a poll
/// tick when the folder was missing at the start.
struct FolderWatch {
    /// The file whose events wake the tail.
    path: PathBuf,
    /// The folder to watch.
    folder: PathBuf,
    /// A wake sender held while a retry is still owed, so the channel stays
    /// open: `wake.recv()` returning `None` means no watch can ever wake the
    /// task again. `None` once the watcher holds the sender itself, or once
    /// nothing more is retried.
    wake: Option<mpsc::UnboundedSender<()>>,
    /// The active watch, once one is made.
    watcher: Option<RecommendedWatcher>,
}

impl FolderWatch {
    /// The watch on `path`'s folder: made now when the folder exists, else owed
    /// to a poll tick.
    fn start(path: &Path, wake: mpsc::UnboundedSender<()>) -> Self {
        let Some(folder) = path.parent() else {
            warn!(
                path = %path.display(),
                "transcript_tail: the transcript has no folder to watch; polling only"
            );
            return Self {
                path: path.to_path_buf(),
                folder: PathBuf::new(),
                wake: None,
                watcher: None,
            };
        };
        let mut watch = Self {
            path: path.to_path_buf(),
            folder: folder.to_path_buf(),
            wake: Some(wake),
            watcher: None,
        };
        watch.make();
        watch
    }

    /// Makes the watch now. The error itself says whether the folder does not
    /// exist yet, which is tried again on the next poll tick; a watcher that
    /// cannot start, or a folder that cannot be watched for another reason, is
    /// reported once and given up on.
    fn make(&mut self) {
        let Some(wake) = self.wake.clone() else {
            return;
        };
        let mut watcher = match watcher_for(&self.path, wake) {
            Ok(watcher) => watcher,
            Err(err) => {
                warn!(
                    path = %self.path.display(),
                    error = %err,
                    "transcript_tail: failed to start the transcript's watcher; polling only"
                );
                self.wake = None;
                return;
            }
        };
        if let Err(err) = watcher.watch(&self.folder, RecursiveMode::NonRecursive) {
            if is_missing_path(&err) {
                debug!(
                    path = %self.path.display(),
                    error = %err,
                    "transcript_tail: the transcript's folder does not exist yet; watching for it on the poll"
                );
            } else {
                warn!(
                    path = %self.path.display(),
                    error = %err,
                    "transcript_tail: failed to watch the transcript's folder; polling only"
                );
                self.wake = None;
            }
            return;
        }
        self.watcher = Some(watcher);
        // The watcher's handler holds a sender now, so the channel closes when
        // the watch does.
        self.wake = None;
        debug!(path = %self.path.display(), "transcript_tail: watching the transcript's folder");
    }

    /// Whether a watch is active.
    #[cfg(test)]
    fn is_watching(&self) -> bool {
        self.watcher.is_some()
    }

    /// Makes the owed watch once the folder appears, and nothing else: a watch
    /// that failed for another reason was already given up on.
    fn retry(&mut self) {
        if self.watcher.is_some() || self.wake.is_none() || !self.folder.exists() {
            return;
        }
        self.make();
    }
}

/// A watcher that wakes `wake` for any event naming `path`.
fn watcher_for(path: &Path, wake: mpsc::UnboundedSender<()>) -> notify::Result<RecommendedWatcher> {
    let handler_path = path.to_path_buf();
    notify::recommended_watcher(move |result: notify::Result<notify::Event>| match result {
        Ok(event) => {
            if event
                .paths
                .iter()
                .any(|event_path| event_path == &handler_path)
            {
                let _ = wake.send(());
            }
        }
        Err(err) => {
            debug!(
                path = %handler_path.display(),
                error = %err,
                "transcript_tail: watcher error"
            );
        }
    })
}

/// Records whether a watch is active, for the specs that wait on one.
#[cfg(test)]
fn note_watch(shared: &Shared, watch: &FolderWatch) {
    shared.watching.store(watch.is_watching(), Ordering::SeqCst);
}

/// Nothing to record in a build without the specs' observation.
#[cfg(not(test))]
fn note_watch(_shared: &Shared, _watch: &FolderWatch) {}

/// Drop every wakeup already queued, so one burst of events costs one read.
fn drain(wake: &mut mpsc::UnboundedReceiver<()>) {
    while wake.try_recv().is_ok() {}
}

/// The tailing task: the only reader of the file. Parks while no client is
/// connected and catches up with one read when one arrives.
async fn tail_task(
    path: PathBuf,
    shared: Arc<Shared>,
    mut client_count: watch::Receiver<usize>,
    mut on_event: impl FnMut(TailEvent) + Send + 'static,
    mut wake: mpsc::UnboundedReceiver<()>,
    mut stop: oneshot::Receiver<()>,
    mut folder_watch: FolderWatch,
) {
    let start_offset = shared.offset.load(Ordering::SeqCst);
    note_watch(&shared, &folder_watch);
    let mut cursor = Cursor {
        offset: start_offset,
        partial: Vec::new(),
        // A start offset past the end is a shrink the consumer must hear about,
        // so it counts as content the consumer has already been given.
        delivered: start_offset > 0,
        reset: false,
        caught_up: false,
    };
    let mut poll = tokio::time::interval(POLL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // A closed wake channel means no watcher wakes the task; the poll goes on.
    let mut watched = true;
    loop {
        let has_clients = *client_count.borrow_and_update() > 0;
        #[cfg(test)]
        shared.parked.store(!has_clients, Ordering::SeqCst);
        if has_clients {
            tokio::select! {
                _ = &mut stop => break,
                changed = client_count.changed() => {
                    if changed.is_err() { break; }
                }
                woken = wake.recv(), if watched => {
                    if woken.is_none() {
                        watched = false;
                        debug!(path = %path.display(), "transcript_tail: no watcher; polling only");
                        continue;
                    }
                    drain(&mut wake);
                    read_wake(&path, &mut cursor, &shared, &mut on_event);
                }
                _ = poll.tick() => {
                    folder_watch.retry();
                    note_watch(&shared, &folder_watch);
                    drain(&mut wake);
                    read_wake(&path, &mut cursor, &shared, &mut on_event);
                }
            }
        } else {
            tokio::select! {
                _ = &mut stop => break,
                changed = client_count.changed() => {
                    if changed.is_err() { break; }
                    if *client_count.borrow() > 0 {
                        drain(&mut wake);
                        read_wake(&path, &mut cursor, &shared, &mut on_event);
                    }
                }
                woken = wake.recv(), if watched => {
                    if woken.is_none() {
                        watched = false;
                        debug!(path = %path.display(), "transcript_tail: no watcher; polling only");
                        continue;
                    }
                    drain(&mut wake);
                }
                _ = poll.tick() => {
                    folder_watch.retry();
                    note_watch(&shared, &folder_watch);
                    drain(&mut wake);
                }
            }
        }
    }
    debug!(path = %path.display(), "transcript_tail: stopped");
}

/// One wake's read, ended by [`TailEvent::CaughtUp`] when it is the first or
/// it restarted, and counted for a spec that polls [`TailHandle::reads`].
fn read_wake(
    path: &Path,
    cursor: &mut Cursor,
    shared: &Shared,
    on_event: &mut impl FnMut(TailEvent),
) {
    let restarted = read_once(path, cursor, shared, on_event);
    if restarted || !cursor.caught_up {
        cursor.caught_up = true;
        on_event(TailEvent::CaughtUp);
    }
    #[cfg(test)]
    shared.reads.fetch_add(1, Ordering::SeqCst);
}

/// Read whatever the file holds past the cursor's read position, delivering one
/// [`TailEvent::Line`] per complete line and holding a partial last line for the
/// next wake. Returns whether it sent [`TailEvent::Restarted`].
fn read_once(
    path: &Path,
    cursor: &mut Cursor,
    shared: &Shared,
    on_event: &mut impl FnMut(TailEvent),
) -> bool {
    let length = match std::fs::metadata(path) {
        Ok(metadata) => metadata.len(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            // Nothing to send; a file that appears later starts from 0, and a
            // consumer that saw the old one hears so before its first line.
            cursor.reset = cursor.delivered;
            cursor.offset = 0;
            cursor.partial.clear();
            shared.offset.store(0, Ordering::SeqCst);
            #[cfg(test)]
            shared.missing.fetch_add(1, Ordering::SeqCst);
            return false;
        }
        Err(err) => {
            debug!(
                path = %path.display(),
                error = %err,
                "transcript_tail: the transcript could not be read"
            );
            return false;
        }
    };
    let mut restarted = false;
    if length < cursor.read_position() || cursor.reset {
        if cursor.restart() {
            on_event(TailEvent::Restarted);
            restarted = true;
        }
        debug!(path = %path.display(), "transcript_tail: restarted");
    }
    if length <= cursor.read_position() {
        return restarted;
    }
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) => {
            debug!(
                path = %path.display(),
                error = %err,
                "transcript_tail: the transcript could not be opened"
            );
            return restarted;
        }
    };
    if let Err(err) = file.seek(SeekFrom::Start(cursor.read_position())) {
        debug!(
            path = %path.display(),
            error = %err,
            "transcript_tail: could not seek in the transcript"
        );
        return restarted;
    }
    let mut appended = Vec::new();
    if let Err(err) = file.read_to_end(&mut appended) {
        debug!(
            path = %path.display(),
            error = %err,
            "transcript_tail: could not read the transcript"
        );
        return restarted;
    }
    cursor.partial.extend_from_slice(&appended);
    let consumed = deliver_lines(&cursor.partial, on_event);
    if consumed > 0 {
        cursor.partial.drain(..consumed);
        cursor.offset += consumed as u64;
        cursor.delivered = true;
    }
    shared.offset.store(cursor.offset, Ordering::SeqCst);
    restarted
}

/// Deliver one event per complete line in `bytes`, returning how many bytes the
/// delivered lines took up. The bytes after the last `\n` are left alone.
fn deliver_lines(bytes: &[u8], on_event: &mut impl FnMut(TailEvent)) -> usize {
    let mut consumed = 0;
    while let Some(newline) = bytes[consumed..].iter().position(|byte| *byte == b'\n') {
        let end = consumed + newline;
        let mut line = &bytes[consumed..end];
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        on_event(TailEvent::Line(String::from_utf8_lossy(line).into_owned()));
        consumed = end + 1;
    }
    consumed
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::expect_used,
        reason = "tests assert their fixture preconditions; the messages name the step that failed"
    )]

    use super::{TailEvent, TailHandle, is_missing_path, spawn};
    use crate::server::test_support::ScratchDir;
    use std::io::Write as _;
    use std::path::{Path, PathBuf};
    use std::time::Duration;
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
    use tokio::sync::watch;

    /// A passing test never reaches this: it fails only when the product is
    /// broken, so it is sized by the slow path it rules out.
    const HANG: Duration = Duration::from_secs(30);

    /// How often a wait checks the task's counters. The sleep lets the task run
    /// between checks; the hang guard is the only bound on how long it takes.
    const CHECK: Duration = Duration::from_millis(5);

    fn write_file(path: &Path, text: &str) {
        std::fs::write(path, text).expect("write the transcript");
    }

    fn append_file(path: &Path, text: &str) {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .expect("open the transcript for appending");
        file.write_all(text.as_bytes())
            .expect("append to the transcript");
    }

    fn tail(
        path: PathBuf,
        start_offset: u64,
        clients: watch::Receiver<usize>,
    ) -> (TailHandle, UnboundedReceiver<TailEvent>) {
        let (tx, rx) = unbounded_channel();
        // These specs follow the lines and restarts; the chat mirror's own
        // tests cover the end of a read.
        let handle = spawn(path, start_offset, clients, move |event| {
            if event != TailEvent::CaughtUp {
                let _ = tx.send(event);
            }
        });
        (handle, rx)
    }

    async fn next_event(events: &mut UnboundedReceiver<TailEvent>) -> TailEvent {
        tokio::time::timeout(HANG, events.recv())
            .await
            .expect("hung: the tailer sent no event within the hang guard")
            .expect("the tailer's event channel closed")
    }

    /// Wait until the task has finished `reads` reads, so the next change to the
    /// file is read by a later wake than the one being tested.
    async fn wait_for_reads(tail: &TailHandle, reads: u64) {
        tokio::time::timeout(HANG, async {
            while tail.reads() < reads {
                tokio::time::sleep(CHECK).await;
            }
        })
        .await
        .expect("hung: the tailer never made the expected read");
    }

    /// Wait until the task has read the file at least once and found it missing.
    async fn wait_for_a_missing_read(tail: &TailHandle) {
        tokio::time::timeout(HANG, async {
            while tail.missing_reads() == 0 {
                tokio::time::sleep(CHECK).await;
            }
        })
        .await
        .expect("hung: the tailer never saw the transcript missing");
    }

    /// Wait until the tail's folder watch is active.
    async fn wait_until_watching(tail: &TailHandle) {
        tokio::time::timeout(HANG, async {
            while !tail.watching() {
                tokio::time::sleep(CHECK).await;
            }
        })
        .await
        .expect("hung waiting for the folder watch");
    }

    /// Wait until the task has parked with no client connected.
    async fn wait_until_parked(tail: &TailHandle) {
        tokio::time::timeout(HANG, async {
            while !tail.parked() {
                tokio::time::sleep(CHECK).await;
            }
        })
        .await
        .expect("hung: the tailer never parked");
    }

    fn line(text: &str) -> TailEvent {
        TailEvent::Line(text.to_owned())
    }

    #[tokio::test]
    async fn appended_lines_arrive_once_each() {
        let scratch = ScratchDir::new("tail-appended");
        let path = scratch.path().join("session.jsonl");
        write_file(&path, "a\nb\n");
        let (_clients, clients_rx) = watch::channel(1usize);
        let (_tail, mut events) = tail(path.clone(), 0, clients_rx);

        assert_eq!(next_event(&mut events).await, line("a"));
        assert_eq!(next_event(&mut events).await, line("b"));

        append_file(&path, "c\n");
        assert_eq!(next_event(&mut events).await, line("c"));

        append_file(&path, "d\n");
        assert_eq!(next_event(&mut events).await, line("d"));
    }

    #[tokio::test]
    async fn a_half_written_line_waits_for_its_newline() {
        let scratch = ScratchDir::new("tail-partial");
        let path = scratch.path().join("session.jsonl");
        write_file(&path, "par");
        let (_clients, clients_rx) = watch::channel(1usize);
        let (tail, mut events) = tail(path.clone(), 0, clients_rx);

        // The task must have read "par" before the rest arrives, or both parts
        // are read at once and never need joining.
        wait_for_reads(&tail, 1).await;
        append_file(&path, "tial\nnext\n");
        assert_eq!(next_event(&mut events).await, line("partial"));
        assert_eq!(next_event(&mut events).await, line("next"));
    }

    #[tokio::test]
    async fn a_truncated_file_starts_over() {
        let scratch = ScratchDir::new("tail-truncated");
        let path = scratch.path().join("session.jsonl");
        write_file(&path, "one\ntwo\n");
        let (_clients, clients_rx) = watch::channel(1usize);
        let (_tail, mut events) = tail(path.clone(), 0, clients_rx);

        assert_eq!(next_event(&mut events).await, line("one"));
        assert_eq!(next_event(&mut events).await, line("two"));

        write_file(&path, "x\n");
        assert_eq!(next_event(&mut events).await, TailEvent::Restarted);
        assert_eq!(next_event(&mut events).await, line("x"));
    }

    #[tokio::test]
    async fn a_missing_file_yields_nothing_until_it_appears() {
        let scratch = ScratchDir::new("tail-missing");
        let path = scratch.path().join("session.jsonl");
        let (_clients, clients_rx) = watch::channel(1usize);
        let (tail, mut events) = tail(path.clone(), 0, clients_rx);

        // The file appears only after the task has found it missing.
        wait_for_a_missing_read(&tail).await;
        write_file(&path, "first\n");
        assert_eq!(next_event(&mut events).await, line("first"));
    }

    #[tokio::test]
    async fn a_tail_started_before_its_folder_exists_still_delivers_lines_written_later() {
        let scratch = ScratchDir::new("tail-no-folder");
        let folder = scratch.path().join("projects").join("not-yet");
        let path = folder.join("session.jsonl");
        let (_clients, clients_rx) = watch::channel(1usize);
        // No folder to watch: the tail runs on its poll alone.
        let (_tail, mut events) = tail(path.clone(), 0, clients_rx);

        std::fs::create_dir_all(&folder).expect("create the transcript's folder");
        write_file(&path, "first\n");
        assert_eq!(next_event(&mut events).await, line("first"));
    }

    #[test]
    fn a_missing_path_error_is_retried_and_others_are_not() {
        assert!(is_missing_path(&notify::Error::new(
            notify::ErrorKind::PathNotFound
        )));
        let not_found = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert!(is_missing_path(&notify::Error::io(not_found)));
        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert!(!is_missing_path(&notify::Error::io(denied)));
        assert!(!is_missing_path(&notify::Error::generic("other")));
        // notify's Windows backend refuses a path that is neither a file nor a
        // directory, which is what a folder that does not exist yet looks like.
        assert!(is_missing_path(&notify::Error::generic(
            "Input watch path is neither a file nor a directory."
        )));
    }

    #[tokio::test]
    async fn a_missing_folder_is_watched_once_it_appears() {
        let scratch = ScratchDir::new("tail-late-watch");
        let folder = scratch.path().join("projects").join("not-yet");
        let path = folder.join("session.jsonl");
        let (_clients, clients_rx) = watch::channel(1usize);
        let (tail, mut events) = tail(path.clone(), 0, clients_rx);

        assert!(!tail.watching(), "no folder exists to watch yet");
        std::fs::create_dir_all(&folder).expect("create the transcript's folder");
        write_file(&path, "");
        wait_until_watching(&tail).await;

        append_file(&path, "first\n");
        assert_eq!(next_event(&mut events).await, line("first"));
    }

    #[tokio::test]
    async fn a_parked_tail_catches_up_on_connect() {
        let scratch = ScratchDir::new("tail-parked");
        let path = scratch.path().join("session.jsonl");
        write_file(&path, "early\n");
        let (clients, clients_rx) = watch::channel(0usize);
        let (tail, mut events) = tail(path.clone(), 0, clients_rx);

        wait_until_parked(&tail).await;
        clients.send(1).expect("connect a client");
        assert_eq!(next_event(&mut events).await, line("early"));

        clients.send(0).expect("disconnect the client");
        wait_until_parked(&tail).await;
        append_file(&path, "late\n");
        clients.send(1).expect("reconnect a client");
        assert_eq!(next_event(&mut events).await, line("late"));
    }

    #[tokio::test]
    async fn a_start_offset_skips_old_lines() {
        let scratch = ScratchDir::new("tail-offset");
        let path = scratch.path().join("session.jsonl");
        write_file(&path, "old\nnew\n");
        let (_clients, clients_rx) = watch::channel(1usize);
        let (_tail, mut events) = tail(path.clone(), 4, clients_rx);

        assert_eq!(next_event(&mut events).await, line("new"));
    }

    #[tokio::test]
    async fn a_start_offset_past_the_end_restarts() {
        let scratch = ScratchDir::new("tail-offset-past-end");
        let path = scratch.path().join("session.jsonl");
        write_file(&path, "ab\n");
        let (_clients, clients_rx) = watch::channel(1usize);
        let (_tail, mut events) = tail(path.clone(), 100, clients_rx);

        assert_eq!(next_event(&mut events).await, TailEvent::Restarted);
        assert_eq!(next_event(&mut events).await, line("ab"));
    }

    #[tokio::test]
    async fn a_file_that_vanishes_and_returns_restarts() {
        let scratch = ScratchDir::new("tail-vanishes");
        let path = scratch.path().join("session.jsonl");
        write_file(&path, "one\n");
        let (_clients, clients_rx) = watch::channel(1usize);
        let (tail, mut events) = tail(path.clone(), 0, clients_rx);

        assert_eq!(next_event(&mut events).await, line("one"));
        std::fs::remove_file(&path).expect("delete the transcript");
        wait_for_a_missing_read(&tail).await;
        write_file(&path, "two\n");
        assert_eq!(next_event(&mut events).await, TailEvent::Restarted);
        assert_eq!(next_event(&mut events).await, line("two"));
    }

    #[tokio::test]
    async fn crlf_lines_lose_their_carriage_return() {
        let scratch = ScratchDir::new("tail-crlf");
        let path = scratch.path().join("session.jsonl");
        write_file(&path, "w\r\n");
        let (_clients, clients_rx) = watch::channel(1usize);
        let (_tail, mut events) = tail(path.clone(), 0, clients_rx);

        assert_eq!(next_event(&mut events).await, line("w"));
    }
}
