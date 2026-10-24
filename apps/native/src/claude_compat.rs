//! The Claude agent's id, and for tests its provider and permission-mode
//! values as a selection's options carry them.
//!
//! Temporary: the Settings page still finds Claude's page by its id until
//! the client draws one page per descriptor.

use protocol::AgentId;
#[cfg(test)]
use protocol::{OptionValue, OptionValues};
#[cfg(test)]
use serde::{Deserialize, Serialize};

/// The option key of Claude's permission mode.
#[cfg(test)]
const PERMISSION_MODE_KEY: &str = "permission_mode";
/// The option key of Claude's provider.
#[cfg(test)]
const PROVIDER_KEY: &str = "provider";
/// The option key of Claude's model.
#[cfg(test)]
const MODEL_KEY: &str = "model";

/// Which Anthropic-compatible endpoint a Claude Code session talks to.
#[cfg(test)]
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

/// Claude's `--permission-mode` values.
#[cfg(test)]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    Default,
    AcceptEdits,
    BypassPermissions,
    Plan,
}

/// The Claude options a spawn carries.
#[cfg(test)]
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

/// `choice` as option values, leaving out each key that holds its default.
#[cfg(test)]
#[must_use]
pub fn to_options(choice: &ClaudeChoice) -> OptionValues {
    let mut options = OptionValues::new();
    if choice.provider != ClaudeProvider::Anthropic
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
pub fn selection(choice: &ClaudeChoice) -> protocol::AgentSelection {
    protocol::AgentSelection {
        agent: claude_id(),
        options: to_options(choice),
    }
}

#[cfg(test)]
fn serde_name<T: Serialize>(value: &T) -> Option<String> {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(name)) => Some(name),
        _ => None,
    }
}
