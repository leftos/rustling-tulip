//! Codex (`codex` CLI from `@openai/codex`) backend.
//!
//! Interactive-only in PR1. Headless via `codex exec --json` will plug into
//! `build_headless_args` + `handle_headless_line` in a follow-up; until then
//! [`AgentBackend::supports_headless`] returns the default `false` and the
//! spawn dispatcher rejects headless+codex up front.

use super::{AgentBackend, CommonSpawnFields, workspace_prelude};
use protocol::{AgentOptions, CodexSandbox, SessionMember};

pub struct CodexBackend;

impl AgentBackend for CodexBackend {
    fn program_env_var(&self) -> &'static str {
        "RUSTLING_TULIP_CODEX"
    }

    fn default_program(&self) -> &'static str {
        "codex"
    }

    fn build_interactive_args(
        &self,
        opts: &AgentOptions,
        common: &CommonSpawnFields,
        members: &[SessionMember],
        initial_prompt: Option<&str>,
    ) -> Vec<String> {
        let sandbox = match opts {
            AgentOptions::Codex { sandbox } => *sandbox,
            AgentOptions::Claude { .. } => {
                debug_assert!(false, "codex backend invoked with non-codex options");
                None
            }
        };
        build_args(common, sandbox, members, initial_prompt)
    }
}

/// Pure arg construction lifted out as a free fn for unit testing.
///
/// Layout:
/// - `--add-dir <path>` for every extra member after the primary cwd
/// - `--model <id>` when [`CommonSpawnFields::model`] is set
/// - permission/sandbox: `--yolo` overrides everything; otherwise
///   `--sandbox <value>` when a sandbox is set
/// - trailing positional `<prompt>` when no prompt-injector is attached;
///   carries `<workspace_prelude>\n<initial_prompt>` for workspace spawns,
///   or just `<initial_prompt>`, or just `<workspace_prelude>`, depending
///   on which are present. Codex doesn't expose a system-prompt flag so
///   the prelude rides along on the user's first message slot.
fn build_args(
    common: &CommonSpawnFields<'_>,
    sandbox: Option<CodexSandbox>,
    members: &[SessionMember],
    initial_prompt: Option<&str>,
) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    for extra in members.iter().skip(1) {
        args.push("--add-dir".to_string());
        args.push(extra.worktree_path.clone());
    }
    if let Some(model) = common.model {
        args.push("--model".to_string());
        args.push(model.to_string());
    }
    if common.dangerously_skip_permissions {
        args.push("--yolo".to_string());
    } else if let Some(s) = sandbox {
        args.push("--sandbox".to_string());
        args.push(s.as_cli_arg().to_string());
    }
    if !common.has_prompt_injector {
        let prelude = workspace_prelude(members);
        let combined = match (prelude, initial_prompt) {
            (Some(pre), Some(user)) => Some(format!("{pre}\n{user}")),
            (Some(pre), None) => Some(pre),
            (None, Some(user)) => Some(user.to_string()),
            (None, None) => None,
        };
        if let Some(text) = combined {
            args.push(text);
        }
    }
    args
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;

    fn members(paths: &[&str]) -> Vec<SessionMember> {
        paths
            .iter()
            .enumerate()
            .map(|(i, p)| SessionMember {
                repo_id: format!("r{i}"),
                repo_name: format!("repo{i}"),
                branch: "main".to_string(),
                worktree_path: (*p).to_string(),
            })
            .collect()
    }

    fn common(skip: bool, model: Option<&str>, has_injector: bool) -> CommonSpawnFields<'_> {
        CommonSpawnFields {
            dangerously_skip_permissions: skip,
            model,
            has_prompt_injector: has_injector,
        }
    }

    #[test]
    fn workspace_sandbox_and_prompt_round_trip() {
        let m = members(&["X:/dev/a", "X:/dev/b"]);
        let args = build_args(
            &common(false, Some("gpt-5"), false),
            Some(CodexSandbox::WorkspaceWrite),
            &m,
            Some("hello"),
        );
        // --add-dir for the second member.
        assert!(args.windows(2).any(|w| w == ["--add-dir", "X:/dev/b"]));
        // --model.
        assert!(args.windows(2).any(|w| w == ["--model", "gpt-5"]));
        // --sandbox.
        assert!(
            args.windows(2)
                .any(|w| w == ["--sandbox", "workspace-write"])
        );
        // Prompt rides at the tail; for workspace spawns it gets the
        // prelude prefixed.
        let tail = args.last().expect("tail prompt");
        assert!(tail.ends_with("\nhello"));
        assert!(tail.contains("repo0"));
        assert!(tail.contains("repo1"));
    }

    #[test]
    fn yolo_overrides_sandbox() {
        let m = members(&["X:/dev/a"]);
        let args = build_args(
            &common(true, None, false),
            Some(CodexSandbox::ReadOnly),
            &m,
            None,
        );
        assert!(args.contains(&"--yolo".to_string()));
        assert!(!args.contains(&"--sandbox".to_string()));
    }

    #[test]
    fn injector_present_omits_positional_prompt() {
        let m = members(&["X:/dev/a"]);
        let args = build_args(
            &common(false, None, true),
            None,
            &m,
            Some("would-be-prompt"),
        );
        assert!(args.is_empty(), "expected empty args, got {args:?}");
    }

    #[test]
    fn single_repo_no_options_no_prompt_is_empty() {
        let m = members(&["X:/dev/a"]);
        let args = build_args(&common(false, None, false), None, &m, None);
        assert!(args.is_empty());
    }

    #[test]
    fn sandbox_emitted_when_skip_is_false() {
        let m = members(&["X:/dev/a"]);
        let args = build_args(
            &common(false, None, false),
            Some(CodexSandbox::DangerFullAccess),
            &m,
            Some("go"),
        );
        assert!(
            args.windows(2)
                .any(|w| w == ["--sandbox", "danger-full-access"])
        );
        assert_eq!(args.last(), Some(&"go".to_string()));
    }
}
