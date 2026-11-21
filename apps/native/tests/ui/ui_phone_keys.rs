//! The phone's Keys page under Settings: a read-only preview of the Terminal
//! bar, a muted hint, a full-width row per key in the saved order, and
//! Restore defaults with its confirm row, which is dimmed while the built-in
//! keys already show.

use std::sync::Arc;
use std::time::Duration;

use crate::support;

use daemon_client::device_tokens::DeviceTokenStore as _;
use gpui::{
    GestureTuning, Modifiers, Pixels, Point, ScrollDelta, ScrollEvent, TestAppContext, TouchPhase,
    point, px,
};
use protocol::{ChatScreen, ClientMessage, ComposerState, DaemonMessage, SessionSnapshot};
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

/// Window point near the top of what `selector` tags, which is a row's top
/// half: a drop there takes the row's `Before`.
fn before(h: &mut Harness<'_>, selector: &str) -> Point<Pixels> {
    let bounds = h.bounds(selector);
    point(bounds.center().x, bounds.top() + px(6.0))
}

/// A send action of one plain press of each named key in `names`, in order.
fn send_action(names: &[&str]) -> Value {
    let steps: Vec<Value> = names
        .iter()
        .map(|name| {
            json!({
                "ctrl": false,
                "alt": false,
                "shift": false,
                "key": { "kind": "named", "name": name },
            })
        })
        .collect();
    json!({ "kind": "send", "steps": steps })
}

/// Types `text` key by key into the focused field.
fn type_text(h: &mut Harness<'_>, text: &str) {
    let keys: Vec<String> = text
        .chars()
        .map(|c| match c {
            ' ' => "space".to_owned(),
            other => other.to_string(),
        })
        .collect();
    h.keys(&keys.join(" "));
}

/// Takes Settings' two Back steps from the Keys page: to the list, then out.
fn close_settings(h: &mut Harness<'_>) {
    for _ in 0..2 {
        h.click_on("phone-settings-back");
    }
}

/// Scrolls the Keys page until what `selector` tags shows, since a row per
/// key runs the page past the window's foot.
fn scroll_page_to(h: &mut Harness<'_>, selector: &str) {
    h.scroll_into_view("phone-settings", selector);
}

/// Taps Add key, scrolled into view under the keys.
fn tap_add_key(h: &mut Harness<'_>) {
    scroll_page_to(h, "phone-keys-add");
    h.click_on("phone-keys-add");
}

/// Adds a key with one empty press, through Add key and Add press.
fn add_key_with_a_press(h: &mut Harness<'_>) {
    tap_add_key(h);
    h.click_on("phone-key-editor-add-step");
}

/// Sets press `step`'s key to the character `value` through the picker.
fn pick_char(h: &mut Harness<'_>, step: usize, value: &str) {
    h.click_on(&format!("phone-key-editor-step-{step}-key"));
    type_text(h, value);
    h.click_on("phone-key-picker-char-use");
}

/// Sets press `step`'s key to the named key `name` through the picker.
fn pick_named(h: &mut Harness<'_>, step: usize, name: &str) {
    h.click_on(&format!("phone-key-editor-step-{step}-key"));
    h.click_on(&format!("phone-key-picker-{name}"));
}

/// Scrolls the key bar until `selector` shows whole inside it.
fn scroll_bar_to(h: &mut Harness<'_>, selector: &str) {
    let at = h.center("phone-key-bar-scroll");
    for _ in 0..50 {
        let (outer, inner) = (h.bounds("phone-key-bar-scroll"), h.bounds(selector));
        let dx = if inner.right() > outer.right() {
            outer.right() - inner.right()
        } else if inner.left() < outer.left() {
            outer.left() - inner.left()
        } else {
            return;
        };
        h.cx.simulate_event(ScrollEvent {
            position: at,
            delta: ScrollDelta::Pixels(point(dx, px(0.0))),
            modifiers: Modifiers::none(),
            touch_phase: TouchPhase::Moved,
            source: gpui::ScrollSource::Pointer,
        });
        h.cx.run_until_parked();
    }
}

/// Opens the shell session `n`'s Terminal page, everything sent drained.
fn on_terminal(h: &mut Harness<'_>) {
    h.click_on(&row("n"));
    h.host(&saved_remote()).pty("n", b"");
    h.cx.run_until_parked();
    h.host(&saved_remote()).sent();
}

/// Session `a`'s Chat page with its chat loaded, the Keys dock open.
fn dock_open(h: &mut Harness<'_>) {
    h.click_on(&row("a"));
    let sent = h.host(&saved_remote()).sent();
    let request = sent.iter().rev().find_map(|msg| match msg {
        ClientMessage::LoadChat {
            session_id,
            request_id,
        } if session_id == "a" => Some(request_id.clone()),
        _ => None,
    });
    h.host(&saved_remote()).send(DaemonMessage::Chat {
        session_id: "a".to_owned(),
        entries: Vec::new(),
        hidden: 0,
        screen: ChatScreen {
            composer: ComposerState::Ready,
            prompt: None,
        },
        request_id: request.flatten(),
    });
    h.cx.run_until_parked();
    h.click_on("phone-composer-keys");
    h.host(&saved_remote()).sent();
}

#[gpui::test]
fn the_keys_row_opens_the_list(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);

    h.click_on("phone-settings-open");
    let row = "phone-settings-row-keys";
    assert!(drawn(&mut h, row), "the category list carries the row");
    assert!(h.bounds(row).size.height >= touch_target());

    h.click_on(row);

    assert!(drawn(&mut h, "phone-settings"), "Settings shows");
    assert!(drawn(&mut h, "phone-keys-list"), "the list draws");
    assert!(drawn(&mut h, "phone-keys-row-0"), "with its first key");
    assert!(!drawn(&mut h, row), "the page takes the list's place");
}

#[gpui::test]
fn the_list_draws_a_row_per_key_in_order(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);
    open_keys(&mut h);

    let mut previous = h.bounds("phone-keys-row-0");
    assert!(previous.size.width > px(0.0), "the first key draws");
    assert!(previous.size.height >= touch_target());
    for index in 1..=last_default_cell() {
        let row = format!("phone-keys-row-{index}");
        let bounds = h.bounds(&row);
        assert!(bounds.size.width > px(0.0), "{row} draws");
        assert!(
            bounds.top() >= previous.bottom(),
            "{row} sits under the key before it: {bounds:?} after {previous:?}"
        );
        previous = bounds;
    }
    assert!(
        !drawn(&mut h, "phone-keys-row-21"),
        "the built-in keys are twenty-one"
    );
}

#[gpui::test]
fn each_key_row_is_full_width_and_a_touch_target_tall_at_390_points(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);
    h.cx.simulate_resize(gpui::size(px(390.0), px(844.0)));
    open_keys(&mut h);

    let list = h.bounds("phone-keys-list");
    assert_eq!(
        list.size.width,
        h.bounds("phone-keys-preview").size.width,
        "the list spans the page, as the preview does"
    );
    for index in 0..=last_default_cell() {
        let row = format!("phone-keys-row-{index}");
        let bounds = h.bounds(&row);
        assert_eq!(
            bounds.left(),
            list.left(),
            "{row} starts at the list's edge"
        );
        assert_eq!(bounds.size.width, list.size.width, "{row} spans the list");
        assert!(
            bounds.size.height >= touch_target(),
            "{row} is a touch target tall: {bounds:?}"
        );
    }
}

#[gpui::test]
fn restore_asks_then_brings_back_the_default_grid(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let ui = json!({ "phone_keys": [named_key("esc", "Esc", "esc")] });
    let mut h = phone_open(cx, &dir, &ui);
    open_keys(&mut h);

    assert!(
        drawn(&mut h, "phone-keys-row-3"),
        "the saved grid's last key draws"
    );
    assert!(
        !drawn(&mut h, "phone-keys-row-4"),
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
        drawn(&mut h, "phone-keys-row-20"),
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

    scroll_page_to(&mut h, "phone-keys-restore");
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

#[gpui::test]
fn adding_a_ctrl_b_key_appends_it_to_the_bar_and_the_dock(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone_open(cx, &dir, &json!({}));
    open_keys(&mut h);

    tap_add_key(&mut h);
    h.click_on("phone-key-editor-add-step");
    h.click_on("phone-key-editor-step-0-ctrl");
    pick_char(&mut h, 0, "b");
    h.click_on("phone-key-editor-save");

    let saved = saved_ui(&dir);
    assert_eq!(saved["phone_keys"][21]["id"], json!("key-1"));
    assert_eq!(saved["phone_keys"][21]["label"], json!("Ctrl+B"));
    assert!(!drawn(&mut h, "phone-key-editor-label"), "the editor pops");

    close_settings(&mut h);
    assert!(!drawn(&mut h, "phone-settings"), "Settings closes");

    on_terminal(&mut h);
    scroll_bar_to(&mut h, "phone-bar-key-1");
    h.click_on("phone-bar-key-1");
    assert_eq!(
        h.host(&saved_remote()).sent_input("n"),
        [0x02],
        "the bar sends Ctrl+B"
    );

    h.click_on("phone-session-back");
    dock_open(&mut h);
    h.click_on("phone-key-21");
    assert_eq!(
        h.host(&saved_remote()).sent_input("a"),
        [0x02],
        "the dock sends Ctrl+B"
    );
}

#[gpui::test]
fn a_key_with_no_presses_is_refused(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);
    open_keys(&mut h);

    tap_add_key(&mut h);
    h.click_on("phone-key-editor-save");

    assert!(drawn(&mut h, "phone-key-editor-error"), "the refusal shows");
    assert!(
        drawn(&mut h, "phone-key-editor-label"),
        "the editor stays open"
    );
    assert!(
        saved_ui(&dir).get("phone_keys").is_none(),
        "nothing was saved"
    );
}

#[gpui::test]
fn a_press_with_no_key_is_refused(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);
    open_keys(&mut h);

    add_key_with_a_press(&mut h);
    h.click_on("phone-key-editor-save");

    assert!(
        drawn(&mut h, "phone-key-editor-error"),
        "a press with no key is refused"
    );
    assert!(
        saved_ui(&dir).get("phone_keys").is_none(),
        "nothing was saved"
    );
}

#[gpui::test]
fn ctrl_and_a_digit_is_refused_as_typing_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);
    open_keys(&mut h);

    add_key_with_a_press(&mut h);
    h.click_on("phone-key-editor-step-0-ctrl");
    pick_char(&mut h, 0, "1");
    h.click_on("phone-key-editor-save");

    assert!(
        drawn(&mut h, "phone-key-editor-error"),
        "Ctrl+1 types nothing, so the key is refused"
    );
    assert!(
        saved_ui(&dir).get("phone_keys").is_none(),
        "nothing was saved"
    );
}

#[gpui::test]
fn a_blank_label_saves_the_derived_label(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);
    open_keys(&mut h);

    add_key_with_a_press(&mut h);
    h.click_on("phone-key-editor-step-0-ctrl");
    pick_char(&mut h, 0, "b");
    h.click_on("phone-key-editor-save");

    assert_eq!(
        saved_ui(&dir)["phone_keys"][21]["label"],
        json!("Ctrl+B"),
        "the blank label takes the label its presses make"
    );
}

#[gpui::test]
fn a_sequence_sends_each_press_in_order(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone_open(cx, &dir, &json!({}));
    open_keys(&mut h);

    tap_add_key(&mut h);
    h.click_on("phone-key-editor-add-step");
    pick_named(&mut h, 0, "esc");
    h.click_on("phone-key-editor-add-step");
    pick_char(&mut h, 1, ":");
    h.click_on("phone-key-editor-add-step");
    pick_char(&mut h, 2, "w");
    h.click_on("phone-key-editor-add-step");
    pick_named(&mut h, 3, "enter");
    h.click_on("phone-key-editor-save");

    close_settings(&mut h);
    on_terminal(&mut h);
    scroll_bar_to(&mut h, "phone-bar-key-1");
    h.click_on("phone-bar-key-1");

    assert_eq!(
        h.host(&saved_remote()).sent_input("n"),
        b"\x1b:w\r",
        "each press sends in order"
    );
}

#[gpui::test]
fn editing_a_key_keeps_its_place_and_id(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let ui = json!({ "phone_keys": [
        named_key("esc", "Esc", "esc"),
        named_key("tab", "Tab", "tab"),
        named_key("enter", "Enter", "enter"),
    ] });
    let mut h = phone_saved(cx, &dir, &ui);
    open_keys(&mut h);

    h.click_on("phone-keys-row-1");
    assert!(
        drawn(&mut h, "phone-key-editor-label"),
        "Tab opens its editor"
    );
    pick_named(&mut h, 0, "enter");
    h.click_on("phone-key-editor-save");

    let saved = saved_ui(&dir);
    assert_eq!(saved["phone_keys"][1]["id"], json!("tab"), "the id stays");
    assert_eq!(
        saved["phone_keys"][1]["action"]["steps"][0]["key"]["name"],
        json!("enter"),
        "the edit lands in place"
    );
    assert_eq!(
        saved["phone_keys"].as_array().map(Vec::len),
        Some(6),
        "no key was added or dropped"
    );
}

#[gpui::test]
fn removing_a_key_drops_it_from_the_bar(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let ui = json!({ "phone_keys": [
        named_key("esc", "Esc", "esc"),
        named_key("tab", "Tab", "tab"),
        named_key("enter", "Enter", "enter"),
    ] });
    let mut h = phone_open(cx, &dir, &ui);
    open_keys(&mut h);

    h.click_on("phone-keys-row-2");
    h.click_on("phone-key-editor-remove");

    assert!(
        !drawn(&mut h, "phone-key-editor-label"),
        "the editor closes"
    );
    assert_eq!(
        saved_ui(&dir)["phone_keys"].as_array().map(Vec::len),
        Some(5),
        "the key is gone"
    );

    close_settings(&mut h);
    on_terminal(&mut h);

    assert!(!drawn(&mut h, "phone-bar-enter"), "the bar drops it");
    assert!(drawn(&mut h, "phone-bar-tab"), "the keys beside it stay");
}

#[gpui::test]
fn paste_quick_and_ctrl_open_no_editor(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);
    open_keys(&mut h);

    for index in [2, 7, 8] {
        h.click_on(&format!("phone-keys-row-{index}"));
        assert!(
            !drawn(&mut h, "phone-key-editor-label"),
            "cell {index} opens no editor"
        );
    }
}

#[gpui::test]
fn the_picker_sets_a_named_key(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);
    open_keys(&mut h);

    add_key_with_a_press(&mut h);
    pick_named(&mut h, 0, "esc");

    assert!(
        drawn(&mut h, "phone-key-editor-label"),
        "the picker pops back to the editor"
    );
    assert!(
        !drawn(&mut h, "phone-key-picker-char"),
        "the picker is gone"
    );

    h.click_on("phone-key-editor-save");

    let saved = saved_ui(&dir);
    assert_eq!(
        saved["phone_keys"][21]["action"]["steps"][0]["key"]["name"],
        json!("esc")
    );
    assert_eq!(
        saved["phone_keys"][21]["label"],
        json!("Esc"),
        "the blank label takes the named key's label"
    );
}

#[gpui::test]
fn the_picker_refuses_two_characters(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);
    open_keys(&mut h);

    add_key_with_a_press(&mut h);
    h.click_on("phone-key-editor-step-0-key");
    type_text(&mut h, "ab");
    h.click_on("phone-key-picker-char-use");

    assert!(
        drawn(&mut h, "phone-key-picker-error"),
        "two characters are refused"
    );
    assert!(
        drawn(&mut h, "phone-key-picker-char"),
        "the picker stays open"
    );
}

#[gpui::test]
fn back_from_the_picker_keeps_the_draft(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);
    open_keys(&mut h);

    add_key_with_a_press(&mut h);
    h.click_on("phone-key-editor-step-0-ctrl");
    h.click_on("phone-key-editor-step-0-key");
    h.click_on("phone-settings-back");

    assert!(!drawn(&mut h, "phone-key-picker-char"), "the picker closes");
    assert!(drawn(&mut h, "phone-key-editor-label"), "the editor stays");
    assert!(
        drawn(&mut h, "phone-key-editor-step-0-ctrl"),
        "the press the picker was over stays"
    );

    pick_char(&mut h, 0, "b");
    h.click_on("phone-key-editor-save");

    assert_eq!(
        saved_ui(&dir)["phone_keys"][21]["action"]["steps"][0]["ctrl"],
        json!(true),
        "the Ctrl set before the picker stayed"
    );
}

#[gpui::test]
fn back_from_the_editor_discards_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);
    open_keys(&mut h);

    tap_add_key(&mut h);
    type_text(&mut h, "tmux");
    h.click_on("phone-settings-back");

    assert!(
        !drawn(&mut h, "phone-key-editor-label"),
        "the editor closes"
    );
    assert!(drawn(&mut h, "phone-keys-list"), "the grid shows again");
    assert!(
        saved_ui(&dir).get("phone_keys").is_none(),
        "the draft was discarded"
    );
}

#[gpui::test]
fn a_long_press_drag_moves_a_key_and_the_bar_follows(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone_open(cx, &dir, &json!({}));
    open_keys(&mut h);

    let from = h.center("phone-keys-row-3");
    let to = before(&mut h, "phone-keys-row-0");
    h.long_press_drag(from, to);

    let saved = saved_ui(&dir);
    assert_eq!(
        saved["phone_keys"][0]["id"],
        json!("up"),
        "the lifted key lands at the grid's start"
    );
    assert_eq!(saved["phone_keys"][1]["id"], json!("esc"), "Esc follows it");

    close_settings(&mut h);
    on_terminal(&mut h);

    assert!(
        h.bounds("phone-bar-up").left() < h.bounds("phone-bar-esc").left(),
        "the bar draws the moved key first, keeping its id"
    );
}

#[gpui::test]
fn a_long_press_drag_moves_paste(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);
    open_keys(&mut h);

    let from = h.center("phone-keys-row-7");
    let to = before(&mut h, "phone-keys-row-0");
    h.long_press_drag(from, to);

    let saved = saved_ui(&dir);
    assert_eq!(
        saved["phone_keys"][0]["id"],
        json!("paste"),
        "a protected key moves, though it cannot be removed"
    );
    assert_eq!(saved["phone_keys"][1]["id"], json!("esc"));
    assert_eq!(
        saved["phone_keys"].as_array().map(Vec::len),
        Some(last_default_cell() + 1),
        "no key was dropped"
    );
}

#[gpui::test]
fn a_long_press_drag_reorders_a_keys_presses(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let ui = json!({ "phone_keys": [
        grid_key("seq", "vim save", &send_action(&["esc", "tab", "enter"])),
    ] });
    let mut h = phone_saved(cx, &dir, &ui);
    open_keys(&mut h);
    h.click_on("phone-keys-row-0");

    let from = h.center("phone-key-editor-step-0");
    let to = point(
        from.x,
        h.bounds("phone-key-editor-steps").bottom() + px(4.0),
    );
    h.long_press_drag(from, to);
    h.click_on("phone-key-editor-save");

    let saved = saved_ui(&dir);
    let steps = saved["phone_keys"][0]["action"]["steps"]
        .as_array()
        .expect("the saved presses");
    let names: Vec<&str> = steps
        .iter()
        .filter_map(|step| step["key"]["name"].as_str())
        .collect();
    assert_eq!(names, ["tab", "enter", "esc"], "the first press moved last");
}

#[gpui::test]
fn a_press_handle_drag_reorders_the_presses(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let ui = json!({ "phone_keys": [
        grid_key("seq", "vim save", &send_action(&["esc", "tab", "enter"])),
    ] });
    let mut h = phone_saved(cx, &dir, &ui);
    open_keys(&mut h);
    h.click_on("phone-keys-row-0");
    assert!(
        h.bounds("phone-key-editor-step-0-handle").size.height >= touch_target(),
        "each press has a handle a touch target tall"
    );

    let from = h.center("phone-key-editor-step-0-handle");
    let to = point(
        h.center("phone-key-editor-step-0").x,
        h.bounds("phone-key-editor-steps").bottom() + px(4.0),
    );
    h.touch(TouchPhase::Started, from);
    h.touch(TouchPhase::Moved, to);
    assert!(
        drawn(&mut h, "phone-drag-lifted"),
        "the press lifts with no wait"
    );
    assert!(drawn(&mut h, "phone-drag-slot"), "and its slot line draws");
    h.touch(TouchPhase::Ended, to);
    assert!(
        !drawn(&mut h, "phone-drag-lifted"),
        "the drop ends the lift"
    );
    assert_eq!(
        saved_ui(&dir)["phone_keys"][0]["action"]["steps"][0]["key"]["name"],
        json!("esc"),
        "the move stays in the draft until Save"
    );
    h.click_on("phone-key-editor-save");

    let saved = saved_ui(&dir);
    let steps = saved["phone_keys"][0]["action"]["steps"]
        .as_array()
        .expect("the saved presses");
    let names: Vec<&str> = steps
        .iter()
        .filter_map(|step| step["key"]["name"].as_str())
        .collect();
    assert_eq!(names, ["tab", "enter", "esc"], "the first press moved last");
}

#[gpui::test]
fn each_press_row_has_its_modifiers_on_a_second_line(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);
    h.cx.simulate_resize(gpui::size(px(390.0), px(844.0)));
    open_keys(&mut h);
    add_key_with_a_press(&mut h);
    h.click_on("phone-key-editor-step-0-ctrl");
    pick_char(&mut h, 0, "b");

    let key = h.bounds("phone-key-editor-step-0-key");
    let handle = h.bounds("phone-key-editor-step-0-handle");
    assert!(
        key.size.height >= touch_target(),
        "the key button is a target"
    );
    assert!(
        handle.left() >= key.right(),
        "the handle sits at the key button's right"
    );
    assert!(
        handle.top() < key.bottom() && key.top() < handle.bottom(),
        "on the key button's line"
    );
    for part in ["ctrl", "alt", "shift", "remove"] {
        let drawn_part = h.bounds(&format!("phone-key-editor-step-0-{part}"));
        assert!(drawn_part.size.width > px(0.0), "{part} draws");
        assert!(
            drawn_part.top() >= key.bottom(),
            "{part} sits on the line under the key button"
        );
        assert!(
            drawn_part.size.height >= touch_target(),
            "{part} is a target"
        );
    }
    assert!(
        h.bounds("phone-key-editor-step-0-remove").left()
            > h.bounds("phone-key-editor-step-0-shift").right(),
        "Remove sits at the line's right"
    );
    assert!(
        h.bounds("phone-key-editor-steps").right()
            >= h.bounds("phone-key-editor-step-0-remove").right(),
        "inside the press list"
    );
}

#[gpui::test]
fn the_key_editor_previews_the_key_as_its_label_changes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);
    open_keys(&mut h);
    tap_add_key(&mut h);

    let blank = h.bounds("phone-key-editor-preview");
    assert!(blank.size.width > px(0.0), "the preview draws on a new key");
    assert!(
        blank.bottom() <= h.bounds("phone-key-editor-label").top(),
        "over the label field"
    );

    type_text(&mut h, "tmux new");

    let typed = h.bounds("phone-key-editor-preview");
    assert!(
        typed.size.width > blank.size.width,
        "the preview key widens to hold the typed label"
    );
    h.click_on("phone-key-editor-preview");
    assert!(
        !drawn(&mut h, "phone-key-picker-char"),
        "the preview takes no tap"
    );

    h.click_on("phone-key-editor-add-step");
    pick_named(&mut h, 0, "esc");
    h.click_on("phone-key-editor-save");

    assert_eq!(
        saved_ui(&dir)["phone_keys"][21]["label"],
        json!("tmux new"),
        "the label the preview drew is the one saved"
    );
}

#[gpui::test]
fn a_long_press_on_add_key_still_opens_the_editor(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);
    open_keys(&mut h);

    scroll_page_to(&mut h, "phone-keys-add");
    h.long_press_on("phone-keys-add");

    assert!(
        drawn(&mut h, "phone-key-editor-label"),
        "a hold off the grid leaves the press to the button, so its tap opens the editor"
    );
}

#[gpui::test]
fn a_hold_below_the_last_row_lifts_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let ui = json!({ "phone_keys": [
        named_key("esc", "Esc", "esc"),
        named_key("tab", "Tab", "tab"),
        named_key("enter", "Enter", "enter"),
    ] });
    let mut h = phone_saved(cx, &dir, &ui);
    open_keys(&mut h);

    let list = h.bounds("phone-keys-list");
    let from = point(list.center().x, list.bottom() + px(4.0));
    let to = before(&mut h, "phone-keys-row-0");
    h.long_press_drag(from, to);

    let saved = saved_ui(&dir);
    assert_eq!(
        saved["phone_keys"][0]["id"],
        json!("esc"),
        "the room under the last row lifts nothing"
    );
    assert_eq!(
        saved["phone_keys"].as_array().map(Vec::len),
        Some(3),
        "so nothing was saved"
    );
}

#[gpui::test]
fn a_long_list_draws_every_row_at_the_list_width(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let keys: Vec<Value> = (0..23)
        .map(|n| named_key(&format!("k{n}"), "K", "esc"))
        .collect();
    let ui = json!({ "phone_keys": keys });
    let mut h = phone_saved(cx, &dir, &ui);
    h.cx.simulate_resize(gpui::size(px(390.0), px(844.0)));
    open_keys(&mut h);

    let first = h.bounds("phone-keys-row-0");
    let (before_last, last) = (h.bounds("phone-keys-row-24"), h.bounds("phone-keys-row-25"));
    assert!(
        last.top() >= before_last.bottom(),
        "the appended protected keys close the list, one under another"
    );
    assert_eq!(
        last.left(),
        first.left(),
        "the last row starts where the first does"
    );
    assert_eq!(last.size.width, first.size.width, "and is the same width");
    assert_eq!(last.size.height, first.size.height, "and the same height");
    assert!(
        !drawn(&mut h, "phone-keys-row-26"),
        "twenty-three saved keys and the three protected ones"
    );
}

#[gpui::test]
fn a_long_key_label_keeps_its_row_one_height(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let ui = json!({ "phone_keys": [
        named_key("esc", "Esc", "esc"),
        grid_key("long", "Esc : w Enter : q", &send_action(&["esc"])),
        named_key("tab", "Tab", "tab"),
        named_key("enter", "Enter", "enter"),
        named_key("up", "Up", "up"),
    ] });
    let mut h = phone_saved(cx, &dir, &ui);
    h.cx.simulate_resize(gpui::size(px(390.0), px(844.0)));
    open_keys(&mut h);

    let long = h.bounds("phone-keys-row-1");
    let above = h.bounds("phone-keys-row-0");
    let below = h.bounds("phone-keys-row-4");
    assert!(
        drawn(&mut h, "phone-keys-row-1"),
        "the long label's row draws"
    );
    assert_eq!(
        long.size.height, above.size.height,
        "a long label keeps its row one height"
    );
    assert_eq!(
        long.size.height, below.size.height,
        "and the rows stay one height"
    );
    assert!(long.size.height >= touch_target());
}

#[gpui::test]
fn a_cancelled_key_drag_saves_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);
    open_keys(&mut h);

    let from = h.center("phone-keys-row-4");
    let to = before(&mut h, "phone-keys-row-0");
    h.touch(TouchPhase::Started, from);
    h.advance(GestureTuning::default().long_press_duration + Duration::from_millis(1));
    h.touch(TouchPhase::Moved, to);
    h.touch(TouchPhase::Cancelled, to);

    assert!(
        saved_ui(&dir).get("phone_keys").is_none(),
        "a cancelled touch drops the drag without saving"
    );
    assert!(
        drawn(&mut h, "phone-keys-row-20"),
        "the list keeps its keys and their order"
    );
}

#[gpui::test]
fn the_keys_page_previews_the_bar_in_saved_order(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);
    open_keys(&mut h);

    let preview = h.bounds("phone-keys-preview");
    assert!(preview.size.width > px(0.0), "the preview draws");
    assert!(
        preview.bottom() <= h.bounds("phone-keys-list").top(),
        "at the top of the page, over the list"
    );
    let mut previous = h.bounds("phone-keys-preview-esc");
    for id in ["tab", "ctrl", "up", "down"] {
        let key = h.bounds(&format!("phone-keys-preview-{id}"));
        assert_eq!(key.top(), previous.top(), "{id} sits in the one row");
        assert!(
            key.left() > previous.left(),
            "{id} follows the key before it, as the bar draws them"
        );
        previous = key;
    }

    h.click_on("phone-keys-preview-esc");
    assert!(
        !drawn(&mut h, "phone-key-editor-label"),
        "a preview key takes no tap"
    );
    assert!(saved_ui(&dir).get("phone_keys").is_none());

    let from = h.center("phone-keys-row-3");
    let to = before(&mut h, "phone-keys-row-0");
    h.long_press_drag(from, to);

    assert_eq!(saved_ui(&dir)["phone_keys"][0]["id"], json!("up"));
    assert!(
        h.bounds("phone-keys-preview-up").left() < h.bounds("phone-keys-preview-esc").left(),
        "the preview follows the saved order"
    );
}

#[gpui::test]
fn a_lifted_key_row_is_dimmed_and_its_slot_drawn(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);
    open_keys(&mut h);
    assert!(!drawn(&mut h, "phone-drag-lifted"), "nothing is lifted yet");
    assert!(!drawn(&mut h, "phone-drag-slot"), "and no slot shows");

    let from = h.center("phone-keys-row-3");
    let to = before(&mut h, "phone-keys-row-0");
    h.touch(TouchPhase::Started, from);
    h.advance(GestureTuning::default().long_press_duration + Duration::from_millis(1));
    h.touch(TouchPhase::Moved, to);

    let (lifted, row) = (h.bounds("phone-drag-lifted"), h.bounds("phone-keys-row-3"));
    assert!(lifted.size.width > px(0.0), "the lifted row is marked");
    assert_eq!(
        (lifted.left(), lifted.top()),
        (row.left(), row.top()),
        "over the row the finger lifted"
    );
    let (slot, first) = (h.bounds("phone-drag-slot"), h.bounds("phone-keys-row-0"));
    assert!(slot.size.height > px(0.0), "the slot line draws");
    assert!(
        slot.bottom() <= first.top(),
        "over the first row, where the key would drop"
    );
    assert_eq!(
        slot.size.width,
        h.bounds("phone-keys-list").size.width,
        "across the list"
    );

    h.touch(TouchPhase::Ended, to);

    assert!(
        !drawn(&mut h, "phone-drag-lifted"),
        "the drop ends the lift"
    );
    assert!(!drawn(&mut h, "phone-drag-slot"), "and the slot line");
}

#[gpui::test]
fn a_protected_key_row_has_no_chevron(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);
    open_keys(&mut h);

    assert!(
        drawn(&mut h, "phone-keys-row-0-chevron"),
        "Esc's row ends in a chevron"
    );
    for index in [2, 7, 8] {
        assert!(drawn(&mut h, &format!("phone-keys-row-{index}")));
        assert!(
            !drawn(&mut h, &format!("phone-keys-row-{index}-chevron")),
            "row {index} (Ctrl, Paste or Quick) opens no editor, so draws no chevron"
        );
    }
}

#[gpui::test]
fn a_sideways_swipe_on_the_preview_leaves_the_page_where_it_was(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);
    open_keys(&mut h);

    let list_top = h.bounds("phone-keys-list").top();
    let esc_left = h.bounds("phone-keys-preview-esc").left();
    let at = h.center("phone-keys-preview");
    let pan = |dx: Pixels, touch_phase: TouchPhase| ScrollEvent {
        position: at,
        delta: ScrollDelta::Pixels(point(dx, px(0.0))),
        modifiers: Modifiers::none(),
        touch_phase,
        source: gpui::ScrollSource::Touch,
    };
    let gap = Duration::from_millis(10);
    h.cx.simulate_event(pan(px(0.0), TouchPhase::Started));
    h.advance(gap);
    h.cx.simulate_event(pan(px(-60.0), TouchPhase::Moved));
    h.advance(gap);
    h.cx.simulate_event(pan(px(-60.0), TouchPhase::Moved));
    h.cx.simulate_event(pan(px(0.0), TouchPhase::Ended));
    h.cx.run_until_parked();

    assert_eq!(
        h.bounds("phone-keys-list").top(),
        list_top,
        "the Settings page keeps its vertical place"
    );
    assert!(
        h.bounds("phone-keys-preview-esc").left() < esc_left,
        "while the preview scrolls sideways"
    );
}

/// The keys `esc`, `tab` and `enter`, saved; Ctrl, Paste and Quick follow
/// them once loaded.
fn three_keys() -> Value {
    json!({ "phone_keys": [
        named_key("esc", "Esc", "esc"),
        named_key("tab", "Tab", "tab"),
        named_key("enter", "Enter", "enter"),
    ] })
}

/// The ids of the saved keys, in order.
fn saved_ids(dir: &TestDir) -> Vec<String> {
    saved_ui(dir)["phone_keys"]
        .as_array()
        .map(|keys| {
            keys.iter()
                .filter_map(|key| key["id"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

#[gpui::test]
fn edit_shows_minus_on_removable_keys_and_none_on_protected_ones(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);
    open_keys(&mut h);
    assert!(
        !drawn(&mut h, "phone-keys-row-0-minus"),
        "no minus before Edit"
    );
    assert!(!drawn(&mut h, "phone-keys-row-0-handle"), "nor a handle");

    h.click_on("phone-keys-edit");

    assert!(
        drawn(&mut h, "phone-keys-row-0-minus"),
        "Esc shows its minus"
    );
    assert!(drawn(&mut h, "phone-keys-row-0-handle"), "and its handle");
    assert!(
        !drawn(&mut h, "phone-keys-row-0-chevron"),
        "the handle takes the chevron's place"
    );
    assert!(
        h.bounds("phone-keys-row-0-minus").size.height >= touch_target(),
        "the minus is a touch target"
    );
    assert!(
        h.bounds("phone-keys-row-0-handle").size.height >= touch_target(),
        "and so is the handle"
    );
    for index in [2, 7, 8] {
        assert!(
            !drawn(&mut h, &format!("phone-keys-row-{index}-minus")),
            "row {index} (Ctrl, Paste or Quick) cannot be removed, so shows no minus"
        );
        assert!(
            drawn(&mut h, &format!("phone-keys-row-{index}-handle")),
            "row {index} still moves, so shows its handle"
        );
    }
    assert!(
        saved_ui(&dir).get("phone_keys").is_none(),
        "Edit saves nothing"
    );
}

#[gpui::test]
fn minus_then_delete_removes_the_key_and_saves(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone_saved(cx, &dir, &three_keys());
    open_keys(&mut h);
    h.click_on("phone-keys-edit");

    h.click_on("phone-keys-row-1-minus");

    assert!(
        drawn(&mut h, "phone-keys-row-1-delete"),
        "the armed row shows Delete"
    );
    assert!(
        !drawn(&mut h, "phone-keys-row-1-handle"),
        "in its handle's place"
    );
    assert!(
        !drawn(&mut h, "phone-keys-row-0-delete"),
        "only the armed row shows Delete"
    );

    h.click_on("phone-keys-row-1-delete");

    let ids = saved_ids(&dir);
    assert_eq!(ids.len(), 5, "one key fewer: {ids:?}");
    assert!(!ids.iter().any(|id| id == "tab"), "Tab is gone: {ids:?}");
    assert_eq!(ids[0], "esc", "the keys beside it stay");
    assert_eq!(ids[1], "enter");
    assert!(
        !drawn(&mut h, "phone-keys-row-1-delete"),
        "no row is armed after the delete"
    );
    assert!(
        drawn(&mut h, "phone-keys-row-0-minus"),
        "edit mode stays on"
    );
}

#[gpui::test]
fn a_second_minus_tap_disarms_the_row(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone_saved(cx, &dir, &three_keys());
    open_keys(&mut h);
    h.click_on("phone-keys-edit");

    h.click_on("phone-keys-row-0-minus");
    assert!(
        drawn(&mut h, "phone-keys-row-0-delete"),
        "the first tap arms"
    );
    h.click_on("phone-keys-row-0-minus");
    assert!(
        !drawn(&mut h, "phone-keys-row-0-delete"),
        "the second tap disarms"
    );
    assert!(
        drawn(&mut h, "phone-keys-row-0-handle"),
        "and the handle comes back"
    );

    h.click_on("phone-keys-row-0-minus");
    h.click_on("phone-keys-row-1-minus");
    assert!(
        !drawn(&mut h, "phone-keys-row-0-delete"),
        "arming another row disarms the first"
    );
    assert!(
        drawn(&mut h, "phone-keys-row-1-delete"),
        "one row at a time"
    );

    h.click_on("phone-keys-row-0");
    assert!(
        !drawn(&mut h, "phone-keys-row-1-delete"),
        "a tap on another row disarms"
    );
    assert_eq!(saved_ids(&dir).len(), 3, "nothing was removed");
}

#[gpui::test]
fn a_handle_drag_reorders_without_a_long_press(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);
    open_keys(&mut h);
    h.click_on("phone-keys-edit");

    let from = h.center("phone-keys-row-3-handle");
    let to = before(&mut h, "phone-keys-row-0");
    h.touch(TouchPhase::Started, from);
    h.touch(TouchPhase::Moved, to);
    assert!(
        drawn(&mut h, "phone-drag-lifted"),
        "the row lifts with no wait"
    );
    h.touch(TouchPhase::Ended, to);

    let ids = saved_ids(&dir);
    assert_eq!(
        ids.first().map(String::as_str),
        Some("up"),
        "the lifted key lands first: {ids:?}"
    );
    assert_eq!(
        ids.get(1).map(String::as_str),
        Some("esc"),
        "Esc follows it"
    );
    assert!(
        !drawn(&mut h, "phone-drag-lifted"),
        "the drop ends the lift"
    );
}

#[gpui::test]
fn done_and_leaving_the_page_turn_edit_mode_off(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone_saved(cx, &dir, &three_keys());
    open_keys(&mut h);
    h.click_on("phone-keys-edit");
    h.click_on("phone-keys-row-0-minus");

    h.click_on("phone-keys-edit");

    assert!(
        !drawn(&mut h, "phone-keys-row-0-minus"),
        "Done hides the minus"
    );
    assert!(!drawn(&mut h, "phone-keys-row-0-delete"), "and Delete");
    assert!(
        drawn(&mut h, "phone-keys-row-0-chevron"),
        "the chevron comes back"
    );

    h.click_on("phone-keys-edit");
    assert!(
        !drawn(&mut h, "phone-keys-row-0-delete"),
        "Edit again arms no row"
    );
    h.click_on("phone-settings-back");
    h.click_on("phone-settings-row-keys");
    assert!(
        !drawn(&mut h, "phone-keys-row-0-minus"),
        "leaving the page turned edit mode off"
    );

    h.click_on("phone-keys-edit");
    close_settings(&mut h);
    open_keys(&mut h);
    assert!(
        !drawn(&mut h, "phone-keys-row-0-minus"),
        "closing Settings turned it off too"
    );
    assert_eq!(saved_ids(&dir).len(), 3, "edit mode saved nothing");
}

#[gpui::test]
fn a_row_tap_in_edit_mode_opens_no_editor(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);
    open_keys(&mut h);
    h.click_on("phone-keys-edit");

    h.click_on("phone-keys-row-0");

    assert!(
        !drawn(&mut h, "phone-key-editor-label"),
        "a row tap opens no editor in edit mode"
    );
    assert!(drawn(&mut h, "phone-keys-row-0-minus"), "the page stays");
}

#[gpui::test]
fn a_key_whose_summary_matches_its_label_draws_no_summary(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let copy = grid_key(
        "copy",
        "Copy",
        &json!({
            "kind": "send",
            "steps": [{ "ctrl": true, "alt": false, "shift": false, "key": { "kind": "char", "value": "c" } }],
        }),
    );
    let ui = json!({ "phone_keys": [named_key("esc", "Esc", "esc"), copy] });
    let mut h = phone_saved(cx, &dir, &ui);
    open_keys(&mut h);

    assert!(drawn(&mut h, "phone-keys-row-0"), "Esc's row draws");
    assert!(
        !drawn(&mut h, "phone-keys-row-0-summary"),
        "Esc sends Esc, so it does not say so twice"
    );
    assert!(
        drawn(&mut h, "phone-keys-row-1-summary"),
        "Copy draws what it sends, Ctrl+C"
    );
}
