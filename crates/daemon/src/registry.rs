//! Repo / workspace registry mutations on top of `AppState`.

use crate::git;
use crate::paths::simplify_path;
use crate::state::{AppState, PersistedState};
use anyhow::{Context as _, anyhow};
use protocol::{Agent, ContainerRef, RepoEntry, SpawnConfig, WorkspaceEntry};
use std::path::Path;
use uuid::Uuid;

/// Append `ContainerRef::Repo(id)` to the manual order iff the order
/// list is non-empty (meaning the user has actually customized it).
/// If the order is empty, leave it alone — that signals "fall back to
/// alphabetical" and we don't want a fresh install to start carrying
/// implicit order around just because someone registered a repo.
fn append_to_container_order(state: &mut PersistedState, entry: ContainerRef) {
    if state.container_order.is_empty() {
        return;
    }
    if !state.container_order.contains(&entry) {
        state.container_order.push(entry);
    }
}

fn remove_from_container_order(state: &mut PersistedState, target: &ContainerRef) {
    state.container_order.retain(|e| e != target);
}

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
    let canonical =
        simplify_path(&std::fs::canonicalize(path).context("canonicalizing repo path")?);
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
        canonical.file_name().map_or_else(
            || canonical_str.clone(),
            |n| n.to_string_lossy().into_owned(),
        )
    });
    let default_branch = git::default_branch(&canonical).await;
    let entry = RepoEntry {
        id: Uuid::new_v4().to_string(),
        name,
        path: canonical_str,
        default_branch,
        default_use_worktree: true,
        last_agent: None,
        last_spawn_config: None,
    };

    state.mutate(|s| {
        s.repos.push(entry.clone());
        append_to_container_order(s, ContainerRef::Repo(entry.id.clone()));
    })?;
    Ok(entry)
}

pub fn set_repo_worktree_default(
    state: &AppState,
    repo_id: &str,
    value: bool,
) -> anyhow::Result<()> {
    state.mutate(|s| {
        if let Some(repo) = s.repos.iter_mut().find(|r| r.id == repo_id) {
            repo.default_use_worktree = value;
        }
    })
}

/// Record which agent was last spawned against a given repo. Drives the
/// spawn-dialog default so repeated launches don't force the user to re-pick.
/// Silently no-ops if `repo_id` does not match any registered repo.
pub fn persist_last_agent(state: &AppState, repo_id: &str, agent: Agent) -> anyhow::Result<()> {
    state.mutate(|s| {
        if let Some(repo) = s.repos.iter_mut().find(|r| r.id == repo_id) {
            repo.last_agent = Some(agent);
        }
    })
}

/// Capture the full spawn config from the latest single-repo launch so that
/// "Launch last again" (double-click on the sidebar row, context-menu submenu)
/// can replay it without re-prompting the user. Silently no-ops if `repo_id`
/// does not match any registered repo.
pub fn persist_repo_last_spawn_config(
    state: &AppState,
    repo_id: &str,
    config: SpawnConfig,
) -> anyhow::Result<()> {
    state.mutate(|s| {
        if let Some(repo) = s.repos.iter_mut().find(|r| r.id == repo_id) {
            repo.last_spawn_config = Some(config);
        }
    })
}

/// Workspace-level equivalent of [`persist_repo_last_spawn_config`]. Captures
/// the spawn config from the latest workspace launch on the workspace itself
/// (not on any member repo — that branch is reserved for single-repo spawns).
pub fn persist_workspace_last_spawn_config(
    state: &AppState,
    workspace_id: &str,
    config: SpawnConfig,
) -> anyhow::Result<()> {
    state.mutate(|s| {
        if let Some(ws) = s.workspaces.iter_mut().find(|w| w.id == workspace_id) {
            ws.last_spawn_config = Some(config);
        }
    })
}

pub fn set_workspace_worktree_default(
    state: &AppState,
    workspace_id: &str,
    value: bool,
) -> anyhow::Result<()> {
    state.mutate(|s| {
        if let Some(ws) = s.workspaces.iter_mut().find(|w| w.id == workspace_id) {
            ws.default_use_worktree = value;
        }
    })
}

pub fn remove_repo(state: &AppState, repo_id: &str) -> anyhow::Result<()> {
    state.mutate(|s| {
        s.repos.retain(|r| r.id != repo_id);
        for ws in &mut s.workspaces {
            ws.member_repo_ids.retain(|id| id != repo_id);
        }
        remove_from_container_order(s, &ContainerRef::Repo(repo_id.to_string()));
    })
}

pub fn upsert_workspace(
    state: &AppState,
    id: Option<String>,
    name: &str,
    member_repo_ids: Vec<String>,
    linked_vscode_workspace: Option<String>,
) -> anyhow::Result<WorkspaceEntry> {
    // Reject empty or whitespace-only names — they render as `WS ` rows in
    // the sidebar with nothing to disambiguate from one another. Also reject
    // duplicate names (case-insensitive, trimmed) against OTHER workspaces.
    // Comparing against `id` lets a rename-to-its-own-name no-op through.
    let trimmed_name = name.trim().to_string();
    if trimmed_name.is_empty() {
        return Err(anyhow!("workspace name cannot be empty"));
    }
    let id = id.unwrap_or_else(|| Uuid::new_v4().to_string());
    let normalized_new = trimmed_name.to_lowercase();
    let collision = state.with_persisted(|s| {
        s.workspaces
            .iter()
            .any(|w| w.id != id && w.name.trim().to_lowercase() == normalized_new)
    });
    if collision {
        return Err(anyhow!(
            "another workspace already uses the name {trimmed_name:?}"
        ));
    }
    // Preserve the existing worktree-default and the last-spawn-config when
    // updating an existing workspace; only first-time upserts start fresh.
    let (prior_default_use_worktree, prior_last_spawn_config) = state.with_persisted(|s| {
        s.workspaces
            .iter()
            .find(|w| w.id == id)
            .map_or((None, None), |w| {
                (Some(w.default_use_worktree), w.last_spawn_config.clone())
            })
    });
    let entry = WorkspaceEntry {
        id: id.clone(),
        name: trimmed_name,
        member_repo_ids,
        linked_vscode_workspace,
        default_use_worktree: prior_default_use_worktree.unwrap_or(true),
        last_spawn_config: prior_last_spawn_config,
    };
    state.mutate(|s| {
        if let Some(slot) = s.workspaces.iter_mut().find(|w| w.id == id) {
            *slot = entry.clone();
        } else {
            s.workspaces.push(entry.clone());
            append_to_container_order(s, ContainerRef::Workspace(entry.id.clone()));
        }
    })?;
    Ok(entry)
}

pub fn remove_workspace(state: &AppState, workspace_id: &str) -> anyhow::Result<()> {
    state.mutate(|s| {
        s.workspaces.retain(|w| w.id != workspace_id);
        remove_from_container_order(s, &ContainerRef::Workspace(workspace_id.to_string()));
    })
}

/// Replace the manual container order wholesale. Validates that every
/// ref in `ordered` matches a currently-registered repo or workspace —
/// unknown refs are rejected so the persisted order stays consistent
/// with the registry. Returns the post-validation order so the caller
/// can broadcast it back to clients verbatim.
pub fn reorder_containers(
    state: &AppState,
    ordered: Vec<ContainerRef>,
) -> anyhow::Result<Vec<ContainerRef>> {
    let (known_repos, known_workspaces) = state.with_persisted(|s| {
        (
            s.repos.iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
            s.workspaces.iter().map(|w| w.id.clone()).collect::<Vec<_>>(),
        )
    });
    for entry in &ordered {
        let ok = match entry {
            ContainerRef::Repo(id) => known_repos.iter().any(|r| r == id),
            ContainerRef::Workspace(id) => known_workspaces.iter().any(|w| w == id),
        };
        if !ok {
            return Err(anyhow!("reorder_containers: unknown ref {entry:?}"));
        }
    }
    state.mutate(|s| s.container_order.clone_from(&ordered))?;
    Ok(ordered)
}

/// Persist the user-chosen display order of sessions within a sidebar
/// container. `container_id` is the workspace id, repo id, or tab id.
/// Stale ids (sessions already removed) are kept in storage and silently
/// ignored at merge time — no cleanup is required here.
pub fn set_session_order(
    state: &AppState,
    container_id: &str,
    ordered_ids: Vec<String>,
) -> anyhow::Result<()> {
    state.mutate(|s| {
        s.session_order
            .insert(container_id.to_string(), ordered_ids);
    })
}

fn paths_eq(a: &str, b: &str) -> bool {
    let a = a.replace('\\', "/").to_lowercase();
    let b = b.replace('\\', "/").to_lowercase();
    a == b
}
