//! The phone Chat's text selection: the selectable texts of the rows a chat
//! pane draws, in drawing order, a position inside one, the word under a
//! byte, range ordering, clamping to a shorter text, and the one function
//! that builds the copied text.

#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the phone Chat's hit test, highlight and Copy read this model"
    )
)]

use std::ops::Range;

use protocol::ChatEntry;

use crate::chat::{self, DisplayRow};
use crate::shell_lines;
use crate::viewer_md_view::MdDoc;

/// A position in the chat's selectable text: the entry it belongs to, the
/// unit's index within that entry, and a byte offset into the unit's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TextPos {
    pub(crate) entry: String,
    pub(crate) unit: u32,
    pub(crate) byte: usize,
}

/// Where a unit's text comes from, so a hit test and the painter can map it
/// back to what the chat draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnitSource {
    /// A user message's text.
    User,
    /// An answer's Markdown row `row`, part `part` (a table row's cell).
    AnswerRow { row: usize, part: usize },
    /// An answer with no parsed document: its raw text.
    AnswerPlain,
    /// A tool card's header one-liner.
    ToolSummary,
    /// A statement of an open tool card, by its `line`.
    ToolStatement { line: usize },
    /// An open tool card's input.
    ToolInput,
    /// An open tool card's result.
    ToolResult,
}

/// One selectable text of a chat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Unit {
    /// The id of the chat entry the text belongs to.
    pub(crate) entry: String,
    /// The unit's index within its entry.
    pub(crate) index: u32,
    pub(crate) source: UnitSource,
    pub(crate) text: String,
}

/// Every selectable text of a chat, in the order the chat draws them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Units {
    units: Vec<Unit>,
}

impl Units {
    /// The selectable texts of `rows`, in drawing order: a user message's
    /// text; an answer's Markdown rows (its raw text without a parsed
    /// document); a tool card's one-liner, plus its statements, input and
    /// result while the card is open; nothing for a folded run or a muted
    /// row; empty texts left out except an answer's empty code line, which
    /// a copy keeps. `doc_of` and `is_open` take an entry id.
    pub(crate) fn build<'a>(
        rows: &[DisplayRow<'a>],
        doc_of: impl Fn(&str) -> Option<&'a MdDoc>,
        is_open: impl Fn(&str) -> bool,
    ) -> Units {
        let mut units = Vec::new();
        for row in rows {
            let DisplayRow::Single(entry) = row else {
                continue;
            };
            let Some(id) = chat::entry_id(entry) else {
                continue;
            };
            let parts = entry_parts(entry, id, &doc_of, &is_open);
            for (index, (source, text)) in parts
                .into_iter()
                .filter(|(source, text)| {
                    !text.is_empty() || matches!(source, UnitSource::AnswerRow { .. })
                })
                .enumerate()
            {
                units.push(Unit {
                    entry: id.to_owned(),
                    index: u32::try_from(index).unwrap_or(u32::MAX),
                    source,
                    text,
                });
            }
        }
        Units { units }
    }

    /// The index of `pos` in the units' order, when a unit has it.
    fn index_of(&self, pos: &TextPos) -> Option<usize> {
        self.units
            .iter()
            .position(|unit| unit.entry == pos.entry && unit.index == pos.unit)
    }

    /// `pos` moved into the units the chat now holds: `None` when its entry
    /// has no units any more, a unit index past the entry's last unit moved
    /// to that unit's end, and a byte past the text's end or inside a
    /// character moved to the text's end or the character's start.
    pub(crate) fn clamp(&self, pos: &TextPos) -> Option<TextPos> {
        if let Some(unit) = self
            .units
            .iter()
            .find(|unit| unit.entry == pos.entry && unit.index == pos.unit)
        {
            return Some(TextPos {
                entry: pos.entry.clone(),
                unit: pos.unit,
                byte: char_floor(&unit.text, pos.byte),
            });
        }
        let last = self
            .units
            .iter()
            .filter(|unit| unit.entry == pos.entry)
            .max_by_key(|unit| unit.index)?;
        Some(TextPos {
            entry: last.entry.clone(),
            unit: last.index,
            byte: last.text.len(),
        })
    }
}

/// The texts `entry` makes selectable, in drawing order, before empty ones
/// are left out.
fn entry_parts<'a>(
    entry: &ChatEntry,
    id: &str,
    doc_of: &impl Fn(&str) -> Option<&'a MdDoc>,
    is_open: &impl Fn(&str) -> bool,
) -> Vec<(UnitSource, String)> {
    match entry {
        ChatEntry::User { text, .. } => vec![(UnitSource::User, text.clone())],
        ChatEntry::Answer { text, .. } => match doc_of(id) {
            Some(doc) => doc
                .unit_texts()
                .into_iter()
                .map(|unit| {
                    (
                        UnitSource::AnswerRow {
                            row: unit.row,
                            part: unit.part,
                        },
                        unit.text,
                    )
                })
                .collect(),
            None => vec![(UnitSource::AnswerPlain, text.clone())],
        },
        ChatEntry::Tool {
            name,
            summary,
            input,
            result,
            ..
        } => {
            let mut parts = vec![(UnitSource::ToolSummary, chat::tool_one_liner(summary, name))];
            if is_open(id) {
                parts.extend(
                    shell_lines::tool_statements(name, summary, input)
                        .into_iter()
                        .enumerate()
                        .map(|(line, text)| (UnitSource::ToolStatement { line }, text)),
                );
                parts.push((UnitSource::ToolInput, input.clone()));
                if let Some(outcome) = result {
                    parts.push((UnitSource::ToolResult, outcome.text.clone()));
                }
            }
            parts
        }
        ChatEntry::Thinking { .. }
        | ChatEntry::Receipt { .. }
        | ChatEntry::TurnEnd { .. }
        | ChatEntry::System { .. }
        | ChatEntry::Unknown => Vec::new(),
    }
}

/// A selection of the chat's text, by its two ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Selection {
    pub(crate) anchor: TextPos,
    pub(crate) focus: TextPos,
}

impl Selection {
    /// The selection's (start, end) in the units' order and by byte: the
    /// anchor first unless the focus comes before it, and equal ends for an
    /// empty one; `None` when either end names no unit.
    pub(crate) fn ordered(&self, units: &Units) -> Option<(TextPos, TextPos)> {
        let anchor = (units.index_of(&self.anchor)?, self.anchor.byte);
        let focus = (units.index_of(&self.focus)?, self.focus.byte);
        if focus < anchor {
            Some((self.focus.clone(), self.anchor.clone()))
        } else {
            Some((self.anchor.clone(), self.focus.clone()))
        }
    }
}

/// The range of `text` a word covers at `byte`: the run of letters, digits
/// and underscores the byte falls in, with a hyphen or apostrophe between
/// two of them kept inside. A byte on such a joining hyphen or apostrophe
/// gives the whole word it joins; one on any other character (a space,
/// punctuation, or a hyphen or apostrophe with no word on one side) is that
/// character alone. A byte at or past the text's end gives an empty range
/// at the end. Both ends are character boundaries.
pub(crate) fn word_at(text: &str, byte: usize) -> Range<usize> {
    let len = text.len();
    if byte >= len {
        return len..len;
    }
    let start = char_floor(text, byte);
    let Some(current) = text[start..].chars().next() else {
        return len..len;
    };
    let end = start + current.len_utf8();
    if !is_word_char(current) {
        let joins = is_joiner(current)
            && text[..start].chars().next_back().is_some_and(is_word_char)
            && text[end..].chars().next().is_some_and(is_word_char);
        if joins {
            return word_start(text, start)..word_end(text, end);
        }
        return start..end;
    }
    word_start(text, start)..word_end(text, end)
}

/// Whether `c` is one of the characters a word is made of.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Whether `c` joins two word characters into one word.
fn is_joiner(c: char) -> bool {
    c == '-' || c == '\''
}

/// Where the word `text[start..end]` starts: back over the word characters
/// before it, each hyphen or apostrophe followed by one kept inside.
fn word_start(text: &str, start: usize) -> usize {
    let mut at = start;
    while let Some(previous) = text[..at].chars().next_back() {
        if is_word_char(previous) {
            at -= previous.len_utf8();
            continue;
        }
        if is_joiner(previous) {
            let before = at - previous.len_utf8();
            if text[..before].chars().next_back().is_some_and(is_word_char) {
                at = before;
                continue;
            }
        }
        break;
    }
    at
}

/// Where the word starting with `text[..end]` ends: on over the word
/// characters after it, each hyphen or apostrophe followed by one kept
/// inside.
fn word_end(text: &str, end: usize) -> usize {
    let mut at = end;
    while let Some(current) = text[at..].chars().next() {
        if is_word_char(current) {
            at += current.len_utf8();
            continue;
        }
        if is_joiner(current) {
            let after = at + current.len_utf8();
            if text[after..].chars().next().is_some_and(is_word_char) {
                at = after;
                continue;
            }
        }
        break;
    }
    at
}

/// `byte` moved back to the start of the character it falls in, or to the
/// text's end when it is at or past it.
fn char_floor(text: &str, byte: usize) -> usize {
    let mut at = byte.min(text.len());
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// The text `selection` covers in `units`: each selected unit's part from
/// its start byte to its end byte, units of one entry joined with a
/// newline and entries by a blank line; nothing for an empty or
/// unresolvable selection.
pub(crate) fn selected_text(units: &Units, selection: &Selection) -> String {
    let Some((start, end)) = selection.ordered(units) else {
        return String::new();
    };
    let (Some(first), Some(last)) = (units.index_of(&start), units.index_of(&end)) else {
        return String::new();
    };
    let mut text = String::new();
    let mut entry: Option<&str> = None;
    for (offset, unit) in units.units[first..=last].iter().enumerate() {
        let index = first + offset;
        let from = if index == first {
            char_floor(&unit.text, start.byte)
        } else {
            0
        };
        let to = if index == last {
            char_floor(&unit.text, end.byte)
        } else {
            unit.text.len()
        };
        match entry {
            Some(id) if id == unit.entry.as_str() => text.push('\n'),
            Some(_) => text.push_str("\n\n"),
            None => {}
        }
        if from <= to {
            text.push_str(&unit.text[from..to]);
        }
        entry = Some(unit.entry.as_str());
    }
    text
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashMap};
    use std::sync::atomic::AtomicBool;

    use protocol::ToolKind;

    use super::*;
    use crate::chat::RunSummary;
    use crate::stretchtext;
    use crate::viewer_md_view::prepare_answer;

    fn user(id: &str, text: &str) -> ChatEntry {
        ChatEntry::User {
            id: id.to_owned(),
            text: text.to_owned(),
            pending: false,
        }
    }

    fn answer(id: &str, text: &str) -> ChatEntry {
        ChatEntry::Answer {
            id: id.to_owned(),
            text: text.to_owned(),
            live: false,
        }
    }

    fn tool(id: &str, name: &str, summary: &str, input: &str, result: Option<&str>) -> ChatEntry {
        ChatEntry::Tool {
            id: id.to_owned(),
            name: name.to_owned(),
            summary: summary.to_owned(),
            input: input.to_owned(),
            result: result.map(|text| protocol::ToolOutcome {
                text: text.to_owned(),
                is_error: false,
            }),
            parent_id: None,
            tail_paths: Vec::new(),
            kind: ToolKind::Other,
            script: None,
        }
    }

    /// `text` as the chat parses an answer.
    fn doc(text: &str) -> MdDoc {
        prepare_answer(
            &stretchtext::compose(text, &BTreeSet::new(), false),
            &AtomicBool::new(false),
        )
    }

    fn single_rows(entries: &[ChatEntry]) -> Vec<DisplayRow<'_>> {
        entries.iter().map(DisplayRow::Single).collect()
    }

    fn pos(entry: &str, unit: u32, byte: usize) -> TextPos {
        TextPos {
            entry: entry.to_owned(),
            unit,
            byte,
        }
    }

    fn shape(units: &Units) -> Vec<(String, u32, UnitSource, String)> {
        units
            .units
            .iter()
            .map(|unit| {
                (
                    unit.entry.clone(),
                    unit.index,
                    unit.source,
                    unit.text.clone(),
                )
            })
            .collect()
    }

    #[test]
    fn word_at_picks_the_word_under_the_byte() {
        assert_eq!(word_at("fix the tests", 5), 4..7);
    }

    #[test]
    fn word_at_keeps_inner_hyphens_and_apostrophes() {
        assert_eq!(word_at("don't re-read it", 6), 6..13);
        assert_eq!(word_at("don't re-read it", 2), 0..5);
    }

    #[test]
    fn word_at_on_punctuation_or_space_is_that_char() {
        assert_eq!(word_at("a, b", 1), 1..2);
        assert_eq!(word_at("a, b", 2), 2..3);
    }

    #[test]
    fn word_at_past_the_end_is_empty() {
        assert_eq!(word_at("ab", 5), 2..2);
        assert_eq!(word_at("", 0), 0..0);
    }

    #[test]
    fn word_at_respects_char_boundaries() {
        assert_eq!(word_at("café au", 3), 0..5);
        assert_eq!(word_at("café au", 4), 0..5);
    }

    #[test]
    fn word_at_on_a_joiner_selects_the_word_it_joins() {
        assert_eq!(word_at("don't", 3), 0..5);
        assert_eq!(word_at("re-read", 2), 0..7);
    }

    #[test]
    fn word_at_on_a_lone_joiner_is_that_char() {
        assert_eq!(word_at("a - b", 2), 2..3);
        assert_eq!(word_at("'quoted", 0), 0..1);
    }

    #[test]
    fn units_list_user_answer_rows_and_tool_summary() {
        let answer_text = "# Head\n\nbody text\n\n| c1 | c2 |\n| --- | --- |\n| v1 | v2 |\n";
        let entries = [
            user("u1", "hello there"),
            answer("a1", answer_text),
            tool("t1", "Read", "read a file", "", None),
        ];
        let rows = single_rows(&entries);
        let mut docs = HashMap::new();
        docs.insert("a1".to_owned(), doc(answer_text));
        let units = Units::build(&rows, |id| docs.get(id), |_| false);
        let expected = vec![
            (
                "u1".to_owned(),
                0,
                UnitSource::User,
                "hello there".to_owned(),
            ),
            (
                "a1".to_owned(),
                0,
                UnitSource::AnswerRow { row: 0, part: 0 },
                "Head".to_owned(),
            ),
            (
                "a1".to_owned(),
                1,
                UnitSource::AnswerRow { row: 1, part: 0 },
                "body text".to_owned(),
            ),
            (
                "a1".to_owned(),
                2,
                UnitSource::AnswerRow { row: 2, part: 0 },
                "c1".to_owned(),
            ),
            (
                "a1".to_owned(),
                3,
                UnitSource::AnswerRow { row: 2, part: 1 },
                "c2".to_owned(),
            ),
            (
                "a1".to_owned(),
                4,
                UnitSource::AnswerRow { row: 3, part: 0 },
                "v1".to_owned(),
            ),
            (
                "a1".to_owned(),
                5,
                UnitSource::AnswerRow { row: 3, part: 1 },
                "v2".to_owned(),
            ),
            (
                "t1".to_owned(),
                0,
                UnitSource::ToolSummary,
                "read a file".to_owned(),
            ),
        ];
        assert_eq!(shape(&units), expected);
    }

    #[test]
    fn an_open_tool_card_adds_statements_input_and_result() {
        let input = r#"{"command":"echo hi\nls -la"}"#;
        let entries = [tool("t1", "Bash", "run two commands", input, Some("done"))];
        let rows = single_rows(&entries);
        let units = Units::build(&rows, |_| None, |_| true);
        let expected = vec![
            (
                "t1".to_owned(),
                0,
                UnitSource::ToolSummary,
                "run two commands".to_owned(),
            ),
            (
                "t1".to_owned(),
                1,
                UnitSource::ToolStatement { line: 0 },
                "echo hi".to_owned(),
            ),
            (
                "t1".to_owned(),
                2,
                UnitSource::ToolStatement { line: 1 },
                "ls -la".to_owned(),
            ),
            ("t1".to_owned(), 3, UnitSource::ToolInput, input.to_owned()),
            (
                "t1".to_owned(),
                4,
                UnitSource::ToolResult,
                "done".to_owned(),
            ),
        ];
        assert_eq!(shape(&units), expected);
    }

    #[test]
    fn a_folded_run_and_muted_rows_have_no_units() {
        let entries = [
            tool("t1", "Read", "read a", "", None),
            tool("t2", "Search", "search b", "", None),
            ChatEntry::Thinking {
                id: "h1".to_owned(),
                secs: Some(3),
            },
            ChatEntry::Receipt {
                id: "r1".to_owned(),
                question: "q".to_owned(),
                answer: "a".to_owned(),
            },
            ChatEntry::TurnEnd {
                id: "e1".to_owned(),
                duration_ms: Some(1000),
            },
            ChatEntry::System {
                id: "s1".to_owned(),
                text: "cleared".to_owned(),
            },
            ChatEntry::Unknown,
        ];
        let rows = vec![
            DisplayRow::Run {
                entries: vec![&entries[0], &entries[1]],
                summary: RunSummary::default(),
            },
            DisplayRow::Single(&entries[2]),
            DisplayRow::Single(&entries[3]),
            DisplayRow::Single(&entries[4]),
            DisplayRow::Single(&entries[5]),
            DisplayRow::Single(&entries[6]),
        ];
        let units = Units::build(&rows, |_| None, |_| true);
        assert!(units.units.is_empty(), "{:?}", units.units);
    }

    #[test]
    fn ordered_swaps_a_focus_before_its_anchor() {
        let entries = [user("u1", "one"), user("u2", "two")];
        let rows = single_rows(&entries);
        let units = Units::build(&rows, |_| None, |_| false);
        let selection = Selection {
            anchor: pos("u2", 0, 2),
            focus: pos("u1", 0, 1),
        };
        assert_eq!(
            selection.ordered(&units),
            Some((pos("u1", 0, 1), pos("u2", 0, 2)))
        );
    }

    #[test]
    fn selected_text_joins_units_and_entries() {
        let answer_text = "# Head\n\n| a1 | b2 |\n| --- | --- |\n| c3 | d4 |\n";
        let entries = [user("u1", "hello world"), answer("a1", answer_text)];
        let rows = single_rows(&entries);
        let mut docs = HashMap::new();
        docs.insert("a1".to_owned(), doc(answer_text));
        let units = Units::build(&rows, |id| docs.get(id), |_| false);
        let selection = Selection {
            anchor: pos("u1", 0, 6),
            focus: pos("a1", 4, 1),
        };
        assert_eq!(
            selected_text(&units, &selection),
            "world\n\nHead\na1\nb2\nc3\nd"
        );
    }

    #[test]
    fn selected_text_of_a_list_item_leaves_out_its_marker() {
        let answer_text = "- one\n- two";
        let entries = [answer("a1", answer_text)];
        let rows = single_rows(&entries);
        let mut docs = HashMap::new();
        docs.insert("a1".to_owned(), doc(answer_text));
        let units = Units::build(&rows, |id| docs.get(id), |_| false);
        let selection = Selection {
            anchor: pos("a1", 0, 0),
            focus: pos("a1", 1, 3),
        };
        assert_eq!(selected_text(&units, &selection), "one\ntwo");
    }

    #[test]
    fn selected_text_keeps_blank_code_lines() {
        let answer_text = "```\nfn a() {\n\n    b();\n}\n```";
        let entries = [answer("a1", answer_text)];
        let rows = single_rows(&entries);
        let mut docs = HashMap::new();
        docs.insert("a1".to_owned(), doc(answer_text));
        let units = Units::build(&rows, |id| docs.get(id), |_| false);
        let blank = pos("a1", 1, 0);
        assert!(units.units[1].text.is_empty(), "the blank line is a unit");
        assert_eq!(units.clamp(&blank), Some(blank));
        assert_eq!(word_at(&units.units[1].text, 0), 0..0);
        let selection = Selection {
            anchor: pos("a1", 0, 0),
            focus: pos("a1", 3, 1),
        };
        assert_eq!(selected_text(&units, &selection), "fn a() {\n\n    b();\n}");
    }

    #[test]
    fn clamp_shortens_to_a_shorter_text() {
        let answer_text = "- one\n- two";
        let entries = [
            user("u1", "0123456789"),
            answer("a1", answer_text),
            user("u2", "café"),
        ];
        let rows = single_rows(&entries);
        let mut docs = HashMap::new();
        docs.insert("a1".to_owned(), doc(answer_text));
        let units = Units::build(&rows, |id| docs.get(id), |_| false);
        assert_eq!(units.clamp(&pos("u1", 0, 40)), Some(pos("u1", 0, 10)));
        assert_eq!(units.clamp(&pos("a1", 9, 0)), Some(pos("a1", 1, 3)));
        assert_eq!(units.clamp(&pos("zz", 0, 0)), None);
        assert_eq!(units.clamp(&pos("u2", 0, 4)), Some(pos("u2", 0, 3)));
    }

    #[test]
    fn an_empty_or_dangling_selection_copies_nothing() {
        let entries = [user("u1", "hello")];
        let rows = single_rows(&entries);
        let units = Units::build(&rows, |_| None, |_| false);
        let empty = Selection {
            anchor: pos("u1", 0, 2),
            focus: pos("u1", 0, 2),
        };
        assert_eq!(selected_text(&units, &empty), "");
        let dangling = Selection {
            anchor: pos("nope", 0, 0),
            focus: pos("u1", 0, 2),
        };
        assert_eq!(selected_text(&units, &dangling), "");
    }
}
