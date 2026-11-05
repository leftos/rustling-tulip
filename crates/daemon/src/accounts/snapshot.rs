//! The accounts snapshot on the wire. Every number the wire carries is
//! finite: serde writes a NaN or an infinity as `null`, which the client
//! cannot decode, so the whole frame would be lost. A window or a per-model
//! limit whose percentage is not finite is left out, an extra-usage limit
//! that is not finite is `None`, and an auto-switch state whose threshold or
//! hysteresis is not finite is dropped; so is a fleet verdict whose
//! remaining allowance or rate is not finite, while its state stays.
use agent_api::{
    AccountKindRow, AccountRow, AccountsSnapshot, AutoState, ExtraUsageRow, FleetRow,
    FleetWindowRow, ModelLimitRow, UsageStatus, UsageWindowRow,
};
use protocol::{
    AccountExtraUsage, AccountKind, AccountModelLimit, AccountSnapshot, AccountUsageStatus,
    AccountUsageWindow, AutoSwitchState, FleetVerdict, FleetWindow,
};

/// A snapshot as the wire carries it: what `Accounts` and `AccountsUpdated`
/// send.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Wire {
    pub(crate) accounts: Vec<AccountSnapshot>,
    pub(crate) auto: Option<AutoSwitchState>,
}

/// `snapshot` on the wire.
pub(crate) fn wire(snapshot: &AccountsSnapshot) -> Wire {
    Wire {
        accounts: snapshot.accounts.iter().map(account_snapshot).collect(),
        auto: snapshot.auto.as_ref().and_then(auto_state),
    }
}

/// `value` when it is finite.
fn finite(value: f64) -> Option<f64> {
    value.is_finite().then_some(value)
}

/// `row` on the wire. A window or a per-model limit whose percentage is not
/// finite is left out, and an extra-usage limit that is not finite is `None`.
pub(crate) fn account_snapshot(row: &AccountRow) -> AccountSnapshot {
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
        strategy: auto.strategy,
        rate_window_minutes: auto.rate_window_minutes,
        paused_reason: auto.paused_reason.clone(),
        warm_up_done: auto.warm_up_done,
        last_decision: auto.last_decision.clone(),
        fleet_verdict: auto.fleet.as_ref().and_then(fleet_verdict),
    })
}

/// `row` on the wire, or `None` when its remaining allowance or rate is not
/// finite.
fn fleet_verdict(row: &FleetRow) -> Option<FleetVerdict> {
    Some(FleetVerdict {
        window: fleet_window(row.window),
        members: row.members,
        accounts: row.accounts,
        remaining: finite(row.remaining)?,
        rate: finite(row.rate)?,
        rate_window: fleet_window(row.rate_window),
        runs_dry: row.runs_dry,
        blocked_at: row.blocked_at,
        resumes_at: row.resumes_at,
        first_reset: row.first_reset,
    })
}

fn fleet_window(window: FleetWindowRow) -> FleetWindow {
    match window {
        FleetWindowRow::FiveHour => FleetWindow::FiveHour,
        FleetWindowRow::SevenDay => FleetWindow::SevenDay,
    }
}

/// Rows the accounts tests build snapshots from.
#[cfg(test)]
pub(crate) mod test_support {
    use agent_api::{
        AccountKindRow, AccountRow, AutoState, FleetRow, FleetWindowRow, UsageRow, UsageStatus,
        UsageWindowRow,
    };
    use chrono::{DateTime, TimeDelta, Utc};

    /// A fixed time for the forecast's stamps.
    pub(crate) fn at() -> DateTime<Utc> {
        DateTime::<Utc>::UNIX_EPOCH + TimeDelta::seconds(1_800_000_000)
    }

    /// A fleet forecast over two half-spent accounts that lasts the week.
    pub(crate) fn fleet() -> FleetRow {
        FleetRow {
            window: FleetWindowRow::FiveHour,
            members: 2,
            accounts: 3,
            remaining: 120.5,
            rate: 9.5,
            rate_window: FleetWindowRow::SevenDay,
            runs_dry: false,
            blocked_at: Some(at() + TimeDelta::seconds(60)),
            resumes_at: Some(at() + TimeDelta::seconds(120)),
            first_reset: Some(at()),
        }
    }

    /// Account `number`, active when it is 1, its five-hour window at `pct`.
    pub(crate) fn row(number: u32, pct: f64) -> AccountRow {
        AccountRow {
            number,
            email: format!("user{number}@example.com"),
            alias: None,
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

    /// A running engine switching at `threshold_pct`.
    pub(crate) fn auto(threshold_pct: f64) -> AutoState {
        AutoState {
            enabled: true,
            threshold_pct,
            hysteresis_pct: 5.0,
            cooldown_secs: 300,
            strategy: protocol::AutoSwitchStrategy::Best,
            rate_window_minutes: None,
            paused_reason: None,
            warm_up_done: true,
            last_decision: None,
            fleet: None,
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test fails with the message of the precondition it lost"
)]
mod tests {
    use super::test_support::{auto, fleet, row};
    use super::*;
    use protocol::FleetWindow;

    const NON_FINITE: [f64; 3] = [f64::NAN, f64::INFINITY, f64::NEG_INFINITY];

    #[test]
    fn a_non_finite_window_is_left_out() {
        for value in NON_FINITE {
            let mut odd = row(1, value);
            odd.usage.seven_day = Some(UsageWindowRow {
                pct: value,
                resets_at: None,
            });

            let account = account_snapshot(&odd);

            assert_eq!(account.five_hour, None, "{value}");
            assert_eq!(account.seven_day, None, "{value}");
            serde_json::to_string(&account).expect("every number left is finite");
        }
        let kept = account_snapshot(&row(1, 42.0));
        assert_eq!(kept.five_hour.map(|window| window.pct), Some(42.0));
    }

    #[test]
    fn a_non_finite_model_limit_is_left_out() {
        for value in NON_FINITE {
            let mut odd = row(1, 10.0);
            odd.usage.model_limits = vec![
                ModelLimitRow {
                    model: "big".to_owned(),
                    pct: value,
                    resets_at: None,
                },
                ModelLimitRow {
                    model: "small".to_owned(),
                    pct: 12.0,
                    resets_at: None,
                },
            ];

            let account = account_snapshot(&odd);

            let models: Vec<&str> = account
                .model_limits
                .iter()
                .map(|limit| limit.model.as_str())
                .collect();
            assert_eq!(models, ["small"], "{value}");
        }
    }

    #[test]
    fn a_non_finite_extra_limit_is_none() {
        for value in NON_FINITE {
            let mut odd = row(1, 10.0);
            odd.usage.extra_usage = Some(ExtraUsageRow {
                enabled: true,
                user_disabled: false,
                spend_limit_reached: true,
                limit: Some(value),
            });

            let account = account_snapshot(&odd);

            let extra = account.extra_usage.expect("the extra usage stays");
            assert!(extra.enabled && extra.spend_limit_reached, "{value}");
            assert_eq!(extra.limit, None, "{value}");
        }
    }

    #[test]
    fn a_non_finite_auto_threshold_drops_the_auto_state() {
        for value in NON_FINITE {
            let threshold = AccountsSnapshot {
                accounts: Vec::new(),
                auto: Some(auto(value)),
            };
            assert_eq!(wire(&threshold).auto, None, "threshold {value}");

            let mut odd = auto(95.0);
            odd.hysteresis_pct = value;
            let hysteresis = AccountsSnapshot {
                accounts: Vec::new(),
                auto: Some(odd),
            };
            assert_eq!(wire(&hysteresis).auto, None, "hysteresis {value}");
        }
        let kept = wire(&AccountsSnapshot {
            accounts: Vec::new(),
            auto: Some(auto(95.0)),
        });
        assert!(kept.auto.is_some(), "a finite state stays");
    }

    #[test]
    fn a_fleet_row_maps_onto_the_wire_verdict() {
        let mut state = auto(95.0);
        state.fleet = Some(fleet());

        let wired = wire(&AccountsSnapshot {
            accounts: Vec::new(),
            auto: Some(state),
        });

        let sent = wired.auto.expect("the auto state stays");
        let verdict = sent
            .fleet_verdict
            .as_ref()
            .expect("the fleet verdict is carried");
        let row = fleet();
        assert_eq!(verdict.window, FleetWindow::FiveHour);
        assert_eq!(verdict.rate_window, FleetWindow::SevenDay);
        assert_eq!(verdict.members, row.members);
        assert_eq!(verdict.accounts, row.accounts);
        assert_eq!(verdict.remaining, row.remaining);
        assert_eq!(verdict.rate, row.rate);
        assert_eq!(verdict.runs_dry, row.runs_dry);
        assert_eq!(verdict.blocked_at, row.blocked_at);
        assert_eq!(verdict.resumes_at, row.resumes_at);
        assert_eq!(verdict.first_reset, row.first_reset);
        serde_json::to_string(&sent).expect("every number left is finite");
    }

    #[test]
    fn a_non_finite_fleet_rate_drops_only_the_verdict() {
        for value in NON_FINITE {
            let mut odd = auto(95.0);
            let mut row = fleet();
            row.rate = value;
            odd.fleet = Some(row);
            let sent = wire(&AccountsSnapshot {
                accounts: Vec::new(),
                auto: Some(odd),
            })
            .auto
            .expect("the auto state stays");
            assert_eq!(sent.fleet_verdict, None, "rate {value}");
            assert!(sent.enabled, "the state keeps its switch: {sent:?}");
            assert!((sent.threshold_pct - 95.0).abs() < f64::EPSILON, "{sent:?}");

            let mut odd = auto(95.0);
            let mut row = fleet();
            row.remaining = value;
            odd.fleet = Some(row);
            let sent = wire(&AccountsSnapshot {
                accounts: Vec::new(),
                auto: Some(odd),
            })
            .auto
            .expect("the auto state stays");
            assert_eq!(sent.fleet_verdict, None, "remaining {value}");
            assert!(sent.enabled, "the state keeps its switch: {sent:?}");
        }

        let mut state = auto(95.0);
        state.fleet = Some(fleet());
        let sent = wire(&AccountsSnapshot {
            accounts: Vec::new(),
            auto: Some(state),
        });
        assert!(
            sent.auto
                .expect("the auto state stays")
                .fleet_verdict
                .is_some(),
            "a finite verdict stays"
        );
    }
}
