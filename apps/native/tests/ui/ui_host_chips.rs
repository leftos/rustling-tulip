//! Host chip specs: a remote host's short name on the header of a pane
//! showing that host's session, on the pop-out header that shares those
//! parts, and on the pill of a tab whose panes are all on that one host.

use crate::support;

use gpui::{TestAppContext, VisualTestContext, px};
use rustling_tulip_native::{HeaderChip, HeaderChipKind, OpenOutcome, PopoutKey};
use support::{Harness, REMOTE, TestDir, mixed_tabs};

/// The host chip [`REMOTE`]'s panes carry.
fn box_chip() -> HeaderChip {
    HeaderChip {
        kind: HeaderChipKind::Host,
        text: "box".to_owned(),
        tip: REMOTE.to_owned(),
        accent: false,
    }
}

/// The host chips among `chips`.
fn host_chips(chips: Option<Vec<HeaderChip>>) -> Vec<HeaderChip> {
    chips
        .expect("the pane shows a session")
        .into_iter()
        .filter(|chip| chip.kind == HeaderChipKind::Host)
        .collect()
}

/// Whether the element tagged `selector` was painted in the main window.
fn drawn(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).origin.x >= px(0.0)
}

#[gpui::test]
fn a_remote_pane_header_shows_its_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = mixed_tabs(cx, &dir);

    let parts = h.root(|root, _| root.pane_header_parts("p2"));
    assert_eq!(
        host_chips(parts.map(|parts| parts.chips)),
        [box_chip()],
        "b1's pane names its host by its short name, the full name on hover"
    );
    assert!(drawn(&mut h, "pane-chip-p2-host"), "the chip is drawn");
}

#[gpui::test]
fn a_local_pane_header_shows_no_host_chip(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = mixed_tabs(cx, &dir);

    let parts = h.root(|root, _| root.pane_header_parts("p1"));
    assert_eq!(
        host_chips(parts.map(|parts| parts.chips)),
        [],
        "This PC's session carries no host chip"
    );
    assert!(!drawn(&mut h, "pane-chip-p1-host"));
}

#[gpui::test]
fn a_tab_of_one_remote_hosts_panes_shows_its_host_on_the_pill(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = mixed_tabs(cx, &dir);

    assert_eq!(
        h.root(|root, _| root.tab_host_chip("t2")).as_deref(),
        Some("box"),
        "t2's only pane is on box"
    );
    assert!(drawn(&mut h, "tab-host-t2"), "its pill draws the chip");
}

#[gpui::test]
fn a_tab_mixing_hosts_shows_no_pill_chip(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = mixed_tabs(cx, &dir);

    assert_eq!(
        h.root(|root, _| root.tab_host_chip("t1")),
        None,
        "t1 holds a This PC pane beside a box pane"
    );
    assert!(!drawn(&mut h, "tab-host-t1"));
}

#[gpui::test]
fn a_popped_out_remote_pane_shows_its_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = mixed_tabs(cx, &dir);
    let key = PopoutKey::Pane {
        tab_id: "t1".to_owned(),
        pane_id: "p2".to_owned(),
    };
    assert_eq!(
        h.root_update(move |root, window, cx| root.open_popout(key, window, cx)),
        OpenOutcome::Opened
    );
    let mut popout: VisualTestContext = h.popout_window();

    let parts = h.root(|root, _| root.popout_header_parts("p2"));
    assert_eq!(
        host_chips(parts.map(|parts| parts.chips)),
        [box_chip()],
        "the pop-out header shows the host as the main header did"
    );
    popout.run_until_parked();
    assert!(
        popout.debug_bounds("pane-chip-p2-host").is_some(),
        "the pop-out draws the chip"
    );
}
