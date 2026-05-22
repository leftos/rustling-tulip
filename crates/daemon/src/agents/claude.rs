//! Claude (`claude` CLI from `@anthropic-ai/claude-code`) backend.
//!
//! Owns the argv layout for both interactive and headless spawns, plus the
//! stream-json parser that drives [`SessionMetrics`] and `recent_actions`
//! updates while a headless session is running.

use super::{AgentBackend, CommonSpawnFields, workspace_prelude};
use crate::session::{SessionRegistry, push_recent_action};
use protocol::{AgentOptions, PermissionMode, SessionMember, SessionStatus};
use serde::Deserialize;
use tracing::debug;

pub struct ClaudeBackend;

impl AgentBackend for ClaudeBackend {
    fn program_env_var(&self) -> &'static str {
        "RUSTLING_TULIP_CLAUDE"
    }

    fn default_program(&self) -> &'static str {
        "claude"
    }

    fn supports_headless(&self) -> bool {
        true
    }

    fn build_interactive_args(
        &self,
        opts: &AgentOptions,
        common: &CommonSpawnFields,
        members: &[SessionMember],
        initial_prompt: Option<&str>,
    ) -> Vec<String> {
        let permission_mode = match opts {
            AgentOptions::Claude { permission_mode } => *permission_mode,
            AgentOptions::Codex { .. } => {
                debug_assert!(false, "claude backend invoked with non-claude options");
                None
            }
        };
        let mut args: Vec<String> = Vec::new();
        for extra in members.iter().skip(1) {
            args.push("--add-dir".to_string());
            args.push(extra.worktree_path.clone());
        }
        // Workspace-context prelude rides on --append-system-prompt so it's
        // invisible to the user but the model sees it. Only emitted for 2+
        // member sessions (where the worktree-vs-original-path mismatch
        // actually matters).
        if let Some(prelude) = workspace_prelude(members) {
            args.push("--append-system-prompt".to_string());
            args.push(prelude);
        }
        extend_common(&mut args, common, permission_mode);
        // When a prompt injector is attached, it carries the prompt as
        // scripted PTY input — passing `-p` would race the injector and
        // disable plan mode. The injector path takes over.
        if !common.has_prompt_injector
            && let Some(prompt) = initial_prompt
        {
            args.push("-p".to_string());
            args.push(prompt.to_string());
        }
        args
    }

    fn build_headless_args(
        &self,
        opts: &AgentOptions,
        common: &CommonSpawnFields,
        members: &[SessionMember],
        initial_prompt: &str,
    ) -> Vec<String> {
        let permission_mode = match opts {
            AgentOptions::Claude { permission_mode } => *permission_mode,
            AgentOptions::Codex { .. } => {
                debug_assert!(false, "claude backend invoked with non-claude options");
                None
            }
        };
        let mut args: Vec<String> = vec![
            "--print".to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--verbose".to_string(),
        ];
        for extra in members.iter().skip(1) {
            args.push("--add-dir".to_string());
            args.push(extra.worktree_path.clone());
        }
        if let Some(prelude) = workspace_prelude(members) {
            args.push("--append-system-prompt".to_string());
            args.push(prelude);
        }
        extend_common(&mut args, common, permission_mode);
        args.push("-p".to_string());
        args.push(initial_prompt.to_string());
        args
    }

    fn handle_headless_line(&self, registry: &SessionRegistry, session_id: &str, raw_line: &str) {
        let parsed = match serde_json::from_str::<StreamEvent>(raw_line) {
            Ok(p) => p,
            Err(err) => {
                debug!(?err, line = raw_line, "unrecognized stream-json event");
                return;
            }
        };
        registry.update(session_id, |rec| match parsed {
            StreamEvent::System { subtype, .. } => {
                rec.status = SessionStatus::Working;
                push_recent_action(rec, format!("system: {subtype}"));
            }
            StreamEvent::Assistant { message } => {
                if let Some(content) = message.and_then(|m| m.content) {
                    for block in content {
                        match block {
                            ContentBlock::ToolUse { name, .. } => {
                                push_recent_action(rec, format!("tool: {name}"));
                            }
                            ContentBlock::Text { text } => {
                                let trimmed = text.trim();
                                if !trimmed.is_empty() {
                                    let snippet = trimmed.chars().take(120).collect::<String>();
                                    push_recent_action(rec, format!("assistant: {snippet}"));
                                }
                            }
                            ContentBlock::Other => {}
                        }
                    }
                }
            }
            StreamEvent::Result {
                total_cost_usd,
                usage,
                subtype,
                ..
            } => {
                if let Some(cost) = total_cost_usd {
                    rec.metrics.cost_usd = cost;
                }
                if let Some(u) = usage {
                    rec.metrics.input_tokens = u.input_tokens.unwrap_or(0);
                    rec.metrics.output_tokens = u.output_tokens.unwrap_or(0);
                }
                rec.status = SessionStatus::Stopped;
                push_recent_action(rec, format!("result: {subtype}"));
            }
            StreamEvent::User {} | StreamEvent::Other => {}
        });
    }
}

/// Append claude's `--model`, `--permission-mode`, and
/// `--dangerously-skip-permissions` args. The claude CLI rejects
/// `--permission-mode` together with `--dangerously-skip-permissions`, so
/// the latter wins when both are set.
fn extend_common(
    args: &mut Vec<String>,
    common: &CommonSpawnFields<'_>,
    permission_mode: Option<PermissionMode>,
) {
    if let Some(model) = common.model {
        args.push("--model".to_string());
        args.push(model.to_string());
    }
    if common.dangerously_skip_permissions {
        args.push("--dangerously-skip-permissions".to_string());
    } else if let Some(mode) = permission_mode {
        args.push("--permission-mode".to_string());
        args.push(mode.as_cli_arg().to_string());
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StreamEvent {
    System {
        #[serde(default)]
        subtype: String,
    },
    Assistant {
        #[serde(default)]
        message: Option<AssistantMessage>,
    },
    User {},
    Result {
        #[serde(default)]
        subtype: String,
        #[serde(default)]
        total_cost_usd: Option<f64>,
        #[serde(default)]
        usage: Option<UsageInfo>,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct AssistantMessage {
    #[serde(default)]
    content: Option<Vec<ContentBlock>>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlock {
    Text {
        #[serde(default)]
        text: String,
    },
    ToolUse {
        #[serde(default)]
        name: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct UsageInfo {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
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
    fn interactive_single_repo_with_model() {
        let m = members(&["X:/dev/a"]);
        let opts = AgentOptions::Claude {
            permission_mode: Some(PermissionMode::AcceptEdits),
        };
        let args = ClaudeBackend.build_interactive_args(
            &opts,
            &common(false, Some("opus"), false),
            &m,
            Some("hello"),
        );
        assert_eq!(
            args,
            vec![
                "--model",
                "opus",
                "--permission-mode",
                "acceptEdits",
                "-p",
                "hello",
            ]
        );
    }

    #[test]
    fn interactive_skip_perms_overrides_permission_mode() {
        let m = members(&["X:/dev/a"]);
        let opts = AgentOptions::Claude {
            permission_mode: Some(PermissionMode::Plan),
        };
        let args =
            ClaudeBackend.build_interactive_args(&opts, &common(true, None, false), &m, Some("hi"));
        assert!(args.contains(&"--dangerously-skip-permissions".to_string()));
        assert!(!args.contains(&"--permission-mode".to_string()));
    }

    #[test]
    fn interactive_workspace_emits_add_dir_and_prelude() {
        let m = members(&["X:/dev/a", "X:/dev/b"]);
        let opts = AgentOptions::Claude {
            permission_mode: None,
        };
        let args =
            ClaudeBackend.build_interactive_args(&opts, &common(false, None, false), &m, None);
        // --add-dir for the second member only (first is cwd).
        assert!(args.windows(2).any(|w| w == ["--add-dir", "X:/dev/b"]));
        // Workspace prelude emitted via --append-system-prompt.
        let idx = args
            .iter()
            .position(|a| a == "--append-system-prompt")
            .expect("prelude flag");
        assert!(args[idx + 1].contains("repo0"));
        assert!(args[idx + 1].contains("repo1"));
    }

    #[test]
    fn interactive_with_injector_omits_prompt_flag() {
        let m = members(&["X:/dev/a"]);
        let opts = AgentOptions::Claude {
            permission_mode: None,
        };
        let args = ClaudeBackend.build_interactive_args(
            &opts,
            &common(false, None, true),
            &m,
            Some("would-be-prompt"),
        );
        assert!(!args.contains(&"-p".to_string()));
        assert!(!args.iter().any(|a| a == "would-be-prompt"));
    }

    #[test]
    fn headless_args_include_print_and_stream_json() {
        let m = members(&["X:/dev/a"]);
        let opts = AgentOptions::Claude {
            permission_mode: None,
        };
        let args = ClaudeBackend.build_headless_args(
            &opts,
            &common(false, Some("sonnet"), false),
            &m,
            "hello world",
        );
        assert!(args.starts_with(&[
            "--print".to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--verbose".to_string(),
        ]));
        assert!(args.ends_with(&["-p".to_string(), "hello world".to_string()]));
        assert!(args.contains(&"--model".to_string()));
    }

    #[test]
    fn headless_workspace_emits_add_dir_and_prelude() {
        let m = members(&["X:/dev/a", "X:/dev/b"]);
        let opts = AgentOptions::Claude {
            permission_mode: None,
        };
        let args = ClaudeBackend.build_headless_args(&opts, &common(false, None, false), &m, "go");
        assert!(args.windows(2).any(|w| w == ["--add-dir", "X:/dev/b"]));
        assert!(args.iter().any(|a| a == "--append-system-prompt"));
    }
}
