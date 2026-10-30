//! Source control: the git read, write and stash messages, repo fetches, and worktree status and cleanup.
use super::connection::ConnCtx;
use super::{Hub, StateEvent};
use crate::{branch_names, file_fetch, git_inspect, git_write, spawn_plan};
use anyhow::{Context as _, anyhow};
use protocol::{ClientMessage, DaemonMessage, SessionMember};
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;
use tracing::warn;

/// Handle the git and source-control messages `dispatch` routes here.
#[expect(
    clippy::too_many_lines,
    reason = "one arm per routed message, moved verbatim from dispatch"
)]
pub(super) async fn handle(hub: &Hub, msg: ClientMessage, ctx: &ConnCtx<'_>) -> anyhow::Result<()> {
    let ConnCtx { out_tx, .. } = *ctx;
    match msg {
        ClientMessage::FetchRepo { repo_id } => {
            fetch_repo(hub, repo_id, out_tx).await;
        }
        ClientMessage::ListBranches { repo_id } => {
            let repo_path = hub
                .state
                .with_persisted(|s| {
                    s.repos
                        .iter()
                        .find(|r| r.id == repo_id)
                        .map(|r| r.path.clone())
                })
                .ok_or_else(|| anyhow!("unknown repo: {repo_id}"))?;
            let path = PathBuf::from(&repo_path);
            // A git failure here (repo moved, `git` off PATH, held index.lock)
            // renders as an empty branch list, which is indistinguishable from
            // a repo that genuinely has none. Degrading is right — the dialog
            // still opens — but it must leave a trail.
            let branches = crate::git::list_branches(&path)
                .await
                .unwrap_or_else(|err| {
                    warn!(?err, %repo_id, path = %path.display(), "list_branches failed; reporting none");
                    Vec::new()
                });
            let current = crate::git::current_branch(&path)
                .await
                .unwrap_or_else(|err| {
                    warn!(?err, %repo_id, path = %path.display(), "current_branch failed");
                    None
                });
            // Same degrade-but-leave-a-trail policy as the local list: a repo
            // with no remote legitimately has none, so an empty vec is not by
            // itself an error signal.
            let remote_branches = crate::git::list_remote_branches(&path)
                .await
                .unwrap_or_else(|err| {
                    warn!(?err, %repo_id, path = %path.display(), "list_remote_branches failed; reporting none");
                    Vec::new()
                });
            let _ = out_tx.send(DaemonMessage::Branches {
                repo_id,
                branches,
                current,
                remote_branches,
            });
        }
        ClientMessage::ListWorktrees { repo_id } => {
            let repo_path = hub
                .state
                .with_persisted(|s| {
                    s.repos
                        .iter()
                        .find(|r| r.id == repo_id)
                        .map(|r| r.path.clone())
                })
                .ok_or_else(|| anyhow!("unknown repo: {repo_id}"))?;
            let path = PathBuf::from(&repo_path);
            let raw = crate::git::list_worktrees(&path).await.unwrap_or_else(|err| {
                warn!(?err, %repo_id, path = %path.display(), "list_worktrees failed; reporting none");
                Vec::new()
            });
            let worktrees = annotate_worktrees(hub, raw);
            let _ = out_tx.send(DaemonMessage::Worktrees { repo_id, worktrees });
        }
        ClientMessage::SuggestBranchName { target, request_id } => {
            match branch_names::suggest(&hub.state, &hub.state.worktrees_dir(), &target).await {
                Ok(name) => {
                    let _ = out_tx.send(DaemonMessage::BranchNameSuggestion {
                        target,
                        name,
                        request_id,
                    });
                }
                // Route the failure to the request that asked, so its own wait
                // ends here rather than after its timeout too.
                Err(err) => {
                    let _ = out_tx.send(DaemonMessage::Error {
                        message: err.to_string(),
                        request_id,
                        code: Some(protocol::error_codes::GIT_FAILED.to_owned()),
                    });
                }
            }
        }
        ClientMessage::ListCommits {
            repo_id,
            branch,
            limit,
            offset,
            worktree_path,
            request_id,
        } => {
            reply_git_read(out_tx, &repo_id, request_id, "commit list", async {
                let path = repo_target_or_err(hub, &repo_id, worktree_path.as_deref())?;
                let commits =
                    git_inspect::list_commits(&path, branch.as_deref(), limit, offset).await?;
                Ok(DaemonMessage::Commits {
                    repo_id: repo_id.clone(),
                    commits,
                    offset,
                    worktree_path: worktree_path.clone(),
                })
            })
            .await;
        }
        ClientMessage::GetCommit {
            repo_id,
            sha,
            request_id,
        } => {
            reply_git_read(out_tx, &repo_id, request_id, "commit detail", async {
                let path = repo_path_or_err(hub, &repo_id)?;
                let detail = git_inspect::get_commit(&path, &sha).await?;
                Ok(DaemonMessage::CommitDetail {
                    repo_id: repo_id.clone(),
                    detail,
                })
            })
            .await;
        }
        ClientMessage::GetFileDiff {
            repo_id,
            path,
            against,
            worktree_path,
        } => {
            file_fetch::check_relative(&path)?;
            let repo = repo_target_or_err(hub, &repo_id, worktree_path.as_deref())?;
            let diff = git_inspect::file_diff(&repo, &path, against.as_deref()).await?;
            let _ = out_tx.send(DaemonMessage::FileDiff {
                repo_id,
                path,
                against,
                diff,
                worktree_path,
            });
        }
        ClientMessage::GetRemoteUrl {
            repo_id,
            request_id,
        } => {
            reply_git_read(out_tx, &repo_id, request_id, "remote url", async {
                let repo = repo_path_or_err(hub, &repo_id)?;
                let info = git_inspect::remote_url(&repo_id, &repo).await?;
                Ok(DaemonMessage::RemoteUrl(info))
            })
            .await;
        }
        ClientMessage::RepoStatus {
            repo_id,
            worktree_path,
            request_id,
        } => {
            reply_git_read(out_tx, &repo_id, request_id, "status", async {
                let repo = repo_target_or_err(hub, &repo_id, worktree_path.as_deref())?;
                let (index_changes, worktree_changes) = git_inspect::repo_status(&repo).await?;
                Ok(DaemonMessage::RepoStatus {
                    repo_id: repo_id.clone(),
                    index_changes,
                    worktree_changes,
                    worktree_path: worktree_path.clone(),
                })
            })
            .await;
        }
        ClientMessage::StageFiles {
            repo_id,
            paths,
            worktree_path,
        } => {
            handle_git_write(
                hub,
                out_tx,
                &repo_id,
                worktree_path.as_deref(),
                "stage",
                async {
                    let repo = repo_target_or_err(hub, &repo_id, worktree_path.as_deref())?;
                    git_write::stage(&repo, &paths).await?;
                    Ok(None)
                },
            )
            .await;
        }
        ClientMessage::UnstageFiles {
            repo_id,
            paths,
            worktree_path,
        } => {
            handle_git_write(
                hub,
                out_tx,
                &repo_id,
                worktree_path.as_deref(),
                "unstage",
                async {
                    let repo = repo_target_or_err(hub, &repo_id, worktree_path.as_deref())?;
                    git_write::unstage(&repo, &paths).await?;
                    Ok(None)
                },
            )
            .await;
        }
        ClientMessage::CommitRepo {
            repo_id,
            message,
            worktree_path,
        } => {
            handle_git_write(
                hub,
                out_tx,
                &repo_id,
                worktree_path.as_deref(),
                "commit",
                async {
                    let repo = repo_target_or_err(hub, &repo_id, worktree_path.as_deref())?;
                    let (sha, short_sha) = git_write::commit(&repo, &message).await?;
                    Ok(Some(DaemonMessage::CommitOk {
                        repo_id: repo_id.clone(),
                        sha,
                        short_sha,
                        worktree_path: worktree_path.clone(),
                    }))
                },
            )
            .await;
        }
        ClientMessage::DiscardChanges {
            repo_id,
            paths,
            worktree_path,
        } => {
            handle_git_write(
                hub,
                out_tx,
                &repo_id,
                worktree_path.as_deref(),
                "discard",
                async {
                    let repo = repo_target_or_err(hub, &repo_id, worktree_path.as_deref())?;
                    git_write::discard(&repo, &paths).await?;
                    Ok(None)
                },
            )
            .await;
        }
        ClientMessage::StashPush {
            repo_id,
            message,
            worktree_path,
        } => {
            handle_stash_write(
                hub,
                out_tx,
                &repo_id,
                worktree_path.as_deref(),
                "stash_push",
                async {
                    let repo = repo_target_or_err(hub, &repo_id, worktree_path.as_deref())?;
                    git_write::stash_push(&repo, &message).await
                },
            )
            .await;
        }
        ClientMessage::ListStashes {
            repo_id,
            worktree_path,
            request_id,
        } => {
            reply_git_read(out_tx, &repo_id, request_id, "stash list", async {
                let repo = repo_target_or_err(hub, &repo_id, worktree_path.as_deref())?;
                let stashes = git_write::stash_list(&repo).await?;
                Ok(DaemonMessage::Stashes {
                    repo_id: repo_id.clone(),
                    stashes,
                    worktree_path: worktree_path.clone(),
                })
            })
            .await;
        }
        ClientMessage::StashPop {
            repo_id,
            stash_id,
            worktree_path,
        } => {
            handle_stash_write(
                hub,
                out_tx,
                &repo_id,
                worktree_path.as_deref(),
                "stash_pop",
                async {
                    let repo = repo_target_or_err(hub, &repo_id, worktree_path.as_deref())?;
                    git_write::stash_pop(&repo, &stash_id).await
                },
            )
            .await;
        }
        ClientMessage::StashApply {
            repo_id,
            stash_id,
            worktree_path,
        } => {
            handle_stash_write(
                hub,
                out_tx,
                &repo_id,
                worktree_path.as_deref(),
                "stash_apply",
                async {
                    let repo = repo_target_or_err(hub, &repo_id, worktree_path.as_deref())?;
                    git_write::stash_apply(&repo, &stash_id).await
                },
            )
            .await;
        }
        ClientMessage::StashDrop {
            repo_id,
            stash_id,
            worktree_path,
        } => {
            handle_stash_write(
                hub,
                out_tx,
                &repo_id,
                worktree_path.as_deref(),
                "stash_drop",
                async {
                    let repo = repo_target_or_err(hub, &repo_id, worktree_path.as_deref())?;
                    git_write::stash_drop(&repo, &stash_id).await
                },
            )
            .await;
        }
        ClientMessage::GetFileSnapshot {
            id,
            repo_id,
            path,
            against,
            worktree_path,
        } => {
            let snapshot = match file_fetch::check_relative(&path)
                .and_then(|()| repo_target_or_err(hub, &repo_id, worktree_path.as_deref()))
            {
                Ok(repo) => git_inspect::file_snapshot(&repo, &path, against.as_deref()).await,
                Err(err) => Err(err),
            };
            match snapshot {
                Ok(content) => {
                    let _ = out_tx.send(DaemonMessage::FileSnapshot {
                        id,
                        repo_id,
                        path: path.clone(),
                        against,
                        old: content.old,
                        new: content.new,
                        language: git_inspect::language_for_path(&path).to_string(),
                        unavailable: content.unavailable,
                        worktree_path,
                    });
                }
                Err(err) => {
                    let _ = out_tx.send(DaemonMessage::FileSnapshotError {
                        id,
                        repo_id,
                        path,
                        against,
                        error: format!("{err:#}"),
                        worktree_path,
                    });
                }
            }
        }
        _ => warn!("git::handle: dispatch routed a message this module does not handle"),
    }
    Ok(())
}

pub(super) fn repo_path_or_err(hub: &Hub, repo_id: &str) -> anyhow::Result<PathBuf> {
    repo_target_or_err(hub, repo_id, None)
}

/// Resolve the working directory git operations should target for
/// `(repo_id, worktree_path)`. `worktree_path = None` falls back to the
/// registered repo path (current behavior). `worktree_path = Some(...)`
/// uses that path after validating (a) the repo is registered and (b)
/// the supplied path is either under the daemon's worktrees root *or*
/// equal to the registered repo's own path. The repo-path equality case
/// is what makes non-worktree sessions work: their `SessionMember::
/// worktree_path` literally equals the registered repo path, and the
/// sidebar passes that through verbatim. Allowing both forms keeps the
/// security invariant (no redirecting git to arbitrary directories)
/// while not breaking the natural data flow.
pub(super) fn repo_target_or_err(
    hub: &Hub,
    repo_id: &str,
    worktree_path: Option<&str>,
) -> anyhow::Result<PathBuf> {
    let registered_path = hub.state.with_persisted(|s| {
        s.repos
            .iter()
            .find(|r| r.id == repo_id)
            .map(|r| r.path.clone())
    });
    let Some(registered_path) = registered_path else {
        return Err(anyhow!("unknown repo: {repo_id}"));
    };
    let Some(wt) = worktree_path else {
        return Ok(PathBuf::from(registered_path));
    };
    let wt_buf = PathBuf::from(wt);
    let registered_buf = PathBuf::from(&registered_path);
    if wt_buf == registered_buf {
        return Ok(wt_buf);
    }
    let wt_root = hub.state.worktrees_dir();
    let canon_wt =
        std::fs::canonicalize(&wt_buf).with_context(|| format!("resolving worktree_path {wt}"))?;
    let canon_root = std::fs::canonicalize(&wt_root)
        .with_context(|| format!("resolving worktrees root {}", wt_root.display()))?;
    if canon_wt.starts_with(&canon_root) {
        return Ok(wt_buf);
    }
    Err(anyhow!(
        "worktree_path {wt} is not under the worktrees root {} and \
         does not match the registered repo path",
        wt_root.display()
    ))
}

/// Runs a git read (`RepoStatus`, `ListStashes`, `ListCommits`, `GetCommit`,
/// `GetRemoteUrl`) and puts the message `body` produces on the wire. A
/// failure becomes a `Error` sent to this requester only, carrying the
/// `request_id` it asked with, so the client can tell which read failed
/// instead of leaving its section stuck on "loading…".
async fn reply_git_read<F>(
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
    repo_id: &str,
    request_id: Option<String>,
    label: &str,
    body: F,
) where
    F: std::future::Future<Output = anyhow::Result<DaemonMessage>>,
{
    match body.await {
        Ok(msg) => {
            let _ = out_tx.send(msg);
        }
        Err(err) => {
            warn!(?err, repo_id, label, "git read failed");
            let _ = out_tx.send(DaemonMessage::Error {
                message: format!("{label} failed: {err:#}"),
                request_id,
                code: Some(protocol::error_codes::GIT_FAILED.to_owned()),
            });
        }
    }
}

/// Runs a stage/unstage/commit body, then on success broadcasts the fresh
/// `RepoStatus` to all connected clients (including this one) and emits the
/// optional follow-up message (e.g. `CommitOk`). On failure, sends
/// `GitWriteError { operation }` to the caller only — peer clients learn
/// nothing happened because the broadcast is never sent. A write that
/// succeeds but whose status refresh fails — or whose repo-path lookup
/// fails because the repo was unregistered mid-write — also answers the
/// caller with `GitWriteError`, so its pending state clears.
async fn handle_git_write<F>(
    hub: &Hub,
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
    repo_id: &str,
    worktree_path: Option<&str>,
    operation: &str,
    body: F,
) where
    F: std::future::Future<Output = anyhow::Result<Option<DaemonMessage>>>,
{
    let outcome = body.await;
    match outcome {
        Ok(follow_up) => {
            match repo_target_or_err(hub, repo_id, worktree_path) {
                Ok(repo) => match git_inspect::repo_status(&repo).await {
                    Ok((index_changes, worktree_changes)) => {
                        let _ = hub.state_events.send(StateEvent::RepoStatus {
                            repo_id: repo_id.to_string(),
                            worktree_path: worktree_path.map(str::to_owned),
                            index_changes,
                            worktree_changes,
                        });
                    }
                    Err(err) => {
                        warn!(?err, repo_id, "git_write: post-write status refresh failed");
                        send_post_write_error(
                            out_tx,
                            repo_id,
                            worktree_path,
                            operation,
                            "status refresh failed",
                            &err,
                        );
                    }
                },
                Err(err) => {
                    warn!(?err, repo_id, "git_write: repo path lookup failed");
                    send_post_write_error(
                        out_tx,
                        repo_id,
                        worktree_path,
                        operation,
                        "repo lookup failed",
                        &err,
                    );
                }
            }
            if let Some(msg) = follow_up {
                let _ = out_tx.send(msg);
            }
        }
        Err(err) => {
            warn!(operation, repo_id, ?err, "git_write: operation failed");
            let _ = out_tx.send(DaemonMessage::GitWriteError {
                repo_id: repo_id.to_string(),
                operation: operation.to_string(),
                error: format!("{err:#}"),
                worktree_path: worktree_path.map(str::to_owned),
            });
        }
    }
}

/// Tells the writer that its write landed but the step after it failed — the
/// status read, or the repo-path lookup when the repo was unregistered
/// mid-write — so a client waiting on that write for `operation` stops
/// waiting. `prefix` names which of them failed.
fn send_post_write_error(
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
    repo_id: &str,
    worktree_path: Option<&str>,
    operation: &str,
    prefix: &str,
    err: &anyhow::Error,
) {
    let _ = out_tx.send(DaemonMessage::GitWriteError {
        repo_id: repo_id.to_string(),
        operation: operation.to_string(),
        error: format!("{prefix}: {err:#}"),
        worktree_path: worktree_path.map(str::to_owned),
    });
}

/// Like [`handle_git_write`] but for stash mutations: in addition to the
/// post-write `RepoStatus` refresh, broadcasts a fresh `Stashes` snapshot
/// so every connected sidebar sees the new stash list without re-requesting.
async fn handle_stash_write<F>(
    hub: &Hub,
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
    repo_id: &str,
    worktree_path: Option<&str>,
    operation: &str,
    body: F,
) where
    F: std::future::Future<Output = anyhow::Result<()>>,
{
    let outcome = body.await;
    match outcome {
        Ok(()) => match repo_target_or_err(hub, repo_id, worktree_path) {
            Ok(repo) => {
                match git_inspect::repo_status(&repo).await {
                    Ok((index_changes, worktree_changes)) => {
                        let _ = hub.state_events.send(StateEvent::RepoStatus {
                            repo_id: repo_id.to_string(),
                            worktree_path: worktree_path.map(str::to_owned),
                            index_changes,
                            worktree_changes,
                        });
                    }
                    Err(err) => {
                        warn!(
                            ?err,
                            repo_id, "stash_write: post-write status refresh failed"
                        );
                        send_post_write_error(
                            out_tx,
                            repo_id,
                            worktree_path,
                            operation,
                            "status refresh failed",
                            &err,
                        );
                    }
                }
                match git_write::stash_list(&repo).await {
                    Ok(stashes) => {
                        let _ = hub.state_events.send(StateEvent::Stashes {
                            repo_id: repo_id.to_string(),
                            worktree_path: worktree_path.map(str::to_owned),
                            stashes,
                        });
                    }
                    Err(err) => {
                        warn!(?err, repo_id, "stash_write: stash_list refresh failed");
                    }
                }
            }
            Err(err) => {
                warn!(?err, repo_id, "stash_write: repo path lookup failed");
                send_post_write_error(
                    out_tx,
                    repo_id,
                    worktree_path,
                    operation,
                    "repo lookup failed",
                    &err,
                );
            }
        },
        Err(err) => {
            warn!(operation, repo_id, ?err, "stash_write: operation failed");
            let _ = out_tx.send(DaemonMessage::GitWriteError {
                repo_id: repo_id.to_string(),
                operation: operation.to_string(),
                error: format!("{err:#}"),
                worktree_path: worktree_path.map(str::to_owned),
            });
        }
    }
}

/// Decorate `git worktree list` output for the spawn dialog's existing-worktree
/// picker: whether a session holds each worktree, and — for the ones under the
/// RT worktrees root — which `wt.<slug>/` group they belong to plus its size
/// and last-modified. Worktrees created outside RT are listed too and are
/// equally launchable; they just carry no group metadata.
fn annotate_worktrees(hub: &Hub, raw: Vec<(String, PathBuf)>) -> Vec<protocol::WorktreeInfo> {
    let snapshots = hub.sessions.snapshots();
    let groups =
        crate::worktrees_admin::scan_root(&hub.state.worktrees_dir(), &hub.sessions, &hub.state);
    let mut group_by_member: std::collections::HashMap<String, &protocol::RootWorktreeEntry> =
        std::collections::HashMap::new();
    for entry in &groups {
        for member in &entry.members {
            group_by_member.insert(
                crate::paths::normalize_path_key(&member.worktree_path),
                entry,
            );
        }
    }
    raw.into_iter()
        .map(|(branch, path)| {
            let path = path.to_string_lossy().into_owned();
            let key = crate::paths::normalize_path_key(&path);
            let group = group_by_member.get(&key).copied();
            protocol::WorktreeInfo {
                branch,
                path,
                status: worktree_status(&snapshots, &key),
                group_path: group.map(|g| g.path.clone()),
                size_bytes: group.and_then(|g| g.size_bytes),
                last_modified_unix: group.and_then(|g| g.last_modified_unix),
            }
        })
        .collect()
}

/// Per-path counterpart of the per-group classification the worktrees manager
/// does: Active while a session is running in it, Detached while a stopped or
/// parked session still claims it, Stale when nothing references it.
fn worktree_status(
    snapshots: &[protocol::SessionSnapshot],
    key: &str,
) -> protocol::RootWorktreeStatus {
    let mut claimed_by_dead_session = false;
    for snap in snapshots {
        let uses_it = snap
            .worktree_paths
            .iter()
            .any(|p| crate::paths::normalize_path_key(p) == key)
            || snap
                .members
                .iter()
                .any(|m| crate::paths::normalize_path_key(&m.worktree_path) == key);
        if !uses_it {
            continue;
        }
        if crate::worktrees_admin::is_session_live(snap) {
            return protocol::RootWorktreeStatus::Active;
        }
        claimed_by_dead_session = true;
    }
    if claimed_by_dead_session {
        protocol::RootWorktreeStatus::Detached
    } else {
        protocol::RootWorktreeStatus::Stale
    }
}

/// Live (running, non-parked) sessions other than `exclude_id` whose recorded
/// worktree paths include `worktree_path`. Uses the same "active" filter as
/// `ListWorktrees` (not Stopped/Error, not parked), so a worktree offered for
/// reuse in the spawn dialog is never wrongly flagged as in-use. Returned as
/// `(session_id, label)` pairs.
pub(super) fn active_sessions_using_worktree(
    hub: &Hub,
    worktree_path: &str,
    exclude_id: Option<&str>,
) -> Vec<(String, String)> {
    let target = crate::paths::normalize_path_key(worktree_path);
    hub.sessions
        .snapshots()
        .into_iter()
        .filter(|s| exclude_id != Some(s.id.as_str()))
        .filter(|s| {
            !matches!(
                s.status,
                protocol::SessionStatus::Stopped | protocol::SessionStatus::Error
            ) && !s.is_inactive
        })
        .filter(|s| {
            s.worktree_paths
                .iter()
                .any(|p| crate::paths::normalize_path_key(p) == target)
                || s.members
                    .iter()
                    .any(|m| crate::paths::normalize_path_key(&m.worktree_path) == target)
        })
        .map(|s| (s.id.clone(), s.label.clone()))
        .collect()
}

/// If this discard would remove any worktree still bound to another live
/// session, returns the ready-to-send `ActionFailed` modal that refuses it
/// (and logs a warning); otherwise `None`, meaning the discard is safe.
/// Removing a shared worktree would yank the dir out from under that other
/// session's running child (the shared-worktree corruption).
pub(super) fn blocked_discard_message(
    hub: &Hub,
    session_id: &str,
    cleanup: &[protocol::CleanupAction],
    members: &[SessionMember],
) -> Option<DaemonMessage> {
    let mut blockers = Vec::new();
    for action in cleanup {
        if !action.remove_worktree {
            continue;
        }
        let Some(member) = members.iter().find(|m| m.repo_id == action.repo_id) else {
            continue;
        };
        let users = active_sessions_using_worktree(hub, &member.worktree_path, Some(session_id));
        if !users.is_empty() {
            let labels = users
                .iter()
                .map(|(_, label)| label.clone())
                .collect::<Vec<_>>()
                .join(", ");
            blockers.push(format!("{} (used by {labels})", member.worktree_path));
        }
    }
    if blockers.is_empty() {
        return None;
    }
    warn!(
        session_id,
        ?blockers,
        "discard_session: refused — worktree still in use by another live session"
    );
    Some(DaemonMessage::ActionFailed {
        title: "Can't remove worktree".to_string(),
        detail: format!(
            "This session's worktree is still in use by another live session, so removing it would break that session:\n\n{}",
            blockers.join("\n")
        ),
        hint: Some(
            "Close the other session(s) first, then remove the worktree — or discard this session while keeping the worktree."
                .to_string(),
        ),
        request_id: None,
    })
}

/// Build the `SpawnFailure` returned when a target worktree is already bound
/// to a live session — the collision that previously slipped through the
/// "worktree dir already present, skipping add" reuse and corrupted the other
/// session on discard.
pub(super) fn worktree_in_use_failure(
    branch_name: &str,
    users: &[(String, String)],
) -> spawn_plan::SpawnFailure {
    let labels = users
        .iter()
        .map(|(_, label)| label.clone())
        .collect::<Vec<_>>()
        .join(", ");
    spawn_plan::SpawnFailure {
        title: "Worktree already in use".to_string(),
        detail: format!(
            "The worktree for branch '{branch_name}' is already in use by another live session ({labels}). Two sessions can't share one worktree."
        ),
        hint: Some(
            "Pick a different branch name — the Random button on the spawn dialog generates a unique one — or close the other session first."
                .to_string(),
        ),
    }
}

/// Run a fetch for `repo_id` and report the outcome.
///
/// Failure is reported, never raised: a fetch is a freshness improvement, so
/// an offline machine or an auth-expired remote must still leave the spawn
/// dialog fully usable against cached refs.
async fn fetch_repo(hub: &Hub, repo_id: String, out_tx: &mpsc::UnboundedSender<DaemonMessage>) {
    let Some(repo_path) = hub.state.with_persisted(|s| {
        s.repos
            .iter()
            .find(|r| r.id == repo_id)
            .map(|r| r.path.clone())
    }) else {
        warn!(%repo_id, "fetch_repo: unknown repo");
        return;
    };
    let error = match crate::git::fetch(Path::new(&repo_path)).await {
        Ok(()) => None,
        Err(err) => {
            warn!(?err, %repo_id, "fetch_repo: fetch failed; previews fall back to cached refs");
            Some(format!("{err:#}"))
        }
    };
    let _ = out_tx.send(DaemonMessage::RepoFetched { repo_id, error });
}

/// Convert the per-platform [`crate::lock_finder`] output into the wire
/// protocol's `WorktreeLockingProcess` shape. Returned list is empty on
/// non-Windows platforms (the `lock_finder` stub returns an empty Vec).
pub(super) fn probe_locking_processes(member_path: &Path) -> Vec<protocol::WorktreeLockingProcess> {
    crate::lock_finder::processes_holding(&[member_path])
        .into_iter()
        .map(|p| protocol::WorktreeLockingProcess {
            pid: p.pid,
            name: p.name,
            cmdline: p.cmdline,
        })
        .collect()
}

/// Handler for [`ClientMessage::RetryWorktreeCleanup`]. Kills the listed
/// pids (graceful Restart Manager shutdown first, then hard kill), then
/// re-runs the robust cleanup against each target. Re-emits
/// [`DaemonMessage::WorktreeCleanupFailed`] for any member that's still
/// wedged so the user can iterate.
pub(super) async fn retry_worktree_cleanup(
    hub: &Hub,
    session_id: &str,
    kill_pids: &[u32],
    targets: &[protocol::RetryWorktreeTarget],
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
) {
    // Defence: refuse any target that isn't under the configured
    // worktrees root. A crafted message could otherwise be made to
    // delete an arbitrary directory the daemon has write access to.
    let root = hub.state.worktrees_dir();
    let root_canon = std::fs::canonicalize(&root).ok();
    let mut valid_targets: Vec<&protocol::RetryWorktreeTarget> = Vec::new();
    for target in targets {
        let canon = std::fs::canonicalize(&target.member_path).ok();
        let ok = match (&canon, &root_canon) {
            (Some(c), Some(r)) => c.starts_with(r),
            // If the path is gone already, count as success — nothing to do.
            (None, _) => {
                continue;
            }
            _ => false,
        };
        if !ok {
            warn!(
                target = %target.member_path,
                "retry_worktree_cleanup: refusing target outside worktrees root"
            );
            continue;
        }
        valid_targets.push(target);
    }

    if !kill_pids.is_empty() {
        let target_paths: Vec<&Path> = valid_targets
            .iter()
            .map(|t| Path::new(t.member_path.as_str()))
            .collect();
        crate::lock_finder::terminate_processes(&target_paths, kill_pids).await;
        // Brief grace period for the OS to release file handles after
        // the kill — without this, the immediate retry races handle
        // teardown the same way the original cleanup raced the kill.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    let mut member_parents: Vec<PathBuf> = Vec::new();
    let mut failures: Vec<protocol::WorktreeCleanupFailure> = Vec::new();
    for target in &valid_targets {
        let member = PathBuf::from(&target.member_path);
        match crate::worktree_cleanup::remove_member(Path::new(&target.repo_path), &member).await {
            crate::worktree_cleanup::CleanupOutcome::Removed => {}
            crate::worktree_cleanup::CleanupOutcome::StillOnDisk { reason } => {
                let locking_processes = probe_locking_processes(&member);
                failures.push(protocol::WorktreeCleanupFailure {
                    member_path: target.member_path.clone(),
                    repo_path: target.repo_path.clone(),
                    reason,
                    locking_processes,
                });
            }
        }
        if let Some(parent) = member.parent()
            && !member_parents.iter().any(|p| p == parent)
        {
            member_parents.push(parent.to_path_buf());
        }
    }
    for parent in &member_parents {
        crate::worktree_cleanup::prune_empty_ancestors(parent, &root);
    }

    if !failures.is_empty() {
        let _ = out_tx.send(DaemonMessage::WorktreeCleanupFailed {
            session_id: session_id.to_string(),
            // Session is already discarded; we don't have a label here.
            // Frontend should retain the original label from the first
            // failure event and reuse it across retries.
            session_label: session_id.to_string(),
            failures,
        });
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use crate::server::test_support::{
        ScratchDir, dispatch_one, drain, git_init, repo_entry, test_hub,
    };
    use protocol::{AppearanceOverrides, SuggestTarget};

    fn repo_target(repo_id: &str) -> SuggestTarget {
        SuggestTarget::Repo {
            repo_id: repo_id.to_string(),
        }
    }

    #[tokio::test]
    async fn a_branch_name_suggestion_echoes_its_request_id() {
        let (hub, scratch) = test_hub("suggest-rid");
        let repo = scratch.path().join("repo");
        std::fs::create_dir_all(&repo).expect("create repo dir");
        git_init(&repo);
        hub.state
            .mutate(|s| s.repos.push(repo_entry("r1", "r1", &repo)))
            .expect("register repo");
        let msg = dispatch_one(
            &hub,
            ClientMessage::SuggestBranchName {
                target: repo_target("r1"),
                request_id: Some("q1".to_string()),
            },
        )
        .await;
        assert!(
            matches!(
                &msg,
                DaemonMessage::BranchNameSuggestion { target, name, request_id: Some(id) }
                    if id == "q1" && *target == repo_target("r1") && !name.is_empty()
            ),
            "{msg:?}"
        );
    }

    #[tokio::test]
    async fn a_failed_branch_name_suggestion_answers_error_with_its_request_id() {
        let (hub, _scratch) = test_hub("suggest-fail-rid");
        let msg = dispatch_one(
            &hub,
            ClientMessage::SuggestBranchName {
                target: repo_target("no-such"),
                request_id: Some("q2".to_string()),
            },
        )
        .await;
        assert!(
            !matches!(&msg, DaemonMessage::BranchNameSuggestion { .. }),
            "a failed suggestion sends no name: {msg:?}"
        );
        assert!(
            matches!(
                &msg,
                DaemonMessage::Error { message, request_id: Some(id), code: Some(code) }
                    if id == "q2" && message.contains("unknown repo")
                        && code == protocol::error_codes::GIT_FAILED
            ),
            "{msg:?}"
        );
    }

    #[tokio::test]
    async fn a_branch_name_suggestion_without_a_request_id_answers_with_none() {
        let (hub, scratch) = test_hub("suggest-no-rid");
        let repo = scratch.path().join("repo");
        std::fs::create_dir_all(&repo).expect("create repo dir");
        git_init(&repo);
        hub.state
            .mutate(|s| s.repos.push(repo_entry("r1", "r1", &repo)))
            .expect("register repo");
        let ok = dispatch_one(
            &hub,
            ClientMessage::SuggestBranchName {
                target: repo_target("r1"),
                request_id: None,
            },
        )
        .await;
        assert!(
            matches!(
                &ok,
                DaemonMessage::BranchNameSuggestion {
                    request_id: None,
                    ..
                }
            ),
            "{ok:?}"
        );
        let failed = dispatch_one(
            &hub,
            ClientMessage::SuggestBranchName {
                target: repo_target("no-such"),
                request_id: None,
            },
        )
        .await;
        assert!(
            matches!(
                &failed,
                DaemonMessage::Error {
                    request_id: None,
                    ..
                }
            ),
            "{failed:?}"
        );
    }

    #[tokio::test]
    async fn a_failed_repo_status_read_echoes_its_request_id() {
        let (hub, _scratch) = test_hub("status-rid");
        let msg = dispatch_one(
            &hub,
            ClientMessage::RepoStatus {
                repo_id: "no-such-repo".to_string(),
                worktree_path: None,
                request_id: Some("q1".to_string()),
            },
        )
        .await;
        assert!(
            matches!(
                &msg,
                DaemonMessage::Error { message, request_id: Some(id), .. }
                    if id == "q1" && message.starts_with("status failed:")
            ),
            "{msg:?}"
        );
    }

    #[tokio::test]
    async fn a_failed_remote_url_read_echoes_its_request_id() {
        let (hub, scratch) = test_hub("remote-rid");
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
            })
            .expect("register repo");
        let msg = dispatch_one(
            &hub,
            ClientMessage::GetRemoteUrl {
                repo_id: "r1".to_string(),
                request_id: Some("q2".to_string()),
            },
        )
        .await;
        assert!(
            matches!(
                &msg,
                DaemonMessage::Error { message, request_id: Some(id), .. }
                    if id == "q2" && message.starts_with("remote url failed:")
            ),
            "{msg:?}"
        );
    }

    #[tokio::test]
    async fn a_snapshot_of_a_path_escaping_the_repo_answers_with_its_id() {
        let (hub, _scratch) = test_hub("snapshot-escape");
        let msg = dispatch_one(
            &hub,
            ClientMessage::GetFileSnapshot {
                id: "diff-snap-1".to_string(),
                repo_id: "r1".to_string(),
                path: "../outside.txt".to_string(),
                against: None,
                worktree_path: None,
            },
        )
        .await;
        assert!(
            matches!(
                &msg,
                DaemonMessage::FileSnapshotError { id, path, error, .. }
                    if id == "diff-snap-1" && path == "../outside.txt" && !error.is_empty()
            ),
            "{msg:?}"
        );
    }

    /// A hub with `r1` registered over a folder whose `.git` points at a
    /// git dir that does not exist, so every status read of it fails.
    fn hub_with_broken_repo(tag: &str) -> (Hub, ScratchDir) {
        let (hub, scratch) = test_hub(tag);
        let folder = scratch.path().join("broken");
        std::fs::create_dir_all(&folder).expect("create repo dir");
        std::fs::write(folder.join(".git"), "gitdir: missing-git-dir\n").expect("write .git file");
        hub.state
            .mutate(|s| {
                s.repos.push(protocol::RepoEntry {
                    id: "r1".to_string(),
                    name: "r1".to_string(),
                    path: folder.to_string_lossy().into_owned(),
                    default_branch: None,
                    default_use_worktree: true,
                    appearance: AppearanceOverrides::default(),
                    last_agent: None,
                    last_spawn_config: None,
                });
            })
            .expect("register repo");
        (hub, scratch)
    }

    #[tokio::test]
    async fn a_git_write_whose_status_refresh_fails_answers_with_a_write_error() {
        let (hub, _scratch) = hub_with_broken_repo("write-refresh");
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        handle_git_write(&hub, &out_tx, "r1", None, "stage", async { Ok(None) }).await;
        let messages = drain(&mut out_rx);
        assert!(
            matches!(
                messages.as_slice(),
                [DaemonMessage::GitWriteError { repo_id, operation, error, worktree_path: None }]
                    if repo_id == "r1"
                        && operation == "stage"
                        && error.starts_with("status refresh failed: ")
            ),
            "{messages:?}"
        );
    }

    #[tokio::test]
    async fn a_stash_write_whose_status_refresh_fails_answers_with_a_write_error() {
        let (hub, _scratch) = hub_with_broken_repo("stash-refresh");
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        handle_stash_write(&hub, &out_tx, "r1", None, "stash_push", async { Ok(()) }).await;
        let messages = drain(&mut out_rx);
        assert!(
            matches!(
                messages.as_slice(),
                [DaemonMessage::GitWriteError { repo_id, operation, error, worktree_path: None }]
                    if repo_id == "r1"
                        && operation == "stash_push"
                        && error.starts_with("status refresh failed: ")
            ),
            "{messages:?}"
        );
    }

    #[tokio::test]
    async fn a_git_write_whose_repo_is_gone_answers_with_a_write_error() {
        let (hub, _scratch) = test_hub("write-repo-gone");
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        handle_git_write(&hub, &out_tx, "gone", None, "stage", async { Ok(None) }).await;
        let messages = drain(&mut out_rx);
        assert!(
            matches!(
                messages.as_slice(),
                [DaemonMessage::GitWriteError { repo_id, operation, error, worktree_path: None }]
                    if repo_id == "gone"
                        && operation == "stage"
                        && error.starts_with("repo lookup failed: ")
            ),
            "{messages:?}"
        );
    }

    #[tokio::test]
    async fn a_stash_write_whose_repo_is_gone_answers_with_a_write_error() {
        let (hub, _scratch) = test_hub("stash-repo-gone");
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        handle_stash_write(&hub, &out_tx, "gone", None, "stash_push", async { Ok(()) }).await;
        let messages = drain(&mut out_rx);
        assert!(
            matches!(
                messages.as_slice(),
                [DaemonMessage::GitWriteError { repo_id, operation, error, worktree_path: None }]
                    if repo_id == "gone"
                        && operation == "stash_push"
                        && error.starts_with("repo lookup failed: ")
            ),
            "{messages:?}"
        );
    }

    /// A worktree nothing references is Stale; the picker uses that to tell a
    /// leftover apart from one a stopped session still owns.
    #[test]
    fn worktree_status_is_stale_without_any_session() {
        let status = worktree_status(&[], &crate::paths::normalize_path_key("X:/wt/repo"));
        assert!(matches!(status, protocol::RootWorktreeStatus::Stale));
    }
}
