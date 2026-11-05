//! Switching the live Claude Code login to another managed account.
//!
//! [`probe_outgoing`] asks the profile oracle, before any lock, whose account the live credential is when its bytes
//! alone cannot tell; its answer is advisory. [`switch`] then takes the target's consume lock, the store's account lock,
//! Claude Code's two credential locks and its config lock, in that order and with each lock's usual wait, and the stop
//! drain's guard once every lock is held. When the live login is a managed account, it snapshots the live credential
//! file and `~/.claude.json` under them, backs the outgoing credential and config up into its account (or stashes the
//! credential when it is another login's), writes the target's credential with the live file's machine-shared keys
//! merged in, splices the target's `oauthAccount` into `~/.claude.json` (dropping `primaryApiKey`) and records the
//! target as the active account. When the live login is no managed account's, or there is none, it activates the
//! target directly (a direct activation): the displaced live credential is stashed first and nothing is
//! backed up, and a missing `~/.claude.json` is written from the target's config backup. A write that fails puts every
//! live file already written back as it was, byte for byte, while it still holds the switch's bytes (removing one the
//! switch created; one rewritten since is left, and the rollback reported failed), keeping the live pair one
//! account's. An API-key account is no account to switch to or from, and a live `primaryApiKey` the
//! switch would drop with no copy kept is refused. No network runs under a lock, and no outcome, error or log line
//! carries a token, a credential or a path.

use std::fmt;
use std::time::Duration;

use serde_json::{Map, Value};
use zeroize::Zeroizing;

use crate::claude_files::{
    ClaudeFileError, Expected, read_live_credentials, remove_global_config,
    remove_live_credentials, restore_global_config, restore_live_credentials,
    rewrite_global_config, rewrite_live_credentials,
};
use crate::locks::{
    AccountLock, AccountLockGuard, ClaudeCodeLocks, ClaudeCredentialsGuard, ConsumeLock,
    ConsumeLockGuard, DirLockGuard, LockError,
};
use crate::paths::{ClaudePaths, Email, LiveStore};
use crate::poll::{identity_in, live_identity};
use crate::refresh::oracle::{
    LineageKey, ResolvedProfile, Verdict, resolve, resolved_matches_slot_identity,
};
use crate::refresh::stash::ConfigSlot;
use crate::refresh::{
    OauthShape, RefreshContext, Scrubbed, Slot, copy, credential_fingerprint, newer_generation,
    scrub, stash_displaced,
};
use crate::secret::CredentialBlob;
use crate::store::{self, AccountRecord, Retention, Sequence, Store, StoreError, StoreErrorKind};
use crate::usage::Identity;

/// The `oauthAccount` object in `~/.claude.json`'s bytes, `None` when there is none or it is not an object. The parsed
/// tree is zeroed when it drops: the live file can hold MCP servers' `env` and `headers` values.
fn oauth_account_of(bytes: &[u8]) -> Option<Value> {
    let tree = Scrubbed(serde_json::from_slice::<Value>(bytes).ok()?);
    let Value::Object(config) = &tree.0 else {
        return None;
    };
    config
        .get("oauthAccount")
        .filter(|value| value.is_object())
        .cloned()
}

/// Whether the `oauthAccount` value `account` names the login `record` does: the same email and organization UUID (a
/// missing one counting as none, as [`Identity`] matches), and the same account uuid when both hold one.
pub(crate) fn backs_record(record: &AccountRecord, account: &Value) -> bool {
    let uuid_matches = match account.get("accountUuid").and_then(Value::as_str) {
        Some(uuid) if !uuid.is_empty() && !record.uuid.is_empty() => uuid == record.uuid,
        Some(_) | None => true,
    };
    identity_in(account).is_some_and(|identity| identity == Identity::of(record)) && uuid_matches
}

/// Whether account `number`'s config backup names the login its record `target` does ([`backs_record`]), read without
/// a lock and writing nothing: [`SwitchErrorKind::TargetBackupMissing`] when there is no config backup,
/// [`SwitchErrorKind::TargetBackupUnreadable`] when it cannot be read or parsed, and
/// [`SwitchErrorKind::TargetIdentityMismatch`] when its `oauthAccount` is empty or names another login. The account
/// number is all that ties a record to its files, so nothing is written or spent for an account that fails it. It logs
/// nothing: a caller that acts on the refusal logs it ([`log_identity_refusal`]), and the quarantine release asks it on
/// every tick.
pub(crate) fn target_identity_check(
    backups: &Store,
    number: u32,
    target: &AccountRecord,
) -> Result<(), SwitchErrorKind> {
    let config = backups
        .read_account_config(number)
        .map_err(|_| SwitchErrorKind::TargetBackupUnreadable)?
        .ok_or(SwitchErrorKind::TargetBackupMissing)?;
    let identity = Scrubbed(Value::Object((*config).clone()));
    if !store::truthy(&identity.0) || !backs_record(target, &identity.0) {
        return Err(SwitchErrorKind::TargetIdentityMismatch);
    }
    Ok(())
}

/// Logs, once and at warn, that account `number`'s [`target_identity_check`] refused it as `kind`, with nothing written
/// or spent for it.
pub(crate) fn log_identity_refusal(number: u32, kind: SwitchErrorKind) {
    tracing::warn!(
        account = number,
        kind = kind.as_str(),
        "the account's config backup is missing, unreadable or names another login than its record; nothing is written or spent for it"
    );
}

/// The credential file's machine-shared keys: the live file's copies are the current
/// generation, so a switch takes them from it, absence included, and drops the target's.
const SHARED_KEYS: [&str; 5] = [
    "mcpOAuth",
    "mcpOAuthClientConfig",
    "mcpXaaIdp",
    "mcpXaaIdpConfig",
    "pluginSecrets",
];
/// The strategy name for a switch to a named account.
const STRATEGY: &str = "direct";

/// Why a switch did not happen, or did not finish.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SwitchErrorKind {
    /// No switchable managed account has this number: none at all, an API-key account, or no
    /// `accounts.json`. Nothing was locked or written.
    UnknownAccount,
    /// `accounts.json` could not be read or parsed. Nothing was written.
    StoreUnreadable,
    /// `~/.claude.json` holds a `primaryApiKey` the switch would drop with no backup, when the live login is no managed
    /// account's (an API-key account's included), or is one whose live credential is another login's (which is
    /// stashed, and no config backup written). Nothing was written.
    ApiKeyAccount,
    /// Sealing does not work in this process (the store's canary failed), so the store takes no write. Nothing was
    /// written.
    StoreReadOnly,
    /// `accounts.json` was written by a later version, so the store takes no write. Nothing was written.
    UnknownSchema,
    /// `CLAUDE_SECURESTORAGE_CONFIG_DIR` is set: Claude Code keeps its login in a store the account store does not
    /// mirror.
    /// Nothing was written.
    StoreUnmirrored,
    /// The live credential file exists but cannot be read or is not UTF-8, or, when the live login is a managed
    /// account's, is missing or empty. Nothing was written.
    LiveUnreadable,
    /// `~/.claude.json` exists but cannot be read or is not a JSON object, or, when the live login is a managed
    /// account's, is missing. Nothing was written.
    ConfigUnreadable,
    /// The outgoing account's credential backup could not be read or written, its config backup could not be written,
    /// or the stash of a live credential that is not the outgoing account's own or that no managed account has could not
    /// be written. No live file was written.
    OutgoingBackupFailed,
    /// The target has no usable backup: its stored credential, its config backup or that backup's `oauthAccount` is
    /// missing. Nothing was written when a backup file is absent; the live login is unchanged either way.
    TargetBackupMissing,
    /// The target's stored credential or config backup exists but could not be read or parsed. The live login is
    /// unchanged.
    TargetBackupUnreadable,
    /// The target's config backup names another login than its record in `accounts.json` (another email, organization
    /// or account uuid): the account number is all that ties a record to its files, and they disagree. Nothing was
    /// written.
    TargetIdentityMismatch,
    /// A lock stayed held past its wait, one of Claude Code's locks was lost while the switch waited, or the live login
    /// read as no managed account's and then, on a second read, as one. Nothing was written.
    LocksBusy,
    /// A lock could not be taken for another reason (an I/O error, or a platform without the locks). Nothing was
    /// written.
    LockFailed,
    /// The stop drain has begun. Nothing was written.
    Draining,
    /// A write failed; no live file is left changed: every live file written before it was put back as it was.
    WriteFailed,
    /// A write failed and putting the live files back failed too, or a live file the switch wrote was rewritten
    /// since and left in place: [`SwitchError::login_state`] says where the live login was left.
    RollbackFailed,
    /// The caller's [`SwitchPrecondition`] no longer holds, judged against the roster and the live login (before the
    /// locks, and again under every lock): the live login is not the account the caller expected to leave, or a paid
    /// move's target is disabled or no longer opted in to spending its credits. Claude Code's live login, config and
    /// credentials were not changed.
    PreconditionFailed,
    /// The caller's switch found auto-switch turned off once it held its gate: the owner turned it off while the tick
    /// that decided the switch ran. Nothing was written.
    AutoSwitchOff,
}

impl SwitchErrorKind {
    /// The switch's kind for a store that refuses writes ([`crate::Store::write_refusal`]): a failed canary or a
    /// later `accounts.json` schema as itself, an `accounts.json` that cannot be read or parsed as
    /// [`Self::StoreUnreadable`].
    #[must_use]
    pub fn of_store_refusal(kind: StoreErrorKind) -> Self {
        match kind {
            StoreErrorKind::StoreReadOnly => Self::StoreReadOnly,
            StoreErrorKind::UnknownSchema => Self::UnknownSchema,
            _ => Self::StoreUnreadable,
        }
    }

    /// The kind's name, as the server's error replies carry it: `unknown_account`, `locks_busy`, `rollback_failed`, ….
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UnknownAccount => "unknown_account",
            Self::StoreUnreadable => "store_unreadable",
            Self::ApiKeyAccount => "api_key_account",
            Self::StoreReadOnly => "store_read_only",
            Self::UnknownSchema => "unknown_schema",
            Self::StoreUnmirrored => "store_unmirrored",
            Self::LiveUnreadable => "live_unreadable",
            Self::ConfigUnreadable => "config_unreadable",
            Self::OutgoingBackupFailed => "outgoing_backup_failed",
            Self::TargetBackupMissing => "target_backup_missing",
            Self::TargetBackupUnreadable => "target_backup_unreadable",
            Self::TargetIdentityMismatch => "target_identity_mismatch",
            Self::LocksBusy => "locks_busy",
            Self::LockFailed => "lock_failed",
            Self::Draining => "draining",
            Self::WriteFailed => "write_failed",
            Self::RollbackFailed => "rollback_failed",
            Self::PreconditionFailed => "precondition_failed",
            Self::AutoSwitchOff => "auto_switch_off",
        }
    }
}

impl fmt::Display for SwitchErrorKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::UnknownAccount => "no managed account has this number",
            Self::StoreUnreadable => {
                "accounts.json could not be read; repair or move it, then retry"
            }
            Self::ApiKeyAccount => {
                "the live login holds an API key no managed account backs up, which the switch would drop; nothing was written"
            }
            Self::StoreReadOnly => {
                "sealing does not work in this process, so the account store takes no write; a restart may fix it"
            }
            Self::UnknownSchema => {
                "accounts.json was written by a later version, so the account store takes no write"
            }
            Self::StoreUnmirrored => {
                "CLAUDE_SECURESTORAGE_CONFIG_DIR is set: Claude Code keeps its login in a store the account store does not mirror"
            }
            Self::LiveUnreadable => {
                "Claude Code's live credential file could not be read; nothing was written"
            }
            Self::ConfigUnreadable => {
                "Claude Code's global config could not be read; nothing was written"
            }
            Self::OutgoingBackupFailed => {
                "the outgoing login's backup could not be read or written, or its stash could not be written; the live login is unchanged"
            }
            Self::TargetBackupMissing => {
                "the account has no usable backup (its stored credential or its oauthAccount is missing); add it again with Add this login"
            }
            Self::TargetBackupUnreadable => {
                "the account's stored backup could not be read; repair or move the unreadable file, then retry; the live login is unchanged"
            }
            Self::TargetIdentityMismatch => {
                "the account's config backup names another login than accounts.json records for it; nothing was written"
            }
            Self::LocksBusy => {
                "a lock on the account store or on Claude Code's files was held by another process; nothing was written, retry in a few seconds"
            }
            Self::LockFailed => {
                "a lock on the account store or on Claude Code's files could not be taken; nothing was written"
            }
            Self::Draining => "the server is shutting down; nothing was written",
            Self::WriteFailed => {
                "a write failed; no live file is left changed, and the live login is unchanged"
            }
            Self::RollbackFailed => {
                "a write failed and rolling it back failed too; Claude Code's live login may need repair"
            }
            Self::PreconditionFailed => {
                "the live login or a credits opt-in changed since the switch was decided; the live login and its files were kept"
            }
            Self::AutoSwitchOff => {
                "auto-switch was turned off before the switch ran; nothing was written"
            }
        };
        formatter.write_str(text)
    }
}

/// What must still hold when a switch runs, checked against the roster and the live login before the locks and again
/// under every lock, the check that counts: the live login is still `expected_outgoing`; when
/// `target_must_be_paid_opted_in`, the target is neither disabled nor without `maySpendCredits`; and, when
/// `outgoing_must_be_opted_out`, the live login's own record has `maySpendCredits` off. A refusal is
/// [`SwitchErrorKind::PreconditionFailed`], with Claude Code's live login, config and credentials not changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SwitchPrecondition {
    /// The account the caller expects the live login to be.
    pub expected_outgoing: u32,
    /// Whether the target must be an enabled account whose owner allowed its credits to be spent.
    pub target_must_be_paid_opted_in: bool,
    /// Whether the account the live login leaves must have its owner's leave to spend credits withdrawn
    /// (`maySpendCredits` off): the auto-switch engine's leave from paid mode once that flag is turned off, the one move
    /// it makes while auto-switch is off.
    pub outgoing_must_be_opted_out: bool,
}

impl SwitchPrecondition {
    /// Whether the switch to `target` from the live login `current` (`None` when it is no managed account's) may go on.
    fn holds(self, current: Option<&AccountRecord>, target: &AccountRecord) -> bool {
        let opted_in = !target.disabled && target.may_spend_credits;
        let opted_out = current.is_some_and(|current| !current.may_spend_credits);
        current.map(|current| current.number) == Some(self.expected_outgoing)
            && (!self.target_must_be_paid_opted_in || opted_in)
            && (!self.outgoing_must_be_opted_out || opted_out)
    }
}

/// Where a switch that wrote a live file and failed left Claude Code's live login.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoginState {
    /// Both live files are back as they were: the login is the one the switch started from.
    Source,
    /// The credential file is the target's; `~/.claude.json` is the target's (the login is then the target's, whole,
    /// though `accounts.json` may not say so), or one the switch wrote and another writer rewrote since (a file the
    /// rollback would have restored as well as one it would have removed).
    Target,
    /// `~/.claude.json` is back as it was; the credential file is the target's, or one the switch wrote and another
    /// writer rewrote since (a file the rollback would have restored as well as one it would have removed).
    Mixed,
}

impl LoginState {
    /// `source`, `target` or `mixed`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Target => "target",
            Self::Mixed => "mixed",
        }
    }
}

/// Which live files a failed write left a temp file beside: the temp file holds the bytes that write was putting there
/// (for the credential file, tokens).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TempsLeft {
    /// Beside Claude Code's live credential file.
    pub credentials: bool,
    /// Beside `~/.claude.json`.
    pub config: bool,
}

/// A switch that did not happen or did not finish: its kind, the account it was switching to, and, once a live file
/// was written, where the live login was left and which temp files the failed writes left.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{kind} (account {account})")]
pub struct SwitchError {
    kind: SwitchErrorKind,
    account: u32,
    login: Option<LoginState>,
    temps: TempsLeft,
}

impl SwitchError {
    fn new(kind: SwitchErrorKind, account: u32) -> Self {
        Self {
            kind,
            account,
            login: None,
            temps: TempsLeft::default(),
        }
    }

    fn with_login(self, login: LoginState, temps: TempsLeft) -> Self {
        Self {
            login: Some(login),
            temps,
            ..self
        }
    }

    /// What went wrong.
    #[must_use]
    pub fn kind(&self) -> SwitchErrorKind {
        self.kind
    }

    /// The account the switch was going to.
    #[must_use]
    pub fn account(&self) -> u32 {
        self.account
    }

    /// Where the live login was left: for [`SwitchErrorKind::WriteFailed`] always [`LoginState::Source`], including an
    /// adoption into the target's backup that failed before any live file was touched, and for
    /// [`SwitchErrorKind::RollbackFailed`] where the rollback stopped; `None` for every other kind, none of which wrote a
    /// live file.
    #[must_use]
    pub fn login_state(&self) -> Option<LoginState> {
        self.login
    }

    /// Which live files a failed write left a temp file beside.
    #[must_use]
    pub fn temps_left(&self) -> TempsLeft {
        self.temps
    }
}

/// An account as a switch outcome names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwitchAccount {
    /// The managed account's number; `None` for a live login no managed account has.
    pub number: Option<u32>,
    /// The login's email.
    pub email: Email,
}

impl SwitchAccount {
    fn of(record: &AccountRecord) -> Self {
        Self {
            number: Some(record.number),
            email: record.email.clone(),
        }
    }
}

/// What a switch came to.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SwitchOutcome {
    /// Whether the live login moved to another account.
    pub switched: bool,
    /// The account the live login was, with no number when no managed account has it; the target itself when nothing
    /// moved; `None` when `~/.claude.json` named no login, or named one whose email cannot be an account's.
    pub from: Option<SwitchAccount>,
    /// The account the live login is now.
    pub to: SwitchAccount,
    /// The strategy name: `direct`, a switch to a named account.
    pub strategy: &'static str,
    /// `switched`, or `already-active` when the target already was the live login.
    pub reason: &'static str,
    /// One line for the user: `Switched to Account-N (email)` or `Already on Account-N (email)`.
    pub message: String,
    /// What the switch did that the user should know of: the outgoing credential stashed rather than backed up, found
    /// wiped, or backed up without its `.prev` copy.
    pub warnings: Vec<String>,
    /// How long a running Claude Code may go on with the login it had: [`KEYCHAIN_CACHE`] when the live login is kept
    /// in the macOS Keychain, whose reads Claude Code caches; `None` when it reads the live credential file afresh, and
    /// for a switch that moved nothing.
    pub takes_effect_within: Option<Duration>,
}

/// How long Claude Code keeps a Keychain read of its live login before it reads the item again: a switch written to
/// the Keychain reaches a running Claude Code within this.
pub const KEYCHAIN_CACHE: Duration = Duration::from_secs(30);

/// [`SwitchOutcome::takes_effect_within`] for a switch that wrote the live login where `paths` keeps it.
fn takes_effect_within(paths: &ClaudePaths) -> Option<Duration> {
    matches!(paths.live_store(), LiveStore::Keychain { .. }).then_some(KEYCHAIN_CACHE)
}

/// What the profile oracle said, before the switch took any lock, about whose account the live credential is.
/// The switch uses the answer only while the live file still holds the bytes it was asked
/// about. Its `Debug` shows only whether there is an answer.
pub struct OutgoingProbe {
    live: Option<CredentialBlob>,
    resolved: Option<ResolvedProfile>,
}

impl OutgoingProbe {
    /// A probe that asked nothing: an outgoing credential the switch cannot attribute by its bytes is unresolved, and
    /// backed up into its account: the backup fails open.
    #[must_use]
    pub fn unresolved() -> Self {
        Self {
            live: None,
            resolved: None,
        }
    }

    /// The oracle's answer about `live`, when it was asked about these bytes and answered.
    fn resolved_for(&self, live: &CredentialBlob) -> Option<&ResolvedProfile> {
        let probed = self.live.as_ref()?;
        if probed.expose() != live.expose() {
            return None;
        }
        self.resolved.as_ref()
    }
}

impl fmt::Debug for OutgoingProbe {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OutgoingProbe")
            .field("resolved", &self.resolved.is_some())
            .finish_non_exhaustive()
    }
}

/// Asks the profile oracle whose account the live credential is, so that a
/// switch can tell an outgoing credential its account rotated from another login's. Call it before [`switch`], which
/// takes the answer as advisory: the oracle is asked only when the live login is a managed account whose backup holds
/// neither the live bytes nor their lineage, and an oracle that fails or cannot tell leaves the probe unresolved. This
/// is the switch's one network call, bounded by the caller's [`crate::ProfileTransport`] timeout.
#[must_use]
pub fn probe_outgoing(context: &RefreshContext<'_>) -> OutgoingProbe {
    let Some(live) = read_live(context.claude) else {
        return OutgoingProbe::unresolved();
    };
    let resolved = if needs_oracle(context, &live) {
        ask_oracle(context, &live)
    } else {
        None
    };
    OutgoingProbe {
        live: Some(live),
        resolved,
    }
}

/// Whether only the oracle can attribute `live`: the live login is a managed account's, and its backup holds neither
/// these bytes nor this lineage.
fn needs_oracle(context: &RefreshContext<'_>, live: &CredentialBlob) -> bool {
    let Ok(Some(sequence)) = context.store.read_sequence() else {
        return false;
    };
    let Some(account) = current_account(context.claude, &sequence) else {
        return false;
    };
    let backup = match context.store.read_credentials(account.number) {
        Ok(backup) => backup,
        // An unreadable backup cannot be compared with the live bytes: it neither matches them nor proves it does not,
        // so the oracle is asked instead of treating the file as absent.
        Err(error) => {
            tracing::debug!(account = account.number, %error, "the outgoing account's backup could not be read; asking the oracle");
            return true;
        }
    };
    !backup.is_some_and(|backup| {
        backup.expose() == live.expose()
            || credential_fingerprint(&backup) == credential_fingerprint(live)
    })
}

/// The account the profile endpoint names for `live`'s access token; `None` when it holds none, or the endpoint failed
/// or named no account.
fn ask_oracle(context: &RefreshContext<'_>, live: &CredentialBlob) -> Option<ResolvedProfile> {
    let oauth = live.oauth()?;
    let resolved = context
        .profile
        .get_profile(oauth.access_token())
        .ok()
        .as_ref()
        .and_then(resolve);
    if resolved.is_none() {
        tracing::debug!(
            "the profile oracle named no account before the switch; the outgoing credential is unresolved"
        );
    }
    resolved
}

/// Switches Claude Code's live login to managed account `number`: synchronous, with lock
/// waits of up to 10 s each for the store's locks and the caller's [`RefreshContext::claude_locks`] for Claude Code's, so an
/// async caller runs it on a blocking thread. `probe` is [`probe_outgoing`]'s answer, taken before the call.
///
/// A target that already is the live login is a no-op (`switched: false`, `already-active`) that takes no lock. A
/// disabled target is switched to: it is only out of the automatic rotation. An API-key account is no
/// target ([`Sequence::switchable_account`]), and a live login that is one is no managed account's. Otherwise, when
/// the live login is a managed account's, the outgoing credential is classified:
/// left alone when its backup holds the same bytes, backed up when it is the
/// backup's lineage, the oracle's answer names the account (whose missing uuid is then recorded), or ownership cannot
/// be told; stashed and not backed up when the oracle, or an earlier verdict, named another account; left alone when
/// its tokens were wiped. Unless it was stashed, the whole `~/.claude.json` is written as the outgoing account's config
/// backup. A credential that would be stashed beside a `primaryApiKey` in `~/.claude.json` is refused instead: no file
/// would keep the key the splice drops.
///
/// A live login that is no managed account's, or none at all, is replaced directly (a direct activation):
/// the live credential, unless it is absent, empty or the target's own bytes, is stashed as a `displaced-live-login`
/// filed under `unmanaged` (or, when `~/.claude.json` names no login, under `accounts.json`'s active account), nothing
/// is backed up, and a missing `~/.claude.json` is created holding the target's `oauthAccount` alone. A `~/.claude.json`
/// holding a `primaryApiKey` is refused there: the key would go with no backup anywhere.
///
/// # Errors
///
/// A [`SwitchError`] whose [`SwitchErrorKind`] says why: [`SwitchErrorKind::UnknownAccount`] for a number no switchable
/// account has, [`SwitchErrorKind::ApiKeyAccount`] for a live `primaryApiKey` the switch would drop with no backup,
/// [`SwitchErrorKind::LocksBusy`] when a lock stayed held, [`SwitchErrorKind::LiveUnreadable`] or
/// [`SwitchErrorKind::ConfigUnreadable`] when a live file cannot be read, each with no live file written;
/// [`SwitchErrorKind::StoreReadOnly`], [`SwitchErrorKind::UnknownSchema`] or [`SwitchErrorKind::StoreUnreadable`] when
/// the store refuses writes, before the locks or, when the refusal appeared during their waits, under them, with
/// nothing written; [`SwitchErrorKind::TargetIdentityMismatch`] when the target's config backup names another login
/// than its record, under the locks, with nothing written; [`SwitchErrorKind::OutgoingBackupFailed`] when the outgoing
/// backup or the stash of the displaced live credential failed, with no live file written;
/// [`SwitchErrorKind::WriteFailed`] when a live write failed and the live files were put back, and
/// [`SwitchErrorKind::RollbackFailed`] when putting them back failed too.
pub fn switch(
    context: &RefreshContext<'_>,
    number: u32,
    probe: &OutgoingProbe,
    precondition: Option<SwitchPrecondition>,
) -> Result<SwitchOutcome, SwitchError> {
    if let Precheck::AlreadyActive(outcome) = before_locks(context, number, precondition)? {
        return Ok(outcome);
    }
    context.store.run_switch_locks_hook();
    let locks = Locks::acquire(context, number)?;
    // Taken only now, past every lock wait, so the server's bounded drain never waits one out.
    let Ok(_work) = context.drain.begin() else {
        return Err(SwitchError::new(SwitchErrorKind::Draining, number));
    };
    let held = Held {
        lock: &locks.lock,
        claude: &locks.claude,
        config: &locks.config,
    };
    Switch {
        context,
        probe,
        number,
        precondition,
    }
    .locked(&held)
}

/// What the refusals before any lock leave a switch to do.
enum Precheck {
    /// The target already is the live login: the switch is this no-op outcome, with no lock taken.
    AlreadyActive(SwitchOutcome),
    /// Take the locks and switch.
    Proceed,
}

/// The refusals a switch makes before any lock, in order: the roster read, the target looked up among the switchable
/// accounts, the caller's precondition (a cheap early refusal; the check under the locks is the one that counts), an
/// already-active target answered as a no-op, then [`SwitchErrorKind::StoreUnmirrored`] and a store that refuses
/// writes ([`crate::Store::write_refusal`], as [`SwitchErrorKind::of_store_refusal`]).
fn before_locks(
    context: &RefreshContext<'_>,
    number: u32,
    precondition: Option<SwitchPrecondition>,
) -> Result<Precheck, SwitchError> {
    let fail = |kind| SwitchError::new(kind, number);
    let sequence = read_roster(context, number)?;
    let target = sequence
        .switchable_account(number)
        .ok_or_else(|| fail(SwitchErrorKind::UnknownAccount))?;
    let current = current_account(context.claude, &sequence);
    refuse_unmet(precondition, current.as_ref(), &target)?;
    if current.is_some_and(|current| current.number == number) {
        return Ok(Precheck::AlreadyActive(already_active(&target)));
    }
    if context.securestorage_dir_set {
        return Err(fail(SwitchErrorKind::StoreUnmirrored));
    }
    if let Some(kind) = context.store.write_refusal() {
        return Err(fail(SwitchErrorKind::of_store_refusal(kind)));
    }
    Ok(Precheck::Proceed)
}

/// [`SwitchErrorKind::PreconditionFailed`] when `precondition` is given and does not hold for the switch from the live
/// login `current` to `target`.
fn refuse_unmet(
    precondition: Option<SwitchPrecondition>,
    current: Option<&AccountRecord>,
    target: &AccountRecord,
) -> Result<(), SwitchError> {
    match precondition {
        Some(precondition) if !precondition.holds(current, target) => {
            tracing::info!(
                account = target.number,
                "the live login or a credits opt-in changed since the switch was decided; the switch stops, the live files unchanged"
            );
            Err(SwitchError::new(
                SwitchErrorKind::PreconditionFailed,
                target.number,
            ))
        }
        Some(_) | None => Ok(()),
    }
}

/// Every lock [`switch`] holds, taken in its order: the target's consume lock, the store's account lock, Claude Code's two
/// credential locks, then its config lock. The fields are declared in the reverse order, so they are released last
/// taken, first released.
struct Locks {
    config: DirLockGuard,
    claude: ClaudeCredentialsGuard,
    lock: AccountLockGuard,
    _consume: ConsumeLockGuard,
}

impl Locks {
    /// Takes every lock in order, each with its usual wait; [`SwitchErrorKind::LocksBusy`] or
    /// [`SwitchErrorKind::LockFailed`] for the first that cannot be taken, releasing those already held.
    fn acquire(context: &RefreshContext<'_>, number: u32) -> Result<Self, SwitchError> {
        let root = context.store.root();
        let failed = |error: LockError| lock_error(&error, number);
        let consume = ConsumeLock::acquire(root, number).map_err(failed)?;
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

/// `accounts.json`: one that does not exist holds no account; one that cannot be read is refused as
/// [`SwitchErrorKind::of_store_refusal`] maps its kind (a later version's file as [`SwitchErrorKind::UnknownSchema`]).
fn read_roster(context: &RefreshContext<'_>, number: u32) -> Result<Sequence, SwitchError> {
    match context.store.read_sequence() {
        Ok(Some(sequence)) => Ok(sequence),
        Ok(None) => Err(SwitchError::new(SwitchErrorKind::UnknownAccount, number)),
        Err(error) => Err(SwitchError::new(
            SwitchErrorKind::of_store_refusal(error.kind()),
            number,
        )),
    }
}

/// The switchable managed account `~/.claude.json`'s `oauthAccount` names, by email and organization;
/// `None` for an API-key account's login, which the switch leaves as no managed
/// account's.
fn current_account(paths: &ClaudePaths, sequence: &Sequence) -> Option<AccountRecord> {
    let identity = live_identity(paths)?;
    sequence
        .records()
        .find(|record| Identity::of(record) == identity)
        .cloned()
}

/// The live credential file's text; `None` when it is missing, empty, unreadable or not UTF-8.
fn read_live(paths: &ClaudePaths) -> Option<CredentialBlob> {
    let bytes = read_live_credentials(paths).ok().flatten()?;
    let text = std::str::from_utf8(&bytes)
        .ok()
        .filter(|text| !text.is_empty())?;
    Some(CredentialBlob::new(text.to_owned()))
}

fn lock_error(error: &LockError, number: u32) -> SwitchError {
    let kind = if matches!(error, LockError::Timeout) {
        SwitchErrorKind::LocksBusy
    } else {
        SwitchErrorKind::LockFailed
    };
    SwitchError::new(kind, number)
}

fn already_active(target: &AccountRecord) -> SwitchOutcome {
    let account = SwitchAccount::of(target);
    SwitchOutcome {
        switched: false,
        from: Some(account.clone()),
        to: account,
        strategy: STRATEGY,
        reason: "already-active",
        message: format!(
            "Already on Account-{} ({})",
            target.number,
            target.email.as_str()
        ),
        warnings: Vec::new(),
        takes_effect_within: None,
    }
}

/// The locks a switch holds once its waits are over: the store's account lock, Claude Code's credential locks and its
/// config lock (the consume lock is held by [`switch`] alone).
struct Held<'h> {
    lock: &'h AccountLockGuard,
    claude: &'h ClaudeCredentialsGuard,
    config: &'h DirLockGuard,
}

/// The live files as they were under the locks, before any write: what a rollback puts back, and the outgoing
/// account's config backup.
struct Snapshot {
    live: CredentialBlob,
    config: Zeroizing<Vec<u8>>,
}

impl Snapshot {
    fn before(&self) -> Before<'_> {
        Before {
            credentials: Prior::Bytes(self.live.expose().as_bytes()),
            config: Prior::Bytes(self.config.as_slice()),
        }
    }
}

/// The live files as a direct activation found them under the locks, before any write: `None` for a file that does not
/// exist. An empty credential file is an empty credential.
struct Found {
    live: Option<CredentialBlob>,
    config: Option<FoundConfig>,
}

impl Found {
    fn before(&self) -> Before<'_> {
        Before {
            credentials: self
                .live
                .as_ref()
                .map_or(Prior::Absent, |live| Prior::Bytes(live.expose().as_bytes())),
            config: self.config.as_ref().map_or(Prior::Absent, |config| {
                Prior::Bytes(config.bytes.as_slice())
            }),
        }
    }

    /// The login the `~/.claude.json` snapshot names, `None` when there is none or no file.
    fn identity(&self) -> Option<&Identity> {
        self.config
            .as_ref()
            .and_then(|config| config.identity.as_ref())
    }
}

/// `~/.claude.json` as a direct activation read it: its bytes, the login its `oauthAccount` names, and whether it holds
/// a `primaryApiKey`.
struct FoundConfig {
    bytes: Zeroizing<Vec<u8>>,
    identity: Option<Identity>,
    api_key: bool,
}

/// What a live file held before the switch wrote it: what a rollback puts back.
#[derive(Clone, Copy)]
enum Prior<'b> {
    /// The file did not exist: a rollback removes the one the switch made, while it still holds the switch's bytes.
    Absent,
    /// The file's bytes, written back byte for byte.
    Bytes(&'b [u8]),
}

impl<'b> Prior<'b> {
    /// What the live file, `~/.claude.json` or the live credential file, must still hold for the switch to rewrite it:
    /// what the switch found there.
    fn expected(self) -> Expected<'b> {
        match self {
            Self::Absent => Expected::Absent,
            Self::Bytes(bytes) => Expected::Bytes(bytes),
        }
    }
}

/// Both live files as the switch found them: what each forward write expects them still to hold, and what a rollback
/// puts back.
#[derive(Clone, Copy)]
struct Before<'b> {
    credentials: Prior<'b>,
    config: Prior<'b>,
}

/// What the target's backup gives the live login. Both trees are zeroed when they drop, on every path.
struct Incoming {
    credentials: CredentialBlob,
    oauth_account: Scrubbed,
}

/// What the switch has written to each live file so far, byte for byte, and which failed writes left a temp file. A
/// rollback restores or removes a file the switch wrote only while the file still holds these bytes.
#[derive(Default)]
struct Written<'w> {
    /// The merged credential's bytes, once the credential file was written.
    credentials: Option<&'w [u8]>,
    /// `~/.claude.json`'s bytes, once it was written, zeroed on drop.
    config: Option<Zeroizing<Vec<u8>>>,
    temps: TempsLeft,
}

/// Why a rollback could not put a live file back.
enum NotPutBack {
    /// Writing the file back, or removing the one the switch created, failed.
    Failed(ClaudeFileError),
    /// The file the switch wrote holds other bytes now, written since: it was left in place, whether the rollback would
    /// have restored it or removed it.
    Changed,
}

impl fmt::Display for NotPutBack {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Failed(error) => fmt::Display::fmt(error, formatter),
            Self::Changed => formatter
                .write_str("a file the switch wrote was rewritten since, and was left in place"),
        }
    }
}

/// What the outgoing account's backup may take of the live credential, by ownership.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outgoing {
    /// The backup holds these very bytes: nothing to back up.
    OwnBytes,
    /// The backup's lineage, its access token rotated.
    OwnFamily,
    /// Another lineage the oracle named as the account's own.
    OwnRotated,
    /// Ownership could not be determined, so the live credential is backed up (the backup fails open).
    Unresolved,
    /// An OAuth block with both tokens emptied (Claude Code's reaction to a refused refresh): nothing worth keeping.
    Wiped,
    /// The oracle named another account.
    Foreign,
    /// An earlier verdict this process remembers named another account.
    KnownForeign,
}

/// The outgoing credential's [`Outgoing`] kind, and the outgoing account's backup as the classification read it under
/// the account lock (`None` when it has none).
struct Classified {
    kind: Outgoing,
    backup: Option<CredentialBlob>,
}

/// One switch to account `number`, under its locks.
struct Switch<'s, 'c> {
    context: &'s RefreshContext<'c>,
    probe: &'s OutgoingProbe,
    number: u32,
    precondition: Option<SwitchPrecondition>,
}

impl Switch<'_, '_> {
    fn fail(&self, kind: SwitchErrorKind) -> SwitchError {
        SwitchError::new(kind, self.number)
    }

    /// Under every lock: the roster and the live login read again, since either may have changed during the waits,
    /// and the store's write refusal checked again ([`crate::Store::write_refusal`], as
    /// [`SwitchErrorKind::of_store_refusal`]), since it too may have appeared during them; the caller's precondition
    /// judged against them, then the switch the live login calls for.
    fn locked(&self, held: &Held<'_>) -> Result<SwitchOutcome, SwitchError> {
        let sequence = read_roster(self.context, self.number)?;
        if let Some(kind) = self.context.store.write_refusal() {
            return Err(self.fail(SwitchErrorKind::of_store_refusal(kind)));
        }
        let target = sequence
            .switchable_account(self.number)
            .ok_or_else(|| self.fail(SwitchErrorKind::UnknownAccount))?;
        let current = current_account(self.context.claude, &sequence);
        refuse_unmet(self.precondition, current.as_ref(), &target)?;
        match current {
            None => self.leave_unmanaged(held, &sequence, &target),
            Some(current) if current.number == target.number => Ok(already_active(&target)),
            Some(current) => self.leave_managed(held, &current, &target),
        }
    }

    /// The direct activation, for a live login that is no managed account's or none at all: the target's backup
    /// read, the live files read, the displaced live credential stashed unless there is none or it is the target's own
    /// bytes, then the target made live, rolling the live files back when a write fails. Nothing is backed up.
    fn leave_unmanaged(
        &self,
        held: &Held<'_>,
        sequence: &Sequence,
        target: &AccountRecord,
    ) -> Result<SwitchOutcome, SwitchError> {
        let (mut incoming, found) = self.read_direct(held, target)?;
        self.refuse_direct(found.config.as_ref(), sequence)?;
        let identity = found.identity();
        let (slot, organization) = displaced_slot(identity, sequence);
        self.stash_live(
            found.live.as_ref(),
            &incoming.credentials,
            slot,
            &organization,
        )?;
        let no_live = CredentialBlob::new(String::new());
        let merged = merge_shared(
            &incoming.credentials,
            found.live.as_ref().unwrap_or(&no_live),
        )
        .ok_or_else(|| self.merge_failed())?;
        let oauth_account = &mut incoming.oauth_account;
        let mut written = Written::default();
        if self
            .activate(
                held,
                &merged,
                found.before(),
                |config| splice(config, oauth_account),
                &mut written,
            )
            .is_err()
        {
            return Err(self.roll_back(held, found.before(), &written));
        }
        Ok(switched(
            self.context.claude,
            displaced_from(identity),
            target,
            Vec::new(),
        ))
    }

    /// What a direct activation reads before it writes anything, in order: Claude Code's locks still held, the target's
    /// backup there and readable, then the live files.
    fn read_direct(
        &self,
        held: &Held<'_>,
        target: &AccountRecord,
    ) -> Result<(Incoming, Found), SwitchError> {
        self.require_locks(held)?;
        self.require_target_backup(target)?;
        let incoming = self.incoming(target)?;
        Ok((incoming, self.found()?))
    }

    /// Refuses a direct activation the `~/.claude.json` snapshot rules out, with nothing written:
    /// [`SwitchErrorKind::ApiKeyAccount`] when it holds a `primaryApiKey`, which the splice would drop with no backup
    /// anywhere; [`SwitchErrorKind::LocksBusy`] when it names a managed account after all, since the live login then
    /// changed since the switch looked, and its credential is no displaced login's.
    fn refuse_direct(
        &self,
        config: Option<&FoundConfig>,
        sequence: &Sequence,
    ) -> Result<(), SwitchError> {
        let Some(config) = config else {
            return Ok(());
        };
        if config.api_key {
            tracing::warn!(
                account = self.number,
                "the live login holds an API key no managed account backs up; the switch stops with nothing written"
            );
            return Err(self.fail(SwitchErrorKind::ApiKeyAccount));
        }
        let managed = config.identity.as_ref().is_some_and(|identity| {
            sequence
                .records()
                .any(|record| Identity::of(record) == *identity)
        });
        if managed {
            tracing::warn!(
                account = self.number,
                "Claude Code's global config named a managed account on a second read; the switch stops with nothing written"
            );
            return Err(self.fail(SwitchErrorKind::LocksBusy));
        }
        Ok(())
    }

    /// The switch from managed account `current`: snapshot, back up the outgoing account, read the target's
    /// backup, then make it live, rolling the live files back when a write fails.
    fn leave_managed(
        &self,
        held: &Held<'_>,
        current: &AccountRecord,
        target: &AccountRecord,
    ) -> Result<SwitchOutcome, SwitchError> {
        let (snapshot, warnings) = self.hand_off(held, current, target)?;
        let mut incoming = self.incoming(target)?;
        let merged = merge_shared(&incoming.credentials, &snapshot.live)
            .ok_or_else(|| self.merge_failed())?;
        let oauth_account = &mut incoming.oauth_account;
        let mut written = Written::default();
        if self
            .activate(
                held,
                &merged,
                snapshot.before(),
                |config| splice(config, oauth_account),
                &mut written,
            )
            .is_err()
        {
            return Err(self.roll_back(held, snapshot.before(), &written));
        }
        Ok(switched(
            self.context.claude,
            Some(SwitchAccount::of(current)),
            target,
            warnings,
        ))
    }

    /// What a switch from managed account `current` does before it reads the target's backup: the live files
    /// snapshotted, Claude Code's locks checked, the target's backup files looked for, the outgoing credential
    /// classified and a foreign one beside an API key refused, then the outgoing account backed up or its credential
    /// stashed. The snapshot and the warnings so far.
    fn hand_off(
        &self,
        held: &Held<'_>,
        current: &AccountRecord,
        target: &AccountRecord,
    ) -> Result<(Snapshot, Vec<String>), SwitchError> {
        let snapshot = self.snapshot()?;
        self.require_locks(held)?;
        self.require_target_backup(target)?;
        let classified = self.classify(current, &snapshot.live)?;
        self.refuse_foreign_api_key(classified.kind, &snapshot)?;
        let mut warnings = Vec::new();
        self.back_up_outgoing(held.lock, current, &classified, &snapshot, &mut warnings)?;
        Ok((snapshot, warnings))
    }

    /// [`SwitchErrorKind::ApiKeyAccount`], with nothing written, when the outgoing credential is another login's
    /// ([`Outgoing::Foreign`] or [`Outgoing::KnownForeign`]) and the `~/.claude.json` snapshot holds a `primaryApiKey`:
    /// the credential would be stashed and no config backup written, so the splice would drop the key with no copy
    /// anywhere.
    fn refuse_foreign_api_key(
        &self,
        kind: Outgoing,
        snapshot: &Snapshot,
    ) -> Result<(), SwitchError> {
        // Only a foreign credential is stashed with no config backup, so only then is the config parsed.
        if !(matches!(kind, Outgoing::Foreign | Outgoing::KnownForeign)
            && holds_api_key(&snapshot.config))
        {
            return Ok(());
        }
        tracing::warn!(
            account = self.number,
            "the live credential is another login's beside an API key no file would keep; the switch stops with nothing written"
        );
        Err(self.fail(SwitchErrorKind::ApiKeyAccount))
    }

    /// [`SwitchErrorKind::LocksBusy`] when either of Claude Code's locks was lost while the switch held it.
    fn require_locks(&self, held: &Held<'_>) -> Result<(), SwitchError> {
        if held.claude.still_owned() && held.config.still_owned() {
            return Ok(());
        }
        tracing::warn!(
            account = self.number,
            "a Claude Code lock was lost while the switch held it; nothing was written"
        );
        Err(self.fail(SwitchErrorKind::LocksBusy))
    }

    /// The merged credential could not be written out: nothing live was written.
    fn merge_failed(&self) -> SwitchError {
        self.fail(SwitchErrorKind::WriteFailed)
            .with_login(LoginState::Source, TempsLeft::default())
    }

    /// The live files under the locks, for a direct activation: [`SwitchErrorKind::LiveUnreadable`] for a credential
    /// file that exists but cannot be read or is not UTF-8, [`SwitchErrorKind::ConfigUnreadable`] for a `~/.claude.json`
    /// that exists but cannot be read or is not a JSON object.
    fn found(&self) -> Result<Found, SwitchError> {
        let live = found_live(self.context.claude)
            .map_err(|()| self.fail(SwitchErrorKind::LiveUnreadable))?;
        let config = found_config(self.context.claude)
            .map_err(|()| self.fail(SwitchErrorKind::ConfigUnreadable))?;
        Ok(Found { live, config })
    }

    /// Stashes `live`, the credential a direct activation displaces, unless
    /// there is none, it is empty, or it is `target`'s own bytes, sealed with `organization`, the slot's as the roster
    /// already read names it. [`SwitchErrorKind::OutgoingBackupFailed`], with no live file written, when the stash fails.
    fn stash_live(
        &self,
        live: Option<&CredentialBlob>,
        target: &CredentialBlob,
        slot: ConfigSlot,
        organization: &str,
    ) -> Result<(), SwitchError> {
        let Some(live) =
            live.filter(|live| !live.expose().is_empty() && live.expose() != target.expose())
        else {
            return Ok(());
        };
        if stash_displaced(self.context, live, slot, organization).is_err() {
            tracing::warn!(
                account = self.number,
                "the displaced live credential could not be stashed; the switch stops with no live file written"
            );
            return Err(self.fail(SwitchErrorKind::OutgoingBackupFailed));
        }
        tracing::warn!(
            account = self.number,
            unmanaged = slot == ConfigSlot::Unmanaged,
            "the live credential, which no managed account backs up, was stashed before the switch replaced it"
        );
        Ok(())
    }

    /// The live credential's and `~/.claude.json`'s bytes, read under the locks. An empty live credential is refused
    /// rather than written over the outgoing account's backup.
    fn snapshot(&self) -> Result<Snapshot, SwitchError> {
        let live = read_live(self.context.claude)
            .ok_or_else(|| self.fail(SwitchErrorKind::LiveUnreadable))?;
        let config = store::read_optional(self.context.claude.global_config())
            .ok()
            .flatten()
            .ok_or_else(|| self.fail(SwitchErrorKind::ConfigUnreadable))?;
        Ok(Snapshot { live, config })
    }

    /// The first step, by [`Self::classify`]'s answer: the live credential stashed, or backed up into `current`'s
    /// backup when there is none or it is a newer generation than the backup classify read ([`newer_generation`]),
    /// else the stored copy kept (recording the oracle's uuid on an own rotation either way), or left alone, and then,
    /// unless it was stashed, the whole `~/.claude.json` written as `current`'s config backup. Any of these writes that
    /// fails fails the switch before a live file is written.
    fn back_up_outgoing(
        &self,
        lock: &AccountLockGuard,
        current: &AccountRecord,
        classified: &Classified,
        snapshot: &Snapshot,
        warnings: &mut Vec<String>,
    ) -> Result<(), SwitchError> {
        let slot = Slot::new(self.context, current.number);
        let kind = classified.kind;
        match kind {
            Outgoing::Foreign | Outgoing::KnownForeign => {
                if slot
                    .stash_foreign(&snapshot.live, &current.org_uuid)
                    .is_err()
                {
                    tracing::warn!(
                        account = current.number,
                        "the outgoing credential could not be stashed; the switch stops with no live file written"
                    );
                    return Err(self.fail(SwitchErrorKind::OutgoingBackupFailed));
                }
                warnings.push(foreign_warning(kind, current.number));
                return Ok(());
            }
            Outgoing::OwnFamily | Outgoing::OwnRotated | Outgoing::Unresolved => {
                let stored = classified.backup.as_ref();
                if stored.is_some_and(|stored| !newer_generation(&snapshot.live, stored)) {
                    let number = current.number;
                    tracing::info!(
                        "switch: kept account {number}'s stored credential; the outgoing copy is not newer"
                    );
                } else {
                    self.back_up_credential(lock, &slot, current.number, &snapshot.live, warnings)?;
                }
            }
            Outgoing::Wiped => warnings.push(wiped_warning(current.number)),
            Outgoing::OwnBytes => {}
        }
        if kind == Outgoing::OwnRotated {
            self.backfill_uuid(lock, current, &snapshot.live);
        }
        let Some(identity) = oauth_account_of(&snapshot.config) else {
            tracing::warn!(
                account = current.number,
                "the live config names no oauthAccount to back up; the switch stops with no live file written"
            );
            return Err(self.fail(SwitchErrorKind::OutgoingBackupFailed));
        };
        self.context.store.write_account_config(lock, current.number, &identity).map_err(|error| {
            tracing::warn!(
                account = current.number,
                %error,
                "the outgoing account's config backup could not be written; the switch stops with no live file written"
            );
            self.fail(SwitchErrorKind::OutgoingBackupFailed)
        })
    }

    /// Writes `live` as account `number`'s backup. A `.prev` copy of the replaced generation that could not be kept is
    /// a warning; a backup that could not be written fails the switch.
    fn back_up_credential(
        &self,
        lock: &AccountLockGuard,
        slot: &Slot<'_, '_>,
        number: u32,
        live: &CredentialBlob,
        warnings: &mut Vec<String>,
    ) -> Result<(), SwitchError> {
        match slot.write(lock, live) {
            Ok(Retention::Failed(_)) => {
                tracing::warn!(
                    account = number,
                    "the outgoing account's replaced backup could not be kept as its .prev copy"
                );
                warnings.push(format!(
                    "Account-{number}'s previous stored credential could not be kept as its .prev copy; the new backup was written."
                ));
                Ok(())
            }
            Ok(_) => Ok(()),
            Err(error) => {
                tracing::warn!(
                    account = number,
                    %error,
                    "the outgoing account's backup could not be written; the switch stops with no live file written"
                );
                Err(self.fail(SwitchErrorKind::OutgoingBackupFailed))
            }
        }
    }

    /// Records the oracle's uuid on `current` when it has none on an own rotation. Best effort: a
    /// failure is logged and the switch goes on.
    fn backfill_uuid(
        &self,
        lock: &AccountLockGuard,
        current: &AccountRecord,
        live: &CredentialBlob,
    ) {
        let Some(resolved) = self.probe.resolved_for(live) else {
            return;
        };
        if let Err(error) = self.context.store.backfill_uuid(
            lock,
            current.number,
            &resolved.uuid,
            &Identity::of(current),
        ) {
            tracing::warn!(account = current.number, %error, "the outgoing account's uuid could not be recorded; the switch goes on");
        }
    }

    /// The outgoing credential's classification over the oracle verdict: the backup's own bytes or lineage first,
    /// then wiped tokens, then the probe's answer when it was about these bytes, then a remembered verdict. A backup
    /// that exists and cannot be read stops the switch
    /// ([`SwitchErrorKind::OutgoingBackupFailed`]) rather than reading as none and being replaced. The backup as read
    /// here, under the account lock, is handed back beside the kind, for the backup step to compare against.
    fn classify(
        &self,
        current: &AccountRecord,
        live: &CredentialBlob,
    ) -> Result<Classified, SwitchError> {
        let backup = match self.context.store.read_credentials(current.number) {
            Ok(backup) => backup,
            Err(error) => {
                tracing::warn!(
                    account = current.number,
                    %error,
                    "the outgoing account's backup could not be read; the switch stops with nothing written"
                );
                return Err(self.fail(SwitchErrorKind::OutgoingBackupFailed));
            }
        };
        let kind = self.kind_of(current, live, backup.as_ref());
        Ok(Classified { kind, backup })
    }

    /// [`Self::classify`]'s answer for `live` over `backup`, the outgoing account's backup as it read it.
    fn kind_of(
        &self,
        current: &AccountRecord,
        live: &CredentialBlob,
        backup: Option<&CredentialBlob>,
    ) -> Outgoing {
        if let Some(backup) = backup {
            if backup.expose() == live.expose() {
                return Outgoing::OwnBytes;
            }
            if credential_fingerprint(backup) == credential_fingerprint(live) {
                return Outgoing::OwnFamily;
            }
        }
        if OauthShape::of(live).is_some_and(|shape| !shape.access && !shape.refresh) {
            return Outgoing::Wiped;
        }
        match self.probe.resolved_for(live) {
            Some(resolved) => match resolved_matches_slot_identity(current, resolved) {
                Verdict::Same | Verdict::SameWithoutUuid => Outgoing::OwnRotated,
                Verdict::Foreign => Outgoing::Foreign,
                Verdict::Unverifiable => Outgoing::Unresolved,
            },
            None => self.remembered(current, live),
        }
    }

    /// With no answer from the oracle: [`Outgoing::KnownForeign`] when this process already found the lineage to be
    /// another account's, else [`Outgoing::Unresolved`].
    fn remembered(&self, current: &AccountRecord, live: &CredentialBlob) -> Outgoing {
        // An empty credential's lineage is keyed by the empty fingerprint.
        let fingerprint = credential_fingerprint(live).unwrap_or_default();
        let key = LineageKey::new(&current.email, current, &fingerprint);
        if self.context.verdicts.get(&key) == Some(false) {
            Outgoing::KnownForeign
        } else {
            Outgoing::Unresolved
        }
    }

    /// Whether the target's credential and config backup files are there, the config backup names the target's login
    /// and the stored credential opens, so that a switch that cannot finish writes no backup, `.prev` copy or stash
    /// row: [`SwitchErrorKind::TargetBackupMissing`] when one is not a regular file (a symbolic link counts as
    /// missing). The config backup's `oauthAccount` must name the login the record does ([`backs_record`]), else
    /// [`SwitchErrorKind::TargetIdentityMismatch`]; a stored credential that cannot be read or opened is
    /// [`SwitchErrorKind::TargetBackupUnreadable`]. [`Self::incoming`] reads them again for the write.
    fn require_target_backup(&self, target: &AccountRecord) -> Result<(), SwitchError> {
        let store = self.context.store;
        let root = store.root();
        if !(store::is_plain_file(&root.credential_path(target.number))
            && store::is_plain_file(&root.config_path(target.number)))
        {
            return Err(self.fail(SwitchErrorKind::TargetBackupMissing));
        }
        target_identity_check(store, self.number, target)
            .inspect_err(|&kind| log_identity_refusal(self.number, kind))
            .map_err(|kind| self.fail(kind))?;
        store
            .read_credentials(target.number)
            .map(drop)
            .map_err(|_| self.fail(SwitchErrorKind::TargetBackupUnreadable))
    }

    /// The target's stored credential and its config backup's `oauthAccount`, which must be there and truthy. The
    /// backup's tree is zeroed as it drops.
    fn incoming(&self, target: &AccountRecord) -> Result<Incoming, SwitchError> {
        let missing = || self.fail(SwitchErrorKind::TargetBackupMissing);
        let unreadable = |_: StoreError| self.fail(SwitchErrorKind::TargetBackupUnreadable);
        let backups = self.context.store;
        let credentials = backups
            .read_credentials(target.number)
            .map_err(unreadable)?
            .ok_or_else(missing)?;
        let config = backups
            .read_account_config(target.number)
            .map_err(unreadable)?
            .ok_or_else(missing)?;
        let identity = config.into_scrubbed();
        let oauth_account = Scrubbed(identity.0.clone());
        if !store::truthy(&oauth_account.0) {
            return Err(missing());
        }
        Ok(Incoming {
            credentials,
            oauth_account,
        })
    }

    /// The writes that make the target live, in order, noting each one done and each temp file a failed one
    /// left: the credential file, then `~/.claude.json` as `edit` changes it (the `oauthAccount` splice), each only while
    /// it still holds what `before` says the switch found there, then `accounts.json`'s active account. A live file
    /// changed since the snapshot is [`ClaudeFileError::Changed`], a failed write like any other. `Err` at the first
    /// that fails.
    fn activate<'w>(
        &self,
        held: &Held<'_>,
        merged: &'w CredentialBlob,
        before: Before<'_>,
        edit: impl FnOnce(&mut Map<String, Value>),
        written: &mut Written<'w>,
    ) -> Result<(), ()> {
        let claude = self.context.claude;
        rewrite_live_credentials(claude, held.claude, before.credentials.expected(), merged)
            .map_err(|error| {
                self.failed_live_write(
                    "the live credential file",
                    &error,
                    &mut written.temps.credentials,
                );
            })?;
        written.credentials = Some(merged.expose().as_bytes());
        let config = rewrite_global_config(claude, held.config, before.config.expected(), edit)
            .map_err(|error| {
                self.failed_live_write("the global config", &error, &mut written.temps.config);
            })?;
        written.config = Some(config);
        self.context.store.set_active(held.lock, self.number).map_err(|error| {
            tracing::warn!(account = self.number, %error, "accounts.json could not be written; the live files are rolled back");
        })?;
        Ok(())
    }

    fn failed_live_write(&self, file: &'static str, error: &ClaudeFileError, temp_left: &mut bool) {
        *temp_left = matches!(error, ClaudeFileError::WriteLeftTemp(_));
        tracing::warn!(account = self.number, file, %error, "a switch write failed; the live files are rolled back");
    }

    /// Puts back the live files the switch wrote, `~/.claude.json` first, through byte-exact writers and under the
    /// locks still held, and only while each still holds the bytes the switch wrote: an earlier file's bytes are
    /// restored over it, and a file that was not there before is removed. A file the switch wrote that was rewritten
    /// since, whether it would be restored or removed, is left in place and counts as not put back. When `~/.claude.json` is not put back the
    /// credential file is left on the target too, so the live pair stays one account's. [`SwitchErrorKind::WriteFailed`]
    /// when all are back, [`SwitchErrorKind::RollbackFailed`] with the login's state when one is not.
    fn roll_back(&self, held: &Held<'_>, before: Before<'_>, written: &Written<'_>) -> SwitchError {
        let claude = self.context.claude;
        let mut temps = written.temps;
        let config_back = match (written.config.as_deref(), before.config) {
            (None, _) => Ok(true),
            (Some(ours), Prior::Bytes(bytes)) => {
                restore_global_config(claude, held.config, ours, bytes)
            }
            (Some(ours), Prior::Absent) => remove_global_config(claude, held.config, ours),
        };
        if let Err(cause) = self.settle("the global config", config_back, &mut temps.config) {
            return self.rollback_failed(LoginState::Target, temps, &cause);
        }
        let credentials_back = match (written.credentials, before.credentials) {
            (None, _) => Ok(true),
            (Some(ours), Prior::Bytes(bytes)) => {
                restore_live_credentials(claude, held.claude, ours, bytes)
            }
            (Some(ours), Prior::Absent) => remove_live_credentials(claude, held.claude, ours),
        };
        if let Err(cause) = self.settle(
            "the live credential file",
            credentials_back,
            &mut temps.credentials,
        ) {
            return self.rollback_failed(LoginState::Mixed, temps, &cause);
        }
        self.log_temps(temps);
        tracing::warn!(
            account = self.number,
            "the switch failed and was rolled back; the live login is unchanged"
        );
        self.fail(SwitchErrorKind::WriteFailed)
            .with_login(LoginState::Source, temps)
    }

    /// One live file's rollback, `Ok(true)` when it is back as it was: [`NotPutBack::Failed`] when the write or the
    /// removal failed, noting a temp file it left in `temp_left`; [`NotPutBack::Changed`], with a warning naming the
    /// file, when a file the switch wrote (one it would restore as well as one it would remove) holds other bytes now
    /// and was left in place.
    fn settle(
        &self,
        file: &'static str,
        result: Result<bool, ClaudeFileError>,
        temp_left: &mut bool,
    ) -> Result<(), NotPutBack> {
        match result {
            Ok(true) => Ok(()),
            Ok(false) => {
                tracing::warn!(
                    account = self.number,
                    file,
                    "a live file the switch wrote was rewritten before the rollback and was left in place"
                );
                Err(NotPutBack::Changed)
            }
            Err(error) => {
                *temp_left |= matches!(error, ClaudeFileError::WriteLeftTemp(_));
                Err(NotPutBack::Failed(error))
            }
        }
    }

    fn rollback_failed(
        &self,
        login: LoginState,
        temps: TempsLeft,
        error: &NotPutBack,
    ) -> SwitchError {
        self.log_temps(temps);
        tracing::error!(
            account = self.number,
            login = login.as_str(),
            %error,
            "the switch failed and rolling it back failed too; Claude Code's live login may need repair"
        );
        self.fail(SwitchErrorKind::RollbackFailed)
            .with_login(login, temps)
    }

    fn log_temps(&self, temps: TempsLeft) {
        if temps.credentials || temps.config {
            tracing::warn!(
                account = self.number,
                credentials = temps.credentials,
                config = temps.config,
                "a failed switch write left its temp file, which holds the bytes it was writing, beside the live file"
            );
        }
    }
}

/// `~/.claude.json` with the target's `oauthAccount` in place of the live one and `primaryApiKey` gone, every other key
/// left as it was: a managed API key left beside the login would be billed instead. A file there was not starts empty,
/// so it is created holding the identity alone. The replaced values are zeroed.
fn splice(config: &mut Map<String, Value>, oauth_account: &mut Scrubbed) {
    if let Some(mut old) = config.insert(
        "oauthAccount".to_owned(),
        std::mem::take(&mut oauth_account.0),
    ) {
        scrub(&mut old);
    }
    if let Some(mut key) = config.shift_remove("primaryApiKey") {
        scrub(&mut key);
    }
}

/// The outcome of a switch that made `target` live, leaving `from`, the live login written where `paths` keeps it.
fn switched(
    paths: &ClaudePaths,
    from: Option<SwitchAccount>,
    target: &AccountRecord,
    warnings: Vec<String>,
) -> SwitchOutcome {
    SwitchOutcome {
        switched: true,
        from,
        to: SwitchAccount::of(target),
        strategy: STRATEGY,
        reason: "switched",
        message: format!(
            "Switched to Account-{} ({})",
            target.number,
            target.email.as_str()
        ),
        warnings,
        takes_effect_within: takes_effect_within(paths),
    }
}

/// The login a direct activation displaced: the live identity's email, with no account number. `None` when
/// `~/.claude.json` named no login, or its email is not one an account could have.
fn displaced_from(identity: Option<&Identity>) -> Option<SwitchAccount> {
    let email = Email::parse(&identity?.email).ok()?;
    Some(SwitchAccount {
        number: None,
        email,
    })
}

/// The live credential file's text, `None` when the file does not exist; `Err` when it exists but cannot be read or is
/// not UTF-8.
fn found_live(paths: &ClaudePaths) -> Result<Option<CredentialBlob>, ()> {
    let Some(bytes) = read_live_credentials(paths).map_err(|_| ())? else {
        return Ok(None);
    };
    let text = std::str::from_utf8(&bytes).map_err(|_| ())?;
    Ok(Some(CredentialBlob::new(text.to_owned())))
}

/// `~/.claude.json` read once, `None` when the file does not exist; `Err` when it exists but cannot be read or is not a
/// JSON object. The parsed tree is zeroed as it drops; only the login it names and whether it holds an API key are kept.
fn found_config(paths: &ClaudePaths) -> Result<Option<FoundConfig>, ()> {
    let Some(bytes) = store::read_optional(paths.global_config()).map_err(|_| ())? else {
        return Ok(None);
    };
    let tree = Scrubbed(serde_json::from_slice(&bytes).map_err(|_| ())?);
    let config = tree.0.as_object().ok_or(())?;
    let identity = config.get("oauthAccount").and_then(identity_in);
    let api_key = config.get("primaryApiKey").is_some_and(store::truthy);
    Ok(Some(FoundConfig {
        bytes,
        identity,
        api_key,
    }))
}

/// Whether `config`, `~/.claude.json`'s bytes, parses to an object holding a truthy `primaryApiKey`; `false` when it
/// does not parse. The parsed tree is zeroed as it drops.
fn holds_api_key(config: &[u8]) -> bool {
    let Ok(tree) = serde_json::from_slice(config).map(Scrubbed) else {
        return false;
    };
    tree.0.get("primaryApiKey").is_some_and(store::truthy)
}

/// Where the credential a direct activation displaces is filed: under `accounts.json`'s active account when
/// `~/.claude.json` names no login, else, or when there is no active account, under `unmanaged`; with the organization
/// `sequence` records for that account, empty for `unmanaged` or an account it has none for.
fn displaced_slot(identity: Option<&Identity>, sequence: &Sequence) -> (ConfigSlot, String) {
    match (identity, sequence.active()) {
        // Even when the active number is an API-key record: the row is inert, since only a switchable
        // account's refresh adopts from the stash.
        (None, Some(active)) => (
            ConfigSlot::Account(active),
            sequence
                .account(active)
                .map_or_else(String::new, |record| record.org_uuid),
        ),
        _ => (ConfigSlot::Unmanaged, String::new()),
    }
}

/// The warning for an outgoing credential stashed rather than backed up.
fn foreign_warning(kind: Outgoing, number: u32) -> String {
    if kind == Outgoing::KnownForeign {
        format!(
            "The live credential was previously identified as another account's. It was preserved and not written into \
             Account-{number}. If the owning account later cannot authenticate, log in as it and add it in Settings > Accounts"
        )
    } else {
        format!(
            "Credential ownership mismatch detected. The live credential was preserved and was not written into Account-{number}. \
             If the account it belongs to later cannot authenticate, log in as it and add it in Settings > Accounts"
        )
    }
}

/// The warning for an outgoing credential whose tokens were wiped.
fn wiped_warning(number: u32) -> String {
    format!(
        "The live credential's tokens were wiped (Claude Code clears them when a refresh is rejected). Account-{number}'s stored \
         backup was kept. If the account cannot authenticate after switching back, log in with Claude Code and add it in \
         Settings > Accounts"
    )
}

/// The credential to make live: `target` with the machine-shared keys
/// ([`SHARED_KEYS`]) taken from `live`, absence included. The target's other keys keep their order, and the live shared
/// keys follow in the live file's order. `target` unchanged when `live` is not a JSON object, or `target` is not a JSON
/// object with a `claudeAiOauth` key; `None` only when the merged object could not be written out.
fn merge_shared(target: &CredentialBlob, live: &CredentialBlob) -> Option<CredentialBlob> {
    let Some((mut merged, shared)) = mergeable(target, live) else {
        return Some(copy(target));
    };
    if let (Some(map), Some(shared)) = (merged.0.as_object_mut(), shared.0.as_object()) {
        for key in SHARED_KEYS {
            if let Some(mut stale) = map.shift_remove(key) {
                scrub(&mut stale);
            }
        }
        for (key, value) in shared
            .iter()
            .filter(|(key, _)| SHARED_KEYS.contains(&key.as_str()))
        {
            map.insert(key.clone(), value.clone());
        }
    }
    merged.to_blob(target.expose().len() + live.expose().len())
}

/// `target` and `live` parsed, when the merge applies: `live` is a JSON object and `target` a JSON object with a
/// `claudeAiOauth` key.
fn mergeable(target: &CredentialBlob, live: &CredentialBlob) -> Option<(Scrubbed, Scrubbed)> {
    let shared = Scrubbed::parse(live.expose()).filter(|tree| tree.0.is_object())?;
    let merged =
        Scrubbed::parse(target.expose()).filter(|tree| tree.0.get("claudeAiOauth").is_some())?;
    Some((merged, shared))
}
