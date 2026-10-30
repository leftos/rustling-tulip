//! The preset wizard on screen: the prompt source stage, the variables
//! stage, the preview with its Launch, and Cancel, Back and Next; and the
//! launch jobs' progress toasts once the wizard has closed. Its state and
//! every rule live in [`crate::preset_wizard`] and [`crate::preset_launch`];
//! this view renders it and forwards clicks, edits, picks, Esc and the
//! daemon's answers.

use std::path::PathBuf;

use gpui::{
    AnyElement, App, ClickEvent, Context, Div, Entity, FontWeight, Keystroke, Stateful,
    Subscription, Window, div, prelude::*, px,
};
use protocol::{
    ClientMessage, DaemonMessage, PresetEntry, PresetPromptSource, PresetTarget, PresetVariable,
    PresetVariableKind,
};

use crate::notice_view::modal_panel;
use crate::preset_launch::{
    JobAction, LAUNCH_CAVEAT, MAX_PER_TAB_LABEL, MAX_PER_TAB_PLACEHOLDER, PREVIEW_EMPTY, Preview,
    PreviewRow, SCRIPT_UNRESOLVED, SCRIPTS_LEGEND, SCRIPTS_NOTE, ScriptFailure, ScriptLine,
    max_per_tab_note, tab_header, touches_launches,
};
use crate::preset_wizard::{
    CLEAR_LABEL, FILE_LABEL, FILE_PLACEHOLDER, FOLDER_LABEL, FOLDER_PLACEHOLDER, INLINE_LABEL,
    INLINE_PLACEHOLDER, ISSUES_HINT, ISSUES_LABEL, ISSUES_NOTE, ISSUES_PLACEHOLDER, IssuesFeedback,
    LaunchButton, LaunchStep, NextButton, PICK_LABEL, Pick, PresetWizard, REQUIRED_HINT,
    REQUIRED_NOTE, SOURCE_LEGEND, SourceKind, Stage, shows_required, source_label, takes_text,
    variable_placeholder,
};
use crate::repo_remove::{is_live, stop_message};
use crate::session_menu::{backdrop, dialog_button, dialog_look};
use crate::spawn_view::checkbox_row;
use crate::target::HostKey;
use crate::text_input::{TextChanged, TextInput, TextInputEvent};
use crate::ui_theme::{UiTheme, theme};
use crate::{RootView, tooltip};

/// The open wizard: its model and the entities of its text fields.
pub(crate) struct PresetWizardDialog {
    wizard: PresetWizard,
    inline_input: Entity<TextInput>,
    issues_input: Entity<TextInput>,
    max_per_tab_input: Entity<TextInput>,
    /// The text field of each prompted variable typed as text, by name.
    text_inputs: Vec<(String, Entity<TextInput>)>,
    /// The typed host path field of each path row, by the pick it stands
    /// in for; built only on a remote host, where no picker opens.
    path_inputs: Vec<(Pick, Entity<TextInput>)>,
    _subscriptions: Vec<Subscription>,
}

/// A text field whose edits the model takes.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Field {
    Inline,
    Issues,
    MaxPerTab,
    Variable(String),
    /// A path row's typed host path, which fills what its pick would.
    Path(Pick),
}

impl PresetWizardDialog {
    /// The typed host path field standing in for `pick`, on a remote host.
    fn path_input(&self, pick: &Pick) -> Option<&Entity<TextInput>> {
        self.path_inputs
            .iter()
            .find(|(own, _)| own == pick)
            .map(|(_, input)| input)
    }

    /// The pick whose path row is tagged `selector`.
    fn pick_at(&self, selector: &str) -> Option<&Pick> {
        self.path_inputs
            .iter()
            .map(|(pick, _)| pick)
            .find(|pick| pick_selector(pick) == selector)
    }

    /// The max-panes field takes no edit while the scripts run, as Cancel
    /// and Back take no click.
    fn lock_max_per_tab(&self, cx: &mut Context<RootView>) {
        let running = self.wizard.scripts_running();
        self.max_per_tab_input
            .update(cx, |input, cx| input.set_read_only(running, cx));
    }
}

/// The selector of a variable's field; its Pick… and Clear add `-pick` and
/// `-clear`.
fn variable_selector(name: &str) -> String {
    format!("preset-variable-{name}")
}

/// The selector of `pick`'s path row; its Pick… adds `-pick` and its
/// refusal `-error`.
fn pick_selector(pick: &Pick) -> String {
    match pick {
        Pick::SourceFile => "preset-source-file".to_owned(),
        Pick::SourceFolder => "preset-source-folder".to_owned(),
        Pick::VariableFile(name) | Pick::VariableFolder(name) => variable_selector(name),
    }
}

impl RootView {
    /// Opens the wizard for `entry` launched on `target`, unless the
    /// connection is down.
    pub(crate) fn open_preset_wizard(
        &mut self,
        entry: PresetEntry,
        target: PresetTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.conn().overlay().is_some() {
            return;
        }
        let repo_path = self
            .sidebar
            .repos()
            .iter()
            .find(|repo| repo.id == entry.source_repo_id)
            .map(|repo| repo.path.clone());
        let mut wizard = PresetWizard::new(entry, target, repo_path.as_deref());
        let host = (!self.capabilities().opens_local_files())
            .then(|| self.conn().target().short_name().map(str::to_owned))
            .flatten();
        wizard.set_host(host.clone());
        let inline_input = cx
            .new(|cx| TextInput::multi_line(String::new(), INLINE_PLACEHOLDER, cx).with_rows(4, 8));
        let issues_input = cx.new(|cx| TextInput::new(String::new(), ISSUES_PLACEHOLDER, cx));
        let mut subscriptions = Self::watch_preset_field(&inline_input, Field::Inline, window, cx);
        subscriptions.extend(Self::watch_preset_field(
            &issues_input,
            Field::Issues,
            window,
            cx,
        ));
        let max_per_tab = wizard.max_per_tab_text().to_owned();
        let max_per_tab_input =
            cx.new(|cx| TextInput::new(max_per_tab, MAX_PER_TAB_PLACEHOLDER, cx));
        subscriptions.extend(Self::watch_preset_field(
            &max_per_tab_input,
            Field::MaxPerTab,
            window,
            cx,
        ));
        let mut text_inputs = Vec::new();
        for variable in wizard.prompted_variables() {
            if !takes_text(&variable.kind) {
                continue;
            }
            let value = wizard.value(&variable.name).to_owned();
            let placeholder = variable_placeholder(variable);
            let input = cx.new(|cx| TextInput::new(value, placeholder, cx));
            let field = Field::Variable(variable.name.clone());
            subscriptions.extend(Self::watch_preset_field(&input, field, window, cx));
            text_inputs.push((variable.name.clone(), input));
        }
        let path_inputs = match &host {
            Some(host) => Self::preset_path_inputs(&wizard, host, &mut subscriptions, window, cx),
            None => Vec::new(),
        };
        self.preset_wizard = Some(PresetWizardDialog {
            wizard,
            inline_input,
            issues_input,
            max_per_tab_input,
            text_inputs,
            path_inputs,
            _subscriptions: subscriptions,
        });
        self.preset_wizard_focus.focus(window);
        cx.notify();
    }

    /// A typed host path field for each of `wizard`'s path rows, prefilled
    /// with what the row holds, for remote `host`, where no picker opens;
    /// their watches go into `subscriptions`.
    fn preset_path_inputs(
        wizard: &PresetWizard,
        host: &str,
        subscriptions: &mut Vec<Subscription>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<(Pick, Entity<TextInput>)> {
        let placeholder = format!("Full path on {host}");
        let picks = [Pick::SourceFile, Pick::SourceFolder].into_iter().chain(
            wizard
                .prompted_variables()
                .into_iter()
                .filter_map(Pick::for_variable),
        );
        let mut inputs = Vec::new();
        for pick in picks {
            let text = wizard.path_text(&pick).to_owned();
            let input = cx.new(|cx| TextInput::new(text, placeholder.clone(), cx));
            let field = Field::Path(pick.clone());
            subscriptions.extend(Self::watch_preset_field(&input, field, window, cx));
            inputs.push((pick, input));
        }
        inputs
    }

    /// Closes the wizard and hands the keyboard back to the active pane.
    pub(crate) fn close_preset_wizard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.preset_wizard.take().is_some() {
            self.focus_active_pane(window, cx);
            cx.notify();
        }
    }

    /// An edit goes to the model; Esc in the field closes the wizard, unless
    /// its scripts still run.
    fn watch_preset_field(
        input: &Entity<TextInput>,
        field: Field,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Subscription> {
        let edits = cx.subscribe_in(input, window, move |this, input, _: &TextChanged, _, cx| {
            let text = input.read(cx).text().to_owned();
            if let Some(dialog) = &mut this.preset_wizard {
                match &field {
                    Field::Inline => dialog.wizard.set_inline_text(text),
                    Field::Issues => dialog.wizard.set_issues_spec(text),
                    Field::MaxPerTab => dialog.wizard.set_max_per_tab(text),
                    Field::Variable(name) => dialog.wizard.set_value(name, text),
                    Field::Path(pick) => dialog.wizard.apply_pick(pick.clone(), text),
                }
            }
            cx.notify();
        });
        let keys = cx.subscribe_in(
            input,
            window,
            |this, _, event: &TextInputEvent, window, cx| {
                if *event == TextInputEvent::Cancel && !this.preset_scripts_running() {
                    this.close_preset_wizard(window, cx);
                }
            },
        );
        vec![edits, keys]
    }

    /// Whether the wizard's scripts still run, so Esc, Cancel and Back wait.
    fn preset_scripts_running(&self) -> bool {
        self.preset_wizard
            .as_ref()
            .is_some_and(|dialog| dialog.wizard.scripts_running())
    }

    /// A key while the wizard is open: Esc closes it and is the wizard's,
    /// unless its scripts still run; every other key belongs to the field
    /// that has the keyboard.
    pub(crate) fn on_preset_wizard_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.preset_wizard.is_none() || keystroke.key != "escape" {
            return false;
        }
        if !self.preset_scripts_running() {
            self.close_preset_wizard(window, cx);
        }
        true
    }

    fn with_preset_wizard(
        &mut self,
        cx: &mut Context<Self>,
        change: impl FnOnce(&mut PresetWizard),
    ) {
        if let Some(dialog) = &mut self.preset_wizard {
            change(&mut dialog.wizard);
            cx.notify();
        }
    }

    /// Next: entering the preview asks for it again each time.
    fn advance_preset_wizard(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = &mut self.preset_wizard else {
            return;
        };
        let wizard = &mut dialog.wizard;
        let request = if wizard.advance() && wizard.stage() == Stage::Preview {
            wizard.start_preview(crate::new_request_id())
        } else {
            None
        };
        cx.notify();
        if let Some(msg) = request {
            self.send_to(&self.host_of_request(&msg), msg);
        }
    }

    /// Launch: the scripts run first when the preset has any, else the
    /// launch goes.
    fn press_preset_launch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = &mut self.preset_wizard else {
            return;
        };
        let step = dialog.wizard.press_launch(crate::new_request_id());
        dialog.lock_max_per_tab(cx);
        cx.notify();
        match step {
            Some(LaunchStep::Scripts(msg)) => self.send_to(&self.host_of_request(&msg), msg),
            Some(LaunchStep::Launch(msg)) => self.send_preset_launch(msg, window, cx),
            None => {}
        }
    }

    /// Sends launch `msg`, records its job, shows its progress toast and
    /// closes the wizard.
    fn send_preset_launch(
        &mut self,
        msg: ClientMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (Some(dialog), ClientMessage::LaunchPreset { job_id, .. }) =
            (&self.preset_wizard, &msg)
        else {
            return;
        };
        let wizard = &dialog.wizard;
        let spec = self.preset_launches.start(
            job_id.clone(),
            wizard.entry().name.clone(),
            wizard.shows_max_per_tab(),
            wizard.launch_cap(),
        );
        self.send_to(&self.host_of_request(&msg), msg);
        self.push_toast_spec(spec, cx);
        self.close_preset_wizard(window, cx);
    }

    /// The daemon's answer to the wizard's preview or script requests, and
    /// the job events of this window's launches.
    pub(crate) fn on_preset_launch_message(
        &mut self,
        msg: &DaemonMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match msg {
            DaemonMessage::PresetPreview { id, prompts } => {
                self.with_preset_wizard(cx, |wizard| {
                    wizard.apply_preview(id, Ok(prompts.clone()));
                });
            }
            DaemonMessage::PresetPreviewError { id, error } => {
                self.with_preset_wizard(cx, |wizard| wizard.apply_preview(id, Err(error.clone())));
            }
            DaemonMessage::PresetScriptsResolved { id, values, .. } => {
                self.finish_preset_scripts(id, Ok(values.clone()), window, cx);
            }
            DaemonMessage::PresetScriptsError {
                id,
                error,
                variable_name,
            } => {
                self.finish_preset_scripts(
                    id,
                    Err(ScriptFailure {
                        error: error.clone(),
                        variable: variable_name.clone(),
                    }),
                    window,
                    cx,
                );
            }
            _ if touches_launches(msg) => self.on_launch_job_message(msg, window, cx),
            _ => {}
        }
    }

    /// A job event or a session or tab change: folds a job event of this
    /// window's launches, keeps Select's marks on its job, shows a first
    /// tab, and refits the toasts' buttons.
    fn on_launch_job_message(
        &mut self,
        msg: &DaemonMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let sidebar = &self.sidebar;
        let listed = |id: &str| sidebar.session(id).map(is_live);
        if let Some(spec) = self.preset_launches.on_message(&listed, msg) {
            self.push_toast_spec(spec, cx);
        }
        self.launched_highlight.follow(&self.preset_launches);
        self.show_first_launch_tabs(window, cx);
        self.sync_preset_toast_actions(cx);
    }

    /// Shows each tab-grouping job's first tab once its id is known: now
    /// when the tab is in, else when it arrives.
    fn show_first_launch_tabs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for tab_id in self.preset_launches.take_first_tabs() {
            if self.tabs.tab(&tab_id).is_some() {
                self.tabs.activate(&tab_id);
                self.after_tabs_change(window, cx);
            } else {
                self.tabs.activate_on_arrival(&tab_id, None);
            }
        }
    }

    /// Fits each launch toast's buttons to its job: Stop all only while the
    /// job runs or one of its sessions is not known to have ended, which any
    /// session update may change.
    fn sync_preset_toast_actions(&mut self, cx: &mut Context<Self>) {
        self.preset_launch_syncs += 1;
        let sidebar = &self.sidebar;
        let listed = |id: &str| sidebar.session(id).map(is_live);
        let mut changed = false;
        for (key, actions) in self.preset_launches.shown_actions(&listed) {
            changed |= self.notices.set_actions(&key, actions);
        }
        if changed {
            cx.notify();
        }
    }

    /// Button `action` on launch toast `key`: Cancel cancels the running
    /// job, Select marks its sessions (and those it makes later) and shows
    /// the sessions panel, Stop all cancels a running job, then stops its
    /// sessions not known to have ended (one the sidebar does not list yet
    /// included) and keeps their worktrees.
    pub(crate) fn press_preset_toast_action(
        &mut self,
        key: &str,
        action: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let sidebar = &self.sidebar;
        let listed = |id: &str| sidebar.session(id).map(is_live);
        match self.preset_launches.press(key, action, &listed) {
            Some(JobAction::Cancel(job_id)) => {
                self.send_to(
                    &HostKey::Local,
                    ClientMessage::CancelPresetLaunch { job_id },
                );
            }
            Some(JobAction::Select) => {
                self.launched_highlight.select(key, &self.preset_launches);
                self.show_sessions_panel(window, cx);
            }
            Some(JobAction::StopAll { cancel, sessions }) => {
                if let Some(job_id) = cancel {
                    self.send_to(
                        &HostKey::Local,
                        ClientMessage::CancelPresetLaunch { job_id },
                    );
                }
                for session_id in sessions {
                    self.send_to(
                        &self.host_of_session(&session_id),
                        stop_message(&session_id),
                    );
                }
            }
            None => {}
        }
        self.sync_preset_toast_actions(cx);
        cx.notify();
    }

    /// How many times the launch toasts' buttons were refitted.
    #[must_use]
    pub fn preset_launch_syncs(&self) -> u64 {
        self.preset_launch_syncs
    }

    fn finish_preset_scripts(
        &mut self,
        id: &str,
        result: Result<Vec<(String, String)>, ScriptFailure>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(dialog) = &mut self.preset_wizard else {
            return;
        };
        let launch = dialog
            .wizard
            .apply_scripts(id, result, crate::new_request_id());
        dialog.lock_max_per_tab(cx);
        cx.notify();
        if let Some(msg) = launch {
            self.send_preset_launch(msg, window, cx);
        }
    }

    /// Forgets this window's running launch jobs and drops their toasts, as
    /// a new connection must; ended jobs keep theirs.
    pub(crate) fn reset_preset_launches(&mut self, cx: &mut Context<Self>) {
        for key in self.preset_launches.drop_running() {
            let shown: Vec<u64> = self
                .notices
                .toasts()
                .iter()
                .filter(|toast| toast.key.as_deref() == Some(key.as_str()))
                .map(|toast| toast.id)
                .collect();
            for id in shown {
                self.notices.dismiss(id);
            }
        }
        cx.notify();
    }

    /// Asks this PC's file or folder picker for `pick` through
    /// [`RootView::pick_local`], so none opens on a remote host and an
    /// answer that lands after a switch to one is dropped; the answer fills
    /// `pick` when the wizard is still open. A path goes as picked.
    fn pick_for_preset(&mut self, pick: Pick, cx: &mut Context<Self>) {
        let Some(dialog) = &self.preset_wizard else {
            return;
        };
        let start = dialog.wizard.pick_start(&pick);
        let picker = match dialog.wizard.pick_filter(&pick) {
            Some(filter) => crate::Pick::File(filter),
            None => crate::Pick::Folder,
        };
        self.pick_local(picker, start, cx, move |this, picked, cx| {
            this.finish_preset_pick(pick, picked, cx);
        });
    }

    fn finish_preset_pick(&mut self, pick: Pick, picked: Option<PathBuf>, cx: &mut Context<Self>) {
        let Some(picked) = picked else {
            return;
        };
        let path = picked.to_string_lossy().into_owned();
        self.with_preset_wizard(cx, |wizard| wizard.apply_pick(pick, path));
    }

    /// The wizard over a backdrop that takes every click beneath it.
    pub(crate) fn preset_wizard_layer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = theme(cx);
        let dialog = self.preset_wizard.as_ref()?;
        let wizard = &dialog.wizard;
        let description = wizard.entry().description.clone().map(|text| {
            div()
                .debug_selector(|| "preset-wizard-description".to_owned())
                .text_color(gpui::rgb(t.muted))
                .child(text)
        });
        let pick_refusal = self.capabilities().local_files_refusal();
        let body = match wizard.stage() {
            Stage::Source => source_body(dialog, pick_refusal, cx),
            Stage::Variables => variables_body(dialog, pick_refusal, cx),
            Stage::Preview => preview_body(dialog, &t),
        };
        let panel = modal_panel(
            crate::fonts::ui_text_size(cx),
            "preset-wizard-panel",
            &theme(cx),
        )
        .track_focus(&self.preset_wizard_focus)
        .child(
            div()
                .debug_selector(|| "preset-wizard-title".to_owned())
                .font_weight(crate::fonts::ui_weight(cx, FontWeight::SEMIBOLD))
                .child(wizard.title()),
        )
        .children(description)
        .child(body)
        .child(wizard_footer(wizard, cx));
        Some(backdrop("preset-wizard", panel, &theme(cx)))
    }

    /// Whether the preset wizard is open.
    #[must_use]
    pub fn preset_wizard_open(&self) -> bool {
        self.preset_wizard.is_some()
    }

    /// The wizard's heading.
    #[must_use]
    pub fn preset_wizard_title(&self) -> Option<String> {
        self.preset_wizard.as_ref().map(|d| d.wizard.title())
    }

    /// The stage on screen: `source` or `variables`.
    #[must_use]
    pub fn preset_wizard_stage(&self) -> Option<&'static str> {
        self.preset_wizard.as_ref().map(|d| d.wizard.stage().name())
    }

    /// The source radio rows as drawn: selector, label and whether picked.
    #[must_use]
    pub fn preset_wizard_sources(&self) -> Vec<(String, String, bool)> {
        let Some(dialog) = &self.preset_wizard else {
            return Vec::new();
        };
        let picked = dialog.wizard.source_kind();
        dialog
            .wizard
            .entry()
            .prompt_sources
            .iter()
            .map(|source| {
                let kind = SourceKind::of(source);
                (
                    kind.selector().to_owned(),
                    source_label(source),
                    kind == picked,
                )
            })
            .collect()
    }

    /// The picked prompts file, while one is.
    #[must_use]
    pub fn preset_wizard_file_path(&self) -> Option<String> {
        self.preset_wizard
            .as_ref()
            .and_then(|d| d.wizard.file_path().map(str::to_owned))
    }

    /// The prompts folder, while one is set.
    #[must_use]
    pub fn preset_wizard_folder_path(&self) -> Option<String> {
        self.preset_wizard
            .as_ref()
            .and_then(|d| d.wizard.folder_path().map(str::to_owned))
    }

    /// The text of the typed host path field tagged `selector`; `None` when
    /// the row has none, as on this PC.
    #[must_use]
    pub fn preset_wizard_path_text(&self, selector: &str, cx: &App) -> Option<String> {
        let dialog = self.preset_wizard.as_ref()?;
        let input = dialog.path_input(dialog.pick_at(selector)?)?;
        Some(input.read(cx).text().to_owned())
    }

    /// The refusal under the typed host path row tagged `selector`.
    #[must_use]
    pub fn preset_wizard_path_refusal(&self, selector: &str) -> Option<String> {
        let dialog = self.preset_wizard.as_ref()?;
        dialog.wizard.typed_path_refusal(dialog.pick_at(selector)?)
    }

    /// Next as drawn: whether it can be pressed, and its tooltip.
    #[must_use]
    pub fn preset_wizard_next(&self) -> Option<NextButton> {
        self.preset_wizard.as_ref().map(|d| d.wizard.next_button())
    }

    /// The value variable `name` holds.
    #[must_use]
    pub fn preset_wizard_value(&self, name: &str) -> Option<String> {
        self.preset_wizard
            .as_ref()
            .map(|d| d.wizard.value(name).to_owned())
    }

    /// The line under the issue spec.
    #[must_use]
    pub fn preset_wizard_issues_line(&self) -> Option<String> {
        self.preset_wizard
            .as_ref()
            .map(|d| match d.wizard.issues_feedback() {
                IssuesFeedback::Hint => ISSUES_HINT.to_owned(),
                IssuesFeedback::Ok(line) | IssuesFeedback::Error(line) => line,
            })
    }

    /// The line over the preview list.
    #[must_use]
    pub fn preset_wizard_preview_line(&self) -> Option<String> {
        self.preset_wizard
            .as_ref()
            .map(|d| preview_line_of(&d.wizard))
    }

    /// The preview's rows as drawn, `[n] first line`, by tab.
    #[must_use]
    pub fn preset_wizard_preview_groups(&self) -> Vec<Vec<String>> {
        self.preset_wizard.as_ref().map_or_else(Vec::new, |d| {
            d.wizard
                .preview_groups()
                .iter()
                .map(|group| group.iter().map(PreviewRow::text).collect())
                .collect()
        })
    }

    /// Launch as drawn on the preview.
    #[must_use]
    pub fn preset_wizard_launch(&self) -> Option<LaunchButton> {
        self.preset_wizard
            .as_ref()
            .map(|d| d.wizard.launch_button())
    }

    /// The script commands as drawn: each label and its command line.
    #[must_use]
    pub fn preset_wizard_scripts(&self) -> Vec<(String, String)> {
        self.preset_wizard.as_ref().map_or_else(Vec::new, |d| {
            d.wizard
                .script_commands()
                .into_iter()
                .map(|command| {
                    let line = match command.line {
                        ScriptLine::Ready(line) => line,
                        ScriptLine::Unresolved => SCRIPT_UNRESOLVED.to_owned(),
                        skipped @ ScriptLine::Skipped(_) => skipped.note().unwrap_or_default(),
                    };
                    (command.label, line)
                })
                .collect()
        })
    }

    /// The line under the script commands after a failed run.
    #[must_use]
    pub fn preset_wizard_script_error(&self) -> Option<String> {
        self.preset_wizard
            .as_ref()
            .and_then(|d| d.wizard.script_stage().failure())
    }

    /// How far the launch job of toast `key` has come, 0 to 1, while it
    /// runs.
    #[must_use]
    pub fn preset_launch_progress(&self, key: &str) -> Option<f32> {
        self.preset_launches.progress(key)
    }

    /// Whether the launch job of toast `key` is still in the registry.
    #[must_use]
    pub fn preset_launch_known(&self, key: &str) -> bool {
        self.preset_launches.knows(key)
    }
}

/// The line over the preview list.
fn preview_line_of(wizard: &PresetWizard) -> String {
    crate::preset_launch::preview_line(wizard.preview(), wizard.preview_summary())
}

/// The Preview stage: the summary or the reason it failed, the script
/// commands, the max-panes field, the caveat and the prompts by tab.
fn preview_body(dialog: &PresetWizardDialog, t: &UiTheme) -> Div {
    let wizard = &dialog.wizard;
    let line = match wizard.preview() {
        Preview::Failed(_) => div()
            .debug_selector(|| "preset-preview-error".to_owned())
            .text_color(gpui::rgb(t.danger))
            .child(preview_line_of(wizard)),
        Preview::Loading { .. } | Preview::Ready(_) => {
            muted(preview_line_of(wizard), t).debug_selector(|| "preset-preview-summary".to_owned())
        }
    };
    let max_per_tab = wizard.shows_max_per_tab().then(|| {
        div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(field_label(
                MAX_PER_TAB_LABEL.to_owned(),
                Some(max_per_tab_note(wizard.entry())),
                t,
            ))
            .child(input_box(
                "preset-max-per-tab".to_owned(),
                &dialog.max_per_tab_input,
                t,
            ))
    });
    let prompts = match wizard.preview() {
        Preview::Ready(prompts) => Some(prompts.len()),
        Preview::Loading { .. } | Preview::Failed(_) => None,
    };
    let caveat = prompts
        .filter(|count| *count > 0)
        .map(|_| muted(LAUNCH_CAVEAT, t));
    let list = match prompts {
        Some(0) => Some(muted(PREVIEW_EMPTY, t)),
        Some(_) => Some(preview_list(wizard, t)),
        None => None,
    };
    div()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .child(line)
        .children(scripts_section(wizard, t))
        .children(max_per_tab)
        .children(caveat)
        .children(list)
}

/// The prompts, under a `Tab n of m` header each when they fill several
/// tabs.
fn preview_list(wizard: &PresetWizard, t: &UiTheme) -> Div {
    let groups = wizard.preview_groups();
    let count = groups.len();
    let groups = groups.into_iter().enumerate().map(|(at, group)| {
        let header = (count > 1).then(|| muted(tab_header(at, count), t));
        let rows = group.into_iter().map(|row| {
            let selector = format!("preset-preview-row-{}", row.index);
            div().debug_selector(move || selector).child(row.text())
        });
        div().flex().flex_col().children(header).children(rows)
    });
    div()
        .debug_selector(|| "preset-preview-list".to_owned())
        .flex()
        .flex_col()
        .gap(px(6.0))
        .children(groups)
}

/// The script commands that run before the launch, as they would run.
fn scripts_section(wizard: &PresetWizard, t: &UiTheme) -> Option<Div> {
    let commands = wizard.script_commands();
    if commands.is_empty() {
        return None;
    }
    let stage = wizard.script_stage();
    let failed = stage.failed_variable();
    let rows = commands.into_iter().map(|command| {
        let selector = format!("preset-script-row-{}", command.name);
        let mark = format!("preset-script-row-{}-failed", command.name);
        let marked = failed == Some(command.name.as_str());
        let line = match &command.line {
            ScriptLine::Ready(line) => Some(div().child(line.clone())),
            ScriptLine::Unresolved => Some(
                div()
                    .text_color(gpui::rgb(t.danger))
                    .child(SCRIPT_UNRESOLVED),
            ),
            ScriptLine::Skipped(_) => None,
        };
        let label = field_label(command.label.clone(), command.line.note(), t);
        let label = if marked {
            label
                .debug_selector(move || mark)
                .text_color(gpui::rgb(t.danger))
        } else {
            label
        };
        div()
            .debug_selector(move || selector)
            .flex()
            .flex_col()
            .child(label)
            .children(line)
    });
    let failure = stage.failure().map(|text| {
        div()
            .debug_selector(|| "preset-script-error".to_owned())
            .text_color(gpui::rgb(t.danger))
            .child(text)
    });
    Some(
        div()
            .debug_selector(|| "preset-script-section".to_owned())
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(field_label(
                SCRIPTS_LEGEND.to_owned(),
                stage.legend_note().map(str::to_owned),
                t,
            ))
            .child(muted(SCRIPTS_NOTE, t))
            .children(rows)
            .children(failure),
    )
}

/// A muted label line.
fn muted(text: impl Into<gpui::SharedString>, t: &UiTheme) -> Div {
    div().text_color(gpui::rgb(t.muted)).child(text.into())
}

/// A field's label with an optional muted note after it.
fn field_label(label: String, note: Option<String>, t: &UiTheme) -> Div {
    div()
        .flex()
        .child(label)
        .children(note.map(|note| div().text_color(gpui::rgb(t.muted)).child(note)))
}

/// A text input in its box, tagged `selector`.
fn input_box(selector: String, input: &Entity<TextInput>, t: &UiTheme) -> Div {
    div()
        .debug_selector(move || selector)
        .flex()
        .flex_1()
        .px(px(6.0))
        .py(px(3.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(gpui::rgb(t.border))
        .child(input.clone())
}

/// A radio row: a filled dot while picked.
fn radio_row(selector: &'static str, picked: bool, label: String) -> Stateful<Div> {
    div()
        .id(selector)
        .debug_selector(move || selector.to_owned())
        .flex()
        .items_center()
        .gap(px(6.0))
        .cursor_pointer()
        .child(if picked { "◉" } else { "○" })
        .child(label)
}

/// What a row's button does to the root view.
type ViewAction = Box<dyn Fn(&mut RootView, &mut Context<RootView>)>;

/// A path row's Pick… button: the pick a press asks for, and why the button
/// is disabled (its tooltip) when it is.
struct PickButton {
    pick: Pick,
    refusal: Option<&'static str>,
}

/// What a path row shows beside its Pick… button.
enum PathCell<'a> {
    /// The path as picked, read-only, with Clear when `on_clear` is given
    /// and the path is set.
    Picked {
        path: Option<&'a str>,
        placeholder: &'a str,
        on_clear: Option<ViewAction>,
    },
    /// On a remote host, the typed host path's field and the refusal
    /// shown under it.
    Typed {
        input: &'a Entity<TextInput>,
        refusal: Option<String>,
    },
}

/// `pick`'s cell: its typed host path field when the dialog has one,
/// else `picked`.
fn path_cell<'a>(
    dialog: &'a PresetWizardDialog,
    pick: &Pick,
    picked: PathCell<'a>,
) -> PathCell<'a> {
    match dialog.path_input(pick) {
        Some(input) => PathCell::Typed {
            input,
            refusal: dialog.wizard.typed_path_refusal(pick),
        },
        None => picked,
    }
}

/// A path row: `cell` beside its Pick… button, then a typed path's
/// refusal under them.
fn path_row(
    selector: &str,
    cell: PathCell<'_>,
    button: PickButton,
    cx: &mut Context<RootView>,
) -> Div {
    let t = theme(cx);
    match cell {
        PathCell::Picked {
            path,
            placeholder,
            on_clear,
        } => picked_path_row(selector, path, placeholder, button, on_clear, cx),
        PathCell::Typed { input, refusal } => {
            let row = div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .child(input_box(selector.to_owned(), input, &t))
                .child(pick_button(selector, button, cx));
            let error = refusal.map(|refusal| {
                let tag = format!("{selector}-error");
                div()
                    .debug_selector(move || tag)
                    .px(px(6.0))
                    .py(px(3.0))
                    .rounded(px(4.0))
                    .bg(gpui::rgb(t.danger_bg))
                    .text_color(gpui::rgb(t.danger))
                    .child(refusal)
            });
            div()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .child(row)
                .children(error)
        }
    }
}

/// A path row's Pick… button, disabled with its refusal as its tooltip
/// when it has one.
fn pick_button(selector: &str, button: PickButton, cx: &mut Context<RootView>) -> Stateful<Div> {
    let PickButton { pick, refusal } = button;
    let pick_enabled = refusal.is_none();
    dialog_button(
        &format!("{selector}-pick"),
        PICK_LABEL.to_owned(),
        dialog_look(
            crate::fonts::ui_text_size(cx),
            crate::fonts::ui_weight_shift(cx),
            false,
            pick_enabled,
            &theme(cx),
        ),
        false,
    )
    .when_some(refusal, |button, tip| button.tooltip(tooltip(tip)))
    .when(pick_enabled, |button| {
        button.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.pick_for_preset(pick.clone(), cx);
        }))
    })
}

/// A read-only path with its Pick… button, and Clear when `clear` is
/// given and the path is set.
fn picked_path_row(
    selector: &str,
    path: Option<&str>,
    placeholder: &str,
    button: PickButton,
    on_clear: Option<ViewAction>,
    cx: &mut Context<RootView>,
) -> Div {
    let t = theme(cx);
    let shown = div()
        .debug_selector({
            let selector = selector.to_owned();
            move || selector
        })
        .flex_1()
        .px(px(6.0))
        .py(px(3.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(gpui::rgb(t.border))
        .text_color(gpui::rgb(if path.is_some() { t.text } else { t.muted }))
        .child(path.unwrap_or(placeholder).to_owned());
    let pick_button = pick_button(selector, button, cx);
    let clear = on_clear.filter(|_| path.is_some()).map(|on_clear| {
        dialog_button(
            &format!("{selector}-clear"),
            CLEAR_LABEL.to_owned(),
            dialog_look(
                crate::fonts::ui_text_size(cx),
                crate::fonts::ui_weight_shift(cx),
                false,
                true,
                &theme(cx),
            ),
            false,
        )
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| on_clear(this, cx)))
    });
    div()
        .flex()
        .items_center()
        .gap(px(6.0))
        .child(shown)
        .child(pick_button)
        .children(clear)
}

/// The Source stage: the allowed sources, then the picked one's field; its
/// Pick… is disabled with `pick_refusal` as its tooltip when one is given.
fn source_body(
    dialog: &PresetWizardDialog,
    pick_refusal: Option<&'static str>,
    cx: &mut Context<RootView>,
) -> Div {
    let t = theme(cx);
    let wizard = &dialog.wizard;
    let picked = wizard.source_kind();
    let radios: Vec<Stateful<Div>> = wizard
        .entry()
        .prompt_sources
        .iter()
        .map(|source| {
            let kind = SourceKind::of(source);
            radio_row(kind.selector(), kind == picked, source_label(source)).on_click(cx.listener(
                move |this, _: &ClickEvent, _, cx| {
                    this.with_preset_wizard(cx, |wizard| {
                        wizard.set_source_kind(kind);
                    });
                },
            ))
        })
        .collect();
    let field = match picked {
        SourceKind::File => file_field(dialog, pick_refusal, cx),
        SourceKind::Folder => folder_field(dialog, pick_refusal, cx),
        SourceKind::Inline => div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(INLINE_LABEL)
            .child(input_box(
                "preset-source-inline-text".to_owned(),
                &dialog.inline_input,
                &t,
            )),
        SourceKind::GithubIssues => issues_field(dialog, &t),
    };
    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(muted(SOURCE_LEGEND, &t))
        .children(radios)
        .child(field)
}

fn file_field(
    dialog: &PresetWizardDialog,
    pick_refusal: Option<&'static str>,
    cx: &mut Context<RootView>,
) -> Div {
    let picked = PathCell::Picked {
        path: dialog.wizard.file_path(),
        placeholder: FILE_PLACEHOLDER,
        on_clear: None,
    };
    let row = path_row(
        "preset-source-file",
        path_cell(dialog, &Pick::SourceFile, picked),
        PickButton {
            pick: Pick::SourceFile,
            refusal: pick_refusal,
        },
        cx,
    );
    div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(FILE_LABEL)
        .child(row)
}

fn folder_field(
    dialog: &PresetWizardDialog,
    pick_refusal: Option<&'static str>,
    cx: &mut Context<RootView>,
) -> Div {
    let t = theme(cx);
    let wizard = &dialog.wizard;
    let note = wizard
        .entry()
        .prompt_sources
        .iter()
        .find_map(|source| match source {
            PresetPromptSource::Folder { relative_path } => {
                Some(format!(" · default: {relative_path}"))
            }
            _ => None,
        });
    let picked = PathCell::Picked {
        path: wizard.folder_path(),
        placeholder: FOLDER_PLACEHOLDER,
        on_clear: None,
    };
    let row = path_row(
        "preset-source-folder",
        path_cell(dialog, &Pick::SourceFolder, picked),
        PickButton {
            pick: Pick::SourceFolder,
            refusal: pick_refusal,
        },
        cx,
    );
    div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(field_label(FOLDER_LABEL.to_owned(), note, &t))
        .child(row)
}

fn issues_field(dialog: &PresetWizardDialog, t: &UiTheme) -> Div {
    let line = match dialog.wizard.issues_feedback() {
        IssuesFeedback::Hint => muted(ISSUES_HINT, t),
        IssuesFeedback::Ok(summary) => {
            muted(summary, t).debug_selector(|| "preset-source-github-issues-ok".to_owned())
        }
        IssuesFeedback::Error(err) => div()
            .debug_selector(|| "preset-source-github-issues-error".to_owned())
            .text_color(gpui::rgb(t.danger))
            .child(err),
    };
    div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(field_label(
            ISSUES_LABEL.to_owned(),
            Some(ISSUES_NOTE.to_owned()),
            t,
        ))
        .child(input_box(
            "preset-source-github-issues-spec".to_owned(),
            &dialog.issues_input,
            t,
        ))
        .child(line)
}

/// The Variables stage: one field per prompted variable; a path field's
/// Pick… is disabled with `pick_refusal` as its tooltip when one is given.
fn variables_body(
    dialog: &PresetWizardDialog,
    pick_refusal: Option<&'static str>,
    cx: &mut Context<RootView>,
) -> Div {
    let wizard = &dialog.wizard;
    let fields: Vec<Div> = wizard
        .prompted_variables()
        .into_iter()
        .map(|variable| variable_field(dialog, variable, pick_refusal, cx))
        .collect();
    div().flex().flex_col().gap(px(8.0)).children(fields)
}

fn variable_field(
    dialog: &PresetWizardDialog,
    variable: &PresetVariable,
    pick_refusal: Option<&'static str>,
    cx: &mut Context<RootView>,
) -> Div {
    let t = theme(cx);
    let wizard = &dialog.wizard;
    let selector = variable_selector(&variable.name);
    let name = variable.name.clone();
    let value = wizard.value(&variable.name);
    let path = (!value.is_empty()).then_some(value);
    let placeholder = variable_placeholder(variable);
    let input: AnyElement = if let Some(pick) = Pick::for_variable(variable) {
        let picked = PathCell::Picked {
            path,
            placeholder: &placeholder,
            on_clear: Some(Box::new(move |this, cx| {
                this.with_preset_wizard(cx, |wizard| wizard.set_value(&name, String::new()));
            })),
        };
        let cell = path_cell(dialog, &pick, picked);
        path_row(
            &selector,
            cell,
            PickButton {
                pick,
                refusal: pick_refusal,
            },
            cx,
        )
        .into_any_element()
    } else if matches!(variable.kind, PresetVariableKind::Toggle { .. }) {
        checkbox_row(
            crate::fonts::ui_text_size(cx),
            crate::fonts::ui_weight_shift(cx),
            &selector,
            wizard.toggle_on(&name),
            false,
            "",
            &theme(cx),
        )
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.with_preset_wizard(cx, |wizard| wizard.toggle(&name));
        }))
        .into_any_element()
    } else {
        match dialog.text_inputs.iter().find(|(n, _)| *n == variable.name) {
            Some((_, input)) => input_box(selector.clone(), input, &t).into_any_element(),
            None => div().into_any_element(),
        }
    };
    let note = shows_required(variable).then(|| REQUIRED_NOTE.to_owned());
    let missing = wizard.is_missing(variable).then(|| {
        let hint = format!("{selector}-required");
        div()
            .debug_selector(move || hint)
            .text_color(gpui::rgb(t.danger))
            .child(REQUIRED_HINT)
    });
    div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(field_label(variable.label.clone(), note, &t))
        .child(input)
        .children(missing)
}

/// Cancel, Back after the first stage, and Next with its tooltip, or
/// Launch on the preview. Cancel and Back wait while scripts run.
fn wizard_footer(wizard: &PresetWizard, cx: &mut Context<RootView>) -> Div {
    let idle = !wizard.scripts_running();
    let cancel = dialog_button(
        "preset-cancel",
        "Cancel".to_owned(),
        dialog_look(
            crate::fonts::ui_text_size(cx),
            crate::fonts::ui_weight_shift(cx),
            false,
            idle,
            &theme(cx),
        ),
        false,
    )
    .when(idle, |button| {
        button.on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
            this.close_preset_wizard(window, cx);
        }))
    });
    let back = (wizard.stage() != Stage::Source).then(|| {
        dialog_button(
            "preset-back",
            "Back".to_owned(),
            dialog_look(
                crate::fonts::ui_text_size(cx),
                crate::fonts::ui_weight_shift(cx),
                false,
                idle,
                &theme(cx),
            ),
            false,
        )
        .when(idle, |button| {
            button.on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.with_preset_wizard(cx, |wizard| {
                    wizard.back();
                });
            }))
        })
    });
    let forward = if wizard.stage() == Stage::Preview {
        let state = wizard.launch_button();
        dialog_button(
            "preset-launch",
            state.label,
            dialog_look(
                crate::fonts::ui_text_size(cx),
                crate::fonts::ui_weight_shift(cx),
                false,
                state.enabled,
                &theme(cx),
            ),
            false,
        )
        .when_some(state.tip, |button, tip| button.tooltip(tooltip(tip)))
        .when(state.enabled, |button| {
            button.on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.press_preset_launch(window, cx);
            }))
        })
    } else {
        let state = wizard.next_button();
        dialog_button(
            "preset-next",
            "Next".to_owned(),
            dialog_look(
                crate::fonts::ui_text_size(cx),
                crate::fonts::ui_weight_shift(cx),
                false,
                state.enabled,
                &theme(cx),
            ),
            false,
        )
        .when_some(state.tip, |button, tip| button.tooltip(tooltip(tip)))
        .when(state.enabled, |button| {
            button.on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.advance_preset_wizard(cx);
            }))
        })
    };
    div()
        .flex()
        .justify_end()
        .gap(px(6.0))
        .child(cancel)
        .children(back)
        .child(forward)
}
