//! Delete-worktree confirm specs: every worktree delete asks the daemon for
//! each branch's fate, offers the matching buttons with the safe one
//! focused, falls back after 10 s, and on confirm closes the panes, stops a
//! live session and discards it with the chosen branch cleanup.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

#[expect(dead_code, reason = "each spec file uses its own share of the helper")]
mod support;

use std::time::Duration;

use gpui::{Modifiers, TestAppContext};
use protocol::{
    BranchCleanup, BranchFate, CleanupAction, ClientMessage, DaemonMessage, GridNode,
    MemberBranchFate, MergeEvidence, SessionSnapshot, SplitDirection, UntouchedReason,
};
use support::{Fixture, Harness, TestDir, pane, session, split, tab};

fn dialog_of(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.delete_dialog_session().map(str::to_owned))
}

fn buttons(h: &mut Harness<'_>) -> Vec<(String, String)> {
    h.root(|root, _| root.delete_dialog_buttons())
}

fn text(h: &mut Harness<'_>) -> Vec<String> {
    h.root(|root, _| root.delete_dialog_text())
}

fn focus(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.delete_dialog_focus())
}

fn pairs(expected: &[(&str, &str)]) -> Vec<(String, String)> {
    expected
        .iter()
        .map(|(s, l)| ((*s).to_owned(), (*l).to_owned()))
        .collect()
}

fn fate(repo: &str, fate: BranchFate) -> MemberBranchFate {
    MemberBranchFate {
        repo_id: repo.to_owned(),
        repo_name: repo.to_owned(),
        branch: "wt/x".to_owned(),
        fate,
    }
}

fn preview(h: &mut Harness<'_>, id: &str, members: Vec<MemberBranchFate>) {
    h.send(DaemonMessage::DiscardPreview {
        session_id: id.to_owned(),
        members,
    });
}

fn landed() -> BranchFate {
    BranchFate::WillDelete {
        into: "origin/main".to_owned(),
        via: MergeEvidence::Ancestry,
    }
}

fn kept(commits: u32) -> BranchFate {
    BranchFate::KeptByDefault {
        unique_commits: Some(commits),
        checked_against: vec!["origin/main".to_owned(), "main".to_owned()],
    }
}

fn cleanup(repos: &[&str], branch: BranchCleanup) -> Vec<CleanupAction> {
    repos
        .iter()
        .map(|repo| CleanupAction {
            repo_id: (*repo).to_owned(),
            remove_worktree: true,
            branch,
        })
        .collect()
}

fn is_discard(msg: &ClientMessage, id: &str, expected: &[CleanupAction]) -> bool {
    matches!(msg, ClientMessage::DiscardSession { session_id, cleanup } if session_id == id && cleanup == expected)
}

fn is_preview(sent: &[ClientMessage], id: &str) -> bool {
    matches!(sent, [ClientMessage::PreviewDiscard { session_id }] if session_id == id)
}

/// `s1` alone in pane `p1` of tab `t1`, and nothing sent yet.
fn single<'a>(cx: &'a mut TestAppContext, dir: &TestDir, s1: SessionSnapshot) -> Harness<'a> {
    let mut h = Harness::with(cx, dir, &Fixture::single(s1));
    h.sent();
    h
}

/// An exited `s1` on a worktree of `r1`, its overlay's delete clicked.
fn overlay_dialog<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = single(cx, dir, session("s1").worktree("r1").exited(1).build());
    h.click_on("exited-remove-pane-delete-p1");
    assert!(is_preview(&h.sent(), "s1"));
    h
}

/// A workspace session on worktrees of `r1` and `r2`.
fn two_members(status: &str) -> SessionSnapshot {
    let mut s = session("s1").worktree("r1").status(status).build();
    let mut second = s.members[0].clone();
    "r2".clone_into(&mut second.repo_id);
    "r2".clone_into(&mut second.repo_name);
    s.members.push(second);
    s
}

#[gpui::test]
fn delete_worktree_opens_dialog_and_requests_preview(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = single(cx, &dir, session("s1").worktree("r1").build());

    h.right_click_on("leaf-s1");
    h.click_on("menu-stop");
    h.click_on("menu-stop-delete");
    let sent = h.sent();
    assert!(
        is_preview(&sent, "s1"),
        "only the preview request: {sent:?}"
    );
    assert_eq!(dialog_of(&mut h).as_deref(), Some("s1"));
    assert_eq!(
        h.root(|root, _| root.session_menu().map(str::to_owned)),
        None,
        "the menu gives way to the dialog"
    );
    assert_eq!(
        text(&mut h),
        ["Removing the worktree for s1.", "Checking branch state…"]
    );
    assert_eq!(
        buttons(&mut h),
        pairs(&[("delete-worktree-cancel", "Cancel")])
    );
    assert!(h.bounds("delete-worktree-cancel").origin.x >= gpui::px(0.0));
}

#[gpui::test]
fn preview_will_delete_offers_delete_all_and_sends_delete(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = overlay_dialog(cx, &dir);

    preview(&mut h, "s1", vec![fate("r1", landed())]);
    assert_eq!(
        buttons(&mut h),
        pairs(&[
            ("delete-worktree-cancel", "Cancel"),
            ("delete-worktree-and-branch", "Delete worktree and branch"),
        ])
    );
    assert_eq!(
        text(&mut h),
        [
            "Removing the worktree for s1.",
            "r1 wt/x already merged into origin/main"
        ]
    );
    h.click_on("delete-worktree-and-branch");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [discard]
            if is_discard(discard, "s1", &cleanup(&["r1"], BranchCleanup::Delete))),
        "every branch landed, so the one button deletes them; the daemon's discard closes the pane: sent {sent:?}"
    );
    assert_eq!(dialog_of(&mut h), None);
}

#[gpui::test]
fn kept_by_default_member_shows_unique_commits_and_keeps_branch(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = overlay_dialog(cx, &dir);

    preview(&mut h, "s1", vec![fate("r1", kept(3))]);
    assert_eq!(
        text(&mut h),
        [
            "Removing the worktree for s1.",
            "r1 wt/x 3 commits not in origin/main, main"
        ]
    );
    assert_eq!(
        buttons(&mut h),
        pairs(&[
            ("delete-worktree-cancel", "Cancel"),
            (
                "delete-worktree-keep-branch",
                "Delete worktree, keep branch"
            ),
            (
                "delete-worktree-and-branch",
                "Delete worktree and branch (loses 3 commits)"
            ),
        ])
    );
    h.click_on("delete-worktree-keep-branch");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [discard]
            if is_discard(discard, "s1", &cleanup(&["r1"], BranchCleanup::Keep))),
        "sent {sent:?}"
    );
}

#[gpui::test]
fn choose_mode_sends_per_member_branch_cleanup(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = single(cx, &dir, two_members("stopped"));
    h.right_click_on("leaf-s1");
    h.click_on("menu-remove-pane-delete");
    assert!(is_preview(&h.sent(), "s1"));

    preview(
        &mut h,
        "s1",
        vec![fate("r1", landed()), fate("r2", kept(2))],
    );
    assert_eq!(
        text(&mut h),
        [
            "Removing the worktree for s1.",
            "r1 wt/x already merged into origin/main",
            "r2 wt/x 2 commits not in origin/main, main",
        ]
    );
    h.click_on("delete-worktree-and-branch");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [discard]
            if is_discard(discard, "s1", &cleanup(&["r1", "r2"], BranchCleanup::Delete))),
        "one cleanup per member, each with the chosen branch: sent {sent:?}"
    );
}

#[gpui::test]
fn worktree_only_mode_keeps_every_branch(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        sessions: vec![two_members("stopped")],
        tabs: vec![tab("t1", &pane("p1", None))],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.sent();
    h.right_click_on("leaf-s1");
    h.click_on("menu-remove-pane-delete");
    assert!(is_preview(&h.sent(), "s1"));

    preview(
        &mut h,
        "s1",
        vec![
            fate(
                "r1",
                BranchFate::Untouched {
                    reason: UntouchedReason::ExternalWorktree,
                },
            ),
            fate(
                "r2",
                BranchFate::Untouched {
                    reason: UntouchedReason::BranchMissing,
                },
            ),
        ],
    );
    assert_eq!(
        text(&mut h),
        [
            "Removing the worktree for s1.",
            "r1 wt/x not managed by rustling-tulip; left alone",
            "r2 wt/x branch no longer exists",
        ]
    );
    assert_eq!(
        buttons(&mut h),
        pairs(&[
            ("delete-worktree-cancel", "Cancel"),
            ("delete-worktree-only", "Delete worktree"),
        ])
    );
    h.click_on("delete-worktree-only");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [discard]
            if is_discard(discard, "s1", &cleanup(&["r1", "r2"], BranchCleanup::Keep))),
        "no pane shows it, so only the discard: sent {sent:?}"
    );
}

#[gpui::test]
fn no_preview_within_10s_shows_fallback(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = overlay_dialog(cx, &dir);

    h.advance(Duration::from_millis(9_900));
    assert_eq!(
        text(&mut h),
        ["Removing the worktree for s1.", "Checking branch state…"]
    );
    h.advance(Duration::from_millis(100));
    assert_eq!(
        text(&mut h),
        [
            "Removing the worktree for s1.",
            "Couldn't determine branch state."
        ]
    );
    assert_eq!(
        buttons(&mut h),
        pairs(&[
            ("delete-worktree-cancel", "Cancel"),
            (
                "delete-worktree-keep-branch",
                "Delete worktree, keep branch"
            ),
            (
                "delete-worktree-and-branch",
                "Delete worktree and branch (commit count unknown)"
            ),
        ])
    );
    assert_eq!(
        focus(&mut h).as_deref(),
        Some("delete-worktree-keep-branch")
    );
    assert!(h.sent().is_empty(), "the fallback sends nothing");

    preview(&mut h, "s1", vec![fate("r1", landed())]);
    assert_eq!(
        buttons(&mut h),
        pairs(&[
            ("delete-worktree-cancel", "Cancel"),
            ("delete-worktree-and-branch", "Delete worktree and branch"),
        ]),
        "a late preview replaces the fallback"
    );
}

#[gpui::test]
fn confirm_stops_live_session_then_discards(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let grid: GridNode = split(
        SplitDirection::Horizontal,
        pane("p1", Some("s1")),
        pane("p2", Some("s1")),
    );
    let fixture = Fixture {
        sessions: vec![session("s1").worktree("r1").build()],
        tabs: vec![tab("t1", &grid)],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.sent();

    h.right_click_on("leaf-s1");
    h.click_on("menu-stop");
    h.click_on("menu-stop-delete");
    assert!(is_preview(&h.sent(), "s1"));
    preview(&mut h, "s1", vec![fate("r1", landed())]);
    h.click_on("delete-worktree-and-branch");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [
            ClientMessage::StopSession { session_id: stopped, cleanup: stop_cleanup },
            discard,
        ] if stopped == "s1" && stop_cleanup.is_empty()
            && is_discard(discard, "s1", &cleanup(&["r1"], BranchCleanup::Delete))),
        "no ClosePane for its two panes: the daemon's discard closes them: sent {sent:?}"
    );
}

#[gpui::test]
fn session_stopping_while_dialog_open_skips_stop_on_confirm(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = single(cx, &dir, session("s1").worktree("r1").build());
    h.right_click_on("leaf-s1");
    h.click_on("menu-stop");
    h.click_on("menu-stop-delete");
    assert!(is_preview(&h.sent(), "s1"));

    h.send(DaemonMessage::SessionUpdated {
        session: session("s1").worktree("r1").exited(0).build(),
        request_id: None,
    });
    preview(&mut h, "s1", vec![fate("r1", landed())]);
    h.click_on("delete-worktree-and-branch");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [discard]
            if is_discard(discard, "s1", &cleanup(&["r1"], BranchCleanup::Delete))),
        "it stopped meanwhile, so no StopSession: sent {sent:?}"
    );
}

#[gpui::test]
fn dialog_closes_when_its_session_is_removed(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = overlay_dialog(cx, &dir);

    h.send(DaemonMessage::SessionRemoved {
        session_id: "s1".to_owned(),
    });
    assert_eq!(dialog_of(&mut h), None);
    assert!(
        h.sent().is_empty(),
        "nothing is discarded for a gone session"
    );
}

/// A live `s1` in pane `p1` whose scrollback is in, so keys reach it, with
/// the dialog opened from its menu.
fn live_dialog<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = single(cx, dir, session("s1").worktree("r1").build());
    h.answer_scrollback("s1", b"");
    h.keys("a");
    assert_eq!(h.sent_input("s1"), b"a", "the pane has the keyboard");
    open_from_menu(&mut h);
    h
}

fn open_from_menu(h: &mut Harness<'_>) {
    h.right_click_on("leaf-s1");
    h.click_on("menu-stop");
    h.click_on("menu-stop-delete");
    assert!(is_preview(&h.sent(), "s1"));
}

#[gpui::test]
fn dialog_keys_never_reach_the_terminal(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = live_dialog(cx, &dir);

    h.keys("a tab");
    assert!(h.sent_input("s1").is_empty(), "the dialog has the keyboard");

    let grid = split(
        SplitDirection::Horizontal,
        pane("p1", Some("s1")),
        pane("p2", Some("s1")),
    );
    h.send(DaemonMessage::TabUpdated {
        tab: tab("t1", &grid),
    });
    h.answer_scrollback("s1", b"");
    assert_eq!(dialog_of(&mut h).as_deref(), Some("s1"));
    h.keys("a");
    assert!(
        h.sent_input("s1").is_empty(),
        "a new pane's focus request leaves the keyboard with the dialog"
    );
}

#[gpui::test]
fn closing_the_dialog_returns_keys_to_the_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = live_dialog(cx, &dir);

    h.keys("escape");
    assert_eq!(dialog_of(&mut h), None);
    h.keys("a");
    assert_eq!(h.sent_input("s1"), b"a", "after Esc");

    open_from_menu(&mut h);
    h.click_on("delete-worktree-cancel");
    assert_eq!(dialog_of(&mut h), None);
    h.keys("a");
    assert_eq!(h.sent_input("s1"), b"a", "after Cancel");
}

#[gpui::test]
fn esc_and_cancel_close_without_sending_discard(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = overlay_dialog(cx, &dir);
    preview(&mut h, "s1", vec![fate("r1", kept(1))]);

    h.keys("escape");
    assert_eq!(dialog_of(&mut h), None, "Esc closes the dialog");

    h.click_on("exited-remove-pane-delete-p1");
    assert!(is_preview(&h.sent(), "s1"));
    let backdrop = h.center("new-tab");
    h.click(backdrop, Modifiers::none());
    assert_eq!(
        dialog_of(&mut h).as_deref(),
        Some("s1"),
        "a backdrop click leaves the dialog open"
    );
    h.click_on("delete-worktree-cancel");
    assert_eq!(dialog_of(&mut h), None, "Cancel closes the dialog");

    h.click_on("exited-remove-pane-delete-p1");
    assert!(is_preview(&h.sent(), "s1"));
    h.click_on("delete-worktree-dialog-close");
    assert_eq!(dialog_of(&mut h), None, "the ✕ closes the dialog");
    let sent = h.sent();
    assert!(
        sent.is_empty(),
        "nothing closed, stopped or discarded: {sent:?}"
    );
}

#[gpui::test]
fn safe_button_is_focused_first(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = overlay_dialog(cx, &dir);
    assert_eq!(focus(&mut h).as_deref(), Some("delete-worktree-cancel"));

    preview(&mut h, "s1", vec![fate("r1", landed())]);
    assert_eq!(
        focus(&mut h).as_deref(),
        Some("delete-worktree-cancel"),
        "one delete button: Cancel keeps the focus"
    );
    h.keys("enter");
    assert_eq!(dialog_of(&mut h), None);
    assert!(h.sent().is_empty(), "a stray Enter deletes nothing");

    h.click_on("exited-remove-pane-delete-p1");
    h.sent();
    preview(&mut h, "s1", vec![fate("r1", kept(2))]);
    assert_eq!(
        focus(&mut h).as_deref(),
        Some("delete-worktree-keep-branch"),
        "with a choice, keeping the branch is the safe answer"
    );
    h.keys("enter");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [discard]
            if is_discard(discard, "s1", &cleanup(&["r1"], BranchCleanup::Keep))),
        "Enter presses the focused keep: sent {sent:?}"
    );

    h.click_on("exited-remove-pane-delete-p1");
    h.sent();
    preview(&mut h, "s1", vec![fate("r1", kept(2))]);
    h.keys("tab");
    assert_eq!(
        focus(&mut h).as_deref(),
        Some("delete-worktree-and-branch"),
        "Tab moves along the footer"
    );
    assert_eq!(
        dialog_of(&mut h).as_deref(),
        Some("s1"),
        "Tab presses nothing"
    );
    h.keys("space");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [discard]
            if is_discard(discard, "s1", &cleanup(&["r1"], BranchCleanup::Delete))),
        "Space presses the focused button: sent {sent:?}"
    );
}
