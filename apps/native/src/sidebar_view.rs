//! Renders the activity rail, the panel it picks and the drag divider beside
//! the terminal; the sessions panel is a header, the spawn toolbar, and
//! container rows with their session leaves.

use std::collections::{HashMap, HashSet};

use crate::appearance;
use crate::appearance_view::Level;
use crate::assets::{
    SIDEBAR_BRANCH_ICON, SIDEBAR_CHEVRON_DOWN_ICON, SIDEBAR_CHEVRON_RIGHT_ICON,
    SIDEBAR_FOLDER_ICON, SIDEBAR_MORE_ICON, SIDEBAR_PLUS_ICON, SIDEBAR_SHELL_ICON,
    SIDEBAR_TABS_ICON, SIDEBAR_WORKSPACE_ICON,
};
use crate::buttons::{ButtonSize, outlined_button, primary_button};
use crate::ellipsis::{ellipsized, truncating};
use crate::fonts::{DEFAULT_FAMILY, UI_FAMILY};
use crate::grid_view::{NO_REPOS_TIP, SPAWN_TIP};
use crate::palette::{
    ACCENT, ASKING, CHIP, HOVER, LILAC, LINE, LINE_STRONG, ON_ACCENT, ON_ASKING, RAISED, SUBTLE,
    TEXT_2,
};
use crate::session_actions::inline_actions;
use crate::session_menu::{menu_frame, menu_item};
use crate::sidebar::{
    Activity, Container, ContainerKind, Leaf, LeafDensity, SidebarView, can_attach,
};
use crate::spawn_view::SpawnEntry;
use crate::status_glyph::{GlyphSize, glyph, glyph_view};
use crate::tabs::{TabPill, tab_pills};
use crate::{Drag, HOVER_BG, MUTED, PANEL_BG, RootView, TEXT, UI_TEXT_SIZE, drag_handle, tooltip};
use gpui::{
    AnyElement, ClickEvent, Context, Corner, Div, FontWeight, MouseButton, MouseDownEvent,
    SharedString, Stateful, StyledText, Svg, TextRun, Window, anchored, deferred, div, font, point,
    prelude::*, px, svg,
};
use protocol::{Agent, SessionMode, SessionStatus};

pub(crate) const ROW_HEIGHT: f32 = 22.0;
pub(crate) const ROW_PADDING: f32 = 8.0;
const LEAF_INDENT: f32 = 22.0;
const TAG_TEXT_SIZE: f32 = 10.0;
const ACCENT_STRIPE_WIDTH: f32 = 3.0;
/// How far the stripe sits from the leaf's left, top and bottom edges.
const STRIPE_LEFT: f32 = 3.0;
const STRIPE_INSET_Y: f32 = 7.0;
/// The sessions body's inset: 8 px at the sides, 2 px at the top and
/// bottom.
const BODY_INSET_X: f32 = 8.0;
const BODY_INSET_Y: f32 = 2.0;
/// A leaf's left padding, before its glyph.
const LEAF_PAD_LEFT: f32 = 12.0;
/// A Compact leaf's one line.
const COMPACT_LEAF_HEIGHT: f32 = 26.0;
/// The subline's and chip row's indent under the label, past the glyph.
const SUBLINE_INDENT: f32 = 20.0;
const SUBLINE_TEXT_SIZE: f32 = 11.5;
/// The gap between the subline's text and chips.
const SUBLINE_GAP: f32 = 8.0;
/// The widest a `T:<name>` chip grows before its name truncates.
const PILL_MAX_WIDTH: f32 = 96.0;

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
                Activity::Sessions => self.sidebar_panel(width, window, cx),
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

    fn sidebar_panel(&self, width: f32, window: &Window, cx: &mut Context<Self>) -> Div {
        let attached = self.focused_session();
        let containers = self.sidebar_containers();
        let body = div()
            .id("sidebar-body")
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .px(px(BODY_INSET_X))
            .py(px(BODY_INSET_Y))
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
                density: self.sidebar.leaf_density(),
                subline_room: width
                    - 2.0 * BODY_INSET_X
                    - LEAF_PAD_LEFT
                    - ROW_PADDING
                    - SUBLINE_INDENT,
            };
            let groups: Vec<AnyElement> = containers
                .iter()
                .map(|container| container_rows(container, &rows, window, cx))
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
            .child(self.toolbar(cx))
            .child(body)
    }

    /// The spawns under the header, in one row: Session, Shell and the ⋯
    /// menu.
    fn toolbar(&self, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(6.0))
            .pt(px(10.0))
            .px(px(10.0))
            .pb(px(8.0))
            .child(add_session(self.has_repos(), cx))
            .child(add_shell(self.sidebar.quick_shell_dir(), cx))
            .child(self.more_button(cx))
    }

    /// The ⋯ button, with its menu under it while open.
    fn more_button(&self, cx: &mut Context<Self>) -> Div {
        let button = div()
            .id(MORE_BUTTON)
            .debug_selector(|| MORE_BUTTON.to_owned())
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .size(px(MORE_BUTTON_SIZE))
            .rounded(px(6.0))
            .cursor_pointer()
            .hover(|style| style.bg(gpui::rgb(HOVER)))
            .tooltip(tooltip(MORE_TIP))
            .child(icon(SIDEBAR_MORE_ICON, 15.0, TEXT_2))
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.open_more_menu(window, cx);
            }));
        let menu = self.sidebar_more_open().then(|| {
            let row = menu_item(MORE_SHELL_ROW, "Shell…", false).on_click(cx.listener(
                |this, _: &ClickEvent, window, cx| {
                    this.close_more_menu(window, cx);
                    this.open_shell_dialog(window, cx);
                },
            ));
            let frame = menu_frame(
                "sidebar-more-menu",
                &self.menu_focus,
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    this.close_more_menu(window, cx);
                    cx.stop_propagation();
                }),
            )
            .child(row);
            let panel = anchored()
                .anchor(Corner::TopRight)
                .offset(point(px(MORE_BUTTON_SIZE), px(MORE_BUTTON_SIZE + 2.0)))
                .snap_to_window()
                .child(frame);
            deferred(panel).with_priority(1)
        });
        div().relative().flex_none().child(button).children(menu)
    }

    /// Whether the toolbar's ⋯ menu is open.
    #[must_use]
    pub fn sidebar_more_open(&self) -> bool {
        self.more_menu == MoreMenu::Open
    }

    /// Opens the ⋯ menu, taking the keyboard so Esc reaches it; any other
    /// menu gives way.
    fn open_more_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_session_menu(window, cx);
        self.close_shell_menu(window, cx);
        self.close_tab_menu(window, cx);
        self.close_container_menu(window, cx);
        self.close_sc_picker(window, cx);
        self.close_sc_file_menu(window, cx);
        self.more_menu = MoreMenu::Open;
        self.menu_focus.focus(window);
        cx.notify();
    }

    /// Closes the ⋯ menu and hands the keyboard back to the active pane.
    pub(crate) fn close_more_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if std::mem::replace(&mut self.more_menu, MoreMenu::Closed) == MoreMenu::Open {
            self.focus_active_pane(window, cx);
            cx.notify();
        }
    }

    /// The layer under the open ⋯ menu that keeps a click outside it from
    /// reaching what lies beneath.
    pub(crate) fn more_menu_layer(&self) -> Option<AnyElement> {
        self.sidebar_more_open().then(|| {
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .occlude()
                .into_any_element()
        })
    }
}

/// Whether the toolbar's ⋯ menu is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MoreMenu {
    Closed,
    Open,
}

/// The ⋯ button's selector.
const MORE_BUTTON: &str = "sidebar-more";
/// The ⋯ menu's one row, which opens the shell folder dialog.
const MORE_SHELL_ROW: &str = "sidebar-more-shell-dialog";
/// The ⋯ button's hover text.
const MORE_TIP: &str = "More: Shell…";
/// The ⋯ button's side, in px.
const MORE_BUTTON_SIZE: f32 = 30.0;

/// A `size` px square icon from `path`, drawn in `color`.
fn icon(path: &'static str, size: f32, color: u32) -> Svg {
    svg()
        .path(path)
        .flex_none()
        .size(px(size))
        .text_color(gpui::rgb(color))
}

/// Session, which opens the spawn dialog, with its `Ctrl N` hint; disabled
/// with no repo, when the hint still shows.
fn add_session(has_repos: bool, cx: &mut Context<RootView>) -> Stateful<Div> {
    let tip = if has_repos { SPAWN_TIP } else { NO_REPOS_TIP };
    let hint = div()
        .debug_selector(|| "sidebar-session-hint".to_owned())
        .flex_none()
        .ml(px(2.0))
        .font_family(DEFAULT_FAMILY)
        .text_size(px(10.0))
        .opacity(0.7)
        .child("Ctrl N");
    primary_button("sidebar-add-session", ButtonSize::Toolbar, has_repos)
        .flex_1()
        .min_w(px(0.0))
        .overflow_hidden()
        .gap(px(6.0))
        .child(icon(SIDEBAR_PLUS_ICON, 14.0, ON_ACCENT))
        .child("Session")
        .child(hint)
        .tooltip(tooltip(tip))
        .when(has_repos, |button| {
            button.on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.open_spawn_dialog(SpawnEntry::Toolbar, window, cx);
            }))
        })
}

/// Shell, a standalone shell in the remembered folder; enabled even with no
/// repo.
fn add_shell(quick_shell_dir: Option<&str>, cx: &mut Context<RootView>) -> Stateful<Div> {
    let tip = match quick_shell_dir {
        Some(dir) => format!("Open a standalone shell in {dir}"),
        None => "Open a standalone shell in your home folder".to_owned(),
    };
    outlined_button("sidebar-add-shell", ButtonSize::Toolbar, true)
        .flex_none()
        .gap(px(6.0))
        .child(icon(SIDEBAR_SHELL_ICON, 14.0, TEXT))
        .child("Shell")
        .tooltip(tooltip(tip))
        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.quick_shell(cx)))
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
    let segments = choices.map(|(choice, label, tip, selector)| {
        let active = choice == view;
        div()
            .id(selector)
            .debug_selector(|| selector.to_owned())
            .flex()
            .items_center()
            .h(px(22.0))
            .px(px(9.0))
            .rounded(px(4.0))
            .text_size(px(11.5))
            .cursor_pointer()
            .tooltip(tooltip(tip))
            .when(active, |segment| {
                segment
                    .bg(gpui::rgb(HOVER))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(gpui::rgb(TEXT))
            })
            .when(!active, |segment| {
                segment
                    .text_color(gpui::rgb(TEXT_2))
                    .hover(|style| style.text_color(gpui::rgb(TEXT)))
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
        .gap(px(8.0))
        .h(px(44.0))
        .pl(px(14.0))
        .pr(px(10.0))
        .border_b_1()
        .border_color(gpui::rgb(LINE))
        .child(
            div()
                .flex_1()
                .text_size(px(13.0))
                .font_weight(FontWeight::BOLD)
                .text_color(gpui::rgb(TEXT))
                .child("Sessions"),
        )
        .child(
            div()
                .flex()
                .flex_none()
                .p(px(2.0))
                .rounded(px(6.0))
                .bg(gpui::rgb(RAISED))
                .children(segments),
        )
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
    window: &Window,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let mut rows = vec![container_row(container, cx).into_any_element()];
    if !container.collapsed {
        if container.kind == ContainerKind::Unbound {
            rows.push(unbound_banner().into_any_element());
        }
        for leaf in &container.leaves {
            rows.push(leaf_row(leaf, ctx, window, cx).into_any_element());
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
    density: LeafDensity,
    /// The width a Comfortable subline's text and chips share, in px.
    subline_room: f32,
}

impl LeafRows<'_> {
    /// Session `id`'s tab pill; `None` until the tab list is in.
    fn pill(&self, id: &str) -> Option<TabPill> {
        self.pills
            .as_ref()
            .map(|pills| pills.get(id).cloned().unwrap_or(TabPill::Unbound))
    }

    /// Whether session `id` can be shown in a pane.
    fn attachable(&self, id: &str) -> bool {
        !self.headless.contains(id)
    }
}

/// The hover of an unbound pill on a session that cannot be shown in a pane.
const HEADLESS_PILL_TIP: &str =
    "Headless sessions run without a terminal, so they can't be opened in a tab";

/// A leaf chip's size: at the subline's end, or in the chip row and a
/// Compact line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChipSize {
    Subline,
    Row,
}

impl ChipSize {
    const fn height(self) -> f32 {
        match self {
            Self::Subline => 17.0,
            Self::Row => 20.0,
        }
    }

    const fn padding(self) -> f32 {
        match self {
            Self::Subline => 5.0,
            Self::Row => 7.0,
        }
    }

    const fn text_size(self) -> f32 {
        match self {
            Self::Subline => 10.0,
            Self::Row => 11.0,
        }
    }
}

/// An outlined Geist Mono chip of `size`, lettered in [`SUBTLE`].
fn chip<E: Styled>(element: E, size: ChipSize) -> E {
    element
        .flex()
        .flex_none()
        .items_center()
        .h(px(size.height()))
        .px(px(size.padding()))
        .rounded(px(5.0))
        .border_1()
        .border_color(gpui::rgb(LINE_STRONG))
        .font_family(DEFAULT_FAMILY)
        .text_size(px(size.text_size()))
        .text_color(gpui::rgb(SUBTLE))
}

/// A chip's lettering, ending in `…` when the chip is capped. `id` names it
/// for the ellipsis probe.
fn chip_text(id: &str, text: String) -> Div {
    ellipsized_box().children(ellipsized(
        id,
        SharedString::from(text.clone()),
        StyledText::new(text),
    ))
}

/// The box a content-sized text sits in when it may shrink: the invisible
/// copy sizes it and the drawn copy lies over it.
fn ellipsized_box() -> Div {
    div()
        .relative()
        .min_w(px(0.0))
        .overflow_hidden()
        .whitespace_nowrap()
}

/// A tab pill's lettering.
fn pill_text(pill: &TabPill) -> String {
    match pill {
        TabPill::Unbound => "unbound".to_owned(),
        TabPill::One { name, .. } => format!("T:{name}"),
        TabPill::Many(names) => format!("T:×{}", names.len()),
    }
}

/// A leaf's tab pill: `unbound` opens the session in a new tab when it can
/// be shown in a pane; `T:<name>` and `T:×N` say where it is shown and do
/// nothing of their own.
fn leaf_pill(
    pill: &TabPill,
    session_id: &str,
    attachable: bool,
    size: ChipSize,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let selector = format!("leaf-pill-{session_id}");
    let text = chip_text(&selector, pill_text(pill));
    let base = chip(
        div()
            .id(SharedString::from(selector.clone()))
            .debug_selector(move || selector),
        size,
    )
    .child(text);
    let tip = pill.hover();
    match pill {
        TabPill::Unbound if !attachable => {
            base.tooltip(tooltip(HEADLESS_PILL_TIP)).into_any_element()
        }
        TabPill::Unbound => {
            let id = session_id.to_owned();
            base.cursor_pointer()
                .hover(|style| style.bg(gpui::rgb(HOVER)))
                .tooltip(tooltip(tip))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.open_in_new_tab(&id);
                    cx.stop_propagation();
                }))
                .into_any_element()
        }
        TabPill::One { .. } => base
            .max_w(px(PILL_MAX_WIDTH))
            .tooltip(tooltip(tip))
            .into_any_element(),
        TabPill::Many(_) => base
            .bg(gpui::rgb(RAISED))
            .border_color(gpui::rgb(RAISED))
            .text_color(gpui::rgb(TEXT_2))
            .tooltip(tooltip(tip))
            .into_any_element(),
    }
}

/// The leaf's chips: its tab pill, when the tab list is in, then its
/// orphan, abandoned or parked tag.
fn leaf_chips(
    leaf: &Leaf,
    pill: Option<TabPill>,
    attachable: bool,
    size: ChipSize,
    cx: &mut Context<RootView>,
) -> Vec<AnyElement> {
    let mut chips: Vec<AnyElement> = pill
        .map(|pill| leaf_pill(&pill, &leaf.id, attachable, size, cx))
        .into_iter()
        .collect();
    if let Some((text, tip)) = leaf.state.tag() {
        let name = format!("leaf-state-{}", leaf.id);
        chips.push(
            chip(div().id(SharedString::from(name.clone())), size)
                .tooltip(tooltip(tip))
                .child(chip_text(&name, text))
                .into_any_element(),
        );
    }
    chips
}

/// The width `text` takes in `family` at `size` px, in px.
fn text_width(window: &Window, text: &str, family: &'static str, size: f32) -> f32 {
    let run = TextRun {
        len: text.len(),
        font: font(family),
        color: gpui::black(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let line = window.text_system().shape_line(
        SharedString::from(text.to_owned()),
        px(size),
        &[run],
        None,
    );
    line.width / px(1.0)
}

/// Whether the subline's `text` and the subline-sized `chips` (their
/// lettering) fit side by side in `room` px.
fn chips_fit(window: &Window, text: Option<&str>, chips: &[String], room: f32) -> bool {
    if chips.is_empty() {
        return true;
    }
    let size = ChipSize::Subline;
    let chip_frame = 2.0 * size.padding() + 2.0;
    let chips_width: f32 = chips
        .iter()
        .map(|chip| {
            let width = text_width(window, chip, DEFAULT_FAMILY, size.text_size()) + chip_frame;
            width.min(PILL_MAX_WIDTH) + SUBLINE_GAP
        })
        .sum::<f32>()
        - SUBLINE_GAP;
    let text_width = text.map_or(0.0, |text| {
        text_width(window, text, UI_FAMILY, SUBLINE_TEXT_SIZE) + SUBLINE_GAP
    });
    text_width + chips_width <= room
}

/// The chevron before a container's name: right while folded, down while
/// open, none (an empty slot) for a tab with no sessions.
fn chevron(container: &Container) -> AnyElement {
    let path = if container.kind == ContainerKind::Tab && container.leaves.is_empty() {
        None
    } else if container.collapsed {
        Some(SIDEBAR_CHEVRON_RIGHT_ICON)
    } else {
        Some(SIDEBAR_CHEVRON_DOWN_ICON)
    };
    match path {
        Some(path) => icon(path, 13.0, SUBTLE).into_any_element(),
        None => div().flex_none().size(px(13.0)).into_any_element(),
    }
}

/// A container kind's icon, the selector suffix naming it and its colour;
/// none for Unbound.
fn kind_icon(kind: ContainerKind) -> Option<(&'static str, &'static str, u32)> {
    match kind {
        ContainerKind::Workspace => Some((SIDEBAR_WORKSPACE_ICON, "ws", LILAC)),
        ContainerKind::Repo => Some((SIDEBAR_BRANCH_ICON, "repo", TEXT_2)),
        ContainerKind::Shell => Some((SIDEBAR_SHELL_ICON, "shell", TEXT_2)),
        ContainerKind::Dir | ContainerKind::Detached => {
            Some((SIDEBAR_FOLDER_ICON, "folder", TEXT_2))
        }
        ContainerKind::Tab => Some((SIDEBAR_TABS_ICON, "tabs", TEXT_2)),
        ContainerKind::Unbound => None,
    }
}

/// A container row's parts after the chevron: the kind icon, the name, the
/// attention badge, the kind tag and the session count, each tagged
/// `container-<key>-<part>`.
fn container_parts(container: &Container) -> Vec<AnyElement> {
    let part = |suffix: &str| format!("container-{}-{suffix}", container.key);
    let mut parts = Vec::new();
    if let Some((path, suffix, color)) = kind_icon(container.kind) {
        let selector = part(&format!("icon-{suffix}"));
        parts.push(
            div()
                .debug_selector(|| selector)
                .flex()
                .flex_none()
                .child(icon(path, 14.0, color))
                .into_any_element(),
        );
    }
    parts.push(
        truncating(&part("name"), StyledText::new(container.name.clone()))
            .flex_1()
            .text_size(px(12.5))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(gpui::rgb(TEXT))
            .into_any_element(),
    );
    if container.attention {
        parts.push(container_badge(part("badge")).into_any_element());
    }
    let tag = part("tag");
    parts.push(
        div()
            .debug_selector(|| tag)
            .flex_none()
            .font_family(DEFAULT_FAMILY)
            .text_size(px(TAG_TEXT_SIZE))
            .text_color(gpui::rgb(SUBTLE))
            .child(container.kind.tag())
            .into_any_element(),
    );
    let count = part("count");
    parts.push(
        div()
            .debug_selector(|| count)
            .flex_none()
            .min_w(px(14.0))
            .text_right()
            .text_size(px(11.0))
            .text_color(gpui::rgb(SUBTLE))
            .child(container.leaves.len().to_string())
            .into_any_element(),
    );
    parts
}

/// The `!` badge of a container holding a session that needs attention.
fn container_badge(selector: String) -> Div {
    div()
        .debug_selector(|| selector)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(16.0))
        .rounded(px(4.0))
        .bg(gpui::rgb(ASKING))
        .text_color(gpui::rgb(ON_ASKING))
        .text_size(px(11.0))
        .font_weight(FontWeight::BOLD)
        .child("!")
}

/// A container row: a click folds it; a right-click on a repo or
/// workspace opens its menu.
fn container_row(container: &Container, cx: &mut Context<RootView>) -> Stateful<Div> {
    let key = container.key.clone();
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
        .flex_none()
        .items_center()
        .gap(px(7.0))
        .h(px(30.0))
        .pl(px(4.0))
        .pr(px(8.0))
        .cursor_pointer()
        .hover(|style| style.bg(gpui::rgb(HOVER_BG)))
        .child(chevron(container))
        .children(container_parts(container))
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

/// A session's leaf at the panel's density: a stripe in its accent down
/// its left edge, the label line, and in Comfortable the subline and, when
/// the chips do not fit beside it, the chip row.
fn leaf_row(
    leaf: &Leaf,
    ctx: &LeafRows<'_>,
    window: &Window,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    let id = leaf.id.as_str();
    let selected = ctx.attached == Some(id);
    let accent = ctx
        .accents
        .get(id)
        .copied()
        .unwrap_or(appearance::BUILTIN_ACCENT);
    let row = leaf_frame(leaf, selected, accent, ctx.density, cx);
    match ctx.density {
        LeafDensity::Compact => {
            let chips = leaf_chips(leaf, ctx.pill(id), ctx.attachable(id), ChipSize::Row, cx);
            row.flex_row()
                .items_center()
                .gap(px(8.0))
                .h(px(COMPACT_LEAF_HEIGHT))
                .children(label_head(leaf))
                .children(inline_buttons(leaf, cx))
                .children(chips)
        }
        LeafDensity::Comfortable => {
            let label_line = div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .children(label_head(leaf))
                .children(inline_buttons(leaf, cx));
            let lower = comfortable_lower(leaf, ctx, window, cx);
            row.flex_col().py(px(6.0)).child(label_line).children(lower)
        }
    }
}

/// The leaf's box: selection and hover grounds, its hover text, the accent
/// stripe, and the clicks.
fn leaf_frame(
    leaf: &Leaf,
    selected: bool,
    accent: u32,
    density: LeafDensity,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    let id = leaf.id.clone();
    let menu_id = leaf.id.clone();
    let name = format!("leaf-{}", leaf.id);
    div()
        .id(SharedString::from(name.clone()))
        .debug_selector(|| name)
        .relative()
        .flex()
        .flex_none()
        .my(px(1.0))
        .pl(px(LEAF_PAD_LEFT))
        .pr(px(ROW_PADDING))
        .rounded(px(6.0))
        .cursor_pointer()
        .when(selected, |row| row.bg(gpui::rgb(CHIP)))
        .when(!selected, |row| {
            row.hover(|style| style.bg(gpui::rgb(RAISED)))
        })
        .tooltip(tooltip(leaf.hover(density)))
        .child(
            div()
                .absolute()
                .left(px(STRIPE_LEFT))
                .top(px(STRIPE_INSET_Y))
                .bottom(px(STRIPE_INSET_Y))
                .w(px(ACCENT_STRIPE_WIDTH))
                .rounded(px(2.0))
                .bg(gpui::rgb(accent)),
        )
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                this.open_session_menu(&menu_id, None, event.position, window, cx);
                cx.stop_propagation();
            }),
        )
        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
            this.select_session(&id, window, cx);
        }))
}

/// The label line's start: the status glyph, the label (dimmed once an
/// idle session's turn has been seen), the `!` badge and the runtime tag.
fn label_head(leaf: &Leaf) -> Vec<AnyElement> {
    let seen_idle = leaf.status == SessionStatus::Idle && !leaf.unseen;
    let mut parts = vec![
        glyph_view(
            glyph(leaf.status, leaf.mode, leaf.unseen),
            GlyphSize::Leaf,
            &format!("leaf-glyph-{}", leaf.id),
        ),
        truncating(
            &format!("leaf-{}-label", leaf.id),
            StyledText::new(leaf.label.clone()),
        )
        .flex_1()
        .text_size(px(13.0))
        .font_weight(FontWeight::MEDIUM)
        .text_color(gpui::rgb(if seen_idle { TEXT_2 } else { TEXT }))
        .into_any_element(),
    ];
    if leaf.attention {
        parts.push(container_badge(format!("leaf-badge-{}", leaf.id)).into_any_element());
    }
    if let Some((text, tip)) = leaf.runtime_tag() {
        parts.push(
            div()
                .id(SharedString::from(format!("leaf-runtime-{}", leaf.id)))
                .flex_none()
                .font_family(DEFAULT_FAMILY)
                .text_size(px(10.5))
                .text_color(gpui::rgb(runtime_color(leaf)))
                .child(text)
                .tooltip(tooltip(tip))
                .into_any_element(),
        );
    }
    parts
}

/// The runtime tag's colour: the agent's, or [`SUBTLE`] for a shell's
/// program.
fn runtime_color(leaf: &Leaf) -> u32 {
    if leaf.mode == SessionMode::PlainShell {
        return SUBTLE;
    }
    match leaf.agent {
        Agent::Claude => ACCENT,
        Agent::Codex => LILAC,
        Agent::Cursor => TEXT_2,
    }
}

/// A Comfortable leaf's subline, with the chips at its end when they fit
/// beside its text, else the chip row under it, where an `unbound` pill is
/// left out.
fn comfortable_lower(
    leaf: &Leaf,
    ctx: &LeafRows<'_>,
    window: &Window,
    cx: &mut Context<RootView>,
) -> Vec<AnyElement> {
    let pill = ctx.pill(&leaf.id);
    let attachable = ctx.attachable(&leaf.id);
    let texts: Vec<String> = pill
        .iter()
        .map(pill_text)
        .chain(leaf.state.tag().map(|(text, _)| text))
        .collect();
    let fits = chips_fit(window, leaf.subline.as_deref(), &texts, ctx.subline_room);
    let (inline, below) = if fits {
        (
            leaf_chips(leaf, pill, attachable, ChipSize::Subline, cx),
            Vec::new(),
        )
    } else {
        let pill = pill.filter(|pill| *pill != TabPill::Unbound);
        (
            Vec::new(),
            leaf_chips(leaf, pill, attachable, ChipSize::Row, cx),
        )
    };
    let mut lower = Vec::new();
    if leaf.subline.is_some() || !inline.is_empty() {
        let selector = format!("leaf-subline-{}", leaf.id);
        let text_id = format!("{selector}-label");
        lower.push(
            div()
                .debug_selector(|| selector)
                .flex()
                .items_center()
                .gap(px(SUBLINE_GAP))
                .mt(px(1.0))
                .pl(px(SUBLINE_INDENT))
                .text_size(px(SUBLINE_TEXT_SIZE))
                .text_color(gpui::rgb(SUBTLE))
                .children(leaf.subline.clone().map(|text| {
                    ellipsized_box().children(ellipsized(
                        &text_id,
                        SharedString::from(text.clone()),
                        StyledText::new(text),
                    ))
                }))
                .children(inline)
                .into_any_element(),
        );
    }
    if !below.is_empty() {
        let selector = format!("leaf-chips-{}", leaf.id);
        lower.push(
            div()
                .debug_selector(|| selector)
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(6.0))
                .mt(px(3.0))
                .pl(px(SUBLINE_INDENT))
                .children(below)
                .into_any_element(),
        );
    }
    lower
}

/// An inline action's 20 px chip, outlined in [`LINE_STRONG`].
fn action_chip(element: Stateful<Div>) -> Stateful<Div> {
    element
        .flex()
        .flex_none()
        .items_center()
        .h(px(20.0))
        .px(px(7.0))
        .rounded(px(5.0))
        .border_1()
        .border_color(gpui::rgb(LINE_STRONG))
        .text_size(px(11.0))
        .text_color(gpui::rgb(TEXT))
        .cursor_pointer()
        .hover(|style| style.bg(gpui::rgb(HOVER)))
}

/// The leaf's Resume and Dismiss. They act on the press and keep it from
/// the row, so the row's click never shows the session.
fn inline_buttons(leaf: &Leaf, cx: &mut Context<RootView>) -> Vec<AnyElement> {
    inline_actions(&leaf.state)
        .into_iter()
        .map(|inline| {
            let selector = format!("leaf-{}-{}", inline.key, leaf.id);
            let id = leaf.id.clone();
            action_chip(
                div()
                    .id(SharedString::from(selector.clone()))
                    .debug_selector(|| selector),
            )
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

    /// Leaf `id`'s hover text at the chosen density. `None` when it is not
    /// listed.
    #[must_use]
    pub fn leaf_hover(&self, id: &str) -> Option<String> {
        self.listed_leaf(id)
            .map(|leaf| leaf.hover(self.sidebar.leaf_density()))
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
