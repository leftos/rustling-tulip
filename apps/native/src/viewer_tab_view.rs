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
    AnyElement, ClickEvent, Context, Div, ElementId, HighlightStyle, Hsla, ScrollStrategy,
    SharedString, Stateful, StyledText, Window, div, prelude::*, px, relative, uniform_list,
};
use protocol::{ClientMessage, DaemonMessage, TabContent};

use crate::buttons::{ButtonSize, outlined_button};
use crate::diff_tab::split_path;
use crate::ellipsis::ellipsized;
use crate::fonts;
use crate::notices::ToastKind;
use crate::open::OpenJob;
use crate::open_view::{Origin, Route, Then};
use crate::palette::{LINE, SUBTLE, SURFACE};
use crate::remote_mode::LOCAL_FILES_REASON;
use crate::syntax::{self, Highlighted, TokenClass};
use crate::viewer_md_view::markdown_toggle;
use crate::viewer_tab::{
    self, Activation, MarkdownMode, OPEN_FAILED_TITLE, ReadFailure, ViewerOrigin, ViewerTab,
    ViewerText,
};
use crate::{HOVER_BG, MUTED, RootView, TEXT, UI_TEXT_SIZE, tooltip};

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
    line_height: f32,
    gutter_width: f32,
    colors: [Hsla; 6],
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
        let number = index + 1;
        let selector = format!("viewer-line-{}-{number}", self.tab_id);
        let line = self.text.line(index);
        let highlights: Vec<(Range<usize>, HighlightStyle)> = self
            .spans
            .as_ref()
            .and_then(|spans| spans.get(index))
            .map(|spans| {
                spans
                    .iter()
                    .filter(|(span, _)| {
                        span.end <= line.len()
                            && line.is_char_boundary(span.start)
                            && line.is_char_boundary(span.end)
                    })
                    .map(|(span, class)| (span.clone(), self.style(*class)))
                    .collect()
            })
            .unwrap_or_default();
        div()
            .w_full()
            .h(px(self.line_height))
            .flex()
            .flex_row()
            .items_center()
            .when(self.target == Some(index), |row| {
                row.bg(gpui::rgb(HOVER_BG))
            })
            .child(
                div()
                    .debug_selector(move || selector)
                    .flex_none()
                    .w(px(self.gutter_width))
                    .pr(px(GUTTER_PAD))
                    .text_right()
                    .text_size(px(GUTTER_TEXT_SIZE))
                    .text_color(gpui::rgb(SUBTLE))
                    .child(number.to_string()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .pl(px(TEXT_PAD_LEFT))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(StyledText::new(line.to_owned()).with_highlights(highlights)),
            )
    }

    fn style(&self, class: TokenClass) -> HighlightStyle {
        HighlightStyle {
            color: Some(self.colors[class.index()]),
            ..HighlightStyle::default()
        }
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
        self.send(ClientMessage::OpenViewerTab {
            id,
            path: normalised,
        });
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
                TabContent::Grid { .. } | TabContent::Diff { .. } => None,
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
    pub(crate) fn on_viewer_message(
        &mut self,
        msg: &DaemonMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
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
                    self.after_tabs_change(window, cx);
                }
                true
            }
            DaemonMessage::Error {
                message,
                request_id: Some(id),
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
        let tab = self.viewer_tabs.get(tab_id)?;
        let body = format!("viewer-{tab_id}");
        let banner = tab.note().map(|note| {
            let selector = format!("viewer-banner-{tab_id}");
            div()
                .debug_selector(move || selector)
                .flex_none()
                .px(px(HEADER_PAD_LEFT))
                .py(px(10.0))
                .text_color(gpui::rgb(MUTED))
                .child(note)
        });
        let lines = tab.text().map(|text| match tab.markdown() {
            Some(MarkdownMode::Rendered) => match tab.markdown_doc() {
                Some((doc, blocks)) => self.viewer_markdown(tab_id, doc, blocks),
                // The document is being parsed.
                None => div().flex_1(),
            },
            Some(MarkdownMode::Source) | None => self.viewer_lines(tab_id, tab, text),
        });
        Some(
            div()
                .debug_selector(move || body)
                .track_focus(&self.viewer_focus)
                .flex()
                .flex_col()
                .flex_1()
                .size_full()
                .text_size(px(UI_TEXT_SIZE))
                .text_color(gpui::rgb(TEXT))
                .child(viewer_header(tab_id, tab, cx))
                .children(banner)
                .children(lines)
                .into_any_element(),
        )
    }

    /// The file's lines in the terminal font, each under its line number,
    /// scrolled to the line an open asked for.
    fn viewer_lines(&self, tab_id: &str, tab: &ViewerTab, text: &Arc<ViewerText>) -> Div {
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
        let rows = Rows {
            tab_id: tab_id.to_owned(),
            text: Arc::clone(text),
            spans: tab.spans().cloned(),
            target: tab.jump_row(),
            line_height: (font.size * LINE_HEIGHT_RATIO).round(),
            gutter_width,
            colors: TokenClass::ALL.map(|class| Hsla::from(gpui::rgb(class.color()))),
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
            .text_size(px(font.size))
            .child(list)
    }
}

/// The header: the file's name whole and its folder dimmed, then the open
/// and copy buttons.
fn viewer_header(tab_id: &str, tab: &ViewerTab, cx: &mut Context<RootView>) -> Div {
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
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(8.0))
        .h(px(HEADER_HEIGHT))
        .pl(px(HEADER_PAD_LEFT))
        .pr(px(HEADER_PAD_RIGHT))
        .bg(gpui::rgb(SURFACE))
        .border_b_1()
        .border_color(gpui::rgb(LINE))
        .child(path_box(tab.path()))
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
    let button = outlined_button(&action.selector(tab_id), ButtonSize::Compact, enabled)
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
fn path_box(path: &str) -> Stateful<Div> {
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
        .text_color(gpui::rgb(TEXT))
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
                    .text_color(gpui::rgb(SUBTLE))
                    .children(ellipsized(
                        PATH_FOLDER,
                        SharedString::from(folder.to_owned()),
                        StyledText::new(folder.to_owned()),
                    )),
            )
        })
        .tooltip(tooltip(path.to_owned()))
}
