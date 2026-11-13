//! Spawn dialog branch list specs: typing filters the known branches and
//! offers a "Create branch" row, the keys and a click commit a row, Esc
//! closes the list before the dialog, Enter with the list closed submits,
//! the checked-out branch is marked, and the base field offers no create
//! row. The Target list takes a typed query that narrows it, scrolls to its
//! highlight as the branch list does, and stays open for a click inside it.

#![expect(
    clippy::panic,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use gpui::{Modifiers, ScrollDelta, ScrollWheelEvent, TestAppContext, TouchPhase, point, px};
use protocol::{ClientMessage, DaemonMessage, SpawnRequest, SpawnTarget};
use rustling_tulip_native::RootView;
use rustling_tulip_native::target::Target;
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
    assert_eq!(spawns(&h.sent()), [] as [protocol::SpawnRequest; 0]);
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
    assert_eq!(spawns(&h.sent()), [] as [protocol::SpawnRequest; 0]);
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
    assert_eq!(spawns(&h.sent()), [] as [protocol::SpawnRequest; 0]);
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

fn target_list_open(h: &mut Harness<'_>) -> bool {
    h.root(|root, _| root.spawn_target_list_open())
}

fn target_rows(h: &mut Harness<'_>) -> Option<Vec<String>> {
    h.root(|root, _| root.spawn_target_rows())
}

fn target_query(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.spawn_target_query())
}

fn target_highlight(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.spawn_target_highlight())
}

fn focus(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.spawn_dialog_focus())
}

fn selected(h: &mut Harness<'_>) -> Vec<String> {
    h.root(|root, _| root.spawn_dialog_selected())
}

/// Whether the element tagged `inner` is drawn whole inside the one tagged
/// `outer`, top to bottom.
fn shows_inside(h: &mut Harness<'_>, outer: &str, inner: &str) -> bool {
    let (outer, inner) = (h.bounds(outer), h.bounds(inner));
    inner.origin.x >= px(0.0) && inner.top() >= outer.top() && inner.bottom() <= outer.bottom()
}

/// [`fixture`] with workspace `yaat` of `r0` and `r1` beside the repos.
fn with_yaat() -> Fixture {
    let mut fixture = fixture();
    fixture.workspaces = vec![support::workspace("yaat", &["r0", "r1"])];
    fixture
}

/// Thirty repos `r00` to `r29`, more than the Target list shows at once;
/// session `s1` of `r00`.
fn many_repos() -> Fixture {
    let mut fixture = Fixture::single(session("s1").in_repo("r00").build());
    fixture.repos = (0..30)
        .map(|n| {
            let id = format!("r{n:02}");
            repo(&id, &format!("C:/{id}"))
        })
        .collect();
    fixture
}

#[gpui::test]
fn typing_narrows_target_list(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &with_yaat());
    h.click_on("sidebar-add-session");
    h.click_on("spawn-target");
    assert!(target_list_open(&mut h));
    assert!(
        drawn(&mut h, "spawn-target-query"),
        "the query field is drawn"
    );

    h.keys("y a");
    assert_eq!(target_query(&mut h).as_deref(), Some("ya"));
    assert_eq!(
        target_rows(&mut h),
        Some(vec!["spawn-target-workspace-yaat".to_owned()]),
        "the list narrows to the workspace"
    );
    assert_eq!(
        target_highlight(&mut h).as_deref(),
        Some("spawn-target-workspace-yaat")
    );

    h.keys("enter");
    assert!(!target_list_open(&mut h), "Enter picked the match");
    assert!(
        selected(&mut h).contains(&"spawn-target-workspace-yaat".to_owned()),
        "{:?}",
        selected(&mut h)
    );
    assert!(is_open(&mut h), "and kept the dialog");

    h.click_on("spawn-target");
    assert_eq!(target_query(&mut h).as_deref(), Some(""), "reopened empty");
    h.keys("z z z");
    assert_eq!(target_rows(&mut h), Some(Vec::new()));
    assert!(
        drawn(
            &mut h,
            "spawn-target-no-match=No matching repo or workspace"
        ),
        "no match says so"
    );
    h.keys("escape");
    assert_eq!(target_query(&mut h).as_deref(), Some(""), "Esc clears");
    assert!(target_list_open(&mut h), "and keeps the list");
    h.keys("escape");
    assert!(!target_list_open(&mut h), "the next Esc closes it");
    assert!(is_open(&mut h));
}

#[gpui::test]
fn second_escape_after_the_target_list_closes_the_dialog(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &with_yaat());
    h.click_on("sidebar-add-session");
    h.click_on("spawn-target");
    assert!(target_list_open(&mut h), "the list opened");

    h.keys("escape");
    assert!(!target_list_open(&mut h), "the first Esc closes the list");
    assert!(is_open(&mut h), "and keeps the dialog");
    assert_eq!(
        focus(&mut h).as_deref(),
        Some("spawn-target"),
        "the keyboard returns to the Target field"
    );

    h.keys("escape");
    assert!(
        !is_open(&mut h),
        "the second Esc closes the dialog, as with a list never opened"
    );
}

#[gpui::test]
fn typing_on_closed_target_field_opens_list_with_query(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &with_yaat());
    h.click_on("sidebar-add-session");
    h.click_on("spawn-target");
    h.click_on("spawn-target");
    assert!(
        !target_list_open(&mut h),
        "the field has the keyboard, closed"
    );

    h.keys("y");
    assert!(target_list_open(&mut h), "a letter opens the list");
    assert_eq!(target_query(&mut h).as_deref(), Some("y"));
    h.keys("a");
    assert_eq!(
        target_query(&mut h).as_deref(),
        Some("ya"),
        "the query field took the keyboard"
    );
    assert_eq!(
        target_rows(&mut h),
        Some(vec!["spawn-target-workspace-yaat".to_owned()])
    );
}

#[gpui::test]
fn target_list_scrolls_to_highlight(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &many_repos());
    h.click_on("sidebar-add-session");
    h.click_on("spawn-target");
    assert_eq!(
        target_highlight(&mut h).as_deref(),
        Some("spawn-target-repo-r00")
    );
    for _ in 0..25 {
        h.keys("down");
    }
    assert_eq!(
        target_highlight(&mut h).as_deref(),
        Some("spawn-target-repo-r25")
    );
    assert!(
        shows_inside(&mut h, "spawn-target-rows", "spawn-target-repo-r25"),
        "Down scrolled the highlight into view: {:?} in {:?}",
        h.bounds("spawn-target-repo-r25"),
        h.bounds("spawn-target-rows")
    );

    h.keys("r 0");
    assert_eq!(
        target_highlight(&mut h).as_deref(),
        Some("spawn-target-repo-r00"),
        "the best match"
    );
    assert!(
        shows_inside(&mut h, "spawn-target-rows", "spawn-target-repo-r00"),
        "an edit scrolled back to it: {:?} in {:?}",
        h.bounds("spawn-target-repo-r00"),
        h.bounds("spawn-target-rows")
    );
}

/// The tailnet host the two-host specs connect beside This PC.
const REMOTE: &str = "box.tail1234.ts.net";

/// A window on This PC holding `r0` and `r1`, with the tailnet host
/// connected beside it holding `r2`, so the Target list draws a header per
/// host.
fn two_hosts<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = Harness::with(cx, dir, &fixture());
    let mut host = h.host(&Target::tailnet(REMOTE, None));
    host.connect();
    host.send(DaemonMessage::Agents {
        agents: agent_registry::descriptors(),
    });
    host.send(DaemonMessage::Sessions {
        sessions: Vec::new(),
    });
    host.send(DaemonMessage::Repos {
        repos: vec![repo("r2", "D:/r2")],
    });
    host.sent();
    h.sent();
    h
}

#[gpui::test]
fn click_on_target_list_header_keeps_it_open(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir);
    h.click_on("sidebar-add-session");
    h.click_on("spawn-target");
    assert!(target_list_open(&mut h));
    assert!(drawn(&mut h, "spawn-target-host-local"), "grouped by host");

    h.click_on("spawn-target-host-local");
    assert!(
        target_list_open(&mut h),
        "a click on a header keeps it open"
    );
    h.click_on("spawn-target-query");
    assert!(target_list_open(&mut h), "a click on the query field too");
    let list = h.bounds("spawn-target-list");
    h.click(
        point(list.origin.x + px(2.0), list.bottom() - px(2.0)),
        Modifiers::none(),
    );
    assert!(target_list_open(&mut h), "and one on its padding");
    h.keys("r 2");
    assert_eq!(
        target_rows(&mut h),
        Some(vec![format!("spawn-target-repo-r2@{REMOTE}")]),
        "typing still reaches the query"
    );
}

#[gpui::test]
fn click_outside_target_list_closes_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &with_yaat());
    h.click_on("sidebar-add-session");
    h.click_on("spawn-target");
    h.keys("y");
    assert!(target_list_open(&mut h));

    let panel = h.bounds("spawn-panel");
    h.click(
        point(panel.origin.x + px(6.0), panel.origin.y + px(6.0)),
        Modifiers::none(),
    );
    assert!(!target_list_open(&mut h), "a click outside closes the list");
    assert!(is_open(&mut h), "and keeps the dialog");
    h.click_on("spawn-target");
    assert_eq!(
        target_query(&mut h).as_deref(),
        Some(""),
        "closing dropped the query"
    );
}

#[gpui::test]
fn click_outside_target_list_does_not_scroll_dialog(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &with_yaat());
    // A short window, so the dialog's body scrolls.
    h.cx.simulate_resize(gpui::size(px(1000.0), px(420.0)));
    h.click_on("sidebar-add-session");
    // The dialog opens on the branch field, the body scrolled to it.
    for _ in 0..12 {
        if h.root(|root, _| root.spawn_dialog_focus()).as_deref() == Some("spawn-target") {
            break;
        }
        h.keys("shift-tab");
    }
    h.keys("down");
    assert!(
        target_list_open(&mut h),
        "Down on the field opened the list"
    );
    let body = h.bounds("spawn-body");
    // In the body's left padding, beside the list hanging under the field.
    h.cx.simulate_event(ScrollWheelEvent {
        position: point(body.left() + px(6.0), body.bottom() - px(10.0)),
        delta: ScrollDelta::Pixels(point(px(0.0), px(-60.0))),
        modifiers: Modifiers::none(),
        touch_phase: TouchPhase::Moved,
        source: gpui::ScrollSource::Pointer,
    });
    h.cx.run_until_parked();
    let field = h.bounds("spawn-target");
    assert!(
        field.top() < body.top(),
        "the wheel left the field partly out of view: {field:?} in {body:?}"
    );

    let panel = h.bounds("spawn-panel");
    h.click(
        point(panel.origin.x + px(6.0), panel.origin.y + px(6.0)),
        Modifiers::none(),
    );
    assert!(!target_list_open(&mut h), "the press closed the list");
    assert_eq!(
        h.bounds("spawn-target").origin.y,
        field.origin.y,
        "and left the body where it was"
    );
}

#[gpui::test]
fn altgr_character_opens_target_list_with_query(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &with_yaat());
    h.click_on("sidebar-add-session");
    h.click_on("spawn-target");
    h.click_on("spawn-target");
    assert!(
        !target_list_open(&mut h),
        "the field has the keyboard, closed"
    );

    h.keys("alt-q->@");
    assert!(!target_list_open(&mut h), "plain Alt types nothing");
    h.keys("ctrl-q->@");
    assert!(!target_list_open(&mut h), "plain Ctrl types nothing");
    h.keys("ctrl-alt-q->@");
    assert!(target_list_open(&mut h), "AltGr types");
    assert_eq!(target_query(&mut h).as_deref(), Some("@"));
}

#[gpui::test]
fn branch_list_scrolls_to_highlight(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    h.click_on("sidebar-add-session");
    h.send(DaemonMessage::Branches {
        repo_id: "r1".to_owned(),
        branches: (0..30).map(|n| format!("b{n:02}")).collect(),
        current: Some("b00".to_owned()),
        remote_branches: Vec::new(),
    });
    h.sent();
    h.click_on("spawn-branch");
    assert_eq!(rows(&mut h).map(|rows| rows.len()), Some(30));
    for _ in 0..20 {
        h.keys("down");
    }
    assert!(
        shows_inside(&mut h, "spawn-branch-list", "spawn-branch-option-20"),
        "Down scrolled the highlight into view: {:?} in {:?}",
        h.bounds("spawn-branch-option-20"),
        h.bounds("spawn-branch-list")
    );
}
