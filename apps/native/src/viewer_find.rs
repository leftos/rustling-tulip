//! Find in the viewer's Source view, without the UI: every occurrence of a
//! plain query in the lines, case-insensitive unless asked, the match the
//! find bar is on and the steps between matches, and the merge of a line's
//! syntax colours with its match tints into the segments it is drawn in.

use std::ops::Range;
use std::sync::Arc;

/// One occurrence of the query: its line, counted from zero, and its byte
/// range in that line, on character boundaries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Match {
    pub line: usize,
    pub range: Range<usize>,
}

/// Every occurrence of `query` in `lines`, line by line and in order along
/// each line, none overlapping; none for an empty query.
pub(crate) fn find_matches<'a>(
    lines: impl IntoIterator<Item = &'a str>,
    query: &str,
    case_sensitive: bool,
) -> Vec<Match> {
    if query.is_empty() {
        return Vec::new();
    }
    let folded_query = (!case_sensitive).then(|| fold(query));
    let mut matches = Vec::new();
    for (line, text) in lines.into_iter().enumerate() {
        let hits = match &folded_query {
            None => text
                .match_indices(query)
                .map(|(start, hit)| start..start + hit.len())
                .collect(),
            Some(folded_query) => folded_hits(text, folded_query),
        };
        matches.extend(hits.into_iter().map(|range| Match { line, range }));
    }
    matches
}

/// `text` with every character lower-cased on its own, so the fold of a
/// line and of a query agree character by character.
fn fold(text: &str) -> String {
    text.chars().flat_map(char::to_lowercase).collect()
}

/// The byte ranges of `line` whose fold holds `folded_query`, widened to the
/// whole characters they fold from, in order and none overlapping. A fold
/// can change a character's length ("İ" folds to three bytes), so each
/// byte of the fold keeps the range of the character it came from. An
/// all-ASCII line folds byte for byte and needs no such map.
fn folded_hits(line: &str, folded_query: &str) -> Vec<Range<usize>> {
    if line.is_ascii() {
        return line
            .to_ascii_lowercase()
            .match_indices(folded_query)
            .map(|(start, hit)| start..start + hit.len())
            .collect();
    }
    let mut folded = String::with_capacity(line.len());
    let mut source: Vec<Range<usize>> = Vec::with_capacity(line.len());
    for (start, c) in line.char_indices() {
        let from = start..start + c.len_utf8();
        for lower in c.to_lowercase() {
            folded.push(lower);
            source.extend(std::iter::repeat_n(from.clone(), lower.len_utf8()));
        }
    }
    let mut hits: Vec<Range<usize>> = Vec::new();
    for (at, hit) in folded.match_indices(folded_query) {
        let first = source.get(at);
        let last = (at + hit.len())
            .checked_sub(1)
            .and_then(|end| source.get(end));
        let (Some(first), Some(last)) = (first, last) else {
            continue;
        };
        if hits.last().is_some_and(|before| first.start < before.end) {
            continue;
        }
        hits.push(first.start..last.end);
    }
    hits
}

/// The find bar's state: the query, whether case counts, the matches and
/// the one the bar is on.
#[derive(Debug, Default)]
pub(crate) struct FindState {
    query: String,
    case_sensitive: bool,
    matches: Arc<[Match]>,
    current: Option<usize>,
}

impl FindState {
    pub(crate) fn query(&self) -> &str {
        &self.query
    }

    pub(crate) fn case_sensitive(&self) -> bool {
        self.case_sensitive
    }

    pub(crate) fn matches(&self) -> &Arc<[Match]> {
        &self.matches
    }

    pub(crate) fn current_match(&self) -> Option<&Match> {
        self.matches.get(self.current?)
    }

    /// The query became `query`: its matches in `lines`, the bar on the
    /// first at or after the line of the match it was on, or with none on
    /// line `top`, the top line in view; with none there on the first.
    pub(crate) fn set_query<'a>(
        &mut self,
        query: &str,
        lines: impl IntoIterator<Item = &'a str>,
        top: usize,
    ) {
        let from = self.current_match().map_or(top, |found| found.line);
        query.clone_into(&mut self.query);
        self.search(lines, from);
    }

    /// Case counts, or stops counting, from now: the matches in `lines`
    /// again, the bar placed as [`Self::set_query`] places it.
    pub(crate) fn set_case_sensitive<'a>(
        &mut self,
        case_sensitive: bool,
        lines: impl IntoIterator<Item = &'a str>,
        top: usize,
    ) {
        let from = self.current_match().map_or(top, |found| found.line);
        self.case_sensitive = case_sensitive;
        self.search(lines, from);
    }

    /// The text became `lines`: the same query's matches in it, the bar on
    /// the first at or after the line the old match was on.
    pub(crate) fn rerun<'a>(&mut self, lines: impl IntoIterator<Item = &'a str>) {
        let from = self.current_match().map_or(0, |found| found.line);
        self.search(lines, from);
    }

    fn search<'a>(&mut self, lines: impl IntoIterator<Item = &'a str>, from_line: usize) {
        self.matches = find_matches(lines, &self.query, self.case_sensitive).into();
        let at_or_after = self
            .matches
            .iter()
            .position(|found| found.line >= from_line);
        self.current = at_or_after.or((!self.matches.is_empty()).then_some(0));
    }

    /// Moves the bar to the next match, past the last onto the first.
    pub(crate) fn next(&mut self) -> Option<&Match> {
        let count = self.matches.len();
        self.current = match self.current {
            _ if count == 0 => None,
            Some(at) => Some((at + 1) % count),
            None => Some(0),
        };
        self.current_match()
    }

    /// Moves the bar to the previous match, before the first onto the last.
    pub(crate) fn prev(&mut self) -> Option<&Match> {
        let count = self.matches.len();
        self.current = match self.current {
            _ if count == 0 => None,
            Some(at) => Some((at + count - 1) % count),
            None => Some(count - 1),
        };
        self.current_match()
    }

    /// The bar's count, "n of m": blank for an empty query and "0 of 0"
    /// for one with no match.
    pub(crate) fn label(&self) -> String {
        if self.query.is_empty() {
            return String::new();
        }
        let at = self.current.map_or(0, |at| at + 1);
        format!("{at} of {}", self.matches.len())
    }
}

/// The matches of `matches`, sorted by line, that lie on line `line`.
pub(crate) fn on_line(matches: &[Match], line: usize) -> &[Match] {
    let start = matches.partition_point(|found| found.line < line);
    let end = matches.partition_point(|found| found.line <= line);
    matches.get(start..end).unwrap_or_default()
}

/// How a segment of a line is tinted for find.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tint {
    None,
    Match,
    Current,
}

/// A run of a line drawn one way: its syntax class, if any, and its tint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Segment<S> {
    pub range: Range<usize>,
    pub syntax: Option<S>,
    pub tint: Tint,
}

#[cfg(test)]
thread_local! {
    /// How many ranges the merge looked at, so a test can bound its work.
    static LOOKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Counts one range the merge looked at; nothing outside tests.
fn looked() {
    #[cfg(test)]
    LOOKS.with(|looks| looks.set(looks.get() + 1));
}

/// A line of `len` bytes cut into the segments it is drawn in, in order,
/// none overlapping and together covering the line: each takes the class of
/// the `syntax` span it lies in, and the [`Tint::Current`] tint inside
/// `current`, else [`Tint::Match`] inside one of `matches`. Neighbours drawn
/// alike are one segment; ranges past the line are cut at its end.
///
/// `syntax` and `matches` are each sorted with none overlapping, so one
/// pass with an index into each does it, in time linear in their lengths.
pub(crate) fn merge_segments<S: Copy + PartialEq>(
    len: usize,
    syntax: &[(Range<usize>, S)],
    matches: &[Range<usize>],
    current: Option<&Range<usize>>,
) -> Vec<Segment<S>> {
    let (mut next_span, mut next_match) = (0, 0);
    let mut segments: Vec<Segment<S>> = Vec::new();
    let mut start = 0;
    while start < len {
        next_span = skip_ended(syntax, next_span, start, |(range, _)| range);
        next_match = skip_ended(matches, next_match, start, |range| range);
        let span = syntax.get(next_span);
        let hit = matches.get(next_match);
        let end = [
            boundary(span.map(|(range, _)| range), start),
            boundary(hit, start),
            boundary(current, start),
        ]
        .into_iter()
        .fold(len, usize::min);
        let class = span
            .filter(|(range, _)| range.start <= start)
            .map(|(_, class)| *class);
        let tint = if current.is_some_and(|range| range.contains(&start)) {
            Tint::Current
        } else if hit.is_some_and(|range| range.start <= start) {
            Tint::Match
        } else {
            Tint::None
        };
        match segments.last_mut() {
            Some(last) if last.syntax == class && last.tint == tint => last.range.end = end,
            _ => segments.push(Segment {
                range: start..end,
                syntax: class,
                tint,
            }),
        }
        start = end;
    }
    segments
}

/// The index of the first of `items`, from `at` on, whose range (as `range`
/// reads it) has not ended by byte `start`.
fn skip_ended<T>(
    items: &[T],
    mut at: usize,
    start: usize,
    range: impl Fn(&T) -> &Range<usize>,
) -> usize {
    while items.get(at).is_some_and(|item| {
        looked();
        range(item).end <= start
    }) {
        at += 1;
    }
    at
}

/// The first byte past `at` where `range` starts or ends; `usize::MAX`
/// with no range or one that ended.
fn boundary(range: Option<&Range<usize>>, at: usize) -> usize {
    match range {
        Some(range) if range.start > at => range.start,
        Some(range) if range.end > at => range.end,
        _ => usize::MAX,
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;

    fn lines_and_ranges(matches: &[Match]) -> Vec<(usize, Range<usize>)> {
        matches
            .iter()
            .map(|found| (found.line, found.range.clone()))
            .collect()
    }

    /// The text of each match in `line`.
    fn texts<'a>(line: &'a str, matches: &[Match]) -> Vec<&'a str> {
        matches
            .iter()
            .map(|found| {
                line.get(found.range.clone())
                    .expect("a match is on char boundaries")
            })
            .collect()
    }

    /// The index of the match `state`'s bar is on.
    fn at(state: &FindState) -> Option<usize> {
        let current = state.current_match()?;
        state.matches().iter().position(|found| found == current)
    }

    /// Four matches of `needle`: lines 0, 2 (twice) and 5.
    const VIEW: [&str; 6] = ["needle", "x", "needle needle", "x", "x", "needle"];

    fn segment(range: Range<usize>, syntax: Option<char>, tint: Tint) -> Segment<char> {
        Segment {
            range,
            syntax,
            tint,
        }
    }

    #[test]
    fn find_matches_are_case_insensitive_by_default() {
        let lines = ["Hello hello", "HELLO", "help"];
        assert_eq!(
            lines_and_ranges(&find_matches(lines, "hello", false)),
            [(0, 0..5), (0, 6..11), (1, 0..5)]
        );
        let mut state = FindState::default();
        assert!(!state.case_sensitive(), "case does not count at first");
        state.set_query("HELLO", lines, 0);
        assert_eq!(state.matches().len(), 3);
    }

    #[test]
    fn find_matches_can_be_case_sensitive() {
        let lines = ["Hello hello", "HELLO"];
        assert_eq!(
            lines_and_ranges(&find_matches(lines, "hello", true)),
            [(0, 6..11)]
        );
        let mut state = FindState::default();
        state.set_query("HELLO", lines, 0);
        state.set_case_sensitive(true, lines, 0);
        assert_eq!(lines_and_ranges(state.matches()), [(1, 0..5)]);
        assert_eq!(state.label(), "1 of 1");
    }

    #[test]
    fn an_empty_query_matches_nothing() {
        assert_eq!(find_matches(["abc", ""], "", false), Vec::<Match>::new());
        let mut state = FindState::default();
        state.set_query("", ["abc"], 0);
        assert_eq!(state.label(), "", "an empty query shows no count");
        assert_eq!(state.next(), None);
        state.set_query("zzz", ["abc"], 0);
        assert_eq!(state.label(), "0 of 0", "a query with no hit");
        assert_eq!(at(&state), None);
    }

    #[test]
    fn a_line_with_two_hits_gives_two_matches_in_order() {
        assert_eq!(
            lines_and_ranges(&find_matches(["abab ab", "x", "AB"], "ab", false)),
            [(0, 0..2), (0, 2..4), (0, 5..7), (2, 0..2)]
        );
        assert_eq!(
            lines_and_ranges(&find_matches(["aaa"], "aa", true)),
            [(0, 0..2)],
            "occurrences do not overlap"
        );
    }

    #[test]
    fn matches_are_byte_ranges_on_char_boundaries() {
        let line = "żółw ÉCOLE école 日本";
        let found = find_matches([line], "école", false);
        assert_eq!(texts(line, &found), ["ÉCOLE", "école"]);
        assert_eq!(texts(line, &find_matches([line], "日本", false)), ["日本"]);

        // "İ" folds to "i̇", three bytes for its two.
        let turkish = "İstanbul İİ";
        let stan = find_matches([turkish], "stan", false);
        assert_eq!(lines_and_ranges(&stan), [(0, 2..6)]);
        assert_eq!(texts(turkish, &stan), ["stan"]);
        let dotted = find_matches([turkish], "i", false);
        assert_eq!(texts(turkish, &dotted), ["İ", "İ", "İ"]);
        assert_eq!(
            lines_and_ranges(&find_matches([turkish], "İ", true)),
            [(0, 0..2), (0, 10..12), (0, 12..14)]
        );
    }

    #[test]
    fn next_wraps_at_the_last_match() {
        let mut state = FindState::default();
        state.set_query("needle", VIEW, 0);
        assert_eq!(at(&state), Some(0));
        assert_eq!(state.label(), "1 of 4");
        for _ in 0..3 {
            state.next();
        }
        assert_eq!(state.label(), "4 of 4");
        let wrapped = state.next().map(|found| found.line);
        assert_eq!(wrapped, Some(0), "past the last match is the first");
        assert_eq!(state.label(), "1 of 4");
    }

    #[test]
    fn prev_wraps_at_the_first_match() {
        let mut state = FindState::default();
        state.set_query("needle", VIEW, 0);
        let wrapped = state.prev().cloned();
        assert_eq!(
            wrapped,
            Some(Match {
                line: 5,
                range: 0..6
            }),
            "before the first match is the last"
        );
        assert_eq!(state.label(), "4 of 4");
        state.prev();
        assert_eq!(
            state.current_match().map(|found| found.range.clone()),
            Some(7..13)
        );
    }

    #[test]
    fn next_after_a_changed_query_starts_at_the_first_match_at_or_after_the_view() {
        let mut state = FindState::default();
        state.set_query("needle", VIEW, 1);
        assert_eq!(at(&state), Some(1), "line 2's first match");
        assert_eq!(state.next().map(|found| found.range.clone()), Some(7..13));
        let mut fresh = FindState::default();
        fresh.set_query("needl", VIEW, 3);
        assert_eq!(at(&fresh), Some(3), "line 5's match, at or after line 3");
        let mut on_top = FindState::default();
        on_top.set_query("needle", VIEW, 2);
        assert_eq!(at(&on_top), Some(1), "a match on the top line counts");
        let mut below = FindState::default();
        below.set_query("needle", VIEW, 6);
        assert_eq!(at(&below), Some(0), "none below the view: the first");
        let mut lost = FindState::default();
        lost.set_query("zzz", VIEW, 0);
        lost.set_query("needle", VIEW, 3);
        assert_eq!(at(&lost), Some(3), "with no match before, the view decides");
    }

    #[test]
    fn a_changed_query_keeps_the_line_of_the_current_match() {
        let mut state = FindState::default();
        state.set_query("needle", VIEW, 0);
        state.next();
        state.next();
        assert_eq!(at(&state), Some(2), "line 2's second match");
        state.set_query("needl", VIEW, 5);
        assert_eq!(
            state.current_match().map(|found| found.line),
            Some(2),
            "the old match's line decides, not the view"
        );
        assert_eq!(at(&state), Some(1), "the first match on that line");
    }

    #[test]
    fn merge_is_linear_on_a_line_with_many_matches() {
        let len = 10_000;
        let matches: Vec<Range<usize>> = (0..len).map(|at| at..at + 1).collect();
        let syntax: Vec<(Range<usize>, char)> =
            (0..len / 2).map(|at| (at * 2..at * 2 + 1, 'k')).collect();
        let current = 5..6;
        LOOKS.with(|looks| looks.set(0));
        let segments = merge_segments(len, &syntax, &matches, Some(&current));
        let looks = LOOKS.with(std::cell::Cell::get);
        assert!(
            looks <= 4 * (len + syntax.len()),
            "{looks} range looks for {len} matches and {} spans",
            syntax.len()
        );
        assert_eq!(
            segments.len(),
            len,
            "alternate bytes coloured: one segment each"
        );

        let big = 1 << 20;
        let every_byte: Vec<Range<usize>> = (0..big).map(|at| at..at + 1).collect();
        assert_eq!(
            merge_segments::<char>(big, &[], &every_byte, None),
            [segment(0..big, None, Tint::Match)],
            "a megabyte of matches is one tinted segment"
        );
    }

    #[test]
    fn find_state_survives_a_text_change_by_rerunning() {
        let mut state = FindState::default();
        state.set_query("needle", VIEW, 0);
        state.next();
        state.next();
        assert_eq!(state.current_match().map(|found| found.line), Some(2));

        state.rerun(["x", "x", "x", "Needle", "needle"]);
        assert_eq!(state.query(), "needle", "the query is kept");
        assert_eq!(lines_and_ranges(state.matches()), [(3, 0..6), (4, 0..6)]);
        assert_eq!(
            state.current_match().map(|found| found.line),
            Some(3),
            "the first match at or after the old one's line"
        );
        assert_eq!(state.label(), "1 of 2");

        state.rerun(["nothing here"]);
        assert_eq!(state.label(), "0 of 0");
        assert_eq!(at(&state), None);
    }

    #[test]
    fn merged_segments_never_overlap_and_cover_the_line() {
        let syntax = [(0..3, 'k'), (4..9, 's'), (9..12, 'c'), (12..20, 'x')];
        let matches = [2..6, 10..14];
        let segments = merge_segments(14, &syntax, &matches, None);
        assert_eq!(segments.first().map(|first| first.range.start), Some(0));
        assert_eq!(segments.last().map(|last| last.range.end), Some(14));
        for pair in segments.windows(2) {
            if let [before, after] = pair {
                assert_eq!(before.range.end, after.range.start, "{segments:?}");
            }
        }
        assert!(segments.iter().all(|seg| !seg.range.is_empty()));
        assert_eq!(
            segments,
            [
                segment(0..2, Some('k'), Tint::None),
                segment(2..3, Some('k'), Tint::Match),
                segment(3..4, None, Tint::Match),
                segment(4..6, Some('s'), Tint::Match),
                segment(6..9, Some('s'), Tint::None),
                segment(9..10, Some('c'), Tint::None),
                segment(10..12, Some('c'), Tint::Match),
                segment(12..14, Some('x'), Tint::Match),
            ]
        );
        assert_eq!(
            merge_segments::<char>(0, &[], &[], None),
            Vec::<Segment<char>>::new()
        );
        assert_eq!(
            merge_segments::<char>(5, &[], &[], None),
            [segment(0..5, None, Tint::None)],
            "a line with nothing on it is one plain segment"
        );
    }

    #[test]
    fn the_current_match_wins_over_a_plain_match_tint() {
        let current = 6..9;
        let segments = merge_segments(9, &[(0..9, 'k')], &[0..4, 6..9], Some(&current));
        assert_eq!(
            segments,
            [
                segment(0..4, Some('k'), Tint::Match),
                segment(4..6, Some('k'), Tint::None),
                segment(6..9, Some('k'), Tint::Current),
            ]
        );
        let lines = ["ab ab"];
        let mut state = FindState::default();
        state.set_query("ab", lines, 0);
        state.next();
        let found = on_line(state.matches(), 0);
        assert_eq!(found.len(), 2);
        assert_eq!(
            state.current_match().map(|hit| hit.range.clone()),
            Some(3..5)
        );
    }
}
