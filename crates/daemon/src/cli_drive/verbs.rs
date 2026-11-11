//! The verbs that change a session or follow one: `spawn`, `send`, `kill`,
//! `wait` and `watch`. Each is a thin loop over one connection, and the
//! conclusions they draw from its messages are in [`super::waiting`].

use super::args::{Payload, Place, SpawnOptions, Until};
use super::waiting::{self, Outcome, TextStep, TextWait, WaitState, WatchEnd, WatchState};
use super::{
    Failure, REQUEST_ID, ask, await_reply, closed_early, initial_sessions, is_mine, render,
};
use crate::cli_client::{Connection, Welcome};
use base64::Engine as _;
use protocol::{
    AgentDescriptor, AgentId, AgentSelection, ClientMessage, DaemonMessage, OptionValues,
    RepoEntry, SessionMode, SessionSnapshot, SessionStatus, SpawnRequest, SpawnTarget,
    WorkspaceEntry, WorktreeReusePolicy,
};
use regex::Regex;
use std::io::Write;
use std::time::Duration;

/// The request id a `ChatSend` asks with, so a refusal of the text is told
/// apart from the barrier's reply.
const SEND_REQUEST_ID: &str = "drive-send";

/// How often `wait --text` reads a screen no message has shown changing.
const SCREEN_TICK: Duration = Duration::from_millis(250);

/// What `wait` was asked for.
#[derive(Clone, Copy)]
pub(super) struct WaitRequest<'a> {
    pub(super) session_id: &'a str,
    pub(super) until: &'a Until,
    pub(super) limit: Duration,
    pub(super) json: bool,
}

/// What the daemon pushes before `Sessions` that `spawn` checks against.
struct Known {
    agents: Vec<AgentDescriptor>,
    repos: Vec<RepoEntry>,
    workspaces: Vec<WorkspaceEntry>,
}

/// The session `session_id` names in `sessions`, or `not_found`.
fn find<'a>(
    sessions: &'a [SessionSnapshot],
    session_id: &str,
) -> Result<&'a SessionSnapshot, Failure> {
    sessions
        .iter()
        .find(|session| session.id == session_id)
        .ok_or_else(|| Failure::unknown_session(session_id))
}

fn check_screen_dump(welcome: &Welcome) -> Result<(), Failure> {
    crate::cli_screen::check_welcome(crate::cli_screen::supports_screen_dump(&welcome.features))
        .map_err(|err| Failure::from_error(&err))
}

async fn dump_screen(connection: &mut Connection, session_id: &str) -> Result<(), Failure> {
    ask(
        connection,
        &ClientMessage::DumpScreen {
            session_id: session_id.to_owned(),
            request_id: Some(REQUEST_ID.to_owned()),
        },
    )
    .await
}

/// The agents, repos and workspaces the daemon pushes before its `Sessions`.
async fn initial_known(connection: &mut Connection) -> Result<Known, Failure> {
    let mut known = Known {
        agents: Vec::new(),
        repos: Vec::new(),
        workspaces: Vec::new(),
    };
    loop {
        match connection.next_message().await {
            None => return Err(closed_early("the sessions")),
            Some(DaemonMessage::Agents { agents }) => known.agents = agents,
            Some(DaemonMessage::Repos { repos }) => known.repos = repos,
            Some(DaemonMessage::Workspaces { workspaces }) => known.workspaces = workspaces,
            Some(DaemonMessage::Sessions { .. }) => return Ok(known),
            Some(_) => {}
        }
    }
}

/// The agent `--agent` names, or `not_found`; a plain shell has none.
fn agent_selection(
    agents: &[AgentDescriptor],
    agent: Option<&str>,
) -> Result<Option<AgentSelection>, Failure> {
    let Some(agent) = agent else {
        return Ok(None);
    };
    if !agents.iter().any(|known| known.id.0 == agent) {
        return Err(Failure::refused(
            Some(protocol::error_codes::NOT_FOUND.to_owned()),
            format!("unknown agent: {agent}"),
        ));
    }
    Ok(Some(AgentSelection {
        agent: AgentId(agent.to_owned()),
        options: OptionValues::default(),
    }))
}

/// Where the session runs. A repo runs in place on the branch it has out,
/// which the daemon reports, so the spawn switches nothing.
async fn spawn_target(
    connection: &mut Connection,
    repos: &[RepoEntry],
    workspaces: &[WorkspaceEntry],
    place: &Place,
) -> Result<SpawnTarget, Failure> {
    match place {
        Place::Cwd(cwd) => {
            let cwd = std::path::absolute(cwd)
                .map_err(|err| Failure::failed(format!("bad folder {cwd}: {err}")))?;
            Ok(SpawnTarget::Standalone {
                cwd: Some(cwd.to_string_lossy().into_owned()),
                add_dirs: Vec::new(),
            })
        }
        Place::Repo(name) => {
            let repo = repos
                .iter()
                .find(|repo| repo.name == *name)
                .ok_or_else(|| {
                    Failure::refused(
                        Some(protocol::error_codes::NOT_FOUND.to_owned()),
                        format!("unknown repo: {name}"),
                    )
                })?;
            if let Some(workspace) = workspaces
                .iter()
                .find(|workspace| workspace.member_repo_ids.contains(&repo.id))
            {
                return Err(Failure::refused(
                    Some(protocol::error_codes::INVALID_REQUEST.to_owned()),
                    format!(
                        "{} is a member of workspace {}; its sessions belong to the workspace. \
                         Spawn in the workspace from the app.",
                        repo.name, workspace.name
                    ),
                ));
            }
            let branch_name = current_branch(connection, repo).await?;
            Ok(SpawnTarget::Single {
                repo_id: repo.id.clone(),
                branch_name,
                base_branch: None,
                use_worktree: false,
                checkout_strategy: None,
                worktree_reuse: WorktreeReusePolicy::default(),
                existing_worktree: None,
            })
        }
    }
}

async fn current_branch(connection: &mut Connection, repo: &RepoEntry) -> Result<String, Failure> {
    ask(
        connection,
        &ClientMessage::ListBranches {
            repo_id: repo.id.clone(),
        },
    )
    .await?;
    let current = await_reply(connection, "the repo's branch", |message| match message {
        DaemonMessage::Branches {
            repo_id, current, ..
        } if repo_id == repo.id => Some(current),
        _ => None,
    })
    .await?;
    current.ok_or_else(|| {
        Failure::failed(format!(
            "repo {} has no branch checked out; use --cwd {}",
            repo.name, repo.path
        ))
    })
}

/// The answer to this connection's spawn, once `message` is it.
fn spawn_reply(message: DaemonMessage) -> Option<Result<SessionSnapshot, Failure>> {
    match message {
        DaemonMessage::SessionUpdated {
            session,
            request_id,
        } if is_mine(request_id.as_deref()) => Some(Ok(session)),
        DaemonMessage::ActionFailed {
            title,
            detail,
            request_id,
            ..
        } if is_mine(request_id.as_deref()) => {
            Some(Err(Failure::failed(format!("{title}: {detail}"))))
        }
        DaemonMessage::CheckoutConfirmRequired {
            branch,
            dirty_count,
            request_id,
            ..
        } if is_mine(request_id.as_deref()) => Some(Err(Failure::checkout_confirm(format!(
            "switching to {branch} needs a choice about {dirty_count} uncommitted changes"
        )))),
        _ => None,
    }
}

pub(super) async fn spawn(
    connection: &mut Connection,
    options: &SpawnOptions,
    json: bool,
) -> Result<String, Failure> {
    let known = initial_known(connection).await?;
    let agent = agent_selection(&known.agents, options.agent.as_deref())?;
    let target = spawn_target(connection, &known.repos, &known.workspaces, &options.place).await?;
    let mode = if agent.is_some() {
        SessionMode::Interactive
    } else {
        SessionMode::PlainShell
    };
    let request = SpawnRequest {
        label: options.label.clone(),
        target,
        mode,
        initial_prompt: options.prompt.clone(),
        dangerously_skip_permissions: false,
        agent,
        extra_args: Vec::new(),
        extra_env: Vec::new(),
        secret_env_keys: Vec::new(),
        prompt_injector: None,
        request_id: Some(REQUEST_ID.to_owned()),
        resume_conversation: None,
    };
    ask(connection, &ClientMessage::SpawnSession(request)).await?;
    let session = await_reply(connection, "the new session", spawn_reply).await??;
    Ok(if json {
        render::spawned_json(&session)
    } else {
        format!("{}\n", session.id)
    })
}

/// The message that delivers `payload` to `session`.
fn input_message(session: &SessionSnapshot, payload: &Payload) -> ClientMessage {
    let bytes = match payload {
        Payload::Text { text, raw } if *raw || session.agent.is_none() => {
            let mut bytes = text.clone().into_bytes();
            bytes.push(b'\r');
            bytes
        }
        Payload::Text { text, .. } => {
            return ClientMessage::ChatSend {
                session_id: session.id.clone(),
                text: text.clone(),
                request_id: Some(SEND_REQUEST_ID.to_owned()),
            };
        }
        Payload::Keys(bytes) => bytes.clone(),
    };
    ClientMessage::SendInput {
        session_id: session.id.clone(),
        data_b64: base64::engine::general_purpose::STANDARD.encode(bytes),
        automatic: false,
    }
}

/// Read until the barrier's `DumpScreen` is answered, so the daemon has taken
/// everything sent before it. A session with no screen answers the barrier
/// all the same; a refusal of the text on the way fails the verb.
async fn await_barrier(connection: &mut Connection) -> Result<(), Failure> {
    loop {
        match connection.next_message().await {
            None => return Err(closed_early("the barrier")),
            Some(DaemonMessage::ScreenDump { request_id, .. })
                if is_mine(request_id.as_deref()) =>
            {
                return Ok(());
            }
            Some(DaemonMessage::Error {
                message,
                request_id,
                code,
            }) => match request_id.as_deref() {
                Some(REQUEST_ID) if code.as_deref() == Some(protocol::error_codes::NO_SCREEN) => {
                    return Ok(());
                }
                Some(SEND_REQUEST_ID | REQUEST_ID) => return Err(Failure::refused(code, message)),
                _ => {}
            },
            Some(_) => {}
        }
    }
}

pub(super) async fn send(
    connection: &mut Connection,
    welcome: &Welcome,
    session_id: &str,
    payload: &Payload,
    json: bool,
) -> Result<String, Failure> {
    check_screen_dump(welcome)?;
    let sessions = initial_sessions(connection).await?;
    let session = find(&sessions, session_id)?;
    if waiting::is_over(session.status) {
        return Err(Failure::ended(session_id));
    }
    ask(connection, &input_message(session, payload)).await?;
    dump_screen(connection, session_id).await?;
    await_barrier(connection).await?;
    Ok(id_reply(session_id, json))
}

fn id_reply(session_id: &str, json: bool) -> String {
    if json {
        render::id_json(session_id)
    } else {
        String::new()
    }
}

pub(super) async fn kill(
    connection: &mut Connection,
    session_id: &str,
    json: bool,
) -> Result<String, Failure> {
    let sessions = initial_sessions(connection).await?;
    let session = find(&sessions, session_id)?;
    if !waiting::is_over(session.status) {
        ask(
            connection,
            &ClientMessage::StopSession {
                session_id: session_id.to_owned(),
                cleanup: Vec::new(),
            },
        )
        .await?;
        loop {
            let Some(message) = connection.next_message().await else {
                return Err(closed_early("the stopped session"));
            };
            if waiting::shows_stopped(&message, session_id) {
                break;
            }
        }
    }
    Ok(id_reply(session_id, json))
}

async fn wait_status(
    connection: &mut Connection,
    session_id: &str,
    target: SessionStatus,
    json: bool,
) -> Result<String, Failure> {
    let mut state = WaitState::new(session_id, Some(target));
    loop {
        let Some(message) = connection.next_message().await else {
            return Err(closed_early("the status"));
        };
        match state.step(&message) {
            Some(Outcome::Reached(status)) => {
                return Ok(if json {
                    render::status_json(session_id, status)
                } else {
                    format!("{}\n", render::status_name(status))
                });
            }
            Some(Outcome::Unknown) => return Err(Failure::unknown_session(session_id)),
            Some(Outcome::Ended | Outcome::Over) => return Err(Failure::ended(session_id)),
            None => {}
        }
    }
}

/// Follow the session's screen until `regex` matches it, each output or
/// status change asking for a new dump, and the tick for one more in case the
/// output overtook the screen.
async fn wait_text(
    connection: &mut Connection,
    welcome: &Welcome,
    session_id: &str,
    regex: &Regex,
    json: bool,
) -> Result<String, Failure> {
    check_screen_dump(welcome)?;
    let mut text = TextWait::new(session_id, regex);
    let mut tick = tokio::time::interval(SCREEN_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let step = tokio::select! {
            message = connection.next_message() => {
                let Some(message) = message else {
                    return Err(closed_early("the screen"));
                };
                text.step(&message)
            }
            _ = tick.tick() => text.tick(),
        };
        match step {
            TextStep::Read => {}
            TextStep::Follow => {
                ask(
                    connection,
                    &ClientMessage::LoadScrollback {
                        session_id: session_id.to_owned(),
                        request_id: None,
                    },
                )
                .await?;
                dump_screen(connection, session_id).await?;
            }
            TextStep::Dump => dump_screen(connection, session_id).await?,
            TextStep::Matched(line) => {
                return Ok(if json {
                    render::line_json(session_id, &line)
                } else {
                    format!("{line}\n")
                });
            }
            TextStep::Unknown => return Err(Failure::unknown_session(session_id)),
            TextStep::Ended => return Err(Failure::ended(session_id)),
            TextStep::Refused(code, message) => return Err(Failure::refused(code, message)),
        }
    }
}

pub(super) async fn wait(
    connection: &mut Connection,
    welcome: &Welcome,
    request: &WaitRequest<'_>,
) -> Result<String, Failure> {
    let WaitRequest {
        session_id,
        until,
        limit,
        json,
    } = *request;
    let waited = async {
        match until {
            Until::Status(status) => wait_status(connection, session_id, *status, json).await,
            Until::Text(regex) => wait_text(connection, welcome, session_id, regex, json).await,
        }
    };
    tokio::time::timeout(limit, waited).await.map_err(|_| {
        let seconds = limit.as_secs();
        Failure::timed_out(format!(
            "session {session_id} did not get there within {seconds} s"
        ))
    })?
}

async fn watch_lines(
    connection: &mut Connection,
    session_id: &str,
    json: bool,
    out: &mut impl Write,
) -> Result<(), Failure> {
    let mut state = WatchState::new(session_id);
    loop {
        let Some(message) = connection.next_message().await else {
            return Err(Failure::failed("rustling-tulipd closed the connection"));
        };
        if let Some(line) = state.step(&message) {
            let text = if json {
                render::watch_json(&line)
            } else {
                render::watch_text(&line)
            };
            out.write_all(text.as_bytes())
                .and_then(|()| out.flush())
                .map_err(|err| Failure::failed(format!("could not write to stdout: {err}")))?;
        }
        match state.end() {
            None => {}
            Some(WatchEnd::Removed) => return Ok(()),
            Some(WatchEnd::Unknown) => return Err(Failure::unknown_session(session_id)),
        }
    }
}

pub(super) async fn watch(
    connection: &mut Connection,
    session_id: &str,
    limit: Option<Duration>,
    json: bool,
    out: &mut impl Write,
) -> Result<String, Failure> {
    let watched = watch_lines(connection, session_id, json, out);
    match limit {
        Some(limit) => tokio::time::timeout(limit, watched)
            .await
            .unwrap_or(Ok(()))?,
        None => watched.await?,
    }
    Ok(String::new())
}
