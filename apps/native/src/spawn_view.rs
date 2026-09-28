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
};
use protocol::{Agent, DaemonMessage};

use crate::combobox::{ComboRow, list_placement};
use crate::session_menu::{backdrop, dialog_button};
use crate::spawn_form::{
    APPROVAL_CHOICES, CODEX_SANDBOX_CHOICES, CURSOR_SANDBOX_CHOICES, Control, EnvRow, FormInputs,
    ListField, MODEL_ALIASES, OpenChoice, Outcome, RunMode, Runtime, ShareButton, SpawnForm,
    TabChoices, Target, WorktreeMode, approval_label, codex_sandbox_label, cursor_sandbox_label,
};
use crate::spawn_preview::{
    CollisionNotice, RECREATE_LABEL, REUSE_NOTE, ReuseChoice, Tone, collision_notice, member_row,
    staleness_notice,
};
use crate::spawns::PaneAim;
use crate::text_input::{NavKey, TextChanged, TextInput, TextInputEvent};
use crate::{
    BORDER, DANGER, HOVER_BG, MUTED, PANEL_BG, RootView, TEXT, UI_TEXT_SIZE, WARNING, tooltip,
};

const DIALOG_WIDTH: f32 = 520.0;
/// The dialog's greatest height, as a share of the window's; the body
/// scrolls past it.
const PANEL_MAX_HEIGHT: f32 = 0.9;
const SELECTED_BG: u32 = 0x0037_3a44;
const DIALOG_TITLE: &str = "Spawn session";
const SHARE_TITLE: &str = "Share this worktree?";
const SHARE_BODY: &str = "A session is already running in the worktree you picked. Both agents will see each other's uncommitted edits, and concurrent writes to the same file will overwrite one another.";
const RANDOM_TIP: &str = "Generate a random worktree branch name";
const SUGGESTING: &str = "Picking a name no existing branch uses…";
const FETCH_FAILED: &str = "Couldn't reach the remote — comparing against the last fetched refs.";
const CURRENT_TAB_DISABLED: &str = "The current tab cannot host terminal panes";
const HEADLESS_DISABLED: &str = "headless mode is not yet supported for cursor";
const PROMPT_ROWS: (usize, usize) = (3, 8);
const MODEL_PLACEHOLDER: &str = "CLI default";
pub(crate) const CLAUDE_LOCKED: &str = "Ignored while trusted launch is on. Claude will run without --permission-mode. The chosen value is preserved for when you toggle trusted launch off.";
pub(crate) const CODEX_LOCKED: &str = "Ignored while trusted launch is on. Codex will run with --yolo, which overrides sandbox mode. The chosen value is preserved for when you toggle trusted launch off.";
const CURSOR_LOCKED: &str = "Ignored while trusted launch is on. Cursor will run with --yolo, which overrides sandbox mode. The chosen value is preserved for when you toggle trusted launch off.";
const CURSOR_PLAN_LABEL: &str = "Plan mode (read-only / planning)";
const CURSOR_PLAN_TIP: &str = "Start cursor in --plan mode (read-only / planning)";
const NO_ENV: &str = "No extra env vars.";
const REMOVE_ENV_TIP: &str = "Remove env var";
/// The tallest a branch list grows before it scrolls.
const LIST_MAX_HEIGHT: f32 = 200.0;
const CURRENT_TAG: &str = "current";
const PREVIEW_HEADERS: [&str; 4] = ["Repo", "Branch", "Action", "Path"];
/// An "ok" badge of the workspace preview table.
const BADGE_OK: u32 = 0x004e_c9b0;

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
}

/// The open spawn dialog: its form and its text fields.
pub(crate) struct SpawnDialog {
    form: SpawnForm,
    /// The pane it was opened from, if any.
    aim: Option<PaneAim>,
    branch_input: Entity<TextInput>,
    base_input: Entity<TextInput>,
    prompt_input: Entity<TextInput>,
    model_input: Entity<TextInput>,
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
            Control::Prompt => Some(&self.prompt_input),
            Control::Model => Some(&self.model_input),
            Control::EnvKey(index) => self.env_inputs.get(*index).map(|row| &row.key),
            Control::EnvValue(index) => self.env_inputs.get(*index).map(|row| &row.value),
            _ => None,
        }
    }

    fn inputs(&self) -> impl Iterator<Item = &Entity<TextInput>> {
        [
            &self.branch_input,
            &self.base_input,
            &self.prompt_input,
            &self.model_input,
        ]
        .into_iter()
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

/// Which text field an event came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Branch,
    Base,
    Prompt,
    Model,
    EnvKey(usize),
    EnvValue(usize),
}

impl Field {
    fn control(self) -> Control {
        match self {
            Self::Branch => Control::Branch,
            Self::Base => Control::Base,
            Self::Prompt => Control::Prompt,
            Self::Model => Control::Model,
            Self::EnvKey(index) => Control::EnvKey(index),
            Self::EnvValue(index) => Control::EnvValue(index),
        }
    }
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

    /// The selector of the dialog's focused control, or of the focused
    /// button of its "Share this worktree?" confirm.
    #[must_use]
    pub fn spawn_dialog_focus(&self) -> Option<String> {
        let form = &self.spawn_dialog.as_ref()?.form;
        Some(match form.share_confirm() {
            Some(button) => button.selector().to_owned(),
            None => form.focused().selector(),
        })
    }

    /// Whether the "Share this worktree?" confirm is open.
    #[must_use]
    pub fn spawn_share_confirm_open(&self) -> bool {
        self.spawn_dialog
            .as_ref()
            .is_some_and(|dialog| dialog.form.share_confirm().is_some())
    }

    /// The selectors of the dialog's chosen options and ticked boxes.
    #[must_use]
    pub fn spawn_dialog_selected(&self) -> Vec<String> {
        let Some(dialog) = &self.spawn_dialog else {
            return Vec::new();
        };
        let form = &dialog.form;
        let mut chosen = vec![
            Control::Target(form.target().clone()),
            Control::Runtime(form.runtime()),
            Control::OpenIn(form.open_in().clone()),
        ];
        chosen.extend(form.trusted().then_some(Control::Trusted));
        chosen.extend(form.use_worktree().then_some(Control::UseWorktree));
        chosen.extend(form.use_worktree().then_some(Control::Mode(form.mode())));
        chosen.extend(
            form.existing_selected()
                .map(|key| Control::Existing(key.to_owned())),
        );
        chosen.extend(
            form.run_mode_shown()
                .then(|| Control::RunMode(form.run_mode())),
        );
        chosen.extend(agent_options_chosen(form));
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
        let form = &self.spawn_dialog.as_ref()?.form;
        if form.is_workspace() {
            return None;
        }
        form.preview_members().first().and_then(staleness_notice)
    }

    /// The collision notice while it shows: its headline, then each choice
    /// with its note.
    #[must_use]
    pub fn spawn_collision(&self) -> Option<Vec<String>> {
        let form = &self.spawn_dialog.as_ref()?.form;
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
        self.spawn_dialog
            .as_ref()
            .is_some_and(|dialog| dialog.form.preview_enabled())
    }

    /// The workspace preview table's rows: the repo, the branch, the action
    /// badges joined by " · ", and the path.
    #[must_use]
    pub fn spawn_preview_rows(&self) -> Vec<Vec<String>> {
        let Some(dialog) = &self.spawn_dialog else {
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

    /// The text of the headless prompt field, while the dialog is open.
    #[must_use]
    pub fn spawn_dialog_prompt(&self, cx: &gpui::App) -> Option<String> {
        let dialog = self.spawn_dialog.as_ref()?;
        Some(dialog.prompt_input.read(cx).text().to_owned())
    }

    /// The text of the model field, while the dialog is open.
    #[must_use]
    pub fn spawn_dialog_model(&self, cx: &gpui::App) -> Option<String> {
        let dialog = self.spawn_dialog.as_ref()?;
        Some(dialog.model_input.read(cx).text().to_owned())
    }

    /// The text of the branch field, while the dialog is open.
    #[must_use]
    pub fn spawn_dialog_branch(&self, cx: &gpui::App) -> Option<String> {
        let dialog = self.spawn_dialog.as_ref()?;
        Some(dialog.branch_input.read(cx).text().to_owned())
    }

    /// The text of the base branch field, while the dialog is open.
    #[must_use]
    pub fn spawn_dialog_base(&self, cx: &gpui::App) -> Option<String> {
        let dialog = self.spawn_dialog.as_ref()?;
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

    fn spawn_list_rows(&self, field: ListField) -> Option<Vec<String>> {
        let form = &self.spawn_dialog.as_ref()?.form;
        form.list_shown(field)
            .then(|| form.list_rows(field).iter().map(ComboRow::label).collect())
    }

    /// Opens the dialog from `entry`, unless there is no repo, the
    /// connection is down or another dialog or menu is open. It starts on
    /// the preselected session's repo or workspace, else the focused one's.
    pub(crate) fn open_spawn_dialog(
        &mut self,
        entry: SpawnEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let blocked = self.spawn_dialog.is_some()
            || self.exit.is_some()
            || self.shell_dialog.is_some()
            || self.appearance_editor.is_some()
            || self.delete_dialog.is_some()
            || self.menu.is_some()
            || self.container_menu.is_some()
            || self.shell_menu.is_some()
            || self.sc_picker_open
            || self.changes.file_menu.is_some()
            || self.changes.discard.is_some()
            || self.stash.drop.is_some()
            || self.notices.has_modal()
            || self.conn.overlay().is_some();
        if blocked || !self.has_repos() {
            return;
        }
        let (aim, preselect) = match entry {
            SpawnEntry::Toolbar => (None, None),
            SpawnEntry::Pane { aim, preselect } => (Some(aim), preselect),
        };
        let focused = preselect.or_else(|| self.focused_session());
        let inputs = FormInputs {
            repos: self.sidebar.repos(),
            workspaces: self.sidebar.workspaces(),
            focused: focused.as_deref().and_then(|id| self.sidebar.session(id)),
            tabs: TabChoices::from_tabs(self.tabs.tabs(), self.tabs.active_id()),
            spawn_defaults: self.sidebar.ui_state().spawn,
        };
        let now = (self.now)();
        let Some((form, messages)) = SpawnForm::open(inputs, &mut self.branch_cache, now) else {
            return;
        };
        self.renaming = None;
        self.close_flyout();
        self.close_tab_menu(window, cx);
        let branch_input = cx.new(|cx| TextInput::new(form.branch().to_owned(), "", cx));
        let base_input = cx.new(|cx| TextInput::new(form.base().to_owned(), "", cx));
        let prompt_input = cx.new(|cx| {
            TextInput::multi_line(form.prompt().to_owned(), form.prompt_placeholder(), cx)
                .with_rows(PROMPT_ROWS.0, PROMPT_ROWS.1)
        });
        let model_input =
            cx.new(|cx| TextInput::new(form.model().to_owned(), MODEL_PLACEHOLDER, cx));
        let mut subscriptions = Self::watch_field(&branch_input, Field::Branch, window, cx);
        subscriptions.extend(Self::watch_field(&base_input, Field::Base, window, cx));
        subscriptions.extend(Self::watch_field(&prompt_input, Field::Prompt, window, cx));
        subscriptions.extend(Self::watch_field(&model_input, Field::Model, window, cx));
        self.spawn_dialog = Some(SpawnDialog {
            form,
            aim,
            branch_input,
            base_input,
            prompt_input,
            model_input,
            env_inputs: Vec::new(),
            _subscriptions: subscriptions,
            timer: None,
            preview_timer: None,
            scroll: ScrollHandle::new(),
            reveal: true,
            panel_bounds: None,
            list_anchor: None,
        });
        self.send_all(messages);
        self.after_spawn_change(cx);
        self.apply_spawn_focus(window, cx);
    }

    /// The fields of env row `index`, holding `row`.
    fn env_inputs(
        index: usize,
        row: &EnvRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> EnvInputs {
        let key = cx.new(|cx| TextInput::new(row.key.clone(), "KEY", cx));
        let value = cx.new(|cx| TextInput::new(row.value.clone(), "value", cx));
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
        let Some(dialog) = &self.spawn_dialog else {
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
        if let Some(dialog) = &mut self.spawn_dialog {
            dialog.env_inputs = inputs;
        }
    }

    /// Enter submits (Ctrl+Enter in the prompt), Esc closes, an edit
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
        let edits = cx.subscribe_in(input, window, move |this, input, _: &TextChanged, _, cx| {
            let text = input.read(cx).text().to_owned();
            if let Some(dialog) = &mut this.spawn_dialog {
                let form = &mut dialog.form;
                match field {
                    Field::Branch => form.edit_branch(&text, &mut this.branch_cache),
                    Field::Base => form.edit_base(&text),
                    Field::Prompt => form.edit_prompt(&text),
                    Field::Model => form.edit_model(&text),
                    Field::EnvKey(index) => form.edit_env_key(index, &text),
                    Field::EnvValue(index) => form.edit_env_value(index, &text),
                }
                if let Some(list) = list {
                    form.list_edited(list);
                }
            }
            this.after_spawn_change(cx);
        });
        let handle = input.read(cx).focus_handle(cx);
        let focus = cx.on_focus(&handle, window, move |this, _, cx| {
            if let Some(dialog) = &mut this.spawn_dialog {
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
                    if let Some(dialog) = &mut this.spawn_dialog {
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
        let Some(dialog) = &mut self.spawn_dialog else {
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
        let closed = self
            .spawn_dialog
            .as_mut()
            .is_some_and(|dialog| dialog.form.escape_list(list));
        if closed {
            cx.notify();
        }
        closed
    }

    fn open_spawn_list(&mut self, list: ListField, cx: &mut Context<Self>) {
        if let Some(dialog) = &mut self.spawn_dialog {
            dialog.form.open_list(list);
            cx.notify();
        }
    }

    fn close_spawn_list(&mut self, list: ListField, cx: &mut Context<Self>) {
        if let Some(dialog) = &mut self.spawn_dialog {
            dialog.form.close_list(list);
            cx.notify();
        }
    }

    fn hover_spawn_list(&mut self, list: ListField, index: usize, cx: &mut Context<Self>) {
        if let Some(dialog) = &mut self.spawn_dialog
            && dialog.form.hover_list(list, index)
        {
            cx.notify();
        }
    }

    fn pick_spawn_list(&mut self, list: ListField, index: usize, cx: &mut Context<Self>) {
        let Some(dialog) = &mut self.spawn_dialog else {
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
        let Some(dialog) = &mut self.spawn_dialog else {
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

    fn send_all(&self, messages: Vec<protocol::ClientMessage>) {
        for msg in messages {
            self.send(msg);
        }
    }

    fn submit_spawn_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = &mut self.spawn_dialog else {
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
        let Some(dialog) = &mut self.spawn_dialog else {
            return;
        };
        let outcome = dialog.form.press(control, &mut self.branch_cache, now);
        self.on_spawn_outcome(outcome, window, cx);
    }

    fn answer_share(&mut self, button: ShareButton, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = &mut self.spawn_dialog else {
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
                self.after_spawn_change(cx);
                self.apply_spawn_focus(window, cx);
            }
            Outcome::Close => self.close_spawn_dialog(window, cx),
            Outcome::ConfirmShare => {
                self.spawn_focus.focus(window);
                cx.notify();
            }
            Outcome::Spawn(submission) => {
                if let Some(msg) = submission.default_change {
                    self.send(msg);
                }
                let aim = self.spawn_dialog.as_mut().and_then(|d| d.aim.take());
                let open_in = match aim {
                    Some(aim) => aim.open_in(submission.open_in),
                    None => submission.open_in,
                };
                self.close_spawn_dialog(window, cx);
                self.spawn(submission.request, open_in, cx);
            }
        }
    }

    /// The fields follow the form, a timer waits out a suggestion, and the
    /// preview follows the fields, a timer waiting out its debounce.
    fn after_spawn_change(&mut self, cx: &mut Context<Self>) {
        let now = (self.now)();
        let Some(dialog) = &mut self.spawn_dialog else {
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
        let (prompt, model) = (form.prompt().to_owned(), form.model().to_owned());
        sync_field(&dialog.prompt_input, &prompt, form.prompt_placeholder(), cx);
        sync_field(
            &dialog.model_input,
            &model,
            MODEL_PLACEHOLDER.to_owned(),
            cx,
        );
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
        let msg = self
            .spawn_dialog
            .as_mut()
            .and_then(|dialog| dialog.form.take_due_preview(now));
        if let Some(msg) = msg {
            self.send(msg);
        }
        cx.notify();
    }

    /// The keyboard goes where the form's focus is: a text field, else the
    /// dialog itself. The body then scrolls the focused control into view.
    pub(crate) fn apply_spawn_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = &mut self.spawn_dialog else {
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
        let Some(dialog) = &mut self.spawn_dialog else {
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
        self.spawn_dialog.as_ref().is_some_and(|dialog| {
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
        let Some(form) = self.spawn_dialog.as_ref().map(|dialog| &dialog.form) else {
            return false;
        };
        if let Some(button) = form.share_confirm() {
            self.on_share_key(key, button, window, cx);
            return true;
        }
        let focused = form.focused();
        match key {
            "tab" => {
                if let Some(dialog) = &mut self.spawn_dialog {
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
                if let Some(dialog) = &mut self.spawn_dialog {
                    dialog.form.toggle_share_focus();
                }
                cx.notify();
            }
            _ => {}
        }
    }

    /// A daemon message the dialog follows. Suggestions are cached whether
    /// or not it is open.
    pub(crate) fn on_spawn_dialog_message(
        &mut self,
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
        if let DaemonMessage::BranchNameSuggestion { target, name } = msg
            && let Some(target) = Target::from_suggest(target)
        {
            self.branch_cache.insert(target.clone(), name.clone());
            if let Some(dialog) = &mut self.spawn_dialog {
                dialog.form.on_suggestion(&target, name);
            }
        }
        let Some(dialog) = &mut self.spawn_dialog else {
            return;
        };
        let messages = if registry {
            let (repos, workspaces) = (self.sidebar.repos(), self.sidebar.workspaces());
            match dialog
                .form
                .set_registry(repos, workspaces, &mut self.branch_cache, now)
            {
                Some(messages) => messages,
                None => return self.close_spawn_dialog(window, cx),
            }
        } else {
            match dialog.form.on_message(msg) {
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
        if let Some(dialog) = &mut self.spawn_dialog {
            let tabs = TabChoices::from_tabs(self.tabs.tabs(), self.tabs.active_id());
            dialog.form.set_tabs(tabs);
        }
    }

    /// The dialog over a backdrop that takes every click beneath it, and the
    /// share confirm over it while open.
    pub(crate) fn spawn_dialog_layers(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let Some(dialog) = &self.spawn_dialog else {
            return Vec::new();
        };
        let form = &dialog.form;
        let focus = form.focused();
        let body = div()
            .id("spawn-body")
            .debug_selector(|| "spawn-body".to_owned())
            .flex()
            .flex_col()
            .gap(px(10.0))
            .min_h(px(0.0))
            .overflow_y_scroll()
            .track_scroll(&dialog.scroll)
            .child(target_field(form, &focus, cx))
            .child(runtime_field(form, &focus, cx))
            .child(open_in_field(form, &focus, cx))
            .children(self.worktree_rows(dialog, &focus, cx))
            .children(run_rows(dialog, &focus, cx))
            .children(trusted_rows(form, &focus, cx))
            .child(advanced_section(dialog, &focus, cx));
        let panel = panel("spawn-panel")
            .max_h(relative(PANEL_MAX_HEIGHT))
            .when(form.share_confirm().is_none(), |panel| {
                panel.track_focus(&self.spawn_focus)
            })
            .child(dialog_header(focus == Control::Close, cx))
            .child(body)
            .child(footer(form, &focus, cx))
            .child(measurer(Measured::Panel, cx));
        let mut layers = vec![backdrop("spawn-dialog", panel)];
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

    fn share_layer(&self, focused: ShareButton, cx: &mut Context<Self>) -> AnyElement {
        let buttons: Vec<AnyElement> = [ShareButton::Cancel, ShareButton::Launch]
            .into_iter()
            .map(|button| {
                dialog_button(
                    button.selector(),
                    button.label().to_owned(),
                    false,
                    focused == button,
                )
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.answer_share(button, window, cx);
                }))
                .into_any_element()
            })
            .collect();
        let panel = panel("spawn-share-panel")
            .track_focus(&self.spawn_focus)
            .child(div().font_weight(FontWeight::SEMIBOLD).child(SHARE_TITLE))
            .child(div().child(SHARE_BODY))
            .child(div().flex().justify_end().gap(px(6.0)).children(buttons));
        backdrop("spawn-share-worktree-confirm", panel)
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

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
        })
}

/// A dialog card.
fn panel(id: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_col()
        .gap(px(10.0))
        .w(px(DIALOG_WIDTH))
        .p(px(14.0))
        .bg(gpui::rgb(PANEL_BG))
        .border_1()
        .border_color(gpui::rgb(BORDER))
        .rounded(px(6.0))
        .text_size(px(UI_TEXT_SIZE))
        .text_color(gpui::rgb(TEXT))
}

fn dialog_header(close_focused: bool, cx: &mut Context<RootView>) -> Div {
    let close = dialog_button("spawn-close", "✕".to_owned(), false, close_focused).on_click(
        cx.listener(|this, _: &ClickEvent, window, cx| this.close_spawn_dialog(window, cx)),
    );
    div()
        .flex()
        .items_center()
        .justify_between()
        .child(div().font_weight(FontWeight::SEMIBOLD).child(DIALOG_TITLE))
        .child(close)
}

/// A labelled field.
pub(crate) fn field(label: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(div().text_color(gpui::rgb(MUTED)).child(label.into()))
}

/// A row of choices that wraps.
pub(crate) fn segmented(buttons: Vec<AnyElement>) -> Div {
    div().flex().flex_wrap().gap(px(4.0)).children(buttons)
}

fn muted(text: impl Into<SharedString>) -> Div {
    div().text_color(gpui::rgb(MUTED)).child(text.into())
}

/// A choice or a plain button: filled when chosen, outlined when focused,
/// dimmed and inert when disabled. The Settings modal's Spawn defaults rows
/// are built from it too, so both places look the same.
pub(crate) fn choice_button(
    name: String,
    label: impl IntoElement,
    look: Look,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> Stateful<Div> {
    div()
        .id(ElementId::Name(SharedString::from(name.clone())))
        .debug_selector(move || name)
        .flex()
        .items_center()
        .gap(px(6.0))
        .px(px(8.0))
        .py(px(3.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(gpui::rgb(if look.focused { TEXT } else { BORDER }))
        .when(look.selected, |button| button.bg(gpui::rgb(SELECTED_BG)))
        .when(look.enabled, |button| {
            button
                .cursor_pointer()
                .hover(|style| style.bg(gpui::rgb(HOVER_BG)))
                .on_click(on_click)
        })
        .when(!look.enabled, |button| button.opacity(0.5))
        .child(label)
}

/// A dialog control's choice button, revealed into view when focused.
fn option_button(
    control: &Control,
    label: impl IntoElement,
    look: Look,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    let name = control.selector();
    let pressed = control.clone();
    let button = choice_button(
        name,
        label,
        look,
        cx.listener(move |this, _: &ClickEvent, window, cx| {
            this.press_spawn_control(&pressed, window, cx);
        }),
    );
    if look.focused && in_body(control) {
        button.child(reveal_marker(cx))
    } else {
        button
    }
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

fn target_field(form: &SpawnForm, focus: &Control, cx: &mut Context<RootView>) -> Div {
    let buttons = form
        .targets()
        .into_iter()
        .map(|target| {
            let label = form.target_label(&target);
            let selected = &target == form.target();
            choice(&Control::Target(target), label, selected, focus, cx)
        })
        .collect();
    field("Target").child(segmented(buttons))
}

fn runtime_field(form: &SpawnForm, focus: &Control, cx: &mut Context<RootView>) -> Div {
    let buttons = Runtime::ALL
        .into_iter()
        .map(|runtime| {
            let selected = runtime == form.runtime();
            choice(
                &Control::Runtime(runtime),
                runtime.label(),
                selected,
                focus,
                cx,
            )
        })
        .collect();
    field("Runtime").child(segmented(buttons))
}

fn open_in_field(form: &SpawnForm, focus: &Control, cx: &mut Context<RootView>) -> Div {
    let chosen = form.open_in();
    let tabs = form.tabs();
    let current = Control::OpenIn(OpenChoice::CurrentTab);
    let current_label = div()
        .flex()
        .gap(px(6.0))
        .child("Current tab")
        .children(tabs.current.as_ref().map(|tab| muted(tab.name.clone())));
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
    field("Open in").child(segmented(buttons))
}

/// A checkbox: its box, its label and a muted note.
fn checkbox(
    control: &Control,
    checked: bool,
    focus: &Control,
    label: Div,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let look = Look {
        selected: false,
        focused: focus == control,
        enabled: true,
    };
    let mark = if checked { "☑" } else { "☐" };
    let row = div().flex().gap(px(6.0)).child(mark).child(label);
    div()
        .flex()
        .child(option_button(control, row, look, cx))
        .into_any_element()
}

fn trusted_rows(form: &SpawnForm, focus: &Control, cx: &mut Context<RootView>) -> Vec<AnyElement> {
    if !form.trusted_shown() {
        return Vec::new();
    }
    let flag = form.trusted_flag();
    let label = div()
        .flex()
        .gap(px(4.0))
        .child("Trusted launch")
        .child(muted(format!("({flag})")));
    let mut rows = vec![checkbox(
        &Control::Trusted,
        form.trusted(),
        focus,
        label,
        cx,
    )];
    if form.trusted() {
        let warning = div()
            .debug_selector(|| "spawn-trusted-launch-warning".to_owned())
            .flex()
            .flex_col()
            .gap(px(2.0))
            .p(px(8.0))
            .rounded(px(4.0))
            .border_1()
            .border_color(gpui::rgb(WARNING))
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Trusted launch"),
            )
            .child(format!("{} Uses {flag}.", form.trusted_detail()));
        rows.push(warning.into_any_element());
    }
    rows
}

fn worktree_checkbox(form: &SpawnForm, focus: &Control, cx: &mut Context<RootView>) -> AnyElement {
    let (text, note) = if form.is_workspace() {
        (
            "Create worktrees",
            "(unchecked: check out the branch in each member's main directory)",
        )
    } else {
        (
            "Create a worktree",
            "(unchecked: run claude in the repo's main directory)",
        )
    };
    let label = div()
        .flex()
        .flex_wrap()
        .gap(px(4.0))
        .child(text)
        .child(muted(note));
    checkbox(&Control::UseWorktree, form.use_worktree(), focus, label, cx)
}

fn mode_field(form: &SpawnForm, focus: &Control, cx: &mut Context<RootView>) -> AnyElement {
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
    field(label).child(segmented(buttons)).into_any_element()
}

fn existing_field(form: &SpawnForm, focus: &Control, cx: &mut Context<RootView>) -> AnyElement {
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
        .then(|| muted(form.existing_placeholder()));
    let note = form.existing_note().map(muted);
    let warning = form
        .existing_warning()
        .map(|text| div().text_color(gpui::rgb(WARNING)).child(text));
    field(label)
        .children(placeholder)
        .child(div().flex().flex_col().gap(px(4.0)).children(options))
        .children(note)
        .children(warning)
        .into_any_element()
}

/// A text field in a box, tagged with its control's selector.
fn input_box(
    control: &Control,
    input: &Entity<TextInput>,
    focus: &Control,
    cx: &mut Context<RootView>,
) -> Div {
    let selector = control.selector();
    let focused = focus == control;
    div()
        .debug_selector(move || selector)
        .flex()
        .flex_1()
        .px(px(6.0))
        .py(px(3.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(gpui::rgb(if focused { TEXT } else { BORDER }))
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
        .py(px(2.0))
        .bg(gpui::rgb(PANEL_BG))
        .border_1()
        .border_color(gpui::rgb(BORDER))
        .rounded(px(4.0))
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
    let name = format!("{prefix}-option-{index}");
    let tag_name = format!("{name}-current");
    let tag = current.then(|| muted(CURRENT_TAG).debug_selector(move || tag_name));
    div()
        .id(ElementId::Name(SharedString::from(name.clone())))
        .debug_selector(|| name)
        .flex()
        .justify_between()
        .gap(px(6.0))
        .px(px(8.0))
        .py(px(3.0))
        .cursor_pointer()
        .when(highlighted, |row| row.bg(gpui::rgb(SELECTED_BG)))
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
        .child(row.label())
        .children(tag)
        .into_any_element()
}

fn branch_field(
    dialog: &SpawnDialog,
    focus: &Control,
    now: Instant,
    cx: &mut Context<RootView>,
) -> AnyElement {
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
        let look = Look {
            selected: false,
            focused: *focus == Control::Random,
            enabled: true,
        };
        option_button(&Control::Random, "Random", look, cx).tooltip(tooltip(RANDOM_TIP))
    });
    let pending = (form.use_worktree() && form.suggestion_pending(now)).then(|| muted(SUGGESTING));
    field(label)
        .child(div().flex().gap(px(6.0)).child(input).children(random))
        .children(pending)
        .into_any_element()
}

fn base_field(dialog: &SpawnDialog, focus: &Control, cx: &mut Context<RootView>) -> AnyElement {
    let form = &dialog.form;
    let input = list_input(dialog, ListField::Base, focus, cx);
    let failed = (form.fetch_failed() && !form.is_workspace()).then(|| muted(FETCH_FAILED));
    field("Base branch (optional)")
        .child(input)
        .children(failed)
        .into_any_element()
}

/// What follows the base field: a repo's staleness warning, a workspace's
/// Preview button and table, and the collision notice with its choice.
fn preview_rows(form: &SpawnForm, focus: &Control, cx: &mut Context<RootView>) -> Vec<AnyElement> {
    let mut rows = Vec::new();
    if form.is_workspace() {
        let look = Look {
            selected: false,
            focused: *focus == Control::Preview,
            enabled: form.preview_enabled(),
        };
        let button = option_button(&Control::Preview, "Preview", look, cx);
        rows.push(div().flex().child(button).into_any_element());
        if !form.preview_members().is_empty() {
            rows.push(preview_table(form));
        }
    } else if let Some(text) = form.preview_members().first().and_then(staleness_notice) {
        rows.push(
            warning_box()
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
fn warning_box() -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .p(px(8.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(gpui::rgb(WARNING))
}

/// The collision notice: what is already there, and Reuse or Recreate.
fn collision_box(
    notice: &CollisionNotice,
    chosen: ReuseChoice,
    focus: &Control,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let recreate_note = if notice.danger {
        div()
            .debug_selector(|| "spawn-collision-danger".to_owned())
            .text_color(gpui::rgb(DANGER))
            .child(notice.recreate_note)
    } else {
        muted(notice.recreate_note)
    };
    let choices = [
        (ReuseChoice::Reuse, notice.reuse_label, muted(REUSE_NOTE)),
        (ReuseChoice::Recreate, RECREATE_LABEL, recreate_note),
    ]
    .into_iter()
    .map(|(choice, label, note)| {
        let control = Control::Reuse(choice);
        let look = Look {
            selected: false,
            focused: *focus == control,
            enabled: true,
        };
        let mark = if choice == chosen { "◉" } else { "○" };
        let row = div()
            .flex()
            .flex_wrap()
            .gap(px(6.0))
            .child(mark)
            .child(label)
            .child(note);
        div()
            .flex()
            .child(option_button(&control, row, look, cx))
            .into_any_element()
    });
    warning_box()
        .debug_selector(|| "spawn-collision".to_owned())
        .child(notice.headline.clone())
        .children(choices)
        .into_any_element()
}

/// One cell of the workspace preview table.
fn table_cell(column: usize) -> Div {
    let cell = div().min_w(px(0.0));
    match column {
        0 => cell.w(px(90.0)),
        1 => cell.w(px(110.0)),
        2 => cell.flex_1().flex().flex_wrap().gap(px(4.0)),
        _ => cell.w(px(130.0)).text_color(gpui::rgb(MUTED)),
    }
}

/// Each member's repo, branch, what the spawn does there and its path.
fn preview_table(form: &SpawnForm) -> AnyElement {
    let header = div()
        .flex()
        .gap(px(6.0))
        .font_weight(FontWeight::SEMIBOLD)
        .children(
            PREVIEW_HEADERS
                .iter()
                .enumerate()
                .map(|(column, title)| table_cell(column).child(*title)),
        );
    let (branch, default_base) = (form.branch().trim(), form.default_base());
    let rows = form.preview_members().iter().map(|member| {
        let row = member_row(member, branch, &default_base);
        let selector = format!("spawn-preview-row-{}", member.repo_id);
        let badges = row.badges.into_iter().map(|(text, tone)| {
            let color = match tone {
                Tone::Ok => BADGE_OK,
                Tone::Warn => WARNING,
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
            .child(table_cell(0).child(row.repo))
            .child(table_cell(1).child(row.branch))
            .child(table_cell(2).children(badges))
            .child(table_cell(3).child(row.path))
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

/// Run mode (not for a plain shell), and the prompt while headless.
fn run_rows(dialog: &SpawnDialog, focus: &Control, cx: &mut Context<RootView>) -> Vec<AnyElement> {
    let form = &dialog.form;
    let mut rows = Vec::new();
    if form.run_mode_shown() {
        let buttons = RunMode::ALL
            .into_iter()
            .map(|mode| {
                let control = Control::RunMode(mode);
                let enabled = mode == RunMode::Interactive || form.headless_enabled();
                let look = Look {
                    selected: form.run_mode() == mode,
                    focused: *focus == control,
                    enabled,
                };
                option_button(&control, mode.label(), look, cx)
                    .when(!enabled, |button| {
                        button.tooltip(tooltip(HEADLESS_DISABLED))
                    })
                    .into_any_element()
            })
            .collect();
        rows.push(
            field("Run mode")
                .child(segmented(buttons))
                .into_any_element(),
        );
    }
    if form.run_mode() == RunMode::Headless {
        let input = input_box(&Control::Prompt, &dialog.prompt_input, focus, cx);
        rows.push(field("Prompt").child(input).into_any_element());
    }
    rows
}

/// The chosen approval or sandbox option and the plan-mode box, for the
/// runtime on show.
fn agent_options_chosen(form: &SpawnForm) -> Vec<Control> {
    match form.runtime() {
        Runtime::Agent(Agent::Claude) => vec![Control::Approval(form.permission_mode())],
        Runtime::Agent(Agent::Codex) => vec![Control::CodexSandbox(form.codex_sandbox())],
        Runtime::Agent(Agent::Cursor) => {
            let mut chosen = vec![Control::CursorSandbox(form.cursor_sandbox())];
            chosen.extend(form.cursor_plan().then_some(Control::CursorPlan));
            chosen
        }
        Runtime::PlainShell => Vec::new(),
    }
}

/// The Advanced toggle, and while open the model, the agent's options and
/// the env rows.
fn advanced_section(dialog: &SpawnDialog, focus: &Control, cx: &mut Context<RootView>) -> Div {
    let form = &dialog.form;
    let open = form.advanced_open();
    let arrow = if open { "▾" } else { "▸" };
    let look = Look {
        selected: false,
        focused: *focus == Control::AdvancedToggle,
        enabled: true,
    };
    let toggle = option_button(
        &Control::AdvancedToggle,
        format!("{arrow} Advanced"),
        look,
        cx,
    );
    let section = div()
        .flex()
        .flex_col()
        .gap(px(10.0))
        .child(div().flex().child(toggle));
    if !open {
        return section;
    }
    section
        .children(model_field(dialog, focus, cx))
        .children(agent_option_rows(form, focus, cx))
        .child(env_field(dialog, focus, cx))
}

/// The model field, and for claude the alias chips.
fn model_field(
    dialog: &SpawnDialog,
    focus: &Control,
    cx: &mut Context<RootView>,
) -> Option<AnyElement> {
    let form = &dialog.form;
    if !form.model_shown() {
        return None;
    }
    let input = input_box(&Control::Model, &dialog.model_input, focus, cx);
    let chips: Vec<AnyElement> = if form.model_chips_shown() {
        MODEL_ALIASES
            .into_iter()
            .map(|alias| {
                let selected = form.model().trim() == alias;
                choice(&Control::ModelChip(alias), alias, selected, focus, cx)
            })
            .collect()
    } else {
        Vec::new()
    };
    let row = div().flex().gap(px(6.0)).child(input).children(chips);
    Some(field("Model").child(row).into_any_element())
}

/// Claude's approval mode, Codex's sandbox, or Cursor's plan mode and
/// sandbox; nothing for a plain shell.
fn agent_option_rows(
    form: &SpawnForm,
    focus: &Control,
    cx: &mut Context<RootView>,
) -> Vec<AnyElement> {
    let locked = form.trusted();
    let note = |text: &'static str| locked.then_some(text);
    match form.runtime() {
        Runtime::Agent(Agent::Claude) => {
            let chosen = form.permission_mode();
            let choices = APPROVAL_CHOICES.map(|mode| {
                (
                    Control::Approval(mode),
                    approval_label(mode),
                    mode == chosen,
                )
            });
            vec![option_field(
                "Claude approval mode",
                &choices,
                note(CLAUDE_LOCKED),
                focus,
                cx,
            )]
        }
        Runtime::Agent(Agent::Codex) => {
            let chosen = form.codex_sandbox();
            let choices = CODEX_SANDBOX_CHOICES.map(|s| {
                (
                    Control::CodexSandbox(s),
                    codex_sandbox_label(s),
                    s == chosen,
                )
            });
            vec![option_field(
                "Codex sandbox mode",
                &choices,
                note(CODEX_LOCKED),
                focus,
                cx,
            )]
        }
        Runtime::Agent(Agent::Cursor) => {
            let chosen = form.cursor_sandbox();
            let choices = CURSOR_SANDBOX_CHOICES.map(|s| {
                (
                    Control::CursorSandbox(s),
                    cursor_sandbox_label(s),
                    s == chosen,
                )
            });
            vec![
                cursor_plan_checkbox(form, focus, cx),
                option_field("Cursor sandbox", &choices, note(CURSOR_LOCKED), focus, cx),
            ]
        }
        Runtime::PlainShell => Vec::new(),
    }
}

/// A labelled row of choices; with a note it is disabled and the note
/// says why.
fn option_field(
    label: &'static str,
    choices: &[(Control, &'static str, bool)],
    locked_note: Option<&'static str>,
    focus: &Control,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let buttons = choices
        .iter()
        .map(|(control, text, selected)| {
            let look = Look {
                selected: *selected,
                focused: focus == control,
                enabled: locked_note.is_none(),
            };
            option_button(control, *text, look, cx).into_any_element()
        })
        .collect();
    field(label)
        .child(segmented(buttons))
        .children(locked_note.map(muted))
        .into_any_element()
}

fn cursor_plan_checkbox(
    form: &SpawnForm,
    focus: &Control,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let control = Control::CursorPlan;
    let look = Look {
        selected: false,
        focused: *focus == control,
        enabled: true,
    };
    let mark = if form.cursor_plan() { "☑" } else { "☐" };
    let row = div()
        .flex()
        .gap(px(6.0))
        .child(mark)
        .child(CURSOR_PLAN_LABEL);
    div()
        .flex()
        .child(option_button(&control, row, look, cx).tooltip(tooltip(CURSOR_PLAN_TIP)))
        .into_any_element()
}

/// The env rows, each with its problem, and `+ Add env var`.
fn env_field(dialog: &SpawnDialog, focus: &Control, cx: &mut Context<RootView>) -> Div {
    let rows: Vec<AnyElement> = dialog
        .env_inputs
        .iter()
        .enumerate()
        .map(|(index, inputs)| env_row(&dialog.form, index, inputs, focus, cx))
        .collect();
    let empty = rows.is_empty().then(|| muted(NO_ENV));
    let look = Look {
        selected: false,
        focused: *focus == Control::EnvAdd,
        enabled: true,
    };
    let add = option_button(&Control::EnvAdd, "+ Add env var", look, cx);
    field("Extra environment variables")
        .children(empty)
        .children(rows)
        .child(div().flex().child(add))
}

fn env_row(
    form: &SpawnForm,
    index: usize,
    inputs: &EnvInputs,
    focus: &Control,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let problem = form.env_problem(index);
    let key = input_box(&Control::EnvKey(index), &inputs.key, focus, cx)
        .when(problem.is_some(), |key| {
            key.border_color(gpui::rgb(WARNING))
        });
    let value = input_box(&Control::EnvValue(index), &inputs.value, focus, cx);
    let remove_control = Control::EnvRemove(index);
    let look = Look {
        selected: false,
        focused: *focus == remove_control,
        enabled: true,
    };
    let remove = option_button(&remove_control, "✕", look, cx).tooltip(tooltip(REMOVE_ENV_TIP));
    let message = problem.map(|problem| {
        div()
            .debug_selector(move || format!("spawn-env-problem-{index}"))
            .text_color(gpui::rgb(WARNING))
            .child(problem.message())
    });
    div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .child(
            div()
                .flex()
                .gap(px(6.0))
                .child(key)
                .child(value)
                .child(remove),
        )
        .children(message)
        .into_any_element()
}

fn footer(form: &SpawnForm, focus: &Control, cx: &mut Context<RootView>) -> Div {
    let cancel = Look {
        selected: false,
        focused: *focus == Control::Cancel,
        enabled: true,
    };
    let submit = Look {
        selected: true,
        focused: *focus == Control::Submit,
        enabled: form.can_submit(),
    };
    div()
        .flex()
        .justify_end()
        .gap(px(6.0))
        .child(option_button(&Control::Cancel, "Cancel", cancel, cx))
        .child(option_button(&Control::Submit, "Spawn", submit, cx))
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
