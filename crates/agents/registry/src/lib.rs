//! The registered agent plugins and lookup by agent.

use agent_api::AgentPlugin;
use protocol::Agent;

/// Claude's provider table, for the daemon's history import, which reads it
/// directly rather than through a plugin method.
pub use agent_claude::{display_name, provider_for_model};

/// Every registered plugin, one per [`Agent`] variant.
static ALL: [&dyn AgentPlugin; 1] = [&agent_claude::CLAUDE];

/// The plugin that spawns `agent`. Plugins are stateless statics.
#[must_use]
pub fn plugin(agent: Agent) -> &'static dyn AgentPlugin {
    match agent {
        Agent::Claude => &agent_claude::CLAUDE,
    }
}

/// Every registered plugin.
#[must_use]
pub fn all() -> &'static [&'static dyn AgentPlugin] {
    &ALL
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_returns_claude_for_claude() {
        assert_eq!(
            plugin(Agent::Claude).program_env_var(),
            "RUSTLING_TULIP_CLAUDE"
        );
    }

    #[test]
    fn all_lists_every_agent() {
        assert_eq!(all().len(), Agent::all().len());
        for agent in Agent::all() {
            let env_var = plugin(*agent).program_env_var();
            assert!(
                all().iter().any(|p| p.program_env_var() == env_var),
                "{env_var} is missing from all()"
            );
        }
    }
}
