//! The footer's accounts chip and its popover: one host's saved accounts
//! ([`RootView::accounts_chip_host`]), drawn from [`crate::accounts`]'s
//! models. The view only renders and forwards clicks; the fold, the
//! stale-reply rule and every text live in the model.

use chrono::Utc;
use gpui::{AnyElement, ClickEvent, Context, Div, FontWeight, Hsla, div, prelude::*, px, relative};
use protocol::{AutoSwitchState, ClientMessage, DaemonMessage, FEATURE_ACCOUNTS};

use crate::accounts::{self, Bar, Chip, ChipCandidate, Level, Row, Tag, WindowLine, WriteAction};
use crate::buttons::{ButtonSize, Layer, outlined_button};
use crate::driver_id::DriverId as _;
use crate::fonts::{ui_px, ui_text_size, ui_weight, ui_weight_shift};
use crate::session_menu::popover_frame;
use crate::target::HostKey;
use crate::ui_theme::{UiTheme, theme};
use crate::{
    FOOTER_HEIGHT, FOOTER_PILL_HEIGHT, FOOTER_PILL_TEXT_SIZE, RootView, new_request_id,
    tagged_text, tooltip,
};

/// The popover's width, in px.
const POPOVER_WIDTH: f32 = 540.0;
/// The popover's gap from the window's right edge, in px.
const POPOVER_RIGHT: f32 = 8.0;
/// A chip bar's size, in px.
const CHIP_BAR: (f32, f32) = (34.0, 5.0);
/// A row bar's size, in px.
const ROW_BAR: (f32, f32) = (150.0, 6.0);
/// The opacity of a row out of rotation or signed out.
const DIM_ROW: f32 = 0.6;
/// The alpha of the accent wash under the row in use.
const ACTIVE_WASH: f32 = 0.06;

impl RootView {
    /// Folds what `msg` from `host` says about its accounts, asking for the
    /// list again when the fold dropped a write's reply.
    pub(crate) fn on_accounts_message(
        &mut self,
        host: &HostKey,
        msg: &DaemonMessage,
        cx: &mut Context<Self>,
    ) {
        let folded = self.accounts.fold(host, msg);
        if folded.ask_again {
            tracing::info!(
                ?host,
                "accounts: a write's reply was overtaken; asking again"
            );
            self.ask_accounts(host);
        }
        if folded.changed {
            cx.notify();
        }
    }

    /// Asks `host` for its accounts when its connection lists the feature;
    /// a new connection's first ask.
    pub(crate) fn ask_accounts_if_listed(&mut self, host: &HostKey) {
        if self.lists_accounts(host) {
            self.ask_accounts(host);
        }
    }

    /// Sends `host` a `ListAccounts` with a fresh request id.
    fn ask_accounts(&mut self, host: &HostKey) {
        let request_id = new_request_id();
        self.accounts.sent_list(host, request_id.clone());
        self.send_to(
            host,
            ClientMessage::ListAccounts {
                request_id: Some(request_id),
            },
        );
    }

    /// A daemon refusal from `host` carrying `request_id`: when it answers
    /// the pending write or list, the popover shows `message`. Returns
    /// whether it did.
    pub(crate) fn on_accounts_refused(
        &mut self,
        host: &HostKey,
        request_id: &str,
        message: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let answered = self.accounts.refused(host, request_id, message);
        if answered {
            tracing::info!(?host, "accounts: the daemon refused an account request");
            cx.notify();
        }
        answered
    }

    /// Whether `host`'s open connection lists the accounts feature.
    fn lists_accounts(&self, host: &HostKey) -> bool {
        self.conn_of(host)
            .is_some_and(|conn| conn.supports(FEATURE_ACCOUNTS))
    }

    /// The host whose accounts the chip and popover show: the focused
    /// pane's host when it lists accounts, else This PC when it does, else
    /// the first connected remote host in saved order that does; `None`
    /// when no host does.
    pub(crate) fn accounts_chip_host(&self) -> Option<HostKey> {
        let connected = self.connected_hosts();
        let mut held = self.held_hosts();
        held.sort_by_key(|host| self.rank(host));
        let hosts: Vec<ChipCandidate> = held
            .into_iter()
            .map(|host| ChipCandidate {
                connected: connected.contains(&host),
                listed: self.lists_accounts(&host) && !self.accounts.accounts(&host).is_empty(),
                host,
            })
            .collect();
        accounts::chip_host(&self.focused_host(), &hosts)
    }

    /// The host the chip and popover show now: while the popover is open,
    /// the host it opened on, else [`Self::accounts_chip_host`].
    fn accounts_shown_host(&self) -> Option<HostKey> {
        if self.accounts_open
            && let Some(host) = self.accounts.popover_host()
        {
            return Some(host.clone());
        }
        self.accounts_chip_host()
    }

    /// The shown host and its chip, while the footer draws one.
    fn accounts_chip_model(&self) -> Option<(HostKey, Chip)> {
        let host = self.accounts_shown_host()?;
        let chip = accounts::chip(self.lists_accounts(&host), self.accounts.accounts(&host))?;
        Some((host, chip))
    }

    /// Whether `host` is this machine: the Local key on a local target. A
    /// window launched against another machine holds it under the Local
    /// key too.
    pub(crate) fn is_this_machine(&self, host: &HostKey) -> bool {
        *host == HostKey::Local && self.conn().target().is_local()
    }

    /// `host`'s name when it is not this machine, which the chip and the
    /// popover header add.
    fn accounts_host_name(&self, host: &HostKey) -> Option<String> {
        (!self.is_this_machine(host)).then(|| self.host_name(host))
    }

    /// Closes the accounts popover once the footer draws no chip, so it
    /// never reopens on its own when the chip comes back.
    pub(crate) fn close_accounts_without_chip(&mut self) {
        if self.accounts_open && self.accounts_chip_model().is_none() {
            self.accounts_open = false;
        }
    }

    /// Whether the accounts popover shows: open, over a chip still drawn.
    pub(crate) fn accounts_popover_open(&self) -> bool {
        self.accounts_open && self.accounts_chip_model().is_some()
    }

    /// The chip's text, while the footer draws it.
    #[must_use]
    pub fn accounts_chip_text(&self) -> Option<String> {
        self.accounts_chip_model()
            .map(|(host, chip)| match self.accounts_host_name(&host) {
                Some(name) => format!("{} · {name}", chip.text()),
                None => chip.text(),
            })
    }

    /// Whether the accounts popover is open.
    #[must_use]
    pub fn accounts_open(&self) -> bool {
        self.accounts_popover_open()
    }

    /// Whether the daemon troubleshooting flyout is open.
    #[must_use]
    pub fn daemon_flyout_open(&self) -> bool {
        self.flyout_open
    }

    /// The account whose Switch the shown host has not answered yet.
    #[must_use]
    pub fn accounts_switching(&self) -> Option<u32> {
        self.accounts_shown_host()
            .and_then(|host| self.accounts.switching(&host))
    }

    /// Opens the accounts popover on the chip's host, which it keeps while
    /// it stays open, closing the daemon flyout and asking that host for
    /// the list afresh, or closes it when open.
    pub fn toggle_accounts(&mut self) {
        if self.accounts_open {
            self.close_flyout();
            return;
        }
        self.close_flyout();
        self.accounts_open = true;
        let host = self.accounts_chip_host();
        self.accounts.set_popover_host(host.clone());
        if let Some(host) = host {
            self.ask_accounts_if_listed(&host);
        }
    }

    /// Sends the shown host a switch to account `number`, unless a write
    /// is pending.
    fn switch_account(&mut self, number: u32, cx: &mut Context<Self>) {
        let Some(host) = self.accounts_shown_host() else {
            return;
        };
        self.send_account_write(
            &host,
            WriteAction::Switch(number),
            |request_id| ClientMessage::SwitchAccount { number, request_id },
            cx,
        );
    }

    /// Sends `host` the account write `message`, recorded as `action`,
    /// unless a write is pending; returns whether it sent.
    pub(crate) fn send_account_write(
        &mut self,
        host: &HostKey,
        action: WriteAction,
        message: impl FnOnce(Option<String>) -> ClientMessage,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.accounts.writing(host) {
            return false;
        }
        let request_id = new_request_id();
        self.accounts.sent_write(host, action, request_id.clone());
        self.send_to(host, message(Some(request_id)));
        cx.notify();
        true
    }

    /// The popover's title, from `host`'s default agent, naming `host`
    /// unless it is This PC.
    fn accounts_header(&self, host: &HostKey) -> String {
        let label = self
            .sidebar
            .agents(host)
            .first()
            .map(|descriptor| descriptor.label.as_str());
        let header = accounts::header(label);
        match self.accounts_host_name(host) {
            Some(name) => format!("{header} on {name}"),
            None => header,
        }
    }

    /// The footer's accounts chip: a dot, the account in use and its two
    /// windows' bars; a click opens or closes the popover.
    pub(crate) fn accounts_chip(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (host, chip) = self.accounts_chip_model()?;
        let host_name = self.accounts_host_name(&host);
        let t = theme(cx);
        let open = self.accounts_open;
        let in_use = chip.five_hour.is_some() || chip.seven_day.is_some();
        let signed_out = chip.signed_out;
        let dot = if signed_out {
            t.danger
        } else if in_use {
            t.waiting
        } else {
            t.faint
        };
        let windows = [("5h", chip.five_hour), ("7d", chip.seven_day)]
            .into_iter()
            .filter_map(|(label, bar)| bar.map(|bar| (label, bar)))
            .map(|(label, bar)| {
                // Only the five-hour percentage takes its level's colour.
                let pct_color = if label == "5h" {
                    pct_color(bar.level, &t)
                } else {
                    t.text_2
                };
                let warm = label == "5h" && matches!(bar.level, Level::Asking | Level::Danger);
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(5.0))
                    .child(div().text_color(gpui::rgb(t.subtle)).child(label))
                    .child(bar_view(&bar, CHIP_BAR, &t))
                    .child(
                        div()
                            .text_color(gpui::rgb(pct_color))
                            .when(warm, |pct| pct.font_weight(ui_weight(cx, FontWeight::BOLD)))
                            .child(tagged_text(format!("footer-accounts-{label}"), bar.pct)),
                    )
            })
            .collect::<Vec<_>>();
        Some(
            div()
                .id("footer-accounts")
                .driver_id(|| "footer-accounts".to_owned())
                .flex()
                .flex_none()
                .items_center()
                .gap(px(7.0))
                .h(ui_px(cx, FOOTER_PILL_HEIGHT))
                .px(px(9.0))
                .rounded(ui_px(cx, FOOTER_PILL_HEIGHT) / 2.0)
                .border_1()
                .border_color(gpui::rgb(if open { t.subtle } else { t.line_strong }))
                .when(open, move |chip| chip.bg(gpui::rgb(t.chip)))
                .text_size(ui_px(cx, FOOTER_PILL_TEXT_SIZE))
                .text_color(gpui::rgb(t.text_2))
                .whitespace_nowrap()
                .cursor_pointer()
                .hover(move |style| style.bg(gpui::rgb(t.hover_bg)))
                .child(
                    div()
                        .flex_none()
                        .size(px(7.0))
                        .rounded_full()
                        .bg(gpui::rgb(dot)),
                )
                .child(
                    div()
                        .font_weight(ui_weight(cx, FontWeight::BOLD))
                        .text_color(gpui::rgb(t.text))
                        .child(tagged_text("footer-accounts-name", chip.name)),
                )
                .children(signed_out.then(|| {
                    div()
                        .text_color(gpui::rgb(t.danger_text))
                        .child(tagged_text(
                            "footer-accounts-state",
                            Tag::SignInRefused.text(),
                        ))
                }))
                .children(windows)
                .children(host_name.map(|name| {
                    div()
                        .text_color(gpui::rgb(t.subtle))
                        .child(tagged_text("footer-accounts-host", name))
                }))
                .tooltip(tooltip(self.accounts_header(&host)))
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.toggle_accounts();
                    cx.notify();
                }))
                .into_any_element(),
        )
    }

    /// The popover above the chip: the header, the engine's notes, one row
    /// per account and when the newest usage was fetched.
    pub(crate) fn accounts_popover(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = theme(cx);
        let host = self.accounts_shown_host().unwrap_or(HostKey::Local);
        let now = Utc::now();
        let list = self.accounts.accounts(&host);
        let auto = self.accounts.auto(&host);
        let switching = self.accounts.switching(&host);
        let writing = self.accounts.writing(&host);
        let controls = auto.map_or_else(Vec::new, |auto| {
            Self::auto_controls(&host, auto, writing, &t, cx)
        });
        let head = div()
            .flex()
            .items_center()
            .gap(px(10.0))
            .px(px(14.0))
            .py(px(10.0))
            .child(
                div()
                    .flex_1()
                    .font_weight(ui_weight(cx, FontWeight::SEMIBOLD))
                    .text_size(ui_px(cx, 14.0))
                    .child(tagged_text(
                        "accounts-header".to_owned(),
                        self.accounts_header(&host),
                    )),
            )
            .children(controls);
        let note = |selector: &str, text: String, color: u32| {
            div()
                .px(px(14.0))
                .pb(px(8.0))
                .text_color(gpui::rgb(color))
                .child(tagged_text(selector.to_owned(), text))
        };
        let notes = [
            accounts::paused_line(auto).map(|line| note("accounts-paused", line, t.asking)),
            self.accounts
                .error(&host)
                .map(|message| note("accounts-error", message.to_owned(), t.danger_text)),
            accounts::forecast(auto, now, &chrono::Local)
                .map(|line| note("accounts-forecast", line, t.text_2)),
        ];
        let rows = list
            .iter()
            .map(|account| accounts::row(account, switching, writing, now, &chrono::Local))
            .map(|row| Self::account_row(row, &t, cx))
            .collect::<Vec<_>>();
        let checked = accounts::checked_line(list, now).map(|line| {
            div()
                .px(px(14.0))
                .py(px(8.0))
                .border_t_1()
                .border_color(gpui::rgb(t.line))
                .text_color(gpui::rgb(t.subtle))
                .child(tagged_text("accounts-checked".to_owned(), line))
        });
        popover_frame(ui_text_size(cx), "accounts-popover", &t)
            .absolute()
            .right(px(POPOVER_RIGHT))
            .bottom(ui_px(cx, FOOTER_HEIGHT) + px(4.0))
            .w(px(POPOVER_WIDTH))
            .overflow_hidden()
            .flex()
            .flex_col()
            .child(head)
            .children(notes.into_iter().flatten())
            .children(rows)
            .children(checked)
            .into_any_element()
    }

    /// The popover header's auto-switch controls: the on/off toggle and,
    /// while it is on, the threshold stepper; every control takes a click
    /// only while no write is pending.
    fn auto_controls(
        host: &HostKey,
        auto: &AutoSwitchState,
        writing: bool,
        t: &UiTheme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let on = auto.enabled;
        let toggle_enabled = !writing;
        let toggle_host = host.clone();
        let toggle = outlined_button(
            ui_text_size(cx),
            ui_weight_shift(cx),
            "accounts-auto-toggle",
            ButtonSize::Compact,
            toggle_enabled,
            Layer::Floating,
            t,
        )
        .child(tagged_text(
            "accounts-auto-toggle".to_owned(),
            if on {
                "Auto-switch: On"
            } else {
                "Auto-switch: Off"
            },
        ))
        .when(toggle_enabled, |button| {
            button.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.send_account_write(
                    &toggle_host,
                    WriteAction::AutoSwitch,
                    |request_id| ClientMessage::SetAutoSwitch {
                        enabled: Some(!on),
                        threshold_pct: None,
                        request_id,
                    },
                    cx,
                );
            }))
        });
        let mut controls = vec![div().flex_none().child(toggle).into_any_element()];
        if auto.enabled {
            controls.push(Self::threshold_button(
                host,
                auto.threshold_pct,
                false,
                writing,
                t,
                cx,
            ));
            controls.push(
                div()
                    .flex_none()
                    .text_color(gpui::rgb(t.text_2))
                    .child(tagged_text(
                        "accounts-auto-threshold".to_owned(),
                        format!("at {:.0}%", auto.threshold_pct),
                    ))
                    .into_any_element(),
            );
            controls.push(Self::threshold_button(
                host,
                auto.threshold_pct,
                true,
                writing,
                t,
                cx,
            ));
        }
        controls
    }

    /// The threshold stepper's `up` or down button, disabled when the step
    /// leaves the bounds or a write is pending.
    fn threshold_button(
        host: &HostKey,
        current: f64,
        up: bool,
        writing: bool,
        t: &UiTheme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let host = host.clone();
        let selector = if up {
            "accounts-auto-up"
        } else {
            "accounts-auto-down"
        };
        let step = accounts::threshold_step(current, up);
        let enabled = !writing && step.is_some();
        outlined_button(
            ui_text_size(cx),
            ui_weight_shift(cx),
            selector,
            ButtonSize::Compact,
            enabled,
            Layer::Floating,
            t,
        )
        .child(tagged_text(selector.to_owned(), if up { "+" } else { "−" }))
        .when(enabled, |button| {
            button.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                if let Some(threshold) = step {
                    this.send_account_write(
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
            }))
        })
        .into_any_element()
    }

    /// One account's row: its name line, its reason, its window lines and
    /// its Switch button.
    fn account_row(row: Row, t: &UiTheme, cx: &mut Context<Self>) -> Div {
        let n = row.number;
        let mut wash: Hsla = gpui::rgb(t.accent).into();
        wash.a = ACTIVE_WASH;
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
                    .child(tagged_text(format!("accounts-name-{n}"), row.name)),
            )
            .children(row.detail.map(|detail| {
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_color(gpui::rgb(t.subtle))
                    .child(tagged_text(format!("accounts-detail-{n}"), detail))
            }))
            .children(row.tag.map(|tag| tag_view(tag, n, t, cx)));
        let reason = row.reason.map(|reason| {
            div()
                .text_color(gpui::rgb(t.text_2))
                .child(tagged_text(format!("accounts-reason-{n}"), reason))
        });
        let windows = row
            .windows
            .into_iter()
            .map(|line| window_view(line, n, t, cx))
            .collect::<Vec<_>>();
        let switch = row.switch.map(|button| {
            let enabled = button.enabled;
            outlined_button(
                ui_text_size(cx),
                ui_weight_shift(cx),
                &format!("accounts-switch-{n}"),
                ButtonSize::Compact,
                enabled,
                Layer::Floating,
                t,
            )
            .child(tagged_text(format!("accounts-switch-{n}"), button.label))
            .when(enabled, |button| {
                button.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.switch_account(n, cx);
                }))
            })
        });
        let edge = t.accent;
        div()
            .driver_id(move || format!("accounts-row-{n}"))
            .relative()
            .flex()
            .gap(px(10.0))
            .px(px(14.0))
            .py(px(9.0))
            .border_t_1()
            .border_color(gpui::rgb(t.line))
            .when(row.active, move |row| row.bg(wash))
            .when(row.dim, |row| row.opacity(DIM_ROW))
            .children(row.active.then(|| {
                div()
                    .absolute()
                    .left_0()
                    .top_0()
                    .bottom_0()
                    .w(px(2.0))
                    .bg(gpui::rgb(edge))
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(name_line)
                    .children(reason)
                    .children(windows),
            )
            .child(
                div()
                    .w(px(64.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_end()
                    .children(switch),
            )
    }
}

/// The fill of a bar at `level`.
fn bar_color(level: Level, t: &UiTheme) -> u32 {
    match level {
        Level::Normal => t.periwinkle,
        Level::Asking => t.asking,
        Level::Danger => t.danger,
        Level::Faint => t.faint,
    }
}

/// The text colour of a percentage at `level`.
fn pct_color(level: Level, t: &UiTheme) -> u32 {
    match level {
        Level::Normal => t.text,
        Level::Asking => t.asking,
        Level::Danger => t.danger_text,
        Level::Faint => t.faint,
    }
}

/// A usage bar of `(width, height)` px: the chip track with its level's
/// fill.
fn bar_view(bar: &Bar, (width, height): (f32, f32), t: &UiTheme) -> Div {
    div()
        .flex_none()
        .w(px(width))
        .h(px(height))
        .rounded(px(height / 2.0))
        .bg(gpui::rgb(t.chip))
        .child(
            div()
                .h_full()
                .w(relative(bar.fill))
                .rounded(px(height / 2.0))
                .bg(gpui::rgb(bar_color(bar.level, t))),
        )
}

/// A row's tag after its name.
fn tag_view(tag: Tag, n: u32, t: &UiTheme, cx: &mut Context<RootView>) -> Div {
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
        .child(tagged_text(format!("accounts-tag-{n}"), tag.text()))
}

/// A row's window line: its label, bar, percentage and reset text.
fn window_view(line: WindowLine, n: u32, t: &UiTheme, cx: &mut Context<RootView>) -> Div {
    let label = line.label;
    div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .h(px(19.0))
        .child(
            div()
                .w(px(18.0))
                .flex_none()
                .text_size(ui_px(cx, 11.0))
                .text_color(gpui::rgb(t.subtle))
                .child(label),
        )
        .child(bar_view(&line.bar, ROW_BAR, t))
        .child(
            div()
                .w(px(36.0))
                .flex_none()
                .flex()
                .justify_end()
                .font_weight(ui_weight(cx, FontWeight::SEMIBOLD))
                .text_color(gpui::rgb(pct_color(line.bar.level, t)))
                .child(tagged_text(format!("accounts-{label}-{n}"), line.bar.pct)),
        )
        .child(
            div()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_color(gpui::rgb(if line.used_up {
                    t.danger_text
                } else {
                    t.text_2
                }))
                .child(tagged_text(
                    format!("accounts-reset-{label}-{n}"),
                    line.reset,
                )),
        )
}
