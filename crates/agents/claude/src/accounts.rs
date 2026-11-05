//! The plugin's account service over the `claude-accounts` store, the three
//! blocking HTTP transports the store's polls, refreshes and profile checks go
//! through, and the process table its engine checks lease holders against.
//!
//! Every reqwest client here is blocking. The service builds one client
//! inside the first call that sends, which the daemon runs off its async
//! threads, shares it among the three transports and every later call, and
//! drops it on a thread of its own, so the service may be dropped anywhere.

mod history;

use std::collections::BTreeMap;
use std::io::{self, Read};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, TryLockError};
use std::time::Duration;

use self::history::UsageHistory;
use agent_api::accounts::{AccountEventRow, EngineTick};
use agent_api::{
    AccountKindRow, AccountRef, AccountRow, AccountsConfig, AccountsError, AccountsFailure,
    AccountsFailureKind, AccountsService, AccountsSnapshot, AutoState, AutoSwitchRules, FleetRow,
    FleetWindowRow, PollReport, PollScope, SwitchReport, UsageRow, UsageStatus, UsageWindowRow,
};
use chrono::{DateTime, Utc};
use claude_accounts::engine::{
    Engine, EventBody, Lease, LeaseHolder, LiveSwitch, SwitchFailure, SwitchFn, TickInputs,
    TickOutcome, TickReport, TransportCollector, WhileOn, next_delay,
};
use claude_accounts::pace::{
    self, AccountReading, FleetWindow, Rotation, WindowReading, WindowVerdict,
};
use claude_accounts::{
    AccessToken, AccountKind, AccountLock, AccountRecord, AccountsRoot, AutoSwitchSection,
    AutoswitchState, CaptureError, ClaudePaths, Drain, FetchScope, Identity, LockError, LockTiming,
    PROFILE_TIMEOUT, PROFILE_URL, PROFILE_USER_AGENT, ProbeVerdicts, ProfileTransport, RateWindow,
    RawProfile, RawToken, RawUsage, RefreshContext, RefreshRequest, RefreshTransport, Secret,
    Settings, SettingsError, Store, StoreError, StoreErrorKind, StrategyName, SwitchAccount,
    SwitchError, SwitchErrorKind, SwitchOutcome, SwitchPrecondition, TOKEN_URL, TransportError,
    USAGE_BETA_HEADER, USAGE_TIMEOUT, USAGE_URL, UsageEntry, UsageTable, UsageTransport,
    UsageWindow, capture_live_login, poll_with_refresh, probe_outgoing,
};
use directories::UserDirs;
use protocol::AutoSwitchStrategy;
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

/// The auto-switch engine's name in the lease it holds.
const ENGINE_OWNER: &str = "rustling-tulip";

/// Why a halted engine stands by, for the snapshot and the halt event.
const HALTED: &str = "a switch failed and so did putting the live login back; auto-switch is halted until it is turned off and on again or the daemon restarts";

/// The paused reason of a cooldown setting that is no duration.
const COOLDOWN_UNREADABLE: &str = "the auto-switch cooldown could not be read";

/// What the last engine tick left for the snapshot's auto-switch state.
#[derive(Debug, Default)]
struct EngineView {
    paused_reason: Option<String>,
    last_decision: Option<String>,
    warm_up_done: bool,
}

/// The account service over the store at an [`AccountsConfig`]'s root. It
/// opens the store on each call and holds no lock across calls but the
/// engine's own, during a tick; what it keeps is the HTTP client, the stop
/// drain, the ownership verdicts, the process table and the auto-switch
/// engine, each safe to share between threads.
pub struct ClaudeAccounts {
    cfg: AccountsConfig,
    /// Built by the first call that sends, never on an async thread.
    client: OnceLock<Client>,
    drain: Arc<Drain>,
    verdicts: ProbeVerdicts,
    processes: Box<dyn proc_table::ProcessTable + Send + Sync>,
    /// The auto-switch engine while auto-switch is on, made by the first
    /// tick that finds it on; held across a tick, so one runs at a time.
    engine: Mutex<Option<Engine>>,
    /// What the last tick left for [`AccountsService::snapshot`]; `None`
    /// while no engine runs.
    view: Mutex<Option<EngineView>>,
    /// The usage samples the fleet forecast reads, recorded by each snapshot
    /// from the store's saved usage. Held only while a forecast is computed,
    /// and never together with another lock here.
    history: Mutex<UsageHistory>,
    /// Held by an engine switch across its re-read of the settings and the
    /// switch, and by a settings write: a tick under way when auto-switch is
    /// turned off makes no switch after the write.
    switch_guard: Mutex<()>,
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
            engine: Mutex::new(None),
            view: Mutex::new(None),
            history: Mutex::new(UsageHistory::default()),
            switch_guard: Mutex::new(()),
        }
    }

    /// How many usage samples the fleet forecast keeps, after the last
    /// snapshot recorded its readings.
    #[doc(hidden)]
    #[must_use]
    pub fn history_samples(&self) -> usize {
        lock(&self.history).samples.len()
    }

    /// The auto-switch state for a snapshot: the settings' ([`auto_state`]),
    /// the fleet forecast over `rows`, and what the last tick left while
    /// auto-switch is on.
    fn auto_snapshot(&self, root: &AccountsRoot, rows: &[AccountRow]) -> Option<AutoState> {
        let mut auto = auto_state(root)?;
        auto.fleet = self.fleet_forecast(root, rows);
        if auto.enabled
            && let Some(view) = &*lock(&self.view)
        {
            // A running engine's own reason, or none, stands over the
            // settings' cooldown reason.
            auto.paused_reason.clone_from(&view.paused_reason);
            auto.warm_up_done = view.warm_up_done;
            auto.last_decision.clone_from(&view.last_decision);
        }
        Some(auto)
    }

    /// The fleet forecast over `rows`: the crate's verdict under the
    /// settings' rate window, with `rows` recorded into the history kept
    /// here; `None` when the settings or the auto-switch state cannot be
    /// read, or when no account is on the timeline.
    fn fleet_forecast(&self, root: &AccountsRoot, rows: &[AccountRow]) -> Option<FleetRow> {
        let settings = Settings::read(&root.settings()).ok()?;
        let state = AutoswitchState::read(root).ok()?;
        let engine_settings = settings.autoswitch.to_engine();
        let readings = fleet_readings(rows);
        let rotation = Rotation {
            active: rows.iter().find(|row| row.active).map(|row| row.number),
            settings: &engine_settings,
            state: &state,
        };
        let rate_span = settings.fleet.rate_window.span();
        let now = jiff::Timestamp::now();
        let verdict = {
            let mut history = lock(&self.history);
            history.record(&readings, now);
            pace::fleet(&readings, &history.samples, rotation, rate_span, now).verdict
        };
        verdict.as_ref().map(verdict_row)
    }

    /// Halts the engine in `slot`, if any, and releases its lease in the
    /// store, so another engine can take over at once.
    fn retire(&self, slot: &mut Option<Engine>) {
        let Some(mut engine) = slot.take() else {
            return;
        };
        engine.halt();
        *lock(&self.view) = None;
        // A release names the holder by its owner and pid alone; the
        // interval is not read.
        let holder = LeaseHolder::new(ENGINE_OWNER, std::process::id(), 0.0);
        let root = AccountsRoot::new(self.cfg.root.clone());
        if let Err(error) = Lease::new(&root).release(&holder) {
            tracing::warn!(%error, "the auto-switch engine could not release its lease");
        }
    }

    /// Ticks `engine` once over `store` with `section`'s settings, and
    /// returns its report with whether the engine was halted before it.
    fn run_tick(
        &self,
        store: &Store,
        engine: &mut Engine,
        section: &AutoSwitchSection,
    ) -> Result<(TickReport, bool), AccountsError> {
        let settings = section.to_engine();
        let switcher = WhileEnabled {
            settings: store.root().settings(),
            guard: &self.switch_guard,
        };
        let was_halted = engine.halted();
        let report = self.with_context(store, |context, usage| {
            let collector = TransportCollector(usage);
            engine.tick(&TickInputs {
                context,
                collector: &collector,
                processes: &*self.processes,
                switcher: &switcher,
                claude_config_dir_set: std::env::var_os(CONFIG_DIR_VAR)
                    .is_some_and(|dir| !dir.is_empty()),
                settings: &settings,
                auto_switch_enabled: true,
            })
        })?;
        Ok((report, was_halted))
    }

    /// The shared client, built now when no call has built it yet. A failed
    /// build is tried again by the next call.
    fn client(&self) -> Result<&Client, AccountsError> {
        shared_client(&self.client, self.cfg.base_url.as_deref())
            .map_err(|_| AccountsError::new("the HTTP client could not be built"))
    }

    /// Claude Code's files: its home is the config's `agent_home`, else
    /// `CLAUDE_CONFIG_DIR`, else `<user home>/.claude`; its global config is
    /// `.claude.json` in the first two, else `<user home>/.claude.json`. Its
    /// live login is where Claude Code keeps it on this platform
    /// ([`with_live_store`]), except under the config's `agent_home`, a
    /// test's own home, where it stays in the live credential file, so no
    /// test reaches the user's Keychain.
    fn claude_paths(&self) -> Result<ClaudePaths, AccountsError> {
        let missing = || AccountsError::new("Claude Code's home folder could not be found");
        let moved = self.cfg.agent_home.clone().or_else(|| {
            std::env::var_os(CONFIG_DIR_VAR)
                .filter(|dir| !dir.is_empty())
                .map(PathBuf::from)
        });
        let paths = if let Some(home) = &moved {
            ClaudePaths::new(home.clone(), home.join(".claude.json"))
        } else {
            let home = own_claude_home().ok_or_else(missing)?;
            let user = UserDirs::new().ok_or_else(missing)?;
            ClaudePaths::new(home, user.home_dir().join(".claude.json"))
        };
        if self.cfg.agent_home.is_some() {
            return Ok(paths);
        }
        let config_dir = std::env::var_os(CONFIG_DIR_VAR);
        Ok(with_live_store(paths, config_dir.as_deref()))
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
        let store = self.open_store()?;
        let mut snapshot = read_store(&store)?.snapshot;
        let auto = self.auto_snapshot(store.root(), &snapshot.accounts);
        snapshot.auto = auto;
        Ok(snapshot)
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
            return Ok(PollReport {
                snapshot: AccountsSnapshot {
                    accounts: Vec::new(),
                    auto: self.auto_snapshot(store.root(), &[]),
                },
                next_poll_in: None,
            });
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
        let mut read = read_store(&store)?;
        let auto = self.auto_snapshot(store.root(), &read.snapshot.accounts);
        read.snapshot.auto = auto;
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

    /// After the wait, halts the auto-switch engine and releases its lease.
    /// A tick takes its drain guard before the engine, so once the wait
    /// returns idle no tick holds the engine; one still running past the
    /// bound keeps it, and its lease goes stale on its own.
    fn begin_shutdown(&self, bound: Duration) -> bool {
        let _held = self.drain.start();
        let idle = self.drain.wait_idle(bound);
        match self.engine.try_lock() {
            Ok(mut slot) => self.retire(&mut slot),
            Err(TryLockError::Poisoned(poisoned)) => self.retire(&mut poisoned.into_inner()),
            Err(TryLockError::WouldBlock) => tracing::warn!(
                "an auto-switch tick still runs at shutdown; its lease is left to go stale"
            ),
        }
        idle
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

    /// Reads the settings, changes the fields given and, when that changed
    /// anything, writes them back under the switch guard and a drain guard.
    /// Unsupported where the account locks are, before anything is read.
    fn set_auto_switch(
        &self,
        enabled: Option<bool>,
        threshold_pct: Option<f64>,
    ) -> Result<bool, AccountsFailure> {
        if !LOCKS_SUPPORTED {
            return Err(AccountsFailure::unsupported());
        }
        let store = self.store().map_err(|error| store_failure(&error))?;
        let root = store.root();
        // Held across the read and the write, so the other settings write
        // beside this one cannot read the file before this write and put its
        // old fields back. An engine switch in flight finishes first; one
        // started after this write reads what it wrote.
        let switching = lock(&self.switch_guard);
        let mut settings =
            Settings::read(&root.settings()).map_err(|error| settings_failure(&error))?;
        let before = settings.clone();
        if let Some(enabled) = enabled {
            settings.autoswitch.enabled = enabled;
        }
        if let Some(threshold) = threshold_pct {
            settings.autoswitch.threshold = threshold;
        }
        if settings == before {
            return Ok(false);
        }
        let Ok(_work) = self.drain.begin() else {
            return Err(draining());
        };
        Settings::write(root, &settings).map_err(|error| settings_failure(&error))?;
        // A tick holds the engine while its switch takes the guard, so the
        // guard is let go before the engine is taken.
        drop(switching);
        // Turned off: the engine is retired now, its halt cleared and its
        // lease released, so turning it on again starts a fresh one.
        if !settings.autoswitch.enabled {
            self.retire(&mut lock(&self.engine));
        }
        Ok(true)
    }

    /// Reads the settings, changes the rules given and, when that changed
    /// anything, writes them back under the switch guard and a drain guard.
    /// Unsupported where the account locks are, before anything is read. The
    /// engine is not retired: its next tick reads the rules this wrote.
    fn set_auto_switch_rules(&self, rules: AutoSwitchRules) -> Result<bool, AccountsFailure> {
        if !LOCKS_SUPPORTED {
            return Err(AccountsFailure::unsupported());
        }
        let store = self.store().map_err(|error| store_failure(&error))?;
        let root = store.root();
        // Held across the read and the write, so the other settings write
        // beside this one cannot read the file before this write and put its
        // old fields back. An engine switch in flight finishes first; one
        // started after this write reads what it wrote.
        let switching = lock(&self.switch_guard);
        let mut settings =
            Settings::read(&root.settings()).map_err(|error| settings_failure(&error))?;
        let before = settings.clone();
        if let Some(hysteresis_pct) = rules.hysteresis_pct {
            settings.autoswitch.hysteresis_pct = hysteresis_pct;
        }
        if let Some(cooldown_secs) = rules.cooldown_secs {
            settings.autoswitch.cooldown_seconds = Duration::from_secs(cooldown_secs).as_secs_f64();
        }
        if let Some(strategy) = rules.strategy {
            settings.autoswitch.strategy = strategy_name(strategy)?;
        }
        if let Some(minutes) = rules.rate_window_minutes {
            settings.fleet.rate_window = forecast_window(minutes)?;
        }
        if settings == before {
            return Ok(false);
        }
        let Ok(_work) = self.drain.begin() else {
            return Err(draining());
        };
        Settings::write(root, &settings).map_err(|error| settings_failure(&error))?;
        drop(switching);
        Ok(true)
    }

    /// Reads the settings each call, before the store. Off, unsupported where
    /// the account locks are, or once the drain has begun: no tick and no
    /// lease claim, the store is not opened, and an engine left from when it
    /// was on is halted and its lease released. On: the engine is made on
    /// first use, with a warm-up from now, and ticked under a drain guard
    /// taken before the engine; a tick that finds the drain begun when it
    /// ends retires the engine before it returns.
    fn engine_tick(&self) -> Result<EngineTick, AccountsError> {
        if !LOCKS_SUPPORTED {
            return Ok(EngineTick::Off);
        }
        let Ok(_work) = self.drain.begin() else {
            self.retire(&mut lock(&self.engine));
            return Ok(EngineTick::Off);
        };
        let settings_path = AccountsRoot::new(self.cfg.root.clone()).settings();
        let section = Settings::read(&settings_path)
            .map_err(|error| {
                AccountsError::new(format!("the account settings could not be read: {error}"))
            })?
            .autoswitch;
        let mut slot = lock(&self.engine);
        if !section.enabled || self.drain.is_draining() {
            self.retire(&mut slot);
            return Ok(EngineTick::Off);
        }
        let store = self.open_store()?;
        let engine = slot.get_or_insert_with(|| {
            Engine::new(
                ENGINE_OWNER,
                std::process::id(),
                Some(jiff::Timestamp::now()),
            )
        });
        let (report, was_halted) = self.run_tick(&store, engine, &section)?;
        let now = jiff::Timestamp::now();
        let delay_s = next_delay(
            &report,
            section.interval_seconds,
            now,
            active_next_poll_at(&store),
            f64::from(now.subsec_nanosecond().unsigned_abs()) / 1e9,
        );
        let delay = Duration::try_from_secs_f64(delay_s)
            .or_else(|_| Duration::try_from_secs_f64(section.interval_seconds))
            .unwrap_or(Duration::from_secs(60));
        let halted = report.outcome == TickOutcome::Halted;
        let mut events: Vec<AccountEventRow> = report
            .events
            .iter()
            .filter_map(|event| event_row(&event.body))
            .collect();
        if halted && !was_halted {
            events.push(AccountEventRow::Halted {
                message: HALTED.to_owned(),
            });
        }
        let paused = match &report.outcome {
            TickOutcome::Paused { reason } => Some(reason.clone()),
            TickOutcome::Halted => Some(HALTED.to_owned()),
            _ => None,
        };
        {
            let mut held = lock(&self.view);
            let view = held.get_or_insert_with(EngineView::default);
            view.paused_reason.clone_from(&paused);
            if let Some(decision) = decision(&report) {
                view.last_decision = Some(decision.to_owned());
            }
            view.warm_up_done = !engine.warming();
        }
        // A shutdown that found this tick running could not release the
        // lease; the tick does it now.
        if self.drain.is_draining() {
            self.retire(&mut slot);
        }
        Ok(EngineTick::Ran {
            events,
            delay,
            paused,
            halted: halted.then(|| HALTED.to_owned()),
        })
    }
}

/// `mutex`'s guard, poisoned or not.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The crate's live switch, made only while `autoswitch.enabled` reads on
/// in the settings file at `settings`, re-read under `guard`, which a
/// settings write also holds; a settings file that cannot be read reads off.
struct WhileEnabled<'a> {
    settings: PathBuf,
    guard: &'a Mutex<()>,
}

impl WhileEnabled<'_> {
    fn enabled(&self) -> bool {
        Settings::read(&self.settings).is_ok_and(|settings| settings.autoswitch.enabled)
    }
}

impl SwitchFn for WhileEnabled<'_> {
    fn switch(
        &self,
        context: &RefreshContext<'_>,
        number: u32,
        precondition: Option<SwitchPrecondition>,
    ) -> Result<claude_accounts::engine::SwitchReport, SwitchFailure> {
        let _guard = lock(self.guard);
        if !self.enabled() {
            return Err(SwitchFailure {
                kind: SwitchErrorKind::AutoSwitchOff,
                login: None,
            });
        }
        LiveSwitch.switch(context, number, precondition)
    }

    fn write_while_on(&self, write: &mut dyn FnMut()) -> WhileOn {
        let _guard = match self.guard.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return WhileOn::Busy,
        };
        if !self.enabled() {
            return WhileOn::Off;
        }
        write();
        WhileOn::Ran
    }
}

/// An engine event a client may tell the user of, as a row; `None` for the
/// rest, and for a switch to a login no stored account has.
fn event_row(body: &EventBody) -> Option<AccountEventRow> {
    match body {
        EventBody::Switch {
            trigger, from, to, ..
        } => Some(AccountEventRow::Switched {
            from: from.as_ref().and_then(|from| from.number),
            to: to.number?,
            trigger: trigger.as_str().to_owned(),
        }),
        EventBody::AllExhausted { earliest_reset_at } => Some(AccountEventRow::AllExhausted {
            earliest_reset_at: earliest_reset_at.and_then(|at| chrono_time(at.as_nanosecond())),
        }),
        EventBody::Quarantined { number, .. } => {
            Some(AccountEventRow::Quarantined { number: *number })
        }
        EventBody::Unquarantined { number, .. } => number
            .parse()
            .ok()
            .map(|number| AccountEventRow::Unquarantined { number }),
        _ => None,
    }
}

/// The decision a tick came to, as the snapshot names it: its outcome, or
/// for no action the reason the tick gave; `None` for a pause or a halt,
/// which keep the last decision.
fn decision(report: &TickReport) -> Option<&'static str> {
    match report.outcome {
        TickOutcome::Switched => Some("switched"),
        TickOutcome::Error => Some("error"),
        TickOutcome::Blocked => Some("blocked"),
        TickOutcome::NoAction => Some(
            report
                .events
                .iter()
                .rev()
                .find_map(|event| match event.body {
                    EventBody::NoSwitch { reason, .. } => Some(reason),
                    _ => None,
                })
                .unwrap_or("no-action"),
        ),
        _ => None,
    }
}

/// The usage table's planned next poll of the live login's account; `None`
/// when there is no active account, no plan, or the store cannot say.
fn active_next_poll_at(store: &Store) -> Option<jiff::Timestamp> {
    let sequence = store.read_sequence().ok()??;
    let active = sequence.active()?;
    let record = sequence.account(active)?;
    let identities = BTreeMap::from([(active, Identity::of(&record))]);
    UsageTable::entries(store.root(), &identities)
        .remove(&active)?
        .next_poll_at
}

/// Whether this platform's account locks work: off 64-bit Windows every
/// lock is refused, so auto-switch cannot run there.
const LOCKS_SUPPORTED: bool = cfg!(all(windows, target_pointer_width = "64"));

/// The auto-switch state the settings in `root` give while no engine runs:
/// their switch, threshold and rules, the defaults when the file or its
/// section is missing. A settings file that cannot be read gives the
/// defaults, paused with the reason; a cooldown that is no duration reads 0,
/// paused with [`COOLDOWN_UNREADABLE`]. `None` where the account locks are
/// unsupported.
fn auto_state(root: &AccountsRoot) -> Option<AutoState> {
    if !LOCKS_SUPPORTED {
        return None;
    }
    let (section, rate_window, paused_reason) = match Settings::read(&root.settings()) {
        Ok(settings) => (settings.autoswitch, settings.fleet.rate_window, None),
        Err(error) => (
            AutoSwitchSection::default(),
            RateWindow::default(),
            Some(format!(
                "the auto-switch settings could not be read: {error}"
            )),
        ),
    };
    let cooldown = Duration::try_from_secs_f64(section.cooldown_seconds);
    let paused_reason =
        paused_reason.or_else(|| cooldown.is_err().then(|| COOLDOWN_UNREADABLE.to_owned()));
    Some(AutoState {
        enabled: section.enabled,
        threshold_pct: section.threshold,
        hysteresis_pct: section.hysteresis_pct,
        cooldown_secs: cooldown.map_or(0, |cooldown| cooldown.as_secs()),
        strategy: protocol_strategy(section.strategy),
        rate_window_minutes: Some(rate_window.minutes()),
        paused_reason,
        warm_up_done: false,
        last_decision: None,
        fleet: None,
    })
}

/// The counted accounts' readings: each account not held out of switching,
/// whose usage read carries a fetch time and whose status leaves it in the
/// forecast.
fn fleet_readings(rows: &[AccountRow]) -> Vec<AccountReading> {
    rows.iter()
        .filter(|row| !row.disabled && in_forecast(row.usage.status))
        .filter_map(|row| {
            let five_hour = row.usage.five_hour.as_ref();
            Some(AccountReading {
                account: row.number,
                fetched_at: jiff_time(row.usage.fetched_at?)?,
                five_hour: five_hour.and_then(finite_window),
                seven_day: row.usage.seven_day.as_ref().and_then(finite_window),
                unstarted_five_hour: five_hour
                    .filter(|window| window.pct.is_finite() && window.resets_at.is_none())
                    .map(|window| window.pct),
            })
        })
        .collect()
}

/// Whether a usage status leaves the account in the forecast: an ok read, or
/// a transient failure, whose last good reading still stands as the engine
/// trusts it. Every other status says the account has no reading to forecast
/// from.
fn in_forecast(status: UsageStatus) -> bool {
    matches!(status, UsageStatus::Ok | UsageStatus::Unavailable)
}

/// `window` as a forecast reading: its percentage when finite, and its reset
/// when the reading carried one that parses.
fn finite_window(window: &UsageWindowRow) -> Option<WindowReading> {
    Some(WindowReading {
        pct: window.pct.is_finite().then_some(window.pct)?,
        resets_at: jiff_time(window.resets_at?)?,
    })
}

/// A verdict as the seam's row.
fn verdict_row(verdict: &WindowVerdict) -> FleetRow {
    FleetRow {
        window: fleet_window_row(verdict.window),
        members: u32::try_from(verdict.members).unwrap_or(u32::MAX),
        accounts: u32::try_from(verdict.accounts).unwrap_or(u32::MAX),
        remaining: verdict.remaining,
        rate: verdict.rate,
        rate_window: fleet_window_row(verdict.rate_window),
        runs_dry: verdict.runs_dry,
        blocked_at: verdict
            .blocked_at
            .and_then(|at| chrono_time(at.as_nanosecond())),
        resumes_at: verdict
            .resumes_at
            .and_then(|at| chrono_time(at.as_nanosecond())),
        first_reset: verdict
            .first_reset
            .and_then(|at| chrono_time(at.as_nanosecond())),
    }
}

/// The seam's name for `window`.
fn fleet_window_row(window: FleetWindow) -> FleetWindowRow {
    match window {
        FleetWindow::FiveHour => FleetWindowRow::FiveHour,
        // The crate's only other window is the 7-day one.
        _ => FleetWindowRow::SevenDay,
    }
}

/// `at` as a jiff timestamp; `None` when chrono holds a time jiff cannot.
fn jiff_time(at: DateTime<Utc>) -> Option<jiff::Timestamp> {
    jiff::Timestamp::from_nanosecond(i128::from(at.timestamp_nanos_opt()?)).ok()
}

/// The engine's spelling of the strategy `name`.
fn protocol_strategy(name: StrategyName) -> AutoSwitchStrategy {
    match name {
        StrategyName::Best => AutoSwitchStrategy::Best,
        StrategyName::ConsumeFirst => AutoSwitchStrategy::ConsumeFirst,
    }
}

/// The settings file's spelling of `strategy`; a strategy this build does not
/// know is refused.
fn strategy_name(strategy: AutoSwitchStrategy) -> Result<StrategyName, AccountsFailure> {
    match strategy {
        AutoSwitchStrategy::Best => Ok(StrategyName::Best),
        AutoSwitchStrategy::ConsumeFirst => Ok(StrategyName::ConsumeFirst),
        AutoSwitchStrategy::Unknown => Err(failure(
            AccountsFailureKind::Refused,
            "strategy_unknown",
            "unknown strategy".to_owned(),
        )),
    }
}

/// The settings file's forecast window `minutes` long; one the file cannot
/// hold is refused.
fn forecast_window(minutes: u32) -> Result<RateWindow, AccountsFailure> {
    RateWindow::from_minutes(minutes).ok_or_else(|| {
        failure(
            AccountsFailureKind::Refused,
            "rate_window_refused",
            "the forecast window is 30, 60, 120 or 180 minutes".to_owned(),
        )
    })
}

/// A settings write's failure: a refused value is
/// [`AccountsFailureKind::Refused`], a lock wait that timed out
/// [`AccountsFailureKind::Busy`], anything else
/// [`AccountsFailureKind::Failed`].
fn settings_failure(error: &SettingsError) -> AccountsFailure {
    let (kind, code) = match error {
        SettingsError::Invalid(_) => (AccountsFailureKind::Refused, "settings_invalid"),
        SettingsError::Lock(LockError::Timeout) => (AccountsFailureKind::Busy, "locks_busy"),
        SettingsError::Lock(_) => (AccountsFailureKind::Failed, "lock_failed"),
        _ => (AccountsFailureKind::Failed, "settings_failed"),
    };
    failure(kind, code, error.to_string())
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
        takes_effect_within: None,
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
        takes_effect_within: outcome.takes_effect_within,
    }
}

/// The `security` tool Claude Code's Keychain item is read and written
/// through on macOS: Apple's own, never one found on `PATH`.
#[cfg(target_os = "macos")]
const SECURITY_TOOL: &str = "/usr/bin/security";

/// Claude Code's Keychain service for its live OAuth login, the default
/// config dir's.
#[cfg(any(target_os = "macos", test))]
const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

/// The Keychain account Claude Code falls back to when it finds no user name.
#[cfg(any(target_os = "macos", test))]
const KEYCHAIN_FALLBACK_ACCOUNT: &str = "claude-code-user";

/// `paths` with the live login where Claude Code keeps it on this platform:
/// on macOS the Keychain item of the service the config dir picks
/// (`config_dir`, `CLAUDE_CONFIG_DIR` as the process has it) and
/// of the user's name, read and written through `/usr/bin/security`.
#[cfg(target_os = "macos")]
fn with_live_store(paths: ClaudePaths, config_dir: Option<&std::ffi::OsStr>) -> ClaudePaths {
    let secure = std::env::var_os(SECURESTORAGE_DIR_VAR);
    let user = std::env::var_os("USER");
    paths.with_keychain(
        keychain_service(secure.as_deref(), config_dir),
        keychain_account(user.as_deref(), claude_accounts::os_user_name),
        SECURITY_TOOL,
    )
}

/// `paths` with the live login where Claude Code keeps it on this platform:
/// off macOS the live credential file, as `paths` already says.
#[cfg(not(target_os = "macos"))]
fn with_live_store(paths: ClaudePaths, _config_dir: Option<&std::ffi::OsStr>) -> ClaudePaths {
    paths
}

/// The Keychain service Claude Code keeps its live login under, as it picks
/// it: `CLAUDE_SECURESTORAGE_CONFIG_DIR` (`secure`) when it is set at all,
/// else `CLAUDE_CONFIG_DIR` (`config_dir`); an empty or missing one is the
/// default service, and any other is the default service suffixed with `-`
/// and the first 8 hex digits of the SHA-256 of the value as given, in NFC.
#[cfg(any(target_os = "macos", test))]
fn keychain_service(
    secure: Option<&std::ffi::OsStr>,
    config_dir: Option<&std::ffi::OsStr>,
) -> String {
    use sha2::{Digest as _, Sha256};
    use unicode_normalization::UnicodeNormalization as _;

    let dir = match secure {
        Some(secure) => secure,
        None => config_dir.unwrap_or_default(),
    };
    if dir.is_empty() {
        return KEYCHAIN_SERVICE.to_owned();
    }
    let normalized: String = dir.to_string_lossy().nfc().collect();
    let digest = Sha256::digest(normalized.as_bytes());
    let suffix: String = digest
        .iter()
        .take(4)
        .flat_map(|byte| [byte >> 4, byte & 0x0f])
        .map(|digit| char::from_digit(u32::from(digit), 16).unwrap_or('0'))
        .collect();
    format!("{KEYCHAIN_SERVICE}-{suffix}")
}

/// The Keychain account Claude Code keeps its live login under, as it picks
/// it: `USER` (`user`) when it is set and not empty, else the OS user name
/// (`os_user`), else `claude-code-user`.
#[cfg(any(target_os = "macos", test))]
fn keychain_account(
    user: Option<&std::ffi::OsStr>,
    os_user: impl FnOnce() -> Option<String>,
) -> String {
    user.and_then(std::ffi::OsStr::to_str)
        .filter(|user| !user.is_empty())
        .map(str::to_owned)
        .or_else(os_user)
        .unwrap_or_else(|| KEYCHAIN_FALLBACK_ACCOUNT.to_owned())
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

/// The live process table on macOS: one `sysinfo` refresh of the process list
/// per snapshot.
#[cfg(target_os = "macos")]
struct MacProcessTable;

#[cfg(target_os = "macos")]
impl proc_table::ProcessTable for MacProcessTable {
    fn snapshot(&self) -> Vec<proc_table::ProcessInfo> {
        use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System, UpdateKind};

        let refresh = ProcessRefreshKind::new().with_cmd(UpdateKind::Always);
        let mut system = System::new_with_specifics(RefreshKind::new().with_processes(refresh));
        system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);
        system
            .processes()
            .values()
            .map(|process| proc_table::ProcessInfo {
                pid: process.pid().as_u32(),
                parent_pid: process.parent().map_or(0, sysinfo::Pid::as_u32),
                name: process.name().to_string_lossy().into_owned(),
                command_line: mac_command_line(process.cmd()),
                created: mac_started_at(process.start_time()),
            })
            .collect()
    }
}

/// A process's arguments joined with single spaces, `None` when it has none.
#[cfg(target_os = "macos")]
fn mac_command_line(args: &[std::ffi::OsString]) -> Option<String> {
    let joined = args
        .iter()
        .map(|arg| arg.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");
    (!joined.is_empty()).then_some(joined)
}

/// `seconds` past the epoch as a [`jiff::Timestamp`], `None` when no timestamp
/// holds it.
#[cfg(target_os = "macos")]
fn mac_started_at(seconds: u64) -> Option<jiff::Timestamp> {
    let seconds = i64::try_from(seconds).ok()?;
    jiff::Timestamp::from_second(seconds).ok()
}

/// The process table the account engine checks a lease holder against: the
/// live Windows table where the store's locks work, the live macOS one, and an
/// empty table elsewhere, where a crashed holder's lease waits out its
/// heartbeat.
#[must_use]
pub fn process_table() -> Box<dyn proc_table::ProcessTable + Send + Sync> {
    #[cfg(all(windows, target_pointer_width = "64"))]
    {
        Box::new(proc_table::Win32ProcessTable)
    }
    #[cfg(target_os = "macos")]
    {
        Box::new(MacProcessTable)
    }
    #[cfg(not(any(all(windows, target_pointer_width = "64"), target_os = "macos")))]
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
    fn the_keychain_service_name_follows_the_config_dir_rule() {
        use std::ffi::OsStr;

        let work = OsStr::new("/Users/me/.claude-work");
        assert_eq!(
            keychain_service(None, None),
            "Claude Code-credentials",
            "the default config dir has the unsuffixed item"
        );
        assert_eq!(
            keychain_service(None, Some(OsStr::new(""))),
            "Claude Code-credentials",
            "an empty CLAUDE_CONFIG_DIR is no config dir"
        );
        assert_eq!(
            keychain_service(None, Some(work)),
            "Claude Code-credentials-1e91dd84",
            "a config dir suffixes the first 8 hex digits of its SHA-256"
        );
        assert_eq!(
            keychain_service(Some(work), Some(OsStr::new("/Users/me/.claude-other"))),
            "Claude Code-credentials-1e91dd84",
            "CLAUDE_SECURESTORAGE_CONFIG_DIR wins over CLAUDE_CONFIG_DIR"
        );
        assert_eq!(
            keychain_service(Some(OsStr::new("")), Some(work)),
            "Claude Code-credentials",
            "an empty CLAUDE_SECURESTORAGE_CONFIG_DIR means the unsuffixed item"
        );
        assert_eq!(
            keychain_service(None, Some(OsStr::new("/Users/me/cafe\u{301}"))),
            keychain_service(None, Some(OsStr::new("/Users/me/caf\u{e9}"))),
            "the config dir is hashed in NFC"
        );
        assert_eq!(keychain_account(Some(OsStr::new("me")), || None), "me");
        assert_eq!(
            keychain_account(Some(OsStr::new("")), || Some("os-me".to_owned())),
            "os-me",
            "with no USER, the OS user name"
        );
        assert_eq!(
            keychain_account(None, || None),
            "claude-code-user",
            "with neither, Claude Code's fallback name"
        );
    }

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

    /// The engine's lifecycle, which only the service's own engine mutex and
    /// drain can stage; a real store, so gated as the store-writing tests are.
    #[cfg(all(windows, target_pointer_width = "64"))]
    #[expect(
        clippy::expect_used,
        reason = "a test fails with the message of the precondition it lost"
    )]
    mod engine_tick {
        use super::super::*;
        use std::path::Path;

        /// A private scratch folder for `test`, emptied first.
        fn scratch(test: &str) -> PathBuf {
            let dir = std::env::temp_dir()
                .join(format!("agent-claude-engine-{test}-{}", std::process::id()));
            if dir.exists() {
                std::fs::remove_dir_all(&dir).expect("the test's old folder can be removed");
            }
            std::fs::create_dir_all(&dir).expect("the test's folder can be made");
            let whoami = std::process::Command::new("whoami")
                .output()
                .expect("whoami can be started");
            let user = String::from_utf8(whoami.stdout).expect("whoami prints text");
            let output = std::process::Command::new("icacls")
                .arg(&dir)
                .args([
                    "/inheritance:r",
                    "/grant:r",
                    &format!("{}:(OI)(CI)F", user.trim()),
                ])
                .output()
                .expect("icacls can be started");
            assert!(output.status.success(), "icacls failed");
            dir
        }

        /// The service over a store at `dir/accounts` holding accounts 1
        /// (active) and 2, with no credential, Claude Code's home at
        /// `dir/claude`, and auto-switch turned on.
        fn service_on(dir: &Path) -> Arc<ClaudeAccounts> {
            let root = dir.join("accounts");
            std::fs::create_dir_all(root.join("cache")).expect("the store folders can be made");
            let record = |number: u32| {
                serde_json::json!({
                    "email": format!("user{number}@example.com"),
                    "uuid": format!("00000000-0000-4000-8000-00000000000{number}"),
                    "added": "2026-09-01T10:00:00Z"
                })
            };
            let accounts = serde_json::json!({
                "schemaVersion": 1,
                "order": [1, 2],
                "active": 1,
                "accounts": {"1": record(1), "2": record(2)}
            });
            std::fs::write(root.join("accounts.json"), accounts.to_string())
                .expect("accounts.json can be written");
            let mut cfg = AccountsConfig::new(root);
            cfg.agent_home = Some(dir.join("claude"));
            cfg.base_url = Some("http://127.0.0.1:9".to_owned());
            let service = Arc::new(ClaudeAccounts::new(cfg));
            service
                .set_auto_switch(Some(true), None)
                .expect("auto-switch is turned on");
            service
        }

        fn lease(dir: &Path) -> PathBuf {
            AccountsRoot::new(dir.join("accounts")).engine_lease()
        }

        /// Lets a thread just started reach the engine mutex the test holds;
        /// the assertions after it hold whatever the interleaving.
        fn let_it_reach_the_engine() {
            std::thread::sleep(Duration::from_millis(300));
        }

        #[test]
        fn turning_auto_switch_off_and_on_clears_a_halt() {
            let dir = scratch("off-on-clears-a-halt");
            let service = service_on(&dir);
            let ticked = service.engine_tick().expect("the tick answers");
            assert!(matches!(ticked, EngineTick::Ran { .. }), "{ticked:?}");
            lock(&service.engine)
                .as_mut()
                .expect("the tick made the engine")
                .halt();

            service
                .set_auto_switch(Some(false), None)
                .expect("auto-switch is turned off");
            assert!(lock(&service.engine).is_none(), "the write retired it");
            assert!(!lease(&dir).exists(), "the write released the lease");
            service
                .set_auto_switch(Some(true), None)
                .expect("auto-switch is turned on again");
            let ticked = service.engine_tick().expect("the tick answers");

            assert!(
                matches!(ticked, EngineTick::Ran { halted: None, .. }),
                "a fresh engine ticks: {ticked:?}"
            );
        }

        #[test]
        fn no_engine_is_created_once_the_drain_has_started() {
            let dir = scratch("no-engine-once-draining");
            let service = service_on(&dir);
            let held = lock(&service.engine);
            let ticker = Arc::clone(&service);
            let tick = std::thread::spawn(move || ticker.engine_tick());
            let_it_reach_the_engine();
            let _ = service.drain.start();
            drop(held);

            let ticked = tick
                .join()
                .expect("the tick did not panic")
                .expect("the tick answers");

            assert_eq!(ticked, EngineTick::Off);
            assert!(lock(&service.engine).is_none(), "no engine was made");
            assert!(!lease(&dir).exists(), "no lease was claimed");
        }

        #[test]
        fn a_tick_finishing_after_shutdown_releases_the_lease() {
            let dir = scratch("tick-after-shutdown-releases");
            let service = service_on(&dir);
            let ticked = service.engine_tick().expect("the tick answers");
            assert!(matches!(ticked, EngineTick::Ran { .. }), "{ticked:?}");
            assert!(lease(&dir).exists(), "the tick claimed the lease");
            let held = lock(&service.engine);
            let ticker = Arc::clone(&service);
            let tick = std::thread::spawn(move || ticker.engine_tick());
            let_it_reach_the_engine();

            let _ = service.begin_shutdown(Duration::ZERO);
            assert!(
                lease(&dir).exists(),
                "the shutdown could not take the engine from the tick"
            );
            drop(held);
            let _ = tick
                .join()
                .expect("the tick did not panic")
                .expect("the tick answers");

            assert!(!lease(&dir).exists(), "the tick released the lease");
            assert!(lock(&service.engine).is_none(), "the tick retired it");
        }
    }

    #[test]
    fn retry_after_rejects_garbage_and_non_finite() {
        for text in ["NaN", "inf", "-5", "soon", ""] {
            assert_eq!(retry_after_secs(text, example_now()), None, "{text:?}");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[expect(
        clippy::expect_used,
        reason = "a snapshot without this process must fail loudly"
    )]
    fn the_macos_table_lists_this_process() {
        let snapshot = process_table().snapshot();
        let me = snapshot
            .iter()
            .find(|process| process.pid == std::process::id())
            .expect("the snapshot lists this process");
        assert_eq!(
            me.parent_pid,
            std::os::unix::process::parent_id(),
            "this process's parent"
        );
        let created = me.created.expect("this process's start time is known");
        assert!(
            created <= jiff::Timestamp::now(),
            "a start time is not after now"
        );
    }
}
