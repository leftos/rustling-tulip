//! Viewer tabs on the root view: opening one, building their state from the
//! tab list and reading their files, the daemon's answers, the keyboard,
//! and the body: a header with the file's name, its folder and the open and
//! copy buttons, over the file's numbered, coloured lines or the banner that
//! says why none show. The state is [`crate::viewer_tab`].

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use gpui::{
    AnyElement, App, ClickEvent, Context, Div, ElementId, Entity, FocusHandle, Focusable,
    HighlightStyle, Hsla, KeyDownEvent, Keystroke, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ScrollStrategy, SharedString, Stateful, StyledText, Subscription,
    UniformListScrollHandle, WeakEntity, Window, div, prelude::*, px, relative, uniform_list,
};
use protocol::{ClientMessage, DaemonMessage, TabContent};

use crate::buttons::{ButtonSize, Layer, outlined_button, primary_button};
use crate::diff_tab::split_path;
use crate::ellipsis::ellipsized;
use crate::fonts;
use crate::notices::ToastKind;
use crate::open::OpenJob;
use crate::open_view::{Origin, Route, Then};
use crate::remote_mode::LOCAL_FILES_REASON;
use crate::syntax::{self, Highlighted, TokenClass};
use crate::target::HostKey;
use crate::text_input::{TextChanged, TextInput, TextInputEvent};
use crate::ui_theme::{UiTheme, theme};
use crate::viewer_find::{self, Match, Segment, Tint};
use crate::viewer_md_view::markdown_toggle;
use crate::viewer_tab::{
    self, Activation, FindBar, MarkdownMode, OPEN_FAILED_TITLE, ReadFailure, ViewerOrigin,
    ViewerTab, ViewerText,
};
use crate::{RootView, UI_TEXT_SIZE, tooltip};

/// The header's height, the diff tab's.
const HEADER_HEIGHT: f32 = 42.0;
const HEADER_PAD_LEFT: f32 = 14.0;
const HEADER_PAD_RIGHT: f32 = 10.0;
/// The room between the file name and its folder, in px.
const PATH_GAP: f32 = 6.0;
/// The gutter's line numbers' size and the room right of them, the diff
/// view's.
const GUTTER_TEXT_SIZE: f32 = 11.0;
const GUTTER_PAD: f32 = 12.0;
/// A digit's width per pixel of text size, the diff view's guess before it
/// measures.
const DIGIT_WIDTH_RATIO: f32 = 0.6;
/// A line's height per pixel of the font size.
const LINE_HEIGHT_RATIO: f32 = 1.4;
/// The room between the text and the gutter, in px.
const TEXT_PAD_LEFT: f32 = 8.0;
/// The header's file name and folder, as their ellipsis probe ids.
const PATH_NAME: &str = "viewer-path-name";
const PATH_FOLDER: &str = "viewer-path-folder";
/// A find match's tint: the accent, lighter than a selected row's
/// [`UiTheme::input_selection`], so a match on a selected row still shows.
fn find_match_bg(t: &UiTheme) -> u32 {
    (t.accent << 8) | 0x40
}
/// The tint of the match the find bar is on: the accent, stronger.
fn find_current_bg(t: &UiTheme) -> u32 {
    (t.accent << 8) | 0xa6
}
/// The find bar's height, its query field's width and the room its count
/// keeps, in px.
const FIND_BAR_HEIGHT: f32 = 38.0;
const FIND_FIELD_WIDTH: f32 = 260.0;
const FIND_COUNT_WIDTH: f32 = 64.0;
const FIND_PLACEHOLDER: &str = "Find";

/// A find bar button's action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FindAction {
    Prev,
    Next,
    MatchCase,
}

impl FindAction {
    const ALL: [Self; 3] = [Self::Prev, Self::Next, Self::MatchCase];

    fn selector(self, tab_id: &str) -> String {
        let name = match self {
            Self::Prev => "prev",
            Self::Next => "next",
            Self::MatchCase => "case",
        };
        format!("viewer-find-{name}-{tab_id}")
    }

    fn label(self) -> &'static str {
        match self {
            Self::Prev => "Previous",
            Self::Next => "Next",
            Self::MatchCase => "Match case",
        }
    }

    fn tooltip(self) -> &'static str {
        match self {
            Self::Prev => "Previous match (Shift+Enter)",
            Self::Next => "Next match (Enter)",
            Self::MatchCase => "Match case",
        }
    }
}

/// A header button's action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeaderAction {
    OpenDefault,
    Reveal,
    CopyPath,
    CopyAll,
}

impl HeaderAction {
    const ALL: [Self; 4] = [
        Self::OpenDefault,
        Self::Reveal,
        Self::CopyPath,
        Self::CopyAll,
    ];

    fn selector(self, tab_id: &str) -> String {
        let name = match self {
            Self::OpenDefault => "open-default",
            Self::Reveal => "reveal",
            Self::CopyPath => "copy-path",
            Self::CopyAll => "copy-all",
        };
        format!("viewer-{name}-{tab_id}")
    }

    fn label(self) -> &'static str {
        match self {
            Self::OpenDefault => "Open in default app",
            Self::Reveal => "Reveal",
            Self::CopyPath => "Copy path",
            Self::CopyAll => "Copy all",
        }
    }
}

/// What one frame of a viewer tab's line list draws from.
struct Rows {
    tab_id: String,
    text: Arc<ViewerText>,
    spans: Option<Arc<Highlighted>>,
    /// The row the line target names, drawn highlighted.
    target: Option<usize>,
    /// The rows selected, drawn tinted.
    selected: Option<Range<usize>>,
    /// The find matches, sorted by line, while the find bar shows.
    matches: Option<Arc<[Match]>>,
    /// The match the find bar is on.
    current: Option<Match>,
    line_height: f32,
    gutter_width: f32,
    colors: [Hsla; 6],
    /// The theme the rows draw in.
    theme: UiTheme,
    /// The root view, which a press or a drag on a row updates.
    root: WeakEntity<RootView>,
    /// The viewer body's focus, which a press on a row takes.
    focus: FocusHandle,
}

impl Rows {
    /// Rows `range`: each a gutter with its line number, then its text.
    fn render(&self, range: Range<usize>) -> Vec<Div> {
        range
            .filter(|index| *index < self.text.line_count())
            .map(|index| self.row(index))
            .collect()
    }

    fn row(&self, index: usize) -> Div {
        let t = &self.theme;
        let number = index + 1;
        let selector = format!("viewer-line-{}-{number}", self.tab_id);
        let text_selector = format!("viewer-text-{}-{number}", self.tab_id);
        let selected = self
            .selected
            .as_ref()
            .is_some_and(|range| range.contains(&index));
        let line = self.text.line(index);
        let fits = |span: &Range<usize>| {
            span.end <= line.len()
                && line.is_char_boundary(span.start)
                && line.is_char_boundary(span.end)
        };
        let syntax: Vec<(Range<usize>, TokenClass)> = self
            .spans
            .as_ref()
            .and_then(|spans| spans.get(index))
            .map(|spans| {
                spans
                    .iter()
                    .filter(|(span, _)| fits(span))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let found: Vec<Range<usize>> = self
            .matches
            .as_deref()
            .map(|matches| {
                viewer_find::on_line(matches, index)
                    .iter()
                    .map(|found| found.range.clone())
                    .filter(fits)
                    .collect()
            })
            .unwrap_or_default();
        let current = self
            .current
            .as_ref()
            .filter(|found| found.line == index)
            .map(|found| found.range.clone())
            .filter(fits);
        let highlights: Vec<(Range<usize>, HighlightStyle)> =
            if found.is_empty() && current.is_none() {
                syntax
                    .into_iter()
                    .filter_map(|(range, class)| {
                        self.segment_style(&Segment {
                            range,
                            syntax: Some(class),
                            tint: Tint::None,
                        })
                    })
                    .collect()
            } else {
                viewer_find::merge_segments(line.len(), &syntax, &found, current.as_ref())
                    .iter()
                    .filter_map(|segment| self.segment_style(segment))
                    .collect()
            };
        // A hovered line lifts its quiet number to secondary text, which
        // keeps 4.5:1 on the hover fill.
        let hovered = self.target == Some(index) && !selected;
        div()
            .w_full()
            .h(px(self.line_height))
            .flex()
            .flex_row()
            .items_center()
            .when(hovered, |row| row.bg(gpui::rgb(t.hover_bg)))
            .when(selected, |row| row.bg(gpui::rgba(t.input_selection)))
            .on_mouse_down(MouseButton::Left, self.on_press(index))
            .child(
                div()
                    .debug_selector(move || selector)
                    .flex_none()
                    .w(px(self.gutter_width))
                    .pr(px(GUTTER_PAD))
                    .text_right()
                    .text_size(px(GUTTER_TEXT_SIZE))
                    .text_color(gpui::rgb(if hovered { t.muted } else { t.subtle }))
                    .child(number.to_string()),
            )
            .child(
                div()
                    .debug_selector(move || text_selector)
                    .flex_1()
                    .min_w(px(0.0))
                    .pl(px(TEXT_PAD_LEFT))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(StyledText::new(line.to_owned()).with_highlights(highlights)),
            )
    }

    /// A left press on row `index`: the body takes the keyboard and the row
    /// is selected, or with Shift the run from the anchor to it.
    fn on_press(&self, index: usize) -> impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static {
        let (root, focus, tab_id) = (self.root.clone(), self.focus.clone(), self.tab_id.clone());
        move |event, window, cx| {
            focus.focus(window);
            // Fails only when the view is gone, and the tab with it.
            root.update(cx, |root, cx| {
                root.press_viewer_line(&tab_id, index, event.modifiers.shift, cx);
            })
            .ok();
        }
    }

    /// How a segment of a line is drawn: its syntax colour and its find
    /// tint; `None` for a plain one.
    fn segment_style(
        &self,
        segment: &Segment<TokenClass>,
    ) -> Option<(Range<usize>, HighlightStyle)> {
        let background = match segment.tint {
            Tint::None => None,
            Tint::Match => Some(find_match_bg(&self.theme)),
            Tint::Current => Some(find_current_bg(&self.theme)),
        };
        if segment.syntax.is_none() && background.is_none() {
            return None;
        }
        let style = HighlightStyle {
            color: segment.syntax.map(|class| self.colors[class.index()]),
            background_color: background.map(|tint| Hsla::from(gpui::rgba(tint))),
            ..HighlightStyle::default()
        };
        Some((segment.range.clone(), style))
    }
}

impl RootView {
    /// Asks the daemon to open, or focus, a viewer tab of `path`, a file of
    /// this PC; the tab it names is activated and its target moves to
    /// `line`. The path is trimmed and made absolute first; an empty one
    /// sends nothing.
    pub fn open_viewer(&mut self, path: &str, line: Option<u32>) {
        let normalised = match viewer_tab::normalise_path(path) {
            Ok(normalised) => normalised,
            Err(err) => {
                tracing::warn!("no viewer tab opened for {path:?}: {err}");
                return;
            }
        };
        let id = self.viewer_opens.open_id(line);
        self.send_to(
            &HostKey::Local,
            ClientMessage::OpenViewerTab {
                id,
                path: normalised,
            },
        );
    }

    /// Gives every viewer tab its state, deriving where its file comes from,
    /// drops the state of tabs that are gone, and starts reading the file of
    /// every tab not read yet.
    pub(crate) fn reconcile_viewer_tabs(&mut self, cx: &mut Context<Self>) {
        let paths: Vec<(String, String)> = self
            .tabs
            .tabs()
            .iter()
            .filter_map(|tab| match &tab.content {
                TabContent::Viewer { path } => Some((tab.id.clone(), path.clone())),
                TabContent::Grid { .. }
                | TabContent::Diff { .. }
                | TabContent::Dashboard
                | TabContent::Unknown => None,
            })
            .collect();
        self.viewer_tabs
            .retain(|tab_id, _| paths.iter().any(|(id, _)| id == tab_id));
        let opens_local_files = self.capabilities().opens_local_files();
        let download_root = self.remote_files_root.as_ref().ok().map(PathBuf::as_path);
        let markdown_default = if self.sidebar.ui_state().viewer_markdown_rendered {
            MarkdownMode::Rendered
        } else {
            MarkdownMode::Source
        };
        for (tab_id, path) in paths {
            self.viewer_tabs.entry(tab_id).or_insert_with(|| {
                let origin =
                    viewer_tab::derive_origin(Path::new(&path), download_root, opens_local_files);
                ViewerTab::new(path, origin, markdown_default)
            });
        }
        let reads: Vec<(String, PathBuf, u64)> = self
            .viewer_tabs
            .iter_mut()
            .filter_map(|(tab_id, tab)| {
                let read = tab.begin_read()?;
                Some((tab_id.clone(), PathBuf::from(tab.path()), read))
            })
            .collect();
        for (tab_id, path, read) in reads {
            Self::read_viewer_file(tab_id, path, read, cx);
        }
    }

    /// Reads tab `tab_id`'s file at `path` on the background executor as
    /// its read `read`.
    fn read_viewer_file(tab_id: String, path: PathBuf, read: u64, cx: &mut Context<Self>) {
        let work = cx
            .background_executor()
            .spawn(async move { viewer_tab::read_file(&path) });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            // Fails only when the view is gone, and the tab with it.
            this.update(cx, |root, cx| {
                root.finish_viewer_read(&tab_id, read, result, cx);
            })
            .ok();
        })
        .detach();
    }

    /// The active tab became a viewer tab: its file is statted on the
    /// background executor and, when its modified time or size changed
    /// since the last read, read again in place.
    pub(crate) fn reload_active_viewer(&mut self, cx: &mut Context<Self>) {
        let Some(tab_id) = self.tabs.active_id().map(str::to_owned) else {
            return;
        };
        let Some(tab) = self.viewer_tabs.get(&tab_id) else {
            return;
        };
        if !tab.can_reload() {
            return;
        }
        let path = PathBuf::from(tab.path());
        let stat_path = path.clone();
        let work = cx
            .background_executor()
            .spawn(async move { viewer_tab::file_stamp(&stat_path) });
        cx.spawn(async move |this, cx| {
            let now = work.await;
            // Fails only when the view is gone, and the tab with it.
            this.update(cx, |root, cx| {
                let read = root
                    .viewer_tabs
                    .get_mut(&tab_id)
                    .filter(|tab| tab.needs_reload(now))
                    .and_then(|tab| tab.begin_reload(now));
                if let Some(read) = read {
                    Self::read_viewer_file(tab_id, path, read, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn finish_viewer_read(
        &mut self,
        tab_id: &str,
        read: u64,
        result: Result<ViewerText, ReadFailure>,
        cx: &mut Context<Self>,
    ) {
        let landed = self
            .viewer_tabs
            .get_mut(tab_id)
            .is_some_and(|tab| tab.finish_read(read, result));
        if !landed {
            tracing::debug!(tab_id, read, "a viewer read landed for a tab that moved on");
            return;
        }
        self.highlight_viewer(tab_id, read, cx);
        self.parse_viewer_markdown(tab_id, read, cx);
        cx.notify();
    }

    /// Colours tab `tab_id`'s text, read `read`, on the background executor
    /// when its language has a grammar; past the highlighter's limits the
    /// text stays uncoloured.
    fn highlight_viewer(&mut self, tab_id: &str, read: u64, cx: &mut Context<Self>) {
        let Some(tab) = self.viewer_tabs.get(tab_id) else {
            return;
        };
        let Some(text) = tab.text() else {
            return;
        };
        let Some(language) = text.language().filter(|id| syntax::has_grammar(id)) else {
            return;
        };
        let (text, cancel) = (Arc::clone(text), tab.highlight_cancel());
        let work = cx.background_executor().spawn(async move {
            let grammar = syntax::syntax_for(language)?;
            let deadline = Instant::now() + syntax::TIME_LIMIT;
            let joined = text.joined();
            match syntax::highlight(&joined, grammar, syntax::syntax_set(), deadline, &cancel) {
                Ok(spans) => Some(Arc::new(spans)),
                Err(reason) => {
                    tracing::debug!(language, ?reason, "viewer text left uncoloured");
                    None
                }
            }
        });
        let tab_id = tab_id.to_owned();
        cx.spawn(async move |this, cx| {
            let Some(spans) = work.await else {
                return;
            };
            // Fails only when the view is gone, and the tab with it.
            this.update(cx, |root, cx| {
                let kept = root
                    .viewer_tabs
                    .get_mut(&tab_id)
                    .is_some_and(|tab| tab.finish_highlight(read, spans));
                if kept {
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Folds a viewer message in; returns whether nothing else is owed it.
    /// A viewer tab holds a file on this PC — a remote file reaches it as a
    /// fetched copy — so it lives in This PC's layout and asks only This PC:
    /// a remote host's open answer or error names nothing of theirs.
    pub(crate) fn on_viewer_message(
        &mut self,
        host: &crate::target::HostKey,
        msg: &DaemonMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if *host != HostKey::Local {
            return false;
        }
        match msg {
            DaemonMessage::Welcome { .. } => {
                self.viewer_opens.reset();
                false
            }
            DaemonMessage::ViewerTabOpened { id, tab_id } => {
                let known = self.tabs.tab(tab_id).is_some();
                if let Some(activation) = self.viewer_opens.opened(id, tab_id, known, (self.now)())
                {
                    // Re-opening the active tab focuses it again too.
                    self.last_active_tab = None;
                    self.activate_viewer(&activation);
                    self.return_stranded_find_focus(&activation.tab_id, window, cx);
                    self.after_tabs_change(window, cx);
                }
                true
            }
            DaemonMessage::Error {
                message,
                request_id: Some(id),
                ..
            } if self.viewer_opens.failed(id) => {
                tracing::warn!("the daemon could not open a viewer tab: {message}");
                self.push_toast(
                    ToastKind::Error,
                    OPEN_FAILED_TITLE,
                    Some(message.clone()),
                    cx,
                );
                true
            }
            _ => false,
        }
    }

    /// Activates the tab an open named and moves its target line; a tab
    /// whose read ended reads its file again, so the open shows the file as
    /// it is now, and the line target applies once that read lands.
    fn activate_viewer(&mut self, activation: &Activation) {
        if let Some(tab) = self.viewer_tabs.get_mut(&activation.tab_id) {
            tab.reread();
            tab.set_line_target(activation.line);
        }
        self.tabs.activate(&activation.tab_id);
    }

    /// Activates the tab an `OpenViewerTab` named before the tab list held
    /// it, once it does.
    pub(crate) fn activate_pending_viewer_tab(&mut self) {
        let now = (self.now)();
        let tabs = &self.tabs;
        let ready = self
            .viewer_opens
            .take_ready(|tab_id| tabs.tab(tab_id).is_some(), now);
        if let Some(activation) = ready {
            self.activate_viewer(&activation);
        }
    }

    /// Gives the keyboard to the active tab when it is a viewer tab; returns
    /// whether it was.
    pub(crate) fn focus_active_viewer_tab(&self, window: &mut Window) -> bool {
        let active_viewer = self
            .tabs
            .active_id()
            .is_some_and(|id| self.viewer_tabs.contains_key(id));
        if active_viewer {
            self.viewer_focus.focus(window);
        }
        active_viewer
    }

    /// Viewer tab `tab_id`'s state.
    #[must_use]
    pub fn viewer_tab(&self, tab_id: &str) -> Option<&ViewerTab> {
        self.viewer_tabs.get(tab_id)
    }

    /// Whether viewer tab `tab_id` is the tab shown and holds the keyboard.
    #[must_use]
    pub fn viewer_tab_focused(&self, tab_id: &str, window: &Window) -> bool {
        self.tabs.active_id() == Some(tab_id)
            && self.viewer_tabs.contains_key(tab_id)
            && self.viewer_focus.is_focused(window)
    }

    /// Whether viewer tab `tab_id`'s find field holds the keyboard.
    #[must_use]
    pub fn viewer_find_focused(&self, tab_id: &str, window: &Window, cx: &App) -> bool {
        self.viewer_tabs
            .get(tab_id)
            .and_then(ViewerTab::find_bar)
            .is_some_and(|bar| bar.input.read(cx).focus_handle(cx).is_focused(window))
    }

    /// Gives the keyboard back to tab `tab_id` when its find field holds it
    /// but the bar no longer shows.
    pub(crate) fn return_stranded_find_focus(&self, tab_id: &str, window: &mut Window, cx: &App) {
        let stranded = self.viewer_tabs.get(tab_id).is_some_and(|tab| {
            !tab.find_shows()
                && tab
                    .find_bar()
                    .is_some_and(|bar| bar.input.read(cx).focus_handle(cx).is_focused(window))
        });
        if stranded {
            self.viewer_focus.focus(window);
        }
    }

    /// Ctrl+F on tab `tab_id`: the find bar shows, made the first time, and
    /// its field takes the keyboard; a rendered Markdown tab shows its
    /// source first. A tab with no text has no find.
    fn open_viewer_find(&mut self, tab_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.viewer_tabs.get_mut(tab_id) else {
            return;
        };
        if !tab.open_find() {
            return;
        }
        if tab.find_bar().is_none() {
            let input = cx.new(|cx| TextInput::new(String::new(), FIND_PLACEHOLDER, cx));
            let subscriptions = Self::watch_viewer_find(tab_id, &input, window, cx);
            tab.set_find_bar(FindBar::new(input, subscriptions));
        }
        if let Some(bar) = tab.find_bar() {
            bar.input.read(cx).focus_handle(cx).focus(window);
        }
        cx.notify();
    }

    /// Enter in the find field goes to the next match, Esc closes the bar,
    /// and every edit searches again.
    fn watch_viewer_find(
        tab_id: &str,
        input: &Entity<TextInput>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Subscription> {
        let (key_tab, edit_tab) = (tab_id.to_owned(), tab_id.to_owned());
        let keys = cx.subscribe_in(
            input,
            window,
            move |this, _, event: &TextInputEvent, window, cx| match event {
                TextInputEvent::Submit => this.step_viewer_find(&key_tab, true, cx),
                TextInputEvent::Cancel => this.close_viewer_find(&key_tab, window, cx),
            },
        );
        let edits = cx.subscribe_in(input, window, move |this, input, _: &TextChanged, _, cx| {
            let query = input.read(cx).text().to_owned();
            this.search_viewer(&edit_tab, &query, cx);
        });
        vec![keys, edits]
    }

    /// The find query of tab `tab_id` became `query`: the bar goes to the
    /// first match at or after the top line in view, scrolled to.
    fn search_viewer(&mut self, tab_id: &str, query: &str, cx: &mut Context<Self>) {
        let row_height = self.viewer_line_height();
        let Some(tab) = self.viewer_tabs.get_mut(tab_id) else {
            return;
        };
        let line = tab.set_find_query(query, top_line(tab.scroll(), row_height));
        scroll_to_match(tab, line, row_height);
        cx.notify();
    }

    /// The case toggle of tab `tab_id`'s find bar was pressed.
    fn toggle_viewer_find_case(&mut self, tab_id: &str, cx: &mut Context<Self>) {
        let row_height = self.viewer_line_height();
        let Some(tab) = self.viewer_tabs.get_mut(tab_id) else {
            return;
        };
        let line = tab.toggle_find_case(top_line(tab.scroll(), row_height));
        scroll_to_match(tab, line, row_height);
        cx.notify();
    }

    /// Tab `tab_id`'s find bar goes to the next match, or the previous one
    /// when not `forward`, scrolled to.
    fn step_viewer_find(&mut self, tab_id: &str, forward: bool, cx: &mut Context<Self>) {
        let row_height = self.viewer_line_height();
        let Some(tab) = self.viewer_tabs.get_mut(tab_id) else {
            return;
        };
        let line = tab.step_find(forward);
        scroll_to_match(tab, line, row_height);
        cx.notify();
    }

    /// Closes tab `tab_id`'s find bar and gives the keyboard back to the
    /// tab.
    fn close_viewer_find(&mut self, tab_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .viewer_tabs
            .get_mut(tab_id)
            .is_some_and(ViewerTab::close_find)
        {
            self.viewer_focus.focus(window);
            cx.notify();
        }
    }

    /// A find bar button of tab `tab_id` was pressed.
    fn press_viewer_find_button(
        &mut self,
        tab_id: &str,
        action: FindAction,
        cx: &mut Context<Self>,
    ) {
        match action {
            FindAction::Prev => self.step_viewer_find(tab_id, false, cx),
            FindAction::Next => self.step_viewer_find(tab_id, true, cx),
            FindAction::MatchCase => self.toggle_viewer_find_case(tab_id, cx),
        }
    }

    /// The find keys on tab `tab_id`: Ctrl+F opens the bar or gives it the
    /// keyboard again; while it is open F3 and Enter go to the next match,
    /// with Shift the previous one (a plain Enter in the field comes as its
    /// submit). Returns whether the key was find's.
    fn on_viewer_find_key(
        &mut self,
        tab_id: &str,
        ks: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let mods = ks.modifiers;
        let ctrl_only = mods.control && !mods.shift && !mods.alt && !mods.platform;
        let shift_or_none = !mods.control && !mods.alt && !mods.platform;
        let open = self
            .viewer_tabs
            .get(tab_id)
            .is_some_and(ViewerTab::find_shows);
        let body_has_keys = self.viewer_focus.is_focused(window);
        match ks.key.as_str() {
            "f" if ctrl_only => self.open_viewer_find(tab_id, window, cx),
            "f3" if open && shift_or_none => self.step_viewer_find(tab_id, !mods.shift, cx),
            "enter" if open && shift_or_none && (mods.shift || body_has_keys) => {
                self.step_viewer_find(tab_id, !mods.shift, cx);
            }
            _ => return false,
        }
        true
    }

    /// A left press on line `line` of tab `tab_id`, with Shift when
    /// `extend`.
    fn press_viewer_line(
        &mut self,
        tab_id: &str,
        line: usize,
        extend: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(tab) = self.viewer_tabs.get_mut(tab_id) {
            tab.press_line(line, extend);
            cx.notify();
        }
    }

    /// The pointer moved over tab `tab_id`'s body: a drag begun on a row
    /// extends the selection to the row level with the pointer, kept to the
    /// rows in view, so a drag past the list's edge stops at its edge row.
    fn drag_viewer_to(&mut self, tab_id: &str, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        let row_height = self.viewer_line_height();
        let Some(tab) = self.viewer_tabs.get_mut(tab_id) else {
            return;
        };
        let held = event.pressed_button == Some(MouseButton::Left);
        if !held {
            tab.release_lines();
            return;
        }
        let (bounds, offset) = {
            let state = tab.scroll().0.borrow();
            (state.base_handle.bounds(), state.base_handle.offset())
        };
        let row = drag_row(
            (event.position.y - bounds.origin.y) / px(1.0),
            -offset.y / px(1.0),
            row_height,
            bounds.size.height / px(1.0),
            tab.line_count(),
        );
        if row.is_some_and(|row| tab.drag_over_line(row, held)) {
            cx.notify();
        }
    }

    /// The left button came up: a drag on tab `tab_id`'s rows ends,
    /// wherever it was released.
    fn release_viewer_lines(&mut self, tab_id: &str) {
        if let Some(tab) = self.viewer_tabs.get_mut(tab_id) {
            tab.release_lines();
        }
    }

    /// A viewer line's height, in px, from the terminal font's size.
    fn viewer_line_height(&self) -> f32 {
        (self.sidebar.ui_state().terminal_font.size * LINE_HEIGHT_RATIO).round()
    }

    /// The Source view's keys on tab `tab_id`: find's keys, then Ctrl+C
    /// copies the selected lines, and nothing when none is; Ctrl+A selects
    /// every line; Esc clears the selection, or with none closes the find
    /// bar.
    fn on_viewer_key(
        &mut self,
        tab_id: &str,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.on_viewer_find_key(tab_id, &event.keystroke, window, cx) {
            cx.stop_propagation();
            return;
        }
        let Some(tab) = self.viewer_tabs.get_mut(tab_id) else {
            return;
        };
        let modifiers = event.keystroke.modifiers;
        let ctrl_only = modifiers.control && !modifiers.shift && !modifiers.alt;
        match event.keystroke.key.as_str() {
            "c" if ctrl_only => {
                if let Some(text) = tab.selected_text() {
                    self.copy_to_clipboard(&text, cx);
                }
                cx.stop_propagation();
            }
            "a" if ctrl_only => {
                if tab.select_all_lines() {
                    cx.notify();
                }
                cx.stop_propagation();
            }
            "escape" if !modifiers.modified() && (tab.clear_selection() || tab.close_find()) => {
                cx.notify();
                cx.stop_propagation();
            }
            _ => {}
        }
    }

    /// A header button of tab `tab_id` was pressed.
    fn press_viewer_button(
        &mut self,
        tab_id: &str,
        action: HeaderAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.viewer_tabs.get(tab_id) else {
            return;
        };
        let path = tab.path().to_owned();
        let origin = match tab.origin() {
            ViewerOrigin::Link => Some(Origin::Link),
            ViewerOrigin::Fetched => Some(Origin::Fetched),
            ViewerOrigin::NotAvailable => None,
        };
        let all = tab.text().map(|text| text.text().to_owned());
        let job = match action {
            HeaderAction::CopyPath => {
                self.copy_to_clipboard(&path, cx);
                return;
            }
            HeaderAction::CopyAll => {
                if let Some(all) = all {
                    self.copy_to_clipboard(&all, cx);
                }
                return;
            }
            HeaderAction::OpenDefault => OpenJob::DefaultApp {
                path: PathBuf::from(path),
                confirmed: false,
            },
            HeaderAction::Reveal => OpenJob::Reveal(PathBuf::from(path)),
        };
        let Some(origin) = origin else {
            tracing::debug!(tab_id, "a viewer tab's file is not on this connection");
            return;
        };
        let route = Route {
            then: Then::Nothing,
            origin,
        };
        self.open_by(job, route, window, cx);
    }

    /// Viewer tab `tab_id`'s body: the header, then the file's lines, or the
    /// banner that says why none show.
    pub(crate) fn viewer_tab_element(
        &self,
        tab_id: &str,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let t = theme(cx);
        let tab = self.viewer_tabs.get(tab_id)?;
        let body = format!("viewer-{tab_id}");
        let banner = tab.note().map(|note| {
            let selector = format!("viewer-banner-{tab_id}");
            div()
                .debug_selector(move || selector)
                .flex_none()
                .px(px(HEADER_PAD_LEFT))
                .py(px(10.0))
                .text_color(gpui::rgb(t.muted))
                .child(note)
        });
        let root = cx.entity().downgrade();
        let lines = tab.text().map(|text| match tab.markdown() {
            Some(MarkdownMode::Rendered) => match tab.markdown_doc() {
                Some((doc, blocks)) => self.viewer_markdown(tab_id, doc, blocks, &theme(cx)),
                // The document is being parsed.
                None => div().flex_1(),
            },
            Some(MarkdownMode::Source) | None => {
                self.viewer_lines(tab_id, tab, text, root, &theme(cx))
            }
        });
        let (key_tab, move_tab) = (tab_id.to_owned(), tab_id.to_owned());
        let (up_tab, up_out_tab) = (tab_id.to_owned(), tab_id.to_owned());
        Some(
            div()
                .debug_selector(move || body)
                .track_focus(&self.viewer_focus)
                .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                    this.on_viewer_key(&key_tab, event, window, cx);
                }))
                .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                    this.drag_viewer_to(&move_tab, event, cx);
                }))
                // Capture, so a child that stops the release cannot keep the
                // drag going; a release off the body ends it too.
                .capture_any_mouse_up(cx.listener(move |this, event: &MouseUpEvent, _, _| {
                    if event.button == MouseButton::Left {
                        this.release_viewer_lines(&up_tab);
                    }
                }))
                .on_mouse_up_out(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseUpEvent, _, _| {
                        this.release_viewer_lines(&up_out_tab);
                    }),
                )
                .flex()
                .flex_col()
                .flex_1()
                .size_full()
                .text_size(fonts::ui_px(cx, UI_TEXT_SIZE))
                .text_color(gpui::rgb(t.text))
                .child(viewer_header(tab_id, tab, cx))
                .children(viewer_find_bar(tab_id, tab, cx))
                .children(banner)
                .children(lines)
                .into_any_element(),
        )
    }

    /// The file's lines in the terminal font, each under its line number,
    /// scrolled to the line an open asked for; a press or a drag on them
    /// updates `root`'s selection.
    fn viewer_lines(
        &self,
        tab_id: &str,
        tab: &ViewerTab,
        text: &Arc<ViewerText>,
        root: WeakEntity<RootView>,
        t: &UiTheme,
    ) -> Div {
        let font = &self.sidebar.ui_state().terminal_font;
        let family = SharedString::from(
            font.family
                .clone()
                .unwrap_or_else(|| fonts::DEFAULT_FAMILY.to_owned()),
        );
        let digits = text.line_count().max(1).to_string().len();
        #[expect(
            clippy::cast_precision_loss,
            reason = "a line number's digit count is tiny"
        )]
        let gutter_width = digits as f32 * GUTTER_TEXT_SIZE * DIGIT_WIDTH_RATIO + GUTTER_PAD;
        let find = tab.find_shows().then(|| tab.find());
        let rows = Rows {
            tab_id: tab_id.to_owned(),
            text: Arc::clone(text),
            spans: tab.spans().cloned(),
            target: tab.jump_row(),
            selected: tab.selected_lines(),
            matches: find.map(|find| Arc::clone(find.matches())),
            current: find.and_then(|find| find.current_match().cloned()),
            line_height: self.viewer_line_height(),
            gutter_width,
            colors: TokenClass::ALL.map(|class| Hsla::from(gpui::rgb(class.color(t)))),
            theme: *t,
            root,
            focus: self.viewer_focus.clone(),
        };
        if let Some(row) = tab.take_jump() {
            tab.scroll()
                .scroll_to_item_strict(row, ScrollStrategy::Center);
        }
        let list = uniform_list(
            ElementId::Name(SharedString::from(format!("viewer-lines-{tab_id}"))),
            text.line_count(),
            move |range, _, _| rows.render(range),
        )
        .track_scroll(tab.scroll().clone())
        .size_full();
        div()
            .flex_1()
            .min_h(px(0.0))
            .font_family(family)
            .font_weight(crate::fonts::mono_weight())
            .text_size(px(font.size))
            .child(list)
    }
}

/// The row a drag reaches with the pointer `y` px below the line list's top,
/// the list scrolled `scrolled` px down, its rows `row_height` px tall in a
/// `viewport` px tall list of `count` rows: the row level with the pointer,
/// kept to the first and last rows in view; `None` with no row.
fn drag_row(y: f32, scrolled: f32, row_height: f32, viewport: f32, count: usize) -> Option<usize> {
    let last_row = count.checked_sub(1)?;
    if row_height <= 0.0 {
        return None;
    }
    let first_in_view = (scrolled / row_height).floor().max(0.0);
    let last_in_view = ((scrolled + viewport) / row_height).ceil() - 1.0;
    let row = ((y + scrolled) / row_height)
        .floor()
        .min(last_in_view)
        .max(first_in_view);
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the row is floored and at least zero; past the text it is cut to the last row"
    )]
    let row = row as usize;
    Some(row.min(last_row))
}

/// The top line in view of a line list scrolled by `scroll`, its rows
/// `row_height` px tall.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the row is floored and at least zero"
)]
fn top_line(scroll: &UniformListScrollHandle, row_height: f32) -> usize {
    if row_height <= 0.0 {
        return 0;
    }
    let scrolled = -scroll.0.borrow().base_handle.offset().y / px(1.0);
    (scrolled / row_height).floor().max(0.0) as usize
}

/// Scrolls `tab`'s lines, `row_height` px tall, to put the match on `line`,
/// if any, in the middle, unless it is already wholly in view.
fn scroll_to_match(tab: &ViewerTab, line: Option<usize>, row_height: f32) {
    if let Some(line) = line
        && !row_in_view(tab.scroll(), row_height, line)
    {
        tab.scroll()
            .scroll_to_item_strict(line, ScrollStrategy::Center);
    }
}

/// Whether row `line`, its rows `row_height` px tall, lies wholly in view
/// in a line list scrolled by `scroll`; never before the list is laid out.
#[expect(
    clippy::cast_precision_loss,
    reason = "a row's offset in px stays far below f32's exact range"
)]
fn row_in_view(scroll: &UniformListScrollHandle, row_height: f32, line: usize) -> bool {
    let (offset, height) = {
        let state = scroll.0.borrow();
        (
            state.base_handle.offset(),
            state.base_handle.bounds().size.height,
        )
    };
    let top = -offset.y / px(1.0);
    let row_top = line as f32 * row_height;
    row_height > 0.0 && row_top >= top && row_top + row_height <= top + height / px(1.0)
}

/// The find bar under the header while it shows: the query field, the
/// count, the previous and next buttons and the case toggle.
fn viewer_find_bar(tab_id: &str, tab: &ViewerTab, cx: &mut Context<RootView>) -> Option<Div> {
    let t = theme(cx);
    if !tab.find_shows() {
        return None;
    }
    let input = tab.find_bar()?.input.clone();
    let find = tab.find();
    let case_on = find.case_sensitive();
    let buttons = FindAction::ALL.map(|action| find_button(tab_id, action, case_on, cx));
    let (bar, count) = (
        format!("viewer-find-{tab_id}"),
        format!("viewer-find-count-{tab_id}"),
    );
    Some(
        div()
            .debug_selector(move || bar)
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.0))
            .h(fonts::ui_px(cx, FIND_BAR_HEIGHT))
            .pl(px(HEADER_PAD_LEFT))
            .pr(px(HEADER_PAD_RIGHT))
            .bg(gpui::rgb(t.surface))
            .border_b_1()
            .border_color(gpui::rgb(t.line))
            .child(
                div()
                    .flex_none()
                    .w(px(FIND_FIELD_WIDTH))
                    .px(px(6.0))
                    .py(px(3.0))
                    .rounded(px(4.0))
                    .border_1()
                    .border_color(gpui::rgb(t.line_strong))
                    .child(input),
            )
            .child(
                div()
                    .debug_selector(move || count)
                    .flex_none()
                    .min_w(px(FIND_COUNT_WIDTH))
                    .text_color(gpui::rgb(t.muted))
                    .child(find.label()),
            )
            .children(buttons),
    )
}

/// A find bar button; the case toggle is filled while case counts.
fn find_button(
    tab_id: &str,
    action: FindAction,
    case_on: bool,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    let selector = action.selector(tab_id);
    let button = if action == FindAction::MatchCase && case_on {
        primary_button(
            crate::fonts::ui_text_size(cx),
            crate::fonts::ui_weight_shift(cx),
            &selector,
            ButtonSize::Compact,
            true,
            Layer::Base,
            &theme(cx),
        )
    } else {
        outlined_button(
            crate::fonts::ui_text_size(cx),
            crate::fonts::ui_weight_shift(cx),
            &selector,
            ButtonSize::Compact,
            true,
            Layer::Base,
            &theme(cx),
        )
    };
    let tab_id = tab_id.to_owned();
    button
        .flex_none()
        .child(action.label())
        .tooltip(tooltip(action.tooltip()))
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.press_viewer_find_button(&tab_id, action, cx);
        }))
}

/// The header: the file's name whole and its folder dimmed, then the open
/// and copy buttons.
fn viewer_header(tab_id: &str, tab: &ViewerTab, cx: &mut Context<RootView>) -> Div {
    let t = theme(cx);
    let opens = tab.origin() != ViewerOrigin::NotAvailable;
    let loaded = tab.text().is_some();
    let buttons = HeaderAction::ALL.map(|action| {
        let enabled = match action {
            HeaderAction::OpenDefault | HeaderAction::Reveal => opens,
            HeaderAction::CopyPath => true,
            HeaderAction::CopyAll => loaded,
        };
        header_button(tab_id, action, enabled, cx)
    });
    let selector = format!("viewer-header-{tab_id}");
    div()
        .debug_selector(move || selector)
        .flex()
        .flex_none()
        .items_center()
        .gap(px(8.0))
        .h(fonts::ui_px(cx, HEADER_HEIGHT))
        .pl(px(HEADER_PAD_LEFT))
        .pr(px(HEADER_PAD_RIGHT))
        .bg(gpui::rgb(t.surface))
        .border_b_1()
        .border_color(gpui::rgb(t.line))
        .child(path_box(tab.path(), &t))
        .child(div().flex_1())
        .children(markdown_toggle(tab_id, tab, cx))
        .children(buttons)
}

/// A header button; a disabled open button's tooltip says why.
fn header_button(
    tab_id: &str,
    action: HeaderAction,
    enabled: bool,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    let button = outlined_button(
        crate::fonts::ui_text_size(cx),
        crate::fonts::ui_weight_shift(cx),
        &action.selector(tab_id),
        ButtonSize::Compact,
        enabled,
        Layer::Base,
        &theme(cx),
    )
    .flex_none()
    .child(action.label());
    if !enabled {
        let opens = matches!(action, HeaderAction::OpenDefault | HeaderAction::Reveal);
        return button.when(opens, |button| button.tooltip(tooltip(LOCAL_FILES_REASON)));
    }
    let tab_id = tab_id.to_owned();
    button.on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
        this.press_viewer_button(&tab_id, action, window, cx);
    }))
}

/// The file name whole, then its folder, which gives up its end to an
/// ellipsis as room runs out; the full path is the tooltip.
fn path_box(path: &str, t: &UiTheme) -> Stateful<Div> {
    let (name, folder) = split_path(path);
    let ellipsis_box = || {
        div()
            .relative()
            .min_w(px(0.0))
            .overflow_hidden()
            .whitespace_nowrap()
    };
    let name = ellipsis_box()
        .flex_none()
        .max_w(relative(1.0))
        .text_color(gpui::rgb(t.text))
        .children(ellipsized(
            PATH_NAME,
            SharedString::from(name.to_owned()),
            StyledText::new(name.to_owned()),
        ));
    div()
        .id("viewer-path")
        .flex()
        .gap(px(PATH_GAP))
        .min_w(px(0.0))
        .overflow_hidden()
        .whitespace_nowrap()
        .child(name)
        .when(!folder.is_empty(), |path| {
            path.child(
                ellipsis_box()
                    .text_color(gpui::rgb(t.subtle))
                    .children(ellipsized(
                        PATH_FOLDER,
                        SharedString::from(folder.to_owned()),
                        StyledText::new(folder.to_owned()),
                    )),
            )
        })
        .tooltip(tooltip(path.to_owned()))
}
