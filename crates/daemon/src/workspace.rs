//! Workspace spawn helpers: resolving member branches, computing worktree
//! paths, and rendering preview rows.

use crate::git;
use crate::state::AppState;
use anyhow::{Context as _, anyhow};
use protocol::{MemberSpawnPreview, RepoEntry, WorkspaceEntry};
use std::path::PathBuf;

pub struct ResolvedMember {
    pub repo: RepoEntry,
    pub branch_exists: bool,
    pub effective_base: Option<String>,
    /// Path where claude will run for this member. When `use_worktree` is
    /// `true` this is the worktree dir under `<repo>.wt/<slug>`; when
    /// `false` it is the repo's primary directory (an in-place checkout).
    pub working_path: PathBuf,
    pub use_worktree: bool,
}

pub async fn resolve_workspace(
    state: &AppState,
    workspace_id: &str,
    branch_name: &str,
    explicit_base: Option<&str>,
    use_worktree: bool,
) -> anyhow::Result<(WorkspaceEntry, Vec<ResolvedMember>)> {
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

    let mut resolved = Vec::with_capacity(repos.len());
    for repo in repos {
        let path = PathBuf::from(&repo.path);
        let exists = git::list_branches(&path)
            .await
            .unwrap_or_default()
            .iter()
            .any(|b| b == branch_name);
        let base = if exists {
            None
        } else {
            Some(
                explicit_base
                    .map(String::from)
                    .or_else(|| repo.default_branch.clone())
                    .unwrap_or_else(|| "main".to_string()),
            )
        };
        let working_path = if use_worktree {
            git::default_worktree_path(&path, branch_name)
        } else {
            path.clone()
        };
        resolved.push(ResolvedMember {
            repo,
            branch_exists: exists,
            effective_base: base,
            working_path,
            use_worktree,
        });
    }
    Ok((workspace, resolved))
}

pub fn previews(resolved: &[ResolvedMember]) -> Vec<MemberSpawnPreview> {
    resolved
        .iter()
        .map(|m| MemberSpawnPreview {
            repo_id: m.repo.id.clone(),
            repo_name: m.repo.name.clone(),
            branch_exists: m.branch_exists,
            effective_base: m.effective_base.clone(),
            worktree_path: m.working_path.to_string_lossy().into_owned(),
        })
        .collect()
}

/// Materialize each member's chosen branch. For `use_worktree` members, this
/// adds a worktree (idempotent if the dir already exists). For in-place
/// members, this errors on a dirty working tree and then checks out (or
/// creates) the branch in the repo's primary directory.
pub async fn ensure_branches(resolved: &[ResolvedMember], branch_name: &str) -> anyhow::Result<()> {
    for member in resolved {
        if member.use_worktree {
            if member.working_path.exists() {
                continue;
            }
            let repo_path = PathBuf::from(&member.repo.path);
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
            git::checkout_in_place(&repo_path, branch_name, member.effective_base.as_deref())
                .await
                .with_context(|| {
                    format!(
                        "checking out {branch_name} in {} (in-place)",
                        member.repo.name
                    )
                })?;
        }
    }
    Ok(())
}
