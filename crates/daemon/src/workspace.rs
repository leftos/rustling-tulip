//! Workspace spawn helpers: resolving member branches, computing worktree
//! paths, and rendering preview rows.

use crate::git;
use crate::spawn_plan;
use crate::state::AppState;
use anyhow::{Context as _, anyhow};
use protocol::{MemberSpawnPreview, RepoEntry, WorkspaceEntry, WorktreeReusePolicy};
use std::path::{Path, PathBuf};
use tracing::{info, warn};

pub struct ResolvedMember {
    pub repo: RepoEntry,
    pub branch_exists: bool,
    pub effective_base: Option<String>,
    /// Path where claude will run for this member. When `use_worktree` is
    /// `true` this is the worktree dir under
    /// `<worktrees_root>/<sanitized-anchor>/wt.<slug>/<rel-to-anchor>`; when
    /// `false` it is the repo's primary directory (an in-place checkout).
    pub working_path: PathBuf,
    pub use_worktree: bool,
}

pub async fn resolve_workspace(
    state: &AppState,
    worktrees_root: &Path,
    workspace_id: &str,
    branch_name: &str,
    explicit_base: Option<&str>,
    use_worktree: bool,
) -> anyhow::Result<(WorkspaceEntry, Vec<ResolvedMember>)> {
    info!(
        workspace_id,
        branch_name, use_worktree, "resolve_workspace: begin"
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
        let base = if exists {
            None
        } else {
            // Same resolution as spawn_single, including the upgrade from an
            // auto-detected local default to its remote-tracking counterpart.
            Some(spawn_plan::resolve_base_for_create(state, &repo, explicit_base).await)
        };
        resolved.push(ResolvedMember {
            repo,
            branch_exists: exists,
            effective_base: base,
            working_path: worktree_path,
            use_worktree,
        });
    }
    Ok((workspace, resolved))
}

/// Build one preview row per member, including the fork-point measurements
/// that tell the user where each branch would actually be cut from.
pub async fn previews(resolved: &[ResolvedMember]) -> Vec<MemberSpawnPreview> {
    let mut out = Vec::with_capacity(resolved.len());
    for member in resolved {
        out.push(preview_for(member).await);
    }
    out
}

async fn preview_for(member: &ResolvedMember) -> MemberSpawnPreview {
    let repo_path = PathBuf::from(&member.repo.path);
    // Only a worktree member can collide with an existing directory; an
    // in-place member's "working path" is the repo itself and always exists.
    let existing = (member.use_worktree && member.working_path.exists())
        .then_some(member.working_path.as_path());
    let base = member.effective_base.as_deref();
    let fork = spawn_plan::fork_point(&repo_path, base, base, existing).await;
    MemberSpawnPreview {
        repo_id: member.repo.id.clone(),
        repo_name: member.repo.name.clone(),
        branch_exists: member.branch_exists,
        effective_base: member.effective_base.clone(),
        worktree_path: member.working_path.to_string_lossy().into_owned(),
        resolved_base_ref: member.effective_base.clone(),
        base_remote_ref: fork.base_remote_ref,
        base_behind_remote: fork.base_behind_remote,
        worktree_exists: existing.is_some(),
        existing_worktree_head: fork.existing_worktree_head,
        existing_worktree_dirty: fork.existing_worktree_dirty,
        existing_worktree_behind_base: fork.existing_worktree_behind_base,
    }
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
    for (i, member) in resolved.iter().enumerate() {
        info!(
            idx = i,
            repo_id = %member.repo.id,
            repo_path = %member.repo.path,
            use_worktree = member.use_worktree,
            "ensure_branches: member step"
        );
        if member.use_worktree {
            let repo_path = PathBuf::from(&member.repo.path);
            if member.working_path.exists() {
                match worktree_reuse {
                    WorktreeReusePolicy::Reuse | WorktreeReusePolicy::Unknown => {
                        info!(
                            idx = i,
                            worktree = %member.working_path.display(),
                            "ensure_branches: worktree dir already present, reusing it as-is"
                        );
                        continue;
                    }
                    WorktreeReusePolicy::RecreateFromBase => {
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
                        // `worktree remove` leaves the branch, so a `-b` re-add
                        // would fail on "already exists".
                        if let Err(err) = git::delete_branch(&repo_path, branch_name).await {
                            info!(
                                ?err,
                                branch_name, "no local branch to delete before recreate"
                            );
                        }
                    }
                }
            }
            git::worktree_add(
                &repo_path,
                &member.working_path,
                branch_name,
                member.effective_base.as_deref(),
            )
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
