//! Settings › Accounts specs: the tab This PC lists while its daemon lists
//! the accounts feature, one row per saved account with Switch, the alias
//! field, Up and Down and Remove, Add current login, the auto-switch block,
//! the refusal line, the controls waiting for a pending write, and the admin
//! controls a remote origin disables.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use std::time::Duration;

use gpui::{TestAppContext, px};
use protocol::{
    AccountLoginState, AccountSnapshot, AutoSwitchState, AutoSwitchStrategy, ClientMessage,
    ConnectionOrigin, DaemonMessage,
};
use rustling_tulip_native::target::{HostKey, Target};
use rustling_tulip_native::{Connection, NetEvent};
use support::{
    Fixture, Harness, PROTOCOL, TestDir, account, accounts_reply, accounts_updated, auto_switch,
    daemon_features, session,
};

/// A window on one session, its pane drawn and holding the keyboard,
/// nothing sent yet.
fn opened<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = Harness::with(cx, dir, &Fixture::single(session("s1").build()));
    h.answer_scrollback("s1", b"");
    h.advance(Duration::from_millis(400));
    h.sent();
    h
}

/// The tailnet host the remote-target spec holds beside This PC.
const REMOTE: &str = "box.tail1234.ts.net";

fn remote() -> Target {
    Target::tailnet(REMOTE, None)
}

/// The Settings host selector's row of the remote host.
fn remote_row() -> String {
    format!(
        "settings-host-{}",
        HostKey::of(&remote())
            .wire()
            .expect("a remote host has a wire key")
    )
}

/// Whether the element tagged `selector` was drawn at some point.
fn drawn(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).origin.x >= px(0.0)
}

/// Whether the Settings tab list offers Accounts.
fn tab_listed(h: &mut Harness<'_>) -> bool {
    h.root(|root, _| root.settings_tab_rows())
        .contains(&"Accounts")
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

/// main in use, ops and lab beside it, in the daemon's order.
fn trio() -> Vec<AccountSnapshot> {
    vec![
        account(1, "main").active().build(),
        account(2, "ops").five_hour(61.0).seven_day(40.0).build(),
        account(3, "lab").build(),
    ]
}

/// The window reconnected to a daemon listing the accounts feature, its
/// first `ListAccounts` answered with `accounts` and `auto`.
fn shown(h: &mut Harness<'_>, accounts: Vec<AccountSnapshot>, auto: Option<AutoSwitchState>) {
    h.reconnect_with_features(&[protocol::FEATURE_ACCOUNTS]);
    let id = listed(h);
    h.send(accounts_reply(accounts, auto, &id));
}

/// Settings open on the Accounts tab, the tab's own `ListAccounts`
/// answered with `accounts` and `auto`.
fn on_page(h: &mut Harness<'_>, accounts: Vec<AccountSnapshot>, auto: Option<AutoSwitchState>) {
    h.click_on("settings-open");
    h.click_on("settings-tab-accounts");
    assert_eq!(h.root(|root, _| root.settings_tab()), "Accounts");
    let id = listed(h);
    h.send(accounts_reply(accounts, auto, &id));
}

/// [`shown`], then [`on_page`].
fn page(h: &mut Harness<'_>, accounts: Vec<AccountSnapshot>, auto: Option<AutoSwitchState>) {
    shown(h, accounts.clone(), auto.clone());
    on_page(h, accounts, auto);
}

/// The account writes the client sent since the last read.
fn writes(h: &mut Harness<'_>) -> Vec<ClientMessage> {
    h.sent()
        .into_iter()
        .filter(|msg| {
            matches!(
                msg,
                ClientMessage::SwitchAccount { .. }
                    | ClientMessage::AddCurrentLogin { .. }
                    | ClientMessage::RemoveAccount { .. }
                    | ClientMessage::SetAccountAlias { .. }
                    | ClientMessage::SetAccountOrder { .. }
                    | ClientMessage::SetAutoSwitch { .. }
                    | ClientMessage::SetAutoSwitchRules { .. }
                    | ClientMessage::StartAccountLogin { .. }
                    | ClientMessage::CancelAccountLogin { .. }
            )
        })
        .collect()
}

/// The one account write the client sent since the last read.
#[track_caller]
fn one_write(h: &mut Harness<'_>) -> ClientMessage {
    let mut sent = writes(h);
    assert_eq!(sent.len(), 1, "one write: {sent:?}");
    sent.remove(0)
}

/// The request id `msg`, an account write, carries.
fn request_id(msg: &ClientMessage) -> String {
    match msg {
        ClientMessage::SwitchAccount { request_id, .. }
        | ClientMessage::AddCurrentLogin { request_id }
        | ClientMessage::RemoveAccount { request_id, .. }
        | ClientMessage::SetAccountAlias { request_id, .. }
        | ClientMessage::SetAccountOrder { request_id, .. }
        | ClientMessage::SetAutoSwitch { request_id, .. }
        | ClientMessage::SetAutoSwitchRules { request_id, .. }
        | ClientMessage::StartAccountLogin { request_id }
        | ClientMessage::CancelAccountLogin { request_id } => request_id.clone(),
        _ => None,
    }
    .expect("an account write carries a request id")
}

fn type_line(h: &mut Harness<'_>, text: &str) {
    let keys: Vec<String> = text
        .chars()
        .map(|c| {
            if c == ' ' {
                "space".to_owned()
            } else {
                c.to_string()
            }
        })
        .collect();
    h.keys(&keys.join(" "));
}

/// The Settings control holding the keyboard.
fn settings_focus(h: &mut Harness<'_>) -> Option<&'static str> {
    let root = h.root.clone();
    h.cx.update(|window, cx| root.read(cx).settings_focus(window, cx))
}

/// Scrolls the page to `selector` and clicks it.
fn press(h: &mut Harness<'_>, selector: &str) {
    h.scroll_into_view("settings-content", selector);
    h.click_on(selector);
}

/// The daemon's `AccountLogin` for `state`, carrying `message`, echoing
/// `id` when it is the direct answer to the requester.
fn login_msg(state: AccountLoginState, message: Option<&str>, id: Option<&str>) -> DaemonMessage {
    DaemonMessage::AccountLogin {
        state,
        session_id: Some("login-1".to_owned()),
        message: message.map(str::to_owned),
        request_id: id.map(str::to_owned),
    }
}

#[gpui::test]
fn the_accounts_tab_is_listed_only_on_this_pc_with_the_feature(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    assert!(!tab_listed(&mut h), "the daemon does not list accounts");

    let mut waiting = Connection::for_target(Target::Local);
    waiting.on_connecting();
    h.event(NetEvent::State(waiting));
    assert!(!tab_listed(&mut h), "no tab before This PC's Welcome");

    h.reconnect_with_features(&[protocol::FEATURE_ACCOUNTS]);
    let rows = h.root(|root, _| root.settings_tab_rows());
    let remote_access = rows
        .iter()
        .position(|row| *row == "Remote access")
        .expect("This PC lists Remote access");
    assert_eq!(
        rows.get(remote_access + 1),
        Some(&"Accounts"),
        "right after Remote access: {rows:?}"
    );

    let mut host = h.host(&remote());
    host.connect();
    host.sent();
    h.click_on("settings-open");
    h.click_on(&remote_row());
    assert!(
        !tab_listed(&mut h),
        "a remote target's daemon is not this client's"
    );
    h.click_on("settings-host-local");
    assert!(tab_listed(&mut h), "This PC lists it again");
}

#[gpui::test]
fn the_accounts_tab_goes_when_the_feature_goes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    page(&mut h, trio(), None);
    h.reconnect_with_features(&[]);
    assert!(!tab_listed(&mut h));
    assert_eq!(
        h.root(|root, _| root.settings_tab()),
        "General",
        "the page gives way to General"
    );
}

#[gpui::test]
fn opening_the_accounts_tab_asks_for_the_list(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    shown(&mut h, trio(), None);
    h.click_on("settings-open");
    h.sent();
    h.click_on("settings-tab-accounts");
    let asked = h
        .sent()
        .into_iter()
        .filter(|msg| {
            matches!(
                msg,
                ClientMessage::ListAccounts {
                    request_id: Some(_)
                }
            )
        })
        .count();
    assert_eq!(asked, 1, "the tab asks once, with a request id");
}

#[gpui::test]
fn the_page_lists_a_row_per_account(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    page(&mut h, trio(), None);
    assert!(drawn(&mut h, "settings-accounts-name-1=main"));
    assert!(drawn(&mut h, "settings-accounts-tag-1=In use"));
    assert!(drawn(&mut h, "settings-accounts-name-2=ops"));
    assert!(drawn(&mut h, "settings-accounts-5h-2=61%"));
    assert!(drawn(&mut h, "settings-accounts-7d-2=40%"));
    assert!(drawn(&mut h, "settings-accounts-name-3=lab"));
    assert!(drawn(&mut h, "settings-accounts-add-login"));
    assert!(
        drawn(
            &mut h,
            "settings-accounts-auto-missing=Auto-switch is not available on this host"
        ),
        "no auto-switch state: the note"
    );
    assert!(!drawn(&mut h, "settings-accounts-auto"), "and no toggle");
    assert!(!drawn(&mut h, "settings-accounts-forecast"));
}

#[gpui::test]
fn switch_on_the_page_sends_switch_account(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    page(&mut h, trio(), None);
    assert!(
        !drawn(&mut h, "settings-accounts-switch-1"),
        "no Switch on the account in use"
    );
    press(&mut h, "settings-accounts-switch-2");
    let sent = one_write(&mut h);
    assert!(
        matches!(sent, ClientMessage::SwitchAccount { number: 2, .. }),
        "{sent:?}"
    );
    assert_eq!(h.root(|root, _| root.accounts_switching()), Some(2));
}

#[gpui::test]
fn remove_sends_remove_account_and_the_active_row_says_switch_first(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    page(&mut h, trio(), None);
    assert!(drawn(
        &mut h,
        "settings-accounts-switch-first-1=Switch first"
    ));
    assert!(
        !drawn(&mut h, "settings-accounts-remove-1"),
        "no Remove on the account in use"
    );
    press(&mut h, "settings-accounts-remove-3");
    let sent = one_write(&mut h);
    assert!(
        matches!(sent, ClientMessage::RemoveAccount { number: 3, .. }),
        "{sent:?}"
    );
}

#[gpui::test]
fn the_alias_field_sends_set_account_alias_on_enter(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    page(&mut h, trio(), None);
    press(&mut h, "settings-accounts-alias-2");
    assert_eq!(settings_focus(&mut h), Some("settings-accounts-alias-2"));
    h.keys("enter");
    assert!(writes(&mut h).is_empty(), "the alias did not change");

    h.keys("ctrl-a");
    type_line(&mut h, "work");
    h.keys("enter");
    let sent = one_write(&mut h);
    assert!(
        matches!(
            &sent,
            ClientMessage::SetAccountAlias {
                number: 2,
                alias: Some(alias),
                ..
            } if alias == "work"
        ),
        "{sent:?}"
    );

    h.send(accounts_reply(trio(), None, &request_id(&sent)));
    press(&mut h, "settings-accounts-alias-3");
    h.keys("ctrl-a backspace enter");
    let sent = one_write(&mut h);
    assert!(
        matches!(
            sent,
            ClientMessage::SetAccountAlias {
                number: 3,
                alias: None,
                ..
            }
        ),
        "a blank alias clears it: {sent:?}"
    );
}

#[gpui::test]
fn up_sends_the_moved_order(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    page(&mut h, trio(), None);
    assert!(
        !drawn(&mut h, "settings-accounts-up-1"),
        "no Up on the first"
    );
    assert!(
        !drawn(&mut h, "settings-accounts-down-3"),
        "no Down on the last"
    );
    press(&mut h, "settings-accounts-up-2");
    let sent = one_write(&mut h);
    assert!(
        matches!(
            &sent,
            ClientMessage::SetAccountOrder { order, .. } if order == &[2, 1, 3]
        ),
        "the full order, ops moved up one place: {sent:?}"
    );
}

#[gpui::test]
fn add_current_login_sends_the_message(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    page(&mut h, trio(), None);
    assert!(drawn(&mut h, "settings-accounts-add-login-note"));
    press(&mut h, "settings-accounts-add-login");
    let sent = one_write(&mut h);
    assert!(
        matches!(sent, ClientMessage::AddCurrentLogin { .. }),
        "{sent:?}"
    );
}

#[gpui::test]
fn the_auto_switch_toggle_sends_set_auto_switch(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    let mut auto = auto_switch(None);
    auto.enabled = false;
    page(&mut h, trio(), Some(auto));
    assert!(
        !drawn(&mut h, "settings-accounts-auto-missing"),
        "the state is here"
    );
    assert!(
        !drawn(&mut h, "settings-accounts-forecast"),
        "no verdict, no forecast"
    );
    press(&mut h, "settings-accounts-auto");
    let sent = one_write(&mut h);
    assert!(
        matches!(
            sent,
            ClientMessage::SetAutoSwitch {
                enabled: Some(true),
                threshold_pct: None,
                ..
            }
        ),
        "{sent:?}"
    );
}

#[gpui::test]
fn the_strategy_choice_sends_set_auto_switch_rules(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    page(&mut h, trio(), Some(auto_switch(None)));
    press(&mut h, "settings-accounts-strategy-consume-first");
    let sent = one_write(&mut h);
    assert!(
        matches!(
            sent,
            ClientMessage::SetAutoSwitchRules {
                hysteresis_pct: None,
                cooldown_secs: None,
                strategy: Some(AutoSwitchStrategy::ConsumeFirst),
                rate_window_minutes: None,
                ..
            }
        ),
        "{sent:?}"
    );
}

#[gpui::test]
fn the_forecast_window_choice_sends_its_minutes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    page(&mut h, trio(), Some(auto_switch(None)));
    press(&mut h, "settings-accounts-window-120");
    let sent = one_write(&mut h);
    assert!(
        matches!(
            sent,
            ClientMessage::SetAutoSwitchRules {
                hysteresis_pct: None,
                cooldown_secs: None,
                strategy: None,
                rate_window_minutes: Some(120),
                ..
            }
        ),
        "{sent:?}"
    );
}

#[gpui::test]
fn a_refused_write_shows_on_the_page(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    page(&mut h, trio(), None);
    press(&mut h, "settings-accounts-switch-2");
    let sent = one_write(&mut h);
    h.send(protocol::DaemonMessage::Error {
        code: Some(protocol::error_codes::ACCOUNT_BUSY.to_owned()),
        message: "another account change is running".to_owned(),
        request_id: Some(request_id(&sent)),
    });
    assert!(drawn(
        &mut h,
        "settings-accounts-error=another account change is running"
    ));
}

#[gpui::test]
fn the_write_controls_wait_for_the_pending_write(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    page(&mut h, trio(), Some(auto_switch(None)));
    press(&mut h, "settings-accounts-switch-2");
    let sent = one_write(&mut h);

    for selector in [
        "settings-accounts-switch-3",
        "settings-accounts-remove-3",
        "settings-accounts-up-2",
        "settings-accounts-add-login",
        "settings-accounts-auto",
        "settings-accounts-strategy-consume-first",
    ] {
        press(&mut h, selector);
        assert!(
            writes(&mut h).is_empty(),
            "{selector} waits for the pending write"
        );
    }

    h.send(accounts_reply(
        trio(),
        Some(auto_switch(None)),
        &request_id(&sent),
    ));
    press(&mut h, "settings-accounts-remove-3");
    assert!(
        matches!(
            one_write(&mut h),
            ClientMessage::RemoveAccount { number: 3, .. }
        ),
        "the controls work again"
    );
}

#[gpui::test]
fn the_admin_controls_are_disabled_for_a_remote_origin(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    let mut features = daemon_features();
    features.push(protocol::FEATURE_ACCOUNTS.to_owned());
    let mut conn = Connection::for_target(Target::Local);
    conn.on_connecting();
    conn.on_socket_open(4242);
    conn.on_welcome(PROTOCOL, features, Some(ConnectionOrigin::Remote));
    h.event(NetEvent::State(conn));
    h.send_welcome(PROTOCOL);
    let id = listed(&mut h);
    h.send(accounts_reply(trio(), Some(auto_switch(None)), &id));
    on_page(&mut h, trio(), Some(auto_switch(None)));

    for selector in [
        "settings-accounts-remove-3",
        "settings-accounts-up-2",
        "settings-accounts-down-2",
        "settings-accounts-add-login",
    ] {
        press(&mut h, selector);
        assert!(writes(&mut h).is_empty(), "{selector} is disabled");
    }
    press(&mut h, "settings-accounts-alias-2");
    h.keys("ctrl-a");
    type_line(&mut h, "work");
    h.keys("enter");
    assert!(writes(&mut h).is_empty(), "the alias field takes no edit");

    press(&mut h, "settings-accounts-switch-2");
    let sent = one_write(&mut h);
    assert!(
        matches!(sent, ClientMessage::SwitchAccount { number: 2, .. }),
        "Switch is served on any origin: {sent:?}"
    );
    h.send(accounts_reply(
        trio(),
        Some(auto_switch(None)),
        &request_id(&sent),
    ));
    press(&mut h, "settings-accounts-auto");
    assert!(
        matches!(
            one_write(&mut h),
            ClientMessage::SetAutoSwitch {
                enabled: Some(false),
                ..
            }
        ),
        "the auto-switch block stays live"
    );
}

#[gpui::test]
fn an_alias_field_is_read_only_while_a_write_is_pending(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    page(&mut h, trio(), None);
    press(&mut h, "settings-accounts-switch-2");
    let sent = one_write(&mut h);

    press(&mut h, "settings-accounts-alias-3");
    h.keys("ctrl-a");
    type_line(&mut h, "x");
    h.keys("enter");
    assert!(writes(&mut h).is_empty(), "the field waits for the write");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-accounts-alias-3"),
        "the field keeps the keyboard"
    );

    h.send(accounts_reply(trio(), None, &request_id(&sent)));
    h.keys("enter");
    assert!(
        writes(&mut h).is_empty(),
        "nothing was typed into the field while it waited"
    );
}

#[gpui::test]
fn closing_settings_with_ok_sends_a_typed_alias(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    page(&mut h, trio(), None);
    press(&mut h, "settings-accounts-alias-2");
    h.keys("ctrl-a");
    type_line(&mut h, "work");
    h.click_on("settings-ok");
    let sent = one_write(&mut h);
    assert!(
        matches!(
            &sent,
            ClientMessage::SetAccountAlias {
                number: 2,
                alias: Some(alias),
                ..
            } if alias == "work"
        ),
        "{sent:?}"
    );
    assert!(
        !h.root(|root, _| root.settings_open()),
        "OK closes Settings"
    );
}

#[gpui::test]
fn leaving_an_untouched_alias_field_does_not_undo_a_rename_from_elsewhere(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    page(&mut h, trio(), None);
    press(&mut h, "settings-accounts-alias-2");
    let mut renamed = trio();
    renamed[1] = account(2, "work").five_hour(61.0).seven_day(40.0).build();
    h.send(accounts_updated(renamed));
    // The test window does not report a blur, so Enter, which commits as
    // leaving does, stands for it.
    h.keys("enter");
    press(&mut h, "settings-accounts-alias-3");
    assert!(
        writes(&mut h).is_empty(),
        "the field the user never edited sends nothing"
    );
}

/// The running login's status line, as the page shows it.
const LOGIN_RUNNING_LINE: &str =
    "Finish the login in its terminal session, “Log in a new account”, in the sidebar.";

#[gpui::test]
fn log_in_sends_a_start_and_shows_the_running_line(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    page(&mut h, trio(), None);
    assert!(
        !drawn(&mut h, "settings-accounts-cancel-login"),
        "no Cancel before a login runs"
    );
    press(&mut h, "settings-accounts-login");
    let sent = one_write(&mut h);
    assert!(
        matches!(sent, ClientMessage::StartAccountLogin { .. }),
        "{sent:?}"
    );
    h.send(login_msg(
        AccountLoginState::Started,
        None,
        Some(&request_id(&sent)),
    ));
    assert!(drawn(
        &mut h,
        &format!("settings-accounts-login-status={LOGIN_RUNNING_LINE}")
    ));
    assert!(
        drawn(&mut h, "settings-accounts-cancel-login"),
        "Cancel shows while the login runs"
    );
    press(&mut h, "settings-accounts-login");
    assert!(
        writes(&mut h).is_empty(),
        "Log in waits for the running login"
    );
}

#[gpui::test]
fn cancel_login_sends_a_cancel_and_shows_cancelled(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    page(&mut h, trio(), None);
    press(&mut h, "settings-accounts-login");
    let sent = one_write(&mut h);
    h.send(login_msg(
        AccountLoginState::Started,
        None,
        Some(&request_id(&sent)),
    ));
    press(&mut h, "settings-accounts-cancel-login");
    let cancel = one_write(&mut h);
    assert!(
        matches!(cancel, ClientMessage::CancelAccountLogin { .. }),
        "{cancel:?}"
    );
    h.send(login_msg(
        AccountLoginState::Cancelled,
        None,
        Some(&request_id(&cancel)),
    ));
    assert!(drawn(
        &mut h,
        "settings-accounts-login-status=The login was cancelled."
    ));
    press(&mut h, "settings-accounts-login");
    assert!(
        matches!(one_write(&mut h), ClientMessage::StartAccountLogin { .. }),
        "the run ended and Log in is live again"
    );
}

#[gpui::test]
fn a_failed_login_shows_its_message_and_screen_lines(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    page(&mut h, trio(), None);
    press(&mut h, "settings-accounts-login");
    let sent = one_write(&mut h);
    h.send(login_msg(
        AccountLoginState::Started,
        None,
        Some(&request_id(&sent)),
    ));
    h.send(login_msg(
        AccountLoginState::Failed,
        Some("the login ended\nscreen line two"),
        None,
    ));
    assert!(drawn(
        &mut h,
        "settings-accounts-login-status=The login failed: the login ended"
    ));
    assert!(
        drawn(&mut h, "settings-accounts-login-status=screen line two"),
        "the screen's later lines show too"
    );
    press(&mut h, "settings-accounts-login");
    assert!(
        matches!(one_write(&mut h), ClientMessage::StartAccountLogin { .. }),
        "the failed run no longer holds Log in"
    );
}

#[gpui::test]
fn a_succeeded_login_reenables_the_button(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    page(&mut h, trio(), None);
    press(&mut h, "settings-accounts-login");
    let sent = one_write(&mut h);
    h.send(login_msg(
        AccountLoginState::Started,
        None,
        Some(&request_id(&sent)),
    ));
    h.send(login_msg(AccountLoginState::Succeeded, None, None));
    assert!(drawn(
        &mut h,
        "settings-accounts-login-status=The new account was added."
    ));
    press(&mut h, "settings-accounts-login");
    let sent = one_write(&mut h);
    assert!(
        matches!(sent, ClientMessage::StartAccountLogin { .. }),
        "the button works again: {sent:?}"
    );
}

#[gpui::test]
fn a_busy_start_shows_the_refusal(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    page(&mut h, trio(), None);
    press(&mut h, "settings-accounts-login");
    let sent = one_write(&mut h);
    h.send(DaemonMessage::Error {
        code: Some(protocol::error_codes::ACCOUNT_BUSY.to_owned()),
        message: "another login is running".to_owned(),
        request_id: Some(request_id(&sent)),
    });
    assert!(drawn(
        &mut h,
        "settings-accounts-error=another login is running"
    ));
    press(&mut h, "settings-accounts-login");
    assert!(
        matches!(one_write(&mut h), ClientMessage::StartAccountLogin { .. }),
        "the button is live again after the refusal"
    );
}

#[gpui::test]
fn login_controls_are_locked_on_a_remote_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    let mut features = daemon_features();
    features.push(protocol::FEATURE_ACCOUNTS.to_owned());
    let mut conn = Connection::for_target(Target::Local);
    conn.on_connecting();
    conn.on_socket_open(4242);
    conn.on_welcome(PROTOCOL, features, Some(ConnectionOrigin::Remote));
    h.event(NetEvent::State(conn));
    h.send_welcome(PROTOCOL);
    let id = listed(&mut h);
    h.send(accounts_reply(trio(), Some(auto_switch(None)), &id));
    on_page(&mut h, trio(), Some(auto_switch(None)));

    press(&mut h, "settings-accounts-login");
    assert!(
        writes(&mut h).is_empty(),
        "Log in is locked on a remote origin"
    );
    h.send(login_msg(AccountLoginState::Started, None, None));
    press(&mut h, "settings-accounts-cancel-login");
    assert!(writes(&mut h).is_empty(), "Cancel login is locked too");
}
