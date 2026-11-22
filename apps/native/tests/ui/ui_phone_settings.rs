//! The phone's Settings: a `Settings` button in the header over Home and
//! over the hosts screen opens a category list in their place; a category's
//! row opens its page, and Back or a committed back gesture steps from the
//! page to the list, then out of Settings. Opening a session closes it.

use std::sync::Arc;

use crate::support;

use base64::Engine as _;
use daemon_client::device_tokens::DeviceTokenStore as _;
use gpui::{BackGesturePhase, Pixels, TestAppContext, point, px};
use protocol::{DaemonMessage, FEATURE_RELAY_PUSH};
use rustling_tulip_native::builtin_themes::DRACULA;
use rustling_tulip_native::file_fetch::FetchRequest;
use rustling_tulip_native::phone::PhoneRoot;
use rustling_tulip_native::phone::push::FakePushPlatform;
use rustling_tulip_native::target::HostKey;
use rustling_tulip_native::theme_store::{BUILT_INS, THEMES_DIR};
use rustling_tulip_native::{LinkAction, PhoneFetched};
use rustling_tulip_native::{fonts, ui_theme};
use serde_json::json;
use support::{Harness, REMOTE, TestDir, repo, saved_remote, saved_ui, seed_ui, session};

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
    for key in [
        "notifications",
        "hosts",
        "appearance",
        "quick-replies",
        "keys",
        "storage",
    ] {
        let row = format!("phone-settings-row-{key}");
        assert!(drawn(&mut h, &row), "the {key} row shows");
        assert!(h.bounds(&row).size.height >= touch_target());
    }
    assert!(
        h.bounds("phone-settings-row-keys").bottom()
            <= h.bounds("phone-settings-row-storage").top(),
        "Storage is the last row"
    );
    assert!(!drawn(&mut h, &switch()), "the list draws no switch");
}

/// The copy of `name` fetched from [`REMOTE`] that the phone keeps, `size`
/// bytes long.
fn kept(name: &str, size: u64) -> PhoneFetched {
    PhoneFetched {
        host: REMOTE.to_owned(),
        host_path: format!("/abs/{name}"),
        local_path: format!("{REMOTE}/root/abs/{name}"),
        size,
        host_label: REMOTE.to_owned(),
    }
}

/// Hands the phone two saved fetches, 1024 and 2048 bytes, whose copies sit
/// under `copies`.
fn keep_two(h: &mut Harness<'_>, copies: &std::path::Path) {
    for (name, size) in [("one.txt", 1024), ("two.txt", 2048)] {
        let saved = kept(name, size);
        let path = copies.join(&saved.local_path);
        std::fs::create_dir_all(path.parent().expect("a copy has a folder"))
            .expect("make the copy's folder");
        std::fs::write(&path, b"copy").expect("write the copy");
        h.root_update(|root, _, cx| root.phone_file_saved(saved, cx));
    }
    // A save opens the viewer over Home; Back shows Home again.
    h.click_on("phone-viewer-back");
}

/// The kept copies' size as the Storage page shows it, and its error line.
fn storage(h: &mut Harness<'_>) -> (String, Option<String>) {
    h.cx.update(|window, cx| {
        let phone = window
            .root::<PhoneRoot>()
            .flatten()
            .expect("the window's root is the phone's");
        let phone = phone.read(cx);
        (
            phone.kept_size_text(),
            phone.storage_error().map(str::to_owned),
        )
    })
}

/// The entries `fetched.json` in `dir` holds.
fn saved_index(dir: &TestDir) -> Vec<serde_json::Value> {
    let text = std::fs::read_to_string(dir.path().join("fetched.json")).expect("read the index");
    let index: serde_json::Value = serde_json::from_str(&text).expect("parse the index");
    index["entries"].as_array().cloned().unwrap_or_default()
}

#[gpui::test]
fn the_storage_row_opens_a_page_with_the_kept_size(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let copies = dir.path().join("copies");
    let mut h = open_phone(cx, &dir, Arc::new(FakePushPlatform::default()));
    h.root_update(|root, _, _| root.set_remote_files_root(copies.clone()));
    keep_two(&mut h, &copies);

    h.click_on("phone-settings-open");
    h.click_on("phone-settings-row-storage");

    assert!(
        drawn(&mut h, "phone-settings-storage-size"),
        "the size shows"
    );
    assert!(drawn(&mut h, "phone-settings-storage-clear"), "with Clear");
    assert!(h.bounds("phone-settings-storage-clear").size.height >= touch_target());
    assert!(
        !drawn(&mut h, "phone-settings-storage-error"),
        "a fresh page shows no error"
    );
    assert_eq!(storage(&mut h), ("3.0 KB".to_owned(), None));
    let entries = saved_index(&dir);
    assert_eq!(entries.len(), 2, "both copies are saved: {entries:?}");
    assert_eq!(entries[0]["host_path"], "/abs/one.txt");
    assert_eq!(entries[1]["size"], 2048);
}

#[gpui::test]
fn a_saved_fetch_on_the_phone_is_kept_in_the_index(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let copies = dir.path().join("copies");
    let mut h = open_phone(cx, &dir, Arc::new(FakePushPlatform::default()));
    let mut host = h.host(&saved_remote());
    host.send(DaemonMessage::Repos {
        repos: vec![repo("r1", "C:/repos/repo")],
    });
    host.send(DaemonMessage::Sessions {
        sessions: vec![session("s1").worktree("r1").build()],
    });
    host.sent();
    h.root_update(|root, _, _| root.set_remote_files_root(copies.clone()));
    let request = FetchRequest {
        host: HostKey::of(&saved_remote()),
        repo_id: "r1".to_owned(),
        repo_name: "repo".to_owned(),
        worktree_path: Some("C:/wt/r1".to_owned()),
        rel_path: "notes.txt".to_owned(),
        host_path: false,
        line: None,
        column: None,
        open_when_done: true,
        action: LinkAction::DefaultApp,
    };
    let id = h
        .root_update(move |root, _, cx| root.fetch_remote_file(request, cx))
        .expect("a fetch on a remote host starts");

    let mut host = h.host(&saved_remote());
    host.send(DaemonMessage::FileFetchStarted {
        id: id.clone(),
        resolved_path: r"\\?\C:\repos\repo\notes.txt".to_owned(),
        size: 4,
    });
    host.send(DaemonMessage::FileChunk {
        id: id.clone(),
        seq: 0,
        data_b64: base64::engine::general_purpose::STANDARD.encode(b"note"),
    });
    host.send(DaemonMessage::FileFetchDone { id });

    let local = format!("{REMOTE}/repo/notes.txt");
    assert_eq!(
        std::fs::read(copies.join(&local)).expect("the copy is saved"),
        b"note"
    );
    assert!(h.opened().is_empty(), "the phone opens nothing itself");
    let entries = saved_index(&dir);
    assert_eq!(entries.len(), 1, "the saved fetch is kept: {entries:?}");
    assert_eq!(entries[0]["host"], REMOTE);
    assert_eq!(entries[0]["host_path"], r"C:\repos\repo\notes.txt");
    assert_eq!(entries[0]["local_path"], local);
    assert_eq!(entries[0]["size"], 4);
    assert_eq!(storage(&mut h).0, "4 B");
}

#[gpui::test]
fn clear_empties_the_kept_copies(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let copies = dir.path().join("copies");
    let mut h = open_phone(cx, &dir, Arc::new(FakePushPlatform::default()));
    h.root_update(|root, _, _| root.set_remote_files_root(copies.clone()));
    keep_two(&mut h, &copies);
    h.click_on("phone-settings-open");
    h.click_on("phone-settings-row-storage");

    h.click_on("phone-settings-storage-clear");

    assert_eq!(storage(&mut h), ("0 B".to_owned(), None));
    assert!(!drawn(&mut h, "phone-settings-storage-error"));
    assert!(saved_index(&dir).is_empty(), "the saved index is empty");
    for name in ["one.txt", "two.txt"] {
        assert!(
            !copies.join(kept(name, 0).local_path).exists(),
            "{name}'s copy is deleted"
        );
    }
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
fn settings_opens_at_its_top_after_closing_scrolled_down(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let platform = Arc::new(FakePushPlatform::default());
    let mut h = open_phone(cx, &dir, platform.clone());
    h.cx.simulate_resize(gpui::size(px(390.0), px(844.0)));
    h.click_on("phone-settings-open");
    h.click_on("phone-settings-row-keys");
    let top = h.bounds("phone-keys-list").top();
    h.scroll_into_view("phone-settings", "phone-keys-restore");
    assert!(
        h.bounds("phone-keys-list").top() < top,
        "the Keys page scrolled down"
    );

    platform.tap(REMOTE, "s1");
    h.cx.run_until_parked();
    assert_eq!(
        shown_session(&mut h).as_deref(),
        Some("s1"),
        "the tap closed Settings for its session"
    );
    h.click_on("phone-session-back");
    h.cx.update(|window, cx| {
        let phone = window
            .root::<PhoneRoot>()
            .flatten()
            .expect("the window's root is the phone's");
        phone.update(cx, PhoneRoot::open_settings_keys);
    });

    assert_eq!(
        h.bounds("phone-keys-list").top(),
        top,
        "Settings opens again at its top"
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
fn leaving_the_hosts_page_closes_the_add_form(cx: &mut TestAppContext) {
    const TYPED: &str = "den.tail1234.ts.net";
    let dir = TestDir::new();
    let mut h = phone(cx, &dir, Arc::new(FakePushPlatform::default()));

    open_hosts(&mut h);
    h.click_on("phone-host-add-another");
    h.click_on("phone-host-name");
    type_text(&mut h, TYPED);
    assert!(drawn(&mut h, "phone-host-name"), "the form shows");

    h.click_on("phone-settings-back");
    assert!(
        drawn(&mut h, "phone-settings-row-hosts"),
        "Back returns to the category list"
    );

    h.click_on("phone-settings-row-hosts");
    assert!(
        !drawn(&mut h, "phone-host-name"),
        "the add form did not come back on the Hosts page"
    );
    h.click_on("phone-host-add-another");
    assert!(
        drawn(&mut h, "phone-host-name"),
        "Add a host opens the form"
    );

    h.click_on("phone-host-add");
    assert!(
        drawn(&mut h, "phone-host-error"),
        "the form came back empty: Add is refused"
    );
    assert!(
        !h.opened_targets().iter().any(|target| target
            .tailnet_host()
            .is_some_and(|host| host.name() == TYPED)),
        "a refused form opens nothing"
    );
    let saved = saved_ui(&dir)["saved_hosts"].clone();
    assert_eq!(
        saved.as_array().map(Vec::len),
        Some(1),
        "a refused form saves nothing: {saved}"
    );
}

#[gpui::test]
fn a_notification_tap_that_cannot_open_closes_the_add_form(cx: &mut TestAppContext) {
    const TYPED: &str = "den.tail1234.ts.net";
    const OTHER_HOST: &str = "other.tail1234.ts.net";
    let dir = TestDir::new();
    let platform = Arc::new(FakePushPlatform::default());
    let mut h = phone(cx, &dir, platform.clone());

    open_hosts(&mut h);
    h.click_on("phone-host-add-another");
    h.click_on("phone-host-name");
    type_text(&mut h, TYPED);
    assert!(drawn(&mut h, "phone-host-name"), "the form shows");

    platform.tap(OTHER_HOST, "s1");
    h.cx.run_until_parked();
    assert!(
        !drawn(&mut h, "phone-settings"),
        "the tap's note returns Home"
    );

    open_hosts(&mut h);
    assert!(
        !drawn(&mut h, "phone-host-name"),
        "the add form did not come back on the Hosts page"
    );
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
