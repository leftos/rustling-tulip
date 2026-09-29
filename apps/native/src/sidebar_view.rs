//! Renders the activity rail, the panel it picks and the drag divider beside
//! the terminal; the sessions panel is a header, the spawn toolbar, and
//! container rows with their session leaves.

use std::collections::{HashMap, HashSet};

use crate::appearance;
use crate::appearance_view::Level;
use crate::connection::DotKind;
use crate::grid_view::{NO_REPOS_TIP, SPAWN_TIP};
use crate::palette::SELECTED_BG;
use crate::session_actions::inline_actions;
use crate::sidebar::{Activity, Container, ContainerKind, Leaf, SidebarView, can_attach};
use crate::spawn_view::SpawnEntry;
use crate::status_glyph::{GlyphSize, glyph, glyph_view};
use crate::tabs::{TabPill, tab_pills};
use crate::{
    BORDER, Drag, HOVER_BG, MUTED, PANEL_BG, RootView, TEXT, UI_TEXT_SIZE, WARNING, dot_color,
    drag_handle, tooltip,
};
use gpui::{
    AnyElement, ClickEvent, Context, Div, FontWeight, MouseButton, MouseDownEvent, SharedString,
    Stateful, Window, div, prelude::*, px,
};

pub(crate) const ROW_HEIGHT: f32 = 22.0;
pub(crate) const ROW_PADDING: f32 = 8.0;
const LEAF_INDENT: f32 = 22.0;
const TAG_TEXT_SIZE: f32 = 10.0;
const ACCENT_STRIPE_WIDTH: f32 = 3.0;

impl RootView {
    /// The rail, the panel it picks with its divider, and the terminal pane
    /// side by side; a folded panel leaves the rail alone.
    pub(crate) fn main_row(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        let row = div()
            .flex()
            .flex_row()
            .flex_1()
            .min_h(px(0.0))
            .child(self.activity_rail(cx));
        let row = if self.sidebar.is_collapsed() {
            row
        } else {
            let width = self.sidebar.width(window.viewport_size().width / px(1.0));
            let active = matches!(self.drag, Some(Drag::Sidebar));
            let panel = match self.sidebar.activity() {
                Activity::Sessions => self.sidebar_panel(width, cx),
                Activity::SourceControl => self.source_control_view(width, cx),
                Activity::NeedsYou => self.needs_you_view(width, cx),
            };
            row.child(panel).child(divider(active, cx))
        };
        row.child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.0))
                .h_full()
                .child(self.tab_bar(cx))
                .child(self.grid_area(cx)),
        )
    }

    fn sidebar_panel(&self, width: f32, cx: &mut Context<Self>) -> Div {
        let attached = self.focused_session();
        let containers = self.sidebar_containers();
        let body = div()
            .id("sidebar-body")
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll();
        let body = if containers.is_empty() {
            body.child(
                div()
                    .px(px(ROW_PADDING))
                    .py(px(6.0))
                    .text_color(gpui::rgb(MUTED))
                    .child("No sessions"),
            )
        } else {
            let rows = LeafRows {
                attached: attached.as_deref(),
                accents: self.sidebar.session_accents(),
                pills: self.tabs.is_loaded().then(|| tab_pills(self.tabs.tabs())),
                headless: self
                    .sidebar
                    .sessions()
                    .iter()
                    .filter(|s| !can_attach(s))
                    .map(|s| s.id.as_str())
                    .collect(),
            };
            let groups: Vec<AnyElement> = containers
                .iter()
                .map(|container| container_rows(container, &rows, cx))
                .collect();
            body.children(groups)
        };
        div()
            .flex()
            .flex_col()
            .flex_none()
            .w(px(width))
            .h_full()
            .debug_selector(|| "sidebar-panel".to_owned())
            .track_focus(&self.sidebar_focus)
            .bg(gpui::rgb(PANEL_BG))
            .text_size(px(UI_TEXT_SIZE))
            .text_color(gpui::rgb(TEXT))
            .child(header(self.sidebar.sidebar_view(), cx))
            .child(toolbar(
                self.has_repos(),
                self.sidebar.quick_shell_dir(),
                cx,
            ))
            .child(body)
    }
}

/// "+ Session", which opens the spawn dialog; disabled with no repo.
fn add_session(has_repos: bool, cx: &mut Context<RootView>) -> Stateful<Div> {
    let tip = if has_repos { SPAWN_TIP } else { NO_REPOS_TIP };
    div()
        .id("sidebar-add-session")
        .debug_selector(|| "sidebar-add-session".to_owned())
        .px(px(6.0))
        .rounded(px(4.0))
        .child("+ Session")
        .tooltip(tooltip(tip))
        .when(has_repos, |button| {
            button
                .cursor_pointer()
                .hover(|style| style.bg(gpui::rgb(HOVER_BG)).text_color(gpui::rgb(TEXT)))
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                    this.open_spawn_dialog(SpawnEntry::Toolbar, window, cx);
                }))
        })
        .when(!has_repos, |button| button.opacity(0.5))
}

/// "+ Shell", a standalone shell in the remembered folder; enabled even
/// with no repo.
fn add_shell(quick_shell_dir: Option<&str>, cx: &mut Context<RootView>) -> Stateful<Div> {
    let tip = match quick_shell_dir {
        Some(dir) => format!("Open a standalone shell in {dir}"),
        None => "Open a standalone shell in your home folder".to_owned(),
    };
    div()
        .id("sidebar-add-shell")
        .debug_selector(|| "sidebar-add-shell".to_owned())
        .px(px(6.0))
        .rounded(px(4.0))
        .child("+ Shell")
        .tooltip(tooltip(tip))
        .cursor_pointer()
        .hover(|style| style.bg(gpui::rgb(HOVER_BG)).text_color(gpui::rgb(TEXT)))
        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.quick_shell(cx)))
}

/// "Shell…", which opens the folder dialog; enabled even with no repo.
fn add_shell_dialog(cx: &mut Context<RootView>) -> Stateful<Div> {
    div()
        .id("sidebar-shell-dialog")
        .debug_selector(|| "sidebar-shell-dialog".to_owned())
        .px(px(6.0))
        .rounded(px(4.0))
        .child("Shell…")
        .tooltip(tooltip("Choose a folder for a standalone shell"))
        .cursor_pointer()
        .hover(|style| style.bg(gpui::rgb(HOVER_BG)).text_color(gpui::rgb(TEXT)))
        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
            this.open_shell_dialog(window, cx);
        }))
}

/// The panel's title and the Repos / Tabs grouping toggle.
fn header(view: SidebarView, cx: &mut Context<RootView>) -> Div {
    let choices = [
        (
            SidebarView::Repos,
            "Repos",
            "Group by workspace/repo",
            "sidebar-view-repos",
        ),
        (
            SidebarView::Tabs,
            "Tabs",
            "Group by tab",
            "sidebar-view-tabs",
        ),
    ];
    let buttons = choices.map(|(choice, label, tip, selector)| {
        let active = choice == view;
        div()
            .id(selector)
            .debug_selector(|| selector.to_owned())
            .px(px(6.0))
            .rounded(px(4.0))
            .text_size(px(TAG_TEXT_SIZE + 1.0))
            .cursor_pointer()
            .tooltip(tooltip(tip))
            .when(active, |button| {
                button
                    .bg(gpui::rgb(SELECTED_BG))
                    .text_color(gpui::rgb(TEXT))
            })
            .when(!active, |button| {
                button.hover(|style| style.bg(gpui::rgb(HOVER_BG)).text_color(gpui::rgb(TEXT)))
            })
            .child(label)
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.set_sidebar_view(choice, cx);
            }))
    });
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(2.0))
        .h(px(ROW_HEIGHT + 4.0))
        .px(px(ROW_PADDING))
        .border_b_1()
        .border_color(gpui::rgb(BORDER))
        .text_color(gpui::rgb(MUTED))
        .child(
            div()
                .flex_1()
                .font_weight(FontWeight::SEMIBOLD)
                .child("Sessions"),
        )
        .children(buttons)
}

/// The spawns, under the header; the row wraps when the sidebar is too
/// narrow to hold them in one line.
fn toolbar(has_repos: bool, quick_shell_dir: Option<&str>, cx: &mut Context<RootView>) -> Div {
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap(px(4.0))
        .px(px(ROW_PADDING))
        .py(px(4.0))
        .border_b_1()
        .border_color(gpui::rgb(BORDER))
        .text_color(gpui::rgb(MUTED))
        .child(add_session(has_repos, cx))
        .child(add_shell(quick_shell_dir, cx))
        .child(add_shell_dialog(cx))
}

/// The drag handle; a press starts a resize the root follows until release.
fn divider(active: bool, cx: &mut Context<RootView>) -> Stateful<Div> {
    drag_handle("sidebar-divider", true, active)
        .debug_selector(|| "sidebar-divider".to_owned())
        .on_mouse_down(MouseButton::Left, cx.listener(RootView::start_drag))
}

/// A container row, then (unless it is collapsed) the Unbound banner and
/// its leaves; grouped under the container's key, since the tabs view can
/// list a session under more than one tab.
fn container_rows(
    container: &Container,
    ctx: &LeafRows<'_>,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let mut rows = vec![container_row(container, cx).into_any_element()];
    if !container.collapsed {
        if container.kind == ContainerKind::Unbound {
            rows.push(unbound_banner().into_any_element());
        }
        for leaf in &container.leaves {
            let id = leaf.id.as_str();
            let selected = ctx.attached == Some(id);
            let accent = ctx
                .accents
                .get(id)
                .copied()
                .unwrap_or(appearance::BUILTIN_ACCENT);
            let pill = ctx.pills.as_ref().map(|pills| {
                let pill = pills.get(id).cloned().unwrap_or(TabPill::Unbound);
                leaf_pill(pill, id, accent, !ctx.headless.contains(id), cx)
            });
            rows.push(leaf_row(leaf, selected, accent, pill, cx).into_any_element());
        }
    }
    div()
        .id(SharedString::from(format!("group-{}", container.key)))
        .flex()
        .flex_col()
        .flex_none()
        .children(rows)
        .into_any_element()
}

/// The text under an expanded Unbound container, with the pill it names.
fn unbound_banner() -> Div {
    div()
        .debug_selector(|| "unbound-banner".to_owned())
        .flex()
        .flex_wrap()
        .items_center()
        .gap_x(px(3.0))
        .pl(px(LEAF_INDENT))
        .pr(px(ROW_PADDING))
        .py(px(4.0))
        .text_size(px(TAG_TEXT_SIZE + 1.0))
        .text_color(gpui::rgb(MUTED))
        .child("These sessions are alive but no tab currently references them. Click the")
        .child(unbound_pill_look(div()).child("unbound"))
        .child("pill on a session to open it in a new tab.")
}

/// The unbound pill's look: dim italic text in a dim border.
fn unbound_pill_look<E: Styled>(pill: E) -> E {
    pill.flex_none()
        .px(px(4.0))
        .rounded(px(3.0))
        .border_1()
        .border_color(gpui::rgb(MUTED))
        .text_size(px(TAG_TEXT_SIZE))
        .text_color(gpui::rgb(MUTED))
        .italic()
        .opacity(0.8)
}

/// What every leaf row of one render shares.
struct LeafRows<'a> {
    /// The session the active pane shows.
    attached: Option<&'a str>,
    accents: HashMap<&'a str, u32>,
    /// Each shown session's pill; `None` until the daemon's tab list is in,
    /// when every session would look unbound.
    pills: Option<HashMap<String, TabPill>>,
    /// The sessions that cannot be shown in a pane.
    headless: HashSet<&'a str>,
}

/// The hover of an unbound pill on a session that cannot be shown in a pane.
const HEADLESS_PILL_TIP: &str =
    "Headless sessions run without a terminal, so they can't be opened in a tab";

/// A leaf's tab pill: `unbound` opens the session in a new tab when it can
/// be shown in a pane; `T:<name>` and `T:×N` say where it is shown and do
/// nothing of their own.
fn leaf_pill(
    pill: TabPill,
    session_id: &str,
    accent: u32,
    attachable: bool,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let selector = format!("leaf-pill-{session_id}");
    let base = div()
        .id(SharedString::from(selector.clone()))
        .debug_selector(move || selector);
    let tip = pill.hover();
    match pill {
        TabPill::Unbound if !attachable => unbound_pill_look(base)
            .tooltip(tooltip(HEADLESS_PILL_TIP))
            .child("unbound")
            .into_any_element(),
        TabPill::Unbound => {
            let id = session_id.to_owned();
            unbound_pill_look(base)
                .cursor_pointer()
                .hover(|style| style.bg(gpui::rgb(HOVER_BG)))
                .tooltip(tooltip(tip))
                .child("unbound")
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.open_in_new_tab(&id);
                    cx.stop_propagation();
                }))
                .into_any_element()
        }
        TabPill::One { name, .. } => bordered_pill(base, (MUTED << 8) | 0x80, MUTED)
            .tooltip(tooltip(tip))
            .child(format!("T:{name}"))
            .into_any_element(),
        TabPill::Many(names) => bordered_pill(base, (accent << 8) | 0xff, accent)
            .bg(gpui::rgba((accent << 8) | 0x33))
            .tooltip(tooltip(tip))
            .child(format!("T:×{}", names.len()))
            .into_any_element(),
    }
}

/// A pill in a `border` (`0xRRGGBBAA`) outline with `text` (`0xRRGGBB`)
/// lettering.
fn bordered_pill(pill: Stateful<Div>, border: u32, text: u32) -> Stateful<Div> {
    pill.flex_none()
        .max_w(px(96.0))
        .truncate()
        .px(px(4.0))
        .rounded(px(3.0))
        .border_1()
        .border_color(gpui::rgba(border))
        .text_size(px(TAG_TEXT_SIZE))
        .text_color(gpui::rgb(text))
}

/// A container row: a click folds it; a right-click on a repo or
/// workspace opens its menu. A tab with no sessions shows no fold chip.
fn container_row(container: &Container, cx: &mut Context<RootView>) -> Stateful<Div> {
    let key = container.key.clone();
    let chip = if container.kind == ContainerKind::Tab && container.leaves.is_empty() {
        ""
    } else if container.collapsed {
        "▸"
    } else {
        "▾"
    };
    let name = format!("container-{}", container.key);
    let level = match container.kind {
        ContainerKind::Repo => Some(Level::Repo(container.id.clone())),
        ContainerKind::Workspace => Some(Level::Workspace(container.id.clone())),
        ContainerKind::Shell
        | ContainerKind::Dir
        | ContainerKind::Detached
        | ContainerKind::Tab
        | ContainerKind::Unbound => None,
    };
    div()
        .id(SharedString::from(name.clone()))
        .debug_selector(|| name)
        .flex()
        .items_center()
        .gap(px(6.0))
        .h(px(ROW_HEIGHT))
        .px(px(ROW_PADDING))
        .cursor_pointer()
        .hover(|style| style.bg(gpui::rgb(HOVER_BG)))
        .child(
            div()
                .flex_none()
                .w(px(10.0))
                .text_color(gpui::rgb(MUTED))
                .child(chip),
        )
        .child(
            div()
                .flex_none()
                .text_size(px(TAG_TEXT_SIZE))
                .text_color(gpui::rgb(MUTED))
                .child(container.kind.tag()),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .font_weight(FontWeight::SEMIBOLD)
                .child(container.name.clone()),
        )
        .when(container.attention, |row| row.child(attention_mark()))
        .child(
            div()
                .flex_none()
                .text_color(gpui::rgb(MUTED))
                .child(container.leaves.len().to_string()),
        )
        .when_some(container.hover.clone(), |row, tip| {
            row.tooltip(tooltip(tip))
        })
        .when_some(level, |row, level| {
            row.on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.open_container_menu(level.clone(), event.position, window, cx);
                    cx.stop_propagation();
                }),
            )
        })
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.toggle_container(&key);
            cx.notify();
        }))
}

/// How a leaf row shows that its session needs attention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeafHighlight {
    None,
    /// A soft amber fill and an amber border.
    Attention,
    /// As [`Self::Attention`], with a deeper fill and an amber stripe in
    /// place of the accent, on the selected leaf.
    AttentionSelected,
}

impl LeafHighlight {
    const fn of(attention: bool, selected: bool) -> Self {
        match (attention, selected) {
            (false, _) => Self::None,
            (true, false) => Self::Attention,
            (true, true) => Self::AttentionSelected,
        }
    }
}

/// [`WARNING`] at `alpha`, as `0xRRGGBBAA`.
const fn warning_alpha(alpha: u32) -> u32 {
    (WARNING << 8) | alpha
}

impl RootView {
    /// How leaf `id` is highlighted for attention; `None` when it is not
    /// listed or needs none.
    #[must_use]
    pub fn leaf_highlight(&self, id: &str) -> LeafHighlight {
        let attention = self
            .sidebar
            .containers()
            .iter()
            .flat_map(|container| &container.leaves)
            .any(|leaf| leaf.id == id && leaf.attention);
        LeafHighlight::of(attention, self.focused_session().as_deref() == Some(id))
    }
}

/// A session's row, with a stripe in its accent down its left edge; a
/// session needing attention is filled and bordered in amber.
fn leaf_row(
    leaf: &Leaf,
    selected: bool,
    accent: u32,
    pill: Option<AnyElement>,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    let id = leaf.id.clone();
    let menu_id = leaf.id.clone();
    let name = format!("leaf-{}", leaf.id);
    let highlight = LeafHighlight::of(leaf.attention, selected);
    let (background, border, stripe) = match highlight {
        LeafHighlight::None => (None, 0, accent),
        LeafHighlight::Attention => (Some(warning_alpha(0x24)), warning_alpha(0x61), accent),
        LeafHighlight::AttentionSelected => {
            (Some(warning_alpha(0x38)), warning_alpha(0x61), WARNING)
        }
    };
    div()
        .id(SharedString::from(name.clone()))
        .debug_selector(|| name)
        .relative()
        .flex()
        .items_center()
        .gap(px(6.0))
        .h(px(ROW_HEIGHT))
        .pl(px(LEAF_INDENT))
        .pr(px(ROW_PADDING))
        .cursor_pointer()
        .hover(|style| style.bg(gpui::rgb(HOVER_BG)))
        .when(selected, |row| row.bg(gpui::rgb(SELECTED_BG)))
        .when_some(background, |row, fill| row.bg(gpui::rgba(fill)))
        .border_1()
        .border_color(gpui::rgba(border))
        .tooltip(tooltip(leaf.tooltip.clone()))
        .child(glyph_view(
            glyph(leaf.status, leaf.mode, leaf.unseen),
            GlyphSize::Leaf,
            &format!("leaf-glyph-{}", leaf.id),
        ))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .child(leaf.label.clone()),
        )
        .children(leaf_tags(leaf))
        .children(inline_buttons(leaf, cx))
        .when(leaf.attention, |row| row.child(attention_mark()))
        .children(pill)
        .child(
            div()
                .absolute()
                .left(px(-1.0))
                .top(px(-1.0))
                .bottom(px(-1.0))
                .w(px(ACCENT_STRIPE_WIDTH))
                .bg(gpui::rgb(stripe)),
        )
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                this.open_session_menu(&menu_id, event.position, window, cx);
                cx.stop_propagation();
            }),
        )
        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
            this.select_session(&id, window, cx);
        }))
}

/// The leaf's runtime and state tags, each with its hover text.
fn leaf_tags(leaf: &Leaf) -> Vec<AnyElement> {
    leaf.tags()
        .into_iter()
        .enumerate()
        .map(|(i, (text, tip))| {
            div()
                .id(SharedString::from(format!("leaf-tag-{}-{i}", leaf.id)))
                .flex_none()
                .text_size(px(TAG_TEXT_SIZE))
                .text_color(gpui::rgb(MUTED))
                .child(text)
                .tooltip(tooltip(tip))
                .into_any_element()
        })
        .collect()
}

/// The leaf's Resume and Dismiss. They act on the press and keep it from
/// the row, so the row's click never shows the session.
fn inline_buttons(leaf: &Leaf, cx: &mut Context<RootView>) -> Vec<AnyElement> {
    inline_actions(&leaf.state)
        .into_iter()
        .map(|inline| {
            let selector = format!("leaf-{}-{}", inline.key, leaf.id);
            let id = leaf.id.clone();
            div()
                .id(SharedString::from(selector.clone()))
                .debug_selector(|| selector)
                .flex_none()
                .px(px(4.0))
                .rounded(px(3.0))
                .border_1()
                .border_color(gpui::rgb(BORDER))
                .text_size(px(TAG_TEXT_SIZE))
                .text_color(gpui::rgb(TEXT))
                .cursor_pointer()
                .hover(|style| style.bg(gpui::rgb(HOVER_BG)))
                .child(inline.action.label(false, false))
                .tooltip(tooltip(inline.tip))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        this.choose_action(&id, inline.action, window, cx);
                    }),
                )
                .into_any_element()
        })
        .collect()
}

impl RootView {
    fn listed_leaf(&self, id: &str) -> Option<Leaf> {
        self.sidebar
            .containers()
            .into_iter()
            .flat_map(|container| container.leaves)
            .find(|leaf| leaf.id == id)
    }

    /// Leaf `id`'s tags as text and hover text: its runtime, then its
    /// orphan, abandoned or parked state. Empty when it is not listed.
    #[must_use]
    pub fn leaf_tags(&self, id: &str) -> Vec<(String, String)> {
        self.listed_leaf(id)
            .map(|leaf| leaf.tags())
            .unwrap_or_default()
    }

    /// Leaf `id`'s inline buttons: selector and hover text.
    #[must_use]
    pub fn leaf_buttons(&self, id: &str) -> Vec<(String, String)> {
        self.listed_leaf(id).map_or_else(Vec::new, |leaf| {
            inline_actions(&leaf.state)
                .into_iter()
                .map(|inline| (format!("leaf-{}-{id}", inline.key), inline.tip.to_owned()))
                .collect()
        })
    }
}

fn attention_mark() -> Div {
    div()
        .flex_none()
        .font_weight(FontWeight::BOLD)
        .text_color(gpui::rgb(dot_color(DotKind::Pending)))
        .child("!")
}
