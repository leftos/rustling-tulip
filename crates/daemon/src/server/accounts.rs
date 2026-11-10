//! The accounts messages. `ListAccounts` is served on every connection
//! origin, since a snapshot carries no secret. The answer is the snapshot
//! the accounts driver last polled; before its first poll it is the plugin's
//! account service's snapshot, read off the async threads. A host with no
//! service, or one whose store cannot be read, answers no accounts and no
//! auto-switch state.
//!
//! The writes (`SwitchAccount`, `AddCurrentLogin`, `RemoveAccount`,
//! `SetAccountAlias`, `SetAccountOrder`, `SetAutoSwitch`,
//! `SetAutoSwitchRules`) each run on a task of their own, so a write never
//! holds the connection's later messages, and call the service's seam method
//! on a blocking thread; every one but the switch, `SetAutoSwitch` and
//! `SetAutoSwitchRules` is local-only, refused before it gets here
//! (`remote::host_only_refusal`). On a host with an account service,
//! a `SetAutoSwitch` that sets neither field, and a `SetAutoSwitchRules` that
//! sets no field, are refused `account_refused` before any seam call; a
//! successful one wakes the engine task after its refresh.
//! After the write returns,
//! whether it succeeded or not, the handler asks the driver for a refresh
//! and waits up to [`REFRESH_WAIT`] for it, so the driver stays the cache's
//! only writer. A success answers `Accounts` from the cache (or, with no
//! driver to refresh it, from one direct read of the store that caches
//! nothing); every other connection hears `AccountsUpdated` from the driver
//! when the snapshot changed. A switch that moved the login also broadcasts
//! `AccountEvent` `switched` with trigger `manual`, after the refresh and
//! before the answer. A failure answers `Error` with an `account_*`
//! code. A host with no service answers every write `account_unsupported`.
//!
//! `StartAccountLogin` and `CancelAccountLogin` (local-only) go to the
//! login slot ([`crate::accounts::login`]), which answers them at once; a
//! host with no service answers them `account_unsupported` too.
use super::Hub;
use super::StateEvent;
use super::connection::ConnCtx;
use crate::accounts::engine::account_event;
use crate::accounts::login;
use crate::accounts::snapshot::{Wire, wire};
use crate::accounts::{Accounts, SharedAccounts};
use agent_api::accounts::AccountEventRow;
use agent_api::{
    AccountsConfig, AccountsFailure, AccountsFailureKind, AccountsSnapshot, AutoSwitchRules,
};
use protocol::{ClientMessage, DaemonMessage, error_codes};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

/// How long a write's answer waits for the driver's refresh before it
/// answers from the cache.
pub(crate) const REFRESH_WAIT: Duration = Duration::from_secs(10);

/// The answer to a write on a host that keeps no accounts.
const NO_ACCOUNTS: &str = "this host keeps no accounts";

/// The answer to a `SetAutoSwitch` or a `SetAutoSwitchRules` that sets
/// nothing.
const NOTHING_TO_CHANGE: &str = "nothing to change";

/// The trigger of a switch a user asked for.
const MANUAL: &str = "manual";

/// The accounts handle over the service the registered plugins offer for
/// this host's account store, or `None` when the daemon runs elevated, the
/// store's folder cannot be resolved, or no plugin keeps accounts. Its
/// account login runs its sessions on the hub `login_hub` is set to.
pub(crate) fn start_service(
    login_hub: &Arc<std::sync::OnceLock<super::Hub>>,
) -> Option<Arc<Accounts>> {
    let dir = match crate::paths::resolve_accounts_dir() {
        Ok(dir) => dir,
        Err(err) => {
            warn!(%err, "could not resolve the account store folder; accounts are off");
            return None;
        }
    };
    let mut agent = None;
    let service = service_for(dir, |cfg| {
        agent_registry::all().iter().find_map(|plugin| {
            let service = plugin.accounts(cfg)?;
            agent = Some(plugin.descriptor().id.clone());
            Some(service)
        })
    });
    info!(enabled = service.is_some(), "account service");
    let service = service?;
    let login_root = match crate::paths::resolve_account_login_dir() {
        Ok(root) => Some(root),
        Err(err) => {
            warn!(%err, "could not resolve the account login folder; account login is off");
            None
        }
    };
    Some(match (login_root, agent) {
        (Some(root), Some(agent)) => Accounts::with_login(
            service,
            login::LoginSetup {
                root,
                runner: Arc::new(login::SessionRunner::new(
                    agent.clone(),
                    Arc::clone(login_hub),
                )),
                agent,
            },
        ),
        _ => Accounts::new(service),
    })
}

/// The service `ask` builds for the store at `dir`; `ask` is not called
/// without a folder (an elevated daemon has none).
pub(crate) fn service_for(
    dir: Option<PathBuf>,
    ask: impl FnOnce(&AccountsConfig) -> Option<SharedAccounts>,
) -> Option<SharedAccounts> {
    let cfg = AccountsConfig::new(dir?);
    #[cfg(feature = "e2e-loopback-peers")]
    let cfg = match accounts_base_url_override(|name| std::env::var(name).ok()) {
        Some(url) => {
            warn!(
                origin = %url,
                "built with e2e-loopback-peers: the account service's profile, usage and refresh requests go to RUSTLING_TULIP_ACCOUNTS_BASE_URL; never ship this build"
            );
            AccountsConfig {
                base_url: Some(url),
                ..cfg
            }
        }
        None => cfg,
    };
    ask(&cfg)
}

/// The origin `RUSTLING_TULIP_ACCOUNTS_BASE_URL` names, read through `read`:
/// the value when it is an `http://` or `https://` origin on a loopback host
/// ([`is_loopback_origin`]), else `None` (any other value is warned of and
/// ignored). Only an `e2e-loopback-peers` build asks, so the live specs can
/// point the account service's requests at a local server.
#[cfg_attr(
    all(not(feature = "e2e-loopback-peers"), not(test)),
    expect(
        dead_code,
        reason = "only an e2e-loopback-peers build reads the override"
    )
)]
fn accounts_base_url_override(read: impl Fn(&str) -> Option<String>) -> Option<String> {
    let value = read("RUSTLING_TULIP_ACCOUNTS_BASE_URL")?;
    if is_loopback_origin(&value) {
        return Some(value);
    }
    if !value.is_empty() {
        warn!(
            value = %value,
            "RUSTLING_TULIP_ACCOUNTS_BASE_URL is not a loopback http(s) origin; ignored"
        );
    }
    None
}

/// Whether `value` is `http://` or `https://`, then `127.0.0.1`, `[::1]` or
/// `localhost`, then an optional `:<port>`, with nothing after it.
#[cfg_attr(
    all(not(feature = "e2e-loopback-peers"), not(test)),
    expect(
        dead_code,
        reason = "only an e2e-loopback-peers build reads the override"
    )
)]
pub(crate) fn is_loopback_origin(value: &str) -> bool {
    let Some(authority) = value
        .strip_prefix("http://")
        .or_else(|| value.strip_prefix("https://"))
    else {
        return false;
    };
    let (host, port) = if authority.ends_with(']') {
        (authority, None)
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };
    let port_ok =
        port.is_none_or(|port| !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()));
    port_ok && (host == "127.0.0.1" || host == "[::1]" || host.eq_ignore_ascii_case("localhost"))
}

/// Handle the accounts messages `dispatch` routes here.
pub(super) async fn handle(hub: &Hub, msg: ClientMessage, ctx: &ConnCtx<'_>) {
    match msg {
        ClientMessage::ListAccounts { request_id } => {
            let Wire { accounts, auto } = match &hub.accounts {
                Some(handle) => match handle.cached() {
                    Some(cached) => cached,
                    // Before the driver's first poll: one read of the
                    // store, which the driver's poll then replaces.
                    None => wire(&read_snapshot(Arc::clone(handle.service())).await),
                },
                None => Wire::default(),
            };
            let _ = ctx.out_tx.send(DaemonMessage::Accounts {
                accounts,
                auto,
                request_id,
            });
            // A client asking is interest: a driver parked with no account
            // polls again.
            if let Some(handle) = &hub.accounts {
                handle.wake.notify_one();
            }
        }
        ClientMessage::SwitchAccount { number, request_id } => {
            write(hub, ctx, request_id, "switch", move |service| {
                service.switch_to(number).map(|report| {
                    report.switched.then(|| AccountEventRow::Switched {
                        from: report.from.and_then(|from| from.number),
                        to: report.to.number.unwrap_or(number),
                        trigger: MANUAL.to_owned(),
                    })
                })
            });
        }
        ClientMessage::AddCurrentLogin { request_id } => {
            write(hub, ctx, request_id, "add current login", |service| {
                service.add_current_login().map(|_| None)
            });
        }
        ClientMessage::RemoveAccount { number, request_id } => {
            write(hub, ctx, request_id, "remove", move |service| {
                service.remove_account(number).map(|_| None)
            });
        }
        ClientMessage::SetAccountAlias {
            number,
            alias,
            request_id,
        } => {
            write(hub, ctx, request_id, "set alias", move |service| {
                service.set_alias(number, alias).map(|_| None)
            });
        }
        ClientMessage::SetAccountOrder { order, request_id } => {
            write(hub, ctx, request_id, "set order", move |service| {
                service.set_order(order).map(|_| None)
            });
        }
        m @ (ClientMessage::SetAutoSwitch { .. } | ClientMessage::SetAutoSwitchRules { .. }) => {
            auto_switch(hub, ctx, m);
        }
        ClientMessage::StartAccountLogin { request_id } => {
            let reply = login::Reply::new(ctx.out_tx.clone(), request_id);
            match &hub.accounts {
                Some(handle) => login::start(
                    handle,
                    hub.state_events.clone(),
                    hub.shutdown_tx.subscribe(),
                    reply,
                ),
                None => reply.send(error(
                    error_codes::ACCOUNT_UNSUPPORTED,
                    NO_ACCOUNTS.to_owned(),
                    reply.id(),
                )),
            }
        }
        ClientMessage::CancelAccountLogin { request_id } => {
            let reply = login::Reply::new(ctx.out_tx.clone(), request_id);
            match &hub.accounts {
                Some(handle) => login::cancel(handle, reply),
                None => reply.send(error(
                    error_codes::ACCOUNT_UNSUPPORTED,
                    NO_ACCOUNTS.to_owned(),
                    reply.id(),
                )),
            }
        }
        other => warn!(?other, "accounts::handle: routed a message it does not own"),
    }
}

/// The `SetAutoSwitch` and `SetAutoSwitchRules` arms of [`handle`]: a message
/// that sets nothing is refused before any seam call, so a host with an
/// account service answers `account_refused` rather than a no-op write; a
/// successful write wakes the engine task after its refresh.
fn auto_switch(hub: &Hub, ctx: &ConnCtx<'_>, msg: ClientMessage) {
    match msg {
        ClientMessage::SetAutoSwitch {
            enabled,
            threshold_pct,
            request_id,
        } => {
            // After the no-service check `write` makes: a host with no
            // account service answers `account_unsupported` either way.
            if hub.accounts.is_some() && enabled.is_none() && threshold_pct.is_none() {
                let _ = ctx.out_tx.send(error(
                    error_codes::ACCOUNT_REFUSED,
                    NOTHING_TO_CHANGE.to_owned(),
                    request_id,
                ));
                return;
            }
            write_then(
                hub,
                ctx,
                request_id,
                "set auto-switch",
                AfterWrite::WakeEngine,
                move |service| {
                    service
                        .set_auto_switch(enabled, threshold_pct)
                        .map(|_| None)
                },
            );
        }
        ClientMessage::SetAutoSwitchRules {
            hysteresis_pct,
            cooldown_secs,
            strategy,
            rate_window_minutes,
            request_id,
        } => {
            let rules = AutoSwitchRules {
                hysteresis_pct,
                cooldown_secs,
                strategy,
                rate_window_minutes,
            };
            // After the no-service check `write` makes: a host with no
            // account service answers `account_unsupported` either way.
            if hub.accounts.is_some() && rules == AutoSwitchRules::default() {
                let _ = ctx.out_tx.send(error(
                    error_codes::ACCOUNT_REFUSED,
                    NOTHING_TO_CHANGE.to_owned(),
                    request_id,
                ));
                return;
            }
            write_then(
                hub,
                ctx,
                request_id,
                "set auto-switch rules",
                AfterWrite::WakeEngine,
                move |service| service.set_auto_switch_rules(rules).map(|_| None),
            );
        }
        other => warn!(
            ?other,
            "accounts::auto_switch: routed a message it does not own"
        ),
    }
}

/// The wire code of a write failure of `kind`.
pub(crate) fn code_of(kind: AccountsFailureKind) -> &'static str {
    match kind {
        AccountsFailureKind::Busy => error_codes::ACCOUNT_BUSY,
        AccountsFailureKind::Draining => error_codes::ACCOUNT_DRAINING,
        AccountsFailureKind::UnknownAccount => error_codes::ACCOUNT_UNKNOWN,
        AccountsFailureKind::Refused => error_codes::ACCOUNT_REFUSED,
        AccountsFailureKind::Unsupported => error_codes::ACCOUNT_UNSUPPORTED,
        _ => error_codes::ACCOUNT_FAILED,
    }
}

/// An `Error` reply with `code`.
pub(crate) fn error(code: &str, message: String, request_id: Option<String>) -> DaemonMessage {
    DaemonMessage::Error {
        message,
        request_id,
        code: Some(code.to_owned()),
    }
}

/// What a write's seam call came to: the event it broadcasts, if any.
type WriteOutcome = Result<Option<AccountEventRow>, AccountsFailure>;

/// Start the write `op` (named `what` in the log) on a task of its own, so
/// the seam call and the refresh wait never hold this connection's later
/// messages; the task answers the requester, after broadcasting the event a
/// successful `op` returned. A host with no account service is answered at
/// once.
fn write(
    hub: &Hub,
    ctx: &ConnCtx<'_>,
    request_id: Option<String>,
    what: &'static str,
    op: impl FnOnce(&dyn agent_api::AccountsService) -> WriteOutcome + Send + 'static,
) {
    write_then(hub, ctx, request_id, what, AfterWrite::Nothing, op);
}

/// What a successful write does after its refresh, before it answers.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AfterWrite {
    Nothing,
    /// Wakes the engine task, for a write to the auto-switch settings.
    WakeEngine,
}

/// [`write`], doing `after` once a successful `op`'s refresh is in.
fn write_then(
    hub: &Hub,
    ctx: &ConnCtx<'_>,
    request_id: Option<String>,
    what: &'static str,
    after: AfterWrite,
    op: impl FnOnce(&dyn agent_api::AccountsService) -> WriteOutcome + Send + 'static,
) {
    let Some(handle) = &hub.accounts else {
        let _ = ctx.out_tx.send(error(
            error_codes::ACCOUNT_UNSUPPORTED,
            NO_ACCOUNTS.to_owned(),
            request_id,
        ));
        return;
    };
    let handle = Arc::clone(handle);
    let out_tx = ctx.out_tx.clone();
    let events = hub.state_events.clone();
    tokio::spawn(async move {
        let reply = write_reply(&handle, &events, request_id, what, after, op).await;
        let _ = out_tx.send(reply);
    });
}

/// Run `op` on a blocking thread, wait for the driver's refresh, broadcast
/// the event `op` returned, and build the answer: `Accounts` on success,
/// `Error` with an `account_*` code otherwise.
async fn write_reply(
    handle: &Accounts,
    events: &tokio::sync::broadcast::Sender<StateEvent>,
    request_id: Option<String>,
    what: &'static str,
    after: AfterWrite,
    op: impl FnOnce(&dyn agent_api::AccountsService) -> WriteOutcome + Send + 'static,
) -> DaemonMessage {
    let service = Arc::clone(handle.service());
    let done = tokio::task::spawn_blocking(move || op(service.as_ref())).await;
    let outcome = match done {
        Ok(Ok(event)) => Ok(event),
        Ok(Err(failure)) => {
            warn!(
                what,
                kind = ?failure.kind,
                code = %failure.code,
                message = %failure.message,
                "an account write failed"
            );
            Err((code_of(failure.kind), failure.message))
        }
        Err(err) => {
            warn!(what, %err, "an account write stopped");
            Err((
                error_codes::ACCOUNT_FAILED,
                "the account write stopped".to_owned(),
            ))
        }
    };
    // A failed write may still have changed the store (a partial write, a
    // rollback), so the refresh comes first either way.
    let lane_open = refreshed(handle, what).await;
    match outcome {
        Ok(event) => {
            if after == AfterWrite::WakeEngine {
                handle.engine_wake.notify_one();
            }
            if let Some(row) = event {
                let _ = events.send(StateEvent::AccountEvent {
                    event: account_event(row),
                });
            }
            let cached = if lane_open { handle.cached() } else { None };
            let Wire { accounts, auto } = match cached {
                Some(cached) => cached,
                None => wire(&read_snapshot(Arc::clone(handle.service())).await),
            };
            DaemonMessage::Accounts {
                accounts,
                auto,
                request_id,
            }
        }
        Err((code, message)) => error(code, message, request_id),
    }
}

/// Ask the driver for a refresh and wait up to [`REFRESH_WAIT`] for it;
/// `false` when the lane is closed (no driver, or it ended before serving
/// the refresh), so the cache cannot be trusted to hold the write.
pub(crate) async fn refreshed(handle: &Accounts, what: &'static str) -> bool {
    let Some(ack) = handle.request_refresh() else {
        return false;
    };
    match tokio::time::timeout(REFRESH_WAIT, ack).await {
        Ok(Ok(())) => true,
        Ok(Err(_)) => false,
        Err(_) => {
            warn!(
                what,
                bound = ?REFRESH_WAIT,
                "the accounts refresh after a write did not come in time; answering from the cache"
            );
            true
        }
    }
}

/// `service`'s snapshot, read on a blocking thread; empty when it fails.
pub(crate) async fn read_snapshot(service: SharedAccounts) -> AccountsSnapshot {
    let read = tokio::task::spawn_blocking(move || service.snapshot()).await;
    match read {
        Ok(Ok(snapshot)) => snapshot,
        Ok(Err(err)) => {
            warn!(%err, "the account service could not read its store; answering no accounts");
            AccountsSnapshot::default()
        }
        Err(err) => {
            warn!(%err, "the account snapshot read stopped; answering no accounts");
            AccountsSnapshot::default()
        }
    }
}

/// An account service the write tests drive: it records each write, fails
/// every write with a chosen failure, and otherwise changes its snapshot as
/// the write would change the store.
#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test fails with the message of the precondition it lost"
)]
pub(crate) mod fake {
    use crate::server::connection::{AuthKind, ConnCtx, FetchRegistry, PtyLane};
    use crate::server::{Hub, dispatch};
    use agent_api::{
        AccountKindRow, AccountRef, AccountRow, AccountsError, AccountsFailure, AccountsService,
        AccountsSnapshot, AutoState, AutoSwitchRules, PollReport, PollScope, SwitchReport,
        UsageRow, UsageStatus,
    };
    use protocol::{ClientMessage, DaemonMessage};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, PoisonError};
    use std::time::Duration;
    use tokio::sync::{Mutex as AsyncMutex, mpsc};

    /// The number [`Writable::add_current_login`] adds.
    pub(crate) const ADDED: u32 = 9;

    /// How long a test waits for a reply before it fails as hung.
    pub(crate) const HANG: Duration = Duration::from_secs(30);

    /// Dispatch `msg` on a connection of origin `auth` whose messages go to
    /// `out_tx`; returns once `dispatch` does, which a write's answer may
    /// follow.
    pub(crate) async fn dispatch_with(
        hub: &Hub,
        msg: ClientMessage,
        auth: &AuthKind,
        out_tx: &mpsc::UnboundedSender<DaemonMessage>,
    ) {
        let (file_tx, _file_rx) = mpsc::channel(1);
        let pty_lane: Arc<PtyLane> = Arc::default();
        let fetches: FetchRegistry = Arc::new(AsyncMutex::new(HashMap::new()));
        let ctx = ConnCtx {
            out_tx,
            file_tx: &file_tx,
            pty_lane: &pty_lane,
            fetches: &fetches,
            tails: &Arc::default(),
            client_id: "c1",
            client_name: None,
            connection: 1,
            auth,
        };
        dispatch(hub, msg, &ctx)
            .await
            .expect("a failed request is reported on the wire, not returned from dispatch");
    }

    /// [`dispatch_with`] on a local connection.
    pub(crate) async fn dispatch_local(
        hub: &Hub,
        msg: ClientMessage,
        out_tx: &mpsc::UnboundedSender<DaemonMessage>,
    ) {
        dispatch_with(hub, msg, &AuthKind::Local, out_tx).await;
    }

    /// The next message on `rx`, failing as hung after [`HANG`].
    pub(crate) async fn next_reply(
        rx: &mut mpsc::UnboundedReceiver<DaemonMessage>,
    ) -> DaemonMessage {
        tokio::time::timeout(HANG, rx.recv())
            .await
            .expect("hung: no reply arrived")
            .expect("the connection's channel is open")
    }

    /// Dispatch `msg` as `auth` and wait for its one reply, sent by
    /// `dispatch` or by the task it started.
    pub(crate) async fn ask_as(hub: &Hub, msg: ClientMessage, auth: &AuthKind) -> DaemonMessage {
        let (out_tx, mut rx) = mpsc::unbounded_channel();
        dispatch_with(hub, msg, auth, &out_tx).await;
        drop(out_tx);
        let reply = next_reply(&mut rx).await;
        assert!(rx.recv().await.is_none(), "exactly one reply is sent");
        reply
    }

    #[derive(Default)]
    pub(crate) struct Writable {
        snapshot: Mutex<AccountsSnapshot>,
        calls: Mutex<Vec<String>>,
        fail: Mutex<Option<AccountsFailure>>,
    }

    /// Account `number`, with no usage read.
    pub(crate) fn account(number: u32, active: bool) -> AccountRow {
        AccountRow {
            number,
            email: format!("user{number}@example.com"),
            alias: None,
            org_name: None,
            kind: AccountKindRow::Oauth,
            active,
            disabled: false,
            usage: UsageRow {
                status: UsageStatus::Ok,
                five_hour: None,
                seven_day: None,
                model_limits: Vec::new(),
                extra_usage: None,
                fetched_at: None,
                error: None,
            },
        }
    }

    impl Writable {
        /// A store of accounts 1 (active) and 2.
        pub(crate) fn two_accounts() -> Self {
            let service = Self::default();
            service.lock_snapshot().accounts = vec![account(1, true), account(2, false)];
            service
        }

        /// Every later write fails with `failure`.
        pub(crate) fn fail_with(&self, failure: AccountsFailure) {
            *self.fail.lock().unwrap_or_else(PoisonError::into_inner) = Some(failure);
        }

        /// The writes that reached the service, in order.
        pub(crate) fn calls(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }

        fn lock_snapshot(&self) -> std::sync::MutexGuard<'_, AccountsSnapshot> {
            self.snapshot.lock().unwrap_or_else(PoisonError::into_inner)
        }

        /// Record `call`, then fail or change the snapshot with `change`.
        fn write(
            &self,
            call: String,
            change: impl FnOnce(&mut AccountsSnapshot),
        ) -> Result<(), AccountsFailure> {
            self.calls
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(call);
            if let Some(failure) = self
                .fail
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
            {
                return Err(failure);
            }
            change(&mut self.lock_snapshot());
            Ok(())
        }
    }

    impl AccountsService for Writable {
        fn snapshot(&self) -> Result<AccountsSnapshot, AccountsError> {
            Ok(self.lock_snapshot().clone())
        }

        fn poll(&self, _scope: PollScope) -> Result<PollReport, AccountsError> {
            Ok(PollReport {
                snapshot: self.lock_snapshot().clone(),
                next_poll_in: None,
            })
        }

        fn begin_shutdown(&self, _bound: Duration) -> bool {
            true
        }

        fn switch_to(&self, number: u32) -> Result<SwitchReport, AccountsFailure> {
            let mut from = None;
            self.write(format!("switch {number}"), |snapshot| {
                for row in &mut snapshot.accounts {
                    if row.active {
                        from = Some(AccountRef {
                            number: Some(row.number),
                            email: row.email.clone(),
                        });
                    }
                    row.active = row.number == number;
                }
            })?;
            Ok(SwitchReport {
                switched: true,
                from,
                to: AccountRef {
                    number: Some(number),
                    email: format!("user{number}@example.com"),
                },
                warnings: Vec::new(),
                takes_effect_within: None,
            })
        }

        fn add_current_login(&self) -> Result<u32, AccountsFailure> {
            self.write("add".to_owned(), |snapshot| {
                snapshot.accounts.push(account(ADDED, false));
            })?;
            Ok(ADDED)
        }

        fn remove_account(&self, number: u32) -> Result<bool, AccountsFailure> {
            self.write(format!("remove {number}"), |snapshot| {
                snapshot.accounts.retain(|row| row.number != number);
            })?;
            Ok(true)
        }

        fn set_alias(&self, number: u32, alias: Option<String>) -> Result<bool, AccountsFailure> {
            self.write(format!("alias {number} {alias:?}"), |snapshot| {
                for row in &mut snapshot.accounts {
                    if row.number == number {
                        row.alias.clone_from(&alias);
                    }
                }
            })?;
            Ok(true)
        }

        fn set_order(&self, order: Vec<u32>) -> Result<bool, AccountsFailure> {
            self.write(format!("order {order:?}"), |snapshot| {
                snapshot.accounts.sort_by_key(|row| {
                    order
                        .iter()
                        .position(|number| *number == row.number)
                        .unwrap_or(usize::MAX)
                });
            })?;
            Ok(true)
        }

        fn set_auto_switch(
            &self,
            enabled: Option<bool>,
            threshold_pct: Option<f64>,
        ) -> Result<bool, AccountsFailure> {
            self.write(format!("auto {enabled:?} {threshold_pct:?}"), |snapshot| {
                let auto = snapshot.auto.get_or_insert(AutoState {
                    enabled: false,
                    threshold_pct: 95.0,
                    hysteresis_pct: 5.0,
                    cooldown_secs: 300,
                    strategy: protocol::AutoSwitchStrategy::Best,
                    rate_window_minutes: None,
                    paused_reason: None,
                    warm_up_done: false,
                    last_decision: None,
                    fleet: None,
                });
                if let Some(enabled) = enabled {
                    auto.enabled = enabled;
                }
                if let Some(threshold_pct) = threshold_pct {
                    auto.threshold_pct = threshold_pct;
                }
            })?;
            Ok(true)
        }

        fn set_auto_switch_rules(&self, rules: AutoSwitchRules) -> Result<bool, AccountsFailure> {
            self.write(format!("rules {rules:?}"), |snapshot| {
                let auto = snapshot.auto.get_or_insert(AutoState {
                    enabled: false,
                    threshold_pct: 95.0,
                    hysteresis_pct: 5.0,
                    cooldown_secs: 300,
                    strategy: protocol::AutoSwitchStrategy::Best,
                    rate_window_minutes: None,
                    paused_reason: None,
                    warm_up_done: false,
                    last_decision: None,
                    fleet: None,
                });
                if let Some(hysteresis_pct) = rules.hysteresis_pct {
                    auto.hysteresis_pct = hysteresis_pct;
                }
                if let Some(cooldown_secs) = rules.cooldown_secs {
                    auto.cooldown_secs = cooldown_secs;
                }
                if let Some(strategy) = rules.strategy {
                    auto.strategy = strategy;
                }
                if let Some(rate_window_minutes) = rules.rate_window_minutes {
                    auto.rate_window_minutes = Some(rate_window_minutes);
                }
            })?;
            Ok(true)
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test fails with the message of the precondition it lost"
)]
mod tests {
    use super::*;
    use crate::server::connection::AuthKind;
    use crate::server::test_support::{dispatch_one_as, test_hub, test_peer};
    use agent_api::{
        AccountKindRow, AccountRow, AccountsError, AccountsService, AutoState, AutoSwitchRules,
        ExtraUsageRow, ModelLimitRow, PollReport, PollScope, UsageRow, UsageStatus, UsageWindowRow,
    };
    use protocol::{
        AccountKind, AccountSnapshot, AccountUsageStatus, AutoSwitchState, AutoSwitchStrategy,
    };
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, PoisonError, mpsc};
    use std::time::Duration;

    /// An account service answering a fixed snapshot or error.
    struct FakeAccounts(Result<AccountsSnapshot, AccountsError>);

    impl AccountsService for FakeAccounts {
        fn snapshot(&self) -> Result<AccountsSnapshot, AccountsError> {
            self.0.clone()
        }

        fn poll(&self, _scope: PollScope) -> Result<PollReport, AccountsError> {
            Ok(PollReport {
                snapshot: self.0.clone()?,
                next_poll_in: None,
            })
        }

        fn begin_shutdown(&self, _bound: Duration) -> bool {
            true
        }
    }

    fn shared(answer: Result<AccountsSnapshot, AccountsError>) -> Arc<Accounts> {
        Accounts::new(Arc::new(FakeAccounts(answer)))
    }

    /// An account service whose poll says it has started, then blocks until
    /// it is released; its snapshot answers at once.
    struct BlockedPoll {
        snapshot: AccountsSnapshot,
        started: Mutex<mpsc::Sender<()>>,
        release: Mutex<mpsc::Receiver<()>>,
    }

    impl AccountsService for BlockedPoll {
        fn snapshot(&self) -> Result<AccountsSnapshot, AccountsError> {
            Ok(self.snapshot.clone())
        }

        fn poll(&self, _scope: PollScope) -> Result<PollReport, AccountsError> {
            let _ = self
                .started
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .send(());
            let _ = self
                .release
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .recv();
            Ok(PollReport {
                snapshot: self.snapshot.clone(),
                next_poll_in: None,
            })
        }

        fn begin_shutdown(&self, _bound: Duration) -> bool {
            true
        }
    }

    fn row(number: u32, pct: f64) -> AccountRow {
        AccountRow {
            number,
            email: format!("user{number}@example.com"),
            alias: Some(format!("alias-{number}")),
            org_name: None,
            kind: AccountKindRow::Oauth,
            active: number == 1,
            disabled: false,
            usage: UsageRow {
                status: UsageStatus::Ok,
                five_hour: Some(UsageWindowRow {
                    pct,
                    resets_at: None,
                }),
                seven_day: None,
                model_limits: Vec::new(),
                extra_usage: None,
                fetched_at: None,
                error: None,
            },
        }
    }

    fn auto(threshold_pct: f64) -> AutoState {
        AutoState {
            enabled: true,
            threshold_pct,
            hysteresis_pct: 5.0,
            cooldown_secs: 300,
            strategy: AutoSwitchStrategy::Best,
            rate_window_minutes: None,
            paused_reason: None,
            warm_up_done: true,
            last_decision: Some("stay".to_owned()),
            fleet: None,
        }
    }

    async fn list(hub: &Hub) -> (Vec<AccountSnapshot>, Option<AutoSwitchState>) {
        let reply = dispatch_one_as(
            hub,
            ClientMessage::ListAccounts {
                request_id: Some("acc-1".to_owned()),
            },
            &AuthKind::Local,
        )
        .await;
        assert!(
            matches!(&reply, DaemonMessage::Accounts { request_id: Some(id), .. } if id == "acc-1"),
            "not the accounts reply: {reply:?}"
        );
        match reply {
            DaemonMessage::Accounts { accounts, auto, .. } => (accounts, auto),
            _ => (Vec::new(), None),
        }
    }

    #[tokio::test]
    async fn list_accounts_answers_empty_from_both_origins() {
        let (hub, _scratch) = test_hub("list-accounts-empty");
        for auth in [AuthKind::Local, AuthKind::Tailnet(test_peer())] {
            let reply = dispatch_one_as(
                &hub,
                ClientMessage::ListAccounts {
                    request_id: Some("acc-1".to_owned()),
                },
                &auth,
            )
            .await;
            assert!(
                matches!(
                    &reply,
                    DaemonMessage::Accounts { accounts, auto: None, request_id: Some(id) }
                        if accounts.is_empty() && id == "acc-1"
                ),
                "{auth:?}: {reply:?}"
            );
        }
    }

    #[tokio::test]
    async fn list_accounts_answers_from_the_service() {
        let (mut hub, _scratch) = test_hub("list-accounts-service");
        hub.accounts = Some(shared(Ok(AccountsSnapshot {
            accounts: vec![row(2, 40.0), row(1, 97.5)],
            auto: Some(auto(95.0)),
        })));

        let (accounts, auto) = list(&hub).await;

        let numbers: Vec<u32> = accounts.iter().map(|account| account.number).collect();
        assert_eq!(numbers, [2, 1]);
        assert_eq!(accounts[1].email, "user1@example.com");
        assert_eq!(accounts[1].alias.as_deref(), Some("alias-1"));
        assert!(accounts[1].active && !accounts[0].active);
        assert_eq!(accounts[1].kind, AccountKind::Oauth);
        assert_eq!(accounts[1].status, AccountUsageStatus::Ok);
        assert_eq!(accounts[1].five_hour.as_ref().map(|w| w.pct), Some(97.5));
        let auto = auto.expect("the service's auto-switch state");
        assert!((auto.threshold_pct - 95.0).abs() < f64::EPSILON);
        assert_eq!(auto.cooldown_secs, 300);
        assert_eq!(auto.last_decision.as_deref(), Some("stay"));
    }

    #[tokio::test]
    async fn list_accounts_answers_empty_on_service_error() {
        let (mut hub, _scratch) = test_hub("list-accounts-service-error");
        hub.accounts = Some(shared(Err(AccountsError::new("the store is unreadable"))));

        let (accounts, auto) = list(&hub).await;

        assert!(accounts.is_empty(), "{accounts:?}");
        assert_eq!(auto, None);
    }

    #[tokio::test]
    async fn list_accounts_drops_non_finite_numbers() {
        let (mut hub, _scratch) = test_hub("list-accounts-non-finite");
        let mut odd = row(1, f64::NAN);
        odd.usage.seven_day = Some(UsageWindowRow {
            pct: f64::INFINITY,
            resets_at: None,
        });
        odd.usage.model_limits = vec![
            ModelLimitRow {
                model: "big".to_owned(),
                pct: f64::NEG_INFINITY,
                resets_at: None,
            },
            ModelLimitRow {
                model: "small".to_owned(),
                pct: 12.0,
                resets_at: None,
            },
        ];
        odd.usage.extra_usage = Some(ExtraUsageRow {
            enabled: true,
            user_disabled: false,
            spend_limit_reached: false,
            limit: Some(f64::NAN),
        });
        hub.accounts = Some(shared(Ok(AccountsSnapshot {
            accounts: vec![odd],
            auto: Some(auto(f64::NAN)),
        })));

        let (accounts, auto) = list(&hub).await;

        let account = &accounts[0];
        assert_eq!(account.five_hour, None);
        assert_eq!(account.seven_day, None);
        let models: Vec<&str> = account
            .model_limits
            .iter()
            .map(|limit| limit.model.as_str())
            .collect();
        assert_eq!(models, ["small"]);
        let extra = account.extra_usage.as_ref().expect("the extra usage stays");
        assert!(extra.enabled);
        assert_eq!(extra.limit, None);
        assert_eq!(auto, None);
        serde_json::to_string(&accounts).expect("every number left is finite");
    }

    #[tokio::test]
    async fn list_accounts_answers_while_a_poll_is_blocked() {
        let (mut hub, _scratch) = test_hub("list-accounts-poll-blocked");
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let service: SharedAccounts = Arc::new(BlockedPoll {
            snapshot: AccountsSnapshot {
                accounts: vec![row(1, 20.0)],
                auto: None,
            },
            started: Mutex::new(started_tx),
            release: Mutex::new(release_rx),
        });
        hub.accounts = Some(Accounts::new(Arc::clone(&service)));
        let poller = std::thread::spawn(move || service.poll(PollScope::OnDemand));
        started_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("the poll started");

        // A hang guard only: the answer is awaited on its channel.
        let answer = tokio::time::timeout(Duration::from_secs(60), list(&hub)).await;
        release_tx.send(()).expect("the poll is still waiting");
        let (accounts, _auto) = answer.expect("the list answered while the poll was blocked");

        let numbers: Vec<u32> = accounts.iter().map(|account| account.number).collect();
        assert_eq!(numbers, [1]);
        let polled = poller.join().expect("the poll thread did not panic");
        assert!(polled.is_ok(), "{polled:?}");
    }

    #[tokio::test]
    async fn list_accounts_answers_from_the_cache() {
        let (mut hub, _scratch) = test_hub("list-accounts-cache");
        let handle = shared(Ok(AccountsSnapshot {
            accounts: vec![row(1, 10.0)],
            auto: None,
        }));
        let polled = AccountsSnapshot {
            accounts: vec![row(2, 60.0), row(1, 30.0)],
            auto: Some(auto(95.0)),
        };
        assert!(handle.store_if_changed(wire(&polled)));
        hub.accounts = Some(handle);

        let (accounts, auto) = list(&hub).await;

        let numbers: Vec<u32> = accounts.iter().map(|account| account.number).collect();
        assert_eq!(numbers, [2, 1], "the cached poll, not the store's snapshot");
        assert_eq!(accounts[0].five_hour.as_ref().map(|w| w.pct), Some(60.0));
        assert!(auto.is_some(), "the cached auto-switch state");
    }

    #[tokio::test]
    async fn list_accounts_wakes_the_driver() {
        let (mut hub, _scratch) = test_hub("list-accounts-wake");
        let handle = shared(Ok(AccountsSnapshot::default()));
        hub.accounts = Some(Arc::clone(&handle));

        let _ = list(&hub).await;

        // A hang guard only: the wake's permit is already stored.
        tokio::time::timeout(Duration::from_secs(60), handle.wake.notified())
            .await
            .expect("the list woke the driver");
    }

    #[test]
    fn elevated_daemon_asks_for_no_accounts_service() {
        let asked = AtomicBool::new(false);
        let ask = |_: &AccountsConfig| -> Option<SharedAccounts> {
            asked.store(true, Ordering::SeqCst);
            Some(Arc::new(FakeAccounts(Ok(AccountsSnapshot::default()))))
        };
        let dir = PathBuf::from(r"C:\accounts");
        let elevated =
            crate::paths::accounts_dir_of(daemon_client::profile::Profile::Elevated, dir.clone());

        assert!(service_for(elevated, ask).is_none());
        assert!(!asked.load(Ordering::SeqCst), "an elevated daemon asked");

        let ordinary =
            crate::paths::accounts_dir_of(daemon_client::profile::Profile::Ordinary, dir);
        assert!(service_for(ordinary, ask).is_some());
        assert!(
            asked.load(Ordering::SeqCst),
            "an ordinary daemon did not ask"
        );
    }

    /// A reader of the process environment holding `value` for the base-URL
    /// variable alone.
    fn holding(value: Option<&str>) -> impl Fn(&str) -> Option<String> {
        move |name| {
            (name == "RUSTLING_TULIP_ACCOUNTS_BASE_URL")
                .then(|| value.map(str::to_owned))
                .flatten()
        }
    }

    #[test]
    fn accounts_base_url_override_takes_an_http_url() {
        assert_eq!(
            accounts_base_url_override(holding(Some("http://127.0.0.1:5000"))),
            Some("http://127.0.0.1:5000".to_owned())
        );
    }

    #[test]
    fn accounts_base_url_override_takes_an_https_loopback_url() {
        assert_eq!(
            accounts_base_url_override(holding(Some("https://[::1]:8443"))),
            Some("https://[::1]:8443".to_owned())
        );
    }

    #[test]
    fn accounts_base_url_override_takes_localhost() {
        assert_eq!(
            accounts_base_url_override(holding(Some("http://localhost:9"))),
            Some("http://localhost:9".to_owned())
        );
    }

    #[test]
    fn accounts_base_url_override_ignores_a_host_off_loopback() {
        assert_eq!(
            accounts_base_url_override(holding(Some("https://example.test"))),
            None
        );
        assert_eq!(
            accounts_base_url_override(holding(Some("http://10.0.0.1:9"))),
            None
        );
    }

    #[test]
    fn accounts_base_url_override_ignores_an_empty_value() {
        assert_eq!(accounts_base_url_override(holding(Some(""))), None);
    }

    #[test]
    fn accounts_base_url_override_ignores_a_value_that_is_not_an_http_url() {
        assert_eq!(accounts_base_url_override(holding(Some("ftp://x"))), None);
    }

    #[test]
    fn accounts_base_url_override_is_none_when_the_variable_is_unset() {
        assert_eq!(accounts_base_url_override(holding(None)), None);
    }

    use super::fake::{ADDED, HANG, Writable, ask_as, dispatch_with, next_reply};
    use agent_api::{AccountsFailure, AccountsFailureKind};
    use protocol::error_codes;

    /// A hub over `service`, with no driver.
    fn write_hub(
        tag: &str,
        service: &Arc<Writable>,
    ) -> (Hub, crate::server::test_support::ScratchDir) {
        let (mut hub, scratch) = test_hub(tag);
        hub.accounts = Some(Accounts::new(
            Arc::clone(service) as Arc<dyn agent_api::AccountsService>
        ));
        (hub, scratch)
    }

    fn numbers(accounts: &[AccountSnapshot]) -> Vec<u32> {
        accounts.iter().map(|account| account.number).collect()
    }

    /// The accounts of an `Accounts` reply that echoes `id`.
    fn accounts_of(reply: DaemonMessage, id: &str) -> Vec<AccountSnapshot> {
        assert!(
            matches!(&reply, DaemonMessage::Accounts { request_id: Some(echoed), .. } if echoed == id),
            "not the accounts reply to {id}: {reply:?}"
        );
        match reply {
            DaemonMessage::Accounts { accounts, .. } => accounts,
            _ => Vec::new(),
        }
    }

    fn switch(number: u32, id: &str) -> ClientMessage {
        ClientMessage::SwitchAccount {
            number,
            request_id: Some(id.to_owned()),
        }
    }

    /// Every write, each with request id `w<n>`.
    fn every_write() -> Vec<ClientMessage> {
        vec![
            switch(2, "w0"),
            ClientMessage::AddCurrentLogin {
                request_id: Some("w1".to_owned()),
            },
            ClientMessage::RemoveAccount {
                number: 2,
                request_id: Some("w2".to_owned()),
            },
            ClientMessage::SetAccountAlias {
                number: 1,
                alias: Some("work".to_owned()),
                request_id: Some("w3".to_owned()),
            },
            ClientMessage::SetAccountOrder {
                order: vec![ADDED, 1],
                request_id: Some("w4".to_owned()),
            },
        ]
    }

    #[tokio::test]
    async fn switch_account_answers_accounts_with_the_request_id() {
        let service = Arc::new(Writable::two_accounts());
        let (hub, _scratch) = write_hub("switch-account", &service);
        let handle = Arc::clone(hub.accounts.as_ref().expect("the hub has accounts"));
        let mut events = hub.state_events.subscribe();
        let (_clients, clients) = tokio::sync::watch::channel(0_usize);
        let (shutdown_tx, shutdown) = tokio::sync::watch::channel(false);
        handle.start_driver(
            hub.state_events.clone(),
            crate::accounts::driver::Watches { clients, shutdown },
        );

        let reply = ask_as(&hub, switch(2, "w1"), &AuthKind::Local).await;

        let accounts = accounts_of(reply, "w1");
        let active: Vec<u32> = accounts
            .iter()
            .filter(|account| account.active)
            .map(|account| account.number)
            .collect();
        assert_eq!(active, [2], "the answer is the post-switch snapshot");
        assert_eq!(service.calls(), ["switch 2"]);
        let cached = handle.cached().expect("the driver cached the refresh");
        assert_eq!(cached.accounts, accounts, "the answer is the cache");
        let event = events.try_recv().expect("the change was broadcast");
        assert!(
            matches!(&event, crate::server::StateEvent::Accounts { accounts: sent, .. } if *sent == accounts),
            "{event:?}"
        );
        shutdown_tx.send_replace(true);
        handle.join_driver(Duration::from_secs(60)).await;
    }

    /// An account service whose switch says it has started, then blocks
    /// until it is released (or the test drops the release); its snapshot
    /// answers at once.
    struct BlockedSwitch {
        snapshot: AccountsSnapshot,
        started: Mutex<mpsc::Sender<()>>,
        release: Mutex<mpsc::Receiver<()>>,
    }

    impl AccountsService for BlockedSwitch {
        fn snapshot(&self) -> Result<AccountsSnapshot, AccountsError> {
            Ok(self.snapshot.clone())
        }

        fn poll(&self, _scope: PollScope) -> Result<PollReport, AccountsError> {
            Ok(PollReport {
                snapshot: self.snapshot.clone(),
                next_poll_in: None,
            })
        }

        fn begin_shutdown(&self, _bound: Duration) -> bool {
            true
        }

        fn switch_to(&self, number: u32) -> Result<agent_api::SwitchReport, AccountsFailure> {
            let _ = self
                .started
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .send(());
            let _ = self
                .release
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .recv();
            Ok(agent_api::SwitchReport {
                switched: true,
                from: None,
                to: agent_api::AccountRef {
                    number: Some(number),
                    email: format!("user{number}@example.com"),
                },
                warnings: Vec::new(),
                takes_effect_within: None,
            })
        }
    }

    #[tokio::test]
    async fn a_write_does_not_hold_the_connection() {
        let (mut hub, _scratch) = test_hub("write-does-not-hold");
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        hub.accounts = Some(Accounts::new(Arc::new(BlockedSwitch {
            snapshot: AccountsSnapshot {
                accounts: vec![row(1, 20.0)],
                auto: None,
            },
            started: Mutex::new(started_tx),
            release: Mutex::new(release_rx),
        })));
        let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel();

        tokio::time::timeout(
            HANG,
            dispatch_with(&hub, switch(1, "w1"), &AuthKind::Local, &out_tx),
        )
        .await
        .expect("the switch did not hold the connection's dispatch");
        // Off the runtime's thread, so the write's task can run meanwhile.
        tokio::task::spawn_blocking(move || started_rx.recv_timeout(HANG))
            .await
            .expect("the wait did not panic")
            .expect("the switch reached the service");
        tokio::time::timeout(
            HANG,
            dispatch_with(
                &hub,
                ClientMessage::ListAccounts {
                    request_id: Some("l1".to_owned()),
                },
                &AuthKind::Local,
                &out_tx,
            ),
        )
        .await
        .expect("the list was dispatched while the switch is blocked");

        let listed = next_reply(&mut out_rx).await;
        assert_eq!(
            numbers(&accounts_of(listed, "l1")),
            [1],
            "the list came first"
        );

        release_tx.send(()).expect("the switch is still waiting");
        let switched = next_reply(&mut out_rx).await;
        assert_eq!(numbers(&accounts_of(switched, "w1")), [1]);
    }

    #[tokio::test]
    async fn each_write_reaches_the_service() {
        let service = Arc::new(Writable::two_accounts());
        let (hub, _scratch) = write_hub("each-write", &service);

        let writes = every_write().into_iter().skip(1);
        let mut answers = Vec::new();
        for (index, msg) in writes.enumerate() {
            let reply = ask_as(&hub, msg, &AuthKind::Local).await;
            answers.push(accounts_of(reply, &format!("w{}", index + 1)));
        }
        let aliased = answers[2]
            .iter()
            .find(|account| account.number == 1)
            .expect("account 1 is listed");
        assert_eq!(aliased.alias.as_deref(), Some("work"), "the new alias");
        let answers: Vec<Vec<u32>> = answers.iter().map(|accounts| numbers(accounts)).collect();

        assert_eq!(
            service.calls(),
            [
                "add".to_owned(),
                "remove 2".to_owned(),
                format!("alias 1 {:?}", Some("work")),
                format!("order [{ADDED}, 1]"),
            ]
        );
        assert_eq!(answers[0], [1, 2, ADDED], "the added login is listed");
        assert_eq!(answers[1], [1, ADDED], "the removed account is gone");
        assert_eq!(answers[3], [ADDED, 1], "the new order");
    }

    #[tokio::test]
    async fn a_failed_write_answers_its_account_code() {
        let cases = [
            (AccountsFailureKind::Busy, error_codes::ACCOUNT_BUSY),
            (AccountsFailureKind::Draining, error_codes::ACCOUNT_DRAINING),
            (
                AccountsFailureKind::UnknownAccount,
                error_codes::ACCOUNT_UNKNOWN,
            ),
            (AccountsFailureKind::Refused, error_codes::ACCOUNT_REFUSED),
            (
                AccountsFailureKind::Unsupported,
                error_codes::ACCOUNT_UNSUPPORTED,
            ),
            (AccountsFailureKind::Failed, error_codes::ACCOUNT_FAILED),
        ];
        for (kind, want) in cases {
            let service = Arc::new(Writable::two_accounts());
            service.fail_with(AccountsFailure {
                kind,
                code: "plugin_code".to_owned(),
                message: format!("{kind:?} said no"),
            });
            let (hub, _scratch) = write_hub("failed-write", &service);

            let reply = ask_as(&hub, switch(2, "w1"), &AuthKind::Local).await;

            let said = format!("{kind:?} said no");
            assert!(
                matches!(
                    &reply,
                    DaemonMessage::Error { code: Some(code), message, request_id: Some(id) }
                        if code == want && *message == said && id == "w1"
                ),
                "{kind:?}: {reply:?}"
            );
            assert_eq!(service.calls(), ["switch 2"], "{kind:?}");
        }
    }

    #[tokio::test]
    async fn a_write_without_an_account_service_answers_unsupported() {
        let (hub, _scratch) = test_hub("write-no-service");
        for (index, msg) in every_write().into_iter().enumerate() {
            let reply = dispatch_one_as(&hub, msg, &AuthKind::Local).await;
            let want_id = format!("w{index}");
            assert!(
                matches!(
                    &reply,
                    DaemonMessage::Error { code: Some(code), message, request_id: Some(id) }
                        if code == error_codes::ACCOUNT_UNSUPPORTED
                            && message == "this host keeps no accounts"
                            && *id == want_id
                ),
                "{reply:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_write_with_no_driver_answers_from_a_direct_read() {
        let service = Arc::new(Writable::two_accounts());
        let (hub, _scratch) = write_hub("write-no-driver", &service);
        let handle = Arc::clone(hub.accounts.as_ref().expect("the hub has accounts"));
        let stale = AccountsSnapshot {
            accounts: vec![row(1, 10.0)],
            auto: None,
        };
        assert!(handle.store_if_changed(wire(&stale)));

        // A hang guard only: a closed lane answers at once.
        let reply = tokio::time::timeout(
            Duration::from_secs(5),
            ask_as(&hub, switch(2, "w1"), &AuthKind::Local),
        )
        .await
        .expect("the answer did not wait for a refresh");

        let accounts = accounts_of(reply, "w1");
        assert_eq!(numbers(&accounts), [1, 2], "the store, not the stale cache");
        assert!(accounts[1].active, "the post-switch snapshot");
        let cached = handle.cached().expect("the cache stays");
        assert_eq!(cached, wire(&stale), "a direct read caches nothing");
    }

    fn set_auto_switch(
        enabled: Option<bool>,
        threshold_pct: Option<f64>,
        id: &str,
    ) -> ClientMessage {
        ClientMessage::SetAutoSwitch {
            enabled,
            threshold_pct,
            request_id: Some(id.to_owned()),
        }
    }

    #[tokio::test]
    async fn set_auto_switch_answers_accounts_with_the_request_id() {
        let service = Arc::new(Writable::two_accounts());
        let (hub, _scratch) = write_hub("set-auto-switch", &service);

        let reply = ask_as(
            &hub,
            set_auto_switch(Some(true), Some(85.0), "a1"),
            &AuthKind::Local,
        )
        .await;

        assert!(
            matches!(&reply, DaemonMessage::Accounts { request_id: Some(id), .. } if id == "a1"),
            "not the accounts reply to a1: {reply:?}"
        );
        let auto = match reply {
            DaemonMessage::Accounts { auto, .. } => auto,
            _ => None,
        }
        .expect("the answer carries the auto-switch state");
        assert!(auto.enabled, "{auto:?}");
        assert!((auto.threshold_pct - 85.0).abs() < f64::EPSILON, "{auto:?}");
        assert_eq!(service.calls(), ["auto Some(true) Some(85.0)"]);
    }

    #[tokio::test]
    async fn set_auto_switch_with_nothing_to_change_is_refused() {
        let service = Arc::new(Writable::two_accounts());
        let (hub, _scratch) = write_hub("set-auto-switch-nothing", &service);

        let reply = ask_as(&hub, set_auto_switch(None, None, "a1"), &AuthKind::Local).await;

        assert!(
            matches!(
                &reply,
                DaemonMessage::Error { code: Some(code), message, request_id: Some(id) }
                    if code == error_codes::ACCOUNT_REFUSED
                        && message == "nothing to change"
                        && id == "a1"
            ),
            "{reply:?}"
        );
        assert!(service.calls().is_empty(), "{:?}", service.calls());
    }

    fn set_auto_switch_rules(rules: AutoSwitchRules, id: &str) -> ClientMessage {
        ClientMessage::SetAutoSwitchRules {
            hysteresis_pct: rules.hysteresis_pct,
            cooldown_secs: rules.cooldown_secs,
            strategy: rules.strategy,
            rate_window_minutes: rules.rate_window_minutes,
            request_id: Some(id.to_owned()),
        }
    }

    #[tokio::test]
    async fn set_auto_switch_rules_answers_accounts_with_the_request_id() {
        let service = Arc::new(Writable::two_accounts());
        let (hub, _scratch) = write_hub("set-auto-switch-rules", &service);
        let rules = AutoSwitchRules {
            hysteresis_pct: Some(7.5),
            cooldown_secs: Some(600),
            strategy: Some(AutoSwitchStrategy::ConsumeFirst),
            rate_window_minutes: Some(120),
        };

        let reply = ask_as(&hub, set_auto_switch_rules(rules, "r1"), &AuthKind::Local).await;

        assert!(
            matches!(&reply, DaemonMessage::Accounts { request_id: Some(id), .. } if id == "r1"),
            "not the accounts reply to r1: {reply:?}"
        );
        let auto = match reply {
            DaemonMessage::Accounts { auto, .. } => auto,
            _ => None,
        }
        .expect("the answer carries the auto-switch state");
        assert!((auto.hysteresis_pct - 7.5).abs() < f64::EPSILON, "{auto:?}");
        assert_eq!(auto.cooldown_secs, 600);
        assert_eq!(auto.strategy, AutoSwitchStrategy::ConsumeFirst);
        assert_eq!(auto.rate_window_minutes, Some(120));
        assert_eq!(service.calls(), [format!("rules {rules:?}")]);
    }

    #[tokio::test]
    async fn set_auto_switch_rules_with_nothing_to_change_is_refused() {
        let service = Arc::new(Writable::two_accounts());
        let (hub, _scratch) = write_hub("set-auto-switch-rules-nothing", &service);

        let reply = ask_as(
            &hub,
            set_auto_switch_rules(AutoSwitchRules::default(), "r1"),
            &AuthKind::Local,
        )
        .await;

        assert!(
            matches!(
                &reply,
                DaemonMessage::Error { code: Some(code), message, request_id: Some(id) }
                    if code == error_codes::ACCOUNT_REFUSED
                        && message == "nothing to change"
                        && id == "r1"
            ),
            "{reply:?}"
        );
        assert!(service.calls().is_empty(), "{:?}", service.calls());
    }

    #[tokio::test]
    async fn a_failed_set_auto_switch_answers_its_account_code() {
        let cases = [
            (AccountsFailureKind::Refused, error_codes::ACCOUNT_REFUSED),
            (AccountsFailureKind::Busy, error_codes::ACCOUNT_BUSY),
            (
                AccountsFailureKind::Unsupported,
                error_codes::ACCOUNT_UNSUPPORTED,
            ),
            (AccountsFailureKind::Failed, error_codes::ACCOUNT_FAILED),
        ];
        for (kind, want) in cases {
            let service = Arc::new(Writable::two_accounts());
            service.fail_with(AccountsFailure {
                kind,
                code: "plugin_code".to_owned(),
                message: format!("{kind:?} said no"),
            });
            let (hub, _scratch) = write_hub("failed-set-auto-switch", &service);

            let reply = ask_as(
                &hub,
                set_auto_switch(None, Some(10.0), "a1"),
                &AuthKind::Local,
            )
            .await;

            let said = format!("{kind:?} said no");
            assert!(
                matches!(
                    &reply,
                    DaemonMessage::Error { code: Some(code), message, request_id: Some(id) }
                        if code == want && *message == said && id == "a1"
                ),
                "{kind:?}: {reply:?}"
            );
            assert_eq!(service.calls(), ["auto None Some(10.0)"], "{kind:?}");
        }
    }

    #[tokio::test]
    async fn a_manual_switch_broadcasts_a_switched_event() {
        let service = Arc::new(Writable::two_accounts());
        let (hub, _scratch) = write_hub("manual-switch-event", &service);
        let mut events = hub.state_events.subscribe();

        let reply = ask_as(&hub, switch(2, "w1"), &AuthKind::Local).await;

        let _ = accounts_of(reply, "w1");
        let event = events.try_recv().expect("the switch was broadcast");
        assert!(
            matches!(
                &event,
                crate::server::StateEvent::AccountEvent {
                    event: protocol::AccountEventKind::Switched { from: Some(1), to: 2, trigger }
                } if trigger == "manual"
            ),
            "{event:?}"
        );
    }
}
