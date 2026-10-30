//! The summarizer service the hub holds: it reads the settings from
//! `state.json`, keeps the key states, the per-session queue, the cache and
//! the day's spend, makes each call through a [`Caller`], and broadcasts the
//! status whenever any of it changes. A request that makes no call gets the
//! template line.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow, bail};
use chrono::{Local, NaiveDate};
use protocol::{
    DaemonMessage, KeyState, SummarizerKey, SummarizerKeySource, SummarizerProvider,
    SummarizerSettings, SummarySource, SummaryTestResult,
};
use tokio::sync::{broadcast, watch};
use tracing::{error, info, warn};

use crate::env_secrets::VaultUnreachable;
use crate::server::StateEvent;
use crate::state::AppState;
use crate::summarizer::cache::{Cache, CacheKey};
use crate::summarizer::cli::{CliConfig, CliError, CliRunner};
use crate::summarizer::http::{CallError, HttpClient, HttpConfig};
use crate::summarizer::keys::{self, KeySource, Rejections};
use crate::summarizer::ledger::{self, Ledger};
use crate::summarizer::policy::{self, Line, Plan, Provider};
use crate::summarizer::prompt::{self, Trigger};
use crate::summarizer::providers;
use crate::summarizer::queue::Queue;
use crate::summarizer::reply::Summary;
use crate::user_env::Secret;

/// The report `TestSummary` summarizes.
const TEST_SAMPLE: &str =
    "I ran the whole test suite and every test passes. The branch is ready for review.";

/// A provider's answer and what it cost.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Called {
    pub(crate) summary: Summary,
    /// US dollars.
    pub(crate) cost_usd: f64,
}

/// Why a call gave no summary, and what it still cost.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CallFailure {
    /// The failure, for the log and a test's `Error`; never a key.
    pub(crate) message: String,
    /// US dollars billed for the failed call.
    pub(crate) cost_usd: f64,
    /// The provider refused the key.
    pub(crate) rejected: bool,
}

/// A call in flight.
pub(crate) type CallFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Called, CallFailure>> + Send + 'a>>;

/// Makes one provider call: the seam a test replaces with a fake provider.
pub(crate) trait Caller: Send + Sync {
    /// Summarize `prompt` with `provider`, sending `key` when it takes one.
    fn call<'a>(
        &'a self,
        provider: &'a Provider,
        key: Option<&'a Secret>,
        prompt: &'a str,
    ) -> CallFuture<'a>;
}

/// The real providers: an agent's CLI run in the summarizer folder, and the
/// HTTP table.
pub(crate) struct LiveCaller {
    http: HttpClient,
    cli: CliRunner,
    folder: PathBuf,
}

impl LiveCaller {
    /// The caller over the summarizer folder, recording refused keys in
    /// `rejections`.
    ///
    /// # Errors
    ///
    /// The summarizer folder cannot be made, or the HTTP client's TLS setup
    /// failed.
    pub(crate) fn start(rejections: Arc<Rejections>) -> anyhow::Result<Self> {
        let folder = crate::paths::resolve_summarizer_dir()?;
        let http = HttpClient::new(HttpConfig::default(), rejections)
            .context("starting the summarizer's HTTP client")?;
        Ok(Self {
            http,
            cli: CliRunner::new(CliConfig::default()),
            folder,
        })
    }
}

impl Caller for LiveCaller {
    fn call<'a>(
        &'a self,
        provider: &'a Provider,
        key: Option<&'a Secret>,
        prompt: &'a str,
    ) -> CallFuture<'a> {
        Box::pin(async move {
            match provider {
                Provider::Cli(plugin) => self
                    .cli
                    .summarize(*plugin, &self.folder, prompt)
                    .await
                    .map(|reply| Called {
                        summary: reply.summary,
                        cost_usd: reply.cost_usd,
                    })
                    .map_err(|err| cli_failure(&err)),
                Provider::Http(row) => {
                    let Some(key) = key else {
                        return Err(CallFailure {
                            message: format!("the {} key is missing", row.id),
                            cost_usd: 0.0,
                            rejected: false,
                        });
                    };
                    self.http
                        .summarize(row, key, prompt)
                        .await
                        .map(|reply| Called {
                            summary: reply.summary,
                            cost_usd: reply.cost_usd,
                        })
                        .map_err(|err| http_failure(&err))
                }
            }
        })
    }
}

fn cli_failure(err: &CliError) -> CallFailure {
    let cost_usd = match err {
        CliError::Reply { cost_usd, .. } => *cost_usd,
        _ => 0.0,
    };
    CallFailure {
        message: err.to_string(),
        cost_usd,
        rejected: false,
    }
}

fn http_failure(err: &CallError) -> CallFailure {
    let cost_usd = match err {
        CallError::MaxTokens { cost_usd } | CallError::Reply { cost_usd, .. } => *cost_usd,
        _ => 0.0,
    };
    CallFailure {
        message: err.to_string(),
        cost_usd,
        rejected: matches!(err, CallError::Rejected),
    }
}

/// One summary request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Request {
    pub(crate) session_id: String,
    pub(crate) trigger: Trigger,
    /// The text [`prompt::shape`] gave.
    pub(crate) input: String,
}

/// How a request ended.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Outcome {
    /// A line, from the model or the template, and what it cost.
    Line { line: Line, cost_usd: f64 },
    /// A newer request for the session replaced this one before it ran.
    Replaced,
    /// No client is connected, so nothing was made.
    Idle,
    /// A working refresh that made no line where a stop request gets the
    /// template: summaries off, past half the cap, no provider, a rejected
    /// or missing key, or a failed call.
    Skipped,
}

/// A test summary's answer.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TestReply {
    pub(crate) result: SummaryTestResult,
    pub(crate) latency_ms: u64,
    pub(crate) cost_usd: f64,
}

/// What [`Summarizer::new`] is built from.
pub(crate) struct SummarizerParts {
    pub(crate) state: Arc<AppState>,
    /// The spend ledger's file.
    pub(crate) spend_file: PathBuf,
    /// The provider calls; `None` when the summarizer failed to start, so
    /// every request gets the template.
    pub(crate) caller: Option<Arc<dyn Caller>>,
    /// The refused keys, shared with the HTTP client.
    pub(crate) rejections: Arc<Rejections>,
    /// How many clients are connected.
    pub(crate) client_count: watch::Receiver<usize>,
    /// Where the status is broadcast.
    pub(crate) events: broadcast::Sender<StateEvent>,
}

/// The status as [`DaemonMessage::SummarizerStatus`] carries it.
struct Snapshot {
    settings: SummarizerSettings,
    providers: Vec<SummarizerProvider>,
    keys: Vec<SummarizerKey>,
    spend_today_usd: f64,
    cap_reached: bool,
}

/// Where a provider's key stands for one call.
enum KeyLookup {
    /// The provider takes no key.
    NotNeeded,
    Found(Secret),
    Missing,
    /// Windows Credential Manager is out of reach of this daemon.
    Unreachable,
}

/// The summarizer service.
pub(crate) struct Summarizer {
    state: Arc<AppState>,
    caller: Option<Arc<dyn Caller>>,
    rejections: Arc<Rejections>,
    client_count: watch::Receiver<usize>,
    events: broadcast::Sender<StateEvent>,
    ledger: Mutex<Ledger>,
    /// Held while the ledger is changed and written, so the writes land in
    /// order.
    ledger_writes: tokio::sync::Mutex<()>,
    cache: Mutex<Cache>,
    queue: Queue,
    /// Each key-taking provider's key state, as last checked.
    keys: Mutex<KeyStates>,
    /// The generation the latest key check took; each check takes the next.
    key_generation: AtomicU64,
    /// Held while a status is read and sent, on connect and on a broadcast,
    /// so a connecting client never ends on an older status than the last
    /// broadcast.
    status_order: Mutex<()>,
}

/// The key states and the generation of the check that wrote them.
#[derive(Debug)]
struct KeyStates {
    generation: u64,
    keys: Vec<SummarizerKey>,
}

/// Every key-taking provider's key state from `source`.
fn check_keys(source: Option<&KeySource>, rejections: &Rejections) -> Vec<SummarizerKey> {
    providers::TABLE
        .iter()
        .map(|row| SummarizerKey {
            provider: row.id.to_owned(),
            state: keys::state(row.id, source, rejections),
        })
        .collect()
}

/// The current local day.
fn today() -> NaiveDate {
    Local::now().date_naive()
}

impl Summarizer {
    /// The service over `parts`. Every key state reads missing until
    /// [`Self::refresh_keys`] first runs, since the key store blocks.
    pub(crate) fn new(parts: SummarizerParts) -> Self {
        let keys = providers::TABLE
            .iter()
            .map(|row| SummarizerKey {
                provider: row.id.to_owned(),
                state: KeyState::Missing,
            })
            .collect();
        Self {
            state: parts.state,
            caller: parts.caller,
            rejections: parts.rejections,
            client_count: parts.client_count,
            events: parts.events,
            ledger: Mutex::new(Ledger::load(parts.spend_file, today())),
            ledger_writes: tokio::sync::Mutex::new(()),
            cache: Mutex::default(),
            queue: Queue::default(),
            keys: Mutex::new(KeyStates {
                generation: 0,
                keys,
            }),
            key_generation: AtomicU64::new(0),
            status_order: Mutex::new(()),
        }
    }

    fn snapshot(&self) -> Snapshot {
        let settings = self.state.summarizer_settings();
        let spend_today_usd = crate::sync::lock(&self.ledger).spent(today());
        Snapshot {
            cap_reached: ledger::cap_reached(spend_today_usd, settings.daily_cap_usd),
            settings,
            providers: policy::offered(),
            keys: crate::sync::lock(&self.keys).keys.clone(),
            spend_today_usd,
        }
    }

    /// The status, as a message.
    pub(crate) fn status_message(&self) -> DaemonMessage {
        let s = self.snapshot();
        DaemonMessage::SummarizerStatus {
            settings: s.settings,
            providers: s.providers,
            keys: s.keys,
            spend_today_usd: s.spend_today_usd,
            cap_reached: s.cap_reached,
        }
    }

    /// Send the status to a connecting client whose state forwarder is
    /// already subscribed. Read and sent under the lock every broadcast
    /// takes, so a broadcast either lands after this send or is already
    /// reflected in it.
    pub(crate) fn send_status(&self, out_tx: &tokio::sync::mpsc::UnboundedSender<DaemonMessage>) {
        let _order = crate::sync::lock(&self.status_order);
        let _ = out_tx.send(self.status_message());
    }

    /// Broadcast the status to every connection.
    fn broadcast_status(&self) {
        let _order = crate::sync::lock(&self.status_order);
        let s = self.snapshot();
        let _ = self.events.send(StateEvent::SummarizerStatus {
            settings: s.settings,
            providers: s.providers,
            keys: s.keys,
            spend_today_usd: s.spend_today_usd,
            cap_reached: s.cap_reached,
        });
    }

    /// The daemon's first key check: broadcast the status when it changed a
    /// key state.
    pub(crate) async fn startup_key_check(&self) {
        if self.refresh_keys().await {
            self.broadcast_status();
        }
    }

    /// Check every key-taking provider's key against the settings' key
    /// source, on a blocking thread. Returns whether a key state changed; a
    /// check that an older one finishes after never overwrites it.
    async fn refresh_keys(&self) -> bool {
        let (generation, source) = self.begin_key_refresh();
        let rejections = Arc::clone(&self.rejections);
        let checked =
            tokio::task::spawn_blocking(move || check_keys(source.as_ref(), &rejections)).await;
        match checked {
            Ok(checked) => self.finish_key_refresh(generation, checked),
            Err(err) => {
                error!(%err, "summarizer: the key check failed; the key states are unchanged");
                false
            }
        }
    }

    /// Take the next key-check generation, and the key source to check.
    fn begin_key_refresh(&self) -> (u64, Option<KeySource>) {
        let generation = self.key_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let source = KeySource::from_wire(&self.state.summarizer_settings().key_source);
        (generation, source)
    }

    /// Keep `checked` unless a newer check already wrote its own. Returns
    /// whether a key state changed.
    fn finish_key_refresh(&self, generation: u64, checked: Vec<SummarizerKey>) -> bool {
        let mut states = crate::sync::lock(&self.keys);
        if generation <= states.generation {
            return false;
        }
        let changed = states.keys != checked;
        *states = KeyStates {
            generation,
            keys: checked,
        };
        changed
    }

    /// Whether the last key check found `provider`'s key refused.
    fn key_rejected(&self, provider: &Provider) -> bool {
        let Provider::Http(row) = provider else {
            return false;
        };
        crate::sync::lock(&self.keys)
            .keys
            .iter()
            .any(|key| key.provider == row.id && key.state == KeyState::Rejected)
    }

    /// Validate and save `settings`, then broadcast the status.
    ///
    /// # Errors
    ///
    /// The provider is not offered, the cap is not a number of dollars of
    /// zero or more, the key source is unknown or names no variable, or
    /// `state.json` cannot be written.
    pub(crate) async fn set_settings(&self, settings: SummarizerSettings) -> anyhow::Result<()> {
        validate(&settings)?;
        self.state.set_summarizer_settings(settings)?;
        info!("summarizer settings changed");
        self.refresh_keys().await;
        self.broadcast_status();
        Ok(())
    }

    /// Save `key` as `provider`'s key, or delete it when `None`, then
    /// broadcast the status. No message or log carries the key.
    ///
    /// # Errors
    ///
    /// The provider is not offered or takes no key, the key is blank or too
    /// long, or the key store refused.
    pub(crate) async fn set_key(&self, provider: &str, key: Option<Secret>) -> anyhow::Result<()> {
        if providers::row(provider).is_none() {
            if policy::resolve(provider).is_some() {
                bail!("The summarizer provider `{provider}` takes no key.");
            }
            bail!("This daemon offers no summarizer provider `{provider}`.");
        }
        let id = provider.to_owned();
        let saving = key.is_some();
        tokio::task::spawn_blocking(move || match key {
            Some(key) => keys::set(&id, &key),
            None => keys::delete(&id),
        })
        .await
        .context("the key store task failed")??;
        info!(provider, saved = saving, "summarizer key changed");
        self.refresh_keys().await;
        self.broadcast_status();
        Ok(())
    }

    /// Summarize `request` once its session's turn comes. A stop request
    /// that makes no call (summaries off, no offered provider, the cap, a
    /// missing key, or a failed start) gets the template line; a failed call
    /// gets it too, its billed cost still counted. A working refresh gets
    /// [`Outcome::Skipped`] in each of those places instead.
    pub(crate) async fn submit(&self, request: Request) -> Outcome {
        self.submit_if(request, || true).await
    }

    /// Summarize `session_id`'s working refresh of `input` once its turn
    /// comes, as [`Self::submit`] does, unless `still_due` answers `false`
    /// when the turn comes: then it gets [`Outcome::Skipped`] with no call.
    pub(crate) async fn submit_refresh(
        &self,
        session_id: &str,
        input: String,
        still_due: impl FnOnce() -> bool,
    ) -> Outcome {
        let request = Request {
            session_id: session_id.to_owned(),
            trigger: Trigger::Working,
            input,
        };
        self.submit_if(request, still_due).await
    }

    /// The body of [`Self::submit`] and [`Self::submit_refresh`]: `proceed`
    /// is asked once the request is admitted, before anything else.
    async fn submit_if(&self, request: Request, proceed: impl FnOnce() -> bool) -> Outcome {
        let Some(_turn) = self.queue.admit(&request.session_id, request.trigger).await else {
            return Outcome::Replaced;
        };
        if !proceed() {
            return Outcome::Skipped;
        }
        let settings = self.state.summarizer_settings();
        let clients = *self.client_count.borrow();
        let spent = self.spent_today();
        let provider = match policy::plan(clients, &settings, request.trigger, spent) {
            Plan::Idle => return Outcome::Idle,
            Plan::Template => return no_call_outcome(&request),
            Plan::Call(provider) => provider,
        };
        let Some(caller) = self.caller.as_deref() else {
            return no_call_outcome(&request);
        };
        if self.key_rejected(&provider) {
            return no_call_outcome(&request);
        }
        let cache_key = CacheKey::new(&provider.id(), request.trigger, &request.input);
        if let Some(summary) = crate::sync::lock(&self.cache).get(&cache_key).cloned() {
            return Outcome::Line {
                line: Line::from_summary(summary),
                cost_usd: 0.0,
            };
        }
        let key = match self.provider_key(&provider, &settings).await {
            KeyLookup::NotNeeded => None,
            KeyLookup::Found(key) => Some(key),
            KeyLookup::Missing | KeyLookup::Unreachable => return no_call_outcome(&request),
        };
        let prompt = prompt::build(request.trigger, &request.input);
        match self.call(caller, &provider, key.as_ref(), &prompt).await.0 {
            Ok(called) => {
                crate::sync::lock(&self.cache).insert(cache_key, called.summary.clone());
                Outcome::Line {
                    line: Line::from_summary(called.summary),
                    cost_usd: called.cost_usd,
                }
            }
            Err(_) => no_call_outcome(&request),
        }
    }

    /// Summarize the fixed sample with the current settings, whether
    /// summaries are on or not, skipping the cache.
    ///
    /// # Errors
    ///
    /// The summarizer failed to start, no provider is set up, the day's cap
    /// is reached, Windows Credential Manager is out of reach
    /// ([`VaultUnreachable`]), or the call failed.
    pub(crate) async fn test(&self) -> anyhow::Result<TestReply> {
        let Some(caller) = self.caller.as_deref() else {
            bail!("The summarizer failed to start; see daemon.log.");
        };
        let settings = self.state.summarizer_settings();
        let Some(provider) = policy::resolve(&settings.provider) else {
            bail!("No summarizer provider is set up.");
        };
        if ledger::cap_reached(self.spent_today(), settings.daily_cap_usd) {
            bail!(
                "Today's summarizer spend has reached its cap of ${:.2}.",
                settings.daily_cap_usd
            );
        }
        let key = match self.provider_key(&provider, &settings).await {
            KeyLookup::NotNeeded => None,
            KeyLookup::Found(key) => Some(key),
            KeyLookup::Missing => {
                bail!("No summarizer provider is set up: the provider's key is missing.")
            }
            KeyLookup::Unreachable => bail!(VaultUnreachable),
        };
        let prompt = prompt::build(Trigger::Stop, &prompt::shape(&[TEST_SAMPLE]));
        let (result, took) = self.call(caller, &provider, key.as_ref(), &prompt).await;
        let called =
            result.map_err(|failure| anyhow!("The test summary failed: {}", failure.message))?;
        let latency_ms = u64::try_from(took.as_millis()).unwrap_or(u64::MAX);
        let line = Line::from_summary(called.summary);
        Ok(TestReply {
            result: SummaryTestResult {
                kind: line.kind,
                headline: line.headline,
                notification: line.notification,
                source: SummarySource::Model,
            },
            latency_ms,
            cost_usd: called.cost_usd,
        })
    }

    fn spent_today(&self) -> f64 {
        crate::sync::lock(&self.ledger).spent(today())
    }

    /// `provider`'s key from the settings' key source, read on a blocking
    /// thread.
    async fn provider_key(&self, provider: &Provider, settings: &SummarizerSettings) -> KeyLookup {
        let Provider::Http(row) = provider else {
            return KeyLookup::NotNeeded;
        };
        let Some(source) = KeySource::from_wire(&settings.key_source) else {
            return KeyLookup::Missing;
        };
        let id = row.id;
        match tokio::task::spawn_blocking(move || keys::get(id, &source)).await {
            Ok(Ok(Some(key))) => KeyLookup::Found(key),
            Ok(Ok(None)) => KeyLookup::Missing,
            Ok(Err(unreachable)) => {
                warn!(provider = id, error = %unreachable, "summarizer: could not read the saved key");
                KeyLookup::Unreachable
            }
            Err(err) => {
                error!(%err, "summarizer: the key lookup failed; treating the key as missing");
                KeyLookup::Missing
            }
        }
    }

    /// Make one call, count what it billed, and on a refused key record it
    /// and check the keys again; broadcast the status when either changed.
    /// Returns the call's result and how long the provider took.
    async fn call(
        &self,
        caller: &dyn Caller,
        provider: &Provider,
        key: Option<&Secret>,
        prompt: &str,
    ) -> (Result<Called, CallFailure>, Duration) {
        let started = Instant::now();
        let result = caller.call(provider, key, prompt).await;
        let took = started.elapsed();
        let (cost_usd, rejected) = match &result {
            Ok(called) => (called.cost_usd, false),
            Err(failure) => {
                warn!(provider = %provider.id(), error = %failure.message, "summarizer: the call failed");
                (failure.cost_usd, failure.rejected)
            }
        };
        if rejected {
            if let Some(key) = key {
                self.rejections.reject(&provider.id(), key);
            }
            self.refresh_keys().await;
        }
        let spent_changed = self.record_spend(cost_usd).await;
        if rejected || spent_changed {
            self.broadcast_status();
        }
        (result, took)
    }

    /// Add `cost_usd` to the day's spend and write the ledger on a blocking
    /// thread. Returns whether the spend changed; a write that fails is
    /// logged and the spend kept in memory.
    async fn record_spend(&self, cost_usd: f64) -> bool {
        if !(cost_usd.is_finite() && cost_usd > 0.0) {
            return false;
        }
        let _write = self.ledger_writes.lock().await;
        let (path, bytes) = {
            let mut ledger = crate::sync::lock(&self.ledger);
            ledger.add(today(), cost_usd);
            (ledger.path().to_owned(), ledger.to_bytes())
        };
        let bytes = match bytes {
            Ok(bytes) => bytes,
            Err(err) => {
                error!(error = %format!("{err:#}"), "summarizer: the spend ledger does not serialize; today's spend is kept in memory");
                return true;
            }
        };
        let written =
            tokio::task::spawn_blocking(move || crate::orphan::write_atomic(&path, &bytes)).await;
        match written {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                warn!(error = %format!("{err:#}"), "summarizer: could not write the spend ledger; today's spend is kept in memory");
            }
            Err(err) => {
                error!(%err, "summarizer: the spend ledger write failed; today's spend is kept in memory");
            }
        }
        true
    }
}

/// What `request` gets when no model line was made for it: the template for
/// a stop request, nothing for a working refresh.
fn no_call_outcome(request: &Request) -> Outcome {
    match request.trigger {
        Trigger::Stop => Outcome::Line {
            line: policy::template(&request.input),
            cost_usd: 0.0,
        },
        Trigger::Working => Outcome::Skipped,
    }
}

/// Refuse settings this daemon cannot honour.
fn validate(settings: &SummarizerSettings) -> anyhow::Result<()> {
    if policy::resolve(&settings.provider).is_none() {
        bail!(
            "This daemon offers no summarizer provider `{}`.",
            settings.provider
        );
    }
    if !settings.daily_cap_usd.is_finite() || settings.daily_cap_usd < 0.0 {
        bail!("The daily cap must be a number of dollars, zero or more.");
    }
    match &settings.key_source {
        SummarizerKeySource::Unknown => bail!("This daemon does not know that key source."),
        SummarizerKeySource::Env { variable } if variable.trim().is_empty() => {
            bail!("Name the environment variable that holds the key.")
        }
        SummarizerKeySource::Stored | SummarizerKeySource::Env { .. } => Ok(()),
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests assert preconditions with expect")]
mod tests {
    use std::sync::Arc;

    use protocol::{KeyState, SummarizerKey, SummarizerSettings, SummarySource};

    use super::{Outcome, Request, Summarizer};
    use crate::server::Hub;
    use crate::server::test_support::{billing_hub, test_hub, test_summarizer};
    use crate::summarizer::prompt::{self, Trigger};
    use crate::summarizer::providers;
    use crate::summarizer::service::test_support::FakeCaller;

    /// What a stop request and a working refresh for `session_id` each get.
    async fn both_outcomes(summarizer: &Summarizer, session_id: &str) -> (Outcome, Outcome) {
        let request = |trigger| Request {
            session_id: session_id.to_owned(),
            trigger,
            input: prompt::shape(&["The tests pass."]),
        };
        let stop = summarizer.submit(request(Trigger::Stop)).await;
        let working = summarizer.submit(request(Trigger::Working)).await;
        (stop, working)
    }

    fn is_template(outcome: &Outcome) -> bool {
        matches!(outcome, Outcome::Line { line, .. } if line.source == SummarySource::Template)
    }

    fn change_settings(hub: &Hub, change: impl FnOnce(&mut SummarizerSettings)) {
        let mut settings = hub.state.summarizer_settings();
        change(&mut settings);
        hub.state
            .set_summarizer_settings(settings)
            .expect("store the settings");
    }

    #[tokio::test]
    async fn a_working_request_is_skipped_where_a_stop_gets_a_template() {
        let (hub, _scratch, fake) = billing_hub("summ-skip", 0.0);
        let defaults = hub.state.summarizer_settings();

        change_settings(&hub, |settings| settings.enabled = false);
        let (stop, working) = both_outcomes(&hub.summarizer, "off").await;
        assert!(is_template(&stop), "off: {stop:?}");
        assert_eq!(working, Outcome::Skipped, "off");

        change_settings(&hub, |settings| {
            settings.enabled = true;
            settings.provider = "no-such-provider".to_owned();
        });
        let (stop, working) = both_outcomes(&hub.summarizer, "no-provider").await;
        assert!(is_template(&stop), "no provider: {stop:?}");
        assert_eq!(working, Outcome::Skipped, "no provider");
        assert_eq!(fake.calls(), 0, "no call is made without a provider");

        change_settings(&hub, |settings| {
            *settings = SummarizerSettings {
                daily_cap_usd: 1.0,
                ..defaults
            };
        });
        crate::sync::lock(&hub.summarizer.ledger).add(super::today(), 0.6);
        let (stop, working) = both_outcomes(&hub.summarizer, "half-cap").await;
        assert!(
            matches!(&stop, Outcome::Line { line, .. } if line.source == SummarySource::Model),
            "past half the cap a stop still calls: {stop:?}"
        );
        assert_eq!(working, Outcome::Skipped, "past half the cap");
        assert_eq!(fake.calls(), 1, "only the stop called");

        let failing = test_summarizer(
            &hub.state,
            &hub.dirs,
            &hub.client_count,
            &hub.state_events,
            Arc::new(FakeCaller::failing()),
        );
        let (stop, working) = both_outcomes(&failing, "failed").await;
        assert!(is_template(&stop), "a failed call: {stop:?}");
        assert_eq!(working, Outcome::Skipped, "a failed call");
    }

    fn states(state: KeyState) -> Vec<SummarizerKey> {
        providers::TABLE
            .iter()
            .map(|row| SummarizerKey {
                provider: row.id.to_owned(),
                state,
            })
            .collect()
    }

    #[tokio::test]
    async fn an_older_key_refresh_finishing_last_does_not_overwrite_a_newer_one() {
        let (hub, _scratch) = test_hub("summ-key-generations");
        let summarizer = &hub.summarizer;
        let (older, _) = summarizer.begin_key_refresh();
        let (newer, _) = summarizer.begin_key_refresh();

        assert!(summarizer.finish_key_refresh(newer, states(KeyState::Stored)));
        assert!(
            !summarizer.finish_key_refresh(older, states(KeyState::Missing)),
            "the older check finishing last changes nothing"
        );
        assert_eq!(
            crate::sync::lock(&summarizer.keys).keys,
            states(KeyState::Stored)
        );
    }
}

/// A fake provider for tests: it counts its calls, records each prompt, and
/// answers one summary at a fixed cost.
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use tokio::sync::Notify;

    use protocol::AlertKind;

    use super::{CallFailure, CallFuture, Called, Caller};
    use crate::summarizer::policy::Provider;
    use crate::summarizer::reply::Summary;
    use crate::user_env::Secret;

    /// The fake provider's notification.
    pub(crate) const FAKE_NOTIFICATION: &str = "The tests pass and the branch is ready.";

    #[derive(Debug, Default)]
    pub(crate) struct FakeCaller {
        calls: AtomicUsize,
        prompts: Mutex<Vec<String>>,
        cost_usd: f64,
        /// When set, each call waits for a notification before answering.
        gate: Option<Arc<Notify>>,
        /// When set, each call fails.
        fails: bool,
    }

    impl FakeCaller {
        /// A fake whose every call fails, billing nothing.
        pub(crate) fn failing() -> Self {
            Self {
                fails: true,
                ..Self::default()
            }
        }

        /// A fake whose every call waits until `gate` is notified, billing
        /// nothing.
        pub(crate) fn gated(gate: Arc<Notify>) -> Self {
            Self {
                gate: Some(gate),
                ..Self::default()
            }
        }

        /// A fake that bills `cost_usd` per call.
        pub(crate) fn billing(cost_usd: f64) -> Self {
            Self {
                cost_usd,
                ..Self::default()
            }
        }

        /// How many calls it answered.
        pub(crate) fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        /// The prompt of every call it answered, in call order.
        pub(crate) fn prompts(&self) -> Vec<String> {
            crate::sync::lock(&self.prompts).clone()
        }
    }

    impl Caller for FakeCaller {
        fn call<'a>(
            &'a self,
            _provider: &'a Provider,
            _key: Option<&'a Secret>,
            prompt: &'a str,
        ) -> CallFuture<'a> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            crate::sync::lock(&self.prompts).push(prompt.to_owned());
            let cost_usd = self.cost_usd;
            let gate = self.gate.clone();
            let fails = self.fails;
            Box::pin(async move {
                if let Some(gate) = gate {
                    gate.notified().await;
                }
                if fails {
                    return Err(CallFailure {
                        message: "the fake provider failed".to_owned(),
                        cost_usd,
                        rejected: false,
                    });
                }
                Ok(Called {
                    summary: Summary {
                        kind: AlertKind::DoneWaiting,
                        headline: "Tests pass".to_owned(),
                        notification: FAKE_NOTIFICATION.to_owned(),
                    },
                    cost_usd,
                })
            })
        }
    }
}
