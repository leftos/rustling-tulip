//! The driver's element registry (feature `driver`): each named element's
//! painted bounds, whether a clipping ancestor hides it, and whether the
//! latest frame still painted it.
#![cfg(feature = "driver")]

use crate::support;

use gpui::{Modifiers, ScrollDelta, ScrollWheelEvent, TestAppContext, TouchPhase, point, px};
use rustling_tulip_native::driver::registry::{self, Entry};
use support::{Fixture, Harness, TestDir, repo, session};

/// Repos `r1` and `r2`, one session of `r1` alone in pane `p1` of tab `t1`.
fn two_repos() -> Fixture {
    let mut fixture = Fixture::single(session("s1").in_repo("r1").build());
    fixture.repos = vec![repo("r1", "D:/src/r1"), repo("r2", "D:/src/r2")];
    fixture
}

/// The registry's entry for `name` in the harness window's latest frame.
fn entry(h: &mut Harness<'_>, name: &str) -> Option<Entry> {
    h.cx.run_until_parked();
    h.cx.update(|window, cx| registry::lookup(cx, window.window_handle().window_id(), name))
}

#[expect(
    clippy::panic,
    reason = "a missing name fails the spec with the names the frame holds"
)]
fn registered(h: &mut Harness<'_>, name: &str) -> Entry {
    entry(h, name).unwrap_or_else(|| {
        let names =
            h.cx.update(|window, cx| registry::names(cx, window.window_handle().window_id()));
        panic!("{name} is not in the registry's latest frame; it holds {names:?}")
    })
}

#[gpui::test]
fn the_registry_gives_a_named_rows_bounds(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());

    for name in ["container-repo:r1", "pane-title-p1"] {
        let found = registered(&mut h, name);
        assert_eq!(found.bounds, h.bounds(name), "{name}'s registry bounds");
        assert_eq!(found.visible, found.bounds, "{name} shows whole");
    }
    // The pane header draws a bottom border, outside the padding box the
    // registry records.
    let header = registered(&mut h, "pane-header-p1");
    let drawn = h.bounds("pane-header-p1");
    assert_eq!(header.bounds.origin, drawn.origin, "the header's origin");
    assert_eq!(
        header.bounds.size.width, drawn.size.width,
        "the header's width"
    );
    assert!(
        header.bounds.size.height < drawn.size.height,
        "the header's padding box stops above its border: {header:?} in {drawn:?}"
    );

    h.right_click_on("container-repo:r1");
    assert!(
        h.root(|root, _| root.container_menu_open()),
        "the menu opened"
    );
    let row = registered(&mut h, "container-menu-spawn");
    assert_eq!(
        row.bounds,
        h.bounds("container-menu-spawn"),
        "the menu row's registry bounds"
    );
}

#[gpui::test]
fn a_scrolled_away_row_reads_as_not_visible(cx: &mut TestAppContext) {
    let mut fixture = Fixture::single(session("s1").in_repo("r00").build());
    fixture.repos = (0..60)
        .map(|n| repo(&format!("r{n:02}"), &format!("D:/src/r{n:02}")))
        .collect();
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture);

    let first = registered(&mut h, "container-repo:r00");
    assert!(
        !first.visible.is_empty(),
        "the list's first row shows: {first:?}"
    );
    let last = registered(&mut h, "container-repo:r59");
    assert!(
        !last.bounds.is_empty(),
        "the last row is laid out: {last:?}"
    );
    assert!(
        last.visible.is_empty(),
        "the last row lies below the list's fold and reads as not visible: {last:?}"
    );
}

#[gpui::test]
fn an_element_gone_from_the_tree_is_stale(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    h.right_click_on("container-repo:r1");
    assert!(
        h.root(|root, _| root.container_menu_open()),
        "the menu opened"
    );
    registered(&mut h, "container-menu-spawn");

    h.keys("escape");
    assert!(
        !h.root(|root, _| root.container_menu_open()),
        "Escape closed the menu"
    );
    assert_eq!(
        entry(&mut h, "container-menu-spawn"),
        None,
        "the closed menu's row is stale"
    );
    assert!(
        entry(&mut h, "container-repo:r1").is_some(),
        "the row the menu opened on is still painted"
    );
}

#[gpui::test]
fn a_menu_does_not_scroll_in_a_driver_build(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    h.right_click_on("container-repo:r1");
    assert!(
        h.root(|root, _| root.container_menu_open()),
        "the menu opened"
    );
    let before = registered(&mut h, "container-menu-spawn").bounds;

    let over = h.bounds("container-menu").center();
    h.cx.simulate_event(ScrollWheelEvent {
        position: over,
        delta: ScrollDelta::Pixels(point(px(0.0), px(-40.0))),
        modifiers: Modifiers::none(),
        touch_phase: TouchPhase::Moved,
    });
    h.cx.run_until_parked();

    assert_eq!(
        registered(&mut h, "container-menu-spawn").bounds,
        before,
        "a menu whose rows fit does not scroll under the wheel"
    );
}
