//! The typed values of Claude's provider and permission-mode options.

use serde::{Deserialize, Serialize};

/// Which Anthropic-compatible endpoint a Claude Code session talks to. The
/// provider chooses the child's routing environment at spawn; the credential
/// it selects is never stored here.
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
    pub fn is_anthropic(&self) -> bool {
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

impl PermissionMode {
    /// CLI flag value as expected by `claude --permission-mode <X>`.
    #[must_use]
    pub fn as_cli_arg(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AcceptEdits => "acceptEdits",
            Self::BypassPermissions => "bypassPermissions",
            Self::Plan => "plan",
        }
    }
}

/// Claude's `--effort` levels, the ones `claude --help` lists.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
    /// A level from a newer daemon or client.
    #[serde(other)]
    Unknown,
}

impl Effort {
    /// CLI flag value as expected by `claude --effort <X>`; `None` for
    /// [`Effort::Unknown`], which passes no flag.
    #[must_use]
    pub fn as_cli_arg(self) -> Option<&'static str> {
        match self {
            Self::Low => Some("low"),
            Self::Medium => Some("medium"),
            Self::High => Some("high"),
            Self::Xhigh => Some("xhigh"),
            Self::Max => Some("max"),
            Self::Unknown => None,
        }
    }
}
