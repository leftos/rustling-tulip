//! Per-repo filesystem watchers that keep the source-control sidebar live
//! without user-triggered refreshes.
//!
//! Architecture:
//!
//! - One [`notify_debouncer_full::Debouncer`] per registered repo, watching
//!   the repo root recursively. The debouncer coalesces bursts (`git add`
//!   writes several files in `.git/` for a single user action) into one
//!   notification per ~250ms quiet window.
//! - Each debouncer's callback pings a tokio `UnboundedSender<()>` owned
//!   by a per-repo refresher task. The refresher drains the backlog, runs
//!   `git_inspect::repo_status` + `git_write::stash_list` in sequence, and
//!   broadcasts the results through the existing
//!   [`crate::server::StateEvent`] channel — so every connected client's
//!   sidebar updates without re-requesting.
//! - Repo lifecycle is driven by `StateEvent::Repos` broadcasts: when a
//!   repo is added, a watcher is spawned; when one is removed, the
//!   corresponding handle is dropped, which stops the debouncer and (via
//!   the tx → rx drop chain) terminates the refresher task.
//!
//! Out of scope: live `Commits` refresh (history view stays user-driven),
//! file-diff invalidation for open Monaco tabs (snapshot semantics by
//! design), and explicit `.git/objects/`-only filtering (defer until
//! profiling shows the broadcast traffic matters).

use crate::server::StateEvent;
use crate::state::AppState;
use crate::{git_inspect, git_write};
use notify::RecursiveMode;
use notify_debouncer_full::new_debouncer;
use protocol::RepoEntry;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Debounce window for the per-repo watcher. Large enough to coalesce a
/// `git commit` burst (several `.git/` writes in close succession) into a
/// single refresh, small enough that interactive feel still feels "live".
const DEBOUNCE_MS: u64 = 250;

/// Owns a per-repo watcher pair. Dropping this stops the OS-level
/// watcher (via the debouncer's Drop impl) and indirectly terminates the
/// refresher task: the closure inside the debouncer holds the unique
/// `UnboundedSender<()>`, so dropping the debouncer drops the sender,
/// which makes the refresher's `recv()` return `None` and the task exits.
struct RepoWatcher {
    /// `Box<dyn Drop>` so we can store debouncers without naming their
    /// generic parameters in the field type. The concrete type is
    /// `Debouncer<RecommendedWatcher, RecommendedCache>` but the public
    /// signature changes between platforms and pinning it in a struct is
    /// noise. We never need to call methods on the debouncer after
    /// `watch`, so a `dyn Any + Send` would also work; `Send + Sync`
    /// keeps it usable from `tokio::spawn`'d cleanup paths.
    _debouncer: Box<dyn std::any::Any + Send + Sync>,
    _refresher: tokio::task::JoinHandle<()>,
}

/// Public entry point. Spawns the supervisor task that owns the per-repo
/// watcher set; the returned handle is only used to keep the supervisor
/// alive for the daemon's lifetime.
pub fn start(state: &Arc<AppState>, state_events: &broadcast::Sender<StateEvent>) {
    let handles = Arc::new(AsyncMutex::new(HashMap::<String, RepoWatcher>::new()));

    // Seed initial watchers from the current registry. Doing this here
    // (instead of waiting for the first broadcast) means a freshly
    // started daemon picks up file changes immediately.
    let initial = state.with_persisted(|s| s.repos.clone());
    let handles_for_seed = Arc::clone(&handles);
    let state_events_for_seed = state_events.clone();
    tokio::spawn(async move {
        let mut guard = handles_for_seed.lock().await;
        for repo in initial {
            spawn_watcher(&repo, &state_events_for_seed, &mut guard);
        }
        drop(guard);
    });

    // Lifecycle supervisor: react to StateEvent::Repos broadcasts and
    // sync the handle set against the new registry.
    let mut rx = state_events.subscribe();
    let state_events_for_sync = state_events.clone();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(StateEvent::Repos(repos)) => {
                    let mut guard = handles.lock().await;
                    sync_handles(&repos, &state_events_for_sync, &mut guard);
                }
                Ok(_) => {
                    // We don't care about Workspaces / RepoStatus / Stashes
                    // events — those are downstream of our refreshes.
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(lagged = n, "git_watch: state event stream lagged");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
        info!("git_watch: supervisor exiting (channel closed)");
    });
}

/// Add watchers for newly-added repos and drop watchers for repos that
/// disappeared from the registry. Existing entries are left untouched —
/// re-creating a watcher on every broadcast would tear down and rebuild
/// the OS handle for no reason.
fn sync_handles(
    repos: &[RepoEntry],
    state_events: &broadcast::Sender<StateEvent>,
    handles: &mut HashMap<String, RepoWatcher>,
) {
    let current_ids: std::collections::HashSet<&str> =
        repos.iter().map(|r| r.id.as_str()).collect();
    handles.retain(|id, _| {
        let keep = current_ids.contains(id.as_str());
        if !keep {
            debug!(repo_id = id, "git_watch: stopping watcher for removed repo");
        }
        keep
    });
    for repo in repos {
        if !handles.contains_key(&repo.id) {
            spawn_watcher(repo, state_events, handles);
        }
    }
}

/// Create the debouncer + refresher task pair for one repo and store
/// them in the handle map. Silently logs and skips on failure (a missing
/// directory or an OS handle exhaustion shouldn't kill the supervisor).
fn spawn_watcher(
    repo: &RepoEntry,
    state_events: &broadcast::Sender<StateEvent>,
    handles: &mut HashMap<String, RepoWatcher>,
) {
    let repo_path = PathBuf::from(&repo.path);
    let repo_id = repo.id.clone();
    if !repo_path.is_dir() {
        warn!(repo_id, path = %repo.path, "git_watch: repo path not a directory; skipping watcher");
        return;
    }

    // notify -> refresher trigger. UnboundedReceiver is fine: the
    // refresher always drains to empty before re-awaiting, so the queue
    // never grows past one logical "burst pending".
    let (tx, mut rx) = mpsc::unbounded_channel::<()>();

    let debouncer_result = new_debouncer(
        Duration::from_millis(DEBOUNCE_MS),
        None,
        move |result: notify_debouncer_full::DebounceEventResult| match result {
            Ok(_events) => {
                let _ = tx.send(());
            }
            Err(errs) => {
                for e in errs {
                    warn!(error = %e, "git_watch: debouncer error");
                }
            }
        },
    );
    let mut debouncer = match debouncer_result {
        Ok(d) => d,
        Err(err) => {
            warn!(repo_id, ?err, "git_watch: failed to create debouncer");
            return;
        }
    };
    if let Err(err) = debouncer.watch(&repo_path, RecursiveMode::Recursive) {
        warn!(repo_id, ?err, "git_watch: failed to start watching repo");
        return;
    }

    let state_events = state_events.clone();
    let refresher_repo_path = repo_path.clone();
    let refresher_repo_id = repo_id.clone();
    let refresher = tokio::spawn(async move {
        while rx.recv().await.is_some() {
            // Drain any backlog: a burst on the channel folds into a
            // single refresh, not N. We're already past the debouncer's
            // quiescence window, so any queued items are redundant.
            while rx.try_recv().is_ok() {}

            match git_inspect::repo_status(&refresher_repo_path).await {
                Ok((index_changes, worktree_changes)) => {
                    let _ = state_events.send(StateEvent::RepoStatus {
                        repo_id: refresher_repo_id.clone(),
                        index_changes,
                        worktree_changes,
                    });
                }
                Err(err) => {
                    warn!(repo_id = %refresher_repo_id, ?err, "git_watch: repo_status failed");
                }
            }
            match git_write::stash_list(&refresher_repo_path).await {
                Ok(stashes) => {
                    let _ = state_events.send(StateEvent::Stashes {
                        repo_id: refresher_repo_id.clone(),
                        stashes,
                    });
                }
                Err(err) => {
                    warn!(repo_id = %refresher_repo_id, ?err, "git_watch: stash_list failed");
                }
            }
        }
        debug!(repo_id = %refresher_repo_id, "git_watch: refresher task exiting");
    });

    info!(repo_id, path = %repo.path, "git_watch: watching repo");
    handles.insert(
        repo_id,
        RepoWatcher {
            _debouncer: Box::new(debouncer),
            _refresher: refresher,
        },
    );
}
