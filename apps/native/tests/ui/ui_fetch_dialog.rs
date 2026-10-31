//! Fetch file… specs: the sessions toolbar's ⋯ menu offers no Fetch file…
//! row while the focused pane holds a session of this PC, whose files are
//! already here.

use crate::support;

use gpui::{TestAppContext, px};
use support::{Fixture, Harness, TestDir, session};

const ROW: &str = "sidebar-more-fetch-file";

/// Whether the element tagged `selector` was ever painted in this spec's
/// window: one never drawn has no bounds.
fn drawn(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).size.width > px(0.0)
}

#[gpui::test]
fn the_more_menu_has_no_fetch_file_on_the_local_daemon(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    h.click_on("sidebar-more");
    assert!(
        !drawn(&mut h, ROW),
        "the local daemon's files are on this PC"
    );
}
