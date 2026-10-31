//! Stretchtext in a chat answer: `{{short|long}}` marks a stretch, which the
//! reader sees as `short` and opens to put `long` in its place; `long` may
//! hold further stretches. Code spans and fences are copied as written, so
//! braces and bars in code never act. Plain Rust: no GPUI and no I/O.
//!
//! The grammar is cc-stretch's `hooks/parse.ts` (as Collie's
//! `web/src/lib/stretch.ts` ports it), on byte offsets. [`compose`] writes
//! the answer as Markdown with each closed stretch's short form and each open
//! stretch's closer bracketed by private-use markers; the Markdown pass lifts
//! those back out into stretchtext link spans. Those code points are stripped
//! from the input first, so an answer cannot forge a stretch.

use std::collections::{BTreeSet, HashMap};
use std::ops::Range;

/// Joins a nested stretch's key to its parent's.
pub(crate) const KEY_SEPARATOR: &str = " \u{203A} ";
/// What an open stretch's closer draws.
pub(crate) const CLOSER: &str = "\u{2039}";
/// Opens a closed stretch's short form; its index in the keys follows, then
/// [`LINK_SEP`].
pub(crate) const LINK_OPEN: char = '\u{E000}';
/// Ends a closed stretch's index and starts its short form.
pub(crate) const LINK_SEP: char = '\u{E001}';
/// Ends a closed stretch's short form.
pub(crate) const LINK_END: char = '\u{E002}';
/// Opens an open stretch's closer; its index in the keys follows, then
/// [`CLOSER_END`].
pub(crate) const CLOSER_OPEN: char = '\u{E003}';
/// Ends an open stretch's closer.
pub(crate) const CLOSER_END: char = '\u{E004}';

/// An answer as Markdown source with its stretches marked, and the key of
/// each marked stretch by the index its markers carry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Composed {
    pub(crate) source: String,
    pub(crate) keys: Vec<String>,
}

/// How deep stretches nest; a `{{` opened deeper is literal text, so drawing
/// an answer cannot overflow the stack.
const MAX_DEPTH: usize = 32;

/// `text` as Markdown with its stretches marked, after every marker code
/// point is stripped from it.
///
/// A stretch whose key is in `open` draws its long form, its own stretches
/// marked in turn, then its closer; a closed one draws its short form as a
/// marked link. A short form over several lines draws its long form with no
/// closer, as a link cannot span lines. A stretch without a bar, or with an
/// empty long form, draws its short form plain. One whose braces never close
/// draws its short form in a `live` answer and its source in a finished one,
/// and so does one whose short form is empty, which has nothing to click.
pub(crate) fn compose(text: &str, open: &BTreeSet<String>, live: bool) -> Composed {
    let text: String = text.chars().filter(|ch| !is_marker(*ch)).collect();
    let segments = parse(&text);
    let mut composer = Composer {
        text: &text,
        open,
        live,
        keys: Vec::new(),
    };
    let source = composer.write(&segments);
    Composed {
        source,
        keys: composer.keys,
    }
}

/// Closes the stretch `key` and every stretch nested in it, so a stretch
/// opened again starts with its own stretches closed.
pub(crate) fn close(open: &mut BTreeSet<String>, key: &str) {
    let nested = format!("{key}{KEY_SEPARATOR}");
    open.retain(|other| other != key && !other.starts_with(&nested));
}

/// Whether `ch` is one of the markers [`compose`] writes.
pub(crate) fn is_marker(ch: char) -> bool {
    (LINK_OPEN..=CLOSER_END).contains(&ch)
}

/// One marker run [`compose`] wrote, found in parsed Markdown text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Marker {
    /// A closed stretch's short form starts; the index of its key.
    LinkStart(usize),
    /// A closed stretch's short form ends.
    LinkEnd,
    /// An open stretch's closer; the index of its key.
    Closer(usize),
    /// A marker character that starts no run.
    Stray,
}

/// The first marker run in `text`: where it starts, what it is, and where
/// it ends.
pub(crate) fn next_marker(text: &str) -> Option<(usize, Marker, usize)> {
    let start = text.find(is_marker)?;
    let first = text[start..].chars().next()?;
    let after = start + first.len_utf8();
    let indexed = |end: char| -> Option<(usize, usize)> {
        let rest = &text[after..];
        let digits = rest.len()
            - rest
                .trim_start_matches(|ch: char| ch.is_ascii_digit())
                .len();
        let index = rest[..digits].parse().ok()?;
        rest[digits..]
            .starts_with(end)
            .then(|| (index, after + digits + end.len_utf8()))
    };
    let found = match first {
        LINK_OPEN => indexed(LINK_SEP).map(|(index, end)| (Marker::LinkStart(index), end)),
        CLOSER_OPEN => indexed(CLOSER_END).map(|(index, end)| (Marker::Closer(index), end)),
        LINK_END => Some((Marker::LinkEnd, after)),
        _ => None,
    };
    let (marker, end) = found.unwrap_or((Marker::Stray, after));
    Some((start, marker, end))
}

/// `text` without its marker runs, for where no stretch may stand: a code
/// block or raw HTML.
pub(crate) fn strip_marker_runs(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some((start, _, end)) = next_marker(rest) {
        out.push_str(&rest[..start]);
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// A piece of an answer: text as written, or a stretch.
enum Segment {
    Text(String),
    Stretch(Stretch),
}

struct Stretch {
    /// The trimmed short form, line breaks as `↵`, after its parents' keys;
    /// a short form repeated among siblings takes `~2`, `~3`. Empty without
    /// a bar.
    key: String,
    short: String,
    /// `None` when no `|` was written.
    long: Option<Vec<Segment>>,
    /// False when the text ends before its `}}`.
    closed: bool,
    /// Its bytes in the text, braces included.
    source: Range<usize>,
}

/// A stretch being read, and the short forms its long form has used.
struct Frame {
    stretch: Stretch,
    siblings: HashMap<String, u32>,
}

/// The stretches read so far: the finished segments, the stretches still
/// open, innermost last, and the short forms used at the top level.
#[derive(Default)]
struct Tree {
    root: Vec<Segment>,
    stack: Vec<Frame>,
    root_siblings: HashMap<String, u32>,
}

/// `text` as segments, read as cc-stretch reads a reply.
fn parse(text: &str) -> Vec<Segment> {
    let mut tree = Tree::default();
    let mut at = 0;
    while at < text.len() {
        at += tree.step(text, at);
    }
    tree.finish(text.len())
}

impl Tree {
    /// Reads what starts at `at` and returns how many bytes it took.
    fn step(&mut self, text: &str, at: usize) -> usize {
        let rest = &text[at..];
        let line_start = at == 0 || text.as_bytes()[at - 1] == b'\n';
        if line_start && let Some(stop) = fence_end(text, at) {
            self.write(&text[at..stop]);
            return stop - at;
        }
        if rest.starts_with('`') {
            let stop = code_span_end(text, at);
            self.write(&text[at..stop]);
            return stop - at;
        }
        if rest.starts_with("{{") && !self.in_short() && self.stack.len() < MAX_DEPTH {
            self.open(at);
            return 2;
        }
        if rest.starts_with('|') && self.in_short() {
            self.divide();
            return 1;
        }
        if rest.starts_with("}}") && !self.stack.is_empty() {
            self.close(at + 2, true);
            return 2;
        }
        let run = plain_run(rest);
        self.write(&rest[..run]);
        run
    }

    fn in_short(&self) -> bool {
        self.stack
            .last()
            .is_some_and(|frame| frame.stretch.long.is_none())
    }

    /// Adds `chunk` to the short form being read, else to the innermost
    /// long form, else to the top level.
    fn write(&mut self, chunk: &str) {
        let segments = match self.stack.last_mut() {
            Some(frame) => match &mut frame.stretch.long {
                None => {
                    frame.stretch.short.push_str(chunk);
                    return;
                }
                Some(long) => long,
            },
            None => &mut self.root,
        };
        match segments.last_mut() {
            Some(Segment::Text(last)) => last.push_str(chunk),
            _ => segments.push(Segment::Text(chunk.to_owned())),
        }
    }

    fn open(&mut self, at: usize) {
        self.stack.push(Frame {
            stretch: Stretch {
                key: String::new(),
                short: String::new(),
                long: None,
                closed: false,
                source: at..at,
            },
            siblings: HashMap::new(),
        });
    }

    /// The first `|` of a short form: the stretch takes its key and starts
    /// its long form.
    fn divide(&mut self) {
        let Some((frame, parents)) = self.stack.split_last_mut() else {
            return;
        };
        let name = frame.stretch.short.trim().replace('\n', "\u{21B5}");
        let (siblings, prefix) = match parents.last_mut() {
            Some(Frame { stretch, siblings }) => (siblings, Some(stretch.key.as_str())),
            None => (&mut self.root_siblings, None),
        };
        let seen = siblings.entry(name.clone()).or_insert(0);
        *seen += 1;
        let own = if *seen == 1 {
            name
        } else {
            format!("{name}~{seen}")
        };
        frame.stretch.key = match prefix {
            Some(prefix) => format!("{prefix}{KEY_SEPARATOR}{own}"),
            None => own,
        };
        frame.stretch.long = Some(Vec::new());
    }

    /// Ends the innermost stretch at `end` into the segments around it.
    fn close(&mut self, end: usize, closed: bool) {
        let Some(mut frame) = self.stack.pop() else {
            return;
        };
        frame.stretch.closed = closed;
        frame.stretch.source.end = end;
        let segments = match self.stack.last_mut() {
            Some(Frame {
                stretch: Stretch {
                    long: Some(long), ..
                },
                ..
            }) => long,
            _ => &mut self.root,
        };
        segments.push(Segment::Stretch(frame.stretch));
    }

    /// The segments, every stretch still open ended unclosed at `len`.
    fn finish(mut self, len: usize) -> Vec<Segment> {
        while !self.stack.is_empty() {
            self.close(len, false);
        }
        self.root
    }
}

/// How many bytes of `rest` are plain text: up to the next byte that may
/// start a code span, a fence's line, or a stretch's brace or bar. A line
/// break is a run of its own, so a fence after it is seen.
fn plain_run(rest: &str) -> usize {
    if rest.starts_with('\n') {
        return 1;
    }
    rest.bytes()
        .skip(1)
        .position(|byte| matches!(byte, b'`' | b'{' | b'}' | b'|' | b'\n'))
        .map_or(rest.len(), |at| at + 1)
}

/// Where the code span opening at `at` ends: past the next run of as many
/// backticks, or at the text's end.
fn code_span_end(text: &str, at: usize) -> usize {
    let rest = &text[at..];
    let run = rest.len() - rest.trim_start_matches('`').len();
    rest[run..]
        .find(&rest[..run])
        .map_or(text.len(), |found| at + run + found + run)
}

/// Where a fenced code block opening at `start` ends (past its closing
/// line, or at the text's end), or `None` when no fence opens there.
fn fence_end(text: &str, start: usize) -> Option<usize> {
    let line_end = |from: usize| text[from..].find('\n').map_or(text.len(), |at| from + at);
    let first = line_end(start);
    let fence = opening_fence(&text[start..first])?;
    let mut at = first + 1;
    while at < text.len() {
        let end = line_end(at);
        if closes_fence(&text[at..end], fence) {
            return Some((end + 1).min(text.len()));
        }
        at = end + 1;
    }
    Some(text.len())
}

/// The run of three or more backticks or tildes `line` opens a fence with.
fn opening_fence(line: &str) -> Option<&str> {
    let body = line.trim_start_matches([' ', '\t']);
    let mark = body.chars().next().filter(|ch| matches!(ch, '`' | '~'))?;
    let run = body.len() - body.trim_start_matches(mark).len();
    (run >= 3).then(|| &body[..run])
}

/// Whether `line`, trimmed, is only `fence`'s character, as often or more.
fn closes_fence(line: &str, fence: &str) -> bool {
    let trimmed = line.trim();
    let mark = fence.chars().next();
    trimmed.starts_with(fence) && trimmed.chars().all(|ch| Some(ch) == mark)
}

/// Writes segments as Markdown with their stretches marked.
struct Composer<'a> {
    text: &'a str,
    open: &'a BTreeSet<String>,
    live: bool,
    keys: Vec<String>,
}

impl Composer<'_> {
    fn write(&mut self, parts: &[Segment]) -> String {
        let mut out = String::new();
        for part in parts {
            match part {
                Segment::Text(text) => out.push_str(text),
                Segment::Stretch(stretch) => out.push_str(&self.draw(stretch)),
            }
        }
        out
    }

    fn draw(&mut self, stretch: &Stretch) -> String {
        let raw = &self.text[stretch.source.clone()];
        if !stretch.closed {
            return if self.live {
                stretch.short.clone()
            } else {
                raw.to_owned()
            };
        }
        if stretch.short.trim().is_empty() {
            return raw.to_owned();
        }
        let Some(long) = stretch.long.as_deref().filter(|long| !is_blank(long)) else {
            return stretch.short.clone();
        };
        if stretch.short.contains('\n') {
            return self.write(long);
        }
        let index = self.keys.len();
        self.keys.push(stretch.key.clone());
        if self.open.contains(&stretch.key) {
            let long = self.write(long);
            return close_after(&long, &format!("{CLOSER_OPEN}{index}{CLOSER_END}"));
        }
        format!("{LINK_OPEN}{index}{LINK_SEP}{}{LINK_END}", stretch.short)
    }
}

/// Whether a long form holds nothing but white space.
fn is_blank(long: &[Segment]) -> bool {
    long.iter()
        .all(|part| matches!(part, Segment::Text(text) if text.trim().is_empty()))
}

/// An open stretch's long form followed by its `closer`, after a space, or
/// after a blank line when the long form ends in a rule or a fence's closing
/// line, which text after it would change.
fn close_after(long: &str, closer: &str) -> String {
    let trimmed = long.trim_end();
    let last = trimmed.rfind('\n').map_or(trimmed, |at| &trimmed[at + 1..]);
    if is_rule(last) || is_fence_line(last) {
        format!("{trimmed}\n\n{closer}")
    } else {
        format!("{long} {closer}")
    }
}

/// `line` without its indent of up to three spaces; `None` when indented
/// more.
fn unindented(line: &str) -> Option<&str> {
    let spaces = line.len() - line.trim_start_matches(' ').len();
    (spaces <= 3).then(|| &line[spaces..])
}

/// Whether `line` is a rule: three or more of one of `-`, `*` and `_`,
/// spaces between allowed.
fn is_rule(line: &str) -> bool {
    let Some(body) = unindented(line) else {
        return false;
    };
    let Some(mark) = body
        .chars()
        .next()
        .filter(|ch| matches!(ch, '-' | '*' | '_'))
    else {
        return false;
    };
    body.chars().all(|ch| ch == mark || ch == ' ' || ch == '\t')
        && body.chars().filter(|ch| *ch == mark).count() >= 3
}

/// Whether `line` is only a run of three or more backticks or tildes.
fn is_fence_line(line: &str) -> bool {
    let Some(body) = unindented(line) else {
        return false;
    };
    let body = body.trim_end_matches([' ', '\t']);
    let Some(mark) = body.chars().next().filter(|ch| matches!(ch, '`' | '~')) else {
        return false;
    };
    body.chars().all(|ch| ch == mark) && body.len() >= 3
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;

    fn set(keys: &[&str]) -> BTreeSet<String> {
        keys.iter().map(|key| (*key).to_owned()).collect()
    }

    /// `composed` with each marked short form written `[short](key)` and each
    /// closer `(‹key)`.
    fn shown(composed: &Composed) -> String {
        let mut out = String::new();
        let mut pending = Vec::new();
        let mut chars = composed.source.chars();
        while let Some(ch) = chars.next() {
            match ch {
                LINK_OPEN | CLOSER_OPEN => {
                    let end = if ch == LINK_OPEN {
                        LINK_SEP
                    } else {
                        CLOSER_END
                    };
                    let digits: String = chars.by_ref().take_while(|c| *c != end).collect();
                    let index: usize = digits.parse().expect("a marker carries its index");
                    let key = composed.keys[index].clone();
                    if ch == LINK_OPEN {
                        out.push('[');
                        pending.push(key);
                    } else {
                        out.push('(');
                        out.push_str(CLOSER);
                        out.push_str(&key);
                        out.push(')');
                    }
                }
                LINK_END => {
                    let key = pending.pop().expect("an end follows its start");
                    out.push_str("](");
                    out.push_str(&key);
                    out.push(')');
                }
                _ => out.push(ch),
            }
        }
        out
    }

    fn finished(text: &str, open: &[&str]) -> String {
        shown(&compose(text, &set(open), false))
    }

    fn live(text: &str, open: &[&str]) -> String {
        shown(&compose(text, &set(open), true))
    }

    const NESTED: &str = "{{A cache|The CI cache, {{keyed|keyed by hash}} only}}";

    #[test]
    fn a_closed_stretch_draws_its_short_form_as_a_link() {
        assert_eq!(
            finished("a {{short|long here}} b", &[]),
            "a [short](short) b"
        );
        assert_eq!(
            finished("a {{short|long here}} b", &["short"]),
            "a long here (\u{2039}short) b"
        );
    }

    #[test]
    fn a_nested_stretch_is_a_link_only_once_its_parent_opens() {
        assert_eq!(finished(NESTED, &[]), "[A cache](A cache)");
        assert_eq!(
            finished(NESTED, &["A cache"]),
            "The CI cache, [keyed](A cache \u{203A} keyed) only (\u{2039}A cache)"
        );
        assert_eq!(
            finished(NESTED, &["A cache", "A cache \u{203A} keyed"]),
            "The CI cache, keyed by hash (\u{2039}A cache \u{203A} keyed) only (\u{2039}A cache)"
        );
    }

    #[test]
    fn a_bar_inside_a_code_span_is_code_not_the_divider() {
        assert_eq!(
            finished("{{see `a|b`|long}}", &[]),
            "[see `a|b`](see `a|b`)"
        );
        assert_eq!(
            finished("{{x|run `a}}b` now}}", &["x"]),
            "run `a}}b` now (\u{2039}x)"
        );
    }

    #[test]
    fn a_stray_opening_is_raw_when_finished_and_hidden_while_live() {
        assert_eq!(finished("a {{ b", &[]), "a {{ b");
        assert_eq!(live("a {{ b", &[]), "a  b");
    }

    #[test]
    fn an_extra_closing_is_literal() {
        assert_eq!(finished("{{a|b}} }}", &[]), "[a](a) }}");
    }

    #[test]
    fn a_closing_inside_the_long_form_ends_it() {
        assert_eq!(
            finished("{{a|text with }} inside}}", &[]),
            "[a](a) inside}}"
        );
    }

    #[test]
    fn empty_forms_draw_raw_or_plain() {
        assert_eq!(finished("{{|long}}", &[]), "{{|long}}");
        assert_eq!(finished("{{short|}}", &[]), "short");
        assert_eq!(finished("{{short|  }}", &[]), "short");
        assert_eq!(finished("{{}}", &[]), "{{}}");
    }

    #[test]
    fn a_short_form_over_lines_draws_open_and_is_no_link() {
        let composed = compose("{{a\nb|c}}", &BTreeSet::new(), false);
        assert_eq!(shown(&composed), "c");
        assert_eq!(composed.keys, Vec::<String>::new());
    }

    #[test]
    fn a_barless_stretch_draws_its_text_plain() {
        assert_eq!(finished("x {{name}} y", &[]), "x name y");
    }

    #[test]
    fn an_unclosed_stretch_shows_its_short_form_while_live_and_raw_when_finished() {
        assert_eq!(live("see {{a|b", &[]), "see a");
        assert_eq!(finished("see {{a|b", &[]), "see {{a|b");
        assert_eq!(finished("see {{a|b {{c|d}} e", &[]), "see {{a|b {{c|d}} e");
    }

    #[test]
    fn private_use_markers_in_the_input_are_stripped() {
        let composed = compose(
            "a\u{E000}0\u{E001}b\u{E002}c\u{E003}0\u{E004}",
            &BTreeSet::new(),
            false,
        );
        assert_eq!(composed.source, "a0bc0");
        assert_eq!(composed.keys, Vec::<String>::new());
    }

    #[test]
    fn a_stretch_in_a_fenced_code_block_stays_verbatim() {
        let text = "```\n{{a|b}}\n```\n{{c|d}}";
        assert_eq!(finished(text, &[]), "```\n{{a|b}}\n```\n[c](c)");
        let tilde = "  ~~~~ rust\n{{a|b}}\n~~~~\n";
        assert_eq!(finished(tilde, &[]), tilde);
        let unclosed = "```\n{{a|b}}";
        assert_eq!(finished(unclosed, &[]), unclosed);
    }

    #[test]
    fn multibyte_text_around_a_stretch_keeps_its_bytes() {
        let text = "\u{fc}n\u{ef} {{caf\u{e9} \u{2615}|long \u{e9}}} \u{2713}";
        assert_eq!(
            finished(text, &[]),
            "\u{fc}n\u{ef} [caf\u{e9} \u{2615}](caf\u{e9} \u{2615}) \u{2713}"
        );
        assert_eq!(
            finished(text, &["caf\u{e9} \u{2615}"]),
            "\u{fc}n\u{ef} long \u{e9} (\u{2039}caf\u{e9} \u{2615}) \u{2713}"
        );
    }

    #[test]
    fn repeated_sibling_short_forms_take_a_count() {
        let composed = compose("{{x|1}} {{x|2}}", &BTreeSet::new(), false);
        assert_eq!(composed.keys, ["x", "x~2"]);
    }

    #[test]
    fn a_long_form_ending_in_a_rule_puts_its_closer_after_a_blank_line() {
        assert_eq!(
            finished("{{a|text\n\n---}}", &["a"]),
            "text\n\n---\n\n(\u{2039}a)"
        );
    }

    #[test]
    fn closing_a_stretch_closes_the_ones_nested_in_it() {
        let mut open = set(&["A", "A \u{203A} b", "A \u{203A} b \u{203A} c", "AB"]);
        close(&mut open, "A");
        assert_eq!(open, set(&["AB"]));
    }
}
