//! The whole text of a tool result a chat's card has to be asked for: the
//! cut marker a capped result ends with, and the per-session requests for,
//! answers to and refusals of those texts.
//!
//! It is plain data: the view sends the message a request returns and draws
//! what the store holds.

use std::collections::HashMap;

use crate::target::HostKey;

/// The first character of the line an agent's capped result ends with.
const CUT_MARKER: char = '\u{2026}';

/// Whether `text` ends with the cut marker an agent writes when it capped a
/// tool result: `… {n} more line`, `… {n} more lines`, `… {n} more
/// character` or `… {n} more characters` as its last line, `{n}` one or
/// more ASCII digits after the marker and one space. A marker that is not
/// the last line leaves `text` uncut.
#[must_use]
pub(crate) fn is_cut(text: &str) -> bool {
    let last = text.rsplit('\n').next().unwrap_or_default();
    let Some(rest) = last
        .strip_prefix(CUT_MARKER)
        .and_then(|rest| rest.strip_prefix(' '))
    else {
        return false;
    };
    let Some((count, noun)) = rest.split_once(' ') else {
        return false;
    };
    let mut chars = count.chars();
    let counted = chars.next().is_some_and(|first| first.is_ascii_digit())
        && chars.all(|ch| ch.is_ascii_digit());
    counted
        && matches!(
            noun,
            "more line" | "more lines" | "more character" | "more characters"
        )
}

/// What the chat knows of one tool result's whole text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FullResult {
    /// Asked of `host`, the answer awaited under `request_id`.
    Pending { request_id: String, host: HostKey },
    /// The daemon answered with the result's whole text, which carries its
    /// own cut marker when the daemon capped it.
    Ready { text: String },
    /// The daemon holds no whole text for the tool.
    Missing,
}

/// The whole tool results one connection asked its hosts for, by session
/// and tool id.
#[derive(Debug, Default)]
pub(crate) struct FullResults {
    results: HashMap<(String, String), FullResult>,
    /// How many requests this store sent, so each one's id is fresh.
    sent: u64,
}

/// The store's key for tool `tool`'s result in `session`.
fn key(session: &str, tool: &str) -> (String, String) {
    (session.to_owned(), tool.to_owned())
}

impl FullResults {
    /// Asks `host` for tool `tool`'s whole result in `session`: the fresh
    /// request id to send, or none while its text is here or one request for
    /// it is still awaited.
    #[must_use]
    pub(crate) fn request(&mut self, session: &str, tool: &str, host: &HostKey) -> Option<String> {
        let key = key(session, tool);
        if matches!(
            self.results.get(&key),
            Some(FullResult::Pending { .. } | FullResult::Ready { .. })
        ) {
            return None;
        }
        self.sent += 1;
        let request_id = format!("chat-tool-result-{}", self.sent);
        self.results.insert(
            key,
            FullResult::Pending {
                request_id: request_id.clone(),
                host: host.clone(),
            },
        );
        Some(request_id)
    }

    /// Takes the daemon's answer to `request_id` for tool `tool`'s result in
    /// `session` while that is the request this store waits for; `text`
    /// `None` means the transcript holds no result for it. Returns whether
    /// it took the answer.
    #[must_use]
    pub(crate) fn apply(
        &mut self,
        session: &str,
        tool: &str,
        request_id: Option<&str>,
        text: Option<String>,
    ) -> bool {
        let key = key(session, tool);
        let awaited = matches!(
            self.results.get(&key),
            Some(FullResult::Pending { request_id: waiting, .. })
                if Some(waiting.as_str()) == request_id
        );
        if !awaited {
            return false;
        }
        let result = match text {
            Some(text) => FullResult::Ready { text },
            None => FullResult::Missing,
        };
        self.results.insert(key, result);
        true
    }

    /// What the store knows of tool `tool`'s result in `session`.
    #[must_use]
    pub(crate) fn get(&self, session: &str, tool: &str) -> Option<&FullResult> {
        self.results.get(&key(session, tool))
    }

    /// Forgets every request `host` still owes, its connection gone: a new
    /// connection sends its own, and a late answer to the old one is
    /// ignored.
    pub(crate) fn forget_pending_on(&mut self, host: &HostKey) {
        self.results.retain(
            |_, result| !matches!(result, FullResult::Pending { host: sent, .. } if sent == host),
        );
    }

    /// Forgets every result of session `session_id`, whose chat closed.
    pub(crate) fn forget_session(&mut self, session_id: &str) {
        self.results.retain(|(session, _), _| session != session_id);
    }
}

#[cfg(test)]
mod tests {
    use super::{FullResult, FullResults, is_cut};
    use crate::target::HostKey;

    #[test]
    fn is_cut_is_true_for_a_cut_multi_line_text() {
        assert!(is_cut("a\nb\n\u{2026} 3 more lines"));
    }

    #[test]
    fn is_cut_is_true_for_one_more_line() {
        assert!(is_cut("x\n\u{2026} 1 more line"));
    }

    #[test]
    fn is_cut_is_true_for_more_characters() {
        assert!(is_cut("kept\n\u{2026} 12 more characters"));
    }

    #[test]
    fn is_cut_is_false_for_a_marker_that_is_not_last() {
        assert!(!is_cut("\u{2026} 3 more lines\nafter"));
    }

    #[test]
    fn is_cut_is_false_for_a_worded_count() {
        assert!(!is_cut("\u{2026} three more lines"));
    }

    #[test]
    fn is_cut_is_false_for_ascii_dots() {
        assert!(!is_cut("...3 more lines"));
    }

    #[test]
    fn is_cut_is_false_for_a_marker_without_its_noun() {
        assert!(!is_cut("\u{2026} 3 more"));
    }

    #[test]
    fn is_cut_is_false_for_an_empty_text() {
        assert!(!is_cut(""));
    }

    #[test]
    fn a_reply_for_an_older_request_is_ignored() {
        let mut results = FullResults::default();
        let request = results.request("s1", "t1", &HostKey::Local);
        assert_eq!(request.as_deref(), Some("chat-tool-result-1"));
        assert!(!results.apply(
            "s1",
            "t1",
            Some("chat-tool-result-0"),
            Some("older".to_owned())
        ));
        assert!(matches!(
            results.get("s1", "t1"),
            Some(FullResult::Pending { .. })
        ));
        assert!(results.apply("s1", "t1", request.as_deref(), Some("whole".to_owned())));
        let ready = results.get("s1", "t1");
        assert!(matches!(ready, Some(FullResult::Ready { .. })));
        if let Some(FullResult::Ready { text }) = ready {
            assert_eq!(text, "whole");
        }
    }

    #[test]
    fn a_reconnect_frees_a_waiting_request() {
        let mut results = FullResults::default();
        let host = HostKey::Local;
        let first = results.request("s1", "t1", &host);
        assert!(first.is_some(), "the first request goes");
        results.forget_pending_on(&host);
        assert_eq!(
            results.get("s1", "t1"),
            None,
            "the wait goes with its connection"
        );
        let again = results.request("s1", "t1", &host);
        assert!(again.is_some(), "a fresh request goes");
        assert_ne!(again, first, "under a request id of its own");
        assert!(
            !results.apply("s1", "t1", first.as_deref(), Some("late".to_owned())),
            "the old connection's answer is ignored"
        );
        assert!(matches!(
            results.get("s1", "t1"),
            Some(FullResult::Pending { .. })
        ));
    }
}
