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
//! a recursive filesystem delete, then prunes the now-empty ancestor
//! dirs (anchor skeleton + `wt.<slug>` wrapper) and runs
//! `git worktree prune` so git's own admin state catches up.

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

/// Prune the now-empty ancestor directories a removed member leaves
/// behind. Run after every member of a group has been cleaned up.
///
/// A member lives at `<root>/wt.<slug>/<sanitized-anchor>/<member>`, so
/// removing it strands the anchor skeleton (`X/dev`, `X`) and the
/// `wt.<slug>` wrapper itself — a long-lived worktrees root otherwise
/// accumulates dozens of empty `wt.*` trees over time. Passing the
/// member's immediate parent (the anchor leaf) as `start`, this walks up
/// removing each directory until it removes the `wt.<slug>` wrapper.
///
/// Uses the non-recursive [`std::fs::remove_dir`], which refuses a
/// non-empty directory — so a sibling member whose cleanup failed (or an
/// old-layout anchor still shared by another group) stops the walk with
/// its contents intact, left for the management modal's scan. The walk
/// stops at — and never removes — the worktrees `root`. A `NotFound` at
/// any level (git already pruned that dir, a parallel actor removed it)
/// is not a stop: the walk continues upward to clear the rest of the
/// skeleton.
pub fn prune_empty_ancestors(start: &Path, root: &Path) {
    let mut cur = start;
    while cur != root && cur.starts_with(root) {
        match std::fs::remove_dir(cur) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                tracing::debug!(
                    ?err,
                    dir = %cur.display(),
                    "worktree_cleanup: ancestor dir not empty or removal failed; \
                     leaving remainder for admin scan",
                );
                break;
            }
        }
        let Some(parent) = cur.parent() else { break };
        cur = parent;
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

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "tests assert preconditions with unwrap"
)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use uuid::Uuid;

    /// Lightweight `tempdir` stand-in — same pattern as
    /// `worktrees_admin::tests` to avoid adding a dep just for tests.
    struct Scratch {
        path: PathBuf,
    }

    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("rt-worktree-cleanup-{}", Uuid::new_v4().simple()));
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn prune_clears_anchor_skeleton_and_wrapper_up_to_root() {
        // New layout: <root>/wt.<slug>/X/dev/<member>. After the member
        // is gone, its parent chain (X/dev, X) plus the wt.<slug> wrapper
        // must all be removed, stopping at — and keeping — the root.
        let scratch = Scratch::new();
        let root = scratch.path();
        let wrapper = root.join("wt.feature-foo");
        let anchor_leaf = wrapper.join("X").join("dev");
        std::fs::create_dir_all(&anchor_leaf).unwrap();

        prune_empty_ancestors(&anchor_leaf, root);

        assert!(!wrapper.exists(), "wt.<slug> wrapper should be removed");
        assert!(root.exists(), "worktrees root must never be removed");
    }

    #[test]
    fn prune_stops_at_non_empty_sibling_member() {
        // Two members share the anchor X/dev. One was removed (its dir is
        // gone); the other is still on disk. Pruning from the removed
        // member's parent must stop at X/dev because the surviving member
        // keeps it non-empty — leaving the whole group for the admin scan.
        let scratch = Scratch::new();
        let root = scratch.path();
        let wrapper = root.join("wt.main");
        let anchor_leaf = wrapper.join("X").join("dev");
        let surviving_member = anchor_leaf.join("yaat-server");
        std::fs::create_dir_all(&surviving_member).unwrap();
        std::fs::write(surviving_member.join("file.txt"), b"content").unwrap();

        prune_empty_ancestors(&anchor_leaf, root);

        assert!(
            surviving_member.exists(),
            "surviving member must be untouched"
        );
        assert!(anchor_leaf.exists(), "non-empty anchor leaf must remain");
        assert!(wrapper.exists(), "wrapper above non-empty content must remain");
    }

    #[test]
    fn prune_continues_past_already_removed_start() {
        // git worktree remove may have already deleted the anchor leaf.
        // A NotFound at `start` must not stop the walk — the wrapper and
        // any intermediate dirs still need clearing.
        let scratch = Scratch::new();
        let root = scratch.path();
        let wrapper = root.join("wt.feature-foo");
        let intermediate = wrapper.join("X");
        std::fs::create_dir_all(&intermediate).unwrap();
        let already_gone = intermediate.join("dev");

        prune_empty_ancestors(&already_gone, root);

        assert!(!wrapper.exists(), "wrapper should be removed despite missing leaf");
        assert!(root.exists(), "worktrees root must never be removed");
    }

    #[test]
    fn prune_never_walks_above_root() {
        // A start path that is not under the root is a no-op (defensive:
        // never delete arbitrary dirs if a bad path arrives).
        let scratch = Scratch::new();
        let root = scratch.path().join("worktrees");
        std::fs::create_dir_all(&root).unwrap();
        let outside = scratch.path().join("elsewhere");
        std::fs::create_dir_all(&outside).unwrap();

        prune_empty_ancestors(&outside, &root);

        assert!(outside.exists(), "dir outside root must be untouched");
    }
}
