//! Repo / workspace registry mutations on top of `AppState`.

use crate::git;
use crate::paths::simplify_path;
use crate::state::{AppState, PersistedState};
use anyhow::{Context as _, anyhow};
use protocol::{
    AgentId, AppearanceOverrides, ContainerRef, RepoEntry, SpawnConfig, WorkspaceEntry,
};
use std::path::Path;
use uuid::Uuid;

/// Refuse `name` when its worktree folder name ([`git::name_slug`]) is
/// already another registered repo's or workspace's. Repos and workspaces
/// share one namespace because both name their group folder under
/// `wt.<branch>/`. `own_id` is the entry being saved, which may keep its own
/// slug. State loaded from disk is never checked, only saves.
fn ensure_group_slug_free(
    state: &AppState,
    name: &str,
    own_id: Option<&str>,
) -> anyhow::Result<()> {
    let slug = git::name_slug(name);
    let clash = state.with_persisted(|s| {
        let repo = s
            .repos
            .iter()
            .filter(|r| Some(r.id.as_str()) != own_id)
            .find(|r| git::name_slug(&r.name) == slug)
            .map(|r| ("repo", r.name.clone()));
        repo.or_else(|| {
            s.workspaces
                .iter()
                .filter(|w| Some(w.id.as_str()) != own_id)
                .find(|w| git::name_slug(&w.name) == slug)
                .map(|w| ("workspace", w.name.clone()))
        })
    });
    match clash {
        Some((kind, other)) => Err(anyhow!(
            "Can't save \"{name}\": its worktree folder name \"{slug}\" is already used by {kind} \"{other}\". Rename one of them."
        )),
        None => Ok(()),
    }
}

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
    // A folder without `.git` is kept as a folder entry with `is_git: false`,
    // which never defaults to a worktree. A `git init` with no commits yet is
    // `is_git: true`, but the spawn path still skips worktree and branch ops
    // until the folder is a usable git working tree.
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
    ensure_group_slug_free(state, &name, None)?;
    let is_git = git::folder_has_git(&canonical);
    // A plain folder has no branches of its own; asking git there would
    // answer for whatever repo encloses it.
    let default_branch = if is_git {
        git::default_branch(&canonical).await
    } else {
        None
    };
    let entry = RepoEntry {
        id: Uuid::new_v4().to_string(),
        name,
        path: canonical_str,
        default_branch,
        default_use_worktree: is_git,
        is_git,
        appearance: AppearanceOverrides::default(),
        last_agent: None,
        last_spawn_config: None,
    };

    // Looked up again under the write's lock: a concurrent add of the same
    // folder may have registered it while this one awaited git.
    let registered = state.mutate(|s| {
        if let Some(found) = s.repos.iter().find(|r| paths_eq(&r.path, &entry.path)) {
            return found.clone();
        }
        s.repos.push(entry.clone());
        append_to_container_order(s, ContainerRef::Repo(entry.id.clone()));
        entry
    })?;
    Ok(registered)
}

/// Bring every registered repo's `is_git` in line with its folder
/// ([`git::folder_has_git`]): a folder that gained `.git` turns into a repo,
/// one that lost it into a plain folder. A repo whose folder is missing keeps
/// its flag, and `default_use_worktree` is never touched. `state.json` is
/// written only when a flag changed. Returns whether one did.
///
/// The folders are checked with the state unlocked, so a slow or detached
/// drive stalls only this caller. An async caller uses
/// [`refresh_is_git_async`], which checks them on a blocking thread.
pub fn refresh_is_git(state: &AppState) -> bool {
    let judged = judge_is_git(&listed_repos(state));
    apply_is_git(state, &judged)
}

/// [`refresh_is_git`] with the folders checked on a blocking thread.
pub async fn refresh_is_git_async(state: &std::sync::Arc<AppState>) -> bool {
    let listed = listed_repos(state);
    let judged = match tokio::task::spawn_blocking(move || judge_is_git(&listed)).await {
        Ok(judged) => judged,
        Err(err) => {
            tracing::warn!(?err, "checking the registered folders for .git failed");
            return false;
        }
    };
    apply_is_git(state, &judged)
}

/// Each registered repo's id, folder and current flag, copied under the lock.
fn listed_repos(state: &AppState) -> Vec<(String, String, bool)> {
    state.with_persisted(|s| {
        s.repos
            .iter()
            .map(|r| (r.id.clone(), r.path.clone(), r.is_git))
            .collect()
    })
}

/// The repos whose flag differs from their folder, with the new flag. A
/// missing folder is left out, so it keeps its flag. Touches the disk.
fn judge_is_git(listed: &[(String, String, bool)]) -> Vec<(String, bool)> {
    listed
        .iter()
        .filter_map(|(id, path, was_git)| {
            let path = Path::new(path);
            if !path.is_dir() {
                return None;
            }
            let is_git = git::folder_has_git(path);
            (is_git != *was_git).then(|| (id.clone(), is_git))
        })
        .collect()
}

/// Write the `judged` flags, re-reading each repo by id under the lock: one
/// removed meanwhile is skipped. Returns whether a flag changed.
fn apply_is_git(state: &AppState, judged: &[(String, bool)]) -> bool {
    if judged.is_empty() {
        return false;
    }
    match state.mutate(|s| {
        let mut changed = false;
        for (id, is_git) in judged {
            if let Some(repo) = s.repos.iter_mut().find(|r| &r.id == id)
                && repo.is_git != *is_git
            {
                repo.is_git = *is_git;
                changed = true;
            }
        }
        changed
    }) {
        Ok(changed) => {
            if changed {
                tracing::info!(repos = ?judged, "refreshed which registered folders are git repos");
            }
            changed
        }
        Err(err) => {
            tracing::warn!(?err, "saving the refreshed is_git flags failed");
            // The in-memory list changed even though the file did not.
            true
        }
    }
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

pub fn set_repo_appearance(
    state: &AppState,
    repo_id: &str,
    appearance: AppearanceOverrides,
) -> anyhow::Result<()> {
    state.mutate(|s| {
        if let Some(repo) = s.repos.iter_mut().find(|r| r.id == repo_id) {
            repo.appearance = appearance;
        }
    })
}

/// Lazily fill in a repo's `default_branch` when it's still None on the
/// persisted entry (initial detection failed, was added by an older build,
/// etc.). Idempotent — does nothing if the value is already set or if the
/// caller's freshly-detected value is also None. Returns the resolved
/// branch name so callers can use it without re-reading state.
pub fn ensure_default_branch(
    state: &AppState,
    repo_id: &str,
    detected: Option<String>,
) -> Option<String> {
    detected.as_ref()?;
    // Best-effort: the value is re-detected on the next call, so a failed
    // persist isn't worth failing the caller over — but it means state.json
    // could not be written, which is never silently acceptable.
    if let Err(err) = state.mutate(|s| {
        if let Some(repo) = s.repos.iter_mut().find(|r| r.id == repo_id)
            && repo.default_branch.is_none()
        {
            repo.default_branch.clone_from(&detected);
        }
    }) {
        tracing::warn!(?err, %repo_id, "persisting detected default branch failed");
    }
    detected
}

/// Record which agent was last spawned against a given repo. Drives the
/// spawn-dialog default so repeated launches don't force the user to re-pick.
/// Silently no-ops if `repo_id` does not match any registered repo.
pub fn persist_last_agent(state: &AppState, repo_id: &str, agent: &AgentId) -> anyhow::Result<()> {
    state.mutate(|s| {
        if let Some(repo) = s.repos.iter_mut().find(|r| r.id == repo_id) {
            repo.last_agent = Some(agent.clone());
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

pub fn set_workspace_appearance(
    state: &AppState,
    workspace_id: &str,
    appearance: AppearanceOverrides,
) -> anyhow::Result<()> {
    state.mutate(|s| {
        if let Some(ws) = s.workspaces.iter_mut().find(|w| w.id == workspace_id) {
            ws.appearance = appearance;
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
    watch_vscode_workspace: Option<bool>,
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
    ensure_group_slug_free(state, &trimmed_name, Some(id.as_str()))?;
    // Preserve the existing worktree-default, watch flag and last-spawn-config
    // when updating an existing workspace; only first-time upserts start fresh.
    let (prior_default_use_worktree, prior_appearance, prior_last_spawn_config, prior_watch) =
        state.with_persisted(|s| {
            s.workspaces
                .iter()
                .find(|w| w.id == id)
                .map_or((None, None, None, None), |w| {
                    (
                        Some(w.default_use_worktree),
                        Some(w.appearance.clone()),
                        w.last_spawn_config.clone(),
                        Some(w.watch_vscode_workspace),
                    )
                })
        });
    let entry = WorkspaceEntry {
        id: id.clone(),
        name: trimmed_name,
        member_repo_ids,
        linked_vscode_workspace,
        watch_vscode_workspace: watch_vscode_workspace.or(prior_watch).unwrap_or(false),
        default_use_worktree: prior_default_use_worktree.unwrap_or(true),
        appearance: prior_appearance.unwrap_or_default(),
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
            s.workspaces
                .iter()
                .map(|w| w.id.clone())
                .collect::<Vec<_>>(),
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

/// Put the session that resumed `old` — `new` — in the place `old` held in
/// every container's session order, so a resumed session keeps its sidebar
/// slot instead of moving to the end. A container that did not list `old` is
/// left alone, and one that already lists `new` drops `old` rather than
/// naming it twice. Returns the containers whose order changed, with their
/// new order.
pub fn replace_session_in_order(
    state: &AppState,
    old: &str,
    new: &str,
) -> anyhow::Result<Vec<(String, Vec<String>)>> {
    state.mutate(|s| {
        let mut changed = Vec::new();
        for (container_id, ordered) in &mut s.session_order {
            let Some(position) = ordered.iter().position(|id| id == old) else {
                continue;
            };
            if ordered.iter().any(|id| id == new) {
                ordered.remove(position);
            } else {
                new.clone_into(&mut ordered[position]);
            }
            changed.push((container_id.clone(), ordered.clone()));
        }
        changed
    })
}

fn paths_eq(a: &str, b: &str) -> bool {
    let a = a.replace('\\', "/").to_lowercase();
    let b = b.replace('\\', "/").to_lowercase();
    a == b
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect for clearer failures"
)]
mod tests {
    use super::*;
    use crate::paths::Dirs;
    use std::collections::HashMap;

    fn scratch_dirs(tag: &str) -> Dirs {
        let root = std::env::temp_dir().join(format!("rt-registry-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create scratch dir");
        Dirs {
            config: root.clone(),
            state_file: root.join("state.json"),
            handshake_file: root.join("daemon.json"),
            remote_config_file: root.join("remote.json"),
            sessions_dir: root.join("sessions"),
            worktrees_dir: root.join("worktrees"),
            binaries_dir: root.join("binaries"),
            bin_templates: None,
        }
    }

    /// A state whose session order is `orders`, keyed by container id.
    fn state_with_order(tag: &str, orders: &[(&str, &[&str])]) -> AppState {
        let state = AppState::load_or_default(&scratch_dirs(tag)).expect("load state");
        let seeded: HashMap<String, Vec<String>> = orders
            .iter()
            .map(|(container, ids)| {
                let ids = ids.iter().map(|id| (*id).to_owned()).collect();
                ((*container).to_owned(), ids)
            })
            .collect();
        state
            .mutate(|s| s.session_order = seeded)
            .expect("seed the orders");
        state
    }

    /// `container`'s persisted session order.
    fn order_of(state: &AppState, container: &str) -> Option<Vec<String>> {
        state.with_persisted(|s| s.session_order.get(container).cloned())
    }

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|id| (*id).to_owned()).collect()
    }

    #[test]
    fn replace_session_in_order_swaps_in_place() {
        let state = state_with_order("swap", &[("r1", &["a", "lost", "b"])]);

        let changed = replace_session_in_order(&state, "lost", "fresh").expect("swap the id");

        assert_eq!(order_of(&state, "r1"), Some(ids(&["a", "fresh", "b"])));
        assert_eq!(changed, vec![("r1".to_owned(), ids(&["a", "fresh", "b"]))]);
    }

    #[test]
    fn replace_session_in_order_returns_only_changed_containers() {
        let state = state_with_order(
            "changed",
            &[
                ("r1", &["a", "lost"]),
                ("r2", &["x", "y"]),
                ("t1", &["b", "lost"]),
            ],
        );

        let mut changed = replace_session_in_order(&state, "lost", "fresh").expect("swap the id");
        changed.sort_by(|left, right| left.0.cmp(&right.0));

        assert_eq!(
            changed,
            vec![
                ("r1".to_owned(), ids(&["a", "fresh"])),
                ("t1".to_owned(), ids(&["b", "fresh"])),
            ],
            "a container that did not list the old id is not returned"
        );
        assert_eq!(order_of(&state, "r2"), Some(ids(&["x", "y"])));
    }

    #[test]
    fn replace_session_in_order_leaves_state_alone_when_old_is_absent() {
        let state = state_with_order("absent", &[("r1", &["a", "b"])]);

        let changed = replace_session_in_order(&state, "gone", "fresh").expect("nothing to swap");

        assert!(changed.is_empty(), "nothing changed");
        assert_eq!(order_of(&state, "r1"), Some(ids(&["a", "b"])));
    }

    #[tokio::test]
    async fn two_concurrent_adds_of_one_folder_register_it_once() {
        let dirs = scratch_dirs("concurrent-add");
        let folder = dirs.config.join("shared");
        std::fs::create_dir_all(&folder).expect("create the folder");
        let state = AppState::load_or_default(&dirs).expect("load state");
        let path = folder.to_string_lossy().into_owned();

        // Both calls pass the first lookup before either registers: each
        // awaits git between the lookup and the push.
        let (first, second) =
            tokio::join!(add_repo(&state, &path, None), add_repo(&state, &path, None));
        let first = first.expect("the first add succeeds");
        let second = second.expect("the second add succeeds");

        let repos = state.with_persisted(|s| s.repos.clone());
        assert_eq!(repos.len(), 1, "one folder, one entry: {repos:?}");
        assert_eq!(first.id, second.id, "both callers get the one entry");
        assert_eq!(repos.first().map(|repo| &repo.id), Some(&first.id));
    }

    #[tokio::test]
    async fn adding_a_folder_judges_it_by_its_git_entry() {
        let dirs = scratch_dirs("add-is-git");
        let plain = dirs.config.join("plain");
        let git_dir = dirs.config.join("git-dir");
        let git_file = dirs.config.join("git-file");
        std::fs::create_dir_all(&plain).expect("create the plain folder");
        std::fs::create_dir_all(git_dir.join(".git")).expect("create a .git folder");
        std::fs::create_dir_all(&git_file).expect("create the git-file folder");
        std::fs::write(git_file.join(".git"), "gitdir: elsewhere").expect("write a .git file");
        let state = AppState::load_or_default(&dirs).expect("load state");

        let added = add_repo(&state, &plain.to_string_lossy(), None)
            .await
            .expect("a plain folder is added");
        assert!(!added.is_git, "a folder without .git is plain: {added:?}");
        assert!(
            !added.default_use_worktree,
            "a plain folder never defaults to a worktree: {added:?}"
        );

        for folder in [&git_dir, &git_file] {
            let added = add_repo(&state, &folder.to_string_lossy(), None)
                .await
                .expect("a git folder is added");
            assert!(added.is_git, "{} holds .git: {added:?}", folder.display());
            assert!(added.default_use_worktree, "{added:?}");
        }
    }

    /// A registered entry for `path` with the given flags.
    fn flagged(id: &str, path: &Path, is_git: bool, default_use_worktree: bool) -> RepoEntry {
        RepoEntry {
            id: id.to_owned(),
            name: id.to_owned(),
            path: path.to_string_lossy().into_owned(),
            default_branch: None,
            default_use_worktree,
            is_git,
            appearance: AppearanceOverrides::default(),
            last_agent: None,
            last_spawn_config: None,
        }
    }

    /// The `is_git` and `default_use_worktree` flags of repo `id`.
    fn flags_of(state: &AppState, id: &str) -> Option<(bool, bool)> {
        state.with_persisted(|s| {
            s.repos
                .iter()
                .find(|r| r.id == id)
                .map(|r| (r.is_git, r.default_use_worktree))
        })
    }

    #[test]
    fn refresh_is_git_follows_the_folders_git_entry() {
        let dirs = scratch_dirs("refresh-is-git");
        let folder = dirs.config.join("folder");
        let missing = dirs.config.join("missing");
        std::fs::create_dir_all(&folder).expect("create the folder");
        let state = AppState::load_or_default(&dirs).expect("load state");
        state
            .mutate(|s| {
                s.repos.push(flagged("r-folder", &folder, false, true));
                s.repos.push(flagged("r-missing", &missing, true, false));
            })
            .expect("seed the repos");

        // Nothing differs: no change is reported and state.json is not
        // written again.
        std::fs::remove_file(&dirs.state_file).expect("remove state.json");
        assert!(!refresh_is_git(&state), "nothing changed");
        assert!(
            !dirs.state_file.exists(),
            "an unchanged refresh writes nothing"
        );

        std::fs::create_dir_all(folder.join(".git")).expect("git init the folder");
        assert!(refresh_is_git(&state), "the folder became a repo");
        assert_eq!(flags_of(&state, "r-folder"), Some((true, true)));
        assert!(dirs.state_file.exists(), "the change is saved");

        std::fs::remove_dir_all(folder.join(".git")).expect("remove .git");
        assert!(refresh_is_git(&state), "the folder stopped being a repo");
        assert_eq!(
            flags_of(&state, "r-folder"),
            Some((false, true)),
            "default_use_worktree is never touched"
        );
        assert_eq!(
            flags_of(&state, "r-missing"),
            Some((true, false)),
            "a missing folder keeps its flags"
        );
        assert!(
            !refresh_is_git(&state),
            "a second refresh finds nothing new"
        );
    }
}
