//! Agent-exit specs: a session whose agent ends keeps its pane as a shell
//! with no toast, and the leaf says which program the pane runs.

use crate::support;

use gpui::TestAppContext;
use protocol::{DaemonMessage, SessionNoticeKind};
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
fn agent_exited_raises_no_toast(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);

    exited(&mut h, "s1");

    assert!(
        toasts(&mut h).is_empty(),
        "the pane shows the shell, no toast says so"
    );
}

#[gpui::test]
fn a_session_notice_raises_its_toast(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);

    h.send(DaemonMessage::SessionNotice {
        session_id: "s1".to_owned(),
        notice: SessionNoticeKind::WaitingInMain,
    });

    assert_eq!(
        toasts(&mut h),
        [(
            "Claude is waiting in the main conversation.".to_owned(),
            None
        )],
        "one toast names the agent that waits"
    );
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
