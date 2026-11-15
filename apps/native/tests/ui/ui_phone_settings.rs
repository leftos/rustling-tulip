//! The phone's Settings: a `Settings` button in the header over Home and
//! over the hosts screen opens a category list in their place; a category's
//! row opens its page, and Back or a committed back gesture steps from the
//! page to the list, then out of Settings. Opening a session closes it.

use std::sync::Arc;

use crate::support;

use daemon_client::device_tokens::DeviceTokenStore as _;
use gpui::{BackGesturePhase, Pixels, TestAppContext, point, px};
use protocol::{DaemonMessage, FEATURE_RELAY_PUSH};
use rustling_tulip_native::builtin_themes::DRACULA;
use rustling_tulip_native::phone::PhoneRoot;
use rustling_tulip_native::phone::push::FakePushPlatform;
use rustling_tulip_native::theme_store::{BUILT_INS, THEMES_DIR};
use rustling_tulip_native::{fonts, ui_theme};
use serde_json::json;
use support::{Harness, REMOTE, TestDir, saved_remote, saved_ui, seed_ui, session};

/// The least height of a row or a target.
fn touch_target() -> Pixels {
    px(44.0)
}

/// Whether the last frame drew what `selector` tags.
fn drawn(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).size.width > px(0.0)
}

/// Opens Settings, then its Hosts page, where the saved hosts and the
/// add form draw.
fn open_hosts(h: &mut Harness<'_>) {
    h.click_on("phone-settings-open");
    h.click_on("phone-settings-row-hosts");
}

/// Opens Settings, then its Appearance page, where the theme list and the
/// text-size row draw.
fn open_appearance(h: &mut Harness<'_>) {
    h.click_on("phone-settings-open");
    h.click_on("phone-settings-row-appearance");
}

/// Types `text` key by key into the focused field.
fn type_text(h: &mut Harness<'_>, text: &str) {
    let keys: Vec<String> = text.chars().map(|c| c.to_string()).collect();
    h.keys(&keys.join(" "));
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

#[gpui::test]
fn the_hosts_page_lists_the_saved_hosts_and_adds_one(cx: &mut TestAppContext) {
    const TYPED: &str = "den.tail1234.ts.net";
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir, Arc::new(FakePushPlatform::default()));

    open_hosts(&mut h);

    assert!(
        drawn(&mut h, &format!("phone-host-row-{REMOTE}")),
        "the saved host's row shows"
    );
    assert!(
        !drawn(&mut h, "phone-host-name"),
        "the form waits for a tap"
    );
    h.click_on("phone-host-add-another");
    assert!(
        drawn(&mut h, "phone-host-name"),
        "Add a host opens the form"
    );

    h.click_on("phone-host-name");
    type_text(&mut h, TYPED);
    h.click_on("phone-host-port");
    h.keys("end backspace backspace backspace backspace");
    type_text(&mut h, "8787");
    h.click_on("phone-host-add");

    let saved = saved_ui(&dir)["saved_hosts"].clone();
    assert!(
        saved
            .as_array()
            .is_some_and(|hosts| hosts.iter().any(|host| host["name"] == TYPED)),
        "Add saves it: {saved}"
    );
}

#[gpui::test]
fn home_draws_no_add_host_footer(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir, Arc::new(FakePushPlatform::default()));

    assert!(drawn(&mut h, "phone-home"), "Home shows");
    for gone in [
        "phone-host-add-another",
        "phone-host-name",
        "phone-host-port",
    ] {
        assert!(!drawn(&mut h, gone), "Home draws no {gone}");
    }
    assert!(
        !drawn(&mut h, &format!("phone-host-row-{REMOTE}")),
        "nor the saved hosts' rows"
    );
}

#[gpui::test]
fn the_hosts_screen_still_offers_add_a_host_with_no_host_open(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir, Arc::new(FakePushPlatform::default()));

    assert!(
        drawn(&mut h, &format!("phone-host-row-{REMOTE}")),
        "the hosts screen shows"
    );
    assert!(!drawn(&mut h, "phone-settings"), "with no Settings open");
    h.click_on("phone-host-add-another");
    assert!(
        drawn(&mut h, "phone-host-name"),
        "Add a host opens the form"
    );
}

#[gpui::test]
fn a_host_connecting_while_the_add_form_is_open_keeps_the_form_on_screen(cx: &mut TestAppContext) {
    const TYPED: &str = "den.tail1234.ts.net";
    let dir = TestDir::new();
    let mut h = phone(cx, &dir, Arc::new(FakePushPlatform::default()));

    h.click_on("phone-host-add-another");
    h.click_on("phone-host-name");
    type_text(&mut h, TYPED);
    assert!(!drawn(&mut h, "phone-settings"), "no Settings yet");

    h.host(&saved_remote()).connect();

    assert!(
        drawn(&mut h, "phone-settings"),
        "Settings shows in its place"
    );
    assert!(
        !drawn(&mut h, "phone-settings-row-hosts"),
        "on the Hosts page"
    );
    for selector in ["phone-host-name", "phone-host-port", "phone-host-add"] {
        assert!(drawn(&mut h, selector), "the form's {selector} paints");
    }

    h.click_on("phone-host-add");
    let added = h
        .opened_targets()
        .pop()
        .expect("Add connects the typed host");
    assert!(
        added
            .tailnet_host()
            .is_some_and(|host| host.name() == TYPED),
        "the form kept the typed name: {added:?}"
    );

    h.click_on("phone-settings-back");
    h.click_on("phone-settings-back");
    assert!(drawn(&mut h, "phone-home"), "Back leads to Home");
}

#[gpui::test]
fn appearance_lists_the_themes_and_marks_the_current_one(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let themes = dir.path().join(THEMES_DIR);
    std::fs::create_dir_all(&themes).expect("make the themes folder");
    std::fs::write(themes.join("broken.json"), "{").expect("write a theme that does not load");
    let mut h = open_phone(cx, &dir, Arc::new(FakePushPlatform::default()));

    open_appearance(&mut h);

    assert!(drawn(&mut h, "phone-settings"), "Settings shows");
    for (id, _, _) in BUILT_INS {
        let row = format!("phone-theme-{id}");
        assert!(drawn(&mut h, &row), "the {id} theme shows");
        assert!(
            h.bounds(&row).size.height >= touch_target(),
            "{row} is a touch target"
        );
    }
    assert!(
        drawn(&mut h, "phone-theme-check-nord"),
        "Nord is the theme in use"
    );
    assert!(!drawn(&mut h, "phone-theme-check-dracula"));
    assert!(
        !drawn(&mut h, "phone-theme-broken"),
        "a theme that does not load is not listed"
    );
    assert!(
        !drawn(&mut h, "phone-settings-row-appearance"),
        "the page takes the list's place"
    );
}

#[gpui::test]
fn tapping_a_theme_switches_and_saves_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir, Arc::new(FakePushPlatform::default()));
    open_appearance(&mut h);

    h.click_on("phone-theme-dracula");

    assert_eq!(h.root(|_, cx| ui_theme::theme(cx)), DRACULA, "it draws");
    assert_eq!(saved_ui(&dir)["theme"], "dracula", "and is saved");
    assert!(drawn(&mut h, "phone-theme-check-dracula"), "the mark moves");
    assert!(!drawn(&mut h, "phone-theme-check-nord"));

    h.click_on("phone-theme-dracula");
    assert_eq!(
        saved_ui(&dir)["theme"],
        "dracula",
        "a tap on the theme in use changes nothing"
    );
}

#[gpui::test]
fn the_text_size_stepper_moves_one_step_and_stops_at_both_ends(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir, Arc::new(FakePushPlatform::default()));
    open_appearance(&mut h);

    for selector in ["phone-text-size-down", "phone-text-size-up"] {
        assert!(drawn(&mut h, selector), "{selector} shows");
        assert!(h.bounds(selector).size.height >= touch_target());
    }

    h.click_on("phone-text-size-up");
    assert_eq!(saved_ui(&dir)["ui_text_size"], 1, "one step up");
    assert_eq!(h.root(|_, cx| fonts::ui_text_size(cx)), 1, "and it draws");
    h.click_on("phone-text-size-down");
    assert_eq!(saved_ui(&dir)["ui_text_size"], 0, "and one step back down");

    for _ in 0..4 {
        h.click_on("phone-text-size-up");
    }
    assert_eq!(saved_ui(&dir)["ui_text_size"], 4, "the top");
    h.click_on("phone-text-size-up");
    assert_eq!(
        saved_ui(&dir)["ui_text_size"],
        4,
        "a tap at the top changes nothing"
    );

    for _ in 0..6 {
        h.click_on("phone-text-size-down");
    }
    assert_eq!(saved_ui(&dir)["ui_text_size"], -2, "the bottom");
    h.click_on("phone-text-size-down");
    assert_eq!(
        saved_ui(&dir)["ui_text_size"],
        -2,
        "a tap at the bottom changes nothing"
    );
}

#[gpui::test]
fn a_phone_over_a_saved_theme_and_size_boots_in_them(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(&dir, &json!({ "theme": "dracula", "ui_text_size": 2 }));
    let mut h = Harness::phone_with(cx, &dir, Arc::new(FakePushPlatform::default()));

    h.click_on("phone-settings-open");
    h.click_on("phone-settings-row-appearance");

    assert!(
        drawn(&mut h, "phone-theme-check-dracula"),
        "the saved theme is marked"
    );
    assert!(!drawn(&mut h, "phone-theme-check-nord"));

    h.click_on("phone-text-size-up");
    assert_eq!(
        saved_ui(&dir)["ui_text_size"],
        3,
        "the stepper starts at the saved 2"
    );
    h.click_on("phone-text-size-down");
    h.click_on("phone-text-size-down");
    assert_eq!(saved_ui(&dir)["ui_text_size"], 1);
}

#[gpui::test]
fn a_saved_theme_that_is_gone_marks_the_default(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(&dir, &json!({ "theme": "gone" }));
    let mut h = Harness::phone_with(cx, &dir, Arc::new(FakePushPlatform::default()));

    h.click_on("phone-settings-open");
    h.click_on("phone-settings-row-appearance");

    assert!(
        drawn(&mut h, "phone-theme-nord"),
        "the built-in the boot falls back to is listed"
    );
    assert!(
        drawn(&mut h, "phone-theme-check-nord"),
        "and marked as the theme in use"
    );
}

#[gpui::test]
fn a_saved_theme_that_is_broken_marks_the_default(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let themes = dir.path().join(THEMES_DIR);
    std::fs::create_dir_all(&themes).expect("make the themes folder");
    std::fs::write(themes.join("broken.json"), "{").expect("write a theme that does not load");
    seed_ui(&dir, &json!({ "theme": "broken" }));
    let mut h = Harness::phone_with(cx, &dir, Arc::new(FakePushPlatform::default()));

    h.click_on("phone-settings-open");
    h.click_on("phone-settings-row-appearance");

    assert!(
        !drawn(&mut h, "phone-theme-broken"),
        "a theme that does not load is not listed"
    );
    assert!(
        drawn(&mut h, "phone-theme-check-nord"),
        "the built-in the boot falls back to is marked"
    );
}
