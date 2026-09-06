//! Workspace spawn helpers: resolving member branches, computing worktree
//! paths, and rendering preview rows.

use crate::git;
use crate::spawn_plan;
use crate::state::AppState;
use anyhow::{Context as _, anyhow};
use protocol::{
    MemberSpawnPreview, PinnedMemberWorktree, RepoEntry, WorkspaceEntry, WorktreeReusePolicy,
};
use std::path::{Path, PathBuf};
use tracing::{info, warn};

pub struct ResolvedMember {
    pub repo: RepoEntry,
    pub branch_exists: bool,
    /// Base for branch *creation* — `None` when the branch already exists and
    /// nothing will be created.
    pub effective_base: Option<String>,
    /// The ref this member would fork from, resolved regardless of whether the
    /// branch exists. Recreating a worktree forks from it, and the staleness
    /// figures are measured against it — both of which apply precisely when
    /// the branch is already there.
    pub resolved_base: String,
    /// Path where claude will run for this member. When `use_worktree` is
    /// `true` this is the worktree dir under
    /// `<worktrees_root>/<sanitized-anchor>/wt.<slug>/<rel-to-anchor>`; when
    /// `false` it is the repo's primary directory (an in-place checkout).
    pub working_path: PathBuf,
    pub use_worktree: bool,
    /// True when `working_path` came from a caller-supplied pin rather than
    /// from branch-name derivation. Pinned members already have a worktree on
    /// disk, so nothing is created or checked out for them.
    pub pinned: bool,
}

/// Inputs that decide where each workspace member runs.
pub struct ResolveRequest<'a> {
    pub branch_name: &'a str,
    pub explicit_base: Option<&'a str>,
    pub use_worktree: bool,
    /// Members bound to an existing worktree directory. Members not named here
    /// derive their path from `branch_name` as usual — which puts them in the
    /// same `wt.<slug>/` group as the pins, so a workspace that gained a repo
    /// after the group was created still launches. Pins naming a repo that
    /// isn't a member of this workspace are ignored.
    pub pins: &'a [PinnedMemberWorktree],
}

pub async fn resolve_workspace(
    state: &AppState,
    worktrees_root: &Path,
    workspace_id: &str,
    req: ResolveRequest<'_>,
) -> anyhow::Result<(WorkspaceEntry, Vec<ResolvedMember>)> {
    let ResolveRequest {
        branch_name,
        explicit_base,
        use_worktree,
        pins,
    } = req;
    info!(
        workspace_id,
        branch_name,
        use_worktree,
        pins = pins.len(),
        "resolve_workspace: begin"
    );
    let (workspace, repos) = state.with_persisted(|s| {
        let ws = s.workspaces.iter().find(|w| w.id == workspace_id).cloned();
        let mut members = Vec::new();
        if let Some(ws) = &ws {
            for id in &ws.member_repo_ids {
                if let Some(repo) = s.repos.iter().find(|r| &r.id == id) {
                    members.push(repo.clone());
                }
            }
        }
        (ws, members)
    });
    let workspace = workspace.ok_or_else(|| anyhow!("unknown workspace: {workspace_id}"))?;
    if repos.is_empty() {
        return Err(anyhow!("workspace has no member repos"));
    }
    if repos.len() != workspace.member_repo_ids.len() {
        return Err(anyhow!(
            "workspace has {} members but only {} are registered",
            workspace.member_repo_ids.len(),
            repos.len()
        ));
    }

    // Compute worktree paths for *all* members in one call so they share a
    // common `wt.<slug>/` root and preserve inter-repo relative offsets.
    let member_paths: Vec<PathBuf> = repos.iter().map(|r| PathBuf::from(&r.path)).collect();
    let worktrees: Vec<PathBuf> = if use_worktree {
        let refs: Vec<&Path> = member_paths.iter().map(PathBuf::as_path).collect();
        git::workspace_worktree_paths(worktrees_root, &refs, branch_name)
    } else {
        member_paths.clone()
    };

    let mut resolved = Vec::with_capacity(repos.len());
    for ((repo, path), worktree_path) in repos.into_iter().zip(member_paths).zip(worktrees) {
        info!(repo_id = %repo.id, repo_path = %repo.path, "resolve_workspace: listing branches for member");
        let exists = git::list_branches(&path)
            .await
            .unwrap_or_default()
            .iter()
            .any(|b| b == branch_name);
        // Same resolution as spawn_single, including the upgrade from an
        // auto-detected local default to its remote-tracking counterpart.
        let resolved_base = spawn_plan::resolve_base_for_create(state, &repo, explicit_base).await;
        let pin = use_worktree
            .then(|| pins.iter().find(|p| p.repo_id == repo.id))
            .flatten();
        let (working_path, pinned) = match pin {
            Some(pin) => {
                let path = crate::paths::resolve_existing_dir(&pin.path)
                    .with_context(|| format!("pinned worktree for member {}", repo.name))?;
                info!(repo_id = %repo.id, worktree = %path.display(), "resolve_workspace: member pinned to an existing worktree");
                (path, true)
            }
            None => (worktree_path, false),
        };
        resolved.push(ResolvedMember {
            repo,
            branch_exists: exists,
            effective_base: (!exists).then(|| resolved_base.clone()),
            resolved_base,
            working_path,
            use_worktree,
            pinned,
        });
    }
    Ok((workspace, resolved))
}

/// Build one preview row per member, including the fork-point measurements
/// that tell the user where each branch would actually be cut from.
pub async fn previews(resolved: &[ResolvedMember], branch_name: &str) -> Vec<MemberSpawnPreview> {
    let mut out = Vec::with_capacity(resolved.len());
    for member in resolved {
        out.push(preview_for(member, branch_name).await);
    }
    out
}

async fn preview_for(member: &ResolvedMember, branch_name: &str) -> MemberSpawnPreview {
    let repo_path = PathBuf::from(&member.repo.path);
    // Only a worktree member can collide with an existing directory; an
    // in-place member's "working path" is the repo itself and always exists.
    let existing = (member.use_worktree && member.working_path.exists())
        .then_some(member.working_path.as_path());
    // A branch with no worktree is the leftover a discarded session leaves
    // behind; a plain add would attach it at its old tip, so measure it.
    let leftover_branch =
        (member.use_worktree && member.branch_exists && existing.is_none()).then_some(branch_name);
    let base = Some(member.resolved_base.as_str());
    let fork = spawn_plan::fork_point(&repo_path, base, base, existing, leftover_branch).await;
    MemberSpawnPreview {
        repo_id: member.repo.id.clone(),
        repo_name: member.repo.name.clone(),
        branch_exists: member.branch_exists,
        effective_base: member.effective_base.clone(),
        worktree_path: member.working_path.to_string_lossy().into_owned(),
        resolved_base_ref: Some(member.resolved_base.clone()),
        base_remote_ref: fork.base_remote_ref,
        base_behind_remote: fork.base_behind_remote,
        worktree_exists: existing.is_some(),
        existing_worktree_head: fork.existing_worktree_head,
        existing_worktree_dirty: fork.existing_worktree_dirty,
        existing_worktree_behind_base: fork.existing_worktree_behind_base,
        existing_branch_head: fork.existing_branch_head,
        existing_branch_behind_base: fork.existing_branch_behind_base,
    }
}

/// Refuse the whole spawn when any member already has the branch or its
/// worktree directory.
///
/// Runs before anything is created: a workspace spawn that failed halfway
/// would leave some members' worktrees on disk for a session that never
/// started, and the next attempt would then see leftovers it didn't make.
/// Pinned members are exempt — a pin *is* the user naming an existing
/// directory.
async fn refuse_member_leftovers(
    resolved: &[ResolvedMember],
    branch_name: &str,
) -> anyhow::Result<()> {
    for member in resolved {
        if member.pinned || !member.use_worktree {
            continue;
        }
        let dir_exists = member.working_path.exists();
        if !dir_exists && !member.branch_exists {
            continue;
        }
        let repo_path = PathBuf::from(&member.repo.path);
        let base = member.resolved_base.as_str();
        let existing = dir_exists.then_some(member.working_path.as_path());
        let leftover_branch = existing.is_none().then_some(branch_name);
        let fork = spawn_plan::fork_point(
            &repo_path,
            Some(base),
            Some(base),
            existing,
            leftover_branch,
        )
        .await;
        warn!(
            repo_id = %member.repo.id,
            branch_name,
            worktree = %member.working_path.display(),
            "ensure_branches: refusing to spawn onto a leftover branch or worktree"
        );
        return Err(spawn_plan::leftover_refusal(
            &member.repo.name,
            branch_name,
            base,
            &fork,
            &member.working_path,
        )
        .into());
    }
    Ok(())
}

/// Materialize each member's chosen branch. For `use_worktree` members, this
/// adds a worktree (idempotent if the dir already exists). For in-place
/// members, this errors on a dirty working tree and then checks out (or
/// creates) the branch in the repo's primary directory.
pub async fn ensure_branches(
    resolved: &[ResolvedMember],
    branch_name: &str,
    worktree_reuse: WorktreeReusePolicy,
) -> anyhow::Result<()> {
    info!(
        members = resolved.len(),
        branch_name, "ensure_branches: begin"
    );
    if matches!(worktree_reuse, WorktreeReusePolicy::RefuseLeftover) {
        refuse_member_leftovers(resolved, branch_name).await?;
    }
    for (i, member) in resolved.iter().enumerate() {
        info!(
            idx = i,
            repo_id = %member.repo.id,
            repo_path = %member.repo.path,
            use_worktree = member.use_worktree,
            "ensure_branches: member step"
        );
        if member.pinned {
            info!(
                idx = i,
                worktree = %member.working_path.display(),
                "ensure_branches: member pinned to an existing worktree, nothing to create"
            );
            continue;
        }
        if member.use_worktree {
            let repo_path = PathBuf::from(&member.repo.path);
            // Empty when the branch already exists, which makes the add below
            // attach that branch as-is. A recreate overrides it: the branch is
            // deleted first, so the member is creating one either way and has
            // to fork from the resolved base.
            // Under `RefuseLeftover` neither branch nor directory exists —
            // `refuse_member_leftovers` already returned otherwise — so the
            // member falls through to a plain create from `effective_base`.
            let mut create_from = member.effective_base.as_deref();
            let dir_exists = member.working_path.exists();
            if dir_exists
                && matches!(
                    worktree_reuse,
                    WorktreeReusePolicy::Reuse | WorktreeReusePolicy::Unknown
                )
            {
                info!(
                    idx = i,
                    worktree = %member.working_path.display(),
                    "ensure_branches: worktree dir already present, reusing it as-is"
                );
                continue;
            }
            // Recreate covers both a full leftover (dir + branch) and a
            // branch-only leftover from a discarded session — either way the
            // user asked for a fresh fork from the resolved base, not a
            // rebind to the old branch tip.
            if matches!(worktree_reuse, WorktreeReusePolicy::RecreateFromBase)
                && (dir_exists || member.branch_exists)
            {
                if dir_exists {
                    warn!(
                        idx = i,
                        worktree = %member.working_path.display(),
                        branch_name,
                        "ensure_branches: recreating worktree from base; uncommitted work there is discarded"
                    );
                    git::worktree_remove(&repo_path, &member.working_path)
                        .await
                        .with_context(|| {
                            format!(
                                "removing existing worktree at {} for {}",
                                member.working_path.display(),
                                member.repo.name
                            )
                        })?;
                }
                // `worktree remove` leaves the branch, so a `-b` re-add
                // would fail on "already exists".
                if let Err(err) = git::delete_branch(&repo_path, branch_name).await {
                    info!(
                        ?err,
                        branch_name, "no local branch to delete before recreate"
                    );
                }
                create_from = Some(&member.resolved_base);
            }
            git::worktree_add(&repo_path, &member.working_path, branch_name, create_from)
                .await
                .with_context(|| {
                    format!(
                        "creating worktree in {} for branch {branch_name}",
                        member.repo.name
                    )
                })?;
        } else {
            let repo_path = PathBuf::from(&member.repo.path);
            // Workspace in-place checkout keeps the safe default (error on a
            // dirty member); the confirm-and-strategy flow is single-repo only.
            git::checkout_in_place(
                &repo_path,
                branch_name,
                member.effective_base.as_deref(),
                None,
            )
            .await
            .with_context(|| {
                format!(
                    "checking out {branch_name} in {} (in-place)",
                    member.repo.name
                )
            })?;
        }
    }
    info!("ensure_branches: done");
    Ok(())
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests assert preconditions with expect")]
mod tests {
    use super::*;
    use protocol::AppearanceOverrides;
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

    fn member(repo: &Path, working_path: PathBuf, resolved_base: &str) -> ResolvedMember {
        ResolvedMember {
            repo: RepoEntry {
                id: "r1".to_string(),
                name: "fixture".to_string(),
                path: repo.to_string_lossy().into_owned(),
                default_branch: Some("main".to_string()),
                default_use_worktree: true,
                appearance: AppearanceOverrides::default(),
                last_agent: None,
                last_spawn_config: None,
            },
            // The case that matters: the branch is already there, because a
            // leftover worktree always has one.
            branch_exists: true,
            effective_base: None,
            resolved_base: resolved_base.to_string(),
            working_path,
            use_worktree: true,
            pinned: false,
        }
    }

    /// Recreating a member whose branch already exists must fork from
    /// `resolved_base`. Reading `effective_base` here instead would pass
    /// `None` to a `worktree add` whose branch was just deleted, which fails
    /// outright — and before that field existed, silently re-attached the old
    /// branch tip and left the stale fork point in place.
    #[tokio::test]
    async fn recreate_forks_an_existing_branch_from_the_resolved_base() {
        let (root, repo) = seed_branch_only_leftover("recreate");
        let worktree = root.join("wt");
        git(
            &repo,
            &["worktree", "add", &worktree.to_string_lossy(), "wt/x"],
        );
        assert_ne!(
            rev(&worktree, "HEAD"),
            rev(&repo, "release"),
            "precondition: the worktree starts on the stale branch"
        );

        let resolved = vec![member(&repo, worktree.clone(), "release")];
        ensure_branches(&resolved, "wt/x", WorktreeReusePolicy::RecreateFromBase)
            .await
            .expect("recreate from base");

        assert_eq!(
            rev(&worktree, "HEAD"),
            rev(&repo, "release"),
            "recreated worktree must sit on the resolved base"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Reuse must leave the worktree untouched — it is the default, and the
    /// non-destructive half of the collision prompt.
    #[tokio::test]
    async fn reuse_leaves_an_existing_worktree_alone() {
        let (root, repo) = seed_branch_only_leftover("reuse");
        let worktree = root.join("wt");
        git(
            &repo,
            &["worktree", "add", &worktree.to_string_lossy(), "wt/x"],
        );
        let before = rev(&worktree, "HEAD");
        std::fs::write(worktree.join("scratch.txt"), "uncommitted\n").expect("dirty the worktree");

        let resolved = vec![member(&repo, worktree.clone(), "release")];
        ensure_branches(&resolved, "wt/x", WorktreeReusePolicy::Reuse)
            .await
            .expect("reuse");

        assert_eq!(rev(&worktree, "HEAD"), before, "reuse must not move HEAD");
        assert!(
            worktree.join("scratch.txt").exists(),
            "reuse must not discard uncommitted work"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Seed a repo where `wt/x` exists at the old `main` tip with no worktree
    /// attached — exactly what "close session and delete worktree" leaves
    /// behind — while `release` stands in for a base that moved on.
    fn seed_branch_only_leftover(tag: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("rt-ws-{tag}-{}", std::process::id()));
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

    /// The stale-pin bug: a discarded session leaves its branch behind, and a
    /// later spawn of the same name silently attached it at the old tip.
    /// Under `RecreateFromBase` the leftover branch must be deleted and the
    /// worktree forked fresh from the resolved base.
    #[tokio::test]
    async fn recreate_forks_a_branch_only_leftover_from_the_resolved_base() {
        let (root, repo) = seed_branch_only_leftover("recreate-branch-only");
        let worktree = root.join("wt");
        assert!(!worktree.exists(), "precondition: no worktree dir");

        let resolved = vec![member(&repo, worktree.clone(), "release")];
        ensure_branches(&resolved, "wt/x", WorktreeReusePolicy::RecreateFromBase)
            .await
            .expect("recreate from base");

        assert_eq!(
            rev(&worktree, "HEAD"),
            rev(&repo, "release"),
            "worktree must fork from the resolved base, not the leftover tip"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Reuse (the default) attaches a branch-only leftover at its old tip —
    /// the historical behavior, kept deliberately and now surfaced by the
    /// collision prompt instead of happening silently.
    #[tokio::test]
    async fn reuse_attaches_a_branch_only_leftover_at_its_old_tip() {
        let (root, repo) = seed_branch_only_leftover("reuse-branch-only");
        let worktree = root.join("wt");
        let old_tip = rev(&repo, "wt/x");

        let resolved = vec![member(&repo, worktree.clone(), "release")];
        ensure_branches(&resolved, "wt/x", WorktreeReusePolicy::Reuse)
            .await
            .expect("reuse");

        assert_eq!(
            rev(&worktree, "HEAD"),
            old_tip,
            "reuse must attach the existing branch at its old tip"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A workspace spawn is all-or-nothing: one member's leftover must stop
    /// the whole launch before any *other* member's worktree is created, or
    /// the retry inherits directories nobody asked for.
    #[tokio::test]
    async fn refuse_leftover_stops_before_creating_any_member() {
        let root = std::env::temp_dir().join(format!("rt-ws-refuse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create scratch root");

        let clean = seed_member(&root, "clean", false);
        let leftover = seed_member(&root, "leftover", true);
        let clean_worktree = root.join("wt-clean");
        let leftover_worktree = root.join("wt-leftover");

        let mut first = member(&clean, clean_worktree.clone(), "release");
        first.branch_exists = false;
        first.effective_base = Some("release".to_string());
        let second = member(&leftover, leftover_worktree.clone(), "release");

        let err = ensure_branches(
            &[first, second],
            "wt/x",
            WorktreeReusePolicy::RefuseLeftover,
        )
        .await
        .expect_err("a member with a leftover branch must refuse the spawn");
        let failure = err
            .downcast_ref::<spawn_plan::SpawnFailure>()
            .expect("refusal must be a SpawnFailure");
        assert_eq!(failure.title, "Leftover branch in the way");

        assert!(
            !clean_worktree.exists(),
            "no member may be created once another member is refused"
        );
        assert!(
            !leftover_worktree.exists(),
            "the refused member is untouched"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A member repo with one commit on `main`, a `release` branch one commit
    /// ahead, and optionally the `wt/x` leftover a discarded session leaves.
    fn seed_member(root: &Path, name: &str, with_leftover_branch: bool) -> PathBuf {
        let repo = root.join(name);
        std::fs::create_dir_all(&repo).expect("create repo dir");
        git(&repo, &["init", "-b", "main"]);
        git(&repo, &["config", "user.email", "t@example.com"]);
        git(&repo, &["config", "user.name", "Test"]);
        git(&repo, &["config", "commit.gpgsign", "false"]);
        commit(&repo, "seed.txt");
        if with_leftover_branch {
            git(&repo, &["branch", "wt/x"]);
        }
        git(&repo, &["checkout", "-b", "release"]);
        commit(&repo, "landed.txt");
        git(&repo, &["checkout", "main"]);
        repo
    }

    /// A pinned member runs in whatever its worktree already has checked out.
    /// The branch the *spawn* asked for is only there for the members that
    /// still need creating, so a pin must not move HEAD onto it — even under
    /// `RecreateFromBase`, which would otherwise delete and re-add the worktree.
    #[tokio::test]
    async fn a_pinned_member_is_never_recreated_or_moved() {
        let root = std::env::temp_dir().join(format!("rt-ws-pinned-{}", std::process::id()));
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

        let worktree = root.join("wt");
        git(
            &repo,
            &["worktree", "add", &worktree.to_string_lossy(), "wt/x"],
        );
        let before = rev(&worktree, "HEAD");
        std::fs::write(worktree.join("scratch.txt"), "uncommitted\n").expect("dirty the worktree");

        let mut pinned = member(&repo, worktree.clone(), "release");
        pinned.pinned = true;
        ensure_branches(&[pinned], "wt/x", WorktreeReusePolicy::RecreateFromBase)
            .await
            .expect("a pinned member needs nothing materialized");

        assert_eq!(
            rev(&worktree, "HEAD"),
            before,
            "a pin must not move the worktree's HEAD"
        );
        assert!(
            worktree.join("scratch.txt").exists(),
            "a pin must not discard uncommitted work"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
