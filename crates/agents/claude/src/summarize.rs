//! The Dashboard summarizer's call through the `claude` CLI: Haiku, headless,
//! on the user's own Claude login.
//!
//! The daemon runs the plugin's program with [`args`], the prompt on stdin,
//! in an empty folder, with [`environment`] as the child's whole environment,
//! and reads its stdout with [`parse_output`]. The environment is an
//! allow-list, so no routing variable reaches the CLI and it always uses the
//! Anthropic login.

use agent_api::{SummarizerRequest, SummarizerSpec, SummaryOutput};
use serde::Deserialize;
use serde_json::Value;

/// The model every summary runs on.
const MODEL: &str = "claude-haiku-4-5";

/// The provider picker's name for this summarizer.
const LABEL: &str = "Claude Haiku (your Claude login)";

/// The provider picker's line on where summaries go.
const PRIVACY: &str = "Summaries run through your own Claude Code login and its terms.";

/// The daemon's variables the CLI gets, each only when set: what Windows and
/// the CLI need to start, and where the login lives.
const PASSED: [&str; 9] = [
    "SystemRoot",
    "TEMP",
    "TMP",
    "USERPROFILE",
    "PATH",
    "CLAUDE_CONFIG_DIR",
    "CLAUDE_SECURESTORAGE_CONFIG_DIR",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
];

/// Set on every call: no thinking (`--effort low` alone does not stop Haiku
/// thinking) and no auto memory.
const FIXED: [(&str, &str); 2] = [
    ("MAX_THINKING_TOKENS", "0"),
    ("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1"),
];

/// How much of the CLI's own error text a failure quotes.
const ERROR_TEXT_MAX_CHARS: usize = 300;

/// The summarizer call for `request`.
pub(crate) fn spec(request: &SummarizerRequest<'_>) -> SummarizerSpec {
    SummarizerSpec {
        args: args(request.schema),
        env: environment(request.read_env),
        label: LABEL.to_owned(),
        privacy: PRIVACY.to_owned(),
        parse_output,
    }
}

/// The CLI's arguments: one headless Haiku turn with no tools, settings,
/// MCP servers, slash commands or saved session, answering in `schema`.
fn args(schema: &str) -> Vec<String> {
    [
        "-p",
        "--model",
        MODEL,
        "--effort",
        "low",
        "--output-format",
        "json",
        "--no-session-persistence",
        "--setting-sources",
        "",
        "--strict-mcp-config",
        "--tools",
        "",
        "--disable-slash-commands",
        "--json-schema",
        schema,
    ]
    .iter()
    .map(|arg| (*arg).to_owned())
    .collect()
}

/// The child's whole environment: each [`PASSED`] variable `read` answers
/// with a non-empty value, then [`FIXED`].
fn environment(read: &dyn Fn(&str) -> Option<String>) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = PASSED
        .iter()
        .filter_map(|name| {
            read(name)
                .filter(|value| !value.is_empty())
                .map(|value| ((*name).to_owned(), value))
        })
        .collect();
    env.extend(
        FIXED
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned())),
    );
    env
}

/// The CLI's `--output-format json` reply.
#[derive(Deserialize)]
struct Envelope {
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    structured_output: Option<Value>,
    #[serde(default)]
    total_cost_usd: Option<f64>,
}

/// The reply text in the CLI's stdout: `structured_output` as JSON text,
/// else `result`; and `total_cost_usd`, 0 when missing.
///
/// # Errors
///
/// Stdout is not the JSON reply, the reply says `is_error`, `result` is a
/// login error, or the reply holds neither field.
pub(crate) fn parse_output(stdout: &str) -> Result<SummaryOutput, String> {
    let envelope: Envelope = serde_json::from_str(stdout.trim())
        .map_err(|err| format!("the CLI's output is not its JSON reply: {err}"))?;
    let result = envelope.result.as_deref().unwrap_or("");
    if envelope.is_error {
        return Err(format!("the CLI reported an error: {}", excerpt(result)));
    }
    let cost_usd = envelope.total_cost_usd.unwrap_or(0.0);
    if let Some(structured) = envelope.structured_output.filter(|value| !value.is_null()) {
        return Ok(SummaryOutput {
            text: structured.to_string(),
            cost_usd,
        });
    }
    if is_login_error(result) {
        return Err(format!(
            "the CLI is not logged in: {}; run `claude` and log in",
            excerpt(result)
        ));
    }
    match envelope.result {
        Some(text) if !text.trim().is_empty() => Ok(SummaryOutput { text, cost_usd }),
        _ => Err("the CLI's reply holds no structured_output and no result".to_owned()),
    }
}

/// Whether `result` is the CLI's answer when it has no usable login.
fn is_login_error(result: &str) -> bool {
    let lower = result.trim().to_ascii_lowercase();
    lower.contains("please run /login")
        || lower.starts_with("invalid api key")
        || lower.starts_with("not logged in")
}

/// At most [`ERROR_TEXT_MAX_CHARS`] characters of `text`.
fn excerpt(text: &str) -> String {
    text.trim().chars().take(ERROR_TEXT_MAX_CHARS).collect()
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use crate::options::ClaudeProvider;
    use crate::{ClaudePlugin, providers};
    use agent_api::{Agent, Origin, ResolvedRows, Secret};

    const SCHEMA: &str =
        r#"{"type":"object","required":["kind"],"properties":{"kind":{"type":"string"}}}"#;

    /// A credential value no debug output may show.
    const TOKEN_SENTINEL: &str = "oauth-sentinel-81f3";

    fn spec_with(read_env: &dyn Fn(&str) -> Option<String>) -> SummarizerSpec {
        ClaudePlugin
            .summarizer(&SummarizerRequest {
                schema: SCHEMA,
                read_env,
            })
            .expect("Claude offers a summarizer")
    }

    #[expect(
        clippy::unnecessary_wraps,
        reason = "it stands in for the environment reader, which answers an Option"
    )]
    fn every_name_set(name: &str) -> Option<String> {
        Some(format!("value-of-{name}"))
    }

    fn names(env: &[(String, String)]) -> Vec<&str> {
        env.iter().map(|(name, _)| name.as_str()).collect()
    }

    #[test]
    fn summarizer_argv_matches_the_designed_invocation() {
        let spec = spec_with(&|_| None);
        let expected = [
            "-p",
            "--model",
            "claude-haiku-4-5",
            "--effort",
            "low",
            "--output-format",
            "json",
            "--no-session-persistence",
            "--setting-sources",
            "",
            "--strict-mcp-config",
            "--tools",
            "",
            "--disable-slash-commands",
            "--json-schema",
            SCHEMA,
        ];
        assert_eq!(spec.args, expected);
    }

    #[test]
    fn summarizer_argv_carries_the_schema_verbatim() {
        let schema = "{ \"odd\" : \"spacing\\n\" , \"x\": [1,2] }";
        let spec = ClaudePlugin
            .summarizer(&SummarizerRequest {
                schema,
                read_env: &|_| None,
            })
            .expect("Claude offers a summarizer");
        assert_eq!(spec.args.last().map(String::as_str), Some(schema));
        let flag = spec
            .args
            .iter()
            .position(|arg| arg == "--json-schema")
            .expect("the schema flag");
        assert_eq!(spec.args.get(flag + 1).map(String::as_str), Some(schema));
    }

    #[test]
    fn summarizer_env_is_exactly_the_allow_list_plus_the_two_fixed() {
        let spec = spec_with(&every_name_set);
        let mut expected: Vec<(String, String)> = PASSED
            .iter()
            .map(|name| ((*name).to_owned(), format!("value-of-{name}")))
            .collect();
        expected.push(("MAX_THINKING_TOKENS".to_owned(), "0".to_owned()));
        expected.push(("CLAUDE_CODE_DISABLE_AUTO_MEMORY".to_owned(), "1".to_owned()));
        assert_eq!(spec.env, expected);
        assert_eq!(
            PASSED,
            [
                "SystemRoot",
                "TEMP",
                "TMP",
                "USERPROFILE",
                "PATH",
                "CLAUDE_CONFIG_DIR",
                "CLAUDE_SECURESTORAGE_CONFIG_DIR",
                "CLAUDE_CODE_OAUTH_TOKEN",
                "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
            ]
        );
    }

    #[test]
    fn summarizer_env_omits_an_unset_or_empty_variable() {
        let read = |name: &str| match name {
            "TEMP" => Some(String::new()),
            "CLAUDE_CONFIG_DIR" => Some(r"D:\claude-home".to_owned()),
            _ => None,
        };
        let spec = spec_with(&read);
        assert_eq!(
            spec.env,
            [
                ("CLAUDE_CONFIG_DIR", r"D:\claude-home"),
                ("MAX_THINKING_TOKENS", "0"),
                ("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1"),
            ]
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
        );
    }

    #[test]
    fn no_routing_variable_reaches_the_summarizer_env() {
        let key = |name: &str| {
            (name == "DEEPSEEK_API_KEY")
                .then(|| (Secret::new("sk-sentinel".to_owned()), Origin::UserScope))
        };
        let plan = providers::spawn_env(ClaudeProvider::Deepseek, &ResolvedRows::default(), &key)
            .expect("the provider's plan");
        let mut routing: Vec<String> = plan.set.into_iter().map(|(name, _)| name).collect();
        routing.extend(plan.remove);
        routing.push("DEEPSEEK_API_KEY".to_owned());
        assert!(
            routing.iter().any(|name| name == "ANTHROPIC_BASE_URL"),
            "the provider table sets the endpoint: {routing:?}"
        );

        let spec = spec_with(&every_name_set);
        let passed = names(&spec.env);
        // The table also names these two, but neither routes: the first is a
        // traffic switch the provider tunes, the second is the Anthropic
        // login itself, which the provider removes.
        let not_routing = [
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
            "CLAUDE_CODE_OAUTH_TOKEN",
        ];
        let leaked: Vec<&String> = routing
            .iter()
            .filter(|name| !not_routing.contains(&name.as_str()))
            .filter(|name| passed.iter().any(|kept| kept.eq_ignore_ascii_case(name)))
            .collect();
        assert!(
            leaked.is_empty(),
            "routing variables reach the CLI: {leaked:?}"
        );
    }

    #[test]
    fn summarizer_spec_debug_shows_env_names_not_values() {
        let read =
            |name: &str| (name == "CLAUDE_CODE_OAUTH_TOKEN").then(|| TOKEN_SENTINEL.to_owned());
        let shown = format!("{:?}", spec_with(&read));
        assert!(shown.contains("CLAUDE_CODE_OAUTH_TOKEN"), "{shown}");
        assert!(!shown.contains(TOKEN_SENTINEL), "{shown}");
    }

    #[test]
    fn claude_offers_a_picker_label_and_privacy_line() {
        let spec = spec_with(&|_| None);
        assert_eq!(spec.label, "Claude Haiku (your Claude login)");
        assert_eq!(
            spec.privacy,
            "Summaries run through your own Claude Code login and its terms."
        );
    }

    #[test]
    fn a_structured_output_object_is_the_reply_text() {
        let out = parse_output(
            r#"{"type":"result","is_error":false,"structured_output":{"kind":"done_waiting","headline":"h","notification":"n"},"total_cost_usd":0.0029}"#,
        )
        .expect("parses");
        let value: Value = serde_json::from_str(&out.text).expect("the text is JSON");
        assert_eq!(value["kind"], "done_waiting");
        assert!((out.cost_usd - 0.0029).abs() < f64::EPSILON);
    }

    #[test]
    fn structured_output_wins_over_result() {
        let out = parse_output(
            r#"{"is_error":false,"result":"ignored prose","structured_output":{"kind":"needs_answer"},"total_cost_usd":0.01}"#,
        )
        .expect("parses");
        assert_eq!(out.text, r#"{"kind":"needs_answer"}"#);
    }

    #[test]
    fn a_fenced_result_is_passed_through_for_the_daemon_parser() {
        let fenced =
            "```json\n{\"kind\":\"done_waiting\",\"headline\":\"h\",\"notification\":\"n\"}\n```";
        let stdout =
            serde_json::json!({"is_error": false, "result": fenced, "total_cost_usd": 0.002})
                .to_string();
        let out = parse_output(&stdout).expect("parses");
        assert_eq!(out.text, fenced);
    }

    #[test]
    fn is_error_true_is_a_failure_even_with_a_result() {
        let err = parse_output(
            r#"{"is_error":true,"result":"API Error: 529 overloaded","structured_output":{"kind":"x"},"total_cost_usd":0.0}"#,
        )
        .expect_err("is_error fails");
        assert!(err.contains("529 overloaded"), "{err}");
    }

    #[test]
    fn a_login_error_result_is_a_failure() {
        for result in [
            "Invalid API key · Please run /login",
            "Not logged in · Please run /login",
        ] {
            let stdout = serde_json::json!({"is_error": false, "result": result}).to_string();
            let err = parse_output(&stdout).expect_err("a login error fails");
            assert!(err.contains("not logged in"), "{err}");
        }
    }

    #[test]
    fn total_cost_usd_is_read_and_a_missing_one_is_zero() {
        let out = parse_output(r#"{"result":"{}"}"#).expect("parses");
        assert!(out.cost_usd.abs() < f64::EPSILON);
        let out = parse_output(r#"{"result":"{}","total_cost_usd":1.25}"#).expect("parses");
        assert!((out.cost_usd - 1.25).abs() < f64::EPSILON);
    }

    #[test]
    fn output_that_is_not_json_is_a_failure() {
        for stdout in ["", "Error: something broke", "{\"result\": "] {
            let err = parse_output(stdout).expect_err("non-JSON fails");
            assert!(err.contains("not its JSON reply"), "{err}");
        }
    }

    #[test]
    fn an_envelope_with_neither_field_is_a_failure() {
        for stdout in [
            r#"{"is_error":false}"#,
            r#"{"result":"  ","structured_output":null}"#,
        ] {
            let err = parse_output(stdout).expect_err("an empty reply fails");
            assert!(err.contains("no structured_output"), "{err}");
        }
    }

    #[test]
    fn the_spec_parses_with_this_module() {
        let spec = spec_with(&|_| None);
        assert_eq!(
            (spec.parse_output)(r#"{"result":"r"}"#),
            parse_output(r#"{"result":"r"}"#)
        );
    }
}
