//! Worktree placement for a spawn: previews, leftover refusals, creating, recreating or reusing a single spawn's worktree, branch pins, and worktree error classification.

use crate::server::Hub;
use crate::spawn_plan;
use anyhow::{Context as _, anyhow};
use protocol::SpawnTarget;
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

/// Whether a spawn target runs in a daemon-managed worktree. A standalone
/// shell has no repo and never does.
pub(in crate::server) fn target_uses_worktree(target: &protocol::SpawnTarget) -> bool {
    match target {
        protocol::SpawnTarget::Single { use_worktree, .. }
        | protocol::SpawnTarget::Workspace { use_worktree, .. } => *use_worktree,
        protocol::SpawnTarget::Standalone { .. } => false,
    }
}

/// Derive the on-disk worktree paths from a `SpawnConfig` + the session's
/// `SessionMember` list. Returns the per-member `worktree_path` strings when
/// the config used `use_worktree = true`, or an empty vec for in-place
/// sessions. Used to populate `SessionRecord::worktree_paths` at spawn time.
pub(super) fn worktree_paths_for_config(
    config: &protocol::SpawnConfig,
    members: &[protocol::SessionMember],
) -> Vec<String> {
    let use_worktree = match &config.target {
        protocol::SpawnTarget::Single { use_worktree, .. }
        | protocol::SpawnTarget::Workspace { use_worktree, .. } => *use_worktree,
        protocol::SpawnTarget::Standalone { .. } => false,
    };
    if use_worktree {
        members.iter().map(|m| m.worktree_path.clone()).collect()
    } else {
        Vec::new()
    }
}

/// Map a raw `git worktree add` failure to a tailored `SpawnFailure`. Known
/// git refusals (branch already checked out, path already exists) get an
/// actionable message; anything else falls back to git's full stderr chain.
pub(super) fn classify_worktree_error(
    err: &anyhow::Error,
    branch_name: &str,
) -> spawn_plan::SpawnFailure {
    let chain = format!("{err:#}");
    let lower = chain.to_lowercase();
    if lower.contains("already checked out") || lower.contains("already used by worktree") {
        spawn_plan::SpawnFailure {
            title: "Branch already checked out".to_string(),
            detail: format!(
                "Git won't create a worktree for '{branch_name}' because that branch is already checked out elsewhere — a branch can only be checked out in one worktree at a time."
            ),
            hint: Some(
                "Pick a different branch name (the Random button generates a unique one), or base the new worktree on a fresh branch."
                    .to_string(),
            ),
        }
    } else if lower.contains("already exists") {
        spawn_plan::SpawnFailure {
            title: "Worktree path already exists".to_string(),
            detail: format!("A directory for this worktree already exists on disk.\n\n{chain}"),
            hint: Some(
                "Choose a different branch name, or remove the stale worktree directory and try again."
                    .to_string(),
            ),
        }
    } else {
        spawn_plan::SpawnFailure {
            title: "Couldn't create worktree".to_string(),
            detail: chain,
            hint: None,
        }
    }
}

/// Resolve a single-repo spawn without touching the disk: which ref the
/// branch would fork from, how stale that ref is, and whether a worktree is
/// already sitting at the target path.
///
/// Deliberately mirrors `spawn_single`'s resolution rather than duplicating
/// it — both call `spawn_plan::resolve_base_for_create`, so a preview can't
/// promise a fork point the spawn won't honor.
pub(super) async fn preview_single_spawn(
    hub: &Hub,
    repo_id: &str,
    branch_name: &str,
    base_branch: Option<&str>,
    use_worktree: bool,
) -> anyhow::Result<protocol::MemberSpawnPreview> {
    let repo = hub
        .state
        .with_persisted(|s| s.repos.iter().find(|r| r.id == repo_id).cloned())
        .ok_or_else(|| anyhow!("unknown repo: {repo_id}"))?;
    let repo_path = PathBuf::from(&repo.path);

    let branch_exists = crate::git::list_branches(&repo_path)
        .await
        .unwrap_or_default()
        .iter()
        .any(|b| b == branch_name);
    // `effective_base` describes branch *creation*, so it stays empty when the
    // branch already exists. The resolved ref is still computed either way:
    // RecreateFromBase forks the worktree from it, and a leftover worktree
    // always has its branch, so gating this on `branch_exists` would blank out
    // the staleness figures in exactly the case they matter most.
    let resolved_base = spawn_plan::resolve_base_for_create(&hub.state, &repo, base_branch).await;
    let effective_base = (!branch_exists).then(|| resolved_base.clone());

    let worktree_path = if use_worktree {
        single_worktree_target(&hub.state.worktrees_dir(), &repo, branch_name).await?
    } else {
        repo_path.clone()
    };
    let existing = (use_worktree && worktree_path.exists()).then_some(worktree_path.as_path());
    // Branch exists but no worktree does: the leftover a discarded session
    // leaves behind. A plain add would attach it at its old tip.
    let leftover_branch =
        (use_worktree && branch_exists && existing.is_none()).then_some(branch_name);

    let fork = spawn_plan::fork_point(
        &repo_path,
        Some(&resolved_base),
        Some(&resolved_base),
        existing,
        leftover_branch,
    )
    .await;
    Ok(protocol::MemberSpawnPreview {
        repo_id: repo.id.clone(),
        repo_name: repo.name.clone(),
        branch_exists,
        effective_base,
        worktree_path: worktree_path.to_string_lossy().into_owned(),
        resolved_base_ref: Some(resolved_base),
        base_remote_ref: fork.base_remote_ref,
        base_behind_remote: fork.base_behind_remote,
        worktree_exists: existing.is_some(),
        existing_worktree_head: fork.existing_worktree_head,
        existing_worktree_dirty: fork.existing_worktree_dirty,
        existing_worktree_behind_base: fork.existing_worktree_behind_base,
        existing_branch_head: fork.existing_branch_head,
        existing_branch_behind_base: fork.existing_branch_behind_base,
    })
}

/// Where a single-repo session of `repo` on `branch_name` puts its worktree:
/// the worktree under `worktrees_root` that already holds the branch, else
/// the repo's group folder `wt.<branch>/<repo-name>`. Errors when that folder
/// exists but belongs to something other than this repo.
pub(super) async fn single_worktree_target(
    worktrees_root: &Path,
    repo: &protocol::RepoEntry,
    branch_name: &str,
) -> anyhow::Result<PathBuf> {
    let repo_path = Path::new(&repo.path);
    let derived = crate::git::single_worktree_path(worktrees_root, branch_name, &repo.name);
    let target =
        crate::git::existing_or_derived_worktree(repo_path, worktrees_root, branch_name, derived)
            .await;
    crate::git::refuse_foreign_worktree_folder(repo_path, &repo.name, &target).await?;
    Ok(target)
}

/// Measure the leftover sitting where a spawn wanted to go, the same way the
/// spawn dialog's preview measures it, and wrap it in the refusal the client
/// renders as a blocking modal.
///
/// A leftover directory is described through its own HEAD; a branch with no
/// directory through the branch tip. Nothing is mutated — the caller returns
/// this instead of creating anything.
async fn leftover_refusal_for(
    repo_name: &str,
    repo_path: &Path,
    worktree_path: &Path,
    branch_name: &str,
    base: &str,
) -> anyhow::Error {
    let existing = worktree_path.exists().then_some(worktree_path);
    let leftover_branch = existing.is_none().then_some(branch_name);
    let fork =
        spawn_plan::fork_point(repo_path, Some(base), Some(base), existing, leftover_branch).await;
    warn!(
        branch_name,
        base,
        worktree = %worktree_path.display(),
        "refusing to spawn onto a leftover branch or worktree"
    );
    anyhow::Error::new(spawn_plan::leftover_refusal(
        repo_name,
        branch_name,
        base,
        &fork,
        worktree_path,
    ))
}

/// Create or bind the single-repo worktree at `worktree_path`, whatever
/// state the target is in: a directory already present (delegated to
/// [`recreate_or_reuse_worktree`]), a branch-only leftover from a discarded
/// session, or nothing at all (a plain add from `base`).
///
/// The branch-only case is the stale-pin trap: a `-b` add against the
/// leftover branch fails on "already exists", while attaching it silently
/// pins the session to the old fork point. Reuse attaches it at its old tip
/// deliberately (the collision prompt showed the user that tip); recreate
/// deletes the branch and forks fresh from `base`; refuse raises a
/// [`spawn_plan::leftover_refusal`] before anything is created, because the
/// caller picked the name automatically and nobody was asked about it.
pub(super) async fn materialize_single_worktree(
    repo_name: &str,
    repo_path: &Path,
    worktree_path: &Path,
    branch_name: &str,
    base: &str,
    policy: protocol::WorktreeReusePolicy,
) -> anyhow::Result<()> {
    if worktree_path.exists() {
        return recreate_or_reuse_worktree(
            repo_name,
            repo_path,
            worktree_path,
            branch_name,
            base,
            policy,
        )
        .await;
    }
    let branch_exists = crate::git::list_branches(repo_path)
        .await
        .unwrap_or_default()
        .iter()
        .any(|b| b == branch_name);
    let create_from = if branch_exists {
        match policy {
            protocol::WorktreeReusePolicy::Reuse | protocol::WorktreeReusePolicy::Unknown => {
                info!(
                    branch_name,
                    "attaching pre-existing branch at its current tip; base branch not applied"
                );
                None
            }
            protocol::WorktreeReusePolicy::RecreateFromBase => {
                warn!(
                    branch_name,
                    base, "deleting leftover branch and forking fresh from base"
                );
                // A folder deleted by hand still counts as a checkout of the
                // branch, and git refuses `branch -D` while any worktree holds
                // it; clear just that stale registration first.
                crate::git::forget_stale_worktree(repo_path, branch_name, worktree_path).await?;
                if let Err(err) = crate::git::delete_branch(repo_path, branch_name).await {
                    debug!(?err, branch_name, "leftover branch delete failed");
                }
                Some(base)
            }
            protocol::WorktreeReusePolicy::RefuseLeftover => {
                return Err(leftover_refusal_for(
                    repo_name,
                    repo_path,
                    worktree_path,
                    branch_name,
                    base,
                )
                .await);
            }
        }
    } else {
        Some(base)
    };
    crate::git::worktree_add(repo_path, worktree_path, branch_name, create_from)
        .await
        .map_err(|e| anyhow::Error::new(classify_worktree_error(&e, branch_name)))
}

/// Apply `policy` to a worktree directory that already exists at
/// `worktree_path`.
///
/// `Reuse` (and `Unknown`, which decodes from a client newer than this
/// daemon) binds to it untouched — the historical behavior. Recreating drops
/// the worktree and re-adds it from `base`, which discards anything
/// uncommitted; the client only offers that after showing the user the
/// worktree's dirty state. Refusing raises before either happens.
async fn recreate_or_reuse_worktree(
    repo_name: &str,
    repo_path: &Path,
    worktree_path: &Path,
    branch_name: &str,
    base: &str,
    policy: protocol::WorktreeReusePolicy,
) -> anyhow::Result<()> {
    match policy {
        protocol::WorktreeReusePolicy::RefuseLeftover => {
            Err(leftover_refusal_for(repo_name, repo_path, worktree_path, branch_name, base).await)
        }
        protocol::WorktreeReusePolicy::Reuse | protocol::WorktreeReusePolicy::Unknown => {
            info!(
                worktree = %worktree_path.display(),
                branch_name,
                "reusing existing worktree; base branch not applied"
            );
            Ok(())
        }
        protocol::WorktreeReusePolicy::RecreateFromBase => {
            warn!(
                worktree = %worktree_path.display(),
                branch_name,
                base,
                "recreating existing worktree from base; uncommitted work there is discarded"
            );
            crate::git::worktree_remove(
                repo_path,
                worktree_path,
                crate::git::WorktreeRemoveForce::Plain,
            )
            .await
            .with_context(|| {
                format!(
                    "removing existing worktree at {} before recreating it",
                    worktree_path.display()
                )
            })?;
            // The branch itself survives `worktree remove`, so re-adding with
            // `-b` would fail on "branch already exists". Delete it too — the
            // user asked for a fresh fork from `base`, not a rebind to the old
            // branch tip. A registration left at a folder deleted by hand
            // still holds the branch, so clear that before deleting it.
            crate::git::forget_stale_worktree(repo_path, branch_name, worktree_path).await?;
            if let Err(err) = crate::git::delete_branch(repo_path, branch_name).await {
                debug!(
                    ?err,
                    branch_name, "no local branch to delete before recreate"
                );
            }
            crate::git::worktree_add(repo_path, worktree_path, branch_name, Some(base))
                .await
                .map_err(|e| anyhow::Error::new(classify_worktree_error(&e, branch_name)))
        }
    }
}

/// Record where a worktree actually ended up relative to its base. Cheap
/// local git calls, and the one line in the log that answers "why is this
/// branch missing work that landed weeks ago".
pub(super) async fn log_fork_point(
    repo_path: &Path,
    branch_name: &str,
    base: &str,
    worktree_path: &Path,
) {
    let fork =
        spawn_plan::fork_point(repo_path, Some(base), Some(base), Some(worktree_path), None).await;
    info!(
        branch_name,
        base,
        head = fork.existing_worktree_head.as_deref().unwrap_or("?"),
        behind_base = fork.existing_worktree_behind_base.unwrap_or(0),
        base_behind_remote = fork.base_behind_remote.unwrap_or(0),
        "worktree fork point"
    );
}

/// Everything a single-repo spawn needs beyond the repo id. Grouped into a
/// struct rather than passed positionally — the list crossed the readable
/// arity a while back, and the two worktree-shaped fields
/// (`worktree_reuse`, `existing_worktree`) are easy to transpose at a call
/// site when they're bare positions.
pub(super) struct SinglePlan<'a> {
    pub(super) branch_name: &'a str,
    pub(super) base_branch: Option<String>,
    pub(super) use_worktree: bool,
    pub(super) checkout_strategy: Option<protocol::CheckoutStrategy>,
    pub(super) worktree_reuse: protocol::WorktreeReusePolicy,
    pub(super) existing_worktree: Option<String>,
}

/// A pin names a directory to run in; in-place mode runs in the repo itself.
/// Asking for both is contradictory, and honoring either one silently would
/// put the session somewhere the user didn't choose.
pub(super) fn reject_pin_without_worktree(target: &SpawnTarget) -> anyhow::Result<()> {
    let pinned = match target {
        SpawnTarget::Single {
            use_worktree,
            existing_worktree,
            ..
        } => !*use_worktree && existing_worktree.is_some(),
        SpawnTarget::Workspace {
            use_worktree,
            existing_worktrees,
            ..
        } => !*use_worktree && !existing_worktrees.is_empty(),
        SpawnTarget::Standalone { .. } => false,
    };
    if pinned {
        return Err(anyhow!(
            "cannot pin an existing worktree on an in-place spawn: pass use_worktree = true or drop the pin"
        ));
    }
    Ok(())
}

/// Branch actually checked out in `worktree`, falling back to `requested` for
/// a detached HEAD or an unreadable tree. Pinned spawns record this rather
/// than the requested name so the sidebar shows the branch the agent will
/// really be working on.
pub(super) async fn pinned_branch(worktree: &Path, requested: &str) -> String {
    match crate::git::current_branch(worktree).await {
        Ok(Some(branch)) => branch,
        Ok(None) => {
            info!(
                worktree = %worktree.display(),
                requested, "pinned worktree is on a detached HEAD; labelling with the requested branch"
            );
            requested.to_string()
        }
        Err(err) => {
            warn!(
                ?err,
                worktree = %worktree.display(),
                "reading pinned worktree HEAD failed; labelling with the requested branch"
            );
            requested.to_string()
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use std::path::Path;
    use std::process::Command;

    fn git(cwd: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn commit(repo: &Path, name: &str) {
        std::fs::write(repo.join(name), "x\n").expect("write file");
        git(repo, &["add", "."]);
        git(repo, &["commit", "-m", name]);
    }

    fn rev(repo: &Path, refname: &str) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", refname])
            .output()
            .expect("spawn git");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    mod refuse_leftover {
        use super::super::materialize_single_worktree;
        use super::{commit, git, rev};
        use crate::spawn_plan::SpawnFailure;
        use std::path::PathBuf;
        use std::process::Command;

        /// A repo where `wt/x` sits at the old `main` tip with no worktree
        /// attached — what "close session and delete worktree" leaves behind
        /// — while `release` stands in for a base that moved on.
        fn seed_branch_only_leftover(tag: &str) -> (PathBuf, PathBuf) {
            let root = std::env::temp_dir().join(format!("rt-refuse-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            let repo = root.join("repo");
            std::fs::create_dir_all(&repo).expect("create repo dir");
            git(&repo, &["init", "-b", "main"]);
            git(&repo, &["config", "user.email", "t@example.com"]);
            git(&repo, &["config", "user.name", "Test"]);
            git(&repo, &["config", "commit.gpgsign", "false"]);
            commit(&repo, "seed.txt");
            git(&repo, &["branch", "wt/x"]);
            git(&repo, &["checkout", "-b", "release"]);
            commit(&repo, "landed.txt");
            git(&repo, &["checkout", "main"]);
            (root, repo)
        }

        fn refusal_title(err: &anyhow::Error) -> String {
            err.downcast_ref::<SpawnFailure>().map_or_else(
                || format!("not a SpawnFailure: {err:#}"),
                |failure| failure.title.clone(),
            )
        }

        /// The trap this policy exists for: a branch a discarded session left
        /// behind must stop the spawn, not get attached at its stale tip.
        #[tokio::test]
        async fn a_branch_only_leftover_is_refused_without_creating_anything() {
            let (root, repo) = seed_branch_only_leftover("branch-only");
            let worktree = root.join("wt");

            let err = materialize_single_worktree(
                "fixture",
                &repo,
                &worktree,
                "wt/x",
                "release",
                protocol::WorktreeReusePolicy::RefuseLeftover,
            )
            .await
            .expect_err("a leftover branch must refuse the spawn");

            assert_eq!(refusal_title(&err), "Leftover branch in the way");
            assert!(!worktree.exists(), "no worktree may be created");
            let listed = Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["worktree", "list"])
                .output()
                .expect("spawn git");
            let listed = String::from_utf8_lossy(&listed.stdout);
            assert!(
                !listed.contains("wt/x"),
                "the leftover branch must still have no worktree: {listed}"
            );

            let _ = std::fs::remove_dir_all(&root);
        }

        #[tokio::test]
        async fn a_leftover_worktree_directory_is_refused() {
            let (root, repo) = seed_branch_only_leftover("dir");
            let worktree = root.join("wt");
            git(
                &repo,
                &["worktree", "add", &worktree.to_string_lossy(), "wt/x"],
            );
            let before = rev(&worktree, "HEAD");

            let err = materialize_single_worktree(
                "fixture",
                &repo,
                &worktree,
                "wt/x",
                "release",
                protocol::WorktreeReusePolicy::RefuseLeftover,
            )
            .await
            .expect_err("a leftover worktree must refuse the spawn");

            assert_eq!(refusal_title(&err), "Leftover branch in the way");
            assert_eq!(
                rev(&worktree, "HEAD"),
                before,
                "the refusal must not touch the existing worktree"
            );

            let _ = std::fs::remove_dir_all(&root);
        }

        /// Refusing is only about leftovers: a name nothing has claimed still
        /// gets its worktree, forked from the resolved base.
        #[tokio::test]
        async fn a_clean_name_is_created_from_the_base() {
            let (root, repo) = seed_branch_only_leftover("clean");
            let worktree = root.join("wt-fresh");

            materialize_single_worktree(
                "fixture",
                &repo,
                &worktree,
                "wt/fresh",
                "release",
                protocol::WorktreeReusePolicy::RefuseLeftover,
            )
            .await
            .expect("a free name must still be created");

            assert_eq!(
                rev(&worktree, "HEAD"),
                rev(&repo, "release"),
                "a fresh worktree must fork from the resolved base"
            );

            let _ = std::fs::remove_dir_all(&root);
        }
    }

    mod recreate {
        use super::super::materialize_single_worktree;
        use super::{commit, git, rev};

        /// A hand-deleted worktree folder leaves its branch registered as
        /// checked out there, so `git branch -D` is refused and the re-add
        /// fails on "branch already exists". Recreate must clear just that
        /// stale registration first.
        #[tokio::test]
        async fn recreate_after_a_hand_deleted_worktree_succeeds() {
            let root =
                std::env::temp_dir().join(format!("rt-recreate-hand-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            let repo = root.join("repo");
            std::fs::create_dir_all(&repo).expect("create repo dir");
            git(&repo, &["init", "-b", "main"]);
            git(&repo, &["config", "user.email", "t@example.com"]);
            git(&repo, &["config", "user.name", "Test"]);
            git(&repo, &["config", "commit.gpgsign", "false"]);
            commit(&repo, "seed.txt");
            let worktree = root.join("wt");
            git(
                &repo,
                &[
                    "worktree",
                    "add",
                    "-b",
                    "wt/recreate",
                    &worktree.to_string_lossy(),
                    "main",
                ],
            );
            // The branch moves ahead of the base, so "points at the base"
            // after the recreate is a real assertion.
            commit(&worktree, "branch-only.txt");
            std::fs::remove_dir_all(&worktree).expect("delete the worktree folder by hand");

            materialize_single_worktree(
                "fixture",
                &repo,
                &worktree,
                "wt/recreate",
                "main",
                protocol::WorktreeReusePolicy::RecreateFromBase,
            )
            .await
            .expect("recreating over a hand-deleted worktree must succeed");

            assert!(worktree.exists(), "the recreated worktree must be on disk");
            assert_eq!(
                rev(&repo, "wt/recreate"),
                rev(&repo, "main"),
                "the recreated branch must point at the base again"
            );

            let _ = std::fs::remove_dir_all(&root);
        }
    }
}
