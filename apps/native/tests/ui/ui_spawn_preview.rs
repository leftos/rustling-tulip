//! Spawn dialog preview specs: a repo's preview goes out 250 ms after the
//! last change and never without a new worktree, replies for fields no
//! longer on show are dropped, the staleness warning and the collision
//! notice show what the reply says, the collision choice reaches the spawn
//! and resets on a change; a workspace previews on its button into a table.

#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use std::time::Duration;

use gpui::{TestAppContext, px};
use protocol::{
    ClientMessage, DaemonMessage, MemberSpawnPreview, SpawnRequest, SpawnTarget,
    WorktreeReusePolicy,
};
use support::{Fixture, Harness, TestDir, repo, session, workspace};

/// Repos `r0` and `r1`, workspace `w1` of both; session `s1` of `r1` alone
/// in pane `p1` of tab `t1`.
fn fixture() -> Fixture {
    let mut fixture = Fixture::single(session("s1").in_repo("r1").build());
    fixture.repos = vec![repo("r0", "C:/r0"), repo("r1", "C:/r1")];
    fixture.workspaces = vec![workspace("w1", &["r0", "r1"])];
    fixture
}

fn is_open(h: &mut Harness<'_>) -> bool {
    h.root(|root, _| root.spawn_dialog_open())
}

fn selected(h: &mut Harness<'_>) -> Vec<String> {
    h.root(|root, _| root.spawn_dialog_selected())
}

fn focus(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.spawn_dialog_focus())
}

fn stale(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.spawn_base_stale())
}

fn collision(h: &mut Harness<'_>) -> Option<Vec<String>> {
    h.root(|root, _| root.spawn_collision())
}

/// Whether the element tagged `selector` is on screen.
fn drawn(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).origin.x >= px(0.0)
}

/// Opens the dialog on `r1` and answers its branches; nothing is typed in
/// the branch field yet.
fn open(h: &mut Harness<'_>) {
    h.click_on("sidebar-add-session");
    assert!(is_open(h), "the dialog opened");
    h.send(DaemonMessage::Branches {
        repo_id: "r1".to_owned(),
        branches: vec!["main".to_owned()],
        current: Some("main".to_owned()),
        remote_branches: vec!["origin/main".to_owned()],
    });
    h.sent();
}

/// Types `keys` in the focused field and closes the list the edit opened.
fn type_keys(h: &mut Harness<'_>, keys: &str) {
    h.keys(keys);
    h.keys("escape");
    assert!(is_open(h), "Esc closed the list only");
}

/// Each `PreviewSpawn` in `sent`: repo, branch and base.
fn previews(sent: &[ClientMessage]) -> Vec<(String, String, Option<String>)> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::PreviewSpawn {
                repo_id,
                branch_name,
                base_branch,
                use_worktree,
                ..
            } => {
                assert!(use_worktree, "only a new worktree is previewed");
                Some((repo_id.clone(), branch_name.clone(), base_branch.clone()))
            }
            _ => None,
        })
        .collect()
}

fn member(repo_id: &str) -> MemberSpawnPreview {
    MemberSpawnPreview {
        repo_id: repo_id.to_owned(),
        repo_name: repo_id.to_uppercase(),
        branch_exists: false,
        effective_base: Some("main".to_owned()),
        worktree_path: format!("C:/wt/{repo_id}"),
        resolved_base_ref: Some("origin/main".to_owned()),
        base_remote_ref: Some("origin/main".to_owned()),
        base_behind_remote: None,
        worktree_exists: false,
        existing_worktree_head: None,
        existing_worktree_dirty: false,
        existing_worktree_behind_base: None,
        existing_branch_head: None,
        existing_branch_behind_base: None,
    }
}

/// `r1`'s preview, three commits behind its remote.
fn behind() -> MemberSpawnPreview {
    MemberSpawnPreview {
        base_behind_remote: Some(3),
        ..member("r1")
    }
}

/// `r1`'s preview with a worktree already at the path.
fn colliding(dirty: bool) -> MemberSpawnPreview {
    MemberSpawnPreview {
        worktree_exists: true,
        existing_worktree_head: Some("abc1234".to_owned()),
        existing_worktree_dirty: dirty,
        ..member("r1")
    }
}

/// `r1`'s id-less reply for `branch`, as a daemon that echoes no id sends.
fn reply(h: &mut Harness<'_>, branch: &str, preview: MemberSpawnPreview) {
    reply_to(h, branch, preview, None);
}

/// `r1`'s reply for `branch` to the request stamped `request_id`.
fn reply_to(
    h: &mut Harness<'_>,
    branch: &str,
    preview: MemberSpawnPreview,
    request_id: Option<String>,
) {
    h.send(DaemonMessage::SpawnPreview {
        repo_id: "r1".to_owned(),
        branch_name: branch.to_owned(),
        preview,
        request_id,
    });
}

/// The `request_id` of the one preview request (repo or workspace) in `sent`.
fn preview_id(sent: &[ClientMessage]) -> Option<String> {
    let ids: Vec<Option<String>> = sent
        .iter()
        .filter_map(|msg| match msg {
            ClientMessage::PreviewSpawn { request_id, .. }
            | ClientMessage::PreviewWorkspaceSpawn { request_id, .. } => Some(request_id.clone()),
            _ => None,
        })
        .collect();
    match ids.as_slice() {
        [id] => id.clone(),
        other => panic!("expected one preview request, sent {other:?}"),
    }
}

/// Waits out the debounce; returns what went out.
fn settle(h: &mut Harness<'_>) -> Vec<ClientMessage> {
    h.advance(Duration::from_millis(260));
    h.sent()
}

fn the_spawn(h: &mut Harness<'_>) -> SpawnRequest {
    h.click_on("spawn-submit");
    let sent = h.sent();
    let spawns: Vec<&SpawnRequest> = sent
        .iter()
        .filter_map(|msg| match msg {
            ClientMessage::SpawnSession(request) => Some(request),
            _ => None,
        })
        .collect();
    match spawns.as_slice() {
        [request] => (*request).clone(),
        other => panic!("expected one SpawnSession, sent {other:?}"),
    }
}

fn reuse_of(request: &SpawnRequest) -> WorktreeReusePolicy {
    match &request.target {
        SpawnTarget::Single { worktree_reuse, .. }
        | SpawnTarget::Workspace { worktree_reuse, .. } => *worktree_reuse,
        other @ SpawnTarget::Standalone { .. } => panic!("a worktree spawn, not {other:?}"),
    }
}

#[gpui::test]
fn preview_sent_after_250ms_debounce(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    open(&mut h);
    type_keys(&mut h, "a");
    h.advance(Duration::from_millis(200));
    assert!(previews(&h.sent()).is_empty(), "nothing at 200 ms");
    h.advance(Duration::from_millis(60));
    assert_eq!(
        previews(&h.sent()),
        [(
            "r1".to_owned(),
            "a".to_owned(),
            Some("origin/main".to_owned())
        )],
        "one by 260 ms"
    );

    h.click_on("spawn-branch");
    for key in ["b", "c", "d"] {
        h.keys(key);
        h.advance(Duration::from_millis(100));
    }
    assert!(
        previews(&h.sent()).is_empty(),
        "each keystroke restarts the wait"
    );
    assert_eq!(
        previews(&settle(&mut h)),
        [(
            "r1".to_owned(),
            "abcd".to_owned(),
            Some("origin/main".to_owned())
        )],
        "one for the last value only"
    );
}

#[gpui::test]
fn no_preview_when_worktree_off(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    open(&mut h);
    assert!(
        previews(&settle(&mut h)).is_empty(),
        "no branch, no preview"
    );
    type_keys(&mut h, "a");
    h.click_on("spawn-worktree");
    assert!(
        !selected(&mut h).contains(&"spawn-worktree".to_owned()),
        "in place"
    );
    assert!(
        previews(&settle(&mut h)).is_empty(),
        "no worktree, no preview"
    );

    h.click_on("spawn-worktree");
    h.click_on("spawn-worktree-mode-existing");
    assert!(
        previews(&settle(&mut h)).is_empty(),
        "an existing worktree, no preview"
    );
}

#[gpui::test]
fn stale_reply_for_old_branch_is_dropped(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    open(&mut h);
    type_keys(&mut h, "a");
    assert_eq!(previews(&settle(&mut h)).len(), 1);
    h.click_on("spawn-branch");
    type_keys(&mut h, "b");
    reply(&mut h, "a", behind());
    assert_eq!(
        stale(&mut h),
        None,
        "the reply for the old branch is dropped"
    );

    assert_eq!(previews(&settle(&mut h))[0].1, "ab");
    reply(&mut h, "ab", behind());
    assert!(stale(&mut h).is_some(), "the reply for the branch on show");
}

#[gpui::test]
fn stale_reply_for_old_base_is_dropped(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    open(&mut h);
    type_keys(&mut h, "a");
    assert_eq!(previews(&settle(&mut h)).len(), 1);
    h.click_on("spawn-base-branch");
    type_keys(&mut h, "ctrl-a m a i n");
    reply(&mut h, "a", behind());
    assert_eq!(
        stale(&mut h),
        None,
        "same repo and branch, but asked for the old base"
    );

    assert_eq!(
        previews(&settle(&mut h)),
        [("r1".to_owned(), "a".to_owned(), Some("main".to_owned()))]
    );
    reply(&mut h, "a", behind());
    assert!(stale(&mut h).is_some());
}

#[gpui::test]
fn older_reply_for_same_branch_after_base_change_is_dropped(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    open(&mut h);
    type_keys(&mut h, "a");
    let first = preview_id(&settle(&mut h));
    h.click_on("spawn-base-branch");
    type_keys(&mut h, "ctrl-a m a i n");
    let sent = settle(&mut h);
    assert_eq!(
        previews(&sent),
        [("r1".to_owned(), "a".to_owned(), Some("main".to_owned()))],
        "the second request asks for the new base"
    );
    let second = preview_id(&sent);

    reply_to(&mut h, "a", behind(), first);
    assert_eq!(
        stale(&mut h),
        None,
        "the reply to the request for the old base is dropped"
    );
    reply_to(&mut h, "a", behind(), second);
    assert!(stale(&mut h).is_some(), "the reply to the latest request");
}

#[gpui::test]
fn reply_after_dialog_reopened_is_dropped(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    open(&mut h);
    type_keys(&mut h, "a");
    let first = preview_id(&settle(&mut h));
    h.click_on("spawn-cancel");
    assert!(!is_open(&mut h), "the dialog closed");

    open(&mut h);
    type_keys(&mut h, "a");
    let second = preview_id(&settle(&mut h));
    reply_to(&mut h, "a", behind(), first);
    assert_eq!(
        stale(&mut h),
        None,
        "the reply to the closed dialog's request is dropped"
    );
    reply_to(&mut h, "a", behind(), second);
    assert!(
        stale(&mut h).is_some(),
        "the reply to this dialog's request"
    );
}

#[gpui::test]
fn preview_error_with_id_clears_pending_and_toasts(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    open(&mut h);
    type_keys(&mut h, "a");
    let id = preview_id(&settle(&mut h));
    h.send(DaemonMessage::Error {
        message: "unknown repo: r1".to_owned(),
        request_id: id,
    });
    assert!(
        h.root(|root, _| root
            .toasts()
            .iter()
            .any(|toast| toast.title == "Daemon error")),
        "the failure still toasts"
    );
    reply(&mut h, "a", behind());
    assert_eq!(
        stale(&mut h),
        None,
        "the failed request is no longer waited for"
    );
}

#[gpui::test]
fn staleness_notice_shows_behind_count(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    open(&mut h);
    type_keys(&mut h, "a");
    settle(&mut h);
    reply(&mut h, "a", behind());
    assert_eq!(
        stale(&mut h).as_deref(),
        Some(
            "main is 3 commits behind origin/main. Branching from it forks from that older point."
        )
    );
    assert!(drawn(&mut h, "spawn-base-stale"));
    assert_eq!(collision(&mut h), None, "nothing is in the way");
}

#[gpui::test]
fn collision_recreate_sends_recreate_from_base(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    open(&mut h);
    type_keys(&mut h, "a");
    settle(&mut h);
    reply(&mut h, "a", colliding(false));
    assert_eq!(
        collision(&mut h),
        Some(vec![
            "A worktree already exists at this path at abc1234.".to_owned(),
            "Reuse it as-is (keeps its existing fork point; the base branch is not applied)"
                .to_owned(),
            "Recreate from the base branch (deletes and re-adds the worktree)".to_owned(),
        ])
    );
    assert!(selected(&mut h).contains(&"spawn-collision-reuse".to_owned()));

    assert_eq!(focus(&mut h).as_deref(), Some("spawn-branch"));
    h.keys("tab tab");
    assert_eq!(focus(&mut h).as_deref(), Some("spawn-base-branch"));
    h.keys("tab");
    assert_eq!(focus(&mut h).as_deref(), Some("spawn-collision-reuse"));
    h.keys("tab space");
    assert_eq!(focus(&mut h).as_deref(), Some("spawn-collision-recreate"));
    assert!(selected(&mut h).contains(&"spawn-collision-recreate".to_owned()));

    let request = the_spawn(&mut h);
    assert_eq!(reuse_of(&request), WorktreeReusePolicy::RecreateFromBase);
}

#[gpui::test]
fn change_after_choosing_recreate_resets_to_reuse(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    open(&mut h);
    type_keys(&mut h, "a");
    settle(&mut h);
    reply(&mut h, "a", colliding(false));
    h.click_on("spawn-collision-recreate");
    assert!(selected(&mut h).contains(&"spawn-collision-recreate".to_owned()));

    h.click_on("spawn-branch");
    type_keys(&mut h, "b");
    assert_eq!(collision(&mut h), None, "the change drops the preview");
    settle(&mut h);
    reply(&mut h, "ab", colliding(false));
    assert!(
        selected(&mut h).contains(&"spawn-collision-reuse".to_owned()),
        "back to Reuse"
    );
    let request = the_spawn(&mut h);
    assert_eq!(reuse_of(&request), WorktreeReusePolicy::Reuse);
}

#[gpui::test]
fn dirty_worktree_shows_danger_note(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    open(&mut h);
    type_keys(&mut h, "a");
    settle(&mut h);
    reply(&mut h, "a", colliding(true));
    let lines = collision(&mut h).expect("the collision shows");
    assert_eq!(
        lines[2],
        "Recreate from the base branch (discards uncommitted changes in that worktree)"
    );
    assert!(drawn(&mut h, "spawn-collision-danger"));
}

/// Opens the dialog on workspace `w1`.
fn open_workspace(h: &mut Harness<'_>) {
    h.click_on("sidebar-add-session");
    assert!(is_open(h), "the dialog opened");
    h.click_on("spawn-target-workspace-w1");
    assert!(selected(h).contains(&"spawn-target-workspace-w1".to_owned()));
    h.sent();
}

fn workspace_previews(sent: &[ClientMessage]) -> Vec<(String, String)> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::PreviewWorkspaceSpawn {
                workspace_id,
                branch_name,
                ..
            } => Some((workspace_id.clone(), branch_name.clone())),
            _ => None,
        })
        .collect()
}

/// Types branch `b` and presses Preview; returns the request's id.
fn preview_workspace(h: &mut Harness<'_>) -> Option<String> {
    h.click_on("spawn-branch");
    h.keys("b");
    press_preview(h)
}

/// Presses Preview; returns the request's id.
fn press_preview(h: &mut Harness<'_>) -> Option<String> {
    assert!(h.root(|root, _| root.spawn_preview_enabled()));
    h.click_on("spawn-preview");
    let sent = h.sent();
    assert_eq!(
        workspace_previews(&sent),
        [("w1".to_owned(), "b".to_owned())]
    );
    preview_id(&sent)
}

/// `w1`'s id-less reply for branch `b`.
fn workspace_reply(h: &mut Harness<'_>, per_member: Vec<MemberSpawnPreview>) {
    workspace_reply_to(h, per_member, None);
}

fn workspace_reply_to(
    h: &mut Harness<'_>,
    per_member: Vec<MemberSpawnPreview>,
    request_id: Option<String>,
) {
    h.send(DaemonMessage::WorkspaceSpawnPreview {
        workspace_id: "w1".to_owned(),
        branch_name: "b".to_owned(),
        per_member,
        request_id,
    });
}

#[gpui::test]
fn workspace_older_reply_is_dropped(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    open_workspace(&mut h);
    let first = preview_workspace(&mut h);
    h.click_on("spawn-base-branch");
    h.keys("x");
    let second = press_preview(&mut h);

    workspace_reply_to(&mut h, vec![member("r0"), member("r1")], first);
    assert!(
        h.root(|root, _| root.spawn_preview_rows()).is_empty(),
        "the reply to the request for the old base is dropped"
    );
    workspace_reply_to(&mut h, vec![member("r0"), member("r1")], second);
    assert_eq!(
        h.root(|root, _| root.spawn_preview_rows()).len(),
        2,
        "the reply to the latest request"
    );
}

#[gpui::test]
fn workspace_preview_button_disabled_without_branch(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    open_workspace(&mut h);
    assert!(!h.root(|root, _| root.spawn_preview_enabled()));
    h.click_on("spawn-preview");
    assert!(
        workspace_previews(&h.sent()).is_empty(),
        "a disabled button"
    );
    assert!(settle(&mut h).iter().all(|msg| !matches!(
        msg,
        ClientMessage::PreviewSpawn { .. } | ClientMessage::PreviewWorkspaceSpawn { .. }
    )));
    preview_workspace(&mut h);
}

#[gpui::test]
fn workspace_preview_table_rows(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    open_workspace(&mut h);
    preview_workspace(&mut h);
    let reused = MemberSpawnPreview {
        branch_exists: true,
        effective_base: None,
        ..member("r0")
    };
    let fresh = MemberSpawnPreview {
        base_behind_remote: Some(2),
        ..member("r1")
    };
    workspace_reply(&mut h, vec![reused, fresh]);
    assert_eq!(
        h.root(|root, _| root.spawn_preview_rows()),
        [
            ["R0", "b", "reuse", "C:/wt/r0"].map(str::to_owned).to_vec(),
            [
                "R1",
                "b",
                "new from main · base 2 behind origin/main",
                "C:/wt/r1"
            ]
            .map(str::to_owned)
            .to_vec(),
        ]
    );
    assert!(drawn(&mut h, "spawn-preview-table"));
    assert!(drawn(&mut h, "spawn-preview-row-r1"));

    h.click_on("spawn-base-branch");
    h.keys("x");
    assert!(
        h.root(|root, _| root.spawn_preview_rows()).is_empty(),
        "a change clears the table"
    );
}

#[gpui::test]
fn workspace_collision_choice_sent(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    open_workspace(&mut h);
    preview_workspace(&mut h);
    let collides = MemberSpawnPreview {
        worktree_exists: true,
        ..member("r1")
    };
    workspace_reply(&mut h, vec![member("r0"), collides]);
    assert_eq!(
        collision(&mut h).map(|lines| lines[0].clone()).as_deref(),
        Some("A worktree already exists at this path.")
    );
    h.click_on("spawn-collision-recreate");
    let request = the_spawn(&mut h);
    assert!(matches!(request.target, SpawnTarget::Workspace { .. }));
    assert_eq!(reuse_of(&request), WorktreeReusePolicy::RecreateFromBase);
}
