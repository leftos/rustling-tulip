//! A diff tab: a header (the path, the sides, the whitespace toggle, the
//! change count and the change buttons) over the side-by-side diff or the
//! text standing in for it. Also the root view's side of the diff tabs:
//! opening them, one view per tab, the snapshots, the live refresh, the
//! whitespace setting and the keyboard. The state lives in
//! [`crate::diff_tab`].

use gpui::{
    AnyElement, App, ClickEvent, Context, Div, Entity, EventEmitter, FocusHandle, KeyDownEvent,
    Stateful, Subscription, Task, Window, div, prelude::*, px,
};
use protocol::{ClientMessage, DaemonMessage, TabContent};

use crate::diff_model::{DiffModel, DiffOptions};
use crate::diff_tab::{
    Built, DiffTabBody, DiffTabHeader, DiffTabState, DiffTarget, LOADING_TEXT, Landed,
    OPEN_FAILED_TITLE, Snapshot, WHITESPACE_LABEL, WHITESPACE_TIP,
};
use crate::diff_view::{DiffView, Nav};
use crate::fonts::FontSettings;
use crate::notices::ToastKind;
use crate::source_control::ScKey;
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
pub(crate) enum DiffTabEvent {
    /// The whitespace toggle was clicked; the setting is every tab's.
    ToggleWhitespace,
}

/// One diff tab's header and body.
pub(crate) struct DiffTabView {
    state: DiffTabState,
    /// The diff, once the first build landed; later builds replace its
    /// model, so its scroll and focus stay.
    view: Option<Entity<DiffView>>,
    font: FontSettings,
    include_whitespace: bool,
    focus: FocusHandle,
    /// The build running on the background executor.
    build: Option<Task<()>>,
}

impl EventEmitter<DiffTabEvent> for DiffTabView {}

impl DiffTabView {
    fn new(
        target: DiffTarget,
        font: FontSettings,
        include_whitespace: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            state: DiffTabState::new(target),
            view: None,
            font,
            include_whitespace,
            focus: cx.focus_handle(),
            build: None,
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
        let options = DiffOptions {
            ignore_trim_whitespace: !self.include_whitespace,
        };
        let build = cx
            .background_executor()
            .spawn(async move { DiffModel::build(&old, &new, options) });
        self.build = Some(cx.spawn(async move |this, cx| {
            let model = build.await;
            // Fails only when the tab is gone, and its diff with it.
            this.update(cx, |tab, cx| tab.finish_build(generation, model, cx))
                .ok();
        }));
    }

    fn finish_build(&mut self, generation: u64, model: DiffModel, cx: &mut Context<Self>) {
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
        cx.notify();
    }

    fn set_include_whitespace(&mut self, include: bool, cx: &mut Context<Self>) {
        if self.include_whitespace != include {
            self.include_whitespace = include;
            self.rebuild(cx);
            cx.notify();
        }
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
        self.state.header(self.include_whitespace)
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
        let glyph = if header.include_whitespace {
            "☑"
        } else {
            "☐"
        };
        let toggle = div()
            .id("diff-whitespace")
            .debug_selector(|| "diff-whitespace".to_owned())
            .flex()
            .flex_none()
            .items_center()
            .gap(px(4.0))
            .px(px(4.0))
            .rounded(px(3.0))
            .cursor_pointer()
            .hover(|style| style.bg(gpui::rgb(HOVER_BG)))
            .child(glyph)
            .child(WHITESPACE_LABEL)
            .tooltip(tooltip(WHITESPACE_TIP))
            .on_click(cx.listener(|_, _: &ClickEvent, _, cx| {
                cx.emit(DiffTabEvent::ToggleWhitespace);
            }));
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
            .child(toggle)
            .child(count)
            .children(buttons)
    }
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
            let (font, include) = (ui.terminal_font.clone(), ui.diff_include_whitespace);
            let view = cx.new(|cx| DiffTabView::new(target, font, include, cx));
            let events = cx.subscribe_in(&view, window, |this, _, event, _, cx| match event {
                DiffTabEvent::ToggleWhitespace => this.toggle_diff_whitespace(cx),
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

    /// Whether diff tab `tab_id` has the keyboard.
    #[must_use]
    pub fn diff_tab_focused(&self, tab_id: &str, window: &Window, cx: &App) -> bool {
        self.diff_tabs
            .get(tab_id)
            .is_some_and(|slot| slot.view.read(cx).focus.is_focused(window))
    }
}
