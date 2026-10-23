//! The agent plugin seam.
//!
//! Each agent CLI the daemon knows how to spawn is an [`Agent`]: it describes
//! itself and its option schema with an [`AgentDescriptor`], builds the CLI's
//! argv from its typed options and a [`SpawnContext`], names the CLI's
//! executable, writes the hook settings file its interactive spawns pass, and
//! turns the CLI's headless output into [`HeadlessEvent`]s the daemon applies
//! to the session.
//!
//! The registry holds every agent as the object-safe [`AgentPlugin`], which
//! this crate implements for every [`Agent`]: each call takes the spawn's
//! [`OptionValues`] and decodes them into the agent's typed options.
//! [`validate`] checks a set of values against an agent's schema, and
//! [`workspace_prelude`] is the workspace note a plugin hands its CLI for a
//! multi-member session.

#[cfg(feature = "test-support")]
pub mod conformance;

use protocol::{AgentDescriptor, FieldKind, OptionField, OptionValue, OptionValues};
use protocol::{SessionMember, SessionStatus};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::fmt;
use std::path::Path;

/// Cross-agent spawn context bundled together so trait signatures stay
/// readable. Fields are borrowed from the daemon's in-flight spawn so the
/// caller doesn't have to clone anything to call a plugin method.
pub struct SpawnContext<'a> {
    pub dangerously_skip_permissions: bool,
    /// `true` when the spawn has a prompt injector attached, in which case
    /// the plugin must NOT emit the initial prompt as a CLI arg — the
    /// injector delivers it through the PTY post-spawn instead. Ignored by
    /// headless paths.
    pub has_prompt_injector: bool,
    /// Conversation id the plugin passes its CLI on an interactive spawn
    /// (Claude's `--session-id`). Ignored by headless paths.
    pub conversation_id: Option<&'a str>,
    /// Conversation the CLI resumes (Claude's `--resume`), in place of the
    /// conversation id and any initial prompt.
    pub resume_conversation: Option<&'a str>,
    /// Directories a standalone target carries as extra folders. The plugin
    /// adds them after the extra members' worktrees; headless spawns ignore
    /// them.
    pub add_dirs: &'a [String],
    /// The settings file the plugin passes its CLI on an interactive spawn,
    /// fresh or resumed: the file the daemon wrote for the session from
    /// [`Agent::hook_settings_file`]. `None` for a spawn whose file could not
    /// be written, which stays on the terminal heuristic. Headless spawns
    /// never carry it.
    pub settings_file: Option<&'a Path>,
}

/// One change a plugin read from a line of its CLI's headless output, for the
/// daemon to apply to the session's record in order.
#[derive(Debug, Clone, PartialEq)]
pub enum HeadlessEvent {
    /// The session's new status.
    Status(SessionStatus),
    /// A line for the session's recent-actions list.
    RecentAction(String),
    /// Usage figures; each `Some` replaces the session's current figure and
    /// each `None` leaves it as it is.
    Metrics {
        cost_usd: Option<f64>,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
    },
}

/// Implemented once per supported CLI, over the agent's own typed options.
/// The daemon never calls it directly: it reaches every agent through the
/// [`AgentPlugin`] this crate implements for it, so the daemon doesn't need
/// to know which agent it is talking to.
pub trait Agent: Send + Sync + 'static {
    /// The agent's options, decoded from the spawn's [`OptionValues`]. Its
    /// serialized form must match the descriptor's fields: each field's id is
    /// a key, and every value the schema accepts must decode (the
    /// `conformance` check under the `test-support` feature proves it).
    type Options: DeserializeOwned + Serialize + Default;

    /// The agent's id, names, look, capabilities and option schema.
    fn descriptor(&self) -> &AgentDescriptor;

    /// Environment-variable name that overrides the default executable name.
    /// Example: `"RUSTLING_TULIP_CLAUDE"`.
    fn program_env_var(&self) -> &'static str;

    /// Fallback executable name when [`Self::program_env_var`] is unset.
    /// This is the name looked up on `PATH`.
    fn default_program(&self) -> &'static str;

    /// The argv passed to the CLI for an interactive (PTY-attached) session.
    /// Does not include the executable name itself — that is resolved by
    /// [`AgentPlugin::resolve_program`].
    fn interactive_args(
        &self,
        opts: &Self::Options,
        ctx: &SpawnContext<'_>,
        members: &[SessionMember],
        initial_prompt: Option<&str>,
    ) -> Vec<String>;

    /// The argv passed to the CLI for a headless session, or `None` for an
    /// agent that has no headless mode.
    fn headless_args(
        &self,
        _opts: &Self::Options,
        _ctx: &SpawnContext<'_>,
        _members: &[SessionMember],
        _prompt: &str,
    ) -> Option<Vec<String>> {
        None
    }

    /// Read one raw stdout line emitted by a headless child into the changes
    /// it makes to the session, in order. A line that carries none, or that
    /// does not parse, yields no events.
    fn parse_headless_line(&self, _line: &str) -> Vec<HeadlessEvent> {
        Vec::new()
    }

    /// The contents of the hook settings file an interactive spawn of this
    /// CLI passes, or `None` for a CLI that takes none. `tracer_exe` is the
    /// cached tracer whose `hook` shim the hooks run, `session_id` the
    /// session they report for, and `config_dir` the daemon's config dir,
    /// where the shim reads the daemon's port and auth token.
    fn hook_settings_file(
        &self,
        _tracer_exe: &Path,
        _session_id: &str,
        _config_dir: &Path,
    ) -> Option<String> {
        None
    }
}

/// An [`Agent`] as the registry holds it: object-safe, taking the spawn's
/// [`OptionValues`] where the typed trait takes its options. Every [`Agent`]
/// implements it; a method that builds a spawn decodes the values into the
/// agent's options and refuses the spawn with an [`AgentError`] naming the
/// agent when they do not decode.
pub trait AgentPlugin: Send + Sync {
    /// See [`Agent::descriptor`].
    fn descriptor(&self) -> &AgentDescriptor;

    /// See [`Agent::program_env_var`].
    fn program_env_var(&self) -> &'static str;

    /// See [`Agent::default_program`].
    fn default_program(&self) -> &'static str;

    /// [`Agent::interactive_args`] over `opts` decoded.
    fn interactive_args(
        &self,
        opts: &OptionValues,
        ctx: &SpawnContext<'_>,
        members: &[SessionMember],
        initial_prompt: Option<&str>,
    ) -> Result<Vec<String>, AgentError>;

    /// [`Agent::headless_args`] over `opts` decoded; `Ok(None)` for an agent
    /// with no headless mode.
    fn headless_args(
        &self,
        opts: &OptionValues,
        ctx: &SpawnContext<'_>,
        members: &[SessionMember],
        prompt: &str,
    ) -> Result<Option<Vec<String>>, AgentError>;

    /// See [`Agent::parse_headless_line`].
    fn parse_headless_line(&self, line: &str) -> Vec<HeadlessEvent>;

    /// See [`Agent::hook_settings_file`].
    fn hook_settings_file(
        &self,
        tracer_exe: &Path,
        session_id: &str,
        config_dir: &Path,
    ) -> Option<String>;

    /// Resolve the executable path (the env-var override, or the default).
    /// Concrete `which`/shim resolution lives in the daemon's
    /// `resolve_agent_program`.
    fn resolve_program(&self) -> String {
        std::env::var(self.program_env_var()).unwrap_or_else(|_| self.default_program().to_string())
    }
}

impl<A: Agent> AgentPlugin for A {
    fn descriptor(&self) -> &AgentDescriptor {
        Agent::descriptor(self)
    }

    fn program_env_var(&self) -> &'static str {
        Agent::program_env_var(self)
    }

    fn default_program(&self) -> &'static str {
        Agent::default_program(self)
    }

    fn interactive_args(
        &self,
        opts: &OptionValues,
        ctx: &SpawnContext<'_>,
        members: &[SessionMember],
        initial_prompt: Option<&str>,
    ) -> Result<Vec<String>, AgentError> {
        let opts = decode_options(self, opts)?;
        Ok(Agent::interactive_args(
            self,
            &opts,
            ctx,
            members,
            initial_prompt,
        ))
    }

    fn headless_args(
        &self,
        opts: &OptionValues,
        ctx: &SpawnContext<'_>,
        members: &[SessionMember],
        prompt: &str,
    ) -> Result<Option<Vec<String>>, AgentError> {
        let opts = decode_options(self, opts)?;
        Ok(Agent::headless_args(self, &opts, ctx, members, prompt))
    }

    fn parse_headless_line(&self, line: &str) -> Vec<HeadlessEvent> {
        Agent::parse_headless_line(self, line)
    }

    fn hook_settings_file(
        &self,
        tracer_exe: &Path,
        session_id: &str,
        config_dir: &Path,
    ) -> Option<String> {
        Agent::hook_settings_file(self, tracer_exe, session_id, config_dir)
    }
}

/// `values` as `agent`'s typed options.
fn decode_options<A: Agent>(agent: &A, values: &OptionValues) -> Result<A::Options, AgentError> {
    serde_json::to_value(values)
        .and_then(serde_json::from_value)
        .map_err(|err| AgentError {
            message: format!(
                "{} could not read the spawn's options: {err}",
                Agent::descriptor(agent).label
            ),
        })
}

/// An agent refused a call: the spawn's options did not decode into the
/// agent's options. The message names the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentError {
    message: String,
}

impl fmt::Display for AgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for AgentError {}

/// A set of option values an agent's schema refuses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    /// The key whose value was refused.
    pub field: String,
    /// What is wrong with it, for people.
    pub message: String,
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ValidationError {}

/// Check `values` against `descriptor`'s option schema and answer the values
/// a spawn runs with: every field's value, its default where `values` names
/// none, without the fields a `when` rule hides.
///
/// Refuses a key the schema has no field for, a toggle's value that is not a
/// bool, a choice's value that is not one of its choices (no value only when
/// the field's default is no value), and a text or model value that is
/// neither text nor no value.
pub fn validate(
    descriptor: &AgentDescriptor,
    values: &OptionValues,
) -> Result<OptionValues, ValidationError> {
    if let Some(key) = values
        .keys()
        .find(|key| !descriptor.fields.iter().any(|field| &field.id == *key))
    {
        return Err(ValidationError {
            field: key.clone(),
            message: format!("{} has no option `{key}`", descriptor.label),
        });
    }
    let mut checked = OptionValues::new();
    for field in &descriptor.fields {
        let value = values.get(&field.id).unwrap_or(&field.default);
        if !accepts(field, value) {
            return Err(ValidationError {
                field: field.id.clone(),
                message: format!(
                    "{}'s option `{}` does not take {}",
                    descriptor.label,
                    field.id,
                    describe(value)
                ),
            });
        }
        checked.insert(field.id.clone(), value.clone());
    }
    let hidden: Vec<&str> = descriptor
        .fields
        .iter()
        .filter(|field| is_hidden(field, &checked))
        .map(|field| field.id.as_str())
        .collect();
    for id in hidden {
        checked.remove(id);
    }
    Ok(checked)
}

/// Whether `field` takes `value`. A kind from a newer peer takes anything.
fn accepts(field: &OptionField, value: &OptionValue) -> bool {
    match (&field.kind, value) {
        (FieldKind::Toggle, OptionValue::Bool(_))
        | (
            FieldKind::Text { .. } | FieldKind::Model { .. },
            OptionValue::Text(_) | OptionValue::Null,
        )
        | (FieldKind::Unknown, _) => true,
        (FieldKind::Choice { choices, .. }, OptionValue::Text(text)) => {
            choices.iter().any(|choice| &choice.value == text)
        }
        (FieldKind::Choice { .. }, OptionValue::Null) => field.default == OptionValue::Null,
        _ => false,
    }
}

/// Whether one of `field`'s rules hides it while the fields hold `values`.
fn is_hidden(field: &OptionField, values: &OptionValues) -> bool {
    field.when.iter().any(|rule| {
        rule.then.hidden == Some(true) && values.get(&rule.if_field) == Some(&rule.equals)
    })
}

/// `value` as an error message names it.
fn describe(value: &OptionValue) -> String {
    match value {
        OptionValue::Bool(flag) => flag.to_string(),
        OptionValue::Text(text) => format!("\"{text}\""),
        OptionValue::Null => "no value".to_string(),
    }
}

/// Build a workspace-context prelude for the agent. Returns `Some` only when
/// the session has 2+ members. The note maps each member's repo name to its
/// per-session worktree path so the agent doesn't try to navigate to
/// original-repo paths referenced in its `instruction_files` that no longer
/// match where the session is rooted.
#[must_use]
pub fn workspace_prelude(members: &[SessionMember], instruction_files: &[&str]) -> Option<String> {
    if members.len() < 2 {
        return None;
    }
    let mut out = format!(
        "Workspace member paths for this session (use these for cross-repo \
         file access — they override any absolute paths referenced in \
         {}):\n",
        instruction_files.join(" / ")
    );
    let name_width = members.iter().map(|m| m.repo_name.len()).max().unwrap_or(0);
    for m in members {
        use std::fmt::Write as _;
        // Width-padded for visual alignment in the agent's view; failure
        // here would mean OOM during string formatting, which we treat as
        // unreachable for a few dozen members at most.
        let _ = writeln!(
            out,
            "  {name:<width$}  ->  {path}",
            name = m.repo_name,
            width = name_width,
            path = m.worktree_path,
        );
    }
    Some(out)
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use protocol::{
        AccentToken, AgentCaps, AgentId, AgentTexts, Choice, ChoiceStyle, FieldKind, FieldOverride,
        FieldRule, OptionField, ShiftEnter, SpawnPlacement,
    };
    use std::sync::LazyLock;

    fn field(id: &str, kind: FieldKind, default: OptionValue) -> OptionField {
        OptionField {
            id: id.to_string(),
            label: id.to_string(),
            help: None,
            kind,
            default,
            settings: true,
            spawn: SpawnPlacement::Main,
            locked_when_trusted: false,
            when: Vec::new(),
        }
    }

    fn text(value: &str) -> OptionValue {
        OptionValue::Text(value.to_string())
    }

    /// A schema with a choice, a toggle, and a text field the toggle hides.
    static DESCRIPTOR: LazyLock<AgentDescriptor> = LazyLock::new(|| AgentDescriptor {
        id: AgentId("test-agent".to_string()),
        label: "Test agent".to_string(),
        tag: "test".to_string(),
        accent: AccentToken::Default,
        shift_enter: ShiftEnter::Lf,
        caps: AgentCaps {
            headless: false,
            resume: false,
            extra_folders: false,
            shell_resume: false,
        },
        trust_note: None,
        texts: AgentTexts {
            shell_resume_label: String::new(),
            no_conversation: String::new(),
        },
        fields: vec![
            field(
                "mode",
                FieldKind::Choice {
                    choices: ["fast", "slow"]
                        .map(|value| Choice {
                            value: value.to_string(),
                            label: value.to_string(),
                        })
                        .to_vec(),
                    style: ChoiceStyle::Segmented,
                },
                text("fast"),
            ),
            field("sandbox", FieldKind::Toggle, OptionValue::Bool(false)),
            OptionField {
                when: vec![FieldRule {
                    if_field: "sandbox".to_string(),
                    equals: OptionValue::Bool(true),
                    then: FieldOverride {
                        hidden: Some(true),
                        placeholder: None,
                        suggestions: None,
                    },
                }],
                ..field(
                    "profile",
                    FieldKind::Text { placeholder: None },
                    OptionValue::Null,
                )
            },
        ],
        chips: Vec::new(),
    });

    fn values(pairs: &[(&str, OptionValue)]) -> OptionValues {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), value.clone()))
            .collect()
    }

    #[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
    struct TestOptions {
        mode: String,
        sandbox: bool,
        profile: Option<String>,
    }

    struct TestAgent;

    impl Agent for TestAgent {
        type Options = TestOptions;

        fn descriptor(&self) -> &AgentDescriptor {
            &DESCRIPTOR
        }

        fn program_env_var(&self) -> &'static str {
            "RUSTLING_TULIP_TEST_AGENT"
        }

        fn default_program(&self) -> &'static str {
            "test-agent"
        }

        fn interactive_args(
            &self,
            opts: &TestOptions,
            _ctx: &SpawnContext<'_>,
            _members: &[SessionMember],
            _initial_prompt: Option<&str>,
        ) -> Vec<String> {
            vec![opts.mode.clone(), opts.sandbox.to_string()]
        }
    }

    fn context() -> SpawnContext<'static> {
        SpawnContext {
            dangerously_skip_permissions: false,
            has_prompt_injector: false,
            conversation_id: None,
            resume_conversation: None,
            add_dirs: &[],
            settings_file: None,
        }
    }

    #[test]
    fn validate_fills_defaults_and_drops_hidden() {
        assert_eq!(
            validate(&DESCRIPTOR, &OptionValues::new()),
            Ok(values(&[
                ("mode", text("fast")),
                ("profile", OptionValue::Null),
                ("sandbox", OptionValue::Bool(false)),
            ]))
        );
        let hidden = values(&[
            ("sandbox", OptionValue::Bool(true)),
            ("profile", text("work")),
        ]);
        assert_eq!(
            validate(&DESCRIPTOR, &hidden),
            Ok(values(&[
                ("mode", text("fast")),
                ("sandbox", OptionValue::Bool(true)),
            ]))
        );
    }

    #[test]
    fn validate_refuses_unknown_key() {
        let err = validate(&DESCRIPTOR, &values(&[("colour", text("red"))]))
            .expect_err("an unknown key refuses");
        assert_eq!(err.field, "colour");
        assert!(err.to_string().contains("colour"), "{err}");
    }

    #[test]
    fn validate_refuses_value_outside_choices() {
        for bad in [text("medium"), OptionValue::Null, OptionValue::Bool(true)] {
            let err = validate(&DESCRIPTOR, &values(&[("mode", bad.clone())]))
                .expect_err("a value outside the choices refuses");
            assert_eq!(err.field, "mode", "{bad:?}");
            assert!(err.to_string().contains("mode"), "{err}");
        }
        let err = validate(&DESCRIPTOR, &values(&[("sandbox", text("yes"))]))
            .expect_err("a toggle takes only a bool");
        assert_eq!(err.field, "sandbox");
    }

    #[test]
    fn adapter_refuses_options_that_do_not_decode() {
        let plugin: &dyn AgentPlugin = &TestAgent;
        let good = values(&[
            ("mode", text("slow")),
            ("sandbox", OptionValue::Bool(true)),
            ("profile", OptionValue::Null),
        ]);
        assert_eq!(
            plugin.interactive_args(&good, &context(), &[], None),
            Ok(vec!["slow".to_string(), "true".to_string()])
        );
        assert_eq!(plugin.headless_args(&good, &context(), &[], "hi"), Ok(None));
        let bad = values(&[("mode", text("slow")), ("sandbox", text("yes"))]);
        let err = plugin
            .interactive_args(&bad, &context(), &[], None)
            .expect_err("options that do not decode refuse the spawn");
        assert!(err.to_string().contains("Test agent"), "{err}");
        plugin
            .headless_args(&bad, &context(), &[], "hi")
            .expect_err("options that do not decode refuse a headless spawn");
    }

    #[test]
    fn option_value_round_trips_untagged() {
        let map = values(&[
            ("a", OptionValue::Bool(true)),
            ("b", text("x")),
            ("c", OptionValue::Null),
        ]);
        let json = serde_json::to_string(&map).expect("option values serialize");
        assert_eq!(json, r#"{"a":true,"b":"x","c":null}"#);
        let back: OptionValues = serde_json::from_str(&json).expect("option values parse");
        assert_eq!(back, map);
    }

    fn member(name: &str, path: &str) -> SessionMember {
        SessionMember {
            repo_id: format!("id-{name}"),
            repo_name: name.to_string(),
            branch: "main".to_string(),
            worktree_path: path.to_string(),
        }
    }

    #[test]
    fn workspace_prelude_names_the_instruction_files() {
        let files = ["CLAUDE.md", "AGENTS.md"];
        let two = [member("a", "X:/wt/a"), member("longer", "X:/wt/longer")];
        assert_eq!(
            workspace_prelude(&two, &files).as_deref(),
            Some(
                "Workspace member paths for this session (use these for cross-repo file access \
                 — they override any absolute paths referenced in CLAUDE.md / AGENTS.md):\n  \
                 a       ->  X:/wt/a\n  longer  ->  X:/wt/longer\n"
            )
        );
        assert_eq!(workspace_prelude(&two[..1], &files), None);
    }
}
