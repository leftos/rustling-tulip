//! The Claude agent's id, provider and permission-mode values as the client
//! reads and writes them in an [`AgentSelection`]'s options.
//!
//! Temporary: it holds copies of the Claude plugin's option types until the
//! client builds its agent controls from the daemon's `Agents` descriptors.

use protocol::{AgentId, AgentSelection, OptionValue, OptionValues};
use serde::{Deserialize, Serialize};

/// The option key of Claude's permission mode.
const PERMISSION_MODE_KEY: &str = "permission_mode";
/// The option key of Claude's provider.
const PROVIDER_KEY: &str = "provider";
/// The option key of Claude's model.
const MODEL_KEY: &str = "model";

/// Which Anthropic-compatible endpoint a Claude Code session talks to.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClaudeProvider {
    /// Anthropic's own API (the default).
    #[default]
    Anthropic,
    /// `DeepSeek`'s Anthropic-compatible endpoint.
    Deepseek,
    /// A provider from a newer daemon or client.
    #[serde(other)]
    Unknown,
}

impl ClaudeProvider {
    /// True for [`ClaudeProvider::Anthropic`].
    #[must_use]
    pub fn is_anthropic(self) -> bool {
        matches!(self, Self::Anthropic)
    }
}

/// Claude's `--permission-mode` values.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    Default,
    AcceptEdits,
    BypassPermissions,
    Plan,
}

/// The Claude options a spawn carries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaudeChoice {
    pub provider: ClaudeProvider,
    /// `None` leaves the CLI's default.
    pub permission_mode: Option<PermissionMode>,
    /// `None` uses the provider's default model.
    pub model: Option<String>,
}

/// The id the Claude plugin registers under.
#[must_use]
pub fn claude_id() -> AgentId {
    AgentId("claude".to_owned())
}

/// True when `agent` is the Claude plugin.
#[must_use]
pub fn is_claude(agent: &AgentId) -> bool {
    agent.0 == "claude"
}

/// The Claude options `selection` holds; a key it lacks reads as its default.
#[must_use]
pub fn from_selection(selection: &AgentSelection) -> ClaudeChoice {
    from_options(&selection.options)
}

/// The Claude options in `options`; a key it lacks reads as its default.
#[must_use]
pub fn from_options(options: &OptionValues) -> ClaudeChoice {
    ClaudeChoice {
        provider: text(options, PROVIDER_KEY)
            .and_then(parse::<ClaudeProvider>)
            .unwrap_or_default(),
        permission_mode: text(options, PERMISSION_MODE_KEY).and_then(parse::<PermissionMode>),
        model: text(options, MODEL_KEY).map(str::to_owned),
    }
}

/// `choice` as option values, leaving out each key that holds its default.
#[must_use]
pub fn to_options(choice: &ClaudeChoice) -> OptionValues {
    let mut options = OptionValues::new();
    if !choice.provider.is_anthropic()
        && let Some(name) = serde_name(&choice.provider)
    {
        options.insert(PROVIDER_KEY.to_owned(), OptionValue::Text(name));
    }
    if let Some(mode) = choice.permission_mode
        && let Some(name) = serde_name(&mode)
    {
        options.insert(PERMISSION_MODE_KEY.to_owned(), OptionValue::Text(name));
    }
    if let Some(model) = &choice.model {
        options.insert(MODEL_KEY.to_owned(), OptionValue::Text(model.clone()));
    }
    options
}

/// A Claude selection running with `choice`.
#[cfg(test)]
#[must_use]
pub fn selection(choice: &ClaudeChoice) -> AgentSelection {
    AgentSelection {
        agent: claude_id(),
        options: to_options(choice),
    }
}

fn text<'a>(options: &'a OptionValues, key: &str) -> Option<&'a str> {
    match options.get(key) {
        Some(OptionValue::Text(value)) => Some(value.as_str()),
        _ => None,
    }
}

fn parse<T: for<'de> Deserialize<'de>>(value: &str) -> Option<T> {
    serde_json::from_value(serde_json::Value::String(value.to_owned())).ok()
}

fn serde_name<T: Serialize>(value: &T) -> Option<String> {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(name)) => Some(name),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ClaudeChoice, ClaudeProvider, PROVIDER_KEY, PermissionMode, claude_id, from_options,
        from_selection, is_claude, selection,
    };
    use protocol::{OptionValue, OptionValues};

    #[test]
    fn to_options_then_from_selection_round_trips() {
        let choice = ClaudeChoice {
            provider: ClaudeProvider::Deepseek,
            permission_mode: Some(PermissionMode::AcceptEdits),
            model: Some("deepseek-chat".to_owned()),
        };
        let chosen = selection(&choice);
        assert!(is_claude(&chosen.agent));
        assert_eq!(chosen.agent, claude_id());
        assert_eq!(
            chosen.options.get(PROVIDER_KEY),
            Some(&OptionValue::Text("deepseek".to_owned()))
        );
        assert_eq!(from_selection(&chosen), choice);
    }

    #[test]
    fn an_unknown_provider_reads_as_unknown() {
        let mut options = OptionValues::new();
        options.insert(
            PROVIDER_KEY.to_owned(),
            OptionValue::Text("from-a-newer-daemon".to_owned()),
        );
        assert_eq!(from_options(&options).provider, ClaudeProvider::Unknown);
    }
}
