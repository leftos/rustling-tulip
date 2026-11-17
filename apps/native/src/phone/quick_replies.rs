//! The phone's Quick replies as plain data: the two sets an agent session
//! and a bare shell offer, their built-in defaults, and the bytes a reply
//! types. `session_view` draws them.

use serde::{Deserialize, Serialize};

use super::reorder;
use crate::drag::Side;

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

/// A saved list as the views can draw it: titles trimmed and made unique, a
/// blank title named "Group", and a reply that is blank or holds a line break
/// dropped.
#[must_use]
pub(crate) fn normalized(groups: Vec<ReplyGroup>) -> Vec<ReplyGroup> {
    let mut taken = Vec::new();
    groups
        .into_iter()
        .map(|group| ReplyGroup {
            title: unique_title(&mut taken, group.title.trim()),
            replies: group
                .replies
                .into_iter()
                .filter(|reply| is_one_line(reply))
                .collect(),
        })
        .collect()
}

/// `title`, made unique among the titles `taken` holds by numbering it " 2",
/// " 3" and so on; a blank title is "Group".
fn unique_title(taken: &mut Vec<String>, title: &str) -> String {
    let base = if title.is_empty() { "Group" } else { title };
    let mut candidate = base.to_owned();
    let mut number = 2;
    while taken.contains(&candidate) {
        candidate = format!("{base} {number}");
        number += 1;
    }
    taken.push(candidate.clone());
    candidate
}

/// Whether `reply` can be typed as a quick reply: it is not blank and holds
/// no line break.
fn is_one_line(reply: &str) -> bool {
    !reply.trim().is_empty() && !reply.contains(['\n', '\r'])
}

/// Why a group title or a reply's text cannot be saved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ListError {
    /// The title is blank.
    BlankTitle,
    /// Another group of the set already holds the title.
    TitleTaken(String),
    /// The reply is blank.
    BlankReply,
    /// The reply holds a line break.
    MultiLineReply,
}

impl ListError {
    /// What the editor says under its field.
    #[must_use]
    pub(crate) fn message(&self) -> String {
        match self {
            Self::BlankTitle => "A group needs a title.".to_owned(),
            Self::TitleTaken(title) => format!("Another group is already called “{title}”."),
            Self::BlankReply => "A reply needs some text.".to_owned(),
            Self::MultiLineReply => "A reply is one line: it is typed, then Return.".to_owned(),
        }
    }
}

/// `typed` as the title to save: trimmed, refused while blank or while
/// another group of `groups` holds it. `editing` is the group keeping its own
/// title, if any.
pub(crate) fn check_title(
    groups: &[ReplyGroup],
    typed: &str,
    editing: Option<usize>,
) -> Result<String, ListError> {
    let title = typed.trim();
    if title.is_empty() {
        return Err(ListError::BlankTitle);
    }
    let taken = groups
        .iter()
        .enumerate()
        .any(|(index, group)| Some(index) != editing && group.title == title);
    if taken {
        return Err(ListError::TitleTaken(title.to_owned()));
    }
    Ok(title.to_owned())
}

/// `typed` as the reply to save: refused while blank or holding a line break,
/// kept as typed otherwise.
pub(crate) fn check_reply(typed: &str) -> Result<String, ListError> {
    if typed.trim().is_empty() {
        return Err(ListError::BlankReply);
    }
    if typed.contains(['\n', '\r']) {
        return Err(ListError::MultiLineReply);
    }
    Ok(typed.to_owned())
}

/// One edit to a set's groups.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReplyEdit {
    /// Appends a group with the title and no replies.
    AddGroup { title: String },
    /// Renames a group, keeping its replies.
    RenameGroup { group: usize, title: String },
    /// Drops a group with its replies.
    RemoveGroup { group: usize },
    /// Appends a reply to a group.
    AddReply { group: usize, text: String },
    /// Replaces one reply's text.
    SetReply {
        group: usize,
        reply: usize,
        text: String,
    },
    /// Drops one reply.
    RemoveReply { group: usize, reply: usize },
    /// Moves a group to `side` of the group at `to`.
    MoveGroup { from: usize, to: usize, side: Side },
    /// Moves one of a group's replies to `side` of the reply at `to`.
    MoveReply {
        group: usize,
        from: usize,
        to: usize,
        side: Side,
    },
}

/// `groups` with `edit` made; `None` while an index names nothing.
#[must_use]
pub(crate) fn apply(groups: &[ReplyGroup], edit: ReplyEdit) -> Option<Vec<ReplyGroup>> {
    let mut next = groups.to_vec();
    match edit {
        ReplyEdit::AddGroup { title } => next.push(ReplyGroup {
            title,
            replies: Vec::new(),
        }),
        ReplyEdit::RenameGroup { group, title } => next.get_mut(group)?.title = title,
        ReplyEdit::RemoveGroup { group } => {
            next.get(group)?;
            next.remove(group);
        }
        ReplyEdit::AddReply { group, text } => next.get_mut(group)?.replies.push(text),
        ReplyEdit::SetReply { group, reply, text } => {
            *next.get_mut(group)?.replies.get_mut(reply)? = text;
        }
        ReplyEdit::RemoveReply { group, reply } => {
            let entry = next.get_mut(group)?;
            entry.replies.get(reply)?;
            entry.replies.remove(reply);
        }
        ReplyEdit::MoveGroup { from, to, side } => {
            next.get(from)?;
            next.get(to)?;
            next = reorder::reordered(&next, from, to, side);
        }
        ReplyEdit::MoveReply {
            group,
            from,
            to,
            side,
        } => {
            let replies = &mut next.get_mut(group)?.replies;
            replies.get(from)?;
            replies.get(to)?;
            let moved = reorder::reordered(replies, from, to, side);
            *replies = moved;
        }
    }
    Some(next)
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test's fixture groups always apply")]
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

    #[test]
    fn normalized_trims_titles() {
        let loaded = normalized(vec![group(" confirm ", &["yes"])]);
        assert_eq!(loaded, vec![group("confirm", &["yes"])]);
    }

    #[test]
    fn normalized_names_a_blank_title_group() {
        let loaded = normalized(vec![group("", &["a"])]);
        assert_eq!(loaded, vec![group("Group", &["a"])]);
    }

    #[test]
    fn normalized_numbers_repeated_titles() {
        let loaded = normalized(vec![
            group("a", &["x"]),
            group("a", &["y"]),
            group("a", &["z"]),
        ]);
        let titles: Vec<&str> = loaded.iter().map(|group| group.title.as_str()).collect();
        assert_eq!(titles, ["a", "a 2", "a 3"]);
    }

    #[test]
    fn normalized_numbering_skips_a_taken_title() {
        let loaded = normalized(vec![
            group("a", &["x"]),
            group("a 2", &["y"]),
            group("a", &["z"]),
        ]);
        let titles: Vec<&str> = loaded.iter().map(|group| group.title.as_str()).collect();
        assert_eq!(titles, ["a", "a 2", "a 3"]);
    }

    #[test]
    fn normalized_drops_blank_replies() {
        let loaded = normalized(vec![group("a", &["", "  ", "ok"])]);
        assert_eq!(loaded, vec![group("a", &["ok"])]);
    }

    #[test]
    fn normalized_drops_multi_line_replies() {
        let loaded = normalized(vec![group("a", &["one\ntwo", "x\ry", "ok"])]);
        assert_eq!(loaded, vec![group("a", &["ok"])]);
    }

    #[test]
    fn normalized_keeps_the_defaults() {
        for set in [ReplySet::Agent, ReplySet::Shell] {
            assert_eq!(normalized(defaults(set)), defaults(set));
        }
    }

    #[test]
    fn check_title_trims_a_new_title() {
        let groups = defaults(ReplySet::Agent);
        assert_eq!(check_title(&groups, "  mine ", None), Ok("mine".to_owned()));
    }

    #[test]
    fn check_title_refuses_a_blank_title() {
        let groups = defaults(ReplySet::Agent);
        assert_eq!(check_title(&groups, "", None), Err(ListError::BlankTitle));
        assert_eq!(
            check_title(&groups, "  ", Some(0)),
            Err(ListError::BlankTitle)
        );
    }

    #[test]
    fn check_title_refuses_a_taken_title() {
        let groups = defaults(ReplySet::Agent);
        assert_eq!(
            check_title(&groups, "common", None),
            Err(ListError::TitleTaken("common".to_owned()))
        );
    }

    #[test]
    fn check_title_lets_a_group_keep_its_title() {
        let groups = defaults(ReplySet::Agent);
        assert_eq!(
            check_title(&groups, "confirm", Some(0)),
            Ok("confirm".to_owned())
        );
    }

    #[test]
    fn check_title_refuses_renaming_onto_another() {
        let groups = defaults(ReplySet::Agent);
        assert_eq!(
            check_title(&groups, "common", Some(0)),
            Err(ListError::TitleTaken("common".to_owned()))
        );
    }

    #[test]
    fn check_title_compares_exactly() {
        let groups = defaults(ReplySet::Agent);
        assert_eq!(
            check_title(&groups, "Common", None),
            Ok("Common".to_owned())
        );
    }

    #[test]
    fn check_reply_takes_one_line() {
        assert_eq!(check_reply("continue"), Ok("continue".to_owned()));
    }

    #[test]
    fn check_reply_keeps_the_text_as_typed() {
        assert_eq!(
            check_reply("  keep my spaces "),
            Ok("  keep my spaces ".to_owned())
        );
    }

    #[test]
    fn check_reply_refuses_a_blank_reply() {
        assert_eq!(check_reply(""), Err(ListError::BlankReply));
        assert_eq!(check_reply("   "), Err(ListError::BlankReply));
    }

    #[test]
    fn check_reply_refuses_a_line_break() {
        assert_eq!(check_reply("a\nb"), Err(ListError::MultiLineReply));
        assert_eq!(check_reply("a\rb"), Err(ListError::MultiLineReply));
    }

    #[test]
    fn list_errors_say_what_to_fix() {
        assert_eq!(ListError::BlankTitle.message(), "A group needs a title.");
        assert_eq!(
            ListError::TitleTaken("common".to_owned()).message(),
            "Another group is already called “common”."
        );
        assert_eq!(ListError::BlankReply.message(), "A reply needs some text.");
        assert_eq!(
            ListError::MultiLineReply.message(),
            "A reply is one line: it is typed, then Return."
        );
    }

    #[test]
    fn apply_add_group_appends() {
        let next = apply(
            &defaults(ReplySet::Agent),
            ReplyEdit::AddGroup {
                title: "mine".to_owned(),
            },
        )
        .expect("the groups");

        let mut expected = defaults(ReplySet::Agent);
        expected.push(group("mine", &[]));
        assert_eq!(next, expected);
    }

    #[test]
    fn apply_rename_keeps_the_replies() {
        let next = apply(
            &defaults(ReplySet::Agent),
            ReplyEdit::RenameGroup {
                group: 1,
                title: "often".to_owned(),
            },
        )
        .expect("the groups");

        assert_eq!(
            next[1],
            group(
                "often",
                &[
                    "continue",
                    "commit and push",
                    "retry",
                    "skip",
                    "drastically simplify"
                ]
            )
        );
    }

    #[test]
    fn apply_remove_group_drops_it() {
        let next = apply(
            &defaults(ReplySet::Agent),
            ReplyEdit::RemoveGroup { group: 0 },
        )
        .expect("the groups");

        assert_eq!(
            next,
            vec![group(
                "common",
                &[
                    "continue",
                    "commit and push",
                    "retry",
                    "skip",
                    "drastically simplify"
                ]
            )]
        );
    }

    #[test]
    fn apply_add_reply_appends() {
        let next = apply(
            &defaults(ReplySet::Agent),
            ReplyEdit::AddReply {
                group: 0,
                text: "maybe".to_owned(),
            },
        )
        .expect("the groups");

        assert_eq!(next[0], group("confirm", &["yes", "no", "maybe"]));
    }

    #[test]
    fn apply_set_reply_edits_in_place() {
        let next = apply(
            &defaults(ReplySet::Agent),
            ReplyEdit::SetReply {
                group: 1,
                reply: 2,
                text: "try again".to_owned(),
            },
        )
        .expect("the groups");

        assert_eq!(next[1].replies[2], "try again");
        assert_eq!(next[1].replies.len(), 5, "the other replies stay");
    }

    #[test]
    fn apply_remove_reply_drops_it() {
        let next = apply(
            &defaults(ReplySet::Agent),
            ReplyEdit::RemoveReply { group: 1, reply: 0 },
        )
        .expect("the groups");

        assert_eq!(
            next[1],
            group(
                "common",
                &["commit and push", "retry", "skip", "drastically simplify"]
            )
        );
    }

    #[test]
    fn apply_refuses_an_index_that_names_nothing() {
        let groups = defaults(ReplySet::Agent);
        assert_eq!(apply(&groups, ReplyEdit::RemoveGroup { group: 5 }), None);
        assert_eq!(
            apply(&groups, ReplyEdit::RemoveReply { group: 1, reply: 9 }),
            None
        );
        assert_eq!(
            apply(
                &groups,
                ReplyEdit::SetReply {
                    group: 7,
                    reply: 0,
                    text: "x".to_owned(),
                }
            ),
            None
        );
    }

    #[test]
    fn apply_move_group_to_the_top() {
        let next = apply(
            &defaults(ReplySet::Agent),
            ReplyEdit::MoveGroup {
                from: 1,
                to: 0,
                side: Side::Before,
            },
        )
        .expect("the groups");

        assert_eq!(
            next,
            vec![
                group(
                    "common",
                    &[
                        "continue",
                        "commit and push",
                        "retry",
                        "skip",
                        "drastically simplify"
                    ]
                ),
                group("confirm", &["yes", "no"]),
            ]
        );
    }

    #[test]
    fn apply_move_reply_down() {
        let next = apply(
            &defaults(ReplySet::Agent),
            ReplyEdit::MoveReply {
                group: 1,
                from: 0,
                to: 2,
                side: Side::After,
            },
        )
        .expect("the groups");

        assert_eq!(
            next[1],
            group(
                "common",
                &[
                    "commit and push",
                    "retry",
                    "continue",
                    "skip",
                    "drastically simplify"
                ]
            )
        );
    }

    #[test]
    fn apply_move_reply_to_the_top() {
        let next = apply(
            &defaults(ReplySet::Agent),
            ReplyEdit::MoveReply {
                group: 1,
                from: 4,
                to: 0,
                side: Side::Before,
            },
        )
        .expect("the groups");

        assert_eq!(
            next[1].replies,
            [
                "drastically simplify",
                "continue",
                "commit and push",
                "retry",
                "skip"
            ]
        );
    }

    #[test]
    fn apply_move_onto_itself_changes_nothing() {
        let groups = defaults(ReplySet::Agent);
        let next = apply(
            &groups,
            ReplyEdit::MoveGroup {
                from: 0,
                to: 0,
                side: Side::After,
            },
        )
        .expect("the groups");

        assert_eq!(next, groups);
    }
}
