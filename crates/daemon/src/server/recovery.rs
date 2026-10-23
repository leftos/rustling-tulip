//! Session history and recovery: reattaching orphans at startup, resuming abandoned sessions, recovering ended ones from history, and recovery claims.
use super::connection::ConnCtx;
use super::git::active_sessions_using_worktree;
use super::sessions::rerun_request;
use super::settings::{env_names, register_repo};
use super::spawn::{check_spawn_request, expected_output_subscribers, spawn_session};
use super::tabs::rebind_session_panes;
use super::{Hub, StateEvent};
use crate::orphan::OrphanMeta;
use crate::paths::Dirs;
use crate::session::{SessionRecord, SessionRegistry, attach_lifecycle};
use crate::{conversations, history, orphan, osc_title, pty_state, screen, tracer_client};
use agent_api::ResolvedRows;
use anyhow::anyhow;
use protocol::{
    Agent, ClientMessage, DaemonMessage, SessionEnd, SessionMember, SessionMode, SpawnRequest,
    SpawnTarget,
};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tracing::{info, warn};

/// Handle the history and recovery messages `dispatch` routes here.
pub(super) async fn handle(hub: &Hub, msg: ClientMessage, ctx: &ConnCtx<'_>) -> anyhow::Result<()> {
    let ConnCtx { out_tx, .. } = *ctx;
    match msg {
        ClientMessage::ResumeAbandoned { session_id } => {
            resume_abandoned(hub, &session_id, out_tx).await?;
        }
        ClientMessage::DiscardAbandoned { session_id } => {
            dismiss_abandoned(hub, &session_id, out_tx)?;
        }
        ClientMessage::ResumeAllAbandoned => {
            resume_all_abandoned(hub, out_tx).await;
        }
        ClientMessage::ListEnvNames { request_id } => {
            let names = env_names().await;
            let _ = out_tx.send(DaemonMessage::EnvNames { names, request_id });
        }
        ClientMessage::ListSessionHistory { request_id } => {
            let items = session_history_items(hub).await;
            let _ = out_tx.send(DaemonMessage::SessionHistory { request_id, items });
        }
        ClientMessage::RecoverSessions { request_id, items } => {
            let results = recover_sessions(hub, &items, out_tx).await;
            let _ = out_tx.send(DaemonMessage::RecoverResult {
                request_id,
                results,
            });
        }
        _ => warn!("recovery::handle: dispatch routed a message this module does not handle"),
    }
    Ok(())
}

/// The session ids being recovered or resumed right now.
#[derive(Clone, Default)]
pub struct RecoveryClaims(Arc<std::sync::Mutex<std::collections::HashSet<String>>>);

impl RecoveryClaims {
    /// Claims `id` for one recovery; None while another holds it. The claim
    /// ends when the returned guard drops, on every exit path.
    pub(crate) fn claim(&self, id: &str) -> Option<RecoveryClaim> {
        if !crate::sync::lock(&self.0).insert(id.to_owned()) {
            return None;
        }
        Some(RecoveryClaim {
            claims: self.clone(),
            id: id.to_owned(),
        })
    }
}

/// One held claim from [`RecoveryClaims::claim`].
pub(crate) struct RecoveryClaim {
    claims: RecoveryClaims,
    id: String,
}

impl Drop for RecoveryClaim {
    fn drop(&mut self) {
        crate::sync::lock(&self.claims.0).remove(&self.id);
    }
}

/// The error a recovery or Resume gets while another holds the session.
const ALREADY_RECOVERING: &str = "already being recovered";

/// Reattach orphan sessions (alive but detached) and abandoned sessions
/// (dead: a daemon crash, a reboot or a killed process tree ended them)
/// before any clients connect so the
/// initial state snapshot already includes them.
///
/// Post-C.3: orphans whose sidecar carries `tracer_pid` + `tracer_pipe` get
/// wired up to the still-running rt-tracer via pipe, restoring full IO.
/// A reattach failure (tracer died between liveness check and pipe open,
/// or another daemon claimed the pipe first) routes the session to the
/// abandoned bucket so the user can Resume from B.2's UI. Legacy sidecars
/// (no tracer fields) fall through to the read-only orphan path — pre-C.3
/// children can't survive the daemon anyway, so this branch is only ever
/// taken by sidecars from a downgrade scenario.
pub(super) async fn reattach_orphans(
    sessions: &Arc<SessionRegistry>,
    dirs: &Dirs,
    attention_tx: &mpsc::UnboundedSender<pty_state::AttentionEvent>,
    orphans: Vec<OrphanMeta>,
    abandoned: Vec<OrphanMeta>,
) {
    let total_start = std::time::Instant::now();
    let orphan_count = orphans.len();
    let abandoned_count = abandoned.len();
    info!(orphan_count, abandoned_count, "reattach_orphans: begin");
    // Reattach concurrently. Each orphan's tracer connect can block up to the
    // per-tracer reattach budget, so a sequential loop made boot time scale
    // with the number of unreachable tracers (N × budget) and stalled the
    // handshake — a single dead tracer would delay every session. All
    // reattaches still complete before the caller binds the listener, so the
    // "clients only see fully-wired sessions" invariant holds; the registry
    // mutations inside each task are independently locked.
    let mut tasks = Vec::with_capacity(orphans.len());
    for meta in orphans {
        tasks.push(tokio::spawn(reattach_one(
            Arc::clone(sessions),
            dirs.clone(),
            attention_tx.clone(),
            meta,
        )));
    }
    for task in tasks {
        if let Err(err) = task.await {
            warn!(?err, "reattach_orphans: reattach task failed to join");
        }
    }
    for meta in abandoned {
        sessions.insert_abandoned(&meta);
    }
    info!(
        orphan_count,
        abandoned_count,
        elapsed_ms = u64::try_from(total_start.elapsed().as_millis()).unwrap_or(u64::MAX),
        "reattach_orphans: done"
    );
}

/// Whether a reattached session's status runs on the shell's watcher: a plain
/// shell, or an agent typed into its shell at spawn.
fn runs_shell_watcher(meta: &OrphanMeta) -> bool {
    meta.mode == SessionMode::PlainShell || meta.agent_in_shell
}

/// Reattach a single orphan to its still-running tracer, or route it to the
/// abandoned bucket if the tracer is unreachable. Runs as its own task so a
/// slow/dead tracer doesn't serialize the others (see `reattach_orphans`).
async fn reattach_one(
    sessions: Arc<SessionRegistry>,
    dirs: Dirs,
    attention_tx: mpsc::UnboundedSender<pty_state::AttentionEvent>,
    meta: OrphanMeta,
) {
    let session_start = std::time::Instant::now();
    match (meta.tracer_pipe.as_deref(), meta.tracer_pid) {
        (Some(pipe), Some(tracer_pid)) => {
            match tracer_client::reattach(
                &meta.session_id,
                pipe,
                tracer_pid,
                expected_output_subscribers(meta.mode),
            )
            .await
            {
                Ok(pty) => {
                    info!(
                        session_id = %meta.session_id,
                        tracer_pid,
                        elapsed_ms = u64::try_from(session_start.elapsed().as_millis()).unwrap_or(u64::MAX),
                        "tracer reattach succeeded"
                    );
                    let pending = sessions.insert_reattached(&meta, Arc::clone(&pty));
                    let snap_tx = attach_lifecycle(
                        &sessions,
                        meta.session_id.clone(),
                        &pty,
                        Some(dirs.clone()),
                    );
                    // Only a shell's classifier reads input volume.
                    let shell_watcher = runs_shell_watcher(&meta);
                    let shell_input = shell_watcher.then(mpsc::unbounded_channel::<usize>);
                    let screen: screen::SharedScreen = Arc::new(Mutex::new(screen::Screen::new(
                        screen::REATTACH_COLS,
                        screen::REATTACH_ROWS,
                    )));
                    let restored = {
                        let mut guard = crate::sync::lock(pending.arc());
                        guard.scrollback_snapshot_req = Some(snap_tx);
                        if let Some((in_tx, _)) = &shell_input {
                            guard.input_notifier = Some(in_tx.clone());
                        }
                        guard.screen = Some(Arc::clone(&screen));
                        guard.status
                    };
                    pty.resize(screen::REATTACH_COLS, screen::REATTACH_ROWS);
                    if let Some((_, in_rx)) = shell_input {
                        pty_state::watch_plain_shell(
                            &sessions,
                            meta.session_id.clone(),
                            pty_state::ShellStreams {
                                output: pty.output.subscribe(),
                                input_pulses: in_rx,
                                screen,
                            },
                            attention_tx.clone(),
                            restored,
                        );
                    } else {
                        pty_state::watch(
                            &sessions,
                            meta.session_id.clone(),
                            pty_state::AgentStreams {
                                output: pty.output.subscribe(),
                                screen,
                            },
                            attention_tx.clone(),
                        );
                    }
                    osc_title::watch(
                        &sessions,
                        meta.session_id.clone(),
                        pty.output.subscribe(),
                        shell_watcher,
                    );
                    pending.publish();
                }
                Err(err) => {
                    warn!(
                        ?err,
                        session_id = %meta.session_id,
                        tracer_pid,
                        elapsed_ms = u64::try_from(session_start.elapsed().as_millis()).unwrap_or(u64::MAX),
                        "tracer reattach failed; routing to abandoned"
                    );
                    sessions.insert_abandoned(&meta);
                }
            }
        }
        _ => {
            sessions.insert_orphan(&meta);
        }
    }
}

/// Resume an abandoned session: read its sidecar (spawn config +
/// `last_prompt`), spawn a session with the same config — continuing the
/// session's own Claude conversation when its transcript is still there, else
/// replaying the captured first prompt — then hand the new session the old
/// one's name, look and sidebar place, remove the abandoned placeholder from
/// the registry and delete its on-disk sidecar.
///
/// The new session gets a new id (`spawn_session` generates one);
/// [`finish_resume`] rebinds every pane that pointed at the old id and moves
/// the id in each container's session order, so the resume lands in the slot
/// it left.
pub(super) async fn resume_abandoned(
    hub: &Hub,
    session_id: &str,
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
) -> anyhow::Result<()> {
    let spawn = move |req: SpawnRequest, busy_excluded: bool| async move {
        spawn_session(hub, req, None, busy_excluded)
            .await
            .map(|snap| snap.id)
    };
    resume_abandoned_with(hub, session_id, out_tx, spawn).await
}

/// [`resume_abandoned`], starting the session with `spawn`, which returns the
/// new session's id.
async fn resume_abandoned_with<F, Fut>(
    hub: &Hub,
    session_id: &str,
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
    spawn: F,
) -> anyhow::Result<()>
where
    F: Fn(SpawnRequest, bool) -> Fut,
    Fut: Future<Output = anyhow::Result<String>>,
{
    let _claim = hub
        .recovering
        .claim(session_id)
        .ok_or_else(|| anyhow!("session {session_id} is {ALREADY_RECOVERING}"))?;
    let (req, busy_excluded) = resume_request(hub, session_id).await?;
    let new_id = spawn(req, busy_excluded).await?;
    finish_resume(hub, session_id, &new_id, out_tx);
    Ok(())
}

/// The request that resumes the abandoned session `id`, and the busy
/// exclusion it keeps: the session's own conversation continued when its
/// transcript is still there, else a fresh start replaying the first prompt
/// (what a plain shell always gets). A conversation that can no longer be
/// resumed never refuses the Resume the user asked for, and neither does a
/// worktree that is gone (the unpinned request recreates it) or one a live
/// session holds (the unpinned spawn refuses it, as it always did).
async fn resume_request(hub: &Hub, id: &str) -> anyhow::Result<(SpawnRequest, bool)> {
    let (fallback, busy_excluded) = {
        let rec = hub
            .sessions
            .get(id)
            .ok_or_else(|| anyhow!("unknown session: {id}"))?;
        abandoned_resume_request(&crate::sync::lock(&rec))?
    };
    match rerun_request(hub, id).await {
        Ok(rerun)
            if rerun.request.resume_conversation.is_some()
                && pinned_worktrees_exist(&rerun.request.target)
                && !pinned_worktree_in_use(hub, id, &rerun.request.target) =>
        {
            let mut req = rerun.request;
            req.initial_prompt = None;
            Ok((req, busy_excluded))
        }
        Ok(_) => Ok((fallback, busy_excluded)),
        Err(err) => {
            warn!(session_id = %id, ?err, "resume: no rerun request, starting fresh");
            Ok((fallback, busy_excluded))
        }
    }
}

/// Whether every worktree path `target` pins still exists as a directory.
/// A pinned spawn of a folder that is gone refuses ("no longer readable"),
/// where the unpinned request recreates the worktree the session left.
fn pinned_worktrees_exist(target: &SpawnTarget) -> bool {
    match target {
        SpawnTarget::Single {
            existing_worktree: Some(path),
            ..
        } => Path::new(path).is_dir(),
        SpawnTarget::Workspace {
            existing_worktrees, ..
        } => existing_worktrees
            .iter()
            .all(|pin| Path::new(&pin.path).is_dir()),
        _ => true,
    }
}

/// The request that resumes the abandoned `rec`, replaying its first prompt,
/// and the busy exclusion the resumed session keeps.
fn abandoned_resume_request(rec: &SessionRecord) -> anyhow::Result<(SpawnRequest, bool)> {
    let session_id = &rec.id;
    if !rec.is_abandoned {
        return Err(anyhow!(
            "session {session_id} is not abandoned; refusing to resume"
        ));
    }
    let Some(stored) = &rec.spawn_config else {
        return Err(anyhow!(
            "session {session_id} has no stored spawn config; cannot resume"
        ));
    };
    let mut req = stored.to_clone_request();
    req.initial_prompt.clone_from(&rec.last_prompt);
    Ok((req, rec.busy_excluded))
}

/// After a successful Resume spawn of the abandoned `old_id` as `new_id`:
/// rebind any pane that pointed at the abandoned session to the fresh one,
/// in every client layout, so the resume reattaches to its original slot
/// instead of orphaning into the sidebar as an unbound session; give the
/// fresh session the name, the look and the sidebar place the abandoned one
/// held; remove the abandoned placeholder and its sidecar (if the spawn
/// failed, the placeholder stays so the user can try again); and mark its
/// history entry recovered, so it stops counting as a session to recover.
fn finish_resume(
    hub: &Hub,
    old_id: &str,
    new_id: &str,
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
) {
    rebind_session_panes(hub, old_id, new_id);
    carry_identity(hub, old_id, new_id);
    keep_session_order(hub, old_id, new_id);
    discard_abandoned(hub, old_id, out_tx);
    mark_history_recovered(hub, old_id);
}

/// Give the spawned `new_id` the name and the look of the abandoned `old_id`
/// it resumed, as `RenameSession` and `SetSessionAppearance` set them. An old
/// record with no user rename, or a new one the registry does not hold,
/// leaves the new record's own default label.
fn carry_identity(hub: &Hub, old_id: &str, new_id: &str) {
    let Some((user_label, appearance)) = hub.sessions.get(old_id).map(|rec| {
        let rec = crate::sync::lock(&rec);
        (rec.user_label.clone(), rec.appearance.clone())
    }) else {
        return;
    };
    hub.sessions.update(new_id, |guard| {
        guard.user_label.clone_from(&user_label);
        guard.label = user_label
            .clone()
            .unwrap_or_else(|| guard.default_label.clone());
        guard.appearance.clone_from(&appearance);
    });
}

/// Put `new_id` in `old_id`'s place in every container's session order, and
/// tell the clients, so the resumed session keeps the sidebar slot it held. A
/// failed save is logged and never fails the resume.
fn keep_session_order(hub: &Hub, old_id: &str, new_id: &str) {
    let changed = match crate::registry::replace_session_in_order(&hub.state, old_id, new_id) {
        Ok(changed) => changed,
        Err(err) => {
            warn!(?err, %old_id, %new_id, "finish_resume: state.mutate failed");
            return;
        }
    };
    for (container_id, ordered_ids) in changed {
        let _ = hub.state_events.send(StateEvent::SessionsReordered {
            container_id,
            ordered_ids,
        });
    }
}

/// Resume, one at a time and with no client, the sessions the last run lost
/// with no end recorded (`ids`): each one that is an abandoned interactive
/// Claude session whose own conversation can be continued in the folders it
/// ran in is respawned continuing that conversation, in its panes. Any other
/// stays abandoned, for Recover. Returns how many were resumed and how many
/// were left, and keeps them in `Hub::start_resume`.
pub(super) async fn auto_resume(
    hub: Hub,
    ids: Vec<String>,
    in_flight: ResumeInFlight,
) -> (usize, usize) {
    let spawn = |req: SpawnRequest, busy_excluded: bool| {
        let hub = &hub;
        async move {
            spawn_session(hub, req, None, busy_excluded)
                .await
                .map(|snap| snap.id)
        }
    };
    auto_resume_with(&hub, ids, spawn, in_flight).await
}

/// `Hub::resume_in_flight`, held true from [`ResumeInFlight::start`] until
/// the guard drops: when the resume ends, panics or is aborted.
pub(super) struct ResumeInFlight(Arc<AtomicBool>);

impl ResumeInFlight {
    pub(super) fn start(flag: &Arc<AtomicBool>) -> Self {
        flag.store(true, Ordering::SeqCst);
        Self(Arc::clone(flag))
    }
}

impl Drop for ResumeInFlight {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// [`auto_resume`], starting each session with `spawn`, which returns the
/// new session's id.
async fn auto_resume_with<F, Fut>(
    hub: &Hub,
    ids: Vec<String>,
    spawn: F,
    in_flight: ResumeInFlight,
) -> (usize, usize)
where
    F: Fn(SpawnRequest, bool) -> Fut,
    Fut: Future<Output = anyhow::Result<String>>,
{
    let _in_flight = in_flight;
    // The registry's own events tell every client; nobody asked for these.
    let (out_tx, _) = mpsc::unbounded_channel();
    let (mut resumed, mut left) = (0, 0);
    for id in ids {
        match auto_resume_one(hub, &id, &spawn, &out_tx).await {
            Ok(Some(new_id)) => {
                info!(session_id = %id, %new_id, "auto-resume at start: resumed");
                resumed += 1;
            }
            Ok(None) => {}
            Err(reason) => {
                info!(session_id = %id, %reason, "auto-resume at start: left for Recover");
                left += 1;
            }
        }
    }
    info!(resumed, left, "auto-resume at start finished");
    *crate::sync::lock(&hub.start_resume) = Some((resumed, left));
    if resumed > 0 || left > 0 {
        let _ = hub.state_events.send(StateEvent::ResumeReport {
            resumed: u32::try_from(resumed).unwrap_or(u32::MAX),
            left_in_recover: u32::try_from(left).unwrap_or(u32::MAX),
        });
    }
    (resumed, left)
}

/// Auto-resume the lost session `id`: the new session's id, `None` when a
/// recovery or a Resume already holds it or it is already gone (resumed,
/// recovered or dismissed), or why it stays abandoned.
async fn auto_resume_one<F, Fut>(
    hub: &Hub,
    id: &str,
    spawn: &F,
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
) -> Result<Option<String>, String>
where
    F: Fn(SpawnRequest, bool) -> Fut,
    Fut: Future<Output = anyhow::Result<String>>,
{
    let Some(_claim) = hub.recovering.claim(id) else {
        info!(session_id = %id, "auto-resume at start: skipped, a recovery or Resume holds it");
        return Ok(None);
    };
    if hub.sessions.get(id).is_none() {
        info!(session_id = %id, "auto-resume at start: skipped, the session is already gone");
        return Ok(None);
    }
    let (req, busy_excluded) = auto_resume_request(hub, id).await?;
    let new_id = spawn(req, busy_excluded)
        .await
        .map_err(|err| format!("spawn failed: {err:#}"))?;
    finish_resume(hub, id, &new_id, out_tx);
    Ok(Some(new_id))
}

/// The request that continues the lost session `id`'s own conversation in the
/// folders it ran in, and the busy exclusion it keeps; or why it can't.
async fn auto_resume_request(hub: &Hub, id: &str) -> Result<(SpawnRequest, bool), String> {
    let (busy_excluded, members) = {
        let rec = hub
            .sessions
            .get(id)
            .ok_or_else(|| "no such session".to_owned())?;
        let rec = crate::sync::lock(&rec);
        if let Some(reason) = auto_resume_refusal(&rec) {
            return Err(reason.to_owned());
        }
        (rec.busy_excluded, rec.members.clone())
    };
    let rerun = rerun_request(hub, id)
        .await
        .map_err(|err| format!("{err:#}"))?;
    if rerun.unpinned {
        return Err("its worktrees could not be pinned to the folders they ran in".to_owned());
    }
    if rerun.request.resume_conversation.is_none() {
        return Err("its conversation's transcript was not found".to_owned());
    }
    if !in_place_checkouts_on_branch(&rerun.request.target, &members).await {
        return Err("its checkout is on another branch".to_owned());
    }
    if pinned_worktree_in_use(hub, id, &rerun.request.target) {
        return Err("its worktree is in use by another session".to_owned());
    }
    Ok((rerun.request, busy_excluded))
}

/// Whether an in-place `target` needs no checkout: every member's folder is
/// already on the branch the spawn would check out. A worktree or standalone
/// target needs none.
async fn in_place_checkouts_on_branch(target: &SpawnTarget, members: &[SessionMember]) -> bool {
    let (SpawnTarget::Single {
        use_worktree: false,
        branch_name: branch,
        ..
    }
    | SpawnTarget::Workspace {
        use_worktree: false,
        branch_name: branch,
        ..
    }) = target
    else {
        return true;
    };
    if members.is_empty() {
        return false;
    }
    for member in members {
        let current = crate::git::current_branch(Path::new(&member.worktree_path)).await;
        if !matches!(current, Ok(Some(ref name)) if name == branch) {
            return false;
        }
    }
    true
}

/// Whether a live session other than `id` holds a worktree `target` pins.
fn pinned_worktree_in_use(hub: &Hub, id: &str, target: &SpawnTarget) -> bool {
    let pinned: Vec<&str> = match target {
        SpawnTarget::Single {
            existing_worktree: Some(path),
            ..
        } => vec![path.as_str()],
        SpawnTarget::Workspace {
            existing_worktrees, ..
        } => existing_worktrees
            .iter()
            .map(|pin| pin.path.as_str())
            .collect(),
        _ => Vec::new(),
    };
    pinned
        .into_iter()
        .any(|path| !active_sessions_using_worktree(hub, path, Some(id)).is_empty())
}

/// Why the record `rec` is no session to resume unattended, if it is not.
fn auto_resume_refusal(rec: &SessionRecord) -> Option<&'static str> {
    if !rec.is_abandoned {
        return Some("not abandoned");
    }
    if rec.mode != SessionMode::Interactive {
        return Some("not an interactive session");
    }
    if !matches!(rec.agent, Agent::Claude) {
        return Some("not a Claude session");
    }
    if rec.spawn_config.is_none() {
        return Some("no stored spawn config");
    }
    if rec.claude_session_id.is_none() {
        return Some("no recorded conversation");
    }
    None
}

/// Resume every session currently flagged as abandoned. Iterates the
/// registry snapshot (taken upfront to avoid mutating it while iterating),
/// calls `resume_abandoned` for each, and emits an `Error` message per
/// failure so the user can see which ones didn't make it. Successful
/// resumes are handled by `resume_abandoned` itself (the spawn broadcasts
/// the new session's `SessionUpdated`, and it consumes the sidecar).
async fn resume_all_abandoned(hub: &Hub, out_tx: &mpsc::UnboundedSender<DaemonMessage>) {
    let abandoned_ids: Vec<String> = hub
        .sessions
        .snapshots()
        .into_iter()
        .filter(|s| s.is_abandoned)
        .map(|s| s.id)
        .collect();
    info!(
        count = abandoned_ids.len(),
        "resume_all_abandoned: starting bulk resume"
    );
    for id in abandoned_ids {
        if let Err(err) = resume_abandoned(hub, &id, out_tx).await {
            warn!(session_id = %id, ?err, "bulk resume: per-session failure");
            let _ = out_tx.send(DaemonMessage::Error {
                message: format!("resume {id}: {err}"),
                request_id: None,
            });
        }
    }
}

/// Record in the session history that `session_id` ended for `end`. The first
/// record for a session wins; see [`history::write_if_absent`].
pub(super) fn record_end(hub: &Hub, session_id: &str, end: SessionEnd) {
    history::record_session_end(&hub.sessions, &hub.dirs, session_id, end);
}

/// The session history as the client lists it, read off the async runtime.
/// A session still in the Abandoned group is listed too: recovering it from
/// the history removes it from that group.
async fn session_history_items(hub: &Hub) -> Vec<protocol::SessionHistoryItem> {
    let dirs = hub.dirs.clone();
    let repos = hub.state.with_persisted(|s| s.repos.clone());
    tokio::task::spawn_blocking(move || {
        history::history_items(&dirs, &repos, &|entry| {
            conversations::lookup_rows(history::recorded_env(entry))
        })
    })
    .await
    .unwrap_or_else(|err| {
        warn!(?err, "building the session history failed");
        Vec::new()
    })
}

/// Broadcast the session history to every client whenever it changes while
/// the daemon runs: a session ending, or one being recovered.
pub(super) fn start_history_broadcaster(hub: &Hub) {
    let hub = hub.clone();
    let mut changes = history::subscribe_changes();
    tokio::spawn(async move {
        while changes.changed().await.is_ok() {
            let items = session_history_items(&hub).await;
            let _ = hub.state_events.send(StateEvent::SessionHistory(items));
        }
    });
}

/// Recover each item in order: plan it from its history entry, register its
/// repo when asked, spawn it, and mark the entry recovered. A failed item
/// doesn't stop the others; each gets its own result.
async fn recover_sessions(
    hub: &Hub,
    items: &[protocol::RecoverItem],
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
) -> Vec<protocol::RecoverItemResult> {
    let mut results = Vec::with_capacity(items.len());
    for item in items {
        let outcome = recover_one(hub, item, out_tx).await;
        if let Err(error) = &outcome {
            warn!(history_id = %item.history_id, %error, "recover_sessions: item failed");
        }
        let (session_id, error) = match outcome {
            Ok(id) => (Some(id), None),
            Err(error) => (None, Some(error)),
        };
        results.push(protocol::RecoverItemResult {
            history_id: item.history_id.clone(),
            session_id,
            error,
        });
    }
    results
}

async fn recover_one(
    hub: &Hub,
    item: &protocol::RecoverItem,
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
) -> Result<String, String> {
    let _claim = hub
        .recovering
        .claim(&item.history_id)
        .ok_or_else(|| ALREADY_RECOVERING.to_owned())?;
    let entry = history::read_one(&hub.dirs, &item.history_id)
        .ok_or_else(|| format!("no history entry {}", item.history_id))?;
    let busy_excluded = {
        let live = hub.sessions.get(&item.history_id);
        let guard = live.as_ref().map(|rec| crate::sync::lock(rec));
        recovery_busy_excluded(guard.as_deref(), &entry)
    };
    let (repos, workspaces) = hub
        .state
        .with_persisted(|s| (s.repos.clone(), s.workspaces.clone()));
    let planned_item = item.clone();
    // Planning looks up transcripts on disk.
    let mut plan = tokio::task::spawn_blocking(move || {
        let registered = history::Registered {
            repos: &repos,
            workspaces: &workspaces,
        };
        let rows = conversations::lookup_rows(history::recorded_env(&entry));
        history::plan_recovery(&entry, &planned_item, &registered, |agent, folder, id| {
            recovery_conversation_exists(agent, folder, id, &rows)
        })
    })
    .await
    .map_err(|err| format!("planning the recovery failed: {err}"))??;
    // A row the recovery would spawn with that can't be resolved — an unset env
    // variable, or a saved secret no longer in the store — or that couldn't be
    // sealed, or a provider key that isn't set, refuses it here, on the request
    // the plan will spawn and before the registration and branch lookup below:
    // registering a repo writes `state.json` and broadcasts `Repos`, and a
    // recovery that then failed would leave a repo the user never asked for
    // behind a spawn that never happened. Planning only reads.
    check_spawn_request(&plan.request).map_err(|failure| format!("{failure}"))?;
    if let Some(path) = plan.register_repo.clone() {
        let repo = register_repo(hub, &path, None, out_tx)
            .await
            .map_err(|err| format!("registering {path}: {err:#}"))?;
        plan.bind_registered_repo(&repo.id);
    }
    if let Some(folder) = plan.placeholder_branch_folder() {
        let current = crate::git::current_branch(Path::new(folder)).await;
        plan.name_branch(history::branch_or_placeholder(current));
    }
    let snapshot = spawn_session(hub, plan.request, None, busy_excluded)
        .await
        .map_err(|err| format!("{err:#}"))?;
    finish_recovery(hub, &item.history_id, &snapshot.id, out_tx);
    Ok(snapshot.id)
}

/// The busy exclusion a recovered session starts with. A session still in
/// the Abandoned group (`live`) may have been toggled after its history entry
/// was written, so its flag wins; otherwise the entry's.
fn recovery_busy_excluded(live: Option<&SessionRecord>, entry: &protocol::HistoryEntry) -> bool {
    match live {
        Some(rec) if rec.is_abandoned => rec.busy_excluded,
        _ => entry.busy_excluded,
    }
}

/// Whether `agent` still holds conversation `id` for `folder`, looked up with
/// the session's `rows`.
pub(super) fn recovery_conversation_exists(
    agent: Agent,
    folder: &str,
    id: &str,
    rows: &ResolvedRows,
) -> bool {
    conversations::exists(agent_registry::plugin(agent), Path::new(folder), id, rows)
}

/// After a successful recovery spawn of `history_id` as `new_id`: mark the
/// history entry recovered and, when that session still sits in the
/// Abandoned group, hand its panes to `new_id` in every client layout and
/// remove it from there. Its history entry already exists, so the removal
/// writes no second end.
fn finish_recovery(
    hub: &Hub,
    history_id: &str,
    new_id: &str,
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
) {
    mark_history_recovered(hub, history_id);
    let still_abandoned = hub
        .sessions
        .get(history_id)
        .is_some_and(|rec| crate::sync::lock(&rec).is_abandoned);
    if still_abandoned {
        rebind_session_panes(hub, history_id, new_id);
        discard_abandoned(hub, history_id, out_tx);
    }
}

/// Stamp the history entry for `session_id` recovered now, logging a failure
/// instead of returning it: the recovery itself already succeeded.
pub(super) fn mark_history_recovered(hub: &Hub, session_id: &str) {
    if let Err(err) = history::mark_recovered(&hub.dirs, session_id, chrono::Utc::now()) {
        warn!(?err, history_id = %session_id, "failed to mark history entry recovered");
    }
}

/// The user's Dismiss of an abandoned session: its start-up loss entry
/// becomes `StoppedByUser`, so it stops counting as a session to recover, and
/// the session is discarded.
fn dismiss_abandoned(
    hub: &Hub,
    session_id: &str,
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
) -> anyhow::Result<()> {
    let _claim = hub
        .recovering
        .claim(session_id)
        .ok_or_else(|| anyhow!("This session is being resumed."))?;
    if let Err(err) = history::dismiss_start_up_loss(&hub.dirs, session_id) {
        warn!(?err, %session_id, "failed to record the dismissed session as stopped");
    }
    discard_abandoned(hub, session_id, out_tx);
    Ok(())
}

/// Remove an abandoned session from the registry + delete its sidecar.
/// Used both as the cleanup step after a successful Resume and as the
/// user-initiated "Dismiss" action for sessions they don't want to
/// recover.
pub(super) fn discard_abandoned(
    hub: &Hub,
    session_id: &str,
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
) {
    record_end(hub, session_id, SessionEnd::StoppedByUser);
    hub.sessions.delete_sidecar(session_id);
    orphan::try_delete_session_dir(&hub.dirs, session_id);
    hub.sessions.remove(session_id);
    let _ = out_tx.send(DaemonMessage::SessionRemoved {
        session_id: session_id.to_string(),
    });
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use crate::env_secrets;
    use crate::server::spawn::check_spawn_request_with;
    use crate::server::test_support::{
        MISSING_SECRET_ID, UNSET_REF, deepseek_key, deepseek_options, deepseek_spawn, dispatch_one,
        env, env_ref_repo_hub, env_spawn, git_init, git_ok, idle_record, secret_ref,
        spawnless_test_hub, test_hub,
    };
    use chrono::Utc;
    use protocol::{SessionMember, SpawnTarget};
    use std::path::PathBuf;
    use tokio::sync::broadcast;

    #[test]
    fn a_reattached_agent_in_shell_session_runs_the_plain_shell_watcher() {
        let meta = |mode| {
            crate::orphan::meta_from_record(
                "s1".to_owned(),
                4242,
                "s1".to_owned(),
                protocol::SessionKind::Standalone,
                mode,
                Vec::new(),
                Utc::now(),
                None,
                Some("claude".to_owned()),
                protocol::Agent::Claude,
                None,
                None,
                None,
                Some(4242),
                Some(r"\\.\pipe\rt-tracer-s1".to_owned()),
                None,
                None,
            )
            .expect("build meta")
        };
        let mut hosted = meta(SessionMode::Interactive);
        assert!(
            !runs_shell_watcher(&hosted),
            "an agent that is the PTY child runs the agent watcher"
        );
        hosted.agent_in_shell = true;
        assert!(
            runs_shell_watcher(&hosted),
            "an agent inside its shell runs the shell's watcher"
        );
        assert!(runs_shell_watcher(&meta(SessionMode::PlainShell)));
    }
    #[tokio::test]
    #[expect(clippy::panic, reason = "a wrong reply fails the test loudly")]
    async fn list_env_names_replies_with_names_and_no_values() {
        const NAME: &str = "RT_TEST_LIST_ENV_NAMES_7C21";
        const SENTINEL: &str = "rt-sentinel-value-5d9a0e";
        // SAFETY: the name is unique to this test and nothing else reads or
        // writes it; std serializes environment access on Windows.
        unsafe { std::env::set_var(NAME, SENTINEL) };
        let (hub, _scratch) = test_hub("list-env-names");
        let reply = dispatch_one(
            &hub,
            ClientMessage::ListEnvNames {
                request_id: Some("r1".to_owned()),
            },
        )
        .await;
        // SAFETY: as above.
        unsafe { std::env::remove_var(NAME) };
        let json = serde_json::to_string(&reply).expect("the reply serializes");
        let DaemonMessage::EnvNames { names, request_id } = reply else {
            panic!("expected env_names, got {reply:?}");
        };
        assert_eq!(request_id.as_deref(), Some("r1"));
        let entry = names
            .iter()
            .find(|entry| entry.name == NAME)
            .expect("the process variable is listed");
        assert!(entry.scopes.contains(&protocol::EnvScope::Process));
        assert!(!json.contains(SENTINEL), "no value on the wire");
    }

    #[tokio::test]
    #[expect(clippy::panic, reason = "a wrong reply fails the test loudly")]
    async fn history_lists_abandoned_tracer_lost_session() {
        use crate::history::test_support::record;
        let (hub, _scratch) = spawnless_test_hub("history-abandoned");
        write_history(&hub, "gone", SessionMode::PlainShell, false);
        write_history(&hub, "abandoned", SessionMode::PlainShell, false);
        write_history(&hub, "stopped", SessionMode::PlainShell, false);
        let mut abandoned = record("abandoned", SessionMode::PlainShell);
        abandoned.is_abandoned = true;
        abandoned.status = protocol::SessionStatus::Stopped;
        hub.sessions.insert(abandoned);
        let mut stopped = record("stopped", SessionMode::PlainShell);
        stopped.status = protocol::SessionStatus::Stopped;
        hub.sessions.insert(stopped);
        let reply =
            dispatch_one(&hub, ClientMessage::ListSessionHistory { request_id: None }).await;
        let DaemonMessage::SessionHistory { items, .. } = reply else {
            panic!("expected session_history, got {reply:?}");
        };
        let mut ids: Vec<&str> = items.iter().map(|i| i.entry.session_id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, ["abandoned", "gone", "stopped"]);
        let abandoned_item = items
            .iter()
            .find(|i| i.entry.session_id == "abandoned")
            .expect("abandoned listed");
        assert_eq!(abandoned_item.entry.end, SessionEnd::TracerLost);
    }

    /// Insert `id` as an abandoned session with an unrecovered `TracerLost`
    /// history entry, as a tracer loss leaves it.
    fn insert_abandoned_with_history(hub: &Hub, id: &str) {
        write_history(hub, id, SessionMode::Interactive, false);
        let mut rec = crate::history::test_support::record(id, SessionMode::Interactive);
        rec.is_abandoned = true;
        rec.status = protocol::SessionStatus::Stopped;
        hub.sessions.insert(rec);
    }

    #[tokio::test]
    async fn recovering_abandoned_entry_removes_abandoned_session() {
        let (hub, _scratch) = spawnless_test_hub("recover-abandoned");
        insert_abandoned_with_history(&hub, "lost");
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        finish_recovery(&hub, "lost", "fresh", &out_tx);
        assert!(
            hub.sessions.get("lost").is_none(),
            "left the Abandoned group"
        );
        assert!(matches!(
            out_rx.try_recv(),
            Ok(DaemonMessage::SessionRemoved { ref session_id }) if session_id == "lost"
        ));
        let entry = history::read_one(&hub.dirs, "lost").expect("entry kept");
        assert!(entry.recovered_at.is_some(), "marked recovered");
        assert_eq!(entry.end, SessionEnd::TracerLost, "no second end written");
    }

    #[tokio::test]
    async fn dismissing_a_start_up_loss_records_stopped_by_user() {
        let (hub, _scratch) = spawnless_test_hub("dismiss-start-up-loss");
        crate::history::test_support::write_meta_for(&hub.dirs, "lost");
        let meta = orphan::load_meta(&hub.dirs, "lost").expect("load meta");
        let ids = history::record_lost_at_start(&hub.dirs, std::slice::from_ref(&meta), Utc::now());
        assert_eq!(ids, vec!["lost".to_owned()]);
        let lost = history::read_one(&hub.dirs, "lost").expect("start-up entry");
        hub.sessions.insert_abandoned(&meta);
        let (out_tx, _out_rx) = mpsc::unbounded_channel();
        dismiss_abandoned(&hub, "lost", &out_tx).expect("nothing holds it");
        assert!(hub.sessions.get("lost").is_none(), "dismissed");
        let entry = history::read_one(&hub.dirs, "lost").expect("entry kept");
        assert_eq!(entry.end, SessionEnd::StoppedByUser);
        assert_eq!(entry.ended_at, lost.ended_at, "the loss time is kept");
        assert_eq!(entry.recovered_at, None);
    }

    #[tokio::test]
    async fn resume_abandoned_marks_history_recovered() {
        let (hub, _scratch) = spawnless_test_hub("resume-marks-recovered");
        insert_abandoned_with_history(&hub, "lost");
        let (out_tx, _out_rx) = mpsc::unbounded_channel();
        finish_resume(&hub, "lost", "fresh", &out_tx);
        assert!(hub.sessions.get("lost").is_none(), "placeholder removed");
        let entry = history::read_one(&hub.dirs, "lost").expect("entry kept");
        assert!(entry.recovered_at.is_some(), "a resume counts as recovery");
        assert_eq!(entry.end, SessionEnd::TracerLost);
    }

    #[tokio::test]
    async fn abandoned_resume_request_returns_the_records_busy_flag() {
        let (hub, scratch) = spawnless_test_hub("resume-busy-excluded");
        insert_abandoned_with_history(&hub, "lost");
        let target = SpawnTarget::Standalone {
            cwd: Some(scratch.path().to_string_lossy().into_owned()),
            add_dirs: Vec::new(),
        };
        let stored = protocol::SpawnConfig::from_request(&env_spawn(
            target,
            SessionMode::Interactive,
            Vec::new(),
            None,
        ));
        hub.sessions.update("lost", |rec| {
            rec.spawn_config = Some(stored);
            rec.busy_excluded = true;
        });
        let rec = hub.sessions.get("lost").expect("lost is registered");
        let (_request, busy_excluded) =
            abandoned_resume_request(&crate::sync::lock(&rec)).expect("lost can resume");
        assert!(busy_excluded, "the resumed session keeps the flag");
    }

    #[tokio::test]
    async fn resume_abandoned_keeps_a_deepseek_provider() {
        let (hub, scratch) = spawnless_test_hub("resume-deepseek");
        insert_abandoned_with_history(&hub, "lost");
        let target = SpawnTarget::Standalone {
            cwd: Some(scratch.path().to_string_lossy().into_owned()),
            add_dirs: Vec::new(),
        };
        let stored = protocol::SpawnConfig::from_request(&deepseek_spawn(
            target,
            SessionMode::Interactive,
            Vec::new(),
            None,
        ));
        hub.sessions
            .update("lost", |rec| rec.spawn_config = Some(stored));
        let rec = hub.sessions.get("lost").expect("lost is registered");
        let (request, _) =
            abandoned_resume_request(&crate::sync::lock(&rec)).expect("lost can resume");
        assert_eq!(request.agent_options, deepseek_options());
    }

    /// `container`'s persisted session order.
    fn order_of(hub: &Hub, container: &str) -> Option<Vec<String>> {
        hub.state
            .with_persisted(|s| s.session_order.get(container).cloned())
    }

    /// `list` as owned ids.
    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|id| (*id).to_owned()).collect()
    }

    /// Record the session order `order` for `container`.
    fn seed_session_order(hub: &Hub, container: &str, order: &[&str]) {
        let order = ids(order);
        hub.state
            .mutate(|s| {
                s.session_order.insert(container.to_owned(), order);
            })
            .expect("seed the session order");
    }

    /// Give `id` the name and look the user chose for it.
    fn rename_and_recolour(hub: &Hub, id: &str) {
        hub.sessions.update(id, |rec| {
            rec.user_label = Some("mine".to_owned());
            rec.label = "mine".to_owned();
            rec.appearance.accent_color = Some("#38bdf8".to_owned());
        });
    }

    #[tokio::test]
    #[expect(clippy::panic, reason = "a wrong spawn fails the test loudly")]
    async fn resume_abandoned_continues_the_conversation() {
        let (_home_lock, home) = claude_home_scratch("resume-continues").await;
        let (hub, scratch) = spawnless_test_hub("resume-continues");
        let folder = scratch.path().to_string_lossy().into_owned();
        home.write_transcript(&folder, RECOVERY_CONVERSATION);
        insert_abandoned_claude(
            &hub,
            "lost",
            standalone(&folder),
            Some(RECOVERY_CONVERSATION),
        );
        hub.sessions.update("lost", |rec| {
            rec.busy_excluded = true;
            rec.last_prompt = Some("first".to_owned());
        });
        let spawned = Spawned::default();
        let (out_tx, _out_rx) = mpsc::unbounded_channel();

        resume_abandoned_with(&hub, "lost", &out_tx, recording_spawn(&spawned))
            .await
            .expect("the resume spawns");

        {
            let spawned = crate::sync::lock(&spawned);
            let [(request, busy_excluded)] = spawned.as_slice() else {
                panic!("expected one spawn, got {}", spawned.len());
            };
            assert_eq!(
                request.resume_conversation.as_deref(),
                Some(RECOVERY_CONVERSATION),
                "a manual Resume continues the session's own conversation"
            );
            assert!(
                request.initial_prompt.is_none(),
                "no prompt is replayed into a continued conversation"
            );
            assert!(*busy_excluded, "the resumed session keeps the flag");
        }
        assert!(hub.sessions.get("lost").is_none(), "placeholder removed");
        let entry = history::read_one(&hub.dirs, "lost").expect("entry kept");
        assert!(entry.recovered_at.is_some(), "a resume counts as recovery");
    }

    #[tokio::test]
    #[expect(clippy::panic, reason = "a wrong spawn fails the test loudly")]
    async fn resume_abandoned_without_a_transcript_replays_the_first_prompt() {
        let (_home_lock, _home) = claude_home_scratch("resume-no-transcript").await;
        let (hub, scratch) = spawnless_test_hub("resume-no-transcript");
        let folder = scratch.path().to_string_lossy().into_owned();
        insert_abandoned_claude(
            &hub,
            "lost",
            standalone(&folder),
            Some(RECOVERY_CONVERSATION),
        );
        hub.sessions
            .update("lost", |rec| rec.last_prompt = Some("first".to_owned()));
        let spawned = Spawned::default();
        let (out_tx, _out_rx) = mpsc::unbounded_channel();

        resume_abandoned_with(&hub, "lost", &out_tx, recording_spawn(&spawned))
            .await
            .expect("the resume still spawns");

        let spawned = crate::sync::lock(&spawned);
        let [(request, _)] = spawned.as_slice() else {
            panic!("expected one spawn, got {}", spawned.len());
        };
        assert_eq!(
            request.resume_conversation, None,
            "a conversation with no transcript is not continued"
        );
        assert_eq!(
            request.initial_prompt.as_deref(),
            Some("first"),
            "the first prompt is replayed, as it was before"
        );
    }

    #[tokio::test]
    #[expect(clippy::panic, reason = "a wrong spawn fails the test loudly")]
    async fn resume_abandoned_for_a_plain_shell_still_starts_fresh() {
        let (hub, scratch) = spawnless_test_hub("resume-plain-shell");
        let folder = scratch.path().to_string_lossy().into_owned();
        insert_abandoned_claude(&hub, "lost", standalone(&folder), None);
        hub.sessions.update("lost", |rec| {
            rec.mode = SessionMode::PlainShell;
            rec.last_prompt = Some("ls".to_owned());
        });
        let spawned = Spawned::default();
        let (out_tx, _out_rx) = mpsc::unbounded_channel();

        resume_abandoned_with(&hub, "lost", &out_tx, recording_spawn(&spawned))
            .await
            .expect("a plain shell resumes too");

        let spawned = crate::sync::lock(&spawned);
        let [(request, _)] = spawned.as_slice() else {
            panic!("expected one spawn, got {}", spawned.len());
        };
        assert_eq!(
            request.resume_conversation, None,
            "a shell has no conversation"
        );
        assert_eq!(request.initial_prompt.as_deref(), Some("ls"));
    }

    #[tokio::test]
    #[expect(clippy::panic, reason = "a wrong spawn fails the test loudly")]
    async fn resume_abandoned_with_its_worktree_deleted_starts_fresh() {
        let (_home_lock, home) = claude_home_scratch("resume-worktree-gone").await;
        let (hub, scratch, _repo) = env_ref_repo_hub("resume-worktree-gone");
        let folder = scratch
            .path()
            .join("worktree")
            .to_string_lossy()
            .into_owned();
        home.write_transcript(&folder, RECOVERY_CONVERSATION);
        insert_abandoned_claude(&hub, "lost", worktree_target(), Some(RECOVERY_CONVERSATION));
        set_member(&hub, "lost", "wt/x", &folder);
        hub.sessions
            .update("lost", |rec| rec.last_prompt = Some("first".to_owned()));
        let spawned = Spawned::default();
        let (out_tx, _out_rx) = mpsc::unbounded_channel();

        resume_abandoned_with(&hub, "lost", &out_tx, recording_spawn(&spawned))
            .await
            .expect("a Resume whose worktree is gone still spawns");

        let spawned = crate::sync::lock(&spawned);
        let [(request, _)] = spawned.as_slice() else {
            panic!("expected one spawn, got {}", spawned.len());
        };
        assert_eq!(
            request.resume_conversation, None,
            "a pinned folder that no longer exists is not pinned"
        );
        assert_eq!(
            request.initial_prompt.as_deref(),
            Some("first"),
            "the unpinned request, which recreates the worktree, as before"
        );
    }

    #[tokio::test]
    #[expect(clippy::panic, reason = "a wrong spawn fails the test loudly")]
    async fn resume_abandoned_whose_worktree_a_live_session_holds_uses_the_unpinned_request() {
        let (_home_lock, home) = claude_home_scratch("resume-worktree-in-use").await;
        let (hub, scratch, _repo) = env_ref_repo_hub("resume-worktree-in-use");
        let folder = scratch
            .path()
            .join("worktree")
            .to_string_lossy()
            .into_owned();
        std::fs::create_dir_all(&folder).expect("create the worktree folder");
        home.write_transcript(&folder, RECOVERY_CONVERSATION);
        insert_abandoned_claude(&hub, "lost", worktree_target(), Some(RECOVERY_CONVERSATION));
        set_member(&hub, "lost", "wt/x", &folder);
        hub.sessions
            .update("lost", |rec| rec.last_prompt = Some("first".to_owned()));
        hub.sessions.insert(history::test_support::record(
            "live",
            SessionMode::Interactive,
        ));
        set_member(&hub, "live", "wt/x", &folder);
        let spawned = Spawned::default();
        let (out_tx, _out_rx) = mpsc::unbounded_channel();

        resume_abandoned_with(&hub, "lost", &out_tx, recording_spawn(&spawned))
            .await
            .expect("a Resume of a held worktree still spawns");

        let spawned = crate::sync::lock(&spawned);
        let [(request, _)] = spawned.as_slice() else {
            panic!("expected one spawn, got {}", spawned.len());
        };
        assert_eq!(
            request.resume_conversation, None,
            "a worktree a live session holds is not pinned"
        );
        assert_eq!(
            request.initial_prompt.as_deref(),
            Some("first"),
            "the unpinned request, whose spawn refuses the held worktree, as before"
        );
    }

    #[tokio::test]
    async fn finish_resume_keeps_user_label_and_appearance() {
        let (hub, _scratch) = spawnless_test_hub("resume-keeps-identity");
        insert_abandoned_with_history(&hub, "lost");
        rename_and_recolour(&hub, "lost");
        hub.sessions.insert(idle_record("fresh"));
        let (out_tx, _out_rx) = mpsc::unbounded_channel();

        finish_resume(&hub, "lost", "fresh", &out_tx);

        let rec = hub.sessions.get("fresh").expect("fresh is registered");
        let rec = crate::sync::lock(&rec);
        assert_eq!(
            rec.user_label.as_deref(),
            Some("mine"),
            "the resume keeps the name the user gave it"
        );
        assert_eq!(rec.label, "mine");
        assert_eq!(
            rec.appearance.accent_color.as_deref(),
            Some("#38bdf8"),
            "and the look"
        );
    }

    #[tokio::test]
    async fn finish_resume_leaves_the_default_label_when_the_old_had_no_rename() {
        let (hub, _scratch) = spawnless_test_hub("resume-default-label");
        insert_abandoned_with_history(&hub, "lost");
        hub.sessions.insert(idle_record("fresh"));
        let (out_tx, _out_rx) = mpsc::unbounded_channel();

        finish_resume(&hub, "lost", "fresh", &out_tx);

        let rec = hub.sessions.get("fresh").expect("fresh is registered");
        let rec = crate::sync::lock(&rec);
        assert_eq!(rec.user_label, None, "there was no rename to keep");
        assert_eq!(
            rec.label, "fresh",
            "the resumed session's own default label"
        );
    }

    #[tokio::test]
    #[expect(clippy::panic, reason = "a wrong broadcast fails the test loudly")]
    async fn finish_resume_swaps_the_id_in_session_order() {
        let (hub, _scratch) = spawnless_test_hub("resume-keeps-place");
        insert_abandoned_with_history(&hub, "lost");
        seed_session_order(&hub, "r1", &["a", "lost", "b"]);
        seed_session_order(&hub, "r2", &["x", "y"]);
        let mut events = hub.state_events.subscribe();
        let (out_tx, _out_rx) = mpsc::unbounded_channel();

        finish_resume(&hub, "lost", "fresh", &out_tx);

        assert_eq!(order_of(&hub, "r1"), Some(ids(&["a", "fresh", "b"])));
        assert_eq!(order_of(&hub, "r2"), Some(ids(&["x", "y"])));
        let reordered: Vec<StateEvent> = std::iter::from_fn(|| events.try_recv().ok()).collect();
        let [
            StateEvent::SessionsReordered {
                container_id,
                ordered_ids,
            },
        ] = reordered.as_slice()
        else {
            panic!("expected one reorder, got {reordered:?}");
        };
        assert_eq!(container_id, "r1", "only the container that listed it");
        assert_eq!(ordered_ids, &["a", "fresh", "b"]);
    }

    /// The request `how` recovers `entry` with, resuming `conversation`,
    /// planned against an empty registry with every conversation present.
    fn planned_recovery(
        entry: &protocol::HistoryEntry,
        how: protocol::RecoverAs,
        conversation: Option<&str>,
    ) -> SpawnRequest {
        let item = protocol::RecoverItem {
            history_id: entry.session_id.clone(),
            conversation_id: conversation.map(str::to_owned),
            how,
        };
        let registered = history::Registered {
            repos: &[],
            workspaces: &[],
        };
        history::plan_recovery(entry, &item, &registered, |_, _, _| true)
            .expect("the recovery plans")
            .request
    }

    #[test]
    fn the_recovery_pre_check_sees_the_provider_that_will_spawn() {
        let folder = r"D:\deepseek-proj";
        let mut rec = history::test_support::record("ds", SessionMode::Interactive);
        rec.members = vec![SessionMember {
            repo_id: String::new(),
            repo_name: String::new(),
            branch: "main".to_owned(),
            worktree_path: folder.to_owned(),
        }];
        let anthropic = history::entry_from_record(&rec, SessionEnd::TracerLost, Utc::now());
        let mut recorded = anthropic.clone();
        recorded.spawn_config = Some(protocol::SpawnConfig::from_request(&deepseek_spawn(
            SpawnTarget::Standalone {
                cwd: Some(folder.to_owned()),
                add_dirs: Vec::new(),
            },
            SessionMode::Interactive,
            Vec::new(),
            None,
        )));
        let mut imported = anthropic.clone();
        imported.provider = protocol::ClaudeProvider::Deepseek;
        let register = protocol::RecoverAs::RegisterRepoThenClaude {
            path: folder.to_owned(),
        };
        for entry in [&recorded, &imported] {
            for how in [protocol::RecoverAs::Claude, register.clone()] {
                let request = planned_recovery(entry, how.clone(), Some(RECOVERY_CONVERSATION));
                let failure = check_spawn_request_with(&request, |_| None)
                    .expect_err("a DeepSeek recovery needs the key");
                assert_eq!(
                    failure.title, "Couldn't start the DeepSeek session",
                    "{how:?}"
                );
                assert!(
                    check_spawn_request_with(&request, deepseek_key).is_ok(),
                    "{how:?}"
                );
            }
        }
        // A shell recovery spawns a plain shell with no provider, so it needs
        // no key.
        for conversation in [Some(RECOVERY_CONVERSATION), None] {
            let shell = planned_recovery(&anthropic, protocol::RecoverAs::Shell, conversation);
            assert_eq!(shell.mode, SessionMode::PlainShell);
            assert!(
                check_spawn_request_with(&shell, |_| None).is_ok(),
                "{conversation:?}"
            );
        }
    }

    #[tokio::test]
    async fn the_end_path_writes_the_busy_flag_into_the_history_entry() {
        let (hub, _scratch) = spawnless_test_hub("recover-busy-excluded");
        let mut rec = history::test_support::record("s1", SessionMode::Interactive);
        rec.busy_excluded = true;
        hub.sessions.insert(rec);
        record_end(&hub, "s1", SessionEnd::TracerLost);
        let entry = history::test_support::wait_for_entry(&hub.dirs, "s1").await;
        assert!(
            entry.busy_excluded,
            "the end path writes the flag recovery reads"
        );
    }

    #[test]
    fn recovery_takes_the_busy_flag_from_a_still_abandoned_session_else_the_entry() {
        let mut entry = history::entry_from_record(
            &history::test_support::record("lost", SessionMode::Interactive),
            SessionEnd::TracerLost,
            Utc::now(),
        );
        entry.busy_excluded = true;
        let mut abandoned = history::test_support::record("lost", SessionMode::Interactive);
        abandoned.is_abandoned = true;
        assert!(
            !recovery_busy_excluded(Some(&abandoned), &entry),
            "a toggle on the abandoned leaf after the entry was written wins"
        );
        abandoned.busy_excluded = true;
        entry.busy_excluded = false;
        assert!(recovery_busy_excluded(Some(&abandoned), &entry));

        let mut live = history::test_support::record("lost", SessionMode::Interactive);
        live.busy_excluded = true;
        assert!(
            !recovery_busy_excluded(Some(&live), &entry),
            "a session that is not abandoned is not the one being recovered"
        );
        entry.busy_excluded = true;
        assert!(
            recovery_busy_excluded(None, &entry),
            "with no session left, the entry's flag"
        );
    }

    #[tokio::test]
    async fn recovering_abandoned_entry_rebinds_its_panes() {
        let (hub, _scratch) = spawnless_test_hub("recover-rebinds");
        insert_abandoned_with_history(&hub, "lost");
        seed_pane_layout(&hub, "lost");
        let (out_tx, _out_rx) = mpsc::unbounded_channel();
        finish_recovery(&hub, "lost", "fresh", &out_tx);
        assert_eq!(
            shown_panes(&hub),
            [Some("fresh".to_owned())],
            "the pane follows"
        );
    }

    /// Store a client layout whose one tab shows `session_id` in its pane.
    fn seed_pane_layout(hub: &Hub, session_id: &str) {
        let tab = crate::tabs::make_tab(None, Some(session_id.to_owned()), &[]);
        hub.state
            .mutate_all_layouts(|layouts| {
                layouts.insert(
                    "client".to_owned(),
                    crate::state::ClientLayout {
                        name: None,
                        tabs: vec![tab],
                    },
                );
            })
            .expect("seed a layout");
    }

    /// The session each tab of the layout [`seed_pane_layout`] stored shows.
    fn shown_panes(hub: &Hub) -> Vec<Option<String>> {
        hub.state
            .mutate_all_layouts(|layouts| {
                layouts["client"]
                    .tabs
                    .iter()
                    .filter_map(|tab| match &tab.content {
                        protocol::TabContent::Grid {
                            grid: protocol::GridNode::Pane { session_id, .. },
                        } => Some(session_id.clone()),
                        _ => None,
                    })
                    .collect()
            })
            .expect("read the layout")
    }

    /// What a test's stand-in for `spawn_session` was asked to start.
    type Spawned = Arc<Mutex<Vec<(SpawnRequest, bool)>>>;

    /// A stand-in for `spawn_session` that records each request in `spawned`
    /// and reports a new session `fresh`.
    fn recording_spawn(
        spawned: &Spawned,
    ) -> impl Fn(SpawnRequest, bool) -> std::future::Ready<anyhow::Result<String>> {
        let spawned = Arc::clone(spawned);
        move |req, busy_excluded| {
            crate::sync::lock(&spawned).push((req, busy_excluded));
            std::future::ready(Ok("fresh".to_owned()))
        }
    }

    /// A stand-in for `spawn_session` that records each request in `spawned`,
    /// registers the new session `fresh` as a real spawn does, so the
    /// resume's own updates reach it, and reports its id.
    fn recording_spawn_registering(
        hub: &Hub,
        spawned: &Spawned,
    ) -> impl Fn(SpawnRequest, bool) -> std::future::Ready<anyhow::Result<String>> {
        let spawned = Arc::clone(spawned);
        let hub = hub.clone();
        move |req, busy_excluded| {
            crate::sync::lock(&spawned).push((req, busy_excluded));
            hub.sessions.insert(idle_record("fresh"));
            std::future::ready(Ok("fresh".to_owned()))
        }
    }

    /// Auto-resume `ids` with [`recording_spawn`] recording into `spawned`.
    async fn auto_resume_recording(hub: &Hub, ids: &[&str], spawned: &Spawned) -> (usize, usize) {
        let ids = ids.iter().map(|id| (*id).to_owned()).collect();
        let in_flight = ResumeInFlight::start(&hub.resume_in_flight);
        auto_resume_with(hub, ids, recording_spawn(spawned), in_flight).await
    }

    /// An in-place target on `branch` in registered repo `r1`.
    fn in_place(branch: &str) -> SpawnTarget {
        SpawnTarget::Single {
            repo_id: "r1".to_owned(),
            branch_name: branch.to_owned(),
            base_branch: None,
            use_worktree: false,
            checkout_strategy: None,
            worktree_reuse: protocol::WorktreeReusePolicy::Reuse,
            existing_worktree: None,
        }
    }

    /// A worktree target on `wt/x` in repo `r1`.
    fn worktree_target() -> SpawnTarget {
        SpawnTarget::Single {
            repo_id: "r1".to_owned(),
            branch_name: "wt/x".to_owned(),
            base_branch: None,
            use_worktree: true,
            checkout_strategy: None,
            worktree_reuse: protocol::WorktreeReusePolicy::Reuse,
            existing_worktree: None,
        }
    }

    /// `id` ran as repo `r1`'s member on `branch` in `folder`.
    fn set_member(hub: &Hub, id: &str, branch: &str, folder: &str) {
        hub.sessions.update(id, |rec| {
            rec.members = vec![SessionMember {
                repo_id: "r1".to_owned(),
                repo_name: "r1".to_owned(),
                branch: branch.to_owned(),
                worktree_path: folder.to_owned(),
            }];
        });
    }

    #[tokio::test]
    #[expect(clippy::panic, reason = "a wrong spawn fails the test loudly")]
    async fn auto_resume_skips_in_place_target_on_another_branch() {
        let (_home_lock, home) = claude_home_scratch("auto-resume-in-place").await;
        let (hub, _scratch, repo) = env_ref_repo_hub("auto-resume-in-place");
        let folder = repo.to_string_lossy().into_owned();
        home.write_transcript(&folder, RECOVERY_CONVERSATION);
        insert_abandoned_claude(
            &hub,
            "elsewhere",
            in_place("feature"),
            Some(RECOVERY_CONVERSATION),
        );
        set_member(&hub, "elsewhere", "feature", &folder);
        insert_abandoned_claude(&hub, "here", in_place("main"), Some(RECOVERY_CONVERSATION));
        set_member(&hub, "here", "main", &folder);
        let spawned = Spawned::default();

        let counts = auto_resume_recording(&hub, &["elsewhere", "here"], &spawned).await;

        assert_eq!(counts, (1, 1));
        let spawned = crate::sync::lock(&spawned);
        let [(request, _)] = spawned.as_slice() else {
            panic!("expected one spawn, got {}", spawned.len());
        };
        assert!(
            matches!(&request.target, SpawnTarget::Single { branch_name, .. } if branch_name == "main"),
            "only the checkout already on its branch resumes: {:?}",
            request.target
        );
        assert!(is_abandoned(&hub, "elsewhere"), "no unattended checkout");
    }

    #[tokio::test]
    async fn auto_resume_skips_a_worktree_in_use() {
        let (_home_lock, home) = claude_home_scratch("auto-resume-in-use").await;
        let (hub, scratch, _repo) = env_ref_repo_hub("auto-resume-in-use");
        let folder = scratch
            .path()
            .join("worktree")
            .to_string_lossy()
            .into_owned();
        home.write_transcript(&folder, RECOVERY_CONVERSATION);
        insert_abandoned_claude(&hub, "lost", worktree_target(), Some(RECOVERY_CONVERSATION));
        set_member(&hub, "lost", "wt/x", &folder);
        hub.sessions.insert(history::test_support::record(
            "live",
            SessionMode::Interactive,
        ));
        set_member(&hub, "live", "wt/x", &folder);
        let spawned = Spawned::default();

        let counts = auto_resume_recording(&hub, &["lost"], &spawned).await;

        assert_eq!(counts, (0, 1));
        assert!(
            crate::sync::lock(&spawned).is_empty(),
            "nothing was spawned"
        );
        assert!(is_abandoned(&hub, "lost"), "it stays abandoned");
    }

    #[tokio::test]
    #[expect(clippy::panic, reason = "the stand-in spawn panics on purpose")]
    async fn resume_in_flight_clears_when_the_task_panics() {
        let (_home_lock, home) = claude_home_scratch("auto-resume-panics").await;
        let (hub, scratch) = spawnless_test_hub("auto-resume-panics");
        let folder = scratch.path().to_string_lossy().into_owned();
        home.write_transcript(&folder, RECOVERY_CONVERSATION);
        insert_abandoned_claude(
            &hub,
            "lost",
            standalone(&folder),
            Some(RECOVERY_CONVERSATION),
        );
        let in_flight = ResumeInFlight::start(&hub.resume_in_flight);
        let task_hub = hub.clone();
        let panicking = |_req: SpawnRequest,
                         _busy_excluded: bool|
         -> std::future::Ready<anyhow::Result<String>> {
            panic!("the spawn panics")
        };

        let joined = tokio::spawn(async move {
            auto_resume_with(&task_hub, vec!["lost".to_owned()], panicking, in_flight).await
        })
        .await;

        assert!(joined.is_err(), "the task panicked");
        assert!(
            !hub.resume_in_flight.load(Ordering::SeqCst),
            "the hold-off ends"
        );
    }

    #[tokio::test]
    async fn auto_resume_counts_a_vanished_record_as_neither() {
        let (hub, _scratch) = spawnless_test_hub("auto-resume-vanished");
        let spawned = Spawned::default();

        let counts = auto_resume_recording(&hub, &["gone"], &spawned).await;

        assert_eq!(counts, (0, 0), "a session already handled is not left");
        assert!(
            crate::sync::lock(&spawned).is_empty(),
            "nothing was spawned"
        );
    }

    #[tokio::test]
    async fn dismiss_refused_while_auto_resume_holds_the_claim() {
        let (hub, _scratch) = spawnless_test_hub("dismiss-claimed");
        insert_abandoned_with_history(&hub, "lost");
        let _held = hub.recovering.claim("lost").expect("auto-resume claims it");
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();

        let err = dismiss_abandoned(&hub, "lost", &out_tx).expect_err("the Dismiss is refused");

        assert_eq!(err.to_string(), "This session is being resumed.");
        assert!(is_abandoned(&hub, "lost"), "nothing was discarded");
        assert!(out_rx.try_recv().is_err(), "no removal was sent");
        let entry = history::read_one(&hub.dirs, "lost").expect("entry kept");
        assert_eq!(entry.end, SessionEnd::TracerLost, "no stop was recorded");
    }

    fn standalone(folder: &str) -> SpawnTarget {
        SpawnTarget::Standalone {
            cwd: Some(folder.to_owned()),
            add_dirs: Vec::new(),
        }
    }

    /// Insert `id` as an abandoned interactive Claude session started from
    /// `target`, with conversation `conversation` and an unrecovered
    /// `TracerLost` entry, as a reboot leaves it.
    fn insert_abandoned_claude(
        hub: &Hub,
        id: &str,
        target: SpawnTarget,
        conversation: Option<&str>,
    ) {
        insert_abandoned_with_history(hub, id);
        let stored = protocol::SpawnConfig::from_request(&env_spawn(
            target,
            SessionMode::Interactive,
            Vec::new(),
            None,
        ));
        hub.sessions.update(id, |rec| {
            rec.agent = Agent::Claude;
            rec.spawn_config = Some(stored);
            rec.claude_session_id = conversation.map(str::to_owned);
        });
    }

    fn is_abandoned(hub: &Hub, id: &str) -> bool {
        hub.sessions
            .get(id)
            .is_some_and(|rec| crate::sync::lock(&rec).is_abandoned)
    }

    #[tokio::test]
    #[expect(clippy::panic, reason = "a wrong spawn fails the test loudly")]
    async fn auto_resume_continues_conversation_and_rebinds_panes() {
        let (_home_lock, home) = claude_home_scratch("auto-resume-continues").await;
        let (hub, scratch) = spawnless_test_hub("auto-resume-continues");
        let folder = scratch.path().to_string_lossy().into_owned();
        home.write_transcript(&folder, RECOVERY_CONVERSATION);
        insert_abandoned_claude(
            &hub,
            "lost",
            standalone(&folder),
            Some(RECOVERY_CONVERSATION),
        );
        hub.sessions.update("lost", |rec| rec.busy_excluded = true);
        seed_pane_layout(&hub, "lost");
        let spawned = Spawned::default();

        let counts = auto_resume_recording(&hub, &["lost"], &spawned).await;

        assert_eq!(counts, (1, 0));
        {
            let spawned = crate::sync::lock(&spawned);
            let [(request, busy_excluded)] = spawned.as_slice() else {
                panic!("expected one spawn, got {}", spawned.len());
            };
            assert_eq!(
                request.resume_conversation.as_deref(),
                Some(RECOVERY_CONVERSATION),
                "the session's own conversation continues"
            );
            assert!(*busy_excluded, "the resumed session keeps the flag");
        }
        assert_eq!(
            shown_panes(&hub),
            [Some("fresh".to_owned())],
            "the pane follows"
        );
        assert!(hub.sessions.get("lost").is_none(), "placeholder removed");
        let entry = history::read_one(&hub.dirs, "lost").expect("entry kept");
        assert!(entry.recovered_at.is_some(), "marked recovered");
        assert_eq!(*crate::sync::lock(&hub.start_resume), Some((1, 0)));
        assert!(
            !hub.resume_in_flight.load(Ordering::SeqCst),
            "the hold-off ends"
        );
    }

    #[tokio::test]
    async fn the_resume_broadcasts_its_report_when_it_finishes() {
        let (_home_lock, home) = claude_home_scratch("resume-report-broadcast").await;
        let (hub, scratch) = spawnless_test_hub("resume-report-broadcast");
        let folder = scratch.path().to_string_lossy().into_owned();
        home.write_transcript(&folder, RECOVERY_CONVERSATION);
        insert_abandoned_claude(
            &hub,
            "lost",
            standalone(&folder),
            Some(RECOVERY_CONVERSATION),
        );
        let mut events = hub.state_events.subscribe();
        let spawned = Spawned::default();

        let counts = auto_resume_recording(&hub, &["lost"], &spawned).await;

        assert_eq!(counts, (1, 0));
        let reports: Vec<(u32, u32)> = std::iter::from_fn(|| events.try_recv().ok())
            .filter_map(|event| match event {
                StateEvent::ResumeReport {
                    resumed,
                    left_in_recover,
                } => Some((resumed, left_in_recover)),
                _ => None,
            })
            .collect();
        assert_eq!(
            reports,
            [(1, 0)],
            "a client connected while the resume ran hears how it went"
        );
    }

    #[tokio::test]
    async fn auto_resume_keeps_name_look_and_place() {
        let (_home_lock, home) = claude_home_scratch("auto-resume-keeps-identity").await;
        let (hub, scratch) = spawnless_test_hub("auto-resume-keeps-identity");
        let folder = scratch.path().to_string_lossy().into_owned();
        home.write_transcript(&folder, RECOVERY_CONVERSATION);
        insert_abandoned_claude(
            &hub,
            "lost",
            standalone(&folder),
            Some(RECOVERY_CONVERSATION),
        );
        rename_and_recolour(&hub, "lost");
        seed_session_order(&hub, "r1", &["a", "lost"]);
        let spawned = Spawned::default();
        let in_flight = ResumeInFlight::start(&hub.resume_in_flight);

        let counts = auto_resume_with(
            &hub,
            vec!["lost".to_owned()],
            recording_spawn_registering(&hub, &spawned),
            in_flight,
        )
        .await;

        assert_eq!(counts, (1, 0));
        let rec = hub.sessions.get("fresh").expect("fresh is registered");
        let rec = crate::sync::lock(&rec);
        assert_eq!(
            rec.user_label.as_deref(),
            Some("mine"),
            "auto-resume keeps the name the user gave it"
        );
        assert_eq!(rec.appearance.accent_color.as_deref(), Some("#38bdf8"));
        drop(rec);
        assert_eq!(
            order_of(&hub, "r1"),
            Some(ids(&["a", "fresh"])),
            "and its sidebar place"
        );
    }

    #[tokio::test]
    async fn auto_resume_skips_plain_shell_and_missing_transcript() {
        let (_home_lock, home) = claude_home_scratch("auto-resume-skips").await;
        let (hub, scratch) = spawnless_test_hub("auto-resume-skips");
        let folder = scratch.path().to_string_lossy().into_owned();
        home.write_transcript(&folder, RECOVERY_CONVERSATION);
        insert_abandoned_claude(
            &hub,
            "shell",
            standalone(&folder),
            Some(RECOVERY_CONVERSATION),
        );
        hub.sessions
            .update("shell", |rec| rec.mode = SessionMode::PlainShell);
        let unwritten = "11111111-2222-3333-4444-555555555555";
        insert_abandoned_claude(&hub, "fresh-claude", standalone(&folder), Some(unwritten));
        let spawned = Spawned::default();

        let counts = auto_resume_recording(&hub, &["shell", "fresh-claude"], &spawned).await;

        assert_eq!(counts, (0, 2));
        assert!(
            crate::sync::lock(&spawned).is_empty(),
            "nothing was spawned"
        );
        for id in ["shell", "fresh-claude"] {
            assert!(is_abandoned(&hub, id), "{id} stays abandoned");
            let entry = history::read_one(&hub.dirs, id).expect("entry kept");
            assert_eq!(entry.recovered_at, None, "{id} stays in Recover");
            assert!(
                hub.recovering.claim(id).is_some(),
                "{id}'s claim was released"
            );
        }
    }

    #[tokio::test]
    async fn auto_resume_skips_worktree_target_that_cannot_be_pinned() {
        let (_home_lock, home) = claude_home_scratch("auto-resume-unpinned").await;
        let (hub, scratch) = spawnless_test_hub("auto-resume-unpinned");
        let folder = scratch
            .path()
            .join("worktree")
            .to_string_lossy()
            .into_owned();
        home.write_transcript(&folder, RECOVERY_CONVERSATION);
        let unregistered_repo = SpawnTarget::Single {
            repo_id: "r1".to_owned(),
            branch_name: "wt/x".to_owned(),
            base_branch: None,
            use_worktree: true,
            checkout_strategy: None,
            worktree_reuse: protocol::WorktreeReusePolicy::Reuse,
            existing_worktree: None,
        };
        insert_abandoned_claude(&hub, "lost", unregistered_repo, Some(RECOVERY_CONVERSATION));
        hub.sessions.update("lost", |rec| {
            rec.members = vec![SessionMember {
                repo_id: "r1".to_owned(),
                repo_name: "r1".to_owned(),
                branch: "wt/x".to_owned(),
                worktree_path: folder.clone(),
            }];
        });
        let spawned = Spawned::default();

        let counts = auto_resume_recording(&hub, &["lost"], &spawned).await;

        assert_eq!(counts, (0, 1));
        assert!(
            crate::sync::lock(&spawned).is_empty(),
            "nothing was spawned"
        );
        assert!(is_abandoned(&hub, "lost"), "it stays abandoned");
    }

    #[tokio::test]
    async fn auto_resume_holds_claim_against_user_resume() {
        let (_home_lock, home) = claude_home_scratch("auto-resume-claimed").await;
        let (hub, scratch) = spawnless_test_hub("auto-resume-claimed");
        let folder = scratch.path().to_string_lossy().into_owned();
        home.write_transcript(&folder, RECOVERY_CONVERSATION);
        insert_abandoned_claude(
            &hub,
            "lost",
            standalone(&folder),
            Some(RECOVERY_CONVERSATION),
        );
        let held = hub
            .recovering
            .claim("lost")
            .expect("the user's Resume claims it");
        let spawned = Spawned::default();

        let counts = auto_resume_recording(&hub, &["lost"], &spawned).await;

        assert_eq!(counts, (0, 0), "the user's Resume owns it");
        assert!(
            crate::sync::lock(&spawned).is_empty(),
            "nothing was spawned"
        );
        assert!(is_abandoned(&hub, "lost"), "left to the user's Resume");
        drop(held);
    }

    #[tokio::test]
    #[expect(clippy::panic, reason = "a wrong reply fails the test loudly")]
    async fn concurrent_recover_and_resume_claim_once() {
        let claims = RecoveryClaims::default();
        let first = claims.claim("x").expect("the first claim wins");
        assert!(claims.claim("x").is_none(), "a second claim loses");
        assert!(claims.claim("y").is_some(), "claims are per id");
        drop(first);
        assert!(claims.claim("x").is_some(), "a dropped claim frees the id");

        let (hub, _scratch) = spawnless_test_hub("recover-claims");
        insert_abandoned_with_history(&hub, "lost");
        let _held = hub.recovering.claim("lost").expect("claim");
        let (out_tx, _out_rx) = mpsc::unbounded_channel();
        let resumed = resume_abandoned(&hub, "lost", &out_tx).await;
        let err = resumed.expect_err("a Resume while a recovery holds it fails");
        assert!(err.to_string().contains(ALREADY_RECOVERING), "{err}");
        let reply = dispatch_one(
            &hub,
            ClientMessage::RecoverSessions {
                request_id: None,
                items: vec![protocol::RecoverItem {
                    history_id: "lost".to_owned(),
                    conversation_id: None,
                    how: protocol::RecoverAs::Claude,
                }],
            },
        )
        .await;
        let DaemonMessage::RecoverResult { results, .. } = reply else {
            panic!("expected recover_result, got {reply:?}");
        };
        assert_eq!(results[0].error.as_deref(), Some(ALREADY_RECOVERING));
        assert!(hub.sessions.get("lost").is_some(), "nothing was spawned");
    }

    /// Write a history entry for `id` in `mode`, recovered or not.
    fn write_history(hub: &Hub, id: &str, mode: SessionMode, recovered: bool) {
        let mut entry = history::entry_from_record(
            &crate::history::test_support::record(id, mode),
            SessionEnd::TracerLost,
            chrono::Utc::now(),
        );
        entry.primary_cwd = Some(hub.dirs.config.to_string_lossy().into_owned());
        entry.recovered_at = recovered.then(chrono::Utc::now);
        history::write_if_absent(&hub.dirs, &entry).expect("write history entry");
    }

    #[tokio::test]
    #[expect(clippy::panic, reason = "a wrong reply fails the test loudly")]
    async fn list_session_history_replies_with_its_request_id() {
        let (hub, _scratch) = spawnless_test_hub("list-history");
        write_history(&hub, "shell", SessionMode::PlainShell, false);
        write_history(&hub, "headless", SessionMode::Headless, false);
        let reply = dispatch_one(
            &hub,
            ClientMessage::ListSessionHistory {
                request_id: Some("r1".to_owned()),
            },
        )
        .await;
        let DaemonMessage::SessionHistory { request_id, items } = reply else {
            panic!("expected session_history, got {reply:?}");
        };
        assert_eq!(request_id.as_deref(), Some("r1"));
        let ids: Vec<&str> = items.iter().map(|i| i.entry.session_id.as_str()).collect();
        assert_eq!(ids, ["shell"]);
    }

    #[tokio::test]
    #[expect(clippy::panic, reason = "a wrong reply fails the test loudly")]
    async fn recover_sessions_reports_each_failure_in_order() {
        let (hub, _scratch) = spawnless_test_hub("recover-errors");
        write_history(&hub, "done", SessionMode::Interactive, true);
        let items = ["missing", "done", "done"]
            .into_iter()
            .zip([
                protocol::RecoverAs::Claude,
                protocol::RecoverAs::Shell,
                protocol::RecoverAs::Unknown,
            ])
            .map(|(id, how)| protocol::RecoverItem {
                history_id: id.to_owned(),
                conversation_id: None,
                how,
            })
            .collect();
        let reply = dispatch_one(
            &hub,
            ClientMessage::RecoverSessions {
                request_id: Some("r2".to_owned()),
                items,
            },
        )
        .await;
        let DaemonMessage::RecoverResult {
            request_id,
            results,
        } = reply
        else {
            panic!("expected recover_result, got {reply:?}");
        };
        assert_eq!(request_id.as_deref(), Some("r2"));
        let outcome: Vec<(&str, Option<&str>, Option<&str>)> = results
            .iter()
            .map(|r| {
                (
                    r.history_id.as_str(),
                    r.session_id.as_deref(),
                    r.error.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            outcome,
            [
                ("missing", None, Some("no history entry missing")),
                ("done", None, Some("already recovered")),
                ("done", None, Some("already recovered")),
            ]
        );
        assert!(
            hub.sessions.snapshots().is_empty(),
            "a refused item spawns nothing"
        );
    }

    #[tokio::test]
    #[expect(clippy::panic, reason = "a wrong reply fails the test loudly")]
    async fn recovery_under_test_hub_cannot_spawn() {
        let (hub, _scratch) = spawnless_test_hub("recover-cannot-spawn");
        write_history(&hub, "shell", SessionMode::PlainShell, false);
        let reply = dispatch_one(
            &hub,
            ClientMessage::RecoverSessions {
                request_id: None,
                items: vec![protocol::RecoverItem {
                    history_id: "shell".to_owned(),
                    conversation_id: None,
                    how: protocol::RecoverAs::Shell,
                }],
            },
        )
        .await;
        let DaemonMessage::RecoverResult { results, .. } = reply else {
            panic!("expected recover_result, got {reply:?}");
        };
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].session_id, None);
        let error = results[0].error.as_deref().unwrap_or_default();
        assert!(error.contains("cache dir"), "{error}");
        assert!(hub.sessions.snapshots().is_empty(), "nothing was spawned");
        let entry = history::read_one(&hub.dirs, "shell").expect("entry kept");
        assert_eq!(entry.recovered_at, None, "a failed recovery is not marked");
    }

    /// The conversation the recovery-ordering entries record, with a transcript
    /// Claude reads as still holding it.
    const RECOVERY_CONVERSATION: &str = "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0";

    /// Serialises the tests that point `CLAUDE_CONFIG_DIR` at a scratch home:
    /// the variable is process-global and tests run on parallel threads.
    static CLAUDE_HOME_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// A scratch Claude home `CLAUDE_CONFIG_DIR` points at while the guard
    /// lives, so recovery's transcript check reads it and not the user's own.
    struct ScratchClaudeHome {
        path: PathBuf,
        _var: crate::env_secrets::test_support::EnvVarGuard,
    }

    impl ScratchClaudeHome {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!("rt-claude-home-{label}"));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create scratch claude home");
            // Callers hold CLAUDE_HOME_LOCK for the guard's lifetime.
            let var =
                crate::env_secrets::test_support::EnvVarGuard::set("CLAUDE_CONFIG_DIR", &path);
            Self { path, _var: var }
        }

        /// A transcript for `folder` and `id`, as Claude Code files one.
        fn write_transcript(&self, folder: &str, id: &str) {
            let dir = self
                .path
                .join("projects")
                .join(agent_claude::encode_project_dir(folder));
            std::fs::create_dir_all(&dir).expect("create the transcript folder");
            std::fs::write(dir.join(format!("{id}.jsonl")), "{}\n").expect("write the transcript");
        }
    }

    impl Drop for ScratchClaudeHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// The env lock and a scratch Claude home, for an async test.
    async fn claude_home_scratch(
        label: &str,
    ) -> (tokio::sync::MutexGuard<'static, ()>, ScratchClaudeHome) {
        let guard = CLAUDE_HOME_LOCK.lock().await;
        (guard, ScratchClaudeHome::new(label))
    }

    /// A history entry for an unregistered Claude session that ran in `folder`
    /// with `row` as its recorded env, which Claude still holds
    /// [`RECOVERY_CONVERSATION`] for there.
    fn unregistered_claude_entry(hub: &Hub, id: &str, folder: &str, row: (&str, String)) {
        let mut record = history::test_support::record(id, SessionMode::Interactive);
        record.agent = Agent::Claude;
        record.members = vec![SessionMember {
            repo_id: "r1".to_owned(),
            repo_name: "r1".to_owned(),
            branch: "main".to_owned(),
            worktree_path: folder.to_owned(),
        }];
        let (key, value) = row;
        let request = env_spawn(
            SpawnTarget::Standalone {
                cwd: Some(folder.to_owned()),
                add_dirs: Vec::new(),
            },
            SessionMode::Interactive,
            env(&[(key, &value)]),
            None,
        );
        record.spawn_config = Some(protocol::SpawnConfig::from_request(&request));
        let mut entry = history::entry_from_record(&record, SessionEnd::TracerLost, Utc::now());
        entry.primary_cwd = Some(folder.to_owned());
        history::write_if_absent(&hub.dirs, &entry).expect("write history entry");
    }

    /// Recover `id` as `RegisterRepoThenClaude` — the recovery that registers a
    /// repo — and return the item's error.
    #[expect(clippy::panic, reason = "a wrong reply fails the test loudly")]
    async fn recover_registering(hub: &Hub, id: &str, folder: &str) -> String {
        let reply = dispatch_one(
            hub,
            ClientMessage::RecoverSessions {
                request_id: None,
                items: vec![protocol::RecoverItem {
                    history_id: id.to_owned(),
                    conversation_id: Some(RECOVERY_CONVERSATION.to_owned()),
                    how: protocol::RecoverAs::RegisterRepoThenClaude {
                        path: folder.to_owned(),
                    },
                }],
            },
        )
        .await;
        let DaemonMessage::RecoverResult { results, .. } = reply else {
            panic!("expected recover_result, got {reply:?}");
        };
        results[0].error.clone().expect("the recovery failed")
    }

    /// The registration a refused recovery must not have made: nothing in the
    /// registry, no broadcast, and `state.json` byte-identical.
    fn assert_nothing_registered(
        hub: &Hub,
        before: Option<&[u8]>,
        events: &mut broadcast::Receiver<StateEvent>,
    ) {
        assert!(
            hub.state.with_persisted(|s| s.repos.is_empty()),
            "no repo was registered"
        );
        assert_eq!(
            std::fs::read(&hub.dirs.state_file).ok().as_deref(),
            before,
            "state.json is untouched"
        );
        assert!(
            matches!(
                events.try_recv(),
                Err(broadcast::error::TryRecvError::Empty)
            ),
            "no state event was broadcast"
        );
    }

    #[tokio::test]
    async fn recovering_with_a_missing_secret_registers_nothing() {
        let (_lock, _secrets_dir) =
            env_secrets::test_support::scratch_async("recover-missing-secret").await;
        let (_home_lock, home) = claude_home_scratch("recover-missing-secret").await;
        let (hub, scratch) = spawnless_test_hub("recover-missing-secret");
        let folder = scratch.path().join("repo");
        std::fs::create_dir_all(&folder).expect("create the folder");
        git_init(&folder);
        git_ok(&folder, &["commit", "--allow-empty", "-m", "seed"]);
        let folder = folder.to_string_lossy().into_owned();
        home.write_transcript(&folder, RECOVERY_CONVERSATION);
        unregistered_claude_entry(
            &hub,
            "gone",
            &folder,
            ("ANTHROPIC_API_KEY", secret_ref(MISSING_SECRET_ID)),
        );
        let before = std::fs::read(&hub.dirs.state_file).ok();
        let mut events = hub.state_events.subscribe();

        let error = recover_registering(&hub, "gone", &folder).await;
        assert!(error.contains("`ANTHROPIC_API_KEY`"), "{error}");
        assert!(error.contains("A saved secret is missing"), "{error}");
        assert_nothing_registered(&hub, before.as_deref(), &mut events);
    }

    #[tokio::test]
    async fn recovering_with_an_unset_env_reference_registers_nothing() {
        let (_lock, _secrets_dir) =
            env_secrets::test_support::scratch_async("recover-unset-env").await;
        let (_home_lock, home) = claude_home_scratch("recover-unset-env").await;
        let (hub, scratch) = spawnless_test_hub("recover-unset-env");
        let folder = scratch.path().join("repo");
        std::fs::create_dir_all(&folder).expect("create the folder");
        git_init(&folder);
        git_ok(&folder, &["commit", "--allow-empty", "-m", "seed"]);
        let folder = folder.to_string_lossy().into_owned();
        home.write_transcript(&folder, RECOVERY_CONVERSATION);
        unregistered_claude_entry(&hub, "gone", &folder, ("K", UNSET_REF.to_owned()));
        let before = std::fs::read(&hub.dirs.state_file).ok();
        let mut events = hub.state_events.subscribe();

        let error = recover_registering(&hub, "gone", &folder).await;
        assert!(error.contains("`RT_TEST_UNSET_ENV_REF_4C1D`"), "{error}");
        assert!(error.contains("isn't set"), "{error}");
        assert_nothing_registered(&hub, before.as_deref(), &mut events);
    }

    #[tokio::test]
    async fn recover_finds_the_conversation_under_the_entrys_own_claude_home() {
        // The daemon's CLAUDE_CONFIG_DIR points at an empty scratch home.
        let (_home_lock, _daemon_home) = claude_home_scratch("recover-row-home").await;
        let (hub, scratch) = spawnless_test_hub("recover-row-home");
        let folder = scratch.path().join("repo").to_string_lossy().into_owned();
        let row_home = scratch.path().join("row-home");
        let project = row_home
            .join("projects")
            .join(agent_claude::encode_project_dir(&folder));
        std::fs::create_dir_all(&project).expect("create the transcript folder");
        std::fs::write(
            project.join(format!("{RECOVERY_CONVERSATION}.jsonl")),
            "{}\n",
        )
        .expect("write the transcript");
        let mut record = history::test_support::record("gone", SessionMode::Interactive);
        record.agent = Agent::Claude;
        record.claude_session_id = Some(RECOVERY_CONVERSATION.to_owned());
        let request = env_spawn(
            SpawnTarget::Standalone {
                cwd: Some(folder.clone()),
                add_dirs: Vec::new(),
            },
            SessionMode::Interactive,
            env(&[("CLAUDE_CONFIG_DIR", &row_home.to_string_lossy())]),
            None,
        );
        record.spawn_config = Some(protocol::SpawnConfig::from_request(&request));
        let mut entry = history::entry_from_record(&record, SessionEnd::TracerLost, Utc::now());
        entry.primary_cwd = Some(folder.clone());
        history::write_if_absent(&hub.dirs, &entry).expect("write history entry");

        let items = session_history_items(&hub).await;

        let item = items
            .iter()
            .find(|item| item.entry.session_id == "gone")
            .expect("the entry is listed");
        let ids: Vec<&str> = item.candidates.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, [RECOVERY_CONVERSATION]);
    }
}
