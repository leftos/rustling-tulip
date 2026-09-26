//! The spawn dialog on screen: opening and closing it, its keys, clicks and
//! text fields, and its rendering. The state and every rule live in
//! [`crate::spawn_form`]; spawning goes through [`RootView::spawn`].

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use gpui::{
    AnyElement, ClickEvent, Context, Div, ElementId, Entity, Focusable as _, FontWeight, Keystroke,
    SharedString, Stateful, Subscription, Task, Window, div, prelude::*, px,
};
use protocol::DaemonMessage;

use crate::session_menu::{backdrop, dialog_button};
use crate::spawn_form::{
    Control, FormInputs, OpenChoice, Outcome, Runtime, ShareButton, SpawnForm, TabChoices, Target,
    WorktreeMode,
};
use crate::spawns::PaneAim;
use crate::text_input::{TextChanged, TextInput, TextInputEvent};
use crate::{BORDER, HOVER_BG, MUTED, PANEL_BG, RootView, TEXT, UI_TEXT_SIZE, tooltip};

const DIALOG_WIDTH: f32 = 520.0;
const BODY_MAX_HEIGHT: f32 = 440.0;
const SELECTED_BG: u32 = 0x0037_3a44;
const WARNING: u32 = 0x00e8_a531;
const DIALOG_TITLE: &str = "Spawn session";
const SHARE_TITLE: &str = "Share this worktree?";
const SHARE_BODY: &str = "A session is already running in the worktree you picked. Both agents will see each other's uncommitted edits, and concurrent writes to the same file will overwrite one another.";
const RANDOM_TIP: &str = "Generate a random worktree branch name";
const SUGGESTING: &str = "Picking a name no existing branch uses…";
const FETCH_FAILED: &str = "Couldn't reach the remote — comparing against the last fetched refs.";
const CURRENT_TAB_DISABLED: &str = "The current tab cannot host terminal panes";

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

/// The open spawn dialog: its form and its two text fields.
pub(crate) struct SpawnDialog {
    form: SpawnForm,
    /// The pane it was opened from, if any.
    aim: Option<PaneAim>,
    branch_input: Entity<TextInput>,
    base_input: Entity<TextInput>,
    _subscriptions: Vec<Subscription>,
    /// Wakes the view when the wait for a branch suggestion runs out.
    timer: Option<Task<()>>,
}

/// Which text field an event came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Branch,
    Base,
}

impl Field {
    fn control(self) -> Control {
        match self {
            Self::Branch => Control::Branch,
            Self::Base => Control::Base,
        }
    }
}

/// How a button looks.
#[derive(Debug, Clone, Copy)]
struct Look {
    selected: bool,
    focused: bool,
    enabled: bool,
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
        chosen.iter().map(Control::selector).collect()
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
        let mut subscriptions = Self::watch_field(&branch_input, Field::Branch, window, cx);
        subscriptions.extend(Self::watch_field(&base_input, Field::Base, window, cx));
        self.spawn_dialog = Some(SpawnDialog {
            form,
            aim,
            branch_input,
            base_input,
            _subscriptions: subscriptions,
            timer: None,
        });
        self.send_all(messages);
        self.after_spawn_change(cx);
        self.apply_spawn_focus(window, cx);
    }

    /// Enter submits, Esc closes, an edit reaches the form and a click in
    /// the field moves the form's focus there.
    fn watch_field(
        input: &Entity<TextInput>,
        field: Field,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Subscription> {
        let keys = cx.subscribe_in(
            input,
            window,
            |this, _, event: &TextInputEvent, window, cx| match event {
                TextInputEvent::Submit => this.submit_spawn_dialog(window, cx),
                TextInputEvent::Cancel => this.close_spawn_dialog(window, cx),
            },
        );
        let edits = cx.subscribe_in(input, window, move |this, input, _: &TextChanged, _, cx| {
            let text = input.read(cx).text().to_owned();
            if let Some(dialog) = &mut this.spawn_dialog {
                match field {
                    Field::Branch => dialog.form.edit_branch(&text, &mut this.branch_cache),
                    Field::Base => dialog.form.edit_base(&text),
                }
            }
            cx.notify();
        });
        let handle = input.read(cx).focus_handle(cx);
        let focus = cx.on_focus(&handle, window, move |this, _, cx| {
            if let Some(dialog) = &mut this.spawn_dialog {
                dialog.form.set_focus(field.control());
            }
            cx.notify();
        });
        vec![keys, edits, focus]
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

    /// The fields follow the form, and a timer waits out a suggestion.
    fn after_spawn_change(&mut self, cx: &mut Context<Self>) {
        let now = (self.now)();
        let Some(dialog) = &mut self.spawn_dialog else {
            return;
        };
        let form = &dialog.form;
        let (branch, base) = (form.branch().to_owned(), form.base().to_owned());
        let placeholders = (form.branch_placeholder(now), form.default_base());
        sync_field(&dialog.branch_input, &branch, placeholders.0, cx);
        sync_field(&dialog.base_input, &base, placeholders.1, cx);
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

    /// The keyboard goes where the form's focus is: a text field, else the
    /// dialog itself.
    pub(crate) fn apply_spawn_focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = &self.spawn_dialog else {
            return;
        };
        let input = match dialog.form.focused() {
            _ if dialog.form.share_confirm().is_some() => None,
            Control::Branch => Some(&dialog.branch_input),
            Control::Base => Some(&dialog.base_input),
            _ => None,
        };
        match input {
            Some(input) => input.read(cx).focus_handle(cx).focus(window),
            None => self.spawn_focus.focus(window),
        }
        cx.notify();
    }

    /// Whether a text field of the dialog holds the keyboard.
    fn spawn_field_focused(&self, window: &Window, cx: &Context<Self>) -> bool {
        self.spawn_dialog.as_ref().is_some_and(|dialog| {
            [&dialog.branch_input, &dialog.base_input]
                .into_iter()
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
                self.close_spawn_dialog(window, cx);
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
            .flex()
            .flex_col()
            .gap(px(10.0))
            .max_h(px(BODY_MAX_HEIGHT))
            .overflow_y_scroll()
            .child(target_field(form, &focus, cx))
            .child(runtime_field(form, &focus, cx))
            .child(open_in_field(form, &focus, cx))
            .children(trusted_rows(form, &focus, cx))
            .children(self.worktree_rows(dialog, &focus, cx));
        let panel = panel("spawn-panel")
            .when(form.share_confirm().is_none(), |panel| {
                panel.track_focus(&self.spawn_focus)
            })
            .child(dialog_header(focus == Control::Close, cx))
            .child(body)
            .child(footer(form, &focus, cx));
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
        let base = (form.use_worktree() && !form.pinning()).then(|| base_field(dialog, focus));
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
fn field(label: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(div().text_color(gpui::rgb(MUTED)).child(label.into()))
}

/// A row of choices that wraps.
fn segmented(buttons: Vec<AnyElement>) -> Div {
    div().flex().flex_wrap().gap(px(4.0)).children(buttons)
}

fn muted(text: impl Into<SharedString>) -> Div {
    div().text_color(gpui::rgb(MUTED)).child(text.into())
}

/// A choice or a plain button: filled when chosen, outlined when focused,
/// dimmed and inert when disabled.
fn option_button(
    control: &Control,
    label: impl IntoElement,
    look: Look,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    let name = control.selector();
    let pressed = control.clone();
    div()
        .id(ElementId::Name(SharedString::from(name.clone())))
        .debug_selector(|| name)
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
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.press_spawn_control(&pressed, window, cx);
                }))
        })
        .when(!look.enabled, |button| button.opacity(0.5))
        .child(label)
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

/// A text field in a box.
fn input_box(selector: &'static str, input: &Entity<TextInput>, focused: bool) -> Div {
    div()
        .debug_selector(|| selector.to_owned())
        .flex()
        .flex_1()
        .px(px(6.0))
        .py(px(3.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(gpui::rgb(if focused { TEXT } else { BORDER }))
        .child(input.clone())
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
    let input = input_box(
        "spawn-branch",
        &dialog.branch_input,
        *focus == Control::Branch,
    );
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

fn base_field(dialog: &SpawnDialog, focus: &Control) -> AnyElement {
    let form = &dialog.form;
    let input = input_box(
        "spawn-base-branch",
        &dialog.base_input,
        *focus == Control::Base,
    );
    let failed = (form.fetch_failed() && !form.is_workspace()).then(|| muted(FETCH_FAILED));
    field("Base branch (optional)")
        .child(input)
        .children(failed)
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
