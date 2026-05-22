//! Cursor (`cursor-agent` CLI from cursor.com) backend.
//!
//! Interactive-only in PR1. Cursor's `--workspace` flag takes a single
//! directory and has no `--add-dir` equivalent, so the dispatcher rejects
//! workspace targets up front (see [`AgentBackend::supports_workspace`]).
//! Headless via `cursor-agent --print --output-format stream-json` is
//! straightforward but deferred to a follow-up alongside codex headless.

use super::{AgentBackend, CommonSpawnFields};
use protocol::{AgentOptions, CursorSandbox, SessionMember};

pub struct CursorBackend;

impl AgentBackend for CursorBackend {
    fn program_env_var(&self) -> &'static str {
        "RUSTLING_TULIP_CURSOR_AGENT"
    }

    fn default_program(&self) -> &'static str {
        "cursor-agent"
    }

    fn supports_workspace(&self) -> bool {
        false
    }

    fn build_interactive_args(
        &self,
        opts: &AgentOptions,
        common: &CommonSpawnFields,
        _members: &[SessionMember],
        initial_prompt: Option<&str>,
    ) -> Vec<String> {
        let (plan_mode, sandbox) = match opts {
            AgentOptions::Cursor { plan_mode, sandbox } => (*plan_mode, *sandbox),
            AgentOptions::Claude { .. } | AgentOptions::Codex { .. } => {
                debug_assert!(false, "cursor backend invoked with non-cursor options");
                (false, None)
            }
        };
        build_args(common, plan_mode, sandbox, initial_prompt)
    }
}

/// Pure arg construction lifted out as a free fn for unit testing.
///
/// Layout:
/// - `--model <id>` when [`CommonSpawnFields::model`] is set
/// - `--plan` when `plan_mode` is true
/// - permission/sandbox: `--yolo` overrides everything; otherwise
///   `--sandbox <value>` when a sandbox is set
/// - trailing positional `<prompt>` when no prompt-injector is attached.
///   Cursor does not expose a system-prompt flag, but workspace mode is
///   gated off (`supports_workspace = false`), so the prelude path doesn't
///   apply.
fn build_args(
    common: &CommonSpawnFields<'_>,
    plan_mode: bool,
    sandbox: Option<CursorSandbox>,
    initial_prompt: Option<&str>,
) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    if let Some(model) = common.model {
        args.push("--model".to_string());
        args.push(model.to_string());
    }
    if plan_mode {
        args.push("--plan".to_string());
    }
    if common.dangerously_skip_permissions {
        args.push("--yolo".to_string());
    } else if let Some(s) = sandbox {
        args.push("--sandbox".to_string());
        args.push(s.as_cli_arg().to_string());
    }
    if !common.has_prompt_injector
        && let Some(prompt) = initial_prompt
    {
        args.push(prompt.to_string());
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn common(skip: bool, model: Option<&str>, has_injector: bool) -> CommonSpawnFields<'_> {
        CommonSpawnFields {
            dangerously_skip_permissions: skip,
            model,
            has_prompt_injector: has_injector,
        }
    }

    #[test]
    fn single_repo_plan_mode_with_sandbox_and_prompt() {
        let args = build_args(
            &common(false, Some("sonnet-4"), false),
            true,
            Some(CursorSandbox::Enabled),
            Some("hello"),
        );
        assert_eq!(
            args,
            vec![
                "--model",
                "sonnet-4",
                "--plan",
                "--sandbox",
                "enabled",
                "hello",
            ]
        );
    }

    #[test]
    fn yolo_overrides_sandbox() {
        let args = build_args(
            &common(true, None, false),
            false,
            Some(CursorSandbox::Disabled),
            Some("go"),
        );
        assert!(args.contains(&"--yolo".to_string()));
        assert!(!args.contains(&"--sandbox".to_string()));
        assert_eq!(args.last(), Some(&"go".to_string()));
    }

    #[test]
    fn no_options_no_prompt_is_empty() {
        let args = build_args(&common(false, None, false), false, None, None);
        assert!(args.is_empty());
    }

    #[test]
    fn injector_present_omits_positional_prompt() {
        let args = build_args(
            &common(false, None, true),
            false,
            None,
            Some("would-be-prompt"),
        );
        assert!(args.is_empty());
    }

    #[test]
    fn plan_mode_without_sandbox_emits_just_plan() {
        let args = build_args(&common(false, None, false), true, None, Some("x"));
        assert_eq!(args, vec!["--plan", "x"]);
    }
}
