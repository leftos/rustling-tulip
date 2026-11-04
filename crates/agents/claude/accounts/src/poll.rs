//! Polling the usage endpoint within the request budget: reserve, fetch, record.
//!
//! The endpoint allows about 28-30 requests per account per trailing hour, and every poll on the store spends one budget
//! through `cache/usage.json`. A poll first reserves the accounts it may fetch under `cache/.usage.lock` (not fetched
//! in the last 180 s, past their planned next poll, out of failure backoff, and not claimed by another process),
//! stamping a 90 s lease on each; it then fetches with no lock held, and records the outcomes under the lock again,
//! fenced by the lease ids, so a result fetched under a lease another process has since replaced is dropped. A success
//! writes the reading and the next poll time; a failure writes the backoff.
//! These cover the account-wide 5-hour and 7-day windows.

mod active;

use std::cell::RefCell;
use std::collections::hash_map::RandomState;
use std::collections::{BTreeMap, BTreeSet};
use std::hash::{BuildHasher, Hasher};
use std::io;
use std::time::{Duration, SystemTime};

use serde_json::{Map, Value};

use crate::engine::relevant_windows;
use crate::locks::{LockError, acquire_in_root};
use crate::paths::{AccountsRoot, ClaudePaths};
use crate::refresh::oracle::LineageKey;
use crate::refresh::{
    GrantError, GrantOutcome, RefreshContext, consume_backup_grant, credential_fingerprint,
    has_refresh_token,
};
use crate::secret::{AccessToken, CredentialBlob, OauthCredentials};
use crate::store::{
    self, AccountRecord, AutoSwitchSettings, Sequence, Store, StoreError, StoreErrorKind,
};
use crate::transport::{RawUsage, TransportError, UsageTransport, is_org_uuid};
use crate::usage::{
    CacheDocument, Identity, MAX_MINOR_EXPONENT, PrepaidEntry, USAGE_SCHEMA_VERSION, UsageTable,
    UsageWindow, account_windows, failure_count, is_currency_code, live_claim, read_document,
    secs_from_timestamp, timestamp_from_secs,
};

/// An account fetched more recently than this is not fetched again: at most one request per account per 180 s.
pub const SERVE_TTL_S: f64 = 180.0;
/// How long a fetch lease keeps other collectors off an account.
pub const CLAIM_TTL_S: f64 = 90.0;
/// How long `cache/.usage.lock` is waited for: the file-lock timeout.
const USAGE_LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// Failure backoff with no `Retry-After`: 30 s, doubling per failure, capped at 600 s.
const BACKOFF_BASE_S: f64 = 30.0;
const BACKOFF_CAP_S: f64 = 600.0;
const BACKOFF_MAX_SHIFT: u32 = 32;
/// Added to a 429's `Retry-After` above [`BACKOFF_CAP_S`]: a retry landing on the server's deadline often re-blocks.
const RETRY_AFTER_MARGIN_S: f64 = 900.0;
/// The longest wait a 429 may ask for, margin included.
const RETRY_AFTER_FLOOR_CAP_S: f64 = 4500.0;
/// The longest wait any other failure may ask for.
const TRUST_MAX_AGE_S: f64 = 3600.0;
/// `Retry-After: 0` on a 429 is the saturated-budget edge: wait at least this long before probing again.
const EDGE_BACKOFF_S: f64 = 300.0;

/// Poll cadence: the floor, the urgent cadence, and the decay ceilings for the active account and the others.
pub(crate) const MIN_INTERVAL_S: f64 = 180.0;
/// The active account's urgent cadence, and the floor no planned poll is tightened below.
pub const URGENT_INTERVAL_S: f64 = 60.0;
pub(crate) const ACTIVE_MAX_INTERVAL_S: f64 = 300.0;
const CANDIDATE_DEFAULT_INTERVAL_S: f64 = 300.0;
const CANDIDATE_MAX_INTERVAL_S: f64 = 600.0;
/// An exhausted account is still polled, this often.
pub(crate) const EXHAUSTED_INTERVAL_S: f64 = 600.0;
/// A binding window that moved at least this many points since the last poll is being consumed.
const MOVEMENT_DELTA_PCT: f64 = 1.0;
/// Each planned interval is moved by up to this fraction either way, so collectors drift apart.
const JITTER_FRAC: f64 = 0.1;
/// After a 429 the cadence is floored here, and grows by [`POST_429_BACKOFF_MULT`] up to [`POST_429_MAX_INTERVAL_S`],
/// for [`RECENT_429_WINDOW_S`] after the 429's backoff lifts.
const POST_429_MIN_INTERVAL_S: f64 = 360.0;
const RECENT_429_WINDOW_S: f64 = 3600.0;
const POST_429_BACKOFF_MULT: f64 = 1.5;
const POST_429_MAX_INTERVAL_S: f64 = 1800.0;
/// The active account polls at the urgent cadence once its binding window is within this many points of the threshold.
pub(crate) const ESCALATION_MARGIN_PCT: f64 = 15.0;
/// No poll is planned later than a window's reset plus this.
const RESET_SLACK_S: f64 = 60.0;
/// A prepaid balance read is due once the last one, success or failure, is at least this old: below the 600 s usage
/// cadence minus its jitter, so every poll at that cadence reads it. The prepaid endpoint's rate limit is unmeasured.
const PREPAID_DUE_S: f64 = 480.0;
/// A pass reads a due prepaid balance only while it has used less than this much of its lease: a balance is optional
/// where the usage readings are not, and one read late in a pass can let the lease lapse before the row is written.
pub(crate) const PREPAID_CUTOFF_S: f64 = CLAIM_TTL_S / 2.0;
/// The minor-unit scale of an amount whose answer names none: cents.
const DEFAULT_MINOR_EXPONENT: u32 = 2;

/// Why a poll, a reserve or a record stopped.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PollError {
    /// `cache/.usage.lock` was not taken.
    #[error("the usage table's lock was not taken")]
    Lock(#[source] LockError),
    /// `cache/usage.json` is a JSON object whose `schemaVersion` is not the integer [`USAGE_SCHEMA_VERSION`]: a newer or
    /// unknown version of this store wrote it, so it is read as empty and never written. A file that is
    /// not a JSON object at all is corrupt instead, and is read as empty and rewritten.
    #[error(
        "cache/usage.json was written by a newer or unknown version of the account store, so it is left untouched"
    )]
    UnsupportedCache,
    /// `cache/usage.json` exists but could not be read.
    #[error("cache/usage.json could not be read")]
    CacheUnreadable(#[source] io::Error),
    /// `cache/usage.json` could not be written.
    #[error("cache/usage.json could not be written")]
    CacheWrite(#[source] io::Error),
    /// `accounts.json` refuses writes to the store (a newer or unknown `schemaVersion`, or a file that cannot be read
    /// or parsed), so `cache/usage.json` was not written. Reads of the table are never refused.
    #[error("the account store refuses writes ({0:?}), so cache/usage.json was left untouched")]
    StoreRefused(StoreErrorKind),
    /// The account store could not be read.
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Why a fetch failed. [`FetchError::kind`] is the token the usage table records as `lastError`.
#[derive(Clone, Copy, Debug, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum FetchError {
    /// The endpoint answered with this status, and this `Retry-After` in seconds if it sent one.
    #[error("the usage endpoint answered HTTP {status}")]
    Http {
        status: u16,
        retry_after: Option<f64>,
    },
    /// The endpoint did not answer in time.
    #[error("the usage endpoint did not answer in time")]
    Timeout,
    /// The endpoint could not be reached.
    #[error("the usage endpoint could not be reached")]
    Network,
    /// The answer was not a usage reading.
    #[error("the usage endpoint's answer was not a usage reading")]
    BadResponse,
}

impl FetchError {
    /// The failure token: `http-<status>`, `timeout`, `network` or `bad-response`.
    #[must_use]
    pub fn kind(&self) -> String {
        match self {
            Self::Http { status, .. } => format!("http-{status}"),
            Self::Timeout => "timeout".to_owned(),
            Self::Network => "network".to_owned(),
            Self::BadResponse => "bad-response".to_owned(),
        }
    }

    fn is_rate_limit(&self) -> bool {
        matches!(self, Self::Http { status: 429, .. })
    }

    fn retry_after(&self) -> Option<f64> {
        match self {
            Self::Http { retry_after, .. } => *retry_after,
            Self::Timeout | Self::Network | Self::BadResponse => None,
        }
    }
}

/// One fetch's result: the reading the usage table stores as `lastGood` (`None` when the answer held no window), or why it failed.
pub type FetchResult = Result<Option<Value>, FetchError>;

/// The leases one reserve won: account number to the account and the lease's fencing id.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Claims {
    held: BTreeMap<u32, (Identity, String)>,
    /// The accounts left unclaimed because a strike quarantines their stored generation, when the reserve was told the
    /// stored fingerprints.
    dead: BTreeSet<u32>,
    /// The claimed accounts whose prepaid balance is due to be read beside their usage ([`PREPAID_DUE_S`]).
    prepaid_due: BTreeSet<u32>,
}

impl Claims {
    /// Whether no account was claimed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }

    /// Whether account `number` was claimed.
    #[must_use]
    pub fn contains(&self, number: u32) -> bool {
        self.held.contains_key(&number)
    }

    /// The claimed account numbers, in order.
    pub fn numbers(&self) -> impl Iterator<Item = u32> + '_ {
        self.held.keys().copied()
    }

    /// Account `number`'s fencing id, if it was claimed.
    #[must_use]
    pub fn claim_id(&self, number: u32) -> Option<&str> {
        self.held.get(&number).map(|(_, id)| id.as_str())
    }
}

/// Which accounts a poll may fetch, and past which gates.
/// Every account is reported whatever the scope; an account outside it is never claimed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum FetchScope<'a> {
    /// Every account, respecting poll plans and repairing plans no planner could have written.
    OnDemand,
    /// Only `accounts` may be fetched, under `mode`; every other account is reported but never reserved.
    Only {
        accounts: &'a BTreeSet<u32>,
        mode: FetchMode,
    },
}

impl FetchScope<'_> {
    /// Whether account `number` may be claimed in this scope.
    fn allows(&self, number: u32) -> bool {
        match self {
            Self::OnDemand => true,
            Self::Only { accounts, .. } => accounts.contains(&number),
        }
    }

    /// The gates an explicit set passes, `None` on demand.
    fn mode(&self) -> Option<FetchMode> {
        match self {
            Self::OnDemand => None,
            Self::Only { mode, .. } => Some(*mode),
        }
    }
}

/// The gates an explicit fetch set passes for the auto engine. Strikes, failure backoff, holds
/// (`heldUntil`) and another collector's live lease refuse in every mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum FetchMode {
    /// `respect_plans=False`: poll-due or stale (an escalation refresh may fetch a not-yet-due account).
    Escalation,
    /// `respect_plans=False, repair_overslept=True`: due plans and stale impossible plans win, valid future plans do not.
    Scheduled,
    /// `force=True`: freshness and poll plans ignored; strikes, backoff, holds and live claims still refuse.
    Force,
}

/// What a record needs to plan each account's next poll: which account is active, the switch threshold, and the
/// configured models whose scoped windows count.
#[derive(Clone, Debug, PartialEq)]
pub struct PlanContext {
    /// The active account's number, if one is active.
    pub active: Option<u32>,
    /// The auto-switch threshold in percent.
    pub threshold: f64,
    /// The settings' `models`: the scoped windows they name (every one under `all`) count toward the binding window and
    /// the next reset.
    pub models: Vec<String>,
}

impl PlanContext {
    /// A context with `active` as the active account, `threshold` as the switch threshold, and `models` as the
    /// configured model names.
    #[must_use]
    pub fn new(active: Option<u32>, threshold: f64, models: Vec<String>) -> Self {
        Self {
            active,
            threshold,
            models,
        }
    }
}

/// The inputs of [`plan_after_fetch`] for one account just fetched successfully.
#[derive(Clone, Copy, Debug)]
pub struct PlanRequest<'a> {
    /// The interval the last plan chose, in seconds.
    pub prev_interval_s: Option<f64>,
    /// The reading before this fetch.
    pub prev_usage: Option<&'a Value>,
    /// The reading this fetch returned.
    pub new_usage: Option<&'a Value>,
    /// Whether the account is the active one.
    pub is_active: bool,
    /// The auto-switch threshold in percent.
    pub threshold: f64,
    /// The settings' `models`: the scoped windows they name (every one under `all`, none when empty) join the
    /// 5-hour and 7-day windows in the binding window and the earliest reset.
    pub models: &'a [String],
    /// Whether the account answered 429 recently (within an hour of that backoff lifting).
    pub recent_429: bool,
    pub now: jiff::Timestamp,
    /// A number in `0..=1` choosing where in the ±10 % jitter the next poll lands.
    pub jitter: f64,
}

/// When to poll an account next, and the interval that time came from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PollPlan {
    pub next_poll_at: jiff::Timestamp,
    pub interval_s: f64,
}

/// One account's part in a poll.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct PollOutcome {
    pub number: u32,
    pub result: PollResult,
    /// When the poll made a credential live for the account Claude Code is logged in as: whether `~/.claude.json`
    /// is left without a `primaryApiKey` (`Some(false)` when clearing it failed, so a managed API key may still be
    /// billed in place of the account). `None` when nothing was made live.
    pub primary_key_cleared: Option<bool>,
    /// When the poll wrote the backup of the account Claude Code is logged in as (a refresh's successor, a rotation
    /// Claude Code made, a stashed successor adopted, or a resynced lineage): whether the write succeeded (`Some(false)`
    /// when it failed, so the backup still holds an older generation). `None` when no backup write was attempted.
    pub backup_written: Option<bool>,
    /// When the poll's write of Claude Code's live credential file failed ([`PollResult::LiveWriteFailed`]): whether
    /// its temp file, which holds the new credential's tokens, could not be removed and is still beside the live file
    /// (`Some(true)`), or was removed (`Some(false)`). `None` when no live write failed, or when none was attempted
    /// because the credential held no `claudeAiOauth` to put in a live file that holds other keys.
    pub live_temp_left: Option<bool>,
}

/// Why a refresh was put off to the next pass, before any grant was spent: the refresh of the account Claude Code is
/// logged in as, for every reason, or an inactive account's at the consume gate, for [`DeferReason::StoreUnwritable`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DeferReason {
    /// The store's consume or account lock, or one of Claude Code's credential locks, stayed held past its wait.
    LocksBusy,
    /// `~/.claude.json` names another account under the locks: a switch or a login landed meanwhile.
    IdentityMismatch,
    /// The live credential cannot be attributed to the account: neither its lineage nor a backup holding a refresh
    /// token, or a newer live generation no verdict owns.
    Unattributable,
    /// The live credential moved to another lineage since the poll read it.
    LiveMoved,
    /// The live credential file could not be read under the locks.
    LiveUnreadable,
    /// The account's backup could not be read, before the locks or under them: nothing can be attributed against it.
    BackupUnreadable,
    /// The unclaimed stash could not be read, or held an entry that does not match its row's fingerprint, while it may
    /// hold the successor of the live generation: spending that generation again could kill the lineage.
    StashUnreadable,
    /// The live credential is not the account's lineage and could not be parked in the unclaimed stash before being
    /// overwritten: its bytes may be another login's only copy, so nothing is spent or written.
    StashUnwritable,
    /// The backup store refuses writes (sealing does not work in this process, or `accounts.json` is a later version's),
    /// so a successor could not be kept there, or it refused a stashed successor of the live generation being adopted:
    /// nothing is spent.
    StoreUnwritable,
}

/// What a poll did for one account.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum PollResult {
    /// The account is disabled: not polled.
    Disabled,
    /// No stored credentials, or none with an access token.
    NoCredentials,
    /// The stored credentials could not be read.
    CredentialsUnreadable(StoreErrorKind),
    /// The access token has expired: not sent.
    TokenExpired,
    /// Not due: fetched in the last 180 s, before its planned poll, in backoff, held, claimed by another process, or
    /// outside the poll's [`FetchScope`].
    NotDue,
    /// Fetched and recorded.
    Fetched {
        five_hour: Option<UsageWindow>,
        seven_day: Option<UsageWindow>,
    },
    /// The fetch failed; the failure and its backoff were recorded.
    Failed(FetchError),
    /// Fetched, but another process replaced the lease meanwhile, so the result was dropped.
    Fenced,
    /// The refresh was refused because `CLAUDE_SECURESTORAGE_CONFIG_DIR` is set; recorded as `store-unmirrored`.
    StoreUnmirrored,
    /// Another refresh of the account held its consume lock; recorded as `consume-busy`.
    ConsumeBusy,
    /// A stashed successor or the stash manifest could not be read, or a stashed entry does not match its row's
    /// fingerprint, so the refresh was deferred; recorded as `stash-unreadable`.
    StashUnreadable,
    /// The token endpoint refused the client id; recorded as `invalid_client`, striking nothing.
    InvalidClient,
    /// The stored generation's refresh lineage is dead (`invalid_grant` or no refresh token): struck, or quarantined
    /// by an earlier strike on the same generation. Log in to the account again.
    AuthDead,
    /// The stop drain has started: no refresh was begun, and nothing was recorded.
    Draining,
    /// A refresh was put off to the next pass before anything was spent: the logged-in account's under Claude Code's
    /// locks, or an inactive account's when the store refuses writes; nothing was recorded.
    Deferred(DeferReason),
    /// The live credential belongs to another account, as the profile oracle answered: its usage is not this
    /// account's, so nothing was recorded.
    ForeignCredential,
    /// The successor of the logged-in account's spent grant, or its backup being restored, could not be written to the
    /// live file (the write failed, or the credential holds no `claudeAiOauth` while the live file holds other keys
    /// that writing it whole would drop): the live file
    /// still holds the old token, and a spent grant's successor was kept in the backup or the unclaimed stash when
    /// either could be written. Nothing was recorded.
    LiveWriteFailed,
}

impl UsageTable {
    /// Claims each account of `identities` that may be fetched at `now`, under `cache/.usage.lock`: a row naming
    /// another account is replaced by a fresh one and claimed; a matching row is claimed when it is not quarantined,
    /// out of backoff, not held, not claimed by a live lease, more than 180 s old, and past its planned poll (or has no
    /// plan, or a plan no planner could have written). Each claim stamps a 90 s lease.
    ///
    /// # Errors
    ///
    /// [`PollError::Lock`], [`PollError::UnsupportedCache`], [`PollError::CacheUnreadable`], [`PollError::CacheWrite`],
    /// [`PollError::StoreRefused`].
    pub fn reserve(
        root: &AccountsRoot,
        identities: &BTreeMap<u32, Identity>,
        now: jiff::Timestamp,
    ) -> Result<Claims, PollError> {
        let pass = Reserve {
            scope: FetchScope::OnDemand,
            fingerprints: None,
            now: secs_from_timestamp(now),
        };
        Self::reserve_with(root, identities, pass)
    }

    /// [`UsageTable::reserve`] within the pass's scope, with each account's stored credential fingerprints when the pass
    /// has them (the backup's, or for the account Claude Code is logged in as its live credential's and its backup's): a
    /// strike then quarantines a row only while its `struckFingerprint` names a stored generation (or is absent, or the
    /// logged-in account's backup cannot be read), and each quarantined account
    /// is noted in the claims, inside the scope or not. An account outside the scope is never claimed and its row never
    /// replaced. The lock is taken even for an [`FetchScope::Only`] with no accounts, since the strikes behind
    /// `claims.dead` are read under it.
    fn reserve_with(
        root: &AccountsRoot,
        identities: &BTreeMap<u32, Identity>,
        pass: Reserve<'_>,
    ) -> Result<Claims, PollError> {
        if identities.is_empty() {
            return Ok(Claims::default());
        }
        root.check_folders(&root.usage_lock())
            .map_err(PollError::CacheUnreadable)?;
        let _lock = acquire_in_root(root, &root.usage_lock(), USAGE_LOCK_TIMEOUT)
            .map_err(PollError::Lock)?;
        let mut document = load_document(root)?;
        let mut accounts = take_accounts(&mut document);
        let claims = reserve_rows(&mut accounts, identities, pass);
        if !claims.is_empty() {
            put_accounts(&mut document, accounts);
            write_document(root, &document)?;
        }
        Ok(claims)
    }

    /// Records each outcome under `cache/.usage.lock`, fenced by `claims`: an outcome whose row no longer carries its
    /// lease id, or names another account, is dropped. A success writes the reading, `fetchedAt` and the next poll
    /// ([`plan_after_fetch`]), and clears the failure state; a failure keeps the reading and writes the failure count,
    /// `lastError`, `backoffUntil` ([`backoff_seconds`]) and, for a 429, `last429At`. Both release the lease. Every time
    /// written is one reading of `clock`, taken once the lock is held: a reserve's
    /// time would date them up to the fetches' length early, and a 429's backoff would lift before the server's
    /// deadline. Returns the accounts whose outcome was written.
    ///
    /// # Errors
    ///
    /// [`PollError::Lock`], [`PollError::UnsupportedCache`], [`PollError::CacheUnreadable`], [`PollError::CacheWrite`],
    /// [`PollError::StoreRefused`].
    pub fn record(
        root: &AccountsRoot,
        outcomes: &BTreeMap<u32, FetchResult>,
        claims: &Claims,
        context: &PlanContext,
        clock: &dyn Fn() -> jiff::Timestamp,
    ) -> Result<BTreeSet<u32>, PollError> {
        let recorded = outcomes
            .iter()
            .map(|(number, result)| (*number, Recorded::Fetch(result)))
            .collect();
        Self::record_rows(root, &recorded, &BTreeMap::new(), claims, context, clock)
    }

    /// [`UsageTable::record`] for every kind of outcome a poll records: a fetch, a refused or dead refresh, or a lease
    /// released with nothing recorded; and beside a fetch that brought a reading, the prepaid balance read with it
    /// (`prepaid`), which writes the row's `prepaid` object alone.
    fn record_rows(
        root: &AccountsRoot,
        outcomes: &BTreeMap<u32, Recorded<'_>>,
        prepaid: &BTreeMap<u32, PrepaidRead>,
        claims: &Claims,
        context: &PlanContext,
        clock: &dyn Fn() -> jiff::Timestamp,
    ) -> Result<BTreeSet<u32>, PollError> {
        if outcomes.is_empty() {
            return Ok(BTreeSet::new());
        }
        root.check_folders(&root.usage_lock())
            .map_err(PollError::CacheUnreadable)?;
        let _lock = acquire_in_root(root, &root.usage_lock(), USAGE_LOCK_TIMEOUT)
            .map_err(PollError::Lock)?;
        let now = clock();
        let mut document = load_document(root)?;
        let mut accounts = take_accounts(&mut document);
        let mut accepted = BTreeSet::new();
        for (number, result) in outcomes {
            let Some((identity, claim_id)) = claims.held.get(number) else {
                continue;
            };
            let Some(row) = accounts
                .get_mut(&number.to_string())
                .and_then(Value::as_object_mut)
            else {
                continue;
            };
            if !identity.matches(row)
                || row.get("claimId").and_then(Value::as_str) != Some(claim_id.as_str())
            {
                continue;
            }
            let is_active = context.active == Some(*number);
            apply_outcome(
                row,
                result,
                Planning {
                    is_active,
                    threshold: context.threshold,
                    models: &context.models,
                },
                now,
            );
            if let (Recorded::Fetch(Ok(Some(_))), Some(read)) = (result, prepaid.get(number)) {
                record_prepaid(row, read);
            }
            accepted.insert(*number);
        }
        if !accepted.is_empty() {
            put_accounts(&mut document, accounts);
            write_document(root, &document)?;
        }
        Ok(accepted)
    }
}

/// One reserve pass's inputs beyond the rows: the accounts it may claim, the stored fingerprints a strike is judged
/// against (refreshing polls only), and the time, in seconds since the epoch.
#[derive(Clone, Copy)]
struct Reserve<'r> {
    scope: FetchScope<'r>,
    fingerprints: Option<&'r BTreeMap<u32, StoredFingerprints>>,
    now: f64,
}

/// The fingerprints of an account's stored sources a strike is judged against: the credential the poll read (the
/// backup, or the live file for the account Claude Code is logged in as), and for the logged-in account its backup too,
/// since its refresh may post either.
#[derive(Clone)]
struct StoredFingerprints {
    read: String,
    backup: Option<BackupFingerprint>,
}

/// The logged-in account's backup as its strike reads it.
#[derive(Clone)]
enum BackupFingerprint {
    /// A backup holding a refresh token, by its fingerprint.
    Fp(String),
    /// No backup, or one with no refresh token: it carries no generation a strike could name.
    Missing,
    /// A backup that could not be read: whether it carries the struck generation is unknown.
    Unreadable,
}

/// What a reserve pass does with one account's row.
enum Verdict {
    /// Leave the row alone.
    Skip,
    /// Stamp a lease on the matching row.
    Claim,
    /// Replace a row naming another account (or none) with a fresh one, and stamp a lease on it.
    Replace,
}

impl Reserve<'_> {
    /// What the pass does with account `number`'s `row`, and whether a strike quarantines it (noted only when the pass
    /// has the stored fingerprints).
    fn verdict(&self, number: u32, identity: &Identity, row: Option<&Value>) -> (Verdict, bool) {
        let allowed = self.scope.allows(number);
        let Some(row) = row
            .and_then(Value::as_object)
            .filter(|row| identity.matches(row))
        else {
            return (
                if allowed {
                    Verdict::Replace
                } else {
                    Verdict::Skip
                },
                false,
            );
        };
        let stored = self
            .fingerprints
            .and_then(|fingerprints| fingerprints.get(&number));
        let struck = strike_applies(row, stored);
        let claim = !struck && allowed && row_eligible(row, self.now, self.scope.mode());
        (
            if claim { Verdict::Claim } else { Verdict::Skip },
            struck && self.fingerprints.is_some(),
        )
    }
}

/// Stamps a lease on each account of `identities` the pass may claim, replacing a row that names another account.
fn reserve_rows(
    accounts: &mut Map<String, Value>,
    identities: &BTreeMap<u32, Identity>,
    pass: Reserve<'_>,
) -> Claims {
    let mut claims = Claims::default();
    for (number, identity) in identities {
        let key = number.to_string();
        let (verdict, dead) = pass.verdict(*number, identity, accounts.get(&key));
        if dead {
            claims.dead.insert(*number);
        }
        match verdict {
            Verdict::Skip => continue,
            Verdict::Claim => {}
            Verdict::Replace => {
                accounts.insert(key.clone(), Value::Object(identity.fresh_row()));
            }
        }
        let Some(row) = accounts.get_mut(&key).and_then(Value::as_object_mut) else {
            continue;
        };
        let claim_id = new_claim_id();
        row.insert("lastAttemptAt".to_owned(), Value::from(pass.now));
        row.insert("claimId".to_owned(), Value::String(claim_id.clone()));
        row.insert("claimUntil".to_owned(), Value::from(pass.now + CLAIM_TTL_S));
        if prepaid_due(row, pass.now) {
            claims.prepaid_due.insert(*number);
        }
        claims.held.insert(*number, (identity.clone(), claim_id));
    }
    claims
}

/// The plan inputs one record needs beyond the row itself.
#[derive(Clone, Copy)]
struct Planning<'a> {
    is_active: bool,
    threshold: f64,
    models: &'a [String],
}

/// One account's outcome as a record writes it.
enum Recorded<'r> {
    /// A fetch's result.
    Fetch(&'r FetchResult),
    /// A refresh refused without a verdict on the token, recorded as a failure with the refusal's `kind` as `lastError`.
    Refused { kind: &'static str },
    /// A dead refresh lineage: a failure, plus a strike bound to the generation posted, as the usage table records it.
    Dead {
        kind: &'static str,
        struck_fp: Option<&'r str>,
    },
    /// Nothing to record: the lease is released and the row otherwise left alone.
    Release,
}

fn apply_outcome(
    row: &mut Map<String, Value>,
    recorded: &Recorded<'_>,
    planning: Planning<'_>,
    now: jiff::Timestamp,
) {
    let at = secs_from_timestamp(now);
    row.insert("claimId".to_owned(), Value::Null);
    row.insert("claimUntil".to_owned(), Value::from(0.0));
    if matches!(recorded, Recorded::Release) {
        return;
    }
    row.insert("lastAttemptAt".to_owned(), Value::from(at));
    match recorded {
        Recorded::Fetch(Ok(reading)) => record_success(row, reading.as_ref(), planning, now),
        Recorded::Fetch(Err(error)) => record_failure(
            row,
            &error.kind(),
            error.retry_after(),
            error.is_rate_limit(),
            at,
        ),
        Recorded::Refused { kind } => record_failure(row, kind, None, false, at),
        Recorded::Dead { kind, struck_fp } => {
            record_failure(row, kind, None, false, at);
            let strikes = failure_count(row.get("authDeadStrikes")).saturating_add(1);
            row.insert("authDeadStrikes".to_owned(), Value::from(strikes));
            row.insert(
                "struckFingerprint".to_owned(),
                struck_fp.map_or(Value::Null, |fp| Value::String(fp.to_owned())),
            );
        }
        Recorded::Release => {}
    }
}

fn record_success(
    row: &mut Map<String, Value>,
    reading: Option<&Value>,
    planning: Planning<'_>,
    now: jiff::Timestamp,
) {
    let at = secs_from_timestamp(now);
    let plan = plan_after_fetch(&PlanRequest {
        prev_interval_s: row.get("pollIntervalS").and_then(Value::as_f64),
        prev_usage: row.get("lastGood"),
        new_usage: reading,
        is_active: planning.is_active,
        threshold: planning.threshold,
        models: planning.models,
        recent_429: recent_429(row, at),
        now,
        jitter: unit_random(),
    });
    row.insert(
        "lastGood".to_owned(),
        reading.cloned().unwrap_or(Value::Null),
    );
    row.insert("fetchedAt".to_owned(), Value::from(at));
    row.insert(
        "nextPollAt".to_owned(),
        Value::from(secs_from_timestamp(plan.next_poll_at)),
    );
    row.insert("pollIntervalS".to_owned(), Value::from(plan.interval_s));
    row.insert("consecutiveFailures".to_owned(), Value::from(0));
    row.insert("lastError".to_owned(), Value::Null);
    row.insert("backoffUntil".to_owned(), Value::Null);
    row.insert("rejectedFingerprint".to_owned(), Value::Null);
    row.insert("authDeadStrikes".to_owned(), Value::from(0));
}

fn record_failure(
    row: &mut Map<String, Value>,
    kind: &str,
    retry_after: Option<f64>,
    rate_limited: bool,
    at: f64,
) {
    let failures = failure_count(row.get("consecutiveFailures")).saturating_add(1);
    row.insert("consecutiveFailures".to_owned(), Value::from(failures));
    row.insert("lastError".to_owned(), Value::String(kind.to_owned()));
    if rate_limited {
        row.insert("last429At".to_owned(), Value::from(at));
    }
    let wait = backoff_seconds(failures, retry_after, rate_limited);
    row.insert("backoffUntil".to_owned(), Value::from(at + wait));
}

/// Seconds to stay in backoff after the `failures`-th failure in a row. With no
/// `retry_after`: 30 s doubling per failure, capped at 600 s. A 429's `Retry-After: 0` waits at least 300 s. A longer
/// ask is honoured, a 429's above 600 s with 900 s added, and capped at 4500 s for a 429 and 3600 s otherwise; the
/// plain curve still wins when it is longer.
#[must_use]
pub fn backoff_seconds(failures: u32, retry_after: Option<f64>, rate_limited: bool) -> f64 {
    let shift = failures.saturating_sub(1).min(BACKOFF_MAX_SHIFT);
    let computed =
        (BACKOFF_BASE_S * 2f64.powi(i32::try_from(shift).unwrap_or(32))).min(BACKOFF_CAP_S);
    // A NaN header is no header; a negative one is zero.
    let Some(asked) = retry_after
        .filter(|seconds| !seconds.is_nan())
        .map(|seconds| seconds.max(0.0))
    else {
        return computed;
    };
    if asked <= 0.0 {
        if !rate_limited {
            return computed;
        }
        return computed.clamp(EDGE_BACKOFF_S, BACKOFF_CAP_S);
    }
    let mut wait = asked;
    if asked > BACKOFF_CAP_S && rate_limited {
        wait = asked + RETRY_AFTER_MARGIN_S;
    }
    wait = wait.min(if rate_limited {
        RETRY_AFTER_FLOOR_CAP_S
    } else {
        TRUST_MAX_AGE_S
    });
    wait.max(computed)
}

/// `(next poll, interval)` for an account just fetched successfully, planned over the 5-hour and
/// 7-day windows and the scoped windows the request's `models` name (`relevant_windows`). A binding window
/// that moved at least a point halves the interval (floor 180 s), or drops it to 60 s for the active account within
/// 15 points of the threshold; one that did not grows it ×1.5 toward 300 s (active) or 600 s. A recent 429 floors the
/// interval at 360 s and grows it ×1.5 up to 1800 s. An exhausted account polls every 600 s at least. The next poll
/// gets ±10 % jitter and is never later than the next window reset plus 60 s.
#[must_use]
pub fn plan_after_fetch(request: &PlanRequest<'_>) -> PollPlan {
    let now = secs_from_timestamp(request.now);
    let interval_s = plan_interval(request);
    let new_windows = windows_of(request.new_usage, request.models);
    let headroom = binding_pct(&new_windows).map(|pct| 100.0 - pct);
    let exhausted = headroom.is_some_and(|headroom| headroom <= 0.0);
    let interval_s = if exhausted {
        interval_s.max(EXHAUSTED_INTERVAL_S)
    } else {
        interval_s
    };
    let mut next = now + interval_s * (1.0 + JITTER_FRAC * (2.0 * request.jitter - 1.0));
    let reset = if exhausted {
        limiting_reset(&new_windows).filter(|reset| *reset > now)
    } else {
        earliest_future_reset(&new_windows, now)
    };
    if let Some(reset) = reset {
        next = next.min(reset + RESET_SLACK_S);
    }
    PollPlan {
        // A time past jiff's range is planned at the fetch itself: due at once rather than never.
        next_poll_at: timestamp_from_secs(next).unwrap_or(request.now),
        interval_s,
    }
}

/// The interval half of [`plan_after_fetch`], before the exhausted-account floor.
fn plan_interval(request: &PlanRequest<'_>) -> f64 {
    let (default, ceiling) = if request.is_active {
        (MIN_INTERVAL_S, ACTIVE_MAX_INTERVAL_S)
    } else {
        (CANDIDATE_DEFAULT_INTERVAL_S, CANDIDATE_MAX_INTERVAL_S)
    };
    // The previous interval or the default: a zero interval is no interval.
    let base = request
        .prev_interval_s
        .filter(|interval| *interval != 0.0)
        .unwrap_or(default);
    let prev_pct = binding_pct(&windows_of(request.prev_usage, request.models));
    let new_pct = binding_pct(&windows_of(request.new_usage, request.models));
    let (moving, mut interval) = match (prev_pct, new_pct) {
        (Some(prev), Some(new)) if (new - prev).abs() >= MOVEMENT_DELTA_PCT => {
            (true, MIN_INTERVAL_S.max(base / 2.0))
        }
        (Some(_), Some(_)) => (false, ceiling.min(MIN_INTERVAL_S.max(base * 1.5))),
        _ => (false, default),
    };
    let urgent = new_pct.is_some_and(|pct| pct >= request.threshold - ESCALATION_MARGIN_PCT);
    if request.is_active && moving && !request.recent_429 && urgent {
        interval = URGENT_INTERVAL_S;
    }
    if request.recent_429 {
        let increased = (base * POST_429_BACKOFF_MULT).max(POST_429_MIN_INTERVAL_S);
        interval = POST_429_MAX_INTERVAL_S.min(interval.max(increased));
    }
    interval
}

/// The windows the planner reads: those [`relevant_windows`] gives for `models`, so the binding window and the earliest
/// reset agree with the switch decision's.
fn windows_of(reading: Option<&Value>, models: &[String]) -> Vec<UsageWindow> {
    let Some(reading) = reading else {
        return Vec::new();
    };
    relevant_windows(reading, models)
        .into_iter()
        .map(|(_, window)| window)
        .collect()
}

/// The binding (fullest) window's utilization.
fn binding_pct(windows: &[UsageWindow]) -> Option<f64> {
    windows.iter().map(|window| window.pct).reduce(f64::max)
}

/// When the last exhausted window resets: the account is usable again then.
fn limiting_reset(windows: &[UsageWindow]) -> Option<f64> {
    windows
        .iter()
        .filter(|window| window.pct >= 100.0)
        .filter_map(|window| window.resets_at.map(secs_from_timestamp))
        .reduce(f64::max)
}

/// The next window reset after `now`, at any utilization.
fn earliest_future_reset(windows: &[UsageWindow], now: f64) -> Option<f64> {
    windows
        .iter()
        .filter_map(|window| window.resets_at.map(secs_from_timestamp))
        .filter(|reset| *reset > now)
        .reduce(f64::min)
}

/// Whether the account answered 429 recently: within an hour of the 429's backoff lifting, or of the 429 itself when a
/// later failure has replaced that backoff.
fn recent_429(row: &Map<String, Value>, now: f64) -> bool {
    let Some(last_429) = number(row, "last429At") else {
        return false;
    };
    let mut anchor = last_429;
    let rate_limit_backoff = row.get("lastError").and_then(Value::as_str) == Some("http-429");
    if let Some(backoff_until) =
        number(row, "backoffUntil").filter(|until| rate_limit_backoff && *until > anchor)
    {
        anchor = backoff_until;
    }
    now < anchor + RECENT_429_WINDOW_S
}

/// Whether a row's dead-token strike quarantines the stored generation:
/// at least one strike, and no `stored` fingerprints, no `struckFingerprint`, or a stored source that carries the struck
/// generation: the credential the poll read, or the logged-in account's backup. A backup that could not be read keeps
/// the strike; a missing one does not. A strike on a generation no stored source carries has lapsed.
fn strike_applies(row: &Map<String, Value>, stored: Option<&StoredFingerprints>) -> bool {
    if failure_count(row.get("authDeadStrikes")) < 1 {
        return false;
    }
    let (Some(stored), Some(struck)) =
        (stored, row.get("struckFingerprint").and_then(Value::as_str))
    else {
        return true;
    };
    if stored.read == struck {
        return true;
    }
    match &stored.backup {
        None | Some(BackupFingerprint::Missing) => false,
        Some(BackupFingerprint::Unreadable) => true,
        Some(BackupFingerprint::Fp(backup)) => backup == struck,
    }
}

/// Whether a matching row the strikes leave alone may be fetched at `now`: never in backoff,
/// held, or under another collector's live lease; then by `mode` ([`FetchMode`]), or on demand (`None`) stale and past
/// its planned poll, with no plan, or with a plan no planner could have written.
fn row_eligible(row: &Map<String, Value>, now: f64, mode: Option<FetchMode>) -> bool {
    let backing_off = number(row, "backoffUntil").is_some_and(|until| now < until);
    let held = number(row, "heldUntil").is_some_and(|until| now < until);
    let claimed = live_claim(number(row, "claimUntil"), number(row, "lastAttemptAt"), now);
    if backing_off || held || claimed {
        return false;
    }
    let stale = number(row, "fetchedAt").is_none_or(|fetched| now - fetched > SERVE_TTL_S);
    let next_poll_at = number(row, "nextPollAt");
    let poll_due = next_poll_at.is_some_and(|next| now >= next);
    let unplanned_or_overslept = || {
        next_poll_at.is_none() || plan_oversleeps(next_poll_at, number(row, "pollIntervalS"), now)
    };
    match mode {
        None => stale && (poll_due || unplanned_or_overslept()),
        Some(FetchMode::Scheduled) => poll_due || (stale && unplanned_or_overslept()),
        Some(FetchMode::Escalation) => poll_due || stale,
        Some(FetchMode::Force) => true,
    }
}

/// Whether a planned poll lies further out than any plan [`plan_after_fetch`] could make from the row's interval.
pub(crate) fn plan_oversleeps(next_poll_at: Option<f64>, interval: Option<f64>, now: f64) -> bool {
    let Some(next_poll_at) = next_poll_at else {
        return false;
    };
    let interval = interval
        .filter(|interval| *interval != 0.0)
        .unwrap_or(EXHAUSTED_INTERVAL_S)
        .max(EXHAUSTED_INTERVAL_S);
    next_poll_at > now + interval * (1.0 + JITTER_FRAC) + RESET_SLACK_S
}

fn number(row: &Map<String, Value>, key: &str) -> Option<f64> {
    row.get(key).and_then(Value::as_f64)
}

/// Fetches one account's usage with `token`, without a refresh: a 2xx answer
/// becomes the reading the usage table stores, any other status a [`FetchError::Http`] with its `Retry-After`.
///
/// # Errors
///
/// [`FetchError`] for a non-2xx status, no answer, or an answer that is not a usage reading.
pub fn fetch_usage<T: UsageTransport + ?Sized>(transport: &T, token: &AccessToken) -> FetchResult {
    match transport.get_usage(token) {
        Ok(answer) => classify(&answer),
        Err(TransportError::Timeout) => Err(FetchError::Timeout),
        // `get_usage` has no default, so a transport answers `Unsupported` only by mistake: it reached no endpoint.
        Err(TransportError::Connect | TransportError::Unsupported) => Err(FetchError::Network),
        Err(TransportError::MalformedBody) => Err(FetchError::BadResponse),
    }
}

/// One prepaid balance read: the balance in minor units, or why the read failed, and when the read returned.
#[derive(Clone, Debug, PartialEq)]
enum PrepaidRead {
    /// The organization's balance; `amount_minor` 0 with no currency is "no prepaid balance", not a failure.
    Balance {
        amount_minor: i64,
        exponent: u32,
        currency: Option<String>,
        /// Whether the organization tops its balance up on its own; `None` when the answer does not say.
        auto_reload: Option<bool>,
        /// When the read returned, in seconds since the epoch: the balance's `fetchedAt`.
        at: f64,
    },
    /// The read failed; recorded under [`FetchError::kind`].
    Failed {
        error: FetchError,
        /// When the read returned, in seconds since the epoch: the failure's `attemptedAt`.
        at: f64,
    },
}

/// The prepaid balance read one claimed account's usage read may bring along: the organization to read it for, when the
/// account has one, its balance is due and the pass has not yet used [`PREPAID_CUTOFF_S`] of its lease, the cutoff and
/// the clock it is judged by, and the read once made.
pub(super) struct PrepaidProbe<'o> {
    org_uuid: Option<&'o str>,
    /// The time, in seconds since the epoch, from which the pass reads no balance: its lease's halfway point.
    cutoff: f64,
    clock: &'o dyn Fn() -> jiff::Timestamp,
    read: RefCell<Option<PrepaidRead>>,
}

impl<'o> PrepaidProbe<'o> {
    /// A probe for organization `org_uuid` that reads only when `due`, the account has an organization, and `clock` is
    /// still before `cutoff`.
    fn new(
        org_uuid: &'o str,
        due: bool,
        cutoff: f64,
        clock: &'o dyn Fn() -> jiff::Timestamp,
    ) -> Self {
        Self {
            org_uuid: (due && !org_uuid.is_empty()).then_some(org_uuid),
            cutoff,
            clock,
            read: RefCell::new(None),
        }
    }

    fn into_read(self) -> Option<PrepaidRead> {
        self.read.into_inner()
    }
}

/// [`fetch_usage`] with `token`, then, when it brought a reading and `probe` is due, the prepaid balance with the same
/// token. The balance read never refreshes, retries or spends a grant: its failure, a 401 included, is only recorded in
/// the row's `prepaid` object, and a transport that does not send it ([`TransportError::Unsupported`]) leaves no read.
pub(super) fn fetch_reading<T: UsageTransport + ?Sized>(
    transport: &T,
    token: &AccessToken,
    probe: &PrepaidProbe<'_>,
) -> FetchResult {
    let result = fetch_usage(transport, token);
    read_prepaid_beside(transport, token, probe, &result);
    result
}

/// The prepaid balance read beside a usage fetch that already ran with `token`: only when `result` brought a reading,
/// `probe` is due, and the pass has not yet used [`PREPAID_CUTOFF_S`] of its lease, with that same token, as
/// [`fetch_reading`] describes. A caller that may still refuse the reading (the logged-in account's
/// [`PollResult::ForeignCredential`]) fetches with [`fetch_usage`] and calls this once the reading is accepted.
pub(super) fn read_prepaid_beside<T: UsageTransport + ?Sized>(
    transport: &T,
    token: &AccessToken,
    probe: &PrepaidProbe<'_>,
    result: &FetchResult,
) {
    if matches!(result, Ok(Some(_)))
        && let Some(org_uuid) = probe.org_uuid
        && secs_from_timestamp((probe.clock)()) < probe.cutoff
    {
        *probe.read.borrow_mut() = read_prepaid(transport, token, org_uuid, probe.clock);
    }
}

/// The balance read for `org_uuid` with `token`, dated by `clock` once the read returns. An `org_uuid` that is not a
/// canonical UUID ([`is_org_uuid`]'s rule) is refused here, whatever transport is given, and like
/// [`TransportError::Unsupported`] leaves no read.
fn read_prepaid<T: UsageTransport + ?Sized>(
    transport: &T,
    token: &AccessToken,
    org_uuid: &str,
    clock: &dyn Fn() -> jiff::Timestamp,
) -> Option<PrepaidRead> {
    if !is_org_uuid(org_uuid) {
        return None;
    }
    let answer = transport.get_prepaid(token, org_uuid);
    let at = secs_from_timestamp(clock());
    let failed = |error| Some(PrepaidRead::Failed { error, at });
    match answer {
        Ok(answer) if answer.status == 200 => Some(parse_prepaid(&answer.body, at)),
        Ok(answer) => failed(FetchError::Http {
            status: answer.status,
            retry_after: None,
        }),
        Err(TransportError::Unsupported) => None,
        Err(TransportError::Timeout) => failed(FetchError::Timeout),
        Err(TransportError::Connect) => failed(FetchError::Network),
        Err(TransportError::MalformedBody) => failed(FetchError::BadResponse),
    }
}

/// A prepaid-credits answer's balance: `amount` in minor units (a whole number, else `bad-response`), `currency` (null
/// or absent, or exactly three ASCII uppercase letters, else `bad-response`), and the scale `balance.credits.exponent`
/// names ([`DEFAULT_MINOR_EXPONENT`] when it names none; one past [`MAX_MINOR_EXPONENT`] is `bad-response`), and
/// whether `auto_reload_settings` turns automatic top-ups on: `true` or `false` from an object whose `enabled` is that
/// boolean, `None` when the settings are null, absent or malformed.
fn parse_prepaid(body: &Value, at: f64) -> PrepaidRead {
    let Some(amount_minor) = body.get("amount").and_then(Value::as_i64) else {
        return PrepaidRead::Failed {
            error: FetchError::BadResponse,
            at,
        };
    };
    let Some(exponent) = minor_exponent(body.pointer("/balance/credits/exponent")) else {
        return PrepaidRead::Failed {
            error: FetchError::BadResponse,
            at,
        };
    };
    let currency = match currency_code(body.get("currency")) {
        Ok(currency) => currency,
        Err(error) => return PrepaidRead::Failed { error, at },
    };
    PrepaidRead::Balance {
        amount_minor,
        exponent,
        currency,
        auto_reload: body
            .get("auto_reload_settings")
            .and_then(Value::as_object)
            .and_then(|settings| settings.get("enabled"))
            .and_then(Value::as_bool),
        at,
    }
}

/// A prepaid answer's currency: `None` when null or absent, the code when it is exactly three ASCII uppercase letters.
///
/// # Errors
///
/// [`FetchError::BadResponse`] for anything else.
fn currency_code(value: Option<&Value>) -> Result<Option<String>, FetchError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(code)) if is_currency_code(code) => Ok(Some(code.clone())),
        Some(_) => Err(FetchError::BadResponse),
    }
}

/// A minor-unit scale as an answer names it: [`DEFAULT_MINOR_EXPONENT`] when absent or null, the number when it is a
/// whole number (an integer, or an integral float such as `2.0`) up to [`MAX_MINOR_EXPONENT`], else `None`.
fn minor_exponent(value: Option<&Value>) -> Option<u32> {
    match value {
        None | Some(Value::Null) => Some(DEFAULT_MINOR_EXPONENT),
        Some(value) => {
            if let Some(exponent) = value.as_u64() {
                return u32::try_from(exponent)
                    .ok()
                    .filter(|exponent| *exponent <= MAX_MINOR_EXPONENT);
            }
            let exponent = value.as_f64()?;
            (0..=MAX_MINOR_EXPONENT)
                .find(|whole| (f64::from(*whole) - exponent).abs() < f64::EPSILON)
        }
    }
}

/// Why a balance-only read (`read_balance`) gave no balance the engine may spend on.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BalanceReadError {
    /// The collector or its transport does not read prepaid balances: no paid fallback runs on it.
    #[error("this collector does not read prepaid balances")]
    Unsupported,
    /// The organization asked for is not the account's own, or not a canonical UUID; nothing was sent.
    #[error("the organization is not the account's own or not a canonical UUID; nothing was sent")]
    Organization,
    /// No switchable account in the roster has this number.
    #[error("no switchable account has this number")]
    UnknownAccount,
    /// The usage table holds no row naming this account, so the balance was not recorded and is not used.
    #[error("the account has no usage row of its own; the balance was not recorded")]
    NoRow,
    /// The read failed; only the row's `prepaid.error` and `prepaid.attemptedAt` record it.
    #[error("the prepaid balance read failed: {0}")]
    Failed(FetchError),
    /// The usage table could not be locked, read or written.
    #[error("the usage table could not be locked, read or written")]
    Cache(#[source] PollError),
    /// The token is the live credential's, whose lineage the profile oracle named as another login's than the
    /// account's; nothing was sent.
    #[error(
        "the token is the live credential, which the profile oracle named as another login's; nothing was sent"
    )]
    ForeignCredential,
}

/// One balance-only read of account `number`'s organization `org_uuid` with `token`, the caller's: no usage `GET`, no
/// refresh and no grant. `identity` is the account's as `accounts.json` records it, and `org_uuid` must be its
/// organization. Under `cache/.usage.lock` the read is written into the account's own row only (the row's identity
/// must match `identity`), as the poll's own balance read is: a balance only over an older `fetchedAt`, a failure only
/// as `prepaid.error` and `prepaid.attemptedAt`, leaving the usage reading and its failure state alone. Returns the
/// balance the row holds after the write: a later read another collector recorded wins.
///
/// # Errors
///
/// As [`BalanceReadError`].
pub(crate) fn read_balance<T: UsageTransport + ?Sized>(
    transport: &T,
    root: &AccountsRoot,
    identity: &Identity,
    request: (u32, &AccessToken, &str),
    clock: &dyn Fn() -> jiff::Timestamp,
) -> Result<PrepaidEntry, BalanceReadError> {
    let (number, token, org_uuid) = request;
    if identity.org_uuid != org_uuid || !is_org_uuid(org_uuid) {
        return Err(BalanceReadError::Organization);
    }
    let read =
        read_prepaid(transport, token, org_uuid, clock).ok_or(BalanceReadError::Unsupported)?;
    root.check_folders(&root.usage_lock())
        .map_err(|error| BalanceReadError::Cache(PollError::CacheUnreadable(error)))?;
    let _lock = acquire_in_root(root, &root.usage_lock(), USAGE_LOCK_TIMEOUT)
        .map_err(|error| BalanceReadError::Cache(PollError::Lock(error)))?;
    let mut document = load_document(root).map_err(BalanceReadError::Cache)?;
    let mut accounts = take_accounts(&mut document);
    let Some(row) = accounts
        .get_mut(&number.to_string())
        .and_then(Value::as_object_mut)
        .filter(|row| identity.matches(row))
    else {
        return Err(BalanceReadError::NoRow);
    };
    record_prepaid(row, &read);
    let stored = row.get("prepaid").and_then(PrepaidEntry::from_value);
    put_accounts(&mut document, accounts);
    write_document(root, &document).map_err(BalanceReadError::Cache)?;
    match read {
        PrepaidRead::Failed { error, .. } => Err(BalanceReadError::Failed(error)),
        PrepaidRead::Balance { .. } => stored.ok_or(BalanceReadError::NoRow),
    }
}

/// Whether a claimed row's prepaid balance is due at `now`: it has none, its last read, success or failure
/// (`attemptedAt`, else `fetchedAt`), is at least [`PREPAID_DUE_S`] old, or that time lies more than
/// [`FUTURE_FETCH_SLACK_S`] ahead of `now`, so a clock that moved back does not keep it from being read.
fn prepaid_due(row: &Map<String, Value>, now: f64) -> bool {
    let Some(prepaid) = row.get("prepaid").and_then(Value::as_object) else {
        return true;
    };
    let last = number(prepaid, "attemptedAt").or_else(|| number(prepaid, "fetchedAt"));
    last.is_none_or(|last| now - last >= PREPAID_DUE_S || last - now > FUTURE_FETCH_SLACK_S)
}

/// How far after a read a stored balance's `fetchedAt` may lie and still count as a later read (another collector's,
/// on a clock a little ahead), in seconds; one further ahead is older than any real read.
pub(crate) const FUTURE_FETCH_SLACK_S: f64 = 60.0;

/// Writes a balance read into the row's `prepaid` object at the read's own time, and nothing else of the row: a balance
/// replaces the figure, its `fetchedAt` and `attemptedAt` and clears `error`, unless the stored figure's `fetchedAt` is
/// as new or newer by at most [`FUTURE_FETCH_SLACK_S`] (another collector recorded a later read; one dated further
/// ahead counts as older than any real read); a failure sets only `error` and `attemptedAt`, so the last good figure and
/// its `fetchedAt` stay, unless a later attempt is already recorded. Keys neither branch models stay as they were.
fn record_prepaid(row: &mut Map<String, Value>, read: &PrepaidRead) {
    let stored = row.get("prepaid").and_then(Value::as_object);
    let stored_time = |key: &str| stored.and_then(|prepaid| number(prepaid, key));
    match read {
        PrepaidRead::Balance {
            amount_minor,
            exponent,
            currency,
            auto_reload,
            at,
        } => {
            if stored_time("fetchedAt")
                .is_some_and(|fetched| fetched >= *at && fetched - at <= FUTURE_FETCH_SLACK_S)
            {
                return;
            }
            let prepaid = prepaid_object(row);
            prepaid.insert("amountMinor".to_owned(), Value::from(*amount_minor));
            prepaid.insert("exponent".to_owned(), Value::from(*exponent));
            prepaid.insert("currency".to_owned(), Value::from(currency.clone()));
            prepaid.insert("autoReload".to_owned(), Value::from(*auto_reload));
            prepaid.insert("fetchedAt".to_owned(), Value::from(*at));
            prepaid.insert("error".to_owned(), Value::Null);
            prepaid.insert("attemptedAt".to_owned(), Value::from(*at));
        }
        PrepaidRead::Failed { error, at } => {
            if stored_time("attemptedAt")
                .is_some_and(|attempted| attempted > *at && attempted - at <= FUTURE_FETCH_SLACK_S)
            {
                return;
            }
            let prepaid = prepaid_object(row);
            prepaid.insert("error".to_owned(), Value::String(error.kind()));
            prepaid.insert("attemptedAt".to_owned(), Value::from(*at));
        }
    }
}

/// The row's `prepaid` object, made fresh when the row has none there or holds something that is not an object: a
/// balance read and a failed one both write into it, so keys neither models survive.
#[expect(
    clippy::expect_used,
    reason = "the entry is replaced by an object just above"
)]
fn prepaid_object(row: &mut Map<String, Value>) -> &mut Map<String, Value> {
    let entry = row
        .entry("prepaid")
        .or_insert_with(|| Value::Object(Map::new()));
    if !entry.is_object() {
        *entry = Value::Object(Map::new());
    }
    entry
        .as_object_mut()
        .expect("prepaid is an object after the replacement")
}

fn classify(answer: &RawUsage) -> FetchResult {
    if (200..300).contains(&answer.status) {
        return build_usage_result(&answer.body);
    }
    Err(FetchError::Http {
        status: answer.status,
        retry_after: answer
            .retry_after
            .filter(|seconds| !seconds.is_nan())
            .map(|seconds| seconds.max(0.0)),
    })
}

/// The usage reading, with no display strings (a display recomputes them from `resets_at`):
/// `{"five_hour": {"pct", "resets_at"}, "seven_day": …, "extra_usage": {…}, "spend": …, "scoped": [...]}`, each part
/// the answer leaves out or empty omitted, and `None` when none of the windows (5-hour, 7-day, per-model) nor `spend` is
/// there. `extra_usage` holds the block's switches ([`extra_usage_flags`]);
/// they are added only to a reading, never making one of an otherwise empty answer. The whole reading is stored as
/// `lastGood`, so an engine with per-model windows set finds them on every row.
fn build_usage_result(body: &Value) -> FetchResult {
    let Value::Object(data) = body else {
        return Err(FetchError::BadResponse);
    };
    let mut result = Map::new();
    for key in ["five_hour", "seven_day"] {
        let Some(window) = data.get(key).filter(|window| store::truthy(window)) else {
            continue;
        };
        let utilization = window.get("utilization").ok_or(FetchError::BadResponse)?;
        let mut entry = Map::new();
        entry.insert("pct".to_owned(), utilization.clone());
        copy_reset(window, &mut entry);
        result.insert(key.to_owned(), Value::Object(entry));
    }
    let scoped: Vec<Value> = match data.get("limits") {
        Some(Value::Array(limits)) => limits.iter().filter_map(scoped_entry).collect(),
        Some(_) | None => Vec::new(),
    };
    if !scoped.is_empty() {
        result.insert("scoped".to_owned(), Value::Array(scoped));
    }
    if let Some(spend) = spend_entry(data) {
        result.insert("spend".to_owned(), spend);
    }
    if result.is_empty() {
        return Ok(None);
    }
    if let Some(extra) = data.get("extra_usage").and_then(Value::as_object) {
        result.insert("extra_usage".to_owned(), extra_usage_flags(extra));
    }
    Ok(Some(Value::Object(result)))
}

/// Copies a part's `resets_at` into its entry when the answer gives one.
fn copy_reset(part: &Value, entry: &mut Map<String, Value>) {
    if let Some(resets_at) = part
        .get("resets_at")
        .filter(|resets_at| store::truthy(resets_at))
    {
        entry.insert("resets_at".to_owned(), resets_at.clone());
    }
}

/// The `extra_usage` block's switches, `{"enabled", "user_disabled", "spend_limit_reached", "disabled_reason_set"}`:
/// `enabled` false when the block leaves `is_enabled` out, so a block that is off is told apart from one that is on with
/// no cap; `user_disabled` and `spend_limit_reached` left out when the block leaves them out or sends null, and
/// `disabled_reason_set` left out when the block has no `disabled_reason` key, true for a non-null one and false for
/// null. A flag left out is unknown, which the paid fallback reads as ineligible.
fn extra_usage_flags(extra: &Map<String, Value>) -> Value {
    let mut flags = Map::new();
    flags.insert(
        "enabled".to_owned(),
        Value::Bool(extra.get("is_enabled").is_some_and(store::truthy)),
    );
    for key in ["user_disabled", "spend_limit_reached"] {
        if let Some(value) = extra.get(key).filter(|value| !value.is_null()) {
            flags.insert(key.to_owned(), Value::Bool(store::truthy(value)));
        }
    }
    if let Some(reason) = extra.get("disabled_reason") {
        flags.insert(
            "disabled_reason_set".to_owned(),
            Value::Bool(!reason.is_null()),
        );
    }
    Value::Object(flags)
}

/// The `spend`: the enabled `extra_usage` block with its credits in minor units turned into currency units by its
/// `decimal_places` (2 when it names none), `{"used", "limit", "pct", "currency", "resets_at"}`. An uncapped block
/// (no `monthly_limit`) keeps its spend with `limit` and `pct` null. Left out when the block is off, `used_credits` is
/// missing, null or not a number, or `decimal_places` is not a whole number up to [`MAX_MINOR_EXPONENT`].
fn spend_entry(data: &Map<String, Value>) -> Option<Value> {
    let extra = data
        .get("extra_usage")
        .filter(|extra| store::truthy(extra))?;
    if !extra.get("is_enabled").is_some_and(store::truthy) {
        return None;
    }
    let exponent = minor_exponent(extra.get("decimal_places"))?;
    let scale = 10f64.powi(i32::try_from(exponent).ok()?);
    let used = python_float(extra.get("used_credits")?)? / scale;
    let limit = extra
        .get("monthly_limit")
        .and_then(python_float)
        .map(|limit| limit / scale);
    let pct = extra.get("utilization").and_then(python_float);
    let mut entry = Map::new();
    entry.insert("used".to_owned(), Value::from(used));
    entry.insert("limit".to_owned(), Value::from(limit));
    entry.insert("pct".to_owned(), Value::from(pct));
    entry.insert(
        "currency".to_owned(),
        extra
            .get("currency")
            .cloned()
            .unwrap_or_else(|| Value::from("USD")),
    );
    copy_reset(extra, &mut entry);
    Some(Value::Object(entry))
}

/// One of the `scoped` per-model windows, `{"name", "pct", "resets_at"}`, from a `limits[]` entry whose
/// `scope.model.display_name` is set and whose `percent` is a number; any other entry gives none.
fn scoped_entry(limit: &Value) -> Option<Value> {
    let name = limit
        .get("scope")?
        .get("model")?
        .get("display_name")
        .filter(|name| store::truthy(name))?;
    let pct = limit.get("percent").and_then(Value::as_f64)?;
    let mut entry = Map::new();
    entry.insert("name".to_owned(), name.clone());
    entry.insert("pct".to_owned(), Value::from(pct));
    copy_reset(limit, &mut entry);
    Some(Value::Object(entry))
}

/// A value as Python's `float()` reads one parsed from JSON: a number, or a string holding one. `None` for anything
/// else, and for a value that is not finite, which JSON cannot hold.
fn python_float(value: &Value) -> Option<f64> {
    let number = match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse::<f64>().ok(),
        Value::Null | Value::Bool(_) | Value::Array(_) | Value::Object(_) => None,
    }?;
    number.is_finite().then_some(number)
}

/// Polls every enabled OAuth account in `store` once: reads its credentials, refuses an expired access token without
/// sending it, reserves the accounts due ([`UsageTable::reserve`]), fetches each with no lock held, and records the
/// outcomes ([`UsageTable::record`]). The account Claude Code is logged in as, found by the identity
/// (email and organization) `~/.claude.json`'s `oauthAccount` names rather than by `accounts.json`'s active number, is
/// fetched with Claude Code's live credential (`claude`'s `.credentials.json`), since Claude Code renews that one and
/// the backup copy is usually stale. Both files are read without a lock and never written. A live
/// login no stored account matches, or a `~/.claude.json` that is missing or unreadable, lends its token to no account;
/// a live credential file that is missing, unreadable or holds no OAuth token falls back to the backup. The logged-in
/// account is also the only one planned at the active cadence: when no stored account is logged in, every account is
/// planned at the candidate cadence, whatever `accounts.json` calls active. `clock` is read once for the expiry checks
/// and the reserve, and again by the record once the fetches are done. `settings` gives the threshold and the model
/// names the cadence plan judges each account's binding window by. Returns one outcome per account in rotation order;
/// no accounts at all gives none.
///
/// # Errors
///
/// [`PollError::Store`] when `accounts.json` cannot be read, and the errors of [`UsageTable::reserve`] and
/// [`UsageTable::record`].
pub fn poll_once<T: UsageTransport + ?Sized>(
    store: &Store,
    claude: &ClaudePaths,
    transport: &T,
    settings: &AutoSwitchSettings,
    clock: &dyn Fn() -> jiff::Timestamp,
) -> Result<Vec<PollOutcome>, PollError> {
    let poller = Poller {
        store,
        claude,
        clock,
        settings,
        refresh: None,
        scope: FetchScope::OnDemand,
    };
    poller.poll(transport)
}

/// [`poll_once`]'s read-only path within `scope`, on `context`'s store, Claude Code's files and clock: each account is
/// read with its stored access token only (the live credential's for the account Claude Code is logged in as), never
/// refreshed and never granted, so an expired token is refused unsent and a 401 is recorded as a failure.
///
/// # Errors
///
/// As [`poll_once`].
pub(crate) fn poll_read_only<T: UsageTransport + ?Sized>(
    transport: &T,
    settings: &AutoSwitchSettings,
    context: &RefreshContext<'_>,
    scope: FetchScope<'_>,
) -> Result<Vec<PollOutcome>, PollError> {
    let poller = Poller {
        store: context.store,
        claude: context.claude,
        clock: context.clock,
        settings,
        refresh: None,
        scope,
    };
    poller.poll(transport)
}

/// [`poll_once`], refreshing inactive accounts through the consume gate ([`consume_backup_grant`]).
/// The store, Claude Code's files and the clock are `refresh`'s.
///
/// An account that is neither the one Claude Code is logged in as nor `accounts.json`'s active one, whose backup
/// access token has expired and which holds a refresh token, is refreshed before its fetch; a fetch of such an account
/// (refreshed or not) that answers 401 refreshes once through the gate and retries once. A dead lineage
/// (`invalid_grant`, no refresh token) is recorded as a failure with a strike bound to the generation the gate posted,
/// and shows as [`PollResult::AuthDead`], as does an account a strike on a stored generation keeps out of the poll (for
/// the account Claude Code is logged in as, its live credential's generation or its backup's, or any while its backup
/// cannot be read); a strike on a generation no stored source carries has lapsed. A refusal with no verdict on the
/// token (`store-unmirrored`, `consume-busy`, `stash-unreadable`, `invalid_client`) is recorded as a failure under its
/// own name, a 401 the refresh could not heal as `refresh-failed`; a transient refresh failure before the fetch falls
/// through to the 401 path. A started drain, an account that became active, or a store that
/// refuses writes (`store-unwritable`, shown as [`PollResult::Deferred`]) records nothing before the fetch and only
/// releases the lease; after a 401 it records the 401. A store a newer version wrote fails the poll with
/// [`PollError::StoreRefused`] before any grant is spent.
///
/// The account Claude Code is logged in as, when its live credential holds an access token, is fetched with that
/// token and refreshed under Claude Code's own locks when it has expired or answers 401:
/// the successor goes to the backup (the unclaimed stash when that write fails) and to the live file's
/// `claudeAiOauth`, and `primaryApiKey` is cleared from `~/.claude.json` under its config lock
/// ([`PollOutcome::primary_key_cleared`], and [`PollOutcome::backup_written`] for the backup). A successor an earlier
/// pass left in the stash is adopted without a POST. A refresh put off before anything is spent shows as
/// [`PollResult::Deferred`] (or the 401 that started it), a live credential the profile oracle names as another
/// account's as [`PollResult::ForeignCredential`], and a successor the live file could not take as
/// [`PollResult::LiveWriteFailed`]. A server-accepted live credential whose lineage differs from the backup's is
/// written to the backup once the oracle attributes it to the account. `accounts.json`'s active account, when Claude
/// Code is not logged in as it, keeps [`poll_once`]'s read-only path.
///
/// The account Claude Code is logged in as, when the reserve leaves it unclaimed (and no strike keeps it out) while its
/// live access token has expired and no hold (`heldUntil`) explains the wait, shows as [`PollResult::TokenExpired`]
/// rather than [`PollResult::NotDue`]: the next pass that claims it refreshes it.
///
/// # Errors
///
/// As [`poll_once`].
pub fn poll_once_with_refresh<T: UsageTransport + ?Sized>(
    transport: &T,
    settings: &AutoSwitchSettings,
    refresh: &RefreshContext<'_>,
) -> Result<Vec<PollOutcome>, PollError> {
    poll_with_refresh(transport, settings, refresh, FetchScope::OnDemand)
}

/// [`poll_once_with_refresh`] within `scope`: every account
/// is gathered and reported (an API key, a missing or expired credential, a struck lineage), but only the accounts the
/// scope allows are reserved, past the gates its [`FetchMode`] names; any other account shows as
/// [`PollResult::NotDue`], or [`PollResult::AuthDead`] when a strike on a stored generation keeps it out.
///
/// # Errors
///
/// As [`poll_once`].
pub fn poll_with_refresh<T: UsageTransport + ?Sized>(
    transport: &T,
    settings: &AutoSwitchSettings,
    refresh: &RefreshContext<'_>,
    scope: FetchScope<'_>,
) -> Result<Vec<PollOutcome>, PollError> {
    let poller = Poller {
        store: refresh.store,
        claude: refresh.claude,
        clock: refresh.clock,
        settings,
        refresh: Some(refresh),
        scope,
    };
    poller.poll(transport)
}

/// One poll's inputs, with the consume gate's when inactive accounts are refreshed, and the accounts it may fetch.
struct Poller<'p> {
    store: &'p Store,
    claude: &'p ClaudePaths,
    clock: &'p dyn Fn() -> jiff::Timestamp,
    /// The auto-switch settings whose threshold and models plan each account's next poll.
    settings: &'p AutoSwitchSettings,
    refresh: Option<&'p RefreshContext<'p>>,
    scope: FetchScope<'p>,
}

/// An account that may be fetched: its credentials, its stored generation's fingerprint (refreshing polls only, with
/// its backup's for the account Claude Code is logged in as), and for an inactive account that may be refreshed, the
/// backup credential the gate compares against. Only a refreshable account lacks credentials: its access
/// token is empty, left so by a grant kept for its refresh token.
struct Candidate {
    identity: Identity,
    credentials: Option<OauthCredentials>,
    fingerprint: Option<String>,
    backup_fingerprint: Option<BackupFingerprint>,
    refreshable: Option<CredentialBlob>,
    /// In a refreshing poll, the account Claude Code is logged in as and the live credential the poll read: it is
    /// fetched and refreshed under Claude Code's locks ([`active::fetch`]).
    live: Option<(AccountRecord, CredentialBlob)>,
}

impl Poller<'_> {
    fn poll<T: UsageTransport + ?Sized>(
        &self,
        transport: &T,
    ) -> Result<Vec<PollOutcome>, PollError> {
        let now = (self.clock)();
        let Some(sequence) = self.store.read_sequence()? else {
            return Ok(Vec::new());
        };
        let live_owner = live_owner(self.claude, &sequence);
        let (mut results, candidates) = self.gather(&sequence, live_owner, now);
        let identities = candidates
            .iter()
            .map(|(number, candidate)| (*number, candidate.identity.clone()))
            .collect();
        let fingerprints: Option<BTreeMap<u32, StoredFingerprints>> = self.refresh.map(|_| {
            let mut fingerprints = BTreeMap::new();
            for (number, candidate) in &candidates {
                if let Some(read) = candidate.fingerprint.clone() {
                    let backup = candidate.backup_fingerprint.clone();
                    fingerprints.insert(*number, StoredFingerprints { read, backup });
                }
            }
            fingerprints
        });
        let started = secs_from_timestamp(now);
        let pass = Reserve {
            scope: self.scope,
            fingerprints: fingerprints.as_ref(),
            now: started,
        };
        let claims = UsageTable::reserve_with(self.store.root(), &identities, pass)?;
        // A balance read is optional work in a pass whose usage readings are not: past this point it waits a pass.
        let cutoff = started + PREPAID_CUTOFF_S;
        let mut attempts = BTreeMap::new();
        let mut persisted = BTreeMap::new();
        let mut prepaid = BTreeMap::new();
        for (number, candidate) in &candidates {
            if claims.contains(*number) {
                let probe = PrepaidProbe::new(
                    &candidate.identity.org_uuid,
                    claims.prepaid_due.contains(number),
                    cutoff,
                    self.clock,
                );
                let (attempt, written) = self.attempt(transport, *number, candidate, &probe);
                if let Some(read) = probe.into_read() {
                    prepaid.insert(*number, read);
                }
                persisted.insert(*number, written);
                attempts.insert(*number, attempt);
            } else if claims.dead.contains(number) {
                results.insert(*number, PollResult::AuthDead);
            } else {
                results.insert(*number, self.unclaimed(*number, candidate, now));
            }
        }
        // The cadence follows the logged-in account only: with none, no account is planned as active.
        let context = PlanContext::new(
            live_owner,
            self.settings.threshold,
            self.settings.models.clone(),
        );
        let recorded = attempts
            .iter()
            .map(|(number, attempt)| (*number, attempt.recorded()))
            .collect();
        let accepted = UsageTable::record_rows(
            self.store.root(),
            &recorded,
            &prepaid,
            &claims,
            &context,
            self.clock,
        )?;
        drop(recorded);
        for (number, attempt) in attempts {
            results.insert(number, attempt.into_result(accepted.contains(&number)));
        }
        Ok(sequence
            .numbers()
            .iter()
            .filter_map(|number| {
                let written = persisted.get(number).copied().unwrap_or_default();
                results.remove(number).map(|result| PollOutcome {
                    number: *number,
                    result,
                    primary_key_cleared: written.primary_key_cleared,
                    backup_written: written.backup_written,
                    live_temp_left: written.live_temp_left,
                })
            })
            .collect())
    }

    /// What an account the reserve left unclaimed, and no strike keeps out, shows: [`PollResult::NotDue`], or
    /// [`PollResult::TokenExpired`] for the account Claude Code is logged in as when its live access token has expired
    /// at `now` and its row is not held (`heldUntil`) for another machine's reading. The expired state is overlaid
    /// so the engine idle-holds instead of counting the gap toward a failover.
    fn unclaimed(&self, number: u32, candidate: &Candidate, now: jiff::Timestamp) -> PollResult {
        let expired = candidate.live.is_some()
            && candidate
                .credentials
                .as_ref()
                .is_some_and(|credentials| credentials.is_expired(now));
        if !expired {
            return PollResult::NotDue;
        }
        let identities = BTreeMap::from([(number, candidate.identity.clone())]);
        let held = UsageTable::entries(self.store.root(), &identities)
            .get(&number)
            .is_some_and(|entry| entry.held(now));
        if held {
            PollResult::NotDue
        } else {
            PollResult::TokenExpired
        }
    }

    /// Each account's credentials when it may be polled, or what keeps it out of the poll.
    fn gather(
        &self,
        sequence: &Sequence,
        live_owner: Option<u32>,
        now: jiff::Timestamp,
    ) -> (BTreeMap<u32, PollResult>, BTreeMap<u32, Candidate>) {
        let mut results = BTreeMap::new();
        let mut candidates = BTreeMap::new();
        for &number in sequence.numbers() {
            let Some(account) = sequence.switchable_account(number) else {
                continue;
            };
            let logged_in = live_owner == Some(number);
            let active = logged_in || sequence.active() == Some(number);
            match self.candidate(&account, logged_in, active, now) {
                Ok(candidate) => {
                    candidates.insert(number, candidate);
                }
                Err(result) => {
                    results.insert(number, result);
                }
            }
        }
        (results, candidates)
    }

    /// The account's credentials when it may be polled at `now`: the live file first for the account Claude Code is
    /// logged in as, else the backup copy. An expired token is refused unless a refreshing poll may refresh it.
    fn candidate(
        &self,
        account: &AccountRecord,
        logged_in: bool,
        active: bool,
        now: jiff::Timestamp,
    ) -> Result<Candidate, PollResult> {
        if account.disabled {
            return Err(PollResult::Disabled);
        }
        if logged_in
            && let Some(blob) = live_blob(self.claude)
            && let Some(credentials) = blob.oauth()
        {
            return self.live_candidate(account, blob, credentials, now);
        }
        self.backup_candidate(account, Identity::of(account), active, now)
    }

    /// The logged-in account with the live credential the poll read. A read-only poll refuses an expired token; a
    /// refreshing poll hands it to [`active::fetch`], which refreshes it under Claude Code's locks, and reads the
    /// backup's fingerprint once, outside the usage lock, since that refresh may post the backup and strike its
    /// generation.
    fn live_candidate(
        &self,
        account: &AccountRecord,
        blob: CredentialBlob,
        credentials: OauthCredentials,
        now: jiff::Timestamp,
    ) -> Result<Candidate, PollResult> {
        let identity = Identity::of(account);
        if self.refresh.is_none() {
            if credentials.is_expired(now) {
                return Err(PollResult::TokenExpired);
            }
            return Ok(Candidate {
                identity,
                credentials: Some(credentials),
                fingerprint: None,
                backup_fingerprint: None,
                refreshable: None,
                live: None,
            });
        }
        let backup_fingerprint = match self.store.read_credentials(account.number) {
            Ok(Some(backup)) if has_refresh_token(&backup) => credential_fingerprint(&backup)
                .map_or(BackupFingerprint::Missing, BackupFingerprint::Fp),
            Ok(_) => BackupFingerprint::Missing,
            Err(_) => BackupFingerprint::Unreadable,
        };
        Ok(Candidate {
            identity,
            credentials: Some(credentials),
            fingerprint: credential_fingerprint(&blob),
            backup_fingerprint: Some(backup_fingerprint),
            refreshable: None,
            live: Some((account.clone(), blob)),
        })
    }

    /// The account's backup copy as a candidate. A token that needs a refresh (expired, or empty) is refused unless a
    /// refreshing poll may refresh it.
    fn backup_candidate(
        &self,
        account: &AccountRecord,
        identity: Identity,
        active: bool,
        now: jiff::Timestamp,
    ) -> Result<Candidate, PollResult> {
        let blob = backup_blob(self.store, account)?;
        let credentials = blob.oauth();
        let refresh = self.refresh.filter(|_| !active && has_refresh_token(&blob));
        if credentials
            .as_ref()
            .is_none_or(|credentials| credentials.is_expired(now))
        {
            check_refreshable(refresh, credentials.is_some())?;
        }
        let fingerprint = self.refresh.and_then(|_| credential_fingerprint(&blob));
        Ok(Candidate {
            identity,
            credentials,
            fingerprint,
            backup_fingerprint: None,
            refreshable: refresh.map(|_| blob),
            live: None,
        })
    }

    /// Fetches one claimed account, refreshing it first or after a 401 when it may be refreshed, and reads its prepaid
    /// balance beside the fetch that brings a reading when `prepaid` is due. Also returns, for the logged-in account,
    /// what its refresh wrote.
    fn attempt<T: UsageTransport + ?Sized>(
        &self,
        transport: &T,
        number: u32,
        candidate: &Candidate,
        prepaid: &PrepaidProbe<'_>,
    ) -> (Attempt, Persisted) {
        if let (Some((account, live)), Some(context)) = (&candidate.live, self.refresh) {
            return active::fetch(transport, context, account, live, prepaid);
        }
        let attempt = match (&candidate.refreshable, self.refresh, &candidate.credentials) {
            (Some(snapshot), Some(context), stored) => {
                let grant = Grant { number, snapshot };
                refreshing_fetch(transport, context, grant, stored.as_ref(), prepaid)
            }
            (_, _, Some(credentials)) => Attempt::Fetch(fetch_reading(
                transport,
                credentials.access_token(),
                prepaid,
            )),
            // Only a refreshable candidate lacks credentials, so this never runs.
            (_, _, None) => Attempt::Release(PollResult::NoCredentials),
        };
        (attempt, Persisted::default())
    }
}

/// What the refresh of the logged-in account wrote, reported on its [`PollOutcome`] under the same names.
#[derive(Clone, Copy, Default)]
struct Persisted {
    primary_key_cleared: Option<bool>,
    backup_written: Option<bool>,
    live_temp_left: Option<bool>,
}

/// The stored account Claude Code is logged in as, found by the identity `~/.claude.json` names.
fn live_owner(claude: &ClaudePaths, sequence: &Sequence) -> Option<u32> {
    let identity = live_identity(claude)?;
    sequence.numbers().iter().copied().find(|number| {
        sequence
            .account(*number)
            .is_some_and(|account| Identity::of(&account) == identity)
    })
}

/// One account's grant as the gate takes it: the account, and the credential the caller holds.
#[derive(Clone, Copy)]
struct Grant<'g> {
    number: u32,
    snapshot: &'g CredentialBlob,
}

/// What one claimed account's fetch came to, and how it is recorded.
enum Attempt {
    /// The usage endpoint was asked.
    Fetch(FetchResult),
    /// The refresh was refused with no verdict on the token: recorded as a failure under `kind`.
    Refused {
        kind: &'static str,
        result: PollResult,
    },
    /// The refresh lineage is dead: recorded as a failure with a strike on `struck_fp`.
    Dead {
        kind: &'static str,
        struck_fp: Option<String>,
    },
    /// Nothing to record; the lease is released.
    Release(PollResult),
}

impl Attempt {
    fn recorded(&self) -> Recorded<'_> {
        match self {
            Self::Fetch(result) => Recorded::Fetch(result),
            Self::Refused { kind, .. } => Recorded::Refused { kind },
            Self::Dead { kind, struck_fp } => Recorded::Dead {
                kind,
                struck_fp: struck_fp.as_deref(),
            },
            Self::Release(_) => Recorded::Release,
        }
    }

    fn into_result(self, accepted: bool) -> PollResult {
        match self {
            Self::Release(result) => result,
            _ if !accepted => PollResult::Fenced,
            Self::Fetch(outcome) => recorded_result(outcome, true),
            Self::Refused { result, .. } => result,
            Self::Dead { .. } => PollResult::AuthDead,
        }
    }
}

/// An inactive account's fetch with refresh: a token that is expired, or empty
/// (`stored` is `None`), is refreshed first, and a 401 refreshes once and retries once. With no access token to send
/// after the refresh (it failed for now, or its successor carries none), the attempt is recorded as `refresh-failed`.
fn refreshing_fetch<T: UsageTransport + ?Sized>(
    transport: &T,
    context: &RefreshContext<'_>,
    grant: Grant<'_>,
    stored: Option<&OauthCredentials>,
    prepaid: &PrepaidProbe<'_>,
) -> Attempt {
    let mut refreshed = None;
    if stored.is_none_or(|stored| stored.is_expired((context.clock)())) {
        match refresh_first(context, grant) {
            Ok(successor) => refreshed = successor,
            Err(stop) => return stop,
        }
    }
    let working = refreshed.as_ref().unwrap_or(grant.snapshot);
    let parsed = refreshed.as_ref().and_then(CredentialBlob::oauth);
    let Some(credentials) = (if refreshed.is_some() {
        parsed.as_ref()
    } else {
        stored
    }) else {
        return Attempt::Refused {
            kind: "refresh-failed",
            result: PollResult::TokenExpired,
        };
    };
    match fetch_reading(transport, credentials.access_token(), prepaid) {
        Err(error @ FetchError::Http { status: 401, .. })
            if credentials.refresh_token().is_some() =>
        {
            retry_after_401(
                transport,
                context,
                Grant {
                    snapshot: working,
                    ..grant
                },
                error,
                prepaid,
            )
        }
        result => Attempt::Fetch(result),
    }
}

/// The refresh before the fetch: the successor, `None` for a transient failure (whose 401 retries the refresh), or the
/// attempt a refusal comes to.
fn refresh_first(
    context: &RefreshContext<'_>,
    grant: Grant<'_>,
) -> Result<Option<CredentialBlob>, Attempt> {
    let mut outcome = consume_backup_grant(context, grant.number, grant.snapshot);
    if let Some(successor) = outcome.credentials.take() {
        return Ok(Some(successor));
    }
    refusal(&outcome, grant.snapshot).map_or(Ok(None), Err)
}

/// The one refresh and one retry after a 401.
fn retry_after_401<T: UsageTransport + ?Sized>(
    transport: &T,
    context: &RefreshContext<'_>,
    grant: Grant<'_>,
    first: FetchError,
    prepaid: &PrepaidProbe<'_>,
) -> Attempt {
    let refresh_failed = || Attempt::Refused {
        kind: "refresh-failed",
        result: PollResult::Failed(first),
    };
    let outcome = consume_backup_grant(context, grant.number, grant.snapshot);
    let Some(blob) = &outcome.credentials else {
        // A refresh that never began says nothing about the token: the 401 stands, with its backoff.
        if matches!(
            outcome.error,
            Some(GrantError::Draining | GrantError::Active | GrantError::StoreUnwritable)
        ) {
            return Attempt::Fetch(Err(first));
        }
        return refusal(&outcome, grant.snapshot).unwrap_or_else(refresh_failed);
    };
    let Some(credentials) = blob.oauth() else {
        return refresh_failed();
    };
    Attempt::Fetch(fetch_reading(
        transport,
        credentials.access_token(),
        prepaid,
    ))
}

/// Whether an account whose backup token needs a refresh may go on to the reserve: not without a refresh to run
/// ([`PollResult::TokenExpired`], or [`PollResult::NoCredentials`] when it has no access token at all).
fn check_refreshable(
    refresh: Option<&RefreshContext<'_>>,
    has_token: bool,
) -> Result<(), PollResult> {
    match (refresh, has_token) {
        (Some(_), _) => Ok(()),
        (None, true) => Err(PollResult::TokenExpired),
        (None, false) => Err(PollResult::NoCredentials),
    }
}

/// What a gate outcome without credentials comes to, `None` for a transient failure.
fn refusal(outcome: &GrantOutcome, working: &CredentialBlob) -> Option<Attempt> {
    let error = outcome.error?;
    match error {
        GrantError::InvalidGrant | GrantError::NoRefreshToken => Some(Attempt::Dead {
            kind: error.kind(),
            struck_fp: outcome
                .consumed_fp
                .clone()
                .or_else(|| credential_fingerprint(working)),
        }),
        GrantError::Draining | GrantError::Active | GrantError::StoreUnwritable => {
            grant_result(error).map(Attempt::Release)
        }
        _ => grant_result(error).map(|result| Attempt::Refused {
            kind: error.kind(),
            result,
        }),
    }
}

/// The poll result a gate refusal shows, `None` for those that show another way.
fn grant_result(error: GrantError) -> Option<PollResult> {
    match error {
        GrantError::Draining => Some(PollResult::Draining),
        // The account became the active one after the poll gathered it: its expired backup token stays unsent.
        GrantError::Active => Some(PollResult::TokenExpired),
        // The store takes no write, so nothing was spent: put off as the logged-in account's pre-spend check puts it off.
        GrantError::StoreUnwritable => Some(PollResult::Deferred(DeferReason::StoreUnwritable)),
        GrantError::StoreUnmirrored => Some(PollResult::StoreUnmirrored),
        GrantError::ConsumeBusy => Some(PollResult::ConsumeBusy),
        GrantError::StashUnreadable => Some(PollResult::StashUnreadable),
        GrantError::InvalidClient => Some(PollResult::InvalidClient),
        GrantError::Transient | GrantError::NoRefreshToken | GrantError::InvalidGrant => None,
    }
}

/// The account Claude Code is logged in as now: `oauthAccount`'s `emailAddress` and `organizationUuid` (missing or not a
/// string counting as none) in `~/.claude.json`, read without a lock and never written.
/// `None` when the file is missing, unreadable or not JSON, or names no email.
/// Only `oauthAccount` is parsed; every other key of the file is skipped unread.
///
/// It holds no secret (only `oauthAccount`'s identity fields), so the parsed tree is dropped without scrubbing.
pub(crate) fn live_identity(claude: &ClaudePaths) -> Option<Identity> {
    #[derive(serde::Deserialize)]
    struct GlobalConfig {
        #[serde(rename = "oauthAccount", default)]
        oauth_account: Option<Value>,
    }
    let bytes = store::read_optional(claude.global_config())
        .ok()
        .flatten()?;
    let config: GlobalConfig = serde_json::from_slice(&bytes).ok()?;
    identity_in(&config.oauth_account?)
}

pub(crate) use crate::claude_files::identity_in;

/// Claude Code's live credential file, read without a lock. `None` when
/// the file is missing, cannot be read or is not UTF-8; one that holds no OAuth access token is also passed over by the
/// caller, which then falls back to the backup copy.
pub(crate) fn live_blob(claude: &ClaudePaths) -> Option<CredentialBlob> {
    let bytes = store::read_optional(&claude.credentials_file())
        .ok()
        .flatten()?;
    let text = std::str::from_utf8(&bytes).ok()?;
    Some(CredentialBlob::new(text.to_owned()))
}

/// Whether `token` is the live credential's access token and the profile oracle named that credential's lineage as
/// another login's than `record`'s: the remembered `false` verdict the active poll answers
/// [`PollResult::ForeignCredential`] on. Only the live token can be foreign; a live file that cannot be read holds no
/// lineage to judge.
pub(crate) fn foreign_live_token(
    context: &RefreshContext<'_>,
    record: &AccountRecord,
    token: &AccessToken,
) -> bool {
    let Some(blob) = live_blob(context.claude) else {
        return false;
    };
    if blob
        .oauth()
        .is_none_or(|oauth| oauth.access_token().expose() != token.expose())
    {
        return false;
    }
    let key = LineageKey::new(
        &record.email,
        record,
        &credential_fingerprint(&blob).unwrap_or_default(),
    );
    context.verdicts.get(&key) == Some(false)
}

/// The account's backup credentials in the store.
fn backup_blob(store: &Store, account: &AccountRecord) -> Result<CredentialBlob, PollResult> {
    match store.read_credentials(account.number) {
        Ok(Some(blob)) => Ok(blob),
        Ok(None) => Err(PollResult::NoCredentials),
        Err(error) => Err(PollResult::CredentialsUnreadable(error.kind())),
    }
}

fn recorded_result(outcome: FetchResult, accepted: bool) -> PollResult {
    if !accepted {
        return PollResult::Fenced;
    }
    match outcome {
        Ok(reading) => {
            let (five_hour, seven_day) = reading.as_ref().map_or((None, None), account_windows);
            PollResult::Fetched {
                five_hour,
                seven_day,
            }
        }
        Err(error) => PollResult::Failed(error),
    }
}

/// `cache/usage.json` for a read-modify-write: a missing file, or bytes that are not a JSON object, start an empty
/// table of [`USAGE_SCHEMA_VERSION`], so the next write heals it; any other object without that version is refused,
/// never overwritten.
fn load_document(root: &AccountsRoot) -> Result<Map<String, Value>, PollError> {
    match read_document(root) {
        CacheDocument::Rows(document) => Ok(document),
        CacheDocument::Missing | CacheDocument::Corrupt => {
            let mut document = Map::new();
            document.insert(
                "schemaVersion".to_owned(),
                Value::from(USAGE_SCHEMA_VERSION),
            );
            document.insert("accounts".to_owned(), Value::Object(Map::new()));
            Ok(document)
        }
        CacheDocument::Unsupported => Err(PollError::UnsupportedCache),
        CacheDocument::Unreadable(error) => Err(PollError::CacheUnreadable(error)),
    }
}

/// Takes the document's `accounts` object out for editing; one that is missing or not an object is empty.
/// [`put_accounts`] puts it back in its place.
fn take_accounts(document: &mut Map<String, Value>) -> Map<String, Value> {
    match document.get_mut("accounts") {
        Some(Value::Object(accounts)) => std::mem::take(accounts),
        Some(_) | None => Map::new(),
    }
}

/// Puts the edited `accounts` object back, at the position the key already holds.
fn put_accounts(document: &mut Map<String, Value>, accounts: Map<String, Value>) {
    document.insert("accounts".to_owned(), Value::Object(accounts));
}

/// Writes the document to `cache/usage.json` through a temp file replaced onto it, making `cache/` owner-only when it
/// is missing; nothing is written when `accounts.json` refuses writes ([`PollError::StoreRefused`])
/// or a folder on the way is a link ([`AccountsRoot::check_folders`]). Every caller holds `cache/.usage.lock`.
fn write_document(root: &AccountsRoot, document: &Map<String, Value>) -> Result<(), PollError> {
    if let Some(kind) = store::version_refusal(root) {
        tracing::warn!(
            ?kind,
            "the account store refuses writes; cache/usage.json is left untouched"
        );
        return Err(PollError::StoreRefused(kind));
    }
    let path = root.usage_json();
    root.check_folders(&path).map_err(PollError::CacheWrite)?;
    crate::guard::create_folder(root.root(), &root.cache_dir())
        .map_err(|error| PollError::CacheWrite(io::Error::other(error)))?;
    store::write_json_atomic(&path, document).map_err(PollError::CacheWrite)
}

/// Clears account `number`'s refresh failure in `cache/usage.json` under `cache/.usage.lock` when its `lastError` is
/// `stash-unreadable`: the failure count, `lastError` and `backoffUntil`, every other field kept. Answers whether the
/// row changed; a missing table or row changes nothing.
///
/// # Errors
///
/// [`PollError::Lock`], [`PollError::UnsupportedCache`], [`PollError::CacheUnreadable`], [`PollError::CacheWrite`],
/// [`PollError::StoreRefused`].
pub(crate) fn clear_stash_failure(root: &AccountsRoot, number: u32) -> Result<bool, PollError> {
    root.check_folders(&root.usage_lock())
        .map_err(PollError::CacheUnreadable)?;
    let _lock =
        acquire_in_root(root, &root.usage_lock(), USAGE_LOCK_TIMEOUT).map_err(PollError::Lock)?;
    let mut document = load_document(root)?;
    let mut accounts = take_accounts(&mut document);
    let Some(row) = accounts
        .get_mut(&number.to_string())
        .and_then(Value::as_object_mut)
    else {
        return Ok(false);
    };
    if row.get("lastError").and_then(Value::as_str) != Some(GrantError::StashUnreadable.kind()) {
        return Ok(false);
    }
    row.insert("consecutiveFailures".to_owned(), Value::from(0));
    row.insert("lastError".to_owned(), Value::Null);
    row.insert("backoffUntil".to_owned(), Value::Null);
    put_accounts(&mut document, accounts);
    write_document(root, &document)?;
    Ok(true)
}

/// A lease's fencing id: 32 random hex digits.
fn new_claim_id() -> String {
    format!("{:016x}{:016x}", random_u64(), random_u64())
}

/// A number in `0..=1`, for the plan's jitter.
fn unit_random() -> f64 {
    let high = u32::try_from(random_u64() >> 32).unwrap_or(u32::MAX);
    f64::from(high) / f64::from(u32::MAX)
}

/// 64 random bits from the standard library's randomly keyed hasher, mixed with the time and the process id.
fn random_u64() -> u64 {
    let mut hasher = RandomState::new().build_hasher();
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    hasher.write_u128(nanos);
    hasher.write_u32(std::process::id());
    hasher.finish()
}
