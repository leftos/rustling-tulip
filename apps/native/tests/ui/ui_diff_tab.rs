//! Diff tab specs: opening a diff from the History and the changes tree,
//! activating the tab the daemon names in either arrival order, the header,
//! the change buttons and F7, the whitespace toggle, syntax highlighting and
//! its toggle, the texts standing in for a diff, the live refresh and a
//! refused open.

#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use std::ops::Range;

use gpui::{Entity, Hsla, SharedString, TestAppContext, TextRun, px, size};
use protocol::{
    ClientMessage, DaemonMessage, GitCommit, GitCommitDetail, GitFileChange, SnapshotUnavailable,
    TabEntry,
};
use rustling_tulip_native::diff_view::{DiffView, Half};
use rustling_tulip_native::fonts;
use rustling_tulip_native::target::Target;
use rustling_tulip_native::{
    BINARY_TEXT, Connection, DIFF_OPEN_FAILED_TITLE, DiffTabBody, DiffTabHeader, EMPTY_TEXT,
    NetEvent, ToastKind,
};
use serde_json::{Value, json};
use support::{Fixture, Harness, TestDir, remote_in_pane, repo, session};

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
    diff_tab_on_host(id, path, against, worktree, None)
}

/// A diff tab of `path` in the tree `worktree` on `host`; `None` is This
/// PC.
fn diff_tab_on_host(
    id: &str,
    path: &str,
    against: Option<&str>,
    worktree: Option<&str>,
    host: Option<&str>,
) -> TabEntry {
    serde_json::from_value(json!({
        "id": id,
        "name": path,
        "content": {
            "kind": "diff",
            "repo_id": "r1",
            "path": path,
            "against": against,
            "worktree_path": worktree,
            "host": host,
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

/// An `OpenDiffTab` or `GetFileSnapshot` the client sent: (id, path,
/// against, worktree, the host it names).
type Open = (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
);

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
                host,
            } => {
                assert_eq!(repo_id, "r1");
                Some((id, path, against, worktree_path, host))
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

/// The `GetFileSnapshot`s in `sent`.
fn snapshots(sent: Vec<ClientMessage>) -> Vec<Open> {
    sent.into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::GetFileSnapshot {
                id,
                repo_id,
                path,
                against,
                worktree_path,
            } => {
                assert_eq!(repo_id, "r1");
                Some((id, path, against, worktree_path, None))
            }
            _ => None,
        })
        .collect()
}

/// The `GetFileSnapshot`s the client sent to This PC.
fn snapshot_requests(h: &mut Harness<'_>) -> Vec<Open> {
    snapshots(h.sent())
}

/// The `GetFileSnapshot`s the client sent to `target`'s daemon.
fn host_snapshots(h: &mut Harness<'_>, target: &Target) -> Vec<Open> {
    snapshots(h.host(target).sent())
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

/// The id of the one `OpenViewerTab` sent since the last look.
fn one_viewer_open(h: &mut Harness<'_>) -> String {
    let ids: Vec<String> = h
        .sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::OpenViewerTab { id, .. } => Some(id),
            _ => None,
        })
        .collect();
    let [id] = ids.as_slice() else {
        panic!("one OpenViewerTab is sent, got {ids:?}");
    };
    id.clone()
}

#[gpui::test]
fn a_remote_welcome_keeps_this_pcs_pending_diff_and_viewer_opens(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &focused());
    let (diff_id, ..) = open_from_history(&mut h);
    h.root_update(|root, _, _| root.open_viewer("C:/notes/a.md", None));
    let viewer_id = one_viewer_open(&mut h);

    h.host(&Target::tailnet(support::REMOTE, None)).connect();

    h.send(updated(&diff_tab("d1", "src/main.rs", Some(SHA))));
    h.send(opened(&diff_id, "d1"));
    assert_eq!(
        active(&mut h).as_deref(),
        Some("d1"),
        "the diff open still activates its tab"
    );
    let viewer: TabEntry = serde_json::from_value(json!({
        "id": "v1",
        "name": "a.md",
        "content": { "kind": "viewer", "path": "C:/notes/a.md" },
        "created_at": "2026-01-01T00:00:00Z",
    }))
    .expect("viewer tab fixture");
    h.send(updated(&viewer));
    h.send(DaemonMessage::ViewerTabOpened {
        id: viewer_id,
        tab_id: "v1".to_owned(),
    });
    assert_eq!(
        active(&mut h).as_deref(),
        Some("v1"),
        "the viewer open still activates its tab"
    );
}

// --- A remote host -------------------------------------------------------

/// [`support::REMOTE`] as the specs dial it beside This PC.
fn remote() -> Target {
    Target::tailnet(support::REMOTE, None)
}

/// The remote session's tree `r1`, and its section's key id.
const REMOTE_TREE: &str = "C:/wt/x";
const REMOTE_MEMBER: &str = "r1::C:/wt/x";

/// This PC with `r1` registered; [`support::REMOTE`] connected beside it.
fn two_hosts<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = Harness::with(cx, dir, &focused());
    h.host(&remote()).connect();
    h
}

/// The panel on [`support::REMOTE`]'s session `s1`, whose tree `r1` holds
/// `s.rs` staged and `c.rs` changed.
fn remote_changes_panel<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let s1 = session("s1").worktree("r1").build();
    let mut h = remote_in_pane(cx, dir, &json!({}), &["r1"], s1);
    h.click_on("activity-source-control");
    h.host(&remote())
        .send(status(&["s.rs"], &["c.rs"], Some(REMOTE_TREE)));
    h.sent();
    h.host(&remote()).sent();
    h
}

/// `target`'s daemon answers `id` with `old` and `new`.
fn answer_from(h: &mut Harness<'_>, target: &Target, id: &str, old: &str, new: &str) {
    h.host(target).send(snapshot(id, old, new, None));
}

/// Reports [`support::REMOTE`]'s connection closed.
fn close_remote(h: &mut Harness<'_>) {
    let mut closed = Connection::for_target(remote());
    closed.on_connecting();
    let _ = closed.on_closed("socket closed".to_owned());
    h.host(&remote()).event(NetEvent::State(closed));
}

#[gpui::test]
fn a_remote_repos_diff_opens_here_naming_its_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_changes_panel(cx, &dir);
    h.click_on(&format!("sc-row-changes-{REMOTE_MEMBER}|c.rs"));
    let (_, path, against, worktree, host) = one_open(&mut h);
    assert_eq!(path, "c.rs");
    assert_eq!(against, None);
    assert_eq!(worktree.as_deref(), Some(REMOTE_TREE));
    assert_eq!(
        host.as_deref(),
        Some(support::REMOTE),
        "the open names the host the file is on"
    );
    assert!(
        h.host(&remote()).sent().is_empty(),
        "the open is This PC's layout's to answer"
    );
}

#[gpui::test]
fn a_remote_diff_tab_reads_from_its_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir);
    h.send(updated(&diff_tab_on_host(
        "d1",
        "src/main.rs",
        None,
        Some(REMOTE_TREE),
        Some(support::REMOTE),
    )));
    let read = host_snapshots(&mut h, &remote());
    assert_eq!(read.len(), 1, "one read of the host: {read:?}");
    assert_eq!(read[0].1, "src/main.rs");
    assert_eq!(read[0].3.as_deref(), Some(REMOTE_TREE));
    assert!(
        snapshot_requests(&mut h).is_empty(),
        "This PC is asked for nothing"
    );
}

#[gpui::test]
fn a_snapshot_lands_only_in_the_tab_that_asked_its_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir);
    h.send(updated(&diff_tab_on_host(
        "d1",
        "src/main.rs",
        None,
        Some(REMOTE_TREE),
        Some(support::REMOTE),
    )));
    let remote_snapshot = host_snapshots(&mut h, &remote())[0].0.clone();
    h.send(updated(&diff_tab_in("d2", "a.rs", None, Some(TREE))));
    let local_snapshot = one_snapshot_id(&mut h);

    answer(&mut h, &remote_snapshot, &lines(3, &[]), &lines(3, &[2]));
    assert_eq!(text(&mut h, "d1"), "loading…", "This PC cannot answer it");

    answer_from(
        &mut h,
        &remote(),
        &remote_snapshot,
        &lines(3, &[]),
        &lines(3, &[2]),
    );
    assert_eq!(header(&mut h, "d1").count, "1 change");

    answer_from(
        &mut h,
        &remote(),
        &local_snapshot,
        &lines(3, &[]),
        &lines(3, &[2]),
    );
    assert_eq!(
        text(&mut h, "d2"),
        "loading…",
        "another host's id is not its tab's"
    );

    answer(&mut h, &local_snapshot, &lines(3, &[]), &lines(3, &[2]));
    assert_eq!(header(&mut h, "d2").count, "1 change");
}

#[gpui::test]
fn a_commits_diff_on_a_remote_repo_reads_from_its_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir);
    h.send(updated(&diff_tab_on_host(
        "d1",
        "src/main.rs",
        Some(SHA),
        Some(REMOTE_TREE),
        Some(support::REMOTE),
    )));
    let read = host_snapshots(&mut h, &remote());
    assert_eq!(read.len(), 1, "one read of the host: {read:?}");
    assert_eq!(read[0].2.as_deref(), Some(SHA));
    assert!(
        snapshot_requests(&mut h).is_empty(),
        "This PC is asked for nothing"
    );
}

#[gpui::test]
fn a_remote_welcome_reads_only_its_own_tabs_again(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir);
    h.send(updated(&diff_tab_on_host(
        "d1",
        "src/main.rs",
        None,
        Some(REMOTE_TREE),
        Some(support::REMOTE),
    )));
    let remote_read = host_snapshots(&mut h, &remote())[0].0.clone();
    h.send(updated(&diff_tab_in("d2", "a.rs", None, Some(TREE))));
    let local_read = one_snapshot_id(&mut h);
    answer_from(&mut h, &remote(), &remote_read, "a\n", "b\n");
    answer(&mut h, &local_read, "a\n", "b\n");

    h.host(&remote()).connect_keeping_sent();
    let again = host_snapshots(&mut h, &remote());
    assert_eq!(again.len(), 1, "the host's own tab reads again: {again:?}");
    assert_eq!(again[0].1, "src/main.rs");
    assert!(
        snapshot_requests(&mut h).is_empty(),
        "This PC's tab is not re-read by the host's Welcome"
    );

    h.send(support::welcome(support::PROTOCOL));
    assert!(
        host_snapshots(&mut h, &remote()).is_empty(),
        "This PC's Welcome leaves the host's tab alone"
    );
    assert_eq!(
        snapshot_requests(&mut h).len(),
        1,
        "This PC's own tab reads again"
    );
}

#[gpui::test]
fn a_tab_restored_before_its_host_connects_says_so_and_reads_once_it_does(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &focused());
    h.send(updated(&diff_tab_on_host(
        "d1",
        "src/main.rs",
        Some(SHA),
        Some(REMOTE_TREE),
        Some(support::REMOTE),
    )));
    assert_eq!(
        text(&mut h, "d1"),
        "box is offline",
        "the host is not connected"
    );
    assert!(
        snapshot_requests(&mut h).is_empty(),
        "This PC is asked for nothing"
    );

    h.host(&remote()).connect_keeping_sent();
    let read = host_snapshots(&mut h, &remote());
    assert_eq!(
        read.len(),
        1,
        "the commit's diff reads from the host once it connects: {read:?}"
    );
    assert_eq!(read[0].2.as_deref(), Some(SHA));
    assert_eq!(text(&mut h, "d1"), "loading…");
}

#[gpui::test]
fn a_status_refreshes_only_its_own_hosts_tabs(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir);
    h.send(updated(&diff_tab_on_host(
        "d1",
        "src/main.rs",
        None,
        Some(TREE),
        Some(support::REMOTE),
    )));
    let remote_read = host_snapshots(&mut h, &remote())[0].0.clone();
    h.send(updated(&diff_tab_in("d2", "a.rs", None, Some(TREE))));
    let local_read = one_snapshot_id(&mut h);
    answer_from(&mut h, &remote(), &remote_read, "a\n", "b\n");
    answer(&mut h, &local_read, "a\n", "b\n");

    h.host(&remote())
        .send(status(&[], &["src/main.rs"], Some(TREE)));
    assert_eq!(
        host_snapshots(&mut h, &remote()).len(),
        1,
        "the host's own tab reads again"
    );
    assert!(
        snapshot_requests(&mut h).is_empty(),
        "This PC's tab shares the key but not the host"
    );

    h.send(status(&[], &["a.rs"], Some(TREE)));
    assert_eq!(
        snapshot_requests(&mut h).len(),
        1,
        "This PC's own tab reads again"
    );
    assert!(
        host_snapshots(&mut h, &remote()).is_empty(),
        "the host's tab is left alone"
    );
}

#[gpui::test]
fn a_remote_close_says_the_host_is_offline_and_its_welcome_reads_again(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir);
    h.send(updated(&diff_tab_on_host(
        "d1",
        "src/main.rs",
        None,
        Some(REMOTE_TREE),
        Some(support::REMOTE),
    )));
    let read = host_snapshots(&mut h, &remote())[0].0.clone();
    answer_from(&mut h, &remote(), &read, &lines(3, &[]), &lines(3, &[2]));
    assert_eq!(header(&mut h, "d1").count, "1 change");

    close_remote(&mut h);
    assert_eq!(
        text(&mut h, "d1"),
        "box is offline",
        "the tab says why it cannot show the diff"
    );

    h.host(&remote()).connect_keeping_sent();
    assert_eq!(
        host_snapshots(&mut h, &remote()).len(),
        1,
        "its next Welcome reads again"
    );
    assert_eq!(
        header(&mut h, "d1").count,
        "1 change",
        "the diff it held shows again while the fresh read is out"
    );
}

/// Whether `tab`'s body says a host is offline.
fn says_offline(h: &mut Harness<'_>, tab: &str) -> bool {
    matches!(
        body(h, tab),
        DiffTabBody::Text { text, .. } if text.ends_with("is offline")
    )
}

#[gpui::test]
fn this_pcs_reconnect_leaves_its_diff_tabs_as_they_were(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &focused());
    h.send(updated(&diff_tab_in("d1", "a.rs", None, Some(TREE))));
    let live = one_snapshot_id(&mut h);
    answer(&mut h, &live, &lines(3, &[]), &lines(3, &[2]));
    h.send(updated(&diff_tab_in(
        "d2",
        "src/main.rs",
        Some(SHA),
        Some(TREE),
    )));
    let commit = one_snapshot_id(&mut h);
    answer(&mut h, &commit, &lines(3, &[]), &lines(3, &[1]));

    h.lose_connection();
    assert!(
        !says_offline(&mut h, "d1"),
        "This PC's own tab never reads offline"
    );
    assert!(!says_offline(&mut h, "d2"), "nor does a commit's diff");
    assert_eq!(header(&mut h, "d2").count, "1 change", "it keeps its count");

    h.welcome_with_features();
    h.send_welcome(support::PROTOCOL);
    let again = snapshot_requests(&mut h);
    assert_eq!(
        again.len(),
        1,
        "the live tab reads again, as it always has, and the commit's does not: {again:?}"
    );
    assert_eq!(again[0].1, "a.rs");
    assert_eq!(again[0].2, None, "the worktree-against-index read");
    assert_eq!(
        header(&mut h, "d2").count,
        "1 change",
        "the commit's diff is untouched"
    );
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
    let (_, path, against, worktree, host) = one_open(&mut h);
    assert_eq!(path, "c.rs");
    assert_eq!(against, None);
    assert_eq!(worktree.as_deref(), Some(TREE));
    assert_eq!(host, None, "This PC's repo names no host");
}

#[gpui::test]
fn clicking_a_staged_row_opens_its_diff_against_head(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = changes_panel(cx, &dir);
    h.click_on(&format!("sc-row-staged-{MEMBER}|s.rs"));
    let (_, path, against, worktree, _) = one_open(&mut h);
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
    let (_, path, against, _, _) = one_open(&mut h);
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
    let (_, path, against, _, _) = one_open(&mut h);
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
            Some(TREE.to_owned()),
            None
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
            highlight: true,
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

/// The client's UI text size, which the diff tab's header is drawn at; it
/// mirrors `UI_TEXT_SIZE` in `lib.rs`, which is private to the crate.
const UI_TEXT_SIZE: f32 = 12.0;
/// A file name longer than a narrow header has room for.
const LONG_NAME: &str =
    "a_really_long_file_name_a_really_long_file_name_a_really_long_file_name.rs";
/// A window whose diff tab header is narrower than the toggles' threshold.
const NARROW: (f32, f32) = (1000.0, 600.0);
/// A window whose diff tab header is wider than the toggles' threshold.
const WIDE: (f32, f32) = (2000.0, 600.0);

fn resize(h: &mut Harness<'_>, (width, height): (f32, f32)) {
    h.cx.simulate_resize(size(px(width), px(height)));
    h.cx.run_until_parked();
}

/// `text`'s width, in px, shaped at the UI text size.
fn shaped_width(h: &mut Harness<'_>, text: &str) -> f32 {
    let family = h.cx.update(|_, cx| fonts::ui_family(cx));
    let run = TextRun {
        len: text.len(),
        font: gpui::font(family),
        color: gpui::black(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let line = h.cx.update(|window, _| {
        window.text_system().shape_line(
            SharedString::from(text.to_owned()),
            px(UI_TEXT_SIZE),
            &[run],
            None,
        )
    });
    line.width / px(1.0)
}

/// Diff tab `d1` of `path`, shown with one change.
fn shown_diff(h: &mut Harness<'_>, path: &str) {
    let id = show(h, &diff_tab("d1", path, None));
    answer(h, &id, "a\n", "b\n");
}

#[gpui::test]
fn the_file_name_stays_whole_when_its_folder_runs_out_of_room(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &focused());
    let folder = "a/very/long/folder/".repeat(20);
    shown_diff(&mut h, &format!("{folder}footer.rs"));
    resize(&mut h, NARROW);

    let name = h.bounds("diff-path-name");
    let path = h.bounds("diff-path");
    let whole = shaped_width(&mut h, "footer.rs");
    assert!(
        name.size.width / px(1.0) >= whole,
        "the name is laid out whole: {name:?} for {whole} px"
    );
    assert!(
        name.left() >= path.left() && name.right() <= path.right(),
        "the name is inside its unclipped box: {name:?} in {path:?}"
    );
}

/// Shows `path` in a narrow header and asserts that the name gives up its
/// end: its box is
/// narrower than the whole name and ends inside the path box, where gpui
/// cuts the text to fit and puts `…` at its end. The test platform draws no
/// glyphs, so the `…` itself is not observable here.
fn squeezed_name(h: &mut Harness<'_>, path: &str) {
    shown_diff(h, path);
    resize(h, NARROW);
    let name = h.bounds("diff-path-name");
    let path = h.bounds("diff-path");
    let whole = shaped_width(h, LONG_NAME);
    assert!(
        name.size.width / px(1.0) < whole,
        "the name gives way: {name:?} for {whole} px"
    );
    assert!(
        name.left() >= path.left() && name.right() <= path.right(),
        "the name ends inside its box, not clipped by it: {name:?} in {path:?}"
    );
}

#[gpui::test]
fn a_bare_file_name_too_long_for_the_header_is_cut_short(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &focused());
    squeezed_name(&mut h, LONG_NAME);
}

#[gpui::test]
fn the_folder_gives_way_before_a_long_file_name(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &focused());
    squeezed_name(&mut h, &format!("src/deep/{LONG_NAME}"));
    let folder = h.bounds("diff-path-folder");
    assert!(
        folder.size.width < px(1.0),
        "the folder is gone before the name shrinks: {folder:?}"
    );
}

#[gpui::test]
fn the_toggles_drop_their_labels_in_a_narrow_header(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &focused());
    shown_diff(&mut h, "src/main.rs");
    let toggles = [
        (
            "diff-whitespace",
            shaped_width(&mut h, "include whitespace"),
        ),
        ("diff-highlight", shaped_width(&mut h, "highlight")),
    ];

    resize(&mut h, WIDE);
    for (toggle, label) in toggles {
        let width = h.bounds(toggle).size.width / px(1.0);
        assert!(
            width >= label,
            "{toggle} shows its label when wide: {width} px"
        );
    }

    resize(&mut h, NARROW);
    for (toggle, label) in toggles {
        let width = h.bounds(toggle).size.width / px(1.0);
        assert!(
            width < label,
            "{toggle} drops its label when narrow: {width} px"
        );
    }
    h.click_on("diff-whitespace");
    assert!(
        !header(&mut h, "d1").include_whitespace,
        "the narrow toggle still takes clicks"
    );

    resize(&mut h, WIDE);
    let width = h.bounds("diff-whitespace").size.width / px(1.0);
    assert!(width >= toggles[0].1, "the label comes back: {width} px");
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

// --- Syntax highlighting -------------------------------------------------

const RUST_OLD: &str = "fn main() {\n    let s = \"hi\";\n}\n";
const RUST_NEW: &str = "fn main() {\n    let s = \"hey\"; // x\n}\n";

/// The syntax colours of row `row`'s `half` in tab `tab`'s diff.
fn colors(h: &mut Harness<'_>, tab: &str, row: usize, half: Half) -> Vec<(Range<usize>, Hsla)> {
    let view = diff_view(h, tab);
    h.cx.update(|_, cx| view.read(cx).syntax_colors(row, half))
}

fn highlight_runs(h: &mut Harness<'_>, tab: &str) -> u64 {
    h.root(|root, cx| root.diff_tab_highlight_runs(tab, cx))
        .expect("the diff tab has a view")
}

/// The colour of the span covering `token` in `line`, among `spans`.
fn color_of(spans: &[(Range<usize>, Hsla)], line: &str, token: &str) -> Option<Hsla> {
    let at = line.find(token)?;
    spans
        .iter()
        .find(|(range, _)| range.start <= at && at + token.len() <= range.end)
        .map(|(_, color)| *color)
}

#[gpui::test]
fn a_rust_diff_is_coloured_once_highlighting_lands(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &focused());
    let id = show(&mut h, &diff_tab("d1", "src/main.rs", None));
    answer(&mut h, &id, RUST_OLD, RUST_NEW);
    assert!(header(&mut h, "d1").highlight, "on by default");
    assert_eq!(highlight_runs(&mut h, "d1"), 2, "one run a side");

    let first = colors(&mut h, "d1", 0, Half::Old);
    let keyword = color_of(&first, "fn main() {", "fn").expect("fn is coloured");
    let function = color_of(&first, "fn main() {", "main").expect("main is coloured");
    assert_ne!(keyword, function, "a keyword and a function differ");
    assert_eq!(first, colors(&mut h, "d1", 0, Half::New), "an equal row");

    let changed = "    let s = \"hey\"; // x";
    let new = colors(&mut h, "d1", 1, Half::New);
    let string = color_of(&new, changed, "\"hey\"").expect("the string is coloured");
    let comment = color_of(&new, changed, "// x").expect("the comment is coloured");
    assert_ne!(string, comment);
    let old = colors(&mut h, "d1", 1, Half::Old);
    assert_eq!(
        color_of(&old, "    let s = \"hi\";", "\"hi\""),
        Some(string),
        "the old side too"
    );
    assert!(
        colors(&mut h, "d1", 2, Half::New).is_empty(),
        "a lone brace"
    );
}

#[gpui::test]
fn the_highlight_toggle_hides_the_colours_and_is_saved(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &focused());
    let id = show(&mut h, &diff_tab("d1", "src/main.rs", None));
    answer(&mut h, &id, RUST_OLD, RUST_NEW);
    assert_ne!(
        colors(&mut h, "d1", 0, Half::Old),
        [] as [(std::ops::Range<usize>, gpui::Hsla); 0]
    );

    h.click_on("diff-highlight");
    assert!(!header(&mut h, "d1").highlight);
    assert_eq!(
        colors(&mut h, "d1", 0, Half::Old),
        [] as [(std::ops::Range<usize>, gpui::Hsla); 0]
    );
    assert_eq!(
        colors(&mut h, "d1", 1, Half::New),
        [] as [(std::ops::Range<usize>, gpui::Hsla); 0]
    );
    assert_eq!(saved_ui(&dir)["diff_highlight"], json!(false));
    assert!(
        snapshot_requests(&mut h).is_empty(),
        "nothing fetched again"
    );

    h.click_on("diff-highlight");
    assert!(header(&mut h, "d1").highlight);
    assert_ne!(
        colors(&mut h, "d1", 0, Half::Old),
        [] as [(std::ops::Range<usize>, gpui::Hsla); 0]
    );
    assert_eq!(highlight_runs(&mut h, "d1"), 2, "the landed colours again");
    assert_eq!(saved_ui(&dir)["diff_highlight"], json!(true));
}

#[gpui::test]
fn a_plaintext_diff_has_no_colours(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &focused());
    let id = show(&mut h, &diff_tab("d1", "notes.txt", None));
    h.send(DaemonMessage::FileSnapshot {
        id,
        repo_id: "r1".to_owned(),
        path: "notes.txt".to_owned(),
        against: None,
        old: RUST_OLD.to_owned(),
        new: RUST_NEW.to_owned(),
        language: "plaintext".to_owned(),
        unavailable: None,
        worktree_path: Some(TREE.to_owned()),
    });
    assert_eq!(body(&mut h, "d1"), DiffTabBody::Diff);
    for row in 0..3 {
        assert!(colors(&mut h, "d1", row, Half::Old).is_empty(), "{row}");
        assert!(colors(&mut h, "d1", row, Half::New).is_empty(), "{row}");
    }
    assert_eq!(highlight_runs(&mut h, "d1"), 0, "no run for plaintext");
}

#[gpui::test]
fn the_whitespace_toggle_reuses_the_highlighting(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &focused());
    let id = show(&mut h, &diff_tab("d1", "src/main.rs", None));
    answer(&mut h, &id, RUST_OLD, RUST_NEW);
    assert_eq!(highlight_runs(&mut h, "d1"), 2);
    let before = colors(&mut h, "d1", 1, Half::New);
    assert_ne!(before, [] as [(std::ops::Range<usize>, gpui::Hsla); 0]);

    h.click_on("diff-whitespace");
    assert!(!header(&mut h, "d1").include_whitespace);
    assert_eq!(highlight_runs(&mut h, "d1"), 2, "no run for the same texts");
    assert_eq!(colors(&mut h, "d1", 1, Half::New), before);

    h.send(status(&[], &["src/main.rs"], Some(TREE)));
    let again = one_snapshot_id(&mut h);
    answer(&mut h, &again, RUST_OLD, "fn main() {}\n");
    assert_eq!(highlight_runs(&mut h, "d1"), 3, "new text on one side only");
    assert_ne!(
        colors(&mut h, "d1", 0, Half::New),
        [] as [(std::ops::Range<usize>, gpui::Hsla); 0]
    );
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
    assert_eq!(
        snapshot_requests(&mut h),
        [] as [(
            std::string::String,
            std::string::String,
            std::option::Option<std::string::String>,
            std::option::Option<std::string::String>,
            std::option::Option<std::string::String>
        ); 0]
    );
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
        code: None,
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

// --- The panel's session -------------------------------------------------

/// The source-control panel's section key ids, in order.
fn section_ids(h: &mut Harness<'_>) -> Vec<String> {
    h.root(|root, _| {
        root.source_control_panel()
            .sections
            .into_iter()
            .map(|section| section.id)
            .collect()
    })
}

/// Clicking the member tree's `c.rs` row; returns the `OpenDiffTab`'s id.
fn open_from_the_member_row(h: &mut Harness<'_>) -> String {
    h.click_on(&format!("sc-row-changes-{MEMBER}|c.rs"));
    one_open(h).0
}

/// The menu on the member tree's `c.rs` row, as (label, enabled, separated).
const MEMBER_MENU: [(&str, bool, bool); 3] = [
    ("Open Changes", true, false),
    ("Stage Changes", true, true),
    ("Discard Changes", true, true),
];

#[gpui::test]
fn a_diff_tab_keeps_the_sessions_sections(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = changes_panel(cx, &dir);
    assert_eq!(section_ids(&mut h), [MEMBER.to_owned()], "the member tree");

    let id = open_from_the_member_row(&mut h);
    h.send(opened(&id, "d1"));
    h.send(updated(&diff_tab("d1", "c.rs", None)));
    assert_eq!(active(&mut h).as_deref(), Some("d1"));
    assert_eq!(
        section_ids(&mut h),
        [MEMBER.to_owned()],
        "the panel keeps the session the terminal tab focused"
    );
}

#[gpui::test]
fn a_file_menu_on_the_session_tree_survives_a_diff_tab_opening(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = changes_panel(cx, &dir);
    let id = open_from_the_member_row(&mut h);
    h.right_click_on(&format!("sc-row-changes-{MEMBER}|c.rs"));
    assert_eq!(menu(&mut h), MEMBER_MENU);

    h.send(opened(&id, "d1"));
    h.send(updated(&diff_tab("d1", "c.rs", None)));
    assert_eq!(active(&mut h).as_deref(), Some("d1"));
    assert_eq!(
        menu(&mut h),
        MEMBER_MENU,
        "the menu on the session's tree is still open"
    );
}

#[gpui::test]
fn the_panel_forgets_a_removed_session(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = changes_panel(cx, &dir);
    let id = open_from_the_member_row(&mut h);
    h.send(opened(&id, "d1"));
    h.send(updated(&diff_tab("d1", "c.rs", None)));
    assert_eq!(
        section_ids(&mut h),
        [MEMBER.to_owned()],
        "kept while the diff shows"
    );

    h.send(DaemonMessage::SessionRemoved {
        session_id: "s1".to_owned(),
    });
    assert_eq!(
        section_ids(&mut h),
        ["r1::".to_owned()],
        "the removed session is forgotten, so the panel falls back"
    );
}
