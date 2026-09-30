//! The Settings modal: a tab list on the left and the chosen tab's rows
//! beside it. General holds keep-awake and copy on select, Notifications
//! the per-reason notification toggles and Windows' toast state, Appearance
//! the appearance editor at the app level, App title the window title's
//! settings; every change applies at once. The main window's title, which
//! the App title tab controls, is kept here too.

use std::sync::Arc;
use std::time::Duration;

use gpui::{
    AnyElement, ClickEvent, Context, Div, Entity, EntityId, FocusHandle, Focusable as _,
    FontWeight, Keystroke, MouseButton, MouseDownEvent, Stateful, Subscription, Window, div,
    prelude::*, px,
};
use protocol::{AttentionReason, ClientMessage, CodexSandbox, PermissionMode};

use crate::appearance;
use crate::appearance_view::{Level, close_footer};
use crate::buttons::{field_frame, focus_ring};
use crate::mouse::CopyOnSelect;
use crate::notify::{NotifyState, WINDOWS_NOTIFICATION_SETTINGS};
use crate::palette::{CHIP, HOVER, LINE, RAISED, SUBTLE, TEXT_2, TRANSPARENT};
use crate::session_menu::{backdrop, dialog_button};
use crate::sidebar::{LeafDensity, SidebarView};
use crate::spawn_form::{
    APPROVAL_CHOICES, CODEX_SANDBOX_CHOICES, approval_label, codex_sandbox_label,
};
use crate::spawn_view::{
    CLAUDE_LOCKED, CODEX_LOCKED, Look, checkbox_row, choice_button, close_button, dialog_card,
    dialog_title_bar, field, segmented,
};
use crate::tabs::tab_session_counts;
use crate::text_input::{TextChanged, TextInput, TextInputEvent};
use crate::window_title::compute_title;
use crate::{RootView, TEXT};

/// The tab list's width, its padding and edge included.
const TAB_LIST_WIDTH: f32 = 150.0;
/// A tab row's height and corner radius, as on the Settings board.
const TAB_ROW_HEIGHT: f32 = 34.0;
const TAB_ROW_RADIUS: f32 = 7.0;
/// As wide as the Appearance tab's two columns, so the modal keeps its size
/// from tab to tab.
const CONTENT_WIDTH: f32 = 656.0;
/// The padding round the shown tab's rows.
const CONTENT_PAD_X: f32 = 18.0;
const CONTENT_PAD_Y: f32 = 16.0;
/// The modal's width: the tab list, the content and its padding, and the
/// card's 1 px edges.
const PANEL_WIDTH: f32 = TAB_LIST_WIDTH + CONTENT_WIDTH + 2.0 * CONTENT_PAD_X + 2.0;
/// The size of a section's head, and of a hint.
const SECTION_HEAD_SIZE: f32 = 14.0;
const HINT_SIZE: f32 = 12.5;
/// The corner radius of a text link, and of the title preview's chip.
const LINK_RADIUS: f32 = 4.0;
const CHIP_RADIUS: f32 = 5.0;
/// How long the window title waits for its inputs to settle, so a session
/// flickering between working and idle does not flicker the taskbar.
const TITLE_DEBOUNCE: Duration = Duration::from_millis(350);
const NOT_CONNECTED: &str = "(daemon not connected)";
const WORKTREES_HINT: &str = "Per-session git worktrees land under this directory. Changes take effect for new sessions only — sessions already spawned keep their existing worktree paths.";

/// A tab of the Settings modal, in the order the list shows them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum SettingsTab {
    #[default]
    General,
    Notifications,
    SpawnDefaults,
    Worktrees,
    Appearance,
    AppTitle,
}

impl SettingsTab {
    const ALL: [Self; 6] = [
        Self::General,
        Self::Notifications,
        Self::SpawnDefaults,
        Self::Worktrees,
        Self::Appearance,
        Self::AppTitle,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Notifications => "Notifications",
            Self::SpawnDefaults => "Spawn defaults",
            Self::Worktrees => "Worktrees",
            Self::Appearance => "Appearance",
            Self::AppTitle => "App title",
        }
    }

    const fn selector(self) -> &'static str {
        match self {
            Self::General => "settings-tab-general",
            Self::Notifications => "settings-tab-notifications",
            Self::SpawnDefaults => "settings-tab-spawn-defaults",
            Self::Worktrees => "settings-tab-worktrees",
            Self::Appearance => "settings-tab-appearance",
            Self::AppTitle => "settings-tab-app-title",
        }
    }

    /// The tab above (`down` false) or below this one, stopping at the ends.
    fn step(self, down: bool) -> Self {
        let at = Self::ALL.iter().position(|tab| *tab == self).unwrap_or(0);
        let next = if down {
            (at + 1).min(Self::ALL.len() - 1)
        } else {
            at.saturating_sub(1)
        };
        Self::ALL[next]
    }
}

/// The General tab's Default view choices, in the order they show.
const SIDEBAR_VIEW_CHOICES: [SidebarView; 2] = [SidebarView::Repos, SidebarView::Tabs];

/// The General tab's Leaf density choices, in the order they show.
const LEAF_DENSITY_CHOICES: [LeafDensity; 2] = [LeafDensity::Comfortable, LeafDensity::Compact];

/// A control of a Settings tab that Tab can reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingsControl {
    KeepAwake,
    SidebarView(SidebarView),
    LeafDensity(LeafDensity),
    CopyOnSelect,
    TitleCount,
    TitleSuffix,
    NotifySettingsLink,
    NotifyAwaitingInput,
    NotifyStopped,
    NotifyError,
    SpawnTrusted,
    SpawnApproval(Option<PermissionMode>),
    SpawnCodexSandbox(Option<CodexSandbox>),
    WorktreesPath,
    WorktreesBrowse,
    WorktreesSave,
    WorktreesReset,
    WorktreesManage,
}

impl SettingsControl {
    const fn selector(self) -> &'static str {
        match self {
            Self::KeepAwake => "settings-keep-awake-toggle",
            Self::SidebarView(SidebarView::Repos) => "settings-sidebar-view-repos",
            Self::SidebarView(SidebarView::Tabs) => "settings-sidebar-view-tabs",
            Self::LeafDensity(LeafDensity::Comfortable) => "settings-leaf-density-comfortable",
            Self::LeafDensity(LeafDensity::Compact) => "settings-leaf-density-compact",
            Self::CopyOnSelect => "settings-terminal-copy-on-selection",
            Self::TitleCount => "settings-title-busy-count",
            Self::TitleSuffix => "settings-title-product-suffix",
            Self::NotifySettingsLink => "settings-open-windows-notifications",
            Self::NotifyAwaitingInput => "settings-notify-awaiting-input",
            Self::NotifyStopped => "settings-notify-stopped",
            Self::NotifyError => "settings-notify-error",
            Self::SpawnTrusted => "settings-spawn-trusted",
            Self::SpawnApproval(mode) => match mode {
                None => "settings-spawn-approval-cli-default",
                Some(PermissionMode::Default) => "settings-spawn-approval-default",
                Some(PermissionMode::AcceptEdits) => "settings-spawn-approval-accept-edits",
                Some(PermissionMode::BypassPermissions) => {
                    "settings-spawn-approval-bypass-permissions"
                }
                Some(PermissionMode::Plan) => "settings-spawn-approval-plan",
            },
            Self::SpawnCodexSandbox(sandbox) => match sandbox {
                None => "settings-spawn-codex-sandbox-cli-default",
                Some(CodexSandbox::ReadOnly) => "settings-spawn-codex-sandbox-read-only",
                Some(CodexSandbox::WorkspaceWrite) => {
                    "settings-spawn-codex-sandbox-workspace-write"
                }
                Some(CodexSandbox::DangerFullAccess) => {
                    "settings-spawn-codex-sandbox-danger-full-access"
                }
            },
            Self::WorktreesPath => "settings-worktrees-root-input",
            Self::WorktreesBrowse => "settings-worktrees-root-browse",
            Self::WorktreesSave => "settings-worktrees-root-save",
            Self::WorktreesReset => "settings-worktrees-root-reset",
            Self::WorktreesManage => "settings-worktrees-open-manager",
        }
    }
}

/// The Worktrees tab's path field, which holds the user's override only,
/// and the save it waits to hear back.
pub(crate) struct WorktreesTab {
    input: Entity<TextInput>,
    focus: FocusHandle,
    /// The field's text, kept as it changes.
    draft: String,
    /// How many Save or Reset sends still owe the daemon's echo.
    awaiting: u32,
    /// The daemon echoed the last save.
    saved: bool,
    _subscriptions: Vec<Subscription>,
}

/// The daemon's keep-awake setting and whether it holds the machine awake
/// now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct KeepAwake {
    pub(crate) enabled: bool,
    pub(crate) active: bool,
}

/// What the keep-awake button offers.
const fn keep_awake_label(state: Option<KeepAwake>) -> &'static str {
    match state {
        Some(KeepAwake { enabled: true, .. }) => "Allow sleep while sessions run",
        _ => "Keep this machine awake while sessions run",
    }
}

/// The line beside the keep-awake button.
const fn keep_awake_status(state: Option<KeepAwake>) -> &'static str {
    match state {
        None => "(daemon not connected)",
        Some(KeepAwake {
            enabled: true,
            active: true,
        }) => "Holding the machine awake — a session is live.",
        Some(KeepAwake {
            enabled: true,
            active: false,
        }) => "Sleep is allowed until a session starts.",
        Some(KeepAwake { enabled: false, .. }) => "The machine may sleep while sessions run.",
    }
}

impl RootView {
    /// Whether the Settings modal is open.
    #[must_use]
    pub fn settings_open(&self) -> bool {
        self.appearance_editor
            .as_ref()
            .is_some_and(|editor| editor.level == Level::App)
    }

    /// The Settings tabs' labels, in the list's order.
    #[must_use]
    pub fn settings_tab_labels(&self) -> Vec<&'static str> {
        SettingsTab::ALL.iter().map(|tab| tab.label()).collect()
    }

    /// The label of the Settings tab shown.
    #[must_use]
    pub fn settings_tab(&self) -> &'static str {
        self.settings_tab.label()
    }

    /// The keep-awake button's label and whether it can be pressed.
    #[must_use]
    pub fn keep_awake_button(&self) -> (&'static str, bool) {
        (keep_awake_label(self.keep_awake), self.keep_awake.is_some())
    }

    /// The keep-awake status line.
    #[must_use]
    pub fn keep_awake_status(&self) -> &'static str {
        keep_awake_status(self.keep_awake)
    }

    /// The App title tab's preview: the title for a tab named "Tab name"
    /// with one of three terminals busy.
    #[must_use]
    pub fn title_preview(&self) -> String {
        let title = &self.sidebar.ui_state().title;
        let preview = compute_title(
            Some((1, 3)),
            Some("Tab name"),
            title.show_count,
            title.suffix,
        );
        if preview.is_empty() {
            "(empty)".to_owned()
        } else {
            preview
        }
    }

    /// What in the Settings frame has the keyboard: `"list"` for the tab
    /// list, else the focused control's selector.
    #[must_use]
    pub fn settings_focus(&self, window: &Window) -> Option<&'static str> {
        if self.settings_tabs_focus.is_focused(window) {
            return Some("list");
        }
        self.focused_settings_control(window)
            .map(SettingsControl::selector)
    }

    /// The shown tab's controls Tab reaches, in order; a disabled one is
    /// left out.
    fn settings_controls(&self) -> Vec<SettingsControl> {
        match self.settings_tab {
            SettingsTab::General => {
                let keep_awake = self.keep_awake.map(|_| SettingsControl::KeepAwake);
                keep_awake
                    .into_iter()
                    .chain(SIDEBAR_VIEW_CHOICES.map(SettingsControl::SidebarView))
                    .chain(LEAF_DENSITY_CHOICES.map(SettingsControl::LeafDensity))
                    .chain([SettingsControl::CopyOnSelect])
                    .collect()
            }
            SettingsTab::AppTitle => {
                vec![SettingsControl::TitleCount, SettingsControl::TitleSuffix]
            }
            SettingsTab::Notifications => vec![
                SettingsControl::NotifySettingsLink,
                SettingsControl::NotifyAwaitingInput,
                SettingsControl::NotifyStopped,
                SettingsControl::NotifyError,
            ],
            SettingsTab::SpawnDefaults => self.spawn_default_controls(),
            SettingsTab::Worktrees => {
                let mut controls = vec![
                    SettingsControl::WorktreesPath,
                    SettingsControl::WorktreesBrowse,
                ];
                if self.worktrees_save_enabled() {
                    controls.push(SettingsControl::WorktreesSave);
                }
                if self.worktrees_override().is_some() {
                    controls.push(SettingsControl::WorktreesReset);
                }
                controls.push(SettingsControl::WorktreesManage);
                controls
            }
            SettingsTab::Appearance => Vec::new(),
        }
    }

    /// The Spawn defaults tab's controls: the toggle, then the two choice
    /// rows while trusted launch has not locked them.
    fn spawn_default_controls(&self) -> Vec<SettingsControl> {
        let mut controls = vec![SettingsControl::SpawnTrusted];
        if !self.sidebar.ui_state().spawn.trusted {
            controls.extend(APPROVAL_CHOICES.map(SettingsControl::SpawnApproval));
            controls.extend(CODEX_SANDBOX_CHOICES.map(SettingsControl::SpawnCodexSandbox));
        }
        controls
    }

    /// The control holding the keyboard, if one does.
    fn focused_settings_control(&self, window: &Window) -> Option<SettingsControl> {
        self.settings_control.filter(|control| {
            let has_keyboard = if *control == SettingsControl::WorktreesPath {
                self.worktrees_path_focused(window)
            } else {
                self.appearance_focus.is_focused(window)
            };
            has_keyboard && self.settings_controls().contains(control)
        })
    }

    /// Gives the keyboard to `control`: the path field takes it itself,
    /// every other control through the modal's focus.
    pub(crate) fn focus_settings_control(&mut self, control: SettingsControl, window: &mut Window) {
        self.settings_control = Some(control);
        match (&self.worktrees_tab, control) {
            (Some(tab), SettingsControl::WorktreesPath) => tab.focus.focus(window),
            _ => self.appearance_focus.focus(window),
        }
    }

    /// Whether the Worktrees tab's path field has the keyboard.
    pub(crate) fn worktrees_path_focused(&self, window: &Window) -> bool {
        self.settings_open()
            && self.settings_tab == SettingsTab::Worktrees
            && self
                .worktrees_tab
                .as_ref()
                .is_some_and(|tab| tab.focus.is_focused(window))
    }

    /// Moves the keyboard one step along the ring the tab list and the
    /// shown tab's controls make, backwards with `back`. From anywhere off
    /// the ring it goes to the tab list.
    fn step_settings_focus(&mut self, back: bool, window: &mut Window, cx: &mut Context<Self>) {
        let controls = self.settings_controls();
        let at = if self.settings_tabs_focus.is_focused(window) {
            Some(0)
        } else {
            self.focused_settings_control(window)
                .and_then(|control| controls.iter().position(|c| *c == control))
                .map(|index| index + 1)
        };
        let len = controls.len() + 1;
        let next = match at {
            None => 0,
            Some(at) if back => (at + len - 1) % len,
            Some(at) => (at + 1) % len,
        };
        if next == 0 {
            self.settings_control = None;
            self.settings_tabs_focus.focus(window);
        } else {
            self.focus_settings_control(controls[next - 1], window);
        }
        cx.notify();
    }

    fn press_settings_control(
        &mut self,
        control: SettingsControl,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_settings_control(control, window);
        match control {
            SettingsControl::KeepAwake => self.toggle_keep_awake(),
            SettingsControl::SidebarView(view) => self.set_sidebar_view(view, cx),
            SettingsControl::LeafDensity(density) => self.set_leaf_density(density, cx),
            SettingsControl::CopyOnSelect => self.toggle_copy_on_select(cx),
            SettingsControl::TitleCount => self.toggle_title_count(window, cx),
            SettingsControl::TitleSuffix => self.toggle_title_suffix(window, cx),
            SettingsControl::NotifySettingsLink => self.open_windows_notification_settings(cx),
            SettingsControl::NotifyAwaitingInput => {
                self.toggle_notification(AttentionReason::AwaitingInput, cx);
            }
            SettingsControl::NotifyStopped => {
                self.toggle_notification(AttentionReason::Stopped, cx);
            }
            SettingsControl::NotifyError => self.toggle_notification(AttentionReason::Error, cx),
            SettingsControl::SpawnTrusted => self.toggle_spawn_trusted(cx),
            SettingsControl::SpawnApproval(mode) => self.set_spawn_permission_mode(mode, cx),
            SettingsControl::SpawnCodexSandbox(sandbox) => {
                self.set_spawn_codex_sandbox(sandbox, cx);
            }
            SettingsControl::WorktreesPath => {}
            SettingsControl::WorktreesBrowse => self.browse_worktrees_root(cx),
            SettingsControl::WorktreesSave => self.save_worktrees_root(),
            SettingsControl::WorktreesReset => self.reset_worktrees_root(cx),
            SettingsControl::WorktreesManage => self.open_worktrees_manager(window, cx),
        }
        cx.notify();
    }

    /// The worktrees root the user set, if they set one.
    fn worktrees_override(&self) -> Option<&str> {
        match &self.worktrees_root {
            Some((root, true)) => Some(root),
            _ => None,
        }
    }

    /// Whether the path field differs from the override (empty when none).
    fn worktrees_save_enabled(&self) -> bool {
        self.worktrees_tab
            .as_ref()
            .is_some_and(|tab| tab.draft.trim() != self.worktrees_override().unwrap_or(""))
    }

    /// The Worktrees tab's line on the root in use.
    #[must_use]
    pub fn worktrees_active_line(&self) -> String {
        match &self.worktrees_root {
            Some((root, true)) => format!("Active: {root} — user override"),
            Some((root, false)) => format!("Active: {root} — default (env or platform fallback)"),
            None => format!("Active: {NOT_CONNECTED} — default (env or platform fallback)"),
        }
    }

    /// The Save button's label and whether it can be pressed.
    #[must_use]
    pub fn worktrees_save_button(&self) -> (&'static str, bool) {
        let enabled = self.worktrees_save_enabled();
        let saved = self.worktrees_tab.as_ref().is_some_and(|tab| tab.saved);
        (if saved && !enabled { "Saved" } else { "Save" }, enabled)
    }

    /// Whether Reset to default can be pressed.
    #[must_use]
    pub fn worktrees_reset_enabled(&self) -> bool {
        self.worktrees_override().is_some()
    }

    /// The Worktrees tab's path field, while the tab has been shown.
    #[must_use]
    pub fn worktrees_path(&self) -> Option<String> {
        self.worktrees_tab.as_ref().map(|tab| tab.draft.clone())
    }

    /// The path field afresh, holding the override.
    fn new_worktrees_tab(&self, window: &mut Window, cx: &mut Context<Self>) -> WorktreesTab {
        let draft = self.worktrees_override().unwrap_or("").to_owned();
        let placeholder = self
            .worktrees_root
            .as_ref()
            .map_or(NOT_CONNECTED, |(root, _)| root.as_str())
            .to_owned();
        let input = cx.new(|cx| TextInput::new(draft.clone(), placeholder, cx));
        let focus = input.read(cx).focus_handle(cx);
        let keys = cx.subscribe_in(
            &input,
            window,
            |this, _, event: &TextInputEvent, window, cx| match event {
                TextInputEvent::Submit => {
                    this.press_settings_control(SettingsControl::WorktreesSave, window, cx);
                }
                TextInputEvent::Cancel => this.close_appearance_editor(window, cx),
            },
        );
        let edits = cx.subscribe_in(&input, window, |this, input, _: &TextChanged, _, cx| {
            let text = input.read(cx).text().to_owned();
            if let Some(tab) = &mut this.worktrees_tab {
                tab.draft = text;
            }
            cx.notify();
        });
        WorktreesTab {
            input,
            focus,
            draft,
            awaiting: 0,
            saved: false,
            _subscriptions: vec![keys, edits],
        }
    }

    /// Save: the field as the override, or none when it is empty.
    fn save_worktrees_root(&mut self) {
        if !self.worktrees_save_enabled() {
            return;
        }
        let Some(tab) = &mut self.worktrees_tab else {
            return;
        };
        let trimmed = tab.draft.trim();
        let path = (!trimmed.is_empty()).then(|| trimmed.to_owned());
        tab.awaiting += 1;
        tab.saved = false;
        tracing::info!(?path, "settings: saving the worktrees root");
        self.send(ClientMessage::SetWorktreesRoot { path });
    }

    fn reset_worktrees_root(&mut self, cx: &mut Context<Self>) {
        if self.worktrees_override().is_none() {
            return;
        }
        if let Some(tab) = &mut self.worktrees_tab {
            tab.awaiting += 1;
            tab.saved = false;
            tab.draft.clear();
            tab.input.update(cx, |input, cx| input.set_text("", cx));
        }
        tracing::info!("settings: resetting the worktrees root");
        self.send(ClientMessage::SetWorktreesRoot { path: None });
    }

    /// Browse: the folder picked fills the path field, if the field is
    /// still the one that asked.
    fn browse_worktrees_root(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = &self.worktrees_tab else {
            return;
        };
        let asked = tab.input.entity_id();
        let picked = (self.pick_folder)(cx);
        cx.spawn(async move |this, cx| {
            let Some(path) = picked.await else {
                return;
            };
            let path = path.to_string_lossy().into_owned();
            // Fails only when the view is gone, and its window with it.
            this.update(cx, |this, cx| this.finish_worktrees_browse(asked, path, cx))
                .ok();
        })
        .detach();
    }

    fn finish_worktrees_browse(&mut self, asked: EntityId, path: String, cx: &mut Context<Self>) {
        let Some(tab) = &mut self.worktrees_tab else {
            return;
        };
        if tab.input.entity_id() != asked {
            return;
        }
        tab.draft.clone_from(&path);
        tab.input.update(cx, |input, cx| input.set_text(path, cx));
        cx.notify();
    }

    /// The daemon's worktrees root. Each echo answers one of this tab's
    /// sends, and the one answering the last shows `Saved` and puts the
    /// override the daemon settled on in the field. An echo answering none
    /// is another client's change: it fills the field only while the field
    /// still reads as the daemon left it.
    pub(crate) fn on_worktrees_root_changed(
        &mut self,
        root: &str,
        is_override: bool,
        cx: &mut Context<Self>,
    ) {
        let previous = self
            .worktrees_root
            .as_ref()
            .filter(|(_, was_overridden)| *was_overridden)
            .map_or_else(String::new, |(root, _)| root.clone());
        self.worktrees_root = Some((root.to_owned(), is_override));
        if let Some(tab) = &mut self.worktrees_tab {
            let placeholder = root.to_owned();
            tab.input
                .update(cx, |input, cx| input.set_placeholder(placeholder, cx));
            let mine = tab.awaiting > 0;
            tab.awaiting = tab.awaiting.saturating_sub(1);
            let settles = mine && tab.awaiting == 0;
            let untouched = !mine && tab.draft == previous;
            if settles || untouched {
                if settles {
                    tab.saved = true;
                }
                tab.draft = if is_override {
                    root.to_owned()
                } else {
                    String::new()
                };
                let text = tab.draft.clone();
                tab.input.update(cx, |input, cx| input.set_text(text, cx));
            }
        }
        cx.notify();
    }

    /// Forgets the root and the tab's field, as a new connection does.
    pub(crate) fn reset_worktrees_tab(&mut self) {
        self.worktrees_root = None;
        self.worktrees_tab = None;
    }

    /// The title last set on the window; empty until the first is.
    #[must_use]
    pub fn window_title(&self) -> &str {
        &self.window_title
    }

    /// Ctrl+, or the gear: the Settings modal on its General tab, with the
    /// keyboard on the tab list.
    pub(crate) fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_appearance_editor(Level::App, window, cx);
        if self.settings_open() {
            self.settings_tab = SettingsTab::General;
            self.settings_control = None;
            self.settings_tabs_focus.focus(window);
        }
    }

    fn select_settings_tab(
        &mut self,
        tab: SettingsTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.settings_tab = tab;
        self.settings_control = None;
        if tab == SettingsTab::Notifications {
            self.read_notify_state(window, cx);
        }
        if tab == SettingsTab::Worktrees {
            self.worktrees_tab = Some(self.new_worktrees_tab(window, cx));
        }
        self.settings_tabs_focus.focus(window);
        cx.notify();
    }

    /// Reads Windows' toast setting on a background thread; the tab shows
    /// `checking…` until it arrives.
    fn read_notify_state(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.notifications.state = None;
        let notifier = Arc::clone(&self.notifications.notifier);
        let read = cx
            .background_executor()
            .spawn(async move { notifier.state() });
        cx.spawn_in(window, async move |this, cx| {
            let state = read.await;
            // Fails only when the view is gone, and its window with it.
            this.update(cx, |this, cx| {
                this.notifications.state = Some(state);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The Notifications tab's line on Windows' toast setting.
    #[must_use]
    pub fn notifications_state_line(&self) -> String {
        format!(
            "Windows notifications: {}",
            NotifyState::line_label(self.notifications.state)
        )
    }

    fn toggle_notification(&mut self, reason: AttentionReason, cx: &mut Context<Self>) {
        let on = !self.sidebar.ui_state().notifications.fires(reason);
        self.sidebar.set_notification(reason, on);
        self.save_ui();
        cx.notify();
    }

    fn toggle_spawn_trusted(&mut self, cx: &mut Context<Self>) {
        let on = !self.sidebar.ui_state().spawn.trusted;
        self.sidebar.set_spawn_trusted(on);
        self.save_ui();
        cx.notify();
    }

    fn set_spawn_permission_mode(&mut self, mode: Option<PermissionMode>, cx: &mut Context<Self>) {
        self.sidebar.set_spawn_permission_mode(mode);
        self.save_ui();
        cx.notify();
    }

    fn set_spawn_codex_sandbox(&mut self, sandbox: Option<CodexSandbox>, cx: &mut Context<Self>) {
        self.sidebar.set_spawn_codex_sandbox(sandbox);
        self.save_ui();
        cx.notify();
    }

    /// Opens Windows' notification settings on a background thread; a
    /// failure is only logged.
    fn open_windows_notification_settings(&self, cx: &mut Context<Self>) {
        let opener = Arc::clone(&self.opener);
        cx.background_executor()
            .spawn(async move {
                if let Err(err) = opener.url(WINDOWS_NOTIFICATION_SETTINGS) {
                    tracing::warn!("could not open Windows' notification settings: {err}");
                }
            })
            .detach();
    }

    /// A key while Settings is open; returns whether it was the frame's.
    /// Tab and Shift+Tab walk the ring of the tab list and the shown tab's
    /// controls, Up and Down on the tab list change the tab, and Space or
    /// Enter presses the focused control. On Appearance, a key anywhere
    /// but the tab list is the appearance editor's.
    pub(crate) fn on_settings_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let mods = &keystroke.modifiers;
        if !self.settings_open() || mods.control || mods.alt || mods.platform || mods.function {
            return false;
        }
        let on_list = self.settings_tabs_focus.is_focused(window);
        if self.settings_tab == SettingsTab::Appearance && !on_list {
            return false;
        }
        let typing = self.worktrees_path_focused(window);
        match (keystroke.key.as_str(), mods.shift) {
            ("tab", back) => {
                self.step_settings_focus(back, window, cx);
                true
            }
            _ if typing => false,
            ("up" | "down", false) if on_list => {
                let tab = self.settings_tab.step(keystroke.key == "down");
                self.select_settings_tab(tab, window, cx);
                true
            }
            ("space" | "enter", false) => match self.focused_settings_control(window) {
                Some(control) => {
                    self.press_settings_control(control, window, cx);
                    true
                }
                None => false,
            },
            _ => false,
        }
    }

    /// The daemon's keep-awake state, or `None` when this connection has
    /// not reported it.
    pub(crate) fn set_keep_awake(&mut self, state: Option<KeepAwake>) {
        self.keep_awake = state;
    }

    fn toggle_keep_awake(&mut self) {
        if let Some(state) = self.keep_awake {
            self.send(ClientMessage::SetKeepAwake {
                enabled: !state.enabled,
            });
        }
    }

    /// Sets how much room each session leaf takes, saving it when it
    /// changed.
    fn set_leaf_density(&mut self, density: LeafDensity, cx: &mut Context<Self>) {
        if self.sidebar.set_leaf_density(density) {
            self.save_ui();
        }
        cx.notify();
    }

    fn toggle_copy_on_select(&mut self, cx: &mut Context<Self>) {
        let on = !self.sidebar.ui_state().general.copy_on_select;
        self.sidebar.set_copy_on_select(on);
        cx.set_global(CopyOnSelect(on));
        self.save_ui();
        cx.notify();
    }

    fn toggle_title_count(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let on = !self.sidebar.ui_state().title.show_count;
        self.sidebar.set_title_show_count(on);
        self.after_title_setting(window, cx);
    }

    fn toggle_title_suffix(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let on = !self.sidebar.ui_state().title.suffix;
        self.sidebar.set_title_suffix(on);
        self.after_title_setting(window, cx);
    }

    fn after_title_setting(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.save_ui();
        self.refresh_window_title(window, cx);
        cx.notify();
    }

    /// The title the window should show now: the active tab's name and
    /// counts as the App title settings shape them.
    fn wanted_window_title(&self) -> String {
        let title = &self.sidebar.ui_state().title;
        let tab = self.tabs.active_tab();
        let counts = tab.and_then(|tab| tab_session_counts(tab, self.sidebar.sessions()));
        compute_title(
            counts,
            tab.map(|tab| tab.name.as_str()),
            title.show_count,
            title.suffix,
        )
    }

    /// Sets the window title [`TITLE_DEBOUNCE`] after it last changed; a
    /// change inside the wait restarts it.
    pub(crate) fn refresh_window_title(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let wanted = self.wanted_window_title();
        let waiting = self.title_pending.as_ref().unwrap_or(&self.window_title);
        if *waiting == wanted {
            return;
        }
        if wanted == self.window_title {
            self.title_pending = None;
            self.title_timer = None;
            return;
        }
        self.title_pending = Some(wanted);
        self.title_timer = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(TITLE_DEBOUNCE).await;
            // Fails only when the view is gone, and its window with it.
            this.update_in(cx, |this, window, _| this.apply_window_title(window))
                .ok();
        }));
    }

    fn apply_window_title(&mut self, window: &mut Window) {
        self.title_timer = None;
        if let Some(title) = self.title_pending.take() {
            window.set_window_title(&title);
            self.window_title = title;
        }
    }

    /// The Settings modal over a backdrop that takes every click beneath
    /// it; a click on the backdrop itself does nothing.
    pub(crate) fn settings_layer(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.settings_open() {
            return None;
        }
        let close = close_button("settings-close", false).on_click(cx.listener(
            |this, _: &ClickEvent, window, cx| {
                this.close_appearance_editor(window, cx);
            },
        ));
        let body = div()
            .flex()
            .child(self.settings_tab_list(window, cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_none()
                    .w(px(CONTENT_WIDTH + 2.0 * CONTENT_PAD_X))
                    .px(px(CONTENT_PAD_X))
                    .py(px(CONTENT_PAD_Y))
                    .children(self.settings_content(window, cx)),
            );
        let panel = dialog_card("settings-panel", PANEL_WIDTH)
            .track_focus(&self.appearance_focus)
            .child(dialog_title_bar("Settings", close))
            .child(body)
            .child(close_footer("settings-footer-close", cx));
        Some(backdrop("settings", panel))
    }

    /// The tab list on the raised ground; the shown tab is filled and bold,
    /// and ringed while the list has the keyboard.
    fn settings_tab_list(&self, window: &Window, cx: &mut Context<Self>) -> Stateful<Div> {
        let list_focused = self.settings_tabs_focus.is_focused(window);
        let tabs = SettingsTab::ALL.map(|tab| {
            let shown = tab == self.settings_tab;
            let selector = tab.selector();
            let (text, weight) = if shown {
                (TEXT, FontWeight::SEMIBOLD)
            } else {
                (TEXT_2, FontWeight::MEDIUM)
            };
            div()
                .id(selector)
                .debug_selector(|| selector.to_owned())
                .flex()
                .flex_none()
                .items_center()
                .h(px(TAB_ROW_HEIGHT))
                .px(px(12.0))
                .rounded(px(TAB_ROW_RADIUS))
                .border_1()
                .border_color(gpui::rgba(TRANSPARENT))
                .font_weight(weight)
                .text_color(gpui::rgb(text))
                .cursor_pointer()
                .when(shown, |row| row.bg(gpui::rgb(CHIP)))
                .when(!shown, |row| {
                    row.hover(|style| style.bg(gpui::rgb(HOVER)).text_color(gpui::rgb(TEXT)))
                })
                .when(shown && list_focused, |row| {
                    row.child(focus_ring(TAB_ROW_RADIUS))
                })
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.select_settings_tab(tab, window, cx);
                }))
                .child(tab.label())
        });
        div()
            .id("settings-tabs")
            .track_focus(&self.settings_tabs_focus)
            .flex()
            .flex_col()
            .flex_none()
            .gap(px(2.0))
            .w(px(TAB_LIST_WIDTH))
            .px(px(10.0))
            .py(px(12.0))
            .bg(gpui::rgb(RAISED))
            .border_r_1()
            .border_color(gpui::rgb(LINE))
            .children(tabs)
    }

    fn settings_content(&self, window: &Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let focused = self.focused_settings_control(window);
        match self.settings_tab {
            SettingsTab::General => self.general_tab(focused, cx),
            SettingsTab::Appearance => self
                .appearance_body(cx)
                .map(IntoElement::into_any_element)
                .into_iter()
                .collect(),
            SettingsTab::AppTitle => self.app_title_tab(focused, cx),
            SettingsTab::Notifications => self.notifications_tab(focused, cx),
            SettingsTab::SpawnDefaults => self.spawn_defaults_tab(focused, cx),
            SettingsTab::Worktrees => self.worktrees_tab_content(focused, cx),
        }
    }

    /// The Worktrees tab: the path field and Browse, the root in use, and
    /// Save, Reset to default and Manage worktrees….
    fn worktrees_tab_content(
        &self,
        focused: Option<SettingsControl>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(tab) = &self.worktrees_tab else {
            return Vec::new();
        };
        let path = SettingsControl::WorktreesPath;
        let field = field_frame(focused == Some(path))
            .debug_selector(|| path.selector().to_owned())
            .flex_1()
            .min_w(px(0.0))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    this.press_settings_control(path, window, cx);
                }),
            )
            .child(tab.input.clone());
        let browse = settings_button(
            SettingsControl::WorktreesBrowse,
            "Browse…",
            true,
            focused,
            cx,
        );
        let path_row = div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child("Path")
            .child(field)
            .child(browse);
        let active = div()
            .debug_selector(|| "settings-worktrees-active".to_owned())
            .text_color(gpui::rgb(TEXT_2))
            .child(self.worktrees_active_line());
        let (save_label, save_enabled) = self.worktrees_save_button();
        let buttons = div()
            .flex()
            .gap(px(8.0))
            .child(settings_button(
                SettingsControl::WorktreesSave,
                save_label,
                save_enabled,
                focused,
                cx,
            ))
            .child(settings_button(
                SettingsControl::WorktreesReset,
                "Reset to default",
                self.worktrees_reset_enabled(),
                focused,
                cx,
            ))
            .child(settings_button(
                SettingsControl::WorktreesManage,
                "Manage worktrees…",
                true,
                focused,
                cx,
            ));
        let section = section("Worktrees root")
            .child(hint(WORKTREES_HINT))
            .child(path_row)
            .child(active)
            .child(buttons);
        vec![section.into_any_element()]
    }

    /// The Spawn defaults tab: trusted launch, and the two agent options it
    /// locks while it is on.
    fn spawn_defaults_tab(
        &self,
        focused: Option<SettingsControl>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let spawn = self.sidebar.ui_state().spawn;
        let locked = spawn.trusted;
        let trusted = toggle(
            SettingsControl::SpawnTrusted,
            spawn.trusted,
            "Trusted launch by default",
            focused,
            cx,
        );
        let approval = choice_row(
            "Claude approval mode",
            &APPROVAL_CHOICES.map(|mode| {
                (
                    SettingsControl::SpawnApproval(mode),
                    approval_label(mode),
                    mode == spawn.permission_mode,
                )
            }),
            locked.then_some((CLAUDE_LOCKED, "settings-spawn-approval-locked")),
            focused,
            cx,
        );
        let sandbox = choice_row(
            "Codex sandbox mode",
            &CODEX_SANDBOX_CHOICES.map(|sandbox| {
                (
                    SettingsControl::SpawnCodexSandbox(sandbox),
                    codex_sandbox_label(sandbox),
                    sandbox == spawn.codex_sandbox,
                )
            }),
            locked.then_some((CODEX_LOCKED, "settings-spawn-codex-sandbox-locked")),
            focused,
            cx,
        );
        let section = section("Spawn defaults")
            .child(hint(
                "Pre-fill these in the spawn dialog. You can override on every spawn.",
            ))
            .child(trusted)
            .child(hint(
                "When enabled, new Claude, Codex and Cursor sessions bypass approval prompts. \
                 Codex and Cursor also bypass sandboxing.",
            ))
            .child(approval)
            .child(sandbox);
        vec![section.into_any_element()]
    }

    fn notifications_tab(
        &self,
        focused: Option<SettingsControl>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let link = SettingsControl::NotifySettingsLink;
        let selector = link.selector();
        let open = div()
            .id(selector)
            .debug_selector(|| selector.to_owned())
            .px(px(4.0))
            .rounded(px(LINK_RADIUS))
            .border_1()
            .border_color(gpui::rgba(TRANSPARENT))
            .text_color(gpui::rgb(appearance::BUILTIN_ACCENT))
            .cursor_pointer()
            .hover(|style| style.bg(gpui::rgb(HOVER)))
            .on_click(press(link, cx))
            .when(focused == Some(link), |open| {
                open.child(focus_ring(LINK_RADIUS))
            })
            .child("Open Windows notification settings");
        let state = div()
            .debug_selector(|| "settings-notify-state".to_owned())
            .child(self.notifications_state_line());
        let windows = div()
            .flex()
            .items_center()
            .gap(px(10.0))
            .child(state)
            .child(open);
        let settings = &self.sidebar.ui_state().notifications;
        let rows = [
            (
                SettingsControl::NotifyAwaitingInput,
                AttentionReason::AwaitingInput,
                "Awaiting input",
            ),
            (
                SettingsControl::NotifyStopped,
                AttentionReason::Stopped,
                "Stopped",
            ),
            (
                SettingsControl::NotifyError,
                AttentionReason::Error,
                "Errored",
            ),
        ]
        .map(|(control, reason, label)| {
            toggle(control, settings.fires(reason), label, focused, cx)
        });
        let section = section("Notifications")
            .child(windows)
            .child(hint(
                "Fire an OS notification when a session transitions to:",
            ))
            .children(rows);
        vec![section.into_any_element()]
    }

    fn general_tab(
        &self,
        focused: Option<SettingsControl>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let (label, enabled) = self.keep_awake_button();
        let keep_awake = SettingsControl::KeepAwake;
        let button = dialog_button(
            keep_awake.selector(),
            label.to_owned(),
            false,
            focused == Some(keep_awake),
            enabled,
        );
        let button = if enabled {
            button.on_click(press(keep_awake, cx))
        } else {
            button
        };
        let status = hint(self.keep_awake_status())
            .debug_selector(|| "settings-keep-awake-status".to_owned());
        let power = section("Power").child(
            div()
                .flex()
                .items_center()
                .gap(px(10.0))
                .child(button)
                .child(status),
        );
        let view = self.sidebar.sidebar_view();
        let choices = SIDEBAR_VIEW_CHOICES.map(|choice| {
            let text = match choice {
                SidebarView::Repos => "Repos",
                SidebarView::Tabs => "Tabs",
            };
            (SettingsControl::SidebarView(choice), text, choice == view)
        });
        let density = self.sidebar.leaf_density();
        let densities = LEAF_DENSITY_CHOICES.map(|choice| {
            let text = match choice {
                LeafDensity::Comfortable => "Comfortable",
                LeafDensity::Compact => "Compact",
            };
            (
                SettingsControl::LeafDensity(choice),
                text,
                choice == density,
            )
        });
        let sidebar = section("Sidebar")
            .child(choice_row("Default view", &choices, None, focused, cx))
            .child(choice_row("Leaf density", &densities, None, focused, cx));
        let copy = toggle(
            SettingsControl::CopyOnSelect,
            self.sidebar.ui_state().general.copy_on_select,
            "Copy selection to clipboard automatically",
            focused,
            cx,
        );
        let terminal = section("Terminal behavior").child(copy).child(hint(
            "Off: select to highlight, copy explicitly with Ctrl+C \
             (when there's a selection) or Ctrl+Shift+C.",
        ));
        vec![
            power.into_any_element(),
            sidebar.into_any_element(),
            terminal.into_any_element(),
        ]
    }

    fn app_title_tab(
        &self,
        focused: Option<SettingsControl>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let title = &self.sidebar.ui_state().title;
        let count = toggle(
            SettingsControl::TitleCount,
            title.show_count,
            "Show busy/total terminal count",
            focused,
            cx,
        );
        let suffix = toggle(
            SettingsControl::TitleSuffix,
            title.suffix,
            "Append “ — rustling-tulip” suffix",
            focused,
            cx,
        );
        let preview = div().flex().gap(px(10.0)).child("Preview").child(
            div()
                .debug_selector(|| "settings-title-preview".to_owned())
                .px(px(8.0))
                .py(px(2.0))
                .rounded(px(CHIP_RADIUS))
                .bg(gpui::rgb(CHIP))
                .child(self.title_preview()),
        );
        let section = section("App title")
            .child(hint(
                "Controls what the OS window title bar shows for the main window.",
            ))
            .child(count)
            .child(hint(
                "Renders (M/N) before the tab name where M is the number of non-idle \
                 terminals in the active tab and N is the total terminal count. Hidden \
                 when the tab has no terminals.",
            ))
            .child(suffix)
            .child(hint(
                "Keeps OS taskbars grouping rustling-tulip windows together. Turn off \
                 for a tighter title.",
            ))
            .child(preview);
        vec![section.into_any_element()]
    }
}

/// A titled group of rows.
fn section(title: &'static str) -> Div {
    div().flex().flex_col().gap(px(10.0)).pb(px(18.0)).child(
        div()
            .text_size(px(SECTION_HEAD_SIZE))
            .font_weight(FontWeight::BOLD)
            .child(title),
    )
}

/// A quiet line under or beside a control.
fn hint(text: &'static str) -> Div {
    div()
        .text_size(px(HINT_SIZE))
        .text_color(gpui::rgb(SUBTLE))
        .child(text)
}

/// A click handler that presses `control`.
fn press(
    control: SettingsControl,
    cx: &mut Context<RootView>,
) -> impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static {
    cx.listener(move |this, _: &ClickEvent, window, cx| {
        this.press_settings_control(control, window, cx);
    })
}

/// `control`'s button, outlined when it has the keyboard; a disabled one is
/// dimmed and inert.
fn settings_button(
    control: SettingsControl,
    label: &'static str,
    enabled: bool,
    focused: Option<SettingsControl>,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    let button = dialog_button(
        control.selector(),
        label.to_owned(),
        false,
        focused == Some(control),
        enabled,
    );
    if enabled {
        button.on_click(press(control, cx))
    } else {
        button
    }
}

/// `control`'s checkbox row, ticked when `checked` and ringed when it has
/// the keyboard.
fn toggle(
    control: SettingsControl,
    checked: bool,
    label: &'static str,
    focused: Option<SettingsControl>,
    cx: &mut Context<RootView>,
) -> Div {
    let row = checkbox_row(control.selector(), checked, focused == Some(control), label)
        .on_click(press(control, cx));
    div().flex().child(row)
}

/// A labelled row of segmented choices, laid out as the spawn dialog's;
/// with a locked note the choices are dimmed, inert and out of the ring,
/// and the note and its selector say why.
fn choice_row(
    label: &'static str,
    choices: &[(SettingsControl, &'static str, bool)],
    locked_note: Option<(&'static str, &'static str)>,
    focused: Option<SettingsControl>,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let buttons = choices
        .iter()
        .map(|(control, text, selected)| {
            let look = Look {
                selected: *selected,
                focused: focused == Some(*control),
                enabled: locked_note.is_none(),
            };
            choice_button(
                control.selector().to_owned(),
                *text,
                look,
                press(*control, cx),
            )
            .into_any_element()
        })
        .collect();
    let note =
        locked_note.map(|(text, selector)| hint(text).debug_selector(move || selector.to_owned()));
    field(label)
        .child(div().flex().child(segmented(buttons)))
        .children(note)
        .into_any_element()
}
