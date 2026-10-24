//! One agent's spawn options: the agent's descriptor and the values its
//! fields hold, read the way the daemon's `validate` reads them, and the
//! fields the spawn dialog draws from them.

use std::collections::BTreeMap;

use protocol::{
    AgentDescriptor, AgentId, ChoiceStyle, FieldKind, FieldOverride, OptionField, OptionValue,
    OptionValues, SpawnPlacement,
};

/// The label of the choice that stands for no value, and the placeholder of
/// a text or model field whose default is no value.
pub(crate) const CLI_DEFAULT_LABEL: &str = "CLI default";

/// The selector part of the choice that stands for no value.
pub(crate) const CLI_DEFAULT_SELECTOR: &str = "cli-default";

/// One agent's options as a spawn sets them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentOptions {
    descriptor: AgentDescriptor,
    /// The values set for the fields, as stored or typed; a field without
    /// one holds its default.
    values: OptionValues,
}

/// One choice a choice field offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChoiceOption {
    /// The value the choice stores: a choice's text, or no value.
    pub value: OptionValue,
    pub label: String,
    /// The choice's part of its control's selector: the value with `_` as
    /// `-`, or [`CLI_DEFAULT_SELECTOR`].
    pub selector: String,
}

/// How a field is edited, with the field's `when` rules applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FieldEditor {
    Toggle,
    Choice {
        choices: Vec<ChoiceOption>,
        style: ChoiceStyle,
    },
    Text {
        placeholder: Option<String>,
    },
    Model {
        placeholder: Option<String>,
        suggestions: Vec<String>,
    },
    /// A kind from a newer daemon, which this client cannot edit.
    Unknown,
}

/// One field the spawn dialog draws.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FieldView {
    pub id: String,
    pub label: String,
    pub help: Option<String>,
    /// [`SpawnPlacement::Main`] or [`SpawnPlacement::Advanced`].
    pub placement: SpawnPlacement,
    pub editor: FieldEditor,
    /// The value the field shows: the one the spawn runs with, or for a
    /// field a trusted launch locks, the one kept for an untrusted launch.
    pub value: OptionValue,
    /// Whether the field cannot be changed: a field locked on a trusted
    /// launch while the launch is trusted, or a kind this client does not
    /// know.
    pub disabled: bool,
}

impl AgentOptions {
    /// The options of a spawn of `descriptor`'s agent before anything is
    /// chosen: each field's value in `settings` (the agent's Settings page
    /// values), else its default.
    pub(crate) fn new(descriptor: AgentDescriptor, settings: Option<&OptionValues>) -> Self {
        let mut options = Self {
            descriptor,
            values: OptionValues::new(),
        };
        if let Some(settings) = settings {
            options.values = options.known(settings);
        }
        options
    }

    /// Replaces the values with a stored spawn's, `stored`: a spawn sends
    /// only the values that differ from their defaults, so a field `stored`
    /// does not name ran on its default and takes it, not the value
    /// underneath; a key the descriptor has no field for is dropped. A
    /// spawn's initial values are the Settings values
    /// ([`AgentOptions::new`]), then the target's last spawn of the agent,
    /// then the source a duplicate copies, each seeded over the one before.
    pub(crate) fn seed(&mut self, stored: &OptionValues) {
        self.values = self.known(stored);
    }

    /// The values of `stored` whose key names one of the agent's fields.
    fn known(&self, stored: &OptionValues) -> OptionValues {
        stored
            .iter()
            .filter(|(id, _)| self.has_field(id))
            .map(|(id, value)| (id.clone(), value.clone()))
            .collect()
    }

    /// Sets field `id` to `value`, as the user chose or typed it.
    pub(crate) fn set(&mut self, id: &str, value: OptionValue) {
        if self.has_field(id) {
            self.values.insert(id.to_owned(), value);
        } else {
            tracing::warn!(
                agent = %self.descriptor.id.0,
                field = id,
                "an option was set that the agent has no field for; it is not sent"
            );
        }
    }

    /// Field `id` holds what it holds in `from`, set or not: a re-seed
    /// keeps the fields the user set.
    pub(crate) fn keep_field(&mut self, id: &str, from: &Self) {
        match from.values.get(id) {
            Some(value) => {
                self.values.insert(id.to_owned(), value.clone());
            }
            None => {
                self.values.remove(id);
            }
        }
    }

    fn has_field(&self, id: &str) -> bool {
        self.descriptor.fields.iter().any(|field| field.id == id)
    }

    /// Every field's value as a spawn runs it: the value set, read as its
    /// field reads it, else the default; on a trusted launch a locked field
    /// holds its default.
    fn resolved(&self, trusted: bool) -> OptionValues {
        self.descriptor
            .fields
            .iter()
            .map(|field| {
                let value = match self.values.get(&field.id) {
                    Some(value) if !(trusted && field.locked_when_trusted) => read(field, value),
                    _ => field.default.clone(),
                };
                (field.id.clone(), value)
            })
            .collect()
    }

    /// The fields the spawn dialog draws: the Main and Advanced fields no
    /// `when` rule hides, in the descriptor's order. A field a trusted
    /// launch locks shows the value it keeps for an untrusted one.
    #[must_use]
    pub(crate) fn fields(&self, trusted: bool) -> Vec<FieldView> {
        let resolved = self.resolved(trusted);
        let kept = self.resolved(false);
        self.descriptor
            .fields
            .iter()
            .filter(|field| matches!(field.spawn, SpawnPlacement::Main | SpawnPlacement::Advanced))
            .filter(|field| !is_hidden(field, &resolved))
            .map(|field| FieldView {
                id: field.id.clone(),
                label: field.label.clone(),
                help: field.help.clone(),
                placement: field.spawn,
                editor: editor(field, &resolved),
                value: kept
                    .get(&field.id)
                    .cloned()
                    .unwrap_or_else(|| field.default.clone()),
                disabled: matches!(field.kind, FieldKind::Unknown)
                    || (trusted && field.locked_when_trusted),
            })
            .collect()
    }

    /// Whether an Advanced field the dialog draws holds a value other than
    /// its default, so the dialog opens its Advanced section on it.
    #[must_use]
    pub(crate) fn advanced_set(&self) -> bool {
        self.descriptor
            .fields
            .iter()
            .filter(|field| field.spawn == SpawnPlacement::Advanced)
            .any(|field| {
                self.fields(false)
                    .iter()
                    .any(|view| view.id == field.id && view.value != field.default)
            })
    }

    /// The values a spawn sends: only the fields whose value differs from
    /// the field's default, without the fields a `when` rule hides and,
    /// on a trusted launch, the fields it locks. The daemon fills the rest
    /// with their defaults.
    #[must_use]
    pub(crate) fn wire_values(&self, trusted: bool) -> OptionValues {
        let resolved = self.resolved(trusted);
        self.descriptor
            .fields
            .iter()
            .filter(|field| !is_hidden(field, &resolved))
            .filter_map(|field| {
                resolved
                    .get(&field.id)
                    .filter(|value| **value != field.default)
                    .map(|value| (field.id.clone(), value.clone()))
            })
            .collect()
    }
}

impl crate::RootView {
    /// The daemon's agents, in its order, as this connection last heard
    /// them; empty while disconnected.
    #[must_use]
    pub fn agents(&self) -> &[AgentDescriptor] {
        &self.agents
    }

    /// Each agent's Settings page values as this connection last heard
    /// them; empty while disconnected.
    #[must_use]
    pub fn agent_settings(&self) -> &BTreeMap<AgentId, OptionValues> {
        &self.agent_settings
    }

    /// Keeps the daemon's agents or their Settings values: each message
    /// replaces what the last one of its kind said. Any other message is
    /// one the root view does not fold, and is dropped.
    pub(crate) fn on_agents(&mut self, msg: protocol::DaemonMessage) {
        match msg {
            protocol::DaemonMessage::Agents { agents } => self.agents = agents,
            protocol::DaemonMessage::AgentSettings { settings } => self.agent_settings = settings,
            _ => {}
        }
    }

    /// Forgets the daemon's agents and their Settings values, as a
    /// disconnect does.
    pub(crate) fn forget_agents(&mut self) {
        self.agents.clear();
        self.agent_settings.clear();
    }
}

/// `value` as `field` reads it: a text or model value trimmed, blank as no
/// value; a value the field does not take (a choice it does not offer, no
/// value on a choice with a default, a value of the wrong shape) as the
/// field's default; anything for a kind this client does not know.
fn read(field: &OptionField, value: &OptionValue) -> OptionValue {
    match (&field.kind, value) {
        (FieldKind::Unknown, _)
        | (FieldKind::Toggle, OptionValue::Bool(_))
        | (FieldKind::Text { .. } | FieldKind::Model { .. }, OptionValue::Null) => value.clone(),
        (FieldKind::Text { .. } | FieldKind::Model { .. }, OptionValue::Text(text)) => {
            let text = text.trim();
            if text.is_empty() {
                OptionValue::Null
            } else {
                OptionValue::Text(text.to_owned())
            }
        }
        (FieldKind::Choice { choices, .. }, OptionValue::Text(text))
            if choices.iter().any(|choice| &choice.value == text) =>
        {
            value.clone()
        }
        (FieldKind::Choice { .. }, OptionValue::Null) if field.default == OptionValue::Null => {
            OptionValue::Null
        }
        _ => field.default.clone(),
    }
}

/// Whether one of `field`'s rules hides it while the fields hold `values`,
/// as the daemon's `validate` decides it.
fn is_hidden(field: &OptionField, values: &OptionValues) -> bool {
    field.when.iter().any(|rule| {
        rule.then.hidden == Some(true) && values.get(&rule.if_field) == Some(&rule.equals)
    })
}

/// The placeholder and suggestions the rules of `field` that match `values`
/// set, a later rule winning over an earlier one.
fn overrides(field: &OptionField, values: &OptionValues) -> FieldOverride {
    let mut merged = FieldOverride {
        hidden: None,
        placeholder: None,
        suggestions: None,
    };
    let matching = field
        .when
        .iter()
        .filter(|rule| values.get(&rule.if_field) == Some(&rule.equals));
    for rule in matching {
        if rule.then.placeholder.is_some() {
            merged.placeholder.clone_from(&rule.then.placeholder);
        }
        if rule.then.suggestions.is_some() {
            merged.suggestions.clone_from(&rule.then.suggestions);
        }
    }
    merged
}

/// How `field` is edited while the fields hold `values`.
fn editor(field: &OptionField, values: &OptionValues) -> FieldEditor {
    let overrides = overrides(field, values);
    match &field.kind {
        FieldKind::Toggle => FieldEditor::Toggle,
        FieldKind::Choice { choices, style } => FieldEditor::Choice {
            choices: choice_options(field, choices),
            style: *style,
        },
        FieldKind::Text { placeholder } => FieldEditor::Text {
            placeholder: placeholder_of(field, placeholder.as_deref(), overrides.placeholder),
        },
        FieldKind::Model {
            placeholder,
            suggestions,
        } => FieldEditor::Model {
            placeholder: placeholder_of(field, placeholder.as_deref(), overrides.placeholder),
            suggestions: overrides.suggestions.unwrap_or_else(|| suggestions.clone()),
        },
        FieldKind::Unknown => FieldEditor::Unknown,
    }
}

/// A text or model field's placeholder: a rule's, else the field's own,
/// else [`CLI_DEFAULT_LABEL`] when the field's default is no value.
fn placeholder_of(
    field: &OptionField,
    own: Option<&str>,
    from_rule: Option<String>,
) -> Option<String> {
    from_rule
        .or_else(|| own.map(str::to_owned))
        .or_else(|| (field.default == OptionValue::Null).then(|| CLI_DEFAULT_LABEL.to_owned()))
}

/// A choice field's choices: a no-value choice first when the field's
/// default is no value, then the descriptor's.
fn choice_options(field: &OptionField, choices: &[protocol::Choice]) -> Vec<ChoiceOption> {
    let no_value = (field.default == OptionValue::Null).then(|| ChoiceOption {
        value: OptionValue::Null,
        label: CLI_DEFAULT_LABEL.to_owned(),
        selector: CLI_DEFAULT_SELECTOR.to_owned(),
    });
    no_value
        .into_iter()
        .chain(choices.iter().map(|choice| ChoiceOption {
            value: OptionValue::Text(choice.value.clone()),
            label: choice.label.clone(),
            selector: choice.value.replace('_', "-"),
        }))
        .collect()
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::expect_used,
        clippy::panic,
        reason = "a test fails with the message of the precondition it lost"
    )]

    use super::*;
    use protocol::AgentId;

    fn test_agent() -> AgentDescriptor {
        agent_api::test_agent::DESCRIPTOR.clone()
    }

    fn claude() -> AgentDescriptor {
        agent_registry::descriptors()
            .into_iter()
            .find(|descriptor| descriptor.id == AgentId("claude".to_owned()))
            .expect("the registry has Claude")
    }

    fn text(value: &str) -> OptionValue {
        OptionValue::Text(value.to_owned())
    }

    fn values(pairs: &[(&str, OptionValue)]) -> OptionValues {
        pairs
            .iter()
            .map(|(id, value)| ((*id).to_owned(), value.clone()))
            .collect()
    }

    fn field<'a>(fields: &'a [FieldView], id: &str) -> &'a FieldView {
        fields
            .iter()
            .find(|field| field.id == id)
            .unwrap_or_else(|| panic!("no field `{id}` among {fields:?}"))
    }

    fn value_of(options: &AgentOptions, id: &str) -> OptionValue {
        field(&options.fields(false), id).value.clone()
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn defaults_come_from_the_field_then_the_settings_value() {
        let options = AgentOptions::new(test_agent(), Some(&values(&[("mode", text("slow"))])));
        assert_eq!(
            value_of(&options, "mode"),
            text("slow"),
            "the Settings value beats the field default"
        );
        assert_eq!(
            value_of(&options, "sandbox"),
            OptionValue::Bool(false),
            "a field the Settings do not name holds its default"
        );
        assert_eq!(
            options.wire_values(false),
            values(&[("mode", text("slow"))]),
            "a Settings value that differs from the field default is sent"
        );
    }

    #[test]
    fn a_missing_settings_key_falls_back_to_the_field_default() {
        for settings in [None, Some(OptionValues::new())] {
            let options = AgentOptions::new(test_agent(), settings.as_ref());
            assert_eq!(value_of(&options, "mode"), text("fast"), "{settings:?}");
            assert_eq!(value_of(&options, "sandbox"), OptionValue::Bool(false));
            assert_eq!(value_of(&options, "profile"), OptionValue::Null);
            assert_eq!(
                options.wire_values(false),
                OptionValues::new(),
                "nothing differs from the field defaults"
            );
        }
    }

    #[test]
    fn a_when_rule_overrides_placeholder_and_suggestions() {
        let mut options = AgentOptions::new(claude(), None);
        let model = |options: &AgentOptions| field(&options.fields(false), "model").editor.clone();
        assert_eq!(
            model(&options),
            FieldEditor::Model {
                placeholder: Some(CLI_DEFAULT_LABEL.to_owned()),
                suggestions: strings(&["opus", "sonnet", "haiku"]),
            },
            "no rule matches: the field's own suggestions and the no-value placeholder"
        );
        options.set("provider", text("deepseek"));
        assert_eq!(
            model(&options),
            FieldEditor::Model {
                placeholder: Some("deepseek-flash".to_owned()),
                suggestions: Vec::new(),
            },
            "the provider rule replaces both"
        );
    }

    #[test]
    fn a_hidden_field_is_dropped_from_the_wire_values() {
        let mut options = AgentOptions::new(test_agent(), None);
        options.set("profile", text("work"));
        assert_eq!(
            options.wire_values(false),
            values(&[("profile", text("work"))])
        );
        options.set("sandbox", OptionValue::Bool(true));
        assert!(
            options
                .fields(false)
                .iter()
                .all(|field| field.id != "profile"),
            "the sandbox rule hides profile"
        );
        assert_eq!(
            options.wire_values(false),
            values(&[("sandbox", OptionValue::Bool(true))]),
            "a hidden field's value is not sent"
        );
    }

    #[test]
    fn a_trusted_launch_locks_and_omits_locked_fields() {
        let mut options = AgentOptions::new(claude(), None);
        options.set("permission_mode", text("plan"));
        options.set("model", text("  opus "));
        let open = options.fields(false);
        assert!(!field(&open, "permission_mode").disabled);
        assert_eq!(
            options.wire_values(false),
            values(&[("model", text("opus")), ("permission_mode", text("plan"))]),
            "a model is sent trimmed"
        );

        let trusted = options.fields(true);
        assert!(
            field(&trusted, "permission_mode").disabled,
            "a trusted launch locks the permission mode"
        );
        assert_eq!(
            field(&trusted, "permission_mode").value,
            text("plan"),
            "a locked field shows the value it keeps"
        );
        assert!(
            !field(&trusted, "model").disabled,
            "a field not locked when trusted stays open"
        );
        assert_eq!(
            options.wire_values(true),
            values(&[("model", text("opus"))]),
            "the locked field is not sent"
        );
        assert_eq!(
            value_of(&options, "permission_mode"),
            text("plan"),
            "an untrusted launch keeps the value the lock set aside"
        );

        options.set("model", text("   "));
        assert_eq!(
            options.wire_values(false),
            values(&[("permission_mode", text("plan"))]),
            "a blank model is no value, the field default"
        );
    }

    #[test]
    fn a_choice_with_a_null_default_offers_no_value_first() {
        let options = AgentOptions::new(claude(), None);
        let fields = options.fields(false);
        let mode = field(&fields, "permission_mode");
        let FieldEditor::Choice { choices, .. } = &mode.editor else {
            panic!("permission_mode is a choice: {mode:?}");
        };
        assert_eq!(
            choices.first(),
            Some(&ChoiceOption {
                value: OptionValue::Null,
                label: CLI_DEFAULT_LABEL.to_owned(),
                selector: CLI_DEFAULT_SELECTOR.to_owned(),
            })
        );
        let selectors: Vec<&str> = choices
            .iter()
            .map(|choice| choice.selector.as_str())
            .collect();
        assert_eq!(
            selectors,
            [
                "cli-default",
                "default",
                "accept-edits",
                "bypass-permissions",
                "plan"
            ]
        );
        assert_eq!(mode.value, OptionValue::Null, "no value is chosen");

        let provider = field(&fields, "provider");
        let FieldEditor::Choice { choices, .. } = &provider.editor else {
            panic!("provider is a choice: {provider:?}");
        };
        let selectors: Vec<&str> = choices
            .iter()
            .map(|choice| choice.selector.as_str())
            .collect();
        assert_eq!(
            selectors,
            ["anthropic", "deepseek"],
            "a choice with a default offers no no-value choice"
        );
    }

    /// Values to store in `field`: one of every shape, and each of its
    /// choices.
    fn candidates(field: &OptionField) -> Vec<OptionValue> {
        let mut candidates = vec![
            OptionValue::Null,
            OptionValue::Bool(true),
            OptionValue::Bool(false),
            text(""),
            text("  padded  "),
            text("unknown-value"),
        ];
        if let FieldKind::Choice { choices, .. } = &field.kind {
            candidates.extend(choices.iter().map(|choice| text(&choice.value)));
        }
        candidates
    }

    /// Stored values to seed: none, each candidate of each field alone, and
    /// every field set at once to its candidate at each index.
    fn samples(descriptor: &AgentDescriptor) -> Vec<OptionValues> {
        let mut samples = vec![OptionValues::new()];
        for field in &descriptor.fields {
            for value in candidates(field) {
                samples.push(values(&[(field.id.as_str(), value)]));
            }
        }
        let widest = descriptor
            .fields
            .iter()
            .map(|field| candidates(field).len())
            .max()
            .unwrap_or(0);
        for index in 0..widest {
            samples.push(
                descriptor
                    .fields
                    .iter()
                    .filter_map(|field| {
                        candidates(field)
                            .get(index)
                            .map(|value| (field.id.clone(), value.clone()))
                    })
                    .collect(),
            );
        }
        samples
    }

    #[test]
    fn wire_values_pass_agent_api_validate() {
        for descriptor in [test_agent(), claude()] {
            for stored in samples(&descriptor) {
                for trusted in [false, true] {
                    let mut options = AgentOptions::new(descriptor.clone(), None);
                    options.seed(&stored);
                    let wire = options.wire_values(trusted);
                    if let Err(err) = agent_api::validate(&descriptor, &wire) {
                        panic!(
                            "{} refused {wire:?}, seeded {stored:?}, trusted {trusted}: {err}",
                            descriptor.id.0
                        );
                    }
                    for field in &descriptor.fields {
                        assert_ne!(
                            wire.get(&field.id),
                            Some(&field.default),
                            "a default is not sent: {wire:?}, seeded {stored:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn an_unknown_kind_is_disabled_and_keeps_its_stored_value() {
        let mut descriptor = test_agent();
        let template = descriptor.fields.first().cloned().expect("a first field");
        descriptor.fields.push(OptionField {
            id: "future".to_owned(),
            kind: FieldKind::Unknown,
            default: OptionValue::Null,
            ..template
        });
        let mut options = AgentOptions::new(descriptor.clone(), None);
        options.seed(&values(&[("future", text("  as stored "))]));

        let fields = options.fields(false);
        let future = field(&fields, "future");
        assert!(future.disabled, "this client cannot edit it");
        assert_eq!(future.editor, FieldEditor::Unknown);
        assert_eq!(future.value, text("  as stored "));
        let wire = options.wire_values(false);
        assert_eq!(
            wire,
            values(&[("future", text("  as stored "))]),
            "the stored value goes back untouched"
        );
        assert!(agent_api::validate(&descriptor, &wire).is_ok());
    }

    #[test]
    fn an_advanced_value_off_its_default_is_reported() {
        let mut options = AgentOptions::new(claude(), None);
        assert!(!options.advanced_set(), "every field at its default");
        options.set("provider", text("deepseek"));
        assert!(!options.advanced_set(), "provider is a Main field");
        options.set("model", text("   "));
        assert!(!options.advanced_set(), "a blank model is no value");
        options.set("model", text("opus"));
        assert!(options.advanced_set());
        options.set("model", OptionValue::Null);
        options.set("permission_mode", text("plan"));
        assert!(options.advanced_set());
    }

    #[test]
    fn a_stored_layer_at_the_default_beats_the_settings_value() {
        let settings = values(&[("provider", text("deepseek"))]);
        let mut options = AgentOptions::new(claude(), Some(&settings));
        assert_eq!(value_of(&options, "provider"), text("deepseek"));
        // A last spawn on Anthropic, the default, was stored without it.
        options.seed(&OptionValues::new());
        assert_eq!(
            value_of(&options, "provider"),
            text("anthropic"),
            "the stored spawn ran on the default, not the Settings value"
        );
        assert_eq!(options.wire_values(false), OptionValues::new());
    }

    #[test]
    fn a_duplicate_source_at_the_default_beats_the_last_spawn() {
        let mut options = AgentOptions::new(claude(), None);
        options.seed(&values(&[
            ("provider", text("deepseek")),
            ("model", text("deepseek-pro")),
        ]));
        assert_eq!(value_of(&options, "provider"), text("deepseek"));
        options.seed(&values(&[("model", text("opus"))]));
        assert_eq!(
            value_of(&options, "provider"),
            text("anthropic"),
            "the duplicate source ran on the default provider"
        );
        assert_eq!(
            options.wire_values(false),
            values(&[("model", text("opus"))])
        );
    }

    #[test]
    fn an_unknown_stored_choice_reads_as_the_default() {
        let mut options = AgentOptions::new(test_agent(), Some(&values(&[("mode", text("slow"))])));
        options.seed(&values(&[("mode", text("warp"))]));
        assert_eq!(
            value_of(&options, "mode"),
            text("fast"),
            "the field default, not the Settings value"
        );
        assert_eq!(options.wire_values(false), OptionValues::new());

        options.seed(&values(&[("mode", OptionValue::Null)]));
        assert_eq!(
            value_of(&options, "mode"),
            text("fast"),
            "no value on a choice with a default reads as the default"
        );
    }
}
