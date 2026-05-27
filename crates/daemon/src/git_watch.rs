//! Per-repo filesystem watchers that keep the source-control sidebar live
//! without user-triggered refreshes.
//!
//! Architecture:
//!
//! - One [`notify_debouncer_full::Debouncer`] per registered repo, watching
//!   the repo root recursively. The debouncer coalesces bursts (`git add`
//!   writes several files in `.git/` for a single user action) into one
//!   notification per quiet window.
//! - The debouncer callback runs [`classify_event`] over every event path
//!   in the batch. Paths under `.git/objects/`, `.git/logs/` (except the
//!   stash reflog), `.git/FETCH_HEAD`, etc. and paths inside well-known
//!   build/cache directories (`target/`, `node_modules/`, `dist/`, …) are
//!   discarded; only paths that can actually change `git status` or `git
//!   stash list` output produce a wakeup. Without this filter, anything
//!   that ran `cargo build` or wrote to `node_modules/` would keep firing
//!   refreshes forever.
//! - Each wakeup is summarised as [`RefreshFlags`] — separate `status` and
//!   `stash` bits, so we only spawn `git stash list` when a stash ref
//!   actually changed (the common case after a normal edit is `status`
//!   only).
//! - The per-repo refresher task drains the queue, ORs the flags
//!   together, and runs `git_inspect::repo_status` and/or
//!   `git_write::stash_list` to broadcast through
//!   [`crate::server::StateEvent`].
//! - The refresher *parks* whenever `client_count` drops to 0: with no UI
//!   listening, the broadcasts would just be dropped on the floor, so
//!   spending git subprocesses on them is pure waste. Events that arrive
//!   while parked are dropped (the first client to reconnect triggers a
//!   one-shot catch-up refresh).
//! - Repo lifecycle is driven by `StateEvent::Repos` broadcasts: when a
//!   repo is added, a watcher is spawned; when one is removed, the
//!   corresponding handle is dropped, which stops the debouncer and (via
//!   the tx → rx drop chain) terminates the refresher task.
//!
//! Out of scope: live `Commits` refresh (history view stays user-driven),
//! file-diff invalidation for open Monaco tabs (snapshot semantics by
//! design).

use crate::server::StateEvent;
use crate::session::{SessionEvent, SessionRegistry};
use crate::state::AppState;
use crate::{git_inspect, git_write};
use notify::RecursiveMode;
use notify_debouncer_full::new_debouncer;
use protocol::RepoEntry;
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tracing::{debug, info, warn};

/// Debounce window for the per-repo watcher. Wide enough to coalesce a
/// `git commit` burst (several `.git/` writes spread over a few hundred
/// milliseconds) into a single refresh, small enough that interactive
/// feel still feels "live" when the user edits a file in their IDE.
const DEBOUNCE_MS: u64 = 750;

/// Directory names (relative to the repo root) whose contents are never
/// relevant to `git status` and whose write traffic dwarfs everything
/// else. Hard-coded rather than parsed from `.gitignore` because (a)
/// parsing per-event is too expensive and (b) the well-known offenders
/// are pretty stable across the ecosystem.
const EXCLUDED_DIRS: &[&str] = &[
    "target",
    "node_modules",
    "dist",
    "build",
    "out",
    ".next",
    ".nuxt",
    ".turbo",
    ".svelte-kit",
    ".angular",
    ".cache",
    ".parcel-cache",
    ".venv",
    "venv",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".gradle",
    ".tmp",
];

/// Files directly under `.git/` whose modification can change `git
/// status` output (current branch, in-progress operation markers, packed
/// refs).
const STATUS_FILES: &[&str] = &[
    "index",
    "HEAD",
    "packed-refs",
    "MERGE_HEAD",
    "ORIG_HEAD",
    "CHERRY_PICK_HEAD",
    "REBASE_HEAD",
    "BISECT_LOG",
];

/// Which refreshes a single batch of file events triggered. `status` and
/// `stash` are independent so the common case (a worktree edit) only
/// pays for one git subprocess instead of two.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
struct RefreshFlags {
    status: bool,
    stash: bool,
}

impl RefreshFlags {
    const STATUS: Self = Self {
        status: true,
        stash: false,
    };
    const BOTH: Self = Self {
        status: true,
        stash: true,
    };

    fn merge(self, other: Self) -> Self {
        Self {
            status: self.status || other.status,
            stash: self.stash || other.stash,
        }
    }

    fn is_empty(self) -> bool {
        !self.status && !self.stash
    }
}

/// Classify a single fs-event path against a repo root. Returns `None`
/// for paths we should ignore (`.git/objects/`, build dirs, lock files,
/// etc.); returns a [`RefreshFlags`] indicating which refresh(es) the
/// event implies. Paths outside `repo_root` also return `None` (notify
/// shouldn't deliver them, but we don't trust the boundary).
fn classify_event(repo_root: &Path, event_path: &Path) -> Option<RefreshFlags> {
    let rel = event_path.strip_prefix(repo_root).ok()?;
    let comps: Vec<&str> = rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect();
    let first = *comps.first()?;

    if first == ".git" {
        return classify_git_internal(&comps[1..]);
    }
    if EXCLUDED_DIRS.contains(&first) {
        return None;
    }
    Some(RefreshFlags::STATUS)
}

/// Classify a path inside `.git/` (with the leading `.git` component
/// already stripped). The bulk of `.git/` traffic — `objects/`,
/// `logs/HEAD`, `FETCH_HEAD`, lock files, `COMMIT_EDITMSG` — is silent
/// noise for our purposes; only refs, index, and a handful of operation
/// markers actually matter.
fn classify_git_internal(tail: &[&str]) -> Option<RefreshFlags> {
    match tail {
        // Stash refs are interesting on their own AND imply status (a
        // stash push touches the index + worktree).
        ["refs", "stash"] | ["logs", "refs", "stash"] => Some(RefreshFlags::BOTH),
        // Single file directly under `.git/`.
        [name] if STATUS_FILES.contains(name) => Some(RefreshFlags::STATUS),
        // Any other ref change (branches, tags, remotes) can shift what
        // `git status` reports as "ahead/behind", so refresh status.
        ["refs", ..] => Some(RefreshFlags::STATUS),
        // Everything else under .git/ — objects/, logs/ (non-stash),
        // lock files, FETCH_HEAD, COMMIT_EDITMSG, info/, hooks/, etc.
        // — is ignored.
        _ => None,
    }
}

/// Owns a per-repo watcher pair. Dropping this stops the OS-level
/// watcher (via the debouncer's Drop impl) and indirectly terminates the
/// refresher task: the closure inside the debouncer holds the unique
/// `UnboundedSender<RefreshFlags>`, so dropping the debouncer drops the
/// sender, which makes the refresher's `recv()` return `None` and the
/// task exits.
///
/// Two flavors exist:
/// - `Active` — full recursive watcher + refresher task. Used once the
///   repo path has a `.git` entry.
/// - `Init` — cheap non-recursive watcher on the repo root that only
///   listens for `.git` creation. When `.git` shows up (e.g. the user
///   runs `git init` inside the folder we previously registered as a
///   plain directory), the watcher promotes itself to `Active` via
///   `promote_tx`. There's no refresher task in this mode.
enum RepoWatcher {
    /// `Box<dyn Any + Send + Sync>` so we can store debouncers without
    /// naming their generic parameters in the field type. The concrete
    /// type is `Debouncer<RecommendedWatcher, RecommendedCache>` but the
    /// public signature changes between platforms and pinning it in a
    /// struct is noise.
    Active {
        _debouncer: Box<dyn std::any::Any + Send + Sync>,
        _refresher: tokio::task::JoinHandle<()>,
    },
    Init {
        _debouncer: Box<dyn std::any::Any + Send + Sync>,
    },
}

/// Public entry point. Spawns the supervisor task that owns the per-repo
/// watcher set; the returned handle is only used to keep the supervisor
/// alive for the daemon's lifetime.
#[expect(
    clippy::too_many_lines,
    reason = "Spawns four parallel supervisor tasks (initial seed, repo-event \
              sync, init-promotion, session-worktree sync). Extracting each into \
              its own helper would require threading the four channels + arcs \
              through five function signatures for no readability gain."
)]
pub fn start(
    state: &Arc<AppState>,
    sessions: &Arc<SessionRegistry>,
    state_events: &broadcast::Sender<StateEvent>,
    client_count: &watch::Receiver<usize>,
) {
    let handles = Arc::new(AsyncMutex::new(HashMap::<String, RepoWatcher>::new()));
    // Worktree watchers are keyed by the worktree's absolute path string
    // (unique across all sessions); the value carries the originating
    // repo_id so refresher tasks can stamp the StateEvent correctly.
    let worktree_handles = Arc::new(AsyncMutex::new(HashMap::<String, WorktreeWatcher>::new()));

    // `promote_tx` is held by every init-flavored watcher and used to ask
    // the supervisor to upgrade it once `.git` appears in the watched
    // folder. A separate task drains the channel so the upgrade isn't
    // gated behind `StateEvent` traffic.
    let (promote_tx, mut promote_rx) = mpsc::unbounded_channel::<String>();

    // Seed initial watchers from the current registry. Doing this here
    // (instead of waiting for the first broadcast) means a freshly
    // started daemon picks up file changes immediately.
    let initial = state.with_persisted(|s| s.repos.clone());
    let handles_for_seed = Arc::clone(&handles);
    let state_events_for_seed = state_events.clone();
    let client_count_for_seed = client_count.clone();
    let promote_tx_for_seed = promote_tx.clone();
    tokio::spawn(async move {
        let mut guard = handles_for_seed.lock().await;
        for repo in initial {
            spawn_watcher(
                &repo,
                &state_events_for_seed,
                &client_count_for_seed,
                &promote_tx_for_seed,
                &mut guard,
            );
        }
        drop(guard);
    });

    // Lifecycle supervisor: react to StateEvent::Repos broadcasts and
    // sync the handle set against the new registry.
    let mut rx = state_events.subscribe();
    let state_events_for_sync = state_events.clone();
    let handles_for_sync = Arc::clone(&handles);
    let client_count_for_sync = client_count.clone();
    let promote_tx_for_sync = promote_tx.clone();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(StateEvent::Repos(repos)) => {
                    let mut guard = handles_for_sync.lock().await;
                    sync_handles(
                        &repos,
                        &state_events_for_sync,
                        &client_count_for_sync,
                        &promote_tx_for_sync,
                        &mut guard,
                    );
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

    // Promotion supervisor: when an init watcher reports that `.git`
    // appeared, look the repo up in the registry, drop the init watcher,
    // and install the full one. If the repo is no longer registered (was
    // removed while we waited), just drop the request.
    let state_for_promote = Arc::clone(state);
    let state_events_for_promote = state_events.clone();
    let client_count_for_promote = client_count.clone();
    let promote_tx_for_promote = promote_tx.clone();
    tokio::spawn(async move {
        while let Some(repo_id) = promote_rx.recv().await {
            // Acquire the handle lock BEFORE re-reading the registry, so
            // the read can't race with a `sync_handles` run that removes
            // this repo's handle. (Both code paths take the same lock.)
            let mut guard = handles.lock().await;
            let repo = state_for_promote
                .with_persisted(|s| s.repos.iter().find(|r| r.id == repo_id).cloned());
            let Some(repo) = repo else { continue };
            // Drop the init watcher first so the OS handle is released
            // before we attempt to install the recursive one. If the
            // handle is already Active (a duplicate promote event raced
            // past the first one), the existence check at the top of
            // `spawn_watcher` would still re-install correctly.
            if let Some(prev) = guard.remove(&repo_id) {
                drop(prev);
            }
            spawn_watcher(
                &repo,
                &state_events_for_promote,
                &client_count_for_promote,
                &promote_tx_for_promote,
                &mut guard,
            );
            info!(repo_id, "git_watch: promoted init watcher to active");
        }
    });

    // Session-worktree supervisor: every active session's member worktrees
    // gets its own filesystem watcher so source-control updates fire live
    // for the on-disk tree the agent is editing — not just the registered
    // repo's main tree. Seeds from the current snapshot, then syncs on
    // every SessionEvent (Updated / Removed).
    let sessions_for_wt = Arc::clone(sessions);
    let state_events_for_wt = state_events.clone();
    let client_count_for_wt = client_count.clone();
    let worktree_handles_for_wt = Arc::clone(&worktree_handles);
    tokio::spawn(async move {
        // Initial seed
        {
            let targets = collect_session_worktree_targets(&sessions_for_wt);
            let mut guard = worktree_handles_for_wt.lock().await;
            sync_worktree_handles(
                &targets,
                &state_events_for_wt,
                &client_count_for_wt,
                &mut guard,
            );
        }
        let mut session_rx = sessions_for_wt.subscribe();
        loop {
            match session_rx.recv().await {
                Ok(SessionEvent::Updated(_) | SessionEvent::Removed(_)) => {
                    let targets = collect_session_worktree_targets(&sessions_for_wt);
                    let mut guard = worktree_handles_for_wt.lock().await;
                    sync_worktree_handles(
                        &targets,
                        &state_events_for_wt,
                        &client_count_for_wt,
                        &mut guard,
                    );
                }
                Ok(SessionEvent::Attention { .. }) => {
                    // Attention events don't change the set of worktrees.
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(lagged = n, "git_watch: session event stream lagged");
                    // Re-sync to be safe.
                    let targets = collect_session_worktree_targets(&sessions_for_wt);
                    let mut guard = worktree_handles_for_wt.lock().await;
                    sync_worktree_handles(
                        &targets,
                        &state_events_for_wt,
                        &client_count_for_wt,
                        &mut guard,
                    );
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
        info!("git_watch: session-worktree supervisor exiting (channel closed)");
    });
}

/// Flatten every live session's member list into `(repo_id, worktree_path)`
/// pairs. Stopped or errored sessions are excluded — their worktrees may
/// have been removed by the cleanup pipeline, and watching a missing dir
/// just churns log spam.
fn collect_session_worktree_targets(sessions: &SessionRegistry) -> Vec<(String, String)> {
    use protocol::SessionStatus;
    let mut out: Vec<(String, String)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for snap in sessions.snapshots() {
        if matches!(snap.status, SessionStatus::Stopped | SessionStatus::Error) {
            continue;
        }
        for member in &snap.members {
            if seen.insert(member.worktree_path.clone()) {
                out.push((member.repo_id.clone(), member.worktree_path.clone()));
            }
        }
    }
    out
}

enum WorktreeWatcher {
    Active {
        _debouncer: Box<dyn std::any::Any + Send + Sync>,
        _refresher: tokio::task::JoinHandle<()>,
    },
}

/// Reconcile the worktree-watcher set against the freshly-derived list of
/// `(repo_id, worktree_path)` targets: spawn watchers for any new ones,
/// drop watchers for paths that no longer have a live session.
fn sync_worktree_handles(
    targets: &[(String, String)],
    state_events: &broadcast::Sender<StateEvent>,
    client_count: &watch::Receiver<usize>,
    handles: &mut HashMap<String, WorktreeWatcher>,
) {
    let current: HashSet<&str> = targets.iter().map(|(_, p)| p.as_str()).collect();
    handles.retain(|path, _| {
        let keep = current.contains(path.as_str());
        if !keep {
            debug!(
                worktree = path,
                "git_watch: stopping watcher for retired session worktree"
            );
        }
        keep
    });
    for (repo_id, worktree_path) in targets {
        if handles.contains_key(worktree_path) {
            continue;
        }
        spawn_worktree_watcher(repo_id, worktree_path, state_events, client_count, handles);
    }
}

/// Create the debouncer + refresher pair for one session worktree. The
/// emitted [`StateEvent::RepoStatus`] / [`StateEvent::Stashes`] events
/// carry `worktree_path = Some(path)` so the source-control sidebar can
/// route them to the matching workspace section instead of conflating
/// them with the registered repo's main-tree status.
fn spawn_worktree_watcher(
    repo_id: &str,
    worktree_path: &str,
    state_events: &broadcast::Sender<StateEvent>,
    client_count: &watch::Receiver<usize>,
    handles: &mut HashMap<String, WorktreeWatcher>,
) {
    let wt_path = PathBuf::from(worktree_path);
    if !wt_path.is_dir() {
        warn!(
            repo_id,
            worktree = worktree_path,
            "git_watch: worktree path not a directory; skipping watcher"
        );
        return;
    }
    let (tx, rx) = mpsc::unbounded_channel::<RefreshFlags>();
    let callback_path = wt_path.clone();
    let callback_repo_id = repo_id.to_string();
    let debouncer_result = new_debouncer(
        Duration::from_millis(DEBOUNCE_MS),
        None,
        move |result: notify_debouncer_full::DebounceEventResult| match result {
            Ok(events) => {
                let mut merged = RefreshFlags::default();
                for ev in events {
                    for path in &ev.paths {
                        if let Some(flags) = classify_event(&callback_path, path) {
                            merged = merged.merge(flags);
                            if merged == RefreshFlags::BOTH {
                                break;
                            }
                        }
                    }
                    if merged == RefreshFlags::BOTH {
                        break;
                    }
                }
                if !merged.is_empty() {
                    let _ = tx.send(merged);
                }
            }
            Err(errs) => {
                for e in errs {
                    warn!(
                        repo_id = %callback_repo_id,
                        error = %e,
                        "git_watch: worktree debouncer error"
                    );
                }
            }
        },
    );
    let mut debouncer = match debouncer_result {
        Ok(d) => d,
        Err(err) => {
            warn!(
                repo_id,
                worktree = worktree_path,
                ?err,
                "git_watch: failed to create worktree debouncer"
            );
            return;
        }
    };
    if let Err(err) = debouncer.watch(&wt_path, RecursiveMode::Recursive) {
        warn!(
            repo_id,
            worktree = worktree_path,
            ?err,
            "git_watch: failed to start worktree watcher"
        );
        return;
    }
    let refresher = tokio::spawn(worktree_refresher_task(
        wt_path.clone(),
        repo_id.to_string(),
        worktree_path.to_string(),
        state_events.clone(),
        client_count.clone(),
        rx,
    ));
    info!(
        repo_id,
        worktree = worktree_path,
        "git_watch: watching session worktree"
    );
    handles.insert(
        worktree_path.to_string(),
        WorktreeWatcher::Active {
            _debouncer: Box::new(debouncer),
            _refresher: refresher,
        },
    );
}

/// Worktree analog of `refresher_task`. Same parking / coalescing logic;
/// runs `refresh_target` with the worktree path so the emitted events
/// carry `worktree_path = Some(...)`.
async fn worktree_refresher_task(
    target_path: PathBuf,
    repo_id: String,
    worktree_path: String,
    state_events: broadcast::Sender<StateEvent>,
    mut client_count: watch::Receiver<usize>,
    mut rx: mpsc::UnboundedReceiver<RefreshFlags>,
) {
    loop {
        let has_clients = *client_count.borrow_and_update() > 0;
        if has_clients {
            tokio::select! {
                res = client_count.changed() => {
                    if res.is_err() { break; }
                }
                maybe = rx.recv() => {
                    let Some(first) = maybe else { break };
                    let mut flags = first;
                    while let Ok(more) = rx.try_recv() {
                        flags = flags.merge(more);
                    }
                    refresh_target(&target_path, &repo_id, Some(&worktree_path), &state_events, flags).await;
                }
            }
        } else {
            tokio::select! {
                res = client_count.changed() => {
                    if res.is_err() { break; }
                    if *client_count.borrow() > 0 {
                        while rx.try_recv().is_ok() {}
                        refresh_target(&target_path, &repo_id, Some(&worktree_path), &state_events, RefreshFlags::BOTH).await;
                    }
                }
                maybe = rx.recv() => {
                    if maybe.is_none() { break; }
                    while rx.try_recv().is_ok() {}
                }
            }
        }
    }
    debug!(repo_id, worktree = %worktree_path, "git_watch: worktree refresher exiting");
}

/// Add watchers for newly-added repos and drop watchers for repos that
/// disappeared from the registry. Existing entries are left untouched —
/// re-creating a watcher on every broadcast would tear down and rebuild
/// the OS handle for no reason.
fn sync_handles(
    repos: &[RepoEntry],
    state_events: &broadcast::Sender<StateEvent>,
    client_count: &watch::Receiver<usize>,
    promote_tx: &mpsc::UnboundedSender<String>,
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
            spawn_watcher(repo, state_events, client_count, promote_tx, handles);
        }
    }
}

/// Create the debouncer + refresher task pair for one repo and store
/// them in the handle map. Silently logs and skips on failure (a missing
/// directory or an OS handle exhaustion shouldn't kill the supervisor).
fn spawn_watcher(
    repo: &RepoEntry,
    state_events: &broadcast::Sender<StateEvent>,
    client_count: &watch::Receiver<usize>,
    promote_tx: &mpsc::UnboundedSender<String>,
    handles: &mut HashMap<String, RepoWatcher>,
) {
    let repo_path = PathBuf::from(&repo.path);
    let repo_id = repo.id.clone();
    if !repo_path.is_dir() {
        warn!(repo_id, path = %repo.path, "git_watch: repo path not a directory; skipping watcher");
        return;
    }
    // Non-git folders get the cheap init-mode watcher instead of the full
    // recursive one. Once `.git` appears (e.g. the user runs `git init` in
    // the registered folder), the init watcher promotes itself to a full
    // recursive watcher via `promote_tx`.
    if !repo_path.join(".git").exists() {
        spawn_init_watcher(repo, promote_tx, handles);
        return;
    }

    // notify -> refresher trigger. UnboundedReceiver is fine: the
    // refresher always drains to empty before re-awaiting, so the queue
    // never grows past one logical "burst pending".
    let (tx, rx) = mpsc::unbounded_channel::<RefreshFlags>();

    let callback_repo_path = repo_path.clone();
    let callback_repo_id = repo_id.clone();
    let debouncer_result = new_debouncer(
        Duration::from_millis(DEBOUNCE_MS),
        None,
        move |result: notify_debouncer_full::DebounceEventResult| match result {
            Ok(events) => {
                let mut merged = RefreshFlags::default();
                for ev in events {
                    for path in &ev.paths {
                        if let Some(flags) = classify_event(&callback_repo_path, path) {
                            merged = merged.merge(flags);
                            if merged == RefreshFlags::BOTH {
                                break;
                            }
                        }
                    }
                    if merged == RefreshFlags::BOTH {
                        break;
                    }
                }
                if !merged.is_empty() {
                    // Channel closed = refresher gone = repo removed.
                    // Drop the event silently.
                    let _ = tx.send(merged);
                }
            }
            Err(errs) => {
                for e in errs {
                    warn!(
                        repo_id = %callback_repo_id,
                        error = %e,
                        "git_watch: debouncer error"
                    );
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

    let refresher = tokio::spawn(refresher_task(
        repo_path.clone(),
        repo_id.clone(),
        state_events.clone(),
        client_count.clone(),
        rx,
    ));

    info!(repo_id, path = %repo.path, "git_watch: watching repo");
    handles.insert(
        repo_id,
        RepoWatcher::Active {
            _debouncer: Box::new(debouncer),
            _refresher: refresher,
        },
    );
}

/// Install the lightweight non-recursive watcher used while a registered
/// folder doesn't yet have a `.git` entry. The watcher only listens for
/// the appearance of `<repo_path>/.git` and, when it sees it, sends the
/// `repo_id` to the promotion supervisor (see `start`), which swaps this
/// handle out for a regular `Active` watcher.
fn spawn_init_watcher(
    repo: &RepoEntry,
    promote_tx: &mpsc::UnboundedSender<String>,
    handles: &mut HashMap<String, RepoWatcher>,
) {
    let repo_path = PathBuf::from(&repo.path);
    let repo_id = repo.id.clone();
    let git_target = repo_path.join(".git");
    let callback_repo_id = repo_id.clone();
    let callback_promote_tx = promote_tx.clone();
    // Short debounce: the user is waiting for the sidebar to come alive
    // after `git init`, so coalescing a 750 ms burst here just adds dead
    // time. There's also very little burstiness on this code path — a
    // single `.git/` directory creation, then quiet.
    let debouncer_result = new_debouncer(
        Duration::from_millis(100),
        None,
        move |result: notify_debouncer_full::DebounceEventResult| {
            let Ok(events) = result else { return };
            let mut saw_git = false;
            for ev in events {
                if ev.paths.iter().any(|p| p == &git_target) {
                    saw_git = true;
                    break;
                }
            }
            // Confirm with a stat — notify can fire on `.git` removal or
            // on a temp file with a similar name, and we only want to
            // promote when the directory is really there now.
            if saw_git && git_target.exists() {
                let _ = callback_promote_tx.send(callback_repo_id.clone());
            }
        },
    );
    let mut debouncer = match debouncer_result {
        Ok(d) => d,
        Err(err) => {
            warn!(repo_id, ?err, "git_watch: failed to create init debouncer");
            return;
        }
    };
    if let Err(err) = debouncer.watch(&repo_path, RecursiveMode::NonRecursive) {
        warn!(repo_id, ?err, "git_watch: failed to start init watcher");
        return;
    }
    // Race guard: `.git` may have appeared between the caller's existence
    // check and the OS handle install. Trigger an immediate promote so we
    // don't sit on an init watcher pointing at an already-initialized repo.
    if repo_path.join(".git").exists() {
        let _ = promote_tx.send(repo_id.clone());
    }
    info!(
        repo_id,
        path = %repo.path,
        "git_watch: init watcher — waiting for .git to appear"
    );
    handles.insert(
        repo_id,
        RepoWatcher::Init {
            _debouncer: Box::new(debouncer),
        },
    );
}

/// Drain events and dispatch refreshes, but only while at least one WS
/// client is connected. While `client_count` is 0 we drop incoming
/// events on the floor — a fresh connection triggers a one-shot
/// catch-up refresh covering whatever happened during the idle window.
async fn refresher_task(
    repo_path: PathBuf,
    repo_id: String,
    state_events: broadcast::Sender<StateEvent>,
    mut client_count: watch::Receiver<usize>,
    mut rx: mpsc::UnboundedReceiver<RefreshFlags>,
) {
    loop {
        let has_clients = *client_count.borrow_and_update() > 0;

        if has_clients {
            tokio::select! {
                res = client_count.changed() => {
                    if res.is_err() {
                        break;
                    }
                    // Re-evaluate has_clients at the top of the loop.
                }
                maybe = rx.recv() => {
                    let Some(first) = maybe else { break };
                    let mut flags = first;
                    while let Ok(more) = rx.try_recv() {
                        flags = flags.merge(more);
                    }
                    refresh(&repo_path, &repo_id, &state_events, flags).await;
                }
            }
        } else {
            tokio::select! {
                res = client_count.changed() => {
                    if res.is_err() {
                        break;
                    }
                    if *client_count.borrow() > 0 {
                        // Catch-up: drain anything that came in while
                        // parked, then run a full refresh so the freshly
                        // reconnected UI sees current truth.
                        while rx.try_recv().is_ok() {}
                        refresh(&repo_path, &repo_id, &state_events, RefreshFlags::BOTH).await;
                    }
                }
                maybe = rx.recv() => {
                    if maybe.is_none() {
                        break;
                    }
                    // Parked: drain any remaining backlog so the
                    // mpsc queue can't grow unbounded.
                    while rx.try_recv().is_ok() {}
                }
            }
        }
    }
    debug!(repo_id = %repo_id, "git_watch: refresher task exiting");
}

async fn refresh(
    repo_path: &Path,
    repo_id: &str,
    state_events: &broadcast::Sender<StateEvent>,
    flags: RefreshFlags,
) {
    refresh_target(repo_path, repo_id, None, state_events, flags).await;
}

/// Refresh logic shared by the registered-repo watcher and the per-session
/// worktree watcher. `worktree_path = None` is the main tree of the
/// registered repo; `Some(path)` is the session worktree the watcher is
/// tracking. The emitted events carry the same `worktree_path` so the
/// sidebar can route them to the correct workspace section.
async fn refresh_target(
    target_path: &Path,
    repo_id: &str,
    worktree_path: Option<&str>,
    state_events: &broadcast::Sender<StateEvent>,
    flags: RefreshFlags,
) {
    if flags.status {
        match git_inspect::repo_status(target_path).await {
            Ok((index_changes, worktree_changes)) => {
                let _ = state_events.send(StateEvent::RepoStatus {
                    repo_id: repo_id.to_string(),
                    worktree_path: worktree_path.map(str::to_owned),
                    index_changes,
                    worktree_changes,
                });
            }
            Err(err) => {
                warn!(repo_id, ?err, "git_watch: repo_status failed");
            }
        }
    }
    if flags.stash {
        match git_write::stash_list(target_path).await {
            Ok(stashes) => {
                let _ = state_events.send(StateEvent::Stashes {
                    repo_id: repo_id.to_string(),
                    worktree_path: worktree_path.map(str::to_owned),
                    stashes,
                });
            }
            Err(err) => {
                warn!(repo_id, ?err, "git_watch: stash_list failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(r"C:\repo")
        } else {
            PathBuf::from("/repo")
        }
    }

    fn p(parts: &[&str]) -> PathBuf {
        let mut path = repo();
        for part in parts {
            path.push(part);
        }
        path
    }

    #[test]
    fn worktree_edit_triggers_status_only() {
        assert_eq!(
            classify_event(&repo(), &p(&["src", "main.rs"])),
            Some(RefreshFlags::STATUS)
        );
    }

    #[test]
    fn index_write_triggers_status() {
        assert_eq!(
            classify_event(&repo(), &p(&[".git", "index"])),
            Some(RefreshFlags::STATUS)
        );
    }

    #[test]
    fn head_write_triggers_status() {
        assert_eq!(
            classify_event(&repo(), &p(&[".git", "HEAD"])),
            Some(RefreshFlags::STATUS)
        );
    }

    #[test]
    fn ref_write_triggers_status() {
        assert_eq!(
            classify_event(&repo(), &p(&[".git", "refs", "heads", "feature"])),
            Some(RefreshFlags::STATUS)
        );
    }

    #[test]
    fn stash_ref_triggers_both() {
        assert_eq!(
            classify_event(&repo(), &p(&[".git", "refs", "stash"])),
            Some(RefreshFlags::BOTH)
        );
    }

    #[test]
    fn stash_reflog_triggers_both() {
        assert_eq!(
            classify_event(&repo(), &p(&[".git", "logs", "refs", "stash"])),
            Some(RefreshFlags::BOTH)
        );
    }

    #[test]
    fn objects_writes_are_ignored() {
        assert_eq!(
            classify_event(&repo(), &p(&[".git", "objects", "ab", "cdef0123456789"])),
            None
        );
        assert_eq!(
            classify_event(&repo(), &p(&[".git", "objects", "pack", "pack-abc.idx"])),
            None
        );
    }

    #[test]
    fn head_reflog_is_ignored() {
        // logs/HEAD fires on every commit but doesn't change status
        // output beyond what `.git/HEAD` + the new commit ref already
        // covered.
        assert_eq!(classify_event(&repo(), &p(&[".git", "logs", "HEAD"])), None);
    }

    #[test]
    fn fetch_head_is_ignored() {
        assert_eq!(classify_event(&repo(), &p(&[".git", "FETCH_HEAD"])), None);
    }

    #[test]
    fn lock_files_are_ignored() {
        assert_eq!(classify_event(&repo(), &p(&[".git", "index.lock"])), None);
        assert_eq!(classify_event(&repo(), &p(&[".git", "HEAD.lock"])), None);
    }

    #[test]
    fn build_dir_writes_are_ignored() {
        for dir in ["target", "node_modules", "dist", ".next", "__pycache__"] {
            assert_eq!(
                classify_event(&repo(), &p(&[dir, "foo.o"])),
                None,
                "expected {dir}/foo.o to be ignored"
            );
        }
    }

    #[test]
    fn path_outside_repo_returns_none() {
        let outside = if cfg!(windows) {
            PathBuf::from(r"C:\other\file.rs")
        } else {
            PathBuf::from("/other/file.rs")
        };
        assert_eq!(classify_event(&repo(), &outside), None);
    }

    #[test]
    fn merge_flags_is_associative() {
        let s = RefreshFlags::STATUS;
        let b = RefreshFlags::BOTH;
        let n = RefreshFlags::default();
        assert_eq!(s.merge(s), s);
        assert_eq!(s.merge(b), b);
        assert_eq!(n.merge(s), s);
        assert_eq!(n.merge(n), n);
    }
}
