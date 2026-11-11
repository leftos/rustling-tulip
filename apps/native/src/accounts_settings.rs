//! The Settings › Accounts page: the saved accounts of the host Settings
//! shows (`settings_host`), one row each with Switch, the alias field, Up
//! and Down and Remove, then Add current login, Log in a new account and
//! its Cancel, the running login's status line, and the auto-switch block.
//! Every control sends its account write at once through
//! [`RootView::send_account_write`] (never Apply), except Remove, which
//! asks in a confirm first; the page shows what the next `Accounts` or
//! `AccountsUpdated` carries, and every write control waits while a write
//! is pending. The texts and the rows' actions live in [`crate::accounts`];
//! this module only draws them and forwards presses.

use chrono::Utc;
use gpui::{
    AnyElement, App, ClickEvent, Context, Div, Entity, FocusHandle, Focusable as _, FontWeight,
    Keystroke, Stateful, Subscription, Window, div, prelude::*, px, relative,
};
use protocol::{
    AccountSnapshot, AutoSwitchStrategy, ClientMessage, ConnectionOrigin, FEATURE_ACCOUNTS,
};

use crate::accounts::{
    self, AliasChange, Level, Removal, Row, RowActions, SWITCH_FIRST, Tag, WriteAction,
};
use crate::buttons::{Layer, field_frame};
use crate::driver_id::DriverId as _;
use crate::fonts::{ui_px, ui_text_size, ui_weight, ui_weight_shift};
use crate::notice_view::modal_panel;
use crate::section::section;
use crate::session_menu::{backdrop, dialog_button, dialog_look};
use crate::settings_view::{
    RowLock, SettingsControl, SettingsTab, ToggleState, choice_row, hint, input_frame, intern,
    settings_button, toggle,
};
use crate::spawn_view::{DISABLED_OPACITY, field};
use crate::target::HostKey;
use crate::text_input::{TextInput, TextInputEvent};
use crate::ui_theme::{UiTheme, theme};
use crate::{RootView, tagged_text, tooltip};

/// Why an admin control is disabled on any host but this machine.
const REMOTE_ORIGIN_TIP: &str = "Only on the machine running the daemon";
/// The auto-switch block's note while the host reports no engine.
const AUTO_MISSING: &str = "Auto-switch is not available on this host";
const AUTO_TOGGLE_LABEL: &str = "Switch accounts automatically";
const ADD_LOGIN_LABEL: &str = "Add current login";
const LOG_IN_LABEL: &str = "Log in a new account…";
const CANCEL_LOGIN_LABEL: &str = "Cancel login";
const UP_LABEL: &str = "Up";
const DOWN_LABEL: &str = "Down";
const REMOVE_LABEL: &str = "Remove";
/// What the Remove confirm says removing a saved login does.
const REMOVE_CONFIRM_BODY: &str =
    "Its saved login is deleted from this computer. You can add it again by logging in.";
/// The section headings.
const TABLE_SECTION: &str = "Accounts";
const AUTO_SECTION: &str = "Auto-switch";
/// An alias field's width, in px: an input's width, fixed so the rows line up.
const ALIAS_WIDTH: f32 = 160.0;
/// A row bar's size, in px.
const ROW_BAR: (f32, f32) = (120.0, 6.0);
/// The opacity of a row out of rotation or signed out.
const DIM_ROW: f32 = 0.6;

/// A control of the Accounts page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AccountsControl {
    Switch(u32),
    Alias(u32),
    Up(u32),
    Down(u32),
    Remove(u32),
    AddLogin,
    LogIn,
    CancelLogin,
    AutoSwitch,
    ThresholdDown,
    ThresholdUp,
    HysteresisDown,
    HysteresisUp,
    /// A Cooldown segment, in seconds.
    Cooldown(u64),
    Strategy(AutoSwitchStrategy),
    /// A Forecast window segment, in minutes.
    Window(u32),
}

impl AccountsControl {
    /// The control's selector: `settings-accounts-…`, a row's ending in its
    /// account's number.
    pub(crate) fn selector_text(self) -> String {
        let part = match self {
            Self::Switch(n) => format!("switch-{n}"),
            Self::Alias(n) => format!("alias-{n}"),
            Self::Up(n) => format!("up-{n}"),
            Self::Down(n) => format!("down-{n}"),
            Self::Remove(n) => format!("remove-{n}"),
            Self::AddLogin => "add-login".to_owned(),
            Self::LogIn => "login".to_owned(),
            Self::CancelLogin => "cancel-login".to_owned(),
            Self::AutoSwitch => "auto".to_owned(),
            Self::ThresholdDown => "threshold-down".to_owned(),
            Self::ThresholdUp => "threshold-up".to_owned(),
            Self::HysteresisDown => "hysteresis-down".to_owned(),
            Self::HysteresisUp => "hysteresis-up".to_owned(),
            Self::Cooldown(secs) => format!("cooldown-{secs}"),
            Self::Strategy(AutoSwitchStrategy::Best) => "strategy-best".to_owned(),
            Self::Strategy(AutoSwitchStrategy::ConsumeFirst) => "strategy-consume-first".to_owned(),
            Self::Strategy(AutoSwitchStrategy::Unknown) => "strategy-unknown".to_owned(),
            Self::Window(minutes) => format!("window-{minutes}"),
        };
        format!("settings-accounts-{part}")
    }

    /// The Settings control it is.
    pub(crate) fn control(self) -> SettingsControl {
        SettingsControl::Accounts {
            control: self,
            selector: intern(self.selector_text()),
        }
    }

    /// Whether it is an admin control, which every host but this machine
    /// disables.
    const fn admin(self) -> bool {
        matches!(
            self,
            Self::Alias(_)
                | Self::Up(_)
                | Self::Down(_)
                | Self::Remove(_)
                | Self::AddLogin
                | Self::LogIn
                | Self::CancelLogin
        )
    }
}

/// A button of the Remove confirm, in Tab order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoveButton {
    Cancel,
    Remove,
}

impl RemoveButton {
    pub(crate) const ALL: [Self; 2] = [Self::Cancel, Self::Remove];

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Cancel => "Cancel",
            Self::Remove => REMOVE_LABEL,
        }
    }

    pub(crate) const fn selector(self) -> &'static str {
        match self {
            Self::Cancel => "account-remove-confirm-cancel",
            Self::Remove => "account-remove-confirm-remove",
        }
    }

    const fn other(self) -> Self {
        match self {
            Self::Cancel => Self::Remove,
            Self::Remove => Self::Cancel,
        }
    }
}

/// The open Remove confirm: the account it would remove, the name its title
/// shows, and the button with the keyboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoveConfirm {
    number: u32,
    label: String,
    focused: RemoveButton,
}

impl RemoveConfirm {
    /// The confirm for account `number`, shown as `label`: its alias, else
    /// its email. Cancel has the keyboard when it opens.
    pub(crate) fn open(number: u32, label: String) -> Self {
        Self {
            number,
            label,
            focused: RemoveButton::Cancel,
        }
    }

    /// `Remove {label}?`
    pub(crate) fn title(&self) -> String {
        format!("Remove {}?", self.label)
    }

    pub(crate) const fn number(&self) -> u32 {
        self.number
    }

    pub(crate) const fn focused(&self) -> RemoveButton {
        self.focused
    }

    /// A key while the confirm is open; returns the button it presses. Esc
    /// presses Cancel, Enter the focused button, and Tab or Shift+Tab moves
    /// the focus to the other button.
    fn key(&mut self, key: &str) -> Option<RemoveButton> {
        match key {
            "escape" => Some(RemoveButton::Cancel),
            "enter" => Some(self.focused),
            "tab" => {
                self.focused = self.focused.other();
                None
            }
            _ => None,
        }
    }

    /// A press of `pressed`, with `writing` saying an account write is
    /// pending: the account whose removal Remove sends, or nothing. Remove
    /// waits for the pending write, which leaves the confirm open and its
    /// Remove disabled until the write's answer arrives; Cancel is the
    /// caller's to close.
    pub(crate) fn press(&mut self, pressed: RemoveButton, writing: bool) -> Option<u32> {
        match pressed {
            RemoveButton::Cancel => None,
            RemoveButton::Remove if writing => None,
            RemoveButton::Remove => Some(self.number),
        }
    }
}

/// One account's alias field and the alias it last showed from the host.
pub(crate) struct AliasField {
    number: u32,
    input: Entity<TextInput>,
    focus: FocusHandle,
    /// The host's alias the field was last set to.
    base: String,
    _subscriptions: Vec<Subscription>,
}

/// The Accounts page's alias fields, one per account it lists.
#[derive(Default)]
pub(crate) struct AliasFields(Vec<AliasField>);

impl std::fmt::Debug for AliasFields {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.0.iter().map(|field| (field.number, &field.base)))
            .finish()
    }
}

impl AliasFields {
    fn get(&self, number: u32) -> Option<&AliasField> {
        self.0.iter().find(|field| field.number == number)
    }
}

/// What every control of the page is drawn with.
#[derive(Clone, Copy)]
struct Page {
    /// An account write is pending.
    writing: bool,
    /// The admin controls are locked: the host is not this machine, or its
    /// daemon calls this connection remote.
    locked: bool,
    focused: Option<SettingsControl>,
}

/// A −/+ stepper row: its label, the value shown and the selector it is
/// tagged with, and each end with whether its step stays in bounds.
struct Stepper {
    label: &'static str,
    value: String,
    selector: &'static str,
    down: (AccountsControl, bool),
    up: (AccountsControl, bool),
}

impl RootView {
    /// Whether Settings offers the Accounts tab: while the connection to
    /// the host Settings shows lists the feature.
    pub(crate) fn accounts_tab_offered(&self) -> bool {
        self.conn_of(&self.settings_host)
            .is_some_and(|conn| conn.supports(FEATURE_ACCOUNTS))
    }

    /// Whether the admin controls are disabled: on any host but this
    /// machine (the Local key on a local target), and on this machine when
    /// its daemon calls this connection remote.
    fn accounts_admin_locked(&self) -> bool {
        !self.is_this_machine(&self.settings_host)
            || self
                .conn_of(&self.settings_host)
                .and_then(crate::connection::Connection::origin)
                == Some(ConnectionOrigin::Remote)
    }

    /// Settings moved to another host: the alias fields, and any edit they
    /// hold, belong to the host they were built for and go, and so does an
    /// open Remove confirm.
    pub(crate) fn drop_account_alias_fields(&mut self) {
        self.settings_pending.account_aliases = AliasFields::default();
        self.settings_pending.remove_confirm = None;
    }

    /// The Accounts tab was selected, or Settings moved to another host
    /// while it shows: the host Settings shows is asked for its list, as
    /// the popover does, the alias fields are built, and a Remove confirm
    /// left from an earlier visit goes.
    pub(crate) fn on_accounts_tab_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let host = self.settings_host.clone();
        self.ask_accounts_if_listed(&host);
        self.hold_account_alias_fields(window, cx);
        self.settings_pending.remove_confirm = None;
    }

    /// While the Accounts tab shows: one alias field per account listed, a
    /// field whose host alias changed taking it unless it has the keyboard.
    pub(crate) fn hold_account_alias_fields(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.settings_tab != SettingsTab::Accounts {
            return;
        }
        let host = self.settings_host.clone();
        let listed: Vec<(u32, String, String)> = self
            .accounts
            .accounts(&host)
            .iter()
            .map(|account| {
                (
                    account.number,
                    account.alias.clone().unwrap_or_default(),
                    account.email.clone(),
                )
            })
            .collect();
        let writing = self.accounts.writing(&host);
        let fields = &mut self.settings_pending.account_aliases.0;
        fields.retain(|field| listed.iter().any(|(number, ..)| *number == field.number));
        for (number, alias, email) in listed {
            let fields = &mut self.settings_pending.account_aliases.0;
            if let Some(field) = fields.iter_mut().find(|field| field.number == number) {
                if field.base != alias && !field.focus.is_focused(window) {
                    field.base.clone_from(&alias);
                    field
                        .input
                        .update(cx, |input, cx| input.set_text(alias, cx));
                }
                // While a write is pending the field takes no edits, and
                // keeps the keyboard if it has it.
                field
                    .input
                    .update(cx, |input, cx| input.set_read_only(writing, cx));
                continue;
            }
            let field = Self::new_alias_field(number, alias, email, window, cx);
            field
                .input
                .update(cx, |input, cx| input.set_read_only(writing, cx));
            self.settings_pending.account_aliases.0.push(field);
            // The frame drawn before the field was built takes no keys.
            cx.notify();
        }
    }

    /// Account `number`'s alias field showing `alias`, its placeholder
    /// `email`: Enter or leaving it sends a changed alias, Esc is Cancel.
    fn new_alias_field(
        number: u32,
        alias: String,
        email: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AliasField {
        let input = cx.new(|cx| TextInput::new(alias.clone(), email, cx));
        let focus = input.read(cx).focus_handle(cx);
        let keys = cx.subscribe_in(
            &input,
            window,
            move |this, _, event: &TextInputEvent, window, cx| match event {
                TextInputEvent::Submit => {
                    this.commit_alias(number, cx);
                }
                TextInputEvent::Cancel => this.close_appearance_editor(window, cx),
            },
        );
        let blur = cx.on_blur(&focus, window, move |this, _, cx| {
            this.commit_alias(number, cx);
        });
        AliasField {
            number,
            input,
            focus,
            base: alias,
            _subscriptions: vec![keys, blur],
        }
    }

    /// The edit `field` holds: its text against the alias it was last set
    /// to from the host, so a rename made elsewhere is never undone by a
    /// field the user did not touch.
    fn alias_edit(field: &AliasField, cx: &App) -> Option<AliasChange> {
        accounts::alias_change(Some(&field.base), field.input.read(cx).text())
    }

    /// Sends account `number`'s alias as its field reads, when the user
    /// edited it; a blank field clears it. Nothing while a write is pending
    /// or while the admin controls are locked (any host but this machine).
    /// Returns whether it sent.
    fn commit_alias(&mut self, number: u32, cx: &mut Context<Self>) -> bool {
        let host = self.settings_host.clone();
        if self.accounts_admin_locked() || self.accounts.writing(&host) {
            return false;
        }
        let Some(alias) = self
            .settings_pending
            .account_aliases
            .get(number)
            .and_then(|field| Self::alias_edit(field, cx))
            .map(AliasChange::alias)
        else {
            return false;
        };
        self.send_account_write(
            &host,
            WriteAction::Alias(number),
            |request_id| ClientMessage::SetAccountAlias {
                number,
                alias,
                request_id,
            },
            cx,
        )
    }

    /// Whether an alias field holds an edit not sent yet.
    pub(crate) fn account_alias_edited(&self, cx: &App) -> bool {
        self.settings_pending
            .account_aliases
            .0
            .iter()
            .any(|field| Self::alias_edit(field, cx).is_some())
    }

    /// Settings closes by OK or its close button: the first alias field
    /// holding an edit sends it, as Enter would; only one write can be in
    /// flight, so any other edit is dropped. Returns whether it sent.
    pub(crate) fn commit_alias_on_close(&mut self, cx: &mut Context<Self>) -> bool {
        let edited: Vec<u32> = self
            .settings_pending
            .account_aliases
            .0
            .iter()
            .filter(|field| Self::alias_edit(field, cx).is_some())
            .map(|field| field.number)
            .collect();
        let Some(first) = edited.first() else {
            return false;
        };
        if edited.len() > 1 {
            tracing::info!(
                "accounts: closed Settings with more than one alias edit; only the first was sent"
            );
        }
        self.commit_alias(*first, cx)
    }

    /// Whether an alias field of the shown Accounts page has the keyboard.
    pub(crate) fn account_alias_focused(&self, window: &Window) -> bool {
        self.settings_tab == SettingsTab::Accounts
            && self
                .settings_pending
                .account_aliases
                .0
                .iter()
                .any(|field| field.focus.is_focused(window))
    }

    /// Account `number`'s alias field's focus, once built.
    pub(crate) fn alias_focus(&self, number: u32) -> Option<FocusHandle> {
        self.settings_pending
            .account_aliases
            .get(number)
            .map(|field| field.focus.clone())
    }

    /// The page's controls in drawing order; with `live_only`, only those
    /// that take input now. An alias field stays in the ring through a
    /// pending write, read-only, so the keyboard is not pulled from it.
    pub(crate) fn accounts_controls(&self, live_only: bool) -> Vec<SettingsControl> {
        let host = self.settings_host.clone();
        let writing = self.accounts.writing(&host);
        let locked = self.accounts_admin_locked();
        let switching = self.accounts.switching(&host);
        let running = self.accounts.login_running(&host);
        let list = self.accounts.accounts(&host);
        let now = Utc::now();
        let admin = !writing && !locked;
        let mut controls: Vec<(AccountsControl, bool)> = Vec::new();
        for (index, account) in list.iter().enumerate() {
            let row = accounts::row(account, switching, writing, now, &Utc);
            let actions = accounts::row_actions(&row, index, list.len());
            let n = account.number;
            if let Some(switch) = row.switch.as_ref().filter(|_| actions.switch) {
                controls.push((AccountsControl::Switch(n), switch.enabled));
            }
            controls.push((AccountsControl::Alias(n), !locked));
            if actions.up {
                controls.push((AccountsControl::Up(n), admin));
            }
            if actions.down {
                controls.push((AccountsControl::Down(n), admin));
            }
            if actions.removal == Removal::Remove {
                controls.push((AccountsControl::Remove(n), admin));
            }
        }
        controls.push((AccountsControl::AddLogin, admin));
        controls.push((AccountsControl::LogIn, admin && !running));
        if running {
            controls.push((AccountsControl::CancelLogin, admin));
        }
        if let Some(auto) = self.accounts.auto(&host) {
            let live = !writing;
            let threshold = |up| accounts::threshold_step(auto.threshold_pct, up).is_some();
            let hysteresis = |up| accounts::hysteresis_step(auto.hysteresis_pct, up).is_some();
            controls.extend([
                (AccountsControl::AutoSwitch, live),
                (AccountsControl::ThresholdDown, live && threshold(false)),
                (AccountsControl::ThresholdUp, live && threshold(true)),
                (AccountsControl::HysteresisDown, live && hysteresis(false)),
                (AccountsControl::HysteresisUp, live && hysteresis(true)),
            ]);
            controls.extend(
                accounts::cooldown_segments(auto.cooldown_secs)
                    .into_iter()
                    .map(|segment| (AccountsControl::Cooldown(segment.value), live)),
            );
            controls.extend(
                accounts::strategy_segments(auto.strategy)
                    .into_iter()
                    .map(|segment| (AccountsControl::Strategy(segment.value), live)),
            );
            controls.extend(
                accounts::window_segments(auto.rate_window_minutes)
                    .into_iter()
                    .map(|segment| (AccountsControl::Window(segment.value), live)),
            );
        }
        controls
            .into_iter()
            .filter(|(_, live)| !live_only || *live)
            .map(|(control, _)| control.control())
            .collect()
    }

    /// A press of the page's `control`: its account write, sent at once
    /// unless a write is pending; an admin control sends nothing on a
    /// host that is not this machine, and a choice already selected nothing
    /// either.
    pub(crate) fn press_accounts_control(
        &mut self,
        control: AccountsControl,
        cx: &mut Context<Self>,
    ) {
        if control.admin() && self.accounts_admin_locked() {
            return;
        }
        let host = self.settings_host.clone();
        match control {
            AccountsControl::Switch(number) => {
                self.send_account_write(
                    &host,
                    WriteAction::Switch(number),
                    |request_id| ClientMessage::SwitchAccount { number, request_id },
                    cx,
                );
            }
            // The field takes keys; Enter and leaving it send.
            AccountsControl::Alias(_) => {}
            AccountsControl::Up(number) | AccountsControl::Down(number) => {
                let up = matches!(control, AccountsControl::Up(_));
                if let Some(order) =
                    accounts::moved_order(self.accounts.accounts(&host), number, up)
                {
                    self.send_account_write(
                        &host,
                        WriteAction::Order,
                        |request_id| ClientMessage::SetAccountOrder { order, request_id },
                        cx,
                    );
                }
            }
            AccountsControl::Remove(number) => self.open_remove_confirm(number, cx),
            AccountsControl::AddLogin => {
                self.send_account_write(
                    &host,
                    WriteAction::AddLogin,
                    |request_id| ClientMessage::AddCurrentLogin { request_id },
                    cx,
                );
            }
            AccountsControl::LogIn => {
                let sent = self.send_account_write(
                    &host,
                    WriteAction::LogIn,
                    |request_id| ClientMessage::StartAccountLogin { request_id },
                    cx,
                );
                if sent {
                    self.accounts.clear_login(&host);
                }
            }
            AccountsControl::CancelLogin => {
                self.send_account_write(
                    &host,
                    WriteAction::CancelLogin,
                    |request_id| ClientMessage::CancelAccountLogin { request_id },
                    cx,
                );
            }
            AccountsControl::AutoSwitch
            | AccountsControl::ThresholdDown
            | AccountsControl::ThresholdUp
            | AccountsControl::HysteresisDown
            | AccountsControl::HysteresisUp
            | AccountsControl::Cooldown(_)
            | AccountsControl::Strategy(_)
            | AccountsControl::Window(_) => self.press_auto_control(control, cx),
        }
    }

    /// The Remove confirm, while Settings shows the Accounts page and the
    /// account it names is still listed. Leaving the page, closing Settings
    /// or the account going closes it.
    fn remove_confirm(&self) -> Option<&RemoveConfirm> {
        let on_page = self.settings_open() && self.settings_tab == SettingsTab::Accounts;
        self.settings_pending
            .remove_confirm
            .as_ref()
            .filter(|confirm| {
                on_page
                    && self
                        .accounts
                        .accounts(&self.settings_host)
                        .iter()
                        .any(|account| account.number == confirm.number())
            })
    }

    /// The Remove confirm's title and the selector of its focused button,
    /// while it shows.
    #[must_use]
    pub fn account_remove_confirm(&self) -> Option<(String, &'static str)> {
        self.remove_confirm()
            .map(|confirm| (confirm.title(), confirm.focused().selector()))
    }

    /// Remove on `number`'s row: its confirm opens with Cancel focused, and
    /// nothing is sent until its Remove is pressed.
    fn open_remove_confirm(&mut self, number: u32, cx: &mut Context<Self>) {
        let Some(label) = self
            .accounts
            .accounts(&self.settings_host)
            .iter()
            .find(|account| account.number == number)
            .map(remove_label)
        else {
            return;
        };
        self.settings_pending.remove_confirm = Some(RemoveConfirm::open(number, label));
        cx.notify();
    }

    /// A key while the Remove confirm shows; returns whether it took the
    /// key. Esc presses Cancel, Tab and Shift+Tab move between its buttons
    /// and Enter presses the focused one; every other key is the confirm's
    /// too, so nothing beneath it acts.
    pub(crate) fn on_account_remove_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.remove_confirm().is_none() {
            return false;
        }
        let mods = &keystroke.modifiers;
        if mods.control || mods.alt || mods.platform || mods.function {
            return true;
        }
        let pressed = self
            .settings_pending
            .remove_confirm
            .as_mut()
            .and_then(|confirm| confirm.key(keystroke.key.as_str()));
        match pressed {
            Some(pressed) => self.press_remove_button(pressed, window, cx),
            None => cx.notify(),
        }
        true
    }

    /// A press of the Remove confirm's `pressed` button: the confirm closes,
    /// and Remove first sends the account's removal.
    fn press_remove_button(
        &mut self,
        pressed: RemoveButton,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.settings_pending.remove_confirm.is_none() {
            return;
        }
        let host = self.settings_host.clone();
        let writing = self.accounts.writing(&host);
        let remove = self
            .settings_pending
            .remove_confirm
            .as_mut()
            .and_then(|confirm| confirm.press(pressed, writing));
        let Some(number) = remove else {
            if pressed == RemoveButton::Remove {
                // The account write the page waits on is still pending.
                cx.notify();
                return;
            }
            self.close_remove_confirm(window, cx);
            return;
        };
        tracing::info!(number, "accounts: removing a saved login");
        let sent = self.send_account_write(
            &host,
            WriteAction::Remove(number),
            |request_id| ClientMessage::RemoveAccount { number, request_id },
            cx,
        );
        if sent {
            self.close_remove_confirm(window, cx);
        }
        cx.notify();
    }

    /// Closes the confirm and hands the keyboard back to the Settings frame.
    fn close_remove_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings_pending.remove_confirm = None;
        self.settings_tabs_focus.focus(window);
        cx.notify();
    }

    /// `host`'s accounts changed or were forgotten: a Remove confirm for the
    /// host Settings shows closes when its account left the list or its row
    /// no longer offers Remove, so a confirm never comes back unasked.
    pub(crate) fn settle_remove_confirm(&mut self, host: &HostKey) {
        if *host != self.settings_host || self.settings_pending.remove_confirm.is_none() {
            return;
        }
        let list = self.accounts.accounts(host);
        let removable = self
            .settings_pending
            .remove_confirm
            .as_ref()
            .and_then(|confirm| {
                list.iter()
                    .position(|account| account.number == confirm.number())
                    .map(|index| (index, &list[index]))
            })
            .is_some_and(|(index, account)| {
                let row = accounts::row(account, None, false, Utc::now(), &Utc);
                accounts::row_actions(&row, index, list.len()).removal == Removal::Remove
            });
        if !removable {
            self.settings_pending.remove_confirm = None;
        }
    }

    /// The Remove confirm over a backdrop that takes every click beneath it.
    pub(crate) fn account_remove_confirm_layer(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let t = theme(cx);
        let confirm = self.remove_confirm()?;
        // Remove waits for a pending account write, and enables when its
        // answer arrives.
        let writing = self.accounts.writing(&self.settings_host);
        let buttons = RemoveButton::ALL.into_iter().map(|pressed| {
            let enabled = pressed == RemoveButton::Cancel || !writing;
            let button = dialog_button(
                pressed.selector(),
                pressed.label().to_owned(),
                dialog_look(
                    crate::fonts::ui_text_size(cx),
                    crate::fonts::ui_weight_shift(cx),
                    pressed == RemoveButton::Remove,
                    enabled,
                    &theme(cx),
                ),
                confirm.focused() == pressed,
            );
            if enabled {
                button.on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.press_remove_button(pressed, window, cx);
                }))
            } else {
                button
            }
        });
        let panel = modal_panel(
            crate::fonts::ui_text_size(cx),
            "account-remove-confirm-panel",
            &theme(cx),
        )
        .track_focus(&self.appearance_focus)
        .child(
            div()
                .font_weight(ui_weight(cx, FontWeight::SEMIBOLD))
                .child(confirm.title()),
        )
        .child(
            div()
                .text_color(gpui::rgb(t.muted))
                .child(REMOVE_CONFIRM_BODY),
        )
        .child(div().flex().justify_end().gap(px(8.0)).children(buttons));
        Some(backdrop("account-remove-confirm", panel, &theme(cx)))
    }

    /// A press of the auto-switch block's `control`, while the host
    /// reports an auto-switch state.
    fn press_auto_control(&mut self, control: AccountsControl, cx: &mut Context<Self>) {
        let host = self.settings_host.clone();
        let auto = self.accounts.auto(&host).cloned();
        match control {
            AccountsControl::AutoSwitch => {
                if let Some(auto) = auto {
                    self.send_account_write(
                        &host,
                        WriteAction::AutoSwitch,
                        |request_id| ClientMessage::SetAutoSwitch {
                            enabled: Some(!auto.enabled),
                            threshold_pct: None,
                            request_id,
                        },
                        cx,
                    );
                }
            }
            AccountsControl::ThresholdDown | AccountsControl::ThresholdUp => {
                let up = control == AccountsControl::ThresholdUp;
                if let Some(threshold) =
                    auto.and_then(|auto| accounts::threshold_step(auto.threshold_pct, up))
                {
                    self.send_account_write(
                        &host,
                        WriteAction::AutoSwitch,
                        |request_id| ClientMessage::SetAutoSwitch {
                            enabled: None,
                            threshold_pct: Some(threshold),
                            request_id,
                        },
                        cx,
                    );
                }
            }
            AccountsControl::HysteresisDown | AccountsControl::HysteresisUp => {
                let up = control == AccountsControl::HysteresisUp;
                if let Some(hysteresis) =
                    auto.and_then(|auto| accounts::hysteresis_step(auto.hysteresis_pct, up))
                {
                    self.send_rules(
                        Rules {
                            hysteresis_pct: Some(hysteresis),
                            ..Rules::default()
                        },
                        cx,
                    );
                }
            }
            AccountsControl::Cooldown(secs) => {
                if auto.is_some_and(|auto| auto.cooldown_secs != secs) {
                    self.send_rules(
                        Rules {
                            cooldown_secs: Some(secs),
                            ..Rules::default()
                        },
                        cx,
                    );
                }
            }
            AccountsControl::Strategy(strategy) => {
                if auto.is_some_and(|auto| auto.strategy != strategy) {
                    self.send_rules(
                        Rules {
                            strategy: Some(strategy),
                            ..Rules::default()
                        },
                        cx,
                    );
                }
            }
            AccountsControl::Window(minutes) => {
                if auto.is_some_and(|auto| auto.rate_window_minutes != Some(minutes)) {
                    self.send_rules(
                        Rules {
                            rate_window_minutes: Some(minutes),
                            ..Rules::default()
                        },
                        cx,
                    );
                }
            }
            AccountsControl::Switch(_)
            | AccountsControl::Alias(_)
            | AccountsControl::Up(_)
            | AccountsControl::Down(_)
            | AccountsControl::Remove(_)
            | AccountsControl::AddLogin
            | AccountsControl::LogIn
            | AccountsControl::CancelLogin => {}
        }
    }

    /// Sends the host Settings shows a `SetAutoSwitchRules` carrying
    /// `rules`.
    fn send_rules(&mut self, rules: Rules, cx: &mut Context<Self>) {
        let host = self.settings_host.clone();
        self.send_account_write(
            &host,
            WriteAction::Rules,
            |request_id| ClientMessage::SetAutoSwitchRules {
                hysteresis_pct: rules.hysteresis_pct,
                cooldown_secs: rules.cooldown_secs,
                strategy: rules.strategy,
                rate_window_minutes: rules.rate_window_minutes,
                request_id,
            },
            cx,
        );
    }

    /// The Accounts page: the refusal of the last write, the account table
    /// and Add current login, then the auto-switch block.
    pub(crate) fn accounts_tab_content(
        &self,
        focused: Option<SettingsControl>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let t = theme(cx);
        let host = self.settings_host.clone();
        let now = Utc::now();
        let list = self.accounts.accounts(&host);
        let writing = self.accounts.writing(&host);
        let switching = self.accounts.switching(&host);
        let page = Page {
            writing,
            locked: self.accounts_admin_locked(),
            focused,
        };
        let mut content = Vec::new();
        if let Some(message) = self.accounts.error(&host) {
            content.push(
                div()
                    .pb(px(12.0))
                    .text_color(gpui::rgb(t.danger_text))
                    .child(tagged_text("settings-accounts-error", message.to_owned()))
                    .into_any_element(),
            );
        }
        let rows = list
            .iter()
            .enumerate()
            .map(|(index, account)| {
                let row = accounts::row(account, switching, writing, now, &chrono::Local);
                let actions = accounts::row_actions(&row, index, list.len());
                self.page_row(&row, actions, page, cx)
            })
            .collect::<Vec<_>>();
        let live = !page.writing && !page.locked;
        let running = self.accounts.login_running(&host);
        let add = locked_tip(
            settings_button(
                AccountsControl::AddLogin.control(),
                ADD_LOGIN_LABEL,
                live,
                focused,
                cx,
            ),
            page.locked,
        );
        let log_in = locked_tip(
            settings_button(
                AccountsControl::LogIn.control(),
                LOG_IN_LABEL,
                live && !running,
                focused,
                cx,
            ),
            page.locked,
        );
        let cancel = running.then(|| {
            locked_tip(
                settings_button(
                    AccountsControl::CancelLogin.control(),
                    CANCEL_LOGIN_LABEL,
                    live,
                    focused,
                    cx,
                ),
                page.locked,
            )
        });
        let buttons = div()
            .flex()
            .gap(px(8.0))
            .child(add)
            .child(log_in)
            .children(cancel);
        let status = accounts::login_status(self.accounts.login(&host)).map(|text| {
            let lines = text.lines().map(|line| {
                let line = line.to_owned();
                hint(ui_text_size(cx), line.clone(), &t)
                    .driver_id(move || format!("settings-accounts-login-status={line}"))
            });
            div().flex().flex_col().gap(px(2.0)).children(lines)
        });
        let label = self
            .sidebar
            .agents(&host)
            .first()
            .map(|descriptor| descriptor.label.as_str());
        let note = hint(ui_text_size(cx), accounts::add_login_note(label), &t)
            .driver_id(|| "settings-accounts-add-login-note".to_owned());
        let table = section(ui_text_size(cx), ui_weight_shift(cx), TABLE_SECTION)
            .children(rows)
            .child(buttons)
            .children(status)
            .child(note);
        content.push(table.into_any_element());
        content.push(self.auto_section(page, cx).into_any_element());
        content
    }

    /// One account's row: its name line, reason and window lines, then its
    /// alias field and buttons.
    fn page_row(&self, row: &Row, actions: RowActions, page: Page, cx: &mut Context<Self>) -> Div {
        let t = theme(cx);
        let n = row.number;
        let name_line = div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .min_w_0()
            .child(
                div()
                    .flex_none()
                    .font_weight(ui_weight(cx, FontWeight::BOLD))
                    .text_color(gpui::rgb(t.text))
                    .child(tagged_text(
                        format!("settings-accounts-name-{n}"),
                        row.name.clone(),
                    )),
            )
            .children(row.detail.clone().map(|detail| {
                div()
                    .flex_1()
                    .min_w_0()
                    .text_color(gpui::rgb(t.subtle))
                    .child(tagged_text(format!("settings-accounts-detail-{n}"), detail))
            }))
            .children(row.tag.map(|tag| tag_chip(tag, n, &t, cx)));
        let reason = row.reason.map(|reason| {
            div()
                .text_color(gpui::rgb(t.text_2))
                .child(tagged_text(format!("settings-accounts-reason-{n}"), reason))
        });
        let windows = row
            .windows
            .iter()
            .map(|line| window_line(line, n, &t, cx))
            .collect::<Vec<_>>();
        let info = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(name_line)
            .children(reason)
            .children(windows);
        let controls = self.row_controls(row, actions, page, cx);
        div()
            .driver_id(move || format!("settings-accounts-row-{n}"))
            .flex()
            .items_center()
            .gap(px(10.0))
            .py(px(8.0))
            .border_t_1()
            .border_color(gpui::rgb(t.line))
            .when(row.dim, |row| row.opacity(DIM_ROW))
            .child(info)
            .child(controls)
    }

    /// A row's alias field, Switch, Up, Down and Remove, or the text that
    /// stands in Remove's place on the account in use.
    fn row_controls(
        &self,
        row: &Row,
        actions: RowActions,
        page: Page,
        cx: &mut Context<Self>,
    ) -> Div {
        let t = theme(cx);
        let n = row.number;
        let admin = !page.writing && !page.locked;
        let alias = self.alias_view(n, page, cx);
        let switch = row
            .switch
            .as_ref()
            .filter(|_| actions.switch)
            .map(|button| {
                settings_button(
                    AccountsControl::Switch(n).control(),
                    button.label,
                    button.enabled,
                    page.focused,
                    cx,
                )
            });
        let up = actions.up.then(|| {
            locked_tip(
                settings_button(
                    AccountsControl::Up(n).control(),
                    UP_LABEL,
                    admin,
                    page.focused,
                    cx,
                ),
                page.locked,
            )
        });
        let down = actions.down.then(|| {
            locked_tip(
                settings_button(
                    AccountsControl::Down(n).control(),
                    DOWN_LABEL,
                    admin,
                    page.focused,
                    cx,
                ),
                page.locked,
            )
        });
        let removal = match actions.removal {
            Removal::Remove => locked_tip(
                settings_button(
                    AccountsControl::Remove(n).control(),
                    REMOVE_LABEL,
                    admin,
                    page.focused,
                    cx,
                ),
                page.locked,
            )
            .into_any_element(),
            Removal::SwitchFirst => div()
                .text_color(gpui::rgb(t.subtle))
                .child(tagged_text(
                    format!("settings-accounts-switch-first-{n}"),
                    SWITCH_FIRST,
                ))
                .into_any_element(),
        };
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(6.0))
            .child(alias)
            .children(switch)
            .children(up)
            .children(down)
            .child(removal)
    }

    /// Account `number`'s alias field; while locked (any host but this
    /// machine), or before the field is built, a dimmed frame showing the
    /// alias that takes no keys.
    fn alias_view(&self, number: u32, page: Page, cx: &mut Context<Self>) -> AnyElement {
        let control = AccountsControl::Alias(number).control();
        if let Some(field) = self
            .settings_pending
            .account_aliases
            .get(number)
            .filter(|_| !page.locked)
        {
            return input_frame(control, &field.input, true, page.focused, cx)
                .w(px(ALIAS_WIDTH))
                .when(page.writing, |frame| frame.opacity(DISABLED_OPACITY))
                .into_any_element();
        }
        let t = theme(cx);
        let alias = self
            .accounts
            .accounts(&self.settings_host)
            .iter()
            .find(|account| account.number == number)
            .and_then(|account| account.alias.clone())
            .unwrap_or_default();
        let selector = control.selector();
        locked_tip(
            field_frame(false, Layer::Floating, &t)
                .id(selector)
                .driver_id(|| selector.to_owned())
                .w(px(ALIAS_WIDTH))
                .opacity(DISABLED_OPACITY)
                .text_color(gpui::rgb(t.subtle))
                .child(alias),
            page.locked,
        )
        .into_any_element()
    }

    /// The auto-switch block: its note while the host reports no engine,
    /// else the toggle, the two steppers, the three choice rows, the paused
    /// line and the forecast.
    fn auto_section(&self, page: Page, cx: &mut Context<Self>) -> Div {
        let t = theme(cx);
        let block = section(ui_text_size(cx), ui_weight_shift(cx), AUTO_SECTION);
        let Some(auto) = self.accounts.auto(&self.settings_host) else {
            return block.child(
                div()
                    .driver_id(|| "settings-accounts-auto-missing".to_owned())
                    .text_color(gpui::rgb(t.subtle))
                    .child(tagged_text("settings-accounts-auto-missing", AUTO_MISSING)),
            );
        };
        let live = !page.writing;
        let switch = toggle(
            AccountsControl::AutoSwitch.control(),
            ToggleState {
                checked: auto.enabled,
                enabled: live,
            },
            AUTO_TOGGLE_LABEL,
            page.focused,
            cx,
        );
        let threshold = stepper(
            Stepper {
                label: "Switch at",
                value: format!("{:.0}%", auto.threshold_pct),
                selector: "settings-accounts-threshold",
                down: (
                    AccountsControl::ThresholdDown,
                    accounts::threshold_step(auto.threshold_pct, false).is_some(),
                ),
                up: (
                    AccountsControl::ThresholdUp,
                    accounts::threshold_step(auto.threshold_pct, true).is_some(),
                ),
            },
            page,
            cx,
        );
        let hysteresis = stepper(
            Stepper {
                label: "Hysteresis",
                value: format!("{:.0}%", auto.hysteresis_pct),
                selector: "settings-accounts-hysteresis",
                down: (
                    AccountsControl::HysteresisDown,
                    accounts::hysteresis_step(auto.hysteresis_pct, false).is_some(),
                ),
                up: (
                    AccountsControl::HysteresisUp,
                    accounts::hysteresis_step(auto.hysteresis_pct, true).is_some(),
                ),
            },
            page,
            cx,
        );
        let lock = if live { RowLock::Live } else { RowLock::Dimmed };
        let cooldown = choices(
            accounts::cooldown_segments(auto.cooldown_secs),
            AccountsControl::Cooldown,
        );
        let strategy = choices(
            accounts::strategy_segments(auto.strategy),
            AccountsControl::Strategy,
        );
        let window = choices(
            accounts::window_segments(auto.rate_window_minutes),
            AccountsControl::Window,
        );
        let paused = accounts::paused_line(Some(auto)).map(|line| {
            div()
                .text_color(gpui::rgb(t.asking))
                .child(tagged_text("settings-accounts-paused", line))
        });
        let forecast = accounts::forecast(Some(auto), Utc::now(), &chrono::Local).map(|line| {
            div()
                .driver_id(|| "settings-accounts-forecast".to_owned())
                .text_color(gpui::rgb(t.text_2))
                .child(tagged_text("settings-accounts-forecast", line))
        });
        block
            .child(switch)
            .child(threshold)
            .child(hysteresis)
            .child(choice_row("Cooldown", &cooldown, lock, page.focused, cx))
            .child(choice_row("Strategy", &strategy, lock, page.focused, cx))
            .child(choice_row(
                "Forecast window",
                &window,
                lock,
                page.focused,
                cx,
            ))
            .children(paused)
            .children(forecast)
    }
}

/// The fields of a `SetAutoSwitchRules`; a field left out keeps its value.
#[derive(Clone, Copy, Default)]
struct Rules {
    hysteresis_pct: Option<f64>,
    cooldown_secs: Option<u64>,
    strategy: Option<AutoSwitchStrategy>,
    rate_window_minutes: Option<u32>,
}

/// The name the Remove confirm shows for `account`: its alias, else its
/// email, as its row shows it.
fn remove_label(account: &AccountSnapshot) -> String {
    accounts::row(account, None, false, Utc::now(), &Utc).name
}

/// `button` with the tooltip naming why it is locked (any host but this
/// machine) while `locked`.
fn locked_tip(button: Stateful<Div>, locked: bool) -> Stateful<Div> {
    button.when(locked, |button| button.tooltip(tooltip(REMOTE_ORIGIN_TIP)))
}

/// A choice row's choices from `segments`, each the control `control` makes
/// of its value.
fn choices<T>(
    segments: Vec<accounts::Segment<T>>,
    control: impl Fn(T) -> AccountsControl,
) -> Vec<(SettingsControl, &'static str, bool)> {
    segments
        .into_iter()
        .map(|segment| {
            (
                control(segment.value).control(),
                intern(segment.label),
                segment.selected,
            )
        })
        .collect()
}

/// A −/+ stepper row; an end whose step leaves the bounds, or every end
/// while a write is pending, is disabled.
fn stepper(stepper: Stepper, page: Page, cx: &mut Context<RootView>) -> Div {
    let t = theme(cx);
    let live = !page.writing;
    let (down, down_ok) = stepper.down;
    let (up, up_ok) = stepper.up;
    let value = div()
        .min_w(px(40.0))
        .flex()
        .justify_center()
        .text_color(gpui::rgb(t.text))
        .child(tagged_text(stepper.selector, stepper.value));
    field(ui_text_size(cx), ui_weight_shift(cx), stepper.label, &t).child(
        div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(settings_button(
                down.control(),
                "−",
                live && down_ok,
                page.focused,
                cx,
            ))
            .child(value)
            .child(settings_button(
                up.control(),
                "+",
                live && up_ok,
                page.focused,
                cx,
            )),
    )
}

/// The fill of a bar at `level`.
const fn bar_color(level: Level, t: &UiTheme) -> u32 {
    match level {
        Level::Normal => t.periwinkle,
        Level::Asking => t.asking,
        Level::Danger => t.danger,
        Level::Faint => t.faint,
    }
}

/// The text colour of a percentage at `level`.
const fn pct_color(level: Level, t: &UiTheme) -> u32 {
    match level {
        Level::Normal => t.text,
        Level::Asking => t.asking,
        Level::Danger => t.danger_text,
        Level::Faint => t.faint,
    }
}

/// A row's tag after its name.
fn tag_chip(tag: Tag, n: u32, t: &UiTheme, cx: &mut Context<RootView>) -> Div {
    let (fill, text) = match tag {
        Tag::InUse => (t.accent, t.on_accent),
        Tag::SignInRefused => (t.danger_wash, t.danger_text),
        Tag::OutOfRotation => (t.chip, t.subtle),
    };
    div()
        .flex_none()
        .px(px(6.0))
        .rounded(px(4.0))
        .bg(gpui::rgb(fill))
        .text_color(gpui::rgb(text))
        .text_size(ui_px(cx, 10.5))
        .font_weight(ui_weight(cx, FontWeight::BOLD))
        .child(tagged_text(
            format!("settings-accounts-tag-{n}"),
            tag.text(),
        ))
}

/// A row's window line: its label, bar, percentage and reset text.
fn window_line(
    line: &accounts::WindowLine,
    n: u32,
    t: &UiTheme,
    cx: &mut Context<RootView>,
) -> Div {
    let label = line.label.as_str();
    let slug = accounts::label_slug(label);
    let (width, height) = ROW_BAR;
    let bar = div()
        .flex_none()
        .w(px(width))
        .h(px(height))
        .rounded(px(height / 2.0))
        .bg(gpui::rgb(t.chip))
        .child(
            div()
                .h_full()
                .w(relative(line.bar.fill))
                .rounded(px(height / 2.0))
                .bg(gpui::rgb(bar_color(line.bar.level, t))),
        );
    div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .child(
            div()
                .min_w(px(18.0))
                .flex_none()
                .text_size(ui_px(cx, 11.0))
                .text_color(gpui::rgb(t.subtle))
                .child(label.to_owned()),
        )
        .child(bar)
        .child(
            div()
                .w(px(36.0))
                .flex_none()
                .flex()
                .justify_end()
                .text_color(gpui::rgb(pct_color(line.bar.level, t)))
                .child(tagged_text(
                    format!("settings-accounts-{slug}-{n}"),
                    line.bar.pct.clone(),
                )),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_color(gpui::rgb(if line.used_up {
                    t.danger_text
                } else {
                    t.text_2
                }))
                .child(tagged_text(
                    format!("settings-accounts-reset-{slug}-{n}"),
                    line.reset.clone(),
                )),
        )
}

#[cfg(test)]
mod tests {
    use protocol::{AccountKind, AccountSnapshot, AccountUsageStatus};

    use super::{RemoveButton, RemoveConfirm, remove_label};

    /// An account with `alias` (none when `None`) and `email`.
    fn account(alias: Option<&str>, email: &str) -> AccountSnapshot {
        AccountSnapshot {
            number: 1,
            email: email.to_owned(),
            alias: alias.map(str::to_owned),
            organization_name: None,
            kind: AccountKind::Oauth,
            active: false,
            disabled: false,
            status: AccountUsageStatus::Ok,
            five_hour: None,
            seven_day: None,
            model_limits: Vec::new(),
            extra_usage: None,
            fetched_at: None,
            error: None,
        }
    }

    #[test]
    fn the_confirm_opens_on_cancel_and_names_the_account() {
        let confirm = RemoveConfirm::open(2, "ops".to_owned());
        assert_eq!(confirm.title(), "Remove ops?");
        assert_eq!(confirm.number(), 2);
        assert_eq!(confirm.focused(), RemoveButton::Cancel);
    }

    #[test]
    fn enter_presses_the_focused_button_and_tab_moves_the_focus() {
        let mut confirm = RemoveConfirm::open(2, "ops".to_owned());
        assert_eq!(confirm.key("enter"), Some(RemoveButton::Cancel));
        assert!(confirm.key("tab").is_none(), "Tab presses nothing");
        assert_eq!(confirm.focused(), RemoveButton::Remove);
        assert_eq!(confirm.key("enter"), Some(RemoveButton::Remove));
        assert!(confirm.key("tab").is_none());
        assert_eq!(confirm.focused(), RemoveButton::Cancel);
    }

    #[test]
    fn escape_presses_cancel_from_either_button() {
        let mut confirm = RemoveConfirm::open(2, "ops".to_owned());
        assert_eq!(confirm.key("escape"), Some(RemoveButton::Cancel));
        confirm.key("tab");
        assert_eq!(confirm.key("escape"), Some(RemoveButton::Cancel));
        assert_eq!(confirm.focused(), RemoveButton::Remove, "Esc picks Cancel");
    }

    #[test]
    fn remove_waits_for_a_pending_write_and_sends_otherwise() {
        let mut confirm = RemoveConfirm::open(3, "lab".to_owned());
        assert_eq!(confirm.press(RemoveButton::Remove, true), None);
        assert_eq!(confirm.press(RemoveButton::Remove, false), Some(3));
        assert_eq!(confirm.press(RemoveButton::Cancel, true), None);
        assert_eq!(confirm.press(RemoveButton::Cancel, false), None);
    }

    #[test]
    fn the_title_names_the_alias_else_the_email() {
        assert_eq!(
            remove_label(&account(Some("ops"), "ops@example.com")),
            "ops"
        );
        assert_eq!(
            remove_label(&account(None, "sam@example.com")),
            "sam@example.com"
        );
        assert_eq!(
            remove_label(&account(Some("  "), "sam@example.com")),
            "sam@example.com",
            "a blank alias shows the email, as the row does"
        );
    }
}
