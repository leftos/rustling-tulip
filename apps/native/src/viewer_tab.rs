//! The viewer tabs' state, without the UI: the `OpenViewerTab` requests
//! this client sent and the tab one of them waits to activate, and per tab
//! its path, where its file comes from, its load state, the line an open
//! asked for and its Markdown mode, and the file as read. The root view
//! renders it.

use std::cell::Cell;
use std::collections::HashMap;
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use gpui::{ListAlignment, ListState, UniformListScrollHandle, px};

use crate::diff_tab::{PENDING_ACTIVATE_TTL, split_path};
use crate::file_fetch::is_under;
use crate::md_blocks;
use crate::remote_mode::LOCAL_FILES_REASON;
use crate::syntax::Highlighted;
use crate::viewer_md_view::MdDoc;
use crate::viewer_text::{self, NotText};

/// The toast an `OpenViewerTab` the daemon refused raises.
pub const OPEN_FAILED_TITLE: &str = "Couldn't open the file";

/// Where a viewer tab's file comes from. Not stored in the tab: it is
/// derived again whenever the tab is built, so a tab restored on a remote
/// host never reads a path of this PC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewerOrigin {
    /// A copy fetched from a remote host, under the download root.
    Fetched,
    /// A file of this PC, opened from a link while on the local daemon.
    Link,
    /// A file of this PC while connected to a remote host: never read.
    NotAvailable,
}

/// How far a viewer tab got with its file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewerLoad {
    /// Not read yet.
    Unread,
    /// Being read on the background executor.
    Loading,
    /// Read, and its lines are shown.
    Loaded,
    /// Over the viewer's size cap.
    TooLarge,
    /// A NUL byte near its start: not text.
    Binary,
    /// Neither UTF-8 nor UTF-16 with a byte order mark.
    NotDecodable,
    /// Gone, or it could not be read.
    Missing,
    /// Never read: the file is not available on this connection.
    NotAvailable,
}

/// The banner of a file over the viewer's size cap.
pub(crate) const TOO_LARGE_BANNER: &str = "This file is over 2 MiB, too large for the viewer.";
/// The banner of a binary file.
pub(crate) const BINARY_BANNER: &str = "This file is binary, so the viewer does not show it.";
/// The banner of a file that is not UTF-8 or UTF-16 with a byte order mark.
pub(crate) const NOT_DECODABLE_BANNER: &str =
    "This file is neither UTF-8 nor UTF-16 with a byte order mark, so the viewer does not show it.";
/// The banner of a file that is gone or cannot be read.
pub(crate) const MISSING_BANNER: &str = "The file is gone or cannot be read.";

/// Why a viewer tab's file read shows no text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadFailure {
    NotText(NotText),
    /// Gone, or the read failed.
    Unreadable,
}

/// A viewer tab's file as read: its text, each line's byte range in it, and
/// the language it is coloured as.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ViewerText {
    text: String,
    lines: Vec<Range<usize>>,
    language: Option<&'static str>,
}

impl ViewerText {
    pub(crate) fn new(text: String, language: Option<&'static str>) -> Self {
        let base = text.as_ptr() as usize;
        let lines = viewer_text::split_lines(&text)
            .into_iter()
            .map(|line| {
                let start = line.as_ptr() as usize - base;
                start..start + line.len()
            })
            .collect();
        Self {
            text,
            lines,
            language,
        }
    }

    /// The whole text, as decoded.
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    pub(crate) fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// Line `index`, counted from zero, without its terminator.
    pub(crate) fn line(&self, index: usize) -> &str {
        self.lines
            .get(index)
            .and_then(|range| self.text.get(range.clone()))
            .unwrap_or_default()
    }

    /// The language id the text is coloured as, if any.
    pub(crate) fn language(&self) -> Option<&'static str> {
        self.language
    }

    /// The lines joined with `\n`, so the highlighter splits them as the
    /// viewer does.
    pub(crate) fn joined(&self) -> String {
        let mut joined = String::with_capacity(self.text.len());
        for index in 0..self.lines.len() {
            if index > 0 {
                joined.push('\n');
            }
            joined.push_str(self.line(index));
        }
        joined
    }
}

/// Reads and decodes the file at `path` for the viewer.
///
/// # Errors
/// [`ReadFailure::NotText`] for a file over the cap, binary or not
/// decodable; [`ReadFailure::Unreadable`] when it is gone or the read fails.
pub(crate) fn read_file(path: &Path) -> Result<ViewerText, ReadFailure> {
    let unreadable = |err: std::io::Error| {
        tracing::warn!(path = %path.display(), "the viewer could not read the file: {err}");
        ReadFailure::Unreadable
    };
    if std::fs::metadata(path).map_err(unreadable)?.len() > viewer_text::MAX_VIEW_BYTES {
        return Err(ReadFailure::NotText(NotText::TooLarge));
    }
    let bytes = std::fs::read(path).map_err(unreadable)?;
    let text = viewer_text::decode(&bytes).map_err(ReadFailure::NotText)?;
    Ok(ViewerText::new(text, viewer_text::language_for_path(path)))
}

/// How far past the rendered view's edges its list lays out blocks, in px.
const MD_OVERDRAW: f32 = 400.0;

/// The next read's number; one counter for every tab, so a read of a closed
/// tab never lands on a later tab of the same id.
static NEXT_READ: AtomicU64 = AtomicU64::new(1);

/// How a Markdown file shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkdownMode {
    Rendered,
    Source,
}

/// The tab an `OpenViewerTab` answer activates, and the line it asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Activation {
    pub tab_id: String,
    pub line: Option<u32>,
}

/// The `OpenViewerTab`s out, and the tab one of them waits to activate.
#[derive(Debug, Default)]
pub(crate) struct ViewerOpens {
    next_open: u64,
    /// The `OpenViewerTab` ids the daemon has not answered, with the line
    /// each asked for.
    sent: HashMap<String, Option<u32>>,
    /// A tab the daemon named before its `TabUpdated` arrived, and when.
    pending: Option<(Activation, Instant)>,
}

impl ViewerOpens {
    /// A fresh `OpenViewerTab` id for an open at `line`, recorded as sent.
    pub(crate) fn open_id(&mut self, line: Option<u32>) -> String {
        self.next_open += 1;
        let id = format!("viewer-open-{}", self.next_open);
        self.sent.insert(id.clone(), line);
        id
    }

    /// `ViewerTabOpened`: the tab to activate now, when this client asked
    /// for it and the tab list already holds it (`known`). A tab the list
    /// does not hold yet waits for its `TabUpdated`; another client's open
    /// is ignored.
    pub(crate) fn opened(
        &mut self,
        id: &str,
        tab_id: &str,
        known: bool,
        now: Instant,
    ) -> Option<Activation> {
        let line = self.sent.remove(id)?;
        let activation = Activation {
            tab_id: tab_id.to_owned(),
            line,
        };
        if known {
            self.pending = None;
            return Some(activation);
        }
        self.pending = Some((activation, now));
        None
    }

    /// The waiting tab, once `known` says the tab list holds it; one that
    /// waited [`PENDING_ACTIVATE_TTL`] is dropped instead.
    pub(crate) fn take_ready(
        &mut self,
        known: impl Fn(&str) -> bool,
        now: Instant,
    ) -> Option<Activation> {
        let (activation, at) = self.pending.as_ref()?;
        if now.saturating_duration_since(*at) >= PENDING_ACTIVATE_TTL {
            self.pending = None;
            return None;
        }
        if !known(&activation.tab_id) {
            return None;
        }
        self.pending.take().map(|(activation, _)| activation)
    }

    /// Whether `id` is an `OpenViewerTab` of this client, which is
    /// forgotten.
    pub(crate) fn failed(&mut self, id: &str) -> bool {
        self.sent.remove(id).is_some()
    }

    /// A new connection: no answer to an old request will come.
    pub(crate) fn reset(&mut self) {
        self.sent.clear();
        self.pending = None;
    }
}

/// Where the file at `path` comes from: a fetched copy when it lies under
/// `download_root`, else a file of this PC, which only a client that opens
/// local files reads.
pub(crate) fn derive_origin(
    path: &Path,
    download_root: Option<&Path>,
    opens_local_files: bool,
) -> ViewerOrigin {
    if download_root.is_some_and(|root| is_under(root, path)) {
        ViewerOrigin::Fetched
    } else if opens_local_files {
        ViewerOrigin::Link
    } else {
        ViewerOrigin::NotAvailable
    }
}

/// `raw` as the daemon stores a viewer tab's path: trimmed, absolute, with
/// this OS's separators.
pub(crate) fn normalise_path(raw: &str) -> std::io::Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "the path is empty",
        ));
    }
    let absolute = std::path::absolute(trimmed)?;
    absolute.into_os_string().into_string().map_err(|path| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("the path is not valid Unicode: {}", path.to_string_lossy()),
        )
    })
}

/// One viewer tab's state.
#[derive(Debug)]
pub struct ViewerTab {
    path: String,
    origin: ViewerOrigin,
    load: ViewerLoad,
    /// The number of the read in flight or landed; `None` before the first.
    read: Option<u64>,
    /// The file's text, once read.
    text: Option<Arc<ViewerText>>,
    /// The text's syntax classes, once highlighted; `None` leaves it
    /// uncoloured.
    spans: Option<Arc<Highlighted>>,
    /// Set to stop the highlighting run in flight.
    highlight_cancel: Arc<AtomicBool>,
    /// The line list's scroll.
    scroll: UniformListScrollHandle,
    /// Whether the line target still waits to be scrolled to.
    jump_pending: Cell<bool>,
    /// The line the last open asked for.
    line_target: Option<u32>,
    /// How a Markdown file shows; `None` for any other file.
    markdown: Option<MarkdownMode>,
    /// Whether the text read is over the render cap, so it shows as source
    /// only.
    markdown_too_large: bool,
    /// The rendered document of the text read and its block list, once
    /// parsed.
    md: Option<(Arc<MdDoc>, ListState)>,
}

impl Drop for ViewerTab {
    /// A closed tab stops its highlighting.
    fn drop(&mut self) {
        self.highlight_cancel.store(true, Ordering::Relaxed);
    }
}

impl ViewerTab {
    /// A tab of `path`; a Markdown file starts in `markdown_default`, the
    /// saved default.
    pub(crate) fn new(path: String, origin: ViewerOrigin, markdown_default: MarkdownMode) -> Self {
        let load = match origin {
            ViewerOrigin::Fetched | ViewerOrigin::Link => ViewerLoad::Unread,
            ViewerOrigin::NotAvailable => ViewerLoad::NotAvailable,
        };
        let markdown = is_markdown(&path).then_some(markdown_default);
        Self {
            path,
            origin,
            load,
            read: None,
            text: None,
            spans: None,
            highlight_cancel: Arc::new(AtomicBool::new(false)),
            scroll: UniformListScrollHandle::new(),
            jump_pending: Cell::new(false),
            line_target: None,
            markdown,
            markdown_too_large: false,
            md: None,
        }
    }

    /// Starts reading an unread tab's file: returns the read's number, or
    /// `None` when the tab is read, being read or never reads.
    pub(crate) fn begin_read(&mut self) -> Option<u64> {
        if self.load != ViewerLoad::Unread {
            return None;
        }
        let read = NEXT_READ.fetch_add(1, Ordering::Relaxed);
        self.read = Some(read);
        self.load = ViewerLoad::Loading;
        Some(read)
    }

    /// An open named this tab again: a tab whose read ended goes back to
    /// unread, dropping its text, its colours and the highlighting in
    /// flight, so the file is read anew.
    pub(crate) fn reread(&mut self) {
        match self.load {
            ViewerLoad::Unread | ViewerLoad::Loading | ViewerLoad::NotAvailable => {}
            ViewerLoad::Loaded
            | ViewerLoad::TooLarge
            | ViewerLoad::Binary
            | ViewerLoad::NotDecodable
            | ViewerLoad::Missing => {
                self.highlight_cancel.store(true, Ordering::Relaxed);
                self.highlight_cancel = Arc::new(AtomicBool::new(false));
                self.text = None;
                self.spans = None;
                self.md = None;
                self.load = ViewerLoad::Unread;
            }
        }
    }

    /// Read `read` ended with `result`; returns whether it landed, which
    /// only the tab's read in flight does.
    pub(crate) fn finish_read(
        &mut self,
        read: u64,
        result: Result<ViewerText, ReadFailure>,
    ) -> bool {
        if self.load != ViewerLoad::Loading || self.read != Some(read) {
            return false;
        }
        self.load = match result {
            Ok(text) => {
                self.apply_render_cap(text.text().len());
                self.text = Some(Arc::new(text));
                self.jump_pending.set(self.line_target.is_some());
                ViewerLoad::Loaded
            }
            Err(ReadFailure::NotText(NotText::TooLarge)) => ViewerLoad::TooLarge,
            Err(ReadFailure::NotText(NotText::Binary)) => ViewerLoad::Binary,
            Err(ReadFailure::NotText(NotText::NotDecodable)) => ViewerLoad::NotDecodable,
            Err(ReadFailure::Unreadable) => ViewerLoad::Missing,
        };
        true
    }

    /// A Markdown text of `len` bytes over the render cap shows as source,
    /// and its mode is locked there.
    fn apply_render_cap(&mut self, len: usize) {
        self.markdown_too_large = self.markdown.is_some() && !md_blocks::renders(len);
        if self.markdown_too_large {
            self.markdown = Some(MarkdownMode::Source);
        }
    }

    /// The text read, once it landed.
    pub(crate) fn text(&self) -> Option<&Arc<ViewerText>> {
        self.text.as_ref()
    }

    /// How many lines the text read has; none before it landed.
    #[must_use]
    pub fn line_count(&self) -> usize {
        self.text.as_ref().map_or(0, |text| text.line_count())
    }

    /// The flag that stops this tab's highlighting.
    pub(crate) fn highlight_cancel(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.highlight_cancel)
    }

    /// Highlighting of read `read` landed with `spans`; kept while that read
    /// is still the one shown.
    pub(crate) fn finish_highlight(&mut self, read: u64, spans: Arc<Highlighted>) -> bool {
        if self.read != Some(read) || self.load != ViewerLoad::Loaded {
            return false;
        }
        self.spans = Some(spans);
        true
    }

    /// The text's syntax classes, once highlighted.
    pub(crate) fn spans(&self) -> Option<&Arc<Highlighted>> {
        self.spans.as_ref()
    }

    /// The line list's scroll.
    pub(crate) fn scroll(&self) -> &UniformListScrollHandle {
        &self.scroll
    }

    /// The row the line target names, counted from zero and clamped to the
    /// text read; `None` with no target or no line.
    #[must_use]
    pub fn jump_row(&self) -> Option<usize> {
        let line = usize::try_from(self.line_target?).ok()?;
        let last = self.line_count().checked_sub(1)?;
        Some(line.saturating_sub(1).min(last))
    }

    /// The row to scroll to once, when an open asked for a line not yet
    /// scrolled to.
    pub(crate) fn take_jump(&self) -> Option<usize> {
        if !self.jump_pending.get() {
            return None;
        }
        let row = self.jump_row()?;
        self.jump_pending.set(false);
        Some(row)
    }

    /// The file's path on this PC.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The file's name: the path's last component.
    #[must_use]
    pub fn name(&self) -> &str {
        display_name(&self.path)
    }

    #[must_use]
    pub fn origin(&self) -> ViewerOrigin {
        self.origin
    }

    #[must_use]
    pub fn load(&self) -> ViewerLoad {
        self.load
    }

    /// The line the last open asked for.
    #[must_use]
    pub fn line_target(&self) -> Option<u32> {
        self.line_target
    }

    /// An open asked for `line`: a line moves the target, none leaves it. A
    /// Markdown tab asked for a line shows its source, where lines are; the
    /// saved default is left as it is.
    pub(crate) fn set_line_target(&mut self, line: Option<u32>) {
        if line.is_some() {
            self.line_target = line;
            self.jump_pending.set(true);
            if self.markdown.is_some() {
                self.markdown = Some(MarkdownMode::Source);
            }
        }
    }

    /// How a Markdown file shows; `None` for any other file.
    #[must_use]
    pub fn markdown(&self) -> Option<MarkdownMode> {
        self.markdown
    }

    /// Whether the text read is over the render cap, so the mode stays
    /// [`MarkdownMode::Source`].
    #[must_use]
    pub fn markdown_too_large(&self) -> bool {
        self.markdown_too_large
    }

    /// Shows a Markdown file as `mode`; returns whether it took, which it
    /// does not for another file or a text over the render cap.
    pub(crate) fn set_markdown(&mut self, mode: MarkdownMode) -> bool {
        if self.markdown.is_none() || self.markdown_too_large {
            return false;
        }
        self.markdown = Some(mode);
        true
    }

    /// Whether the text read is a Markdown document the rendered view can
    /// show.
    pub(crate) fn renders_markdown(&self) -> bool {
        self.markdown.is_some() && !self.markdown_too_large && self.load == ViewerLoad::Loaded
    }

    /// The parse of read `read` landed as `doc`; kept, with a fresh block
    /// list, while that read is still the one shown.
    pub(crate) fn finish_markdown(&mut self, read: u64, doc: Arc<MdDoc>) -> bool {
        if self.read != Some(read) || !self.renders_markdown() {
            return false;
        }
        let list = ListState::new(doc.row_count(), ListAlignment::Top, px(MD_OVERDRAW));
        self.md = Some((doc, list));
        true
    }

    /// The rendered document and its block list, once parsed.
    pub(crate) fn markdown_doc(&self) -> Option<&(Arc<MdDoc>, ListState)> {
        self.md.as_ref()
    }

    /// The banner that says why no text shows, when none can.
    #[must_use]
    pub fn note(&self) -> Option<&'static str> {
        match self.load {
            ViewerLoad::Unread | ViewerLoad::Loading | ViewerLoad::Loaded => None,
            ViewerLoad::TooLarge => Some(TOO_LARGE_BANNER),
            ViewerLoad::Binary => Some(BINARY_BANNER),
            ViewerLoad::NotDecodable => Some(NOT_DECODABLE_BANNER),
            ViewerLoad::Missing => Some(MISSING_BANNER),
            ViewerLoad::NotAvailable => Some(LOCAL_FILES_REASON),
        }
    }
}

/// What names a viewer tab of `path`: its last component, or the whole path
/// when that is empty (a path ending in a separator).
pub(crate) fn display_name(path: &str) -> &str {
    match split_path(path).0 {
        "" => path,
        name => name,
    }
}

/// Whether `path` names a Markdown file, by its extension.
fn is_markdown(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("markdown"))
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use std::path::Path;
    use std::time::Instant;

    use super::*;
    use crate::md_blocks::RENDER_CAP;
    use crate::remote_mode::LOCAL_FILES_REASON;

    const NOTES: &str = r"C:\notes\a.md";

    fn activation(tab_id: &str, line: Option<u32>) -> Activation {
        Activation {
            tab_id: tab_id.to_owned(),
            line,
        }
    }

    #[test]
    fn an_open_answered_before_its_tab_arrived_waits_for_it() {
        let mut opens = ViewerOpens::default();
        let now = Instant::now();
        let id = opens.open_id(Some(12));
        assert_eq!(id, "viewer-open-1");
        assert_eq!(opens.opened(&id, "v9", false, now), None);
        assert_eq!(
            opens.take_ready(|tab| tab == "t1", now),
            None,
            "v9 not listed yet"
        );
        assert_eq!(
            opens.take_ready(|tab| tab == "v9", now),
            Some(activation("v9", Some(12)))
        );
        assert_eq!(opens.take_ready(|_| true, now), None, "activated once");
        assert!(!opens.failed(&id), "the answered id is forgotten");
    }

    #[test]
    fn a_second_open_moves_the_target_line() {
        let mut opens = ViewerOpens::default();
        let now = Instant::now();
        let mut tab = ViewerTab::new(NOTES.to_owned(), ViewerOrigin::Link, MarkdownMode::Rendered);
        assert_eq!(tab.line_target(), None);
        let first = opens.open_id(Some(10));
        let second = opens.open_id(Some(42));

        let opened = opens.opened(&first, "v1", true, now);
        assert_eq!(opened, Some(activation("v1", Some(10))));
        tab.set_line_target(opened.and_then(|open| open.line));
        assert_eq!(tab.line_target(), Some(10));

        let opened = opens.opened(&second, "v1", true, now);
        assert_eq!(opened, Some(activation("v1", Some(42))));
        tab.set_line_target(opened.and_then(|open| open.line));
        assert_eq!(tab.line_target(), Some(42), "the second open moves it");

        let third = opens.open_id(None);
        let opened = opens.opened(&third, "v1", true, now);
        assert_eq!(opened, Some(activation("v1", None)));
        tab.set_line_target(opened.and_then(|open| open.line));
        assert_eq!(
            tab.line_target(),
            Some(42),
            "an open without a line leaves the target"
        );
    }

    #[test]
    fn origin_is_derived_fetched_under_the_download_root() {
        let root = Path::new(r"C:\data\remote-files");
        let fetched = root.join("box").join("r1").join("a.md");
        assert_eq!(
            derive_origin(&fetched, Some(root), false),
            ViewerOrigin::Fetched,
            "a fetched copy reads on a remote host"
        );
        assert_eq!(
            derive_origin(&fetched, Some(root), true),
            ViewerOrigin::Fetched
        );
        assert_eq!(
            derive_origin(root, Some(root), true),
            ViewerOrigin::Link,
            "the root itself is no fetched copy"
        );
        let escape = root.join("..").join("x.md");
        assert_eq!(
            derive_origin(&escape, Some(root), false),
            ViewerOrigin::NotAvailable,
            "a path climbing out of the root is not under it"
        );
    }

    #[test]
    fn origin_is_link_on_local_and_unavailable_on_a_remote_host() {
        let path = Path::new(NOTES);
        let root = Path::new(r"C:\data\remote-files");
        assert_eq!(derive_origin(path, Some(root), true), ViewerOrigin::Link);
        assert_eq!(
            derive_origin(path, None, true),
            ViewerOrigin::Link,
            "no download root"
        );
        assert_eq!(
            derive_origin(path, Some(root), false),
            ViewerOrigin::NotAvailable
        );

        let local = ViewerTab::new(NOTES.to_owned(), ViewerOrigin::Link, MarkdownMode::Rendered);
        assert_eq!(local.load(), ViewerLoad::Unread);
        assert_eq!(local.note(), None);
        assert_eq!(local.name(), "a.md");
        let remote = ViewerTab::new(
            NOTES.to_owned(),
            ViewerOrigin::NotAvailable,
            MarkdownMode::Rendered,
        );
        assert_eq!(remote.load(), ViewerLoad::NotAvailable, "it reads nothing");
        assert_eq!(remote.note(), Some(LOCAL_FILES_REASON));
        assert_eq!(remote.name(), "a.md");
    }

    fn text(text: &str) -> ViewerText {
        ViewerText::new(text.to_owned(), None)
    }

    /// A tab of [`NOTES`] whose file read landed as `text`.
    fn loaded(text_read: &str) -> ViewerTab {
        let mut tab = ViewerTab::new(NOTES.to_owned(), ViewerOrigin::Link, MarkdownMode::Rendered);
        let run = tab.begin_read().expect("an unread tab reads");
        assert!(tab.finish_read(run, Ok(text(text_read))));
        tab
    }

    #[test]
    fn an_unread_tab_loads_once_and_a_stale_result_is_dropped() {
        let mut closed =
            ViewerTab::new(NOTES.to_owned(), ViewerOrigin::Link, MarkdownMode::Rendered);
        let stale = closed.begin_read().expect("an unread tab reads");
        drop(closed);

        let mut tab = ViewerTab::new(NOTES.to_owned(), ViewerOrigin::Link, MarkdownMode::Rendered);
        let run = tab
            .begin_read()
            .expect("the tab of the same path reads too");
        assert_eq!(tab.load(), ViewerLoad::Loading);
        assert_eq!(tab.begin_read(), None, "a tab reads its file once");

        assert!(
            !tab.finish_read(stale, Ok(text("old\n"))),
            "the closed tab's read does not land on the new one"
        );
        assert_eq!(tab.load(), ViewerLoad::Loading);

        assert!(tab.finish_read(run, Ok(text("a\nb\n"))));
        assert_eq!(tab.load(), ViewerLoad::Loaded);
        assert_eq!(tab.line_count(), 2);
        assert_eq!(tab.note(), None);
        assert!(
            !tab.finish_read(run, Ok(text("again\n"))),
            "a read lands once"
        );
        assert_eq!(tab.line_count(), 2);
    }

    #[test]
    fn a_line_target_is_clamped_to_the_file() {
        let mut tab = loaded("a\nb\nc\n");
        assert_eq!(tab.jump_row(), None, "no line asked for");
        tab.set_line_target(Some(2));
        assert_eq!(tab.jump_row(), Some(1), "line 2 is row 1");
        tab.set_line_target(Some(99));
        assert_eq!(tab.jump_row(), Some(2), "past the end is the last line");
        tab.set_line_target(Some(0));
        assert_eq!(tab.jump_row(), Some(0), "line 0 is the first line");

        let mut empty = loaded("");
        empty.set_line_target(Some(5));
        assert_eq!(empty.jump_row(), None, "an empty file has no line");

        let mut unread =
            ViewerTab::new(NOTES.to_owned(), ViewerOrigin::Link, MarkdownMode::Rendered);
        unread.set_line_target(Some(5));
        assert_eq!(unread.jump_row(), None, "nothing is read yet");
    }

    #[test]
    fn a_not_available_tab_never_reads() {
        let mut tab = ViewerTab::new(
            NOTES.to_owned(),
            ViewerOrigin::NotAvailable,
            MarkdownMode::Rendered,
        );
        assert_eq!(tab.begin_read(), None);
        assert_eq!(tab.load(), ViewerLoad::NotAvailable);
        assert!(!tab.finish_read(1, Ok(text("x\n"))), "no read lands");
        assert_eq!(tab.load(), ViewerLoad::NotAvailable);
        assert_eq!(tab.note(), Some(LOCAL_FILES_REASON));
    }

    #[test]
    fn each_not_text_reason_has_its_banner() {
        for (failure, load, banner) in [
            (
                ReadFailure::NotText(NotText::TooLarge),
                ViewerLoad::TooLarge,
                TOO_LARGE_BANNER,
            ),
            (
                ReadFailure::NotText(NotText::Binary),
                ViewerLoad::Binary,
                BINARY_BANNER,
            ),
            (
                ReadFailure::NotText(NotText::NotDecodable),
                ViewerLoad::NotDecodable,
                NOT_DECODABLE_BANNER,
            ),
            (ReadFailure::Unreadable, ViewerLoad::Missing, MISSING_BANNER),
        ] {
            let mut tab =
                ViewerTab::new(NOTES.to_owned(), ViewerOrigin::Link, MarkdownMode::Rendered);
            let run = tab.begin_read().expect("an unread tab reads");
            assert!(tab.finish_read(run, Err(failure)));
            assert_eq!(tab.load(), load);
            assert_eq!(tab.note(), Some(banner), "{load:?}");
            assert_eq!(tab.line_count(), 0, "{load:?} shows no lines");
        }
    }

    #[test]
    fn a_path_ending_in_a_separator_is_named_by_the_whole_path() {
        let tab = ViewerTab::new(
            r"C:\notes\".to_owned(),
            ViewerOrigin::Link,
            MarkdownMode::Rendered,
        );
        assert_eq!(tab.name(), r"C:\notes\");
        assert_eq!(display_name("C:/notes/"), "C:/notes/");
        assert_eq!(display_name(NOTES), "a.md");
    }

    #[test]
    fn a_markdown_tab_follows_the_saved_default() {
        let rendered = ViewerTab::new(NOTES.to_owned(), ViewerOrigin::Link, MarkdownMode::Rendered);
        assert_eq!(rendered.markdown(), Some(MarkdownMode::Rendered));
        let source = ViewerTab::new(NOTES.to_owned(), ViewerOrigin::Link, MarkdownMode::Source);
        assert_eq!(source.markdown(), Some(MarkdownMode::Source));
        let upper = ViewerTab::new(
            r"C:\notes\README.MARKDOWN".to_owned(),
            ViewerOrigin::Link,
            MarkdownMode::Source,
        );
        assert_eq!(upper.markdown(), Some(MarkdownMode::Source));
        let plain = ViewerTab::new(
            r"C:\notes\a.txt".to_owned(),
            ViewerOrigin::Link,
            MarkdownMode::Rendered,
        );
        assert_eq!(plain.markdown(), None, "only a Markdown file has a mode");
    }

    #[test]
    fn a_document_over_the_render_cap_opens_as_source() {
        let mut big = loaded(&"a".repeat(RENDER_CAP + 1));
        assert_eq!(big.markdown(), Some(MarkdownMode::Source));
        assert!(big.markdown_too_large());
        assert!(
            !big.set_markdown(MarkdownMode::Rendered),
            "the toggle is off"
        );
        assert_eq!(big.markdown(), Some(MarkdownMode::Source));

        let mut at_cap = loaded(&"a".repeat(RENDER_CAP));
        assert_eq!(at_cap.markdown(), Some(MarkdownMode::Rendered));
        assert!(!at_cap.markdown_too_large());
        assert!(at_cap.set_markdown(MarkdownMode::Source));
        assert_eq!(at_cap.markdown(), Some(MarkdownMode::Source));
    }
}
