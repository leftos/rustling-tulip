//! A scripted agent, for tests of the plugin seam and of the code that drives
//! a plugin through it.

use crate::{
    Agent, ConversationPlan, ConversationQuery, EnvPlan, FoundConversation, HookReport, KeyLookup,
    PendingUpdate, Refusal, ResolvedRows, SpawnCheck, SpawnContext,
};
use protocol::{
    AccentToken, AgentCaps, AgentDescriptor, AgentId, AgentTexts, Choice, ChoiceStyle, FieldKind,
    FieldOverride, FieldRule, OptionField, OptionValue, SessionMember, SessionStatus, ShiftEnter,
    SpawnPlacement,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};

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

/// The conversations a [`TestAgent`] finds, keyed by the folder text a query
/// names.
type Script = HashMap<String, Vec<FoundConversation>>;

/// An agent whose conversation answers the test sets: `conversation_plan`
/// returns `plan`, whatever it is asked, and `find_conversations` returns
/// the conversations last set for the query's folder (see
/// [`TestAgent::set_found`]), only the one `known_id` names when it names
/// one, and ignores the window. Clones share one script, so a test holding a
/// clone can make a conversation appear while the daemon holds the agent.
/// After [`TestAgent::set_panics`], `find_conversations` panics instead.
/// Its interactive argv is its `mode` and `sandbox` options, its
/// environment sets `TEST_MODE` to its `mode`, and it maps a hook body
/// naming a `test_status` (see its `map_hook_event`).
#[derive(Debug, Clone, Default)]
pub struct TestAgent {
    pub plan: ConversationPlan,
    script: Arc<Mutex<Script>>,
    panics: Arc<AtomicBool>,
}

impl TestAgent {
    /// An agent that plans `plan` and finds no conversation yet.
    #[must_use]
    pub fn new(plan: ConversationPlan) -> Self {
        Self {
            plan,
            ..Self::default()
        }
    }

    /// From now on, `find_conversations` panics when `panics` is true, as a
    /// plugin with a bug would.
    pub fn set_panics(&self, panics: bool) {
        self.panics.store(panics, Ordering::SeqCst);
    }

    /// From now on, `find_conversations` for `folder` (its text as the query
    /// names it, compared verbatim) answers `found`.
    pub fn set_found(&self, folder: &str, found: Vec<FoundConversation>) {
        self.script().insert(folder.to_string(), found);
    }

    fn script(&self) -> MutexGuard<'_, Script> {
        self.script.lock().unwrap_or_else(PoisonError::into_inner)
    }
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

    /// A body whose `test_status` is `working`, `idle` or `awaiting_input`
    /// maps to that status, its last message naming the spawn's `mode`
    /// (`none` without options) and trust: `"<mode> trusted=<trusted>"`.
    /// Any other body maps to nothing.
    fn map_hook_event(
        &self,
        body: &serde_json::Value,
        spawn: Option<&TestOptions>,
        trusted: bool,
    ) -> Option<HookReport> {
        let status = match body.get("test_status")?.as_str()? {
            "working" => SessionStatus::Working,
            "idle" => SessionStatus::Idle,
            "awaiting_input" => SessionStatus::AwaitingInput,
            _ => return None,
        };
        let mode = spawn.map_or("none", |opts| opts.mode.as_str());
        Some(HookReport {
            status: Some(status),
            pending: PendingUpdate::Clear,
            conversation_id: None,
            last_message: Some(format!("{mode} trusted={trusted}")),
            failure: None,
            permission_mismatch: None,
            agent_ended: false,
            agent_started: false,
            subagent: false,
            background: None,
            weak_idle: false,
        })
    }

    fn find_conversations(&self, query: &ConversationQuery<'_>) -> Vec<FoundConversation> {
        assert!(
            !self.panics.load(Ordering::SeqCst),
            "test agent: find_conversations scripted to panic"
        );
        let folder = query.folder.to_string_lossy();
        let found = self
            .script()
            .get(folder.as_ref())
            .cloned()
            .unwrap_or_default();
        match query.known_id {
            Some(id) => found.into_iter().filter(|c| c.id == id).collect(),
            None => found,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::TestAgent;
    use crate::{
        AgentPlugin, ConversationPlan, ConversationQuery, FoundConversation, ResolvedRows,
    };
    use std::path::Path;

    fn conversation(id: &str) -> FoundConversation {
        FoundConversation {
            id: id.to_string(),
            last_active: chrono::DateTime::UNIX_EPOCH,
            title: None,
        }
    }

    fn find(agent: &TestAgent, folder: &str, known_id: Option<&str>) -> Vec<FoundConversation> {
        let plugin: &dyn AgentPlugin = agent;
        let rows = ResolvedRows::default();
        plugin.find_conversations(&ConversationQuery {
            folder: Path::new(folder),
            window: None,
            known_id,
            rows: &rows,
        })
    }

    #[test]
    fn script_changes_are_seen_by_the_next_find() {
        let agent = TestAgent::new(ConversationPlan::DiscoverAfterLaunch);
        let handle = agent.clone();
        assert_eq!(find(&agent, r"D:\proj", None), []);
        handle.set_found(r"D:\proj", vec![conversation("c-1")]);
        assert_eq!(find(&agent, r"D:\proj", None), [conversation("c-1")]);
        assert_eq!(find(&agent, r"D:\other", None), [], "another folder");
        handle.set_found(r"D:\proj", vec![conversation("c-2"), conversation("c-1")]);
        assert_eq!(
            find(&agent, r"D:\proj", None),
            [conversation("c-2"), conversation("c-1")]
        );
    }

    #[test]
    fn known_id_filters_the_found_list() {
        let agent = TestAgent::new(ConversationPlan::None);
        agent.set_found(r"D:\proj", vec![conversation("c-1"), conversation("c-2")]);
        assert_eq!(find(&agent, r"D:\proj", Some("c-2")), [conversation("c-2")]);
        assert_eq!(find(&agent, r"D:\proj", Some("c-9")), []);
    }
}
