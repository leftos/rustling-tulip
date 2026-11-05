//! The accounts seam: the saved agent logins a host switches between, and
//! their usage as last read.
//!
//! A plugin that keeps accounts hands the daemon an [`AccountsService`] for
//! the store [`AccountsConfig`] names. Every method blocks (it reads files,
//! and later ones send HTTP requests), so the daemon calls each one off its
//! async threads. What crosses the seam is plain data: the rows below, which
//! the daemon maps onto its wire types, never a plugin's own types.

use chrono::{DateTime, Utc};
use protocol::AutoSwitchStrategy;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

/// Where a plugin's account service keeps its store, and where its requests
/// go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountsConfig {
    /// The account store's root folder. It may not exist yet.
    pub root: PathBuf,
    /// An origin (`scheme://host:port`, no path) every request the service
    /// sends goes to in place of the plugin's own endpoints: the tests' local
    /// server. Plain `http` is accepted only here; `None` in production.
    pub base_url: Option<String>,
    /// The agent's own config home, overriding the one the plugin resolves
    /// from the environment: tests only; `None` in production.
    pub agent_home: Option<PathBuf>,
}

impl AccountsConfig {
    /// The store at `root`, reached at the plugin's own endpoints.
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            base_url: None,
            agent_home: None,
        }
    }
}

/// Which accounts a poll reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PollScope {
    /// Every account whose own schedule says it is due.
    OnDemand,
}

/// What a poll found: the store's snapshot after it, and when the next poll
/// is worth sending.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PollReport {
    pub snapshot: AccountsSnapshot,
    /// How long until the next poll; `None` when the store has no account,
    /// so there is nothing to poll until one is added.
    pub next_poll_in: Option<Duration>,
}

/// Why an account service could not answer. The message names no secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountsError {
    pub message: String,
}

impl AccountsError {
    /// An error saying `message`.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for AccountsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for AccountsError {}

/// What kind of failure an account write met.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AccountsFailureKind {
    /// A lock on the store or on the agent's files stayed held: retry in a
    /// few seconds.
    Busy,
    /// The service is shutting down and takes no write.
    Draining,
    /// No account has the number asked for.
    UnknownAccount,
    /// The write was refused as asked: the active account, a store that
    /// takes no write, a login that cannot be added, an invalid value.
    Refused,
    /// This agent's accounts do not support the write.
    Unsupported,
    /// The write failed.
    Failed,
}

/// Why an account write did not happen or did not finish. The message names
/// no secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountsFailure {
    pub kind: AccountsFailureKind,
    /// The plugin's own name for the failure, such as `locks_busy`.
    pub code: String,
    /// One line for the user.
    pub message: String,
}

impl AccountsFailure {
    /// The failure of a write this agent's accounts do not support.
    #[must_use]
    pub fn unsupported() -> Self {
        Self {
            kind: AccountsFailureKind::Unsupported,
            code: "unsupported".to_owned(),
            message: "this agent's accounts do not support that".to_owned(),
        }
    }
}

impl fmt::Display for AccountsFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for AccountsFailure {}

/// An account as a switch names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountRef {
    /// The account's number in the store; `None` for a login no stored
    /// account has.
    pub number: Option<u32>,
    pub email: String,
}

/// What a switch came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchReport {
    /// Whether the login moved; `false` when the account already was the
    /// active one, a no-op.
    pub switched: bool,
    /// The login before the switch; `None` when there was none to name.
    pub from: Option<AccountRef>,
    /// The login now.
    pub to: AccountRef,
    /// What the switch did that the user should know of.
    pub warnings: Vec<String>,
    /// How long a running agent may go on with the login it had, when it
    /// caches its read of the login (about 30 s for a login kept in the
    /// macOS Keychain); `None` when the switch reaches it at once, or moved
    /// nothing.
    pub takes_effect_within: Option<Duration>,
}

/// A plugin's accounts, as the daemon holds them: shared, and called from
/// several threads at once, so it holds no lock across calls. Every method
/// blocks. Every write defaults to [`AccountsFailure::unsupported`]; a write
/// answering `bool` says whether anything changed.
pub trait AccountsService: Send + Sync {
    /// Every account in the store, in the store's order, with the usage it
    /// last saved for each, and the auto-switch state where it can run.
    /// Sends no request: a missing or empty store is an empty snapshot.
    ///
    /// # Errors
    ///
    /// The store could not be opened or read.
    fn snapshot(&self) -> Result<AccountsSnapshot, AccountsError>;

    /// Reads the usage of the accounts `scope` names that are due, saves it
    /// in the store, and returns the snapshot after it with the wait until
    /// the next poll. A missing or empty store sends no request and is an
    /// empty snapshot with no next poll.
    ///
    /// # Errors
    ///
    /// The store or its settings could not be read, or the poll could not
    /// save what it read.
    fn poll(&self, scope: PollScope) -> Result<PollReport, AccountsError>;

    /// Refuses every new credential spend from now on and waits up to `bound`
    /// for the ones in flight: `true` when none is left.
    fn begin_shutdown(&self, bound: Duration) -> bool;

    /// Switches the agent's login to account `number`. Blocks, for lock waits
    /// and a network check of the outgoing login.
    ///
    /// # Errors
    ///
    /// The switch was refused or failed; nothing changed unless the failure
    /// says otherwise.
    fn switch_to(&self, number: u32) -> Result<SwitchReport, AccountsFailure> {
        let _ = number;
        Err(AccountsFailure::unsupported())
    }

    /// Adds the agent's current login to the store, or refreshes the stored
    /// account it already is, and returns that account's number. Blocks.
    ///
    /// # Errors
    ///
    /// The login could not be added.
    fn add_current_login(&self) -> Result<u32, AccountsFailure> {
        Err(AccountsFailure::unsupported())
    }

    /// Removes account `number` from the store. Blocks.
    ///
    /// # Errors
    ///
    /// The account is unknown or in use, or the removal failed.
    fn remove_account(&self, number: u32) -> Result<bool, AccountsFailure> {
        let _ = number;
        Err(AccountsFailure::unsupported())
    }

    /// Sets account `number`'s alias, or removes it with `None`. Blocks.
    ///
    /// # Errors
    ///
    /// The account is unknown, the alias is refused, or the write failed.
    fn set_alias(&self, number: u32, alias: Option<String>) -> Result<bool, AccountsFailure> {
        let _ = (number, alias);
        Err(AccountsFailure::unsupported())
    }

    /// Sets the rotation order to `order`, which names every account in it
    /// once. Blocks.
    ///
    /// # Errors
    ///
    /// The order is not a permutation of the current one, or the write
    /// failed.
    fn set_order(&self, order: Vec<u32>) -> Result<bool, AccountsFailure> {
        let _ = order;
        Err(AccountsFailure::unsupported())
    }

    /// Turns auto-switch on or off, sets the usage, in percent, at which it
    /// switches away, or both; a `None` keeps its value. Blocks.
    ///
    /// # Errors
    ///
    /// The threshold is refused, auto-switch is unsupported here, or the
    /// settings could not be written.
    fn set_auto_switch(
        &self,
        enabled: Option<bool>,
        threshold_pct: Option<f64>,
    ) -> Result<bool, AccountsFailure> {
        let _ = (enabled, threshold_pct);
        Err(AccountsFailure::unsupported())
    }

    /// Sets auto-switch's rules: the hysteresis, in percentage points, the
    /// least time between two switches, how the engine picks its target, and
    /// the span the fleet forecast reads usage rise over, in minutes; a
    /// `None` keeps its value. Blocks.
    ///
    /// # Errors
    ///
    /// A rule is refused, auto-switch is unsupported here, or the settings
    /// could not be written.
    fn set_auto_switch_rules(&self, rules: AutoSwitchRules) -> Result<bool, AccountsFailure> {
        let _ = rules;
        Err(AccountsFailure::unsupported())
    }

    /// Runs one auto-switch engine tick when auto-switch is on: the engine
    /// is made on first use, claims and heartbeats its lease, and may switch
    /// the login. While auto-switch is off, or where it cannot run, nothing
    /// runs and no lease is claimed ([`EngineTick::Off`]); turning it off
    /// halts the engine and releases its lease at the next call. Blocks.
    ///
    /// # Errors
    ///
    /// The store could not be opened, or the tick's context could not be
    /// built; no tick ran.
    fn engine_tick(&self) -> Result<EngineTick, AccountsError> {
        Ok(EngineTick::Off)
    }
}

/// What one [`AccountsService::engine_tick`] came to.
#[derive(Debug, Clone, PartialEq)]
pub enum EngineTick {
    /// Auto-switch is off or cannot run here: nothing ran and no lease was
    /// claimed. Nothing is worth calling again until the settings change.
    Off,
    /// A tick ran.
    Ran {
        /// What the tick did that a client may tell the user of, in order.
        events: Vec<AccountEventRow>,
        /// How long until the next tick.
        delay: Duration,
        /// Why the engine stands by; `None` while it decides.
        paused: Option<String>,
        /// Why the engine halted for good; `None` while it runs. A halted
        /// engine is not worth ticking again until the settings change.
        halted: Option<String>,
    },
}

/// Something that happened to a store's accounts that a client may tell the
/// user of.
#[derive(Debug, Clone, PartialEq)]
pub enum AccountEventRow {
    /// The login moved from account `from` (`None` when it was no stored
    /// account) to account `to`; `trigger` names what moved it, `manual`
    /// for a user's switch.
    Switched {
        from: Option<u32>,
        to: u32,
        trigger: String,
    },
    /// Every account is out of usage; the first reset among them, if known.
    AllExhausted {
        earliest_reset_at: Option<DateTime<Utc>>,
    },
    /// Account `number` is held out of switching.
    Quarantined { number: u32 },
    /// Account `number` is back in switching.
    Unquarantined { number: u32 },
    /// The auto-switch engine stopped for good; `message` says why.
    Halted { message: String },
}

/// Every account a store holds and the auto-switch state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AccountsSnapshot {
    pub accounts: Vec<AccountRow>,
    /// `None` where auto-switch cannot run; while it is off, a state with
    /// `enabled: false`.
    pub auto: Option<AutoState>,
}

/// How an account signs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountKindRow {
    Oauth,
    ApiKey,
}

/// One account of a store, for display. It never carries a credential.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountRow {
    /// The account's number in the store.
    pub number: u32,
    pub email: String,
    pub alias: Option<String>,
    pub org_name: Option<String>,
    pub kind: AccountKindRow,
    /// Whether this is the account in use.
    pub active: bool,
    /// Held out of automatic switching.
    pub disabled: bool,
    pub usage: UsageRow,
}

/// What the last usage read of an account found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageStatus {
    Ok,
    TokenExpired,
    ApiKey,
    ReloginRequired,
    ForeignCredential,
    NoCredentials,
    StoreUnwritable,
    Unavailable,
}

/// One usage window: how much of it is used, in percent (100 or more is
/// exhausted), and when it resets, if known.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageWindowRow {
    pub pct: f64,
    pub resets_at: Option<DateTime<Utc>>,
}

/// A per-model limit, in the shape of [`UsageWindowRow`].
#[derive(Debug, Clone, PartialEq)]
pub struct ModelLimitRow {
    pub model: String,
    pub pct: f64,
    pub resets_at: Option<DateTime<Utc>>,
}

/// An account's extra (paid) usage.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtraUsageRow {
    pub enabled: bool,
    pub user_disabled: bool,
    pub spend_limit_reached: bool,
    pub limit: Option<f64>,
}

/// An account's usage as last read.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageRow {
    pub status: UsageStatus,
    pub five_hour: Option<UsageWindowRow>,
    pub seven_day: Option<UsageWindowRow>,
    pub model_limits: Vec<ModelLimitRow>,
    pub extra_usage: Option<ExtraUsageRow>,
    /// When the usage shown was fetched.
    pub fetched_at: Option<DateTime<Utc>>,
    /// The last read's failure, such as `http-429`.
    pub error: Option<String>,
}

/// The auto-switch engine as it stands.
#[derive(Debug, Clone, PartialEq)]
pub struct AutoState {
    pub enabled: bool,
    /// The usage, in percent, at which the engine switches away.
    pub threshold_pct: f64,
    /// How far, in percentage points, usage must fall back before an account
    /// counts as recovered.
    pub hysteresis_pct: f64,
    /// The least time between two switches, in seconds.
    pub cooldown_secs: u64,
    /// How the engine picks the account to switch to.
    pub strategy: AutoSwitchStrategy,
    /// The span the fleet forecast reads usage rise over, in minutes.
    pub rate_window_minutes: Option<u32>,
    /// Why the engine is paused; `None` while it runs.
    pub paused_reason: Option<String>,
    pub warm_up_done: bool,
    /// The engine's last decision, as it names it.
    pub last_decision: Option<String>,
    /// The fleet forecast over the accounts, computed whether or not
    /// auto-switch is on; `None` while it cannot be computed.
    pub fleet: Option<FleetRow>,
}

/// A usage window the fleet forecast speaks of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FleetWindowRow {
    /// The 5-hour window.
    FiveHour,
    /// The 7-day window.
    SevenDay,
}

/// The fleet forecast over a store's accounts: whether the pooled allowance,
/// spent at the accounts' present rate, lasts the coming week, and when it
/// runs dry.
#[derive(Debug, Clone, PartialEq)]
pub struct FleetRow {
    /// The window the first gap belongs to.
    pub window: FleetWindowRow,
    /// How many accounts are in the pool.
    pub members: u32,
    /// How many accounts are on the timeline.
    pub accounts: u32,
    /// The pooled remaining allowance, in 5-hour points.
    pub remaining: f64,
    /// The summed rate per hour, in the points of `rate_window`.
    pub rate: f64,
    /// The window whose points `rate` is in.
    pub rate_window: FleetWindowRow,
    /// Whether the timeline has a gap.
    pub runs_dry: bool,
    /// When the first gap starts.
    pub blocked_at: Option<DateTime<Utc>>,
    /// When the first gap ends.
    pub resumes_at: Option<DateTime<Utc>>,
    /// The earliest reset among the accounts.
    pub first_reset: Option<DateTime<Utc>>,
}

/// The auto-switch rules a [`AccountsService::set_auto_switch_rules`] sets; a
/// `None` field keeps its value.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct AutoSwitchRules {
    /// How far, in percentage points, usage must fall back before an account
    /// counts as recovered.
    pub hysteresis_pct: Option<f64>,
    /// The least time between two switches, in seconds.
    pub cooldown_secs: Option<u64>,
    /// How the engine picks the account to switch to.
    pub strategy: Option<AutoSwitchStrategy>,
    /// The span the fleet forecast reads usage rise over, in minutes.
    pub rate_window_minutes: Option<u32>,
}
