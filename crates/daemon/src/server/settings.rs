//! Repos, workspaces and host settings: the registry and settings messages, VS Code workspace scans, and keep-awake.
use super::connection::ConnCtx;
use super::remote::LAN_RETIRED;
use super::{Hub, StateEvent};
use crate::registry::{
    add_repo, remove_repo, remove_workspace, set_repo_worktree_default,
    set_workspace_worktree_default, upsert_workspace,
};
use crate::session::SessionRegistry;
use crate::state::AppState;
use crate::{user_env, vscode};
use anyhow::anyhow;
use protocol::{ClientMessage, DaemonMessage, VscodeWorkspaceSuggestion};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, warn};

/// Handle the repo, workspace and host-settings messages `dispatch` routes here.
#[expect(
    clippy::too_many_lines,
    reason = "one arm per routed message, moved verbatim from dispatch"
)]
pub(super) async fn handle(hub: &Hub, msg: ClientMessage, ctx: &ConnCtx<'_>) -> anyhow::Result<()> {
    let ConnCtx { out_tx, .. } = *ctx;
    match msg {
        ClientMessage::ListRepos => {
            let repos = hub.state.with_persisted(|s| s.repos.clone());
            let _ = out_tx.send(DaemonMessage::Repos { repos });
        }
        ClientMessage::AddRepo { path, name } => {
            register_repo(hub, &path, name, out_tx).await?;
        }
        ClientMessage::RemoveRepo { repo_id } => {
            remove_repo(&hub.state, &repo_id)?;
            let (repos, workspaces, container_order) = hub.state.with_persisted(|s| {
                (
                    s.repos.clone(),
                    s.workspaces.clone(),
                    s.container_order.clone(),
                )
            });
            let _ = hub.state_events.send(StateEvent::Repos(repos));
            let _ = hub.state_events.send(StateEvent::Workspaces(workspaces));
            let _ = hub
                .state_events
                .send(StateEvent::ContainersReordered(container_order));
        }
        ClientMessage::ListWorkspaces => {
            let workspaces = hub.state.with_persisted(|s| s.workspaces.clone());
            let _ = out_tx.send(DaemonMessage::Workspaces { workspaces });
        }
        ClientMessage::UpsertWorkspace {
            id,
            name,
            member_repo_ids,
            linked_vscode_workspace,
        } => {
            upsert_workspace(
                &hub.state,
                id,
                &name,
                member_repo_ids,
                linked_vscode_workspace,
                None,
            )?;
            let (workspaces, container_order) = hub
                .state
                .with_persisted(|s| (s.workspaces.clone(), s.container_order.clone()));
            let _ = hub.state_events.send(StateEvent::Workspaces(workspaces));
            let _ = hub
                .state_events
                .send(StateEvent::ContainersReordered(container_order));
        }
        ClientMessage::RemoveWorkspace { workspace_id } => {
            remove_workspace(&hub.state, &workspace_id)?;
            let (workspaces, container_order) = hub
                .state
                .with_persisted(|s| (s.workspaces.clone(), s.container_order.clone()));
            let _ = hub.state_events.send(StateEvent::Workspaces(workspaces));
            let _ = hub
                .state_events
                .send(StateEvent::ContainersReordered(container_order));
        }
        ClientMessage::AcceptVscodeWorkspaceSuggestion { suggestion, watch } => {
            accept_vscode_suggestion(hub, suggestion, watch, out_tx).await?;
            // Broadcast container order in case new repos were registered.
            let container_order = hub.state.with_persisted(|s| s.container_order.clone());
            let _ = hub
                .state_events
                .send(StateEvent::ContainersReordered(container_order));
        }
        ClientMessage::ParseVscodeWorkspace { path } => {
            let known_repos: Vec<(String, String)> = hub.state.with_persisted(|s| {
                s.repos
                    .iter()
                    .map(|r| (r.id.clone(), r.path.clone()))
                    .collect()
            });
            let suggestion =
                crate::vscode::parse_workspace_file(std::path::Path::new(&path), &known_repos)
                    .map_err(|e| anyhow!("parse_vscode_workspace: {e}"))?;
            let _ = out_tx.send(DaemonMessage::VscodeWorkspaceParsed { suggestion });
        }
        ClientMessage::ScanVscodeWorkspaces { path } => {
            let suggestions = scan_vscode_workspaces_in_path(hub, &path);
            let _ = out_tx.send(DaemonMessage::VscodeWorkspacesScanned { path, suggestions });
        }
        ClientMessage::SetRepoWorktreeDefault { repo_id, value } => {
            set_repo_worktree_default(&hub.state, &repo_id, value)?;
            let repos = hub.state.with_persisted(|s| s.repos.clone());
            let _ = hub.state_events.send(StateEvent::Repos(repos));
        }
        ClientMessage::SetWorkspaceWorktreeDefault {
            workspace_id,
            value,
        } => {
            set_workspace_worktree_default(&hub.state, &workspace_id, value)?;
            let workspaces = hub.state.with_persisted(|s| s.workspaces.clone());
            let _ = hub.state_events.send(StateEvent::Workspaces(workspaces));
        }
        ClientMessage::SetWorktreesRoot { path } => {
            let (resolved, is_override) = hub.state.set_worktrees_root(path)?;
            let _ = hub.state_events.send(StateEvent::WorktreesRootChanged {
                root: resolved.to_string_lossy().into_owned(),
                is_override,
            });
        }
        ClientMessage::ConfigureLan { .. }
        | ClientMessage::StartPairing
        | ClientMessage::CancelPairing => {
            info!("dispatch: a retired LAN access message was answered with an Error");
            let _ = out_tx.send(DaemonMessage::Error {
                message: LAN_RETIRED.to_owned(),
                request_id: None,
            });
        }
        ClientMessage::SetKeepAwake { enabled } => {
            hub.state.set_keep_awake(enabled)?;
            hub.keep_awake_enabled.send_replace(enabled);
            info!(enabled, "keep-awake setting changed");
        }
        ClientMessage::InspectWorktreesRoot => {
            let root = hub.state.worktrees_dir();
            let is_override = hub.state.worktrees_root_is_override();
            let entries = crate::worktrees_admin::scan_root(&root, &hub.sessions, &hub.state);
            let _ = out_tx.send(DaemonMessage::WorktreesRootSnapshot {
                root: root.to_string_lossy().into_owned(),
                is_override,
                entries,
            });
        }
        ClientMessage::DeleteWorktreeAt { path } => {
            let root = hub.state.worktrees_dir();
            let target = std::path::PathBuf::from(&path);
            crate::worktrees_admin::delete_group(&root, &target, &hub.sessions).await?;
            // Push a fresh snapshot only to the requesting client. Other
            // windows with the modal open can refresh on demand.
            let is_override = hub.state.worktrees_root_is_override();
            let entries = crate::worktrees_admin::scan_root(&root, &hub.sessions, &hub.state);
            let _ = out_tx.send(DaemonMessage::WorktreesRootSnapshot {
                root: root.to_string_lossy().into_owned(),
                is_override,
                entries,
            });
        }
        _ => warn!("settings::handle: dispatch routed a message this module does not handle"),
    }
    Ok(())
}

/// Wire up the keep-awake watcher: seed its setting watch from persisted
/// state, spawn the task, and hand back the two ends the [`Hub`] keeps — the
/// setting sender (written by [`ClientMessage::SetKeepAwake`]) and the status
/// receiver (read by the initial-state push).
pub(super) fn start_keep_awake(
    state: &AppState,
    sessions: &Arc<SessionRegistry>,
    state_events: &broadcast::Sender<StateEvent>,
) -> (
    tokio::sync::watch::Sender<bool>,
    tokio::sync::watch::Receiver<crate::keep_awake::Status>,
) {
    let enabled = state.keep_awake();
    let (enabled_tx, enabled_rx) = tokio::sync::watch::channel(enabled);
    let (status_tx, status_rx) = tokio::sync::watch::channel(crate::keep_awake::Status {
        enabled,
        active: false,
    });
    crate::keep_awake::spawn(
        Arc::clone(sessions),
        enabled_rx,
        status_tx,
        state_events.clone(),
        crate::keep_awake::native(),
    );
    (enabled_tx, status_rx)
}

/// Register the folder at `path` as a repo (or find it already registered),
/// broadcast the new registry, and offer the requester any VS Code workspace
/// found beside it.
pub(super) async fn register_repo(
    hub: &Hub,
    path: &str,
    name: Option<String>,
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
) -> anyhow::Result<protocol::RepoEntry> {
    let entry = add_repo(&hub.state, path, name).await?;
    let (repos, container_order) = hub
        .state
        .with_persisted(|s| (s.repos.clone(), s.container_order.clone()));
    let _ = hub.state_events.send(StateEvent::Repos(repos));
    let _ = hub
        .state_events
        .send(StateEvent::ContainersReordered(container_order));
    for suggestion in scan_vscode_workspaces(hub, &entry.path) {
        // VSCode workspace suggestions are per-client (only the
        // adder gets prompted), so this stays on out_tx.
        let _ = out_tx.send(DaemonMessage::VscodeWorkspaceSuggestion {
            repo_id: entry.id.clone(),
            suggestion,
        });
    }
    debug!(repo_id = %entry.id, "repo added");
    Ok(entry)
}

/// The names of the variables an `${env:NAME}` reference can read, never
/// their values, listed off the async runtime because it reads the registry.
pub(super) async fn env_names() -> Vec<protocol::EnvName> {
    tokio::task::spawn_blocking(user_env::names)
        .await
        .unwrap_or_else(|err| {
            warn!(?err, "listing the environment variable names failed");
            Vec::new()
        })
}

fn scan_vscode_workspaces(hub: &Hub, repo_path: &str) -> Vec<VscodeWorkspaceSuggestion> {
    scan_vscode_workspace_files(hub, repo_path, true)
}

fn scan_vscode_workspaces_in_path(hub: &Hub, path: &str) -> Vec<VscodeWorkspaceSuggestion> {
    scan_vscode_workspace_files(hub, path, false)
}

fn scan_vscode_workspace_files(
    hub: &Hub,
    dir_path: &str,
    require_multiple_folders: bool,
) -> Vec<VscodeWorkspaceSuggestion> {
    let known: Vec<(String, String)> = hub.state.with_persisted(|s| {
        s.repos
            .iter()
            .map(|r| (r.id.clone(), r.path.clone()))
            .collect()
    });
    let dir = std::path::Path::new(dir_path);
    vscode::find_workspace_files(dir)
        .into_iter()
        .filter_map(|file| match vscode::parse_workspace_file(&file, &known) {
            Ok(s) if !require_multiple_folders || s.folders.len() > 1 => Some(s),
            Ok(_) => None,
            Err(err) => {
                warn!(?err, file = %file.display(), "failed to parse code-workspace");
                None
            }
        })
        .collect()
}

async fn accept_vscode_suggestion(
    hub: &Hub,
    suggestion: VscodeWorkspaceSuggestion,
    watch: bool,
    _out_tx: &mpsc::UnboundedSender<DaemonMessage>,
) -> anyhow::Result<()> {
    let mut member_repo_ids = Vec::with_capacity(suggestion.folders.len());
    for folder in &suggestion.folders {
        let repo_id = if let Some(id) = folder.matched_repo_id.clone() {
            id
        } else {
            let entry = add_repo(&hub.state, &folder.path, folder.name.clone()).await?;
            entry.id
        };
        member_repo_ids.push(repo_id);
    }
    upsert_workspace(
        &hub.state,
        None,
        &suggestion.suggested_name,
        member_repo_ids,
        Some(suggestion.source_path.clone()),
        Some(watch),
    )?;
    let (repos, workspaces) = hub
        .state
        .with_persisted(|s| (s.repos.clone(), s.workspaces.clone()));
    let _ = hub.state_events.send(StateEvent::Repos(repos));
    let _ = hub.state_events.send(StateEvent::Workspaces(workspaces));
    Ok(())
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use crate::server::test_support::{dispatch_err, repo_entry, test_hub, workspace_entry};
    use std::path::Path;
    #[tokio::test]
    async fn accepting_a_vscode_suggestion_stores_whether_it_watches() {
        let (hub, _scratch) = test_hub("vscode-watch-flag");
        hub.state
            .mutate(|s| s.repos.push(repo_entry("r1", "r1", Path::new("C:/r1"))))
            .expect("register the repo");
        let suggestion = |name: &str| protocol::VscodeWorkspaceSuggestion {
            source_path: format!("C:/work/{name}.code-workspace"),
            suggested_name: name.to_string(),
            folders: vec![protocol::VscodeWorkspaceFolder {
                path: "C:/r1".to_string(),
                name: None,
                matched_repo_id: Some("r1".to_string()),
            }],
        };

        assert_eq!(
            dispatch_err(
                &hub,
                ClientMessage::AcceptVscodeWorkspaceSuggestion {
                    suggestion: suggestion("watched"),
                    watch: true,
                },
            )
            .await,
            None
        );
        assert_eq!(
            dispatch_err(
                &hub,
                ClientMessage::AcceptVscodeWorkspaceSuggestion {
                    suggestion: suggestion("plain"),
                    watch: false,
                },
            )
            .await,
            None
        );

        let (watched, plain) = hub.state.with_persisted(|s| {
            let flag = |name: &str| {
                s.workspaces
                    .iter()
                    .find(|workspace| workspace.name == name)
                    .map(|workspace| workspace.watch_vscode_workspace)
            };
            (flag("watched"), flag("plain"))
        });
        assert_eq!(watched, Some(true), "Create & watch records the flag");
        assert_eq!(plain, Some(false), "Create records that it does not watch");
    }

    #[tokio::test]
    async fn add_repo_refuses_a_name_whose_slug_is_taken_by_a_workspace() {
        let (hub, scratch) = test_hub("slug-repo");
        hub.state
            .mutate(|s| s.workspaces.push(workspace_entry("w1", "shop", &[])))
            .expect("register workspace");
        let dir = scratch.path().join("shop-repo");
        std::fs::create_dir_all(&dir).expect("create repo dir");

        let err = dispatch_err(
            &hub,
            ClientMessage::AddRepo {
                path: dir.to_string_lossy().into_owned(),
                name: Some("Shop!".to_string()),
            },
        )
        .await;

        assert_eq!(
            err.as_deref(),
            Some(
                "Can't save \"Shop!\": its worktree folder name \"shop\" is already used by workspace \"shop\". Rename one of them."
            )
        );
        assert!(hub.state.with_persisted(|s| s.repos.is_empty()));
    }

    #[tokio::test]
    async fn upsert_workspace_refuses_a_slug_taken_by_a_repo() {
        let (hub, scratch) = test_hub("slug-ws");
        let dir = scratch.path().join("app");
        hub.state
            .mutate(|s| s.repos.push(repo_entry("r1", "My App", &dir)))
            .expect("register repo");

        let err = dispatch_err(
            &hub,
            ClientMessage::UpsertWorkspace {
                id: None,
                name: "my app".to_string(),
                member_repo_ids: vec!["r1".to_string()],
                linked_vscode_workspace: None,
            },
        )
        .await;

        assert_eq!(
            err.as_deref(),
            Some(
                "Can't save \"my app\": its worktree folder name \"my-app\" is already used by repo \"My App\". Rename one of them."
            )
        );
        assert!(hub.state.with_persisted(|s| s.workspaces.is_empty()));
    }

    #[tokio::test]
    async fn renaming_a_workspace_to_its_own_slug_is_allowed() {
        let (hub, _scratch) = test_hub("slug-rename");
        hub.state
            .mutate(|s| s.workspaces.push(workspace_entry("w1", "Shop", &[])))
            .expect("register workspace");

        let err = dispatch_err(
            &hub,
            ClientMessage::UpsertWorkspace {
                id: Some("w1".to_string()),
                name: "shop!".to_string(),
                member_repo_ids: Vec::new(),
                linked_vscode_workspace: None,
            },
        )
        .await;

        assert_eq!(err, None);
        let names: Vec<String> = hub
            .state
            .with_persisted(|s| s.workspaces.iter().map(|w| w.name.clone()).collect());
        assert_eq!(names, vec!["shop!".to_string()]);
    }
}
