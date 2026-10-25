//! The registered agent plugins and lookup by agent id.

use agent_api::AgentPlugin;
use protocol::{AgentDescriptor, AgentId};

/// Every registered plugin, the default agent first.
static ALL: [&dyn AgentPlugin; 1] = [&agent_claude::CLAUDE];

/// The plugin registered under `agent`, or `None` when no plugin has that
/// id. Plugins are stateless statics.
#[must_use]
pub fn plugin(agent: &AgentId) -> Option<&'static dyn AgentPlugin> {
    ALL.iter()
        .copied()
        .find(|plugin| &plugin.descriptor().id == agent)
}

/// The agent a session that names none runs: the first registered one.
#[must_use]
pub fn default_agent() -> AgentId {
    ALL[0].descriptor().id.clone()
}

/// Every registered plugin.
#[must_use]
pub fn all() -> &'static [&'static dyn AgentPlugin] {
    &ALL
}

/// Every registered plugin's descriptor, in registration order.
#[must_use]
pub fn descriptors() -> Vec<AgentDescriptor> {
    ALL.iter()
        .map(|plugin| plugin.descriptor().clone())
        .collect()
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;

    #[test]
    fn plugin_finds_claude_by_id() {
        let claude = plugin(&AgentId("claude".to_owned())).expect("claude is registered");
        assert_eq!(claude.program_env_var(), "RUSTLING_TULIP_CLAUDE");
    }

    #[test]
    fn plugin_refuses_an_unknown_id() {
        for id in ["aider", "", "Claude"] {
            assert!(plugin(&AgentId(id.to_owned())).is_none(), "{id:?}");
        }
    }

    #[test]
    fn default_agent_is_the_first_registered() {
        assert_eq!(default_agent(), AgentId("claude".to_owned()));
        let first = plugin(&default_agent()).expect("the default agent is registered");
        assert_eq!(first.program_env_var(), all()[0].program_env_var());
    }

    #[test]
    fn descriptors_list_every_plugin() {
        let descriptors = descriptors();
        assert_eq!(descriptors.len(), all().len());
        for (descriptor, registered) in descriptors.iter().zip(all()) {
            assert_eq!(descriptor, registered.descriptor());
            assert!(plugin(&descriptor.id).is_some(), "{:?}", descriptor.id);
        }
    }

    #[test]
    #[expect(clippy::panic, reason = "the failure names the agent that has no fake")]
    fn every_registered_plugin_has_a_fake_cli_on_disk() {
        for registered in all() {
            let id = &registered.descriptor().id;
            let fake = registered
                .fake_cli()
                .unwrap_or_else(|| panic!("agent {id:?} has no fake CLI"));
            assert!(
                fake.shim.is_file(),
                "agent {id:?}: fake CLI shim {} is not a file",
                fake.shim.display()
            );
        }
    }
}
