//! Repos, workspaces and host settings: the registry and settings messages, VS Code workspace scans, keep-awake, and the agents' launch commands.
use super::connection::ConnCtx;
use super::remote::LAN_RETIRED;
use super::{Hub, StateEvent};
use crate::registry::{
    add_repo, remove_repo, remove_workspace, set_repo_worktree_default,
    set_workspace_worktree_default, upsert_workspace,
};
use crate::session::SessionRegistry;
use crate::state::{AgentCommand, AppState};
use crate::{user_env, vscode};
use anyhow::{anyhow, bail};
use protocol::{
    AgentCommandEntry, AgentId, ClientMessage, DaemonMessage, KeepAwakeMode, OptionValues,
    VscodeWorkspaceSuggestion,
};
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
            let mode = if enabled {
                KeepAwakeMode::Always
            } else {
                KeepAwakeMode::Off
            };
            set_keep_awake_mode(hub, mode)?;
        }
        ClientMessage::SetKeepAwakeMode { mode } => set_keep_awake_mode(hub, mode)?,
        ClientMessage::SetAgentCommand {
            agent,
            program,
            args,
            request_id,
        } => answer_refusal(
            out_tx,
            set_agent_command(hub, &agent, AgentCommand { program, args }),
            request_id,
        )?,
        ClientMessage::SetAgentSettings {
            agent,
            options,
            request_id,
        } => answer_refusal(
            out_tx,
            set_agent_settings(hub, &agent, &options),
            request_id,
        )?,
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

/// Persist `mode` and hand it to the keep-awake watcher, which re-derives the
/// hold and broadcasts the status. A mode this daemon does not know is
/// refused: logged, and nothing changes.
fn set_keep_awake_mode(hub: &Hub, mode: KeepAwakeMode) -> anyhow::Result<()> {
    if mode == KeepAwakeMode::Unknown {
        warn!("keep-awake: refused a mode this daemon does not know; the setting is unchanged");
        return Ok(());
    }
    hub.state.set_keep_awake_mode(mode)?;
    hub.keep_awake_mode.send_replace(mode);
    info!(?mode, "keep-awake mode changed");
    Ok(())
}

/// A Settings request's outcome: a refusal of a request that carried a
/// `request_id` is answered here with an `Error` echoing it, to this
/// requester only; any other outcome is returned as it came, so a refusal
/// without one still gets the connection's `Error { request_id: None }`.
fn answer_refusal(
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
    outcome: anyhow::Result<()>,
    request_id: Option<String>,
) -> anyhow::Result<()> {
    match (outcome, request_id) {
        (Err(err), Some(request_id)) => {
            warn!(?err, %request_id, "settings request refused");
            let _ = out_tx.send(DaemonMessage::Error {
                message: err.to_string(),
                request_id: Some(request_id),
            });
            Ok(())
        }
        (outcome, _) => outcome,
    }
}

/// Check `command` against `agent`'s reserved flags, persist it (a blank pair
/// removes the agent's entry) and broadcast every agent's command.
fn set_agent_command(hub: &Hub, agent: &AgentId, command: AgentCommand) -> anyhow::Result<()> {
    let plugin = agent_registry::plugin(agent)
        .ok_or_else(|| anyhow!("no agent plugin `{}` is registered", agent.0))?;
    check_agent_command(plugin.reserved_args(), &command)?;
    hub.state.set_agent_command(agent, command)?;
    info!(agent = agent.0.as_str(), "agent launch command changed");
    let commands = agent_command_entries(&hub.state, &|name| std::env::var(name).ok());
    let _ = hub.state_events.send(StateEvent::AgentCommands(commands));
    Ok(())
}

/// Validate `options` against `agent`'s option schema, persist them as the
/// agent's settings and broadcast every agent's settings. An unknown agent or
/// a value the schema refuses answers an error and changes nothing.
fn set_agent_settings(hub: &Hub, agent: &AgentId, options: &OptionValues) -> anyhow::Result<()> {
    let plugin = agent_registry::plugin(agent)
        .ok_or_else(|| anyhow!("This daemon has no agent `{}`.", agent.0))?;
    let options = agent_api::validate(plugin.descriptor(), options)
        .map_err(|err| anyhow!("{}", err.message))?;
    hub.state.set_agent_settings(agent, options)?;
    info!(agent = agent.0.as_str(), "agent settings changed");
    let _ = hub
        .state_events
        .send(StateEvent::AgentSettings(hub.state.agent_settings()));
    Ok(())
}

/// Refuse a launch command a field of which holds a control character, whose
/// extra arguments do not split, or one of whose extra arguments names a flag
/// in `reserved`: the flag itself, or a long flag followed by `=`.
fn check_agent_command(reserved: &[&str], command: &AgentCommand) -> anyhow::Result<()> {
    for (field, text) in [
        ("Program", &command.program),
        ("Extra arguments", &command.args),
    ] {
        if text.chars().any(char::is_control) {
            bail!(
                "The launch command's {field} cannot contain a newline or other control character."
            );
        }
    }
    let tokens = shell_words::split(&command.args)
        .map_err(|_| anyhow!("The extra arguments have an unclosed quote."))?;
    for token in &tokens {
        for flag in reserved {
            let with_value = flag.starts_with("--")
                && token
                    .strip_prefix(flag)
                    .is_some_and(|rest| rest.starts_with('='));
            if token == flag || with_value {
                bail!("{flag} is set by rustling-tulip; remove it from the extra arguments.");
            }
        }
    }
    Ok(())
}

/// Every registered agent's launch command, blank when unset, each naming
/// the environment variable that overrides its program when `env` has it.
pub(super) fn agent_command_entries(
    state: &AppState,
    env: &dyn Fn(&str) -> Option<String>,
) -> Vec<AgentCommandEntry> {
    agent_registry::all()
        .iter()
        .map(|plugin| {
            let agent = plugin.descriptor().id.clone();
            let command = state.agent_command(&agent);
            let variable = plugin.program_env_var();
            AgentCommandEntry {
                agent,
                program: command.program,
                args: command.args,
                program_env: env(variable).map(|_| variable.to_owned()),
            }
        })
        .collect()
}

/// Wire up the keep-awake watcher: seed its mode watch from persisted state,
/// spawn the task, and hand back the two ends the [`Hub`] keeps — the mode
/// sender (written by [`ClientMessage::SetKeepAwakeMode`]) and the status
/// receiver (read by the initial-state push).
pub(super) fn start_keep_awake(
    state: &AppState,
    sessions: &Arc<SessionRegistry>,
    state_events: &broadcast::Sender<StateEvent>,
) -> (
    tokio::sync::watch::Sender<KeepAwakeMode>,
    tokio::sync::watch::Receiver<crate::keep_awake::Status>,
) {
    let mode = state.keep_awake_mode();
    let (mode_tx, mode_rx) = tokio::sync::watch::channel(mode);
    let (status_tx, status_rx) = tokio::sync::watch::channel(crate::keep_awake::Status {
        mode,
        active: false,
    });
    crate::keep_awake::spawn(
        Arc::clone(sessions),
        mode_rx,
        status_tx,
        state_events.clone(),
        crate::keep_awake::native(),
    );
    (mode_tx, status_rx)
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
    use crate::server::test_support::{
        dispatch_err, dispatch_one, repo_entry, test_hub, workspace_entry,
    };
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

    /// Leaves the host's power state alone.
    struct NoHold;

    impl crate::keep_awake::Inhibitor for NoHold {
        fn set(&mut self, _hold: bool) {}
    }

    /// The mode of the next keep-awake status on the state broadcast.
    async fn next_keep_awake_mode(rx: &mut broadcast::Receiver<StateEvent>) -> KeepAwakeMode {
        loop {
            let evt = rx.recv().await.expect("the state broadcast stays open");
            if let StateEvent::KeepAwakeStatus { mode, .. } = evt {
                return mode;
            }
        }
    }

    #[tokio::test]
    async fn set_keep_awake_mode_persists_and_broadcasts_the_mode() {
        let (hub, _scratch) = test_hub("keep-awake-mode");
        let mut events = hub.state_events.subscribe();
        let (status_tx, _status_rx) = tokio::sync::watch::channel(crate::keep_awake::Status {
            mode: hub.state.keep_awake_mode(),
            active: false,
        });
        crate::keep_awake::spawn(
            Arc::clone(&hub.sessions),
            hub.keep_awake_mode.subscribe(),
            status_tx,
            hub.state_events.clone(),
            Box::new(NoHold),
        );
        let wait = std::time::Duration::from_secs(5);
        let first = tokio::time::timeout(wait, next_keep_awake_mode(&mut events))
            .await
            .expect("the watcher publishes its first status");
        assert_eq!(first, KeepAwakeMode::WhenBusy, "the persisted default");

        let sent = dispatch_err(
            &hub,
            ClientMessage::SetKeepAwakeMode {
                mode: KeepAwakeMode::Always,
            },
        )
        .await;
        assert_eq!(sent, None, "the change is accepted");
        assert_eq!(hub.state.keep_awake_mode(), KeepAwakeMode::Always);
        let reloaded = AppState::load_or_default(&hub.dirs).expect("reload state");
        assert_eq!(
            reloaded.keep_awake_mode(),
            KeepAwakeMode::Always,
            "the mode reached state.json"
        );
        let broadcast = tokio::time::timeout(wait, next_keep_awake_mode(&mut events))
            .await
            .expect("the watcher broadcasts the new mode");
        assert_eq!(broadcast, KeepAwakeMode::Always);
    }

    fn set_command(program: &str, args: &str) -> ClientMessage {
        ClientMessage::SetAgentCommand {
            agent: agent_registry::default_agent(),
            program: program.to_owned(),
            args: args.to_owned(),
            request_id: None,
        }
    }

    /// Dispatch `msg`, expect it refused with `want`, and check nothing was
    /// stored.
    async fn assert_agent_command_refused(hub: &Hub, msg: ClientMessage, want: &str) {
        let err = dispatch_err(hub, msg).await;
        assert_eq!(err.as_deref(), Some(want));
        assert!(
            hub.state.with_persisted(|s| s.agent_commands.is_empty()),
            "a refused command is not stored"
        );
    }

    #[tokio::test]
    async fn set_agent_command_refuses_control_characters_in_either_field() {
        let (hub, _scratch) = test_hub("agent-command-control");
        for bad in ["claude\nrm", "claude\r", "cl\u{7}aude", "a\tb"] {
            assert_agent_command_refused(
                &hub,
                set_command(bad, ""),
                "The launch command's Program cannot contain a newline or other control character.",
            )
            .await;
            assert_agent_command_refused(
                &hub,
                set_command("", bad),
                "The launch command's Extra arguments cannot contain a newline or other control character.",
            )
            .await;
        }
    }

    #[tokio::test]
    async fn set_agent_command_refuses_an_unclosed_quote() {
        let (hub, _scratch) = test_hub("agent-command-quote");
        for bad in ["--debug \"a b", "'x", "--name \"it's\" 'y"] {
            assert_agent_command_refused(
                &hub,
                set_command("", bad),
                "The extra arguments have an unclosed quote.",
            )
            .await;
        }
    }

    #[tokio::test]
    async fn set_agent_command_refuses_every_reserved_flag() {
        let (hub, _scratch) = test_hub("agent-command-reserved");
        let reserved = agent_registry::plugin(&agent_registry::default_agent())
            .expect("Claude is registered")
            .reserved_args();
        assert!(!reserved.is_empty(), "Claude reserves its own flags");
        for flag in reserved {
            let want =
                format!("{flag} is set by rustling-tulip; remove it from the extra arguments.");
            assert_agent_command_refused(
                &hub,
                set_command("", &format!("--debug {flag} x")),
                &want,
            )
            .await;
            assert_agent_command_refused(&hub, set_command("", &format!("'{flag}'")), &want).await;
            if flag.starts_with("--") {
                assert_agent_command_refused(
                    &hub,
                    set_command("", &format!("{flag}=value")),
                    &want,
                )
                .await;
            }
        }
        assert_eq!(
            dispatch_err(&hub, set_command("", "--debug --models=x -pr")).await,
            None,
            "a flag that only starts like a reserved one is allowed"
        );
    }

    /// The next agent-commands broadcast on the state stream.
    async fn next_agent_commands(
        rx: &mut broadcast::Receiver<StateEvent>,
    ) -> Vec<protocol::AgentCommandEntry> {
        loop {
            let evt = rx.recv().await.expect("the state broadcast stays open");
            if let StateEvent::AgentCommands(commands) = evt {
                return commands;
            }
        }
    }

    /// The next agent settings broadcast on `rx`.
    async fn next_agent_settings(
        rx: &mut tokio::sync::broadcast::Receiver<StateEvent>,
    ) -> std::collections::BTreeMap<protocol::AgentId, protocol::OptionValues> {
        loop {
            let evt = rx.recv().await.expect("the state broadcast stays open");
            if let StateEvent::AgentSettings(settings) = evt {
                return settings;
            }
        }
    }

    fn set_settings(agent: &str, options: &[(&str, protocol::OptionValue)]) -> ClientMessage {
        ClientMessage::SetAgentSettings {
            agent: protocol::AgentId(agent.to_owned()),
            options: options
                .iter()
                .map(|(key, value)| ((*key).to_owned(), value.clone()))
                .collect(),
            request_id: None,
        }
    }

    fn text(value: &str) -> protocol::OptionValue {
        protocol::OptionValue::Text(value.to_owned())
    }

    #[tokio::test]
    async fn refused_agent_settings_echo_the_request_id() {
        let (hub, _scratch) = test_hub("agent-settings-request-id");
        let claude = agent_registry::default_agent();
        let reply = dispatch_one(
            &hub,
            ClientMessage::SetAgentSettings {
                agent: claude.clone(),
                options: protocol::OptionValues::from([("x".to_owned(), text("1"))]),
                request_id: Some("s1".to_owned()),
            },
        )
        .await;
        assert!(
            matches!(
                &reply,
                DaemonMessage::Error { message, request_id: Some(id) }
                    if message == "Claude has no option `x`" && id == "s1"
            ),
            "the refusal answers the request it refuses: {reply:?}"
        );
        assert!(hub.state.agent_settings().is_empty(), "nothing was stored");
        assert_eq!(
            dispatch_err(&hub, set_settings(&claude.0, &[("x", text("1"))]))
                .await
                .as_deref(),
            Some("Claude has no option `x`"),
            "a refusal without a request id still fails as before"
        );
    }

    #[tokio::test]
    async fn refused_agent_command_echoes_the_request_id() {
        let (hub, _scratch) = test_hub("agent-command-request-id");
        let reply = dispatch_one(
            &hub,
            ClientMessage::SetAgentCommand {
                agent: agent_registry::default_agent(),
                program: String::new(),
                args: "'x".to_owned(),
                request_id: Some("c1".to_owned()),
            },
        )
        .await;
        assert!(
            matches!(
                &reply,
                DaemonMessage::Error { message, request_id: Some(id) }
                    if message == "The extra arguments have an unclosed quote." && id == "c1"
            ),
            "the refusal answers the request it refuses: {reply:?}"
        );
        assert!(
            hub.state.with_persisted(|s| s.agent_commands.is_empty()),
            "a refused command is not stored"
        );
    }

    #[tokio::test]
    async fn set_agent_settings_validates_persists_and_broadcasts() {
        let (hub, _scratch) = test_hub("agent-settings-save");
        let mut events = hub.state_events.subscribe();
        let wait = std::time::Duration::from_secs(5);
        let claude = agent_registry::default_agent();

        let sent = dispatch_err(
            &hub,
            set_settings(&claude.0, &[("provider", text("deepseek"))]),
        )
        .await;
        assert_eq!(sent, None, "valid settings are accepted");
        let validated = protocol::OptionValues::from([
            ("permission_mode".to_owned(), protocol::OptionValue::Null),
            ("provider".to_owned(), text("deepseek")),
            ("model".to_owned(), protocol::OptionValue::Null),
        ]);
        let expected = std::collections::BTreeMap::from([(claude.clone(), validated)]);
        let reloaded = AppState::load_or_default(&hub.dirs).expect("reload state");
        assert_eq!(
            reloaded.agent_settings(),
            expected,
            "the validated settings reached state.json"
        );
        let broadcast = tokio::time::timeout(wait, next_agent_settings(&mut events))
            .await
            .expect("the change is broadcast");
        assert_eq!(broadcast, expected);
    }

    #[tokio::test]
    async fn set_agent_settings_refuses_an_unknown_agent_and_changes_nothing() {
        let (hub, _scratch) = test_hub("agent-settings-refused");
        let claude = agent_registry::default_agent();
        let sent = dispatch_err(&hub, set_settings("nope", &[])).await;
        assert_eq!(sent.as_deref(), Some("This daemon has no agent `nope`."));
        let sent = dispatch_err(&hub, set_settings(&claude.0, &[("x", text("1"))])).await;
        assert_eq!(
            sent.as_deref(),
            Some("Claude has no option `x`"),
            "an unknown option is refused naming it"
        );
        assert!(hub.state.agent_settings().is_empty(), "nothing was stored");
        let reloaded = AppState::load_or_default(&hub.dirs).expect("reload state");
        assert!(
            reloaded.agent_settings().is_empty(),
            "nothing reached state.json"
        );
    }

    #[tokio::test]
    async fn set_agent_command_persists_broadcasts_and_a_blank_pair_clears_it() {
        let (hub, _scratch) = test_hub("agent-command-save");
        let mut events = hub.state_events.subscribe();
        let wait = std::time::Duration::from_secs(5);

        let sent = dispatch_err(&hub, set_command("claude-wrapper", "--debug \"a b\"")).await;
        assert_eq!(sent, None, "a valid command is accepted");
        let reloaded = AppState::load_or_default(&hub.dirs).expect("reload state");
        assert_eq!(
            reloaded.agent_command(&agent_registry::default_agent()),
            crate::state::AgentCommand {
                program: "claude-wrapper".to_owned(),
                args: "--debug \"a b\"".to_owned(),
            },
            "the command reached state.json"
        );
        let broadcast = tokio::time::timeout(wait, next_agent_commands(&mut events))
            .await
            .expect("the change is broadcast");
        assert_eq!(broadcast.len(), 1, "one entry per registered agent");
        let entry = &broadcast[0];
        assert_eq!(entry.agent, agent_registry::default_agent());
        assert_eq!(entry.program, "claude-wrapper");
        assert_eq!(entry.args, "--debug \"a b\"");

        let sent = dispatch_err(&hub, set_command(" ", "")).await;
        assert_eq!(sent, None, "a blank pair is accepted");
        assert!(
            hub.state.with_persisted(|s| s.agent_commands.is_empty()),
            "a blank pair removes the entry"
        );
        let broadcast = tokio::time::timeout(wait, next_agent_commands(&mut events))
            .await
            .expect("the clear is broadcast");
        assert!(
            broadcast
                .iter()
                .all(|entry| entry.program.is_empty() && entry.args.is_empty()),
            "{broadcast:?}"
        );
    }

    #[test]
    fn agent_command_entries_name_the_overriding_variable() {
        let (hub, _scratch) = test_hub("agent-command-env");
        let unset = agent_command_entries(&hub.state, &|_| None);
        assert!(
            unset.iter().all(|entry| entry.program_env.is_none()),
            "{unset:?}"
        );
        assert_eq!(
            unset.len(),
            agent_registry::all().len(),
            "every agent is listed"
        );

        let set = agent_command_entries(&hub.state, &|name| {
            (name == "RUSTLING_TULIP_CLAUDE").then(|| r"C:\fake\claude.exe".to_owned())
        });
        let claude = set
            .iter()
            .find(|entry| entry.agent == agent_registry::default_agent())
            .expect("Claude is listed");
        assert_eq!(claude.program_env.as_deref(), Some("RUSTLING_TULIP_CLAUDE"));
    }

    #[tokio::test]
    async fn legacy_set_keep_awake_maps_onto_the_modes() {
        let (hub, _scratch) = test_hub("keep-awake-legacy");
        for (enabled, want) in [(false, KeepAwakeMode::Off), (true, KeepAwakeMode::Always)] {
            let sent = dispatch_err(&hub, ClientMessage::SetKeepAwake { enabled }).await;
            assert_eq!(
                sent, None,
                "SetKeepAwake {{ enabled: {enabled} }} is accepted"
            );
            assert_eq!(hub.state.keep_awake_mode(), want, "persisted for {enabled}");
            assert_eq!(
                *hub.keep_awake_mode.borrow(),
                want,
                "published for {enabled}"
            );
        }
    }
}
