//! Tabs and pane layouts: the tab and layout messages, diff tabs, and keeping layouts in step with sessions.
use super::connection::ConnCtx;
use super::git::repo_path_or_err;
use super::{Hub, StateEvent, TabEvent};
use crate::registry::{reorder_containers, set_session_order};
use chrono::Utc;
use protocol::{ClientMessage, DaemonMessage, InitLayoutKind, TabContent, TabEntry};
use tokio::sync::mpsc;
use tracing::warn;

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
        } => {
            let outcome = open_diff_tab(
                hub,
                client_id,
                &repo_id,
                &path,
                against.as_deref(),
                worktree_path.as_deref(),
            );
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
                    });
                }
            }
        }
        ClientMessage::ListTabs => {
            let tabs = hub.state.client_layout(client_id);
            let _ = out_tx.send(DaemonMessage::Tabs { tabs });
        }
        ClientMessage::CreateTab {
            name,
            initial_session_id,
            request_id,
        } => {
            // Resolve the default name against the current tab list so
            // sequential opens produce `Tab`, `Tab 2`, `Tab 3`, ...
            let created = hub.state.mutate_client_layout(client_id, |tabs| {
                let tab = crate::tabs::make_tab(name, initial_session_id, tabs);
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
        ClientMessage::RestoreTab { tab, index } => {
            let (restored, ordered_ids) =
                hub.state.try_mutate_client_layout(client_id, |tabs| {
                    let restored = crate::tabs::restore_tab(tabs, tab, index)?;
                    let ordered_ids = tabs.iter().map(|t| t.id.clone()).collect();
                    Ok::<_, anyhow::Error>((restored, ordered_ids))
                })?;
            hub.emit_tab(client_id, TabEvent::Updated(restored));
            hub.emit_tab(client_id, TabEvent::Reordered(ordered_ids));
        }
        ClientMessage::RestoreTabSnapshot { tab } => {
            let restored = hub.state.try_mutate_client_layout(client_id, |tabs| {
                crate::tabs::restore_tab_snapshot(tabs, tab)
            })?;
            hub.emit_tab(client_id, TabEvent::Updated(restored));
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
            request_id,
        } => {
            let updated = hub.state.try_mutate_client_layout(client_id, |tabs| {
                let tab = crate::tabs::find_tab_mut(tabs, &tab_id)?;
                crate::tabs::split_tab_pane(tab, &pane_id, direction, place, new_session_id)?;
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
        } => {
            let updated = hub.state.mutate_client_layout(client_id, |tabs| {
                let tab = crate::tabs::find_tab_mut(tabs, &tab_id)?;
                let grid = crate::tabs::grid_or_err_mut(tab)?;
                crate::tabs::replace_pane_session(grid, &pane_id, session_id)?;
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
            for id in &tab_ids {
                hub.emit_tab(client_id, TabEvent::Removed(id.clone()));
            }
            hub.emit_tab(client_id, TabEvent::Updated(new_tab));
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
            if let Some(survivor) = source {
                hub.emit_tab(client_id, TabEvent::Updated(survivor));
            } else {
                hub.emit_tab(client_id, TabEvent::Removed(source_tab_id));
            }
            hub.emit_tab(client_id, TabEvent::Updated(new_tab));
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

struct OpenDiffOutcome {
    tab_id: String,
    /// `Some` when a new tab was created (caller should broadcast
    /// `TabUpdated`); `None` when an existing diff tab matched and we just
    /// focused it.
    created: Option<TabEntry>,
}

/// Find or create a diff tab keyed on (`repo_id`, `path`, `against`).
/// Returns the tab id either way so the client can activate it.
fn open_diff_tab(
    hub: &Hub,
    client_id: &str,
    repo_id: &str,
    path: &str,
    against: Option<&str>,
    worktree_path: Option<&str>,
) -> anyhow::Result<OpenDiffOutcome> {
    repo_path_or_err(hub, repo_id)?;
    let against_owned = against.map(str::to_string);
    let worktree_path_owned = worktree_path.map(str::to_string);
    let outcome: anyhow::Result<OpenDiffOutcome> = hub.state.mutate_client_layout(client_id, |tabs| {
        if let Some(existing) = tabs.iter().find(|t| {
            matches!(
                &t.content,
                TabContent::Diff {
                    repo_id: r,
                    path: p,
                    against: a,
                    worktree_path: w,
                } if r == repo_id && p == path && a.as_deref() == against && w.as_deref() == worktree_path
            )
        }) {
            return Ok(OpenDiffOutcome {
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
            },
            created_at: chrono::Utc::now(),
            picked_layout: None,
        };
        let id = new_tab.id.clone();
        tabs.push(new_tab.clone());
        Ok(OpenDiffOutcome {
            tab_id: id,
            created: Some(new_tab),
        })
    })?;
    outcome
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
/// `MergeTabs`, `MovePane`, `SwapPanes`, `ReorderTabs`, `ExtractToNewTab`) with
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
    });
}

/// Build a one-tab layout with a pane per currently-running session — the
/// "open all active sessions" first-connect choice. Empty when no sessions
/// exist (the client starts with no tabs).
fn build_all_sessions_layout(hub: &Hub) -> Vec<TabEntry> {
    let panes: Vec<(String, Option<String>)> = hub
        .sessions
        .snapshots()
        .into_iter()
        .map(|s| (uuid::Uuid::new_v4().to_string(), Some(s.id)))
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

/// Auto-close every pane bound to `session_id` across all client layouts when
/// the session is removed: a pane's sibling fills its space, and a tab whose
/// last pane closed is removed entirely. A surviving tab untouched since its
/// layout was picked is rebuilt in that layout, by each client's own flag.
/// Emits scoped `TabUpdated` (survivors) / `TabRemoved` (emptied tabs) per
/// affected client.
pub(super) fn close_session_panes(hub: &Hub, session_id: &str) {
    // Per-client outcomes: a surviving tab to re-broadcast, or a tab id whose
    // last pane closed (already removed from that client's layout).
    enum Outcome {
        Updated(String, TabEntry),
        Removed(String, String),
    }
    let outcomes = match hub.state.mutate_all_layouts(|layouts| {
        let mut out: Vec<Outcome> = Vec::new();
        for (cid, layout) in layouts.iter_mut() {
            let result = crate::tabs::close_session_panes(&mut layout.tabs, session_id);
            for tab in result.updated {
                out.push(Outcome::Updated(cid.clone(), tab));
            }
            for tab_id in result.removed_tab_ids {
                out.push(Outcome::Removed(cid.clone(), tab_id));
            }
        }
        out
    }) {
        Ok(list) => list,
        Err(err) => {
            warn!(?err, "close_session_panes: state.mutate failed");
            return;
        }
    };
    for outcome in outcomes {
        match outcome {
            Outcome::Updated(cid, tab) => hub.emit_tab(&cid, TabEvent::Updated(tab)),
            Outcome::Removed(cid, tab_id) => hub.emit_tab(&cid, TabEvent::Removed(tab_id)),
        }
    }
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
    use crate::server::connection::{AuthKind, FetchRegistry, PtyForwarders};
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
                DaemonMessage::Error { message, request_id: Some(id) }
                    if id == "m1" && message.contains("at least two tabs")
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
                DaemonMessage::Error { message, request_id: None }
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
                DaemonMessage::Error { message, request_id: Some(id) }
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
                request_id: Some("s1".to_string()),
            },
        )
        .await;
        assert!(
            matches!(
                &msg,
                DaemonMessage::Error { message, request_id: Some(id) }
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
                DaemonMessage::Error { message, request_id: Some(id) }
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
                DaemonMessage::Error { message, request_id: Some(id) }
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
            },
        )
        .await;
        assert!(
            matches!(
                &msg,
                DaemonMessage::Error { message, request_id: Some(id) }
                    if id == "diff-open-1" && message.contains("unknown repo")
            ),
            "{msg:?}"
        );
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
        let pty_forwarders: PtyForwarders = Arc::new(AsyncMutex::new(HashMap::new()));
        let fetches: FetchRegistry = Arc::new(AsyncMutex::new(HashMap::new()));
        let local = AuthKind::Local;
        let ctx = ConnCtx {
            out_tx: &out_tx,
            file_tx: &file_tx,
            pty_forwarders: &pty_forwarders,
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
        let bound: Vec<(String, Option<String>)> = panes
            .iter()
            .map(|pane| ((*pane).to_string(), Some(format!("s-{pane}"))))
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
                DaemonMessage::Error { message, request_id: Some(id) }
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
                ("p1".to_string(), Some("s-p3".to_string())),
                ("p2".to_string(), Some("s-p2".to_string())),
            ]
        );
        assert_eq!(dst.id, "t2");
        assert_eq!(
            sessions(dst),
            vec![
                ("p3".to_string(), Some("s-p1".to_string())),
                ("p4".to_string(), Some("s-p4".to_string())),
            ]
        );
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
                DaemonMessage::Error { message, request_id: Some(id) }
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
                DaemonMessage::Error { message, request_id: None }
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
            let pty_forwarders: PtyForwarders = Arc::new(AsyncMutex::new(HashMap::new()));
            let fetches: FetchRegistry = Arc::new(AsyncMutex::new(HashMap::new()));
            let local = AuthKind::Local;
            let ctx = ConnCtx {
                out_tx: &out_tx,
                file_tx: &file_tx,
                pty_forwarders: &pty_forwarders,
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
        fn four_panes() -> Vec<(String, Option<String>)> {
            vec![
                ("p1".to_string(), Some("doomed".to_string())),
                ("p2".to_string(), Some("s2".to_string())),
                ("p3".to_string(), Some("s3".to_string())),
                ("p4".to_string(), Some("s4".to_string())),
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
                }),
                second: Box::new(GridNode::Split {
                    direction: SplitDirection::Horizontal,
                    ratio: 0.5,
                    first: Box::new(GridNode::Pane {
                        pane_id: "p3".to_string(),
                        session_id: Some("s3".to_string()),
                    }),
                    second: Box::new(GridNode::Pane {
                        pane_id: "p4".to_string(),
                        session_id: Some("s4".to_string()),
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
