//! The phone's Settings: a `Settings` button in the header over Home and
//! over the hosts screen opens a category list in their place; a category's
//! row opens its page, and Back or a committed back gesture steps from the
//! page to the list, then out of Settings. Opening a session closes it.

use std::sync::Arc;

use crate::support;

use daemon_client::device_tokens::DeviceTokenStore as _;
use gpui::{BackGesturePhase, Pixels, TestAppContext, point, px};
use protocol::{DaemonMessage, FEATURE_RELAY_PUSH};
use rustling_tulip_native::phone::PhoneRoot;
use rustling_tulip_native::phone::push::FakePushPlatform;
use serde_json::json;
use support::{Harness, REMOTE, TestDir, saved_remote, seed_ui, session};

/// The least height of a row or a target.
fn touch_target() -> Pixels {
    px(44.0)
}

/// Whether the last frame drew what `selector` tags.
fn drawn(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).size.width > px(0.0)
}

/// The saved host's Notifications switch.
fn switch() -> String {
    format!("phone-host-push-{REMOTE}")
}

/// A phone with [`REMOTE`] saved and paired, its push through `platform`,
/// the host not connected yet.
fn phone<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    platform: Arc<FakePushPlatform>,
) -> Harness<'a> {
    seed_ui(
        dir,
        &json!({ "saved_hosts": [{ "name": REMOTE, "port": 8787 }] }),
    );
    let h = Harness::phone_with(cx, dir, platform);
    h.tokens().set(REMOTE, "t1").expect("seed a token");
    h
}

/// A phone whose [`REMOTE`] is open, taking relay pushes and listing `s1`.
fn open_phone<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    platform: Arc<FakePushPlatform>,
) -> Harness<'a> {
    let mut h = phone(cx, dir, platform);
    let mut host = h.host(&saved_remote());
    host.connect_with_features(&[FEATURE_RELAY_PUSH]);
    host.send(DaemonMessage::Sessions {
        sessions: vec![session("s1").build()],
    });
    h
}

/// The host and id of the session the phone shows in place of Home.
fn shown_session(h: &mut Harness<'_>) -> Option<String> {
    h.cx.update(|window, cx| {
        let phone = window
            .root::<PhoneRoot>()
            .flatten()
            .expect("the window's root is the phone's");
        phone.read(cx).shown_session().map(|(_, id)| id.to_owned())
    })
}

/// A back gesture in `phase` from Settings' leading edge.
fn back_gesture(h: &mut Harness<'_>, phase: BackGesturePhase) {
    let page = h.bounds("phone-settings");
    let at = point(page.origin.x + px(4.0), page.origin.y + px(200.0));
    h.back_gesture(phase, at);
}

#[gpui::test]
fn home_shows_the_settings_button(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir, Arc::new(FakePushPlatform::default()));

    assert!(drawn(&mut h, "phone-home"), "Home shows");
    assert!(
        drawn(&mut h, "phone-settings-open"),
        "under its Settings button"
    );
    assert!(h.bounds("phone-settings-open").size.height >= touch_target());
    assert!(
        h.bounds("phone-settings-open").bottom() <= h.bounds("phone-home").top(),
        "the header sits over the list"
    );
}

#[gpui::test]
fn the_settings_button_shows_the_category_list(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir, Arc::new(FakePushPlatform::default()));

    h.click_on("phone-settings-open");

    assert!(drawn(&mut h, "phone-settings"), "Settings shows");
    assert!(!drawn(&mut h, "phone-home"), "in Home's place");
    assert!(drawn(&mut h, "phone-settings-back"));
    assert!(h.bounds("phone-settings-back").size.height >= touch_target());
    let row = "phone-settings-row-notifications";
    assert!(drawn(&mut h, row), "the Notifications row shows");
    assert!(h.bounds(row).size.height >= touch_target());
    assert!(!drawn(&mut h, &switch()), "the list draws no switch");
}

#[gpui::test]
fn the_notifications_row_shows_an_open_hosts_switch(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir, Arc::new(FakePushPlatform::default()));

    h.click_on("phone-settings-open");
    h.click_on("phone-settings-row-notifications");

    assert!(drawn(&mut h, &switch()), "the switch shows");
    assert!(h.bounds(&switch()).size.height >= touch_target());
    assert!(
        !drawn(&mut h, "phone-settings-row-notifications"),
        "the page takes the list's place"
    );
    assert!(!drawn(&mut h, "phone-settings-notifications-empty"));
}

#[gpui::test]
fn the_notifications_page_says_when_no_host_offers_a_switch(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir, Arc::new(FakePushPlatform::default()));
    h.host(&saved_remote()).connect();

    h.click_on("phone-settings-open");
    h.click_on("phone-settings-row-notifications");

    assert!(drawn(&mut h, "phone-settings-notifications-empty"));
    assert!(!drawn(&mut h, &switch()));
}

#[gpui::test]
fn back_twice_returns_home(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir, Arc::new(FakePushPlatform::default()));
    h.click_on("phone-settings-open");
    h.click_on("phone-settings-row-notifications");

    h.click_on("phone-settings-back");
    assert!(
        drawn(&mut h, "phone-settings-row-notifications"),
        "the page goes back to the list"
    );
    assert!(!drawn(&mut h, &switch()));

    h.click_on("phone-settings-back");
    assert!(!drawn(&mut h, "phone-settings"), "the list closes");
    assert!(drawn(&mut h, "phone-home"), "Home shows again");
}

#[gpui::test]
fn the_back_gesture_steps_back_the_same_way(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir, Arc::new(FakePushPlatform::default()));
    h.click_on("phone-settings-open");
    h.click_on("phone-settings-row-notifications");

    back_gesture(&mut h, BackGesturePhase::Started);
    assert!(
        drawn(&mut h, &switch()),
        "a gesture still under way takes no step"
    );

    back_gesture(&mut h, BackGesturePhase::Committed);
    assert!(
        drawn(&mut h, "phone-settings-row-notifications"),
        "to the list"
    );

    back_gesture(&mut h, BackGesturePhase::Committed);
    assert!(!drawn(&mut h, "phone-settings"), "the list closes");
    assert!(drawn(&mut h, "phone-home"), "Home shows again");
}

#[gpui::test]
fn a_notification_tap_under_settings_shows_its_session(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let platform = Arc::new(FakePushPlatform::default());
    let mut h = open_phone(cx, &dir, platform.clone());
    h.click_on("phone-settings-open");
    h.click_on("phone-settings-row-notifications");

    platform.tap(REMOTE, "s1");
    h.cx.run_until_parked();

    assert_eq!(shown_session(&mut h).as_deref(), Some("s1"));
    assert!(drawn(&mut h, "phone-session"), "the session page shows");
    assert!(!drawn(&mut h, "phone-settings"), "Settings closed for it");

    h.click_on("phone-session-back");
    assert!(
        drawn(&mut h, "phone-home"),
        "Back returns to Home, not Settings"
    );
}

#[gpui::test]
fn the_hosts_screen_shows_the_settings_button(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir, Arc::new(FakePushPlatform::default()));

    assert!(
        drawn(&mut h, &format!("phone-host-row-{REMOTE}")),
        "no host is open: the hosts screen shows"
    );
    assert!(drawn(&mut h, "phone-settings-open"));

    h.click_on("phone-settings-open");
    assert!(drawn(&mut h, "phone-settings"), "Settings shows");
    assert!(!drawn(&mut h, &format!("phone-host-row-{REMOTE}")));

    h.click_on("phone-settings-back");
    assert!(
        drawn(&mut h, &format!("phone-host-row-{REMOTE}")),
        "Back returns to the hosts screen"
    );
}
