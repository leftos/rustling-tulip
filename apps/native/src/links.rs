//! The links a terminal grid holds: URLs, and file paths with an optional
//! `:line:column` suffix.
//!
//! A row arrives as the text of its cells — the spacer cell behind a wide
//! glyph left out, the blank cells past the row's text kept as spaces — with a
//! map of the grid column each character starts in, so that a wide glyph
//! leaves character indices and columns as two different things. Rows are
//! grouped into the logical lines a link can span: a soft wrap joins the row
//! below as it is, and a hard line break a TUI emitted mid-path is stitched
//! back together when both sides of the break look like one path. Every stitch
//! is a boundary a reading of the path can stop at, so an over-eager stitch
//! degrades into the fragment that was on screen.

#![cfg_attr(not(test), expect(dead_code, reason = "only the tests call these"))]

use std::sync::LazyLock;

use regex::Regex;

/// What a link points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    Url,
    Path,
}

/// One grid row as the detector sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalRow {
    /// The row's characters in order, one per cell that holds a glyph.
    pub text: String,
    /// The grid column each character of `text` starts in, one entry each.
    pub columns: Vec<u16>,
    /// This row continues the one above it — a soft wrap.
    pub is_wrapped: bool,
}

/// One reading of a detected path: the whole stitched path, or a fragment of
/// it that stops at a stitch boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalLinkCandidate {
    pub path: String,
    pub line: Option<u32>,
    pub column: Option<u32>,
}

/// One link, as the rows it was matched in describe it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalLink {
    pub kind: LinkKind,
    /// The matched text, stitching included.
    pub text: String,
    /// `candidates[0].path` — the longest reading.
    pub target: String,
    pub line: Option<u32>,
    pub column: Option<u32>,
    /// The readings a caller may open, longest first. One entry unless the
    /// link crosses a hard stitch; each extra entry truncates at one stitch
    /// boundary.
    pub candidates: Vec<TerminalLinkCandidate>,
    /// Character offsets into the assembled line the link was matched in.
    pub start_index: usize,
    pub end_index: usize,
    /// Row indices into the `rows` array the link was matched in. A caller
    /// reading the grid gets grid lines instead, where a line in the history
    /// is negative. Columns are 0-based, `end_column` exclusive.
    pub start_row: i32,
    pub start_column: usize,
    pub end_row: i32,
    pub end_column: usize,
}

static URL_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| compile(r#"(?i)(?-u:\b)https?://[^\s<>"'`{}|\\^]+"#));
static PATH_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    compile(
        r#"(^|[\s(\[{<])((?:[A-Za-z]:[\\/]|\\\\[^\s\\/:*?"<>|]+[\\/][^\s\\/:*?"<>|]+[\\/]|\/|\.{1,2}[\\/]|[A-Za-z0-9_.-]+[\\/])[^\s<>"'`|]*)"#,
    )
});
static TRAILING_PUNCTUATION: LazyLock<Regex> = LazyLock::new(|| compile(r"[),.;!?}\]]+$"));
static LINE_COLUMN_SUFFIX: LazyLock<Regex> =
    LazyLock::new(|| compile(r"^(.*?)(?::([1-9][0-9]{0,6})(?::([1-9][0-9]{0,6}))?)$"));

/// Compiles one of this module's patterns.
#[expect(
    clippy::expect_used,
    reason = "the patterns are literals, and this module's tests match every one of them"
)]
fn compile(source: &str) -> Regex {
    Regex::new(source).expect("link pattern compiles")
}

/// Everything that can sit between a TUI box's edge and its content, plus the
/// pad spaces a row carries past its text.
fn is_frame(c: char) -> bool {
    c.is_whitespace() || is_border(c)
}

/// The frame characters that are an actual drawn border rather than padding.
fn is_border(c: char) -> bool {
    matches!(
        c,
        '│' | '┃' | '║' | '╎' | '╏' | '┆' | '┇' | '┊' | '┋' | '▌' | '▐' | '|'
    )
}

/// A path run continues across a row break only if both sides of the break
/// are characters a path is made of.
fn is_path_continuation(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            '_' | '.' | '-' | '\\' | '/' | '~' | '$' | '%' | '+' | '@' | '#'
        )
}

/// How far short of its wrap column a row may stop and still count as wrapped.
const STITCH_TOLERANCE: usize = 4;
/// Hard stitches allowed in one logical line. Soft wraps are uncapped — the
/// grid already calls those one line.
const STITCH_LIMIT: usize = 4;

/// A row's content with the frame around it removed.
struct Frame<'a> {
    content: &'a str,
    /// Character index in the row where the content starts.
    start: usize,
    /// Character index in the row just past the content.
    end: usize,
}

/// A piece of one row's text as it appears in an assembled logical line.
struct Segment {
    row_index: usize,
    /// Character index in the row where `text` starts.
    char_offset: usize,
    text: String,
    /// The grid column each character of `text` starts in, plus the column
    /// just past its last character.
    columns: Vec<u16>,
    /// This piece was joined on by a hard stitch, so its start is a boundary
    /// a candidate can truncate at.
    stitched: bool,
}

impl Segment {
    /// How many characters the piece contributes.
    fn chars(&self) -> usize {
        self.columns.len().saturating_sub(1)
    }

    /// The grid column character `index` of the piece starts in.
    fn column(&self, index: usize) -> usize {
        self.columns
            .get(index)
            .map_or_else(|| self.end_column(), |column| usize::from(*column))
    }

    /// The grid column just past character `index` of the piece.
    fn column_after(&self, index: usize) -> usize {
        self.columns
            .get(index + 1)
            .map_or_else(|| self.end_column(), |column| usize::from(*column))
    }

    /// The grid column just past the piece's last character.
    fn end_column(&self) -> usize {
        self.columns.last().map_or(0, |column| usize::from(*column))
    }
}

/// Where a character index of an assembled line falls.
struct Location {
    row: usize,
    /// The grid column the character there starts in.
    column: usize,
    /// The grid column just past that character.
    after: usize,
}

/// A column as a [`TerminalRow`] map stores it.
pub(crate) fn column_limit(column: usize) -> u16 {
    u16::try_from(column).unwrap_or(u16::MAX)
}

/// The grid column a character index of `row` starts in, or `cols` one past
/// the row's end.
fn column_of(row: &TerminalRow, index: usize, cols: usize) -> usize {
    row.columns
        .get(index)
        .map_or(cols, |column| usize::from(*column))
}

/// The grid column just past the character at `index` of `row`, which is the
/// second cell of a wide glyph.
fn column_after(row: &TerminalRow, index: usize, cols: usize) -> usize {
    row.columns
        .get(index + 1)
        .map_or(cols, |column| usize::from(*column))
}

/// A whole row's columns, plus the column just past its text.
fn row_columns(row: &TerminalRow, cols: usize) -> Vec<u16> {
    slice_columns(row, 0, row.columns.len(), cols)
}

/// The columns of the characters `start..end` of `row`, plus the column just
/// past them.
fn slice_columns(row: &TerminalRow, start: usize, end: usize, cols: usize) -> Vec<u16> {
    let mut columns = row.columns.get(start..end).unwrap_or_default().to_vec();
    columns.push(column_limit(column_after(row, end.saturating_sub(1), cols)));
    columns
}

/// Every link in one line of text: a single row, or the logical line a group
/// of rows assembles into.
#[must_use]
pub fn detect_links(text: &str) -> Vec<TerminalLink> {
    let chars = chars_len(text);
    detect_segment_links(&[Segment {
        row_index: 0,
        char_offset: 0,
        text: text.to_owned(),
        columns: (0..=chars).map(column_limit).collect(),
        stitched: false,
    }])
}

/// Every link in `rows`, whose text is padded to `cols`.
#[must_use]
pub fn detect_row_links(rows: &[TerminalRow], cols: usize) -> Vec<TerminalLink> {
    let mut links = Vec::new();
    for group in group_rows(rows, cols) {
        links.extend(detect_segment_links(&group));
    }
    links
}

/// Whether `below` continues `above` across a hard line break — a real
/// newline a TUI emitted rather than a soft wrap the grid flagged.
#[must_use]
pub fn can_stitch(above: &TerminalRow, below: &TerminalRow, cols: usize) -> bool {
    if below.is_wrapped {
        return false;
    }
    let a = strip_frame(&above.text);
    let b = strip_frame(&below.text);
    if a.content.is_empty() || b.content.is_empty() {
        return false;
    }
    let tail = a.content.chars().next_back();
    let head = b.content.chars().next();
    if !tail.is_some_and(is_path_continuation) || !head.is_some_and(is_path_continuation) {
        return false;
    }
    // A TUI only breaks a row because it ran out of width, so a row that
    // stops well short of its wrap column ended rather than wrapped.
    let content_end = column_after(above, a.end.saturating_sub(1), cols);
    if content_end < wrap_column(above, cols).saturating_sub(STITCH_TOLERANCE) {
        return false;
    }
    // A full row wraps; a row longer than the one above it is not its tail.
    chars_len(b.content) <= chars_len(a.content)
}

/// Split a row into the content a link can live in and the frame around it.
///
/// The trailing run of frame characters — a box's right border plus the pad
/// spaces past the text — always goes. The leading run only goes when it holds
/// a border glyph, which makes it a box's left edge and its padding; a bare
/// run of spaces is indentation that belongs to the content, and keeping it is
/// what lets an indented row read as a fresh line rather than a continuation.
fn strip_frame(text: &str) -> Frame<'_> {
    let mut end = chars_len(text);
    let mut end_byte = text.len();
    for (byte, c) in text.char_indices().rev() {
        if !is_frame(c) {
            break;
        }
        end_byte = byte;
        end -= 1;
    }
    let mut start = 0;
    let mut start_byte = 0;
    let mut bordered = false;
    for (byte, c) in text.char_indices() {
        if byte >= end_byte || !is_frame(c) {
            break;
        }
        start_byte = byte + c.len_utf8();
        start += 1;
        bordered |= is_border(c);
    }
    if !bordered {
        start = 0;
        start_byte = 0;
    }
    Frame {
        content: &text[start_byte..end_byte],
        start,
        end,
    }
}

/// The column a row would have wrapped at: a box's right border if it has one,
/// otherwise the terminal width.
fn wrap_column(row: &TerminalRow, cols: usize) -> usize {
    let mut index = chars_len(&row.text);
    for c in row.text.chars().rev() {
        index -= 1;
        if !c.is_whitespace() {
            return if is_border(c) {
                column_of(row, index, cols)
            } else {
                cols
            };
        }
    }
    cols
}

/// The number of characters `text` holds.
fn chars_len(text: &str) -> usize {
    text.chars().count()
}

/// A segment's position in its group as a link reports it.
fn row_index(row: usize) -> i32 {
    i32::try_from(row).unwrap_or(i32::MAX)
}

/// The rows of `rows`, grouped into the logical lines a link can span.
fn group_rows(rows: &[TerminalRow], cols: usize) -> Vec<Vec<Segment>> {
    let mut groups: Vec<Vec<Segment>> = Vec::new();
    let mut current: Vec<Segment> = Vec::new();
    let mut stitches = 0;
    for (index, row) in rows.iter().enumerate() {
        if current.is_empty() {
            current.push(whole_row(index, row, cols));
            stitches = 0;
            continue;
        }
        if row.is_wrapped {
            current.push(whole_row(index, row, cols));
            continue;
        }
        let continues = index
            .checked_sub(1)
            .and_then(|above| rows.get(above))
            .is_some_and(|previous| stitches < STITCH_LIMIT && can_stitch(previous, row, cols));
        if continues {
            // The row above keeps only its framed content: its border and pad
            // spaces would otherwise land between the halves of the path.
            truncate_to_content(&mut current, rows);
            let framed = strip_frame(&row.text);
            current.push(Segment {
                row_index: index,
                char_offset: framed.start,
                text: framed.content.to_owned(),
                columns: slice_columns(row, framed.start, framed.end, cols),
                stitched: true,
            });
            stitches += 1;
            continue;
        }
        groups.push(std::mem::take(&mut current));
        current = vec![whole_row(index, row, cols)];
        stitches = 0;
    }
    if !current.is_empty() {
        groups.push(current);
    }
    groups
}

fn whole_row(index: usize, row: &TerminalRow, cols: usize) -> Segment {
    Segment {
        row_index: index,
        char_offset: 0,
        text: row.text.clone(),
        columns: row_columns(row, cols),
        stitched: false,
    }
}

/// Cuts the last segment back to the content of its row.
fn truncate_to_content(segments: &mut [Segment], rows: &[TerminalRow]) {
    let Some(last) = segments.last_mut() else {
        return;
    };
    let Some(row) = rows.get(last.row_index) else {
        return;
    };
    let keep = strip_frame(&row.text).end.saturating_sub(last.char_offset);
    if keep < last.chars() {
        last.text = last.text.chars().take(keep).collect();
        last.columns.truncate(keep + 1);
    }
}

fn detect_segment_links(segments: &[Segment]) -> Vec<TerminalLink> {
    let text: String = segments
        .iter()
        .map(|segment| segment.text.as_str())
        .collect();
    let indices = CharIndices::of(&text);
    let mut links: Vec<TerminalLink> = Vec::new();

    for captures in URL_PATTERN.captures_iter(&text) {
        let Some(found) = captures.get(0) else {
            continue;
        };
        let start_index = indices.at(found.start());
        let trimmed = trim_link_candidate(found.as_str());
        if trimmed.is_empty() {
            continue;
        }
        links.push(build_link(LinkKind::Url, start_index, &trimmed, segments));
    }

    for captures in PATH_PATTERN.captures_iter(&text) {
        let Some(path) = captures.get(2) else {
            continue;
        };
        let start_index = indices.at(path.start());
        let trimmed = trim_link_candidate(path.as_str());
        let end_index = start_index + chars_len(&trimmed);
        if trimmed.is_empty() || overlaps_existing_link(start_index, end_index, &links) {
            continue;
        }
        links.push(build_link(LinkKind::Path, start_index, &trimmed, segments));
    }

    links.sort_by_key(|link| link.start_index);
    links
}

/// Where each character of an assembled line starts, so a byte offset a
/// pattern reports can be turned into the character index the links use.
struct CharIndices {
    offsets: Vec<usize>,
}

impl CharIndices {
    fn of(text: &str) -> Self {
        Self {
            offsets: text.char_indices().map(|(byte, _)| byte).collect(),
        }
    }

    /// The character index at byte offset `byte`.
    fn at(&self, byte: usize) -> usize {
        self.offsets.partition_point(|offset| *offset < byte)
    }
}

fn build_link(
    kind: LinkKind,
    start_index: usize,
    text: &str,
    segments: &[Segment],
) -> TerminalLink {
    let end_index = start_index + chars_len(text);
    let start = locate(segments, start_index);
    let end = locate(segments, end_index.saturating_sub(1));
    // A browser cannot verify a URL the way the filesystem verifies a path, so
    // there is nothing to fall back to: one candidate, always.
    let candidates = if kind == LinkKind::Url {
        vec![TerminalLinkCandidate {
            path: text.to_owned(),
            line: None,
            column: None,
        }]
    } else {
        build_candidates(text, start_index, end_index, segments)
    };
    let primary = candidates
        .first()
        .cloned()
        .unwrap_or_else(|| TerminalLinkCandidate {
            path: text.to_owned(),
            line: None,
            column: None,
        });
    TerminalLink {
        kind,
        text: text.to_owned(),
        target: primary.path.clone(),
        line: primary.line,
        column: primary.column,
        candidates,
        start_index,
        end_index,
        start_row: row_index(start.row),
        start_column: start.column,
        end_row: row_index(end.row),
        end_column: end.after,
    }
}

/// The readings of a path, longest first: the whole match, then the match cut
/// back to each hard-stitch boundary inside it, from the last to the first.
fn build_candidates(
    text: &str,
    start_index: usize,
    end_index: usize,
    segments: &[Segment],
) -> Vec<TerminalLinkCandidate> {
    let mut readings: Vec<String> = vec![text.to_owned()];
    let mut boundaries: Vec<usize> = Vec::new();
    let mut offset = 0;
    for segment in segments {
        if segment.stitched && offset > start_index && offset < end_index {
            boundaries.push(offset);
        }
        offset += segment.chars();
    }
    for boundary in boundaries.iter().rev() {
        readings.push(text.chars().take(boundary - start_index).collect());
    }

    let mut candidates: Vec<TerminalLinkCandidate> = Vec::new();
    for reading in &readings {
        if reading.is_empty() {
            continue;
        }
        let split = split_path_position(reading);
        if split.path.is_empty() || candidates.contains(&split) {
            continue;
        }
        candidates.push(split);
    }
    candidates
}

/// Where `index` falls in the assembled line.
fn locate(segments: &[Segment], index: usize) -> Location {
    let mut offset = 0;
    for segment in segments {
        let next = offset + segment.chars();
        if index < next {
            let inner = index - offset;
            return Location {
                row: segment.row_index,
                column: segment.column(inner),
                after: segment.column_after(inner),
            };
        }
        offset = next;
    }
    match segments.last() {
        Some(last) => Location {
            row: last.row_index,
            column: last.end_column(),
            after: last.end_column(),
        },
        None => Location {
            row: 0,
            column: index,
            after: index + 1,
        },
    }
}

/// The match with the punctuation a sentence ends on stripped off it.
fn trim_link_candidate(value: &str) -> String {
    let mut next = value;
    while let Some(found) = TRAILING_PUNCTUATION.find(next) {
        next = &next[..found.start()];
    }
    if let Some(without_colon) = next.strip_suffix(':')
        && !is_bare_drive(next)
    {
        next = without_colon;
    }
    next.to_owned()
}

/// `X:` — a drive a link may name on its own, whose colon is not a separator.
fn is_bare_drive(value: &str) -> bool {
    let mut chars = value.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.next() == Some(':')
        && chars.next().is_none()
}

/// The path and its `:line:column` suffix, if it has one.
fn split_path_position(value: &str) -> TerminalLinkCandidate {
    let Some(captures) = LINE_COLUMN_SUFFIX.captures(value) else {
        return TerminalLinkCandidate {
            path: value.to_owned(),
            line: None,
            column: None,
        };
    };
    TerminalLinkCandidate {
        path: captures
            .get(1)
            .map_or("", |group| group.as_str())
            .to_owned(),
        line: captures
            .get(2)
            .and_then(|group| group.as_str().parse::<u32>().ok()),
        column: captures
            .get(3)
            .and_then(|group| group.as_str().parse::<u32>().ok()),
    }
}

fn overlaps_existing_link(start_index: usize, end_index: usize, links: &[TerminalLink]) -> bool {
    links
        .iter()
        .any(|link| start_index < link.end_index && end_index > link.start_index)
}

#[cfg(test)]
mod tests {
    use super::{
        LinkKind, TerminalLink, TerminalLinkCandidate, TerminalRow, can_stitch, detect_links,
        detect_row_links, group_rows, trim_link_candidate,
    };

    /// A row of `text` padded to `cols`, with each character of `two_column`
    /// taking two columns.
    fn row_with(text: &str, cols: usize, is_wrapped: bool, two_column: &[char]) -> TerminalRow {
        let at = |column: usize| u16::try_from(column).unwrap_or(u16::MAX);
        let mut padded = String::new();
        let mut columns = Vec::new();
        let mut column = 0;
        for c in text.chars() {
            padded.push(c);
            columns.push(at(column));
            column += if two_column.contains(&c) { 2 } else { 1 };
        }
        while column < cols {
            padded.push(' ');
            columns.push(at(column));
            column += 1;
        }
        TerminalRow {
            text: padded,
            columns,
            is_wrapped,
        }
    }

    /// A row of `text` padded to `cols`, one column to a character.
    fn row(text: &str, cols: usize, is_wrapped: bool) -> TerminalRow {
        row_with(text, cols, is_wrapped, &[])
    }

    fn candidate(path: &str, line: Option<u32>, column: Option<u32>) -> TerminalLinkCandidate {
        TerminalLinkCandidate {
            path: path.to_owned(),
            line,
            column,
        }
    }

    fn targets(links: &[TerminalLink]) -> Vec<&str> {
        links.iter().map(|link| link.target.as_str()).collect()
    }

    #[test]
    fn url_drops_trailing_punctuation() {
        let links = detect_links("see https://example.com/a, ok");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].kind, LinkKind::Url);
        assert_eq!(links[0].target, "https://example.com/a");
        assert_eq!(
            links[0].candidates,
            vec![candidate("https://example.com/a", None, None)]
        );
        assert_eq!(links[0].start_row, 0);
        assert_eq!(links[0].end_row, 0);
        assert_eq!(links[0].start_column, links[0].start_index);
        assert_eq!(links[0].end_column, links[0].end_index);
    }

    #[test]
    fn path_with_line_and_column_reference() {
        let links = detect_links("at src/main.rs:42:7 failed");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].kind, LinkKind::Path);
        assert_eq!(links[0].target, "src/main.rs");
        assert_eq!(links[0].line, Some(42));
        assert_eq!(links[0].column, Some(7));
        assert_eq!(
            links[0].candidates,
            vec![candidate("src/main.rs", Some(42), Some(7))]
        );
    }

    #[test]
    fn parenthesised_path_drops_trailing_punctuation() {
        let links = detect_links("(see ./docs/plan.md).");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target, "./docs/plan.md");
        assert_eq!(links[0].line, None);
        assert_eq!(links[0].column, None);
    }

    #[test]
    fn soft_wrap_merges_a_path() {
        let cols = 20;
        let rows = [
            row("See X:/dev/project/l", cols, false),
            row("ong/name/file.ts:12", cols, true),
        ];

        let links = detect_row_links(&rows, cols);

        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target, "X:/dev/project/long/name/file.ts");
        assert_eq!(links[0].line, Some(12));
        assert_eq!(links[0].candidates.len(), 1);
        assert_eq!(links[0].start_row, 0);
        assert_eq!(links[0].start_column, 4);
        assert_eq!(links[0].end_row, 1);
        assert_eq!(links[0].end_column, 19);
    }

    #[test]
    fn box_hard_wrap_merges_and_keeps_a_fallback() {
        let cols = 28;
        let rows = [
            row("│ X:/dev/proj/notes-file │", cols, false),
            row("│ le.txt:7:3             │", cols, false),
        ];

        assert!(can_stitch(&rows[0], &rows[1], cols));

        let links = detect_row_links(&rows, cols);

        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target, "X:/dev/proj/notes-filele.txt");
        assert_eq!(
            links[0].candidates,
            vec![
                candidate("X:/dev/proj/notes-filele.txt", Some(7), Some(3)),
                candidate("X:/dev/proj/notes-file", None, None),
            ]
        );
        assert_eq!(links[0].start_row, 0);
        assert_eq!(links[0].start_column, 2);
        assert_eq!(links[0].end_row, 1);
        assert_eq!(links[0].end_column, 12);
    }

    #[test]
    fn short_row_does_not_stitch() {
        let cols = 40;
        let rows = [
            row("Wrote to ./output.txt", cols, false),
            row("done", cols, false),
        ];

        assert!(!can_stitch(&rows[0], &rows[1], cols));

        let links = detect_row_links(&rows, cols);

        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target, "./output.txt");
        assert_eq!(links[0].candidates.len(), 1);
    }

    #[test]
    fn longer_continuation_does_not_stitch() {
        let cols = 24;
        let rows = [
            row("│ a/b.ts │", cols, false),
            row("│ continuation-here │", cols, false),
        ];

        assert!(!can_stitch(&rows[0], &rows[1], cols));

        let links = detect_row_links(&rows, cols);

        assert_eq!(targets(&links), ["a/b.ts"]);
    }

    #[test]
    fn whitespace_led_continuation_does_not_stitch() {
        let cols = 20;
        let rows = [
            row("Log at /var/log/a.tx", cols, false),
            row("    t-file.log", cols, false),
        ];

        assert!(!can_stitch(&rows[0], &rows[1], cols));

        let links = detect_row_links(&rows, cols);

        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target, "/var/log/a.tx");
        assert_eq!(links[0].candidates.len(), 1);
    }

    #[test]
    fn unc_path_is_detected() {
        let links = detect_links(r"\\server\share\file.txt");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].kind, LinkKind::Path);
        assert_eq!(links[0].target, r"\\server\share\file.txt");
        assert_eq!(links[0].line, None);
    }

    #[test]
    fn parent_relative_path_with_line_is_detected() {
        let links = detect_links("../x/y.rs:3");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target, "../x/y.rs");
        assert_eq!(links[0].line, Some(3));
        assert_eq!(links[0].column, None);
    }

    #[test]
    fn drive_letter_alone_is_not_a_path() {
        assert!(detect_links("X:").is_empty());
        assert_eq!(trim_link_candidate("X:"), "X:");
        assert_eq!(trim_link_candidate("X:/a:"), "X:/a");
    }

    #[test]
    fn path_overlapping_a_url_is_skipped() {
        let links = detect_links("https://example.com/a(b/c.rs");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].kind, LinkKind::Url);
        assert_eq!(links[0].target, "https://example.com/a(b/c.rs");
    }

    #[test]
    fn fifth_hard_stitch_is_refused() {
        let cols = 20;
        let full = "aaaaaaaaaaaaaaaaaaaa";
        let rows = [
            row("X:/dir/aaaaaaaaaaaaa", cols, false),
            row(full, cols, false),
            row(full, cols, false),
            row(full, cols, false),
            row(full, cols, false),
            row("bbbbbbbbbbbbbbbbbbbb", cols, false),
        ];

        assert!(can_stitch(&rows[3], &rows[4], cols));
        assert!(can_stitch(&rows[4], &rows[5], cols));
        let groups = group_rows(&rows, cols);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].len(), 5);
        assert_eq!(groups[1].len(), 1);

        let links = detect_row_links(&rows, cols);

        assert_eq!(links.len(), 1);
        assert_eq!(links[0].end_row, 4);
        assert_eq!(
            links[0].target,
            format!("X:/dir/{}{}", "a".repeat(13), "a".repeat(80))
        );
    }

    #[test]
    fn fallbacks_are_ordered_longest_first_with_two_stitches() {
        let cols = 20;
        let head = "./aaaa/bbbb/cccc.ttt";
        let middle = "u".repeat(20);
        let rows = [
            row(head, cols, false),
            row(&middle, cols, false),
            row("vvv.rs:9:2", cols, false),
        ];

        assert!(can_stitch(&rows[0], &rows[1], cols));
        assert!(can_stitch(&rows[1], &rows[2], cols));

        let links = detect_row_links(&rows, cols);

        assert_eq!(links.len(), 1);
        assert_eq!(links[0].text, format!("{head}{middle}vvv.rs:9:2"));
        assert_eq!(
            links[0].candidates,
            vec![
                candidate(&format!("{head}{middle}vvv.rs"), Some(9), Some(2)),
                candidate(&format!("{head}{middle}"), None, None),
                candidate(head, None, None),
            ]
        );
    }

    #[test]
    fn cjk_inside_a_path_is_opened_by_its_real_name() {
        let cols = 30;
        let rows = [row_with(
            "C:/Users/田中/notes.txt",
            cols,
            false,
            &['田', '中'],
        )];

        let links = detect_row_links(&rows, cols);

        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target, "C:/Users/田中/notes.txt");
        assert_eq!(links[0].line, None);
        assert_eq!(links[0].start_column, 0);
        assert_eq!(links[0].end_column, 23);
    }

    #[test]
    fn accented_letter_before_a_url_still_detects_it() {
        let links = detect_links("caféhttps://example.com/a");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].kind, LinkKind::Url);
        assert_eq!(links[0].target, "https://example.com/a");
        assert_eq!(links[0].start_index, 4);
    }

    #[test]
    fn non_ascii_digits_are_not_a_line_number() {
        let links = detect_links("src/main.rs:4٢");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target, "src/main.rs:4٢");
        assert_eq!(links[0].line, None);
        assert_eq!(links[0].column, None);
    }
}
