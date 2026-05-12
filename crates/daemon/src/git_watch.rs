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
use crate::state::AppState;
use crate::{git_inspect, git_write};
use notify::RecursiveMode;
use notify_debouncer_full::new_debouncer;
use protocol::RepoEntry;
use std::collections::HashMap;
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
struct RepoWatcher {
    /// `Box<dyn Any + Send + Sync>` so we can store debouncers without
    /// naming their generic parameters in the field type. The concrete
    /// type is `Debouncer<RecommendedWatcher, RecommendedCache>` but the
    /// public signature changes between platforms and pinning it in a
    /// struct is noise.
    _debouncer: Box<dyn std::any::Any + Send + Sync>,
    _refresher: tokio::task::JoinHandle<()>,
}

/// Public entry point. Spawns the supervisor task that owns the per-repo
/// watcher set; the returned handle is only used to keep the supervisor
/// alive for the daemon's lifetime.
pub fn start(
    state: &Arc<AppState>,
    state_events: &broadcast::Sender<StateEvent>,
    client_count: watch::Receiver<usize>,
) {
    let handles = Arc::new(AsyncMutex::new(HashMap::<String, RepoWatcher>::new()));

    // Seed initial watchers from the current registry. Doing this here
    // (instead of waiting for the first broadcast) means a freshly
    // started daemon picks up file changes immediately.
    let initial = state.with_persisted(|s| s.repos.clone());
    let handles_for_seed = Arc::clone(&handles);
    let state_events_for_seed = state_events.clone();
    let client_count_for_seed = client_count.clone();
    tokio::spawn(async move {
        let mut guard = handles_for_seed.lock().await;
        for repo in initial {
            spawn_watcher(
                &repo,
                &state_events_for_seed,
                &client_count_for_seed,
                &mut guard,
            );
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
                    sync_handles(&repos, &state_events_for_sync, &client_count, &mut guard);
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
    client_count: &watch::Receiver<usize>,
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
            spawn_watcher(repo, state_events, client_count, handles);
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
        RepoWatcher {
            _debouncer: Box::new(debouncer),
            _refresher: refresher,
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
    if flags.status {
        match git_inspect::repo_status(repo_path).await {
            Ok((index_changes, worktree_changes)) => {
                let _ = state_events.send(StateEvent::RepoStatus {
                    repo_id: repo_id.to_string(),
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
        match git_write::stash_list(repo_path).await {
            Ok(stashes) => {
                let _ = state_events.send(StateEvent::Stashes {
                    repo_id: repo_id.to_string(),
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
            classify_event(
                &repo(),
                &p(&[".git", "objects", "ab", "cdef0123456789"])
            ),
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
        assert_eq!(
            classify_event(&repo(), &p(&[".git", "logs", "HEAD"])),
            None
        );
    }

    #[test]
    fn fetch_head_is_ignored() {
        assert_eq!(
            classify_event(&repo(), &p(&[".git", "FETCH_HEAD"])),
            None
        );
    }

    #[test]
    fn lock_files_are_ignored() {
        assert_eq!(
            classify_event(&repo(), &p(&[".git", "index.lock"])),
            None
        );
        assert_eq!(
            classify_event(&repo(), &p(&[".git", "HEAD.lock"])),
            None
        );
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
