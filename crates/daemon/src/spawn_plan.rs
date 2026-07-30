//! Shared spawn resolution: which ref a session's branch forks from, and
//! whether the spawn would land on a worktree that already exists.
//!
//! Both the spawn path and the preview path resolve through here, so the
//! dialog can never promise a fork point the spawn won't honor.

use crate::git;
use crate::registry::ensure_default_branch;
use crate::state::AppState;
use protocol::RepoEntry;
use std::path::Path;

/// Resolve the ref a *new* branch should be created from.
///
/// Priority: explicit caller value → the repo's persisted default → lazy
/// re-detection (covers entries registered before detection succeeded) → the
/// repo's current branch → `main` as a last resort.
///
/// An explicit caller value is honored verbatim — a user who types `main`
/// may well mean the local ref, and quietly redirecting them is its own
/// surprise. Everything else is upgraded to its remote-tracking counterpart,
/// because an auto-detected local default is exactly the ref that goes
/// stale: in a repo driven through worktrees the primary tree's `main` never
/// advances, so "base off the default branch" would otherwise mean "base off
/// wherever main sat the last time someone checked it out by hand".
pub async fn resolve_base_for_create(
    state: &AppState,
    repo: &RepoEntry,
    explicit_base: Option<&str>,
) -> String {
    if let Some(base) = explicit_base {
        return base.to_string();
    }
    let repo_path = Path::new(&repo.path);
    let local = if let Some(branch) = repo.default_branch.clone() {
        branch
    } else {
        let detected = git::default_branch(repo_path).await;
        ensure_default_branch(state, &repo.id, detected.clone());
        match detected {
            Some(branch) => branch,
            None => git::current_branch(repo_path)
                .await
                .ok()
                .flatten()
                .unwrap_or_else(|| "main".to_string()),
        }
    };
    git::resolve_base_ref(repo_path, &local).await
}

/// Git facts about where a spawn would fork from.
///
/// Computed for the preview and logged on the spawn itself. A stale fork
/// point is invisible until it bites — a branch quietly cut from a months-old
/// `main` looks identical to a fresh one until something that landed in
/// between turns up missing.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ForkPoint {
    /// Remote-tracking counterpart of the base, when one exists.
    pub base_remote_ref: Option<String>,
    /// Commits the base trails its remote counterpart. `Some(0)` means level;
    /// `None` means there was nothing to compare against.
    pub base_behind_remote: Option<u32>,
    pub existing_worktree_head: Option<String>,
    pub existing_worktree_dirty: bool,
    /// Commits the existing worktree's HEAD trails the resolved base.
    pub existing_worktree_behind_base: Option<u32>,
}

/// Measure `base` against its remote, and any pre-existing worktree against
/// `resolved_base`.
///
/// Every field degrades to `None`/`false` rather than erroring: this feeds an
/// advisory display, and a repo that has never been fetched (or has no
/// remote) must still preview cleanly.
pub async fn fork_point(
    repo_path: &Path,
    base: Option<&str>,
    resolved_base: Option<&str>,
    existing_worktree: Option<&Path>,
) -> ForkPoint {
    let mut out = ForkPoint::default();

    if let Some(base) = base {
        out.base_remote_ref = git::remote_tracking_for(repo_path, base).await;
        // Skip the comparison when the base already *is* the remote ref —
        // "origin/main is 0 behind origin/main" is noise, not information.
        if let Some(remote) = &out.base_remote_ref
            && remote != base
        {
            out.base_behind_remote = git::ahead_behind(repo_path, base, remote)
                .await
                .map(|(_, behind)| behind);
        }
    }

    if let Some(worktree) = existing_worktree {
        out.existing_worktree_head = git::head_short_sha(worktree).await;
        out.existing_worktree_dirty = git::changed_files(worktree)
            .await
            .is_ok_and(|files| !files.is_empty());
        if let Some(base) = resolved_base {
            out.existing_worktree_behind_base = git::ahead_behind(worktree, "HEAD", base)
                .await
                .map(|(_, behind)| behind);
        }
    }

    out
}
