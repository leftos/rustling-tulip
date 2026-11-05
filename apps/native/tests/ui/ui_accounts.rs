//! Accounts chip specs: the footer's chip naming This PC's account in use
//! and its two usage windows, the popover it opens with one row per saved
//! account, Switch and its pending and refused states, the auto-switch
//! toggle and threshold stepper, the rows' reasons and ages, and the rule
//! that drops a reply a newer broadcast overtook.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use gpui::{TestAppContext, px};
use protocol::{
    AccountSnapshot, AccountUsageStatus, AutoSwitchState, ClientMessage, DaemonMessage,
};
use support::{Harness, TestDir, account, accounts_reply, accounts_updated, auto_switch};

/// Whether the element tagged `selector` was drawn at some point.
fn drawn(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).origin.x >= px(0.0)
}

/// The request id of the newest `ListAccounts` the client sent since the
/// last read.
fn listed(h: &mut Harness<'_>) -> String {
    h.sent()
        .into_iter()
        .rev()
        .find_map(|msg| match msg {
            ClientMessage::ListAccounts { request_id } => request_id,
            _ => None,
        })
        .expect("the client asked for the accounts with a request id")
}

/// The request id and account of the newest `SwitchAccount` the client sent
/// since the last read.
fn switched(h: &mut Harness<'_>) -> (u32, String) {
    h.sent()
        .into_iter()
        .rev()
        .find_map(|msg| match msg {
            ClientMessage::SwitchAccount { number, request_id } => {
                request_id.map(|id| (number, id))
            }
            _ => None,
        })
        .expect("the client sent a SwitchAccount with a request id")
}

/// main in use at 34 % and 12 %, ops beside it.
fn pair() -> Vec<AccountSnapshot> {
    vec![
        account(1, "main").active().build(),
        account(2, "ops").five_hour(61.0).seven_day(40.0).build(),
    ]
}

/// The window reconnected to a daemon listing the accounts feature, its
/// first `ListAccounts` answered with `accounts`.
fn shown(h: &mut Harness<'_>, accounts: Vec<AccountSnapshot>) {
    h.reconnect_with_features(&[protocol::FEATURE_ACCOUNTS]);
    let id = listed(h);
    h.send(accounts_reply(accounts, None, &id));
}

/// [`shown`], then the popover opened from the chip, its own
/// `ListAccounts` answered with `accounts` too.
fn popover(h: &mut Harness<'_>, accounts: Vec<AccountSnapshot>) {
    popover_with_auto(h, accounts, None);
}

/// [`popover`], its own `ListAccounts` answered with `auto` as well.
fn popover_with_auto(
    h: &mut Harness<'_>,
    accounts: Vec<AccountSnapshot>,
    auto: Option<AutoSwitchState>,
) {
    shown(h, accounts.clone());
    h.click_on("footer-accounts");
    assert!(h.root(|root, _| root.accounts_open()), "the chip opens it");
    let id = listed(h);
    h.send(accounts_reply(accounts, auto, &id));
}

/// The newest `SetAutoSwitch` the client sent since the last read: its
/// fields and request id.
fn auto_sent(h: &mut Harness<'_>) -> (Option<bool>, Option<f64>, String) {
    h.sent()
        .into_iter()
        .rev()
        .find_map(|msg| match msg {
            ClientMessage::SetAutoSwitch {
                enabled,
                threshold_pct,
                request_id,
            } => request_id.map(|id| (enabled, threshold_pct, id)),
            _ => None,
        })
        .expect("the client sent a SetAutoSwitch with a request id")
}

fn chip_text(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.accounts_chip_text())
}

#[gpui::test]
fn the_chip_shows_the_active_account_and_its_bars(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::open(cx, &dir);
    shown(&mut h, pair());
    assert_eq!(chip_text(&mut h).as_deref(), Some("main · 5h 34% · 7d 12%"));
    assert!(drawn(&mut h, "footer-accounts"));
    assert!(drawn(&mut h, "footer-accounts-name=main"));
    assert!(drawn(&mut h, "footer-accounts-5h=34%"));
    assert!(drawn(&mut h, "footer-accounts-7d=12%"));

    h.send(accounts_updated(vec![account(1, "main").build()]));
    assert_eq!(chip_text(&mut h).as_deref(), Some("No active account"));
    assert!(drawn(&mut h, "footer-accounts-name=No active account"));
}

#[gpui::test]
fn no_chip_without_the_feature_or_accounts(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::open(cx, &dir);
    // The painted bounds outlive the element, so the empty list goes first:
    // the rows below would otherwise show for the frame between the new
    // connection's state and its Welcome.
    shown(&mut h, Vec::new());
    assert_eq!(chip_text(&mut h), None, "the host keeps no account");
    assert!(!drawn(&mut h, "footer-accounts"));

    h.reconnect_with_features(&[]);
    h.send(accounts_updated(pair()));
    assert_eq!(chip_text(&mut h), None, "the daemon does not list accounts");
    assert!(!drawn(&mut h, "footer-accounts"));
}

#[gpui::test]
fn the_chip_opens_and_closes_the_popover(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::open(cx, &dir);
    popover(&mut h, pair());
    assert!(drawn(&mut h, "accounts-popover"));
    assert!(drawn(&mut h, "accounts-name-1=main"));
    assert!(drawn(&mut h, "accounts-tag-1=In use"));
    assert!(drawn(&mut h, "accounts-name-2=ops"));
    assert!(drawn(&mut h, "accounts-5h-2=61%"));

    h.click_on("footer-accounts");
    assert!(
        !h.root(|root, _| root.accounts_open()),
        "a second click closes it"
    );

    h.click_on("footer-accounts");
    assert!(h.root(|root, _| root.accounts_open()));
    h.keys("escape");
    assert!(!h.root(|root, _| root.accounts_open()), "Esc closes it");

    h.click_on("daemon-pill");
    assert!(h.root(|root, _| root.daemon_flyout_open()));
    h.root_update(|root, _, cx| {
        root.toggle_accounts();
        cx.notify();
    });
    assert!(h.root(|root, _| root.accounts_open()));
    assert!(
        !h.root(|root, _| root.daemon_flyout_open()),
        "the two popovers never show together"
    );
    h.click_on("daemon-pill");
    assert!(
        !h.root(|root, _| root.accounts_open()),
        "a click outside closes it"
    );
}

#[gpui::test]
fn the_popover_stays_closed_after_a_reconnect(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::open(cx, &dir);
    popover(&mut h, pair());
    h.reconnect_with_features(&[protocol::FEATURE_ACCOUNTS]);
    assert!(
        !h.root(|root, _| root.accounts_open()),
        "the chip went with the rows"
    );
    let id = listed(&mut h);
    h.send(accounts_reply(pair(), None, &id));
    assert!(chip_text(&mut h).is_some(), "the chip is back");
    assert!(
        !h.root(|root, _| root.accounts_open()),
        "the popover does not reopen on its own"
    );
}

#[gpui::test]
fn switch_sends_switch_account_and_shows_switching(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::open(cx, &dir);
    popover(&mut h, pair());
    assert!(drawn(&mut h, "accounts-switch-2=Switch"));
    assert!(
        !drawn(&mut h, "accounts-switch-1"),
        "no Switch on the account in use"
    );

    h.click_on("accounts-switch-2");
    let (number, _id) = switched(&mut h);
    assert_eq!(number, 2);
    assert_eq!(h.root(|root, _| root.accounts_switching()), Some(2));
    assert!(drawn(&mut h, "accounts-switch-2=Switching…"));
    assert!(
        h.root(|root, _| root.accounts_open()),
        "the popover stays open"
    );
}

#[gpui::test]
fn a_relogin_row_shows_its_reason_and_no_switch(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::open(cx, &dir);
    let mut accounts = pair();
    accounts.push(
        account(5, "client")
            .status(AccountUsageStatus::ReloginRequired)
            .build(),
    );
    popover(&mut h, accounts);
    assert!(drawn(&mut h, "accounts-tag-5=sign-in refused"));
    assert!(drawn(
        &mut h,
        "accounts-reason-5=Log in again from Settings › Accounts"
    ));
    assert!(!drawn(&mut h, "accounts-switch-5"));
    assert!(!drawn(&mut h, "accounts-5h-5"), "no bars");
}

#[gpui::test]
fn a_stale_row_shows_its_age(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::open(cx, &dir);
    popover(
        &mut h,
        vec![
            account(1, "main").active().build(),
            account(2, "ops").fetched_minutes_ago(30).build(),
        ],
    );
    assert!(drawn(&mut h, "accounts-detail-2=ops@example.com · 30m ago"));
    assert!(
        drawn(&mut h, "accounts-detail-1=main@example.com"),
        "a fresh row shows no age"
    );
    assert!(h.root(|root, _| root.accounts_open()));
}

#[gpui::test]
fn a_read_only_store_row_shows_its_reason(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::open(cx, &dir);
    let mut accounts = pair();
    accounts.push(
        account(3, "lab")
            .status(AccountUsageStatus::StoreUnwritable)
            .build(),
    );
    popover(&mut h, accounts);
    assert!(drawn(&mut h, "accounts-reason-3=Store read-only"));
    assert!(!drawn(&mut h, "accounts-reason-2"));
}

#[gpui::test]
fn a_paused_auto_switch_shows_its_reason(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::open(cx, &dir);
    popover(&mut h, pair());
    assert!(
        !drawn(&mut h, "accounts-auto-toggle"),
        "no auto-switch state yet"
    );
    h.send(DaemonMessage::AccountsUpdated {
        accounts: pair(),
        auto: Some(auto_switch(Some("held by another engine"))),
    });
    assert!(drawn(
        &mut h,
        "accounts-paused=Auto-switch paused: held by another engine"
    ));
    assert!(drawn(&mut h, "accounts-auto-toggle=Auto-switch: On"));
}

#[gpui::test]
fn the_auto_switch_toggle_sends_only_enabled(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::open(cx, &dir);
    let mut auto = auto_switch(None);
    auto.enabled = false;
    popover_with_auto(&mut h, pair(), Some(auto));
    assert!(drawn(&mut h, "accounts-auto-toggle=Auto-switch: Off"));

    h.click_on("accounts-auto-toggle");
    let (enabled, threshold, _) = auto_sent(&mut h);
    assert_eq!(enabled, Some(true));
    assert_eq!(threshold, None, "a toggle sends no threshold");
}

#[gpui::test]
fn the_threshold_buttons_send_the_stepped_threshold(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::open(cx, &dir);
    let mut auto = auto_switch(None);
    auto.threshold_pct = 80.0;
    popover_with_auto(&mut h, pair(), Some(auto));
    assert!(drawn(&mut h, "accounts-auto-threshold=at 80%"));
    assert!(drawn(&mut h, "accounts-auto-down=−"));
    assert!(drawn(&mut h, "accounts-auto-up=+"));

    h.click_on("accounts-auto-up");
    let (enabled, threshold, _) = auto_sent(&mut h);
    assert_eq!(threshold, Some(85.0));
    assert_eq!(enabled, None, "a step sends no toggle");
}

#[gpui::test]
fn the_auto_switch_controls_wait_for_the_pending_write(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::open(cx, &dir);
    let mut auto = auto_switch(None);
    auto.threshold_pct = 80.0;
    popover_with_auto(&mut h, pair(), Some(auto.clone()));
    h.click_on("accounts-auto-up");
    let (_, threshold, id) = auto_sent(&mut h);
    assert_eq!(threshold, Some(85.0));

    h.click_on("accounts-auto-up");
    assert!(
        h.sent()
            .iter()
            .all(|msg| !matches!(msg, ClientMessage::SetAutoSwitch { .. })),
        "a second press while the write is pending sends nothing"
    );

    h.click_on("accounts-switch-2");
    assert!(
        h.sent()
            .iter()
            .all(|msg| !matches!(msg, ClientMessage::SwitchAccount { .. })),
        "the Switch button waits for the pending write"
    );

    auto.threshold_pct = 85.0;
    h.send(accounts_reply(pair(), Some(auto), &id));
    h.click_on("accounts-auto-up");
    let (_, threshold, _) = auto_sent(&mut h);
    assert_eq!(threshold, Some(90.0), "the controls work again");
}

#[gpui::test]
fn no_threshold_buttons_while_auto_switch_is_off(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::open(cx, &dir);
    let mut auto = auto_switch(None);
    auto.enabled = false;
    popover_with_auto(&mut h, pair(), Some(auto));
    assert!(drawn(&mut h, "accounts-auto-toggle=Auto-switch: Off"));
    assert!(!drawn(&mut h, "accounts-auto-up"));
    assert!(!drawn(&mut h, "accounts-auto-down"));
}

#[gpui::test]
fn list_accounts_is_asked_on_welcome_only_with_the_feature(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::open(cx, &dir);
    let asked = |sent: &[ClientMessage]| {
        sent.iter()
            .filter(|msg| matches!(msg, ClientMessage::ListAccounts { .. }))
            .count()
    };
    let first = h.sent();
    assert_eq!(asked(&first), 0, "the daemon does not list accounts");

    h.reconnect_with_features(&[protocol::FEATURE_ACCOUNTS]);
    let sent = h.sent();
    assert_eq!(asked(&sent), 1, "{sent:?}");
    assert!(
        sent.iter().any(|msg| matches!(
            msg,
            ClientMessage::ListAccounts {
                request_id: Some(_)
            }
        )),
        "the ask carries a request id"
    );

    h.reconnect_with_features(&[]);
    assert_eq!(
        asked(&h.sent()),
        0,
        "a reconnect without the feature asks nothing"
    );
}

#[gpui::test]
fn an_error_reply_to_a_switch_is_shown(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::open(cx, &dir);
    popover(&mut h, pair());
    h.click_on("accounts-switch-2");
    let (_, id) = switched(&mut h);
    h.send(DaemonMessage::Error {
        code: Some(protocol::error_codes::ACCOUNT_BUSY.to_owned()),
        message: "another account change is running".to_owned(),
        request_id: Some(id),
    });
    assert!(drawn(
        &mut h,
        "accounts-error=another account change is running"
    ));
    assert_eq!(
        h.root(|root, _| root.accounts_switching()),
        None,
        "the switch is no longer pending"
    );
    assert!(
        h.root(|root, _| root.toasts().is_empty()),
        "shown in the popover, not as a toast"
    );
}

#[gpui::test]
fn a_reply_after_a_newer_update_is_dropped_and_asked_again(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::open(cx, &dir);
    popover(&mut h, pair());
    h.click_on("accounts-switch-2");
    let (_, id) = switched(&mut h);
    let after = vec![
        account(1, "main").build(),
        account(2, "ops")
            .active()
            .five_hour(61.0)
            .seven_day(40.0)
            .build(),
    ];

    // A poll that finished before the switch, broadcast before its reply.
    h.send(accounts_updated(pair()));
    h.send(accounts_reply(after.clone(), None, &id));
    assert_eq!(
        chip_text(&mut h).as_deref(),
        Some("main · 5h 34% · 7d 12%"),
        "the reply an update overtook is dropped"
    );
    let again = listed(&mut h);
    h.send(accounts_reply(after, None, &again));
    assert_eq!(
        chip_text(&mut h).as_deref(),
        Some("ops · 5h 61% · 7d 40%"),
        "the fresh list lands"
    );
}
