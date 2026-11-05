//! rustling-tulip's account store, read and written in place, and the locks and secret types that guard it.
//!
//! The store's root is the folder the caller supplies ([`AccountsRoot`]): `accounts.json` lists the
//! accounts, their rotation order, the active one and the next account number, `configs/<n>.json` holds each account's
//! `oauthAccount` alone and `credentials/<n>.cred` each account's sealed credential file. [`Store`] reads that root and
//! writes it, keeping every key it does not know, and refuses every write while sealing does not work in this process
//! (the canary fails) or `accounts.json` carries a `schemaVersion` it does not know. Writes take the account lock as a
//! parameter; [`ClaudeCodeLocks`] takes Claude Code's own directory locks. The locks work on 64-bit Windows and macOS;
//! elsewhere every lock returns [`LockError::Unsupported`], so no store write runs there. Credentials stay inside [`CredentialBlob`],
//! [`AccessToken`] and [`RefreshToken`], which never print, clone or serialize. The crate reads no environment
//! variable: the caller says where the account store and Claude Code's files are.
//!
//! [`UsageTable`] reads the store's usage cache, `cache/usage.json`, and [`poll_once`] polls the usage endpoint
//! within the request budget recorded in it, over a [`UsageTransport`] the caller implements: the crate has no
//! HTTP client.
//!
//! [`consume_backup_grant`] spends a backup account's one-time refresh grant through the consume gate, over a
//! [`RefreshTransport`] the caller implements, and parks a successor it cannot write in the unclaimed stash.
//!
//! [`engine`] holds the auto-switch engine's shared files: the [`AutoswitchState`] and the [`Lease`] that keeps one
//! engine live per account store, and the [`decision_value`] a switch decision trusts.
//!
//! [`pace`] says when a usage window runs out at the rate it has been used, and whether it lasts to its reset.
//!
//! [`seal`] turns a [`CredentialBlob`] into a file's bytes sealed to this Windows user's DPAPI keys and bound to an
//! account slot, and opens them again; off Windows it stores them unsealed.
//!
//! [`guard`] makes folders only the current user can reach, and checks a folder or file's owner, its write access and
//! that it is no link before a credential file is read.
//!
//! [`switch`] makes another managed account Claude Code's live login under the store's and Claude Code's locks, after
//! [`probe_outgoing`] has asked the profile oracle, before any lock, whose the outgoing credential is.
//!
//! [`Settings`] reads and writes the store's settings file, `settings.json`: the auto-switch engine's settings and the
//! fleet verdict's, strictly checked.
//!
//! [`capture_live_login`] adds Claude Code's live login to the store, or writes it into its account when the live copy
//! is later, once the profile oracle has confirmed whose it is; [`Store::add_accounts`] is the store's one way to add
//! accounts.

mod capture;
mod claude_files;
pub mod drain;
pub mod engine;
pub mod guard;
mod keychain;
mod locks;
pub mod pace;
mod paths;
mod poll;
mod refresh;
pub mod seal;
mod secret;
mod settings;
mod store;
mod switch;
mod transport;
mod usage;
#[cfg(all(windows, target_pointer_width = "64"))]
mod win32;

pub use capture::{CaptureError, CaptureKind, CaptureOutcome, capture_live_login};
pub use claude_files::{
    ClaudeFileError, clear_primary_api_key, delete_live_keychain_item, update_global_config,
    write_live_credentials,
};
pub use drain::{Drain, Draining, WorkGuard};
pub use engine::{
    AutoswitchState, EngineError, Ineligible, Lease, LeaseHolder, LeaseOwner, LeaseState,
    OwnerExit, PAID_BALANCE_MAX_AGE_S, PAID_STAY_READ_S, PaidBalance, PaidCandidate, PaidFallback,
    QuarantineEntry, QuarantineRecord, STATE_SCHEMA_VERSION, decision_value, paid_eligible,
    paid_ranking, parse_model_names, scoped_window_counts,
};
#[cfg(unix)]
pub use keychain::os_user_name;
pub use locks::{
    ACCOUNT_LOCK_TIMEOUT, AccountLock, AccountLockGuard, CLAUDE_LOCK_TIMEOUT, CONFIG_STALENESS,
    CREDENTIALS_STALENESS, ClaudeCodeLocks, ClaudeCredentialsGuard, ConsumeLock, ConsumeLockGuard,
    DirLock, DirLockGuard, FileLock, FileLockGuard, LockError, LockTiming, TOUCH_INTERVAL,
};
pub use paths::{AccountsRoot, ClaudePaths, Email, LiveStore};
pub use poll::{
    BalanceReadError, CLAIM_TTL_S, Claims, DeferReason, FetchError, FetchMode, FetchResult,
    FetchScope, PlanContext, PlanRequest, PollError, PollOutcome, PollPlan, PollResult,
    SERVE_TTL_S, backoff_seconds, fetch_usage, plan_after_fetch, poll_once, poll_once_with_refresh,
    poll_with_refresh,
};
pub use refresh::oracle::ProbeVerdicts;
pub use refresh::stash::{DiscardError, HeldEntry, HeldReason, discard_held, held_entries};
pub use refresh::{
    GrantError, GrantOutcome, RefreshContext, RefreshError, RefreshedGrant, StashReason,
    TokenAccount, access_token_fingerprint, consume_backup_grant, credential_fingerprint,
    newer_generation, refresh_grant,
};
pub use secret::{
    AccessToken, CredentialBlob, OAUTH_EXPIRY_BUFFER, OauthCredentials, RefreshToken, Secret,
};
pub use settings::{
    AutoSwitchSection, FleetSettings, RateWindow, Settings, SettingsError, StrategyName,
};
pub use store::{
    AccountConfig, AccountKind, AccountNumber, AccountRecord, AutoSwitchSettings, MAX_ALIAS_CHARS,
    NewAccount, Retention, SealCanary, Sequence, Store, StoreError, StoreErrorKind, Strategy,
    default_canary,
};
pub use switch::{
    KEYCHAIN_CACHE, LoginState, OutgoingProbe, SwitchAccount, SwitchError, SwitchErrorKind,
    SwitchOutcome, SwitchPrecondition, TempsLeft, probe_outgoing, switch,
};
pub use transport::{
    CLIENT_ID, LOCKED_REFRESH_TIMEOUT, PREPAID_URL_TEMPLATE, PROFILE_TIMEOUT, PROFILE_URL,
    PROFILE_USER_AGENT, ProfileTransport, REFRESH_TIMEOUT, RawProfile, RawToken, RawUsage,
    RefreshRequest, RefreshTransport, TOKEN_URL, TransportError, USAGE_BETA_HEADER, USAGE_TIMEOUT,
    USAGE_URL, UsageTransport, prepaid_url,
};
pub use usage::{
    Identity, PrepaidEntry, USAGE_SCHEMA_VERSION, UsageEntry, UsageTable, UsageWindow,
};
