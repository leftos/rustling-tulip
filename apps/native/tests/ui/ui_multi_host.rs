//! Multi-host specs: the connections the window opens, one per host, and
//! which host's connection each message goes out on.

use crate::support;

use gpui::{Modifiers, TestAppContext};
use protocol::ClientMessage;
use rustling_tulip_native::target::Target;
use support::{Fixture, Harness, TestDir, session};

#[gpui::test]
fn the_window_opens_one_connection_for_the_boot_target(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));

    assert_eq!(
        h.opened_targets(),
        [Target::Local],
        "a default start opens one connection, to this PC"
    );

    h.answer_scrollback("s1", b"");
    let at = h.cell_center("p1", 0, 0);
    h.click(at, Modifiers::none());
    h.sent();
    h.keys("a");
    let typed: Vec<String> = h
        .sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::SendInput { session_id, .. } => Some(session_id),
            _ => None,
        })
        .collect();
    assert_eq!(
        typed,
        ["s1"],
        "a key typed into the pane goes out on this PC's connection"
    );
}
