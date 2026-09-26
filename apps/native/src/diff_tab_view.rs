//! A diff tab: a header (the path, the sides, the whitespace and highlight
//! toggles, the change count and the change buttons) over the side-by-side
//! diff or the text standing in for it. Also the root view's side of the
//! diff tabs: opening them, one view per tab, the snapshots, the live
//! refresh, the syntax highlighting, the whitespace and highlight settings
//! and the keyboard. The state lives in [`crate::diff_tab`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use gpui::{
    AnyElement, App, ClickEvent, Context, Div, Entity, EventEmitter, FocusHandle, KeyDownEvent,
    Stateful, Subscription, Task, Window, div, prelude::*, px,
};
use protocol::{ClientMessage, DaemonMessage, TabContent};

use crate::diff_model::{DiffModel, DiffOptions};
use crate::diff_tab::{
    Built, DiffTabBody, DiffTabHeader, DiffTabState, DiffTarget, HIGHLIGHT_LABEL, HIGHLIGHT_TIP,
    LOADING_TEXT, Landed, OPEN_FAILED_TITLE, Snapshot, WHITESPACE_LABEL, WHITESPACE_TIP,
};
use crate::diff_view::{DiffView, Nav};
use crate::fonts::FontSettings;
use crate::notices::ToastKind;
use crate::source_control::ScKey;
use crate::syntax::{self, Highlighted};
use crate::{BAR_BG, BORDER, HOVER_BG, MUTED, RootView, TEXT, UI_TEXT_SIZE, tooltip};

const HEADER_HEIGHT: f32 = 24.0;
/// The change buttons, left to right: where each moves, its selector, glyph
/// and tooltip.
const NAV_BUTTONS: [(Nav, &str, &str, &str); 4] = [
    (Nav::First, "diff-first", "⏮", "First change"),
    (Nav::Prev, "diff-prev", "◀", "Previous change (Shift+F7)"),
    (Nav::Next, "diff-next", "▶", "Next change (F7)"),
    (Nav::Last, "diff-last", "⏭", "Last change"),
];

/// What a diff tab asks of the root view.
#[derive(Debug, Clone, Copy)]
pub(crate) enum DiffTabEvent {
    /// The whitespace toggle was clicked; the setting is every tab's.
    ToggleWhitespace,
    /// The highlight toggle was clicked; the setting is every tab's.
    ToggleHighlight,
}

/// One side's syntax classes and the text and language they came from.
struct SideSyntax {
    language: String,
    text: Arc<str>,
    /// `None` when the side is left uncoloured: no grammar, or a cutoff.
    spans: Option<Arc<Highlighted>>,
}

/// A side's highlighting, running on the background executor.
struct PendingSyntax {
    /// The run's number; only a side's latest run lands.
    run: u64,
    language: String,
    text: Arc<str>,
    task: Option<Task<()>>,
    /// Set to stop the run between lines.
    cancel: Arc<AtomicBool>,
}

impl PendingSyntax {
    /// Stops the run at its next line; its result, if any, is not taken.
    fn stop(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Whether `language` and `text` are the ones `(held_language, held_text)`
/// name.
fn same_input(held: (&str, &Arc<str>), language: &str, text: &Arc<str>) -> bool {
    held.0 == language && (Arc::ptr_eq(held.1, text) || **held.1 == **text)
}

/// One diff tab's header and body.
pub(crate) struct DiffTabView {
    state: DiffTabState,
    /// The diff, once the first build landed; later builds replace its
    /// model, so its scroll and focus stay.
    view: Option<Entity<DiffView>>,
    font: FontSettings,
    include_whitespace: bool,
    /// Whether the diff is coloured by its language.
    highlight: bool,
    focus: FocusHandle,
    /// The build running on the background executor.
    build: Option<Task<()>>,
    /// The (old, new) texts the view's model was built from.
    model_texts: Option<(Arc<str>, Arc<str>)>,
    /// Each side's last landed highlighting, old then new; kept while the
    /// toggle is off and across whitespace rebuilds.
    syntax: [Option<SideSyntax>; 2],
    /// Each side's highlighting in flight, old then new.
    pending: [Option<PendingSyntax>; 2],
    /// How many highlight runs the tab started.
    highlight_runs: u64,
}

impl EventEmitter<DiffTabEvent> for DiffTabView {}

impl Drop for DiffTabView {
    /// A closed tab stops its highlighting runs.
    fn drop(&mut self) {
        self.stop_highlighting();
    }
}

impl DiffTabView {
    fn new(
        target: DiffTarget,
        font: FontSettings,
        include_whitespace: bool,
        highlight: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            state: DiffTabState::new(target),
            view: None,
            font,
            include_whitespace,
            highlight,
            focus: cx.focus_handle(),
            build: None,
            model_texts: None,
            syntax: [None, None],
            pending: [None, None],
            highlight_runs: 0,
        }
    }

    /// Acts on a landed reply; returns whether to fetch again.
    fn land(&mut self, landed: Landed, cx: &mut Context<Self>) -> bool {
        if landed.build {
            self.rebuild(cx);
        }
        cx.notify();
        landed.refetch
    }

    /// Builds the diff of the texts held on the background executor. A
    /// build started before it is discarded when it lands.
    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let Some((old, new, generation)) = self.state.build_input() else {
            return;
        };
        self.highlight_texts(cx);
        let options = DiffOptions {
            ignore_trim_whitespace: !self.include_whitespace,
        };
        let texts = (old.clone(), new.clone());
        let build = cx
            .background_executor()
            .spawn(async move { DiffModel::build(&old, &new, options) });
        self.build = Some(cx.spawn(async move |this, cx| {
            let model = build.await;
            // Fails only when the tab is gone, and its diff with it.
            this.update(cx, |tab, cx| tab.finish_build(generation, model, texts, cx))
                .ok();
        }));
    }

    fn finish_build(
        &mut self,
        generation: u64,
        model: DiffModel,
        texts: (Arc<str>, Arc<str>),
        cx: &mut Context<Self>,
    ) {
        let built = Built {
            changes: model.change_count(),
            line_endings: model.line_endings_changed(),
            final_newline: model.final_newline_changed(),
        };
        if !self.state.accept_build(generation, built) {
            return;
        }
        if let Some(view) = &self.view {
            view.update(cx, |view, cx| view.set_model(model, cx));
        } else {
            let font = self.font.clone();
            self.view = Some(cx.new(|cx| DiffView::new(model, font, cx)));
        }
        self.model_texts = Some(texts);
        self.push_syntax(cx);
        cx.notify();
    }

    /// Starts highlighting each side of the texts held that has no landed
    /// or running highlighting of the same text and language. Nothing while
    /// the toggle is off, or for a language without a grammar.
    fn highlight_texts(&mut self, cx: &mut Context<Self>) {
        if !self.highlight {
            return;
        }
        let (Some(language), Some((old, new))) =
            (self.state.language().map(str::to_owned), self.state.texts())
        else {
            return;
        };
        if !syntax::has_grammar(&language) {
            return;
        }
        for (side, text) in [old, new].into_iter().enumerate() {
            let landed = self.syntax[side]
                .as_ref()
                .is_some_and(|s| same_input((&s.language, &s.text), &language, &text));
            let running = self.pending[side]
                .as_ref()
                .is_some_and(|p| same_input((&p.language, &p.text), &language, &text));
            if !landed && !running {
                self.start_highlight(side, language.clone(), text, cx);
            }
        }
    }

    /// Highlights `text` in `language` on the background executor as side
    /// `side`'s latest run, replacing any run before it.
    fn start_highlight(
        &mut self,
        side: usize,
        language: String,
        text: Arc<str>,
        cx: &mut Context<Self>,
    ) {
        if let Some(replaced) = self.pending[side].take() {
            replaced.stop();
        }
        self.highlight_runs += 1;
        let run = self.highlight_runs;
        let cancel = Arc::new(AtomicBool::new(false));
        let (id, input, stop) = (language.clone(), text.clone(), cancel.clone());
        let work = cx.background_executor().spawn(async move {
            let Some(grammar) = syntax::syntax_for(&id) else {
                tracing::debug!(side, run, language = %id, "diff side left uncoloured: no grammar");
                return None;
            };
            let deadline = Instant::now() + syntax::TIME_LIMIT;
            match syntax::highlight(&input, grammar, syntax::syntax_set(), deadline, &stop) {
                Ok(spans) => Some(Arc::new(spans)),
                Err(reason) => {
                    tracing::debug!(side, run, ?reason, "diff side left uncoloured");
                    None
                }
            }
        });
        let task = cx.spawn(async move |this, cx| {
            let spans = work.await;
            // Fails only when the tab is gone.
            this.update(cx, |tab, cx| tab.finish_highlight(side, run, spans, cx))
                .ok();
        });
        self.pending[side] = Some(PendingSyntax {
            run,
            language,
            text,
            task: Some(task),
            cancel,
        });
    }

    /// Stops every highlighting run in flight; none of them lands.
    fn stop_highlighting(&mut self) {
        for pending in self.pending.iter_mut().filter_map(Option::take) {
            pending.stop();
        }
    }

    /// Side `side`'s run `run` finished with `spans`; kept when it is still
    /// that side's latest run.
    fn finish_highlight(
        &mut self,
        side: usize,
        run: u64,
        spans: Option<Arc<Highlighted>>,
        cx: &mut Context<Self>,
    ) {
        let Some(mut pending) = self.pending[side].take_if(|p| p.run == run) else {
            return;
        };
        // The task running this; it is finishing, so it is left to end.
        if let Some(task) = pending.task.take() {
            task.detach();
        }
        // Texts newer than the model shown wait for their rebuild, which
        // pushes them; the model shown keeps its colours meanwhile.
        let shown = self.model_texts.as_ref().is_some_and(|(old, new)| {
            let model = if side == 0 { old } else { new };
            Arc::ptr_eq(model, &pending.text) || **model == *pending.text
        });
        self.syntax[side] = Some(SideSyntax {
            language: pending.language,
            text: pending.text,
            spans,
        });
        if shown {
            self.push_syntax(cx);
        }
    }

    /// Gives the view the highlighting of the texts its model was built
    /// from, side by side; none while the toggle is off or before it lands.
    fn push_syntax(&self, cx: &mut Context<Self>) {
        let Some(view) = &self.view else {
            return;
        };
        let spans = |side: usize, text: &Arc<str>| {
            let language = self.state.language()?;
            let held = self.syntax[side].as_ref()?;
            if !self.highlight || !same_input((&held.language, &held.text), language, text) {
                return None;
            }
            held.spans.clone()
        };
        let (old, new) = self
            .model_texts
            .as_ref()
            .map_or((None, None), |(old, new)| (spans(0, old), spans(1, new)));
        view.update(cx, |view, cx| view.set_syntax(old, new, cx));
    }

    fn set_include_whitespace(&mut self, include: bool, cx: &mut Context<Self>) {
        if self.include_whitespace != include {
            self.include_whitespace = include;
            self.rebuild(cx);
            cx.notify();
        }
    }

    /// Turns the colours on, highlighting what has not been, or off, which
    /// hides them at once and stops the runs in flight.
    fn set_highlight(&mut self, highlight: bool, cx: &mut Context<Self>) {
        if self.highlight == highlight {
            return;
        }
        self.highlight = highlight;
        if highlight {
            self.highlight_texts(cx);
        } else {
            self.stop_highlighting();
        }
        self.push_syntax(cx);
        cx.notify();
    }

    fn go(&mut self, nav: Nav, cx: &mut Context<Self>) {
        if let Some(view) = &self.view {
            view.update(cx, |view, cx| view.go(nav, cx));
        }
    }

    /// F7 goes to the next change and Shift+F7 to the previous one, while
    /// the tab has the keyboard.
    fn on_key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let ks = &event.keystroke;
        let mods = ks.modifiers;
        if ks.key != "f7" || mods.control || mods.alt || mods.platform {
            return;
        }
        self.go(if mods.shift { Nav::Prev } else { Nav::Next }, cx);
        cx.stop_propagation();
    }

    fn header(&self) -> DiffTabHeader {
        self.state.header(self.include_whitespace, self.highlight)
    }

    fn header_bar(header: &DiffTabHeader, cx: &mut Context<Self>) -> Div {
        // Right-aligned in a clipped box, so a long path loses its start.
        let path = div()
            .id("diff-path")
            .debug_selector(|| "diff-path".to_owned())
            .flex()
            .justify_end()
            .min_w(px(0.0))
            .overflow_hidden()
            .child(
                div()
                    .flex_none()
                    .whitespace_nowrap()
                    .child(header.path.clone()),
            )
            .tooltip(tooltip(header.path.clone()));
        let mode = div()
            .id("diff-mode")
            .flex_none()
            .text_color(gpui::rgb(MUTED))
            .child(header.mode.clone())
            .when_some(header.mode_tip.clone(), |mode, tip| {
                mode.tooltip(tooltip(tip))
            });
        let whitespace = toggle(
            DiffTabEvent::ToggleWhitespace,
            header.include_whitespace,
            cx,
        );
        let highlight = toggle(DiffTabEvent::ToggleHighlight, header.highlight, cx);
        let count = div()
            .flex_none()
            .text_color(gpui::rgb(MUTED))
            .child(header.count.clone());
        let buttons: Vec<Stateful<Div>> = NAV_BUTTONS
            .into_iter()
            .map(|(nav, selector, glyph, tip)| {
                nav_button(nav, selector, glyph, tip, header.nav_enabled, cx)
            })
            .collect();
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.0))
            .h(px(HEADER_HEIGHT))
            .px(px(8.0))
            .bg(gpui::rgb(BAR_BG))
            .border_b_1()
            .border_color(gpui::rgb(BORDER))
            .child(path)
            .child(mode)
            .child(div().flex_1())
            .child(whitespace)
            .child(highlight)
            .child(count)
            .children(buttons)
    }
}

/// A header checkbox that asks the root view for `event`, checked when `on`.
fn toggle(event: DiffTabEvent, on: bool, cx: &mut Context<DiffTabView>) -> Stateful<Div> {
    let (selector, label, tip) = match event {
        DiffTabEvent::ToggleWhitespace => ("diff-whitespace", WHITESPACE_LABEL, WHITESPACE_TIP),
        DiffTabEvent::ToggleHighlight => ("diff-highlight", HIGHLIGHT_LABEL, HIGHLIGHT_TIP),
    };
    let glyph = if on { "☑" } else { "☐" };
    div()
        .id(selector)
        .debug_selector(move || selector.to_owned())
        .flex()
        .flex_none()
        .items_center()
        .gap(px(4.0))
        .px(px(4.0))
        .rounded(px(3.0))
        .cursor_pointer()
        .hover(|style| style.bg(gpui::rgb(HOVER_BG)))
        .child(glyph)
        .child(label)
        .tooltip(tooltip(tip))
        .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| cx.emit(event)))
}

/// A change button; a disabled one is dimmed and takes no click.
fn nav_button(
    nav: Nav,
    selector: &'static str,
    glyph: &'static str,
    tip: &'static str,
    enabled: bool,
    cx: &mut Context<DiffTabView>,
) -> Stateful<Div> {
    let base = div()
        .id(selector)
        .debug_selector(move || selector.to_owned())
        .flex_none()
        .px(px(4.0))
        .rounded(px(3.0))
        .child(glyph)
        .tooltip(tooltip(tip));
    if !enabled {
        return base.opacity(0.5);
    }
    base.cursor_pointer()
        .hover(|style| style.bg(gpui::rgb(HOVER_BG)))
        .on_click(cx.listener(move |tab, _: &ClickEvent, _, cx| tab.go(nav, cx)))
}

/// The text standing in for the diff, centred, with a muted line under it.
fn message(text: String, note: Option<&'static str>) -> AnyElement {
    div()
        .flex_1()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(4.0))
        .child(text)
        .when_some(note, |body, note| {
            body.child(div().text_color(gpui::rgb(MUTED)).child(note))
        })
        .into_any_element()
}

impl Render for DiffTabView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let header = self.header();
        let body = match (self.state.body(), &self.view) {
            (DiffTabBody::Diff, Some(view)) => div()
                .flex_1()
                .min_h(px(0.0))
                .w_full()
                .child(view.clone())
                .into_any_element(),
            (DiffTabBody::Diff, None) => message(LOADING_TEXT.to_owned(), None),
            (DiffTabBody::Text { text, note }, _) => message(text, note),
        };
        div()
            .id("diff-tab")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key_down))
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .h_full()
            .text_size(px(UI_TEXT_SIZE))
            .text_color(gpui::rgb(TEXT))
            .child(Self::header_bar(&header, cx))
            .child(body)
    }
}

/// A diff tab's view and the subscription to its events.
pub(crate) struct DiffSlot {
    view: Entity<DiffTabView>,
    _events: Subscription,
}

impl RootView {
    /// Asks the daemon to open, or focus, the diff of `path` in the tree
    /// `key`; the tab it names is activated when it answers.
    pub(crate) fn open_diff(&mut self, key: &ScKey, path: &str, against: Option<String>) {
        let id = self.diff_opens.open_id();
        self.send(ClientMessage::OpenDiffTab {
            id,
            repo_id: key.repo_id.clone(),
            path: path.to_owned(),
            against,
            worktree_path: key.worktree.clone(),
        });
    }

    /// Gives every diff tab a view, which fetches its snapshot, and drops
    /// the views of tabs that are gone. A hidden tab keeps its view.
    pub(crate) fn reconcile_diff_tabs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let targets: Vec<(String, DiffTarget)> = self
            .tabs
            .tabs()
            .iter()
            .filter_map(|tab| match &tab.content {
                TabContent::Diff {
                    repo_id,
                    path,
                    against,
                    worktree_path,
                } => Some((
                    tab.id.clone(),
                    DiffTarget {
                        repo_id: repo_id.clone(),
                        path: path.clone(),
                        against: against.clone(),
                        worktree_path: worktree_path.clone(),
                    },
                )),
                TabContent::Grid { .. } => None,
            })
            .collect();
        self.diff_tabs
            .retain(|tab_id, _| targets.iter().any(|(id, _)| id == tab_id));
        for (tab_id, target) in targets {
            if self.diff_tabs.contains_key(&tab_id) {
                continue;
            }
            let ui = self.sidebar.ui_state();
            let (font, include, highlight) = (
                ui.terminal_font.clone(),
                ui.diff_include_whitespace,
                ui.diff_highlight,
            );
            let view = cx.new(|cx| DiffTabView::new(target, font, include, highlight, cx));
            let events = cx.subscribe_in(&view, window, |this, _, event, _, cx| match event {
                DiffTabEvent::ToggleWhitespace => this.toggle_diff_whitespace(cx),
                DiffTabEvent::ToggleHighlight => this.toggle_diff_highlight(cx),
            });
            self.fetch_diff(&view, cx);
            self.diff_tabs.insert(
                tab_id,
                DiffSlot {
                    view,
                    _events: events,
                },
            );
        }
    }

    /// Sends `view`'s tab a fresh `GetFileSnapshot`.
    fn fetch_diff(&mut self, view: &Entity<DiffTabView>, cx: &mut Context<Self>) {
        let id = self.diff_opens.snapshot_id();
        let msg = view.update(cx, |tab, _| tab.state.start_fetch(id));
        self.send(msg);
    }

    fn diff_views(&self) -> Vec<Entity<DiffTabView>> {
        self.diff_tabs
            .values()
            .map(|slot| slot.view.clone())
            .collect()
    }

    /// Folds a diff message in; returns whether nothing else is owed it.
    pub(crate) fn on_diff_message(
        &mut self,
        msg: &DaemonMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match msg {
            DaemonMessage::Welcome { .. } => {
                self.diff_opens.reset();
                for view in self.diff_views() {
                    if view.update(cx, |tab, _| tab.state.reset_connection()) {
                        self.fetch_diff(&view, cx);
                    }
                }
                false
            }
            DaemonMessage::DiffTabOpened { id, tab_id } => {
                let known = self.tabs.tab(tab_id).is_some();
                if let Some(tab_id) = self.diff_opens.opened(id, tab_id, known, (self.now)()) {
                    // Re-opening the active tab focuses it again too.
                    self.last_active_tab = None;
                    self.tabs.activate(&tab_id);
                    self.after_tabs_change(window, cx);
                }
                true
            }
            DaemonMessage::FileSnapshot {
                id,
                old,
                new,
                language,
                unavailable,
                ..
            } => {
                let snapshot = Snapshot {
                    old: old.clone(),
                    new: new.clone(),
                    language: language.clone(),
                    unavailable: unavailable.clone(),
                };
                self.land_snapshot(id, cx, |state| state.on_snapshot(id, snapshot));
                true
            }
            DaemonMessage::FileSnapshotError { id, error, .. } => {
                let error = error.clone();
                self.land_snapshot(id, cx, |state| state.on_snapshot_error(id, error));
                true
            }
            DaemonMessage::Error {
                message,
                request_id: Some(id),
            } if self.diff_opens.failed(id) => {
                tracing::warn!("the daemon could not open a diff tab: {message}");
                self.push_toast(
                    ToastKind::Error,
                    OPEN_FAILED_TITLE,
                    Some(message.clone()),
                    cx,
                );
                true
            }
            DaemonMessage::RepoStatus {
                repo_id,
                worktree_path,
                ..
            } => {
                let key = ScKey {
                    repo_id: repo_id.clone(),
                    worktree: worktree_path.clone(),
                };
                for view in self.diff_views() {
                    if view.update(cx, |tab, _| tab.state.on_repo_status(&key)) {
                        self.fetch_diff(&view, cx);
                    }
                }
                false
            }
            _ => false,
        }
    }

    /// Hands the reply to fetch `id` to the tab waiting for it, and fetches
    /// again when a refresh came in meanwhile. A stale id reaches no tab.
    fn land_snapshot(
        &mut self,
        id: &str,
        cx: &mut Context<Self>,
        reply: impl FnOnce(&mut DiffTabState) -> Option<Landed>,
    ) {
        let waiting = self
            .diff_tabs
            .values()
            .find(|slot| slot.view.read(cx).state.awaits(id))
            .map(|slot| slot.view.clone());
        let Some(view) = waiting else {
            return;
        };
        let refetch = view.update(cx, |tab, cx| {
            reply(&mut tab.state).is_some_and(|landed| tab.land(landed, cx))
        });
        if refetch {
            self.fetch_diff(&view, cx);
        }
    }

    /// Activates the tab an `OpenDiffTab` named before the tab list held it,
    /// once it does.
    pub(crate) fn activate_pending_diff_tab(&mut self) {
        let now = (self.now)();
        let tabs = &self.tabs;
        let ready = self
            .diff_opens
            .take_ready(|tab_id| tabs.tab(tab_id).is_some(), now);
        if let Some(tab_id) = ready {
            self.tabs.activate(&tab_id);
        }
    }

    /// Flips whether the diffs show whitespace-only changes, saves it, and
    /// rebuilds every diff tab from the texts it holds.
    fn toggle_diff_whitespace(&mut self, cx: &mut Context<Self>) {
        let include = !self.sidebar.ui_state().diff_include_whitespace;
        self.sidebar.set_diff_include_whitespace(include);
        self.save_ui();
        for view in self.diff_views() {
            view.update(cx, |tab, cx| tab.set_include_whitespace(include, cx));
        }
        cx.notify();
    }

    /// Flips whether the diffs are coloured by their language, saves it,
    /// and applies it to every diff tab.
    fn toggle_diff_highlight(&mut self, cx: &mut Context<Self>) {
        let highlight = !self.sidebar.ui_state().diff_highlight;
        self.sidebar.set_diff_highlight(highlight);
        self.save_ui();
        for view in self.diff_views() {
            view.update(cx, |tab, cx| tab.set_highlight(highlight, cx));
        }
        cx.notify();
    }

    /// Gives the keyboard to the active tab when it is a diff tab; returns
    /// whether it was.
    pub(crate) fn focus_active_diff_tab(&self, window: &mut Window, cx: &App) -> bool {
        let Some(slot) = self.tabs.active_id().and_then(|id| self.diff_tabs.get(id)) else {
            return false;
        };
        slot.view.read(cx).focus.focus(window);
        true
    }

    /// The diff tab `tab_id`'s view, as an element.
    pub(crate) fn diff_tab_element(&self, tab_id: &str) -> Option<AnyElement> {
        Some(self.diff_tabs.get(tab_id)?.view.clone().into_any_element())
    }

    /// Diff tab `tab_id`'s header, as text.
    #[must_use]
    pub fn diff_tab_header(&self, tab_id: &str, cx: &App) -> Option<DiffTabHeader> {
        Some(self.diff_tabs.get(tab_id)?.view.read(cx).header())
    }

    /// What diff tab `tab_id`'s body shows.
    #[must_use]
    pub fn diff_tab_body(&self, tab_id: &str, cx: &App) -> Option<DiffTabBody> {
        Some(self.diff_tabs.get(tab_id)?.view.read(cx).state.body())
    }

    /// Diff tab `tab_id`'s side-by-side view, once its first diff is built.
    #[must_use]
    pub fn diff_tab_view(&self, tab_id: &str, cx: &App) -> Option<Entity<DiffView>> {
        self.diff_tabs.get(tab_id)?.view.read(cx).view.clone()
    }

    /// How many highlight runs diff tab `tab_id` started, one a side each.
    #[must_use]
    pub fn diff_tab_highlight_runs(&self, tab_id: &str, cx: &App) -> Option<u64> {
        Some(self.diff_tabs.get(tab_id)?.view.read(cx).highlight_runs)
    }

    /// Whether diff tab `tab_id` has the keyboard.
    #[must_use]
    pub fn diff_tab_focused(&self, tab_id: &str, window: &Window, cx: &App) -> bool {
        self.diff_tabs
            .get(tab_id)
            .is_some_and(|slot| slot.view.read(cx).focus.is_focused(window))
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use std::ops::Range;

    use gpui::{Hsla, TestAppContext};

    use super::*;
    use crate::diff_view::Half;

    fn target() -> DiffTarget {
        DiffTarget {
            repo_id: "r1".to_owned(),
            path: "src/main.rs".to_owned(),
            against: None,
            worktree_path: None,
        }
    }

    /// Lands a Rust snapshot of `old` and `new` on `tab`'s state, and
    /// builds nothing.
    fn snapshot(tab: &mut DiffTabView, id: &str, old: &str, new: &str) -> Landed {
        tab.state.start_fetch(id.to_owned());
        let snapshot = Snapshot {
            old: old.to_owned(),
            new: new.to_owned(),
            language: "rust".to_owned(),
            unavailable: None,
        };
        tab.state.on_snapshot(id, snapshot).expect("the fetch out")
    }

    /// The syntax colours of the first row's old half on screen.
    fn first_row(tab: &Entity<DiffTabView>, cx: &TestAppContext) -> Vec<(Range<usize>, Hsla)> {
        cx.read(|cx| {
            let view = tab.read(cx).view.clone().expect("a diff is built");
            view.read(cx).syntax_colors(0, Half::Old)
        })
    }

    #[gpui::test]
    fn highlighting_landing_before_the_rebuild_keeps_the_colours_shown(cx: &mut TestAppContext) {
        let font = FontSettings::default();
        let tab = cx.new(|cx| DiffTabView::new(target(), font, true, true, cx));
        tab.update(cx, |tab, cx| {
            let landed = snapshot(tab, "s1", "fn a() {}\n", "fn b() {}\n");
            tab.land(landed, cx);
        });
        cx.run_until_parked();
        let before = first_row(&tab, cx);
        assert!(!before.is_empty(), "the first texts are coloured");

        // New texts, whose highlighting lands while the old model shows.
        tab.update(cx, |tab, cx| {
            snapshot(tab, "s2", "let x = 1;\n", "let y = 2;\n");
            tab.highlight_texts(cx);
        });
        cx.run_until_parked();
        assert_eq!(
            first_row(&tab, cx),
            before,
            "the model shown keeps its colours"
        );

        tab.update(cx, DiffTabView::rebuild);
        cx.run_until_parked();
        let after = first_row(&tab, cx);
        assert!(!after.is_empty(), "the rebuild takes the landed colours");
        assert_ne!(after, before);
    }
}
