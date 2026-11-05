//! The accounts seam: the saved agent logins a host switches between, and
//! their usage as last read.
//!
//! A plugin that keeps accounts hands the daemon an [`AccountsService`] for
//! the store [`AccountsConfig`] names. Every method blocks (it reads files,
//! and later ones send HTTP requests), so the daemon calls each one off its
//! async threads. What crosses the seam is plain data: the rows below, which
//! the daemon maps onto its wire types, never a plugin's own types.

use chrono::{DateTime, Utc};
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

/// A plugin's accounts, as the daemon holds them: shared, and called from
/// several threads at once, so it holds no lock across calls. Every method
/// blocks.
pub trait AccountsService: Send + Sync {
    /// Every account in the store, in the store's order, with the usage it
    /// last saved for each, and the auto-switch state when an engine runs.
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
}

/// Every account a store holds and the auto-switch state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AccountsSnapshot {
    pub accounts: Vec<AccountRow>,
    /// `None` while no auto-switch engine runs.
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
    /// Why the engine is paused; `None` while it runs.
    pub paused_reason: Option<String>,
    pub warm_up_done: bool,
    /// The engine's last decision, as it names it.
    pub last_decision: Option<String>,
}
