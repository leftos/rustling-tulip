//! The phone's Notifications switch: one per saved tailnet host whose
//! daemon takes relay pushes, showing the daemon's `PushStatus`, registering
//! this phone for that host's pushes and re-registering when the phone's
//! token changed.

#![expect(
    clippy::panic,
    reason = "a spec fails with the message of the precondition it lost"
)]

use std::sync::Arc;

use crate::support;

use daemon_client::device_tokens::DeviceTokenStore as _;
use gpui::{TestAppContext, px};
use protocol::{
    ApnsEnvironment, AttentionReason, ClientMessage, DaemonMessage, FEATURE_RELAY_PUSH,
    PushRegistration, Secret,
};
use rustling_tulip_native::phone::PhoneRoot;
use rustling_tulip_native::phone::push::{
    Authorization, Delivered, FakePushPlatform, NEEDS_YOU_KIND, PlatformCall, PushPlatform,
    UnsupportedPlatform,
};
use rustling_tulip_native::target::Target;
use rustling_tulip_native::{Connection, NetEvent};
use serde_json::json;
use support::{Harness, REMOTE, TestDir, saved_remote, seed_ui, session};

/// Whether the last frame drew what `selector` tags.
fn drawn(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).size.width > px(0.0)
}

/// The saved host's switch.
fn switch() -> String {
    format!("phone-host-push-{REMOTE}")
}

/// Opens Settings, then its Notifications page, where the switches draw.
fn open_notifications(h: &mut Harness<'_>) {
    h.click_on("phone-settings-open");
    h.click_on("phone-settings-row-notifications");
}

/// Opens Settings, then its Hosts page, where the add form draws.
fn open_hosts(h: &mut Harness<'_>) {
    h.click_on("phone-settings-open");
    h.click_on("phone-settings-row-hosts");
}

/// Reads the phone's root.
fn read_phone<R>(h: &mut Harness<'_>, read: impl FnOnce(&PhoneRoot, &gpui::App) -> R) -> R {
    h.cx.update(|window, cx| {
        let phone = window
            .root::<PhoneRoot>()
            .flatten()
            .expect("the window's root is the phone's");
        read(phone.read(cx), cx)
    })
}

/// The host and id of the session the phone shows in place of Home.
fn shown_session(h: &mut Harness<'_>) -> Option<(String, String)> {
    read_phone(h, |phone, _| {
        phone
            .shown_session()
            .map(|(host, id)| (host.wire().unwrap_or_default().to_owned(), id.to_owned()))
    })
}

/// Why the phone could not open the session a notification named, while it
/// says so.
fn open_note(h: &mut Harness<'_>) -> Option<String> {
    read_phone(h, |phone, _| phone.open_note().map(str::to_owned))
}

/// Taps the phone's notification for `session_id` on `host`, then lets its
/// root take the tap.
fn tap(platform: &FakePushPlatform, host: &str, session_id: &str, h: &mut Harness<'_>) {
    platform.tap(host, session_id);
    h.cx.run_until_parked();
}

/// A phone with [`REMOTE`] saved and paired, its push through `platform`.
fn phone<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    platform: Arc<dyn PushPlatform>,
) -> Harness<'a> {
    seed_ui(
        dir,
        &json!({ "saved_hosts": [{ "name": REMOTE, "port": 8787 }] }),
    );
    let h = Harness::phone_with(cx, dir, platform);
    h.tokens().set(REMOTE, "t1").expect("seed a token");
    h
}

/// Connects [`REMOTE`] taking relay pushes and gives the request id of the
/// `GetPushStatus` the phone sent on open.
fn open_with_relay_push(h: &mut Harness<'_>) -> String {
    open_target_with_relay_push(h, &saved_remote())
}

/// Connects `target` taking relay pushes and gives the request id of the
/// `GetPushStatus` the phone sent on open.
fn open_target_with_relay_push(h: &mut Harness<'_>, target: &Target) -> String {
    let mut features = support::daemon_features();
    features.push(FEATURE_RELAY_PUSH.to_owned());
    let mut host = h.host(target);
    host.connect_keeping_sent_with(features);
    let asked = status_requests(host.sent());
    let [request_id] = asked.as_slice() else {
        panic!("one GetPushStatus on open: {asked:?}");
    };
    request_id.clone()
}

/// The request id of each `GetPushStatus` in `sent`.
fn status_requests(sent: Vec<ClientMessage>) -> Vec<String> {
    sent.into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::GetPushStatus { request_id } => Some(request_id),
            _ => None,
        })
        .collect()
}

/// The saved host's hint line under its switch.
fn hint() -> String {
    format!("phone-host-push-hint-{REMOTE}")
}

/// A saved host's connection, dropped.
fn closed(target: Target) -> Connection {
    let mut conn = Connection::for_target(target);
    conn.on_connecting();
    conn.on_socket_open(8787);
    let _ = conn.on_closed("suspended".to_owned());
    conn
}

/// The daemon's `PushStatus` holding `token` for this phone, answering
/// `request_id`.
fn status(token: Option<&str>, request_id: &str) -> DaemonMessage {
    DaemonMessage::PushStatus {
        vapid_public_key: "vapid".to_owned(),
        endpoint: None,
        device_token: token.map(|token| Secret(token.to_owned())),
        needs_you: true,
        finished: true,
        hold_at_desk: true,
        finished_grace_secs: None,
        request_id: Some(request_id.to_owned()),
    }
}

/// Each `RegisterPush` in `sent`: its registration, its three switches and
/// its request id.
fn registers(sent: Vec<ClientMessage>) -> Vec<(PushRegistration, [bool; 3], String)> {
    sent.into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::RegisterPush {
                registration,
                needs_you,
                finished,
                hold_at_desk,
                finished_grace_secs: _,
                request_id,
            } => Some((
                registration,
                [needs_you, finished, hold_at_desk],
                request_id,
            )),
            _ => None,
        })
        .collect()
}

/// The registration the fake platform's token and keys make.
fn fake_registration() -> PushRegistration {
    let keys = FakePushPlatform::keys();
    PushRegistration::Apns {
        token: Secret(FakePushPlatform::TOKEN.to_owned()),
        environment: ApnsEnvironment::Sandbox,
        p256dh: keys.p256dh_b64url().expect("the fixed key is valid"),
        auth: Secret(keys.auth_b64url()),
    }
}

/// A Needs you notification `id` the drawer shows for `session` on `host`.
fn shown(id: &str, session: &str, host: &str) -> Delivered {
    Delivered {
        id: id.to_owned(),
        host: Some(host.to_owned()),
        session_id: Some(session.to_owned()),
        kind: Some(NEEDS_YOU_KIND.to_owned()),
        waited_since: None,
    }
}

#[gpui::test]
fn a_wait_that_stops_on_its_own_removes_its_notification(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let platform = Arc::new(FakePushPlatform::default());
    platform.set_delivered(vec![shown("n1", "s1", REMOTE)]);
    let mut h = phone(cx, &dir, platform.clone());
    h.host(&saved_remote()).connect();
    h.host(&saved_remote())
        .send(remote_list("awaiting_input", "s2"));
    assert_eq!(platform.badges(), vec![2]);

    h.host(&saved_remote()).send(DaemonMessage::SessionUpdated {
        session: session("s1").status("stopped").build(),
        request_id: None,
    });
    h.host(&saved_remote()).send(DaemonMessage::Attention {
        session_id: "s1".to_owned(),
        reason: AttentionReason::Stopped,
    });

    assert_eq!(platform.removed(), vec![vec!["n1".to_owned()]]);
    assert_eq!(
        platform.badges().last(),
        Some(&1),
        "an ended session is not counted"
    );
}

#[gpui::test]
fn a_finished_notification_for_an_idle_session_stays(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let platform = Arc::new(FakePushPlatform::default());
    platform.set_delivered(vec![Delivered {
        kind: Some("finished".to_owned()),
        ..shown("n1", "s1", REMOTE)
    }]);
    let mut h = phone(cx, &dir, platform.clone());
    h.host(&saved_remote()).connect();

    h.host(&saved_remote()).send(remote_list("idle", "s2"));

    assert_eq!(delivered_asks(&platform), 1);
    assert_eq!(platform.removed(), Vec::<Vec<String>>::new());
}

#[gpui::test]
fn a_return_to_the_foreground_waits_for_a_fresh_list(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let platform = Arc::new(FakePushPlatform::default());
    let mut h = phone(cx, &dir, platform.clone());
    h.host(&saved_remote()).connect();
    h.host(&saved_remote()).send(remote_list("working", "s2"));
    h.host(&saved_remote()).sent();

    h.cx.simulate_visibility_change(gpui::WindowVisibility::Hidden);
    platform.set_delivered(vec![shown("n1", "s1", REMOTE)]);
    h.cx.simulate_visibility_change(gpui::WindowVisibility::Visible);
    h.cx.run_until_parked();

    let sent = h.host(&saved_remote()).sent();
    assert!(
        sent.iter()
            .any(|msg| matches!(msg, ClientMessage::ListSessions)),
        "the phone asks for a fresh list: {sent:?}"
    );
    assert_eq!(
        platform.removed(),
        Vec::<Vec<String>>::new(),
        "the list from before the suspension decides nothing"
    );

    h.host(&saved_remote())
        .send(remote_list("awaiting_input", "s2"));
    assert_eq!(platform.removed(), Vec::<Vec<String>>::new(), "s1 waits");

    h.host(&saved_remote()).send(remote_list("working", "s2"));
    assert_eq!(platform.removed(), vec![vec!["n1".to_owned()]]);
}

#[gpui::test]
fn only_the_newest_sweep_acts(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let platform = Arc::new(FakePushPlatform::default());
    platform.set_delivered(vec![shown("n1", "s1", REMOTE)]);
    platform.set_hold_delivered(true);
    let mut h = phone(cx, &dir, platform.clone());
    h.host(&saved_remote()).connect();
    h.host(&saved_remote())
        .send(remote_list("awaiting_input", "s2"));
    h.host(&saved_remote()).send(remote_list("working", "s2"));
    assert_eq!(delivered_asks(&platform), 2, "two sweeps are out");

    platform.release_delivered();
    h.cx.run_until_parked();

    assert_eq!(platform.removed(), vec![vec!["n1".to_owned()]]);
    assert_eq!(platform.badges(), vec![1], "the older sweep sets nothing");
}

/// [`REMOTE`]'s whole session list: `s1` in `s1_status`, `s2` waiting on the
/// user and labelled `s2_label`.
fn remote_list(s1_status: &str, s2_label: &str) -> DaemonMessage {
    DaemonMessage::Sessions {
        sessions: vec![
            session("s1").status(s1_status).build(),
            session("s2")
                .status("awaiting_input")
                .label(s2_label)
                .build(),
        ],
    }
}

/// How many times the platform was asked for the delivered notifications.
fn delivered_asks(platform: &FakePushPlatform) -> usize {
    platform
        .calls()
        .iter()
        .filter(|call| **call == PlatformCall::Delivered)
        .count()
}

#[gpui::test]
fn a_handled_wait_removes_its_notification_and_sets_the_badge(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let platform = Arc::new(FakePushPlatform::default());
    platform.set_delivered(vec![shown("n1", "s1", REMOTE), shown("n2", "s2", REMOTE)]);
    let mut h = phone(cx, &dir, platform.clone());
    h.host(&saved_remote()).connect();
    assert_eq!(
        delivered_asks(&platform),
        0,
        "nothing is swept before the host's list is in"
    );

    h.host(&saved_remote())
        .send(remote_list("awaiting_input", "s2"));
    assert_eq!(platform.removed(), Vec::<Vec<String>>::new(), "both wait");
    assert_eq!(platform.badges(), vec![2]);

    h.host(&saved_remote()).send(remote_list("working", "s2"));
    assert_eq!(platform.removed(), vec![vec!["n1".to_owned()]]);
    assert_eq!(platform.badges(), vec![2, 1], "the badge is the count left");
}

#[gpui::test]
fn a_notification_for_a_host_not_connected_stays(cx: &mut TestAppContext) {
    const OTHER_HOST: &str = "den.tail1234.ts.net";
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({
            "saved_hosts": [
                { "name": REMOTE, "port": 8787 },
                { "name": OTHER_HOST, "port": 8787 },
            ]
        }),
    );
    let platform = Arc::new(FakePushPlatform::default());
    platform.set_delivered(vec![shown("n-den", "s1", OTHER_HOST)]);
    let mut h = Harness::phone_with(cx, &dir, platform.clone());
    h.tokens().set(REMOTE, "t1").expect("seed a token");
    h.host(&saved_remote()).connect();

    h.host(&saved_remote()).send(remote_list("working", "s2"));

    assert_eq!(delivered_asks(&platform), 1, "the loaded host sweeps");
    assert_eq!(
        platform.removed(),
        Vec::<Vec<String>>::new(),
        "the other host's notification stays"
    );
    assert_eq!(platform.badges(), vec![1], "only the loaded host counts");
}

#[gpui::test]
fn an_unrelated_label_change_sweeps_nothing_again(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let platform = Arc::new(FakePushPlatform::default());
    platform.set_delivered(vec![shown("n2", "s2", REMOTE)]);
    let mut h = phone(cx, &dir, platform.clone());
    h.host(&saved_remote()).connect();
    h.host(&saved_remote())
        .send(remote_list("awaiting_input", "s2"));
    assert_eq!(delivered_asks(&platform), 1);

    h.host(&saved_remote())
        .send(remote_list("awaiting_input", "renamed"));

    assert_eq!(
        delivered_asks(&platform),
        1,
        "the same Needs you set asks nothing"
    );
    assert_eq!(platform.badges(), vec![2], "nor sets the badge again");
    assert_eq!(platform.removed(), Vec::<Vec<String>>::new());
}

#[gpui::test]
fn the_switch_shows_for_a_tailnet_host_that_takes_relay_push(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir, Arc::new(FakePushPlatform::default()));
    h.host(&saved_remote())
        .connect_with_features(&[FEATURE_RELAY_PUSH]);
    open_notifications(&mut h);

    assert!(drawn(&mut h, &switch()), "the switch shows");
    assert!(
        drawn(&mut h, &format!("{}-empty", switch())),
        "it shows off before the daemon answers"
    );
}

#[gpui::test]
fn no_switch_without_relay_push(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir, Arc::new(FakePushPlatform::default()));
    let sent = {
        let mut host = h.host(&saved_remote());
        host.connect_keeping_sent();
        host.sent()
    };
    open_notifications(&mut h);

    assert!(
        !drawn(&mut h, &switch()),
        "an older daemon offers no switch"
    );
    assert!(
        !sent
            .iter()
            .any(|msg| matches!(msg, ClientMessage::GetPushStatus { .. })),
        "nor is asked for its push state: {sent:?}"
    );
}

#[gpui::test]
fn no_switch_when_the_platform_is_unsupported(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir, Arc::new(UnsupportedPlatform));
    let mut features = support::daemon_features();
    features.push(FEATURE_RELAY_PUSH.to_owned());
    let sent = {
        let mut host = h.host(&saved_remote());
        host.connect_keeping_sent_with(features);
        host.sent()
    };
    open_notifications(&mut h);

    assert!(!drawn(&mut h, &switch()), "a phone with no push shows none");
    assert!(
        !sent
            .iter()
            .any(|msg| matches!(msg, ClientMessage::GetPushStatus { .. })),
        "and asks no host: {sent:?}"
    );
}

#[gpui::test]
fn opening_a_host_asks_for_its_push_status(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir, Arc::new(FakePushPlatform::default()));

    let request_id = open_with_relay_push(&mut h);

    assert_ne!(request_id, "", "the request carries an id to answer by");
}

#[gpui::test]
fn turning_it_on_registers_with_all_three_on(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let platform = Arc::new(FakePushPlatform::default());
    let mut h = phone(cx, &dir, platform.clone());
    let asked = open_with_relay_push(&mut h);
    h.host(&saved_remote()).send(status(None, &asked));
    open_notifications(&mut h);
    assert!(drawn(&mut h, &format!("{}-empty", switch())), "off");

    h.click_on(&switch());

    let sent = registers(h.host(&saved_remote()).sent());
    let [(registration, switches, request_id)] = sent.as_slice() else {
        panic!("one RegisterPush: {sent:?}");
    };
    assert_eq!(*registration, fake_registration());
    assert_eq!(*switches, [true, true, true], "all three kinds on");
    assert!(
        drawn(&mut h, &format!("{}-tick", switch())),
        "it shows on while the registration is out"
    );
    assert!(
        !platform
            .calls()
            .contains(&PlatformCall::RequestAuthorization),
        "a granted permission is not asked again"
    );

    h.host(&saved_remote())
        .send(status(Some(FakePushPlatform::TOKEN), request_id));
    assert!(drawn(&mut h, &format!("{}-tick", switch())), "on");
}

#[gpui::test]
fn turning_it_on_when_blocked_shows_the_hint_and_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let platform = Arc::new(FakePushPlatform::default());
    platform.set_authorization(Authorization::Denied);
    let mut h = phone(cx, &dir, platform.clone());
    let asked = open_with_relay_push(&mut h);
    h.host(&saved_remote()).send(status(None, &asked));
    open_notifications(&mut h);

    h.click_on(&switch());

    let hint = format!("phone-host-push-hint-{REMOTE}");
    assert!(drawn(&mut h, &hint), "the blocked line shows");
    assert!(drawn(&mut h, &format!("{}-empty", switch())), "off");
    assert_eq!(
        registers(h.host(&saved_remote()).sent()),
        Vec::new(),
        "nothing is registered"
    );
    assert!(
        !platform
            .calls()
            .contains(&PlatformCall::RequestAuthorization),
        "a denied permission is not asked for"
    );
}

#[gpui::test]
fn a_status_with_another_token_reregisters(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir, Arc::new(FakePushPlatform::default()));
    let asked = open_with_relay_push(&mut h);

    h.host(&saved_remote())
        .send(status(Some("ffff0000"), &asked));

    let sent = registers(h.host(&saved_remote()).sent());
    let [(registration, switches, _)] = sent.as_slice() else {
        panic!("one RegisterPush: {sent:?}");
    };
    assert_eq!(
        *registration,
        fake_registration(),
        "with this phone's token"
    );
    assert_eq!(*switches, [true, true, true]);
}

#[gpui::test]
fn turning_it_off_unregisters(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir, Arc::new(FakePushPlatform::default()));
    let asked = open_with_relay_push(&mut h);
    h.host(&saved_remote())
        .send(status(Some(FakePushPlatform::TOKEN), &asked));
    open_notifications(&mut h);
    assert!(drawn(&mut h, &format!("{}-tick", switch())), "on");
    assert_eq!(
        registers(h.host(&saved_remote()).sent()),
        Vec::new(),
        "the same token registers nothing"
    );

    h.click_on(&switch());

    let sent = h.host(&saved_remote()).sent();
    assert!(
        sent.iter()
            .any(|msg| matches!(msg, ClientMessage::UnregisterPush { .. })),
        "turning it off unregisters: {sent:?}"
    );
    assert!(drawn(&mut h, &format!("{}-empty", switch())), "off");
}

#[gpui::test]
fn the_switch_stays_reachable_once_a_host_is_open(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir, Arc::new(FakePushPlatform::default()));
    open_with_relay_push(&mut h);
    h.host(&saved_remote()).send(DaemonMessage::Sessions {
        sessions: vec![session("s1").build()],
    });

    assert!(drawn(&mut h, "phone-home"), "the session list shows");
    assert!(
        !drawn(&mut h, &format!("phone-host-row-{REMOTE}")),
        "no host row while the sessions list"
    );
    assert!(!drawn(&mut h, &switch()), "Home draws no switch");

    open_notifications(&mut h);
    assert!(
        drawn(&mut h, &switch()),
        "Settings' Notifications page does"
    );
    assert!(
        h.bounds(&switch()).size.height >= px(44.0),
        "a full touch target"
    );
}

#[gpui::test]
fn a_reopen_asks_again_and_ignores_the_first_reply(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir, Arc::new(FakePushPlatform::default()));
    let first = open_with_relay_push(&mut h);
    open_notifications(&mut h);
    h.host(&saved_remote())
        .event(NetEvent::State(closed(saved_remote())));

    let second = open_with_relay_push(&mut h);
    assert_ne!(first, second, "the reopen asks with a new request id");
    h.host(&saved_remote())
        .send(status(Some(FakePushPlatform::TOKEN), &first));
    assert!(
        drawn(&mut h, &format!("{}-empty", switch())),
        "the first connection's reply changes nothing"
    );

    h.host(&saved_remote())
        .send(status(Some(FakePushPlatform::TOKEN), &second));
    assert!(drawn(&mut h, &format!("{}-tick", switch())), "on");
}

#[gpui::test]
fn the_not_asked_path_asks_then_registers(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let platform = Arc::new(FakePushPlatform::default());
    platform.set_authorization(Authorization::NotDetermined);
    let mut h = phone(cx, &dir, platform.clone());
    let asked = open_with_relay_push(&mut h);
    h.host(&saved_remote()).send(status(None, &asked));
    open_notifications(&mut h);

    h.click_on(&switch());

    assert!(
        platform
            .calls()
            .contains(&PlatformCall::RequestAuthorization),
        "a permission not asked yet is asked"
    );
    let sent = registers(h.host(&saved_remote()).sent());
    let [(registration, switches, _)] = sent.as_slice() else {
        panic!("one RegisterPush once granted: {sent:?}");
    };
    assert_eq!(*registration, fake_registration());
    assert_eq!(*switches, [true, true, true]);
    assert!(drawn(&mut h, &format!("{}-tick", switch())));
}

#[gpui::test]
fn a_denied_prompt_answer_shows_the_blocked_hint(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let platform = Arc::new(FakePushPlatform::default());
    platform.set_authorization(Authorization::NotDetermined);
    platform.set_request_answer(Authorization::Denied);
    let mut h = phone(cx, &dir, platform.clone());
    let asked = open_with_relay_push(&mut h);
    h.host(&saved_remote()).send(status(None, &asked));
    open_notifications(&mut h);

    h.click_on(&switch());

    assert!(
        platform
            .calls()
            .contains(&PlatformCall::RequestAuthorization),
        "the owner is asked"
    );
    assert!(drawn(&mut h, &hint()), "the blocked line shows");
    assert!(drawn(&mut h, &format!("{}-empty", switch())), "off");
    assert_eq!(registers(h.host(&saved_remote()).sent()), Vec::new());
}

#[gpui::test]
fn a_token_error_shows_the_failure_hint_and_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let platform = Arc::new(FakePushPlatform::default());
    platform.set_token(Err("no APNs token".to_owned()));
    let mut h = phone(cx, &dir, platform.clone());
    let asked = open_with_relay_push(&mut h);
    h.host(&saved_remote()).send(status(None, &asked));
    open_notifications(&mut h);

    h.click_on(&switch());

    assert!(drawn(&mut h, &hint()), "the failure line shows");
    assert!(drawn(&mut h, &format!("{}-empty", switch())), "off");
    assert_eq!(
        registers(h.host(&saved_remote()).sent()),
        Vec::new(),
        "nothing is registered"
    );
}

#[gpui::test]
fn a_failed_reregister_marks_home_and_the_notifications_row(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let platform = Arc::new(FakePushPlatform::default());
    let mut h = phone(cx, &dir, platform.clone());
    let asked = open_with_relay_push(&mut h);
    assert!(
        !drawn(&mut h, "phone-settings-open-mark"),
        "no hint, no mark"
    );

    // The daemon holds another token, and this phone's token read fails on
    // the re-register it asks for.
    platform.set_token(Err("no APNs token".to_owned()));
    h.host(&saved_remote())
        .send(status(Some("ffff0000"), &asked));

    assert!(
        drawn(&mut h, "phone-settings-open-mark"),
        "the failed re-register marks Home's Settings button"
    );
    h.click_on("phone-settings-open");
    assert!(
        drawn(&mut h, "phone-settings-row-notifications-mark"),
        "and the Notifications row"
    );
}

#[gpui::test]
fn a_daemon_refusal_shows_its_text_and_files_no_toast(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir, Arc::new(FakePushPlatform::default()));
    let asked = open_with_relay_push(&mut h);
    h.host(&saved_remote()).send(status(None, &asked));
    open_notifications(&mut h);
    h.click_on(&switch());
    let sent = registers(h.host(&saved_remote()).sent());
    let [(_, _, request_id)] = sent.as_slice() else {
        panic!("one RegisterPush: {sent:?}");
    };

    h.host(&saved_remote()).send(DaemonMessage::Error {
        message: "this device is not paired".to_owned(),
        request_id: Some(request_id.clone()),
        code: Some("invalid_request".to_owned()),
    });

    assert!(drawn(&mut h, &hint()), "the daemon's text shows");
    assert_eq!(
        h.root(|root, _| root.toasts().len()),
        0,
        "the refusal files no toast"
    );
    let again = status_requests(h.host(&saved_remote()).sent());
    assert_eq!(
        again.len(),
        1,
        "the switch asks for the status again: {again:?}"
    );
}

#[gpui::test]
fn an_ssh_host_never_shows_a_switch(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({ "saved_hosts": [{ "kind": "ssh", "name": "devbox" }] }),
    );
    let mut h = Harness::phone_with(cx, &dir, Arc::new(FakePushPlatform::default()));
    let ssh = Target::parse_ssh("devbox", None).expect("a valid SSH host");
    let mut features = support::daemon_features();
    features.push(FEATURE_RELAY_PUSH.to_owned());
    let sent = {
        let mut host = h.host(&ssh);
        host.connect_keeping_sent_with(features);
        host.sent()
    };
    open_notifications(&mut h);

    assert!(
        !drawn(&mut h, "phone-host-push-devbox"),
        "an SSH host offers no switch"
    );
    assert_eq!(status_requests(sent), Vec::<String>::new(), "nor is asked");
}

#[gpui::test]
fn two_hosts_at_push_1_stay_apart(cx: &mut TestAppContext) {
    const OTHER_HOST: &str = "den.tail1234.ts.net";
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({
            "saved_hosts": [
                { "name": REMOTE, "port": 8787 },
                { "name": OTHER_HOST, "port": 8787 },
            ]
        }),
    );
    let mut h = Harness::phone_with(cx, &dir, Arc::new(FakePushPlatform::default()));
    h.tokens().set(REMOTE, "t1").expect("seed a token");
    h.tokens().set(OTHER_HOST, "t2").expect("seed a token");
    let other = Target::tailnet(OTHER_HOST, Some(8787));
    let mine = open_with_relay_push(&mut h);
    let theirs = open_target_with_relay_push(&mut h, &other);
    open_notifications(&mut h);
    assert_eq!(mine, theirs, "each host numbers its own requests");

    h.host(&saved_remote())
        .send(status(Some(FakePushPlatform::TOKEN), &mine));

    assert!(
        drawn(&mut h, &format!("{}-tick", switch())),
        "the answered host reads on"
    );
    assert!(
        drawn(&mut h, &format!("phone-host-push-{OTHER_HOST}-empty")),
        "the other host's switch waits for its own reply"
    );
}

#[gpui::test]
fn a_tap_before_the_host_connects_opens_the_session_once_it_is_listed(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let platform = Arc::new(FakePushPlatform::default());
    let mut h = phone(cx, &dir, platform.clone());
    tap(&platform, REMOTE, "s1", &mut h);
    assert_eq!(
        shown_session(&mut h),
        None,
        "the host has listed nothing yet"
    );

    h.host(&saved_remote()).connect();
    assert!(
        drawn(&mut h, "phone-open-pending"),
        "the wait shows on Home"
    );

    h.host(&saved_remote()).send(remote_list("working", "s2"));

    assert_eq!(
        shown_session(&mut h),
        Some((REMOTE.to_owned(), "s1".to_owned()))
    );
    assert!(
        !drawn(&mut h, "phone-open-pending"),
        "the wait is over once the session shows"
    );
}

#[gpui::test]
fn a_tap_for_a_session_no_longer_listed_notes_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let platform = Arc::new(FakePushPlatform::default());
    let mut h = phone(cx, &dir, platform.clone());
    h.host(&saved_remote()).connect();
    h.host(&saved_remote()).send(remote_list("working", "s2"));

    tap(&platform, REMOTE, "gone", &mut h);

    assert_eq!(shown_session(&mut h), None, "no session page opens");
    assert_eq!(
        open_note(&mut h),
        Some(format!("That session is no longer on {REMOTE}."))
    );
    assert!(drawn(&mut h, "phone-open-note"), "the line shows on Home");
}

#[gpui::test]
fn a_tap_for_an_unsaved_host_notes_it(cx: &mut TestAppContext) {
    const OTHER_HOST: &str = "den.tail1234.ts.net";
    let dir = TestDir::new();
    let platform = Arc::new(FakePushPlatform::default());
    let mut h = phone(cx, &dir, platform.clone());

    tap(&platform, OTHER_HOST, "s1", &mut h);

    assert_eq!(shown_session(&mut h), None);
    assert_eq!(
        open_note(&mut h),
        Some(format!("{OTHER_HOST} is not saved on this phone."))
    );
    assert!(
        drawn(&mut h, "phone-open-note"),
        "the line shows on the hosts screen"
    );
}

#[gpui::test]
fn the_wait_line_shows_while_no_host_is_open(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let platform = Arc::new(FakePushPlatform::default());
    let mut h = phone(cx, &dir, platform.clone());

    tap(&platform, REMOTE, "s1", &mut h);

    assert!(
        drawn(&mut h, "phone-open-pending"),
        "the wait shows on the hosts screen"
    );
}

#[gpui::test]
fn a_waiting_notification_tap_closes_settings_and_opens_its_session(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let platform = Arc::new(FakePushPlatform::default());
    let mut h = phone(cx, &dir, platform.clone());
    h.click_on("phone-settings-open");
    assert!(drawn(&mut h, "phone-settings"), "Settings shows");

    tap(&platform, REMOTE, "s1", &mut h);

    assert!(
        !drawn(&mut h, "phone-settings"),
        "Settings closes for the tap"
    );
    assert!(
        drawn(&mut h, "phone-open-pending"),
        "the wait shows on Home"
    );

    h.host(&saved_remote()).connect();
    h.host(&saved_remote()).send(remote_list("working", "s2"));

    assert_eq!(
        shown_session(&mut h),
        Some((REMOTE.to_owned(), "s1".to_owned()))
    );
}

#[gpui::test]
fn a_tap_for_a_host_not_paired_notes_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({ "saved_hosts": [{ "name": REMOTE, "port": 8787 }] }),
    );
    let platform = Arc::new(FakePushPlatform::default());
    let mut h = Harness::phone_with(cx, &dir, platform.clone());

    tap(&platform, REMOTE, "s1", &mut h);

    assert_eq!(
        open_note(&mut h),
        Some(format!("{REMOTE} is not paired with this phone."))
    );
    assert!(
        drawn(&mut h, "phone-open-note"),
        "the line shows on the hosts screen"
    );
    h.host(&saved_remote()).connect();
    h.host(&saved_remote()).send(remote_list("working", "s2"));
    assert_eq!(shown_session(&mut h), None, "the pending open is cleared");
}

#[gpui::test]
fn a_tap_after_returning_to_the_foreground_waits_for_the_fresh_list(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let platform = Arc::new(FakePushPlatform::default());
    let mut h = phone(cx, &dir, platform.clone());
    h.host(&saved_remote()).connect();
    h.host(&saved_remote()).send(remote_list("working", "s2"));

    h.cx.simulate_visibility_change(gpui::WindowVisibility::Hidden);
    h.cx.simulate_visibility_change(gpui::WindowVisibility::Visible);
    h.cx.run_until_parked();
    tap(&platform, REMOTE, "s1", &mut h);
    assert_eq!(
        shown_session(&mut h),
        None,
        "the list from before the suspension decides nothing"
    );

    h.host(&saved_remote()).send(remote_list("working", "s2"));
    assert_eq!(
        shown_session(&mut h),
        Some((REMOTE.to_owned(), "s1".to_owned()))
    );
}

#[gpui::test]
fn a_second_tap_replaces_the_first(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let platform = Arc::new(FakePushPlatform::default());
    let mut h = phone(cx, &dir, platform.clone());
    tap(&platform, REMOTE, "s1", &mut h);
    tap(&platform, REMOTE, "s2", &mut h);

    h.host(&saved_remote()).connect();
    h.host(&saved_remote()).send(remote_list("working", "s3"));

    assert_eq!(
        shown_session(&mut h),
        Some((REMOTE.to_owned(), "s2".to_owned()))
    );
}

#[gpui::test]
fn a_tap_matches_the_host_name_however_it_is_spelt(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let platform = Arc::new(FakePushPlatform::default());
    let mut h = phone(cx, &dir, platform.clone());
    let spelt = format!(" {}.. ", REMOTE.to_uppercase());
    tap(&platform, &spelt, "s1", &mut h);

    h.host(&saved_remote()).connect();
    h.host(&saved_remote()).send(remote_list("working", "s2"));

    assert_eq!(
        shown_session(&mut h),
        Some((REMOTE.to_owned(), "s1".to_owned()))
    );
}

#[gpui::test]
fn navigating_clears_a_pending_open(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let platform = Arc::new(FakePushPlatform::default());
    let mut h = phone(cx, &dir, platform.clone());
    h.host(&saved_remote()).connect();
    tap(&platform, REMOTE, "s1", &mut h);
    assert!(drawn(&mut h, "phone-open-pending"), "the wait shows");

    open_hosts(&mut h);
    h.click_on("phone-host-add-another");

    h.host(&saved_remote()).send(remote_list("working", "s2"));

    assert_eq!(
        shown_session(&mut h),
        None,
        "the hosts form took the tap's place"
    );
    assert!(!drawn(&mut h, "phone-open-pending"), "the wait is gone");
}

#[gpui::test]
fn a_tap_that_notes_while_a_session_shows_returns_home_with_the_note(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let platform = Arc::new(FakePushPlatform::default());
    let mut h = phone(cx, &dir, platform.clone());
    h.host(&saved_remote()).connect();
    h.host(&saved_remote()).send(remote_list("working", "s2"));
    tap(&platform, REMOTE, "s1", &mut h);
    assert_eq!(
        shown_session(&mut h),
        Some((REMOTE.to_owned(), "s1".to_owned())),
        "the first tap opens its session"
    );

    tap(&platform, REMOTE, "gone", &mut h);

    assert_eq!(shown_session(&mut h), None, "the page closes for the note");
    assert_eq!(
        open_note(&mut h),
        Some(format!("That session is no longer on {REMOTE}."))
    );
    assert!(drawn(&mut h, "phone-open-note"), "Home shows the note");
}

#[gpui::test]
fn opening_a_sheet_clears_a_pending_open(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let platform = Arc::new(FakePushPlatform::default());
    let mut h = phone(cx, &dir, platform.clone());
    h.host(&saved_remote()).connect();
    h.host(&saved_remote()).send(remote_list("working", "s2"));
    h.cx.simulate_visibility_change(gpui::WindowVisibility::Hidden);
    h.cx.simulate_visibility_change(gpui::WindowVisibility::Visible);
    h.cx.run_until_parked();
    tap(&platform, REMOTE, "s1", &mut h);
    assert_eq!(
        shown_session(&mut h),
        None,
        "no list has come since the foreground"
    );

    let opened = h.cx.update(|window, cx| {
        let phone = window
            .root::<PhoneRoot>()
            .flatten()
            .expect("the window's root is the phone's");
        phone.update(cx, |phone, cx| phone.open_sheet_on("s1", cx))
    });
    assert!(opened, "the listed session's sheet opens");

    h.host(&saved_remote()).send(remote_list("working", "s2"));

    assert_eq!(
        shown_session(&mut h),
        None,
        "the sheet took the tap's place"
    );
}
