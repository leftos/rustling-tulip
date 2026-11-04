//! The auto-switch engine's tick, [`Engine::tick`], ported from cswap's `test_autoswitch.py` one test per cswap test:
//! a scratch account store whose account 1 is Claude Code's live login, with a fake usage collection (cswap's tests patch
//! `usage_entries_by_account`; these set the fake collection's entries), a fake switch that moves the live login, a fake
//! token endpoint and a process table.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use claude_accounts::engine::{
    AccountRef, ActiveHealth, Collected, Collector, Engine, EngineError, Event, EventBody, Lease,
    LeaseHolder, LeaseState, NO_RESET_FALLBACK_S, ReleaseReason, ScheduleInputs, SwitchFailure,
    SwitchFn, SwitchReport, TickInputs, TickOutcome, TickReport, TransportCollector, Trigger,
    WarmUp, WhileOn, next_delay, quarantine,
};
use claude_accounts::{
    AccessToken, AccountLock, AccountsRoot, BalanceReadError, ClaudePaths, CredentialBlob, Drain,
    Email, LockTiming, LoginState, PollError, PollOutcome, PollResult, PrepaidEntry, ProbeVerdicts,
    ProfileTransport, RawProfile, RawToken, RawUsage, RefreshContext, RefreshRequest,
    RefreshTransport, Store, SwitchErrorKind, SwitchPrecondition, TransportError, UsageEntry,
    UsageTransport, poll_once_with_refresh,
};
use claude_accounts::{AutoSwitchSettings, Strategy};
use jiff::{SignedDuration, Timestamp};
use proc_table::{ProcessInfo, ProcessTable};
use serde_json::{Map, Value, json};

use crate::cadence::INTERVAL;
use crate::support::{NOW_S, Scratch, at, read_json, write};

/// An `expiresAt` far in the future, in milliseconds.
const FAR: i64 = 4_102_444_800_000;

/// An hour, in seconds.
const H: i64 = 3600;

fn email(number: u32) -> String {
    format!("account{number}@example.test")
}

fn address(number: u32) -> Email {
    Email::parse(&email(number)).expect("the email is safe")
}

fn uuid(number: u32) -> String {
    format!("00000000-0000-4000-8000-0000000000{number:02}")
}

/// cswap's `_usage`: the 5-hour window at `pct` and an empty 7-day window.
fn usage(pct: f64) -> Value {
    json!({"five_hour": {"pct": pct}, "seven_day": {"pct": 0.0}})
}

/// [`usage`] with the 5-hour window resetting at `reset`.
fn usage_at(pct: f64, reset: &str) -> Value {
    json!({"five_hour": {"pct": pct, "resets_at": reset}, "seven_day": {"pct": 0.0}})
}

/// cswap's `_usage7`: both windows, the 7-day one resetting at `reset`.
fn usage7(pct5: f64, pct7: f64, reset: &str) -> Value {
    json!({"five_hour": {"pct": pct5}, "seven_day": {"pct": pct7, "resets_at": reset}})
}

/// A credential whose tokens are `fake-access-<n>` and `fake-refresh-<n>`, lapsing at `expires_at_ms`.
fn credential(number: u32, expires_at_ms: i64) -> String {
    json!({ "claudeAiOauth": {
        "accessToken": format!("fake-access-{number}"),
        "refreshToken": format!("fake-refresh-{number}"),
        "expiresAt": expires_at_ms,
    } })
    .to_string()
}

/// Makes account `number` Claude Code's live login, as `~/.claude.json`'s `oauthAccount` names it.
fn write_live(claude: &ClaudePaths, number: u32) {
    write(
        claude.global_config(),
        &json!({"oauthAccount": {"emailAddress": email(number), "organizationUuid": ""}})
            .to_string(),
    );
}

/// The account `~/.claude.json` names, by its email.
fn live_number(claude: &ClaudePaths) -> Option<u32> {
    let config = read_json(claude.global_config());
    let address = config["oauthAccount"]["emailAddress"].as_str()?;
    address
        .strip_prefix("account")?
        .strip_suffix("@example.test")?
        .parse()
        .ok()
}

/// A usage collection that hands the tick the entries and results a test set, and remembers how it was asked.
#[derive(Default)]
struct FakeCollection {
    collected: RefCell<Collected>,
    /// The warm-up's settled count; every member when unset.
    warm_settled: Cell<Option<usize>>,
    warm_calls: RefCell<Vec<Vec<u32>>>,
    scheduled_calls: Cell<usize>,
    /// What the phase-2 refetch answers; `collected` when unset.
    refetched: RefCell<Option<Collected>>,
    /// Whether the phase-2 refetch fails.
    refetch_fails: Cell<bool>,
    refetch_calls: RefCell<Vec<Vec<u32>>>,
    /// The account of each exit target's re-read, in order.
    recheck_calls: RefCell<Vec<u32>>,
    /// The usage endpoint the exit target's re-read reaches through the crate's own collection; when unset, the re-read
    /// answers as the phase-2 refetch does.
    recheck_over: RefCell<Option<Rc<dyn UsageTransport>>>,
    /// What each balance read answers, in order; `Unsupported` once it runs dry.
    balance_answers: RefCell<VecDeque<Result<PrepaidEntry, BalanceReadError>>>,
    /// Each balance read's account and token.
    balance_calls: RefCell<Vec<(u32, String)>>,
}

impl FakeCollection {
    fn calls(&self) -> usize {
        self.warm_calls.borrow().len() + self.scheduled_calls.get()
    }

    /// The accounts each balance read was for, in order.
    fn balance_reads(&self) -> Vec<u32> {
        self.balance_calls
            .borrow()
            .iter()
            .map(|(number, _)| *number)
            .collect()
    }
}

impl Collector for FakeCollection {
    fn warmup(
        &self,
        _context: &RefreshContext<'_>,
        _settings: &AutoSwitchSettings,
        _current: u32,
        candidates: &[u32],
        _warm_since: Timestamp,
    ) -> Result<WarmUp, PollError> {
        self.warm_calls.borrow_mut().push(candidates.to_vec());
        let total = candidates.len() + 1;
        Ok(WarmUp {
            collected: self.collected.borrow().clone(),
            settled: self.warm_settled.get().unwrap_or(total),
            total,
        })
    }

    fn scheduled(
        &self,
        _context: &RefreshContext<'_>,
        _settings: &AutoSwitchSettings,
        _inputs: &ScheduleInputs<'_>,
    ) -> Result<Collected, PollError> {
        self.scheduled_calls.set(self.scheduled_calls.get() + 1);
        Ok(self.collected.borrow().clone())
    }

    fn refetch(
        &self,
        _context: &RefreshContext<'_>,
        _settings: &AutoSwitchSettings,
        members: &[u32],
    ) -> Result<Collected, PollError> {
        self.refetch_calls.borrow_mut().push(members.to_vec());
        if self.refetch_fails.get() {
            return Err(PollError::UnsupportedCache);
        }
        let refetched = self.refetched.borrow().clone();
        Ok(refetched.unwrap_or_else(|| self.collected.borrow().clone()))
    }

    fn recheck(
        &self,
        context: &RefreshContext<'_>,
        settings: &AutoSwitchSettings,
        number: u32,
    ) -> Result<Collected, PollError> {
        self.recheck_calls.borrow_mut().push(number);
        let over = self.recheck_over.borrow().clone();
        if let Some(transport) = over {
            return TransportCollector(&*transport).recheck(context, settings, number);
        }
        if self.refetch_fails.get() {
            return Err(PollError::UnsupportedCache);
        }
        let refetched = self.refetched.borrow().clone();
        Ok(refetched.unwrap_or_else(|| self.collected.borrow().clone()))
    }

    fn read_balance(
        &self,
        _context: &RefreshContext<'_>,
        number: u32,
        token: &AccessToken,
        _org_uuid: &str,
    ) -> Result<PrepaidEntry, BalanceReadError> {
        self.balance_calls
            .borrow_mut()
            .push((number, token.expose().to_owned()));
        self.balance_answers
            .borrow_mut()
            .pop_front()
            .unwrap_or(Err(BalanceReadError::Unsupported))
    }
}

/// A collector that keeps the trait's default balance read and collects nothing.
struct DefaultBalance;

impl Collector for DefaultBalance {
    fn warmup(
        &self,
        _context: &RefreshContext<'_>,
        _settings: &AutoSwitchSettings,
        _current: u32,
        _candidates: &[u32],
        _warm_since: Timestamp,
    ) -> Result<WarmUp, PollError> {
        Err(PollError::UnsupportedCache)
    }

    fn scheduled(
        &self,
        _context: &RefreshContext<'_>,
        _settings: &AutoSwitchSettings,
        _inputs: &ScheduleInputs<'_>,
    ) -> Result<Collected, PollError> {
        Err(PollError::UnsupportedCache)
    }

    fn refetch(
        &self,
        _context: &RefreshContext<'_>,
        _settings: &AutoSwitchSettings,
        _members: &[u32],
    ) -> Result<Collected, PollError> {
        Err(PollError::UnsupportedCache)
    }

    fn recheck(
        &self,
        _context: &RefreshContext<'_>,
        _settings: &AutoSwitchSettings,
        _number: u32,
    ) -> Result<Collected, PollError> {
        Err(PollError::UnsupportedCache)
    }
}

/// A usage endpoint that answers every request 401, and counts its answers.
#[derive(Default)]
struct Unauthorized {
    answers: Cell<u32>,
}

impl UsageTransport for Unauthorized {
    fn get_usage(&self, _token: &AccessToken) -> Result<RawUsage, TransportError> {
        self.answers.set(self.answers.get() + 1);
        Ok(RawUsage {
            status: 401,
            body: Value::Null,
            retry_after: None,
        })
    }
}

/// Runs once, just before a switch: the moment between the tick's decision and the switch's locks.
type BeforeSwitch = Box<dyn FnOnce()>;

/// A switch that judges the caller's precondition as the crate's switch does (the live login is the expected one, a
/// paid move's target is enabled and opted in, and a leave the owner permitted leaves an account still opted out, as
/// `accounts.json` reads at the switch), then answers from a queue and, once it runs dry, moves the live login to the
/// target. Its in-place writes answer from their own queue, running the write once it runs dry.
#[derive(Default)]
struct FakeSwitch {
    answers: RefCell<VecDeque<Result<SwitchReport, SwitchFailure>>>,
    calls: RefCell<Vec<u32>>,
    preconditions: RefCell<Vec<Option<SwitchPrecondition>>>,
    before: RefCell<Option<BeforeSwitch>>,
    while_on: RefCell<VecDeque<WhileOn>>,
    while_on_calls: Cell<usize>,
}

/// Whether `precondition` holds for a switch to `number`, judged on `accounts.json` and the live login as they are now.
fn precondition_holds(
    context: &RefreshContext<'_>,
    number: u32,
    precondition: SwitchPrecondition,
) -> bool {
    let accounts = read_json(&context.store.root().accounts_json());
    let record = &accounts["accounts"][number.to_string()];
    let opted_in = record["maySpendCredits"] == json!(true) && record["disabled"] != json!(true);
    let outgoing = &accounts["accounts"][precondition.expected_outgoing.to_string()];
    let opted_out = outgoing["maySpendCredits"] != json!(true);
    live_number(context.claude) == Some(precondition.expected_outgoing)
        && (!precondition.target_must_be_paid_opted_in || opted_in)
        && (!precondition.outgoing_must_be_opted_out || opted_out)
}

impl FakeSwitch {
    /// Runs `hook` once, just before the next switch judges its precondition.
    fn before_switch(&self, hook: impl FnOnce() + 'static) {
        *self.before.borrow_mut() = Some(Box::new(hook));
    }

    fn preconditions(&self) -> Vec<Option<SwitchPrecondition>> {
        self.preconditions.borrow().clone()
    }

    fn answer(&self, answer: Result<SwitchReport, SwitchFailure>) {
        self.answers.borrow_mut().push_back(answer);
    }

    fn fail(&self, kind: SwitchErrorKind) {
        self.answer(Err(SwitchFailure { kind, login: None }));
    }

    fn calls(&self) -> Vec<u32> {
        self.calls.borrow().clone()
    }

    /// Queues what the next in-place write answers.
    fn answer_while_on(&self, answer: WhileOn) {
        self.while_on.borrow_mut().push_back(answer);
    }
}

impl SwitchFn for FakeSwitch {
    fn switch(
        &self,
        context: &RefreshContext<'_>,
        number: u32,
        precondition: Option<SwitchPrecondition>,
    ) -> Result<SwitchReport, SwitchFailure> {
        self.calls.borrow_mut().push(number);
        self.preconditions.borrow_mut().push(precondition);
        let hook = self.before.borrow_mut().take();
        if let Some(hook) = hook {
            hook();
        }
        if let Some(precondition) = precondition
            && !precondition_holds(context, number, precondition)
        {
            return Err(SwitchFailure {
                kind: SwitchErrorKind::PreconditionFailed,
                login: None,
            });
        }
        let queued = self.answers.borrow_mut().pop_front();
        if let Some(answer) = queued {
            return answer;
        }
        let from = live_number(context.claude);
        write_live(context.claude, number);
        Ok(SwitchReport {
            switched: true,
            from: from.map(|from| AccountRef::new(Some(from), email(from))),
            to: AccountRef::new(Some(number), email(number)),
            reason: "switched".to_owned(),
            warnings: Vec::new(),
        })
    }

    fn write_while_on(&self, write: &mut dyn FnMut()) -> WhileOn {
        self.while_on_calls.set(self.while_on_calls.get() + 1);
        let answer = self
            .while_on
            .borrow_mut()
            .pop_front()
            .unwrap_or(WhileOn::Ran);
        if answer == WhileOn::Ran {
            write();
        }
        answer
    }
}

/// A token endpoint that answers from a queue, `Connect` once it runs dry, and counts its POSTs.
#[derive(Default)]
struct Endpoint {
    answers: Mutex<VecDeque<Result<RawToken, TransportError>>>,
    posts: AtomicU32,
}

impl Endpoint {
    fn answer(&self, status: u16, body: Value) {
        self.answers
            .lock()
            .expect("no test panics holding the queue")
            .push_back(Ok(RawToken { status, body }));
    }

    fn grant(&self, access: &str, refresh: &str) {
        self.answer(200, granted(access, refresh));
    }

    fn posts(&self) -> u32 {
        self.posts.load(Ordering::SeqCst)
    }
}

fn granted(access: &str, refresh: &str) -> Value {
    json!({"access_token": access, "refresh_token": refresh, "expires_in": 3600, "token_type": "Bearer", "scope": "user:inference"})
}

impl RefreshTransport for Endpoint {
    fn post_token(&self, _request: &RefreshRequest) -> Result<RawToken, TransportError> {
        self.posts.fetch_add(1, Ordering::SeqCst);
        self.answers
            .lock()
            .expect("no test panics holding the queue")
            .pop_front()
            .unwrap_or(Err(TransportError::Connect))
    }
}

struct NoProfile;

impl ProfileTransport for NoProfile {
    fn get_profile(&self, _access: &AccessToken) -> Result<RawProfile, TransportError> {
        Err(TransportError::Connect)
    }
}

#[derive(Default)]
struct Processes(RefCell<Vec<ProcessInfo>>);

impl ProcessTable for Processes {
    fn snapshot(&self) -> Vec<ProcessInfo> {
        self.0.borrow().clone()
    }
}

/// cswap's engine harness: OAuth accounts 1 to N with far-off credentials, account 1 the live login, cswap's default
/// settings, and an engine with the warm-up off. Shared with [`crate::cadence`], which reads its engine, ticks it, and
/// reads the fake switch's calls.
pub(crate) struct Bench {
    scratch: Scratch,
    store: Store,
    claude: ClaudePaths,
    drain: Arc<Drain>,
    verdicts: ProbeVerdicts,
    /// Milliseconds from the tests' fixed time.
    clock: Arc<AtomicI64>,
    endpoint: Endpoint,
    collection: FakeCollection,
    switcher: FakeSwitch,
    processes: Processes,
    claude_config_dir_set: bool,
    securestorage_dir_set: bool,
    /// The auto-switch settings each tick runs on.
    settings: AutoSwitchSettings,
    /// Whether auto-switch is on for each tick.
    auto_switch_enabled: bool,
    pub(crate) engine: Engine,
}

impl Bench {
    pub(crate) fn new(test: &str, accounts: u32) -> Self {
        let scratch = Scratch::new(&format!("tick_{test}"));
        let root = scratch.root();
        let mut records = Map::new();
        for number in 1..=accounts {
            records.insert(
                number.to_string(),
                json!({ "email": email(number), "uuid": uuid(number), "added": "2026-09-01T10:00:00Z" }),
            );
        }
        let order: Vec<u32> = (1..=accounts).collect();
        write(
            &root.accounts_json(),
            &json!({ "active": 1, "order": order, "accounts": records }).to_string(),
        );
        let store = Store::open(root.clone()).expect("the scratch store opens");
        let home = root
            .root()
            .parent()
            .expect("the account store has a parent")
            .to_path_buf();
        let bench = Self {
            scratch,
            store,
            claude: ClaudePaths::new(home.join("claude"), home.join(".claude.json")),
            drain: Arc::new(Drain::new()),
            verdicts: ProbeVerdicts::new(),
            clock: Arc::new(AtomicI64::new(0)),
            endpoint: Endpoint::default(),
            collection: FakeCollection::default(),
            switcher: FakeSwitch::default(),
            processes: Processes::default(),
            claude_config_dir_set: false,
            securestorage_dir_set: false,
            settings: AutoSwitchSettings::default(),
            auto_switch_enabled: true,
            engine: Engine::new("ops-tower", 101, None),
        };
        for number in 1..=accounts {
            bench.put_credential(number, &credential(number, FAR));
            let config = bench.config_backup(number);
            fs::create_dir_all(config.parent().expect("a config backup has a folder"))
                .expect("the config folder can be made");
            write(
                &config,
                &json!({ "oauthAccount": { "emailAddress": email(number), "organizationUuid": "" } }).to_string(),
            );
        }
        write_live(&bench.claude, 1);
        bench
    }

    fn config_backup(&self, number: u32) -> PathBuf {
        self.root().config_path(number)
    }

    /// Reopens the bench's store with a canary that says sealing does not work.
    fn store_without_sealing(&mut self) {
        self.store =
            Store::open_with_canary(self.root(), || false).expect("the scratch store opens");
    }

    /// Makes `autoswitch_state.json` read-only, so every write of it fails while reads go on, or writable again.
    fn state_read_only(&self, read_only: bool) {
        let path = self.root().autoswitch_state();
        if !path.exists() {
            write(&path, "{}");
        }
        let mut permissions = fs::metadata(&path)
            .expect("the state file exists")
            .permissions();
        permissions.set_readonly(read_only);
        fs::set_permissions(&path, permissions).expect("the state file's permissions can be set");
    }

    pub(crate) fn root(&self) -> AccountsRoot {
        self.scratch.root()
    }

    fn put_credential(&self, number: u32, text: &str) {
        let lock = AccountLock::acquire(&self.root()).expect("the account lock is free");
        self.store
            .write_credentials(&lock, number, &CredentialBlob::new(text.to_owned()))
            .expect("the credential can be written");
    }

    fn stored(&self, number: u32) -> String {
        self.store
            .read_credentials(number)
            .expect("the credential can be read")
            .expect("the credential exists")
            .expose()
            .to_owned()
    }

    pub(crate) fn set_account(&self, number: u32, key: &str, value: Value) {
        let path = self.root().accounts_json();
        let mut sequence = read_json(&path);
        sequence["accounts"][number.to_string()][key] = value;
        write(&path, &sequence.to_string());
    }

    pub(crate) fn now(&self) -> Timestamp {
        at(self.clock.load(Ordering::SeqCst))
    }

    /// Turns auto-switch on or off for the next ticks.
    pub(crate) fn set_auto_switch(&mut self, enabled: bool) {
        self.auto_switch_enabled = enabled;
    }

    /// A flag the next freshen sets when it starts, after the ranking.
    fn freshen_seen(&self) -> Arc<AtomicBool> {
        let seen = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&seen);
        self.on_freshen(move || flag.store(true, Ordering::SeqCst));
        seen
    }

    fn ms(&self) -> i64 {
        NOW_S * 1000 + self.clock.load(Ordering::SeqCst)
    }

    pub(crate) fn advance(&self, seconds: i64) {
        self.clock.fetch_add(seconds * 1000, Ordering::SeqCst);
    }

    fn in_secs(&self, seconds: i64) -> Timestamp {
        self.now() + SignedDuration::from_secs(seconds)
    }

    pub(crate) fn reset_in(&self, seconds: i64) -> String {
        self.in_secs(seconds).to_string()
    }

    /// `rows` at the clock now: a reading fetched now, or no reading and a timeout.
    fn rows(&self, rows: &[(u32, Option<Value>)]) -> Collected {
        let now = self.now();
        let mut collected = Collected::default();
        for (number, reading) in rows {
            let mut entry = UsageEntry::default();
            if let Some(reading) = reading {
                entry.last_good = Some(reading.clone());
                entry.fetched_at = Some(now);
            } else {
                entry.last_error = Some("timeout".to_owned());
                entry.consecutive_failures = 1;
            }
            collected.entries.insert(*number, entry);
            collected.results.insert(*number, PollResult::NotDue);
        }
        collected
    }

    /// Sets the collection to `rows` at the clock now: a reading fetched now, or no reading and a timeout.
    pub(crate) fn set_usage(&self, rows: &[(u32, Option<Value>)]) {
        *self.collection.collected.borrow_mut() = self.rows(rows);
    }

    /// Sets what the phase-2 refetch answers to `rows` at the clock now, as [`Bench::set_usage`].
    fn set_refetch(&self, rows: &[(u32, Option<Value>)]) {
        *self.collection.refetched.borrow_mut() = Some(self.rows(rows));
    }

    /// Replaces account `number`'s entry in the phase-2 refetch's answer.
    fn set_refetch_entry(&self, number: u32, entry: UsageEntry) {
        self.collection
            .refetched
            .borrow_mut()
            .as_mut()
            .expect("the refetch's answer was set")
            .entries
            .insert(number, entry);
    }

    /// Makes the phase-2 refetch fail.
    fn fail_refetch(&self) {
        self.collection.refetch_fails.set(true);
    }

    /// The members of each phase-2 refetch, in order.
    fn refetch_calls(&self) -> Vec<Vec<u32>> {
        self.collection.refetch_calls.borrow().clone()
    }

    /// The account of each exit target's re-read, in order.
    fn recheck_calls(&self) -> Vec<u32> {
        self.collection.recheck_calls.borrow().clone()
    }

    pub(crate) fn set_result(&self, number: u32, result: PollResult) {
        self.collection
            .collected
            .borrow_mut()
            .results
            .insert(number, result);
    }

    pub(crate) fn set_entry(&self, number: u32, entry: UsageEntry) {
        self.collection
            .collected
            .borrow_mut()
            .entries
            .insert(number, entry);
    }

    pub(crate) fn tick(&mut self) -> TickReport {
        let clock = Arc::clone(&self.clock);
        let now = move || at(clock.load(Ordering::SeqCst));
        let context = RefreshContext {
            store: &self.store,
            claude: &self.claude,
            transport: &self.endpoint,
            drain: &self.drain,
            securestorage_dir_set: self.securestorage_dir_set,
            clock: &now,
            profile: &NoProfile,
            verdicts: &self.verdicts,
            claude_locks: LockTiming::CLAUDE,
        };
        let inputs = TickInputs {
            context: &context,
            collector: &self.collection,
            processes: &self.processes,
            switcher: &self.switcher,
            claude_config_dir_set: self.claude_config_dir_set,
            settings: &self.settings,
            auto_switch_enabled: self.auto_switch_enabled,
        };
        self.engine.tick(&inputs)
    }

    /// Runs `run` with the bench's store, Claude Code's files, token endpoint, drain, verdicts and clock, and
    /// `profile` as the profile endpoint.
    fn with_context<R>(
        &self,
        profile: &dyn ProfileTransport,
        run: impl FnOnce(&RefreshContext<'_>) -> R,
    ) -> R {
        let clock = Arc::clone(&self.clock);
        let now = move || at(clock.load(Ordering::SeqCst));
        let context = RefreshContext {
            store: &self.store,
            claude: &self.claude,
            transport: &self.endpoint,
            drain: &self.drain,
            securestorage_dir_set: self.securestorage_dir_set,
            clock: &now,
            profile,
            verdicts: &self.verdicts,
            claude_locks: LockTiming::CLAUDE,
        };
        run(&context)
    }

    /// Account `number`'s balance-only read in organization `org_uuid` through the crate's own collection over
    /// `transport`, with the token `fake-access-<number>`.
    pub(crate) fn read_balance(
        &self,
        transport: &dyn UsageTransport,
        number: u32,
        org_uuid: &str,
    ) -> Result<PrepaidEntry, BalanceReadError> {
        let token = AccessToken::new(format!("fake-access-{number}"));
        self.with_context(&NoProfile, |context| {
            TransportCollector(transport).read_balance(context, number, &token, org_uuid)
        })
    }

    /// [`Bench::read_balance`] through a collector that keeps the trait's default.
    pub(crate) fn read_balance_by_default(
        &self,
        number: u32,
        org_uuid: &str,
    ) -> Result<PrepaidEntry, BalanceReadError> {
        let token = AccessToken::new(format!("fake-access-{number}"));
        self.with_context(&NoProfile, |context| {
            DefaultBalance.read_balance(context, number, &token, org_uuid)
        })
    }

    /// Account `number`'s `maySpendCredits` flag in `accounts.json`.
    pub(crate) fn set_may_spend(&self, number: u32, on: bool) {
        self.set_account(number, "maySpendCredits", json!(on));
    }

    /// Account `number`'s reading in the collection with extra usage `on` and every other `extra_usage` flag off.
    pub(crate) fn set_extra_usage(&self, number: u32, on: bool) {
        let mut collected = self.collection.collected.borrow_mut();
        let entry = collected
            .entries
            .get_mut(&number)
            .expect("the account has a usage entry");
        let reading = entry.last_good.as_mut().expect("the account has a reading");
        reading["extra_usage"] = json!({"enabled": on, "user_disabled": false, "spend_limit_reached": false, "disabled_reason_set": false});
    }

    /// Account `number`'s usage entry in the collection with a prepaid balance of `amount_minor` cents in USD, read
    /// `age_s` seconds ago.
    pub(crate) fn set_prepaid(&self, number: u32, amount_minor: i64, age_s: i64) {
        let prepaid = self.balance(amount_minor, age_s);
        let mut collected = self.collection.collected.borrow_mut();
        collected
            .entries
            .get_mut(&number)
            .expect("the account has a usage entry")
            .prepaid = Some(prepaid);
    }

    /// A prepaid balance of `amount_minor` cents in USD, read `age_s` seconds ago.
    pub(crate) fn balance(&self, amount_minor: i64, age_s: i64) -> PrepaidEntry {
        let mut prepaid = PrepaidEntry::default();
        prepaid.amount_minor = Some(amount_minor);
        prepaid.exponent = Some(2);
        prepaid.currency = Some("USD".to_owned());
        prepaid.auto_reload = Some(false);
        prepaid.fetched_at = Some(self.in_secs(-age_s));
        prepaid.attempted_at = prepaid.fetched_at;
        prepaid
    }

    /// Every one of accounts 1 to `accounts` at its 5-hour limit, resetting in 3 hours, with extra usage on.
    pub(crate) fn paid_fleet(&self, accounts: u32) {
        let reset = self.reset_in(3 * H);
        let rows: Vec<(u32, Option<Value>)> = (1..=accounts)
            .map(|number| (number, Some(usage_at(100.0, &reset))))
            .collect();
        self.set_usage(&rows);
        for number in 1..=accounts {
            self.set_extra_usage(number, true);
        }
    }

    /// Opts account `number` in and gives it a balance of `amount_minor` cents read `age_s` seconds ago.
    pub(crate) fn fund(&self, number: u32, amount_minor: i64, age_s: i64) {
        self.set_may_spend(number, true);
        self.set_prepaid(number, amount_minor, age_s);
    }

    /// Queues what the next balance read answers.
    pub(crate) fn answer_balance(&self, answer: Result<PrepaidEntry, BalanceReadError>) {
        self.collection
            .balance_answers
            .borrow_mut()
            .push_back(answer);
    }

    /// Writes account `number`'s far-off credential again, sealed to its record as it is now: a credential sealed before
    /// the record's organization changed no longer opens.
    pub(crate) fn reseal_credential(&self, number: u32) {
        for path in [
            self.root().credential_path(number),
            self.root().credential_previous_path(number),
        ] {
            if path.exists() {
                fs::remove_file(&path).expect("the old sealed credential can be removed");
            }
        }
        self.put_credential(number, &credential(number, FAR));
    }

    /// Makes account `number`, in organization `org_uuid`, Claude Code's live login, its live credential file holding
    /// `credential`.
    pub(crate) fn set_live(&self, number: u32, org_uuid: &str, credential: &str) {
        write(
            self.claude.global_config(),
            &json!({"oauthAccount": {"emailAddress": email(number), "organizationUuid": org_uuid}})
                .to_string(),
        );
        let file = self.claude.credentials_file();
        fs::create_dir_all(file.parent().expect("the credential file has a folder"))
            .expect("Claude Code's folder can be made");
        write(&file, credential);
    }

    /// One poll of every account over `transport` ([`poll_once_with_refresh`]), with `profile` as the profile endpoint,
    /// remembering its verdicts in the bench's memo.
    pub(crate) fn poll_with_profile(
        &self,
        transport: &dyn UsageTransport,
        profile: &dyn ProfileTransport,
    ) -> Vec<PollOutcome> {
        self.with_context(profile, |context| {
            poll_once_with_refresh(transport, &self.settings, context)
        })
        .expect("the poll runs")
    }

    /// How many refresh grants the token endpoint was asked for.
    pub(crate) fn token_posts(&self) -> u32 {
        self.endpoint.posts()
    }

    fn tick_with(&mut self, rows: &[(u32, Option<Value>)]) -> TickReport {
        self.set_usage(rows);
        self.tick()
    }

    fn live(&self) -> Option<u32> {
        live_number(&self.claude)
    }

    /// The accounts the fake switch was called with, in order.
    pub(crate) fn switch_calls(&self) -> Vec<u32> {
        self.switcher.calls()
    }

    /// The number of collections the engine asked for: warm-up, scheduled and refetch alike.
    pub(crate) fn collection_calls(&self) -> usize {
        self.collection.calls()
    }

    /// Makes the warm-up report `settled` of its members, so it stays on for another tick.
    pub(crate) fn set_warm_settled(&self, settled: usize) {
        self.collection.warm_settled.set(Some(settled));
    }

    fn state(&self) -> Value {
        let path = self.root().autoswitch_state();
        if path.exists() {
            read_json(&path)
        } else {
            json!({})
        }
    }

    /// Runs `hook` at the start of the next freshen, after the ranking ([`Store::on_freshen`]).
    fn on_freshen(&self, hook: impl FnOnce() + Send + 'static) {
        self.store.on_freshen(hook);
    }
}

fn bodies(report: &TickReport) -> impl Iterator<Item = &EventBody> {
    report.events.iter().map(|event| &event.body)
}

pub(crate) fn reasons(report: &TickReport) -> Vec<&'static str> {
    bodies(report)
        .filter_map(|body| match body {
            EventBody::NoSwitch { reason, .. } => Some(*reason),
            _ => None,
        })
        .collect()
}

fn details(report: &TickReport) -> Vec<String> {
    bodies(report)
        .filter_map(|body| match body {
            EventBody::NoSwitch { detail, .. } => Some(detail.clone()),
            _ => None,
        })
        .collect()
}

fn switches(report: &TickReport) -> Vec<(Trigger, Option<u32>)> {
    bodies(report)
        .filter_map(|body| match body {
            EventBody::Switch { trigger, to, .. } => Some((*trigger, to.number)),
            _ => None,
        })
        .collect()
}

fn quarantines(report: &TickReport) -> Vec<(u32, String)> {
    bodies(report)
        .filter_map(|body| match body {
            EventBody::Quarantined { number, reason, .. } => Some((*number, reason.clone())),
            _ => None,
        })
        .collect()
}

fn errors(report: &TickReport) -> Vec<(String, bool)> {
    bodies(report)
        .filter_map(|body| match body {
            EventBody::Error { message, transient } => Some((message.clone(), *transient)),
            _ => None,
        })
        .collect()
}

/// Each all-exhausted event's earliest reset.
fn exhausted(report: &TickReport) -> Vec<Option<Timestamp>> {
    bodies(report)
        .filter_map(|body| match body {
            EventBody::AllExhausted { earliest_reset_at } => Some(*earliest_reset_at),
            _ => None,
        })
        .collect()
}

fn poll(report: &TickReport) -> &Event {
    report
        .events
        .iter()
        .find(|event| matches!(event.body, EventBody::Poll { .. }))
        .expect("the tick reports a poll")
}

fn three(test: &str) -> Bench {
    Bench::new(test, 3)
}

fn over_threshold(bench: &mut Bench) -> TickReport {
    bench.tick_with(&[
        (1, Some(usage(95.0))),
        (2, Some(usage(10.0))),
        (3, Some(usage(50.0))),
    ])
}

// Every readable account is at or over the threshold, none at its limit: a proactive tick holds until the active
// account's binding window reaches 100 %.
#[test]
fn all_above_threshold_holds_until_the_active_reaches_its_limit() {
    let mut bench = three("all_above_threshold_holds");
    let (a, b, c) = (
        bench.reset_in(600),
        bench.reset_in(1200),
        bench.reset_in(1800),
    );
    let report = bench.tick_with(&[
        (1, Some(usage_at(95.0, &a))),
        (2, Some(usage_at(96.0, &b))),
        (3, Some(usage_at(97.0, &c))),
    ]);
    assert_eq!(report.outcome, TickOutcome::NoAction);
    assert_eq!(reasons(&report), ["all-above-threshold"]);
    assert_eq!(report.sleep_target, None);
    assert!(!report.blocked_wait_long);
    assert!(
        bench.switcher.calls().is_empty(),
        "a proactive tick with every account at or over the threshold switches nothing"
    );
}

// The hold applies under consume-first too: a fleet whose active and readable peers are all at or over the threshold
// makes no proactive switch on either strategy.
#[test]
fn all_above_threshold_holds_under_consume_first() {
    let mut bench = consume_first("all_above_threshold_consume_first");
    let (a, b, c) = (
        bench.reset_in(600),
        bench.reset_in(1200),
        bench.reset_in(1800),
    );
    let report = bench.tick_with(&[
        (1, Some(usage_at(95.0, &a))),
        (2, Some(usage_at(96.0, &b))),
        (3, Some(usage_at(97.0, &c))),
    ]);
    assert_eq!(report.outcome, TickOutcome::NoAction);
    assert_eq!(reasons(&report), ["all-above-threshold"]);
    assert!(
        bench.switcher.calls().is_empty(),
        "consume-first holds every account at or over the threshold too"
    );
}

// cswap: TestDecisionTable.test_no_active_account
#[test]
fn no_active_account() {
    let mut bench = Bench::new("no_active_account", 2);
    fs::remove_file(bench.claude.global_config()).expect("the live config can be removed");
    let report = bench.tick_with(&[(1, Some(usage(95.0))), (2, Some(usage(10.0)))]);
    assert_eq!(report.outcome, TickOutcome::NoAction);
    assert_eq!(reasons(&report), ["no-active-account"]);
    assert_eq!(bench.switcher.calls().len(), 0);
}

// cswap: TestDecisionTable.test_unmanaged_live_login_is_never_touched
#[test]
fn unmanaged_live_login_is_never_touched() {
    let mut bench = Bench::new("unmanaged_live_login", 2);
    let stranger =
        json!({"oauthAccount": {"emailAddress": "stranger@example.test", "organizationUuid": ""}})
            .to_string();
    write(bench.claude.global_config(), &stranger);
    let report = bench.tick_with(&[(1, Some(usage(95.0))), (2, Some(usage(10.0)))]);
    assert_eq!(report.outcome, TickOutcome::NoAction);
    assert_eq!(reasons(&report), ["unmanaged-active-account"]);
    assert_eq!(bench.switcher.calls().len(), 0);
}

// cswap: TestDecisionTable.test_stale_beyond_trust_blocks_all_exhausted
#[test]
fn stale_beyond_trust_blocks_all_exhausted() {
    let mut bench = three("stale_beyond_trust");
    let reset = bench.reset_in(48 * 3600);
    bench.set_usage(&[(1, Some(usage(95.0)))]);
    let mut stale_exhausted = UsageEntry::default();
    stale_exhausted.last_good = Some(usage_at(100.0, &reset));
    stale_exhausted.fetched_at = Some(bench.in_secs(-400));
    stale_exhausted.consecutive_failures = 1;
    bench.set_entry(2, stale_exhausted);
    let mut overdue = UsageEntry::default();
    overdue.last_good = Some(usage(10.0));
    overdue.fetched_at = Some(bench.in_secs(-400));
    bench.set_entry(3, overdue);
    let report = bench.tick();
    assert_eq!(report.outcome, TickOutcome::NoAction);
    assert_eq!(exhausted(&report).len(), 0);
    assert_eq!(reasons(&report), ["all-above-threshold"]);
}

// cswap: TestDecisionTable.test_trusted_stale_exhausted_set_still_fires_all_exhausted
#[test]
fn trusted_stale_exhausted_set_still_fires_all_exhausted() {
    let mut bench = three("trusted_stale_exhausted");
    let reset = bench.reset_in(48 * 3600);
    bench.set_usage(&[(1, Some(usage(95.0)))]);
    let mut stale_exhausted = UsageEntry::default();
    stale_exhausted.last_good = Some(usage_at(100.0, &reset));
    stale_exhausted.fetched_at = Some(bench.in_secs(-400));
    stale_exhausted.consecutive_failures = 1;
    bench.set_entry(2, stale_exhausted.clone());
    bench.set_entry(3, stale_exhausted);
    let report = bench.tick();
    assert_eq!(report.outcome, TickOutcome::Blocked);
    assert_eq!(
        exhausted(&report),
        [Some(reset.parse::<Timestamp>().expect("the reset parses"))]
    );
}

// cswap: TestDecisionTable.test_all_exhausted_carries_earliest_reset
#[test]
fn all_exhausted_carries_earliest_reset() {
    let mut bench = three("all_exhausted_earliest");
    let (later, earliest, middle) = (
        bench.reset_in(3 * 3600),
        bench.reset_in(5400),
        bench.reset_in(2 * 3600),
    );
    let report = bench.tick_with(&[
        (1, Some(usage_at(100.0, &later))),
        (2, Some(usage_at(100.0, &earliest))),
        (3, Some(usage_at(100.0, &middle))),
    ]);
    assert_eq!(report.outcome, TickOutcome::Blocked);
    assert_eq!(exhausted(&report), [Some(bench.in_secs(5400))]);
    assert_eq!(
        report.sleep_target,
        Some(bench.in_secs(5400 + 60)),
        "the sleep target is the reset plus cswap's slack"
    );
    assert!(report.blocked_wait_long);
}

// cswap: TestDecisionTable.test_all_exhausted_ignores_non_future_reset
#[test]
fn all_exhausted_ignores_non_future_reset() {
    for offset in [-60_i64, 0] {
        let mut bench = three(&format!("all_exhausted_past_reset_{}", offset.abs()));
        let reset = bench.reset_in(offset);
        let report = bench.tick_with(&[
            (1, Some(usage_at(100.0, &reset))),
            (2, Some(usage_at(100.0, &reset))),
            (3, Some(usage_at(100.0, &reset))),
        ]);
        assert_eq!(report.outcome, TickOutcome::Blocked);
        assert_eq!(exhausted(&report), [None::<Timestamp>], "offset {offset}");
        assert_eq!(report.sleep_target, None);
        assert!(
            report.blocked_wait_long,
            "the long blocked cadence stands in for the unknown reset"
        );
    }
}

/// cswap's `TestIdleHold._HELD`: the active token expired while Claude Code is idle; both peers readable.
fn held(bench: &mut Bench) -> TickReport {
    bench.set_usage(&[(1, None), (2, Some(usage(10.0))), (3, Some(usage(20.0)))]);
    bench.set_result(1, PollResult::TokenExpired);
    bench.tick()
}

// cswap: TestIdleHold.test_token_expired_holds_instead_of_failover
#[test]
fn token_expired_holds_instead_of_failover() {
    let mut bench = three("token_expired_holds");
    let mut seen = BTreeSet::new();
    for _ in 0..6 {
        let report = held(&mut bench);
        assert_eq!(report.outcome, TickOutcome::NoAction);
        seen.extend(reasons(&report));
        bench.advance(60);
    }
    assert_eq!(bench.live(), Some(1));
    assert_eq!(bench.switcher.calls().len(), 0);
    assert_eq!(seen, BTreeSet::from(["active-idle"]));
    assert_eq!(bench.engine.health().unhealthy_ticks, 0);
}

// cswap: TestIdleHold.test_idle_hold_slows_cadence
#[test]
fn idle_hold_slows_cadence() {
    let mut bench = three("idle_hold_slows_cadence");
    let report = held(&mut bench);
    assert_eq!(report.outcome, TickOutcome::NoAction);
    assert!(
        report.idle_hold_slow,
        "the idle hold asks for the slow cadence"
    );
    let healthy = bench.tick_with(&[
        (1, Some(usage(50.0))),
        (2, Some(usage(10.0))),
        (3, Some(usage(20.0))),
    ]);
    assert!(
        !healthy.idle_hold_slow,
        "a healthy tick keeps the normal cadence"
    );
}

// cswap: TestIdleHold.test_idle_hold_cap_escalates_to_failover
#[test]
fn idle_hold_cap_escalates_to_failover() {
    let mut bench = three("idle_hold_cap");
    assert_eq!(held(&mut bench).outcome, TickOutcome::NoAction);
    bench.advance(1801);
    assert_eq!(held(&mut bench).outcome, TickOutcome::NoAction);
    assert_eq!(held(&mut bench).outcome, TickOutcome::NoAction);
    let report = held(&mut bench);
    assert_eq!(report.outcome, TickOutcome::Switched);
    assert_eq!(switches(&report), [(Trigger::Failover, Some(2))]);
}

// The idle-hold lapse warning cswap logs (autoswitch.py:1103) is an event here.
#[test]
fn an_idle_hold_past_its_cap_warns() {
    let mut bench = three("idle_hold_lapse_warning");
    let warnings = |report: &TickReport| {
        bodies(report)
            .filter(|body| matches!(body, EventBody::Warning { .. }))
            .count()
    };
    assert_eq!(
        warnings(&held(&mut bench)),
        0,
        "a hold inside its cap does not warn"
    );
    bench.advance(30 * 60 + 1);
    let report = held(&mut bench);
    assert_eq!(warnings(&report), 1, "the lapsed hold warns once per tick");
    assert_eq!(reasons(&report), ["active-usage-unknown"]);
}

// cswap: TestIdleHold.test_recovery_resets_the_hold_clock
#[test]
fn recovery_resets_the_hold_clock() {
    let mut bench = three("recovery_resets_hold_clock");
    held(&mut bench);
    bench.advance(30 * 60 - 60);
    bench.tick_with(&[
        (1, Some(usage(50.0))),
        (2, Some(usage(10.0))),
        (3, Some(usage(20.0))),
    ]);
    bench.advance(120);
    let report = held(&mut bench);
    assert_eq!(report.outcome, TickOutcome::NoAction);
    assert_eq!(reasons(&report), ["active-idle"]);
    assert_eq!(bench.engine.health().unhealthy_ticks, 0);
    assert_eq!(bench.live(), Some(1));
}

// cswap: TestIdleHold.test_plain_fetch_failure_still_counts_unhealthy
#[test]
fn plain_fetch_failure_still_counts_unhealthy() {
    let mut bench = three("plain_fetch_failure");
    held(&mut bench);
    let report = bench.tick_with(&[(1, None), (2, Some(usage(10.0))), (3, Some(usage(20.0)))]);
    assert_eq!(report.outcome, TickOutcome::NoAction);
    assert_eq!(
        bench.engine.health(),
        ActiveHealth {
            unhealthy_ticks: 1,
            idle_hold_since: None
        }
    );
}

// cswap: TestIdleHold.test_foreign_credential_sentinel_fails_over_instead_of_holding
#[test]
fn foreign_credential_sentinel_fails_over_instead_of_holding() {
    let mut bench = three("foreign_credential_fails_over");
    let mut outcomes = Vec::new();
    let mut last = None;
    for _ in 0..3 {
        bench.set_usage(&[(1, None), (2, Some(usage(10.0))), (3, Some(usage(20.0)))]);
        bench.set_result(1, PollResult::ForeignCredential);
        let report = bench.tick();
        outcomes.push(report.outcome.clone());
        last = Some(report);
    }
    assert_eq!(
        outcomes,
        [
            TickOutcome::NoAction,
            TickOutcome::NoAction,
            TickOutcome::Switched
        ]
    );
    assert_eq!(
        switches(&last.expect("three ticks ran")),
        [(Trigger::Failover, Some(2))]
    );
    assert_eq!(bench.engine.health().idle_hold_since, None);
}

fn mark_api_key(bench: &Bench, number: u32) {
    bench.set_account(number, "kind", json!("api_key"));
}

// cswap: TestApiKeyAccounts.test_active_api_key_idles_engine
#[test]
fn active_api_key_idles_engine() {
    let mut bench = Bench::new("active_api_key_idles", 2);
    mark_api_key(&bench, 1);
    bench.set_usage(&[(1, None), (2, Some(usage(10.0)))]);
    let report = bench.tick();
    assert_eq!(report.outcome, TickOutcome::NoAction);
    assert_eq!(reasons(&report), ["active-api-key"]);
    assert_eq!(bench.switcher.calls().len(), 0);
}

// cswap: TestFreshening.test_near_expiry_target_is_refreshed_and_persisted
#[test]
fn near_expiry_target_is_refreshed_and_persisted() {
    let mut bench = Bench::new("near_expiry_refreshed", 2);
    bench.put_credential(2, &credential(2, bench.ms() + 60_000));
    bench.endpoint.grant("fresh-access-2", "fresh-refresh-2");
    let report = bench.tick_with(&[(1, Some(usage(95.0))), (2, Some(usage(10.0)))]);
    assert_eq!(report.outcome, TickOutcome::Switched);
    assert_eq!(bench.endpoint.posts(), 1);
    assert!(
        bench.stored(2).contains("fresh-access-2"),
        "the freshened credential is the target's backup"
    );
    assert_eq!(bench.switcher.calls(), [2]);
}

// cswap: TestFreshening.test_fresh_target_is_not_refreshed
#[test]
fn fresh_target_is_not_refreshed() {
    let mut bench = Bench::new("fresh_target_not_refreshed", 2);
    bench.put_credential(2, &credential(2, bench.ms() + 3_600_000));
    let report = bench.tick_with(&[(1, Some(usage(95.0))), (2, Some(usage(10.0)))]);
    assert_eq!(report.outcome, TickOutcome::Switched);
    assert_eq!(bench.endpoint.posts(), 0);
}

// cswap: TestFreshening.test_invalid_grant_quarantines_and_tries_next
#[test]
fn invalid_grant_quarantines_and_tries_next() {
    let mut bench = three("invalid_grant_quarantines");
    bench.put_credential(2, &credential(2, 1));
    bench
        .endpoint
        .answer(400, json!({"error": "invalid_grant"}));
    let report = bench.tick_with(&[
        (1, Some(usage(95.0))),
        (2, Some(usage(10.0))),
        (3, Some(usage(20.0))),
    ]);
    assert_eq!(report.outcome, TickOutcome::Switched);
    assert_eq!(bench.live(), Some(3));
    assert_eq!(quarantines(&report), [(2, "invalid_grant".to_owned())]);
    assert!(bench.state()["quarantine"].get("2").is_some());
}

// cswap: TestFreshening.test_transient_failure_skips_without_quarantine
#[test]
fn transient_failure_skips_without_quarantine() {
    let mut bench = Bench::new("transient_freshen", 2);
    bench.put_credential(2, &credential(2, 1));
    let report = bench.tick_with(&[(1, Some(usage(95.0))), (2, Some(usage(10.0)))]);
    assert_eq!(report.outcome, TickOutcome::Error);
    assert_eq!(bench.live(), Some(1));
    assert!(bench.state().get("quarantine").is_none());
    assert_eq!(
        errors(&report),
        [(
            "could not freshen any candidate (network?)".to_owned(),
            true
        )]
    );
}

/// Whether sealing works for [`a_store_refusing_writes_is_reported_as_the_freshen_error`]'s store; each test runs in
/// its own process, so no other test sees it.
static SEALING: AtomicBool = AtomicBool::new(true);

fn sealing() -> bool {
    SEALING.load(Ordering::SeqCst)
}

/// A store that starts refusing writes after the tick's opening check (sealing stops working once the freshen begins)
/// refuses every candidate's freshen alike, before any POST: the tick names the store, not the network.
#[test]
fn a_store_refusing_writes_is_reported_as_the_freshen_error() {
    let mut bench = Bench::new("store_unwritable_freshen", 2);
    bench.put_credential(2, &credential(2, 1));
    bench.store = Store::open_with_canary(bench.root(), sealing).expect("the scratch store opens");
    bench.on_freshen(|| SEALING.store(false, Ordering::SeqCst));
    let report = bench.tick_with(&[(1, Some(usage(95.0))), (2, Some(usage(10.0)))]);
    assert_eq!(report.outcome, TickOutcome::Error);
    assert_eq!(bench.endpoint.posts(), 0, "nothing was spent");
    assert_eq!(bench.live(), Some(1));
    assert!(bench.state().get("quarantine").is_none());
    assert_eq!(
        errors(&report),
        [(
            concat!(
                "could not freshen: the account store refuses writes ",
                "(its DPAPI check failed or accounts.json is a newer version); nothing is spent until it is writable"
            )
            .to_owned(),
            true
        )]
    );
}

// cswap: TestQuarantineLifecycle.test_quarantine_persists_across_engine_instances
#[test]
fn quarantine_persists_across_engine_instances() {
    let mut bench = three("quarantine_persists");
    quarantine(&bench.store, 2, &address(2), "invalid_grant", bench.now())
        .expect("the quarantine is written");
    bench.engine = Engine::new("ops-tower", 101, None);
    let report = bench.tick_with(&[
        (1, Some(usage(95.0))),
        (2, Some(usage(0.0))),
        (3, Some(usage(50.0))),
    ]);
    assert_eq!(report.outcome, TickOutcome::Switched);
    assert_eq!(bench.live(), Some(3));
}

// cswap: TestQuarantineLifecycle.test_replaced_credentials_lift_quarantine
#[test]
fn replaced_credentials_lift_quarantine() {
    let mut bench = three("replaced_credentials_lift");
    quarantine(&bench.store, 2, &address(2), "invalid_grant", bench.now())
        .expect("the quarantine is written");
    let replaced = json!({"claudeAiOauth": {"accessToken": "fake-access-2b", "refreshToken": "fake-refresh-2b", "expiresAt": FAR}});
    bench.put_credential(2, &replaced.to_string());
    let report = bench.tick_with(&[
        (1, Some(usage(95.0))),
        (2, Some(usage(0.0))),
        (3, Some(usage(50.0))),
    ]);
    assert!(bodies(&report).any(|body| matches!(body, EventBody::Unquarantined { .. })));
    assert_eq!(report.outcome, TickOutcome::Switched);
    assert_eq!(bench.live(), Some(2));
    assert!(bench.state()["quarantine"].get("2").is_none());
}

// cswap: TestEventsShape.test_every_event_has_envelope
#[test]
fn every_event_has_envelope() {
    let mut bench = three("every_event_has_envelope");
    let report = over_threshold(&mut bench);
    assert_ne!(report.events.len(), 0);
    for event in &report.events {
        let payload = event.to_json();
        assert_eq!(payload["schemaVersion"], 1);
        assert_eq!(payload["event"], event.kind());
        assert!(payload["ts"].as_str().expect("ts is text").ends_with('Z'));
    }
}

// cswap: TestEventsShape.test_switch_event_refs_match_account_ref_shape
#[test]
fn switch_event_refs_match_account_ref_shape() {
    let mut bench = three("switch_event_refs");
    let report = over_threshold(&mut bench);
    let switch = report
        .events
        .iter()
        .find(|event| matches!(event.body, EventBody::Switch { .. }))
        .expect("the tick switched");
    let payload = switch.to_json();
    assert_eq!(payload["from"], json!({"number": 1, "email": email(1)}));
    assert_eq!(payload["to"], json!({"number": 2, "email": email(2)}));
    assert_eq!(payload["trigger"], "proactive");
}

// cswap: TestEventsShape.test_poll_event_human_line, as the poll event's fields: cswap's line is drawn from them.
#[test]
fn poll_event_carries_each_accounts_usage() {
    let mut bench = three("poll_event_fields");
    let report = bench.tick_with(&[(1, Some(usage(42.0))), (2, Some(usage(10.0))), (3, None)]);
    let EventBody::Poll {
        active,
        headroom,
        fetch_errors,
        windows,
        ..
    } = &poll(&report).body
    else {
        unreachable!("poll() finds a poll event");
    };
    assert_eq!(active, &Some(AccountRef::new(Some(1), email(1))));
    assert_eq!(
        headroom,
        &BTreeMap::from([(1, Some(58.0)), (2, Some(90.0)), (3, None)])
    );
    assert_eq!(fetch_errors, &BTreeMap::from([(3, "timeout".to_owned())]));
    assert_eq!(
        windows.get(&2),
        Some(&vec![("5h".to_owned(), 10.0), ("7d".to_owned(), 0.0)])
    );
    assert_eq!(windows.get(&3), None);
}

// cswap: TestEventsShape.test_poll_event_windows_match_the_decision_set; the poll event shows the windows the decision
// reads: the account-wide windows, then each configured model's scoped window under its own name.
#[test]
fn poll_event_windows_match_the_decision_set() {
    let mut bench = Bench::new("poll_event_windows", 2);
    bench.settings.models = vec!["Fable".to_owned()];
    let scoped = json!({"five_hour": {"pct": 3.0}, "seven_day": {"pct": 89.0}, "scoped": [{"name": "Fable", "pct": 21.0}]});
    let report = bench.tick_with(&[(1, Some(usage(42.0))), (2, Some(scoped))]);
    assert_eq!(
        poll(&report).to_json()["windowsPct"]["2"],
        json!({"5h": 3.0, "7d": 89.0, "Fable": 21.0})
    );
}

// cswap: TestTokenIdentity.test_identity_conflict_quarantines_instead_of_activating
#[test]
fn identity_conflict_quarantines_instead_of_activating() {
    let mut bench = three("identity_conflict_quarantines");
    bench.put_credential(2, &credential(2, 0));
    let mut body = granted("fresh-access-2", "fresh-refresh-2");
    body["account"] = json!({"uuid": "uuid-somebody-else", "email_address": "z@example.test"});
    body["organization"] = json!({"uuid": ""});
    bench.endpoint.answer(200, body);
    let report = bench.tick_with(&[
        (1, Some(usage(95.0))),
        (2, Some(usage(10.0))),
        (3, Some(usage(80.0))),
    ]);
    assert_eq!(quarantines(&report), [(2, "identity-conflict".to_owned())]);
    assert_eq!(
        bench.state()["quarantine"]["2"]["reason"],
        "identity-conflict"
    );
    assert_eq!(report.outcome, TickOutcome::Switched);
    assert_eq!(bench.live(), Some(3));
}

// cswap: TestTokenIdentity.test_dead_slot_quarantined_even_with_safety_copy_present, without the safety copy: the
// stash is the refresh tests' concern.
#[test]
fn dead_slot_quarantined() {
    let mut bench = three("dead_slot_quarantined");
    bench.put_credential(2, &credential(2, 0));
    bench
        .endpoint
        .answer(400, json!({"error": "invalid_grant"}));
    let report = bench.tick_with(&[
        (1, Some(usage(95.0))),
        (2, Some(usage(10.0))),
        (3, Some(usage(80.0))),
    ]);
    assert_eq!(bench.state()["quarantine"]["2"]["reason"], "invalid_grant");
    assert_eq!(report.outcome, TickOutcome::Switched);
    assert_eq!(bench.live(), Some(3));
}

// cswap: TestConsumeFirst.test_left_snapshot_uses_the_ranking_now_not_a_fresh_clock_read (test_autoswitch.py:5904)
#[test]
fn left_snapshot_uses_the_ranking_now_not_a_fresh_clock_read() {
    let mut bench = Bench::new("left_snapshot_ranking_now", 2);
    let reset = bench.reset_in(100);
    let jump = Arc::clone(&bench.clock);
    bench.on_freshen(move || {
        jump.fetch_add(200_000, Ordering::SeqCst);
    });
    let report = bench.tick_with(&[(1, Some(usage_at(95.0, &reset))), (2, Some(usage(10.0)))]);
    assert_eq!(report.outcome, TickOutcome::Switched);
    assert_eq!(
        bench.state()["leftRecoveryAt"],
        json!(1_790_000_100.0),
        "the departure snapshot is taken at the ranking's time, before the clock moved past the reset"
    );
    assert_eq!(bench.state()["leftHeadroom"], json!(5.0));
}

// cswap: TestConsumeFirst.test_locked_recheck_stops_concurrent_engine (test_autoswitch.py:2887): another engine
// switches after this tick's own cooldown check; the re-check under the state lock backs off.
#[test]
fn locked_recheck_stops_concurrent_engine() {
    let mut bench = three("locked_recheck");
    let path = bench.root().autoswitch_state();
    let other = r#"{"lastSwitchAt":1790000000.0,"lastSwitchTo":"3"}"#;
    let written = path.clone();
    bench.on_freshen(move || write(&written, other));
    let report = over_threshold(&mut bench);
    assert_eq!(report.outcome, TickOutcome::NoAction);
    assert!(reasons(&report).contains(&"cooldown"));
    assert!(bench.switcher.calls().is_empty(), "no double switch");
    assert_eq!(bench.live(), Some(1));
    assert_eq!(
        fs::read_to_string(&path).expect("the state file is there"),
        other,
        "a cooldown leaves the file's bytes"
    );
}

// cswap: TestConsumeFirst.test_single_account_below_threshold_is_no_action (test_autoswitch.py:2978)
#[test]
fn single_account_below_threshold_is_no_action() {
    let mut bench = Bench::new("single_account_consume_first", 1);
    bench.settings.strategy = Strategy::ConsumeFirst;
    let soon = bench.reset_in(24 * 3600);
    let report = bench.tick_with(&[(1, Some(usage7(20.0, 20.0, &soon)))]);
    assert_eq!(report.outcome, TickOutcome::NoAction);
    assert_eq!(reasons(&report), ["below-threshold"]);
    assert!(bench.refetch_calls().is_empty(), "a hold never refetches");
}

/// A three-account bench under the consume-first strategy.
fn consume_first(test: &str) -> Bench {
    let mut bench = three(test);
    bench.settings.strategy = Strategy::ConsumeFirst;
    bench
}

/// cswap's `_R_SOON`, `_R_LATER` and `_R_LATEST`: three 7-day resets, soonest first, fixed when called.
fn weekly_resets(bench: &Bench) -> (String, String, String) {
    (
        bench.reset_in(24 * 3600),
        bench.reset_in(4 * 24 * 3600),
        bench.reset_in(6 * 24 * 3600),
    )
}

/// cswap's `_two_phase_tick`: the scheduled collection serves `stored`, the phase-2 refetch `fresh`.
fn two_phase_tick(
    bench: &mut Bench,
    stored: &[(u32, Option<Value>)],
    fresh: &[(u32, Option<Value>)],
) -> TickReport {
    bench.set_usage(stored);
    bench.set_refetch(fresh);
    bench.tick()
}

fn polls(report: &TickReport) -> usize {
    bodies(report)
        .filter(|body| matches!(body, EventBody::Poll { .. }))
        .count()
}

// cswap: TestAdaptiveScheduler.test_consume_first_hold_never_escalates_below_threshold (test_autoswitch.py:1620)
#[test]
fn consume_first_hold_never_refetches() {
    let mut bench = consume_first("cf_hold_never_refetches");
    let (soon, later, latest) = weekly_resets(&bench);
    for _ in 0..4 {
        let report = bench.tick_with(&[
            (1, Some(usage7(50.0, 20.0, &soon))),
            (2, Some(usage7(10.0, 10.0, &later))),
            (3, Some(usage7(10.0, 10.0, &latest))),
        ]);
        assert_eq!(report.outcome, TickOutcome::NoAction);
        assert_eq!(reasons(&report), ["already-consuming-soonest"]);
        bench.advance(60);
    }
    assert!(
        bench.refetch_calls().is_empty(),
        "a hold never asks for the phase-2 refetch"
    );
    assert!(bench.switcher.calls().is_empty(), "a hold never switches");
}

// cswap: TestAdaptiveScheduler.test_consume_first_stale_target_holds_then_switches (test_autoswitch.py:1656)
#[test]
fn consume_first_stale_target_holds_then_switches() {
    let mut bench = consume_first("cf_stale_target");
    let (soon, later, latest) = weekly_resets(&bench);
    let view_a = [
        (1, Some(usage7(50.0, 20.0, &soon))),
        (2, Some(usage7(10.0, 10.0, &later))),
        (3, Some(usage7(10.0, 10.0, &latest))),
    ];
    assert_eq!(bench.tick_with(&view_a).outcome, TickOutcome::NoAction);
    bench.advance(60);
    assert_eq!(bench.tick_with(&view_a).outcome, TickOutcome::NoAction);
    bench.advance(181);
    let view_b = [
        (1, Some(usage7(50.0, 20.0, &latest))),
        (2, Some(usage7(10.0, 10.0, &later))),
        (3, Some(usage7(10.0, 10.0, &latest))),
    ];
    bench.set_usage(&view_b);
    bench.set_refetch(&view_b);
    let mut backed_off = UsageEntry::default();
    backed_off.last_good = Some(usage7(10.0, 10.0, &later));
    backed_off.fetched_at = Some(bench.in_secs(-241));
    backed_off.backoff_until = Some(bench.in_secs(600));
    bench.set_refetch_entry(2, backed_off);
    let before = bench.state();
    let report = bench.tick();
    assert_eq!(report.outcome, TickOutcome::NoAction);
    assert!(
        reasons(&report).contains(&"stale-usage"),
        "{:?}",
        reasons(&report)
    );
    assert!(details(&report).contains(&"account 2 usage could not be refreshed this tick (backoff or a concurrent poller); retrying".to_owned()));
    assert_eq!(bench.live(), Some(1));
    assert!(
        bench.switcher.calls().is_empty(),
        "a stale target is never switched to"
    );
    assert_eq!(bench.state(), before, "the hold writes nothing");
    assert!(
        !report.blocked_wait_long,
        "a stale-usage hold retries at the normal cadence"
    );
    assert_eq!(bench.refetch_calls().len(), 1);
    bench.advance(700);
    let report = two_phase_tick(&mut bench, &view_b, &view_b);
    assert_eq!(report.outcome, TickOutcome::Switched);
    assert_eq!(bench.live(), Some(2));
    assert_eq!(switches(&report), [(Trigger::ConsumeFirst, Some(2))]);
}

// cswap's freshness gate (autoswitch.py:1395-1396): a stale consume-first pick holds the tick and never slides to the
// next-ranked target, even one the refetch left fresh.
#[test]
fn consume_first_stale_target_never_slides_to_a_worse_target() {
    let mut bench = consume_first("cf_stale_never_slides");
    let (soon, later, latest) = weekly_resets(&bench);
    let view = [
        (1, Some(usage7(50.0, 20.0, &latest))),
        (2, Some(usage7(10.0, 10.0, &soon))),
        (3, Some(usage7(10.0, 10.0, &later))),
    ];
    bench.set_usage(&view);
    bench.set_refetch(&view);
    let mut backed_off = UsageEntry::default();
    backed_off.last_good = Some(usage7(10.0, 10.0, &soon));
    backed_off.fetched_at = Some(bench.in_secs(-241));
    backed_off.backoff_until = Some(bench.in_secs(600));
    bench.set_refetch_entry(2, backed_off);
    let report = bench.tick();
    assert_eq!(report.outcome, TickOutcome::NoAction);
    assert_eq!(reasons(&report), ["stale-usage"]);
    assert!(
        bench.switcher.calls().is_empty(),
        "the fresh, worse-ranked account 3 is never tried"
    );
    assert_eq!(bench.live(), Some(1));
}

// cswap: TestConsumeFirstStrategy.test_below_threshold_switches_to_soonest_weekly_reset (test_autoswitch.py:2796)
#[test]
fn consume_first_below_threshold_switches_to_soonest_reset() {
    let mut bench = consume_first("cf_switches_to_soonest");
    let (soon, later, latest) = weekly_resets(&bench);
    let report = bench.tick_with(&[
        (1, Some(usage7(20.0, 20.0, &later))),
        (2, Some(usage7(10.0, 10.0, &soon))),
        (3, Some(usage7(10.0, 10.0, &latest))),
    ]);
    assert_eq!(report.outcome, TickOutcome::Switched);
    assert_eq!(bench.live(), Some(2));
    assert_eq!(switches(&report), [(Trigger::ConsumeFirst, Some(2))]);
    assert_eq!(
        bench.refetch_calls(),
        [vec![1, 2, 3]],
        "phase 2 refetches the active account and the candidates once"
    );
    assert_eq!(polls(&report), 1, "phase 2 emits no second poll event");
}

// cswap: TestConsumeFirstStrategy.test_two_phase_refetch_disqualifies_stale_pick (test_autoswitch.py:3101)
#[test]
fn two_phase_refetch_disqualifies_stale_pick() {
    let mut bench = consume_first("two_phase_disqualifies");
    let (soon, later, latest) = weekly_resets(&bench);
    let stored = [
        (1, Some(usage7(20.0, 20.0, &later))),
        (2, Some(usage7(10.0, 10.0, &soon))),
        (3, Some(usage7(10.0, 10.0, &latest))),
    ];
    let fresh = [
        (1, Some(usage7(20.0, 20.0, &later))),
        (2, Some(usage7(100.0, 100.0, &soon))),
        (3, Some(usage7(10.0, 10.0, &latest))),
    ];
    let report = two_phase_tick(&mut bench, &stored, &fresh);
    assert_eq!(report.outcome, TickOutcome::NoAction);
    assert_eq!(bench.live(), Some(1));
    assert_eq!(reasons(&report), ["already-consuming-soonest"]);
    assert_eq!(bench.refetch_calls().len(), 1, "phase 2 fired once");
}

// cswap: TestConsumeFirstStrategy.test_two_phase_refetch_confirms_switch (test_autoswitch.py:3122)
#[test]
fn two_phase_refetch_confirms_switch() {
    let mut bench = consume_first("two_phase_confirms");
    let (soon, later, latest) = weekly_resets(&bench);
    let view = [
        (1, Some(usage7(20.0, 20.0, &later))),
        (2, Some(usage7(10.0, 10.0, &soon))),
        (3, Some(usage7(10.0, 10.0, &latest))),
    ];
    let report = two_phase_tick(&mut bench, &view, &view);
    assert_eq!(report.outcome, TickOutcome::Switched);
    assert_eq!(bench.live(), Some(2));
    assert_eq!(bench.refetch_calls(), [vec![1, 2, 3]]);
}

// cswap: TestConsumeFirstStrategy.test_two_phase_refetch_reranks_to_fresh_best (test_autoswitch.py:3136)
#[test]
fn two_phase_refetch_reranks_to_fresh_best() {
    let mut bench = consume_first("two_phase_reranks");
    let (soon, later, latest) = weekly_resets(&bench);
    let stored = [
        (1, Some(usage7(20.0, 20.0, &latest))),
        (2, Some(usage7(10.0, 10.0, &soon))),
        (3, Some(usage7(10.0, 10.0, &later))),
    ];
    let fresh = [
        (1, Some(usage7(20.0, 20.0, &latest))),
        (2, Some(usage7(10.0, 10.0, &later))),
        (3, Some(usage7(10.0, 10.0, &soon))),
    ];
    let report = two_phase_tick(&mut bench, &stored, &fresh);
    assert_eq!(report.outcome, TickOutcome::Switched);
    assert_eq!(
        bench.switcher.calls(),
        [3],
        "phase 2 is a full re-rank, not a check of the first pick"
    );
    assert_eq!(bench.live(), Some(3));
}

// cswap: TestConsumeFirstStrategy.test_threshold_crossed_in_phase_two_holds_then_escapes_next_tick (test_autoswitch.py:3155)
#[test]
fn threshold_crossed_in_phase_two_holds_then_escapes_next_tick() {
    let mut bench = consume_first("two_phase_threshold_crossed");
    let (soon, later, latest) = weekly_resets(&bench);
    let stored = [
        (1, Some(usage7(20.0, 20.0, &later))),
        (2, Some(usage7(10.0, 10.0, &soon))),
        (3, Some(usage7(10.0, 10.0, &latest))),
    ];
    let fresh = [
        (1, Some(usage7(100.0, 20.0, &later))),
        (2, Some(usage7(10.0, 10.0, &latest))),
        (3, Some(usage7(10.0, 10.0, &latest))),
    ];
    let report = two_phase_tick(&mut bench, &stored, &fresh);
    assert_eq!(report.outcome, TickOutcome::NoAction);
    assert_eq!(bench.live(), Some(1));
    assert!(
        switches(&report).is_empty(),
        "the phase-2 hold switches nothing"
    );
    assert_eq!(bench.refetch_calls().len(), 1);
    assert_eq!(
        reasons(&report),
        ["already-consuming-soonest"],
        "phase 2 never re-classifies the trigger"
    );
    let report = bench.tick_with(&fresh);
    assert_eq!(report.outcome, TickOutcome::Switched);
    assert_eq!(bench.live(), Some(2));
    assert_eq!(switches(&report), [(Trigger::AtLimit, Some(2))]);
    assert_eq!(
        bench.refetch_calls().len(),
        1,
        "an escape is never refetched"
    );
}

// cswap: TestHorizonAxisDoesNotFlap.test_the_bar_is_recomputed_on_the_phase_two_snapshot (test_autoswitch.py:6140)
#[test]
fn the_bar_is_recomputed_on_the_phase_two_snapshot() {
    let mut bench = consume_first("bar_phase_two");
    let (out10, out400, out500) = (
        bench.reset_in(10 * 3600),
        bench.reset_in(400 * 3600),
        bench.reset_in(500 * 3600),
    );
    let report = bench.tick_with(&[
        (1, Some(usage7(20.0, 20.0, &out500))),
        (2, Some(usage7(5.0, 5.0, &out10))),
        (3, Some(usage7(5.0, 5.0, &out400))),
    ]);
    assert_eq!(report.outcome, TickOutcome::Switched);
    assert_eq!(bench.live(), Some(2));
    bench.advance(301);
    let stale = [
        (1, Some(usage7(80.0, 80.0, &out10))),
        (2, Some(usage7(70.0, 70.0, &out500))),
        (3, Some(usage7(60.0, 60.0, &out400))),
    ];
    let fresh = [
        (1, Some(usage7(10.0, 10.0, &out10))),
        (2, Some(usage7(85.0, 85.0, &out500))),
        (3, Some(usage7(60.0, 60.0, &out400))),
    ];
    let report = two_phase_tick(&mut bench, &stale, &fresh);
    assert_eq!(report.outcome, TickOutcome::Switched);
    assert_eq!(
        bench.live(),
        Some(1),
        "on the fresh snapshot the account left holds 6x the active's headroom and resets soonest, so the bar releases it"
    );
}

#[test]
fn phase_two_unreadable_active_still_ranks() {
    let mut bench = consume_first("phase_two_unreadable_active");
    let (soon, later, latest) = weekly_resets(&bench);
    let stored = [
        (1, Some(usage7(20.0, 20.0, &later))),
        (2, Some(usage7(10.0, 10.0, &soon))),
        (3, Some(usage7(10.0, 10.0, &latest))),
    ];
    let fresh = [
        (1, None),
        (2, Some(usage7(10.0, 10.0, &soon))),
        (3, Some(usage7(10.0, 10.0, &latest))),
    ];
    let report = two_phase_tick(&mut bench, &stored, &fresh);
    assert_eq!(report.outcome, TickOutcome::NoAction);
    assert_eq!(
        reasons(&report),
        ["reset-unknown"],
        "the re-rank runs on the unreadable active reading and finds its reset unknown"
    );
    assert_eq!(bench.refetch_calls().len(), 1);
    assert!(
        bench.switcher.calls().is_empty(),
        "an unknown reset never switches"
    );
}

#[test]
fn consume_first_departure_snapshot_is_the_phase_two_reading() {
    let mut bench = consume_first("cf_departure_snapshot");
    let (soon, later, latest) = weekly_resets(&bench);
    let stored = [
        (1, Some(usage7(20.0, 20.0, &later))),
        (2, Some(usage7(10.0, 10.0, &soon))),
        (3, Some(usage7(10.0, 10.0, &latest))),
    ];
    let fresh = [
        (1, Some(usage7(30.0, 30.0, &later))),
        (2, Some(usage7(10.0, 10.0, &soon))),
        (3, Some(usage7(10.0, 10.0, &latest))),
    ];
    let report = two_phase_tick(&mut bench, &stored, &fresh);
    assert_eq!(report.outcome, TickOutcome::Switched);
    assert_eq!(
        bench.state()["leftHeadroom"],
        json!(70.0),
        "the snapshot is the refetched reading"
    );
    assert_eq!(bench.state()["leftTrigger"], json!("consume-first"));
}

#[test]
fn best_strategy_never_refetches() {
    let mut bench = three("best_never_refetches");
    let report = over_threshold(&mut bench);
    assert_eq!(report.outcome, TickOutcome::Switched);
    assert!(
        bench.refetch_calls().is_empty(),
        "the best strategy never refetches"
    );
}

#[test]
fn a_failed_refetch_is_a_transient_error() {
    let mut bench = consume_first("failed_refetch");
    let (soon, later, latest) = weekly_resets(&bench);
    bench.set_usage(&[
        (1, Some(usage7(20.0, 20.0, &later))),
        (2, Some(usage7(10.0, 10.0, &soon))),
        (3, Some(usage7(10.0, 10.0, &latest))),
    ]);
    bench.fail_refetch();
    let report = bench.tick();
    assert_eq!(report.outcome, TickOutcome::Error);
    assert_eq!(
        errors(&report),
        [(
            format!(
                "usage could not be collected: {}",
                PollError::UnsupportedCache
            ),
            true
        )]
    );
    assert!(
        bench.switcher.calls().is_empty(),
        "a failed refetch never switches"
    );
    assert_eq!(bench.live(), Some(1));
    assert_eq!(bench.state(), json!({}), "no state write");
    assert_eq!(polls(&report), 1);
}

// cswap: TestWarmUp.test_warmup_none_keeps_existing_behaviour
#[test]
fn warmup_none_keeps_existing_behaviour() {
    let mut bench = three("warmup_none");
    let report = bench.tick_with(&[
        (1, Some(usage(95.0))),
        (2, Some(usage(10.0))),
        (3, Some(usage(20.0))),
    ]);
    assert_eq!(report.outcome, TickOutcome::Switched);
    assert!(bench.collection.warm_calls.borrow().is_empty());
    assert_eq!(bench.collection.scheduled_calls.get(), 1);
    assert!(!reasons(&report).contains(&"warming-up"));
    assert_eq!(bench.live(), Some(2));
}

// cswap: TestWarmUp.test_warmup_blocks_switch_until_all_polled and test_warmup_detail_reports_progress
#[test]
fn warmup_blocks_switch_until_all_polled() {
    let mut bench = three("warmup_blocks");
    bench.engine = Engine::new("ops-tower", 101, Some(bench.now()));
    bench.collection.warm_settled.set(Some(2));
    let rows = [
        (1, Some(usage(95.0))),
        (2, Some(usage(10.0))),
        (3, Some(usage(20.0))),
    ];
    let report = bench.tick_with(&rows);
    assert_eq!(report.outcome, TickOutcome::NoAction);
    assert_eq!(reasons(&report), ["warming-up"]);
    assert_eq!(details(&report), ["2/3 accounts polled since start"]);
    assert!(
        report
            .events
            .iter()
            .any(|event| matches!(event.body, EventBody::Poll { .. }))
    );
    assert_eq!(bench.switcher.calls().len(), 0);
    bench.collection.warm_settled.set(None);
    bench.advance(61);
    let report = bench.tick_with(&rows);
    assert_eq!(report.outcome, TickOutcome::Switched);
    assert_eq!(bench.live(), Some(2));
    bench.tick_with(&rows);
    assert_eq!(
        bench.collection.warm_calls.borrow().len(),
        2,
        "the gate never re-arms"
    );
    assert_eq!(bench.collection.scheduled_calls.get(), 1);
}

// A member whose warm-up poll came back a sentinel is not forced again on the next warm-up tick, and still counts.
#[test]
fn a_sentinel_member_is_not_forced_again() {
    let mut bench = three("warmup_drops_sentinels");
    bench.engine = Engine::new("ops-tower", 101, Some(bench.now()));
    bench.collection.warm_settled.set(Some(2));
    bench.set_usage(&[(1, Some(usage(95.0))), (2, Some(usage(10.0))), (3, None)]);
    bench.set_result(3, PollResult::AuthDead);
    bench.tick();
    bench.collection.warm_settled.set(Some(1));
    let report = bench.tick();
    assert_eq!(*bench.collection.warm_calls.borrow(), [vec![2, 3], vec![2]]);
    assert_eq!(details(&report), ["2/3 accounts polled since start"]);
}

fn lease_owner(bench: &Bench) -> Option<Value> {
    let path = bench.root().engine_lease();
    path.exists().then(|| read_json(&path)["owner"].clone())
}

// A live engine claims the lease and heartbeats it.
#[test]
fn a_live_engine_holds_the_lease() {
    let mut bench = three("live_engine_holds_lease");
    bench.tick_with(&[
        (1, Some(usage(50.0))),
        (2, Some(usage(10.0))),
        (3, Some(usage(20.0))),
    ]);
    let lease = read_json(&bench.root().engine_lease());
    assert_eq!(lease["owner"], "ops-tower");
    assert_eq!(lease["pid"], 101);
}

// Another engine's fresh lease pauses this one, which leaves the lease alone and decides nothing.
#[test]
fn another_fresh_lease_pauses_the_engine() {
    let mut bench = three("another_lease_pauses");
    let now = bench.now().as_second();
    let lease = json!({"owner": "rustling-tulip", "pid": 77, "startedAt": now, "heartbeatAt": now, "intervalSeconds": 60.0});
    write(&bench.root().engine_lease(), &lease.to_string());
    let report = over_threshold(&mut bench);
    let TickOutcome::Paused { reason } = &report.outcome else {
        unreachable!("expected a pause, got {:?}", report.outcome);
    };
    assert!(
        reason.contains("rustling-tulip") && reason.contains("77"),
        "{reason}"
    );
    assert_eq!(bench.collection.calls(), 0);
    assert_eq!(bench.switcher.calls().len(), 0);
    assert_eq!(lease_owner(&bench), Some(json!("rustling-tulip")));
}

// The route refusal: `CLAUDE_CONFIG_DIR` set means Claude Code reads another profile's login.
#[test]
fn claude_config_dir_pauses_the_engine() {
    let mut bench = three("claude_config_dir_pauses");
    bench.claude_config_dir_set = true;
    let report = over_threshold(&mut bench);
    let TickOutcome::Paused { reason } = &report.outcome else {
        unreachable!("expected a pause, got {:?}", report.outcome);
    };
    assert!(reason.contains("CLAUDE_CONFIG_DIR"), "{reason}");
    assert_eq!(bench.collection.calls(), 0);
    assert_eq!(lease_owner(&bench), None);
}

// A switch that finds sealing broken pauses the engine, and records no switch.
#[test]
fn a_store_read_only_switch_pauses_the_engine() {
    let mut bench = three("store_read_only_pauses");
    bench.switcher.fail(SwitchErrorKind::StoreReadOnly);
    let report = over_threshold(&mut bench);
    assert_eq!(
        report.outcome,
        TickOutcome::Paused {
            reason: SwitchErrorKind::StoreReadOnly.to_string()
        }
    );
    assert!(bench.state().get("lastSwitchAt").is_none());
}

// A switch whose precondition no longer holds under its locks skips its target, as a refused target is skipped: the
// next candidate is switched to, with no error, no quarantine and no halt.
#[test]
fn perform_maps_the_precondition_refusal_to_a_skip() {
    let mut bench = three("precondition_refusal_skips");
    bench.switcher.fail(SwitchErrorKind::PreconditionFailed);

    let report = over_threshold(&mut bench);

    assert_eq!(report.outcome, TickOutcome::Switched);
    assert_eq!(
        bench.switcher.calls(),
        [2, 3],
        "the refused target is skipped for the next"
    );
    assert_eq!(bench.live(), Some(3));
    assert!(
        errors(&report).is_empty(),
        "a refused precondition is no error: {:?}",
        errors(&report)
    );
    assert!(!bench.engine.halted());
    assert!(
        bench.state().get("quarantine").is_none(),
        "nothing is quarantined"
    );
    assert_eq!(bench.state()["lastSwitchTo"], json!("3"));
}

#[test]
fn rollback_failed_halts_until_restart() {
    let mut bench = three("rollback_failed_halts");
    bench.switcher.answer(Err(SwitchFailure {
        kind: SwitchErrorKind::RollbackFailed,
        login: Some(LoginState::Mixed),
    }));
    let report = over_threshold(&mut bench);
    assert_eq!(report.outcome, TickOutcome::Halted);
    let errors = errors(&report);
    assert_eq!(errors.len(), 1);
    assert!(!errors[0].1, "a failed rollback is not transient");
    assert!(bench.engine.halted());
    let report = over_threshold(&mut bench);
    assert_eq!(report.outcome, TickOutcome::Halted);
    assert_eq!(
        bench.collection.calls(),
        1,
        "a halted engine collects nothing"
    );
    assert_eq!(bench.switcher.calls(), [2]);
    bench.engine = Engine::new("ops-tower", 101, None);
    bench.advance(301);
    assert_eq!(
        over_threshold(&mut bench).outcome,
        TickOutcome::Switched,
        "a restart clears the halt"
    );
}

// A target the switch cannot use is skipped for the next candidate.
#[test]
fn a_target_the_switch_refuses_is_skipped() {
    for kind in [
        SwitchErrorKind::TargetBackupMissing,
        SwitchErrorKind::TargetBackupUnreadable,
        SwitchErrorKind::ApiKeyAccount,
        SwitchErrorKind::UnknownAccount,
    ] {
        let mut bench = three(&format!("switch_skips_{}", kind.as_str()));
        bench.switcher.fail(kind);
        let report = over_threshold(&mut bench);
        assert_eq!(report.outcome, TickOutcome::Switched, "{kind:?}");
        assert_eq!(bench.switcher.calls(), [2, 3], "{kind:?}");
        assert_eq!(bench.live(), Some(3), "{kind:?}");
    }
}

/// Rewrites account `number`'s config backup to name another login than its record.
fn name_another_login(bench: &Bench, number: u32) {
    write(
        &bench.config_backup(number),
        &json!({ "oauthAccount": { "emailAddress": "other@example.test", "organizationUuid": "" } }).to_string(),
    );
}

/// Puts the live login back on account 1 and moves the clock past the cooldown, for a second tick.
fn back_on_one(bench: &Bench) {
    write_live(&bench.claude, 1);
    bench.advance(301);
}

// A target the switch finds backed by another login's config is quarantined with an event, and not picked again.
#[test]
fn a_target_whose_backup_names_another_login_is_quarantined_with_an_event() {
    let mut bench = three("switch_identity_mismatch_quarantines");
    name_another_login(&bench, 2);
    bench.switcher.fail(SwitchErrorKind::TargetIdentityMismatch);

    let report = over_threshold(&mut bench);

    assert_eq!(report.outcome, TickOutcome::Switched);
    assert_eq!(bench.switcher.calls(), [2, 3]);
    assert_eq!(quarantines(&report), [(2, "identity-mismatch".to_owned())]);
    assert!(bench.state()["quarantine"].get("2").is_some());

    back_on_one(&bench);
    let next = over_threshold(&mut bench);

    assert_eq!(
        bench.switcher.calls(),
        [2, 3, 3],
        "the quarantined account is not picked again"
    );
    assert!(
        quarantines(&next).is_empty(),
        "the quarantine event is not repeated"
    );
}

// A near-expiry target whose config backup names another login is quarantined before its grant is spent.
#[test]
fn freshen_identity_mismatch_quarantines_without_spending() {
    let mut bench = three("freshen_identity_mismatch");
    bench.put_credential(2, &credential(2, bench.ms() + 60_000));
    bench.endpoint.grant("fresh-access-2", "fresh-refresh-2");
    name_another_login(&bench, 2);

    let report = over_threshold(&mut bench);

    assert_eq!(bench.endpoint.posts(), 0, "nothing was spent");
    assert_eq!(quarantines(&report), [(2, "identity-mismatch".to_owned())]);
    assert_eq!(bench.switcher.calls(), [3]);
    assert_eq!(bench.live(), Some(3));
}

// An identity-mismatch quarantine outlives a replaced credential, and lifts once the config backup names the
// record's login again.
#[test]
fn an_identity_mismatch_quarantine_lifts_when_the_backup_names_the_login_again() {
    let mut bench = three("identity_mismatch_lifts");
    name_another_login(&bench, 2);
    quarantine(
        &bench.store,
        2,
        &address(2),
        "identity-mismatch",
        bench.now(),
    )
    .expect("the quarantine is written");
    let replaced = json!({"claudeAiOauth": {"accessToken": "fake-access-2b", "refreshToken": "fake-refresh-2b", "expiresAt": FAR}});
    bench.put_credential(2, &replaced.to_string());

    let held = over_threshold(&mut bench);

    assert!(
        !bodies(&held).any(|body| matches!(body, EventBody::Unquarantined { .. })),
        "a new credential does not lift it"
    );
    assert_eq!(bench.live(), Some(3));

    back_on_one(&bench);
    write(
        &bench.config_backup(2),
        &json!({ "oauthAccount": { "emailAddress": email(2), "organizationUuid": "" } })
            .to_string(),
    );
    let lifted = over_threshold(&mut bench);

    assert!(bodies(&lifted).any(|body| matches!(
        body,
        EventBody::Unquarantined {
            reason: ReleaseReason::IdentityRestored,
            ..
        }
    )));
    assert!(bench.state()["quarantine"].get("2").is_none());
    assert_eq!(bench.live(), Some(2));
}

// A state file a newer version wrote, found under the state lock, pauses the engine before the switch acts: the
// switch is never called and the file keeps its bytes.
#[test]
fn a_state_file_of_an_unknown_version_stops_the_switch_before_it_acts() {
    let mut bench = three("state_unknown_version");
    let path = bench.root().autoswitch_state();
    let newer = r#"{"schemaVersion":2,"paidFallback":{"number":3}}"#;
    let written = path.clone();
    bench.on_freshen(move || write(&written, newer));

    let report = over_threshold(&mut bench);

    assert_eq!(
        report.outcome,
        TickOutcome::Paused {
            reason: EngineError::UnknownVersion.to_string()
        }
    );
    assert!(bench.switcher.calls().is_empty(), "the switch never ran");
    assert_eq!(bench.live(), Some(1));
    assert_eq!(
        lease_owner(&bench),
        None,
        "the lease is released, so a newer engine can take it at once"
    );
    assert_eq!(
        fs::read_to_string(&path).expect("the state file is there"),
        newer,
        "the file keeps its bytes"
    );
}

// A switch that failed for a passing reason is a transient error: nothing recorded, no other candidate tried.
#[test]
fn a_transient_switch_failure_is_an_error_event() {
    for kind in [
        SwitchErrorKind::LocksBusy,
        SwitchErrorKind::LockFailed,
        SwitchErrorKind::LiveUnreadable,
        SwitchErrorKind::ConfigUnreadable,
        SwitchErrorKind::OutgoingBackupFailed,
        SwitchErrorKind::WriteFailed,
    ] {
        let mut bench = three(&format!("switch_transient_{}", kind.as_str()));
        bench.switcher.fail(kind);
        let report = over_threshold(&mut bench);
        assert_eq!(report.outcome, TickOutcome::Error, "{kind:?}");
        assert_eq!(errors(&report), [(kind.to_string(), true)], "{kind:?}");
        assert_eq!(bench.switcher.calls(), [2], "{kind:?}");
        assert!(
            !bench.root().autoswitch_state().exists(),
            "a failed switch writes no state: {kind:?}"
        );
        assert_eq!(bench.engine.health(), ActiveHealth::default(), "{kind:?}");
    }
}

// A switch refused for a store-wide reason pauses the engine.
#[test]
fn a_store_wide_switch_refusal_pauses_the_engine() {
    for kind in [
        SwitchErrorKind::StoreUnreadable,
        SwitchErrorKind::StoreReadOnly,
        SwitchErrorKind::UnknownSchema,
        SwitchErrorKind::StoreUnmirrored,
    ] {
        let mut bench = three(&format!("switch_pauses_{}", kind.as_str()));
        bench.switcher.fail(kind);
        let report = over_threshold(&mut bench);
        assert_eq!(
            report.outcome,
            TickOutcome::Paused {
                reason: kind.to_string()
            },
            "{kind:?}"
        );
        assert_eq!(bench.switcher.calls(), [2], "{kind:?}");
    }
}

// A switch refused because the drain began stops the tick without a word.
#[test]
fn a_draining_switch_stops_silently() {
    let mut bench = three("switch_draining");
    bench.switcher.fail(SwitchErrorKind::Draining);
    let report = over_threshold(&mut bench);
    assert_eq!(report.outcome, TickOutcome::NoAction);
    assert_eq!(errors(&report).len(), 0);
    assert_eq!(reasons(&report).len(), 0);
    assert_eq!(switches(&report).len(), 0);
    assert_eq!(bench.switcher.calls(), [2]);
}

// cswap: TestDryRunAndNoOp.test_already_active_result_is_noop
#[test]
fn already_active_result_is_noop() {
    let mut bench = three("already_active_noop");
    bench.switcher.answer(Ok(SwitchReport {
        switched: false,
        from: Some(AccountRef::new(Some(2), email(2))),
        to: AccountRef::new(Some(2), email(2)),
        reason: "already-active".to_owned(),
        warnings: Vec::new(),
    }));
    let report = over_threshold(&mut bench);
    assert_eq!(report.outcome, TickOutcome::NoAction);
    assert_eq!(reasons(&report), ["already-active"]);
    assert!(
        !bench.root().autoswitch_state().exists(),
        "an already-active target writes no state"
    );
}

/// How a case makes the store refuse.
type Refusal = fn(&mut Bench);

// A store that refuses writes or cannot be read pauses the engine before it claims the lease.
#[test]
fn a_store_refusal_pauses_before_the_lease() {
    let cases: [(SwitchErrorKind, Refusal); 4] = [
        (SwitchErrorKind::StoreReadOnly, |bench| {
            bench.store_without_sealing();
        }),
        (SwitchErrorKind::UnknownSchema, |bench| {
            let path = bench.root().accounts_json();
            let mut accounts: serde_json::Value =
                serde_json::from_str(&fs::read_to_string(&path).expect("accounts.json reads"))
                    .expect("it is JSON");
            accounts["schemaVersion"] = json!(2);
            write(&path, &accounts.to_string());
        }),
        (SwitchErrorKind::StoreUnmirrored, |bench| {
            bench.securestorage_dir_set = true;
        }),
        (SwitchErrorKind::StoreUnreadable, |bench| {
            write(&bench.root().accounts_json(), "{not json");
        }),
    ];
    for (kind, refuse) in cases {
        let mut bench = three(&format!("store_refusal_{}", kind.as_str()));
        refuse(&mut bench);
        let report = over_threshold(&mut bench);
        assert_eq!(
            report.outcome,
            TickOutcome::Paused {
                reason: kind.to_string()
            },
            "{kind:?}"
        );
        assert_eq!(
            lease_owner(&bench),
            None,
            "no lease was claimed; a refusal other than a newer store leaves the lease alone, a newer store releases it: {kind:?}"
        );
        assert_eq!(bench.collection.calls(), 0, "{kind:?}");
        assert!(bench.switcher.calls().is_empty(), "{kind:?}");
    }
}

// A halted engine releases its lease on every tick, so another engine can take over at once, and decides nothing else.
#[test]
fn a_halted_engine_releases_its_lease() {
    let mut bench = three("halted_releases_lease");
    bench.switcher.answer(Err(SwitchFailure {
        kind: SwitchErrorKind::RollbackFailed,
        login: Some(LoginState::Mixed),
    }));
    assert_eq!(over_threshold(&mut bench).outcome, TickOutcome::Halted);
    bench.advance(60);
    assert_eq!(over_threshold(&mut bench).outcome, TickOutcome::Halted);
    assert_eq!(
        lease_owner(&bench),
        None,
        "the first halted tick, 60 s in, leaves no lease, long before it could go stale"
    );
    for _ in 0..4 {
        bench.advance(60);
        assert_eq!(over_threshold(&mut bench).outcome, TickOutcome::Halted);
    }
    let other = LeaseHolder::new("rustling-tulip", 77, 60.0);
    let seen = Lease::new(&bench.root())
        .observe(&other, bench.now(), &bench.processes)
        .expect("the lease can be read");
    assert_eq!(
        seen,
        LeaseState::Free,
        "the halted engine released its lease"
    );
    assert_eq!(
        bench.collection.calls(),
        1,
        "a halted engine collects nothing"
    );
    assert_eq!(bench.switcher.calls(), [2]);
}

// The tick whose switch fails its rollback halts the engine and releases the lease in that same tick.
#[test]
fn the_tick_that_halts_releases_its_lease() {
    let mut bench = three("halting_tick_releases_lease");
    bench.switcher.answer(Err(SwitchFailure {
        kind: SwitchErrorKind::RollbackFailed,
        login: Some(LoginState::Mixed),
    }));
    assert_eq!(over_threshold(&mut bench).outcome, TickOutcome::Halted);
    assert!(bench.engine.halted());
    assert!(
        !bench.root().engine_lease().exists(),
        "the tick that halts leaves no lease file"
    );
}

// A newer accounts.json releases the lease even when another refusal, here a sealing canary that fails, also holds.
#[test]
fn a_newer_store_releases_the_lease_even_when_sealing_fails() {
    let mut bench = three("newer_store_canary_fails");
    bench.tick_with(&[
        (1, Some(usage(50.0))),
        (2, Some(usage(10.0))),
        (3, Some(usage(20.0))),
    ]);
    assert_eq!(
        lease_owner(&bench),
        Some(json!("ops-tower")),
        "the first tick claims the lease"
    );
    let path = bench.root().accounts_json();
    let mut accounts: Value =
        serde_json::from_str(&fs::read_to_string(&path).expect("accounts.json reads"))
            .expect("it is JSON");
    accounts["schemaVersion"] = json!(2);
    write(&path, &accounts.to_string());
    bench.store_without_sealing();
    bench.advance(60);
    let report = over_threshold(&mut bench);
    assert_eq!(
        report.outcome,
        TickOutcome::Paused {
            reason: SwitchErrorKind::UnknownSchema.to_string()
        }
    );
    assert!(
        !bench.root().engine_lease().exists(),
        "a newer store releases the lease whatever else refuses"
    );
}

// An engine that finds accounts.json at a newer version releases its lease and pauses, on that tick and every later
// one, collecting and switching nothing, so a newer engine can take over at once.
#[test]
fn an_engine_that_finds_a_newer_store_releases_its_lease() {
    let mut bench = three("newer_store_releases_lease");
    bench.tick_with(&[
        (1, Some(usage(50.0))),
        (2, Some(usage(10.0))),
        (3, Some(usage(20.0))),
    ]);
    assert_eq!(
        lease_owner(&bench),
        Some(json!("ops-tower")),
        "the first tick claims the lease"
    );
    let path = bench.root().accounts_json();
    let mut accounts: Value =
        serde_json::from_str(&fs::read_to_string(&path).expect("accounts.json reads"))
            .expect("it is JSON");
    accounts["schemaVersion"] = json!(2);
    write(&path, &accounts.to_string());
    let collected = bench.collection.calls();
    let switched = bench.switcher.calls();

    for tick in ["the first tick", "a later tick"] {
        bench.advance(60);
        let report = over_threshold(&mut bench);
        assert_eq!(
            report.outcome,
            TickOutcome::Paused {
                reason: SwitchErrorKind::UnknownSchema.to_string()
            },
            "{tick} in a newer store pauses"
        );
        assert_eq!(
            lease_owner(&bench),
            None,
            "{tick} in a newer store leaves no lease"
        );
        assert_eq!(
            bench.collection.calls(),
            collected,
            "{tick} in a newer store collects nothing"
        );
        assert_eq!(
            bench.switcher.calls(),
            switched,
            "{tick} in a newer store switches nothing"
        );
    }
}

// Another engine takes the lease after this tick claimed it: the check under the state lock stops the switch.
#[test]
fn a_lease_taken_before_the_switch_stops_it() {
    let mut bench = three("lease_taken_before_switch");
    let path = bench.root().engine_lease();
    let now = bench.now().as_second();
    bench.on_freshen(move || {
        let lease = json!({"owner": "rustling-tulip", "pid": 77, "startedAt": now, "heartbeatAt": now, "intervalSeconds": 60.0});
        write(&path, &lease.to_string());
    });
    let report = over_threshold(&mut bench);
    let TickOutcome::Paused { reason } = &report.outcome else {
        unreachable!("expected a pause, got {:?}", report.outcome);
    };
    assert!(
        reason.contains("rustling-tulip") && reason.contains("77"),
        "{reason}"
    );
    assert!(
        bench.switcher.calls().is_empty(),
        "no switch without the lease"
    );
    assert_eq!(bench.live(), Some(1));
    assert!(!bench.root().autoswitch_state().exists());
}

/// A process that is not the lease's other engine (pid 77), so the bench's table is not empty.
fn bystander() -> ProcessInfo {
    ProcessInfo {
        pid: 5,
        parent_pid: 1,
        name: "explorer.exe".to_owned(),
        command_line: Some("explorer.exe".to_owned()),
        created: None,
    }
}

// Another engine's fresh lease whose process is not running is taken over at the tick's lease check, so the tick
// goes on.
#[test]
fn a_fresh_lease_whose_holder_is_not_running_is_taken_over() {
    let mut bench = three("dead_holder_taken_over");
    bench.processes.0.borrow_mut().push(bystander());
    let now = bench.now().as_second();
    let lease = json!({"owner": "rustling-tulip", "pid": 77, "startedAt": now, "heartbeatAt": now, "intervalSeconds": 60.0});
    write(&bench.root().engine_lease(), &lease.to_string());
    let report = over_threshold(&mut bench);
    assert!(
        !matches!(report.outcome, TickOutcome::Paused { .. }),
        "a lease whose holder is not running pauses nothing: {:?}",
        report.outcome
    );
    assert_eq!(lease_owner(&bench), Some(json!("ops-tower")));
}

// A lease written mid-tick by an engine that is not running is not this engine's either: the check under the state
// lock judges its holder against the tick's process table, finds the lease free rather than held, and still stops the
// switch, as a lease that is no longer this engine's.
#[test]
fn a_lease_whose_holder_is_not_running_taken_before_the_switch_stops_it() {
    let mut bench = three("dead_holder_lease_taken_before_switch");
    bench.processes.0.borrow_mut().push(bystander());
    let path = bench.root().engine_lease();
    let now = bench.now().as_second();
    bench.on_freshen(move || {
        let lease = json!({"owner": "rustling-tulip", "pid": 77, "startedAt": now, "heartbeatAt": now, "intervalSeconds": 60.0});
        write(&path, &lease.to_string());
    });
    let report = over_threshold(&mut bench);
    assert_eq!(
        report.outcome,
        TickOutcome::Paused {
            reason: "the engine lease is no longer this engine's".to_owned()
        }
    );
    assert!(
        bench.switcher.calls().is_empty(),
        "no switch without the lease"
    );
    assert_eq!(bench.live(), Some(1));
    assert!(!bench.root().autoswitch_state().exists());
}

// An identity conflict whose quarantine write failed keeps the account out of rotation until a later tick writes it.
#[test]
fn a_failed_conflict_quarantine_still_blocks_the_target() {
    let mut bench = Bench::new("conflict_quarantine_unwritten", 2);
    bench.put_credential(2, &credential(2, 0));
    let mut body = granted("fresh-access-2", "fresh-refresh-2");
    body["account"] = json!({"uuid": "uuid-somebody-else", "email_address": "z@example.test"});
    body["organization"] = json!({"uuid": ""});
    bench.endpoint.answer(200, body);
    bench.state_read_only(true);
    let rows = [(1, Some(usage(95.0))), (2, Some(usage(10.0)))];
    let first = bench.tick_with(&rows);
    assert_eq!(first.outcome, TickOutcome::Error);
    assert!(
        quarantines(&first).is_empty(),
        "the quarantine was not written"
    );
    bench.advance(60);
    let second = bench.tick_with(&rows);
    bench.state_read_only(false);
    assert!(
        bench.switcher.calls().is_empty(),
        "the conflicted account is not switched to"
    );
    assert_eq!(second.outcome, TickOutcome::Blocked);
    bench.advance(60);
    let third = bench.tick_with(&rows);
    assert_eq!(quarantines(&third), [(2, "identity-conflict".to_owned())]);
    assert_eq!(
        bench.state()["quarantine"]["2"]["reason"],
        "identity-conflict"
    );
    assert!(
        bench.switcher.calls().is_empty(),
        "nothing is switched to once the quarantine is written"
    );
}

// cswap's `_account_is_switchable`: an account with a credential but no config backup is neither ranked nor collected.
#[test]
fn an_account_without_a_config_backup_is_not_switchable() {
    let mut bench = three("no_config_backup");
    fs::remove_file(bench.config_backup(2)).expect("the config backup can be removed");
    assert_eq!(over_threshold(&mut bench).outcome, TickOutcome::Switched);
    assert_eq!(bench.switcher.calls(), [3], "account 2 is not ranked");
    let mut warming = three("no_config_backup_collected");
    fs::remove_file(warming.config_backup(2)).expect("the config backup can be removed");
    warming.engine = Engine::new("ops-tower", 101, Some(warming.now()));
    warming.collection.warm_settled.set(Some(1));
    over_threshold(&mut warming);
    assert_eq!(
        *warming.collection.warm_calls.borrow(),
        [vec![3]],
        "account 2 is not collected"
    );
}

fn config_warnings(report: &TickReport) -> Vec<String> {
    bodies(report)
        .filter_map(|body| match body {
            EventBody::ConfigWarning { message } => Some(message.clone()),
            _ => None,
        })
        .collect()
}

// cswap: TestModelFilter's typo guard (autoswitch.py:2296): a model name no usage window carries warns once.
#[test]
fn a_typoed_model_name_warns_once() {
    let mut bench = three("model_typo_warns_once");
    bench.settings.models = vec!["Fabel".to_owned()];
    let scoped = |pct: f64| json!({"five_hour": {"pct": pct}, "seven_day": {"pct": 0.0}, "scoped": [{"name": "Fable", "pct": 21.0}]});
    let rows = [
        (1, Some(scoped(50.0))),
        (2, Some(scoped(10.0))),
        (3, Some(scoped(20.0))),
    ];
    let first = bench.tick_with(&rows);
    bench.advance(60);
    let second = bench.tick_with(&rows);
    assert_eq!(
        config_warnings(&first),
        [
            "autoswitch.models: Fabel matches no account's usage windows — only the 5h/7d limits are being watched for it (typo?)"
        ]
    );
    assert!(
        config_warnings(&second).is_empty(),
        "the check runs to its end once"
    );
    bench.settings.models = vec!["Fabell".to_owned()];
    bench.advance(60);
    let changed = bench.tick_with(&rows);
    assert_eq!(
        config_warnings(&changed),
        [
            "autoswitch.models: Fabell matches no account's usage windows — only the 5h/7d limits are being watched for it (typo?)"
        ],
        "a changed list is checked again"
    );
    bench.advance(60);
    assert!(
        config_warnings(&bench.tick_with(&rows)).is_empty(),
        "the changed list too is checked once"
    );
    let mut spelled = three("model_spelled_right");
    spelled.settings.models = vec!["fable".to_owned()];
    assert!(
        config_warnings(&spelled.tick_with(&rows)).is_empty(),
        "names match case-insensitively"
    );
}

/// cswap's `_model_usage`: the 5-hour window at `five_hour`, an empty 7-day window and a `Fable` window at `fable`.
fn model_usage(five_hour: f64, fable: f64) -> Value {
    json!({"five_hour": {"pct": five_hour}, "seven_day": {"pct": 0.0}, "scoped": [{"name": "Fable", "pct": fable}]})
}

/// cswap's `TestModelAwareSwitch._seed`: three accounts, account 1 live, with the settings' `models` set to `models`.
fn modelled(test: &str, models: &[&str]) -> Bench {
    let mut bench = three(test);
    bench.settings.models = models.iter().map(|&model| model.to_owned()).collect();
    bench
}

/// The account each switch event went to.
fn switch_targets(report: &TickReport) -> Vec<&AccountRef> {
    bodies(report)
        .filter_map(|body| match body {
            EventBody::Switch { to, .. } => Some(to),
            _ => None,
        })
        .collect()
}

// cswap: TestModelAwareSwitch.test_model_maxed_switches_despite_session_headroom
#[test]
fn model_maxed_switches_despite_session_headroom() {
    let mut bench = modelled("model_maxed_switches_despite_session_headroom", &["Fable"]);
    let report = bench.tick_with(&[
        (1, Some(model_usage(5.0, 100.0))),
        (2, Some(model_usage(5.0, 30.0))),
        (3, Some(model_usage(5.0, 60.0))),
    ]);
    assert_eq!(report.outcome, TickOutcome::Switched);
    assert_eq!(
        bench.live(),
        Some(2),
        "account 2 has the most Fable headroom"
    );
    assert_eq!(
        switch_targets(&report),
        [&AccountRef::new(Some(2), email(2))]
    );
}

// cswap: TestModelAwareSwitch.test_without_model_setting_the_same_usage_holds
#[test]
fn without_model_setting_the_same_usage_holds() {
    let mut bench = three("without_model_setting_the_same_usage_holds");
    let report = bench.tick_with(&[
        (1, Some(model_usage(5.0, 100.0))),
        (2, Some(model_usage(5.0, 30.0))),
        (3, Some(model_usage(5.0, 60.0))),
    ]);
    assert_eq!(report.outcome, TickOutcome::NoAction);
    assert_eq!(bench.live(), Some(1));
    assert_eq!(reasons(&report), ["below-threshold"]);
}

// cswap: TestModelAwareSwitch.test_model_headroom_still_gated_by_session_window
#[test]
fn model_headroom_still_gated_by_session_window() {
    let mut bench = modelled("model_headroom_still_gated_by_session_window", &["Fable"]);
    let report = bench.tick_with(&[
        (1, Some(model_usage(100.0, 40.0))),
        (2, Some(model_usage(10.0, 40.0))),
        (3, Some(model_usage(20.0, 40.0))),
    ]);
    assert_eq!(report.outcome, TickOutcome::Switched);
    assert_eq!(
        bench.live(),
        Some(2),
        "the lowest binding window, the fuller of 5h and Fable"
    );
}

// cswap: TestModelAwareSwitch.test_comma_separated_models_switch_on_any
#[test]
fn comma_separated_models_switch_on_any() {
    let mut bench = modelled("comma_separated_models_switch_on_any", &["Fable", "Opus"]);
    let reading = |five_hour: f64, fable: f64, opus: f64| {
        json!({
            "five_hour": {"pct": five_hour},
            "seven_day": {"pct": 0.0},
            "scoped": [{"name": "Fable", "pct": fable}, {"name": "Opus", "pct": opus}],
        })
    };
    let report = bench.tick_with(&[
        (1, Some(reading(5.0, 20.0, 100.0))),
        (2, Some(reading(5.0, 20.0, 30.0))),
        (3, Some(reading(5.0, 20.0, 70.0))),
    ]);
    assert_eq!(report.outcome, TickOutcome::Switched);
    assert_eq!(
        bench.live(),
        Some(2),
        "account 1 is maxed on Opus; account 2 has the most room"
    );
}

// cswap: TestModelAwareSwitch.test_all_sentinel_binds_every_scoped_window
#[test]
fn all_sentinel_binds_every_scoped_window() {
    let mut bench = modelled("all_sentinel_binds_every_scoped_window", &["all"]);
    let reading = |name: &str, pct: f64| json!({"five_hour": {"pct": 5.0}, "seven_day": {"pct": 0.0}, "scoped": [{"name": name, "pct": pct}]});
    let report = bench.tick_with(&[
        (1, Some(reading("Sonnet", 100.0))),
        (2, Some(reading("Sonnet", 20.0))),
        (3, Some(reading("Opus", 60.0))),
    ]);
    assert_eq!(report.outcome, TickOutcome::Switched);
    assert_eq!(bench.live(), Some(2));
}

// cswap: TestModelAwareSwitch.test_dual_exhausted_candidate_recovers_at_its_later_reset
#[test]
fn dual_exhausted_candidate_recovers_at_its_later_reset() {
    let mut bench = modelled(
        "dual_exhausted_candidate_recovers_at_its_later_reset",
        &["Fable"],
    );
    let (five_hour_reset, fable_reset, later) = (
        bench.reset_in(H),
        bench.reset_in(3 * H),
        bench.reset_in(8 * H),
    );
    let dual = json!({
        "five_hour": {"pct": 100.0, "resets_at": five_hour_reset},
        "seven_day": {"pct": 0.0},
        "scoped": [{"name": "Fable", "pct": 100.0, "resets_at": fable_reset}],
    });
    let report = bench.tick_with(&[
        (1, Some(model_usage(95.0, 10.0))),
        (2, Some(dual)),
        (3, Some(usage_at(100.0, &later))),
    ]);
    assert_eq!(report.outcome, TickOutcome::Blocked);
    assert_eq!(
        exhausted(&report),
        [Some(bench.in_secs(3 * H))],
        "account 2 is usable only at its later, Fable reset"
    );
}

// cswap: TestModelAwareSwitch.test_unknown_recovery_falls_back_instead_of_oversleeping
#[test]
fn unknown_recovery_falls_back_instead_of_oversleeping() {
    let mut bench = modelled(
        "unknown_recovery_falls_back_instead_of_oversleeping",
        &["Fable"],
    );
    let later = bench.reset_in(8 * H);
    let report = bench.tick_with(&[
        (1, Some(model_usage(95.0, 10.0))),
        (2, Some(model_usage(0.0, 100.0))),
        (3, Some(usage_at(100.0, &later))),
    ]);
    assert_eq!(report.outcome, TickOutcome::Blocked);
    assert_eq!(exhausted(&report), [None::<Timestamp>]);
    assert_eq!(report.sleep_target, None);
    let delay = next_delay(&report, INTERVAL, bench.now(), None, 0.5);
    assert!(
        (delay - NO_RESET_FALLBACK_S).abs() < f64::EPSILON,
        "the bounded fallback, not a sleep toward account 3's reset: {delay}"
    );
}

// cswap: TestModelAwareSwitch.test_scoped_only_exhaustion_drives_the_wake_time
#[test]
fn scoped_only_exhaustion_drives_the_wake_time() {
    let mut bench = modelled("scoped_only_exhaustion_drives_the_wake_time", &["Fable"]);
    let fable_reset = bench.reset_in(19 * H);
    let blocked = json!({
        "five_hour": {"pct": 3.0, "resets_at": bench.reset_in(H)},
        "seven_day": {"pct": 0.0},
        "scoped": [{"name": "Fable", "pct": 100.0, "resets_at": fable_reset}],
    });
    let report = bench.tick_with(&[
        (1, Some(model_usage(95.0, 10.0))),
        (2, Some(blocked.clone())),
        (3, Some(blocked)),
    ]);
    assert_eq!(report.outcome, TickOutcome::Blocked);
    assert_eq!(exhausted(&report), [Some(bench.in_secs(19 * H))]);
}

// cswap :965-973: a live login with no `accounts.json` is an unmanaged account, not a missing one.
#[test]
fn a_live_login_without_a_roster_is_unmanaged() {
    let mut bench = Bench::new("live_login_no_roster", 2);
    fs::remove_file(bench.root().accounts_json()).expect("accounts.json can be removed");
    let report = bench.tick_with(&[(1, Some(usage(95.0))), (2, Some(usage(10.0)))]);
    assert_eq!(report.outcome, TickOutcome::NoAction);
    assert_eq!(reasons(&report), ["unmanaged-active-account"]);
}

// cswap :949-953: recovered quarantines are lifted before the live login is looked at.
#[test]
fn recovered_quarantines_lift_before_the_roster_check() {
    let mut bench = three("quarantine_lift_before_roster");
    quarantine(&bench.store, 2, &address(2), "invalid_grant", bench.now())
        .expect("the quarantine is written");
    let replaced = json!({"claudeAiOauth": {"accessToken": "fake-access-2b", "refreshToken": "fake-refresh-2b", "expiresAt": FAR}});
    bench.put_credential(2, &replaced.to_string());
    let stranger =
        json!({"oauthAccount": {"emailAddress": "stranger@example.test", "organizationUuid": ""}})
            .to_string();
    write(bench.claude.global_config(), &stranger);
    let report = over_threshold(&mut bench);
    assert_eq!(reasons(&report), ["unmanaged-active-account"]);
    assert!(bodies(&report).any(|body| matches!(body, EventBody::Unquarantined { .. })));
}

// cswap :1008-1012: an account whose poll was a sentinel has no fetch error in the poll event.
#[test]
fn a_sentinel_account_has_no_fetch_error() {
    let mut bench = three("sentinel_no_fetch_error");
    bench.set_usage(&[(1, Some(usage(50.0))), (2, None), (3, None)]);
    bench.set_result(2, PollResult::AuthDead);
    let report = bench.tick();
    let EventBody::Poll { fetch_errors, .. } = &poll(&report).body else {
        unreachable!("poll() finds a poll event");
    };
    assert_eq!(fetch_errors, &BTreeMap::from([(3, "timeout".to_owned())]));
}

// A freshen refused because the drain began stops the tick without a word, as a draining switch does.
#[test]
fn a_draining_freshen_stops_silently() {
    let mut bench = Bench::new("freshen_draining", 2);
    bench.put_credential(2, &credential(2, 1));
    let _held = bench.drain.start();
    let report = bench.tick_with(&[(1, Some(usage(95.0))), (2, Some(usage(10.0)))]);
    assert_eq!(report.outcome, TickOutcome::NoAction);
    assert_eq!(errors(&report).len(), 0);
    assert_eq!(reasons(&report).len(), 0);
    assert!(
        bench.switcher.calls().is_empty(),
        "a draining tick switches nothing"
    );
}

// A target quarantined after the tick read the state is skipped under the state lock for the next one.
#[test]
fn a_target_quarantined_under_the_lock_is_skipped() {
    let mut bench = three("quarantined_under_lock");
    let store = Store::open(bench.root()).expect("the scratch store opens");
    let now = bench.now();
    bench.on_freshen(move || {
        quarantine(&store, 2, &address(2), "invalid_grant", now)
            .expect("the other engine's quarantine is written");
    });
    let report = over_threshold(&mut bench);
    assert_eq!(report.outcome, TickOutcome::Switched);
    assert_eq!(bench.switcher.calls(), [3]);
}

// A switch the state file could not record still starts the cooldown, from the engine's memory.
#[test]
fn an_unrecorded_switch_still_cools_down() {
    let mut bench = three("unrecorded_cooldown");
    bench.state_read_only(true);
    let first = over_threshold(&mut bench);
    assert_eq!(first.outcome, TickOutcome::Switched);
    assert_eq!(
        errors(&first).len(),
        1,
        "the unrecorded switch is an error event"
    );
    bench.advance(60);
    let second = bench.tick_with(&[
        (1, Some(usage(10.0))),
        (2, Some(usage(95.0))),
        (3, Some(usage(50.0))),
    ]);
    bench.state_read_only(false);
    assert_eq!(second.outcome, TickOutcome::NoAction);
    assert_eq!(reasons(&second), ["cooldown"]);
    assert_eq!(bench.switcher.calls(), [2]);
}

/// Each paid-fallback-entered event's account, from-account, amount and currency.
fn entered(report: &TickReport) -> Vec<(u32, u32, f64, String)> {
    bodies(report)
        .filter_map(|body| match body {
            EventBody::PaidFallbackEntered {
                number,
                from,
                amount,
                currency,
            } => Some((*number, *from, *amount, currency.clone())),
            _ => None,
        })
        .collect()
}

/// The paid move's precondition from account 1: the live login is still 1, and the target is opted in.
fn paid_from_one() -> SwitchPrecondition {
    SwitchPrecondition {
        expected_outgoing: 1,
        target_must_be_paid_opted_in: true,
        outgoing_must_be_opted_out: false,
    }
}

/// The `startedAt` of the engine lease, which the bench's engine claimed on an earlier tick.
fn lease_started_at(bench: &Bench) -> f64 {
    read_json(&bench.root().engine_lease())["startedAt"]
        .as_f64()
        .expect("the lease has a start")
}

/// A tick below the threshold, which claims the lease and decides nothing.
fn claim_lease(bench: &mut Bench, accounts: u32) {
    let rows: Vec<(u32, Option<Value>)> = (1..=accounts)
        .map(|number| (number, Some(usage(10.0))))
        .collect();
    assert_eq!(bench.tick_with(&rows).outcome, TickOutcome::NoAction);
    bench.advance(60);
}

/// Writes a state file whose `paidFallback` is on account `number`, entered in place, by `lease_owner`, with an
/// owner-exit bar set while only account `number` was opted in when `barred`.
fn write_paid_fallback(bench: &Bench, number: u32, lease_owner: &Value, barred: bool) {
    let mut paid =
        json!({"number": number, "from": number, "since": NOW_S - 600, "leaseOwner": lease_owner});
    if barred {
        paid["ownerExit"] = json!({"at": NOW_S - 300, "optedIn": [number]});
    }
    write(
        &bench.root().autoswitch_state(),
        &json!({"schemaVersion": 1, "paidFallback": paid}).to_string(),
    );
}

// I1: the opt-in is judged again at the switch. A flag cleared after the decision refuses the move, and the next paid
// target is tried; a balance that aged past 15 minutes by the switch refuses it under the state lock.
#[test]
fn paid_entry_needs_the_flag_at_the_switch() {
    let mut bench = three("paid_flag_at_switch");
    bench.paid_fleet(3);
    bench.fund(2, 200_000, 60);
    bench.fund(3, 100_000, 60);
    let path = bench.root().accounts_json();
    bench.switcher.before_switch(move || {
        let mut sequence = read_json(&path);
        sequence["accounts"]["2"]["maySpendCredits"] = json!(false);
        write(&path, &sequence.to_string());
    });
    let report = bench.tick();
    assert_eq!(report.outcome, TickOutcome::Switched);
    assert_eq!(
        bench.switch_calls(),
        [2, 3],
        "the refused target is skipped for the next"
    );
    assert_eq!(
        bench.switcher.preconditions(),
        [Some(paid_from_one()), Some(paid_from_one())]
    );
    assert_eq!(bench.live(), Some(3));
    assert_eq!(entered(&report), [(3, 1, 1000.0, "USD".to_owned())]);

    let mut aged = Bench::new("paid_age_at_switch", 2);
    aged.paid_fleet(2);
    aged.fund(2, 100_000, 899);
    let clock = Arc::clone(&aged.clock);
    aged.on_freshen(move || {
        clock.fetch_add(2_000, Ordering::SeqCst);
    });
    let report = aged.tick();
    assert!(
        aged.switch_calls().is_empty(),
        "a balance past 15 minutes at the switch is never spent on"
    );
    assert_eq!(reasons(&report), ["stale-balance"]);
    assert_eq!(aged.live(), Some(1));
    assert!(entered(&report).is_empty(), "no paid fallback is entered");
}

// I2: a cached balance over 15 minutes old is read again, with the token the freshen left, before the switch; no
// usage request is sent for it.
#[test]
fn paid_entry_reads_a_stale_balance_before_switching() {
    let mut bench = Bench::new("paid_stale_read", 2);
    bench.put_credential(2, &credential(2, bench.ms() + 60_000));
    bench.endpoint.grant("fresh-access-2", "fresh-refresh-2");
    bench.paid_fleet(2);
    bench.fund(2, 100_000, 1000);
    bench.answer_balance(Ok(bench.balance(150_000, 0)));
    let report = bench.tick();
    assert_eq!(report.outcome, TickOutcome::Switched);
    assert_eq!(
        bench.token_posts(),
        1,
        "the switch's own freshen is the one grant"
    );
    assert_eq!(
        *bench.collection.balance_calls.borrow(),
        [(2, "fresh-access-2".to_owned())],
        "one balance read, with the freshened token"
    );
    assert_eq!(
        bench.collection.scheduled_calls.get(),
        1,
        "only the tick's own collection"
    );
    assert!(bench.refetch_calls().is_empty(), "no usage refetch");
    assert!(bench.recheck_calls().is_empty(), "no exit re-read");
    assert_eq!(bench.switch_calls(), [2]);
    assert_eq!(
        entered(&report),
        [(2, 1, 1500.0, "USD".to_owned())],
        "the event names the fresh balance"
    );
}

// I2: a stale balance whose read fails, or reads 0, is not spent on: the tick holds with stale-balance.
#[test]
fn paid_entry_holds_when_the_balance_read_fails() {
    let mut bench = Bench::new("paid_read_fails", 2);
    bench.paid_fleet(2);
    bench.fund(2, 100_000, 1000);
    bench.answer_balance(Err(BalanceReadError::NoRow));
    let report = bench.tick();
    assert_eq!(report.outcome, TickOutcome::Blocked);
    assert_eq!(reasons(&report), ["stale-balance"]);
    assert!(
        exhausted(&report).is_empty(),
        "a paid target was found, so the tick is not all-exhausted"
    );
    assert!(bench.switch_calls().is_empty(), "no switch");

    bench.advance(121);
    bench.paid_fleet(2);
    bench.fund(2, 100_000, 1000);
    bench.answer_balance(Ok(bench.balance(0, 0)));
    let report = bench.tick();
    assert_eq!(
        reasons(&report),
        ["stale-balance"],
        "a balance that reads 0 is no target"
    );
    assert!(bench.switch_calls().is_empty(), "no switch");
    assert_eq!(bench.collection.balance_reads(), [2, 2]);
    assert_eq!(bench.live(), Some(1));
}

// Step 3's limit: an account's balance is read at most once per 2 minutes.
#[test]
fn a_balance_read_is_not_repeated_within_two_minutes() {
    let mut bench = Bench::new("paid_read_limit", 2);
    let stale_tick = |bench: &mut Bench| {
        bench.paid_fleet(2);
        bench.fund(2, 100_000, 1000);
        bench.answer_balance(Err(BalanceReadError::NoRow));
        bench.tick()
    };
    assert_eq!(reasons(&stale_tick(&mut bench)), ["stale-balance"]);
    bench.advance(60);
    assert_eq!(reasons(&stale_tick(&mut bench)), ["stale-balance"]);
    assert_eq!(
        bench.collection.balance_reads(),
        [2],
        "no second read within 2 minutes"
    );
    bench.advance(61);
    assert_eq!(reasons(&stale_tick(&mut bench)), ["stale-balance"]);
    assert_eq!(
        bench.collection.balance_reads(),
        [2, 2],
        "read again once 2 minutes passed"
    );
    assert!(bench.switch_calls().is_empty(), "no switch");
}

// I3: a proactive tick finds every account exhausted too, but never enters the paid fallback.
#[test]
fn no_paid_entry_on_a_proactive_all_exhausted_tick() {
    let mut bench = three("paid_not_proactive");
    bench.paid_fleet(3);
    bench.set_usage(&[
        (1, Some(usage(95.0))),
        (2, Some(usage(100.0))),
        (3, Some(usage(100.0))),
    ]);
    for number in 1..=3 {
        bench.set_extra_usage(number, true);
    }
    bench.fund(2, 100_000, 60);
    let report = bench.tick();
    assert_eq!(
        exhausted(&report).len(),
        1,
        "the proactive tick holds as all-exhausted"
    );
    assert!(bench.switch_calls().is_empty(), "no switch");
    assert!(entered(&report).is_empty(), "no paid fallback is entered");
}

// I3: a failover (the active account's usage unreadable) never enters the paid fallback.
#[test]
fn no_paid_entry_on_failover() {
    let mut bench = three("paid_not_failover");
    let mut last = None;
    for _ in 0..3 {
        bench.set_usage(&[(1, None), (2, Some(usage(100.0))), (3, Some(usage(100.0)))]);
        bench.set_extra_usage(2, true);
        bench.fund(2, 100_000, 60);
        last = Some(bench.tick());
    }
    let report = last.expect("three ticks ran");
    assert_eq!(
        exhausted(&report).len(),
        1,
        "the third tick fails over onto an all-exhausted census"
    );
    assert!(bench.switch_calls().is_empty(), "no switch");
    assert!(entered(&report).is_empty(), "no paid fallback is entered");
}

// I3: the warm-up decides nothing, paid entry included.
#[test]
fn no_paid_entry_during_warm_up() {
    let mut bench = three("paid_not_warming");
    bench.engine = Engine::new("ops-tower", 101, Some(bench.now()));
    bench.set_warm_settled(1);
    bench.paid_fleet(3);
    bench.fund(2, 100_000, 60);
    let report = bench.tick();
    assert_eq!(reasons(&report), ["warming-up"]);
    assert!(bench.switch_calls().is_empty(), "no switch");
    assert!(entered(&report).is_empty(), "no paid fallback is entered");
}

// I3: an owner's move off the paid account bars entry: the tick holds as all-exhausted.
#[test]
fn no_paid_entry_behind_an_owner_exit_bar() {
    let mut bench = three("paid_owner_exit_bar");
    claim_lease(&mut bench, 3);
    let ours = json!({"owner": "ops-tower", "pid": 101, "startedAt": lease_started_at(&bench)});
    write_paid_fallback(&bench, 2, &ours, true);
    bench.paid_fleet(3);
    bench.fund(2, 100_000, 60);
    let report = bench.tick();
    assert_eq!(exhausted(&report).len(), 1);
    assert!(bench.switch_calls().is_empty(), "no switch");
    assert!(entered(&report).is_empty(), "no paid fallback is entered");
}

// I4: the largest balance first, ties in rotation order (accounts.json's order, not the account numbers).
#[test]
fn paid_entry_picks_the_largest_balance_then_rotation_order() {
    let mut bench = Bench::new("paid_largest_then_rotation", 4);
    let path = bench.root().accounts_json();
    let mut sequence = read_json(&path);
    sequence["order"] = json!([1, 4, 3, 2]);
    write(&path, &sequence.to_string());
    bench.paid_fleet(4);
    bench.fund(2, 50_000, 60);
    bench.fund(3, 100_000, 60);
    bench.fund(4, 100_000, 60);
    let report = bench.tick();
    assert_eq!(report.outcome, TickOutcome::Switched);
    assert_eq!(bench.switch_calls(), [4]);
    assert_eq!(entered(&report), [(4, 1, 1000.0, "USD".to_owned())]);
}

// I4: an eligible active account is kept: paid mode is entered in place, with no switch.
#[test]
fn an_eligible_active_account_is_kept_in_place() {
    let mut bench = three("paid_in_place");
    bench.paid_fleet(3);
    bench.fund(1, 50_000, 60);
    bench.fund(2, 100_000, 60);
    let report = bench.tick();
    assert_eq!(report.outcome, TickOutcome::NoAction);
    assert!(
        bench.switch_calls().is_empty(),
        "an eligible active account is never left for a larger balance"
    );
    assert_eq!(entered(&report), [(1, 1, 500.0, "USD".to_owned())]);
    let paid = &bench.state()["paidFallback"];
    assert_eq!(
        (paid["number"].clone(), paid["from"].clone()),
        (json!(1), json!(1))
    );
    assert_eq!(paid["leaseOwner"]["owner"], json!("ops-tower"));
    assert_eq!(paid["leaseOwner"]["pid"], json!(101));
    assert_eq!(
        paid["leaseOwner"]["startedAt"].as_f64(),
        Some(lease_started_at(&bench))
    );
    assert!(
        bench.state().get("lastSwitchAt").is_none(),
        "an entry in place records no switch"
    );
}

// An at-limit tick that finds this engine already on the live account's credits reports on-credits and enters again.
#[test]
fn an_at_limit_tick_already_on_credits_reports_on_credits() {
    let mut bench = three("paid_on_credits");
    bench.paid_fleet(3);
    bench.fund(1, 50_000, 60);
    assert_eq!(entered(&bench.tick()).len(), 1);
    let state = bench.state();
    bench.advance(60);
    bench.paid_fleet(3);
    bench.fund(1, 50_000, 60);
    let report = bench.tick();
    assert_eq!(reasons(&report), ["on-credits"]);
    assert!(entered(&report).is_empty(), "no second entered event");
    assert!(bench.switch_calls().is_empty(), "no switch");
    assert_eq!(
        bench.state(),
        state,
        "the paid fallback is not written again"
    );
}

// I13: the entering switch writes paidFallback in its own record; a later normal switch clears it.
#[test]
fn paid_fallback_is_written_with_the_entering_switch_and_cleared_by_a_normal_switch() {
    let mut bench = three("paid_written_and_cleared");
    bench.paid_fleet(3);
    bench.fund(2, 100_000, 60);
    assert_eq!(bench.tick().outcome, TickOutcome::Switched);
    let state = bench.state();
    assert_eq!(state["lastSwitchTo"], json!("2"));
    let paid = &state["paidFallback"];
    assert_eq!(
        (paid["number"].clone(), paid["from"].clone()),
        (json!(2), json!(1))
    );
    assert_eq!(
        paid["since"], state["lastSwitchAt"],
        "set in the same write as the switch record"
    );
    assert_eq!(paid["leaseOwner"]["owner"], json!("ops-tower"));

    bench.advance(H);
    let report = bench.tick_with(&[
        (1, Some(usage(100.0))),
        (2, Some(usage(100.0))),
        (3, Some(usage(10.0))),
    ]);
    assert_eq!(report.outcome, TickOutcome::Switched);
    assert_eq!(bench.switch_calls(), [2, 3]);
    assert!(
        bench.state().get("paidFallback").is_none(),
        "a normal switch clears the paid fallback"
    );
}

// I14: the paid state of a paidFallback another engine wrote is ignored: this engine enters on its own.
#[test]
fn a_paid_fallback_of_another_lease_owner_is_ignored() {
    let mut bench = three("paid_other_owner");
    claim_lease(&mut bench, 3);
    let theirs = json!({"owner": "rustling-tulip", "pid": 77, "startedAt": NOW_S - 900});
    write_paid_fallback(&bench, 1, &theirs, false);
    bench.paid_fleet(3);
    bench.fund(1, 50_000, 60);
    let report = bench.tick();
    assert_eq!(
        reasons(&report),
        Vec::<&str>::new(),
        "no on-credits for another engine's paid fallback"
    );
    assert_eq!(entered(&report), [(1, 1, 500.0, "USD".to_owned())]);
    let paid = &bench.state()["paidFallback"];
    assert_eq!(paid["leaseOwner"]["owner"], json!("ops-tower"));
    assert!(paid.get("ownerExit").is_none());
}

// I8 and I14: an owner-exit bar is the owner's act, so it bars entry whichever engine (or an earlier run of this one)
// wrote it.
#[test]
fn an_owner_exit_bar_from_another_lease_owner_still_bars_entry() {
    let mut bench = three("paid_other_owner_bar");
    claim_lease(&mut bench, 3);
    let theirs = json!({"owner": "rustling-tulip", "pid": 77, "startedAt": NOW_S - 900});
    write_paid_fallback(&bench, 2, &theirs, true);
    let barred = bench.state();
    bench.paid_fleet(3);
    bench.fund(2, 100_000, 60);
    let report = bench.tick();
    assert_eq!(
        exhausted(&report).len(),
        1,
        "the bar holds the tick as all-exhausted"
    );
    assert!(bench.switch_calls().is_empty(), "no switch");
    assert!(entered(&report).is_empty(), "no paid fallback is entered");
    assert_eq!(bench.state(), barred, "the bar stays");
}

// I8: a normal switch keeps the owner-exit bar, the rest of the key with it: only a refill or an account gained by the
// opted-in set lifts it.
#[test]
fn a_non_paid_switch_keeps_the_owner_exit_bar() {
    let mut bench = three("paid_bar_survives_switch");
    claim_lease(&mut bench, 3);
    let ours = json!({"owner": "ops-tower", "pid": 101, "startedAt": lease_started_at(&bench)});
    write_paid_fallback(&bench, 2, &ours, true);
    let barred = bench.state()["paidFallback"].clone();
    bench.set_may_spend(2, true);
    let report = bench.tick_with(&[
        (1, Some(usage(100.0))),
        (2, Some(usage(100.0))),
        (3, Some(usage(98.0))),
    ]);
    assert_eq!(report.outcome, TickOutcome::Switched, "{:?}", report.events);
    assert_eq!(bench.switch_calls(), [3]);
    let state = bench.state();
    assert_eq!(state["lastSwitchTo"], json!("3"));
    let kept = &state["paidFallback"];
    assert_eq!(
        kept["ownerExit"]["optedIn"], barred["ownerExit"]["optedIn"],
        "the switch record keeps the bar: {kept}"
    );
    assert_eq!(
        kept["ownerExit"]["at"].as_f64(),
        barred["ownerExit"]["at"].as_f64()
    );
    assert_eq!(
        (kept["number"].clone(), kept["from"].clone()),
        (json!(2), json!(2)),
        "and the rest of the key"
    );
    assert_eq!(kept["since"].as_f64(), barred["since"].as_f64());
}

// D1: Claude Code idle on the paid account (its live token expired, so its poll is a sentinel) is no reason to leave:
// paid mode holds through the idle hold, with no switch and the paid fallback as it was.
#[test]
fn an_expired_live_token_on_the_paid_account_holds_without_exit() {
    let mut bench = three("paid_token_expired");
    enter_paid_on_two(&mut bench);
    let paid = bench.state()["paidFallback"].clone();
    for _ in 0..2 {
        paid_rows(&bench, &[100.0, 100.0, 100.0]);
        bench.set_result(2, PollResult::TokenExpired);
        let report = bench.tick();
        assert!(switches(&report).is_empty(), "{:?}", report.events);
        assert!(
            left(&report).is_empty(),
            "paid mode is not left: {:?}",
            report.events
        );
        bench.advance(60);
    }
    assert_eq!(bench.switch_calls(), [2], "only the entering switch");
    assert_eq!(
        bench.state()["paidFallback"],
        paid,
        "the paid fallback is unchanged"
    );
}

// I14: another engine takes the lease after the decision: the check under the state lock stops the paid switch.
#[test]
fn a_lease_taken_before_a_paid_switch_stops_it() {
    let mut bench = three("paid_lease_taken");
    bench.paid_fleet(3);
    bench.fund(2, 100_000, 60);
    let path = bench.root().engine_lease();
    let now = bench.now().as_second();
    bench.on_freshen(move || {
        let lease = json!({"owner": "rustling-tulip", "pid": 77, "startedAt": now, "heartbeatAt": now, "intervalSeconds": 60.0});
        write(&path, &lease.to_string());
    });
    let report = bench.tick();
    let TickOutcome::Paused { reason } = &report.outcome else {
        unreachable!("expected a pause, got {:?}", report.outcome);
    };
    assert!(
        reason.contains("rustling-tulip") && reason.contains("77"),
        "{reason}"
    );
    assert!(
        bench.switch_calls().is_empty(),
        "no paid switch without the lease"
    );
    assert_eq!(bench.live(), Some(1));
    assert!(bench.state().get("paidFallback").is_none());
    assert!(entered(&report).is_empty(), "no paid fallback is entered");
}

// A paid switch refused because the drain began stops the tick without a word.
#[test]
fn a_draining_switch_stops_a_paid_entry_silently() {
    let mut bench = three("paid_draining");
    bench.paid_fleet(3);
    bench.fund(2, 100_000, 60);
    bench.switcher.fail(SwitchErrorKind::Draining);
    let report = bench.tick();
    assert_eq!(report.outcome, TickOutcome::NoAction);
    assert_eq!(errors(&report).len(), 0);
    assert_eq!(reasons(&report).len(), 0);
    assert_eq!(switches(&report).len(), 0);
    assert!(entered(&report).is_empty(), "no paid fallback is entered");
    assert_eq!(bench.switch_calls(), [2]);
    assert!(bench.state().get("paidFallback").is_none());
}

// Step 3: the collector's refusal of a foreign live credential is a failed read: the next paid target is tried.
#[test]
fn a_foreign_credential_balance_refusal_moves_to_the_next_target() {
    let mut bench = three("paid_foreign_balance");
    bench.paid_fleet(3);
    bench.fund(2, 200_000, 1000);
    bench.fund(3, 100_000, 1000);
    bench.answer_balance(Err(BalanceReadError::ForeignCredential));
    bench.answer_balance(Ok(bench.balance(100_000, 0)));
    let report = bench.tick();
    assert_eq!(report.outcome, TickOutcome::Switched);
    assert_eq!(bench.collection.balance_reads(), [2, 3]);
    assert_eq!(bench.switch_calls(), [3]);
    assert_eq!(entered(&report), [(3, 1, 1000.0, "USD".to_owned())]);
}

/// Each paid-fallback-left event's account and reason.
fn left(report: &TickReport) -> Vec<(u32, &'static str)> {
    bodies(report)
        .filter_map(|body| match body {
            EventBody::PaidFallbackLeft { number, reason, .. } => Some((*number, *reason)),
            _ => None,
        })
        .collect()
}

/// Each warning event's message.
fn warnings(report: &TickReport) -> Vec<String> {
    bodies(report)
        .filter_map(|body| match body {
            EventBody::Warning { message } => Some(message.clone()),
            _ => None,
        })
        .collect()
}

/// Enters the paid fallback on account 2 of a three-account bench by a switch from account 1, then lets a minute pass.
fn enter_paid_on_two(bench: &mut Bench) {
    bench.paid_fleet(3);
    bench.fund(2, 100_000, 60);
    assert_eq!(bench.tick().outcome, TickOutcome::Switched);
    assert_eq!(bench.live(), Some(2));
    bench.advance(60);
}

/// Each account's 5-hour window at its entry of `pcts` (account 1 first), resetting in 3 hours, with extra usage on,
/// and account 2 funded with a balance read a minute ago.
fn paid_rows(bench: &Bench, pcts: &[f64]) {
    let reset = bench.reset_in(3 * H);
    let rows: Vec<(u32, Option<Value>)> = (1_u32..)
        .zip(pcts)
        .map(|(number, pct)| (number, Some(usage_at(*pct, &reset))))
        .collect();
    bench.set_usage(&rows);
    for (number, _) in &rows {
        bench.set_extra_usage(*number, true);
    }
    bench.fund(2, 100_000, 60);
}

// I5 and I4: in paid mode neither a larger balance nor a refill under 3 % moves the login; the first tick an account has
// 3 % headroom, the engine switches to it and the leaving switch clears the paid fallback.
#[test]
fn paid_mode_leaves_at_three_percent_not_before() {
    let mut bench = three("paid_leaves_at_three");
    enter_paid_on_two(&mut bench);
    paid_rows(&bench, &[100.0, 100.0, 100.0]);
    bench.fund(3, 500_000, 60);
    let report = bench.tick();
    assert_eq!(reasons(&report), ["on-credits"]);
    assert_eq!(
        bench.switch_calls(),
        [2],
        "a larger balance never moves the login"
    );

    bench.advance(60);
    paid_rows(&bench, &[100.0, 100.0, 98.0]);
    let report = bench.tick();
    assert_eq!(report.outcome, TickOutcome::NoAction);
    assert_eq!(reasons(&report), ["on-credits"]);
    assert_eq!(bench.switch_calls(), [2], "2 % headroom is not a refill");

    bench.advance(60);
    paid_rows(&bench, &[100.0, 100.0, 97.0]);
    let report = bench.tick();
    assert_eq!(report.outcome, TickOutcome::Switched);
    assert_eq!(bench.switch_calls(), [2, 3]);
    assert_eq!(left(&report), [(2, "refill")]);
    let refilled = bodies(&report).find_map(|body| match body {
        EventBody::PaidFallbackLeft {
            to, headroom_pct, ..
        } => Some((*to, *headroom_pct)),
        _ => None,
    });
    let (to, headroom_pct) = refilled.expect("the leave carries its event");
    assert_eq!(
        to,
        Some(3),
        "the left event names the refilled account the login moved to"
    );
    assert!(
        headroom_pct.is_some_and(|pct| pct >= 3.0),
        "and its headroom of at least 3 %: {headroom_pct:?}"
    );
    assert!(
        entered(&report).is_empty(),
        "the leaving switch spends no credits"
    );
    assert!(
        bench.state().get("paidFallback").is_none(),
        "the leaving switch clears the paid fallback"
    );
}

// I5: the paid account's own window refilling ends paid mode with no switch.
#[test]
fn the_paid_accounts_own_refill_ends_paid_mode_without_a_switch() {
    let mut bench = three("paid_own_refill");
    enter_paid_on_two(&mut bench);
    paid_rows(&bench, &[100.0, 99.0, 100.0]);
    let report = bench.tick();
    assert_eq!(report.outcome, TickOutcome::NoAction);
    assert_eq!(left(&report), [(2, "own-refill")]);
    assert_eq!(bench.switch_calls(), [2], "no switch");
    let state = bench.state();
    assert!(state.get("paidFallback").is_none());
    assert_eq!(state["lastSwitchTo"], json!("2"), "the switch record stays");
}

// I13: the paid fallback round-trips through the ticks spent on credits, its unknown keys included, and the leaving
// switch clears it in the same write as its switch record, keeping the file's other keys.
#[test]
fn paid_fallback_round_trips_and_clears_with_the_leaving_switch() {
    let mut bench = three("paid_round_trip");
    enter_paid_on_two(&mut bench);
    let mut state = bench.state();
    state["paidFallback"]["note"] = json!("kept");
    state["custom"] = json!(1);
    write(&bench.root().autoswitch_state(), &state.to_string());
    paid_rows(&bench, &[100.0, 100.0, 100.0]);
    assert_eq!(reasons(&bench.tick()), ["on-credits"]);
    assert_eq!(
        bench.state(),
        state,
        "a tick on credits leaves the file as it was"
    );

    bench.advance(60);
    paid_rows(&bench, &[100.0, 100.0, 50.0]);
    let report = bench.tick();
    assert_eq!(report.outcome, TickOutcome::Switched);
    assert_eq!(left(&report), [(2, "refill")]);
    let after = bench.state();
    assert!(
        after.get("paidFallback").is_none(),
        "cleared by the leaving switch"
    );
    assert_eq!(after["lastSwitchTo"], json!("3"));
    assert_eq!(after["lastSwitchFrom"], json!(2));
    assert_eq!(after["custom"], json!(1), "the file's other keys are kept");
}

// I6: the paid account's flag turned off moves the login within the tick, even while a candidate's usage is unreadable:
// with no other paid target, onto the account paid mode was entered from, whose extra usage is known off.
#[test]
fn flag_off_exits_paid_mode_even_while_a_candidate_is_unreadable() {
    let mut bench = three("paid_flag_off_unreadable");
    enter_paid_on_two(&mut bench);
    let reset = bench.reset_in(3 * H);
    bench.set_usage(&[
        (1, Some(usage_at(100.0, &reset))),
        (2, Some(usage_at(100.0, &reset))),
        (3, None),
    ]);
    bench.set_extra_usage(1, false);
    bench.set_extra_usage(2, true);
    bench.set_prepaid(2, 100_000, 60);
    bench.set_may_spend(2, false);
    let report = bench.tick();
    assert_eq!(report.outcome, TickOutcome::Switched);
    assert_eq!(bench.switch_calls(), [2, 1]);
    assert_eq!(
        bench.switcher.preconditions().last(),
        Some(&Some(SwitchPrecondition {
            expected_outgoing: 2,
            target_must_be_paid_opted_in: false,
            outgoing_must_be_opted_out: true,
        })),
        "the exit move checks the live login is still the paid account"
    );
    assert_eq!(left(&report), [(2, "flag-off")]);
    assert!(entered(&report).is_empty(), "the move spends no credits");
    assert!(bench.state().get("paidFallback").is_none());
    assert_eq!(bench.live(), Some(1));
}

/// Paid mode on account 2, entered from account 1, with 2's flag now off: account 1's reading has extra usage known
/// off, account 3's usage is unreadable, and no account is a paid target.
fn flag_off_on_two(test: &str) -> Bench {
    let mut bench = three(test);
    enter_paid_on_two(&mut bench);
    let reset = bench.reset_in(3 * H);
    bench.set_usage(&[
        (1, Some(usage_at(100.0, &reset))),
        (2, Some(usage_at(100.0, &reset))),
        (3, None),
    ]);
    bench.set_extra_usage(1, false);
    bench.set_extra_usage(2, true);
    bench.set_prepaid(2, 100_000, 60);
    bench.set_may_spend(2, false);
    bench
}

/// Makes the refetch answer what the collection holds, with account 1's extra usage `on`.
fn refetch_one_with_extra_usage(bench: &Bench, on: bool) {
    let mut collected = bench.collection.collected.borrow().clone();
    let entry = collected
        .entries
        .get_mut(&1)
        .expect("account 1 has a usage entry");
    let reading = entry.last_good.as_mut().expect("account 1 has a reading");
    reading["extra_usage"] = json!({"enabled": on, "user_disabled": false, "spend_limit_reached": false, "disabled_reason_set": false});
    *bench.collection.refetched.borrow_mut() = Some(collected);
}

/// Whether one of the report's warnings says the paid account holds with no account to move to.
fn holds_with_no_target(report: &TickReport) -> bool {
    warnings(report)
        .iter()
        .any(|message| message.contains("no account to move to"))
}

// I6 and I1: an exit target is read again before the move: an account whose extra usage turned on since the tick's
// reading would spend credits without its opt-in, so it is skipped, and with no other target the paid account holds.
#[test]
fn an_exit_skips_an_account_whose_extra_usage_turned_on() {
    let mut bench = flag_off_on_two("paid_exit_extra_on");
    refetch_one_with_extra_usage(&bench, true);
    let report = bench.tick();
    assert_eq!(bench.switch_calls(), [2], "no move onto account 1");
    assert!(holds_with_no_target(&report), "{:?}", warnings(&report));
    assert_eq!(bench.recheck_calls(), [1]);
    assert_eq!(bench.live(), Some(2));
}

// The hold the fleet line shows: a tick that leaves the login on the paid account, spending credits it may no longer
// spend, names that account in its report.
#[test]
fn a_held_exit_reports_the_credits_hold() {
    let mut bench = flag_off_on_two("paid_exit_credits_hold");
    refetch_one_with_extra_usage(&bench, true);
    let report = bench.tick();
    assert!(holds_with_no_target(&report), "{:?}", warnings(&report));
    assert_eq!(bench.live(), Some(2));
    assert_eq!(report.credits_hold, Some(2));
}

// I6 and I9: the exit move carries the precondition that the live login is still the paid account.
#[test]
fn an_exit_move_carries_the_outgoing_precondition() {
    let mut bench = flag_off_on_two("paid_exit_precondition");
    refetch_one_with_extra_usage(&bench, false);
    let report = bench.tick();
    assert_eq!(report.outcome, TickOutcome::Switched);
    assert_eq!(bench.switch_calls(), [2, 1]);
    assert_eq!(
        bench.switcher.preconditions().last(),
        Some(&Some(SwitchPrecondition {
            expected_outgoing: 2,
            target_must_be_paid_opted_in: false,
            outgoing_must_be_opted_out: true,
        }))
    );
    assert_eq!(bench.recheck_calls(), [1]);
    assert_eq!(report.credits_hold, None, "a move holds the login nowhere");
}

// OPS-453: a refill leave is an exit move: it carries the precondition that the live login is still the paid account.
#[test]
fn a_refill_exit_carries_the_outgoing_precondition() {
    let mut bench = three("paid_refill_precondition");
    enter_paid_on_two(&mut bench);
    paid_rows(&bench, &[100.0, 100.0, 97.0]);
    let report = bench.tick();
    assert_eq!(report.outcome, TickOutcome::Switched);
    assert_eq!(bench.switch_calls(), [2, 3]);
    assert_eq!(
        bench.switcher.preconditions().last(),
        Some(&Some(SwitchPrecondition {
            expected_outgoing: 2,
            target_must_be_paid_opted_in: false,
            outgoing_must_be_opted_out: false,
        })),
        "the refill leave checks the live login is still the paid account"
    );
    assert_eq!(left(&report), [(2, "refill")]);
}

// OPS-453: a refill leave refused because the live login is no longer the paid account stops: the owner moved the
// login, so the leave never overwrites the owner's move, no other target is tried and the paid fallback stays.
#[test]
fn a_refill_exit_stops_when_the_owner_moved_the_login() {
    let mut bench = three("paid_refill_owner_moved");
    enter_paid_on_two(&mut bench);
    paid_rows(&bench, &[96.0, 100.0, 97.0]);
    bench.switcher.fail(SwitchErrorKind::PreconditionFailed);
    let report = bench.tick();
    assert_eq!(report.outcome, TickOutcome::NoAction);
    let calls = bench.switch_calls();
    assert_eq!(
        calls.len(),
        2,
        "only the first-ranked of the two refilled targets is tried: {calls:?}"
    );
    assert!(
        left(&report).is_empty(),
        "the login was not left by the engine"
    );
    assert!(
        bench.state().get("paidFallback").is_some(),
        "the paid fallback is not cleared"
    );
}

// OPS-453 and I11: the exit target's re-read spends no grant: a 401 for the target is recorded, no token is posted,
// and the target, not read again, is not an exit target.
#[test]
fn an_exit_refetch_answers_a_401_without_a_grant() {
    let mut bench = flag_off_on_two("paid_exit_recheck_401");
    let path = bench.root().accounts_json();
    let mut sequence = read_json(&path);
    sequence["active"] = json!(2);
    write(&path, &sequence.to_string());
    let transport = Rc::new(Unauthorized::default());
    let over: Rc<dyn UsageTransport> = Rc::<Unauthorized>::clone(&transport);
    *bench.collection.recheck_over.borrow_mut() = Some(over);
    let posts = bench.token_posts();
    let report = bench.tick();
    assert_eq!(bench.recheck_calls(), [1]);
    assert_eq!(
        transport.answers.get(),
        1,
        "the target's usage was read once and answered 401"
    );
    assert_eq!(bench.token_posts(), posts, "no token POST");
    assert_eq!(bench.switch_calls(), [2], "no move onto account 1");
    assert!(holds_with_no_target(&report), "{:?}", warnings(&report));
    assert_eq!(bench.live(), Some(2));
}

// OPS-453: a bar salvaged from a malformed paid fallback is lifted by a refill: the compare-and-swap matches the
// still-malformed file, and the key is removed while the file's other keys stay.
#[test]
fn a_salvaged_bar_is_lifted_by_a_refill() {
    let mut bench = three("paid_salvaged_bar_lifted");
    let malformed =
        json!({"number": 2, "from": 1, "since": 1.0, "ownerExit": 5, "leaseOwner": "x"});
    write(
        &bench.root().autoswitch_state(),
        &json!({"custom": 1, "paidFallback": malformed}).to_string(),
    );
    let report = bench.tick_with(&[
        (1, Some(usage(10.0))),
        (2, Some(usage(10.0))),
        (3, Some(usage(10.0))),
    ]);
    assert!(
        bench.switch_calls().is_empty(),
        "no switch: {:?}",
        report.outcome
    );
    let state = bench.state();
    assert!(
        state.get("paidFallback").is_none(),
        "the salvaged bar is lifted: {state}"
    );
    assert_eq!(state["custom"], json!(1), "the file's other keys are kept");
}

// I6: an exit target the refetch did not fetch this tick (a backoff, a hold or another collector's claim kept it off)
// is not moved to on its stored reading: holding on the paid account beats moving onto extra usage that may be on.
#[test]
fn an_exit_skips_a_target_whose_usage_could_not_be_refreshed() {
    let mut bench = flag_off_on_two("paid_exit_not_refreshed");
    refetch_one_with_extra_usage(&bench, false);
    let mut entry = bench
        .collection
        .collected
        .borrow()
        .entries
        .get(&1)
        .expect("account 1 has a usage entry")
        .clone();
    entry.fetched_at = Some(bench.in_secs(-600));
    entry.consecutive_failures = 1;
    bench.set_refetch_entry(1, entry);
    let report = bench.tick();
    assert_eq!(bench.switch_calls(), [2], "no move onto account 1");
    assert!(holds_with_no_target(&report), "{:?}", warnings(&report));
    assert_eq!(bench.live(), Some(2));
}

// I8 and I9: an exit move refused because the live login is no longer the paid account stops the exit: the owner moved
// the login, so no other target is tried and no "no account to move to" warning is given; the next tick sets the bar.
#[test]
fn an_exit_stops_when_the_owner_moved_the_login() {
    let mut bench = flag_off_on_two("paid_exit_owner_moved");
    bench.switcher.fail(SwitchErrorKind::PreconditionFailed);
    let report = bench.tick();
    assert_eq!(bench.switch_calls(), [2, 1], "one exit attempt");
    assert!(!holds_with_no_target(&report), "{:?}", warnings(&report));

    bench.advance(60);
    write_live(&bench.claude, 3);
    paid_rows(&bench, &[100.0, 100.0, 100.0]);
    let report = bench.tick();
    assert_eq!(left(&report), [(2, "owner-exit")]);
    assert!(
        bench.state()["paidFallback"].get("ownerExit").is_some(),
        "the owner's move sets the bar: {}",
        bench.state()
    );
}

// I6: an exit target whose re-read fails (the recheck's error path) is not moved to on the tick's old reading.
#[test]
fn an_exit_skips_a_target_whose_refetch_fails() {
    let mut bench = flag_off_on_two("paid_exit_refetch_fails");
    bench.fail_refetch();
    let report = bench.tick();
    assert_eq!(bench.switch_calls(), [2], "no move onto account 1");
    assert!(holds_with_no_target(&report), "{:?}", warnings(&report));
    assert_eq!(bench.live(), Some(2));
}

// I6: the flag turned off moves the login even while the paid account's own usage is unknown: onto the eligible paid
// account, which the paid fallback then names.
#[test]
fn flag_off_exits_while_the_active_usage_is_unknown() {
    let mut bench = three("paid_flag_off_active_unknown");
    enter_paid_on_two(&mut bench);
    let reset = bench.reset_in(3 * H);
    bench.set_usage(&[
        (1, Some(usage_at(100.0, &reset))),
        (2, None),
        (3, Some(usage_at(100.0, &reset))),
    ]);
    bench.set_extra_usage(1, true);
    bench.set_extra_usage(3, true);
    bench.fund(3, 50_000, 60);
    bench.set_may_spend(2, false);
    let report = bench.tick();
    assert_eq!(report.outcome, TickOutcome::Switched);
    assert_eq!(bench.switch_calls(), [2, 3]);
    assert_eq!(
        bench.switcher.preconditions().last(),
        Some(&Some(SwitchPrecondition {
            expected_outgoing: 2,
            target_must_be_paid_opted_in: true,
            outgoing_must_be_opted_out: true,
        }))
    );
    assert_eq!(left(&report), [(2, "flag-off")]);
    assert_eq!(entered(&report), [(3, 2, 500.0, "USD".to_owned())]);
    let paid = &bench.state()["paidFallback"];
    assert_eq!(
        (paid["number"].clone(), paid["from"].clone()),
        (json!(3), json!(1)),
        "the record keeps the original entry's from"
    );
}

// I7 and I11: in paid mode the paid account's balance is read every 2 minutes with the live token, with no usage
// request and no grant.
#[test]
fn paid_mode_reads_the_balance_every_two_minutes_without_a_usage_get() {
    let mut bench = three("paid_stay_reads");
    enter_paid_on_two(&mut bench);
    bench.set_live(2, "", &credential(2, FAR));
    for _ in 0..4 {
        bench.answer_balance(Ok(bench.balance(100_000, 0)));
    }
    for _ in 0..4 {
        paid_rows(&bench, &[100.0, 100.0, 100.0]);
        assert_eq!(reasons(&bench.tick()), ["on-credits"]);
        bench.advance(60);
    }
    let live = (2, "fake-access-2".to_owned());
    assert_eq!(
        *bench.collection.balance_calls.borrow(),
        [live.clone(), live],
        "a read on the first tick on credits and one 2 minutes later"
    );
    assert_eq!(
        bench.collection.scheduled_calls.get(),
        5,
        "only each tick's own collection"
    );
    assert!(bench.refetch_calls().is_empty(), "no usage refetch");
    assert!(bench.recheck_calls().is_empty(), "no exit re-read");
    assert_eq!(bench.token_posts(), 0, "no grant");
}

// I11: an expired live token is never refreshed for a balance read: the read waits for a live token that works.
#[test]
fn a_balance_read_never_spends_a_grant() {
    let mut bench = three("paid_read_no_grant");
    enter_paid_on_two(&mut bench);
    bench.set_live(2, "", &credential(2, bench.ms() - 1000));
    paid_rows(&bench, &[100.0, 100.0, 100.0]);
    assert_eq!(
        reasons(&bench.tick()),
        ["on-credits"],
        "a fresh cached balance carries the tick"
    );
    assert!(
        bench.collection.balance_reads().is_empty(),
        "no read with an expired token"
    );
    assert_eq!(bench.token_posts(), 0, "no grant for it");

    bench.advance(60);
    bench.set_live(2, "", &credential(2, FAR));
    bench.answer_balance(Ok(bench.balance(100_000, 0)));
    paid_rows(&bench, &[100.0, 100.0, 100.0]);
    assert_eq!(reasons(&bench.tick()), ["on-credits"]);
    assert_eq!(
        *bench.collection.balance_calls.borrow(),
        [(2, "fake-access-2".to_owned())]
    );
    assert_eq!(bench.token_posts(), 0, "the live token is used as it is");
}

// I8: the owner's move off the paid account bars paid entry, with one warning for the at-limit account it moved to,
// which spends credits it was not allowed to; a credits toggle that adds an account to the opted-in set lifts the bar.
#[test]
fn a_manual_exit_from_the_paid_account_is_not_reentered() {
    let mut bench = three("paid_manual_exit");
    enter_paid_on_two(&mut bench);
    write_live(&bench.claude, 1);
    paid_rows(&bench, &[100.0, 100.0, 100.0]);
    let report = bench.tick();
    assert_eq!(left(&report), [(2, "owner-exit")]);
    assert_eq!(warnings(&report).len(), 1, "{:?}", warnings(&report));
    assert_eq!(
        exhausted(&report).len(),
        1,
        "the bar holds the tick as all-exhausted"
    );
    assert_eq!(
        bench.switch_calls(),
        [2],
        "no move back onto the paid account"
    );
    assert_eq!(
        bench.state()["paidFallback"]["ownerExit"]["optedIn"],
        json!([2])
    );

    bench.advance(60);
    paid_rows(&bench, &[100.0, 100.0, 100.0]);
    let report = bench.tick();
    assert!(left(&report).is_empty(), "the exit is reported once");
    assert!(warnings(&report).is_empty(), "the warning is given once");
    assert_eq!(bench.switch_calls(), [2]);

    bench.advance(60);
    paid_rows(&bench, &[100.0, 100.0, 100.0]);
    bench.fund(3, 50_000, 60);
    let report = bench.tick();
    assert_eq!(
        report.outcome,
        TickOutcome::Switched,
        "an account gained by the opted-in set lifts the bar"
    );
    assert_eq!(bench.switch_calls(), [2, 2]);
    assert_eq!(entered(&report), [(2, 1, 1000.0, "USD".to_owned())]);
}

/// Rewrites the state file's owner-exit bar as set while the accounts `numbers` were opted in.
fn set_bar_opted_in(bench: &Bench, numbers: &[u32]) {
    let path = bench.root().autoswitch_state();
    let mut state = read_json(&path);
    state["paidFallback"]["ownerExit"]["optedIn"] = json!(numbers);
    write(&path, &state.to_string());
}

/// A bench whose state file holds an owner-exit bar on account 2, set while accounts 2 and 3 were opted in, with every
/// account at its limit and accounts 2 and 3 funded and opted in.
fn barred_with_two_and_three(test: &str) -> Bench {
    let mut bench = three(test);
    claim_lease(&mut bench, 3);
    let ours = json!({"owner": "ops-tower", "pid": 101, "startedAt": lease_started_at(&bench)});
    write_paid_fallback(&bench, 2, &ours, true);
    set_bar_opted_in(&bench, &[2, 3]);
    bench.paid_fleet(3);
    bench.fund(2, 100_000, 60);
    bench.fund(3, 100_000, 60);
    bench
}

// I8: turning a credits toggle off never lifts the owner-exit bar: only a gain in the opted-in set or a refill does.
#[test]
fn turning_a_toggle_off_under_an_owner_exit_bar_keeps_it() {
    let mut bench = barred_with_two_and_three("paid_bar_toggle_off");
    bench.set_may_spend(3, false);
    let report = bench.tick();
    assert_eq!(
        exhausted(&report).len(),
        1,
        "the bar holds the tick as all-exhausted"
    );
    assert!(bench.switch_calls().is_empty(), "no switch");
    assert!(entered(&report).is_empty(), "no paid fallback is entered");
    assert_eq!(
        bench.state()["paidFallback"]["ownerExit"]["optedIn"],
        json!([2, 3]),
        "the bar stays as written"
    );
}

// I8: an owner-exit bar whose opted-in set cannot be read, in a paidFallback that is malformed, still bars entry.
#[test]
fn a_malformed_owner_exit_still_bars_paid_entry() {
    let mut bench = three("paid_malformed_bar");
    claim_lease(&mut bench, 3);
    let paid =
        json!({"number": 2, "from": 1, "since": 1.0, "leaseOwner": "x", "ownerExit": {"at": 5.0}});
    write(
        &bench.root().autoswitch_state(),
        &json!({"schemaVersion": 1, "paidFallback": paid}).to_string(),
    );
    bench.paid_fleet(3);
    bench.fund(2, 100_000, 60);
    let report = bench.tick();
    assert_eq!(
        exhausted(&report).len(),
        1,
        "the bar holds the tick as all-exhausted"
    );
    assert!(bench.switch_calls().is_empty(), "no switch");
    assert!(entered(&report).is_empty(), "no paid fallback is entered");
}

// I8: a toggle turned off and on again leaves the opted-in set within the bar's, so the bar stays.
#[test]
fn turning_an_account_off_and_on_again_keeps_the_owner_exit_bar() {
    let mut bench = barred_with_two_and_three("paid_bar_toggle_off_on");
    bench.set_may_spend(3, false);
    let report = bench.tick();
    assert!(
        entered(&report).is_empty(),
        "no paid fallback is entered with the toggle off"
    );

    bench.advance(60);
    bench.paid_fleet(3);
    bench.fund(2, 100_000, 60);
    bench.fund(3, 100_000, 60);
    let report = bench.tick();
    assert!(
        entered(&report).is_empty(),
        "no paid fallback is entered with the toggle on again"
    );
    assert!(bench.switch_calls().is_empty(), "no switch");
    assert_eq!(
        bench.state()["paidFallback"]["ownerExit"]["optedIn"],
        json!([2, 3]),
        "the bar stays as written"
    );
}

// I8: the owner's move onto another eligible paid account is kept: the engine never moves the login back to the larger
// balance it left.
#[test]
fn a_manual_switch_onto_an_eligible_account_is_kept() {
    let mut bench = three("paid_manual_onto_eligible");
    bench.paid_fleet(3);
    bench.fund(2, 100_000, 60);
    bench.fund(3, 50_000, 60);
    assert_eq!(bench.tick().outcome, TickOutcome::Switched);
    write_live(&bench.claude, 3);
    for _ in 0..2 {
        bench.advance(60);
        paid_rows(&bench, &[100.0, 100.0, 100.0]);
        bench.fund(3, 50_000, 60);
        let report = bench.tick();
        assert!(switches(&report).is_empty(), "{:?}", report.events);
        assert!(
            warnings(&report).is_empty(),
            "an opted-in account spends with consent"
        );
    }
    assert_eq!(bench.switch_calls(), [2]);
    assert_eq!(bench.live(), Some(3));
}

/// The pause reason of a tick that ran while auto-switch was off.
const AUTO_SWITCH_OFF: &str = "auto-switch is off";

/// `test`'s three-account bench with auto-switch off.
fn switched_off(test: &str) -> Bench {
    let mut bench = three(test);
    bench.auto_switch_enabled = false;
    bench
}

/// Whether `report` paused because auto-switch is off.
fn paused_off(report: &TickReport) -> bool {
    report.outcome
        == TickOutcome::Paused {
            reason: AUTO_SWITCH_OFF.to_owned(),
        }
}

// While auto-switch is off, a tick still reads usage and reports it, then moves nothing: no proactive switch, no
// at-limit failover and no paid-fallback entry, in place or by a switch.
#[test]
fn auto_switch_off_pauses_after_reading_usage_and_switches_nothing() {
    let mut proactive = switched_off("off_proactive");
    let report = over_threshold(&mut proactive);
    assert!(paused_off(&report), "{:?}", report.outcome);
    assert!(proactive.collection_calls() > 0, "the tick read usage");
    assert!(
        report
            .events
            .iter()
            .any(|event| matches!(event.body, EventBody::Poll { .. }))
    );
    assert!(proactive.switch_calls().is_empty(), "no proactive switch");
    assert_eq!(proactive.live(), Some(1));

    let mut at_limit = switched_off("off_at_limit");
    let report = at_limit.tick_with(&[
        (1, Some(usage(100.0))),
        (2, Some(usage(10.0))),
        (3, Some(usage(20.0))),
    ]);
    assert!(paused_off(&report), "{:?}", report.outcome);
    assert!(at_limit.switch_calls().is_empty(), "no at-limit switch");
    assert!(switches(&report).is_empty(), "no switch event");
    assert_eq!(at_limit.live(), Some(1));

    let mut in_place = switched_off("off_paid_in_place");
    in_place.paid_fleet(3);
    in_place.fund(1, 50_000, 60);
    let report = in_place.tick();
    assert!(paused_off(&report), "{:?}", report.outcome);
    assert!(
        entered(&report).is_empty(),
        "no paid fallback is entered in place"
    );
    assert!(
        in_place.state().get("paidFallback").is_none(),
        "nothing is written"
    );

    let mut paid_switch = switched_off("off_paid_switch");
    paid_switch.paid_fleet(3);
    paid_switch.fund(2, 100_000, 60);
    let report = paid_switch.tick();
    assert!(paused_off(&report), "{:?}", report.outcome);
    assert!(paid_switch.switch_calls().is_empty(), "no paid move");
    assert!(
        entered(&report).is_empty(),
        "no paid fallback is entered by a switch"
    );
    assert!(paid_switch.state().get("paidFallback").is_none());
    assert_eq!(paid_switch.live(), Some(1));
}

// An engine with auto-switch off still claims and heartbeats its lease, so no other engine takes over.
#[test]
fn auto_switch_off_still_heartbeats_the_lease() {
    let mut bench = switched_off("off_heartbeats");
    let rows = [
        (1, Some(usage(50.0))),
        (2, Some(usage(10.0))),
        (3, Some(usage(20.0))),
    ];
    assert!(paused_off(&bench.tick_with(&rows)));
    let first = read_json(&bench.root().engine_lease());
    assert_eq!(first["owner"], "ops-tower");
    assert_eq!(first["pid"], 101);

    bench.advance(60);
    assert!(paused_off(&bench.tick_with(&rows)));
    let second = read_json(&bench.root().engine_lease());
    let beat = |lease: &Value| {
        lease["heartbeatAt"]
            .as_f64()
            .expect("the lease has a heartbeat")
    };
    assert!(
        beat(&second) > beat(&first),
        "the lease was heartbeaten: {first} then {second}"
    );
    assert_eq!(
        second["startedAt"], first["startedAt"],
        "the same hold on the lease"
    );
}

// Another engine's lease shows before auto-switch off, and the paused engine reads no usage.
#[test]
fn auto_switch_off_reports_another_engines_lease_first() {
    let mut bench = switched_off("off_another_lease");
    let now = bench.now().as_second();
    let lease = json!({"owner": "rustling-tulip", "pid": 77, "startedAt": now, "heartbeatAt": now, "intervalSeconds": 60.0});
    write(&bench.root().engine_lease(), &lease.to_string());
    let report = over_threshold(&mut bench);
    let TickOutcome::Paused { reason } = &report.outcome else {
        unreachable!("expected a pause, got {:?}", report.outcome);
    };
    assert!(
        reason.contains("rustling-tulip") && reason.contains("77"),
        "{reason}"
    );
    assert_eq!(bench.collection_calls(), 0);
    assert_eq!(lease_owner(&bench), Some(json!("rustling-tulip")));
}

// While auto-switch is off, a target it cannot move to is not prepared: its expiring token is not refreshed, so no grant
// is spent and its dead grant is not found and quarantined, and nothing switches.
#[test]
fn auto_switch_off_refreshes_no_target() {
    let mut bench = switched_off("off_no_refresh");
    bench.put_credential(2, &credential(2, 1));
    bench
        .endpoint
        .answer(400, json!({"error": "invalid_grant"}));
    let freshened = bench.freshen_seen();
    let report = bench.tick_with(&[
        (1, Some(usage(95.0))),
        (2, Some(usage(10.0))),
        (3, Some(usage(20.0))),
    ]);
    assert!(paused_off(&report), "{:?}", report.outcome);
    assert!(!freshened.load(Ordering::SeqCst), "no target is freshened");
    assert_eq!(bench.token_posts(), 0, "no grant is spent");
    assert!(
        quarantines(&report).is_empty(),
        "{:?}",
        quarantines(&report)
    );
    assert!(bench.state()["quarantine"].get("2").is_none());
    assert!(bench.switch_calls().is_empty(), "no switch");
    assert_eq!(bench.live(), Some(1));
}

// While auto-switch is off, a tick that finds no live login, or one no managed account has, or an API-key login,
// reads as paused too.
#[test]
fn auto_switch_off_reads_paused_with_no_live_login() {
    let mut none = switched_off("off_no_live_login");
    fs::remove_file(none.claude.global_config()).expect("the live config can be removed");
    let report = none.tick_with(&[
        (1, Some(usage(95.0))),
        (2, Some(usage(10.0))),
        (3, Some(usage(20.0))),
    ]);
    assert!(paused_off(&report), "{:?}", report.outcome);
    assert!(none.switch_calls().is_empty(), "nothing to switch from");

    let mut unmanaged = switched_off("off_unmanaged_login");
    let stranger =
        json!({"oauthAccount": {"emailAddress": "stranger@example.test", "organizationUuid": ""}})
            .to_string();
    write(unmanaged.claude.global_config(), &stranger);
    assert!(paused_off(&over_threshold(&mut unmanaged)));

    let mut api_key = switched_off("off_api_key_login");
    mark_api_key(&api_key, 1);
    assert!(paused_off(&over_threshold(&mut api_key)));
    assert!(
        api_key.switch_calls().is_empty(),
        "an API-key login is never switched"
    );
}

// While auto-switch is off, the warm-up reads as paused from the first tick.
#[test]
fn auto_switch_off_reads_paused_during_warm_up() {
    let mut bench = switched_off("off_warm_up");
    bench.engine = Engine::new("ops-tower", 101, Some(bench.now()));
    bench.collection.warm_settled.set(Some(2));
    let report = bench.tick_with(&[
        (1, Some(usage(95.0))),
        (2, Some(usage(10.0))),
        (3, Some(usage(20.0))),
    ]);
    assert!(paused_off(&report), "{:?}", report.outcome);
    assert!(bench.engine.warming(), "the warm-up still runs");
    assert!(
        report
            .events
            .iter()
            .any(|event| matches!(event.body, EventBody::Poll { .. }))
    );
    assert!(
        bench.switch_calls().is_empty(),
        "no switch during the warm-up"
    );
}

// Turned back on, the engine acts at the very next tick, on that tick's usage.
#[test]
fn auto_switch_back_on_switches_at_the_next_tick_from_current_data() {
    let mut bench = switched_off("off_back_on");
    assert!(paused_off(&over_threshold(&mut bench)));
    assert!(bench.switch_calls().is_empty(), "no switch while off");

    bench.auto_switch_enabled = true;
    bench.advance(1);
    let report = over_threshold(&mut bench);
    assert_eq!(report.outcome, TickOutcome::Switched);
    assert_eq!(bench.switch_calls(), [2]);
    assert_eq!(bench.live(), Some(2));
}

/// The precondition of the owner's leave off paid mode on account 2 to an account spending no credits.
fn opted_out_exit() -> SwitchPrecondition {
    SwitchPrecondition {
        expected_outgoing: 2,
        target_must_be_paid_opted_in: false,
        outgoing_must_be_opted_out: true,
    }
}

/// [`flag_off_on_two`] with auto-switch then turned off.
fn off_flag_off_on_two(test: &str) -> Bench {
    let mut bench = flag_off_on_two(test);
    bench.auto_switch_enabled = false;
    bench
}

// OPS-460: while auto-switch is off, turning the paid account's "May spend credits" off permits one move: the leave onto
// the account paid mode was entered from, whose extra usage is known off. The next tick moves nothing.
#[test]
fn off_flag_off_exit_moves_once_to_an_account_with_extra_usage_known_off() {
    let mut bench = off_flag_off_on_two("off_exit_unpaid");
    refetch_one_with_extra_usage(&bench, false);
    let report = bench.tick();
    assert!(paused_off(&report), "{:?}", report.outcome);
    assert_eq!(bench.switch_calls(), [2, 1]);
    assert_eq!(
        bench.switcher.preconditions().last(),
        Some(&Some(opted_out_exit()))
    );
    assert_eq!(
        switches(&report).len(),
        1,
        "the switch event stays in the report"
    );
    assert_eq!(left(&report), [(2, "flag-off")]);
    assert!(
        bench.state().get("paidFallback").is_none(),
        "the leaving switch clears the paid fallback"
    );
    assert_eq!(bench.live(), Some(1));

    bench.advance(60);
    let report = bench.tick();
    assert!(paused_off(&report), "{:?}", report.outcome);
    assert_eq!(bench.switch_calls(), [2, 1], "one move only");
    assert!(
        left(&report).is_empty(),
        "no left event: {:?}",
        left(&report)
    );
}

// OPS-460: the permitted leave tries the paid targets first, as an on engine does: the largest balance, recorded as the
// paid fallback with the original entry's from.
#[test]
fn off_flag_off_exit_moves_to_the_largest_eligible_paid_target() {
    let mut bench = three("off_exit_paid");
    enter_paid_on_two(&mut bench);
    paid_rows(&bench, &[100.0, 100.0, 100.0]);
    bench.fund(1, 50_000, 60);
    bench.fund(3, 80_000, 60);
    bench.set_may_spend(2, false);
    bench.auto_switch_enabled = false;
    let report = bench.tick();
    assert!(paused_off(&report), "{:?}", report.outcome);
    assert_eq!(bench.switch_calls(), [2, 3]);
    let paid_leave = SwitchPrecondition {
        target_must_be_paid_opted_in: true,
        ..opted_out_exit()
    };
    assert_eq!(
        bench.switcher.preconditions().last(),
        Some(&Some(paid_leave))
    );
    assert_eq!(left(&report), [(2, "flag-off")]);
    assert_eq!(entered(&report), [(3, 2, 800.0, "USD".to_owned())]);
    let paid = &bench.state()["paidFallback"];
    assert_eq!(
        (paid["number"].clone(), paid["from"].clone()),
        (json!(3), json!(1))
    );
}

// OPS-460: the permission rests on the stored flag, not on the first clause the account fails: a paid account that is
// disabled as well as opted out still leaves, and the left event says `flag-off`.
#[test]
fn off_flag_off_exit_reaches_a_disabled_paid_account() {
    let mut bench = off_flag_off_on_two("off_exit_disabled");
    bench.set_account(2, "disabled", json!(true));
    refetch_one_with_extra_usage(&bench, false);
    let report = bench.tick();
    assert_eq!(bench.switch_calls(), [2, 1], "{:?}", report.events);
    assert_eq!(
        bench.switcher.preconditions().last(),
        Some(&Some(opted_out_exit()))
    );
    assert_eq!(left(&report), [(2, "flag-off")]);
    assert_eq!(bench.live(), Some(1));
}

// OPS-460: while off, a refill elsewhere is no leave (it would never carry the permission); the flag-off exit still
// runs, by the exit rules.
#[test]
fn off_flag_off_exit_skips_the_refill_leave() {
    let mut bench = three("off_exit_skips_refill");
    enter_paid_on_two(&mut bench);
    paid_rows(&bench, &[100.0, 100.0, 90.0]);
    bench.set_extra_usage(1, false);
    bench.set_may_spend(2, false);
    refetch_one_with_extra_usage(&bench, false);
    bench.auto_switch_enabled = false;
    let report = bench.tick();
    assert!(paused_off(&report), "{:?}", report.outcome);
    assert_eq!(
        bench.switch_calls(),
        [2, 1],
        "the exit target, not the refilled account 3"
    );
    assert_eq!(
        bench.switcher.preconditions().last(),
        Some(&Some(opted_out_exit()))
    );
    assert_eq!(left(&report), [(2, "flag-off")]);
}

// OPS-460: while off, with the paid account still opted in, a refill elsewhere moves nothing and paid mode stays, with
// no balance read, no re-read and no freshen.
#[test]
fn off_refill_leave_never_moves_while_the_flag_is_on() {
    let mut bench = three("off_refill_flag_on");
    enter_paid_on_two(&mut bench);
    paid_rows(&bench, &[100.0, 100.0, 90.0]);
    bench.auto_switch_enabled = false;
    let freshened = bench.freshen_seen();
    let report = bench.tick();
    assert!(paused_off(&report), "{:?}", report.outcome);
    assert_eq!(bench.switch_calls(), [2], "no move");
    assert!(
        left(&report).is_empty(),
        "no left event: {:?}",
        left(&report)
    );
    assert_eq!(
        bench.state()["paidFallback"]["number"],
        json!(2),
        "paid mode stays"
    );
    assert!(!freshened.load(Ordering::SeqCst), "nothing is freshened");
    assert!(
        bench.collection.balance_reads().is_empty(),
        "no balance read"
    );
    assert!(bench.recheck_calls().is_empty(), "no re-read");
}

// OPS-460: a paid entry never carries the permission, even from a live account whose flag is off: on, its precondition
// has no opted-out clause; off, it is not made at all.
#[test]
fn off_paid_entry_never_carries_the_permit() {
    let mut on = three("on_paid_entry_permit");
    on.paid_fleet(3);
    on.fund(2, 100_000, 60);
    assert_eq!(on.tick().outcome, TickOutcome::Switched);
    assert_eq!(on.switcher.preconditions(), [Some(paid_from_one())]);

    let mut off = switched_off("off_paid_entry_permit");
    off.paid_fleet(3);
    off.fund(2, 100_000, 60);
    let freshened = off.freshen_seen();
    let report = off.tick();
    assert!(paused_off(&report), "{:?}", report.outcome);
    assert!(off.switch_calls().is_empty(), "no paid move");
    assert!(
        entered(&report).is_empty(),
        "no entry: {:?}",
        entered(&report)
    );
    assert!(
        !freshened.load(Ordering::SeqCst),
        "no paid target is freshened"
    );
    assert!(
        off.collection.balance_reads().is_empty(),
        "no paid target's balance is read"
    );
}

// OPS-460 and J11: while off, paid mode on an opted-in account whose reading is unknown does no exit work: no freshen,
// no balance read, no re-read, no switch, and paid mode stays.
#[test]
fn off_exit_for_another_reason_does_no_work() {
    let mut bench = three("off_exit_other_reason");
    enter_paid_on_two(&mut bench);
    let reset = bench.reset_in(3 * H);
    bench.set_usage(&[
        (1, Some(usage_at(100.0, &reset))),
        (2, None),
        (3, Some(usage_at(100.0, &reset))),
    ]);
    bench.set_extra_usage(1, false);
    bench.set_extra_usage(3, true);
    bench.fund(3, 50_000, 60);
    bench.auto_switch_enabled = false;
    let freshened = bench.freshen_seen();
    let posts = bench.token_posts();
    let report = bench.tick();
    assert!(paused_off(&report), "{:?}", report.outcome);
    assert_eq!(bench.switch_calls(), [2], "no move");
    assert!(
        left(&report).is_empty(),
        "no left event: {:?}",
        left(&report)
    );
    assert!(
        warnings(&report).is_empty(),
        "no warning: {:?}",
        warnings(&report)
    );
    assert!(!freshened.load(Ordering::SeqCst), "nothing is freshened");
    assert_eq!(bench.token_posts(), posts, "no grant");
    assert!(
        bench.collection.balance_reads().is_empty(),
        "no balance read"
    );
    assert!(bench.recheck_calls().is_empty(), "no re-read");
    assert_eq!(
        bench.state()["paidFallback"]["number"],
        json!(2),
        "paid mode stays"
    );
}

// OPS-460: with no account to move to, the permitted leave holds as an on engine's does: the warning and the credits
// hold, every tick.
#[test]
fn off_flag_off_exit_with_no_target_holds_and_warns_each_tick() {
    let mut bench = off_flag_off_on_two("off_exit_no_target");
    refetch_one_with_extra_usage(&bench, true);
    for _ in 0..2 {
        let report = bench.tick();
        assert!(paused_off(&report), "{:?}", report.outcome);
        assert!(holds_with_no_target(&report), "{:?}", warnings(&report));
        assert_eq!(report.credits_hold, Some(2));
        bench.advance(60);
    }
    assert_eq!(bench.switch_calls(), [2], "no move");
    assert_eq!(bench.live(), Some(2));
}

// OPS-460 and J7: "May spend credits" turned back on before the switch holds its locks refuses the permitted leave:
// nothing moves, no other target is tried, and paid mode stays.
#[test]
fn off_flag_off_exit_stops_when_the_flag_is_back_on_under_the_locks() {
    let mut bench = off_flag_off_on_two("off_exit_flag_back_on");
    refetch_one_with_extra_usage(&bench, false);
    let path = bench.root().accounts_json();
    bench.switcher.before_switch(move || {
        let mut sequence = read_json(&path);
        sequence["accounts"]["2"]["maySpendCredits"] = json!(true);
        write(&path, &sequence.to_string());
    });
    let report = bench.tick();
    assert!(paused_off(&report), "{:?}", report.outcome);
    assert_eq!(bench.switch_calls(), [2, 1], "one attempt");
    assert!(
        left(&report).is_empty(),
        "no left event: {:?}",
        left(&report)
    );
    assert!(!holds_with_no_target(&report), "{:?}", warnings(&report));
    assert_eq!(bench.live(), Some(2));
    assert_eq!(bench.state()["paidFallback"]["number"], json!(2));
}

/// `test`'s three-account bench, every account at its limit with extra usage on and account 1, the live login, funded
/// and opted in: an at-limit tick enters the paid fallback in place.
fn in_place(test: &str) -> Bench {
    let bench = three(test);
    bench.paid_fleet(3);
    bench.fund(1, 50_000, 60);
    bench
}

// OPS-460 Part 3: the in-place entry writes only through the switch's re-read of the setting: auto-switch found off
// there writes nothing and pauses; on again, the next tick enters.
#[test]
fn an_in_place_entry_rereads_the_setting_under_the_gate() {
    let mut bench = in_place("in_place_rereads");
    bench.switcher.answer_while_on(WhileOn::Off);
    let report = bench.tick();
    assert!(paused_off(&report), "{:?}", report.outcome);
    assert_eq!(bench.switcher.while_on_calls.get(), 1);
    assert!(
        entered(&report).is_empty(),
        "no entry: {:?}",
        entered(&report)
    );
    assert!(
        bench.state().get("paidFallback").is_none(),
        "nothing is written"
    );

    bench.advance(60);
    bench.paid_fleet(3);
    bench.fund(1, 50_000, 60);
    let report = bench.tick();
    assert_eq!(report.outcome, TickOutcome::NoAction);
    assert_eq!(entered(&report), [(1, 1, 500.0, "USD".to_owned())]);
    assert_eq!(bench.state()["paidFallback"]["number"], json!(1));
}

// OPS-460 Part 3: a gate held by a switch in flight leaves the in-place entry to the next tick, with no write and no
// event.
#[test]
fn an_in_place_entry_skips_while_the_gate_is_busy() {
    let mut bench = in_place("in_place_busy");
    bench.switcher.answer_while_on(WhileOn::Busy);
    let report = bench.tick();
    assert_eq!(report.outcome, TickOutcome::NoAction);
    assert!(
        entered(&report).is_empty(),
        "no entry: {:?}",
        entered(&report)
    );
    assert!(
        bench.state().get("paidFallback").is_none(),
        "nothing is written"
    );

    bench.advance(60);
    bench.paid_fleet(3);
    bench.fund(1, 50_000, 60);
    let report = bench.tick();
    assert_eq!(entered(&report), [(1, 1, 500.0, "USD".to_owned())]);
    assert_eq!(bench.switcher.while_on_calls.get(), 2);

    // An unrecorded paid move's marker outlives a busy entry: the entry that records it emits no second entered event.
    let mut moved = three("in_place_busy_unrecorded");
    moved.paid_fleet(3);
    moved.fund(2, 100_000, 60);
    moved.state_read_only(true);
    let report = moved.tick();
    moved.state_read_only(false);
    assert_eq!(
        entered(&report).len(),
        1,
        "the unrecorded move announces the entry: {:?}",
        report.events
    );
    assert_eq!(moved.live(), Some(2));
    for answer in [WhileOn::Busy, WhileOn::Ran] {
        moved.advance(60);
        moved.paid_fleet(3);
        moved.fund(2, 100_000, 60);
        moved.switcher.answer_while_on(answer);
        let report = moved.tick();
        assert!(
            entered(&report).is_empty(),
            "no second entered event: {:?}",
            report.events
        );
    }
    assert_eq!(
        moved.switcher.while_on_calls.get(),
        2,
        "both entries were tried in place"
    );
    assert_eq!(
        moved.state()["paidFallback"]["number"],
        json!(2),
        "the second records it"
    );
}

/// Paid mode on account 2, every account at its limit with extra usage on and account 2 funded, then auto-switch off.
fn off_in_paid_mode(test: &str) -> Bench {
    let mut bench = three(test);
    enter_paid_on_two(&mut bench);
    paid_rows(&bench, &[100.0, 100.0, 100.0]);
    bench.auto_switch_enabled = false;
    bench
}

/// Whether an off tick of `bench` that ends paid mode in place did it without a move or any exit work, and the next
/// tick emits no left event again.
#[track_caller]
fn ends_in_place(bench: &mut Bench, reason: &'static str) {
    let freshened = bench.freshen_seen();
    let report = bench.tick();
    assert!(paused_off(&report), "{:?}", report.outcome);
    assert_eq!(left(&report), [(2, reason)]);
    assert!(
        bench.state().get("paidFallback").is_none(),
        "the paid fallback is cleared"
    );
    assert_eq!(bench.switch_calls(), [2], "no switch");
    assert!(!freshened.load(Ordering::SeqCst), "nothing is freshened");
    assert!(
        bench.collection.balance_reads().is_empty(),
        "no balance read"
    );
    assert!(bench.recheck_calls().is_empty(), "no re-read");

    bench.advance(60);
    paid_rows(bench, &[100.0, 100.0, 100.0]);
    let report = bench.tick();
    assert!(left(&report).is_empty(), "paid mode ends once");
    assert_eq!(bench.switch_calls(), [2]);
}

// OPS-460: while off, a balance used up ends paid mode in place, with no switch.
#[test]
fn off_ineligible_for_balance_used_up_ends_paid_mode_in_place() {
    let mut bench = off_in_paid_mode("off_balance_used_up");
    bench.set_prepaid(2, 0, 60);
    ends_in_place(&mut bench, "balance-zero");
}

// OPS-460: while off, extra usage turned off in Claude ends paid mode in place, with no switch.
#[test]
fn off_ineligible_for_extra_usage_off_ends_paid_mode_in_place() {
    let mut bench = off_in_paid_mode("off_extra_usage_off");
    bench.set_extra_usage(2, false);
    ends_in_place(&mut bench, "extra-usage-off");
}

// OPS-460: while off, a balance that reads stale keeps paid mode until it reads again: no write, no event, no read.
#[test]
fn off_with_a_stale_balance_keeps_paid_mode() {
    let mut bench = off_in_paid_mode("off_stale_balance");
    bench.set_prepaid(2, 100_000, 1_000);
    keeps_paid_mode(&mut bench);
}

/// Whether an off tick of `bench` keeps paid mode on account 2: no write, no event, no read, no switch.
#[track_caller]
fn keeps_paid_mode(bench: &mut Bench) {
    let report = bench.tick();
    assert!(paused_off(&report), "{:?}", report.outcome);
    assert!(
        left(&report).is_empty(),
        "no left event: {:?}",
        left(&report)
    );
    assert_eq!(
        bench.state()["paidFallback"]["number"],
        json!(2),
        "paid mode is kept"
    );
    assert!(
        bench.collection.balance_reads().is_empty(),
        "no balance read"
    );
    assert_eq!(bench.switch_calls(), [2]);
}

// OPS-460: while off, a zero balance that reads stale keeps paid mode until it reads again, as any stale balance does.
#[test]
fn off_with_a_stale_zero_balance_keeps_paid_mode() {
    let mut bench = off_in_paid_mode("off_stale_zero_balance");
    bench.set_prepaid(2, 0, 1_000);
    keeps_paid_mode(&mut bench);
}

// OPS-460: while off, a used-up balance ends paid mode in place whatever the record's other clauses say: an account
// taken out of the rotation, its flag on, still ends it.
#[test]
fn off_disabled_paid_account_still_ends_in_place_when_its_balance_is_used_up() {
    let mut bench = off_in_paid_mode("off_disabled_balance_used_up");
    bench.set_account(2, "disabled", json!(true));
    bench.set_prepaid(2, 0, 60);
    ends_in_place(&mut bench, "balance-zero");
}

// OPS-460: while off, extra usage turned off ends paid mode in place on an account taken out of the rotation too.
#[test]
fn off_disabled_paid_account_still_ends_in_place_when_extra_usage_turns_off() {
    let mut bench = off_in_paid_mode("off_disabled_extra_usage_off");
    bench.set_account(2, "disabled", json!(true));
    bench.set_extra_usage(2, false);
    ends_in_place(&mut bench, "extra-usage-off");
}

// OPS-460: while off, a consume-first tick that found a target refetches nothing for it: no phase-2 refetch, so no
// refreshing poll and no grant.
#[test]
fn an_off_consume_first_tick_refetches_nothing() {
    let mut bench = consume_first("off_consume_first_refetch");
    bench.auto_switch_enabled = false;
    let (soon, later, latest) = weekly_resets(&bench);
    let report = bench.tick_with(&[
        (1, Some(usage7(20.0, 20.0, &later))),
        (2, Some(usage7(10.0, 10.0, &soon))),
        (3, Some(usage7(10.0, 10.0, &latest))),
    ]);
    assert!(paused_off(&report), "{:?}", report.outcome);
    assert!(
        bench.refetch_calls().is_empty(),
        "no refetch: {:?}",
        bench.refetch_calls()
    );
    assert_eq!(bench.token_posts(), 0, "no grant");
    assert!(
        bench.switch_calls().is_empty(),
        "no switch: {:?}",
        bench.switch_calls()
    );

    bench.auto_switch_enabled = true;
    bench.advance(60);
    bench.tick_with(&[
        (1, Some(usage7(20.0, 20.0, &later))),
        (2, Some(usage7(10.0, 10.0, &soon))),
        (3, Some(usage7(10.0, 10.0, &latest))),
    ]);
    assert_eq!(
        bench.refetch_calls().len(),
        1,
        "on, the same tick refetches"
    );
}

// OPS-460 and J7: a permitted leave refused under the locks stops the exit there: no other paid target is freshened or
// read, no unpaid target is re-read, and no `stale-balance` line is given.
#[test]
fn a_flag_off_leave_refused_under_the_locks_tries_no_other_target() {
    let mut bench = three("off_leave_refused_stops");
    enter_paid_on_two(&mut bench);
    paid_rows(&bench, &[100.0, 100.0, 100.0]);
    bench.fund(1, 50_000, 60);
    bench.fund(3, 80_000, 60);
    bench.set_extra_usage(1, false);
    bench.set_may_spend(2, false);
    // Account 1's token is expired, so a freshen of it would ask the token endpoint.
    bench.put_credential(1, &credential(1, 1));
    bench.auto_switch_enabled = false;
    let path = bench.root().accounts_json();
    bench.switcher.before_switch(move || {
        let mut sequence = read_json(&path);
        sequence["accounts"]["2"]["maySpendCredits"] = json!(true);
        write(&path, &sequence.to_string());
    });
    let posts = bench.token_posts();
    let report = bench.tick();
    assert!(paused_off(&report), "{:?}", report.outcome);
    assert_eq!(bench.switch_calls(), [2, 3], "one attempt");
    assert_eq!(bench.token_posts(), posts, "no other target is freshened");
    assert!(
        bench.collection.balance_reads().is_empty(),
        "no balance read"
    );
    assert!(bench.recheck_calls().is_empty(), "no re-read");
    assert!(
        !reasons(&report).contains(&"stale-balance"),
        "{:?}",
        reasons(&report)
    );
    assert!(
        left(&report).is_empty(),
        "no left event: {:?}",
        left(&report)
    );
    assert_eq!(bench.state()["paidFallback"]["number"], json!(2));
}

// OPS-460: on, a paid account both disabled and opted out leaves with the left reason `flag-off`, and its log and
// warning name the same reason.
#[test]
fn on_disabled_opted_out_paid_account_leaves_with_reason_flag_off() {
    let mut bench = flag_off_on_two("on_disabled_opted_out");
    bench.set_account(2, "disabled", json!(true));
    refetch_one_with_extra_usage(&bench, false);
    let report = bench.tick();
    assert_eq!(report.outcome, TickOutcome::Switched);
    assert_eq!(bench.switch_calls(), [2, 1]);
    assert_eq!(left(&report), [(2, "flag-off")]);

    let mut held = flag_off_on_two("on_disabled_opted_out_held");
    held.set_account(2, "disabled", json!(true));
    refetch_one_with_extra_usage(&held, true);
    let report = held.tick();
    assert!(
        warnings(&report)
            .iter()
            .any(|message| message.contains("(flag-off)")),
        "{:?}",
        warnings(&report)
    );
}
