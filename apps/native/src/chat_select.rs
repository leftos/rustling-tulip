//! The phone Chat's text selection: the selectable texts of the rows a chat
//! pane draws, in drawing order, a position inside one, the word under a
//! byte, range ordering, clamping to a shorter text, and the one function
//! that builds the copied text.

use std::collections::HashMap;
use std::ops::Range;

use protocol::ChatEntry;

use crate::chat::{self, DisplayRow, RunItem, ScriptRow};
use crate::shell_lines;
use crate::transcript_view::TranscriptLine;
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
    /// A followed run's or subagent's transcript line, by its index within
    /// its entry ([`transcript_keys`]).
    TranscriptLine { line: u32 },
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
    /// By entry id, then by where the text comes from, the unit's position
    /// in `units`: the first unit with that source.
    by_source: HashMap<String, HashMap<UnitSource, usize>>,
}

/// A selection's ends as positions in the units' order, with their bytes
/// moved back to a character's start: what each drawn unit's selected
/// bytes are read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SelectedSpan {
    first: usize,
    start_byte: usize,
    last: usize,
    end_byte: usize,
}

impl Units {
    /// The selectable texts of `rows`, in drawing order: a user message's
    /// text; an answer's Markdown rows (its raw text without a parsed
    /// document); a tool card's one-liner, plus its statements, input and
    /// result while the card is open; nothing for a muted row; a script's
    /// line adds nothing, and under it an open script lists its call's and
    /// inner calls' cards, a closed one only its inner calls' cards that tail
    /// files; a closed run lists only its scripts (as standing ones), an open
    /// run each of its cards' texts as a single row's; empty texts left out
    /// except an answer's empty code line, which a copy keeps. `doc_of` and `is_open` take an entry id, `is_run_open`
    /// a run's first entry id, and `result_of` a tool id, answering the text
    /// that card draws for its result.
    pub(crate) fn build<'a>(
        rows: &[DisplayRow<'a>],
        doc_of: impl Fn(&str) -> Option<&'a MdDoc>,
        is_open: impl Fn(&str) -> bool,
        is_run_open: impl Fn(&str) -> bool,
        result_of: impl Fn(&str) -> Option<String>,
    ) -> Units {
        let mut units = Vec::new();
        for row in rows {
            match row {
                DisplayRow::Single(entry) => {
                    push_entry(&mut units, entry, &doc_of, &is_open, &result_of);
                }
                DisplayRow::Script(script) => {
                    push_script(&mut units, script, &doc_of, &is_open, &result_of);
                }
                DisplayRow::Run { entries, .. } => {
                    let open = entries
                        .first()
                        .and_then(RunItem::id)
                        .is_some_and(&is_run_open);
                    for item in entries {
                        match item {
                            RunItem::Entry(entry) if open => {
                                push_entry(&mut units, entry, &doc_of, &is_open, &result_of);
                            }
                            RunItem::Entry(_) => {}
                            RunItem::Script(script) => {
                                push_script(&mut units, script, &doc_of, &is_open, &result_of);
                            }
                        }
                    }
                }
            }
        }
        Units::indexed(units)
    }

    /// The selectable texts of a transcript's `lines`, one unit per line in
    /// drawing order, each keyed as [`transcript_keys`] gives it.
    pub(crate) fn from_transcript(lines: &[TranscriptLine]) -> Units {
        let units = transcript_keys(lines)
            .into_iter()
            .zip(lines)
            .map(|((entry, index), line)| Unit {
                entry,
                index,
                source: UnitSource::TranscriptLine { line: index },
                text: line.text.clone(),
            })
            .collect();
        Units::indexed(units)
    }

    /// `units` with the lookup by entry and source built over them.
    fn indexed(units: Vec<Unit>) -> Units {
        let mut by_source: HashMap<String, HashMap<UnitSource, usize>> = HashMap::new();
        for (at, unit) in units.iter().enumerate() {
            by_source
                .entry(unit.entry.clone())
                .or_default()
                .entry(unit.source)
                .or_insert(at);
        }
        Units { units, by_source }
    }

    /// The position in the units' order and the index within entry `entry`
    /// of the unit drawn from `source`, when that text is a unit (an empty
    /// one is not).
    pub(crate) fn locate(&self, entry: &str, source: UnitSource) -> Option<(usize, u32)> {
        let at = *self.by_source.get(entry)?.get(&source)?;
        Some((at, self.units.get(at)?.index))
    }

    /// The index within entry `entry` of the unit drawn from `source`, when
    /// that text is a unit (an empty one is not). The drawing reads
    /// [`Self::locate`], which gives the position too.
    #[cfg(test)]
    fn index_of_source(&self, entry: &str, source: UnitSource) -> Option<u32> {
        self.locate(entry, source).map(|(_, index)| index)
    }

    /// `ordered`, a selection's (start, end) in the units' order, as
    /// positions in the units with each byte moved back to a character's
    /// start; `None` when either end names no unit.
    pub(crate) fn resolve(&self, ordered: &(TextPos, TextPos)) -> Option<SelectedSpan> {
        let (start, end) = ordered;
        let (first, last) = (self.index_of(start)?, self.index_of(end)?);
        Some(SelectedSpan {
            first,
            start_byte: char_floor(&self.units.get(first)?.text, start.byte),
            last,
            end_byte: char_floor(&self.units.get(last)?.text, end.byte),
        })
    }

    /// The bytes of the unit at position `at` that `span` covers: from the
    /// start byte in the first unit, to the end byte in the last, the whole
    /// text in between; `None` when the unit is outside the span or nothing
    /// of it is covered.
    pub(crate) fn range_at(&self, span: &SelectedSpan, at: usize) -> Option<Range<usize>> {
        if at < span.first || at > span.last {
            return None;
        }
        let text = &self.units.get(at)?.text;
        let from = if at == span.first { span.start_byte } else { 0 };
        let to = if at == span.last {
            span.end_byte
        } else {
            text.len()
        };
        (from < to).then_some(from..to)
    }

    /// The bytes of unit `unit` of entry `entry` that `ordered`, a
    /// selection's (start, end) in the units' order, covers: from the start
    /// byte in the start unit, to the end byte in the end unit, the whole
    /// text in between, both ends moved back to a character's start; `None`
    /// when the unit is outside the selection, unknown, or nothing of it is
    /// covered. The drawing reads [`Self::range_at`] with the selection
    /// resolved once per frame.
    #[cfg(test)]
    fn selected_range(
        &self,
        ordered: &(TextPos, TextPos),
        entry: &str,
        unit: u32,
    ) -> Option<Range<usize>> {
        let span = self.resolve(ordered)?;
        let at = self
            .units
            .iter()
            .position(|drawn| drawn.entry == entry && drawn.index == unit)?;
        self.range_at(&span, at)
    }

    /// The index of `pos` in the units' order, when a unit has it.
    fn index_of(&self, pos: &TextPos) -> Option<usize> {
        self.units
            .iter()
            .position(|unit| unit.entry == pos.entry && unit.index == pos.unit)
    }

    /// The word ([`word_at`]) of the unit `pos` names, around its byte, as
    /// its (start, end) positions; `None` when no unit has `pos`, or the
    /// byte is at the text's end or on white space.
    pub(crate) fn word_around(&self, pos: &TextPos) -> Option<(TextPos, TextPos)> {
        let text = &self.units.get(self.index_of(pos)?)?.text;
        let word = word_at(text, pos.byte);
        let blank = text.get(word.clone())?.chars().all(char::is_whitespace);
        if blank {
            return None;
        }
        let at = |byte| TextPos {
            entry: pos.entry.clone(),
            unit: pos.unit,
            byte,
        };
        Some((at(word.start), at(word.end)))
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

/// The (entry key, index within that entry) of each of a transcript's
/// `lines`, in drawing order: the id of the entry that drew the line (the
/// hidden-entries note's fixed key), and the line's index among that
/// entry's lines, so a line keeps its key while lines come or go above it.
pub(crate) fn transcript_keys(lines: &[TranscriptLine]) -> Vec<(String, u32)> {
    let mut counts: HashMap<String, u32> = HashMap::new();
    let mut keys = Vec::with_capacity(lines.len());
    for line in lines {
        let entry = line.drawn_by.clone();
        let count = counts.entry(entry.clone()).or_default();
        keys.push((entry, *count));
        *count = count.saturating_add(1);
    }
    keys
}

/// Pushes the units of the cards a script draws under its line, in drawing
/// order, as each card gave them on its own: open, the script call's and
/// each inner call's; closed, only the inner calls that tail files
/// (`ScriptRow::shown_inner`).
fn push_script<'a>(
    units: &mut Vec<Unit>,
    script: &ScriptRow<'_>,
    doc_of: &impl Fn(&str) -> Option<&'a MdDoc>,
    is_open: &impl Fn(&str) -> bool,
    result_of: &impl Fn(&str) -> Option<String>,
) {
    let open = chat::entry_id(script.call).is_some_and(is_open);
    let cards = open
        .then_some(script.call)
        .into_iter()
        .chain(script.shown_inner(open));
    for entry in cards {
        push_entry(units, entry, doc_of, is_open, result_of);
    }
}

/// Pushes onto `units` the selectable texts of `entry`, numbered within it,
/// empty ones left out except an answer's rows.
fn push_entry<'a>(
    units: &mut Vec<Unit>,
    entry: &ChatEntry,
    doc_of: &impl Fn(&str) -> Option<&'a MdDoc>,
    is_open: &impl Fn(&str) -> bool,
    result_of: &impl Fn(&str) -> Option<String>,
) {
    let Some(id) = chat::entry_id(entry) else {
        return;
    };
    let parts = entry_parts(entry, id, doc_of, is_open, result_of);
    for (index, (source, text)) in parts
        .into_iter()
        .filter(|(source, text)| !text.is_empty() || matches!(source, UnitSource::AnswerRow { .. }))
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

/// The texts `entry` makes selectable, in drawing order, before empty ones
/// are left out.
fn entry_parts<'a>(
    entry: &ChatEntry,
    id: &str,
    doc_of: &impl Fn(&str) -> Option<&'a MdDoc>,
    is_open: &impl Fn(&str) -> bool,
    result_of: &impl Fn(&str) -> Option<String>,
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
                    let text = result_of(id).unwrap_or_else(|| outcome.text.clone());
                    parts.push((UnitSource::ToolResult, text));
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

/// The selection a held finger at `focus` makes from `word`, the (start,
/// end) of the word the hold picked: the word itself while the finger lies
/// within it (its ends included) in the units' order (entry, unit, byte);
/// anchored at the word's end while the finger lies before it, at its
/// start while after it; `None` when any of them names no unit.
pub(crate) fn widen(units: &Units, word: &(TextPos, TextPos), focus: TextPos) -> Option<Selection> {
    let (start, end) = word;
    let at = (units.index_of(&focus)?, focus.byte);
    let from = (units.index_of(start)?, start.byte);
    let to = (units.index_of(end)?, end.byte);
    let (anchor, focus) = if at < from {
        (end.clone(), focus)
    } else if at <= to {
        (start.clone(), end.clone())
    } else {
        (start.clone(), focus)
    };
    Some(Selection { anchor, focus })
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
pub(crate) fn char_floor(text: &str, byte: usize) -> usize {
    let mut at = byte.min(text.len());
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// The text `selection` covers in `units`: each selected unit's part from
/// its start byte to its end byte, units of one entry joined with a
/// newline and entries by a blank line, a transcript's lines each by a
/// newline whatever their entries; nothing for an empty or
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
        let line = matches!(unit.source, UnitSource::TranscriptLine { .. });
        match entry {
            Some(id) if line || id == unit.entry.as_str() => text.push('\n'),
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
    use crate::links::IssueContext;
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
            &IssueContext::default(),
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
        let units = Units::build(&rows, |id| docs.get(id), |_| false, |_| false, |_| None);
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
        let units = Units::build(&rows, |_| None, |_| true, |_| false, |_| None);
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
                entries: vec![RunItem::Entry(&entries[0]), RunItem::Entry(&entries[1])],
                summary: RunSummary::default(),
            },
            DisplayRow::Single(&entries[2]),
            DisplayRow::Single(&entries[3]),
            DisplayRow::Single(&entries[4]),
            DisplayRow::Single(&entries[5]),
            DisplayRow::Single(&entries[6]),
        ];
        let units = Units::build(&rows, |_| None, |_| true, |_| false, |_| None);
        assert!(units.units.is_empty(), "{:?}", units.units);
    }

    #[test]
    fn ordered_swaps_a_focus_before_its_anchor() {
        let entries = [user("u1", "one"), user("u2", "two")];
        let rows = single_rows(&entries);
        let units = Units::build(&rows, |_| None, |_| false, |_| false, |_| None);
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
        let units = Units::build(&rows, |id| docs.get(id), |_| false, |_| false, |_| None);
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
        let units = Units::build(&rows, |id| docs.get(id), |_| false, |_| false, |_| None);
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
        let units = Units::build(&rows, |id| docs.get(id), |_| false, |_| false, |_| None);
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
        let units = Units::build(&rows, |id| docs.get(id), |_| false, |_| false, |_| None);
        assert_eq!(units.clamp(&pos("u1", 0, 40)), Some(pos("u1", 0, 10)));
        assert_eq!(units.clamp(&pos("a1", 9, 0)), Some(pos("a1", 1, 3)));
        assert_eq!(units.clamp(&pos("zz", 0, 0)), None);
        assert_eq!(units.clamp(&pos("u2", 0, 4)), Some(pos("u2", 0, 3)));
    }

    #[test]
    fn an_empty_or_dangling_selection_copies_nothing() {
        let entries = [user("u1", "hello")];
        let rows = single_rows(&entries);
        let units = Units::build(&rows, |_| None, |_| false, |_| false, |_| None);
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

    #[test]
    fn selected_range_in_a_unit_covers_start_middle_end_and_one_unit() {
        let entries = [
            user("u1", "hello"),
            user("u2", "middle"),
            user("u3", "café x"),
        ];
        let rows = single_rows(&entries);
        let units = Units::build(&rows, |_| None, |_| false, |_| false, |_| None);
        let across = (pos("u1", 0, 2), pos("u3", 0, 4));
        assert_eq!(units.selected_range(&across, "u1", 0), Some(2..5));
        assert_eq!(units.selected_range(&across, "u2", 0), Some(0..6));
        assert_eq!(
            units.selected_range(&across, "u3", 0),
            Some(0..3),
            "an end inside a character moves to its start"
        );
        let inside = (pos("u2", 0, 1), pos("u2", 0, 4));
        assert_eq!(units.selected_range(&inside, "u1", 0), None);
        assert_eq!(units.selected_range(&inside, "u2", 0), Some(1..4));
        assert_eq!(units.selected_range(&inside, "u3", 0), None);
        let empty = (pos("u2", 0, 3), pos("u2", 0, 3));
        assert_eq!(units.selected_range(&empty, "u2", 0), None);
        assert_eq!(units.selected_range(&across, "zz", 0), None);
        assert_eq!(units.selected_range(&across, "u1", 1), None);
    }

    #[test]
    fn index_of_source_finds_each_drawn_unit() {
        let answer_text = "# Head\n\n| c1 | c2 |\n| --- | --- |\n";
        let entries = [
            user("u1", "hi"),
            answer("a1", answer_text),
            tool("t1", "Bash", "list", r#"{"command":"ls"}"#, Some("done")),
            tool("t2", "Read", "read", "", None),
        ];
        let rows = single_rows(&entries);
        let mut docs = HashMap::new();
        docs.insert("a1".to_owned(), doc(answer_text));
        let units = Units::build(&rows, |id| docs.get(id), |_| true, |_| false, |_| None);
        assert_eq!(units.index_of_source("u1", UnitSource::User), Some(0));
        let cell = UnitSource::AnswerRow { row: 1, part: 1 };
        assert_eq!(units.index_of_source("a1", cell), Some(2));
        let statement = UnitSource::ToolStatement { line: 0 };
        assert_eq!(units.index_of_source("t1", statement), Some(1));
        assert_eq!(units.index_of_source("t1", UnitSource::ToolResult), Some(3));
        assert_eq!(
            units.index_of_source("t2", UnitSource::ToolInput),
            None,
            "an empty input is no unit"
        );
        assert_eq!(units.index_of_source("u1", UnitSource::AnswerPlain), None);
        assert_eq!(units.index_of_source("zz", UnitSource::User), None);
    }

    #[test]
    fn an_open_run_lists_its_cards_units() {
        let entries = [
            tool("t1", "Read", "read a", "", None),
            tool("t2", "Search", "search b", "", None),
        ];
        let rows = vec![DisplayRow::Run {
            entries: vec![RunItem::Entry(&entries[0]), RunItem::Entry(&entries[1])],
            summary: RunSummary::default(),
        }];
        let units = Units::build(&rows, |_| None, |_| false, |id| id == "t1", |_| None);
        let expected = vec![
            (
                "t1".to_owned(),
                0,
                UnitSource::ToolSummary,
                "read a".to_owned(),
            ),
            (
                "t2".to_owned(),
                0,
                UnitSource::ToolSummary,
                "search b".to_owned(),
            ),
        ];
        assert_eq!(shape(&units), expected);
        let folded = Units::build(&rows, |_| None, |_| false, |_| false, |_| None);
        assert!(folded.units.is_empty(), "{:?}", folded.units);
    }

    #[test]
    fn a_script_contributes_its_cards_units_only_while_open() {
        let entries = [
            tool("s1", "REPL", "run a script", "{}", Some("done")),
            tool("r1", "Read", "read a", "", Some("text")),
        ];
        let before = Units::build(
            &single_rows(&entries),
            |_| None,
            |_| true,
            |_| false,
            |_| None,
        );
        assert_ne!(before.units, Vec::new());
        let script = ScriptRow {
            call: &entries[0],
            inner: vec![&entries[1]],
            running: false,
        };
        let standing = vec![DisplayRow::Script(script.clone())];
        let units = Units::build(&standing, |_| None, |_| true, |_| false, |_| None);
        assert_eq!(
            shape(&units),
            shape(&before),
            "an open script standing alone"
        );
        let closed = Units::build(&standing, |_| None, |id| id != "s1", |_| false, |_| None);
        assert!(
            closed.units.is_empty(),
            "a closed script: {:?}",
            closed.units
        );
        let in_run = vec![DisplayRow::Run {
            entries: vec![RunItem::Script(script)],
            summary: RunSummary::default(),
        }];
        let open = Units::build(&in_run, |_| None, |_| true, |id| id == "s1", |_| None);
        assert_eq!(shape(&open), shape(&before), "a script in an open run");
        let closed_run = Units::build(&in_run, |_| None, |_| true, |_| false, |_| None);
        assert_eq!(
            shape(&closed_run),
            shape(&before),
            "an open script under a closed run's line"
        );
        let closed = Units::build(&in_run, |_| None, |id| id != "s1", |_| false, |_| None);
        assert!(closed.units.is_empty(), "{:?}", closed.units);
    }

    #[test]
    fn a_closed_run_of_a_script_pushes_its_tailing_inner_calls_units_only() {
        let mut entries = [
            tool("s1", "REPL", "run a script", "{}", Some("done")),
            tool("r1", "Read", "read a", "", Some("text")),
            tool("b1", "Bash", "build it", "", Some("built")),
        ];
        if let ChatEntry::Tool { tail_paths, .. } = &mut entries[2] {
            tail_paths.push("build.log".to_owned());
        }
        let tailing = Units::build(
            &single_rows(&entries[2..]),
            |_| None,
            |_| false,
            |_| false,
            |_| None,
        );
        assert_ne!(tailing.units, Vec::new());
        let in_run = vec![DisplayRow::Run {
            entries: vec![RunItem::Script(ScriptRow {
                call: &entries[0],
                inner: vec![&entries[1], &entries[2]],
                running: false,
            })],
            summary: RunSummary::default(),
        }];
        let units = Units::build(&in_run, |_| None, |_| false, |_| false, |_| None);
        assert_eq!(shape(&units), shape(&tailing));
    }

    #[test]
    fn units_index_lookup_matches_a_linear_scan() {
        let answer_text = "intro\n\n| c1 | c2 |\n| --- | --- |\n| v1 | v2 |\n\n```\nfn a() {}\n\nlet b = 1;\n```\n";
        let input = r#"{"command":"echo hi\nls -la"}"#;
        let entries = [
            user("u1", "hello"),
            answer("a1", answer_text),
            tool("t1", "Bash", "run two commands", input, Some("done")),
        ];
        let rows = single_rows(&entries);
        let mut docs = HashMap::new();
        docs.insert("a1".to_owned(), doc(answer_text));
        let units = Units::build(&rows, |id| docs.get(id), |_| true, |_| false, |_| None);
        assert!(units.units.len() > 10, "{:?}", units.units);
        for unit in &units.units {
            let linear = units
                .units
                .iter()
                .position(|other| other.entry == unit.entry && other.source == unit.source)
                .map(|at| (at, units.units[at].index));
            assert_eq!(units.locate(&unit.entry, unit.source), linear, "{unit:?}");
            assert_eq!(
                units.index_of_source(&unit.entry, unit.source),
                linear.map(|(_, index)| index)
            );
        }
        assert_eq!(units.locate("u1", UnitSource::ToolInput), None);
        assert_eq!(units.locate("zz", UnitSource::User), None);
    }

    #[test]
    fn word_around_picks_the_word_under_the_byte() {
        let entries = [user("u1", "alpha bravo, charlie")];
        let rows = single_rows(&entries);
        let units = Units::build(&rows, |_| None, |_| false, |_| false, |_| None);
        assert_eq!(
            units.word_around(&pos("u1", 0, 8)),
            Some((pos("u1", 0, 6), pos("u1", 0, 11)))
        );
        assert_eq!(
            units.word_around(&pos("u1", 0, 11)),
            Some((pos("u1", 0, 11), pos("u1", 0, 12))),
            "punctuation is its own word"
        );
    }

    #[test]
    fn word_around_white_space_the_end_or_no_unit_is_none() {
        let entries = [user("u1", "alpha  bravo")];
        let rows = single_rows(&entries);
        let units = Units::build(&rows, |_| None, |_| false, |_| false, |_| None);
        assert_eq!(units.word_around(&pos("u1", 0, 5)), None, "a space");
        assert_eq!(units.word_around(&pos("u1", 0, 12)), None, "the end");
        assert_eq!(units.word_around(&pos("u1", 1, 0)), None, "no unit");
        assert_eq!(units.word_around(&pos("zz", 0, 0)), None, "no entry");
    }

    /// Two users' texts, "alpha bravo charlie" and "delta echo", and the
    /// word "bravo" of the first.
    fn bravo() -> (Units, (TextPos, TextPos)) {
        let entries = [user("u1", "alpha bravo charlie"), user("u2", "delta echo")];
        let rows = single_rows(&entries);
        let units = Units::build(&rows, |_| None, |_| false, |_| false, |_| None);
        (units, (pos("u1", 0, 6), pos("u1", 0, 11)))
    }

    #[test]
    fn widen_after_the_word_anchors_at_its_start() {
        let (units, word) = bravo();
        for focus in [pos("u1", 0, 15), pos("u2", 0, 3)] {
            assert_eq!(
                widen(&units, &word, focus.clone()),
                Some(Selection {
                    anchor: pos("u1", 0, 6),
                    focus
                })
            );
        }
        assert_eq!(
            widen(&units, &word, pos("u1", 0, 6)),
            Some(Selection {
                anchor: pos("u1", 0, 6),
                focus: pos("u1", 0, 11)
            }),
            "at the word's start, the word"
        );
    }

    #[test]
    fn widen_inside_the_word_keeps_the_word() {
        let (units, word) = bravo();
        for focus in [pos("u1", 0, 8), pos("u1", 0, 11)] {
            assert_eq!(
                widen(&units, &word, focus),
                Some(Selection {
                    anchor: pos("u1", 0, 6),
                    focus: pos("u1", 0, 11)
                })
            );
        }
    }

    #[test]
    fn widen_before_the_word_anchors_at_its_end() {
        let (units, word) = bravo();
        let focus = pos("u1", 0, 2);
        let selection = widen(&units, &word, focus.clone());
        assert_eq!(
            selection,
            Some(Selection {
                anchor: pos("u1", 0, 11),
                focus
            })
        );
        let copied = selection.map(|selection| selected_text(&units, &selection));
        assert_eq!(copied.as_deref(), Some("pha bravo"));
    }

    fn line(text: &str, entry: Option<&str>, drawn_by: &str) -> TranscriptLine {
        TranscriptLine {
            text: text.to_owned(),
            kind: crate::transcript_view::LineKind::Answer,
            entry: entry.map(str::to_owned),
            drawn_by: drawn_by.to_owned(),
        }
    }

    /// A note, a two-line answer, a tool call with two output lines and a
    /// system line, as a transcript draws them.
    fn transcript() -> Vec<TranscriptLine> {
        vec![
            line(
                "… 3 earlier entries not shown",
                None,
                crate::transcript_view::HIDDEN_NOTE_KEY,
            ),
            line("● alpha bravo", None, "a1"),
            line("  charlie delta", None, "a1"),
            line("● Bash(ls)", Some("t1"), "t1"),
            line("  ⎿  a.rs", Some("t1"), "t1"),
            line("     b.rs", Some("t1"), "t1"),
            line("cleared", None, "s1"),
        ]
    }

    fn transcript_line(line: u32) -> UnitSource {
        UnitSource::TranscriptLine { line }
    }

    #[test]
    fn transcript_units_key_entry_lines_by_id_and_the_rest_by_count() {
        let units = Units::from_transcript(&transcript());
        let expected = vec![
            (
                crate::transcript_view::HIDDEN_NOTE_KEY.to_owned(),
                0,
                transcript_line(0),
                "… 3 earlier entries not shown".to_owned(),
            ),
            (
                "a1".to_owned(),
                0,
                transcript_line(0),
                "● alpha bravo".to_owned(),
            ),
            (
                "a1".to_owned(),
                1,
                transcript_line(1),
                "  charlie delta".to_owned(),
            ),
            (
                "t1".to_owned(),
                0,
                transcript_line(0),
                "● Bash(ls)".to_owned(),
            ),
            (
                "t1".to_owned(),
                1,
                transcript_line(1),
                "  ⎿  a.rs".to_owned(),
            ),
            (
                "t1".to_owned(),
                2,
                transcript_line(2),
                "     b.rs".to_owned(),
            ),
            ("s1".to_owned(), 0, transcript_line(0), "cleared".to_owned()),
        ];
        assert_eq!(shape(&units), expected);
        assert_eq!(units.locate("t1", transcript_line(2)), Some((5, 2)));
    }

    #[test]
    fn selected_transcript_text_joins_lines_with_a_newline() {
        let units = Units::from_transcript(&transcript());
        let across = Selection {
            anchor: pos("a1", 1, 10),
            focus: pos("t1", 1, 7),
        };
        assert_eq!(
            selected_text(&units, &across),
            "delta\n● Bash(ls)\n  ⎿  ",
            "prefixes included, one newline between lines of any entries"
        );
    }

    #[test]
    fn a_transcript_word_and_a_whole_line_select_their_text() {
        let units = Units::from_transcript(&transcript());
        let word = units
            .word_around(&pos("a1", 0, 12))
            .map(|(anchor, focus)| Selection { anchor, focus });
        assert_eq!(
            word.map(|word| selected_text(&units, &word)).as_deref(),
            Some("bravo")
        );
        let whole = Selection {
            anchor: pos("t1", 2, 0),
            focus: units
                .clamp(&pos("t1", 2, usize::MAX))
                .unwrap_or_else(|| pos("t1", 2, 0)),
        };
        assert_eq!(selected_text(&units, &whole), "     b.rs");
    }

    #[test]
    fn widen_to_no_unit_is_none() {
        let (units, word) = bravo();
        assert_eq!(widen(&units, &word, pos("zz", 0, 0)), None);
        let gone = (pos("zz", 0, 0), pos("zz", 0, 1));
        assert_eq!(widen(&units, &gone, pos("u1", 0, 0)), None);
    }
}
