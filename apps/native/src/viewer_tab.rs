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
use std::time::{Instant, SystemTime};

use gpui::{
    Entity, ListAlignment, ListOffset, ListState, Subscription, UniformListScrollHandle, px,
};

use crate::diff_tab::{PENDING_ACTIVATE_TTL, split_path};
use crate::file_fetch::is_under;
use crate::md_blocks;
use crate::remote_mode::LOCAL_FILES_REASON;
use crate::syntax::Highlighted;
use crate::text_input::TextInput;
use crate::viewer_find::FindState;
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
/// The banner of a file that is gone or cannot be read, over the lines of
/// its last read.
pub(crate) const MISSING_LAST_READ_BANNER: &str =
    "The file is gone or cannot be read. Showing the last read.";

/// What a reload compares to tell whether a file changed: its modified time
/// and its size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileStamp {
    pub(crate) modified: Option<SystemTime>,
    pub(crate) len: u64,
}

impl FileStamp {
    fn of(metadata: &std::fs::Metadata) -> Self {
        Self {
            modified: metadata.modified().ok(),
            len: metadata.len(),
        }
    }
}

/// The stamp of the file at `path`; `None` when it cannot be statted.
pub(crate) fn file_stamp(path: &Path) -> Option<FileStamp> {
    match std::fs::metadata(path) {
        Ok(metadata) => Some(FileStamp::of(&metadata)),
        Err(err) => {
            tracing::debug!(path = %path.display(), "the viewer could not stat the file: {err}");
            None
        }
    }
}

/// Why a viewer tab's file read shows no text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadFailure {
    NotText(NotText),
    /// Gone, or the read failed.
    Unreadable,
}

/// A viewer tab's file as read: its text, each line's byte range in it, the
/// language it is coloured as, and the file's stamp when it was read.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ViewerText {
    text: String,
    lines: Vec<Range<usize>>,
    language: Option<&'static str>,
    stamp: Option<FileStamp>,
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
            stamp: None,
        }
    }

    /// The text, read from a file stamped `stamp`.
    #[must_use]
    pub(crate) fn with_stamp(mut self, stamp: FileStamp) -> Self {
        self.stamp = Some(stamp);
        self
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

    /// Every line, without its terminator.
    pub(crate) fn lines(&self) -> impl Iterator<Item = &str> {
        (0..self.lines.len()).map(|index| self.line(index))
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
    let stamp = FileStamp::of(&std::fs::metadata(path).map_err(unreadable)?);
    if stamp.len > viewer_text::MAX_VIEW_BYTES {
        return Err(ReadFailure::NotText(NotText::TooLarge));
    }
    let bytes = std::fs::read(path).map_err(unreadable)?;
    let text = viewer_text::decode(&bytes).map_err(ReadFailure::NotText)?;
    Ok(ViewerText::new(text, viewer_text::language_for_path(path)).with_stamp(stamp))
}

/// The whole lines selected in the Source view: from the line a press began
/// on, the anchor, to the line it reached, the head, either way round.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct LineSelection {
    /// The anchor and the head, counted from zero; `None` when nothing is
    /// selected.
    ends: Option<(usize, usize)>,
}

impl LineSelection {
    /// The lines selected, counted from zero, end exclusive.
    pub(crate) fn range(&self) -> Option<Range<usize>> {
        let (anchor, head) = self.ends?;
        Some(anchor.min(head)..anchor.max(head) + 1)
    }

    /// Selects `line` alone and anchors there.
    pub(crate) fn click(&mut self, line: usize) {
        self.ends = Some((line, line));
    }

    /// Moves the head to `line`, keeping the anchor; with nothing selected
    /// `line` is selected alone.
    pub(crate) fn extend(&mut self, line: usize) {
        match &mut self.ends {
            Some((_, head)) => *head = line,
            None => self.click(line),
        }
    }

    /// Selects every one of `len` lines; none when there are none.
    pub(crate) fn select_all(&mut self, len: usize) {
        self.ends = len.checked_sub(1).map(|last| (0, last));
    }

    pub(crate) fn clear(&mut self) {
        self.ends = None;
    }

    /// Keeps the selection inside a text of `len` lines: ends past its last
    /// line move onto it, and with no line nothing stays selected.
    pub(crate) fn clamp(&mut self, len: usize) {
        let Some(last) = len.checked_sub(1) else {
            self.clear();
            return;
        };
        if let Some((anchor, head)) = &mut self.ends {
            *anchor = (*anchor).min(last);
            *head = (*head).min(last);
        }
    }

    /// The selected lines of `text` joined with `\n`, without gutter
    /// numbers and with no trailing newline, whatever the file's line
    /// endings; `None` when no line of it is selected.
    pub(crate) fn text(&self, text: &ViewerText) -> Option<String> {
        let lines: Vec<&str> = self
            .range()?
            .filter(|index| *index < text.line_count())
            .map(|index| text.line(index))
            .collect();
        (!lines.is_empty()).then(|| lines.join("\n"))
    }
}

/// A tab's find bar, made the first time it opens: its query field and the
/// subscriptions to the field's events.
pub(crate) struct FindBar {
    pub(crate) input: Entity<TextInput>,
    _subscriptions: Vec<Subscription>,
}

impl FindBar {
    pub(crate) fn new(input: Entity<TextInput>, subscriptions: Vec<Subscription>) -> Self {
        Self {
            input,
            _subscriptions: subscriptions,
        }
    }
}

impl std::fmt::Debug for FindBar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FindBar").finish_non_exhaustive()
    }
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
    /// The lines selected in the Source view.
    selection: LineSelection,
    /// Whether a left press on a row is held, so a move over the rows
    /// extends the selection.
    selecting: bool,
    /// Find in the Source view: the query, its matches and the one the bar
    /// is on.
    find: FindState,
    /// The find bar, once it opened on this tab.
    find_bar: Option<FindBar>,
    /// Whether the find bar is open.
    find_open: bool,
    /// The file's stamp at the last read that landed text; `None` before
    /// one, or when the last read found no text.
    last_stamp: Option<FileStamp>,
    /// The reload in flight, which keeps the text shown until it lands;
    /// `None` when no reload is.
    reload: Option<Reload>,
}

/// A reload in flight: its read's number and the stamp of the stat that
/// began it.
#[derive(Debug, Clone, Copy)]
struct Reload {
    read: u64,
    stamp: Option<FileStamp>,
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
            selection: LineSelection::default(),
            selecting: false,
            find: FindState::default(),
            find_bar: None,
            find_open: false,
            last_stamp: None,
            reload: None,
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

    /// Whether the tab may read its file again in place: its read ended, it
    /// reads at all, and no reload is in flight.
    pub(crate) fn can_reload(&self) -> bool {
        let ended = !matches!(
            self.load,
            ViewerLoad::Unread | ViewerLoad::Loading | ViewerLoad::NotAvailable
        );
        ended && self.reload.is_none()
    }

    /// Whether the file, stamped `now` (`None` when it cannot be statted),
    /// changed since the tab last read it, and the tab may read it again.
    pub(crate) fn needs_reload(&self, now: Option<FileStamp>) -> bool {
        self.can_reload() && now != self.last_stamp
    }

    /// Starts reading the file, stamped `now` by the stat that found it
    /// changed, again in place: returns the read's number, or `None` when
    /// the tab may not reload. The text shown stays the tab's read, so its
    /// colours and parse in flight still land, and it stays shown with the
    /// rendered document, the selection and find until the reload lands.
    pub(crate) fn begin_reload(&mut self, now: Option<FileStamp>) -> Option<u64> {
        if !self.can_reload() {
            return None;
        }
        let read = NEXT_READ.fetch_add(1, Ordering::Relaxed);
        self.reload = Some(Reload { read, stamp: now });
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
                self.reload = None;
                self.load = ViewerLoad::Unread;
            }
        }
    }

    /// Read `read` ended with `result`; returns whether it landed, which
    /// only the tab's read in flight does, an open's or a reload's.
    ///
    /// A reload's text becomes the tab's read. A reload that fails to read
    /// keeps the last read's lines under the Missing banner, still the
    /// tab's read; one that finds no text drops them for its banner and
    /// keeps the stamp it was begun with, so the same file is not read
    /// again.
    pub(crate) fn finish_read(
        &mut self,
        read: u64,
        result: Result<ViewerText, ReadFailure>,
    ) -> bool {
        let reload = self.reload.filter(|reload| reload.read == read);
        let open = self.load == ViewerLoad::Loading && self.read == Some(read);
        if reload.is_none() && !open {
            return false;
        }
        self.reload = None;
        self.last_stamp = match &result {
            Ok(text) => text.stamp,
            Err(ReadFailure::NotText(_)) => reload.and_then(|reload| reload.stamp),
            Err(ReadFailure::Unreadable) => None,
        };
        self.load = match result {
            Ok(text) => {
                self.read = Some(read);
                self.land_text(text, reload.is_some());
                ViewerLoad::Loaded
            }
            Err(ReadFailure::Unreadable) if reload.is_some() => ViewerLoad::Missing,
            Err(failure) => {
                self.drop_text();
                match failure {
                    ReadFailure::NotText(NotText::TooLarge) => ViewerLoad::TooLarge,
                    ReadFailure::NotText(NotText::Binary) => ViewerLoad::Binary,
                    ReadFailure::NotText(NotText::NotDecodable) => ViewerLoad::NotDecodable,
                    ReadFailure::Unreadable => ViewerLoad::Missing,
                }
            }
        };
        true
    }

    /// `text` landed: the selection is clamped to it and find re-runs on it.
    /// An open's text scrolls to the line target; a reload's leaves the
    /// scroll and recolours from scratch.
    fn land_text(&mut self, text: ViewerText, reload: bool) {
        self.apply_render_cap(text.text().len());
        self.selection.clamp(text.line_count());
        self.text = Some(Arc::new(text));
        if let Some(read) = &self.text
            && !self.find.query().is_empty()
        {
            self.find.rerun(read.lines());
        }
        if !reload {
            self.jump_pending.set(self.line_target.is_some());
            return;
        }
        self.highlight_cancel.store(true, Ordering::Relaxed);
        self.highlight_cancel = Arc::new(AtomicBool::new(false));
        self.spans = None;
        if self.markdown_too_large {
            self.md = None;
        }
    }

    /// No text shows: the lines, their colours, the rendered document and
    /// the selection go.
    fn drop_text(&mut self) {
        self.text = None;
        self.spans = None;
        self.md = None;
        self.selection.clear();
    }

    /// Whether the text read shows as numbered lines, where lines can be
    /// selected: a loaded file that is not a rendered Markdown document.
    fn shows_source(&self) -> bool {
        self.text.is_some() && self.markdown != Some(MarkdownMode::Rendered)
    }

    /// A left press on row `line`: it selects that line, or with `extend`
    /// (Shift held) runs the selection from the anchor to it, and a drag
    /// from here extends it.
    pub(crate) fn press_line(&mut self, line: usize, extend: bool) {
        if !self.shows_source() || line >= self.line_count() {
            return;
        }
        if extend {
            self.selection.extend(line);
        } else {
            self.selection.click(line);
        }
        self.selecting = true;
    }

    /// The pointer moved over row `line` with the left button `held`;
    /// returns whether the selection changed, which only a drag begun on a
    /// row does.
    pub(crate) fn drag_over_line(&mut self, line: usize, held: bool) -> bool {
        if !held {
            self.selecting = false;
        }
        if !self.selecting || line >= self.line_count() {
            return false;
        }
        let before = self.selection;
        self.selection.extend(line);
        self.selection != before
    }

    /// The left button was released: a drag ends.
    pub(crate) fn release_lines(&mut self) {
        self.selecting = false;
    }

    /// Selects every line; returns whether the Source view shows any.
    pub(crate) fn select_all_lines(&mut self) -> bool {
        if !self.shows_source() {
            return false;
        }
        self.selection.select_all(self.line_count());
        true
    }

    /// Clears the selection; returns whether a line was selected.
    pub(crate) fn clear_selection(&mut self) -> bool {
        let had = self.selection.range().is_some();
        self.selection.clear();
        had
    }

    /// The selected lines, counted from zero, end exclusive; `None` when
    /// none is.
    #[must_use]
    pub fn selected_lines(&self) -> Option<Range<usize>> {
        self.selection.range()
    }

    /// The find bar's count, "n of m", blank for an empty query; `None`
    /// while the bar does not show.
    #[must_use]
    pub fn find_label(&self) -> Option<String> {
        self.find_shows().then(|| self.find.label())
    }

    /// Whether the find bar shows: open, over the Source view.
    #[must_use]
    pub fn find_shows(&self) -> bool {
        self.find_open && self.shows_source()
    }

    /// Ctrl+F: the find bar opens; a rendered Markdown tab shows its source,
    /// where find works, its saved default left as it is. Returns whether
    /// the bar shows, which it never does on a tab with no text.
    pub(crate) fn open_find(&mut self) -> bool {
        if self.text.is_none() {
            return false;
        }
        if self.markdown == Some(MarkdownMode::Rendered) {
            self.markdown = Some(MarkdownMode::Source);
        }
        self.find_open = true;
        true
    }

    /// Closes the find bar; returns whether it was open.
    pub(crate) fn close_find(&mut self) -> bool {
        std::mem::replace(&mut self.find_open, false)
    }

    pub(crate) fn find(&self) -> &FindState {
        &self.find
    }

    pub(crate) fn find_bar(&self) -> Option<&FindBar> {
        self.find_bar.as_ref()
    }

    pub(crate) fn set_find_bar(&mut self, bar: FindBar) {
        self.find_bar = Some(bar);
    }

    /// The query became `query`, with line `top` the top one in view;
    /// returns the line of the match the bar is on now.
    pub(crate) fn set_find_query(&mut self, query: &str, top: usize) -> Option<usize> {
        if !self.find_shows() {
            return None;
        }
        let text = self.text.as_ref()?;
        self.find.set_query(query, text.lines(), top);
        self.find.current_match().map(|found| found.line)
    }

    /// Case counts, or stops counting, with line `top` the top one in view;
    /// returns the line of the match the bar is on now.
    pub(crate) fn toggle_find_case(&mut self, top: usize) -> Option<usize> {
        if !self.find_shows() {
            return None;
        }
        let text = self.text.as_ref()?;
        let case_sensitive = !self.find.case_sensitive();
        self.find
            .set_case_sensitive(case_sensitive, text.lines(), top);
        self.find.current_match().map(|found| found.line)
    }

    /// Moves the bar to the next match, or the previous one when not
    /// `forward`, wrapping; returns its line.
    pub(crate) fn step_find(&mut self, forward: bool) -> Option<usize> {
        if !self.find_shows() {
            return None;
        }
        let found = if forward {
            self.find.next()
        } else {
            self.find.prev()
        };
        found.map(|found| found.line)
    }

    /// The selected lines as the clipboard takes them; `None` when none is.
    pub(crate) fn selected_text(&self) -> Option<String> {
        self.selection.text(self.text.as_deref()?)
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
    /// is still the one shown, under the Missing banner too.
    pub(crate) fn finish_highlight(&mut self, read: u64, spans: Arc<Highlighted>) -> bool {
        if self.read != Some(read) || self.text.is_none() {
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
    #[must_use]
    pub fn scroll(&self) -> &UniformListScrollHandle {
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
        if mode == MarkdownMode::Rendered {
            self.selection.clear();
            self.selecting = false;
            self.find_open = false;
        }
        true
    }

    /// Whether the text shown, loaded or the last read under the Missing
    /// banner, is a Markdown document the rendered view can show.
    pub(crate) fn renders_markdown(&self) -> bool {
        self.markdown.is_some() && !self.markdown_too_large && self.text.is_some()
    }

    /// The parse of read `read` landed as `doc`; kept, with a fresh block
    /// list, while that read is still the one shown. A document that
    /// replaces another, a reload's, keeps its top row, clamped to the new
    /// rows.
    pub(crate) fn finish_markdown(&mut self, read: u64, doc: Arc<MdDoc>) -> bool {
        if self.read != Some(read) || !self.renders_markdown() {
            return false;
        }
        let list = ListState::new(doc.row_count(), ListAlignment::Top, px(MD_OVERDRAW));
        if let Some((_, old)) = &self.md {
            let top = old.logical_scroll_top();
            let last = doc.row_count().saturating_sub(1);
            list.scroll_to(if top.item_ix <= last {
                top
            } else {
                ListOffset {
                    item_ix: last,
                    offset_in_item: px(0.0),
                }
            });
        }
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
            ViewerLoad::Missing if self.text.is_some() => Some(MISSING_LAST_READ_BANNER),
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

    #[test]
    fn a_click_selects_one_line() {
        let mut selection = LineSelection::default();
        assert_eq!(selection.range(), None, "nothing is selected at first");
        selection.click(3);
        assert_eq!(selection.range(), Some(3..4));
        selection.click(1);
        assert_eq!(selection.range(), Some(1..2), "a second click starts over");
    }

    #[test]
    fn a_shift_click_extends_from_the_anchor_either_way() {
        let mut selection = LineSelection::default();
        selection.click(4);
        selection.extend(7);
        assert_eq!(selection.range(), Some(4..8), "down from the anchor");
        selection.extend(1);
        assert_eq!(
            selection.range(),
            Some(1..5),
            "up from the anchor, which stays put"
        );

        let mut fresh = LineSelection::default();
        fresh.extend(2);
        assert_eq!(
            fresh.range(),
            Some(2..3),
            "with no anchor the line becomes it"
        );
    }

    #[test]
    fn a_drag_extends_to_the_row_under_the_pointer() {
        let mut selection = LineSelection::default();
        selection.click(5);
        for row in [6, 7, 8] {
            selection.extend(row);
        }
        assert_eq!(selection.range(), Some(5..9));
        for row in [7, 6, 5, 4, 3] {
            selection.extend(row);
        }
        assert_eq!(
            selection.range(),
            Some(3..6),
            "dragging back past the anchor"
        );
        selection.extend(5);
        assert_eq!(selection.range(), Some(5..6), "back on the anchor's row");
    }

    #[test]
    fn select_all_covers_every_line() {
        let mut selection = LineSelection::default();
        selection.select_all(4);
        assert_eq!(selection.range(), Some(0..4));
        selection.select_all(0);
        assert_eq!(selection.range(), None, "an empty text has no line");
    }

    #[test]
    fn the_selection_clamps_when_the_text_shrinks() {
        let mut selection = LineSelection::default();
        selection.click(2);
        selection.extend(9);
        selection.clamp(5);
        assert_eq!(selection.range(), Some(2..5), "cut at the last line");
        selection.clamp(10);
        assert_eq!(selection.range(), Some(2..5), "growing adds nothing");

        let mut past = LineSelection::default();
        past.click(7);
        past.extend(8);
        past.clamp(3);
        assert_eq!(
            past.range(),
            Some(2..3),
            "wholly past the end: the last line"
        );
        past.clamp(0);
        assert_eq!(past.range(), None, "no line left");
    }

    #[test]
    fn the_selected_text_joins_lines_with_newlines_without_a_trailing_one() {
        let read = text("one\r\ntwo\nthree\rfour\n");
        let mut selection = LineSelection::default();
        assert_eq!(selection.text(&read), None, "nothing selected");
        selection.click(0);
        selection.extend(2);
        assert_eq!(selection.text(&read).as_deref(), Some("one\ntwo\nthree"));
        selection.select_all(read.line_count());
        assert_eq!(
            selection.text(&read).as_deref(),
            Some("one\ntwo\nthree\nfour")
        );
        selection.click(1);
        assert_eq!(selection.text(&read).as_deref(), Some("two"));
    }

    #[test]
    fn clear_empties_the_selection() {
        let mut selection = LineSelection::default();
        selection.click(1);
        selection.extend(3);
        selection.clear();
        assert_eq!(selection.range(), None);
        assert_eq!(selection.text(&text("a\nb\nc\nd\n")), None);
        selection.extend(2);
        assert_eq!(
            selection.range(),
            Some(2..3),
            "a cleared selection has no anchor"
        );
    }

    /// A plain text file, where lines select and find works.
    const PLAIN: &str = r"C:\notes\a.txt";

    /// A file modified `secs` seconds after the epoch, `len` bytes long.
    fn stamp(secs: u64, len: u64) -> FileStamp {
        FileStamp {
            modified: Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs)),
            len,
        }
    }

    /// A tab of [`PLAIN`] whose file read landed as `text_read`, stamped
    /// `at`.
    fn plain_at(text_read: &str, at: FileStamp) -> ViewerTab {
        let mut tab = ViewerTab::new(PLAIN.to_owned(), ViewerOrigin::Link, MarkdownMode::Rendered);
        let run = tab.begin_read().expect("an unread tab reads");
        assert!(tab.finish_read(run, Ok(text(text_read).with_stamp(at))));
        tab
    }

    #[test]
    fn an_unchanged_stamp_does_not_reload() {
        let mut tab = plain_at("a\n", stamp(10, 2));
        assert!(!tab.needs_reload(Some(stamp(10, 2))));
        let run = tab.begin_reload(None).expect("a loaded tab reloads");
        assert!(tab.finish_read(run, Ok(text("a\n").with_stamp(stamp(10, 2)))));
        assert!(
            !tab.needs_reload(Some(stamp(10, 2))),
            "a reload's stamp is the last read"
        );
    }

    #[test]
    fn a_changed_mtime_or_size_reloads() {
        let tab = plain_at("a\n", stamp(10, 2));
        assert!(tab.needs_reload(Some(stamp(11, 2))), "a newer mtime");
        assert!(tab.needs_reload(Some(stamp(9, 2))), "an older mtime");
        assert!(tab.needs_reload(Some(stamp(10, 3))), "another size");
        assert!(tab.needs_reload(None), "the file is gone");
    }

    #[test]
    fn an_unread_or_loading_tab_is_not_reloaded() {
        let mut tab = ViewerTab::new(PLAIN.to_owned(), ViewerOrigin::Link, MarkdownMode::Rendered);
        assert!(!tab.needs_reload(Some(stamp(10, 2))), "unread");
        assert!(!tab.needs_reload(None), "unread");
        assert_eq!(tab.begin_reload(None), None);
        assert_eq!(tab.load(), ViewerLoad::Unread);

        tab.begin_read().expect("an unread tab reads");
        assert!(!tab.needs_reload(Some(stamp(10, 2))), "loading");
        assert_eq!(tab.begin_reload(None), None);
        assert_eq!(tab.load(), ViewerLoad::Loading);
    }

    #[test]
    fn a_not_available_tab_is_never_reloaded() {
        let mut tab = ViewerTab::new(
            PLAIN.to_owned(),
            ViewerOrigin::NotAvailable,
            MarkdownMode::Rendered,
        );
        assert!(!tab.needs_reload(Some(stamp(10, 2))));
        assert!(!tab.needs_reload(None));
        assert_eq!(tab.begin_reload(None), None);
        assert_eq!(tab.load(), ViewerLoad::NotAvailable);
    }

    #[test]
    fn a_failed_reload_keeps_the_lines_under_the_last_read_banner() {
        let mut tab = plain_at("a\nb\nc\n", stamp(10, 6));
        tab.press_line(1, false);
        tab.release_lines();
        let run = tab.begin_reload(None).expect("a loaded tab reloads");
        assert!(!tab.needs_reload(None), "not while a reload is in flight");
        assert_eq!(tab.begin_reload(None), None, "one reload at a time");
        assert_eq!(tab.line_count(), 3, "the old lines show while it reads");
        assert_eq!(tab.load(), ViewerLoad::Loaded);

        assert!(tab.finish_read(run, Err(ReadFailure::Unreadable)));
        assert_eq!(tab.load(), ViewerLoad::Missing);
        assert_eq!(tab.note(), Some(MISSING_LAST_READ_BANNER));
        assert_eq!(
            MISSING_LAST_READ_BANNER,
            "The file is gone or cannot be read. Showing the last read."
        );
        assert_eq!(tab.line_count(), 3, "the last read stays");
        assert_eq!(tab.selected_lines(), Some(1..2), "and its selection");
        assert!(!tab.needs_reload(None), "still gone: nothing to read");
        assert!(
            tab.needs_reload(Some(stamp(10, 6))),
            "back, even as it was: read it"
        );
    }

    #[test]
    fn a_reload_past_the_cap_replaces_the_lines_with_the_banner() {
        let mut tab = plain_at("a\nb\n", stamp(10, 4));
        tab.press_line(0, false);
        tab.release_lines();
        let run = tab.begin_reload(None).expect("a loaded tab reloads");
        assert!(tab.finish_read(run, Err(ReadFailure::NotText(NotText::TooLarge))));
        assert_eq!(tab.load(), ViewerLoad::TooLarge);
        assert_eq!(tab.note(), Some(TOO_LARGE_BANNER));
        assert_eq!(tab.line_count(), 0, "no stale lines under the banner");
        assert!(tab.text().is_none());
        assert_eq!(tab.selected_lines(), None);
    }

    #[test]
    fn a_reload_that_lands_clamps_the_selection_and_reruns_find() {
        let mut tab = plain_at("a\nneedle\nc\nneedle\n", stamp(10, 18));
        assert!(tab.open_find());
        assert_eq!(tab.set_find_query("needle", 0), Some(1));
        assert_eq!(tab.find_label().as_deref(), Some("1 of 2"));
        tab.press_line(3, false);
        tab.release_lines();

        let run = tab.begin_reload(None).expect("a loaded tab reloads");
        assert!(tab.finish_read(run, Ok(text("needle\nb\n").with_stamp(stamp(11, 9)))));
        assert_eq!(tab.load(), ViewerLoad::Loaded);
        assert_eq!(tab.line_count(), 2);
        assert_eq!(
            tab.selected_lines(),
            Some(1..2),
            "clamped onto the last line"
        );
        assert_eq!(
            tab.find_label().as_deref(),
            Some("1 of 1"),
            "the query kept"
        );
        assert!(
            !tab.needs_reload(Some(stamp(11, 9))),
            "the new stamp is the last read"
        );
    }

    #[test]
    fn a_reload_does_not_jump_to_the_line_target() {
        let mut tab = ViewerTab::new(PLAIN.to_owned(), ViewerOrigin::Link, MarkdownMode::Rendered);
        tab.set_line_target(Some(2));
        let run = tab.begin_read().expect("an unread tab reads");
        assert!(tab.finish_read(run, Ok(text("a\nb\nc\n").with_stamp(stamp(10, 6)))));
        assert_eq!(tab.take_jump(), Some(1), "the open scrolls to its line");

        let run = tab.begin_reload(None).expect("a loaded tab reloads");
        assert!(tab.finish_read(run, Ok(text("a\nb\nc\nd\n").with_stamp(stamp(11, 8)))));
        assert_eq!(tab.take_jump(), None, "a reload leaves the scroll");
        assert_eq!(tab.line_target(), Some(2));
    }

    #[test]
    fn a_stale_reload_result_is_dropped() {
        let mut tab = plain_at("a\n", stamp(10, 2));
        let overtaken = tab.begin_reload(None).expect("a loaded tab reloads");
        tab.reread();
        let fresh = tab.begin_read().expect("the re-open reads");
        assert!(
            !tab.finish_read(overtaken, Ok(text("old\nold\n"))),
            "the reload a re-open overtook"
        );
        assert_eq!(tab.load(), ViewerLoad::Loading);
        assert!(tab.finish_read(fresh, Ok(text("new\n").with_stamp(stamp(12, 4)))));
        assert_eq!(tab.line_count(), 1);

        let run = tab.begin_reload(None).expect("a loaded tab reloads");
        assert!(tab.finish_read(run, Ok(text("x\ny\n").with_stamp(stamp(13, 4)))));
        assert!(
            !tab.finish_read(run, Ok(text("x\ny\nz\n"))),
            "a reload lands once"
        );
        assert_eq!(tab.line_count(), 2);
    }

    #[test]
    fn a_missing_file_that_returns_reloads() {
        let mut tab = ViewerTab::new(PLAIN.to_owned(), ViewerOrigin::Link, MarkdownMode::Rendered);
        let run = tab.begin_read().expect("an unread tab reads");
        assert!(tab.finish_read(run, Err(ReadFailure::Unreadable)));
        assert_eq!(
            tab.note(),
            Some(MISSING_BANNER),
            "nothing was read: no last read to show"
        );
        assert!(!tab.needs_reload(None), "still gone");
        assert!(tab.needs_reload(Some(stamp(10, 5))), "it came back");

        let run = tab.begin_reload(None).expect("a missing tab reloads");
        assert!(tab.finish_read(run, Ok(text("back\n").with_stamp(stamp(10, 5)))));
        assert_eq!(tab.load(), ViewerLoad::Loaded);
        assert_eq!(tab.note(), None);
        assert_eq!(tab.line_count(), 1);
    }

    #[test]
    fn a_binary_file_is_not_read_again_while_unchanged() {
        let mut tab = plain_at("a\n", stamp(10, 2));
        let binary = stamp(11, 9);
        assert!(tab.needs_reload(Some(binary)));
        let run = tab
            .begin_reload(Some(binary))
            .expect("a loaded tab reloads");
        assert!(tab.finish_read(run, Err(ReadFailure::NotText(NotText::Binary))));
        assert_eq!(tab.load(), ViewerLoad::Binary);
        assert!(
            !tab.needs_reload(Some(binary)),
            "the same binary file is not read again"
        );
        assert!(tab.needs_reload(Some(stamp(12, 9))), "a change is read");
        assert!(tab.needs_reload(None), "and so is its going");
    }

    #[test]
    fn a_failed_reload_keeps_the_old_texts_colours_landing() {
        let mut tab = ViewerTab::new(PLAIN.to_owned(), ViewerOrigin::Link, MarkdownMode::Rendered);
        let shown = tab.begin_read().expect("an unread tab reads");
        assert!(tab.finish_read(shown, Ok(text("a\nb\n").with_stamp(stamp(10, 4)))));
        let reload = tab.begin_reload(None).expect("a loaded tab reloads");
        assert!(tab.finish_read(reload, Err(ReadFailure::Unreadable)));
        assert!(
            tab.finish_highlight(shown, Arc::new(Vec::new())),
            "the shown text's colours land after the failed reload"
        );
        assert!(tab.spans().is_some());

        let mut md = ViewerTab::new(NOTES.to_owned(), ViewerOrigin::Link, MarkdownMode::Rendered);
        let shown = md.begin_read().expect("an unread tab reads");
        assert!(md.finish_read(shown, Ok(text("# A\n").with_stamp(stamp(10, 4)))));
        let reload = md.begin_reload(None).expect("a loaded tab reloads");
        assert!(md.finish_read(reload, Err(ReadFailure::Unreadable)));
        assert!(
            md.finish_markdown(shown, md_doc("# A\n")),
            "the shown text's parse lands after the failed reload"
        );
        assert!(md.markdown_doc().is_some(), "the rendered tab is not blank");
    }

    /// The rendered document of `text`.
    fn md_doc(text: &str) -> Arc<MdDoc> {
        Arc::new(crate::viewer_md_view::prepare(
            text,
            Path::new(r"C:\notes"),
            &AtomicBool::new(false),
        ))
    }

    #[test]
    fn a_rendered_reload_keeps_the_scroll_made_during_the_reload() {
        let doc = "# A\n\npara\n\n".repeat(20);
        let mut tab = ViewerTab::new(NOTES.to_owned(), ViewerOrigin::Link, MarkdownMode::Rendered);
        let run = tab.begin_read().expect("an unread tab reads");
        assert!(tab.finish_read(run, Ok(text(&doc).with_stamp(stamp(10, 4)))));
        assert!(tab.finish_markdown(run, md_doc(&doc)));

        let longer = [doc.as_str(), "more\n"].concat();
        let reload = tab.begin_reload(None).expect("a loaded tab reloads");
        let top = ListOffset {
            item_ix: 7,
            offset_in_item: px(0.0),
        };
        tab.markdown_doc()
            .expect("the old document shows during the reload")
            .1
            .scroll_to(top);
        assert!(tab.finish_read(reload, Ok(text(&longer).with_stamp(stamp(11, 5)))));
        assert!(tab.finish_markdown(reload, md_doc(&longer)));
        let (_, list) = tab.markdown_doc().expect("the new document");
        assert_eq!(
            list.logical_scroll_top().item_ix,
            7,
            "the scroll made while it read stays"
        );
    }
}
