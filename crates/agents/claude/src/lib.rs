//! Claude (`claude` CLI from `@anthropic-ai/claude-code`) plugin.
//!
//! Owns Claude's descriptor and option schema with the [`ClaudeOptions`] they
//! decode into, the argv layout for both interactive and headless spawns, the
//! hook plan whose settings document an interactive spawn passes as
//! `--settings`, the resume command a recovered shell types, and the
//! stream-json parser that turns a headless session's output into
//! [`HeadlessEvent`]s.

mod hooks;
mod import;
pub mod options;
mod providers;
mod rules;
mod transcripts;

pub use providers::{display_name, provider_for_model};
pub use transcripts::encode_project_dir;

use agent_api::{
    Agent, AgentError, ConversationPlan, ConversationQuery, EnvPlan, FoundConversation,
    HeadlessEvent, HookPlan, HookReport, HookShim, ImportedRun, KeyLookup, ReadySignal,
    RecoveredRun, Refusal, ResolvedRows, ScreenRules, SpawnCheck, SpawnContext, TracerRun,
    workspace_prelude,
};
use options::{ClaudeProvider, PermissionMode};
use protocol::{
    AccentToken, AgentCaps, AgentDescriptor, AgentId, AgentTexts, ChipRule, Choice, ChoiceStyle,
    FieldKind, FieldOverride, FieldRule, OptionField, OptionValue, SessionMember, SessionStatus,
    ShiftEnter, SpawnPlacement,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::LazyLock;
use tracing::debug;

/// The instruction files the workspace prelude names.
const INSTRUCTION_FILES: &[&str] = &["CLAUDE.md", "AGENTS.md"];

/// The model a `DeepSeek` spawn passes as `--model` when it names none.
pub const DEEPSEEK_MODEL: &str = "deepseek-flash";

/// The options a Claude spawn takes, one per field of Claude's descriptor.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeOptions {
    /// Claude's `--permission-mode <value>`; `None` passes none, leaving the
    /// CLI's default. Ignored (and the flag omitted) on a trusted spawn.
    pub permission_mode: Option<PermissionMode>,
    /// The endpoint the CLI talks to.
    pub provider: ClaudeProvider,
    /// Claude's `--model <value>`; `None` passes the provider's default model
    /// (see [`default_model`]), or no flag when it has none.
    pub model: Option<String>,
}

pub struct ClaudePlugin;

/// The Claude plugin the registry hands out.
pub static CLAUDE: ClaudePlugin = ClaudePlugin;

/// Claude's descriptor, built once.
static DESCRIPTOR: LazyLock<AgentDescriptor> = LazyLock::new(claude_descriptor);

fn claude_descriptor() -> AgentDescriptor {
    AgentDescriptor {
        id: AgentId("claude".to_string()),
        label: "Claude".to_string(),
        tag: "claude".to_string(),
        accent: AccentToken::Claude,
        shift_enter: ShiftEnter::BackslashCr,
        caps: AgentCaps {
            headless: true,
            resume: true,
            extra_folders: true,
            shell_resume: true,
        },
        trust_note: Some("Trusted launch passes --dangerously-skip-permissions.".to_string()),
        texts: AgentTexts {
            shell_resume_label: "Shell running claude --resume".to_string(),
            no_conversation: "no conversation found".to_string(),
        },
        fields: vec![permission_mode_field(), provider_field(), model_field()],
        chips: vec![ChipRule {
            field: "provider".to_string(),
            hide_when: OptionValue::Text("anthropic".to_string()),
            tip: "Runs on DeepSeek".to_string(),
        }],
    }
}

/// A field shown on Claude's Settings page and placed in the spawn dialog's
/// Main section, with no help text and no rules; a field placed elsewhere
/// sets its own `spawn`.
fn main_field(id: &str, label: &str, kind: FieldKind, default: OptionValue) -> OptionField {
    OptionField {
        id: id.to_string(),
        label: label.to_string(),
        help: None,
        kind,
        default,
        settings: true,
        spawn: SpawnPlacement::Main,
        locked_when_trusted: false,
        when: Vec::new(),
    }
}

fn choices(pairs: &[(&str, &str)]) -> Vec<Choice> {
    pairs
        .iter()
        .map(|(value, label)| Choice {
            value: (*value).to_string(),
            label: (*label).to_string(),
        })
        .collect()
}

/// The permission mode, in the spawn dialog's Advanced section: no value is
/// the CLI's default; each choice is a [`PermissionMode`]'s serde name.
fn permission_mode_field() -> OptionField {
    OptionField {
        spawn: SpawnPlacement::Advanced,
        locked_when_trusted: true,
        ..main_field(
            "permission_mode",
            "Permission mode",
            FieldKind::Choice {
                choices: choices(&[
                    ("default", "Default"),
                    ("accept_edits", "Accept edits"),
                    ("bypass_permissions", "Bypass permissions"),
                    ("plan", "Plan"),
                ]),
                style: ChoiceStyle::Dropdown,
            },
            OptionValue::Null,
        )
    }
}

fn provider_field() -> OptionField {
    main_field(
        "provider",
        "Provider",
        FieldKind::Choice {
            choices: choices(&[("anthropic", "Anthropic"), ("deepseek", "DeepSeek")]),
            style: ChoiceStyle::Segmented,
        },
        OptionValue::Text("anthropic".to_string()),
    )
}

/// The model, chosen per spawn in the dialog's Advanced section and not on
/// the Settings page: no value passes the provider's default; on `DeepSeek`
/// the placeholder names its default model and the Anthropic aliases go.
fn model_field() -> OptionField {
    OptionField {
        settings: false,
        spawn: SpawnPlacement::Advanced,
        when: vec![FieldRule {
            if_field: "provider".to_string(),
            equals: OptionValue::Text("deepseek".to_string()),
            then: FieldOverride {
                hidden: None,
                placeholder: Some(DEEPSEEK_MODEL.to_string()),
                suggestions: Some(Vec::new()),
            },
        }],
        ..main_field(
            "model",
            "Model",
            FieldKind::Model {
                placeholder: None,
                suggestions: ["opus", "sonnet", "haiku"].map(str::to_string).to_vec(),
            },
            OptionValue::Null,
        )
    }
}

impl Agent for ClaudePlugin {
    type Options = ClaudeOptions;

    fn descriptor(&self) -> &AgentDescriptor {
        &DESCRIPTOR
    }

    fn program_env_var(&self) -> &'static str {
        "RUSTLING_TULIP_CLAUDE"
    }

    fn default_program(&self) -> &'static str {
        "claude"
    }

    /// `fake/`'s shim, with Claude Code's home under `CLAUDE_CONFIG_DIR` and
    /// `claude` shadowed on `PATH`.
    #[cfg(feature = "test-support")]
    fn fake_cli(&self) -> Option<agent_api::FakeCli> {
        let shim = if cfg!(windows) {
            "fake-claude.cmd"
        } else {
            "fake-claude.sh"
        };
        Some(agent_api::FakeCli {
            shim: std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("fake")
                .join(shim),
            home_env: Some("CLAUDE_CONFIG_DIR"),
            path_stub: Some("claude"),
        })
    }

    fn passthrough_vars(&self) -> &'static [&'static str] {
        &["ANTHROPIC_API_KEY", "CLAUDE_CONFIG_DIR"]
    }

    /// `claude`, and `node`, which an npm-installed `claude` runs as.
    fn liveness_tokens(&self) -> &'static [&'static str] {
        &["claude", "node"]
    }

    /// The recorded options, else the default ones. `run.trusted` is not
    /// read: a trusted recovery carries `--dangerously-skip-permissions` on
    /// its request, and the recorded permission mode stays as it was.
    fn recovery_options(&self, run: &RecoveredRun<'_>) -> ClaudeOptions {
        run.recorded_as().unwrap_or_default()
    }

    /// Revision 3 keeps the run's options and trust on the history entry
    /// itself, so older imports are read again from their tracer logs.
    fn import_rev(&self) -> u32 {
        import::IMPORT_REV
    }

    fn import_tracer_run(&self, run: &TracerRun<'_>) -> Option<ImportedRun<ClaudeOptions>> {
        import::import_tracer_run(run)
    }

    /// Every flag the interactive and headless argv carry, plus the other
    /// ways of picking a conversation, which would break the session's
    /// conversation-id tracking.
    fn reserved_args(&self) -> &'static [&'static str] {
        &[
            "--settings",
            "--resume",
            "--session-id",
            "--model",
            "--permission-mode",
            "--dangerously-skip-permissions",
            "--add-dir",
            "--append-system-prompt",
            "-p",
            "--print",
            "--output-format",
            "--verbose",
            "-r",
            "--continue",
            "-c",
            "--fork-session",
        ]
    }

    fn check_request(&self, opts: &ClaudeOptions, req: &SpawnCheck<'_>) -> Result<(), Refusal> {
        providers::check_request(opts.provider, req)
    }

    fn spawn_env(
        &self,
        opts: &ClaudeOptions,
        rows: &ResolvedRows,
        keys: &dyn KeyLookup,
    ) -> Result<EnvPlan, Refusal> {
        providers::spawn_env(opts.provider, rows, keys)
    }

    /// A fresh session starts under an id Claude is handed with
    /// `--session-id`, so it can be resumed after it ends; a resumed one keeps
    /// its own.
    fn conversation_plan(&self, _opts: &ClaudeOptions, resume: Option<&str>) -> ConversationPlan {
        match resume {
            Some(id) => ConversationPlan::Resumed(id.to_string()),
            None => ConversationPlan::Minted(uuid::Uuid::new_v4().to_string()),
        }
    }

    fn find_conversations(&self, query: &ConversationQuery<'_>) -> Vec<FoundConversation> {
        transcripts::find_conversations(query)
    }

    fn interactive_args(
        &self,
        opts: &ClaudeOptions,
        common: &SpawnContext<'_>,
        members: &[SessionMember],
        initial_prompt: Option<&str>,
    ) -> Vec<String> {
        let mut args: Vec<String> = Vec::new();
        for extra in members.iter().skip(1) {
            args.push("--add-dir".to_string());
            args.push(extra.worktree_path.clone());
        }
        for dir in common.add_dirs {
            args.push("--add-dir".to_string());
            args.push(dir.clone());
        }
        // Workspace-context prelude rides on --append-system-prompt so it's
        // invisible to the user but the model sees it. Only emitted for 2+
        // member sessions (where the worktree-vs-original-path mismatch
        // actually matters).
        if let Some(prelude) = workspace_prelude(members, INSTRUCTION_FILES) {
            args.push("--append-system-prompt".to_string());
            args.push(prelude);
        }
        extend_common(&mut args, common, opts);
        // The injected hooks ride on the plan's args (`--settings <path>` for
        // Claude), ahead of the resume branch so a resumed conversation carries
        // them too, and ahead of `--session-id` so both interactive forms keep
        // them in one position.
        args.extend(common.hook_args.iter().cloned());
        // A resumed conversation keeps its own id and history, so it takes
        // neither a fresh `--session-id` nor a kickoff prompt.
        if let Some(id) = common.resume_conversation {
            args.push("--resume".to_string());
            args.push(id.to_string());
            return args;
        }
        if let Some(id) = common.conversation_id {
            args.push("--session-id".to_string());
            args.push(id.to_string());
        }
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

    fn headless_args(
        &self,
        opts: &ClaudeOptions,
        common: &SpawnContext<'_>,
        members: &[SessionMember],
        prompt: &str,
    ) -> Option<Vec<String>> {
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
        if let Some(prelude) = workspace_prelude(members, INSTRUCTION_FILES) {
            args.push("--append-system-prompt".to_string());
            args.push(prelude);
        }
        extend_common(&mut args, common, opts);
        args.push("-p".to_string());
        args.push(prompt.to_string());
        Some(args)
    }

    fn parse_headless_line(&self, line: &str) -> Vec<HeadlessEvent> {
        match serde_json::from_str::<StreamEvent>(line) {
            Ok(event) => headless_events(event),
            Err(err) => {
                debug!(?err, line, "unrecognized stream-json event");
                Vec::new()
            }
        }
    }

    fn hook_plan(&self, _opts: &ClaudeOptions, _trusted: bool, shim: &HookShim<'_>) -> HookPlan {
        hooks::hook_plan(shim)
    }

    fn map_hook_event(
        &self,
        body: &serde_json::Value,
        spawn: Option<&ClaudeOptions>,
        trusted: bool,
    ) -> Option<HookReport> {
        hooks::map_hook_event(body, spawn, trusted)
    }

    /// `claude --settings "<path>" --resume <id>`, naming the hook settings
    /// file the shell's spawn wrote. The path is always double-quoted, since
    /// the config dir can hold spaces; a path holding a double quote cannot be
    /// quoted for cmd and POSIX shells at once, so it, like a spawn that wrote
    /// no file, resumes without the settings.
    ///
    /// A shell would type `claude` without a provider's routing, running on
    /// Anthropic, so a session on another provider is refused, naming the
    /// provider when this daemon knows it.
    fn shell_resume(
        &self,
        opts: &ClaudeOptions,
        conversation_id: &str,
        hook_files: &[PathBuf],
    ) -> Result<Option<String>, AgentError> {
        if !opts.provider.is_anthropic() {
            return Err(AgentError::new(match display_name(opts.provider) {
                Some(name) => format!("{name} sessions recover as Claude."),
                None => "Sessions on this Claude provider recover as Claude.".to_owned(),
            }));
        }
        let settings = hook_files
            .first()
            .map(|path| path.to_string_lossy())
            .filter(|path| !path.contains('"'));
        Ok(Some(match settings {
            Some(path) => format!("claude --settings \"{path}\" --resume {conversation_id}"),
            None => format!("claude --resume {conversation_id}"),
        }))
    }

    /// Claude's rule table, which the daemon reads a session's rendered screen
    /// with.
    fn screen_rules(&self) -> &ScreenRules {
        rules::screen_rules()
    }

    /// The banner tuning the daemon's startup heuristic was measured against.
    fn ready_signal(&self) -> ReadySignal {
        rules::READY_SIGNAL
    }

    /// Claude reads a file a prompt names with `@` in front of its path.
    fn file_reference(&self, repo_relative: &str) -> String {
        rules::file_reference(repo_relative)
    }
}

/// The session changes one parsed stream-json event makes, in order.
fn headless_events(event: StreamEvent) -> Vec<HeadlessEvent> {
    match event {
        StreamEvent::System { subtype } => vec![
            HeadlessEvent::Status(SessionStatus::Working),
            HeadlessEvent::RecentAction(format!("system: {subtype}")),
        ],
        StreamEvent::Assistant { message } => message
            .and_then(|m| m.content)
            .into_iter()
            .flatten()
            .filter_map(block_action)
            .map(HeadlessEvent::RecentAction)
            .collect(),
        StreamEvent::Result {
            total_cost_usd,
            usage,
            subtype,
        } => vec![
            HeadlessEvent::Metrics {
                cost_usd: total_cost_usd,
                input_tokens: usage.as_ref().map(|u| u.input_tokens.unwrap_or(0)),
                output_tokens: usage.as_ref().map(|u| u.output_tokens.unwrap_or(0)),
            },
            HeadlessEvent::Status(SessionStatus::Stopped),
            HeadlessEvent::RecentAction(format!("result: {subtype}")),
        ],
        StreamEvent::User {} | StreamEvent::Other => Vec::new(),
    }
}

/// The recent action one assistant content block records, if any: a tool
/// call's name, or the first 120 characters of non-blank text.
fn block_action(block: ContentBlock) -> Option<String> {
    match block {
        ContentBlock::ToolUse { name } => Some(format!("tool: {name}")),
        ContentBlock::Text { text } => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return None;
            }
            let snippet = trimmed.chars().take(120).collect::<String>();
            Some(format!("assistant: {snippet}"))
        }
        ContentBlock::Other => None,
    }
}

/// The model a spawn on `provider` passes as `--model` when it names none.
#[must_use]
pub fn default_model(provider: ClaudeProvider) -> Option<&'static str> {
    match provider {
        ClaudeProvider::Deepseek => Some(DEEPSEEK_MODEL),
        ClaudeProvider::Anthropic | ClaudeProvider::Unknown => None,
    }
}

/// Append claude's `--model`, `--permission-mode`, and
/// `--dangerously-skip-permissions` args. A spawn that names no model on a
/// provider with a default model (see [`default_model`]) passes
/// that one. The claude CLI rejects `--permission-mode` together with
/// `--dangerously-skip-permissions`, so the latter wins when both are set.
fn extend_common(args: &mut Vec<String>, common: &SpawnContext<'_>, opts: &ClaudeOptions) {
    if let Some(model) = opts
        .model
        .as_deref()
        .or_else(|| default_model(opts.provider))
    {
        args.push("--model".to_string());
        args.push(model.to_string());
    }
    if common.dangerously_skip_permissions {
        args.push("--dangerously-skip-permissions".to_string());
    } else if let Some(mode) = opts.permission_mode {
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
    use protocol::OptionValues;

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

    fn options(permission_mode: Option<PermissionMode>, model: Option<&str>) -> ClaudeOptions {
        ClaudeOptions {
            permission_mode,
            provider: ClaudeProvider::Anthropic,
            model: model.map(str::to_string),
        }
    }

    fn common(skip: bool, has_injector: bool) -> SpawnContext<'static> {
        SpawnContext {
            dangerously_skip_permissions: skip,
            has_prompt_injector: has_injector,
            conversation_id: None,
            resume_conversation: None,
            add_dirs: &[],
            hook_args: &[],
        }
    }

    fn interactive(
        opts: &ClaudeOptions,
        common: &SpawnContext<'_>,
        members: &[SessionMember],
        prompt: Option<&str>,
    ) -> Vec<String> {
        ClaudePlugin.interactive_args(opts, common, members, prompt)
    }

    fn headless(
        opts: &ClaudeOptions,
        common: &SpawnContext<'_>,
        members: &[SessionMember],
        prompt: &str,
    ) -> Vec<String> {
        ClaudePlugin
            .headless_args(opts, common, members, prompt)
            .expect("Claude has a headless mode")
    }

    const RESUME_ID: &str = "85573bb1-c581-489e-baaa-94d5a384744c";

    #[test]
    fn conformance() {
        agent_api::conformance::check(&ClaudePlugin);
    }

    #[test]
    fn the_settings_page_holds_permission_mode_and_provider_not_model() {
        let settings: Vec<&str> = ClaudePlugin
            .descriptor()
            .fields
            .iter()
            .filter(|field| field.settings)
            .map(|field| field.id.as_str())
            .collect();
        assert_eq!(
            settings,
            ["permission_mode", "provider"],
            "the model is chosen per spawn, not on the Settings page"
        );
    }

    fn shell_resume(hook_files: &[std::path::PathBuf]) -> Option<String> {
        ClaudePlugin
            .shell_resume(&ClaudeOptions::default(), "abc-123", hook_files)
            .expect("Claude resumes in a shell")
    }

    #[test]
    fn shell_resume_quotes_a_settings_path_with_spaces() {
        let path =
            std::path::PathBuf::from(r"C:\Users\Some Name\config\sessions\s1\hook-settings.json");
        assert_eq!(
            shell_resume(&[path]).as_deref(),
            Some(
                r#"claude --settings "C:\Users\Some Name\config\sessions\s1\hook-settings.json" --resume abc-123"#
            )
        );
    }

    #[test]
    fn shell_resume_without_hook_files_is_a_bare_resume() {
        assert_eq!(
            shell_resume(&[]).as_deref(),
            Some("claude --resume abc-123")
        );
    }

    #[test]
    fn shell_resume_drops_the_settings_for_an_unquotable_path() {
        let path = std::path::PathBuf::from("C:\\odd\"name\\hook-settings.json");
        assert_eq!(
            shell_resume(&[path]).as_deref(),
            Some("claude --resume abc-123")
        );
    }

    fn recovered(recorded: Option<&OptionValues>, trusted: bool) -> ClaudeOptions {
        ClaudePlugin.recovery_options(&RecoveredRun { recorded, trusted })
    }

    fn claude_values(provider: &str, mode: OptionValue, model: OptionValue) -> OptionValues {
        OptionValues::from([
            (
                "provider".to_string(),
                OptionValue::Text(provider.to_string()),
            ),
            ("permission_mode".to_string(), mode),
            ("model".to_string(), model),
        ])
    }

    #[test]
    fn recovery_replays_recorded_provider_permission_and_model() {
        let recorded = claude_values(
            "deepseek",
            OptionValue::Text("plan".to_string()),
            OptionValue::Text("deepseek-pro".to_string()),
        );
        assert_eq!(
            recovered(Some(&recorded), false),
            ClaudeOptions {
                permission_mode: Some(PermissionMode::Plan),
                provider: ClaudeProvider::Deepseek,
                model: Some("deepseek-pro".to_string()),
            }
        );
    }

    #[test]
    fn recovery_without_a_record_is_the_default_options() {
        assert_eq!(recovered(None, false), ClaudeOptions::default());
        assert_eq!(recovered(None, true), ClaudeOptions::default());
    }

    #[test]
    fn recovery_trusted_leaves_the_recorded_permission_mode() {
        let recorded = claude_values(
            "anthropic",
            OptionValue::Text("accept_edits".to_string()),
            OptionValue::Null,
        );
        let trusted = recovered(Some(&recorded), true);
        assert_eq!(trusted, recovered(Some(&recorded), false));
        assert_eq!(trusted.permission_mode, Some(PermissionMode::AcceptEdits));
        let unset = claude_values("anthropic", OptionValue::Null, OptionValue::Null);
        assert_eq!(recovered(Some(&unset), true).permission_mode, None);
    }

    fn on_provider(provider: ClaudeProvider) -> ClaudeOptions {
        ClaudeOptions {
            provider,
            ..ClaudeOptions::default()
        }
    }

    #[test]
    fn shell_resume_refuses_deepseek() {
        let err = ClaudePlugin
            .shell_resume(&on_provider(ClaudeProvider::Deepseek), "abc-123", &[])
            .expect_err("a DeepSeek entry does not resume in a shell");
        assert_eq!(err.to_string(), "DeepSeek sessions recover as Claude.");
    }

    #[test]
    fn shell_resume_refuses_an_unknown_provider() {
        let err = ClaudePlugin
            .shell_resume(&on_provider(ClaudeProvider::Unknown), "abc-123", &[])
            .expect_err("an unknown provider does not resume in a shell");
        assert_eq!(
            err.to_string(),
            "Sessions on this Claude provider recover as Claude."
        );
    }

    #[test]
    fn shell_resume_refusal_does_not_depend_on_the_id() {
        let files = [std::path::PathBuf::from(HOOK_SETTINGS)];
        for provider in [ClaudeProvider::Deepseek, ClaudeProvider::Unknown] {
            let opts = on_provider(provider);
            let refusals: Vec<Result<Option<String>, AgentError>> = ["abc-123", "", RESUME_ID]
                .iter()
                .flat_map(|id| {
                    [
                        ClaudePlugin.shell_resume(&opts, id, &[]),
                        ClaudePlugin.shell_resume(&opts, id, &files),
                    ]
                })
                .collect();
            assert!(refusals[0].is_err(), "{provider:?}");
            for refusal in &refusals {
                assert_eq!(refusal, &refusals[0], "{provider:?}");
            }
        }
    }

    #[test]
    fn claude_reserved_args_list() {
        assert_eq!(
            Agent::reserved_args(&ClaudePlugin),
            [
                "--settings",
                "--resume",
                "--session-id",
                "--model",
                "--permission-mode",
                "--dangerously-skip-permissions",
                "--add-dir",
                "--append-system-prompt",
                "-p",
                "--print",
                "--output-format",
                "--verbose",
                "-r",
                "--continue",
                "-c",
                "--fork-session",
            ]
        );
    }

    /// Every flag-shaped token in `args`.
    fn flags(args: &[String]) -> Vec<String> {
        args.iter()
            .filter(|arg| arg.starts_with('-'))
            .cloned()
            .collect()
    }

    #[test]
    fn every_flag_claude_emits_is_reserved() {
        let m = members(&["X:/dev/a", "X:/dev/b"]);
        let extra = vec!["X:/dev/c".to_string()];
        let shim = HookShim {
            tracer_exe: std::path::Path::new("X:/rt/rt-tracer.exe"),
            session_id: SESSION_ID,
            config_dir: std::path::Path::new("X:/rt"),
        };
        let hook_args =
            Agent::hook_plan(&ClaudePlugin, &ClaudeOptions::default(), false, &shim).args;
        let mut emitted = Vec::new();
        for skip in [false, true] {
            let opts = options(Some(PermissionMode::Plan), Some("opus"));
            let fresh = SpawnContext {
                add_dirs: &extra,
                hook_args: &hook_args,
                ..with_session_id(skip, false)
            };
            emitted.extend(flags(&interactive(&opts, &fresh, &m, Some("hello"))));
            let resumed = SpawnContext {
                resume_conversation: Some(RESUME_ID),
                ..fresh
            };
            emitted.extend(flags(&interactive(&opts, &resumed, &m, None)));
            emitted.extend(flags(&headless(&opts, &fresh, &m, "hello")));
        }
        let reserved = Agent::reserved_args(&ClaudePlugin);
        for flag in [
            "--settings",
            "--resume",
            "--session-id",
            "--model",
            "--permission-mode",
            "--dangerously-skip-permissions",
            "--add-dir",
            "--append-system-prompt",
            "-p",
            "--print",
            "--output-format",
            "--verbose",
        ] {
            assert!(
                emitted.iter().any(|f| f == flag),
                "{flag} is emitted by some spawn: {emitted:?}"
            );
        }
        for flag in &emitted {
            assert!(
                reserved.contains(&flag.as_str()),
                "{flag} is emitted but not reserved"
            );
        }
    }

    #[test]
    fn resume_replaces_session_id_and_drops_the_prompt() {
        let m = members(&["X:/dev/a", "X:/dev/b"]);
        let opts = options(None, None);
        let common = SpawnContext {
            resume_conversation: Some(RESUME_ID),
            ..with_session_id(true, false)
        };
        let args = interactive(&opts, &common, &m, Some("hello"));
        assert!(!args.contains(&"--session-id".to_string()), "{args:?}");
        assert!(!args.contains(&SESSION_ID.to_string()), "{args:?}");
        assert!(!args.contains(&"-p".to_string()), "{args:?}");
        assert!(!args.contains(&"hello".to_string()), "{args:?}");
        let tail: Vec<&str> = args.iter().map(String::as_str).skip(2).collect();
        assert_eq!(args[..2], ["--add-dir", "X:/dev/b"]);
        assert_eq!(tail[0], "--append-system-prompt");
        assert_eq!(
            tail[2..],
            ["--dangerously-skip-permissions", "--resume", RESUME_ID]
        );
    }

    #[test]
    fn standalone_add_dirs_follow_member_add_dirs() {
        let m = members(&["X:/dev/a"]);
        let opts = options(None, None);
        let extra = vec!["X:/dev/b".to_string(), "X:/dev/c".to_string()];
        let common = SpawnContext {
            add_dirs: &extra,
            resume_conversation: Some(RESUME_ID),
            ..common(false, false)
        };
        let args = interactive(&opts, &common, &m, None);
        assert_eq!(
            args,
            vec![
                "--add-dir",
                "X:/dev/b",
                "--add-dir",
                "X:/dev/c",
                "--resume",
                RESUME_ID
            ]
        );
    }

    const SESSION_ID: &str = "0b6f4c7e-1111-4222-8333-944455556666";

    fn with_session_id(skip: bool, has_injector: bool) -> SpawnContext<'static> {
        SpawnContext {
            conversation_id: Some(SESSION_ID),
            ..common(skip, has_injector)
        }
    }

    #[test]
    fn interactive_args_include_session_id() {
        let m = members(&["X:/dev/a"]);
        let opts = options(None, Some("opus"));
        let args = interactive(&opts, &with_session_id(false, false), &m, None);
        assert_eq!(args, vec!["--model", "opus", "--session-id", SESSION_ID]);
    }

    #[test]
    fn interactive_args_with_prompt_include_session_id() {
        let m = members(&["X:/dev/a"]);
        let opts = options(None, None);
        let args = interactive(&opts, &with_session_id(false, false), &m, Some("hello"));
        assert_eq!(args, vec!["--session-id", SESSION_ID, "-p", "hello"]);

        // With an injector the prompt stays off the argv; the id does not.
        let injected = interactive(&opts, &with_session_id(false, true), &m, Some("hello"));
        assert_eq!(injected, vec!["--session-id", SESSION_ID]);
    }

    const HOOK_SETTINGS: &str =
        "X:/rt/sessions/0b6f4c7e-1111-4222-8333-944455556666/hook-settings.json";

    #[test]
    fn interactive_args_pass_the_hook_settings_file() {
        let m = members(&["X:/dev/a"]);
        let opts = options(None, Some("opus"));
        let hook_args = ["--settings".to_string(), HOOK_SETTINGS.to_string()];
        let common = SpawnContext {
            hook_args: &hook_args,
            ..with_session_id(false, false)
        };
        let args = interactive(&opts, &common, &m, None);
        assert_eq!(
            args,
            vec![
                "--model",
                "opus",
                "--settings",
                HOOK_SETTINGS,
                "--session-id",
                SESSION_ID,
            ]
        );
    }

    #[test]
    fn resume_args_pass_the_hook_settings_file() {
        let m = members(&["X:/dev/a"]);
        let opts = options(None, None);
        let hook_args = ["--settings".to_string(), HOOK_SETTINGS.to_string()];
        let common = SpawnContext {
            hook_args: &hook_args,
            resume_conversation: Some(RESUME_ID),
            ..with_session_id(true, false)
        };
        let args = interactive(&opts, &common, &m, Some("hello"));
        assert_eq!(
            args,
            vec![
                "--dangerously-skip-permissions",
                "--settings",
                HOOK_SETTINGS,
                "--resume",
                RESUME_ID,
            ]
        );
    }

    #[test]
    fn headless_args_take_no_settings() {
        let m = members(&["X:/dev/a"]);
        let opts = options(None, None);
        let hook_args = ["--settings".to_string(), HOOK_SETTINGS.to_string()];
        let common = SpawnContext {
            hook_args: &hook_args,
            ..common(false, false)
        };
        let args = headless(&opts, &common, &m, "hello");
        assert!(!args.contains(&"--settings".to_string()), "{args:?}");
        assert!(!args.contains(&HOOK_SETTINGS.to_string()), "{args:?}");
    }

    #[test]
    fn headless_args_have_no_session_id() {
        let m = members(&["X:/dev/a"]);
        let opts = options(None, None);
        let args = headless(&opts, &with_session_id(false, false), &m, "hello");
        assert!(!args.contains(&"--session-id".to_string()), "{args:?}");
        assert!(!args.contains(&SESSION_ID.to_string()), "{args:?}");
    }

    #[test]
    fn deepseek_spawn_passes_model_deepseek_flash() {
        let m = members(&["X:/dev/a"]);
        let opts = ClaudeOptions {
            provider: ClaudeProvider::Deepseek,
            ..options(None, None)
        };
        let interactive_args = interactive(&opts, &common(false, false), &m, None);
        assert!(
            interactive_args
                .windows(2)
                .any(|w| *w == ["--model", "deepseek-flash"]),
            "{interactive_args:?}"
        );
        let headless_args = headless(&opts, &common(false, false), &m, "hi");
        assert!(
            headless_args
                .windows(2)
                .any(|w| *w == ["--model", "deepseek-flash"]),
            "{headless_args:?}"
        );
        let chosen_opts = ClaudeOptions {
            model: Some("deepseek-pro".to_string()),
            ..opts
        };
        let chosen = interactive(&chosen_opts, &common(false, false), &m, None);
        assert_eq!(chosen, vec!["--model", "deepseek-pro"]);
        let anthropic = interactive(&options(None, None), &common(false, false), &m, None);
        assert!(!anthropic.contains(&"--model".to_string()), "{anthropic:?}");
    }

    #[test]
    fn interactive_single_repo_with_model() {
        let m = members(&["X:/dev/a"]);
        let opts = options(Some(PermissionMode::AcceptEdits), Some("opus"));
        let args = interactive(&opts, &common(false, false), &m, Some("hello"));
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
        let opts = options(Some(PermissionMode::Plan), None);
        let args = interactive(&opts, &common(true, false), &m, Some("hi"));
        assert!(args.contains(&"--dangerously-skip-permissions".to_string()));
        assert!(!args.contains(&"--permission-mode".to_string()));
    }

    #[test]
    fn interactive_workspace_emits_add_dir_and_prelude() {
        let m = members(&["X:/dev/a", "X:/dev/b"]);
        let opts = options(None, None);
        let args = interactive(&opts, &common(false, false), &m, None);
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
        let opts = options(None, None);
        let args = interactive(&opts, &common(false, true), &m, Some("would-be-prompt"));
        assert!(!args.contains(&"-p".to_string()));
        assert!(!args.iter().any(|a| a == "would-be-prompt"));
    }

    #[test]
    fn headless_args_include_print_and_stream_json() {
        let m = members(&["X:/dev/a"]);
        let opts = options(None, Some("sonnet"));
        let args = headless(&opts, &common(false, false), &m, "hello world");
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
        let opts = options(None, None);
        let args = headless(&opts, &common(false, false), &m, "go");
        assert!(args.windows(2).any(|w| w == ["--add-dir", "X:/dev/b"]));
        assert!(args.iter().any(|a| a == "--append-system-prompt"));
    }

    #[test]
    fn default_model_names_deepseek_flash_for_deepseek_only() {
        assert_eq!(
            default_model(ClaudeProvider::Deepseek),
            Some("deepseek-flash")
        );
        assert_eq!(default_model(ClaudeProvider::Anthropic), None);
        assert_eq!(default_model(ClaudeProvider::Unknown), None);
    }

    mod headless {
        use super::super::*;

        fn parse(line: &str) -> Vec<HeadlessEvent> {
            ClaudePlugin.parse_headless_line(line)
        }

        #[test]
        fn system_line_sets_working_and_records_the_subtype() {
            assert_eq!(
                parse(r#"{"type":"system","subtype":"init","session_id":"s1"}"#),
                vec![
                    HeadlessEvent::Status(SessionStatus::Working),
                    HeadlessEvent::RecentAction("system: init".to_string()),
                ]
            );
        }

        #[test]
        fn assistant_line_records_each_block_in_order() {
            let long = format!("{}{}", "a".repeat(120), "b".repeat(10));
            let line = serde_json::json!({
                "type": "assistant",
                "message": {"content": [
                    {"type": "text", "text": format!("  {long}  ")},
                    {"type": "text", "text": "   "},
                    {"type": "tool_use", "id": "t1", "name": "Read", "input": {}},
                    {"type": "thinking", "thinking": "hmm"},
                ]},
            })
            .to_string();
            assert_eq!(
                parse(&line),
                vec![
                    HeadlessEvent::RecentAction(format!("assistant: {}", "a".repeat(120))),
                    HeadlessEvent::RecentAction("tool: Read".to_string()),
                ]
            );
        }

        #[test]
        fn result_line_with_usage_reports_cost_and_tokens() {
            let line = r#"{"type":"result","subtype":"success","total_cost_usd":0.25,
                "usage":{"input_tokens":1200,"output_tokens":340}}"#;
            assert_eq!(
                parse(line),
                vec![
                    HeadlessEvent::Metrics {
                        cost_usd: Some(0.25),
                        input_tokens: Some(1200),
                        output_tokens: Some(340),
                    },
                    HeadlessEvent::Status(SessionStatus::Stopped),
                    HeadlessEvent::RecentAction("result: success".to_string()),
                ]
            );
        }

        #[test]
        fn result_line_without_usage_reports_no_tokens() {
            assert_eq!(
                parse(r#"{"type":"result","subtype":"error_max_turns","total_cost_usd":0.5}"#),
                vec![
                    HeadlessEvent::Metrics {
                        cost_usd: Some(0.5),
                        input_tokens: None,
                        output_tokens: None,
                    },
                    HeadlessEvent::Status(SessionStatus::Stopped),
                    HeadlessEvent::RecentAction("result: error_max_turns".to_string()),
                ]
            );
        }

        #[test]
        fn user_line_yields_nothing() {
            assert_eq!(
                parse(r#"{"type":"user","message":{"content":[]}}"#),
                Vec::new()
            );
        }

        #[test]
        fn garbage_yields_nothing() {
            assert_eq!(parse("not json at all"), Vec::new());
        }
    }
}
