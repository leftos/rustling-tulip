//! Paint specs: the stopped-pane overlays sit their message and buttons on a
//! card, and the main area is opaque, so nothing behind the window shows
//! through it.

use crate::support;

use gpui::{Bounds, Pixels, TestAppContext};
use rustling_tulip_native::palette::GROUND;
use support::{Fixture, Harness, TestDir, session};

/// Whether the element tagged `selector` has been painted on screen. gpui
/// keeps the bounds of an element that left the tree, so a `false` only
/// holds for an element the spec never showed.
fn painted(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).origin.x >= gpui::px(0.0)
}

/// Whether `inner`'s area lies inside `outer`'s.
fn inside(outer: &Bounds<Pixels>, inner: &Bounds<Pixels>) -> bool {
    outer.origin.x <= inner.origin.x
        && outer.origin.y <= inner.origin.y
        && inner.origin.x + inner.size.width <= outer.origin.x + outer.size.width
        && inner.origin.y + inner.size.height <= outer.origin.y + outer.size.height
}

/// The fill the main area paints.
fn main_fill(h: &mut Harness<'_>) -> u32 {
    h.root(|root, _| root.main_area_fill())
}

#[gpui::test]
fn exited_overlay_buttons_sit_on_a_card(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture::single(session("s1").exited(3).build());
    let mut h = Harness::with(cx, &dir, &fixture);

    assert!(painted(&mut h, "exited-card-p1"), "the card is painted");
    assert!(h.in_model("exited-card-p1"), "the card is in the model");
    let card = h.bounds("exited-card-p1");
    for selector in [
        "exited-restart-p1",
        "exited-new-session-p1",
        "exited-remove-pane-p1",
    ] {
        assert!(painted(&mut h, selector), "{selector} is painted");
        let button = h.bounds(selector);
        assert!(inside(&card, &button), "{selector} sits on the card");
    }
}

#[gpui::test]
fn abandoned_overlay_buttons_sit_on_a_card(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture::single(
        session("s1")
            .abandoned()
            .last_prompt("fix the build")
            .build(),
    );
    let mut h = Harness::with(cx, &dir, &fixture);

    assert!(painted(&mut h, "abandoned-card-p1"), "the card is painted");
    assert!(h.in_model("abandoned-card-p1"), "the card is in the model");
    let card = h.bounds("abandoned-card-p1");
    for selector in ["abandoned-resume-p1", "abandoned-dismiss-p1"] {
        assert!(painted(&mut h, selector), "{selector} is painted");
        let button = h.bounds(selector);
        assert!(inside(&card, &button), "{selector} sits on the card");
    }
}

#[gpui::test]
fn main_area_is_opaque_with_no_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());

    assert_eq!(
        main_fill(&mut h),
        GROUND,
        "the empty main area paints the ground"
    );
}

#[gpui::test]
fn main_area_is_opaque_with_a_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture::single(session("s1").build());
    let mut h = Harness::with(cx, &dir, &fixture);

    assert!(h.in_model("pane-grid-p1"), "the tab shows its pane");
    assert_eq!(
        main_fill(&mut h),
        GROUND,
        "the gutter around the pane paints the ground"
    );
}
