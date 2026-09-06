//! Shared spawn resolution: which ref a session's branch forks from, and
//! whether the spawn would land on a worktree that already exists.
//!
//! Both the spawn path and the preview path resolve through here, so the
//! dialog can never promise a fork point the spawn won't honor.

use crate::git;
use crate::registry::ensure_default_branch;
use crate::state::AppState;
use protocol::RepoEntry;
use std::fmt::Write as _;
use std::path::Path;

/// Structured spawn-time failure carried out of the spawn pipeline so the
/// dispatcher can render it as a blocking `ActionFailed` modal with an
/// actionable hint, instead of a bare error toast that drops the git reason.
#[derive(Debug, thiserror::Error)]
#[error("{title}: {detail}")]
pub struct SpawnFailure {
    pub title: String,
    pub detail: String,
    pub hint: Option<String>,
}

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
    let local = local_base_name(state, repo, explicit_base).await;
    if explicit_base.is_some() {
        return local;
    }
    git::resolve_base_ref(Path::new(&repo.path), &local).await
}

/// The base branch name before any remote-tracking upgrade: the explicit
/// caller value, or the same detection chain [`resolve_base_for_create`]
/// walks (persisted default → lazy re-detection → current branch → `main`).
///
/// Callers that need both the local name and its remote counterpart start
/// here; [`resolve_base_for_create`] is the "just give me the fork point"
/// wrapper on top.
pub async fn local_base_name(
    state: &AppState,
    repo: &RepoEntry,
    explicit_base: Option<&str>,
) -> String {
    if let Some(base) = explicit_base {
        return base.to_string();
    }
    let repo_path = Path::new(&repo.path);
    if let Some(branch) = repo.default_branch.clone() {
        return branch;
    }
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
    /// Abbreviated tip of a pre-existing branch that has no worktree — the
    /// leftover a discarded session leaves behind. A plain `worktree add`
    /// would attach the branch at this tip, base branch ignored.
    pub existing_branch_head: Option<String>,
    /// Commits that branch-only leftover trails the resolved base.
    pub existing_branch_behind_base: Option<u32>,
}

/// Measure `base` against its remote, any pre-existing worktree against
/// `resolved_base`, and — when there is no worktree — a pre-existing branch
/// of the same name against `resolved_base`.
///
/// Every field degrades to `None`/`false` rather than erroring: this feeds an
/// advisory display, and a repo that has never been fetched (or has no
/// remote) must still preview cleanly.
pub async fn fork_point(
    repo_path: &Path,
    base: Option<&str>,
    resolved_base: Option<&str>,
    existing_worktree: Option<&Path>,
    existing_branch: Option<&str>,
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
    } else if let Some(branch) = existing_branch {
        // The worktree-exists case already measures the branch through the
        // worktree's HEAD; this arm covers the branch-only leftover.
        out.existing_branch_head = git::ref_short_sha(repo_path, branch).await;
        if let Some(base) = resolved_base {
            out.existing_branch_behind_base = git::ahead_behind(repo_path, branch, base)
                .await
                .map(|(_, behind)| behind);
        }
    }

    out
}

/// The failure a [`protocol::WorktreeReusePolicy::RefuseLeftover`] spawn
/// raises when the name it was handed is already on disk.
///
/// The caller never showed the user a collision panel — the name was picked
/// automatically — so the message has to carry everything that panel would
/// have: which branch, where it lives, what it points at, and how far behind
/// the base it is.
pub fn leftover_refusal(
    repo_name: &str,
    branch: &str,
    base: &str,
    fork: &ForkPoint,
    worktree_path: &Path,
) -> SpawnFailure {
    let mut detail = if let Some(head) = &fork.existing_branch_head {
        let mut line = format!("Branch '{branch}' already exists in {repo_name} at {head}");
        append_behind(&mut line, fork.existing_branch_behind_base, base);
        line.push_str(", with no worktree attached. ");
        line
    } else {
        let mut line = format!(
            "A worktree for branch '{branch}' already exists in {repo_name} at {}",
            worktree_path.display()
        );
        if let Some(head) = &fork.existing_worktree_head {
            let _ = write!(line, " (HEAD {head}");
            append_behind(&mut line, fork.existing_worktree_behind_base, base);
            if fork.existing_worktree_dirty {
                line.push_str(", with uncommitted changes");
            }
            line.push(')');
        }
        line.push_str(". ");
        line
    };
    detail.push_str(
        "This launch picked the branch name automatically and will not attach to old code.",
    );
    SpawnFailure {
        title: "Leftover branch in the way".to_string(),
        detail,
        hint: Some(format!(
            "Open the spawn dialog for this repo or workspace to reuse the leftover at its tip or recreate it from {base}, or delete the branch."
        )),
    }
}

/// `, 3 commits behind origin/main` — omitted entirely when the comparison
/// wasn't available, rather than claiming a misleading zero.
fn append_behind(line: &mut String, behind: Option<u32>, base: &str) {
    if let Some(behind) = behind {
        let plural = if behind == 1 { "" } else { "s" };
        let _ = write!(line, ", {behind} commit{plural} behind {base}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A branch-only leftover must name its tip and its distance from the
    /// base: those two numbers are the whole reason the spawn is refused.
    #[test]
    fn refusal_names_a_branch_only_leftover_tip_and_staleness() {
        let fork = ForkPoint {
            existing_branch_head: Some("abc1234".to_string()),
            existing_branch_behind_base: Some(7),
            ..ForkPoint::default()
        };
        let failure = leftover_refusal(
            "fixture",
            "wt/brave-otter",
            "origin/main",
            &fork,
            Path::new("X:/wt/fixture"),
        );
        assert_eq!(failure.title, "Leftover branch in the way");
        assert!(failure.detail.contains("wt/brave-otter"), "{failure:?}");
        assert!(failure.detail.contains("fixture"), "{failure:?}");
        assert!(failure.detail.contains("abc1234"), "{failure:?}");
        assert!(
            failure.detail.contains("7 commits behind origin/main"),
            "{failure:?}"
        );
        assert!(
            failure
                .hint
                .as_deref()
                .is_some_and(|h| h.contains("origin/main")),
            "{failure:?}"
        );
    }

    /// A leftover directory names the path, its HEAD, its staleness, and that
    /// it has uncommitted work — deleting it blind would lose that work.
    #[test]
    fn refusal_names_a_leftover_worktree_path_head_and_dirt() {
        let fork = ForkPoint {
            existing_worktree_head: Some("def5678".to_string()),
            existing_worktree_behind_base: Some(1),
            existing_worktree_dirty: true,
            ..ForkPoint::default()
        };
        let failure = leftover_refusal(
            "fixture",
            "wt/brave-otter",
            "main",
            &fork,
            Path::new("X:/wt/fixture"),
        );
        assert!(failure.detail.contains("X:/wt/fixture"), "{failure:?}");
        assert!(failure.detail.contains("def5678"), "{failure:?}");
        assert!(
            failure.detail.contains("1 commit behind main"),
            "{failure:?}"
        );
        assert!(
            failure.detail.contains("uncommitted changes"),
            "{failure:?}"
        );
    }
}
