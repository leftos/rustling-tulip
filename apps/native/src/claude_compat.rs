//! The Claude agent's id, provider and permission-mode values as the client
//! reads and writes them in a selection's options.
//!
//! Temporary: it holds copies of the Claude plugin's option types until the
//! client builds its agent controls from the daemon's `Agents` descriptors.

use protocol::{AgentId, OptionValue, OptionValues};
use serde::{Deserialize, Serialize};

/// The option key of Claude's permission mode.
pub(crate) const PERMISSION_MODE_KEY: &str = "permission_mode";
/// The option key of Claude's provider.
pub(crate) const PROVIDER_KEY: &str = "provider";
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

/// The providers a Claude spawn can pick, Anthropic first.
pub(crate) const PROVIDER_CHOICES: [ClaudeProvider; 2] =
    [ClaudeProvider::Anthropic, ClaudeProvider::Deepseek];

/// A provider's label in Settings and in every other place the client
/// names one. The client's one `ClaudeProvider` -> name table: a provider
/// added later is named here and nowhere else.
pub(crate) fn provider_label(provider: ClaudeProvider) -> &'static str {
    match provider {
        ClaudeProvider::Anthropic => "Anthropic",
        ClaudeProvider::Deepseek => "DeepSeek",
        ClaudeProvider::Unknown => "Unknown",
    }
}

/// Claude's approval modes, the CLI default first.
pub(crate) const APPROVAL_CHOICES: [Option<PermissionMode>; 5] = [
    None,
    Some(PermissionMode::Default),
    Some(PermissionMode::AcceptEdits),
    Some(PermissionMode::BypassPermissions),
    Some(PermissionMode::Plan),
];

pub(crate) fn approval_label(mode: Option<PermissionMode>) -> &'static str {
    match mode {
        None => "CLI default",
        Some(PermissionMode::Default) => "default",
        Some(PermissionMode::AcceptEdits) => "accept edits",
        Some(PermissionMode::BypassPermissions) => "bypass permissions",
        Some(PermissionMode::Plan) => "plan",
    }
}

/// `mode` as Claude's `permission_mode` option value: no value for the CLI
/// default.
pub(crate) fn permission_mode_value(mode: Option<PermissionMode>) -> OptionValue {
    let name = match mode {
        None => return OptionValue::Null,
        Some(PermissionMode::Default) => "default",
        Some(PermissionMode::AcceptEdits) => "accept_edits",
        Some(PermissionMode::BypassPermissions) => "bypass_permissions",
        Some(PermissionMode::Plan) => "plan",
    };
    OptionValue::Text(name.to_owned())
}

/// `provider` as Claude's `provider` option value.
pub(crate) fn provider_value(provider: ClaudeProvider) -> OptionValue {
    let name = match provider {
        ClaudeProvider::Anthropic => "anthropic",
        ClaudeProvider::Deepseek => "deepseek",
        ClaudeProvider::Unknown => "unknown",
    };
    OptionValue::Text(name.to_owned())
}

/// The note under Claude's approval choices while trusted launch is on.
pub(crate) const CLAUDE_LOCKED: &str = "Ignored while trusted launch is on. Claude will run without --permission-mode. The chosen value is preserved for when you toggle trusted launch off.";

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
#[cfg(test)]
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
pub fn selection(choice: &ClaudeChoice) -> protocol::AgentSelection {
    protocol::AgentSelection {
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

#[cfg(test)]
fn serde_name<T: Serialize>(value: &T) -> Option<String> {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(name)) => Some(name),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        APPROVAL_CHOICES, ClaudeProvider, PERMISSION_MODE_KEY, PROVIDER_CHOICES, PROVIDER_KEY,
        from_options, permission_mode_value, provider_value,
    };
    use protocol::{OptionValue, OptionValues};

    #[test]
    fn option_values_read_back_as_their_choice() {
        for mode in APPROVAL_CHOICES {
            let options =
                OptionValues::from([(PERMISSION_MODE_KEY.to_owned(), permission_mode_value(mode))]);
            assert_eq!(from_options(&options).permission_mode, mode);
        }
        for provider in PROVIDER_CHOICES {
            let options = OptionValues::from([(PROVIDER_KEY.to_owned(), provider_value(provider))]);
            assert_eq!(from_options(&options).provider, provider);
        }
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
