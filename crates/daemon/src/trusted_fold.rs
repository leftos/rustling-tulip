//! The trusted flag older releases saved and older peers still send, folded into the agent's
//! bypass choice as its plugin's descriptor names it.

use protocol::{AgentDescriptor, AgentId, AgentSelection, OptionValues, SpawnConfig, SpawnRequest};

/// The descriptor of the plugin `selection` names, when one is registered.
fn descriptor_of(selection: Option<&AgentSelection>) -> Option<&'static AgentDescriptor> {
    selection
        .and_then(|selection| agent_registry::plugin(&selection.agent))
        .map(agent_api::AgentPlugin::descriptor)
}

/// Fold a saved spawn config's trusted flag into its agent's bypass choice.
///
/// Args:
///     config: The config, changed in place.
///
/// Returns:
///     True when the flag or the options changed.
pub(crate) fn fold_config(config: &mut SpawnConfig) -> bool {
    let descriptor = descriptor_of(config.agent.as_ref());
    config.fold_trusted(descriptor)
}

/// Fold a received spawn request's trusted flag into its agent's bypass choice, so the flag is
/// true exactly when the options hold that choice.
///
/// Args:
///     request: The request, changed in place.
///
/// Returns:
///     True when the flag or the options changed.
pub(crate) fn fold_request(request: &mut SpawnRequest) -> bool {
    let descriptor = descriptor_of(request.agent.as_ref());
    request.fold_trusted(descriptor)
}

/// Set `agent`'s bypass choice in `options` when `trusted` is set.
///
/// Args:
///     agent: The agent the options are for.
///     trusted: The saved trusted flag.
///     options: The agent's options, changed in place.
///
/// Returns:
///     True when the options changed; false when the flag is unset, the agent is not registered
///     or has no bypass choice, or the options already held it.
pub(crate) fn fold_options(agent: &AgentId, trusted: bool, options: &mut OptionValues) -> bool {
    trusted
        && agent_registry::plugin(agent)
            .is_some_and(|plugin| plugin.descriptor().set_bypass(options))
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{OptionValue, SessionMode, SpawnTarget};

    fn request(agent: &str, options: OptionValues) -> SpawnRequest {
        SpawnRequest {
            label: None,
            target: SpawnTarget::Standalone {
                cwd: None,
                add_dirs: Vec::new(),
            },
            mode: SessionMode::Interactive,
            initial_prompt: None,
            dangerously_skip_permissions: true,
            agent: Some(AgentSelection {
                agent: AgentId(agent.to_owned()),
                options,
            }),
            extra_args: Vec::new(),
            extra_env: Vec::new(),
            secret_env_keys: Vec::new(),
            prompt_injector: None,
            request_id: None,
            resume_conversation: None,
        }
    }

    #[test]
    fn a_trusted_request_from_an_older_client_runs_in_bypass_mode() {
        let plan = OptionValues::from([(
            "permission_mode".to_owned(),
            OptionValue::Text("plan".to_owned()),
        )]);
        let mut claude = request("claude", plan);
        assert!(fold_request(&mut claude));
        assert!(claude.dangerously_skip_permissions);
        assert_eq!(
            claude.agent.map(|selection| selection.options),
            Some(OptionValues::from([(
                "permission_mode".to_owned(),
                OptionValue::Text("bypass_permissions".to_owned()),
            )]))
        );

        let mut test_agent = request("test-agent", OptionValues::new());
        assert!(fold_request(&mut test_agent));
        assert!(
            !test_agent.dangerously_skip_permissions,
            "an agent with no bypass choice runs with its prompts"
        );
    }
}
