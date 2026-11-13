//! Subagents as sidebar leaves: the status a subagent's leaf draws and the
//! leaf itself, nested under its host session. Plain Rust, so every rule is
//! unit-tested.

use protocol::{AgentState, DispatchRunSnapshot, SessionMode, SessionStatus, SubagentSnapshot};

use crate::sidebar::{FinishedSubagentRows, Leaf, LeafDensity, LeafKind, LeafState};
use crate::target::HostKey;

/// The shallowest row a subagent draws at: one the session's main
/// conversation started.
const MIN_DEPTH: u8 = 1;
/// The deepest row a subagent draws at; a deeper one draws at this depth.
const MAX_DEPTH: u8 = 3;

/// The status dot a subagent's leaf draws for its `state`: a running or
/// queued one works, a finished one reads idle, and one that failed or that
/// this build does not know reads stopped.
#[must_use]
pub fn leaf_status(state: AgentState) -> SessionStatus {
    match state {
        AgentState::Running | AgentState::Queued => SessionStatus::Working,
        AgentState::Finished => SessionStatus::Idle,
        AgentState::Failed | AgentState::Unknown => SessionStatus::Stopped,
    }
}

/// Whether a subagent runs: the states [`leaf_status`] draws as working.
fn runs(sub: &SubagentSnapshot) -> bool {
    matches!(sub.state, AgentState::Running | AgentState::Queued)
}

/// The subagents of `list` that show in the sidebar: with `show_finished`,
/// every one, else a running one and a finished one whose subtree (the
/// entries after it more deeply nested) has a running row, so a running
/// child never draws orphaned. `list` is flat in tree order.
#[must_use]
pub fn shown(list: &[SubagentSnapshot], show_finished: bool) -> Vec<&SubagentSnapshot> {
    let mut kept = Vec::new();
    for (at, sub) in list.iter().enumerate() {
        let subtree_runs = list[at + 1..]
            .iter()
            .take_while(|later| later.depth > sub.depth)
            .any(runs);
        if show_finished || runs(sub) || subtree_runs {
            kept.push(sub);
        }
    }
    kept
}

/// The density `leaf` draws at: `Compact` for a finished or failed subagent
/// when `finished_rows` draws those so, else the sidebar's `density`.
#[must_use]
pub fn row_density(
    leaf: &Leaf,
    density: LeafDensity,
    finished_rows: FinishedSubagentRows,
) -> LeafDensity {
    let finished = leaf.kind == LeafKind::Subagent && leaf.status != SessionStatus::Working;
    if finished && finished_rows == FinishedSubagentRows::Compact {
        LeafDensity::Compact
    } else {
        density
    }
}

/// The subagent leaf's label: its description, else its agent type, else
/// `Subagent`.
#[must_use]
pub fn label(sub: &SubagentSnapshot) -> String {
    sub.description
        .clone()
        .or_else(|| sub.agent_type.clone())
        .unwrap_or_else(|| "Subagent".to_owned())
}

/// The subagent leaf's subline: its agent type and model, the ones it has
/// joined with ` · `; `None` when it has neither.
#[must_use]
pub fn subline(sub: &SubagentSnapshot) -> Option<String> {
    let parts: Vec<&str> = [sub.agent_type.as_deref(), sub.model.as_deref()]
        .into_iter()
        .flatten()
        .collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// The subagent leaf's hover text: its full description first when it has
/// one, then what it is, its agent type, model and effort, when it started
/// in local time, and how it ended, one per line, each left out when the
/// subagent does not have it.
#[must_use]
pub fn hover_text(sub: &SubagentSnapshot) -> String {
    let started = sub.started_at.map(|at| {
        let at = at.with_timezone(&chrono::Local);
        format!("Started: {}", at.format("%Y-%m-%d %H:%M"))
    });
    let description = sub
        .description
        .as_ref()
        .filter(|d| !d.trim().is_empty())
        .cloned();
    [
        description,
        Some("Subagent".to_owned()),
        sub.agent_type.as_ref().map(|t| format!("Type: {t}")),
        sub.model.as_ref().map(|m| format!("Model: {m}")),
        sub.effort.as_ref().map(|e| format!("Effort: {e}")),
        started,
        end_line(sub),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join("\n")
}

/// How the subagent ended: its end status, else `Finished` or `Failed` by
/// its state; `None` while it still runs or its state is unknown.
fn end_line(sub: &SubagentSnapshot) -> Option<String> {
    if let Some(status) = &sub.end_status {
        return Some(status.clone());
    }
    match sub.state {
        AgentState::Finished => Some("Finished".to_owned()),
        AgentState::Failed => Some("Failed".to_owned()),
        AgentState::Running | AgentState::Queued | AgentState::Unknown => None,
    }
}

/// The host session's fold tooltip, naming what its `nested` rows are:
/// dispatch runs when none of them is one of its `nested_subagents`,
/// subagents when all are, else both; "Show …" while `collapsed`.
#[must_use]
pub fn fold_tip(collapsed: bool, nested: usize, nested_subagents: usize) -> &'static str {
    match (collapsed, nested_subagents == 0, nested_subagents == nested) {
        (false, true, _) => "Hide its dispatch runs",
        (false, false, true) => "Hide its subagents",
        (false, false, false) => "Hide its subagents and dispatch runs",
        (true, true, _) => "Show its dispatch runs",
        (true, false, true) => "Show its subagents",
        (true, false, false) => "Show its subagents and dispatch runs",
    }
}

/// `snapshot`'s leaf, listed by `host` and nested under its session
/// `host_session`.
#[must_use]
pub fn subagent_leaf(host: &HostKey, host_session: &str, snapshot: &SubagentSnapshot) -> Leaf {
    Leaf {
        host_session: Some(host_session.to_owned()),
        ..base_leaf(host, snapshot)
    }
}

/// The leaf of `snapshot`, listed by `host`: one of `run`'s subagents,
/// which `host_session` hosts when the run nests under a session, drawing
/// `run_depth` levels below the run's own row.
#[must_use]
pub fn run_subagent_leaf(
    host: &HostKey,
    run: &DispatchRunSnapshot,
    host_session: Option<&str>,
    run_depth: u8,
    snapshot: &SubagentSnapshot,
) -> Leaf {
    Leaf {
        run_id: Some(run.run_id.clone()),
        host_session: host_session.map(str::to_owned),
        depth: run_depth.saturating_add(snapshot.depth).min(MAX_DEPTH),
        ..base_leaf(host, snapshot)
    }
}

/// `snapshot`'s leaf, listed by `host`, nested under no session and
/// belonging to no run.
fn base_leaf(host: &HostKey, snapshot: &SubagentSnapshot) -> Leaf {
    Leaf {
        id: snapshot.agent_id.clone(),
        kind: LeafKind::Subagent,
        host: host.clone(),
        status: leaf_status(snapshot.state),
        mode: SessionMode::Headless,
        agent: None,
        label: label(snapshot),
        account_login: false,
        agent_up: true,
        subline: subline(snapshot),
        runtime: Some("SA".to_owned()),
        icon: None,
        chips: Vec::new(),
        attention: false,
        busy_excluded: false,
        unseen: false,
        background: false,
        state: LeafState::Live,
        tooltip: hover_text(snapshot),
        depth: snapshot.depth.clamp(MIN_DEPTH, MAX_DEPTH),
        host_session: None,
        run_id: None,
        nested: 0,
        nested_subagents: 0,
        collapsed: false,
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;

    fn sub(id: &str) -> SubagentSnapshot {
        SubagentSnapshot {
            agent_id: id.to_owned(),
            depth: 1,
            ..SubagentSnapshot::default()
        }
    }

    /// `sub` with every part its hover lists.
    fn full(id: &str) -> SubagentSnapshot {
        SubagentSnapshot {
            agent_type: Some("explore".to_owned()),
            model: Some("model-x".to_owned()),
            effort: Some("high".to_owned()),
            started_at: Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).single(),
            ..sub(id)
        }
    }

    fn started_line(sub: &SubagentSnapshot) -> String {
        let at = sub.started_at.map(|at| at.with_timezone(&chrono::Local));
        format!(
            "Started: {}",
            at.map(|at| at.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_default()
        )
    }

    #[test]
    fn label_falls_back_to_type_then_subagent() {
        let mut s = sub("a1");
        assert_eq!(label(&s), "Subagent");
        s.agent_type = Some("explore".to_owned());
        assert_eq!(label(&s), "explore");
        s.description = Some("Find the bug".to_owned());
        assert_eq!(label(&s), "Find the bug");
    }

    #[test]
    fn subline_joins_type_and_model() {
        let mut s = full("a1");
        assert_eq!(subline(&s).as_deref(), Some("explore · model-x"));
        s.model = None;
        assert_eq!(subline(&s).as_deref(), Some("explore"));
        s.model = Some("model-x".to_owned());
        s.agent_type = None;
        assert_eq!(subline(&s).as_deref(), Some("model-x"));
    }

    #[test]
    fn subline_is_none_without_type_or_model() {
        let mut s = sub("a1");
        s.effort = Some("high".to_owned());
        assert_eq!(subline(&s), None);
    }

    #[test]
    fn hover_text_lists_type_model_effort_start_and_end() {
        let mut s = full("a1");
        s.description = Some("Map the tail code".to_owned());
        s.state = AgentState::Finished;
        let text = hover_text(&s);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines,
            [
                "Map the tail code",
                "Subagent",
                "Type: explore",
                "Model: model-x",
                "Effort: high",
                started_line(&s).as_str(),
                "Finished",
            ],
            "{text}"
        );

        s.state = AgentState::Failed;
        assert_eq!(hover_text(&s).lines().last(), Some("Failed"));

        s.end_status = Some("stopped by the user".to_owned());
        assert_eq!(
            hover_text(&s).lines().last(),
            Some("stopped by the user"),
            "the end status beats the state"
        );
    }

    #[test]
    fn hover_text_leaves_out_missing_parts_and_a_running_end() {
        for state in [AgentState::Running, AgentState::Queued, AgentState::Unknown] {
            let mut s = sub("a1");
            s.state = state;
            assert_eq!(hover_text(&s), "Subagent", "{state:?}");
        }
        let mut s = sub("a1");
        s.model = Some("model-x".to_owned());
        s.state = AgentState::Running;
        assert_eq!(hover_text(&s), "Subagent\nModel: model-x");

        let mut s = sub("a1");
        s.state = AgentState::Running;
        s.description = Some("  ".to_owned());
        assert_eq!(
            hover_text(&s),
            "Subagent",
            "a blank description adds no line"
        );
        s.description = Some("Map the tail code".to_owned());
        assert_eq!(hover_text(&s), "Map the tail code\nSubagent");
    }

    #[test]
    fn fold_tip_names_what_it_hides() {
        for (collapsed, nested, nested_subagents, want) in [
            (false, 2, 0, "Hide its dispatch runs"),
            (false, 2, 2, "Hide its subagents"),
            (false, 3, 1, "Hide its subagents and dispatch runs"),
            (true, 2, 0, "Show its dispatch runs"),
            (true, 2, 2, "Show its subagents"),
            (true, 3, 1, "Show its subagents and dispatch runs"),
        ] {
            assert_eq!(
                fold_tip(collapsed, nested, nested_subagents),
                want,
                "{collapsed} {nested} {nested_subagents}"
            );
        }
    }

    #[test]
    fn subagent_leaf_carries_the_sa_tag() {
        let s = full("a1");
        let leaf = subagent_leaf(&HostKey::Local, "s1", &s);
        assert_eq!(leaf.runtime.as_deref(), Some("SA"));
        assert_eq!(leaf.label, label(&s));
        assert_eq!(leaf.subline, subline(&s));
        assert_eq!(leaf.tooltip, hover_text(&s));
        assert_eq!(leaf.host_session.as_deref(), Some("s1"));
    }

    /// Subagent `id` at `depth` in `state`.
    fn in_state(id: &str, depth: u8, state: AgentState) -> SubagentSnapshot {
        SubagentSnapshot {
            depth,
            state,
            ..sub(id)
        }
    }

    fn ids<'a>(list: &[&'a SubagentSnapshot]) -> Vec<&'a str> {
        list.iter().map(|s| s.agent_id.as_str()).collect()
    }

    #[test]
    fn shown_keeps_every_row_when_finished_ones_show() {
        let list = [
            in_state("a1", 1, AgentState::Failed),
            in_state("a2", 1, AgentState::Running),
        ];
        assert_eq!(ids(&shown(&list, true)), ["a1", "a2"]);
    }

    #[test]
    fn shown_hides_a_finished_row() {
        let list = [in_state("a1", 1, AgentState::Finished)];
        assert!(shown(&list, false).is_empty(), "a finished row hides");
    }

    #[test]
    fn shown_hides_failed_and_unknown_rows() {
        let list = [
            in_state("a1", 1, AgentState::Failed),
            in_state("a2", 1, AgentState::Unknown),
        ];
        assert!(shown(&list, false).is_empty(), "neither row shows");
    }

    #[test]
    fn shown_keeps_a_running_row() {
        let list = [
            in_state("a1", 1, AgentState::Running),
            in_state("a2", 1, AgentState::Queued),
        ];
        assert_eq!(ids(&shown(&list, false)), ["a1", "a2"]);
    }

    #[test]
    fn shown_keeps_a_finished_parent_of_a_running_child() {
        let list = [
            in_state("a1", 1, AgentState::Finished),
            in_state("c1", 2, AgentState::Running),
        ];
        assert_eq!(ids(&shown(&list, false)), ["a1", "c1"]);
    }

    #[test]
    fn shown_hides_a_finished_parent_whose_children_finished() {
        let list = [
            in_state("a1", 1, AgentState::Finished),
            in_state("c1", 2, AgentState::Finished),
            in_state("c2", 3, AgentState::Failed),
        ];
        assert!(shown(&list, false).is_empty(), "{list:?}");
    }

    #[test]
    fn shown_keeps_a_running_sibling_after_a_hidden_subtree() {
        let list = [
            in_state("a1", 1, AgentState::Finished),
            in_state("c1", 2, AgentState::Finished),
            in_state("a2", 1, AgentState::Running),
        ];
        assert_eq!(ids(&shown(&list, false)), ["a2"]);
    }

    /// A subagent leaf of a subagent in `state`.
    fn subagent_row(state: AgentState) -> Leaf {
        subagent_leaf(&HostKey::Local, "s1", &in_state("a1", 1, state))
    }

    #[test]
    fn row_density_draws_a_finished_subagent_compact_when_asked() {
        for state in [
            AgentState::Finished,
            AgentState::Failed,
            AgentState::Unknown,
        ] {
            assert_eq!(
                row_density(
                    &subagent_row(state),
                    LeafDensity::Comfortable,
                    FinishedSubagentRows::Compact,
                ),
                LeafDensity::Compact,
                "{state:?}"
            );
        }
    }

    #[test]
    fn row_density_keeps_a_running_subagent_at_the_leaf_density() {
        for state in [AgentState::Running, AgentState::Queued] {
            for density in [LeafDensity::Comfortable, LeafDensity::Compact] {
                assert_eq!(
                    row_density(&subagent_row(state), density, FinishedSubagentRows::Compact),
                    density,
                    "{state:?}"
                );
            }
        }
    }

    #[test]
    fn row_density_follows_by_default() {
        for density in [LeafDensity::Comfortable, LeafDensity::Compact] {
            assert_eq!(
                row_density(
                    &subagent_row(AgentState::Finished),
                    density,
                    FinishedSubagentRows::Follow,
                ),
                density
            );
        }
    }

    #[test]
    fn row_density_leaves_a_session_leaf_alone() {
        let session = Leaf {
            kind: LeafKind::Session,
            ..subagent_row(AgentState::Finished)
        };
        assert_eq!(
            row_density(
                &session,
                LeafDensity::Comfortable,
                FinishedSubagentRows::Compact,
            ),
            LeafDensity::Comfortable
        );
    }
}
