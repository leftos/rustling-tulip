//! The check every agent crate runs in one test, so an agent's option schema
//! and its typed options cannot drift apart.

use crate::{Agent, AgentPlugin, RecoveredRun, SummarizerRequest, decode_options, validate};
use protocol::{AgentDescriptor, FieldKind, OptionValue, OptionValues};

/// Check that `agent`'s option schema and its [`Agent::Options`] agree.
///
/// Each case (the empty map, every field's default, every choice of every
/// choice field, and the value each `when` rule tests for) is validated
/// against the descriptor, decoded into the agent's options, serialized back
/// into option values, and validated again; a shell resume command the
/// agent answers for those options must name the conversation it resumes.
/// A recovery with nothing recorded must succeed, and the adapter's import
/// revision must be the agent's.
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
    check_recovery_and_import(agent);
    check_summarizer(agent);
}

/// A summarizer, when the agent offers one, carries the schema it was asked
/// for, names each environment variable once, and has a picker label and a
/// privacy line; the adapter answers the agent's own spec.
#[expect(
    clippy::panic,
    reason = "a conformance failure must fail the calling test"
)]
fn check_summarizer<A: Agent>(agent: &A) {
    const SCHEMA: &str = r#"{"type":"object","title":"conformance-schema"}"#;
    let read_env = |name: &str| Some(format!("conformance-{name}"));
    let request = SummarizerRequest {
        schema: SCHEMA,
        read_env: &read_env,
    };
    let Some(spec) = Agent::summarizer(agent, &request) else {
        assert!(
            AgentPlugin::summarizer(agent, &request).is_none(),
            "the adapter offers a summarizer the agent does not"
        );
        return;
    };
    assert!(
        spec.args.iter().any(|arg| arg.contains(SCHEMA)),
        "the summarizer's arguments {:?} do not carry the schema",
        spec.args
    );
    let mut names: Vec<String> = Vec::new();
    for (name, _) in &spec.env {
        assert!(
            !name.is_empty(),
            "the summarizer's environment has an empty name"
        );
        let folded = name.to_ascii_uppercase();
        assert!(
            !names.contains(&folded),
            "the summarizer's environment names {name:?} twice"
        );
        names.push(folded);
    }
    assert!(!spec.label.is_empty(), "the summarizer has no label");
    assert!(
        !spec.privacy.is_empty(),
        "the summarizer has no privacy line"
    );
    let adapted = AgentPlugin::summarizer(agent, &request)
        .unwrap_or_else(|| panic!("the adapter offers no summarizer where the agent does"));
    assert_eq!(adapted.args, spec.args, "the adapter's arguments differ");
}

/// A recovery with nothing recorded yields option values, trusted or not,
/// and the adapter reports the agent's own import revision.
#[expect(
    clippy::panic,
    reason = "a conformance failure must fail the calling test"
)]
fn check_recovery_and_import<A: Agent>(agent: &A) {
    for trusted in [false, true] {
        let run = RecoveredRun {
            recorded: None,
            trusted,
        };
        if let Err(err) = AgentPlugin::recovery_options(agent, &run) {
            panic!("a recovery with nothing recorded (trusted {trusted}) fails: {err}");
        }
    }
    assert_eq!(
        AgentPlugin::import_rev(agent),
        Agent::import_rev(agent),
        "the adapter's import revision differs from the agent's"
    );
}

/// Every screen rule a plugin hands the daemon passes
/// [`crate::ScreenRules::validate`].
#[expect(
    clippy::panic,
    reason = "a conformance failure must fail the calling test"
)]
fn check_screen_rules<A: Agent>(agent: &A) {
    if let Err(errors) = agent.screen_rules().validate() {
        let lines: Vec<String> = errors.iter().map(ToString::to_string).collect();
        panic!("the screen rules are refused:\n{}", lines.join("\n"));
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
