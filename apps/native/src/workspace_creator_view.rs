//! The workspace creator on screen: its two mode tabs, the name field, the
//! repo ticks or the picked `.code-workspace` file, and its footer. Its state
//! and every rule live in [`crate::workspace_creator`]; this view renders it
//! and forwards clicks and keys.

use gpui::{
    AnyElement, App, ClickEvent, Context, Div, Entity, Focusable as _, FontWeight, Keystroke,
    Stateful, Subscription, Window, div, prelude::*, px,
};

use protocol::{ClientMessage, RepoEntry, VscodeWorkspaceSuggestion, WorkspaceEntry};

use crate::notice_view::modal_panel;
use crate::session_menu::{backdrop, dialog_button};
use crate::spawn_view::checkbox_row;
use crate::text_input::{TextChanged, TextInput, TextInputEvent};
use crate::workspace_creator::{
    CreatorControl, CreatorMode, FILE_LABEL, MEMBERS_LABEL, NAME_PLACEHOLDER, NO_FILE,
    NO_REPOS_NOTE, PARSING, Settled, TITLE, WORKSPACE_NAME_LABEL, WorkspaceCreator,
};
use crate::{BORDER, DANGER, MUTED, RootView, TEXT};

/// The open creator: its model and the name field's input entity.
pub(crate) struct WorkspaceCreatorDialog {
    creator: WorkspaceCreator,
    name_input: Entity<TextInput>,
    _subscriptions: Vec<Subscription>,
}

impl RootView {
    /// "Add workspace…": opens the creator, unless the connection is down or
    /// another dialog, menu or prompt is up.
    pub(crate) fn open_workspace_creator(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let blocked = self.workspace_creator.is_some()
            || self.vscode_prompt_open()
            || self.exit.is_some()
            || self.spawn_dialog.is_some()
            || self.appearance_editor.is_some()
            || self.delete_dialog.is_some()
            || self.menu.is_some()
            || self.container_menu.is_some()
            || self.sidebar_more_open()
            || self.shell_menu.is_some()
            || self.sc_picker_open
            || self.changes.file_menu.is_some()
            || self.changes.discard.is_some()
            || self.stash.drop.is_some()
            || self.notices.has_modal()
            || self.conn.overlay().is_some();
        if blocked {
            return;
        }
        self.renaming = None;
        self.close_flyout();
        let name_input = cx.new(|cx| TextInput::new(String::new(), NAME_PLACEHOLDER, cx));
        let subscriptions = Self::watch_workspace_name(&name_input, window, cx);
        self.workspace_creator = Some(WorkspaceCreatorDialog {
            creator: WorkspaceCreator::default(),
            name_input,
            _subscriptions: subscriptions,
        });
        self.apply_workspace_creator_focus(window, cx);
    }

    /// Closes the creator and hands the keyboard back to the active pane.
    pub(crate) fn close_workspace_creator(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.workspace_creator.take().is_some() {
            self.focus_active_pane(window, cx);
            cx.notify();
        }
    }

    /// Enter submits (through the field's own event), Esc closes, an edit
    /// redraws the form and a click in the field moves the focus there.
    fn watch_workspace_name(
        input: &Entity<TextInput>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Subscription> {
        let keys = cx.subscribe_in(
            input,
            window,
            |this, _, event: &TextInputEvent, window, cx| match event {
                TextInputEvent::Submit => this.submit_workspace_creator(window, cx),
                TextInputEvent::Cancel => this.close_workspace_creator(window, cx),
            },
        );
        let edits = cx.subscribe_in(input, window, |this, input, _: &TextChanged, _, cx| {
            let text = input.read(cx).text().to_owned();
            if let Some(dialog) = &mut this.workspace_creator {
                dialog.creator.set_name(text);
            }
            cx.notify();
        });
        let handle = input.read(cx).focus_handle(cx);
        let focus = cx.on_focus(&handle, window, |this, _, cx| {
            if let Some(dialog) = &mut this.workspace_creator {
                dialog.creator.set_focus(CreatorControl::Name);
            }
            cx.notify();
        });
        vec![keys, edits, focus]
    }

    /// A key while the creator is open; returns whether it was the creator's
    /// (typing in the name field is not). Tab and Shift+Tab move the focus,
    /// Esc closes, Enter and Space press the focused control.
    pub(crate) fn on_workspace_creator_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(focus) = self
            .workspace_creator
            .as_ref()
            .map(|dialog| dialog.creator.focus())
        else {
            return false;
        };
        match keystroke.key.as_str() {
            "escape" => {
                self.close_workspace_creator(window, cx);
                true
            }
            "tab" => {
                if let Some(dialog) = &mut self.workspace_creator {
                    dialog.creator.move_focus(!keystroke.modifiers.shift);
                }
                self.apply_workspace_creator_focus(window, cx);
                true
            }
            _ if self.workspace_name_focused(window, cx) => false,
            "enter" | "space" => {
                self.press_workspace_control(focus, window, cx);
                true
            }
            _ => true,
        }
    }

    /// A click on `control`, or Space while it has the focus: the tab
    /// switches, Browse asks for a file and Create submits.
    fn press_workspace_control(
        &mut self,
        control: CreatorControl,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(dialog) = &mut self.workspace_creator {
            match control {
                CreatorControl::FromRepos => dialog.creator.set_mode(CreatorMode::FromRepos),
                CreatorControl::FromVscode => dialog.creator.set_mode(CreatorMode::FromVscode),
                CreatorControl::Name
                | CreatorControl::Browse
                | CreatorControl::Cancel
                | CreatorControl::Create => dialog.creator.set_focus(control),
            }
        }
        match control {
            CreatorControl::FromRepos | CreatorControl::FromVscode | CreatorControl::Name => {
                self.apply_workspace_creator_focus(window, cx);
            }
            CreatorControl::Browse => self.browse_workspace_file(cx),
            CreatorControl::Cancel => self.close_workspace_creator(window, cx),
            CreatorControl::Create => self.submit_workspace_creator(window, cx),
        }
    }

    /// The prompt or the creator, when open, takes the keys it handles.
    /// `false` means the key belongs to a field (the name edit passes through).
    pub(crate) fn on_creator_dialogs_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.vscode_prompt_open() {
            self.on_vscode_prompt_key(keystroke, window, cx);
            return true;
        }
        if self.workspace_creator.is_some() {
            return self.on_workspace_creator_key(keystroke, window, cx);
        }
        false
    }

    /// Ticks or unticks `repo_id` in the From-repos member list.
    fn toggle_workspace_member(&mut self, repo_id: &str, cx: &mut Context<Self>) {
        if let Some(dialog) = &mut self.workspace_creator {
            dialog.creator.toggle_member(repo_id);
            cx.notify();
        }
    }

    /// Browse: the file picker opens at its own remembered folder, else the
    /// last repo folder, and a file it returns is parsed by the daemon.
    fn browse_workspace_file(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = &self.workspace_creator else {
            return;
        };
        if dialog.creator.parsing() {
            return;
        }
        let start = self
            .sidebar
            .last_vscode_dir()
            .or_else(|| self.sidebar.last_repo_dir())
            .map(std::path::PathBuf::from);
        let picked = (self.pick_file)(cx, start);
        cx.spawn(async move |this, cx| {
            let picked = picked.await;
            // Fails only when the view is gone, and its window with it.
            this.update(cx, |this, cx| this.finish_browse_workspace_file(picked, cx))
                .ok();
        })
        .detach();
        cx.notify();
    }

    /// The file picker's answer: the picked file's folder is remembered for
    /// this picker, and the parse goes out under no request id while the
    /// creator waits on `VscodeWorkspaceParsed` (or an `Error`).
    fn finish_browse_workspace_file(
        &mut self,
        picked: Option<std::path::PathBuf>,
        cx: &mut Context<Self>,
    ) {
        let Some(picked) = picked else {
            return;
        };
        let parent = picked
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(&picked);
        let dir = parent.to_string_lossy().into_owned();
        if self.sidebar.set_last_vscode_dir(&dir) {
            self.save_ui();
        }
        let path = picked.to_string_lossy().into_owned();
        {
            let Some(dialog) = &mut self.workspace_creator else {
                return;
            };
            dialog.creator.begin_parse(path.clone());
        }
        self.send(ClientMessage::ParseVscodeWorkspace { path });
        cx.notify();
    }

    /// Create: sends the form and keeps the creator open until the daemon's
    /// workspace list holds the name.
    fn submit_workspace_creator(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let message = {
            let Some(dialog) = &mut self.workspace_creator else {
                return;
            };
            let Some(message) = dialog.creator.submit_message() else {
                return;
            };
            dialog.creator.begin_create();
            message
        };
        self.send(message);
        // A submit from the name field takes the keyboard off the field, so
        // the open creator still answers Esc while it waits.
        self.workspace_creator_focus.focus(window);
        cx.notify();
    }

    /// The daemon's parsed suggestion: fills the folder rows, prefills the
    /// name and keeps the form usable.
    pub(crate) fn on_vscode_parsed(
        &mut self,
        suggestion: VscodeWorkspaceSuggestion,
        cx: &mut Context<Self>,
    ) {
        let Some(dialog) = &mut self.workspace_creator else {
            return;
        };
        if !dialog.creator.parsed(suggestion) {
            // A stale reply for an earlier Browse, or nothing in flight.
            return;
        }
        let name = dialog.creator.name().to_owned();
        let input = dialog.name_input.clone();
        input.update(cx, |input, cx| input.set_text(name, cx));
        cx.notify();
    }

    /// An unrouted `Error` while the creator waits: a pending Create owns it
    /// (Browse may still be in flight beside it), else a pending parse does.
    /// Inline in the creator, and not also the generic toast. Returns whether
    /// the creator took it.
    pub(crate) fn on_creator_error(&mut self, message: &str, cx: &mut Context<Self>) -> bool {
        let Some(dialog) = &mut self.workspace_creator else {
            return false;
        };
        let took = if dialog.creator.has_pending_create() {
            dialog.creator.refuse(message)
        } else {
            dialog.creator.fail_parse(Some(message))
        };
        if took {
            cx.notify();
        }
        took
    }

    /// Folds a `Workspaces` broadcast: closes on the submitted name, and
    /// keeps the creator open, with its form, on a refusal.
    pub(crate) fn settle_workspace_creator(
        &mut self,
        workspaces: &[WorkspaceEntry],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let settled = match self.workspace_creator.as_mut() {
            Some(dialog) => dialog.creator.settle(workspaces),
            None => return,
        };
        match settled {
            Settled::Closed => self.close_workspace_creator(window, cx),
            Settled::Refused(_) | Settled::Pending => cx.notify(),
        }
    }

    /// The keyboard goes where the form's focus is: the name field, else the
    /// dialog itself.
    pub(crate) fn apply_workspace_creator_focus(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(dialog) = &self.workspace_creator else {
            return;
        };
        if dialog.creator.focus() == CreatorControl::Name {
            dialog.name_input.read(cx).focus_handle(cx).focus(window);
        } else {
            self.workspace_creator_focus.focus(window);
        }
        cx.notify();
    }

    /// Whether the name field holds the keyboard.
    fn workspace_name_focused(&self, window: &Window, cx: &Context<Self>) -> bool {
        self.workspace_creator.as_ref().is_some_and(|dialog| {
            dialog
                .name_input
                .read(cx)
                .focus_handle(cx)
                .is_focused(window)
        })
    }

    /// The creator over a backdrop that takes every click beneath it; a click
    /// on the backdrop itself does nothing.
    pub(crate) fn workspace_creator_layer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let dialog = self.workspace_creator.as_ref()?;
        let creator = &dialog.creator;
        let focus = creator.focus();
        let tabs = div()
            .flex()
            .gap(px(4.0))
            .children(CreatorMode::ALL.map(|mode| workspace_tab(mode, creator.mode(), cx)));
        let body = match creator.mode() {
            CreatorMode::FromRepos => {
                repos_body(dialog, self.sidebar.repos(), focus, cx).into_any_element()
            }
            CreatorMode::FromVscode => vscode_body(dialog, focus, cx).into_any_element(),
        };
        let error = creator.error().map(|message| {
            div()
                .debug_selector(|| "workspace-creator-error".to_owned())
                .text_color(gpui::rgb(DANGER))
                .child(message.to_owned())
        });
        let cancel = dialog_button(
            CreatorControl::Cancel.selector(),
            "Cancel".to_owned(),
            false,
            focus == CreatorControl::Cancel,
            true,
        )
        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
            this.press_workspace_control(CreatorControl::Cancel, window, cx);
        }));
        let can_submit = creator.can_submit();
        let create = dialog_button(
            CreatorControl::Create.selector(),
            "Create".to_owned(),
            false,
            focus == CreatorControl::Create,
            can_submit,
        )
        .when(can_submit, |button| {
            button.on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.press_workspace_control(CreatorControl::Create, window, cx);
            }))
        });
        let panel = modal_panel("workspace-creator-panel")
            .track_focus(&self.workspace_creator_focus)
            .child(div().font_weight(FontWeight::SEMIBOLD).child(TITLE))
            .child(tabs)
            .child(body)
            .children(error)
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap(px(6.0))
                    .child(cancel)
                    .child(create),
            );
        Some(backdrop("workspace-creator", panel))
    }

    /// Whether the workspace creator is open.
    #[must_use]
    pub fn workspace_creator_open(&self) -> bool {
        self.workspace_creator.is_some()
    }

    /// The creator's title.
    #[must_use]
    pub fn workspace_creator_title(&self) -> Option<String> {
        self.workspace_creator.as_ref().map(|_| TITLE.to_owned())
    }

    /// The mode tabs as shown: selector and label.
    #[must_use]
    pub fn workspace_creator_tabs(&self) -> Vec<(String, String)> {
        CreatorMode::ALL
            .into_iter()
            .map(|mode| (mode.selector().to_owned(), mode.label().to_owned()))
            .collect()
    }

    /// The selector of the mode on screen.
    #[must_use]
    pub fn workspace_creator_mode(&self) -> Option<&'static str> {
        self.workspace_creator
            .as_ref()
            .map(|dialog| dialog.creator.mode().selector())
    }

    /// The name field's text, read back from its input.
    #[must_use]
    pub fn workspace_creator_name(&self, cx: &App) -> Option<String> {
        self.workspace_creator
            .as_ref()
            .map(|dialog| dialog.name_input.read(cx).text().to_owned())
    }

    /// Whether Create can send.
    #[must_use]
    pub fn workspace_creator_can_submit(&self) -> bool {
        self.workspace_creator
            .as_ref()
            .is_some_and(|dialog| dialog.creator.can_submit())
    }

    /// The From-repos member rows as drawn: selector, name and tick.
    #[must_use]
    pub fn workspace_creator_members(&self) -> Vec<(String, String, bool)> {
        self.workspace_creator
            .as_ref()
            .map_or_else(Vec::new, |dialog| {
                dialog.creator.member_rows(self.sidebar.repos())
            })
    }

    /// The picked file, while one is.
    #[must_use]
    pub fn workspace_creator_file(&self) -> Option<String> {
        self.workspace_creator
            .as_ref()
            .and_then(|dialog| dialog.creator.file().map(str::to_owned))
    }

    /// Whether a parse is in flight.
    #[must_use]
    pub fn workspace_creator_parsing(&self) -> bool {
        self.workspace_creator
            .as_ref()
            .is_some_and(|dialog| dialog.creator.parsing())
    }

    /// The parsed file's folder rows as drawn: label and badge.
    #[must_use]
    pub fn workspace_creator_folder_rows(&self) -> Vec<(String, String)> {
        self.workspace_creator
            .as_ref()
            .map_or_else(Vec::new, |dialog| {
                dialog
                    .creator
                    .folder_rows()
                    .into_iter()
                    .map(|row| {
                        let badge = row.badge().to_owned();
                        (row.label, badge)
                    })
                    .collect()
            })
    }

    /// The inline error, while one shows.
    #[must_use]
    pub fn workspace_creator_error(&self) -> Option<String> {
        self.workspace_creator
            .as_ref()
            .and_then(|dialog| dialog.creator.error().map(str::to_owned))
    }

    /// The selector of the focused control.
    #[must_use]
    pub fn workspace_creator_focus(&self) -> Option<String> {
        self.workspace_creator
            .as_ref()
            .map(|dialog| dialog.creator.focus().selector().to_owned())
    }

    /// Every selector the open creator draws, for the spec harness.
    #[must_use]
    pub fn workspace_creator_controls(&self) -> Vec<(String, String)> {
        let Some(dialog) = &self.workspace_creator else {
            return Vec::new();
        };
        let mut controls: Vec<(String, String)> = CreatorMode::ALL
            .into_iter()
            .map(|mode| (mode.selector().to_owned(), mode.label().to_owned()))
            .collect();
        controls.push((
            CreatorControl::Name.selector().to_owned(),
            "Name".to_owned(),
        ));
        controls.push((
            CreatorControl::Browse.selector().to_owned(),
            "Browse…".to_owned(),
        ));
        controls.push((
            "workspace-creator-file".to_owned(),
            dialog.creator.file().unwrap_or(NO_FILE).to_owned(),
        ));
        controls.extend(
            dialog
                .creator
                .member_rows(self.sidebar.repos())
                .into_iter()
                .map(|(selector, label, _)| (selector, label)),
        );
        for (index, row) in dialog.creator.folder_rows().iter().enumerate() {
            controls.push((
                format!("workspace-creator-folder-{index}"),
                format!("{} {}", row.label, row.badge()),
            ));
        }
        controls.push((
            CreatorControl::Cancel.selector().to_owned(),
            "Cancel".to_owned(),
        ));
        controls.push((
            CreatorControl::Create.selector().to_owned(),
            "Create".to_owned(),
        ));
        controls
    }
}

/// One mode tab, lit while it is the mode on screen.
fn workspace_tab(
    mode: CreatorMode,
    active: CreatorMode,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    let control = match mode {
        CreatorMode::FromRepos => CreatorControl::FromRepos,
        CreatorMode::FromVscode => CreatorControl::FromVscode,
    };
    let lit = mode == active;
    div()
        .id(mode.selector())
        .debug_selector(move || mode.selector().to_owned())
        .px(px(8.0))
        .py(px(3.0))
        .rounded(px(4.0))
        .cursor_pointer()
        .text_color(gpui::rgb(if lit { TEXT } else { MUTED }))
        .when(lit, |tab| tab.bg(gpui::rgb(crate::HOVER_BG)))
        .child(mode.label())
        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
            this.press_workspace_control(control, window, cx);
        }))
}

/// The From-repos body: the name field and one row per registered repo.
fn repos_body(
    dialog: &WorkspaceCreatorDialog,
    repos: &[RepoEntry],
    focus: CreatorControl,
    cx: &mut Context<RootView>,
) -> Div {
    let rows = dialog.creator.member_rows(repos);
    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(div().text_color(gpui::rgb(MUTED)).child("Name"))
        .child(name_box(dialog, focus))
        .child(div().text_color(gpui::rgb(MUTED)).child(MEMBERS_LABEL))
        .when(rows.is_empty(), |body| {
            body.child(
                div()
                    .debug_selector(|| "workspace-creator-no-repos".to_owned())
                    .text_color(gpui::rgb(MUTED))
                    .child(NO_REPOS_NOTE),
            )
        })
        .children(rows.into_iter().map(|(selector, name, checked)| {
            let repo_id = selector
                .strip_prefix("workspace-creator-member-")
                .unwrap_or_default()
                .to_owned();
            checkbox_row(&selector, checked, false, name).on_click(cx.listener(
                move |this, _: &ClickEvent, _, cx| this.toggle_workspace_member(&repo_id, cx),
            ))
        }))
}

/// The From-VS-Code body: Browse, the parse's state, and the parsed file's
/// name field and folder rows.
fn vscode_body(
    dialog: &WorkspaceCreatorDialog,
    focus: CreatorControl,
    cx: &mut Context<RootView>,
) -> Div {
    let creator = &dialog.creator;
    let file = creator.file().unwrap_or(NO_FILE);
    let browse = dialog_button(
        CreatorControl::Browse.selector(),
        "Browse…".to_owned(),
        false,
        focus == CreatorControl::Browse,
        !creator.parsing(),
    )
    .when(!creator.parsing(), |button| {
        button.on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
            this.press_workspace_control(CreatorControl::Browse, window, cx);
        }))
    });
    let parsed = creator.suggestion().map(|_| {
        let rows = creator.folder_rows();
        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                div()
                    .text_color(gpui::rgb(MUTED))
                    .child(WORKSPACE_NAME_LABEL),
            )
            .child(name_box(dialog, focus))
            .child(
                div()
                    .text_color(gpui::rgb(MUTED))
                    .child(format!("Folders ({})", rows.len())),
            )
            .children(rows.into_iter().enumerate().map(|(index, row)| {
                let badge = row.badge();
                div()
                    .debug_selector(move || format!("workspace-creator-folder-{index}"))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(row.label)
                    .child(div().text_color(gpui::rgb(MUTED)).child(badge))
            }))
    });
    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(div().text_color(gpui::rgb(MUTED)).child(FILE_LABEL))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .child(
                    div()
                        .debug_selector(|| "workspace-creator-file".to_owned())
                        .flex_1()
                        .child(file.to_owned()),
                )
                .child(browse),
        )
        .when(creator.parsing(), |body| {
            body.child(
                div()
                    .debug_selector(|| "workspace-creator-parsing".to_owned())
                    .text_color(gpui::rgb(MUTED))
                    .child(PARSING),
            )
        })
        .children(parsed)
}

/// The name field in its box.
fn name_box(dialog: &WorkspaceCreatorDialog, focus: CreatorControl) -> Div {
    div()
        .debug_selector(|| CreatorControl::Name.selector().to_owned())
        .flex()
        .flex_1()
        .px(px(6.0))
        .py(px(3.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(gpui::rgb(if focus == CreatorControl::Name {
            TEXT
        } else {
            BORDER
        }))
        .child(dialog.name_input.clone())
}
