//! One decision of the auto-switch engine, cswap's `AutoSwitchEngine.tick`. A tick re-reads the settings, stops when
//! the engine halted, pauses while another engine holds the lease or `CLAUDE_CONFIG_DIR` is
//! set, idles while the live login is an API key, lifts recovered quarantines, collects usage (the start-up warm-up
//! until every account settled, then the scheduled collection), classifies the active account's reading, ranks the
//! candidates (a consume-first pick on a fresh refetch, ranked again), freshens the chosen target and switches under the
//! state file's lock. While auto-switch is off it freshens no target and enters no paid fallback, its one move being the
//! leave from paid mode the owner permits by turning the paid account's "May spend credits" off, and pauses, unless
//! another pause or a halt ended it first. Everything it saw and did comes
//! back as [`Event`]s in a [`TickReport`]; it never panics and never returns an error.

use std::collections::{BTreeMap, BTreeSet};

use jiff::{SignedDuration, Timestamp};
use proc_table::ProcessTable;
use serde_json::Value;

use crate::engine::collect::{is_sentinel, shows_expired};
use crate::engine::paid::{
    balance_used_up, cached_balance, extra_usage_known_off, extra_usage_on, record_eligible,
};
use crate::engine::perform::{LeftSnapshot, PaidMove, PerformRequest, Performed, cooling, perform};
use crate::engine::reading::relevant_windows;
use crate::engine::{
    AccountRef, AccountUsage, ActiveHealth, ActiveReading, AutoswitchState, CandidateAccount,
    Candidates, Collected, EngineError, Event, EventBody, Freshened, Hold, IDENTITY_CONFLICT,
    IDENTITY_MISMATCH, IDLE_HOLD_MAX_S, Ineligible, Lease, LeaseHolder, LeaseOwner, LeaseState,
    OwnerExit, PAID_STAY_READ_S, PaidBalance, PaidCandidate, PaidFallback, RankInputs, Ranking,
    SPENT_HEADROOM_PCT, ScheduleInputs, SwitchFailure, SwitchFn, Trigger, TriggerDecision,
    TrustedReadings, WarmUp, WhileOn, all_above_threshold, binding_recovery, candidates,
    classify_trigger, collect_recheck, collect_refetch, collect_scheduled, collect_warmup,
    decision_value, freshen_target, paid_eligible, paid_ranking, quarantine, rank,
    release_recovered_quarantines,
};
use crate::locks::ACCOUNT_LOCK_TIMEOUT;
use crate::poll::{
    BalanceReadError, EXHAUSTED_INTERVAL_S, PollError, PollResult, SERVE_TTL_S, foreign_live_token,
    live_blob, live_identity, read_balance,
};
use crate::refresh::{GrantError, RefreshContext};
use crate::secret::AccessToken;
use crate::store::{
    self, AccountKind, AccountRecord, AutoSwitchSettings, Sequence, Store, StoreErrorKind,
};
use crate::switch::{LoginState, SwitchErrorKind, SwitchPrecondition};
use crate::transport::UsageTransport;
use crate::usage::{Identity, PrepaidEntry, UsageEntry, secs_from_timestamp};

/// cswap's `MAX_SLEEP_S`: the longest the engine sleeps while every account is exhausted, the exhausted accounts' poll
/// cadence, so a reset that comes early is still seen.
pub const MAX_SLEEP_S: f64 = EXHAUSTED_INTERVAL_S;
/// cswap's `NO_RESET_FALLBACK_S`: the wait of a blocked tick whose recovery time is unknown, and of an idle hold.
pub const NO_RESET_FALLBACK_S: f64 = 300.0;
/// cswap's `RESET_SLACK_S`: how long after a known reset the all-exhausted sleep ends.
pub const RESET_SLACK_S: f64 = 60.0;

/// Why a tick pauses while auto-switch is off ([`TickInputs::auto_switch_enabled`]).
pub const AUTO_SWITCH_OFF: &str = "auto-switch is off";
/// Why a tick pauses while `CLAUDE_CONFIG_DIR` is set.
const CLAUDE_CONFIG_DIR_SET: &str = "CLAUDE_CONFIG_DIR is set, so Claude Code reads its login from another profile; the engine does not switch it";

/// The freshen refusals every candidate meets alike, most actionable first, each with the message the engine's error
/// event carries.
const SYSTEMIC: [(&str, &str); 5] = [
    (
        "store-unmirrored",
        "CLAUDE_SECURESTORAGE_CONFIG_DIR is set; unset it or run from a normal shell",
    ),
    (
        "store-unwritable",
        "the account store refuses writes (its DPAPI check failed or accounts.json is a newer version); nothing is spent until it is writable",
    ),
    (
        "invalid_client",
        "the OAuth client was rejected; systemic, not this account",
    ),
    (
        "stash-unreadable",
        concat!(
            "a stash entry for an account does not open or does not match its row; it is kept, as it may be the only copy of a ",
            "spent successor, and holds that account's refresh until the stored credential changes or you discard it under ",
            "Held stash entries on this PC",
        ),
    ),
    (
        "consume-busy",
        "another refresh, switch, capture or stash discard holds the account; retries next pass",
    ),
];

/// How a tick collects usage: the start-up warm-up, the scheduled collection, consume-first's phase-2 refetch and the
/// exit target's re-read. [`TransportCollector`] runs the crate's own, [`collect_warmup`], [`collect_scheduled`],
/// [`collect_refetch`] and [`collect_recheck`].
pub trait Collector {
    /// The warm-up under `settings`: forces one fetch of `current` and each of `candidates` not polled since `warm_since`.
    ///
    /// # Errors
    ///
    /// As [`collect_warmup`].
    fn warmup(
        &self,
        context: &RefreshContext<'_>,
        settings: &AutoSwitchSettings,
        current: u32,
        candidates: &[u32],
        warm_since: Timestamp,
    ) -> Result<WarmUp, PollError>;

    /// The scheduled collection under `settings`.
    ///
    /// # Errors
    ///
    /// As [`collect_scheduled`].
    fn scheduled(
        &self,
        context: &RefreshContext<'_>,
        settings: &AutoSwitchSettings,
        inputs: &ScheduleInputs<'_>,
    ) -> Result<Collected, PollError>;

    /// Consume-first's phase-2 refetch of `members`, the active account and the candidates, under `settings`.
    ///
    /// # Errors
    ///
    /// As [`collect_refetch`].
    fn refetch(
        &self,
        context: &RefreshContext<'_>,
        settings: &AutoSwitchSettings,
        members: &[u32],
    ) -> Result<Collected, PollError>;

    /// The exit target's re-read under `settings`: one read of account `number`'s usage with its stored access token
    /// only. It brings no new reading without a refresh or a grant: a 401 is recorded as a failure with its backoff, and
    /// an entry still within the serve TTL is served as it is.
    ///
    /// # Errors
    ///
    /// As [`collect_recheck`].
    fn recheck(
        &self,
        context: &RefreshContext<'_>,
        settings: &AutoSwitchSettings,
        number: u32,
    ) -> Result<Collected, PollError>;

    /// One balance-only read of account `number`'s prepaid balance in organization `org_uuid` with `token`, the
    /// caller's: no usage request and no grant. A `token` that is the live credential's, when the profile oracle named
    /// that credential another login's than the account's, is refused before anything is sent
    /// ([`BalanceReadError::ForeignCredential`]); only the live token can be foreign. The default reads nothing and
    /// answers [`BalanceReadError::Unsupported`], so a collector that cannot read balances never enables the paid
    /// fallback.
    ///
    /// # Errors
    ///
    /// As [`BalanceReadError`].
    fn read_balance(
        &self,
        _context: &RefreshContext<'_>,
        _number: u32,
        _token: &AccessToken,
        _org_uuid: &str,
    ) -> Result<PrepaidEntry, BalanceReadError> {
        Err(BalanceReadError::Unsupported)
    }
}

/// The crate's own collection, over a usage endpoint.
#[derive(Clone, Copy)]
pub struct TransportCollector<'t>(pub &'t dyn UsageTransport);

impl Collector for TransportCollector<'_> {
    fn warmup(
        &self,
        context: &RefreshContext<'_>,
        settings: &AutoSwitchSettings,
        current: u32,
        candidates: &[u32],
        warm_since: Timestamp,
    ) -> Result<WarmUp, PollError> {
        collect_warmup(self.0, settings, context, current, candidates, warm_since)
    }

    fn scheduled(
        &self,
        context: &RefreshContext<'_>,
        settings: &AutoSwitchSettings,
        inputs: &ScheduleInputs<'_>,
    ) -> Result<Collected, PollError> {
        collect_scheduled(self.0, settings, context, inputs)
    }

    fn refetch(
        &self,
        context: &RefreshContext<'_>,
        settings: &AutoSwitchSettings,
        members: &[u32],
    ) -> Result<Collected, PollError> {
        collect_refetch(self.0, settings, context, members)
    }

    fn recheck(
        &self,
        context: &RefreshContext<'_>,
        settings: &AutoSwitchSettings,
        number: u32,
    ) -> Result<Collected, PollError> {
        collect_recheck(self.0, settings, context, number)
    }

    /// The transport's prepaid read, parsed as the poll's own balance read is, written into the account's own usage row
    /// under `cache/.usage.lock` (only over an older `fetchedAt`; a failure as `prepaid.error` and
    /// `prepaid.attemptedAt` alone). The account's identity is its `accounts.json` record's, and `org_uuid` must be its
    /// organization. Before anything is sent, a `token` that is the live credential's is refused as
    /// [`BalanceReadError::ForeignCredential`] when the profile oracle named that credential's lineage another login's
    /// (the verdict the active poll's `ForeignCredential` result rests on); only the live token can be foreign.
    fn read_balance(
        &self,
        context: &RefreshContext<'_>,
        number: u32,
        token: &AccessToken,
        org_uuid: &str,
    ) -> Result<PrepaidEntry, BalanceReadError> {
        let record = match context.store.read_sequence() {
            Ok(Some(sequence)) => sequence.switchable_account(number),
            Ok(None) => None,
            Err(error) => {
                tracing::warn!(account = number, error = %error, "the roster could not be read for a balance read; no balance is read");
                None
            }
        };
        let record = record.ok_or(BalanceReadError::UnknownAccount)?;
        if foreign_live_token(context, &record, token) {
            tracing::info!(
                account = number,
                "the live credential is another login's; its balance is not read"
            );
            return Err(BalanceReadError::ForeignCredential);
        }
        read_balance(
            self.0,
            context.store.root(),
            &Identity::of(&record),
            (number, token, org_uuid),
            context.clock,
        )
    }
}

/// Everything a tick runs on besides the engine's own memory: the store, Claude Code's files, the token endpoint and
/// the clock (all in `context`), the usage collection, the process table, the switch, and the environment.
pub struct TickInputs<'a> {
    /// The store, Claude Code's files, the token endpoint, the drain and the clock.
    pub context: &'a RefreshContext<'a>,
    /// How usage is collected.
    pub collector: &'a dyn Collector,
    /// The machine's processes, scanned to check whether another engine's lease holder is still running.
    pub processes: &'a dyn ProcessTable,
    /// How the live login is switched.
    pub switcher: &'a dyn SwitchFn,
    /// Whether `CLAUDE_CONFIG_DIR` is set in the caller's environment.
    pub claude_config_dir_set: bool,
    /// The auto-switch settings this tick runs on.
    pub settings: &'a AutoSwitchSettings,
    /// Whether auto-switch is on. While it is off the tick still holds the lease, reads usage and keeps its records
    /// (identity conflicts, the paid fallback's state), but freshens no target and enters no paid fallback. Its one
    /// move is the leave from its own paid mode once the paid account's "May spend credits" is turned off, which
    /// carries [`SwitchPrecondition::outgoing_must_be_opted_out`]; paid mode that ends for another reason (the balance
    /// used up, extra usage turned off) ends in place, with no move. The tick ends [`TickOutcome::Paused`] with the
    /// reason [`AUTO_SWITCH_OFF`] unless another pause or a halt ended it. A [`SwitchFn`] that finds it turned off
    /// since the tick began refuses every other switch with [`SwitchErrorKind::AutoSwitchOff`], which ends the tick the
    /// same way, and refuses an in-place entry's write ([`SwitchFn::write_while_on`]).
    pub auto_switch_enabled: bool,
}

/// What a tick came to: cswap's `TickOutcome`, plus the pause and the halt.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TickOutcome {
    /// The live login moved.
    Switched,
    /// Something failed; a later tick may succeed.
    Error,
    /// Nothing to do: below the threshold, a hold, the cooldown, or the warm-up.
    NoAction,
    /// The engine wanted to switch and had nowhere to go.
    Blocked,
    /// The engine stands by and decides nothing, for `reason`: another engine holds the lease or runs, the store
    /// refuses writes, Claude Code reads another profile, or auto-switch is off ([`AUTO_SWITCH_OFF`], whatever else the
    /// tick came to but another pause or a halt).
    Paused {
        /// Why, in words.
        reason: String,
    },
    /// A switch failed and so did putting the live files back: the engine decides nothing until it is restarted, and
    /// only keeps heartbeating its lease so no other engine takes over the half-switched login.
    Halted,
}

/// What a tick saw and did.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct TickReport {
    /// What happened, in order.
    pub events: Vec<Event>,
    /// What the tick came to.
    pub outcome: TickOutcome,
    /// When every account is exhausted and the soonest recovery is known: that time plus [`RESET_SLACK_S`].
    pub sleep_target: Option<Timestamp>,
    /// Whether the tick was blocked on something that will not change soon (no candidates, or every account
    /// exhausted), so the next tick may wait longer than the interval, as cswap's `_blocked_wait_long`.
    pub blocked_wait_long: bool,
    /// Whether the active account idle-holds, which slows the cadence to [`NO_RESET_FALLBACK_S`], as cswap's
    /// `_idle_hold_slow`.
    pub idle_hold_slow: bool,
    /// The paid account the tick held the login on because it may no longer spend credits and no account to move to
    /// was found; `None` otherwise.
    pub credits_hold: Option<u32>,
}

impl TickReport {
    /// A report of `outcome` with no events, no sleep target and neither cadence flag, the fields a caller fills in
    /// afterwards; the only way to build one outside the crate, [`TickReport`] being `#[non_exhaustive]`.
    #[must_use]
    pub fn new(outcome: TickOutcome) -> Self {
        Self {
            events: Vec::new(),
            outcome,
            sleep_target: None,
            blocked_wait_long: false,
            idle_hold_slow: false,
            credits_hold: None,
        }
    }
}

/// The auto-switch engine's memory between ticks: its name and pid (the lease's holder), the warm-up, the active
/// account's health, whether it halted, the identity conflicts whose quarantine is not written yet, the last switch it
/// could not record, and which configured model names were last checked.
#[derive(Debug)]
pub struct Engine {
    owner: String,
    pid: u32,
    warm_since: Option<Timestamp>,
    warm_sentinels: BTreeSet<u32>,
    health: ActiveHealth,
    halted: bool,
    /// Accounts whose token named another account but whose quarantine was not written: out of the census until it is.
    conflicted: BTreeSet<u32>,
    /// When this engine last switched without recording it in the state file (seconds since the epoch).
    last_switch_at: Option<f64>,
    /// The settings' `models` that cswap's check of them last ran to its end on; `None` until it has.
    models_checked: Option<Vec<String>>,
    /// When the paid fallback's entry last tried to read each account's balance: at most once per
    /// [`PAID_STAY_READ_S`]. A restart forgets it.
    balance_reads: BTreeMap<u32, Timestamp>,
    /// The target of a paid move whose switch was not recorded, whose entered event was already emitted: the next paid
    /// entry decision, which enters in place on it, emits none again. Cleared by that decision.
    unrecorded_entry: Option<u32>,
}

impl Engine {
    /// The engine `owner` (such as `ops-tower`) in process `pid`. With `warm_since`, no decision is made until every
    /// account was polled once since then (the warm-up); `None` decides from the first tick.
    #[must_use]
    pub fn new(owner: impl Into<String>, pid: u32, warm_since: Option<Timestamp>) -> Self {
        Self {
            owner: owner.into(),
            pid,
            warm_since,
            warm_sentinels: BTreeSet::new(),
            health: ActiveHealth::default(),
            halted: false,
            conflicted: BTreeSet::new(),
            last_switch_at: None,
            models_checked: None,
            balance_reads: BTreeMap::new(),
            unrecorded_entry: None,
        }
    }

    /// The active account's health as the last tick left it.
    #[must_use]
    pub fn health(&self) -> ActiveHealth {
        self.health
    }

    /// Whether a failed rollback halted the engine; only a new engine clears it.
    #[must_use]
    pub fn halted(&self) -> bool {
        self.halted
    }

    /// Halts the engine as a failed rollback does: [`Self::halted`] is true, every later tick only releases this
    /// engine's lease, so another engine can take over at once, and never collects or switches; only a new engine
    /// clears it.
    pub fn halt(&mut self) {
        self.halted = true;
    }

    /// Whether the warm-up still holds every decision back.
    #[must_use]
    pub fn warming(&self) -> bool {
        self.warm_since.is_some()
    }

    /// Runs one tick on `inputs` and reports it. A tick whose outcome is [`TickOutcome::Halted`], the one that halts the
    /// engine included, releases this engine's lease before it returns.
    pub fn tick(&mut self, inputs: &TickInputs<'_>) -> TickReport {
        let mut tick = Tick::new(inputs);
        let outcome = tick.run(self);
        if outcome == TickOutcome::Halted {
            tick.release_lease(
                &tick.holder(self),
                "the halted auto-switch engine could not release its lease",
            );
        }
        TickReport {
            events: tick.events,
            outcome,
            sleep_target: tick.sleep_target,
            blocked_wait_long: tick.blocked_wait_long,
            idle_hold_slow: tick.idle_hold_slow,
            credits_hold: tick.credits_hold,
        }
    }
}

/// A step of the tick: `Err` ends the tick with that outcome.
type Step<T> = Result<T, TickOutcome>;

/// The live login's account and the roster it was found in.
struct Roster {
    sequence: Sequence,
    active: AccountRecord,
}

/// The tick's usage: the accounts collected besides the active one, what the collection left, the readings a decision
/// may trust, and when they were judged.
struct TickUsage {
    members: Vec<u32>,
    collected: Collected,
    readings: BTreeMap<u32, Value>,
    now: Timestamp,
}

/// What the ranking decided, on which readings and census, at which time: consume-first's phase-2 ones when it ran.
struct Decision {
    ranking: Ranking,
    /// Whether every readable account is at or over the threshold: a proactive tick holds rather than switching.
    all_above: bool,
    readings: BTreeMap<u32, Value>,
    /// Each account's usage table entry as the tick's collection left it, the prepaid balance included.
    entries: BTreeMap<u32, UsageEntry>,
    census: Candidates,
    now: Timestamp,
    /// The phase-2 refetch's entries, which gate each consume-first target on its freshness; `None` when phase 2 did
    /// not run.
    fresh: Option<BTreeMap<u32, UsageEntry>>,
}

/// Why no candidate freshened.
#[derive(Default)]
struct FreshenFailures {
    transient: bool,
    /// The most actionable systemic refusal, an index into [`SYSTEMIC`].
    systemic: Option<usize>,
    /// The drain began: the tick stops without a word, as a draining switch does.
    draining: bool,
}

struct Tick<'t, 'a> {
    inputs: &'t TickInputs<'a>,
    settings: AutoSwitchSettings,
    models: Vec<String>,
    events: Vec<Event>,
    sleep_target: Option<Timestamp>,
    blocked_wait_long: bool,
    idle_hold_slow: bool,
    /// The paid account the tick held the login on, when it held it.
    credits_hold: Option<u32>,
    /// The engine's unwritten identity conflicts, as the tick began: left out of the census.
    conflicted: BTreeSet<u32>,
    /// Whether a switch's precondition refused a target this tick.
    precondition_refused: bool,
    /// The paid account the tick leaves for a refill, when it does: the ranking then drops every candidate under
    /// [`SPENT_HEADROOM_PCT`] headroom.
    leaving_paid: Option<u32>,
}

/// A paid account that may no longer spend credits, the eligibility clause it failed, and the owner's leave to move
/// off it when its "May spend credits" is off.
struct PaidExit<'p> {
    paid: &'p PaidFallback,
    reason: Ineligible,
    opted_out: Option<OptedOut>,
}

/// The owner's leave for a move off paid mode: the paid account's "May spend credits" is off, as the tick's roster
/// reads it. Only [`Tick::paid_mode`] makes one, so no other move carries it; the switch checks the flag again under its
/// locks ([`SwitchPrecondition::outgoing_must_be_opted_out`]). It is the one move an engine makes while auto-switch is
/// off.
#[derive(Clone, Copy)]
struct OptedOut;

/// The kind of a switch the tick makes, which sets the precondition it carries.
#[derive(Clone)]
enum Move {
    /// A move on usage alone: no precondition.
    Normal,
    /// A move off the paid account `from` onto an account that spends no credits: the live login must still be `from`,
    /// and, with `opted_out`, its "May spend credits" still off.
    Exit {
        from: u32,
        opted_out: Option<OptedOut>,
    },
    /// A move onto an account whose prepaid credits are spent, recorded as the paid fallback; with `opted_out`, a leave
    /// from paid mode whose account's "May spend credits" must still be off.
    Paid {
        paid: Box<PaidMove>,
        opted_out: Option<OptedOut>,
    },
}

impl Move {
    /// Whether the move carries the owner's leave to move off paid mode ([`OptedOut`]).
    fn opted_out(&self) -> bool {
        matches!(
            self,
            Self::Exit {
                opted_out: Some(_),
                ..
            } | Self::Paid {
                opted_out: Some(_),
                ..
            }
        )
    }
}

/// What the paid account's balance read in paid mode came to.
enum StayRead {
    /// The balance was read.
    Read(PrepaidEntry),
    /// It was not due, or the read failed or could not be made.
    NotRead,
    /// The live access token is expired, so it was not read: Claude Code is idle.
    TokenExpired,
}

/// The paid fallback's targets in the order to try them, and the engine that would enter it.
struct PaidPlan {
    targets: Vec<PaidTarget>,
    lease_owner: LeaseOwner,
    /// The account the paid fallback was first entered from: the live account on an entry, the paid fallback's own
    /// `from` on an exit to another paid account.
    entered_from: u32,
    /// The owner's leave to move off paid mode, on an exit whose paid account's "May spend credits" is off; `None` on
    /// an entry.
    opted_out: Option<OptedOut>,
}

/// A paid target: its account, and its cached balance when that is fresh enough to spend on as it is.
struct PaidTarget {
    number: u32,
    fresh: Option<PaidBalance>,
}

impl<'t, 'a> Tick<'t, 'a> {
    fn new(inputs: &'t TickInputs<'a>) -> Self {
        let settings = inputs.settings.clone();
        let models = settings.models.clone();
        Self {
            inputs,
            settings,
            models,
            events: Vec::new(),
            sleep_target: None,
            blocked_wait_long: false,
            idle_hold_slow: false,
            credits_hold: None,
            conflicted: BTreeSet::new(),
            precondition_refused: false,
            leaving_paid: None,
        }
    }

    fn store(&self) -> &'a Store {
        self.inputs.context.store
    }

    fn now(&self) -> Timestamp {
        (self.inputs.context.clock)()
    }

    fn emit(&mut self, body: EventBody) {
        let at = self.now();
        self.events.push(Event { at, body });
    }

    fn no_switch(&mut self, reason: &'static str, detail: impl Into<String>) {
        self.emit(EventBody::NoSwitch {
            reason,
            detail: detail.into(),
        });
    }

    /// An error event, and the tick's error outcome.
    fn error(&mut self, message: String, transient: bool) -> TickOutcome {
        self.emit(EventBody::Error { message, transient });
        TickOutcome::Error
    }

    /// The tick's outcome. While auto-switch is off, every outcome but a pause of its own (another engine's lease, a
    /// refusal) or a halt reads as [`AUTO_SWITCH_OFF`]'s pause, the endings of [`Self::prepare`] included.
    fn run(&mut self, engine: &mut Engine) -> TickOutcome {
        if engine.halted {
            return TickOutcome::Halted;
        }
        let ticked = self
            .prepare(engine)
            .and_then(|(roster, state)| self.act(engine, &roster, state));
        let outcome = match ticked {
            Ok(outcome) | Err(outcome) => outcome,
        };
        if self.inputs.auto_switch_enabled
            || matches!(outcome, TickOutcome::Paused { .. } | TickOutcome::Halted)
        {
            return outcome;
        }
        auto_switch_off()
    }

    /// The tick after [`Self::prepare`]: the usage, the paid step, the trigger, the cooldown, the decision and the
    /// switch. While auto-switch is off it runs all the same up to the switch, which pauses before it freshens any
    /// target ([`Self::switch`]), so its reads and records stay current; the paid fallback is neither entered nor left
    /// by a move, but for the leave the owner permits by turning the paid account's "May spend credits" off
    /// ([`Self::paid_mode`]).
    fn act(
        &mut self,
        engine: &mut Engine,
        roster: &Roster,
        mut state: AutoswitchState,
    ) -> Step<TickOutcome> {
        let ours = self.our_paid_fallback(engine, &state);
        let paid_mode = ours
            .as_ref()
            .is_some_and(|paid| paid.number == roster.active.number && paid.owner_exit.is_none());
        let usage = self.gather(engine, roster, &state, paid_mode)?;
        let usage = self.paid_step(engine, roster, &mut state, ours, usage)?;
        let trigger = self.classify(engine, roster.active.number, &usage)?;
        self.cooldown(engine, trigger, &state)?;
        let decision = self.decide(engine, trigger, roster, &state, usage)?;
        Ok(self.switch(engine, trigger, roster, &decision, &Move::Normal))
    }

    /// The engine's lease holder, heartbeating at the interval the tick read.
    fn holder(&self, engine: &Engine) -> LeaseHolder {
        LeaseHolder::new(
            engine.owner.clone(),
            engine.pid,
            self.settings.interval_seconds,
        )
    }

    /// Releases `holder`'s lease, as an engine that cannot act does (a halted one, or one that finds `accounts.json` at a
    /// newer version), so another engine can take over at once; a lease that is not ours, or none, is left alone. A
    /// failed release is logged as `failed` and changes nothing else in the tick.
    fn release_lease(&self, holder: &LeaseHolder, failed: &'static str) {
        if let Err(error) = Lease::new(self.store().root()).release(holder) {
            tracing::warn!(error = %error, "{failed}");
        }
    }

    /// The lease, the retried identity-conflict quarantines, the state file with recovered quarantines lifted, and the
    /// live login's account.
    fn prepare(&mut self, engine: &mut Engine) -> Step<(Roster, AutoswitchState)> {
        let sequence = self.hold_lease(engine)?;
        self.retry_conflicts(engine, sequence.as_ref());
        self.conflicted.clone_from(&engine.conflicted);
        let state = self.release_quarantines()?;
        let roster = self.roster(sequence)?;
        Ok((roster, state))
    }

    /// Looks at the lease, checks the refusals, then claims or heartbeats the lease; a refusal never claims it, and one
    /// for a newer `accounts.json` releases it. Returns `accounts.json` as the refusals read it.
    fn hold_lease(&mut self, engine: &Engine) -> Step<Option<Sequence>> {
        let holder = self.holder(engine);
        let lease = Lease::new(self.store().root());
        let observed = lease.observe(&holder, self.now(), self.inputs.processes);
        self.check_lease(observed)?;
        let sequence = self.refusals(&holder)?;
        let claimed = lease.claim_or_heartbeat(&holder, self.now(), self.inputs.processes);
        self.check_lease(claimed)?;
        Ok(sequence)
    }

    /// Another engine's fresh lease pauses the tick; a lease that cannot be read or written ends it as an error.
    fn check_lease(&mut self, lease: Result<LeaseState, EngineError>) -> Step<()> {
        match lease {
            Ok(LeaseState::Free | LeaseState::Ours) => Ok(()),
            Ok(LeaseState::HeldByOther { owner, pid, .. }) => Err(TickOutcome::Paused {
                reason: held_by_other(&owner, pid),
            }),
            Err(error) => Err(self.error(
                format!("the engine lease could not be read or written: {error}"),
                true,
            )),
        }
    }

    /// `CLAUDE_CONFIG_DIR` set, or a store that refuses writes or cannot be read pauses
    /// the tick; a store a newer version wrote ([`StoreErrorKind::UnknownSchema`]) also releases `holder`'s lease, so a
    /// newer engine can take over at once. That check comes first, so whatever else also refuses, a newer store still
    /// releases the lease. Returns `accounts.json`, `None` when there is none.
    fn refusals(&self, holder: &LeaseHolder) -> Step<Option<Sequence>> {
        let paused = |reason: String| Err(TickOutcome::Paused { reason });
        if store::version_refusal(self.store().root()) == Some(StoreErrorKind::UnknownSchema) {
            tracing::warn!(
                "accounts.json is of a newer version; the auto-switch engine releases its lease and pauses"
            );
            self.release_lease(
                holder,
                "the auto-switch engine could not release its lease in a newer account store",
            );
            return paused(
                SwitchErrorKind::of_store_refusal(StoreErrorKind::UnknownSchema).to_string(),
            );
        }
        if self.inputs.claude_config_dir_set {
            return paused(CLAUDE_CONFIG_DIR_SET.to_owned());
        }
        if self.inputs.context.securestorage_dir_set {
            return paused(SwitchErrorKind::StoreUnmirrored.to_string());
        }
        if let Some(kind) = self.store().write_refusal() {
            tracing::warn!(
                ?kind,
                "the account store refuses writes; the auto-switch engine pauses"
            );
            if kind == StoreErrorKind::UnknownSchema {
                self.release_lease(
                    holder,
                    "the auto-switch engine could not release its lease in a newer account store",
                );
            }
            return paused(SwitchErrorKind::of_store_refusal(kind).to_string());
        }
        self.store().read_sequence().or_else(|error| {
            tracing::warn!(kind = ?error.kind(), "accounts.json could not be read; the auto-switch engine pauses");
            paused(SwitchErrorKind::StoreUnreadable.to_string())
        })
    }

    /// Writes each identity-conflict quarantine an earlier tick could not, and lets the written ones back out of the
    /// engine's memory; an account no longer in `sequence` is forgotten.
    fn retry_conflicts(&mut self, engine: &mut Engine, sequence: Option<&Sequence>) {
        let Some(sequence) = sequence else {
            return;
        };
        let pending: Vec<u32> = engine.conflicted.iter().copied().collect();
        for number in pending {
            let Some(account) = sequence.account(number) else {
                engine.conflicted.remove(&number);
                continue;
            };
            match quarantine(
                self.store(),
                number,
                &account.email,
                IDENTITY_CONFLICT,
                self.now(),
            ) {
                Ok(()) => {
                    engine.conflicted.remove(&number);
                    self.quarantined(&account, IDENTITY_CONFLICT);
                }
                Err(error) => {
                    tracing::warn!(account = number, error = %error, "the identity-conflict quarantine is still not written; the account stays out");
                }
            }
        }
    }

    /// The live login's managed account. No live login, or one no managed account has (or no `accounts.json` at all),
    /// is nothing to act on (a switch would overwrite an unmanaged login with no backup); a managed API key idles the
    /// engine, since API-key accounts have no quota to watch and are never switched to or from.
    fn roster(&mut self, sequence: Option<Sequence>) -> Step<Roster> {
        let Some(identity) = live_identity(self.inputs.context.claude) else {
            return Err(self.no_active(false));
        };
        let Some(sequence) = sequence else {
            return Err(self.no_active(true));
        };
        let Some(active) = sequence
            .every_record()
            .find(|record| Identity::of(record) == identity)
            .cloned()
        else {
            return Err(self.no_active(true));
        };
        if active.kind == AccountKind::ApiKey {
            self.no_switch("active-api-key", "API-key accounts have no quota to watch");
            return Err(TickOutcome::NoAction);
        }
        Ok(Roster { sequence, active })
    }

    fn no_active(&mut self, unmanaged: bool) -> TickOutcome {
        self.emit(EventBody::Poll {
            active: None,
            headroom: BTreeMap::new(),
            threshold: self.settings.threshold,
            fetch_errors: BTreeMap::new(),
            windows: BTreeMap::new(),
        });
        if unmanaged {
            self.no_switch(
                "unmanaged-active-account",
                "add it with Add this login to include it in rotation",
            );
        } else {
            self.no_switch(
                "no-active-account",
                "log in, then add the account with Add this login",
            );
        }
        TickOutcome::NoAction
    }

    /// The state file, with every quarantine whose account was replaced since lifted.
    fn release_quarantines(&mut self) -> Step<AutoswitchState> {
        let read = AutoswitchState::read(self.store().root())
            .and_then(|state| release_recovered_quarantines(self.store(), state));
        let (state, released) = match read {
            Ok(read) => read,
            Err(error) => {
                return Err(self.error(
                    format!("the auto-switch state file could not be read or written: {error}"),
                    true,
                ));
            }
        };
        for lifted in released {
            self.emit(EventBody::Unquarantined {
                number: lifted.number,
                email: lifted.email,
                reason: lifted.reason,
            });
        }
        Ok(state)
    }

    /// The collection over the switchable accounts, its poll event, the warm-up's hold while it has not settled, and
    /// cswap's model-name check. `paid_mode` is whether this engine spends on the active account's credits.
    fn gather(
        &mut self,
        engine: &mut Engine,
        roster: &Roster,
        state: &AutoswitchState,
        paid_mode: bool,
    ) -> Step<TickUsage> {
        let members = self.collection_members(roster, state);
        let (collected, progress) =
            self.collect(engine, roster.active.number, &members, paid_mode)?;
        let now = self.now();
        let usage = TickUsage {
            members,
            readings: trusted_readings(&collected, &self.models, now),
            collected,
            now,
        };
        self.emit_poll(&roster.active, &usage);
        if let Some((settled, total)) = progress {
            self.no_switch(
                "warming-up",
                format!("{settled}/{total} accounts polled since start"),
            );
            return Err(TickOutcome::NoAction);
        }
        if engine.models_checked.as_ref() != Some(&self.models) {
            self.check_model_names(engine, roster, state, &usage.readings);
        }
        Ok(usage)
    }

    /// cswap's typo guard (`_check_model_names`) for the settings' `models`: once every switchable, unquarantined OAuth
    /// account has a readable reading, a configured model name no reading's scoped windows carry is a `config-warning`,
    /// and the check does not run again until the `models` a tick reads differ from the ones it checked. Until then it
    /// runs on each tick.
    fn check_model_names(
        &mut self,
        engine: &mut Engine,
        roster: &Roster,
        state: &AutoswitchState,
        readings: &BTreeMap<u32, Value>,
    ) {
        let wanted = wanted_models(&self.models);
        if wanted.is_empty() {
            return;
        }
        let mut relevant = Vec::new();
        for record in roster.sequence.records() {
            if !state.quarantine.contains_key(&record.number.to_string())
                && switchable(self.store(), record)
            {
                let Some(reading) = readings.get(&record.number) else {
                    return;
                };
                relevant.push(reading);
            }
        }
        let Some(missing) = missing_models(&wanted, &relevant) else {
            return;
        };
        engine.models_checked = Some(self.models.clone());
        if !missing.is_empty() {
            let message = format!(
                "autoswitch.models: {} matches no account's usage windows — only the 5h/7d limits are being watched for it (typo?)",
                missing.join(", ")
            );
            self.emit(EventBody::ConfigWarning { message });
        }
    }

    /// The cooldown since the last switch, the state file's or this engine's unrecorded one, holds the tick.
    fn cooldown(&mut self, engine: &Engine, trigger: Trigger, state: &AutoswitchState) -> Step<()> {
        if cooling(
            trigger,
            state,
            engine.last_switch_at,
            &self.settings,
            self.now(),
        ) {
            self.no_switch("cooldown", "");
            return Err(TickOutcome::NoAction);
        }
        Ok(())
    }

    /// The warm-up's collection and, while it has not settled, its progress; else the scheduled collection.
    fn collect(
        &mut self,
        engine: &mut Engine,
        active: u32,
        members: &[u32],
        paid_mode: bool,
    ) -> Step<(Collected, Option<(usize, usize)>)> {
        let context = self.inputs.context;
        if let Some(warm_since) = engine.warm_since {
            return self.warm_up(engine, active, members, warm_since);
        }
        let schedule = ScheduleInputs {
            current: active,
            candidates: members,
            idle_hold: engine.health.idle_hold_since.is_some(),
            threshold: self.settings.threshold,
            models: &self.models,
            paid_mode,
        };
        let collected = self
            .inputs
            .collector
            .scheduled(context, &self.settings, &schedule);
        match collected {
            Ok(collected) => Ok((collected, None)),
            Err(error) => Err(self.error(format!("usage could not be collected: {error}"), true)),
        }
    }

    /// The warm-up, less the members whose earlier warm-up poll was a sentinel (nothing a fetch could change), which
    /// count as settled. The warm-up ends for good on the tick every member settles.
    fn warm_up(
        &mut self,
        engine: &mut Engine,
        active: u32,
        members: &[u32],
        warm_since: Timestamp,
    ) -> Step<(Collected, Option<(usize, usize)>)> {
        let pending: Vec<u32> = members
            .iter()
            .copied()
            .filter(|number| !engine.warm_sentinels.contains(number))
            .collect();
        let dropped = members.len() - pending.len();
        let warm = match self.inputs.collector.warmup(
            self.inputs.context,
            &self.settings,
            active,
            &pending,
            warm_since,
        ) {
            Ok(warm) => warm,
            Err(error) => {
                return Err(self.error(format!("usage could not be collected: {error}"), true));
            }
        };
        for (number, result) in &warm.collected.results {
            if *number != active && is_sentinel(result) {
                engine.warm_sentinels.insert(*number);
            }
        }
        if warm.settled >= warm.total {
            engine.warm_since = None;
            engine.warm_sentinels.clear();
            return Ok((warm.collected, None));
        }
        Ok((
            warm.collected,
            Some((warm.settled + dropped, warm.total + dropped)),
        ))
    }

    /// cswap's `PollEvent`: each account's headroom, the windows of each readable one, and the fetch error of each
    /// unreadable one.
    fn emit_poll(&mut self, active: &AccountRecord, usage: &TickUsage) {
        let mut headroom = BTreeMap::new();
        let mut fetch_errors = BTreeMap::new();
        let mut windows = BTreeMap::new();
        for (number, entry) in &usage.collected.entries {
            let reading = usage.readings.get(number);
            headroom.insert(
                *number,
                reading.and_then(|reading| {
                    AccountUsage::from_reading(reading, &self.models).headroom()
                }),
            );
            let Some(reading) = reading else {
                let sentinel = usage.collected.results.get(number).is_some_and(is_sentinel);
                if let Some(error) = entry.last_error.as_ref().filter(|_| !sentinel) {
                    fetch_errors.insert(*number, error.clone());
                }
                continue;
            };
            let labelled = window_pcts(reading, &self.models);
            if !labelled.is_empty() {
                windows.insert(*number, labelled);
            }
        }
        self.emit(EventBody::Poll {
            active: Some(AccountRef::new(Some(active.number), active.email.as_str())),
            headroom,
            threshold: self.settings.threshold,
            fetch_errors,
            windows,
        });
    }

    /// The trigger classification on the active account's reading; a hold ends the tick.
    fn classify(&mut self, engine: &mut Engine, active: u32, usage: &TickUsage) -> Step<Trigger> {
        let reading = active_reading(usage, &self.models, active);
        let classification = classify_trigger(reading, engine.health, &self.settings, usage.now);
        engine.health = classification.health;
        if reading == ActiveReading::TokenExpired
            && classification.decision != TriggerDecision::Hold(Hold::ActiveIdle)
        {
            let message = format!(
                "the active account's token has been expired and owned for over {:.0} minutes; it counts as unhealthy again (a dead refresh token?)",
                IDLE_HOLD_MAX_S / 60.0
            );
            tracing::warn!("{message}");
            self.emit(EventBody::Warning { message });
        }
        let hold = match classification.decision {
            TriggerDecision::Switch(trigger) => return Ok(trigger),
            TriggerDecision::Hold(hold) => hold,
        };
        match hold {
            Hold::BelowThreshold => self.below_threshold(
                headroom_of(&usage.readings, &self.models, active).unwrap_or(100.0),
            ),
            Hold::ActiveIdle => {
                self.idle_hold_slow = true;
                self.no_switch(
                    "active-idle",
                    "token expired while Claude Code is idle; resumes on next use",
                );
            }
            Hold::ActiveUsageUnknown { ticks, needed } => self.no_switch(
                "active-usage-unknown",
                format!("{ticks}/{needed} before failover"),
            ),
        }
        Err(TickOutcome::NoAction)
    }

    fn below_threshold(&mut self, active_headroom: f64) {
        let detail = format!(
            "{}% < {}%",
            pct_label(100.0 - active_headroom),
            pct_label(self.settings.threshold)
        );
        self.no_switch("below-threshold", detail);
    }

    /// The census and the ranking (cswap's `_tick_inner` 1129-1377): the targets to try, best first, or the reason
    /// there are none. A consume-first ranking that found a target is decided again on a refetch ([`Self::phase_two`]).
    fn decide(
        &mut self,
        engine: &mut Engine,
        trigger: Trigger,
        roster: &Roster,
        state: &AutoswitchState,
        usage: TickUsage,
    ) -> Step<Decision> {
        let active = roster.active.number;
        let census = self.census(roster, state);
        if trigger == Trigger::ConsumeFirst
            && census.oauth.is_empty()
            && let Some(headroom) = headroom_of(&usage.readings, &self.models, active)
        {
            self.below_threshold(headroom);
            return Err(TickOutcome::NoAction);
        }
        if census.oauth.is_empty() {
            self.blocked_wait_long = true;
            self.no_switch("no-candidates", "");
            return Err(TickOutcome::Blocked);
        }
        let now = self.now();
        let trusted = TrustedReadings::new(&usage.readings, &self.models);
        let inputs = RankInputs {
            trigger,
            active,
            candidates: &census,
            usage: &trusted,
            state,
            settings: &self.settings,
            now,
        };
        let all_above = all_above_threshold(&inputs);
        let mut ranking = rank(&inputs);
        if self.leaving_paid.is_some() {
            ranking.ordered.retain(|number| {
                headroom_of(&usage.readings, &self.models, *number)
                    .is_some_and(|headroom| headroom >= SPENT_HEADROOM_PCT)
            });
        }
        let mut decision = Decision {
            ranking,
            all_above,
            readings: usage.readings,
            entries: usage.collected.entries,
            census,
            now,
            fresh: None,
        };
        // While auto-switch is off the refetch would only serve a switch that cannot be made, refreshing tokens for it.
        if self.inputs.auto_switch_enabled
            && trigger == Trigger::ConsumeFirst
            && !decision.ranking.ordered.is_empty()
        {
            self.phase_two(active, usage.members, state, &mut decision)?;
        }
        if decision.ranking.ordered.is_empty() {
            return Err(self.nothing_qualifies(engine, trigger, roster, state, &decision));
        }
        Ok(decision)
    }

    /// cswap's two-phase commit (autoswitch.py:1264-1293): consume-first decides below the threshold on readings that
    /// may be a candidate cadence old, so before it switches it refetches the active account and the candidates,
    /// replaces the decision's readings and time with the refetch's, and ranks again in full. The trigger is not
    /// classified again and the cooldown not checked again; no second poll event is emitted. A failed refetch ends the
    /// tick as a failed collection does.
    fn phase_two(
        &mut self,
        active: u32,
        mut members: Vec<u32>,
        state: &AutoswitchState,
        decision: &mut Decision,
    ) -> Step<()> {
        members.push(active);
        members.sort_unstable();
        members.dedup();
        let fresh =
            match self
                .inputs
                .collector
                .refetch(self.inputs.context, &self.settings, &members)
            {
                Ok(fresh) => fresh,
                Err(error) => {
                    return Err(self.error(format!("usage could not be collected: {error}"), true));
                }
            };
        let now = self.now();
        decision.readings = trusted_readings(&fresh, &self.models, now);
        decision.now = now;
        decision.ranking = rank(&RankInputs {
            trigger: Trigger::ConsumeFirst,
            active,
            candidates: &decision.census,
            usage: &TrustedReadings::new(&decision.readings, &self.models),
            state,
            settings: &self.settings,
            now,
        });
        decision.fresh = Some(fresh.entries);
        Ok(())
    }

    /// The accounts the collection covers besides the active one: the roster's switchable ([`switchable`]),
    /// unquarantined accounts.
    fn collection_members(&self, roster: &Roster, state: &AutoswitchState) -> Vec<u32> {
        let active = roster.active.number;
        roster
            .sequence
            .numbers()
            .iter()
            .copied()
            .filter(|number| *number != active)
            .filter(|number| !state.quarantine.contains_key(&number.to_string()))
            .filter(|number| {
                roster
                    .sequence
                    .switchable_account(*number)
                    .is_some_and(|record| switchable(self.store(), &record))
            })
            .collect()
    }

    /// cswap's census: the roster's switchable accounts ([`switchable`]) but the unwritten identity conflicts, the
    /// active and quarantined ones left out by [`candidates`].
    fn census(&self, roster: &Roster, state: &AutoswitchState) -> Candidates {
        let store = self.store();
        let accounts: Vec<CandidateAccount> = roster
            .sequence
            .numbers()
            .iter()
            .filter(|number| !self.conflicted.contains(number))
            .filter_map(|number| roster.sequence.switchable_account(*number))
            .filter(|record| switchable(store, record))
            .map(|record| CandidateAccount::from(&record))
            .collect();
        candidates(&accounts, roster.active.number, state)
    }

    /// Why nothing qualified (autoswitch.py:1301-1377): no candidate readable, a consume-first hold, the all-above
    /// hold, a candidate that may yet qualify, or every candidate exhausted: the paid fallback's entry when it applies
    /// ([`Self::paid_entry`], never while auto-switch is off), else the all-exhausted hold with the sleep target of the
    /// soonest provable recovery.
    fn nothing_qualifies(
        &mut self,
        engine: &mut Engine,
        trigger: Trigger,
        roster: &Roster,
        state: &AutoswitchState,
        decision: &Decision,
    ) -> TickOutcome {
        if !decision.ranking.any_known {
            self.no_switch("no-comparison", "no candidate has readable usage");
            return TickOutcome::Blocked;
        }
        if trigger == Trigger::ConsumeFirst {
            if decision.ranking.active_weekly_reset.is_none() {
                self.no_switch(
                    "reset-unknown",
                    "active account's weekly reset time is unknown; consume-first is idle until it is reported",
                );
            } else {
                self.no_switch(
                    "already-consuming-soonest",
                    "no sooner-resetting account with room to spare",
                );
            }
            return TickOutcome::NoAction;
        }
        let exhausted = decision.census.oauth.iter().all(|number| {
            headroom_of(&decision.readings, &self.models, *number)
                .is_some_and(|headroom| headroom <= 0.0)
        });
        if !exhausted {
            if !trigger.is_escape() && decision.all_above {
                self.no_switch(
                    "all-above-threshold",
                    "every readable account is at or over the threshold; the active account runs to its limit before a switch",
                );
                return TickOutcome::NoAction;
            }
            self.no_switch(
                "no-qualifying-candidate",
                "no candidate is below the threshold and better than the active account by the hysteresis margin, or usage is unreadable this tick",
            );
            return TickOutcome::Blocked;
        }
        if self.inputs.auto_switch_enabled
            && let Some(outcome) = self.paid_entry(engine, trigger, roster, state, decision)
        {
            return outcome;
        }
        self.blocked_wait_long = true;
        let earliest = earliest_recovery(&decision.readings, &self.models, self.now());
        self.sleep_target = earliest.and_then(|earliest| {
            let slack = SignedDuration::try_from_secs_f64(RESET_SLACK_S).ok()?;
            earliest.checked_add(slack).ok()
        });
        self.emit(EventBody::AllExhausted {
            earliest_reset_at: earliest,
        });
        TickOutcome::Blocked
    }

    /// Freshens each target in order and switches to the first one ready; a target the switch refuses is skipped for
    /// the next. The departure snapshot is the ranking's own reading of the active account, at the ranking's time
    /// (consume-first's phase-2 refetch). A consume-first target the refetch left stale holds the tick
    /// ([`Self::stale_target`]). Each move is of the kind `outgoing` names, which sets its precondition. While
    /// auto-switch is off a move that may not be made then ([`Self::perform`]) is not prepared: the tick pauses before
    /// the first target is freshened.
    fn switch(
        &mut self,
        engine: &mut Engine,
        trigger: Trigger,
        roster: &Roster,
        decision: &Decision,
        outgoing: &Move,
    ) -> TickOutcome {
        if !self.inputs.auto_switch_enabled && !outgoing.opted_out() {
            return auto_switch_off();
        }
        let left = self.left_snapshot(decision, roster.active.number);
        let mut failures = FreshenFailures::default();
        for &number in &decision.ranking.ordered {
            if self.stale_target(decision, number) {
                return TickOutcome::NoAction;
            }
            let Some(account) = roster.sequence.switchable_account(number) else {
                continue;
            };
            if !self.freshen(engine, &account, &mut failures) {
                if failures.draining {
                    return TickOutcome::NoAction;
                }
                continue;
            }
            if let Some(outcome) = self.perform(engine, trigger, number, left, outgoing.clone()) {
                return outcome;
            }
        }
        self.no_target(&failures)
    }

    /// The account a switch leaves, as the ranking read it at the ranking's time.
    fn left_snapshot(&self, decision: &Decision, active: u32) -> LeftSnapshot {
        LeftSnapshot {
            headroom: headroom_of(&decision.readings, &self.models, active),
            recovery: binding_recovery(
                usage_of(&decision.readings, &self.models, active).as_ref(),
                decision.now,
            ),
        }
    }

    /// This engine's paid fallback as the state file holds it: `None` when there is none, and when its lease owner is
    /// not this engine's current hold on the lease (another engine's), which is treated as absent. An owner-exit bar is
    /// the owner's act, not engine state, so a barred key is this engine's to honour and lift whatever its lease owner.
    fn our_paid_fallback(&self, engine: &Engine, state: &AutoswitchState) -> Option<PaidFallback> {
        let paid = state.paid_fallback.as_ref()?;
        if paid.owner_exit.is_some() {
            return Some(paid.clone());
        }
        let holder = self.holder(engine);
        let started_at = self.lease_start(&holder)?;
        paid.lease_owner
            .is(&holder, started_at)
            .then(|| paid.clone())
    }

    /// The paid step, on every tick that gathered usage, before the trigger is classified: with this engine's paid
    /// fallback (`ours`) on the live account, paid mode ([`Self::paid_mode`]); on another account, the login moved
    /// ([`Self::login_moved`]); with an owner-exit bar, its lifting ([`Self::lift_bar`]). `state` follows each write it
    /// makes. `Err` ends the tick; `Ok` hands the usage on to the rest of it.
    fn paid_step(
        &mut self,
        engine: &mut Engine,
        roster: &Roster,
        state: &mut AutoswitchState,
        ours: Option<PaidFallback>,
        usage: TickUsage,
    ) -> Step<TickUsage> {
        let Some(paid) = ours else {
            return Ok(usage);
        };
        if paid.owner_exit.is_some() {
            self.lift_bar(engine, roster, state, &paid, &usage.readings)?;
            return Ok(usage);
        }
        if paid.number != roster.active.number {
            self.login_moved(engine, roster, state, &paid, &usage.readings)?;
            return Ok(usage);
        }
        self.paid_mode(engine, roster, state, &paid, usage)
    }

    /// Lifts `paid`'s owner-exit bar once some account has [`SPENT_HEADROOM_PCT`] headroom, or the opted-in accounts
    /// hold one the bar's set does not: the paid fallback is cleared under the state file's lock. A credits toggle
    /// turned off never lifts it.
    fn lift_bar(
        &mut self,
        engine: &Engine,
        roster: &Roster,
        state: &mut AutoswitchState,
        paid: &PaidFallback,
        readings: &BTreeMap<u32, Value>,
    ) -> Step<()> {
        let Some(bar) = &paid.owner_exit else {
            return Ok(());
        };
        let active_refilled = headroom_of(readings, &self.models, roster.active.number)
            .is_some_and(|headroom| headroom >= SPENT_HEADROOM_PCT);
        let gained = bar.opted_in.as_ref().is_some_and(|known| {
            opted_in(&roster.sequence)
                .iter()
                .any(|number| !known.contains(number))
        });
        if !active_refilled && !self.refill_exists(roster, state, readings) && !gained {
            return Ok(());
        }
        if self.write_paid(engine, paid, None)? {
            log_bar_lifted(paid.number);
            state.paid_fallback = None;
        }
        Ok(())
    }

    /// The login is no longer on the account `paid` names. A switch this engine made but could not record clears the
    /// paid fallback; any other move is the owner's: the bar is set ([`OwnerExit`], with the opted-in accounts as they
    /// are), the left event emitted, and an at-limit live account that spends credits without its toggle is warned of.
    fn login_moved(
        &mut self,
        engine: &Engine,
        roster: &Roster,
        state: &mut AutoswitchState,
        paid: &PaidFallback,
        readings: &BTreeMap<u32, Value>,
    ) -> Step<()> {
        let own_move = engine.last_switch_at.is_some_and(|at| at > paid.since);
        let next = (!own_move).then(|| {
            let mut barred = paid.clone();
            barred.owner_exit = Some(OwnerExit::new(
                secs_from_timestamp(self.now()),
                opted_in(&roster.sequence),
            ));
            barred
        });
        if !self.write_paid(engine, paid, next.clone())? {
            return Ok(());
        }
        state.paid_fallback = next;
        if own_move {
            return Ok(());
        }
        let live = roster.active.number;
        tracing::info!(
            account = paid.number,
            live,
            "the login was moved off the paid account; paid entry is barred"
        );
        self.emit(EventBody::PaidFallbackLeft {
            number: paid.number,
            reason: "owner-exit",
            to: None,
            headroom_pct: None,
        });
        if !roster.active.may_spend_credits
            && headroom_of(readings, &self.models, live).is_some_and(|headroom| headroom <= 0.0)
            && extra_usage_on(readings.get(&live)).is_ok()
        {
            tracing::warn!(
                account = live,
                "the live account spends extra-usage credits it may not spend; the engine does not move it"
            );
            self.emit(EventBody::Warning {
                message: format!(
                    "account {live} is at its limit with extra usage on, so it spends credits, but May spend credits is off for it; the engine does not move it"
                ),
            });
        }
        Ok(())
    }

    /// Paid mode, on the live account `paid` names. Its own refill ends paid mode with no switch; a refill of
    /// [`SPENT_HEADROOM_PCT`] elsewhere leaves through the ranking ([`Self::leave_for_refill`]); else the account stays
    /// on its credits while [`paid_eligible`] holds, its balance read every [`PAID_STAY_READ_S`] ([`Self::stay_read`]),
    /// and leaves by the exit rules when it fails ([`Self::exit_paid`]). A balance gone stale while the live token is
    /// expired (Claude Code idle) hands the tick on, to its idle hold. While auto-switch is off there is no refill leave,
    /// and the exit rules run only when the paid account's "May spend credits" is off, the owner's leave for that one
    /// move ([`OptedOut`]); otherwise [`Self::paid_mode_off`] judges it.
    fn paid_mode(
        &mut self,
        engine: &mut Engine,
        roster: &Roster,
        state: &mut AutoswitchState,
        paid: &PaidFallback,
        usage: TickUsage,
    ) -> Step<TickUsage> {
        let number = paid.number;
        if headroom_of(&usage.readings, &self.models, number).is_some_and(|headroom| headroom > 0.0)
        {
            if self.write_paid(engine, paid, None)? {
                state.paid_fallback = None;
                self.emit(EventBody::PaidFallbackLeft {
                    number,
                    reason: "own-refill",
                    to: None,
                    headroom_pct: None,
                });
            }
            return Err(TickOutcome::NoAction);
        }
        let enabled = self.inputs.auto_switch_enabled;
        if enabled && self.refill_exists(roster, state, &usage.readings) {
            return Err(self.leave_for_refill(engine, roster, state, number, usage));
        }
        let opted_out = (!roster.active.may_spend_credits).then_some(OptedOut);
        let decision = self.paid_decision(roster, state, &usage);
        let record = record_eligible(&self.paid_candidate(&roster.active, state, &decision, None));
        if !enabled && opted_out.is_none() {
            return self.paid_mode_off(engine, state, paid, &decision, usage);
        }
        if let Err(reason) = record {
            return Err(self.exit_paid(
                engine,
                roster,
                state,
                &decision,
                &PaidExit {
                    paid,
                    reason,
                    opted_out,
                },
            ));
        }
        if matches!(
            usage.collected.results.get(&number),
            Some(PollResult::TokenExpired)
        ) {
            tracing::info!(
                account = number,
                "the paid account's live token is expired (Claude Code idle); paid mode holds without a reading"
            );
            return Ok(usage);
        }
        let read = self.stay_read(engine, &roster.active, &usage);
        let prepaid = match &read {
            StayRead::Read(prepaid) => Some(prepaid),
            StayRead::NotRead | StayRead::TokenExpired => cached_prepaid(&decision, number),
        };
        match paid_eligible(
            &self.paid_candidate(&roster.active, state, &decision, prepaid),
            self.now(),
        ) {
            Ok(_) => {
                self.no_switch(
                    "on-credits",
                    format!("account {number} carries the fleet on its prepaid credits"),
                );
                Err(TickOutcome::NoAction)
            }
            Err(Ineligible::StaleBalance) if matches!(read, StayRead::TokenExpired) => Ok(usage),
            Err(reason) => Err(self.exit_paid(
                engine,
                roster,
                state,
                &decision,
                &PaidExit {
                    paid,
                    reason,
                    opted_out,
                },
            )),
        }
    }

    /// Paid mode while auto-switch is off, on an account whose "May spend credits" is on: nothing is refreshed,
    /// refetched or read for it. Its extra usage known off in the tick's reading, or its cached balance read fresh at
    /// zero, ends paid mode in place, whatever its record's other clauses say: the paid fallback is cleared under the
    /// state file's lock and, when that wrote, the left event says why; nothing is switched. Otherwise paid mode is kept
    /// (a stale balance, a stale zero included, until it reads again) and the tick pauses, or, with the live token
    /// expired (Claude Code idle), hands on to its idle hold as an on engine's does.
    fn paid_mode_off(
        &mut self,
        engine: &Engine,
        state: &mut AutoswitchState,
        paid: &PaidFallback,
        decision: &Decision,
        usage: TickUsage,
    ) -> Step<TickUsage> {
        let number = paid.number;
        let reading = decision.readings.get(&number);
        let reason = if extra_usage_known_off(reading) {
            extra_usage_on(reading).err()
        } else if balance_used_up(cached_prepaid(decision, number), self.now()) {
            Some(Ineligible::BalanceZero)
        } else {
            None
        };
        let Some(reason) = reason else {
            if matches!(
                usage.collected.results.get(&number),
                Some(PollResult::TokenExpired)
            ) {
                tracing::info!(
                    account = number,
                    "the paid account's live token is expired (Claude Code idle); paid mode holds without a reading"
                );
                return Ok(usage);
            }
            return Err(auto_switch_off());
        };
        if self.write_paid(engine, paid, None)? {
            state.paid_fallback = None;
            tracing::info!(
                account = number,
                reason = %reason,
                "the paid account may no longer spend credits; auto-switch is off, so paid mode ends where it is, with no move"
            );
            self.emit(EventBody::PaidFallbackLeft {
                number,
                reason: left_reason(reason),
                to: None,
                headroom_pct: None,
            });
        }
        Err(TickOutcome::NoAction)
    }

    /// Whether some census account has at least [`SPENT_HEADROOM_PCT`] trusted headroom in `readings`.
    fn refill_exists(
        &self,
        roster: &Roster,
        state: &AutoswitchState,
        readings: &BTreeMap<u32, Value>,
    ) -> bool {
        self.census(roster, state).oauth.iter().any(|number| {
            headroom_of(readings, &self.models, *number)
                .is_some_and(|headroom| headroom >= SPENT_HEADROOM_PCT)
        })
    }

    /// Leaves paid mode on `number` for an account with [`SPENT_HEADROOM_PCT`] headroom: the normal ranking on the
    /// escape trigger, less every candidate under that headroom, then the switch, an exit move whose precondition is
    /// that the live login is still `number` and whose record clears the paid fallback; the left event once the login
    /// moved.
    fn leave_for_refill(
        &mut self,
        engine: &mut Engine,
        roster: &Roster,
        state: &AutoswitchState,
        number: u32,
        usage: TickUsage,
    ) -> TickOutcome {
        self.leaving_paid = Some(number);
        let trigger = if headroom_of(&usage.readings, &self.models, number).is_some() {
            Trigger::AtLimit
        } else {
            Trigger::Failover
        };
        let decision = match self.decide(engine, trigger, roster, state, usage) {
            Ok(decision) => decision,
            Err(outcome) => return outcome,
        };
        let outcome = self.switch(
            engine,
            trigger,
            roster,
            &decision,
            &Move::Exit {
                from: number,
                opted_out: None,
            },
        );
        if outcome == TickOutcome::Switched {
            let to = self
                .events
                .iter()
                .rev()
                .find_map(|event| match &event.body {
                    EventBody::Switch { to, .. } => to.number,
                    _ => None,
                });
            let headroom_pct = to.and_then(|to| headroom_of(&decision.readings, &self.models, to));
            self.emit(EventBody::PaidFallbackLeft {
                number,
                reason: "refill",
                to,
                headroom_pct,
            });
        }
        outcome
    }

    /// The paid step's decision: the tick's readings and usage entries over the census, with no ranking.
    fn paid_decision(
        &self,
        roster: &Roster,
        state: &AutoswitchState,
        usage: &TickUsage,
    ) -> Decision {
        Decision {
            ranking: Ranking::default(),
            all_above: false,
            readings: usage.readings.clone(),
            entries: usage.collected.entries.clone(),
            census: self.census(roster, state),
            now: usage.now,
            fresh: None,
        }
    }

    /// The paid account's balance-only read with the live token, at most once per [`PAID_STAY_READ_S`]: no usage
    /// request and no grant. Not made while the live access token is expired (it is never refreshed for this), when the
    /// live file holds no OAuth token, or while the account's poll found the live credential another login's.
    fn stay_read(
        &mut self,
        engine: &mut Engine,
        account: &AccountRecord,
        usage: &TickUsage,
    ) -> StayRead {
        let number = account.number;
        let now = self.now();
        if engine.balance_reads.get(&number).is_some_and(|last| {
            secs_from_timestamp(now) - secs_from_timestamp(*last) < PAID_STAY_READ_S
        }) {
            return StayRead::NotRead;
        }
        if matches!(
            usage.collected.results.get(&number),
            Some(PollResult::ForeignCredential)
        ) {
            tracing::info!(
                account = number,
                "the live credential is another login's; the paid account's balance is not read"
            );
            return StayRead::NotRead;
        }
        let Some(oauth) = live_blob(self.inputs.context.claude).and_then(|blob| blob.oauth())
        else {
            tracing::info!(
                account = number,
                "the live credential holds no access token; the paid account's balance is not read"
            );
            return StayRead::NotRead;
        };
        if oauth.is_expired(now) {
            tracing::info!(
                account = number,
                "the live access token is expired; the paid account's balance is not read until it is renewed"
            );
            return StayRead::TokenExpired;
        }
        engine.balance_reads.insert(number, now);
        match self.inputs.collector.read_balance(
            self.inputs.context,
            number,
            oauth.access_token(),
            &account.org_uuid,
        ) {
            Ok(prepaid) => StayRead::Read(prepaid),
            Err(error) => {
                tracing::info!(account = number, error = %error, "the paid account's balance read failed; it is judged on its last balance");
                StayRead::NotRead
            }
        }
    }

    /// The paid account may no longer spend credits (`exit`): the login moves to the eligible paid account with the
    /// largest balance ([`Self::paid_switch`]); failing that, to an account whose extra usage is known off
    /// ([`Self::exit_to_unpaid`]); else it holds with a warning, every tick. The left event follows a move. With the
    /// owner's leave ([`OptedOut`]: the account's "May spend credits" is off) every move carries it, the left event's
    /// reason is `flag-off` whichever clause failed first, and the exit runs while auto-switch is off too; without it,
    /// only while auto-switch is on.
    fn exit_paid(
        &mut self,
        engine: &mut Engine,
        roster: &Roster,
        state: &AutoswitchState,
        decision: &Decision,
        exit: &PaidExit<'_>,
    ) -> TickOutcome {
        let number = exit.paid.number;
        let reason = if exit.opted_out.is_some() {
            "flag-off"
        } else {
            left_reason(exit.reason)
        };
        tracing::info!(
            account = number,
            reason,
            "the paid account may no longer spend credits; the engine moves the login off it by the exit rules"
        );
        let left = EventBody::PaidFallbackLeft {
            number,
            reason,
            to: None,
            headroom_pct: None,
        };
        let targets = self.paid_targets(roster, state, decision);
        if !targets.is_empty() {
            let plan = PaidPlan {
                targets,
                lease_owner: exit.paid.lease_owner.clone(),
                entered_from: exit.paid.from,
                opted_out: exit.opted_out,
            };
            let outcome = self.paid_switch(engine, roster, state, decision, &plan);
            if outcome == TickOutcome::Switched {
                self.emit(left);
                return outcome;
            }
            if outcome != TickOutcome::Blocked {
                return outcome;
            }
        }
        match self.exit_to_unpaid(engine, roster, decision, exit) {
            Some(TickOutcome::Switched) => {
                self.emit(left);
                TickOutcome::Switched
            }
            Some(outcome) => outcome,
            None => {
                tracing::warn!(
                    account = number,
                    reason,
                    "the paid account may no longer spend credits and there is no account to move to; it holds"
                );
                self.credits_hold = Some(number);
                self.emit(EventBody::Warning {
                    message: format!(
                        "account {number} may no longer spend credits ({reason}) and no account to move to was found; the login stays on it"
                    ),
                });
                TickOutcome::Blocked
            }
        }
    }

    /// Moves the login off the paid account onto an account whose extra usage is known off, so nothing spends credits:
    /// the account paid mode was entered from first, then the others in rotation order, each move carrying the exit's
    /// leave when it has one. `None` when none switched.
    fn exit_to_unpaid(
        &mut self,
        engine: &mut Engine,
        roster: &Roster,
        decision: &Decision,
        exit: &PaidExit<'_>,
    ) -> Option<TickOutcome> {
        let paid = exit.paid;
        let mut order = decision.census.oauth.clone();
        order.retain(|number| extra_usage_known_off(decision.readings.get(number)));
        if let Some(place) = order.iter().position(|number| *number == paid.from) {
            let from = order.remove(place);
            order.insert(0, from);
        }
        let left = self.left_snapshot(decision, roster.active.number);
        let mut failures = FreshenFailures::default();
        for number in order {
            let Some(account) = roster.sequence.switchable_account(number) else {
                continue;
            };
            if !self.freshen(engine, &account, &mut failures) {
                if failures.draining {
                    return Some(TickOutcome::NoAction);
                }
                continue;
            }
            if !self.still_known_off(number) {
                continue;
            }
            let kind = Move::Exit {
                from: paid.number,
                opted_out: exit.opted_out,
            };
            if let Some(outcome) = self.perform(engine, Trigger::AtLimit, number, left, kind) {
                return Some(outcome);
            }
        }
        None
    }

    /// Whether account `number`'s extra usage is still known off on its usage refetched now: an exit target was judged
    /// on the tick's readings, which may be a poll cadence old, and a move onto an account whose owner turned extra
    /// usage on since would spend its credits without its opt-in. The re-read ([`Collector::recheck`]) spends no grant.
    /// A failed re-read, or one that left the account not fetched within [`SERVE_TTL_S`] (a backoff, a hold, another
    /// collector's claim, an expired token or a 401), is not known off.
    fn still_known_off(&self, number: u32) -> bool {
        let fresh = match self
            .inputs
            .collector
            .recheck(self.inputs.context, &self.settings, number)
        {
            Ok(fresh) => fresh,
            Err(error) => {
                tracing::info!(account = number, error = %error, "the exit target's usage could not be read again; it is not an exit target");
                return false;
            }
        };
        if !fresh
            .entries
            .get(&number)
            .is_some_and(|entry| entry.fresh(self.now(), SERVE_TTL_S))
        {
            tracing::info!(
                account = number,
                "the exit target's usage could not be refreshed this tick; it is not an exit target"
            );
            return false;
        }
        let readings = trusted_readings(&fresh, &self.models, self.now());
        if extra_usage_known_off(readings.get(&number)) {
            return true;
        }
        tracing::info!(
            account = number,
            "account {number}'s extra usage is no longer known off; it is not an exit target"
        );
        false
    }

    /// Writes `next` as the paid fallback under the state file's lock, only while the lease is still this engine's and
    /// the file still holds `expected`: `Ok(true)` when written, `Ok(false)` when the file changed since (nothing is
    /// written), `Err` with the tick's end when the lease is lost or the file cannot be read or written.
    fn write_paid(
        &mut self,
        engine: &Engine,
        expected: &PaidFallback,
        next: Option<PaidFallback>,
    ) -> Step<bool> {
        let holder = self.holder(engine);
        let root = self.store().root();
        let processes = self.inputs.processes;
        let now = self.now();
        let written = AutoswitchState::update_if(root, ACCOUNT_LOCK_TIMEOUT, |state| {
            let observed = Lease::new(root).observe(&holder, now, processes);
            let write = matches!(observed, Ok(LeaseState::Ours))
                && state.paid_fallback.as_ref() == Some(expected);
            if write {
                state.paid_fallback = next;
            }
            (observed.map(|lease| (lease, write)), write)
        });
        match written {
            Ok(Ok((LeaseState::Ours, write))) => Ok(write),
            Ok(Ok((lost, _))) => Err(TickOutcome::Paused {
                reason: lease_lost(&lost),
            }),
            Ok(Err(error)) => Err(self.error(
                format!("the engine lease could not be read under the state lock: {error}"),
                true,
            )),
            Err(error) => Err(self.error(
                format!("the paid fallback could not be written to autoswitch_state.json: {error}"),
                true,
            )),
        }
    }

    /// The paid fallback's entry at an all-exhausted tick: only on [`Trigger::AtLimit`], after the warm-up, with the
    /// lease held and no owner-exit bar, whichever engine wrote it. A paid fallback this engine is already in on the live account
    /// reports `on-credits`; an eligible active account enters in place ([`Self::enter_in_place`]); else the eligible
    /// accounts are tried largest balance first ([`Self::paid_switch`]). `None` when no paid target exists, which
    /// leaves the tick to its all-exhausted hold. A `paidFallback` another engine wrote is treated as absent, its bar
    /// aside.
    fn paid_entry(
        &mut self,
        engine: &mut Engine,
        trigger: Trigger,
        roster: &Roster,
        state: &AutoswitchState,
        decision: &Decision,
    ) -> Option<TickOutcome> {
        let unrecorded_entry = engine.unrecorded_entry.take();
        if trigger != Trigger::AtLimit || engine.warming() {
            return None;
        }
        let holder = self.holder(engine);
        let started_at = self.lease_start(&holder)?;
        let active = roster.active.number;
        if let Some(paid) = state.paid_fallback.as_ref() {
            if paid.owner_exit.is_some() {
                return None;
            }
            if paid.lease_owner.is(&holder, started_at) && paid.number == active {
                self.no_switch(
                    "on-credits",
                    format!("account {active} carries the fleet on its prepaid credits"),
                );
                return Some(TickOutcome::NoAction);
            }
        }
        let lease_owner = LeaseOwner::new(engine.owner.clone(), engine.pid, started_at);
        let cached = cached_prepaid(decision, active);
        if let Ok(balance) = paid_eligible(
            &self.paid_candidate(&roster.active, state, decision, cached),
            self.now(),
        ) {
            let announce = unrecorded_entry != Some(active);
            let (outcome, recorded) =
                self.enter_in_place(&holder, active, &lease_owner, balance, announce);
            if !recorded && !announce {
                // Nothing was written, so a later decision still enters on the unrecorded move, without its event again.
                engine.unrecorded_entry = unrecorded_entry;
            }
            return Some(outcome);
        }
        let targets = self.paid_targets(roster, state, decision);
        if targets.is_empty() {
            return None;
        }
        let plan = PaidPlan {
            targets,
            lease_owner,
            entered_from: active,
            opted_out: None,
        };
        Some(self.paid_switch(engine, roster, state, decision, &plan))
    }

    /// When this engine's hold on the lease began; `None` when the lease is not its own or cannot be read.
    fn lease_start(&self, holder: &LeaseHolder) -> Option<f64> {
        match Lease::new(self.store().root()).started_at(holder) {
            Ok(started_at) => started_at,
            Err(error) => {
                tracing::warn!(error = %error, "the engine lease could not be read; no paid fallback is entered");
                None
            }
        }
    }

    /// `record` as the paid fallback's eligibility test judges it, with the decision's trusted reading and `prepaid`.
    fn paid_candidate<'d>(
        &self,
        record: &'d AccountRecord,
        state: &AutoswitchState,
        decision: &'d Decision,
        prepaid: Option<&'d PrepaidEntry>,
    ) -> PaidCandidate<'d> {
        let number = record.number;
        PaidCandidate {
            record,
            reading: decision.readings.get(&number),
            prepaid,
            available: switchable(self.store(), record)
                && !state.quarantine.contains_key(&number.to_string())
                && !self.conflicted.contains(&number),
        }
    }

    /// The census accounts that may carry the fleet on their credits, in the order to try them ([`paid_ranking`] over
    /// the rotation): each eligible one, and each whose only failure is a stale balance, ranked on its cached amount
    /// until it is read again.
    fn paid_targets(
        &self,
        roster: &Roster,
        state: &AutoswitchState,
        decision: &Decision,
    ) -> Vec<PaidTarget> {
        let now = self.now();
        let mut ranked = Vec::new();
        let mut fresh = BTreeMap::new();
        for &number in &decision.census.oauth {
            let Some(record) = roster.sequence.switchable_account(number) else {
                continue;
            };
            let prepaid = cached_prepaid(decision, number);
            match paid_eligible(&self.paid_candidate(&record, state, decision, prepaid), now) {
                Ok(balance) => {
                    fresh.insert(number, balance.clone());
                    ranked.push((number, balance));
                }
                Err(Ineligible::StaleBalance) => {
                    ranked.extend(
                        prepaid
                            .and_then(|prepaid| cached_balance(prepaid, now))
                            .map(|balance| (number, balance)),
                    );
                }
                Err(_) => {}
            }
        }
        paid_ranking(&ranked, roster.sequence.numbers())
            .into_iter()
            .map(|number| PaidTarget {
                number,
                fresh: fresh.remove(&number),
            })
            .collect()
    }

    /// Enters the paid fallback on the active account, with no switch: under the state file's lock, with the lease
    /// checked again there, `paidFallback` is written with `from` the account itself, only through
    /// [`SwitchFn::write_while_on`], which judges auto-switch on again under the switch's own guard; and the entered event
    /// emitted when `announce` (not when an unrecorded paid move onto it already emitted it). Auto-switch off by then
    /// ends the tick paused, and a guard held by a switch in flight leaves the entry to a later tick; neither writes.
    /// With the tick's outcome, whether the entry was recorded.
    fn enter_in_place(
        &mut self,
        holder: &LeaseHolder,
        active: u32,
        lease_owner: &LeaseOwner,
        balance: PaidBalance,
        announce: bool,
    ) -> (TickOutcome, bool) {
        if !self.inputs.auto_switch_enabled {
            return (auto_switch_off(), false);
        }
        let root = self.store().root();
        let processes = self.inputs.processes;
        let switcher = self.inputs.switcher;
        let now = self.now();
        let written = AutoswitchState::update_if(root, ACCOUNT_LOCK_TIMEOUT, |state| {
            let observed = Lease::new(root).observe(holder, now, processes);
            let mut wrote = false;
            let entered = observed.map(|lease| {
                if !matches!(lease, LeaseState::Ours) {
                    return Err(lease);
                }
                Ok(switcher.write_while_on(&mut || {
                    state.paid_fallback = Some(PaidFallback::new(
                        active,
                        active,
                        secs_from_timestamp(now),
                        lease_owner.clone(),
                    ));
                    wrote = true;
                }))
            });
            (entered, wrote)
        });
        match written {
            Ok(Ok(Ok(WhileOn::Off))) => (auto_switch_off(), false),
            Ok(Ok(Ok(WhileOn::Busy))) => {
                tracing::info!(
                    account = active,
                    "a switch holds the switch gate; the paid fallback is entered in place at a later tick"
                );
                (TickOutcome::NoAction, false)
            }
            Ok(Ok(Ok(WhileOn::Ran))) => {
                if announce {
                    self.emit(EventBody::PaidFallbackEntered {
                        number: active,
                        from: active,
                        amount: balance.amount,
                        currency: balance.currency,
                    });
                }
                (TickOutcome::NoAction, true)
            }
            Ok(Ok(Err(lost))) => (
                TickOutcome::Paused {
                    reason: lease_lost(&lost),
                },
                false,
            ),
            Ok(Err(error)) => (
                self.error(
                    format!("the engine lease could not be read under the state lock: {error}"),
                    true,
                ),
                false,
            ),
            Err(error) => (
                self.error(
                    format!(
                        "the paid fallback could not be recorded in autoswitch_state.json: {error}"
                    ),
                    true,
                ),
                false,
            ),
        }
    }

    /// Freshens each paid target in order, makes sure its balance is fresh ([`Self::entry_balance`]) and switches to
    /// the first one ready, with the paid move's precondition. A target whose balance cannot be had, or that the
    /// switch's precondition refuses, is skipped for the next; with none left, `stale-balance`.
    fn paid_switch(
        &mut self,
        engine: &mut Engine,
        roster: &Roster,
        state: &AutoswitchState,
        decision: &Decision,
        plan: &PaidPlan,
    ) -> TickOutcome {
        let active = roster.active.number;
        let left = self.left_snapshot(decision, active);
        let mut failures = FreshenFailures::default();
        let mut balance_failed = false;
        for target in &plan.targets {
            let Some(account) = roster.sequence.switchable_account(target.number) else {
                continue;
            };
            if !self.freshen(engine, &account, &mut failures) {
                if failures.draining {
                    return TickOutcome::NoAction;
                }
                continue;
            }
            let Some((prepaid, balance)) =
                self.entry_balance(engine, state, decision, &account, target)
            else {
                balance_failed = true;
                continue;
            };
            let paid = PaidMove {
                from: active,
                entered_from: plan.entered_from,
                prepaid,
                balance,
                lease_owner: plan.lease_owner.clone(),
            };
            let kind = Move::Paid {
                paid: Box::new(paid),
                opted_out: plan.opted_out,
            };
            if let Some(outcome) = self.perform(engine, Trigger::AtLimit, target.number, left, kind)
            {
                return outcome;
            }
        }
        if failures.systemic.is_none() && (balance_failed || self.precondition_refused) {
            self.no_switch(
                "stale-balance",
                "no paid target's balance is fresh and positive this tick; retrying",
            );
            return TickOutcome::Blocked;
        }
        self.no_target(&failures)
    }

    /// The balance a paid move to `account` is decided on: the cached one when it is fresh, else one balance-only read
    /// with the token the freshen left, at most once per [`PAID_STAY_READ_S`] per account, judged again by
    /// [`paid_eligible`]. `None` when there is none to spend on.
    fn entry_balance(
        &mut self,
        engine: &mut Engine,
        state: &AutoswitchState,
        decision: &Decision,
        account: &AccountRecord,
        target: &PaidTarget,
    ) -> Option<(PrepaidEntry, PaidBalance)> {
        let number = account.number;
        if let Some(balance) = &target.fresh {
            return cached_prepaid(decision, number)
                .map(|prepaid| (prepaid.clone(), balance.clone()));
        }
        let now = self.now();
        if engine.balance_reads.get(&number).is_some_and(|last| {
            secs_from_timestamp(now) - secs_from_timestamp(*last) < PAID_STAY_READ_S
        }) {
            tracing::info!(
                account = number,
                "the paid target's balance was read under 2 minutes ago; it is skipped this tick"
            );
            return None;
        }
        engine.balance_reads.insert(number, now);
        let prepaid = self.read_target_balance(account)?;
        match paid_eligible(
            &self.paid_candidate(account, state, decision, Some(&prepaid)),
            self.now(),
        ) {
            Ok(balance) => Some((prepaid, balance)),
            Err(reason) => {
                tracing::info!(account = number, reason = %reason, "the paid target's fresh balance is not spendable; the next target is tried");
                None
            }
        }
    }

    /// One balance-only read of `account`'s balance with its stored access token; `None` when the token cannot be read
    /// or the read fails, a foreign live credential's refusal included.
    fn read_target_balance(&self, account: &AccountRecord) -> Option<PrepaidEntry> {
        let number = account.number;
        let token = match self.store().read_credentials(number) {
            Ok(Some(blob)) => blob
                .oauth()
                .map(|oauth| AccessToken::new(oauth.access_token().expose().to_owned())),
            Ok(None) => None,
            Err(error) => {
                tracing::warn!(account = number, kind = ?error.kind(), "the paid target's credential could not be read; its balance is not read");
                None
            }
        };
        let Some(token) = token else {
            tracing::info!(
                account = number,
                "the paid target has no access token; its balance is not read"
            );
            return None;
        };
        match self.inputs.collector.read_balance(
            self.inputs.context,
            number,
            &token,
            &account.org_uuid,
        ) {
            Ok(prepaid) => Some(prepaid),
            Err(error) => {
                tracing::info!(account = number, error = %error, "the paid target's balance read failed; the next target is tried");
                None
            }
        }
    }

    /// Why no target was switched to, once every one was tried: the most actionable systemic freshen refusal, a
    /// transient failure, or no viable target.
    fn no_target(&mut self, failures: &FreshenFailures) -> TickOutcome {
        if let Some(index) = failures.systemic {
            let message = SYSTEMIC.get(index).map_or("", |(_, message)| message);
            return self.error(format!("could not freshen: {message}"), true);
        }
        if failures.transient {
            return self.error(
                "could not freshen any candidate (network?)".to_owned(),
                true,
            );
        }
        self.no_switch("no-viable-target", "");
        TickOutcome::Blocked
    }

    /// cswap's freshness gate on a consume-first target (autoswitch.py:1387-1409): when phase 2 ran and its refetch did
    /// not leave `number` fetched within [`SERVE_TTL_S`] (a failure backoff or a concurrent poller kept it off), the
    /// tick holds with `stale-usage` instead of switching on old data or moving to a worse-ranked target. The hold
    /// writes nothing. An escape never ran phase 2 and is never gated.
    fn stale_target(&mut self, decision: &Decision, number: u32) -> bool {
        let Some(entries) = &decision.fresh else {
            return false;
        };
        if entries
            .get(&number)
            .is_some_and(|entry| entry.fresh(self.now(), SERVE_TTL_S))
        {
            return false;
        }
        self.no_switch(
            "stale-usage",
            format!("account {number} usage could not be refreshed this tick (backoff or a concurrent poller); retrying"),
        );
        true
    }

    /// Whether `account` is ready to switch to, cswap's freshen statuses: a dead grant and a config backup naming
    /// another login ([`IDENTITY_MISMATCH`]) are quarantined here, an identity conflict was quarantined by
    /// [`freshen_target`] (or, when that write failed, is kept out of the census until a later tick writes it), and the
    /// drain stops the tick.
    fn freshen(
        &mut self,
        engine: &mut Engine,
        account: &AccountRecord,
        failures: &mut FreshenFailures,
    ) -> bool {
        let freshened = match freshen_target(self.inputs.context, account) {
            Ok(Freshened::Ready) => return true,
            Ok(Freshened::Failed(GrantError::Draining)) => {
                failures.draining = true;
                return false;
            }
            Ok(freshened) => freshened,
            Err(error) => {
                tracing::warn!(account = account.number, error = %error, "the target's identity-conflict quarantine was not written");
                engine.conflicted.insert(account.number);
                failures.transient = true;
                return false;
            }
        };
        let kind = freshened.kind();
        if kind == IDENTITY_CONFLICT {
            self.quarantined(account, IDENTITY_CONFLICT);
        } else if kind == IDENTITY_MISMATCH {
            self.quarantine(account, IDENTITY_MISMATCH, failures);
        } else if kind == "invalid_grant" {
            self.quarantine(account, "invalid_grant", failures);
        } else if let Some(index) = SYSTEMIC.iter().position(|(systemic, _)| *systemic == kind) {
            failures.systemic = Some(failures.systemic.map_or(index, |known| known.min(index)));
        } else {
            failures.transient = true;
        }
        false
    }

    fn quarantine(
        &mut self,
        account: &AccountRecord,
        reason: &str,
        failures: &mut FreshenFailures,
    ) {
        match quarantine(
            self.store(),
            account.number,
            &account.email,
            reason,
            self.now(),
        ) {
            Ok(()) => self.quarantined(account, reason),
            Err(error) => {
                tracing::warn!(account = account.number, error = %error, reason, "the account's quarantine was not written");
                failures.transient = true;
            }
        }
    }

    /// Quarantines account `number` as [`IDENTITY_MISMATCH`], with the event, after the switch found its config backup
    /// naming another login than its record (as `accounts.json` reads now). A quarantine that cannot be written is
    /// logged, and the account is tried again on a later tick.
    fn quarantine_mismatch(&mut self, number: u32) {
        let Some(account) = self
            .store()
            .read_sequence()
            .ok()
            .flatten()
            .and_then(|sequence| sequence.account(number))
        else {
            tracing::warn!(
                account = number,
                "accounts.json could not be read; the identity-mismatch quarantine was not written"
            );
            return;
        };
        match quarantine(
            self.store(),
            number,
            &account.email,
            IDENTITY_MISMATCH,
            self.now(),
        ) {
            Ok(()) => self.quarantined(&account, IDENTITY_MISMATCH),
            Err(error) => tracing::warn!(
                account = number,
                error = %error,
                "the identity-mismatch quarantine was not written; the account is tried again on a later tick"
            ),
        }
    }

    fn quarantined(&mut self, account: &AccountRecord, reason: &str) {
        self.emit(EventBody::Quarantined {
            number: account.number,
            email: account.email.as_str().to_owned(),
            reason: reason.to_owned(),
        });
    }

    /// Switches to `number` under the state file's lock; `None` when the switch refused the target, or it was
    /// quarantined since the tick read the state, to try the next. An exit move carries the precondition that the live
    /// login is still the paid account it leaves; its refusal means the owner moved the login, so the exit stops with
    /// no action. A paid move carries its precondition (the live login is still the
    /// account it leaves, and the target is opted in and enabled), is recorded as the paid fallback, and emits the
    /// entered event once the login moved. A move carrying the owner's leave to move off paid mode ([`OptedOut`]) also
    /// carries the precondition that the paid account's "May spend credits" is still off.
    /// While auto-switch is off only such a move is made; any other ends the tick paused.
    fn perform(
        &mut self,
        engine: &mut Engine,
        trigger: Trigger,
        number: u32,
        left: LeftSnapshot,
        kind: Move,
    ) -> Option<TickOutcome> {
        if !self.inputs.auto_switch_enabled && !kind.opted_out() {
            return Some(auto_switch_off());
        }
        self.perform_switch(engine, trigger, number, left, kind)
    }

    /// [`Self::perform`] with auto-switch on.
    fn perform_switch(
        &mut self,
        engine: &mut Engine,
        trigger: Trigger,
        number: u32,
        left: LeftSnapshot,
        kind: Move,
    ) -> Option<TickOutcome> {
        let holder = self.holder(engine);
        // A refused move that carried the owner's leave stops the exit as a refused exit move does: the flag may be on
        // again, and the next tick retries while the move is still permitted.
        let exit = matches!(kind, Move::Exit { .. }) || kind.opted_out();
        let (precondition, paid) = precondition_of(kind);
        let entered = paid.as_ref().map(|paid| EventBody::PaidFallbackEntered {
            number,
            from: paid.from,
            amount: paid.balance.amount,
            currency: paid.balance.currency.clone(),
        });
        let request = PerformRequest {
            context: self.inputs.context,
            switcher: self.inputs.switcher,
            number,
            trigger,
            left,
            settings: &self.settings,
            holder: &holder,
            processes: self.inputs.processes,
            remembered_switch_at: engine.last_switch_at,
            precondition,
            paid,
        };
        match perform(&request) {
            Performed::Switched(report) => {
                self.emit_switched(trigger, report, entered);
                Some(TickOutcome::Switched)
            }
            Performed::Unrecorded(report, error) => {
                self.unrecorded(engine, trigger, report, &error, entered);
                Some(TickOutcome::Switched)
            }
            Performed::Cooldown => {
                self.no_switch("cooldown", "");
                Some(TickOutcome::NoAction)
            }
            Performed::Quarantined => {
                tracing::info!(
                    account = number,
                    "the target was quarantined since the tick read the state; the next candidate is tried"
                );
                None
            }
            Performed::PreconditionFailed => self.precondition_failed(number, exit),
            Performed::LeaseLost(lease) => Some(TickOutcome::Paused {
                reason: lease_lost(&lease),
            }),
            Performed::AlreadyActive(reason) => {
                self.no_switch("already-active", reason);
                Some(TickOutcome::NoAction)
            }
            Performed::Failed(failure) => self.react(engine, number, failure),
            Performed::Unavailable(message) => Some(self.error(message, true)),
            Performed::Refused(error) => Some(self.refused(&holder, &error)),
        }
    }

    /// The switch event of a move that moved the login, then the paid fallback's entered event when it was a paid move.
    fn emit_switched(
        &mut self,
        trigger: Trigger,
        report: crate::engine::SwitchReport,
        entered: Option<EventBody>,
    ) {
        self.emit_switch(trigger, report);
        if let Some(body) = entered {
            self.emit(body);
        }
    }

    /// A move that moved the login but could not be recorded in the state file: the engine remembers the switch (and a
    /// paid move's target as its unrecorded entry), emits its events, and reports the failed record as a transient error.
    fn unrecorded(
        &mut self,
        engine: &mut Engine,
        trigger: Trigger,
        report: crate::engine::SwitchReport,
        error: &EngineError,
        entered: Option<EventBody>,
    ) {
        engine.last_switch_at = Some(secs_from_timestamp(self.now()));
        if let Some(EventBody::PaidFallbackEntered { number, .. }) = &entered {
            engine.unrecorded_entry = Some(*number);
        }
        self.emit_switched(trigger, report, entered);
        self.emit(EventBody::Error {
            message: format!("the switch was not recorded in autoswitch_state.json: {error}"),
            transient: true,
        });
    }

    /// A switch to `number` whose precondition no longer held under its locks. An exit move stops with no action: the
    /// owner moved the login, or turned its credits back on. Any other move skips the target for the next.
    fn precondition_failed(&mut self, number: u32, exit: bool) -> Option<TickOutcome> {
        if exit {
            tracing::info!(
                account = number,
                "the owner moved the login, or turned its credits back on; the exit is not made"
            );
            return Some(TickOutcome::NoAction);
        }
        self.precondition_refused = true;
        tracing::info!(
            account = number,
            "the switch's precondition no longer held under its locks; the next candidate is tried"
        );
        None
    }

    /// A state file of a newer version refused the switch's record: the engine releases its lease and pauses.
    fn refused(&self, holder: &LeaseHolder, error: &EngineError) -> TickOutcome {
        tracing::warn!(
            "the auto-switch state file is of a newer version; the auto-switch engine releases its lease and pauses"
        );
        self.release_lease(
            holder,
            "the auto-switch engine could not release its lease beside a newer state file",
        );
        TickOutcome::Paused {
            reason: error.to_string(),
        }
    }

    fn emit_switch(&mut self, trigger: Trigger, report: crate::engine::SwitchReport) {
        self.emit(EventBody::Switch {
            trigger,
            from: report.from,
            to: report.to,
            warnings: report.warnings,
        });
    }

    /// The engine's reaction to a failed switch to account `number`: skip the target (`None`), quarantine it and skip
    /// it, a transient error, a pause, a silent stop, or a halt until restart.
    fn react(
        &mut self,
        engine: &mut Engine,
        number: u32,
        failure: SwitchFailure,
    ) -> Option<TickOutcome> {
        let kind = failure.kind;
        match kind {
            SwitchErrorKind::TargetIdentityMismatch => {
                self.quarantine_mismatch(number);
                None
            }
            SwitchErrorKind::TargetBackupMissing
            | SwitchErrorKind::TargetBackupUnreadable
            | SwitchErrorKind::ApiKeyAccount
            | SwitchErrorKind::UnknownAccount
            | SwitchErrorKind::PreconditionFailed => {
                tracing::info!(
                    kind = kind.as_str(),
                    "the switch refused the target; the next candidate is tried"
                );
                None
            }
            SwitchErrorKind::LocksBusy
            | SwitchErrorKind::LockFailed
            | SwitchErrorKind::LiveUnreadable
            | SwitchErrorKind::ConfigUnreadable
            | SwitchErrorKind::OutgoingBackupFailed
            | SwitchErrorKind::WriteFailed => Some(self.error(kind.to_string(), true)),
            SwitchErrorKind::StoreUnreadable
            | SwitchErrorKind::StoreReadOnly
            | SwitchErrorKind::UnknownSchema
            | SwitchErrorKind::StoreUnmirrored => Some(TickOutcome::Paused {
                reason: kind.to_string(),
            }),
            SwitchErrorKind::Draining => Some(TickOutcome::NoAction),
            SwitchErrorKind::AutoSwitchOff => Some(auto_switch_off()),
            SwitchErrorKind::RollbackFailed => {
                engine.halted = true;
                let left = failure.login.map_or("unknown", LoginState::as_str);
                let message = format!(
                    "{kind}; the live login was left: {left}; the engine decides nothing until it is restarted"
                );
                tracing::error!(
                    left,
                    "a switch failed and its rollback failed too; the auto-switch engine halts"
                );
                self.emit(EventBody::Error {
                    message,
                    transient: false,
                });
                Some(TickOutcome::Halted)
            }
        }
    }
}

/// cswap's `_account_is_switchable` and its disabled check: an enabled account with a stored credential and a config
/// backup.
fn switchable(store: &Store, record: &AccountRecord) -> bool {
    !record.disabled
        && store
            .read_credentials(record.number)
            .is_ok_and(|credential| credential.is_some())
        && store
            .read_account_config(record.number)
            .is_ok_and(|config| config.is_some())
}

/// The precondition a move of kind `kind` carries, and the paid move a paid one records: none for a normal move; the
/// live login still the paid account for an exit; that, and the target opted in and enabled, for a paid move; and, for
/// a move with the owner's leave, the paid account's "May spend credits" still off.
fn precondition_of(kind: Move) -> (Option<SwitchPrecondition>, Option<PaidMove>) {
    match kind {
        Move::Normal => (None, None),
        Move::Exit { from, opted_out } => (
            Some(SwitchPrecondition {
                expected_outgoing: from,
                target_must_be_paid_opted_in: false,
                outgoing_must_be_opted_out: opted_out.is_some(),
            }),
            None,
        ),
        Move::Paid { paid, opted_out } => (
            Some(SwitchPrecondition {
                expected_outgoing: paid.from,
                target_must_be_paid_opted_in: true,
                outgoing_must_be_opted_out: opted_out.is_some(),
            }),
            Some(*paid),
        ),
    }
}

/// The pause of a tick that ran while auto-switch is off.
fn auto_switch_off() -> TickOutcome {
    TickOutcome::Paused {
        reason: AUTO_SWITCH_OFF.to_owned(),
    }
}

/// The pause reason of a lease another engine holds.
fn held_by_other(owner: &str, pid: u32) -> String {
    format!("another engine holds the lease: {owner} (pid {pid})")
}

/// The pause reason of a lease found not to be ours under the state lock.
fn lease_lost(lease: &LeaseState) -> String {
    match lease {
        LeaseState::HeldByOther { owner, pid, .. } => held_by_other(owner, *pid),
        LeaseState::Free | LeaseState::Ours => {
            "the engine lease is no longer this engine's".to_owned()
        }
    }
}

/// The configured model names cswap checks, each by its lowercase form beside its spelling: every name but `all`.
/// `models` are the settings' `models`, whose read already kept each name once.
fn wanted_models(models: &[String]) -> Vec<(String, String)> {
    models
        .iter()
        .map(|model| (model.to_lowercase(), model.clone()))
        .filter(|(low, _)| low != "all")
        .collect()
}

/// The `wanted` names no reading's scoped windows carry, `None` when there is no reading to prove it by.
fn missing_models(wanted: &[(String, String)], readings: &[&Value]) -> Option<Vec<String>> {
    if readings.is_empty() || !readings.iter().all(|reading| reading.is_object()) {
        return None;
    }
    let mut seen = BTreeSet::new();
    for reading in readings {
        for scoped in reading
            .get("scoped")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(name) = scoped.get("name").and_then(Value::as_str) {
                seen.insert(name.to_lowercase());
            }
        }
    }
    Some(
        wanted
            .iter()
            .filter(|(low, _)| !seen.contains(low))
            .map(|(_, name)| name.clone())
            .collect(),
    )
}

/// Each account's reading a decision may trust: its usage table entry's [`decision_value`] at `now`, unless its poll
/// result is a sentinel.
fn trusted_readings(
    collected: &Collected,
    models: &[String],
    now: Timestamp,
) -> BTreeMap<u32, Value> {
    let mut readings = BTreeMap::new();
    for (number, entry) in &collected.entries {
        if collected.results.get(number).is_some_and(is_sentinel) {
            continue;
        }
        if let Some(reading) = decision_value(entry, models, now) {
            readings.insert(*number, reading.clone());
        }
    }
    readings
}

/// The active account's reading: an expired token when the collection's result says so, else its trusted headroom.
fn active_reading(usage: &TickUsage, models: &[String], active: u32) -> ActiveReading {
    if usage
        .collected
        .results
        .get(&active)
        .is_some_and(shows_expired)
    {
        return ActiveReading::TokenExpired;
    }
    headroom_of(&usage.readings, models, active)
        .map_or(ActiveReading::Unreadable, ActiveReading::Headroom)
}

/// Logs the lifting of the owner-exit bar on account `number`; a bar salvaged from a malformed paid fallback names
/// account 0, which is no account, so it is logged without one.
fn log_bar_lifted(number: u32) {
    if number == 0 {
        tracing::info!(
            "the owner-exit bar of a malformed paid fallback is lifted; the paid fallback may be entered again"
        );
    } else {
        tracing::info!(
            account = number,
            "the owner-exit bar is lifted; the paid fallback may be entered again"
        );
    }
}

/// The accounts whose credits toggle is on, in ascending order: the set an owner-exit bar is compared with.
fn opted_in(sequence: &Sequence) -> Vec<u32> {
    let mut numbers: Vec<u32> = sequence
        .every_record()
        .filter(|record| record.may_spend_credits)
        .map(|record| record.number)
        .collect();
    numbers.sort_unstable();
    numbers.dedup();
    numbers
}

/// The paid-fallback-left event's reason for an eligibility clause the paid account failed.
fn left_reason(reason: Ineligible) -> &'static str {
    match reason {
        Ineligible::FlagOff => "flag-off",
        Ineligible::BalanceZero => "balance-zero",
        Ineligible::ExtraUsageOff
        | Ineligible::UserDisabled
        | Ineligible::SpendLimitReached
        | Ineligible::DisabledReasonSet => "extra-usage-off",
        Ineligible::StaleBalance => "stale-balance",
        Ineligible::NotOauth
        | Ineligible::Disabled
        | Ineligible::NotSwitchable
        | Ineligible::ExtraUsageUnknown
        | Ineligible::NoBalance
        | Ineligible::CurrencyMissing => "ineligible",
    }
}

/// Account `number`'s prepaid balance as the decision's usage table entry holds it.
fn cached_prepaid(decision: &Decision, number: u32) -> Option<&PrepaidEntry> {
    decision
        .entries
        .get(&number)
        .and_then(|entry| entry.prepaid.as_ref())
}

fn usage_of(
    readings: &BTreeMap<u32, Value>,
    models: &[String],
    number: u32,
) -> Option<AccountUsage> {
    readings
        .get(&number)
        .map(|reading| AccountUsage::from_reading(reading, models))
}

fn headroom_of(readings: &BTreeMap<u32, Value>, models: &[String], number: u32) -> Option<f64> {
    usage_of(readings, models, number)?.headroom()
}

/// cswap's `_window_pcts`: the windows the decision reads, each with its utilization: `5h`, `7d`, then each scoped
/// window `models` names, under its own name.
fn window_pcts(reading: &Value, models: &[String]) -> Vec<(String, f64)> {
    relevant_windows(reading, models)
        .into_iter()
        .map(|(label, window)| (label, window.pct))
        .collect()
}

/// cswap's `_earliest_recovery`: the soonest moment any exhausted account (the active one included) is usable again,
/// each account's being the latest reset among its windows at 100 % or more, the scoped windows `models` names among
/// them. An exhausted account whose recovery cannot be proven (no reset, or one not after `now`) makes the answer
/// unknown.
fn earliest_recovery(
    readings: &BTreeMap<u32, Value>,
    models: &[String],
    now: Timestamp,
) -> Option<Timestamp> {
    let mut earliest: Option<Timestamp> = None;
    for reading in readings.values() {
        let usage = AccountUsage::from_reading(reading, models);
        let mut blocked = usage
            .windows
            .iter()
            .filter(|window| window.pct >= 100.0)
            .peekable();
        if blocked.peek().is_none() {
            continue;
        }
        let usable_at = blocked
            .filter_map(|window| window.resets_at)
            .max()
            .filter(|usable_at| *usable_at > now)?;
        earliest = Some(earliest.map_or(usable_at, |earliest| earliest.min(usable_at)));
    }
    earliest
}

/// A percentage for a detail line, cswap's `pct_label`: up to eight decimals, trailing zeros dropped, so float noise
/// such as 95.00000000000001 reads as 95 and 99.9 never as 100.
fn pct_label(value: f64) -> String {
    let text = format!("{value:.8}");
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}
