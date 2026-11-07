//! Agent-exit specs: a session whose agent ends keeps its pane as a shell
//! with no toast, and the leaf says which program the pane runs.

use crate::support;

use gpui::TestAppContext;
use protocol::{DaemonMessage, SessionMode, SessionNoticeKind};
use rustling_tulip_native::chat::PaneView;
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

/// Why `p1`'s header cannot switch to its chat now; `None` when it can.
fn chat_tip(h: &mut Harness<'_>) -> Option<&'static str> {
    h.root(|root, _| root.pane_view_disabled_tip("p1", PaneView::Chat))
}

/// A plain shell the daemon promoted when its screen showed the agent: an
/// interactive session naming the agent, its leaf the agent's.
fn promoted_shell(program: &str) -> protocol::SessionSnapshot {
    let mut snap = session("s1").shell("D:/work").program_name(program).build();
    snap.mode = SessionMode::Interactive;
    snap
}

#[gpui::test]
fn plain_shell_becomes_chat_capable_when_promoted(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(
        cx,
        &dir,
        &Fixture::single(
            session("s1")
                .shell("D:/work")
                .no_agent()
                .program_name("pwsh")
                .build(),
        ),
    );
    h.sent();
    assert_eq!(
        chat_tip(&mut h),
        Some("The agent isn't running"),
        "a plain shell offers no chat"
    );

    h.send(DaemonMessage::SessionUpdated {
        session: promoted_shell("claude"),
        request_id: None,
    });
    assert_eq!(
        chat_tip(&mut h),
        None,
        "the promoted shell's Chat is enabled"
    );

    h.send(DaemonMessage::SessionUpdated {
        session: promoted_shell("pwsh"),
        request_id: None,
    });
    exited(&mut h, "s1");
    assert_eq!(
        chat_tip(&mut h),
        Some("The agent isn't running"),
        "the agent's exit disables Chat"
    );
}

/// Whether leaf `id` holds a finished turn this client has not shown.
fn leaf_unseen(h: &mut Harness<'_>, id: &str) -> Option<bool> {
    h.root(|root, _| {
        root.sidebar_containers()
            .into_iter()
            .flat_map(|c| c.leaves)
            .find(|leaf| leaf.id == id)
            .map(|leaf| leaf.unseen)
    })
}

#[gpui::test]
fn a_turn_ending_after_the_agent_exited_is_not_waiting(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);

    h.send(DaemonMessage::SessionUpdated {
        session: session("s1").status("working").build(),
        request_id: None,
    });
    h.send(DaemonMessage::SessionUpdated {
        session: session("s1").program_name("nu").build(),
        request_id: None,
    });
    assert_eq!(
        leaf_unseen(&mut h, "s1"),
        Some(false),
        "nothing waits on the user: the pane is back to its shell"
    );
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
