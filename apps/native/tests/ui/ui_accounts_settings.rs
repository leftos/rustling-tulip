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
        "a remote host whose daemon does not list accounts offers no tab"
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

/// An email wider than a row, so its detail line wraps.
const LONG: &str = "a-very-long-mailbox-name-that-keeps-going-and-going-and-going-and-going-and-going-and-going@a-long-company-domain.example.com";

#[gpui::test]
fn a_long_email_wraps_on_the_page(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    page(
        &mut h,
        vec![account(1, "main").email(LONG).active().build()],
        None,
    );
    let name = h.bounds("settings-accounts-name-1=main");
    let detail = h.bounds(&format!("settings-accounts-detail-1={LONG}"));
    assert!(
        detail.size.height > name.size.height * 1.5,
        "the detail {detail:?} wraps past the name {name:?}"
    );
}

#[gpui::test]
fn a_short_email_stays_on_one_line_on_the_page(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    page(
        &mut h,
        vec![
            account(1, "main").active().build(),
            account(2, "spare").build(),
        ],
        None,
    );
    let name = h.bounds("settings-accounts-name-2=spare");
    let detail = h.bounds("settings-accounts-detail-2=spare@example.com");
    assert!(
        detail.size.height <= name.size.height * 1.2,
        "the detail {detail:?} stays on the name's line {name:?}"
    );
}

#[gpui::test]
fn the_page_shows_a_model_limit_in_use(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    page(
        &mut h,
        vec![
            account(1, "main")
                .active()
                .model_limit("Fable", 37.0)
                .build(),
            account(2, "ops").model_limit("Fable", 0.0).build(),
        ],
        None,
    );
    assert!(
        drawn(&mut h, "settings-accounts-fable-7d-1=37%"),
        "a per-model limit in use draws its line"
    );
    assert!(
        !drawn(&mut h, "settings-accounts-fable-7d-2=0%"),
        "a per-model limit at 0 % draws no line"
    );
}

#[gpui::test]
fn settings_accounts_lines_up_usage_lines_of_any_label(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    page(
        &mut h,
        vec![
            account(1, "main")
                .active()
                .seven_day(40.0)
                .model_limit("Fable", 37.0)
                .build(),
        ],
        None,
    );
    assert!(
        drawn(&mut h, "settings-accounts-7d-1=40%"),
        "the seven-day line is drawn"
    );
    assert!(
        drawn(&mut h, "settings-accounts-fable-7d-1=37%"),
        "the model limit's line is drawn"
    );
    let seven_day = h.bounds("settings-accounts-7d-1=40%");
    let model = h.bounds("settings-accounts-fable-7d-1=37%");
    assert_eq!(
        seven_day.origin.x + seven_day.size.width,
        model.origin.x + model.size.width,
        "the model limit's percentage ends where the seven-day line's does"
    );
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

/// The Remove confirm's title and the selector of its focused button, while
/// it shows.
fn remove_confirm(h: &mut Harness<'_>) -> Option<(String, &'static str)> {
    h.root(|root, _| root.account_remove_confirm())
}

#[gpui::test]
fn remove_opens_a_confirm_and_sends_nothing(cx: &mut TestAppContext) {
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
    assert!(writes(&mut h).is_empty(), "Remove asks before it deletes");
    assert_eq!(
        remove_confirm(&mut h),
        Some(("Remove lab?".to_owned(), "account-remove-confirm-cancel"))
    );
    assert!(drawn(&mut h, "account-remove-confirm-remove"));
}

#[gpui::test]
fn confirming_remove_sends_remove_account(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    page(&mut h, trio(), None);
    press(&mut h, "settings-accounts-remove-3");
    h.click_on("account-remove-confirm-remove");
    let sent = one_write(&mut h);
    assert!(
        matches!(sent, ClientMessage::RemoveAccount { number: 3, .. }),
        "{sent:?}"
    );
    assert_eq!(remove_confirm(&mut h), None, "the confirm closes on Remove");
}

#[gpui::test]
fn cancel_and_escape_close_the_remove_confirm(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    page(&mut h, trio(), None);
    press(&mut h, "settings-accounts-remove-3");
    h.keys("tab");
    assert_eq!(
        remove_confirm(&mut h),
        Some(("Remove lab?".to_owned(), "account-remove-confirm-remove")),
        "Tab moves the focus"
    );
    h.keys("shift-tab");
    assert_eq!(
        remove_confirm(&mut h),
        Some(("Remove lab?".to_owned(), "account-remove-confirm-cancel"))
    );
    h.click_on("account-remove-confirm-cancel");
    assert_eq!(remove_confirm(&mut h), None);
    let sent = writes(&mut h);
    assert!(sent.is_empty(), "Cancel sends nothing: {sent:?}");

    press(&mut h, "settings-accounts-remove-3");
    h.keys("escape");
    assert_eq!(remove_confirm(&mut h), None, "Esc cancels");
    let sent = writes(&mut h);
    assert!(sent.is_empty(), "Esc sends nothing: {sent:?}");
    assert!(
        h.root(|root, _| root.settings_open()),
        "Esc closed only the confirm"
    );
}

#[gpui::test]
fn an_account_leaving_the_list_closes_the_remove_confirm(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    page(&mut h, trio(), None);
    press(&mut h, "settings-accounts-remove-3");
    h.send(accounts_updated(vec![
        account(1, "main").active().build(),
        account(2, "ops").five_hour(61.0).seven_day(40.0).build(),
    ]));
    assert_eq!(remove_confirm(&mut h), None, "the account it names is gone");
    let sent = writes(&mut h);
    assert!(sent.is_empty(), "nothing is sent: {sent:?}");
}

#[gpui::test]
fn leaving_the_accounts_page_closes_the_remove_confirm(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    page(&mut h, trio(), None);
    press(&mut h, "settings-accounts-remove-3");
    h.root_update(|root, window, cx| root.show_settings_tab("General", window, cx));
    assert_eq!(remove_confirm(&mut h), None, "the page left");
    let sent = writes(&mut h);
    assert!(sent.is_empty(), "nothing is sent: {sent:?}");
}

#[gpui::test]
fn a_reconnect_closes_the_remove_confirm(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    page(&mut h, trio(), None);
    press(&mut h, "settings-accounts-remove-3");
    // A new connection: its Welcome forgets the host's accounts and closes
    // Settings; its first list brings the same three back, and the page opens
    // on them again.
    h.send_welcome(PROTOCOL);
    let id = listed(&mut h);
    h.send(accounts_reply(trio(), None, &id));
    on_page(&mut h, trio(), None);
    assert!(
        drawn(&mut h, "settings-accounts-name-3=lab"),
        "the list came back with the account the confirm named"
    );
    assert_eq!(
        remove_confirm(&mut h),
        None,
        "a new connection's list does not reopen it"
    );
    let sent = writes(&mut h);
    assert!(sent.is_empty(), "nothing is sent: {sent:?}");
}

#[gpui::test]
fn a_remove_confirm_does_not_come_back_when_its_account_returns(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    page(&mut h, trio(), None);
    press(&mut h, "settings-accounts-remove-3");
    h.send(accounts_updated(vec![
        account(1, "main").active().build(),
        account(2, "ops").five_hour(61.0).seven_day(40.0).build(),
    ]));
    h.send(accounts_updated(trio()));
    assert!(
        drawn(&mut h, "settings-accounts-name-3=lab"),
        "the account is listed again"
    );
    assert_eq!(
        remove_confirm(&mut h),
        None,
        "it closed when its account left, and stays closed"
    );
    let sent = writes(&mut h);
    assert!(sent.is_empty(), "nothing is sent: {sent:?}");
}

#[gpui::test]
fn the_remove_confirm_closes_when_its_account_becomes_active(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    page(&mut h, trio(), None);
    press(&mut h, "settings-accounts-remove-3");
    h.send(accounts_updated(vec![
        account(1, "main").build(),
        account(2, "ops").five_hour(61.0).seven_day(40.0).build(),
        account(3, "lab").active().build(),
    ]));
    assert_eq!(
        remove_confirm(&mut h),
        None,
        "the row says Switch first now, so the confirm goes"
    );
    let sent = writes(&mut h);
    assert!(sent.is_empty(), "nothing is sent: {sent:?}");
}

#[gpui::test]
fn the_alias_field_sends_nothing_on_enter_and_apply_sends_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    page(&mut h, trio(), None);
    press(&mut h, "settings-accounts-alias-2");
    assert_eq!(settings_focus(&mut h), Some("settings-accounts-alias-2"));
    h.keys("enter");
    assert!(writes(&mut h).is_empty(), "the alias did not change");

    h.keys("secondary-a");
    type_line(&mut h, "work");
    h.keys("enter");
    assert!(writes(&mut h).is_empty(), "Enter sends nothing");
    assert!(
        h.root(|root, _| root.settings_apply_enabled()),
        "the typed alias is pending"
    );
    h.click_on("settings-apply");
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
    h.keys("secondary-a backspace enter");
    assert!(writes(&mut h).is_empty(), "Enter sends nothing");
    h.click_on("settings-apply");
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

/// The page's rows of [`trio`]'s accounts, top to bottom.
fn row_order(h: &mut Harness<'_>) -> Vec<u32> {
    let mut rows: Vec<(f32, u32)> = [1, 2, 3]
        .into_iter()
        .map(|n| {
            let y = h.bounds(&format!("settings-accounts-row-{n}")).origin.y / px(1.0);
            (y, n)
        })
        .collect();
    rows.sort_by(|a, b| a.0.total_cmp(&b.0));
    rows.into_iter().map(|(_, n)| n).collect()
}

#[gpui::test]
fn up_holds_the_moved_order_until_apply(cx: &mut TestAppContext) {
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
    assert!(writes(&mut h).is_empty(), "Up sends nothing");
    assert_eq!(row_order(&mut h), [2, 1, 3], "ops moved up one place");
    press(&mut h, "settings-accounts-down-1");
    assert!(writes(&mut h).is_empty(), "Down sends nothing");
    assert_eq!(row_order(&mut h), [2, 3, 1], "the presses add up");
    h.click_on("settings-apply");
    let sent = one_write(&mut h);
    assert!(
        matches!(
            &sent,
            ClientMessage::SetAccountOrder { order, .. } if order == &[2, 3, 1]
        ),
        "the full shown order: {sent:?}"
    );
}

#[gpui::test]
fn cancel_restores_the_saved_order(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    page(&mut h, trio(), None);
    press(&mut h, "settings-accounts-up-2");
    assert_eq!(row_order(&mut h), [2, 1, 3]);
    h.click_on("settings-cancel");
    assert!(writes(&mut h).is_empty(), "nothing is sent");
    on_page(&mut h, trio(), None);
    assert_eq!(row_order(&mut h), [1, 2, 3], "the saved order shows");
    assert!(!h.root(|root, _| root.settings_apply_enabled()));
}

/// [`trio`] as the daemon answers once `work` and `home` are the aliases
/// of 2 and 3 and lab moved above ops.
fn renamed_and_moved() -> Vec<AccountSnapshot> {
    vec![
        account(1, "main").active().build(),
        account(3, "home").build(),
        account(2, "work").five_hour(61.0).seven_day(40.0).build(),
    ]
}

/// On the page of [`trio`]: `work` typed as 2's alias, `home` as 3's,
/// and lab moved up, nothing sent.
fn three_edits(h: &mut Harness<'_>) {
    page(h, trio(), None);
    press(h, "settings-accounts-alias-2");
    h.keys("secondary-a");
    type_line(h, "work");
    press(h, "settings-accounts-alias-3");
    h.keys("secondary-a");
    type_line(h, "home");
    press(h, "settings-accounts-up-3");
    assert!(writes(h).is_empty(), "nothing is sent before Apply");
}

/// The three writes of [`three_edits`], each checked, with their ids.
#[track_caller]
fn three_writes(h: &mut Harness<'_>) -> Vec<String> {
    let sent = writes(h);
    assert_eq!(sent.len(), 3, "two aliases and the order: {sent:?}");
    let alias = |number: u32, text: &str| {
        sent.iter().any(|msg| {
            matches!(
                msg,
                ClientMessage::SetAccountAlias { number: n, alias: Some(a), .. }
                    if *n == number && a == text
            )
        })
    };
    assert!(alias(2, "work") && alias(3, "home"), "{sent:?}");
    assert!(
        sent.iter().any(|msg| matches!(
            msg,
            ClientMessage::SetAccountOrder { order, .. } if order == &[1, 3, 2]
        )),
        "{sent:?}"
    );
    let ids: Vec<String> = sent.iter().map(request_id).collect();
    let distinct: std::collections::HashSet<&String> = ids.iter().collect();
    assert_eq!(distinct.len(), 3, "each write has its own id: {ids:?}");
    ids
}

#[gpui::test]
fn apply_sends_two_aliases_and_the_order_at_once(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    three_edits(&mut h);
    h.click_on("settings-apply");
    three_writes(&mut h);
}

#[gpui::test]
fn ok_waits_for_every_account_answer(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    three_edits(&mut h);
    h.click_on("settings-ok");
    let ids = three_writes(&mut h);
    assert!(h.root(|root, _| root.settings_open()), "OK waits");
    h.send(accounts_reply(renamed_and_moved(), None, &ids[0]));
    h.send(accounts_updated(renamed_and_moved()));
    h.send(accounts_reply(renamed_and_moved(), None, &ids[1]));
    assert!(
        h.root(|root, _| root.settings_open()),
        "a broadcast answers nothing; one write is still unanswered"
    );
    h.send(accounts_reply(renamed_and_moved(), None, &ids[2]));
    assert!(
        !h.root(|root, _| root.settings_open()),
        "the last answer closes Settings"
    );
}

#[gpui::test]
fn ok_stays_open_when_an_account_write_is_refused(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    page(&mut h, trio(), None);
    press(&mut h, "settings-accounts-alias-2");
    h.keys("secondary-a");
    type_line(&mut h, "work");
    press(&mut h, "settings-accounts-up-3");
    h.click_on("settings-ok");
    let sent = writes(&mut h);
    assert_eq!(sent.len(), 2, "the alias and the order: {sent:?}");
    let order = sent
        .iter()
        .find(|msg| matches!(msg, ClientMessage::SetAccountOrder { .. }))
        .expect("the order was sent");
    let alias = sent
        .iter()
        .find(|msg| matches!(msg, ClientMessage::SetAccountAlias { .. }))
        .expect("the alias was sent");
    h.send(protocol::DaemonMessage::Error {
        code: Some(protocol::error_codes::ACCOUNT_BUSY.to_owned()),
        message: "another account change is running".to_owned(),
        request_id: Some(request_id(order)),
    });
    let mut renamed = trio();
    renamed[1] = account(2, "work").five_hour(61.0).seven_day(40.0).build();
    h.send(accounts_reply(renamed.clone(), None, &request_id(alias)));
    assert!(
        h.root(|root, _| root.settings_open()),
        "a refusal keeps Settings open"
    );
    assert!(
        drawn(
            &mut h,
            "settings-accounts-error=another account change is running"
        ),
        "the other write's answer leaves the refusal shown"
    );
    h.click_on("settings-tab-general");
    h.click_on("settings-tab-accounts");
    let id = listed(&mut h);
    h.send(accounts_reply(renamed, None, &id));
    assert!(
        drawn(
            &mut h,
            "settings-accounts-error=another account change is running"
        ),
        "so does a list reply"
    );
}

#[gpui::test]
fn a_held_order_the_daemon_caught_up_with_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    page(&mut h, trio(), None);
    press(&mut h, "settings-accounts-down-2");
    assert!(h.root(|root, _| root.settings_apply_enabled()));
    h.send(accounts_updated(trio()[..2].to_vec()));
    assert!(
        !h.root(|root, _| root.settings_apply_enabled()),
        "lab left: the shown order 1, 2 is the daemon's"
    );
    h.click_on("settings-apply");
    assert!(writes(&mut h).is_empty(), "no order is sent");
}

#[gpui::test]
fn a_held_order_takes_in_an_account_added_since(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    page(&mut h, trio()[..2].to_vec(), None);
    press(&mut h, "settings-accounts-down-1");
    h.send(accounts_updated(trio()));
    h.click_on("settings-apply");
    let sent = one_write(&mut h);
    assert!(
        matches!(
            &sent,
            ClientMessage::SetAccountOrder { order, .. } if order == &[2, 1, 3]
        ),
        "the shown order over the daemon's current list: {sent:?}"
    );
}

#[gpui::test]
fn a_typed_alias_survives_a_rename_from_elsewhere(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    typed_alias(&mut h);
    press(&mut h, "settings-accounts-up-3");
    let mut renamed = trio();
    renamed[1] = account(2, "home").five_hour(61.0).seven_day(40.0).build();
    h.send(accounts_updated(renamed));
    assert_eq!(
        h.root(|root, cx| root.settings_account_alias(2, cx)),
        Some("work".to_owned()),
        "the draft stays"
    );
    h.click_on("settings-apply");
    let sent = writes(&mut h);
    assert!(
        sent.iter().any(|msg| matches!(
            msg,
            ClientMessage::SetAccountAlias { number: 2, alias: Some(alias), .. } if alias == "work"
        )),
        "Apply sends the draft: {sent:?}"
    );
}

#[gpui::test]
fn an_applied_alias_keeps_showing_until_its_answer(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    typed_alias(&mut h);
    h.click_on("settings-apply");
    let sent = one_write(&mut h);
    assert_eq!(
        h.root(|root, cx| root.settings_account_alias(2, cx)),
        Some("work".to_owned()),
        "the sent alias shows before its answer"
    );
    assert!(!h.root(|root, _| root.settings_apply_enabled()));
    let mut renamed = trio();
    renamed[1] = account(2, "work").five_hour(61.0).seven_day(40.0).build();
    h.send(accounts_reply(renamed, None, &request_id(&sent)));
    assert_eq!(
        h.root(|root, cx| root.settings_account_alias(2, cx)),
        Some("work".to_owned())
    );
    assert!(!h.root(|root, _| root.settings_apply_enabled()));
}

#[gpui::test]
fn a_refused_alias_reads_as_edited_again(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    typed_alias(&mut h);
    h.click_on("settings-apply");
    let sent = one_write(&mut h);
    h.send(protocol::DaemonMessage::Error {
        code: Some(protocol::error_codes::ACCOUNT_BUSY.to_owned()),
        message: "another account change is running".to_owned(),
        request_id: Some(request_id(&sent)),
    });
    assert_eq!(
        h.root(|root, cx| root.settings_account_alias(2, cx)),
        Some("work".to_owned()),
        "the field keeps the refused text"
    );
    assert!(
        h.root(|root, _| root.settings_apply_enabled()),
        "and Apply relights"
    );
}

#[gpui::test]
fn the_apply_answer_updates_the_list(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    page(&mut h, trio(), None);
    press(&mut h, "settings-accounts-up-2");
    h.click_on("settings-apply");
    let sent = one_write(&mut h);
    let mut moved = trio();
    moved.swap(0, 1);
    h.send(accounts_reply(moved, None, &request_id(&sent)));
    assert_eq!(row_order(&mut h), [2, 1, 3], "the answer's order shows");
    assert!(
        !h.root(|root, _| root.settings_apply_enabled()),
        "nothing is held"
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
fn the_auto_switch_toggle_holds_until_apply(cx: &mut TestAppContext) {
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
    assert!(writes(&mut h).is_empty(), "the toggle sends nothing");
    h.click_on("settings-apply");
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
fn the_strategy_choice_holds_until_apply(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    page(&mut h, trio(), Some(auto_switch(None)));
    press(&mut h, "settings-accounts-strategy-consume-first");
    assert!(writes(&mut h).is_empty(), "the choice sends nothing");
    h.click_on("settings-apply");
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
fn the_forecast_window_choice_holds_until_apply(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    page(&mut h, trio(), Some(auto_switch(None)));
    press(&mut h, "settings-accounts-window-120");
    assert!(writes(&mut h).is_empty(), "the choice sends nothing");
    h.click_on("settings-apply");
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
    ] {
        press(&mut h, selector);
        assert!(
            writes(&mut h).is_empty(),
            "{selector} waits for the pending write"
        );
    }
    assert!(
        !h.root(|root, _| root.settings_apply_enabled()),
        "Up held nothing while the write was pending"
    );
    press(&mut h, "settings-accounts-strategy-consume-first");
    assert!(writes(&mut h).is_empty(), "a choice sends nothing");
    assert!(
        h.root(|root, _| root.settings_apply_enabled()),
        "the auto-switch controls stay live and hold their value"
    );
    assert_eq!(
        remove_confirm(&mut h),
        None,
        "a Remove press while the write is pending opens no confirm"
    );

    h.send(accounts_reply(
        trio(),
        Some(auto_switch(None)),
        &request_id(&sent),
    ));
    press(&mut h, "settings-accounts-remove-3");
    h.click_on("account-remove-confirm-remove");
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
    h.keys("secondary-a");
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
    assert!(writes(&mut h).is_empty(), "the toggle waits for Apply");
    h.click_on("settings-apply");
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
    h.keys("secondary-a");
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
    assert!(
        !h.root(|root, _| root.settings_apply_enabled()),
        "no alias edit is pending"
    );
}

#[gpui::test]
fn closing_settings_with_ok_sends_a_typed_alias(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    page(&mut h, trio(), None);
    press(&mut h, "settings-accounts-alias-2");
    h.keys("secondary-a");
    type_line(&mut h, "work");
    h.click_on("settings-ok");
    let sent = one_write(&mut h);
    assert!(
        h.root(|root, _| root.settings_open()),
        "OK waits for the alias's answer"
    );
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
    let mut renamed = trio();
    renamed[1] = account(2, "work").five_hour(61.0).seven_day(40.0).build();
    h.send(accounts_reply(renamed, None, &request_id(&sent)));
    assert!(
        !h.root(|root, _| root.settings_open()),
        "OK closes Settings once the alias is answered"
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
    h.click_on("settings-apply");
    press(&mut h, "settings-accounts-alias-3");
    h.click_on("settings-apply");
    assert!(
        writes(&mut h).is_empty(),
        "the field the user never edited sends nothing"
    );
}

/// Settings open on the Accounts page of [`trio`], `work` typed into
/// account 2's alias field and nothing sent.
fn typed_alias(h: &mut Harness<'_>) {
    page(h, trio(), None);
    press(h, "settings-accounts-alias-2");
    h.keys("secondary-a");
    type_line(h, "work");
    assert!(writes(h).is_empty(), "typing sends nothing");
}

/// Settings closed by whatever the spec pressed: nothing was sent, and
/// reopening it shows account 2's saved alias.
#[track_caller]
fn the_typed_alias_is_gone(h: &mut Harness<'_>) {
    assert!(!h.root(|root, _| root.settings_open()), "Settings closed");
    assert!(writes(h).is_empty(), "nothing is sent");
    on_page(h, trio(), None);
    assert_eq!(
        h.root(|root, cx| root.settings_account_alias(2, cx)),
        Some("ops".to_owned()),
        "the saved alias shows"
    );
    assert!(!h.root(|root, _| root.settings_apply_enabled()));
}

#[gpui::test]
fn cancel_discards_a_typed_alias(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    typed_alias(&mut h);
    h.click_on("settings-cancel");
    the_typed_alias_is_gone(&mut h);
}

#[gpui::test]
fn the_close_button_discards_a_typed_alias(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    typed_alias(&mut h);
    h.click_on("settings-close");
    the_typed_alias_is_gone(&mut h);
}

#[gpui::test]
fn escape_discards_a_typed_alias(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    typed_alias(&mut h);
    h.keys("escape");
    the_typed_alias_is_gone(&mut h);
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

/// The newest `ListAccounts` request id in `sent`.
fn list_id_in(sent: Vec<ClientMessage>) -> Option<String> {
    sent.into_iter().rev().find_map(|msg| match msg {
        ClientMessage::ListAccounts { request_id } => request_id,
        _ => None,
    })
}

#[gpui::test]
fn a_remote_hosts_accounts_show_on_its_settings_page(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    // This PC (the client's own daemon) keeps no account.
    shown(&mut h, Vec::new(), None);
    // The remote host keeps three and lists the feature.
    let mut features = daemon_features();
    features.push(protocol::FEATURE_ACCOUNTS.to_owned());
    let mut host = h.host(&remote());
    host.connect_keeping_sent_with(features);
    let asked = list_id_in(host.sent()).expect("the remote host is asked on its Welcome");
    host.send(accounts_reply(trio(), Some(auto_switch(None)), &asked));

    h.click_on("settings-open");
    h.click_on(&remote_row());
    assert!(
        tab_listed(&mut h),
        "a remote host that lists accounts offers its Accounts tab"
    );
    h.click_on("settings-tab-accounts");
    let mut host = h.host(&remote());
    let asked = list_id_in(host.sent()).expect("the tab asks the remote host, not This PC");
    host.send(accounts_reply(trio(), Some(auto_switch(None)), &asked));
    assert!(
        drawn(&mut h, "settings-accounts-switch-2"),
        "the remote host's rows are drawn"
    );

    press(&mut h, "settings-accounts-remove-3");
    assert!(
        h.host(&remote()).sent().is_empty(),
        "administration stays on the machine running the daemon"
    );
    press(&mut h, "settings-accounts-switch-2");
    let sent = h.host(&remote()).sent();
    assert!(
        sent.iter()
            .any(|msg| matches!(msg, ClientMessage::SwitchAccount { number: 2, .. })),
        "Switch goes to the remote host: {sent:?}"
    );
    assert!(
        !h.sent()
            .iter()
            .any(|msg| matches!(msg, ClientMessage::SwitchAccount { .. })),
        "and not to This PC"
    );
}

/// Settings open on the remote host's Accounts page: This PC keeps no
/// account, the remote host lists the feature and keeps [`trio`], nothing
/// sent left unread on it.
fn remote_page(h: &mut Harness<'_>) {
    shown(h, Vec::new(), None);
    let mut features = daemon_features();
    features.push(protocol::FEATURE_ACCOUNTS.to_owned());
    let mut host = h.host(&remote());
    host.connect_keeping_sent_with(features);
    let asked = list_id_in(host.sent()).expect("the remote host is asked on its Welcome");
    host.send(accounts_reply(trio(), Some(auto_switch(None)), &asked));
    h.click_on("settings-open");
    h.click_on(&remote_row());
    h.click_on("settings-tab-accounts");
    let mut host = h.host(&remote());
    let asked = list_id_in(host.sent()).expect("the tab asks the remote host");
    host.send(accounts_reply(trio(), Some(auto_switch(None)), &asked));
    h.sent();
}

#[gpui::test]
fn a_window_launched_against_another_machine_locks_administration(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    // The window's own connection, under the Local key, reaches an SSH
    // host whose daemon calls it local.
    let mut features = daemon_features();
    features.push(protocol::FEATURE_ACCOUNTS.to_owned());
    let mut conn = Connection::for_target(Target::parse_ssh("devbox", None).expect("an SSH host"));
    conn.on_connecting();
    conn.on_socket_open(8787);
    conn.on_welcome(PROTOCOL, features, None);
    h.event(NetEvent::State(conn));
    h.send_welcome(PROTOCOL);
    let id = listed(&mut h);
    h.send(accounts_reply(trio(), Some(auto_switch(None)), &id));
    on_page(&mut h, trio(), Some(auto_switch(None)));

    for selector in [
        "settings-accounts-remove-3",
        "settings-accounts-up-2",
        "settings-accounts-add-login",
        "settings-accounts-login",
    ] {
        press(&mut h, selector);
        assert!(
            writes(&mut h).is_empty(),
            "{selector} is disabled on another machine"
        );
    }
    press(&mut h, "settings-accounts-switch-2");
    let sent = one_write(&mut h);
    assert!(
        matches!(sent, ClientMessage::SwitchAccount { number: 2, .. }),
        "Switch stays live: {sent:?}"
    );
}

#[gpui::test]
fn a_remote_hosts_accounts_lock_administration(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    remote_page(&mut h);

    for selector in [
        "settings-accounts-up-2",
        "settings-accounts-down-2",
        "settings-accounts-login",
    ] {
        press(&mut h, selector);
        assert!(
            h.host(&remote()).sent().is_empty(),
            "{selector} is disabled on a remote host"
        );
    }
    press(&mut h, "settings-accounts-alias-2");
    h.keys("secondary-a");
    type_line(&mut h, "work");
    h.keys("enter");
    assert!(
        h.host(&remote()).sent().is_empty(),
        "the alias field takes no edit on a remote host"
    );

    press(&mut h, "settings-accounts-switch-2");
    let sent = h.host(&remote()).sent();
    assert!(
        sent.iter()
            .any(|msg| matches!(msg, ClientMessage::SwitchAccount { number: 2, .. })),
        "Switch stays live and goes to the remote host: {sent:?}"
    );
    assert!(writes(&mut h).is_empty(), "nothing goes to This PC");
}

#[gpui::test]
fn cancel_discards_held_auto_switch_settings(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    page(&mut h, trio(), Some(auto_switch(None)));
    press(&mut h, "settings-accounts-auto");
    press(&mut h, "settings-accounts-threshold-down");
    press(&mut h, "settings-accounts-strategy-consume-first");
    assert!(drawn(&mut h, "settings-accounts-threshold=85%"));
    h.click_on("settings-cancel");
    assert!(writes(&mut h).is_empty(), "nothing is sent");
    on_page(&mut h, trio(), Some(auto_switch(None)));
    assert!(
        drawn(&mut h, "settings-accounts-threshold=90%"),
        "the saved threshold shows"
    );
    assert!(!h.root(|root, _| root.settings_apply_enabled()));
}

#[gpui::test]
fn a_threshold_and_a_toggle_go_in_one_set_auto_switch(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    page(&mut h, trio(), Some(auto_switch(None)));
    press(&mut h, "settings-accounts-threshold-down");
    press(&mut h, "settings-accounts-threshold-down");
    assert!(
        drawn(&mut h, "settings-accounts-threshold=80%"),
        "the stepper steps from the shown value"
    );
    press(&mut h, "settings-accounts-auto");
    assert!(writes(&mut h).is_empty(), "nothing is sent before Apply");
    h.click_on("settings-apply");
    let sent = one_write(&mut h);
    assert!(
        matches!(
            sent,
            ClientMessage::SetAutoSwitch {
                enabled: Some(false),
                threshold_pct: Some(threshold),
                ..
            } if (threshold - 80.0).abs() < f64::EPSILON
        ),
        "{sent:?}"
    );
}

#[gpui::test]
fn two_rules_go_in_one_set_auto_switch_rules(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    page(&mut h, trio(), Some(auto_switch(None)));
    press(&mut h, "settings-accounts-hysteresis-up");
    press(&mut h, "settings-accounts-hysteresis-up");
    assert!(drawn(&mut h, "settings-accounts-hysteresis=7%"));
    press(&mut h, "settings-accounts-strategy-consume-first");
    assert!(writes(&mut h).is_empty(), "nothing is sent before Apply");
    h.click_on("settings-apply");
    let sent = one_write(&mut h);
    assert!(
        matches!(
            sent,
            ClientMessage::SetAutoSwitchRules {
                hysteresis_pct: Some(hysteresis),
                cooldown_secs: None,
                strategy: Some(AutoSwitchStrategy::ConsumeFirst),
                rate_window_minutes: None,
                ..
            } if (hysteresis - 7.0).abs() < f64::EPSILON
        ),
        "{sent:?}"
    );
}

#[gpui::test]
fn pressing_back_to_the_saved_value_holds_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    page(&mut h, trio(), Some(auto_switch(None)));
    press(&mut h, "settings-accounts-auto");
    assert!(h.root(|root, _| root.settings_apply_enabled()));
    press(&mut h, "settings-accounts-auto");
    press(&mut h, "settings-accounts-threshold-down");
    press(&mut h, "settings-accounts-threshold-up");
    assert!(
        !h.root(|root, _| root.settings_apply_enabled()),
        "every value is back at the saved one"
    );
    assert!(writes(&mut h).is_empty());
}

#[gpui::test]
fn a_remote_hosts_auto_switch_settings_stage_too(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    remote_page(&mut h);
    press(&mut h, "settings-accounts-auto");
    assert!(
        h.host(&remote()).sent().is_empty(),
        "the toggle waits for Apply on a remote host"
    );
    h.click_on("settings-apply");
    let sent = h.host(&remote()).sent();
    assert!(
        sent.iter().any(|msg| matches!(
            msg,
            ClientMessage::SetAutoSwitch {
                enabled: Some(false),
                threshold_pct: None,
                ..
            }
        )),
        "Apply sends it to the remote host: {sent:?}"
    );
    assert!(writes(&mut h).is_empty(), "nothing goes to This PC");
}
