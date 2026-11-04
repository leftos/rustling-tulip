//! Freshening a switch target and the quarantine around it:
//! a target whose stored access token is
//! about to lapse has its grant spent through the consume gate before it is activated, the account the token endpoint
//! names is checked against the record, and an account whose credential is dead or belongs to someone else is held out
//! of rotation until its credential is replaced.

use serde_json::Value;

use crate::engine::{AutoswitchState, EngineError, QuarantineEntry, QuarantineRecord};
use crate::locks::AccountLock;
use crate::paths::Email;
use crate::refresh::{
    GrantError, RefreshContext, TokenAccount, consume_backup_grant, credential_fingerprint,
    oauth_expiry,
};
use crate::store::{AccountRecord, Sequence, Store};
use crate::switch::{SwitchErrorKind, log_identity_refusal, target_identity_check};
use crate::usage::Identity;

/// How long before its `expiresAt` a target's access token is refreshed before a switch:
/// twice Claude Code's own five-minute refresh buffer, so Claude Code does not refresh it again right after the switch.
pub const FRESHEN_BUFFER: jiff::SignedDuration = jiff::SignedDuration::from_mins(10);

/// The quarantine reason of an account whose credential authenticates as another account.
pub const IDENTITY_CONFLICT: &str = "identity-conflict";

/// The quarantine reason of an account whose config backup names another login than its record: the
/// account number is all that ties a record to its files, and they disagree.
pub const IDENTITY_MISMATCH: &str = "identity-mismatch";

/// What freshening a switch target came to ([`freshen_target`]), by status
/// ([`Freshened::kind`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Freshened {
    /// The target's credential outlives [`FRESHEN_BUFFER`], or was just freshened: it may be switched to.
    Ready,
    /// The freshened credential authenticates as another account: the target was quarantined and is not switchable.
    IdentityConflict,
    /// The target's config backup names another login than its record, found before its grant was spent: nothing was
    /// spent or written, and the caller quarantines it as [`IDENTITY_MISMATCH`].
    IdentityMismatch,
    /// No freshened credential: the gate's refusal or failure, [`GrantError::Transient`] for a stored credential that
    /// cannot be read and [`GrantError::NoRefreshToken`] for one with no OAuth block.
    Failed(GrantError),
}

impl Freshened {
    /// The status: `ok`, `identity-conflict`, `identity-mismatch`,
    /// `invalid_grant` (a dead lineage the caller quarantines), a systemic refusal's name (`store-unmirrored`, `store-unwritable`, `invalid_client`,
    /// `stash-unreadable`, `consume-busy`) or `transient`.
    #[must_use]
    pub fn kind(self) -> &'static str {
        match self {
            Self::Ready => "ok",
            Self::IdentityConflict => IDENTITY_CONFLICT,
            Self::IdentityMismatch => IDENTITY_MISMATCH,
            Self::Failed(GrantError::InvalidGrant | GrantError::NoRefreshToken) => "invalid_grant",
            Self::Failed(
                error @ (GrantError::StoreUnmirrored
                | GrantError::StoreUnwritable
                | GrantError::InvalidClient
                | GrantError::StashUnreadable
                | GrantError::ConsumeBusy),
            ) => error.kind(),
            Self::Failed(_) => "transient",
        }
    }
}

/// What the token endpoint's account said about an account's record ([`note_token_identity`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum IdentityCheck {
    /// The token named no account uuid, or the record's blank uuid was not backfilled: nothing is known either way.
    Unverified,
    /// The token's account is the record's.
    Matches,
    /// The record had no uuid and now has the token's.
    Backfilled,
    /// The token belongs to another organization or another account than the record's.
    Conflict,
}

/// Why a quarantine was released ([`release_recovered_quarantines`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReleaseReason {
    /// The account number is gone or now holds another email.
    AccountReplaced,
    /// The account's stored credential is no longer the one it was quarantined with.
    CredentialsReplaced,
    /// An [`IDENTITY_MISMATCH`] account's config backup names its record's login again.
    IdentityRestored,
}

impl ReleaseReason {
    /// The reason: `account-replaced`, `credentials-replaced` or `identity-restored`.
    #[must_use]
    pub fn kind(self) -> &'static str {
        match self {
            Self::AccountReplaced => "account-replaced",
            Self::CredentialsReplaced => "credentials-replaced",
            Self::IdentityRestored => "identity-restored",
        }
    }
}

/// One quarantine [`release_recovered_quarantines`] lifted, for the caller's event.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Released {
    /// The account number, the state file's key.
    pub number: String,
    /// The account's email: the record's now, or the entry's when the account was replaced (empty when it has none).
    pub email: String,
    /// Why it was lifted.
    pub reason: ReleaseReason,
}

/// Quarantines account `number` for `reason` at `now`: an entry in the state file bound
/// to the fingerprint of the account's stored credential (none when it is absent or cannot be read), so replacing the
/// credential releases it.
///
/// # Errors
///
/// As [`AutoswitchState::update`]: the state file's lock was not taken, or the file could not be read or written.
pub fn quarantine(
    store: &Store,
    number: u32,
    email: &Email,
    reason: &str,
    now: jiff::Timestamp,
) -> Result<(), EngineError> {
    let fingerprint = if let Ok(credentials) = store.read_credentials(number) {
        credentials.as_ref().and_then(credential_fingerprint)
    } else {
        tracing::warn!(
            account = number,
            "the quarantined account's credential could not be read; its quarantine records no fingerprint"
        );
        None
    };
    let entry = QuarantineEntry::new(
        email.as_str(),
        reason,
        now.strftime("%Y-%m-%dT%H:%M:%SZ").to_string(),
        fingerprint,
    );
    AutoswitchState::update(store.root(), |state| {
        state
            .quarantine
            .insert(number.to_string(), QuarantineRecord::Entry(entry));
    })?;
    Ok(())
}

/// Lifts every quarantine in `state` whose account was replaced since:
/// the account number is gone or holds another email, or its stored credential's fingerprint is no longer the
/// entry's (the user logged in again and re-captured it). An [`IDENTITY_MISMATCH`] entry is lifted on neither credential
/// rule but when the account's config backup names its record's login again ([`ReleaseReason::IdentityRestored`]),
/// and stays while it does not. An entry that is not an object, and an account whose
/// `accounts.json` or credential cannot be read, stay quarantined. Returns the state written, `state` itself when
/// nothing was lifted, and what was lifted.
///
/// # Errors
///
/// As [`AutoswitchState::update`], when something was lifted.
pub fn release_recovered_quarantines(
    store: &Store,
    state: AutoswitchState,
) -> Result<(AutoswitchState, Vec<Released>), EngineError> {
    if state.quarantine.is_empty() {
        return Ok((state, Vec::new()));
    }
    let Ok(sequence) = store.read_sequence() else {
        tracing::warn!("accounts.json could not be read; no quarantine is lifted this pass");
        return Ok((state, Vec::new()));
    };
    let released: Vec<Released> = state
        .quarantine
        .iter()
        .filter_map(|(number, record)| release_of(store, sequence.as_ref(), number, record))
        .collect();
    if released.is_empty() {
        return Ok((state, released));
    }
    let state = AutoswitchState::update(store.root(), |state| {
        for lifted in &released {
            state.quarantine.remove(&lifted.number);
        }
    })?;
    Ok((state, released))
}

/// Whether the quarantine of account `number` is lifted, and why.
fn release_of(
    store: &Store,
    sequence: Option<&Sequence>,
    number: &str,
    record: &QuarantineRecord,
) -> Option<Released> {
    let Quarantined {
        email,
        fingerprint,
        reason,
    } = quarantined_as(record)?;
    let account = number
        .parse::<u32>()
        .ok()
        .and_then(|number| sequence?.account(number))
        .filter(|account| Some(account.email.as_str()) == email);
    let Some(account) = account else {
        return Some(Released {
            number: number.to_owned(),
            email: email.unwrap_or_default().to_owned(),
            reason: ReleaseReason::AccountReplaced,
        });
    };
    if reason == Some(IDENTITY_MISMATCH) {
        return target_identity_check(store, account.number, &account)
            .is_ok()
            .then(|| Released {
                number: number.to_owned(),
                email: account.email.as_str().to_owned(),
                reason: ReleaseReason::IdentityRestored,
            });
    }
    let Ok(credentials) = store.read_credentials(account.number) else {
        tracing::warn!(
            account = account.number,
            "the quarantined account's credential could not be read; it stays quarantined"
        );
        return None;
    };
    let current = credentials.as_ref().and_then(credential_fingerprint);
    (current.as_deref() != fingerprint).then(|| Released {
        number: number.to_owned(),
        email: account.email.as_str().to_owned(),
        reason: ReleaseReason::CredentialsReplaced,
    })
}

/// What a quarantine entry was written with, each field `None` when a malformed entry lacks it.
struct Quarantined<'a> {
    email: Option<&'a str>,
    fingerprint: Option<&'a str>,
    reason: Option<&'a str>,
}

/// The email, fingerprint and reason an entry was quarantined with, read leniently from a malformed object
/// key by key; `None` for an entry that is not an object.
fn quarantined_as(record: &QuarantineRecord) -> Option<Quarantined<'_>> {
    match record {
        QuarantineRecord::Entry(entry) => Some(Quarantined {
            email: Some(entry.email.as_str()),
            fingerprint: entry.refresh_token_fingerprint.as_deref(),
            reason: Some(entry.reason.as_str()),
        }),
        QuarantineRecord::Malformed(Value::Object(map)) => Some(Quarantined {
            email: map.get("email").and_then(Value::as_str),
            fingerprint: map.get("refreshTokenFingerprint").and_then(Value::as_str),
            reason: map.get("reason").and_then(Value::as_str),
        }),
        QuarantineRecord::Malformed(_) => None,
    }
}

/// Checks the account the token endpoint named for a credential just refreshed against the account's record:
/// no uuid in the token says nothing; an organization that differs, when both
/// sides record one, is a conflict; a record with no uuid gets the token's (under the account lock, and only while the
/// record still holds its email and organization); otherwise a different uuid, both trimmed, is a conflict. A failed
/// backfill is logged and never fails the freshen. The caller must not hold the account lock, which the backfill takes.
#[must_use]
pub fn note_token_identity(
    store: &Store,
    account: &AccountRecord,
    token: Option<&TokenAccount>,
) -> IdentityCheck {
    let Some(token) = token else {
        return IdentityCheck::Unverified;
    };
    let Some(token_uuid) = token.account_uuid.as_deref() else {
        return IdentityCheck::Unverified;
    };
    let token_org = token.organization_uuid.as_deref().unwrap_or_default();
    if !token_org.is_empty() && !account.org_uuid.is_empty() && token_org != account.org_uuid {
        return IdentityCheck::Conflict;
    }
    if account.uuid.trim().is_empty() {
        return backfill(store, account, token_uuid);
    }
    if account.uuid.trim() == token_uuid {
        IdentityCheck::Matches
    } else {
        IdentityCheck::Conflict
    }
}

/// Records `uuid` as the account's uuid; [`IdentityCheck::Unverified`] when it was not written.
fn backfill(store: &Store, account: &AccountRecord, uuid: &str) -> IdentityCheck {
    let Ok(lock) = AccountLock::acquire(store.root()) else {
        tracing::debug!(
            account = account.number,
            "the account lock was not taken; the account's uuid is not backfilled"
        );
        return IdentityCheck::Unverified;
    };
    match store.backfill_uuid(&lock, account.number, uuid, &Identity::of(account)) {
        Ok(true) => IdentityCheck::Backfilled,
        Ok(false) => IdentityCheck::Unverified,
        Err(error) => {
            tracing::debug!(account = account.number, kind = ?error.kind(), "the account's uuid could not be backfilled");
            IdentityCheck::Unverified
        }
    }
}

/// Makes sure a switch target's stored access token outlives [`FRESHEN_BUFFER`] before it is activated.
/// A token lapsing within the buffer whose account's config backup names another login than its record (the switch's own
/// target identity check) is [`Freshened::IdentityMismatch`], and one whose config backup is missing or unreadable is
/// [`GrantError::Transient`], each logged once with nothing spent; any other has its grant spent through
/// [`consume_backup_grant`]; the account the endpoint names is
/// then checked ([`note_token_identity`]) against the account's record as `accounts.json` holds it after the grant (re-read
/// for it; nothing is checked when it cannot be read), and a conflict quarantines the target as
/// [`IDENTITY_CONFLICT`] instead of returning it as ready. Only the target's stored backup is ever touched, never
/// Claude Code's live credential. The caller must not hold the account lock or the target's consume lock: the gate and
/// the backfill take them.
///
/// # Errors
///
/// As [`AutoswitchState::update`], when an identity conflict's quarantine was not written. The successor is stored
/// by then.
pub fn freshen_target(
    context: &RefreshContext<'_>,
    account: &AccountRecord,
) -> Result<Freshened, EngineError> {
    let store = context.store;
    store.run_freshen_hook();
    let Ok(Some(credentials)) = store.read_credentials(account.number) else {
        return Ok(Freshened::Failed(GrantError::Transient));
    };
    let Ok(expires_at) = oauth_expiry(&credentials) else {
        return Ok(Freshened::Failed(GrantError::NoRefreshToken));
    };
    if !expires_at.is_some_and(|expires_at| near_expiry(expires_at, (context.clock)())) {
        return Ok(Freshened::Ready);
    }
    if let Err(kind) = target_identity_check(store, account.number, account) {
        log_identity_refusal(account.number, kind);
        return Ok(if kind == SwitchErrorKind::TargetIdentityMismatch {
            Freshened::IdentityMismatch
        } else {
            Freshened::Failed(GrantError::Transient)
        });
    }
    let outcome = consume_backup_grant(context, account.number, &credentials);
    if outcome.error.is_some() || outcome.credentials.is_none() {
        return Ok(Freshened::Failed(
            outcome.error.unwrap_or(GrantError::Transient),
        ));
    }
    if check_after_grant(store, account.number, outcome.token_account.as_ref())
        == IdentityCheck::Conflict
    {
        quarantine(
            store,
            account.number,
            &account.email,
            IDENTITY_CONFLICT,
            (context.clock)(),
        )?;
        return Ok(Freshened::IdentityConflict);
    }
    Ok(Freshened::Ready)
}

/// [`note_token_identity`] against account `number`'s record as `accounts.json` holds it now;
/// [`IdentityCheck::Unverified`] when it cannot be read or no longer has the account.
fn check_after_grant(store: &Store, number: u32, token: Option<&TokenAccount>) -> IdentityCheck {
    let Ok(Some(sequence)) = store.read_sequence() else {
        tracing::debug!(
            account = number,
            "accounts.json could not be read after the grant; the token's account is not checked"
        );
        return IdentityCheck::Unverified;
    };
    sequence
        .account(number)
        .map_or(IdentityCheck::Unverified, |record| {
            note_token_identity(store, &record, token)
        })
}

/// Whether an access token lapsing at `expires_at` (milliseconds since the epoch) lapses within [`FRESHEN_BUFFER`] of
/// `now`: `now_ms + FRESHEN_BUFFER_MS >= expires_at`.
fn near_expiry(expires_at: f64, now: jiff::Timestamp) -> bool {
    let Ok(soon) = now.checked_add(FRESHEN_BUFFER) else {
        return true;
    };
    #[expect(
        clippy::cast_precision_loss,
        reason = "a millisecond timestamp is far inside f64's exact integer range"
    )]
    let soon = soon.as_millisecond() as f64;
    soon >= expires_at
}
