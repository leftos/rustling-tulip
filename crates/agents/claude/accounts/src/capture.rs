//! Capturing Claude Code's live login into the account store ("Add this login").
//!
//! [`capture_live_login`] reads the live credential file and `~/.claude.json` with no lock, refuses a login that is no
//! subscription login to keep (none, an API key, no refresh token, an email no file name can hold), and asks the profile
//! oracle, still holding no lock, to confirm whose login it is; a login it cannot confirm is refused, and nothing is
//! refreshed to confirm it. It then takes the matched account's consume lock, the account lock, Claude Code's
//! credential locks and its config lock, in that order, and the stop drain's guard once they are held, and reads both
//! files again: a login that changed meanwhile is refused, and only the locked read is written.
//!
//! A login no account has is added as the next account number, carrying the uuid the oracle resolved when the login has
//! none, and made the active account. A login an account has is written into it only when the live copy is later than
//! the stored one, so a stored copy is never rolled back; a stored credential that does not open is left as it is, and
//! the account's uuid is filled from the login's whatever the verdict. No outcome, error or log line carries a token or
//! a path.

use std::fmt;

use serde_json::Value;

use crate::claude_files::{ClaudeFileError, LiveLogin, read_live_login, read_live_login_locked};
use crate::locks::{
    AccountLock, AccountLockGuard, ClaudeCodeLocks, ClaudeCredentialsGuard, ConsumeLock,
    ConsumeLockGuard, DirLockGuard, LockError,
};
use crate::paths::Email;
use crate::refresh::oracle::{Verdict, resolve, resolved_matches_identity};
use crate::refresh::{
    OauthShape, RefreshContext, credential_fingerprint, has_refresh_token, newer_generation,
    oauth_expiry, stash,
};
use crate::secret::CredentialBlob;
use crate::store::{
    AccountKind, AccountNumber, NewAccount, Retention, Sequence, StoreError, StoreErrorKind,
};
use crate::switch::backs_record;
use crate::usage::Identity;

/// What a capture did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureKind {
    /// The login was added as a new account and made the active account.
    Added,
    /// The live copy was later than the stored one and was written into the account, which was made active.
    Refreshed,
    /// The stored copy is newer than the live one, or another credential whose age cannot be told against it, and was
    /// kept; nothing was written but the active account, set to this one, and the uuid when the record has none.
    Kept,
    /// The stored copy is the live one, or its lineage at the same expiry; nothing was written but the active account,
    /// set to this one, and the uuid when the record has none.
    Unchanged,
}

impl CaptureKind {
    /// The kind's name, as the server's reply carries it: `added`, `refreshed`, `kept` or `unchanged`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Refreshed => "refreshed",
            Self::Kept => "kept",
            Self::Unchanged => "unchanged",
        }
    }
}

/// A capture's result: what it did, and the account it concerns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureOutcome {
    pub kind: CaptureKind,
    pub number: u32,
    pub email: Email,
}

/// Why a capture did not happen. Its text names no token, file contents or path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CaptureError {
    /// There is no live login: no credential file, one with no `claudeAiOauth`, or a `~/.claude.json` that names no
    /// login. Nothing was locked or written.
    #[error("there is no Claude Code login on this PC to add")]
    NoLogin,
    /// The live credential file or `~/.claude.json` exists but could not be read, is a link, or is not text or JSON.
    /// Nothing was written.
    #[error(
        "Claude Code's live credential file or global config could not be read; nothing was written"
    )]
    LiveUnreadable,
    /// `~/.claude.json` holds an API key: the live login is no subscription login. Nothing was locked or written.
    #[error("the live login is an API key, not a Claude subscription login")]
    ApiKeyLogin,
    /// The live credential holds no refresh token, so the account could never be refreshed. Nothing was locked or
    /// written.
    #[error("the live login holds no refresh token")]
    NoRefreshToken,
    /// The live login's email cannot be part of a file name. Nothing was locked or written.
    #[error("the live login's email cannot be part of a file name")]
    UnsafeEmail,
    /// The profile endpoint did not confirm the live login's identity: it failed, refused the access token, or named
    /// another account. Nothing was locked or written.
    #[error("the profile endpoint did not confirm the live login; nothing was written")]
    Unverified,
    /// The live login read under the locks is not the one read and confirmed before them, or now matches another
    /// account. Nothing was written.
    #[error("the live login changed while it was being read; nothing was written")]
    LoginChanged,
    /// An API-key account with the live login's email and organization is stored: an OAuth login never matches one.
    /// Nothing was written.
    #[error("the live login is already stored as an API-key account; nothing was written")]
    StoredAsApiKey,
    /// Account `number` has the live login's email and organization under another account uuid: it was left as it is,
    /// and nothing was written.
    #[error(
        "account {number} has the live login's email and organization under another account id; it was left as it is"
    )]
    AnotherAccountId { number: u32 },
    /// The store found the login already stored when it came to add it: another writer added it meanwhile. Nothing was
    /// written.
    #[error("the live login is already stored; nothing was written")]
    AccountExists,
    /// The matched account's stored credential exists and does not open (unreadable, sealed for another account, or
    /// rolled back): it was left as it is, and nothing was written.
    #[error("account {number}'s stored credential cannot be opened, so it was left as it is")]
    StoredUnopenable { number: u32 },
    /// The account store has no account number left to give. Nothing was written.
    #[error("the account store has no account number left to give")]
    NumbersExhausted,
    /// The stop drain has begun. Nothing was written.
    #[error("the server is shutting down; nothing was written")]
    Draining,
    /// A lock stayed held past its wait, or one of Claude Code's locks was lost while held. Nothing was written.
    #[error(
        "a lock on the account store or on Claude Code's files was held by another process; nothing was written"
    )]
    LocksBusy,
    /// A lock could not be taken for another reason. Nothing was written.
    #[error(
        "a lock on the account store or on Claude Code's files could not be taken; nothing was written"
    )]
    LockFailed,
    /// The account store refused a read or a write; `account` is the account it concerns, if one.
    #[error("the account store refused the capture: {kind}")]
    Store {
        kind: StoreErrorKind,
        account: Option<u32>,
    },
}

/// Captures Claude Code's live login into the account store, as "Add this login": synchronous, with the network call to
/// the profile endpoint before any lock and lock waits of up to 10 s for the store's locks and the context's
/// [`RefreshContext::claude_locks`] for Claude Code's, so an async caller runs it on a blocking thread. It reads the
/// context's store, Claude Code files, profile endpoint, drain and clock, and nothing else. Every lock is released
/// before it returns.
///
/// # Errors
///
/// A [`CaptureError`] saying why: the refusals of the unlocked read ([`CaptureError::NoLogin`],
/// [`CaptureError::ApiKeyLogin`], [`CaptureError::NoRefreshToken`], [`CaptureError::UnsafeEmail`],
/// [`CaptureError::LiveUnreadable`]), [`CaptureError::Unverified`] when the oracle does not confirm the login,
/// [`CaptureError::StoredAsApiKey`] or [`CaptureError::AnotherAccountId`] for a stored account with its email
/// and organization that is not it, the uuid compared being the login's own or the one the profile endpoint resolved
/// when it has none, [`CaptureError::AccountExists`] when another writer added it meanwhile, the
/// lock errors and [`CaptureError::Draining`], [`CaptureError::LoginChanged`] when the locked read differs,
/// [`CaptureError::StoredUnopenable`] when a matched account's stored credential does not open,
/// and [`CaptureError::NumbersExhausted`] or [`CaptureError::Store`] when the store refuses.
///
/// A login whose `oauthAccount` has no `accountUuid` keeps the uuid the profile endpoint resolved for it, on the record
/// it adds or on the matched one that has none.
pub fn capture_live_login(context: &RefreshContext<'_>) -> Result<CaptureOutcome, CaptureError> {
    let first =
        Login::of(read_live_login(context.claude).map_err(|error| live_read_error(&error))?)?;
    let resolved_uuid = verify(context, &first)?;
    let sequence = context
        .store
        .read_sequence()
        .map_err(|error| store_error(&error))?;
    let candidate = matching_account(sequence.as_ref(), &first, resolved_uuid.as_deref())?;
    let locks = CaptureLocks::acquire(context, candidate)?;
    // Taken only now, past every lock wait, so the server's bounded drain never waits one out.
    let Ok(_work) = context.drain.begin() else {
        return Err(CaptureError::Draining);
    };
    let mut login = locked_login(context, &locks, &first, candidate, resolved_uuid.as_deref())?;
    // After the locked read's comparison with `first`, which includes the uuid: `first` is the login as read.
    login.record_uuid(resolved_uuid);
    match candidate {
        None => add_new(context, &locks.lock, &login),
        Some(number) => refresh_matched(context, &locks.lock, &login, number),
    }
}

/// The live login, checked as one to keep: its whole credential text, `oauthAccount`, and the identity that names.
struct Login {
    credential: CredentialBlob,
    oauth_account: Value,
    email: Email,
    org_uuid: String,
    uuid: String,
    org_name: String,
}

impl fmt::Debug for Login {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Login").finish_non_exhaustive()
    }
}

impl Login {
    /// `live` when it is a subscription login to keep: a credential with a `claudeAiOauth` object and a refresh token,
    /// no API key in `~/.claude.json`, and an `oauthAccount` naming an email a file name can hold.
    fn of(live: LiveLogin) -> Result<Self, CaptureError> {
        // First, so a login that is only an API key, with no OAuth credential, is named as one.
        if live.api_key {
            return Err(CaptureError::ApiKeyLogin);
        }
        let Some(credential) = live
            .credential
            .filter(|credential| OauthShape::of(credential).is_some())
        else {
            return Err(CaptureError::NoLogin);
        };
        if !has_refresh_token(&credential) {
            return Err(CaptureError::NoRefreshToken);
        }
        let Some(oauth_account) = live.oauth_account else {
            return Err(CaptureError::NoLogin);
        };
        // A missing or non-string field counts as none, as the switch's identity read takes it.
        let text = |key: &str| {
            oauth_account
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let email = text("emailAddress");
        if email.is_empty() {
            return Err(CaptureError::NoLogin);
        }
        Ok(Self {
            email: Email::parse(&email).map_err(|_| CaptureError::UnsafeEmail)?,
            org_uuid: text("organizationUuid"),
            uuid: text("accountUuid"),
            org_name: text("organizationName"),
            credential,
            oauth_account,
        })
    }

    fn identity(&self) -> Identity {
        Identity::new(self.email.as_str(), &self.org_uuid)
    }

    /// Records `resolved`, the uuid the profile endpoint resolved for this login, when the login has none: a new record
    /// carries it, and a matched one without it is backfilled from it.
    fn record_uuid(&mut self, resolved: Option<String>) {
        match resolved {
            Some(uuid) if self.uuid.is_empty() => self.uuid = uuid,
            _ => {}
        }
    }

    /// Whether `other` is the same login as this one: the same credential fingerprint, email, organization and account
    /// uuid.
    fn same_login(&self, other: &Self) -> bool {
        credential_fingerprint(&self.credential) == credential_fingerprint(&other.credential)
            && self.email == other.email
            && self.org_uuid == other.org_uuid
            && self.uuid == other.uuid
    }
}

/// Asks the profile oracle whose login `login`'s access token is, with no lock held: the uuid decides, the organization
/// corroborating, or, for a login with no uuid, the email and organization. `Some` carries the uuid the oracle resolved
/// for a login that has none ([`Verdict::SameWithoutUuid`]); a login whose own uuid decided is `None`. Anything short of
/// a confirmation is [`CaptureError::Unverified`]; the token is never refreshed to get one.
fn verify(context: &RefreshContext<'_>, login: &Login) -> Result<Option<String>, CaptureError> {
    let resolved = login
        .credential
        .oauth()
        .and_then(|oauth| context.profile.get_profile(oauth.access_token()).ok())
        .as_ref()
        .and_then(resolve);
    let Some(resolved) = resolved else {
        return unverified();
    };
    match resolved_matches_identity(
        login.email.as_str(),
        &login.org_uuid,
        &login.uuid,
        &resolved,
    ) {
        Verdict::Same => Ok(None),
        Verdict::SameWithoutUuid => Ok(Some(resolved.uuid)),
        Verdict::Foreign | Verdict::Unverifiable => unverified(),
    }
}

/// Logs that the live login is not added and answers [`CaptureError::Unverified`], as [`verify`] does for a profile answer
/// that is none or names another account.
fn unverified() -> Result<Option<String>, CaptureError> {
    tracing::info!("the profile endpoint did not confirm the live login; it is not added");
    Err(CaptureError::Unverified)
}

/// The account `login` is, among every record in `sequence` (API-key ones included), by email and organization:
/// `None` when no account has them. One that has them but is an API-key account is [`CaptureError::StoredAsApiKey`],
/// and one whose account uuid is neither the one `login`'s `oauthAccount` names nor, when it names none, the one the
/// profile endpoint resolved for it, `resolved`, is [`CaptureError::AnotherAccountId`].
fn matching_account(
    sequence: Option<&Sequence>,
    login: &Login,
    resolved: Option<&str>,
) -> Result<Option<u32>, CaptureError> {
    let identity = login.identity();
    let Some(record) = sequence.and_then(|sequence| {
        sequence
            .every_record()
            .find(|record| Identity::of(record) == identity)
    }) else {
        return Ok(None);
    };
    if record.kind == AccountKind::ApiKey {
        return Err(CaptureError::StoredAsApiKey);
    }
    if !backs_record(record, &login.oauth_account) {
        return Err(CaptureError::AnotherAccountId {
            number: record.number,
        });
    }
    let recorded = record.uuid.trim();
    if resolved.is_some_and(|resolved| !recorded.is_empty() && recorded != resolved) {
        return Err(CaptureError::AnotherAccountId {
            number: record.number,
        });
    }
    Ok(Some(record.number))
}

/// Every lock a capture holds, taken in order: the matched account's consume lock (none for a new account), the
/// account lock, Claude Code's credential locks, then its config lock. The fields are declared in the reverse order, so
/// they are released last taken, first released.
struct CaptureLocks {
    config: DirLockGuard,
    claude: ClaudeCredentialsGuard,
    lock: AccountLockGuard,
    _consume: Option<ConsumeLockGuard>,
}

impl CaptureLocks {
    fn acquire(context: &RefreshContext<'_>, candidate: Option<u32>) -> Result<Self, CaptureError> {
        let root = context.store.root();
        let failed = |error: LockError| lock_error(&error);
        let consume = candidate
            .map(|number| ConsumeLock::acquire(root, number))
            .transpose()
            .map_err(failed)?;
        let lock = AccountLock::acquire(root).map_err(failed)?;
        let claude = ClaudeCodeLocks::credentials_with(context.claude, context.claude_locks)
            .map_err(failed)?;
        let config =
            ClaudeCodeLocks::config_with(context.claude, context.claude_locks).map_err(failed)?;
        Ok(Self {
            config,
            claude,
            lock,
            _consume: consume,
        })
    }
}

/// Under every lock: the store's write refusal, then the live login read again, which must be `first`, the login
/// confirmed before the locks, and match the same account, `candidate`, in `accounts.json` read now, comparing the uuid
/// the login names or, when it names none, the one the profile resolved, `resolved`.
fn locked_login(
    context: &RefreshContext<'_>,
    locks: &CaptureLocks,
    first: &Login,
    candidate: Option<u32>,
    resolved: Option<&str>,
) -> Result<Login, CaptureError> {
    if let Some(kind) = context.store.write_refusal() {
        return Err(CaptureError::Store {
            kind,
            account: candidate,
        });
    }
    let live = read_live_login_locked(context.claude, &locks.claude, &locks.config)
        .map_err(|error| live_read_error(&error))?;
    // The login passed every check before the locks, so a locked read that no longer does is a login that changed.
    let login = Login::of(live).map_err(|_| CaptureError::LoginChanged)?;
    if !login.same_login(first) {
        tracing::info!(
            "the live login changed between the unlocked and the locked read; it is not added"
        );
        return Err(CaptureError::LoginChanged);
    }
    let sequence = context
        .store
        .read_sequence()
        .map_err(|error| store_error(&error))?;
    if matching_account(sequence.as_ref(), &login, resolved)? != candidate {
        tracing::info!(
            "the live login matches another account under the locks than before them; it is not added"
        );
        return Err(CaptureError::LoginChanged);
    }
    Ok(login)
}

/// Adds `login` as the next account number, then makes it the active account.
fn add_new(
    context: &RefreshContext<'_>,
    lock: &AccountLockGuard,
    login: &Login,
) -> Result<CaptureOutcome, CaptureError> {
    let account = NewAccount {
        number: AccountNumber::Next,
        email: login.email.clone(),
        uuid: login.uuid.clone(),
        org_uuid: login.org_uuid.clone(),
        org_name: login.org_name.clone(),
        added: format!("{:.0}", (context.clock)()),
        alias: None,
        disabled: false,
        kind: AccountKind::Oauth,
        oauth_account: &login.oauth_account,
        credentials: &login.credential,
    };
    let numbers = context
        .store
        .add_accounts(lock, &[account])
        .map_err(|error| store_error(&error))?;
    // One account in, one number out: an empty answer would be the store's own fault.
    let Some(&number) = numbers.first() else {
        return Err(CaptureError::Store {
            kind: StoreErrorKind::AccountsWrite,
            account: None,
        });
    };
    context
        .store
        .set_active(lock, number)
        .map_err(|error| store_error(&error))?;
    tracing::info!(
        account = number,
        "the live login was added to the account store"
    );
    Ok(CaptureOutcome {
        kind: CaptureKind::Added,
        number,
        email: login.email.clone(),
    })
}

/// Writes `login` into account `number` when the live copy is later than the stored one, and only then; a stored
/// credential that does not open is left as it is. A stored copy kept or unchanged writes nothing but the active
/// account, which is set to `number` when it is not already, since `number` is the live login. The account's uuid is
/// filled from the login's when the record has none, whatever the verdict.
fn refresh_matched(
    context: &RefreshContext<'_>,
    lock: &AccountLockGuard,
    login: &Login,
    number: u32,
) -> Result<CaptureOutcome, CaptureError> {
    let stored = context.store.read_credentials(number).map_err(|error| {
        tracing::warn!(account = number, %error, "the account's stored credential does not open; the live login is not written over it");
        CaptureError::StoredUnopenable { number }
    })?;
    let kind = stored_verdict(stored.as_ref(), &login.credential);
    context
        .store
        .backfill_uuid(lock, number, &login.uuid, &login.identity())
        .map_err(|error| store_error(&error))?;
    if kind == CaptureKind::Refreshed {
        let old_fp = stored.as_ref().and_then(credential_fingerprint);
        write_matched(context, lock, login, number, old_fp.as_deref())?;
    } else {
        // Writes nothing when the account already is the active one.
        context
            .store
            .set_active(lock, number)
            .map_err(|error| store_error(&error))?;
    }
    tracing::info!(
        account = number,
        outcome = kind.as_str(),
        "the live login was captured into its account"
    );
    Ok(CaptureOutcome {
        kind,
        number,
        email: login.email.clone(),
    })
}

/// What a capture does with `live` for an account whose stored copy is `stored`, never rolling the stored copy back:
/// - no stored copy, or a live `expiresAt` strictly later than the stored one, is written ([`CaptureKind::Refreshed`]);
/// - the same bytes, or the same lineage at the same `expiresAt`, are [`CaptureKind::Unchanged`];
/// - a stored `expiresAt` strictly later, another credential at the same `expiresAt`, or another credential holding a
///   refresh token whose `expiresAt` is zero or missing (its age cannot be told) is [`CaptureKind::Kept`].
fn stored_verdict(stored: Option<&CredentialBlob>, live: &CredentialBlob) -> CaptureKind {
    let Some(stored) = stored else {
        return CaptureKind::Refreshed;
    };
    if stored.expose() == live.expose() {
        return CaptureKind::Unchanged;
    }
    if newer_generation(live, stored) {
        return CaptureKind::Refreshed;
    }
    let same_lineage = credential_fingerprint(stored) == credential_fingerprint(live);
    if !same_lineage || expiry(stored) > expiry(live) {
        CaptureKind::Kept
    } else {
        CaptureKind::Unchanged
    }
}

/// A credential's `expiresAt`, 0 when it has none or it is not a number.
fn expiry(blob: &CredentialBlob) -> f64 {
    oauth_expiry(blob).ok().flatten().unwrap_or(0.0)
}

/// The matched account's write, under every lock: its credential, its config backup and the active account;
/// `disabled`, `alias` and `maySpendCredits` are kept as they are. Its uuid is filled by [`refresh_matched`] whatever
/// the verdict. Once the credential is written, the stash rows that held up the replaced one, whose fingerprint was
/// `old_fp` (none for an account that stored none), are retired ([`stash::retire_superseded`]).
fn write_matched(
    context: &RefreshContext<'_>,
    lock: &AccountLockGuard,
    login: &Login,
    number: u32,
    old_fp: Option<&str>,
) -> Result<(), CaptureError> {
    let store = context.store;
    let failed = |error: StoreError| store_error(&error);
    if let Retention::Failed(kind) = store
        .write_credentials(lock, number, &login.credential)
        .map_err(failed)?
    {
        tracing::warn!(
            account = number,
            ?kind,
            "the account's replaced credential could not be kept as its .prev copy"
        );
    }
    if let Some(old_fp) = old_fp {
        let new_fp = credential_fingerprint(&login.credential);
        stash::retire_superseded(
            store.root(),
            number,
            old_fp,
            new_fp.as_deref(),
            (context.clock)(),
        );
    }
    store
        .write_account_config(lock, number, &login.oauth_account)
        .map_err(failed)?;
    store.set_active(lock, number).map_err(failed)?;
    Ok(())
}

fn live_read_error(error: &ClaudeFileError) -> CaptureError {
    match error {
        ClaudeFileError::WrongLock | ClaudeFileError::LockLost => CaptureError::LocksBusy,
        _ => CaptureError::LiveUnreadable,
    }
}

fn lock_error(error: &LockError) -> CaptureError {
    if matches!(error, LockError::Timeout) {
        CaptureError::LocksBusy
    } else {
        CaptureError::LockFailed
    }
}

fn store_error(error: &StoreError) -> CaptureError {
    match error.kind() {
        StoreErrorKind::NumbersExhausted => CaptureError::NumbersExhausted,
        StoreErrorKind::AccountExists => CaptureError::AccountExists,
        kind => CaptureError::Store {
            kind,
            account: error.account(),
        },
    }
}
