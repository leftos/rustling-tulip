//! What a terminal selection copies when it crosses a TUI's panel: the
//! panel's box-drawing borders, rules and padding left out, and the
//! paragraphs the panel wrapped joined again.

use std::ops::Range;

/// One selected grid line: the characters of its selected cells, the grid
/// columns each spans, and whether the grid wrapped it into the next line.
pub(crate) struct CopyRow {
    pub(crate) text: String,
    /// One entry per character of `text`: 2 for a wide glyph, a tab's run
    /// of columns to its stop, 0 for a zero-width mark.
    pub(crate) widths: Vec<usize>,
    pub(crate) wrapped: bool,
}

/// A character and the grid columns it spans.
type Cell = (char, usize);

const VERTICALS: [char; 9] = ['│', '┃', '║', '╎', '╏', '┆', '┇', '┊', '┋'];
const HORIZONTALS: [char; 9] = ['─', '━', '═', '╌', '╍', '┄', '┅', '┈', '┉'];
const TOP_LEFT: [char; 6] = ['╭', '┌', '┏', '╔', '╒', '╓'];
const TOP_RIGHT: [char; 6] = ['╮', '┐', '┓', '╗', '╕', '╖'];
const BOTTOM_LEFT: [char; 6] = ['╰', '└', '┗', '╚', '╘', '╙'];
const BOTTOM_RIGHT: [char; 6] = ['╯', '┘', '┛', '╝', '╛', '╜'];
const LEFT_TEE: [char; 7] = ['├', '┣', '╠', '┝', '┠', '╞', '╟'];
const RIGHT_TEE: [char; 7] = ['┤', '┫', '╣', '┥', '┨', '╡', '╢'];
/// The light square corners and tees a box-drawn table is framed with.
const SQUARE: [char; 6] = ['┌', '┐', '└', '┘', '├', '┤'];
/// The crossings a table's rule has where a column divider meets it.
const JUNCTIONS: [char; 21] = [
    '┬', '┼', '┴', '┳', '╋', '┻', '╦', '╬', '╩', '╤', '╪', '╧', '╥', '╫', '╨', '┯', '┿', '┷', '┰',
    '╂', '┸',
];
const BULLETS: [&str; 4] = ["-", "*", "•", "·"];

/// Where a panel rule sits in its panel.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RuleKind {
    Top,
    Middle,
    Bottom,
}

/// The sides of a row the selection may have cut off: the first row's
/// left, the last row's right.
#[derive(Clone, Copy)]
struct Cut {
    left: bool,
    right: bool,
}

/// A panel's horizontal rule.
struct Rule {
    kind: RuleKind,
    /// Whether its corners or tees are the light square ones.
    square: bool,
    /// The text set inside the rule, the horizontals around it trimmed.
    title: String,
}

/// A row with a panel bar on each side, or on the side the selection kept.
struct Framed {
    /// The cells between the bars, padding kept.
    inner: Vec<Cell>,
    left_cut: bool,
    right_cut: bool,
    /// Every box-drawing vertical in the row, the panel's own included.
    bars: usize,
}

enum Shape {
    Rule(Rule),
    Framed(Framed),
    Other,
}

/// A panel row with its bars gone.
struct PanelRow {
    /// The row's cells, its trailing spaces trimmed and its indent kept.
    cells: Vec<Cell>,
    /// The columns of the row's leading blanks.
    indent: usize,
    /// The column, counted from the left bar, just past the row's text.
    end: usize,
    /// The blank columns between the row's text and the right bar.
    trailing: usize,
    left_cut: bool,
    /// A row with both bars.
    whole: bool,
    /// A row that is never joined to its neighbours: a section break, a
    /// drawing, a table row, a numbered or diff line, a line of code.
    fixed: bool,
}

/// The selection's text with its panels' frames left out, or `None` when
/// the selection crosses no panel.
pub(crate) fn strip_box(rows: &[CopyRow]) -> Option<String> {
    let last = rows.len().checked_sub(1)?;
    let shapes: Vec<Shape> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let cut = Cut {
                left: i == 0,
                right: i == last,
            };
            shape(&cells(row), cut)
        })
        .collect();
    let panels: Vec<Range<usize>> = runs(&shapes)
        .into_iter()
        .filter(|run| is_panel(&shapes[run.clone()]))
        .collect();
    if panels.is_empty() {
        return None;
    }
    let text = render(rows, &shapes, &panels);
    (!text.is_empty()).then_some(text)
}

fn cells(row: &CopyRow) -> Vec<Cell> {
    row.text
        .chars()
        .enumerate()
        .map(|(i, c)| (c, row.widths.get(i).copied().unwrap_or(1)))
        .collect()
}

fn columns(cells: &[Cell]) -> usize {
    cells.iter().map(|&(_, width)| width).sum()
}

fn shape(cells: &[Cell], cut: Cut) -> Shape {
    let chars: Vec<char> = cells.iter().map(|&(c, _)| c).collect();
    let Some(first) = chars.iter().position(|&c| c != ' ') else {
        return Shape::Other;
    };
    let last = chars.iter().rposition(|&c| c != ' ').unwrap_or(first);
    if let Some(rule) = rule(&chars[first..=last], cut) {
        return Shape::Rule(rule);
    }
    framed(cells, first, last, cut).map_or(Shape::Other, Shape::Framed)
}

/// `text` (a row's non-space span) as a panel rule: horizontals between two
/// corners or tees of one kind, a title allowed inside, and no junction,
/// which would make it a table's rule. A side the selection cut may end in
/// a horizontal instead.
fn rule(text: &[char], cut: Cut) -> Option<Rule> {
    let (kind, run) = rule_kind(text, cut)?;
    let square = [text.first(), text.last()]
        .into_iter()
        .flatten()
        .filter(|c| !HORIZONTALS.contains(c))
        .all(|c| SQUARE.contains(c));
    let title: String = text[run].iter().collect();
    let title = title.trim_matches(|c: char| c == ' ' || HORIZONTALS.contains(&c));
    Some(Rule {
        kind,
        square,
        title: title.to_owned(),
    })
}

/// The rule's kind and the span of `text` between its ends.
fn rule_kind(text: &[char], cut: Cut) -> Option<(RuleKind, Range<usize>)> {
    let left = text.first().copied().and_then(left_end);
    let right = text.last().copied().and_then(right_end);
    let kind = match (left, right) {
        (Some(l), Some(r)) if l == r => l,
        (Some(l), None) if cut.right => l,
        (None, Some(r)) if cut.left => r,
        _ => return None,
    };
    let run = usize::from(left.is_some())..text.len() - usize::from(right.is_some());
    text.get(run.clone())
        .is_some_and(is_rule_run)
        .then_some((kind, run))
}

fn is_rule_run(run: &[char]) -> bool {
    let horizontal = |c: Option<&char>| c.is_some_and(|c| HORIZONTALS.contains(c));
    horizontal(run.first()) && horizontal(run.last()) && !run.iter().any(|c| JUNCTIONS.contains(c))
}

fn left_end(c: char) -> Option<RuleKind> {
    end_kind(c, &TOP_LEFT, &BOTTOM_LEFT, &LEFT_TEE)
}

fn right_end(c: char) -> Option<RuleKind> {
    end_kind(c, &TOP_RIGHT, &BOTTOM_RIGHT, &RIGHT_TEE)
}

fn end_kind(c: char, top: &[char], bottom: &[char], tee: &[char]) -> Option<RuleKind> {
    if top.contains(&c) {
        Some(RuleKind::Top)
    } else if bottom.contains(&c) {
        Some(RuleKind::Bottom)
    } else if tee.contains(&c) {
        Some(RuleKind::Middle)
    } else {
        None
    }
}

/// The row as a framed panel row, when its first and last non-space
/// characters (at `first` and `last`) are bars, or a side the selection cut
/// lacks one.
fn framed(cells: &[Cell], first: usize, last: usize, cut: Cut) -> Option<Framed> {
    let is_bar = |i: usize| VERTICALS.contains(&cells[i].0);
    let left_bar = is_bar(first);
    let right_bar = last > first && is_bar(last);
    if !left_bar && !right_bar {
        return None;
    }
    let start = if left_bar {
        first + 1
    } else if cut.left {
        0
    } else {
        return None;
    };
    let end = if right_bar {
        last
    } else if cut.right {
        cells.len()
    } else {
        return None;
    };
    Some(Framed {
        inner: cells[start..end].to_vec(),
        left_cut: !left_bar,
        right_cut: !right_bar,
        bars: cells.iter().filter(|(c, _)| VERTICALS.contains(c)).count(),
    })
}

/// The runs of consecutive rule and framed rows: a top rule opens a run, a
/// bottom rule closes one, and any other row ends it.
fn runs(shapes: &[Shape]) -> Vec<Range<usize>> {
    let mut runs = Vec::new();
    let mut start = None;
    for (i, shape) in shapes.iter().enumerate() {
        match shape {
            Shape::Other => close(&mut runs, &mut start, i),
            Shape::Rule(rule) if rule.kind == RuleKind::Top => {
                close(&mut runs, &mut start, i);
                start = Some(i);
            }
            Shape::Rule(rule) if rule.kind == RuleKind::Bottom => {
                runs.push(start.take().unwrap_or(i)..i + 1);
            }
            Shape::Rule(_) | Shape::Framed(_) => {
                start.get_or_insert(i);
            }
        }
    }
    close(&mut runs, &mut start, shapes.len());
    runs
}

fn close(runs: &mut Vec<Range<usize>>, start: &mut Option<usize>, end: usize) {
    if let Some(from) = start.take() {
        runs.push(from..end);
    }
}

/// Whether a run is a panel: it holds a framed row, and a panel rule or a
/// whole row with only the panel's two bars. A run of table rows alone is
/// a table, and so is a run framed in light square corners that holds a
/// `├─┤` rule: a one-column table.
fn is_panel(run: &[Shape]) -> bool {
    let rules: Vec<&Rule> = run
        .iter()
        .filter_map(|shape| match shape {
            Shape::Rule(rule) => Some(rule),
            Shape::Framed(_) | Shape::Other => None,
        })
        .collect();
    let square_table = rules.iter().all(|rule| rule.square)
        && rules.iter().any(|rule| rule.kind == RuleKind::Middle);
    let mut framed = framed_rows(run).peekable();
    let has_framed = framed.peek().is_some();
    let has_plain = framed.any(|f| f.bars <= 2 && !f.left_cut && !f.right_cut);
    has_framed && (!rules.is_empty() || has_plain) && !square_table
}

fn framed_rows(run: &[Shape]) -> impl Iterator<Item = &Framed> {
    run.iter().filter_map(|shape| match shape {
        Shape::Framed(framed) => Some(framed),
        Shape::Rule(_) | Shape::Other => None,
    })
}

/// The copied text: each panel's lines, and every other row as the grid
/// copies it, trailing blanks trimmed and a wrapped row run into the next.
fn render(rows: &[CopyRow], shapes: &[Shape], panels: &[Range<usize>]) -> String {
    let mut lines = Vec::new();
    let mut carry = String::new();
    let mut i = 0;
    while i < rows.len() {
        if let Some(panel) = panels.iter().find(|panel| panel.start == i) {
            flush(&mut lines, &mut carry);
            lines.extend(panel_lines(&shapes[panel.clone()]));
            i = panel.end;
            continue;
        }
        let row = &rows[i];
        if row.wrapped {
            carry.push_str(&row.text);
        } else {
            carry.push_str(row.text.trim_end_matches(' '));
            lines.push(std::mem::take(&mut carry));
        }
        i += 1;
    }
    flush(&mut lines, &mut carry);
    lines.join("\n")
}

fn flush(lines: &mut Vec<String>, carry: &mut String) {
    if !carry.is_empty() {
        lines.push(std::mem::take(carry));
    }
}

/// A panel's text: its top rule's title first, rules dropped (a middle one
/// leaving a blank line), the common indent and trailing spaces gone, empty
/// rows at its top and bottom dropped, and wrapped paragraphs rejoined.
fn panel_lines(run: &[Shape]) -> Vec<String> {
    let title = match run.first() {
        Some(Shape::Rule(rule)) if rule.kind == RuleKind::Top && !rule.title.is_empty() => {
            Some(rule.title.clone())
        }
        _ => None,
    };
    let width = framed_rows(run)
        .filter(|f| !f.left_cut && !f.right_cut)
        .map(|f| columns(&f.inner))
        .max();
    let width = width.unwrap_or_else(|| {
        framed_rows(run)
            .map(|f| columns(&f.inner))
            .max()
            .unwrap_or(0)
    });
    let mut rows: Vec<PanelRow> = run
        .iter()
        .filter_map(|shape| panel_row(shape, width))
        .collect();
    let common = rows
        .iter()
        .filter(|row| !row.cells.is_empty() && !row.left_cut)
        .map(|row| row.indent)
        .min()
        .unwrap_or(0);
    for row in rows.iter_mut().filter(|row| row.left_cut) {
        row.indent = common;
    }
    let padding = rows
        .iter()
        .filter(|row| row.whole)
        .map(|row| row.trailing)
        .min()
        .unwrap_or(0);
    let first = rows.iter().position(|row| !row.cells.is_empty());
    let last = rows.iter().rposition(|row| !row.cells.is_empty());
    let body = match (first, last) {
        (Some(first), Some(last)) => {
            join_rows(&rows[first..=last], common, width.saturating_sub(padding))
        }
        _ => Vec::new(),
    };
    title.into_iter().chain(body).collect()
}

/// A shape of a panel's run as one of its rows: a framed row with its bars
/// gone, a middle rule as an empty row that breaks the section, and `None`
/// for its top and bottom rules.
fn panel_row(shape: &Shape, width: usize) -> Option<PanelRow> {
    match shape {
        Shape::Framed(framed) => Some(framed_row(framed, width)),
        Shape::Rule(rule) if rule.kind == RuleKind::Middle => Some(PanelRow {
            cells: Vec::new(),
            indent: 0,
            end: 0,
            trailing: width,
            left_cut: false,
            whole: false,
            fixed: true,
        }),
        Shape::Rule(_) | Shape::Other => None,
    }
}

fn framed_row(framed: &Framed, width: usize) -> PanelRow {
    let len = framed
        .inner
        .iter()
        .rposition(|&(c, _)| c != ' ')
        .map_or(0, |i| i + 1);
    let cells = framed.inner[..len].to_vec();
    let trailing = columns(&framed.inner[len..]);
    let indent = cells
        .iter()
        .take_while(|&&(c, _)| c == ' ' || c == '\t')
        .map(|&(_, w)| w)
        .sum();
    let text: String = cells.iter().map(|&(c, _)| c).collect();
    PanelRow {
        indent,
        end: if framed.left_cut {
            width.saturating_sub(trailing)
        } else {
            columns(&cells)
        },
        trailing,
        left_cut: framed.left_cut,
        whole: !framed.left_cut && !framed.right_cut,
        fixed: cells.iter().any(|(c, _)| VERTICALS.contains(c)) || !joinable(&text),
        cells,
    }
}

/// Whether a row's text may be joined to a neighbour: it does not start
/// with a box-drawing character, a line number or a diff marker, and does
/// not end like a line of code.
fn joinable(text: &str) -> bool {
    let body = text.trim_start();
    let digits = body.chars().take_while(char::is_ascii_digit).count();
    let numbered = digits > 0 && body[digits..].starts_with(' ');
    let drawn = body
        .chars()
        .next()
        .is_some_and(|c| ('\u{2500}'..='\u{257f}').contains(&c));
    let diff = body.starts_with("+ ") || body.starts_with("- ");
    !(drawn || numbered || diff || body.ends_with([';', '{', '}']))
}

fn join_rows(rows: &[PanelRow], common: usize, width: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut prev: Option<&PanelRow> = None;
    for row in rows {
        let text = dedent(&row.cells, common);
        match lines.last_mut() {
            Some(line) if prev.is_some_and(|prev| continues(prev, row, width)) => {
                line.push(' ');
                line.push_str(text.trim_start());
            }
            _ => lines.push(text),
        }
        prev = Some(row);
    }
    lines
}

/// `cells` as text with up to `common` columns of leading blanks left out.
fn dedent(cells: &[Cell], common: usize) -> String {
    let mut skipped = 0;
    let lead = cells
        .iter()
        .take_while(|&&(c, width)| {
            let take = (c == ' ' || c == '\t') && skipped + width <= common;
            skipped += width;
            take
        })
        .count();
    cells[lead..].iter().map(|&(c, _)| c).collect()
}

/// Whether `next` is the soft wrap of `prev`: neither is fixed, `next` has
/// text, starts no list item and keeps `prev`'s indent, and its first word
/// would not have fit in the `width` columns of `prev`'s row.
fn continues(prev: &PanelRow, next: &PanelRow, width: usize) -> bool {
    let body: Vec<Cell> = next
        .cells
        .iter()
        .skip_while(|&&(c, _)| c == ' ' || c == '\t')
        .copied()
        .collect();
    let word: Vec<Cell> = body
        .iter()
        .take_while(|&&(c, _)| c != ' ')
        .copied()
        .collect();
    let marker: String = word.iter().map(|&(c, _)| c).collect();
    !prev.fixed
        && !next.fixed
        && !prev.cells.is_empty()
        && !body.is_empty()
        && !starts_list(&marker)
        && prev.indent == next.indent
        && prev.end + 1 + columns(&word) > width
}

/// Whether a row's first word marks a list item: a bullet, or a number
/// followed by `.` or `)`.
fn starts_list(word: &str) -> bool {
    let numbered = word
        .strip_suffix(['.', ')'])
        .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
    numbered || BULLETS.contains(&word)
}

#[cfg(test)]
mod tests {
    use super::{CopyRow, strip_box};

    /// A double-width glyph in these tests: the CJK ideographs.
    fn is_wide(c: char) -> bool {
        ('\u{4e00}'..='\u{9fff}').contains(&c)
    }

    fn row(text: String) -> CopyRow {
        let widths = text
            .chars()
            .map(|c| if is_wide(c) { 2 } else { 1 })
            .collect();
        CopyRow {
            text,
            widths,
            wrapped: false,
        }
    }

    /// The rows as a selection spanning a grid wider than the panel copies
    /// them: each with the blank cells right of the panel.
    fn rows(lines: &[String]) -> Vec<CopyRow> {
        lines
            .iter()
            .map(|line| row(format!("{line}    ")))
            .collect()
    }

    fn plain(lines: &[&str]) -> Vec<CopyRow> {
        lines.iter().map(|line| row((*line).to_owned())).collect()
    }

    fn side(inner: &str, width: usize) -> String {
        format!("│{inner:<width$}│")
    }

    fn top(title: &str, width: usize) -> String {
        format!("╭{title:─<width$}╮")
    }

    fn divider(width: usize) -> String {
        format!("├{}┤", "─".repeat(width))
    }

    fn bottom(width: usize) -> String {
        format!("╰{}╯", "─".repeat(width))
    }

    #[test]
    fn a_rounded_panel_with_padding_copies_only_its_text() {
        let w = 30;
        let input = rows(&[
            top("─── Claude Code ", w),
            side("", w),
            side("   Welcome back!", w),
            side("", w),
            side("   /help for help", w),
            bottom(w),
        ]);
        assert_eq!(
            strip_box(&input).as_deref(),
            Some("Claude Code\nWelcome back!\n\n/help for help")
        );
    }

    #[test]
    fn a_titled_panel_keeps_its_title_as_the_first_line() {
        let w = 20;
        let input = rows(&[top("─── Claude Code v2 ", w), side(" hi", w), bottom(w)]);
        assert_eq!(strip_box(&input).as_deref(), Some("Claude Code v2\nhi"));
    }

    #[test]
    fn a_box_nested_inside_a_panel_is_kept() {
        let w = 30;
        let input = rows(&[
            top("", w),
            side(" outer text", w),
            side(" ┌────────┐", w),
            side(" │ inner  │", w),
            side(" └────────┘", w),
            bottom(w),
        ]);
        assert_eq!(
            strip_box(&input).as_deref(),
            Some("outer text\n┌────────┐\n│ inner  │\n└────────┘")
        );
    }

    #[test]
    fn a_full_width_nested_box_is_not_joined_to_the_prose_above_it() {
        let w = 20;
        let rule = "─".repeat(16);
        let input = rows(&[
            top("", w),
            side(" some prose here", w),
            side(&format!(" ┌{rule}┐"), w),
            side(&format!(" │{:<16}│", " in"), w),
            side(&format!(" └{rule}┘"), w),
            bottom(w),
        ]);
        let want = format!("some prose here\n┌{rule}┐\n│{:<16}│\n└{rule}┘", " in");
        assert_eq!(strip_box(&input), Some(want));
    }

    #[test]
    fn a_code_line_ending_in_a_semicolon_is_not_joined_to_the_next() {
        let w = 24;
        let input = rows(&[
            top("", w),
            side(" let sum = compute(xyz);", w),
            side(" print(sum);", w),
            bottom(w),
        ]);
        assert_eq!(
            strip_box(&input).as_deref(),
            Some("let sum = compute(xyz);\nprint(sum);")
        );
    }

    #[test]
    fn a_selection_starting_mid_panel_without_its_top_rule_is_stripped() {
        let w = 24;
        let cut: String = side(" first line", w).chars().skip(8).collect();
        let input = rows(&[
            cut,
            side(" second line", w),
            side(" the third row is longer", w),
            bottom(w),
        ]);
        assert_eq!(
            strip_box(&input).as_deref(),
            Some("line\nsecond line\nthe third row is longer")
        );
    }

    #[test]
    fn a_literal_bar_inside_a_sentence_is_not_a_border() {
        let input = plain(&["the pipe a │ b is literal", "and so is │ this"]);
        assert_eq!(strip_box(&input), None);
    }

    #[test]
    fn a_box_drawn_table_copied_whole_is_unchanged() {
        let input = plain(&[
            "┌───┬───┐",
            "│ a │ b │",
            "├───┼───┤",
            "│ c │ d │",
            "└───┴───┘",
        ]);
        assert_eq!(strip_box(&input), None);
    }

    #[test]
    fn a_one_column_box_drawn_table_is_unchanged() {
        let input = plain(&["┌──────┐", "│ Name │", "├──────┤", "│ a    │", "└──────┘"]);
        assert_eq!(strip_box(&input), None);
    }

    #[test]
    fn a_markdown_rendered_table_is_unchanged() {
        let input = plain(&[
            "Here are the results:",
            "┌────────┬───────┐",
            "│ Name   │ Value │",
            "├────────┼───────┤",
            "│ alpha  │ 1     │",
            "│ beta   │ 2     │",
            "└────────┴───────┘",
        ]);
        assert_eq!(strip_box(&input), None);
    }

    #[test]
    fn a_table_inside_a_panel_loses_the_panel_bars_and_keeps_its_grid() {
        let w = 40;
        let input = rows(&[
            top("", w),
            side(" Results:", w),
            side(" ┌───────┬───────┐", w),
            side(" │ a     │ b     │", w),
            side(" ├───────┼───────┤", w),
            side(" │ 1     │ 2     │", w),
            side(" └───────┴───────┘", w),
            bottom(w),
        ]);
        assert_eq!(
            strip_box(&input).as_deref(),
            Some(
                "Results:\n┌───────┬───────┐\n│ a     │ b     │\n├───────┼───────┤\n│ 1     │ 2     │\n└───────┴───────┘"
            )
        );
    }

    #[test]
    fn a_wrapped_paragraph_inside_a_panel_is_rejoined_and_a_list_is_not() {
        let w = 24;
        let input = rows(&[
            top("", w),
            side(" alpha beta gamma delta", w),
            side(" epsilon zeta", w),
            side("", w),
            side(" - one two three fourth", w),
            side(" - five", w),
            bottom(w),
        ]);
        assert_eq!(
            strip_box(&input).as_deref(),
            Some("alpha beta gamma delta epsilon zeta\n\n- one two three fourth\n- five")
        );
    }

    #[test]
    fn a_word_that_would_not_fit_inside_the_right_padding_is_rejoined() {
        let w = 40;
        let input = rows(&[
            top("", w),
            side(" aaaa bbbb cccc dddd eeee ffff gggg hhh", w),
            side(" iiii jjjj kkkk llll mmmm nnnn ooo", w),
            side(" ppppp", w),
            bottom(w),
        ]);
        assert_eq!(
            strip_box(&input).as_deref(),
            Some("aaaa bbbb cccc dddd eeee ffff gggg hhh iiii jjjj kkkk llll mmmm nnnn ooo ppppp")
        );
    }

    #[test]
    fn a_wide_glyph_paragraph_that_wraps_at_the_bar_is_rejoined() {
        let input = rows(&[
            format!("╭{}╮", "─".repeat(13)),
            "│ abcdefghijkl│".to_owned(),
            format!("│{:13}│", ""),
            "│ 你好世界大家│".to_owned(),
            "│ 再见        │".to_owned(),
            format!("╰{}╯", "─".repeat(13)),
        ]);
        assert_eq!(
            strip_box(&input).as_deref(),
            Some("abcdefghijkl\n\n你好世界大家 再见")
        );
    }

    #[test]
    fn a_middle_rule_breaks_the_section() {
        let w = 20;
        let input = rows(&[
            top("", w),
            side(" alpha beta gamma de", w),
            divider(w),
            side(" Section two", w),
            bottom(w),
        ]);
        assert_eq!(
            strip_box(&input).as_deref(),
            Some("alpha beta gamma de\n\nSection two")
        );
    }

    #[test]
    fn an_ascii_box_is_unchanged() {
        let input = plain(&["+---+---+", "| a | b |", "+---+---+"]);
        assert_eq!(strip_box(&input), None);
    }

    #[test]
    fn a_welcome_panel_keeps_its_middle_divider() {
        let w = 44;
        let pad = " ".repeat(18);
        let input = rows(&[
            top("─── Claude Code v2 ", w),
            side(&format!("{pad}│ Tips"), w),
            side("  Welcome back!   │ Run /init", w),
            side(&format!("{pad}│ {}", "─".repeat(20)), w),
            side(&format!("{pad}│ What's new"), w),
            bottom(w),
        ]);
        let inner = " ".repeat(16);
        let want = format!(
            "Claude Code v2\n{inner}│ Tips\nWelcome back!   │ Run /init\n{inner}│ {}\n{inner}│ What's new",
            "─".repeat(20)
        );
        assert_eq!(strip_box(&input), Some(want));
    }
}
