//! The History half of the source-control panel: the split under the
//! changes area, one History block per section with its commits, `load more`
//! row and forge button, and the selected commit's detail pane. The state is
//! [`crate::history::HistoryModel`]; this renders it and forwards clicks and
//! drags.

use gpui::{
    AnyElement, ClickEvent, Context, Div, FontWeight, IntoElement, MouseButton, MouseDownEvent,
    SharedString, Stateful, Window, canvas, div, prelude::*, px,
};
use protocol::DaemonMessage;

use crate::history::{
    Applied, CommitRow, DetailPane, ForgeButton, HistoryBlock, HistoryBody, MIN_CHANGES_SIDE,
    MIN_LIST_SIDE, MoreRow, ScLayout, changes_height, clamp_split, list_height,
};
use crate::notices::ToastKind;
use crate::open::{self, OpenJob};
use crate::open_view::{COULD_NOT_OPEN, Then};
use crate::sidebar::Activity;
use crate::source_control::{Part, ScKey, Section, sections};
use crate::{BORDER, Drag, HOVER_BG, MUTED, RootView, TEXT, drag_handle, tooltip};

/// The forge button's label.
pub const FORGE_LABEL: &str = "Open in forge";
const HISTORY_LABEL: &str = "History";
const ROW_HEIGHT: f32 = 22.0;
const ROW_PADDING: f32 = 8.0;
const BUTTON_HEIGHT: f32 = 18.0;
const AUTHOR_WIDTH: f32 = 90.0;

impl RootView {
    /// The panel's sections, and whether they are a focused session's
    /// members.
    fn history_sections(&self) -> (Vec<Section>, bool) {
        let members = self.sc_focused_members();
        let shown = sections(
            self.sidebar.repos(),
            members,
            self.sidebar.source_control().pinned_repo.as_deref(),
        );
        (shown, members.is_some())
    }

    fn history_expanded(&self, key: &ScKey, session_focused: bool) -> bool {
        !self
            .sidebar
            .source_control()
            .is_history_collapsed(key, session_focused)
    }

    /// Every section's History block, in section order; none with no repo
    /// registered.
    #[must_use]
    pub fn history_panel(&self) -> Vec<HistoryBlock> {
        if self.sidebar.repos().is_empty() {
            return Vec::new();
        }
        let (shown, focused) = self.history_sections();
        let show_title = shown.len() > 1;
        shown
            .iter()
            .map(|section| {
                self.history.block(
                    section,
                    self.history_expanded(&section.key, focused),
                    show_title,
                )
            })
            .collect()
    }

    /// A daemon message for the History; `true` when it was an error the
    /// History answered for, which nothing else may take.
    pub(crate) fn apply_history(&mut self, msg: &DaemonMessage, cx: &mut Context<Self>) -> bool {
        match self.history.apply(msg) {
            Applied::Ignored => false,
            Applied::Changed => {
                cx.notify();
                false
            }
            Applied::Consumed => {
                cx.notify();
                true
            }
        }
    }

    /// Drops the History of keys that stopped being sections and, while the
    /// panel shows, asks for the first page of every expanded History and
    /// the remote of every shown repo not yet read.
    pub(crate) fn seed_history(&mut self) {
        let (shown, focused) = self.history_sections();
        let keys: Vec<ScKey> = shown.iter().map(|section| section.key.clone()).collect();
        self.history.retain(&keys);
        let ids: Vec<String> = keys.iter().map(ScKey::id).collect();
        self.sc_layout.blocks.retain(|id, _| ids.contains(id));
        let visible = self.sidebar.activity() == Activity::SourceControl
            && !self.sidebar.is_collapsed()
            && !self.sidebar.repos().is_empty();
        if !visible {
            return;
        }
        let expanded: Vec<ScKey> = keys
            .into_iter()
            .filter(|key| self.history_expanded(key, focused))
            .collect();
        let repos: Vec<&str> = shown
            .iter()
            .map(|section| section.key.repo_id.as_str())
            .collect();
        let reads = self.history.request_missing(&expanded, &repos);
        for msg in reads {
            self.send(msg);
        }
    }

    /// Refresh: every expanded History reads again from its first page, and
    /// every remote is looked up again.
    pub(crate) fn refresh_history(&mut self) {
        let (shown, focused) = self.history_sections();
        let expanded: Vec<ScKey> = shown
            .into_iter()
            .map(|section| section.key)
            .filter(|key| self.history_expanded(key, focused))
            .collect();
        for msg in self.history.refresh(&expanded) {
            self.send(msg);
        }
        self.seed_history();
    }

    /// A click on a History header: folds or unfolds it, saving the choice.
    fn toggle_history(&mut self, key: &ScKey) {
        let (_, focused) = self.history_sections();
        let collapse = self.history_expanded(key, focused);
        if self.sidebar.set_sc_collapsed(key, Part::History, collapse) {
            self.save_ui();
        }
        if collapse {
            self.history.collapse(key);
        }
        self.seed_history();
    }

    fn click_more(&mut self, key: &ScKey) {
        if let Some(msg) = self.history.load_more(key) {
            self.send(msg);
        }
    }

    fn click_commit(&mut self, key: &ScKey, sha: &str) {
        if let Some(msg) = self.history.select(key, sha) {
            self.send(msg);
        }
    }

    /// A click on a file of the detail pane: its diff against the commit,
    /// in the section's tree.
    fn open_commit_file(&mut self, key: &ScKey, sha: &str, path: &str) {
        self.open_diff(key, path, Some(sha.to_owned()));
    }

    fn open_forge(&mut self, url: &str, window: &mut Window, cx: &mut Context<Self>) {
        match open::validate_http_url(url) {
            Ok(url) => self.dispatch_open(OpenJob::Url(url.to_owned()), Then::Nothing, window, cx),
            Err(err) => {
                tracing::warn!("refusing forge link {url}: {err}");
                self.push_toast(ToastKind::Error, COULD_NOT_OPEN, Some(err), cx);
            }
        }
    }

    /// Follows the changes / History split to window height `y`.
    pub(crate) fn drag_sc_split(&mut self, y: f32) {
        if let Some((top, height)) = self.sc_layout.body {
            let next = clamp_split(y - top, height, MIN_CHANGES_SIDE);
            self.sidebar.set_sc_changes_height(next);
        }
    }

    /// Follows the commit list / detail split of section `key_id` to window
    /// height `y`.
    pub(crate) fn drag_history_list(&mut self, key_id: &str, y: f32) {
        if let Some(&(top, height)) = self.sc_layout.blocks.get(key_id) {
            let next = clamp_split(y - top, height, MIN_LIST_SIDE);
            self.sidebar.set_sc_history_list_height(key_id, next);
        }
    }

    /// The panel's body: `changes` on top, then, under a drag handle while
    /// any History is expanded, one History block per section.
    pub(crate) fn sc_split_body(
        &self,
        changes: Vec<AnyElement>,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let (shown, focused) = self.history_sections();
        let show_title = shown.len() > 1;
        let blocks: Vec<(ScKey, HistoryBlock)> = shown
            .iter()
            .map(|section| {
                let expanded = self.history_expanded(&section.key, focused);
                (
                    section.key.clone(),
                    self.history.block(section, expanded, show_title),
                )
            })
            .collect();
        let any_expanded = blocks.iter().any(|(_, block)| block.expanded);
        let top = div()
            .id("sc-changes")
            .debug_selector(|| "sc-changes".to_owned())
            .flex()
            .flex_col()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .children(changes);
        let top = if any_expanded {
            let total = self.sc_layout.body.map(|(_, height)| height);
            top.flex_none()
                .h(px(changes_height(self.sidebar.source_control(), total)))
        } else {
            top.flex_1()
        };
        let handle = any_expanded.then(|| {
            let active = matches!(self.drag, Some(Drag::ScSplit));
            drag_handle("sc-split", false, active)
                .debug_selector(|| "sc-split".to_owned())
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, _, cx| {
                        this.drag = Some(Drag::ScSplit);
                        cx.stop_propagation();
                    }),
                )
        });
        let bottom = div()
            .flex()
            .flex_col()
            .min_h(px(0.0))
            .map(|bottom| {
                if any_expanded {
                    bottom.flex_1()
                } else {
                    bottom.flex_none()
                }
            })
            .children(
                blocks
                    .into_iter()
                    .map(|(key, block)| self.history_block(&key, block, cx)),
            );
        div()
            .id("sc-body")
            .debug_selector(|| "sc-body".to_owned())
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .child(top)
            .children(handle)
            .child(bottom)
            .child(body_probe(cx))
    }

    fn history_block(
        &self,
        key: &ScKey,
        block: HistoryBlock,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let frame = div()
            .flex()
            .flex_col()
            .min_h(px(0.0))
            .border_t_1()
            .border_color(gpui::rgb(BORDER))
            .child(history_header(key, &block, cx));
        if !block.expanded {
            return frame.flex_none().into_any_element();
        }
        let content = match block.body {
            HistoryBody::Commits { rows, more, detail } => {
                self.commits_content(key, &block.id, (rows, more, detail), cx)
            }
            other => {
                let name = format!("sc-history-body-{}", block.id);
                div()
                    .id(SharedString::from(name.clone()))
                    .debug_selector(|| name)
                    .pl(px(ROW_PADDING * 2.0))
                    .pr(px(ROW_PADDING))
                    .pb(px(6.0))
                    .text_color(gpui::rgb(MUTED))
                    .child(other.text().unwrap_or_default().to_owned())
                    .into_any_element()
            }
        };
        frame.flex_1().child(content).into_any_element()
    }

    /// The commit list, and under a drag handle the selected commit's pane.
    fn commits_content(
        &self,
        key: &ScKey,
        id: &str,
        (rows, more, detail): (Vec<CommitRow>, Option<MoreRow>, Option<DetailPane>),
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let list_name = format!("sc-history-list-{id}");
        let list = div()
            .id(SharedString::from(list_name.clone()))
            .debug_selector(|| list_name)
            .flex()
            .flex_col()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .children(rows.into_iter().map(|row| commit_row(key, id, row, cx)))
            .children(more.map(|more| more_row(key, id, &more, cx)));
        let content = div()
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .child(block_probe(id.to_owned(), cx));
        let (Some(detail), Some(sha)) = (detail, self.history.selected(key)) else {
            return content.child(list.flex_1()).into_any_element();
        };
        let total = self.sc_layout.blocks.get(id).map(|(_, height)| *height);
        let height = list_height(self.sidebar.source_control(), key, total);
        let active = matches!(&self.drag, Some(Drag::HistoryList(dragged)) if dragged == id);
        let name = format!("sc-history-split-{id}");
        let drag_id = id.to_owned();
        let handle = drag_handle(SharedString::from(name.clone()), false, active)
            .debug_selector(|| name)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                    this.drag = Some(Drag::HistoryList(drag_id.clone()));
                    cx.stop_propagation();
                }),
            );
        content
            .child(list.flex_none().h(px(height)))
            .child(handle)
            .child(detail_pane(key, id, sha, detail, cx))
            .into_any_element()
    }
}

/// A block's header: the fold toggle, then the forge button.
fn history_header(key: &ScKey, block: &HistoryBlock, cx: &mut Context<RootView>) -> Div {
    let name = format!("sc-history-{}", block.id);
    let caret = if block.expanded { "▾" } else { "▸" };
    let toggle_key = key.clone();
    let toggle = div()
        .id(SharedString::from(name.clone()))
        .debug_selector(|| name)
        .flex()
        .flex_1()
        .min_w(px(0.0))
        .h_full()
        .items_center()
        .gap(px(6.0))
        .cursor_pointer()
        .child(div().flex_none().text_color(gpui::rgb(MUTED)).child(caret))
        .child(
            div()
                .flex_none()
                .font_weight(FontWeight::SEMIBOLD)
                .child(HISTORY_LABEL),
        )
        .when_some(block.title.clone(), |toggle, title| {
            toggle.child(
                div()
                    .min_w(px(0.0))
                    .truncate()
                    .text_color(gpui::rgb(MUTED))
                    .child(title),
            )
        })
        .when_some(block.count, |toggle, count| {
            toggle.child(
                div()
                    .flex_none()
                    .text_color(gpui::rgb(MUTED))
                    .child(count.to_string()),
            )
        })
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.toggle_history(&toggle_key);
            cx.notify();
        }));
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(6.0))
        .h(px(ROW_HEIGHT))
        .px(px(ROW_PADDING))
        .child(toggle)
        .child(forge_button(&block.id, block.forge.clone(), cx))
}

fn forge_button(id: &str, forge: ForgeButton, cx: &mut Context<RootView>) -> Stateful<Div> {
    let name = format!("sc-history-forge-{id}");
    let button = div()
        .id(SharedString::from(name.clone()))
        .debug_selector(|| name)
        .flex()
        .flex_none()
        .items_center()
        .h(px(BUTTON_HEIGHT))
        .px(px(6.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(gpui::rgb(BORDER))
        .text_color(gpui::rgb(MUTED))
        .child(FORGE_LABEL)
        .tooltip(tooltip(forge.tooltip));
    match forge.url {
        Some(url) => button
            .cursor_pointer()
            .hover(|style| style.bg(gpui::rgb(HOVER_BG)).text_color(gpui::rgb(TEXT)))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.open_forge(&url, window, cx);
            })),
        None => button.opacity(0.5),
    }
}

fn commit_row(key: &ScKey, id: &str, row: CommitRow, cx: &mut Context<RootView>) -> Stateful<Div> {
    let name = format!("sc-commit-{id}-{}", row.sha);
    let (key, sha) = (key.clone(), row.sha);
    div()
        .id(SharedString::from(name.clone()))
        .debug_selector(|| name)
        .flex()
        .flex_none()
        .items_center()
        .gap(px(6.0))
        .h(px(ROW_HEIGHT))
        .pl(px(ROW_PADDING * 2.0))
        .pr(px(ROW_PADDING))
        .cursor_pointer()
        .hover(|style| style.bg(gpui::rgb(HOVER_BG)))
        .when(row.selected, |row| {
            row.bg(gpui::rgb(HOVER_BG)).text_color(gpui::rgb(TEXT))
        })
        .child(
            div()
                .flex_none()
                .text_color(gpui::rgb(MUTED))
                .child(row.short_sha),
        )
        .child(div().flex_1().min_w(px(0.0)).truncate().child(row.subject))
        .child(
            div()
                .flex_none()
                .max_w(px(AUTHOR_WIDTH))
                .truncate()
                .text_color(gpui::rgb(MUTED))
                .child(row.author),
        )
        .tooltip(tooltip(row.tooltip))
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.click_commit(&key, &sha);
            cx.notify();
        }))
}

fn more_row(key: &ScKey, id: &str, more: &MoreRow, cx: &mut Context<RootView>) -> Stateful<Div> {
    let name = format!("sc-history-more-{id}");
    let row = div()
        .id(SharedString::from(name.clone()))
        .debug_selector(|| name)
        .flex_none()
        .h(px(ROW_HEIGHT))
        .pl(px(ROW_PADDING * 2.0))
        .pr(px(ROW_PADDING))
        .truncate()
        .text_color(gpui::rgb(MUTED))
        .child(more.text().to_owned());
    if *more == MoreRow::Loading {
        return row;
    }
    let key = key.clone();
    row.cursor_pointer()
        .hover(|style| style.bg(gpui::rgb(HOVER_BG)).text_color(gpui::rgb(TEXT)))
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.click_more(&key);
            cx.notify();
        }))
}

/// The selected commit's pane under the list.
fn detail_pane(
    key: &ScKey,
    id: &str,
    sha: &str,
    detail: DetailPane,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    let name = format!("sc-detail-{id}");
    let pane = div()
        .id(SharedString::from(name.clone()))
        .debug_selector(|| name)
        .flex()
        .flex_col()
        .flex_1()
        .min_h(px(0.0))
        .overflow_y_scroll()
        .px(px(ROW_PADDING))
        .py(px(4.0))
        .gap(px(2.0));
    let view = match detail {
        DetailPane::Loading => {
            return pane
                .text_color(gpui::rgb(MUTED))
                .child(crate::history::LOADING);
        }
        DetailPane::Failed(text) => return pane.text_color(gpui::rgb(MUTED)).child(text),
        DetailPane::Loaded(view) => view,
    };
    let muted = |text: String| div().text_color(gpui::rgb(MUTED)).child(text);
    let files = view.files.into_iter().enumerate().map(|(index, file)| {
        let name = format!("sc-detail-file-{id}-{index}");
        let (key, sha, path) = (key.clone(), sha.to_owned(), file.path.clone());
        div()
            .id(SharedString::from(name.clone()))
            .debug_selector(|| name)
            .flex()
            .flex_none()
            .items_center()
            .gap(px(6.0))
            .h(px(ROW_HEIGHT))
            .px(px(4.0))
            .cursor_pointer()
            .hover(|style| style.bg(gpui::rgb(HOVER_BG)))
            .child(
                div()
                    .flex_none()
                    .text_color(gpui::rgb(MUTED))
                    .child(file.status),
            )
            .child(div().flex_1().min_w(px(0.0)).truncate().child(file.path))
            .when_some(file.tooltip, |row, tip| row.tooltip(tooltip(tip)))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, _| {
                this.open_commit_file(&key, &sha, &path);
            }))
    });
    pane.child(div().font_weight(FontWeight::SEMIBOLD).child(view.heading))
        .child(muted(view.author))
        .child(muted(view.date))
        .when_some(view.body, |pane, body| {
            pane.child(div().py(px(4.0)).child(body))
        })
        .child(muted("Files:".to_owned()))
        .children(files)
        .when_some(view.no_files, |pane, note| {
            pane.child(muted(note.to_owned()))
        })
}

/// An invisible layer that records where the panel's body was laid out, for
/// the split and its clamp. A move redraws, so the frame after the one that
/// found it lays the split out in the real height; the bounds settle, so it
/// stops there.
fn body_probe(cx: &mut Context<RootView>) -> impl IntoElement {
    layout_probe(cx, |layout, laid| {
        let moved = layout.body != Some(laid);
        layout.body = Some(laid);
        moved
    })
}

/// An invisible layer that records where a History block's content was
/// laid out, for its list / detail split and its clamp, redrawing when it
/// moved.
fn block_probe(id: String, cx: &mut Context<RootView>) -> impl IntoElement {
    layout_probe(cx, move |layout, laid| {
        layout.blocks.insert(id, laid) != Some(laid)
    })
}

/// An invisible layer filling its parent that hands its laid-out (top,
/// height) to `record`, and redraws the view once the frame is done when
/// `record` says it moved. gpui drops a notify made while it draws, so the
/// redraw is deferred to after the frame.
fn layout_probe(
    cx: &mut Context<RootView>,
    record: impl FnOnce(&mut ScLayout, (f32, f32)) -> bool + 'static,
) -> impl IntoElement {
    let root = cx.entity();
    canvas(
        move |bounds, _, cx| {
            let laid = (bounds.origin.y / px(1.0), bounds.size.height / px(1.0));
            let moved = root.update(cx, |this, _| record(&mut this.sc_layout, laid));
            if moved {
                cx.defer(move |cx| root.update(cx, |_, cx| cx.notify()));
            }
        },
        |_, (), _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}
