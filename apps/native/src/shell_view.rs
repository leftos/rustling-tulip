//! The Shell… dialog on screen — its folder field, Browse, the checkbox, the
//! clear link and its buttons — and the quick "+ Shell" spawn. Its state and
//! every rule live in [`crate::shell_dialog`]; spawning goes through
//! [`RootView::spawn`].

use gpui::{
    AnyElement, App, ClickEvent, Context, Div, Entity, Focusable as _, FontWeight, Keystroke,
    Stateful, Subscription, Window, div, prelude::*, px,
};

use crate::notice_view::modal_panel;
use crate::session_menu::{backdrop, dialog_button, dialog_look};
use crate::shell_dialog::{
    FOLDER_PLACEHOLDER, ShellControl, ShellForm, host_placeholder, standalone_shell_request,
};
use crate::spawn_view::{Look, checkbox_row, choice_button, segmented};
use crate::spawns::OpenIn;
use crate::target::HostKey;
use crate::text_input::{TextChanged, TextInput, TextInputEvent};
use crate::ui_theme::{UiTheme, theme};
use crate::{Pick, RootView, tooltip};

const TITLE: &str = "Open a shell";
const SAVE_DEFAULT_LABEL: &str = "Use as quick shell default";
const CLEAR_DEFAULT_LABEL: &str = "Use home folder for + Shell";

/// The open Shell… dialog: the host the shell opens on, its form and its
/// folder field.
pub(crate) struct ShellDialog {
    host: HostKey,
    form: ShellForm,
    folder_input: Entity<TextInput>,
    _subscriptions: Vec<Subscription>,
}

impl RootView {
    /// Whether the Shell… dialog is open.
    #[must_use]
    pub fn shell_dialog_open(&self) -> bool {
        self.shell_dialog.is_some()
    }

    /// The text of the folder field, while the dialog is open.
    #[must_use]
    pub fn shell_dialog_folder(&self, cx: &App) -> Option<String> {
        let dialog = self.shell_dialog.as_ref()?;
        Some(shell_folder(dialog, cx).to_owned())
    }

    /// The selector of the dialog's focused control.
    #[must_use]
    pub fn shell_dialog_focus(&self, cx: &App) -> Option<&'static str> {
        let dialog = self.shell_dialog.as_ref()?;
        let folder = shell_folder(dialog, cx);
        let focused = dialog.form.focused(folder, self.shell_browse_offered());
        Some(focused.selector())
    }

    /// Whether Open shell can go, while the dialog is open.
    #[must_use]
    pub fn shell_dialog_can_submit(&self, cx: &App) -> bool {
        let Some(dialog) = self.shell_dialog.as_ref() else {
            return false;
        };
        dialog.form.can_submit(shell_folder(dialog, cx))
    }

    /// The hint under the folder field, while the folder is something the
    /// dialog cannot open: not a full path, or on a remote host a path with
    /// a `..` segment.
    #[must_use]
    pub fn shell_dialog_hint(&self, cx: &App) -> Option<String> {
        let dialog = self.shell_dialog.as_ref()?;
        dialog.form.hint(shell_folder(dialog, cx))
    }

    /// Whether the dialog would remember its folder for the quick shell.
    #[must_use]
    pub fn shell_dialog_saves_default(&self) -> bool {
        self.shell_dialog
            .as_ref()
            .is_some_and(|dialog| dialog.form.save_default())
    }

    /// Whether the dialog offers to go back to the home folder.
    #[must_use]
    pub fn shell_dialog_clears_default(&self) -> bool {
        self.shell_dialog
            .as_ref()
            .is_some_and(|dialog| dialog.form.remembered())
    }

    /// The remembered quick-shell folder, a folder of this PC: only This
    /// PC's daemon is offered it.
    pub(crate) fn quick_shell_default(&self, host: &HostKey) -> Option<&str> {
        self.sidebar
            .quick_shell_dir()
            .filter(|_| *host == HostKey::Local)
    }

    /// The toolbar's "+ Shell" tip: where the quick shell opens on the
    /// focused session's host, else on This PC.
    pub(crate) fn quick_shell_tip(&self) -> String {
        let host = self.focused_host();
        if host != HostKey::Local {
            let name = self.host_name(&host);
            return format!("Open a standalone shell in {name}'s home folder");
        }
        match self.quick_shell_default(&host) {
            Some(dir) => format!("Open a standalone shell in {dir}"),
            None => "Open a standalone shell in your home folder".to_owned(),
        }
    }

    /// "+ Shell": the quick shell on the focused session's host, else on
    /// This PC.
    pub(crate) fn quick_shell(&mut self, cx: &mut Context<Self>) {
        let host = self.focused_host();
        self.quick_shell_on(&host, cx);
    }

    /// The quick shell on `host`, in the tab on screen: This PC's opens in
    /// the remembered folder, else in the daemon's own default; a remote
    /// host's sends no folder, so its daemon picks its own default. An
    /// offline host refuses with the offline toast.
    pub(crate) fn quick_shell_on(&mut self, host: &HostKey, cx: &mut Context<Self>) {
        let cwd = self.quick_shell_default(host).map(str::to_owned);
        let open_in = self.open_in_active_tab();
        self.spawn_on(host, standalone_shell_request(cwd), open_in, cx);
        cx.notify();
    }

    /// "Shell…": opens the dialog, unless the connection is down or another
    /// dialog, menu or modal notice is up. No repo is needed.
    pub(crate) fn open_shell_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let blocked = self.shell_dialog.is_some()
            || self.fetch_dialog.is_some()
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
            || self.conn().overlay().is_some();
        if blocked {
            return;
        }
        let host = self.focused_host();
        self.shell_generation += 1;
        let (form, placeholder) = self.shell_form(&host);
        let folder = self
            .quick_shell_default(&host)
            .filter(|_| form.offers_default())
            .map(str::to_owned)
            .unwrap_or_default();
        let folder_input = cx.new(|cx| TextInput::new(folder, placeholder, cx));
        let subscriptions = Self::watch_shell_field(&folder_input, window, cx);
        self.renaming = None;
        self.close_flyout();
        self.close_tab_menu(window, cx);
        self.shell_dialog = Some(ShellDialog {
            host,
            form,
            folder_input,
            _subscriptions: subscriptions,
        });
        self.apply_shell_focus(window, cx);
    }

    /// The form numbered [`Self::shell_generation`] for a shell on `host`,
    /// and its folder field's placeholder: on a host whose files this PC
    /// opens, the quick-shell default and this PC's path rules; elsewhere
    /// the host's path rules and no default.
    fn shell_form(&self, host: &HostKey) -> (ShellForm, String) {
        if self.capabilities_of(host).opens_local_files() {
            let remembered = self.quick_shell_default(host);
            (
                ShellForm::new(self.shell_generation, remembered),
                FOLDER_PLACEHOLDER.to_owned(),
            )
        } else {
            let name = self.host_name(host);
            (
                ShellForm::without_default(self.shell_generation, &name),
                host_placeholder(&name),
            )
        }
    }

    /// The host row's choice: the dialog's form is rebuilt for `host` under
    /// a new number, so a folder picker still open answers nothing, and
    /// the typed folder stays.
    fn set_shell_host(&mut self, host: HostKey, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .shell_dialog
            .as_ref()
            .is_none_or(|dialog| dialog.host == host)
        {
            return;
        }
        self.shell_generation += 1;
        let (mut form, placeholder) = self.shell_form(&host);
        if let Some(dialog) = &mut self.shell_dialog {
            form.keep_save_default(&dialog.form);
            dialog.host = host;
            dialog.form = form;
            let input = dialog.folder_input.clone();
            input.update(cx, |input, cx| input.set_placeholder(placeholder, cx));
        }
        self.apply_shell_focus(window, cx);
    }

    /// Closes the dialog and hands the keyboard back to the active pane.
    pub(crate) fn close_shell_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.shell_dialog.take().is_some() {
            self.focus_active_pane(window, cx);
            cx.notify();
        }
    }

    /// Enter submits (through the field's own event), Esc closes, an edit
    /// redraws the dialog (the folder is what Submit and the hint are judged
    /// by) and a click in the field moves the form's focus there.
    fn watch_shell_field(
        input: &Entity<TextInput>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Subscription> {
        let keys = cx.subscribe_in(
            input,
            window,
            |this, _, event: &TextInputEvent, window, cx| match event {
                TextInputEvent::Submit => this.submit_shell_dialog(window, cx),
                TextInputEvent::Cancel => this.close_shell_dialog(window, cx),
            },
        );
        let edits = cx.subscribe_in(input, window, |_, _, _: &TextChanged, _, cx| {
            cx.notify();
        });
        let handle = input.read(cx).focus_handle(cx);
        let focus = cx.on_focus(&handle, window, |this, _, cx| {
            if let Some(dialog) = &mut this.shell_dialog {
                dialog.form.set_focus(ShellControl::Folder);
            }
            cx.notify();
        });
        vec![keys, edits, focus]
    }

    /// Open one: the shell goes out to the dialog's host under a request
    /// id, and the folder is remembered, for This PC alone, only once that
    /// spawn lands. An offline host refuses with the offline toast.
    fn submit_shell_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(folder) = self.shell_dialog_folder(cx) else {
            return;
        };
        if let Some(host) = self.shell_dialog.as_ref().map(|dialog| dialog.host.clone())
            && self.is_remote_offline(&host)
        {
            self.push_offline_toast(&host, cx);
            return;
        }
        let Some((host, submission)) = self.shell_dialog.as_mut().and_then(|dialog| {
            let host = dialog.host.clone();
            dialog
                .form
                .submit(&folder)
                .map(|submission| (host, submission))
        }) else {
            return;
        };
        self.close_shell_dialog(window, cx);
        let open_in = self.open_in_active_tab();
        let request_id = self.spawn_on(
            &host,
            standalone_shell_request(Some(submission.folder.clone())),
            open_in,
            cx,
        );
        if submission.save_default && host == HostKey::Local {
            self.quick_shell_saves.arm(&request_id, &submission.folder);
        }
    }

    /// "Use home folder for + Shell": forgets the remembered folder and
    /// closes, spawning nothing.
    fn clear_quick_shell_default(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.sidebar.set_quick_shell_dir(None) {
            self.save_ui();
        }
        self.close_shell_dialog(window, cx);
    }

    /// Where a spawn asked for the current tab opens: the tab on screen when
    /// it can hold panes, else a tab of its own — the spawn dialog's own
    /// rule, read from the tab itself.
    fn open_in_active_tab(&self) -> OpenIn {
        match self.tabs.active_tab().filter(|tab| tab.grid().is_some()) {
            Some(tab) => OpenIn::CurrentTab(tab.id.clone()),
            None => OpenIn::NewTab,
        }
    }

    /// A key while the dialog is open; returns whether it was the dialog's
    /// (every key but typing in the folder field is). Tab and Shift+Tab move
    /// the focus, Esc closes, Space presses the focused button.
    pub(crate) fn on_shell_dialog_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let key = keystroke.key.as_str();
        let Some(folder) = self.shell_dialog_folder(cx) else {
            return false;
        };
        let browse = self.shell_browse_offered();
        let Some(focused) = self
            .shell_dialog
            .as_ref()
            .map(|dialog| dialog.form.focused(&folder, browse))
        else {
            return false;
        };
        match key {
            "tab" => {
                if let Some(dialog) = &mut self.shell_dialog {
                    dialog
                        .form
                        .move_focus(!keystroke.modifiers.shift, &folder, browse);
                }
                self.apply_shell_focus(window, cx);
                true
            }
            "escape" => {
                self.close_shell_dialog(window, cx);
                true
            }
            _ if self.shell_field_focused(window, cx) => false,
            "enter" | "space" => {
                self.press_shell_control(focused, window, cx);
                true
            }
            _ => true,
        }
    }

    /// A click on `control`, or Space while it has the focus; a click moves
    /// the keyboard to that control, as the spawn dialog's does.
    fn press_shell_control(
        &mut self,
        control: ShellControl,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(dialog) = &mut self.shell_dialog {
            dialog.form.set_focus(control);
        }
        match control {
            ShellControl::Close | ShellControl::Cancel => self.close_shell_dialog(window, cx),
            ShellControl::Submit => self.submit_shell_dialog(window, cx),
            ShellControl::ClearDefault => self.clear_quick_shell_default(window, cx),
            ShellControl::Browse => self.browse_for_folder(cx),
            ShellControl::SaveDefault => {
                if let Some(dialog) = &mut self.shell_dialog {
                    dialog.form.toggle_save_default();
                }
                self.apply_shell_focus(window, cx);
            }
            ShellControl::Folder => self.apply_shell_focus(window, cx),
        }
    }

    /// "Browse…": the folder the OS picker returns replaces the field, which
    /// happens only while this dialog is the one that asked and the field
    /// still holds what it held then. Browse is disabled meanwhile.
    fn browse_for_folder(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = &self.shell_dialog else {
            return;
        };
        if dialog.form.browsing() || !self.shell_browse_offered() {
            return;
        }
        let generation = dialog.form.generation();
        let folder = shell_folder(dialog, cx).to_owned();
        // The picker opens at the typed folder, else the remembered one.
        let typed = folder.trim();
        let start = if typed.is_empty() {
            self.sidebar.quick_shell_dir().map(std::path::PathBuf::from)
        } else {
            Some(std::path::PathBuf::from(typed))
        };
        let asked = self.pick_local(Pick::Folder, start, cx, move |this, picked, cx| {
            let picked = picked.map(|path| path.to_string_lossy().into_owned());
            this.finish_browse(generation, picked.as_deref(), cx);
        });
        if asked && let Some(dialog) = &mut self.shell_dialog {
            dialog.form.begin_browse(&folder);
        }
        cx.notify();
    }

    /// The folder picker's answer for dialog `generation`, applied when it is
    /// still the dialog on screen with its folder unchanged.
    fn finish_browse(&mut self, generation: u64, picked: Option<&str>, cx: &mut Context<Self>) {
        let Some(dialog) = &mut self.shell_dialog else {
            return;
        };
        let current = shell_folder(dialog, cx).to_owned();
        let applied = dialog.form.finish_browse(generation, &current, picked);
        if let Some(folder) = applied {
            let input = dialog.folder_input.clone();
            input.update(cx, |input, cx| input.set_text(folder, cx));
        }
        cx.notify();
    }

    /// The keyboard goes where the form's focus is: the folder field, else
    /// the dialog itself.
    pub(crate) fn apply_shell_focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = &self.shell_dialog else {
            return;
        };
        let focus = dialog
            .form
            .focused(shell_folder(dialog, cx), self.shell_browse_offered());
        match focus {
            ShellControl::Folder => dialog.folder_input.read(cx).focus_handle(cx).focus(window),
            _ => self.shell_focus.focus(window),
        }
        cx.notify();
    }

    /// Whether Browse is offered: this PC's folder picker answers only for
    /// a dialog on a host whose files this PC opens.
    fn shell_browse_offered(&self) -> bool {
        self.shell_dialog
            .as_ref()
            .is_some_and(|dialog| self.capabilities_of(&dialog.host).opens_local_files())
    }

    /// Whether the folder field holds the keyboard.
    fn shell_field_focused(&self, window: &Window, cx: &Context<Self>) -> bool {
        self.shell_dialog.as_ref().is_some_and(|dialog| {
            dialog
                .folder_input
                .read(cx)
                .focus_handle(cx)
                .is_focused(window)
        })
    }

    /// The dialog over a backdrop that takes every click beneath it; a click
    /// on the backdrop itself does nothing.
    pub(crate) fn shell_dialog_layer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = theme(cx);
        let dialog = self.shell_dialog.as_ref()?;
        let form = &dialog.form;
        let folder = shell_folder(dialog, cx).to_owned();
        let focus = form.focused(&folder, self.shell_browse_offered());
        let close = dialog_button(
            ShellControl::Close.selector(),
            "✕".to_owned(),
            dialog_look(
                crate::fonts::ui_text_size(cx),
                crate::fonts::ui_weight_shift(cx),
                false,
                true,
                &theme(cx),
            ),
            focus == ShellControl::Close,
        )
        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
            this.close_shell_dialog(window, cx);
        }));
        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .child(
                div()
                    .font_weight(crate::fonts::ui_weight(cx, FontWeight::SEMIBOLD))
                    .child(TITLE),
            )
            .child(close);
        let refusal = self.capabilities_of(&dialog.host).local_files_refusal();
        let browse_enabled = !form.browsing() && refusal.is_none();
        let browse = dialog_button(
            ShellControl::Browse.selector(),
            "Browse…".to_owned(),
            dialog_look(
                crate::fonts::ui_text_size(cx),
                crate::fonts::ui_weight_shift(cx),
                false,
                browse_enabled,
                &theme(cx),
            ),
            focus == ShellControl::Browse,
        )
        .when_some(refusal, |button, tip| button.tooltip(tooltip(tip)))
        .when(browse_enabled, |button| {
            button.on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.press_shell_control(ShellControl::Browse, window, cx);
            }))
        });
        let field = div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(div().text_color(gpui::rgb(t.muted)).child("Folder"))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(input_box(dialog, focus, &t))
                    .child(browse),
            )
            .children(hint_row(form, &folder, &t));
        let clear = form.remembered().then(|| {
            link_button(
                ShellControl::ClearDefault.selector(),
                CLEAR_DEFAULT_LABEL,
                focus == ShellControl::ClearDefault,
                &t,
            )
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.press_shell_control(ShellControl::ClearDefault, window, cx);
            }))
        });
        let hosts = self.connected_hosts();
        let host_row = (hosts.len() > 1).then(|| shell_host_row(&dialog.host, hosts, cx));
        let panel = modal_panel(crate::fonts::ui_text_size(cx), "shell-panel", &theme(cx))
            .track_focus(&self.shell_focus)
            .child(header)
            .children(host_row)
            .child(field)
            .children(
                form.offers_default()
                    .then(|| save_default_row(form, focus, cx)),
            )
            .children(clear)
            .child(footer(form.can_submit(&folder), focus, cx));
        Some(backdrop("shell-dialog", panel, &theme(cx)))
    }
}

/// The Host row: one segment per held host, `selected` lit; a click moves
/// the dialog to that host. It is not a keyboard stop.
fn shell_host_row(selected: &HostKey, hosts: Vec<HostKey>, cx: &mut Context<RootView>) -> Div {
    let t = theme(cx);
    let offset = crate::fonts::ui_text_size(cx);
    let buttons = hosts
        .into_iter()
        .map(|host| {
            let look = Look {
                selected: host == *selected,
                focused: false,
                enabled: true,
            };
            let selector = format!("shell-host-{}", host.wire().unwrap_or("local"));
            let label = crate::sidebar::host_label(&host);
            choice_button(
                offset,
                selector,
                label,
                look,
                cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.set_shell_host(host.clone(), window, cx);
                }),
                &t,
            )
            .into_any_element()
        })
        .collect();
    div()
        .debug_selector(|| "shell-host-row".to_owned())
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(div().text_color(gpui::rgb(t.muted)).child("Host"))
        .child(div().flex().child(segmented(buttons, &t)))
}

/// The folder field's text, while the dialog is open.
fn shell_folder<'a>(dialog: &'a ShellDialog, cx: &'a App) -> &'a str {
    dialog.folder_input.read(cx).text()
}

/// The folder field in its box.
fn input_box(dialog: &ShellDialog, focus: ShellControl, t: &UiTheme) -> Div {
    div()
        .debug_selector(|| ShellControl::Folder.selector().to_owned())
        .flex()
        .flex_1()
        .px(px(6.0))
        .py(px(3.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(gpui::rgb(if focus == ShellControl::Folder {
            t.text
        } else {
            t.border
        }))
        .child(dialog.folder_input.clone())
}

/// What the folder field says about a text that is not yet a full path.
fn hint_row(form: &ShellForm, folder: &str, t: &UiTheme) -> Option<Div> {
    form.hint(folder).map(|hint| {
        div()
            .debug_selector(|| "shell-path-hint".to_owned())
            .text_color(gpui::rgb(t.muted))
            .child(hint)
    })
}

/// The "Use as quick shell default" checkbox, ticked or not.
fn save_default_row(
    form: &ShellForm,
    focus: ShellControl,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    checkbox_row(
        crate::fonts::ui_text_size(cx),
        crate::fonts::ui_weight_shift(cx),
        ShellControl::SaveDefault.selector(),
        form.save_default(),
        focus == ShellControl::SaveDefault,
        SAVE_DEFAULT_LABEL,
        &theme(cx),
    )
    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
        this.press_shell_control(ShellControl::SaveDefault, window, cx);
    }))
}

/// A link-style button: plain text that lights up under the pointer.
fn link_button(
    selector: &'static str,
    label: &'static str,
    focused: bool,
    t: &UiTheme,
) -> Stateful<Div> {
    div()
        .id(selector)
        .debug_selector(move || selector.to_owned())
        .px(px(4.0))
        .py(px(2.0))
        .rounded(px(4.0))
        .text_color(gpui::rgb(if focused { t.text } else { t.muted }))
        .cursor_pointer()
        .hover(|style| {
            style
                .bg(gpui::rgb(t.popover_hover))
                .text_color(gpui::rgb(t.text))
        })
        .child(label)
}

/// Cancel, and Open shell while a folder is there to open (`can_submit`).
fn footer(can_submit: bool, focus: ShellControl, cx: &mut Context<RootView>) -> Div {
    let cancel = dialog_button(
        ShellControl::Cancel.selector(),
        "Cancel".to_owned(),
        dialog_look(
            crate::fonts::ui_text_size(cx),
            crate::fonts::ui_weight_shift(cx),
            false,
            true,
            &theme(cx),
        ),
        focus == ShellControl::Cancel,
    )
    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
        this.press_shell_control(ShellControl::Cancel, window, cx);
    }));
    let submit = dialog_button(
        ShellControl::Submit.selector(),
        "Open shell".to_owned(),
        dialog_look(
            crate::fonts::ui_text_size(cx),
            crate::fonts::ui_weight_shift(cx),
            false,
            can_submit,
            &theme(cx),
        ),
        focus == ShellControl::Submit,
    )
    .when(can_submit, |button| {
        button.on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
            this.press_shell_control(ShellControl::Submit, window, cx);
        }))
    });
    div()
        .flex()
        .justify_end()
        .gap(px(6.0))
        .child(cancel)
        .child(submit)
}
