//! The accounts messages: the read-only half. `ListAccounts` is served on
//! every connection origin, since a snapshot carries no secret. The answer
//! is the snapshot the accounts driver last polled; before its first poll it
//! is the plugin's account service's snapshot, read off the async threads.
//! A host with no service, or one whose store cannot be read, answers no
//! accounts and no auto-switch state.
use super::Hub;
use super::connection::ConnCtx;
use crate::accounts::snapshot::{Wire, wire};
use crate::accounts::{Accounts, SharedAccounts};
use agent_api::{AccountsConfig, AccountsSnapshot};
use protocol::{ClientMessage, DaemonMessage};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{info, warn};

/// The accounts handle over the service the registered plugins offer for
/// this host's account store, or `None` when the daemon runs elevated, the
/// store's folder cannot be resolved, or no plugin keeps accounts.
pub(crate) fn start_service() -> Option<Arc<Accounts>> {
    let dir = match crate::paths::resolve_accounts_dir() {
        Ok(dir) => dir,
        Err(err) => {
            warn!(%err, "could not resolve the account store folder; accounts are off");
            return None;
        }
    };
    let service = service_for(dir, |cfg| {
        agent_registry::all()
            .iter()
            .find_map(|plugin| plugin.accounts(cfg))
    });
    info!(enabled = service.is_some(), "account service");
    service.map(Accounts::new)
}

/// The service `ask` builds for the store at `dir`; `ask` is not called
/// without a folder (an elevated daemon has none).
pub(crate) fn service_for(
    dir: Option<PathBuf>,
    ask: impl FnOnce(&AccountsConfig) -> Option<SharedAccounts>,
) -> Option<SharedAccounts> {
    let cfg = AccountsConfig::new(dir?);
    ask(&cfg)
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
        other => warn!(?other, "accounts::handle: routed a message it does not own"),
    }
}

/// `service`'s snapshot, read on a blocking thread; empty when it fails.
async fn read_snapshot(service: SharedAccounts) -> AccountsSnapshot {
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
        AccountKindRow, AccountRow, AccountsError, AccountsService, AutoState, ExtraUsageRow,
        ModelLimitRow, PollReport, PollScope, UsageRow, UsageStatus, UsageWindowRow,
    };
    use protocol::{AccountKind, AccountSnapshot, AccountUsageStatus, AutoSwitchState};
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
            paused_reason: None,
            warm_up_done: true,
            last_decision: Some("stay".to_owned()),
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
}
