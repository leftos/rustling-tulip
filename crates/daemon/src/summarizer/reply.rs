//! A provider's summary reply: the JSON object it is read from (inside a code
//! fence or after prose, as models often send it), the `kind` check, and the
//! caps on the two text fields.

use protocol::AlertKind;
use serde::Deserialize;

/// The most words a headline keeps.
pub(crate) const HEADLINE_MAX_WORDS: usize = 8;

/// The most characters a notification keeps.
pub(crate) const NOTIFICATION_MAX_CHARS: usize = 100;

/// The JSON Schema of the reply object, which a CLI summarizer is asked to
/// answer in: the three fields [`parse`] reads, each a string, `kind` one of
/// the three it accepts.
pub(crate) const SUMMARY_SCHEMA: &str = r#"{"type":"object","properties":{"kind":{"type":"string","enum":["needs_answer","working_update","done_waiting"]},"headline":{"type":"string"},"notification":{"type":"string"}},"required":["kind","headline","notification"],"additionalProperties":false}"#;

/// One summary, read and capped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Summary {
    /// What the session needs; never [`AlertKind::Unknown`].
    pub(crate) kind: AlertKind,
    /// At most [`HEADLINE_MAX_WORDS`] words.
    pub(crate) headline: String,
    /// At most [`NOTIFICATION_MAX_CHARS`] characters.
    pub(crate) notification: String,
}

/// Why a reply's text gave no summary.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ReplyError {
    /// No JSON object with `kind`, `headline` and `notification` strings.
    #[error("the reply holds no JSON object with kind, headline and notification")]
    NoObject,
    /// The object's `kind` is not one of the three the summarizer asks for.
    #[error("the reply's kind is not needs_answer, working_update or done_waiting")]
    UnknownKind,
}

#[derive(Deserialize)]
struct WireSummary {
    kind: AlertKind,
    headline: String,
    notification: String,
}

/// Read the first JSON object in `text` that has the three fields, refuse an
/// unknown `kind`, and cut each text field to its cap at a word boundary.
pub(crate) fn parse(text: &str) -> Result<Summary, ReplyError> {
    let wire = first_object(text).ok_or(ReplyError::NoObject)?;
    if wire.kind == AlertKind::Unknown {
        return Err(ReplyError::UnknownKind);
    }
    Ok(Summary {
        kind: wire.kind,
        headline: cut_words(&wire.headline, HEADLINE_MAX_WORDS),
        notification: cut_chars(&wire.notification, NOTIFICATION_MAX_CHARS),
    })
}

/// The first object in `text` with the three fields, trying each `{` in turn,
/// so a fence, prose or a stray brace before it does not matter.
fn first_object(text: &str) -> Option<WireSummary> {
    text.match_indices('{').find_map(|(start, _)| {
        let rest = text.get(start..)?;
        serde_json::Deserializer::from_str(rest)
            .into_iter::<WireSummary>()
            .next()?
            .ok()
    })
}

/// The first `max` words of `text`, joined by single spaces.
fn cut_words(text: &str, max: usize) -> String {
    text.split_whitespace()
        .take(max)
        .collect::<Vec<_>>()
        .join(" ")
}

/// `text` trimmed and cut to at most `max` characters, at the last word
/// boundary that fits; a single word longer than `max` is cut at `max`.
fn cut_chars(text: &str, max: usize) -> String {
    let text = text.trim();
    let Some((end, next)) = text.char_indices().nth(max) else {
        return text.to_owned();
    };
    let head = text.get(..end).unwrap_or(text);
    if next.is_whitespace() {
        return head.trim_end().to_owned();
    }
    match head.rfind(char::is_whitespace) {
        Some(space) => head.get(..space).unwrap_or(head).trim_end().to_owned(),
        None => head.to_owned(),
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests fail loudly on setup errors")]
mod tests {
    use super::*;

    const OBJECT: &str = r#"{"kind": "needs_answer", "headline": "Pick a branch name", "notification": "The agent asks which branch to use."}"#;

    #[test]
    fn a_bare_object_parses() {
        let summary = parse(OBJECT).expect("a bare object parses");
        assert_eq!(summary.kind, AlertKind::NeedsAnswer);
        assert_eq!(summary.headline, "Pick a branch name");
        assert_eq!(summary.notification, "The agent asks which branch to use.");
    }

    #[test]
    fn fenced_json_parses() {
        let text = format!("```json\n{OBJECT}\n```");
        let summary = parse(&text).expect("a fenced object parses");
        assert_eq!(summary.kind, AlertKind::NeedsAnswer);
        assert_eq!(summary.headline, "Pick a branch name");
    }

    #[test]
    fn prose_before_a_fenced_object_parses() {
        let text = format!("Here is the summary {{as asked}}:\n\n```\n{OBJECT}\n```\nDone.");
        let summary = parse(&text).expect("the object after the prose parses");
        assert_eq!(summary.notification, "The agent asks which branch to use.");
    }

    #[test]
    fn each_known_kind_parses() {
        for (wire, kind) in [
            ("needs_answer", AlertKind::NeedsAnswer),
            ("working_update", AlertKind::WorkingUpdate),
            ("done_waiting", AlertKind::DoneWaiting),
        ] {
            let text = format!(r#"{{"kind": "{wire}", "headline": "h", "notification": "n"}}"#);
            assert_eq!(parse(&text).expect("a known kind parses").kind, kind);
        }
    }

    #[test]
    fn the_schema_requires_the_three_strings_and_lists_each_kind_parse_accepts() {
        let schema: serde_json::Value =
            serde_json::from_str(SUMMARY_SCHEMA).expect("the schema is JSON");
        assert_eq!(schema["type"], "object");
        assert_eq!(
            schema["required"],
            serde_json::json!(["kind", "headline", "notification"])
        );
        for field in ["kind", "headline", "notification"] {
            assert_eq!(schema["properties"][field]["type"], "string", "{field}");
        }
        let kinds = schema["properties"]["kind"]["enum"]
            .as_array()
            .expect("kind lists its values");
        assert_eq!(kinds.len(), 3);
        for kind in kinds {
            let kind = kind.as_str().expect("a kind is a string");
            let text = format!(r#"{{"kind": "{kind}", "headline": "h", "notification": "n"}}"#);
            assert!(
                parse(&text).is_ok(),
                "the schema's kind {kind} does not parse"
            );
        }
    }

    #[test]
    fn an_unknown_kind_is_a_failure() {
        let text = r#"{"kind": "celebrate", "headline": "h", "notification": "n"}"#;
        assert_eq!(parse(text), Err(ReplyError::UnknownKind));
    }

    #[test]
    fn text_without_the_object_is_a_failure() {
        assert_eq!(parse("no JSON here"), Err(ReplyError::NoObject));
        assert_eq!(parse(""), Err(ReplyError::NoObject));
        assert_eq!(
            parse(r#"{"kind": "done_waiting", "headline": "h"}"#),
            Err(ReplyError::NoObject)
        );
        assert_eq!(
            parse(r#"{"kind": "done_waiting", "headl"#),
            Err(ReplyError::NoObject)
        );
    }

    #[test]
    fn a_headline_over_eight_words_is_cut_at_a_word_boundary() {
        let text = r#"{"kind": "working_update", "headline": "one two three four five six seven eight nine ten", "notification": "n"}"#;
        let summary = parse(text).expect("parses");
        assert_eq!(summary.headline, "one two three four five six seven eight");
    }

    #[test]
    fn a_headline_of_eight_words_is_kept() {
        let text = r#"{"kind": "working_update", "headline": "one two three four five six seven eight", "notification": "n"}"#;
        let summary = parse(text).expect("parses");
        assert_eq!(summary.headline, "one two three four five six seven eight");
    }

    #[test]
    fn a_notification_over_100_chars_is_cut_at_a_word_boundary() {
        // 19 words of five letters and a space: 114 characters.
        let long = "abcde ".repeat(19);
        let text = format!(
            r#"{{"kind": "done_waiting", "headline": "h", "notification": "{}"}}"#,
            long.trim_end()
        );
        let summary = parse(&text).expect("parses");
        assert!(summary.notification.chars().count() <= NOTIFICATION_MAX_CHARS);
        // Characters 97..=101 are the 17th word, which would not fit whole.
        assert_eq!(summary.notification, "abcde ".repeat(16).trim_end());
    }

    #[test]
    fn a_notification_ending_on_a_word_at_the_cap_keeps_that_word() {
        // 20 words of four letters and a space: the 20th word ends at char 99.
        let exact = "abcd ".repeat(20);
        let text = format!(
            r#"{{"kind": "done_waiting", "headline": "h", "notification": "{exact}tail"}}"#
        );
        let summary = parse(&text).expect("parses");
        assert_eq!(summary.notification, exact.trim_end());
    }

    #[test]
    fn a_notification_of_one_long_word_is_cut_at_the_cap() {
        let word = "é".repeat(150);
        let text =
            format!(r#"{{"kind": "done_waiting", "headline": "h", "notification": "{word}"}}"#);
        let summary = parse(&text).expect("parses");
        assert_eq!(summary.notification, "é".repeat(NOTIFICATION_MAX_CHARS));
    }

    #[test]
    fn a_notification_at_the_cap_is_kept_whole() {
        let exact = "x".repeat(NOTIFICATION_MAX_CHARS);
        let text =
            format!(r#"{{"kind": "done_waiting", "headline": "h", "notification": "{exact}"}}"#);
        assert_eq!(parse(&text).expect("parses").notification, exact);
    }
}
