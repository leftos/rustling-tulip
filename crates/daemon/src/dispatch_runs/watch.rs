//! The live-run folder's watcher: debounced file events, the periodic full
//! scan and the liveness tick, all on one task that owns every listing and
//! removal (`docs/plans/dispatch-follow.md` sections 6 and 10).

use super::parse::{Parsed, is_run_file, parse};
use super::tail::{self, Tailer};
use super::{DispatchRuns, Timing, guard};
use anyhow::Context as _;
use notify::RecursiveMode;
use notify_debouncer_full::new_debouncer;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Instant, Interval, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// Watch `folder` (which exists), scan it once, and run the watcher's task
/// until `shutdown`. Must be called inside a Tokio runtime: the scan starts
/// the tailers of the runs it lists.
pub(super) fn start(
    folder: PathBuf,
    runs: Arc<DispatchRuns>,
    timing: Timing,
    shutdown: CancellationToken,
) -> anyhow::Result<JoinHandle<()>> {
    let (tx, rx) = mpsc::unbounded_channel::<PathBuf>();
    let callback_folder = folder.clone();
    let mut debouncer = new_debouncer(
        timing.debounce,
        None,
        move |result: notify_debouncer_full::DebounceEventResult| match result {
            Ok(events) => {
                for path in events.iter().flat_map(|event| event.paths.iter()) {
                    if is_run_file(path) {
                        let _ = tx.send(path.clone());
                    }
                }
            }
            Err(errs) => {
                for e in errs {
                    warn!(
                        folder = %callback_folder.display(),
                        error = %e,
                        "dispatch_runs: live-run folder debouncer error"
                    );
                }
            }
        },
    )
    .context("creating the live-run folder's debouncer")?;
    debouncer
        .watch(&folder, RecursiveMode::NonRecursive)
        .with_context(|| format!("watching the live-run folder {}", folder.display()))?;
    let mut watcher = Watcher {
        folder,
        runs,
        timing,
        shutdown,
        unknown_logged: HashSet::new(),
    };
    // The startup scan lists the runs already there before `spawn` returns,
    // so a caller ordering work after it (DF.6's tab prune) sees them.
    watcher.scan();
    Ok(tokio::spawn(async move {
        // The debouncer stops watching when dropped, so the task keeps it.
        let _debouncer = debouncer;
        watcher.run(rx).await;
    }))
}

/// The watcher task's state.
struct Watcher {
    folder: PathBuf,
    runs: Arc<DispatchRuns>,
    timing: Timing,
    shutdown: CancellationToken,
    /// The unknown-schema files already logged, so each is logged once.
    unknown_logged: HashSet<PathBuf>,
}

impl Watcher {
    /// Follow events, scans and liveness ticks until shutdown; the startup
    /// scan already ran in [`start`]. The runs' tailers hold child tokens of
    /// `shutdown`, so they stop with it.
    async fn run(mut self, mut paths: mpsc::UnboundedReceiver<PathBuf>) {
        let mut scan = interval_after(self.timing.scan);
        let mut liveness = interval_after(self.timing.liveness);
        loop {
            tokio::select! {
                () = self.shutdown.cancelled() => break,
                Some(path) = paths.recv() => self.on_path(&path),
                _ = scan.tick() => self.scan(),
                _ = liveness.tick() => self.check_liveness(),
            }
        }
    }

    /// A debounced event on a run file: a file that is there is read again,
    /// one that is gone removes its run.
    fn on_path(&mut self, path: &Path) {
        if path.exists() {
            self.upsert(path);
        } else if let Some(run_id) = run_id_of(path) {
            self.runs.remove(&run_id);
        }
    }

    /// Read every run file, remove the runs whose file is gone, and delete
    /// the stop markers with no run file beside them.
    fn scan(&mut self) {
        let entries = match std::fs::read_dir(&self.folder) {
            Ok(entries) => entries,
            Err(err) => {
                warn!(folder = %self.folder.display(), %err, "dispatch_runs: reading the live-run folder failed");
                return;
            }
        };
        let paths: Vec<PathBuf> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
        let mut present = HashSet::new();
        for path in paths.iter().filter(|path| is_run_file(path)) {
            present.extend(run_id_of(path));
            self.upsert(path);
        }
        for run_id in self.runs.run_ids() {
            if !present.contains(&run_id) {
                self.runs.remove(&run_id);
            }
        }
        for path in paths.iter().filter(|path| is_stop_marker(path)) {
            if !path.with_extension("json").exists() {
                delete_file(path);
            }
        }
    }

    /// Read one run file and list, update or skip its run. A run is listed
    /// only once its guard passes; a file that fails it is deleted. A file
    /// that does not parse is skipped and tried again on the next event or
    /// scan, never removing a listed run.
    fn upsert(&mut self, path: &Path) {
        let Some(run_id) = run_id_of(path) else {
            return;
        };
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) => {
                debug!(path = %path.display(), %err, "dispatch_runs: reading a run file failed");
                return;
            }
        };
        let file = match parse(&text) {
            Parsed::Run(file) => file,
            Parsed::UnknownSchema(schema) => {
                if self.unknown_logged.insert(path.to_path_buf()) {
                    info!(path = %path.display(), schema, "dispatch run file has an unknown schema; skipped");
                }
                return;
            }
            Parsed::Invalid(reason) => {
                debug!(path = %path.display(), %reason, "dispatch_runs: a run file did not parse; retried later");
                return;
            }
        };
        if self.runs.listed_file(&run_id).is_some() {
            let cancel = self.shutdown.child_token();
            if let Some(generation) = self.runs.replace(&run_id, file.clone(), cancel.clone()) {
                self.start_tailer(run_id, generation, &file, cancel);
            }
            return;
        }
        if !guard::process_alive(file.child_pid, file.child_start, file.started_at) {
            info!(
                run_id,
                pid = file.child_pid,
                "dispatch_runs: a run file names a dead process; deleted"
            );
            delete_file(path);
            return;
        }
        let main_checkout = crate::git::repo_path_for_worktree(&file.worktree);
        let cancel = self.shutdown.child_token();
        let generation = self
            .runs
            .insert(&run_id, file.clone(), main_checkout, cancel.clone());
        self.start_tailer(run_id, generation, &file, cancel);
    }

    /// Start the tailer of the run listed as `run_id` at `generation`.
    fn start_tailer(
        &self,
        run_id: String,
        generation: u64,
        file: &super::parse::LiveRunFile,
        cancel: CancellationToken,
    ) {
        let tailer = Tailer {
            runs: Arc::clone(&self.runs),
            run_id,
            generation,
            stream: file.stream.clone(),
            agent: file.agent_id(),
            period: self.timing.tail,
        };
        tokio::spawn(tail::run(tailer, cancel));
    }

    /// Drop every run whose process is gone or is not the one its file
    /// recorded, deleting its file first.
    fn check_liveness(&self) {
        for target in self.runs.guard_targets() {
            if guard::process_alive(target.child_pid, target.child_start, target.started_at) {
                continue;
            }
            info!(
                run_id = target.run_id,
                pid = target.child_pid,
                "dispatch_runs: a run's process is gone; dropped"
            );
            delete_file(&self.folder.join(format!("{}.json", target.run_id)));
            self.runs
                .remove_generation(&target.run_id, target.generation);
        }
    }
}

/// An interval whose first tick is one `period` from now.
fn interval_after(period: std::time::Duration) -> Interval {
    let mut interval = tokio::time::interval_at(Instant::now() + period, period);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    interval
}

/// A run file's run id: its name without the extension.
fn run_id_of(path: &Path) -> Option<String> {
    path.file_stem()?.to_str().map(str::to_owned)
}

/// Whether `path` is a stop marker, `<run_id>.stop`.
fn is_stop_marker(path: &Path) -> bool {
    path.extension() == Some(OsStr::new("stop"))
}

/// Delete `path`, a file the tool may be deleting at the same moment: a
/// missing file is fine.
fn delete_file(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            warn!(path = %path.display(), %err, "dispatch_runs: deleting a stale file failed");
        }
    }
}
