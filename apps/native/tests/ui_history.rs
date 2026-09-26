//! Source-control History specs: lazy loading, the commit rows and their
//! tooltip, paging, a failed read, the commit detail and its files, the
//! forge button, the persisted split and the collapse defaults.

#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "a spec fails with the message of the precondition it lost"
)]

#[expect(dead_code, reason = "each spec file uses its own share of the helper")]
mod support;

use gpui::{Modifiers, ScrollDelta, ScrollWheelEvent, TestAppContext, TouchPhase, point, px};
use protocol::{
    ClientMessage, DaemonMessage, GitCommit, GitCommitDetail, GitFileChange, GitRemoteUrl,
};
use rustling_tulip_native::{
    CommitDetailView, DetailFile, DetailPane, HistoryBlock, HistoryBody, MoreRow,
};
use serde_json::json;
use support::{Fixture, Harness, Opened, TestDir, repo, session};

/// The browsed repo's section key id.
const BROWSED: &str = "r1::";
/// The focused session's member section key id.
const MEMBER: &str = "r1::C:/wt/r1";
const MEMBER_TREE: &str = "C:/wt/r1";

/// `r1` registered, no session focused.
fn browsing() -> Fixture {
    Fixture {
        repos: vec![repo("r1", "D:/src/r1")],
        ..Fixture::default()
    }
}

/// `r1` registered and a focused session on a worktree of it, on
/// `feat/x`.
fn focused() -> Fixture {
    let s1 = session("s1")
        .members(&[("r1", "feat/x", MEMBER_TREE)])
        .build();
    let mut fixture = Fixture::single(s1);
    fixture.repos = vec![repo("r1", "D:/src/r1")];
    fixture
}

fn open_panel(h: &mut Harness<'_>) {
    h.click_on("activity-source-control");
}

fn blocks(h: &mut Harness<'_>) -> Vec<HistoryBlock> {
    h.root(|root, _| root.history_panel())
}

fn block(h: &mut Harness<'_>) -> HistoryBlock {
    blocks(h).into_iter().next().expect("one History block")
}

/// A `ListCommits` the client sent: (repo, worktree, offset, limit,
/// request id).
type ListRead = (String, Option<String>, u32, u32, String);

fn split_reads(sent: Vec<ClientMessage>) -> (Vec<ListRead>, Vec<String>) {
    let mut lists = Vec::new();
    let mut remotes = Vec::new();
    for msg in sent {
        match msg {
            ClientMessage::ListCommits {
                repo_id,
                branch,
                limit,
                offset,
                worktree_path,
                request_id,
            } => {
                assert_eq!(branch, None, "the section's own HEAD");
                lists.push((
                    repo_id,
                    worktree_path,
                    offset,
                    limit,
                    request_id.expect("a commit read carries an id"),
                ));
            }
            ClientMessage::GetRemoteUrl { request_id, .. } => {
                remotes.push(request_id.expect("a remote read carries an id"));
            }
            _ => {}
        }
    }
    (lists, remotes)
}

fn list_reads(h: &mut Harness<'_>) -> Vec<ListRead> {
    split_reads(h.sent()).0
}

fn commit(n: usize) -> GitCommit {
    GitCommit {
        sha: format!("{:x}{}", 0x0a00_0000 + n, "0".repeat(33)),
        short_sha: format!("{:x}", 0x0a00_0000 + n),
        author_name: "Ada Lovelace".to_owned(),
        author_email: "ada@example.com".to_owned(),
        authored_at: "2026-09-26T14:03:12+02:00".to_owned(),
        subject: format!("change {n}"),
    }
}

fn commits(worktree: Option<&str>, offset: u32, from: usize, count: usize) -> DaemonMessage {
    DaemonMessage::Commits {
        repo_id: "r1".to_owned(),
        commits: (from..from + count).map(commit).collect(),
        offset,
        worktree_path: worktree.map(str::to_owned),
    }
}

fn remote(forge: &str, web_url: Option<&str>) -> DaemonMessage {
    DaemonMessage::RemoteUrl(GitRemoteUrl {
        repo_id: "r1".to_owned(),
        raw_url: "git@github.com:o/r.git".to_owned(),
        web_url: web_url.map(str::to_owned),
        forge: forge.to_owned(),
    })
}

fn detail(n: usize, parents: usize, changes: Vec<GitFileChange>) -> DaemonMessage {
    DaemonMessage::CommitDetail {
        repo_id: "r1".to_owned(),
        detail: GitCommitDetail {
            commit: commit(n),
            body: "Why it changed.".to_owned(),
            parent_shas: (0..parents).map(|p| format!("parent{p}")).collect(),
            changes,
        },
    }
}

fn change(path: &str, status: &str, from: Option<&str>) -> GitFileChange {
    GitFileChange {
        path: path.to_owned(),
        status: status.to_owned(),
        from_path: from.map(str::to_owned),
    }
}

fn rows_of(block: &HistoryBlock) -> (Vec<String>, Option<MoreRow>, Option<DetailPane>) {
    match &block.body {
        HistoryBody::Commits { rows, more, detail } => (
            rows.iter().map(|row| row.subject.clone()).collect(),
            more.clone(),
            detail.clone(),
        ),
        other => panic!("expected commit rows, got {other:?}"),
    }
}

fn loaded_detail(block: &HistoryBlock) -> CommitDetailView {
    match rows_of(block).2 {
        Some(DetailPane::Loaded(view)) => view,
        other => panic!("expected a loaded detail, got {other:?}"),
    }
}

fn toast_count(h: &mut Harness<'_>) -> usize {
    h.root(|root, _| root.toasts().len())
}

/// Scrolls section `id`'s commit list to its end, so its last row is
/// under the pointer.
fn scroll_list_to_end(h: &mut Harness<'_>, id: &str) {
    let at = h.center(&format!("sc-history-list-{id}"));
    h.cx.simulate_event(ScrollWheelEvent {
        position: at,
        delta: ScrollDelta::Pixels(point(px(0.0), px(-100_000.0))),
        modifiers: Modifiers::none(),
        touch_phase: TouchPhase::Moved,
    });
    h.cx.run_until_parked();
}

fn saved_ui(dir: &TestDir) -> serde_json::Value {
    serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("native-ui.json")).expect("native-ui.json"),
    )
    .expect("native-ui.json is JSON")
}

#[gpui::test]
fn expanding_sends_list_commits_with_limit_50(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &focused());
    open_panel(&mut h);
    assert!(
        list_reads(&mut h).is_empty(),
        "collapsed, so nothing is read"
    );

    h.click_on(&format!("sc-history-{MEMBER}"));
    let reads = list_reads(&mut h);
    assert_eq!(reads.len(), 1, "one read: {reads:?}");
    let (repo_id, worktree, offset, limit, _) = &reads[0];
    assert_eq!(repo_id, "r1");
    assert_eq!(worktree.as_deref(), Some(MEMBER_TREE));
    assert_eq!((*offset, *limit), (0, 50));
    assert_eq!(block(&mut h).body, HistoryBody::Loading);
    assert_eq!(
        saved_ui(&dir)["source_control"]["collapsed"][format!("{MEMBER}|history")],
        json!(false)
    );
}

#[gpui::test]
fn rows_render_with_their_tooltip(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &browsing());
    open_panel(&mut h);
    let reads = list_reads(&mut h);
    assert_eq!(reads.len(), 1, "expanded while browsing, so read at once");

    h.send(commits(None, 0, 1, 2));
    let shown = block(&mut h);
    assert_eq!(shown.count, Some(2));
    let HistoryBody::Commits { rows, more, detail } = &shown.body else {
        panic!("expected rows, got {:?}", shown.body);
    };
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].short_sha, "a000001");
    assert_eq!(rows[0].subject, "change 1");
    assert_eq!(rows[0].author, "Ada Lovelace");
    assert_eq!(
        rows[0].tooltip,
        format!(
            "change 1\nSHA: {}\nAuthor: Ada Lovelace <ada@example.com>\nDate: 2026-09-26 14:03",
            commit(1).sha
        )
    );
    assert_eq!(*more, None, "a short first page is the whole history");
    assert_eq!(*detail, None);
    assert!(
        h.bounds(&format!("sc-commit-{BROWSED}-{}", commit(1).sha))
            .size
            .height
            > px(0.0)
    );
}

#[gpui::test]
fn load_more_sends_the_next_offset_and_disappears_when_exhausted(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &browsing());
    open_panel(&mut h);
    list_reads(&mut h);
    h.send(commits(None, 0, 0, 50));
    assert_eq!(rows_of(&block(&mut h)).1, Some(MoreRow::LoadMore));

    scroll_list_to_end(&mut h, BROWSED);
    h.click_on(&format!("sc-history-more-{BROWSED}"));
    let reads = list_reads(&mut h);
    assert_eq!(reads.len(), 1);
    assert_eq!(reads[0].2, 50, "the next page starts after the loaded ones");
    assert_eq!(rows_of(&block(&mut h)).1, Some(MoreRow::Loading));

    // The page repeats the last loaded commit, which is shown once.
    h.send(commits(None, 50, 49, 11));
    let shown = block(&mut h);
    assert_eq!(shown.count, Some(60));
    assert_eq!(rows_of(&shown).1, None, "fewer than 50 ends the list");
}

#[gpui::test]
fn a_failed_load_shows_the_text_with_no_toast(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &browsing());
    open_panel(&mut h);
    let reads = list_reads(&mut h);
    let request_id = reads[0].4.clone();

    h.send(DaemonMessage::Error {
        message: "commit list failed: not a git repository".to_owned(),
        request_id: Some(request_id),
    });
    assert_eq!(
        block(&mut h).body,
        HistoryBody::Failed("couldn't load history: not a git repository".to_owned())
    );
    assert_eq!(toast_count(&mut h), 0, "the panel shows it; no toast");

    h.send(DaemonMessage::Error {
        message: "something else".to_owned(),
        request_id: Some("not-ours".to_owned()),
    });
    assert_eq!(toast_count(&mut h), 1, "other errors still toast");
}

#[gpui::test]
fn clicking_a_commit_sends_get_commit_and_the_detail_shows_files(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &browsing());
    open_panel(&mut h);
    list_reads(&mut h);
    h.send(commits(None, 0, 1, 3));

    let row = format!("sc-commit-{BROWSED}-{}", commit(2).sha);
    h.click_on(&row);
    let sent = h.sent();
    assert!(
        sent.iter().any(|msg| matches!(
            msg,
            ClientMessage::GetCommit { repo_id, sha, request_id: Some(_) }
                if repo_id == "r1" && *sha == commit(2).sha
        )),
        "sent {sent:?}"
    );
    assert_eq!(rows_of(&block(&mut h)).2, Some(DetailPane::Loading));

    h.send(detail(
        2,
        1,
        vec![
            change("src/lib.rs", "M", None),
            change("src/new.rs", "R", Some("src/old.rs")),
        ],
    ));
    let view = loaded_detail(&block(&mut h));
    assert_eq!(view.heading, "a000002 change 2");
    assert_eq!(view.author, "Author: Ada Lovelace <ada@example.com>");
    assert_eq!(view.date, "Date: 2026-09-26 14:03");
    assert_eq!(view.body.as_deref(), Some("Why it changed."));
    assert_eq!(
        view.files,
        [
            DetailFile {
                status: "M".to_owned(),
                path: "src/lib.rs".to_owned(),
                tooltip: None,
            },
            DetailFile {
                status: "R".to_owned(),
                path: "src/new.rs".to_owned(),
                tooltip: Some("src/old.rs → src/new.rs".to_owned()),
            },
        ]
    );
    assert!(h.bounds(&format!("sc-detail-{BROWSED}")).size.height > px(0.0));

    h.click_on(&row);
    assert_eq!(rows_of(&block(&mut h)).2, None, "a second click deselects");
    h.click_on(&row);
    assert!(
        !h.sent()
            .iter()
            .any(|msg| matches!(msg, ClientMessage::GetCommit { .. })),
        "the detail is cached"
    );
    assert_eq!(loaded_detail(&block(&mut h)).heading, "a000002 change 2");
}

#[gpui::test]
fn a_merge_commits_detail_says_it_has_no_file_changes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &browsing());
    open_panel(&mut h);
    list_reads(&mut h);
    h.send(commits(None, 0, 1, 2));
    h.click_on(&format!("sc-commit-{BROWSED}-{}", commit(1).sha));
    h.send(detail(1, 2, Vec::new()));
    let view = loaded_detail(&block(&mut h));
    assert!(view.files.is_empty());
    assert_eq!(view.no_files, Some("no file changes (merge commit)"));

    h.click_on(&format!("sc-commit-{BROWSED}-{}", commit(2).sha));
    h.send(detail(2, 1, Vec::new()));
    assert_eq!(
        loaded_detail(&block(&mut h)).no_files,
        Some("no file changes")
    );
}

#[gpui::test]
fn clicking_a_detail_file_opens_its_diff_against_the_commit(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &focused());
    open_panel(&mut h);
    h.click_on(&format!("sc-history-{MEMBER}"));
    list_reads(&mut h);
    h.send(commits(Some(MEMBER_TREE), 0, 1, 2));
    h.click_on(&format!("sc-commit-{MEMBER}-{}", commit(1).sha));
    h.send(detail(1, 1, vec![change("src/main.rs", "M", None)]));
    h.sent();

    h.click_on(&format!("sc-detail-file-{MEMBER}-0"));
    let opens: Vec<ClientMessage> = h
        .sent()
        .into_iter()
        .filter(|msg| matches!(msg, ClientMessage::OpenDiffTab { .. }))
        .collect();
    assert_eq!(opens.len(), 1, "one diff tab: {opens:?}");
    let ClientMessage::OpenDiffTab {
        id,
        repo_id,
        path,
        against,
        worktree_path,
    } = &opens[0]
    else {
        panic!("filtered to diff tabs");
    };
    assert!(!id.is_empty());
    assert_eq!(repo_id, "r1");
    assert_eq!(path, "src/main.rs");
    assert_eq!(against.as_deref(), Some(commit(1).sha.as_str()));
    assert_eq!(worktree_path.as_deref(), Some(MEMBER_TREE));
}

#[gpui::test]
fn the_forge_button_opens_the_branch_url(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &focused());
    open_panel(&mut h);
    let (_, remotes) = split_reads(h.sent());
    assert_eq!(remotes.len(), 1, "one lookup for the shown repo");

    h.send(remote("github", Some("https://github.com/o/r/")));
    let forge = block(&mut h).forge;
    assert_eq!(forge.tooltip, "Open feat/x on GitHub");
    h.click_on(&format!("sc-history-forge-{MEMBER}"));
    assert_eq!(
        h.opened(),
        [Opened::Url("https://github.com/o/r/tree/feat/x".to_owned())]
    );
    assert!(
        split_reads(h.sent()).1.is_empty(),
        "the remote is cached per repo"
    );
}

#[gpui::test]
fn the_forge_button_is_disabled_with_each_tooltip(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &browsing());
    open_panel(&mut h);
    h.sent();
    let forge = block(&mut h).forge;
    assert_eq!(forge.url, None);
    assert_eq!(forge.tooltip, "Looking up origin…");
    h.click_on(&format!("sc-history-forge-{BROWSED}"));
    assert!(h.opened().is_empty(), "a disabled button opens nothing");

    h.send(remote("unknown", Some("https://git.example/o/r")));
    let forge = block(&mut h).forge;
    assert_eq!(forge.url, None);
    assert_eq!(forge.tooltip, "origin isn't on GitHub, GitLab or Bitbucket");

    h.send(DaemonMessage::Welcome {
        protocol_version: 1,
        supported_versions: vec![1],
    });
    let (_, remotes) = split_reads(h.sent());
    assert_eq!(
        remotes.len(),
        1,
        "a new connection looks the remote up again"
    );
    assert_eq!(block(&mut h).forge.tooltip, "Looking up origin…");
    h.send(DaemonMessage::Error {
        message: "remote url failed: No such remote 'origin'".to_owned(),
        request_id: Some(remotes[0].clone()),
    });
    let forge = block(&mut h).forge;
    assert_eq!(forge.url, None);
    assert_eq!(forge.tooltip, "This repository has no origin remote");
    assert_eq!(toast_count(&mut h), 0);
    h.click_on(&format!("sc-history-forge-{BROWSED}"));
    assert!(h.opened().is_empty());
}

#[gpui::test]
fn dragging_the_split_persists_changes_height(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &browsing());
    open_panel(&mut h);
    let top = h.bounds("sc-changes").origin.y;
    assert_eq!(h.bounds("sc-changes").size.height, px(420.0), "the default");

    let from = h.center("sc-split");
    let to = point(from.x, top + px(300.0));
    h.drag(from, to, [Modifiers::none(); 2]);
    let saved = saved_ui(&dir)["source_control"]["changes_height"]
        .as_f64()
        .expect("the height is saved");
    assert!((saved - 300.0).abs() < 0.5, "saved {saved}");
    assert_eq!(h.bounds("sc-changes").size.height, px(300.0));

    let from = h.center("sc-split");
    h.drag(from, point(from.x, top), [Modifiers::none(); 2]);
    assert_eq!(
        h.bounds("sc-changes").size.height,
        px(140.0),
        "the changes keep 140"
    );
    drop(h);

    let mut h = Harness::with(cx, &dir, &browsing());
    assert_eq!(h.bounds("sc-changes").size.height, px(140.0), "restored");
}

#[gpui::test]
fn history_is_collapsed_for_a_focused_session_and_expanded_when_browsing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = focused();
    let tabs = std::mem::take(&mut fixture.tabs);
    let mut h = Harness::with(cx, &dir, &fixture);
    open_panel(&mut h);
    let shown = block(&mut h);
    assert_eq!(shown.id, BROWSED, "no pane, so the repo is browsed");
    assert!(shown.expanded);
    assert_eq!(list_reads(&mut h).len(), 1);
    assert!(h.in_model("sc-split"));

    h.send(DaemonMessage::Tabs { tabs });
    let shown = block(&mut h);
    assert_eq!(shown.id, MEMBER, "the focused session's member");
    assert!(!shown.expanded, "collapsed by default");
    assert_eq!(shown.body, HistoryBody::Collapsed);
    assert!(list_reads(&mut h).is_empty());
    assert!(
        h.bounds("sc-changes").size.height > px(420.0),
        "with no History expanded the changes take the rest"
    );
}

#[gpui::test]
fn the_first_frame_of_the_panel_clamps_the_split(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    std::fs::write(
        dir.path().join("native-ui.json"),
        r#"{"source_control": {"changes_height": 5000.0}}"#,
    )
    .expect("seeding native-ui.json");
    let mut h = Harness::with(cx, &dir, &browsing());
    open_panel(&mut h);

    let body = h.bounds("sc-body");
    let changes = h.bounds("sc-changes");
    assert!(body.size.height > px(280.0), "room for both sides");
    let window_height = h.cx.update(|window, _| window.viewport_size().height);
    assert!(
        body.bottom() <= window_height,
        "the body fits the window: {body:?}"
    );
    assert_eq!(
        changes.size.height,
        body.size.height - px(140.0),
        "the stored height is clamped so the History keeps 140"
    );
    let header = h.bounds(&format!("sc-history-{BROWSED}"));
    assert!(header.size.height > px(0.0), "the History header is drawn");
    assert!(
        header.origin.y >= changes.bottom() && header.bottom() <= body.bottom(),
        "and inside the body: header {header:?}, body {body:?}"
    );
}

#[gpui::test]
fn switching_the_rail_to_source_control_reads_an_expanded_history(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &browsing());
    assert!(
        list_reads(&mut h).is_empty(),
        "the sessions panel shows, so no History is read"
    );

    open_panel(&mut h);
    let reads = list_reads(&mut h);
    assert_eq!(reads.len(), 1, "one read: {reads:?}");
    let (repo_id, worktree, offset, limit, _) = &reads[0];
    assert_eq!(
        (repo_id.as_str(), worktree.as_deref(), *offset, *limit),
        ("r1", None, 0, 50)
    );
}

#[gpui::test]
fn refresh_reloads_from_offset_0(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &browsing());
    open_panel(&mut h);
    list_reads(&mut h);
    h.send(commits(None, 0, 0, 50));
    scroll_list_to_end(&mut h, BROWSED);
    h.click_on(&format!("sc-history-more-{BROWSED}"));
    list_reads(&mut h);
    h.send(commits(None, 50, 50, 50));
    h.send(remote("github", Some("https://github.com/o/r")));
    assert_eq!(block(&mut h).count, Some(100));

    h.click_on("sc-refresh");
    let (lists, remotes) = split_reads(h.sent());
    assert_eq!(lists.len(), 1, "one read: {lists:?}");
    assert_eq!(lists[0].2, 0, "from the start");
    assert_eq!(remotes.len(), 1, "and the remote again");
    h.send(commits(None, 0, 7, 3));
    let shown = block(&mut h);
    assert_eq!(shown.count, Some(3), "replaced, not appended");
    assert_eq!(rows_of(&shown).0, ["change 7", "change 8", "change 9"]);
}
