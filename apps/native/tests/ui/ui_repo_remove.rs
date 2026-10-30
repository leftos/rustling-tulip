//! Repo/workspace Remove specs: the container menu's last row arms on its
//! first click and removes on the second; with live sessions it opens a
//! dialog that lists them, Cancel focused first, whose Stop and remove stops
//! each session and then unregisters the container. The dialog stays open
//! until the daemon's list no longer holds the level, and a refusal is just
//! the generic error toast.

use crate::support;

use gpui::TestAppContext;
use protocol::{ClientMessage, DaemonMessage, PresetTarget};
use support::{Fixture, Harness, TestDir, pane, repo, session, tab, workspace};

/// Repo `r1`, registered, with nothing running in it.
fn empty_repo() -> Fixture {
    Fixture {
        repos: vec![repo("r1", "C:/r1")],
        tabs: vec![tab("t1", &pane("p1", None))],
        ..Fixture::default()
    }
}

/// Repo `r1` with `s1` live in it, in pane `p1` of tab `t1`.
fn live_repo() -> Fixture {
    let mut fixture = Fixture::single(session("s1").in_repo("r1").build());
    fixture.repos = vec![repo("r1", "C:/r1")];
    fixture
}

fn armed(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.repo_remove_armed())
}

fn armed_label(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.repo_remove_armed_label())
}

fn dialog_open(h: &mut Harness<'_>) -> bool {
    h.root(|root, _| root.repo_remove_open())
}

fn title(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.repo_remove_title())
}

fn subject(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.repo_remove_subject())
}

fn session_rows(h: &mut Harness<'_>) -> Vec<String> {
    h.root(|root, _| root.repo_remove_session_rows())
}

fn buttons(h: &mut Harness<'_>) -> Vec<(String, String)> {
    h.root(|root, _| root.repo_remove_buttons())
}

fn focus(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.repo_remove_focus())
}

fn pairs(expected: &[(&str, &str)]) -> Vec<(String, String)> {
    expected
        .iter()
        .map(|(selector, label)| ((*selector).to_owned(), (*label).to_owned()))
        .collect()
}

/// Opens the container menu of `container` and clicks its Remove row.
fn click_remove(h: &mut Harness<'_>, container: &str) {
    h.right_click_on(container);
    // Opening the menu asks for the container's presets; drain that request.
    h.sent();
    h.click_on("container-menu-remove");
}

#[gpui::test]
fn a_container_without_live_sessions_removes_on_its_second_click(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &empty_repo());
    h.sent();

    click_remove(&mut h, "container-repo:r1");
    assert_eq!(
        armed(&mut h),
        Some("repo:r1".to_owned()),
        "the first click arms"
    );
    assert_eq!(
        armed_label(&mut h),
        Some("Confirm: remove repo…".to_owned()),
        "and the row says so"
    );
    assert!(!dialog_open(&mut h), "nothing to stop, so no dialog");
    assert!(h.sent().is_empty(), "arming sends nothing");

    h.click_on("container-menu-remove");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::RemoveRepo { repo_id }] if repo_id == "r1"),
        "the second click removes: sent {sent:?}"
    );
    assert_eq!(armed(&mut h), None, "and the arm is cleared");
    assert!(!dialog_open(&mut h));
    assert!(
        !h.root(|root, _| root.container_menu_open()),
        "the menu gave way"
    );
}

#[gpui::test]
fn a_live_session_opens_the_dialog_and_stop_and_remove_stops_then_removes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &live_repo());
    h.sent();

    click_remove(&mut h, "container-repo:r1");
    assert!(
        dialog_open(&mut h),
        "a live session asks before it is ended"
    );
    assert_eq!(armed(&mut h), None, "the row did not arm");
    assert_eq!(title(&mut h).as_deref(), Some("Remove repo?"));
    assert_eq!(subject(&mut h).as_deref(), Some("r1"));
    assert_eq!(session_rows(&mut h), ["s1 (claude)"]);
    assert_eq!(focus(&mut h).as_deref(), Some("repo-remove-cancel"));
    assert_eq!(
        buttons(&mut h),
        pairs(&[
            ("repo-remove-cancel", "Cancel"),
            ("repo-remove-anyway", "Remove anyway"),
            ("repo-remove-stop-and-remove", "Stop and remove"),
        ])
    );
    assert!(
        !h.root(|root, _| root.container_menu_open()),
        "the menu gave way to the dialog"
    );
    assert!(h.sent().is_empty(), "opening it sends nothing");

    h.keys("escape");
    assert!(!dialog_open(&mut h), "Esc cancels");

    click_remove(&mut h, "container-repo:r1");
    h.click_on("repo-remove-stop-and-remove");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [
            ClientMessage::StopSession { session_id, cleanup },
            ClientMessage::RemoveRepo { repo_id },
        ] if session_id == "s1" && cleanup.is_empty() && repo_id == "r1"),
        "stop, then remove at once: sent {sent:?}"
    );
    assert!(
        dialog_open(&mut h),
        "the dialog waits for the daemon's list"
    );

    h.send(DaemonMessage::Repos { repos: Vec::new() });
    assert!(
        !dialog_open(&mut h),
        "the level left the registry, so the dialog closes"
    );
    assert_eq!(armed(&mut h), None);
}

#[gpui::test]
fn remove_anyway_unregisters_without_stopping_anything(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &live_repo());
    h.sent();

    click_remove(&mut h, "container-repo:r1");
    h.click_on("repo-remove-anyway");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::RemoveRepo { repo_id }] if repo_id == "r1"),
        "the removal alone: sent {sent:?}"
    );
    assert!(dialog_open(&mut h), "and the dialog waits too");
}

#[gpui::test]
fn a_refused_removal_toasts_and_leaves_the_dialog_open(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &live_repo());
    h.sent();

    click_remove(&mut h, "container-repo:r1");
    h.click_on("repo-remove-stop-and-remove");
    h.sent();

    h.send(DaemonMessage::Error {
        message: "the registry is read-only".to_owned(),
        request_id: None,
        code: None,
    });
    assert!(dialog_open(&mut h), "a refusal leaves it open");
    let toasts: Vec<String> = h.root(|root, _| {
        root.toasts()
            .iter()
            .map(|toast| toast.title.clone())
            .collect()
    });
    assert_eq!(
        toasts,
        ["Daemon error"],
        "the generic toast a refusal without a request id raises"
    );
}

#[gpui::test]
fn a_refusal_drops_the_armed_row_and_toasts(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &empty_repo());
    h.sent();

    click_remove(&mut h, "container-repo:r1");
    assert_eq!(
        armed(&mut h),
        Some("repo:r1".to_owned()),
        "the row waits for its second click"
    );
    h.send(DaemonMessage::Error {
        message: "the registry is read-only".to_owned(),
        request_id: None,
        code: None,
    });
    assert_eq!(armed(&mut h), None, "a refusal drops the arm");
    let toasts: Vec<String> = h.root(|root, _| {
        root.toasts()
            .iter()
            .map(|toast| toast.title.clone())
            .collect()
    });
    assert_eq!(
        toasts,
        ["Daemon error"],
        "the generic toast a refusal without a request id raises"
    );
    assert!(!dialog_open(&mut h), "and no dialog was ever opened");
    assert_eq!(
        preset_labels(&mut h),
        ["Launch preset… (loading)"],
        "the refusal is not taken as the loading preset list's failure"
    );

    let request_id = Some(h.preset_request());
    h.send(DaemonMessage::Presets {
        target: PresetTarget::Repo {
            repo_id: "r1".to_owned(),
        },
        entries: Vec::new(),
        request_id,
    });
    assert_eq!(
        preset_labels(&mut h),
        ["Launch preset… (none defined)"],
        "the list with the menu's id still fills the rows"
    );
}

fn preset_labels(h: &mut Harness<'_>) -> Vec<String> {
    h.root(|root, _| root.container_menu_labels())
        .into_iter()
        .filter(|label| label.starts_with("Launch preset"))
        .collect()
}

#[gpui::test]
fn a_workspace_row_says_workspace_and_removes_the_workspace(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        repos: vec![repo("r1", "C:/r1")],
        workspaces: vec![workspace("w1", &["r1"])],
        tabs: vec![tab("t1", &pane("p1", None))],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.sent();

    click_remove(&mut h, "container-ws:w1");
    assert_eq!(armed(&mut h), Some("ws:w1".to_owned()));
    assert_eq!(
        armed_label(&mut h),
        Some("Confirm: remove workspace…".to_owned())
    );

    h.click_on("container-menu-remove");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::RemoveWorkspace { workspace_id }] if workspace_id == "w1"),
        "sent {sent:?}"
    );
    assert_eq!(armed(&mut h), None);
}

#[gpui::test]
fn a_live_workspace_session_asks_about_the_workspace(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        repos: vec![repo("r1", "C:/r1")],
        workspaces: vec![workspace("w1", &["r1"])],
        sessions: vec![session("s1").in_workspace("w1").build()],
        tabs: vec![tab("t1", &pane("p1", Some("s1")))],
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.sent();

    click_remove(&mut h, "container-ws:w1");
    assert!(dialog_open(&mut h));
    assert_eq!(title(&mut h).as_deref(), Some("Remove workspace?"));
    assert_eq!(subject(&mut h).as_deref(), Some("w1"));
    assert_eq!(session_rows(&mut h), ["s1 (claude)"]);

    h.click_on("repo-remove-cancel");
    assert!(!dialog_open(&mut h), "Cancel closes it");
    assert!(h.sent().is_empty(), "and sends nothing");
}

#[gpui::test]
fn dialog_keys_never_reach_the_terminal(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &live_repo());
    h.answer_scrollback("s1", b"");
    h.keys("a");
    assert_eq!(h.sent_input("s1"), b"a", "the pane has the keyboard");

    click_remove(&mut h, "container-repo:r1");
    h.keys("a tab");
    assert!(h.sent_input("s1").is_empty(), "the dialog has the keyboard");
}
