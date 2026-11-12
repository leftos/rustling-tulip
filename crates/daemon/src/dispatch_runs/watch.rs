//! The live-run folder's watcher: debounced file events, the periodic full
//! scan and the liveness tick, all on one task that owns every listing and
//! removal (`docs/plans/dispatch-follow.md` sections 6 and 10).

use super::guard::{self, Liveness, Verdict};
use super::parse::{LiveRunFile, Parsed, is_run_file, parse};
use super::tail::{self, Tailer};
use super::{DispatchRuns, Timing};
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
        kept_logged: HashSet::new(),
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
    /// The dead-child files already logged as kept for their runner, so each
    /// is logged once; a path leaves it when its file is deleted or listed.
    kept_logged: HashSet<PathBuf>,
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
    /// the stop markers with no run file beside them. The markers go last, so
    /// a run file this pass deleted takes its marker in the same pass; a run
    /// file kept for a runner that may be alive is what protects its marker.
    fn scan(&mut self) {
        let entries = match std::fs::read_dir(&self.folder) {
            Ok(entries) => entries,
            Err(err) => {
                warn!(folder = %self.folder.display(), %err, "dispatch_runs: reading the live-run folder failed");
                return;
            }
        };
        let paths: Vec<PathBuf> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
        self.kept_logged.retain(|path| paths.contains(path));
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
    /// only once its guard passes; a file that fails it is never listed, and
    /// is deleted only once its runner is dead or not recorded. A file that
    /// does not parse is skipped and tried again on the next event or scan,
    /// never removing a listed run.
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
        let child = guard::liveness(file.child_pid, file.child_start, file.started_at);
        if !should_list(self.kept_logged.contains(path), child) {
            self.drop_dead_file(&run_id, path, &file);
            return;
        }
        self.kept_logged.remove(path);
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

    /// An unlisted file whose child is dead: never listed; deleted only when
    /// its runner is dead or not recorded, else kept for the runner, which
    /// still reads the stop marker, and logged once.
    fn drop_dead_file(&mut self, run_id: &str, path: &Path, file: &LiveRunFile) {
        let runner = guard::runner_liveness(file.runner_pid, file.runner_start, file.started_at);
        if guard::verdict(Liveness::Dead, runner) == (Verdict::Gone { delete_files: true }) {
            info!(
                run_id,
                pid = file.child_pid,
                "dispatch_runs: a run file names a dead process; deleted"
            );
            self.kept_logged.remove(path);
            delete_file(path);
        } else if self.kept_logged.insert(path.to_path_buf()) {
            info!(
                run_id,
                pid = file.child_pid,
                runner_pid = file.runner_pid,
                "dispatch_runs: a run file names a dead process; kept for its runner"
            );
        }
    }

    /// Drop every run whose child is gone or is not the one its file
    /// recorded. Its file is deleted first only when its runner is dead or
    /// not recorded; the runner is read only for a dead child.
    fn check_liveness(&mut self) {
        for target in self.runs.guard_targets() {
            let child = guard::liveness(target.child_pid, target.child_start, target.started_at);
            if child != Liveness::Dead {
                continue;
            }
            let runner =
                guard::runner_liveness(target.runner_pid, target.runner_start, target.started_at);
            let Verdict::Gone { delete_files } = guard::verdict(child, runner) else {
                continue;
            };
            let path = self.folder.join(format!("{}.json", target.run_id));
            if delete_files {
                info!(
                    run_id = target.run_id,
                    pid = target.child_pid,
                    "dispatch_runs: a run's process is gone; dropped"
                );
                delete_file(&path);
            } else {
                info!(
                    run_id = target.run_id,
                    pid = target.child_pid,
                    runner_pid = target.runner_pid,
                    "dispatch_runs: a run's process is gone; dropped, its file kept for its runner"
                );
                self.kept_logged.insert(path);
            }
            self.runs
                .remove_generation(&target.run_id, target.generation);
        }
    }
}

/// Whether an unlisted run file's run may be listed: its child is not known
/// dead, and the watcher has not already dropped it (`kept`, a dead child's
/// file kept for its runner), so a dropped run is never listed again while
/// its file remains, even once its child's pid reads unknown or alive.
pub(super) fn should_list(kept: bool, child: Liveness) -> bool {
    !kept && child != Liveness::Dead
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
