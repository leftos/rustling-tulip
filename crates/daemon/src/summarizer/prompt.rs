//! The summarizer's prompt: the template every provider is sent, and the
//! shaping of the agent's text that goes into it (joined, redacted, capped).

use std::sync::LazyLock;

use regex::Regex;

/// The prompt template, with `{{TRIGGER}}` and `{{MESSAGE}}` placeholders.
const TEMPLATE: &str = include_str!("prompt.md");

/// The most characters of the text kept from its start when it is capped.
const HEAD_CHARS: usize = 1_000;

/// The most characters of the text kept from its end when it is capped.
const TAIL_CHARS: usize = 5_000;

/// What stands between the head and the tail of a capped text.
const ELISION: &str = "\n[…]\n";

/// What stands between two reports joined into one text.
const REPORT_SEPARATOR: &str = "\n\n";

/// What a secret-shaped string becomes.
const REDACTED: &str = "[redacted]";

/// Secret shapes, applied in order: PEM blocks (a block with no end runs to
/// the end of the text), then API keys and tokens.
const SECRET_PATTERNS: &[&str] = &[
    r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----",
    r"(?s)-----BEGIN [A-Z ]+-----.*?-----END [A-Z ]+-----",
    r"(?s)-----BEGIN [A-Z ]+-----.*",
    r"\bsk-[A-Za-z0-9_\-]{16,}",
    r"\bghp_[A-Za-z0-9]{20,}",
    r"\bgithub_pat_[A-Za-z0-9_]{20,}",
    r"\bxox[bp]-[A-Za-z0-9\-]{10,}",
    r"\bAKIA[0-9A-Z]{16}\b",
];

/// [`SECRET_PATTERNS`] compiled; a pattern that fails to compile is logged
/// and left out, which a test catches.
static PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    SECRET_PATTERNS
        .iter()
        .filter_map(|pattern| match Regex::new(pattern) {
            Ok(regex) => Some(regex),
            Err(error) => {
                tracing::error!(pattern, %error, "a summarizer secret pattern does not compile");
                None
            }
        })
        .collect()
});

/// What started a summary call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Trigger {
    /// The agent ended its turn; the text is its reports since the last call.
    Stop,
    /// The agent is still in its turn; the text is the last it wrote.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the Dashboard's working refresh submits it")
    )]
    Working,
}

impl Trigger {
    /// The value the prompt's `Trigger:` line carries.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Working => "working",
        }
    }
}

/// The text sent for `reports`, given oldest first: joined, with every
/// secret-shaped string redacted, then capped. A working refresh passes its
/// one text as the only report.
pub(crate) fn shape(reports: &[&str]) -> String {
    cap(&redact(&reports.join(REPORT_SEPARATOR)))
}

/// The whole prompt for a `trigger` over `input`, the text [`shape`] gave.
/// The trigger goes in first, so placeholder text inside `input` stays as
/// written.
pub(crate) fn build(trigger: Trigger, input: &str) -> String {
    TEMPLATE
        .replace("{{TRIGGER}}", trigger.as_str())
        .replace("{{MESSAGE}}", input)
}

/// `text` with every match of [`SECRET_PATTERNS`] replaced by [`REDACTED`].
fn redact(text: &str) -> String {
    PATTERNS.iter().fold(text.to_owned(), |text, pattern| {
        pattern.replace_all(&text, REDACTED).into_owned()
    })
}

/// `text` whole when it has at most [`HEAD_CHARS`] + [`TAIL_CHARS`]
/// characters, else its first and last characters with [`ELISION`] between.
fn cap(text: &str) -> String {
    let count = text.chars().count();
    if count <= HEAD_CHARS + TAIL_CHARS {
        return text.to_owned();
    }
    let head: String = text.chars().take(HEAD_CHARS).collect();
    let tail: String = text.chars().skip(count - TAIL_CHARS).collect();
    format!("{head}{ELISION}{tail}")
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests fail loudly on setup errors")]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::summarizer::reply::SUMMARY_SCHEMA;

    const ELIDED: &str = "\n[…]\n";

    fn schema() -> serde_json::Value {
        serde_json::from_str(SUMMARY_SCHEMA).expect("the schema is JSON")
    }

    /// The reply shape the template spells out: its one line that is a JSON object.
    fn template_reply_shape() -> serde_json::Value {
        let line = TEMPLATE
            .lines()
            .find(|line| line.trim_start().starts_with('{'))
            .expect("the template shows the reply object on a line of its own");
        serde_json::from_str(line).expect("the template's reply line is JSON")
    }

    #[test]
    fn the_template_names_exactly_the_schema_fields_and_no_speech() {
        let schema = schema();
        let schema_fields: BTreeSet<&str> = schema["properties"]
            .as_object()
            .expect("the schema has properties")
            .keys()
            .map(String::as_str)
            .collect();
        let shape = template_reply_shape();
        let template_fields: BTreeSet<&str> = shape
            .as_object()
            .expect("the reply line is an object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(template_fields, schema_fields);
        assert!(!TEMPLATE.contains("speech"));
    }

    #[test]
    fn the_template_lists_each_kind_the_schema_accepts() {
        let schema = schema();
        let kinds = schema["properties"]["kind"]["enum"]
            .as_array()
            .expect("kind lists its values");
        for kind in kinds {
            let kind = kind.as_str().expect("a kind is a string");
            assert!(TEMPLATE.contains(&format!("\"{kind}\"")), "{kind}");
        }
    }

    #[test]
    fn the_template_has_no_session_line_and_no_agent_names() {
        assert!(
            !TEMPLATE
                .lines()
                .any(|line| line.trim_start().starts_with("Session:"))
        );
        let names =
            regex::Regex::new("(?i)claude|anthropic|deepseek").expect("the pattern compiles");
        assert!(!names.is_match(TEMPLATE));
    }

    #[test]
    fn the_template_asks_to_spell_out_or_drop_identifiers() {
        for words in [
            "Spell out or drop",
            "plan and issue numbers",
            "function names",
            "paths",
        ] {
            assert!(TEMPLATE.contains(words), "{words}");
        }
    }

    #[test]
    fn a_built_prompt_has_the_trigger_line_and_no_placeholders() {
        for (trigger, line) in [
            (Trigger::Stop, "Trigger: stop"),
            (Trigger::Working, "Trigger: working"),
        ] {
            let prompt = build(trigger, "The tests pass.");
            assert!(prompt.lines().any(|l| l == line), "{line}");
            assert!(!prompt.contains("{{"), "{line}");
            assert!(prompt.contains("The tests pass."), "{line}");
        }
    }

    #[test]
    fn a_report_holding_a_placeholder_is_not_substituted() {
        let input = shape(&["Wrote {{TRIGGER}} and {{MESSAGE}} into the doc."]);
        let prompt = build(Trigger::Stop, &input);
        assert!(prompt.contains("Wrote {{TRIGGER}} and {{MESSAGE}} into the doc."));
        assert_eq!(
            prompt
                .lines()
                .filter(|line| *line == "Trigger: stop")
                .count(),
            1
        );
        assert!(prompt.lines().all(|line| line != "Trigger: {{TRIGGER}}"));
    }

    #[test]
    fn short_text_is_untouched() {
        assert_eq!(shape(&["All done."]), "All done.");
    }

    #[test]
    fn text_at_6000_chars_is_kept_whole() {
        let text = "x".repeat(6_000);
        assert_eq!(shape(&[&text]), text);
    }

    #[test]
    fn text_over_6000_chars_keeps_the_first_1000_and_last_5000_with_an_elision() {
        let text = format!(
            "{}{}{}",
            "h".repeat(1_000),
            "m".repeat(500),
            "t".repeat(5_000)
        );
        assert_eq!(
            shape(&[&text]),
            format!("{}{ELIDED}{}", "h".repeat(1_000), "t".repeat(5_000))
        );
    }

    #[test]
    fn the_cap_counts_chars_not_bytes() {
        let whole = "é".repeat(6_000);
        assert_eq!(shape(&[&whole]), whole);
        let long = "é".repeat(7_000);
        assert_eq!(
            shape(&[&long]),
            format!("{}{ELIDED}{}", "é".repeat(1_000), "é".repeat(5_000))
        );
    }

    #[test]
    fn an_sk_key_is_redacted() {
        assert_eq!(
            shape(&["key sk-abcdefghijklmnop1234 here"]),
            "key [redacted] here"
        );
        assert_eq!(
            shape(&["key sk-proj-ABCD_efgh-1234ijkl here"]),
            "key [redacted] here"
        );
    }

    #[test]
    fn a_ghp_token_is_redacted() {
        assert_eq!(shape(&["ghp_abcdefghijklmnopqrstuvwxyz0123"]), "[redacted]");
    }

    #[test]
    fn a_github_pat_is_redacted() {
        assert_eq!(
            shape(&["token github_pat_11ABCDEFG_abcdefghijklmnop."]),
            "token [redacted]."
        );
    }

    #[test]
    fn an_xoxb_and_xoxp_token_is_redacted() {
        assert_eq!(shape(&["xoxb-1234567890-abcdef"]), "[redacted]");
        assert_eq!(shape(&["xoxp-1234567890-abcdef"]), "[redacted]");
    }

    #[test]
    fn an_akia_key_is_redacted() {
        assert_eq!(shape(&["id AKIAIOSFODNN7EXAMPLE set"]), "id [redacted] set");
    }

    #[test]
    fn a_pem_block_is_redacted_across_lines() {
        let text = "before\n-----BEGIN CERTIFICATE-----\nMIIBszCCAVmgAwIBAgIU\nabc\n-----END CERTIFICATE-----\nafter";
        assert_eq!(shape(&[text]), "before\n[redacted]\nafter");
    }

    #[test]
    fn a_private_key_block_is_redacted() {
        // Split so the repo's private-key hook does not take the fixture for a real key.
        let text = concat!(
            "-----BEGIN RSA PRIVATE ",
            "KEY-----\nMIIEpAIBAAKCAQEA\n-----END RSA PRIVATE ",
            "KEY----- kept"
        );
        assert_eq!(shape(&[text]), "[redacted] kept");
    }

    #[test]
    fn an_unterminated_pem_block_is_redacted_to_the_end() {
        let text = concat!(
            "the key:\n-----BEGIN OPENSSH PRIVATE ",
            "KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\nmore"
        );
        assert_eq!(shape(&[text]), "the key:\n[redacted]");
    }

    #[test]
    fn ordinary_text_with_sk_in_a_word_is_kept() {
        for text in [
            "the task-force-abcdefghijklmnop met",
            "a risk-averse-abcdefghijklmnop plan",
            "short sk-abc123 stays",
            "ghp_short stays",
            "xoxa-1234567890-abcdef stays",
        ] {
            assert_eq!(shape(&[text]), text);
        }
    }

    #[test]
    fn a_short_akia_look_alike_is_kept() {
        for text in ["AKIA1234 stays", "AKIAIOSFODNN7EXAMPLEX stays"] {
            assert_eq!(shape(&[text]), text);
        }
    }

    #[test]
    fn redaction_runs_before_the_cap() {
        let key = format!("sk-{}", "a".repeat(30));
        // The key starts at char 991, so a cap first would keep "sk-" and six
        // of its characters in the head, too short a piece to match.
        let text = format!("{} {key} {}", "x".repeat(990), "y".repeat(6_000));
        let shaped = shape(&[&text]);
        assert!(!shaped.contains("sk-"), "a key cut by the cap survived");
        assert!(!shaped.contains("aaaa"), "a key cut by the cap survived");
    }

    #[test]
    fn the_pattern_table_compiles_every_pattern() {
        assert_eq!(PATTERNS.len(), SECRET_PATTERNS.len());
    }

    #[test]
    fn reports_are_joined_oldest_first() {
        assert_eq!(
            shape(&["First report.", "Second report.", "Third report."]),
            "First report.\n\nSecond report.\n\nThird report."
        );
    }

    #[test]
    fn one_report_has_no_separator() {
        assert_eq!(shape(&["Only report."]), "Only report.");
    }

    #[test]
    fn several_reports_are_capped_after_joining() {
        let old = "o".repeat(4_000);
        let new = "n".repeat(4_000);
        let shaped = shape(&[&old, &new]);
        assert!(shaped.starts_with(&"o".repeat(1_000)));
        assert!(shaped.ends_with(&"n".repeat(4_000)));
        assert_eq!(shaped.chars().count(), 6_000 + ELIDED.chars().count());
    }

    #[test]
    fn the_prompt_carries_the_capped_redacted_text() {
        let text = format!(
            "{} ghp_abcdefghijklmnopqrstuvwxyz0123 {}",
            "a".repeat(10),
            "z".repeat(7_000)
        );
        let input = shape(&[&text]);
        let prompt = build(Trigger::Working, &input);
        assert!(prompt.contains(&input));
        assert!(!prompt.contains("ghp_"));
        assert!(prompt.contains(ELIDED));
    }
}
