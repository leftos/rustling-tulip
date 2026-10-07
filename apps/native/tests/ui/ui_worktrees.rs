//! Worktrees specs: the Settings Worktrees tab's path, Browse, Save, Reset
//! and override line, and the Manage worktrees modal — its snapshot rows,
//! delete confirms, bulk delete, and Launch session here into a spawn dialog
//! held on the group's repo and pinned to its worktree.

#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use gpui::{Modifiers, TestAppContext};
use protocol::{ClientMessage, DaemonMessage, RootWorktreeEntry, SpawnTarget};
use rustling_tulip_native::WorktreesButton;
use serde_json::json;
use support::{Fixture, Harness, PROTOCOL, TestDir, repo, session};

/// `s1` in `r1`, alone in pane `p1`; repos `r1` and `r2`.
fn fixture() -> Fixture {
    let mut fixture = Fixture::single(session("s1").in_repo("r1").build());
    fixture.repos = vec![repo("r1", "C:/r1"), repo("r2", "C:/r2")];
    fixture
}

fn opened<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = Harness::with(cx, dir, &fixture());
    h.answer_scrollback("s1", b"");
    h.sent();
    h
}

fn root_changed(h: &mut Harness<'_>, root: &str, is_override: bool) {
    h.send(DaemonMessage::WorktreesRootChanged {
        root: root.to_owned(),
        is_override,
    });
}

/// Settings on its Worktrees tab.
fn worktrees_tab(h: &mut Harness<'_>) {
    h.keys("ctrl-,");
    h.click_on("settings-tab-worktrees");
}

fn open_manager(h: &mut Harness<'_>) {
    worktrees_tab(h);
    h.click_on("settings-worktrees-open-manager");
}

fn type_text(h: &mut Harness<'_>, text: &str) {
    let keys: Vec<String> = text.chars().map(|c| c.to_string()).collect();
    h.keys(&keys.join(" "));
}

fn settings_focus(h: &mut Harness<'_>) -> Option<&'static str> {
    let root = h.root.clone();
    h.cx.update(|window, cx| root.read(cx).settings_focus(window))
}

fn path(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.worktrees_path())
}

fn save(h: &mut Harness<'_>) -> (&'static str, bool) {
    h.root(|root, _| root.worktrees_save_button())
}

fn active_line(h: &mut Harness<'_>) -> String {
    h.root(|root, _| root.worktrees_active_line())
}

/// The paths the client asked the daemon to set as the root.
fn root_sets(sent: &[ClientMessage]) -> Vec<Option<String>> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::SetWorktreesRoot { path } => Some(path.clone()),
            _ => None,
        })
        .collect()
}

fn deletes(sent: &[ClientMessage]) -> Vec<String> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::DeleteWorktreeAt { path } => Some(path.clone()),
            _ => None,
        })
        .collect()
}

fn inspects(sent: &[ClientMessage]) -> usize {
    sent.iter()
        .filter(|msg| matches!(msg, ClientMessage::InspectWorktreesRoot))
        .count()
}

/// A group at `path` named `anchor`, launching into `r1` at
/// `<path>/r1`.
fn entry(path: &str, anchor: &str, status: &str) -> RootWorktreeEntry {
    serde_json::from_value(json!({
        "path": path, "anchor": anchor, "branch_slug": "feat",
        "members": [], "status": { "kind": status }, "session_id": null,
        "size_bytes": null, "last_modified_unix": null,
        "launch": { "kind": "single", "repo_id": "r1", "branch": "feat/x",
            "worktree_path": format!("{path}/r1") },
    }))
    .expect("root entry fixture")
}

fn snapshot(h: &mut Harness<'_>, entries: Vec<RootWorktreeEntry>) {
    h.send(DaemonMessage::WorktreesRootSnapshot {
        root: "C:/wt".to_owned(),
        is_override: false,
        entries,
    });
}

fn button(h: &mut Harness<'_>, selector: &str) -> WorktreesButton {
    h.root(|root, _| root.worktrees_manager_buttons())
        .into_iter()
        .find(|button| button.selector == selector)
        .unwrap_or_else(|| panic!("no button {selector}"))
}

/// A group at `path` whose launch names `repo_id`, registered or not.
fn entry_into(path: &str, repo_id: &str, status: &str) -> RootWorktreeEntry {
    serde_json::from_value(json!({
        "path": path, "anchor": "", "branch_slug": "feat",
        "members": [], "status": { "kind": status }, "session_id": null,
        "size_bytes": null, "last_modified_unix": null,
        "launch": { "kind": "single", "repo_id": repo_id, "branch": "feat/x",
            "worktree_path": format!("{path}/r1") },
    }))
    .expect("root entry fixture")
}

/// Every toast shown, as its title and detail.
fn toasts(h: &mut Harness<'_>) -> Vec<(String, Option<String>)> {
    h.root(|root, _| {
        root.toasts()
            .iter()
            .map(|toast| (toast.title.clone(), toast.detail.clone()))
            .collect()
    })
}

fn manager_open(h: &mut Harness<'_>) -> bool {
    h.root(|root, _| root.worktrees_manager_open())
}

fn daemon_error(h: &mut Harness<'_>) {
    h.send(DaemonMessage::Error {
        message: "worktree delete failed".to_owned(),
        request_id: None,
    });
}

#[gpui::test]
fn worktrees_tab_saves_override_and_shows_saved_after_echo(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    root_changed(&mut h, "C:/default", false);
    worktrees_tab(&mut h);
    assert_eq!(
        active_line(&mut h),
        "Active: C:/default — default (env or platform fallback)"
    );
    assert_eq!(
        path(&mut h).as_deref(),
        Some(""),
        "the field holds the override only"
    );
    assert_eq!(save(&mut h), ("Save", false));
    h.click_on("settings-worktrees-root-input");
    type_text(&mut h, "D:/wt");
    assert_eq!(path(&mut h).as_deref(), Some("D:/wt"));
    assert_eq!(save(&mut h), ("Save", true));
    h.click_on("settings-worktrees-root-save");
    assert_eq!(root_sets(&h.sent()), [Some("D:/wt".to_owned())]);
    assert_eq!(save(&mut h).0, "Save", "not Saved before the echo");
    root_changed(&mut h, "D:/wt", true);
    assert_eq!(save(&mut h), ("Saved", false));
    assert_eq!(active_line(&mut h), "Active: D:/wt — user override");
    assert!(h.root(|root, _| root.worktrees_reset_enabled()));
}

#[gpui::test]
fn empty_draft_saves_none(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    root_changed(&mut h, "D:/wt", true);
    worktrees_tab(&mut h);
    assert_eq!(path(&mut h).as_deref(), Some("D:/wt"));
    h.click_on("settings-worktrees-root-input");
    h.keys("ctrl-a backspace");
    assert_eq!(path(&mut h).as_deref(), Some(""));
    h.keys("enter");
    assert_eq!(root_sets(&h.sent()), [None], "Enter saves; empty clears");
    root_changed(&mut h, "C:/default", false);
    assert_eq!(save(&mut h), ("Saved", false));
    assert_eq!(path(&mut h).as_deref(), Some(""));
}

#[gpui::test]
fn reset_disabled_without_override(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    root_changed(&mut h, "C:/default", false);
    worktrees_tab(&mut h);
    assert!(!h.root(|root, _| root.worktrees_reset_enabled()));
    let mut ring = Vec::new();
    for _ in 0..3 {
        h.keys("tab");
        ring.push(settings_focus(&mut h));
    }
    assert_eq!(
        ring,
        [
            Some("settings-worktrees-root-input"),
            Some("settings-worktrees-root-browse"),
            Some("settings-worktrees-open-manager"),
        ],
        "disabled Save and Reset are out of the ring"
    );
    h.click_on("settings-worktrees-root-reset");
    assert!(
        root_sets(&h.sent()).is_empty(),
        "a disabled Reset sends nothing"
    );
    root_changed(&mut h, "D:/wt", true);
    h.click_on("settings-worktrees-root-reset");
    assert_eq!(root_sets(&h.sent()), [None]);
    assert_eq!(path(&mut h).as_deref(), Some(""));
}

#[gpui::test]
fn browse_fills_the_path_from_the_picker(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    root_changed(&mut h, "C:/default", false);
    worktrees_tab(&mut h);
    h.set_picked_folder(Some("E:/trees"));
    h.click_on("settings-worktrees-root-browse");
    assert_eq!(h.folder_asks(), 1);
    assert_eq!(path(&mut h).as_deref(), Some("E:/trees"));
    assert_eq!(save(&mut h), ("Save", true));
}

#[gpui::test]
fn two_quick_saves_show_saved_only_after_the_last_echo(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    root_changed(&mut h, "C:/default", false);
    worktrees_tab(&mut h);
    h.click_on("settings-worktrees-root-input");
    type_text(&mut h, "D:/one");
    h.keys("enter");
    h.click_on("settings-worktrees-root-input");
    h.keys("ctrl-a backspace");
    type_text(&mut h, "D:/two");
    h.keys("enter");
    assert_eq!(
        root_sets(&h.sent()),
        [Some("D:/one".to_owned()), Some("D:/two".to_owned())]
    );
    root_changed(&mut h, "D:/one", true);
    assert_eq!(save(&mut h).0, "Save", "one save still owes its echo");
    assert_eq!(path(&mut h).as_deref(), Some("D:/two"), "the field keeps B");
    root_changed(&mut h, "D:/two", true);
    assert_eq!(save(&mut h), ("Saved", false));
    assert_eq!(path(&mut h).as_deref(), Some("D:/two"));
    assert_eq!(active_line(&mut h), "Active: D:/two — user override");
}

#[gpui::test]
fn another_clients_root_change_does_not_overwrite_an_edited_field(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    root_changed(&mut h, "C:/default", false);
    worktrees_tab(&mut h);
    root_changed(&mut h, "D:/first", true);
    assert_eq!(
        path(&mut h).as_deref(),
        Some("D:/first"),
        "an untouched field follows the daemon"
    );
    h.click_on("settings-worktrees-root-input");
    h.keys("ctrl-a backspace");
    type_text(&mut h, "D:/mine");
    root_changed(&mut h, "E:/other", true);
    assert_eq!(path(&mut h).as_deref(), Some("D:/mine"), "the edit stands");
    assert_eq!(
        save(&mut h).0,
        "Save",
        "no Saved for a change it did not send"
    );
    assert_eq!(active_line(&mut h), "Active: E:/other — user override");
    assert!(root_sets(&h.sent()).is_empty(), "nothing was sent");
}

#[gpui::test]
fn picker_result_after_settings_closed_is_dropped(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    root_changed(&mut h, "D:/wt", true);
    worktrees_tab(&mut h);
    h.set_picked_folder(Some("E:/late"));
    let at = h.center("settings-worktrees-root-browse");
    // The click is not pumped, so the picker's answer is still on the
    // executor when Settings closes.
    h.cx.simulate_click(at, Modifiers::none());
    h.keys("escape");
    assert_eq!(h.folder_asks(), 1);
    assert!(
        !h.root(|root, _| root.settings_open()),
        "Settings closed first"
    );
    assert_eq!(path(&mut h), None, "the tab went with Settings");
    worktrees_tab(&mut h);
    assert_eq!(
        path(&mut h).as_deref(),
        Some("D:/wt"),
        "a fresh tab reads the root again"
    );
}

#[gpui::test]
fn manager_requests_snapshot_and_lists_rows_titled_by_group_name(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    root_changed(&mut h, "C:/wt", false);
    open_manager(&mut h);
    assert!(manager_open(&mut h));
    assert_eq!(inspects(&h.sent()), 1);
    assert_eq!(
        h.root(|root, _| root.worktrees_manager_lines()),
        ["Root: C:/wt — default", "Scanning worktrees root…"]
    );
    assert!(!button(&mut h, "worktrees-manager-refresh").enabled);
    let mut group = entry("C:/wt/wt.feat/app", "App", "detached");
    group.size_bytes = Some(2048);
    group.session_id = Some("s9".to_owned());
    snapshot(&mut h, vec![group]);
    let rows = h.root(|root, _| root.worktrees_manager_rows());
    assert_eq!(rows[0].title, "App");
    assert_eq!(
        rows[0].lines[..4],
        [
            "Detached",
            "C:/wt/wt.feat/app",
            "Branch: feat",
            "Size: 2.0 KB"
        ]
    );
    assert_eq!(
        rows[0].lines.last().map(String::as_str),
        Some("Session: s9")
    );
    assert!(button(&mut h, "worktrees-manager-refresh").enabled);
    h.click_on("worktrees-manager-refresh");
    assert_eq!(inspects(&h.sent()), 1);
    snapshot(&mut h, Vec::new());
    assert_eq!(
        h.root(|root, _| root.worktrees_manager_lines()),
        [
            "Root: C:/wt — default",
            "No managed worktrees under this root."
        ]
    );
}

#[gpui::test]
fn root_change_rescans_an_open_manager(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    root_changed(&mut h, "C:/wt", false);
    open_manager(&mut h);
    snapshot(&mut h, vec![entry("C:/wt/wt.a", "", "stale")]);
    h.sent();
    root_changed(&mut h, "D:/trees", false);
    assert_eq!(inspects(&h.sent()), 1, "the new root is scanned");
    assert!(
        !button(&mut h, "worktrees-manager-refresh").enabled,
        "pending"
    );
    h.send(DaemonMessage::WorktreesRootSnapshot {
        root: "D:/trees".to_owned(),
        is_override: false,
        entries: vec![entry("D:/trees/wt.b", "", "stale")],
    });
    assert_eq!(
        h.root(|root, _| root.worktrees_manager_lines()),
        ["Root: D:/trees — default"]
    );
    let rows = h.root(|root, _| root.worktrees_manager_rows());
    assert_eq!(rows[0].title, "D:/trees/wt.b", "the rows follow the root");
}

#[gpui::test]
fn row_without_anchor_is_titled_by_path(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    open_manager(&mut h);
    snapshot(&mut h, vec![entry("C:/wt/wt.old", "X/dev", "stale")]);
    let rows = h.root(|root, _| root.worktrees_manager_rows());
    assert_eq!(rows[0].title, "C:/wt/wt.old");
    assert_eq!(rows[0].lines[0], "Stale");
    assert_eq!(rows[0].lines[1], "Branch: feat", "the path is not repeated");
}

#[gpui::test]
fn delete_asks_and_sends_delete_worktree_at(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    open_manager(&mut h);
    let mut stale = entry("C:/wt/wt.a", "", "stale");
    stale.size_bytes = Some(3 * 1024 * 1024);
    snapshot(&mut h, vec![stale]);
    h.sent();
    h.click_on("worktrees-manager-row-delete-0");
    let (title, body, _) = h
        .root(|root, _| root.worktrees_manager_confirm())
        .expect("the confirm opened");
    assert_eq!(title, "Confirm delete");
    assert_eq!(body, "Delete this worktree group? Frees ~3.0 MB on disk.");
    assert_eq!(
        h.root(|root, _| root.worktrees_manager_focus()).as_deref(),
        Some("worktrees-manager-confirm-cancel")
    );
    h.click_on("worktrees-manager-confirm-ok");
    assert_eq!(deletes(&h.sent()), ["C:/wt/wt.a"]);
    assert!(
        !button(&mut h, "worktrees-manager-refresh").enabled,
        "pending"
    );
    daemon_error(&mut h);
    assert!(
        button(&mut h, "worktrees-manager-refresh").enabled,
        "an error answers it"
    );
}

#[gpui::test]
fn delete_disabled_for_active(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    open_manager(&mut h);
    snapshot(&mut h, vec![entry("C:/wt/wt.a", "", "active")]);
    h.sent();
    let delete = button(&mut h, "worktrees-manager-row-delete-0");
    assert!(!delete.enabled);
    assert_eq!(
        delete.tooltip.as_deref(),
        Some("Stop the active session before deleting")
    );
    h.click_on("worktrees-manager-row-delete-0");
    assert!(h.root(|root, _| root.worktrees_manager_confirm()).is_none());
    assert_eq!(deletes(&h.sent()), [] as [std::string::String; 0]);
}

#[gpui::test]
fn delete_all_stale_sends_one_per_path_and_clears_pending_after_all_answers(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    open_manager(&mut h);
    snapshot(
        &mut h,
        vec![
            entry("C:/wt/wt.a", "", "stale"),
            entry("C:/wt/wt.b", "", "detached"),
            entry("C:/wt/wt.c", "", "stale"),
        ],
    );
    h.sent();
    assert_eq!(
        button(&mut h, "worktrees-manager-bulk-delete").label,
        "Delete all stale (2)"
    );
    h.click_on("worktrees-manager-bulk-delete");
    let (_, body, _) = h
        .root(|root, _| root.worktrees_manager_confirm())
        .expect("the confirm opened");
    assert_eq!(body, "Delete 2 stale worktree groups?");
    h.keys("tab enter");
    assert_eq!(deletes(&h.sent()), ["C:/wt/wt.a", "C:/wt/wt.c"]);
    snapshot(&mut h, vec![entry("C:/wt/wt.c", "", "stale")]);
    assert!(
        !button(&mut h, "worktrees-manager-refresh").enabled,
        "one delete still owes an answer"
    );
    daemon_error(&mut h);
    assert!(button(&mut h, "worktrees-manager-refresh").enabled);
}

#[gpui::test]
fn launch_here_opens_spawn_dialog_locked_and_pinned(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    open_manager(&mut h);
    snapshot(&mut h, vec![entry("C:/wt/wt.feat", "", "stale")]);
    h.sent();
    h.click_on("worktrees-manager-row-launch-0");
    assert!(!manager_open(&mut h));
    assert!(
        !h.root(|root, _| root.settings_open()),
        "Settings closes too"
    );
    assert!(h.root(|root, _| root.spawn_dialog_open()));
    let sent = h.sent();
    assert!(sent.iter().any(|msg| matches!(
        msg,
        ClientMessage::ListWorktrees { repo_id } if repo_id == "r1"
    )));
    let selected = h.root(|root, _| root.spawn_dialog_selected());
    for chosen in [
        "spawn-target-repo-r1",
        "spawn-worktree-mode-existing",
        "spawn-existing-C:/wt/wt.feat/r1",
    ] {
        assert!(
            selected.iter().any(|s| s == chosen),
            "{chosen} in {selected:?}"
        );
    }
    assert_eq!(
        h.root(|root, _| root.spawn_dialog_focus()).as_deref(),
        Some("spawn-existing-C:/wt/wt.feat/r1")
    );
    assert!(
        h.bounds("spawn-target-repo-r2").origin.x < gpui::px(0.0),
        "the locked target shows alone"
    );
    assert!(h.bounds("spawn-target-repo-r1").origin.x >= gpui::px(0.0));
    h.click_on("spawn-submit");
    let request = h
        .sent()
        .into_iter()
        .find_map(|msg| match msg {
            ClientMessage::SpawnSession(request) => Some(request),
            _ => None,
        })
        .expect("a spawn");
    let SpawnTarget::Single {
        repo_id,
        existing_worktree,
        ..
    } = request.target
    else {
        panic!("a single-repo spawn");
    };
    assert_eq!(repo_id, "r1");
    assert_eq!(existing_worktree.as_deref(), Some("C:/wt/wt.feat/r1"));
}

#[gpui::test]
fn launch_into_active_asks_share_first(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    open_manager(&mut h);
    snapshot(&mut h, vec![entry("C:/wt/wt.feat", "", "active")]);
    assert_eq!(
        button(&mut h, "worktrees-manager-row-launch-0")
            .tooltip
            .as_deref(),
        Some("Launch a second session in this worktree")
    );
    h.click_on("worktrees-manager-row-launch-0");
    let (title, body, _) = h
        .root(|root, _| root.worktrees_manager_confirm())
        .expect("the share confirm opened");
    assert_eq!(title, "Share this worktree?");
    assert_eq!(
        body,
        "A session is already running in feat. Launching here puts a second agent in the same working tree."
    );
    assert_eq!(
        h.root(|root, _| root.worktrees_manager_focus()).as_deref(),
        Some("worktrees-manager-share-cancel")
    );
    h.keys("escape");
    assert!(h.root(|root, _| root.worktrees_manager_confirm()).is_none());
    assert!(manager_open(&mut h), "Esc closes only the confirm");
    h.click_on("worktrees-manager-row-launch-0");
    h.click_on("worktrees-manager-share-ok");
    assert!(!manager_open(&mut h));
    assert!(h.root(|root, _| root.spawn_dialog_open()));
}

#[gpui::test]
fn launch_into_active_asks_share_once(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    open_manager(&mut h);
    snapshot(&mut h, vec![entry("C:/wt/wt.feat", "", "active")]);
    h.click_on("worktrees-manager-row-launch-0");
    h.click_on("worktrees-manager-share-ok");
    assert!(h.root(|root, _| root.spawn_dialog_open()));
    h.sent();
    h.click_on("spawn-submit");
    assert!(
        !h.root(|root, _| root.spawn_share_confirm_open()),
        "the manager's confirm already answered for the pin"
    );
    let request = h
        .sent()
        .into_iter()
        .find_map(|msg| match msg {
            ClientMessage::SpawnSession(request) => Some(request),
            _ => None,
        })
        .expect("a spawn, with no second confirm");
    let SpawnTarget::Single {
        repo_id,
        existing_worktree,
        ..
    } = request.target
    else {
        panic!("a single-repo spawn");
    };
    assert_eq!(repo_id, "r1");
    assert_eq!(existing_worktree.as_deref(), Some("C:/wt/wt.feat/r1"));
}

#[gpui::test]
fn launch_for_an_unregistered_target_keeps_the_manager_open(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    open_manager(&mut h);
    snapshot(&mut h, vec![entry_into("C:/wt/wt.gone", "r9", "stale")]);
    h.sent();
    h.click_on("worktrees-manager-row-launch-0");
    assert!(manager_open(&mut h), "the manager stays up");
    assert!(!h.root(|root, _| root.spawn_dialog_open()));
    assert_eq!(
        toasts(&mut h),
        [(
            "Can't launch here: the repo is no longer registered".to_owned(),
            None
        )]
    );
}

#[gpui::test]
fn manager_closes_on_welcome(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    open_manager(&mut h);
    h.keys("escape");
    assert!(!manager_open(&mut h));
    assert!(h.root(|root, _| root.settings_open()));
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-worktrees-open-manager"),
        "the keyboard goes back to Manage worktrees…"
    );
    h.keys("enter");
    assert!(manager_open(&mut h), "Enter reopens it");
    h.send(DaemonMessage::Welcome {
        protocol_version: PROTOCOL,
        supported_versions: vec![PROTOCOL],
    });
    assert!(!manager_open(&mut h));
}
