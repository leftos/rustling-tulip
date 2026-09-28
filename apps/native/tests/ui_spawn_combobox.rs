//! Spawn dialog branch list specs: typing filters the known branches and
//! offers a "Create branch" row, the keys and a click commit a row, Esc
//! closes the list before the dialog, Enter with the list closed submits,
//! the checked-out branch is marked, and the base field offers no create
//! row.

#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "a spec fails with the message of the precondition it lost"
)]

#[expect(dead_code, reason = "each spec file uses its own share of the helper")]
mod support;

use gpui::{TestAppContext, px};
use protocol::{ClientMessage, DaemonMessage, SpawnRequest, SpawnTarget};
use rustling_tulip_native::RootView;
use support::{Fixture, Harness, TestDir, repo, session};

/// Repos `r0` and `r1`; session `s1` of `r1` alone in pane `p1` of tab `t1`.
fn fixture() -> Fixture {
    let mut fixture = Fixture::single(session("s1").in_repo("r1").build());
    fixture.repos = vec![repo("r0", "C:/r0"), repo("r1", "C:/r1")];
    fixture
}

fn is_open(h: &mut Harness<'_>) -> bool {
    h.root(|root, _| root.spawn_dialog_open())
}

fn branch(h: &mut Harness<'_>) -> Option<String> {
    h.root(RootView::spawn_dialog_branch)
}

fn base(h: &mut Harness<'_>) -> Option<String> {
    h.root(RootView::spawn_dialog_base)
}

fn rows(h: &mut Harness<'_>) -> Option<Vec<String>> {
    h.root(|root, _| root.spawn_branch_rows())
}

fn base_rows(h: &mut Harness<'_>) -> Option<Vec<String>> {
    h.root(|root, _| root.spawn_base_rows())
}

/// Opens the dialog on `r1` with "+ Session" and answers its branch list:
/// `main`, `wt/red-fox` and `feature/Login`, `current` checked out.
fn open(h: &mut Harness<'_>, current: &str) {
    h.click_on("sidebar-add-session");
    assert!(is_open(h), "the dialog opened");
    h.send(DaemonMessage::Branches {
        repo_id: "r1".to_owned(),
        branches: ["main", "wt/red-fox", "feature/Login"]
            .map(str::to_owned)
            .to_vec(),
        current: Some(current.to_owned()),
        remote_branches: vec!["origin/main".to_owned()],
    });
    h.sent();
    assert_eq!(rows(h), None, "the list waits for a click or an edit");
}

fn spawns(sent: &[ClientMessage]) -> Vec<SpawnRequest> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::SpawnSession(request) => Some(request.clone()),
            _ => None,
        })
        .collect()
}

/// Whether the element tagged `selector` is on screen.
fn drawn(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).origin.x >= px(0.0)
}

#[gpui::test]
fn typing_filters_branch_list(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    open(&mut h, "main");
    h.keys("r e d");
    assert_eq!(branch(&mut h).as_deref(), Some("red"));
    assert_eq!(
        rows(&mut h),
        Some(vec![
            "wt/red-fox".to_owned(),
            "Create branch “red”".to_owned()
        ])
    );
    assert!(drawn(&mut h, "spawn-branch-list"), "the list is drawn");
    assert!(drawn(&mut h, "spawn-branch-option-1"));
}

#[gpui::test]
fn create_row_commits_typed_name(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    open(&mut h, "main");
    h.keys("l o g space");
    assert_eq!(branch(&mut h).as_deref(), Some("log "));
    assert_eq!(
        rows(&mut h),
        Some(vec![
            "feature/Login".to_owned(),
            "Create branch “log”".to_owned()
        ])
    );
    h.click_on("spawn-branch-option-1");
    assert_eq!(
        branch(&mut h).as_deref(),
        Some("log"),
        "the typed name, trimmed"
    );
    assert_eq!(rows(&mut h), None, "a commit closes the list");
    assert!(is_open(&mut h));
    assert!(spawns(&h.sent()).is_empty());
}

#[gpui::test]
fn down_enter_commits_highlighted_branch(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    open(&mut h, "main");
    h.keys("down");
    assert_eq!(
        rows(&mut h).map(|rows| rows.len()),
        Some(3),
        "Down opens the list on every branch"
    );
    h.keys("down enter");
    assert_eq!(branch(&mut h).as_deref(), Some("wt/red-fox"));
    assert_eq!(rows(&mut h), None);
    assert!(is_open(&mut h), "Enter committed the row, not the dialog");
    assert!(spawns(&h.sent()).is_empty());
}

#[gpui::test]
fn esc_closes_list_not_dialog(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    open(&mut h, "main");
    h.keys("f");
    assert!(rows(&mut h).is_some());
    h.keys("escape");
    assert_eq!(rows(&mut h), None, "the first Esc closes the list");
    assert!(is_open(&mut h), "and keeps the dialog");
    assert_eq!(branch(&mut h).as_deref(), Some("f"), "and what was typed");
    h.keys("escape");
    assert!(!is_open(&mut h), "the second Esc closes the dialog");
    assert!(spawns(&h.sent()).is_empty());
}

#[gpui::test]
fn enter_with_list_closed_submits(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    open(&mut h, "main");
    h.keys("x");
    assert!(rows(&mut h).is_some());
    h.keys("escape enter");
    assert!(!is_open(&mut h), "Enter submitted");
    let sent = spawns(&h.sent());
    let [request] = sent.as_slice() else {
        panic!("expected one SpawnSession, sent {sent:?}");
    };
    assert!(matches!(
        &request.target,
        SpawnTarget::Single { branch_name, .. } if branch_name == "x"
    ));
}

#[gpui::test]
fn first_enter_commits_row_second_submits(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    open(&mut h, "main");
    h.keys("q");
    assert_eq!(rows(&mut h), Some(vec!["Create branch “q”".to_owned()]));
    h.keys("enter");
    assert!(is_open(&mut h), "the first Enter commits the row");
    assert_eq!(rows(&mut h), None);
    h.keys("enter");
    assert!(!is_open(&mut h), "the second Enter submits");
    let sent = spawns(&h.sent());
    let [request] = sent.as_slice() else {
        panic!("expected one SpawnSession, sent {sent:?}");
    };
    assert!(matches!(
        &request.target,
        SpawnTarget::Single { branch_name, .. } if branch_name == "q"
    ));
}

#[gpui::test]
fn current_branch_is_marked(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    open(&mut h, "wt/red-fox");
    h.click_on("spawn-branch");
    assert_eq!(
        rows(&mut h),
        Some(vec![
            "main".to_owned(),
            "wt/red-fox".to_owned(),
            "feature/Login".to_owned()
        ]),
        "a click opens the list"
    );
    assert!(drawn(&mut h, "spawn-branch-option-1-current"));
    assert!(!drawn(&mut h, "spawn-branch-option-0-current"));
    assert!(!drawn(&mut h, "spawn-branch-option-2-current"));
}

#[gpui::test]
fn base_field_offers_no_create_row(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    open(&mut h, "main");
    assert_eq!(base(&mut h).as_deref(), Some("origin/main"));
    h.click_on("spawn-base-branch");
    assert_eq!(
        base_rows(&mut h),
        Some(vec![
            "origin/main".to_owned(),
            "main".to_owned(),
            "wt/red-fox".to_owned(),
            "feature/Login".to_owned()
        ]),
        "remote branches, then local ones"
    );
    h.keys("ctrl-a z z z");
    assert_eq!(base(&mut h).as_deref(), Some("zzz"));
    assert_eq!(base_rows(&mut h), None, "nothing matches: no list at all");
    h.keys("ctrl-a f o x");
    assert_eq!(base_rows(&mut h), Some(vec!["wt/red-fox".to_owned()]));
    h.keys("enter");
    assert_eq!(base(&mut h).as_deref(), Some("wt/red-fox"));
    assert!(is_open(&mut h));
}
