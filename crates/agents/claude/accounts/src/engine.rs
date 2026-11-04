//! The auto-switch engine's shared files and the usage reading it decides on.
//!
//! [`AutoswitchState`] is `autoswitch_state.json` (the last switch and the quarantined accounts), which every
//! engine on an account store reads and writes under `.autoswitch_state.lock`, keeping the keys it does not know. The
//! [`Lease`] in `engine_lease.json` names the one engine that is live on the root, so a second one stands by.
//! [`next_delay`] and [`sleep_event`] say how long a tick waits before the next one. [`decision_value`] is the usage
//! reading a switch decision may trust; [`collect_warmup`] and [`collect_scheduled`] collect the readings a decision
//! runs on, [`collect_refetch`] re-reads consume-first's members for its phase 2, and [`collect_recheck`] re-reads an
//! exit target with its stored token only, spending no grant. [`Engine::tick`] runs one
//! decision and reports it as [`Event`]s and a [`TickOutcome`].

mod cadence;
mod collect;
mod event;
mod freshen;
mod lease;
mod paid;
mod perform;
mod rank;
mod reading;
mod state;
mod tick;
mod trigger;

use std::io;
use std::path::Path;

use zeroize::Zeroizing;

use crate::locks::LockError;
use crate::paths::AccountsRoot;
use crate::store::{self, StoreErrorKind};

pub use crate::poll::URGENT_INTERVAL_S;
pub use cadence::{next_delay, sleep_event};
pub use collect::{
    Collected, ScheduleInputs, WarmUp, collect_recheck, collect_refetch, collect_scheduled,
    collect_warmup, due_candidate, polled_since, warm_settled,
};
pub use event::{AccountRef, EVENT_SCHEMA_VERSION, Event, EventBody};
pub use freshen::{
    FRESHEN_BUFFER, Freshened, IDENTITY_CONFLICT, IDENTITY_MISMATCH, IdentityCheck, ReleaseReason,
    Released, freshen_target, note_token_identity, quarantine, release_recovered_quarantines,
};
pub use lease::{LEASE_SCHEMA_VERSION, Lease, LeaseHolder, LeaseState};
pub use paid::{
    Ineligible, PAID_BALANCE_MAX_AGE_S, PAID_STAY_READ_S, PaidBalance, PaidCandidate,
    paid_eligible, paid_ranking,
};
pub use perform::{LiveSwitch, SwitchFailure, SwitchFn, SwitchReport, WhileOn};
pub use rank::{
    AccountUsage, CandidateAccount, Candidates, HORIZON_HEADROOM_RATIO, RECOVERY_HORIZON_S,
    RECOVERY_HYSTERESIS_S, RankInputs, Ranking, SPENT_HEADROOM_PCT, TrustedReadings, WindowSource,
    all_above_threshold, binding_recovery, candidates, left_account_recovered, no_return_account,
    rank, rank_candidates,
};
pub(crate) use reading::relevant_windows;
pub use reading::{decision_value, parse_model_names, scoped_window_counts};
pub use state::{
    AutoswitchState, LeaseOwner, OwnerExit, PaidFallback, QuarantineEntry, QuarantineRecord,
    STATE_SCHEMA_VERSION,
};
pub use tick::{
    AUTO_SWITCH_OFF, Collector, Engine, MAX_SLEEP_S, NO_RESET_FALLBACK_S, RESET_SLACK_S,
    TickInputs, TickOutcome, TickReport, TransportCollector,
};
pub use trigger::{
    ActiveHealth, ActiveReading, Classification, Hold, IDLE_HOLD_MAX_S, Trigger, TriggerDecision,
    classify_trigger, in_cooldown,
};

/// Why an engine file was not read or written.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EngineError {
    /// The file's lock was not taken: held past the timeout, not openable, or unsupported off 64-bit Windows.
    #[error("the auto-switch engine file's lock was not taken")]
    Lock(#[source] LockError),
    /// The file exists but could not be read; it is left as it is.
    #[error("the auto-switch engine file exists but could not be read; it was left unchanged")]
    Read(#[source] io::Error),
    /// The file could not be written or removed; the old file is unchanged.
    #[error(
        "the auto-switch engine file could not be written or removed; the old file is unchanged"
    )]
    Write(#[source] io::Error),
    /// `accounts.json` refuses writes to the store (a newer or unknown `schemaVersion`, or a file that cannot be read
    /// or parsed), so the engine file was not written.
    #[error(
        "the account store refuses writes ({0:?}), so the auto-switch engine file was left unchanged"
    )]
    StoreRefused(StoreErrorKind),
    /// The engine file carries a `schemaVersion` this crate does not know: it is read, never written.
    #[error(
        "the auto-switch engine file was written by a newer or unknown version of the account store, so it was left unchanged"
    )]
    UnknownVersion,
}

/// The bytes of the file at `path`, `None` when it does not exist, with transient sharing errors retried; a file that
/// is a symbolic link or junction is [`EngineError::Read`].
fn read_file(path: &Path) -> Result<Option<Zeroizing<Vec<u8>>>, EngineError> {
    store::read_optional_unlinked(path).map_err(EngineError::Read)
}

/// [`EngineError::StoreRefused`] when `accounts.json` of `root` refuses writes ([`store::version_refusal`]); checked
/// under the engine file's own lock, before its first byte changes.
fn store_refusal(root: &AccountsRoot) -> Result<(), EngineError> {
    store::version_refusal(root).map_or(Ok(()), |kind| Err(EngineError::StoreRefused(kind)))
}
