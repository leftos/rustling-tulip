//! Making the switch a tick decided on: under the state file's lock, the cooldown is checked again
//! (another engine may have switched since the tick read the state), the target's quarantine and the engine lease are
//! checked again, the switch runs, and a switch that moved the live login is recorded with the departure snapshot of
//! the account it left. Nothing else writes the state file. The switch itself goes through [`SwitchFn`], which the
//! server fills with [`LiveSwitch`] and the tests with a fake.

use jiff::Timestamp;
use proc_table::ProcessTable;

use crate::engine::paid::balance_still_spendable;
use crate::engine::{
    AccountRef, AutoswitchState, EngineError, Lease, LeaseHolder, LeaseOwner, LeaseState,
    PaidBalance, PaidFallback, Trigger, in_cooldown,
};
use crate::locks::ACCOUNT_LOCK_TIMEOUT;
use crate::refresh::RefreshContext;
use crate::store::AutoSwitchSettings;
use crate::switch::{
    LoginState, SwitchAccount, SwitchError, SwitchErrorKind, SwitchOutcome, SwitchPrecondition,
    probe_outgoing, switch,
};
use crate::usage::{PrepaidEntry, secs_from_timestamp};

/// What a switch the engine asked for came to, when it did not fail: [`crate::SwitchOutcome`] as the engine reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwitchReport {
    /// Whether the live login moved; `false` when the target already was the live login.
    pub switched: bool,
    /// The account the live login was.
    pub from: Option<AccountRef>,
    /// The account the live login is now.
    pub to: AccountRef,
    /// `switched`, or `already-active`.
    pub reason: String,
    /// What the switch warned of.
    pub warnings: Vec<String>,
}

impl From<SwitchOutcome> for SwitchReport {
    fn from(outcome: SwitchOutcome) -> Self {
        Self {
            switched: outcome.switched,
            from: outcome.from.as_ref().map(account_ref),
            to: account_ref(&outcome.to),
            reason: outcome.reason.to_owned(),
            warnings: outcome.warnings,
        }
    }
}

fn account_ref(account: &SwitchAccount) -> AccountRef {
    AccountRef::new(account.number, account.email.as_str())
}

/// A switch the engine asked for that failed: why, and, when a live file was written, where the live login was left.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SwitchFailure {
    /// Why the switch failed.
    pub kind: SwitchErrorKind,
    /// Where the live login was left, as [`SwitchError::login_state`] says.
    pub login: Option<LoginState>,
}

impl From<SwitchError> for SwitchFailure {
    fn from(error: SwitchError) -> Self {
        Self {
            kind: error.kind(),
            login: error.login_state(),
        }
    }
}

/// How the engine switches Claude Code's live login to a managed account.
pub trait SwitchFn {
    /// Switches the live login to account `number`, only while `precondition` holds when one is given (as
    /// [`crate::switch`] checks it under every lock).
    ///
    /// # Errors
    ///
    /// A [`SwitchFailure`] saying why the switch did not happen or did not finish;
    /// [`SwitchErrorKind::PreconditionFailed`] when `precondition` no longer holds.
    fn switch(
        &self,
        context: &RefreshContext<'_>,
        number: u32,
        precondition: Option<SwitchPrecondition>,
    ) -> Result<SwitchReport, SwitchFailure>;

    /// Runs `write`, a record write that only an engine with auto-switch on may make (the paid fallback's entry in
    /// place), only while auto-switch is on as this switch judges it: the same check, under the same guard, as
    /// [`Self::switch`] makes. It never waits for that guard: a guard already held answers [`WhileOn::Busy`] at once.
    /// The engine calls it with the state file's lock held.
    fn write_while_on(&self, write: &mut dyn FnMut()) -> WhileOn;
}

/// What [`SwitchFn::write_while_on`] came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WhileOn {
    /// Auto-switch is on: the write ran.
    Ran,
    /// Auto-switch is off: nothing was written.
    Off,
    /// What the check needs is held by a switch in flight: nothing was written, and the engine tries again next tick.
    Busy,
}

/// The crate's own switch: [`probe_outgoing`], then [`switch`].
#[derive(Clone, Copy, Debug, Default)]
pub struct LiveSwitch;

impl SwitchFn for LiveSwitch {
    fn switch(
        &self,
        context: &RefreshContext<'_>,
        number: u32,
        precondition: Option<SwitchPrecondition>,
    ) -> Result<SwitchReport, SwitchFailure> {
        let probe = probe_outgoing(context);
        switch(context, number, &probe, precondition)
            .map(SwitchReport::from)
            .map_err(SwitchFailure::from)
    }

    /// The crate's switch has no setting of its own: the write runs.
    fn write_while_on(&self, write: &mut dyn FnMut()) -> WhileOn {
        write();
        WhileOn::Ran
    }
}

/// The account a switch leaves, as the ranking saw it: its headroom and when its binding window resets.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct LeftSnapshot {
    /// The left account's headroom, `None` when unknown.
    pub(crate) headroom: Option<f64>,
    /// When its binding window resets, `None` when the reset is unknown or past, which the ranking treats as infinitely far
    /// (written as null).
    pub(crate) recovery: Option<Timestamp>,
}

/// A switch to make: the account, why, the departure snapshot, the settings the tick read, the engine's lease holder,
/// the process table the lease's holder is looked up in, and the last switch the engine made that the state file may
/// not hold.
pub(crate) struct PerformRequest<'a> {
    pub(crate) context: &'a RefreshContext<'a>,
    pub(crate) switcher: &'a dyn SwitchFn,
    pub(crate) number: u32,
    pub(crate) trigger: Trigger,
    pub(crate) left: LeftSnapshot,
    pub(crate) settings: &'a AutoSwitchSettings,
    pub(crate) holder: &'a LeaseHolder,
    pub(crate) processes: &'a dyn ProcessTable,
    /// When this engine last switched without recording it (seconds since the epoch).
    pub(crate) remembered_switch_at: Option<f64>,
    /// What the switch must find still true under its locks; `None` for a move that needs nothing.
    pub(crate) precondition: Option<SwitchPrecondition>,
    /// The paid fallback this move enters; `None` for a move that spends no credits, which clears `paidFallback`.
    pub(crate) paid: Option<PaidMove>,
}

/// A move onto an account's prepaid credits: the account the login leaves, the balance it was decided on, and the
/// engine that makes it.
#[derive(Clone, Debug)]
pub(crate) struct PaidMove {
    /// The account the login is expected to leave.
    pub(crate) from: u32,
    /// The account the paid fallback was first entered from, recorded as its `from`: `from` itself on an entry, the
    /// original entry's on a move between paid accounts.
    pub(crate) entered_from: u32,
    /// The target's balance as the decision read it, judged again under the state lock.
    pub(crate) prepaid: PrepaidEntry,
    /// The same balance in major units, for the entered event.
    pub(crate) balance: PaidBalance,
    /// The engine making the move, recorded as the paid fallback's `leaseOwner`.
    pub(crate) lease_owner: LeaseOwner,
}

/// What [`perform`] came to.
#[derive(Debug)]
pub(crate) enum Performed {
    /// The live login moved, and the switch is recorded.
    Switched(SwitchReport),
    /// The live login moved, but recording it in the state file failed.
    Unrecorded(SwitchReport, EngineError),
    /// Another engine switched within the cooldown since the tick read the state: nothing was switched.
    Cooldown,
    /// The target was quarantined since the tick read the state: nothing was switched.
    Quarantined,
    /// The engine lease is not this engine's any more, as it reads under the state lock: nothing was switched.
    LeaseLost(LeaseState),
    /// The target already was the live login, with the switch's reason.
    AlreadyActive(String),
    /// The switch's precondition no longer held under its locks: nothing was switched, and the target is skipped.
    PreconditionFailed,
    /// The switch failed.
    Failed(SwitchFailure),
    /// The state file's lock was not taken or the file could not be read, or the engine lease could not be read under
    /// it, as the message says: nothing was switched.
    Unavailable(String),
    /// The state file, as it reads under its lock, is of a newer or unknown version, so the switch could not be
    /// recorded: [`EngineError::UnknownVersion`], before anything was switched.
    Refused(EngineError),
}

/// The switch: under the state file's lock, the re-checks, the switch and the record of a switch that moved
/// the live login (`lastSwitchAt`, `lastSwitchTo`, `lastSwitchFrom`, the departure snapshot, `leftTrigger`, and
/// `paidFallback`, set by a paid move and cleared by any other). Only
/// that record writes the file: every other outcome leaves it as it was.
pub(crate) fn perform(request: &PerformRequest<'_>) -> Performed {
    let mut recorded = None;
    let written = AutoswitchState::update_if(
        request.context.store.root(),
        ACCOUNT_LOCK_TIMEOUT,
        |state| {
            let (performed, switched) = switch_locked(request, state);
            if let Performed::Switched(report) = &performed {
                recorded = Some(report.clone());
            }
            (performed, switched)
        },
    );
    match written {
        Ok(performed) => performed,
        Err(error) => match recorded {
            Some(report) => Performed::Unrecorded(report, error),
            None => Performed::Unavailable(format!(
                "the auto-switch state file could not be locked or read: {error}"
            )),
        },
    }
}

/// The re-checks, the switch and the record, with the state file's lock held and `state` as it reads under it; with
/// whether the state is to be written. A state of a newer or unknown version is refused first, since its switch could
/// never be recorded. The lease lock is taken inside the state lock, never the other way round.
fn switch_locked(request: &PerformRequest<'_>, state: &mut AutoswitchState) -> (Performed, bool) {
    if let Err(error) = state.refuse_unknown_version() {
        return (Performed::Refused(error), false);
    }
    let context = request.context;
    if let Some(refused) = recheck(request, state, (context.clock)()) {
        return (refused, false);
    }
    let report = match request
        .switcher
        .switch(context, request.number, request.precondition)
    {
        Ok(report) => report,
        Err(failure) if failure.kind == SwitchErrorKind::PreconditionFailed => {
            return (Performed::PreconditionFailed, false);
        }
        Err(failure) => return (Performed::Failed(failure), false),
    };
    if !report.switched {
        return (Performed::AlreadyActive(report.reason), false);
    }
    let at = secs_from_timestamp((context.clock)());
    state.last_switch_at = Some(at);
    state.last_switch_to = Some(request.number.to_string());
    state.last_switch_from = report.from.as_ref().and_then(|from| from.number);
    state.left_headroom = Some(request.left.headroom);
    state.left_recovery_at = Some(request.left.recovery.map(secs_from_timestamp));
    state.left_trigger = Some(request.trigger.as_str().to_owned());
    state.paid_fallback = match &request.paid {
        Some(paid) => Some(PaidFallback::new(
            request.number,
            paid.entered_from,
            at,
            paid.lease_owner.clone(),
        )),
        // An owner-exit bar outlives a normal switch: only a refill or an account gained by the opted-in set lifts it.
        None => state
            .paid_fallback
            .take()
            .filter(|paid| paid.owner_exit.is_some()),
    };
    (Performed::Switched(report), true)
}

/// The re-checks under the state lock before the switch, at `now`: the cooldown, the target's quarantine, the engine
/// lease, and a paid move's balance (still positive and at most [`crate::engine::PAID_BALANCE_MAX_AGE_S`] old); the
/// refusal, or `None` to go on.
fn recheck(
    request: &PerformRequest<'_>,
    state: &AutoswitchState,
    now: Timestamp,
) -> Option<Performed> {
    if cooling(
        request.trigger,
        state,
        request.remembered_switch_at,
        request.settings,
        now,
    ) {
        return Some(Performed::Cooldown);
    }
    if state.quarantine.contains_key(&request.number.to_string()) {
        return Some(Performed::Quarantined);
    }
    match Lease::new(request.context.store.root()).observe(request.holder, now, request.processes) {
        Ok(LeaseState::Ours) => {}
        Ok(lost) => return Some(Performed::LeaseLost(lost)),
        Err(error) => {
            let message =
                format!("the engine lease could not be read under the state lock: {error}");
            return Some(Performed::Unavailable(message));
        }
    }
    if let Some(paid) = &request.paid
        && !balance_still_spendable(&paid.prepaid, now)
    {
        tracing::info!(
            account = request.number,
            "the paid target's balance is no longer fresh and positive under the state lock; the move is refused"
        );
        return Some(Performed::PreconditionFailed);
    }
    None
}

/// [`in_cooldown`] from the later of the state file's last switch and `remembered`, a switch this engine made that the
/// file may not hold.
pub(crate) fn cooling(
    trigger: Trigger,
    state: &AutoswitchState,
    remembered: Option<f64>,
    settings: &AutoSwitchSettings,
    now: Timestamp,
) -> bool {
    let Some(remembered) = remembered else {
        return in_cooldown(trigger, state, settings, now);
    };
    let last = AutoswitchState {
        last_switch_at: Some(
            state
                .last_switch_at
                .map_or(remembered, |written| written.max(remembered)),
        ),
        ..AutoswitchState::default()
    };
    in_cooldown(trigger, &last, settings, now)
}

#[cfg(all(test, accounts_locks))]
#[expect(
    clippy::expect_used,
    reason = "a test that cannot set up its scratch store or claim its lease must fail loudly"
)]
mod tests {
    use std::fs;
    use std::sync::Arc;

    use proc_table::EmptyTable;

    use super::{
        LeftSnapshot, PerformRequest, Performed, SwitchFailure, SwitchFn, SwitchReport, WhileOn,
        perform,
    };
    use crate::drain::Drain;
    use crate::engine::{Lease, LeaseHolder, Trigger};
    use crate::locks::LockTiming;
    use crate::paths::{AccountsRoot, ClaudePaths};
    use crate::refresh::RefreshContext;
    use crate::refresh::oracle::ProbeVerdicts;
    use crate::secret::AccessToken;
    use crate::store::{AutoSwitchSettings, Store};
    use crate::switch::{SwitchErrorKind, SwitchPrecondition};
    use crate::transport::{
        ProfileTransport, RawProfile, RawToken, RefreshRequest, RefreshTransport, TransportError,
    };

    /// A switch whose precondition never holds.
    struct Refused;

    impl SwitchFn for Refused {
        fn switch(
            &self,
            _context: &RefreshContext<'_>,
            _number: u32,
            _precondition: Option<SwitchPrecondition>,
        ) -> Result<SwitchReport, SwitchFailure> {
            Err(SwitchFailure {
                kind: SwitchErrorKind::PreconditionFailed,
                login: None,
            })
        }

        fn write_while_on(&self, write: &mut dyn FnMut()) -> WhileOn {
            write();
            WhileOn::Ran
        }
    }

    /// The token and profile endpoints, neither of which the refused switch reaches.
    struct Offline;

    impl RefreshTransport for Offline {
        fn post_token(&self, _request: &RefreshRequest) -> Result<RawToken, TransportError> {
            Err(TransportError::Connect)
        }
    }

    impl ProfileTransport for Offline {
        fn get_profile(&self, _access: &AccessToken) -> Result<RawProfile, TransportError> {
            Err(TransportError::Connect)
        }
    }

    /// The switch's precondition refusal is its own outcome, which the tick skips the target on, never a failure.
    #[test]
    fn perform_maps_the_precondition_refusal_to_a_skip() {
        let dir = std::env::temp_dir().join(format!(
            "claude-accounts-perform-precondition-{}",
            std::process::id()
        ));
        // A folder left by an earlier run is wiped first; one that does not exist is the usual case.
        let _wiped = fs::remove_dir_all(&dir);
        #[cfg(unix)]
        {
            // Every folder and file the test makes is owner-only, as the user's temp folder makes them on Windows.
            // SAFETY: umask only swaps the process's file-mode creation mask; it takes no pointer and cannot fail.
            let _previous = unsafe { libc::umask(0o077) };
        }
        let root = AccountsRoot::new(dir.join("backup"));
        fs::create_dir_all(root.root()).expect("the scratch store folder can be made");
        let store = Store::open(root.clone()).expect("the scratch store opens");
        let claude = ClaudePaths::new(dir.join("claude"), dir.join(".claude.json"));
        let drain = Arc::new(Drain::new());
        let verdicts = ProbeVerdicts::new();
        let now = || jiff::Timestamp::from_second(1_790_000_000).expect("the time is in range");
        let context = RefreshContext {
            store: &store,
            claude: &claude,
            transport: &Offline,
            drain: &drain,
            securestorage_dir_set: false,
            clock: &now,
            profile: &Offline,
            verdicts: &verdicts,
            claude_locks: LockTiming::CLAUDE,
        };
        let holder = LeaseHolder::new("rustling-tulip", 101, 60.0);
        Lease::new(&root)
            .claim_or_heartbeat(&holder, now(), &EmptyTable)
            .expect("the lease is claimed");
        let settings = AutoSwitchSettings::default();
        let request = PerformRequest {
            context: &context,
            switcher: &Refused,
            number: 2,
            trigger: Trigger::Proactive,
            left: LeftSnapshot {
                headroom: None,
                recovery: None,
            },
            settings: &settings,
            holder: &holder,
            processes: &EmptyTable,
            remembered_switch_at: None,
            precondition: Some(SwitchPrecondition {
                expected_outgoing: 1,
                target_must_be_paid_opted_in: true,
                outgoing_must_be_opted_out: false,
            }),
            paid: None,
        };

        let performed = perform(&request);

        // Litter in the temp folder is harmless; the next run wipes it.
        let _removed = fs::remove_dir_all(&dir);
        assert!(
            matches!(performed, Performed::PreconditionFailed),
            "{performed:?}"
        );
    }
}
