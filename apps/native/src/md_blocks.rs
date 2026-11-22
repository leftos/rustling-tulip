//! The Markdown view's block model: a document's blocks, each heading with
//! its unique slug, and inline spans with their style flags and link targets.
//! Plain Rust: no GPUI and no I/O.

use crate::links::{self, LinkKind};
use crate::open::validate_http_url;
use crate::stretchtext::{self, Marker};

use std::ops::Range;
use std::path::{Path, PathBuf};

use pulldown_cmark::{Alignment, CodeBlockKind, Event, LinkType, Options, Parser, Tag, TagEnd};

/// The largest Markdown file the viewer renders; a larger one opens as source.
pub(crate) const RENDER_CAP: usize = 512 * 1024;

/// The deepest a document's block quotes and lists are nested together;
/// anything opened deeper is flattened into the deepest frame already open, so
/// dropping the document cannot overflow the stack.
pub const MAX_NESTING: usize = 64;

/// A parsed Markdown document.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Document {
    /// The document's blocks, in order.
    pub(crate) blocks: Vec<Block>,
}

/// A block-level element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Block {
    /// A heading of level 1 to 6, with its slug and its spans.
    Heading {
        level: u8,
        slug: String,
        spans: Vec<Span>,
    },
    /// A paragraph.
    Paragraph(Vec<Span>),
    /// A list; `start` is the number an ordered list begins at, `None` for a
    /// bullet list.
    List {
        start: Option<u64>,
        items: Vec<ListItem>,
    },
    /// A block quote and the blocks inside it.
    Quote(Vec<Block>),
    /// A code block: the fence's language, when it names one, and its text.
    Code { lang: Option<String>, text: String },
    /// A thematic break.
    Rule,
    /// A table: each column's alignment, the header cells, then the body rows.
    Table {
        aligns: Vec<Align>,
        head: Vec<Vec<Span>>,
        rows: Vec<Vec<Vec<Span>>>,
    },
    /// Raw HTML, kept as literal text.
    Html(String),
}

/// One list item: its task-list state, when it is a task item, and its blocks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ListItem {
    /// `Some(true)` for a checked task item, `Some(false)` for an unchecked
    /// one, `None` for a plain item.
    pub(crate) task: Option<bool>,
    /// The item's blocks; a nested list is one of them.
    pub(crate) blocks: Vec<Block>,
}

/// A table column's alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Align {
    /// No alignment named.
    None,
    /// Left-aligned.
    Left,
    /// Centered.
    Center,
    /// Right-aligned.
    Right,
}

/// A run of inline text with its style flags and its link target.
#[expect(
    clippy::struct_excessive_bools,
    reason = "each style is set on its own and read separately by the renderer"
)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Span {
    /// The run's text.
    pub(crate) text: String,
    /// Bold.
    pub(crate) strong: bool,
    /// Italic.
    pub(crate) emphasis: bool,
    /// Struck through.
    pub(crate) strikethrough: bool,
    /// Inline code.
    pub(crate) code: bool,
    /// Drawn muted: an image's alt text.
    pub(crate) muted: bool,
    /// The link the run is part of, when it is in one.
    pub(crate) link: Option<LinkTarget>,
    /// The run's byte range in the source text.
    pub(crate) range: Range<usize>,
}

/// What a link's target is, and so what opening it does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LinkTarget {
    /// An `http://` or `https://` URL.
    Url(String),
    /// A file path, resolved against the file's own folder, and the `#fragment`
    /// the link named, if it named one.
    File {
        path: PathBuf,
        anchor: Option<String>,
    },
    /// A `#heading` anchor, matched against the headings' slugs.
    Anchor(String),
    /// A target the viewer never opens.
    Refused,
    /// A stretch of a chat answer, by its key: a click on its short form
    /// opens it, a click on its closer closes it.
    Stretchtext { key: String },
    /// A Linear issue: the identifier a click raises and the web address it
    /// opens.
    Issue { identifier: String, web_url: String },
}

/// Lifts the marker runs [`stretchtext::compose`] wrote out of `document`:
/// each closed stretch's short form becomes spans linked to
/// [`LinkTarget::Stretchtext`] with its key from `keys`, and each closer a
/// muted `‹` span with the same link. A start with no end among one run of
/// spans, or an index `keys` does not hold, leaves its text plain; a code
/// block or raw HTML only loses its markers.
pub(crate) fn lift_stretchtext(document: &mut Document, keys: &[String]) {
    for block in &mut document.blocks {
        lift_block(block, keys);
    }
}

fn lift_block(block: &mut Block, keys: &[String]) {
    match block {
        Block::Heading { spans, .. } | Block::Paragraph(spans) => lift_spans(spans, keys),
        Block::List { items, .. } => {
            for inner in items.iter_mut().flat_map(|item| item.blocks.iter_mut()) {
                lift_block(inner, keys);
            }
        }
        Block::Quote(blocks) => {
            for inner in blocks {
                lift_block(inner, keys);
            }
        }
        Block::Code { text, .. } | Block::Html(text) => {
            *text = stretchtext::strip_marker_runs(text);
        }
        Block::Table { head, rows, .. } => {
            for cell in head.iter_mut().chain(rows.iter_mut().flatten()) {
                lift_spans(cell, keys);
            }
        }
        Block::Rule => {}
    }
}

fn lift_spans(spans: &mut Vec<Span>, keys: &[String]) {
    if !spans
        .iter()
        .any(|span| span.text.contains(stretchtext::is_marker))
    {
        return;
    }
    let mut lifter = Lifter::default();
    for span in std::mem::take(spans) {
        lifter.span(&span, keys);
    }
    *spans = lifter.finish();
}

/// The spans lifted so far, and the short form being lifted: its key and
/// the index of its first span.
#[derive(Default)]
struct Lifter {
    out: Vec<Span>,
    link: Option<(String, usize)>,
}

impl Lifter {
    fn span(&mut self, span: &Span, keys: &[String]) {
        let mut rest = span.text.as_str();
        while let Some((start, marker, end)) = stretchtext::next_marker(rest) {
            self.piece(span, &rest[..start]);
            self.marker(span, marker, keys);
            rest = &rest[end..];
        }
        self.piece(span, rest);
    }

    /// `text` with `span`'s styles, linked to the short form being lifted
    /// unless it is already in a link.
    fn piece(&mut self, span: &Span, text: &str) {
        if text.is_empty() {
            return;
        }
        let link = match (&self.link, &span.link) {
            (Some((key, _)), None) => Some(LinkTarget::Stretchtext { key: key.clone() }),
            _ => span.link.clone(),
        };
        self.out.push(Span {
            text: text.to_owned(),
            strong: span.strong,
            emphasis: span.emphasis,
            strikethrough: span.strikethrough,
            code: span.code,
            muted: span.muted,
            link,
            range: span.range.clone(),
        });
    }

    fn marker(&mut self, span: &Span, marker: Marker, keys: &[String]) {
        match marker {
            Marker::LinkStart(index) => {
                self.unlink();
                self.link = keys.get(index).map(|key| (key.clone(), self.out.len()));
            }
            Marker::LinkEnd => self.link = None,
            Marker::Closer(index) => {
                if let Some(key) = keys.get(index) {
                    self.out.push(Span {
                        text: stretchtext::CLOSER.to_owned(),
                        strong: false,
                        emphasis: false,
                        strikethrough: false,
                        code: false,
                        muted: true,
                        link: Some(LinkTarget::Stretchtext { key: key.clone() }),
                        range: span.range.clone(),
                    });
                }
            }
            Marker::Stray => {}
        }
    }

    /// Ends a short form whose end never came: its spans lose the link.
    fn unlink(&mut self) {
        let Some((key, from)) = self.link.take() else {
            return;
        };
        for span in self.out.iter_mut().skip(from) {
            if matches!(&span.link, Some(LinkTarget::Stretchtext { key: own }) if *own == key) {
                span.link = None;
            }
        }
    }

    fn finish(mut self) -> Vec<Span> {
        self.unlink();
        self.out
    }
}

/// Links each path [`links::text_links`] finds in `document`'s prose and
/// inline code outside every link to [`LinkTarget::File`], as a chat answer
/// draws it: a code block, raw HTML and an image's alt text stay plain, and
/// a Markdown link or a stretch keeps its own target.
pub(crate) fn link_paths(document: &mut Document) {
    for block in &mut document.blocks {
        link_block(block);
    }
}

fn link_block(block: &mut Block) {
    match block {
        Block::Heading { spans, .. } | Block::Paragraph(spans) => link_spans(spans),
        Block::List { items, .. } => {
            for inner in items.iter_mut().flat_map(|item| item.blocks.iter_mut()) {
                link_block(inner);
            }
        }
        Block::Quote(blocks) => {
            for inner in blocks {
                link_block(inner);
            }
        }
        Block::Table { head, rows, .. } => {
            for cell in head.iter_mut().chain(rows.iter_mut().flatten()) {
                link_spans(cell);
            }
        }
        Block::Code { .. } | Block::Html(_) | Block::Rule => {}
    }
}

/// `spans` with each run of plain text joined into one span, so a path the
/// parser cut into several runs is read whole, then each path in a span
/// outside every link cut out as a span of its own, linked to it.
fn link_spans(spans: &mut Vec<Span>) {
    let mut out: Vec<Span> = Vec::with_capacity(spans.len());
    for span in join_plain(std::mem::take(spans)) {
        split_links(span, false, &mut out);
    }
    *spans = out;
}

/// `text`, a card's or a receipt's plain line, as spans: each path and web
/// address [`links::text_links`] finds in it a span linked to it.
pub(crate) fn linked_text_spans(text: &str) -> Vec<Span> {
    let whole = Span {
        text: text.to_owned(),
        strong: false,
        emphasis: false,
        strikethrough: false,
        code: false,
        muted: false,
        link: None,
        range: 0..text.len(),
    };
    let mut out = Vec::new();
    split_links(whole, true, &mut out);
    out
}

/// Whether `next` continues `last` as one run of plain text: both outside
/// every link, neither code nor muted, with the same styles.
fn joins(last: &Span, next: &Span) -> bool {
    let plain = |span: &Span| span.link.is_none() && !span.code && !span.muted;
    plain(last)
        && plain(next)
        && last.strong == next.strong
        && last.emphasis == next.emphasis
        && last.strikethrough == next.strikethrough
}

/// `spans` with each run that [`joins`] made one span.
fn join_plain(spans: Vec<Span>) -> Vec<Span> {
    let mut out: Vec<Span> = Vec::with_capacity(spans.len());
    for span in spans {
        match out.last_mut() {
            Some(last) if joins(last, &span) => {
                last.text.push_str(&span.text);
                last.range =
                    last.range.start.min(span.range.start)..last.range.end.max(span.range.end);
            }
            _ => out.push(span),
        }
    }
    out
}

/// Pushes `span` onto `out`, cut around each link [`links::text_links`]
/// finds in its text (web addresses too when `urls` is set), each link a
/// span of its own; a span already in a link, or muted, is pushed whole.
fn split_links(span: Span, urls: bool, out: &mut Vec<Span>) {
    if span.link.is_some() || span.muted {
        out.push(span);
        return;
    }
    let mut at = 0;
    for link in links::text_links(&span.text, urls) {
        let Some(target) = span
            .text
            .get(link.range.clone())
            .and_then(|text| text_target(text, link.kind))
        else {
            continue;
        };
        out.extend(piece(&span, at..link.range.start, None));
        out.extend(piece(&span, link.range.clone(), Some(target)));
        at = link.range.end;
    }
    out.extend(piece(&span, at..span.text.len(), None));
}

/// The part `range` of `span`'s text with its styles and `link`; none when
/// it is empty.
fn piece(span: &Span, range: Range<usize>, link: Option<LinkTarget>) -> Option<Span> {
    let text = span.text.get(range).filter(|text| !text.is_empty())?;
    Some(Span {
        text: text.to_owned(),
        link,
        range: span.range.clone(),
        ..*span
    })
}

/// What a link of `kind` found in plain text opens: a file at its path, or
/// a web address that passes the URL check.
fn text_target(text: &str, kind: LinkKind) -> Option<LinkTarget> {
    match kind {
        LinkKind::Path => Some(LinkTarget::File {
            path: PathBuf::from(text),
            anchor: None,
        }),
        LinkKind::Url => validate_http_url(text)
            .ok()
            .map(|url| LinkTarget::Url(url.to_owned())),
        LinkKind::Issue => None,
    }
}

/// Links each Linear issue [`links::text_issue_links`] finds in `document`'s
/// prose and inline code outside every link to [`LinkTarget::Issue`], as a
/// phone chat answer draws it: a code block, raw HTML and an image's alt text
/// stay plain, and a Markdown link or a stretch keeps its own target.
pub(crate) fn link_issues(document: &mut Document, context: &links::IssueContext) {
    for block in &mut document.blocks {
        issue_block(block, context);
    }
}

fn issue_block(block: &mut Block, context: &links::IssueContext) {
    match block {
        Block::Heading { spans, .. } | Block::Paragraph(spans) => issue_spans(spans, context),
        Block::List { items, .. } => {
            for inner in items.iter_mut().flat_map(|item| item.blocks.iter_mut()) {
                issue_block(inner, context);
            }
        }
        Block::Quote(blocks) => {
            for inner in blocks {
                issue_block(inner, context);
            }
        }
        Block::Table { head, rows, .. } => {
            for cell in head.iter_mut().chain(rows.iter_mut().flatten()) {
                issue_spans(cell, context);
            }
        }
        Block::Code { .. } | Block::Html(_) | Block::Rule => {}
    }
}

/// `spans` with each run of plain text joined into one span, so an identifier
/// the parser cut apart is read whole, then each issue in a span outside every
/// link cut out as a span of its own, linked to it.
fn issue_spans(spans: &mut Vec<Span>, context: &links::IssueContext) {
    let mut out: Vec<Span> = Vec::with_capacity(spans.len());
    for span in join_plain(std::mem::take(spans)) {
        split_issues(span, context, &mut out);
    }
    *spans = out;
}

/// Pushes `span` onto `out`, cut around each issue
/// [`links::text_issue_links`] finds in its text, each issue a span of its
/// own; a span already in a link, or muted, is pushed whole.
fn split_issues(span: Span, context: &links::IssueContext, out: &mut Vec<Span>) {
    if span.link.is_some() || span.muted {
        out.push(span);
        return;
    }
    let mut at = 0;
    for (range, identifier, web_url) in links::text_issue_links(&span.text, context) {
        out.extend(piece(&span, at..range.start, None));
        let target = LinkTarget::Issue {
            identifier,
            web_url,
        };
        out.extend(piece(&span, range.clone(), Some(target)));
        at = range.end;
    }
    out.extend(piece(&span, at..span.text.len(), None));
}

/// How a soft break, a single line break inside a paragraph, draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SoftBreaks {
    /// As a space, `CommonMark`'s reading, for a file viewer, release notes and
    /// any other Markdown source that wraps its lines.
    Space,
    /// As a line break, as the terminal and the phone draw it, for a Chat
    /// answer.
    Line,
}

/// The extensions [`parse`] reads a source as Markdown with.
fn parse_options() -> Options {
    Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS
}

/// `source` with every backslash in a Windows path's plain text doubled, so
/// `CommonMark`'s escape rule keeps it: an agent answer writes `X:\dev\.tmp`
/// meaning that backslash, where the parser reads `\.` as an escaped dot.
///
/// Code spans, code blocks, links, images and HTML are copied as written,
/// since there a `\.` is the author's escape; only a chat answer's text, which
/// is prose a model wrote rather than a Markdown file, goes through this. A
/// leading byte-order mark is dropped, as [`parse`] drops it, so both read the
/// same source.
#[must_use]
pub(crate) fn keep_path_backslashes(source: &str) -> String {
    let source = source.strip_prefix('\u{FEFF}').unwrap_or(source);
    let mut out = String::with_capacity(source.len());
    let mut at = 0;
    for skipped in skipped_ranges(source) {
        if at < skipped.start {
            out.push_str(&escape_windows_paths(&source[at..skipped.start]));
        }
        out.push_str(&source[skipped.start..skipped.end]);
        at = skipped.end;
    }
    if at < source.len() {
        out.push_str(&escape_windows_paths(&source[at..]));
    }
    out
}

/// `text` with every backslash of each Windows path token doubled, and
/// everything else as written. A token starts at a drive (`X:\`) or a UNC
/// (`\\server`) root that begins a word, and runs to the next white space.
fn escape_windows_paths(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut at = 0;
    let mut prev: Option<char> = None;
    while let Some(ch) = text[at..].chars().next() {
        if path_starts_at(prev, &text[at..]) {
            let end = token_end(text, at);
            out.push_str(&text[copied..at]);
            out.push_str(&escape_path_token(&text[at..end]));
            copied = end;
            prev = text[at..end].chars().next_back();
            at = end;
        } else {
            prev = Some(ch);
            at += ch.len_utf8();
        }
    }
    out.push_str(&text[copied..]);
    out
}

/// `token`, a Windows path, with its backslashes escaped for `CommonMark`: one
/// before an ASCII punctuation character becomes `\\\`, a literal backslash
/// followed by that character's own escape, so the punctuation stays literal
/// and does not open Markdown; every other one is doubled.
fn escape_path_token(token: &str) -> String {
    let mut out = String::with_capacity(token.len() * 2);
    let mut chars = token.chars().peekable();
    while let Some(ch) = chars.next() {
        let escapes_markdown = chars
            .peek()
            .is_some_and(|next| next.is_ascii_punctuation() && *next != '\\');
        if ch == '\\' && escapes_markdown {
            out.push_str("\\\\\\");
            continue;
        }
        if ch == '\\' {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Whether a Windows path token starts at the beginning of `rest`, whose
/// previous character is `prev`: a drive path (`X:\`) or a UNC path (`\\server`),
/// each at the start of a word.
fn path_starts_at(prev: Option<char>, rest: &str) -> bool {
    let mut chars = rest.chars();
    match (chars.next(), chars.next()) {
        (Some(letter), Some(':')) if letter.is_ascii_alphabetic() => {
            chars.next() == Some('\\') && prev.is_none_or(|before| !before.is_alphanumeric())
        }
        (Some('\\'), Some('\\')) => {
            let server = chars
                .next()
                .is_some_and(|after| !after.is_whitespace() && after != '\\');
            server && prev.is_none_or(|before| !before.is_alphanumeric() && before != '\\')
        }
        _ => false,
    }
}

/// The end of the token that starts at byte `start` of `text`: the next white
/// space, or the text's end.
fn token_end(text: &str, start: usize) -> usize {
    text[start..]
        .find(char::is_whitespace)
        .map_or(text.len(), |offset| start + offset)
}

/// The byte ranges of `source` [`keep_path_backslashes`] copies as written:
/// inline code, code blocks, links with their destination, images and HTML.
fn skipped_ranges(source: &str) -> Vec<Range<usize>> {
    let mut skips = Skips::default();
    let mut events = Parser::new_ext(source, parse_options()).into_offset_iter();
    for (event, range) in events.by_ref() {
        match event {
            Event::Start(tag) => skips.start(&tag, range),
            Event::End(tag) => skips.end(tag, range),
            Event::Code(_) | Event::Html(_) | Event::InlineHtml(_) => skips.ranges.push(range),
            _ => {}
        }
    }
    let definitions = events
        .reference_definitions()
        .iter()
        .map(|(_, definition)| definition.span.clone());
    skips.ranges.extend(definitions);
    merge_ranges(skips.ranges)
}

/// The ranges [`skipped_ranges`] collects, and the construct each open one
/// began, waiting for its end.
#[derive(Default)]
struct Skips {
    ranges: Vec<Range<usize>>,
    block: Option<Range<usize>>,
    link: Nest,
    image: Nest,
}

impl Skips {
    /// Begins an open construct when `tag` starts one to copy as written.
    fn start(&mut self, tag: &Tag<'_>, range: Range<usize>) {
        match tag {
            Tag::CodeBlock(_) | Tag::HtmlBlock => self.block = Some(range),
            Tag::Link { .. } => self.link.enter(range),
            Tag::Image { .. } => self.image.enter(range),
            _ => {}
        }
    }

    /// Closes the open construct `tag` ends, recording its whole range when it
    /// is the outermost one still open.
    fn end(&mut self, tag: TagEnd, range: Range<usize>) {
        let whole = match tag {
            TagEnd::CodeBlock | TagEnd::HtmlBlock => self
                .block
                .take()
                .map(|open| open.start.min(range.start)..open.end.max(range.end)),
            TagEnd::Link => self.link.leave(range),
            TagEnd::Image => self.image.leave(range),
            _ => None,
        };
        if let Some(whole) = whole {
            self.ranges.push(whole);
        }
    }
}

/// A construct that may nest, such as an image inside an image: how many are
/// open, and where the outermost one began.
#[derive(Default)]
struct Nest {
    depth: usize,
    open: Option<Range<usize>>,
}

impl Nest {
    /// Enters the construct at `range`, recording the range only at the
    /// outermost depth.
    fn enter(&mut self, range: Range<usize>) {
        self.depth += 1;
        if self.depth == 1 {
            self.open = Some(range);
        }
    }

    /// Leaves the construct at `range`, giving the whole construct's range
    /// once the outermost one closes.
    fn leave(&mut self, range: Range<usize>) -> Option<Range<usize>> {
        self.depth = self.depth.saturating_sub(1);
        if self.depth > 0 {
            return None;
        }
        let open = self.open.take()?;
        Some(open.start.min(range.start)..open.end.max(range.end))
    }
}

/// `ranges` in order, each one that touches or overlaps the one before it
/// joined into it.
fn merge_ranges(mut ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    ranges.sort_unstable_by_key(|range| range.start);
    let mut out: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for range in ranges {
        match out.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => out.push(range),
        }
    }
    out
}

/// Parses `text`, a Markdown file's contents, into its blocks.
///
/// `file_dir` is the folder the file was read from: a relative link's path is
/// resolved against it. `soft_breaks` says how a single line break inside a
/// paragraph draws.
#[must_use]
pub(crate) fn parse(text: &str, file_dir: &Path, soft_breaks: SoftBreaks) -> Document {
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    let mut builder = Builder::new(file_dir, soft_breaks);
    for (event, range) in Parser::new_ext(text, parse_options()).into_offset_iter() {
        builder.event(event, range);
    }
    builder.finish()
}

/// Whether a document of `len` bytes is rendered rather than shown as source.
#[must_use]
pub(crate) fn renders(len: usize) -> bool {
    len <= RENDER_CAP
}

/// Classifies a link `target`: a URL, a file beside `file_dir`, an in-document
/// anchor, or a target that is refused.
///
/// Only `http` and `https` are URLs. Every other scheme is refused, except a
/// one-letter scheme, which is a Windows drive (`C:\notes.md`) and so a file. A
/// relative path is percent-decoded and resolved against `file_dir`, with any
/// `#fragment` split off.
#[must_use]
pub(crate) fn classify_link(target: &str, file_dir: &Path) -> LinkTarget {
    let target = target.trim_matches(|ch: char| ch.is_whitespace() || ch.is_control());
    let (path, anchor) = split_fragment(target);
    if path.is_empty() {
        return match anchor {
            Some(fragment) => LinkTarget::Anchor(fragment),
            None => LinkTarget::Refused,
        };
    }
    if let Some(scheme) = scheme(path) {
        if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") {
            return match validate_http_url(target) {
                Ok(url) => LinkTarget::Url(url.to_owned()),
                Err(_) => LinkTarget::Refused,
            };
        }
        if is_drive_scheme(scheme) {
            return LinkTarget::File {
                path: PathBuf::from(path),
                anchor,
            };
        }
        return LinkTarget::Refused;
    }
    let decoded = percent_decode(path);
    if is_network_path(&decoded) {
        return LinkTarget::Refused;
    }
    LinkTarget::File {
        path: file_dir.join(decoded),
        anchor,
    }
}

/// Whether `path` names a network location: two leading separators, in any mix
/// of `/` and `\`, as in `//host/share` or `\\host\share`.
fn is_network_path(path: &str) -> bool {
    let mut chars = path.chars();
    matches!(
        (chars.next(), chars.next()),
        (Some(first), Some(second)) if is_separator(first) && is_separator(second)
    )
}

/// Whether `ch` separates path components on either platform.
fn is_separator(ch: char) -> bool {
    matches!(ch, '/' | '\\')
}

/// A heading's GitHub-style slug: lower-cased, spaces to `-`, punctuation and
/// inline-code marks dropped.
#[must_use]
pub(crate) fn slug(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            out.extend(ch.to_lowercase());
        } else if ch == '-' || ch == '_' {
            out.push(ch);
        } else if ch.is_whitespace() {
            out.push('-');
        }
    }
    out
}

/// Splits a `#fragment` off a link target, percent-decoding and lower-casing
/// it; an empty target or fragment is no fragment at all.
fn split_fragment(target: &str) -> (&str, Option<String>) {
    match target.split_once('#') {
        Some((path, fragment)) if !fragment.is_empty() => {
            (path, Some(percent_decode(fragment).to_lowercase()))
        }
        Some((path, _)) => (path, None),
        None => (target, None),
    }
}

/// The scheme `path` names, when it names one: the text before a `:` that no
/// path separator comes before.
fn scheme(path: &str) -> Option<&str> {
    let colon = path.find(':')?;
    let before = &path[..colon];
    if before.is_empty() || before.contains(['/', '\\', '?']) {
        return None;
    }
    Some(before)
}

/// Whether `scheme` is one letter, and so a Windows drive rather than a scheme.
fn is_drive_scheme(scheme: &str) -> bool {
    let mut chars = scheme.chars();
    matches!(
        (chars.next(), chars.next()),
        (Some(letter), None) if letter.is_ascii_alphabetic()
    )
}

/// `text` with `%XX` escapes decoded; an escape that is not valid hex is kept
/// as it is.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%'
            && at + 2 < bytes.len()
            && let (Some(high), Some(low)) = (hex_value(bytes[at + 1]), hex_value(bytes[at + 2]))
        {
            out.push(high * 16 + low);
            at += 3;
            continue;
        }
        out.push(bytes[at]);
        at += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The value of an ASCII hex digit.
fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// The alignment of a table column.
fn align_of(alignment: Alignment) -> Align {
    match alignment {
        Alignment::None => Align::None,
        Alignment::Left => Align::Left,
        Alignment::Center => Align::Center,
        Alignment::Right => Align::Right,
    }
}

/// A fenced code block's language: the first word of its info string, without
/// a comma-separated suffix.
fn info_language(info: &str) -> Option<String> {
    let word = info.split_whitespace().next()?.split(',').next()?;
    (!word.is_empty()).then(|| word.to_ascii_lowercase())
}

/// A container the builder is filling: the document root, a block quote or a
/// list item.
enum Frame {
    /// Blocks being collected, with a list item's task state.
    Blocks {
        blocks: Vec<Block>,
        task: Option<bool>,
    },
    /// A list between its `Item` tags, holding its finished items.
    List {
        start: Option<u64>,
        items: Vec<ListItem>,
    },
    /// A table between its `Table` tags.
    Table(TableBuild),
}

/// The table a [`Frame::Table`] is filling.
#[derive(Default)]
struct TableBuild {
    aligns: Vec<Align>,
    head: Vec<Vec<Span>>,
    rows: Vec<Vec<Vec<Span>>>,
    row: Vec<Vec<Span>>,
    in_head: bool,
}

/// An image's alt text, collected between its `Image` tags, and the source
/// range of the image.
struct ImageBuf {
    text: String,
    range: Range<usize>,
}

/// Builds [`Document`] from pulldown-cmark's event stream.
///
/// Nesting goes through an explicit stack, never recursion, so a deeply nested
/// document cannot overflow the stack.
struct Builder<'a> {
    file_dir: &'a Path,
    soft_breaks: SoftBreaks,
    stack: Vec<Frame>,
    spans: Vec<Span>,
    strong: u32,
    emphasis: u32,
    strikethrough: u32,
    links: Vec<LinkTarget>,
    image: Option<ImageBuf>,
    image_depth: u32,
    html: Option<String>,
    code: Option<(Option<String>, String)>,
    heading: Option<u8>,
    slugs: Vec<String>,
    nesting: usize,
    skipped: u32,
}

impl<'a> Builder<'a> {
    /// A builder whose stack holds only the document root.
    fn new(file_dir: &'a Path, soft_breaks: SoftBreaks) -> Self {
        Self {
            file_dir,
            soft_breaks,
            stack: vec![Frame::Blocks {
                blocks: Vec::new(),
                task: None,
            }],
            spans: Vec::new(),
            strong: 0,
            emphasis: 0,
            strikethrough: 0,
            links: Vec::new(),
            image: None,
            image_depth: 0,
            html: None,
            code: None,
            heading: None,
            slugs: Vec::new(),
            nesting: 0,
            skipped: 0,
        }
    }

    /// The finished document.
    fn finish(mut self) -> Document {
        self.flush_spans();
        let blocks = match self.stack.pop() {
            Some(Frame::Blocks { blocks, .. }) => blocks,
            Some(Frame::List { .. } | Frame::Table(_)) | None => Vec::new(),
        };
        Document { blocks }
    }

    /// Folds one parser event into the document being built.
    fn event(&mut self, event: Event<'_>, range: Range<usize>) {
        match event {
            Event::Start(tag) => self.start(tag, range),
            Event::End(tag) => self.end(tag),
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                self.text(&text, range);
            }
            Event::Code(code) => self.code_span(&code, range),
            Event::SoftBreak => {
                let ch = match self.soft_breaks {
                    SoftBreaks::Space => ' ',
                    SoftBreaks::Line => '\n',
                };
                self.break_span(ch, range);
            }
            Event::HardBreak => self.break_span('\n', range),
            Event::Rule => self.rule(),
            Event::TaskListMarker(checked) => self.mark_task(checked),
            Event::InlineMath(_) | Event::DisplayMath(_) | Event::FootnoteReference(_) => {}
        }
    }

    /// Opens the element `tag` starts.
    fn start(&mut self, tag: Tag<'_>, range: Range<usize>) {
        match tag {
            Tag::Heading { level, .. } => {
                self.flush_spans();
                self.heading = Some(level as u8);
            }
            Tag::BlockQuote(_) => self.begin_quote(),
            Tag::CodeBlock(kind) => self.begin_code(kind),
            Tag::HtmlBlock => self.begin_html(),
            Tag::List(start) => self.begin_list(start),
            Tag::Item => self.begin_item(),
            Tag::Table(aligns) => self.begin_table(&aligns),
            Tag::TableHead => self.set_in_head(true),
            Tag::TableRow => self.begin_row(),
            Tag::TableCell => self.flush_spans(),
            Tag::Emphasis => self.emphasis += 1,
            Tag::Strong => self.strong += 1,
            Tag::Strikethrough => self.strikethrough += 1,
            Tag::Link {
                link_type,
                dest_url,
                ..
            } => {
                let target = if link_type == LinkType::Email {
                    LinkTarget::Refused
                } else {
                    classify_link(&dest_url, self.file_dir)
                };
                self.links.push(target);
            }
            Tag::Image { .. } => self.begin_image(range),
            Tag::Paragraph
            | Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::Superscript
            | Tag::Subscript
            | Tag::MetadataBlock(_) => {}
        }
    }

    /// Closes the element `tag` ends.
    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.flush_spans(),
            TagEnd::Heading(_) => self.end_heading(),
            TagEnd::BlockQuote(_) => self.end_quote(),
            TagEnd::CodeBlock => self.end_code(),
            TagEnd::HtmlBlock => self.end_html(),
            TagEnd::List(_) => self.end_list(),
            TagEnd::Item => self.end_item(),
            TagEnd::Table => self.end_table(),
            TagEnd::TableHead => self.set_in_head(false),
            TagEnd::TableRow => self.end_row(),
            TagEnd::TableCell => self.end_cell(),
            TagEnd::Emphasis => self.emphasis = self.emphasis.saturating_sub(1),
            TagEnd::Strong => self.strong = self.strong.saturating_sub(1),
            TagEnd::Strikethrough => self.strikethrough = self.strikethrough.saturating_sub(1),
            TagEnd::Link => {
                self.links.pop();
            }
            TagEnd::Image => self.end_image(),
            TagEnd::FootnoteDefinition
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::Superscript
            | TagEnd::Subscript
            | TagEnd::MetadataBlock(_) => {}
        }
    }

    /// A text run, or one of the HTML runs kept as literal text.
    fn text(&mut self, text: &str, range: Range<usize>) {
        if let Some(image) = &mut self.image {
            image.text.push_str(text);
            return;
        }
        if let Some((_, code)) = &mut self.code {
            code.push_str(text);
            return;
        }
        if let Some(html) = &mut self.html {
            html.push_str(text);
            return;
        }
        let span = self.make_span(text, range, false);
        self.spans.push(span);
    }

    /// An inline code run.
    fn code_span(&mut self, code: &str, range: Range<usize>) {
        if let Some(image) = &mut self.image {
            image.text.push_str(code);
            return;
        }
        if let Some((_, text)) = &mut self.code {
            text.push_str(code);
            return;
        }
        let span = self.make_span(code, range, true);
        self.spans.push(span);
    }

    /// A soft or hard break, as a space or a newline; an image's alt text
    /// takes a space either way.
    fn break_span(&mut self, ch: char, range: Range<usize>) {
        if let Some(image) = &mut self.image {
            image.text.push(' ');
            return;
        }
        if let Some(html) = &mut self.html {
            html.push(ch);
            return;
        }
        let span = self.make_span(&ch.to_string(), range, false);
        self.spans.push(span);
    }

    /// A thematic break.
    fn rule(&mut self) {
        self.flush_spans();
        self.push_block(Block::Rule);
    }

    /// A task list marker, recorded on the list item being filled.
    fn mark_task(&mut self, checked: bool) {
        if let Some(Frame::Blocks { task, .. }) = self.stack.last_mut() {
            *task = Some(checked);
        }
    }

    /// A span with the styles and link that are open now.
    fn make_span(&self, text: &str, range: Range<usize>, code: bool) -> Span {
        Span {
            text: text.to_owned(),
            strong: self.strong > 0,
            emphasis: self.emphasis > 0,
            strikethrough: self.strikethrough > 0,
            code,
            muted: false,
            link: self.links.last().cloned(),
            range,
        }
    }

    /// Opens a container for a block quote or a list item, ending the inline
    /// run before it.
    fn begin_blocks(&mut self) {
        self.flush_spans();
        self.stack.push(Frame::Blocks {
            blocks: Vec::new(),
            task: None,
        });
    }

    /// Opens a block quote, or counts it as skipped at the nesting cap.
    fn begin_quote(&mut self) {
        if self.skip_container() {
            return;
        }
        self.nesting += 1;
        self.begin_blocks();
    }

    /// Opens a list, or counts it as skipped at the nesting cap.
    fn begin_list(&mut self, start: Option<u64>) {
        if self.skip_container() {
            return;
        }
        self.nesting += 1;
        self.flush_spans();
        self.stack.push(Frame::List {
            start,
            items: Vec::new(),
        });
    }

    /// Opens a list item, or counts it as skipped when its list is.
    fn begin_item(&mut self) {
        if self.skipped > 0 {
            self.skipped += 1;
            return;
        }
        self.begin_blocks();
    }

    /// Whether a quote or list opened now is flattened into the deepest frame
    /// open, counted so that its end is skipped too.
    fn skip_container(&mut self) -> bool {
        if self.skipped > 0 || self.nesting >= MAX_NESTING {
            self.skipped += 1;
            return true;
        }
        false
    }

    /// Whether the end arriving now closes a skipped container.
    fn close_skipped(&mut self) -> bool {
        if self.skipped > 0 {
            self.skipped -= 1;
            return true;
        }
        false
    }

    /// Opens a code block, ending the inline run before it.
    fn begin_code(&mut self, kind: CodeBlockKind<'_>) {
        self.flush_spans();
        let lang = match kind {
            CodeBlockKind::Indented => None,
            CodeBlockKind::Fenced(info) => info_language(&info),
        };
        self.code = Some((lang, String::new()));
    }

    /// Opens a block of raw HTML, ending the inline run before it.
    fn begin_html(&mut self) {
        self.flush_spans();
        self.html = Some(String::new());
    }

    /// Opens a table, ending the inline run before it.
    fn begin_table(&mut self, aligns: &[Alignment]) {
        self.flush_spans();
        self.stack.push(Frame::Table(TableBuild {
            aligns: aligns.iter().copied().map(align_of).collect(),
            ..TableBuild::default()
        }));
    }

    /// Opens a table row.
    fn begin_row(&mut self) {
        if let Some(Frame::Table(table)) = self.stack.last_mut() {
            table.row = Vec::new();
        }
    }

    /// Records whether the table rows that follow are its header.
    fn set_in_head(&mut self, in_head: bool) {
        if let Some(Frame::Table(table)) = self.stack.last_mut() {
            table.in_head = in_head;
        }
    }

    /// Opens an image's alt text; an image nested in another only adds its alt
    /// text to the one around it.
    fn begin_image(&mut self, range: Range<usize>) {
        self.image_depth += 1;
        if self.image_depth == 1 {
            self.image = Some(ImageBuf {
                text: String::new(),
                range,
            });
        }
    }

    /// Closes a heading, with its unique slug.
    fn end_heading(&mut self) {
        let level = self.heading.take().unwrap_or(1);
        let spans = std::mem::take(&mut self.spans);
        let text: String = spans.iter().map(|span| span.text.as_str()).collect();
        let slug = self.unique_slug(&text);
        self.push_block(Block::Heading { level, slug, spans });
    }

    /// `text`'s slug, with `-1`, `-2`, … appended until it is unique.
    fn unique_slug(&mut self, text: &str) -> String {
        let base = slug(text);
        if !self.slugs.contains(&base) {
            self.slugs.push(base.clone());
            return base;
        }
        let mut suffix = 1;
        loop {
            let candidate = format!("{base}-{suffix}");
            if !self.slugs.contains(&candidate) {
                self.slugs.push(candidate.clone());
                return candidate;
            }
            suffix += 1;
        }
    }

    /// Closes a block quote around the blocks collected inside it.
    fn end_quote(&mut self) {
        if self.close_skipped() {
            return;
        }
        self.nesting = self.nesting.saturating_sub(1);
        self.flush_spans();
        if let Some(Frame::Blocks { blocks, .. }) = self.stack.pop() {
            self.push_block(Block::Quote(blocks));
        }
    }

    /// Closes a code block, without the newline its last line ends in.
    fn end_code(&mut self) {
        let Some((lang, mut text)) = self.code.take() else {
            return;
        };
        if text.ends_with('\n') {
            text.pop();
        }
        self.push_block(Block::Code { lang, text });
    }

    /// Closes a block of raw HTML.
    fn end_html(&mut self) {
        let Some(html) = self.html.take() else {
            return;
        };
        let text = html.trim_end_matches('\n');
        if !text.is_empty() {
            self.push_block(Block::Html(text.to_owned()));
        }
    }

    /// Closes a list around the items collected inside it.
    fn end_list(&mut self) {
        if self.close_skipped() {
            return;
        }
        self.nesting = self.nesting.saturating_sub(1);
        self.flush_spans();
        if let Some(Frame::List { start, items }) = self.stack.pop() {
            self.push_block(Block::List { start, items });
        }
    }

    /// Closes a list item into the list around it.
    fn end_item(&mut self) {
        if self.close_skipped() {
            return;
        }
        self.flush_spans();
        if let Some(Frame::Blocks { blocks, task }) = self.stack.pop()
            && let Some(Frame::List { items, .. }) = self.stack.last_mut()
        {
            items.push(ListItem { task, blocks });
        }
    }

    /// Closes a table, padding its cells out to its column count.
    fn end_table(&mut self) {
        let Some(Frame::Table(table)) = self.stack.pop() else {
            return;
        };
        let width = table.aligns.len();
        let mut head = table.head;
        let mut rows = table.rows;
        pad_cells(&mut head, width);
        for row in &mut rows {
            pad_cells(row, width);
        }
        self.push_block(Block::Table {
            aligns: table.aligns,
            head,
            rows,
        });
    }

    /// Closes a table row into the table around it; header rows are already
    /// in its head.
    fn end_row(&mut self) {
        if let Some(Frame::Table(table)) = self.stack.last_mut() {
            if table.in_head {
                return;
            }
            let row = std::mem::take(&mut table.row);
            table.rows.push(row);
        }
    }

    /// Closes a table cell into its row, or into the table's head.
    fn end_cell(&mut self) {
        let spans = std::mem::take(&mut self.spans);
        if let Some(Frame::Table(table)) = self.stack.last_mut() {
            if table.in_head {
                table.head.push(spans);
            } else {
                table.row.push(spans);
            }
        }
    }

    /// Closes an image into its alt text span, muted, or its `[image]` label.
    fn end_image(&mut self) {
        if self.image_depth == 0 {
            return;
        }
        self.image_depth -= 1;
        if self.image_depth > 0 {
            return;
        }
        let Some(image) = self.image.take() else {
            return;
        };
        let text = if image.text.trim().is_empty() {
            "[image]".to_owned()
        } else {
            image.text
        };
        let mut span = self.make_span(&text, image.range, false);
        span.muted = true;
        self.spans.push(span);
    }

    /// Pushes the inline run collected so far as a paragraph, when it has any
    /// spans.
    fn flush_spans(&mut self) {
        if self.spans.is_empty() {
            return;
        }
        let spans = std::mem::take(&mut self.spans);
        self.push_block(Block::Paragraph(spans));
    }

    /// Pushes a block into the container being filled.
    fn push_block(&mut self, block: Block) {
        if let Some(Frame::Blocks { blocks, .. }) = self.stack.last_mut() {
            blocks.push(block);
        }
    }
}

/// Pads `cells` out to `width` with empty cells.
fn pad_cells(cells: &mut Vec<Vec<Span>>, width: usize) {
    while cells.len() < width {
        cells.push(Vec::new());
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn docs_dir() -> &'static Path {
        Path::new("notes")
    }

    fn parse_doc(text: &str) -> Document {
        parse(text, docs_dir(), SoftBreaks::Space)
    }

    fn texts(spans: &[Span]) -> String {
        spans.iter().map(|span| span.text.as_str()).collect()
    }

    fn heading_at(blocks: &[Block], index: usize) -> (u8, String) {
        match &blocks[index] {
            Block::Heading { level, slug, .. } => Some((*level, slug.clone())),
            _ => None,
        }
        .expect("a heading block")
    }

    fn spans_at(blocks: &[Block], index: usize) -> &[Span] {
        match &blocks[index] {
            Block::Paragraph(spans) => Some(spans.as_slice()),
            _ => None,
        }
        .expect("a paragraph block")
    }

    fn list_at(blocks: &[Block], index: usize) -> (Option<u64>, Vec<ListItem>) {
        match &blocks[index] {
            Block::List { start, items } => Some((*start, items.clone())),
            _ => None,
        }
        .expect("a list block")
    }

    fn quote_at(blocks: &[Block], index: usize) -> Vec<Block> {
        match &blocks[index] {
            Block::Quote(inner) => Some(inner.clone()),
            _ => None,
        }
        .expect("a block quote block")
    }

    fn code_at(blocks: &[Block], index: usize) -> (Option<String>, String) {
        match &blocks[index] {
            Block::Code { lang, text } => Some((lang.clone(), text.clone())),
            _ => None,
        }
        .expect("a code block")
    }

    type TableParts = (Vec<Align>, Vec<Vec<Span>>, Vec<Vec<Vec<Span>>>);

    fn table_at(blocks: &[Block], index: usize) -> TableParts {
        match &blocks[index] {
            Block::Table { aligns, head, rows } => {
                Some((aligns.clone(), head.clone(), rows.clone()))
            }
            _ => None,
        }
        .expect("a table block")
    }

    fn nesting_depth(blocks: &[Block]) -> usize {
        let mut stack: Vec<(&[Block], usize)> = vec![(blocks, 0)];
        let mut deepest = 0;
        while let Some((level, depth)) = stack.pop() {
            deepest = deepest.max(depth);
            for block in level {
                match block {
                    Block::Quote(inner) => stack.push((inner, depth + 1)),
                    Block::List { items, .. } => {
                        for item in items {
                            stack.push((&item.blocks, depth + 1));
                        }
                    }
                    _ => {}
                }
            }
        }
        deepest
    }

    #[test]
    fn headings_lists_and_code_blocks_become_blocks() {
        let doc =
            parse_doc("# Title\n\nText *em* here.\n\n- one\n- two\n\n```rust\nfn main() {}\n```\n");
        assert_eq!(doc.blocks.len(), 4, "{:?}", doc.blocks);
        assert_eq!(heading_at(&doc.blocks, 0), (1, "title".to_owned()));
        assert_eq!(texts(spans_at(&doc.blocks, 1)), "Text em here.");
        let (start, items) = list_at(&doc.blocks, 2);
        assert_eq!(start, None);
        assert_eq!(items.len(), 2);
        assert_eq!(texts(spans_at(&items[0].blocks, 0)), "one");
        assert_eq!(texts(spans_at(&items[1].blocks, 0)), "two");
        let (lang, code) = code_at(&doc.blocks, 3);
        assert_eq!(lang.as_deref(), Some("rust"));
        assert_eq!(code, "fn main() {}");
    }

    #[test]
    fn a_javascript_link_is_refused() {
        let doc = parse_doc("[click](javascript:alert(1))\n");
        let spans = spans_at(&doc.blocks, 0);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].link, Some(LinkTarget::Refused));
    }

    #[test]
    fn an_email_autolink_is_refused() {
        let doc = parse_doc("<me@example.com>\n");
        let spans = spans_at(&doc.blocks, 0);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].link, Some(LinkTarget::Refused));
    }

    #[test]
    fn a_file_link_resolves_against_the_files_folder() {
        let dir = docs_dir();
        assert_eq!(
            classify_link("sub/notes.md", dir),
            LinkTarget::File {
                path: dir.join("sub/notes.md"),
                anchor: None
            }
        );
        assert_eq!(
            classify_link("a%20b.md#Part", dir),
            LinkTarget::File {
                path: dir.join("a b.md"),
                anchor: Some("part".to_owned())
            }
        );
        assert_eq!(
            classify_link("C:\\notes\\x.md", dir),
            LinkTarget::File {
                path: PathBuf::from("C:\\notes\\x.md"),
                anchor: None
            }
        );
    }

    #[test]
    fn protocol_relative_and_unc_targets_are_refused() {
        let dir = docs_dir();
        for target in [
            "//host/share/x.md",
            "\\\\host\\share\\x.md",
            "%2F%2Fhost%2Fshare",
            "%5C%5Chost%5Cshare",
        ] {
            assert_eq!(classify_link(target, dir), LinkTarget::Refused, "{target}");
        }
    }

    #[test]
    fn an_anchor_names_its_heading_slug() {
        let doc = parse_doc("# Intro\n\nBody\n");
        assert_eq!(heading_at(&doc.blocks, 0), (1, "intro".to_owned()));
        assert_eq!(
            classify_link("#intro", docs_dir()),
            LinkTarget::Anchor("intro".to_owned())
        );
    }

    #[test]
    fn html_is_kept_as_text() {
        let doc = parse_doc("<div>hi</div>\n\ntext\n");
        assert_eq!(doc.blocks.len(), 2);
        assert_eq!(doc.blocks[0], Block::Html("<div>hi</div>".to_owned()));
        assert_eq!(texts(spans_at(&doc.blocks, 1)), "text");
    }

    #[test]
    fn a_table_keeps_column_alignment() {
        let doc =
            parse_doc("| left | center | right |\n|:-----|:------:|------:|\n| a | b | c |\n");
        let (aligns, head, rows) = table_at(&doc.blocks, 0);
        assert_eq!(aligns, vec![Align::Left, Align::Center, Align::Right]);
        let head_texts: Vec<String> = head.iter().map(|cell| texts(cell)).collect();
        assert_eq!(head_texts, vec!["left", "center", "right"]);
        assert_eq!(rows.len(), 1);
        let row_texts: Vec<String> = rows[0].iter().map(|cell| texts(cell)).collect();
        assert_eq!(row_texts, vec!["a", "b", "c"]);
    }

    #[test]
    fn an_image_becomes_its_alt_text() {
        let doc = parse_doc("![a cat](cat.png)\n");
        let spans = spans_at(&doc.blocks, 0);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "a cat");
        assert!(spans[0].muted);
        assert_eq!(spans[0].link, None);
    }

    #[test]
    fn an_image_alt_text_keeps_a_break_as_a_space() {
        let doc = parse("![a\nb](x.png)\n", docs_dir(), SoftBreaks::Line);
        assert_eq!(spans_at(&doc.blocks, 0)[0].text, "a b");
    }

    #[test]
    fn a_large_document_is_source_only() {
        assert_eq!(RENDER_CAP, 512 * 1024);
        assert!(renders(RENDER_CAP));
        assert!(!renders(RENDER_CAP + 1));
    }

    #[test]
    fn an_https_and_http_link_classify_as_urls() {
        let dir = docs_dir();
        assert_eq!(
            classify_link("http://example.com/a?b=1", dir),
            LinkTarget::Url("http://example.com/a?b=1".to_owned())
        );
        assert_eq!(
            classify_link("https://example.com", dir),
            LinkTarget::Url("https://example.com".to_owned())
        );
        assert_eq!(
            classify_link("HTTPS://example.com", dir),
            LinkTarget::Url("HTTPS://example.com".to_owned())
        );
    }

    #[test]
    fn a_file_scheme_link_is_refused() {
        assert_eq!(
            classify_link("file:///C:/x.md", docs_dir()),
            LinkTarget::Refused
        );
        assert_eq!(
            classify_link("FILE:///C:/x.md", docs_dir()),
            LinkTarget::Refused
        );
    }

    #[test]
    fn a_data_and_vbscript_link_is_refused() {
        assert_eq!(
            classify_link("data:text/html,<b>x</b>", docs_dir()),
            LinkTarget::Refused
        );
        assert_eq!(
            classify_link("vbscript:msgbox(1)", docs_dir()),
            LinkTarget::Refused
        );
    }

    #[test]
    fn a_mailto_link_is_refused() {
        assert_eq!(
            classify_link("mailto:me@example.com", docs_dir()),
            LinkTarget::Refused
        );
    }

    #[test]
    fn a_scheme_check_ignores_case_and_leading_space() {
        let dir = docs_dir();
        assert_eq!(
            classify_link("JaVaScRiPt:alert(1)", dir),
            LinkTarget::Refused
        );
        assert_eq!(
            classify_link(" javascript:alert(1)", dir),
            LinkTarget::Refused
        );
        assert_eq!(
            classify_link("java\u{7}script:alert(1)", dir),
            LinkTarget::Refused
        );
    }

    #[test]
    fn an_empty_link_target_is_refused_or_anchorless() {
        let dir = docs_dir();
        assert_eq!(classify_link("", dir), LinkTarget::Refused);
        assert_eq!(classify_link("   ", dir), LinkTarget::Refused);
        assert_eq!(classify_link("#", dir), LinkTarget::Refused);
    }

    #[test]
    fn nested_lists_keep_their_depth_and_order() {
        let doc = parse_doc("- a\n  - b\n  - c\n- d\n");
        let (start, items) = list_at(&doc.blocks, 0);
        assert_eq!(start, None);
        assert_eq!(items.len(), 2);
        assert_eq!(texts(spans_at(&items[0].blocks, 0)), "a");
        let (inner_start, inner) = list_at(&items[0].blocks, 1);
        assert_eq!(inner_start, None);
        assert_eq!(inner.len(), 2);
        assert_eq!(texts(spans_at(&inner[0].blocks, 0)), "b");
        assert_eq!(texts(spans_at(&inner[1].blocks, 0)), "c");
        assert_eq!(texts(spans_at(&items[1].blocks, 0)), "d");
    }

    #[test]
    fn an_ordered_list_keeps_its_start_number() {
        let doc = parse_doc("3. a\n4. b\n");
        let (start, items) = list_at(&doc.blocks, 0);
        assert_eq!(start, Some(3));
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn a_task_list_item_records_checked_and_unchecked() {
        let doc = parse_doc("- [x] done\n- [ ] todo\n");
        let (_, items) = list_at(&doc.blocks, 0);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].task, Some(true));
        assert_eq!(items[1].task, Some(false));
        assert_eq!(texts(spans_at(&items[0].blocks, 0)), "done");
        assert_eq!(texts(spans_at(&items[1].blocks, 0)), "todo");
    }

    #[test]
    fn a_loose_list_item_holds_several_paragraphs() {
        let doc = parse_doc("- one\n\n  two\n");
        let (_, items) = list_at(&doc.blocks, 0);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].blocks.len(), 2);
        assert_eq!(texts(spans_at(&items[0].blocks, 0)), "one");
        assert_eq!(texts(spans_at(&items[0].blocks, 1)), "two");
    }

    #[test]
    fn a_fenced_code_block_keeps_its_language_and_text() {
        let doc = parse_doc("```python\nx = 1\ny = 2\n```\n");
        let (lang, code) = code_at(&doc.blocks, 0);
        assert_eq!(lang.as_deref(), Some("python"));
        assert_eq!(code, "x = 1\ny = 2");
    }

    #[test]
    fn an_info_string_takes_only_the_first_word() {
        let comma = parse_doc("```rust,ignore\nfn x() {}\n```\n");
        let (lang, _) = code_at(&comma.blocks, 0);
        assert_eq!(lang.as_deref(), Some("rust"));
        let titled = parse_doc("```rust title=x\nfn x() {}\n```\n");
        let (lang, _) = code_at(&titled.blocks, 0);
        assert_eq!(lang.as_deref(), Some("rust"));
    }

    #[test]
    fn an_indented_code_block_has_no_language() {
        let doc = parse_doc("    indented\n");
        let (lang, code) = code_at(&doc.blocks, 0);
        assert_eq!(lang, None);
        assert_eq!(code, "indented");
    }

    #[test]
    fn a_fence_without_a_language_is_plain() {
        let doc = parse_doc("```\nplain\n```\n");
        let (lang, code) = code_at(&doc.blocks, 0);
        assert_eq!(lang, None);
        assert_eq!(code, "plain");
    }

    #[test]
    fn a_code_block_keeps_blank_lines_and_no_trailing_newline_added() {
        let doc = parse_doc("```\na\n\nb\n```\n");
        let (lang, code) = code_at(&doc.blocks, 0);
        assert_eq!(lang, None);
        assert_eq!(code, "a\n\nb");
    }

    #[test]
    fn inline_code_is_one_code_span() {
        let doc = parse_doc("use `parse` now\n");
        let spans = spans_at(&doc.blocks, 0);
        assert_eq!(spans.len(), 3);
        assert!(spans[1].code);
        assert_eq!(spans[1].text, "parse");
        assert!(!spans[0].code);
    }

    #[test]
    fn emphasis_inside_a_link_keeps_both_the_style_and_the_target() {
        let source = "[a *b* c](https://example.com)";
        let doc = parse_doc(source);
        let spans = spans_at(&doc.blocks, 0);
        assert_eq!(spans.len(), 3);
        let target = LinkTarget::Url("https://example.com".to_owned());
        assert_eq!(spans[0].link, Some(target.clone()));
        assert_eq!(spans[1].link, Some(target.clone()));
        assert_eq!(spans[2].link, Some(target));
        assert!(spans[1].emphasis);
        assert!(!spans[0].emphasis);
        for span in spans {
            assert_eq!(&source[span.range.clone()], span.text, "{span:?}");
        }
    }

    #[test]
    fn strong_and_emphasis_nest_into_flags() {
        let doc = parse_doc("***both***\n");
        let spans = spans_at(&doc.blocks, 0);
        assert_eq!(spans.len(), 1);
        assert!(spans[0].strong && spans[0].emphasis);
        assert_eq!(spans[0].text, "both");
    }

    #[test]
    fn strikethrough_sets_its_flag() {
        let doc = parse_doc("~~gone~~\n");
        let spans = spans_at(&doc.blocks, 0);
        assert_eq!(spans.len(), 1);
        assert!(spans[0].strikethrough);
        assert_eq!(spans[0].text, "gone");
    }

    #[test]
    fn a_hard_break_and_a_soft_break_differ() {
        let soft = parse_doc("a\nb\n");
        let hard = parse_doc("a  \nb\n");
        let soft_spans = spans_at(&soft.blocks, 0);
        let hard_spans = spans_at(&hard.blocks, 0);
        assert_eq!(soft_spans.len(), 3);
        assert_eq!(hard_spans.len(), 3);
        assert_eq!(texts(soft_spans), "a b");
        assert_eq!(texts(hard_spans), "a\nb");
    }

    #[test]
    fn a_soft_break_is_a_line_break_in_line_mode() {
        let line = parse("a\nb\n", docs_dir(), SoftBreaks::Line);
        assert_eq!(texts(spans_at(&line.blocks, 0)), "a\nb");
    }

    #[test]
    fn raw_inline_html_is_kept_as_text() {
        let doc = parse_doc("a <b>c</b> d\n");
        assert_eq!(doc.blocks.len(), 1);
        assert_eq!(texts(spans_at(&doc.blocks, 0)), "a <b>c</b> d");
    }

    #[test]
    fn a_block_html_comment_is_kept_as_text() {
        let doc = parse_doc("<!-- note -->\n\n<script>alert(1)</script>\n");
        assert_eq!(doc.blocks.len(), 2);
        assert_eq!(&doc.blocks[0], &Block::Html("<!-- note -->".to_owned()));
        assert_eq!(
            &doc.blocks[1],
            &Block::Html("<script>alert(1)</script>".to_owned())
        );
    }

    #[test]
    fn a_blockquote_holds_nested_blocks() {
        let doc = parse_doc("> # Tip\n>\n> some *text*\n");
        let inner = quote_at(&doc.blocks, 0);
        assert_eq!(inner.len(), 2);
        assert_eq!(heading_at(&inner, 0), (1, "tip".to_owned()));
        assert_eq!(texts(spans_at(&inner, 1)), "some text");
        assert!(spans_at(&inner, 1)[1].emphasis);
    }

    #[test]
    fn a_thematic_break_becomes_a_rule() {
        let doc = parse_doc("a\n\n---\n\nb\n");
        assert_eq!(doc.blocks.len(), 3);
        assert_eq!(doc.blocks[1], Block::Rule);
    }

    #[test]
    fn a_table_cell_holds_inline_styles() {
        let doc = parse_doc("| h |\n|---|\n| **b** |\n");
        let (_, _, rows) = table_at(&doc.blocks, 0);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0].len(), 1);
        assert!(rows[0][0][0].strong);
        assert_eq!(rows[0][0][0].text, "b");
    }

    #[test]
    fn a_ragged_table_row_is_padded_to_the_header_width() {
        let doc = parse_doc("| a | b | c |\n|---|---|---|\n| only |\n");
        let (_, head, rows) = table_at(&doc.blocks, 0);
        assert_eq!(head.len(), 3);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].len(), 3);
        assert_eq!(texts(&rows[0][0]), "only");
        assert_eq!(rows[0][1], Vec::new());
        assert_eq!(rows[0][2], Vec::new());
    }

    #[test]
    fn an_image_inside_a_link_keeps_the_link_and_the_alt_text() {
        let doc = parse_doc("[![alt](img.png)](https://example.com)\n");
        let spans = spans_at(&doc.blocks, 0);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "alt");
        assert!(spans[0].muted);
        assert_eq!(
            spans[0].link,
            Some(LinkTarget::Url("https://example.com".to_owned()))
        );
    }

    #[test]
    fn a_nested_image_keeps_the_whole_alt_text() {
        let doc = parse_doc("![foo ![bar](/u)](/u2)\n");
        let spans = spans_at(&doc.blocks, 0);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "foo bar");
        assert!(spans[0].muted);
    }

    #[test]
    fn an_image_with_empty_alt_shows_a_placeholder_label() {
        let doc = parse_doc("![](x.png)\n");
        let spans = spans_at(&doc.blocks, 0);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "[image]");
        assert!(spans[0].muted);
    }

    #[test]
    fn duplicate_headings_get_suffixed_slugs() {
        let doc = parse_doc("# Same\n\n# Same\n\n# Same\n");
        let slugs: Vec<String> = (0..3)
            .map(|index| heading_at(&doc.blocks, index).1)
            .collect();
        assert_eq!(slugs, vec!["same", "same-1", "same-2"]);
        assert_eq!(doc.blocks.len(), 3, "one block per heading");
    }

    #[test]
    fn a_heading_slug_drops_punctuation_and_inline_code_marks() {
        let doc = parse_doc("## Use `parse` now!\n");
        let (level, heading_slug) = heading_at(&doc.blocks, 0);
        assert_eq!(level, 2);
        assert_eq!(heading_slug, "use-parse-now");
        assert_eq!(slug("Hello, World!"), "hello-world");
    }

    #[test]
    fn an_anchor_link_to_a_missing_slug_still_classifies_as_anchor() {
        assert_eq!(
            classify_link("#not-there", docs_dir()),
            LinkTarget::Anchor("not-there".to_owned())
        );
    }

    #[test]
    fn an_empty_document_has_no_blocks() {
        for text in ["", "\n\n", "  \n\n   \n"] {
            let doc = parse_doc(text);
            assert!(doc.blocks.is_empty(), "{text:?} -> {:?}", doc.blocks);
        }
    }

    #[test]
    fn malformed_markdown_never_panics() {
        let unclosed = parse_doc("```rust\nfn x() {\n");
        let (lang, code) = code_at(&unclosed.blocks, 0);
        assert_eq!(lang.as_deref(), Some("rust"));
        assert_eq!(code, "fn x() {");

        let unmatched = parse_doc("an * unmatched\n");
        assert!(texts(spans_at(&unmatched.blocks, 0)).contains("an * unmatched"));

        let bracket = parse_doc("[x](\n");
        assert!(texts(spans_at(&bracket.blocks, 0)).contains("[x]("));

        let pipe = parse_doc("| a |\n");
        assert_eq!(texts(spans_at(&pipe.blocks, 0)), "| a |");

        let deep = parse_doc(">>>>>>>> deep\n");
        assert_eq!(nesting_depth(&deep.blocks), 8);
    }

    #[test]
    fn a_very_long_document_parses_within_the_cap() {
        let paragraphs = 80_000;
        let text = "para\n\n".repeat(paragraphs);
        assert!(renders(text.len()));
        let doc = parse_doc(&text);
        assert_eq!(doc.blocks.len(), paragraphs);
        assert!(
            doc.blocks
                .iter()
                .all(|block| matches!(block, Block::Paragraph(_)))
        );
    }

    #[test]
    fn a_document_over_the_cap_is_source_only() {
        assert!(!renders(2 * RENDER_CAP));
        assert!(!renders(usize::MAX / 2));
    }

    #[test]
    fn nesting_is_capped_at_max_nesting() {
        let quotes = format!("{}word\n", "> ".repeat(RENDER_CAP / 2));
        let doc = parse_doc(&quotes);
        let depth = nesting_depth(&doc.blocks);
        assert!(depth <= MAX_NESTING, "quote depth {depth}");
        drop(doc);

        let mut list = String::new();
        for level in 0..MAX_NESTING * 4 {
            list.push_str(&"  ".repeat(level));
            list.push_str("- item\n");
        }
        let doc = parse_doc(&list);
        let depth = nesting_depth(&doc.blocks);
        assert!(depth <= MAX_NESTING, "list depth {depth}");
        drop(doc);
    }

    #[test]
    fn crlf_input_parses_like_lf() {
        let crlf = parse_doc("a\r\n\r\nb\r\n");
        let lf = parse_doc("a\n\nb\n");
        assert_eq!(crlf.blocks.len(), 2);
        assert_eq!(lf.blocks.len(), 2);
        assert_eq!(
            texts(spans_at(&crlf.blocks, 0)),
            texts(spans_at(&lf.blocks, 0))
        );
        assert_eq!(texts(spans_at(&crlf.blocks, 1)), "b");
    }

    #[test]
    fn a_utf8_bom_at_the_start_is_not_text() {
        let doc = parse_doc("\u{FEFF}# Title\n");
        assert_eq!(doc.blocks.len(), 1);
        assert_eq!(heading_at(&doc.blocks, 0), (1, "title".to_owned()));
    }

    /// `text` as a chat answer reads it: composed with no stretch open,
    /// parsed with line breaks, its stretches lifted and its paths linked.
    fn answer_doc(text: &str) -> Document {
        let composed = stretchtext::compose(text, &std::collections::BTreeSet::new(), false);
        let source = keep_path_backslashes(&composed.source);
        let mut doc = parse(&source, Path::new(""), SoftBreaks::Line);
        lift_stretchtext(&mut doc, &composed.keys);
        link_paths(&mut doc);
        doc
    }

    /// Each span of `spans` in a link: its text, whether it is code, and its
    /// target.
    fn linked(spans: &[Span]) -> Vec<(String, bool, LinkTarget)> {
        spans
            .iter()
            .filter_map(|span| {
                span.link
                    .clone()
                    .map(|link| (span.text.clone(), span.code, link))
            })
            .collect()
    }

    fn file(path: &str) -> LinkTarget {
        LinkTarget::File {
            path: PathBuf::from(path),
            anchor: None,
        }
    }

    #[test]
    fn a_windows_path_in_an_inline_code_span_is_one_file_link() {
        let path = r"X:\dev\city-life.wt\city-142-ui-mocks\.tmp\mocks\screens\index.html";
        let doc = answer_doc(&format!("Open `{path}` to see the four screens"));
        let spans = spans_at(&doc.blocks, 0);
        assert_eq!(linked(spans), [(path.to_owned(), true, file(path))]);
        assert_eq!(texts(spans), format!("Open {path} to see the four screens"));
    }

    #[test]
    fn a_unix_path_in_prose_is_a_file_link() {
        let doc = answer_doc("The log is at /var/log/app.log, see it.");
        let spans = spans_at(&doc.blocks, 0);
        assert_eq!(
            linked(spans),
            [(
                "/var/log/app.log".to_owned(),
                false,
                file("/var/log/app.log")
            )]
        );
        assert_eq!(texts(spans), "The log is at /var/log/app.log, see it.");
    }

    #[test]
    fn home_and_relative_root_paths_are_file_links() {
        let item_links = |path: &str| {
            let doc = answer_doc(&format!("- read {path} first\n"));
            let (_, items) = list_at(&doc.blocks, 0);
            let item = items.first().expect("one item");
            linked(spans_at(&item.blocks, 0))
        };
        for path in ["./notes/a.md", "../up/b.rs", "./x", "../x"] {
            assert_eq!(
                item_links(path),
                [(path.to_owned(), false, file(path))],
                "{path}"
            );
        }
        // Nothing expands `~`, so a home path would never open.
        for path in ["~/.claude/c.md", "~/x", r"~\x"] {
            assert_eq!(item_links(path), [], "{path} is plain");
        }
    }

    #[test]
    fn a_single_segment_absolute_word_in_prose_is_not_linked() {
        for word in ["/compact", "/clear", "/model", "/tmp"] {
            let doc = answer_doc(&format!("Run {word} and `{word}` now."));
            assert_eq!(linked(spans_at(&doc.blocks, 0)), [], "{word} is plain");
        }
        for path in ["/etc/hosts", "/a/b", r"X:\a", "X:/a"] {
            let doc = answer_doc(&format!("Open {path} now."));
            assert_eq!(
                linked(spans_at(&doc.blocks, 0)),
                [(path.to_owned(), false, file(path))],
                "{path} links"
            );
        }
    }

    #[test]
    fn a_windows_path_in_prose_keeps_its_backslashes_and_links() {
        let path = r"X:\dev\.tmp\a.rs";
        let doc = answer_doc(&format!("see {path} now"));
        let spans = spans_at(&doc.blocks, 0);
        assert_eq!(texts(spans), format!("see {path} now"));
        assert_eq!(linked(spans), [(path.to_owned(), false, file(path))]);
    }

    #[test]
    fn a_windows_path_with_an_underscore_folder_keeps_its_backslash() {
        let path = r"C:\Users\x\_notes";
        let doc = answer_doc(path);
        let spans = spans_at(&doc.blocks, 0);
        assert_eq!(texts(spans), path);
        assert_eq!(linked(spans), [(path.to_owned(), false, file(path))]);
    }

    #[test]
    fn a_unc_windows_path_keeps_its_backslashes_and_is_not_linked() {
        let path = r"\\server\share\.git\x";
        let doc = answer_doc(path);
        let spans = spans_at(&doc.blocks, 0);
        assert_eq!(texts(spans), path);
        assert_eq!(linked(spans), []);
    }

    #[test]
    fn a_windows_path_in_a_code_span_is_drawn_as_written() {
        let path = r"X:\dev\.tmp\a.rs";
        let doc = answer_doc(&format!("Open `{path}` to see"));
        let spans = spans_at(&doc.blocks, 0);
        assert_eq!(texts(spans), format!("Open {path} to see"));
        assert_eq!(linked(spans), [(path.to_owned(), true, file(path))]);
    }

    #[test]
    fn a_windows_path_in_a_fenced_block_is_drawn_as_written() {
        let doc = answer_doc("```\nX:\\dev\\.tmp\\a.rs\n```\n");
        assert_eq!(
            code_at(&doc.blocks, 0),
            (None, r"X:\dev\.tmp\a.rs".to_owned())
        );
    }

    #[test]
    fn a_markdown_escape_outside_a_windows_path_still_escapes() {
        let doc = answer_doc(r"a \* star and \_ under");
        let spans = spans_at(&doc.blocks, 0);
        assert_eq!(texts(spans), "a * star and _ under");
        assert_eq!(linked(spans), []);
    }

    #[test]
    fn a_trailing_dot_after_a_windows_path_is_outside_its_link() {
        let path = r"X:\dev\.tmp\a.rs";
        let doc = answer_doc(&format!("open {path}."));
        let spans = spans_at(&doc.blocks, 0);
        assert_eq!(texts(spans), format!("open {path}."));
        assert_eq!(linked(spans), [(path.to_owned(), false, file(path))]);
    }

    #[test]
    fn a_windows_path_in_link_text_keeps_commonmarks_reading() {
        let doc = answer_doc(r"[X:\dev\.tmp\a.rs](https://example.com)");
        let spans = spans_at(&doc.blocks, 0);
        assert_eq!(texts(spans), r"X:\dev.tmp\a.rs");
        let target = LinkTarget::Url("https://example.com".to_owned());
        assert!(
            spans.iter().all(|span| span.link == Some(target.clone())),
            "{spans:?}"
        );
    }

    #[test]
    fn a_bold_windows_path_keeps_its_backslashes_and_links() {
        let path = r"X:\dev\.tmp\a.rs";
        let doc = answer_doc(&format!("**{path}**"));
        let spans = spans_at(&doc.blocks, 0);
        assert_eq!(texts(spans), path);
        assert!(spans.iter().all(|span| span.strong), "{spans:?}");
        assert_eq!(linked(spans), [(path.to_owned(), false, file(path))]);
    }

    #[test]
    fn a_windows_path_with_forward_slashes_is_drawn_as_written() {
        let path = "X:/dev/.tmp/a.rs";
        let doc = answer_doc(&format!("see {path}"));
        let spans = spans_at(&doc.blocks, 0);
        assert_eq!(texts(spans), format!("see {path}"));
        assert_eq!(linked(spans), [(path.to_owned(), false, file(path))]);
    }

    #[test]
    fn a_windows_path_with_a_star_keeps_its_star_literal() {
        let first = r"X:\dev\.tmp\*.log";
        let second = r"X:\dev\.tmp\*.json";
        let doc = answer_doc(&format!("Deleted {first} and {second}"));
        let spans = spans_at(&doc.blocks, 0);
        assert_eq!(texts(spans), format!("Deleted {first} and {second}"));
        assert!(
            spans.iter().all(|span| !span.emphasis && !span.strong),
            "{spans:?}"
        );
        assert_eq!(
            linked(spans),
            [
                (first.to_owned(), false, file(first)),
                (second.to_owned(), false, file(second)),
            ]
        );
    }

    #[test]
    fn a_windows_path_in_a_fenced_block_after_a_bom_is_drawn_as_written() {
        let doc = answer_doc("\u{FEFF}```\nX:\\dev\\.tmp\\a.rs\n```\n");
        assert_eq!(
            code_at(&doc.blocks, 0),
            (None, r"X:\dev\.tmp\a.rs".to_owned())
        );
    }

    #[test]
    fn a_windows_path_in_a_nested_image_destination_is_left_as_written() {
        let source = r"![a ![b](x.png)](X:\dev\.tmp\c.png)";
        assert_eq!(keep_path_backslashes(source), source);
    }

    #[test]
    fn a_windows_path_in_a_reference_definition_is_left_as_written() {
        let source = "See [the log][1].\n\n[1]: X:\\dev\\.tmp\\run.log";
        assert_eq!(keep_path_backslashes(source), source);
    }

    #[test]
    fn a_lone_double_backslash_is_not_a_windows_path_start() {
        let doc = answer_doc(r"Windows uses \\ as the separator");
        let spans = spans_at(&doc.blocks, 0);
        assert_eq!(texts(spans), r"Windows uses \ as the separator");
    }

    #[test]
    fn a_path_in_a_fenced_block_is_not_linked() {
        let doc = answer_doc("```\ncat /etc/hosts\n```\n");
        assert_eq!(code_at(&doc.blocks, 0), (None, "cat /etc/hosts".to_owned()));
    }

    #[test]
    fn a_slash_command_in_prose_is_not_linked() {
        let doc = answer_doc("Run /reload-plugins and `/code-review` now.");
        assert_eq!(linked(spans_at(&doc.blocks, 0)), []);
    }

    #[test]
    fn a_bare_name_and_a_relative_path_are_not_linked() {
        let doc = answer_doc("Edit main.rs in src/bin and notes.md.");
        assert_eq!(linked(spans_at(&doc.blocks, 0)), []);
    }

    #[test]
    fn a_markdown_link_text_keeps_its_link() {
        let doc = answer_doc("See [/tmp/x.log](https://example.com/log) now.");
        let target = LinkTarget::Url("https://example.com/log".to_owned());
        assert_eq!(
            linked(spans_at(&doc.blocks, 0)),
            [("/tmp/x.log".to_owned(), false, target)]
        );
    }

    #[test]
    fn a_stretchtext_short_form_is_unchanged() {
        let doc = answer_doc("{{Logs in /tmp/x.log|the long story}} done\n");
        let spans = spans_at(&doc.blocks, 0);
        let links = linked(spans);
        assert!(!links.is_empty(), "the short form is a link: {spans:?}");
        assert!(
            links
                .iter()
                .all(|(_, _, target)| matches!(target, LinkTarget::Stretchtext { .. })),
            "every link is the stretch's: {links:?}"
        );
    }

    #[test]
    fn a_cards_line_links_paths_and_web_addresses() {
        let spans = linked_text_spans("Keep /tmp/a.txt or see https://example.com/x.");
        assert_eq!(
            linked(&spans),
            [
                ("/tmp/a.txt".to_owned(), false, file("/tmp/a.txt")),
                (
                    "https://example.com/x".to_owned(),
                    false,
                    LinkTarget::Url("https://example.com/x".to_owned())
                ),
            ]
        );
        assert_eq!(
            texts(&spans),
            "Keep /tmp/a.txt or see https://example.com/x."
        );
        assert_eq!(linked(&linked_text_spans("Pick one")), []);
    }

    #[test]
    fn a_cards_backticked_path_links() {
        let spans = linked_text_spans("Keep `/tmp/a.txt` as is");
        assert_eq!(
            linked(&spans),
            [("/tmp/a.txt".to_owned(), false, file("/tmp/a.txt"))]
        );
        assert_eq!(texts(&spans), "Keep `/tmp/a.txt` as is");
    }

    /// A host's context with Linear on, its team `RT` and workspace `ws`
    /// known.
    fn issue_context() -> links::IssueContext {
        links::IssueContext::new(&["RT".to_owned()], Some("ws"), true)
    }

    #[test]
    fn link_issues_links_prose_and_inline_code_not_a_code_block() {
        let mut doc = answer_doc("See RT-12 and `RT-13`.\n\n```\nRT-14\n```\n");
        link_issues(&mut doc, &issue_context());
        let issue = |identifier: &str| LinkTarget::Issue {
            identifier: identifier.to_owned(),
            web_url: format!("https://linear.app/ws/issue/{identifier}"),
        };
        let spans = spans_at(&doc.blocks, 0);
        let links: Vec<(String, Option<LinkTarget>)> = spans
            .iter()
            .map(|span| (span.text.clone(), span.link.clone()))
            .collect();
        assert_eq!(
            links,
            vec![
                ("See ".to_owned(), None),
                ("RT-12".to_owned(), Some(issue("RT-12"))),
                (" and ".to_owned(), None),
                ("RT-13".to_owned(), Some(issue("RT-13"))),
                (".".to_owned(), None),
            ]
        );
        let (_, code) = code_at(&doc.blocks, 1);
        assert_eq!(code, "RT-14", "a code block stays plain");
    }

    #[test]
    fn link_issues_keeps_a_markdown_links_own_target() {
        let mut doc = answer_doc("[RT-12](https://linear.app/ws/issue/RT-12)\n");
        link_issues(&mut doc, &issue_context());
        let spans = spans_at(&doc.blocks, 0);
        assert_eq!(texts(spans), "RT-12");
        assert_eq!(spans.len(), 1, "{spans:?}");
        assert_eq!(
            spans[0].link,
            Some(LinkTarget::Url(
                "https://linear.app/ws/issue/RT-12".to_owned()
            ))
        );
    }
}
