//! The phone's Quick replies as plain data: the two sets an agent session
//! and a bare shell offer, their built-in defaults, and the bytes a reply
//! types. `session_view` draws them.

use serde::{Deserialize, Serialize};

/// One titled group of replies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplyGroup {
    /// The section label; unique within its set.
    pub title: String,
    /// The texts sent, each typed then Return; single-line, not blank.
    #[serde(default)]
    pub replies: Vec<String>,
}

/// Which of the two reply sets: the one an agent session offers, or a bare
/// shell's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplySet {
    /// The replies a session with an agent offers.
    Agent,
    /// The replies a session with no agent offers.
    Shell,
}

impl ReplySet {
    /// The set a session with an agent or without one offers.
    #[must_use]
    pub(crate) fn of(has_agent: bool) -> Self {
        if has_agent { Self::Agent } else { Self::Shell }
    }
}

/// A set's groups: the saved ones when the user has any, else the defaults.
/// An empty saved list stays empty.
#[must_use]
pub(crate) fn groups(saved: Option<&[ReplyGroup]>, set: ReplySet) -> Vec<ReplyGroup> {
    let Some(saved) = saved else {
        return defaults(set);
    };
    saved.to_vec()
}

/// The replies a set offers before the user edits them. The agent set is
/// the one a session with an agent has always had: every harness reads
/// "yes" and "continue" the same way.
#[must_use]
pub(crate) fn defaults(set: ReplySet) -> Vec<ReplyGroup> {
    match set {
        ReplySet::Agent => vec![
            group("confirm", &["yes", "no"]),
            group(
                "common",
                &[
                    "continue",
                    "commit and push",
                    "retry",
                    "skip",
                    "drastically simplify",
                ],
            ),
        ],
        ReplySet::Shell => vec![group("confirm", &["y", "n"])],
    }
}

/// A group of the given title, offering `replies`.
fn group(title: &str, replies: &[&str]) -> ReplyGroup {
    ReplyGroup {
        title: title.to_owned(),
        replies: replies.iter().map(|reply| (*reply).to_owned()).collect(),
    }
}

/// The bytes a quick reply types: its text, then `\r`, which submits it.
#[must_use]
pub(crate) fn quick_bytes(text: &str) -> Vec<u8> {
    let mut bytes = text.as_bytes().to_vec();
    bytes.push(b'\r');
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A group as this module's tests write one.
    fn group(title: &str, replies: &[&str]) -> ReplyGroup {
        ReplyGroup {
            title: title.to_owned(),
            replies: replies.iter().map(|reply| (*reply).to_owned()).collect(),
        }
    }

    #[test]
    fn the_set_follows_whether_the_session_has_an_agent() {
        assert_eq!(ReplySet::of(true), ReplySet::Agent);
        assert_eq!(ReplySet::of(false), ReplySet::Shell);
    }

    #[test]
    fn agent_defaults_are_todays_groups() {
        assert_eq!(
            defaults(ReplySet::Agent),
            vec![
                group("confirm", &["yes", "no"]),
                group(
                    "common",
                    &[
                        "continue",
                        "commit and push",
                        "retry",
                        "skip",
                        "drastically simplify"
                    ],
                ),
            ]
        );
    }

    #[test]
    fn shell_defaults_are_y_and_n() {
        assert_eq!(
            defaults(ReplySet::Shell),
            vec![group("confirm", &["y", "n"])]
        );
    }

    #[test]
    fn an_absent_set_is_its_defaults() {
        assert_eq!(groups(None, ReplySet::Agent), defaults(ReplySet::Agent));
        assert_eq!(groups(None, ReplySet::Shell), defaults(ReplySet::Shell));
    }

    #[test]
    fn an_empty_saved_set_stays_empty() {
        assert_eq!(groups(Some(&[]), ReplySet::Shell), Vec::new());
    }

    #[test]
    fn a_saved_set_replaces_the_defaults() {
        let mine = [group("mine", &["run the tests"])];
        assert_eq!(groups(Some(&mine), ReplySet::Agent), mine.to_vec());
    }

    #[test]
    fn an_agent_session_gets_the_agent_quick_groups() {
        let shown = groups(None, ReplySet::of(true));
        let titles: Vec<&str> = shown.iter().map(|group| group.title.as_str()).collect();
        assert_eq!(titles, ["confirm", "common"]);
        assert_eq!(shown[0].replies, ["yes", "no"]);
        assert_eq!(
            shown[1].replies,
            [
                "continue",
                "commit and push",
                "retry",
                "skip",
                "drastically simplify"
            ]
        );
    }

    #[test]
    fn a_shell_session_gets_only_y_and_n() {
        let shown = groups(None, ReplySet::of(false));
        assert_eq!(shown.len(), 1, "a bare shell has no common group");
        assert_eq!(shown[0].title, "confirm");
        assert_eq!(shown[0].replies, ["y", "n"]);
    }

    #[test]
    fn a_quick_reply_types_its_text_then_return() {
        assert_eq!(quick_bytes("continue"), b"continue\r");
        assert_eq!(quick_bytes("yes"), b"yes\r");
    }

    #[test]
    fn a_quick_reply_types_utf8_then_return() {
        assert_eq!(quick_bytes("é"), [0xc3, 0xa9, 0x0d]);
    }
}
