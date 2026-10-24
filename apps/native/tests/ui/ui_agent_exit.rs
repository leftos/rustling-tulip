//! Agent-exit specs: a session whose agent ends keeps its pane as a shell,
//! the client offers to close it in a timed toast, and the leaf says which
//! program the pane runs.

use crate::support;

use gpui::TestAppContext;
use protocol::{ClientMessage, DaemonMessage};
use rustling_tulip_native::toast_action_selector;
use support::{Fixture, Harness, TestDir, session};

/// Every toast's title and detail, oldest first.
fn toasts(h: &mut Harness<'_>) -> Vec<(String, Option<String>)> {
    h.root(|root, _| {
        root.toasts()
            .iter()
            .map(|toast| (toast.title.clone(), toast.detail.clone()))
            .collect()
    })
}

/// The window opened on the fake daemon with one idle Claude session `s1`.
fn opened<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = Harness::with(cx, dir, &Fixture::single(session("s1").build()));
    h.sent();
    h
}

/// The daemon's notice that session `id`'s agent ended.
fn exited(h: &mut Harness<'_>, id: &str) {
    h.send(DaemonMessage::AgentExited {
        session_id: id.to_owned(),
    });
}

#[gpui::test]
fn agent_exited_raises_the_shell_toast(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);

    exited(&mut h, "s1");

    assert_eq!(
        toasts(&mut h),
        [("Claude exited. The pane is a shell now.".to_owned(), None)],
        "one toast says the pane is a shell"
    );
}

#[gpui::test]
fn close_session_stops_and_discards_the_session(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    exited(&mut h, "s1");
    let id = h.root(|root, _| root.toasts()[0].id);

    h.click_on(&toast_action_selector(id, "close-session"));

    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [
            ClientMessage::StopSession { session_id: stopped, cleanup: stop_cleanup },
            ClientMessage::DiscardSession { session_id: discarded, cleanup },
        ] if stopped == "s1" && stop_cleanup.is_empty()
            && discarded == "s1" && cleanup.is_empty()),
        "the session stops then goes, and its worktrees stay on disk: {sent:?}"
    );
    assert!(toasts(&mut h).is_empty(), "the toast goes with the press");
}

#[gpui::test]
fn agent_exited_for_an_unknown_session_raises_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);

    exited(&mut h, "gone");

    assert!(toasts(&mut h).is_empty(), "nothing to close, no toast");
}

#[gpui::test]
fn leaf_reads_the_shell_after_claude_exits(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);

    h.send(DaemonMessage::SessionUpdated {
        session: session("s1").program_name("pwsh").build(),
        request_id: None,
    });

    let tags = h.root(|root, _| root.leaf_tags("s1"));
    assert_eq!(
        tags.first().map(|(text, _)| text.as_str()),
        Some("pwsh"),
        "the leaf names the program the pane runs now"
    );
}
