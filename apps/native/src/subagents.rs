//! Subagents as sidebar leaves: the status a subagent's leaf draws and the
//! leaf itself, nested under its host session. Plain Rust, so every rule is
//! unit-tested.

use protocol::{AgentState, SessionMode, SessionStatus, SubagentSnapshot};

use crate::sidebar::{Leaf, LeafKind, LeafState};
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

/// `snapshot`'s leaf, listed by `host` and nested under its session
/// `host_session`.
#[must_use]
pub fn subagent_leaf(host: &HostKey, host_session: &str, snapshot: &SubagentSnapshot) -> Leaf {
    Leaf {
        id: snapshot.agent_id.clone(),
        kind: LeafKind::Subagent,
        host: host.clone(),
        status: leaf_status(snapshot.state),
        mode: SessionMode::Headless,
        agent: None,
        label: snapshot
            .description
            .clone()
            .or_else(|| snapshot.agent_type.clone())
            .unwrap_or_else(|| "Subagent".to_owned()),
        account_login: false,
        agent_up: true,
        subline: None,
        runtime: None,
        icon: None,
        chips: Vec::new(),
        attention: false,
        busy_excluded: false,
        unseen: false,
        background: false,
        state: LeafState::Live,
        tooltip: String::new(),
        depth: snapshot.depth.clamp(MIN_DEPTH, MAX_DEPTH),
        host_session: Some(host_session.to_owned()),
        nested: 0,
        nested_subagents: 0,
        collapsed: false,
    }
}
