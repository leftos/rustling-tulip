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
    check_chat(agent);
    check_recovery_and_import(agent);
    check_summarizer(agent);
    check_icon(descriptor);
}

/// The largest runtime icon a descriptor may carry, in bytes.
const MAX_ICON_BYTES: usize = 8192;

/// A runtime icon, when the descriptor carries one, passes [`icon_refusal`],
/// the rules the client checks before it draws one.
#[expect(
    clippy::panic,
    reason = "a conformance failure must fail the calling test"
)]
fn check_icon(descriptor: &AgentDescriptor) {
    let Some(svg) = descriptor.icon.as_deref() else {
        return;
    };
    if let Err(refusal) = icon_refusal(svg) {
        panic!("the icon is refused: {refusal}: {svg:.40}");
    }
}

/// The elements a runtime icon may not hold, by lowercase local name.
const FORBIDDEN_ICON_ELEMENTS: [&str; 4] = ["image", "feimage", "foreignobject", "script"];

/// Why the client would refuse icon markup `svg`, or `Ok` when it draws it:
/// the client's `runtime_icon::icon_is_safe` rules. The markup is at most
/// [`MAX_ICON_BYTES`], one root `<svg …>…</svg>`, holds no `<image>`,
/// `<feImage>`, `<foreignObject>` or `<script>`, every `href` and `url(…)` names a
/// same-document `#id`, and nothing is `@import`ed; names are matched
/// without ASCII case.
fn icon_refusal(svg: &str) -> Result<(), String> {
    if svg.len() > MAX_ICON_BYTES {
        return Err(format!(
            "it is {} bytes, over the {MAX_ICON_BYTES}-byte limit",
            svg.len()
        ));
    }
    let lower = svg.trim().to_ascii_lowercase();
    if !is_one_svg(&lower) {
        return Err("it is not one <svg> element".to_owned());
    }
    if let Some(name) = tags(&lower)
        .find_map(|(_, name)| FORBIDDEN_ICON_ELEMENTS.into_iter().find(|&bad| bad == name))
    {
        return Err(format!("it has a <{name}> element"));
    }
    if !hrefs_stay_inside(&lower) {
        return Err("an href points outside the icon".to_owned());
    }
    if !urls_stay_inside(&lower) {
        return Err("a url() points outside the icon".to_owned());
    }
    if lower.contains("@import") {
        return Err("it has an @import".to_owned());
    }
    Ok(())
}

/// Every tag in lowercase markup `lower`: whether it closes, and its local
/// name (the part after any `prefix:`). Comments, declarations and
/// processing instructions are skipped.
fn tags(lower: &str) -> impl Iterator<Item = (bool, &str)> {
    lower.match_indices('<').filter_map(|(at, _)| {
        let rest = &lower[at + 1..];
        let (closing, rest) = match rest.strip_prefix('/') {
            Some(rest) => (true, rest),
            None => (false, rest),
        };
        if rest.starts_with(['!', '?']) {
            return None;
        }
        let end = rest
            .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
            .unwrap_or(rest.len());
        let name = &rest[..end];
        Some((closing, name.rsplit(':').next().unwrap_or(name)))
    })
}

/// Whether trimmed lowercase markup `lower` is one `<svg …>…</svg>` with no
/// other `svg` element, before or after it or inside it, and nothing (no
/// prolog, no doctype) before it.
fn is_one_svg(lower: &str) -> bool {
    let mut svgs = tags(lower).filter(|(_, name)| *name == "svg");
    lower.starts_with("<svg")
        && lower.ends_with("</svg>")
        && tags(lower).next() == Some((false, "svg"))
        && svgs.next() == Some((false, "svg"))
        && svgs.next() == Some((true, "svg"))
        && svgs.next().is_none()
}

/// Whether every `href` attribute (`xlink:href` and other prefixes too) in
/// lowercase markup `lower` holds a quoted same-document `#id` and nothing
/// else.
fn hrefs_stay_inside(lower: &str) -> bool {
    lower.match_indices("href").all(|(at, _)| {
        let Some(value) = lower[at + "href".len()..].trim_start().strip_prefix('=') else {
            return true;
        };
        quoted(value.trim_start()).is_some_and(is_id_reference)
    })
}

/// Whether `text` is exactly `#` and one or more id characters: ASCII
/// letters and digits, `_`, `-`, `.` and `:`. A path, a character reference
/// or a percent escape after the `#` is not an id.
fn is_id_reference(text: &str) -> bool {
    text.strip_prefix('#').is_some_and(|id| {
        !id.is_empty()
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'))
    })
}

/// The value of attribute text `text` that opens with a `"` or `'` quote, up
/// to the matching quote; `None` unquoted or unclosed.
fn quoted(text: &str) -> Option<&str> {
    let quote = text.chars().next().filter(|c| matches!(c, '"' | '\''))?;
    let body = &text[1..];
    body.find(quote).map(|end| &body[..end])
}

/// Whether every `url(…)` in lowercase markup `lower` names a same-document
/// `#id` and nothing else, quoted or not, with whitespace allowed only
/// inside the parentheses around it.
fn urls_stay_inside(lower: &str) -> bool {
    lower.match_indices("url(").all(|(at, _)| {
        let rest = &lower[at + "url(".len()..];
        rest.find(')').is_some_and(|end| {
            let inner = rest[..end].trim();
            let unquoted = match inner.chars().next() {
                Some(quote @ ('"' | '\'')) => inner[1..].strip_suffix(quote),
                _ => Some(inner),
            };
            unquoted.is_some_and(is_id_reference)
        })
    })
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

/// A plugin that reads chat screens reads a screen of 1000 empty rows
/// without panicking.
fn check_chat<A: Agent>(agent: &A) {
    if agent.read_chat_screen(&[]).is_some() {
        let rows = vec![String::new(); 1000];
        let _reading = agent.read_chat_screen(&rows);
    }
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

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test fails with the message of the precondition it lost"
)]
mod tests {
    use super::*;

    /// Asserts that every icon in `svgs` is refused with a reason naming
    /// `reason`.
    fn assert_refused(svgs: &[&str], reason: &str) {
        for svg in svgs {
            let refusal = icon_refusal(svg).expect_err(svg);
            assert!(refusal.contains(reason), "{svg}: {refusal}");
        }
    }

    #[test]
    fn an_oversized_icon_is_refused() {
        let wrap = |body: &str| format!("<svg>{body}</svg>");
        let at_limit = wrap(&" ".repeat(MAX_ICON_BYTES - wrap("").len()));
        assert_eq!(icon_refusal(&at_limit), Ok(()));
        let over = wrap(&" ".repeat(MAX_ICON_BYTES + 1 - wrap("").len()));
        assert_refused(&[&over], "byte limit");
    }

    #[test]
    fn an_icon_with_an_image_element_is_refused() {
        assert_refused(
            &[
                r##"<svg><image href="#a"/></svg>"##,
                "<svg><IMAGE/></svg>",
                "<svg><svg:image/></svg>",
            ],
            "<image>",
        );
    }

    #[test]
    fn an_icon_with_an_external_href_is_refused() {
        assert_refused(
            &[
                r#"<svg><use href="https://example.com/a.svg#a"/></svg>"#,
                "<svg><use xlink:href = 'file:///c:/secret'/></svg>",
                r#"<svg><a HREF="javascript:alert(1)"/></svg>"#,
                "<svg><use href=#a/></svg>",
            ],
            "href",
        );
    }

    #[test]
    fn an_icon_with_an_external_url_or_import_is_refused() {
        assert_refused(
            &[
                r#"<svg><rect fill="url(https://example.com/p)"/></svg>"#,
                r#"<svg><rect style="fill: URL( 'http://example.com/p' )"/></svg>"#,
            ],
            "url()",
        );
        assert_refused(&["<svg><style>@IMPORT 'a.css';</style></svg>"], "@import");
    }

    #[test]
    fn a_same_document_href_and_url_are_allowed() {
        let svg = concat!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink">"#,
            r##"<defs><linearGradient id="g"/></defs>"##,
            r##"<use href="#g"/><use xlink:href = '#g'/>"##,
            r##"<rect fill="url(#g)" style="fill: url( '#g' )"/></svg>"##,
        );
        assert_eq!(icon_refusal(svg), Ok(()));
    }

    #[test]
    fn a_script_or_foreign_object_is_refused() {
        assert_refused(
            &[
                "<svg><script>alert(1)</script></svg>",
                "<svg><SCRIPT/></svg>",
            ],
            "<script>",
        );
        assert_refused(
            &["<svg><foreignObject><p/></foreignObject></svg>"],
            "<foreignobject>",
        );
    }

    #[test]
    fn two_root_svgs_are_refused() {
        assert_refused(
            &[
                "<svg></svg><svg></svg>",
                "<svg><svg></svg></svg>",
                "<svgx></svgx>",
                r#"<?xml version="1.0"?><svg></svg>"#,
                r#"<!DOCTYPE svg [<!ENTITY x SYSTEM "file:///c:/secret">]><svg>&x;</svg>"#,
                "",
            ],
            "one <svg>",
        );
    }

    #[test]
    fn an_feimage_is_refused() {
        assert_refused(
            &[
                concat!(
                    r#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16">"#,
                    r##"<filter id="f"><feImage href="#/../../../../../Users/victim/Pictures/a.png"/></filter>"##,
                    r#"<rect width="16" height="16" filter="url(#f)"/></svg>"#,
                ),
                "<svg><FEIMAGE/></svg>",
                "<svg><svg:feImage/></svg>",
            ],
            "<feimage>",
        );
    }

    #[test]
    fn an_href_with_a_path_after_the_hash_is_refused() {
        assert_refused(
            &[
                r##"<svg><use href="#/../../a.png"/></svg>"##,
                r##"<svg><use href="#a\b"/></svg>"##,
                r##"<svg><use href="#a%2fb"/></svg>"##,
                r##"<svg><use href="#a b"/></svg>"##,
                r#"<svg><use href=" #a"/></svg>"#,
                r##"<svg><use href="#"/></svg>"##,
            ],
            "href",
        );
        assert_refused(
            &[
                r#"<svg><rect fill="url(#/../a.png)"/></svg>"#,
                r#"<svg><rect fill="url('#a/b')"/></svg>"#,
                r#"<svg><rect fill="url(#)"/></svg>"#,
            ],
            "url()",
        );
    }

    #[test]
    fn an_href_with_a_character_reference_is_refused() {
        assert_refused(
            &[
                r##"<svg><use href="#&#47;..&#47;a.png"/></svg>"##,
                r#"<svg><use href="&#35;a"/></svg>"#,
            ],
            "href",
        );
        assert_refused(&[r#"<svg><rect fill="url(#&#47;a)"/></svg>"#], "url()");
    }

    #[test]
    #[should_panic(expected = "the icon is refused")]
    fn a_descriptor_with_a_refused_icon_fails_the_check() {
        let mut descriptor = crate::test_agent::DESCRIPTOR.clone();
        descriptor.icon = Some("<svg><script/></svg>".to_owned());
        check_icon(&descriptor);
    }
}
