//! The phone's Keys page under Settings: a muted hint, the key grid in fixed
//! equal columns (four at 390 pt), and Restore defaults with its confirm row,
//! which is dimmed while the built-in grid already shows.

use std::sync::Arc;

use crate::support;

use daemon_client::device_tokens::DeviceTokenStore as _;
use gpui::{Pixels, TestAppContext, px};
use protocol::{DaemonMessage, SessionSnapshot};
use rustling_tulip_native::phone::push::FakePushPlatform;
use rustling_tulip_native::target::HostKey;
use serde_json::{Value, json};
use support::{Harness, REMOTE, TestDir, repo, saved_remote, saved_ui, seed_ui, session};

/// The least height of a row or a target.
fn touch_target() -> Pixels {
    px(44.0)
}

/// Whether the last frame drew what `selector` tags.
fn drawn(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).size.width > px(0.0)
}

/// A phone with [`REMOTE`] saved and paired and `ui`'s fields beside it, no
/// host open: its hosts screen shows with the `Settings` button in the
/// header.
fn phone_saved<'a>(cx: &'a mut TestAppContext, dir: &TestDir, ui: &Value) -> Harness<'a> {
    let mut file = ui.as_object().cloned().unwrap_or_default();
    file.insert(
        "saved_hosts".to_owned(),
        json!([{ "name": REMOTE, "port": 8787 }]),
    );
    seed_ui(dir, &Value::Object(file));
    Harness::phone_with(cx, dir, Arc::new(FakePushPlatform::default()))
}

/// A phone with [`REMOTE`] saved and paired, no host open.
fn phone<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    phone_saved(cx, dir, &json!({}))
}

/// Opens Settings, then its Keys page.
fn open_keys(h: &mut Harness<'_>) {
    h.click_on("phone-settings-open");
    h.click_on("phone-settings-row-keys");
}

/// A key as `native-ui.json` stores it.
fn grid_key(id: &str, label: &str, action: &Value) -> Value {
    json!({ "id": id, "label": label, "action": action })
}

/// A send action of one press of the named key `name`.
fn named_key(id: &str, label: &str, name: &str) -> Value {
    grid_key(
        id,
        label,
        &json!({
            "kind": "send",
            "steps": [{ "ctrl": false, "alt": false, "shift": false, "key": { "kind": "named", "name": name } }],
        }),
    )
}

/// `a` working with a mirrored chat and `n` working with no agent, both in r1.
fn listed() -> Vec<SessionSnapshot> {
    vec![
        session("a")
            .label("Alpha")
            .in_repo("r1")
            .status("working")
            .build(),
        session("n")
            .label("November")
            .in_repo("r1")
            .status("working")
            .no_agent()
            .build(),
    ]
}

/// The selector of the saved host's Home row for session `id`.
fn row(id: &str) -> String {
    let wire = HostKey::of(&saved_remote())
        .wire()
        .expect("a tailnet host has a wire key")
        .to_owned();
    format!("phone-home-row-{wire}::{id}")
}

/// A phone with [`REMOTE`] open and [`listed`] listed, `ui`'s fields saved
/// beside the host, everything sent so far drained.
fn phone_open<'a>(cx: &'a mut TestAppContext, dir: &TestDir, ui: &Value) -> Harness<'a> {
    let mut file = ui.as_object().cloned().unwrap_or_default();
    file.insert(
        "saved_hosts".to_owned(),
        json!([{ "name": REMOTE, "port": 8787 }]),
    );
    seed_ui(dir, &Value::Object(file));
    let mut h = Harness::phone(cx, dir);
    h.tokens().set(REMOTE, "t1").expect("seed a token");
    let mut host = h.host(&saved_remote());
    host.connect();
    host.send(DaemonMessage::Repos {
        repos: vec![repo("r1", "C:/r1")],
    });
    host.send(DaemonMessage::Sessions { sessions: listed() });
    h.host(&saved_remote()).sent();
    h
}

/// The index of the last key in [`key_grid::defaults`].
fn last_default_cell() -> usize {
    20
}

#[gpui::test]
fn the_keys_row_opens_the_grid(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);

    h.click_on("phone-settings-open");
    let row = "phone-settings-row-keys";
    assert!(drawn(&mut h, row), "the category list carries the row");
    assert!(h.bounds(row).size.height >= touch_target());

    h.click_on(row);

    assert!(drawn(&mut h, "phone-settings"), "Settings shows");
    assert!(drawn(&mut h, "phone-keys-grid"), "the grid draws");
    assert!(drawn(&mut h, "phone-keys-cell-0"), "with its first key");
    assert!(!drawn(&mut h, row), "the page takes the list's place");
}

#[gpui::test]
fn the_grid_lists_the_keys_in_order(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);
    open_keys(&mut h);

    let mut previous = h.bounds("phone-keys-cell-0");
    assert!(previous.size.width > px(0.0), "the first key draws");
    assert!(previous.size.height >= touch_target());
    for index in 1..=last_default_cell() {
        let cell = format!("phone-keys-cell-{index}");
        let bounds = h.bounds(&cell);
        assert!(bounds.size.width > px(0.0), "{cell} draws");
        assert!(
            bounds.top() > previous.top()
                || (bounds.top() == previous.top() && bounds.left() > previous.left()),
            "{cell} follows the key before it: {bounds:?} after {previous:?}"
        );
        previous = bounds;
    }
    assert!(
        !drawn(&mut h, "phone-keys-cell-21"),
        "the built-in grid holds twenty-one keys"
    );
}

#[gpui::test]
fn the_grid_draws_four_columns_at_390_points(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);
    h.cx.simulate_resize(gpui::size(px(390.0), px(844.0)));
    open_keys(&mut h);

    let first = h.bounds("phone-keys-cell-0");
    let mut left = first.left();
    for index in 1..4 {
        let cell = format!("phone-keys-cell-{index}");
        let bounds = h.bounds(&cell);
        assert_eq!(
            bounds.top(),
            first.top(),
            "{cell} shares the first row's top"
        );
        assert!(bounds.left() > left, "{cell} follows the one before it");
        left = bounds.left();
    }
    let fifth = h.bounds("phone-keys-cell-4");
    assert!(
        fifth.top() > first.top(),
        "the fifth key starts a second row"
    );
}

#[gpui::test]
fn restore_asks_then_brings_back_the_default_grid(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let ui = json!({ "phone_keys": [named_key("esc", "Esc", "esc")] });
    let mut h = phone_open(cx, &dir, &ui);
    open_keys(&mut h);

    assert!(
        drawn(&mut h, "phone-keys-cell-3"),
        "the saved grid's last key draws"
    );
    assert!(
        !drawn(&mut h, "phone-keys-cell-4"),
        "the built-in grid's keys do not"
    );

    h.click_on("phone-keys-restore");
    assert!(
        drawn(&mut h, "phone-keys-restore-confirm"),
        "Restore asks first"
    );

    h.click_on("phone-keys-restore-confirm");

    assert!(
        saved_ui(&dir).get("phone_keys").is_none(),
        "the field goes: the built-in grid shows again"
    );
    assert!(
        drawn(&mut h, "phone-keys-cell-20"),
        "all twenty-one default keys are back"
    );

    h.click_on("phone-settings-back");
    h.click_on("phone-settings-back");
    assert!(!drawn(&mut h, "phone-settings"), "Settings closes");

    h.click_on(&row("n"));
    h.host(&saved_remote()).pty("n", b"");
    h.cx.run_until_parked();

    assert!(
        drawn(&mut h, "phone-bar-esc"),
        "the bar draws the default grid's first key"
    );
    assert!(drawn(&mut h, "phone-bar-tab"), "and the key after it");
}

#[gpui::test]
fn restore_is_dimmed_while_the_defaults_show(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);
    open_keys(&mut h);

    h.click_on("phone-keys-restore");

    assert!(
        !drawn(&mut h, "phone-keys-restore-confirm"),
        "a grid already showing the defaults asks nothing"
    );
    assert!(saved_ui(&dir).get("phone_keys").is_none());
}

#[gpui::test]
fn cancel_keeps_the_saved_grid(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let ui = json!({ "phone_keys": [named_key("esc", "Esc", "esc")] });
    let mut h = phone_saved(cx, &dir, &ui);
    open_keys(&mut h);

    h.click_on("phone-keys-restore");
    h.click_on("phone-keys-restore-cancel");

    assert_eq!(saved_ui(&dir)["phone_keys"][0]["id"], json!("esc"));
    assert!(!drawn(&mut h, "phone-keys-restore-confirm"));
}

#[gpui::test]
fn restore_confirm_is_dropped_on_leaving_the_keys_page(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let ui = json!({ "phone_keys": [named_key("esc", "Esc", "esc")] });
    let mut h = phone_saved(cx, &dir, &ui);
    open_keys(&mut h);

    h.click_on("phone-keys-restore");
    assert!(
        drawn(&mut h, "phone-keys-restore-confirm"),
        "Restore asks first"
    );

    h.click_on("phone-settings-back");
    assert!(
        drawn(&mut h, "phone-settings-row-keys"),
        "the page pops to the list"
    );

    h.click_on("phone-settings-row-keys");

    assert!(
        !drawn(&mut h, "phone-keys-restore-confirm"),
        "the question does not come back with the page"
    );
    assert_eq!(
        saved_ui(&dir)["phone_keys"][0]["id"],
        json!("esc"),
        "nothing was restored"
    );
}
