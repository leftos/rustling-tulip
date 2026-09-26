//! Diff tab specs: opening a diff from the History and the changes tree,
//! activating the tab the daemon names in either arrival order, the header,
//! the change buttons and F7, the whitespace toggle, the texts standing in
//! for a diff, the live refresh and a refused open.

#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "a spec fails with the message of the precondition it lost"
)]

#[expect(dead_code, reason = "each spec file uses its own share of the helper")]
mod support;

use gpui::{Entity, TestAppContext};
use protocol::{
    ClientMessage, DaemonMessage, GitCommit, GitCommitDetail, GitFileChange, SnapshotUnavailable,
    TabEntry,
};
use rustling_tulip_native::diff_view::DiffView;
use rustling_tulip_native::{
    BINARY_TEXT, DIFF_OPEN_FAILED_TITLE, DiffTabBody, DiffTabHeader, EMPTY_TEXT, ToastKind,
};
use serde_json::{Value, json};
use support::{Fixture, Harness, TestDir, repo, session};

/// The focused session's member tree of `r1`, and its section's key id.
const TREE: &str = "C:/wt/r1";
const MEMBER: &str = "r1::C:/wt/r1";
const SHA: &str = "0a00001000000000000000000000000000000000";

/// `r1` registered and a focused session `s1` on a worktree of it, in tab
/// `t1`.
fn focused() -> Fixture {
    let s1 = session("s1").members(&[("r1", "feat/x", TREE)]).build();
    let mut fixture = Fixture::single(s1);
    fixture.repos = vec![repo("r1", "D:/src/r1")];
    fixture
}

fn diff_tab(id: &str, path: &str, against: Option<&str>) -> TabEntry {
    diff_tab_in(id, path, against, Some(TREE))
}

/// A diff tab of `path` in the tree `worktree`; `None` is the main tree.
fn diff_tab_in(id: &str, path: &str, against: Option<&str>, worktree: Option<&str>) -> TabEntry {
    serde_json::from_value(json!({
        "id": id,
        "name": path,
        "content": {
            "kind": "diff",
            "repo_id": "r1",
            "path": path,
            "against": against,
            "worktree_path": worktree,
        },
        "created_at": "2026-01-01T00:00:00Z",
    }))
    .expect("diff tab fixture")
}

fn updated(tab: &TabEntry) -> DaemonMessage {
    DaemonMessage::TabUpdated { tab: tab.clone() }
}

fn opened(id: &str, tab_id: &str) -> DaemonMessage {
    DaemonMessage::DiffTabOpened {
        id: id.to_owned(),
        tab_id: tab_id.to_owned(),
    }
}

/// An `OpenDiffTab` the client sent: (id, path, against, worktree).
type Open = (String, String, Option<String>, Option<String>);

fn opens(h: &mut Harness<'_>) -> Vec<Open> {
    h.sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::OpenDiffTab {
                id,
                repo_id,
                path,
                against,
                worktree_path,
            } => {
                assert_eq!(repo_id, "r1");
                Some((id, path, against, worktree_path))
            }
            _ => None,
        })
        .collect()
}

fn one_open(h: &mut Harness<'_>) -> Open {
    let mut sent = opens(h);
    assert_eq!(sent.len(), 1, "one open: {sent:?}");
    sent.remove(0)
}

/// The `GetFileSnapshot`s the client sent: (id, path, against, worktree).
fn snapshot_requests(h: &mut Harness<'_>) -> Vec<Open> {
    h.sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::GetFileSnapshot {
                id,
                repo_id,
                path,
                against,
                worktree_path,
            } => {
                assert_eq!(repo_id, "r1");
                Some((id, path, against, worktree_path))
            }
            _ => None,
        })
        .collect()
}

fn one_snapshot_id(h: &mut Harness<'_>) -> String {
    let mut sent = snapshot_requests(h);
    assert_eq!(sent.len(), 1, "one snapshot request: {sent:?}");
    sent.remove(0).0
}

fn snapshot(
    id: &str,
    old: &str,
    new: &str,
    unavailable: Option<SnapshotUnavailable>,
) -> DaemonMessage {
    DaemonMessage::FileSnapshot {
        id: id.to_owned(),
        repo_id: "r1".to_owned(),
        path: "src/main.rs".to_owned(),
        against: None,
        old: old.to_owned(),
        new: new.to_owned(),
        language: "rust".to_owned(),
        unavailable,
        worktree_path: Some(TREE.to_owned()),
    }
}

fn answer(h: &mut Harness<'_>, id: &str, old: &str, new: &str) {
    h.send(snapshot(id, old, new, None));
}

/// `count` numbered lines, with the lines in `changed` (1-based) edited.
fn lines(count: usize, changed: &[usize]) -> String {
    (1..=count)
        .map(|n| {
            if changed.contains(&n) {
                format!("line {n} edited\n")
            } else {
                format!("line {n}\n")
            }
        })
        .collect()
}

fn header(h: &mut Harness<'_>, tab: &str) -> DiffTabHeader {
    h.root(|root, cx| root.diff_tab_header(tab, cx))
        .expect("the diff tab has a view")
}

fn body(h: &mut Harness<'_>, tab: &str) -> DiffTabBody {
    h.root(|root, cx| root.diff_tab_body(tab, cx))
        .expect("the diff tab has a view")
}

fn text(h: &mut Harness<'_>, tab: &str) -> String {
    match body(h, tab) {
        DiffTabBody::Text { text, .. } => text,
        DiffTabBody::Diff => panic!("the diff shows, not a text"),
    }
}

fn diff_view(h: &mut Harness<'_>, tab: &str) -> Entity<DiffView> {
    h.root(|root, cx| root.diff_tab_view(tab, cx))
        .expect("the diff is built")
}

fn current_hunk(h: &mut Harness<'_>, tab: &str) -> Option<usize> {
    let view = diff_view(h, tab);
    h.cx.update(|_, cx| view.read(cx).current_hunk())
}

fn active(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.active_tab_id().map(str::to_owned))
}

/// The tab delivered by the daemon and shown by a tab-bar click; returns
/// its snapshot request's id.
fn show(h: &mut Harness<'_>, tab: &TabEntry) -> String {
    h.send(updated(tab));
    let id = one_snapshot_id(h);
    h.click_on(&format!("tab-{}", tab.id));
    assert_eq!(active(h).as_deref(), Some(tab.id.as_str()));
    id
}

// --- The History ---------------------------------------------------------

fn commit() -> GitCommit {
    GitCommit {
        sha: SHA.to_owned(),
        short_sha: "0a00001".to_owned(),
        author_name: "Ada Lovelace".to_owned(),
        author_email: "ada@example.com".to_owned(),
        authored_at: "2026-09-26T14:03:12+02:00".to_owned(),
        subject: "change 1".to_owned(),
    }
}

/// Clicks `src/main.rs` in the History's commit detail; returns the
/// `OpenDiffTab` it sent.
fn open_from_history(h: &mut Harness<'_>) -> Open {
    h.click_on("activity-source-control");
    h.click_on(&format!("sc-history-{MEMBER}"));
    h.sent();
    h.send(DaemonMessage::Commits {
        repo_id: "r1".to_owned(),
        commits: vec![commit()],
        offset: 0,
        worktree_path: Some(TREE.to_owned()),
    });
    h.click_on(&format!("sc-commit-{MEMBER}-{SHA}"));
    h.send(DaemonMessage::CommitDetail {
        repo_id: "r1".to_owned(),
        detail: GitCommitDetail {
            commit: commit(),
            body: String::new(),
            parent_shas: vec!["parent0".to_owned()],
            changes: vec![GitFileChange {
                path: "src/main.rs".to_owned(),
                status: "M".to_owned(),
                from_path: None,
            }],
        },
    });
    h.sent();
    h.click_on(&format!("sc-detail-file-{MEMBER}-0"));
    let open = one_open(h);
    assert_eq!(open.2.as_deref(), Some(SHA));
    open
}

#[gpui::test]
fn a_history_open_answered_before_its_tab_arrives_activates_the_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &focused());
    let (id, ..) = open_from_history(&mut h);
    assert_eq!(id, "diff-open-1");

    h.send(opened(&id, "d1"));
    assert_eq!(
        active(&mut h).as_deref(),
        Some("t1"),
        "d1 is not listed yet"
    );
    h.send(updated(&diff_tab("d1", "src/main.rs", Some(SHA))));
    assert_eq!(active(&mut h).as_deref(), Some("d1"));
}

#[gpui::test]
fn a_history_open_answered_after_its_tab_arrives_activates_the_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &focused());
    let (id, ..) = open_from_history(&mut h);

    h.send(updated(&diff_tab("d1", "src/main.rs", Some(SHA))));
    assert_eq!(
        active(&mut h).as_deref(),
        Some("t1"),
        "not active by itself"
    );
    h.send(opened("another-client-1", "d1"));
    assert_eq!(
        active(&mut h).as_deref(),
        Some("t1"),
        "another client's open"
    );
    h.send(opened(&id, "d1"));
    assert_eq!(active(&mut h).as_deref(), Some("d1"));
}

// --- The changes tree ----------------------------------------------------

fn file(path: &str) -> Value {
    json!({ "path": path, "status": "M", "from_path": null })
}

/// The member tree's status: `staged` in the index, `changed` in the
/// worktree.
fn status(staged: &[&str], changed: &[&str], worktree: Option<&str>) -> DaemonMessage {
    serde_json::from_value(json!({
        "type": "repo_status",
        "repo_id": "r1",
        "index_changes": staged.iter().map(|p| file(p)).collect::<Vec<_>>(),
        "worktree_changes": changed.iter().map(|p| file(p)).collect::<Vec<_>>(),
        "worktree_path": worktree,
    }))
    .expect("status fixture")
}

/// The panel on the member tree, with `s.rs` staged and `c.rs` changed.
fn changes_panel<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = Harness::with(cx, dir, &focused());
    h.click_on("activity-source-control");
    h.send(status(&["s.rs"], &["c.rs"], Some(TREE)));
    h.sent();
    h
}

#[gpui::test]
fn clicking_a_changes_row_opens_its_worktree_diff(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = changes_panel(cx, &dir);
    h.click_on(&format!("sc-row-changes-{MEMBER}|c.rs"));
    let (_, path, against, worktree) = one_open(&mut h);
    assert_eq!(path, "c.rs");
    assert_eq!(against, None);
    assert_eq!(worktree.as_deref(), Some(TREE));
}

#[gpui::test]
fn clicking_a_staged_row_opens_its_diff_against_head(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = changes_panel(cx, &dir);
    h.click_on(&format!("sc-row-staged-{MEMBER}|s.rs"));
    let (_, path, against, worktree) = one_open(&mut h);
    assert_eq!(path, "s.rs");
    assert_eq!(against.as_deref(), Some("HEAD"));
    assert_eq!(worktree.as_deref(), Some(TREE));
}

fn menu(h: &mut Harness<'_>) -> Vec<(&'static str, bool, bool)> {
    h.root(|root, _| root.sc_file_menu_items())
        .expect("the menu is open")
        .iter()
        .map(|item| (item.label, item.enabled, item.separated))
        .collect()
}

#[gpui::test]
fn the_file_menu_opens_the_diff_first(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = changes_panel(cx, &dir);
    h.right_click_on(&format!("sc-row-changes-{MEMBER}|c.rs"));
    assert_eq!(
        menu(&mut h),
        [
            ("Open Changes", true, false),
            ("Stage Changes", true, true),
            ("Discard Changes", true, true),
        ]
    );
    h.click_on("sc-file-menu-open");
    assert!(!h.root(|root, _| root.sc_file_menu_open()));
    let (_, path, against, _) = one_open(&mut h);
    assert_eq!((path.as_str(), against), ("c.rs", None));

    h.right_click_on(&format!("sc-row-staged-{MEMBER}|s.rs"));
    assert_eq!(
        menu(&mut h),
        [
            ("Open Staged Changes", true, false),
            ("Unstage Changes", true, true),
        ]
    );
    h.click_on("sc-file-menu-unstage");
    h.sent();
    h.right_click_on(&format!("sc-row-staged-{MEMBER}|s.rs"));
    assert_eq!(
        menu(&mut h),
        [
            ("Open Staged Changes", true, false),
            ("Unstage Changes", false, true),
        ],
        "a write out disables the writes, not Open"
    );
    h.click_on("sc-file-menu-open");
    let (_, path, against, _) = one_open(&mut h);
    assert_eq!((path.as_str(), against.as_deref()), ("s.rs", Some("HEAD")));
}

/// `c.rs`'s row in the main tree's Changes.
const MAIN_ROW: &str = "sc-row-changes-r1::|c.rs";

/// The panel on `r1`'s main tree, with `c.rs` changed. No session is
/// focused, so the panel keeps the main tree whichever tab is active.
fn main_tree_panel<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let fixture = Fixture {
        repos: vec![repo("r1", "D:/src/r1")],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, dir, &fixture);
    h.click_on("activity-source-control");
    h.send(status(&[], &["c.rs"], None));
    h.sent();
    h
}

#[gpui::test]
fn a_diff_tab_activated_under_the_discard_confirm_leaves_it_the_keyboard(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = main_tree_panel(cx, &dir);
    h.click_on(MAIN_ROW);
    let (open_id, ..) = one_open(&mut h);
    h.send(opened(&open_id, "d1"));
    h.click_on("sc-discard-all-r1::");
    assert!(h.root(|root, _| root.discard_confirm_view()).is_some());

    h.send(updated(&diff_tab_in("d1", "c.rs", None, None)));
    assert_eq!(
        active(&mut h).as_deref(),
        Some("d1"),
        "the pending open completes"
    );
    assert!(h.root(|root, _| root.discard_confirm_view()).is_some());
    let focused = h.root.clone();
    assert!(
        !h.cx
            .update(|window, cx| focused.read(cx).diff_tab_focused("d1", window, cx)),
        "the confirm keeps the keyboard"
    );
    h.keys("escape");
    assert!(
        h.root(|root, _| root.discard_confirm_view()).is_none(),
        "Esc reaches the confirm"
    );
}

#[gpui::test]
fn reopening_the_active_diff_tab_gives_it_the_keyboard(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = main_tree_panel(cx, &dir);
    h.click_on(MAIN_ROW);
    let (open_id, ..) = one_open(&mut h);
    h.send(updated(&diff_tab_in("d1", "c.rs", None, None)));
    let snap = one_snapshot_id(&mut h);
    h.send(opened(&open_id, "d1"));
    assert_eq!(active(&mut h).as_deref(), Some("d1"));
    answer(&mut h, &snap, &lines(60, &[]), &lines(60, &[5, 45]));

    h.click_on(MAIN_ROW);
    let (open_id, ..) = one_open(&mut h);
    let focused = h.root.clone();
    assert!(
        !h.cx
            .update(|window, cx| focused.read(cx).diff_tab_focused("d1", window, cx)),
        "the click took the keyboard to the sidebar"
    );
    h.send(opened(&open_id, "d1"));
    assert_eq!(active(&mut h).as_deref(), Some("d1"));
    h.keys("f7");
    assert_eq!(current_hunk(&mut h, "d1"), Some(0), "F7 reaches the diff");
}

// --- The tab -------------------------------------------------------------

#[gpui::test]
fn a_snapshot_fills_the_header_and_the_count(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &focused());
    h.send(updated(&diff_tab("d1", "src/deep/main.rs", Some(SHA))));
    let sent = snapshot_requests(&mut h);
    assert_eq!(
        sent,
        [(
            "diff-snap-1".to_owned(),
            "src/deep/main.rs".to_owned(),
            Some(SHA.to_owned()),
            Some(TREE.to_owned())
        )]
    );
    h.click_on("tab-d1");
    assert_eq!(
        header(&mut h, "d1"),
        DiffTabHeader {
            path: "src/deep/main.rs".to_owned(),
            mode: "@ 0a00001".to_owned(),
            mode_tip: Some(SHA.to_owned()),
            include_whitespace: true,
            count: "…".to_owned(),
            nav_enabled: false,
        }
    );
    assert_eq!(text(&mut h, "d1"), "loading…");

    answer(&mut h, "diff-snap-1", &lines(40, &[]), &lines(40, &[5, 30]));
    let shown = header(&mut h, "d1");
    assert_eq!(shown.count, "2 changes");
    assert!(shown.nav_enabled);
    assert_eq!(body(&mut h, "d1"), DiffTabBody::Diff);

    h.send(updated(&diff_tab("d2", "a.rs", None)));
    h.send(updated(&diff_tab("d3", "b.rs", Some("HEAD"))));
    let ids: Vec<String> = snapshot_requests(&mut h).into_iter().map(|s| s.0).collect();
    assert_eq!(ids, ["diff-snap-2", "diff-snap-3"]);
    answer(&mut h, "diff-snap-2", "a\n", "b\n");
    assert_eq!(header(&mut h, "d2").mode, "worktree vs index");
    assert_eq!(header(&mut h, "d2").count, "1 change");
    assert_eq!(header(&mut h, "d3").mode, "index vs HEAD");
}

#[gpui::test]
fn the_change_buttons_move_the_current_change_and_wrap(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &focused());
    let id = show(&mut h, &diff_tab("d1", "src/main.rs", None));
    answer(&mut h, &id, &lines(60, &[]), &lines(60, &[5, 25, 45]));
    assert_eq!(current_hunk(&mut h, "d1"), None);

    let mut seen = Vec::new();
    for button in [
        "diff-next",
        "diff-next",
        "diff-next",
        "diff-next",
        "diff-prev",
        "diff-first",
        "diff-last",
    ] {
        h.click_on(button);
        seen.push(current_hunk(&mut h, "d1"));
    }
    assert_eq!(
        seen,
        [
            Some(0),
            Some(1),
            Some(2),
            Some(0),
            Some(2),
            Some(0),
            Some(2)
        ]
    );
}

#[gpui::test]
fn f7_moves_through_the_changes_as_soon_as_the_tab_opens(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = changes_panel(cx, &dir);
    h.click_on(&format!("sc-row-changes-{MEMBER}|c.rs"));
    let (open_id, ..) = one_open(&mut h);
    h.send(updated(&diff_tab("d1", "c.rs", None)));
    let snap = one_snapshot_id(&mut h);
    h.send(opened(&open_id, "d1"));
    assert_eq!(active(&mut h).as_deref(), Some("d1"));
    let focused = h.root.clone();
    assert!(
        h.cx.update(|window, cx| focused.read(cx).diff_tab_focused("d1", window, cx)),
        "the tab has the keyboard"
    );

    answer(&mut h, &snap, &lines(60, &[]), &lines(60, &[5, 45]));
    h.keys("f7");
    assert_eq!(current_hunk(&mut h, "d1"), Some(0));
    h.keys("f7");
    assert_eq!(current_hunk(&mut h, "d1"), Some(1));
    h.keys("shift-f7");
    assert_eq!(current_hunk(&mut h, "d1"), Some(0));
    h.keys("shift-f7");
    assert_eq!(current_hunk(&mut h, "d1"), Some(1), "wraps");
}

fn saved_ui(dir: &TestDir) -> Value {
    serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("native-ui.json")).expect("native-ui.json"),
    )
    .expect("native-ui.json is JSON")
}

#[gpui::test]
fn the_whitespace_toggle_is_saved_and_rebuilds_every_diff(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &focused());
    let id = show(&mut h, &diff_tab("d1", "src/main.rs", None));
    h.send(updated(&diff_tab("d2", "other.rs", None)));
    let other = one_snapshot_id(&mut h);
    answer(&mut h, &id, "a\n  b\nc\n", "a\nb\nc\n");
    answer(&mut h, &other, "x\n", "x \n");
    assert_eq!(header(&mut h, "d1").count, "1 change");
    assert_eq!(header(&mut h, "d2").count, "1 change");

    h.click_on("diff-whitespace");
    let shown = header(&mut h, "d1");
    assert!(!shown.include_whitespace);
    assert_eq!(shown.count, "no changes");
    assert!(!shown.nav_enabled);
    assert_eq!(
        body(&mut h, "d1"),
        DiffTabBody::Text {
            text: EMPTY_TEXT.to_owned(),
            note: None
        }
    );
    assert_eq!(header(&mut h, "d2").count, "no changes", "every tab");
    assert!(
        snapshot_requests(&mut h).is_empty(),
        "rebuilt from the texts held"
    );
    assert_eq!(saved_ui(&dir)["diff_include_whitespace"], json!(false));

    h.click_on("diff-whitespace");
    assert_eq!(header(&mut h, "d1").count, "1 change");
    assert_eq!(saved_ui(&dir)["diff_include_whitespace"], json!(true));
}

#[gpui::test]
fn a_binary_or_too_large_file_is_not_shown(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &focused());
    let id = show(&mut h, &diff_tab("d1", "logo.png", None));
    h.send(snapshot(&id, "", "", Some(SnapshotUnavailable::Binary)));
    assert_eq!(text(&mut h, "d1"), BINARY_TEXT);
    assert_eq!(header(&mut h, "d1").count, "");

    let id = show(&mut h, &diff_tab("d2", "big.log", None));
    let too_large = SnapshotUnavailable::TooLarge {
        bytes: 5 * 1024 * 1024 + 300 * 1024,
        limit: 2 * 1024 * 1024,
    };
    h.send(snapshot(&id, "", "", Some(too_large)));
    assert_eq!(
        text(&mut h, "d2"),
        "File too large to diff (5.3 MiB, limit 2 MiB)"
    );
}

#[gpui::test]
fn a_failed_snapshot_says_why(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &focused());
    let id = show(&mut h, &diff_tab("d1", "src/main.rs", None));
    h.send(DaemonMessage::FileSnapshotError {
        id: "diff-snap-99".to_owned(),
        repo_id: "r1".to_owned(),
        path: "src/main.rs".to_owned(),
        against: None,
        error: "stale".to_owned(),
        worktree_path: Some(TREE.to_owned()),
    });
    assert_eq!(text(&mut h, "d1"), "loading…", "a stale id reaches no tab");
    h.send(DaemonMessage::FileSnapshotError {
        id,
        repo_id: "r1".to_owned(),
        path: "src/main.rs".to_owned(),
        against: None,
        error: "path not in HEAD".to_owned(),
        worktree_path: Some(TREE.to_owned()),
    });
    assert_eq!(text(&mut h, "d1"), "Could not load diff: path not in HEAD");
}

fn scroll_top(h: &mut Harness<'_>, view: &Entity<DiffView>) -> f32 {
    h.cx.update(|_, cx| view.read(cx).scroll_top())
}

#[gpui::test]
fn a_status_for_the_tree_refetches_and_keeps_the_scroll(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &focused());
    let id = show(&mut h, &diff_tab("d1", "src/main.rs", None));
    answer(&mut h, &id, &lines(200, &[]), &lines(200, &[100]));
    let view = diff_view(&mut h, "d1");
    h.cx.update(|_, cx| {
        view.update(cx, |view, cx| {
            view.scroll_handle()
                .scroll_to_item_strict(80, gpui::ScrollStrategy::Top);
            cx.notify();
        });
    });
    h.cx.run_until_parked();
    let top = scroll_top(&mut h, &view);
    assert!(top > 0.0, "scrolled down");

    h.send(status(&[], &["src/main.rs"], None));
    assert!(
        snapshot_requests(&mut h).is_empty(),
        "another tree's status"
    );
    h.send(status(&[], &["src/main.rs"], Some(TREE)));
    let again = one_snapshot_id(&mut h);
    assert_ne!(again, id);
    assert_eq!(body(&mut h, "d1"), DiffTabBody::Diff, "no loading state");
    answer(&mut h, &again, &lines(200, &[]), &lines(200, &[100, 150]));
    assert_eq!(header(&mut h, "d1").count, "2 changes");
    let rebuilt = diff_view(&mut h, "d1");
    assert_eq!(rebuilt.entity_id(), view.entity_id(), "the same view");
    assert!(
        (scroll_top(&mut h, &view) - top).abs() < 1.0,
        "the scroll stays"
    );
}

#[gpui::test]
fn a_commits_diff_does_not_refetch(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &focused());
    let id = show(&mut h, &diff_tab("d1", "src/main.rs", Some(SHA)));
    answer(&mut h, &id, "a\n", "b\n");
    h.send(status(&[], &["src/main.rs"], Some(TREE)));
    assert!(snapshot_requests(&mut h).is_empty());
    assert_eq!(header(&mut h, "d1").count, "1 change");
}

#[gpui::test]
fn a_refused_open_raises_a_toast(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = changes_panel(cx, &dir);
    h.click_on(&format!("sc-row-changes-{MEMBER}|c.rs"));
    let (id, ..) = one_open(&mut h);
    h.send(DaemonMessage::Error {
        message: "open diff tab failed: unknown repo: r1".to_owned(),
        request_id: Some(id.clone()),
    });
    let toasts: Vec<(ToastKind, String, Option<String>)> = h.root(|root, _| {
        root.toasts()
            .iter()
            .map(|toast| (toast.kind, toast.title.clone(), toast.detail.clone()))
            .collect()
    });
    assert_eq!(
        toasts,
        [(
            ToastKind::Error,
            DIFF_OPEN_FAILED_TITLE.to_owned(),
            Some("open diff tab failed: unknown repo: r1".to_owned())
        )]
    );
    h.send(opened(&id, "d1"));
    assert_eq!(active(&mut h).as_deref(), Some("t1"), "the id is forgotten");
}
