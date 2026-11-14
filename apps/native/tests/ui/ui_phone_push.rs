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
    ApnsEnvironment, ClientMessage, DaemonMessage, FEATURE_RELAY_PUSH, PushRegistration, Secret,
};
use rustling_tulip_native::phone::push::{
    Authorization, FakePushPlatform, PlatformCall, PushPlatform, UnsupportedPlatform,
};
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
    let mut features = support::daemon_features();
    features.push(FEATURE_RELAY_PUSH.to_owned());
    let mut host = h.host(&saved_remote());
    host.connect_keeping_sent_with(features);
    let asked: Vec<String> = host
        .sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::GetPushStatus { request_id } => Some(request_id),
            _ => None,
        })
        .collect();
    let [request_id] = asked.as_slice() else {
        panic!("one GetPushStatus on open: {asked:?}");
    };
    request_id.clone()
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

#[gpui::test]
fn the_switch_shows_for_a_tailnet_host_that_takes_relay_push(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir, Arc::new(FakePushPlatform::default()));
    h.host(&saved_remote())
        .connect_with_features(&[FEATURE_RELAY_PUSH]);

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

    assert!(drawn(&mut h, "sidebar-panel"), "the session list shows");
    assert!(
        !drawn(&mut h, &format!("phone-host-row-{REMOTE}")),
        "no host row while the sessions list"
    );
    assert!(drawn(&mut h, &switch()), "the switch is still there");
    assert!(
        h.bounds(&switch()).size.height >= px(44.0),
        "a full touch target"
    );
}
