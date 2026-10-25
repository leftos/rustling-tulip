//! Spawning sessions: request checks, spawn previews, single, workspace and standalone spawns, and the interactive, plain-shell and headless launches.

mod hook_settings;
mod shell;
mod worktree;
#[cfg(test)]
pub(super) use self::hook_settings::hook_settings_path;
pub(super) use self::hook_settings::{announce_replayed_prompt, remove_hook_settings};
pub(super) use self::worktree::target_uses_worktree;

use self::hook_settings::{HookSettings, write_hook_files, write_hook_settings};
use self::shell::{
    AgentLaunch, Launch, ShellProgram, TypedCommand, choose_agent_command, plan_launch,
    resolve_agent_program, resolve_shell_program,
};
use self::worktree::{
    SinglePlan, classify_worktree_error, log_fork_point, materialize_single_worktree,
    pinned_branch, preview_single_spawn, reject_pin_without_worktree, single_worktree_target,
    worktree_paths_for_config,
};
use super::connection::ConnCtx;
use super::git::{active_sessions_using_worktree, worktree_in_use_failure};
use super::sessions::{request_origin, write_meta_then_sync};
use super::{Hub, StateEvent};
use crate::conversations;
use crate::conversations::discovery::{self, Baseline, Waiter};
use crate::conversations::pre_create::{self, PreCreateSpec};
use crate::orphan::OrphanMeta;
use crate::pty::PtySpawnSpec;
use crate::registry::{
    persist_last_agent, persist_repo_last_spawn_config, persist_workspace_last_spawn_config,
};
use crate::screen::rules::rules_for;
use crate::session::SessionRegistry;
use crate::session::{
    SessionRecord, UpdateOrigin, attach_lifecycle, default_agent, new_id, push_recent_action,
};
use crate::{
    env_secrets, headless, inject, orphan, osc_title, pty_state, screen, spawn_plan, tracer_client,
    user_env, workspace as ws,
};
use agent_api::{
    AgentPlugin, ConversationPlan, EnvPlan, KeyLookup, ResolvedRows, SpawnCheck, SpawnContext,
};
use anyhow::{Context as _, anyhow};
use chrono::Utc;
use directories::UserDirs;
use protocol::{
    AgentId, AppearanceOverrides, ClientMessage, DaemonMessage, OptionValues, SessionKind,
    SessionMember, SessionMetrics, SessionMode, SessionStatus, SpawnRequest, SpawnTarget,
};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Handle the spawn and spawn-preview messages `dispatch` routes here.
pub(super) async fn handle(hub: &Hub, msg: ClientMessage, ctx: &ConnCtx<'_>) -> anyhow::Result<()> {
    let ConnCtx {
        out_tx, connection, ..
    } = *ctx;
    match msg {
        ClientMessage::PreviewWorkspaceSpawn {
            workspace_id,
            branch_name,
            base_branch,
            request_id,
        } => {
            // Preview always reflects the worktree path; the in-place
            // affordance is hidden behind the use_worktree spawn flag and
            // doesn't change preview output meaningfully.
            let resolved = ws::resolve_workspace(
                &hub.state,
                &hub.state.worktrees_dir(),
                &workspace_id,
                ws::ResolveRequest {
                    branch_name: &branch_name,
                    explicit_base: base_branch.as_deref(),
                    use_worktree: true,
                    // Preview answers "where would a fresh spawn land"; a pin
                    // has no fork point to preview.
                    pins: &[],
                },
            )
            .await;
            match resolved {
                Ok((_, resolved)) => {
                    let per_member = ws::previews(&resolved, &branch_name).await;
                    let _ = out_tx.send(DaemonMessage::WorkspaceSpawnPreview {
                        workspace_id,
                        branch_name,
                        per_member,
                        request_id,
                    });
                }
                Err(err) => send_preview_failure(out_tx, &err, request_id),
            }
        }
        ClientMessage::PreviewSpawn {
            repo_id,
            branch_name,
            base_branch,
            use_worktree,
            request_id,
        } => {
            let preview = preview_single_spawn(
                hub,
                &repo_id,
                &branch_name,
                base_branch.as_deref(),
                use_worktree,
            )
            .await;
            match preview {
                Ok(preview) => {
                    let _ = out_tx.send(DaemonMessage::SpawnPreview {
                        repo_id,
                        branch_name,
                        preview,
                        request_id,
                    });
                }
                Err(err) => send_preview_failure(out_tx, &err, request_id),
            }
        }
        ClientMessage::SpawnSession(req) => {
            // A request `check_spawn_request` refuses is refused before the
            // user is asked to confirm a checkout it would never make.
            if let Err(failure) = check_spawn_request(&req) {
                send_spawn_failure(out_tx, &failure.into(), req.request_id.as_deref());
            } else if let Some(confirm) = in_place_checkout_confirm(hub, &req).await {
                let _ = out_tx.send(confirm);
            } else {
                // The requester hears of its spawn through the registry's one
                // broadcast, which carries `request_id` on this connection.
                let request_id = req.request_id.clone();
                let origin = request_origin(connection, request_id.clone());
                if let Err(err) = spawn_session(hub, req, origin, false).await {
                    send_spawn_failure(out_tx, &err, request_id.as_deref());
                }
            }
        }
        _ => warn!("spawn::handle: dispatch routed a message this module does not handle"),
    }
    Ok(())
}

/// Answers a failed spawn preview (`PreviewSpawn`, `PreviewWorkspaceSpawn`)
/// with an `Error` to this requester only, carrying the `request_id` it asked
/// with so the client can clear that pending preview.
fn send_preview_failure(
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
    err: &anyhow::Error,
    request_id: Option<String>,
) {
    warn!(?err, "spawn preview failed");
    let _ = out_tx.send(DaemonMessage::Error {
        message: err.to_string(),
        request_id,
    });
}

/// If `req` is an in-place single-repo spawn that would switch a dirty working
/// tree to a *different* branch and no strategy was chosen yet, return the
/// `CheckoutConfirmRequired` message asking the client how to proceed. Returns
/// `None` when the spawn can run normally (worktree mode, same branch, clean
/// tree, non-git folder, or a strategy already chosen).
async fn in_place_checkout_confirm(hub: &Hub, req: &SpawnRequest) -> Option<DaemonMessage> {
    let SpawnTarget::Single {
        repo_id,
        branch_name,
        use_worktree: false,
        checkout_strategy: None,
        ..
    } = &req.target
    else {
        return None;
    };
    let repo_path = hub.state.with_persisted(|s| {
        s.repos
            .iter()
            .find(|r| r.id == *repo_id)
            .map(|r| r.path.clone())
    })?;
    let repo_path = PathBuf::from(repo_path);
    if !crate::git::is_initialized(&repo_path).await {
        return None;
    }
    match crate::git::in_place_checkout_preflight(&repo_path, branch_name).await {
        Ok(crate::git::InPlaceCheckout::Dirty { count }) => Some(checkout_confirm_reply(
            repo_id,
            branch_name,
            req.request_id.as_deref(),
            count,
        )),
        Ok(_) => None,
        Err(err) => {
            warn!(
                ?err,
                repo_id, "in_place_checkout_confirm: preflight failed; spawning anyway"
            );
            None
        }
    }
}

/// The prompt for an in-place spawn of `branch` in `repo_id` whose tree has
/// `dirty_count` uncommitted changes, echoing the spawn's `request_id`: it
/// goes to the requester only.
fn checkout_confirm_reply(
    repo_id: &str,
    branch: &str,
    request_id: Option<&str>,
    dirty_count: usize,
) -> DaemonMessage {
    DaemonMessage::CheckoutConfirmRequired {
        repo_id: repo_id.to_owned(),
        branch: branch.to_owned(),
        dirty_count: u32::try_from(dirty_count).unwrap_or(u32::MAX),
        request_id: request_id.map(str::to_owned),
    }
}

pub(crate) async fn spawn_session(
    hub: &Hub,
    req: SpawnRequest,
    origin: Option<UpdateOrigin>,
    busy_excluded: bool,
) -> anyhow::Result<protocol::SessionSnapshot> {
    spawn_session_with_extras(hub, req, origin, busy_excluded, SpawnExtras::default()).await
}

/// [`spawn_session`] carrying the daemon-internal `extras` a recovery sets.
pub(crate) async fn spawn_session_with_extras(
    hub: &Hub,
    req: SpawnRequest,
    origin: Option<UpdateOrigin>,
    busy_excluded: bool,
    extras: SpawnExtras,
) -> anyhow::Result<protocol::SessionSnapshot> {
    // Boxed so the callers' futures stay small: the spawn pipeline's own
    // future is large.
    Box::pin(spawn_session_with(
        hub,
        req,
        origin,
        busy_excluded,
        extras,
        user_env::resolve,
    ))
    .await
}

/// What a spawn takes beside its request that never travels on the wire
/// `SpawnRequest` and is never stored in `SpawnConfig`: a recovered shell's
/// hook agent, the conversation it resumes in it and the options and trust
/// it resumes with. A recovery sets them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SpawnExtras {
    /// The agent whose plugin writes a recovered shell's hook files, builds
    /// its resume command and maps its hook events.
    pub(crate) hook_agent: Option<AgentId>,
    /// The conversation a recovered shell resumes in its hook agent.
    pub(crate) resume_conversation: Option<String>,
    /// The recovered selection's options, which the hook agent's hook plan
    /// and resume command take in place of the plain shell's own.
    pub(crate) hook_options: OptionValues,
    /// Whether the recovered run was trusted, which the hook agent's hook
    /// plan takes in place of the plain shell's own flag.
    pub(crate) hook_trusted: bool,
}

/// The longest a recovered shell waits for its prompt before typing its
/// resume command; it types sooner once the prompt has printed.
const SHELL_RESUME_DELAY_MS: u32 = 2000;

/// [`spawn_session_with_extras`] with the variable lookup `resolve` in place
/// of [`user_env::resolve`]: it resolves the rows' env references and the
/// provider's key.
#[expect(
    clippy::too_many_lines,
    reason = "Spawn is naturally a flat pipeline (resolve → config → mode switch → \
              timing). Extracting helpers would require threading the timing values \
              and adding a result wrapper for diminishing returns"
)]
async fn spawn_session_with<R>(
    hub: &Hub,
    mut req: SpawnRequest,
    origin: Option<UpdateOrigin>,
    busy_excluded: bool,
    extras: SpawnExtras,
    resolve: R,
) -> anyhow::Result<protocol::SessionSnapshot>
where
    R: KeyLookup,
{
    // Phase-level timing for the spawn pipeline so we can tell at a glance
    // whether a slow spawn was the git worktree setup, the PTY/child boot,
    // or something on the registry side. Logged at info so users debugging
    // perceived slowness can read them without flipping log filters.
    let t_total = std::time::Instant::now();
    // A plain shell carrying agent options, or a request the agent's plugin
    // refuses, refuses before the rows are sealed and before any git work,
    // so a refused spawn writes nothing to the credential store and checks
    // nothing out.
    check_plain_shell_options(&req)?;
    let (agent, backend, options) = request_agent(&req)?;
    // The validated options carry every field's default, so the stored
    // config replays what the session ran with.
    if let Some(selection) = req.agent.as_mut() {
        selection.options.clone_from(&options);
    }
    check_with_plugin(&req, backend, &options, &resolve)?;
    // A secret row's literal moves into the credential store and becomes a
    // `${secret:<id>}` reference before anything captures the request, so the
    // session record, the sidecar, `state.json`, the `Repos`/`Workspaces`
    // broadcast, the `GetSpawnConfig` reply and the history entry all hold the
    // reference and never the value.
    env_secrets::seal_rows(&mut req.extra_env, &req.secret_env_keys)?;
    // Capture the persist-able config before destructuring so duplicates
    // can be reconstructed verbatim from the session record / sidecar.
    let stored_config = protocol::SpawnConfig::from_request(&req);
    let SpawnRequest {
        label,
        target,
        mode,
        initial_prompt,
        dangerously_skip_permissions,
        agent: _,
        extra_env,
        secret_env_keys: _,
        prompt_injector,
        request_id: _,
        resume_conversation,
    } = req;
    info!(
        ?mode,
        agent = agent.0.as_str(),
        target_kind = match &target {
            SpawnTarget::Single { .. } => "single",
            SpawnTarget::Workspace { .. } => "workspace",
            SpawnTarget::Standalone { .. } => "standalone",
        },
        "spawn_session: begin"
    );
    if matches!(&target, SpawnTarget::Standalone { .. }) && !standalone_supports(mode) {
        return Err(anyhow!(
            "standalone targets only support plain_shell and interactive sessions"
        ));
    }
    reject_pin_without_worktree(&target)?;
    if mode == SessionMode::Headless && !backend.descriptor().caps.headless {
        return Err(headless_unsupported(backend));
    }
    // Env references resolve before any git work, so a refusal leaves no
    // worktree behind. `stored_config` keeps the references as sent; only the
    // child's environment gets their values.
    let rows = ResolvedRows::new(resolve_env_refs(&extra_env, |name| resolve.key(name))?);
    // The plugin's routing variables and key join the child's environment
    // here, after `stored_config` was captured, so they are never stored; a
    // missing key refuses before any git work.
    let plan = backend
        .spawn_env(&options, &rows, &resolve)
        .map_err(spawn_plan::SpawnFailure::from)?;
    let env = EnvPlan {
        set: crate::child_env::compose(backend.passthrough_vars(), &rows, &plan, |name| {
            std::env::var(name).ok()
        }),
        remove: plan.remove,
    };

    let t_resolve = std::time::Instant::now();
    let mut standalone_add_dirs = Vec::new();
    let (kind, members, primary_cwd, default_label, workspace_id) = match target {
        SpawnTarget::Single {
            repo_id,
            branch_name,
            base_branch,
            use_worktree,
            checkout_strategy,
            worktree_reuse,
            existing_worktree,
        } => {
            spawn_single(
                hub,
                &repo_id,
                SinglePlan {
                    branch_name: &branch_name,
                    base_branch,
                    use_worktree,
                    checkout_strategy,
                    worktree_reuse,
                    existing_worktree,
                },
            )
            .await?
        }
        SpawnTarget::Workspace {
            workspace_id,
            branch_name,
            base_branch,
            use_worktree,
            worktree_reuse,
            existing_worktrees,
        } => {
            spawn_workspace(
                hub,
                &workspace_id,
                ws::ResolveRequest {
                    branch_name: &branch_name,
                    explicit_base: base_branch.as_deref(),
                    use_worktree,
                    pins: &existing_worktrees,
                },
                worktree_reuse,
            )
            .await?
        }
        SpawnTarget::Standalone { cwd, add_dirs } => {
            if mode == SessionMode::Interactive {
                standalone_add_dirs = add_dirs;
                spawn_standalone_agent(cwd.as_deref())?
            } else {
                spawn_standalone_shell(cwd.as_deref())?
            }
        }
    };
    info!(
        elapsed_ms = u64::try_from(t_resolve.elapsed().as_millis()).unwrap_or(u64::MAX),
        member_count = members.len(),
        cwd = %primary_cwd.display(),
        "spawn_session: git/worktree resolution done"
    );

    let session_id = new_id();
    let label = label.unwrap_or(default_label);
    let cfg = SpawnArgs {
        dangerously_skip_permissions,
        agent,
        backend,
        options,
        env,
        prompt_injector,
        origin,
        resume_conversation,
        add_dirs: standalone_add_dirs,
        extras,
    };

    // Record last-used agent per targeted repo. Best-effort: a state.json
    // write failure here is logged but not fatal to the spawn.
    let mut repos_changed = false;
    let mut workspaces_changed = false;
    for member in &members {
        if let Err(err) = persist_last_agent(&hub.state, &member.repo_id, &cfg.agent) {
            warn!(?err, repo_id = %member.repo_id, agent = cfg.agent.0.as_str(), "persist_last_agent failed");
        } else {
            repos_changed = true;
        }
    }

    // Record the full spawn config on the targeted container so "Launch last
    // again" (sidebar double-click / context-menu submenu) can replay it.
    // Workspace spawns persist to the workspace only — single-repo "launch
    // last" intentionally stays decoupled from workspace launches.
    match &stored_config.target {
        SpawnTarget::Single { repo_id, .. } => {
            if let Err(err) =
                persist_repo_last_spawn_config(&hub.state, repo_id, stored_config.clone())
            {
                warn!(?err, %repo_id, "persist_repo_last_spawn_config failed");
            } else {
                repos_changed = true;
            }
        }
        SpawnTarget::Workspace { workspace_id, .. } => {
            if let Err(err) =
                persist_workspace_last_spawn_config(&hub.state, workspace_id, stored_config.clone())
            {
                warn!(?err, %workspace_id, "persist_workspace_last_spawn_config failed");
            } else {
                workspaces_changed = true;
            }
        }
        SpawnTarget::Standalone { .. } => {}
    }
    if repos_changed {
        let repos = hub.state.with_persisted(|s| s.repos.clone());
        let _ = hub.state_events.send(StateEvent::Repos(repos));
    }
    if workspaces_changed {
        let workspaces = hub.state.with_persisted(|s| s.workspaces.clone());
        let _ = hub.state_events.send(StateEvent::Workspaces(workspaces));
    }

    let t_child = std::time::Instant::now();
    let result = match mode {
        SessionMode::Headless => {
            let prompt = initial_prompt
                .clone()
                .ok_or_else(|| anyhow!("headless sessions require an initial_prompt"))?;
            spawn_headless_session(
                hub,
                session_id,
                label,
                kind,
                members,
                workspace_id,
                primary_cwd,
                prompt,
                &cfg,
                stored_config,
                busy_excluded,
            )
        }
        SessionMode::PlainShell => {
            if initial_prompt.is_some() {
                return Err(anyhow!(
                    "plain shell sessions do not accept an initial_prompt"
                ));
            }
            spawn_plain_shell_session(
                hub,
                session_id,
                label,
                kind,
                members,
                workspace_id,
                primary_cwd,
                &cfg,
                stored_config,
                busy_excluded,
            )
            .await
        }
        SessionMode::Interactive => {
            spawn_interactive_session(
                hub,
                session_id,
                label,
                kind,
                members,
                workspace_id,
                primary_cwd,
                initial_prompt,
                &cfg,
                stored_config,
                busy_excluded,
            )
            .await
        }
    };
    let outcome = if result.is_ok() { "ok" } else { "error" };
    info!(
        outcome,
        elapsed_child_ms = u64::try_from(t_child.elapsed().as_millis()).unwrap_or(u64::MAX),
        elapsed_total_ms = u64::try_from(t_total.elapsed().as_millis()).unwrap_or(u64::MAX),
        "spawn_session: child boot done"
    );
    if let Err(err) = &result {
        warn!(
            error = %format!("{err:#}"),
            "spawn_session: child boot failed"
        );
    }
    result
}

pub(super) fn expected_output_subscribers(mode: SessionMode) -> usize {
    match mode {
        SessionMode::Interactive | SessionMode::PlainShell => 2,
        SessionMode::Headless => 0,
    }
}

/// Per-spawn configuration bundled together to keep spawn-fn signatures narrow.
struct SpawnArgs {
    dangerously_skip_permissions: bool,
    /// The agent this spawn runs; a plain shell's is the default agent.
    agent: AgentId,
    /// `agent`'s plugin, as [`agent_registry::plugin`] found it.
    backend: &'static dyn AgentPlugin,
    /// The agent's options as [`agent_api::validate`] answered them, keyed by
    /// its descriptor's fields.
    options: OptionValues,
    /// The child's environment: the keep-list, the agent's passthrough
    /// variables, the resolved rows and the agent's routing variables (see
    /// [`crate::child_env::compose`]). Never stored.
    env: EnvPlan,
    /// Scripted PTY input driven post-spawn for interactive sessions. When
    /// `Some`, the interactive spawn path omits the prompt CLI flag — the
    /// injector is responsible for delivering the prompt. Ignored by the
    /// headless and plain-shell paths.
    prompt_injector: Option<protocol::PromptInjector>,
    /// The request that asked for this spawn; the session's first broadcast
    /// carries it, so the requester hears of its spawn once, in order.
    origin: Option<UpdateOrigin>,
    /// The Claude conversation an interactive Claude spawn resumes.
    resume_conversation: Option<String>,
    /// A standalone target's extra `--add-dir` directories.
    add_dirs: Vec<String>,
    /// What a recovery sets beside the request (see [`SpawnExtras`]); only
    /// the plain-shell path reads it.
    extras: SpawnExtras,
}

impl SpawnArgs {
    /// Build the borrowed [`SpawnContext`] view plugins consume.
    fn common(&self) -> SpawnContext<'_> {
        SpawnContext {
            dangerously_skip_permissions: self.dangerously_skip_permissions,
            has_prompt_injector: self.prompt_injector.is_some(),
            conversation_id: None,
            resume_conversation: self.resume_conversation.as_deref(),
            add_dirs: &self.add_dirs,
            // Only `spawn_interactive_session` writes hook files and overrides
            // this.
            hook_args: &[],
        }
    }
}

/// The agent `request` runs, its plugin, and its options as the plugin's
/// schema validates them: every field's value, its default where the request
/// names none. A plain shell runs no agent; it is planned with the default
/// agent's defaults, as its environment always was. Refuses an agent session
/// that names no agent, an agent this daemon has no plugin for, and options
/// the agent's schema refuses.
fn request_agent(
    request: &SpawnRequest,
) -> Result<(AgentId, &'static dyn AgentPlugin, OptionValues), spawn_plan::SpawnFailure> {
    let (agent, values) = match &request.agent {
        Some(selection) => (selection.agent.clone(), selection.options.clone()),
        None if request.mode == SessionMode::PlainShell => (default_agent(), OptionValues::new()),
        None => return Err(agent_refusal("An agent session needs an agent.".to_owned())),
    };
    let Some(backend) = agent_registry::plugin(&agent) else {
        return Err(agent_refusal(format!(
            "This daemon has no agent `{}`.",
            agent.0
        )));
    };
    let options = agent_api::validate(backend.descriptor(), &values)
        .map_err(|err| agent_refusal(err.message))?;
    Ok((agent, backend, options))
}

/// The refusal of a spawn whose agent selection is wrong, before any work.
fn agent_refusal(detail: String) -> spawn_plan::SpawnFailure {
    spawn_plan::SpawnFailure {
        title: "Couldn't start the session".to_owned(),
        detail,
        hint: None,
    }
}

/// The refusal of a headless spawn for an agent that has no headless mode.
fn headless_unsupported(backend: &dyn AgentPlugin) -> anyhow::Error {
    anyhow!(
        "headless mode is not yet supported for {}; use interactive mode",
        backend.descriptor().tag
    )
}

/// What a new interactive session's record takes from its spawn.
struct NewInteractive {
    session_id: String,
    label: String,
    kind: SessionKind,
    members: Vec<SessionMember>,
    workspace_id: Option<String>,
    agent: AgentId,
    /// The leaf's program name: the agent descriptor's tag.
    program_name: String,
    cwd: String,
    stored_config: protocol::SpawnConfig,
    last_prompt: Option<String>,
    conversation_id: Option<String>,
    started_at: chrono::DateTime<Utc>,
    busy_excluded: bool,
    /// The label of the shell the agent is typed into, when the PTY child is
    /// that shell; `None` when the agent is the child.
    shell_label: Option<String>,
}

/// The record of a new interactive session.
fn interactive_record(
    new: NewInteractive,
    pty: Option<Arc<crate::pty::PtyHandle>>,
) -> SessionRecord {
    let mut record = SessionRecord {
        id: new.session_id,
        label: new.label.clone(),
        default_label: new.label,
        user_label: None,
        kind: new.kind,
        worktree_paths: worktree_paths_for_config(&new.stored_config, &new.members),
        members: new.members,
        mode: SessionMode::Interactive,
        started_at: new.started_at,
        status: SessionStatus::Idle,
        status_since: Some(new.started_at),
        exit_code: None,
        metrics: SessionMetrics::default(),
        recent_actions: Vec::new(),
        pty,
        headless: None,
        workspace_id: new.workspace_id,
        agent: new.agent,
        terminal_title: None,
        program_name: Some(new.program_name),
        current_cwd: Some(new.cwd),
        appearance: AppearanceOverrides::default(),
        spawn_config: Some(new.stored_config),
        is_abandoned: false,
        is_inactive: false,
        last_prompt: new.last_prompt,
        input_notifier: None,
        screen: None,
        scrollback_snapshot_req: None,
        spawn_origin: None,
        conversation_id: new.conversation_id,
        pending_input: None,
        last_hook_time: None,
        hook_driven: false,
        background: false,
        last_assistant_message: None,
        busy_excluded: new.busy_excluded,
        agent_in_shell: new.shell_label.is_some(),
        shell_label: new.shell_label,
        agent_exit_announced: false,
        size_set: false,
        hook_agent: None,
    };
    push_recent_action(&mut record, "session started".to_string());
    record
}

/// What a new plain-shell session's record takes from its spawn.
struct NewPlainShell {
    session_id: String,
    label: String,
    kind: SessionKind,
    members: Vec<SessionMember>,
    workspace_id: Option<String>,
    shell_label: String,
    cwd: String,
    stored_config: protocol::SpawnConfig,
    started_at: chrono::DateTime<Utc>,
    busy_excluded: bool,
    /// The agent a recovered shell's hook events are mapped with.
    hook_agent: Option<AgentId>,
}

/// The record of a new plain-shell session.
fn plain_shell_record(
    new: NewPlainShell,
    pty: Option<Arc<crate::pty::PtyHandle>>,
) -> SessionRecord {
    let mut record = SessionRecord {
        id: new.session_id,
        label: new.label.clone(),
        default_label: new.label,
        user_label: None,
        kind: new.kind,
        members: new.members.clone(),
        mode: SessionMode::PlainShell,
        started_at: new.started_at,
        status: SessionStatus::Idle,
        status_since: Some(new.started_at),
        exit_code: None,
        metrics: SessionMetrics::default(),
        recent_actions: Vec::new(),
        pty,
        headless: None,
        workspace_id: new.workspace_id,
        // Plain-shell sessions have no agent CLI in the picture; the record
        // still needs an agent, so it carries the default one. A recovered
        // shell's hooks map with its `hook_agent` instead.
        agent: default_agent(),
        terminal_title: None,
        program_name: Some(new.shell_label.clone()),
        current_cwd: Some(new.cwd),
        appearance: AppearanceOverrides::default(),
        spawn_config: Some(new.stored_config.clone()),
        is_abandoned: false,
        is_inactive: false,
        worktree_paths: worktree_paths_for_config(&new.stored_config, &new.members),
        // Plain shells never carry a kickoff prompt.
        last_prompt: None,
        input_notifier: None,
        screen: None,
        scrollback_snapshot_req: None,
        spawn_origin: None,
        conversation_id: None,
        pending_input: None,
        last_hook_time: None,
        hook_driven: false,
        background: false,
        last_assistant_message: None,
        busy_excluded: new.busy_excluded,
        agent_in_shell: false,
        shell_label: None,
        agent_exit_announced: false,
        size_set: false,
        hook_agent: new.hook_agent,
    };
    push_recent_action(
        &mut record,
        format!("session started: {}", new.shell_label.as_str()),
    );
    record
}

/// Where a session's tracer runs, as its sidecar records it.
struct TracerSidecar {
    pid: u32,
    pipe: String,
    exe_path: String,
}

/// The spawn-time sidecar of the new interactive session `record`, whose
/// child is `pid`. `program_name` is the program the user runs: the agent's
/// label, as the live record carries it, not the tracer the daemon spawns to
/// supervise it. Liveness of a tracer-backed session comes from `tracer_pid`,
/// which `is_session_alive` checks against the tracer's own name, and never
/// from `program_name`.
fn interactive_meta(
    record: &SessionRecord,
    pid: u32,
    tracer: TracerSidecar,
) -> anyhow::Result<OrphanMeta> {
    orphan::meta_from_record(
        record.id.clone(),
        pid,
        record.label.clone(),
        record.kind.clone(),
        SessionMode::Interactive,
        record.members.clone(),
        record.started_at,
        record.workspace_id.clone(),
        record.program_name.clone(),
        Some(record.agent.clone()),
        record.spawn_config.clone(),
        record.last_prompt.clone(),
        record.current_cwd.clone(),
        Some(tracer.pid),
        Some(tracer.pipe),
        Some(tracer.exe_path),
        record.conversation_id.clone(),
    )
}

/// The spawn-time sidecar of a new plain-shell session whose child is `pid`.
/// Like [`interactive_meta`], `program_name` is the program the user runs: the
/// shell's label, as the live record carries it, not the tracer the daemon
/// spawns to supervise it. Liveness of a tracer-backed session comes from
/// `tracer_pid`. The record itself is already in the registry at the call
/// site, so this takes the fields the sidecar needs instead of `&record`.
#[expect(
    clippy::too_many_arguments,
    reason = "The call site holds the spawn's locals, not the record, so it passes them through"
)]
fn plain_shell_meta(
    session_id: String,
    pid: u32,
    label: String,
    kind: SessionKind,
    members: Vec<SessionMember>,
    started_at: chrono::DateTime<Utc>,
    workspace_id: Option<String>,
    program_name: Option<String>,
    stored_config: Option<protocol::SpawnConfig>,
    current_cwd: Option<String>,
    tracer: TracerSidecar,
) -> anyhow::Result<OrphanMeta> {
    orphan::meta_from_record(
        session_id,
        pid,
        label,
        kind,
        SessionMode::PlainShell,
        members,
        started_at,
        workspace_id,
        program_name,
        // A plain shell runs no agent.
        None,
        stored_config,
        // Plain shells never carry a kickoff prompt.
        None,
        current_cwd,
        Some(tracer.pid),
        Some(tracer.pipe),
        Some(tracer.exe_path),
        None,
    )
}

/// The conversation id an interactive spawn records and passes its CLI, the
/// conversation it resumes, and the baseline its discovery starts from, as
/// `backend` plans them: a minted id is recorded, a resumed one is recorded
/// and resumed, a pre-created one is recorded when `program` (with
/// `leading_args`, in `cwd`) printed one, and a discovered one is looked for
/// after launch, against the conversations `backend` already holds for `cwd`
/// (looked up with the rows of `extra_env`).
async fn planned_conversation(
    backend: &'static dyn AgentPlugin,
    cfg: &SpawnArgs,
    program: &str,
    leading_args: &[String],
    cwd: &Path,
    extra_env: &[(String, String)],
) -> (Option<String>, Option<String>, Option<Baseline>) {
    match backend.conversation_plan(&cfg.options, cfg.resume_conversation.as_deref()) {
        ConversationPlan::Minted(id) => (Some(id), None, None),
        ConversationPlan::Resumed(id) => (Some(id.clone()), Some(id), None),
        ConversationPlan::None => (None, None, None),
        ConversationPlan::PreCreate { args, parse } => {
            let spec = PreCreateSpec {
                program: program.to_owned(),
                args: leading_args.iter().cloned().chain(args).collect(),
                cwd: cwd.to_path_buf(),
                env_set: cfg.env.set.clone(),
                env_remove: cfg.env.remove.clone(),
            };
            let agent = &backend.descriptor().id.0;
            let id = pre_create::pre_create(spec, parse).await;
            if let Some(id) = &id {
                info!(agent = %agent, id_len = id.len(), "pre-created the conversation");
            } else {
                warn!(
                    agent = %agent,
                    "pre-create gave no conversation id; spawning without a recorded conversation"
                );
            }
            (id, None, None)
        }
        ConversationPlan::DiscoverAfterLaunch => (
            None,
            None,
            discovery_baseline(backend, cwd, extra_env).await,
        ),
    }
}

/// The conversations `backend` holds for `folder` before the spawn, every
/// one recorded up to now, so discovery never gives the new session one of
/// them. `None`, logged, when the lookup itself fails: the session then
/// records no conversation.
async fn discovery_baseline(
    backend: &'static dyn AgentPlugin,
    folder: &Path,
    extra_env: &[(String, String)],
) -> Option<Baseline> {
    let folder = folder.to_path_buf();
    let extra_env = extra_env.to_vec();
    let lookup = tokio::task::spawn_blocking(move || {
        let rows = conversations::lookup_rows(&extra_env);
        let launched_at = Utc::now();
        let window = (chrono::DateTime::<Utc>::MIN_UTC, launched_at);
        let ids = conversations::candidates(backend, &folder, window, &rows)
            .into_iter()
            .map(|found| found.id)
            .collect();
        Baseline {
            ids,
            plugin: backend,
            rows,
            folder,
            launched_at,
        }
    });
    match lookup.await {
        Ok(baseline) => Some(baseline),
        Err(err) => {
            warn!(
                ?err,
                agent = %backend.descriptor().id.0,
                "discovery baseline lookup failed; spawning without a recorded conversation"
            );
            None
        }
    }
}

/// Register `session_id`'s waiter when its plan discovers its conversation:
/// `discover` is the baseline taken before the spawn. Answers whether a
/// waiter was registered, so the caller starts its poller once the session
/// is published.
fn register_discovery(
    sessions: &SessionRegistry,
    session_id: &str,
    discover: Option<Baseline>,
) -> bool {
    let Some(baseline) = discover else {
        return false;
    };
    discovery::register(sessions, Waiter::new(session_id.to_owned(), baseline));
    true
}

#[expect(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "Constructing a session record is naturally wide; bundling into a struct adds noise"
)]
async fn spawn_interactive_session(
    hub: &Hub,
    session_id: String,
    label: String,
    kind: SessionKind,
    members: Vec<SessionMember>,
    workspace_id: Option<String>,
    primary_cwd: PathBuf,
    initial_prompt: Option<String>,
    cfg: &SpawnArgs,
    stored_config: protocol::SpawnConfig,
    busy_excluded: bool,
) -> anyhow::Result<protocol::SessionSnapshot> {
    let last_prompt = initial_prompt.clone();
    let agent = cfg.agent.clone();
    let backend = cfg.backend;
    // The host's launch command is read now, never from the session's stored
    // config, so Restart and Resume use the current one.
    let command = choose_agent_command(backend, &hub.state.agent_command(&agent), &|name| {
        std::env::var(name).ok()
    })?;
    let raw_program = command.program.clone();
    let (program, prepend_args) = resolve_agent_program(&raw_program);
    let (conversation_id, resume_conversation, discover) = planned_conversation(
        backend,
        cfg,
        &program,
        &prepend_args,
        &primary_cwd,
        &stored_config.extra_env,
    )
    .await;
    // The argv names the hook files, so they are written before the argv is
    // built. A plugin without hooks plans none; a failure leaves this spawn on
    // the heuristic.
    let hook_settings = write_hook_settings(
        hub,
        &session_id,
        &agent,
        &cfg.options,
        cfg.dangerously_skip_permissions,
    );
    let common = SpawnContext {
        conversation_id: conversation_id.as_deref(),
        resume_conversation: resume_conversation.as_deref(),
        hook_args: hook_settings.as_ref().map_or(&[], HookSettings::args),
        ..cfg.common()
    };
    // An agent whose descriptor says it can run inside a shell is typed into
    // the user's shell, which stays in the pane once the agent ends.
    let shell_resume = backend.descriptor().caps.shell_resume;
    let launch = match shell_resume.then(|| host_shell(hub)).flatten() {
        Some(shell) => {
            // A program word typed verbatim stands alone: the resolver's
            // leading arguments belong to the resolved program only.
            let mut argv = Vec::new();
            if !command.typed_verbatim {
                argv.push(program.clone());
                argv.extend(prepend_args.iter().cloned());
            }
            argv.extend(backend.interactive_args(&cfg.options, &common, &members, None)?);
            let cwd = primary_cwd.to_string_lossy();
            plan_launch(
                shell_resume,
                Some(shell),
                AgentLaunch {
                    argv: &argv,
                    typed: TypedCommand {
                        program_word: command.typed_verbatim.then_some(raw_program.as_str()),
                        extra: &command.extra_text,
                    },
                    cwd: &cwd,
                    initial_prompt: initial_prompt.as_deref(),
                    preset: cfg.prompt_injector.clone(),
                },
            )
        }
        None => Launch::Direct,
    };
    let (program, args, env, injectors, shell_label) = match launch {
        Launch::Direct => {
            let args = backend.interactive_args(
                &cfg.options,
                &common,
                &members,
                initial_prompt.as_deref(),
            )?;
            let final_args = command.direct_args(prepend_args, args);
            info!(
                agent = agent.0.as_str(),
                raw = %raw_program,
                program = %program,
                extra_argc = command.extra_args.len(),
                argc = final_args.len(),
                args = ?final_args,
                "spawn_interactive_session: resolved program"
            );
            let injectors: Vec<protocol::PromptInjector> =
                cfg.prompt_injector.clone().into_iter().collect();
            (program, final_args, cfg.env.set.clone(), injectors, None)
        }
        Launch::InShell(launch) => {
            info!(
                agent = agent.0.as_str(),
                raw = %raw_program,
                program = %program,
                shell = %launch.shell.label,
                "spawn_interactive_session: the agent is typed into the shell"
            );
            let env = with_shell_env(cfg.env.set.clone(), launch.shell.extra_env);
            (
                launch.shell.program,
                launch.shell.args,
                env,
                launch.injectors,
                Some(launch.shell.label),
            )
        }
    };
    let agent_in_shell = shell_label.is_some();

    let initial_cwd = primary_cwd.to_string_lossy().into_owned();
    let spec = PtySpawnSpec {
        session_id: session_id.clone(),
        program,
        args,
        cwd: primary_cwd,
        env,
        env_remove: cfg.env.remove.clone(),
        cols: 120,
        rows: 32,
    };
    let (cols, rows) = (spec.cols, spec.rows);
    #[cfg(test)]
    super::child_env_probe::record(&spec.env, &spec.env_remove);
    let expected_subscribers = expected_output_subscribers(SessionMode::Interactive);
    let spawn_result = match hook_settings
        .as_ref()
        .map(|settings| settings.tracer_exe.as_path())
    {
        // The same cached copy the injected settings file names, so the hook
        // command can only name the tracer this session really runs.
        Some(tracer_exe) => {
            tracer_client::spawn_with_tracer(tracer_exe, &hub.dirs, spec, expected_subscribers)
                .await
        }
        None => tracer_client::spawn(&hub.dirs, spec, expected_subscribers).await,
    };
    let tracer_spawn = match spawn_result {
        Ok(spawned) => spawned,
        Err(err) => {
            // A failed spawn must leave neither a settings file with nothing
            // to clean it up nor a sidecar-less session dir, which the orphan
            // scan skips forever.
            if let Some(settings) = hook_settings {
                settings.discard(&session_id);
            }
            return Err(err.context(format!("spawning {} via tracer", agent.0)));
        }
    };
    let pty = tracer_spawn.handle;
    let tracer = TracerSidecar {
        pid: tracer_spawn.tracer_pid,
        pipe: tracer_spawn.pipe_name,
        exe_path: tracer_spawn.tracer_exe_path.to_string_lossy().into_owned(),
    };
    let started_at = Utc::now();
    let record = interactive_record(
        NewInteractive {
            session_id: session_id.clone(),
            label,
            kind,
            members,
            workspace_id,
            agent: agent.clone(),
            program_name: backend.descriptor().tag.clone(),
            cwd: initial_cwd,
            stored_config,
            last_prompt,
            conversation_id,
            started_at,
            busy_excluded,
            shell_label,
        },
        Some(Arc::clone(&pty)),
    );
    let meta = pty
        .pid()
        .and_then(|pid| match interactive_meta(&record, pid, tracer) {
            Ok(meta) => Some(meta),
            Err(err) => {
                warn!(?err, %session_id, "no sidecar written for the session");
                None
            }
        });

    // Two-stage insert: put the record in the registry first so attach_lifecycle's
    // exit watcher can find it, then patch the lifecycle channels onto the record
    // before publishing SessionUpdated. Publishing earlier would race with the
    // client's LoadScrollback request — the client could see the session and
    // request scrollback before scrollback_snapshot_req is wired up, fall through
    // to a direct file read (empty before first output), and miss any output that
    // streams in before the per-client forwarder is started. Hook events held
    // for this id while the tracer spawned are applied to the record here, so
    // the first broadcast is still the wired publish below. The discovery
    // waiter is registered before the exit watcher starts, so a child that
    // exits at once still gets its final look.
    let discovering = register_discovery(&hub.sessions, &session_id, discover);
    let (pending, awaiting) = hub.pending_hooks.insert_with_pending(hub, record);
    let snap_tx = attach_lifecycle(
        &hub.sessions,
        session_id.clone(),
        &pty,
        Some(hub.dirs.clone()),
    );
    let screen: screen::SharedScreen = Arc::new(Mutex::new(screen::Screen::new(cols, rows)));
    // A shell hosting the agent runs the shell's watcher, which reads input
    // volume and follows the agent while it runs in the shell.
    let shell_input = agent_in_shell.then(mpsc::unbounded_channel::<usize>);
    {
        let mut guard = crate::sync::lock(pending.arc());
        guard.scrollback_snapshot_req = Some(snap_tx);
        guard.screen = Some(Arc::clone(&screen));
        if let Some((in_tx, _)) = &shell_input {
            guard.input_notifier = Some(in_tx.clone());
        }
    }
    if let Some((_, in_rx)) = shell_input {
        pty_state::watch_plain_shell(
            &hub.sessions,
            session_id.clone(),
            pty_state::ShellStreams {
                output: pty.output.subscribe(),
                input_pulses: in_rx,
                screen,
                rules: rules_for(&agent),
            },
            hub.attention_tx.clone(),
            SessionStatus::Idle,
        );
    } else {
        pty_state::watch(
            &hub.sessions,
            session_id.clone(),
            pty_state::AgentStreams {
                output: pty.output.subscribe(),
                screen,
                rules: rules_for(&agent),
            },
            hub.attention_tx.clone(),
        );
    }
    osc_title::watch(
        &hub.sessions,
        session_id.clone(),
        pty.output.subscribe(),
        agent_in_shell,
    );
    let snap = crate::sync::lock(pending.arc()).snapshot();
    pending.publish_from(cfg.origin.clone());
    announce_replayed_prompt(hub, &session_id, awaiting);
    if discovering {
        discovery::start_poller(&hub.sessions, hub.dirs.clone(), session_id.clone());
    }

    if let Some(mut meta) = meta {
        meta.status = Some(snap.status);
        meta.status_since = snap.status_since;
        write_meta_then_sync(hub, &meta);
    }

    if !injectors.is_empty() {
        // What follows the launch is meant for the agent, so it waits for
        // the agent to be up in its shell.
        let agent_up: Option<inject::AgentUp> = agent_in_shell.then(|| {
            let registry = Arc::clone(&hub.sessions);
            let id = session_id.clone();
            Box::new(move || pty_state::agent_up(&registry, &id)) as inject::AgentUp
        });
        inject::run_chain(
            session_id.clone(),
            Arc::clone(&pty),
            injectors,
            agent_up,
            backend.ready_signal(),
        );
    }
    Ok(snap)
}

/// The user's shell, to host an agent typed into it; `None`, logged, when
/// none resolves, and the agent is then spawned directly.
fn host_shell(hub: &Hub) -> Option<ShellProgram> {
    match resolve_shell_program(&hub.dirs) {
        Ok(shell) => Some(shell),
        Err(err) => {
            warn!(
                error = %format!("{err:#}"),
                "no shell to host the agent; spawning the agent directly"
            );
            None
        }
    }
}

/// `env` with the shell's own variables set over it.
fn with_shell_env(
    mut env: Vec<(String, String)>,
    shell_env: Vec<(String, String)>,
) -> Vec<(String, String)> {
    for (k, v) in shell_env {
        if let Some(slot) = env.iter_mut().find(|(existing, _)| existing == &k) {
            slot.1 = v;
        } else {
            env.push((k, v));
        }
    }
    env
}

#[expect(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "Constructing a session record is naturally wide; bundling into a struct adds noise"
)]
async fn spawn_plain_shell_session(
    hub: &Hub,
    session_id: String,
    label: String,
    kind: SessionKind,
    members: Vec<SessionMember>,
    workspace_id: Option<String>,
    primary_cwd: PathBuf,
    cfg: &SpawnArgs,
    stored_config: protocol::SpawnConfig,
    busy_excluded: bool,
) -> anyhow::Result<protocol::SessionSnapshot> {
    let shell = resolve_shell_program(&hub.dirs)?;
    // A recovered shell gets its hook agent's injected hooks, so the CLI's
    // reports drive its status. The files are written after the last
    // fallible setup step and before the spawn, and the resume command the
    // plugin builds from them is typed once the shell is up.
    let resume = match (&cfg.extras.hook_agent, &cfg.extras.resume_conversation) {
        (Some(agent), Some(conversation)) => prepare_shell_resume(
            hub,
            &session_id,
            cfg,
            agent_registry::plugin(agent)
                .ok_or_else(|| anyhow!("no agent plugin `{}` is registered", agent.0))?,
            conversation,
        )?,
        _ => ShellResume::default(),
    };
    let ShellResume {
        hook_settings,
        injector: resume_injector,
    } = resume;
    let initial_cwd = primary_cwd.to_string_lossy().into_owned();
    let env = with_shell_env(cfg.env.set.clone(), shell.extra_env);
    let spec = PtySpawnSpec {
        session_id: session_id.clone(),
        program: shell.program,
        args: shell.args,
        cwd: primary_cwd,
        env,
        env_remove: cfg.env.remove.clone(),
        cols: 120,
        rows: 32,
    };
    let (cols, rows) = (spec.cols, spec.rows);
    #[cfg(test)]
    super::child_env_probe::record(&spec.env, &spec.env_remove);
    let expected_subscribers = expected_output_subscribers(SessionMode::PlainShell);
    let spawn_result = match &hook_settings {
        // The same cached copy the settings file names, so that file can only
        // name the tracer this session really runs.
        Some(settings) => {
            tracer_client::spawn_with_tracer(
                &settings.tracer_exe,
                &hub.dirs,
                spec,
                expected_subscribers,
            )
            .await
        }
        None => tracer_client::spawn(&hub.dirs, spec, expected_subscribers).await,
    };
    let tracer_spawn = match spawn_result {
        Ok(spawned) => spawned,
        Err(err) => {
            // A failed spawn must leave neither a settings file with nothing
            // to clean it up nor a sidecar-less session dir, which the orphan
            // scan skips forever.
            if let Some(settings) = hook_settings {
                settings.discard(&session_id);
            }
            return Err(err.context("spawning plain shell via tracer"));
        }
    };
    let pty = tracer_spawn.handle;
    let tracer_pid = tracer_spawn.tracer_pid;
    let tracer_pipe = tracer_spawn.pipe_name;
    let tracer_exe_path = tracer_spawn.tracer_exe_path.to_string_lossy().into_owned();
    let pid = pty.pid();

    let started_at = Utc::now();
    let record = plain_shell_record(
        NewPlainShell {
            session_id: session_id.clone(),
            label: label.clone(),
            kind: kind.clone(),
            members: members.clone(),
            workspace_id: workspace_id.clone(),
            shell_label: shell.label.clone(),
            cwd: initial_cwd.clone(),
            stored_config: stored_config.clone(),
            started_at,
            busy_excluded,
            hook_agent: cfg.extras.hook_agent.clone(),
        },
        Some(Arc::clone(&pty)),
    );

    // Two-stage insert — see spawn_interactive_session for the same pattern
    // and rationale, including the held hook events applied here.
    let (pending, awaiting) = hub.pending_hooks.insert_with_pending(hub, record);
    let snap_tx = attach_lifecycle(
        &hub.sessions,
        session_id.clone(),
        &pty,
        Some(hub.dirs.clone()),
    );
    let (in_tx, in_rx) = mpsc::unbounded_channel::<usize>();
    let screen: screen::SharedScreen = Arc::new(Mutex::new(screen::Screen::new(cols, rows)));
    {
        let mut guard = crate::sync::lock(pending.arc());
        guard.scrollback_snapshot_req = Some(snap_tx);
        guard.input_notifier = Some(in_tx);
        guard.screen = Some(Arc::clone(&screen));
    }
    pty_state::watch_plain_shell(
        &hub.sessions,
        session_id.clone(),
        pty_state::ShellStreams {
            output: pty.output.subscribe(),
            input_pulses: in_rx,
            screen,
            rules: rules_for(&default_agent()),
        },
        hub.attention_tx.clone(),
        SessionStatus::Idle,
    );
    // Keep `osc_title::watch` so window-title escape sequences (which pwsh
    // emits by default) are recorded as `terminal_title` annotations; the
    // canonical session `label` stays whatever the spawn pipeline assigned.
    osc_title::watch(
        &hub.sessions,
        session_id.clone(),
        pty.output.subscribe(),
        true,
    );
    let snap = crate::sync::lock(pending.arc()).snapshot();
    pending.publish_from(cfg.origin.clone());
    announce_replayed_prompt(hub, &session_id, awaiting);

    if let Some(pid) = pid
        && let Ok(mut meta) = plain_shell_meta(
            session_id.clone(),
            pid,
            label,
            kind,
            members,
            started_at,
            workspace_id,
            Some(shell.label.clone()),
            Some(stored_config),
            Some(initial_cwd),
            TracerSidecar {
                pid: tracer_pid,
                pipe: tracer_pipe,
                exe_path: tracer_exe_path,
            },
        )
    {
        meta.status = Some(snap.status);
        meta.status_since = snap.status_since;
        meta.hook_agent.clone_from(&cfg.extras.hook_agent);
        write_meta_then_sync(hub, &meta);
    }

    // A recovered shell types the resume command its hook agent's plugin
    // built; any other plain shell runs the injector its request carried.
    if let Some(injector) = resume_injector.or_else(|| cfg.prompt_injector.clone()) {
        // A shell-prompt step reads no agent readiness, so the default stands in.
        inject::run(
            session_id.clone(),
            Arc::clone(&pty),
            injector,
            agent_api::ReadySignal::default(),
        );
    }
    Ok(snap)
}

/// A headless run's child: the plugin's own program and arguments. The host's
/// launch command never applies, since extra arguments could break the
/// stream-json parse.
///
/// # Errors
///
/// The options do not decode, or the agent builds no headless argv (the
/// dispatcher already rejects headless mode for an agent whose descriptor
/// lacks it).
fn headless_spec(
    cfg: &SpawnArgs,
    members: &[SessionMember],
    prompt: &str,
    primary_cwd: PathBuf,
) -> anyhow::Result<headless::HeadlessSpec> {
    let backend = cfg.backend;
    let args = backend
        .headless_args(&cfg.options, &cfg.common(), members, prompt)?
        .ok_or_else(|| headless_unsupported(backend))?;
    Ok(headless::HeadlessSpec {
        program: backend.resolve_program(),
        args,
        cwd: primary_cwd,
        env: cfg.env.set.clone(),
        agent: cfg.agent.clone(),
    })
}

/// What a recovered shell's spawn prepared before its tracer starts: the
/// hook files it wrote and the injector that types its resume command.
#[derive(Default)]
struct ShellResume {
    hook_settings: Option<HookSettings>,
    injector: Option<protocol::PromptInjector>,
}

/// Write `plugin`'s hook files for the recovered shell `session_id`, then ask
/// `plugin` for the command that resumes `conversation_id` with them, typed
/// once the shell's prompt is up. Both take the recovered selection's options
/// and trust from `cfg.extras`, never the plain shell's own. A plugin with no
/// command gets a bare shell and keeps no hook files; a plugin's refusal
/// refuses the spawn and removes the files.
fn prepare_shell_resume(
    hub: &Hub,
    session_id: &str,
    cfg: &SpawnArgs,
    plugin: &dyn AgentPlugin,
    conversation_id: &str,
) -> anyhow::Result<ShellResume> {
    let options = &cfg.extras.hook_options;
    let hook_settings = write_hook_files(hub, session_id, plugin, options, cfg.extras.hook_trusted);
    let files = hook_settings.as_ref().map_or(&[][..], HookSettings::files);
    match plugin.shell_resume(options, conversation_id, files) {
        Ok(Some(command)) => Ok(ShellResume {
            hook_settings,
            injector: Some(shell_resume_injector(command)),
        }),
        Ok(None) => {
            if let Some(settings) = hook_settings {
                settings.discard(session_id);
            }
            Ok(ShellResume::default())
        }
        Err(err) => {
            if let Some(settings) = hook_settings {
                settings.discard(session_id);
            }
            Err(anyhow!(
                "{} cannot resume conversation {conversation_id} in a shell: {err}",
                plugin.descriptor().label
            ))
        }
    }
}

/// The injector that types a recovered shell's resume `command` once its
/// prompt has printed, or after [`SHELL_RESUME_DELAY_MS`].
fn shell_resume_injector(command: String) -> protocol::PromptInjector {
    protocol::PromptInjector {
        steps: vec![
            protocol::InjectorStep::Delay {
                ms: SHELL_RESUME_DELAY_MS,
            },
            protocol::InjectorStep::Text {
                content: command,
                newline: true,
            },
        ],
        verify_mode_marker: None,
        startup: protocol::InjectorStartup::ShellPrompt,
    }
}

#[expect(
    clippy::too_many_arguments,
    clippy::needless_pass_by_value,
    reason = "Constructing a session record is naturally wide; the String args are partly \
              consumed and partly cloned, splitting them by ownership clarifies nothing"
)]
fn spawn_headless_session(
    hub: &Hub,
    session_id: String,
    label: String,
    kind: SessionKind,
    members: Vec<SessionMember>,
    workspace_id: Option<String>,
    primary_cwd: PathBuf,
    prompt: String,
    cfg: &SpawnArgs,
    stored_config: protocol::SpawnConfig,
    busy_excluded: bool,
) -> anyhow::Result<protocol::SessionSnapshot> {
    let last_prompt = Some(prompt.clone());
    let initial_cwd = primary_cwd.to_string_lossy().into_owned();
    let spec = headless_spec(cfg, &members, &prompt, primary_cwd)?;
    #[cfg(test)]
    super::child_env_probe::record(&spec.env, &[]);

    let started_at = Utc::now();
    let mut record = SessionRecord {
        id: session_id.clone(),
        label: label.clone(),
        default_label: label.clone(),
        user_label: None,
        kind: kind.clone(),
        members: members.clone(),
        mode: SessionMode::Headless,
        started_at,
        status: SessionStatus::Spawning,
        status_since: Some(started_at),
        exit_code: None,
        metrics: SessionMetrics::default(),
        recent_actions: Vec::new(),
        pty: None,
        headless: None,
        workspace_id: workspace_id.clone(),
        agent: cfg.agent.clone(),
        terminal_title: None,
        program_name: Some(cfg.backend.descriptor().tag.clone()),
        current_cwd: Some(initial_cwd.clone()),
        appearance: AppearanceOverrides::default(),
        spawn_config: Some(stored_config.clone()),
        is_abandoned: false,
        is_inactive: false,
        worktree_paths: worktree_paths_for_config(&stored_config, &members),
        last_prompt: last_prompt.clone(),
        input_notifier: None,
        screen: None,
        scrollback_snapshot_req: None,
        spawn_origin: None,
        conversation_id: None,
        pending_input: None,
        last_hook_time: None,
        hook_driven: false,
        background: false,
        last_assistant_message: None,
        busy_excluded,
        agent_in_shell: false,
        shell_label: None,
        agent_exit_announced: false,
        size_set: false,
        hook_agent: None,
    };
    push_recent_action(&mut record, "headless session started".to_string());
    // Held unpublished until the child runs, so a failed spawn is never
    // broadcast and the first snapshot clients see carries its handle.
    let pending = hub.sessions.insert_pending(record);
    let handle = match headless::spawn(&spec, &hub.sessions, session_id.clone(), hub.dirs.clone()) {
        Ok(handle) => handle,
        Err(err) => {
            pending.discard();
            return Err(err.context(format!("spawning headless {}", cfg.agent.0)));
        }
    };
    let snap = {
        let mut rec = crate::sync::lock(pending.arc());
        rec.headless = Some(Arc::clone(&handle));
        rec.snapshot()
    };
    pending.publish_from(cfg.origin.clone());

    if let Some(pid) = handle.pid()
        && let Ok(mut meta) = orphan::meta_from_record(
            session_id.clone(),
            pid,
            label,
            kind,
            SessionMode::Headless,
            members,
            started_at,
            workspace_id,
            // Same as interactive: the child is a node shim on Windows.
            None,
            Some(cfg.agent.clone()),
            Some(stored_config),
            last_prompt,
            Some(initial_cwd),
            // Headless `claude --print` doesn't use a PTY and therefore doesn't
            // go through rt-tracer; the tracer fields stay None.
            None,
            None,
            None,
            None,
        )
    {
        meta.status = Some(snap.status);
        meta.status_since = snap.status_since;
        write_meta_then_sync(hub, &meta);
    }

    Ok(snap)
}

type SpawnResolution = (
    SessionKind,
    Vec<SessionMember>,
    PathBuf,
    String,
    Option<String>,
);

/// Send a spawn failure to the requesting client as a blocking `ActionFailed`
/// modal, echoing the request's id. Uses the structured `SpawnFailure` fields
/// when present; otherwise surfaces the full anyhow chain under a generic
/// title.
pub(super) fn send_spawn_failure(
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
    err: &anyhow::Error,
    request_id: Option<&str>,
) {
    let msg = err.downcast_ref::<spawn_plan::SpawnFailure>().map_or_else(
        || DaemonMessage::ActionFailed {
            title: "Couldn't start session".to_string(),
            detail: format!("{err:#}"),
            hint: None,
            request_id: request_id.map(str::to_owned),
        },
        |sf| DaemonMessage::ActionFailed {
            title: sf.title.clone(),
            detail: sf.detail.clone(),
            hint: sf.hint.clone(),
            request_id: request_id.map(str::to_owned),
        },
    );
    let _ = out_tx.send(msg);
}

async fn spawn_single(
    hub: &Hub,
    repo_id: &str,
    plan: SinglePlan<'_>,
) -> anyhow::Result<SpawnResolution> {
    let SinglePlan {
        branch_name,
        base_branch,
        use_worktree,
        checkout_strategy,
        worktree_reuse,
        existing_worktree,
    } = plan;
    let repo = hub
        .state
        .with_persisted(|s| s.repos.iter().find(|r| r.id == repo_id).cloned())
        .ok_or_else(|| anyhow!("unknown repo: {repo_id}"))?;
    let repo_path = PathBuf::from(&repo.path);

    // Non-git folder (no `.git`, or `git init` with no commits): skip
    // worktree/branch ops entirely and spawn directly in the folder. The
    // caller-supplied `branch_name` is still recorded on the member so the
    // session label and sidebar grouping stay coherent, but no branch is
    // created or checked out.
    if !crate::git::is_initialized(&repo_path).await {
        if let Some(pinned) = existing_worktree {
            return Err(anyhow!(
                "cannot pin worktree {pinned}: {} is not a git repository",
                repo.name
            ));
        }
        let member = SessionMember {
            repo_id: repo.id.clone(),
            repo_name: repo.name.clone(),
            branch: branch_name.to_string(),
            worktree_path: repo_path.to_string_lossy().into_owned(),
        };
        let label = format!("{}: {branch_name}", repo.name);
        return Ok((SessionKind::Single, vec![member], repo_path, label, None));
    }

    // Base branch used when the target needs to be *created*. See
    // `spawn_plan::resolve_base_for_create` for the priority chain and why an
    // auto-detected default is upgraded to its remote-tracking counterpart.
    let base_for_create =
        spawn_plan::resolve_base_for_create(&hub.state, &repo, base_branch.as_deref()).await;

    let (working_path, member_branch) = if let Some(pinned) = existing_worktree {
        let worktree_path = crate::paths::resolve_existing_dir(&pinned)
            .with_context(|| format!("pinned worktree for {}", repo.name))?;
        // Deliberately no in-use refusal here. The client resolves a pin from
        // a picker that shows which worktrees a live session already holds and
        // confirms before sending one, so the pin *is* the acknowledgement.
        let in_use = active_sessions_using_worktree(hub, &worktree_path.to_string_lossy(), None);
        if !in_use.is_empty() {
            let labels: Vec<&str> = in_use.iter().map(|(_, label)| label.as_str()).collect();
            warn!(
                worktree = %worktree_path.display(),
                sharing_with = ?labels,
                "spawning into a worktree a live session already holds"
            );
        }
        let branch = pinned_branch(&worktree_path, branch_name).await;
        (worktree_path, branch)
    } else if use_worktree {
        let worktree_path =
            single_worktree_target(&hub.state.worktrees_dir(), &repo, branch_name).await?;
        // Refuse to bind a worktree that's already driving a live session.
        // Without this the reuse path below would silently share one worktree
        // across two sessions.
        let in_use = active_sessions_using_worktree(hub, &worktree_path.to_string_lossy(), None);
        if !in_use.is_empty() {
            return Err(worktree_in_use_failure(branch_name, &in_use).into());
        }
        materialize_single_worktree(
            &repo.name,
            &repo_path,
            &worktree_path,
            branch_name,
            &base_for_create,
            worktree_reuse,
        )
        .await?;
        crate::worktrees_admin::mark_group(
            &crate::git::group_dir(&hub.state.worktrees_dir(), branch_name, &repo.name),
            crate::git::GroupKind::Repo,
            &repo.name,
            &[worktree_path.as_path()],
        );
        log_fork_point(&repo_path, branch_name, &base_for_create, &worktree_path).await;
        (worktree_path, branch_name.to_string())
    } else {
        crate::git::checkout_in_place(
            &repo_path,
            branch_name,
            Some(&base_for_create),
            checkout_strategy,
        )
        .await
        .context("checking out branch in-place")?;
        (repo_path.clone(), branch_name.to_string())
    };

    let member = SessionMember {
        repo_id: repo.id.clone(),
        repo_name: repo.name.clone(),
        branch: member_branch.clone(),
        worktree_path: working_path.to_string_lossy().into_owned(),
    };
    let label = format!("{}: {member_branch}", repo.name);
    Ok((SessionKind::Single, vec![member], working_path, label, None))
}

fn spawn_standalone_shell(cwd: Option<&str>) -> anyhow::Result<SpawnResolution> {
    let cwd = resolve_standalone_cwd(cwd)?;
    let label = format!("Shell: {}", path_leaf_label(&cwd));
    Ok((SessionKind::Standalone, Vec::new(), cwd, label, None))
}

/// Whether a standalone (no repo) target can run `mode`: a plain shell, or
/// any registered agent interactively in the folder, with no worktree.
/// Headless sessions still need a repo or workspace.
fn standalone_supports(mode: SessionMode) -> bool {
    match mode {
        SessionMode::PlainShell | SessionMode::Interactive => true,
        SessionMode::Headless => false,
    }
}

/// An agent running in a plain folder, labelled with the folder's name.
fn spawn_standalone_agent(cwd: Option<&str>) -> anyhow::Result<SpawnResolution> {
    let cwd = resolve_standalone_cwd(cwd)?;
    let label = path_leaf_label(&cwd);
    Ok((SessionKind::Standalone, Vec::new(), cwd, label, None))
}

fn resolve_standalone_cwd(cwd: Option<&str>) -> anyhow::Result<PathBuf> {
    let path = match cwd.map(str::trim).filter(|s| !s.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => default_standalone_cwd()?,
    };
    let metadata = std::fs::metadata(&path)
        .with_context(|| format!("reading the standalone folder: {}", path.display()))?;
    if !metadata.is_dir() {
        return Err(anyhow!(
            "standalone path is not a folder: {}",
            path.display()
        ));
    }
    let canonical = std::fs::canonicalize(&path).unwrap_or(path);
    Ok(crate::paths::simplify_path(&canonical))
}

fn default_standalone_cwd() -> anyhow::Result<PathBuf> {
    if let Some(user_dirs) = UserDirs::new() {
        return Ok(user_dirs.home_dir().to_path_buf());
    }
    std::env::current_dir().context("resolving fallback standalone shell directory")
}

fn path_leaf_label(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map_or_else(|| path.display().to_string(), str::to_string)
}

async fn spawn_workspace(
    hub: &Hub,
    workspace_id: &str,
    req: ws::ResolveRequest<'_>,
    worktree_reuse: protocol::WorktreeReusePolicy,
) -> anyhow::Result<SpawnResolution> {
    let branch_name = req.branch_name;
    let use_worktree = req.use_worktree;
    info!(
        workspace_id,
        branch_name, use_worktree, "spawn_workspace: begin"
    );
    let (workspace, resolved) =
        ws::resolve_workspace(&hub.state, &hub.state.worktrees_dir(), workspace_id, req).await?;
    // Refuse to bind any member worktree that's already driving a live
    // session — mirrors the single-repo guard so a workspace re-spawn on a
    // branch whose worktrees already exist can't be shared across sessions.
    // Pinned members are exempt: the client resolves a pin from a picker that
    // shows which worktrees are live and confirms before sending one.
    if use_worktree {
        for member in &resolved {
            let in_use =
                active_sessions_using_worktree(hub, &member.working_path.to_string_lossy(), None);
            if in_use.is_empty() {
                continue;
            }
            if !member.pinned {
                return Err(worktree_in_use_failure(branch_name, &in_use).into());
            }
            let labels: Vec<&str> = in_use.iter().map(|(_, label)| label.as_str()).collect();
            warn!(
                worktree = %member.working_path.display(),
                sharing_with = ?labels,
                "spawning into a worktree a live session already holds"
            );
        }
    }
    ws::ensure_branches(&resolved, branch_name, worktree_reuse)
        .await
        .map_err(|e| anyhow::Error::new(classify_worktree_error(&e, branch_name)))?;
    if use_worktree {
        let created: Vec<&Path> = resolved
            .iter()
            .filter(|m| !m.pinned)
            .map(|m| m.working_path.as_path())
            .collect();
        crate::worktrees_admin::mark_group(
            &crate::git::group_dir(&hub.state.worktrees_dir(), branch_name, &workspace.name),
            crate::git::GroupKind::Workspace,
            &workspace.name,
            &created,
        );
        for member in &resolved {
            if member.pinned {
                continue;
            }
            let repo_path = PathBuf::from(&member.repo.path);
            let base = member.effective_base.clone().unwrap_or_default();
            log_fork_point(&repo_path, branch_name, &base, &member.working_path).await;
        }
    }

    let primary = resolved
        .first()
        .ok_or_else(|| anyhow!("workspace has no members"))?
        .working_path
        .clone();
    let mut members: Vec<SessionMember> = Vec::with_capacity(resolved.len());
    for member in &resolved {
        // A pinned member runs on whatever its worktree already has checked
        // out, which need not be the branch the group's directory is named for.
        let branch = if member.pinned {
            pinned_branch(&member.working_path, branch_name).await
        } else {
            branch_name.to_string()
        };
        members.push(SessionMember {
            repo_id: member.repo.id.clone(),
            repo_name: member.repo.name.clone(),
            branch,
            worktree_path: member.working_path.to_string_lossy().into_owned(),
        });
    }
    let label = format!("{}: {branch_name}", workspace.name);
    Ok((
        SessionKind::Workspace,
        members,
        primary,
        label,
        Some(workspace.id.clone()),
    ))
}

/// The spawn's `extra_env` with each reference (see
/// [`protocol::env_rows::reference`]) replaced by the value it stands for: an
/// env reference by the value `lookup` finds for it, a secret reference by the
/// value saved under its id in Windows Credential Manager. Literals pass
/// through as-is.
///
/// A reference that can't be resolved refuses the spawn, naming the row's key
/// and never the value.
pub(crate) fn resolve_env_refs(
    extra_env: &[(String, String)],
    mut lookup: impl FnMut(&str) -> Option<(user_env::Secret, user_env::Origin)>,
) -> Result<Vec<(String, String)>, spawn_plan::SpawnFailure> {
    extra_env
        .iter()
        .map(|(key, value)| match protocol::env_rows::reference(value) {
            Some(protocol::env_rows::Reference::Env(name)) => {
                let Some((secret, origin)) = lookup(&name) else {
                    return Err(spawn_plan::SpawnFailure {
                        title: "Environment variable not set".to_owned(),
                        detail: format!(
                            "`{name}` isn't set in the daemon's environment, your user environment or the system environment."
                        ),
                        hint: Some(
                            "Set it as a user environment variable, then spawn again.".to_owned(),
                        ),
                    });
                };
                debug!(key = %key, name = %name, ?origin, "spawn env reference resolved");
                Ok((key.clone(), secret.expose().to_owned()))
            }
            Some(protocol::env_rows::Reference::Secret(id)) => {
                let Some(secret) = env_secrets::open(&id) else {
                    return Err(spawn_plan::SpawnFailure {
                        title: "A saved secret is missing".to_owned(),
                        detail: format!(
                            "`{key}`'s saved value is no longer in Windows Credential Manager."
                        ),
                        hint: Some(
                            "Open the spawn dialog, type the value again, and spawn.".to_owned(),
                        ),
                    });
                };
                debug!(key = %key, id = %id, "spawn secret reference resolved");
                Ok((key.clone(), secret.expose().to_owned()))
            }
            None => Ok((key.clone(), value.clone())),
        })
        .collect()
}

/// Everything the daemon can check about a spawn request before it does work
/// it can't undo, on the mode, its agent options (see
/// [`check_plain_shell_options`]), what its agent's plugin refuses and the
/// env rows the request will spawn with: see [`check_env_rows_with`]. The
/// dispatcher runs it before a checkout
/// confirm, and recovery on the planned request before it registers a repo;
/// `spawn_session` seals and resolves again for the child.
pub(super) fn check_spawn_request(request: &SpawnRequest) -> Result<(), spawn_plan::SpawnFailure> {
    check_spawn_request_with(request, user_env::resolve)
}

/// [`check_spawn_request`] with the variable lookup `resolve` in place of
/// [`user_env::resolve`].
pub(super) fn check_spawn_request_with(
    request: &SpawnRequest,
    resolve: impl Fn(&str) -> Option<(user_env::Secret, user_env::Origin)>,
) -> Result<(), spawn_plan::SpawnFailure> {
    check_plain_shell_options(request)?;
    check_env_rows_with(request, resolve)
}

/// A plain shell runs no agent: a request naming one, or carrying
/// `dangerously_skip_permissions`, is refused before any git work or capture.
fn check_plain_shell_options(request: &SpawnRequest) -> Result<(), spawn_plan::SpawnFailure> {
    if request.mode != SessionMode::PlainShell {
        return Ok(());
    }
    let detail = if request.agent.is_some() {
        "A plain shell runs no agent."
    } else if request.dangerously_skip_permissions {
        "Plain shell sessions do not accept dangerously_skip_permissions."
    } else {
        return Ok(());
    };
    Err(spawn_plan::SpawnFailure {
        title: "Couldn't start the shell session".to_owned(),
        detail: detail.to_owned(),
        hint: None,
    })
}

/// The refusal `backend`, the plugin of `request`'s agent, makes of the
/// request decoded as `options`, reading any key it requires through `keys`.
/// A plain shell runs no agent, so its plugin is never asked.
fn check_with_plugin(
    request: &SpawnRequest,
    backend: &dyn AgentPlugin,
    options: &OptionValues,
    keys: &dyn KeyLookup,
) -> Result<(), spawn_plan::SpawnFailure> {
    if request.mode == SessionMode::PlainShell {
        return Ok(());
    }
    let check = SpawnCheck {
        mode: request.mode,
        rows: &request.extra_env,
        keys,
    };
    backend
        .check_request(options, &check)
        .map_err(spawn_plan::SpawnFailure::from)
}

/// The checks behind [`check_spawn_request`]: the request must pass its
/// agent plugin's check (see [`check_with_plugin`]), every secret row must
/// be sealable (see [`env_secrets::check_sealable`]), and every reference
/// resolvable through `resolve`.
fn check_env_rows_with(
    request: &SpawnRequest,
    resolve: impl Fn(&str) -> Option<(user_env::Secret, user_env::Origin)>,
) -> Result<(), spawn_plan::SpawnFailure> {
    let (_, backend, options) = request_agent(request)?;
    check_with_plugin(request, backend, &options, &resolve)?;
    env_secrets::check_sealable(&request.extra_env, &request.secret_env_keys)?;
    resolve_env_refs(&request.extra_env, &resolve)?;
    Ok(())
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use crate::history;
    use crate::server::child_env_probe;
    use crate::server::recovery::resume_abandoned;
    use crate::server::sessions::{DuplicateAsk, duplicate_session, stop_session};
    use crate::server::test_support::{
        DEEPSEEK_KEY_SENTINEL, MISSING_SECRET_ID, ScratchDir, UNSET_REF, deepseek_key,
        deepseek_spawn, dispatch_one, env, env_ref_repo_hub, env_spawn, git_init, git_ok,
        hook_settings_file, hook_test_hub, idle_record, lookup_a, no_key, repo_entry, secret_ref,
        spawnless_test_hub, test_hub, workspace_entry,
    };
    use protocol::SessionEnd;
    use tokio::sync::broadcast;

    /// The text after `id: ` on the first line that holds it.
    #[cfg(windows)]
    fn after_id(text: &str) -> Option<String> {
        text.lines()
            .find_map(|line| line.trim().strip_prefix("id: "))
            .map(|id| id.trim().to_owned())
    }

    /// The spawn arguments of a test agent spawn with its default options.
    fn test_agent_spawn_args() -> SpawnArgs {
        SpawnArgs {
            dangerously_skip_permissions: false,
            agent: agent_api::test_agent::DESCRIPTOR.id.clone(),
            backend: Box::leak(Box::new(agent_api::test_agent::TestAgent::new(
                ConversationPlan::None,
            ))),
            options: [
                (
                    "mode".to_owned(),
                    protocol::OptionValue::Text("m".to_owned()),
                ),
                ("sandbox".to_owned(), protocol::OptionValue::Bool(false)),
            ]
            .into_iter()
            .collect(),
            env: agent_api::EnvPlan::default(),
            prompt_injector: None,
            origin: None,
            resume_conversation: None,
            add_dirs: Vec::new(),
            extras: SpawnExtras::default(),
        }
    }

    /// The conversation `planned_conversation` plans for a test agent whose
    /// plan pre-creates it by running `script` in `powershell.exe`.
    #[cfg(windows)]
    async fn planned_by_pre_create(script: &str) -> (Option<String>, Option<String>) {
        let agent = Box::leak(Box::new(agent_api::test_agent::TestAgent::new(
            ConversationPlan::PreCreate {
                args: vec![
                    "-NoProfile".to_owned(),
                    "-Command".to_owned(),
                    script.to_owned(),
                ],
                parse: after_id,
            },
        )));
        let cfg = test_agent_spawn_args();
        let scratch = ScratchDir::new("pre-create-plan");
        let (id, resume, _) =
            planned_conversation(agent, &cfg, "powershell.exe", &[], scratch.path(), &[]).await;
        (id, resume)
    }

    #[tokio::test]
    async fn discover_after_launch_spawn_registers_a_waiter_with_its_baseline() {
        let agent: &'static agent_api::test_agent::TestAgent = Box::leak(Box::new(
            agent_api::test_agent::TestAgent::new(ConversationPlan::DiscoverAfterLaunch),
        ));
        let scratch = ScratchDir::new("discover-plan");
        agent.set_found(
            &scratch.path().to_string_lossy(),
            vec![agent_api::FoundConversation {
                id: "before-1".to_owned(),
                last_active: Utc::now(),
                title: None,
            }],
        );
        let cfg = test_agent_spawn_args();
        let (id, resume, discover) =
            planned_conversation(agent, &cfg, "test-agent.exe", &[], scratch.path(), &[]).await;
        assert_eq!((id, resume), (None, None));
        let baseline = discover.expect("a discover plan takes a baseline");

        let dirs = history::test_support::scratch_dirs("discover-plan");
        let registry = crate::session::SessionRegistry::new(dirs.clone());
        registry.insert(history::test_support::record(
            "s1",
            SessionMode::Interactive,
        ));
        assert!(register_discovery(&registry, "s1", Some(baseline)));

        let waiter = registry
            .discovery()
            .waiter("s1")
            .expect("a registered waiter");
        let expected: std::collections::HashSet<String> = ["before-1".to_owned()].into();
        assert_eq!(waiter.baseline(), &expected);
    }

    #[tokio::test]
    async fn a_conversation_created_during_start_up_is_claimed() {
        let agent: &'static agent_api::test_agent::TestAgent = Box::leak(Box::new(
            agent_api::test_agent::TestAgent::new(ConversationPlan::DiscoverAfterLaunch),
        ));
        let scratch = ScratchDir::new("discover-start-up");
        let cfg = test_agent_spawn_args();
        let (_, _, discover) =
            planned_conversation(agent, &cfg, "test-agent.exe", &[], scratch.path(), &[]).await;
        let baseline = discover.expect("a discover plan takes a baseline");
        let launched_at = baseline.launched_at;

        let dirs = history::test_support::scratch_dirs("discover-start-up");
        let registry = crate::session::SessionRegistry::new(dirs.clone());
        let mut record = history::test_support::record("s1", SessionMode::Interactive);
        record.started_at = launched_at + chrono::TimeDelta::seconds(1);
        registry.insert(record);
        agent.set_found(
            &scratch.path().to_string_lossy(),
            vec![agent_api::FoundConversation {
                id: "c-start".to_owned(),
                last_active: launched_at + chrono::TimeDelta::milliseconds(500),
                title: None,
            }],
        );
        assert!(register_discovery(&registry, "s1", Some(baseline)));
        let waiter = registry.discovery().waiter("s1").expect("the waiter");

        discovery::pass(
            &registry,
            &dirs,
            &waiter,
            Utc::now(),
            discovery::PassBy::Poller,
        );

        let rec = registry.get("s1").expect("the record");
        assert_eq!(
            crate::sync::lock(&rec).conversation_id.as_deref(),
            Some("c-start")
        );
    }

    #[tokio::test]
    async fn a_child_that_exits_at_once_has_its_waiter_seen_by_the_final_look() {
        let agent: &'static agent_api::test_agent::TestAgent = Box::leak(Box::new(
            agent_api::test_agent::TestAgent::new(ConversationPlan::DiscoverAfterLaunch),
        ));
        let folder = r"D:\exit-at-once";
        let baseline = Baseline {
            ids: std::collections::HashSet::new(),
            plugin: agent,
            rows: agent_api::ResolvedRows::default(),
            folder: PathBuf::from(folder),
            launched_at: Utc::now() - chrono::TimeDelta::minutes(1),
        };
        agent.set_found(
            folder,
            vec![agent_api::FoundConversation {
                id: "c-quick".to_owned(),
                last_active: Utc::now(),
                title: None,
            }],
        );
        let dirs = history::test_support::scratch_dirs("discover-exit-at-once");
        let registry = crate::session::SessionRegistry::new(dirs.clone());

        assert!(register_discovery(&registry, "s1", Some(baseline)));
        let (pty, exit) = history::test_support::fake_pty();
        history::test_support::insert_live(
            &registry,
            &dirs,
            history::test_support::record("s1", SessionMode::Interactive),
            &pty,
        );
        exit.send(crate::pty::PtyExit::Code(0))
            .expect("the exit watcher listens");

        let entry = history::test_support::wait_for_entry(&dirs, "s1").await;
        assert_eq!(entry.conversation_id.as_deref(), Some("c-quick"));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn planned_conversation_passes_a_pre_created_id_as_conversation_id() {
        let planned = planned_by_pre_create("[Console]::Out.WriteLine('id: pre-1')").await;
        assert_eq!(planned, (Some("pre-1".to_owned()), None));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn a_failed_pre_create_spawns_without_a_conversation() {
        assert_eq!(planned_by_pre_create("exit 3").await, (None, None));
    }

    /// A hub with one `git init`ed repo `r1` and a workspace `w1` holding it.
    fn hub_with_previewable_repo(tag: &str) -> (Hub, ScratchDir) {
        let (hub, scratch) = test_hub(tag);
        let repo = scratch.path().join("repo");
        std::fs::create_dir_all(&repo).expect("create repo dir");
        git_init(&repo);
        hub.state
            .mutate(|s| {
                s.repos.push(protocol::RepoEntry {
                    id: "r1".to_string(),
                    name: "r1".to_string(),
                    path: repo.to_string_lossy().into_owned(),
                    default_branch: None,
                    default_use_worktree: true,
                    appearance: AppearanceOverrides::default(),
                    last_agent: None,
                    last_spawn_config: None,
                });
                s.workspaces.push(protocol::WorkspaceEntry {
                    id: "w1".to_string(),
                    name: "w1".to_string(),
                    member_repo_ids: vec!["r1".to_string()],
                    linked_vscode_workspace: None,
                    watch_vscode_workspace: false,
                    default_use_worktree: true,
                    appearance: AppearanceOverrides::default(),
                    last_spawn_config: None,
                });
            })
            .expect("register repo and workspace");
        (hub, scratch)
    }

    #[tokio::test]
    async fn a_spawn_preview_echoes_its_request_id() {
        let (hub, _scratch) = hub_with_previewable_repo("preview-rid");
        let msg = dispatch_one(
            &hub,
            ClientMessage::PreviewSpawn {
                repo_id: "r1".to_string(),
                branch_name: "feat".to_string(),
                base_branch: None,
                use_worktree: true,
                request_id: Some("p1".to_string()),
            },
        )
        .await;
        assert!(
            matches!(
                &msg,
                DaemonMessage::SpawnPreview { repo_id, request_id: Some(id), .. }
                    if id == "p1" && repo_id == "r1"
            ),
            "{msg:?}"
        );
    }

    #[tokio::test]
    async fn a_failed_spawn_preview_echoes_its_request_id() {
        let (hub, _scratch) = test_hub("preview-fail-rid");
        let msg = dispatch_one(
            &hub,
            ClientMessage::PreviewSpawn {
                repo_id: "no-such-repo".to_string(),
                branch_name: "feat".to_string(),
                base_branch: None,
                use_worktree: true,
                request_id: Some("p2".to_string()),
            },
        )
        .await;
        assert!(
            matches!(
                &msg,
                DaemonMessage::Error { message, request_id: Some(id) }
                    if id == "p2" && message.contains("no-such-repo")
            ),
            "{msg:?}"
        );
    }

    #[tokio::test]
    async fn a_workspace_spawn_preview_echoes_its_request_id() {
        let (hub, _scratch) = hub_with_previewable_repo("ws-preview-rid");
        let msg = dispatch_one(
            &hub,
            ClientMessage::PreviewWorkspaceSpawn {
                workspace_id: "w1".to_string(),
                branch_name: "feat".to_string(),
                base_branch: None,
                request_id: Some("p3".to_string()),
            },
        )
        .await;
        assert!(
            matches!(
                &msg,
                DaemonMessage::WorkspaceSpawnPreview { workspace_id, request_id: Some(id), .. }
                    if id == "p3" && workspace_id == "w1"
            ),
            "{msg:?}"
        );
    }

    #[tokio::test]
    async fn a_failed_workspace_spawn_preview_echoes_its_request_id() {
        let (hub, _scratch) = test_hub("ws-preview-fail-rid");
        let msg = dispatch_one(
            &hub,
            ClientMessage::PreviewWorkspaceSpawn {
                workspace_id: "no-such-workspace".to_string(),
                branch_name: "feat".to_string(),
                base_branch: None,
                request_id: Some("p4".to_string()),
            },
        )
        .await;
        assert!(
            matches!(
                &msg,
                DaemonMessage::Error { request_id: Some(id), .. } if id == "p4"
            ),
            "{msg:?}"
        );
    }

    /// Run a real interactive Claude spawn for `session_id` in `folder` on
    /// `hub`, which gets as far as the stub tracer and fails there, and answer
    /// the error's whole chain.
    async fn failing_interactive_claude_spawn(
        hub: &Hub,
        folder: &Path,
        session_id: &str,
    ) -> String {
        let request = env_spawn(
            SpawnTarget::Standalone {
                cwd: Some(folder.to_string_lossy().into_owned()),
                add_dirs: Vec::new(),
            },
            SessionMode::Interactive,
            Vec::new(),
            None,
        );
        let stored_config = protocol::SpawnConfig::from_request(&request);
        let cfg = SpawnArgs {
            dangerously_skip_permissions: false,
            agent: agent_registry::default_agent(),
            backend: claude_backend(),
            options: claude_options(),
            env: agent_api::EnvPlan::default(),
            prompt_injector: None,
            origin: None,
            resume_conversation: None,
            add_dirs: Vec::new(),
            extras: SpawnExtras::default(),
        };
        format!(
            "{:#}",
            spawn_interactive_session(
                hub,
                session_id.to_owned(),
                "cleanup".to_owned(),
                SessionKind::Standalone,
                Vec::new(),
                None,
                folder.to_path_buf(),
                None,
                &cfg,
                stored_config,
                false,
            )
            .await
            .expect_err("the stub tracer is not a runnable executable")
        )
    }

    #[tokio::test]
    async fn an_unsplittable_launch_command_refuses_the_interactive_spawn() {
        let (hub, scratch) = spawnless_test_hub("launch-command-split");
        let folder = scratch.path().join("work");
        std::fs::create_dir_all(&folder).expect("create the folder");
        // Stored past the save check, as an older or hand-edited state.json can.
        hub.state
            .mutate(|s| {
                s.agent_commands.insert(
                    agent_registry::default_agent(),
                    crate::state::AgentCommand {
                        program: String::new(),
                        args: "--debug 'open".to_owned(),
                    },
                );
            })
            .expect("store the command");
        let session_id = "launch-command-split-session";

        let error = failing_interactive_claude_spawn(&hub, &folder, session_id).await;

        assert!(
            error.contains(
                "The Claude launch command's extra arguments cannot be split: missing closing quote. Fix them in Settings."
            ),
            "{error}"
        );
        assert!(
            !hub.dirs.sessions_dir.join(session_id).exists(),
            "the refused spawn wrote nothing"
        );
    }

    #[test]
    fn a_headless_launch_ignores_the_launch_command() {
        let (hub, _scratch) = test_hub("launch-command-headless");
        hub.state
            .set_agent_command(
                &agent_registry::default_agent(),
                crate::state::AgentCommand {
                    program: "my-claude".to_owned(),
                    args: "--debug".to_owned(),
                },
            )
            .expect("store the command");
        let cfg = SpawnArgs {
            dangerously_skip_permissions: false,
            agent: agent_registry::default_agent(),
            backend: claude_backend(),
            options: claude_options(),
            env: agent_api::EnvPlan::default(),
            prompt_injector: None,
            origin: None,
            resume_conversation: None,
            add_dirs: Vec::new(),
            extras: SpawnExtras::default(),
        };
        let backend =
            agent_registry::plugin(&agent_registry::default_agent()).expect("Claude is registered");

        let spec = headless_spec(&cfg, &[], "hello", PathBuf::from("C:/work"))
            .expect("Claude has a headless mode");

        assert_eq!(spec.program, backend.resolve_program());
        let own = backend
            .headless_args(&cfg.options, &cfg.common(), &[], "hello")
            .expect("the options decode")
            .expect("Claude has a headless mode");
        assert_eq!(spec.args, own, "only the plugin's own arguments");
    }

    #[tokio::test]
    async fn a_stored_launch_command_is_not_captured_in_the_spawn_config() {
        let (hub, _scratch, _repo) = env_ref_repo_hub("launch-command-config");
        hub.state
            .set_agent_command(
                &agent_registry::default_agent(),
                crate::state::AgentCommand {
                    program: "my-claude-wrapper".to_owned(),
                    args: "--launch-extra-marker".to_owned(),
                },
            )
            .expect("store the command");
        let mut target = single_target(false, None);
        if let SpawnTarget::Single { branch_name, .. } = &mut target {
            "main".clone_into(branch_name);
        }
        spawn_session(
            &hub,
            env_spawn(target, SessionMode::Interactive, Vec::new(), None),
            None,
            false,
        )
        .await
        .expect_err("no tracer can start");

        let stored = hub
            .state
            .with_persisted(|s| s.repos[0].last_spawn_config.clone())
            .expect("last spawn config stored");
        let stored = serde_json::to_string(&stored).expect("encode the spawn config");
        assert!(!stored.contains("my-claude-wrapper"), "{stored}");
        assert!(!stored.contains("--launch-extra-marker"), "{stored}");
    }

    /// The spawn arguments of a recovered shell that resumes Claude
    /// conversation `abc-123`.
    fn shell_resume_spawn_args() -> SpawnArgs {
        SpawnArgs {
            dangerously_skip_permissions: false,
            agent: agent_registry::default_agent(),
            backend: claude_backend(),
            options: claude_options(),
            env: agent_api::EnvPlan::default(),
            prompt_injector: None,
            origin: None,
            resume_conversation: None,
            add_dirs: Vec::new(),
            extras: SpawnExtras {
                hook_agent: Some(agent_registry::default_agent()),
                resume_conversation: Some("abc-123".to_owned()),
                hook_options: claude_options(),
                hook_trusted: false,
            },
        }
    }

    /// Run a real plain-shell recovery spawn for `session_id` in `folder` on
    /// `hub`, which gets as far as the stub tracer and fails there, and answer
    /// the error's whole chain.
    async fn failing_plain_shell_recovery_spawn(
        hub: &Hub,
        folder: &Path,
        session_id: &str,
    ) -> String {
        let request = env_spawn(
            SpawnTarget::Standalone {
                cwd: Some(folder.to_string_lossy().into_owned()),
                add_dirs: Vec::new(),
            },
            SessionMode::PlainShell,
            Vec::new(),
            None,
        );
        let stored_config = protocol::SpawnConfig::from_request(&request);
        format!(
            "{:#}",
            spawn_plain_shell_session(
                hub,
                session_id.to_owned(),
                "cleanup".to_owned(),
                SessionKind::Standalone,
                Vec::new(),
                None,
                folder.to_path_buf(),
                &shell_resume_spawn_args(),
                stored_config,
                false,
            )
            .await
            .expect_err("the stub tracer is not a runnable executable")
        )
    }

    /// Claude's options as a spawn request sends them.
    fn claude_options() -> protocol::OptionValues {
        OptionValues::from([
            ("permission_mode".to_owned(), protocol::OptionValue::Null),
            (
                "provider".to_owned(),
                protocol::OptionValue::Text("anthropic".to_owned()),
            ),
            ("model".to_owned(), protocol::OptionValue::Null),
        ])
    }

    /// Claude's plugin.
    fn claude_backend() -> &'static dyn AgentPlugin {
        agent_registry::plugin(&agent_registry::default_agent()).expect("Claude is registered")
    }

    /// Claude with its default options.
    fn claude_selection() -> protocol::AgentSelection {
        protocol::AgentSelection {
            agent: agent_registry::default_agent(),
            options: OptionValues::new(),
        }
    }

    #[tokio::test]
    async fn a_failed_tracer_spawn_removes_its_hook_settings() {
        let (hub, scratch) = hook_test_hub("hook-settings-cleanup");
        let folder = scratch.path().join("folder");
        std::fs::create_dir_all(&folder).expect("create folder");
        let session_id = "cleanup-session";
        let file = hook_settings_file(&hub, session_id);

        // The writer itself works on this hub: it answers `Some` and leaves the
        // file on disk. Discarding the probe first keeps the assertions after
        // the spawn about what the spawn wrote, not about the probe.
        let probe = write_hook_settings(
            &hub,
            session_id,
            &agent_registry::default_agent(),
            &claude_options(),
            false,
        )
        .expect("this hub resolves a cached tracer");
        assert!(file.exists(), "the writer wrote the settings file");
        probe.discard(session_id);
        assert!(!file.exists(), "the probe is removed");

        // A real interactive Claude spawn at that same id goes through the
        // writer and then fails at the stub tracer, which is not runnable.
        let error = failing_interactive_claude_spawn(&hub, &folder, session_id).await;
        assert!(error.contains("spawning claude via tracer"), "{error}");
        assert!(
            !file.exists(),
            "the failed spawn removed the settings file it wrote"
        );
        assert!(
            !hub.dirs.sessions_dir.join(session_id).exists(),
            "and the session dir it created"
        );

        // A file already at the id is gone afterwards only if the spawn itself
        // rewrote and then removed it — a spawn that skipped the writer would
        // leave the marker behind. The dir pre-existed, so this spawn keeps it.
        let marker = write_hook_settings(
            &hub,
            session_id,
            &agent_registry::default_agent(),
            &claude_options(),
            false,
        )
        .expect("writes the marker file");
        assert!(file.exists(), "the marker is there before the second spawn");
        let error = failing_interactive_claude_spawn(&hub, &folder, session_id).await;
        assert!(error.contains("spawning claude via tracer"), "{error}");
        drop(marker);
        assert!(
            !file.exists(),
            "the failed spawn rewrote and removed the marked file"
        );
        assert!(
            hub.dirs.sessions_dir.join(session_id).exists(),
            "a directory this spawn did not create is kept"
        );
    }

    /// A plugin that writes Claude's hook files and answers a shell resume
    /// as scripted: `Ok(true)` a command naming every hook file it was
    /// handed (refusing when one is missing on disk), `Ok(false)` none, and
    /// `Err` a refusal.
    struct ScriptedResume {
        answer: Result<bool, String>,
    }

    impl agent_api::Agent for ScriptedResume {
        type Options = serde_json::Value;

        fn descriptor(&self) -> &protocol::AgentDescriptor {
            &agent_api::test_agent::DESCRIPTOR
        }

        fn program_env_var(&self) -> &'static str {
            "RUSTLING_TULIP_SCRIPTED_RESUME"
        }

        fn default_program(&self) -> &'static str {
            "scripted-resume"
        }

        fn liveness_tokens(&self) -> &'static [&'static str] {
            &[]
        }

        fn recovery_options(&self, run: &agent_api::RecoveredRun<'_>) -> serde_json::Value {
            run.recorded_as().unwrap_or_default()
        }

        fn check_request(
            &self,
            _opts: &serde_json::Value,
            _req: &SpawnCheck<'_>,
        ) -> Result<(), agent_api::Refusal> {
            Ok(())
        }

        fn spawn_env(
            &self,
            _opts: &serde_json::Value,
            _rows: &ResolvedRows,
            _keys: &dyn KeyLookup,
        ) -> Result<EnvPlan, agent_api::Refusal> {
            Ok(EnvPlan::default())
        }

        fn conversation_plan(
            &self,
            _opts: &serde_json::Value,
            _resume: Option<&str>,
        ) -> ConversationPlan {
            ConversationPlan::None
        }

        fn interactive_args(
            &self,
            _opts: &serde_json::Value,
            _ctx: &SpawnContext<'_>,
            _members: &[SessionMember],
            _initial_prompt: Option<&str>,
        ) -> Vec<String> {
            Vec::new()
        }

        fn hook_plan(
            &self,
            _opts: &serde_json::Value,
            trusted: bool,
            shim: &agent_api::HookShim<'_>,
        ) -> agent_api::HookPlan {
            agent_registry::plugin(&agent_registry::default_agent())
                .expect("Claude is registered")
                .hook_plan(&claude_options(), trusted, shim)
                .expect("Claude's default options decode")
        }

        fn shell_resume(
            &self,
            _opts: &serde_json::Value,
            conversation_id: &str,
            hook_files: &[PathBuf],
        ) -> Result<Option<String>, agent_api::AgentError> {
            match &self.answer {
                Ok(true) if hook_files.iter().all(|file| file.exists()) => {
                    let files: Vec<String> = hook_files
                        .iter()
                        .map(|file| file.display().to_string())
                        .collect();
                    Ok(Some(format!(
                        "scripted --hooks {} --resume {conversation_id}",
                        files.join(",")
                    )))
                }
                Ok(true) => Err(agent_api::AgentError::new("a hook file is missing")),
                Ok(false) => Ok(None),
                Err(message) => Err(agent_api::AgentError::new(message.clone())),
            }
        }
    }

    /// The injector that types `command` into a recovered shell.
    fn typed(command: String) -> Vec<protocol::InjectorStep> {
        vec![
            protocol::InjectorStep::Delay {
                ms: SHELL_RESUME_DELAY_MS,
            },
            protocol::InjectorStep::Text {
                content: command,
                newline: true,
            },
        ]
    }

    #[tokio::test]
    async fn shell_resume_types_the_plugins_command_after_writing_the_hook_files() {
        let (hub, _scratch) = hook_test_hub("shell-resume-command");
        let file = hook_settings_file(&hub, "s1");
        let resume = prepare_shell_resume(
            &hub,
            "s1",
            &shell_resume_spawn_args(),
            &ScriptedResume { answer: Ok(true) },
            "abc-123",
        )
        .expect("the plugin answers a command");
        assert!(file.exists(), "the hook file is written");
        let settings = resume.hook_settings.as_ref().expect("hooks were written");
        assert_eq!(settings.files(), std::slice::from_ref(&file));
        let injector = resume.injector.expect("a command to type");
        assert_eq!(injector.startup, protocol::InjectorStartup::ShellPrompt);
        assert_eq!(
            injector.steps,
            typed(format!(
                "scripted --hooks {} --resume abc-123",
                file.display()
            ))
        );

        // Claude's own command names the same file, quoted.
        let resume = prepare_shell_resume(
            &hub,
            "s2",
            &shell_resume_spawn_args(),
            agent_registry::plugin(&agent_registry::default_agent()).expect("Claude is registered"),
            "abc-123",
        )
        .expect("Claude answers a command");
        let file = hook_settings_file(&hub, "s2");
        assert_eq!(
            resume.injector.expect("a command to type").steps,
            typed(format!(
                "claude --settings \"{}\" --resume abc-123",
                file.display()
            ))
        );
    }

    #[tokio::test]
    async fn shell_resume_err_refuses_the_spawn_and_removes_the_hook_files() {
        let (hub, _scratch) = hook_test_hub("shell-resume-refused");
        let error = prepare_shell_resume(
            &hub,
            "s1",
            &shell_resume_spawn_args(),
            &ScriptedResume {
                answer: Err("Scripted will not resume here".to_owned()),
            },
            "abc-123",
        )
        .err()
        .expect("the plugin's refusal refuses the spawn");
        assert!(
            format!("{error:#}").contains("Scripted will not resume here"),
            "{error:#}"
        );
        assert!(
            !hook_settings_file(&hub, "s1").exists(),
            "the refused spawn removed the hook file it wrote"
        );
        assert!(
            !hub.dirs.sessions_dir.join("s1").exists(),
            "and the session dir it created"
        );
    }

    #[tokio::test]
    async fn shell_resume_none_spawns_a_bare_shell() {
        let (hub, _scratch) = hook_test_hub("shell-resume-none");
        let resume = prepare_shell_resume(
            &hub,
            "s1",
            &shell_resume_spawn_args(),
            &ScriptedResume { answer: Ok(false) },
            "abc-123",
        )
        .expect("no command is not a refusal");
        assert!(resume.injector.is_none(), "nothing is typed");
        assert!(resume.hook_settings.is_none(), "a bare shell gets no hooks");
        assert!(!hook_settings_file(&hub, "s1").exists());
        assert!(!hub.dirs.sessions_dir.join("s1").exists());
    }

    /// A plugin whose hook file records the options and trust its hook plan
    /// was handed, and whose resume command names the options it was handed.
    struct EchoOptions;

    impl agent_api::Agent for EchoOptions {
        type Options = serde_json::Value;

        fn descriptor(&self) -> &protocol::AgentDescriptor {
            &agent_api::test_agent::DESCRIPTOR
        }

        fn program_env_var(&self) -> &'static str {
            "RUSTLING_TULIP_ECHO_OPTIONS"
        }

        fn default_program(&self) -> &'static str {
            "echo-options"
        }

        fn liveness_tokens(&self) -> &'static [&'static str] {
            &[]
        }

        fn recovery_options(&self, run: &agent_api::RecoveredRun<'_>) -> serde_json::Value {
            run.recorded_as().unwrap_or_default()
        }

        fn check_request(
            &self,
            _opts: &serde_json::Value,
            _req: &SpawnCheck<'_>,
        ) -> Result<(), agent_api::Refusal> {
            Ok(())
        }

        fn spawn_env(
            &self,
            _opts: &serde_json::Value,
            _rows: &ResolvedRows,
            _keys: &dyn KeyLookup,
        ) -> Result<EnvPlan, agent_api::Refusal> {
            Ok(EnvPlan::default())
        }

        fn conversation_plan(
            &self,
            _opts: &serde_json::Value,
            _resume: Option<&str>,
        ) -> ConversationPlan {
            ConversationPlan::None
        }

        fn interactive_args(
            &self,
            _opts: &serde_json::Value,
            _ctx: &SpawnContext<'_>,
            _members: &[SessionMember],
            _initial_prompt: Option<&str>,
        ) -> Vec<String> {
            Vec::new()
        }

        fn hook_plan(
            &self,
            opts: &serde_json::Value,
            trusted: bool,
            _shim: &agent_api::HookShim<'_>,
        ) -> agent_api::HookPlan {
            agent_api::HookPlan {
                files: vec![agent_api::HookFile {
                    name: "echo.json".to_owned(),
                    contents: serde_json::json!({ "options": opts, "trusted": trusted })
                        .to_string(),
                }],
                args: Vec::new(),
            }
        }

        fn shell_resume(
            &self,
            opts: &serde_json::Value,
            conversation_id: &str,
            _hook_files: &[PathBuf],
        ) -> Result<Option<String>, agent_api::AgentError> {
            Ok(Some(format!("echo {opts} --resume {conversation_id}")))
        }
    }

    /// A recovered shell's spawn arguments whose own options are empty and
    /// untrusted, resuming with `hook_options` and `hook_trusted`.
    fn echo_resume_spawn_args(hook_options: OptionValues, hook_trusted: bool) -> SpawnArgs {
        let mut cfg = shell_resume_spawn_args();
        cfg.options = OptionValues::new();
        cfg.dangerously_skip_permissions = false;
        cfg.extras.hook_options = hook_options;
        cfg.extras.hook_trusted = hook_trusted;
        cfg
    }

    /// The hook file `resume` wrote, read back as JSON.
    fn echoed_hook_file(resume: &ShellResume) -> serde_json::Value {
        let settings = resume.hook_settings.as_ref().expect("hooks were written");
        assert_eq!(settings.files().len(), 1, "{:?}", settings.files());
        let file = settings.files().first().expect("one hook file");
        let text = std::fs::read_to_string(file).expect("read the hook file");
        serde_json::from_str(&text).expect("the hook file is JSON")
    }

    #[tokio::test]
    async fn shell_resume_passes_the_recovered_options_not_the_shells_defaults() {
        let (hub, _scratch) = hook_test_hub("shell-resume-recovered-options");
        let recovered = OptionValues::from([(
            "mode".to_owned(),
            protocol::OptionValue::Text("recovered-marker".to_owned()),
        )]);
        let resume = prepare_shell_resume(
            &hub,
            "s1",
            &echo_resume_spawn_args(recovered, false),
            &EchoOptions,
            "abc-123",
        )
        .expect("the plugin answers a command");

        assert_eq!(
            echoed_hook_file(&resume)["options"],
            serde_json::json!({ "mode": "recovered-marker" }),
            "the hook plan gets the recovered options"
        );
        assert_eq!(
            resume.injector.expect("a command to type").steps,
            typed(r#"echo {"mode":"recovered-marker"} --resume abc-123"#.to_owned()),
            "the resume command gets the recovered options"
        );
    }

    #[tokio::test]
    async fn shell_resume_hook_plan_gets_the_recovered_trust() {
        let (hub, _scratch) = hook_test_hub("shell-resume-recovered-trust");
        for trusted in [true, false] {
            let session_id = format!("s-{trusted}");
            let resume = prepare_shell_resume(
                &hub,
                &session_id,
                &echo_resume_spawn_args(OptionValues::new(), trusted),
                &EchoOptions,
                "abc-123",
            )
            .expect("the plugin answers a command");
            assert_eq!(
                echoed_hook_file(&resume)["trusted"],
                serde_json::Value::Bool(trusted),
                "the plain shell is untrusted; the hook plan gets the recovered run's trust"
            );
        }
    }

    #[tokio::test]
    async fn a_failed_plain_shell_tracer_spawn_removes_its_hook_settings() {
        let (hub, scratch) = hook_test_hub("shell-hook-settings-cleanup");
        let folder = scratch.path().join("folder");
        std::fs::create_dir_all(&folder).expect("create folder");
        let session_id = "cleanup-shell";
        let file = hook_settings_file(&hub, session_id);

        let error = failing_plain_shell_recovery_spawn(&hub, &folder, session_id).await;
        assert!(error.contains("spawning plain shell via tracer"), "{error}");
        assert!(
            !file.exists(),
            "the failed spawn removed the settings file it wrote"
        );
        assert!(
            !hub.dirs.sessions_dir.join(session_id).exists(),
            "and the session dir it created"
        );

        // A marker at the id is gone afterwards only if the spawn itself
        // rewrote and then removed it — a spawn that skipped the writer would
        // leave it behind. The dir pre-existed, so this spawn keeps it.
        let dir = hub.dirs.sessions_dir.join(session_id);
        std::fs::create_dir_all(&dir).expect("pre-create the session dir");
        std::fs::write(&file, b"stale marker").expect("write the marker");
        let error = failing_plain_shell_recovery_spawn(&hub, &folder, session_id).await;
        assert!(error.contains("spawning plain shell via tracer"), "{error}");
        assert!(
            !file.exists(),
            "the failed spawn rewrote and removed the marked file"
        );
        assert!(
            dir.exists(),
            "a directory this spawn did not create is kept"
        );
    }

    /// A graceful stop removes the hook settings file a spawn wrote, its
    /// sidecar, and the session directory once that file was all it held.
    #[tokio::test]
    async fn a_graceful_stop_removes_the_hook_settings_file() {
        use crate::history::test_support::{fake_pty, insert_live, record, write_meta_for};
        let (hub, _scratch) = test_hub("stop-hook-settings");
        let (pty, _exit_tx) = fake_pty();
        insert_live(
            &hub.sessions,
            &hub.dirs,
            record("s1", SessionMode::Interactive),
            &pty,
        );
        write_meta_for(&hub.dirs, "s1");
        let file = hook_settings_file(&hub, "s1");
        std::fs::write(&file, b"{\"hooks\":{}}").expect("write hook settings");

        stop_session(&hub, "s1").await.expect("stop");

        assert!(
            !file.exists(),
            "a graceful stop removes the hook settings file"
        );
        assert!(
            orphan::load_meta(&hub.dirs, "s1").is_err(),
            "and the session no longer appears as an orphan"
        );
        assert!(
            !hub.dirs.sessions_dir.join("s1").exists(),
            "and its directory, which held nothing else"
        );
    }

    #[test]
    fn a_whole_value_env_reference_resolves() {
        let resolved = resolve_env_refs(&env(&[("K", "${env:A}")]), lookup_a).expect("resolves");
        assert_eq!(resolved, env(&[("K", "va")]));
    }

    #[test]
    fn literals_and_embedded_or_invalid_references_pass_through() {
        let extra = env(&[
            ("LIT", "plain"),
            ("EMBEDDED", "x${env:A}y"),
            ("BAD_NAME", "${env:1BAD}"),
            ("NO_NAME", "${env:}"),
            ("UNCLOSED", "${env:A"),
            ("EXTRA_BRACE", "${env:A}}"),
            ("EMPTY", ""),
        ]);
        let resolved = resolve_env_refs(&extra, |_| None).expect("no references");
        assert_eq!(resolved, extra);
    }

    #[test]
    fn an_unset_env_reference_refuses_naming_the_variable() {
        let failure = resolve_env_refs(&env(&[("K", "${env:MISSING_1}")]), lookup_a)
            .expect_err("unset reference refuses");
        assert_eq!(failure.title, "Environment variable not set");
        assert_eq!(
            failure.detail,
            "`MISSING_1` isn't set in the daemon's environment, your user environment or the system environment."
        );
        assert_eq!(
            failure.hint.as_deref(),
            Some("Set it as a user environment variable, then spawn again.")
        );
    }

    /// The reply must be the unset-reference refusal, echoing `rid`.
    #[expect(clippy::panic, reason = "a wrong reply fails the test loudly")]
    fn assert_unset_refusal(reply: DaemonMessage, rid: &str) {
        let DaemonMessage::ActionFailed {
            title,
            detail,
            hint,
            request_id,
        } = reply
        else {
            panic!("expected action_failed, got {reply:?}");
        };
        assert_eq!(title, "Environment variable not set");
        assert!(detail.contains("`RT_TEST_UNSET_ENV_REF_4C1D`"), "{detail}");
        assert!(hint.is_some());
        assert_eq!(request_id.as_deref(), Some(rid));
    }

    #[tokio::test]
    async fn a_worktree_spawn_with_an_unset_env_reference_is_refused_before_any_git_work() {
        let (hub, _scratch, repo) = env_ref_repo_hub("env-ref-unset");
        let req = env_spawn(
            single_target(true, None),
            SessionMode::Interactive,
            env(&[("EXAMPLE_API_KEY", UNSET_REF)]),
            Some("rq-env"),
        );
        // Straight into `spawn_session`, the path every replay (duplicate,
        // resume, recover) takes, past the dispatcher's own early check.
        let err = spawn_session(&hub, req, None, false)
            .await
            .expect_err("an unset reference refuses");
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        send_spawn_failure(&out_tx, &err, Some("rq-env"));
        assert_unset_refusal(out_rx.try_recv().expect("a reply"), "rq-env");
        let worktrees = git_ok(&repo, &["worktree", "list", "--porcelain"]);
        assert_eq!(
            worktrees
                .lines()
                .filter(|l| l.starts_with("worktree "))
                .count(),
            1,
            "only the main worktree: {worktrees}"
        );
        assert_eq!(git_ok(&repo, &["branch", "--list", "wt/x"]).trim(), "");
        let made = std::fs::read_dir(hub.state.worktrees_dir()).map_or(0, Iterator::count);
        assert_eq!(made, 0, "nothing under the worktrees root");
        assert!(hub.sessions.snapshots().is_empty(), "nothing was spawned");
    }

    #[tokio::test]
    async fn a_plain_shell_with_an_agent_refuses() {
        let (hub, _scratch, repo) = env_ref_repo_hub("shell-permission-mode");
        let mut req = env_spawn(
            single_target(true, None),
            SessionMode::PlainShell,
            Vec::new(),
            Some("rq-pm"),
        );
        req.agent = Some(claude_selection());
        // Straight into `spawn_session`, the path every replay (duplicate,
        // resume, recover) takes, past the dispatcher's own early check.
        let err = spawn_session(&hub, req, None, false)
            .await
            .expect_err("a plain shell refuses an agent");
        let text = format!("{err:#}");
        assert!(text.contains("A plain shell runs no agent."), "{text}");
        let worktrees = git_ok(&repo, &["worktree", "list", "--porcelain"]);
        assert_eq!(
            worktrees
                .lines()
                .filter(|l| l.starts_with("worktree "))
                .count(),
            1,
            "only the main worktree: {worktrees}"
        );
        assert_eq!(git_ok(&repo, &["branch", "--list", "wt/x"]).trim(), "");
        let made = std::fs::read_dir(hub.state.worktrees_dir()).map_or(0, Iterator::count);
        assert_eq!(made, 0, "nothing under the worktrees root");
        assert!(
            hub.state
                .with_persisted(|s| s.repos[0].last_spawn_config.clone())
                .is_none(),
            "no last spawn config was stored"
        );
        assert!(hub.sessions.snapshots().is_empty(), "nothing was spawned");
    }

    #[test]
    fn a_plain_shell_request_with_an_agent_is_refused_by_the_pre_check() {
        let mut req = env_spawn(
            single_target(true, None),
            SessionMode::PlainShell,
            Vec::new(),
            Some("rq-md"),
        );
        req.agent = Some(claude_selection());
        let failure =
            check_spawn_request_with(&req, |_| None).expect_err("the pre-check refuses an agent");
        assert_eq!(failure.title, "Couldn't start the shell session");
        assert_eq!(failure.detail, "A plain shell runs no agent.");
        assert!(failure.hint.is_none(), "{:?}", failure.hint);
    }

    #[tokio::test]
    #[expect(clippy::panic, reason = "a wrong reply fails the test loudly")]
    async fn an_unset_env_reference_is_refused_before_the_checkout_confirm() {
        let (hub, _scratch, repo) = env_ref_repo_hub("env-ref-confirm");
        std::fs::write(repo.join("dirty.txt"), "x\n").expect("dirty the tree");
        let literal = env_spawn(
            single_target(false, None),
            SessionMode::Interactive,
            env(&[("K", "v")]),
            Some("rq-lit"),
        );
        let reply = dispatch_one(&hub, ClientMessage::SpawnSession(literal)).await;
        let DaemonMessage::CheckoutConfirmRequired { .. } = reply else {
            panic!("the literal spawn needs a checkout confirm, got {reply:?}");
        };
        let unset = env_spawn(
            single_target(false, None),
            SessionMode::Interactive,
            env(&[("K", UNSET_REF)]),
            Some("rq-ref"),
        );
        let reply = dispatch_one(&hub, ClientMessage::SpawnSession(unset)).await;
        assert_unset_refusal(reply, "rq-ref");
        assert!(hub.sessions.snapshots().is_empty(), "nothing was spawned");
    }

    #[tokio::test]
    async fn the_child_env_gets_reference_values_while_stored_and_replayed_configs_keep_references()
    {
        let (hub, _scratch, _repo) = env_ref_repo_hub("env-ref-stored");
        let extra = env(&[("RT_PATH_COPY", "${env:PATH}"), ("RT_LIT", "x${env:PATH}")]);
        let mut target = single_target(false, None);
        if let SpawnTarget::Single { branch_name, .. } = &mut target {
            "main".clone_into(branch_name);
        }
        let req = env_spawn(target, SessionMode::PlainShell, extra.clone(), None);
        let _ = child_env_probe::take();
        // The spawnless hub fails at the tracer, after the spec is built.
        spawn_session(&hub, req, None, false)
            .await
            .expect_err("no tracer can start");
        let child = child_env_probe::take().expect("a spawn spec's env was built");
        let path = std::env::var("PATH").expect("PATH is set");
        let value = |key: &str| child.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
        assert_eq!(
            value("RT_PATH_COPY"),
            Some(path),
            "the child gets the value"
        );
        assert_eq!(value("RT_LIT").as_deref(), Some("x${env:PATH}"));

        let stored = hub
            .state
            .with_persisted(|s| s.repos[0].last_spawn_config.clone())
            .expect("last spawn config stored");
        assert_eq!(stored.extra_env, extra, "the reference is stored as sent");
        assert_eq!(
            stored.to_clone_request().extra_env,
            extra,
            "resume replays it"
        );
        assert_eq!(
            stored
                .to_duplicate_request(Some("wt/y".to_owned()))
                .extra_env,
            extra,
            "duplicate replays it"
        );
    }

    /// Repo `r1`'s `main`, in place: no worktree.
    fn in_place_main() -> SpawnTarget {
        let mut target = single_target(false, None);
        if let SpawnTarget::Single { branch_name, .. } = &mut target {
            "main".clone_into(branch_name);
        }
        target
    }

    #[tokio::test]
    #[expect(clippy::panic, reason = "a wrong reply fails the test loudly")]
    async fn a_deepseek_spawn_without_a_key_is_refused_before_git_work() {
        let (hub, _scratch, repo) = env_ref_repo_hub("deepseek-no-key");
        let req = deepseek_spawn(
            single_target(true, None),
            SessionMode::Interactive,
            Vec::new(),
            Some("rq-ds"),
        );
        let err = spawn_session_with(&hub, req, None, false, SpawnExtras::default(), no_key)
            .await
            .expect_err("a missing key refuses");
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        send_spawn_failure(&out_tx, &err, Some("rq-ds"));
        let reply = out_rx.try_recv().expect("a reply");
        let DaemonMessage::ActionFailed {
            title,
            detail,
            hint,
            request_id,
        } = reply
        else {
            panic!("expected action_failed, got {reply:?}");
        };
        assert_eq!(title, "Couldn't start the DeepSeek session");
        assert!(detail.contains("`DEEPSEEK_API_KEY`"), "{detail}");
        assert!(
            hint.as_deref()
                .is_some_and(|h| h.contains("setx DEEPSEEK_API_KEY")),
            "{hint:?}"
        );
        assert_eq!(request_id.as_deref(), Some("rq-ds"));
        let worktrees = git_ok(&repo, &["worktree", "list", "--porcelain"]);
        assert_eq!(
            worktrees
                .lines()
                .filter(|l| l.starts_with("worktree "))
                .count(),
            1,
            "only the main worktree: {worktrees}"
        );
        assert_eq!(git_ok(&repo, &["branch", "--list", "wt/x"]).trim(), "");
        let made = std::fs::read_dir(hub.state.worktrees_dir()).map_or(0, Iterator::count);
        assert_eq!(made, 0, "nothing under the worktrees root");
        assert!(hub.sessions.snapshots().is_empty(), "nothing was spawned");
    }

    /// The refusal of an interactive spawn in place whose agent is `agent`,
    /// after checking it stored and spawned nothing.
    async fn agent_refusal_of(tag: &str, agent: Option<protocol::AgentSelection>) -> String {
        let (hub, _scratch, _repo) = env_ref_repo_hub(tag);
        let mut req = env_spawn(in_place_main(), SessionMode::Interactive, Vec::new(), None);
        req.agent = agent;
        let err = spawn_session(&hub, req, None, false)
            .await
            .expect_err("the agent selection is refused");
        assert!(
            hub.state
                .with_persisted(|s| s.repos[0].last_spawn_config.clone())
                .is_none(),
            "no last spawn config was stored"
        );
        assert!(hub.sessions.snapshots().is_empty(), "nothing was spawned");
        format!("{err:#}")
    }

    #[tokio::test]
    async fn a_spawn_with_a_bad_option_refuses_naming_the_field() {
        let mut selection = claude_selection();
        selection
            .options
            .insert("x".to_owned(), protocol::OptionValue::Text("1".to_owned()));
        let text = agent_refusal_of("spawn-bad-option", Some(selection)).await;
        assert!(text.contains("Claude has no option `x`"), "{text}");
    }

    #[tokio::test]
    async fn a_spawn_naming_an_unknown_agent_refuses() {
        let selection = protocol::AgentSelection {
            agent: AgentId("nope".to_owned()),
            options: OptionValues::new(),
        };
        let text = agent_refusal_of("spawn-unknown-agent", Some(selection)).await;
        assert!(text.contains("This daemon has no agent `nope`."), "{text}");
    }

    #[tokio::test]
    async fn an_agent_session_without_an_agent_refuses() {
        let text = agent_refusal_of("spawn-no-agent", None).await;
        assert!(text.contains("An agent session needs an agent."), "{text}");
    }

    #[tokio::test]
    async fn a_spawn_stores_the_validated_defaults() {
        let (hub, _scratch, _repo) = env_ref_repo_hub("spawn-stores-defaults");
        let mut req = env_spawn(in_place_main(), SessionMode::Interactive, Vec::new(), None);
        req.agent = Some(claude_selection());
        // The spawnless hub fails at the tracer, after the config is stored.
        spawn_session_with(
            &hub,
            req,
            None,
            false,
            SpawnExtras::default(),
            crate::server::test_support::no_key,
        )
        .await
        .expect_err("no tracer can start");
        let stored = hub
            .state
            .with_persisted(|s| s.repos[0].last_spawn_config.clone())
            .expect("last spawn config stored");
        assert_eq!(
            stored.agent,
            Some(protocol::AgentSelection {
                agent: agent_registry::default_agent(),
                options: claude_options(),
            }),
            "every field's default is stored"
        );
    }

    #[tokio::test]
    async fn the_stored_config_of_a_deepseek_spawn_holds_only_the_users_rows() {
        let (hub, _scratch, _repo) = env_ref_repo_hub("deepseek-stored");
        let extra = env(&[("RT_USER_ROW", "x")]);
        let req = deepseek_spawn(
            in_place_main(),
            SessionMode::Interactive,
            extra.clone(),
            None,
        );
        let _ = child_env_probe::take();
        // The spawnless hub fails at the tracer, after the spec is built.
        spawn_session_with(&hub, req, None, false, SpawnExtras::default(), deepseek_key)
            .await
            .expect_err("no tracer can start");
        let child = child_env_probe::take().expect("a spawn spec's env was built");
        assert_eq!(
            child_value(&child, "ANTHROPIC_AUTH_TOKEN").as_deref(),
            Some(DEEPSEEK_KEY_SENTINEL),
            "the child gets the key"
        );
        assert_eq!(child_value(&child, "RT_USER_ROW").as_deref(), Some("x"));

        let stored = hub
            .state
            .with_persisted(|s| s.repos[0].last_spawn_config.clone())
            .expect("last spawn config stored");
        assert_eq!(stored.extra_env, extra, "only the user's rows are stored");
        assert_eq!(
            stored
                .agent
                .and_then(|selection| selection.options.get("provider").cloned()),
            Some(protocol::OptionValue::Text("deepseek".to_owned()))
        );
        let state = std::fs::read_to_string(&hub.dirs.state_file).expect("state.json written");
        assert!(!state.contains(DEEPSEEK_KEY_SENTINEL), "{state}");
        assert!(!state.contains("ANTHROPIC_BASE_URL"), "{state}");
    }

    #[tokio::test]
    async fn a_deepseek_interactive_spec_carries_the_routing_set_and_removals() {
        let (hub, _scratch, _repo) = env_ref_repo_hub("deepseek-spec");
        let req = deepseek_spawn(in_place_main(), SessionMode::Interactive, Vec::new(), None);
        let _ = child_env_probe::take_both();
        // The spawnless hub fails at the tracer, after the spec is built.
        spawn_session_with(&hub, req, None, false, SpawnExtras::default(), deepseek_key)
            .await
            .expect_err("no tracer can start");
        let (child, removed) = child_env_probe::take_both().expect("a spawn spec was built");
        assert_eq!(
            child_value(&child, "ANTHROPIC_BASE_URL").as_deref(),
            Some("https://api.deepseek.com/anthropic")
        );
        assert_eq!(
            child_value(&child, "CLAUDE_DEEPSEEK_SESSION").as_deref(),
            Some("1")
        );
        assert_eq!(
            child_value(&child, "ANTHROPIC_AUTH_TOKEN").as_deref(),
            Some(DEEPSEEK_KEY_SENTINEL)
        );
        assert_eq!(child_value(&child, "ANTHROPIC_API_KEY"), None);
        for name in ["ANTHROPIC_API_KEY", "CLAUDE_CODE_USE_FOUNDRY"] {
            assert!(removed.iter().any(|r| r == name), "{name} in {removed:?}");
        }
    }

    #[tokio::test]
    #[expect(clippy::panic, reason = "a wrong reply fails the test loudly")]
    async fn a_plain_shell_deepseek_request_is_refused() {
        let (hub, _scratch, repo) = env_ref_repo_hub("deepseek-shell");
        let req = deepseek_spawn(
            single_target(true, None),
            SessionMode::PlainShell,
            Vec::new(),
            None,
        );
        // No key anywhere: the shell refusal comes first, not the key's.
        let err = spawn_session_with(&hub, req, None, false, SpawnExtras::default(), no_key)
            .await
            .expect_err("a plain shell refuses an agent");
        let text = format!("{err:#}");
        assert!(text.contains("A plain shell runs no agent."), "{text}");
        assert!(!text.contains("DEEPSEEK_API_KEY"), "{text}");
        let worktrees = git_ok(&repo, &["worktree", "list", "--porcelain"]);
        assert_eq!(
            worktrees
                .lines()
                .filter(|l| l.starts_with("worktree "))
                .count(),
            1,
            "only the main worktree: {worktrees}"
        );
        assert_eq!(git_ok(&repo, &["branch", "--list", "wt/x"]).trim(), "");
        assert!(
            hub.state
                .with_persisted(|s| s.repos[0].last_spawn_config.clone())
                .is_none(),
            "no last spawn config was stored"
        );

        let req = deepseek_spawn(
            single_target(true, None),
            SessionMode::PlainShell,
            Vec::new(),
            Some("rq-sh"),
        );
        let reply = dispatch_one(&hub, ClientMessage::SpawnSession(req)).await;
        let DaemonMessage::ActionFailed {
            detail, request_id, ..
        } = reply
        else {
            panic!("the pre-check refuses, got {reply:?}");
        };
        assert!(detail.contains("A plain shell runs no agent."), "{detail}");
        assert_eq!(request_id.as_deref(), Some("rq-sh"));
        assert!(hub.sessions.snapshots().is_empty(), "nothing was spawned");
    }

    #[test]
    fn the_env_row_pre_check_refuses_a_deepseek_spawn_without_its_key() {
        let deepseek = deepseek_spawn(
            single_target(true, None),
            SessionMode::Interactive,
            Vec::new(),
            None,
        );
        let anthropic = env_spawn(
            single_target(true, None),
            SessionMode::Interactive,
            Vec::new(),
            None,
        );
        let failure = check_env_rows_with(&deepseek, |_| None).expect_err("a missing key refuses");
        assert!(
            failure.detail.contains("`DEEPSEEK_API_KEY`"),
            "{}",
            failure.detail
        );
        assert!(check_env_rows_with(&deepseek, deepseek_key).is_ok());
        assert!(check_env_rows_with(&anthropic, |_| None).is_ok());
    }

    #[tokio::test]
    async fn a_locked_key_row_is_refused_before_it_is_sealed() {
        assert_deepseek_row_refused_before_sealing(
            "deepseek-locked",
            "ANTHROPIC_AUTH_TOKEN",
            "`ANTHROPIC_AUTH_TOKEN` is set by the DeepSeek provider",
        )
        .await;
    }

    #[tokio::test]
    async fn a_removed_key_row_is_refused_before_it_is_sealed() {
        assert_deepseek_row_refused_before_sealing(
            "deepseek-removed",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "`CLAUDE_CODE_OAUTH_TOKEN` is removed from DeepSeek sessions",
        )
        .await;
    }

    /// A `DeepSeek` spawn with a secret-shaped `key` row is refused with a detail
    /// holding `expected`, by `spawn_session` and by the dispatcher's pre-check
    /// before a checkout confirm, and neither seals the row.
    #[expect(clippy::panic, reason = "a wrong reply fails the test loudly")]
    async fn assert_deepseek_row_refused_before_sealing(tag: &str, key: &str, expected: &str) {
        let (_lock, _secrets_dir) = env_secrets::test_support::scratch_async(tag).await;
        let (hub, _scratch, repo) = env_ref_repo_hub(tag);
        let rows = env(&[(key, SECRET_SENTINEL)]);
        assert!(
            protocol::env_rows::is_secret_key(key),
            "the row is one sealing would take"
        );
        let req = deepseek_spawn(
            in_place_main(),
            SessionMode::Interactive,
            rows.clone(),
            None,
        );
        let err = spawn_session_with(&hub, req, None, false, SpawnExtras::default(), deepseek_key)
            .await
            .expect_err("the row refuses");
        let failure = err
            .downcast_ref::<spawn_plan::SpawnFailure>()
            .expect("a spawn failure");
        assert!(failure.detail.contains(expected), "{}", failure.detail);
        assert!(!format!("{err:#}").contains(SECRET_SENTINEL));
        let config = std::env::var_os("RUSTLING_TULIP_CONFIG_DIR").expect("a scratch config dir");
        assert!(
            !Path::new(&config).join("env-secrets.json").exists(),
            "nothing was sealed"
        );

        // A dirty tree makes this target need a checkout confirm, so the
        // refusal below can only come from the dispatcher's pre-check.
        std::fs::write(repo.join("dirty.txt"), "x\n").expect("dirty the tree");
        let control = env_spawn(
            single_target(false, None),
            SessionMode::Interactive,
            env(&[("K", "v")]),
            Some("rq-ctl"),
        );
        let reply = dispatch_one(&hub, ClientMessage::SpawnSession(control)).await;
        let DaemonMessage::CheckoutConfirmRequired { .. } = reply else {
            panic!("the target needs a checkout confirm, got {reply:?}");
        };
        let req = deepseek_spawn(
            single_target(false, None),
            SessionMode::Interactive,
            rows,
            Some("rq-lk"),
        );
        let reply = dispatch_one(&hub, ClientMessage::SpawnSession(req)).await;
        let DaemonMessage::ActionFailed {
            detail, request_id, ..
        } = reply
        else {
            panic!("the pre-check refuses before the confirm, got {reply:?}");
        };
        assert!(detail.contains(expected), "{detail}");
        assert!(!detail.contains(SECRET_SENTINEL), "{detail}");
        assert_eq!(request_id.as_deref(), Some("rq-lk"));
        assert!(
            !Path::new(&config).join("env-secrets.json").exists(),
            "the pre-check sealed nothing"
        );
        assert!(hub.sessions.snapshots().is_empty(), "nothing was spawned");
    }

    /// A value no stored copy of a sealed row may ever hold.
    const SECRET_SENTINEL: &str = "rt-secret-sentinel-3d8f1a";

    /// `key`'s value in a child env the spawn composed.
    fn child_value(child: &[(String, String)], key: &str) -> Option<String> {
        child.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    /// `text` holds no copy of the sentinel and does carry a reference.
    fn assert_sealed_only(text: &str, what: &str) {
        assert!(!text.contains(SECRET_SENTINEL), "{what}: {text}");
        assert!(text.contains("${secret:"), "{what}: {text}");
    }

    /// The `Repos` list of the next `StateEvent::Repos` on `rx`.
    #[expect(clippy::panic, reason = "a wrong event fails the test loudly")]
    fn next_repos(rx: &mut broadcast::Receiver<StateEvent>) -> Vec<protocol::RepoEntry> {
        loop {
            match rx.try_recv() {
                Ok(StateEvent::Repos(repos)) => return repos,
                Ok(_) => {}
                Err(err) => panic!("no repos broadcast arrived: {err}"),
            }
        }
    }

    /// An interactive spawn onto `target` whose `EXAMPLE_API_KEY` row is the
    /// literal sentinel.
    fn secret_literal_spawn(target: SpawnTarget) -> SpawnRequest {
        env_spawn(
            target,
            SessionMode::Interactive,
            env(&[("EXAMPLE_API_KEY", SECRET_SENTINEL)]),
            None,
        )
    }

    /// Register `id` as an interactive session whose record and sidecar carry
    /// `stored`, exactly as the spawn path's constructors leave them, and write
    /// the history entry its stop would.
    ///
    /// A spawn cannot run to completion under a test hub — it needs a real
    /// tracer — so a test that would otherwise read the record, the sidecar or
    /// the history off a live spawn feeds the config the pipeline captured
    /// (which is what the spawn hands these constructors) to the same
    /// constructors here.
    fn record_stopped_session(hub: &Hub, id: &str, stored: &protocol::SpawnConfig) {
        let record = interactive_record(
            NewInteractive {
                session_id: id.to_owned(),
                label: id.to_owned(),
                kind: SessionKind::Standalone,
                members: Vec::new(),
                workspace_id: None,
                agent: agent_registry::default_agent(),
                program_name: claude_backend().descriptor().tag.clone(),
                cwd: hub.dirs.config.to_string_lossy().into_owned(),
                stored_config: stored.clone(),
                last_prompt: None,
                conversation_id: None,
                started_at: Utc::now(),
                busy_excluded: false,
                shell_label: None,
            },
            None,
        );
        let meta = interactive_meta(
            &record,
            4242,
            TracerSidecar {
                pid: 4243,
                pipe: r"\\.\pipe\rt-tracer-sidecar".to_owned(),
                exe_path: "rt-tracer.exe".to_owned(),
            },
        )
        .expect("sidecar meta");
        let entry = history::entry_from_record(&record, SessionEnd::StoppedByUser, Utc::now());
        hub.sessions.insert(record);
        hub.sessions.write_sidecar(&meta);
        history::write_if_absent(&hub.dirs, &entry).expect("write the history entry");
    }

    /// Standalone target whose folder is the hub's own config dir.
    fn standalone_here(hub: &Hub) -> SpawnTarget {
        SpawnTarget::Standalone {
            cwd: Some(hub.dirs.config.to_string_lossy().into_owned()),
            add_dirs: Vec::new(),
        }
    }

    /// Put `record` into `hub`'s registry unpublished and then publish it,
    /// answering the snapshot it was published with, the way a spawn's first
    /// record reaches clients without a child.
    fn publish_record(hub: &Hub, record: SessionRecord) -> protocol::SessionSnapshot {
        let pending = hub.sessions.insert_pending(record);
        let snap = crate::sync::lock(pending.arc()).snapshot();
        pending.publish_from(None);
        snap
    }

    /// A spawn-time config captured from a plain request, the way the spawn
    /// paths capture one.
    fn standalone_config(mode: SessionMode) -> protocol::SpawnConfig {
        protocol::SpawnConfig::from_request(&env_spawn(
            SpawnTarget::Standalone {
                cwd: None,
                add_dirs: Vec::new(),
            },
            mode,
            Vec::new(),
            None,
        ))
    }

    /// The sidecar keeps the program the user runs, so a reattached session's
    /// leaf reads `claude` and not the tracer supervising it.
    #[test]
    fn interactive_sidecar_names_the_agent_not_the_tracer() {
        let record = interactive_record(
            NewInteractive {
                session_id: "agent-1".to_owned(),
                label: "agent-1".to_owned(),
                kind: SessionKind::Standalone,
                members: Vec::new(),
                workspace_id: None,
                agent: agent_registry::default_agent(),
                program_name: claude_backend().descriptor().tag.clone(),
                cwd: "C:\\work".to_owned(),
                stored_config: standalone_config(SessionMode::Interactive),
                last_prompt: None,
                conversation_id: None,
                started_at: Utc::now(),
                busy_excluded: false,
                shell_label: None,
            },
            None,
        );
        let meta = interactive_meta(
            &record,
            42,
            TracerSidecar {
                pid: 7,
                pipe: r"\\.\pipe\rt-tracer-agent-1".to_owned(),
                exe_path: "rt-tracer.exe".to_owned(),
            },
        )
        .expect("sidecar meta");
        assert_eq!(meta.program_name, record.program_name);
        assert_eq!(meta.program_name.as_deref(), Some("claude"));
        assert_eq!(meta.tracer_pid, Some(7), "liveness runs off the tracer pid");
    }

    /// The same for a plain-shell session: its sidecar names the shell.
    #[test]
    fn plain_shell_sidecar_names_the_shell_not_the_tracer() {
        let record = plain_shell_record(
            NewPlainShell {
                session_id: "shell-1".to_owned(),
                label: "shell-1".to_owned(),
                kind: SessionKind::Standalone,
                members: Vec::new(),
                workspace_id: None,
                shell_label: "pwsh".to_owned(),
                cwd: "C:\\work".to_owned(),
                stored_config: standalone_config(SessionMode::PlainShell),
                started_at: Utc::now(),
                busy_excluded: false,
                hook_agent: None,
            },
            None,
        );
        let meta = plain_shell_meta(
            record.id.clone(),
            42,
            record.label.clone(),
            record.kind.clone(),
            record.members.clone(),
            record.started_at,
            record.workspace_id.clone(),
            record.program_name.clone(),
            record.spawn_config.clone(),
            record.current_cwd.clone(),
            TracerSidecar {
                pid: 7,
                pipe: r"\\.\pipe\rt-tracer-shell-1".to_owned(),
                exe_path: "rt-tracer.exe".to_owned(),
            },
        )
        .expect("sidecar meta");
        assert_eq!(meta.program_name, record.program_name);
        assert_eq!(meta.program_name.as_deref(), Some("pwsh"));
        assert_eq!(meta.tracer_pid, Some(7), "liveness runs off the tracer pid");
    }

    #[tokio::test]
    #[expect(clippy::panic, reason = "a missing broadcast fails the test loudly")]
    async fn a_busy_excluded_record_publishes_the_exclusion_in_its_first_update_plain_shell() {
        let (hub, _scratch) = test_hub("busy-first-plain-shell");
        let mut events = hub.sessions.subscribe();
        let request = env_spawn(
            standalone_here(&hub),
            SessionMode::PlainShell,
            Vec::new(),
            None,
        );
        let record = plain_shell_record(
            NewPlainShell {
                session_id: "shell-1".to_owned(),
                label: "shell-1".to_owned(),
                kind: SessionKind::Standalone,
                members: Vec::new(),
                workspace_id: None,
                shell_label: "pwsh".to_owned(),
                cwd: hub.dirs.config.to_string_lossy().into_owned(),
                stored_config: protocol::SpawnConfig::from_request(&request),
                started_at: Utc::now(),
                busy_excluded: true,
                hook_agent: None,
            },
            None,
        );
        let snap = publish_record(&hub, record);
        assert!(snap.busy_excluded, "the first record carries the flag");
        let Ok(crate::session::SessionEvent::Updated(first, None)) = events.try_recv() else {
            panic!("expected the record's first SessionUpdated");
        };
        assert_eq!(first.id, snap.id, "the update is the new record's");
        assert!(
            first.busy_excluded,
            "the first update carries the flag without a follow-up write"
        );
    }

    #[tokio::test]
    #[expect(clippy::panic, reason = "a missing broadcast fails the test loudly")]
    async fn a_busy_excluded_record_publishes_the_exclusion_in_its_first_update_interactive() {
        let (hub, _scratch) = test_hub("busy-first-interactive");
        let mut events = hub.sessions.subscribe();
        let request = env_spawn(
            standalone_here(&hub),
            SessionMode::Interactive,
            Vec::new(),
            None,
        );
        let record = interactive_record(
            NewInteractive {
                session_id: "agent-1".to_owned(),
                label: "agent-1".to_owned(),
                kind: SessionKind::Standalone,
                members: Vec::new(),
                workspace_id: None,
                agent: agent_registry::default_agent(),
                program_name: claude_backend().descriptor().tag.clone(),
                cwd: hub.dirs.config.to_string_lossy().into_owned(),
                stored_config: protocol::SpawnConfig::from_request(&request),
                last_prompt: None,
                conversation_id: None,
                started_at: Utc::now(),
                busy_excluded: true,
                shell_label: None,
            },
            None,
        );
        let snap = publish_record(&hub, record);
        assert!(snap.busy_excluded, "the first record carries the flag");
        let Ok(crate::session::SessionEvent::Updated(first, None)) = events.try_recv() else {
            panic!("expected the record's first SessionUpdated");
        };
        assert_eq!(first.id, snap.id, "the update is the new record's");
        assert!(
            first.busy_excluded,
            "the first update carries the flag without a follow-up write"
        );
    }

    /// A spawn whose dialog row holds a literal secret. The spawnless hub
    /// cannot stage a tracer, so the spawn stops after the config is captured —
    /// which is the part this test is about.
    #[tokio::test]
    #[expect(clippy::panic, reason = "a wrong reply fails the test loudly")]
    async fn a_literal_secret_row_is_stored_as_a_reference_everywhere() {
        let (_lock, _secrets_dir) =
            env_secrets::test_support::scratch_async("seal-everywhere").await;
        let (hub, _scratch, _repo) = env_ref_repo_hub("seal-everywhere");
        let mut repos_rx = hub.state_events.subscribe();
        let mut target = single_target(false, None);
        if let SpawnTarget::Single { branch_name, .. } = &mut target {
            "main".clone_into(branch_name);
        }
        spawn_session(&hub, secret_literal_spawn(target), None, false)
            .await
            .expect_err("no tracer can start");

        let repos = serde_json::to_string(&next_repos(&mut repos_rx)).expect("serialize repos");
        assert_sealed_only(&repos, "the Repos broadcast");
        let state = std::fs::read_to_string(&hub.dirs.state_file).expect("state.json written");
        assert_sealed_only(&state, "state.json");

        let stored = hub
            .state
            .with_persisted(|s| s.repos[0].last_spawn_config.clone())
            .expect("last spawn config stored");
        assert_eq!(stored.extra_env[0].0, "EXAMPLE_API_KEY");
        record_stopped_session(&hub, "sealed", &stored);
        let recorded = hub
            .sessions
            .get("sealed")
            .and_then(|rec| crate::sync::lock(&rec).spawn_config.clone())
            .expect("the record carries the config");
        assert_sealed_only(
            &serde_json::to_string(&recorded).expect("serialize config"),
            "the record's config",
        );

        let meta = std::fs::read_to_string(hub.dirs.sessions_dir.join("sealed").join("meta.json"))
            .expect("meta.json written");
        assert_sealed_only(&meta, "meta.json");
        let entry = std::fs::read_to_string(hub.dirs.history_dir().join("sealed.json"))
            .expect("the history entry written");
        assert_sealed_only(&entry, "the history entry");

        let reply = dispatch_one(
            &hub,
            ClientMessage::GetSpawnConfig {
                session_id: "sealed".to_owned(),
            },
        )
        .await;
        let DaemonMessage::SpawnConfigReply { config, .. } = reply else {
            panic!("expected spawn_config_reply, got {reply:?}");
        };
        assert_sealed_only(
            &serde_json::to_string(&config.expect("the record has a config")).expect("serialize"),
            "the GetSpawnConfig reply",
        );
    }

    #[tokio::test]
    async fn the_child_gets_the_sealed_value() {
        let (_lock, _secrets_dir) = env_secrets::test_support::scratch_async("seal-child").await;
        let (hub, _scratch, _repo) = env_ref_repo_hub("seal-child");
        let id = env_secrets::seal("EXAMPLE_API_KEY", SECRET_SENTINEL).expect("seal the row");
        let mut target = single_target(false, None);
        if let SpawnTarget::Single { branch_name, .. } = &mut target {
            "main".clone_into(branch_name);
        }
        let req = env_spawn(
            target,
            SessionMode::Interactive,
            env(&[("EXAMPLE_API_KEY", &secret_ref(&id))]),
            None,
        );
        let _ = child_env_probe::take();
        spawn_session(&hub, req, None, false)
            .await
            .expect_err("no tracer can start");
        let child = child_env_probe::take().expect("a spawn spec's env was built");
        assert_eq!(
            child_value(&child, "EXAMPLE_API_KEY").as_deref(),
            Some(SECRET_SENTINEL),
            "a secret reference resolves to the value it stands for"
        );
    }

    /// A literal under a key that names no credential is left exactly as sent,
    /// and reaches the child unchanged. Nothing here seals, so this passes
    /// before ES.3 too: it pins the harm the pattern must not do.
    #[tokio::test]
    async fn a_plain_literal_row_is_stored_verbatim() {
        let (_lock, _secrets_dir) = env_secrets::test_support::scratch_async("seal-plain").await;
        let (hub, _scratch, _repo) = env_ref_repo_hub("seal-plain");
        let rows = env(&[("RUST_LOG", "debug"), ("RT_PLAIN", "not-a-secret")]);
        let mut target = single_target(false, None);
        if let SpawnTarget::Single { branch_name, .. } = &mut target {
            "main".clone_into(branch_name);
        }
        let _ = child_env_probe::take();
        spawn_session(
            &hub,
            env_spawn(target, SessionMode::Interactive, rows.clone(), None),
            None,
            false,
        )
        .await
        .expect_err("no tracer can start");

        let stored = hub
            .state
            .with_persisted(|s| s.repos[0].last_spawn_config.clone())
            .expect("last spawn config stored");
        assert_eq!(stored.extra_env, rows, "a plain literal is stored as sent");
        let child = child_env_probe::take().expect("a spawn spec's env was built");
        assert_eq!(
            child_value(&child, "RT_PLAIN").as_deref(),
            Some("not-a-secret")
        );
        assert_eq!(child_value(&child, "RUST_LOG").as_deref(), Some("debug"));
    }

    #[tokio::test]
    async fn a_secret_env_keys_row_is_sealed_though_its_name_is_plain() {
        let (_lock, _secrets_dir) = env_secrets::test_support::scratch_async("seal-toggled").await;
        let (hub, _scratch, _repo) = env_ref_repo_hub("seal-toggled");
        let mut target = single_target(false, None);
        if let SpawnTarget::Single { branch_name, .. } = &mut target {
            "main".clone_into(branch_name);
        }
        let mut req = env_spawn(
            target,
            SessionMode::Interactive,
            env(&[("GH_PAT", SECRET_SENTINEL)]),
            None,
        );
        req.secret_env_keys = vec!["GH_PAT".to_owned()];
        let _ = child_env_probe::take();
        spawn_session(&hub, req, None, false)
            .await
            .expect_err("no tracer can start");

        let stored = hub
            .state
            .with_persisted(|s| s.repos[0].last_spawn_config.clone())
            .expect("last spawn config stored");
        let (key, value) = &stored.extra_env[0];
        assert_eq!(key, "GH_PAT");
        assert!(value.starts_with("${secret:"), "{value}");
        assert!(!value.contains(SECRET_SENTINEL), "{value}");
        let child = child_env_probe::take().expect("a spawn spec's env was built");
        assert_eq!(
            child_value(&child, "GH_PAT").as_deref(),
            Some(SECRET_SENTINEL),
            "a toggled row is sealed and resolved like a pattern row"
        );
    }

    #[tokio::test]
    async fn duplicate_resume_and_recover_resolve_the_sealed_value() {
        let (_lock, _secrets_dir) = env_secrets::test_support::scratch_async("seal-replay").await;
        let (hub, scratch) = spawnless_test_hub("seal-replay");
        let folder = scratch.path().join("replay");
        std::fs::create_dir_all(&folder).expect("create folder");
        let folder = folder.to_string_lossy().into_owned();
        let id = env_secrets::seal("EXAMPLE_API_KEY", SECRET_SENTINEL).expect("seal the row");
        let target = SpawnTarget::Standalone {
            cwd: Some(folder.clone()),
            add_dirs: Vec::new(),
        };
        let request = env_spawn(
            target.clone(),
            SessionMode::Interactive,
            env(&[("EXAMPLE_API_KEY", &secret_ref(&id))]),
            None,
        );
        let stored = protocol::SpawnConfig::from_request(&request);
        let (out_tx, _out_rx) = mpsc::unbounded_channel::<DaemonMessage>();

        let mut source = idle_record("source");
        source.spawn_config = Some(stored.clone());
        hub.sessions.insert(source);
        let _ = child_env_probe::take();
        let ask = DuplicateAsk {
            session_id: "source",
            request_id: None,
            carry_busy_excluded: false,
            same_checkout: false,
        };
        duplicate_session(&hub, ask, 1, &out_tx).await;
        assert_eq!(
            child_value(
                &child_env_probe::take().expect("the duplicate built an env"),
                "EXAMPLE_API_KEY"
            )
            .as_deref(),
            Some(SECRET_SENTINEL),
            "a duplicate resolves the reference the record holds"
        );

        let mut abandoned = idle_record("abandoned");
        abandoned.spawn_config = Some(stored.clone());
        abandoned.is_abandoned = true;
        abandoned.status = protocol::SessionStatus::Stopped;
        hub.sessions.insert(abandoned);
        let _ = child_env_probe::take();
        resume_abandoned(&hub, "abandoned", &out_tx)
            .await
            .expect_err("no tracer can start");
        assert_eq!(
            child_value(
                &child_env_probe::take().expect("the resume built an env"),
                "EXAMPLE_API_KEY"
            )
            .as_deref(),
            Some(SECRET_SENTINEL),
            "a resume resolves the reference the record holds"
        );
    }

    #[tokio::test]
    #[expect(clippy::panic, reason = "a wrong reply fails the test loudly")]
    async fn a_missing_secret_refuses_before_any_git_work() {
        let (_lock, _secrets_dir) = env_secrets::test_support::scratch_async("seal-missing").await;
        let (hub, _scratch, repo) = env_ref_repo_hub("seal-missing");
        let req = env_spawn(
            single_target(true, None),
            SessionMode::Interactive,
            env(&[("EXAMPLE_API_KEY", &secret_ref(MISSING_SECRET_ID))]),
            Some("rq-secret"),
        );
        // Straight into `spawn_session`, the path every replay takes, past the
        // dispatcher's own early check.
        let err = spawn_session(&hub, req, None, false)
            .await
            .expect_err("a missing saved secret refuses");
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        send_spawn_failure(&out_tx, &err, Some("rq-secret"));
        let DaemonMessage::ActionFailed {
            title,
            detail,
            hint,
            request_id,
        } = out_rx.try_recv().expect("a reply")
        else {
            panic!("expected action_failed");
        };
        assert_eq!(title, "A saved secret is missing");
        assert!(detail.contains("`EXAMPLE_API_KEY`"), "{detail}");
        assert!(detail.contains("Windows Credential Manager"), "{detail}");
        assert!(!detail.contains(SECRET_SENTINEL), "{detail}");
        assert!(hint.is_some());
        assert_eq!(request_id.as_deref(), Some("rq-secret"));

        let worktrees = git_ok(&repo, &["worktree", "list", "--porcelain"]);
        assert_eq!(
            worktrees
                .lines()
                .filter(|l| l.starts_with("worktree "))
                .count(),
            1,
            "only the main worktree: {worktrees}"
        );
        assert_eq!(git_ok(&repo, &["branch", "--list", "wt/x"]).trim(), "");
        let made = std::fs::read_dir(hub.state.worktrees_dir()).map_or(0, Iterator::count);
        assert_eq!(made, 0, "nothing under the worktrees root");
        assert!(hub.sessions.snapshots().is_empty(), "nothing was spawned");
        assert!(
            hub.state
                .with_persisted(|s| s.repos[0].last_spawn_config.is_none()),
            "no config was captured either"
        );
    }

    #[tokio::test]
    #[expect(clippy::panic, reason = "a wrong reply fails the test loudly")]
    async fn a_missing_secret_is_refused_before_the_checkout_confirm() {
        let (_lock, _secrets_dir) =
            env_secrets::test_support::scratch_async("seal-missing-confirm").await;
        let (hub, _scratch, repo) = env_ref_repo_hub("seal-missing-confirm");
        std::fs::write(repo.join("dirty.txt"), "x\n").expect("dirty the tree");
        let req = env_spawn(
            single_target(false, None),
            SessionMode::Interactive,
            env(&[("EXAMPLE_API_KEY", &secret_ref(MISSING_SECRET_ID))]),
            Some("rq-secret"),
        );
        let reply = dispatch_one(&hub, ClientMessage::SpawnSession(req)).await;
        let DaemonMessage::ActionFailed { title, detail, .. } = reply else {
            panic!("the missing secret is refused before the confirm, got {reply:?}");
        };
        assert_eq!(title, "A saved secret is missing");
        assert!(detail.contains("`EXAMPLE_API_KEY`"), "{detail}");
        assert!(hub.sessions.snapshots().is_empty(), "nothing was spawned");
    }

    #[tokio::test]
    #[expect(clippy::panic, reason = "a wrong reply fails the test loudly")]
    async fn an_over_long_secret_is_refused_before_the_checkout_confirm() {
        let (_lock, _secrets_dir) =
            env_secrets::test_support::scratch_async("seal-too-long-confirm").await;
        let (hub, _scratch, repo) = env_ref_repo_hub("seal-too-long-confirm");
        std::fs::write(repo.join("dirty.txt"), "x\n").expect("dirty the tree");
        // One UTF-16 code unit over what Windows Credential Manager takes in
        // one credential blob.
        let too_long = "a".repeat(1281);
        let req = env_spawn(
            single_target(false, None),
            SessionMode::Interactive,
            env(&[("EXAMPLE_API_KEY", &too_long)]),
            Some("rq-long"),
        );
        let reply = dispatch_one(&hub, ClientMessage::SpawnSession(req)).await;
        let DaemonMessage::ActionFailed {
            title,
            detail,
            hint,
            request_id,
        } = reply
        else {
            panic!("the over-long secret is refused before the confirm, got {reply:?}");
        };
        assert_eq!(title, "Could not save a secret");
        assert!(detail.contains("EXAMPLE_API_KEY"), "{detail}");
        assert!(detail.contains("too long"), "{detail}");
        assert!(
            !detail.contains(&too_long),
            "the refusal never carries the value: {detail}"
        );
        assert!(hint.is_none());
        assert_eq!(request_id.as_deref(), Some("rq-long"));
        assert!(hub.sessions.snapshots().is_empty(), "nothing was spawned");
    }

    #[tokio::test]
    async fn standalone_target_refuses_headless() {
        let (hub, _scratch) = spawnless_test_hub("standalone-agents");
        for (mode, agent) in [(SessionMode::Headless, Some(claude_selection()))] {
            let req = SpawnRequest {
                label: None,
                target: SpawnTarget::Standalone {
                    cwd: None,
                    add_dirs: Vec::new(),
                },
                mode,
                initial_prompt: Some("x".to_owned()),
                dangerously_skip_permissions: false,
                agent,
                extra_env: Vec::new(),
                secret_env_keys: Vec::new(),
                prompt_injector: None,
                request_id: None,
                resume_conversation: None,
            };
            let err = spawn_session(&hub, req, None, false)
                .await
                .expect_err("refused before anything spawns");
            assert!(
                err.to_string().contains("standalone targets only support"),
                "{err:#}"
            );
        }
        assert!(standalone_supports(SessionMode::Interactive));
        assert!(standalone_supports(SessionMode::PlainShell));
        assert!(!standalone_supports(SessionMode::Headless));
    }

    /// Where an interactive spawn of `agent` in `folder` stops on the
    /// spawnless hub, as the error's whole chain.
    async fn standalone_agent_spawn_error(
        hub: &Hub,
        agent: protocol::AgentSelection,
        folder: &Path,
    ) -> String {
        let mut req = env_spawn(
            SpawnTarget::Standalone {
                cwd: Some(folder.to_string_lossy().into_owned()),
                add_dirs: Vec::new(),
            },
            SessionMode::Interactive,
            Vec::new(),
            None,
        );
        req.agent = Some(agent);
        let err = spawn_session(hub, req, None, false)
            .await
            .expect_err("the spawnless hub launches nothing");
        format!("{err:#}")
    }

    /// A standalone agent in `folder` runs there, with no members and so no
    /// worktree.
    fn assert_standalone_agent_runs_in(folder: &Path) {
        let (kind, members, cwd, label, workspace_id) =
            spawn_standalone_agent(Some(&folder.to_string_lossy())).expect("resolve the folder");
        let canonical = std::fs::canonicalize(folder).expect("canonicalize the folder");
        assert_eq!(kind, SessionKind::Standalone);
        assert!(members.is_empty(), "{members:?}");
        assert_eq!(cwd, crate::paths::simplify_path(&canonical));
        assert_eq!(label, path_leaf_label(folder));
        assert_eq!(workspace_id, None);
    }

    #[tokio::test]
    async fn standalone_target_spawns_interactive_claude_in_the_folder() {
        let (hub, scratch) = spawnless_test_hub("standalone-claude");
        let folder = scratch.path().join("plain-claude");
        std::fs::create_dir_all(&folder).expect("create folder");
        let error = standalone_agent_spawn_error(&hub, claude_selection(), &folder).await;
        assert!(error.contains("spawning claude via tracer"), "{error}");
        assert_standalone_agent_runs_in(&folder);
        assert!(hub.sessions.snapshots().is_empty(), "nothing was spawned");
    }

    #[tokio::test]
    async fn standalone_claude_in_a_missing_folder_fails_with_folder_not_found() {
        let (hub, scratch) = spawnless_test_hub("standalone-claude-missing");
        let folder = scratch.path().join("gone");
        let error = standalone_agent_spawn_error(&hub, claude_selection(), &folder).await;
        assert!(error.contains("reading the standalone folder"), "{error}");
        assert!(error.contains(&*folder.to_string_lossy()), "{error}");
    }

    fn single_target(use_worktree: bool, existing_worktree: Option<&str>) -> SpawnTarget {
        SpawnTarget::Single {
            repo_id: "r1".to_string(),
            branch_name: "wt/x".to_string(),
            base_branch: None,
            use_worktree,
            checkout_strategy: None,
            worktree_reuse: protocol::WorktreeReusePolicy::Reuse,
            existing_worktree: existing_worktree.map(str::to_string),
        }
    }

    #[test]
    fn checkout_confirm_echoes_the_spawn_request_id() {
        let reply = checkout_confirm_reply("r1", "wt/x", Some("req-9"), 4);
        assert!(
            matches!(
                &reply,
                DaemonMessage::CheckoutConfirmRequired {
                    repo_id,
                    branch,
                    dirty_count: 4,
                    request_id: Some(id),
                } if repo_id == "r1" && branch == "wt/x" && id == "req-9"
            ),
            "{reply:?}"
        );
        let reply = checkout_confirm_reply("r1", "wt/x", None, 4);
        assert!(
            matches!(
                &reply,
                DaemonMessage::CheckoutConfirmRequired {
                    request_id: None,
                    ..
                }
            ),
            "a spawn without an id gets a prompt without one: {reply:?}"
        );
    }

    #[test]
    fn pinning_an_in_place_single_spawn_is_refused() {
        // In-place runs in the repo; a pin names a different directory. Doing
        // either silently would put the session somewhere unchosen.
        let err = reject_pin_without_worktree(&single_target(false, Some("X:/wt/repo")))
            .expect_err("pin + in-place must not be accepted");
        assert!(
            format!("{err:#}").contains("cannot pin an existing worktree"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn pinning_a_worktree_spawn_is_allowed() {
        reject_pin_without_worktree(&single_target(true, Some("X:/wt/repo")))
            .expect("a pin with use_worktree is the supported combination");
    }

    #[test]
    fn an_unpinned_in_place_spawn_is_allowed() {
        reject_pin_without_worktree(&single_target(false, None))
            .expect("in-place without a pin is the ordinary case");
    }

    #[test]
    fn pinning_an_in_place_workspace_spawn_is_refused() {
        let target = SpawnTarget::Workspace {
            workspace_id: "ws1".to_string(),
            branch_name: "wt/x".to_string(),
            base_branch: None,
            use_worktree: false,
            worktree_reuse: protocol::WorktreeReusePolicy::Reuse,
            existing_worktrees: vec![protocol::PinnedMemberWorktree {
                repo_id: "r1".to_string(),
                path: "X:/wt/repo".to_string(),
            }],
        };
        reject_pin_without_worktree(&target).expect_err("pin + in-place must not be accepted");
    }

    // Per-agent argv builders + workspace-prelude tests live in the agent
    // crates under `crates/agents/`.

    mod worktree_layout {
        use super::*;
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

        /// A repo at `dir` with one commit on `main`.
        fn seed_repo(dir: &Path) {
            std::fs::create_dir_all(dir).expect("create repo dir");
            git(dir, &["init", "-b", "main"]);
            git(dir, &["config", "user.email", "t@example.com"]);
            git(dir, &["config", "user.name", "Test"]);
            git(dir, &["config", "commit.gpgsign", "false"]);
            std::fs::write(dir.join("seed.txt"), "x\n").expect("write seed file");
            git(dir, &["add", "."]);
            git(dir, &["commit", "-m", "seed"]);
        }

        fn key(path: &str) -> String {
            crate::paths::normalize_path_key(path)
        }

        fn single_preview(msg: DaemonMessage) -> Option<protocol::MemberSpawnPreview> {
            match msg {
                DaemonMessage::SpawnPreview { preview, .. } => Some(preview),
                _ => None,
            }
        }

        fn workspace_previews(msg: DaemonMessage) -> Option<Vec<protocol::MemberSpawnPreview>> {
            match msg {
                DaemonMessage::WorkspaceSpawnPreview { per_member, .. } => Some(per_member),
                _ => None,
            }
        }

        async fn preview_single(hub: &Hub, branch: &str) -> protocol::MemberSpawnPreview {
            let msg = dispatch_one(
                hub,
                ClientMessage::PreviewSpawn {
                    repo_id: "r1".to_string(),
                    branch_name: branch.to_string(),
                    base_branch: None,
                    use_worktree: true,
                    request_id: None,
                },
            )
            .await;
            single_preview(msg).expect("a spawn preview")
        }

        /// A branch already checked out in a worktree under the root (here
        /// at the older anchor layout) is reused where it is; a branch nobody
        /// holds gets the new `wt.<branch>/<repo>` folder.
        #[tokio::test]
        async fn single_spawn_reuses_an_old_layout_worktree_holding_the_branch() {
            let (hub, scratch) = test_hub("reuse-single");
            let repo = scratch.path().join("src").join("app");
            seed_repo(&repo);
            hub.state
                .mutate(|s| s.repos.push(repo_entry("r1", "App", &repo)))
                .expect("register repo");
            let root = hub.state.worktrees_dir();
            let old = root.join("wt.feat").join("X").join("src").join("app");
            git(
                &repo,
                &["worktree", "add", "-b", "feat", &old.to_string_lossy()],
            );

            let held = preview_single(&hub, "feat").await;
            assert_eq!(key(&held.worktree_path), key(&old.to_string_lossy()));
            assert!(
                held.worktree_exists,
                "the held worktree is reported as existing"
            );

            let fresh = preview_single(&hub, "fresh").await;
            assert_eq!(
                key(&fresh.worktree_path),
                key(&root.join("wt.fresh").join("app").to_string_lossy())
            );
            assert!(!fresh.worktree_exists);
        }

        /// A workspace member whose branch sits in an old-layout worktree
        /// keeps it; a member with no worktree for the branch goes to the new
        /// `wt.<branch>/<workspace>/<offset>` path.
        #[tokio::test]
        async fn workspace_reuses_old_layout_member_worktrees() {
            let (hub, scratch) = test_hub("reuse-ws");
            let web = scratch.path().join("src").join("apps").join("web");
            let core = scratch.path().join("src").join("libs").join("core");
            seed_repo(&web);
            seed_repo(&core);
            hub.state
                .mutate(|s| {
                    s.repos.push(repo_entry("web", "web", &web));
                    s.repos.push(repo_entry("core", "core", &core));
                    s.workspaces
                        .push(workspace_entry("w1", "Shop", &["web", "core"]));
                })
                .expect("register workspace");
            let root = hub.state.worktrees_dir();
            let old_web = root
                .join("wt.feat")
                .join("D")
                .join("src")
                .join("apps")
                .join("web");
            git(
                &web,
                &["worktree", "add", "-b", "feat", &old_web.to_string_lossy()],
            );

            let msg = dispatch_one(
                &hub,
                ClientMessage::PreviewWorkspaceSpawn {
                    workspace_id: "w1".to_string(),
                    branch_name: "feat".to_string(),
                    base_branch: None,
                    request_id: None,
                },
            )
            .await;
            let rows = workspace_previews(msg).expect("a workspace spawn preview");
            assert_eq!(rows.len(), 2);
            assert_eq!(key(&rows[0].worktree_path), key(&old_web.to_string_lossy()));
            assert!(rows[0].worktree_exists);
            let new_core = root.join("wt.feat").join("shop").join("libs").join("core");
            assert_eq!(
                key(&rows[1].worktree_path),
                key(&new_core.to_string_lossy())
            );
            assert!(!rows[1].worktree_exists);
        }

        fn foreign_folder_message(path: &Path, repo_name: &str) -> String {
            format!(
                "Can't use the worktree folder {}: it exists but isn't a worktree of {repo_name}. Remove it from Manage worktrees, or rename the repo or workspace.",
                path.display()
            )
        }

        /// A registered repo `App` plus another repo whose worktree already
        /// sits at App's derived `wt.feat/app` folder.
        fn seed_foreign_single(hub: &Hub, scratch: &Path) -> PathBuf {
            let repo = scratch.join("src").join("app");
            let other = scratch.join("elsewhere").join("app");
            seed_repo(&repo);
            seed_repo(&other);
            hub.state
                .mutate(|s| s.repos.push(repo_entry("r1", "App", &repo)))
                .expect("register repo");
            let derived = hub.state.worktrees_dir().join("wt.feat").join("app");
            git(
                &other,
                &["worktree", "add", "-b", "feat", &derived.to_string_lossy()],
            );
            derived
        }

        #[tokio::test]
        async fn spawn_refuses_a_derived_folder_that_is_not_this_repos_worktree() {
            let (hub, scratch) = test_hub("foreign-spawn");
            let derived = seed_foreign_single(&hub, scratch.path());

            let err = spawn_single(
                &hub,
                "r1",
                SinglePlan {
                    branch_name: "feat",
                    base_branch: None,
                    use_worktree: true,
                    checkout_strategy: None,
                    worktree_reuse: protocol::WorktreeReusePolicy::Reuse,
                    existing_worktree: None,
                },
            )
            .await
            .expect_err("the spawn is refused");

            assert_eq!(err.to_string(), foreign_folder_message(&derived, "App"));
            assert!(
                derived.join("seed.txt").is_file(),
                "the other repo's worktree is left alone"
            );
        }

        #[tokio::test]
        async fn preview_reports_a_foreign_folder_as_an_error() {
            let (hub, scratch) = test_hub("foreign-preview");
            let derived = seed_foreign_single(&hub, scratch.path());

            let msg = dispatch_one(
                &hub,
                ClientMessage::PreviewSpawn {
                    repo_id: "r1".to_string(),
                    branch_name: "feat".to_string(),
                    base_branch: None,
                    use_worktree: true,
                    request_id: Some("p1".to_string()),
                },
            )
            .await;

            let expected = foreign_folder_message(&derived, "App");
            assert!(
                matches!(
                    &msg,
                    DaemonMessage::Error { message, request_id: Some(id) }
                        if id == "p1" && *message == expected
                ),
                "{msg:?}"
            );
        }

        #[tokio::test]
        async fn workspace_member_refuses_a_foreign_folder() {
            let (hub, scratch) = test_hub("foreign-ws");
            let web = scratch.path().join("src").join("apps").join("web");
            let core = scratch.path().join("src").join("libs").join("core");
            seed_repo(&web);
            seed_repo(&core);
            hub.state
                .mutate(|s| {
                    s.repos.push(repo_entry("web", "web", &web));
                    s.repos.push(repo_entry("core", "core", &core));
                    s.workspaces
                        .push(workspace_entry("w1", "Shop", &["web", "core"]));
                })
                .expect("register workspace");
            let foreign = hub
                .state
                .worktrees_dir()
                .join("wt.feat")
                .join("shop")
                .join("libs")
                .join("core");
            std::fs::create_dir_all(&foreign).expect("create foreign folder");
            std::fs::write(foreign.join("keep.txt"), "x\n").expect("write foreign file");

            let msg = dispatch_one(
                &hub,
                ClientMessage::PreviewWorkspaceSpawn {
                    workspace_id: "w1".to_string(),
                    branch_name: "feat".to_string(),
                    base_branch: None,
                    request_id: Some("p2".to_string()),
                },
            )
            .await;

            let expected = foreign_folder_message(&foreign, "core");
            assert!(
                matches!(
                    &msg,
                    DaemonMessage::Error { message, request_id: Some(id) }
                        if id == "p2" && *message == expected
                ),
                "{msg:?}"
            );
            assert!(foreign.join("keep.txt").is_file());
        }
    }

    // --- Tailnet listener: auth kind, routers, Origin/Host, handshake ---

    // --- Boot wiring and the remote switch ---

    // --- ConfigureRemote and RemoteStatus ---
}
