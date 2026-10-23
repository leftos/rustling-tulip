//! Renders the activity rail, the panel it picks and the drag divider beside
//! the terminal; the sessions panel is a header, the spawn toolbar, and
//! container rows with their session leaves.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::Pick;
use crate::appearance;
use crate::assets::{
    SIDEBAR_BRANCH_ICON, SIDEBAR_CHEVRON_DOWN_ICON, SIDEBAR_CHEVRON_RIGHT_ICON,
    SIDEBAR_FOLDER_ICON, SIDEBAR_MORE_ICON, SIDEBAR_PLAY_ICON, SIDEBAR_PLUS_ICON,
    SIDEBAR_SHELL_ICON, SIDEBAR_TABS_ICON, SIDEBAR_WORKSPACE_ICON,
};
use crate::buttons::{ButtonSize, focus_ring, outlined_button, primary_button};
use crate::ellipsis::{ellipsized, truncating};
use crate::fonts::{DEFAULT_FAMILY, UI_FAMILY};
use crate::grid_view::{ADD_REPO_TIP, NO_REPOS_TIP, SPAWN_TIP};
use crate::launch_last;
use crate::palette::{
    ACCENT, ASKING, CHIP, GROUND, HOVER, LAUNCHED_TINT, LILAC, LINE, LINE_STRONG, ON_ACCENT,
    ON_ASKING, RAISED, SUBTLE, TEXT_2, TRANSPARENT,
};
use crate::preset_launch::LaunchHighlight;
use crate::remote_mode::{Capabilities, sidebar_no_repos_note};
use crate::repo_add::RepoAdd;
use crate::session_actions::inline_actions;
use crate::session_menu::disabled_row;
use crate::session_menu::{
    BorderedButton, MENU_MAX_SHARE, MenuContainer, bordered_button, menu_frame, menu_item,
    menu_separator,
};
use crate::sidebar::{
    Activity, Container, ContainerKind, DETACHED_BANNER, Leaf, LeafDensity, SidebarView, can_attach,
};
use crate::spawn_form::{provider_chip_text, provider_chip_tip};
use crate::spawn_view::SpawnEntry;
use crate::status_glyph::{GlyphSize, glyph, glyph_view};
use crate::tabs::{TabPill, tab_pills};
use crate::target::Target;
use crate::{
    DANGER, Drag, HOVER_BG, MUTED, PANEL_BG, RootView, TEXT, UI_TEXT_SIZE, drag_handle,
    tagged_text, tooltip,
};
use gpui::{
    AnyElement, Bounds, ClickEvent, Context, Corner, Corners, Div, FontWeight, MouseButton,
    MouseDownEvent, PathBuilder, Pixels, SharedString, Stateful, StyledText, Svg, TextRun, Window,
    anchored, canvas, deferred, div, font, point, prelude::*, px, svg,
};
use protocol::{Agent, ClientMessage, SessionMode, SessionStatus};

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
    pub(crate) fn main_row(&mut self, window: &Window, cx: &mut Context<Self>) -> Div {
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
                Activity::SourceControl => self.source_control_view(width, window, cx),
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
                .bg(gpui::rgb(GROUND))
                .child(self.tab_bar(cx))
                .child(self.grid_area(cx)),
        )
    }

    /// The fill the main area paints: the ground, so no window or desktop
    /// shows through the tab bar's strip, an empty main area, the pane
    /// gutter or the first-run dialog's backdrop.
    #[must_use]
    pub fn main_area_fill(&self) -> u32 {
        GROUND
    }

    fn sidebar_panel(&mut self, width: f32, window: &Window, cx: &mut Context<Self>) -> Div {
        let attached = self.focused_session();
        let containers = self.sidebar_containers();
        // The chip is the only way to arm, so an arm cannot outlive it.
        if !detached_stop_all_shown(&containers) {
            self.detached_stop_all.disarm();
        }
        let body = div()
            .id("sidebar-body")
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .px(px(BODY_INSET_X))
            .py(px(BODY_INSET_Y))
            .overflow_y_scroll();
        // With no repo or workspace, the note leads and the folder and shell
        // sessions still list under it; "No sessions" is for a registry
        // with nothing to list.
        let no_repos = self.no_repos_yet();
        let body = body.when(no_repos, |body| {
            body.child(no_repos_note(self.add_repo_gate(), self.conn.target(), cx))
        });
        let body = if containers.is_empty() {
            body.when(!no_repos, |body| {
                body.child(
                    div()
                        .px(px(ROW_PADDING))
                        .py(px(6.0))
                        .text_color(gpui::rgb(MUTED))
                        .child("No sessions"),
                )
            })
        } else {
            // The ring and the row keys follow the keyboard: a row the
            // sidebar no longer holds (a menu or a pane took over) draws
            // none.
            let focused_row = self
                .sidebar_focus
                .is_focused(window)
                .then_some(self.row_focus.as_deref())
                .flatten();
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
                focused_row,
                hovered_row: self.hovered_container.as_deref(),
                detached_stop_all: self.detached_stop_all.armed(),
                highlighted: &self.launched_highlight,
                subline_room: width
                    - 2.0 * BODY_INSET_X
                    - LEAF_PAD_LEFT
                    - ROW_PADDING
                    - SUBLINE_INDENT,
            };
            let groups: Vec<AnyElement> = containers
                .iter()
                .map(|container| {
                    let add_workspace = container
                        .path
                        .as_deref()
                        .and_then(|path| self.cwd_workspace_suggestion(path))
                        .map(|suggestion| suggestion.suggested_name.clone());
                    container_rows(container, add_workspace.as_deref(), &rows, window, cx)
                })
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
            .child(self.toolbar(window, cx))
            .child(body)
    }

    /// The spawns under the header, in one row: Session, Shell and the ⋯
    /// menu.
    fn toolbar(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(6.0))
            .pt(px(10.0))
            .px(px(10.0))
            .pb(px(8.0))
            .child(add_session(self.has_repos(), cx))
            .child(add_shell(self.quick_shell_default(), cx))
            .child(self.more_button(window, cx))
    }

    /// The ⋯ button, with its menu under it while open.
    fn more_button(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        // The tip names the rows the menu actually shows: Resume all is
        // suppressed below two abandoned sessions.
        let tip = if crate::sidebar::resume_all_label(self.sidebar.abandoned_count()).is_some() {
            MORE_TIP
        } else {
            MORE_TIP_WITHOUT_RESUME
        };
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
            .tooltip(tooltip(tip))
            .child(icon(SIDEBAR_MORE_ICON, 15.0, TEXT_2))
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.open_more_menu(window, cx);
            }));
        let menu = self.sidebar_more_open().then(|| {
            let shell = menu_item(MORE_SHELL_ROW, "Shell…", false).on_click(cx.listener(
                |this, _: &ClickEvent, window, cx| {
                    this.close_more_menu(window, cx);
                    this.open_shell_dialog(window, cx);
                },
            ));
            let add_repo = self.more_add_repo_row(cx);
            let add_workspace = if self.sidebar.repos().len() < 2 {
                div()
                    .id(MORE_ADD_WORKSPACE_ROW)
                    .debug_selector(|| MORE_ADD_WORKSPACE_ROW.to_owned())
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .px(px(8.0))
                    .py(px(3.0))
                    .rounded(px(4.0))
                    .text_color(gpui::rgb(MUTED))
                    .child("Add workspace…")
                    .child(
                        div()
                            .debug_selector(|| "sidebar-more-add-workspace-hint".to_owned())
                            .text_size(px(11.0))
                            .child(NEEDS_TWO_REPOS),
                    )
            } else {
                menu_item(MORE_ADD_WORKSPACE_ROW, "Add workspace…", false).on_click(cx.listener(
                    |this, _: &ClickEvent, window, cx| {
                        this.close_more_menu(window, cx);
                        this.open_workspace_creator(window, cx);
                    },
                ))
            };
            let resume =
                crate::sidebar::resume_all_label(self.sidebar.abandoned_count()).map(|label| {
                    menu_item(MORE_RESUME_ALL_ROW, label, false).on_click(cx.listener(
                        |this, _: &ClickEvent, window, cx| {
                            this.close_more_menu(window, cx);
                            this.send(ClientMessage::ResumeAllAbandoned);
                        },
                    ))
                });
            let frame = menu_frame(
                "sidebar-more-menu",
                &self.menu_focus,
                window.viewport_size().height * MENU_MAX_SHARE,
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    this.close_more_menu(window, cx);
                    cx.stop_propagation();
                }),
            )
            .child(shell)
            .child(add_repo)
            .child(add_workspace);
            let frame = if let Some(row) = resume {
                frame
                    .child(menu_separator().debug_selector(|| MORE_RESUME_ALL_SEPARATOR.to_owned()))
                    .child(row)
            } else {
                frame
            };
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

    /// Whether the daemon's registry is in and holds no repo or workspace,
    /// which the sessions panel notes over an Add repo button.
    #[must_use]
    pub fn no_repos_yet(&self) -> bool {
        self.sidebar.repos_loaded()
            && self.sidebar.repos().is_empty()
            && self.sidebar.workspaces().is_empty()
    }

    /// The ⋯ menu's Add repo… row, drawn as [`Self::add_repo_gate`] says.
    fn more_add_repo_row(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let gate = self.add_repo_gate();
        if !gate.enabled {
            return disabled_row(MORE_ADD_REPO_ROW, "Add repo…", gate.tip);
        }
        menu_item(MORE_ADD_REPO_ROW, "Add repo…", false).on_click(cx.listener(
            |this, _: &ClickEvent, window, cx| {
                this.close_more_menu(window, cx);
                this.add_repo(cx);
            },
        ))
    }

    /// How every Add repo control is drawn on the daemon connected to now.
    pub(crate) fn add_repo_gate(&self) -> AddRepoGate {
        AddRepoGate::for_caps(self.capabilities())
    }

    /// Add repo: the picker opens at the folder the last added repo sits
    /// in, and the folder picked is registered with the daemon. A cancel
    /// sends nothing; a refusal comes back as the daemon's error toast.
    pub(crate) fn add_repo(&mut self, cx: &mut Context<Self>) {
        let start = self.sidebar.last_repo_dir().map(PathBuf::from);
        self.pick_local(Pick::Folder, start, cx, |this, picked, _| {
            this.finish_add_repo(picked);
        });
    }

    /// The picker's answer: the folder beside it is remembered and the
    /// folder registered.
    fn finish_add_repo(&mut self, picked: Option<PathBuf>) {
        let Some(add) = RepoAdd::from_pick(picked) else {
            return;
        };
        if self.sidebar.set_last_repo_dir(&add.last_dir) {
            self.save_ui();
        }
        tracing::info!("adding repo {} as {}", add.path, add.name);
        self.send(add.message());
    }

    /// Add repo for a DIR or SH container's folder: the folder is registered
    /// straight away, with no picker and no change to the next pick's folder.
    pub(crate) fn add_repo_at(&mut self, path: String, _cx: &mut Context<Self>) {
        tracing::info!("adding repo {path}");
        self.send(ClientMessage::AddRepo { path, name: None });
    }

    /// Add workspace for a DIR or SH container's folder: accepts the
    /// `.code-workspace` its scan found, without watching it.
    pub(crate) fn add_workspace_from_scan(&mut self, path: &str, _cx: &mut Context<Self>) {
        let Some(suggestion) = self.cwd_workspace_suggestion(path).cloned() else {
            return;
        };
        self.send(ClientMessage::AcceptVscodeWorkspaceSuggestion {
            suggestion,
            watch: false,
        });
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

/// Whether an Add repo control takes a press, and its tooltip. Add repo
/// opens this PC's folder picker, so where `caps` opens no local files it
/// is disabled with the reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AddRepoGate {
    pub(crate) enabled: bool,
    pub(crate) tip: &'static str,
}

impl AddRepoGate {
    pub(crate) fn for_caps(caps: Capabilities) -> Self {
        match caps.local_files_refusal() {
            Some(reason) => Self {
                enabled: false,
                tip: reason,
            },
            None => Self {
                enabled: true,
                tip: ADD_REPO_TIP,
            },
        }
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
/// The ⋯ menu's row that opens the shell folder dialog.
const MORE_SHELL_ROW: &str = "sidebar-more-shell-dialog";
/// The ⋯ menu's row that picks a folder to register as a repo.
const MORE_ADD_REPO_ROW: &str = "sidebar-more-add-repo";
/// The ⋯ menu's row that opens the workspace creator.
const MORE_ADD_WORKSPACE_ROW: &str = "sidebar-more-add-workspace";
/// The note the workspace row carries while fewer than two repos exist.
const NEEDS_TWO_REPOS: &str = "needs 2 repos";
/// The ⋯ menu's row that resumes every abandoned session.
const MORE_RESUME_ALL_ROW: &str = "more-resume-all";
/// The line above that row.
const MORE_RESUME_ALL_SEPARATOR: &str = "more-resume-all-separator";
/// The ⋯ button's hover text while Resume all is shown.
const MORE_TIP: &str = "More: Shell…, Add repo…, Add workspace…, Resume all";
/// The ⋯ button's hover text while fewer than two abandoned sessions leave
/// Resume all off the menu.
const MORE_TIP_WITHOUT_RESUME: &str = "More: Shell…, Add repo…, Add workspace…";

/// The note an empty registry shows, with the Add repo button under it,
/// drawn as `gate` says, in the wording `target` calls for.
fn no_repos_note(gate: AddRepoGate, target: &Target, cx: &mut Context<RootView>) -> Div {
    let add = BorderedButton {
        selector: "sidebar-add-repo".to_owned(),
        label: "Add repo",
        tip: gate.tip,
        enabled: gate.enabled,
    };
    div()
        .debug_selector(|| "sidebar-no-repos".to_owned())
        .flex()
        .flex_col()
        .items_start()
        .gap(px(6.0))
        .px(px(ROW_PADDING))
        .py(px(6.0))
        .child(
            tagged_text("sidebar-no-repos-note", sidebar_no_repos_note(target))
                .text_color(gpui::rgb(MUTED)),
        )
        .child(bordered_button(add, cx, |this, _, cx| this.add_repo(cx)))
}
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

/// A container row, then (unless it is collapsed) the folder banner of a DIR
/// or SH container, the Unbound or Detached container's banner, and its
/// leaves; grouped under the container's key, since the tabs view can list a
/// session under more than one tab.
fn container_rows(
    container: &Container,
    add_workspace: Option<&str>,
    ctx: &LeafRows<'_>,
    window: &Window,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let focused = ctx.focused_row == Some(container.key.as_str());
    let hovered = ctx.hovered_row == Some(container.key.as_str());
    let mut rows = vec![container_row(container, focused, hovered, cx).into_any_element()];
    if !container.collapsed {
        if matches!(container.kind, ContainerKind::Dir | ContainerKind::Shell)
            && let Some(path) = container.path.as_deref()
        {
            rows.push(path_banner(container, path, add_workspace, cx).into_any_element());
        }
        if container.kind == ContainerKind::Unbound {
            rows.push(unbound_banner().into_any_element());
        }
        if container.kind == ContainerKind::Detached {
            rows.push(detached_banner(container, ctx.detached_stop_all, cx).into_any_element());
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

/// The folder under an expanded DIR or SH container, with the actions its
/// container offers: Add repo always, Add workspace once a scan found a
/// `.code-workspace` file there.
fn path_banner(
    container: &Container,
    path: &str,
    add_workspace: Option<&str>,
    cx: &mut Context<RootView>,
) -> Div {
    let key = container.key.clone();
    let banner_selector = format!("container-{key}-banner");
    let repo_selector = format!("container-{key}-add-repo");
    let repo_path = path.to_owned();
    let repo_tip = format!("Add repo: {path}");
    let banner = div()
        .debug_selector(move || banner_selector.clone())
        .flex()
        .flex_wrap()
        .items_center()
        .gap_x(px(6.0))
        .pl(px(LEAF_INDENT))
        .pr(px(ROW_PADDING))
        .py(px(4.0))
        .text_size(px(TAG_TEXT_SIZE + 1.0))
        .text_color(gpui::rgb(MUTED))
        .child(div().flex_1().min_w(px(0.0)).child(truncating(
            &format!("container-{key}-path"),
            StyledText::new(path.to_owned()),
        )))
        .child(
            banner_link(repo_selector, "Add repo", repo_tip).on_click(cx.listener(
                move |this, _: &ClickEvent, _window, cx| {
                    this.add_repo_at(repo_path.clone(), cx);
                },
            )),
        );
    let Some(name) = add_workspace else {
        return banner;
    };
    let workspace_selector = format!("container-{key}-add-workspace");
    let workspace_path = path.to_owned();
    let workspace_tip = format!("Add workspace: {name}");
    banner.child(
        banner_link(workspace_selector, "Add workspace", workspace_tip).on_click(cx.listener(
            move |this, _: &ClickEvent, _window, cx| {
                this.add_workspace_from_scan(&workspace_path, cx);
            },
        )),
    )
}

/// A banner action: plain text that reads as a link, naming what it acts on
/// in its hover text.
fn banner_link(selector: String, label: &'static str, tip: String) -> Stateful<Div> {
    div()
        .id(SharedString::from(selector.clone()))
        .debug_selector(move || selector.clone())
        .flex_none()
        .cursor_pointer()
        .text_color(gpui::rgb(TEXT_2))
        .hover(|style| style.text_color(gpui::rgb(TEXT)))
        .tooltip(tooltip(tip))
        .child(label)
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

/// The Detached banner's Stop all waiting for its second click.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum DetachedStopAll {
    #[default]
    Idle,
    Armed,
}

impl DetachedStopAll {
    /// Whether the chip shows its confirming label.
    pub(crate) const fn armed(self) -> bool {
        matches!(self, Self::Armed)
    }

    /// A click with no argument carrying whether it was armed on entry: the
    /// first click arms it, the second disarms it.
    pub(crate) fn click(&mut self) -> bool {
        let armed = self.armed();
        *self = if armed { Self::Idle } else { Self::Armed };
        armed
    }

    /// Returns whether it was armed.
    pub(crate) fn disarm(&mut self) -> bool {
        let armed = self.armed();
        *self = Self::Idle;
        armed
    }
}

/// The Detached banner's Stop all chip: selector, both labels and each
/// state's hover text.
const DETACHED_STOP_ALL_SELECTOR: &str = "detached-stop-all";
const DETACHED_STOP_ALL: &str = "Stop all";
const DETACHED_STOP_ALL_ARMED: &str = "Confirm: stop all";
const DETACHED_STOP_ALL_TIP: &str = "Stop every session still running in this container";
const DETACHED_STOP_ALL_ARMED_TIP: &str = "Click again to stop every running session";

/// Whether the Detached container's banner shows its Stop all chip: the
/// container is expanded and one of its leaves still runs.
fn detached_stop_all_shown(containers: &[Container]) -> bool {
    containers.iter().any(|container| {
        container.kind == ContainerKind::Detached
            && !container.collapsed
            && container
                .leaves
                .iter()
                .any(|leaf| leaf.status != SessionStatus::Stopped)
    })
}

/// The text under an expanded Detached container, with its Stop all chip
/// while one of its leaves still runs.
fn detached_banner(container: &Container, armed: bool, cx: &mut Context<RootView>) -> Div {
    let live: Vec<String> = container
        .leaves
        .iter()
        .filter(|leaf| leaf.status != SessionStatus::Stopped)
        .map(|leaf| leaf.id.clone())
        .collect();
    let banner = div()
        .debug_selector(|| "detached-banner".to_owned())
        .flex()
        .flex_wrap()
        .items_center()
        .gap_x(px(3.0))
        .pl(px(LEAF_INDENT))
        .pr(px(ROW_PADDING))
        .py(px(4.0))
        .text_size(px(TAG_TEXT_SIZE + 1.0))
        .text_color(gpui::rgb(MUTED));
    if live.is_empty() {
        return banner.child(DETACHED_BANNER);
    }
    banner
        .child(div().flex_1().min_w(px(0.0)).child(DETACHED_BANNER))
        .child(detached_stop_all_chip(live, armed, cx))
}

/// The Detached banner's Stop all chip: the first click arms it, the second
/// stops every leaf in `live`.
fn detached_stop_all_chip(
    live: Vec<String>,
    armed: bool,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let tip = if armed {
        DETACHED_STOP_ALL_ARMED_TIP
    } else {
        DETACHED_STOP_ALL_TIP
    };
    chip(
        div()
            .id(SharedString::from(DETACHED_STOP_ALL_SELECTOR))
            .debug_selector(|| DETACHED_STOP_ALL_SELECTOR.to_owned()),
        ChipSize::Row,
    )
    .cursor_pointer()
    .text_color(gpui::rgb(if armed { DANGER } else { TEXT_2 }))
    .hover(|style| style.bg(gpui::rgb(HOVER)))
    .tooltip(tooltip(tip))
    .child(if armed {
        DETACHED_STOP_ALL_ARMED
    } else {
        DETACHED_STOP_ALL
    })
    .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
        this.click_detached_stop_all(&live, cx);
    }))
    .into_any_element()
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
    /// The container row the sidebar's keys act on, if any.
    focused_row: Option<&'a str>,
    /// The container row the pointer is over, if any.
    hovered_row: Option<&'a str>,
    /// The Detached banner's Stop all is armed.
    detached_stop_all: bool,
    /// The sessions Select launched marked.
    highlighted: &'a LaunchHighlight,
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

/// The leaf's chips: its tab pill, when the tab list is in, then the
/// session's provider, then its orphan, abandoned or parked tag.
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
    if let Some(provider) = leaf.provider {
        let name = format!("leaf-provider-{}", leaf.id);
        let selector = name.clone();
        chips.push(
            chip(
                div()
                    .id(SharedString::from(name.clone()))
                    .debug_selector(move || selector),
                size,
            )
            .bg(gpui::rgb(CHIP))
            .border_color(gpui::rgba(TRANSPARENT))
            .text_color(gpui::rgb(TEXT_2))
            .tooltip(tooltip(provider_chip_tip(provider)))
            .child(chip_text(&name, provider_chip_text(provider).to_owned()))
            .into_any_element(),
        );
    }
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

/// The lettering of the chips [`leaf_chips`] draws for `leaf`: its tab pill
/// when the tab list is in, then the session's provider, then its orphan,
/// abandoned or parked tag. It is what the subline measures its fit
/// against.
fn chip_texts(leaf: &Leaf, pill: Option<&TabPill>) -> Vec<String> {
    pill.map(pill_text)
        .into_iter()
        .chain(
            leaf.provider
                .map(|provider| provider_chip_text(provider).to_owned()),
        )
        .chain(leaf.state.tag().map(|(text, _)| text))
        .collect()
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
    let mut parts = Vec::new();
    if let Some(icon) = container_icon(container) {
        parts.push(icon);
    }
    parts.extend(container_info_parts(container));
    parts
}

/// The container's kind icon, when its kind has one.
fn container_icon(container: &Container) -> Option<AnyElement> {
    let part = |suffix: &str| format!("container-{}-{suffix}", container.key);
    let (path, suffix, color) = kind_icon(container.kind)?;
    let selector = part(&format!("icon-{suffix}"));
    Some(
        div()
            .debug_selector(|| selector)
            .flex()
            .flex_none()
            .child(icon(path, 14.0, color))
            .into_any_element(),
    )
}

/// The row's name, badge, tag and count.
fn container_info_parts(container: &Container) -> Vec<AnyElement> {
    let part = |suffix: &str| format!("container-{}-{suffix}", container.key);
    let mut parts = Vec::new();
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

/// A container row: a click focuses and folds it; a right-click on a repo
/// or workspace opens its menu; a double-click there replays its last
/// launch, and its ▶ replays into the current tab. A row with a launch
/// summary is two lines.
fn container_row(
    container: &Container,
    focused: bool,
    hovered: bool,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    let key = container.key.clone();
    let hover_key = key.clone();
    let click_key = key.clone();
    let click_id = container.id.clone();
    let name = format!("container-{}", container.key);
    let container_menu = match container.kind {
        ContainerKind::Repo => Some(MenuContainer::Repo(container.id.clone())),
        ContainerKind::Workspace => Some(MenuContainer::Workspace(container.id.clone())),
        ContainerKind::Tab => Some(MenuContainer::Tab(container.id.clone())),
        ContainerKind::Shell
        | ContainerKind::Dir
        | ContainerKind::Detached
        | ContainerKind::Unbound => None,
    };
    let launchable = matches!(
        container.kind,
        ContainerKind::Repo | ContainerKind::Workspace
    );
    let workspace = container.kind == ContainerKind::Workspace;
    let summary = container.summary.clone();
    let two_line = summary.is_some();
    let row = div()
        .id(SharedString::from(name.clone()))
        .debug_selector(|| name)
        .relative()
        .flex()
        .flex_none()
        .gap(px(7.0))
        .h(px(if summary.is_some() { 46.0 } else { 30.0 }))
        .pl(px(4.0))
        .pr(px(8.0))
        .cursor_pointer()
        .hover(|style| style.bg(gpui::rgb(HOVER_BG)))
        .when(focused, |row| row.child(container_focus_ring(container)));
    let row = if summary.is_some() {
        row.items_start().pt(px(5.0))
    } else {
        row.items_center()
    };
    let row = if let Some(summary) = summary.as_deref() {
        let row = row.child(div().mt(px(2.0)).child(chevron(container)));
        let row = if let Some(icon) = container_icon(container) {
            row.child(div().mt(px(2.0)).child(icon))
        } else {
            row
        };
        row.child(container_lines(container, summary))
    } else {
        row.child(chevron(container))
            .children(container_parts(container))
    };
    let row = row.when(launchable && (hovered || focused), |row| {
        row.child(container_launch(container, two_line, cx))
    });
    row.when_some(container.tooltip(), |row, tip| row.tooltip(tooltip(tip)))
        .when_some(container_menu, |row, menu| {
            row.on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.open_container_menu(menu.clone(), event.position, window, cx);
                    cx.stop_propagation();
                }),
            )
        })
        .on_hover(cx.listener(move |this, over: &bool, _window, cx| {
            this.set_hovered_container(&hover_key, *over, cx);
        }))
        .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
            if launchable && event.click_count() >= 2 {
                let container = if workspace {
                    launch_last::Container::Workspace(&click_id)
                } else {
                    launch_last::Container::Repo(&click_id)
                };
                this.launch_last_row(container, launch_last::Placement::CurrentTab, window, cx);
                return;
            }
            this.focus_container_row(&click_key, window, cx);
            this.toggle_container(&click_key);
            cx.notify();
        }))
}

/// A repo or workspace row's launch-last ▶, at the row's right. It acts on
/// the press and stops it reaching the row, so the row never folds.
fn container_launch(
    container: &Container,
    two_line: bool,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    let selector = format!("container-{}-launch", container.key);
    let name = selector.clone();
    let id = container.id.clone();
    let workspace = container.kind == ContainerKind::Workspace;
    div()
        .id(SharedString::from(selector))
        .debug_selector(|| name)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(16.0))
        .rounded(px(4.0))
        .cursor_pointer()
        .hover(|style| style.bg(gpui::rgb(HOVER)))
        .when(two_line, |button| button.mt(px(2.0)))
        .tooltip(tooltip("Launch last again in the current tab"))
        .child(icon(SIDEBAR_PLAY_ICON, 12.0, SUBTLE))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                let container = if workspace {
                    launch_last::Container::Workspace(&id)
                } else {
                    launch_last::Container::Repo(&id)
                };
                this.launch_last_row(container, launch_last::Placement::CurrentTab, window, cx);
            }),
        )
}

/// The 2 px accent ring of the container row holding the keyboard.
fn container_focus_ring(container: &Container) -> Div {
    focus_ring(4.0).debug_selector(|| format!("container-{}-focus", container.key))
}

/// A two-line row's text: the name line over the launch summary.
fn container_lines(container: &Container, summary: &str) -> Div {
    let part = |suffix: &str| format!("container-{}-{suffix}", container.key);
    let line = div()
        .flex()
        .items_center()
        .gap(px(6.0))
        .children(container_info_parts(container));
    let selector = part("summary");
    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w(px(0.0))
        .child(line)
        .child(
            truncating(&selector, StyledText::new(summary.to_owned()))
                .debug_selector(|| selector)
                .mt(px(1.0))
                .text_size(px(10.5))
                .text_color(gpui::rgb(MUTED)),
        )
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
    let ground = leaf_ground(selected, ctx.highlighted.contains(id));
    let row = leaf_frame(leaf, ground, accent, ctx.density, cx);
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

impl RootView {
    /// The fill session `id`'s leaf draws with: see [`leaf_ground`].
    #[must_use]
    pub fn leaf_fill(&self, id: &str) -> Option<u32> {
        let selected = self.focused_session().as_deref() == Some(id);
        leaf_ground(selected, self.launched_highlight.contains(id))
    }
}

/// A leaf's fill: the chip while `selected`, else the launched tint while
/// `highlighted` by Select launched; None leaves the ground, lit on hover.
fn leaf_ground(selected: bool, highlighted: bool) -> Option<u32> {
    if selected {
        Some(CHIP)
    } else if highlighted {
        Some(LAUNCHED_TINT)
    } else {
        None
    }
}

/// The leaf's box: its fill `ground`, or the hover fill without one, its
/// hover text, the accent stripe, and the clicks.
fn leaf_frame(
    leaf: &Leaf,
    ground: Option<u32>,
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
        .when_some(ground, |row, ground| row.bg(gpui::rgb(ground)))
        .when(ground.is_none(), |row| {
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

/// Whether the leaf's label draws dimmed: the session is idle with no
/// background work running, and its turn has been seen.
fn seen_idle(leaf: &Leaf) -> bool {
    leaf.status == SessionStatus::Idle && !leaf.background && !leaf.unseen
}

/// The label line's start: the status glyph, the label (dimmed once an
/// idle session's turn has been seen), the `!` badge or, on a session that
/// does not count as busy, its marker, and the runtime tag.
fn label_head(leaf: &Leaf) -> Vec<AnyElement> {
    let seen_idle = seen_idle(leaf);
    let mut parts = vec![
        glyph_view(
            glyph(leaf.status, leaf.background, leaf.mode, leaf.unseen),
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
    if leaf.shows_attention() {
        parts.push(container_badge(format!("leaf-badge-{}", leaf.id)).into_any_element());
    }
    if leaf.busy_excluded {
        parts.push(busy_excluded_marker(&leaf.id));
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

/// The hover text of an excluded leaf's marker.
const BUSY_EXCLUDED_TIP: &str = "Not counted as busy";
/// The marker's box and stroke, in px.
const BUSY_EXCLUDED_SIZE: f32 = 12.0;
const BUSY_EXCLUDED_STROKE: f32 = 1.2;
/// The marker's strokes in its 12 px box: a pulse line, cut where the slash
/// crosses it so the slash reads on top, then the slash.
const BUSY_EXCLUDED_STROKES: [&[(f32, f32)]; 3] = [
    &[(0.8, 6.5), (3.0, 6.5), (4.5, 3.2), (5.04, 4.79)],
    &[(6.2, 8.21), (6.6, 9.4), (8.0, 6.5), (11.2, 6.5)],
    &[(2.2, 11.0), (9.8, 1.0)],
];

/// The muted crossed-out pulse an excluded leaf wears in the `!` badge's
/// slot.
fn busy_excluded_marker(id: &str) -> AnyElement {
    let selector = format!("leaf-busy-excluded-{id}");
    div()
        .id(SharedString::from(selector.clone()))
        .debug_selector(|| selector)
        .flex_none()
        .size(px(BUSY_EXCLUDED_SIZE))
        .child(
            canvas(
                |_, _, _| {},
                |bounds, (), window, _| paint_busy_excluded(bounds, window),
            )
            .size_full(),
        )
        .tooltip(tooltip(BUSY_EXCLUDED_TIP))
        .into_any_element()
}

fn paint_busy_excluded(bounds: Bounds<Pixels>, window: &mut Window) {
    for stroke in BUSY_EXCLUDED_STROKES {
        let mut path = PathBuilder::stroke(px(BUSY_EXCLUDED_STROKE));
        for (i, &(x, y)) in stroke.iter().enumerate() {
            let at = bounds.origin + point(px(x), px(y));
            if i == 0 {
                path.move_to(at);
            } else {
                path.line_to(at);
            }
        }
        match path.build() {
            Ok(path) => window.paint_path(path, gpui::rgb(SUBTLE)),
            Err(err) => tracing::warn!("the busy-excluded marker's stroke did not build: {err:?}"),
        }
        // gpui does not expose lyon's line caps and joins; a dot of the
        // stroke's width on every point draws them round.
        for &(x, y) in stroke {
            let dot = Bounds::centered_at(
                bounds.origin + point(px(x), px(y)),
                gpui::size(px(BUSY_EXCLUDED_STROKE), px(BUSY_EXCLUDED_STROKE)),
            );
            window.paint_quad(
                gpui::fill(dot, gpui::rgb(SUBTLE))
                    .corner_radii(Corners::all(px(BUSY_EXCLUDED_STROKE / 2.0))),
            );
        }
    }
}

/// The runtime tag's colour: the agent's, or [`SUBTLE`] for a shell's
/// program.
fn runtime_color(leaf: &Leaf) -> u32 {
    if leaf.mode == SessionMode::PlainShell {
        return SUBTLE;
    }
    match leaf.agent {
        Agent::Claude => ACCENT,
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
    let texts = chip_texts(leaf, pill.as_ref());
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

    /// The Detached banner's Stop all chip was clicked: the first click arms
    /// it, the second stops each leaf in `live` and disarms.
    fn click_detached_stop_all(&mut self, live: &[String], cx: &mut Context<Self>) {
        if self.detached_stop_all.click() {
            for session_id in live {
                self.send(ClientMessage::StopSession {
                    session_id: session_id.clone(),
                    cleanup: Vec::new(),
                });
            }
        }
        cx.notify();
    }

    /// The Detached banner's Stop all label, while its chip is shown — an
    /// unfolded sidebar on the sessions panel with a live leaf: the armed
    /// text after the first click.
    #[must_use]
    pub fn detached_stop_all_label(&self) -> Option<&'static str> {
        let visible = !self.sidebar.is_collapsed()
            && self.sidebar.activity() == Activity::Sessions
            && detached_stop_all_shown(&self.sidebar_containers());
        visible.then_some(if self.detached_stop_all.armed() {
            DETACHED_STOP_ALL_ARMED
        } else {
            DETACHED_STOP_ALL
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{AddRepoGate, chip_texts, seen_idle};
    use crate::grid_view::ADD_REPO_TIP;
    use crate::remote_mode::{Capabilities, LOCAL_FILES_REASON};
    use crate::sidebar::{Leaf, LeafState};
    use crate::tabs::TabPill;
    use crate::target::Target;
    use protocol::{Agent, ClaudeProvider, SessionMode, SessionStatus};

    #[test]
    fn add_repo_is_offered_only_where_this_pcs_picker_answers() {
        assert_eq!(
            AddRepoGate::for_caps(Capabilities::for_target(&Target::Local)),
            AddRepoGate {
                enabled: true,
                tip: ADD_REPO_TIP,
            }
        );
        let remote = Target::tailnet("box.tail1234.ts.net", None);
        assert_eq!(
            AddRepoGate::for_caps(Capabilities::for_target(&remote)),
            AddRepoGate {
                enabled: false,
                tip: LOCAL_FILES_REASON,
            }
        );
    }

    /// A Claude leaf in `main`, tagged `orphan`, on `provider`.
    fn leaf(provider: Option<ClaudeProvider>) -> Leaf {
        Leaf {
            id: "s1".to_owned(),
            status: SessionStatus::Idle,
            mode: SessionMode::Interactive,
            agent: Agent::Claude,
            label: "s1".to_owned(),
            subline: Some("main".to_owned()),
            runtime: Some("claude".to_owned()),
            provider,
            attention: false,
            busy_excluded: false,
            unseen: false,
            background: false,
            state: LeafState::Orphan,
            trusted: false,
            tooltip: "s1".to_owned(),
        }
    }

    #[test]
    fn a_background_leaf_is_not_drawn_as_seen_idle() {
        let settled = leaf(None);
        assert!(seen_idle(&settled), "a seen idle leaf dims its label");
        let running = Leaf {
            background: true,
            ..leaf(None)
        };
        assert!(
            !seen_idle(&running),
            "background work is still running, so the label stays bright"
        );
        let unseen = Leaf {
            unseen: true,
            ..leaf(None)
        };
        assert!(!seen_idle(&unseen), "an unseen turn stays bright");
    }

    #[test]
    fn the_subline_measures_the_provider_chip_among_the_others() {
        assert_eq!(
            chip_texts(
                &leaf(Some(ClaudeProvider::Deepseek)),
                Some(&TabPill::Unbound)
            ),
            ["unbound", "DeepSeek", "orphan"]
        );
        assert_eq!(
            chip_texts(&leaf(None), Some(&TabPill::Unbound)),
            ["unbound", "orphan"],
            "an Anthropic leaf has one chip less to fit"
        );
    }
}
