//! The phone's Quick replies page under Settings: a section per set, a row
//! per group, and a group's own page listing its replies. A row opens its
//! page and Back (its button or the gesture) steps out of it, then out of
//! the page, then out of Settings. The page is read-only.

use std::sync::Arc;

use crate::support;

use gpui::{BackGesturePhase, Pixels, TestAppContext, point, px};
use rustling_tulip_native::phone::push::FakePushPlatform;
use serde_json::json;
use support::{Harness, REMOTE, TestDir, seed_ui};

/// The least height of a row or a target.
fn touch_target() -> Pixels {
    px(44.0)
}

/// Whether the last frame drew what `selector` tags.
fn drawn(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).size.width > px(0.0)
}

/// A phone with [`REMOTE`] saved and paired, no host open: its hosts screen
/// shows with the `Settings` button in the header.
fn phone<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    seed_ui(
        dir,
        &json!({ "saved_hosts": [{ "name": REMOTE, "port": 8787 }] }),
    );
    Harness::phone_with(cx, dir, Arc::new(FakePushPlatform::default()))
}

/// Opens Settings, then its Quick replies page.
fn open_replies(h: &mut Harness<'_>) {
    h.click_on("phone-settings-open");
    h.click_on("phone-settings-row-quick-replies");
}

/// A back gesture in `phase` from Settings' leading edge.
fn back_gesture(h: &mut Harness<'_>, phase: BackGesturePhase) {
    let page = h.bounds("phone-settings");
    let at = point(page.origin.x + px(4.0), page.origin.y + px(200.0));
    h.back_gesture(phase, at);
}

#[gpui::test]
fn the_quick_replies_row_opens_its_page(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);

    h.click_on("phone-settings-open");
    let row = "phone-settings-row-quick-replies";
    assert!(drawn(&mut h, row), "the category list carries the row");
    assert!(h.bounds(row).size.height >= touch_target());

    h.click_on(row);

    assert!(drawn(&mut h, "phone-settings"), "Settings shows");
    assert!(
        drawn(&mut h, "phone-replies-agent"),
        "the agent set's section"
    );
    assert!(drawn(&mut h, "phone-replies-shell"), "and the shell's");
    assert!(!drawn(&mut h, row), "the page takes the list's place");
}

#[gpui::test]
fn the_page_lists_each_sets_groups_in_order(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);
    open_replies(&mut h);

    let confirm = h.bounds("phone-replies-agent-group-0");
    let common = h.bounds("phone-replies-agent-group-1");
    let shell = h.bounds("phone-replies-shell-group-0");
    assert!(confirm.size.height >= touch_target(), "a group row");
    assert!(drawn(&mut h, "phone-replies-agent-groups"));
    assert!(drawn(&mut h, "phone-replies-shell-groups"));
    assert!(
        confirm.top() < common.top(),
        "the agent set's groups keep their order"
    );
    assert!(
        common.top() < shell.top(),
        "the agent set comes over the shell's"
    );
    assert!(
        !drawn(&mut h, "phone-replies-agent-group-2"),
        "the agent set has two groups"
    );
    assert!(
        !drawn(&mut h, "phone-replies-shell-group-1"),
        "the shell set has one"
    );
}

#[gpui::test]
fn a_group_row_opens_its_replies(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);
    open_replies(&mut h);

    h.click_on("phone-replies-agent-group-1");

    assert!(drawn(&mut h, "phone-replies-group-title"), "its Title row");
    assert!(drawn(&mut h, "phone-replies-replies"), "over its replies");
    for reply in 0..5 {
        let row = format!("phone-replies-reply-{reply}");
        assert!(drawn(&mut h, &row), "{row} shows");
        assert!(h.bounds(&row).size.height >= touch_target());
    }
    assert!(
        !drawn(&mut h, "phone-replies-reply-5"),
        "common holds five replies"
    );
    assert!(
        !drawn(&mut h, "phone-replies-agent-group-1"),
        "the group page takes the set's rows away"
    );
}

#[gpui::test]
fn back_from_a_group_page_returns_to_the_page_then_the_list(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);
    open_replies(&mut h);
    h.click_on("phone-replies-agent-group-0");

    h.click_on("phone-settings-back");
    assert!(
        drawn(&mut h, "phone-replies-agent-group-0"),
        "the group page pops to its set"
    );
    assert!(!drawn(&mut h, "phone-replies-replies"));

    h.click_on("phone-settings-back");
    assert!(
        drawn(&mut h, "phone-settings-row-quick-replies"),
        "the page pops to the list"
    );

    h.click_on("phone-settings-back");
    assert!(!drawn(&mut h, "phone-settings"), "the list closes");
    assert!(
        drawn(&mut h, &format!("phone-host-row-{REMOTE}")),
        "the hosts screen shows again"
    );
}

#[gpui::test]
fn the_back_gesture_steps_out_of_a_group_page(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);
    open_replies(&mut h);
    h.click_on("phone-replies-agent-group-0");

    back_gesture(&mut h, BackGesturePhase::Committed);
    assert!(
        drawn(&mut h, "phone-replies-agent-group-0"),
        "the gesture pops the group page"
    );
    assert!(!drawn(&mut h, "phone-replies-replies"));

    back_gesture(&mut h, BackGesturePhase::Committed);
    assert!(
        drawn(&mut h, "phone-settings-row-quick-replies"),
        "then the page"
    );

    back_gesture(&mut h, BackGesturePhase::Committed);
    assert!(!drawn(&mut h, "phone-settings"), "then Settings closes");
    assert!(
        drawn(&mut h, &format!("phone-host-row-{REMOTE}")),
        "the hosts screen shows again"
    );
}

#[gpui::test]
fn saved_groups_show_on_the_page(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({
            "saved_hosts": [{ "name": REMOTE, "port": 8787 }],
            "phone_agent_replies": [{ "title": "mine", "replies": ["run the tests"] }],
        }),
    );
    let mut h = Harness::phone_with(cx, &dir, Arc::new(FakePushPlatform::default()));

    open_replies(&mut h);

    assert!(
        drawn(&mut h, "phone-replies-agent-group-0"),
        "the saved group shows"
    );
    assert!(
        !drawn(&mut h, "phone-replies-agent-group-1"),
        "and the defaults do not"
    );
    assert!(
        drawn(&mut h, "phone-replies-shell-group-0"),
        "the other set keeps its default group"
    );

    h.click_on("phone-replies-agent-group-0");
    assert!(drawn(&mut h, "phone-replies-reply-0"), "its one reply");
    assert!(!drawn(&mut h, "phone-replies-reply-1"));
}
