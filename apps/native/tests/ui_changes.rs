//! Source-control changes specs: a failed status, the Staged and Changes
//! buckets and their folded trees, the bucket actions, the row hover
//! buttons and right-click menu, the discard confirm, the pending state,
//! the git-write error banner and the persisted section caret.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

#[expect(dead_code, reason = "each spec file uses its own share of the helper")]
mod support;

use gpui::{Modifiers, TestAppContext, px};
use protocol::{ClientMessage, DaemonMessage};
use rustling_tulip_native::{
    Bucket, ScAction, ScBucketRow, ScButton, ScFileRow, ScSectionRow, ScTreeRow, ToastKind,
};
use serde_json::{Value, json};
use support::{Fixture, Harness, TestDir, repo, session};

const SECTION: &str = "sc-section-r1::";

fn one_repo() -> Fixture {
    Fixture {
        repos: vec![repo("r1", "D:/src/r1")],
        ..Fixture::default()
    }
}

/// A changed file; `from` is the path a rename came from.
fn file(path: &str, status: &str, from: Option<&str>) -> Value {
    json!({ "path": path, "status": status, "from_path": from })
}

fn modified(paths: &[&str]) -> Vec<Value> {
    paths.iter().map(|path| file(path, "M", None)).collect()
}

/// r1's main-tree status.
#[expect(
    clippy::needless_pass_by_value,
    reason = "the fixtures read plainly at their call sites, and json! only borrows"
)]
fn status(staged: Vec<Value>, changes: Vec<Value>) -> DaemonMessage {
    serde_json::from_value(json!({
        "type": "repo_status",
        "repo_id": "r1",
        "index_changes": staged,
        "worktree_changes": changes,
        "worktree_path": null,
    }))
    .expect("status fixture")
}

/// The status requests sent since the last read, as `(repo_id, request_id)`.
fn status_requests(h: &mut Harness<'_>) -> Vec<(String, Option<String>)> {
    h.sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::RepoStatus {
                repo_id,
                request_id,
                ..
            } => Some((repo_id, request_id)),
            _ => None,
        })
        .collect()
}

/// A git write the client sent: its kind, repo, paths and worktree.
type Write = (&'static str, String, Vec<String>, Option<String>);

/// The git writes sent since the last read.
fn writes(h: &mut Harness<'_>) -> Vec<Write> {
    h.sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::StageFiles {
                repo_id,
                paths,
                worktree_path,
            } => Some(("stage", repo_id, paths, worktree_path)),
            ClientMessage::UnstageFiles {
                repo_id,
                paths,
                worktree_path,
            } => Some(("unstage", repo_id, paths, worktree_path)),
            ClientMessage::DiscardChanges {
                repo_id,
                paths,
                worktree_path,
            } => Some(("discard", repo_id, paths, worktree_path)),
            _ => None,
        })
        .collect()
}

fn write(kind: &'static str, paths: &[&str]) -> Write {
    (
        kind,
        "r1".to_owned(),
        paths.iter().map(|path| (*path).to_owned()).collect(),
        None,
    )
}

fn section(h: &mut Harness<'_>) -> ScSectionRow {
    h.root(|root, _| root.source_control_panel())
        .sections
        .into_iter()
        .next()
        .expect("a section")
}

fn bucket(h: &mut Harness<'_>, which: Bucket) -> ScBucketRow {
    section(h)
        .buckets
        .into_iter()
        .find(|bucket| bucket.bucket == which)
        .expect("the bucket shows")
}

fn files(bucket: &ScBucketRow) -> Vec<ScFileRow> {
    bucket
        .rows
        .iter()
        .filter_map(|row| match row {
            ScTreeRow::File(file) => Some(file.clone()),
            ScTreeRow::Folder(_) => None,
        })
        .collect()
}

fn labels(buttons: &[ScButton]) -> Vec<(&'static str, bool)> {
    buttons
        .iter()
        .map(|button| (button.label, button.enabled))
        .collect()
}

fn menu_items(h: &mut Harness<'_>) -> Option<Vec<(&'static str, bool)>> {
    h.root(|root, _| root.sc_file_menu_items())
        .map(|items| labels(&items))
}

/// Moves the mouse onto `selector`, so its row's hover buttons show.
fn hover_on(h: &mut Harness<'_>, selector: &str) {
    let at = h.center(selector);
    h.hover(at, Modifiers::none());
}

fn saved_ui(dir: &TestDir) -> Value {
    serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("native-ui.json")).expect("native-ui.json"),
    )
    .expect("native-ui.json is JSON")
}

/// The panel on r1, with the status request it seeded read off.
fn open_panel<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> (Harness<'a>, Option<String>) {
    let mut h = Harness::with(cx, dir, &one_repo());
    h.click_on("activity-source-control");
    let requests = status_requests(&mut h);
    assert_eq!(requests.len(), 1, "one status request for r1's main tree");
    let id = requests.into_iter().next().and_then(|(_, id)| id);
    (h, id)
}

#[gpui::test]
fn a_failed_status_shows_its_reason_and_raises_no_toast(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = open_panel(cx, &dir);
    assert_eq!(
        id.as_deref(),
        Some("sc-status-1"),
        "the request carries an id"
    );
    assert_eq!(section(&mut h).body.as_deref(), Some("loading…"));

    h.send(DaemonMessage::Error {
        message: "status failed: not a git repository".to_owned(),
        request_id: id,
    });
    let row = section(&mut h);
    assert_eq!(
        row.body.as_deref(),
        Some("couldn't load status: not a git repository")
    );
    assert!(!row.collapsed);
    assert!(
        h.root(|root, _| root.toasts().is_empty()),
        "the section says it, so no toast does"
    );
    assert!(h.bounds("sc-section-body-r1::").size.height > px(0.0));

    h.send(DaemonMessage::Repos {
        repos: one_repo().repos,
    });
    assert!(
        status_requests(&mut h).is_empty(),
        "a failed tree is not asked for again by itself"
    );
}

#[gpui::test]
fn refresh_retries_a_failed_status(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = open_panel(cx, &dir);
    h.send(DaemonMessage::Error {
        message: "status failed: locked".to_owned(),
        request_id: id,
    });

    h.click_on("sc-refresh");
    assert_eq!(
        status_requests(&mut h),
        [("r1".to_owned(), Some("sc-status-2".to_owned()))],
        "Refresh asks again under a new id"
    );
    assert_eq!(
        section(&mut h).body.as_deref(),
        Some("couldn't load status: locked"),
        "the failure stays until the answer"
    );

    h.send(status(Vec::new(), modified(&["a.rs"])));
    let row = section(&mut h);
    assert_eq!(row.body, None, "the answer clears the failure");
    assert_eq!(row.buckets.len(), 1);
}

#[gpui::test]
fn buckets_show_their_counts_and_folded_trees(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = open_panel(cx, &dir);
    h.send(status(
        vec![
            file("src/a.rs", "M", None),
            file("new.rs", "R", Some("old.rs")),
        ],
        vec![
            file("src/b.rs", "M", None),
            file("src/c/d.rs", "A", None),
            file("notes.md", "?", None),
            file("src/a.rs", "M", None),
        ],
    ));
    let row = section(&mut h);
    assert_eq!(
        row.count,
        Some(5),
        "the header keeps the distinct-path count"
    );
    assert_eq!(row.body, None);
    let titles: Vec<(&str, usize)> = row
        .buckets
        .iter()
        .map(|bucket| (bucket.title, bucket.count))
        .collect();
    assert_eq!(titles, [("Staged Changes", 2), ("Changes", 4)]);

    let staged = &row.buckets[0];
    assert_eq!(labels(&staged.actions), [("Unstage all", true)]);
    let changes = &row.buckets[1];
    assert_eq!(
        labels(&changes.actions),
        [("Discard all", true), ("Stage all", true)]
    );
    assert!(changes.actions[0].danger && !changes.actions[1].danger);

    let shape: Vec<String> = changes
        .rows
        .iter()
        .map(|row| match row {
            ScTreeRow::Folder(folder) => format!(
                "{}{} ({})",
                "  ".repeat(folder.depth),
                folder.label,
                folder.files
            ),
            ScTreeRow::File(file) => {
                format!("{}{} {}", "  ".repeat(file.depth), file.status, file.name)
            }
        })
        .collect();
    assert_eq!(
        shape,
        [
            "src (3)",
            "  c (1)",
            "    A d.rs",
            "  M a.rs",
            "  M b.rs",
            "? notes.md"
        ],
        "folders first with their file counts, then files by basename"
    );

    let rename = files(staged)
        .into_iter()
        .find(|file| file.name == "new.rs")
        .expect("the rename row");
    assert_eq!(rename.tooltip, "old.rs → new.rs");
    assert_eq!(rename.status, "R");
    let plain = files(staged)
        .into_iter()
        .find(|file| file.name == "a.rs")
        .expect("a.rs");
    assert_eq!(plain.tooltip, "src/a.rs", "the tooltip is the full path");

    assert!(h.bounds("sc-bucket-staged-r1::").size.height > px(0.0));
    assert!(h.bounds("sc-row-changes-r1::|src/c/d.rs").size.height > px(0.0));
}

#[gpui::test]
fn an_empty_bucket_is_hidden(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = open_panel(cx, &dir);
    h.send(status(Vec::new(), modified(&["a.rs"])));
    let buckets: Vec<Bucket> = section(&mut h)
        .buckets
        .iter()
        .map(|bucket| bucket.bucket)
        .collect();
    assert_eq!(buckets, [Bucket::Changes]);
    assert_eq!(h.bounds("sc-bucket-staged-r1::").size.height, px(0.0));
    assert!(h.bounds("sc-bucket-changes-r1::").size.height > px(0.0));

    h.send(status(modified(&["a.rs"]), Vec::new()));
    let buckets: Vec<Bucket> = section(&mut h)
        .buckets
        .iter()
        .map(|bucket| bucket.bucket)
        .collect();
    assert_eq!(buckets, [Bucket::Staged]);
}

#[gpui::test]
fn stage_all_and_unstage_all_send_every_path(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = open_panel(cx, &dir);
    h.send(status(
        vec![file("new.rs", "R", Some("old.rs"))],
        modified(&["z.rs", "src/a.rs"]),
    ));

    h.click_on("sc-stage-all-r1::");
    assert_eq!(writes(&mut h), [write("stage", &["z.rs", "src/a.rs"])]);

    h.send(status(
        vec![file("new.rs", "R", Some("old.rs"))],
        modified(&["z.rs"]),
    ));
    h.click_on("sc-unstage-all-r1::");
    assert_eq!(
        writes(&mut h),
        [write("unstage", &["old.rs", "new.rs"])],
        "a rename moves both sides"
    );
}

#[gpui::test]
fn the_row_hover_buttons_send_their_message(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = open_panel(cx, &dir);
    let listing = || {
        status(
            vec![file("new.rs", "R", Some("old.rs"))],
            modified(&["src/b.rs"]),
        )
    };
    h.send(listing());
    let staged_row = files(&bucket(&mut h, Bucket::Staged))[0].clone();
    assert_eq!(labels(&staged_row.buttons), [("−", true)]);
    assert_eq!(staged_row.buttons[0].tooltip, Some("Unstage"));
    let changed_row = files(&bucket(&mut h, Bucket::Changes))[0].clone();
    assert_eq!(labels(&changed_row.buttons), [("↺", true), ("+", true)]);
    let tips: Vec<Option<&str>> = changed_row
        .buttons
        .iter()
        .map(|button| button.tooltip)
        .collect();
    assert_eq!(tips, [Some("Discard"), Some("Stage")]);
    assert!(changed_row.buttons[0].danger);

    hover_on(&mut h, "sc-row-staged-r1::|new.rs");
    h.click_on("sc-unstage-r1::|new.rs");
    assert_eq!(writes(&mut h), [write("unstage", &["old.rs", "new.rs"])]);

    h.send(listing());
    hover_on(&mut h, "sc-row-changes-r1::|src/b.rs");
    h.click_on("sc-stage-r1::|src/b.rs");
    assert_eq!(writes(&mut h), [write("stage", &["src/b.rs"])]);

    h.send(listing());
    hover_on(&mut h, "sc-row-changes-r1::|src/b.rs");
    h.click_on("sc-discard-r1::|src/b.rs");
    assert!(writes(&mut h).is_empty(), "a discard asks first");
    let confirm = h
        .root(|root, _| root.discard_confirm_view())
        .expect("the discard confirm");
    assert_eq!(confirm.title, "Discard 1 change");
    assert_eq!(confirm.paths, ["src/b.rs"]);
}

#[gpui::test]
fn the_right_click_menu_stages_and_discards(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = open_panel(cx, &dir);
    let listing = || status(modified(&["s.rs"]), modified(&["c.rs"]));
    h.send(listing());

    h.right_click_on("sc-row-changes-r1::|c.rs");
    assert_eq!(
        menu_items(&mut h),
        Some(vec![("Stage Changes", true), ("Discard Changes", true)])
    );
    let looks: Vec<(bool, bool)> = h
        .root(|root, _| root.sc_file_menu_items())
        .expect("the menu")
        .iter()
        .map(|item| (item.danger, item.separated))
        .collect();
    assert_eq!(
        looks,
        [(false, false), (true, true)],
        "Discard is red, under a separator"
    );
    h.click_on("sc-file-menu-stage");
    assert!(!h.root(|root, _| root.sc_file_menu_open()));
    assert_eq!(writes(&mut h), [write("stage", &["c.rs"])]);

    h.send(listing());
    h.right_click_on("sc-row-changes-r1::|c.rs");
    h.click_on("sc-file-menu-discard");
    assert!(writes(&mut h).is_empty());
    let confirm = h
        .root(|root, _| root.discard_confirm_view())
        .expect("the menu's discard asks first");
    assert_eq!(confirm.paths, ["c.rs"]);
    h.click_on("discard-confirm-discard");
    assert_eq!(writes(&mut h), [write("discard", &["c.rs"])]);

    h.send(listing());
    h.right_click_on("sc-row-staged-r1::|s.rs");
    assert_eq!(menu_items(&mut h), Some(vec![("Unstage Changes", true)]));
    h.keys("escape");
    assert!(!h.root(|root, _| root.sc_file_menu_open()), "Esc closes it");
}

#[gpui::test]
fn the_discard_confirm_lists_the_paths_and_cancel_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = open_panel(cx, &dir);
    h.send(status(Vec::new(), modified(&["a.rs", "src/b.rs"])));

    h.click_on("sc-discard-all-r1::");
    let confirm = h
        .root(|root, _| root.discard_confirm_view())
        .expect("Discard all asks first");
    assert_eq!(confirm.title, "Discard 2 changes");
    assert_eq!(confirm.paths, ["a.rs", "src/b.rs"]);
    assert_eq!(
        confirm.focused, "discard-confirm-cancel",
        "Cancel has the focus"
    );
    assert!(h.bounds("discard-confirm-paths").size.height <= px(200.0));
    assert!(h.bounds("discard-confirm-discard").size.height > px(0.0));

    h.click_on("discard-confirm-cancel");
    assert!(h.root(|root, _| root.discard_confirm_view()).is_none());
    assert!(writes(&mut h).is_empty(), "Cancel sends nothing");

    h.click_on("sc-discard-all-r1::");
    h.keys("escape");
    assert!(
        h.root(|root, _| root.discard_confirm_view()).is_none(),
        "Esc cancels"
    );
    h.click_on("sc-discard-all-r1::");
    h.click_on("discard-confirm-close");
    assert!(
        h.root(|root, _| root.discard_confirm_view()).is_none(),
        "✕ cancels"
    );
    assert!(writes(&mut h).is_empty());
}

/// Session `s1` on r1's main tree, in pane `p1` with the keyboard, and the
/// panel open on its tree with one changed file.
fn session_on_r1<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut fixture = Fixture::single(
        session("s1")
            .members(&[("r1", "main", "D:/src/r1")])
            .build(),
    );
    fixture.repos = one_repo().repos;
    let mut h = Harness::with(cx, dir, &fixture);
    h.answer_scrollback("s1", b"");
    h.click_on("activity-source-control");
    h.send(status(Vec::new(), modified(&["c.rs"])));
    h.sent();
    h
}

#[gpui::test]
fn a_welcome_closes_the_discard_confirm(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = open_panel(cx, &dir);
    h.send(status(Vec::new(), modified(&["a.rs"])));
    h.click_on("sc-discard-all-r1::");
    h.keys("tab");

    h.send(DaemonMessage::Welcome {
        protocol_version: 1,
        supported_versions: vec![1],
    });
    assert!(
        h.root(|root, _| root.discard_confirm_view()).is_none(),
        "a new connection closes the confirm"
    );
    h.keys("enter");
    assert!(writes(&mut h).is_empty(), "Enter sends nothing");
}

#[gpui::test]
fn a_registry_without_the_repo_closes_the_discard_confirm(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = session_on_r1(cx, &dir);
    h.click_on("sc-discard-all-r1::");
    assert!(h.root(|root, _| root.discard_confirm_view()).is_some());

    h.send(DaemonMessage::Repos { repos: Vec::new() });
    assert!(
        h.root(|root, _| root.discard_confirm_view()).is_none(),
        "the confirm goes with its section"
    );
    h.keys("a");
    assert_eq!(h.sent_input("s1"), b"a", "the pane has the keyboard again");
    assert!(writes(&mut h).is_empty());
}

#[gpui::test]
fn a_stale_file_menu_hands_the_keyboard_back_to_the_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = session_on_r1(cx, &dir);
    h.right_click_on("sc-row-changes-r1::|c.rs");
    assert!(h.root(|root, _| root.sc_file_menu_open()));

    h.send(DaemonMessage::Repos { repos: Vec::new() });
    assert!(
        !h.root(|root, _| root.sc_file_menu_open()),
        "the menu goes with its section"
    );
    h.keys("a");
    assert_eq!(h.sent_input("s1"), b"a", "the pane has the keyboard again");
}

#[gpui::test]
fn enter_on_the_danger_button_sends_discard_changes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = open_panel(cx, &dir);
    h.send(status(Vec::new(), modified(&["a.rs", "b.rs"])));
    h.click_on("sc-discard-all-r1::");
    let focused = |h: &mut Harness<'_>| {
        h.root(|root, _| root.discard_confirm_view().map(|view| view.focused))
    };

    h.keys("tab");
    assert_eq!(focused(&mut h), Some("discard-confirm-discard"));
    h.keys("shift-tab");
    assert_eq!(focused(&mut h), Some("discard-confirm-cancel"));
    h.keys("tab");
    h.keys("enter");
    assert!(h.root(|root, _| root.discard_confirm_view()).is_none());
    assert_eq!(writes(&mut h), [write("discard", &["a.rs", "b.rs"])]);
}

#[gpui::test]
fn pending_disables_the_buttons_until_a_status(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = open_panel(cx, &dir);
    let listing = || status(modified(&["s.rs"]), modified(&["c.rs"]));
    h.send(listing());
    h.click_on("sc-stage-all-r1::");
    assert_eq!(writes(&mut h).len(), 1);

    let row = section(&mut h);
    for bucket in &row.buckets {
        assert!(
            bucket.actions.iter().all(|button| !button.enabled),
            "{} actions are disabled",
            bucket.title
        );
        for file in files(bucket) {
            assert!(file.buttons.iter().all(|button| !button.enabled));
        }
    }
    h.click_on("sc-unstage-all-r1::");
    assert!(writes(&mut h).is_empty(), "a disabled action sends nothing");
    h.right_click_on("sc-row-changes-r1::|c.rs");
    assert_eq!(
        menu_items(&mut h),
        Some(vec![("Stage Changes", false), ("Discard Changes", false)])
    );
    h.click_on("sc-file-menu-stage");
    assert!(writes(&mut h).is_empty(), "a disabled item sends nothing");
    h.keys("escape");

    h.send(listing());
    let row = section(&mut h);
    assert!(
        row.buckets
            .iter()
            .all(|bucket| bucket.actions.iter().all(|button| button.enabled)),
        "the status re-enables them"
    );
    h.click_on("sc-unstage-all-r1::");
    assert_eq!(writes(&mut h), [write("unstage", &["s.rs"])]);
}

#[gpui::test]
fn a_git_write_error_shows_a_banner_and_a_toast_and_x_dismisses_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = open_panel(cx, &dir);
    h.send(status(Vec::new(), modified(&["c.rs"])));
    h.click_on("sc-stage-all-r1::");
    h.sent();

    h.send(DaemonMessage::GitWriteError {
        repo_id: "r1".to_owned(),
        operation: "stage".to_owned(),
        error: "index.lock exists".to_owned(),
        worktree_path: None,
    });
    let row = section(&mut h);
    assert_eq!(row.banner.as_deref(), Some("stage: index.lock exists"));
    assert!(
        row.buckets[0].actions.iter().all(|button| button.enabled),
        "the error ends the pending write"
    );
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
            "Git stage failed".to_owned(),
            Some("index.lock exists".to_owned())
        )]
    );
    assert!(h.bounds("sc-banner-r1::").size.height > px(0.0));

    h.send(DaemonMessage::GitWriteError {
        repo_id: "r1".to_owned(),
        operation: "discard".to_owned(),
        error: "permission denied".to_owned(),
        worktree_path: None,
    });
    assert_eq!(
        section(&mut h).banner.as_deref(),
        Some("discard: permission denied"),
        "a new error replaces the banner"
    );

    h.click_on("sc-banner-close-r1::");
    assert_eq!(section(&mut h).banner, None);
}

#[gpui::test]
fn the_section_caret_folds_and_persists(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    {
        let (mut h, _) = open_panel(cx, &dir);
        h.send(status(Vec::new(), modified(&["c.rs"])));
        assert!(!section(&mut h).collapsed);

        h.click_on(SECTION);
        let row = section(&mut h);
        assert!(row.collapsed);
        assert_eq!((row.body, row.buckets.len()), (None, 0), "folded, no body");
        assert_eq!(row.count, Some(1), "the header still counts");
        assert_eq!(
            saved_ui(&dir)["source_control"]["collapsed"]["r1::|changes"],
            json!(true)
        );
    }

    let mut h = Harness::with(cx, &dir, &one_repo());
    h.send(status(Vec::new(), modified(&["c.rs"])));
    assert!(section(&mut h).collapsed, "restored folded");
    h.click_on(SECTION);
    assert!(!section(&mut h).collapsed);
    assert_eq!(
        saved_ui(&dir)["source_control"]["collapsed"]["r1::|changes"],
        json!(false)
    );
}

#[gpui::test]
fn a_clean_tree_starts_folded_and_says_so_when_opened(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = open_panel(cx, &dir);
    h.send(status(Vec::new(), Vec::new()));
    assert!(section(&mut h).collapsed, "a clean tree folds by default");
    h.click_on(SECTION);
    assert_eq!(section(&mut h).body.as_deref(), Some("working tree clean"));
}

#[gpui::test]
fn a_folder_row_toggles_its_files(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = open_panel(cx, &dir);
    h.send(status(Vec::new(), modified(&["src/a.rs", "src/b.rs"])));
    assert_eq!(bucket(&mut h, Bucket::Changes).rows.len(), 3);

    h.click_on("sc-folder-changes-r1::|src");
    let rows = bucket(&mut h, Bucket::Changes).rows;
    assert_eq!(rows.len(), 1, "only the folder row");
    assert!(matches!(&rows[0], ScTreeRow::Folder(folder) if folder.collapsed));

    h.click_on("sc-folder-changes-r1::|src");
    assert_eq!(bucket(&mut h, Bucket::Changes).rows.len(), 3);
    assert_eq!(
        files(&bucket(&mut h, Bucket::Changes))[0].buttons[1].action,
        ScAction::Stage
    );
}
