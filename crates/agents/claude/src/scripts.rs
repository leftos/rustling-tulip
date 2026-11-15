//! Code Mode's `REPL` scripts, as one chat's transcript tells them: which
//! script each later record belongs to. A model's `REPL` call opens a script;
//! its result is either its outcome (it ran at once) or a receipt naming the
//! eval number it runs under in the background, and a later delivery (a
//! `Poll` result or a `poll_events` attachment) settles that eval with an
//! event. The calls a script ran are virtual records, tied to it only while
//! exactly one script can own them: right after one at-once result, or right
//! after a delivery that settled one eval; any model record or user prompt
//! closes that window. A `repl-error-` call is a background script's
//! failure, never a script of its own.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use crate::chat_entries::{SUMMARY_MAX_CHARS, cap_chars};

/// The id prefix of a call a script ran.
const INNER_PREFIX: &str = "repl_";
/// The id prefix of the virtual call that carries a background script's
/// failure.
pub(crate) const ERROR_PREFIX: &str = "repl-error-";
const CLOSE_TAG: &str = "</event>";

/// A `Poll` result's first line, listing the nonces of the events it
/// delivers.
static MANIFEST: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(r"^<system>authentic event nonces for this delivery: ([^—<]+?) —").ok()
});
/// A background script's receipt.
static DISPATCHED: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"^\(dispatched #([0-9]+)\)$").ok());
static OPEN_TAG: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"<event\b[^>]*>").ok());
static OPENING_TAG: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"^<event\b[^>]*>").ok());
static ATTRIBUTE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r#"([A-Za-z0-9_-]+)="([^"]*)""#).ok());

/// The scripts of one chat, kept between its transcript's lines: the
/// Claude plugin's chat context. A fresh book knows no script.
#[derive(Debug, Default)]
pub struct ScriptBook {
    scripts: HashMap<String, ScriptRecord>,
    /// Each eval number's script id.
    by_eval: HashMap<u32, String>,
    /// An inner call's or error call's id to its script's id.
    tied_to: HashMap<String, String>,
    /// The evals the latest delivery settled.
    window: Vec<u32>,
    /// The scripts whose at-once result arrived since the last model record
    /// or prompt, or since the last delivery.
    at_once: Vec<String>,
    /// The evals the record being read delivers.
    delivered: Vec<u32>,
}

#[derive(Debug, Default)]
struct ScriptRecord {
    inner: Vec<String>,
    error_call: Option<String>,
}

/// Who wrote a record: the model, Claude Code on a script's behalf (a
/// virtual `<synthetic>` record), or anyone else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Origin {
    Model,
    Synthetic,
    Other,
}

/// The script a virtual call belongs to.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Tie {
    /// A call the script ran.
    Inner(String),
    /// The call that carries the script's failure.
    Error(String),
}

/// A settled `repl-eval` event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReplEvent {
    pub(crate) eval: u32,
    pub(crate) ms: Option<u64>,
    pub(crate) ok: bool,
    /// A failure's first error line.
    pub(crate) error: Option<String>,
    /// The text of the event's `<result>` or `<error>`.
    pub(crate) text: String,
}

impl ScriptBook {
    /// A record from `origin` starts being read; a model record or a user
    /// prompt closes the tie window.
    pub(crate) fn begin_row(&mut self, origin: Origin, is_prompt: bool) {
        self.delivered.clear();
        if origin == Origin::Model || is_prompt {
            self.window.clear();
            self.at_once.clear();
        }
    }

    /// The record has been read: the evals it delivered open the window.
    pub(crate) fn end_row(&mut self) {
        if self.delivered.is_empty() {
            return;
        }
        let mut seen = HashSet::new();
        self.window = std::mem::take(&mut self.delivered)
            .into_iter()
            .filter(|eval| seen.insert(*eval))
            .collect();
        self.at_once.clear();
    }

    /// The model's `REPL` call `id` opens its script.
    pub(crate) fn open(&mut self, id: &str) {
        self.scripts.insert(id.to_owned(), ScriptRecord::default());
    }

    pub(crate) fn is_script(&self, id: &str) -> bool {
        self.scripts.contains_key(id)
    }

    /// Script `id`'s receipt: it runs in the background as `eval`.
    pub(crate) fn dispatched(&mut self, id: &str, eval: u32) {
        self.by_eval.insert(eval, id.to_owned());
    }

    /// Script `id`'s outcome arrived with its call's result.
    pub(crate) fn at_once(&mut self, id: &str) {
        self.at_once.push(id.to_owned());
    }

    /// The script whose error call `id` is.
    pub(crate) fn error_call_owner(&self, id: &str) -> Option<String> {
        let owner = self.tied_to.get(id)?;
        let record = self.scripts.get(owner)?;
        (record.error_call.as_deref() == Some(id)).then(|| owner.clone())
    }

    /// Ties a virtual call to the one script the window holds: a `repl_`
    /// call is one it ran, a `repl-error-` call its failure. None when no
    /// single script can own it, or for any other call.
    pub(crate) fn tie(&mut self, id: &str) -> Option<Tie> {
        let script = self.window_script()?.to_owned();
        let record = self.scripts.get_mut(&script)?;
        let tie = if id.starts_with(ERROR_PREFIX) {
            record.error_call = Some(id.to_owned());
            Tie::Error(script.clone())
        } else if id.starts_with(INNER_PREFIX) {
            if !record.inner.iter().any(|held| held == id) {
                record.inner.push(id.to_owned());
            }
            Tie::Inner(script.clone())
        } else {
            return None;
        };
        self.tied_to.insert(id.to_owned(), script);
        Some(tie)
    }

    /// The events a delivery carries, each with its script's id when the
    /// book knows the eval; every eval counts as delivered.
    pub(crate) fn settle(&mut self, events: Vec<ReplEvent>) -> Vec<(String, ReplEvent)> {
        let mut settled = Vec::new();
        for event in events {
            self.delivered.push(event.eval);
            if let Some(id) = self.by_eval.get(&event.eval)
                && self.scripts.contains_key(id)
            {
                settled.push((id.clone(), event));
            }
        }
        settled
    }

    /// The one script a virtual call can belong to: the one at-once result
    /// while no delivery window is open, else the one script the latest
    /// delivery settled.
    fn window_script(&self) -> Option<&str> {
        let at_once = match (self.at_once.as_slice(), self.window.is_empty()) {
            ([one], true) => Some(one.as_str()),
            _ => None,
        };
        let delivered = match self.window.as_slice() {
            [eval] => self.by_eval.get(eval).map(String::as_str),
            _ => None,
        };
        at_once
            .or(delivered)
            .filter(|id| self.scripts.contains_key(*id))
    }
}

/// The first non-empty line of `text`, trimmed.
fn first_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|line| !line.is_empty())
}

/// The first non-empty line of a call's `description`, cut to a card
/// summary's length; empty when it has none.
pub(crate) fn description_line(input: &Value) -> String {
    let line = input
        .get("description")
        .and_then(Value::as_str)
        .and_then(first_line)
        .unwrap_or("");
    cap_chars(line, SUMMARY_MAX_CHARS)
}

/// The eval number a script's result names when it is a receipt.
pub(crate) fn dispatched_eval(text: &str) -> Option<u32> {
    let captures = DISPATCHED.as_ref()?.captures(text.trim())?;
    captures.get(1)?.as_str().parse().ok()
}

/// The events a tool result delivers: none unless its first line is a
/// delivery's manifest.
pub(crate) fn poll_events(text: &str) -> Vec<ReplEvent> {
    let first = text.split('\n').next().unwrap_or("");
    let Some(listed) = MANIFEST
        .as_ref()
        .and_then(|manifest| manifest.captures(first))
        .and_then(|captures| captures.get(1))
    else {
        return Vec::new();
    };
    let nonces: HashSet<&str> = listed
        .as_str()
        .split(' ')
        .filter(|n| !n.is_empty())
        .collect();
    parse_events(text, Some(&nonces))
}

/// The event one queued envelope carries, when it is a settled repl-eval.
pub(crate) fn queued_events(text: &str) -> Vec<ReplEvent> {
    parse_events(text, None)
}

/// The repl-eval events a delivery carries, in text order. With `nonces`
/// (a `Poll` result's manifest), an element runs from an open tag naming one
/// to the last close tag before the next such tag; without, the text is one
/// element from its opening tag to its last close tag. Markup a result
/// quotes is body.
fn parse_events(text: &str, nonces: Option<&HashSet<&str>>) -> Vec<ReplEvent> {
    let elements = match nonces {
        Some(nonces) => listed_elements(text, nonces),
        None => queued_element(text),
    };
    elements
        .into_iter()
        .filter_map(|(tag, body)| event_of(tag, body, nonces))
        .collect()
}

/// An element's open tag: the tag, where it starts and where it ends.
struct Start<'a> {
    tag: &'a str,
    at: usize,
    end: usize,
}

/// An element's open tag and body: from `start` to the last close tag before
/// `stop`.
fn element_of<'a>(text: &'a str, start: &Start<'a>, stop: usize) -> Option<(&'a str, &'a str)> {
    let close = text.get(..stop)?.rfind(CLOSE_TAG)?;
    if close < start.end {
        return None;
    }
    Some((start.tag, text.get(start.end..close)?))
}

fn listed_elements<'a>(text: &'a str, nonces: &HashSet<&str>) -> Vec<(&'a str, &'a str)> {
    let Some(open) = OPEN_TAG.as_ref() else {
        return Vec::new();
    };
    let starts: Vec<Start<'_>> = open
        .find_iter(text)
        .filter(|m| !m.as_str().ends_with("/>"))
        .filter(|m| nonces.contains(attribute(m.as_str(), "nonce").unwrap_or("")))
        .map(|m| Start {
            tag: m.as_str(),
            at: m.start(),
            end: m.end(),
        })
        .collect();
    starts
        .iter()
        .enumerate()
        .filter_map(|(i, start)| {
            let stop = starts.get(i + 1).map_or(text.len(), |next| next.at);
            element_of(text, start, stop)
        })
        .collect()
}

fn queued_element(text: &str) -> Vec<(&str, &str)> {
    let trimmed = text.trim();
    let Some(open) = OPENING_TAG.as_ref().and_then(|tag| tag.find(trimmed)) else {
        return Vec::new();
    };
    let start = Start {
        tag: open.as_str(),
        at: 0,
        end: open.end(),
    };
    element_of(trimmed, &start, trimmed.len())
        .into_iter()
        .collect()
}

/// The value of an open tag's `name="value"` attribute; the last one wins.
fn attribute<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    ATTRIBUTE
        .as_ref()?
        .captures_iter(tag)
        .filter(|captures| captures.get(1).map(|key| key.as_str()) == Some(name))
        .filter_map(|captures| captures.get(2).map(|value| value.as_str()))
        .last()
}

/// A whole number's value; none for anything else.
fn whole<T: std::str::FromStr>(text: &str) -> Option<T> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// The event an element describes; none when it is no delivered repl-eval:
/// another kind, an eval that is no whole number, a nonce the manifest does
/// not list, or a body that opens with neither a result nor an error.
fn event_of(tag: &str, body: &str, nonces: Option<&HashSet<&str>>) -> Option<ReplEvent> {
    if attribute(tag, "kind") != Some("repl-eval") {
        return None;
    }
    let eval = whole(attribute(tag, "eval").unwrap_or(""))?;
    if attribute(tag, "status") != Some("settled") {
        return None;
    }
    if let Some(nonces) = nonces
        && !nonces.contains(attribute(tag, "nonce").unwrap_or(""))
    {
        return None;
    }
    let (ok, error, text) = outcome_of(body)?;
    Some(ReplEvent {
        eval,
        ms: whole(attribute(tag, "ms").unwrap_or("")),
        ok,
        error,
        text,
    })
}

/// The elements a settled event's body may open with before its outcome:
/// the script's console output and its refused calls.
const LEADING_ELEMENTS: [(&str, &str); 2] = [("<log>", "</log>"), ("<refused>", "</refused>")];

/// `body` past the `<log>` and `<refused>` elements it opens with, each
/// ending at its first close tag.
fn past_leading_elements(body: &str) -> &str {
    let mut rest = body.trim_start();
    while let Some(after) = LEADING_ELEMENTS.iter().find_map(|(open, close)| {
        let inside = rest.strip_prefix(open)?;
        inside.find(close).map(|end| &inside[end + close.len()..])
    }) {
        rest = after.trim_start();
    }
    rest
}

/// An event's outcome, read from the element its body opens with once any
/// leading `<log>` and `<refused>` elements are passed, as a script's result
/// may quote any markup: `<result>` is a success, `<error>` a failure with
/// its first line. Also the outcome's text.
fn outcome_of(body: &str) -> Option<(bool, Option<String>, String)> {
    let opening = past_leading_elements(body);
    if let Some(rest) = opening.strip_prefix("<result>") {
        return Some((true, None, inner_text(rest, "</result>")));
    }
    let rest = opening.strip_prefix("<error>")?;
    let error = first_line(rest)
        .map(|line| line.split("</error>").next().unwrap_or("").trim())
        .filter(|line| !line.is_empty())
        .map(str::to_owned);
    Some((false, error, inner_text(rest, "</error>")))
}

/// `rest` without its closing tag `close` and the space around it.
fn inner_text(rest: &str, close: &str) -> String {
    let trimmed = rest.trim_end();
    trimmed
        .strip_suffix(close)
        .unwrap_or(trimmed)
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::{ReplEvent, description_line, dispatched_eval, poll_events, queued_events};
    use serde_json::json;

    /// An event's `(ok, error line, text)`.
    type Outcome = (bool, Option<String>, String);

    /// The outcome of each event `body` settles as, for a settled event, and
    /// the events of the same event marked `late`.
    fn outcomes(body: &str) -> (Vec<Outcome>, Vec<ReplEvent>) {
        let read = |text: &str| -> Vec<Outcome> {
            queued_events(text)
                .into_iter()
                .map(|event| {
                    assert_eq!(event.ms, Some(3715), "{event:?}");
                    (event.ok, event.error, event.text)
                })
                .collect()
        };
        let settled = event(200, 3715, body, None);
        let late = settled.replace("status=\"settled\"", "status=\"late\"");
        (read(&settled), queued_events(&late))
    }

    #[test]
    fn an_event_reads_its_outcome_after_any_leading_log_and_refused_elements() {
        let log = "<log>[stderr] ⚠ 1 tool call(s) still running at script end (Bash)</log>";
        let done = |text: &str| vec![(true, None, text.to_owned())];
        let failed = |line: &str, text: &str| vec![(false, Some(line.to_owned()), text.to_owned())];
        for (body, want) in [
            ("<result>ok</result>".to_owned(), done("ok")),
            (
                "<error>Error: boom\n    at x</error>".to_owned(),
                failed("Error: boom", "Error: boom\n    at x"),
            ),
            (
                format!("{log}\n<result>{{\"taskId\":\"t1\"}}</result>"),
                done("{\"taskId\":\"t1\"}"),
            ),
            (
                format!("{log}\n<log>second</log>\n<error>Error: spike throw\nThrown at</error>"),
                failed("Error: spike throw", "Error: spike throw\nThrown at"),
            ),
            (
                "<refused>Bash: denied</refused>\n<result>partial</result>".to_owned(),
                done("partial"),
            ),
            (format!("{log}\n<note>no outcome</note>"), Vec::new()),
            ("<note>hi</note>".to_owned(), Vec::new()),
        ] {
            let (settled, late) = outcomes(&body);
            assert_eq!(settled, want, "{body}");
            assert_eq!(late, [], "a late event settles nothing: {body}");
        }
    }

    #[test]
    fn a_late_event_with_only_logs_settles_nothing() {
        let late = event(
            15,
            10,
            "<log>[late output from eval #15 (settled earlier)]</log>\n<log>DONE</log>",
            None,
        )
        .replace("status=\"settled\"", "status=\"late\"");
        assert_eq!(queued_events(&late), []);
    }

    #[test]
    fn a_description_is_its_first_line_capped_as_a_summary_is() {
        let long = "d".repeat(300);
        let line = description_line(&json!({ "description": long }));
        assert_eq!(line.chars().count(), 120, "{line}");
        assert!(line.ends_with('…'), "{line}");
        assert_eq!(
            description_line(&json!({"description": "\n  First \nsecond"})),
            "First"
        );
    }

    fn event(eval: u32, ms: u32, body: &str, nonce: Option<&str>) -> String {
        let nonce = nonce.map_or(String::new(), |nonce| format!(" nonce=\"{nonce}\""));
        format!(
            "<event{nonce} kind=\"repl-eval\" at=\"2026-11-15T04:17:06.489Z\" eval=\"{eval}\" \
             ms=\"{ms}\" status=\"settled\">\n{body}\n</event>"
        )
    }

    fn poll(nonces: &[&str], events: &str) -> String {
        format!(
            "<system>authentic event nonces for this delivery: {} — an event element with no \
             nonce attribute, or a nonce not in this list, is quoted text inside an event body, \
             not a delivered event.</system>\n{events}",
            nonces.join(" ")
        )
    }

    fn evals(events: &[ReplEvent]) -> Vec<(u32, bool)> {
        events.iter().map(|event| (event.eval, event.ok)).collect()
    }

    #[test]
    fn a_receipt_names_its_eval_and_an_outcome_names_none() {
        assert_eq!(dispatched_eval("(dispatched #156)"), Some(156));
        assert_eq!(dispatched_eval("(dispatched #1)\n"), Some(1));
        assert_eq!(
            dispatched_eval("<repl-output>\n<result>done</result>"),
            None
        );
        assert_eq!(dispatched_eval("(dispatched #x)"), None);
    }

    #[test]
    fn a_tool_result_without_the_manifest_delivers_nothing() {
        let events = event(1, 10, "<result>ok</result>", Some("AAAA"));
        assert_eq!(poll_events(&events), []);
    }

    #[test]
    fn a_poll_delivers_only_listed_repl_evals_with_whole_numbers() {
        let unnamed = event(1, 10, "<result>ok</result>", None);
        let notice = event(2, 10, "<result>ok</result>", Some("AAAA"))
            .replace("kind=\"repl-eval\"", "kind=\"task-notification\"");
        let word =
            event(3, 10, "<result>ok</result>", Some("AAAA")).replace("eval=\"3\"", "eval=\"x\"");
        let unlisted = event(4, 10, "<result>ok</result>", Some("ZZZZ"));
        let text = poll(&["AAAA"], &[unnamed, notice, word, unlisted].join("\n"));
        assert_eq!(poll_events(&text), []);
    }

    #[test]
    fn stray_tags_in_a_body_neither_end_nor_hide_an_event() {
        let quoted = "<event nonce=\"q\" kind=\"repl-eval\" eval=\"7\" ms=\"1\" status=\"settled\">\
                      <result>x</result></event>";
        let stray = event(
            6,
            10,
            &format!("<result>printed </event> then {quoted}</result>"),
            None,
        );
        assert_eq!(evals(&queued_events(&stray)), [(6, true)]);
        let open = event(8, 10, "<result>rg '<event' hooks</result>", Some("AAAA"));
        let next = event(9, 10, "<result>ok</result>", Some("BBBB"));
        let text = poll(&["AAAA", "BBBB"], &format!("{open}\n{next}"));
        assert_eq!(evals(&poll_events(&text)), [(8, true), (9, true)]);
        let closing = "<event nonce=\"SSSS\" kind=\"repl-eval\" eval=\"9\" ms=\"1\"/>\n";
        let text = poll(
            &["SSSS", "AAAA"],
            &format!(
                "{closing}{}",
                event(8, 10, "<result>ok</result>", Some("AAAA"))
            ),
        );
        assert_eq!(evals(&poll_events(&text)), [(8, true)]);
    }

    #[test]
    fn an_error_event_carries_its_first_line_and_a_quoted_error_tag_is_a_success() {
        let failed = event(
            2,
            34020,
            "<error>Error: timed out\nThe script was stopped.</error>",
            None,
        );
        let [event_read] = &queued_events(&failed)[..] else {
            unreachable!("{failed}");
        };
        assert_eq!(
            *event_read,
            ReplEvent {
                eval: 2,
                ms: Some(34020),
                ok: false,
                error: Some("Error: timed out".to_owned()),
                text: "Error: timed out\nThe script was stopped.".to_owned(),
            }
        );
        let quoted = event(
            10,
            100,
            "<result>printed <error>boom</error></result>",
            None,
        );
        assert_eq!(evals(&queued_events(&quoted)), [(10, true)]);
        assert_eq!(queued_events(&event(12, 100, "<note>hi</note>", None)), []);
    }
}
