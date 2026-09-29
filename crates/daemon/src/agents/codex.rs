//! Codex (`codex` CLI from `@openai/codex`) backend.
//!
//! Interactive uses the bare `codex` invocation; headless uses
//! `codex exec --json`. The JSONL event shape is defined in
//! `openai/codex` → `codex-rs/exec/src/exec_events.rs`.

use super::{AgentBackend, CommonSpawnFields, workspace_prelude};
use crate::session::{SessionRegistry, push_recent_action};
use protocol::{AgentOptions, CodexSandbox, SessionMember, SessionStatus};
use serde::Deserialize;
use tracing::debug;

pub struct CodexBackend;

impl AgentBackend for CodexBackend {
    fn program_env_var(&self) -> &'static str {
        "RUSTLING_TULIP_CODEX"
    }

    fn default_program(&self) -> &'static str {
        "codex"
    }

    fn supports_headless(&self) -> bool {
        true
    }

    /// A resumed spawn keeps the resumed id at once: `codex resume` appends
    /// to the original rollout, whose name predates this spawn. A fresh
    /// run's id comes from its rollout once found.
    fn own_conversation_at_spawn(
        &self,
        resume: Option<&str>,
        _created: Option<&str>,
    ) -> Option<String> {
        resume.map(str::to_owned)
    }

    fn build_interactive_args(
        &self,
        opts: &AgentOptions,
        common: &CommonSpawnFields,
        members: &[SessionMember],
        initial_prompt: Option<&str>,
    ) -> Vec<String> {
        let sandbox = sandbox_of(opts);
        build_args(common, sandbox, members, initial_prompt)
    }

    fn build_headless_args(
        &self,
        opts: &AgentOptions,
        common: &CommonSpawnFields,
        members: &[SessionMember],
        initial_prompt: &str,
    ) -> Vec<String> {
        // `codex exec --json` emits the same event stream documented in
        // `exec_events.rs`. Flags pass through the same way as the
        // interactive path, just behind the `exec` subcommand + `--json`.
        let sandbox = sandbox_of(opts);
        let mut args: Vec<String> = vec!["exec".to_string(), "--json".to_string()];
        if let Some(model) = common.model {
            args.push("--model".to_string());
            args.push(model.to_string());
        }
        if common.dangerously_skip_permissions {
            args.push("--dangerously-bypass-approvals-and-sandbox".to_string());
        } else if let Some(s) = sandbox {
            args.push("--sandbox".to_string());
            args.push(s.as_cli_arg().to_string());
        }
        for extra in members.iter().skip(1) {
            args.push("--add-dir".to_string());
            args.push(extra.worktree_path.clone());
        }
        // Trailing positional: workspace prelude + user prompt, same shape
        // as the interactive path (codex has no system-prompt flag).
        let prelude = workspace_prelude(members);
        let combined = match prelude {
            Some(pre) => format!("{pre}\n{initial_prompt}"),
            None => initial_prompt.to_string(),
        };
        args.push(combined);
        args
    }

    fn handle_headless_line(&self, registry: &SessionRegistry, session_id: &str, raw_line: &str) {
        let parsed = match serde_json::from_str::<ExecEvent>(raw_line) {
            Ok(p) => p,
            Err(err) => {
                debug!(?err, line = raw_line, "unrecognized codex exec event");
                return;
            }
        };
        registry.update(session_id, |rec| match parsed {
            ExecEvent::ThreadStarted { thread_id } => {
                if thread_id.is_some() {
                    rec.agent_conversation_id = thread_id;
                }
                rec.status = SessionStatus::Working;
                push_recent_action(rec, "thread started".to_string());
            }
            ExecEvent::TurnStarted => {
                rec.status = SessionStatus::Working;
            }
            ExecEvent::TurnCompleted { usage } => {
                if let Some(u) = usage {
                    // codex emits cumulative-per-turn counts; mirror claude's
                    // last-wins semantics (claude reports a single Result
                    // event per session, codex emits one per turn).
                    if let Some(input) = u.input_tokens {
                        rec.metrics.input_tokens = u64::try_from(input).unwrap_or(0);
                    }
                    if let Some(output) = u.output_tokens {
                        rec.metrics.output_tokens = u64::try_from(output).unwrap_or(0);
                    }
                }
                push_recent_action(rec, "turn complete".to_string());
            }
            ExecEvent::TurnFailed { error } => {
                rec.status = SessionStatus::Error;
                push_recent_action(rec, format!("turn failed: {}", error.message));
            }
            ExecEvent::ItemStarted { item } | ExecEvent::ItemUpdated { item } => {
                handle_item(rec, item, false);
            }
            ExecEvent::ItemCompleted { item } => {
                handle_item(rec, item, true);
            }
            ExecEvent::Error { message } => {
                rec.status = SessionStatus::Error;
                push_recent_action(rec, format!("error: {message}"));
            }
            ExecEvent::Other => {}
        });
    }
}

fn sandbox_of(opts: &AgentOptions) -> Option<CodexSandbox> {
    match opts {
        AgentOptions::Codex { sandbox } => *sandbox,
        AgentOptions::Claude { .. } | AgentOptions::Cursor { .. } => {
            debug_assert!(false, "codex backend invoked with non-codex options");
            None
        }
    }
}

/// Apply one item-event to the session record. `is_terminal` is true only
/// for `item.completed` — used to suppress noisy progress chatter from
/// `item.updated` (text streaming, file-change progress) so `recent_actions`
/// only carries the meaningful "this happened" lines.
fn handle_item(rec: &mut crate::session::SessionRecord, item: ThreadItem, is_terminal: bool) {
    match item.details {
        ThreadItemDetails::AgentMessage { text } => {
            // Only on completion — `item.updated` for an agent_message would
            // emit the same line repeatedly as streaming text grows.
            if is_terminal {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    let snippet = trimmed.chars().take(120).collect::<String>();
                    push_recent_action(rec, format!("assistant: {snippet}"));
                }
            }
        }
        ThreadItemDetails::CommandExecution {
            command, status, ..
        } => {
            // Emit once at completion (terminal) so we don't spam the feed.
            if is_terminal {
                let snippet = command.chars().take(80).collect::<String>();
                push_recent_action(rec, format!("shell ({status}): {snippet}"));
            }
        }
        ThreadItemDetails::FileChange { changes, status } => {
            if is_terminal {
                let summary = changes
                    .iter()
                    .map(|c| format!("{} {}", c.kind, c.path))
                    .collect::<Vec<_>>()
                    .join(", ");
                push_recent_action(rec, format!("file_change ({status}): {summary}"));
            }
        }
        ThreadItemDetails::McpToolCall { server, tool, .. } => {
            if is_terminal {
                push_recent_action(rec, format!("mcp: {server}/{tool}"));
            }
        }
        ThreadItemDetails::WebSearch { query, .. } => {
            if is_terminal {
                push_recent_action(rec, format!("web_search: {query}"));
            }
        }
        // Quiet — these variants would flood the recent-actions feed, and
        // `Other` covers future variants we don't know about yet.
        ThreadItemDetails::TodoList {}
        | ThreadItemDetails::Reasoning
        | ThreadItemDetails::CollabToolCall {}
        | ThreadItemDetails::Other => {}
        ThreadItemDetails::Error { message } => {
            rec.status = SessionStatus::Error;
            push_recent_action(rec, format!("item error: {message}"));
        }
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
///
/// With [`CommonSpawnFields::resume_conversation`] set, the form is
/// `resume -C <cwd> [--add-dir P]... [--model M]
/// [--dangerously-bypass-approvals-and-sandbox | --sandbox S] <id>`: no
/// prompt or prelude (the conversation already holds them), `-C` only when
/// [`CommonSpawnFields::cwd`] is set, and the flags always re-passed.
fn build_args(
    common: &CommonSpawnFields<'_>,
    sandbox: Option<CodexSandbox>,
    members: &[SessionMember],
    initial_prompt: Option<&str>,
) -> Vec<String> {
    if let Some(id) = common.resume_conversation {
        return build_resume_args(common, sandbox, members, id);
    }
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

/// The `codex resume` form of [`build_args`]. `--yolo` is not listed by
/// `codex resume --help`, so the long bypass flag is passed instead.
fn build_resume_args(
    common: &CommonSpawnFields<'_>,
    sandbox: Option<CodexSandbox>,
    members: &[SessionMember],
    id: &str,
) -> Vec<String> {
    let mut args: Vec<String> = vec!["resume".to_string()];
    if let Some(cwd) = common.cwd {
        args.push("-C".to_string());
        args.push(cwd.to_string());
    }
    for extra in members.iter().skip(1) {
        args.push("--add-dir".to_string());
        args.push(extra.worktree_path.clone());
    }
    if let Some(model) = common.model {
        args.push("--model".to_string());
        args.push(model.to_string());
    }
    if common.dangerously_skip_permissions {
        args.push("--dangerously-bypass-approvals-and-sandbox".to_string());
    } else if let Some(s) = sandbox {
        args.push("--sandbox".to_string());
        args.push(s.as_cli_arg().to_string());
    }
    args.push(id.to_string());
    args
}

// ---------------------------------------------------------------------------
// codex exec --json event schema.
//
// Mirrors `openai/codex` → `codex-rs/exec/src/exec_events.rs`. The top-level
// `type` discriminant carries dotted forms like `thread.started`, so each
// variant uses an explicit `#[serde(rename = "...")]`. Unknown event types
// fall into `Other` via `#[serde(other)]` and are silently dropped — codex's
// schema is allowed to grow without breaking older daemons.

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ExecEvent {
    /// `thread_id` is the id `codex exec resume <id>` resumes.
    #[serde(rename = "thread.started")]
    ThreadStarted {
        #[serde(default)]
        thread_id: Option<String>,
    },
    #[serde(rename = "turn.started")]
    TurnStarted,
    #[serde(rename = "turn.completed")]
    TurnCompleted {
        #[serde(default)]
        usage: Option<Usage>,
    },
    #[serde(rename = "turn.failed")]
    TurnFailed { error: ThreadErrorEvent },
    #[serde(rename = "item.started")]
    ItemStarted { item: ThreadItem },
    #[serde(rename = "item.updated")]
    ItemUpdated { item: ThreadItem },
    #[serde(rename = "item.completed")]
    ItemCompleted { item: ThreadItem },
    #[serde(rename = "error")]
    Error {
        #[serde(default)]
        message: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: Option<i64>,
    #[serde(default)]
    output_tokens: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct ThreadErrorEvent {
    #[serde(default)]
    message: String,
}

#[derive(Debug, Deserialize)]
struct ThreadItem {
    #[serde(flatten)]
    details: ThreadItemDetails,
}

// `ThreadItem` is parsed at a nested level (inside the outer envelope's
// `item` field), so its `type` discriminator doesn't collide with the
// outer `ExecEvent`'s `type`.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ThreadItemDetails {
    AgentMessage {
        #[serde(default)]
        text: String,
    },
    Reasoning,
    CommandExecution {
        #[serde(default)]
        command: String,
        #[serde(default)]
        status: ItemStatus,
    },
    FileChange {
        #[serde(default)]
        changes: Vec<FileChange>,
        #[serde(default)]
        status: ItemStatus,
    },
    McpToolCall {
        #[serde(default)]
        server: String,
        #[serde(default)]
        tool: String,
    },
    WebSearch {
        #[serde(default)]
        query: String,
    },
    TodoList {},
    CollabToolCall {},
    Error {
        #[serde(default)]
        message: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct FileChange {
    #[serde(default)]
    path: String,
    #[serde(default)]
    kind: PatchKind,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PatchKind {
    Add,
    Delete,
    Update,
    #[default]
    #[serde(other)]
    Other,
}

impl std::fmt::Display for PatchKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Add => "add",
            Self::Delete => "delete",
            Self::Update => "update",
            Self::Other => "other",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ItemStatus {
    InProgress,
    #[default]
    Completed,
    Failed,
    Declined,
    #[serde(other)]
    Other,
}

impl std::fmt::Display for ItemStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Declined => "declined",
            Self::Other => "other",
        };
        f.write_str(s)
    }
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
            claude_session_id: None,
            resume_conversation: None,
            add_dirs: &[],
            cwd: None,
        }
    }

    const RESUME_ID: &str = "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b";

    fn resuming<'a>(
        skip: bool,
        model: Option<&'a str>,
        cwd: Option<&'a str>,
    ) -> CommonSpawnFields<'a> {
        CommonSpawnFields {
            resume_conversation: Some(RESUME_ID),
            cwd,
            ..common(skip, model, false)
        }
    }

    #[test]
    fn resume_passes_cd_flags_and_id_without_prompt() {
        let m = members(&["X:/dev/a"]);
        let args = build_args(
            &resuming(false, Some("gpt-5"), Some("X:/dev/a")),
            Some(CodexSandbox::WorkspaceWrite),
            &m,
            Some("hello"),
        );
        assert_eq!(
            args,
            [
                "resume",
                "-C",
                "X:/dev/a",
                "--model",
                "gpt-5",
                "--sandbox",
                "workspace-write",
                RESUME_ID
            ]
        );
        let without_cwd = build_args(&resuming(false, None, None), None, &m, Some("hello"));
        assert_eq!(without_cwd, ["resume", RESUME_ID]);
    }

    #[test]
    fn resume_uses_bypass_flag_not_yolo() {
        let m = members(&["X:/dev/a"]);
        let args = build_args(
            &resuming(true, None, Some("X:/dev/a")),
            Some(CodexSandbox::ReadOnly),
            &m,
            None,
        );
        assert_eq!(
            args,
            [
                "resume",
                "-C",
                "X:/dev/a",
                "--dangerously-bypass-approvals-and-sandbox",
                RESUME_ID
            ]
        );
    }

    #[test]
    fn resume_workspace_keeps_add_dirs_and_drops_prelude() {
        let m = members(&["X:/dev/a", "X:/dev/b"]);
        let args = build_args(
            &resuming(false, None, Some("X:/dev/a")),
            None,
            &m,
            Some("hello"),
        );
        assert_eq!(
            args,
            [
                "resume",
                "-C",
                "X:/dev/a",
                "--add-dir",
                "X:/dev/b",
                RESUME_ID
            ]
        );
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

    #[test]
    fn thread_started_without_an_id_still_turns_the_session_working() {
        use crate::history::test_support::{record, scratch_dirs};
        use protocol::{Agent, SessionMode};
        let dirs = scratch_dirs("codex-thread-started-no-id");
        let registry = SessionRegistry::new(dirs.clone());
        let mut rec = record("h2", SessionMode::Headless);
        rec.agent = Agent::Codex;
        registry.insert(rec);

        CodexBackend.handle_headless_line(&registry, "h2", r#"{"type":"thread.started"}"#);

        let rec = registry.get("h2").expect("record");
        let rec = crate::sync::lock(&rec);
        assert_eq!(rec.status, SessionStatus::Working);
        assert_eq!(rec.agent_conversation_id, None);
        drop(rec);
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn thread_started_line_sets_the_headless_record_id() {
        use crate::history::test_support::{record, scratch_dirs};
        use protocol::{Agent, SessionMode};
        let dirs = scratch_dirs("codex-thread-started");
        let registry = SessionRegistry::new(dirs.clone());
        let mut rec = record("h1", SessionMode::Headless);
        rec.agent = Agent::Codex;
        registry.insert(rec);

        CodexBackend.handle_headless_line(
            &registry,
            "h1",
            r#"{"type":"thread.started","thread_id":"0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b"}"#,
        );

        let rec = registry.get("h1").expect("record");
        assert_eq!(
            crate::sync::lock(&rec).agent_conversation_id.as_deref(),
            Some("0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b")
        );
        let _ = std::fs::remove_dir_all(&dirs.config);
    }
}
