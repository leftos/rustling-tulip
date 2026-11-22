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
    AnyElement, App, ClickEvent, Context, Div, ElementId, Entity, FontStyle, FontWeight,
    HighlightStyle, Hsla, InteractiveText, ListOffset, ListState, Modifiers, SharedString,
    StrikethroughStyle, StyledText, TextAlign, UnderlineStyle, Window, div, list, prelude::*, px,
};

use crate::buttons::{ButtonSize, Layer, outlined_button, primary_button};
use crate::chat_select::UnitSource;
use crate::chat_view::SelectDraw;
use crate::driver_id::DriverId as _;
use crate::fonts::{self, ui_px_at};
use crate::link_menu_view::ChatFileLink;
use crate::links::{self, IssueContext, TerminalLinkCandidate};
use crate::md_blocks::{self, Align, Block, LinkTarget, Span};
use crate::notices::ToastKind;
use crate::open::{LinkAction, OpenJob};
use crate::open_view::{COULD_NOT_OPEN, Opening, Origin, PathLink, Then};
use crate::stretchtext::Composed;
use crate::syntax::{self, Highlighted, LineSpans, TokenClass};
use crate::ui_theme::{UiTheme, theme};
use crate::viewer_tab::{self, MarkdownMode, ViewerLoad, ViewerOrigin, ViewerTab};
use crate::viewer_tab_view::PHONE_VIEWER_ID;
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

/// One selectable text of a document: its row, its part within that row (a
/// table row's cell), and the text it draws.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MdUnit {
    pub(crate) row: usize,
    pub(crate) part: usize,
    pub(crate) text: String,
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

    /// Each selectable text the document draws, in order: a heading, a
    /// paragraph and a list item's own text as part 0 (a list item's marker
    /// is not part of its spans, so it is never copied), a table row one
    /// part per cell, a code line and inline HTML as part 0, and nothing
    /// for a rule. Every empty text is left out except an empty code line,
    /// which a copy keeps so a code block's blank lines survive.
    pub(crate) fn unit_texts(&self) -> Vec<MdUnit> {
        let mut units = Vec::new();
        for (row, drawn) in self.rows.iter().enumerate() {
            match &drawn.kind {
                RowKind::Heading { spans, .. }
                | RowKind::Text(spans)
                | RowKind::Item { spans, .. } => {
                    push_unit(&mut units, row, 0, inline_text(spans).0);
                }
                RowKind::TableRow { cells, .. } => {
                    for (part, cell) in cells.iter().enumerate() {
                        push_unit(&mut units, row, part, inline_text(cell).0);
                    }
                }
                RowKind::CodeLine { text, .. } => {
                    units.push(MdUnit {
                        row,
                        part: 0,
                        text: text.clone(),
                    });
                }
                RowKind::Html(text) => {
                    push_unit(&mut units, row, 0, text.clone());
                }
                RowKind::Rule => {}
            }
        }
        units
    }

    /// Each file link the document draws as a link under
    /// [`LinkRoute::Chat`], in order: its path and its `#fragment`, as a
    /// click on it passes them.
    pub(crate) fn file_links(&self) -> Vec<(String, Option<String>)> {
        self.rows
            .iter()
            .flat_map(Row::span_lists)
            .flat_map(|spans| link_ranges(spans, LinkRoute::Chat))
            .filter_map(|(_, target)| match target {
                LinkTarget::File { path, anchor } => {
                    Some((path.to_string_lossy().into_owned(), anchor))
                }
                _ => None,
            })
            .collect()
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
    /// A desktop chat answer's: what [`Self::WebOnly`] follows, and a file
    /// link, opened in its default app, or its link menu on Shift.
    Chat,
    /// A phone chat answer's: what [`Self::WebOnly`] follows, a file link,
    /// its path (the fragment dropped) opened as a tapped path on the phone
    /// (the viewer for a file, the browser for a folder), and an issue link,
    /// raised as its card.
    Phone,
}

impl LinkRoute {
    /// Whether a link to `target` is drawn as a link and followed.
    pub(crate) fn follows(self, target: &LinkTarget) -> bool {
        match self {
            Self::ViewerTab => true,
            Self::WebOnly => matches!(target, LinkTarget::Url(_) | LinkTarget::Stretchtext { .. }),
            Self::Chat => matches!(
                target,
                LinkTarget::Url(_) | LinkTarget::Stretchtext { .. } | LinkTarget::File { .. }
            ),
            Self::Phone => matches!(
                target,
                LinkTarget::Url(_)
                    | LinkTarget::Stretchtext { .. }
                    | LinkTarget::File { .. }
                    | LinkTarget::Issue { .. }
            ),
        }
    }
}

/// What a click on a rendered document's link is followed by, besides its
/// target: the document's id, the links its route follows, and the chat
/// answer it is, if it is one.
#[derive(Clone)]
struct LinkClick {
    tab_id: String,
    route: LinkRoute,
    stretchtext: Option<(String, String)>,
}

impl LinkClick {
    /// Follows a click on a link to `target` as the route asks.
    fn follow(
        &self,
        root: &mut RootView,
        target: LinkTarget,
        window: &mut Window,
        cx: &mut Context<RootView>,
    ) {
        match (self.route, target) {
            (_, LinkTarget::Stretchtext { key }) => {
                if let Some((session_id, answer_id)) = &self.stretchtext {
                    root.toggle_stretchtext(session_id, answer_id, &key, cx);
                } else {
                    tracing::debug!("a stretchtext link outside a chat answer");
                }
            }
            (LinkRoute::ViewerTab, target) => {
                let prefer_viewer = prefers_viewer(window.modifiers());
                root.follow_md_link(&self.tab_id, target, prefer_viewer, window, cx);
            }
            (LinkRoute::WebOnly | LinkRoute::Chat | LinkRoute::Phone, LinkTarget::Url(url)) => {
                root.dispatch_open(
                    &crate::target::HostKey::Local,
                    OpenJob::Url(url),
                    Then::Nothing,
                    window,
                    cx,
                );
            }
            (LinkRoute::Chat, LinkTarget::File { path, anchor }) => {
                self.chat_file(root, &path, anchor.as_deref(), window, cx);
            }
            (LinkRoute::Phone, LinkTarget::File { path, .. }) => {
                self.phone_file(root, &path, window, cx);
            }
            (
                LinkRoute::Phone,
                LinkTarget::Issue {
                    identifier,
                    web_url,
                },
            ) => self.phone_issue(root, &identifier, &web_url, window, cx),
            (LinkRoute::WebOnly, _) => {
                tracing::debug!("a chat answer follows only web links");
            }
            (LinkRoute::Chat | LinkRoute::Phone, _) => {
                tracing::debug!("a chat answer follows only web, file and issue links");
            }
        }
    }

    /// Opens a chat answer's file link at `path`: its link menu when Shift
    /// is held, else in its default app.
    fn chat_file(
        &self,
        root: &mut RootView,
        path: &Path,
        anchor: Option<&str>,
        window: &mut Window,
        cx: &mut Context<RootView>,
    ) {
        let Some((session_id, _)) = &self.stretchtext else {
            tracing::debug!("a file link outside a chat answer");
            return;
        };
        let link = ChatFileLink {
            path,
            anchor,
            menu: window.modifiers().shift,
        };
        root.chat_file_link(session_id, link, window, cx);
    }

    /// Opens a phone chat answer's file link at `path` as a tapped path.
    fn phone_file(
        &self,
        root: &mut RootView,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<RootView>,
    ) {
        let Some((session_id, _)) = &self.stretchtext else {
            tracing::debug!("a file link outside a chat answer");
            return;
        };
        root.phone_chat_file_link(session_id, path, window, cx);
    }

    /// Raises a phone chat answer's issue link: `identifier`, whose web
    /// address is `web_url`.
    fn phone_issue(
        &self,
        root: &mut RootView,
        identifier: &str,
        web_url: &str,
        window: &mut Window,
        cx: &mut Context<RootView>,
    ) {
        let Some((session_id, _)) = &self.stretchtext else {
            tracing::debug!("an issue link outside a chat answer");
            return;
        };
        root.phone_chat_issue_link(session_id, identifier, web_url, window, cx);
    }
}

/// The window's `RootView`: the window root on the desktop, or the one the phone's `PhoneRoot` holds.
fn window_root_view(window: &Window, cx: &App) -> Option<Entity<RootView>> {
    window.root::<RootView>().flatten().or_else(|| {
        window
            .root::<crate::phone::PhoneRoot>()
            .flatten()
            .map(|phone| phone.read(cx).root().clone())
    })
}

/// Whether a click on a file link with `modifiers` held views the file:
/// Ctrl+click opens it in the external app, and a plain click or
/// Ctrl+Shift+click views it.
fn prefers_viewer(modifiers: Modifiers) -> bool {
    !(if cfg!(target_os = "macos") {
        modifiers.platform
    } else {
        modifiers.control
    }) || modifiers.shift
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
    flatten(
        md_blocks::parse(text, file_dir, md_blocks::SoftBreaks::Space),
        cancel,
    )
}

/// A chat answer as [`crate::stretchtext::compose`] wrote it, parsed into rows
/// with its stretches lifted into stretchtext links, the paths in its prose
/// and inline code made file links and its Linear issue identifiers made
/// issue links against `issues`, its code coloured as [`prepare`] colours it.
/// A Windows path keeps its backslashes, since an answer's `\.` means the
/// backslash and not `CommonMark`'s escape; a Markdown file, which
/// [`prepare`] reads, keeps `CommonMark`'s reading.
pub(crate) fn prepare_answer(
    composed: &Composed,
    issues: &IssueContext,
    cancel: &AtomicBool,
) -> MdDoc {
    let source = md_blocks::keep_path_backslashes(&composed.source);
    let mut document = md_blocks::parse(&source, Path::new(""), md_blocks::SoftBreaks::Line);
    md_blocks::lift_stretchtext(&mut document, &composed.keys);
    md_blocks::link_paths(&mut document);
    md_blocks::link_issues(&mut document, issues);
    flatten(document, cancel)
}

/// Markdown `source` that comes from no file, such as release notes, parsed
/// into rows, its code coloured as [`prepare`] colours it.
pub(crate) fn prepare_text(source: &str) -> MdDoc {
    flatten(
        md_blocks::parse(source, Path::new(""), md_blocks::SoftBreaks::Space),
        &AtomicBool::new(false),
    )
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

/// Pushes `text` as a unit of `row` when it has anything to select.
fn push_unit(units: &mut Vec<MdUnit>, row: usize, part: usize, text: String) {
    if !text.is_empty() {
        units.push(MdUnit { row, part, text });
    }
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
/// it reads apart from a web link; its closer draws muted, with none. A
/// chat's file link takes [`link_color`]'s own colour.
fn span_style(span: &Span, route: LinkRoute, t: &UiTheme) -> HighlightStyle {
    let followed = span.link.as_ref().filter(|target| route.follows(target));
    let stretch = matches!(followed, Some(LinkTarget::Stretchtext { .. }));
    let linked = followed.is_some() && !(stretch && span.muted);
    let underline_color = stretch.then(|| Hsla {
        a: STRETCH_UNDERLINE_ALPHA,
        ..hsla(t.periwinkle)
    });
    let color = if let Some(target) = followed.filter(|_| linked) {
        Some(hsla(link_color(target, route, t)))
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

/// The colour of a link to `target` that `route` follows: a chat's file
/// link (a path) in lilac, every other link in periwinkle.
fn link_color(target: &LinkTarget, route: LinkRoute, t: &UiTheme) -> u32 {
    match (route, target) {
        (LinkRoute::Chat | LinkRoute::Phone, LinkTarget::File { .. }) => t.lilac,
        _ => t.periwinkle,
    }
}

/// `text`, a plain line of session `session_id`'s chat such as a card's
/// question or a receipt, with each path and web address in it drawn as a
/// link `route` follows and opened as a chat answer's link opens. A click
/// on a link goes no further, so a link inside a card's option row opens
/// the link without picking the option. `id` names the text among its
/// siblings; a line without links is plain text.
pub(crate) fn linked_text(
    id: &str,
    text: &str,
    route: LinkRoute,
    session_id: &str,
    t: &UiTheme,
) -> AnyElement {
    let spans = md_blocks::linked_text_spans(text);
    let links = link_ranges(&spans, route);
    if links.is_empty() {
        return text.to_owned().into_any_element();
    }
    let (joined, ranges) = inline_text(&spans);
    let highlights: Vec<(Range<usize>, HighlightStyle)> = ranges
        .into_iter()
        .zip(&spans)
        .filter(|(range, _)| !range.is_empty())
        .map(|(range, span)| (range, span_style(span, route, t)))
        .collect();
    let styled = StyledText::new(joined).with_highlights(highlights);
    let (bounds, targets): (Vec<Range<usize>>, Vec<LinkTarget>) = links.into_iter().unzip();
    let click = LinkClick {
        tab_id: id.to_owned(),
        route,
        stretchtext: Some((session_id.to_owned(), String::new())),
    };
    InteractiveText::new(ElementId::Name(SharedString::from(id.to_owned())), styled)
        .on_click(bounds, move |index, window, cx| {
            cx.stop_propagation();
            let Some(target) = targets.get(index).cloned() else {
                return;
            };
            let Some(root) = window_root_view(window, cx) else {
                tracing::debug!("a chat line's link click in a window without the root view");
                return;
            };
            root.update(cx, |root, cx| click.follow(root, target, window, cx));
        })
        .into_any_element()
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

/// `highlights`, ordered and not overlapping, with `selected` drawn on
/// `background`: each range cut where the selection starts and ends, the
/// pieces inside it given the background, and the selected bytes no range
/// covers given a range of their own. They stay ordered and not
/// overlapping, as [`StyledText::with_highlights`] needs; no selection, or
/// an empty one, leaves them as they are.
pub(crate) fn select_highlights(
    highlights: Vec<(Range<usize>, HighlightStyle)>,
    selected: Option<Range<usize>>,
    background: Hsla,
) -> Vec<(Range<usize>, HighlightStyle)> {
    let Some(selected) = selected.filter(|range| !range.is_empty()) else {
        return highlights;
    };
    let lit = |style: HighlightStyle| HighlightStyle {
        background_color: Some(background),
        ..style
    };
    let mut out = Vec::with_capacity(highlights.len() + 3);
    let mut push = |range: Range<usize>, style: HighlightStyle| {
        if !range.is_empty() {
            out.push((range, style));
        }
    };
    // The first selected byte not yet given its background.
    let mut uncovered = selected.start;
    for (range, style) in highlights {
        push(
            uncovered..range.start.min(selected.end),
            lit(HighlightStyle::default()),
        );
        push(range.start..range.end.min(selected.start), style);
        push(
            range.start.max(selected.start)..range.end.min(selected.end),
            lit(style),
        );
        push(range.start.max(selected.end)..range.end, style);
        uncovered = uncovered.max(range.end);
    }
    push(uncovered..selected.end, lit(HighlightStyle::default()));
    out
}

/// A task item's box at UI text `offset`, filled when it is checked.
fn task_box(offset: i8, checked: bool, t: &UiTheme) -> Div {
    let color = if checked { t.accent } else { t.line_strong };
    div()
        .mt(ui_px_at(offset, TASK_BOX_TOP))
        .size(ui_px_at(offset, TASK_BOX))
        .rounded(px(2.0))
        .border_1()
        .border_color(gpui::rgb(color))
        .when(checked, |boxed| boxed.bg(gpui::rgb(t.accent)))
}

/// What every row of one rendered document is drawn with; `tab_id` names
/// the document in its rows' selectors and the links it follows, which
/// `route` picks.
pub(crate) struct Painter {
    /// The whole pixels added to headings, task boxes and list markers.
    offset: i8,
    tab_id: String,
    route: LinkRoute,
    mono: SharedString,
    mono_size: f32,
    colors: [Hsla; 6],
    theme: UiTheme,
    /// The session and answer a chat answer's stretchtext links toggle.
    stretchtext: Option<(String, String)>,
    /// The chat entry the document is and how its selectable texts are
    /// drawn, in the phone's chat only.
    select: Option<(String, SelectDraw)>,
}

impl Painter {
    /// A painter for document `tab_id` at UI text `offset`, following the
    /// links `route` picks, whose code draws in the terminal font `code`, a
    /// family (the bundled one when `None`) and a size, coloured as the
    /// source view colours a file in `t`.
    pub(crate) fn new(
        offset: i8,
        tab_id: String,
        route: LinkRoute,
        (family, size): (Option<&str>, f32),
        t: &UiTheme,
    ) -> Self {
        Self {
            offset,
            tab_id,
            route,
            mono: SharedString::from(family.unwrap_or(fonts::DEFAULT_FAMILY).to_owned()),
            mono_size: size,
            colors: TokenClass::ALL.map(|class| hsla(class.color(t))),
            theme: *t,
            stretchtext: None,
            select: None,
        }
    }

    /// This painter drawing chat entry `entry_id`'s texts through `select`
    /// (registered, and highlighted where selected); no change without it.
    pub(crate) fn selectable(mut self, entry_id: &str, select: Option<&SelectDraw>) -> Self {
        self.select = select.map(|select| (entry_id.to_owned(), select.clone()));
        self
    }

    /// Part `part` of row `ix`, `text`, aligned by `align` and styled with
    /// `highlights`, through the painter's selection drawing when it has
    /// one.
    fn styled(
        &self,
        (ix, part, align): (usize, usize, TextAlign),
        text: String,
        highlights: Vec<(Range<usize>, HighlightStyle)>,
    ) -> StyledText {
        match &self.select {
            Some((entry, select)) => {
                let source = UnitSource::AnswerRow { row: ix, part };
                select.text((entry, source), align, text.into(), highlights)
            }
            None => StyledText::new(text).with_highlights(highlights),
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
                .text_size(ui_px_at(self.offset, heading_size(*level)))
                .font_weight(FontWeight::BOLD)
                .child(self.inline(spans, ix, 0, TextAlign::Left)),
            RowKind::Text(spans) => div().child(self.inline(spans, ix, 0, TextAlign::Left)),
            RowKind::Item { marker, spans } => self.item(marker, spans, ix),
            RowKind::CodeLine {
                text,
                spans,
                first,
                last,
            } => self.code_line(text, spans.as_ref(), (*first, *last), ix),
            RowKind::TableRow {
                aligns,
                cells,
                head,
                last,
            } => self.table_row(aligns, cells, *head, *last, ix),
            RowKind::Rule => div().w_full().h(px(1.0)).bg(gpui::rgb(t.line)),
            RowKind::Html(text) => div()
                .font_family(self.mono.clone())
                .font_weight(crate::fonts::mono_weight())
                .text_size(px(self.mono_size))
                .text_color(gpui::rgb(t.subtle))
                .child(match &self.select {
                    Some(_) => self
                        .styled((ix, 0, TextAlign::Left), text.clone(), Vec::new())
                        .into_any_element(),
                    None => text.clone().into_any_element(),
                }),
        }
    }

    /// Inline spans as one text with a highlight per span; `part` tells the
    /// cells of one table row apart, and `align` is how its element aligns
    /// it.
    fn inline(&self, spans: &[Span], ix: usize, part: usize, align: TextAlign) -> AnyElement {
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
        let text = InteractiveText::new(id, self.styled((ix, part, align), text, highlights));
        if links.is_empty() {
            return text.into_any_element();
        }
        let (bounds, targets): (Vec<Range<usize>>, Vec<LinkTarget>) = links.into_iter().unzip();
        let click = LinkClick {
            tab_id: self.tab_id.clone(),
            route: self.route,
            stretchtext: self.stretchtext.clone(),
        };
        text.on_click(bounds, move |index, window, cx| {
            let Some(target) = targets.get(index).cloned() else {
                tracing::debug!(index, "a Markdown link click outside every link");
                return;
            };
            let Some(root) = window_root_view(window, cx) else {
                tracing::debug!("a Markdown link click in a window without the root view");
                return;
            };
            root.update(cx, |root, cx| click.follow(root, target, window, cx));
        })
        .into_any_element()
    }

    fn item(&self, marker: &Marker, spans: &[Span], ix: usize) -> Div {
        let t = &self.theme;
        let marker = match marker {
            Marker::Task(checked) => task_box(self.offset, *checked, t),
            Marker::Text(text) => div().child(text.clone()),
        };
        div()
            .flex()
            .flex_row()
            .child(
                div()
                    .flex_none()
                    .w(ui_px_at(self.offset, MARKER_WIDTH))
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
                    .child(self.inline(spans, ix, 0, TextAlign::Left)),
            )
    }

    /// A code line in the terminal font, coloured as the source view
    /// colours a file; the block's first and last lines carry its padding
    /// and corners; `ix` is its row.
    fn code_line(
        &self,
        text: &str,
        spans: Option<&LineSpans>,
        (first, last): (bool, bool),
        ix: usize,
    ) -> Div {
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
            .font_weight(crate::fonts::mono_weight())
            .text_size(px(self.mono_size))
            .when(first, |top| top.pt(px(CODE_PAD)).rounded_t(px(CODE_RADIUS)))
            .when(last, |bottom| {
                bottom.pb(px(CODE_PAD)).rounded_b(px(CODE_RADIUS))
            })
            .child(
                div()
                    .h(px(line_height))
                    .whitespace_nowrap()
                    .child(self.styled((ix, 0, TextAlign::Left), text.to_owned(), highlights)),
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
                let align = match aligns.get(index).copied().unwrap_or(Align::None) {
                    Align::Center => TextAlign::Center,
                    Align::Right => TextAlign::Right,
                    Align::None | Align::Left => TextAlign::Left,
                };
                let cell_box = div()
                    .flex_1()
                    .min_w(px(0.0))
                    .px(px(CELL_PAD_X))
                    .py(px(CELL_PAD_Y))
                    .child(self.inline(cell, ix, index, align));
                match align {
                    TextAlign::Center => cell_box.text_center(),
                    TextAlign::Right => cell_box.text_right(),
                    TextAlign::Left => cell_box,
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
        .pl(ui_px_at(painter.offset, MARKER_WIDTH) * steps(row.indent, 1.0))
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
        .driver_id(move || selector)
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
        self.follow_md_link(tab_id, target, true, window, cx);
    }

    /// A link to `target` in Markdown tab `tab_id` was clicked: a URL opens
    /// in the browser; a file, resolved on this PC against the tab file's
    /// folder, opens by the route the tab's file came by (from a fetched
    /// copy, only a file in the folder for fetched files), in the viewer
    /// when `prefer_viewer` and it can show the file, there at the line an
    /// `#L<n>` fragment names or, for a Markdown file, at the heading
    /// another names (any other fragment is dropped); a heading anchor
    /// scrolls to its heading; a refused target says so.
    /// A tab whose file is not on this PC follows no link.
    pub(crate) fn follow_md_link(
        &mut self,
        tab_id: &str,
        target: LinkTarget,
        prefer_viewer: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The phone's viewer follows only a jump within its page.
        if tab_id == PHONE_VIEWER_ID && !matches!(target, LinkTarget::Anchor(_)) {
            tracing::debug!(tab_id, "the phone's viewer follows no link out of its page");
            return;
        }
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
                let job = OpenJob::Url(url);
                self.dispatch_open(
                    &crate::target::HostKey::Local,
                    job,
                    Then::Nothing,
                    window,
                    cx,
                );
            }
            LinkTarget::File { path, anchor } => {
                let candidate = md_link_candidate(&path, anchor, prefer_viewer);
                let base_dirs = vec![folder.to_string_lossy().into_owned()];
                let action = if prefer_viewer {
                    LinkAction::Viewer
                } else {
                    LinkAction::DefaultApp
                };
                let opening = Opening {
                    origin,
                    action,
                    at: None,
                };
                let path = PathLink {
                    candidates: vec![candidate],
                    base_dirs,
                    opening,
                    menu: None,
                };
                self.resolve_path(path, window, cx);
            }
            LinkTarget::Anchor(slug) => self.scroll_to_md_anchor(tab_id, &slug, cx),
            LinkTarget::Stretchtext { .. } => {
                tracing::debug!(tab_id, "a viewer tab has no stretchtext to open");
            }
            LinkTarget::Issue { identifier, .. } => {
                tracing::debug!(tab_id, %identifier, "a viewer tab has no issue to raise");
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
            0,
            tab_id.to_owned(),
            LinkRoute::ViewerTab,
            (font.family.as_deref(), font.size),
            t,
        );
        let doc = Arc::clone(doc);
        let selector = format!("viewer-md-{tab_id}");
        div()
            .driver_id(move || selector)
            .flex_1()
            .min_h(px(0.0))
            .text_size(px(UI_TEXT_SIZE))
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
                crate::fonts::ui_text_size(cx),
                crate::fonts::ui_weight_shift(cx),
                &selector,
                ButtonSize::Compact,
                enabled,
                Layer::Base,
                &theme(cx),
            )
        } else {
            outlined_button(
                crate::fonts::ui_text_size(cx),
                crate::fonts::ui_weight_shift(cx),
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

/// The link candidate a Markdown link to file `path` with `anchor` opens.
/// Only the viewer (`prefer_viewer`) takes the anchor: an `#L<n>` one is a
/// line on any file, any other goes on as the suffix of a Markdown file, a
/// heading, and is dropped for every other file. The external app opens the
/// file as it did.
fn md_link_candidate(
    path: &Path,
    anchor: Option<String>,
    prefer_viewer: bool,
) -> TerminalLinkCandidate {
    let anchor = anchor.filter(|_| prefer_viewer);
    let line = anchor.as_deref().and_then(links::anchor_line);
    TerminalLinkCandidate {
        path: path.to_string_lossy().into_owned(),
        line,
        column: None,
        suffix: anchor
            .filter(|_| line.is_none() && viewer_tab::is_markdown(&path.to_string_lossy()))
            .map(|anchor| format!("#{anchor}")),
    }
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

    #[test]
    fn a_markdown_link_to_a_web_page_drops_its_fragment() {
        let page = md_link_candidate(Path::new("C:/docs/page.html"), Some("x".to_owned()), true);
        assert_eq!(page.suffix, None, "a web page's fragment is dropped");
        assert_eq!(page.line, None);

        let notes = md_link_candidate(Path::new("C:/docs/notes.md"), Some("x".to_owned()), true);
        assert_eq!(
            notes.suffix.as_deref(),
            Some("#x"),
            "a Markdown file keeps its heading"
        );

        let line = md_link_candidate(Path::new("C:/docs/page.html"), Some("l3".to_owned()), true);
        assert_eq!(
            (line.line, line.suffix),
            (Some(3), None),
            "a line anchor is a line on any file"
        );

        let external =
            md_link_candidate(Path::new("C:/docs/notes.md"), Some("x".to_owned()), false);
        assert_eq!(external.suffix, None, "the external app takes no anchor");
    }

    fn spans_of(text: &str) -> Vec<Span> {
        match md_blocks::parse(text, Path::new("notes"), md_blocks::SoftBreaks::Space)
            .blocks
            .first()
        {
            Some(Block::Paragraph(spans)) => spans.clone(),
            other => panic!("one paragraph, got {other:?}"),
        }
    }

    #[test]
    fn highlight_split_cuts_span_ranges_around_the_selection() {
        let bg = gpui::red();
        let bold = HighlightStyle {
            font_weight: Some(FontWeight::BOLD),
            ..HighlightStyle::default()
        };
        let italic = HighlightStyle {
            font_style: Some(FontStyle::Italic),
            ..HighlightStyle::default()
        };
        let lit = |style: HighlightStyle| HighlightStyle {
            background_color: Some(bg),
            ..style
        };
        let spans = vec![(0..4, bold), (4..8, italic), (10..12, bold)];
        assert_eq!(select_highlights(spans.clone(), None, bg), spans);
        assert_eq!(select_highlights(spans.clone(), Some(3..3), bg), spans);
        assert_eq!(
            select_highlights(spans.clone(), Some(2..11), bg),
            vec![
                (0..2, bold),
                (2..4, lit(bold)),
                (4..8, lit(italic)),
                (8..10, lit(HighlightStyle::default())),
                (10..11, lit(bold)),
                (11..12, bold),
            ]
        );
        assert_eq!(
            select_highlights(spans.clone(), Some(5..6), bg),
            vec![
                (0..4, bold),
                (4..5, italic),
                (5..6, lit(italic)),
                (6..8, italic),
                (10..12, bold),
            ]
        );
        assert_eq!(
            select_highlights(Vec::new(), Some(1..3), bg),
            vec![(1..3, lit(HighlightStyle::default()))]
        );
    }

    #[test]
    fn only_ctrl_alone_sends_a_file_link_out_of_the_viewer() {
        // `Modifiers::secondary_key` needs an `App`; this is its value on
        // this platform, the key `prefers_viewer` reads.
        let mac = cfg!(target_os = "macos");
        let ctrl = Modifiers {
            platform: mac,
            control: !mac,
            ..Modifiers::none()
        };
        let ctrl_shift = Modifiers {
            shift: true,
            ..ctrl
        };
        assert!(prefers_viewer(Modifiers::none()), "a plain click views");
        assert!(!prefers_viewer(ctrl), "Ctrl+click opens externally");
        assert!(prefers_viewer(ctrl_shift), "Ctrl+Shift+click views");
    }

    #[test]
    fn unit_texts_lists_rows_and_cells_without_markers_or_rules() {
        let doc = prepare_text(
            "# Title\n\n- one\n\n| a | b |\n| --- | --- |\n| c | d |\n\n```\nfn f() {}\n\nlet x = 1;\n```\n\n---\n",
        );
        let listed = doc.unit_texts();
        let units: Vec<(usize, usize, &str)> = listed
            .iter()
            .map(|unit| (unit.row, unit.part, unit.text.as_str()))
            .collect();
        assert_eq!(
            units,
            vec![
                (0, 0, "Title"),
                (1, 0, "one"),
                (2, 0, "a"),
                (2, 1, "b"),
                (3, 0, "c"),
                (3, 1, "d"),
                (4, 0, "fn f() {}"),
                (5, 0, ""),
                (6, 0, "let x = 1;"),
            ]
        );
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
            let style = span_style(span, LinkRoute::WebOnly, &UiTheme::CONTRAST);
            assert!(
                style.underline.is_none() && style.color.is_none(),
                "{label} is plain text: {style:?}"
            );
        }
        assert_eq!(link_ranges(&spans, LinkRoute::ViewerTab).len(), 3);
    }

    #[test]
    fn the_chat_route_follows_web_stretch_and_file_links() {
        let targets = [
            LinkTarget::Url("https://example.com".to_owned()),
            LinkTarget::Stretchtext {
                key: "k".to_owned(),
            },
            LinkTarget::File {
                path: PathBuf::from("logs/app.log"),
                anchor: None,
            },
            LinkTarget::Anchor("top".to_owned()),
            LinkTarget::Refused,
        ];
        let follows = |route: LinkRoute| -> Vec<bool> {
            targets.iter().map(|target| route.follows(target)).collect()
        };
        assert_eq!(follows(LinkRoute::Chat), [true, true, true, false, false]);
        assert_eq!(follows(LinkRoute::Phone), [true, true, true, false, false]);
        assert_eq!(
            follows(LinkRoute::WebOnly),
            [true, true, false, false, false]
        );
    }

    #[test]
    fn the_phone_route_follows_issue_links_and_the_others_do_not() {
        let issue = LinkTarget::Issue {
            identifier: "RT-12".to_owned(),
            web_url: "https://linear.app/ws/issue/RT-12".to_owned(),
        };
        assert!(LinkRoute::Phone.follows(&issue));
        assert!(
            !LinkRoute::Chat.follows(&issue),
            "the desktop chat keeps an identifier plain"
        );
        assert!(!LinkRoute::WebOnly.follows(&issue));
        assert!(LinkRoute::ViewerTab.follows(&issue));
    }

    #[test]
    fn a_desktop_chat_answer_draws_an_issue_identifier_as_plain_text() {
        let context = IssueContext::new(&["RT".to_owned()], Some("ws"), true);
        let answer = prepare_answer(
            &Composed {
                source: "see RT-12 for it".to_owned(),
                keys: Vec::new(),
            },
            &context,
            &AtomicBool::new(false),
        );
        let spans = match &answer.rows[0].kind {
            RowKind::Text(spans) => spans.as_slice(),
            other => panic!("one paragraph, got {other:?}"),
        };
        let issue = spans
            .iter()
            .find(|span| matches!(&span.link, Some(LinkTarget::Issue { .. })))
            .expect("the identifier parses as an issue link");
        assert_eq!(issue.text, "RT-12");
        assert!(
            link_ranges(spans, LinkRoute::Chat).is_empty(),
            "no click on the desktop"
        );
        let style = span_style(issue, LinkRoute::Chat, &UiTheme::CONTRAST);
        assert!(
            style.underline.is_none() && style.color.is_none(),
            "{style:?}"
        );
        assert_eq!(link_ranges(spans, LinkRoute::Phone).len(), 1);
    }

    #[test]
    fn a_chat_answer_follows_its_file_links_and_not_its_anchors() {
        let spans = spans_of("[log](logs/app.log) [top](#top)\n");
        let (text, _) = inline_text(&spans);
        let links = link_ranges(&spans, LinkRoute::Chat);
        let [(log, target)] = links.as_slice() else {
            panic!("only the file link is clickable, got {links:?}");
        };
        assert_eq!(&text[log.clone()], "log");
        assert_eq!(
            *target,
            LinkTarget::File {
                path: Path::new("notes").join("logs/app.log"),
                anchor: None,
            }
        );
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

    #[test]
    fn a_chat_answer_keeps_single_line_breaks() {
        let answer = prepare_answer(
            &Composed {
                source: "1\n2\n3\n".to_owned(),
                keys: Vec::new(),
            },
            &IssueContext::default(),
            &AtomicBool::new(false),
        );
        assert_eq!(answer.drawn_text(), "1\n2\n3");
        let viewer = prepare("1\n2\n3\n", Path::new(""), &AtomicBool::new(false));
        assert_eq!(
            viewer.drawn_text(),
            "1 2 3",
            "a file viewer keeps the space"
        );
    }

    #[test]
    fn a_chat_answer_keeps_the_backslash_before_a_dot_in_a_windows_path() {
        let answer = prepare_answer(
            &Composed {
                source: r"Open X:\dev\.tmp\repo\src\main.rs now".to_owned(),
                keys: Vec::new(),
            },
            &IssueContext::default(),
            &AtomicBool::new(false),
        );
        assert_eq!(
            answer.drawn_text(),
            r"Open X:\dev\.tmp\repo\src\main.rs now"
        );
    }

    #[test]
    fn a_markdown_file_keeps_commonmark_escapes_in_a_windows_path() {
        let viewer = prepare(r"X:\dev\.tmp", Path::new(""), &AtomicBool::new(false));
        assert_eq!(viewer.drawn_text(), r"X:\dev.tmp");
    }
}
