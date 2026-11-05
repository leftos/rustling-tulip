//! The plugin's account service over the `claude-accounts` store, the three
//! blocking HTTP transports the store's polls, refreshes and profile checks go
//! through, and the process table its engine checks lease holders against.
//!
//! Every reqwest client here is blocking. The service builds one client
//! inside the first call that sends, which the daemon runs off its async
//! threads, shares it among the three transports and every later call, and
//! drops it on a thread of its own, so the service may be dropped anywhere.

use std::collections::BTreeMap;
use std::io::{self, Read};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use agent_api::{
    AccountKindRow, AccountRef, AccountRow, AccountsConfig, AccountsError, AccountsFailure,
    AccountsFailureKind, AccountsService, AccountsSnapshot, PollReport, PollScope, SwitchReport,
    UsageRow, UsageStatus, UsageWindowRow,
};
use chrono::{DateTime, Utc};
use claude_accounts::{
    AccessToken, AccountKind, AccountLock, AccountRecord, AccountsRoot, CaptureError, ClaudePaths,
    Drain, FetchScope, Identity, LockError, LockTiming, PROFILE_TIMEOUT, PROFILE_URL,
    PROFILE_USER_AGENT, ProbeVerdicts, ProfileTransport, RawProfile, RawToken, RawUsage,
    RefreshContext, RefreshRequest, RefreshTransport, Secret, Settings, Store, StoreError,
    StoreErrorKind, SwitchAccount, SwitchError, SwitchErrorKind, SwitchOutcome, TOKEN_URL,
    TransportError, USAGE_BETA_HEADER, USAGE_TIMEOUT, USAGE_URL, UsageEntry, UsageTable,
    UsageTransport, UsageWindow, capture_live_login, poll_with_refresh, probe_outgoing,
};
use directories::UserDirs;
use reqwest::blocking::{Body, Client, RequestBuilder};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue, RETRY_AFTER, USER_AGENT};
use reqwest::redirect::Policy;
use serde_json::Value;
use zeroize::Zeroizing;

use crate::transcripts::{CONFIG_DIR_VAR, own_claude_home};

/// The most of a response body a transport reads: a longer body is not read
/// on, and counts as one that does not parse.
pub const BODY_CAP: usize = 64 * 1024;

/// The variable that moves Claude Code's credentials into a store the
/// account store does not mirror.
const SECURESTORAGE_DIR_VAR: &str = "CLAUDE_SECURESTORAGE_CONFIG_DIR";

/// The least wait between two polls, in seconds: the store crate's urgent
/// cadence, the floor no planned poll is tightened below.
const POLL_FLOOR_S: f64 = claude_accounts::engine::URGENT_INTERVAL_S;

/// The account service over the store at an [`AccountsConfig`]'s root. It
/// opens the store on each call and holds no lock across calls; what it keeps
/// is the HTTP client, the stop drain, the ownership verdicts and the process
/// table, each safe to share between threads.
pub struct ClaudeAccounts {
    cfg: AccountsConfig,
    /// Built by the first call that sends, never on an async thread.
    client: OnceLock<Client>,
    drain: Arc<Drain>,
    verdicts: ProbeVerdicts,
    #[expect(dead_code, reason = "the auto-switch engine (RT-912) reads it")]
    processes: Box<dyn proc_table::ProcessTable + Send + Sync>,
}

impl ClaudeAccounts {
    /// The service over the store `cfg` names.
    #[must_use]
    pub fn new(cfg: AccountsConfig) -> Self {
        Self {
            cfg,
            client: OnceLock::new(),
            drain: Arc::new(Drain::new()),
            verdicts: ProbeVerdicts::new(),
            processes: process_table(),
        }
    }

    /// The shared client, built now when no call has built it yet. A failed
    /// build is tried again by the next call.
    fn client(&self) -> Result<&Client, AccountsError> {
        shared_client(&self.client, self.cfg.base_url.as_deref())
            .map_err(|_| AccountsError::new("the HTTP client could not be built"))
    }

    /// Claude Code's files: its home is the config's `agent_home`, else
    /// `CLAUDE_CONFIG_DIR`, else `<user home>/.claude`; its global config is
    /// `.claude.json` in the first two, else `<user home>/.claude.json`.
    fn claude_paths(&self) -> Result<ClaudePaths, AccountsError> {
        let missing = || AccountsError::new("Claude Code's home folder could not be found");
        let moved = self.cfg.agent_home.clone().or_else(|| {
            std::env::var_os(CONFIG_DIR_VAR)
                .filter(|dir| !dir.is_empty())
                .map(PathBuf::from)
        });
        if let Some(home) = moved {
            let global = home.join(".claude.json");
            return Ok(ClaudePaths::new(home, global));
        }
        let home = own_claude_home().ok_or_else(missing)?;
        let user = UserDirs::new().ok_or_else(missing)?;
        Ok(ClaudePaths::new(home, user.home_dir().join(".claude.json")))
    }

    fn open_store(&self) -> Result<Store, AccountsError> {
        self.store().map_err(|error| store_error(&error))
    }

    fn store(&self) -> Result<Store, StoreError> {
        Store::open(AccountsRoot::new(self.cfg.root.clone()))
    }

    /// Runs `run` with the context a store transaction over `store` needs
    /// (Claude Code's files, the refresh and profile transports, the drain,
    /// the ownership verdicts, Claude Code's lock timing) and the usage
    /// transport, all sending with the shared client.
    ///
    /// # Errors
    ///
    /// The client could not be built, or Claude Code's home could not be
    /// found; `run` did not run then.
    fn with_context<T>(
        &self,
        store: &Store,
        run: impl FnOnce(&RefreshContext<'_>, &UsageClient) -> T,
    ) -> Result<T, AccountsError> {
        let origin = self.cfg.base_url.as_deref();
        let client = self.client()?;
        let usage = UsageClient::with_client(client.clone(), origin);
        let refresh = RefreshClient::with_client(client.clone(), origin);
        let profile = ProfileClient::with_client(client.clone(), origin);
        let claude = self.claude_paths()?;
        let context = RefreshContext {
            store,
            claude: &claude,
            transport: &refresh,
            drain: &self.drain,
            securestorage_dir_set: std::env::var_os(SECURESTORAGE_DIR_VAR).is_some(),
            clock: &jiff::Timestamp::now,
            profile: &profile,
            verdicts: &self.verdicts,
            claude_locks: LockTiming::CLAUDE,
        };
        Ok(run(&context, &usage))
    }
}

impl Drop for ClaudeAccounts {
    /// Drops the client on a thread of its own: a blocking client may not be
    /// dropped on an async thread, and the last handle on the service may be
    /// dropped on one.
    fn drop(&mut self) {
        if let Some(client) = self.client.take()
            && let Err(error) = std::thread::Builder::new()
                .name("accounts-client-drop".to_owned())
                .spawn(move || drop(client))
        {
            tracing::warn!(
                %error,
                "the account service's HTTP client could not be dropped on a thread of its own; it drops where the service does"
            );
        }
    }
}

impl AccountsService for ClaudeAccounts {
    fn snapshot(&self) -> Result<AccountsSnapshot, AccountsError> {
        Ok(read_store(&self.open_store()?)?.snapshot)
    }

    fn poll(&self, scope: PollScope) -> Result<PollReport, AccountsError> {
        let fetch = match scope {
            PollScope::OnDemand => FetchScope::OnDemand,
            _ => return Err(AccountsError::new("this poll scope is not supported")),
        };
        let store = self.open_store()?;
        let has_accounts = store
            .read_sequence()
            .map_err(|error| store_error(&error))?
            .is_some_and(|sequence| sequence.every_record().next().is_some());
        if !has_accounts {
            return Ok(PollReport::default());
        }
        let settings = Settings::read(&store.root().settings())
            .map_err(|error| {
                AccountsError::new(format!("the account settings could not be read: {error}"))
            })?
            .autoswitch
            .to_engine();
        self.with_context(&store, |context, usage| {
            poll_with_refresh(usage, &settings, context, fetch)
        })?
        .map_err(|error| AccountsError::new(error.to_string()))?;
        let read = read_store(&store)?;
        let next_poll_in = if read.snapshot.accounts.is_empty() {
            None
        } else {
            Some(next_poll_wait(
                read.next_poll_at,
                jiff::Timestamp::now(),
                settings.interval_seconds,
            ))
        };
        Ok(PollReport {
            snapshot: read.snapshot,
            next_poll_in,
        })
    }

    fn begin_shutdown(&self, bound: Duration) -> bool {
        let _held = self.drain.start();
        self.drain.wait_idle(bound)
    }

    /// Reads the roster first, so an unknown number or the live login's own
    /// is answered without a probe; otherwise probes the outgoing login and
    /// switches with no precondition: a manual switch moves from whatever
    /// login is live.
    fn switch_to(&self, number: u32) -> Result<SwitchReport, AccountsFailure> {
        let store = self.store().map_err(|error| store_failure(&error))?;
        self.with_context(&store, |context, _| {
            if let Some(answer) = roster_answer(context, number) {
                return answer;
            }
            let probe = probe_outgoing(context);
            claude_accounts::switch(context, number, &probe, None)
                .map(switch_report)
                .map_err(switch_failure)
        })
        .map_err(|error| service_failure(&error))?
    }

    fn add_current_login(&self) -> Result<u32, AccountsFailure> {
        let store = self.store().map_err(|error| store_failure(&error))?;
        self.with_context(&store, |context, _| capture_live_login(context))
            .map_err(|error| service_failure(&error))?
            .map(|outcome| outcome.number)
            .map_err(|error| capture_failure(&error))
    }

    /// Holds a drain guard across the removal, which takes none of its own,
    /// so a shutdown waits for it.
    fn remove_account(&self, number: u32) -> Result<bool, AccountsFailure> {
        let store = self.store().map_err(|error| store_failure(&error))?;
        let claude = self
            .claude_paths()
            .map_err(|error| service_failure(&error))?;
        let Ok(_work) = self.drain.begin() else {
            return Err(draining());
        };
        store
            .remove_account(&claude, number)
            .map_err(|error| store_failure(&error))
    }

    /// Takes the account lock, then a drain guard held across the write, the
    /// order a switch takes them in.
    fn set_alias(&self, number: u32, alias: Option<String>) -> Result<bool, AccountsFailure> {
        let store = self.store().map_err(|error| store_failure(&error))?;
        let lock = AccountLock::acquire(store.root()).map_err(|error| lock_failure(&error))?;
        let Ok(_work) = self.drain.begin() else {
            return Err(draining());
        };
        store
            .set_alias(&lock, number, alias.as_deref())
            .map_err(|error| store_failure(&error))
    }

    /// As [`AccountsService::set_alias`]: the account lock, then a drain
    /// guard held across the write.
    fn set_order(&self, order: Vec<u32>) -> Result<bool, AccountsFailure> {
        let store = self.store().map_err(|error| store_failure(&error))?;
        let lock = AccountLock::acquire(store.root()).map_err(|error| lock_failure(&error))?;
        let Ok(_work) = self.drain.begin() else {
            return Err(draining());
        };
        store
            .set_order(&lock, &order)
            .map_err(|error| store_failure(&error))
    }
}

/// The refusal of a write once [`AccountsService::begin_shutdown`] has begun.
fn draining() -> AccountsFailure {
    failure(
        AccountsFailureKind::Draining,
        "draining",
        "the server is shutting down; nothing was written".to_owned(),
    )
}

/// What the roster alone answers for a switch to `number`, before any probe:
/// an unknown account for a number no switchable account has (or no roster),
/// the no-op for the account Claude Code is logged in as now. `None` when the
/// switch has to run, a roster that cannot be read included, which the
/// switch itself refuses.
fn roster_answer(
    context: &RefreshContext<'_>,
    number: u32,
) -> Option<Result<SwitchReport, AccountsFailure>> {
    let unknown = || {
        let kind = SwitchErrorKind::UnknownAccount;
        Err(failure(
            AccountsFailureKind::UnknownAccount,
            kind.as_str(),
            format!("{kind} (account {number})"),
        ))
    };
    let sequence = match context.store.read_sequence() {
        Ok(Some(sequence)) => sequence,
        Ok(None) => return Some(unknown()),
        Err(_) => return None,
    };
    let Some(target) = sequence.switchable_account(number) else {
        return Some(unknown());
    };
    if live_identity(context.claude)? != Identity::of(&target) {
        return None;
    }
    let account = AccountRef {
        number: Some(target.number),
        email: target.email.as_str().to_owned(),
    };
    Some(Ok(SwitchReport {
        switched: false,
        from: Some(account.clone()),
        to: account,
        warnings: Vec::new(),
    }))
}

/// The login `claude`'s global config names (its `oauthAccount` email and
/// organization); `None` when the file is missing, unreadable or names none.
fn live_identity(claude: &ClaudePaths) -> Option<Identity> {
    let bytes = std::fs::read(claude.global_config()).ok()?;
    let config: Value = serde_json::from_slice(&bytes).ok()?;
    let account = config.get("oauthAccount")?;
    let email = account
        .get("emailAddress")
        .and_then(Value::as_str)
        .filter(|email| !email.is_empty())?;
    let org_uuid = account
        .get("organizationUuid")
        .and_then(Value::as_str)
        .unwrap_or("");
    Some(Identity::new(email, org_uuid))
}

fn failure(kind: AccountsFailureKind, code: &str, message: String) -> AccountsFailure {
    AccountsFailure {
        kind,
        code: code.to_owned(),
        message,
    }
}

fn account_ref(account: &SwitchAccount) -> AccountRef {
    AccountRef {
        number: account.number,
        email: account.email.as_str().to_owned(),
    }
}

fn switch_report(outcome: SwitchOutcome) -> SwitchReport {
    SwitchReport {
        switched: outcome.switched,
        from: outcome.from.as_ref().map(account_ref),
        to: account_ref(&outcome.to),
        warnings: outcome.warnings,
    }
}

/// A failure of the service around a write (its HTTP client, Claude Code's
/// home), before the write ran.
fn service_failure(error: &AccountsError) -> AccountsFailure {
    failure(
        AccountsFailureKind::Failed,
        "unavailable",
        error.message.clone(),
    )
}

/// A switch's failure: the crate's kind name as the code, its text as the
/// message, which for a failed rollback also says where the live login was
/// left. A plain failure is logged whole first.
fn switch_failure(error: SwitchError) -> AccountsFailure {
    let kind = match error.kind() {
        SwitchErrorKind::LocksBusy => AccountsFailureKind::Busy,
        SwitchErrorKind::Draining => AccountsFailureKind::Draining,
        SwitchErrorKind::UnknownAccount => AccountsFailureKind::UnknownAccount,
        SwitchErrorKind::ApiKeyAccount
        | SwitchErrorKind::StoreReadOnly
        | SwitchErrorKind::UnknownSchema
        | SwitchErrorKind::StoreUnmirrored
        | SwitchErrorKind::PreconditionFailed
        | SwitchErrorKind::AutoSwitchOff => AccountsFailureKind::Refused,
        _ => AccountsFailureKind::Failed,
    };
    if kind == AccountsFailureKind::Failed {
        tracing::error!(?error, "the account switch failed");
    }
    let mut message = error.to_string();
    if error.kind() == SwitchErrorKind::RollbackFailed
        && let Some(state) = error.login_state()
    {
        message.push_str("; the live login was left at: ");
        message.push_str(state.as_str());
    }
    failure(kind, error.kind().as_str(), message)
}

/// A capture's failure: a `snake_case` name of the crate's variant as the code
/// (a store refusal's own kind name), its text as the message. A plain
/// failure is logged whole first.
fn capture_failure(error: &CaptureError) -> AccountsFailure {
    let (kind, code) = match error {
        CaptureError::NoLogin => (AccountsFailureKind::Refused, "no_login"),
        CaptureError::ApiKeyLogin => (AccountsFailureKind::Refused, "api_key_login"),
        CaptureError::NoRefreshToken => (AccountsFailureKind::Refused, "no_refresh_token"),
        CaptureError::UnsafeEmail => (AccountsFailureKind::Refused, "unsafe_email"),
        CaptureError::StoredAsApiKey => (AccountsFailureKind::Refused, "stored_as_api_key"),
        CaptureError::AnotherAccountId { .. } => {
            (AccountsFailureKind::Refused, "another_account_id")
        }
        CaptureError::AccountExists => (AccountsFailureKind::Refused, "account_exists"),
        CaptureError::NumbersExhausted => (AccountsFailureKind::Refused, "numbers_exhausted"),
        CaptureError::Draining => (AccountsFailureKind::Draining, "draining"),
        CaptureError::LocksBusy => (AccountsFailureKind::Busy, "locks_busy"),
        CaptureError::LiveUnreadable => (AccountsFailureKind::Failed, "live_unreadable"),
        CaptureError::Unverified => (AccountsFailureKind::Failed, "unverified"),
        CaptureError::LoginChanged => (AccountsFailureKind::Failed, "login_changed"),
        CaptureError::StoredUnopenable { .. } => (AccountsFailureKind::Failed, "stored_unopenable"),
        CaptureError::LockFailed => (AccountsFailureKind::Failed, "lock_failed"),
        CaptureError::Store { kind, .. } => (store_kind(*kind, false), store_code(*kind)),
        _ => (AccountsFailureKind::Failed, "capture_failed"),
    };
    if kind == AccountsFailureKind::Failed {
        tracing::error!(?error, "adding the live login failed");
    }
    failure(kind, code, error.to_string())
}

/// A store write's failure: the store kind's `snake_case` name as the code, its
/// text as the message. A lock wait that timed out, which the store reports
/// as an `accounts.json` write failure, is [`AccountsFailureKind::Busy`].
fn store_failure(error: &StoreError) -> AccountsFailure {
    let lock_timed_out = std::error::Error::source(error)
        .and_then(|source| source.downcast_ref::<io::Error>())
        .and_then(io::Error::get_ref)
        .and_then(|inner| inner.downcast_ref::<LockError>())
        .is_some_and(|lock| matches!(lock, LockError::Timeout));
    let code = if lock_timed_out {
        "locks_busy"
    } else {
        store_code(error.kind())
    };
    failure(
        store_kind(error.kind(), lock_timed_out),
        code,
        error.to_string(),
    )
}

/// The failure kind of a store error of `kind`; `lock_timed_out` when a lock
/// wait under it timed out.
fn store_kind(kind: StoreErrorKind, lock_timed_out: bool) -> AccountsFailureKind {
    if lock_timed_out {
        return AccountsFailureKind::Busy;
    }
    match kind {
        StoreErrorKind::UnknownAccount | StoreErrorKind::AccountsMissing => {
            AccountsFailureKind::UnknownAccount
        }
        StoreErrorKind::ActiveAccount
        | StoreErrorKind::StoreReadOnly
        | StoreErrorKind::UnknownSchema
        | StoreErrorKind::RootUnsafe
        | StoreErrorKind::RootNotDirectory
        | StoreErrorKind::OrderMismatch
        | StoreErrorKind::InvalidAlias
        | StoreErrorKind::AccountReidentified
        | StoreErrorKind::UnsafeEmail
        | StoreErrorKind::AccountExists
        | StoreErrorKind::NumberTaken
        | StoreErrorKind::NumbersExhausted => AccountsFailureKind::Refused,
        _ => AccountsFailureKind::Failed,
    }
}

/// A `snake_case` name of a store error's kind.
fn store_code(kind: StoreErrorKind) -> &'static str {
    match kind {
        StoreErrorKind::RootNotDirectory => "root_not_directory",
        StoreErrorKind::RootUnsafe => "root_unsafe",
        StoreErrorKind::RootUnreadable => "root_unreadable",
        StoreErrorKind::AccountsMissing => "accounts_missing",
        StoreErrorKind::AccountsUnreadable => "accounts_unreadable",
        StoreErrorKind::AccountsCorrupt => "accounts_corrupt",
        StoreErrorKind::AccountsWrite => "accounts_write",
        StoreErrorKind::UnknownSchema => "unknown_schema",
        StoreErrorKind::AccountReidentified => "account_reidentified",
        StoreErrorKind::UnsafeEmail => "unsafe_email",
        StoreErrorKind::ConfigUnreadable => "config_unreadable",
        StoreErrorKind::ConfigCorrupt => "config_corrupt",
        StoreErrorKind::ConfigWrite => "config_write",
        StoreErrorKind::ConfigWriteLeftTemp => "config_write_left_temp",
        StoreErrorKind::CredentialsUnreadable => "credentials_unreadable",
        StoreErrorKind::Foreign => "foreign",
        StoreErrorKind::CredentialsUnopenable => "credentials_unopenable",
        StoreErrorKind::CredentialsRolledBack => "credentials_rolled_back",
        StoreErrorKind::CredentialsWrite => "credentials_write",
        StoreErrorKind::StoreReadOnly => "store_read_only",
        StoreErrorKind::UnknownAccount => "unknown_account",
        StoreErrorKind::WrongLock => "wrong_lock",
        StoreErrorKind::NumbersExhausted => "numbers_exhausted",
        StoreErrorKind::NumberTaken => "number_taken",
        StoreErrorKind::AccountExists => "account_exists",
        StoreErrorKind::LeftoverUnopenable => "leftover_unopenable",
        StoreErrorKind::OrderMismatch => "order_mismatch",
        StoreErrorKind::ActiveAccount => "active_account",
        StoreErrorKind::InvalidAlias => "invalid_alias",
        _ => "store_error",
    }
}

/// The account lock's failure, taken for an alias or order write: a wait
/// that timed out is [`AccountsFailureKind::Busy`].
fn lock_failure(error: &LockError) -> AccountsFailure {
    match error {
        LockError::Timeout => failure(AccountsFailureKind::Busy, "locks_busy", error.to_string()),
        _ => failure(
            AccountsFailureKind::Failed,
            "lock_failed",
            error.to_string(),
        ),
    }
}

/// A store as read: its snapshot, and the earliest next poll its usage table
/// plans for any of its accounts.
struct StoreRead {
    snapshot: AccountsSnapshot,
    next_poll_at: Option<jiff::Timestamp>,
}

/// Every account in `store`, in its order, with the usage saved for each.
/// Sends no request: an empty store is an empty snapshot.
fn read_store(store: &Store) -> Result<StoreRead, AccountsError> {
    let Some(sequence) = store.read_sequence().map_err(|error| store_error(&error))? else {
        return Ok(StoreRead {
            snapshot: AccountsSnapshot::default(),
            next_poll_at: None,
        });
    };
    // The rotation order first, then every record it leaves out, by number.
    let order = sequence.numbers();
    let records: Vec<AccountRecord> = order
        .iter()
        .filter_map(|&number| sequence.account(number))
        .chain(
            sequence
                .every_record()
                .filter(|record| !order.contains(&record.number))
                .cloned(),
        )
        .collect();
    let identities: BTreeMap<u32, Identity> = records
        .iter()
        .map(|record| (record.number, Identity::of(record)))
        .collect();
    let mut usage = UsageTable::entries(store.root(), &identities);
    // A disabled account is never polled, so a plan it kept says nothing
    // about when the next poll is due.
    let next_poll_at = records
        .iter()
        .filter(|record| !record.disabled)
        .filter_map(|record| usage.get(&record.number)?.next_poll_at)
        .min();
    let active = sequence.active();
    let accounts = records
        .into_iter()
        .map(|record| {
            let entry = usage.remove(&record.number).unwrap_or_default();
            account_row(record, active, &entry)
        })
        .collect();
    Ok(StoreRead {
        snapshot: AccountsSnapshot {
            accounts,
            auto: None,
        },
        next_poll_at,
    })
}

/// The wait from `now` until `next_poll_at`, or `interval_s` (the settings'
/// poll interval) when no poll is planned; at least [`POLL_FLOOR_S`] either
/// way.
fn next_poll_wait(
    next_poll_at: Option<jiff::Timestamp>,
    now: jiff::Timestamp,
    interval_s: f64,
) -> Duration {
    let secs = match next_poll_at {
        Some(at) => at.duration_since(now).as_secs_f64().max(POLL_FLOOR_S),
        None => interval_s.max(POLL_FLOOR_S),
    };
    Duration::try_from_secs_f64(secs).unwrap_or(Duration::from_secs_f64(POLL_FLOOR_S))
}

/// A store error as the seam's error.
fn store_error(error: &StoreError) -> AccountsError {
    AccountsError::new(format!("the account store could not be read: {error}"))
}

/// The `lastError` names of a poll whose access token was refused and could
/// not be refreshed: a 401, and a 401 the refresh could not heal.
const TOKEN_EXPIRED_ERRORS: [&str; 2] = ["http-401", "refresh-failed"];

/// The `lastError` names of a dead refresh lineage, which only a new login
/// heals.
const RELOGIN_ERRORS: [&str; 2] = ["invalid_grant", "no_refresh_token"];

/// What the usage `entry` of an OAuth account says: the latest poll's failure
/// when it failed, else [`UsageStatus::Ok`] with a saved reading and
/// [`UsageStatus::Unavailable`] without one.
fn oauth_status(entry: &UsageEntry) -> UsageStatus {
    if entry.consecutive_failures > 0
        && let Some(error) = entry.last_error.as_deref()
    {
        return if TOKEN_EXPIRED_ERRORS.contains(&error) {
            UsageStatus::TokenExpired
        } else if RELOGIN_ERRORS.contains(&error) {
            UsageStatus::ReloginRequired
        } else {
            UsageStatus::Unavailable
        };
    }
    if entry.last_good.is_some() || entry.five_hour.is_some() || entry.seven_day.is_some() {
        UsageStatus::Ok
    } else {
        UsageStatus::Unavailable
    }
}

/// `record` as a row, with the usage `entry` saved for it. An API-key account
/// reads as [`UsageStatus::ApiKey`]; an OAuth account as [`oauth_status`].
fn account_row(record: AccountRecord, active: Option<u32>, entry: &UsageEntry) -> AccountRow {
    let kind = if record.kind == AccountKind::ApiKey {
        AccountKindRow::ApiKey
    } else {
        AccountKindRow::Oauth
    };
    let status = if kind == AccountKindRow::ApiKey {
        UsageStatus::ApiKey
    } else {
        oauth_status(entry)
    };
    AccountRow {
        number: record.number,
        email: record.email.as_str().to_owned(),
        alias: record.alias,
        org_name: Some(record.org_name).filter(|name| !name.is_empty()),
        kind,
        active: active == Some(record.number),
        disabled: record.disabled,
        usage: UsageRow {
            status,
            five_hour: entry.five_hour.as_ref().map(window_row),
            seven_day: entry.seven_day.as_ref().map(window_row),
            model_limits: Vec::new(),
            extra_usage: None,
            fetched_at: entry
                .fetched_at
                .and_then(|at| chrono_time(at.as_nanosecond())),
            error: entry.last_error.clone(),
        },
    }
}

fn window_row(window: &UsageWindow) -> UsageWindowRow {
    UsageWindowRow {
        pct: window.pct,
        resets_at: window
            .resets_at
            .and_then(|at| chrono_time(at.as_nanosecond())),
    }
}

/// The time `nanos` nanoseconds past the epoch (before it when negative),
/// `None` when chrono cannot hold it.
fn chrono_time(nanos: i128) -> Option<DateTime<Utc>> {
    // Euclidean division keeps the sub-second part in 0..1e9 before the epoch.
    let second = i64::try_from(nanos.div_euclid(1_000_000_000)).ok()?;
    let subsec = u32::try_from(nanos.rem_euclid(1_000_000_000)).ok()?;
    DateTime::from_timestamp(second, subsec)
}

/// The process table the account engine checks a lease holder against: the
/// live Windows table where the store's locks work, an empty one elsewhere,
/// where a crashed holder's lease waits out its heartbeat.
#[must_use]
pub fn process_table() -> Box<dyn proc_table::ProcessTable + Send + Sync> {
    #[cfg(all(windows, target_pointer_width = "64"))]
    {
        Box::new(proc_table::Win32ProcessTable)
    }
    #[cfg(not(all(windows, target_pointer_width = "64")))]
    {
        Box::new(proc_table::EmptyTable)
    }
}

/// `real` (an `https://host/path` endpoint) at `origin` in place of its own
/// scheme and host, or `real` itself without one.
fn endpoint(real: &str, origin: Option<&str>) -> String {
    let Some(origin) = origin else {
        return real.to_owned();
    };
    let path = real
        .find("://")
        .and_then(|at| {
            let rest = &real[at + 3..];
            rest.find('/').map(|slash| &rest[slash..])
        })
        .unwrap_or("/");
    format!("{}{path}", origin.trim_end_matches('/'))
}

/// A client that follows no redirect, uses no proxy, and speaks only HTTPS
/// unless an origin override is set. Each request sets its own timeout.
fn client(origin: Option<&str>) -> Result<Client, TransportError> {
    Client::builder()
        .redirect(Policy::none())
        .no_proxy()
        .https_only(origin.is_none())
        .build()
        .map_err(|_| TransportError::Connect)
}

/// The client in `slot`, built for `origin` now when the slot is empty. A
/// failed build leaves the slot empty, so the next call tries again.
fn shared_client<'a>(
    slot: &'a OnceLock<Client>,
    origin: Option<&str>,
) -> Result<&'a Client, TransportError> {
    if let Some(client) = slot.get() {
        return Ok(client);
    }
    let built = client(origin)?;
    // A racing call may have filled the slot first: its client wins and
    // `built` drops here, on this (blocking) thread.
    Ok(slot.get_or_init(|| built))
}

/// A transport's way out: the origin it sends to in place of the real
/// endpoint's, and the client it sends with, shared or built on first use.
struct Http {
    origin: Option<String>,
    client: OnceLock<Client>,
}

impl Http {
    fn new(origin: Option<&str>) -> Self {
        Self {
            origin: origin.map(str::to_owned),
            client: OnceLock::new(),
        }
    }

    fn with_client(client: Client, origin: Option<&str>) -> Self {
        Self {
            origin: origin.map(str::to_owned),
            client: OnceLock::from(client),
        }
    }

    fn origin(&self) -> Option<&str> {
        self.origin.as_deref()
    }

    fn client(&self) -> Result<&Client, TransportError> {
        shared_client(&self.client, self.origin())
    }
}

/// `Bearer <token>` as a header value marked sensitive, so reqwest never
/// shows it. A token that cannot be a header value is refused unsent.
fn bearer(token: &AccessToken) -> Result<HeaderValue, TransportError> {
    let text = Secret::new(format!("Bearer {}", token.expose()));
    let mut value =
        HeaderValue::from_str(text.expose()).map_err(|_| TransportError::Unsupported)?;
    value.set_sensitive(true);
    Ok(value)
}

/// One answer as read: its status, its `Retry-After` in seconds, and its
/// body parsed (`Null` for an empty, oversized or unparseable body other than
/// a 2xx one).
struct Answer {
    status: u16,
    retry_after: Option<f64>,
    body: Value,
}

/// Sends `request` and reads its answer, whatever its status, reading at
/// most [`BODY_CAP`] of the body.
fn exchange(request: RequestBuilder) -> Result<Answer, TransportError> {
    let response = request.send().map_err(|error| request_error(&error))?;
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|text| retry_after_secs(text, jiff::Timestamp::now()));
    let mut bytes = Zeroizing::new(Vec::with_capacity(BODY_CAP + 1));
    response
        .take(u64::try_from(BODY_CAP + 1).unwrap_or(u64::MAX))
        .read_to_end(&mut bytes)
        .map_err(|error| read_error(&error))?;
    let parsed = if bytes.len() > BODY_CAP {
        None
    } else {
        serde_json::from_slice::<Value>(&bytes).ok()
    };
    let body = match parsed {
        Some(body) => body,
        None if (200..300).contains(&status) => return Err(TransportError::MalformedBody),
        None => Value::Null,
    };
    Ok(Answer {
        status,
        retry_after,
        body,
    })
}

/// A `Retry-After` value as a wait in seconds from `now`: a non-negative
/// finite number of seconds, or an HTTP-date (`Sun, 06 Nov 1994 08:49:37
/// GMT`), a past one waiting 0. Anything else is `None`.
fn retry_after_secs(text: &str, now: jiff::Timestamp) -> Option<f64> {
    let text = text.trim();
    if let Ok(secs) = text.parse::<f64>() {
        return (secs.is_finite() && secs >= 0.0).then_some(secs);
    }
    let at = jiff::fmt::rfc2822::DateTimeParser::new()
        .parse_timestamp(text)
        .ok()?;
    Some(at.duration_since(now).as_secs_f64().max(0.0))
}

/// A failed send as a transport error.
fn request_error(error: &reqwest::Error) -> TransportError {
    if error.is_timeout() {
        TransportError::Timeout
    } else {
        TransportError::Connect
    }
}

/// A failed body read as a transport error.
fn read_error(error: &io::Error) -> TransportError {
    let timed_out = error.kind() == io::ErrorKind::TimedOut
        || error
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<reqwest::Error>())
            .is_some_and(reqwest::Error::is_timeout);
    if timed_out {
        TransportError::Timeout
    } else {
        TransportError::Connect
    }
}

/// The usage endpoint over a blocking reqwest client. Drop it off the async
/// threads once it has sent.
pub struct UsageClient {
    http: Http,
}

impl UsageClient {
    /// The client for the real endpoint, or for `origin` in its place,
    /// building its HTTP client on its first request.
    #[must_use]
    pub fn new(origin: Option<&str>) -> Self {
        Self {
            http: Http::new(origin),
        }
    }

    /// As [`UsageClient::new`], sending with `client` (a shared handle).
    #[must_use]
    pub fn with_client(client: Client, origin: Option<&str>) -> Self {
        Self {
            http: Http::with_client(client, origin),
        }
    }
}

impl UsageTransport for UsageClient {
    fn get_usage(&self, token: &AccessToken) -> Result<RawUsage, TransportError> {
        let origin = self.http.origin();
        let (beta_name, beta_value) = USAGE_BETA_HEADER;
        let answer = exchange(
            self.http
                .client()?
                .get(endpoint(USAGE_URL, origin))
                .timeout(USAGE_TIMEOUT)
                .header(AUTHORIZATION, bearer(token)?)
                .header(beta_name, beta_value),
        )?;
        Ok(RawUsage {
            status: answer.status,
            body: answer.body,
            retry_after: answer.retry_after,
        })
    }
}

/// The token endpoint over a blocking reqwest client. Drop it off the async
/// threads once it has sent.
pub struct RefreshClient {
    http: Http,
}

impl RefreshClient {
    /// The client for the real endpoint, or for `origin` in its place,
    /// building its HTTP client on its first request.
    #[must_use]
    pub fn new(origin: Option<&str>) -> Self {
        Self {
            http: Http::new(origin),
        }
    }

    /// As [`RefreshClient::new`], sending with `client` (a shared handle).
    #[must_use]
    pub fn with_client(client: Client, origin: Option<&str>) -> Self {
        Self {
            http: Http::with_client(client, origin),
        }
    }
}

/// A request body read from secret text, which is zeroed when the client
/// drops the body.
struct SecretBody {
    text: Secret,
    at: usize,
}

impl Read for SecretBody {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let rest = self.text.expose().as_bytes().get(self.at..).unwrap_or(&[]);
        let count = rest.len().min(buf.len());
        buf[..count].copy_from_slice(&rest[..count]);
        self.at += count;
        Ok(count)
    }
}

impl RefreshTransport for RefreshClient {
    fn post_token(&self, request: &RefreshRequest) -> Result<RawToken, TransportError> {
        let origin = self.http.origin();
        let client = self.http.client()?;
        let text = request.json_body();
        let length = u64::try_from(text.expose().len()).unwrap_or(u64::MAX);
        let body = Body::sized(SecretBody { text, at: 0 }, length);
        let answer = exchange(
            client
                .post(endpoint(TOKEN_URL, origin))
                .timeout(request.timeout())
                .header(CONTENT_TYPE, "application/json")
                .body(body),
        )?;
        Ok(RawToken {
            status: answer.status,
            body: answer.body,
        })
    }
}

/// The profile endpoint over a blocking reqwest client. Drop it off the async
/// threads once it has sent.
pub struct ProfileClient {
    http: Http,
}

impl ProfileClient {
    /// The client for the real endpoint, or for `origin` in its place,
    /// building its HTTP client on its first request.
    #[must_use]
    pub fn new(origin: Option<&str>) -> Self {
        Self {
            http: Http::new(origin),
        }
    }

    /// As [`ProfileClient::new`], sending with `client` (a shared handle).
    #[must_use]
    pub fn with_client(client: Client, origin: Option<&str>) -> Self {
        Self {
            http: Http::with_client(client, origin),
        }
    }
}

impl ProfileTransport for ProfileClient {
    fn get_profile(&self, access: &AccessToken) -> Result<RawProfile, TransportError> {
        let origin = self.http.origin();
        let answer = exchange(
            self.http
                .client()?
                .get(endpoint(PROFILE_URL, origin))
                .timeout(PROFILE_TIMEOUT)
                .header(AUTHORIZATION, bearer(access)?)
                .header(CONTENT_TYPE, "application/json")
                .header(USER_AGENT, PROFILE_USER_AGENT),
        )?;
        Ok(RawProfile {
            status: answer.status,
            body: answer.body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chrono_time_converts_a_pre_epoch_time() {
        let before = chrono_time(-1_500_000_000).map(|at| at.to_rfc3339());
        let after = chrono_time(1_500_000_000).map(|at| at.to_rfc3339());

        assert_eq!(before.as_deref(), Some("1969-12-31T23:59:58.500+00:00"));
        assert_eq!(after.as_deref(), Some("1970-01-01T00:00:01.500+00:00"));
    }

    /// 1994-11-06 08:49:37 UTC, the HTTP-date example's time.
    fn example_now() -> jiff::Timestamp {
        jiff::Timestamp::UNIX_EPOCH + jiff::SignedDuration::from_secs(784_111_777)
    }

    #[test]
    fn retry_after_reads_seconds() {
        assert_eq!(retry_after_secs("30", example_now()), Some(30.0));
        assert_eq!(retry_after_secs(" 1.5 ", example_now()), Some(1.5));
        assert_eq!(retry_after_secs("0", example_now()), Some(0.0));
    }

    #[test]
    fn retry_after_reads_an_http_date() {
        let secs = retry_after_secs("Sun, 06 Nov 1994 08:51:37 GMT", example_now());

        assert_eq!(secs, Some(120.0));
    }

    #[test]
    fn retry_after_past_date_is_zero() {
        let secs = retry_after_secs("Sun, 06 Nov 1994 08:49:00 GMT", example_now());

        assert_eq!(secs, Some(0.0));
    }

    #[test]
    fn retry_after_rejects_garbage_and_non_finite() {
        for text in ["NaN", "inf", "-5", "soon", ""] {
            assert_eq!(retry_after_secs(text, example_now()), None, "{text:?}");
        }
    }
}
