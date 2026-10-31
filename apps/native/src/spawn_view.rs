//! The spawn dialog on screen: opening and closing it, its keys, clicks and
//! text fields, and its rendering. The state and every rule live in
//! [`crate::spawn_form`]; spawning goes through [`RootView::spawn`].

use std::mem;
use std::ops::Range;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use gpui::{
    AnyElement, Bounds, ClickEvent, Context, Div, ElementId, Entity, Focusable as _, FontWeight,
    Keystroke, MouseButton, MouseDownEvent, MouseMoveEvent, Pixels, ScrollHandle, SharedString,
    Stateful, Subscription, Task, Window, canvas, deferred, div, point, prelude::*, px, relative,
    svg,
};
use protocol::{DaemonMessage, OptionValue, SpawnPlacement};

use crate::agent_options::{FieldEditor, FieldView};
use crate::agent_options_view;
use crate::assets::{SIDEBAR_CHEVRON_DOWN_ICON, SIDEBAR_CHEVRON_RIGHT_ICON};
use crate::buttons::{ButtonKind, ButtonSize, Layer, button, button_look, field_frame, focus_ring};
use crate::combobox::{ComboRow, list_placement};
use crate::notice_view::{MODAL_RADIUS, modal_panel};
use crate::notices;
use crate::overlay_owner::Overlay;
use crate::remote_mode::spawn_no_repos_note;
use crate::session_menu::{backdrop, dialog_button, dialog_look};
use crate::spawn_form::{
    Control, EnvRow, EnvRowNote, FormInputs, HostInputs, ListField, Lock, OpenChoice, Outcome,
    Prefill, ShareButton, SpawnForm, TabChoices, Target, WorktreeMode,
};
use crate::spawn_preview::{
    CollisionNotice, RECREATE_LABEL, REUSE_NOTE, ReuseChoice, Tone, collision_notice, member_row,
    staleness_notice,
};
use crate::spawns::PaneAim;
use crate::target::HostKey;
use crate::text_input::{NavKey, TextChanged, TextInput, TextInputEvent};
use crate::ui_theme::{UiTheme, theme};
use crate::{RootView, tagged_text, tooltip};

/// The spawn dialog's width, as on the Spawn board.
const DIALOG_WIDTH: f32 = 640.0;
/// The padding at the sides of a dialog's header, body and footer.
const DIALOG_PAD_X: f32 = 18.0;
/// The size of a dialog's title.
const TITLE_SIZE: f32 = 16.0;
/// The side of a dialog's close button.
const CLOSE_SIZE: f32 = 28.0;
/// The corner radius of a dialog's close button.
const CLOSE_RADIUS: f32 = 6.0;
/// The size of a field's label.
const LABEL_SIZE: f32 = 12.0;
/// The size of a checkbox's label, and of a note under it.
const CHECK_LABEL_SIZE: f32 = 13.0;
const NOTE_SIZE: f32 = 12.0;
/// The side of a checkbox's box and of a radio's ring.
const CHECK_SIZE: f32 = 16.0;
/// The corner radius of a checkbox's box.
const CHECK_RADIUS: f32 = 4.0;
/// The side of a chosen radio's dot.
const RADIO_DOT: f32 = 7.0;
/// The least outer height of a segment of a segmented control.
const SEGMENT_HEIGHT: f32 = 32.0;
/// The corner radius of a segment, and of the control round the segments.
const SEGMENT_RADIUS: f32 = 6.0;
const SEGMENTED_RADIUS: f32 = 8.0;
/// The size of a segment's label.
const SEGMENT_TEXT_SIZE: f32 = 12.5;
/// The dialog's ordinary buttons: outlined, regular.
const OUTLINED: (ButtonKind, ButtonSize) = (ButtonKind::Outlined, ButtonSize::Regular);
/// The opacity of a disabled control, as `buttons.rs` dims a button.
pub(crate) const DISABLED_OPACITY: f32 = 0.45;
/// The corner radius of a notice box in the dialog.
const NOTICE_RADIUS: f32 = 8.0;
/// The corner radius of a branch list, as a popover's.
const LIST_RADIUS: f32 = 10.0;
/// The dialog's greatest height, as a share of the window's; the body
/// scrolls past it.
const PANEL_MAX_HEIGHT: f32 = 0.9;
const DIALOG_TITLE: &str = "Spawn session";
const SHARE_TITLE: &str = "Share this worktree?";
const SHARE_BODY: &str = "A session is already running in the worktree you picked. Both agents will see each other's uncommitted edits, and concurrent writes to the same file will overwrite one another.";
const RANDOM_TIP: &str = "Generate a random worktree branch name";
const SUGGESTING: &str = "Picking a name no existing branch uses…";
const FETCH_FAILED: &str = "Couldn't reach the remote — comparing against the last fetched refs.";
const CURRENT_TAB_DISABLED: &str = "The current tab cannot host terminal panes";
/// The trusted-launch warning's text under its title.
const TRUSTED_WARNING: &str = "Approval prompts are bypassed for this session.";
const NO_ENV: &str = "No extra env vars.";
const ENV_VALUE_PLACEHOLDER: &str = "value";
const REMOVE_ENV_TIP: &str = "Remove env var";
/// A saved secret row's read-only value, and its button.
const SAVED_SECRET: &str = "Saved secret";
const REPLACE_SECRET: &str = "Replace";
/// A secret row's reveal button, while masked and while revealed.
const SHOW_SECRET: &str = "Show";
const HIDE_SECRET: &str = "Hide";
const SECRET_TOGGLE: &str = "Secret";
/// The tallest a branch list grows before it scrolls.
const LIST_MAX_HEIGHT: f32 = 200.0;
const CURRENT_TAG: &str = "current";
const PREVIEW_HEADERS: [&str; 4] = ["Repo", "Branch", "Action", "Path"];

/// Where the spawn dialog was opened from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SpawnEntry {
    /// "+ Session", Ctrl+Shift+N or the main area's "Spawn a session".
    Toolbar,
    /// A pane's "New session…": the pane takes the session when the Open-in
    /// choice is the current tab. `preselect` names the session whose repo
    /// or workspace the dialog starts on.
    Pane {
        aim: PaneAim,
        preselect: Option<String>,
    },
    /// Held on one target or tab, pinned to an existing worktree when the
    /// lock names one, and starting on `preselect`'s repo or workspace when
    /// given: "Launch session here" in Manage worktrees, or a container
    /// menu's spawn row.
    Locked {
        lock: Lock,
        preselect: Option<String>,
    },
    /// A Shift-duplicate: held on the source's repo or workspace when it
    /// has one, and filled from `prefill` over the Spawn defaults.
    Duplicate {
        lock: Option<Lock>,
        prefill: Box<Prefill>,
    },
}

/// The open spawn dialog: its form and its text fields.
pub(crate) struct SpawnDialog {
    form: SpawnForm,
    /// The pane it was opened from, if any.
    aim: Option<PaneAim>,
    branch_input: Entity<TextInput>,
    base_input: Entity<TextInput>,
    /// One field per text or model field of the chosen agent, in order.
    option_inputs: Vec<OptionInput>,
    /// One pair of fields per env row of the form, in order.
    env_inputs: Vec<EnvInputs>,
    _subscriptions: Vec<Subscription>,
    /// Wakes the view when the wait for a branch suggestion runs out.
    timer: Option<Task<()>>,
    /// Sends a repo's preview request once its debounce runs out.
    preview_timer: Option<Task<()>>,
    /// Where the body is scrolled to.
    scroll: ScrollHandle,
    /// The focus moved since the body last scrolled the focused control
    /// into view.
    reveal: bool,
    /// Where the dialog card was last laid out; a branch list stays inside.
    panel_bounds: Option<Bounds<Pixels>>,
    /// Where the field whose list shows was last laid out.
    list_anchor: Option<Bounds<Pixels>>,
}

/// The spawn dialog while open: its form, or the state it opens in while no
/// repo is registered.
pub(crate) enum SpawnModal {
    Form(Box<SpawnDialog>),
    /// [`spawn_no_repos_note`]'s wording (the host's short name on a remote
    /// one) with Add repo, Spawn disabled; the first repo to arrive opens the
    /// form from `entry`.
    NoRepos(Box<SpawnEntry>),
}

impl SpawnEntry {
    /// The target the dialog is held on, if any.
    fn lock(&self) -> Option<&Lock> {
        match self {
            Self::Locked { lock, .. } => Some(lock),
            Self::Duplicate { lock, .. } => lock.as_ref(),
            Self::Toolbar | Self::Pane { .. } => None,
        }
    }
}

/// Which of the dialog's laid-out bounds a measurement is.
#[derive(Debug, Clone, Copy)]
enum Measured {
    Panel,
    ListAnchor,
}

impl SpawnDialog {
    /// The text field a control is, if it is one.
    fn input(&self, control: &Control) -> Option<&Entity<TextInput>> {
        match control {
            Control::Branch => Some(&self.branch_input),
            Control::Base => Some(&self.base_input),
            Control::FieldText(id) => self
                .option_inputs
                .iter()
                .find(|input| &input.field == id)
                .map(|input| &input.input),
            Control::EnvKey(index) => self.env_inputs.get(*index).map(|row| &row.key),
            Control::EnvValue(index) => self.env_inputs.get(*index).map(|row| &row.value),
            _ => None,
        }
    }

    fn inputs(&self) -> impl Iterator<Item = &Entity<TextInput>> {
        [&self.branch_input, &self.base_input]
            .into_iter()
            .chain(self.option_inputs.iter().map(|input| &input.input))
            .chain(
                self.env_inputs
                    .iter()
                    .flat_map(|row| [&row.key, &row.value]),
            )
    }
}

/// The key and value fields of one env row, and what watches them.
struct EnvInputs {
    key: Entity<TextInput>,
    value: Entity<TextInput>,
    _subscriptions: Vec<Subscription>,
}

/// The text field of one of the chosen agent's text or model fields, and
/// what watches it. It keeps the text as typed; the form reads it trimmed.
pub(crate) struct OptionInput {
    /// The agent field's id.
    pub(crate) field: String,
    pub(crate) input: Entity<TextInput>,
    _subscriptions: Vec<Subscription>,
}

/// Which text field an event came from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Field {
    Branch,
    Base,
    /// An agent's text or model field, by its id.
    Option(String),
    EnvKey(usize),
    EnvValue(usize),
}

impl Field {
    fn control(&self) -> Control {
        match self {
            Self::Branch => Control::Branch,
            Self::Base => Control::Base,
            Self::Option(id) => Control::FieldText(id.clone()),
            Self::EnvKey(index) => Control::EnvKey(*index),
            Self::EnvValue(index) => Control::EnvValue(*index),
        }
    }
}

/// The text and placeholder a text or model field's input shows; `None`
/// for a field of another kind.
fn text_field(field: &FieldView) -> Option<(String, String)> {
    let placeholder = match &field.editor {
        FieldEditor::Text { placeholder } | FieldEditor::Model { placeholder, .. } => {
            placeholder.clone().unwrap_or_default()
        }
        FieldEditor::Toggle | FieldEditor::Choice { .. } | FieldEditor::Unknown => return None,
    };
    let text = match &field.value {
        OptionValue::Text(text) => text.clone(),
        OptionValue::Bool(_) | OptionValue::Null => String::new(),
    };
    Some((text, placeholder))
}

/// How a button looks.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Look {
    pub(crate) selected: bool,
    pub(crate) focused: bool,
    pub(crate) enabled: bool,
}

impl RootView {
    /// Whether the spawn dialog is open.
    #[must_use]
    pub fn spawn_dialog_open(&self) -> bool {
        self.spawn_dialog.is_some()
    }

    /// Whether the dialog is open in its no-repos state.
    #[must_use]
    pub fn spawn_dialog_no_repos(&self) -> bool {
        matches!(self.spawn_dialog, Some(SpawnModal::NoRepos(_)))
    }

    /// The open dialog's form side, unless it is in its no-repos state.
    fn form_dialog(&self) -> Option<&SpawnDialog> {
        match self.spawn_dialog.as_ref()? {
            SpawnModal::Form(dialog) => Some(dialog),
            SpawnModal::NoRepos(_) => None,
        }
    }

    /// The selector of the dialog's focused control, or of the focused
    /// button of its "Share this worktree?" confirm.
    #[must_use]
    pub fn spawn_dialog_focus(&self) -> Option<String> {
        let form = &self.form_dialog()?.form;
        Some(match form.share_confirm() {
            Some(button) => button.selector().to_owned(),
            None => form.focused().selector(),
        })
    }

    /// Whether the "Share this worktree?" confirm is open.
    #[must_use]
    pub fn spawn_share_confirm_open(&self) -> bool {
        self.form_dialog()
            .is_some_and(|dialog| dialog.form.share_confirm().is_some())
    }

    /// The selectors of the dialog's chosen options and ticked boxes.
    #[must_use]
    pub fn spawn_dialog_selected(&self) -> Vec<String> {
        let Some(SpawnModal::Form(dialog)) = &self.spawn_dialog else {
            return Vec::new();
        };
        let form = &dialog.form;
        let mut chosen = vec![
            Control::Target(form.target().clone()),
            Control::Runtime(form.runtime()),
            Control::OpenIn(form.open_in().clone()),
        ];
        chosen.extend(form.chosen_options());
        chosen.extend(form.trusted().then_some(Control::Trusted));
        chosen.extend(form.use_worktree().then_some(Control::UseWorktree));
        chosen.extend(form.use_worktree().then_some(Control::Mode(form.mode())));
        chosen.extend(
            form.existing_selected()
                .map(|key| Control::Existing(key.to_owned())),
        );
        chosen.extend(
            (0..form.env_rows().len())
                .filter(|&index| form.env_secret_shown(index) && form.env_rows()[index].secret)
                .map(Control::EnvSecret),
        );
        chosen.extend(
            form.collision()
                .map(|_| Control::Reuse(form.reuse_choice())),
        );
        chosen.iter().map(Control::selector).collect()
    }

    /// The warning that the base trails its remote counterpart, while a
    /// repo's preview shows one.
    #[must_use]
    pub fn spawn_base_stale(&self) -> Option<String> {
        let form = &self.form_dialog()?.form;
        if form.is_workspace() {
            return None;
        }
        form.preview_members().first().and_then(staleness_notice)
    }

    /// The collision notice while it shows: its headline, then each choice
    /// with its note.
    #[must_use]
    pub fn spawn_collision(&self) -> Option<Vec<String>> {
        let form = &self.form_dialog()?.form;
        let notice = collision_notice(form.collision()?)?;
        Some(vec![
            notice.headline,
            format!("{} {REUSE_NOTE}", notice.reuse_label),
            format!("{RECREATE_LABEL} {}", notice.recreate_note),
        ])
    }

    /// Whether the workspace's Preview button can be pressed.
    #[must_use]
    pub fn spawn_preview_enabled(&self) -> bool {
        self.form_dialog()
            .is_some_and(|dialog| dialog.form.preview_enabled())
    }

    /// The workspace preview table's rows: the repo, the branch, the action
    /// badges joined by " · ", and the path.
    #[must_use]
    pub fn spawn_preview_rows(&self) -> Vec<Vec<String>> {
        let Some(SpawnModal::Form(dialog)) = &self.spawn_dialog else {
            return Vec::new();
        };
        let form = &dialog.form;
        if !form.is_workspace() {
            return Vec::new();
        }
        let (branch, default_base) = (form.branch().trim(), form.default_base());
        form.preview_members()
            .iter()
            .map(|member| {
                let row = member_row(member, branch, &default_base);
                let badges: Vec<String> = row.badges.into_iter().map(|(text, _)| text).collect();
                vec![row.repo, row.branch, badges.join(" · "), row.path]
            })
            .collect()
    }

    /// The text of the chosen agent's text or model field `field`, as
    /// typed, while the dialog shows it.
    #[must_use]
    pub fn spawn_dialog_option_text(&self, field: &str, cx: &gpui::App) -> Option<String> {
        let dialog = self.form_dialog()?;
        dialog
            .option_inputs
            .iter()
            .find(|input| input.field == field)
            .map(|input| input.input.read(cx).text().to_owned())
    }

    /// The trusted-launch row's detail, the chosen agent's trust note,
    /// while the dialog is open.
    #[must_use]
    pub fn spawn_dialog_trust_note(&self) -> Option<String> {
        self.form_dialog()?.form.trusted_detail().map(str::to_owned)
    }

    /// The key and value fields of every env row, while the dialog is open.
    #[must_use]
    pub fn spawn_dialog_env(&self, cx: &gpui::App) -> Option<Vec<(String, String)>> {
        let dialog = self.form_dialog()?;
        let text = |input: &Entity<TextInput>| input.read(cx).text().to_owned();
        Some(
            dialog
                .env_inputs
                .iter()
                .map(|row| (text(&row.key), text(&row.value)))
                .collect(),
        )
    }

    /// The env section's note after a move to another host dropped secret
    /// rows, while the dialog is open and shows it.
    #[must_use]
    pub fn spawn_dialog_env_section_note(&self) -> Option<&'static str> {
        self.form_dialog()?.form.env_section_note()
    }

    /// The note env row `index` shows about where its value is kept, while
    /// the dialog is open and the row has one.
    #[must_use]
    pub fn spawn_dialog_env_note(&self, index: usize) -> Option<&'static str> {
        self.form_dialog()?
            .form
            .env_row_note(index)
            .map(EnvRowNote::text)
    }

    /// Whether env row `index`'s value field is masked, while the dialog is
    /// open and the row exists.
    #[must_use]
    pub fn spawn_dialog_env_masked(&self, index: usize, cx: &gpui::App) -> Option<bool> {
        let dialog = self.form_dialog()?;
        let inputs = dialog.env_inputs.get(index)?;
        Some(inputs.value.read(cx).is_masked())
    }

    /// The text of the branch field, while the dialog is open.
    #[must_use]
    pub fn spawn_dialog_branch(&self, cx: &gpui::App) -> Option<String> {
        let dialog = self.form_dialog()?;
        Some(dialog.branch_input.read(cx).text().to_owned())
    }

    /// The text of the base branch field, while the dialog is open.
    #[must_use]
    pub fn spawn_dialog_base(&self, cx: &gpui::App) -> Option<String> {
        let dialog = self.form_dialog()?;
        Some(dialog.base_input.read(cx).text().to_owned())
    }

    /// The rows the branch field's list shows, or `None` while it is closed.
    #[must_use]
    pub fn spawn_branch_rows(&self) -> Option<Vec<String>> {
        self.spawn_list_rows(ListField::Branch)
    }

    /// The rows the base branch field's list shows, or `None` while it is
    /// closed.
    #[must_use]
    pub fn spawn_base_rows(&self) -> Option<Vec<String>> {
        self.spawn_list_rows(ListField::Base)
    }

    /// The rows env row `index`'s variable-name list shows, each a name and
    /// its scopes (`user · system`), or `None` while it is closed.
    #[must_use]
    pub fn spawn_env_name_rows(&self, index: usize) -> Option<Vec<(String, String)>> {
        let form = &self.form_dialog()?.form;
        let field = ListField::EnvValue(index);
        form.list_shown(field).then(|| {
            form.list_rows(field)
                .iter()
                .map(|row| (row.label(), row.detail().unwrap_or_default().to_owned()))
                .collect()
        })
    }

    fn spawn_list_rows(&self, field: ListField) -> Option<Vec<String>> {
        let form = &self.form_dialog()?.form;
        form.list_shown(field)
            .then(|| form.list_rows(field).iter().map(ComboRow::label).collect())
    }

    /// Why a spawn dialog cannot open for `lock` right now, if it cannot:
    /// another overlay is up, or the locked target left the registry. With
    /// no repo and no lock it opens in its no-repos state.
    ///
    /// `handoff` says the caller is the worktrees manager's launch, which
    /// closes the manager and Settings as part of the hand-off, so neither
    /// of those two counts against it.
    pub(crate) fn spawn_dialog_blocker(
        &self,
        lock: Option<&Lock>,
        handoff: bool,
    ) -> Option<&'static str> {
        if self.spawn_dialog.is_some()
            || self.exit.is_some()
            || self.shell_dialog.is_some()
            || self.fetch_dialog.is_some()
            || (self.appearance_editor.is_some() && !handoff)
            || (self.worktrees_manager.is_some() && !handoff)
            || self.recover.is_some()
            || self.delete_dialog.is_some()
            || self.open_in_main(Overlay::SessionMenu)
            || self.container_menu.is_some()
            || self.open_in_main(Overlay::ShellMenu)
            || self.sc_picker_open
            || self.changes.file_menu.is_some()
            || self.changes.discard.is_some()
            || self.stash.drop.is_some()
            || self.notices.has_modal()
            || self.conn().overlay().is_some()
        {
            return Some("another dialog is open");
        }
        let lock = lock?;
        let host = lock
            .target()
            .map_or(HostKey::Local, |target| target.host().clone());
        lock.unregistered(
            self.sidebar.repos_of(&host),
            self.sidebar.workspaces_of(&host),
        )
    }

    /// `host` as the spawn dialog lists it: its registry, its agents and
    /// their Settings values on that host.
    fn spawn_host_inputs(&self, host: &HostKey) -> HostInputs {
        HostInputs {
            host: host.clone(),
            label: self.host_name(host),
            repos: self.sidebar.repos_of(host).to_vec(),
            workspaces: self.sidebar.workspaces_of(host).to_vec(),
            agents: self.sidebar.agents(host).to_vec(),
            agent_settings: self
                .hosts
                .get(host)
                .map(|conn| conn.mirrors.agent_settings.clone())
                .unwrap_or_default(),
            offline: self.is_remote_offline(host),
        }
    }

    /// Every host the window holds as the spawn dialog lists them, This PC
    /// first; an offline host is listed with its targets disabled.
    fn spawn_hosts(&self) -> Vec<HostInputs> {
        self.held_hosts()
            .iter()
            .map(|host| self.spawn_host_inputs(host))
            .collect()
    }

    /// The session the dialog opens next to, with its host: `preselect` on
    /// the host that lists it, else the focused pane's session on that
    /// pane's own host.
    fn spawn_focus(
        &self,
        preselect: Option<String>,
    ) -> Option<(HostKey, protocol::SessionSnapshot)> {
        let (host, id) = if let Some(id) = preselect {
            (self.host_of_session(&id), id)
        } else {
            let tab = self.tabs.active_tab()?;
            let pane = self.tabs.focused_pane(&tab.id)?;
            (self.tabs.pane_host(&tab.id, &pane), self.focused_session()?)
        };
        let session = self.sidebar.session_on(&host, &id)?.clone();
        Some((host, session))
    }

    /// Opens the spawn dialog held on `lock`, prefilled from `prefill`, for
    /// an action the user reviews in it before anything spawns. When the
    /// dialog cannot open — another dialog is up, or the locked target left
    /// the registry — a notice titled `refusal_title` says why. Returns
    /// whether a dialog opened.
    pub(crate) fn open_prefilled_dialog(
        &mut self,
        lock: Option<Lock>,
        prefill: Prefill,
        refusal_title: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if let Some(reason) = self.spawn_dialog_blocker(lock.as_ref(), false) {
            tracing::warn!("{refusal_title}: no spawn dialog: {reason}");
            let notice = notices::spawn_refusal(refusal_title, reason);
            self.on_action_failed(notice, None, window, cx);
            return false;
        }
        self.open_spawn_dialog(
            SpawnEntry::Duplicate {
                lock,
                prefill: Box::new(prefill),
            },
            window,
            cx,
        )
    }

    /// Opens the dialog from `entry`, unless the connection is down or
    /// another dialog or menu is open. It starts on the preselected
    /// session's repo or workspace, else the focused one's; with no repo it
    /// opens in its no-repos state. Returns whether a dialog opened.
    pub(crate) fn open_spawn_dialog(
        &mut self,
        entry: SpawnEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.is_main_window(window) {
            self.open_in_main_window(cx, move |root, window, cx| {
                root.open_spawn_dialog(entry, window, cx);
            });
            return true;
        }
        if entry.lock().and_then(Lock::target).is_none() && !self.has_repos() {
            self.prefills.clear();
            if self.spawn_dialog_blocker(None, false).is_some() {
                return false;
            }
            self.open_no_repos_dialog(entry, window, cx);
            return true;
        }
        let (aim, preselect, lock, prefill) = match entry {
            SpawnEntry::Toolbar => (None, None, None, None),
            SpawnEntry::Pane { aim, preselect } => (Some(aim), preselect, None, None),
            SpawnEntry::Locked { lock, preselect } => (None, preselect, Some(lock), None),
            SpawnEntry::Duplicate { lock, prefill } => (None, None, lock, Some(prefill)),
        };
        self.prefills.clear();
        if self.spawn_dialog_blocker(lock.as_ref(), false).is_some() {
            return false;
        }
        let focused = self.spawn_focus(preselect);
        let inputs = FormInputs {
            hosts: self.spawn_hosts(),
            focused: focused
                .as_ref()
                .map(|(host, session)| (host.clone(), session)),
            tabs: TabChoices::from_tabs(self.tabs.tabs(), self.tabs.active_id()),
            spawn_defaults: self.sidebar.ui_state().spawn,
            lock,
        };
        let now = (self.now)();
        let Some((mut form, mut messages)) = SpawnForm::open(inputs, &mut self.branch_cache, now)
        else {
            return false;
        };
        if let Some(prefill) = &prefill {
            messages.extend(form.apply_prefill(prefill, &self.branch_cache, now));
        }
        self.renaming = None;
        self.close_flyout();
        self.close_tab_menu(window, cx);
        self.close_more_menu(window, cx);
        let branch_input = cx.new(|cx| TextInput::new(form.branch().to_owned(), "", cx));
        let base_input = cx.new(|cx| TextInput::new(form.base().to_owned(), "", cx));
        let mut subscriptions = Self::watch_field(&branch_input, Field::Branch, window, cx);
        subscriptions.extend(Self::watch_field(&base_input, Field::Base, window, cx));
        self.spawn_dialog = Some(SpawnModal::Form(Box::new(SpawnDialog {
            form,
            aim,
            branch_input,
            base_input,
            option_inputs: Vec::new(),
            env_inputs: Vec::new(),
            _subscriptions: subscriptions,
            timer: None,
            preview_timer: None,
            scroll: ScrollHandle::new(),
            reveal: true,
            panel_bounds: None,
            list_anchor: None,
        })));
        self.sync_env_inputs(window, cx);
        self.sync_option_inputs(window, cx);
        self.send_all(messages);
        self.after_spawn_change(cx);
        self.apply_spawn_focus(window, cx);
        true
    }

    /// The dialog in its no-repos state, waiting to open the form from
    /// `entry` once a repo is registered.
    fn open_no_repos_dialog(
        &mut self,
        entry: SpawnEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.renaming = None;
        self.close_flyout();
        self.close_tab_menu(window, cx);
        self.close_more_menu(window, cx);
        self.spawn_dialog = Some(SpawnModal::NoRepos(Box::new(entry)));
        self.apply_spawn_focus(window, cx);
    }

    /// A repo arrived while the dialog waited in its no-repos state: the
    /// form opens from the entry it waited with, on that repo.
    fn leave_no_repos(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.spawn_dialog.take() {
            Some(SpawnModal::NoRepos(entry)) => {
                self.open_spawn_dialog(*entry, window, cx);
                if self.spawn_dialog.is_none() {
                    self.focus_active_pane(window, cx);
                    cx.notify();
                }
            }
            other => self.spawn_dialog = other,
        }
    }

    /// The fields of env row `index`, holding `row`.
    fn env_inputs(
        index: usize,
        row: &EnvRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> EnvInputs {
        let key = cx.new(|cx| TextInput::new(row.key.clone(), "KEY", cx));
        let value = cx.new(|cx| TextInput::new(row.value.clone(), ENV_VALUE_PLACEHOLDER, cx));
        let mut subscriptions = Self::watch_field(&key, Field::EnvKey(index), window, cx);
        subscriptions.extend(Self::watch_field(
            &value,
            Field::EnvValue(index),
            window,
            cx,
        ));
        EnvInputs {
            key,
            value,
            _subscriptions: subscriptions,
        }
    }

    /// A row added or removed: the env fields are made afresh from the
    /// form's rows, so each field's index matches its row again.
    fn sync_env_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(SpawnModal::Form(dialog)) = &self.spawn_dialog else {
            return;
        };
        let rows = dialog.form.env_rows().to_vec();
        if rows.len() == dialog.env_inputs.len() {
            return;
        }
        let inputs = rows
            .iter()
            .enumerate()
            .map(|(index, row)| Self::env_inputs(index, row, window, cx))
            .collect();
        if let Some(SpawnModal::Form(dialog)) = &mut self.spawn_dialog {
            dialog.env_inputs = inputs;
        }
    }

    /// The chosen agent's text and model fields changed (another runtime,
    /// a field a rule hides or shows): their inputs are made afresh, each
    /// holding the form's value.
    fn sync_option_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(SpawnModal::Form(dialog)) = &self.spawn_dialog else {
            return;
        };
        let fields: Vec<(String, String, String)> = dialog
            .form
            .option_fields()
            .iter()
            .filter_map(|field| {
                text_field(field).map(|(text, hint)| (field.id.clone(), text, hint))
            })
            .collect();
        let same = fields.len() == dialog.option_inputs.len()
            && fields
                .iter()
                .zip(&dialog.option_inputs)
                .all(|((id, _, _), input)| *id == input.field);
        if same {
            return;
        }
        let inputs = fields
            .into_iter()
            .map(|(id, text, placeholder)| {
                let input = cx.new(|cx| TextInput::new(text, placeholder, cx));
                let subscriptions =
                    Self::watch_field(&input, Field::Option(id.clone()), window, cx);
                OptionInput {
                    field: id,
                    input,
                    _subscriptions: subscriptions,
                }
            })
            .collect();
        if let Some(SpawnModal::Form(dialog)) = &mut self.spawn_dialog {
            dialog.option_inputs = inputs;
        }
    }

    /// Enter submits, Esc closes, an edit
    /// reaches the form and a click in the field moves the form's focus
    /// there. In a field with a branch list, Enter and Esc go to the list
    /// while it shows, Up and Down move through it, and leaving the field
    /// closes it.
    fn watch_field(
        input: &Entity<TextInput>,
        field: Field,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Subscription> {
        let list = ListField::of(&field.control());
        let keys = cx.subscribe_in(
            input,
            window,
            move |this, _, event: &TextInputEvent, window, cx| match event {
                TextInputEvent::Submit => {
                    if !list.is_some_and(|list| this.enter_spawn_list(list, cx)) {
                        this.submit_spawn_dialog(window, cx);
                    }
                }
                TextInputEvent::Cancel => {
                    if !list.is_some_and(|list| this.escape_spawn_list(list, cx)) {
                        this.close_spawn_dialog(window, cx);
                    }
                }
            },
        );
        let edited = field.clone();
        let edits = cx.subscribe_in(input, window, move |this, input, _: &TextChanged, _, cx| {
            let text = input.read(cx).text().to_owned();
            if let Some(SpawnModal::Form(dialog)) = &mut this.spawn_dialog {
                let form = &mut dialog.form;
                match &edited {
                    Field::Branch => form.edit_branch(&text, &mut this.branch_cache),
                    Field::Base => form.edit_base(&text),
                    Field::Option(id) => form.edit_option_text(id, &text),
                    Field::EnvKey(index) => form.edit_env_key(*index, &text),
                    Field::EnvValue(index) => form.edit_env_value(*index, &text),
                }
                if let Some(list) = list {
                    form.list_edited(list);
                }
            }
            this.after_spawn_change(cx);
        });
        let handle = input.read(cx).focus_handle(cx);
        let focus = cx.on_focus(&handle, window, move |this, _, cx| {
            if let Some(SpawnModal::Form(dialog)) = &mut this.spawn_dialog {
                dialog.form.set_focus(field.control());
                dialog.reveal = true;
            }
            cx.notify();
        });
        let mut subscriptions = vec![keys, edits, focus];
        if let Some(list) = list {
            subscriptions.push(cx.subscribe_in(
                input,
                window,
                move |this, _, key: &NavKey, _, cx| {
                    if let Some(SpawnModal::Form(dialog)) = &mut this.spawn_dialog {
                        dialog.form.list_nav(list, *key == NavKey::Down);
                    }
                    cx.notify();
                },
            ));
            subscriptions.push(cx.on_blur(&handle, window, move |this, _, cx| {
                this.close_spawn_list(list, cx);
            }));
        }
        subscriptions
    }

    /// Enter in `list`'s field: commits the highlighted row while the list
    /// shows; returns whether it did.
    fn enter_spawn_list(&mut self, list: ListField, cx: &mut Context<Self>) -> bool {
        let Some(SpawnModal::Form(dialog)) = &mut self.spawn_dialog else {
            return false;
        };
        let committed = dialog.form.enter_list(list, &mut self.branch_cache);
        if committed {
            self.after_spawn_change(cx);
        }
        committed
    }

    /// Esc in `list`'s field: closes the list while it shows; returns
    /// whether it did.
    fn escape_spawn_list(&mut self, list: ListField, cx: &mut Context<Self>) -> bool {
        let closed = match &mut self.spawn_dialog {
            Some(SpawnModal::Form(dialog)) => dialog.form.escape_list(list),
            _ => false,
        };
        if closed {
            cx.notify();
        }
        closed
    }

    fn open_spawn_list(&mut self, list: ListField, cx: &mut Context<Self>) {
        if let Some(SpawnModal::Form(dialog)) = &mut self.spawn_dialog {
            dialog.form.open_list(list);
            cx.notify();
        }
    }

    fn close_spawn_list(&mut self, list: ListField, cx: &mut Context<Self>) {
        if let Some(SpawnModal::Form(dialog)) = &mut self.spawn_dialog {
            dialog.form.close_list(list);
            cx.notify();
        }
    }

    fn hover_spawn_list(&mut self, list: ListField, index: usize, cx: &mut Context<Self>) {
        if let Some(SpawnModal::Form(dialog)) = &mut self.spawn_dialog
            && dialog.form.hover_list(list, index)
        {
            cx.notify();
        }
    }

    fn pick_spawn_list(&mut self, list: ListField, index: usize, cx: &mut Context<Self>) {
        let Some(SpawnModal::Form(dialog)) = &mut self.spawn_dialog else {
            return;
        };
        dialog.form.pick_list(list, index, &mut self.branch_cache);
        self.after_spawn_change(cx);
    }

    /// Keeps where the card or the listed field was laid out; a change
    /// draws again so the list is placed from it.
    fn measure_spawn(
        &mut self,
        what: Measured,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(SpawnModal::Form(dialog)) = &mut self.spawn_dialog else {
            return;
        };
        let slot = match what {
            Measured::Panel => &mut dialog.panel_bounds,
            Measured::ListAnchor => &mut dialog.list_anchor,
        };
        if *slot != Some(bounds) {
            *slot = Some(bounds);
            cx.defer_in(window, |_, window, _| window.refresh());
        }
    }

    /// Closes the dialog and hands the keyboard back to the active pane.
    pub(crate) fn close_spawn_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.spawn_dialog.take().is_some() {
            self.focus_active_pane(window, cx);
            cx.notify();
        }
    }

    /// Sends the dialog's requests to the host its target is on; the env
    /// name list is This PC's.
    fn send_all(&self, messages: Vec<protocol::ClientMessage>) {
        let host = self.spawn_form_host();
        for msg in messages {
            let to = match msg {
                protocol::ClientMessage::ListEnvNames { .. } => HostKey::Local,
                _ => host.clone(),
            };
            self.send_to(&to, msg);
        }
    }

    /// The host the open dialog's target is on; This PC with no form open.
    fn spawn_form_host(&self) -> HostKey {
        match &self.spawn_dialog {
            Some(SpawnModal::Form(dialog)) => dialog.form.host().clone(),
            _ => HostKey::Local,
        }
    }

    fn submit_spawn_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(SpawnModal::Form(dialog)) = &mut self.spawn_dialog else {
            return;
        };
        let outcome = dialog.form.submit(&mut self.branch_cache);
        self.on_spawn_outcome(outcome, window, cx);
    }

    fn press_spawn_control(
        &mut self,
        control: &Control,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let now = (self.now)();
        let Some(SpawnModal::Form(dialog)) = &mut self.spawn_dialog else {
            return;
        };
        let outcome = dialog.form.press(control, &mut self.branch_cache, now);
        self.on_spawn_outcome(outcome, window, cx);
    }

    fn answer_share(&mut self, button: ShareButton, window: &mut Window, cx: &mut Context<Self>) {
        let Some(SpawnModal::Form(dialog)) = &mut self.spawn_dialog else {
            return;
        };
        let outcome = dialog.form.answer_share(button, &mut self.branch_cache);
        self.on_spawn_outcome(outcome, window, cx);
    }

    /// Carries out what the form decided.
    fn on_spawn_outcome(&mut self, outcome: Outcome, window: &mut Window, cx: &mut Context<Self>) {
        match outcome {
            Outcome::Stay(messages) => {
                self.send_all(messages);
                self.sync_env_inputs(window, cx);
                self.sync_option_inputs(window, cx);
                self.after_spawn_change(cx);
                self.apply_spawn_focus(window, cx);
            }
            Outcome::Close => self.close_spawn_dialog(window, cx),
            Outcome::ConfirmShare => {
                self.spawn_focus.focus(window);
                cx.notify();
            }
            Outcome::Spawn(submission) => {
                let host = self.spawn_form_host();
                if let Some(msg) = submission.default_change {
                    self.send_to(&host, msg);
                }
                let aim = match &mut self.spawn_dialog {
                    Some(SpawnModal::Form(dialog)) => dialog.aim.take(),
                    _ => None,
                };
                let open_in = match aim {
                    Some(aim) => aim.open_in(submission.open_in),
                    None => submission.open_in,
                };
                self.close_spawn_dialog(window, cx);
                self.spawn_on(&host, submission.request, open_in, cx);
            }
        }
    }

    /// The fields follow the form, a timer waits out a suggestion, and the
    /// preview follows the fields, a timer waiting out its debounce.
    fn after_spawn_change(&mut self, cx: &mut Context<Self>) {
        let now = (self.now)();
        let Some(SpawnModal::Form(dialog)) = &mut self.spawn_dialog else {
            return;
        };
        dialog.form.follow_preview(now);
        dialog.preview_timer = dialog.form.preview_due().map(|due| {
            let delay = due.saturating_duration_since(now);
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(delay).await;
                // Fails only when the view is gone, and the dialog with it.
                this.update(cx, Self::send_due_preview).ok();
            })
        });
        let form = &dialog.form;
        let (branch, base) = (form.branch().to_owned(), form.base().to_owned());
        let placeholders = (form.branch_placeholder(now), form.default_base());
        sync_field(&dialog.branch_input, &branch, placeholders.0, cx);
        sync_field(&dialog.base_input, &base, placeholders.1, cx);
        let fields = form.option_fields();
        for option in &dialog.option_inputs {
            let Some(field) = fields.iter().find(|field| field.id == option.field) else {
                continue;
            };
            let Some((value, placeholder)) = text_field(field) else {
                continue;
            };
            // A field a trusted launch locks takes no typing.
            let read_only = field.disabled;
            option
                .input
                .update(cx, |input, cx| input.set_read_only(read_only, cx));
            // The form reads the text trimmed: typed text it agrees with
            // stays as typed.
            let typed = option.input.read(cx).text().to_owned();
            let text = if typed.trim() == value { typed } else { value };
            sync_field(&option.input, &text, placeholder, cx);
        }
        for (index, (row, inputs)) in form.env_rows().iter().zip(&dialog.env_inputs).enumerate() {
            sync_field(
                &inputs.value,
                &row.value,
                ENV_VALUE_PLACEHOLDER.to_owned(),
                cx,
            );
            let masked = form.env_value_masked(index);
            inputs
                .value
                .update(cx, |input, cx| input.set_masked(masked, cx));
        }
        dialog.timer = form
            .suggestion_deadline()
            .filter(|deadline| *deadline > now)
            .map(|deadline| {
                let delay = deadline.saturating_duration_since(now);
                cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(delay).await;
                    // Fails only when the view is gone, and the dialog with it.
                    this.update(cx, Self::after_spawn_change).ok();
                })
            });
        cx.notify();
    }

    /// The preview debounce ran out: its request goes to the daemon.
    fn send_due_preview(&mut self, cx: &mut Context<Self>) {
        let now = (self.now)();
        let msg = match &mut self.spawn_dialog {
            Some(SpawnModal::Form(dialog)) => dialog.form.take_due_preview(now),
            _ => None,
        };
        if let Some(msg) = msg {
            self.send_to(&self.spawn_form_host(), msg);
        }
        cx.notify();
    }

    /// The keyboard goes where the form's focus is: a text field, else the
    /// dialog itself. The body then scrolls the focused control into view.
    pub(crate) fn apply_spawn_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(SpawnModal::Form(dialog)) = &mut self.spawn_dialog else {
            if self.spawn_dialog.is_some() {
                self.spawn_focus.focus(window);
                cx.notify();
            }
            return;
        };
        dialog.reveal = true;
        let dialog = &*dialog;
        let input = if dialog.form.share_confirm().is_some() {
            None
        } else {
            dialog.input(&dialog.form.focused())
        };
        match input {
            Some(input) => input.read(cx).focus_handle(cx).focus(window),
            None => self.spawn_focus.focus(window),
        }
        cx.notify();
    }

    /// The focused control was laid out at `control`: after a focus change
    /// the body scrolls the least that shows it whole, and the window draws
    /// again at the new offset.
    fn reveal_spawn_focus(
        &mut self,
        control: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(SpawnModal::Form(dialog)) = &mut self.spawn_dialog else {
            return;
        };
        if !mem::take(&mut dialog.reveal) {
            return;
        }
        let viewport = dialog.scroll.bounds();
        let offset = dialog.scroll.offset();
        let y = reveal_offset(
            viewport.top()..viewport.bottom(),
            control.top()..control.bottom(),
            offset.y,
        );
        if y != offset.y {
            dialog.scroll.set_offset(point(offset.x, y));
            cx.defer_in(window, |_, window, _| window.refresh());
        }
    }

    /// Whether a text field of the dialog holds the keyboard.
    fn spawn_field_focused(&self, window: &Window, cx: &Context<Self>) -> bool {
        self.form_dialog().is_some_and(|dialog| {
            dialog
                .inputs()
                .any(|input| input.read(cx).focus_handle(cx).is_focused(window))
        })
    }

    /// A key while the dialog is open; returns whether it was the dialog's
    /// (every key but typing in a field is). Tab and Shift+Tab move the
    /// focus, Esc closes, Space and Enter press the focused control.
    pub(crate) fn on_spawn_dialog_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let key = keystroke.key.as_str();
        if matches!(self.spawn_dialog, Some(SpawnModal::NoRepos(_))) {
            match key {
                "escape" => self.close_spawn_dialog(window, cx),
                "enter" | "space" => self.add_repo(window, cx),
                _ => {}
            }
            return true;
        }
        let Some(form) = self.form_dialog().map(|dialog| &dialog.form) else {
            return false;
        };
        if let Some(button) = form.share_confirm() {
            self.on_share_key(key, button, window, cx);
            return true;
        }
        let focused = form.focused();
        match key {
            "tab" => {
                if let Some(SpawnModal::Form(dialog)) = &mut self.spawn_dialog {
                    dialog.form.move_focus(!keystroke.modifiers.shift);
                }
                self.apply_spawn_focus(window, cx);
                true
            }
            "escape" => {
                let list_closed =
                    ListField::of(&focused).is_some_and(|list| self.escape_spawn_list(list, cx));
                if !list_closed {
                    self.close_spawn_dialog(window, cx);
                }
                true
            }
            _ if self.spawn_field_focused(window, cx) => false,
            "enter" | "space" => {
                self.press_spawn_control(&focused, window, cx);
                true
            }
            _ => true,
        }
    }

    /// A key while the "Share this worktree?" confirm is open: Esc cancels,
    /// Enter and Space press the focused button, Tab moves the focus.
    fn on_share_key(
        &mut self,
        key: &str,
        focused: ShareButton,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match key {
            "escape" => self.answer_share(ShareButton::Cancel, window, cx),
            "enter" | "space" => self.answer_share(focused, window, cx),
            "tab" => {
                if let Some(SpawnModal::Form(dialog)) = &mut self.spawn_dialog {
                    dialog.form.toggle_share_focus();
                }
                cx.notify();
            }
            _ => {}
        }
    }

    /// A daemon message from `host` the dialog follows. Suggestions are
    /// cached whether or not it is open. A host's repo or workspace list
    /// replaces that host's targets in the dialog.
    pub(crate) fn on_spawn_dialog_message(
        &mut self,
        host: &HostKey,
        msg: &DaemonMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let now = (self.now)();
        let registry = matches!(
            msg,
            DaemonMessage::Repos { .. } | DaemonMessage::Workspaces { .. }
        );
        let suggestion = matches!(msg, DaemonMessage::BranchNameSuggestion { .. });
        // The dialog asks without a request id, so a reply carrying one
        // answers a launch-last wait instead.
        if let DaemonMessage::BranchNameSuggestion {
            target,
            name,
            request_id,
        } = msg
            && request_id.is_none()
            && let Some(target) = Target::from_suggest(host, target)
        {
            self.branch_cache.insert(target.clone(), name.clone());
            if let Some(SpawnModal::Form(dialog)) = &mut self.spawn_dialog {
                dialog.form.on_suggestion(&target, name);
            }
        }
        if matches!(self.spawn_dialog, Some(SpawnModal::NoRepos(_))) {
            if registry && self.has_repos() {
                self.leave_no_repos(window, cx);
            }
            return;
        }
        let Some(SpawnModal::Form(dialog)) = &mut self.spawn_dialog else {
            return;
        };
        let messages = if registry {
            let inputs = self.spawn_host_inputs(host);
            let Some(SpawnModal::Form(dialog)) = &mut self.spawn_dialog else {
                return;
            };
            match dialog
                .form
                .set_registry(inputs, &mut self.branch_cache, now)
            {
                Some(messages) => messages,
                None => return self.close_spawn_dialog(window, cx),
            }
        } else {
            match dialog.form.on_message(host, msg) {
                Some(messages) => messages,
                None if suggestion => Vec::new(),
                None => return,
            }
        };
        self.send_all(messages);
        self.after_spawn_change(cx);
    }

    /// The tabs changed: Open in follows them.
    pub(crate) fn refresh_spawn_tabs(&mut self) {
        if let Some(SpawnModal::Form(dialog)) = &mut self.spawn_dialog {
            let tabs = TabChoices::from_tabs(self.tabs.tabs(), self.tabs.active_id());
            dialog.form.set_tabs(tabs);
        }
    }

    /// The dialog over a backdrop that takes every click beneath it, and the
    /// share confirm over it while open.
    pub(crate) fn spawn_dialog_layers(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let t = theme(cx);
        let dialog = match &self.spawn_dialog {
            None => return Vec::new(),
            Some(SpawnModal::NoRepos(_)) => return vec![self.no_repos_layer(cx)],
            Some(SpawnModal::Form(dialog)) => dialog,
        };
        let form = &dialog.form;
        let focus = form.focused();
        let placement = group()
            .child(target_field(form, &focus, cx))
            .child(runtime_field(form, &focus, cx))
            .children(agent_options_view::rows(
                form,
                &dialog.option_inputs,
                SpawnPlacement::Main,
                &focus,
                cx,
            ))
            .child(open_in_field(form, &focus, cx));
        let run = divided(&t)
            .children(trusted_rows(form, &focus, cx))
            .child(advanced_section(dialog, &focus, cx));
        let body = div()
            .id("spawn-body")
            .debug_selector(|| "spawn-body".to_owned())
            .flex()
            .flex_col()
            .gap(px(16.0))
            .px(px(DIALOG_PAD_X))
            .py(px(16.0))
            .min_h(px(0.0))
            .overflow_y_scroll()
            .track_scroll(&dialog.scroll)
            .child(placement)
            .child(divided(&t).children(self.worktree_rows(dialog, &focus, cx)))
            .child(run);
        let close = close_button("spawn-close", focus == Control::Close, &theme(cx)).on_click(
            cx.listener(|this, _: &ClickEvent, window, cx| this.close_spawn_dialog(window, cx)),
        );
        let panel = dialog_card(
            crate::fonts::ui_text_size(cx),
            "spawn-panel",
            DIALOG_WIDTH,
            &theme(cx),
        )
        .max_h(relative(PANEL_MAX_HEIGHT))
        .when(form.share_confirm().is_none(), |panel| {
            panel.track_focus(&self.spawn_focus)
        })
        .child(dialog_title_bar(
            crate::fonts::ui_text_size(cx),
            crate::fonts::ui_weight_shift(cx),
            DIALOG_TITLE,
            close,
            &t,
        ))
        .child(body)
        .child(footer(form, &focus, cx))
        .child(measurer(Measured::Panel, cx));
        let mut layers = vec![backdrop("spawn-dialog", panel, &theme(cx))];
        if let Some(button) = form.share_confirm() {
            layers.push(self.share_layer(button, cx));
        }
        layers
    }

    /// The worktree checkbox and mode, the existing pick, the branch and the
    /// base: a repo's checkbox leads, a workspace's branch field does.
    fn worktree_rows(
        &self,
        dialog: &SpawnDialog,
        focus: &Control,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let form = &dialog.form;
        let now = (self.now)();
        let branch = (!form.pinning()).then(|| branch_field(dialog, focus, now, cx));
        let mut worktree = vec![worktree_checkbox(form, focus, cx)];
        if form.use_worktree() {
            worktree.push(mode_field(form, focus, cx));
        }
        if form.pinning() {
            worktree.push(existing_field(form, focus, cx));
        }
        let mut base = Vec::new();
        if form.use_worktree() && !form.pinning() {
            base.push(base_field(dialog, focus, cx));
            base.extend(preview_rows(form, focus, cx));
        }
        if form.is_workspace() {
            branch.into_iter().chain(worktree).chain(base).collect()
        } else {
            worktree.into_iter().chain(branch).chain(base).collect()
        }
    }

    /// The dialog with no repo to spawn into: the target's no-repos note over
    /// Add repo, and Spawn disabled.
    fn no_repos_layer(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = theme(cx);
        let close = close_button("spawn-close", false, &theme(cx)).on_click(
            cx.listener(|this, _: &ClickEvent, window, cx| this.close_spawn_dialog(window, cx)),
        );
        let gate = self.add_repo_gate();
        let add = button(
            "spawn-add-repo",
            button_look(
                crate::fonts::ui_text_size(cx),
                crate::fonts::ui_weight_shift(cx),
                OUTLINED.0,
                OUTLINED.1,
                gate.enabled,
                Layer::Floating,
                &theme(cx),
            ),
        )
        .flex_none()
        .child("Add repo")
        .tooltip(tooltip(gate.tip))
        .when(gate.enabled, |add| {
            add.on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.add_repo(window, cx);
            }))
        });
        let body = div()
            .debug_selector(|| "spawn-no-repos".to_owned())
            .flex()
            .flex_col()
            .items_start()
            .gap(px(12.0))
            .px(px(DIALOG_PAD_X))
            .py(px(16.0))
            .child(
                tagged_text(
                    "spawn-no-repos-note",
                    spawn_no_repos_note(self.conn().target()),
                )
                .text_color(gpui::rgb(t.text_2)),
            )
            .child(add);
        let cancel = button(
            "spawn-cancel",
            button_look(
                crate::fonts::ui_text_size(cx),
                crate::fonts::ui_weight_shift(cx),
                ButtonKind::Outlined,
                ButtonSize::Large,
                true,
                Layer::Floating,
                &theme(cx),
            ),
        )
        .flex_none()
        .child("Cancel")
        .on_click(
            cx.listener(|this, _: &ClickEvent, window, cx| this.close_spawn_dialog(window, cx)),
        );
        let submit = button(
            "spawn-submit",
            button_look(
                crate::fonts::ui_text_size(cx),
                crate::fonts::ui_weight_shift(cx),
                ButtonKind::Primary,
                ButtonSize::Large,
                false,
                Layer::Floating,
                &theme(cx),
            ),
        )
        .flex_none()
        .child("Spawn");
        let panel = dialog_card(
            crate::fonts::ui_text_size(cx),
            "spawn-panel",
            DIALOG_WIDTH,
            &theme(cx),
        )
        .track_focus(&self.spawn_focus)
        .child(dialog_title_bar(
            crate::fonts::ui_text_size(cx),
            crate::fonts::ui_weight_shift(cx),
            DIALOG_TITLE,
            close,
            &t,
        ))
        .child(body)
        .child(dialog_footer(&t).child(cancel).child(submit));
        backdrop("spawn-dialog", panel, &theme(cx))
    }

    fn share_layer(&self, focused: ShareButton, cx: &mut Context<Self>) -> AnyElement {
        let buttons: Vec<AnyElement> = [ShareButton::Cancel, ShareButton::Launch]
            .into_iter()
            .map(|button| {
                dialog_button(
                    button.selector(),
                    button.label().to_owned(),
                    dialog_look(
                        crate::fonts::ui_text_size(cx),
                        crate::fonts::ui_weight_shift(cx),
                        false,
                        true,
                        &theme(cx),
                    ),
                    focused == button,
                )
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.answer_share(button, window, cx);
                }))
                .into_any_element()
            })
            .collect();
        let panel = modal_panel(
            crate::fonts::ui_text_size(cx),
            "spawn-share-panel",
            &theme(cx),
        )
        .track_focus(&self.spawn_focus)
        .child(
            div()
                .font_weight(crate::fonts::ui_weight(cx, FontWeight::SEMIBOLD))
                .child(SHARE_TITLE),
        )
        .child(div().child(SHARE_BODY))
        .child(div().flex().justify_end().gap(px(6.0)).children(buttons));
        backdrop("spawn-share-worktree-confirm", panel, &theme(cx))
    }
}

/// The input shows the form's text and placeholder; `set_text` sends no
/// change back.
fn sync_field(
    input: &Entity<TextInput>,
    text: &str,
    placeholder: String,
    cx: &mut Context<RootView>,
) {
    input.update(cx, |input, cx| {
        if input.text() != text {
            input.set_text(text.to_owned(), cx);
        }
        input.set_placeholder(placeholder, cx);
    });
}

/// Seconds since the Unix epoch, for anything that reads an age.
pub(crate) fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
        })
}

/// A dialog's card, `width` wide and tagged `id`: the modal card, its
/// header, body and footer bands drawn edge to edge.
pub(crate) fn dialog_card(offset: i8, id: &'static str, width: f32, t: &UiTheme) -> Stateful<Div> {
    modal_panel(offset, id, t)
        .debug_selector(move || id.to_owned())
        .w(px(width))
        .p(px(0.0))
        .gap(px(0.0))
}

/// A dialog's header band: the bold title, then `close` at the right, over
/// a divider.
pub(crate) fn dialog_title_bar(
    offset: i8,
    shift: i8,
    title: impl Into<SharedString>,
    close: impl IntoElement,
    t: &UiTheme,
) -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(10.0))
        .pl(px(DIALOG_PAD_X))
        .pr(px(12.0))
        .py(px(11.0))
        .border_b_1()
        .border_color(gpui::rgb(t.line))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .text_size(crate::fonts::ui_px_at(offset, TITLE_SIZE))
                .font_weight(crate::fonts::ui_weight_at(shift, FontWeight::BOLD))
                .child(title.into()),
        )
        .child(close)
}

/// A dialog's footer band, on the raised ground under a divider; the caller
/// adds its buttons, which sit at the right.
pub(crate) fn dialog_footer(t: &UiTheme) -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_end()
        .gap(px(10.0))
        .px(px(DIALOG_PAD_X))
        .py(px(14.0))
        .border_t_1()
        .border_color(gpui::rgb(t.line))
        .bg(gpui::rgb(t.modal_inset))
        .rounded_b(px(MODAL_RADIUS - 1.0))
}

/// A dialog's close button, tagged `selector`: a borderless 28 px square
/// holding the ✕, ringed while `focused`; the caller adds the handler.
pub(crate) fn close_button(selector: &str, focused: bool, t: &UiTheme) -> Stateful<Div> {
    let name = selector.to_owned();
    div()
        .id(ElementId::Name(SharedString::from(name.clone())))
        .debug_selector(|| name)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(CLOSE_SIZE))
        .rounded(px(CLOSE_RADIUS))
        .border_1()
        .border_color(gpui::rgba(t.transparent))
        .text_color(gpui::rgb(t.text_2))
        .cursor_pointer()
        .hover(|style| {
            style
                .bg(gpui::rgb(t.popover_hover))
                .text_color(gpui::rgb(t.text))
        })
        .child("✕")
        .when(focused, |close| close.child(focus_ring(CLOSE_RADIUS, t)))
}

/// The dialog's first rows, which stand under no divider.
fn group() -> Div {
    div().flex().flex_col().gap(px(14.0))
}

/// A group of the dialog's rows under a divider.
fn divided(t: &UiTheme) -> Div {
    group()
        .pt(px(14.0))
        .border_t_1()
        .border_color(gpui::rgb(t.line))
}

/// A field's label: small, semibold and secondary.
pub(crate) fn field_label(
    offset: i8,
    shift: i8,
    label: impl Into<SharedString>,
    t: &UiTheme,
) -> Div {
    div()
        .text_size(crate::fonts::ui_px_at(offset, LABEL_SIZE))
        .font_weight(crate::fonts::ui_weight_at(shift, FontWeight::SEMIBOLD))
        .text_color(gpui::rgb(t.text_2))
        .child(label.into())
}

/// A labelled field.
pub(crate) fn field(offset: i8, shift: i8, label: impl Into<SharedString>, t: &UiTheme) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(7.0))
        .child(field_label(offset, shift, label, t))
}

/// A segmented control: its segments in a well a step lighter than the
/// modal, inside a thin edge, wrapping onto more lines when they do not fit.
pub(crate) fn segmented(buttons: Vec<AnyElement>, t: &UiTheme) -> Div {
    div()
        .flex()
        .flex_wrap()
        .gap(px(2.0))
        .p(px(3.0))
        .rounded(px(SEGMENTED_RADIUS))
        .bg(gpui::rgb(t.modal_inset))
        .border_1()
        .border_color(gpui::rgb(t.line))
        .children(buttons)
}

pub(crate) fn muted(text: impl Into<SharedString>, t: &UiTheme) -> Div {
    div().text_color(gpui::rgb(t.muted)).child(text.into())
}

/// A note under a checkbox's or a radio's label.
pub(crate) fn note(offset: i8, text: impl Into<SharedString>, t: &UiTheme) -> Div {
    div()
        .text_size(crate::fonts::ui_px_at(offset, NOTE_SIZE))
        .text_color(gpui::rgb(t.subtle))
        .child(text.into())
}

/// A segment of a [`segmented`] control: filled and bold when chosen,
/// ringed when focused, dimmed and inert when disabled. The Settings
/// modal's choice rows are built from it too, so both places look the same.
pub(crate) fn choice_button(
    offset: i8,
    shift: i8,
    name: String,
    label: impl IntoElement,
    look: Look,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static,
    t: &UiTheme,
) -> Stateful<Div> {
    let (text, weight) = if look.selected {
        (t.text, FontWeight::SEMIBOLD)
    } else {
        (t.text_2, FontWeight::MEDIUM)
    };
    div()
        .id(ElementId::Name(SharedString::from(name.clone())))
        .debug_selector(move || name)
        .flex()
        .flex_grow()
        .items_center()
        .justify_center()
        .gap(px(6.0))
        .min_h(crate::fonts::ui_px_at(offset, SEGMENT_HEIGHT))
        .px(px(12.0))
        .rounded(px(SEGMENT_RADIUS))
        .border_1()
        .border_color(gpui::rgba(t.transparent))
        .text_size(crate::fonts::ui_px_at(offset, SEGMENT_TEXT_SIZE))
        .font_weight(crate::fonts::ui_weight_at(shift, weight))
        .text_color(gpui::rgb(text))
        .when(look.selected, |segment| {
            segment.bg(gpui::rgb(t.popover_hover))
        })
        .when(look.enabled, |segment| {
            segment
                .cursor_pointer()
                .hover(move |style| style.text_color(gpui::rgb(t.text)))
                .on_click(on_click)
        })
        .when(!look.enabled, |segment| {
            segment.opacity(DISABLED_OPACITY).cursor_default()
        })
        .when(look.focused, |segment| {
            segment.child(focus_ring(SEGMENT_RADIUS, t))
        })
        .child(label)
}

/// A checkbox row tagged `selector`: its box, the accent with a tick when
/// `checked` and a strong outline when not, ringed while `focused`, then
/// `label`. The caller adds the handler.
pub(crate) fn checkbox_row(
    offset: i8,
    shift: i8,
    selector: &str,
    checked: bool,
    focused: bool,
    label: impl IntoElement,
    t: &UiTheme,
) -> Stateful<Div> {
    let name = selector.to_owned();
    // Each state tags the box itself, building its name only when a spec reads
    // it, so a release render allocates nothing for them.
    let mark = div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .mt(px(1.0))
        .size(px(CHECK_SIZE))
        .rounded(px(CHECK_RADIUS))
        .border_1()
        .text_size(px(11.0))
        .font_weight(crate::fonts::ui_weight_at(shift, FontWeight::BOLD))
        .when(checked, |mark| {
            mark.debug_selector(|| format!("{name}-tick"))
                .bg(gpui::rgb(t.accent))
                .border_color(gpui::rgb(t.accent))
                .text_color(gpui::rgb(t.on_accent))
                .child("✓")
        })
        .when(!checked, |mark| {
            mark.debug_selector(|| format!("{name}-empty"))
                .border_color(gpui::rgb(t.line_strong))
        })
        .when(focused, |mark| mark.child(focus_ring(CHECK_RADIUS, t)));
    div()
        .id(ElementId::Name(SharedString::from(name.clone())))
        .debug_selector(|| name)
        .flex()
        .items_start()
        .gap(px(10.0))
        .text_size(crate::fonts::ui_px_at(offset, CHECK_LABEL_SIZE))
        .cursor_pointer()
        .child(mark)
        .child(label)
}

/// A checkbox's or a radio's label: its text, and a note under it.
pub(crate) fn check_label(text: impl Into<SharedString>, under: Option<Div>) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .child(div().child(text.into()))
        .children(under)
}

/// A radio row tagged `selector`: its ring, the accent with a dot when
/// `chosen`, ringed while `focused`, then `label`.
fn radio_row(
    selector: &str,
    chosen: bool,
    focused: bool,
    label: impl IntoElement,
    t: &UiTheme,
) -> Stateful<Div> {
    let name = selector.to_owned();
    let ring = div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .mt(px(1.0))
        .size(px(CHECK_SIZE))
        .rounded_full()
        .border_1()
        .border_color(gpui::rgb(if chosen { t.accent } else { t.line_strong }))
        .when(chosen, |ring| {
            ring.child(
                div()
                    .size(px(RADIO_DOT))
                    .rounded_full()
                    .bg(gpui::rgb(t.accent)),
            )
        })
        .when(focused, |ring| ring.child(focus_ring(CHECK_SIZE / 2.0, t)));
    div()
        .id(ElementId::Name(SharedString::from(name.clone())))
        .debug_selector(|| name)
        .flex()
        .items_start()
        .gap(px(10.0))
        .cursor_pointer()
        .child(ring)
        .child(label)
}

/// A dialog control of `kind` and `size` holding `label`: it presses its
/// control, wears the ring while focused and is revealed into view then.
fn spawn_button(
    control: &Control,
    (kind, size): (ButtonKind, ButtonSize),
    label: impl IntoElement,
    enabled: bool,
    focus: &Control,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    let focused = focus == control;
    let base = button(
        &control.selector(),
        button_look(
            crate::fonts::ui_text_size(cx),
            crate::fonts::ui_weight_shift(cx),
            kind,
            size,
            enabled,
            Layer::Floating,
            &theme(cx),
        ),
    )
    .flex_none()
    .child(label)
    .when(focused, |button| {
        button.child(focus_ring(size.radius(), &theme(cx)))
    });
    let base = if enabled {
        base.on_click(press_control(control, cx))
    } else {
        base
    };
    reveal_when_focused(base, control, focused, cx)
}

/// `element` with the reveal marker while it is the focused control of the
/// scrolling body.
pub(crate) fn reveal_when_focused(
    element: Stateful<Div>,
    control: &Control,
    focused: bool,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    if focused && in_body(control) {
        element.child(reveal_marker(cx))
    } else {
        element
    }
}

/// The listener that presses `control`.
pub(crate) fn press_control(
    control: &Control,
    cx: &mut Context<RootView>,
) -> impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static {
    let pressed = control.clone();
    cx.listener(move |this, _: &ClickEvent, window, cx| {
        this.press_spawn_control(&pressed, window, cx);
    })
}

/// A dialog control's choice button, revealed into view when focused.
pub(crate) fn option_button(
    control: &Control,
    label: impl IntoElement,
    look: Look,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    let button = choice_button(
        crate::fonts::ui_text_size(cx),
        crate::fonts::ui_weight_shift(cx),
        control.selector(),
        label,
        look,
        press_control(control, cx),
        &theme(cx),
    );
    reveal_when_focused(button, control, look.focused, cx)
}

/// Whether a control sits in the scrolling body, not the header or footer.
fn in_body(control: &Control) -> bool {
    !matches!(control, Control::Close | Control::Cancel | Control::Submit)
}

/// Laid over the focused control: where it lands tells the body how far to
/// scroll to show it. An absolute child sits inside its parent's border, so
/// the marker reaches out over the controls' 1px outline (`border_1`).
fn reveal_marker(cx: &mut Context<RootView>) -> impl IntoElement {
    let root = cx.weak_entity();
    canvas(
        move |bounds, window, cx| {
            // Fails only when the view is gone, and the dialog with it.
            root.update(cx, |this, cx| this.reveal_spawn_focus(bounds, window, cx))
                .ok();
        },
        |_, (), _, _| {},
    )
    .absolute()
    .inset(px(-1.0))
}

/// The body's new scroll offset (negative once scrolled down) that shows
/// `control` whole in `viewport` with the least movement, both ranges being
/// where they lie at `offset`. A control taller than the viewport shows its
/// top.
fn reveal_offset(viewport: Range<Pixels>, control: Range<Pixels>, offset: Pixels) -> Pixels {
    let too_tall = control.end - control.start > viewport.end - viewport.start;
    if control.start < viewport.start || too_tall {
        offset + (viewport.start - control.start)
    } else if control.end > viewport.end {
        offset - (control.end - viewport.end)
    } else {
        offset
    }
}

fn choice(
    control: &Control,
    label: impl IntoElement,
    selected: bool,
    focus: &Control,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let look = Look {
        selected,
        focused: focus == control,
        enabled: true,
    };
    option_button(control, label, look, cx).into_any_element()
}

/// The Target field: one segmented row of every target, or, with two or
/// more hosts listed, a header row per host over its own targets (an
/// offline host's dimmed and disabled, an empty host's no-repos line).
fn target_field(form: &SpawnForm, focus: &Control, cx: &mut Context<RootView>) -> Div {
    let t = theme(cx);
    let label = field(
        crate::fonts::ui_text_size(cx),
        crate::fonts::ui_weight_shift(cx),
        "Target",
        &t,
    );
    if !form.grouped() {
        let buttons = target_buttons(form, form.shown_targets(), focus, cx);
        return label.child(segmented(buttons, &t));
    }
    let groups = form.target_groups().into_iter().map(|group| {
        let key = group.host.wire().unwrap_or("local").to_owned();
        let header = labelled_text(format!("spawn-target-host-{key}"), group.label.clone())
            .font_weight(crate::fonts::ui_weight(cx, FontWeight::SEMIBOLD))
            .text_color(gpui::rgb(t.text_2))
            .when(group.offline, |header| header.opacity(DISABLED_OPACITY));
        let body = if group.targets.is_empty() {
            labelled_text(
                format!("spawn-target-no-repos-{key}"),
                group.no_repos_note(),
            )
            .text_color(gpui::rgb(t.muted))
            .when(group.offline, |note| note.opacity(DISABLED_OPACITY))
            .into_any_element()
        } else {
            segmented(target_buttons(form, group.targets, focus, cx), &t).into_any_element()
        };
        div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(header)
            .child(body)
    });
    label.child(div().flex().flex_col().gap(px(8.0)).children(groups))
}

/// `text` tagged `selector`, with its text tagged `<selector>=<text>`.
fn labelled_text(selector: String, text: String) -> Div {
    let tag = format!("{selector}={text}");
    div()
        .debug_selector(move || selector)
        .child(div().debug_selector(move || tag).child(text))
}

/// One picker button per target in `targets`, each enabled only where the
/// form lets it be picked.
fn target_buttons(
    form: &SpawnForm,
    targets: Vec<Target>,
    focus: &Control,
    cx: &mut Context<RootView>,
) -> Vec<AnyElement> {
    targets
        .into_iter()
        .map(|target| {
            let label = form.target_label(&target);
            let selected = &target == form.target();
            let enabled = form.target_enabled(&target);
            let control = Control::Target(target);
            let look = Look {
                selected,
                focused: focus == &control,
                enabled,
            };
            option_button(&control, label, look, cx).into_any_element()
        })
        .collect()
}

fn runtime_field(form: &SpawnForm, focus: &Control, cx: &mut Context<RootView>) -> Div {
    let t = theme(cx);
    let buttons = form
        .runtimes()
        .into_iter()
        .map(|runtime| {
            let selected = runtime == form.runtime();
            let label = form.runtime_label(&runtime);
            choice(&Control::Runtime(runtime), label, selected, focus, cx)
        })
        .collect();
    field(
        crate::fonts::ui_text_size(cx),
        crate::fonts::ui_weight_shift(cx),
        "Runtime",
        &t,
    )
    .child(segmented(buttons, &t))
}

fn open_in_field(form: &SpawnForm, focus: &Control, cx: &mut Context<RootView>) -> Div {
    let t = theme(cx);
    if let Some(id) = form.open_in_locked() {
        let control = Control::OpenIn(OpenChoice::Tab(id.to_owned()));
        let name = form
            .tab_name(id)
            .map_or_else(|| id.to_owned(), str::to_owned);
        let look = Look {
            selected: true,
            focused: *focus == control,
            enabled: false,
        };
        let chip = option_button(&control, name, look, cx);
        return field(
            crate::fonts::ui_text_size(cx),
            crate::fonts::ui_weight_shift(cx),
            "Open in",
            &t,
        )
        .child(segmented(vec![chip.into_any_element()], &t));
    }
    let chosen = form.open_in();
    let tabs = form.tabs();
    let current = Control::OpenIn(OpenChoice::CurrentTab);
    let current_label = div()
        .flex()
        .gap(px(6.0))
        .child("Current tab")
        .children(tabs.current.as_ref().map(|tab| muted(tab.name.clone(), &t)));
    let look = Look {
        selected: *chosen == OpenChoice::CurrentTab,
        focused: *focus == current,
        enabled: tabs.current.is_some(),
    };
    let current_button = option_button(&current, current_label, look, cx)
        .when(tabs.current.is_none(), |button| {
            button.tooltip(tooltip(CURRENT_TAB_DISABLED))
        })
        .into_any_element();
    let mut buttons = vec![current_button];
    let new_tab = OpenChoice::NewTab;
    let selected = *chosen == new_tab;
    buttons.push(choice(
        &Control::OpenIn(new_tab),
        "New tab",
        selected,
        focus,
        cx,
    ));
    for tab in &tabs.others {
        let option = OpenChoice::Tab(tab.id.clone());
        let selected = *chosen == option;
        buttons.push(choice(
            &Control::OpenIn(option),
            tab.name.clone(),
            selected,
            focus,
            cx,
        ));
    }
    field(
        crate::fonts::ui_text_size(cx),
        crate::fonts::ui_weight_shift(cx),
        "Open in",
        &t,
    )
    .child(segmented(buttons, &t))
}

/// A checkbox of the dialog: its box and `label`, pressing its control.
fn checkbox(
    control: &Control,
    checked: bool,
    focus: &Control,
    label: Div,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let focused = focus == control;
    let row = checkbox_row(
        crate::fonts::ui_text_size(cx),
        crate::fonts::ui_weight_shift(cx),
        &control.selector(),
        checked,
        focused,
        label,
        &theme(cx),
    )
    .on_click(press_control(control, cx));
    div()
        .flex()
        .child(reveal_when_focused(row, control, focused, cx))
        .into_any_element()
}

fn trusted_rows(form: &SpawnForm, focus: &Control, cx: &mut Context<RootView>) -> Vec<AnyElement> {
    let t = theme(cx);
    if !form.trusted_shown() {
        return Vec::new();
    }
    let label = check_label(
        "Trusted launch",
        form.trusted_detail()
            .map(|detail| note(crate::fonts::ui_text_size(cx), detail.to_owned(), &t)),
    );
    let mut rows = vec![checkbox(
        &Control::Trusted,
        form.trusted(),
        focus,
        label,
        cx,
    )];
    if form.trusted() {
        let warning = warning_box(&t)
            .debug_selector(|| "spawn-trusted-launch-warning".to_owned())
            .gap(px(2.0))
            .child(
                div()
                    .font_weight(crate::fonts::ui_weight(cx, FontWeight::SEMIBOLD))
                    .child("Trusted launch"),
            )
            .child(TRUSTED_WARNING);
        rows.push(warning.into_any_element());
    }
    rows
}

fn worktree_checkbox(form: &SpawnForm, focus: &Control, cx: &mut Context<RootView>) -> AnyElement {
    let t = theme(cx);
    let (text, unchecked) = if form.is_workspace() {
        (
            "Create worktrees",
            "Unchecked: check out the branch in each member's main directory",
        )
    } else {
        (
            "Create a worktree",
            "Unchecked: run the agent in the repo's main directory",
        )
    };
    let label = check_label(
        text,
        Some(note(crate::fonts::ui_text_size(cx), unchecked, &t)),
    );
    checkbox(&Control::UseWorktree, form.use_worktree(), focus, label, cx)
}

fn mode_field(form: &SpawnForm, focus: &Control, cx: &mut Context<RootView>) -> AnyElement {
    let t = theme(cx);
    let (label, new) = if form.is_workspace() {
        ("Worktrees", "New worktrees")
    } else {
        ("Worktree", "New worktree")
    };
    let buttons = [
        (WorktreeMode::New, new),
        (WorktreeMode::Existing, "Use existing"),
    ]
    .into_iter()
    .map(|(mode, text)| choice(&Control::Mode(mode), text, form.mode() == mode, focus, cx))
    .collect();
    field(
        crate::fonts::ui_text_size(cx),
        crate::fonts::ui_weight_shift(cx),
        label,
        &t,
    )
    .child(segmented(buttons, &t))
    .into_any_element()
}

fn existing_field(form: &SpawnForm, focus: &Control, cx: &mut Context<RootView>) -> AnyElement {
    let t = theme(cx);
    let label = if form.is_workspace() {
        "Existing worktree group"
    } else {
        "Existing worktree"
    };
    let now = now_unix();
    let options: Vec<AnyElement> = form
        .existing_options()
        .iter()
        .map(|option| {
            let selected = form.existing_selected() == Some(option.key.as_str());
            let control = Control::Existing(option.key.clone());
            choice(&control, option.label(now), selected, focus, cx)
        })
        .collect();
    let placeholder = form
        .existing_selected()
        .is_none()
        .then(|| muted(form.existing_placeholder(), &t));
    let note = form.existing_note().map(|text| muted(text, &t));
    let warning = form
        .existing_warning()
        .map(|text| div().text_color(gpui::rgb(t.warning)).child(text));
    field(
        crate::fonts::ui_text_size(cx),
        crate::fonts::ui_weight_shift(cx),
        label,
        &t,
    )
    .children(placeholder)
    .when(!options.is_empty(), |field| {
        field.child(segmented(options, &t).flex_col())
    })
    .children(note)
    .children(warning)
    .into_any_element()
}

/// A text field in a box, tagged with its control's selector.
pub(crate) fn input_box(
    control: &Control,
    input: &Entity<TextInput>,
    focus: &Control,
    cx: &mut Context<RootView>,
) -> Div {
    let selector = control.selector();
    let focused = focus == control;
    field_frame(focused, &theme(cx))
        .debug_selector(move || selector)
        .flex_1()
        .min_w(px(0.0))
        .child(input.clone())
        .when(focused, |field| field.child(reveal_marker(cx)))
}

/// Measures where it is laid out, filling its parent, for `what`.
fn measurer(what: Measured, cx: &mut Context<RootView>) -> impl IntoElement {
    let root = cx.weak_entity();
    canvas(
        move |bounds, window, cx| {
            // Fails only when the view is gone, and the dialog with it.
            root.update(cx, |this, cx| this.measure_spawn(what, bounds, window, cx))
                .ok();
        },
        |_, (), _, _| {},
    )
    .absolute()
    .inset(px(0.0))
}

/// A text field with its branch list: a click opens the list, a press
/// outside the field and the list closes it, and while it shows the list
/// hangs under the field (over it when there is more room there), inside
/// the dialog.
fn list_input(
    dialog: &SpawnDialog,
    list: ListField,
    focus: &Control,
    cx: &mut Context<RootView>,
) -> Div {
    let control = list.control();
    let input = dialog.input(&control).cloned();
    let boxed = match &input {
        Some(input) => input_box(&control, input, focus, cx),
        None => div(),
    };
    let shown = dialog.form.list_shown(list);
    boxed
        .relative()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _: &MouseDownEvent, _, cx| this.open_spawn_list(list, cx)),
        )
        .on_mouse_down_out(cx.listener(move |this, _: &MouseDownEvent, _, cx| {
            this.close_spawn_list(list, cx);
        }))
        .when(shown, |boxed| {
            boxed
                .child(measurer(Measured::ListAnchor, cx))
                .child(branch_list(dialog, list, cx))
        })
}

/// The open list of `list`'s field, drawn over the dialog.
fn branch_list(dialog: &SpawnDialog, list: ListField, cx: &mut Context<RootView>) -> AnyElement {
    let t = theme(cx);
    let form = &dialog.form;
    let rows = form.list_rows(list);
    let highlight = form.list_highlight(list);
    let current = form.current_branch();
    let prefix = list.control().selector();
    let wanted = px(LIST_MAX_HEIGHT);
    let (above, max_height) = match (dialog.panel_bounds, dialog.list_anchor) {
        (Some(panel), Some(anchor)) => list_placement(
            panel.top()..panel.bottom(),
            anchor.top()..anchor.bottom(),
            wanted,
        ),
        _ => (false, wanted),
    };
    let items: Vec<AnyElement> = rows
        .iter()
        .enumerate()
        .map(|(index, row)| {
            let is_current =
                matches!(row, ComboRow::Branch(name) if Some(name.as_str()) == current);
            list_row(
                list,
                &prefix,
                index,
                row,
                (index == highlight, is_current),
                cx,
            )
        })
        .collect();
    let name = format!("{prefix}-list");
    let panel = div()
        .id(ElementId::Name(SharedString::from(name.clone())))
        .debug_selector(|| name)
        .absolute()
        .left(px(0.0))
        .w_full()
        .when(above, |list| list.bottom(relative(1.0)).mb(px(2.0)))
        .when(!above, |list| list.top(relative(1.0)).mt(px(2.0)))
        .max_h(max_height)
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .p(px(4.0))
        .bg(gpui::rgb(t.popover))
        .border_1()
        .border_color(gpui::rgb(t.popover_edge))
        .rounded(px(LIST_RADIUS))
        .occlude()
        .children(items);
    deferred(panel).with_priority(1).into_any_element()
}

/// One row of a branch list; `(highlighted, current)` say how it looks.
fn list_row(
    list: ListField,
    prefix: &str,
    index: usize,
    row: &ComboRow,
    (highlighted, current): (bool, bool),
    cx: &mut Context<RootView>,
) -> AnyElement {
    let t = theme(cx);
    let name = format!("{prefix}-option-{index}");
    let tag_name = format!("{name}-current");
    let tag = current.then(|| muted(CURRENT_TAG, &t).debug_selector(move || tag_name));
    let detail_name = format!("{name}-scopes");
    let detail = row
        .detail()
        .map(|detail| muted(detail.to_owned(), &t).debug_selector(move || detail_name));
    div()
        .id(ElementId::Name(SharedString::from(name.clone())))
        .debug_selector(|| name)
        .flex()
        .justify_between()
        .gap(px(6.0))
        .px(px(8.0))
        .py(px(4.0))
        .rounded(px(SEGMENT_RADIUS))
        .cursor_pointer()
        .when(highlighted, move |row| row.bg(gpui::rgb(t.popover_hover)))
        .on_mouse_move(cx.listener(move |this, _: &MouseMoveEvent, _, cx| {
            this.hover_spawn_list(list, index, cx);
        }))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
                this.pick_spawn_list(list, index, cx);
            }),
        )
        .child(
            div()
                .flex()
                .gap(px(8.0))
                .child(row.label())
                .children(detail),
        )
        .children(tag)
        .into_any_element()
}

fn branch_field(
    dialog: &SpawnDialog,
    focus: &Control,
    now: Instant,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let t = theme(cx);
    let form = &dialog.form;
    let label = match (form.is_workspace(), form.use_worktree()) {
        (true, true) => "New worktree branch (same across all members)",
        (true, false) => "Branch (same across all members)",
        (false, true) => "New worktree branch",
        (false, false) => "Branch",
    };
    let input = if form.has_list(ListField::Branch) {
        list_input(dialog, ListField::Branch, focus, cx)
    } else {
        input_box(&Control::Branch, &dialog.branch_input, focus, cx)
    };
    let random = form.use_worktree().then(|| {
        spawn_button(&Control::Random, OUTLINED, "Random", true, focus, cx)
            .tooltip(tooltip(RANDOM_TIP))
    });
    let pending =
        (form.use_worktree() && form.suggestion_pending(now)).then(|| muted(SUGGESTING, &t));
    field(
        crate::fonts::ui_text_size(cx),
        crate::fonts::ui_weight_shift(cx),
        label,
        &t,
    )
    .child(div().flex().gap(px(8.0)).child(input).children(random))
    .children(pending)
    .into_any_element()
}

fn base_field(dialog: &SpawnDialog, focus: &Control, cx: &mut Context<RootView>) -> AnyElement {
    let t = theme(cx);
    let form = &dialog.form;
    let input = list_input(dialog, ListField::Base, focus, cx);
    let failed = (form.fetch_failed() && !form.is_workspace()).then(|| muted(FETCH_FAILED, &t));
    field(
        crate::fonts::ui_text_size(cx),
        crate::fonts::ui_weight_shift(cx),
        "Base branch (optional)",
        &t,
    )
    .child(input)
    .children(failed)
    .into_any_element()
}

/// What follows the base field: a repo's staleness warning, a workspace's
/// Preview button and table, and the collision notice with its choice.
fn preview_rows(form: &SpawnForm, focus: &Control, cx: &mut Context<RootView>) -> Vec<AnyElement> {
    let t = theme(cx);
    let mut rows = Vec::new();
    if form.is_workspace() {
        let enabled = form.preview_enabled();
        let button = spawn_button(&Control::Preview, OUTLINED, "Preview", enabled, focus, cx);
        rows.push(div().flex().child(button).into_any_element());
        if !form.preview_members().is_empty() {
            rows.push(preview_table(crate::fonts::ui_weight_shift(cx), form, &t));
        }
    } else if let Some(text) = form.preview_members().first().and_then(staleness_notice) {
        rows.push(
            warning_box(&t)
                .debug_selector(|| "spawn-base-stale".to_owned())
                .child(text)
                .into_any_element(),
        );
    }
    if let Some(notice) = form.collision().and_then(collision_notice) {
        rows.push(collision_box(&notice, form.reuse_choice(), focus, cx));
    }
    rows
}

/// A box outlined in the warning colour.
fn warning_box(t: &UiTheme) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .px(px(11.0))
        .py(px(9.0))
        .rounded(px(NOTICE_RADIUS))
        .border_1()
        .border_color(gpui::rgb(t.warning))
}

/// The collision notice: what is already there, and Reuse or Recreate.
fn collision_box(
    notice: &CollisionNotice,
    chosen: ReuseChoice,
    focus: &Control,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let t = theme(cx);
    let recreate_note = if notice.danger {
        div()
            .debug_selector(|| "spawn-collision-danger".to_owned())
            .text_color(gpui::rgb(t.danger))
            .child(notice.recreate_note)
    } else {
        muted(notice.recreate_note, &t)
    };
    let choices = [
        (
            ReuseChoice::Reuse,
            notice.reuse_label,
            muted(REUSE_NOTE, &t),
        ),
        (ReuseChoice::Recreate, RECREATE_LABEL, recreate_note),
    ]
    .into_iter()
    .map(|(choice, label, note)| {
        let control = Control::Reuse(choice);
        let focused = *focus == control;
        let text = div()
            .flex()
            .flex_wrap()
            .gap(px(6.0))
            .child(label)
            .child(note);
        let row = radio_row(
            &control.selector(),
            choice == chosen,
            focused,
            text,
            &theme(cx),
        )
        .on_click(press_control(&control, cx));
        div()
            .flex()
            .child(reveal_when_focused(row, &control, focused, cx))
            .into_any_element()
    });
    warning_box(&t)
        .debug_selector(|| "spawn-collision".to_owned())
        .child(notice.headline.clone())
        .children(choices)
        .into_any_element()
}

/// One cell of the workspace preview table.
fn table_cell(column: usize, t: &UiTheme) -> Div {
    let cell = div().min_w(px(0.0));
    match column {
        0 => cell.w(px(90.0)),
        1 => cell.w(px(110.0)),
        2 => cell.flex_1().flex().flex_wrap().gap(px(4.0)),
        _ => cell.w(px(130.0)).text_color(gpui::rgb(t.muted)),
    }
}

/// Each member's repo, branch, what the spawn does there and its path.
fn preview_table(shift: i8, form: &SpawnForm, t: &UiTheme) -> AnyElement {
    let header = div()
        .flex()
        .gap(px(6.0))
        .font_weight(crate::fonts::ui_weight_at(shift, FontWeight::SEMIBOLD))
        .children(
            PREVIEW_HEADERS
                .iter()
                .enumerate()
                .map(|(column, title)| table_cell(column, t).child(*title)),
        );
    let (branch, default_base) = (form.branch().trim(), form.default_base());
    let rows = form.preview_members().iter().map(|member| {
        let row = member_row(member, branch, &default_base);
        let selector = format!("spawn-preview-row-{}", member.repo_id);
        let badges = row.badges.into_iter().map(|(text, tone)| {
            let color = match tone {
                Tone::Ok => t.spawn_badge_ok,
                Tone::Warn => t.warning,
            };
            div()
                .px(px(4.0))
                .rounded(px(3.0))
                .border_1()
                .border_color(gpui::rgb(color))
                .text_color(gpui::rgb(color))
                .child(text)
        });
        div()
            .debug_selector(move || selector)
            .flex()
            .gap(px(6.0))
            .child(table_cell(0, t).child(row.repo))
            .child(table_cell(1, t).child(row.branch))
            .child(table_cell(2, t).children(badges))
            .child(table_cell(3, t).child(row.path))
    });
    div()
        .debug_selector(|| "spawn-preview-table".to_owned())
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(header)
        .children(rows)
        .into_any_element()
}

/// The Advanced toggle, and while open the agent's Advanced fields and the
/// env rows.
fn advanced_section(dialog: &SpawnDialog, focus: &Control, cx: &mut Context<RootView>) -> Div {
    let t = theme(cx);
    let form = &dialog.form;
    let open = form.advanced_open();
    let control = Control::AdvancedToggle;
    let focused = *focus == control;
    let chevron = if open {
        SIDEBAR_CHEVRON_DOWN_ICON
    } else {
        SIDEBAR_CHEVRON_RIGHT_ICON
    };
    let toggle = div()
        .id("spawn-advanced")
        .debug_selector(|| "spawn-advanced".to_owned())
        .flex()
        .items_center()
        .gap(px(6.0))
        .px(px(2.0))
        .rounded(px(CHECK_RADIUS))
        .border_1()
        .border_color(gpui::rgba(t.transparent))
        .text_size(crate::fonts::ui_px(cx, SEGMENT_TEXT_SIZE))
        .text_color(gpui::rgb(t.text_2))
        .cursor_pointer()
        .hover(move |style| style.text_color(gpui::rgb(t.text)))
        .on_click(press_control(&control, cx))
        .child(
            svg()
                .path(chevron)
                .flex_none()
                .size(px(12.0))
                .text_color(gpui::rgb(t.text_2)),
        )
        .child("Advanced")
        .when(focused, |toggle| {
            toggle.child(focus_ring(CHECK_RADIUS, &theme(cx)))
        });
    let section = div().flex().flex_col().gap(px(14.0)).child(
        div()
            .flex()
            .child(reveal_when_focused(toggle, &control, focused, cx)),
    );
    if !open {
        return section;
    }
    section
        .children(agent_options_view::rows(
            form,
            &dialog.option_inputs,
            SpawnPlacement::Advanced,
            focus,
            cx,
        ))
        .child(env_field(dialog, focus, cx))
}

/// The env rows, each with its problem, and `+ Add env var`.
fn env_field(dialog: &SpawnDialog, focus: &Control, cx: &mut Context<RootView>) -> Div {
    let t = theme(cx);
    let rows: Vec<AnyElement> = dialog
        .env_inputs
        .iter()
        .enumerate()
        .map(|(index, inputs)| env_row(dialog, index, inputs, focus, cx))
        .collect();
    let empty = rows.is_empty().then(|| muted(NO_ENV, &t));
    let dropped = dialog.form.env_section_note().map(|text| muted(text, &t));
    let add = spawn_button(&Control::EnvAdd, OUTLINED, "+ Add env var", true, focus, cx);
    field(
        crate::fonts::ui_text_size(cx),
        crate::fonts::ui_weight_shift(cx),
        "Extra environment variables",
        &t,
    )
    .children(dropped)
    .children(empty)
    .children(rows)
    .child(div().flex().child(add))
}

fn env_row(
    dialog: &SpawnDialog,
    index: usize,
    inputs: &EnvInputs,
    focus: &Control,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let t = theme(cx);
    let form = &dialog.form;
    let problem = form.env_problem(index);
    let key = input_box(&Control::EnvKey(index), &inputs.key, focus, cx)
        .when(problem.is_some(), |key| {
            key.border_color(gpui::rgb(t.warning))
        });
    let remove = spawn_button(&Control::EnvRemove(index), OUTLINED, "✕", true, focus, cx)
        .tooltip(tooltip(REMOVE_ENV_TIP));
    let message = problem.map(|problem| {
        div()
            .debug_selector(move || format!("spawn-env-problem-{index}"))
            .text_color(gpui::rgb(t.warning))
            .child(problem.message())
    });
    let note = form.env_row_note(index).map(|note| {
        let color = if note.is_warning() {
            t.warning
        } else {
            t.muted
        };
        div()
            .debug_selector(move || format!("spawn-env-note-{index}"))
            .text_color(gpui::rgb(color))
            .child(note.text())
    });
    div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .child(
            div()
                .flex()
                .gap(px(8.0))
                .child(key)
                .children(env_value_cells(dialog, index, focus, cx))
                .child(remove),
        )
        .children(message)
        .children(note)
        .into_any_element()
}

/// Env row `index`'s value part: a saved secret's read-only text and its
/// Replace button; else the value field, then the Show / Hide button and
/// the Secret toggle where they show.
fn env_value_cells(
    dialog: &SpawnDialog,
    index: usize,
    focus: &Control,
    cx: &mut Context<RootView>,
) -> Vec<AnyElement> {
    let t = theme(cx);
    let form = &dialog.form;
    let saved = form
        .env_rows()
        .get(index)
        .is_some_and(EnvRow::is_saved_secret);
    if saved {
        let text = field_frame(false, &theme(cx))
            .debug_selector(move || format!("spawn-env-saved-{index}"))
            .flex_1()
            .min_w(px(0.0))
            .text_color(gpui::rgb(t.muted))
            .child(SAVED_SECRET);
        let replace = spawn_button(
            &Control::EnvReplace(index),
            OUTLINED,
            REPLACE_SECRET,
            true,
            focus,
            cx,
        );
        return vec![text.into_any_element(), replace.into_any_element()];
    }
    let mut cells =
        vec![list_input(dialog, ListField::EnvValue(index), focus, cx).into_any_element()];
    if form.env_reveal_shown(index) {
        let label = if form.env_revealed(index) {
            HIDE_SECRET
        } else {
            SHOW_SECRET
        };
        let reveal = spawn_button(&Control::EnvReveal(index), OUTLINED, label, true, focus, cx);
        cells.push(reveal.into_any_element());
    }
    if form.env_secret_shown(index) {
        let checked = form.env_rows().get(index).is_some_and(|row| row.secret);
        let label = div().child(SECRET_TOGGLE);
        let toggle = checkbox(&Control::EnvSecret(index), checked, focus, label, cx);
        cells.push(
            div()
                .flex()
                .flex_none()
                .items_center()
                .child(toggle)
                .into_any_element(),
        );
    }
    cells
}

/// Cancel and Spawn, the footer's large buttons.
fn footer(form: &SpawnForm, focus: &Control, cx: &mut Context<RootView>) -> Div {
    let t = theme(cx);
    let cancel = (ButtonKind::Outlined, ButtonSize::Large);
    let submit = (ButtonKind::Primary, ButtonSize::Large);
    dialog_footer(&t)
        .child(spawn_button(
            &Control::Cancel,
            cancel,
            "Cancel",
            true,
            focus,
            cx,
        ))
        .child(spawn_button(
            &Control::Submit,
            submit,
            "Spawn",
            form.can_submit(),
            focus,
            cx,
        ))
}

#[cfg(test)]
mod tests {
    use gpui::px;

    use super::reveal_offset;

    #[test]
    fn visible_control_keeps_the_offset() {
        let offset = reveal_offset(px(100.0)..px(300.0), px(150.0)..px(200.0), px(-40.0));
        assert_eq!(offset, px(-40.0));
    }

    #[test]
    fn control_below_scrolls_to_its_bottom() {
        let offset = reveal_offset(px(100.0)..px(300.0), px(280.0)..px(330.0), px(-40.0));
        assert_eq!(offset, px(-70.0), "its bottom meets the viewport's");
    }

    #[test]
    fn control_above_scrolls_to_its_top() {
        let offset = reveal_offset(px(100.0)..px(300.0), px(80.0)..px(120.0), px(-40.0));
        assert_eq!(offset, px(-20.0), "its top meets the viewport's");
    }

    #[test]
    fn control_taller_than_viewport_aligns_top() {
        let below = reveal_offset(px(100.0)..px(300.0), px(250.0)..px(500.0), px(0.0));
        assert_eq!(below, px(-150.0), "from below, its top shows");
        let above = reveal_offset(px(100.0)..px(300.0), px(50.0)..px(320.0), px(-60.0));
        assert_eq!(above, px(-10.0), "from above too");
    }
}
