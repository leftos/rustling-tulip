//! A side-by-side diff of two texts: rows that pair the old and the new
//! lines, the word spans that changed inside a modified row, and the hunks
//! the next / previous change keys walk.
//!
//! Lines are compared with `similar`'s Patience diff. The texts are held once
//! and every row points into them by byte range.

use std::cell::OnceCell;
use std::ops::Range;
use std::time::{Duration, Instant};

use similar::{Algorithm, DiffOp, DiffTag, TextDiff, capture_diff_slices_deadline};

/// How long one line diff may run before it gives up and approximates.
const DIFF_DEADLINE: Duration = Duration::from_secs(2);
/// How long the word diff of one modified row may run.
const INLINE_DEADLINE: Duration = Duration::from_millis(100);
/// The longest line, in bytes, a modified row word-diffs; a row with a
/// longer side marks no words, only its row tint.
const MAX_INLINE_BYTES: usize = 1000;
/// The word similarity a modified row needs for its changed words to be
/// marked; below it the row is highlighted as a whole.
const MIN_INLINE_RATIO: f32 = 0.5;

/// How lines are compared.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiffOptions {
    /// Compares lines without their leading and trailing whitespace; they
    /// are still shown as they are.
    pub ignore_trim_whitespace: bool,
}

/// What a row shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    /// The same line on both sides.
    Equal,
    /// A line only the old text has; the new side is a filler.
    Delete,
    /// A line only the new text has; the old side is a filler.
    Insert,
    /// An old line and the new line that replaced it.
    Modify,
}

/// One side's line in a row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Side {
    /// The line's number in its text, from 1.
    pub line_no: u32,
    /// The line's bytes in its text, without the line ending.
    pub text_range: Range<usize>,
}

/// One row of the side-by-side view; a side that is `None` is a filler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub left: Option<Side>,
    pub right: Option<Side>,
    pub kind: RowKind,
}

/// The changed word spans of a modified row, as byte ranges into each
/// side's line text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InlineSpans {
    pub left: Vec<Range<usize>>,
    pub right: Vec<Range<usize>>,
}

/// The rows and hunks of a diff between two texts.
#[derive(Debug)]
pub struct DiffModel {
    old: String,
    new: String,
    opts: DiffOptions,
    rows: Vec<Row>,
    hunks: Vec<Range<usize>>,
    /// Each row's changed words, worked out the first time they are asked
    /// for; `None` for a row that marks none.
    inline: Vec<OnceCell<Option<InlineSpans>>>,
    line_endings_changed: bool,
    final_newline_changed: bool,
}

impl DiffModel {
    /// Diffs `old` against `new` line by line.
    #[must_use]
    pub fn build(old: &str, new: &str, opts: DiffOptions) -> Self {
        Self::build_with(old, new, opts, |old_keys, new_keys| {
            line_ops(old_keys, new_keys, Instant::now() + DIFF_DEADLINE)
        })
    }

    /// Diffs with the line ops `diff` returns for the two sides' keys.
    fn build_with(
        old: &str,
        new: &str,
        opts: DiffOptions,
        diff: impl FnOnce(&[&str], &[&str]) -> Vec<DiffOp>,
    ) -> Self {
        let old_lines = split_lines(old);
        let new_lines = split_lines(new);
        let old_keys = keys(old, &old_lines, opts);
        let new_keys = keys(new, &new_lines, opts);
        let ops = diff(&old_keys, &new_keys);
        let rows = rows_from_ops(&ops, &old_lines, &new_lines);
        let hunks = hunks_of(&rows);
        let inline = std::iter::repeat_with(OnceCell::new)
            .take(rows.len())
            .collect();
        let line_endings_changed = line_endings_differ(old, new, &rows);
        let final_newline_changed =
            !old.is_empty() && !new.is_empty() && old.ends_with('\n') != new.ends_with('\n');
        Self {
            old: old.to_owned(),
            new: new.to_owned(),
            opts,
            rows,
            hunks,
            inline,
            line_endings_changed,
            final_newline_changed,
        }
    }

    /// Every row, top to bottom.
    #[must_use]
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// The text of an old-side line.
    #[must_use]
    pub fn old_text(&self, side: &Side) -> &str {
        self.old.get(side.text_range.clone()).unwrap_or_default()
    }

    /// The text of a new-side line.
    #[must_use]
    pub fn new_text(&self, side: &Side) -> &str {
        self.new.get(side.text_range.clone()).unwrap_or_default()
    }

    /// The changed words of row `row`: only a modified row whose two lines
    /// are similar enough, and no longer than [`MAX_INLINE_BYTES`], has
    /// them. With trim whitespace ignored, the lines' leading and trailing
    /// whitespace is left out of the word diff. Worked out on the first call
    /// and kept.
    #[must_use]
    pub fn inline(&self, row: usize) -> Option<&InlineSpans> {
        let cell = self.inline.get(row)?;
        cell.get_or_init(|| {
            let row = self.rows.get(row)?;
            if row.kind != RowKind::Modify {
                return None;
            }
            let (left, right) = (row.left.as_ref()?, row.right.as_ref()?);
            let (old, new) = (self.old_text(left), self.new_text(right));
            if old.len() > MAX_INLINE_BYTES || new.len() > MAX_INLINE_BYTES {
                return None;
            }
            if self.opts.ignore_trim_whitespace {
                trimmed_inline_spans(old, new)
            } else {
                inline_spans(old, new)
            }
        })
        .as_ref()
    }

    /// The hunks, each a maximal run of rows that are not equal, as row
    /// index ranges.
    #[must_use]
    pub fn hunks(&self) -> &[Range<usize>] {
        &self.hunks
    }

    /// How many changes the diff has: its hunk count.
    #[must_use]
    pub fn change_count(&self) -> usize {
        self.hunks.len()
    }

    /// The first hunk starting below row `from_row`, else the first hunk.
    #[must_use]
    pub fn next_hunk(&self, from_row: usize) -> Option<usize> {
        let first = self.first_hunk()?;
        Some(
            self.hunks
                .iter()
                .position(|hunk| hunk.start > from_row)
                .unwrap_or(first),
        )
    }

    /// The last hunk starting above row `from_row`, else the last hunk.
    #[must_use]
    pub fn prev_hunk(&self, from_row: usize) -> Option<usize> {
        let last = self.last_hunk()?;
        Some(
            self.hunks
                .iter()
                .rposition(|hunk| hunk.start < from_row)
                .unwrap_or(last),
        )
    }

    #[must_use]
    pub fn first_hunk(&self) -> Option<usize> {
        (!self.hunks.is_empty()).then_some(0)
    }

    #[must_use]
    pub fn last_hunk(&self) -> Option<usize> {
        self.hunks.len().checked_sub(1)
    }

    /// Whether a line both sides have ends in `\r\n` on one side and `\n`
    /// on the other; that alone makes no hunk.
    #[must_use]
    pub fn line_endings_changed(&self) -> bool {
        self.line_endings_changed
    }

    /// Whether only one of two non-empty texts ends in a newline; that alone
    /// makes no hunk.
    #[must_use]
    pub fn final_newline_changed(&self) -> bool {
        self.final_newline_changed
    }
}

/// Whether a row with both sides has a line ending in `\r\n` on one side and
/// `\n` on the other. A last line without an ending is left to
/// [`DiffModel::final_newline_changed`].
fn line_endings_differ(old: &str, new: &str, rows: &[Row]) -> bool {
    let ending = |text: &str, side: &Side| text.as_bytes().get(side.text_range.end).copied();
    rows.iter().any(|row| match (&row.left, &row.right) {
        (Some(left), Some(right)) => match (ending(old, left), ending(new, right)) {
            (Some(old_end), Some(new_end)) => old_end != new_end,
            _ => false,
        },
        _ => false,
    })
}

/// Each line's bytes, without its `\n` or a `\r` before it. A text that
/// ends in a newline has no empty last line, so a newline at the end of only
/// one side changes nothing; an empty text has no lines.
fn split_lines(text: &str) -> Vec<Range<usize>> {
    let mut lines = Vec::new();
    let mut start = 0;
    for (at, _) in text.match_indices('\n') {
        lines.push(without_cr(text, start..at));
        start = at + 1;
    }
    if start < text.len() {
        lines.push(without_cr(text, start..text.len()));
    }
    lines
}

fn without_cr(text: &str, line: Range<usize>) -> Range<usize> {
    if text.get(line.clone()).is_some_and(|s| s.ends_with('\r')) {
        line.start..line.end - 1
    } else {
        line
    }
}

/// What each line is compared by: its text, trimmed when `opts` says so.
fn keys<'a>(text: &'a str, lines: &[Range<usize>], opts: DiffOptions) -> Vec<&'a str> {
    lines
        .iter()
        .map(|line| {
            let s = text.get(line.clone()).unwrap_or_default();
            if opts.ignore_trim_whitespace {
                s.trim()
            } else {
                s
            }
        })
        .collect()
}

/// The line diff: Patience, which past `deadline` approximates the rest of
/// the diff with a coarser but still correct one.
fn line_ops(old: &[&str], new: &[&str], deadline: Instant) -> Vec<DiffOp> {
    capture_diff_slices_deadline(Algorithm::Patience, old, new, Some(deadline))
}

/// The rows `ops` describe. A replace pairs its old and new lines row by
/// row as modified; the side with more lines ends in deletes or inserts.
fn rows_from_ops(ops: &[DiffOp], old: &[Range<usize>], new: &[Range<usize>]) -> Vec<Row> {
    let side = |lines: &[Range<usize>], index: usize| {
        lines.get(index).map(|range| Side {
            line_no: u32::try_from(index + 1).unwrap_or(u32::MAX),
            text_range: range.clone(),
        })
    };
    let mut rows = Vec::with_capacity(old.len().max(new.len()));
    for op in ops {
        let (tag, old_range, new_range) = op.as_tag_tuple();
        let paired = match tag {
            DiffTag::Equal | DiffTag::Replace => old_range.len().min(new_range.len()),
            DiffTag::Delete | DiffTag::Insert => 0,
        };
        let pair_kind = if tag == DiffTag::Equal {
            RowKind::Equal
        } else {
            RowKind::Modify
        };
        for offset in 0..paired {
            rows.push(Row {
                left: side(old, old_range.start + offset),
                right: side(new, new_range.start + offset),
                kind: pair_kind,
            });
        }
        for index in old_range.skip(paired) {
            rows.push(Row {
                left: side(old, index),
                right: None,
                kind: RowKind::Delete,
            });
        }
        for index in new_range.skip(paired) {
            rows.push(Row {
                left: None,
                right: side(new, index),
                kind: RowKind::Insert,
            });
        }
    }
    rows
}

/// The maximal runs of rows that are not equal.
fn hunks_of(rows: &[Row]) -> Vec<Range<usize>> {
    let mut hunks: Vec<Range<usize>> = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        if row.kind == RowKind::Equal {
            continue;
        }
        match hunks.last_mut() {
            Some(hunk) if hunk.end == index => hunk.end = index + 1,
            _ => hunks.push(index..index + 1),
        }
    }
    hunks
}

/// The changed words between two similar lines, or `None` when their word
/// similarity is under [`MIN_INLINE_RATIO`].
fn inline_spans(old: &str, new: &str) -> Option<InlineSpans> {
    let diff = TextDiff::configure()
        .timeout(INLINE_DEADLINE)
        .diff_words(old, new);
    if diff.ratio() < MIN_INLINE_RATIO {
        return None;
    }
    let old_starts = token_starts(diff.iter_old_slices());
    let new_starts = token_starts(diff.iter_new_slices());
    let bytes = |starts: &[usize], tokens: Range<usize>| {
        let start = starts.get(tokens.start).copied().unwrap_or_default();
        let end = starts.get(tokens.end).copied().unwrap_or(start);
        start..end
    };
    let mut spans = InlineSpans::default();
    for op in diff.ops() {
        let (tag, old_tokens, new_tokens) = op.as_tag_tuple();
        if tag == DiffTag::Equal {
            continue;
        }
        push_span(&mut spans.left, bytes(&old_starts, old_tokens));
        push_span(&mut spans.right, bytes(&new_starts, new_tokens));
    }
    Some(spans)
}

/// [`inline_spans`] of two lines without their leading and trailing
/// whitespace, as byte ranges into the untrimmed lines.
fn trimmed_inline_spans(old: &str, new: &str) -> Option<InlineSpans> {
    let lead = |line: &str| line.len() - line.trim_start().len();
    let shift = |spans: Vec<Range<usize>>, by: usize| -> Vec<Range<usize>> {
        spans
            .into_iter()
            .map(|span| span.start + by..span.end + by)
            .collect()
    };
    let spans = inline_spans(old.trim(), new.trim())?;
    Some(InlineSpans {
        left: shift(spans.left, lead(old)),
        right: shift(spans.right, lead(new)),
    })
}

/// The byte offset each token starts at, and the end of the last one.
fn token_starts<'a>(tokens: impl Iterator<Item = &'a str>) -> Vec<usize> {
    let mut starts = vec![0];
    let mut at = 0;
    for token in tokens {
        at += token.len();
        starts.push(at);
    }
    starts
}

/// Adds `span`, merged into the last span when they touch.
fn push_span(spans: &mut Vec<Range<usize>>, span: Range<usize>) {
    if span.is_empty() {
        return;
    }
    match spans.last_mut() {
        Some(last) if last.end == span.start => last.end = span.end,
        _ => spans.push(span),
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;

    fn build(old: &str, new: &str) -> DiffModel {
        DiffModel::build(old, new, DiffOptions::default())
    }

    /// A list of just `range`.
    fn one(range: Range<usize>) -> Vec<Range<usize>> {
        vec![range]
    }

    fn kinds(model: &DiffModel) -> Vec<RowKind> {
        model.rows().iter().map(|row| row.kind).collect()
    }

    /// Each row's (left, right) line numbers.
    fn numbers(model: &DiffModel) -> Vec<(Option<u32>, Option<u32>)> {
        model
            .rows()
            .iter()
            .map(|row| {
                (
                    row.left.as_ref().map(|s| s.line_no),
                    row.right.as_ref().map(|s| s.line_no),
                )
            })
            .collect()
    }

    #[test]
    fn two_empty_texts_have_no_rows_and_no_changes() {
        let model = build("", "");
        assert!(model.rows().is_empty());
        assert_eq!(model.change_count(), 0);
        assert_eq!(model.next_hunk(0), None);
        assert_eq!(model.prev_hunk(0), None);
        assert_eq!(model.first_hunk(), None);
        assert_eq!(model.last_hunk(), None);
    }

    #[test]
    fn an_added_file_is_all_inserts() {
        let model = build("", "a\nb\nc\n");
        assert_eq!(kinds(&model), [RowKind::Insert; 3]);
        assert_eq!(
            numbers(&model),
            [(None, Some(1)), (None, Some(2)), (None, Some(3))]
        );
        assert_eq!(model.hunks(), one(0..3));
    }

    #[test]
    fn a_deleted_file_is_all_deletes() {
        let model = build("a\nb\n", "");
        assert_eq!(kinds(&model), [RowKind::Delete; 2]);
        assert_eq!(numbers(&model), [(Some(1), None), (Some(2), None)]);
        assert_eq!(model.change_count(), 1);
    }

    #[test]
    fn a_trailing_newline_on_one_side_is_no_change() {
        let model = build("a\nb", "a\nb\n");
        assert_eq!(kinds(&model), [RowKind::Equal; 2]);
        assert_eq!(model.change_count(), 0);
    }

    #[test]
    fn crlf_and_lf_lines_with_the_same_text_are_equal() {
        let model = build("one\r\ntwo\r\n", "one\ntwo\n");
        assert_eq!(kinds(&model), [RowKind::Equal; 2]);
        let row = model.rows().first().expect("a row");
        let left = row.left.as_ref().expect("an old line");
        assert_eq!(model.old_text(left), "one", "the \\r is not shown");
    }

    #[test]
    fn a_whitespace_only_change_is_a_hunk_unless_trim_whitespace_is_ignored() {
        let (old, new) = ("fn a() {\n    x();\n}\n", "fn a() {\n  x();  \n}\n");
        let exact = build(old, new);
        assert_eq!(
            kinds(&exact),
            [RowKind::Equal, RowKind::Modify, RowKind::Equal]
        );
        assert_eq!(exact.change_count(), 1);

        let ignoring = DiffModel::build(
            old,
            new,
            DiffOptions {
                ignore_trim_whitespace: true,
            },
        );
        assert_eq!(kinds(&ignoring), [RowKind::Equal; 3]);
        assert_eq!(ignoring.change_count(), 0);
        let row = ignoring.rows().get(1).expect("the middle row");
        let right = row.right.as_ref().expect("a new line");
        assert_eq!(
            ignoring.new_text(right),
            "  x();  ",
            "compared trimmed, shown as it is"
        );
    }

    #[test]
    fn inner_whitespace_still_counts_when_trim_whitespace_is_ignored() {
        let opts = DiffOptions {
            ignore_trim_whitespace: true,
        };
        let model = DiffModel::build("a b\n", "a  b\n", opts);
        assert_eq!(kinds(&model), [RowKind::Modify]);
    }

    #[test]
    fn one_very_long_line_builds_one_row() {
        let old = "word ".repeat(20_000);
        let new = format!("{old}tail");
        let model = build(&old, &new);
        assert_eq!(kinds(&model), [RowKind::Modify]);
        assert_eq!(model.inline(0), None, "too long to word-diff");
    }

    #[test]
    fn a_modified_row_with_a_side_over_the_cap_marks_no_words() {
        let long = format!("{} end", "word ".repeat(1000));
        assert!(long.len() > 5000);
        let model = build(
            &format!("{long}\nshort one\n"),
            &format!("{long}!\nshort two\n"),
        );
        assert_eq!(kinds(&model), [RowKind::Modify, RowKind::Modify]);
        assert_eq!(
            model.inline(0),
            None,
            "a side over the cap is not word-diffed"
        );
        let spans = model
            .inline(1)
            .expect("a short modified row marks its words");
        assert_eq!(spans.left, one(6..9), "one");
        assert_eq!(spans.right, one(6..9), "two");
    }

    #[test]
    fn trim_whitespace_marks_words_without_the_removed_indent() {
        let opts = DiffOptions {
            ignore_trim_whitespace: true,
        };
        let model = DiffModel::build("  let x = 1;\n", "let x = 2;\n", opts);
        assert_eq!(kinds(&model), [RowKind::Modify]);
        let spans = model.inline(0).expect("similar lines mark their words");
        assert_eq!(
            spans.left,
            one(10..12),
            "`1;` in the old line, past its indent"
        );
        assert_eq!(spans.right, one(8..10), "`2;`");
    }

    #[test]
    fn a_multi_byte_word_change_marks_whole_characters() {
        let model = build("naïve café\n", "naïve cafés\n");
        let spans = model.inline(0).expect("similar lines mark their words");
        let new = "naïve cafés";
        assert_eq!(spans.right, one(7..13));
        assert_eq!(new.get(7..13), Some("cafés"), "on char boundaries");
        assert_eq!(spans.left, one(7..12), "café");
    }

    #[test]
    fn an_expired_deadline_still_gives_a_correct_diff() {
        let old = ["a", "b", "c", "d", "e"];
        let new = ["a", "x", "c", "e", "f"];
        let ops = line_ops(&old, &new, Instant::now());
        let (mut old_at, mut new_at) = (0, 0);
        let mut rebuilt: Vec<&str> = Vec::new();
        for op in &ops {
            let (tag, old_range, new_range) = op.as_tag_tuple();
            assert_eq!(old_range.start, old_at, "every old line once, in order");
            assert_eq!(new_range.start, new_at, "every new line once, in order");
            let (old_lines, new_lines) = (
                old.get(old_range.clone()).unwrap_or_default(),
                new.get(new_range.clone()).unwrap_or_default(),
            );
            match tag {
                DiffTag::Equal => {
                    assert_eq!(old_lines, new_lines, "equal lines are equal");
                    rebuilt.extend_from_slice(old_lines);
                }
                DiffTag::Delete => {}
                DiffTag::Insert | DiffTag::Replace => rebuilt.extend_from_slice(new_lines),
            }
            (old_at, new_at) = (old_range.end, new_range.end);
        }
        assert_eq!((old_at, new_at), (old.len(), new.len()));
        assert_eq!(rebuilt, new, "applying the ops rebuilds the new text");
    }

    #[test]
    fn a_final_newline_change_has_no_hunk_but_is_flagged() {
        let model = build("a\nb", "a\nb\n");
        assert_eq!(model.change_count(), 0);
        assert!(model.final_newline_changed());
        assert!(!model.line_endings_changed());
    }

    #[test]
    fn a_line_ending_change_has_no_hunk_but_is_flagged() {
        let model = build("one\r\ntwo\r\n", "one\ntwo\n");
        assert_eq!(model.change_count(), 0);
        assert!(model.line_endings_changed());
        assert!(!model.final_newline_changed());
    }

    #[test]
    fn identical_texts_flag_no_invisible_change() {
        let model = build("a\r\nb\n", "a\r\nb\n");
        assert!(!model.line_endings_changed());
        assert!(!model.final_newline_changed());
    }

    #[test]
    fn a_replace_pairs_lines_as_modified_and_the_excess_is_deleted() {
        let model = build("keep\nd1\nd2\nd3\nkeep2\n", "keep\ni1\ni2\nkeep2\n");
        assert_eq!(
            kinds(&model),
            [
                RowKind::Equal,
                RowKind::Modify,
                RowKind::Modify,
                RowKind::Delete,
                RowKind::Equal
            ]
        );
        assert_eq!(
            numbers(&model),
            [
                (Some(1), Some(1)),
                (Some(2), Some(2)),
                (Some(3), Some(3)),
                (Some(4), None),
                (Some(5), Some(4)),
            ]
        );
        assert_eq!(model.hunks(), one(1..4));
    }

    #[test]
    fn a_replace_with_more_new_lines_ends_in_inserts() {
        let model = build("x\n", "y1\ny2\n");
        assert_eq!(kinds(&model), [RowKind::Modify, RowKind::Insert]);
        assert_eq!(numbers(&model), [(Some(1), Some(1)), (None, Some(2))]);
    }

    #[test]
    fn a_one_word_change_marks_that_word_on_each_side() {
        let model = build("the quick brown fox\n", "the quick red fox\n");
        let spans = model.inline(0).expect("similar lines mark their words");
        assert_eq!(spans.left, one(10..15), "brown");
        assert_eq!(spans.right, one(10..13), "red");
    }

    #[test]
    fn dissimilar_lines_mark_no_words() {
        let model = build("alpha beta gamma\n", "one two three\n");
        assert_eq!(kinds(&model), [RowKind::Modify]);
        assert_eq!(model.inline(0), None);
    }

    #[test]
    fn rows_that_are_not_modified_mark_no_words() {
        let model = build("a\nb\n", "a\n");
        assert_eq!(model.inline(0), None, "equal");
        assert_eq!(model.inline(1), None, "deleted");
        assert_eq!(model.inline(9), None, "past the end");
    }

    #[test]
    fn hunks_wrap_in_both_directions() {
        let old = "a\nb\nc\nd\ne\nf\ng\n";
        let new = "a\nB\nc\nd\nE\nf\nG\n";
        let model = build(old, new);
        assert_eq!(model.hunks(), [1..2, 4..5, 6..7]);
        assert_eq!(model.first_hunk(), Some(0));
        assert_eq!(model.last_hunk(), Some(2));
        assert_eq!(model.next_hunk(0), Some(0));
        assert_eq!(model.next_hunk(1), Some(1));
        assert_eq!(model.next_hunk(4), Some(2));
        assert_eq!(
            model.next_hunk(6),
            Some(0),
            "past the last wraps to the first"
        );
        assert_eq!(model.prev_hunk(6), Some(1));
        assert_eq!(model.prev_hunk(4), Some(0));
        assert_eq!(
            model.prev_hunk(1),
            Some(2),
            "before the first wraps to the last"
        );
    }

    /// A Rust-like source of `lines` lines.
    fn rust_like(lines: usize) -> Vec<String> {
        (0..lines)
            .map(|i| match i % 6 {
                0 => format!("fn item_{i}(value: u32) -> u32 {{"),
                1 => format!("    let total = value * {i} + {};", i % 13),
                2 => format!("    // step {i}: fold the running total"),
                3 => format!("    let next = total.wrapping_add({});", i % 7),
                4 => "    next".to_owned(),
                _ => "}".to_owned(),
            })
            .collect()
    }

    /// `base` with `hunks` evenly spaced changes: three lines edited in
    /// each, and an inserted or a deleted line in every other.
    fn scattered_changes(base: &[String], hunks: usize) -> Vec<String> {
        let stride = base.len() / hunks;
        let mut out = Vec::with_capacity(base.len() + hunks);
        for (i, line) in base.iter().enumerate() {
            let hunk = i / stride;
            let at = if hunk < hunks { i % stride } else { 0 };
            match at {
                10..=12 => out.push(format!("{line} // edited")),
                13 if hunk.is_multiple_of(2) => {
                    out.push(format!("    // inserted in hunk {hunk}"));
                    out.push(line.clone());
                }
                13 => {}
                _ => out.push(line.clone()),
            }
        }
        out
    }

    fn join(lines: &[String]) -> String {
        let mut text = lines.join("\n");
        text.push('\n');
        text
    }

    fn time_build(old: &str, new: &str, algorithm: Option<Algorithm>) -> (Duration, DiffModel) {
        let started = Instant::now();
        let model = match algorithm {
            None => DiffModel::build(old, new, DiffOptions::default()),
            Some(algorithm) => DiffModel::build_with(old, new, DiffOptions::default(), |o, n| {
                capture_diff_slices_deadline(algorithm, o, n, Some(Instant::now() + DIFF_DEADLINE))
            }),
        };
        (started.elapsed(), model)
    }

    #[test]
    #[ignore = "spike timing: run with --release -- --ignored --nocapture"]
    #[expect(clippy::print_stderr, reason = "spike timing output")]
    fn spike_build_timings() {
        let base = rust_like(5_000);
        let old = join(&base);
        let scattered = join(&scattered_changes(&base, 60));
        let all: Vec<String> = base.iter().map(|line| format!("{line} // all")).collect();
        let all = join(&all);
        for (name, new) in [("5k, 60 hunks", &scattered), ("5k, every line", &all)] {
            let (patience, model) = time_build(&old, new, None);
            let (myers, myers_model) = time_build(&old, new, Some(Algorithm::Myers));
            if name.starts_with("5k, 60") {
                assert_eq!(model.change_count(), 60, "the fixture has 60 hunks");
            }
            let changed = model
                .rows()
                .iter()
                .filter(|row| row.kind != RowKind::Equal)
                .count();
            let started = Instant::now();
            let marked = (0..model.rows().len())
                .filter(|&row| model.inline(row).is_some())
                .count();
            let inline = started.elapsed();
            eprintln!(
                "{name}: rows {}, changed rows {changed}, hunks {} (myers {}); \
                 build patience {patience:?}, myers {myers:?}; \
                 inline for all {marked} marked rows {inline:?}",
                model.rows().len(),
                model.change_count(),
                myers_model.change_count(),
            );
        }
    }
}
