//! Each host's saved agent accounts as its daemon last sent them, for the
//! footer's accounts chip and popover (and the Settings page that reads the
//! same state): the fold of `Accounts` and `AccountsUpdated`, the rule that
//! drops a reply an update overtook, and the plain view models the chip and
//! the popover draw. Plain Rust, fed from the root view's message folds;
//! every time the models show is read against a `now` and a time zone the
//! caller passes.
//!
//! The stale-reply rule: the client counts each host's `AccountsUpdated`
//! broadcasts. A `ListAccounts` or an account write is stamped with that
//! count and its request id, and its `Accounts` reply lands only while it is
//! the pending ask and the count has not moved since; a broadcast always
//! lands. A dropped reply to a write asks for the list again, since it was
//! the only post-write snapshot the client was sent.

use std::collections::HashMap;
use std::fmt::Display;

use chrono::{DateTime, TimeDelta, TimeZone, Utc};
use protocol::{
    AccountSnapshot, AccountUsageStatus, AccountUsageWindow, AutoSwitchState, AutoSwitchStrategy,
    DaemonMessage,
};

use crate::target::HostKey;

/// How old a row's usage may be before the row shows its age, in minutes.
const STALE_AFTER_MINUTES: i64 = 10;

/// The chip's text while no saved account is the one in use.
pub(crate) const NO_ACTIVE_ACCOUNT: &str = "No active account";
/// The second line of a row whose sign-in the provider refused.
pub(crate) const RELOGIN_LINE: &str = "Log in again from Settings › Accounts";
/// The line of a row whose store the daemon cannot write.
pub(crate) const STORE_READ_ONLY: &str = "Store read-only";
/// A row's Switch button, and its label while its switch is pending.
const SWITCH_LABEL: &str = "Switch";
const SWITCHING_LABEL: &str = "Switching…";
/// The threshold stepper's step, in percentage points.
const THRESHOLD_STEP: f64 = 5.0;
/// The threshold stepper's lowest value, in percent.
const THRESHOLD_MIN: f64 = 50.0;
/// The threshold stepper's highest value, in percent.
const THRESHOLD_MAX: f64 = 95.0;

/// The Hysteresis stepper's step, lowest and highest value, in percentage
/// points.
const HYSTERESIS_STEP: f64 = 1.0;
const HYSTERESIS_MIN: f64 = 0.0;
const HYSTERESIS_MAX: f64 = 50.0;
/// The Cooldown row's choices, in minutes.
const COOLDOWN_MINUTES: [u64; 6] = [0, 1, 5, 15, 30, 60];
/// The Cooldown row's 0-minute segment.
const OFF_LABEL: &str = "Off";
/// The Strategy row's choices.
const STRATEGY_CHOICES: [(AutoSwitchStrategy, &str); 2] = [
    (AutoSwitchStrategy::Best, "Most headroom"),
    (AutoSwitchStrategy::ConsumeFirst, "Resets soonest"),
];
/// The Forecast window row's choices, in minutes.
const FORECAST_WINDOWS: [(u32, &str); 4] =
    [(30, "30 min"), (60, "1 h"), (120, "2 h"), (180, "3 h")];
/// What a Settings page row says in Remove's place on the account in use.
pub(crate) const SWITCH_FIRST: &str = "Switch first";

/// An ask sent to a host: its request id and the host's update count when
/// it was sent.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Ask {
    request_id: String,
    updates_at_ask: u64,
}

/// A write sent to a host and not answered yet.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WriteAsk {
    action: WriteAction,
    ask: Ask,
}

/// What the pending write asks a host's daemon to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteAction {
    Switch(u32),
    AddLogin,
    Remove(u32),
    Alias(u32),
    Order,
    AutoSwitch,
    Rules,
}

/// One host's accounts.
#[derive(Debug, Default)]
struct HostAccounts {
    accounts: Vec<AccountSnapshot>,
    auto: Option<AutoSwitchState>,
    /// How many `AccountsUpdated` broadcasts this connection has sent.
    updates_seen: u64,
    /// The newest `ListAccounts` not answered yet.
    list: Option<Ask>,
    /// The account write not answered yet.
    write: Option<WriteAsk>,
    /// The daemon's refusal of the last ask, until the next reply lands.
    error: Option<String>,
}

/// What a fold did: whether it changed what is shown, and whether the
/// client must ask for the list again.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Folded {
    pub(crate) changed: bool,
    pub(crate) ask_again: bool,
}

/// Every host's accounts.
#[derive(Debug, Default)]
pub(crate) struct AccountsState {
    hosts: HashMap<HostKey, HostAccounts>,
}

impl AccountsState {
    /// Folds what `msg` from `host` says about its accounts: a broadcast
    /// replaces them, a reply replaces them under the stale-reply rule, and
    /// a `Welcome` forgets everything the host's old connection said.
    pub(crate) fn fold(&mut self, host: &HostKey, msg: &DaemonMessage) -> Folded {
        match msg {
            DaemonMessage::Welcome { .. } => Folded {
                changed: self.hosts.remove(host).is_some(),
                ask_again: false,
            },
            DaemonMessage::AccountsUpdated { accounts, auto } => {
                let state = self.hosts.entry(host.clone()).or_default();
                state.updates_seen += 1;
                state.accounts.clone_from(accounts);
                state.auto.clone_from(auto);
                Folded {
                    changed: true,
                    ask_again: false,
                }
            }
            DaemonMessage::Accounts {
                accounts,
                auto,
                request_id,
            } => self.reply(host, request_id.as_deref(), accounts, auto.as_ref()),
            _ => Folded::default(),
        }
    }

    /// The `Accounts` reply `request_id` from `host`.
    fn reply(
        &mut self,
        host: &HostKey,
        request_id: Option<&str>,
        accounts: &[AccountSnapshot],
        auto: Option<&AutoSwitchState>,
    ) -> Folded {
        let (Some(state), Some(id)) = (self.hosts.get_mut(host), request_id) else {
            tracing::debug!(?host, "accounts: dropping a reply no ask is waiting for");
            return Folded::default();
        };
        let (asked, write) = if state.list.as_ref().is_some_and(|ask| ask.request_id == id) {
            (state.list.take(), false)
        } else if state
            .write
            .as_ref()
            .is_some_and(|write| write.ask.request_id == id)
        {
            (state.write.take().map(|write| write.ask), true)
        } else {
            tracing::debug!(
                ?host,
                "accounts: dropping a reply to an ask no longer pending"
            );
            return Folded::default();
        };
        if asked.is_none_or(|ask| ask.updates_at_ask != state.updates_seen) {
            tracing::debug!(
                ?host,
                write,
                "accounts: dropping a reply an update overtook"
            );
            return Folded {
                changed: write,
                ask_again: write,
            };
        }
        state.accounts = accounts.to_vec();
        state.auto = auto.cloned();
        state.error = None;
        Folded {
            changed: true,
            ask_again: false,
        }
    }

    /// A daemon `Error` from `host` answering `request_id`: when it answers
    /// the pending write or list, the ask ends and `message` shows until
    /// the next reply lands. Returns whether it answered either.
    pub(crate) fn refused(&mut self, host: &HostKey, request_id: &str, message: &str) -> bool {
        let Some(state) = self.hosts.get_mut(host) else {
            return false;
        };
        if state
            .write
            .as_ref()
            .is_some_and(|write| write.ask.request_id == request_id)
        {
            state.write = None;
        } else if state
            .list
            .as_ref()
            .is_some_and(|ask| ask.request_id == request_id)
        {
            state.list = None;
        } else {
            return false;
        }
        state.error = Some(message.to_owned());
        true
    }

    /// The `ListAccounts` `request_id` was sent to `host`; an older one's
    /// reply no longer lands.
    pub(crate) fn sent_list(&mut self, host: &HostKey, request_id: String) {
        let state = self.hosts.entry(host.clone()).or_default();
        state.list = Some(Ask {
            request_id,
            updates_at_ask: state.updates_seen,
        });
    }

    /// The account write `action`, `request_id`, was sent to `host`; the
    /// last refusal goes.
    pub(crate) fn sent_write(&mut self, host: &HostKey, action: WriteAction, request_id: String) {
        let state = self.hosts.entry(host.clone()).or_default();
        state.write = Some(WriteAsk {
            action,
            ask: Ask {
                request_id,
                updates_at_ask: state.updates_seen,
            },
        });
        state.error = None;
    }

    /// `host`'s accounts, in its daemon's order; none before it sent any.
    pub(crate) fn accounts(&self, host: &HostKey) -> &[AccountSnapshot] {
        self.hosts.get(host).map_or(&[], |state| &state.accounts)
    }

    /// `host`'s auto-switch state, while its daemon runs an engine.
    pub(crate) fn auto(&self, host: &HostKey) -> Option<&AutoSwitchState> {
        self.hosts.get(host)?.auto.as_ref()
    }

    /// The daemon's refusal of the last ask sent to `host`.
    pub(crate) fn error(&self, host: &HostKey) -> Option<&str> {
        self.hosts.get(host)?.error.as_deref()
    }

    /// The account whose switch `host` has not answered yet.
    pub(crate) fn switching(&self, host: &HostKey) -> Option<u32> {
        match self.hosts.get(host)?.write.as_ref()?.action {
            WriteAction::Switch(number) => Some(number),
            _ => None,
        }
    }

    /// Whether `host` has an account write pending.
    pub(crate) fn writing(&self, host: &HostKey) -> bool {
        self.hosts
            .get(host)
            .is_some_and(|state| state.write.is_some())
    }
}

/// How a usage bar and its percentage are coloured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Level {
    /// Below 80 %: the periwinkle fill.
    Normal,
    /// 80 % or more.
    Asking,
    /// 90 % or more.
    Danger,
    /// A row out of rotation.
    Faint,
}

/// One usage window's bar: its percentage text, its fill share (at least
/// 1 %, at most all) and its level.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Bar {
    pub(crate) pct: String,
    pub(crate) fill: f32,
    pub(crate) level: Level,
}

/// The level of a window used `pct` percent.
fn level(pct: f64) -> Level {
    if pct >= 90.0 {
        Level::Danger
    } else if pct >= 80.0 {
        Level::Asking
    } else {
        Level::Normal
    }
}

/// `window`'s bar; a `dim` row's is faint.
#[expect(
    clippy::cast_possible_truncation,
    reason = "the fill is a share between 0.01 and 1"
)]
pub(crate) fn bar(window: &AccountUsageWindow, dim: bool) -> Bar {
    let pct = if window.pct.is_finite() {
        window.pct
    } else {
        0.0
    };
    Bar {
        pct: format!("{pct:.0}%"),
        fill: (pct / 100.0).clamp(0.01, 1.0) as f32,
        level: if dim { Level::Faint } else { level(pct) },
    }
}

/// The name an account goes by: its alias, else its email.
fn display_name(account: &AccountSnapshot) -> &str {
    alias(account).unwrap_or(&account.email)
}

/// An account's alias, when it has a non-empty one.
fn alias(account: &AccountSnapshot) -> Option<&str> {
    account
        .alias
        .as_deref()
        .filter(|alias| !alias.trim().is_empty())
}

/// The footer chip: the account in use and its two windows, or
/// [`NO_ACTIVE_ACCOUNT`] with no bars.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Chip {
    pub(crate) name: String,
    pub(crate) five_hour: Option<Bar>,
    pub(crate) seven_day: Option<Bar>,
    /// The account in use, its sign-in refused: no bars, the danger dot.
    pub(crate) signed_out: bool,
}

impl Chip {
    /// The chip as one line: `{name} · 5h {pct} · 7d {pct}`, a window the
    /// account has none of left out.
    pub(crate) fn text(&self) -> String {
        if self.signed_out {
            return format!("{} · {}", self.name, Tag::SignInRefused.text());
        }
        std::iter::once(self.name.clone())
            .chain(
                [("5h", &self.five_hour), ("7d", &self.seven_day)]
                    .into_iter()
                    .filter_map(|(label, bar)| {
                        bar.as_ref().map(|bar| format!("{label} {}", bar.pct))
                    }),
            )
            .collect::<Vec<_>>()
            .join(" · ")
    }
}

/// The chip for a host that `supports` the accounts feature and lists
/// `accounts`; none when it does not list the feature or keeps no account.
pub(crate) fn chip(supports: bool, accounts: &[AccountSnapshot]) -> Option<Chip> {
    if !supports || accounts.is_empty() {
        return None;
    }
    let Some(active) = accounts.iter().find(|account| account.active) else {
        return Some(Chip {
            name: NO_ACTIVE_ACCOUNT.to_owned(),
            five_hour: None,
            seven_day: None,
            signed_out: false,
        });
    };
    if active.status == AccountUsageStatus::ReloginRequired {
        return Some(signed_out_chip(active));
    }
    Some(Chip {
        name: display_name(active).to_owned(),
        five_hour: active.five_hour.as_ref().map(|window| bar(window, false)),
        seven_day: active.seven_day.as_ref().map(|window| bar(window, false)),
        signed_out: false,
    })
}

/// The chip of `active`, whose sign-in was refused: its name and the tag,
/// no stale usage.
fn signed_out_chip(active: &AccountSnapshot) -> Chip {
    Chip {
        name: display_name(active).to_owned(),
        five_hour: None,
        seven_day: None,
        signed_out: true,
    }
}

/// The tag after a row's name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tag {
    InUse,
    SignInRefused,
    OutOfRotation,
}

impl Tag {
    pub(crate) fn text(self) -> &'static str {
        match self {
            Self::InUse => "In use",
            Self::SignInRefused => "sign-in refused",
            Self::OutOfRotation => "out of rotation",
        }
    }
}

/// One window line of a row: `5h` or `7d`, its bar and its reset text,
/// which is danger-coloured when the window is used up.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WindowLine {
    pub(crate) label: &'static str,
    pub(crate) bar: Bar,
    pub(crate) reset: String,
    pub(crate) used_up: bool,
}

/// A row's Switch button.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SwitchButton {
    pub(crate) label: &'static str,
    pub(crate) enabled: bool,
}

/// One account's row in the popover.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Row {
    pub(crate) number: u32,
    pub(crate) name: String,
    /// The muted text after the name: the email beside an alias, and a
    /// stale row's age as `· {age}`.
    pub(crate) detail: Option<String>,
    pub(crate) active: bool,
    pub(crate) dim: bool,
    pub(crate) tag: Option<Tag>,
    pub(crate) reason: Option<&'static str>,
    pub(crate) windows: Vec<WindowLine>,
    pub(crate) switch: Option<SwitchButton>,
}

/// `account`'s row at `now` in time zone `tz`, while the switch to
/// `switching` is pending (none pending: `None`) and `writing` says an
/// account write is pending. The active row and a row whose sign-in was
/// refused offer no Switch; while any write is pending, no row's Switch
/// takes a click.
pub(crate) fn row<Tz: TimeZone>(
    account: &AccountSnapshot,
    switching: Option<u32>,
    writing: bool,
    now: DateTime<Utc>,
    tz: &Tz,
) -> Row
where
    Tz::Offset: Display,
{
    let relogin = account.status == AccountUsageStatus::ReloginRequired;
    // Only a dimmed row's bars go faint; the account in use keeps its
    // levels even out of rotation.
    let dim = !account.active && (relogin || account.disabled);
    let tag = if account.active {
        Some(Tag::InUse)
    } else if relogin {
        Some(Tag::SignInRefused)
    } else if account.disabled {
        Some(Tag::OutOfRotation)
    } else {
        None
    };
    let reason = if relogin {
        Some(RELOGIN_LINE)
    } else if account.status == AccountUsageStatus::StoreUnwritable {
        Some(STORE_READ_ONLY)
    } else {
        None
    };
    let windows = if relogin {
        Vec::new()
    } else {
        [("5h", &account.five_hour), ("7d", &account.seven_day)]
            .into_iter()
            .filter_map(|(label, window)| {
                window
                    .as_ref()
                    .map(|window| window_line(label, window, dim, now, tz))
            })
            .collect()
    };
    let switch = (!account.active && !relogin).then(|| SwitchButton {
        label: if switching == Some(account.number) {
            SWITCHING_LABEL
        } else {
            SWITCH_LABEL
        },
        enabled: switching.is_none() && !writing,
    });
    let age = account
        .fetched_at
        .map(|at| now - at)
        .filter(|elapsed| *elapsed > TimeDelta::minutes(STALE_AFTER_MINUTES))
        .map(age_text);
    let alias = alias(account);
    let detail = match (alias, age) {
        (Some(_), Some(age)) => Some(format!("{} · {age}", account.email)),
        (Some(_), None) => Some(account.email.clone()),
        (None, Some(age)) => Some(format!("· {age}")),
        (None, None) => None,
    };
    Row {
        number: account.number,
        name: display_name(account).to_owned(),
        detail,
        active: account.active,
        dim,
        tag,
        reason,
        windows,
        switch,
    }
}

/// The line for `window`, labelled `label`, of a row `dim` or not.
fn window_line<Tz: TimeZone>(
    label: &'static str,
    window: &AccountUsageWindow,
    dim: bool,
    now: DateTime<Utc>,
    tz: &Tz,
) -> WindowLine
where
    Tz::Offset: Display,
{
    let used_up = window.pct >= 100.0;
    let reset = match (used_up, window.resets_at) {
        (true, Some(at)) => format!("used up · back {}", clock(at, now, tz)),
        (true, None) => "used up".to_owned(),
        (false, Some(at)) => format!("resets {}", clock(at, now, tz)),
        (false, None) => "not started".to_owned(),
    };
    WindowLine {
        label,
        bar: bar(window, dim),
        reset,
        used_up,
    }
}

/// `at` on the wall clock of `tz`: `HH:MM` on `now`'s day, else
/// `Ddd HH:MM`.
fn clock<Tz: TimeZone>(at: DateTime<Utc>, now: DateTime<Utc>, tz: &Tz) -> String
where
    Tz::Offset: Display,
{
    let local = at.with_timezone(tz);
    if local.date_naive() == now.with_timezone(tz).date_naive() {
        local.format("%H:%M").to_string()
    } else {
        local.format("%a %H:%M").to_string()
    }
}

/// The popover's title: the agent's display name and " accounts", or
/// "Accounts" before the host listed an agent.
pub(crate) fn header(agent_label: Option<&str>) -> String {
    agent_label.map_or_else(
        || "Accounts".to_owned(),
        |label| format!("{label} accounts"),
    )
}

/// The threshold one step from `current` (5 percentage points, within 50 to
/// 95), or `None` past the bound.
pub(crate) fn threshold_step(current: f64, up: bool) -> Option<f64> {
    let next = if up {
        (current / THRESHOLD_STEP).floor() * THRESHOLD_STEP + THRESHOLD_STEP
    } else {
        (current / THRESHOLD_STEP).ceil() * THRESHOLD_STEP - THRESHOLD_STEP
    };
    (THRESHOLD_MIN..=THRESHOLD_MAX)
        .contains(&next)
        .then_some(next)
}

/// The hysteresis one step from `current` (1 percentage point, within 0 to
/// 50), or `None` past the bound; a value outside the bounds steps back
/// onto the nearer one.
pub(crate) fn hysteresis_step(current: f64, up: bool) -> Option<f64> {
    let next = if up {
        (current.floor() + HYSTERESIS_STEP).max(HYSTERESIS_MIN)
    } else {
        (current.ceil() - HYSTERESIS_STEP).min(HYSTERESIS_MAX)
    };
    (HYSTERESIS_MIN..=HYSTERESIS_MAX)
        .contains(&next)
        .then_some(next)
}

/// What stands in a row's Remove place on the Settings page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Removal {
    /// The Remove button.
    Remove,
    /// The account in use: the text [`SWITCH_FIRST`], no button.
    SwitchFirst,
}

/// The buttons a Settings page row has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RowActions {
    /// Switch, as the popover offers it.
    pub(crate) switch: bool,
    pub(crate) up: bool,
    pub(crate) down: bool,
    pub(crate) removal: Removal,
}

/// The buttons of `row`, place `index` of `count` rows: Switch where the
/// popover offers it, Up but on the first, Down but on the last, and Remove
/// but on the account in use, which says [`SWITCH_FIRST`] instead.
pub(crate) fn row_actions(row: &Row, index: usize, count: usize) -> RowActions {
    RowActions {
        switch: row.switch.is_some(),
        up: index > 0,
        down: index + 1 < count,
        removal: if row.active {
            Removal::SwitchFirst
        } else {
            Removal::Remove
        },
    }
}

/// The rotation order of `accounts` with account `number` moved one place
/// `up` or down; `None` when it is not listed or already at that end.
pub(crate) fn moved_order(accounts: &[AccountSnapshot], number: u32, up: bool) -> Option<Vec<u32>> {
    let mut order: Vec<u32> = accounts.iter().map(|account| account.number).collect();
    let at = order.iter().position(|listed| *listed == number)?;
    let to = if up {
        at.checked_sub(1)?
    } else {
        Some(at + 1).filter(|to| *to < order.len())?
    };
    order.swap(at, to);
    Some(order)
}

/// What an edit of the alias field sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AliasChange {
    Set(String),
    Clear,
}

impl AliasChange {
    /// The `alias` of the `SetAccountAlias` it sends.
    pub(crate) fn alias(self) -> Option<String> {
        match self {
            Self::Set(alias) => Some(alias),
            Self::Clear => None,
        }
    }
}

/// What an edit of the alias field to `typed` sends over the alias
/// `current`, both read trimmed: `None` when it changes nothing, a blank
/// field clearing it.
pub(crate) fn alias_change(current: Option<&str>, typed: &str) -> Option<AliasChange> {
    let typed = typed.trim();
    let current = current.map_or("", str::trim);
    (typed != current).then(|| {
        if typed.is_empty() {
            AliasChange::Clear
        } else {
            AliasChange::Set(typed.to_owned())
        }
    })
}

/// The note under Add current login, naming the agent `agent_label` (the
/// descriptor's label), or "the agent" before the host listed one.
pub(crate) fn add_login_note(agent_label: Option<&str>) -> String {
    let agent = agent_label.unwrap_or("the agent");
    format!(
        "Saves the login {agent} is using now. Don't /logout first: logging out can revoke the account you're leaving."
    )
}

/// One segment of a Settings choice row: the value it sends, its label and
/// whether it shows selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Segment<T> {
    pub(crate) value: T,
    pub(crate) label: String,
    pub(crate) selected: bool,
}

/// A cooldown of `secs` as a segment label: [`OFF_LABEL`], `{n} min`, or
/// `{n} s` for a value of no whole minutes.
fn cooldown_label(secs: u64) -> String {
    if secs == 0 {
        OFF_LABEL.to_owned()
    } else if secs.is_multiple_of(60) {
        format!("{} min", secs / 60)
    } else {
        format!("{secs} s")
    }
}

/// The Cooldown row's segments, in seconds, `current_secs` selected; a
/// current value off the list is a segment of its own, in its place by
/// value.
pub(crate) fn cooldown_segments(current_secs: u64) -> Vec<Segment<u64>> {
    let mut choices: Vec<u64> = COOLDOWN_MINUTES
        .iter()
        .map(|minutes| minutes * 60)
        .collect();
    if !choices.contains(&current_secs) {
        choices.push(current_secs);
        choices.sort_unstable();
    }
    choices
        .into_iter()
        .map(|value| Segment {
            value,
            label: cooldown_label(value),
            selected: value == current_secs,
        })
        .collect()
}

/// The Strategy row's segments, `current` selected; a strategy this build
/// does not know selects neither.
pub(crate) fn strategy_segments(current: AutoSwitchStrategy) -> Vec<Segment<AutoSwitchStrategy>> {
    STRATEGY_CHOICES
        .iter()
        .map(|(value, label)| Segment {
            value: *value,
            label: (*label).to_owned(),
            selected: *value == current,
        })
        .collect()
}

/// The Forecast window row's segments, in minutes, `current` selected; none
/// is while the host reports none or one off the list.
pub(crate) fn window_segments(current: Option<u32>) -> Vec<Segment<u32>> {
    FORECAST_WINDOWS
        .iter()
        .map(|(value, label)| Segment {
            value: *value,
            label: (*label).to_owned(),
            selected: Some(*value) == current,
        })
        .collect()
}

/// Why the engine is paused, while it is.
pub(crate) fn paused_line(auto: Option<&AutoSwitchState>) -> Option<String> {
    auto?
        .paused_reason
        .as_deref()
        .map(|reason| format!("Auto-switch paused: {reason}"))
}

/// The fleet forecast, while the engine sends a verdict.
pub(crate) fn forecast<Tz: TimeZone>(
    auto: Option<&AutoSwitchState>,
    now: DateTime<Utc>,
    tz: &Tz,
) -> Option<String>
where
    Tz::Offset: Display,
{
    let verdict = auto?.fleet_verdict.as_ref()?;
    let mut parts = Vec::new();
    match (verdict.runs_dry, verdict.blocked_at) {
        (false, _) => parts.push("Rotation lasts the next 7 days".to_owned()),
        (true, Some(at)) if at > now => parts.push(format!("Runs dry at {}", clock(at, now, tz))),
        (true, _) => parts.push("All accounts used up".to_owned()),
    }
    if verdict.runs_dry
        && let Some(back) = verdict.resumes_at
    {
        parts.push(format!("back {}", clock(back, now, tz)));
    }
    parts.push(format!("{} accounts", verdict.accounts));
    Some(parts.join(" · "))
}

/// The popover's footer: when the newest fetch among `accounts` was made;
/// none before any was.
pub(crate) fn checked_line(accounts: &[AccountSnapshot], now: DateTime<Utc>) -> Option<String> {
    let newest = accounts
        .iter()
        .filter_map(|account| account.fetched_at)
        .max()?;
    Some(format!("checked {}", age_text(now - newest)))
}

/// How long ago something `elapsed` back happened: `{n}s ago`, `{n}m ago`
/// or `{n}h ago`; a time ahead of the clock reads as just now.
pub(crate) fn age_text(elapsed: TimeDelta) -> String {
    let secs = elapsed.num_seconds().max(0);
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else {
        format!("{}h ago", secs / 3600)
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test fails with the message of the precondition it lost"
)]
mod tests {
    use chrono::{DateTime, TimeDelta, TimeZone, Utc};
    use protocol::{
        AccountKind, AccountSnapshot, AccountUsageStatus, AccountUsageWindow, AutoSwitchState,
        AutoSwitchStrategy, DaemonMessage, FleetVerdict, FleetWindow,
    };

    use super::{
        AccountsState, AliasChange, Level, RELOGIN_LINE, Removal, RowActions, STORE_READ_ONLY,
        Segment, Tag, WriteAction, add_login_note, age_text, alias_change, bar, checked_line, chip,
        cooldown_segments, forecast, header, hysteresis_step, moved_order, paused_line, row,
        row_actions, strategy_segments, threshold_step, window_segments,
    };
    use crate::target::HostKey;

    /// Wednesday 4 November 2026, noon.
    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 4, 12, 0, 0)
            .single()
            .unwrap_or_default()
    }

    fn window(pct: f64, resets_at: Option<DateTime<Utc>>) -> AccountUsageWindow {
        AccountUsageWindow { pct, resets_at }
    }

    /// Account `number` aliased `alias`, both windows used a little and
    /// fetched just now.
    fn account(number: u32, alias: &str) -> AccountSnapshot {
        AccountSnapshot {
            number,
            email: format!("{alias}@example.com"),
            alias: Some(alias.to_owned()),
            organization_name: None,
            kind: AccountKind::Oauth,
            active: false,
            disabled: false,
            status: AccountUsageStatus::Ok,
            five_hour: Some(window(34.0, Some(now() + TimeDelta::hours(2)))),
            seven_day: Some(window(12.0, Some(now() + TimeDelta::days(2)))),
            model_limits: Vec::new(),
            extra_usage: None,
            fetched_at: Some(now()),
            error: None,
        }
    }

    fn active(number: u32) -> AccountSnapshot {
        AccountSnapshot {
            active: true,
            ..account(number, &format!("a{number}"))
        }
    }

    fn auto(paused: Option<&str>) -> AutoSwitchState {
        AutoSwitchState {
            enabled: true,
            threshold_pct: 90.0,
            hysteresis_pct: 5.0,
            cooldown_secs: 300,
            strategy: AutoSwitchStrategy::Best,
            rate_window_minutes: None,
            paused_reason: paused.map(str::to_owned),
            warm_up_done: true,
            last_decision: None,
            fleet_verdict: None,
        }
    }

    fn updated(accounts: Vec<AccountSnapshot>) -> DaemonMessage {
        DaemonMessage::AccountsUpdated {
            accounts,
            auto: None,
        }
    }

    fn reply(id: Option<&str>, accounts: Vec<AccountSnapshot>) -> DaemonMessage {
        DaemonMessage::Accounts {
            accounts,
            auto: None,
            request_id: id.map(str::to_owned),
        }
    }

    fn welcome() -> DaemonMessage {
        DaemonMessage::Welcome {
            protocol_version: 1,
            supported_versions: vec![1],
            features: Vec::new(),
            daemon_id: None,
            origin: None,
        }
    }

    fn in_use(state: &AccountsState, host: &HostKey) -> Option<u32> {
        state
            .accounts(host)
            .iter()
            .find(|account| account.active)
            .map(|account| account.number)
    }

    #[test]
    fn a_reply_with_the_pending_id_and_no_update_applies() {
        let mut state = AccountsState::default();
        let host = HostKey::Local;
        state.sent_list(&host, "l1".to_owned());
        let folded = state.fold(&host, &reply(Some("l1"), vec![active(1), account(2, "b")]));
        assert!(folded.changed, "the reply lands");
        assert!(!folded.ask_again);
        assert_eq!(in_use(&state, &host), Some(1));
        assert_eq!(state.accounts(&host).len(), 2);
    }

    #[test]
    fn a_reply_after_a_newer_update_is_dropped() {
        let mut state = AccountsState::default();
        let host = HostKey::Local;
        state.sent_list(&host, "l1".to_owned());
        assert!(state.fold(&host, &updated(vec![active(2)])).changed);
        let folded = state.fold(&host, &reply(Some("l1"), vec![active(1)]));
        assert!(!folded.changed, "an update came after the ask");
        assert!(!folded.ask_again, "a dropped list asks nothing");
        assert_eq!(
            in_use(&state, &host),
            Some(2),
            "the update's snapshot stays"
        );
    }

    #[test]
    fn a_reply_with_another_id_is_dropped() {
        let mut state = AccountsState::default();
        let host = HostKey::Local;
        state.sent_list(&host, "l1".to_owned());
        assert!(
            !state
                .fold(&host, &reply(Some("other"), vec![active(1)]))
                .changed
        );
        assert!(!state.fold(&host, &reply(None, vec![active(1)])).changed);
        assert!(
            state.accounts(&host).is_empty(),
            "a stray reply lands nowhere"
        );

        state.sent_list(&host, "l2".to_owned());
        assert!(
            !state
                .fold(&host, &reply(Some("l1"), vec![active(1)]))
                .changed,
            "a newer ask replaced the older one"
        );
        assert!(
            state
                .fold(&host, &reply(Some("l2"), vec![active(3)]))
                .changed
        );
        assert_eq!(in_use(&state, &host), Some(3));
        assert!(
            !state
                .fold(&host, &reply(Some("l2"), vec![active(1)]))
                .changed,
            "an ask takes one reply"
        );
    }

    #[test]
    fn a_dropped_write_reply_asks_again() {
        let mut state = AccountsState::default();
        let host = HostKey::Local;
        state.fold(&host, &updated(vec![active(1), account(2, "b")]));
        state.sent_write(&host, WriteAction::Switch(2), "s1".to_owned());
        assert_eq!(state.switching(&host), Some(2));
        state.fold(&host, &updated(vec![active(1), account(2, "b")]));
        let folded = state.fold(&host, &reply(Some("s1"), vec![account(1, "a"), active(2)]));
        assert!(folded.ask_again, "the write's own snapshot was dropped");
        assert!(folded.changed, "the write is no longer pending");
        assert_eq!(state.switching(&host), None);
        assert_eq!(in_use(&state, &host), Some(1));

        state.sent_write(&host, WriteAction::Switch(2), "s2".to_owned());
        let folded = state.fold(&host, &reply(Some("s2"), vec![account(1, "a"), active(2)]));
        assert!(folded.changed && !folded.ask_again, "no update overtook it");
        assert_eq!(in_use(&state, &host), Some(2));
    }

    #[test]
    fn a_dropped_write_reply_asks_again_for_every_action() {
        let actions = [
            WriteAction::Switch(2),
            WriteAction::AddLogin,
            WriteAction::Remove(2),
            WriteAction::Alias(2),
            WriteAction::Order,
            WriteAction::AutoSwitch,
            WriteAction::Rules,
        ];
        for action in actions {
            let mut state = AccountsState::default();
            let host = HostKey::Local;
            state.fold(&host, &updated(vec![active(1), account(2, "b")]));
            state.sent_write(&host, action, "w1".to_owned());
            state.fold(&host, &updated(vec![active(1), account(2, "b")]));
            let folded = state.fold(&host, &reply(Some("w1"), vec![account(1, "a"), active(2)]));
            assert!(folded.ask_again, "{action:?} asks again");
            assert!(folded.changed, "{action:?} is no longer pending");
            assert!(!state.writing(&host), "{action:?} ended");
        }
    }

    #[test]
    fn a_pending_write_is_not_a_switch() {
        let mut state = AccountsState::default();
        let host = HostKey::Local;
        state.sent_write(&host, WriteAction::AutoSwitch, "w1".to_owned());
        assert!(state.writing(&host));
        assert_eq!(state.switching(&host), None, "not a switch");
    }

    #[test]
    fn an_update_always_applies() {
        let mut state = AccountsState::default();
        let host = HostKey::Local;
        assert!(state.fold(&host, &updated(vec![active(1)])).changed);
        assert_eq!(in_use(&state, &host), Some(1));
        state.sent_list(&host, "l1".to_owned());
        state.sent_write(&host, WriteAction::Switch(2), "s1".to_owned());
        assert!(state.fold(&host, &updated(vec![active(4)])).changed);
        assert_eq!(
            in_use(&state, &host),
            Some(4),
            "pending asks hold no update back"
        );
    }

    #[test]
    fn a_welcome_clears_the_host() {
        let mut state = AccountsState::default();
        let local = HostKey::Local;
        let other = HostKey::Remote("box".to_owned());
        state.fold(&local, &updated(vec![active(1)]));
        state.fold(&other, &updated(vec![active(2)]));
        state.sent_list(&local, "l1".to_owned());
        assert!(state.fold(&local, &welcome()).changed);
        assert!(
            state.accounts(&local).is_empty(),
            "the Welcome forgets the rows"
        );
        assert_eq!(in_use(&state, &other), Some(2), "and only that host's");
        assert!(
            !state
                .fold(&local, &reply(Some("l1"), vec![active(1)]))
                .changed,
            "an ask of the old connection lands nowhere"
        );
    }

    #[test]
    fn a_refusal_of_the_pending_write_shows_until_the_next_reply() {
        let mut state = AccountsState::default();
        let host = HostKey::Local;
        state.fold(&host, &updated(vec![active(1), account(2, "b")]));
        state.sent_write(&host, WriteAction::Switch(2), "s1".to_owned());
        assert!(!state.refused(&host, "other", "no"), "not the write's id");
        assert!(state.refused(&host, "s1", "busy"));
        assert_eq!(state.error(&host), Some("busy"));
        assert_eq!(state.switching(&host), None);
        state.sent_list(&host, "l1".to_owned());
        state.fold(&host, &reply(Some("l1"), vec![active(1)]));
        assert_eq!(state.error(&host), None, "the next reply clears it");
    }

    #[test]
    fn no_chip_without_the_feature() {
        assert_eq!(chip(false, &[active(1)]), None);
    }

    #[test]
    fn no_chip_with_no_accounts() {
        assert_eq!(chip(true, &[]), None);
    }

    #[test]
    fn the_chip_reads_no_active_account_without_an_active_row() {
        let shown = chip(true, &[account(1, "main")]).map(|chip| chip.text());
        assert_eq!(shown.as_deref(), Some("No active account"));
    }

    #[test]
    fn the_chip_names_the_active_account_and_its_windows() {
        let mut main = account(1, "main");
        main.active = true;
        let shown = chip(true, &[account(2, "ops"), main]).map(|chip| chip.text());
        assert_eq!(shown.as_deref(), Some("main · 5h 34% · 7d 12%"));
        let mut bare = active(3);
        bare.alias = None;
        let shown = chip(true, &[bare]).map(|chip| chip.name);
        assert_eq!(
            shown.as_deref(),
            Some("a3@example.com"),
            "no alias: the email"
        );
    }

    #[test]
    fn no_paused_line_while_running() {
        assert_eq!(paused_line(None), None);
        assert_eq!(paused_line(Some(&auto(None))), None, "running, not paused");
        assert_eq!(
            paused_line(Some(&auto(Some("held by another engine")))).as_deref(),
            Some("Auto-switch paused: held by another engine")
        );
    }

    #[test]
    fn threshold_step_moves_by_five_within_bounds() {
        let rows = [
            (80.0, true, Some(85.0)),
            (80.0, false, Some(75.0)),
            (82.5, true, Some(85.0)),
            (82.5, false, Some(80.0)),
            (95.0, true, None),
            (50.0, false, None),
            (99.9, false, Some(95.0)),
            (99.9, true, None),
            (49.0, true, Some(50.0)),
        ];
        for (current, up, want) in rows {
            assert_eq!(threshold_step(current, up), want, "{current} up={up}");
        }
    }

    #[test]
    fn no_forecast_without_a_fleet_verdict() {
        assert_eq!(forecast(None, now(), &Utc), None);
        assert_eq!(forecast(Some(&auto(None)), now(), &Utc), None);
        let verdict = FleetVerdict {
            window: FleetWindow::FiveHour,
            members: 3,
            accounts: 3,
            remaining: 10.0,
            rate: 1.0,
            rate_window: FleetWindow::FiveHour,
            runs_dry: false,
            blocked_at: None,
            resumes_at: None,
            first_reset: None,
        };
        let lasting = AutoSwitchState {
            fleet_verdict: Some(verdict.clone()),
            ..auto(None)
        };
        assert_eq!(
            forecast(Some(&lasting), now(), &Utc).as_deref(),
            Some("Rotation lasts the next 7 days · 3 accounts")
        );
        let dry = AutoSwitchState {
            fleet_verdict: Some(FleetVerdict {
                runs_dry: true,
                blocked_at: Some(now() + TimeDelta::hours(6)),
                resumes_at: Some(now() + TimeDelta::hours(8)),
                ..verdict
            }),
            ..auto(None)
        };
        assert_eq!(
            forecast(Some(&dry), now(), &Utc).as_deref(),
            Some("Runs dry at 18:00 · back 20:00 · 3 accounts")
        );
    }

    #[test]
    fn no_switch_on_the_active_or_a_relogin_row() {
        let in_use = row(&active(1), None, false, now(), &Utc);
        assert_eq!(in_use.switch, None, "the active row");
        assert_eq!(in_use.tag, Some(Tag::InUse));
        assert!(in_use.active);

        let mut refused = account(5, "client");
        refused.status = AccountUsageStatus::ReloginRequired;
        let refused = row(&refused, None, false, now(), &Utc);
        assert_eq!(refused.switch, None, "a relogin row");
        assert_eq!(refused.tag, Some(Tag::SignInRefused));
        assert_eq!(refused.reason, Some(RELOGIN_LINE));
        assert!(refused.windows.is_empty(), "no bars");
        assert!(refused.dim);

        let ready = row(&account(2, "ops"), None, false, now(), &Utc);
        let button = ready.switch.expect("an ordinary row offers Switch");
        assert_eq!((button.label, button.enabled), ("Switch", true));
        assert_eq!(ready.windows.len(), 2);

        let pending = row(&account(2, "ops"), Some(2), false, now(), &Utc).switch;
        let pending = pending.expect("the pending row keeps its button");
        assert_eq!((pending.label, pending.enabled), ("Switching…", false));
        let other = row(&account(3, "lab"), Some(2), false, now(), &Utc).switch;
        assert_eq!(other.map(|button| button.enabled), Some(false));

        let busy = row(&account(2, "ops"), None, true, now(), &Utc).switch;
        assert_eq!(
            busy.map(|button| (button.label, button.enabled)),
            Some(("Switch", false)),
            "a write other than a switch only disables it"
        );

        let mut spare = account(4, "spare");
        spare.disabled = true;
        let spare = row(&spare, None, false, now(), &Utc);
        assert_eq!(spare.tag, Some(Tag::OutOfRotation));
        assert!(spare.switch.is_some(), "still switchable");
        assert!(
            spare
                .windows
                .iter()
                .all(|line| line.bar.level == Level::Faint)
        );
    }

    #[test]
    fn a_stale_row_shows_its_age_after_its_email() {
        let mut ops = account(2, "ops");
        ops.fetched_at = Some(now() - TimeDelta::minutes(30));
        assert_eq!(
            row(&ops, None, false, now(), &Utc).detail.as_deref(),
            Some("ops@example.com · 30m ago")
        );
        ops.fetched_at = Some(now() - TimeDelta::minutes(5));
        assert_eq!(
            row(&ops, None, false, now(), &Utc).detail.as_deref(),
            Some("ops@example.com"),
            "fresh enough"
        );
        ops.alias = None;
        ops.fetched_at = Some(now() - TimeDelta::hours(2));
        let bare = row(&ops, None, false, now(), &Utc);
        assert_eq!(bare.name, "ops@example.com");
        assert_eq!(bare.detail.as_deref(), Some("· 2h ago"));
    }

    #[test]
    fn a_read_only_store_row_shows_its_reason() {
        let mut lab = account(3, "lab");
        lab.status = AccountUsageStatus::StoreUnwritable;
        assert_eq!(
            row(&lab, None, false, now(), &Utc).reason,
            Some(STORE_READ_ONLY)
        );
        assert_eq!(
            row(&account(3, "lab"), None, false, now(), &Utc).reason,
            None
        );
    }

    #[test]
    fn age_text_reads_seconds_minutes_hours() {
        assert_eq!(age_text(TimeDelta::seconds(12)), "12s ago");
        assert_eq!(age_text(TimeDelta::seconds(190)), "3m ago");
        assert_eq!(age_text(TimeDelta::seconds(7300)), "2h ago");
        assert_eq!(age_text(TimeDelta::seconds(-5)), "0s ago", "a clock skew");
    }

    #[test]
    fn reset_texts_name_today_a_later_day_not_started_and_used_up() {
        let mut lab = account(3, "lab");
        lab.five_hour = Some(window(
            40.0,
            Some(now() + TimeDelta::hours(5) + TimeDelta::minutes(40)),
        ));
        lab.seven_day = Some(window(20.0, Some(now() + TimeDelta::hours(21))));
        let resets: Vec<String> = row(&lab, None, false, now(), &Utc)
            .windows
            .into_iter()
            .map(|line| line.reset)
            .collect();
        assert_eq!(resets, ["resets 17:40", "resets Thu 09:00"]);

        lab.five_hour = Some(window(0.0, None));
        lab.seven_day = Some(window(
            100.0,
            Some(now() + TimeDelta::hours(6) + TimeDelta::minutes(5)),
        ));
        let lines = row(&lab, None, false, now(), &Utc).windows;
        let shown: Vec<(&str, bool)> = lines
            .iter()
            .map(|line| (line.reset.as_str(), line.used_up))
            .collect();
        assert_eq!(
            shown,
            [("not started", false), ("used up · back 18:05", true)]
        );
    }

    #[test]
    fn bar_levels_and_least_fill() {
        let level = |pct: f64| bar(&window(pct, None), false).level;
        assert_eq!(level(95.0), Level::Danger);
        assert_eq!(level(90.0), Level::Danger);
        assert_eq!(level(85.0), Level::Asking);
        assert_eq!(level(80.0), Level::Asking);
        assert_eq!(level(50.0), Level::Normal);
        assert_eq!(bar(&window(95.0, None), true).level, Level::Faint);
        let fill = |pct: f64| bar(&window(pct, None), false).fill;
        assert!((fill(0.0) - 0.01).abs() < f32::EPSILON, "at least 1 %");
        assert!((fill(150.0) - 1.0).abs() < f32::EPSILON);
        assert!((fill(34.0) - 0.34).abs() < 1e-6);
        assert_eq!(bar(&window(34.4, None), false).pct, "34%");
    }

    #[test]
    fn checked_line_names_the_newest_fetch() {
        let mut older = account(1, "a");
        older.fetched_at = Some(now() - TimeDelta::minutes(5));
        let mut newer = account(2, "b");
        newer.fetched_at = Some(now() - TimeDelta::seconds(12));
        assert_eq!(
            checked_line(&[older.clone(), newer], now()).as_deref(),
            Some("checked 12s ago")
        );
        older.fetched_at = None;
        assert_eq!(checked_line(&[older], now()), None, "nothing fetched yet");
    }

    #[test]
    fn header_names_the_default_agent() {
        assert_eq!(header(Some("Agent")), "Agent accounts");
        assert_eq!(header(None), "Accounts");
    }

    #[test]
    fn the_chip_of_a_signed_out_active_account_says_so() {
        let mut main = active(1);
        main.status = AccountUsageStatus::ReloginRequired;
        let shown = chip(true, &[main]).expect("the account in use still shows");
        assert_eq!(shown.text(), "a1 · sign-in refused");
        assert!(shown.signed_out);
        assert_eq!(shown.five_hour, None, "no stale usage");
        assert_eq!(shown.seven_day, None);
    }

    #[test]
    fn row_actions_follow_the_place_and_the_account_in_use() {
        let first = row(&active(1), None, false, now(), &Utc);
        assert_eq!(
            row_actions(&first, 0, 3),
            RowActions {
                switch: false,
                up: false,
                down: true,
                removal: Removal::SwitchFirst,
            },
            "the first row is the one in use: no Switch, no Up, Switch first"
        );
        let middle = row(&account(2, "ops"), None, false, now(), &Utc);
        assert_eq!(
            row_actions(&middle, 1, 3),
            RowActions {
                switch: true,
                up: true,
                down: true,
                removal: Removal::Remove,
            }
        );
        let last = row(&account(3, "lab"), None, false, now(), &Utc);
        let actions = row_actions(&last, 2, 3);
        assert!(actions.up && !actions.down, "no Down on the last row");
        let only = row_actions(&last, 0, 1);
        assert!(!only.up && !only.down, "a lone row moves nowhere");
        let mut refused = account(4, "client");
        refused.status = AccountUsageStatus::ReloginRequired;
        let refused = row(&refused, None, false, now(), &Utc);
        assert!(!row_actions(&refused, 1, 3).switch, "as in the popover");
        assert_eq!(row_actions(&refused, 1, 3).removal, Removal::Remove);
    }

    #[test]
    fn a_move_swaps_the_row_with_its_neighbour() {
        let accounts = [active(1), account(2, "b"), account(3, "c")];
        assert_eq!(moved_order(&accounts, 2, true), Some(vec![2, 1, 3]));
        assert_eq!(moved_order(&accounts, 2, false), Some(vec![1, 3, 2]));
        assert_eq!(
            moved_order(&accounts, 1, true),
            None,
            "the first goes no higher"
        );
        assert_eq!(moved_order(&accounts, 3, false), None, "the last no lower");
        assert_eq!(moved_order(&accounts, 9, true), None, "an unknown account");
    }

    #[test]
    fn hysteresis_step_moves_by_one_within_bounds() {
        let rows = [
            (5.0, true, Some(6.0)),
            (5.0, false, Some(4.0)),
            (4.5, true, Some(5.0)),
            (4.5, false, Some(4.0)),
            (50.0, true, None),
            (0.0, false, None),
            (60.0, false, Some(50.0)),
            (-3.0, true, Some(0.0)),
        ];
        for (current, up, want) in rows {
            assert_eq!(hysteresis_step(current, up), want, "{current} up={up}");
        }
    }

    fn shown<T: Copy>(segments: &[Segment<T>]) -> Vec<(T, &str, bool)> {
        segments
            .iter()
            .map(|segment| (segment.value, segment.label.as_str(), segment.selected))
            .collect()
    }

    #[test]
    fn cooldown_segments_list_the_choices_and_the_current_value() {
        assert_eq!(
            shown(&cooldown_segments(300)),
            [
                (0, "Off", false),
                (60, "1 min", false),
                (300, "5 min", true),
                (900, "15 min", false),
                (1800, "30 min", false),
                (3600, "60 min", false),
            ]
        );
        let extra = cooldown_segments(600);
        assert_eq!(extra.len(), 7, "a value off the list is its own segment");
        assert_eq!(
            shown(&extra)[3],
            (600, "10 min", true),
            "in its place by value"
        );
        assert!(
            extra.iter().filter(|segment| segment.selected).count() == 1,
            "only the current value is selected"
        );
        assert_eq!(shown(&cooldown_segments(90))[2], (90, "90 s", true));
        assert_eq!(shown(&cooldown_segments(0))[0], (0, "Off", true));
    }

    #[test]
    fn strategy_segments_select_the_known_strategy_only() {
        assert_eq!(
            shown(&strategy_segments(AutoSwitchStrategy::Best)),
            [
                (AutoSwitchStrategy::Best, "Most headroom", true),
                (AutoSwitchStrategy::ConsumeFirst, "Resets soonest", false),
            ]
        );
        assert!(strategy_segments(AutoSwitchStrategy::ConsumeFirst)[1].selected);
        assert!(
            strategy_segments(AutoSwitchStrategy::Unknown)
                .iter()
                .all(|segment| !segment.selected),
            "a strategy this build does not know selects neither"
        );
    }

    #[test]
    fn window_segments_select_the_current_window() {
        assert_eq!(
            shown(&window_segments(Some(120))),
            [
                (30, "30 min", false),
                (60, "1 h", false),
                (120, "2 h", true),
                (180, "3 h", false),
            ]
        );
        assert!(
            window_segments(None)
                .iter()
                .all(|segment| !segment.selected),
            "no window reported selects none"
        );
        assert!(
            window_segments(Some(45))
                .iter()
                .all(|segment| !segment.selected),
            "a window off the list selects none"
        );
    }

    #[test]
    fn an_alias_edit_sends_only_a_change() {
        assert_eq!(alias_change(Some("ops"), "ops"), None, "unchanged");
        assert_eq!(alias_change(Some("ops"), " ops "), None, "spaces only");
        assert_eq!(
            alias_change(Some("ops"), "work"),
            Some(AliasChange::Set("work".to_owned()))
        );
        assert_eq!(
            alias_change(Some("ops"), "  "),
            Some(AliasChange::Clear),
            "blank clears"
        );
        assert_eq!(alias_change(None, ""), None, "nothing to clear");
        assert_eq!(
            alias_change(None, "lab").and_then(AliasChange::alias),
            Some("lab".to_owned())
        );
        assert_eq!(AliasChange::Clear.alias(), None);
    }

    #[test]
    fn the_add_login_note_names_the_agent() {
        assert_eq!(
            add_login_note(Some("Agent")),
            "Saves the login Agent is using now. Don't /logout first: logging out can revoke the account you're leaving."
        );
        assert!(add_login_note(None).starts_with("Saves the login the agent is using now."));
    }

    #[test]
    fn an_active_account_out_of_rotation_keeps_its_bar_colours() {
        let mut main = active(1);
        main.disabled = true;
        let shown = row(&main, None, false, now(), &Utc);
        assert!(!shown.dim);
        let levels: Vec<Level> = shown.windows.iter().map(|line| line.bar.level).collect();
        assert_eq!(levels, [Level::Normal, Level::Normal]);
    }
}
