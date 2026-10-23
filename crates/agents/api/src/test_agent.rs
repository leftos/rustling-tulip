//! A scripted agent, for tests of the plugin seam and of the code that drives
//! a plugin through it.

use crate::{
    Agent, ConversationPlan, ConversationQuery, EnvPlan, FoundConversation, KeyLookup, Refusal,
    ResolvedRows, SpawnCheck, SpawnContext,
};
use protocol::{
    AccentToken, AgentCaps, AgentDescriptor, AgentId, AgentTexts, Choice, ChoiceStyle, FieldKind,
    FieldOverride, FieldRule, OptionField, OptionValue, SessionMember, ShiftEnter, SpawnPlacement,
};
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

/// The options [`TestAgent`] takes, one per field of [`DESCRIPTOR`].
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct TestOptions {
    pub mode: String,
    pub sandbox: bool,
    pub profile: Option<String>,
}

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

/// [`TestAgent`]'s schema: a choice, a toggle, and a text field the toggle
/// hides.
pub static DESCRIPTOR: LazyLock<AgentDescriptor> = LazyLock::new(|| AgentDescriptor {
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
            OptionValue::Text("fast".to_string()),
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

/// An agent whose conversation answers the test sets: `conversation_plan`
/// returns `plan` and `find_conversations` returns `found`, whatever either is
/// asked. Its interactive argv is its `mode` and `sandbox` options, and its
/// environment sets `TEST_MODE` to its `mode`.
#[derive(Debug, Clone, Default)]
pub struct TestAgent {
    pub plan: ConversationPlan,
    pub found: Vec<FoundConversation>,
}

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

    fn check_request(&self, _opts: &TestOptions, _req: &SpawnCheck<'_>) -> Result<(), Refusal> {
        Ok(())
    }

    fn spawn_env(
        &self,
        opts: &TestOptions,
        _rows: &ResolvedRows,
        _keys: &dyn KeyLookup,
    ) -> Result<EnvPlan, Refusal> {
        Ok(EnvPlan {
            set: vec![("TEST_MODE".to_string(), opts.mode.clone())],
            remove: Vec::new(),
        })
    }

    fn conversation_plan(&self, _opts: &TestOptions, _resume: Option<&str>) -> ConversationPlan {
        self.plan.clone()
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

    fn find_conversations(&self, _query: &ConversationQuery<'_>) -> Vec<FoundConversation> {
        self.found.clone()
    }
}
