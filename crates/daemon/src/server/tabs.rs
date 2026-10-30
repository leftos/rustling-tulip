//! Tabs and pane layouts: the tab and layout messages, diff and viewer tabs, and keeping layouts in step with sessions.
use super::connection::ConnCtx;
use super::git::repo_path_or_err;
use super::{Hub, StateEvent, TabEvent};
use crate::registry::{reorder_containers, set_session_order};
use chrono::Utc;
use protocol::{ClientMessage, DaemonMessage, InitLayoutKind, TabContent, TabEntry};
use tokio::sync::mpsc;
use tracing::{info, warn};

/// Handle the tab and layout messages `dispatch` routes here.
#[expect(
    clippy::too_many_lines,
    reason = "one arm per routed message, moved verbatim from dispatch"
)]
pub(super) fn handle(hub: &Hub, msg: ClientMessage, ctx: &ConnCtx<'_>) -> anyhow::Result<()> {
    let ConnCtx {
        out_tx,
        client_id,
        client_name,
        ..
    } = *ctx;
    match msg {
        ClientMessage::OpenDiffTab {
            id,
            repo_id,
            path,
            against,
            worktree_path,
            host,
        } => {
            let key = DiffTabKey {
                repo_id: &repo_id,
                path: &path,
                against: against.as_deref(),
                worktree_path: worktree_path.as_deref(),
                host: host.as_deref(),
            };
            let outcome = open_diff_tab(hub, client_id, &key);
            match outcome {
                Ok(outcome) => {
                    if let Some(new_tab) = outcome.created {
                        hub.emit_tab(client_id, TabEvent::Updated(new_tab));
                    }
                    let _ = out_tx.send(DaemonMessage::DiffTabOpened {
                        id,
                        tab_id: outcome.tab_id,
                    });
                }
                Err(err) => {
                    let _ = out_tx.send(DaemonMessage::Error {
                        message: format!("open diff tab failed: {err:#}"),
                        request_id: Some(id),
                        code: Some(protocol::error_codes::OPEN_FAILED.to_owned()),
                    });
                }
            }
        }
        ClientMessage::OpenViewerTab { id, path } => {
            let outcome = open_viewer_tab(hub, client_id, &path);
            match outcome {
                Ok(outcome) => {
                    if let Some(new_tab) = outcome.created {
                        hub.emit_tab(client_id, TabEvent::Updated(new_tab));
                    }
                    let _ = out_tx.send(DaemonMessage::ViewerTabOpened {
                        id,
                        tab_id: outcome.tab_id,
                    });
                }
                Err(err) => {
                    let _ = out_tx.send(DaemonMessage::Error {
                        message: format!("open viewer tab failed: {err:#}"),
                        request_id: Some(id),
                        code: Some(protocol::error_codes::OPEN_FAILED.to_owned()),
                    });
                }
            }
        }
        ClientMessage::OpenDashboardTab { id } => match open_dashboard_tab(hub, client_id) {
            Ok(outcome) => {
                if let Some(new_tab) = outcome.created {
                    hub.emit_tab(client_id, TabEvent::Updated(new_tab));
                }
                let _ = out_tx.send(DaemonMessage::DashboardTabOpened {
                    id,
                    tab_id: outcome.tab_id,
                });
            }
            Err(err) => {
                let _ = out_tx.send(DaemonMessage::Error {
                    message: format!("open dashboard tab failed: {err:#}"),
                    request_id: Some(id),
                    code: Some(protocol::error_codes::OPEN_FAILED.to_owned()),
                });
            }
        },
        ClientMessage::ListTabs => {
            let tabs = hub.state.client_layout(client_id);
            let _ = out_tx.send(DaemonMessage::Tabs { tabs });
        }
        ClientMessage::CreateTab {
            name,
            initial_session_id,
            host,
            request_id,
        } => {
            // Resolve the default name against the current tab list so
            // sequential opens produce `Tab`, `Tab 2`, `Tab 3`, ...
            let created = hub.state.mutate_client_layout(client_id, |tabs| {
                let tab = crate::tabs::make_tab(name, initial_session_id, host, tabs);
                tabs.push(tab.clone());
                tab
            });
            match created {
                Ok(new_tab) => hub.emit_tab(client_id, TabEvent::Updated(new_tab)),
                Err(err) => send_tab_failure(out_tx, &err, request_id),
            }
        }
        ClientMessage::CloseTab { tab_id } => {
            let removed = hub.state.mutate_client_layout(client_id, |tabs| {
                let prev = tabs.len();
                tabs.retain(|t| t.id != tab_id);
                prev != tabs.len()
            })?;
            if removed {
                hub.emit_tab(client_id, TabEvent::Removed(tab_id));
            }
        }
        ClientMessage::RestoreTab { tab, .. } | ClientMessage::RestoreTabSnapshot { tab }
            if tab.content == TabContent::Unknown =>
        {
            warn!(tab_id = %tab.id, "refusing to restore a tab of a kind this build does not know");
            let _ = out_tx.send(DaemonMessage::Error {
                message: "can't restore a tab this app does not understand".to_owned(),
                request_id: None,
                code: Some(protocol::error_codes::INVALID_REQUEST.to_owned()),
            });
        }
        ClientMessage::RestoreTab { tab, index } => {
            let restored = hub.state.try_mutate_client_layout(client_id, |tabs| {
                if holds_another_dashboard(tabs, &tab) {
                    return Ok(None);
                }
                let restored = crate::tabs::restore_tab(tabs, tab, index)?;
                let ordered_ids = tabs.iter().map(|t| t.id.clone()).collect();
                Ok::<_, anyhow::Error>(Some((restored, ordered_ids)))
            })?;
            if let Some((restored, ordered_ids)) = restored {
                hub.emit_tab(client_id, TabEvent::Updated(restored));
                hub.emit_tab(client_id, TabEvent::Reordered(ordered_ids));
            }
        }
        ClientMessage::RestoreTabSnapshot { tab } => {
            let restored = hub.state.try_mutate_client_layout(client_id, |tabs| {
                if holds_another_dashboard(tabs, &tab) {
                    return Ok(None);
                }
                crate::tabs::restore_tab_snapshot(tabs, tab).map(Some)
            })?;
            if let Some(restored) = restored {
                hub.emit_tab(client_id, TabEvent::Updated(restored));
            }
        }
        ClientMessage::RenameTab { tab_id, name } => {
            let updated = hub.state.mutate_client_layout(client_id, |tabs| {
                crate::tabs::find_tab_mut(tabs, &tab_id).map(|t| {
                    t.name = name;
                    t.clone()
                })
            })??;
            hub.emit_tab(client_id, TabEvent::Updated(updated));
        }
        ClientMessage::ReorderTabs {
            ordered_ids,
            request_id,
        } => {
            let reordered = hub.state.try_mutate_client_layout(client_id, |tabs| {
                crate::tabs::reorder_tabs(tabs, &ordered_ids)
            });
            match reordered {
                Ok(()) => hub.emit_tab(client_id, TabEvent::Reordered(ordered_ids)),
                Err(err) => send_tab_failure(out_tx, &err, request_id),
            }
        }
        ClientMessage::InitLayout { kind } => {
            // First-connect chooser reply: seed this client's brand-new layout
            // and send it the resulting tabs. CloneClient deep-copies another
            // client's layout with fresh pane ids (same sessions); AllSessions
            // builds one tab of panes over every running session.
            let clear_legacy = matches!(kind, InitLayoutKind::CloneLegacy);
            let tabs = match kind {
                InitLayoutKind::Empty | InitLayoutKind::Unknown => Vec::new(),
                InitLayoutKind::CloneLegacy => hub.state.legacy_tabs(),
                InitLayoutKind::CloneClient { client_id: source } => {
                    crate::tabs::clone_tabs_fresh_ids(&hub.state.client_layout(&source))
                }
                InitLayoutKind::AllSessions => build_all_sessions_layout(hub),
            };
            hub.state.set_client_layout(
                client_id,
                client_name.map(std::string::ToString::to_string),
                tabs,
            )?;
            if clear_legacy {
                hub.state.clear_legacy_tabs()?;
            }
            let _ = out_tx.send(DaemonMessage::Tabs {
                tabs: hub.state.client_layout(client_id),
            });
        }
        ClientMessage::ReorderContainers { ordered } => {
            let applied = reorder_containers(&hub.state, ordered)?;
            let _ = hub
                .state_events
                .send(StateEvent::ContainersReordered(applied));
        }
        ClientMessage::ReorderSessions {
            container_id,
            ordered_ids,
        } => {
            set_session_order(&hub.state, &container_id, ordered_ids.clone())?;
            let _ = hub.state_events.send(StateEvent::SessionsReordered {
                container_id,
                ordered_ids,
            });
        }
        ClientMessage::SplitPane {
            tab_id,
            pane_id,
            direction,
            place,
            new_session_id,
            host,
            request_id,
        } => {
            let updated = hub.state.try_mutate_client_layout(client_id, |tabs| {
                let tab = crate::tabs::find_tab_mut(tabs, &tab_id)?;
                let new_pane = crate::tabs::new_pane(new_session_id, host);
                crate::tabs::split_tab_pane(tab, &pane_id, direction, place, new_pane)?;
                Ok::<_, anyhow::Error>(tab.clone())
            });
            match updated {
                Ok(updated) => hub.emit_tab(client_id, TabEvent::Updated(updated)),
                Err(err) => send_tab_failure(out_tx, &err, request_id),
            }
        }
        ClientMessage::ClosePane {
            tab_id,
            pane_id,
            request_id,
        } => {
            let outcome = hub.state.try_mutate_client_layout(client_id, |tabs| {
                crate::tabs::close_pane_in_tabs(tabs, &tab_id, &pane_id)
            });
            match outcome {
                Ok(crate::tabs::ClosedTab::Removed(id)) => {
                    hub.emit_tab(client_id, TabEvent::Removed(id));
                }
                Ok(crate::tabs::ClosedTab::Updated(tab)) => {
                    hub.emit_tab(client_id, TabEvent::Updated(tab));
                }
                Err(err) => send_tab_failure(out_tx, &err, request_id),
            }
        }
        ClientMessage::SetPaneRatio {
            tab_id,
            split_path,
            ratio,
        } => {
            let updated = hub.state.mutate_client_layout(client_id, |tabs| {
                let tab = crate::tabs::find_tab_mut(tabs, &tab_id)?;
                crate::tabs::set_tab_pane_ratio(tab, &split_path, ratio)?;
                Ok::<_, anyhow::Error>(tab.clone())
            })??;
            hub.emit_tab(client_id, TabEvent::Updated(updated));
        }
        ClientMessage::ReplacePaneSession {
            tab_id,
            pane_id,
            session_id,
            host,
        } => {
            let updated = hub.state.mutate_client_layout(client_id, |tabs| {
                let tab = crate::tabs::find_tab_mut(tabs, &tab_id)?;
                let grid = crate::tabs::grid_or_err_mut(tab)?;
                crate::tabs::replace_pane_session(grid, &pane_id, session_id, host)?;
                Ok::<_, anyhow::Error>(tab.clone())
            })??;
            hub.emit_tab(client_id, TabEvent::Updated(updated));
        }
        ClientMessage::MovePane {
            src_tab_id,
            src_pane_id,
            dst_tab_id,
            dst_pane_id,
            edge,
            touch,
            request_id,
        } => {
            if src_pane_id == dst_pane_id {
                // Self-move is a no-op; don't mutate or broadcast.
            } else {
                let mv = crate::tabs::PaneMove {
                    src_tab_id: &src_tab_id,
                    src_pane_id: &src_pane_id,
                    dst_tab_id: &dst_tab_id,
                    dst_pane_id: &dst_pane_id,
                    edge,
                    touch,
                };
                match move_pane(hub, client_id, &mv) {
                    Ok(events) => {
                        for event in events {
                            hub.emit_tab(client_id, event);
                        }
                    }
                    Err(err) => send_tab_failure(out_tx, &err, request_id),
                }
            }
        }
        ClientMessage::SwapPanes {
            src_tab_id,
            src_pane_id,
            dst_tab_id,
            dst_pane_id,
            request_id,
        } => {
            if src_tab_id == dst_tab_id && src_pane_id == dst_pane_id {
                // A swap of a pane with itself is a no-op; don't mutate or
                // broadcast.
            } else {
                let swap = crate::tabs::PaneSwap {
                    src_tab_id: &src_tab_id,
                    src_pane_id: &src_pane_id,
                    dst_tab_id: &dst_tab_id,
                    dst_pane_id: &dst_pane_id,
                };
                let swapped = hub.state.try_mutate_client_layout(client_id, |tabs| {
                    crate::tabs::swap_panes(tabs, &swap)
                });
                match swapped {
                    Ok(changed) => {
                        for tab in changed {
                            hub.emit_tab(client_id, moved_event(tab));
                        }
                    }
                    Err(err) => send_tab_failure(out_tx, &err, request_id),
                }
            }
        }
        ClientMessage::RearrangeTab { tab_id, layout } => {
            let updated = hub.state.try_mutate_client_layout(client_id, |tabs| {
                let tab = crate::tabs::find_tab_mut(tabs, &tab_id)?;
                crate::tabs::pick_layout(tab, layout)?;
                Ok::<_, anyhow::Error>(tab.clone())
            })?;
            hub.emit_tab(client_id, TabEvent::Updated(updated));
        }
        ClientMessage::MergeTabs {
            tab_ids,
            name,
            layout,
            request_id,
        } => {
            let merged = hub.state.try_mutate_client_layout(client_id, |tabs| {
                crate::tabs::merge_tabs(tabs, &tab_ids, name, layout)
            });
            let new_tab = match merged {
                Ok(new_tab) => new_tab,
                Err(err) => {
                    send_tab_failure(out_tx, &err, request_id);
                    return Ok(());
                }
            };
            // The new tab first, so its panes are in a tab after each event.
            hub.emit_tab(client_id, TabEvent::Updated(new_tab));
            for id in tab_ids {
                hub.emit_tab(client_id, TabEvent::Removed(id));
            }
        }
        ClientMessage::ExtractToNewTab {
            source_tab_id,
            pane_ids,
            name,
            layout,
            request_id,
        } => {
            let extracted = hub.state.try_mutate_client_layout(client_id, |tabs| {
                crate::tabs::extract_to_new_tab(tabs, &source_tab_id, &pane_ids, name, layout)
            });
            let crate::tabs::ExtractOutcome { new_tab, source } = match extracted {
                Ok(extracted) => extracted,
                Err(err) => {
                    send_tab_failure(out_tx, &err, request_id);
                    return Ok(());
                }
            };
            // The new tab first, so its panes are in a tab after each event.
            hub.emit_tab(client_id, TabEvent::Updated(new_tab));
            if let Some(survivor) = source {
                hub.emit_tab(client_id, TabEvent::Updated(survivor));
            } else {
                hub.emit_tab(client_id, TabEvent::Removed(source_tab_id));
            }
        }
        ClientMessage::ForgetHostSessions {
            host,
            session_ids,
            all,
            request_id,
        } => {
            if let Err(err) = forget_host_panes(hub, &host, &session_ids, all) {
                send_tab_failure(out_tx, &err, request_id);
            }
        }
        _ => warn!("tabs::handle: dispatch routed a message this module does not handle"),
    }
    Ok(())
}

/// Ensure a layout exists for a non-chooser connection: an existing client
/// (refresh its name) or the shared legacy key (no `client_id`). Legacy
/// connections mirror the pre-per-client global layout without consuming it,
/// so a real client can still adopt it through the first-connect chooser
/// (`CloneLegacy`). Real clients with no layout never reach here — they go
/// through the chooser.
pub(super) fn ensure_client_layout(hub: &Hub, client_id: &str, client_name: Option<&str>) {
    if hub.state.has_client_layout(client_id) {
        if let Err(err) = hub
            .state
            .set_client_name(client_id, client_name.map(std::string::ToString::to_string))
        {
            warn!(?err, "failed to refresh client layout name");
        }
        return;
    }
    if let Err(err) = hub.state.set_client_layout(
        client_id,
        client_name.map(std::string::ToString::to_string),
        hub.state.legacy_tabs(),
    ) {
        warn!(?err, %client_id, "failed to initialize legacy client layout");
    }
}

/// Move a pane between (possibly same) tabs and produce the broadcast events
/// to fire afterwards. Performs extract + insert under a single fallible
/// mutation so an extract that is never followed by its insert — a missing
/// destination tab, an insert edge the grid can't take — rolls the layout back
/// instead of persisting a pane that vanished from both tabs.
fn move_pane(
    hub: &Hub,
    client_id: &str,
    mv: &crate::tabs::PaneMove<'_>,
) -> anyhow::Result<Vec<TabEvent>> {
    let moved = hub
        .state
        .try_mutate_client_layout(client_id, |tabs| crate::tabs::move_pane(tabs, mv))?;
    Ok(moved.into_iter().map(moved_event).collect())
}

/// The broadcast event for one tab a move or a swap changed.
fn moved_event(tab: crate::tabs::MovedTab) -> TabEvent {
    match tab {
        crate::tabs::MovedTab::Updated(tab) => TabEvent::Updated(tab),
        crate::tabs::MovedTab::Removed(id) => TabEvent::Removed(id),
    }
}

struct OpenTabOutcome {
    tab_id: String,
    /// `Some` when a new tab was created (caller should broadcast
    /// `TabUpdated`); `None` when an existing tab matched and we just
    /// focused it.
    created: Option<TabEntry>,
}

/// What identifies a diff tab: the same key opens the same tab.
struct DiffTabKey<'a> {
    repo_id: &'a str,
    path: &'a str,
    against: Option<&'a str>,
    worktree_path: Option<&'a str>,
    /// The host of the repo's daemon; `None` is this daemon.
    host: Option<&'a str>,
}

/// Find or create a diff tab keyed on [`DiffTabKey`]. Returns the tab id
/// either way so the client can activate it. A local diff tab needs its repo
/// registered here; a foreign one (`host` set) names a repo of another
/// daemon, which this one cannot check.
fn open_diff_tab(
    hub: &Hub,
    client_id: &str,
    key: &DiffTabKey<'_>,
) -> anyhow::Result<OpenTabOutcome> {
    let &DiffTabKey {
        repo_id,
        path,
        against,
        worktree_path,
        host,
    } = key;
    if host.is_none() {
        repo_path_or_err(hub, repo_id)?;
    }
    let against_owned = against.map(str::to_string);
    let worktree_path_owned = worktree_path.map(str::to_string);
    let outcome: anyhow::Result<OpenTabOutcome> =
        hub.state.mutate_client_layout(client_id, |tabs| {
            if let Some(existing) = tabs.iter().find(|t| {
                matches!(
                    &t.content,
                    TabContent::Diff {
                        repo_id: tab_repo,
                        path: tab_path,
                        against: tab_against,
                        worktree_path: tab_worktree,
                        host: tab_host,
                    } if tab_repo == repo_id
                        && tab_path == path
                        && tab_against.as_deref() == against
                        && tab_worktree.as_deref() == worktree_path
                        && tab_host.as_deref() == host
                )
            }) {
                return Ok(OpenTabOutcome {
                    tab_id: existing.id.clone(),
                    created: None,
                });
            }
            let new_tab = TabEntry {
                id: uuid::Uuid::new_v4().to_string(),
                name: diff_tab_name(path, against_owned.as_deref()),
                content: TabContent::Diff {
                    repo_id: repo_id.to_string(),
                    path: path.to_string(),
                    against: against_owned.clone(),
                    worktree_path: worktree_path_owned.clone(),
                    host: host.map(str::to_string),
                },
                created_at: chrono::Utc::now(),
                picked_layout: None,
            };
            let id = new_tab.id.clone();
            tabs.push(new_tab.clone());
            Ok(OpenTabOutcome {
                tab_id: id,
                created: Some(new_tab),
            })
        })?;
    outcome
}

/// Find or create a viewer tab keyed on its path. Returns the tab id either
/// way so the client can activate it.
fn open_viewer_tab(hub: &Hub, client_id: &str, path: &str) -> anyhow::Result<OpenTabOutcome> {
    anyhow::ensure!(!path.trim().is_empty(), "the path is empty");
    let outcome: anyhow::Result<OpenTabOutcome> =
        hub.state.mutate_client_layout(client_id, |tabs| {
            if let Some(existing) = tabs
                .iter()
                .find(|t| matches!(&t.content, TabContent::Viewer { path: p } if p == path))
            {
                return Ok(OpenTabOutcome {
                    tab_id: existing.id.clone(),
                    created: None,
                });
            }
            let new_tab = TabEntry {
                id: uuid::Uuid::new_v4().to_string(),
                name: viewer_tab_name(path),
                content: TabContent::Viewer {
                    path: path.to_string(),
                },
                created_at: Utc::now(),
                picked_layout: None,
            };
            let id = new_tab.id.clone();
            tabs.push(new_tab.clone());
            Ok(OpenTabOutcome {
                tab_id: id,
                created: Some(new_tab),
            })
        })?;
    outcome
}

/// Whether restoring `tab` would give the layout `tabs` a second Dashboard:
/// `tab` is a Dashboard and another one is listed. Such a restore is
/// skipped, leaving the listed Dashboard, which an undo on the client
/// focuses in its place; logged, since a restore has no reply.
fn holds_another_dashboard(tabs: &[TabEntry], tab: &TabEntry) -> bool {
    let another = matches!(tab.content, TabContent::Dashboard)
        && tabs
            .iter()
            .any(|t| t.id != tab.id && matches!(t.content, TabContent::Dashboard));
    if another {
        info!(tab_id = %tab.id, "not restoring a second Dashboard tab; the listed one stays");
    }
    another
}

/// Find or create the client's one Dashboard tab. Returns the tab id either
/// way so the client can activate it.
fn open_dashboard_tab(hub: &Hub, client_id: &str) -> anyhow::Result<OpenTabOutcome> {
    hub.state.mutate_client_layout(client_id, |tabs| {
        if let Some(existing) = tabs
            .iter()
            .find(|t| matches!(t.content, TabContent::Dashboard))
        {
            return OpenTabOutcome {
                tab_id: existing.id.clone(),
                created: None,
            };
        }
        let new_tab = TabEntry {
            id: uuid::Uuid::new_v4().to_string(),
            name: "Dashboard".to_owned(),
            content: TabContent::Dashboard,
            created_at: Utc::now(),
            picked_layout: None,
        };
        let id = new_tab.id.clone();
        tabs.push(new_tab.clone());
        OpenTabOutcome {
            tab_id: id,
            created: Some(new_tab),
        }
    })
}

/// A viewer tab is named after the file it shows; a path ending in a
/// separator keeps the whole path as its name.
fn viewer_tab_name(path: &str) -> String {
    let basename = path.rsplit(['/', '\\']).next().unwrap_or(path);
    if basename.is_empty() {
        path.to_string()
    } else {
        basename.to_string()
    }
}

fn diff_tab_name(path: &str, against: Option<&str>) -> String {
    let basename = path.rsplit(['/', '\\']).next().unwrap_or(path);
    match against {
        None => format!("{basename} (changes)"),
        Some("HEAD") => format!("{basename} (staged)"),
        Some(rev) => format!("{basename} @ {}", short_rev(rev)),
    }
}

fn short_rev(rev: &str) -> &str {
    if rev.len() > 7 { &rev[..7] } else { rev }
}

/// Answers a failed tab-layout request (`CreateTab`, `SplitPane`, `ClosePane`,
/// `MergeTabs`, `MovePane`, `SwapPanes`, `ReorderTabs`, `ExtractToNewTab`,
/// `ForgetHostSessions`) with
/// an `Error` to this requester only, carrying the `request_id` it asked with
/// so the client can drop what it armed for it.
fn send_tab_failure(
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
    err: &anyhow::Error,
    request_id: Option<String>,
) {
    warn!(?err, "tab layout request failed");
    let _ = out_tx.send(DaemonMessage::Error {
        message: err.to_string(),
        request_id,
        code: Some(protocol::error_codes::TAB_REFUSED.to_owned()),
    });
}

/// Build a one-tab layout with a pane per currently-running session — the
/// "open all active sessions" first-connect choice. Empty when no sessions
/// exist (the client starts with no tabs).
fn build_all_sessions_layout(hub: &Hub) -> Vec<TabEntry> {
    let panes: Vec<crate::tabs::PaneRef> = hub
        .sessions
        .snapshots()
        .into_iter()
        .map(|s| crate::tabs::PaneRef {
            pane_id: uuid::Uuid::new_v4().to_string(),
            session_id: Some(s.id),
            host: None,
        })
        .collect();
    let Some(grid) = crate::tabs::build_grid(&panes, 0, None) else {
        return Vec::new();
    };
    vec![TabEntry {
        id: uuid::Uuid::new_v4().to_string(),
        name: "All sessions".to_string(),
        content: TabContent::Grid { grid },
        created_at: Utc::now(),
        picked_layout: None,
    }]
}

/// Auto-close every local pane bound to `session_id` across all client layouts
/// when the session is removed: a pane's sibling fills its space, and a tab
/// whose last pane closed is removed entirely. A surviving tab untouched since
/// its layout was picked is rebuilt in that layout, by each client's own flag.
/// Emits scoped `TabUpdated` (survivors) / `TabRemoved` (emptied tabs) per
/// affected client.
pub(super) fn close_session_panes(hub: &Hub, session_id: &str) {
    if let Err(err) = close_panes_in_all_layouts(hub, |tabs| {
        crate::tabs::close_session_panes(tabs, session_id)
    }) {
        warn!(?err, "close_session_panes: state.mutate failed");
    }
}

/// [`close_session_panes`] for another daemon's sessions: close, across all
/// client layouts, every pane of `host` whose session is in `session_ids`, or
/// every pane of `host` when `all` is set. No matching pane emits nothing.
fn forget_host_panes(
    hub: &Hub,
    host: &str,
    session_ids: &[String],
    all: bool,
) -> anyhow::Result<()> {
    close_panes_in_all_layouts(hub, |tabs| {
        crate::tabs::forget_host_panes(tabs, host, session_ids, all)
    })
}

/// Run `close` on every client's layout, then emit each client's scoped
/// `TabUpdated` / `TabRemoved` events for what it closed.
fn close_panes_in_all_layouts(
    hub: &Hub,
    close: impl Fn(&mut Vec<TabEntry>) -> crate::tabs::SessionCloseResult,
) -> anyhow::Result<()> {
    // Per-client outcomes: a surviving tab to re-broadcast, or a tab id whose
    // last pane closed (already removed from that client's layout).
    enum Outcome {
        Updated(String, TabEntry),
        Removed(String, String),
    }
    let outcomes = hub.state.mutate_all_layouts(|layouts| {
        let mut out: Vec<Outcome> = Vec::new();
        for (cid, layout) in layouts.iter_mut() {
            let result = close(&mut layout.tabs);
            for tab in result.updated {
                out.push(Outcome::Updated(cid.clone(), tab));
            }
            for tab_id in result.removed_tab_ids {
                out.push(Outcome::Removed(cid.clone(), tab_id));
            }
        }
        out
    })?;
    for outcome in outcomes {
        match outcome {
            Outcome::Updated(cid, tab) => hub.emit_tab(&cid, TabEvent::Updated(tab)),
            Outcome::Removed(cid, tab_id) => hub.emit_tab(&cid, TabEvent::Removed(tab_id)),
        }
    }
    Ok(())
}

/// Rebind every pane bound to `old_id` to `new_id` across every client's
/// layout, broadcasting a scoped `TabUpdated` per affected tab. Used by
/// `resume_abandoned` so a resumed session reattaches to the pane its
/// abandoned predecessor occupied instead of orphaning into the sidebar.
pub(super) fn rebind_session_panes(hub: &Hub, old_id: &str, new_id: &str) {
    let outcomes = match hub.state.mutate_all_layouts(|layouts| {
        let mut out: Vec<(String, TabEntry)> = Vec::new();
        for (cid, layout) in layouts.iter_mut() {
            for tab in crate::tabs::rebind_session(&mut layout.tabs, old_id, new_id) {
                out.push((cid.clone(), tab));
            }
        }
        out
    }) {
        Ok(list) => list,
        Err(err) => {
            warn!(?err, "rebind_session_panes: state.mutate failed");
            return;
        }
    };
    for (cid, tab) in outcomes {
        hub.emit_tab(&cid, TabEvent::Updated(tab));
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use crate::server::connection::{AuthKind, FetchRegistry, PtyLane};
    use crate::server::dispatch;
    use crate::server::test_support::{dispatch_one, drain, seed_two_pane_layout, test_hub};
    use crate::state::AppState;
    use protocol::PaneDropEdge;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::{Mutex as AsyncMutex, broadcast};
    fn merge_of_one(request_id: Option<&str>) -> ClientMessage {
        ClientMessage::MergeTabs {
            tab_ids: vec!["t1".to_string()],
            name: None,
            layout: protocol::MergeLayout::TileVertical,
            request_id: request_id.map(str::to_owned),
        }
    }

    #[tokio::test]
    async fn a_rejected_merge_echoes_its_request_id() {
        let (hub, _scratch) = test_hub("merge-rid");
        seed_two_pane_layout(&hub, "c1");
        let msg = dispatch_one(&hub, merge_of_one(Some("m1"))).await;
        assert!(
            matches!(
                &msg,
                DaemonMessage::Error { message, request_id: Some(id), code: Some(code) }
                    if id == "m1" && message.contains("at least two tabs")
                        && code == protocol::error_codes::TAB_REFUSED
            ),
            "{msg:?}"
        );
    }

    #[tokio::test]
    async fn an_id_less_merge_failure_still_answers_without_one() {
        let (hub, _scratch) = test_hub("merge-no-rid");
        seed_two_pane_layout(&hub, "c1");
        let msg = dispatch_one(&hub, merge_of_one(None)).await;
        assert!(
            matches!(
                &msg,
                DaemonMessage::Error { message, request_id: None, .. }
                    if message.contains("at least two tabs")
            ),
            "{msg:?}"
        );
    }

    #[tokio::test]
    async fn a_rolled_back_move_pane_echoes_its_request_id() {
        let (hub, _scratch) = test_hub("move-rid");
        seed_two_pane_layout(&hub, "c1");
        let before = hub.state.client_layout("c1");
        let msg = dispatch_one(
            &hub,
            ClientMessage::MovePane {
                src_tab_id: "t1".to_string(),
                src_pane_id: "p1".to_string(),
                dst_tab_id: "ghost-tab".to_string(),
                dst_pane_id: "p2".to_string(),
                edge: PaneDropEdge::Right,
                touch: false,
                request_id: Some("v1".to_string()),
            },
        )
        .await;
        assert!(
            matches!(
                &msg,
                DaemonMessage::Error { message, request_id: Some(id), .. }
                    if id == "v1" && message.contains("ghost-tab")
            ),
            "{msg:?}"
        );
        assert_eq!(
            hub.state.client_layout("c1"),
            before,
            "the move rolled back"
        );
    }

    #[tokio::test]
    async fn a_rejected_split_echoes_its_request_id() {
        let (hub, _scratch) = test_hub("split-rid");
        seed_two_pane_layout(&hub, "c1");
        let msg = dispatch_one(
            &hub,
            ClientMessage::SplitPane {
                tab_id: "ghost-tab".to_string(),
                pane_id: "p1".to_string(),
                direction: protocol::SplitDirection::Horizontal,
                place: protocol::SplitPlace::Second,
                new_session_id: None,
                host: None,
                request_id: Some("s1".to_string()),
            },
        )
        .await;
        assert!(
            matches!(
                &msg,
                DaemonMessage::Error { message, request_id: Some(id), .. }
                    if id == "s1" && message.contains("ghost-tab")
            ),
            "{msg:?}"
        );
    }

    #[tokio::test]
    async fn a_rejected_extract_echoes_its_request_id() {
        let (hub, _scratch) = test_hub("extract-rid");
        seed_two_pane_layout(&hub, "c1");
        let msg = dispatch_one(
            &hub,
            ClientMessage::ExtractToNewTab {
                source_tab_id: "ghost-tab".to_string(),
                pane_ids: vec!["p1".to_string()],
                name: None,
                layout: None,
                request_id: Some("x1".to_string()),
            },
        )
        .await;
        assert!(
            matches!(
                &msg,
                DaemonMessage::Error { message, request_id: Some(id), .. }
                    if id == "x1" && message.contains("ghost-tab")
            ),
            "{msg:?}"
        );
    }

    #[tokio::test]
    async fn a_refused_close_pane_echoes_its_request_id() {
        let (hub, _scratch) = test_hub("close-pane-rid");
        seed_two_pane_layout(&hub, "c1");
        let msg = dispatch_one(
            &hub,
            ClientMessage::ClosePane {
                tab_id: "t1".to_string(),
                pane_id: "ghost-pane".to_string(),
                request_id: Some("r1".to_string()),
            },
        )
        .await;
        assert!(
            matches!(
                &msg,
                DaemonMessage::Error { message, request_id: Some(id), .. }
                    if id == "r1" && message.contains("ghost-pane")
            ),
            "{msg:?}"
        );
    }

    #[tokio::test]
    async fn opening_a_diff_tab_of_an_unknown_repo_answers_with_its_request_id() {
        let (hub, _scratch) = test_hub("open-diff-unknown");
        let msg = dispatch_one(
            &hub,
            ClientMessage::OpenDiffTab {
                id: "diff-open-1".to_string(),
                repo_id: "no-such-repo".to_string(),
                path: "src/a.rs".to_string(),
                against: None,
                worktree_path: None,
                host: None,
            },
        )
        .await;
        assert!(
            matches!(
                &msg,
                DaemonMessage::Error { message, request_id: Some(id), .. }
                    if id == "diff-open-1" && message.contains("unknown repo")
            ),
            "{msg:?}"
        );
    }

    #[tokio::test]
    async fn a_foreign_diff_tab_skips_the_repo_check() {
        let (hub, _scratch) = test_hub("open-diff-foreign");
        let open = || ClientMessage::OpenDiffTab {
            id: "diff-open-2".to_string(),
            repo_id: "a-repo-of-devbox".to_string(),
            path: "src/a.rs".to_string(),
            against: None,
            worktree_path: None,
            host: Some("ssh:devbox".to_string()),
        };
        let msg = dispatch_one(&hub, open()).await;
        let DaemonMessage::DiffTabOpened { id, tab_id } = &msg else {
            unreachable!("a foreign diff tab opens without a local repo: {msg:?}");
        };
        assert_eq!(id, "diff-open-2");
        let tabs = hub.state.client_layout("c1");
        assert_eq!(tabs.len(), 1, "{tabs:?}");
        assert_eq!(&tabs[0].id, tab_id);
        assert!(
            matches!(
                &tabs[0].content,
                TabContent::Diff { repo_id, host: Some(host), .. }
                    if repo_id == "a-repo-of-devbox" && host == "ssh:devbox"
            ),
            "{:?}",
            tabs[0].content
        );

        let again = dispatch_one(&hub, open()).await;
        assert!(
            matches!(&again, DaemonMessage::DiffTabOpened { tab_id: same, .. } if same == tab_id),
            "the same foreign diff focuses the open tab: {again:?}"
        );
        assert_eq!(hub.state.client_layout("c1").len(), 1);
    }

    fn open_viewer(id: &str, path: &str) -> ClientMessage {
        ClientMessage::OpenViewerTab {
            id: id.to_string(),
            path: path.to_string(),
        }
    }

    #[tokio::test]
    async fn opening_the_same_viewer_path_twice_focuses_one_tab() {
        let (hub, _scratch) = test_hub("open-viewer-twice");
        let mut events = hub.tab_events.subscribe();

        let first = dispatch_one(&hub, open_viewer("v1", "C:\\notes\\a.md")).await;
        let (id, tab_id) = match first {
            DaemonMessage::ViewerTabOpened { id, tab_id } => (id, tab_id),
            other => unreachable!("the first open answers with its tab id: {other:?}"),
        };
        assert_eq!(id, "v1");
        assert_eq!(
            take_tab_events(&mut events).len(),
            1,
            "the created tab is broadcast once"
        );

        let second = dispatch_one(&hub, open_viewer("v2", "C:\\notes\\a.md")).await;
        let (id, focused) = match second {
            DaemonMessage::ViewerTabOpened { id, tab_id } => (id, tab_id),
            other => unreachable!("the second open answers with its tab id: {other:?}"),
        };
        assert_eq!(id, "v2");
        assert_eq!(focused, tab_id, "the same path focuses the existing tab");
        assert!(
            take_tab_events(&mut events).is_empty(),
            "focusing broadcasts nothing"
        );

        let layout = hub.state.client_layout("c1");
        assert_eq!(layout.len(), 1, "one tab for one path: {layout:?}");
        assert!(
            matches!(&layout[0].content, TabContent::Viewer { path } if path == "C:\\notes\\a.md"),
            "{layout:?}"
        );
    }

    fn open_dashboard(id: &str) -> ClientMessage {
        ClientMessage::OpenDashboardTab { id: id.to_string() }
    }

    #[tokio::test]
    async fn opening_the_dashboard_twice_focuses_one_tab() {
        let (hub, _scratch) = test_hub("open-dashboard-twice");
        let mut events = hub.tab_events.subscribe();

        let first = dispatch_one(&hub, open_dashboard("d1")).await;
        let (id, tab_id) = match first {
            DaemonMessage::DashboardTabOpened { id, tab_id } => (id, tab_id),
            other => unreachable!("the first open answers with its tab id: {other:?}"),
        };
        assert_eq!(id, "d1");
        assert_eq!(
            take_tab_events(&mut events).len(),
            1,
            "the created tab is broadcast once"
        );

        let second = dispatch_one(&hub, open_dashboard("d2")).await;
        let (id, focused) = match second {
            DaemonMessage::DashboardTabOpened { id, tab_id } => (id, tab_id),
            other => unreachable!("the second open answers with its tab id: {other:?}"),
        };
        assert_eq!(id, "d2");
        assert_eq!(focused, tab_id, "a second open focuses the existing tab");
        assert!(
            take_tab_events(&mut events).is_empty(),
            "focusing broadcasts nothing"
        );

        let layout = hub.state.client_layout("c1");
        assert_eq!(layout.len(), 1, "one Dashboard per client: {layout:?}");
        assert!(
            matches!(&layout[0].content, TabContent::Dashboard),
            "{layout:?}"
        );
        assert_eq!(layout[0].name, "Dashboard");
    }

    /// Undo of a closed Dashboard can reach the daemon after an open made a
    /// new one; the client still has one Dashboard.
    #[tokio::test]
    async fn restoring_a_dashboard_when_one_is_open_keeps_one() {
        let (hub, _scratch) = test_hub("restore-dashboard");
        let open_tab_id = match dispatch_one(&hub, open_dashboard("d1")).await {
            DaemonMessage::DashboardTabOpened { tab_id, .. } => tab_id,
            other => unreachable!("the open answers with its tab id: {other:?}"),
        };
        let closed = TabEntry {
            id: "closed-dashboard".to_owned(),
            name: "Dashboard".to_owned(),
            content: TabContent::Dashboard,
            created_at: Utc::now(),
            picked_layout: None,
        };
        let mut events = hub.tab_events.subscribe();

        for msg in [
            ClientMessage::RestoreTab {
                tab: closed.clone(),
                index: 0,
            },
            ClientMessage::RestoreTabSnapshot { tab: closed },
        ] {
            let replies = dispatch_all(&hub, msg).await;
            assert!(replies.is_empty(), "nothing to answer: {replies:?}");
            assert!(
                take_tab_events(&mut events).is_empty(),
                "no second Dashboard is broadcast"
            );
            let layout = hub.state.client_layout("c1");
            assert_eq!(layout.len(), 1, "one Dashboard per client: {layout:?}");
            assert_eq!(layout[0].id, open_tab_id, "the open one stays");
        }
    }

    #[tokio::test]
    async fn an_empty_viewer_path_is_refused() {
        let (hub, _scratch) = test_hub("open-viewer-empty");
        let msg = dispatch_one(&hub, open_viewer("v1", "  ")).await;
        assert!(
            matches!(
                &msg,
                DaemonMessage::Error { message, request_id: Some(id), .. }
                    if id == "v1" && message == "open viewer tab failed: the path is empty"
            ),
            "{msg:?}"
        );
        assert!(
            hub.state.client_layout("c1").is_empty(),
            "a refused path stores no tab"
        );
    }

    #[tokio::test]
    async fn a_viewer_tab_is_named_after_the_file() {
        let (hub, _scratch) = test_hub("open-viewer-name");
        for (request, path, name) in [
            ("v1", "C:\\notes\\a.md", "a.md"),
            ("v2", "/home/u/b.txt", "b.txt"),
        ] {
            let msg = dispatch_one(&hub, open_viewer(request, path)).await;
            let tab_id = match msg {
                DaemonMessage::ViewerTabOpened { tab_id, .. } => tab_id,
                other => unreachable!("{path} opens a viewer tab: {other:?}"),
            };
            let layout = hub.state.client_layout("c1");
            let tab = layout
                .iter()
                .find(|t| t.id == tab_id)
                .expect("the answered tab is in the layout");
            assert_eq!(tab.name, name, "{path}");
        }
    }

    /// A cross-tab move extracts from the source before it discovers the
    /// destination is unknown — and, within one tab, before it discovers the
    /// insert target is gone; both roll the layout back.
    #[test]
    fn failed_cross_tab_move_leaves_layout_unchanged() {
        let (hub, _scratch) = test_hub("rollback-move");
        seed_two_pane_layout(&hub, "c1");
        let before = hub.state.client_layout("c1");

        let err = move_pane(
            &hub,
            "c1",
            &move_of(("t1", "p1"), ("ghost-tab", "p2"), PaneDropEdge::Right),
        )
        .expect_err("the destination tab is unknown");
        assert!(
            err.to_string().contains("ghost-tab"),
            "unexpected error: {err}"
        );
        assert_eq!(
            hub.state.client_layout("c1"),
            before,
            "the extracted pane is back in its source tab"
        );

        move_pane(
            &hub,
            "c1",
            &move_of(("t1", "p1"), ("t1", "p1"), PaneDropEdge::Right),
        )
        .expect_err("the pane cannot be inserted next to itself");
        assert_eq!(
            hub.state.client_layout("c1"),
            before,
            "a failed same-tab move is rolled back too"
        );
    }

    /// One pane move, as a pill drop or a menu sends it.
    fn move_of<'a>(
        src: (&'a str, &'a str),
        dst: (&'a str, &'a str),
        edge: PaneDropEdge,
    ) -> crate::tabs::PaneMove<'a> {
        crate::tabs::PaneMove {
            src_tab_id: src.0,
            src_pane_id: src.1,
            dst_tab_id: dst.0,
            dst_pane_id: dst.1,
            edge,
            touch: false,
        }
    }

    /// Dispatch `msg` as client `c1`, returning every message it sent to that
    /// connection. A request that answers nothing on success leaves it empty.
    async fn dispatch_all(hub: &Hub, msg: ClientMessage) -> Vec<DaemonMessage> {
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        let (file_tx, _file_rx) = mpsc::channel(1);
        let pty_lane: Arc<PtyLane> = Arc::default();
        let fetches: FetchRegistry = Arc::new(AsyncMutex::new(HashMap::new()));
        let local = AuthKind::Local;
        let ctx = ConnCtx {
            out_tx: &out_tx,
            file_tx: &file_tx,
            pty_lane: &pty_lane,
            fetches: &fetches,
            client_id: "c1",
            client_name: None,
            connection: 1,
            auth: &local,
        };
        dispatch(hub, msg, &ctx).await.expect("dispatch succeeds");
        drain(&mut out_rx)
    }

    /// Every tab event the hub emitted since `rx` was made.
    fn take_tab_events(
        rx: &mut broadcast::Receiver<crate::server::ScopedTabEvent>,
    ) -> Vec<TabEvent> {
        let mut out = Vec::new();
        while let Ok(scoped) = rx.try_recv() {
            out.push(scoped.event);
        }
        out
    }

    /// A tab whose panes, in order, are `panes`, each bound to `s-<pane id>`.
    fn seeded_tab(id: &str, panes: &[&str]) -> TabEntry {
        let bound: Vec<crate::tabs::PaneRef> = panes
            .iter()
            .map(|pane| crate::server::test_support::local_pane(pane, Some(&format!("s-{pane}"))))
            .collect();
        let grid = crate::tabs::build_balanced(&bound, protocol::SplitDirection::Horizontal)
            .expect("panes build a grid");
        TabEntry {
            id: id.to_string(),
            name: id.to_string(),
            content: TabContent::Grid { grid },
            created_at: Utc::now(),
            picked_layout: None,
        }
    }

    /// Tabs `t1` (panes `p1`, `p2`) and `t2` (panes `p3`, `p4`), all bound.
    fn seed_bound_layout(hub: &Hub, client_id: &str) {
        hub.state
            .mutate_client_layout(client_id, |tabs| {
                tabs.push(seeded_tab("t1", &["p1", "p2"]));
                tabs.push(seeded_tab("t2", &["p3", "p4"]));
            })
            .expect("seed the client layout");
    }

    fn swap_of(src: (&str, &str), dst: (&str, &str), request_id: Option<&str>) -> ClientMessage {
        ClientMessage::SwapPanes {
            src_tab_id: src.0.to_string(),
            src_pane_id: src.1.to_string(),
            dst_tab_id: dst.0.to_string(),
            dst_pane_id: dst.1.to_string(),
            request_id: request_id.map(str::to_owned),
        }
    }

    #[tokio::test]
    async fn a_refused_swap_panes_echoes_its_request_id_and_rolls_back() {
        let (hub, _scratch) = test_hub("swap-rid");
        seed_bound_layout(&hub, "c1");
        let before = hub.state.client_layout("c1");
        let msg = dispatch_one(&hub, swap_of(("t1", "p1"), ("ghost-tab", "p2"), Some("w1"))).await;
        assert!(
            matches!(
                &msg,
                DaemonMessage::Error { message, request_id: Some(id), .. }
                    if id == "w1" && message.contains("ghost-tab")
            ),
            "{msg:?}"
        );
        assert_eq!(
            hub.state.client_layout("c1"),
            before,
            "the refused swap changed nothing"
        );
    }

    #[tokio::test]
    async fn swap_panes_emits_a_tab_update_for_each_tab() {
        let (hub, _scratch) = test_hub("swap-events");
        seed_bound_layout(&hub, "c1");
        let mut events = hub.tab_events.subscribe();

        let replies = dispatch_all(&hub, swap_of(("t1", "p1"), ("t2", "p3"), None)).await;
        assert!(
            replies.is_empty(),
            "a swap answers nothing on success: {replies:?}"
        );

        let events = take_tab_events(&mut events);
        let sessions = |tab: &TabEntry| crate::tabs::collect_panes(tab.grid().expect("a grid tab"));
        let [TabEvent::Updated(src), TabEvent::Updated(dst)] = events.as_slice() else {
            unreachable!("expected one update per tab, got {events:?}");
        };
        assert_eq!(src.id, "t1");
        assert_eq!(
            sessions(src),
            vec![
                crate::server::test_support::local_pane("p1", Some("s-p3")),
                crate::server::test_support::local_pane("p2", Some("s-p2")),
            ]
        );
        assert_eq!(dst.id, "t2");
        assert_eq!(
            sessions(dst),
            vec![
                crate::server::test_support::local_pane("p3", Some("s-p1")),
                crate::server::test_support::local_pane("p4", Some("s-p4")),
            ]
        );
    }

    /// The pane ids of `tab`'s grid, in walk order.
    fn pane_ids(tab: &TabEntry) -> Vec<String> {
        crate::tabs::collect_panes(tab.grid().expect("a grid tab"))
            .into_iter()
            .map(|pane| pane.pane_id)
            .collect()
    }

    #[tokio::test]
    async fn merge_tabs_emits_the_new_tab_before_removing_the_merged_ones() {
        let (hub, _scratch) = test_hub("merge-order");
        seed_bound_layout(&hub, "c1");
        let mut events = hub.tab_events.subscribe();

        let merge = ClientMessage::MergeTabs {
            tab_ids: vec!["t1".to_string(), "t2".to_string()],
            name: None,
            layout: protocol::MergeLayout::TileVertical,
            request_id: None,
        };
        let replies = dispatch_all(&hub, merge).await;
        assert!(replies.is_empty(), "a merge answers nothing: {replies:?}");

        let events = take_tab_events(&mut events);
        let [
            TabEvent::Updated(merged),
            TabEvent::Removed(first),
            TabEvent::Removed(second),
        ] = events.as_slice()
        else {
            unreachable!("expected the new tab, then each merged tab's removal: {events:?}");
        };
        assert_eq!(pane_ids(merged), ["p1", "p2", "p3", "p4"]);
        assert_eq!((first.as_str(), second.as_str()), ("t1", "t2"));
    }

    fn extract_of(source_tab_id: &str, pane_ids: &[&str]) -> ClientMessage {
        ClientMessage::ExtractToNewTab {
            source_tab_id: source_tab_id.to_string(),
            pane_ids: pane_ids.iter().map(|id| (*id).to_string()).collect(),
            name: None,
            layout: None,
            request_id: None,
        }
    }

    #[tokio::test]
    async fn extract_to_new_tab_emits_the_new_tab_before_its_source() {
        let (hub, _scratch) = test_hub("extract-order");
        seed_bound_layout(&hub, "c1");
        let mut events = hub.tab_events.subscribe();

        dispatch_all(&hub, extract_of("t1", &["p1"])).await;
        let survived = take_tab_events(&mut events);
        let [TabEvent::Updated(new_tab), TabEvent::Updated(source)] = survived.as_slice() else {
            unreachable!("expected the new tab, then its source: {survived:?}");
        };
        assert_eq!(pane_ids(new_tab), ["p1"]);
        assert_eq!(source.id, "t1");
        assert_eq!(pane_ids(source), ["p2"]);

        dispatch_all(&hub, extract_of("t2", &["p3", "p4"])).await;
        let emptied = take_tab_events(&mut events);
        let [TabEvent::Updated(new_tab), TabEvent::Removed(gone)] = emptied.as_slice() else {
            unreachable!("expected the new tab, then its emptied source's removal: {emptied:?}");
        };
        assert_eq!(pane_ids(new_tab), ["p3", "p4"]);
        assert_eq!(gone, "t2");
    }

    #[tokio::test]
    async fn a_swap_of_a_pane_with_itself_is_a_silent_no_op() {
        let (hub, _scratch) = test_hub("swap-self");
        seed_bound_layout(&hub, "c1");
        let before = hub.state.client_layout("c1");
        let mut events = hub.tab_events.subscribe();

        let replies = dispatch_all(&hub, swap_of(("t1", "p1"), ("t1", "p1"), None)).await;
        assert!(replies.is_empty(), "{replies:?}");
        assert!(
            take_tab_events(&mut events).is_empty(),
            "a swap with itself broadcasts nothing"
        );
        assert_eq!(hub.state.client_layout("c1"), before);
    }

    #[tokio::test]
    async fn a_refused_reorder_tabs_echoes_its_request_id() {
        let (hub, _scratch) = test_hub("reorder-rid");
        seed_bound_layout(&hub, "c1");
        let before = hub.state.client_layout("c1");
        let msg = dispatch_one(
            &hub,
            ClientMessage::ReorderTabs {
                ordered_ids: vec!["t2".to_string()],
                request_id: Some("o1".to_string()),
            },
        )
        .await;
        assert!(
            matches!(
                &msg,
                DaemonMessage::Error { message, request_id: Some(id), .. }
                    if id == "o1" && message.contains("reorder set mismatch")
            ),
            "{msg:?}"
        );
        assert_eq!(hub.state.client_layout("c1"), before);
    }

    #[tokio::test]
    async fn an_id_less_reorder_failure_still_answers_without_one() {
        let (hub, _scratch) = test_hub("reorder-no-rid");
        seed_bound_layout(&hub, "c1");
        let msg = dispatch_one(
            &hub,
            ClientMessage::ReorderTabs {
                ordered_ids: vec!["t2".to_string()],
                request_id: None,
            },
        )
        .await;
        assert!(
            matches!(
                &msg,
                DaemonMessage::Error { message, request_id: None, .. }
                    if message.contains("reorder set mismatch")
            ),
            "{msg:?}"
        );
    }

    #[tokio::test]
    async fn reorder_tabs_success_still_broadcasts_tabs_reordered() {
        let (hub, _scratch) = test_hub("reorder-ok");
        seed_bound_layout(&hub, "c1");
        let mut events = hub.tab_events.subscribe();
        let reversed = vec!["t2".to_string(), "t1".to_string()];

        let replies = dispatch_all(
            &hub,
            ClientMessage::ReorderTabs {
                ordered_ids: reversed.clone(),
                request_id: Some("o2".to_string()),
            },
        )
        .await;
        assert!(
            replies.is_empty(),
            "a reorder answers nothing on success: {replies:?}"
        );

        let events = take_tab_events(&mut events);
        assert!(
            matches!(events.as_slice(), [TabEvent::Reordered(ids)] if ids == &reversed),
            "{events:?}"
        );
        let ids: Vec<String> = hub
            .state
            .client_layout("c1")
            .into_iter()
            .map(|tab| tab.id)
            .collect();
        assert_eq!(ids, reversed);
    }

    mod untouched_layout {
        use super::*;
        use protocol::{GridNode, RearrangeLayout, SplitDirection};

        /// Dispatch `msg` as client `c1`, failing when the dispatch errors.
        async fn dispatch_ok(hub: &Hub, msg: ClientMessage) {
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
                client_id: "c1",
                client_name: None,
                connection: 1,
                auth: &local,
            };
            dispatch(hub, msg, &ctx).await.expect("dispatch succeeds");
        }

        fn rearrange(layout: RearrangeLayout) -> ClientMessage {
            ClientMessage::RearrangeTab {
                tab_id: "t1".to_string(),
                layout,
            }
        }

        fn only_tab(hub: &Hub, client_id: &str) -> TabEntry {
            let tabs = hub.state.client_layout(client_id);
            assert_eq!(tabs.len(), 1, "{tabs:?}");
            tabs.into_iter().next().expect("one tab")
        }

        /// Panes `p1`..`p4`, `p1` bound to `doomed` and the rest to their own
        /// sessions.
        fn four_panes() -> Vec<crate::tabs::PaneRef> {
            vec![
                crate::server::test_support::local_pane("p1", Some("doomed")),
                crate::server::test_support::local_pane("p2", Some("s2")),
                crate::server::test_support::local_pane("p3", Some("s3")),
                crate::server::test_support::local_pane("p4", Some("s4")),
            ]
        }

        fn seed_four_panes(hub: &Hub, client_id: &str, picked: Option<RearrangeLayout>) {
            let grid =
                crate::tabs::build_balanced(&four_panes(), protocol::SplitDirection::Horizontal)
                    .expect("four panes build a grid");
            hub.state
                .mutate_client_layout(client_id, |tabs| {
                    tabs.push(TabEntry {
                        id: "t1".to_string(),
                        name: "Tab".to_string(),
                        content: TabContent::Grid { grid },
                        created_at: Utc::now(),
                        picked_layout: picked,
                    });
                })
                .expect("seed the client layout");
        }

        #[tokio::test]
        async fn rearrange_tab_sets_the_untouched_flag() {
            let (hub, _scratch) = test_hub("pick-rearrange");
            seed_two_pane_layout(&hub, "c1");
            dispatch_ok(&hub, rearrange(RearrangeLayout::Vertical)).await;
            assert_eq!(
                only_tab(&hub, "c1").picked_layout,
                Some(RearrangeLayout::Vertical)
            );
        }

        #[tokio::test]
        async fn set_pane_ratio_message_clears_the_flag() {
            let (hub, _scratch) = test_hub("pick-ratio");
            seed_two_pane_layout(&hub, "c1");
            dispatch_ok(&hub, rearrange(RearrangeLayout::Horizontal)).await;
            dispatch_ok(
                &hub,
                ClientMessage::SetPaneRatio {
                    tab_id: "t1".to_string(),
                    split_path: Vec::new(),
                    ratio: 0.7,
                },
            )
            .await;
            assert_eq!(only_tab(&hub, "c1").picked_layout, None);
        }

        #[test]
        fn session_removal_rebuilds_each_clients_untouched_tab_independently() {
            let (hub, _scratch) = test_hub("pick-session-removal");
            seed_four_panes(&hub, "c1", Some(RearrangeLayout::Horizontal));
            seed_four_panes(&hub, "c2", None);

            close_session_panes(&hub, "doomed");

            let survivors = &four_panes()[1..];
            let picked = only_tab(&hub, "c1");
            assert_eq!(
                picked.grid(),
                crate::tabs::build_balanced(survivors, protocol::SplitDirection::Horizontal)
                    .as_ref(),
                "the untouched tab is rebuilt evenly"
            );
            assert_eq!(picked.picked_layout, Some(RearrangeLayout::Horizontal));
            let touched = only_tab(&hub, "c2");
            let collapsed = GridNode::Split {
                direction: SplitDirection::Horizontal,
                ratio: 0.5,
                first: Box::new(GridNode::Pane {
                    pane_id: "p2".to_string(),
                    session_id: Some("s2".to_string()),
                    host: None,
                }),
                second: Box::new(GridNode::Split {
                    direction: SplitDirection::Horizontal,
                    ratio: 0.5,
                    first: Box::new(GridNode::Pane {
                        pane_id: "p3".to_string(),
                        session_id: Some("s3".to_string()),
                        host: None,
                    }),
                    second: Box::new(GridNode::Pane {
                        pane_id: "p4".to_string(),
                        session_id: Some("s4".to_string()),
                        host: None,
                    }),
                }),
            };
            assert_eq!(
                touched.grid(),
                Some(&collapsed),
                "the touched tab keeps the sizes it had: closing p1 folds its split into p2"
            );
            assert_eq!(touched.picked_layout, None);
            assert_eq!(
                crate::tabs::collect_panes(touched.grid().expect("grid")),
                survivors.to_vec()
            );
        }

        /// [`four_panes`] with `p1` showing session `doomed` of `ssh:devbox`.
        fn four_panes_one_foreign() -> Vec<crate::tabs::PaneRef> {
            let mut panes = four_panes();
            panes[0].host = Some("ssh:devbox".to_string());
            panes
        }

        fn seed_foreign_four_panes(hub: &Hub, client_id: &str, picked: Option<RearrangeLayout>) {
            let grid = crate::tabs::build_balanced(
                &four_panes_one_foreign(),
                protocol::SplitDirection::Horizontal,
            )
            .expect("four panes build a grid");
            hub.state
                .mutate_client_layout(client_id, |tabs| {
                    tabs.push(TabEntry {
                        id: "t1".to_string(),
                        name: "Tab".to_string(),
                        content: TabContent::Grid { grid },
                        created_at: Utc::now(),
                        picked_layout: picked,
                    });
                })
                .expect("seed the client layout");
        }

        fn forget_doomed() -> ClientMessage {
            ClientMessage::ForgetHostSessions {
                host: "ssh:devbox".to_string(),
                session_ids: vec!["doomed".to_string()],
                all: false,
                request_id: None,
            }
        }

        #[tokio::test]
        async fn forget_host_sessions_updates_every_client_layout() {
            let (hub, _scratch) = test_hub("pick-forget-host");
            seed_foreign_four_panes(&hub, "c1", Some(RearrangeLayout::Horizontal));
            seed_foreign_four_panes(&hub, "c2", None);

            close_session_panes(&hub, "doomed");
            for cid in ["c1", "c2"] {
                assert_eq!(
                    crate::tabs::collect_panes(only_tab(&hub, cid).grid().expect("grid")),
                    four_panes_one_foreign(),
                    "{cid}: a local removal leaves the foreign pane with the same id"
                );
            }

            let mut events = hub.tab_events.subscribe();
            dispatch_ok(&hub, forget_doomed()).await;

            let survivors = &four_panes()[1..];
            let picked = only_tab(&hub, "c1");
            assert_eq!(
                picked.grid(),
                crate::tabs::build_balanced(survivors, protocol::SplitDirection::Horizontal)
                    .as_ref(),
                "the untouched tab is rebuilt evenly"
            );
            assert_eq!(picked.picked_layout, Some(RearrangeLayout::Horizontal));
            let touched = only_tab(&hub, "c2");
            assert_eq!(touched.picked_layout, None);
            assert_eq!(
                crate::tabs::collect_panes(touched.grid().expect("grid")),
                survivors.to_vec()
            );
            let updated = super::take_tab_events(&mut events);
            assert_eq!(updated.len(), 2, "one update per client: {updated:?}");
            assert!(
                updated
                    .iter()
                    .all(|event| matches!(event, TabEvent::Updated(tab) if tab.id == "t1")),
                "{updated:?}"
            );

            dispatch_ok(&hub, forget_doomed()).await;
            let again = super::take_tab_events(&mut events);
            assert!(
                again.is_empty(),
                "forgetting again changes nothing: {again:?}"
            );
        }

        #[tokio::test]
        async fn flag_persists_across_state_reload() {
            let (hub, _scratch) = test_hub("pick-reload");
            seed_two_pane_layout(&hub, "c1");
            let fixed = RearrangeLayout::Grid {
                cols: 3,
                aspect: None,
            };
            dispatch_ok(&hub, rearrange(fixed)).await;
            let reloaded = AppState::load_or_default(&hub.dirs).expect("reload state");
            let tabs = reloaded.client_layout("c1");
            assert_eq!(tabs.first().and_then(|t| t.picked_layout), Some(fixed));

            // An automatic pick's aspect is part of the stored layout, so it
            // survives a reload too.
            let automatic = RearrangeLayout::Grid {
                cols: 0,
                aspect: Some(1.6),
            };
            dispatch_ok(&hub, rearrange(automatic)).await;
            let reloaded = AppState::load_or_default(&hub.dirs).expect("reload state");
            let tabs = reloaded.client_layout("c1");
            assert_eq!(tabs.first().and_then(|t| t.picked_layout), Some(automatic));
        }
    }
}
