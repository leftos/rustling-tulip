//! The daemon-driven account login: one slot on [`Accounts`], so at most one
//! login runs at a time, and the run task that is the only actor with side
//! effects.
//!
//! A start (`StartAccountLogin`, local-only) claims the slot from Idle and
//! starts the run task, storing its handle while it still holds the slot's
//! lock. The run task makes the run's scratch folder (`<login root>/<run id>`,
//! owner-only), asks the service for the login command, spawns it through the
//! [`LoginRunner`] and answers the requester `Started`. It then waits on the
//! session's exit, a cancel, the shutdown watch and the [`LOGIN_TIMEOUT`]
//! deadline in one biased `select!`, exit first. The first end claimed under
//! the slot's lock decides the terminal state; only an exit with code 0 that
//! nothing beat moves on to the capture.
//!
//! Every end other than an exit the client already discards stops the
//! session (unless its exit is already in), waits up to [`STOP_WAIT`] for the
//! exit, and has the runner discard the session's record. The scratch folder
//! is removed only once the child is confirmed gone and no capture runs;
//! otherwise it is left for the start-up sweep. On Windows, a folder a
//! process the session left still holds (a wrapped CLI's grandchild whose
//! cwd it is) is retried for up to `SCRATCH_RELEASE_WAIT` before it too is
//! left for the sweep. The slot is Idle again before the terminal state is
//! sent, so a client that starts again on it is not refused busy.
//!
//! The slot's lock is never held across an `.await`, a spawn of the login,
//! the capture, or a send.
use super::Accounts;
use crate::server::{CommandSession, CommandSpec, Hub, StateEvent};
use agent_api::{AccountsFailure, LoginCapture, LoginCommand};
use futures::FutureExt as _;
use futures::future::BoxFuture;
use protocol::{AccountLoginState, DaemonMessage};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::{Notify, broadcast, mpsc, oneshot, watch};
use tracing::{debug, info, warn};

/// How long a login may run before the daemon stops it as timed out.
pub(crate) const LOGIN_TIMEOUT: Duration = Duration::from_secs(600);

/// How long a run waits for the login's exit after stopping its session;
/// past it the scratch folder is left for the start-up sweep.
pub(crate) const STOP_WAIT: Duration = Duration::from_secs(30);

/// A run's id: the session id its login session gets, which names its
/// scratch folder too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunId(String);

impl RunId {
    fn new() -> Self {
        Self(crate::session::new_id())
    }

    /// The id as the session and the scratch folder carry it.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// How the login's session ended, as its exit notice reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExitFacts {
    /// The child exited by itself with `code`.
    Exited { code: i32 },
    /// The session was stopped before it exited (a user's Stop, or the
    /// run's own stop).
    StoppedFirst,
    /// The terminal was lost; the child's fate is unknown.
    Abandoned,
    /// The session's record was gone when the exit came.
    RecordGone,
}

/// What a login session's exit future yields: its facts (`None` when the
/// notice's sender was dropped unsent, so the child is not confirmed gone)
/// and the last lines its screen showed.
#[derive(Debug)]
pub(crate) struct LoginExit {
    pub(crate) facts: Option<ExitFacts>,
    pub(crate) tail: Vec<String>,
}

/// A spawned login session: its id and the future its exit resolves.
pub(crate) struct LoginSession {
    pub(crate) session_id: String,
    pub(crate) exit: BoxFuture<'static, LoginExit>,
}

/// What runs the login's command in a terminal session of its own.
pub(crate) trait LoginRunner: Send + Sync + 'static {
    /// Spawn `cmd` for run `run` with its config in `scratch`; the session's
    /// id is `run`.
    fn spawn(
        &self,
        run: &str,
        scratch: &Path,
        cmd: LoginCommand,
    ) -> BoxFuture<'static, anyhow::Result<LoginSession>>;

    /// Stop the session `session_id`; its exit reaches the session's exit
    /// future.
    fn stop(&self, session_id: &str) -> BoxFuture<'static, ()>;

    /// Discard the session `session_id`'s record; a record already gone is
    /// no error.
    fn discard(&self, session_id: &str) -> BoxFuture<'static, ()>;
}

/// What a spawn answers on a runner whose hub is not set yet.
const NO_RUNNER: &str = "this daemon cannot run an account login";

/// The login session's title.
const LOGIN_TITLE: &str = "Log in a new account";

/// The daemon's runner: the agent's program, resolved as for any spawn, run
/// directly in a terminal session of its own on the hub, stopped with
/// `stop_session_if_live` and discarded as a pane close discards a session.
pub(crate) struct SessionRunner {
    agent: protocol::AgentId,
    /// The hub, set once it is built; a spawn before that fails.
    hub: Arc<OnceLock<Hub>>,
    #[cfg(test)]
    spawner: Option<Arc<dyn TestSpawner>>,
}

/// A test's stand-in for [`crate::server::spawn_command_session`].
#[cfg(test)]
pub(crate) trait TestSpawner: Send + Sync {
    fn spawn(&self, hub: &Hub, spec: CommandSpec) -> anyhow::Result<CommandSession>;
}

impl SessionRunner {
    /// The runner of `agent`'s login, on the hub `hub` is set to.
    pub(crate) fn new(agent: protocol::AgentId, hub: Arc<OnceLock<Hub>>) -> Self {
        Self {
            agent,
            hub,
            #[cfg(test)]
            spawner: None,
        }
    }

    /// This runner, spawning its sessions through `spawner`.
    #[cfg(test)]
    pub(crate) fn spawning_with(mut self, spawner: Arc<dyn TestSpawner>) -> Self {
        self.spawner = Some(spawner);
        self
    }
}

impl LoginRunner for SessionRunner {
    fn spawn(
        &self,
        run: &str,
        scratch: &Path,
        cmd: LoginCommand,
    ) -> BoxFuture<'static, anyhow::Result<LoginSession>> {
        let hub = self.hub.get().cloned();
        let agent = self.agent.clone();
        let session_id = run.to_owned();
        let cwd = scratch.to_owned();
        #[cfg(test)]
        let spawner = self.spawner.clone();
        Box::pin(async move {
            let hub = hub.ok_or_else(|| anyhow::anyhow!(NO_RUNNER))?;
            let (program, mut args) = crate::server::agent_program(&hub, &agent)?;
            args.extend(cmd.args);
            let passthrough: &'static [&'static str] =
                agent_registry::plugin(&agent).map_or(&[], |plugin| plugin.passthrough_vars());
            let spec = CommandSpec {
                session_id,
                program,
                args,
                passthrough,
                env: cmd.env,
                env_remove: cmd.env_remove,
                cwd,
                label: LOGIN_TITLE.to_owned(),
                user_label: Some(LOGIN_TITLE.to_owned()),
            };
            #[cfg(test)]
            if let Some(spawner) = spawner {
                return Ok(login_session(spawner.spawn(&hub, spec)?));
            }
            let session = crate::server::spawn_command_session(&hub, spec).await?;
            Ok(login_session(session))
        })
    }

    fn stop(&self, session_id: &str) -> BoxFuture<'static, ()> {
        let hub = self.hub.get().cloned();
        let session_id = session_id.to_owned();
        Box::pin(async move {
            let Some(hub) = hub else {
                return;
            };
            // A session already stopped (a drain, a user's Stop) or discarded
            // is expected: its exit still comes, and it is never orphan-killed.
            if !crate::server::stop_session_if_live(&hub, &session_id).await {
                debug!(%session_id, "the login session holds no live handle to stop; waiting for its exit");
            }
        })
    }

    fn discard(&self, session_id: &str) -> BoxFuture<'static, ()> {
        let hub = self.hub.get().cloned();
        let session_id = session_id.to_owned();
        Box::pin(async move {
            if let Some(hub) = hub {
                discard_record(&hub, &session_id).await;
            }
        })
    }
}

/// Discard `session_id`'s record, its sidecar and its session dir, as a
/// pane close does; one already gone is a no-op.
async fn discard_record(hub: &Hub, session_id: &str) {
    // Nothing a standalone session's discard sends is for anyone.
    let (out_tx, _unread) = mpsc::unbounded_channel();
    crate::server::discard_session(hub, session_id, &[], &out_tx).await;
}

/// The run's view of a spawned command session: its exit future feeds the
/// session's output into a screen of its own until the exit notice comes,
/// then reads that screen's rows as the tail.
fn login_session(session: CommandSession) -> LoginSession {
    let CommandSession {
        snapshot,
        exit,
        output,
        cols,
        rows,
    } = session;
    LoginSession {
        session_id: snapshot.id,
        exit: exit_future(exit, output, cols, rows),
    }
}

/// See [`login_session`]. Every chunk reaches the output before the exit is
/// handled, so the drain after the notice reads the last one too; a lagged
/// receiver only loses older chunks.
fn exit_future(
    mut exit: oneshot::Receiver<crate::session::ExitFacts>,
    mut output: broadcast::Receiver<Vec<u8>>,
    cols: u16,
    rows: u16,
) -> BoxFuture<'static, LoginExit> {
    use broadcast::error::{RecvError, TryRecvError};
    Box::pin(async move {
        let mut screen = crate::screen::Screen::new(cols, rows);
        let mut reading = true;
        let facts = loop {
            tokio::select! {
                biased;
                facts = &mut exit => break facts.ok(),
                chunk = output.recv(), if reading => match chunk {
                    Ok(bytes) => screen.feed(&bytes),
                    Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => reading = false,
                },
            }
        };
        loop {
            match output.try_recv() {
                Ok(bytes) => screen.feed(&bytes),
                Err(TryRecvError::Lagged(_)) => {}
                Err(TryRecvError::Empty | TryRecvError::Closed) => break,
            }
        }
        LoginExit {
            facts: facts.map(exit_facts),
            tail: screen.rows(),
        }
    })
}

/// The run's reading of how the session's exit was handled.
fn exit_facts(facts: crate::session::ExitFacts) -> ExitFacts {
    use crate::session::ExitFacts as Handled;
    match facts {
        Handled::Exited { code } => ExitFacts::Exited { code },
        Handled::StoppedFirst => ExitFacts::StoppedFirst,
        Handled::Abandoned => ExitFacts::Abandoned,
        Handled::RecordGone => ExitFacts::RecordGone,
    }
}

/// Claim a starting or running login's end for the daemon's shutdown and
/// wake its run, before a drain stops every session: the run then reads its
/// session's stop as the shutdown, not as a stop from outside. A login being
/// captured or already ending keeps its end.
pub(crate) fn claim_shutdown(accounts: &Accounts) {
    let mut slot = crate::sync::lock(&accounts.login);
    let (run, session_id, wake) = match std::mem::replace(&mut *slot, LoginSlot::Idle) {
        LoginSlot::Starting { run } => (run, None, None),
        LoginSlot::Running {
            run,
            session_id,
            wake,
        } => (run, Some(session_id), Some(wake)),
        other => {
            *slot = other;
            return;
        }
    };
    info!(
        run = run.as_str(),
        "the account login ends for the daemon's shutdown"
    );
    *slot = LoginSlot::Ending {
        run,
        session_id,
        reason: EndReason::Shutdown,
    };
    drop(slot);
    if let Some(wake) = wake {
        wake.notify_one();
    }
}

/// The start-up sweep, before any client connects: for each entry under
/// the login `root`, the login a previous run left there. Its session is
/// stopped and waited for when it still runs, then discarded with its
/// sidecar and session dir; the service's scratch login is cleaned up and
/// the entry removed. An entry whose child does not end within
/// [`STOP_WAIT`] is left for the next start. Returns every entry's id, none
/// of which may be resumed.
pub(crate) async fn sweep(hub: &Hub, root: &Path) -> HashSet<String> {
    let mut swept = HashSet::new();
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return swept,
        Err(err) => {
            warn!(%err, root = %root.display(), "could not read the account login folder");
            return swept;
        }
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
            warn!(entry = %path.display(), "an account login folder whose name is not text is left");
            continue;
        };
        info!(session_id = %id, "the start-up sweep clears a login left by an earlier run");
        swept.insert(id.clone());
        if !sweep_session(hub, &id).await {
            warn!(
                session_id = %id,
                bound = ?STOP_WAIT,
                "a left login's child did not end in time; its folder is left for the next start"
            );
            continue;
        }
        if let Some(accounts) = &hub.accounts {
            discard_login(Arc::clone(accounts.service()), path.clone()).await;
        }
        remove_scratch(path).await;
    }
    swept
}

/// Stop the session `session_id` names when its child still runs and wait
/// until it is gone, then discard it; with no session, drop any sidecar and
/// session dir it left. Whether the child is confirmed gone.
async fn sweep_session(hub: &Hub, session_id: &str) -> bool {
    let Some(rec) = hub.sessions.get(session_id) else {
        hub.sessions.delete_sidecar(session_id);
        hub.sessions.delete_session_dir(session_id);
        return true;
    };
    let live = crate::sync::lock(&rec).pty.is_some();
    drop(rec);
    if live {
        // Subscribed before the stop, then the record re-read after each
        // event, so the exit is never missed. No client can discard the
        // record before the bind, so a missing one is a handled exit.
        let mut events = hub.sessions.subscribe();
        if !crate::server::stop_session_if_live(hub, session_id).await {
            debug!(%session_id, "the left login holds no live handle to stop");
        }
        let gone = || {
            hub.sessions
                .get(session_id)
                .is_none_or(|rec| crate::server::child_gone(&crate::sync::lock(&rec)))
        };
        let wait = async {
            while !gone() {
                match events.recv().await {
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return false,
                }
            }
            true
        };
        if !tokio::time::timeout(STOP_WAIT, wait).await.unwrap_or(false) {
            return false;
        }
    }
    discard_record(hub, session_id).await;
    true
}

/// What a handle that runs logins holds: the login root (`<data
/// dir>/account-login`), the agent whose login runs, and the runner.
pub(crate) struct LoginSetup {
    pub(crate) root: PathBuf,
    pub(crate) agent: protocol::AgentId,
    pub(crate) runner: Arc<dyn LoginRunner>,
}

/// Where a reply to one request goes: the requester's connection and the
/// request's id.
#[derive(Debug, Clone)]
pub(crate) struct Reply {
    out_tx: mpsc::UnboundedSender<DaemonMessage>,
    request_id: Option<String>,
}

impl Reply {
    pub(crate) fn new(
        out_tx: mpsc::UnboundedSender<DaemonMessage>,
        request_id: Option<String>,
    ) -> Self {
        Self { out_tx, request_id }
    }

    /// The request's id.
    pub(crate) fn id(&self) -> Option<String> {
        self.request_id.clone()
    }

    /// Send `msg` to the requester.
    pub(crate) fn send(&self, msg: DaemonMessage) {
        let _ = self.out_tx.send(msg);
    }

    /// Answer `AccountLogin` in `state`.
    fn login(&self, state: AccountLoginState, session_id: Option<String>, message: Option<String>) {
        let _ = self.out_tx.send(DaemonMessage::AccountLogin {
            state,
            session_id,
            message,
            request_id: self.request_id.clone(),
        });
    }
}

/// The first claim on how a run ends.
#[derive(Debug, Clone)]
pub(crate) enum EndReason {
    /// A cancel, whose requester gets the terminal state directly.
    Cancel {
        reply: Reply,
    },
    Timeout,
    Shutdown,
    Exit(ExitFacts),
    Captured(Result<LoginCapture, AccountsFailure>),
    /// The capture panicked; what it left is unknown.
    CaptureStopped,
}

/// Where the login stands.
#[derive(Debug)]
pub(crate) enum LoginSlot {
    Idle,
    Starting {
        run: RunId,
    },
    Running {
        run: RunId,
        session_id: String,
        wake: Arc<Notify>,
    },
    Capturing {
        run: RunId,
        session_id: String,
    },
    Ending {
        run: RunId,
        session_id: Option<String>,
        reason: EndReason,
    },
}

impl LoginSlot {
    /// The run the slot holds, if any.
    fn run(&self) -> Option<&RunId> {
        match self {
            Self::Idle => None,
            Self::Starting { run, .. }
            | Self::Running { run, .. }
            | Self::Capturing { run, .. }
            | Self::Ending { run, .. } => Some(run),
        }
    }
}

/// A start or a cancel on a handle with no login setup.
const NO_LOGIN: &str = "this host cannot run an account login";
/// A start while the shutdown watch has flipped; also the shutdown's end.
const DRAINING: &str = "the daemon is shutting down";
/// A second start.
const ALREADY_RUNNING: &str = "an account login is already running";
/// A cancel with no run.
const NOT_RUNNING: &str = "no account login is running";
/// A cancel while the login starts.
const STARTING: &str = "the login is starting";
/// A cancel during the capture.
const FINISHING: &str = "the login is finishing";
/// A cancel after the run's end was claimed.
const ENDING: &str = "the login is ending";
/// The end of a run task that stopped any other way than its own.
const UNEXPECTED: &str = "the login stopped unexpectedly";
/// The deadline's end.
const TIMED_OUT: &str = "the login timed out";
/// A stop from outside the run.
const SESSION_STOPPED: &str = "the login session was stopped";
/// A lost terminal.
const LOST_TERMINAL: &str = "lost the login terminal";
/// How many of the screen's last non-blank lines a failure carries.
const TAIL_LINES: usize = 5;

/// A capture that panicked.
const CAPTURE_STOPPED: &str = "the login capture stopped";

/// Answer `reply` `Error` with `code` and `message`.
fn refuse(reply: &Reply, code: &str, message: &str) {
    reply.send(crate::server::account_error(
        code,
        message.to_owned(),
        reply.id(),
    ));
}

/// Handle a `StartAccountLogin` from a local connection: refuse it, or claim
/// the slot and start the run task, which answers the requester.
pub(crate) fn start(
    accounts: &Arc<Accounts>,
    events: broadcast::Sender<StateEvent>,
    shutdown: watch::Receiver<bool>,
    reply: Reply,
) {
    use protocol::error_codes::{ACCOUNT_BUSY, ACCOUNT_DRAINING, ACCOUNT_UNSUPPORTED};
    let Some(setup) = accounts.login_setup() else {
        refuse(&reply, ACCOUNT_UNSUPPORTED, NO_LOGIN);
        return;
    };
    // The watch is read under the slot's lock, where the shutdown's join
    // takes the task: a start either refuses here or stores a task the join
    // waits on.
    let mut slot = crate::sync::lock(&accounts.login);
    let refusal = if is_down(&shutdown) {
        Some((ACCOUNT_DRAINING, DRAINING))
    } else if matches!(*slot, LoginSlot::Idle) {
        None
    } else {
        Some((ACCOUNT_BUSY, ALREADY_RUNNING))
    };
    if let Some((code, message)) = refusal {
        drop(slot);
        refuse(&reply, code, message);
        return;
    }
    let run = RunId::new();
    info!(run = run.as_str(), agent = %setup.agent.0, "an account login starts");
    *slot = LoginSlot::Starting { run: run.clone() };
    let task = tokio::spawn(run_login(RunCtx {
        accounts: Arc::clone(accounts),
        root: setup.root.clone(),
        runner: Arc::clone(&setup.runner),
        run,
        events,
        shutdown,
        reply,
    }));
    *crate::sync::lock(&accounts.login_task) = Some(task);
    drop(slot);
}

/// Handle a `CancelAccountLogin` from a local connection: claim a running
/// login's end for the cancel and wake its run task, which answers the
/// requester with the terminal state; or refuse it.
pub(crate) fn cancel(accounts: &Accounts, reply: Reply) {
    use protocol::error_codes::{ACCOUNT_BUSY, ACCOUNT_REFUSED, ACCOUNT_UNSUPPORTED};
    if accounts.login_setup().is_none() {
        refuse(&reply, ACCOUNT_UNSUPPORTED, NO_LOGIN);
        return;
    }
    let mut slot = crate::sync::lock(&accounts.login);
    match std::mem::replace(&mut *slot, LoginSlot::Idle) {
        LoginSlot::Running {
            run,
            session_id,
            wake,
        } => {
            info!(run = run.as_str(), "the account login is cancelled");
            *slot = LoginSlot::Ending {
                run,
                session_id: Some(session_id),
                reason: EndReason::Cancel { reply },
            };
            drop(slot);
            wake.notify_one();
        }
        other => {
            let (code, message) = match &other {
                LoginSlot::Idle => (ACCOUNT_REFUSED, NOT_RUNNING),
                LoginSlot::Starting { .. } => (ACCOUNT_BUSY, STARTING),
                LoginSlot::Capturing { .. } => (ACCOUNT_BUSY, FINISHING),
                LoginSlot::Running { .. } | LoginSlot::Ending { .. } => (ACCOUNT_BUSY, ENDING),
            };
            *slot = other;
            drop(slot);
            refuse(&reply, code, message);
        }
    }
}

/// What a run task owns.
struct RunCtx {
    accounts: Arc<Accounts>,
    root: PathBuf,
    runner: Arc<dyn LoginRunner>,
    run: RunId,
    events: broadcast::Sender<StateEvent>,
    shutdown: watch::Receiver<bool>,
    reply: Reply,
}

/// What every phase of a run reads.
struct Phase<'a> {
    accounts: &'a Accounts,
    runner: &'a Arc<dyn LoginRunner>,
    run: &'a RunId,
    scratch: &'a Path,
    events: &'a broadcast::Sender<StateEvent>,
}

/// Whether the shutdown watch has flipped.
fn is_down(shutdown: &watch::Receiver<bool>) -> bool {
    *shutdown.borrow()
}

/// Resolves once the shutdown watch flips; never when its sender is gone.
async fn flipped(shutdown: &mut watch::Receiver<bool>) {
    let gone = shutdown.wait_for(|down| *down).await.is_err();
    if gone {
        std::future::pending::<()>().await;
    }
}

/// Broadcast `state` to every connection, with no request id.
fn broadcast_login(
    events: &broadcast::Sender<StateEvent>,
    state: AccountLoginState,
    session_id: Option<String>,
    message: Option<String>,
) {
    let _ = events.send(StateEvent::AccountLogin {
        state,
        session_id,
        message,
    });
}

/// Frees the slot if the run task ends any other way than its own (a panic,
/// an abort): sets Idle, answers a start still waiting `Error`, or else
/// broadcasts `Failed`, and answers a waiting cancel. It leaves the folder
/// and the session for the start-up sweep.
struct RunGuard {
    accounts: Arc<Accounts>,
    run: RunId,
    events: broadcast::Sender<StateEvent>,
    /// The start's requester, until `Started` is sent.
    start_reply: Option<Reply>,
    /// The session, once spawned.
    session_id: Option<String>,
    done: bool,
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        warn!(
            run = self.run.as_str(),
            "an account login run ended unexpectedly; its folder and session are left for the start-up sweep"
        );
        let taken = {
            let mut slot = crate::sync::lock(&self.accounts.login);
            if slot.run() == Some(&self.run) {
                Some(std::mem::replace(&mut *slot, LoginSlot::Idle))
            } else {
                None
            }
        };
        if let Some(reply) = self.start_reply.take() {
            refuse(&reply, protocol::error_codes::ACCOUNT_FAILED, UNEXPECTED);
            return;
        }
        let message = Some(UNEXPECTED.to_owned());
        if let Some(LoginSlot::Ending {
            reason: EndReason::Cancel { reply },
            ..
        }) = taken
        {
            reply.login(
                AccountLoginState::Failed,
                self.session_id.clone(),
                message.clone(),
            );
        }
        broadcast_login(
            &self.events,
            AccountLoginState::Failed,
            self.session_id.clone(),
            message,
        );
    }
}

/// The run task: everything a run does, from the claim until Idle.
async fn run_login(ctx: RunCtx) {
    let RunCtx {
        accounts,
        root,
        runner,
        run,
        events,
        mut shutdown,
        reply,
    } = ctx;
    let mut guard = RunGuard {
        accounts: Arc::clone(&accounts),
        run: run.clone(),
        events: events.clone(),
        start_reply: Some(reply),
        session_id: None,
        done: false,
    };
    let scratch = root.join(run.as_str());
    let phase = Phase {
        accounts: &accounts,
        runner: &runner,
        run: &run,
        scratch: &scratch,
        events: &events,
    };
    let wake = Arc::new(Notify::new());
    let Some(session) = begin(&phase, &root, &shutdown, &wake, &mut guard).await else {
        return;
    };
    let LoginSession {
        session_id,
        mut exit,
    } = session;
    // Started goes out before anything is awaited, so an exit already in
    // still comes after it.
    if let Some(reply) = guard.start_reply.take() {
        reply.login(AccountLoginState::Started, Some(session_id.clone()), None);
    }
    broadcast_login(
        &events,
        AccountLoginState::Started,
        Some(session_id.clone()),
        None,
    );
    let ended = watch_run(&accounts, &run, &wake, &mut shutdown, &mut exit).await;
    capture_if_due(&accounts, &run, &scratch).await;
    finish(&phase, &mut guard, &session_id, &mut exit, ended).await;
}

/// The ending phase: stop the session if its exit is not in, clean up once
/// the child is confirmed gone, refresh after a capture, set Idle, then send
/// the terminal state.
async fn finish(
    phase: &Phase<'_>,
    guard: &mut RunGuard,
    session_id: &str,
    exit: &mut BoxFuture<'static, LoginExit>,
    ended: Option<LoginExit>,
) {
    let Some(reason) = end_reason(phase.accounts, phase.run) else {
        // The slot lost the run: the guard frees what it can.
        return;
    };
    let ended = settle(phase, session_id, exit, ended).await;
    let gone = ended.as_ref().is_some_and(|done| done.facts.is_some());
    let tail = ended.map(|done| done.tail).unwrap_or_default();
    if gone {
        clean_up(phase, &reason, session_id).await;
    } else {
        warn!(
            run = phase.run.as_str(),
            "the login child is not confirmed gone; its folder is left for the start-up sweep"
        );
    }
    if matches!(reason, EndReason::Captured(_) | EndReason::CaptureStopped) {
        refresh_after_capture(phase).await;
    }
    let (state, message) = terminal(&reason, &tail);
    set_idle(phase.accounts, phase.run);
    guard.done = true;
    info!(run = phase.run.as_str(), ?state, "the account login ended");
    if let EndReason::Cancel { reply } = &reason {
        reply.login(state, Some(session_id.to_owned()), message.clone());
    }
    broadcast_login(phase.events, state, Some(session_id.to_owned()), message);
}

/// The session's exit, stopping the session first when the exit is not in.
/// Only the run stops its session, only once its end is claimed, and never
/// one whose exit is already in. `None` when the stop or the exit does not
/// come within [`STOP_WAIT`].
async fn settle(
    phase: &Phase<'_>,
    session_id: &str,
    exit: &mut BoxFuture<'static, LoginExit>,
    ended: Option<LoginExit>,
) -> Option<LoginExit> {
    if ended.is_some() {
        return ended;
    }
    if let Some(done) = (&mut *exit).now_or_never() {
        return Some(done);
    }
    if tokio::time::timeout(STOP_WAIT, phase.runner.stop(session_id))
        .await
        .is_err()
    {
        warn!(
            run = phase.run.as_str(),
            bound = ?STOP_WAIT,
            "the login session's stop did not finish in time; its folder and record are left for the start-up sweep"
        );
        return None;
    }
    let done = tokio::time::timeout(STOP_WAIT, exit).await.ok();
    if done.is_none() {
        warn!(
            run = phase.run.as_str(),
            bound = ?STOP_WAIT,
            "the login session did not end in time after its stop"
        );
    }
    done
}

/// Clean up after a child confirmed gone: the service's scratch login when
/// no capture deleted it, the session's record where the run discards it,
/// then the folder. A discard that does not finish within [`STOP_WAIT`]
/// leaves the record and the folder for the start-up sweep.
async fn clean_up(phase: &Phase<'_>, reason: &EndReason, session_id: &str) {
    if !matches!(reason, EndReason::Captured(_)) {
        discard_login(
            Arc::clone(phase.accounts.service()),
            phase.scratch.to_owned(),
        )
        .await;
    }
    if discards_record(reason)
        && tokio::time::timeout(STOP_WAIT, phase.runner.discard(session_id))
            .await
            .is_err()
    {
        warn!(
            run = phase.run.as_str(),
            bound = ?STOP_WAIT,
            "the login session's discard did not finish in time; its folder and record are left for the start-up sweep"
        );
        return;
    }
    remove_scratch(phase.scratch.to_owned()).await;
}

/// After a capture that ran, whatever it came to: have the driver re-read
/// the store, as an account write does; with no driver to broadcast it,
/// broadcast one direct read of the store, so `AccountsUpdated` comes before
/// the terminal state.
async fn refresh_after_capture(phase: &Phase<'_>) {
    if crate::server::accounts_refreshed(phase.accounts, "login capture").await {
        return;
    }
    let read = crate::server::read_account_snapshot(Arc::clone(phase.accounts.service())).await;
    let super::snapshot::Wire { accounts, auto } = super::snapshot::wire(&read);
    let _ = phase.events.send(StateEvent::Accounts { accounts, auto });
}

/// The Starting phase: make the folder, ask for the command and spawn it.
/// `None` when the run ended before a session existed: the folder is
/// removed, the slot is Idle and the requester has its `Error`.
async fn begin(
    phase: &Phase<'_>,
    root: &Path,
    shutdown: &watch::Receiver<bool>,
    wake: &Arc<Notify>,
    guard: &mut RunGuard,
) -> Option<LoginSession> {
    use protocol::error_codes::{ACCOUNT_DRAINING, ACCOUNT_FAILED};
    let prepared = prepare(
        Arc::clone(phase.accounts.service()),
        root.to_owned(),
        phase.scratch.to_owned(),
    )
    .await;
    let cmd = match prepared {
        Ok(cmd) => cmd,
        Err((code, message)) => {
            fail_start(phase, guard, code, &message).await;
            return None;
        }
    };
    if is_down(shutdown) || !is_starting(phase.accounts, phase.run) {
        fail_start(phase, guard, ACCOUNT_DRAINING, DRAINING).await;
        return None;
    }
    let spawned = phase
        .runner
        .spawn(phase.run.as_str(), phase.scratch, cmd)
        .await;
    let session = match spawned {
        Ok(session) => session,
        Err(err) => {
            let message = format!("{err:#}");
            warn!(run = phase.run.as_str(), err = %message, "the login could not spawn");
            fail_start(phase, guard, ACCOUNT_FAILED, &message).await;
            return None;
        }
    };
    guard.session_id = Some(session.session_id.clone());
    mark_spawned(phase, &session.session_id, shutdown, wake);
    Some(session)
}

/// Starting{run} → Running, or → Ending{Shutdown} when the watch flipped
/// while the spawn ran; an end already claimed gets the session's id.
fn mark_spawned(
    phase: &Phase<'_>,
    session_id: &str,
    shutdown: &watch::Receiver<bool>,
    wake: &Arc<Notify>,
) {
    let run = phase.run;
    let mut slot = crate::sync::lock(&phase.accounts.login);
    if matches!(&*slot, LoginSlot::Starting { run: held } if held == run) {
        *slot = if is_down(shutdown) {
            LoginSlot::Ending {
                run: run.clone(),
                session_id: Some(session_id.to_owned()),
                reason: EndReason::Shutdown,
            }
        } else {
            LoginSlot::Running {
                run: run.clone(),
                session_id: session_id.to_owned(),
                wake: Arc::clone(wake),
            }
        };
    } else if let LoginSlot::Ending {
        run: held,
        session_id: held_session @ None,
        ..
    } = &mut *slot
        && held == run
    {
        *held_session = Some(session_id.to_owned());
    }
}

/// End a run that never spawned: remove its folder, set Idle, answer the
/// requester `Error`.
async fn fail_start(phase: &Phase<'_>, guard: &mut RunGuard, code: &str, message: &str) {
    remove_scratch(phase.scratch.to_owned()).await;
    set_idle(phase.accounts, phase.run);
    guard.done = true;
    if let Some(reply) = guard.start_reply.take() {
        refuse(&reply, code, message);
    }
}

/// Make the run's folder (owner-only) and ask for the login command, off
/// the async threads; an `Error`'s code and message when either fails or the
/// command's two env lists share a name.
async fn prepare(
    service: super::SharedAccounts,
    root: PathBuf,
    scratch: PathBuf,
) -> Result<LoginCommand, (&'static str, String)> {
    use protocol::error_codes::ACCOUNT_FAILED;
    let done =
        tokio::task::spawn_blocking(move || -> Result<LoginCommand, (&'static str, String)> {
            make_scratch(&root, &scratch).map_err(|err| {
                warn!(%err, scratch = %scratch.display(), "could not make the login's folder");
                (
                    ACCOUNT_FAILED,
                    format!("could not make the login's folder: {err}"),
                )
            })?;
            let cmd = service.login_command(&scratch).map_err(|failure| {
                warn!(
                    kind = ?failure.kind,
                    code = %failure.code,
                    message = %failure.message,
                    "the login command was refused"
                );
                (
                    crate::server::account_code_of(failure.kind),
                    failure.message,
                )
            })?;
            if let Some(name) = shared_env_name(&cmd) {
                return Err((
                    ACCOUNT_FAILED,
                    format!("the login command both sets and removes {name}"),
                ));
            }
            Ok(cmd)
        })
        .await;
    done.unwrap_or_else(|err| {
        Err((
            ACCOUNT_FAILED,
            format!("the login's preparation stopped: {err}"),
        ))
    })
}

/// Make `scratch` under `root`, owner-only off Windows (on Windows it
/// inherits the data dir's DACL).
fn make_scratch(root: &Path, scratch: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(root)?;
    let builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::DirBuilderExt as _;
        let mut builder = builder;
        builder.mode(0o700);
        builder
    };
    builder.create(scratch)
}

/// A name the command both sets and removes: the spawn applies `env` first
/// and `env_remove` after, so it would be dropped. Names compare without case
/// on Windows, where the environment does.
fn shared_env_name(cmd: &LoginCommand) -> Option<&str> {
    let same = |a: &str, b: &str| {
        if cfg!(windows) {
            a.eq_ignore_ascii_case(b)
        } else {
            a == b
        }
    };
    cmd.env
        .iter()
        .map(|(name, _)| name.as_str())
        .find(|name| cmd.env_remove.iter().any(|removed| same(name, removed)))
}

/// Whether the slot is still Starting for `run`.
fn is_starting(accounts: &Accounts, run: &RunId) -> bool {
    matches!(&*crate::sync::lock(&accounts.login), LoginSlot::Starting { run: held } if held == run)
}

/// Whether the slot is still Running for `run`.
fn is_running(accounts: &Accounts, run: &RunId) -> bool {
    matches!(&*crate::sync::lock(&accounts.login), LoginSlot::Running { run: held, .. } if held == run)
}

/// The Running phase: wait, exit first, for the exit, a cancel's wake, the
/// shutdown watch or the deadline, until the run's end is claimed. The exit,
/// when it is what ended the wait.
async fn watch_run(
    accounts: &Accounts,
    run: &RunId,
    wake: &Notify,
    shutdown: &mut watch::Receiver<bool>,
    exit: &mut BoxFuture<'static, LoginExit>,
) -> Option<LoginExit> {
    let deadline = tokio::time::Instant::now() + LOGIN_TIMEOUT;
    // A wake is a hint: the slot is read again after each.
    while is_running(accounts, run) {
        tokio::select! {
            biased;
            done = &mut *exit => {
                claim_exit(accounts, run, done.facts);
                return Some(done);
            }
            () = wake.notified() => {}
            () = flipped(shutdown) => claim(accounts, run, EndReason::Shutdown),
            () = tokio::time::sleep_until(deadline) => claim(accounts, run, EndReason::Timeout),
        }
    }
    None
}

/// Running{run} → Ending{reason}; nothing when the slot no longer runs `run`.
fn claim(accounts: &Accounts, run: &RunId, reason: EndReason) {
    let mut slot = crate::sync::lock(&accounts.login);
    if let LoginSlot::Running {
        run: held,
        session_id,
        ..
    } = &*slot
        && held == run
    {
        info!(
            run = run.as_str(),
            ?reason,
            "the account login's end is claimed"
        );
        let session_id = Some(session_id.clone());
        *slot = LoginSlot::Ending {
            run: run.clone(),
            session_id,
            reason,
        };
    }
}

/// The exit's claim: Running{run} → Capturing on a clean exit, else
/// Ending{Exit}; a dropped notice reads as a lost terminal. Nothing when an
/// end was claimed first.
fn claim_exit(accounts: &Accounts, run: &RunId, facts: Option<ExitFacts>) {
    let mut slot = crate::sync::lock(&accounts.login);
    if let LoginSlot::Running {
        run: held,
        session_id,
        ..
    } = &*slot
        && held == run
    {
        let session_id = session_id.clone();
        *slot = match facts {
            Some(ExitFacts::Exited { code: 0 }) => LoginSlot::Capturing {
                run: run.clone(),
                session_id,
            },
            other => LoginSlot::Ending {
                run: run.clone(),
                session_id: Some(session_id),
                reason: EndReason::Exit(other.unwrap_or(ExitFacts::Abandoned)),
            },
        };
    }
}

/// The Capturing phase, when the slot is in it: run the capture off the
/// async threads, never aborted, then claim Ending with what it came to.
async fn capture_if_due(accounts: &Accounts, run: &RunId, scratch: &Path) {
    let capturing = matches!(
        &*crate::sync::lock(&accounts.login),
        LoginSlot::Capturing { run: held, .. } if held == run
    );
    if !capturing {
        return;
    }
    let service = Arc::clone(accounts.service());
    let dir = scratch.to_owned();
    let reason = match tokio::task::spawn_blocking(move || service.capture_login_from(&dir)).await {
        Ok(result) => {
            if let Err(failure) = &result {
                warn!(
                    run = run.as_str(),
                    kind = ?failure.kind,
                    code = %failure.code,
                    message = %failure.message,
                    "the login capture failed"
                );
            }
            EndReason::Captured(result)
        }
        Err(err) => {
            warn!(%err, run = run.as_str(), "the login capture stopped");
            EndReason::CaptureStopped
        }
    };
    let mut slot = crate::sync::lock(&accounts.login);
    if let LoginSlot::Capturing {
        run: held,
        session_id,
    } = &*slot
        && held == run
    {
        let session_id = Some(session_id.clone());
        *slot = LoginSlot::Ending {
            run: run.clone(),
            session_id,
            reason,
        };
    }
}

/// The claimed end reason of `run`, if the slot is Ending for it.
fn end_reason(accounts: &Accounts, run: &RunId) -> Option<EndReason> {
    match &*crate::sync::lock(&accounts.login) {
        LoginSlot::Ending {
            run: held, reason, ..
        } if held == run => Some(reason.clone()),
        _ => None,
    }
}

/// Set Idle, if the slot still holds `run`.
fn set_idle(accounts: &Accounts, run: &RunId) {
    let mut slot = crate::sync::lock(&accounts.login);
    if slot.run() == Some(run) {
        *slot = LoginSlot::Idle;
    }
}

/// Whether the run discards its session's record: every end but a self-exit,
/// whose record the client already discards.
fn discards_record(reason: &EndReason) -> bool {
    !matches!(
        reason,
        EndReason::Captured(_)
            | EndReason::CaptureStopped
            | EndReason::Exit(ExitFacts::Exited { .. })
    )
}

/// The terminal state and its message for `reason`, with the screen's last
/// lines after a failure's own message.
fn terminal(reason: &EndReason, tail: &[String]) -> (AccountLoginState, Option<String>) {
    let failed = |message: &str| (AccountLoginState::Failed, Some(with_tail(message, tail)));
    match reason {
        EndReason::Captured(Ok(_)) => (AccountLoginState::Succeeded, None),
        EndReason::Captured(Err(failure)) => failed(&failure.message),
        EndReason::CaptureStopped => failed(CAPTURE_STOPPED),
        EndReason::Cancel { .. } => (AccountLoginState::Cancelled, None),
        EndReason::Shutdown => (AccountLoginState::Cancelled, Some(DRAINING.to_owned())),
        EndReason::Timeout => failed(TIMED_OUT),
        EndReason::Exit(ExitFacts::Exited { code }) => {
            failed(&format!("the login exited with code {code}"))
        }
        EndReason::Exit(ExitFacts::StoppedFirst | ExitFacts::RecordGone) => (
            AccountLoginState::Cancelled,
            Some(SESSION_STOPPED.to_owned()),
        ),
        EndReason::Exit(ExitFacts::Abandoned) => failed(LOST_TERMINAL),
    }
}

/// `message`, then up to [`TAIL_LINES`] of `tail`'s last non-blank lines,
/// one per line.
fn with_tail(message: &str, tail: &[String]) -> String {
    let lines: Vec<&str> = tail
        .iter()
        .map(|line| line.trim_end())
        .filter(|line| !line.trim().is_empty())
        .collect();
    let mut out = message.to_owned();
    for line in &lines[lines.len().saturating_sub(TAIL_LINES)..] {
        out.push('\n');
        out.push_str(line);
    }
    out
}

/// Ask the service to clean up what the run left outside its folder; a
/// failure is logged.
async fn discard_login(service: super::SharedAccounts, scratch: PathBuf) {
    match tokio::task::spawn_blocking(move || service.discard_login(&scratch)).await {
        Ok(Ok(())) => {}
        Ok(Err(failure)) => warn!(
            code = %failure.code,
            message = %failure.message,
            "could not clean up after the login"
        ),
        Err(err) => warn!(%err, "the login cleanup stopped"),
    }
}

/// How long [`remove_scratch`] keeps retrying a folder a process the session
/// left still holds (a wrapped CLI's grandchild, whose cwd is the folder
/// until the tracer's job ends it). Longer than the tracer's slowest normal
/// exit after its child's: a 200 ms grace plus up to a 3 s reader join.
#[cfg(windows)]
const SCRATCH_RELEASE_WAIT: Duration = Duration::from_secs(5);

/// The pause between two tries at removing a held folder.
#[cfg(windows)]
const SCRATCH_RETRY: Duration = Duration::from_millis(50);

/// Windows' `ERROR_SHARING_VIOLATION`: another process holds the folder or a
/// file in it. No other target reports it.
#[cfg(windows)]
const SHARING_VIOLATION: i32 = 32;

/// Remove the run's folder off the async threads; one already gone is fine.
/// A folder still held by a process the session left is retried every
/// [`SCRATCH_RETRY`] for up to [`SCRATCH_RELEASE_WAIT`]; past that, and on
/// any other failure, it is left for the start-up sweep.
#[cfg(windows)]
async fn remove_scratch(scratch: PathBuf) {
    let shown = scratch.display().to_string();
    let deadline = tokio::time::Instant::now() + SCRATCH_RELEASE_WAIT;
    loop {
        let path = scratch.clone();
        match tokio::task::spawn_blocking(move || std::fs::remove_dir_all(&path)).await {
            Ok(Ok(())) => return,
            Ok(Err(err)) if err.kind() == std::io::ErrorKind::NotFound => return,
            Ok(Err(err))
                if err.raw_os_error() == Some(SHARING_VIOLATION)
                    && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(SCRATCH_RETRY).await;
            }
            Ok(Err(err)) => {
                warn!(
                    %err,
                    scratch = %shown,
                    "could not remove the login's folder; the start-up sweep removes it"
                );
                return;
            }
            Err(err) => {
                warn!(%err, scratch = %shown, "the login folder's removal stopped");
                return;
            }
        }
    }
}

/// Remove the run's folder off the async threads; one already gone is fine,
/// and any other failure is left for the start-up sweep.
#[cfg(not(windows))]
async fn remove_scratch(scratch: PathBuf) {
    let shown = scratch.display().to_string();
    match tokio::task::spawn_blocking(move || std::fs::remove_dir_all(&scratch)).await {
        Ok(Ok(())) => {}
        Ok(Err(err)) if err.kind() == std::io::ErrorKind::NotFound => {}
        Ok(Err(err)) => {
            warn!(
                %err,
                scratch = %shown,
                "could not remove the login's folder; the start-up sweep removes it"
            );
        }
        Err(err) => {
            warn!(%err, scratch = %shown, "the login folder's removal stopped");
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test fails with the message of the precondition it lost"
)]
mod tests {
    use super::*;
    use crate::accounts::SharedAccounts;
    use crate::history::test_support::{
        ScriptedExit, insert_live, insert_live_notifying, record, scripted_pty, write_meta_for,
    };
    use crate::pty::PtyExit;
    use crate::server::accounts_fake::{ADDED, HANG, account, dispatch_local, next_reply};
    use crate::server::test_support::{ScratchDir, test_hub};
    use crate::server::{CommandSession, CommandSpec, Hub};
    use agent_api::{
        AccountRow, AccountsError, AccountsFailureKind, AccountsService, AccountsSnapshot,
        PollReport, PollScope,
    };
    use protocol::SessionMode;
    use protocol::{ClientMessage, error_codes};
    use std::collections::{HashMap, VecDeque};
    use std::sync::{Mutex, PoisonError};
    use tokio::sync::oneshot;

    fn locked<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        m.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// An account service whose login command, capture and cleanup the test
    /// controls.
    #[derive(Default)]
    struct FakeService {
        accounts: Mutex<Vec<AccountRow>>,
        refuse: Mutex<Option<AccountsFailure>>,
        command: Mutex<Option<LoginCommand>>,
        captures: Mutex<Vec<PathBuf>>,
        discards: Mutex<Vec<PathBuf>>,
        capture_gate: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
        capture_started: Mutex<Option<mpsc::UnboundedSender<()>>>,
        capture_fails: Mutex<Option<AccountsFailure>>,
        capture_panics: std::sync::atomic::AtomicBool,
    }

    impl FakeService {
        /// The next capture says it started on the returned receiver, then
        /// waits until the returned sender sends (or is dropped).
        fn hold_capture(&self) -> (std::sync::mpsc::Sender<()>, mpsc::UnboundedReceiver<()>) {
            let (release, gate) = std::sync::mpsc::channel();
            let (started_tx, started) = mpsc::unbounded_channel();
            *locked(&self.capture_gate) = Some(gate);
            *locked(&self.capture_started) = Some(started_tx);
            (release, started)
        }

        fn captures(&self) -> usize {
            locked(&self.captures).len()
        }

        fn discards(&self) -> usize {
            locked(&self.discards).len()
        }
    }

    impl AccountsService for FakeService {
        fn snapshot(&self) -> Result<AccountsSnapshot, AccountsError> {
            Ok(AccountsSnapshot {
                accounts: locked(&self.accounts).clone(),
                auto: None,
            })
        }

        fn poll(&self, _scope: PollScope) -> Result<PollReport, AccountsError> {
            Ok(PollReport {
                snapshot: self.snapshot()?,
                next_poll_in: None,
            })
        }

        fn begin_shutdown(&self, _bound: Duration) -> bool {
            true
        }

        fn login_command(&self, scratch: &Path) -> Result<LoginCommand, AccountsFailure> {
            if let Some(failure) = locked(&self.refuse).clone() {
                return Err(failure);
            }
            Ok(locked(&self.command)
                .clone()
                .unwrap_or_else(|| LoginCommand {
                    args: vec!["login".to_owned()],
                    env: vec![("FAKE_CONFIG_DIR".to_owned(), scratch.display().to_string())],
                    env_remove: vec!["FAKE_LIVE_TOKEN".to_owned()],
                }))
        }

        fn capture_login_from(&self, scratch: &Path) -> Result<LoginCapture, AccountsFailure> {
            locked(&self.captures).push(scratch.to_owned());
            if let Some(started) = locked(&self.capture_started).take() {
                let _ = started.send(());
            }
            let gate = locked(&self.capture_gate).take();
            if let Some(gate) = gate {
                let _ = gate.recv();
            }
            if self
                .capture_panics
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                std::panic::resume_unwind(Box::new("the fake capture panicked"));
            }
            // A failed capture may still have changed the store.
            locked(&self.accounts).push(account(ADDED, false));
            if let Some(failure) = locked(&self.capture_fails).clone() {
                return Err(failure);
            }
            Ok(LoginCapture {
                number: ADDED,
                added: true,
            })
        }

        fn discard_login(&self, scratch: &Path) -> Result<(), AccountsFailure> {
            locked(&self.discards).push(scratch.to_owned());
            Ok(())
        }
    }

    /// What the fake runner does with its next spawn.
    enum Plan {
        /// Spawn; the exit comes when the test (or a stop) sends it.
        Run,
        /// Spawn once the sender is dropped or sends, then as `Run`.
        Hold(oneshot::Receiver<()>),
        /// Spawn a session whose exit is already in.
        ExitAtOnce(ExitFacts),
        /// Spawn a session whose exit is in only once the slot is Ending, and
        /// which wakes no one when it comes in.
        ExitOnceEnding(Arc<Accounts>),
        /// Spawn a session whose exit future panics.
        ExitPanics,
        /// Fail the spawn.
        Fail,
    }

    /// A runner whose spawns, exits, stops and discards the test controls;
    /// each call is recorded and noted on a channel.
    #[derive(Default)]
    struct FakeRunner {
        plans: Mutex<VecDeque<Plan>>,
        exits: Mutex<HashMap<String, oneshot::Sender<LoginExit>>>,
        calls: Mutex<Vec<String>>,
        stop_gate: Mutex<Option<oneshot::Receiver<()>>>,
        notes: Mutex<Option<mpsc::UnboundedSender<String>>>,
    }

    /// A session's exit future over the notice `rx`.
    fn exit_on(rx: oneshot::Receiver<LoginExit>) -> BoxFuture<'static, LoginExit> {
        Box::pin(async move {
            rx.await.unwrap_or(LoginExit {
                facts: None,
                tail: Vec::new(),
            })
        })
    }

    fn boom() -> LoginExit {
        std::panic::resume_unwind(Box::new("the fake login panicked"))
    }

    impl FakeRunner {
        fn plan(&self, plan: Plan) {
            locked(&self.plans).push_back(plan);
        }

        fn note(&self, call: String) {
            locked(&self.calls).push(call.clone());
            if let Some(notes) = &*locked(&self.notes) {
                let _ = notes.send(call);
            }
        }

        fn calls(&self) -> Vec<String> {
            locked(&self.calls).clone()
        }

        fn spawns(&self) -> usize {
            self.calls()
                .iter()
                .filter(|call| call.starts_with("spawn"))
                .count()
        }

        /// Send session `session_id`'s exit.
        fn exit(&self, session_id: &str, facts: ExitFacts, tail: &[&str]) {
            let sender = locked(&self.exits)
                .remove(session_id)
                .expect("the session is waiting for its exit");
            let _ = sender.send(LoginExit {
                facts: Some(facts),
                tail: tail.iter().map(|line| (*line).to_owned()).collect(),
            });
        }

        /// Drop session `session_id`'s exit notice unsent.
        fn drop_notice(&self, session_id: &str) {
            drop(locked(&self.exits).remove(session_id));
        }

        /// The next stop holds until the returned sender sends or drops.
        fn hold_stop(&self) -> oneshot::Sender<()> {
            let (release, gate) = oneshot::channel();
            *locked(&self.stop_gate) = Some(gate);
            release
        }
    }

    impl LoginRunner for FakeRunner {
        fn spawn(
            &self,
            run: &str,
            scratch: &Path,
            _cmd: LoginCommand,
        ) -> BoxFuture<'static, anyhow::Result<LoginSession>> {
            self.note(format!("spawn {run} dir={}", scratch.is_dir()));
            let plan = locked(&self.plans).pop_front().unwrap_or(Plan::Run);
            let session_id = run.to_owned();
            let ready = |exit: LoginExit| -> BoxFuture<'static, LoginExit> {
                Box::pin(async move { exit })
            };
            let session = |session_id: String, exit| {
                Box::pin(async move { Ok(LoginSession { session_id, exit }) })
                    as BoxFuture<'static, anyhow::Result<LoginSession>>
            };
            match plan {
                Plan::Fail => Box::pin(async { Err(anyhow::anyhow!("program not found")) }),
                Plan::ExitAtOnce(facts) => session(
                    session_id,
                    ready(LoginExit {
                        facts: Some(facts),
                        tail: vec!["screen line".to_owned()],
                    }),
                ),
                Plan::ExitPanics => session(session_id, Box::pin(async { boom() })),
                Plan::ExitOnceEnding(accounts) => session(
                    session_id,
                    Box::pin(futures::future::poll_fn(move |_| {
                        if matches!(
                            *crate::sync::lock(&accounts.login),
                            LoginSlot::Ending { .. }
                        ) {
                            std::task::Poll::Ready(LoginExit {
                                facts: Some(ExitFacts::Exited { code: 0 }),
                                tail: Vec::new(),
                            })
                        } else {
                            std::task::Poll::Pending
                        }
                    })),
                ),
                Plan::Run => {
                    let (tx, rx) = oneshot::channel();
                    locked(&self.exits).insert(session_id.clone(), tx);
                    session(session_id, exit_on(rx))
                }
                Plan::Hold(gate) => {
                    let (tx, rx) = oneshot::channel();
                    locked(&self.exits).insert(session_id.clone(), tx);
                    Box::pin(async move {
                        let _ = gate.await;
                        Ok(LoginSession {
                            session_id,
                            exit: exit_on(rx),
                        })
                    })
                }
            }
        }

        fn stop(&self, session_id: &str) -> BoxFuture<'static, ()> {
            self.note(format!("stop {session_id}"));
            let gate = locked(&self.stop_gate).take();
            let sender = locked(&self.exits).remove(session_id);
            Box::pin(async move {
                if let Some(gate) = gate {
                    let _ = gate.await;
                }
                if let Some(sender) = sender {
                    let _ = sender.send(LoginExit {
                        facts: Some(ExitFacts::StoppedFirst),
                        tail: vec!["stopped screen".to_owned()],
                    });
                }
            })
        }

        fn discard(&self, session_id: &str) -> BoxFuture<'static, ()> {
            self.note(format!("discard {session_id}"));
            Box::pin(async {})
        }
    }

    /// A hub whose accounts run logins through the fakes, in a scratch login
    /// root.
    struct Rig {
        hub: Hub,
        _scratch: ScratchDir,
        accounts: Arc<Accounts>,
        runner: Arc<FakeRunner>,
        service: Arc<FakeService>,
        events: broadcast::Receiver<StateEvent>,
        notes: mpsc::UnboundedReceiver<String>,
        root: PathBuf,
    }

    fn rig(tag: &str) -> Rig {
        rig_with(tag, None)
    }

    /// [`rig`], whose logins run through the daemon's [`SessionRunner`] with
    /// `sessions` playing the spawned terminals, when given.
    fn rig_with(tag: &str, sessions: Option<&Arc<FakeSessions>>) -> Rig {
        let (mut hub, scratch) = test_hub(tag);
        let root = scratch.path().join("account-login");
        let service = Arc::new(FakeService::default());
        let runner = Arc::new(FakeRunner::default());
        let (notes_tx, notes) = mpsc::unbounded_channel();
        *locked(&runner.notes) = Some(notes_tx);
        let hub_slot = Arc::new(OnceLock::new());
        let used: Arc<dyn LoginRunner> = match sessions {
            Some(sessions) => Arc::new(
                SessionRunner::new(agent_registry::default_agent(), Arc::clone(&hub_slot))
                    .spawning_with(Arc::clone(sessions) as Arc<dyn TestSpawner>),
            ),
            None => Arc::clone(&runner) as Arc<dyn LoginRunner>,
        };
        let accounts = Accounts::with_login(
            Arc::clone(&service) as SharedAccounts,
            LoginSetup {
                root: root.clone(),
                agent: agent_registry::default_agent(),
                runner: used,
            },
        );
        hub.accounts = Some(Arc::clone(&accounts));
        let _ = hub_slot.set(hub.clone());
        let events = hub.state_events.subscribe();
        Rig {
            hub,
            _scratch: scratch,
            accounts,
            runner,
            service,
            events,
            notes,
            root,
        }
    }

    impl Rig {
        /// Dispatch `msg` on a local connection; its replies come on the
        /// returned receiver.
        async fn send(&self, msg: ClientMessage) -> mpsc::UnboundedReceiver<DaemonMessage> {
            let (out_tx, rx) = mpsc::unbounded_channel();
            dispatch_local(&self.hub, msg, &out_tx).await;
            rx
        }

        async fn start(&self, id: &str) -> mpsc::UnboundedReceiver<DaemonMessage> {
            self.send(ClientMessage::StartAccountLogin {
                request_id: Some(id.to_owned()),
            })
            .await
        }

        async fn cancel(&self, id: &str) -> mpsc::UnboundedReceiver<DaemonMessage> {
            self.send(ClientMessage::CancelAccountLogin {
                request_id: Some(id.to_owned()),
            })
            .await
        }

        /// Start a login and wait for its direct `Started`; its session id.
        async fn start_running(&self, id: &str) -> String {
            let mut rx = self.start(id).await;
            started(next_reply(&mut rx).await, id)
        }

        /// The next `AccountLogin` broadcast, skipping every other event.
        async fn login_event(&mut self) -> (AccountLoginState, Option<String>, Option<String>) {
            loop {
                let event = tokio::time::timeout(HANG, self.events.recv())
                    .await
                    .expect("hung: no login event came");
                match event {
                    Ok(StateEvent::AccountLogin {
                        state,
                        session_id,
                        message,
                    }) => return (state, session_id, message),
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => {
                        unreachable!("the hub holds the events' sender")
                    }
                }
            }
        }

        /// Assert no `AccountLogin` broadcast is waiting.
        fn no_login_event(&mut self) {
            loop {
                match self.events.try_recv() {
                    Ok(StateEvent::AccountLogin {
                        state,
                        session_id,
                        message,
                    }) => {
                        unreachable!(
                            "a login event was broadcast: {state:?} {session_id:?} {message:?}"
                        )
                    }
                    Ok(_) | Err(broadcast::error::TryRecvError::Lagged(_)) => {}
                    Err(_) => return,
                }
            }
        }

        /// The next runner call.
        async fn note(&mut self) -> String {
            tokio::time::timeout(HANG, self.notes.recv())
                .await
                .expect("hung: the runner was not called")
                .expect("the runner's notes are open")
        }

        /// The scratch folders under the login root.
        fn dirs(&self) -> Vec<String> {
            let Ok(entries) = std::fs::read_dir(&self.root) else {
                return Vec::new();
            };
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        }

        /// The slot's state, named.
        fn slot(&self) -> &'static str {
            match &*crate::sync::lock(&self.accounts.login) {
                LoginSlot::Idle => "idle",
                LoginSlot::Starting { .. } => "starting",
                LoginSlot::Running { .. } => "running",
                LoginSlot::Capturing { .. } => "capturing",
                LoginSlot::Ending { .. } => "ending",
            }
        }

        fn shut_down(&self) {
            self.hub.shutdown_tx.send_replace(true);
        }
    }

    /// The session id of a direct `Started` that echoes `id`.
    fn started(reply: DaemonMessage, id: &str) -> String {
        match reply {
            DaemonMessage::AccountLogin {
                state: AccountLoginState::Started,
                session_id: Some(session_id),
                message: None,
                request_id: Some(echoed),
            } if echoed == id => session_id,
            other => unreachable!("not the Started reply to {id}: {other:?}"),
        }
    }

    fn assert_error(reply: &DaemonMessage, code: &str, message: &str, id: &str) {
        assert!(
            matches!(
                reply,
                DaemonMessage::Error { code: Some(got), message: said, request_id: Some(echoed) }
                    if got == code && said == message && echoed == id
            ),
            "not the {code} reply to {id}: {reply:?}"
        );
    }

    async fn yield_a_few() {
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn login_exit_zero_captures_and_succeeds() {
        let mut rig = rig("login-exit-zero");
        let (_clients_tx, clients) = watch::channel(0_usize);
        rig.accounts.start_driver(
            rig.hub.state_events.clone(),
            crate::accounts::driver::Watches {
                clients,
                shutdown: rig.hub.shutdown_tx.subscribe(),
            },
        );

        let session = rig.start_running("l1").await;
        assert_eq!(rig.note().await, format!("spawn {session} dir=true"));
        assert_eq!(rig.dirs(), [session.as_str()]);
        rig.runner
            .exit(&session, ExitFacts::Exited { code: 0 }, &[]);

        let mut added_seen = false;
        let terminal = loop {
            let event = tokio::time::timeout(HANG, rig.events.recv())
                .await
                .expect("hung: the run did not end")
                .expect("the events are open");
            match event {
                StateEvent::Accounts { accounts, .. } => {
                    added_seen |= accounts.iter().any(|row| row.number == ADDED);
                }
                StateEvent::AccountLogin {
                    state: AccountLoginState::Started,
                    ..
                } => {}
                StateEvent::AccountLogin {
                    state,
                    session_id,
                    message,
                } => break (state, session_id, message),
                _ => {}
            }
        };
        assert!(added_seen, "AccountsUpdated with the new row came first");
        assert_eq!(
            terminal,
            (AccountLoginState::Succeeded, Some(session.clone()), None)
        );
        assert!(rig.dirs().is_empty(), "{:?}", rig.dirs());
        assert_eq!(rig.slot(), "idle");
        assert_eq!(rig.service.captures(), 1);
        assert_eq!(rig.service.discards(), 0, "a capture needs no cleanup");
        assert_eq!(rig.runner.calls(), [format!("spawn {session} dir=true")]);
        rig.shut_down();
        rig.accounts.join_driver(HANG).await;
    }

    #[tokio::test]
    async fn login_exit_before_started_still_sends_started_first() {
        let mut rig = rig("login-exit-before-started");
        rig.runner
            .plan(Plan::ExitAtOnce(ExitFacts::Exited { code: 3 }));

        let mut rx = rig.start("l1").await;

        let session = started(next_reply(&mut rx).await, "l1");
        assert_eq!(
            rig.login_event().await,
            (AccountLoginState::Started, Some(session.clone()), None)
        );
        assert_eq!(
            rig.login_event().await,
            (
                AccountLoginState::Failed,
                Some(session),
                Some("the login exited with code 3\nscreen line".to_owned())
            )
        );
        assert!(rig.dirs().is_empty(), "{:?}", rig.dirs());
        assert_eq!(rig.slot(), "idle");
    }

    #[tokio::test]
    async fn login_nonzero_exit_fails() {
        let mut rig = rig("login-nonzero-exit");
        let session = rig.start_running("l1").await;
        let _ = rig.login_event().await;

        rig.runner.exit(
            &session,
            ExitFacts::Exited { code: 3 },
            &["one", "  ", "two"],
        );

        assert_eq!(
            rig.login_event().await,
            (
                AccountLoginState::Failed,
                Some(session.clone()),
                Some("the login exited with code 3\none\ntwo".to_owned())
            )
        );
        assert!(rig.dirs().is_empty(), "{:?}", rig.dirs());
        assert_eq!(rig.slot(), "idle");
        assert_eq!(rig.service.captures(), 0);
        assert_eq!(rig.service.discards(), 1, "the scratch login is cleaned up");
        assert_eq!(
            rig.runner.calls(),
            [format!("spawn {session} dir=true")],
            "a self-exit is neither stopped nor discarded"
        );
    }

    /// A cancel whose stop's exit comes in while a process the session left
    /// (a wrapped CLI's grandchild, whose cwd is the scratch folder until the
    /// tracer's job takes it) still holds the folder: the run removes the folder
    /// once that process lets go, before it answers the cancel.
    #[cfg(windows)]
    #[tokio::test]
    async fn login_cancel_removes_a_folder_a_lingering_grandchild_held() {
        use std::os::windows::fs::OpenOptionsExt as _;
        /// How long the grandchild outlives the forwarded exit; the tracer's
        /// grace alone is 200 ms.
        const LINGER: Duration = Duration::from_secs(1);
        let mut rig = rig("login-cancel-held-folder");
        let session = rig.start_running("l1").await;
        let _ = rig.login_event().await;
        // A process's cwd handle: shares read and write, never delete.
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0x1 | 0x2) // FILE_SHARE_READ | FILE_SHARE_WRITE
            .custom_flags(0x0200_0000) // FILE_FLAG_BACKUP_SEMANTICS: a directory
            .open(rig.root.join(&session))
            .expect("the scratch folder opens as a cwd holds it");
        let mut held = Some(held);

        let mut canceller = rig.cancel("c1").await;
        let reply = {
            let reply = next_reply(&mut canceller);
            tokio::pin!(reply);
            tokio::select! {
                reply = &mut reply => reply,
                () = tokio::time::sleep(LINGER) => {
                    held = None;
                    reply.await
                }
            }
        };
        drop(held);

        assert!(
            matches!(
                &reply,
                DaemonMessage::AccountLogin {
                    state: AccountLoginState::Cancelled,
                    session_id: Some(id),
                    request_id: Some(echoed),
                    ..
                } if *id == session && echoed == "c1"
            ),
            "{reply:?}"
        );
        assert!(
            rig.dirs().is_empty(),
            "the folder is left: {:?}",
            rig.dirs()
        );
        assert_eq!(rig.slot(), "idle");
    }

    #[tokio::test]
    async fn login_cancel_stops_session_then_cancels() {
        let mut rig = rig("login-cancel-running");
        let session = rig.start_running("l1").await;
        let _ = rig.login_event().await;

        let mut canceller = rig.cancel("c1").await;

        let reply = next_reply(&mut canceller).await;
        assert!(
            matches!(
                &reply,
                DaemonMessage::AccountLogin {
                    state: AccountLoginState::Cancelled,
                    session_id: Some(id),
                    message: None,
                    request_id: Some(echoed),
                } if *id == session && echoed == "c1"
            ),
            "{reply:?}"
        );
        assert_eq!(
            rig.login_event().await,
            (AccountLoginState::Cancelled, Some(session.clone()), None)
        );
        assert_eq!(
            rig.runner.calls(),
            [
                format!("spawn {session} dir=true"),
                format!("stop {session}"),
                format!("discard {session}"),
            ]
        );
        assert!(rig.dirs().is_empty(), "{:?}", rig.dirs());
        assert_eq!(rig.slot(), "idle");
        assert_eq!(rig.service.captures(), 0);
        assert_eq!(rig.service.discards(), 1);
    }

    #[tokio::test]
    async fn login_cancel_before_capture_never_captures() {
        let mut rig = rig("login-cancel-before-capture");
        let session = rig.start_running("l1").await;
        let _ = rig.login_event().await;

        let mut canceller = rig.cancel("c1").await;
        rig.runner
            .exit(&session, ExitFacts::Exited { code: 0 }, &[]);

        let reply = next_reply(&mut canceller).await;
        assert!(
            matches!(
                &reply,
                DaemonMessage::AccountLogin {
                    state: AccountLoginState::Cancelled,
                    request_id: Some(echoed),
                    ..
                } if echoed == "c1"
            ),
            "{reply:?}"
        );
        assert_eq!(
            rig.login_event().await,
            (AccountLoginState::Cancelled, Some(session), None)
        );
        assert_eq!(rig.service.captures(), 0, "a cancel beat the capture");
        assert!(rig.dirs().is_empty(), "{:?}", rig.dirs());
        assert_eq!(rig.slot(), "idle");
    }

    #[tokio::test]
    async fn login_cancel_during_capture_answers_busy() {
        let mut rig = rig("login-cancel-capturing");
        let (release, mut capture_started) = rig.service.hold_capture();
        let session = rig.start_running("l1").await;
        let _ = rig.login_event().await;
        rig.runner
            .exit(&session, ExitFacts::Exited { code: 0 }, &[]);
        tokio::time::timeout(HANG, capture_started.recv())
            .await
            .expect("hung: the capture did not start");

        let mut canceller = rig.cancel("c1").await;

        assert_error(
            &next_reply(&mut canceller).await,
            error_codes::ACCOUNT_BUSY,
            "the login is finishing",
            "c1",
        );
        assert_eq!(rig.slot(), "capturing");
        release.send(()).expect("the capture is waiting");
        assert_eq!(
            rig.login_event().await,
            (AccountLoginState::Succeeded, Some(session), None)
        );
        assert!(rig.dirs().is_empty(), "{:?}", rig.dirs());
        assert_eq!(rig.slot(), "idle");
    }

    #[tokio::test]
    async fn login_cancel_outside_running_is_refused() {
        let mut rig = rig("login-cancel-refused");
        let mut idle = rig.cancel("c0").await;
        assert_error(
            &next_reply(&mut idle).await,
            error_codes::ACCOUNT_REFUSED,
            "no account login is running",
            "c0",
        );
        assert_eq!(rig.slot(), "idle");

        let (spawn_release, gate) = oneshot::channel();
        rig.runner.plan(Plan::Hold(gate));
        let mut requester = rig.start("l1").await;
        let _ = rig.note().await;
        let mut starting = rig.cancel("c1").await;
        assert_error(
            &next_reply(&mut starting).await,
            error_codes::ACCOUNT_BUSY,
            "the login is starting",
            "c1",
        );
        assert_eq!(rig.slot(), "starting");

        spawn_release.send(()).expect("the spawn is waiting");
        let session = started(next_reply(&mut requester).await, "l1");
        let stop_release = rig.runner.hold_stop();
        let mut canceller = rig.cancel("c2").await;
        assert_eq!(rig.note().await, format!("stop {session}"));
        let mut ending = rig.cancel("c3").await;
        assert_error(
            &next_reply(&mut ending).await,
            error_codes::ACCOUNT_BUSY,
            "the login is ending",
            "c3",
        );
        assert_eq!(rig.slot(), "ending");

        stop_release.send(()).expect("the stop is waiting");
        let reply = next_reply(&mut canceller).await;
        assert!(
            matches!(
                &reply,
                DaemonMessage::AccountLogin {
                    state: AccountLoginState::Cancelled,
                    request_id: Some(echoed),
                    ..
                } if echoed == "c2"
            ),
            "{reply:?}"
        );
        assert_eq!(rig.slot(), "idle");
    }

    #[tokio::test(start_paused = true)]
    async fn login_timeout_stops_session_and_fails() {
        let mut rig = rig("login-timeout");
        let session = rig.start_running("l1").await;
        let _ = rig.login_event().await;

        tokio::time::advance(LOGIN_TIMEOUT).await;

        assert_eq!(
            rig.login_event().await,
            (
                AccountLoginState::Failed,
                Some(session.clone()),
                Some("the login timed out\nstopped screen".to_owned())
            )
        );
        assert_eq!(
            rig.runner.calls(),
            [
                format!("spawn {session} dir=true"),
                format!("stop {session}"),
                format!("discard {session}"),
            ]
        );
        assert!(rig.dirs().is_empty(), "{:?}", rig.dirs());
        assert_eq!(rig.slot(), "idle");
    }

    #[tokio::test(start_paused = true)]
    async fn login_exit_at_deadline_beats_timeout() {
        let mut rig = rig("login-exit-at-deadline");
        let session = rig.start_running("l1").await;
        let _ = rig.login_event().await;

        rig.runner
            .exit(&session, ExitFacts::Exited { code: 0 }, &[]);
        tokio::time::advance(LOGIN_TIMEOUT).await;

        assert_eq!(
            rig.login_event().await,
            (AccountLoginState::Succeeded, Some(session.clone()), None)
        );
        assert_eq!(rig.service.captures(), 1);
        assert_eq!(rig.runner.calls(), [format!("spawn {session} dir=true")]);
    }

    #[tokio::test(start_paused = true)]
    async fn login_old_deadline_never_ends_new_run() {
        let mut rig = rig("login-old-deadline");
        let first = rig.start_running("l1").await;
        let _ = rig.login_event().await;
        tokio::time::advance(LOGIN_TIMEOUT / 2).await;
        rig.runner.exit(&first, ExitFacts::Exited { code: 3 }, &[]);
        let (state, _, _) = rig.login_event().await;
        assert_eq!(state, AccountLoginState::Failed);

        let second = rig.start_running("l2").await;
        let _ = rig.login_event().await;
        tokio::time::advance(LOGIN_TIMEOUT / 2 + Duration::from_secs(1)).await;
        yield_a_few().await;

        assert_eq!(rig.slot(), "running");
        rig.no_login_event();
        assert_eq!(rig.dirs(), [second.as_str()]);
        assert!(
            !rig.runner
                .calls()
                .iter()
                .any(|call| call.starts_with("stop")),
            "{:?}",
            rig.runner.calls()
        );
    }

    #[tokio::test]
    async fn login_watch_shutdown_stops_session_and_joins() {
        let mut rig = rig("login-watch-shutdown");
        let session = rig.start_running("l1").await;
        let _ = rig.login_event().await;

        rig.shut_down();
        rig.accounts.join_driver(HANG).await;

        assert!(
            crate::sync::lock(&rig.accounts.login_task).is_none(),
            "the run was joined"
        );
        assert_eq!(rig.slot(), "idle");
        assert_eq!(
            rig.login_event().await,
            (
                AccountLoginState::Cancelled,
                Some(session.clone()),
                Some("the daemon is shutting down".to_owned())
            )
        );
        assert_eq!(
            rig.runner.calls(),
            [
                format!("spawn {session} dir=true"),
                format!("stop {session}"),
                format!("discard {session}"),
            ]
        );
        assert!(rig.dirs().is_empty(), "{:?}", rig.dirs());
    }

    #[tokio::test]
    async fn login_shutdown_during_capture_waits_for_capture() {
        let mut rig = rig("login-shutdown-capturing");
        let (release, mut capture_started) = rig.service.hold_capture();
        let session = rig.start_running("l1").await;
        let _ = rig.login_event().await;
        rig.runner
            .exit(&session, ExitFacts::Exited { code: 0 }, &[]);
        tokio::time::timeout(HANG, capture_started.recv())
            .await
            .expect("hung: the capture did not start");

        rig.shut_down();
        let accounts = Arc::clone(&rig.accounts);
        let join = tokio::spawn(async move { accounts.join_driver(HANG).await });
        yield_a_few().await;
        assert_eq!(rig.slot(), "capturing", "a shutdown changes nothing");
        release.send(()).expect("the capture is waiting");
        join.await.expect("the join did not panic");

        assert_eq!(
            rig.login_event().await,
            (AccountLoginState::Succeeded, Some(session), None)
        );
        assert_eq!(rig.service.captures(), 1);
        assert!(rig.dirs().is_empty(), "{:?}", rig.dirs());
        assert_eq!(rig.slot(), "idle");
    }

    #[tokio::test]
    async fn login_shutdown_while_starting_stops_the_new_session() {
        let mut rig = rig("login-shutdown-starting");
        let (spawn_release, gate) = oneshot::channel();
        rig.runner.plan(Plan::Hold(gate));
        let mut requester = rig.start("l1").await;
        let _ = rig.note().await;

        rig.shut_down();
        spawn_release.send(()).expect("the spawn is waiting");

        let session = started(next_reply(&mut requester).await, "l1");
        assert_eq!(
            rig.login_event().await,
            (AccountLoginState::Started, Some(session.clone()), None)
        );
        assert_eq!(
            rig.login_event().await,
            (
                AccountLoginState::Cancelled,
                Some(session.clone()),
                Some("the daemon is shutting down".to_owned())
            )
        );
        assert_eq!(
            rig.runner.calls(),
            [
                format!("spawn {session} dir=true"),
                format!("stop {session}"),
                format!("discard {session}"),
            ]
        );
        assert!(rig.dirs().is_empty(), "{:?}", rig.dirs());
        assert_eq!(rig.slot(), "idle");
    }

    #[tokio::test]
    async fn login_start_during_shutdown_answers_draining() {
        let mut rig = rig("login-start-draining");
        rig.shut_down();

        let mut rx = rig.start("l1").await;

        assert_error(
            &next_reply(&mut rx).await,
            error_codes::ACCOUNT_DRAINING,
            "the daemon is shutting down",
            "l1",
        );
        assert!(rig.runner.calls().is_empty(), "{:?}", rig.runner.calls());
        assert!(rig.dirs().is_empty(), "{:?}", rig.dirs());
        assert_eq!(rig.slot(), "idle");
        rig.no_login_event();
    }

    #[tokio::test]
    async fn login_dropped_exit_notice_leaves_scratch() {
        let mut rig = rig("login-dropped-notice");
        let session = rig.start_running("l1").await;
        let _ = rig.login_event().await;

        rig.runner.drop_notice(&session);

        assert_eq!(
            rig.login_event().await,
            (
                AccountLoginState::Failed,
                Some(session.clone()),
                Some("lost the login terminal".to_owned())
            )
        );
        assert_eq!(rig.dirs(), [session.as_str()], "left for the sweep");
        assert_eq!(rig.service.discards(), 0);
        assert_eq!(rig.runner.calls(), [format!("spawn {session} dir=true")]);
        assert_eq!(rig.slot(), "idle");
    }

    #[tokio::test]
    async fn login_second_start_is_busy_in_every_state() {
        let mut rig = rig("login-second-start");
        let busy = "an account login is already running";

        let (spawn_release, gate) = oneshot::channel();
        rig.runner.plan(Plan::Hold(gate));
        let mut first = rig.start("l1").await;
        let _ = rig.note().await;
        let mut starting = rig.start("l2").await;
        assert_error(
            &next_reply(&mut starting).await,
            error_codes::ACCOUNT_BUSY,
            busy,
            "l2",
        );
        assert_eq!(rig.dirs().len(), 1, "{:?}", rig.dirs());

        spawn_release.send(()).expect("the spawn is waiting");
        let session = started(next_reply(&mut first).await, "l1");
        let mut running = rig.start("l3").await;
        assert_error(
            &next_reply(&mut running).await,
            error_codes::ACCOUNT_BUSY,
            busy,
            "l3",
        );

        let stop_release = rig.runner.hold_stop();
        let _canceller = rig.cancel("c1").await;
        assert_eq!(rig.note().await, format!("stop {session}"));
        let mut ending = rig.start("l4").await;
        assert_error(
            &next_reply(&mut ending).await,
            error_codes::ACCOUNT_BUSY,
            busy,
            "l4",
        );
        assert_eq!(rig.dirs(), [session.as_str()]);
        stop_release.send(()).expect("the stop is waiting");
        loop {
            let (state, _, _) = rig.login_event().await;
            if state == AccountLoginState::Cancelled {
                break;
            }
        }

        let (capture_release, mut capture_started) = rig.service.hold_capture();
        let second = rig.start_running("l5").await;
        rig.runner.exit(&second, ExitFacts::Exited { code: 0 }, &[]);
        tokio::time::timeout(HANG, capture_started.recv())
            .await
            .expect("hung: the capture did not start");
        let mut capturing = rig.start("l6").await;
        assert_error(
            &next_reply(&mut capturing).await,
            error_codes::ACCOUNT_BUSY,
            busy,
            "l6",
        );
        assert_eq!(rig.dirs(), [second]);
        capture_release.send(()).expect("the capture is waiting");

        assert_eq!(rig.runner.spawns(), 2, "{:?}", rig.runner.calls());
    }

    #[tokio::test]
    async fn login_start_after_terminal_is_accepted() {
        let mut rig = rig("login-start-after-terminal");
        let first = rig.start_running("l1").await;
        let _ = rig.login_event().await;
        rig.runner.exit(&first, ExitFacts::Exited { code: 3 }, &[]);
        let (state, _, _) = rig.login_event().await;
        assert_eq!(state, AccountLoginState::Failed);

        let second = rig.start_running("l2").await;

        assert_ne!(first, second, "a new run id");
        assert_eq!(rig.dirs(), [second]);
        assert_eq!(rig.slot(), "running");
    }

    #[tokio::test]
    async fn login_spawn_failure_answers_error_and_removes_scratch() {
        let mut rig = rig("login-spawn-failure");
        rig.runner.plan(Plan::Fail);

        let mut rx = rig.start("l1").await;

        assert_error(
            &next_reply(&mut rx).await,
            error_codes::ACCOUNT_FAILED,
            "program not found",
            "l1",
        );
        let spawn = rig.note().await;
        assert!(spawn.ends_with("dir=true"), "{spawn}");
        assert!(rig.dirs().is_empty(), "{:?}", rig.dirs());
        assert_eq!(rig.slot(), "idle");
        rig.no_login_event();
    }

    #[tokio::test]
    async fn login_command_refusal_answers_its_code() {
        let mut rig = rig("login-command-refused");
        *locked(&rig.service.refuse) = Some(AccountsFailure {
            kind: AccountsFailureKind::Refused,
            code: "live_home".to_owned(),
            message: "the scratch folder is the live config home".to_owned(),
        });

        let mut rx = rig.start("l1").await;

        assert_error(
            &next_reply(&mut rx).await,
            error_codes::ACCOUNT_REFUSED,
            "the scratch folder is the live config home",
            "l1",
        );
        assert!(rig.runner.calls().is_empty(), "{:?}", rig.runner.calls());
        assert!(rig.dirs().is_empty(), "{:?}", rig.dirs());
        assert_eq!(rig.slot(), "idle");
        rig.no_login_event();
    }

    #[tokio::test]
    async fn login_command_sharing_an_env_name_is_refused() {
        let mut rig = rig("login-command-shared-env");
        *locked(&rig.service.command) = Some(LoginCommand {
            args: Vec::new(),
            env: vec![("FAKE_HOME".to_owned(), "x".to_owned())],
            env_remove: vec!["FAKE_HOME".to_owned()],
        });

        let mut rx = rig.start("l1").await;

        assert_error(
            &next_reply(&mut rx).await,
            error_codes::ACCOUNT_FAILED,
            "the login command both sets and removes FAKE_HOME",
            "l1",
        );
        assert!(rig.runner.calls().is_empty(), "{:?}", rig.runner.calls());
        assert!(rig.dirs().is_empty(), "{:?}", rig.dirs());
        assert_eq!(rig.slot(), "idle");
        rig.no_login_event();
    }

    #[tokio::test]
    async fn account_login_without_service_answers_unsupported() {
        let (mut hub, _scratch) = test_hub("login-no-service");
        let messages = |n: usize| {
            [
                ClientMessage::StartAccountLogin {
                    request_id: Some(format!("s{n}")),
                },
                ClientMessage::CancelAccountLogin {
                    request_id: Some(format!("c{n}")),
                },
            ]
        };
        for msg in messages(0) {
            let id = match &msg {
                ClientMessage::StartAccountLogin { request_id }
                | ClientMessage::CancelAccountLogin { request_id } => request_id.clone(),
                _ => None,
            }
            .expect("each message carries an id");
            let (out_tx, mut rx) = mpsc::unbounded_channel();
            dispatch_local(&hub, msg, &out_tx).await;
            assert_error(
                &next_reply(&mut rx).await,
                error_codes::ACCOUNT_UNSUPPORTED,
                "this host keeps no accounts",
                &id,
            );
        }

        hub.accounts = Some(Accounts::new(
            Arc::new(FakeService::default()) as SharedAccounts
        ));
        for msg in messages(1) {
            let id = match &msg {
                ClientMessage::StartAccountLogin { request_id }
                | ClientMessage::CancelAccountLogin { request_id } => request_id.clone(),
                _ => None,
            }
            .expect("each message carries an id");
            let (out_tx, mut rx) = mpsc::unbounded_channel();
            dispatch_local(&hub, msg, &out_tx).await;
            assert_error(
                &next_reply(&mut rx).await,
                error_codes::ACCOUNT_UNSUPPORTED,
                "this host cannot run an account login",
                &id,
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn login_start_racing_shutdown_is_joined_or_refused() {
        let runtime = tokio::runtime::Handle::current();
        for round in 0..200 {
            let rig = rig(&format!("login-start-race-{round}"));
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let (out_tx, mut out_rx) = mpsc::unbounded_channel();
            let starter = {
                let (barrier, accounts, runtime) = (
                    Arc::clone(&barrier),
                    Arc::clone(&rig.accounts),
                    runtime.clone(),
                );
                let events = rig.hub.state_events.clone();
                let shutdown = rig.hub.shutdown_tx.subscribe();
                std::thread::spawn(move || {
                    let _entered = runtime.enter();
                    barrier.wait();
                    start(
                        &accounts,
                        events,
                        shutdown,
                        Reply::new(out_tx, Some("l1".to_owned())),
                    );
                })
            };
            let stopper = {
                let (barrier, accounts, runtime) = (
                    Arc::clone(&barrier),
                    Arc::clone(&rig.accounts),
                    runtime.clone(),
                );
                let shutdown_tx = rig.hub.shutdown_tx.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    shutdown_tx.send_replace(true);
                    runtime.block_on(accounts.join_driver(HANG));
                })
            };
            tokio::task::spawn_blocking(move || {
                starter.join().expect("the start did not panic");
                stopper.join().expect("the join did not panic");
            })
            .await
            .expect("the threads were joined");

            assert!(
                crate::sync::lock(&rig.accounts.login_task).is_none(),
                "round {round}: a start's task outlived the join"
            );
            let reply = next_reply(&mut out_rx).await;
            assert!(
                matches!(
                    &reply,
                    DaemonMessage::Error { code: Some(code), .. }
                        if code == error_codes::ACCOUNT_DRAINING
                ) || matches!(
                    &reply,
                    DaemonMessage::AccountLogin {
                        state: AccountLoginState::Started,
                        ..
                    }
                ),
                "round {round}: {reply:?}"
            );
        }
    }

    #[tokio::test]
    async fn login_claim_after_exit_does_not_stop_an_exited_session() {
        let mut rig = rig("login-claim-after-exit");
        rig.runner
            .plan(Plan::ExitOnceEnding(Arc::clone(&rig.accounts)));
        let session = rig.start_running("l1").await;
        let _ = rig.login_event().await;

        rig.shut_down();
        rig.accounts.join_driver(HANG).await;

        assert_eq!(
            rig.login_event().await,
            (
                AccountLoginState::Cancelled,
                Some(session.clone()),
                Some("the daemon is shutting down".to_owned())
            )
        );
        assert_eq!(
            rig.runner.calls(),
            [
                format!("spawn {session} dir=true"),
                format!("discard {session}"),
            ],
            "an exited session is not stopped"
        );
        assert!(rig.dirs().is_empty(), "{:?}", rig.dirs());
        assert_eq!(rig.slot(), "idle");
    }

    #[tokio::test(start_paused = true)]
    async fn login_hung_stop_still_frees_the_slot() {
        let mut rig = rig("login-hung-stop");
        let session = rig.start_running("l1").await;
        let _ = rig.login_event().await;
        assert_eq!(rig.note().await, format!("spawn {session} dir=true"));
        let _stop_held = rig.runner.hold_stop();

        let mut canceller = rig.cancel("c1").await;
        assert_eq!(rig.note().await, format!("stop {session}"));
        tokio::time::advance(STOP_WAIT).await;

        let reply = next_reply(&mut canceller).await;
        assert!(
            matches!(
                &reply,
                DaemonMessage::AccountLogin {
                    state: AccountLoginState::Cancelled,
                    request_id: Some(echoed),
                    ..
                } if echoed == "c1"
            ),
            "{reply:?}"
        );
        assert_eq!(
            rig.login_event().await,
            (AccountLoginState::Cancelled, Some(session.clone()), None)
        );
        assert_eq!(rig.slot(), "idle");
        assert_eq!(rig.dirs(), [session.as_str()], "left for the sweep");
        assert_eq!(rig.service.discards(), 0);
        assert!(
            !rig.runner
                .calls()
                .iter()
                .any(|call| call.starts_with("discard")),
            "the record is left for the sweep: {:?}",
            rig.runner.calls()
        );
    }

    /// Start the accounts driver over `rig`'s hub.
    fn start_driver(rig: &Rig) -> watch::Sender<usize> {
        let (clients_tx, clients) = watch::channel(0_usize);
        rig.accounts.start_driver(
            rig.hub.state_events.clone(),
            crate::accounts::driver::Watches {
                clients,
                shutdown: rig.hub.shutdown_tx.subscribe(),
            },
        );
        clients_tx
    }

    #[tokio::test]
    async fn login_failed_capture_still_refreshes() {
        let mut rig = rig("login-failed-capture");
        *locked(&rig.service.capture_fails) = Some(AccountsFailure {
            kind: AccountsFailureKind::Failed,
            code: "no_login".to_owned(),
            message: "the scratch folder holds no login".to_owned(),
        });
        let _clients = start_driver(&rig);
        let session = rig.start_running("l1").await;
        rig.runner
            .exit(&session, ExitFacts::Exited { code: 0 }, &[]);

        let mut added_seen = false;
        let terminal = loop {
            let event = tokio::time::timeout(HANG, rig.events.recv())
                .await
                .expect("hung: the run did not end")
                .expect("the events are open");
            match event {
                StateEvent::Accounts { accounts, .. } => {
                    added_seen |= accounts.iter().any(|row| row.number == ADDED);
                }
                StateEvent::AccountLogin {
                    state: AccountLoginState::Started,
                    ..
                } => {}
                StateEvent::AccountLogin {
                    state,
                    session_id,
                    message,
                } => break (state, session_id, message),
                _ => {}
            }
        };
        assert!(
            added_seen,
            "the refresh after the failed capture came first"
        );
        assert_eq!(
            terminal,
            (
                AccountLoginState::Failed,
                Some(session),
                Some("the scratch folder holds no login".to_owned())
            )
        );
        assert_eq!(
            rig.service.discards(),
            0,
            "a failed capture cleaned up itself"
        );
        assert!(rig.dirs().is_empty(), "{:?}", rig.dirs());
        rig.shut_down();
        rig.accounts.join_driver(HANG).await;
    }

    #[tokio::test]
    async fn login_capture_panic_discards_the_scratch_login() {
        let mut rig = rig("login-capture-panic");
        rig.service
            .capture_panics
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let session = rig.start_running("l1").await;
        let _ = rig.login_event().await;

        rig.runner
            .exit(&session, ExitFacts::Exited { code: 0 }, &[]);

        assert_eq!(
            rig.login_event().await,
            (
                AccountLoginState::Failed,
                Some(session),
                Some("the login capture stopped".to_owned())
            )
        );
        assert_eq!(rig.service.captures(), 1);
        assert_eq!(rig.service.discards(), 1, "the scratch login is cleaned up");
        assert!(rig.dirs().is_empty(), "{:?}", rig.dirs());
        assert_eq!(rig.slot(), "idle");
    }

    #[tokio::test]
    async fn login_run_that_panics_frees_the_slot() {
        let mut rig = rig("login-run-panics");
        rig.runner.plan(Plan::ExitPanics);

        let session = rig.start_running("l1").await;
        let _ = rig.login_event().await;

        assert_eq!(
            rig.login_event().await,
            (
                AccountLoginState::Failed,
                Some(session.clone()),
                Some("the login stopped unexpectedly".to_owned())
            )
        );
        assert_eq!(rig.slot(), "idle");
        assert_eq!(rig.dirs(), [session], "left for the sweep");
    }

    /// The terminals a [`SessionRunner`] spawns in a test: each a live
    /// session on the hub, wired as a command session is, whose tracer the
    /// test scripts. A kill reports `on_kill`, if set.
    struct FakeSessions {
        on_kill: Option<PtyExit>,
        scripts: Mutex<HashMap<String, ScriptedExit>>,
    }

    impl FakeSessions {
        fn new(on_kill: Option<PtyExit>) -> Arc<Self> {
            Arc::new(Self {
                on_kill,
                scripts: Mutex::new(HashMap::new()),
            })
        }

        fn with_script<T>(&self, session_id: &str, f: impl FnOnce(&ScriptedExit) -> T) -> T {
            f(locked(&self.scripts)
                .get(session_id)
                .expect("the session was spawned"))
        }

        fn chunk(&self, session_id: &str, bytes: &[u8]) {
            self.with_script(session_id, |script| script.chunk(bytes));
        }

        fn finish(&self, session_id: &str, exit: PtyExit) {
            self.with_script(session_id, |script| script.finish(exit));
        }
    }

    impl TestSpawner for FakeSessions {
        fn spawn(&self, hub: &Hub, spec: CommandSpec) -> anyhow::Result<CommandSession> {
            let (pty, script) = scripted_pty(self.on_kill);
            let output = pty.output.subscribe();
            let mut rec = record(&spec.session_id, SessionMode::PlainShell);
            rec.label.clone_from(&spec.label);
            rec.user_label.clone_from(&spec.user_label);
            let exit = insert_live_notifying(&hub.sessions, &hub.dirs, rec, &pty);
            let snapshot = crate::sync::lock(
                &hub.sessions
                    .get(&spec.session_id)
                    .expect("the session was inserted"),
            )
            .snapshot();
            locked(&self.scripts).insert(spec.session_id, script);
            Ok(CommandSession {
                snapshot,
                exit,
                output,
                cols: 40,
                rows: 10,
            })
        }
    }

    impl Rig {
        /// The `AccountLogin` broadcasts already sent, in order.
        fn login_events_now(&mut self) -> Vec<(AccountLoginState, Option<String>, Option<String>)> {
            let mut seen = Vec::new();
            loop {
                match self.events.try_recv() {
                    Ok(StateEvent::AccountLogin {
                        state,
                        session_id,
                        message,
                    }) => seen.push((state, session_id, message)),
                    Ok(_) | Err(broadcast::error::TryRecvError::Lagged(_)) => {}
                    Err(_) => return seen,
                }
            }
        }

        fn has_session(&self, session_id: &str) -> bool {
            self.hub.sessions.get(session_id).is_some()
        }
    }

    /// A scratch login root under `hub`'s scratch folder, with one folder
    /// per id.
    fn login_root(scratch: &ScratchDir, ids: &[&str]) -> PathBuf {
        let root = scratch.path().join("account-login");
        for id in ids {
            std::fs::create_dir_all(root.join(id)).expect("the login folder can be made");
            std::fs::write(root.join(id).join(".credentials.json"), "{}")
                .expect("the login folder takes a file");
        }
        root
    }

    fn ids(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|id| (*id).to_owned()).collect()
    }

    #[tokio::test]
    async fn command_session_exit_before_await_is_kept() {
        let (hub, _scratch) = test_hub("command-session-early-exit");
        let (pty, script) = scripted_pty(None);
        let exit = insert_live_notifying(
            &hub.sessions,
            &hub.dirs,
            record("cmd-1", SessionMode::PlainShell),
            &pty,
        );

        script.finish(PtyExit::Code(3));
        tokio::time::timeout(HANG, async {
            loop {
                let rec = hub.sessions.get("cmd-1").expect("the record stays");
                if crate::sync::lock(&rec).exit_code == Some(3) {
                    return;
                }
                drop(rec);
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("hung: the exit was not handled");

        assert_eq!(
            tokio::time::timeout(HANG, exit)
                .await
                .expect("hung: the exit notice never came"),
            Ok(crate::session::ExitFacts::Exited { code: 3 })
        );
    }

    #[tokio::test]
    async fn login_failed_message_carries_last_screen_lines() {
        let sessions = FakeSessions::new(Some(PtyExit::Code(1)));
        let mut rig = rig_with("login-screen-tail", Some(&sessions));
        let session = rig.start_running("l1").await;
        let _ = rig.login_event().await;

        sessions.chunk(
            &session,
            b"line 1\r\nline 2\r\n\r\nline 3\r\nline 4\r\nline 5\r\nline 6\r\nline 7\r\n",
        );
        sessions.finish(&session, PtyExit::Code(3));

        assert_eq!(
            rig.login_event().await,
            (
                AccountLoginState::Failed,
                Some(session.clone()),
                Some(
                    "the login exited with code 3\nline 3\nline 4\nline 5\nline 6\nline 7"
                        .to_owned()
                )
            )
        );
        assert!(rig.dirs().is_empty(), "{:?}", rig.dirs());
        assert_eq!(rig.slot(), "idle");
    }

    #[tokio::test]
    async fn login_drain_shutdown_claims_before_shutdown_all() {
        let sessions = FakeSessions::new(Some(PtyExit::Code(1)));
        let mut rig = rig_with("login-drain-shutdown", Some(&sessions));
        let session = rig.start_running("l1").await;
        let _ = rig.login_event().await;
        let at_kill = Arc::new(Mutex::new(Vec::new()));
        {
            let at_kill = Arc::clone(&at_kill);
            let accounts = Arc::clone(&rig.accounts);
            sessions.with_script(&session, |script| {
                script.on_kill(move || {
                    let held = match &*crate::sync::lock(&accounts.login) {
                        LoginSlot::Ending {
                            reason: EndReason::Shutdown,
                            ..
                        } => "ending for the shutdown",
                        LoginSlot::Ending { .. } => "ending",
                        LoginSlot::Running { .. } => "running",
                        _ => "other",
                    };
                    locked(&at_kill).push(held);
                });
            });
        }

        let _acks = rig.send(ClientMessage::Shutdown { drain: true }).await;

        assert_eq!(
            rig.login_event().await,
            (
                AccountLoginState::Cancelled,
                Some(session.clone()),
                Some("the daemon is shutting down".to_owned())
            )
        );
        assert_eq!(*locked(&at_kill), ["ending for the shutdown"]);
        rig.accounts.join_driver(HANG).await;
        assert_eq!(rig.slot(), "idle");
        assert!(rig.dirs().is_empty(), "{:?}", rig.dirs());
        assert!(!rig.has_session(&session), "the run discarded its session");
    }

    #[tokio::test]
    async fn stop_accounts_joins_the_login_run() {
        let sessions = FakeSessions::new(Some(PtyExit::Code(1)));
        let mut rig = rig_with("login-stop-accounts", Some(&sessions));
        let session = rig.start_running("l1").await;
        let _ = rig.login_event().await;

        tokio::time::timeout(HANG, crate::server::stop_accounts(&rig.hub))
            .await
            .expect("hung: stop_accounts did not return");

        assert_eq!(
            rig.slot(),
            "idle",
            "the run ended before stop_accounts returned"
        );
        assert!(crate::sync::lock(&rig.accounts.login_task).is_none());
        assert_eq!(
            rig.login_events_now(),
            [(
                AccountLoginState::Cancelled,
                Some(session.clone()),
                Some("the daemon is shutting down".to_owned())
            )]
        );
        assert!(rig.dirs().is_empty(), "{:?}", rig.dirs());
        assert!(!rig.has_session(&session), "the run discarded its session");
    }

    #[tokio::test]
    async fn login_outside_stop_cancels() {
        let sessions = FakeSessions::new(Some(PtyExit::Code(1)));
        let mut rig = rig_with("login-outside-stop", Some(&sessions));
        let session = rig.start_running("l1").await;
        let _ = rig.login_event().await;

        let _replies = rig
            .send(ClientMessage::StopSession {
                session_id: session.clone(),
                cleanup: Vec::new(),
            })
            .await;

        assert_eq!(
            rig.login_event().await,
            (
                AccountLoginState::Cancelled,
                Some(session.clone()),
                Some("the login session was stopped".to_owned())
            )
        );
        assert_eq!(rig.slot(), "idle");
        assert!(rig.dirs().is_empty(), "{:?}", rig.dirs());
        assert!(!rig.has_session(&session), "the run discarded its session");
        assert_eq!(rig.service.discards(), 1, "the scratch login is cleaned up");
    }

    #[tokio::test]
    async fn login_discard_before_exit_waits_for_exit_facts() {
        let sessions = FakeSessions::new(None);
        let mut rig = rig_with("login-discard-before-exit", Some(&sessions));
        let session = rig.start_running("l1").await;
        let _ = rig.login_event().await;

        let _stopped = rig
            .send(ClientMessage::StopSession {
                session_id: session.clone(),
                cleanup: Vec::new(),
            })
            .await;
        let _discarded = rig
            .send(ClientMessage::DiscardSession {
                session_id: session.clone(),
                cleanup: Vec::new(),
            })
            .await;
        assert!(
            !rig.has_session(&session),
            "the pane close removed the record"
        );
        yield_a_few().await;

        assert_eq!(rig.slot(), "running", "no exit facts yet");
        assert_eq!(rig.dirs(), [session.as_str()], "the child may still run");
        rig.no_login_event();

        sessions.finish(&session, PtyExit::Code(1));

        assert_eq!(
            rig.login_event().await,
            (
                AccountLoginState::Cancelled,
                Some(session.clone()),
                Some("the login session was stopped".to_owned())
            )
        );
        assert_eq!(rig.slot(), "idle");
        assert!(rig.dirs().is_empty(), "{:?}", rig.dirs());
    }

    #[tokio::test]
    async fn login_stop_after_pty_taken_never_orphan_kills() {
        let sessions = FakeSessions::new(None);
        let mut rig = rig_with("login-stop-after-pty-taken", Some(&sessions));
        let session = rig.start_running("l1").await;
        let _ = rig.login_event().await;
        let stops = || {
            let rec = rig.hub.sessions.get(&session).expect("the record stays");
            let rec = crate::sync::lock(&rec);
            rec.recent_actions
                .iter()
                .filter(|action| action.contains("stopped by user"))
                .count()
        };

        // The drain's stop takes the pty; the run's own stop comes after it,
        // while the exit is not in yet.
        let _acks = rig.send(ClientMessage::Shutdown { drain: true }).await;
        yield_a_few().await;

        assert_eq!(stops(), 1, "only the drain stopped the session");
        assert_eq!(rig.slot(), "ending", "the run waits for the exit facts");
        sessions.finish(&session, PtyExit::Code(1));
        assert_eq!(
            rig.login_event().await,
            (
                AccountLoginState::Cancelled,
                Some(session.clone()),
                Some("the daemon is shutting down".to_owned())
            )
        );
        assert!(rig.dirs().is_empty(), "{:?}", rig.dirs());
    }

    #[tokio::test]
    async fn login_session_carries_its_title_as_the_user_label() {
        let sessions = FakeSessions::new(None);
        let mut rig = rig_with("login-user-label", Some(&sessions));
        let session = rig.start_running("l1").await;
        let _ = rig.login_event().await;

        let rec = rig
            .hub
            .sessions
            .get(&session)
            .expect("the login session was spawned");
        let rec = crate::sync::lock(&rec);
        assert_eq!(
            rec.user_label.as_deref(),
            Some(LOGIN_TITLE),
            "the sidebar shows the title the login's hint names"
        );
        assert_eq!(rec.label, LOGIN_TITLE);
    }

    #[tokio::test]
    async fn login_tracer_lost_fails() {
        let sessions = FakeSessions::new(Some(PtyExit::Code(1)));
        let mut rig = rig_with("login-tracer-lost", Some(&sessions));
        let session = rig.start_running("l1").await;
        let _ = rig.login_event().await;

        sessions.chunk(&session, b"waiting for the browser\r\n");
        sessions.finish(&session, PtyExit::TracerLost);

        assert_eq!(
            rig.login_event().await,
            (
                AccountLoginState::Failed,
                Some(session.clone()),
                Some("lost the login terminal\nwaiting for the browser".to_owned())
            )
        );
        assert_eq!(rig.slot(), "idle");
        assert!(rig.dirs().is_empty(), "{:?}", rig.dirs());
        assert!(
            !rig.has_session(&session),
            "the run discarded the abandoned session"
        );
    }

    #[tokio::test]
    async fn sweep_stops_a_reattached_login_session() {
        let (hub, scratch) = test_hub("sweep-reattached");
        let root = login_root(&scratch, &["login-1"]);
        let (pty, script) = scripted_pty(None);
        insert_live(
            &hub.sessions,
            &hub.dirs,
            record("login-1", SessionMode::PlainShell),
            &pty,
        );
        drop(pty);
        write_meta_for(&hub.dirs, "login-1");
        let killed = Arc::new(tokio::sync::Notify::new());
        {
            let killed = Arc::clone(&killed);
            script.on_kill(move || killed.notify_one());
        }

        let sweep = sweep(&hub, &root);
        tokio::pin!(sweep);
        tokio::select! {
            _ = &mut sweep => unreachable!("the sweep ended before the child was gone"),
            () = killed.notified() => {}
        }
        assert!(
            root.join("login-1").is_dir(),
            "kept while the child may run"
        );
        script.finish(PtyExit::Code(1));
        let swept = tokio::time::timeout(HANG, sweep)
            .await
            .expect("hung: the sweep did not end");

        assert_eq!(swept, ids(&["login-1"]));
        assert!(hub.sessions.get("login-1").is_none(), "the record is gone");
        assert!(
            !hub.dirs.sessions_dir.join("login-1").exists(),
            "the session dir is gone"
        );
        assert!(!root.join("login-1").exists(), "the login folder is gone");
    }

    #[tokio::test]
    async fn sweep_discards_an_abandoned_login_and_its_resume() {
        let (hub, scratch) = test_hub("sweep-abandoned");
        let root = login_root(&scratch, &["login-1"]);
        let mut rec = record("login-1", SessionMode::PlainShell);
        rec.status = protocol::SessionStatus::Stopped;
        rec.is_abandoned = true;
        hub.sessions.insert_pending(rec).publish();
        write_meta_for(&hub.dirs, "login-1");

        let swept = tokio::time::timeout(HANG, sweep(&hub, &root))
            .await
            .expect("hung: the sweep did not end");

        assert_eq!(swept, ids(&["login-1"]), "its id leaves the resume list");
        assert!(hub.sessions.get("login-1").is_none(), "the record is gone");
        assert!(
            !hub.dirs.sessions_dir.join("login-1").exists(),
            "the sidecar is gone"
        );
        assert!(!root.join("login-1").exists(), "the login folder is gone");
    }

    #[tokio::test]
    async fn sweep_removes_a_scratch_dir_with_no_session() {
        let (hub, scratch) = test_hub("sweep-no-session");
        let root = login_root(&scratch, &["login-1"]);

        let swept = tokio::time::timeout(HANG, sweep(&hub, &root))
            .await
            .expect("hung: the sweep did not end");

        assert_eq!(swept, ids(&["login-1"]));
        assert!(!root.join("login-1").exists(), "the login folder is gone");
        assert!(root.is_dir(), "the root stays");
    }

    #[tokio::test]
    async fn sweep_leaves_other_sessions_alone() {
        let (hub, scratch) = test_hub("sweep-others");
        let root = login_root(&scratch, &["login-1"]);
        let (pty, _script) = scripted_pty(Some(PtyExit::Code(1)));
        insert_live(
            &hub.sessions,
            &hub.dirs,
            record("other-live", SessionMode::PlainShell),
            &pty,
        );
        let mut abandoned = record("other-abandoned", SessionMode::PlainShell);
        abandoned.status = protocol::SessionStatus::Stopped;
        abandoned.is_abandoned = true;
        hub.sessions.insert_pending(abandoned).publish();

        let swept = tokio::time::timeout(HANG, sweep(&hub, &root))
            .await
            .expect("hung: the sweep did not end");

        assert_eq!(swept, ids(&["login-1"]));
        let live = hub
            .sessions
            .get("other-live")
            .expect("the live session stays");
        assert!(crate::sync::lock(&live).pty.is_some(), "and keeps running");
        assert!(hub.sessions.get("other-abandoned").is_some());
        assert!(!root.join("login-1").exists());
    }
}
