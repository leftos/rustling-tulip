//! The session context menu, the pane header's Stop and the stopped-pane
//! overlay: rendering and forwarding to [`crate::session_actions`].

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::fonts::ui_px_at;
use gpui::{
    AnyElement, App, Bounds, ClickEvent, Context, Div, ElementId, Entity, FocusHandle,
    Focusable as _, FontWeight, Keystroke, MouseButton, MouseDownEvent, Pixels, Point,
    ScrollWheelEvent, SharedString, Stateful, Subscription, Task, Window, WindowId, anchored,
    deferred, div, prelude::*, px,
};
use protocol::{
    ClientMessage, DaemonMessage, MemberBranchFate, PresetEntry, PresetTarget, SessionMode,
    SessionSnapshot, SpawnConfig, TabEntry,
};

use crate::appearance::{self, ACCENT_PRESETS, AppearanceChange};
use crate::appearance_view::Level;
use crate::branch_fate::{DeleteWorktreeConfirm, DialogButton, confirm_messages};
use crate::buttons::{
    ButtonKind, ButtonLook, ButtonSize, Layer, button, button_look, field_frame, focus_ring,
    outlined_button,
};
use crate::grid_view::{DOCK_BACK_TIP, NO_REPOS_TIP, PANE_PENDING_TIP, TAB_DOCK_BACK_TIP};
use crate::launch_last;
use crate::notice_view::{MODAL_RADIUS, drop_shadow, modal_panel};
use crate::notices::ToastKind;
use crate::open::{OpenJob, VsCodeTarget, copy_path, repo_folder, workspace_vscode_target};
use crate::open_view::Then;
use crate::overlay_owner::Overlay;
use crate::popouts::{POP_OUT_ALREADY_TIP, PopoutKey, pane_to_pop};
use crate::session_actions::{
    ABANDONED_ACTIONS, ActionState, DuplicateTarget, HEADLESS_UNSUPPORTED_TIP, MenuEntry, MenuMode,
    MoveTarget, OfferedRow, PendingPrefill, PlacedFocus, SessionAction, SessionRow, Step,
    StopPlace, Submenu, SubmenuLine, abandoned_lines, action_disabled_tip, action_state,
    busy_exclusion_message, busy_row, duplicate_lines, exit_code_label, exited_message,
    header_shows_exit_code, menu_entries, move_lines, move_source, orphan_banner_text,
    overlay_actions, pane_shows_exit, plan, rename_message, session_rows, worktree_to_reveal,
};
use crate::sidebar::{Container, ContainerKind, can_attach};
use crate::spawn_form::{Lock, Prefill, Target};
use crate::spawn_view::SpawnEntry;
use crate::tabs::{
    HostedSession, SessionRef, collect_panes, destination, find_tab_containing_session,
};
use crate::target::HostKey;
use crate::text_input::{TextInput, TextInputEvent};
use crate::ui_theme::{UiTheme, theme};
use crate::{RootView, UI_TEXT_SIZE, tooltip};

/// What the pane header's Pop out does.
const POP_OUT_TIP: &str = "Open this pane in a window of its own";
/// What a session pop-out's Close window does.
const CLOSE_WINDOW_TIP: &str = "Close this window; the session is left as it is";

pub(crate) const MENU_WIDTH: f32 = 240.0;
/// The share of the window's height a menu may take before its rows scroll,
/// as the data lists take before theirs do.
pub(crate) const MENU_MAX_SHARE: f32 = 0.6;
const NEW_SESSION_TIP: &str = "Open the spawn dialog; the new session takes over this pane";
const DIALOG_WIDTH: f32 = 440.0;

/// `panel` centred over a tinted layer that takes every click beneath it, and
/// tags it `selector`; a click on the layer itself does nothing.
pub(crate) fn backdrop(selector: &'static str, panel: Stateful<Div>, t: &UiTheme) -> AnyElement {
    div()
        .id(selector)
        .debug_selector(|| selector.to_owned())
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(gpui::rgba(t.scrim))
        .occlude()
        .child(panel)
        .into_any_element()
}

/// The corner radius of a menu and of a popover.
const POPOVER_RADIUS: f32 = 10.0;

/// The card a popover sits in, tagged `id`: the popover fill with its edge
/// and shadow, taking the pointer from whatever is under it.
pub(crate) fn popover_frame(offset: i8, id: &'static str, t: &UiTheme) -> Stateful<Div> {
    div()
        .id(id)
        .debug_selector(move || id.to_owned())
        .bg(gpui::rgb(t.popover))
        .border_1()
        .border_color(gpui::rgb(t.popover_edge))
        .rounded(px(POPOVER_RADIUS))
        .shadow(vec![drop_shadow(t.popover_shadow, 8.0, 24.0)])
        .text_size(ui_px_at(offset, UI_TEXT_SIZE))
        .text_color(gpui::rgb(t.text))
        .occlude()
}

/// The frame a context menu sits in, a [`popover_frame`] tagged `id` and
/// holding `focus`; a press outside it runs `on_out`. It never grows past
/// `max_height`, scrolling its rows instead, so a long menu stays inside the
/// window.
pub(crate) fn menu_frame(
    offset: i8,
    id: &'static str,
    focus: &FocusHandle,
    max_height: Pixels,
    on_out: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    t: &UiTheme,
) -> Stateful<Div> {
    popover_frame(offset, id, t)
        .track_focus(focus)
        .flex()
        .flex_col()
        .w(px(MENU_WIDTH))
        .max_h(max_height)
        .overflow_y_scroll()
        .p(px(4.0))
        .on_mouse_down_out(on_out)
}

/// Who opened the delete-worktree confirm, and so what its answer does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfirmOwner {
    /// The session menu or the exited overlay: a delete answer sends the
    /// stop and the discard.
    Menu,
    /// The quit's branch-fate walk, at session `index` of `total`: the
    /// answer is recorded and the walk moves on; Cancel ends the walk.
    ExitWalk { index: usize, total: usize },
}

impl ConfirmOwner {
    /// The walk's "Session n of m" line under the heading.
    fn progress(self) -> Option<String> {
        match self {
            Self::Menu => None,
            Self::ExitWalk { index, total } => Some(format!("Session {index} of {total}")),
        }
    }
}

/// The open delete-worktree confirm.
pub(crate) struct DeleteDialog {
    confirm: DeleteWorktreeConfirm,
    owner: ConfirmOwner,
    /// Wakes the dialog at the preview's deadline; replacing or dropping
    /// it cancels the wake-up.
    timer: Option<Task<()>>,
}

/// The open context menu.
pub(crate) struct SessionMenu {
    session_id: String,
    /// Where the right-click was, in window coordinates.
    at: Point<Pixels>,
    mode: MenuMode,
    /// The name editor, which replaces the rows while renaming.
    rename: Option<(Entity<TextInput>, Subscription)>,
    /// The submenu that replaces the rows, if any.
    submenu: Submenu,
    /// The `(tab, pane)` whose header opened the menu, which Move to ▸
    /// moves; `None` when it opened elsewhere.
    pane: Option<(String, String)>,
    /// The most height its frame may take, from the window it opened in.
    max_height: Pixels,
}

/// The label of a submenu's row back to the menu's rows.
const SUBMENU_BACK: &str = "‹ Back";

/// The title of the notice a Shift-duplicate whose dialog cannot open shows.
const SHIFT_DUPLICATE_FAILED_TITLE: &str = "Duplicate failed";

/// The title of the notice a launch-last edit whose dialog cannot open shows.
const LAUNCH_EDIT_FAILED_TITLE: &str = "Couldn't edit the launch";

/// The selectors of a submenu's `lines`, in the groups its separators
/// divide.
fn submenu_groups<T>(lines: Vec<SubmenuLine<T>>) -> Vec<Vec<String>> {
    let mut groups = vec![Vec::new()];
    for line in lines {
        let selector = match line {
            SubmenuLine::Separator => {
                groups.push(Vec::new());
                continue;
            }
            SubmenuLine::Back { selector } => selector.to_owned(),
            SubmenuLine::Choice { selector, .. } => selector,
        };
        if let Some(group) = groups.last_mut() {
            group.push(selector);
        }
    }
    groups
}

/// The row that opens the appearance editor.
const APPEARANCE_ROW: &str = "session-menu-appearance";
/// The row that opens the Accent submenu.
const ACCENT_ROW: &str = "session-menu-accent";
const INHERIT_ROW: &str = "accent-inherit";
/// The Accent submenu's row back to the actions.
const BACK_ROW: &str = "accent-back";
/// The toast a colour the protocol refuses raises.
const ACCENT_FAILED_TITLE: &str = "Couldn't set the accent";
/// The container menu's one row.
const CONTAINER_APPEARANCE_ROW: &str = "container-menu-appearance";
/// The container menu's last row: Remove, or its armed confirm.
const CONTAINER_REMOVE_ROW: &str = "container-menu-remove";
/// The container menu's row opening the Launch last again submenu.
const CONTAINER_LAUNCH_LAST_ROW: &str = "container-launch-last";
const CONTAINER_LAUNCH_LAST_LABEL: &str = "Launch last again ▸";
/// The Launch last again submenu's fixed rows; a named tab's selector is
/// [`CONTAINER_LAUNCH_LAST_TAB_PREFIX`] and the tab's id.
const CONTAINER_LAUNCH_LAST_BACK: &str = "container-launch-last-back";
const CONTAINER_LAUNCH_LAST_CURRENT: &str = "container-launch-last-current";
const CONTAINER_LAUNCH_LAST_NEW: &str = "container-launch-last-new";
const CONTAINER_LAUNCH_LAST_EDIT: &str = "container-launch-last-edit";
const CONTAINER_LAUNCH_LAST_TAB_PREFIX: &str = "container-launch-last-tab-";
/// The container menu's first row: the spawn dialog, held on the container
/// or, for a tab container, on that tab.
const CONTAINER_SPAWN_ROW: &str = "container-menu-spawn";
/// The container menu's single preset row while the list is out, failed or
/// empty; a listed preset's row adds `-<index>`.
const CONTAINER_PRESET_ROW: &str = "container-menu-launch-preset";
const PRESET_LOADING_LABEL: &str = "Launch preset… (loading)";
const PRESET_FAILED_LABEL: &str = "Launch preset… (failed to load)";
const PRESET_NONE_LABEL: &str = "Launch preset… (none defined)";
const PRESET_NONE_TIP: &str = "No .rustling-tulip/presets.json in this repo";

/// The presets a repo's or workspace's menu lists, asked for each time it
/// opens.
#[derive(Debug, Clone)]
pub(crate) enum MenuPresets {
    Loading,
    Failed(String),
    Listed(Vec<PresetEntry>),
}

impl MenuPresets {
    /// The listed preset at `index`, while the list holds one there.
    fn entry(&self, index: usize) -> Option<&PresetEntry> {
        match self {
            Self::Listed(entries) => entries.get(index),
            Self::Loading | Self::Failed(_) => None,
        }
    }
}

/// What a container menu row does, whether a click or a key fires it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ContainerAction {
    /// The spawn dialog, held on the repo or workspace, or for a tab
    /// container on that tab.
    Spawn,
    /// Swaps the rows for the Launch last again submenu.
    LaunchLast,
    /// The container's appearance editor.
    Appearance,
    /// The wizard for the listed preset at this index.
    Preset(usize),
    /// The container's folder in the file manager.
    Explorer,
    /// The container in VS Code.
    VsCode,
    /// The container's folder path on the clipboard.
    CopyPath,
    /// Arms the Remove row, or removes the container once armed.
    Remove,
    /// Swaps the submenu back for the menu's rows.
    Back,
    /// A Launch last again choice.
    Launch(ContainerLaunch),
}

/// Whether a container menu row can be pressed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RowState {
    Enabled,
    /// Drawn muted; the tooltip says why.
    Disabled(String),
    /// Drawn muted with no tooltip: the preset list is still loading.
    Inert,
}

/// One row of the container menu: its selector, its text, whether it can be
/// pressed, and what a click or a key on it does.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ContainerRow {
    selector: String,
    label: String,
    state: RowState,
    action: ContainerAction,
}

impl ContainerRow {
    fn new(selector: &str, label: &str, state: RowState, action: ContainerAction) -> Self {
        Self {
            selector: selector.to_owned(),
            label: label.to_owned(),
            state,
            action,
        }
    }

    fn enabled(&self) -> bool {
        self.state == RowState::Enabled
    }
}

/// The preset rows `presets` draws: one muted row while loading, failed or
/// empty, else a row per preset.
fn preset_rows(presets: &MenuPresets) -> Vec<ContainerRow> {
    let single = |label: &str, state: RowState| {
        vec![ContainerRow::new(
            CONTAINER_PRESET_ROW,
            label,
            state,
            ContainerAction::Preset(0),
        )]
    };
    match presets {
        MenuPresets::Loading => single(PRESET_LOADING_LABEL, RowState::Inert),
        MenuPresets::Failed(err) => single(PRESET_FAILED_LABEL, RowState::Disabled(err.clone())),
        MenuPresets::Listed(entries) if entries.is_empty() => single(
            PRESET_NONE_LABEL,
            RowState::Disabled(PRESET_NONE_TIP.to_owned()),
        ),
        MenuPresets::Listed(entries) => entries
            .iter()
            .enumerate()
            .map(|(index, entry)| ContainerRow {
                selector: format!("{CONTAINER_PRESET_ROW}-{index}"),
                label: format!("Launch preset · {}", entry.name),
                state: RowState::Enabled,
                action: ContainerAction::Preset(index),
            })
            .collect(),
    }
}

/// Which kind of container a menu is for, as its rows need it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ContainerShape {
    /// A tab container: one spawn row when the tab can hold a spawn.
    Tab { spawnable: bool },
    /// A repo or workspace, its Remove row reading `remove_label`.
    Level { remove_label: String },
}

/// The container's stored launch config, as its Launch last again row
/// needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LaunchFact {
    /// No stored config: no row.
    None,
    /// A headless config, which this client refuses to replay.
    Headless,
    /// A config the submenu replays.
    Ready,
}

/// What the container menu's rows depend on, read from the client when
/// they are built.
struct ContainerFacts<'a> {
    shape: ContainerShape,
    launch: LaunchFact,
    presets: &'a MenuPresets,
    /// Explorer has a folder to open.
    has_folder: bool,
    /// Why Explorer's folder no longer opens, once the check found it gone.
    folder_gone: Option<&'static str>,
    /// VS Code has a folder or workspace file to open.
    has_vscode_job: bool,
    /// Why VS Code's target no longer opens, once the check found it gone.
    vscode_gone: Option<&'static str>,
    /// Copy path has a path to copy.
    has_copy_path: bool,
    /// Why this host opens no local files, when it does not.
    local_files_refusal: Option<&'static str>,
}

/// The state of an Explorer or VS Code row: disabled when the container has
/// nothing to open, when this host opens no local files, or when its target
/// no longer exists, the first of these that holds giving the tip.
fn open_row_state(
    has_target: bool,
    gone: Option<&'static str>,
    refusal: Option<&'static str>,
) -> RowState {
    match (has_target, refusal.or(gone)) {
        (false, _) => RowState::Disabled(CONTAINER_NO_FOLDER_TIP.to_owned()),
        (true, Some(tip)) => RowState::Disabled(tip.to_owned()),
        (true, None) => RowState::Enabled,
    }
}

/// The container menu's rows in the groups its separators divide.
fn container_row_groups(facts: &ContainerFacts<'_>) -> Vec<Vec<ContainerRow>> {
    let remove_label = match &facts.shape {
        ContainerShape::Tab { spawnable: false } => return Vec::new(),
        ContainerShape::Tab { spawnable: true } => {
            return vec![vec![ContainerRow::new(
                CONTAINER_SPAWN_ROW,
                "Spawn session here",
                RowState::Enabled,
                ContainerAction::Spawn,
            )]];
        }
        ContainerShape::Level { remove_label } => remove_label,
    };
    let enabled = |selector: &str, label: &str, action| {
        ContainerRow::new(selector, label, RowState::Enabled, action)
    };
    let mut groups = vec![vec![enabled(
        CONTAINER_SPAWN_ROW,
        "Spawn session",
        ContainerAction::Spawn,
    )]];
    let launch_state = match facts.launch {
        LaunchFact::None => None,
        LaunchFact::Headless => Some(RowState::Disabled(HEADLESS_UNSUPPORTED_TIP.to_owned())),
        LaunchFact::Ready => Some(RowState::Enabled),
    };
    if let Some(state) = launch_state {
        groups.push(vec![ContainerRow::new(
            CONTAINER_LAUNCH_LAST_ROW,
            CONTAINER_LAUNCH_LAST_LABEL,
            state,
            ContainerAction::LaunchLast,
        )]);
    }
    let mut middle = vec![enabled(
        CONTAINER_APPEARANCE_ROW,
        "Appearance…",
        ContainerAction::Appearance,
    )];
    middle.extend(preset_rows(facts.presets));
    groups.push(middle);
    let copy_state = if facts.has_copy_path {
        RowState::Enabled
    } else {
        RowState::Disabled(CONTAINER_NO_FOLDER_TIP.to_owned())
    };
    groups.push(vec![
        ContainerRow::new(
            CONTAINER_EXPLORER_ROW,
            "Open in Explorer",
            open_row_state(
                facts.has_folder,
                facts.folder_gone,
                facts.local_files_refusal,
            ),
            ContainerAction::Explorer,
        ),
        ContainerRow::new(
            CONTAINER_VSCODE_ROW,
            "Open in VS Code",
            open_row_state(
                facts.has_vscode_job,
                facts.vscode_gone,
                facts.local_files_refusal,
            ),
            ContainerAction::VsCode,
        ),
        ContainerRow::new(
            CONTAINER_COPY_PATH_ROW,
            "Copy path",
            copy_state,
            ContainerAction::CopyPath,
        ),
    ]);
    groups.push(vec![enabled(
        CONTAINER_REMOVE_ROW,
        remove_label,
        ContainerAction::Remove,
    )]);
    groups
}

/// The Launch last again submenu's rows in the groups its separators
/// divide.
fn launch_row_groups(lines: Vec<SubmenuLine<ContainerLaunch>>) -> Vec<Vec<ContainerRow>> {
    let mut groups = vec![Vec::new()];
    for line in lines {
        let row = match line {
            SubmenuLine::Separator => {
                groups.push(Vec::new());
                continue;
            }
            SubmenuLine::Back { selector } => ContainerRow::new(
                selector,
                SUBMENU_BACK,
                RowState::Enabled,
                ContainerAction::Back,
            ),
            SubmenuLine::Choice {
                selector,
                label,
                choice,
            } => ContainerRow {
                selector,
                label,
                state: RowState::Enabled,
                action: ContainerAction::Launch(choice),
            },
        };
        if let Some(group) = groups.last_mut() {
            group.push(row);
        }
    }
    groups
}

/// Whether `ks` opens a focused row's context menu: Shift+F10, or the Menu
/// key alone.
pub(crate) fn is_menu_key(ks: &Keystroke) -> bool {
    let m = &ks.modifiers;
    let other = m.control || m.alt || m.platform || m.function;
    match ks.key.as_str() {
        "f10" => m.shift && !other,
        "menu" => !m.shift && !other,
        _ => false,
    }
}

/// A key that moves a menu's highlight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MenuStep {
    Up,
    Down,
    Home,
    End,
}

impl MenuStep {
    fn of(key: &str) -> Option<Self> {
        match key {
            "up" => Some(Self::Up),
            "down" => Some(Self::Down),
            "home" => Some(Self::Home),
            "end" => Some(Self::End),
            _ => None,
        }
    }
}

/// The selector the highlight moves to from `current` on `step`: through
/// the enabled rows only, wrapping at the ends. With no highlight, or one on
/// a row no longer enabled, Down starts at the first enabled row and Up at
/// the last. `None` when no row is enabled.
fn step_highlight(rows: &[ContainerRow], current: Option<&str>, step: MenuStep) -> Option<String> {
    let enabled: Vec<&ContainerRow> = rows.iter().filter(|row| row.enabled()).collect();
    let count = enabled.len();
    let last = count.checked_sub(1)?;
    let at = current.and_then(|selector| enabled.iter().position(|row| row.selector == selector));
    let next = match (step, at) {
        (MenuStep::Home, _) | (MenuStep::Down, None) => 0,
        (MenuStep::End, _) | (MenuStep::Up, None) => last,
        (MenuStep::Down, Some(at)) => (at + 1) % count,
        (MenuStep::Up, Some(at)) => (at + last) % count,
    };
    enabled.get(next).map(|row| row.selector.clone())
}

/// The enabled row the highlight `current` names, which Enter and Space
/// activate.
fn highlighted_row<'a>(
    rows: &'a [ContainerRow],
    current: Option<&str>,
) -> Option<&'a ContainerRow> {
    let current = current?;
    rows.iter()
        .find(|row| row.enabled() && row.selector == current)
}

/// The container menu's open rows.
const CONTAINER_EXPLORER_ROW: &str = "container-menu-explorer";
const CONTAINER_VSCODE_ROW: &str = "container-menu-vscode";
const CONTAINER_COPY_PATH_ROW: &str = "container-menu-copy-path";
/// Why an open row is disabled: a workspace has no single folder, and a
/// workspace whose members have none leaves VS Code nothing to open.
const CONTAINER_NO_FOLDER_TIP: &str = "This container has no folder";
/// Why an open row is disabled once the check found its target gone: a
/// repo's folder, a workspace's linked file, or every member folder.
const CONTAINER_FOLDER_GONE_TIP: &str = "This folder no longer exists";
const CONTAINER_WORKSPACE_FILE_GONE_TIP: &str = "This workspace file no longer exists";
const CONTAINER_MEMBERS_GONE_TIP: &str = "None of this workspace's folders exist";

/// What the check run when a container menu opens found of the folders and
/// workspace file its open rows target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MenuTargets {
    /// The check still runs, or none runs: every target counts as present.
    Checking,
    /// The check reported these targets gone.
    Known { missing: Vec<PathBuf> },
}

/// Mints the token each container menu open gives its target check.
static NEXT_TARGET_CHECK: AtomicU64 = AtomicU64::new(0);

/// Whether a target check's report, minted as `reported`, fills a menu that
/// holds the token `held` and shows `targets`: only its own open's check,
/// and only while it still waits.
fn check_applies(targets: &MenuTargets, held: u64, reported: u64) -> bool {
    held == reported && *targets == MenuTargets::Checking
}

/// The submenu that replaces the container menu's rows, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContainerSubmenu {
    None,
    LaunchLast,
}

/// What a row of the Launch last again submenu does.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ContainerLaunch {
    /// Replay the stored config where the placement names.
    Placement(launch_last::Placement),
    /// Open the spawn dialog prefilled instead of replaying.
    Edit,
}

/// The container a container menu acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MenuContainer {
    Repo(String),
    Workspace(String),
    Tab(String),
}

impl MenuContainer {
    /// The menu a sidebar container row opens: repos, workspaces and tabs
    /// have one.
    pub(crate) fn of(container: &Container) -> Option<Self> {
        let id = container.id.clone();
        match container.kind {
            ContainerKind::Repo => Some(Self::Repo(id)),
            ContainerKind::Workspace => Some(Self::Workspace(id)),
            ContainerKind::Tab => Some(Self::Tab(id)),
            ContainerKind::Shell
            | ContainerKind::Dir
            | ContainerKind::Detached
            | ContainerKind::Unbound => None,
        }
    }

    /// The appearance and remove level the container names, for the two
    /// kinds that have one.
    fn level(&self) -> Option<Level> {
        match self {
            Self::Repo(id) => Some(Level::Repo(id.clone())),
            Self::Workspace(id) => Some(Level::Workspace(id.clone())),
            Self::Tab(_) => None,
        }
    }

    /// The spawn target the container names, for the two kinds that have
    /// one.
    fn target(&self) -> Option<Target> {
        match self {
            Self::Repo(id) => Some(Target::Repo(id.clone())),
            Self::Workspace(id) => Some(Target::Workspace(id.clone())),
            Self::Tab(_) => None,
        }
    }

    /// The preset target the container names, for the two kinds that have
    /// presets.
    fn preset_target(&self) -> Option<PresetTarget> {
        match self {
            Self::Repo(id) => Some(PresetTarget::Repo {
                repo_id: id.clone(),
            }),
            Self::Workspace(id) => Some(PresetTarget::Workspace {
                workspace_id: id.clone(),
            }),
            Self::Tab(_) => None,
        }
    }
}

/// The open repo, workspace or tab container's context menu.
pub(crate) struct ContainerMenu {
    /// The container it acts on.
    container: MenuContainer,
    /// Where the right-click was, in window coordinates.
    at: Point<Pixels>,
    /// The submenu that replaces the rows, if any.
    submenu: ContainerSubmenu,
    /// The most height its frame may take, from the window it opened in.
    max_height: Pixels,
    /// The presets its preset rows show; a tab container's list stays
    /// empty and unshown.
    presets: MenuPresets,
    /// The `request_id` of this open's `ListPresets`; only the `Presets` or
    /// `Error` carrying it fills the preset rows. `None` for a tab
    /// container, which asks for none.
    preset_request: Option<String>,
    /// Which of its open rows' targets are gone; the rows stay enabled
    /// while the check runs.
    targets: MenuTargets,
    /// The token of this open's target check; a report carrying another
    /// open's token is dropped.
    check: u64,
    /// The selector of the row the keys act on, drawn as if hovered.
    highlight: Option<String>,
    /// The sidebar row a key opened the menu from, which Escape focuses
    /// again; `None` for a right-click.
    return_to: Option<String>,
}

impl ContainerMenu {
    /// The appearance and remove level the menu acts on.
    pub(crate) fn level(&self) -> Option<Level> {
        self.container.level()
    }
}

fn preset_selector(name: &str) -> String {
    format!("accent-preset-{}", name.to_ascii_lowercase())
}

fn recent_selector(color: u32) -> String {
    format!("accent-recent-{color:06x}")
}

/// The open gutter-dot command menu. Its texts are taken when it opens, so
/// output, or a gutter re-indexed while the menu is up, leaves it pointing
/// at the command it was opened for.
pub(crate) struct ShellMenu {
    /// The pane whose dot opened it.
    pane_id: String,
    /// The session that pane shows, when it shows one.
    session_id: Option<String>,
    /// The header row: the dot's tooltip.
    header: String,
    /// The command line, `""` when the shell left none.
    command: String,
    /// The command's output, `""` when there is none.
    output: String,
    /// Where the dot's top-right corner is, in window coordinates.
    at: Point<Pixels>,
    /// The most height its frame may take, from the window it opened in.
    max_height: Pixels,
}

/// The four rows of the gutter menu, with the selector of each.
const SHELL_MENU_ROWS: [&str; 4] = [
    "shell-menu-copy-command",
    "shell-menu-copy-output",
    "shell-menu-copy-both",
    "shell-menu-rerun",
];

/// A menu row as shown: its selector, its text, and what it does.
struct Row {
    selector: String,
    label: &'static str,
    entry: MenuEntry,
}

impl RootView {
    /// The session whose context menu is open.
    #[must_use]
    pub fn session_menu(&self) -> Option<&str> {
        self.menu.as_ref().map(|menu| menu.session_id.as_str())
    }

    /// The pane whose header opened the session menu, while it shows its
    /// actions with no submenu open.
    pub(crate) fn session_menu_pane(&self) -> Option<&str> {
        self.actions_menu()
            .filter(|menu| menu.submenu == Submenu::None)
            .and_then(|menu| menu.pane.as_ref())
            .map(|(_, pane_id)| pane_id.as_str())
    }

    /// A pane showing the session whose header Stop waits for its confirm.
    #[must_use]
    pub fn armed_stop_pane(&self) -> Option<&str> {
        let session_id = self.confirm.armed(StopPlace::Grid)?;
        self.tabs
            .tabs()
            .iter()
            .filter_map(TabEntry::grid)
            .flat_map(collect_panes)
            .find(|pane| pane.session == Some(session_id))
            .map(|pane| pane.id)
    }

    /// The selectors of the menu's rows as shown now.
    #[must_use]
    pub fn menu_rows(&self) -> Vec<String> {
        match self.menu.as_ref() {
            Some(menu) if menu.rename.is_some() => vec!["menu-rename-input".to_owned()],
            _ => self.rows().into_iter().map(|row| row.selector).collect(),
        }
    }

    /// The open menu when it shows its actions list or one of its
    /// submenus, for a session the list holds.
    fn actions_menu(&self) -> Option<&SessionMenu> {
        self.menu
            .as_ref()
            .filter(|menu| menu.rename.is_none() && menu.mode == MenuMode::Actions)
            .filter(|menu| self.sidebar.session(&menu.session_id).is_some())
    }

    /// The open menu when it shows accent rows: its actions list or its
    /// Accent submenu.
    fn accent_menu(&self) -> Option<&SessionMenu> {
        self.actions_menu()
            .filter(|menu| matches!(menu.submenu, Submenu::None | Submenu::Accent))
    }

    /// The rows under the state actions and above the appearance rows, as
    /// the open menu's session offers them; none while a submenu is open.
    fn offered_rows(&self) -> Vec<OfferedRow> {
        self.actions_menu()
            .filter(|menu| menu.submenu == Submenu::None)
            .and_then(|menu| self.sidebar.session(&menu.session_id))
            .map(|session| {
                let host = self.host_of_session(&session.id);
                session_rows(
                    HostedSession::new(&host, session),
                    &self.tabs,
                    &self.sidebar.sessions_by_host(),
                    &self.popouts,
                    self.capabilities_of(&host),
                )
            })
            .unwrap_or_default()
    }

    /// The Duplicate submenu's lines while it is open.
    fn duplicate_menu_lines(&self) -> Option<Vec<SubmenuLine<DuplicateTarget>>> {
        self.actions_menu()
            .filter(|menu| menu.submenu == Submenu::Duplicate)
            .map(|_| duplicate_lines(self.tabs.tabs()))
    }

    /// The Move to submenu's lines while it is open.
    fn move_menu_lines(&self) -> Option<Vec<SubmenuLine<MoveTarget>>> {
        self.actions_menu()
            .filter(|menu| menu.submenu == Submenu::MoveTo)
            .map(|menu| move_lines(self.tabs.tabs(), &menu.session_id))
    }

    /// The selectors of the menu's rows as shown now, in the groups its
    /// separators divide: the state actions, then the rows under them,
    /// then the appearance rows; or an open submenu's rows (the Accent
    /// submenu's as one group).
    #[must_use]
    pub fn menu_groups(&self) -> Vec<Vec<String>> {
        if let Some(lines) = self.duplicate_menu_lines() {
            return submenu_groups(lines);
        }
        if let Some(lines) = self.move_menu_lines() {
            return submenu_groups(lines);
        }
        let offered = self
            .offered_rows()
            .into_iter()
            .map(|offered| offered.row.selector().to_owned())
            .chain(
                self.pane_view_menu_selectors_and_labels()
                    .into_iter()
                    .map(|(selector, _)| selector.to_owned()),
            )
            .collect();
        [self.menu_rows(), offered, self.appearance_group_rows()]
            .into_iter()
            .filter(|group| !group.is_empty())
            .collect()
    }

    /// The tooltip of the menu row tagged `selector` when it is dimmed.
    #[must_use]
    pub fn menu_disabled_tip(&self, selector: &str) -> Option<&'static str> {
        let session = self
            .menu
            .as_ref()
            .and_then(|menu| self.sidebar.session(&menu.session_id));
        let action_tip = self
            .rows()
            .into_iter()
            .find(|row| row.selector == selector)
            .and_then(|row| match (row.entry, session) {
                (MenuEntry::Action(action), Some(session)) => action_disabled_tip(action, session),
                _ => None,
            });
        if action_tip.is_some() {
            return action_tip;
        }
        self.offered_rows()
            .into_iter()
            .find(|offered| offered.row.selector() == selector)
            .and_then(|offered| offered.disabled)
    }

    /// The label of the row tagged `selector` in the group under the state
    /// actions, or of the busy-exclusion row among the appearance rows.
    #[must_use]
    pub fn menu_row_label(&self, selector: &str) -> Option<&'static str> {
        self.offered_rows()
            .into_iter()
            .map(|offered| offered.row)
            .chain(self.busy_menu_row())
            .find(|row| row.selector() == selector)
            .map(SessionRow::label)
    }

    /// The busy-exclusion row while the appearance rows show under the
    /// actions (no submenu open).
    fn busy_menu_row(&self) -> Option<SessionRow> {
        self.accent_menu()
            .filter(|menu| menu.submenu == Submenu::None)
            .and_then(|menu| self.sidebar.session(&menu.session_id))
            .map(busy_row)
    }

    /// The selectors of the menu's last group as shown now: under the
    /// actions, the rows that open the appearance editor and the Accent
    /// submenu, then the checkable "Don't count as busy"; with the Accent
    /// submenu open, its Back, presets, recent colours and Inherit.
    #[must_use]
    pub fn appearance_group_rows(&self) -> Vec<String> {
        let Some(menu) = self.accent_menu() else {
            return Vec::new();
        };
        if menu.submenu != Submenu::Accent {
            let mut rows = vec![APPEARANCE_ROW.to_owned(), ACCENT_ROW.to_owned()];
            rows.extend(self.busy_menu_row().map(|row| row.selector().to_owned()));
            return rows;
        }
        let presets = ACCENT_PRESETS.iter().map(|p| preset_selector(p.name));
        let recent = self
            .sidebar
            .recent_colors()
            .into_iter()
            .map(recent_selector);
        [BACK_ROW.to_owned()]
            .into_iter()
            .chain(presets)
            .chain(recent)
            .chain([INHERIT_ROW.to_owned()])
            .collect()
    }

    /// The labels of the menu's rows as shown now.
    #[must_use]
    pub fn menu_labels(&self) -> Vec<String> {
        self.rows()
            .into_iter()
            .map(|row| row.label.to_owned())
            .collect()
    }

    /// Whether a restart or resume of `session_id` waits for its reply.
    #[must_use]
    pub fn restart_pending(&self, session_id: &str) -> bool {
        self.duplicates.is_pending(session_id)
    }

    /// The active tab's focused pane.
    #[must_use]
    pub fn focused_pane(&self) -> Option<String> {
        self.tabs.focused_pane(self.tabs.active_id()?)
    }

    /// Opens the menu of `session_id` at `at`, taking the keyboard so Esc
    /// reaches it. `pane` is the `(tab, pane)` whose header was
    /// right-clicked, `None` when the menu opens elsewhere. The menu draws in
    /// `window`, the window that asked for it.
    pub fn open_session_menu(
        &mut self,
        session_id: &str,
        pane: Option<&(String, String)>,
        at: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.sidebar.session(session_id).is_none() {
            return;
        }
        self.close_more_menu(window, cx);
        // Another window's menu is not under this one's backdrop: only one
        // menu is open at a time across the windows.
        self.close_shell_menu(window, cx);
        self.close_empty_pane_menu(window, cx);
        // A menu open in another window hands that window's keyboard back.
        self.close_session_menu(window, cx);
        self.prefills.clear();
        self.overlays
            .opened(Overlay::SessionMenu, window.window_handle().window_id());
        self.menu = Some(SessionMenu {
            session_id: session_id.to_owned(),
            at,
            mode: MenuMode::Actions,
            rename: None,
            submenu: Submenu::None,
            pane: pane.cloned(),
            max_height: window.viewport_size().height * MENU_MAX_SHARE,
        });
        self.confirm.disarm();
        self.menu_focus.focus(window);
        cx.notify();
    }

    /// Closes the menu and hands the keyboard back to the terminal of the
    /// window it was open in.
    pub(crate) fn close_session_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm.disarm();
        if self.menu.take().is_some() {
            let owner = self.overlays.owner(Overlay::SessionMenu);
            self.hand_back_focus(owner, window, cx);
            cx.notify();
        }
    }

    /// Closes the menu and the menu's delete-worktree confirm of a session
    /// the daemon no longer lists, and forgets its appearance sends still
    /// in flight. The quit's walk skips such a session itself.
    pub(crate) fn drop_stale_session_ui(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let sidebar = &self.sidebar;
        self.pending_appearance
            .retain_sessions(|session_id| sidebar.session(session_id).is_some());
        if self
            .menu
            .as_ref()
            .is_some_and(|menu| self.sidebar.session(&menu.session_id).is_none())
        {
            self.close_session_menu(window, cx);
        }
        if self.delete_dialog.as_ref().is_some_and(|dialog| {
            dialog.owner == ConfirmOwner::Menu
                && self.sidebar.session(dialog.confirm.session_id()).is_none()
        }) {
            self.close_delete_dialog(window, cx);
        }
        if self
            .repo_remove
            .dialog()
            .is_some_and(|dialog| !self.level_listed(dialog.level()))
        {
            self.close_repo_remove_dialog(window, cx);
        }
        self.drop_stale_appearance_editor(window, cx);
    }

    /// Closes the container menu when the container it acts on left the
    /// registry or the tab list. It runs after a tab list is folded, so a
    /// removed tab's menu cannot outlive it.
    pub(crate) fn drop_stale_container_menu(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .container_menu
            .as_ref()
            .is_some_and(|menu| !self.container_listed(&menu.container))
        {
            self.close_container_menu(window, cx);
        }
    }

    /// Whether the container a menu names is still there.
    fn container_listed(&self, container: &MenuContainer) -> bool {
        match container {
            MenuContainer::Repo(id) => self.sidebar.find_repo(id).is_some(),
            MenuContainer::Workspace(id) => self.sidebar.find_workspace(id).is_some(),
            MenuContainer::Tab(id) => self.tabs.tabs().iter().any(|tab| &tab.id == id),
        }
    }

    /// Whether the tab container `id` can hold a spawn: only a grid tab can.
    fn tab_spawnable(&self, id: &str) -> bool {
        self.tabs
            .tabs()
            .iter()
            .find(|tab| tab.id == id)
            .is_some_and(|tab| tab.grid().is_some())
    }

    /// The session a tab container's spawn starts on: the first pane that
    /// holds one.
    fn tab_preselect(&self, id: &str) -> Option<String> {
        let tab = self.tabs.tabs().iter().find(|tab| tab.id == id)?;
        collect_panes(tab.grid()?)
            .into_iter()
            .find_map(|pane| pane.session)
            .map(str::to_owned)
    }

    /// Whether the repo or workspace `level` names is registered on any
    /// host.
    fn level_listed(&self, level: &Level) -> bool {
        match level {
            Level::Repo(id) => self.sidebar.find_repo(id).is_some(),
            Level::Workspace(id) => self.sidebar.find_workspace(id).is_some(),
            Level::Session(_) | Level::App => false,
        }
    }

    /// Whether a repo's or workspace's context menu is open.
    #[must_use]
    pub fn container_menu_open(&self) -> bool {
        self.container_menu.is_some()
    }

    /// The repo or workspace `container` names, as a container a launch
    /// replays.
    fn launch_container(container: &MenuContainer) -> Option<launch_last::Container<'_>> {
        match container {
            MenuContainer::Repo(id) => Some(launch_last::Container::Repo(id)),
            MenuContainer::Workspace(id) => Some(launch_last::Container::Workspace(id)),
            MenuContainer::Tab(_) => None,
        }
    }

    /// The stored launch config of the repo or workspace the container menu
    /// acts on.
    fn container_launch_config(&self, container: &MenuContainer) -> Option<SpawnConfig> {
        Self::launch_container(container)
            .and_then(|container| self.sidebar.last_spawn_config(container))
    }

    /// What the open menu's rows depend on now.
    fn container_facts<'a>(&self, menu: &'a ContainerMenu) -> ContainerFacts<'a> {
        let container = &menu.container;
        let shape = match (container, container.level()) {
            (_, Some(level)) => {
                let key = crate::repo_remove::container_key(&level);
                let armed = self.repo_remove.armed() == Some(key.as_str());
                ContainerShape::Level {
                    remove_label: crate::repo_remove::row_label(&level, armed),
                }
            }
            (MenuContainer::Tab(id), None) => ContainerShape::Tab {
                spawnable: self.tab_spawnable(id),
            },
            (MenuContainer::Repo(_) | MenuContainer::Workspace(_), None) => {
                ContainerShape::Tab { spawnable: false }
            }
        };
        let launch = match self.container_launch_config(container) {
            None => LaunchFact::None,
            Some(config) if config.mode == SessionMode::Headless => LaunchFact::Headless,
            Some(_) => LaunchFact::Ready,
        };
        ContainerFacts {
            shape,
            launch,
            presets: &menu.presets,
            has_folder: self.container_folder(container).is_some(),
            folder_gone: self.container_folder_gone(container),
            has_vscode_job: self.container_vscode_job(container).is_some(),
            vscode_gone: self.container_vscode_gone(container),
            has_copy_path: self.container_copy_path(container).is_some(),
            local_files_refusal: self
                .capabilities_of(&self.host_of_menu_container(container))
                .local_files_refusal(),
        }
    }

    /// The container menu's rows as shown now, in the groups its separators
    /// divide; empty while it is closed.
    fn container_menu_rows(&self) -> Vec<Vec<ContainerRow>> {
        let Some(menu) = self.container_menu.as_ref() else {
            return Vec::new();
        };
        match menu.submenu {
            ContainerSubmenu::None => container_row_groups(&self.container_facts(menu)),
            ContainerSubmenu::LaunchLast => launch_row_groups(self.container_launch_lines()),
        }
    }

    /// The selectors of the container menu's rows as shown now, in the groups
    /// its separators divide; empty while it is closed.
    #[must_use]
    pub fn container_menu_groups(&self) -> Vec<Vec<String>> {
        self.container_menu_rows()
            .into_iter()
            .map(|group| group.into_iter().map(|row| row.selector).collect())
            .collect()
    }

    /// The tooltip of the container menu row tagged `selector` when the row
    /// is drawn disabled.
    #[must_use]
    pub fn container_menu_disabled_tip(&self, selector: &str) -> Option<String> {
        self.container_menu_rows()
            .into_iter()
            .flatten()
            .find(|row| row.selector == selector)
            .and_then(|row| match row.state {
                RowState::Disabled(tip) => Some(tip),
                RowState::Enabled | RowState::Inert => None,
            })
    }

    /// The selector of the container menu row the keys act on, if any.
    #[must_use]
    pub fn container_menu_highlight(&self) -> Option<String> {
        self.container_menu.as_ref()?.highlight.clone()
    }

    /// The labels of the container menu's rows as shown now; empty while it
    /// is closed.
    #[must_use]
    pub fn container_menu_labels(&self) -> Vec<String> {
        self.container_menu_rows()
            .into_iter()
            .flatten()
            .map(|row| row.label)
            .collect()
    }

    /// The Launch last again submenu: Back, Current tab, New tab, every grid
    /// tab by name, then Edit first….
    fn container_launch_lines(&self) -> Vec<SubmenuLine<ContainerLaunch>> {
        let mut lines = vec![
            SubmenuLine::Back {
                selector: CONTAINER_LAUNCH_LAST_BACK,
            },
            SubmenuLine::Choice {
                selector: CONTAINER_LAUNCH_LAST_CURRENT.to_owned(),
                label: "Current tab".to_owned(),
                choice: ContainerLaunch::Placement(launch_last::Placement::CurrentTab),
            },
            SubmenuLine::Choice {
                selector: CONTAINER_LAUNCH_LAST_NEW.to_owned(),
                label: "New tab".to_owned(),
                choice: ContainerLaunch::Placement(launch_last::Placement::NewTab),
            },
        ];
        let tabs: Vec<SubmenuLine<ContainerLaunch>> = self
            .tabs
            .tabs()
            .iter()
            .filter(|tab| tab.grid().is_some())
            .map(|tab| SubmenuLine::Choice {
                selector: format!("{CONTAINER_LAUNCH_LAST_TAB_PREFIX}{}", tab.id),
                label: tab.name.clone(),
                choice: ContainerLaunch::Placement(launch_last::Placement::Tab(tab.id.clone())),
            })
            .collect();
        if !tabs.is_empty() {
            lines.push(SubmenuLine::Separator);
            lines.extend(tabs);
        }
        lines.push(SubmenuLine::Separator);
        lines.push(SubmenuLine::Choice {
            selector: CONTAINER_LAUNCH_LAST_EDIT.to_owned(),
            label: "Edit first…".to_owned(),
            choice: ContainerLaunch::Edit,
        });
        lines
    }

    /// Swaps the container menu's rows for `submenu`, or back to them.
    fn show_container_submenu(
        &mut self,
        submenu: ContainerSubmenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(menu) = &mut self.container_menu else {
            return;
        };
        menu.submenu = submenu;
        let keyed = menu.highlight.is_some();
        // The submenu hides the Remove row, so its arm cannot outlive the
        // swap; Back returns it unarmed.
        self.repo_remove.disarm();
        if keyed {
            let rows: Vec<ContainerRow> =
                self.container_menu_rows().into_iter().flatten().collect();
            let first = step_highlight(&rows, None, MenuStep::Home);
            if let Some(menu) = &mut self.container_menu {
                menu.highlight = first;
            }
        }
        self.menu_focus.focus(window);
        cx.notify();
    }

    /// Opens the menu of the focused sidebar row `key` from the keyboard, at
    /// the row's bottom-left corner, its first enabled row highlighted;
    /// Escape later hands the keyboard back to the row. A row with no menu
    /// opens nothing.
    pub(crate) fn open_container_menu_by_key(
        &mut self,
        key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(container) = self
            .sidebar_containers()
            .iter()
            .find(|container| container.key == key)
            .and_then(MenuContainer::of)
        else {
            return;
        };
        let at = match &self.focused_row_bounds {
            Some((painted, bounds)) if painted == key => bounds.bottom_left(),
            _ => {
                tracing::debug!(
                    row = key,
                    "no painted bounds for the focused row; menu at the origin"
                );
                Point::default()
            }
        };
        self.open_container_menu(container, at, window, cx);
        let rows: Vec<ContainerRow> = self.container_menu_rows().into_iter().flatten().collect();
        let first = step_highlight(&rows, None, MenuStep::Home);
        if let Some(menu) = &mut self.container_menu {
            menu.highlight = first;
            menu.return_to = Some(key.to_owned());
        }
    }

    /// Records where the focused sidebar row `key` was painted, so a key
    /// that opens its menu anchors it there.
    pub(crate) fn note_focused_row_bounds(&mut self, key: &str, bounds: Bounds<Pixels>) {
        self.focused_row_bounds = Some((key.to_owned(), bounds));
    }

    /// The keys an open container menu takes: Up, Down, Home and End move
    /// the highlight through the enabled rows, Enter and Space run the
    /// highlighted row, Tab does nothing, and Escape on a menu a key opened
    /// closes it and focuses its row again. `false` for any other key, with
    /// no container menu open, or while the menu does not hold the keyboard.
    pub(crate) fn on_container_menu_key(
        &mut self,
        ks: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.menu_focus.is_focused(window) {
            return false;
        }
        let Some(menu) = self.container_menu.as_ref() else {
            return false;
        };
        let current = menu.highlight.clone();
        let return_to = menu.return_to.clone();
        let rows: Vec<ContainerRow> = self.container_menu_rows().into_iter().flatten().collect();
        if let Some(step) = MenuStep::of(&ks.key) {
            let next = step_highlight(&rows, current.as_deref(), step);
            if let Some(menu) = &mut self.container_menu {
                menu.highlight = next;
            }
            cx.notify();
            return true;
        }
        match ks.key.as_str() {
            "enter" | "space" => {
                if let Some(row) = highlighted_row(&rows, current.as_deref()) {
                    self.activate_container_row(row, window, cx);
                }
                true
            }
            "tab" => true,
            "escape" => {
                let Some(key) = return_to else {
                    return false;
                };
                self.close_container_menu(window, cx);
                self.focus_container_row(&key, window, cx);
                true
            }
            _ => false,
        }
    }

    /// Runs the action of a container menu row: the one a click or a key
    /// pressed. A row that is not enabled does nothing.
    fn activate_container_row(
        &mut self,
        row: &ContainerRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !row.enabled() {
            return;
        }
        let Some(container) = self
            .container_menu
            .as_ref()
            .map(|menu| menu.container.clone())
        else {
            return;
        };
        match row.action.clone() {
            ContainerAction::Spawn => self.container_spawn(&container, window, cx),
            ContainerAction::LaunchLast => {
                self.show_container_submenu(ContainerSubmenu::LaunchLast, window, cx);
            }
            ContainerAction::Back => {
                self.show_container_submenu(ContainerSubmenu::None, window, cx);
            }
            ContainerAction::Appearance => self.container_appearance(&container, window, cx),
            ContainerAction::Preset(index) => self.container_preset(&container, index, window, cx),
            ContainerAction::Explorer => {
                let job = self.container_folder(&container).map(OpenJob::Folder);
                self.container_open(job, window, cx);
            }
            ContainerAction::VsCode => {
                let job = self.container_vscode_job(&container);
                self.container_open(job, window, cx);
            }
            ContainerAction::CopyPath => self.container_copy(&container, window, cx),
            ContainerAction::Remove => {
                if let Some(level) = container.level() {
                    self.click_remove_container(&level, window, cx);
                }
            }
            ContainerAction::Launch(ContainerLaunch::Placement(placement)) => {
                self.choose_container_launch(&container, placement, window, cx);
            }
            ContainerAction::Launch(ContainerLaunch::Edit) => {
                self.open_container_launch_edit(&container, window, cx);
            }
        }
    }

    /// Closes the container menu and opens the spawn dialog held on the
    /// repo or workspace, or on the tab for a tab container.
    fn container_spawn(
        &mut self,
        container: &MenuContainer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (lock, preselect) = match container {
            MenuContainer::Tab(id) => (Lock::for_tab(id.clone()), self.tab_preselect(id)),
            MenuContainer::Repo(_) | MenuContainer::Workspace(_) => {
                let Some(target) = container.target() else {
                    return;
                };
                (Lock::for_target(target), None)
            }
        };
        self.close_container_menu(window, cx);
        self.open_spawn_dialog(SpawnEntry::Locked { lock, preselect }, window, cx);
    }

    /// Closes the container menu and opens the container's appearance
    /// editor.
    fn container_appearance(
        &mut self,
        container: &MenuContainer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(level) = container.level() else {
            return;
        };
        self.close_container_menu(window, cx);
        self.open_appearance_editor(level, window, cx);
    }

    /// Closes the container menu and opens the wizard for the listed preset
    /// at `index` on the container.
    fn container_preset(
        &mut self,
        container: &MenuContainer,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = container.preset_target() else {
            return;
        };
        let entry = self
            .container_menu
            .as_ref()
            .and_then(|menu| menu.presets.entry(index))
            .cloned();
        self.close_container_menu(window, cx);
        if let Some(entry) = entry {
            self.open_preset_wizard(entry, target, window, cx);
        }
    }

    /// Closes the container menu and hands `job` to the opener.
    fn container_open(
        &mut self,
        job: Option<OpenJob>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(job) = job else {
            return;
        };
        let host = self.container_menu_host();
        self.close_container_menu(window, cx);
        self.dispatch_open(&host, job, Then::Nothing, window, cx);
    }

    /// The host that owns the repo or workspace `container` names; This PC
    /// for a tab, which is This PC's layout.
    fn host_of_menu_container(&self, container: &MenuContainer) -> HostKey {
        match container {
            MenuContainer::Repo(id) => self.host_of_repo(id),
            MenuContainer::Workspace(id) => self.host_of_workspace(id),
            MenuContainer::Tab(_) => HostKey::Local,
        }
    }

    /// The host of the container the open container menu acts on; This PC
    /// while none is open.
    fn container_menu_host(&self) -> HostKey {
        self.container_menu.as_ref().map_or(HostKey::Local, |menu| {
            self.host_of_menu_container(&menu.container)
        })
    }

    /// Closes the container menu and puts the container's path on the
    /// clipboard.
    fn container_copy(
        &mut self,
        container: &MenuContainer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = self.container_copy_path(container) else {
            return;
        };
        self.close_container_menu(window, cx);
        self.copy_to_clipboard(&path, cx);
    }

    /// Closes the container menu and replays its container's last launch
    /// where `placement` says.
    fn choose_container_launch(
        &mut self,
        container: &MenuContainer,
        placement: launch_last::Placement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_container_menu(window, cx);
        match container {
            MenuContainer::Repo(id) => {
                self.launch_last_row(launch_last::Container::Repo(id), placement, window, cx);
            }
            MenuContainer::Workspace(id) => {
                self.launch_last_row(launch_last::Container::Workspace(id), placement, window, cx);
            }
            MenuContainer::Tab(_) => {}
        }
    }

    /// Closes the container menu and opens the spawn dialog prefilled with
    /// the container's stored config, held on that container.
    fn open_container_launch_edit(
        &mut self,
        container: &MenuContainer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = container.target() else {
            return;
        };
        let Some(config) = self.container_launch_config(container) else {
            return;
        };
        self.close_container_menu(window, cx);
        let prefill = Prefill {
            config: Some(config),
            open_in: None,
        };
        self.open_prefilled_dialog(
            Some(Lock::for_target(target)),
            prefill,
            LAUNCH_EDIT_FAILED_TITLE,
            window,
            cx,
        );
    }

    /// Opens the context menu of `container` at `at`, taking the keyboard so
    /// Esc reaches it; any other menu gives way.
    pub(crate) fn open_container_menu(
        &mut self,
        container: MenuContainer,
        at: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.container_listed(&container) {
            return;
        }
        self.close_session_menu(window, cx);
        self.close_shell_menu(window, cx);
        self.close_tab_menu(window, cx);
        self.close_sc_picker(window, cx);
        self.close_sc_file_menu(window, cx);
        self.close_more_menu(window, cx);
        let target = container.preset_target();
        let preset_request = target.as_ref().map(|_| crate::new_request_id());
        let presets = if target.is_some() {
            MenuPresets::Loading
        } else {
            MenuPresets::Listed(Vec::new())
        };
        self.container_menu = Some(ContainerMenu {
            container,
            at,
            submenu: ContainerSubmenu::None,
            max_height: window.viewport_size().height * MENU_MAX_SHARE,
            presets,
            preset_request: preset_request.clone(),
            targets: MenuTargets::Checking,
            check: NEXT_TARGET_CHECK.fetch_add(1, Ordering::Relaxed),
            highlight: None,
            return_to: None,
        });
        if let Some(target) = target {
            self.send_to(
                &self.host_of_preset_target(&target),
                ClientMessage::ListPresets {
                    target,
                    request_id: preset_request,
                },
            );
        }
        self.check_container_targets(window, cx);
        self.menu_focus.focus(window);
        cx.notify();
    }

    /// Checks off the UI thread whether the open menu's Explorer and VS
    /// Code targets still exist. Nothing is checked on a host that opens no
    /// local files, whose rows are disabled anyway.
    fn check_container_targets(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .capabilities_of(&self.container_menu_host())
            .local_files_refusal()
            .is_some()
        {
            return;
        }
        let Some((container, check)) = self
            .container_menu
            .as_ref()
            .map(|menu| (menu.container.clone(), menu.check))
        else {
            return;
        };
        let mut paths: Vec<PathBuf> = self.container_folder(&container).into_iter().collect();
        let vscode_job = self.container_vscode_job(&container);
        for path in vscode_job.iter().flat_map(OpenJob::target_paths) {
            if !paths.contains(path) {
                paths.push(path.clone());
            }
        }
        if paths.is_empty() {
            return;
        }
        let opener = Arc::clone(&self.opener);
        let probe = cx.background_executor().spawn(async move {
            paths
                .into_iter()
                .filter(|path| !opener.path_exists(path))
                .collect::<Vec<PathBuf>>()
        });
        cx.spawn_in(window, async move |this, cx| {
            let missing = probe.await;
            // Fails only when the view is gone, and the menu with it.
            this.update_in(cx, |root, _, cx| {
                root.on_container_targets(&container, check, missing, cx);
            })
            .ok();
        })
        .detach();
    }

    /// A target check's report fills the open menu when that menu is the
    /// open the check ran for and still waits for it; any other is dropped.
    fn on_container_targets(
        &mut self,
        container: &MenuContainer,
        check: u64,
        missing: Vec<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        let waiting = self
            .container_menu
            .as_mut()
            .filter(|menu| check_applies(&menu.targets, menu.check, check));
        let Some(menu) = waiting else {
            tracing::debug!(?container, "dropping a target check no open menu waits for");
            return;
        };
        if !missing.is_empty() {
            tracing::info!(
                ?container,
                ?missing,
                "container menu targets no longer exist"
            );
        }
        menu.targets = MenuTargets::Known { missing };
        cx.notify();
    }

    /// The open menu's targets its check found gone; none until it reports.
    fn container_missing(&self) -> &[PathBuf] {
        match self.container_menu.as_ref().map(|menu| &menu.targets) {
            Some(MenuTargets::Known { missing }) => missing,
            Some(MenuTargets::Checking) | None => &[],
        }
    }

    /// Why Explorer's folder no longer opens, once the check found it gone.
    fn container_folder_gone(&self, container: &MenuContainer) -> Option<&'static str> {
        let folder = self.container_folder(container)?;
        self.container_missing()
            .contains(&folder)
            .then_some(CONTAINER_FOLDER_GONE_TIP)
    }

    /// Why VS Code's target no longer opens, once the check found it gone: a
    /// repo's folder, a workspace's linked file, or every member folder.
    fn container_vscode_gone(&self, container: &MenuContainer) -> Option<&'static str> {
        let missing = self.container_missing();
        match container {
            MenuContainer::Repo(_) => self.container_folder_gone(container),
            MenuContainer::Workspace(id) => {
                match workspace_vscode_target(self.sidebar.workspaces(), self.sidebar.repos(), id)?
                {
                    VsCodeTarget::WorkspaceFile(path) => missing
                        .contains(&path)
                        .then_some(CONTAINER_WORKSPACE_FILE_GONE_TIP),
                    VsCodeTarget::Folders(paths) => paths
                        .iter()
                        .all(|path| missing.contains(path))
                        .then_some(CONTAINER_MEMBERS_GONE_TIP),
                }
            }
            MenuContainer::Tab(_) => None,
        }
    }

    /// A `Presets` reply fills the open menu's preset rows when it carries
    /// that open's request id and the rows still load. Any other reply, a
    /// broadcast or another open's included, is dropped.
    pub(crate) fn on_preset_message(&mut self, msg: &DaemonMessage, cx: &mut Context<Self>) {
        let DaemonMessage::Presets {
            target,
            entries,
            request_id,
        } = msg
        else {
            return;
        };
        let Some(menu) = self.preset_menu_waiting(request_id.as_deref()) else {
            tracing::debug!(
                ?request_id,
                "dropping a preset list no open menu waits for: {target:?}"
            );
            return;
        };
        menu.presets = MenuPresets::Listed(entries.clone());
        cx.notify();
    }

    /// The open menu whose preset rows still load for `request_id`.
    fn preset_menu_waiting(&mut self, request_id: Option<&str>) -> Option<&mut ContainerMenu> {
        let request_id = request_id?;
        self.container_menu.as_mut().filter(|menu| {
            matches!(menu.presets, MenuPresets::Loading)
                && menu.preset_request.as_deref() == Some(request_id)
        })
    }

    /// A refusal carrying the open menu's preset request id is its list's
    /// failure: the rows show it. Returns whether the menu took it.
    pub(crate) fn on_preset_list_failed(
        &mut self,
        request_id: &str,
        message: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(menu) = self.preset_menu_waiting(Some(request_id)) else {
            return false;
        };
        menu.presets = MenuPresets::Failed(message.to_owned());
        cx.notify();
        true
    }

    /// Closes the container menu and hands the keyboard back to the active
    /// pane, dropping a Remove row's arm with it.
    pub(crate) fn close_container_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.repo_remove.disarm();
        if self.container_menu.take().is_some() {
            self.focus_active_pane(window, cx);
            cx.notify();
        }
    }

    /// The folder Explorer and Copy path act on: a repo's own, and none for
    /// a workspace.
    fn container_folder(&self, container: &MenuContainer) -> Option<PathBuf> {
        match container {
            MenuContainer::Repo(id) => repo_folder(self.sidebar.repos(), id),
            MenuContainer::Workspace(_) | MenuContainer::Tab(_) => None,
        }
    }

    /// The path Copy path puts on the clipboard: a repo's own folder, and
    /// none for a workspace.
    fn container_copy_path(&self, container: &MenuContainer) -> Option<String> {
        match container {
            MenuContainer::Repo(id) => copy_path(self.sidebar.repos(), id),
            MenuContainer::Workspace(_) | MenuContainer::Tab(_) => None,
        }
    }

    /// The job Open in VS Code hands the opener: a repo's folder, a
    /// workspace's linked `.code-workspace` file, or its member folders
    /// less those the menu's check found gone.
    fn container_vscode_job(&self, container: &MenuContainer) -> Option<OpenJob> {
        match container {
            MenuContainer::Repo(id) => repo_folder(self.sidebar.repos(), id)
                .map(|folder| OpenJob::VsCodeFolder(vec![folder])),
            MenuContainer::Workspace(id) => {
                let missing = self.container_missing();
                workspace_vscode_target(self.sidebar.workspaces(), self.sidebar.repos(), id).map(
                    |target| match target {
                        VsCodeTarget::WorkspaceFile(path) => OpenJob::VsCodeWorkspace(path),
                        VsCodeTarget::Folders(paths) => OpenJob::VsCodeFolder(
                            paths
                                .into_iter()
                                .filter(|path| !missing.contains(path))
                                .collect(),
                        ),
                    },
                )
            }
            MenuContainer::Tab(_) => None,
        }
    }

    /// The open container menu's rows over `frame`, a separator between
    /// groups.
    fn container_menu_frame(&self, frame: Stateful<Div>, cx: &mut Context<Self>) -> Stateful<Div> {
        let highlight = self.container_menu_highlight();
        let mut frame = frame;
        for (index, group) in self.container_menu_rows().into_iter().enumerate() {
            if index > 0 {
                frame = frame.child(menu_separator(&theme(cx)));
            }
            for row in group {
                let highlighted = highlight.as_deref() == Some(row.selector.as_str());
                frame = frame.child(Self::container_row_element(row, highlighted, cx));
            }
        }
        frame
    }

    /// One container menu row as drawn: a click on an enabled one runs its
    /// action, and a disabled one says why in its tooltip. The highlighted
    /// row is drawn as if hovered.
    fn container_row_element(
        row: ContainerRow,
        highlighted: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match row.state.clone() {
            RowState::Disabled(tip) => {
                disabled_row(&row.selector, row.label, tip, &theme(cx)).into_any_element()
            }
            RowState::Inert => inert_row(&row.selector, row.label, &theme(cx)).into_any_element(),
            RowState::Enabled => {
                // Remove stays open to arm, so its press must not reach the
                // frame's press-outside.
                let remove = row.action == ContainerAction::Remove;
                let item = menu_item(&row.selector, row.label.clone(), remove, &theme(cx))
                    .when(highlighted, |item| {
                        item.bg(gpui::rgb(theme(cx).popover_hover))
                    });
                let item = if remove {
                    item.on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|_, _: &MouseDownEvent, _, cx| cx.stop_propagation()),
                    )
                } else {
                    item
                };
                item.on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.activate_container_row(&row, window, cx);
                    if remove {
                        cx.stop_propagation();
                    }
                }))
                .into_any_element()
            }
        }
    }

    /// The open container menu over a layer that keeps a click outside it
    /// from reaching what lies beneath.
    pub(crate) fn container_menu_layer(&self, cx: &mut Context<Self>) -> Option<[AnyElement; 2]> {
        let menu = self.container_menu.as_ref()?;
        let at = menu.at;
        let max_height = menu.max_height;
        let backdrop = div().absolute().top_0().left_0().size_full().occlude();
        let frame = menu_frame(
            crate::fonts::ui_text_size(cx),
            "container-menu",
            &self.menu_focus,
            max_height,
            cx.listener(|this, _: &MouseDownEvent, window, cx| {
                this.close_container_menu(window, cx);
                cx.stop_propagation();
            }),
            &theme(cx),
        );
        let frame = self.container_menu_frame(frame, cx);
        let panel = anchored().position(at).snap_to_window().child(frame);
        Some([
            backdrop.into_any_element(),
            deferred(panel).with_priority(1).into_any_element(),
        ])
    }

    fn set_menu_mode(&mut self, mode: MenuMode, window: &mut Window, cx: &mut Context<Self>) {
        let Some(menu) = &mut self.menu else {
            return;
        };
        menu.mode = mode;
        menu.rename = None;
        menu.submenu = Submenu::None;
        self.confirm.disarm();
        self.menu_focus.focus(window);
        cx.notify();
    }

    /// Swaps the menu's rows for `submenu`, or back to the actions
    /// ([`Submenu::None`]).
    fn show_submenu(&mut self, submenu: Submenu, window: &mut Window, cx: &mut Context<Self>) {
        let Some(menu) = &mut self.menu else {
            return;
        };
        menu.submenu = submenu;
        self.confirm.disarm();
        self.menu_focus.focus(window);
        cx.notify();
    }

    /// Duplicates `session_id` to `target` and closes the menu; the reply
    /// places the copy ([`Self::place_duplicate`]). With Shift, a session in
    /// a repo or workspace asks for its spawn config instead, and the reply
    /// opens the spawn dialog filled from it ([`Self::on_spawn_config`]); a
    /// standalone session, which the dialog cannot target, duplicates
    /// plainly.
    fn duplicate_to(
        &mut self,
        session_id: &str,
        target: DuplicateTarget,
        shift: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let prefill = self
            .sidebar
            .session(session_id)
            .filter(|_| shift)
            .map(|session| PendingPrefill::of(session, target.clone()))
            .filter(|pending| pending.source.is_some());
        let host = self.host_of_session(session_id);
        let request = if let Some(pending) = prefill {
            Some(self.prefills.request(pending))
        } else {
            let request_id = crate::new_request_id();
            self.duplicates
                .request(&host, session_id, request_id, target)
        };
        if let Some(request) = request {
            self.send_duplicate(&host, request, cx);
        }
        self.close_session_menu(window, cx);
    }

    /// Sends a duplicate's `request` to `host`; to a remote host that is
    /// offline it is not sent: the duplicate ends at once with a toast.
    fn send_duplicate(&mut self, host: &HostKey, request: ClientMessage, cx: &mut Context<Self>) {
        if !self.is_remote_offline(host) {
            self.send_to(host, request);
            return;
        }
        if let ClientMessage::DuplicateSession {
            request_id: Some(request_id),
            ..
        } = &request
        {
            self.duplicates.fail(request_id);
        }
        self.push_offline_toast(host, cx);
    }

    /// The spawn config a Shift-duplicate asked for arrived: the dialog
    /// opens held on the config's repo or workspace, else the source's,
    /// filled from it over the Spawn defaults. When the dialog cannot open
    /// (the repo left, another dialog is open), a notice says why. A reply
    /// no Shift-duplicate waits for is dropped.
    pub(crate) fn on_spawn_config(
        &mut self,
        session_id: &str,
        config: Option<SpawnConfig>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pending) = self.prefills.take(session_id) else {
            return;
        };
        let lock = config
            .as_ref()
            .and_then(|config| Target::from_spawn(&config.target))
            .or_else(|| pending.source.clone())
            .map(Lock::for_target);
        let prefill = Prefill {
            config,
            open_in: pending.open_in(),
        };
        self.open_prefilled_dialog(lock, prefill, SHIFT_DUPLICATE_FAILED_TITLE, window, cx);
    }

    /// Moves a pane showing `session_id` to `target` and closes the menu:
    /// the pane whose header opened the menu, else the session's first.
    fn move_to(
        &mut self,
        session_id: &str,
        target: MoveTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let clicked = self.menu.as_ref().and_then(|menu| menu.pane.as_ref());
        let shown = SessionRef::new(self.host_of_session(session_id), session_id);
        let source = move_source(self.tabs.tabs(), &shown, clicked);
        self.close_session_menu(window, cx);
        let Some((tab_id, pane_id)) = source else {
            tracing::warn!(session = %session_id, "move to: no pane shows the session");
            return;
        };
        match target {
            MoveTarget::NewTab => self.move_pane_to_new_tab(&tab_id, &pane_id),
            MoveTarget::Tab(dst_tab_id) => {
                self.move_pane_to_tab(&tab_id, &pane_id, &dst_tab_id, cx);
            }
        }
    }

    /// Carries out a row of the group under the state actions for
    /// `session_id`: a submenu opens in the menu's place; any other row
    /// closes the menu and acts.
    fn choose_row(
        &mut self,
        session_id: &str,
        row: SessionRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match row {
            SessionRow::Duplicate => self.show_submenu(Submenu::Duplicate, window, cx),
            SessionRow::MoveTo => self.show_submenu(Submenu::MoveTo, window, cx),
            SessionRow::AddToCurrentTab { .. } => {
                let session = self
                    .sidebar
                    .find_session(session_id)
                    .filter(|(_, s)| can_attach(s))
                    .map(|(host, s)| (host, s.clone()));
                self.close_session_menu(window, cx);
                if let Some((host, session)) = session {
                    self.place_session(&host, &session, window, cx);
                }
            }
            SessionRow::AddToNewTab => {
                self.close_session_menu(window, cx);
                self.open_in_new_tab(session_id);
            }
            SessionRow::PopOut => {
                let key = match pane_to_pop(session_id, self.tabs.tabs(), self.tabs.active_id()) {
                    Some((tab_id, pane_id)) => PopoutKey::Pane { tab_id, pane_id },
                    None => PopoutKey::session(&self.host_of_session(session_id), session_id),
                };
                self.close_session_menu(window, cx);
                self.open_popout(key, window, cx);
            }
            SessionRow::RevealWorktree => {
                let path = self
                    .sidebar
                    .session(session_id)
                    .and_then(worktree_to_reveal)
                    .map(PathBuf::from);
                let host = self.host_of_session(session_id);
                self.close_session_menu(window, cx);
                if let Some(path) = path {
                    let job = OpenJob::DefaultApp {
                        path,
                        confirmed: true,
                    };
                    self.dispatch_open(&host, job, Then::Nothing, window, cx);
                }
            }
            SessionRow::BusyExcluded { excluded } => {
                self.send_to(
                    &self.host_of_session(session_id),
                    busy_exclusion_message(session_id, excluded),
                );
                self.close_session_menu(window, cx);
            }
        }
    }

    /// Sets `session_id`'s accent and frame colour both to `color`, or
    /// clears both (`None`), keeping its other fields and any change still
    /// on its way, and closes the menu. A choice that changes nothing sends
    /// nothing; a colour the protocol refuses raises a toast.
    fn pick_accent(
        &mut self,
        session_id: &str,
        color: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(own) = self.effective_appearance(session_id) {
            match AppearanceChange::accent_and_frame(color.map(appearance::hex)).normalized() {
                Ok(change) if change.changes_nothing_in(&own) => {}
                Ok(change) => self.send_session_appearance(session_id, &own, &change),
                Err(err) => {
                    tracing::warn!("not sending session {session_id}'s accent: {err}");
                    self.push_toast(
                        ToastKind::Error,
                        ACCENT_FAILED_TITLE,
                        Some(err.to_string()),
                        cx,
                    );
                }
            }
        }
        self.close_session_menu(window, cx);
    }

    /// Carries out `action` on `session_id`, disarming the header Stop. A
    /// worktree delete opens the delete-worktree confirm in the menu's
    /// place. A restart already on the way does nothing.
    pub(crate) fn choose_action(
        &mut self,
        session_id: &str,
        action: SessionAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.sidebar.session(session_id).cloned() else {
            return;
        };
        if action_disabled_tip(action, &session).is_some() {
            return;
        }
        cx.notify();
        self.confirm.disarm();
        match plan(action, &session, self.is_shown(session_id)) {
            Step::ConfirmWorktreeDelete => {
                self.close_session_menu(window, cx);
                self.open_delete_dialog(session_id, ConfirmOwner::Menu, window, cx);
            }
            Step::Send(messages) => {
                for msg in messages {
                    self.send_to(&self.host_of_session(session_id), msg);
                }
                self.close_session_menu(window, cx);
            }
            Step::EditName => self.start_session_rename(&session, window, cx),
            Step::Duplicate => {
                let request_id = crate::new_request_id();
                let target = if self.has_session_popout(session_id) {
                    DuplicateTarget::RestartInPopout
                } else {
                    DuplicateTarget::Restart
                };
                let host = self.host_of_session(session_id);
                if let Some(request) = self
                    .duplicates
                    .request(&host, session_id, request_id, target)
                {
                    self.send_duplicate(&host, request, cx);
                    self.close_session_menu(window, cx);
                }
            }
        }
    }

    /// Whether a tab pane or the session's own pop-out shows `session_id`,
    /// so a stop keeps it listed.
    fn is_shown(&self, session_id: &str) -> bool {
        let shown = SessionRef::new(self.host_of_session(session_id), session_id);
        find_tab_containing_session(self.tabs.tabs(), &shown).is_some()
            || self.popouts.session_key(session_id).is_some()
    }

    /// Swaps the menu's rows for a name editor seeded with the name the
    /// sidebar shows. Enter sends the rename; Esc goes back to the rows.
    fn start_session_rename(
        &mut self,
        session: &SessionSnapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = session
            .user_label
            .clone()
            .unwrap_or_else(|| session.label.clone());
        let input = cx.new(|cx| TextInput::new(name, "Session name", cx));
        let session_id = session.id.clone();
        let events = cx.subscribe_in(
            &input,
            window,
            move |this, input, event: &TextInputEvent, window, cx| match event {
                TextInputEvent::Submit => {
                    let text = input.read(cx).text().to_owned();
                    this.send_to(
                        &this.host_of_session(&session_id),
                        rename_message(&session_id, &text),
                    );
                    this.close_session_menu(window, cx);
                }
                TextInputEvent::Cancel => this.set_menu_mode(MenuMode::Actions, window, cx),
            },
        );
        input.read(cx).focus_handle(cx).focus(window);
        if let Some(menu) = &mut self.menu {
            menu.rename = Some((input, events));
        }
    }

    /// A duplicate that answers one of this client's requests is placed by
    /// its target: a restart takes its original's panes (focusing the
    /// first) or a new tab; a copy opens a new tab, or goes into its tab,
    /// which comes to the front with the copy focused. `copy` carries the
    /// host that replied; its layout goes to This PC and the original's
    /// discard to that host.
    pub(crate) fn place_duplicate(
        &mut self,
        request_id: Option<&str>,
        copy: HostedSession<'_>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(request_id) = request_id.filter(|id| self.duplicates.has_request(id)) else {
            return;
        };
        let sessions = self.sidebar.sessions_by_host();
        let Some(mut placed) = self
            .duplicates
            .place(request_id, copy, &self.tabs, &sessions)
        else {
            return;
        };
        self.confirm.disarm();
        match &placed.focus {
            PlacedFocus::NewTab => {
                let create_request = crate::new_request_id();
                self.tabs.arm_create(&create_request);
                for msg in &mut placed.messages {
                    if let protocol::ClientMessage::CreateTab { request_id, .. } = msg {
                        *request_id = Some(create_request.clone());
                    }
                }
            }
            PlacedFocus::Pane { tab_id, pane_id } => self.tabs.focus_pane(tab_id, pane_id),
            PlacedFocus::Tab(tab_id) => self.tabs.activate(tab_id),
            PlacedFocus::Popout { original } => {
                self.rekey_session_popout(original, &copy.session.id, cx);
            }
        }
        for msg in placed.messages {
            self.tabs.note_layout_request(&msg);
            self.send_to(&destination(&msg, &placed.session_host), msg);
        }
        if matches!(placed.focus, PlacedFocus::Pane { .. } | PlacedFocus::Tab(_)) {
            self.after_tabs_change(window, cx);
        }
    }

    /// A spawn or duplicate failure: its restart is no longer on the way.
    pub(crate) fn fail_duplicate(&mut self, request_id: Option<&str>) {
        if let Some(id) = request_id {
            self.duplicates.fail(id);
        }
    }

    /// Closes the menus, the delete-worktree confirm and the repo/workspace
    /// Remove dialog, and drops every armed confirm, as when the connection
    /// goes.
    pub(crate) fn reset_session_ui(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_session_menu(window, cx);
        self.close_container_menu(window, cx);
        self.close_shell_menu(window, cx);
        self.close_tab_menu(window, cx);
        self.close_sc_picker(window, cx);
        self.close_sc_file_menu(window, cx);
        self.close_more_menu(window, cx);
        self.close_delete_dialog(window, cx);
        self.close_repo_remove_dialog(window, cx);
    }

    /// The open menu over a layer that keeps a click outside it from
    /// reaching what lies beneath, while window `here` draws it.
    pub(crate) fn session_menu_layer(
        &self,
        here: WindowId,
        cx: &mut Context<Self>,
    ) -> Option<[AnyElement; 2]> {
        if !self.overlays.shows(Overlay::SessionMenu, here) {
            return None;
        }
        let menu = self.menu.as_ref()?;
        self.sidebar.session(&menu.session_id)?;
        let backdrop = div().absolute().top_0().left_0().size_full().occlude();
        let panel = anchored()
            .position(menu.at)
            .snap_to_window()
            .child(self.menu_panel(menu, cx));
        Some([
            backdrop.into_any_element(),
            deferred(panel).with_priority(1).into_any_element(),
        ])
    }

    fn menu_panel(&self, menu: &SessionMenu, cx: &mut Context<Self>) -> Stateful<Div> {
        let rows: Vec<AnyElement> = match &menu.rename {
            Some((input, _)) => vec![rename_row(input, &theme(cx))],
            None => self
                .rows()
                .into_iter()
                .map(|row| self.menu_row(&menu.session_id, &row, cx))
                .collect(),
        };
        let choosing_stop =
            menu.rename.is_none() && self.rows().iter().any(|row| row.entry == MenuEntry::Cancel);
        menu_frame(
            crate::fonts::ui_text_size(cx),
            "session-menu",
            &self.menu_focus,
            menu.max_height,
            cx.listener(|this, _: &MouseDownEvent, window, cx| {
                this.close_session_menu(window, cx);
                cx.stop_propagation();
            }),
            &theme(cx),
        )
        .when(choosing_stop, |panel| {
            panel.child(muted_row("Stop session?", &theme(cx)))
        })
        .children(rows)
        .children(self.session_group(&menu.session_id, cx))
        .children(self.accent_rows(menu, cx))
    }

    /// The rows under the state actions behind a separator, or, once one
    /// of them is chosen, its submenu.
    fn session_group(&self, session_id: &str, cx: &mut Context<Self>) -> Vec<AnyElement> {
        if let Some(lines) = self.duplicate_menu_lines() {
            return lines
                .into_iter()
                .map(|line| Self::submenu_line(session_id, line, Self::duplicate_to, cx))
                .collect();
        }
        if let Some(lines) = self.move_menu_lines() {
            return lines
                .into_iter()
                .map(|line| {
                    Self::submenu_line(
                        session_id,
                        line,
                        |this, id, target, _shift, window, cx| {
                            this.move_to(id, target, window, cx);
                        },
                        cx,
                    )
                })
                .collect();
        }
        let offered = self.offered_rows();
        if offered.is_empty() {
            return Vec::new();
        }
        let mut rows = vec![menu_separator(&theme(cx)).into_any_element()];
        rows.extend(
            offered
                .into_iter()
                .map(|offered| Self::offered_row(session_id, offered, cx)),
        );
        rows.extend(self.pane_view_menu_rows(cx));
        rows
    }

    /// A row of the group under the state actions for `session_id`: it
    /// acts or opens its submenu, or is dimmed with its reason as the
    /// tooltip.
    fn offered_row(session_id: &str, offered: OfferedRow, cx: &mut Context<Self>) -> AnyElement {
        let row = offered.row;
        let item = menu_item(row.selector(), row.label(), false, &theme(cx));
        if let Some(tip) = offered.disabled {
            return item
                .opacity(0.6)
                .cursor_default()
                .tooltip(tooltip(tip))
                .into_any_element();
        }
        let session_id = session_id.to_owned();
        item.on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
            this.choose_row(&session_id, row, window, cx);
        }))
        .into_any_element()
    }

    /// The checkable "Don't count as busy" row for `session_id`: its ✓
    /// shows the flag, and a click sends the opposite and closes the menu.
    fn busy_exclusion_row(session_id: &str, row: SessionRow, cx: &mut Context<Self>) -> AnyElement {
        let checked = matches!(row, SessionRow::BusyExcluded { excluded: true });
        let session_id = session_id.to_owned();
        check_row(row.selector(), row.label(), checked, &theme(cx))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.choose_row(&session_id, row, window, cx);
            }))
            .into_any_element()
    }

    /// A line of a submenu of tabs for `session_id`; a choice runs `pick`,
    /// told whether Shift was held.
    fn submenu_line<T: Clone + 'static>(
        session_id: &str,
        line: SubmenuLine<T>,
        pick: fn(&mut Self, &str, T, bool, &mut Window, &mut Context<Self>),
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match line {
            SubmenuLine::Back { selector } => menu_item(selector, SUBMENU_BACK, false, &theme(cx))
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                    this.show_submenu(Submenu::None, window, cx);
                }))
                .into_any_element(),
            SubmenuLine::Separator => menu_separator(&theme(cx)).into_any_element(),
            SubmenuLine::Choice {
                selector,
                label,
                choice,
            } => {
                let session_id = session_id.to_owned();
                menu_item(&selector, label, false, &theme(cx))
                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                        let shift = event.modifiers().shift;
                        pick(this, &session_id, choice.clone(), shift, window, cx);
                    }))
                    .into_any_element()
            }
        }
    }

    /// The Accent row under the actions, or, once it is chosen, the
    /// submenu: Back, the presets, the recent colours and Inherit.
    fn accent_rows(&self, menu: &SessionMenu, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let t = theme(cx);
        if self.accent_menu().is_none() {
            return Vec::new();
        }
        let session_id = menu.session_id.as_str();
        if menu.submenu != Submenu::Accent {
            let current = self.sidebar.appearance(Some(session_id)).accent.value;
            let level = Level::Session(session_id.to_owned());
            let editor = swatch_row(APPEARANCE_ROW, "Appearance…", None, &t).on_click(cx.listener(
                move |this, _: &ClickEvent, window, cx| {
                    this.open_appearance_editor(level.clone(), window, cx);
                },
            ));
            let row = swatch_row(ACCENT_ROW, "Accent ▸", Some(current), &t).on_click(cx.listener(
                |this, _: &ClickEvent, window, cx| this.show_submenu(Submenu::Accent, window, cx),
            ));
            let mut rows = vec![
                menu_separator(&t).into_any_element(),
                editor.into_any_element(),
                row.into_any_element(),
            ];
            rows.extend(
                self.busy_menu_row()
                    .map(|busy| Self::busy_exclusion_row(session_id, busy, cx)),
            );
            return rows;
        }
        let back = swatch_row(BACK_ROW, SUBMENU_BACK, None, &t).on_click(cx.listener(
            |this, _: &ClickEvent, window, cx| this.show_submenu(Submenu::None, window, cx),
        ));
        let mut rows = vec![
            back.into_any_element(),
            menu_separator(&t).into_any_element(),
        ];
        let theme_accent = self.sidebar.theme_base().accent;
        rows.extend(ACCENT_PRESETS.iter().map(|preset| {
            swatch_row(
                &preset_selector(preset.name),
                preset.name,
                Some(preset.shown(theme_accent)),
                &t,
            )
            .on_click(Self::accent_choice(session_id, preset.color, cx))
            .into_any_element()
        }));
        let recent = self.sidebar.recent_colors();
        if !recent.is_empty() {
            let swatches = recent.into_iter().map(|color| {
                let name = recent_selector(color);
                div()
                    .id(ElementId::Name(SharedString::from(name.clone())))
                    .debug_selector(|| name)
                    .size(px(16.0))
                    .rounded(px(3.0))
                    .border_1()
                    .border_color(gpui::rgb(t.border))
                    .bg(gpui::rgb(color))
                    .cursor_pointer()
                    .on_click(Self::accent_choice(session_id, Some(color), cx))
            });
            rows.push(muted_row("Recent", &t).into_any_element());
            rows.push(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(4.0))
                    .px(px(8.0))
                    .py(px(3.0))
                    .children(swatches)
                    .into_any_element(),
            );
        }
        rows.push(menu_separator(&t).into_any_element());
        rows.push(
            swatch_row(INHERIT_ROW, "Inherit", None, &t)
                .on_click(Self::accent_choice(session_id, None, cx))
                .into_any_element(),
        );
        rows
    }

    /// The click handler that picks `color` (or Inherit) for `session_id`.
    fn accent_choice(
        session_id: &str,
        color: Option<u32>,
        cx: &mut Context<Self>,
    ) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
        let session_id = session_id.to_owned();
        cx.listener(move |this, _: &ClickEvent, window, cx| {
            this.pick_accent(&session_id, color, window, cx);
        })
    }

    /// The open menu's rows for its session's current state; none while a
    /// submenu replaces them.
    fn rows(&self) -> Vec<Row> {
        let Some(menu) = self
            .menu
            .as_ref()
            .filter(|menu| menu.submenu == Submenu::None)
        else {
            return Vec::new();
        };
        let Some(session) = self.sidebar.session(&menu.session_id) else {
            return Vec::new();
        };
        // The labels name a tab pane; a session pop-out alone is none.
        let shown = SessionRef::new(self.host_of_session(&session.id), &session.id);
        let in_pane = find_tab_containing_session(self.tabs.tabs(), &shown).is_some();
        menu_entries(session, menu.mode)
            .into_iter()
            .map(|entry| {
                let (selector, label) = match entry {
                    MenuEntry::Action(action) => (
                        format!("menu-{}", action.key()),
                        self.action_label(session, action, in_pane),
                    ),
                    MenuEntry::StopChoice => ("menu-stop".to_owned(), "Stop session…"),
                    MenuEntry::Cancel => ("menu-cancel".to_owned(), "Cancel"),
                };
                Row {
                    selector,
                    label,
                    entry,
                }
            })
            .collect()
    }

    /// An action's text: on the way, or as it stands.
    fn action_label(
        &self,
        session: &SessionSnapshot,
        action: SessionAction,
        in_pane: bool,
    ) -> &'static str {
        if self.duplicates.is_pending(&session.id)
            && let Some(pending) = action.pending_label()
        {
            return pending;
        }
        action.label(session.has_per_session_worktree, in_pane)
    }

    fn menu_row(&self, session_id: &str, row: &Row, cx: &mut Context<Self>) -> AnyElement {
        match row.entry {
            MenuEntry::Action(action) => self
                .action_button(session_id, action, &row.selector, row.label, cx)
                .into_any_element(),
            MenuEntry::StopChoice => menu_item(&row.selector, row.label, true, &theme(cx))
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                    this.set_menu_mode(MenuMode::StopChoice, window, cx);
                }))
                .into_any_element(),
            MenuEntry::Cancel => menu_item(&row.selector, row.label, false, &theme(cx))
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                    this.set_menu_mode(MenuMode::Actions, window, cx);
                }))
                .into_any_element(),
        }
    }

    /// A button for `action`. It acts on the press, so the root's
    /// press-elsewhere disarm never sees its own confirm; a restart on the
    /// way is inert.
    fn action_button(
        &self,
        session_id: &str,
        action: SessionAction,
        selector: &str,
        label: &'static str,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        if let Some(tip) = self
            .sidebar
            .session(session_id)
            .and_then(|session| action_disabled_tip(action, session))
        {
            return disabled_row(selector, label, tip, &theme(cx));
        }
        let button = menu_item(selector, label, is_danger(action), &theme(cx));
        if !self.action_ready(session_id, action) {
            return button.opacity(0.6).cursor_default();
        }
        on_action_press(button, session_id, action, cx)
    }

    /// A compact button of a pane overlay for `action` on `session_id`,
    /// outlined or danger as the menu colours it; a disabled one stops its
    /// press.
    fn overlay_action_button(
        &self,
        session_id: &str,
        action: SessionAction,
        selector: &str,
        label: &'static str,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let kind = if is_danger(action) {
            ButtonKind::Danger
        } else {
            ButtonKind::Outlined
        };
        let ready = self.action_ready(session_id, action);
        let button = button(
            selector,
            button_look(
                crate::fonts::ui_text_size(cx),
                kind,
                ButtonSize::Compact,
                ready,
                Layer::Floating,
                &theme(cx),
            ),
        )
        .child(label);
        if ready {
            on_action_press(button, session_id, action, cx)
        } else {
            button
        }
    }

    /// Whether `action` on `session_id` can run now: a duplicate waits for
    /// the one in flight.
    fn action_ready(&self, session_id: &str, action: SessionAction) -> bool {
        action.pending_label().is_none() || !self.duplicates.is_pending(session_id)
    }

    /// The pane header's Pop out, which opens the pane in a window of its
    /// own on the press; dimmed while an open pop-out already shows its
    /// session. Nothing for an empty pane.
    pub(crate) fn header_pop_out(
        &self,
        tab_id: &str,
        pane_id: &str,
        session_id: Option<&str>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        session_id
            .and_then(|id| self.sidebar.session(id))
            .filter(|session| can_attach(session))?;
        let key = PopoutKey::Pane {
            tab_id: tab_id.to_owned(),
            pane_id: pane_id.to_owned(),
        };
        let button = header_text_button(
            &format!("pane-popout-{pane_id}"),
            "Pop out",
            theme(cx).muted,
            &theme(cx),
        );
        if self.popouts.blocking(&key, self.tabs.tabs()).is_some() {
            return Some(
                button
                    .opacity(0.6)
                    .cursor_default()
                    .hover(|style| style)
                    .tooltip(tooltip(POP_OUT_ALREADY_TIP))
                    .into_any_element(),
            );
        }
        let button = button.tooltip(tooltip(POP_OUT_TIP));
        Some(
            on_press(button, cx, move |this, window, cx| {
                this.open_popout(key.clone(), window, cx);
            })
            .into_any_element(),
        )
    }

    /// A pane pop-out's Dock back, which docks it back as the main grid's
    /// card's Dock back does.
    pub(crate) fn popout_dock_back(pane_id: &str, cx: &mut Context<Self>) -> AnyElement {
        let button = header_text_button(
            &format!("popout-dock-{pane_id}"),
            "Dock back",
            theme(cx).muted,
            &theme(cx),
        )
        .tooltip(tooltip(DOCK_BACK_TIP));
        on_press(button, cx, |this, window, cx| {
            this.dock_back_from_popout(window, cx);
        })
        .into_any_element()
    }

    /// A tab pop-out's Dock back: the window closes and the tab draws in
    /// the main window again.
    pub(crate) fn popout_tab_dock_back(tab_id: &str, cx: &mut Context<Self>) -> AnyElement {
        let button = header_text_button(
            &format!("popout-tab-dock-{tab_id}"),
            "Dock back",
            theme(cx).muted,
            &theme(cx),
        )
        .tooltip(tooltip(TAB_DOCK_BACK_TIP));
        on_press(button, cx, |this, window, cx| {
            this.dock_back_from_popout(window, cx);
        })
        .into_any_element()
    }

    /// A session pop-out's Close window, in place of Dock back: the window
    /// closes and the session is left as it is.
    pub(crate) fn popout_close_window(slot_id: &str, cx: &mut Context<Self>) -> AnyElement {
        let button = header_text_button(
            &format!("popout-close-{slot_id}"),
            "Close window",
            theme(cx).muted,
            &theme(cx),
        )
        .tooltip(tooltip(CLOSE_WINDOW_TIP));
        on_press(button, cx, |this, window, cx| {
            this.dock_back_from_popout(window, cx);
        })
        .into_any_element()
    }

    /// Docks back the pop-out whose window is `window`, found by the
    /// window's id at the press since its key follows a moved pane. The
    /// press is in that window, so it closes the window itself: the window
    /// cannot be reached by its handle while it dispatches the press.
    pub(crate) fn dock_back_from_popout(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.forget_popout_window(window.window_handle().window_id(), cx);
        window.remove_window();
    }

    /// Runs `act` in the main window, as a pop-out overlay's action that
    /// opens a main-window dialog does: when `window` is a pane or session
    /// pop-out, the pop-out docks back first and the main window is
    /// activated, so the dialog has the keyboard there. A tab pop-out stays
    /// open: the dialog opens in the main window, which comes to the front.
    fn in_main_window(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        act: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let in_popout = window
            .window_handle()
            .downcast::<crate::popout_view::PopoutView>()
            .is_some();
        if !in_popout {
            act(self, window, cx);
            return;
        }
        if self
            .window_tab(window.window_handle().window_id())
            .is_some()
        {
            self.open_in_main_window(cx, act);
            return;
        }
        self.dock_back_from_popout(window, cx);
        let main = cx
            .windows()
            .into_iter()
            .find_map(|handle| handle.downcast::<RootView>());
        let Some(main) = main else {
            tracing::warn!("a pop-out action found no main window to run in");
            return;
        };
        cx.defer(move |cx| {
            let ran = main.update(cx, |root, window, cx| {
                window.activate_window();
                act(root, window, cx);
            });
            if let Err(err) = ran {
                tracing::warn!("running a pop-out action in the main window: {err:#}");
            }
        });
    }

    /// The pane header's Stop (then "Confirm stop" / "Cancel"), or the exit
    /// code once the session has stopped, as drawn at `place`. Nothing for
    /// an empty pane. The buttons act on the press, as the tab close does.
    pub(crate) fn header_stop(
        &self,
        pane_id: &str,
        session_id: Option<&str>,
        place: StopPlace,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(session) = session_id.and_then(|id| self.sidebar.session(id)) else {
            return Vec::new();
        };
        if header_shows_exit_code(session) {
            let name = match place {
                StopPlace::Grid => format!("exit-code-{pane_id}"),
                StopPlace::Popout => format!("popout-exit-code-{pane_id}"),
            };
            return vec![
                div()
                    .debug_selector(|| name)
                    .flex_none()
                    .px(px(4.0))
                    .child(exit_code_label(session))
                    .into_any_element(),
            ];
        }
        let id = session.id.clone();
        let stop = SessionAction::StopKeepWorktree;
        let prefix = place.selector_prefix();
        if !self.confirm.is_armed(place, &id) {
            let button = header_text_button(
                &format!("{prefix}-{pane_id}"),
                "Stop",
                theme(cx).muted,
                &theme(cx),
            );
            return vec![
                on_press(button, cx, move |this, _, _| {
                    this.confirm.click(place, &id);
                })
                .into_any_element(),
            ];
        }
        let confirm = header_text_button(
            &format!("{prefix}-confirm-{pane_id}"),
            "Confirm stop",
            theme(cx).danger,
            &theme(cx),
        );
        let cancel = header_text_button(
            &format!("{prefix}-cancel-{pane_id}"),
            "Cancel",
            theme(cx).text,
            &theme(cx),
        );
        vec![
            on_press(confirm, cx, move |this, window, cx| {
                if this.confirm.click(place, &id) {
                    this.choose_action(&id, stop, window, cx);
                }
            })
            .into_any_element(),
            on_press(cancel, cx, |this, _, _| {
                this.confirm.disarm();
            })
            .into_any_element(),
        ]
    }

    /// The overlay's "New session…": the spawn dialog aimed at this pane,
    /// whose new session replaces the stopped one. Disabled with no repo,
    /// and while a spawn aimed at the pane is on its way.
    fn overlay_new_session(
        &self,
        tab_id: &str,
        pane_id: &str,
        session_id: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let pending = self.spawns.aims_at(tab_id, pane_id);
        let tip = if !self.has_repos() {
            NO_REPOS_TIP
        } else if pending {
            PANE_PENDING_TIP
        } else {
            NEW_SESSION_TIP
        };
        let button = BorderedButton {
            selector: format!("exited-new-session-{pane_id}"),
            label: "New session…",
            tip,
            enabled: self.has_repos() && !pending,
        };
        let (tab_id, pane_id, session_id) =
            (tab_id.to_owned(), pane_id.to_owned(), session_id.to_owned());
        bordered_button(button, cx, move |this, window, cx| {
            let (tab_id, pane_id, session_id) =
                (tab_id.clone(), pane_id.clone(), session_id.clone());
            // The spawn dialog is a main-window dialog.
            this.in_main_window(window, cx, move |this, window, cx| {
                this.new_session_in_pane(&tab_id, &pane_id, Some(&session_id), window, cx);
            });
        })
        .into_any_element()
    }

    /// The selectors of the stopped-pane overlays of the tab on screen:
    /// each overlay and its buttons, as [`Self::exited_overlay`] draws them.
    #[must_use]
    pub fn exited_overlay_selectors(&self) -> Vec<String> {
        let Some(grid) = self.tabs.active_tab().and_then(TabEntry::grid) else {
            return Vec::new();
        };
        let mut selectors = Vec::new();
        for pane in collect_panes(grid) {
            let Some(session) = pane
                .session
                .and_then(|id| self.sidebar.session(id))
                .filter(|session| pane_shows_exit(session))
            else {
                continue;
            };
            let mut keys: Vec<&str> = overlay_actions(session)
                .into_iter()
                .map(SessionAction::key)
                .collect();
            keys.insert(1, "new-session");
            selectors.push(format!("exited-{}", pane.id));
            selectors.push(format!("exited-card-{}", pane.id));
            selectors.extend(keys.iter().map(|key| format!("exited-{key}-{}", pane.id)));
        }
        selectors
    }

    /// The layer over an exited session's terminal: the exit code and what
    /// to do with the pane of `tab_id` it draws in, or, for a terminal in no
    /// tab (a session pop-out's), with the session, with no New session for
    /// a pane to fill.
    pub(crate) fn exited_overlay(
        &self,
        tab_id: Option<&str>,
        pane_id: &str,
        session_id: Option<&str>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let session = session_id
            .and_then(|id| self.sidebar.session(id))
            .filter(|session| pane_shows_exit(session))?;
        let in_pane = tab_id.is_some();
        let mut buttons: Vec<AnyElement> = overlay_actions(session)
            .into_iter()
            .map(|action| {
                let selector = format!("exited-{}-{pane_id}", action.key());
                let label = self.action_label(session, action, in_pane);
                self.overlay_action_button(&session.id, action, &selector, label, cx)
                    .into_any_element()
            })
            .collect();
        if let Some(tab_id) = tab_id {
            buttons.insert(
                1,
                self.overlay_new_session(tab_id, pane_id, &session.id, cx),
            );
        }
        Some(
            overlay_layer(
                crate::fonts::ui_text_size(cx),
                format!("exited-{pane_id}"),
                &theme(cx),
            )
            .child(
                overlay_card(format!("exited-card-{pane_id}"), &theme(cx))
                    .child(exited_message(session))
                    .child(overlay_buttons(buttons)),
            )
            .into_any_element(),
        )
    }

    /// The layer over an abandoned session's pane: what happened, the
    /// prompt it was running, and Resume and Dismiss.
    pub(crate) fn abandoned_overlay(
        &self,
        pane_id: &str,
        session_id: Option<&str>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let session = session_id
            .and_then(|id| self.sidebar.session(id))
            .filter(|session| action_state(session) == ActionState::Abandoned)?;
        let buttons: Vec<AnyElement> = ABANDONED_ACTIONS
            .iter()
            .map(|inline| {
                let selector = format!("abandoned-{}-{pane_id}", inline.key);
                let label = self.action_label(session, inline.action, true);
                self.overlay_action_button(&session.id, inline.action, &selector, label, cx)
                    .tooltip(tooltip(inline.tip))
                    .into_any_element()
            })
            .collect();
        let mut lines = abandoned_lines(session).into_iter();
        Some(
            overlay_layer(
                crate::fonts::ui_text_size(cx),
                format!("abandoned-{pane_id}"),
                &theme(cx),
            )
            .child(
                overlay_card(format!("abandoned-card-{pane_id}"), &theme(cx))
                    .children(lines.next())
                    .children(lines.map(|line| {
                        div()
                            .max_w(px(DIALOG_WIDTH))
                            .text_color(gpui::rgb(theme(cx).muted))
                            .child(line)
                    }))
                    .child(overlay_buttons(buttons)),
            )
            .into_any_element(),
        )
    }

    /// The banner atop an orphan's pane: its live stream is gone though its
    /// process still runs.
    pub(crate) fn orphan_banner(
        &self,
        offset: i8,
        pane_id: &str,
        session_id: Option<&str>,
        t: &UiTheme,
    ) -> Option<Div> {
        let session = session_id
            .and_then(|id| self.sidebar.session(id))
            .filter(|session| session.is_orphan)?;
        let name = format!("orphan-banner-{pane_id}");
        Some(
            div()
                .debug_selector(|| name)
                .flex_none()
                .px(px(8.0))
                .py(px(4.0))
                .bg(gpui::rgba((t.warning << 8) | 0x24))
                .text_size(ui_px_at(offset, UI_TEXT_SIZE))
                .text_color(gpui::rgb(t.text))
                .child(orphan_banner_text(
                    session,
                    self.sidebar.agents(&crate::target::HostKey::Local),
                )),
        )
    }

    /// The selectors of the abandoned overlays and orphan banners of the
    /// tab on screen, as [`Self::abandoned_overlay`] and
    /// [`Self::orphan_banner`] draw them.
    #[must_use]
    pub fn pane_notice_selectors(&self) -> Vec<String> {
        let Some(grid) = self.tabs.active_tab().and_then(TabEntry::grid) else {
            return Vec::new();
        };
        let mut selectors = Vec::new();
        for pane in collect_panes(grid) {
            let Some(session) = pane.session.and_then(|id| self.sidebar.session(id)) else {
                continue;
            };
            if action_state(session) == ActionState::Abandoned {
                selectors.push(format!("abandoned-{}", pane.id));
                selectors.push(format!("abandoned-card-{}", pane.id));
                selectors.extend(
                    ABANDONED_ACTIONS
                        .iter()
                        .map(|inline| format!("abandoned-{}-{}", inline.key, pane.id)),
                );
            }
            if session.is_orphan {
                selectors.push(format!("orphan-banner-{}", pane.id));
            }
        }
        selectors
    }

    /// The text of pane `pane_id`'s abandoned overlay or orphan banner in
    /// the tab on screen; empty when it shows neither.
    #[must_use]
    pub fn pane_notice(&self, pane_id: &str) -> Vec<String> {
        let Some(session) = self.active_pane_session(pane_id) else {
            return Vec::new();
        };
        let mut text = Vec::new();
        if session.is_orphan {
            text.push(orphan_banner_text(
                session,
                self.sidebar.agents(&crate::target::HostKey::Local),
            ));
        }
        if action_state(session) == ActionState::Abandoned {
            text.extend(abandoned_lines(session));
        }
        text
    }
}

/// A tinted layer over a whole pane, its content centred, that takes the
/// pointer from the terminal under it.
fn overlay_layer(offset: i8, name: String, t: &UiTheme) -> Stateful<Div> {
    div()
        .id(ElementId::Name(SharedString::from(name.clone())))
        .debug_selector(|| name)
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(10.0))
        .bg(gpui::rgba(t.scrim))
        .occlude()
        .text_size(ui_px_at(offset, UI_TEXT_SIZE))
        .text_color(gpui::rgb(t.text))
}

/// The card an overlay's message and buttons sit on, centred over the
/// dimmed terminal: the dialog's surface, edge and shadow.
fn overlay_card(id: String, t: &UiTheme) -> Div {
    div()
        .debug_selector(|| id)
        .flex()
        .flex_col()
        .items_center()
        .gap(px(10.0))
        .px(px(24.0))
        .py(px(16.0))
        .bg(gpui::rgb(t.modal))
        .border_1()
        .border_color(gpui::rgb(t.modal_edge))
        .rounded(px(MODAL_RADIUS))
        .shadow(vec![drop_shadow(t.dialog_shadow, 30.0, 80.0)])
}

fn overlay_buttons(buttons: Vec<AnyElement>) -> Div {
    div()
        .flex()
        .flex_wrap()
        .justify_center()
        .gap(px(6.0))
        .children(buttons)
}

impl RootView {
    /// The session whose delete-worktree confirm is open.
    #[must_use]
    pub fn delete_dialog_session(&self) -> Option<&str> {
        self.delete_dialog
            .as_ref()
            .map(|dialog| dialog.confirm.session_id())
    }

    /// The confirm's footer buttons as shown: selector and label.
    #[must_use]
    pub fn delete_dialog_buttons(&self) -> Vec<(String, String)> {
        let Some(dialog) = &self.delete_dialog else {
            return Vec::new();
        };
        dialog
            .confirm
            .buttons()
            .into_iter()
            .map(|button| (button.selector().to_owned(), dialog.confirm.label(button)))
            .collect()
    }

    /// The confirm's text: the intro, the status note, then one
    /// "repo branch fate" line per member.
    #[must_use]
    pub fn delete_dialog_text(&self) -> Vec<String> {
        let Some(dialog) = &self.delete_dialog else {
            return Vec::new();
        };
        let confirm = &dialog.confirm;
        let label = self.session_label(confirm.session_id());
        let mut lines: Vec<String> = dialog.owner.progress().into_iter().collect();
        lines.push(format!("Removing the worktree for {label}."));
        lines.extend(confirm.status_note().map(str::to_owned));
        lines.extend(
            confirm
                .member_rows()
                .into_iter()
                .map(|row| format!("{} {} {}", row.repo, row.branch, row.fate)),
        );
        lines
    }

    /// The selector of the confirm's focused button.
    #[must_use]
    pub fn delete_dialog_focus(&self) -> Option<String> {
        self.delete_dialog
            .as_ref()
            .map(|dialog| dialog.confirm.focused().selector().to_owned())
    }

    /// Whether the open confirm is the quit's branch-fate walk.
    pub(crate) fn exit_walk_confirm_open(&self) -> bool {
        self.delete_dialog
            .as_ref()
            .is_some_and(|dialog| matches!(dialog.owner, ConfirmOwner::ExitWalk { .. }))
    }

    /// Opens the confirm for `session_id` on behalf of `owner`, replacing
    /// any confirm open: asks the daemon for its branch fates, arms the
    /// fallback at the preview's deadline and takes the keyboard.
    pub(crate) fn open_delete_dialog(
        &mut self,
        session_id: &str,
        owner: ConfirmOwner,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.is_main_window(window) {
            let session_id = session_id.to_owned();
            self.open_in_main_window(cx, move |root, window, cx| {
                root.open_delete_dialog(&session_id, owner, window, cx);
            });
            return;
        }
        let confirm = DeleteWorktreeConfirm::new(session_id, (self.now)());
        self.send_to(
            &self.host_of_session(confirm.session_id()),
            confirm.request(),
        );
        self.delete_dialog = Some(DeleteDialog {
            confirm,
            owner,
            timer: None,
        });
        self.schedule_delete_dialog_tick(cx);
        self.dialog_focus.focus(window);
        cx.notify();
    }

    /// Arms a timer for the rest of the wait for the preview, or disarms it
    /// once the wait is over.
    fn schedule_delete_dialog_tick(&mut self, cx: &mut Context<Self>) {
        let now = (self.now)();
        let Some(dialog) = &mut self.delete_dialog else {
            return;
        };
        dialog.timer = dialog.confirm.deadline().map(|deadline| {
            let delay = deadline.saturating_duration_since(now);
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(delay).await;
                // Fails only when the view is gone, and the dialog with it.
                this.update(cx, Self::tick_delete_dialog).ok();
            })
        });
    }

    /// The timer fired: fall back when the deadline has passed, else wait
    /// out the rest, as the clock may lag the timer.
    fn tick_delete_dialog(&mut self, cx: &mut Context<Self>) {
        let now = (self.now)();
        let Some(dialog) = &mut self.delete_dialog else {
            return;
        };
        if dialog.confirm.tick(now) {
            cx.notify();
        }
        self.schedule_delete_dialog_tick(cx);
    }

    /// The daemon's branch fates for the open confirm's session.
    pub(crate) fn on_discard_preview(
        &mut self,
        session_id: &str,
        members: &[MemberBranchFate],
        cx: &mut Context<Self>,
    ) {
        if let Some(dialog) = &mut self.delete_dialog
            && dialog.confirm.on_preview(session_id, members)
        {
            cx.notify();
        }
    }

    /// Closes the confirm and hands the keyboard back to the active pane;
    /// closing the quit's walk ends the walk and returns to the exit dialog.
    pub(crate) fn close_delete_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self.delete_dialog.take() else {
            return;
        };
        match dialog.owner {
            ConfirmOwner::Menu => self.focus_active_pane(window, cx),
            ConfirmOwner::ExitWalk { .. } => self.cancel_exit_walk(window, cx),
        }
        cx.notify();
    }

    /// The user's answer. For the menu, a delete sends the confirm's
    /// messages for the session as it stands now; Cancel sends nothing. For
    /// the quit's walk, a delete records the branch choice and Cancel ends
    /// the walk.
    fn answer_delete_dialog(
        &mut self,
        button: DialogButton,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(dialog) = self.delete_dialog.take() else {
            return;
        };
        if let ConfirmOwner::ExitWalk { .. } = dialog.owner {
            match button.branch() {
                Some(branch) => {
                    self.answer_exit_walk(dialog.confirm.session_id(), branch, window, cx);
                }
                None => self.cancel_exit_walk(window, cx),
            }
            cx.notify();
            return;
        }
        let session = self.sidebar.session(dialog.confirm.session_id()).cloned();
        if let (Some(branch), Some(session)) = (button.branch(), session) {
            for msg in confirm_messages(&session, branch) {
                self.send_to(&self.host_of_session(&session.id), msg);
            }
        }
        self.focus_active_pane(window, cx);
        cx.notify();
    }

    /// A key while the confirm is open: Esc cancels, Enter and Space press
    /// the focused button, Tab and Shift+Tab move the focus.
    pub(crate) fn on_delete_dialog_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match keystroke.key.as_str() {
            "escape" => self.close_delete_dialog(window, cx),
            "enter" | "space" => {
                if let Some(button) = self.delete_dialog.as_ref().map(|d| d.confirm.focused()) {
                    self.answer_delete_dialog(button, window, cx);
                }
            }
            "tab" => {
                if let Some(dialog) = &mut self.delete_dialog {
                    dialog.confirm.move_focus(!keystroke.modifiers.shift);
                    cx.notify();
                }
            }
            _ => {}
        }
    }

    /// The confirm over a backdrop that takes every click beneath it; a
    /// click on the backdrop itself does nothing.
    pub(crate) fn delete_dialog_layer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let dialog = self.delete_dialog.as_ref()?;
        let confirm = &dialog.confirm;
        let close = dialog_button(
            "delete-worktree-dialog-close",
            "✕".to_owned(),
            dialog_look(crate::fonts::ui_text_size(cx), false, true, &theme(cx)),
            false,
        )
        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
            this.close_delete_dialog(window, cx);
        }));
        let progress = dialog
            .owner
            .progress()
            .map(|line| div().text_color(gpui::rgb(theme(cx).muted)).child(line));
        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Delete worktree?"),
                    )
                    .children(progress),
            )
            .child(close);
        let buttons: Vec<AnyElement> = confirm
            .buttons()
            .into_iter()
            .map(|button| {
                let focused = confirm.focused() == button;
                dialog_button(
                    button.selector(),
                    confirm.label(button),
                    dialog_look(
                        crate::fonts::ui_text_size(cx),
                        button.is_danger(),
                        true,
                        &theme(cx),
                    ),
                    focused,
                )
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.answer_delete_dialog(button, window, cx);
                }))
                .into_any_element()
            })
            .collect();
        let panel = modal_panel(
            crate::fonts::ui_text_size(cx),
            "delete-worktree-panel",
            &theme(cx),
        )
        .track_focus(&self.dialog_focus)
        .child(header)
        .child(self.delete_dialog_body(confirm, &theme(cx)))
        .child(div().flex().justify_end().gap(px(6.0)).children(buttons));
        Some(backdrop("delete-worktree-dialog", panel, &theme(cx)))
    }

    /// Whose worktree goes, the wait or its failure, and each member's
    /// branch with its fate.
    fn delete_dialog_body(&self, confirm: &DeleteWorktreeConfirm, t: &UiTheme) -> Div {
        let intro = div()
            .flex()
            .flex_wrap()
            .child("Removing the worktree for ")
            .child(
                div()
                    .font_weight(FontWeight::BOLD)
                    .child(self.session_label(confirm.session_id())),
            )
            .child(".");
        let note = confirm
            .status_note()
            .map(|note| div().text_color(gpui::rgb(t.muted)).child(note));
        let members = confirm.member_rows().into_iter().map(|row| {
            div()
                .flex()
                .flex_wrap()
                .gap(px(8.0))
                .child(div().font_weight(FontWeight::SEMIBOLD).child(row.repo))
                .child(div().text_color(gpui::rgb(t.muted)).child(row.branch))
                .child(row.fate)
        });
        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(intro)
            .children(note)
            .children(members)
    }
}

/// A dialog's button drawn as `look` (see [`dialog_look`]); the focused
/// one wears the focus ring in `look`'s accent. A disabled one is dimmed and
/// its press stops at it; its caller attaches no handler to it.
pub(crate) fn dialog_button(
    selector: &str,
    label: String,
    look: DialogLook,
    focused: bool,
) -> Stateful<Div> {
    let DialogLook { button: look, ring } = look;
    button(selector, look)
        .when(focused, |button| button.child(ring))
        .child(label)
}

/// How a dialog's button is drawn: the button, and the focus ring it wears
/// while focused.
pub(crate) struct DialogLook {
    button: ButtonLook,
    ring: Div,
}

/// The look of a dialog's button in `t`: outlined, or outlined with the
/// danger colour's text, on the floating layer; dimmed unless `enabled`.
pub(crate) fn dialog_look(offset: i8, danger: bool, enabled: bool, t: &UiTheme) -> DialogLook {
    let kind = if danger {
        ButtonKind::Danger
    } else {
        ButtonKind::Outlined
    };
    let size = ButtonSize::Regular;
    DialogLook {
        button: button_look(offset, kind, size, enabled, Layer::Floating, t),
        ring: focus_ring(size.radius(), t),
    }
}

/// `button`, which runs `action` on `session_id` on a left press and keeps
/// the press from the root.
fn on_action_press(
    button: Stateful<Div>,
    session_id: &str,
    action: SessionAction,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    let session_id = session_id.to_owned();
    button.on_mouse_down(
        MouseButton::Left,
        cx.listener(move |this, _: &MouseDownEvent, window, cx| {
            cx.stop_propagation();
            if action == SessionAction::RemovePaneDeleteWorktree {
                // Its confirm is a main-window dialog.
                let session_id = session_id.clone();
                this.in_main_window(window, cx, move |this, window, cx| {
                    this.choose_action(&session_id, action, window, cx);
                });
            } else {
                this.choose_action(&session_id, action, window, cx);
            }
        }),
    )
}

/// Runs `act` on a left press and keeps the press from the root.
fn on_press(
    button: Stateful<Div>,
    cx: &mut Context<RootView>,
    act: impl Fn(&mut RootView, &mut Window, &mut Context<RootView>) + 'static,
) -> Stateful<Div> {
    button.on_mouse_down(
        MouseButton::Left,
        cx.listener(move |this, _: &MouseDownEvent, window, cx| {
            cx.stop_propagation();
            act(this, window, cx);
            cx.notify();
        }),
    )
}

/// A bordered text button of an empty pane, the empty main area or the
/// stopped-pane overlay.
pub(crate) struct BorderedButton {
    pub selector: String,
    pub label: &'static str,
    pub tip: &'static str,
    pub enabled: bool,
}

/// `button`, a compact outlined button that runs `act` on a left press as
/// [`on_press`] does. A disabled one is dimmed, has no hover and the default
/// cursor, and its press only stops at it: `act` is never attached.
pub(crate) fn bordered_button(
    button: BorderedButton,
    cx: &mut Context<RootView>,
    act: impl Fn(&mut RootView, &mut Window, &mut Context<RootView>) + 'static,
) -> Stateful<Div> {
    let BorderedButton {
        selector,
        label,
        tip,
        enabled,
    } = button;
    let base = outlined_button(
        crate::fonts::ui_text_size(cx),
        &selector,
        ButtonSize::Compact,
        enabled,
        Layer::Floating,
        &theme(cx),
    )
    .tooltip(tooltip(tip))
    .child(label);
    if enabled {
        on_press(base, cx, act)
    } else {
        base
    }
}

/// Whether an action shows in the danger colour, as in the Tauri menu.
fn is_danger(action: SessionAction) -> bool {
    action.deletes_worktree()
        || matches!(
            action,
            SessionAction::StopKeepWorktree
                | SessionAction::RemovePane
                | SessionAction::DismissAbandoned
        )
}

/// A clickable row of the menu or the overlay.
pub(crate) fn menu_item(
    selector: &str,
    label: impl Into<SharedString>,
    danger: bool,
    t: &UiTheme,
) -> Stateful<Div> {
    let name = selector.to_owned();
    let label: SharedString = label.into();
    let hover = t.popover_hover;
    div()
        .id(ElementId::Name(SharedString::from(name.clone())))
        .debug_selector(|| name)
        .px(px(8.0))
        .py(px(3.0))
        .rounded(px(4.0))
        .cursor_pointer()
        .text_color(gpui::rgb(if danger { t.danger_text } else { t.text }))
        .hover(move |style| style.bg(gpui::rgb(hover)))
        .child(label)
}

/// A menu row that is drawn but cannot be pressed: muted, with `tip` as its
/// tooltip.
pub(crate) fn disabled_row(
    selector: &str,
    label: impl Into<SharedString>,
    tip: impl Into<SharedString>,
    t: &UiTheme,
) -> Stateful<Div> {
    inert_row(selector, label, t).tooltip(tooltip(tip))
}

/// A menu row that is drawn muted and cannot be pressed, with no tooltip.
fn inert_row(selector: &str, label: impl Into<SharedString>, t: &UiTheme) -> Stateful<Div> {
    let name = selector.to_owned();
    let label: SharedString = label.into();
    div()
        .id(ElementId::Name(SharedString::from(name.clone())))
        .debug_selector(move || name)
        .flex()
        .items_center()
        .px(px(8.0))
        .py(px(3.0))
        .rounded(px(4.0))
        .text_color(gpui::rgb(t.muted))
        .cursor_default()
        .child(label)
}

/// A clickable menu row with a colour swatch before its label; no swatch
/// leaves the space empty so the labels line up.
fn swatch_row(
    selector: &str,
    label: &'static str,
    color: Option<u32>,
    t: &UiTheme,
) -> Stateful<Div> {
    let swatch = div().flex_none().size(px(12.0)).rounded(px(3.0));
    slot_row(
        selector,
        swatch.when_some(color, |swatch, color| swatch.bg(gpui::rgb(color))),
        label,
        t,
    )
}

/// A clickable menu row with a ✓ in the swatch's slot while `checked`,
/// tagged `<selector>-check`; unchecked leaves the slot empty.
fn check_row(selector: &str, label: &'static str, checked: bool, t: &UiTheme) -> Stateful<Div> {
    let check = format!("{selector}-check");
    let slot = div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(12.0))
        .text_size(px(11.0))
        .line_height(px(11.0));
    let slot = if checked {
        slot.debug_selector(|| check).child("✓")
    } else {
        slot
    };
    slot_row(selector, slot, label, t)
}

/// A clickable menu row with `slot` before its label, tagged `selector`.
fn slot_row(selector: &str, slot: Div, label: &'static str, t: &UiTheme) -> Stateful<Div> {
    let name = selector.to_owned();
    let hover = t.popover_hover;
    div()
        .id(ElementId::Name(SharedString::from(name.clone())))
        .debug_selector(|| name)
        .flex()
        .items_center()
        .gap(px(8.0))
        .px(px(8.0))
        .py(px(3.0))
        .rounded(px(4.0))
        .cursor_pointer()
        .hover(move |style| style.bg(gpui::rgb(hover)))
        .child(slot)
        .child(label)
}

/// A thin line between groups of menu rows.
pub(crate) fn menu_separator(t: &UiTheme) -> Div {
    div().h(px(1.0)).my(px(4.0)).bg(gpui::rgb(t.menu_rule))
}

/// A small text button in a pane header.
fn header_text_button(
    selector: &str,
    label: &'static str,
    color: u32,
    t: &UiTheme,
) -> Stateful<Div> {
    let name = selector.to_owned();
    let hover = t.hover_bg;
    div()
        .id(ElementId::Name(SharedString::from(name.clone())))
        .debug_selector(|| name)
        .flex_none()
        .px(px(4.0))
        .rounded(px(3.0))
        .cursor_pointer()
        .text_color(gpui::rgb(color))
        .hover(move |style| style.bg(gpui::rgb(hover)))
        .child(label)
}

/// A non-clickable line of the menu or the overlay, in the muted color.
pub(crate) fn muted_row(text: impl Into<SharedString>, t: &UiTheme) -> Div {
    let text: SharedString = text.into();
    div()
        .px(px(8.0))
        .py(px(3.0))
        .text_color(gpui::rgb(t.muted))
        .child(text)
}

/// The name editor with its hint.
fn rename_row(input: &Entity<TextInput>, t: &UiTheme) -> AnyElement {
    div()
        .debug_selector(|| "menu-rename-input".to_owned())
        .flex()
        .flex_col()
        .gap(px(4.0))
        .p(px(4.0))
        .child(
            div()
                .text_color(gpui::rgb(t.muted))
                .child("Rename session (blank restores the default)"),
        )
        .child(field_frame(true, t).child(input.clone()))
        .into_any_element()
}

impl RootView {
    /// Whether the gutter-dot command menu is open.
    #[must_use]
    pub fn shell_menu_open(&self) -> bool {
        self.shell_menu.is_some()
    }

    /// The selectors of the gutter menu's rows, empty while it is closed.
    #[must_use]
    pub fn shell_menu_rows(&self) -> Vec<String> {
        self.shell_menu
            .as_ref()
            .map(|_| SHELL_MENU_ROWS.map(str::to_owned).to_vec())
            .unwrap_or_default()
    }

    /// The header row the gutter menu shows.
    #[must_use]
    pub fn shell_menu_header(&self) -> Option<String> {
        self.shell_menu.as_ref().map(|menu| menu.header.clone())
    }

    /// Whether the gutter menu's "Re-run command" acts: its command is not
    /// empty, its session still takes input, and its pane can take the
    /// command (a multi-line one only as a bracketed paste).
    #[must_use]
    pub fn shell_menu_can_rerun(&self, cx: &App) -> bool {
        self.shell_menu
            .as_ref()
            .is_some_and(|menu| self.can_rerun(menu, cx))
    }

    /// Opens the gutter menu of the dot at `index` of the pane `pane_id`,
    /// which the pane anchored at `at`.
    pub(crate) fn open_shell_menu(
        &mut self,
        pane_id: &str,
        index: usize,
        at: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(command) = self.pane_shell_command(pane_id, index, cx) else {
            return;
        };
        self.close_session_menu(window, cx);
        self.close_tab_menu(window, cx);
        self.close_more_menu(window, cx);
        self.close_empty_pane_menu(window, cx);
        self.overlays
            .opened(Overlay::ShellMenu, window.window_handle().window_id());
        self.shell_menu = Some(ShellMenu {
            pane_id: pane_id.to_owned(),
            session_id: self.pane_session(pane_id),
            header: command.header,
            command: command.command,
            output: command.output,
            at,
            max_height: window.viewport_size().height * MENU_MAX_SHARE,
        });
        self.menu_focus.focus(window);
        cx.notify();
    }

    /// Closes the gutter menu and hands the keyboard back to the terminal of
    /// the window it was open in.
    pub(crate) fn close_shell_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.shell_menu.take().is_some() {
            let owner = self.overlays.owner(Overlay::ShellMenu);
            self.hand_back_focus(owner, window, cx);
            cx.notify();
        }
    }

    /// Whether `menu`'s command can be typed again: it is not empty, its
    /// session still takes input, and its pane takes the command's bytes.
    fn can_rerun(&self, menu: &ShellMenu, cx: &App) -> bool {
        !menu.command.is_empty()
            && menu.session_id.as_deref().is_some_and(|id| {
                self.sidebar
                    .session(id)
                    .is_some_and(|session| action_state(session) == ActionState::Running)
            })
            && self
                .panes
                .get(&menu.pane_id)
                .is_some_and(|slot| slot.view().read(cx).rerun_bytes(&menu.command).is_some())
    }

    /// The open gutter menu over a layer that keeps a click outside it from
    /// reaching what lies beneath. The layer takes the wheel too, so a
    /// scroll anywhere, which would move the menu's dot, closes it. Only
    /// window `here`, the one it opened in, draws it.
    pub(crate) fn shell_menu_layer(
        &self,
        here: WindowId,
        cx: &mut Context<Self>,
    ) -> Option<[AnyElement; 2]> {
        if !self.overlays.shows(Overlay::ShellMenu, here) {
            return None;
        }
        let menu = self.shell_menu.as_ref()?;
        let backdrop = div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .occlude()
            .on_scroll_wheel(cx.listener(|this, _: &ScrollWheelEvent, window, cx| {
                this.close_shell_menu(window, cx);
            }));
        let panel = anchored()
            .position(menu.at)
            .snap_to_window()
            .child(self.shell_menu_panel(menu, cx));
        Some([
            backdrop.into_any_element(),
            deferred(panel).with_priority(1).into_any_element(),
        ])
    }

    fn shell_menu_panel(&self, menu: &ShellMenu, cx: &mut Context<Self>) -> Stateful<Div> {
        let both = if menu.output.is_empty() {
            menu.command.clone()
        } else {
            format!("{}\n{}", menu.command, menu.output)
        };
        menu_frame(
            crate::fonts::ui_text_size(cx),
            "shell-menu",
            &self.menu_focus,
            menu.max_height,
            cx.listener(|this, _: &MouseDownEvent, window, cx| {
                this.close_shell_menu(window, cx);
                cx.stop_propagation();
            }),
            &theme(cx),
        )
        .child(muted_row(menu.header.clone(), &theme(cx)))
        .child(Self::shell_copy_row(
            SHELL_MENU_ROWS[0],
            "Copy command",
            menu.command.clone(),
            cx,
        ))
        .child(Self::shell_copy_row(
            SHELL_MENU_ROWS[1],
            "Copy output only",
            menu.output.clone(),
            cx,
        ))
        .child(Self::shell_copy_row(
            SHELL_MENU_ROWS[2],
            "Copy command and output",
            both,
            cx,
        ))
        .child(self.shell_rerun_row(menu, cx))
    }

    /// A copy row: it puts `text` on the clipboard, chip included, and
    /// closes the menu.
    fn shell_copy_row(
        selector: &str,
        label: &'static str,
        text: String,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        menu_item(selector, label, false, &theme(cx))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.copy_to_clipboard(&text, cx);
                    this.close_shell_menu(window, cx);
                }),
            )
            .into_any_element()
    }

    /// The re-run row: inert and dimmed while its session is stopped, its
    /// command empty, or its session gone.
    fn shell_rerun_row(&self, menu: &ShellMenu, cx: &mut Context<Self>) -> AnyElement {
        let row = menu_item(SHELL_MENU_ROWS[3], "Re-run command", false, &theme(cx));
        if !self.can_rerun(menu, cx) {
            return row.opacity(0.6).cursor_default().into_any_element();
        }
        let (pane_id, command) = (menu.pane_id.clone(), menu.command.clone());
        row.on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                // First, so the keyboard it hands the tab's focused pane
                // back is then taken by the command's pane.
                this.close_shell_menu(window, cx);
                this.rerun_command(&pane_id, &command, window, cx);
            }),
        )
        .into_any_element()
    }

    /// Types `command` at `pane_id`'s session without a newline and makes
    /// that pane its tab's focused one, keyboard included, as the menu's
    /// re-run does.
    fn rerun_command(
        &mut self,
        pane_id: &str,
        command: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.panes.get(pane_id).map(|slot| slot.view().clone()) else {
            return;
        };
        view.update(cx, |pane, cx| pane.rerun_command(command, window, cx));
        self.pane_focused(pane_id, window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REFUSAL: &str = "This host opens no files on this PC";

    fn repo_facts(presets: &MenuPresets) -> ContainerFacts<'_> {
        ContainerFacts {
            shape: ContainerShape::Level {
                remove_label: "Remove repo".to_owned(),
            },
            launch: LaunchFact::Ready,
            presets,
            has_folder: true,
            folder_gone: None,
            has_vscode_job: true,
            vscode_gone: None,
            has_copy_path: true,
            local_files_refusal: None,
        }
    }

    /// The action the row tagged `selector` must carry.
    fn action_of(selector: &str) -> Option<ContainerAction> {
        let action = match selector {
            CONTAINER_SPAWN_ROW => ContainerAction::Spawn,
            CONTAINER_LAUNCH_LAST_ROW => ContainerAction::LaunchLast,
            CONTAINER_APPEARANCE_ROW => ContainerAction::Appearance,
            CONTAINER_PRESET_ROW => ContainerAction::Preset(0),
            CONTAINER_EXPLORER_ROW => ContainerAction::Explorer,
            CONTAINER_VSCODE_ROW => ContainerAction::VsCode,
            CONTAINER_COPY_PATH_ROW => ContainerAction::CopyPath,
            CONTAINER_REMOVE_ROW => ContainerAction::Remove,
            other => {
                let index = other
                    .strip_prefix(CONTAINER_PRESET_ROW)?
                    .strip_prefix('-')?
                    .parse()
                    .ok()?;
                ContainerAction::Preset(index)
            }
        };
        Some(action)
    }

    fn row<'a>(groups: &'a [Vec<ContainerRow>], selector: &str) -> Option<&'a ContainerRow> {
        groups.iter().flatten().find(|row| row.selector == selector)
    }

    fn state(groups: &[Vec<ContainerRow>], selector: &str) -> Option<RowState> {
        row(groups, selector).map(|row| row.state.clone())
    }

    #[test]
    fn a_reopened_menu_ignores_the_previous_opens_check() {
        let checking = MenuTargets::Checking;
        assert!(
            !check_applies(&checking, 2, 1),
            "the previous open's check fills nothing"
        );
        assert!(
            check_applies(&checking, 2, 2),
            "the open's own check fills it"
        );
        let known = MenuTargets::Known {
            missing: Vec::new(),
        };
        assert!(
            !check_applies(&known, 2, 2),
            "a menu that has its report takes no second"
        );
    }

    #[test]
    fn every_disabled_row_has_a_tip_and_every_row_its_action() {
        let presets = [
            MenuPresets::Loading,
            MenuPresets::Failed("presets.json: bad".to_owned()),
            MenuPresets::Listed(Vec::new()),
        ];
        let launches = [LaunchFact::None, LaunchFact::Headless, LaunchFact::Ready];
        let mut rows_seen = 0;
        for presets in &presets {
            for launch in launches {
                for mask in 0..16_u8 {
                    let facts = ContainerFacts {
                        launch,
                        has_folder: mask & 1 != 0,
                        has_vscode_job: mask & 2 != 0,
                        has_copy_path: mask & 4 != 0,
                        local_files_refusal: (mask & 8 != 0).then_some(REFUSAL),
                        ..repo_facts(presets)
                    };
                    for row in container_row_groups(&facts).iter().flatten() {
                        rows_seen += 1;
                        if let RowState::Disabled(tip) = &row.state {
                            assert!(!tip.is_empty(), "{} has an empty tip", row.selector);
                        }
                        assert_eq!(
                            Some(row.action.clone()),
                            action_of(&row.selector),
                            "{} fires its own action",
                            row.selector
                        );
                    }
                }
            }
        }
        assert!(rows_seen > 0, "the builder made rows");
    }

    #[test]
    fn a_disabled_row_carries_its_tip_and_an_enabled_row_none() {
        let loading = MenuPresets::Loading;
        let workspace = ContainerFacts {
            has_folder: false,
            has_copy_path: false,
            launch: LaunchFact::Headless,
            ..repo_facts(&loading)
        };
        let groups = container_row_groups(&workspace);
        let no_folder = Some(RowState::Disabled(CONTAINER_NO_FOLDER_TIP.to_owned()));
        assert_eq!(state(&groups, CONTAINER_EXPLORER_ROW), no_folder);
        assert_eq!(state(&groups, CONTAINER_COPY_PATH_ROW), no_folder);
        assert_eq!(
            state(&groups, CONTAINER_VSCODE_ROW),
            Some(RowState::Enabled)
        );
        assert_eq!(
            state(&groups, CONTAINER_LAUNCH_LAST_ROW),
            Some(RowState::Disabled(HEADLESS_UNSUPPORTED_TIP.to_owned()))
        );
        assert_eq!(state(&groups, CONTAINER_PRESET_ROW), Some(RowState::Inert));

        let remote = ContainerFacts {
            local_files_refusal: Some(REFUSAL),
            ..repo_facts(&loading)
        };
        let groups = container_row_groups(&remote);
        let refused = Some(RowState::Disabled(REFUSAL.to_owned()));
        assert_eq!(state(&groups, CONTAINER_EXPLORER_ROW), refused);
        assert_eq!(state(&groups, CONTAINER_VSCODE_ROW), refused);
        assert_eq!(
            state(&groups, CONTAINER_COPY_PATH_ROW),
            Some(RowState::Enabled),
            "Copy path needs no local opener"
        );
    }

    #[test]
    fn a_tab_container_offers_only_spawn_and_only_on_a_grid_tab() {
        let presets = MenuPresets::Listed(Vec::new());
        let grid = ContainerFacts {
            shape: ContainerShape::Tab { spawnable: true },
            ..repo_facts(&presets)
        };
        assert_eq!(
            container_row_groups(&grid),
            [[ContainerRow::new(
                CONTAINER_SPAWN_ROW,
                "Spawn session here",
                RowState::Enabled,
                ContainerAction::Spawn,
            )]]
        );
        let other = ContainerFacts {
            shape: ContainerShape::Tab { spawnable: false },
            ..repo_facts(&presets)
        };
        assert_eq!(
            container_row_groups(&other),
            Vec::<Vec<ContainerRow>>::new()
        );
    }

    /// Rows `a`, a disabled `b`, an inert `c`, `d`, then a disabled `e`.
    fn nav_rows() -> Vec<ContainerRow> {
        let row = |selector: &str, state| {
            ContainerRow::new(selector, selector, state, ContainerAction::Spawn)
        };
        vec![
            row("a", RowState::Enabled),
            row("b", RowState::Disabled("no".to_owned())),
            row("c", RowState::Inert),
            ContainerRow::new("d", "d", RowState::Enabled, ContainerAction::CopyPath),
            row("e", RowState::Disabled("no".to_owned())),
        ]
    }

    #[test]
    fn down_and_up_skip_disabled_rows_and_wrap_at_the_ends() {
        let rows = nav_rows();
        let step = |current, step| step_highlight(&rows, current, step);
        assert_eq!(step(Some("a"), MenuStep::Down).as_deref(), Some("d"));
        assert_eq!(step(Some("d"), MenuStep::Down).as_deref(), Some("a"));
        assert_eq!(step(Some("a"), MenuStep::Up).as_deref(), Some("d"));
        assert_eq!(step(Some("d"), MenuStep::Up).as_deref(), Some("a"));
    }

    #[test]
    fn no_highlight_starts_at_the_first_or_last_enabled_row() {
        let rows = nav_rows();
        assert_eq!(
            step_highlight(&rows, None, MenuStep::Down).as_deref(),
            Some("a")
        );
        assert_eq!(
            step_highlight(&rows, None, MenuStep::Up).as_deref(),
            Some("d")
        );
        assert_eq!(
            step_highlight(&rows, Some("b"), MenuStep::Down).as_deref(),
            Some("a"),
            "a highlight on a row no longer enabled starts again"
        );
    }

    #[test]
    fn home_and_end_jump_to_the_first_and_last_enabled_rows() {
        let rows = nav_rows();
        assert_eq!(
            step_highlight(&rows, Some("d"), MenuStep::Home).as_deref(),
            Some("a")
        );
        assert_eq!(
            step_highlight(&rows, Some("a"), MenuStep::End).as_deref(),
            Some("d"),
            "the disabled last row is skipped"
        );
    }

    #[test]
    fn a_menu_with_no_enabled_row_has_no_highlight() {
        let rows: Vec<ContainerRow> = nav_rows()
            .into_iter()
            .filter(|row| !row.enabled())
            .collect();
        assert_eq!(step_highlight(&rows, None, MenuStep::Down), None);
        assert_eq!(step_highlight(&[], Some("a"), MenuStep::End), None);
    }

    #[test]
    fn shift_f10_and_the_menu_key_open_a_menu_and_plain_f10_does_not() {
        let opens = |text: &str| Keystroke::parse(text).is_ok_and(|ks| is_menu_key(&ks));
        assert!(opens("shift-f10"));
        assert!(opens("menu"));
        assert!(!opens("f10"));
        assert!(!opens("ctrl-shift-f10"));
        assert!(!opens("enter"));
    }

    #[test]
    fn activating_the_highlight_returns_its_row_action_and_never_a_disabled_one() {
        let rows = nav_rows();
        assert_eq!(
            highlighted_row(&rows, Some("d")).map(|row| row.action.clone()),
            Some(ContainerAction::CopyPath)
        );
        assert_eq!(highlighted_row(&rows, Some("b")), None);
        assert_eq!(highlighted_row(&rows, None), None);
    }
}
