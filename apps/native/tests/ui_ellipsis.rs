//! Ellipsis specs: every one-line text that a view cuts short ends in `…`,
//! read back through the debug-build probe the ellipsis helpers keep.

#![cfg(debug_assertions)]
#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

#[expect(dead_code, reason = "each spec file uses its own share of the helper")]
mod support;

use chrono::Utc;
use gpui::{Modifiers, ScrollDelta, ScrollWheelEvent, TestAppContext, TouchPhase, point, px};
use protocol::{ClientMessage, DaemonMessage, GitCommit, GitCommitDetail, GitFileChange, TabEntry};
use rustling_tulip_native::ellipsis::{drawn_text, enable_probe};
use serde_json::{Value, json};
use support::{Fixture, Harness, TestDir, pane, repo, session, tab};

/// A spec's test dir, with the ellipsis probe switched on for its thread.
fn probed() -> TestDir {
    enable_probe();
    TestDir::new()
}

/// A text far wider than any box in the window, starting with `start`.
fn long(start: &str) -> String {
    format!("{start}{}", "-and-then-some-more".repeat(30))
}

/// Asserts the text drawn under `id` was cut short of `full` with a `…`,
/// keeping its start.
fn assert_ellipsized(id: &str, full: &str) {
    let drawn = drawn_text(id);
    let head: String = full.chars().take(6).collect();
    assert!(
        drawn.as_deref().is_some_and(|text| text.ends_with('…')
            && text.chars().count() < full.chars().count()
            && text.starts_with(&head)),
        "{id} should draw the start of {full:?} and end in an ellipsis, drew {drawn:?}"
    );
}

fn one_repo() -> Fixture {
    Fixture {
        repos: vec![repo("r1", "D:/src/r1")],
        ..Fixture::default()
    }
}

fn status(changes: &[&str]) -> DaemonMessage {
    let files: Vec<Value> = changes
        .iter()
        .map(|path| json!({ "path": path, "status": "M", "from_path": null }))
        .collect();
    serde_json::from_value(json!({
        "type": "repo_status",
        "repo_id": "r1",
        "index_changes": [],
        "worktree_changes": files,
        "worktree_path": null,
    }))
    .expect("status fixture")
}

/// r1's source-control panel, open with no session focused.
fn sc_panel<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = Harness::with(cx, dir, &one_repo());
    h.click_on("activity-source-control");
    h
}

// --- Changes -------------------------------------------------------------

#[gpui::test]
fn changes_view_bucket_head_is_drawn_through_the_ellipsis_helper(cx: &mut TestAppContext) {
    let dir = probed();
    let mut h = sc_panel(cx, &dir);
    h.send(status(&["a.rs"]));
    // "Changes" fits any panel width the sidebar allows, so this shows the
    // head draws through the helper; the section head spec shows it cut.
    assert_eq!(
        drawn_text("sc-bucket-changes-r1::-title").as_deref(),
        Some("CHANGES")
    );
}

#[gpui::test]
fn changes_view_folder_label_ends_in_an_ellipsis(cx: &mut TestAppContext) {
    let dir = probed();
    let mut h = sc_panel(cx, &dir);
    let folder = long("folder");
    h.send(status(&[
        &format!("{folder}/a.rs"),
        &format!("{folder}/b.rs"),
    ]));
    assert_ellipsized(&format!("sc-folder-changes-r1::|{folder}-label"), &folder);
}

#[gpui::test]
fn changes_view_file_name_ends_in_an_ellipsis(cx: &mut TestAppContext) {
    let dir = probed();
    let mut h = sc_panel(cx, &dir);
    let name = format!("{}.rs", long("file"));
    h.send(status(&[&name]));
    assert_ellipsized(&format!("sc-row-changes-r1::|{name}-name"), &name);
}

#[gpui::test]
fn source_control_view_section_head_repo_ends_in_an_ellipsis(cx: &mut TestAppContext) {
    let dir = probed();
    let id = long("repo");
    let fixture = Fixture {
        repos: vec![repo(&id, "D:/src/r1")],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.click_on("activity-source-control");
    assert_ellipsized(&format!("sc-section-{id}::-repo"), &id.to_uppercase());
}

/// r1's panel with `s1` focused on `branch` in the worktree `C:/wt/r1`,
/// whose section is `r1::C:/wt/r1`.
fn on_branch<'a>(cx: &'a mut TestAppContext, dir: &TestDir, branch: &str) -> Harness<'a> {
    let s1 = session("s1").members(&[("r1", branch, "C:/wt/r1")]).build();
    let mut fixture = Fixture::single(s1);
    fixture.repos = vec![repo("r1", "D:/src/r1")];
    let mut h = Harness::with(cx, dir, &fixture);
    h.click_on("activity-source-control");
    h
}

#[gpui::test]
fn source_control_view_section_head_branch_ends_in_an_ellipsis(cx: &mut TestAppContext) {
    let dir = probed();
    let branch = long("feat/");
    let _h = on_branch(cx, &dir, &branch);
    assert_ellipsized("sc-section-r1::C:/wt/r1-branch", &branch.to_uppercase());
}

#[gpui::test]
fn source_control_view_section_head_shows_repo_and_branch_on_two_lines(cx: &mut TestAppContext) {
    let dir = probed();
    let _h = on_branch(cx, &dir, "feat/x");
    assert_eq!(
        drawn_text("sc-section-r1::C:/wt/r1-repo").as_deref(),
        Some("R1")
    );
    assert_eq!(
        drawn_text("sc-section-r1::C:/wt/r1-branch").as_deref(),
        Some("FEAT/X")
    );
}

#[gpui::test]
fn stash_view_head_is_drawn_through_the_ellipsis_helper(cx: &mut TestAppContext) {
    let dir = probed();
    let h = sc_panel(cx, &dir);
    h.cx.run_until_parked();
    // "Stashes" fits any panel width the sidebar allows; see the bucket head.
    assert_eq!(
        drawn_text("sc-stashes-r1::-title").as_deref(),
        Some("STASHES")
    );
}

// --- History -------------------------------------------------------------

const BROWSED: &str = "r1::";

fn commit(n: usize, subject: &str, author: &str) -> GitCommit {
    GitCommit {
        sha: format!("{:x}{}", 0x0a00_0000 + n, "0".repeat(33)),
        short_sha: format!("{:x}", 0x0a00_0000 + n),
        author_name: author.to_owned(),
        author_email: "ada@example.com".to_owned(),
        authored_at: "2026-09-26T14:03:12+02:00".to_owned(),
        subject: subject.to_owned(),
    }
}

fn commits(offset: u32, commits: Vec<GitCommit>) -> DaemonMessage {
    DaemonMessage::Commits {
        repo_id: "r1".to_owned(),
        commits,
        offset,
        worktree_path: None,
    }
}

/// The request id of the last `ListCommits` the client sent.
fn last_list_read(h: &mut Harness<'_>) -> String {
    h.sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::ListCommits { request_id, .. } => request_id,
            _ => None,
        })
        .next_back()
        .expect("a commit read with an id")
}

#[gpui::test]
fn history_view_title_ends_in_an_ellipsis(cx: &mut TestAppContext) {
    let dir = probed();
    let branch = long("feat/");
    let s1 = session("s1")
        .members(&[("r1", "main", "D:/src/r1"), ("r2", &branch, "C:/wt/r2")])
        .build();
    let mut fixture = Fixture::single(s1);
    fixture.repos = vec![repo("r1", "D:/src/r1"), repo("r2", "D:/src/r2")];
    let mut h = Harness::with(cx, &dir, &fixture);
    h.click_on("activity-source-control");
    assert_ellipsized("sc-history-r2::C:/wt/r2-title", &format!("r2 · {branch}"));
}

#[gpui::test]
fn history_view_commit_subject_ends_in_an_ellipsis(cx: &mut TestAppContext) {
    let dir = probed();
    let mut h = sc_panel(cx, &dir);
    let subject = long("subject");
    h.send(commits(0, vec![commit(1, &subject, "Ada")]));
    let row = format!("sc-commit-{BROWSED}-{}", commit(1, "", "").sha);
    assert_ellipsized(&format!("{row}-subject"), &subject);
}

#[gpui::test]
fn history_view_commit_author_ends_in_an_ellipsis(cx: &mut TestAppContext) {
    let dir = probed();
    let mut h = sc_panel(cx, &dir);
    let author = long("Author");
    h.send(commits(0, vec![commit(1, "change", &author)]));
    let row = format!("sc-commit-{BROWSED}-{}", commit(1, "", "").sha);
    assert_ellipsized(&format!("{row}-author"), &author);
}

#[gpui::test]
fn history_view_more_row_ends_in_an_ellipsis(cx: &mut TestAppContext) {
    let dir = probed();
    let mut h = sc_panel(cx, &dir);
    let page = (0..50).map(|n| commit(n, "change", "Ada")).collect();
    h.send(commits(0, page));
    let at = h.center(&format!("sc-history-list-{BROWSED}"));
    h.cx.simulate_event(ScrollWheelEvent {
        position: at,
        delta: ScrollDelta::Pixels(point(px(0.0), px(-100_000.0))),
        modifiers: Modifiers::none(),
        touch_phase: TouchPhase::Moved,
    });
    h.cx.run_until_parked();
    h.sent();
    h.click_on(&format!("sc-history-more-{BROWSED}"));
    let request_id = last_list_read(&mut h);
    let reason = long("reason");
    h.send(DaemonMessage::Error {
        message: format!("commit list failed: {reason}"),
        request_id: Some(request_id),
    });
    assert_ellipsized(
        &format!("sc-history-more-{BROWSED}"),
        &format!("couldn't load history: {reason}"),
    );
}

#[gpui::test]
fn history_view_detail_file_path_ends_in_an_ellipsis(cx: &mut TestAppContext) {
    let dir = probed();
    let mut h = sc_panel(cx, &dir);
    h.send(commits(0, vec![commit(1, "change", "Ada")]));
    h.click_on(&format!("sc-commit-{BROWSED}-{}", commit(1, "", "").sha));
    let path = format!("src/{}.rs", long("file"));
    h.send(DaemonMessage::CommitDetail {
        repo_id: "r1".to_owned(),
        detail: GitCommitDetail {
            commit: commit(1, "change", "Ada"),
            body: String::new(),
            parent_shas: vec!["parent0".to_owned()],
            changes: vec![GitFileChange {
                path: path.clone(),
                status: "M".to_owned(),
                from_path: None,
            }],
        },
    });
    assert_ellipsized(&format!("sc-detail-file-{BROWSED}-0-path"), &path);
}

// --- Diff tab ------------------------------------------------------------

fn diff_tab(path: &str) -> TabEntry {
    serde_json::from_value(json!({
        "id": "d1",
        "name": "diff",
        "content": {
            "kind": "diff",
            "repo_id": "r1",
            "path": path,
            "against": null,
            "worktree_path": "C:/wt/r1",
        },
        "created_at": "2026-01-01T00:00:00Z",
    }))
    .expect("diff tab fixture")
}

/// A diff tab of `path`, shown with one change.
fn show_diff(h: &mut Harness<'_>, path: &str) {
    h.send(DaemonMessage::TabUpdated {
        tab: diff_tab(path),
    });
    let id = h
        .sent()
        .into_iter()
        .find_map(|msg| match msg {
            ClientMessage::GetFileSnapshot { id, .. } => Some(id),
            _ => None,
        })
        .expect("the tab asks for its snapshot");
    h.click_on("tab-d1");
    h.send(DaemonMessage::FileSnapshot {
        id,
        repo_id: "r1".to_owned(),
        path: path.to_owned(),
        against: None,
        old: "a\n".to_owned(),
        new: "b\n".to_owned(),
        language: "rust".to_owned(),
        unavailable: None,
        worktree_path: Some("C:/wt/r1".to_owned()),
    });
}

fn focused() -> Fixture {
    let s1 = session("s1")
        .members(&[("r1", "feat/x", "C:/wt/r1")])
        .build();
    let mut fixture = Fixture::single(s1);
    fixture.repos = vec![repo("r1", "D:/src/r1")];
    fixture
}

#[gpui::test]
fn diff_tab_view_path_name_ends_in_an_ellipsis(cx: &mut TestAppContext) {
    let dir = probed();
    let mut h = Harness::with(cx, &dir, &focused());
    let name = format!("{}.rs", long("name"));
    show_diff(&mut h, &name);
    assert_ellipsized("diff-path-name", &name);
}

#[gpui::test]
fn diff_tab_view_path_folder_ends_in_an_ellipsis(cx: &mut TestAppContext) {
    let dir = probed();
    let mut h = Harness::with(cx, &dir, &focused());
    let folder = long("folder");
    show_diff(&mut h, &format!("{folder}/a.rs"));
    assert_ellipsized("diff-path-folder", &folder);
}

// --- Pane header ---------------------------------------------------------

#[gpui::test]
fn grid_view_pane_title_ends_in_an_ellipsis(cx: &mut TestAppContext) {
    let dir = probed();
    let title = long("Title");
    let h = Harness::with(
        cx,
        &dir,
        &Fixture::single(session("s1").label(&title).build()),
    );
    h.cx.run_until_parked();
    assert_ellipsized("pane-title-p1", &title);
}

#[gpui::test]
fn grid_view_branch_chip_ends_in_an_ellipsis(cx: &mut TestAppContext) {
    let dir = probed();
    let branch = long("feat/");
    let s1 = session("s1")
        .label("Short")
        .members(&[("tulip", &branch, "C:/wt/tulip")])
        .build();
    let mut fixture = Fixture::single(s1);
    fixture.repos = vec![repo("tulip", "D:/src/tulip")];
    let h = Harness::with(cx, &dir, &fixture);
    h.cx.run_until_parked();
    assert_ellipsized("pane-chip-p1-branch", &format!("tulip:{branch}"));
}

// --- Source-control header, context line and stashes ---------------------

#[gpui::test]
fn source_control_view_picker_label_ends_in_an_ellipsis(cx: &mut TestAppContext) {
    let dir = probed();
    let id = long("repo");
    let mut fixture = Fixture::single(session("s1").build());
    fixture.repos = vec![repo("r1", "D:/src/r1"), repo(&id, "D:/src/r2")];
    let mut h = Harness::with(cx, &dir, &fixture);
    h.click_on("activity-source-control");
    h.click_on("sc-picker");
    h.click_on(&format!("sc-picker-{id}"));
    assert_ellipsized("sc-picker-label", &format!("{id} ▾"));
}

#[gpui::test]
fn source_control_view_context_line_ends_in_an_ellipsis(cx: &mut TestAppContext) {
    let dir = probed();
    let id = long("repo");
    let fixture = Fixture {
        repos: vec![repo(&id, "D:/src/r1")],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.click_on("activity-source-control");
    assert_ellipsized("sc-context", &format!("{id} · no active pane"));
}

#[gpui::test]
fn stash_view_subject_ends_in_an_ellipsis(cx: &mut TestAppContext) {
    let dir = probed();
    let mut h = sc_panel(cx, &dir);
    h.click_on("sc-stashes-r1::");
    let subject = long("WIP on main");
    h.send(
        serde_json::from_value(json!({
            "type": "stashes",
            "repo_id": "r1",
            "stashes": [{ "id": "stash@{0}", "subject": subject, "created_at": "2024-05-01T12:34:56Z" }],
            "worktree_path": null,
        }))
        .expect("stashes fixture"),
    );
    assert_ellipsized("sc-stash-row-r1::|stash@{0}-subject", &subject);
}

// --- Needs you -----------------------------------------------------------

/// `s1` asking in r1 under the terminal title `title`, which its row shows
/// as its label and in its detail, with the Needs you panel open.
fn asking<'a>(cx: &'a mut TestAppContext, dir: &TestDir, title: &str) -> Harness<'a> {
    let fixture = Fixture {
        repos: vec![repo("r1", "D:/src/r1")],
        sessions: vec![
            session("s1")
                .in_repo("r1")
                .terminal_title(title)
                .status("awaiting_input")
                .status_since(Utc::now())
                .build(),
        ],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, dir, &fixture);
    h.click_on("activity-needs-you");
    h
}

#[gpui::test]
fn needs_you_view_row_label_ends_in_an_ellipsis(cx: &mut TestAppContext) {
    let dir = probed();
    let title = long("Title");
    let _h = asking(cx, &dir, &title);
    assert_ellipsized("needs-you-row-s1-label", &title);
}

#[gpui::test]
fn needs_you_view_row_detail_ends_in_an_ellipsis(cx: &mut TestAppContext) {
    let dir = probed();
    let title = long("Title");
    let _h = asking(cx, &dir, &title);
    assert_ellipsized(
        "needs-you-row-s1-detail",
        &format!("Waiting for input · {title}"),
    );
}

// --- Sidebar -------------------------------------------------------------

#[gpui::test]
fn sidebar_view_tab_pill_ends_in_an_ellipsis(cx: &mut TestAppContext) {
    let dir = probed();
    let name = long("tab");
    let mut fixture = Fixture::single(session("s1").build());
    fixture.tabs = vec![tab(&name, &pane("p1", Some("s1")))];
    let h = Harness::with(cx, &dir, &fixture);
    h.cx.run_until_parked();
    assert_ellipsized("leaf-pill-s1", &format!("T:{name}"));
}

#[gpui::test]
fn sidebar_view_container_name_ends_in_an_ellipsis(cx: &mut TestAppContext) {
    let dir = probed();
    let id = long("repo");
    let fixture = Fixture {
        repos: vec![repo(&id, "D:/src/r1")],
        sessions: vec![session("s1").in_repo(&id).build()],
        ..Fixture::default()
    };
    let h = Harness::with(cx, &dir, &fixture);
    h.cx.run_until_parked();
    assert_ellipsized(&format!("container-repo:{id}-name"), &id);
}

#[gpui::test]
fn sidebar_view_leaf_label_ends_in_an_ellipsis(cx: &mut TestAppContext) {
    let dir = probed();
    let label = long("Label");
    let h = Harness::with(
        cx,
        &dir,
        &Fixture::single(session("s1").label(&label).build()),
    );
    h.cx.run_until_parked();
    assert_ellipsized("leaf-s1-label", &label);
}

#[gpui::test]
fn sidebar_view_leaf_subline_ends_in_an_ellipsis(cx: &mut TestAppContext) {
    let dir = probed();
    let branch = long("feat/");
    let mut fixture = Fixture::single(
        session("s1")
            .members(&[("r1", &branch, "C:/wt/r1")])
            .build(),
    );
    fixture.repos = vec![repo("r1", "D:/src/r1")];
    let h = Harness::with(cx, &dir, &fixture);
    h.cx.run_until_parked();
    assert_ellipsized("leaf-subline-s1-label", &branch);
}

// --- Undo shelf and footer -----------------------------------------------

#[gpui::test]
fn undo_view_message_ends_in_an_ellipsis(cx: &mut TestAppContext) {
    let dir = probed();
    let label = long("Label");
    let mut h = Harness::with(
        cx,
        &dir,
        &Fixture::single(session("s1").label(&label).build()),
    );
    h.click_on("close-pane-p1");
    h.click_on("pane-close-only");
    let id = h
        .root(|root, _| root.undo_entries().first().map(|entry| entry.id))
        .expect("an undo entry");
    assert_ellipsized(
        &format!("undo-entry-{id}-message"),
        &format!("Closed pane \"{label}\""),
    );
}

#[gpui::test]
fn footer_status_ends_in_an_ellipsis(cx: &mut TestAppContext) {
    let dir = probed();
    let label = long("Label");
    let h = Harness::with(
        cx,
        &dir,
        &Fixture::single(session("s1").label(&label).build()),
    );
    h.cx.run_until_parked();
    assert_ellipsized("footer-status", &format!("{label} · s1"));
}

#[gpui::test]
fn footer_status_that_fits_is_drawn_whole(cx: &mut TestAppContext) {
    let dir = probed();
    let h = Harness::with(
        cx,
        &dir,
        &Fixture::single(session("s1").label("Short").build()),
    );
    h.cx.run_until_parked();
    assert_eq!(drawn_text("footer-status").as_deref(), Some("Short · s1"));
}
