//! Worktree cleanup-failed dialog specs: the daemon's failure opens a modal
//! with Ignore focused, the retry kills the ticked lockers and retries every
//! folder, Esc ignores, failures queue one per session, and a new connection
//! drops them.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use gpui::TestAppContext;
use protocol::{
    ClientMessage, DaemonMessage, RetryWorktreeTarget, WorktreeCleanupFailure,
    WorktreeLockingProcess,
};
use support::{Harness, PROTOCOL, TestDir};

fn locker(pid: u32, name: &str) -> WorktreeLockingProcess {
    WorktreeLockingProcess {
        pid,
        name: name.to_owned(),
        cmdline: Some(format!("{name} --watch")),
    }
}

fn failure(member: &str, lockers: Vec<WorktreeLockingProcess>) -> WorktreeCleanupFailure {
    WorktreeCleanupFailure {
        member_path: format!("C:/wts/wt.feat-x/{member}"),
        repo_path: format!("C:/src/{member}"),
        reason: "fs::remove_dir_all: access denied".to_owned(),
        locking_processes: lockers,
    }
}

fn fail(h: &mut Harness<'_>, id: &str, label: &str, failures: Vec<WorktreeCleanupFailure>) {
    h.send(DaemonMessage::WorktreeCleanupFailed {
        session_id: id.to_owned(),
        session_label: label.to_owned(),
        failures,
    });
}

fn opened<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = Harness::open(cx, dir);
    h.sent();
    h
}

fn session_of(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.cleanup_failed_session().map(str::to_owned))
}

fn focus(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.cleanup_failed_focus())
}

fn retry_label(h: &mut Harness<'_>) -> String {
    h.root(|root, _| {
        root.cleanup_failed_controls()
            .into_iter()
            .find(|(selector, _)| selector == "cleanup-retry")
            .map(|(_, label)| label)
            .expect("the retry button is shown")
    })
}

fn two_lockers(h: &mut Harness<'_>) {
    fail(
        h,
        "s1",
        "feat/x",
        vec![
            failure("a", vec![locker(10, "node.exe"), locker(20, "code.exe")]),
            failure("b", vec![locker(20, "code.exe")]),
        ],
    );
}

#[gpui::test]
fn cleanup_failed_opens_with_ignore_focused(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    two_lockers(&mut h);

    assert_eq!(session_of(&mut h).as_deref(), Some("s1"));
    assert_eq!(focus(&mut h).as_deref(), Some("cleanup-ignore"));
    assert_eq!(
        h.root(|root, _| root.cleanup_failed_body()).as_deref(),
        Some(
            "Session feat/x stopped, but 2 of its worktree directories are still on disk. On Windows this usually means a child process (a dev server, a build, your editor) still has files open inside it."
        )
    );
    let selectors: Vec<String> = h.root(|root, _| {
        root.cleanup_failed_controls()
            .into_iter()
            .map(|(selector, _)| selector)
            .collect()
    });
    assert_eq!(
        selectors,
        [
            "cleanup-close",
            "cleanup-open-folder-0",
            "cleanup-open-folder-1",
            "cleanup-proc-10",
            "cleanup-proc-20",
            "cleanup-ignore",
            "cleanup-retry",
        ]
    );
    assert_eq!(retry_label(&mut h), "Kill 2 processes & retry");
    assert!(h.bounds("cleanup-ignore").origin.x >= gpui::px(0.0));
    assert!(h.sent().is_empty(), "opening sends nothing");
}

#[gpui::test]
fn kill_and_retry_sends_selected_pids_and_targets(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    two_lockers(&mut h);

    h.click_on("cleanup-proc-20");
    assert_eq!(retry_label(&mut h), "Kill 1 selected & retry");
    h.click_on("cleanup-retry");

    let sent = h.sent();
    let expected_targets = vec![
        RetryWorktreeTarget {
            member_path: "C:/wts/wt.feat-x/a".to_owned(),
            repo_path: "C:/src/a".to_owned(),
        },
        RetryWorktreeTarget {
            member_path: "C:/wts/wt.feat-x/b".to_owned(),
            repo_path: "C:/src/b".to_owned(),
        },
    ];
    assert!(
        matches!(
            sent.as_slice(),
            [ClientMessage::RetryWorktreeCleanup { session_id, kill_pids, targets }]
                if session_id == "s1" && kill_pids == &[10] && targets == &expected_targets
        ),
        "one retry with the ticked pid and both folders: {sent:?}"
    );
    assert!(!h.in_model("cleanup-failed"), "the retry closes it");
}

#[gpui::test]
fn unticking_all_reads_retry_without_killing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    two_lockers(&mut h);

    // From Ignore, Tab wraps through the retry and ✕ to the checkboxes.
    h.keys("tab tab tab tab tab");
    assert_eq!(focus(&mut h).as_deref(), Some("cleanup-proc-10"));
    h.keys("space tab space");
    assert_eq!(focus(&mut h).as_deref(), Some("cleanup-proc-20"));
    assert!(h.root(|root, _| root.cleanup_failed_ticked()).is_empty());
    assert_eq!(retry_label(&mut h), "Retry without killing");

    h.keys("tab tab enter");
    let sent = h.sent();
    assert!(
        matches!(
            sent.as_slice(),
            [ClientMessage::RetryWorktreeCleanup { kill_pids, targets, .. }]
                if kill_pids.is_empty() && targets.len() == 2
        ),
        "a retry that kills nothing: {sent:?}"
    );
}

#[gpui::test]
fn escape_ignores(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    fail(&mut h, "s1", "feat/x", vec![failure("a", vec![])]);
    assert_eq!(retry_label(&mut h), "Retry cleanup");

    h.keys("escape");

    assert!(!h.in_model("cleanup-failed"));
    assert!(h.sent().is_empty(), "ignoring sends nothing");
}

#[gpui::test]
fn queued_failures_shown_one_at_a_time(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    fail(&mut h, "s1", "one", vec![failure("a", vec![])]);
    fail(&mut h, "s2", "two", vec![failure("b", vec![])]);
    // A retry's failure names the session by its id; the first label stays.
    fail(&mut h, "s1", "s1", vec![failure("c", vec![])]);

    assert_eq!(session_of(&mut h).as_deref(), Some("s1"));
    assert!(
        h.root(|root, _| root.cleanup_failed_body())
            .expect("open")
            .starts_with("Session one stopped, but its worktree directory is still on disk.")
    );
    h.click_on("cleanup-ignore");
    assert_eq!(session_of(&mut h).as_deref(), Some("s2"));
    assert_eq!(focus(&mut h).as_deref(), Some("cleanup-ignore"));
    h.click_on("cleanup-close");
    assert!(!h.in_model("cleanup-failed"));
    assert!(!h.in_model("cleanup-close"), "its controls go with it");
    assert!(h.sent().is_empty());
}

#[gpui::test]
fn retry_failure_after_retry_press_shows_original_label(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    fail(&mut h, "s1", "feat/x", vec![failure("a", vec![])]);
    h.click_on("cleanup-retry");
    assert!(!h.in_model("cleanup-failed"), "the retry closes it");
    h.sent();

    // The daemon's failure after a retry names the session by its id.
    fail(&mut h, "s1", "s1", vec![failure("a", vec![])]);

    assert!(h.in_model("cleanup-failed"));
    assert!(
        h.root(|root, _| root.cleanup_failed_body())
            .expect("open")
            .starts_with("Session feat/x stopped, but its worktree directory is still on disk.")
    );
}

#[gpui::test]
fn welcome_clears_the_queue(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    fail(&mut h, "s1", "one", vec![failure("a", vec![])]);
    fail(&mut h, "s2", "two", vec![failure("b", vec![])]);

    h.send(DaemonMessage::Welcome {
        protocol_version: PROTOCOL,
        supported_versions: vec![PROTOCOL],
    });

    assert!(!h.in_model("cleanup-failed"));
}
