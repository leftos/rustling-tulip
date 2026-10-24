//! The check every agent crate runs in one test, so an agent's option schema
//! and its typed options cannot drift apart.

use crate::{Agent, LineMatcher, ScreenRegion, ScreenVerdict, decode_options, validate};
use protocol::{AgentDescriptor, FieldKind, OptionValue, OptionValues};

/// Check that `agent`'s option schema and its [`Agent::Options`] agree.
///
/// Each case (the empty map, every field's default, every choice of every
/// choice field, and the value each `when` rule tests for) is validated
/// against the descriptor, decoded into the agent's options, serialized back
/// into option values, and validated again; a shell resume command the
/// agent answers for those options must name the conversation it resumes.
///
/// # Panics
///
/// Panics naming the case and the step that failed, so the calling test
/// fails.
#[expect(
    clippy::panic,
    reason = "a conformance failure must fail the calling test"
)]
pub fn check<A: Agent>(agent: &A) {
    let descriptor = agent.descriptor();
    for (case, values) in cases(descriptor) {
        let validated = validate(descriptor, &values)
            .unwrap_or_else(|err| panic!("{case}: the schema refuses {values:?}: {err}"));
        let options = decode_options(agent, &validated)
            .unwrap_or_else(|err| panic!("{case}: {validated:?} does not decode: {err}"));
        let reserialized: OptionValues = serde_json::to_value(&options)
            .and_then(serde_json::from_value)
            .unwrap_or_else(|err| panic!("{case}: the options do not serialize to values: {err}"));
        if let Err(err) = validate(descriptor, &reserialized) {
            panic!("{case}: the schema refuses the re-serialized {reserialized:?}: {err}");
        }
        check_shell_resume(agent, &case, &options);
    }
    check_screen_rules(agent);
}

/// Every screen rule a plugin hands the daemon is named, ordered and readable:
/// a non-empty unique name, priorities in non-increasing order, and no
/// placeholder left in the parts the daemon evaluates.
#[expect(
    clippy::panic,
    reason = "a conformance failure must fail the calling test"
)]
fn check_screen_rules<A: Agent>(agent: &A) {
    let mut names: Vec<&str> = Vec::new();
    let mut last_priority: Option<u16> = None;
    for (index, rule) in agent.screen_rules().rules.iter().enumerate() {
        assert!(!rule.name.is_empty(), "screen rule {index} has no name");
        assert!(
            !names.contains(&rule.name.as_str()),
            "screen rule {:?} is named twice",
            rule.name
        );
        names.push(&rule.name);
        if let Some(previous) = last_priority
            && rule.priority > previous
        {
            panic!(
                "screen rule {:?} (priority {}) follows priority {previous}; priorities must not rise",
                rule.name, rule.priority
            );
        }
        last_priority = Some(rule.priority);
        let unknown_matcher = matches!(rule.matcher, LineMatcher::Unknown)
            || rule.not.iter().any(|m| matches!(m, LineMatcher::Unknown));
        assert!(
            !unknown_matcher,
            "screen rule {:?} matches an unknown kind",
            rule.name
        );
        assert!(
            !matches!(rule.region, ScreenRegion::Unknown),
            "screen rule {:?} reads an unknown region",
            rule.name
        );
        assert!(
            !matches!(rule.verdict, ScreenVerdict::Unknown),
            "screen rule {:?} gives an unknown verdict",
            rule.name
        );
    }
}

/// A shell resume command, when the agent answers one, resumes the
/// conversation it was asked for.
#[expect(
    clippy::panic,
    reason = "a conformance failure must fail the calling test"
)]
fn check_shell_resume<A: Agent>(agent: &A, case: &str, options: &A::Options) {
    const ID: &str = "conformance-conversation";
    if let Ok(Some(command)) = agent.shell_resume(options, ID, &[])
        && !command.contains(ID)
    {
        panic!("{case}: the shell resume command {command:?} does not name the conversation");
    }
}

/// Every case [`check`] runs, each named for its failure message.
fn cases(descriptor: &AgentDescriptor) -> Vec<(String, OptionValues)> {
    let mut cases = vec![("the empty map".to_string(), OptionValues::new())];
    for field in &descriptor.fields {
        cases.push((
            format!("`{}`'s default", field.id),
            single(&field.id, field.default.clone()),
        ));
        if let FieldKind::Choice { choices, .. } = &field.kind {
            for choice in choices {
                cases.push((
                    format!("`{}` = \"{}\"", field.id, choice.value),
                    single(&field.id, OptionValue::Text(choice.value.clone())),
                ));
            }
        }
        for rule in &field.when {
            cases.push((
                format!("`{}`'s rule on `{}`", field.id, rule.if_field),
                single(&rule.if_field, rule.equals.clone()),
            ));
        }
    }
    cases
}

/// A map holding `value` at `key` alone.
fn single(key: &str, value: OptionValue) -> OptionValues {
    OptionValues::from([(key.to_string(), value)])
}
