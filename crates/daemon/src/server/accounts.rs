//! The accounts messages: the read-only half. `ListAccounts` is served on
//! every connection origin, since a snapshot carries no secret. The answer
//! comes from the plugin's account service, read off the async threads; a
//! host with no service, or one whose store cannot be read, answers no
//! accounts and no auto-switch state.
use super::Hub;
use super::connection::ConnCtx;
use agent_api::{
    AccountKindRow, AccountRow, AccountsConfig, AccountsService, AccountsSnapshot, AutoState,
    ExtraUsageRow, ModelLimitRow, UsageStatus, UsageWindowRow,
};
use protocol::{
    AccountExtraUsage, AccountKind, AccountModelLimit, AccountSnapshot, AccountUsageStatus,
    AccountUsageWindow, AutoSwitchState, ClientMessage, DaemonMessage,
};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use tracing::{info, warn};

/// The account service as the hub holds it: one call at a time.
pub type SharedAccounts = Arc<Mutex<Box<dyn AccountsService>>>;

/// The account service the registered plugins offer over this host's
/// account store, or `None` when the daemon runs elevated, the store's
/// folder cannot be resolved, or no plugin keeps accounts.
pub(crate) fn start_service() -> Option<SharedAccounts> {
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
    service
}

/// The service `ask` builds for the store at `dir`; `ask` is not called
/// without a folder (an elevated daemon has none).
pub(crate) fn service_for(
    dir: Option<PathBuf>,
    ask: impl FnOnce(&AccountsConfig) -> Option<Box<dyn AccountsService>>,
) -> Option<SharedAccounts> {
    let cfg = AccountsConfig::new(dir?);
    ask(&cfg).map(|service| Arc::new(Mutex::new(service)))
}

/// Handle the accounts messages `dispatch` routes here.
pub(super) async fn handle(hub: &Hub, msg: ClientMessage, ctx: &ConnCtx<'_>) {
    match msg {
        ClientMessage::ListAccounts { request_id } => {
            let snapshot = match &hub.accounts {
                Some(service) => read_snapshot(Arc::clone(service)).await,
                None => AccountsSnapshot::default(),
            };
            let _ = ctx.out_tx.send(DaemonMessage::Accounts {
                accounts: snapshot.accounts.iter().map(account_snapshot).collect(),
                auto: snapshot.auto.as_ref().and_then(auto_state),
                request_id,
            });
        }
        other => warn!(?other, "accounts::handle: routed a message it does not own"),
    }
}

/// `service`'s snapshot, read on a blocking thread; empty when it fails.
async fn read_snapshot(service: SharedAccounts) -> AccountsSnapshot {
    let read = tokio::task::spawn_blocking(move || {
        service
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .snapshot()
    })
    .await;
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

/// `value` when it is finite.
fn finite(value: f64) -> Option<f64> {
    value.is_finite().then_some(value)
}

/// `row` on the wire. A window or a per-model limit whose percentage is not
/// finite is left out, and an extra-usage limit that is not finite is `None`.
fn account_snapshot(row: &AccountRow) -> AccountSnapshot {
    let usage = &row.usage;
    AccountSnapshot {
        number: row.number,
        email: row.email.clone(),
        alias: row.alias.clone(),
        organization_name: row.org_name.clone(),
        kind: match row.kind {
            AccountKindRow::Oauth => AccountKind::Oauth,
            AccountKindRow::ApiKey => AccountKind::ApiKey,
        },
        active: row.active,
        disabled: row.disabled,
        status: usage_status(usage.status),
        five_hour: usage.five_hour.as_ref().and_then(usage_window),
        seven_day: usage.seven_day.as_ref().and_then(usage_window),
        model_limits: usage.model_limits.iter().filter_map(model_limit).collect(),
        extra_usage: usage.extra_usage.as_ref().map(extra_usage),
        fetched_at: usage.fetched_at,
        error: usage.error.clone(),
    }
}

fn usage_status(status: UsageStatus) -> AccountUsageStatus {
    match status {
        UsageStatus::Ok => AccountUsageStatus::Ok,
        UsageStatus::TokenExpired => AccountUsageStatus::TokenExpired,
        UsageStatus::ApiKey => AccountUsageStatus::ApiKey,
        UsageStatus::ReloginRequired => AccountUsageStatus::ReloginRequired,
        UsageStatus::ForeignCredential => AccountUsageStatus::ForeignCredential,
        UsageStatus::NoCredentials => AccountUsageStatus::NoCredentials,
        UsageStatus::StoreUnwritable => AccountUsageStatus::StoreUnwritable,
        UsageStatus::Unavailable => AccountUsageStatus::Unavailable,
    }
}

fn usage_window(window: &UsageWindowRow) -> Option<AccountUsageWindow> {
    Some(AccountUsageWindow {
        pct: finite(window.pct)?,
        resets_at: window.resets_at,
    })
}

fn model_limit(limit: &ModelLimitRow) -> Option<AccountModelLimit> {
    Some(AccountModelLimit {
        model: limit.model.clone(),
        pct: finite(limit.pct)?,
        resets_at: limit.resets_at,
    })
}

fn extra_usage(extra: &ExtraUsageRow) -> AccountExtraUsage {
    AccountExtraUsage {
        enabled: extra.enabled,
        user_disabled: extra.user_disabled,
        spend_limit_reached: extra.spend_limit_reached,
        limit: extra.limit.and_then(finite),
    }
}

/// `auto` on the wire, or `None` when its threshold or hysteresis is not
/// finite.
fn auto_state(auto: &AutoState) -> Option<AutoSwitchState> {
    Some(AutoSwitchState {
        enabled: auto.enabled,
        threshold_pct: finite(auto.threshold_pct)?,
        hysteresis_pct: finite(auto.hysteresis_pct)?,
        cooldown_secs: auto.cooldown_secs,
        paused_reason: auto.paused_reason.clone(),
        warm_up_done: auto.warm_up_done,
        last_decision: auto.last_decision.clone(),
        fleet_verdict: None,
    })
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
    use agent_api::{AccountsError, UsageRow};
    use std::sync::atomic::{AtomicBool, Ordering};

    /// An account service answering a fixed snapshot or error.
    struct FakeAccounts(Result<AccountsSnapshot, AccountsError>);

    impl AccountsService for FakeAccounts {
        fn snapshot(&self) -> Result<AccountsSnapshot, AccountsError> {
            self.0.clone()
        }
    }

    fn shared(answer: Result<AccountsSnapshot, AccountsError>) -> SharedAccounts {
        Arc::new(Mutex::new(Box::new(FakeAccounts(answer))))
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

    #[test]
    fn elevated_daemon_asks_for_no_accounts_service() {
        let asked = AtomicBool::new(false);
        let ask = |_: &AccountsConfig| -> Option<Box<dyn AccountsService>> {
            asked.store(true, Ordering::SeqCst);
            Some(Box::new(FakeAccounts(Ok(AccountsSnapshot::default()))))
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
