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
    pub worktree_path: PathBuf,
}

pub async fn resolve_workspace(
    state: &AppState,
    workspace_id: &str,
    branch_name: &str,
    explicit_base: Option<&str>,
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
        let worktree_path = git::default_worktree_path(&path, branch_name);
        resolved.push(ResolvedMember {
            repo,
            branch_exists: exists,
            effective_base: base,
            worktree_path,
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
            worktree_path: m.worktree_path.to_string_lossy().into_owned(),
        })
        .collect()
}

pub async fn ensure_worktrees(
    resolved: &[ResolvedMember],
    branch_name: &str,
) -> anyhow::Result<()> {
    for member in resolved {
        if member.worktree_path.exists() {
            continue;
        }
        let repo_path = PathBuf::from(&member.repo.path);
        git::worktree_add(
            &repo_path,
            &member.worktree_path,
            branch_name,
            member.effective_base.as_deref(),
        )
        .await
        .with_context(|| {
            format!("creating worktree in {} for branch {branch_name}", member.repo.name)
        })?;
    }
    Ok(())
}
