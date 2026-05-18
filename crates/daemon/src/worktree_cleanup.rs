//! Robust per-worktree teardown shared by `discard_session` and
//! `worktrees_admin::delete_group`.
//!
//! The previous discard path was a single `git worktree remove --force`
//! that logged a warning and gave up on failure. That left two leaks:
//!   1. When git remove failed (Windows file locks from child build
//!      processes, antivirus scans, etc.) the worktree contents stayed
//!      on disk forever — sometimes 10+ GB of `node_modules` / `target`.
//!   2. Even on success, the `wt.<branch-slug>/` wrapper dir was never
//!      removed, so a fresh worktrees root accumulated empty parent
//!      dirs over time.
//!
//! The robust helper here tries git remove, retries once after a brief
//! delay (covers transient locks like Defender mid-scan), falls back to
//! a recursive filesystem delete, then cleans up the wrapper dir and
//! runs `git worktree prune` so git's own admin state catches up.

use std::path::Path;
use std::time::Duration;

use crate::git;

/// Outcome of attempting to remove a single member worktree.
#[derive(Debug)]
pub enum CleanupOutcome {
    /// Member dir is gone from disk (git remove, retried git remove, or
    /// fs fallback succeeded).
    Removed,
    /// All attempts failed and the member dir still exists. The reason
    /// is the final error chain joined for surfacing to the user.
    StillOnDisk { reason: String },
}

/// Try every available cleanup mechanism for one member worktree.
///
/// Sequence:
///   1. `git -C <repo> worktree remove --force <member>`
///   2. Sleep 250 ms, then retry (1) — catches transient Windows locks.
///   3. `std::fs::remove_dir_all(<member>)` — last-resort, bypasses git.
///   4. After member removal, `git -C <repo> worktree prune` so the
///      originating repo's `.git/worktrees/<name>` admin dir is dropped.
///
/// Returns `Removed` if the member is gone after the sequence, even if
/// git itself never succeeded (the fs fallback may have done the work).
/// `StillOnDisk` only on the truly stuck case (active file locks the
/// caller will need to surface to the user).
pub async fn remove_member(repo: &Path, member: &Path) -> CleanupOutcome {
    let mut last_err: Option<String> = None;

    // Attempt 1: git remove.
    match git::worktree_remove(repo, member).await {
        Ok(()) => {}
        Err(err) => {
            last_err = Some(format!("git worktree remove: {err}"));
            tokio::time::sleep(Duration::from_millis(250)).await;
            // Attempt 2: retry git remove after a brief delay.
            if let Err(err) = git::worktree_remove(repo, member).await {
                last_err = Some(format!("git worktree remove (retry): {err}"));
            }
        }
    }

    if !member.exists() {
        prune_repo(repo).await;
        return CleanupOutcome::Removed;
    }

    // Attempt 3: filesystem fallback. git may have updated its admin
    // state and left the heavy contents behind.
    match std::fs::remove_dir_all(member) {
        Ok(()) => {
            prune_repo(repo).await;
            CleanupOutcome::Removed
        }
        Err(err) => {
            // NotFound here means a parallel actor already removed the
            // dir (manual rm, another tab's cleanup). Treat as success.
            if err.kind() == std::io::ErrorKind::NotFound {
                prune_repo(repo).await;
                return CleanupOutcome::Removed;
            }
            let reason = match last_err {
                Some(prev) => format!("{prev}; fs::remove_dir_all: {err}"),
                None => format!("fs::remove_dir_all: {err}"),
            };
            CleanupOutcome::StillOnDisk { reason }
        }
    }
}

/// Remove the `wt.<branch-slug>/` wrapper dir if it's empty. Run after
/// every member of a group has been cleaned up — even on full success
/// the wrapper is otherwise leaked, and a long-lived worktrees root
/// accumulates dozens of empty `wt.*` directories over time.
///
/// If the wrapper isn't empty (a member's cleanup failed and left
/// contents behind), this is a no-op so the next admin scan can still
/// see what's there.
pub fn finalize_wrapper(wrapper: &Path) {
    match std::fs::remove_dir(wrapper) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            tracing::debug!(
                ?err,
                wrapper = %wrapper.display(),
                "worktree_cleanup: wrapper dir not empty or removal failed; \
                 leaving for admin scan",
            );
        }
    }
}

/// Best-effort `git worktree prune` to drop the originating repo's
/// `.git/worktrees/<name>/` admin dir for a deleted member. Failures
/// are logged but don't change the cleanup outcome — the user's
/// worktrees directory is gone either way.
async fn prune_repo(repo: &Path) {
    if let Err(err) = git::worktree_prune(repo).await {
        tracing::warn!(
            ?err,
            repo = %repo.display(),
            "worktree_cleanup: git worktree prune failed",
        );
    }
}

