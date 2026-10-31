//! The viewer's rendered Markdown: a document parsed off the UI thread and
//! flattened into rows (one per code line, list item and table row, one for
//! every other block), its code coloured as the source view colours a file,
//! drawn by a list that builds only the rows in view; and the header's
//! Rendered | Source toggle, which switches the clicked tab and saves the
//! default new tabs open in.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use gpui::{
    AnyElement, ClickEvent, Context, Div, ElementId, FontStyle, FontWeight, HighlightStyle, Hsla,
    InteractiveText, ListOffset, ListState, SharedString, StrikethroughStyle, StyledText,
    UnderlineStyle, Window, div, list, prelude::*, px,
};

use crate::buttons::{ButtonSize, Layer, outlined_button, primary_button};
use crate::fonts;
use crate::links::TerminalLinkCandidate;
use crate::md_blocks::{self, Align, Block, LinkTarget, Span};
use crate::notices::ToastKind;
use crate::open::OpenJob;
use crate::open_view::{COULD_NOT_OPEN, Origin, Then};
use crate::stretchtext::Composed;
use crate::syntax::{self, Highlighted, LineSpans, TokenClass};
use crate::ui_theme::{UiTheme, theme};
use crate::viewer_tab::{MarkdownMode, ViewerLoad, ViewerOrigin, ViewerTab};
use crate::{RootView, UI_TEXT_SIZE, tooltip};

/// The tooltip of the toggle a document over the render cap disables.
pub(crate) const TOO_LARGE_TO_RENDER: &str = "Too large to render as Markdown";

/// The detail of the toast a refused link raises.
const REFUSED_LINK: &str = "Only http and https links open from a Markdown file.";

/// The room left and right of the rows, and above the first.
const BODY_PAD_X: f32 = 24.0;
const BODY_PAD_TOP: f32 = 16.0;
/// The room after a block.
const BLOCK_GAP: f32 = 10.0;
/// The room after a list item.
const ITEM_GAP: f32 = 4.0;
/// A list item's marker column, and the room right of the marker; a nested
/// row is indented by one marker column per level.
const MARKER_WIDTH: f32 = 24.0;
const MARKER_PAD: f32 = 6.0;
/// A task item's box, and how far it sits below the line's top.
const TASK_BOX: f32 = 12.0;
const TASK_BOX_TOP: f32 = 3.0;
/// The room inside a quote's left border.
const QUOTE_PAD: f32 = 12.0;
/// The room inside a code block, and its corners.
const CODE_PAD: f32 = 10.0;
const CODE_RADIUS: f32 = 4.0;
/// The room inside a table cell.
const CELL_PAD_X: f32 = 8.0;
const CELL_PAD_Y: f32 = 4.0;
/// A code line's height per pixel of the font size, the source view's.
const LINE_HEIGHT_RATIO: f32 = 1.4;
/// A link's underline and a strikethrough's line, in px.
const DECORATION: f32 = 1.0;
/// The alpha of a stretch's underline, in the link colour.
const STRETCH_UNDERLINE_ALPHA: f32 = 0.45;

/// A parsed Markdown document as the rows the view draws, and where each
/// top-level block's rows start.
#[derive(Debug)]
pub(crate) struct MdDoc {
    rows: Vec<Row>,
    /// Per top-level block, the index of its first row: the row a heading's
    /// anchor scrolls to. Every block has at least one row.
    first_row_of_block: Vec<usize>,
    /// Per heading slug, the heading's own row.
    anchor_rows: HashMap<String, usize>,
}

impl MdDoc {
    /// How many rows the document draws: one list item each.
    pub(crate) fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// The row heading anchor `slug` scrolls to, when a heading has it.
    pub(crate) fn anchor_row(&self, slug: &str) -> Option<usize> {
        self.anchor_rows.get(slug).copied()
    }

    /// The text the document draws, a line per row; a table row's cells are
    /// joined by ` | `.
    pub(crate) fn drawn_text(&self) -> String {
        self.rows
            .iter()
            .map(Row::drawn_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Each stretchtext link the document draws, in order: its drawn text
    /// and the key of the stretch it opens or closes.
    pub(crate) fn stretchtext_links(&self) -> Vec<(String, String)> {
        self.rows
            .iter()
            .flat_map(Row::span_lists)
            .flatten()
            .filter_map(|span| match &span.link {
                Some(LinkTarget::Stretchtext { key }) => Some((span.text.clone(), key.clone())),
                _ => None,
            })
            .collect()
    }
}

/// Which links a rendered document follows on a click.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LinkRoute {
    /// A viewer tab's: every link, followed as the tab's file writes it.
    ViewerTab,
    /// A chat answer's: a web link, opened in the browser, and a stretch,
    /// opened or closed in place; any other link is plain text.
    WebOnly,
}

impl LinkRoute {
    /// Whether a link to `target` is drawn as a link and followed.
    pub(crate) fn follows(self, target: &LinkTarget) -> bool {
        match self {
            Self::ViewerTab => true,
            Self::WebOnly => matches!(target, LinkTarget::Url(_) | LinkTarget::Stretchtext { .. }),
        }
    }
}

/// Each span `route` follows a link from: its byte range in
/// [`inline_text`]'s string, with its target. An empty span has nothing to
/// click.
pub(crate) fn link_ranges(spans: &[Span], route: LinkRoute) -> Vec<(Range<usize>, LinkTarget)> {
    let (_, ranges) = inline_text(spans);
    ranges
        .into_iter()
        .zip(spans)
        .filter(|(range, _)| !range.is_empty())
        .filter_map(|(range, span)| span.link.clone().map(|target| (range, target)))
        .filter(|(_, target)| route.follows(target))
        .collect()
}

/// The folder of the file at `path`; a viewer tab's path is absolute, so it
/// always has one.
fn file_dir(path: &str) -> PathBuf {
    Path::new(path)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
}

/// One drawn row: how many quotes and list levels it sits in, the room
/// after it, and what it shows.
#[derive(Debug)]
struct Row {
    quote: usize,
    indent: usize,
    gap: f32,
    kind: RowKind,
}

impl Row {
    /// The runs of spans the row draws: a table row's per cell.
    fn span_lists(&self) -> Vec<&[Span]> {
        match &self.kind {
            RowKind::Heading { spans, .. } | RowKind::Text(spans) | RowKind::Item { spans, .. } => {
                vec![spans.as_slice()]
            }
            RowKind::TableRow { cells, .. } => cells.iter().map(Vec::as_slice).collect(),
            RowKind::CodeLine { .. } | RowKind::Rule | RowKind::Html(_) => Vec::new(),
        }
    }

    fn drawn_text(&self) -> String {
        match &self.kind {
            RowKind::CodeLine { text, .. } | RowKind::Html(text) => text.clone(),
            _ => self
                .span_lists()
                .into_iter()
                .map(|spans| inline_text(spans).0)
                .collect::<Vec<_>>()
                .join(" | "),
        }
    }
}

#[derive(Debug)]
enum RowKind {
    Heading {
        level: u8,
        spans: Vec<Span>,
    },
    /// A paragraph, or the empty row of a block that has nothing to show.
    Text(Vec<Span>),
    /// A list item's marker and its first paragraph; the item's other blocks
    /// are rows of their own, one level in.
    Item {
        marker: Marker,
        spans: Vec<Span>,
    },
    CodeLine {
        text: String,
        spans: Option<LineSpans>,
        first: bool,
        last: bool,
    },
    TableRow {
        aligns: Vec<Align>,
        cells: Vec<Vec<Span>>,
        head: bool,
        last: bool,
    },
    Rule,
    Html(String),
}

/// A list item's marker: a bullet or number, or a task's box.
#[derive(Debug)]
enum Marker {
    Text(String),
    Task(bool),
}

/// Where a block's rows sit: inside how many quotes, how many list levels in.
#[derive(Debug, Clone, Copy)]
struct Place {
    quote: usize,
    indent: usize,
}

/// Parses `text`, read from a file in `file_dir`, into rows, colouring its
/// code blocks whose language has a grammar within the highlighter's time
/// limit for the whole document; `cancel` stops the colouring.
pub(crate) fn prepare(text: &str, file_dir: &Path, cancel: &AtomicBool) -> MdDoc {
    flatten(md_blocks::parse(text, file_dir), cancel)
}

/// A chat answer as [`crate::stretchtext::compose`] wrote it, parsed into rows
/// with its stretches lifted into stretchtext links, its code coloured as
/// [`prepare`] colours it.
pub(crate) fn prepare_answer(composed: &Composed, cancel: &AtomicBool) -> MdDoc {
    let mut document = md_blocks::parse(&composed.source, Path::new(""));
    md_blocks::lift_stretchtext(&mut document, &composed.keys);
    flatten(document, cancel)
}

/// `document`'s blocks as rows.
fn flatten(document: md_blocks::Document, cancel: &AtomicBool) -> MdDoc {
    let md_blocks::Document { blocks } = document;
    let mut flat = Flattener {
        rows: Vec::new(),
        anchor_rows: HashMap::new(),
        deadline: Instant::now() + syntax::TIME_LIMIT,
        cancel,
    };
    let mut first_row_of_block = Vec::with_capacity(blocks.len());
    let top = Place {
        quote: 0,
        indent: 0,
    };
    for block in blocks {
        first_row_of_block.push(flat.rows.len());
        flat.block(block, top, BLOCK_GAP);
    }
    MdDoc {
        rows: flat.rows,
        first_row_of_block,
        anchor_rows: flat.anchor_rows,
    }
}

/// Turns blocks into rows, in the order they are drawn.
struct Flattener<'a> {
    rows: Vec<Row>,
    /// Per heading slug, the heading's row; the first heading wins a slug.
    anchor_rows: HashMap<String, usize>,
    deadline: Instant,
    cancel: &'a AtomicBool,
}

impl Flattener<'_> {
    fn push(&mut self, at: Place, kind: RowKind, gap: f32) {
        self.rows.push(Row {
            quote: at.quote,
            indent: at.indent,
            gap,
            kind,
        });
    }

    /// `block`'s rows, at least one, the last followed by `gap`.
    fn block(&mut self, block: Block, at: Place, gap: f32) {
        let first = self.rows.len();
        match block {
            Block::Heading { level, slug, spans } => {
                self.anchor_rows.entry(slug).or_insert(self.rows.len());
                self.push(at, RowKind::Heading { level, spans }, gap);
            }
            Block::Paragraph(spans) => self.push(at, RowKind::Text(spans), gap),
            Block::Rule => self.push(at, RowKind::Rule, gap),
            Block::Html(text) => self.push(at, RowKind::Html(text), gap),
            Block::Code { lang, text } => self.code(lang.as_deref(), &text, at),
            Block::Quote(blocks) => {
                let inner = Place {
                    quote: at.quote + 1,
                    ..at
                };
                for inner_block in blocks {
                    self.block(inner_block, inner, BLOCK_GAP);
                }
            }
            Block::List { start, items } => {
                for (index, item) in items.into_iter().enumerate() {
                    let marker = match item.task {
                        Some(checked) => Marker::Task(checked),
                        None => Marker::Text(list_marker(start, index)),
                    };
                    self.item(marker, item.blocks, at);
                }
            }
            Block::Table { aligns, head, rows } => self.table(&aligns, head, rows, at),
        }
        match self.rows.get_mut(first..).and_then(<[Row]>::last_mut) {
            Some(last) => last.gap = gap,
            None => self.push(at, RowKind::Text(Vec::new()), gap),
        }
    }

    /// A list item: its marker beside its first paragraph, then its other
    /// blocks one level in.
    fn item(&mut self, marker: Marker, blocks: Vec<Block>, at: Place) {
        let mut rest = blocks.into_iter().peekable();
        let spans = match rest.next_if(|block| matches!(block, Block::Paragraph(_))) {
            Some(Block::Paragraph(spans)) => spans,
            _ => Vec::new(),
        };
        self.push(at, RowKind::Item { marker, spans }, ITEM_GAP);
        let inner = Place {
            indent: at.indent + 1,
            ..at
        };
        for block in rest {
            self.block(block, inner, ITEM_GAP);
        }
    }

    /// A code block, a row per line.
    fn code(&mut self, lang: Option<&str>, text: &str, at: Place) {
        let mut colours = colour_code(lang, text, self.deadline, self.cancel).map(Vec::into_iter);
        let count = text.split('\n').count();
        for (index, line) in text.split('\n').enumerate() {
            let spans = colours.as_mut().and_then(Iterator::next);
            let kind = RowKind::CodeLine {
                text: line.to_owned(),
                spans,
                first: index == 0,
                last: index + 1 == count,
            };
            self.push(at, kind, 0.0);
        }
    }

    /// A table, a row for its header and one per body row.
    fn table(
        &mut self,
        aligns: &[Align],
        head: Vec<Vec<Span>>,
        rows: Vec<Vec<Vec<Span>>>,
        at: Place,
    ) {
        let body = rows.len();
        let head_row = RowKind::TableRow {
            aligns: aligns.to_vec(),
            cells: head,
            head: true,
            last: body == 0,
        };
        self.push(at, head_row, 0.0);
        for (index, cells) in rows.into_iter().enumerate() {
            let row = RowKind::TableRow {
                aligns: aligns.to_vec(),
                cells,
                head: false,
                last: index + 1 == body,
            };
            self.push(at, row, 0.0);
        }
    }
}

/// A code block's colours, when its language has a grammar and the
/// highlighter finishes within its limits.
fn colour_code(
    lang: Option<&str>,
    code: &str,
    deadline: Instant,
    cancel: &AtomicBool,
) -> Option<Highlighted> {
    let language = lang.filter(|id| syntax::has_grammar(id))?;
    let grammar = syntax::syntax_for(language)?;
    match syntax::highlight(code, grammar, syntax::syntax_set(), deadline, cancel) {
        Ok(spans) => Some(spans),
        Err(reason) => {
            tracing::debug!(language, ?reason, "a Markdown code block left uncoloured");
            None
        }
    }
}

/// The spans' texts joined, and each span's byte range in the result.
pub(crate) fn inline_text(spans: &[Span]) -> (String, Vec<Range<usize>>) {
    let mut text = String::with_capacity(spans.iter().map(|span| span.text.len()).sum());
    let ranges = spans
        .iter()
        .map(|span| {
            let start = text.len();
            text.push_str(&span.text);
            start..text.len()
        })
        .collect();
    (text, ranges)
}

/// The marker of item `index` of a list: a bullet, or the number counted
/// from the ordered list's `start`.
pub(crate) fn list_marker(start: Option<u64>, index: usize) -> String {
    match start {
        None => "•".to_owned(),
        Some(start) => {
            let offset = u64::try_from(index).unwrap_or(u64::MAX);
            format!("{}.", start.saturating_add(offset))
        }
    }
}

/// A heading's text size by its level.
fn heading_size(level: u8) -> f32 {
    match level {
        1 => 24.0,
        2 => 20.0,
        3 => 17.0,
        4 => 15.0,
        _ => UI_TEXT_SIZE,
    }
}

fn hsla(color: u32) -> Hsla {
    Hsla::from(gpui::rgb(color))
}

/// `count` steps of `step` px.
fn steps(count: usize, step: f32) -> f32 {
    f32::from(u16::try_from(count).unwrap_or(u16::MAX)) * step
}

/// A span's look: bold, italic, struck through, a code chip, muted, or the
/// colour and underline of a link `route` follows.
///
/// A stretch's short form takes the link colour over a faded underline, so
/// it reads apart from a web link; its closer draws muted, with none.
fn span_style(span: &Span, route: LinkRoute, t: &UiTheme) -> HighlightStyle {
    let followed = span.link.as_ref().filter(|target| route.follows(target));
    let stretch = matches!(followed, Some(LinkTarget::Stretchtext { .. }));
    let linked = followed.is_some() && !(stretch && span.muted);
    let underline_color = stretch.then(|| Hsla {
        a: STRETCH_UNDERLINE_ALPHA,
        ..hsla(t.periwinkle)
    });
    let color = if linked {
        Some(hsla(t.periwinkle))
    } else if span.muted {
        Some(hsla(t.subtle))
    } else {
        None
    };
    HighlightStyle {
        color,
        font_weight: span.strong.then_some(FontWeight::BOLD),
        font_style: span.emphasis.then_some(FontStyle::Italic),
        background_color: span.code.then(|| hsla(t.chip)),
        underline: linked.then_some(UnderlineStyle {
            thickness: px(DECORATION),
            color: underline_color,
            wavy: false,
        }),
        strikethrough: span.strikethrough.then_some(StrikethroughStyle {
            thickness: px(DECORATION),
            color: None,
        }),
        ..HighlightStyle::default()
    }
}

/// A code line's colours, dropping any span that does not fit `line` on
/// its character boundaries.
fn line_highlights(
    line: &str,
    spans: &LineSpans,
    colors: &[Hsla; 6],
) -> Vec<(Range<usize>, HighlightStyle)> {
    spans
        .iter()
        .filter(|(span, _)| {
            span.end <= line.len()
                && line.is_char_boundary(span.start)
                && line.is_char_boundary(span.end)
        })
        .map(|(span, class)| {
            let style = HighlightStyle {
                color: Some(colors[class.index()]),
                ..HighlightStyle::default()
            };
            (span.clone(), style)
        })
        .collect()
}

/// A task item's box, filled when it is checked.
fn task_box(checked: bool, t: &UiTheme) -> Div {
    let color = if checked { t.accent } else { t.line_strong };
    div()
        .mt(px(TASK_BOX_TOP))
        .size(px(TASK_BOX))
        .rounded(px(2.0))
        .border_1()
        .border_color(gpui::rgb(color))
        .when(checked, |boxed| boxed.bg(gpui::rgb(t.accent)))
}

/// What every row of one rendered document is drawn with; `tab_id` names
/// the document in its rows' selectors and the links it follows, which
/// `route` picks.
pub(crate) struct Painter {
    tab_id: String,
    route: LinkRoute,
    mono: SharedString,
    mono_size: f32,
    colors: [Hsla; 6],
    theme: UiTheme,
    /// The session and answer a chat answer's stretchtext links toggle.
    stretchtext: Option<(String, String)>,
}

impl Painter {
    /// A painter for document `tab_id`, following the links `route` picks,
    /// whose code draws in the terminal font `family` (the bundled one when
    /// `None`) at `size`, coloured as the source view colours a file in `t`.
    pub(crate) fn new(
        tab_id: String,
        route: LinkRoute,
        family: Option<&str>,
        size: f32,
        t: &UiTheme,
    ) -> Self {
        Self {
            tab_id,
            route,
            mono: SharedString::from(family.unwrap_or(fonts::DEFAULT_FAMILY).to_owned()),
            mono_size: size,
            colors: TokenClass::ALL.map(|class| hsla(class.color(t))),
            theme: *t,
            stretchtext: None,
        }
    }

    /// This painter drawing answer `answer_id` of session `session_id`'s
    /// chat, whose stretchtext links open and close its stretches.
    pub(crate) fn for_answer(mut self, session_id: &str, answer_id: &str) -> Self {
        self.stretchtext = Some((session_id.to_owned(), answer_id.to_owned()));
        self
    }

    /// Row `ix`'s content, before its indent and quote borders.
    fn row(&self, row: &Row, ix: usize) -> Div {
        let t = &self.theme;
        match &row.kind {
            RowKind::Heading { level, spans } => div()
                .text_size(px(heading_size(*level)))
                .font_weight(FontWeight::BOLD)
                .child(self.inline(spans, ix, 0)),
            RowKind::Text(spans) => div().child(self.inline(spans, ix, 0)),
            RowKind::Item { marker, spans } => self.item(marker, spans, ix),
            RowKind::CodeLine {
                text,
                spans,
                first,
                last,
            } => self.code_line(text, spans.as_ref(), *first, *last),
            RowKind::TableRow {
                aligns,
                cells,
                head,
                last,
            } => self.table_row(aligns, cells, *head, *last, ix),
            RowKind::Rule => div().w_full().h(px(1.0)).bg(gpui::rgb(t.line)),
            RowKind::Html(text) => div()
                .font_family(self.mono.clone())
                .text_size(px(self.mono_size))
                .text_color(gpui::rgb(t.subtle))
                .child(text.clone()),
        }
    }

    /// Inline spans as one text with a highlight per span; `part` tells the
    /// cells of one table row apart.
    fn inline(&self, spans: &[Span], ix: usize, part: usize) -> AnyElement {
        let t = &self.theme;
        let (text, ranges) = inline_text(spans);
        let links = link_ranges(spans, self.route);
        let highlights: Vec<(Range<usize>, HighlightStyle)> = ranges
            .into_iter()
            .zip(spans)
            .filter(|(range, _)| !range.is_empty())
            .map(|(range, span)| (range, span_style(span, self.route, t)))
            .collect();
        let id = ElementId::Name(SharedString::from(format!(
            "md-text-{}-{ix}-{part}",
            self.tab_id
        )));
        let text = InteractiveText::new(id, StyledText::new(text).with_highlights(highlights));
        if links.is_empty() {
            return text.into_any_element();
        }
        let (bounds, targets): (Vec<Range<usize>>, Vec<LinkTarget>) = links.into_iter().unzip();
        let (tab_id, route) = (self.tab_id.clone(), self.route);
        let stretchtext = self.stretchtext.clone();
        text.on_click(bounds, move |index, window, cx| {
            let Some(target) = targets.get(index).cloned() else {
                tracing::debug!(index, "a Markdown link click outside every link");
                return;
            };
            let Some(Some(root)) = window.root::<RootView>() else {
                tracing::debug!("a Markdown link click in a window without the root view");
                return;
            };
            root.update(cx, |root, cx| match (route, target) {
                (_, LinkTarget::Stretchtext { key }) => {
                    if let Some((session_id, answer_id)) = &stretchtext {
                        root.toggle_stretchtext(session_id, answer_id, &key, cx);
                    } else {
                        tracing::debug!("a stretchtext link outside a chat answer");
                    }
                }
                (LinkRoute::ViewerTab, target) => root.follow_md_link(&tab_id, target, window, cx),
                (LinkRoute::WebOnly, LinkTarget::Url(url)) => {
                    root.dispatch_open(OpenJob::Url(url), Then::Nothing, window, cx);
                }
                (LinkRoute::WebOnly, _) => {
                    tracing::debug!("a chat answer follows only web links");
                }
            });
        })
        .into_any_element()
    }

    fn item(&self, marker: &Marker, spans: &[Span], ix: usize) -> Div {
        let t = &self.theme;
        let marker = match marker {
            Marker::Task(checked) => task_box(*checked, t),
            Marker::Text(text) => div().child(text.clone()),
        };
        div()
            .flex()
            .flex_row()
            .child(
                div()
                    .flex_none()
                    .w(px(MARKER_WIDTH))
                    .pr(px(MARKER_PAD))
                    .flex()
                    .justify_end()
                    .text_color(gpui::rgb(t.subtle))
                    .child(marker),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .child(self.inline(spans, ix, 0)),
            )
    }

    /// A code line in the terminal font, coloured as the source view
    /// colours a file; the block's first and last lines carry its padding
    /// and corners.
    fn code_line(&self, text: &str, spans: Option<&LineSpans>, first: bool, last: bool) -> Div {
        let t = &self.theme;
        let line_height = (self.mono_size * LINE_HEIGHT_RATIO).round();
        let highlights = spans
            .map(|spans| line_highlights(text, spans, &self.colors))
            .unwrap_or_default();
        div()
            .px(px(CODE_PAD))
            .bg(gpui::rgb(t.sunken))
            .overflow_hidden()
            .font_family(self.mono.clone())
            .text_size(px(self.mono_size))
            .when(first, |top| top.pt(px(CODE_PAD)).rounded_t(px(CODE_RADIUS)))
            .when(last, |bottom| {
                bottom.pb(px(CODE_PAD)).rounded_b(px(CODE_RADIUS))
            })
            .child(
                div()
                    .h(px(line_height))
                    .whitespace_nowrap()
                    .child(StyledText::new(text.to_owned()).with_highlights(highlights)),
            )
    }

    /// A table row: equal columns, each cell aligned as its column says; the
    /// header bold on the surface colour.
    fn table_row(
        &self,
        aligns: &[Align],
        cells: &[Vec<Span>],
        head: bool,
        last: bool,
        ix: usize,
    ) -> Div {
        let t = &self.theme;
        let cells: Vec<Div> = cells
            .iter()
            .enumerate()
            .map(|(index, cell)| {
                let cell_box = div()
                    .flex_1()
                    .min_w(px(0.0))
                    .px(px(CELL_PAD_X))
                    .py(px(CELL_PAD_Y))
                    .child(self.inline(cell, ix, index));
                match aligns.get(index).copied().unwrap_or(Align::None) {
                    Align::Center => cell_box.text_center(),
                    Align::Right => cell_box.text_right(),
                    Align::None | Align::Left => cell_box,
                }
            })
            .collect();
        div()
            .flex()
            .flex_row()
            .border_l_1()
            .border_r_1()
            .border_t_1()
            .border_color(gpui::rgb(t.line))
            .when(head, |row| {
                row.font_weight(FontWeight::BOLD)
                    .bg(gpui::rgb(t.surface))
                    .rounded_t(px(CODE_RADIUS))
            })
            .when(last, |row| row.border_b_1().rounded_b(px(CODE_RADIUS)))
            .children(cells)
    }
}

/// Row `ix` of `doc` as its list item: indented, inside its quotes' borders,
/// tagged with its top-level block when it is the block's first row, with
/// `pad_x` left and right of it and `pad_top` above the first row.
pub(crate) fn render_row(
    doc: &MdDoc,
    painter: &Painter,
    ix: usize,
    pad_x: f32,
    pad_top: f32,
) -> AnyElement {
    let t = &painter.theme;
    let Some(row) = doc.rows.get(ix) else {
        return div().into_any_element();
    };
    let mut framed = div()
        .pl(px(steps(row.indent, MARKER_WIDTH)))
        .pb(px(row.gap))
        .child(painter.row(row, ix));
    for _ in 0..row.quote {
        framed = div()
            .pl(px(QUOTE_PAD))
            .border_l_2()
            .border_color(gpui::rgb(t.line_strong))
            .text_color(gpui::rgb(t.subtle))
            .child(framed);
    }
    let selector = match doc.first_row_of_block.binary_search(&ix) {
        Ok(block) => format!("viewer-md-block-{}-{block}", painter.tab_id),
        Err(_) => format!("viewer-md-row-{}-{ix}", painter.tab_id),
    };
    div()
        .debug_selector(move || selector)
        .px(px(pad_x))
        .when(ix == 0, |first| first.pt(px(pad_top)))
        .child(framed)
        .into_any_element()
}

impl RootView {
    /// Parses tab `tab_id`'s Markdown text, read `read`, on the background
    /// executor, when the tab can render it.
    pub(crate) fn parse_viewer_markdown(
        &mut self,
        tab_id: &str,
        read: u64,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.viewer_tabs.get(tab_id) else {
            return;
        };
        if !tab.renders_markdown() {
            return;
        }
        let Some(text) = tab.text() else {
            return;
        };
        let text = Arc::clone(text);
        let file_dir = file_dir(tab.path());
        let cancel = tab.highlight_cancel();
        let work = cx
            .background_executor()
            .spawn(async move { Arc::new(prepare(text.text(), &file_dir, &cancel)) });
        let tab_id = tab_id.to_owned();
        cx.spawn(async move |this, cx| {
            let doc = work.await;
            // Fails only when the view is gone, and the tab with it.
            this.update(cx, |root, cx| {
                let kept = root
                    .viewer_tabs
                    .get_mut(&tab_id)
                    .is_some_and(|tab| tab.finish_markdown(read, doc));
                if kept {
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Shows tab `tab_id` as `mode` and saves it as the mode new Markdown
    /// tabs open in; other open tabs keep theirs.
    fn set_viewer_markdown(
        &mut self,
        tab_id: &str,
        mode: MarkdownMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let took = self
            .viewer_tabs
            .get_mut(tab_id)
            .is_some_and(|tab| tab.set_markdown(mode));
        if !took {
            return;
        }
        self.return_stranded_find_focus(tab_id, window, cx);
        self.sidebar
            .set_viewer_markdown_rendered(mode == MarkdownMode::Rendered);
        self.save_ui();
        cx.notify();
    }

    /// Follows `href`, a link target as Markdown tab `tab_id`'s file writes
    /// it, as a click on the link does.
    pub fn follow_md_href(
        &mut self,
        tab_id: &str,
        href: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.viewer_tabs.get(tab_id) else {
            return;
        };
        let target = md_blocks::classify_link(href, &file_dir(tab.path()));
        self.follow_md_link(tab_id, target, window, cx);
    }

    /// A link to `target` in Markdown tab `tab_id` was clicked: a URL opens
    /// in the browser; a file, resolved on this PC against the tab file's
    /// folder and without its `#fragment`, opens by the route the tab's file
    /// came by (from a fetched copy, only a file in the folder for fetched
    /// files); a heading anchor scrolls to its heading; a refused target
    /// says so.
    /// A tab whose file is not on this PC follows no link.
    pub(crate) fn follow_md_link(
        &mut self,
        tab_id: &str,
        target: LinkTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.viewer_tabs.get(tab_id) else {
            return;
        };
        let origin = match tab.origin() {
            ViewerOrigin::Link => Origin::Link,
            ViewerOrigin::Fetched => Origin::Fetched,
            ViewerOrigin::NotAvailable => return,
        };
        let folder = file_dir(tab.path());
        match target {
            LinkTarget::Url(url) => {
                self.dispatch_open(OpenJob::Url(url), Then::Nothing, window, cx);
            }
            LinkTarget::File { path, .. } => {
                let candidate = TerminalLinkCandidate {
                    path: path.to_string_lossy().into_owned(),
                    line: None,
                    column: None,
                };
                let base_dirs = vec![folder.to_string_lossy().into_owned()];
                self.resolve_path(vec![candidate], base_dirs, origin, window, cx);
            }
            LinkTarget::Anchor(slug) => self.scroll_to_md_anchor(tab_id, &slug, cx),
            LinkTarget::Stretchtext { .. } => {
                tracing::debug!(tab_id, "a viewer tab has no stretchtext to open");
            }
            LinkTarget::Refused => {
                tracing::warn!(tab_id, "refusing a Markdown link that is not http or https");
                self.push_toast(
                    ToastKind::Error,
                    COULD_NOT_OPEN,
                    Some(REFUSED_LINK.to_owned()),
                    cx,
                );
            }
        }
    }

    /// Scrolls Markdown tab `tab_id` so heading anchor `slug`'s row is at the
    /// top; a slug no heading has does nothing.
    fn scroll_to_md_anchor(&self, tab_id: &str, slug: &str, cx: &mut Context<Self>) {
        let Some((doc, rows)) = self
            .viewer_tabs
            .get(tab_id)
            .and_then(ViewerTab::markdown_doc)
        else {
            tracing::debug!(tab_id, slug, "an anchor link before the document is parsed");
            return;
        };
        let Some(row) = doc.anchor_row(slug) else {
            tracing::debug!(tab_id, slug, "no heading has this anchor");
            return;
        };
        rows.scroll_to(ListOffset {
            item_ix: row,
            offset_in_item: px(0.0),
        });
        cx.notify();
    }

    /// The row at the top of tab `tab_id`'s rendered Markdown, once parsed.
    #[must_use]
    pub fn viewer_markdown_top_row(&self, tab_id: &str) -> Option<usize> {
        let (_, rows) = self.viewer_tabs.get(tab_id)?.markdown_doc()?;
        Some(rows.logical_scroll_top().item_ix)
    }

    /// The rendered document `doc` of tab `tab_id`, a list item per row.
    pub(crate) fn viewer_markdown(
        &self,
        tab_id: &str,
        doc: &Arc<MdDoc>,
        rows: &ListState,
        t: &UiTheme,
    ) -> Div {
        let font = &self.sidebar.ui_state().terminal_font;
        let painter = Painter::new(
            tab_id.to_owned(),
            LinkRoute::ViewerTab,
            font.family.as_deref(),
            font.size,
            t,
        );
        let doc = Arc::clone(doc);
        let selector = format!("viewer-md-{tab_id}");
        div()
            .debug_selector(move || selector)
            .flex_1()
            .min_h(px(0.0))
            .child(
                list(rows.clone(), move |ix, _, _| {
                    render_row(&doc, &painter, ix, BODY_PAD_X, BODY_PAD_TOP)
                })
                .size_full(),
            )
    }
}

/// The header's Rendered | Source toggle of a Markdown tab whose text has
/// loaded, the shown mode filled; disabled, with a tooltip saying why, for a
/// document over the render cap. `None` for any other file, and until the
/// text loads.
pub(crate) fn markdown_toggle(
    tab_id: &str,
    tab: &ViewerTab,
    cx: &mut Context<RootView>,
) -> Option<Div> {
    let shown = tab.markdown()?;
    if tab.load() != ViewerLoad::Loaded {
        return None;
    }
    let enabled = !tab.markdown_too_large();
    let buttons = [
        (MarkdownMode::Rendered, "rendered", "Rendered"),
        (MarkdownMode::Source, "source", "Source"),
    ]
    .map(|(mode, name, label)| {
        let selector = format!("viewer-md-{name}-{tab_id}");
        let button = if mode == shown {
            primary_button(
                &selector,
                ButtonSize::Compact,
                enabled,
                Layer::Base,
                &theme(cx),
            )
        } else {
            outlined_button(
                &selector,
                ButtonSize::Compact,
                enabled,
                Layer::Base,
                &theme(cx),
            )
        }
        .flex_none()
        .child(label);
        if !enabled {
            return button.tooltip(tooltip(TOO_LARGE_TO_RENDER));
        }
        let tab_id = tab_id.to_owned();
        button.on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
            this.set_viewer_markdown(&tab_id, mode, window, cx);
        }))
    });
    Some(div().flex().flex_none().gap(px(4.0)).children(buttons))
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use std::path::Path;

    fn spans_of(text: &str) -> Vec<Span> {
        match md_blocks::parse(text, Path::new("notes")).blocks.first() {
            Some(Block::Paragraph(spans)) => spans.clone(),
            other => panic!("one paragraph, got {other:?}"),
        }
    }

    #[test]
    fn inline_spans_become_highlight_ranges_on_char_boundaries() {
        let spans = spans_of("café **naïve** `zoë` ~~日本~~ 🌷 *end*\n");
        let (text, ranges) = inline_text(&spans);
        assert_eq!(text, "café naïve zoë 日本 🌷 end");
        assert_eq!(ranges.len(), spans.len());
        let mut at = 0;
        for (span, range) in spans.iter().zip(&ranges) {
            assert_eq!(range.start, at, "the ranges tile the text");
            assert!(text.is_char_boundary(range.start) && text.is_char_boundary(range.end));
            assert_eq!(&text[range.clone()], span.text, "{span:?}");
            at = range.end;
        }
        assert_eq!(at, text.len());
        let strong = spans
            .iter()
            .position(|span| span.strong)
            .expect("a strong span");
        assert_eq!(&text[ranges[strong].clone()], "naïve");
    }

    #[test]
    fn a_chat_answer_follows_only_web_links() {
        let spans = spans_of("[web](https://example.com/page) [doc](other.md) [top](#top)\n");
        let (text, _) = inline_text(&spans);
        let links = link_ranges(&spans, LinkRoute::WebOnly);
        let [(web, web_target)] = links.as_slice() else {
            panic!("only the web link is clickable, got {links:?}");
        };
        assert_eq!(&text[web.clone()], "web");
        assert_eq!(
            *web_target,
            LinkTarget::Url("https://example.com/page".to_owned())
        );
        for label in ["doc", "top"] {
            let span = spans
                .iter()
                .find(|span| span.text == label)
                .expect("the link's span");
            let style = span_style(span, LinkRoute::WebOnly);
            assert!(
                style.underline.is_none() && style.color.is_none(),
                "{label} is plain text: {style:?}"
            );
        }
        assert_eq!(link_ranges(&spans, LinkRoute::ViewerTab).len(), 3);
    }

    #[test]
    fn link_spans_become_click_ranges() {
        let spans = spans_of("plain [web](https://example.com/page) and [doc](other.md)\n");
        let (text, _) = inline_text(&spans);
        let links = link_ranges(&spans, LinkRoute::ViewerTab);
        let [(web, web_target), (doc, doc_target)] = links.as_slice() else {
            panic!("two link ranges, got {links:?}");
        };
        assert_eq!(&text[web.clone()], "web");
        assert_eq!(
            *web_target,
            LinkTarget::Url("https://example.com/page".to_owned())
        );
        assert_eq!(&text[doc.clone()], "doc");
        assert_eq!(
            *doc_target,
            LinkTarget::File {
                path: Path::new("notes").join("other.md"),
                anchor: None,
            }
        );
    }

    #[test]
    fn anchor_slugs_map_to_their_heading_row() {
        let text = "# First\n\n```\na\nb\n```\n\n- x\n- y\n\n## Second\n\npara\n";
        let doc = prepare(text, Path::new("notes"), &AtomicBool::new(false));
        // heading 1 row; code 2; list 2; the second heading is block 3, row 5.
        assert_eq!(doc.anchor_row("first"), Some(0));
        assert_eq!(doc.anchor_row("second"), Some(5));
        assert_eq!(doc.anchor_row("missing"), None);
    }

    #[test]
    fn a_nested_heading_anchor_maps_to_its_own_row() {
        let text = "# Top\n\n- a\n- b\n\n  ## Inner\n\n> q\n>\n> ## Quoted\n";
        let doc = prepare(text, Path::new("notes"), &AtomicBool::new(false));
        // Top row 0; items a, b at 1, 2, the heading in b at 3; the quote's
        // paragraph at 4, its heading at 5.
        assert_eq!(doc.first_row_of_block, vec![0, 1, 4]);
        assert_eq!(doc.anchor_row("inner"), Some(3));
        assert_eq!(doc.anchor_row("quoted"), Some(5));
    }

    #[test]
    fn a_numbered_list_marker_counts_from_its_start() {
        assert_eq!(list_marker(Some(3), 0), "3.");
        assert_eq!(list_marker(Some(3), 2), "5.");
        assert_eq!(list_marker(Some(0), 1), "1.");
        assert_eq!(list_marker(None, 4), "•");
        assert_eq!(list_marker(Some(u64::MAX), 1), format!("{}.", u64::MAX));
    }

    #[test]
    fn each_block_maps_to_its_first_row() {
        let text = "# A\n\n```\na\nb\nc\n```\n\n- x\n  - y\n- z\n\n\
                    | h |\n|---|\n| 1 |\n| 2 |\n\n>\n\npara\n";
        let doc = prepare(text, Path::new("notes"), &AtomicBool::new(false));
        // heading 1 row; code 3; list x, y, z; table head + 2; empty quote 1;
        // paragraph 1.
        assert_eq!(doc.first_row_of_block, vec![0, 1, 4, 7, 10, 11]);
        assert_eq!(doc.row_count(), 12);
        let indents: Vec<usize> = doc.rows[4..7].iter().map(|row| row.indent).collect();
        assert_eq!(indents, vec![0, 1, 0], "a nested item is one level in");
        assert!(
            matches!(&doc.rows[5].kind, RowKind::Item { marker: Marker::Text(m), .. } if m == "•")
        );
        assert!(matches!(
            doc.rows[3].kind,
            RowKind::CodeLine {
                last: true,
                first: false,
                ..
            }
        ));
    }
}
