//! Live sessions: listing, duplicating, renaming and restyling them, their input and scrollback, the per-connection session event stream, and stopping, parking and discarding them.
use super::connection::{ConnCtx, PtyLane};
use super::git::{blocked_discard_message, probe_locking_processes, retry_worktree_cleanup};
use super::recovery::{mark_history_recovered, record_end, recovery_conversation_exists};
use super::spawn::{remove_hook_settings, send_spawn_failure, spawn_session, target_uses_worktree};
use super::tabs::close_session_panes;
use super::{Hub, StateEvent};
use crate::branch_fate::{FateInput, SessionBase};
use crate::paths::Dirs;
use crate::registry::{set_repo_appearance, set_workspace_appearance};
use crate::session::{
    ScrollbackSnapshot, SessionEvent, SessionRegistry, UpdateOrigin, build_replay_snapshot,
    push_recent_action,
};
use crate::{branch_fate, branch_names, orphan};
use anyhow::{Context as _, anyhow};
use base64::Engine as _;
use protocol::{
    AppearanceOverrides, BranchCleanup, ClientMessage, DaemonMessage, MemberBranchFate, SessionEnd,
    SessionMember, SpawnRequest, SpawnTarget,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, warn};

/// Handle the session messages `dispatch` routes here.
#[expect(
    clippy::too_many_lines,
    reason = "one arm per routed message, moved verbatim from dispatch"
)]
pub(super) async fn handle(hub: &Hub, msg: ClientMessage, ctx: &ConnCtx<'_>) -> anyhow::Result<()> {
    let ConnCtx {
        out_tx,
        pty_lane,
        connection,
        ..
    } = *ctx;
    match msg {
        ClientMessage::ListSessions => {
            let _ = out_tx.send(DaemonMessage::Sessions {
                sessions: hub.sessions.snapshots(),
            });
        }
        ClientMessage::DuplicateSession {
            session_id,
            request_id,
            carry_busy_excluded,
            same_checkout,
        } => {
            let ask = DuplicateAsk {
                session_id: &session_id,
                request_id,
                carry_busy_excluded,
                same_checkout,
            };
            duplicate_session(hub, ask, connection, out_tx).await;
        }
        ClientMessage::GetSpawnConfig { session_id } => {
            let config = hub
                .sessions
                .get(&session_id)
                .and_then(|rec| crate::sync::lock(&rec).spawn_config.clone());
            let _ = out_tx.send(DaemonMessage::SpawnConfigReply { session_id, config });
        }
        ClientMessage::RenameSession { session_id, label } => {
            // Empty / whitespace-only labels clear the user override and
            // restore the daemon-generated default. The user sees their
            // typed value drop straight to None — no surprise reuse of an
            // accidental space-padded name.
            let normalized = label.and_then(|raw| {
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                }
            });
            // The registry update mirrors the new labels into the sidecar.
            hub.sessions.update(&session_id, |guard| {
                guard.user_label.clone_from(&normalized);
                guard.label = normalized
                    .clone()
                    .unwrap_or_else(|| guard.default_label.clone());
            });
        }
        ClientMessage::SetSessionBusyExcluded {
            session_id,
            excluded,
        } => {
            // The registry update mirrors the flag into the sidecar and
            // broadcasts the session; an unknown session is ignored, as a
            // rename of one is.
            hub.sessions.update(&session_id, |guard| {
                guard.busy_excluded = excluded;
            });
        }
        ClientMessage::SetRepoAppearance {
            repo_id,
            appearance,
        } => {
            let appearance = appearance.normalized()?;
            set_repo_appearance(&hub.state, &repo_id, appearance)?;
            let repos = hub.state.with_persisted(|s| s.repos.clone());
            let _ = hub.state_events.send(StateEvent::Repos(repos));
        }
        ClientMessage::SetWorkspaceAppearance {
            workspace_id,
            appearance,
        } => {
            let appearance = appearance.normalized()?;
            set_workspace_appearance(&hub.state, &workspace_id, appearance)?;
            let workspaces = hub.state.with_persisted(|s| s.workspaces.clone());
            let _ = hub.state_events.send(StateEvent::Workspaces(workspaces));
        }
        ClientMessage::SetSessionAppearance {
            session_id,
            appearance,
            request_id,
        } => {
            let change = AppearanceChangeRequest {
                session_id: &session_id,
                appearance: &appearance,
                request_id,
                connection,
            };
            set_session_appearance(&hub.sessions, change, out_tx);
        }
        ClientMessage::Detach { session_id } => {
            let mut forwarders = pty_lane.forwarders.lock().await;
            if let Some(handle) = forwarders.remove(&session_id) {
                handle.abort();
                // As in `reply_then_forward`: a forwarder mid-resync must not
                // queue its `Scrollback` after the detach, where a remount's
                // `LoadScrollback` reply could come before it. Only the
                // multi-threaded runtime can show the difference: on the
                // current-thread runtime the tests run on, `abort` already
                // stops an idle forwarder before it runs again.
                let _ = handle.await;
            }
        }
        ClientMessage::SendInput {
            session_id,
            data_b64,
            automatic,
        } => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data_b64.as_bytes())
                .context("decoding input b64")?;
            if let Some(rec) = hub.sessions.get(&session_id) {
                let (pty, notifier) = {
                    let guard = crate::sync::lock(&rec);
                    (guard.pty.clone(), guard.input_notifier.clone())
                };
                // The owner's key answers a pending hook prompt; the bytes are
                // forwarded outside the record lock.
                //
                // Bytes the terminal sent on its own (replies to the child's
                // queries, wheel reports) are not the owner answering, so they
                // leave the prompt and its pending input alone.
                if !automatic {
                    hub.sessions.clear_answered_prompt(&session_id);
                }
                if let Some(pty) = pty {
                    if let Some(tx) = notifier {
                        let _ = tx.send(bytes.len());
                    }
                    // A run of keys under way learns the owner typed under
                    // it; the owner never waits for the typing lane.
                    if !automatic {
                        pty.typing().owner_typed();
                    }
                    pty.write_input(bytes);
                }
            }
        }
        ClientMessage::Resize {
            session_id,
            cols,
            rows,
            claim,
        } => {
            if let Some(rec) = hub.sessions.get(&session_id) {
                // The gate, the applied size and both resizes happen under one
                // record lock, so two clients' resizes never interleave. The PTY
                // resize is a channel send, and the screen lock is a leaf
                // that is never held while waiting on the record.
                let mut guard = crate::sync::lock(&rec);
                // The client that sized the session may resize it unclaimed:
                // a relaunch keeps its id and attaches without a claim.
                let from_size_client = guard.size_client.as_deref() == Some(ctx.client_id);
                if claim || guard.applied_size.is_none() || from_size_client {
                    if guard.applied_size == Some((cols, rows)) {
                        // ConPTY answers every resize, an unchanged one
                        // included, with a full repaint of the screen.
                        tracing::debug!(
                            session_id = %session_id,
                            cols,
                            rows,
                            "resize skipped: the session is already at this size"
                        );
                        guard.size_client = Some(ctx.client_id.to_owned());
                    } else {
                        if let Some(pty) = &guard.pty {
                            pty.resize(cols, rows);
                        }
                        if let Some(screen) = &guard.screen {
                            crate::sync::lock(screen).resize(cols, rows);
                        }
                        // Only a size the PTY was sent is remembered: a record
                        // without one yet must still forward a later claim to
                        // the PTY once the spawn attaches it.
                        if guard.pty.is_some() {
                            guard.applied_size = Some((cols, rows));
                            guard.size_client = Some(ctx.client_id.to_owned());
                        }
                    }
                } else {
                    tracing::debug!(
                        session_id = %session_id,
                        cols,
                        rows,
                        "resize ignored: unclaimed and the session is already sized by another client"
                    );
                }
            }
        }
        ClientMessage::StopSession {
            session_id,
            cleanup: _,
        } => {
            stop_session(hub, &session_id).await?;
        }
        ClientMessage::ParkSession { session_id } => {
            park_session(hub, &session_id).await;
        }
        ClientMessage::DiscardSession {
            session_id,
            cleanup,
        } => {
            discard_session(hub, &session_id, &cleanup, out_tx).await;
        }
        ClientMessage::PreviewDiscard { session_id } => {
            preview_discard(hub, &session_id, out_tx).await;
        }
        ClientMessage::RetryWorktreeCleanup {
            session_id,
            kill_pids,
            targets,
        } => {
            retry_worktree_cleanup(hub, &session_id, &kill_pids, &targets, out_tx).await;
        }
        ClientMessage::LoadScrollback {
            session_id,
            request_id,
        } => {
            load_scrollback_and_attach_forwarder(hub, &session_id, request_id, out_tx, pty_lane)
                .await;
        }
        _ => warn!("sessions::handle: dispatch routed a message this module does not handle"),
    }
    Ok(())
}

/// Stop every active session (kill PTY / headless child, drop orphan sidecar)
/// and clear the registry. Worktrees are intentionally left in place — exit
/// is the wrong moment to ask the user about per-member cleanup.
pub(super) async fn shutdown_all(hub: &Hub) {
    let ids: Vec<String> = hub.sessions.snapshots().into_iter().map(|s| s.id).collect();
    info!(count = ids.len(), "shutdown_all: starting");
    // Recorded before the stops so the shutdown, not their `StoppedByUser`,
    // is what the history keeps.
    for id in &ids {
        record_end(hub, id, SessionEnd::DaemonShutdown);
    }
    for id in &ids {
        info!(session_id = %id, "shutdown_all: stopping session");
        if let Err(err) = stop_session(hub, id).await {
            warn!(session_id = %id, ?err, "stop_session during shutdown failed");
        }
        info!(session_id = %id, "shutdown_all: session stopped");
    }
    info!("shutdown_all: complete");
}

/// How long the `LoadScrollback` handler waits for `attach_lifecycle`'s task
/// to reply with the atomic snapshot before falling back to a direct
/// scrollback file read. In normal operation the reply arrives within a
/// single task tick; the timeout protects against a hung lifecycle task
/// (which would otherwise block this client forever).
const SCROLLBACK_SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(5);

/// Atomic `LoadScrollback`: pull the scrollback bytes AND the
/// live receiver that starts at the next un-persisted PTY chunk from
/// `attach_lifecycle`'s task, spawn a per-client forwarder using that receiver
/// (so future PTY bytes for this session reach this WS client), and finally
/// reply with the Scrollback frame. Doing all three in one handler closes the
/// window in which output emitted between `Attach` and the file read would be
/// dropped or duplicated.
///
/// Sessions without an active PTY (orphans, abandoned, headless) have
/// `scrollback_snapshot_req == None`; the helper falls back to a direct file
/// read in that case and does NOT spawn a forwarder.
async fn load_scrollback_and_attach_forwarder(
    hub: &Hub,
    session_id: &str,
    request_id: Option<String>,
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
    pty_lane: &Arc<PtyLane>,
) {
    match take_snapshot(&hub.sessions, session_id).await {
        Snapshot::Taken(snap) => {
            let fwd = Forwarding {
                out_tx: out_tx.clone(),
                lane: Arc::clone(pty_lane),
                sessions: Arc::clone(&hub.sessions),
                dirs: hub.dirs.clone(),
            };
            reply_then_forward(snap, session_id, request_id, &fwd).await;
        }
        // No live PTY (orphan/abandoned/headless), or the session exited.
        Snapshot::Exited => {
            let _ = out_tx.send(file_scrollback_reply(&hub.dirs, session_id, request_id));
        }
        Snapshot::TimedOut => {
            warn!(
                %session_id,
                "load_scrollback: timed out waiting for snapshot from lifecycle task"
            );
            let _ = out_tx.send(file_scrollback_reply(&hub.dirs, session_id, request_id));
        }
    }
}

/// What a session's lifecycle task answered a snapshot request with.
enum Snapshot {
    /// The session's atomic snapshot.
    Taken(ScrollbackSnapshot),
    /// No snapshot will come: the session has no live PTY (no request
    /// channel), or its lifecycle task dropped the request because the
    /// session exited. Its scrollback file is final.
    Exited,
    /// The lifecycle task did not answer within
    /// [`SCROLLBACK_SNAPSHOT_TIMEOUT`]; the session may still be live.
    TimedOut,
}

/// Ask the session's lifecycle task for its atomic snapshot.
async fn take_snapshot(sessions: &SessionRegistry, session_id: &str) -> Snapshot {
    let snap_tx = sessions
        .get(session_id)
        .and_then(|arc| crate::sync::lock(&arc).scrollback_snapshot_req.clone());
    let Some(snap_tx) = snap_tx else {
        return Snapshot::Exited;
    };
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    if snap_tx.send(reply_tx).is_err() {
        return Snapshot::Exited;
    }
    match tokio::time::timeout(SCROLLBACK_SNAPSHOT_TIMEOUT, reply_rx).await {
        Ok(Ok(snap)) => Snapshot::Taken(snap),
        Ok(Err(_recv_err)) => {
            // The normal path for a request queued behind the exit: the file
            // it falls back to is final by then.
            debug!(%session_id, "take_snapshot: lifecycle task dropped snapshot reply");
            Snapshot::Exited
        }
        Err(_elapsed) => Snapshot::TimedOut,
    }
}

/// A `Scrollback` read straight from the session's scrollback file, for a
/// session without a live snapshot. It still re-asserts the child's
/// bracketed-paste mode from the persisted sidecar: on the snapshot-timeout
/// path the session *is* live, and handing it an unprefixed replay re-opens
/// the stale-paste-flag bug.
fn file_scrollback_reply(
    dirs: &Dirs,
    session_id: &str,
    request_id: Option<String>,
) -> DaemonMessage {
    let bp_enabled = crate::termstate::load(dirs, session_id);
    let (data, truncated) = build_replay_snapshot(Some(dirs), session_id, bp_enabled);
    scrollback_reply(session_id, &data, truncated, request_id, false)
}

/// What a session's PTY forwarder needs beyond its live receiver: the
/// connection's queue and PTY lane, and the registry and dirs a resync reads.
#[derive(Clone)]
pub(super) struct Forwarding {
    pub(super) out_tx: mpsc::UnboundedSender<DaemonMessage>,
    pub(super) lane: Arc<PtyLane>,
    pub(super) sessions: Arc<SessionRegistry>,
    pub(super) dirs: Dirs,
}

/// Queues the snapshot's `Scrollback` reply, then starts the per-client
/// forwarder for its live receiver. The lifecycle task has been sending every
/// chunk after the snapshot into it, so output produced meanwhile waits in it;
/// and since `out_tx` is a FIFO, the reply reaches the client before any
/// `PtyOutput` from the new forwarder.
async fn reply_then_forward(
    snap: ScrollbackSnapshot,
    session_id: &str,
    request_id: Option<String>,
    fwd: &Forwarding,
) {
    // Replace any existing forwarder for this session (e.g. a previous
    // Terminal mount in the same WS session that didn't get a clean
    // Detach). Stop it before the reply, spawn the fresh one after.
    let mut forwarders = fwd.lane.forwarders.lock().await;
    if let Some(old) = forwarders.remove(session_id) {
        old.abort();
        // `abort` does not wait, and on the multi-threaded runtime the old
        // forwarder may be mid-send on another worker, or mid-resync;
        // awaiting it (it yields `Err(Cancelled)` once stopped) keeps its
        // `PtyOutput` or its resync's `Scrollback` from landing after the
        // reply. It never takes this map's lock, so waiting here is safe.
        let _ = old.await;
    }
    let _ = fwd.out_tx.send(scrollback_reply(
        session_id,
        &snap.data,
        snap.truncated,
        request_id,
        true,
    ));
    let handle = tokio::spawn(forward(snap.live, session_id.to_string(), fwd.clone()));
    forwarders.insert(session_id.to_string(), handle);
}

/// A session's PTY forwarder on one connection: queues each chunk from `live`
/// as base64 `PtyOutput`, charged to the session in the connection's PTY lane.
/// A chunk that would take the session past
/// [`PTY_SESSION_BUDGET`](super::connection::PTY_SESSION_BUDGET) is dropped
/// with `live`, which stops the lifecycle task feeding this client (the
/// scrollback file still gets every chunk), and the forwarder resyncs the
/// client in this same task ([`resync`]), going on from the fresh snapshot's
/// receiver. Ends when `live` closes, the connection's queue closes, or a
/// resync leaves nothing to forward.
async fn forward(mut live: mpsc::UnboundedReceiver<Vec<u8>>, session_id: String, fwd: Forwarding) {
    loop {
        let Some(bytes) = live.recv().await else {
            return;
        };
        let data_b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        if fwd.lane.try_charge(&session_id, data_b64.len()) {
            let output = DaemonMessage::PtyOutput {
                session_id: session_id.clone(),
                data_b64,
            };
            if fwd.out_tx.send(output).is_err() {
                return;
            }
            continue;
        }
        drop(live);
        info!(
            %session_id,
            "PTY output is over the session's queue budget; resyncing the client"
        );
        let Some(fresh) = resync(&session_id, &fwd).await else {
            return;
        };
        live = fresh;
    }
}

/// Resync a client whose session went over its PTY budget, once the send
/// pump has taken everything the session queued: queue a fresh snapshot's
/// `Scrollback` (no request id, `forwarder_restarted`) and hand back its live
/// receiver. A session that exited gets its file's scrollback, not
/// restarted, and a session gone from the registry gets nothing; both hand
/// back `None`. A snapshot that timed out while the session still exists is
/// asked for again once the session's queue has drained again, for as long
/// as it takes.
async fn resync(session_id: &str, fwd: &Forwarding) -> Option<mpsc::UnboundedReceiver<Vec<u8>>> {
    loop {
        fwd.lane.drained(session_id).await;
        if fwd.sessions.get(session_id).is_none() {
            debug!(%session_id, "resync: the session is gone; nothing to send");
            return None;
        }
        match take_snapshot(&fwd.sessions, session_id).await {
            Snapshot::Taken(snap) => {
                let _ = fwd.out_tx.send(scrollback_reply(
                    session_id,
                    &snap.data,
                    snap.truncated,
                    None,
                    true,
                ));
                return Some(snap.live);
            }
            Snapshot::Exited => {
                let _ = fwd
                    .out_tx
                    .send(file_scrollback_reply(&fwd.dirs, session_id, None));
                return None;
            }
            Snapshot::TimedOut => debug!(
                %session_id,
                "resync: the snapshot timed out; asking again once the session's queue drains"
            ),
        }
    }
}

/// One `Scrollback` reply, echoing the request's id.
fn scrollback_reply(
    session_id: &str,
    data: &[u8],
    truncated: bool,
    request_id: Option<String>,
    forwarder_restarted: bool,
) -> DaemonMessage {
    DaemonMessage::Scrollback {
        session_id: session_id.to_string(),
        data_b64: base64::engine::general_purpose::STANDARD.encode(data),
        truncated,
        request_id,
        forwarder_restarted,
    }
}

/// A [`ClientMessage::DuplicateSession`], as the daemon handles it.
pub(super) struct DuplicateAsk<'a> {
    pub(super) session_id: &'a str,
    pub(super) request_id: Option<String>,
    pub(super) carry_busy_excluded: bool,
    pub(super) same_checkout: bool,
}

/// Spawns a copy of the asked session from its stored config, carrying its
/// busy exclusion when asked to, in the source's own checkout when asked
/// to. The copy's first broadcast carries
/// `request_id` on connection `connection`; a failure answers the requester
/// directly, carrying it too.
pub(super) async fn duplicate_session(
    hub: &Hub,
    ask: DuplicateAsk<'_>,
    connection: u64,
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
) {
    let DuplicateAsk {
        session_id,
        request_id,
        carry_busy_excluded,
        same_checkout,
    } = ask;
    let busy_excluded = carried_busy_excluded(&hub.sessions, session_id, carry_busy_excluded);
    let req = match duplicate_request(hub, session_id, same_checkout).await {
        Ok(req) => req,
        Err(err) => {
            let _ = out_tx.send(DaemonMessage::Error {
                message: err.to_string(),
                request_id,
                code: Some(protocol::error_codes::SPAWN_REFUSED.to_owned()),
            });
            return;
        }
    };
    let origin = request_origin(connection, request_id.clone());
    let resumed = req.resume_conversation.is_some();
    match spawn_session(hub, req, origin, busy_excluded).await {
        Ok(_) => finish_rerun(hub, session_id, resumed),
        Err(err) => send_spawn_failure(out_tx, &err, request_id.as_deref()),
    }
}

/// The busy exclusion a copy of `source_id` starts with: the source's when
/// `carry` is set (a Restart or a Resume), else none (a Duplicate). A source
/// the registry no longer holds has nothing to carry.
fn carried_busy_excluded(sessions: &SessionRegistry, source_id: &str, carry: bool) -> bool {
    carry
        && sessions
            .get(source_id)
            .is_some_and(|rec| crate::sync::lock(&rec).busy_excluded)
}

/// The origin of a registry update made by connection `connection`'s
/// request `request_id`; `None` when the request carried no id, since
/// there is then nothing to echo.
pub(super) fn request_origin(connection: u64, request_id: Option<String>) -> Option<UpdateOrigin> {
    request_id.map(|request_id| UpdateOrigin {
        connection,
        request_id,
    })
}

/// The request that duplicates `session_id`. A worktree duplicate runs on
/// its own branch: the source still holds its checkout, and a name the
/// daemon picks can't attach a leftover the user was never shown. With
/// `same_checkout` (a Restart or a Resume, whose source no longer holds the
/// checkout) it reruns on the source's branch, reusing its worktree, and
/// continues the source's agent conversation while its transcript exists.
async fn duplicate_request(
    hub: &Hub,
    session_id: &str,
    same_checkout: bool,
) -> anyhow::Result<SpawnRequest> {
    if same_checkout {
        return rerun_request(hub, session_id)
            .await
            .map(|rerun| rerun.request);
    }
    let (stored, _) = source_config(hub, session_id)?;
    let fresh = match stored.target.suggest_target() {
        Some(target) if target_uses_worktree(&stored.target) => {
            Some(branch_names::suggest(&hub.state, &hub.state.worktrees_dir(), &target).await?)
        }
        _ => None,
    };
    info!(source = %session_id, ?fresh, "duplicate_session: spawning");
    Ok(stored.to_duplicate_request(fresh))
}

/// `session_id`'s stored spawn config and its recorded members.
fn source_config(
    hub: &Hub,
    session_id: &str,
) -> anyhow::Result<(protocol::SpawnConfig, Vec<SessionMember>)> {
    let (stored, members) = hub
        .sessions
        .get(session_id)
        .map(|rec| {
            let rec = crate::sync::lock(&rec);
            (rec.spawn_config.clone(), rec.members.clone())
        })
        .ok_or_else(|| anyhow!("unknown session: {session_id}"))?;
    let stored = stored.ok_or_else(|| {
        anyhow!("session {session_id} has no stored spawn config; cannot duplicate")
    })?;
    Ok((stored, members))
}

/// The request for a Restart or a Resume of `session_id`, once its child has
/// exited: its config with every worktree member pinned to the folder it ran
/// in, as Recover pins them, so the rerun binds the very worktree it left
/// wherever it sits; and its conversation continued while its agent still
/// holds it for that pinned folder, looked up with the source's own rows.
pub(super) async fn rerun_request(hub: &Hub, session_id: &str) -> anyhow::Result<Rerun> {
    wait_for_source_exit(hub, session_id).await?;
    let (stored, members) = source_config(hub, session_id)?;
    let stored_env = stored.extra_env.clone();
    let conversation = hub
        .sessions
        .get(session_id)
        .and_then(|rec| resumable_conversation(&crate::sync::lock(&rec)));
    let mut req = stored.to_duplicate_request(None);
    let mut unpinned = false;
    if target_uses_worktree(&req.target) {
        let (repos, workspaces) = hub
            .state
            .with_persisted(|s| (s.repos.clone(), s.workspaces.clone()));
        let registered = crate::history::Registered {
            repos: &repos,
            workspaces: &workspaces,
        };
        match crate::history::pin_recorded(&req.target, &members, &registered) {
            Some(pinned) => req.target = pinned,
            None => unpinned = true,
        }
    }
    req.resume_conversation = match conversation {
        Some(c) => {
            // The lookup reads the disk, as Recover's planning does.
            tokio::task::spawn_blocking(move || {
                let rows = crate::conversations::lookup_rows(&stored_env);
                recovery_conversation_exists(&c.agent, &c.folder, &c.id, &rows).then_some(c.id)
            })
            .await?
        }
        None => None,
    };
    info!(
        source = %session_id,
        resume = ?req.resume_conversation,
        "duplicate_session: rerunning in the source's checkout"
    );
    Ok(Rerun {
        request: req,
        unpinned,
    })
}

/// A rerun's request, and whether its worktree target kept the recorded
/// config's unpinned form because some member could not be pinned to the
/// folder it ran in.
pub(super) struct Rerun {
    pub(super) request: SpawnRequest,
    pub(super) unpinned: bool,
}

/// How long a rerun or a worktree removal gives a just-killed child to exit:
/// a Stop's kill is fire-and-forget and the actual `TerminateProcess` hop
/// runs through the tracer pipe asynchronously.
const KILL_GRACE: Duration = Duration::from_millis(250);

/// Wait up to [`KILL_GRACE`] for `session_id`'s child to have exited, so a
/// rerun never binds the worktree (and resumes the conversation) while the
/// old child may still write to it. A Stop marks the session stopped before
/// its kill lands, so the status alone doesn't say the child is gone.
async fn wait_for_source_exit(hub: &Hub, session_id: &str) -> anyhow::Result<()> {
    let mut events = hub.sessions.subscribe();
    let exited = || {
        hub.sessions
            .get(session_id)
            .is_none_or(|rec| child_gone(&crate::sync::lock(&rec)))
    };
    let wait = async {
        while !exited() {
            match events.recv().await {
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return false,
            }
        }
        true
    };
    if tokio::time::timeout(KILL_GRACE, wait)
        .await
        .unwrap_or(false)
    {
        return Ok(());
    }
    info!(source = %session_id, "duplicate_session: the source's child is still exiting");
    Err(anyhow!(
        "The session is still stopping; try Restart again in a moment."
    ))
}

/// Whether `rec`'s PTY child has exited: it holds no pty, and its exit
/// watcher has run (it drops the scrollback request, which a Stop leaves).
/// A headless record keeps no such trace, so it never waits.
fn child_gone(rec: &crate::session::SessionRecord) -> bool {
    rec.pty.is_none() && rec.scrollback_snapshot_req.is_none()
}

/// After a rerun of `source_id` spawned: one that continues the source's
/// conversation marks its history entry recovered, as Recover does, so
/// Recover never resumes that conversation a second time.
fn finish_rerun(hub: &Hub, source_id: &str, resumed: bool) {
    if resumed {
        mark_history_recovered(hub, source_id);
    }
}

/// The agent conversation a rerun of a session could continue.
struct SourceConversation {
    agent: protocol::AgentId,
    folder: String,
    id: String,
}

/// `rec`'s conversation: an interactive session's agent conversation id, if
/// it has one, in its launch folder (its first member's worktree, which a
/// rerun pins, else its standalone folder), where the agent files the
/// transcript.
fn resumable_conversation(rec: &crate::session::SessionRecord) -> Option<SourceConversation> {
    if rec.mode != protocol::SessionMode::Interactive {
        return None;
    }
    let id = rec.conversation_id.clone()?;
    let folder = crate::history::primary_cwd(rec).filter(|folder| !folder.is_empty())?;
    Some(SourceConversation {
        agent: rec.agent.clone(),
        folder,
        id,
    })
}

/// What connection `connection` receives for a registry event. A
/// `SessionUpdated` carries the `request_id` of the request that made it
/// only on the connection that sent that request.
fn session_event_message(event: SessionEvent, connection: u64) -> DaemonMessage {
    match event {
        SessionEvent::Updated(snap, origin) => DaemonMessage::SessionUpdated {
            session: *snap,
            request_id: origin
                .filter(|origin| origin.connection == connection)
                .map(|origin| origin.request_id),
        },
        SessionEvent::Removed(session_id) => DaemonMessage::SessionRemoved { session_id },
        SessionEvent::Attention { session_id, reason } => {
            DaemonMessage::Attention { session_id, reason }
        }
        SessionEvent::AgentExited { session_id } => DaemonMessage::AgentExited { session_id },
        SessionEvent::Notice { session_id, notice } => {
            DaemonMessage::SessionNotice { session_id, notice }
        }
        SessionEvent::WaitingAlert {
            session_id,
            kind,
            notification,
            source,
        } => DaemonMessage::WaitingAlert {
            session_id,
            kind,
            notification,
            source,
        },
    }
}

/// A [`ClientMessage::SetSessionAppearance`], as the daemon handles it.
struct AppearanceChangeRequest<'a> {
    session_id: &'a str,
    appearance: &'a AppearanceOverrides,
    request_id: Option<String>,
    /// The connection that sent the request.
    connection: u64,
}

/// Stores a session's appearance. Every client sees the change through the
/// registry's one `SessionUpdated` broadcast, in order with every other
/// update; on the requester's connection it carries `request_id`. A refused
/// appearance, or a session the registry does not hold, answers the
/// requester with an `Error` carrying it.
fn set_session_appearance(
    sessions: &SessionRegistry,
    change: AppearanceChangeRequest<'_>,
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
) {
    let AppearanceChangeRequest {
        session_id,
        appearance,
        request_id,
        connection,
    } = change;
    let appearance = match appearance.normalized() {
        Ok(appearance) => appearance,
        Err(err) => {
            let _ = out_tx.send(DaemonMessage::Error {
                message: err.to_string(),
                request_id,
                code: Some(protocol::error_codes::INVALID_REQUEST.to_owned()),
            });
            return;
        }
    };
    let origin = request_origin(connection, request_id.clone());
    // The registry update mirrors the new appearance into the sidecar.
    let found = sessions.update_from(session_id, origin, |guard| {
        guard.appearance.clone_from(&appearance);
    });
    if !found {
        let _ = out_tx.send(DaemonMessage::Error {
            message: format!("unknown session: {session_id}"),
            request_id,
            code: Some(protocol::error_codes::NOT_FOUND.to_owned()),
        });
    }
}

/// Forwards the registry's session events to connection `connection`, in
/// the order the registry sent them. A connection that falls behind the
/// broadcast buffer gets the whole `Sessions` list in place of the events it
/// missed, and the events after it.
pub(super) fn spawn_session_forwarder(
    registry: Arc<SessionRegistry>,
    out_tx: mpsc::UnboundedSender<DaemonMessage>,
    connection: u64,
) -> tokio::task::JoinHandle<()> {
    // Subscribed before the task starts, so no event sent after this call
    // is missed.
    let mut rx = registry.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let _ = out_tx.send(session_event_message(event, connection));
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(
                        lagged = n,
                        connection, "client event stream lagged; resending the session list"
                    );
                    // Subscribe before taking the list: the stale buffered
                    // events go with the old receiver, and every event after
                    // the list applies on top of it.
                    rx = registry.subscribe();
                    for msg in resync_messages(&registry, connection) {
                        let _ = out_tx.send(msg);
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

/// What a connection that lagged past the broadcast buffer gets: the whole
/// `Sessions` list, then each listed session's spawn reply once more for the
/// sessions this connection's own requests spawned, since the list carries
/// no `request_id` and the reply may have been among the events it missed.
fn resync_messages(registry: &SessionRegistry, connection: u64) -> Vec<DaemonMessage> {
    let listed = registry.snapshots_with_spawn_origins();
    let replies: Vec<DaemonMessage> = listed
        .iter()
        .filter_map(|(session, origin)| {
            let origin = origin.as_ref().filter(|o| o.connection == connection)?;
            Some(DaemonMessage::SessionUpdated {
                session: session.clone(),
                request_id: Some(origin.request_id.clone()),
            })
        })
        .collect();
    let sessions = listed.into_iter().map(|(session, _)| session).collect();
    std::iter::once(DaemonMessage::Sessions { sessions })
        .chain(replies)
        .collect()
}

/// Kill the session's process if still alive, then mark it `is_inactive =
/// true` in the registry so the sidebar shows it as parked. The session
/// record stays in the registry — the user can Resume it later.
async fn park_session(hub: &Hub, session_id: &str) {
    if hub.sessions.get(session_id).is_none() {
        return;
    }
    // Recorded before the kill, so this wins over the exit watcher's later
    // `Exited` for the same session.
    record_end(hub, session_id, SessionEnd::StoppedByUser);
    // The handles are taken in the same update that marks the session
    // parked, before the kill, as `stop_session` does: the exit watcher then
    // either ran first (and took the pty itself) or finds it released, and
    // never reads a tracer lost during the kill as an unexpected loss.
    let mut pty = None;
    let mut headless_handle = None;
    hub.sessions.update(session_id, |r| {
        r.is_inactive = true;
        // A parked session has nothing left waiting on it.
        r.pending_input = None;
        r.background = false;
        pty = r.pty.take();
        headless_handle = r.headless.take();
        r.input_notifier = None;
        if !matches!(
            r.status,
            protocol::SessionStatus::Stopped | protocol::SessionStatus::Error
        ) {
            r.status = protocol::SessionStatus::Stopped;
        }
        push_recent_action(r, "parked — worktree retained".to_string());
    });
    if let Some(pty) = pty {
        pty.kill();
    }
    if let Some(h) = headless_handle {
        h.kill().await;
    }
    // Auto-close panes bound to this session in every client's layout; the
    // session record itself lives on in the registry for the Resume button.
    close_session_panes(hub, session_id);
}

/// Remove a stopped or inactive session from the registry. Optionally removes
/// per-member worktrees from disk. Does not kill a running session.
///
/// On cleanup failure (member dir still on disk after git remove + retry +
/// fs fallback), emits [`DaemonMessage::WorktreeCleanupFailed`] back to the
/// requesting client with the list of processes holding the dir open so the
/// user can decide whether to kill them and retry.
/// Snapshot the discard-relevant state of a session record: its members,
/// whether it owns per-session worktrees, and its label for failure
/// messages. Missing sessions fall back to empty/id defaults so a repeated
/// discard stays a no-op instead of an error.
fn discard_snapshot(hub: &Hub, session_id: &str) -> (Vec<SessionMember>, bool, String) {
    hub.sessions.get(session_id).map_or_else(
        || (Vec::new(), false, session_id.to_string()),
        |rec| {
            let guard = crate::sync::lock(&rec);
            let spawned_with_worktree =
                guard
                    .spawn_config
                    .as_ref()
                    .is_some_and(|cfg| match &cfg.target {
                        SpawnTarget::Single { use_worktree, .. }
                        | SpawnTarget::Workspace { use_worktree, .. } => *use_worktree,
                        SpawnTarget::Standalone { .. } => false,
                    });
            (
                guard.members.clone(),
                spawned_with_worktree || !guard.worktree_paths.is_empty(),
                guard.label.clone(),
            )
        },
    )
}

/// Reap the branch a removed session worktree leaves behind, per the
/// caller's [`BranchCleanup`] choice.
///
/// `git worktree remove` never deletes the branch it had checked out, and
/// left alone that branch pins a future spawn of the same name to this
/// session's old fork point. `Keep` leaves it regardless; `Delete` removes it
/// unconditionally; `Auto` (and any mode this build does not know) deletes it
/// only when its commits already exist in one of `targets`, by ancestry or by
/// patch equivalence. Failure is logged, never raised — the worktree the
/// user asked to delete is already gone.
async fn reap_discarded_session_branch(
    repo_path: &str,
    branch: &str,
    mode: BranchCleanup,
    targets: &[String],
) {
    match mode {
        BranchCleanup::Keep => {
            info!(
                branch,
                repo = %repo_path,
                "discard_session: keeping the session branch at the user's request"
            );
            return;
        }
        BranchCleanup::Delete => {
            force_delete_session_branch(repo_path, branch, "at the user's request").await;
            return;
        }
        BranchCleanup::Auto | BranchCleanup::Unknown => {}
    }

    match crate::git::branch_merge_status(Path::new(repo_path), branch, targets).await {
        Ok(crate::git::BranchMergeStatus::Merged { into, via }) => {
            let reason = format!("already in {into} ({via:?})");
            force_delete_session_branch(repo_path, branch, &reason).await;
        }
        Ok(crate::git::BranchMergeStatus::Unmerged { unique_commits }) => info!(
            branch,
            repo = %repo_path,
            unique_commits,
            ?targets,
            "discard_session: kept the session branch — it has unmerged commits"
        ),
        Err(err) => warn!(
            ?err,
            branch,
            repo = %repo_path,
            ?targets,
            "discard_session: merge check failed; branch left in place"
        ),
    }
}

/// `git branch -D` with the outcome logged either way. `why` is folded into
/// the success line so the log says what licensed the delete.
async fn force_delete_session_branch(repo_path: &str, branch: &str, why: &str) {
    match crate::git::delete_branch(Path::new(repo_path), branch).await {
        Ok(()) => info!(
            branch,
            repo = %repo_path,
            why,
            "discard_session: deleted the session branch"
        ),
        Err(err) => warn!(
            ?err,
            branch,
            repo = %repo_path,
            "discard_session: session branch delete failed; branch left in place"
        ),
    }
}

/// Answer [`ClientMessage::PreviewDiscard`]: what would happen to each
/// member's branch if the user confirmed the discard right now. Read-only.
/// An unknown session, or one with no per-session worktrees, yields an empty
/// member list — the same no-op stance a repeat discard takes.
async fn preview_discard(
    hub: &Hub,
    session_id: &str,
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
) {
    let (members, has_per_session_worktree, _) = discard_snapshot(hub, session_id);
    let SessionBase::Repo(explicit_base) = session_spawn_base(hub, session_id) else {
        info!(
            session_id,
            "preview_discard: standalone session has no branches"
        );
        let _ = out_tx.send(DaemonMessage::DiscardPreview {
            session_id: session_id.to_string(),
            members: Vec::new(),
        });
        return;
    };
    if members.is_empty() {
        info!(session_id, "preview_discard: no members to preview");
    }

    let worktrees_root = hub.state.worktrees_dir();
    let mut fates: Vec<MemberBranchFate> = Vec::new();
    for member in &members {
        let fate = if has_per_session_worktree {
            member_fate(hub, member, explicit_base.as_deref(), &worktrees_root).await
        } else {
            // An in-place session runs in the repo itself; its branch is not
            // the daemon's to reap.
            protocol::BranchFate::Untouched {
                reason: protocol::UntouchedReason::ExternalWorktree,
            }
        };
        fates.push(MemberBranchFate {
            repo_id: member.repo_id.clone(),
            repo_name: member.repo_name.clone(),
            branch: member.branch.clone(),
            fate,
        });
    }

    let _ = out_tx.send(DaemonMessage::DiscardPreview {
        session_id: session_id.to_string(),
        members: fates,
    });
}

/// Resolve one member's targets and run the fate rules against them.
async fn member_fate(
    hub: &Hub,
    member: &SessionMember,
    explicit_base: Option<&str>,
    worktrees_root: &Path,
) -> protocol::BranchFate {
    let Some(repo) = hub
        .state
        .with_persisted(|s| s.repos.iter().find(|r| r.id == member.repo_id).cloned())
    else {
        return protocol::BranchFate::Untouched {
            reason: protocol::UntouchedReason::ExternalWorktree,
        };
    };
    let targets = branch_fate::merge_targets(&hub.state, &repo, explicit_base).await;
    branch_fate::member_branch_fate(FateInput {
        repo_path: Path::new(&repo.path),
        member_worktree: Path::new(&member.worktree_path),
        worktrees_root,
        branch: &member.branch,
        targets: &targets,
    })
    .await
}

/// The base branch a session's members forked from, per its stored spawn
/// config. A session that is already gone reads as repo-backed with no
/// explicit base, which re-derives the repo default rather than guessing.
fn session_spawn_base(hub: &Hub, session_id: &str) -> SessionBase {
    hub.sessions
        .get(session_id)
        .map_or(SessionBase::Repo(None), |rec| {
            branch_fate::session_base(crate::sync::lock(&rec).spawn_config.as_ref())
        })
}

/// Write a session's spawn-time sidecar and mirror the record into it (see
/// [`SessionRegistry::write_sidecar`]).
pub(super) fn write_meta_then_sync(hub: &Hub, meta: &orphan::OrphanMeta) {
    hub.sessions.write_sidecar(meta);
}

async fn discard_session(
    hub: &Hub,
    session_id: &str,
    cleanup: &[protocol::CleanupAction],
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
) {
    // A repeated discard reaches here after the record is gone — the client's
    // pane close following its own auto-discard for a self-exited session.
    // Every step below is a no-op for an unknown id at best (the history end,
    // the registry removal) and a needless write at worst: the layout sweep
    // rewrites state.json, and the sidecar deletes would take a session dir
    // the registry no longer vouches for with them.
    if hub.sessions.get(session_id).is_none() {
        debug!(%session_id, "discard_session for unknown session; ignoring");
        return;
    }
    // Discard drops the registry record and can delete worktrees from disk —
    // the most destructive thing a client can ask for. It left no trail at
    // all, which made "the session vanished" impossible to attribute from a
    // log alone.
    info!(
        %session_id,
        cleanup_targets = cleanup.len(),
        remove_worktrees = cleanup.iter().filter(|c| c.remove_worktree).count(),
        "discard_session: begin"
    );
    let (members, has_per_session_worktree, session_label) = discard_snapshot(hub, session_id);
    let explicit_base = match session_spawn_base(hub, session_id) {
        SessionBase::Repo(base) => base,
        SessionBase::Standalone => None,
    };

    // Refuse the whole discard if any worktree it would remove is still in
    // use by another live session. The user must close the other session(s)
    // first, or discard while keeping the worktree.
    if has_per_session_worktree
        && let Some(blocked) = blocked_discard_message(hub, session_id, cleanup, &members)
    {
        let _ = out_tx.send(blocked);
        return;
    }

    // Brief grace period before touching the filesystem. The typical
    // call sequence is stop_session → discard_session back-to-back, and
    // stop_session's pty.kill() is fire-and-forget — the actual
    // TerminateProcess hop runs through the tracer pipe asynchronously.
    // Without this pause, `git worktree remove --force` races the still-
    // dying child's file handles and fails on Windows. 250 ms covers
    // the typical kill propagation; the fs fallback in
    // `worktree_cleanup::remove_member` handles the rest.
    let wants_removal = cleanup
        .iter()
        .any(|a| a.remove_worktree && has_per_session_worktree);
    if wants_removal {
        tokio::time::sleep(KILL_GRACE).await;
    }

    // Track each removed member's immediate parent so we can prune the
    // now-empty anchor skeleton + wt.<branch-slug>/ wrapper once all
    // members of the group have been processed. A `HashSet` would dedupe
    // in O(1), but the typical N is 1–3 members so a Vec scan is cheaper
    // and keeps allocations bounded.
    let worktrees_root = hub.state.worktrees_dir();
    let mut member_parents: Vec<PathBuf> = Vec::new();
    let mut failures: Vec<protocol::WorktreeCleanupFailure> = Vec::new();
    for action in cleanup {
        if !action.remove_worktree || !has_per_session_worktree {
            continue;
        }
        let Some(member) = members.iter().find(|m| m.repo_id == action.repo_id) else {
            continue;
        };
        let repo = hub
            .state
            .with_persisted(|s| s.repos.iter().find(|r| r.id == member.repo_id).cloned());
        let Some(repo) = repo else {
            continue;
        };
        let repo_path = repo.path.clone();
        // Resolved while the session record is still intact, and only for the
        // mode that consults them.
        let targets = if matches!(action.branch, BranchCleanup::Auto | BranchCleanup::Unknown) {
            branch_fate::merge_targets(&hub.state, &repo, explicit_base.as_deref()).await
        } else {
            Vec::new()
        };
        let worktree_path = PathBuf::from(&member.worktree_path);
        match crate::worktree_cleanup::remove_member(Path::new(&repo_path), &worktree_path).await {
            crate::worktree_cleanup::CleanupOutcome::Removed => {
                // Gated to worktrees under the daemon's own root so a pinned
                // external worktree never loses its branch.
                if worktree_path.starts_with(&worktrees_root) {
                    reap_discarded_session_branch(
                        &repo_path,
                        &member.branch,
                        action.branch,
                        &targets,
                    )
                    .await;
                }
            }
            crate::worktree_cleanup::CleanupOutcome::StillOnDisk { reason } => {
                warn!(
                    %reason,
                    member = %worktree_path.display(),
                    "discard_session: worktree cleanup failed; member dir still on disk"
                );
                let locking_processes = probe_locking_processes(&worktree_path);
                failures.push(protocol::WorktreeCleanupFailure {
                    member_path: worktree_path.to_string_lossy().into_owned(),
                    repo_path: repo_path.clone(),
                    reason,
                    locking_processes,
                });
            }
        }
        if let Some(parent) = worktree_path.parent()
            && !member_parents.iter().any(|p| p == parent)
        {
            member_parents.push(parent.to_path_buf());
        }
    }
    for parent in &member_parents {
        crate::worktree_cleanup::prune_empty_ancestors(parent, &worktrees_root);
    }
    record_end(hub, session_id, SessionEnd::StoppedByUser);
    hub.sessions.delete_sidecar(session_id);
    hub.sessions.delete_session_dir(session_id);
    hub.sessions.remove(session_id);
    close_session_panes(hub, session_id);

    if !failures.is_empty() {
        let _ = out_tx.send(DaemonMessage::WorktreeCleanupFailed {
            session_id: session_id.to_string(),
            session_label,
            failures,
        });
    }
}

pub(super) async fn stop_session(hub: &Hub, session_id: &str) -> anyhow::Result<()> {
    if hub.sessions.get(session_id).is_none() {
        return Err(anyhow!("unknown session: {session_id}"));
    }
    // Recorded before the kill, so this wins over the exit watcher's later
    // `Exited` for the same session.
    record_end(hub, session_id, SessionEnd::StoppedByUser);
    // Mark the session stopped BEFORE killing the child.
    //
    // The session's exit handling (`session::handle_exit`) also marks it
    // stopped, but *with* an exit code — and a client reads a
    // running-to-stopped transition carrying an exit code as a self-exit,
    // which for a worktree-less session means auto-discard. Killing first
    // leaves a window where the exit watcher's snapshot is the first
    // "stopped" the client ever sees, making a deliberate Stop
    // indistinguishable from a crash and silently discarding a session the
    // user only asked to stop. Claiming the transition here closes it: the
    // watcher's later update finds the status already stopped.
    //
    // The handles are taken in this same update, so the exit watcher either
    // ran first (and took the pty itself) or finds it released: a tracer lost
    // during the kill never reads as an unexpected loss.
    let mut pty = None;
    let mut headless_handle = None;
    hub.sessions.update(session_id, |r| {
        r.status = protocol::SessionStatus::Stopped;
        // A stopped session has nothing left waiting on it.
        r.pending_input = None;
        r.background = false;
        pty = r.pty.take();
        headless_handle = r.headless.take();
        r.input_notifier = None;
        r.is_inactive = false;
        push_recent_action(r, "stopped by user".to_string());
    });
    let had_live_handle = pty.is_some() || headless_handle.is_some();
    if let Some(pty) = pty {
        pty.kill();
    }
    if let Some(h) = headless_handle {
        h.kill().await;
    }
    if !had_live_handle {
        // Orphan session — neither PTY nor headless handle survived the
        // daemon restart. Read the sidecar to recover the pid and try a
        // best-effort kill. Without this, `stop_session` on an orphan
        // would mark daemon state as stopped but leave the underlying
        // agent / shell process running forever.
        match orphan::load_meta(&hub.dirs, session_id) {
            Ok(meta) => {
                if orphan::kill_pid(&meta) {
                    tracing::info!(%session_id, pid = meta.pid, "killed orphan process");
                } else {
                    tracing::warn!(
                        %session_id,
                        pid = meta.pid,
                        "orphan kill returned false (pid gone, name mismatch, or signal denied)",
                    );
                }
            }
            Err(err) => {
                tracing::warn!(?err, %session_id, "no orphan sidecar to drive kill");
            }
        }
    }
    hub.sessions.delete_sidecar(session_id);
    remove_hook_settings(hub, session_id);
    Ok(())
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "tests assert preconditions with expect/unwrap; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use crate::history;
    use crate::history::test_support::{FakeExit, fake_pty, insert_live};
    use crate::pty::PtyExit;
    use crate::server::connection::{AuthKind, FetchRegistry, PTY_SESSION_BUDGET, stop_transfers};
    use crate::server::dispatch;
    use crate::server::recovery::discard_abandoned;
    use crate::server::test_support::{
        MemClient, appearance_registry, deepseek_options, deepseek_spawn, dispatch_all, env_spawn,
        hello, idle_record, mem_session, messages_before_sentinel, next_message, only_update,
        publish_sentinel, repo_entry, snapshot_of, spawnless_test_hub, test_hub, test_peer,
        wait_until,
    };
    use chrono::Utc;
    use protocol::SessionStatus;
    use protocol::{SessionKind, SessionMode};
    use std::collections::HashMap;
    use tokio::sync::Mutex as AsyncMutex;
    /// Output the PTY produced after the snapshot waits in its live receiver;
    /// the reply must still reach the client first. Multi-threaded, so a
    /// forwarder spawned before the reply is queued gets the chance to win.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scrollback_reply_precedes_live_output_and_echoes_the_request_id() {
        let (live_tx, live) = mpsc::unbounded_channel();
        live_tx.send(b"after".to_vec()).unwrap();
        let snap = ScrollbackSnapshot {
            data: b"history".to_vec(),
            truncated: true,
            live,
        };
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        let (hub, _scratch) = test_hub("reply-precedes-output");
        let fwd = Forwarding {
            out_tx,
            lane: Arc::default(),
            sessions: Arc::clone(&hub.sessions),
            dirs: hub.dirs.clone(),
        };
        let old = tokio::spawn(std::future::pending::<()>());
        fwd.lane
            .forwarders
            .lock()
            .await
            .insert("s1".to_string(), old);

        reply_then_forward(snap, "s1", Some("req-1".to_string()), &fwd).await;

        let first = out_rx.recv().await.unwrap();
        assert!(
            matches!(
                &first,
                DaemonMessage::Scrollback {
                    session_id,
                    truncated: true,
                    request_id: Some(id),
                    forwarder_restarted: true,
                    ..
                } if session_id == "s1" && id == "req-1"
            ),
            "{first:?}"
        );
        let second = tokio::time::timeout(HANG_GUARD, out_rx.recv())
            .await
            .expect("hung: the new forwarder never sent the queued output")
            .unwrap();
        let expected = base64::engine::general_purpose::STANDARD.encode(b"after");
        assert!(
            matches!(&second, DaemonMessage::PtyOutput { data_b64, .. } if *data_b64 == expected),
            "{second:?}"
        );
        assert!(fwd.lane.forwarders.lock().await.contains_key("s1"));
    }

    /// 48 KiB of PTY output, 64 KiB as base64, ending in `tag` and a newline.
    fn tagged_chunk(tag: &str) -> Vec<u8> {
        let mut bytes = vec![b'.'; 48 * 1024 - tag.len() - 1];
        bytes.extend_from_slice(tag.as_bytes());
        bytes.push(b'\n');
        bytes
    }

    /// Chunks enough to take one session past its budget: 64 of them fill it.
    const TRIP_CHUNKS: usize = 80;

    /// Send chunks `range` of session `id` through its tracer side.
    async fn send_chunks(exit: &FakeExit, id: &str, range: std::ops::Range<usize>) {
        for i in range {
            exit.chunk(tagged_chunk(&format!("<{id}:{i}>"))).await;
        }
    }

    /// Let every task run until all of them wait, well inside the send
    /// timeout of a stalled pump.
    async fn settle() {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    /// Live sessions `ids` in `hub`, and a connection served as `auth` (a
    /// tailnet one as an approved device) that attached each of them and then
    /// stopped reading. Returns the client, the task serving it, and each
    /// session's tracer side, in `ids` order.
    async fn stalled_client_attached_to(
        hub: &Hub,
        ids: &[&str],
        auth: AuthKind,
    ) -> (MemClient, tokio::task::JoinHandle<()>, Vec<FakeExit>) {
        let mut exits = Vec::new();
        for id in ids {
            let (pty, exit) = fake_pty();
            insert_live(&hub.sessions, &hub.dirs, idle_record(id), &pty);
            exits.push(exit);
        }
        let token = if let AuthKind::Tailnet(peer) = &auth {
            let (_, token) = crate::sync::lock(&hub.devices)
                .issue(peer, Utc::now())
                .expect("issue a device token");
            token
        } else {
            hub.auth_token.clone()
        };
        let (mut client, session) = mem_session(hub, auth);
        client.send(&hello(&token, None));
        let welcome = tokio::time::timeout(Duration::from_secs(60), client.next(true))
            .await
            .expect("the daemon answers the Hello");
        assert!(matches!(welcome, Some(DaemonMessage::Welcome { .. })));
        client.stall();
        for id in ids {
            client.send(&ClientMessage::LoadScrollback {
                session_id: (*id).to_owned(),
                request_id: Some(format!("attach-{id}")),
            });
        }
        settle().await;
        (client, session, exits)
    }

    /// Every message the daemon sends until it has been quiet for a second
    /// of virtual time, or until it closes the connection.
    async fn drain_until_quiet(client: &mut MemClient) -> Vec<DaemonMessage> {
        let mut messages = Vec::new();
        while let Ok(Some(message)) =
            tokio::time::timeout(Duration::from_secs(1), client.next(true)).await
        {
            messages.push(message);
        }
        messages
    }

    /// The `Scrollback`s for `id` without a request id: `(restarted, data)`.
    fn resyncs(messages: &[DaemonMessage], id: &str) -> Vec<(bool, Vec<u8>)> {
        messages
            .iter()
            .filter_map(|message| match message {
                DaemonMessage::Scrollback {
                    session_id,
                    data_b64,
                    request_id: None,
                    forwarder_restarted,
                    ..
                } if session_id == id => Some((*forwarder_restarted, decode(data_b64))),
                _ => None,
            })
            .collect()
    }

    fn decode(data_b64: &str) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .decode(data_b64)
            .expect("valid base64")
    }

    /// What a client shows for `id` after `messages`: the last `Scrollback`
    /// it accepted, then every `PtyOutput` after it.
    fn client_view(messages: &[DaemonMessage], id: &str) -> Vec<u8> {
        let mut view = Vec::new();
        for message in messages {
            match message {
                DaemonMessage::Scrollback {
                    session_id,
                    data_b64,
                    ..
                } if session_id == id => view = decode(data_b64),
                DaemonMessage::PtyOutput {
                    session_id,
                    data_b64,
                } if session_id == id => view.extend(decode(data_b64)),
                _ => {}
            }
        }
        view
    }

    fn outputs_contain(messages: &[DaemonMessage], id: &str, tag: &str) -> bool {
        messages.iter().any(|message| {
            matches!(message, DaemonMessage::PtyOutput { session_id, data_b64 }
                if session_id == id && decode(data_b64).ends_with(format!("{tag}\n").as_bytes()))
        })
    }

    #[tokio::test(start_paused = true)]
    async fn a_never_reading_client_keeps_each_sessions_queue_bounded_and_the_connection_closes() {
        let (hub, _scratch) = test_hub("never-reading-client");
        // The bound, on a queue nothing drains.
        let (pty, exit) = fake_pty();
        insert_live(&hub.sessions, &hub.dirs, idle_record("s1"), &pty);
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        let lane: Arc<PtyLane> = Arc::default();
        load_scrollback_and_attach_forwarder(&hub, "s1", None, &out_tx, &lane).await;
        send_chunks(&exit, "s1", 0..TRIP_CHUNKS).await;
        settle().await;
        let mut queued = 0;
        while let Ok(message) = out_rx.try_recv() {
            if let DaemonMessage::PtyOutput { data_b64, .. } = message {
                queued += data_b64.len();
            }
        }
        assert!(
            queued <= PTY_SESSION_BUDGET,
            "{queued} bytes queued past the budget"
        );
        assert_eq!(lane.queued("s1"), queued, "the lane counts what is queued");

        // The connection, an approved tailnet device's: its stalled send
        // closes it.
        let tailnet = AuthKind::Tailnet(test_peer());
        let (_client, session, exits) = stalled_client_attached_to(&hub, &["s2"], tailnet).await;
        send_chunks(&exits[0], "s2", 0..TRIP_CHUNKS).await;
        tokio::time::timeout(Duration::from_secs(600), session)
            .await
            .expect("the stalled connection closes")
            .expect("the session did not panic");
        assert_eq!(
            *hub.client_count.borrow(),
            0,
            "the client count is released"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn pty_output_over_the_budget_resyncs_with_one_scrollback() {
        let (hub, _scratch) = test_hub("budget-resync");
        let (mut client, _session, exits) =
            stalled_client_attached_to(&hub, &["s1"], AuthKind::Local).await;
        send_chunks(&exits[0], "s1", 0..TRIP_CHUNKS).await;
        settle().await;
        client.resume();
        send_chunks(&exits[0], "s1", TRIP_CHUNKS..TRIP_CHUNKS + 3).await;
        let messages = drain_until_quiet(&mut client).await;

        let resynced = resyncs(&messages, "s1");
        assert_eq!(resynced.len(), 1, "one resync");
        assert!(resynced[0].0, "the resync restarts the forwarder");
        let last_tag = format!("<s1:{}>\n", TRIP_CHUNKS + 2);
        let view = client_view(&messages, "s1");
        assert!(
            view.ends_with(last_tag.as_bytes()),
            "output goes on after the resync"
        );

        client.send(&ClientMessage::LoadScrollback {
            session_id: "s1".to_owned(),
            request_id: Some("check".to_owned()),
        });
        let fresh = drain_until_quiet(&mut client).await;
        let fresh = client_view(&fresh, "s1");
        assert!(
            fresh.ends_with(last_tag.as_bytes()),
            "a fresh attach holds the last output"
        );
        assert!(
            view.ends_with(&fresh),
            "the client's view after the resync matches the scrollback file: {} vs {} bytes",
            view.len(),
            fresh.len()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_noisy_session_does_not_resync_a_quiet_one() {
        let (hub, _scratch) = test_hub("noisy-and-quiet");
        let (mut client, _session, exits) =
            stalled_client_attached_to(&hub, &["s1", "s2"], AuthKind::Local).await;
        send_chunks(&exits[1], "s2", 0..1).await;
        send_chunks(&exits[0], "s1", 0..TRIP_CHUNKS).await;
        settle().await;
        client.resume();
        send_chunks(&exits[1], "s2", 1..2).await;
        let messages = drain_until_quiet(&mut client).await;

        assert_eq!(
            resyncs(&messages, "s1").len(),
            1,
            "the noisy session resyncs"
        );
        assert!(
            resyncs(&messages, "s2").is_empty(),
            "the quiet one does not"
        );
        assert!(outputs_contain(&messages, "s2", "<s2:0>"));
        assert!(outputs_contain(&messages, "s2", "<s2:1>"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_load_scrollback_during_a_resync_supersedes_it() {
        let (hub, _scratch) = test_hub("load-during-resync");
        let (mut client, _session, exits) =
            stalled_client_attached_to(&hub, &["s1"], AuthKind::Local).await;
        send_chunks(&exits[0], "s1", 0..TRIP_CHUNKS).await;
        settle().await;
        client.send(&ClientMessage::LoadScrollback {
            session_id: "s1".to_owned(),
            request_id: Some("again".to_owned()),
        });
        settle().await;
        client.resume();
        send_chunks(&exits[0], "s1", TRIP_CHUNKS..TRIP_CHUNKS + 2).await;
        let messages = drain_until_quiet(&mut client).await;

        assert!(
            messages.iter().any(|message| matches!(message,
                DaemonMessage::Scrollback { request_id: Some(id), .. } if id == "again")),
            "the requester's reply arrives"
        );
        assert!(resyncs(&messages, "s1").is_empty(), "no id-less Scrollback");
        assert!(outputs_contain(
            &messages,
            "s1",
            &format!("<s1:{}>", TRIP_CHUNKS + 1)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn a_detach_during_a_resync_sends_nothing_after_it() {
        let (hub, _scratch) = test_hub("detach-during-resync");
        let (mut client, _session, exits) =
            stalled_client_attached_to(&hub, &["s1"], AuthKind::Local).await;
        send_chunks(&exits[0], "s1", 0..TRIP_CHUNKS).await;
        settle().await;
        client.send(&ClientMessage::Detach {
            session_id: "s1".to_owned(),
        });
        settle().await;
        client.resume();
        send_chunks(&exits[0], "s1", TRIP_CHUNKS..TRIP_CHUNKS + 2).await;
        let messages = drain_until_quiet(&mut client).await;

        assert!(
            resyncs(&messages, "s1").is_empty(),
            "no resync after the detach"
        );
        assert!(
            !outputs_contain(&messages, "s1", &format!("<s1:{TRIP_CHUNKS}>")),
            "no output after the detach"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_connection_closing_mid_resync_leaves_no_task() {
        let (hub, _scratch) = test_hub("close-mid-resync");
        let (pty, exit) = fake_pty();
        insert_live(&hub.sessions, &hub.dirs, idle_record("s1"), &pty);
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        let lane: Arc<PtyLane> = Arc::default();
        let fetches: FetchRegistry = Arc::new(AsyncMutex::new(HashMap::new()));
        load_scrollback_and_attach_forwarder(&hub, "s1", None, &out_tx, &lane).await;
        send_chunks(&exit, "s1", 0..TRIP_CHUNKS).await;
        settle().await;

        // The connection's teardown, while the forwarder waits to resync.
        stop_transfers(&lane, &fetches, &Arc::default()).await;
        drop(out_tx);
        let closed = tokio::time::timeout(Duration::from_secs(600), async {
            while out_rx.recv().await.is_some() {}
        })
        .await;
        assert!(
            closed.is_ok(),
            "a forwarder still holds the connection's queue"
        );
        assert_eq!(Arc::strong_count(&lane), 1, "nothing else holds the lane");
    }

    #[tokio::test(start_paused = true)]
    async fn a_session_exiting_mid_resync_gets_its_final_scrollback() {
        let (hub, _scratch) = test_hub("exit-mid-resync");
        let (mut client, _session, mut exits) =
            stalled_client_attached_to(&hub, &["s1"], AuthKind::Local).await;
        send_chunks(&exits[0], "s1", 0..TRIP_CHUNKS).await;
        settle().await;
        exits.remove(0).finish(PtyExit::Code(0)).await;
        settle().await;
        client.resume();
        let messages = drain_until_quiet(&mut client).await;

        let resynced = resyncs(&messages, "s1");
        assert_eq!(resynced.len(), 1, "one final Scrollback");
        assert!(!resynced[0].0, "no forwarder follows it");
        let last_tag = format!("<s1:{}>\n", TRIP_CHUNKS - 1);
        assert!(
            resynced[0].1.ends_with(last_tag.as_bytes()),
            "it holds the session's last output"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_resync_whose_snapshot_times_out_retries_and_keeps_forwarding() {
        let (hub, _scratch) = test_hub("resync-snapshot-timeout");
        let (mut client, _session, exits) =
            stalled_client_attached_to(&hub, &["s1"], AuthKind::Local).await;
        // The lifecycle task's snapshot channel, swapped for one the test
        // holds, so the resync's first request goes unanswered.
        let record = hub.sessions.get("s1").expect("s1 is live");
        let (held_tx, mut held_rx) = mpsc::unbounded_channel();
        let lifecycle_tx = crate::sync::lock(&record)
            .scrollback_snapshot_req
            .replace(held_tx)
            .expect("s1 has a lifecycle task");
        send_chunks(&exits[0], "s1", 0..TRIP_CHUNKS).await;
        settle().await;
        client.resume();

        let unanswered = tokio::time::timeout(Duration::from_secs(60), held_rx.recv())
            .await
            .expect("the resync asks for a snapshot")
            .expect("the request channel is open");
        crate::sync::lock(&record).scrollback_snapshot_req = Some(lifecycle_tx);
        tokio::time::sleep(SCROLLBACK_SNAPSHOT_TIMEOUT * 2).await;
        drop(unanswered);
        send_chunks(&exits[0], "s1", TRIP_CHUNKS..TRIP_CHUNKS + 3).await;
        let messages = drain_until_quiet(&mut client).await;

        let resynced = resyncs(&messages, "s1");
        assert_eq!(resynced.len(), 1, "one resync, from the retried snapshot");
        assert!(resynced[0].0, "the retried resync restarts the forwarder");
        let last_tag = format!("<s1:{}>\n", TRIP_CHUNKS + 2);
        assert!(
            client_view(&messages, "s1").ends_with(last_tag.as_bytes()),
            "output goes on after the resync"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_removed_session_mid_resync_gets_no_scrollback() {
        let (hub, _scratch) = test_hub("removed-mid-resync");
        let (mut client, _session, exits) =
            stalled_client_attached_to(&hub, &["s1"], AuthKind::Local).await;
        send_chunks(&exits[0], "s1", 0..TRIP_CHUNKS).await;
        settle().await;
        hub.sessions.remove("s1");
        settle().await;
        client.resume();
        let messages = drain_until_quiet(&mut client).await;

        assert!(
            resyncs(&messages, "s1").is_empty(),
            "a removed session gets no resync"
        );
    }

    /// Dispatch `msg` against `hub` without expecting a reply.
    async fn dispatch_silently(hub: &Hub, msg: ClientMessage) {
        dispatch_silently_as(hub, "c1", msg).await;
    }

    /// Dispatch `msg` against `hub` from the client `client_id` without
    /// expecting a reply.
    async fn dispatch_silently_as(hub: &Hub, client_id: &str, msg: ClientMessage) {
        let (out_tx, _out_rx) = mpsc::unbounded_channel();
        let (file_tx, _file_rx) = mpsc::channel(1);
        let pty_lane: Arc<PtyLane> = Arc::default();
        let fetches: FetchRegistry = Arc::new(AsyncMutex::new(HashMap::new()));
        let local = AuthKind::Local;
        let ctx = ConnCtx {
            out_tx: &out_tx,
            file_tx: &file_tx,
            pty_lane: &pty_lane,
            fetches: &fetches,
            tails: &Arc::default(),
            client_id,
            client_name: None,
            connection: 1,
            auth: &local,
        };
        dispatch(hub, msg, &ctx)
            .await
            .expect("a send-input dispatch succeeds");
    }

    /// A hub holding `s1`, idle with no process, its screen at 80x24.
    fn hub_with_screen(tag: &str) -> (Hub, crate::server::test_support::ScratchDir) {
        let (hub, scratch) = test_hub(tag);
        let mut rec = idle_record("s1");
        rec.screen = Some(Arc::new(std::sync::Mutex::new(crate::screen::Screen::new(
            80, 24,
        ))));
        hub.sessions.insert(rec);
        (hub, scratch)
    }

    fn resize(cols: u16, rows: u16, claim: bool) -> ClientMessage {
        ClientMessage::Resize {
            session_id: "s1".to_owned(),
            cols,
            rows,
            claim,
        }
    }

    /// Gives `s1` a PTY with no process behind it, so a resize has somewhere to
    /// go.
    fn attach_test_pty(hub: &Hub) {
        #[derive(Debug)]
        struct NoopKiller;
        impl portable_pty::ChildKiller for NoopKiller {
            fn kill(&mut self) -> std::io::Result<()> {
                Ok(())
            }
            fn clone_killer(&self) -> Box<dyn portable_pty::ChildKiller + Send + Sync> {
                Box::new(NoopKiller)
            }
        }
        let (producer, lifecycle_output) = crate::pty::PtyProducer::channel(16);
        let (input_tx, _input_rx) = mpsc::unbounded_channel();
        let (resize_tx, _resize_rx) = mpsc::unbounded_channel();
        let pty = Arc::new(crate::pty::PtyHandle::from_parts(
            crate::pty::PtyHandleParts {
                output: producer.output(),
                lifecycle_output,
                input_tx,
                resize_tx,
                killer: Box::new(NoopKiller),
                pid: None,
            },
        ));
        hub.sessions
            .update("s1", |rec| rec.pty = Some(Arc::clone(&pty)));
    }

    /// The size `s1`'s screen is at.
    fn screen_size(hub: &Hub) -> (u16, u16) {
        let rec = hub.sessions.get("s1").expect("s1 is registered");
        let screen = crate::sync::lock(&rec)
            .screen
            .clone()
            .expect("s1 has a screen");
        crate::sync::lock(&screen).size()
    }

    #[tokio::test]
    async fn the_first_resize_sizes_an_unsized_session() {
        let (hub, _scratch) = hub_with_screen("first-resize");
        dispatch_silently(&hub, resize(100, 30, false)).await;
        assert_eq!(
            screen_size(&hub),
            (100, 30),
            "an unclaimed resize sizes a session no client has sized"
        );
    }

    /// A client relaunched at a new size keeps its id and attaches unclaimed;
    /// the size it left behind is its own, so its new one replaces it.
    #[tokio::test]
    async fn an_unclaimed_resize_from_the_client_that_sized_the_session_applies() {
        let (hub, _scratch) = hub_with_screen("unclaimed-same-client");
        attach_test_pty(&hub);
        dispatch_silently(&hub, resize(100, 30, false)).await;
        dispatch_silently(&hub, resize(60, 20, false)).await;
        assert_eq!(
            screen_size(&hub),
            (60, 20),
            "the sizing client's unclaimed resize applies"
        );

        let (hub, _scratch) = hub_with_screen("unclaimed-after-own-claim");
        attach_test_pty(&hub);
        dispatch_silently(&hub, resize(100, 30, true)).await;
        dispatch_silently(&hub, resize(60, 20, false)).await;
        assert_eq!(
            screen_size(&hub),
            (60, 20),
            "an unclaimed resize after the same client's claim applies"
        );
    }

    #[tokio::test]
    async fn an_unclaimed_resize_from_another_client_is_still_ignored() {
        let (hub, _scratch) = hub_with_screen("unclaimed-other-client");
        attach_test_pty(&hub);
        dispatch_silently_as(&hub, "c1", resize(100, 30, false)).await;
        dispatch_silently_as(&hub, "c2", resize(60, 20, false)).await;
        assert_eq!(
            screen_size(&hub),
            (100, 30),
            "another client's unclaimed resize is ignored"
        );

        let (hub, _scratch) = hub_with_screen("unclaimed-other-after-claim");
        attach_test_pty(&hub);
        dispatch_silently_as(&hub, "c1", resize(100, 30, true)).await;
        dispatch_silently_as(&hub, "c2", resize(60, 20, false)).await;
        assert_eq!(
            screen_size(&hub),
            (100, 30),
            "another client's unclaimed resize after a claim is ignored"
        );
        dispatch_silently_as(&hub, "c2", resize(70, 22, true)).await;
        dispatch_silently_as(&hub, "c1", resize(60, 20, false)).await;
        assert_eq!(
            screen_size(&hub),
            (70, 22),
            "once another client claims, the first one's unclaimed resize is ignored"
        );
    }

    #[tokio::test]
    async fn a_claimed_resize_to_a_new_size_applies() {
        let (hub, _scratch) = hub_with_screen("claimed-resize");
        dispatch_silently(&hub, resize(100, 30, false)).await;
        dispatch_silently(&hub, resize(60, 20, true)).await;
        assert_eq!(screen_size(&hub), (60, 20), "a claim takes the size");
        dispatch_silently(&hub, resize(90, 25, true)).await;
        assert_eq!(screen_size(&hub), (90, 25), "another claim takes it again");
    }

    /// A claimed resize to the size the session's PTY already has sends nothing
    /// to the PTY: `ConPTY` answers every resize, an unchanged one included, with a
    /// full repaint of the screen.
    #[tokio::test]
    async fn a_claimed_resize_to_the_applied_size_does_not_resize_the_pty() {
        #[derive(Debug)]
        struct NoopKiller;
        impl portable_pty::ChildKiller for NoopKiller {
            fn kill(&mut self) -> std::io::Result<()> {
                Ok(())
            }
            fn clone_killer(&self) -> Box<dyn portable_pty::ChildKiller + Send + Sync> {
                Box::new(NoopKiller)
            }
        }
        let (hub, _scratch) = hub_with_screen("same-size-claim");
        let (producer, lifecycle_output) = crate::pty::PtyProducer::channel(16);
        let (input_tx, _input_rx) = mpsc::unbounded_channel();
        let (resize_tx, mut resize_rx) = mpsc::unbounded_channel();
        let pty = Arc::new(crate::pty::PtyHandle::from_parts(
            crate::pty::PtyHandleParts {
                output: producer.output(),
                lifecycle_output,
                input_tx,
                resize_tx,
                killer: Box::new(NoopKiller),
                pid: None,
            },
        ));
        hub.sessions
            .update("s1", |rec| rec.pty = Some(Arc::clone(&pty)));
        dispatch_silently(&hub, resize(77, 27, false)).await;
        dispatch_silently(&hub, resize(77, 27, true)).await;
        assert_eq!(
            resize_rx.try_recv().ok(),
            Some((77, 27)),
            "the first resize sizes the PTY"
        );
        assert!(
            resize_rx.try_recv().is_err(),
            "a claim at the applied size sends no second resize"
        );
        dispatch_silently(&hub, resize(77, 27, false)).await;
        dispatch_silently_as(&hub, "c2", resize(77, 27, true)).await;
        assert!(
            resize_rx.try_recv().is_err(),
            "neither the sizing client's unclaimed resize nor another client's claim \
             at the applied size sends a resize"
        );
        dispatch_silently_as(&hub, "c2", resize(60, 20, false)).await;
        assert_eq!(
            resize_rx.try_recv().ok(),
            Some((60, 20)),
            "a skipped claim still makes its client the one that sized the session"
        );
        dispatch_silently(&hub, resize(90, 25, true)).await;
        assert_eq!(
            resize_rx.try_recv().ok(),
            Some((90, 25)),
            "a claim at a new size still applies"
        );
    }

    /// A claimed resize before the session has a PTY sizes the screen but is not
    /// remembered as applied, so a later claim at that size still reaches the
    /// PTY once it exists.
    #[tokio::test]
    async fn a_resize_before_the_pty_exists_does_not_mark_the_size_applied() {
        #[derive(Debug)]
        struct NoopKiller;
        impl portable_pty::ChildKiller for NoopKiller {
            fn kill(&mut self) -> std::io::Result<()> {
                Ok(())
            }
            fn clone_killer(&self) -> Box<dyn portable_pty::ChildKiller + Send + Sync> {
                Box::new(NoopKiller)
            }
        }
        let (hub, _scratch) = hub_with_screen("no-pty-claim");
        dispatch_silently(&hub, resize(77, 27, true)).await;
        let (producer, lifecycle_output) = crate::pty::PtyProducer::channel(16);
        let (input_tx, _input_rx) = mpsc::unbounded_channel();
        let (resize_tx, mut resize_rx) = mpsc::unbounded_channel();
        let pty = Arc::new(crate::pty::PtyHandle::from_parts(
            crate::pty::PtyHandleParts {
                output: producer.output(),
                lifecycle_output,
                input_tx,
                resize_tx,
                killer: Box::new(NoopKiller),
                pid: None,
            },
        ));
        hub.sessions
            .update("s1", |rec| rec.pty = Some(Arc::clone(&pty)));
        dispatch_silently(&hub, resize(77, 27, true)).await;
        assert_eq!(
            resize_rx.try_recv().ok(),
            Some((77, 27)),
            "a claim at that size reaches the PTY once it exists"
        );
    }

    /// The owner's key leaves a hook-reported prompt at once, with no settle
    /// window.
    #[tokio::test]
    async fn owner_input_clears_a_pending_hook_prompt() {
        use crate::history::test_support::{fake_pty, insert_live, record};
        let (hub, _scratch) = test_hub("owner-input-clears");
        let (pty, _exit_tx) = fake_pty();
        insert_live(
            &hub.sessions,
            &hub.dirs,
            record("s1", SessionMode::Interactive),
            &pty,
        );
        hub.sessions.update("s1", |rec| {
            rec.hook_driven = true;
            rec.status = SessionStatus::AwaitingInput;
            rec.pending_input = Some(protocol::PendingInput::Other {
                message: "approve?".to_owned(),
            });
        });
        let data_b64 = base64::engine::general_purpose::STANDARD.encode(b"y");
        dispatch_silently(
            &hub,
            ClientMessage::SendInput {
                session_id: "s1".to_owned(),
                data_b64,
                automatic: false,
            },
        )
        .await;
        let snapshot = snapshot_of(&hub, "s1");
        assert_eq!(
            snapshot.status,
            SessionStatus::Idle,
            "the owner's key leaves the prompt at once"
        );
        assert_eq!(
            snapshot.pending_input, None,
            "the pending prompt is cleared"
        );
    }

    /// An owner key bumps the typing lane's owner epoch; the terminal's own
    /// reply does not, and neither takes the lane.
    #[tokio::test]
    async fn send_input_bumps_the_owner_epoch_only_for_an_owner_key() {
        use crate::history::test_support::{insert_live, record};
        let (hub, _scratch) = test_hub("send-input-owner-epoch");
        let (pty, _exit_tx) = fake_pty();
        insert_live(
            &hub.sessions,
            &hub.dirs,
            record("s1", SessionMode::Interactive),
            &pty,
        );
        let send = |bytes: &[u8], automatic: bool| ClientMessage::SendInput {
            session_id: "s1".to_owned(),
            data_b64: base64::engine::general_purpose::STANDARD.encode(bytes),
            automatic,
        };
        dispatch_silently(&hub, send(b"\x1b[12;1R", true)).await;
        assert_eq!(
            pty.typing().owner_epoch(),
            0,
            "a terminal reply is no owner key"
        );
        dispatch_silently(&hub, send(b"y", false)).await;
        assert_eq!(
            pty.typing().owner_epoch(),
            1,
            "the owner's key bumps the epoch"
        );
        assert_eq!(pty.typing().holder(), None, "input never takes the lane");
    }

    /// The terminal's own replies never answer a hook-reported prompt.
    #[tokio::test]
    async fn automatic_input_leaves_a_hook_prompt_pending() {
        use crate::history::test_support::{fake_pty, insert_live, record};
        let (hub, _scratch) = test_hub("automatic-input-keeps");
        let (pty, _exit_tx) = fake_pty();
        insert_live(
            &hub.sessions,
            &hub.dirs,
            record("s1", SessionMode::Interactive),
            &pty,
        );
        hub.sessions.update("s1", |rec| {
            rec.hook_driven = true;
            rec.status = SessionStatus::AwaitingInput;
            rec.pending_input = Some(protocol::PendingInput::Other {
                message: "approve?".to_owned(),
            });
        });
        let data_b64 = base64::engine::general_purpose::STANDARD.encode(b"\x1b[12;1R");
        dispatch_silently(
            &hub,
            ClientMessage::SendInput {
                session_id: "s1".to_owned(),
                data_b64,
                automatic: true,
            },
        )
        .await;
        let snapshot = snapshot_of(&hub, "s1");
        assert_eq!(
            snapshot.status,
            SessionStatus::AwaitingInput,
            "a terminal reply does not answer the prompt"
        );
        assert!(
            snapshot.pending_input.is_some(),
            "the pending prompt survives a terminal reply"
        );
    }

    /// A session with no hook events keeps exactly today's behaviour: input
    /// never touches its status or pending state.
    #[tokio::test]
    async fn owner_input_leaves_a_heuristic_prompt_alone() {
        use crate::history::test_support::{fake_pty, insert_live, record};
        let (hub, _scratch) = test_hub("owner-input-heuristic");
        let (pty, _exit_tx) = fake_pty();
        insert_live(
            &hub.sessions,
            &hub.dirs,
            record("s1", SessionMode::Interactive),
            &pty,
        );
        hub.sessions.update("s1", |rec| {
            rec.status = SessionStatus::AwaitingInput;
            rec.pending_input = Some(protocol::PendingInput::Other {
                message: "approve?".to_owned(),
            });
        });
        let data_b64 = base64::engine::general_purpose::STANDARD.encode(b"y");
        dispatch_silently(
            &hub,
            ClientMessage::SendInput {
                session_id: "s1".to_owned(),
                data_b64,
                automatic: false,
            },
        )
        .await;
        let snapshot = snapshot_of(&hub, "s1");
        assert_eq!(
            snapshot.status,
            SessionStatus::AwaitingInput,
            "a heuristic prompt is the classifier's to clear"
        );
        assert!(
            snapshot.pending_input.is_some(),
            "the heuristic's pending input is untouched"
        );
    }

    #[tokio::test]
    async fn stop_then_exit_keeps_stopped_by_user() {
        use crate::history::test_support::{fake_pty, insert_live, record};
        use crate::pty::PtyExit;
        let (hub, _scratch) = test_hub("stop-then-exit");
        let (pty, exit_tx) = fake_pty();
        insert_live(
            &hub.sessions,
            &hub.dirs,
            record("s1", SessionMode::Interactive),
            &pty,
        );
        stop_session(&hub, "s1").await.expect("stop");
        // The exit watcher deletes meta.json as its last step, so a fresh one
        // tells the test when the watcher has finished.
        let meta = orphan::meta_from_record(
            "s1".to_string(),
            1,
            "s1".to_string(),
            SessionKind::Standalone,
            SessionMode::Interactive,
            Vec::new(),
            Utc::now(),
            None,
            None,
            Some(agent_registry::default_agent()),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .expect("build meta");
        orphan::write_meta(&hub.dirs, &meta).expect("write meta");
        exit_tx.send(PtyExit::Code(0)).expect("report the exit");
        // The exit watcher's last step drops the sidecar.
        crate::history::test_support::sidecar_gone(&hub.dirs, "s1").await;
        let entries = history::read_all(&hub.dirs);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].end, SessionEnd::StoppedByUser);
    }

    #[tokio::test]
    async fn discard_writes_stopped_by_user() {
        use crate::history::test_support::record;
        let (hub, _scratch) = test_hub("discard-history");
        hub.sessions.insert(record("s1", SessionMode::PlainShell));
        let (out_tx, _out_rx) = mpsc::unbounded_channel();
        discard_session(&hub, "s1", &[], &out_tx).await;
        assert!(hub.sessions.get("s1").is_none(), "the session is gone");
        let entries = history::read_all(&hub.dirs);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_id, "s1");
        assert_eq!(entries[0].end, SessionEnd::StoppedByUser);
    }

    /// The spawn-time sidecar is built from a snapshot taken before the
    /// session's first status tick; the sync right after the write must leave
    /// the status that actually reached the record on disk.
    #[tokio::test]
    async fn spawn_sidecar_sync_picks_up_a_status_change() {
        use crate::history::test_support::record;
        let (hub, _scratch) = test_hub("spawn-sidecar-sync");
        hub.sessions.insert(record("s1", SessionMode::Interactive));
        let mut meta = orphan::meta_from_record(
            "s1".to_string(),
            1,
            "s1".to_string(),
            SessionKind::Standalone,
            SessionMode::Interactive,
            Vec::new(),
            Utc::now(),
            None,
            None,
            Some(agent_registry::default_agent()),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .expect("build meta");
        meta.status = Some(SessionStatus::Working);
        meta.status_since =
            Some(chrono::DateTime::from_timestamp(1_600_000_000, 0).expect("a fixed stamp"));

        // The first status tick lands after the snapshot was taken, and its own
        // sync found no sidecar to update.
        hub.sessions.update("s1", |rec| {
            rec.status = SessionStatus::AwaitingInput;
        });

        write_meta_then_sync(&hub, &meta);

        let loaded = orphan::load_meta(&hub.dirs, "s1").expect("load meta");
        assert_eq!(loaded.status, Some(SessionStatus::AwaitingInput));
        let live = hub
            .sessions
            .snapshots()
            .into_iter()
            .next()
            .expect("the record");
        assert_eq!(loaded.status_since, live.status_since);
    }

    /// A discard for a session the registry no longer holds — the client's
    /// pane close racing its own auto-discard — must leave the disk alone.
    #[tokio::test]
    async fn discard_of_unknown_session_is_a_no_op() {
        let (hub, _scratch) = test_hub("discard-unknown");
        hub.state.mutate(|_| ()).expect("persist state.json");
        // A sidecar left behind by a session the registry no longer lists.
        let stray = hub.dirs.sessions_dir.join("ghost");
        std::fs::create_dir_all(&stray).expect("create stray session dir");
        std::fs::write(stray.join("meta.json"), b"{}").expect("write stray meta");
        let before = std::fs::read(&hub.dirs.state_file).expect("read state.json");
        let before_mtime = std::fs::metadata(&hub.dirs.state_file)
            .and_then(|m| m.modified())
            .expect("state.json mtime");

        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        discard_session(&hub, "ghost", &[], &out_tx).await;

        assert!(
            out_rx.try_recv().is_err(),
            "an unknown session produces no reply"
        );
        assert!(
            stray.join("meta.json").is_file(),
            "the stray sidecar survives"
        );
        assert_eq!(
            std::fs::read(&hub.dirs.state_file).expect("read state.json"),
            before,
            "state.json keeps its contents"
        );
        assert_eq!(
            std::fs::metadata(&hub.dirs.state_file)
                .and_then(|m| m.modified())
                .expect("state.json mtime"),
            before_mtime,
            "state.json is not rewritten"
        );
        assert!(
            crate::history::read_one(&hub.dirs, "ghost").is_none(),
            "no history entry for a session that never existed"
        );
    }

    #[tokio::test]
    async fn drained_shutdown_writes_daemon_shutdown() {
        use crate::history::test_support::{fake_pty, insert_live, record};
        let (hub, _scratch) = test_hub("shutdown-history");
        let (pty, _exit_tx) = fake_pty();
        insert_live(
            &hub.sessions,
            &hub.dirs,
            record("s1", SessionMode::Interactive),
            &pty,
        );
        shutdown_all(&hub).await;
        let entries = history::read_all(&hub.dirs);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].end, SessionEnd::DaemonShutdown);
    }

    #[tokio::test]
    async fn park_writes_stopped_by_user() {
        use crate::history::test_support::{fake_pty, insert_live, record};
        let (hub, _scratch) = test_hub("park-history");
        let (pty, _exit_tx) = fake_pty();
        insert_live(
            &hub.sessions,
            &hub.dirs,
            record("s1", SessionMode::Interactive),
            &pty,
        );
        park_session(&hub, "s1").await;
        let entries = history::read_all(&hub.dirs);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].end, SessionEnd::StoppedByUser);
    }

    /// A stopped session drops the hook prompt it was waiting on: nothing
    /// is left to answer once the process is gone.
    #[tokio::test]
    async fn a_stopped_session_drops_its_pending_input() {
        use crate::history::test_support::{fake_pty, insert_live, record};
        let (hub, _scratch) = test_hub("stop-drops-pending");
        let (pty, _exit_tx) = fake_pty();
        let mut rec = record("s1", SessionMode::Interactive);
        rec.status = SessionStatus::AwaitingInput;
        rec.pending_input = Some(protocol::PendingInput::Permission {
            tool_name: "Bash".to_owned(),
            summary: "cargo build".to_owned(),
        });
        rec.hook_driven = true;
        rec.background = true;
        insert_live(&hub.sessions, &hub.dirs, rec, &pty);
        stop_session(&hub, "s1").await.expect("stop");
        let snapshot = snapshot_of(&hub, "s1");
        assert_eq!(snapshot.status, SessionStatus::Stopped);
        assert_eq!(
            snapshot.pending_input, None,
            "the prompt left with the stop"
        );
        assert!(!snapshot.background, "no background work outlives the stop");
    }

    /// A parked session drops its prompt the same way: it keeps its record
    /// for Resume, but nothing is waiting on it.
    #[tokio::test]
    async fn a_parked_session_drops_its_pending_input() {
        use crate::history::test_support::{fake_pty, insert_live, record};
        let (hub, _scratch) = test_hub("park-drops-pending");
        let (pty, _exit_tx) = fake_pty();
        let mut rec = record("s1", SessionMode::Interactive);
        rec.status = SessionStatus::AwaitingInput;
        rec.pending_input = Some(protocol::PendingInput::Permission {
            tool_name: "Bash".to_owned(),
            summary: "cargo build".to_owned(),
        });
        rec.hook_driven = true;
        insert_live(&hub.sessions, &hub.dirs, rec, &pty);
        park_session(&hub, "s1").await;
        let snapshot = snapshot_of(&hub, "s1");
        assert_eq!(snapshot.status, SessionStatus::Stopped);
        assert_eq!(
            snapshot.pending_input, None,
            "the prompt left with the park"
        );
    }

    #[tokio::test]
    async fn stop_session_sends_no_attention() {
        use crate::history::test_support::{fake_pty, insert_live, record};
        use crate::pty::PtyExit;
        let (hub, _scratch) = test_hub("stop-no-attention");
        let (pty, exit_tx) = fake_pty();
        insert_live(
            &hub.sessions,
            &hub.dirs,
            record("s1", SessionMode::Interactive),
            &pty,
        );
        let mut events = hub.sessions.subscribe();
        let sent = dispatch_all(
            &hub,
            ClientMessage::StopSession {
                session_id: "s1".to_owned(),
                cleanup: Vec::new(),
            },
        )
        .await;
        assert!(
            !sent
                .iter()
                .any(|msg| matches!(msg, DaemonMessage::Attention { .. })),
            "a user stop asks for no attention: {sent:?}"
        );
        exit_tx
            .send(PtyExit::Code(1))
            .expect("deliver the kill's exit");
        // The watcher's update, attention and sidecar delete run in one poll
        // with no await between them, so on this current-thread runtime any
        // attention has gone out once the update shows.
        let watcher_ran = || {
            hub.sessions
                .get("s1")
                .is_some_and(|rec| crate::sync::lock(&rec).scrollback_snapshot_req.is_none())
        };
        wait_until("the exit watcher", watcher_ran).await;
        while let Ok(event) = events.try_recv() {
            assert!(
                !matches!(event, SessionEvent::Attention { .. }),
                "the kill's exit asks for no attention: {event:?}"
            );
        }
        let snapshot = snapshot_of(&hub, "s1");
        assert_eq!(snapshot.status, SessionStatus::Stopped);
        assert_eq!(snapshot.exit_code, None);
    }

    #[tokio::test]
    #[expect(clippy::panic, reason = "a wrong target shape fails the test loudly")]
    async fn restart_duplicate_pins_the_source_worktree() {
        let (hub, scratch) = spawnless_test_hub("restart-pin");
        hub.state
            .mutate(|s| {
                s.repos
                    .push(repo_entry("r1", "r1", &scratch.path().join("repo")));
            })
            .expect("register repo");
        let elsewhere = scratch
            .path()
            .join("old-root")
            .join("wt.silly-gecko")
            .join("r1")
            .to_string_lossy()
            .into_owned();
        let target = SpawnTarget::Single {
            repo_id: "r1".to_owned(),
            branch_name: "wt/silly-gecko".to_owned(),
            base_branch: Some("main".to_owned()),
            use_worktree: true,
            checkout_strategy: None,
            worktree_reuse: protocol::WorktreeReusePolicy::Reuse,
            existing_worktree: None,
        };
        let mut source = idle_record("src");
        source.members = vec![SessionMember {
            repo_id: "r1".to_owned(),
            repo_name: "r1".to_owned(),
            branch: "wt/silly-gecko".to_owned(),
            worktree_path: elsewhere.clone(),
        }];
        source.spawn_config = Some(protocol::SpawnConfig::from_request(&env_spawn(
            target,
            SessionMode::Interactive,
            Vec::new(),
            None,
        )));
        hub.sessions.insert(source);
        let request = duplicate_request(&hub, "src", true)
            .await
            .expect("a restart reruns the source's config");
        match request.target {
            SpawnTarget::Single {
                branch_name,
                existing_worktree,
                ..
            } => {
                assert_eq!(branch_name, "wt/silly-gecko");
                assert_eq!(
                    existing_worktree.as_deref(),
                    Some(elsewhere.as_str()),
                    "pinned to the folder the source ran in"
                );
            }
            other => panic!("expected a single target, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn restart_refuses_while_the_stopped_child_is_still_dying() {
        use crate::history::test_support::{fake_pty, insert_live, record};
        let (hub, _scratch) = test_hub("restart-still-stopping");
        let (pty, _exit_tx) = fake_pty();
        insert_live(
            &hub.sessions,
            &hub.dirs,
            record("s1", SessionMode::Interactive),
            &pty,
        );
        hub.sessions.update("s1", |rec| {
            rec.status = SessionStatus::Stopped;
            rec.pty = None;
        });
        let err = duplicate_request(&hub, "s1", true)
            .await
            .expect_err("the stopped child has not exited");
        assert_eq!(
            err.to_string(),
            "The session is still stopping; try Restart again in a moment."
        );
    }

    #[tokio::test]
    async fn a_rerun_that_resumes_marks_the_source_recovered() {
        let (hub, _scratch) = test_hub("rerun-marks-recovered");
        hub.sessions.insert(idle_record("src"));
        record_end(&hub, "src", SessionEnd::StoppedByUser);
        let recovered = || {
            history::read_one(&hub.dirs, "src")
                .expect("the source's history entry")
                .recovered_at
        };
        finish_rerun(&hub, "src", false);
        assert_eq!(
            recovered(),
            None,
            "a fresh conversation leaves Recover alone"
        );
        finish_rerun(&hub, "src", true);
        assert!(
            recovered().is_some(),
            "Recover must not resume it a second time"
        );
    }

    #[tokio::test]
    async fn tracer_lost_after_user_stop_does_not_abandon() {
        use crate::history::test_support::{fake_pty, insert_live, record, write_meta_for};
        use crate::pty::PtyExit;
        let (hub, _scratch) = test_hub("stop-then-lost");
        let (pty, exit_tx) = fake_pty();
        insert_live(
            &hub.sessions,
            &hub.dirs,
            record("s1", SessionMode::Interactive),
            &pty,
        );
        write_meta_for(&hub.dirs, "s1");
        stop_session(&hub, "s1").await.expect("stop");
        exit_tx.send(PtyExit::TracerLost).expect("report the loss");
        // Only the exit watcher drops the scrollback request (a stop leaves
        // it), so this waits for the watcher to have run.
        let watcher_ran = || {
            hub.sessions
                .get("s1")
                .is_some_and(|rec| crate::sync::lock(&rec).scrollback_snapshot_req.is_none())
        };
        // The exit watcher ends a stopped session like any exit.
        wait_until("the exit watcher", watcher_ran).await;
        let rec = hub.sessions.get("s1").expect("still listed");
        assert!(!crate::sync::lock(&rec).is_abandoned);
        assert!(
            orphan::load_meta(&hub.dirs, "s1").is_err(),
            "a stopped session's sidecar is deleted"
        );
        let entries = history::read_all(&hub.dirs);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].end, SessionEnd::StoppedByUser);
    }

    #[tokio::test]
    async fn dismiss_after_tracer_loss_keeps_tracer_lost_history() {
        use crate::history::test_support::{fake_pty, insert_live, record, write_meta_for};
        use crate::pty::PtyExit;
        let (hub, _scratch) = test_hub("lost-then-dismiss");
        let (pty, exit_tx) = fake_pty();
        insert_live(
            &hub.sessions,
            &hub.dirs,
            record("s1", SessionMode::Interactive),
            &pty,
        );
        write_meta_for(&hub.dirs, "s1");
        exit_tx.send(PtyExit::TracerLost).expect("report the loss");
        let abandoned = || {
            hub.sessions
                .get("s1")
                .is_some_and(|rec| crate::sync::lock(&rec).is_abandoned)
        };
        wait_until("the session to be abandoned", abandoned).await;
        discard_abandoned(&hub, "s1");
        assert!(hub.sessions.get("s1").is_none(), "dismissed");
        let entries = history::read_all(&hub.dirs);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].end, SessionEnd::TracerLost);
    }

    /// A backstop for the waits below: a passing run never reaches it.
    const HANG_GUARD: Duration = Duration::from_secs(300);

    /// More chunks than tokio's cooperative budget of 128, fewer than the
    /// lifecycle queue's 256.
    const TRAILING_CHUNKS: usize = 200;

    /// A live `s1` on `hub` with its own `pty` reference dropped, so only the
    /// record and the tracer side keep it, and a client's `live` receiver
    /// from a snapshot taken before any output.
    async fn live_session_with_client(
        hub: &Hub,
    ) -> (
        history::test_support::FakeExit,
        mpsc::UnboundedReceiver<Vec<u8>>,
    ) {
        use crate::history::test_support::{fake_pty, insert_live, record};
        let (pty, exit_tx) = fake_pty();
        insert_live(
            &hub.sessions,
            &hub.dirs,
            record("s1", SessionMode::Interactive),
            &pty,
        );
        drop(pty);
        let snap_tx = hub
            .sessions
            .get("s1")
            .and_then(|rec| crate::sync::lock(&rec).scrollback_snapshot_req.clone())
            .expect("a live session takes snapshot requests");
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        snap_tx.send(reply_tx).expect("the lifecycle task is alive");
        let snap = tokio::time::timeout(HANG_GUARD, reply_rx)
            .await
            .expect("hung: no snapshot reply")
            .expect("a snapshot reply");
        (exit_tx, snap.live)
    }

    /// Sends `<0>`, `<1>`, … without yielding and returns the bytes sent.
    async fn send_numbered_chunks(exit_tx: &history::test_support::FakeExit) -> Vec<u8> {
        tokio::task::unconstrained(async {
            let mut sent = Vec::new();
            for seq in 0..TRAILING_CHUNKS {
                let chunk = format!("<{seq}>").into_bytes();
                sent.extend_from_slice(&chunk);
                exit_tx.chunk(chunk).await;
            }
            sent
        })
        .await
    }

    /// Waits for the next update that reports `s1` stopped.
    async fn wait_for_stopped(events: &mut tokio::sync::broadcast::Receiver<SessionEvent>) {
        let stopped = async {
            loop {
                match events.recv().await {
                    Ok(SessionEvent::Updated(snap, _))
                        if snap.id == "s1" && snap.status == protocol::SessionStatus::Stopped =>
                    {
                        return;
                    }
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        std::future::pending::<()>().await;
                    }
                }
            }
        };
        tokio::time::timeout(HANG_GUARD, stopped)
            .await
            .expect("hung: the session never read Stopped");
    }

    /// Waits for a client's `live` receiver to close.
    async fn wait_for_live_closed(live: &mut mpsc::UnboundedReceiver<Vec<u8>>) {
        tokio::time::timeout(HANG_GUARD, async { while live.recv().await.is_some() {} })
            .await
            .expect("hung: the client's live receiver never closed");
    }

    /// A discard sent as the session reads `Stopped` leaves no session dir:
    /// no output still on its way recreates it.
    #[tokio::test]
    async fn a_discard_after_the_exit_leaves_no_session_dir() {
        let (hub, _scratch) = test_hub("discard-after-exit");
        let (exit_tx, mut live) = live_session_with_client(&hub).await;
        let mut events = hub.sessions.subscribe();
        send_numbered_chunks(&exit_tx).await;
        tokio::task::unconstrained(exit_tx.finish(crate::pty::PtyExit::Code(0))).await;

        wait_for_stopped(&mut events).await;
        dispatch_silently(
            &hub,
            ClientMessage::DiscardSession {
                session_id: "s1".to_owned(),
                cleanup: Vec::new(),
            },
        )
        .await;
        wait_for_live_closed(&mut live).await;

        assert!(
            !hub.dirs.sessions_dir.join("s1").exists(),
            "the discarded session's dir is gone"
        );
    }

    /// A `LoadScrollback` sent as the session reads `Stopped` gets every
    /// chunk the session sent before its exit.
    #[tokio::test]
    async fn load_scrollback_after_the_exit_has_every_chunk() {
        let (hub, _scratch) = test_hub("load-after-exit");
        let (exit_tx, _live) = live_session_with_client(&hub).await;
        let mut events = hub.sessions.subscribe();
        let sent = send_numbered_chunks(&exit_tx).await;
        tokio::task::unconstrained(exit_tx.finish(crate::pty::PtyExit::Code(0))).await;

        wait_for_stopped(&mut events).await;
        let replies = dispatch_all(
            &hub,
            ClientMessage::LoadScrollback {
                session_id: "s1".to_owned(),
                request_id: None,
            },
        )
        .await;

        let data = replies
            .iter()
            .find_map(|msg| match msg {
                DaemonMessage::Scrollback { data_b64, .. } => Some(
                    base64::engine::general_purpose::STANDARD
                        .decode(data_b64)
                        .expect("the reply's data is base64"),
                ),
                _ => None,
            })
            .expect("a Scrollback reply");
        assert_eq!(
            String::from_utf8_lossy(&data),
            String::from_utf8_lossy(&sent),
            "the scrollback a client loads once the session reads Stopped"
        );
    }

    /// Stop then Discard back to back, with output still queued behind them,
    /// leaves no session dir: the lifecycle task writes nothing for a
    /// session whose dir was deleted.
    #[tokio::test]
    async fn stop_then_discard_leaves_no_session_dir() {
        let (hub, _scratch) = test_hub("stop-then-discard");
        let (exit_tx, mut live) = live_session_with_client(&hub).await;
        stop_session(&hub, "s1").await.expect("stop");
        send_numbered_chunks(&exit_tx).await;
        let (out_tx, _out_rx) = mpsc::unbounded_channel();
        discard_session(&hub, "s1", &[], &out_tx).await;
        tokio::task::unconstrained(exit_tx.finish(crate::pty::PtyExit::Code(0))).await;

        wait_for_live_closed(&mut live).await;

        assert!(
            !hub.dirs.sessions_dir.join("s1").exists(),
            "the discarded session's dir is gone"
        );
    }

    #[tokio::test]
    #[expect(clippy::panic, reason = "a missing broadcast fails the test loudly")]
    async fn set_session_busy_excluded_updates_the_registry_and_broadcasts() {
        let (hub, _scratch) = test_hub("busy-excluded-set");
        hub.sessions.insert(idle_record("s1"));
        let mut events = hub.sessions.subscribe();
        for excluded in [true, false] {
            let sent = dispatch_all(
                &hub,
                ClientMessage::SetSessionBusyExcluded {
                    session_id: "s1".to_owned(),
                    excluded,
                },
            )
            .await;
            assert!(sent.is_empty(), "the broadcast is the answer: {sent:?}");
            let rec = hub.sessions.get("s1").expect("s1 is registered");
            assert_eq!(crate::sync::lock(&rec).busy_excluded, excluded);
            let Ok(SessionEvent::Updated(snap, None)) = events.try_recv() else {
                panic!("expected a SessionUpdated broadcast for excluded = {excluded}");
            };
            assert_eq!(snap.busy_excluded, excluded);
        }
    }

    #[tokio::test]
    async fn set_session_busy_excluded_for_an_unknown_session_answers_like_rename() {
        let (hub, _scratch) = test_hub("busy-excluded-unknown");
        let renamed = dispatch_all(
            &hub,
            ClientMessage::RenameSession {
                session_id: "ghost".to_owned(),
                label: Some("x".to_owned()),
            },
        )
        .await;
        let excluded = dispatch_all(
            &hub,
            ClientMessage::SetSessionBusyExcluded {
                session_id: "ghost".to_owned(),
                excluded: true,
            },
        )
        .await;
        assert!(
            renamed.is_empty(),
            "a rename of an unknown session: {renamed:?}"
        );
        assert!(
            excluded.is_empty(),
            "answers as a rename does: {excluded:?}"
        );
        assert!(hub.sessions.get("ghost").is_none(), "no session is made");
    }

    #[test]
    fn carried_busy_excluded_is_the_sources_flag_only_when_carried() {
        let (sessions, _dirs) = appearance_registry("busy-carry");
        sessions.update("s1", |rec| rec.busy_excluded = true);
        assert!(
            !carried_busy_excluded(&sessions, "s1", false),
            "a Duplicate starts counted"
        );
        assert!(
            carried_busy_excluded(&sessions, "s1", true),
            "a Restart or Resume keeps the flag"
        );
        assert!(!carried_busy_excluded(&sessions, "ghost", true));
    }

    #[tokio::test]
    async fn duplicate_keeps_a_deepseek_provider() {
        let (hub, scratch) = spawnless_test_hub("duplicate-deepseek");
        let target = SpawnTarget::Standalone {
            cwd: Some(scratch.path().to_string_lossy().into_owned()),
            add_dirs: Vec::new(),
        };
        let mut source = idle_record("source");
        source.spawn_config = Some(protocol::SpawnConfig::from_request(&deepseek_spawn(
            target,
            SessionMode::Interactive,
            Vec::new(),
            None,
        )));
        hub.sessions.insert(source);
        let request = duplicate_request(&hub, "source", false)
            .await
            .expect("source can be duplicated");
        assert_eq!(
            request.agent.map(|selection| selection.options),
            Some(deepseek_options())
        );
    }

    #[tokio::test]
    #[expect(clippy::panic, reason = "a wrong target shape fails the test loudly")]
    async fn restart_duplicate_keeps_the_source_branch() {
        let (hub, _scratch) = spawnless_test_hub("restart-same-checkout");
        let target = SpawnTarget::Single {
            repo_id: "r1".to_owned(),
            branch_name: "wt/silly-gecko".to_owned(),
            base_branch: Some("main".to_owned()),
            use_worktree: true,
            checkout_strategy: None,
            worktree_reuse: protocol::WorktreeReusePolicy::Reuse,
            existing_worktree: None,
        };
        let mut source = idle_record("src");
        source.spawn_config = Some(protocol::SpawnConfig::from_request(&env_spawn(
            target,
            SessionMode::Interactive,
            Vec::new(),
            None,
        )));
        hub.sessions.insert(source);
        let request = duplicate_request(&hub, "src", true)
            .await
            .expect("a restart reruns the source's config");
        match request.target {
            SpawnTarget::Single {
                branch_name,
                worktree_reuse,
                use_worktree,
                ..
            } => {
                assert_eq!(branch_name, "wt/silly-gecko", "the source's branch");
                assert_eq!(worktree_reuse, protocol::WorktreeReusePolicy::Reuse);
                assert!(use_worktree);
            }
            other => panic!("expected a single target, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_duplicate_carries_no_lineage() {
        let (hub, _scratch) = spawnless_test_hub("duplicate-no-lineage");
        let mut source = idle_record("src");
        source.spawn_config = Some(protocol::SpawnConfig::from_request(&env_spawn(
            SpawnTarget::Standalone {
                cwd: Some(hub.dirs.config.to_string_lossy().into_owned()),
                add_dirs: Vec::new(),
            },
            SessionMode::Interactive,
            Vec::new(),
            None,
        )));
        source.resumed_from = vec!["predecessor-of-src".to_owned()];
        hub.sessions.insert(source);

        for same_checkout in [false, true] {
            let request = duplicate_request(&hub, "src", same_checkout)
                .await
                .expect("a duplicate reruns the source's config");
            let json = serde_json::to_string(&request).expect("encode the request");
            assert!(
                !json.contains("predecessor-of-src"),
                "same_checkout {same_checkout}: {json}"
            );
        }
        assert!(
            crate::server::spawn::SpawnExtras::default()
                .resumed_from
                .is_empty(),
            "and the plain spawn path a duplicate takes names no predecessor"
        );
    }

    /// A Restart or Resume of a Claude session continues its conversation
    /// while the transcript exists; a Duplicate never does.
    #[tokio::test]
    async fn restart_duplicate_resumes_the_conversation_while_its_transcript_exists() {
        let (hub, scratch) = spawnless_test_hub("restart-resume");
        let folder = scratch.path().join("work").to_string_lossy().into_owned();
        let target = SpawnTarget::Standalone {
            cwd: Some(folder.clone()),
            add_dirs: Vec::new(),
        };
        let home = scratch.path().join("claude-home");
        let mut source = idle_record("src");
        source.conversation_id = Some("conv-1".to_owned());
        source.spawn_config = Some(protocol::SpawnConfig::from_request(&env_spawn(
            target,
            SessionMode::Interactive,
            vec![("CLAUDE_CONFIG_DIR".to_owned(), home.display().to_string())],
            None,
        )));
        hub.sessions.insert(source);
        let resumed = |same_checkout| {
            let hub = &hub;
            async move {
                duplicate_request(hub, "src", same_checkout)
                    .await
                    .expect("src can be rerun")
                    .resume_conversation
            }
        };

        assert_eq!(
            resumed(true).await,
            None,
            "no transcript: a fresh conversation"
        );
        let project = home
            .join("projects")
            .join(agent_claude::encode_project_dir(&folder));
        std::fs::create_dir_all(&project).expect("create the transcript folder");
        std::fs::write(project.join("conv-1.jsonl"), "{}\n").expect("write the transcript");
        assert_eq!(resumed(true).await.as_deref(), Some("conv-1"));
        assert_eq!(resumed(false).await, None, "a Duplicate starts fresh");
    }

    fn accent(color: &str) -> AppearanceOverrides {
        AppearanceOverrides {
            accent_color: Some(color.to_string()),
            ..AppearanceOverrides::default()
        }
    }

    /// The accent and `request_id` of every message on `rx` before the
    /// sentinel, which must all be `SessionUpdated`s.
    async fn updates_before_sentinel(
        rx: &mut mpsc::UnboundedReceiver<DaemonMessage>,
    ) -> Vec<(Option<String>, Option<String>)> {
        messages_before_sentinel(rx)
            .await
            .into_iter()
            .map(|msg| match msg {
                DaemonMessage::SessionUpdated {
                    session,
                    request_id,
                } => (session.appearance.accent_color, request_id),
                other => unreachable!("only SessionUpdated messages arrive, got {other:?}"),
            })
            .collect()
    }

    #[tokio::test]
    async fn session_appearance_echoes_the_request_id_in_order_to_the_requester_only() {
        let (sessions, _dirs) = appearance_registry("echo");
        let (out_tx, mut requester) = mpsc::unbounded_channel();
        let (other_tx, mut other) = mpsc::unbounded_channel();
        let forwarders = [
            spawn_session_forwarder(Arc::clone(&sessions), out_tx.clone(), 1),
            spawn_session_forwarder(Arc::clone(&sessions), other_tx, 2),
        ];

        sessions.update("s1", |guard| {
            guard.terminal_title = Some("busy".to_string());
        });
        for (color, id) in [("#38BDF8", "req-a"), ("#FB7185", "req-b")] {
            let change = AppearanceChangeRequest {
                session_id: "s1",
                appearance: &accent(color),
                request_id: Some(id.to_string()),
                connection: 1,
            };
            set_session_appearance(&sessions, change, &out_tx);
        }
        publish_sentinel(&sessions);

        let sky = Some("#38bdf8".to_string());
        let rose = Some("#fb7185".to_string());
        assert_eq!(
            updates_before_sentinel(&mut requester).await,
            [
                (None, None),
                (sky.clone(), Some("req-a".to_string())),
                (rose.clone(), Some("req-b".to_string())),
            ],
            "the requester's echoes follow the earlier broadcast, once each"
        );
        assert_eq!(
            updates_before_sentinel(&mut other).await,
            [(None, None), (sky, None), (rose, None)],
            "another connection sees the same updates without the id"
        );
        for forwarder in forwarders {
            forwarder.abort();
        }
    }

    #[tokio::test]
    async fn a_spawn_reply_reaches_the_requester_once_with_its_request_id() {
        let (sessions, _dirs) = appearance_registry("spawn-reply");
        let (requester_tx, mut requester) = mpsc::unbounded_channel();
        let (other_tx, mut other) = mpsc::unbounded_channel();
        let forwarders = [
            spawn_session_forwarder(Arc::clone(&sessions), requester_tx, 1),
            spawn_session_forwarder(Arc::clone(&sessions), other_tx, 2),
        ];

        // A spawn needs a real child, so this drives the seam the handler
        // uses: the origin it builds, published the way the interactive and
        // plain-shell spawn paths publish their record.
        let pending = sessions.insert_pending(idle_record("s2"));
        pending.publish_from(request_origin(1, Some("r1".to_string())));
        publish_sentinel(&sessions);

        assert_eq!(
            only_update(&mut requester).await,
            ("s2".to_string(), Some("r1".to_string())),
            "the requester hears of its spawn once, with its id"
        );
        assert_eq!(
            only_update(&mut other).await,
            ("s2".to_string(), None),
            "another connection hears of it once, without the id"
        );
        for forwarder in forwarders {
            forwarder.abort();
        }
    }

    #[tokio::test]
    async fn a_duplicate_reply_reaches_the_requester_once_with_its_request_id() {
        let (sessions, _dirs) = appearance_registry("duplicate-reply");
        let (other_tx, mut other) = mpsc::unbounded_channel();
        let (requester_tx, mut requester) = mpsc::unbounded_channel();
        let forwarders = [
            spawn_session_forwarder(Arc::clone(&sessions), other_tx, 1),
            spawn_session_forwarder(Arc::clone(&sessions), requester_tx, 2),
        ];

        // The duplicate's origin, published the way the headless spawn path
        // inserts its record.
        sessions.insert_from(idle_record("s2"), request_origin(2, Some("d1".to_string())));
        publish_sentinel(&sessions);
        assert_eq!(
            only_update(&mut requester).await,
            ("s2".to_string(), Some("d1".to_string())),
            "the requester hears of its duplicate once, with its id"
        );
        assert_eq!(only_update(&mut other).await, ("s2".to_string(), None));

        sessions.insert_from(idle_record("s3"), request_origin(2, None));
        publish_sentinel(&sessions);
        assert_eq!(
            only_update(&mut requester).await,
            ("s3".to_string(), None),
            "a request without an id has nothing to echo"
        );
        assert_eq!(only_update(&mut other).await, ("s3".to_string(), None));
        for forwarder in forwarders {
            forwarder.abort();
        }
    }

    #[tokio::test]
    async fn a_lagged_forwarder_resyncs_with_a_sessions_list() {
        let (sessions, _dirs) = appearance_registry("lagged");
        let (out_tx, mut rx) = mpsc::unbounded_channel();
        let forwarder = spawn_session_forwarder(Arc::clone(&sessions), out_tx, 1);

        // The current-thread runtime first polls the forwarder at the await
        // below, by which time the broadcast buffer has overflowed.
        for step in 0..=crate::session::EVENT_BROADCAST_CAPACITY {
            sessions.update("s1", |guard| {
                guard.terminal_title = Some(format!("step {step}"));
            });
        }
        let listed = match next_message(&mut rx).await {
            DaemonMessage::Sessions { sessions } => Some(sessions),
            _ => None,
        };
        assert_eq!(
            listed.expect("the lagged connection gets the session list"),
            sessions.snapshots(),
            "the list is the registry as it stands"
        );

        sessions.update("s1", |guard| {
            guard.terminal_title = Some("after".to_string());
        });
        let title = match next_message(&mut rx).await {
            DaemonMessage::SessionUpdated { session, .. } => Some(session.terminal_title),
            _ => None,
        };
        assert_eq!(
            title.expect("updates after the list still arrive"),
            Some("after".to_string()),
            "the missed updates are not replayed after the list"
        );
        forwarder.abort();
    }

    #[tokio::test]
    async fn a_lag_resends_the_spawn_reply_to_its_requester() {
        let (sessions, _dirs) = appearance_registry("lag-reply");
        let (out_tx, mut rx) = mpsc::unbounded_channel();
        let forwarder = spawn_session_forwarder(Arc::clone(&sessions), out_tx, 1);

        // Connection 1 spawns s2 and connection 2 spawns s3; the updates
        // after them push both replies out of connection 1's buffer.
        sessions
            .insert_pending(idle_record("s2"))
            .publish_from(request_origin(1, Some("r1".to_string())));
        sessions.insert_from(idle_record("s3"), request_origin(2, Some("r2".to_string())));
        for step in 0..=crate::session::EVENT_BROADCAST_CAPACITY {
            sessions.update("s1", |guard| {
                guard.terminal_title = Some(format!("step {step}"));
            });
        }

        let listed = match next_message(&mut rx).await {
            DaemonMessage::Sessions { sessions } => Some(sessions.len()),
            _ => None,
        };
        assert_eq!(listed, Some(3), "the resync lists every session first");
        // The forwarder sends the list and the resent replies in one go, so
        // the sentinel published now comes after all of them.
        publish_sentinel(&sessions);
        assert_eq!(
            only_update(&mut rx).await,
            ("s2".to_string(), Some("r1".to_string())),
            "the lost reply follows the list, to its requester only"
        );
        forwarder.abort();
    }

    #[test]
    fn a_discarded_pending_insert_leaves_no_trace() {
        let (sessions, _dirs) = appearance_registry("discard");
        let mut events = sessions.subscribe();

        sessions.insert_pending(idle_record("s2")).discard();

        assert!(sessions.get("s2").is_none(), "the record is gone");
        assert!(
            matches!(
                events.try_recv(),
                Err(broadcast::error::TryRecvError::Empty)
            ),
            "no client ever hears of a record that was never published"
        );
    }

    #[test]
    fn a_session_appearance_for_an_unknown_session_echoes_the_request_id_on_the_error() {
        let (sessions, _dirs) = appearance_registry("unknown");
        let mut other_client = sessions.subscribe();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        let change = AppearanceChangeRequest {
            session_id: "gone",
            appearance: &accent("#FB7185"),
            request_id: Some("req-11".to_string()),
            connection: 1,
        };

        set_session_appearance(&sessions, change, &out_tx);

        let reply = out_rx.try_recv().expect("the requester hears of the miss");
        assert!(
            matches!(
                &reply,
                DaemonMessage::Error { message, request_id: Some(id), .. }
                    if id == "req-11" && message == "unknown session: gone"
            ),
            "{reply:?}"
        );
        assert!(other_client.try_recv().is_err(), "nothing changed");
    }

    #[test]
    fn a_refused_session_appearance_echoes_the_request_id_on_the_error() {
        let (sessions, _dirs) = appearance_registry("refused");
        let mut other_client = sessions.subscribe();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        let change = AppearanceChangeRequest {
            session_id: "s1",
            appearance: &accent("blue"),
            request_id: Some("req-10".to_string()),
            connection: 1,
        };

        set_session_appearance(&sessions, change, &out_tx);

        let reply = out_rx.try_recv().expect("the requester hears the refusal");
        assert!(
            matches!(
                &reply,
                DaemonMessage::Error { message, request_id: Some(id), .. }
                    if id == "req-10" && message == "accent color must use #RRGGBB format"
            ),
            "{reply:?}"
        );
        assert!(other_client.try_recv().is_err(), "nothing changed");
    }
}
