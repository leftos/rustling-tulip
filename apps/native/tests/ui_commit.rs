//! Source-control commit box specs: when the box shows, Ctrl+Enter and the
//! button, the pending state, `CommitOk` and a failed commit, the draft
//! across a fold and a reconnect, Esc, and blank drafts.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

#[expect(dead_code, reason = "each spec file uses its own share of the helper")]
mod support;

use gpui::TestAppContext;
use protocol::{ClientMessage, DaemonMessage};
use rustling_tulip_native::{ScKey, ScSectionRow};
use serde_json::{Value, json};
use support::{Fixture, Harness, TestDir, repo, session};

const SECTION: &str = "sc-section-r1::";
const INPUT: &str = "sc-commit-input-r1::";
const BUTTON: &str = "sc-commit-r1::";
const WORKTREE: &str = "C:/wt/r1";

fn one_repo() -> Fixture {
    Fixture {
        repos: vec![repo("r1", "D:/src/r1")],
        ..Fixture::default()
    }
}

/// Session `s1` in pane `p1`, a member of r1 on the worktree `WORKTREE`.
fn worktree_session() -> Fixture {
    let s1 = session("s1").members(&[("r1", "feat", WORKTREE)]).build();
    let mut fixture = Fixture::single(s1);
    fixture.repos = vec![repo("r1", "D:/src/r1")];
    fixture
}

fn main_key() -> ScKey {
    ScKey {
        repo_id: "r1".to_owned(),
        worktree: None,
    }
}

fn modified(paths: &[&str]) -> Vec<Value> {
    paths
        .iter()
        .map(|path| json!({ "path": path, "status": "M", "from_path": null }))
        .collect()
}

/// r1's status from `worktree`, with `staged` and `changes` modified.
fn status_from(worktree: Option<&str>, staged: &[&str], changes: &[&str]) -> DaemonMessage {
    serde_json::from_value(json!({
        "type": "repo_status",
        "repo_id": "r1",
        "index_changes": modified(staged),
        "worktree_changes": modified(changes),
        "worktree_path": worktree,
    }))
    .expect("status fixture")
}

/// r1's main-tree status.
fn status(staged: &[&str], changes: &[&str]) -> DaemonMessage {
    status_from(None, staged, changes)
}

fn commit_ok() -> DaemonMessage {
    DaemonMessage::CommitOk {
        repo_id: "r1".to_owned(),
        sha: "0123456789abcdef0123456789abcdef01234567".to_owned(),
        short_sha: "0123456".to_owned(),
        worktree_path: None,
    }
}

/// The commits sent since the last read, as `(repo, message, worktree)`.
fn commits(h: &mut Harness<'_>) -> Vec<(String, String, Option<String>)> {
    h.sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::CommitRepo {
                repo_id,
                message,
                worktree_path,
            } => Some((repo_id, message, worktree_path)),
            _ => None,
        })
        .collect()
}

fn section(h: &mut Harness<'_>) -> ScSectionRow {
    h.root(|root, _| root.source_control_panel())
        .sections
        .into_iter()
        .next()
        .expect("a section")
}

/// The commit box's button as `(label, enabled)`, when the box shows.
fn button(h: &mut Harness<'_>) -> Option<(&'static str, bool)> {
    section(h)
        .commit
        .map(|commit| (commit.label, commit.enabled))
}

fn draft(h: &mut Harness<'_>, key: &ScKey) -> Option<String> {
    let key = key.clone();
    h.root(move |root, cx| root.sc_commit_input(&key, cx))
}

/// Types `text` into the field that has the keyboard, a space as `space`.
fn type_text(h: &mut Harness<'_>, text: &str) {
    let keys: Vec<String> = text
        .chars()
        .map(|c| {
            if c == ' ' {
                "space".to_owned()
            } else {
                c.to_string()
            }
        })
        .collect();
    h.keys(&keys.join(" "));
}

/// The panel on r1's main tree with `a.rs` staged and `b.rs` changed, and
/// `message` typed into the commit input.
fn staged_with_draft<'a>(cx: &'a mut TestAppContext, dir: &TestDir, message: &str) -> Harness<'a> {
    let mut h = Harness::with(cx, dir, &one_repo());
    h.click_on("activity-source-control");
    h.send(status(&["a.rs"], &["b.rs"]));
    h.click_on(INPUT);
    type_text(&mut h, message);
    h.sent();
    h
}

#[gpui::test]
fn the_box_shows_only_with_something_staged_or_a_draft(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &one_repo());
    h.click_on("activity-source-control");
    h.send(status(&[], &["b.rs"]));
    assert_eq!(button(&mut h), None, "nothing staged, no draft");

    h.send(status(&["a.rs"], &["b.rs"]));
    assert_eq!(
        button(&mut h),
        Some(("Commit", false)),
        "staged, but no message yet"
    );
    let first = h.bounds(INPUT);

    h.click_on(INPUT);
    type_text(&mut h, "wip");
    assert_eq!(button(&mut h), Some(("Commit", true)));
    assert!(
        h.bounds(BUTTON).top() > first.top(),
        "the button sits under the input"
    );

    h.send(status(&[], &["b.rs"]));
    assert_eq!(
        button(&mut h),
        Some(("Commit", false)),
        "the draft keeps the box up with nothing staged"
    );

    h.keys("backspace backspace backspace");
    assert_eq!(draft(&mut h, &main_key()).as_deref(), Some(""));
    assert_eq!(button(&mut h), None, "no draft and nothing staged");
}

#[gpui::test]
fn ctrl_enter_sends_the_trimmed_message_and_the_worktree(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &worktree_session());
    h.click_on("activity-source-control");
    h.send(status_from(Some(WORKTREE), &["a.rs"], &[]));
    let input = format!("sc-commit-input-r1::{WORKTREE}");
    h.click_on(&input);
    type_text(&mut h, "  fix: it  ");
    h.sent();

    h.keys("ctrl-enter");
    assert_eq!(
        commits(&mut h),
        [(
            "r1".to_owned(),
            "fix: it".to_owned(),
            Some(WORKTREE.to_owned())
        )]
    );

    h.keys("ctrl-enter");
    assert!(
        commits(&mut h).is_empty(),
        "a commit out takes no second one"
    );
}

#[gpui::test]
fn while_committing_the_button_reads_committing_and_the_input_is_read_only(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let mut h = staged_with_draft(cx, &dir, "fix");
    h.click_on(BUTTON);
    assert_eq!(
        commits(&mut h),
        [("r1".to_owned(), "fix".to_owned(), None)],
        "the button commits too"
    );
    assert_eq!(button(&mut h), Some(("Committing…", false)));
    let staged = section(&mut h)
        .buckets
        .into_iter()
        .next()
        .expect("the Staged bucket");
    assert!(
        staged.actions.iter().all(|action| !action.enabled),
        "the other writes wait too"
    );

    h.click_on(INPUT);
    type_text(&mut h, "x");
    assert_eq!(
        draft(&mut h, &main_key()).as_deref(),
        Some("fix"),
        "the input takes no typing"
    );

    h.send(status(&[], &["b.rs"]));
    assert_eq!(
        button(&mut h),
        Some(("Committing…", false)),
        "the status the commit broadcasts does not end it"
    );
}

#[gpui::test]
fn commit_ok_clears_the_box(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = staged_with_draft(cx, &dir, "fix");
    h.keys("ctrl-enter");
    assert_eq!(commits(&mut h).len(), 1);
    h.send(status(&[], &["b.rs"]));

    h.send(commit_ok());
    assert_eq!(draft(&mut h, &main_key()).as_deref(), Some(""));
    assert_eq!(button(&mut h), None, "nothing staged and no draft");

    h.click_on(SECTION);
    h.click_on(SECTION);
    h.send(status(&["b.rs"], &[]));
    h.click_on(INPUT);
    type_text(&mut h, "next");
    assert_eq!(
        draft(&mut h, &main_key()).as_deref(),
        Some("next"),
        "the input takes typing again"
    );
    assert_eq!(button(&mut h), Some(("Commit", true)));
}

#[gpui::test]
fn a_commit_error_keeps_the_draft_and_shows_the_banner(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = staged_with_draft(cx, &dir, "fix");
    h.keys("ctrl-enter");
    assert_eq!(commits(&mut h).len(), 1);

    h.send(DaemonMessage::GitWriteError {
        repo_id: "r1".to_owned(),
        operation: "commit".to_owned(),
        error: "git commit failed: pre-commit hook rejected".to_owned(),
        worktree_path: None,
    });
    assert_eq!(draft(&mut h, &main_key()).as_deref(), Some("fix"));
    assert_eq!(
        section(&mut h).banner.as_deref(),
        Some("commit: git commit failed: pre-commit hook rejected")
    );
    assert_eq!(button(&mut h), Some(("Commit", true)), "ready to retry");
    assert!(h.bounds("sc-banner-r1::").top() > h.bounds(INPUT).top());
}

#[gpui::test]
fn a_draft_survives_folding_the_section_and_a_welcome(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = staged_with_draft(cx, &dir, "wip");

    h.click_on(SECTION);
    assert!(section(&mut h).collapsed);
    assert_eq!(button(&mut h), None, "a folded section shows no box");
    h.click_on(SECTION);
    assert_eq!(draft(&mut h, &main_key()).as_deref(), Some("wip"));
    assert_eq!(button(&mut h), Some(("Commit", true)));

    h.send(DaemonMessage::Welcome {
        protocol_version: 1,
        supported_versions: vec![1],
    });
    h.send(status(&["a.rs"], &["b.rs"]));
    assert_eq!(draft(&mut h, &main_key()).as_deref(), Some("wip"));
    assert_eq!(button(&mut h), Some(("Commit", true)));
}

#[gpui::test]
fn esc_returns_the_keyboard_to_the_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &worktree_session());
    h.click_on("activity-source-control");
    h.send(status_from(Some(WORKTREE), &["a.rs"], &[]));
    h.click_on(&format!("sc-commit-input-r1::{WORKTREE}"));
    type_text(&mut h, "a");
    h.sent_input("s1");

    h.keys("escape");
    h.keys("x");
    assert_eq!(h.sent_input("s1"), b"x", "the pane has the keyboard");
    let key = ScKey {
        repo_id: "r1".to_owned(),
        worktree: Some(WORKTREE.to_owned()),
    };
    assert_eq!(draft(&mut h, &key).as_deref(), Some("a"));
}

/// The panel on `worktree_session`'s worktree with `a.rs` staged and `b.rs`
/// changed, and `message` typed into the commit input; the pane's input so
/// far is read.
fn worktree_draft<'a>(cx: &'a mut TestAppContext, dir: &TestDir, message: &str) -> Harness<'a> {
    let mut h = Harness::with(cx, dir, &worktree_session());
    h.click_on("activity-source-control");
    h.send(status_from(Some(WORKTREE), &["a.rs"], &["b.rs"]));
    h.click_on(&format!("sc-commit-input-r1::{WORKTREE}"));
    type_text(&mut h, message);
    h.sent();
    h.sent_input("s1");
    h
}

#[gpui::test]
fn commit_ok_with_nothing_staged_hands_the_keyboard_to_the_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = worktree_draft(cx, &dir, "fix");
    h.keys("ctrl-enter");
    assert_eq!(commits(&mut h).len(), 1);
    h.send(status_from(Some(WORKTREE), &[], &["b.rs"]));

    h.send(DaemonMessage::CommitOk {
        repo_id: "r1".to_owned(),
        sha: "0123456789abcdef0123456789abcdef01234567".to_owned(),
        short_sha: "0123456".to_owned(),
        worktree_path: Some(WORKTREE.to_owned()),
    });
    assert_eq!(button(&mut h), None, "the box hid");
    h.keys("x");
    assert_eq!(h.sent_input("s1"), b"x", "the pane has the keyboard");
}

#[gpui::test]
fn folding_the_section_hands_the_keyboard_to_the_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = worktree_draft(cx, &dir, "wip");
    h.click_on(&format!("sc-section-r1::{WORKTREE}"));
    assert!(section(&mut h).collapsed);

    h.keys("x");
    assert_eq!(h.sent_input("s1"), b"x", "the pane has the keyboard");
}

#[gpui::test]
fn emptying_the_draft_with_nothing_staged_hands_the_keyboard_to_the_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &worktree_session());
    h.click_on("activity-source-control");
    h.send(status_from(Some(WORKTREE), &["a.rs"], &["b.rs"]));
    h.click_on(&format!("sc-commit-input-r1::{WORKTREE}"));
    type_text(&mut h, "ab");
    h.send(status_from(Some(WORKTREE), &[], &["b.rs"]));
    assert_eq!(
        button(&mut h),
        Some(("Commit", false)),
        "the draft keeps the box up"
    );
    h.sent_input("s1");

    h.keys("backspace backspace");
    assert_eq!(button(&mut h), None, "the box hid");
    h.keys("x");
    assert_eq!(h.sent_input("s1"), b"x", "the pane has the keyboard");
}

/// The Stashes part's push row as `(label, enabled)`.
fn stash_push(h: &mut Harness<'_>) -> Option<(&'static str, bool)> {
    section(h)
        .stashes
        .and_then(|part| part.push)
        .map(|push| (push.label, push.enabled))
}

fn stash_pushes(h: &mut Harness<'_>) -> usize {
    h.sent()
        .into_iter()
        .filter(|msg| matches!(msg, ClientMessage::StashPush { .. }))
        .count()
}

#[gpui::test]
fn the_stash_push_waits_while_a_commit_is_out(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = staged_with_draft(cx, &dir, "fix");
    h.click_on("sc-stashes-r1::");
    assert_eq!(stash_push(&mut h), Some(("Stash", true)));

    h.click_on(BUTTON);
    assert_eq!(commits(&mut h).len(), 1);
    assert_eq!(
        stash_push(&mut h),
        Some(("Stash", false)),
        "a commit out disables the push"
    );
    h.click_on("sc-stash-push-r1::");
    h.click_on("sc-stash-input-r1::");
    h.keys("enter");
    assert_eq!(
        stash_pushes(&mut h),
        0,
        "neither the button nor Enter pushes"
    );

    h.send(commit_ok());
    assert_eq!(stash_push(&mut h), Some(("Stash", true)));
}

#[gpui::test]
fn a_blank_draft_does_not_send(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = staged_with_draft(cx, &dir, "   ");
    assert_eq!(button(&mut h), Some(("Commit", false)));

    h.keys("ctrl-enter");
    h.click_on(BUTTON);
    assert!(
        commits(&mut h).is_empty(),
        "a whitespace-only draft sends nothing"
    );

    h.keys("enter");
    h.keys("ctrl-enter");
    assert!(commits(&mut h).is_empty(), "nor does a blank line");
}
