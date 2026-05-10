//! Repo / workspace registry mutations on top of `AppState`.

use crate::git;
use crate::state::AppState;
use anyhow::{Context as _, anyhow};
use protocol::{RepoEntry, WorkspaceEntry};
use std::path::Path;
use uuid::Uuid;

pub async fn add_repo(
    state: &AppState,
    path_str: &str,
    explicit_name: Option<String>,
) -> anyhow::Result<RepoEntry> {
    let path = Path::new(path_str);
    if !path.is_dir() {
        return Err(anyhow!("path is not a directory: {path_str}"));
    }
    if !path.join(".git").exists() {
        return Err(anyhow!("not a git repo (no .git): {path_str}"));
    }
    let canonical = std::fs::canonicalize(path).context("canonicalizing repo path")?;
    let canonical_str = canonical.to_string_lossy().into_owned();

    let existing = state.with_persisted(|s| {
        s.repos
            .iter()
            .find(|r| paths_eq(&r.path, &canonical_str))
            .cloned()
    });
    if let Some(found) = existing {
        return Ok(found);
    }

    let name = explicit_name.unwrap_or_else(|| {
        canonical
            .file_name()
            .map_or_else(|| canonical_str.clone(), |n| n.to_string_lossy().into_owned())
    });
    let default_branch = git::default_branch(&canonical).await;
    let entry = RepoEntry {
        id: Uuid::new_v4().to_string(),
        name,
        path: canonical_str,
        default_branch,
    };

    state.mutate(|s| s.repos.push(entry.clone()))?;
    Ok(entry)
}

pub fn remove_repo(state: &AppState, repo_id: &str) -> anyhow::Result<()> {
    state.mutate(|s| {
        s.repos.retain(|r| r.id != repo_id);
        for ws in &mut s.workspaces {
            ws.member_repo_ids.retain(|id| id != repo_id);
        }
    })
}

pub fn upsert_workspace(
    state: &AppState,
    id: Option<String>,
    name: String,
    member_repo_ids: Vec<String>,
    linked_vscode_workspace: Option<String>,
) -> anyhow::Result<WorkspaceEntry> {
    let id = id.unwrap_or_else(|| Uuid::new_v4().to_string());
    let entry = WorkspaceEntry {
        id: id.clone(),
        name,
        member_repo_ids,
        linked_vscode_workspace,
    };
    state.mutate(|s| {
        if let Some(slot) = s.workspaces.iter_mut().find(|w| w.id == id) {
            *slot = entry.clone();
        } else {
            s.workspaces.push(entry.clone());
        }
    })?;
    Ok(entry)
}

pub fn remove_workspace(state: &AppState, workspace_id: &str) -> anyhow::Result<()> {
    state.mutate(|s| s.workspaces.retain(|w| w.id != workspace_id))
}

fn paths_eq(a: &str, b: &str) -> bool {
    let a = a.replace('\\', "/").to_lowercase();
    let b = b.replace('\\', "/").to_lowercase();
    a == b
}
