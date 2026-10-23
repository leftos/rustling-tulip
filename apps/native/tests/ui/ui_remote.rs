//! Remote-mode specs: on a remote host the client opens, reveals and picks
//! none of the host's files on this PC, never stops or shuts down the host's
//! daemon (a window close only disconnects), hides the local daemon's files
//! in the flyout, and names the host in the footer.

#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use std::path::Path;
use std::time::Duration;

use gpui::{Modifiers, TestAppContext, px};
use protocol::{
    ClientMessage, DaemonMessage, PresetEntry, PresetTarget, SessionSnapshot, SpawnRequest,
    SpawnTarget, SplitDirection, WorktreeCleanupFailure,
};
use rustling_tulip_native::target::Target;
use rustling_tulip_native::{Connection, NetCommand, RootView};
use serde_json::json;
use support::{Fixture, Harness, Opened, TestDir, pane, repo, session, split, tab};

const HOST: &str = "box.tail1234.ts.net";
const LOCAL_FILES: &str = "Not available on a remote host: this opens files on this PC";
const PATH_LINK: &str = "Paths on a remote host can't be opened from this PC yet";
const URL: &str = "https://example.com/docs";

/// A connection to `target` open on `port`, welcomed with the fake daemon's
/// features.
fn open_on(target: Target, port: u16) -> Connection {
    let mut conn = Connection::for_target(target);
    conn.on_connecting();
    conn.on_socket_open(port);
    conn.on_welcome(support::PROTOCOL, support::daemon_features());
    conn
}

/// An open connection to the tailnet host [`HOST`].
fn remote() -> Connection {
    open_on(Target::tailnet(HOST, None), 8787)
}

/// An open connection to the local daemon, on the fake daemon's port.
fn local() -> Connection {
    open_on(Target::Local, 4242)
}

/// Whether the model shows what `selector` tags and it was painted; gpui
/// keeps an element's bounds after it leaves, so a spec asks before the
/// element was ever drawn.
fn drawn(h: &mut Harness<'_>, selector: &str) -> bool {
    h.in_model(selector) && h.bounds(selector).origin.x >= px(0.0)
}

/// Whether an element showing `text` was drawn under `selector`'s text tag
/// (`<selector>=<text>`): what the screen reads, not what the model says.
/// gpui keeps what it painted before, so a spec asks about a text only
/// before it was ever drawn.
fn shows(h: &mut Harness<'_>, selector: &str, text: &str) -> bool {
    h.bounds(&format!("{selector}={text}")).origin.x >= px(0.0)
}

/// Whether resting the mouse on `selector` draws a tooltip reading `text`
/// against the mouse there. gpui puts a tooltip's corner a pixel from the
/// mouse, so one drawn for an earlier control elsewhere does not count.
fn tooltip_on(h: &mut Harness<'_>, selector: &str, text: &str) -> bool {
    let at = h.center(selector);
    h.hover(at, Modifiers::none());
    h.advance(Duration::from_millis(600));
    let tip = h.bounds(&format!("tooltip={text}"));
    let slack = px(2.0);
    tip.origin.x >= px(0.0)
        && at.x >= tip.left() - slack
        && at.x <= tip.right() + slack
        && at.y >= tip.top() - slack
        && at.y <= tip.bottom() + slack
}

/// Every toast's title and detail, oldest first.
fn toasts(h: &mut Harness<'_>) -> Vec<(String, Option<String>)> {
    h.root(|root, _| {
        root.toasts()
            .iter()
            .map(|toast| (toast.title.clone(), toast.detail.clone()))
            .collect()
    })
}

fn session_tip(h: &mut Harness<'_>, selector: &str) -> Option<&'static str> {
    let selector = selector.to_owned();
    h.root(move |root, _| root.menu_disabled_tip(&selector))
}

fn container_tip(h: &mut Harness<'_>, selector: &str) -> Option<String> {
    let selector = selector.to_owned();
    h.root(move |root, _| root.container_menu_disabled_tip(&selector))
}

#[gpui::test]
fn reveal_disabled_on_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(
        cx,
        &dir,
        &Fixture::single(session("s1").worktree("r1").build()),
    );

    h.right_click_on("leaf-s1");
    assert_eq!(
        h.root(|root, _| root.menu_row_label("session-menu-reveal")),
        Some("Reveal worktree")
    );
    assert_eq!(
        session_tip(&mut h, "session-menu-reveal"),
        None,
        "Reveal is offered on the local daemon"
    );
    h.keys("escape");

    h.set_connection(remote());
    h.right_click_on("leaf-s1");
    assert_eq!(
        h.root(|root, _| root.menu_row_label("session-menu-reveal")),
        Some("Reveal worktree"),
        "still listed on a remote host"
    );
    assert_eq!(
        session_tip(&mut h, "session-menu-reveal"),
        Some(LOCAL_FILES)
    );
    h.click_on("session-menu-reveal");
    assert_eq!(h.opened(), [], "the dimmed row opens nothing");
}

#[gpui::test]
fn container_open_rows_disabled_on_remote_copy_path_enabled(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("s1").in_repo("r1").build());
    fixture.repos = vec![repo("r1", "D:/src/r1")];
    let mut h = Harness::with(cx, &dir, &fixture);
    let open_rows = ["container-menu-explorer", "container-menu-vscode"];

    h.right_click_on("container-repo:r1");
    for selector in open_rows {
        assert_eq!(
            container_tip(&mut h, selector).as_deref(),
            None,
            "{selector} is offered on the local daemon"
        );
    }
    h.keys("escape");

    h.set_connection(remote());
    h.right_click_on("container-repo:r1");
    for selector in open_rows {
        assert_eq!(
            container_tip(&mut h, selector).as_deref(),
            Some(LOCAL_FILES),
            "{selector} is disabled on a remote host"
        );
        assert!(
            tooltip_on(&mut h, selector, LOCAL_FILES),
            "{selector} is drawn with the reason as its tooltip"
        );
        h.click_on(selector);
    }
    assert_eq!(h.opened(), [], "the disabled rows open nothing");
    assert_eq!(
        toasts(&mut h),
        [],
        "the rows take no press, so no open is even refused"
    );
    assert_eq!(
        container_tip(&mut h, "container-menu-copy-path").as_deref(),
        None
    );
    h.click_on("container-menu-copy-path");
    assert_eq!(
        h.clipboard().as_deref(),
        Some("D:/src/r1"),
        "Copy path copies the host's path"
    );
}

#[gpui::test]
fn add_repo_disabled_on_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    h.set_connection(remote());
    h.set_picked_folder(Some("D:/work/repo"));
    assert!(
        drawn(&mut h, "sidebar-add-repo"),
        "the note keeps its button"
    );
    assert!(drawn(&mut h, "empty-add-repo"));

    h.click_on("sidebar-add-repo");
    h.click_on("empty-add-repo");
    h.click_on("sidebar-more");
    h.click_on("sidebar-more-add-repo");
    h.keys("escape");
    // A tab's pane takes the keys, so the shortcut opens the dialog.
    h.load(&Fixture::single(session("s1").build()));
    h.keys("ctrl-shift-n");
    assert!(
        drawn(&mut h, "spawn-add-repo"),
        "the no-repos dialog keeps its button"
    );
    h.click_on("spawn-add-repo");
    h.keys("enter");

    assert_eq!(h.folder_asks(), 0, "no folder picker opens on this PC");
    let sent = h.sent();
    assert!(
        !sent
            .iter()
            .any(|msg| matches!(msg, ClientMessage::AddRepo { .. })),
        "nothing is registered: sent {sent:?}"
    );
}

/// No repo registered, `s1` in pane `p1`, on a remote host.
fn remote_without_repos<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = with_repos(cx, dir, Vec::new());
    h.set_connection(remote());
    h
}

#[gpui::test]
fn sidebar_add_repo_drawn_disabled_on_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_without_repos(cx, &dir);
    assert!(tooltip_on(&mut h, "sidebar-add-repo", LOCAL_FILES));
}

#[gpui::test]
fn empty_pane_add_repo_drawn_disabled_on_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    h.set_connection(remote());
    assert!(tooltip_on(&mut h, "empty-add-repo", LOCAL_FILES));
}

#[gpui::test]
fn more_menu_add_repo_drawn_disabled_on_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_without_repos(cx, &dir);
    h.click_on("sidebar-more");
    assert!(tooltip_on(&mut h, "sidebar-more-add-repo", LOCAL_FILES));
    h.click_on("sidebar-more-add-repo");
    assert!(
        h.root(|root, _| root.sidebar_more_open()),
        "the disabled row takes no press, so the menu stays open"
    );
}

#[gpui::test]
fn spawn_dialog_add_repo_drawn_disabled_on_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_without_repos(cx, &dir);
    h.keys("ctrl-shift-n");
    assert!(h.root(|root, _| root.spawn_dialog_no_repos()));
    assert!(tooltip_on(&mut h, "spawn-add-repo", LOCAL_FILES));
}

#[gpui::test]
fn workspace_browse_focused_on_local_opens_no_picker_after_switch_to_remote(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let mut h = with_repos(cx, &dir, two_repos());
    h.set_picked_file(Some("D:/work/team.code-workspace"));
    vscode_creator(&mut h);
    h.keys("tab");
    assert_eq!(
        creator_focus(&mut h).as_deref(),
        Some("workspace-creator-browse")
    );

    h.set_connection(remote());
    h.keys("enter");
    assert_eq!(h.file_asks(), 0, "no file picker opens on this PC");
    assert_eq!(h.folder_asks(), 0);
    assert_eq!(h.opened(), [], "nothing else opens either");
}

#[gpui::test]
fn folder_pick_landing_after_switch_to_remote_is_dropped(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    h.hold_picks();
    h.click_on("sidebar-add-repo");
    assert_eq!(h.folder_asks(), 1, "the picker opens on the local daemon");

    h.set_connection(remote());
    h.release_picked_folder(Some("D:/work/repo"));
    let sent = h.sent();
    assert!(
        !sent
            .iter()
            .any(|msg| matches!(msg, ClientMessage::AddRepo { .. })),
        "a folder of this PC is never registered on the host: sent {sent:?}"
    );
}

/// `s` alone in pane `p1`, its scrollback answered, the pane focused, on the
/// local daemon.
fn attached<'a>(cx: &'a mut TestAppContext, dir: &TestDir, s: SessionSnapshot) -> Harness<'a> {
    let id = s.id.clone();
    let mut h = Harness::with(cx, dir, &Fixture::single(s));
    h.answer_scrollback(&id, b"");
    let at = h.cell_center("p1", 0, 0);
    h.click(at, Modifiers::none());
    h.sent();
    h
}

/// [`attached`], then on a remote host.
fn attached_remote<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    s: SessionSnapshot,
) -> Harness<'a> {
    let mut h = attached(cx, dir, s);
    h.set_connection(remote());
    h
}

/// Writes `rel` under `root`, so a link to it would open on the local
/// daemon.
fn write_file(root: &Path, rel: &str) {
    let path = root.join(rel);
    let parent = path.parent().expect("a file has a parent folder");
    std::fs::create_dir_all(parent).expect("create the file's folder");
    std::fs::write(&path, "x\n").expect("write the file");
}

#[gpui::test]
fn path_link_does_not_open_on_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    write_file(dir.path(), "docs/notes.md");
    let cwd = dir.path().to_string_lossy().into_owned();
    let mut h = attached_remote(cx, &dir, session("s1").shell(&cwd).build());
    h.pty("s1", b"wrote docs/notes.md");
    let on = h.cell_center("p1", 9, 0);
    h.click(on, Modifiers::secondary_key());
    assert_eq!(h.opened(), [], "a host path never opens on this PC");
    assert_eq!(
        toasts(&mut h),
        [("Couldn't open".to_owned(), Some(PATH_LINK.to_owned()))]
    );
}

#[gpui::test]
fn run_confirm_opened_on_local_opens_nothing_after_switch_to_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    write_file(dir.path(), "tools/run.bat");
    let cwd = dir.path().to_string_lossy().into_owned();
    let mut h = attached(cx, &dir, session("s1").shell(&cwd).build());
    h.pty("s1", b"run tools/run.bat");
    let on = h.cell_center("p1", 8, 0);
    h.click(on, Modifiers::secondary_key());
    assert!(
        h.run_confirm().is_some(),
        "the local daemon asks before running the file"
    );

    h.set_connection(remote());
    let run = h.bounds("run-confirm-run").center();
    h.click(run, Modifiers::none());
    assert_eq!(h.opened(), [], "the file is not this PC's to run any more");
    assert_eq!(
        toasts(&mut h),
        [("Couldn't open".to_owned(), Some(LOCAL_FILES.to_owned()))]
    );
}

#[gpui::test]
fn url_link_opens_on_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached_remote(cx, &dir, session("s1").build());
    h.pty("s1", format!("see {URL} here").as_bytes());
    let on = h.cell_center("p1", 10, 0);
    h.click(on, Modifiers::secondary_key());
    assert_eq!(h.opened(), [Opened::Url(URL.to_owned())]);
    assert_eq!(toasts(&mut h), []);
}

#[gpui::test]
fn quit_does_not_shut_down_remote_daemon(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        sessions: vec![session("s1").in_repo("r1").build()],
        tabs: vec![tab("t1", &pane("p1", Some("s1")))],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.set_connection(remote());
    h.commands();

    let closed = h.cx.simulate_close();
    h.cx.run_until_parked();

    assert!(closed, "the window closes at once");
    assert_eq!(h.quit_requests(), 1);
    assert!(
        !h.root(|root, _| root.exit_dialog_open()),
        "no exit dialog asks about the host's sessions"
    );
    let commands = h.commands();
    assert!(
        !commands
            .iter()
            .any(|command| matches!(command, NetCommand::Shutdown { .. } | NetCommand::Stop)),
        "the host's daemon is neither shut down nor stopped"
    );
}

#[gpui::test]
fn flyout_hides_stop_handshake_and_daemon_log_on_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    let kept = ["restart-daemon", "open-native-log", "reveal-config"];
    let local_only = ["stop-daemon", "copy-handshake", "open-daemon-log"];

    h.set_connection(remote());
    h.click_on("daemon-pill");
    for selector in kept {
        assert!(drawn(&mut h, selector), "{selector} shows on a remote host");
    }
    for selector in local_only {
        assert!(
            !drawn(&mut h, selector),
            "{selector} is hidden on a remote host"
        );
    }
    h.click_on("daemon-pill");

    h.set_connection(local());
    h.click_on("daemon-pill");
    for selector in kept.into_iter().chain(local_only) {
        assert!(
            drawn(&mut h, selector),
            "{selector} shows on the local daemon"
        );
    }
}

/// A connection to `target` that is still connecting.
fn connecting(target: Target) -> Connection {
    let mut conn = Connection::for_target(target);
    conn.on_connecting();
    conn
}

#[gpui::test]
fn footer_pill_and_overlay_name_the_remote_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    assert!(shows(&mut h, "daemon-pill", "daemon · 1 session · :4242"));
    h.set_connection(connecting(Target::Local));
    assert!(
        shows(&mut h, "overlay-restart", "Restart daemon"),
        "the local overlay offers a restart"
    );

    h.set_connection(remote());
    assert!(shows(&mut h, "daemon-pill", "box · 1 session"));

    h.set_connection(connecting(Target::tailnet(HOST, None)));
    assert!(
        shows(&mut h, "connecting-overlay", "Connecting to box…"),
        "the overlay names the host too"
    );
    assert!(shows(&mut h, "overlay-restart", "Reconnect now"));
}

#[gpui::test]
fn restart_reads_reconnect_now_on_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    h.click_on("daemon-pill");
    assert!(shows(&mut h, "restart-daemon", "Restart daemon"));
    h.click_on("daemon-pill");

    h.set_connection(remote());
    h.commands();
    h.click_on("daemon-pill");
    assert!(shows(&mut h, "restart-daemon", "Reconnect now"));
    h.click_on("restart-daemon");
    let commands = h.commands();
    assert!(
        matches!(commands.as_slice(), [NetCommand::Restart]),
        "the button asks the network thread to reconnect"
    );
}

#[gpui::test]
fn no_repos_note_names_the_remote_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    assert!(shows(
        &mut h,
        "sidebar-no-repos-note",
        "No repos or workspaces yet."
    ));

    h.set_connection(remote());
    assert!(
        shows(
            &mut h,
            "sidebar-no-repos-note",
            "No repos or workspaces on box yet."
        ),
        "the note says whose registry is empty"
    );
}

#[gpui::test]
fn spawn_dialog_no_repos_note_names_the_remote_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = with_repos(cx, &dir, Vec::new());
    h.keys("ctrl-shift-n");
    assert!(h.root(|root, _| root.spawn_dialog_no_repos()));
    assert!(shows(&mut h, "spawn-no-repos-note", "No repos yet."));

    h.set_connection(remote());
    assert!(
        h.root(|root, _| root.spawn_dialog_no_repos()),
        "the dialog stays open"
    );
    assert!(
        shows(&mut h, "spawn-no-repos-note", "No repos on box yet."),
        "the note says whose registry is empty"
    );
}

/// A connection to the tailnet host [`HOST`] that was open and dropped: what
/// the user reconnects from.
fn dropped_remote() -> Connection {
    let mut conn = remote();
    conn.on_closed("socket closed".to_owned());
    conn
}

#[gpui::test]
fn reconnect_from_a_dropped_remote_connection_sends_restart(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    h.set_connection(dropped_remote());
    h.commands();

    h.click_on("daemon-pill");
    assert!(shows(&mut h, "restart-daemon", "Reconnect now"));
    h.click_on("restart-daemon");
    let commands = h.commands();
    assert!(
        matches!(commands.as_slice(), [NetCommand::Restart]),
        "the button asks for one Reconnect: no stop, shutdown or target switch"
    );
}

/// Whether the open flyout's detail rows read `labels`, `|`-joined.
fn details_read(h: &mut Harness<'_>, labels: &str) -> bool {
    shows(h, "flyout-details", labels)
}

#[gpui::test]
fn handshake_cleared_when_target_switches(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    h.click_on("daemon-pill");
    assert!(
        details_read(&mut h, "State|Port|PID|Protocol|Sessions"),
        "the local daemon's handshake shows"
    );

    // A drop after the first connect draws no overlay, so the flyout stays.
    let mut dropped = local();
    dropped.on_closed("socket closed".to_owned());
    h.set_connection(dropped);
    assert!(
        details_read(&mut h, "State|Port|PID|Protocol|Reason"),
        "a new state on the same target keeps it"
    );

    h.set_connection(remote());
    h.set_connection(local());
    assert!(
        details_read(&mut h, "State|Sessions"),
        "the handshake from before the switch is gone"
    );
}

/// A failed cleanup of one worktree folder, held by nothing.
fn cleanup_failed(h: &mut Harness<'_>) {
    h.send(DaemonMessage::WorktreeCleanupFailed {
        session_id: "s1".to_owned(),
        session_label: "feat/x".to_owned(),
        failures: vec![WorktreeCleanupFailure {
            member_path: "C:/wts/wt.feat-x/a".to_owned(),
            repo_path: "C:/src/a".to_owned(),
            reason: "fs::remove_dir_all: access denied".to_owned(),
            locking_processes: Vec::new(),
        }],
    });
}

/// `s1` alone in pane `p1`, its scrollback answered, with `repos`.
fn with_repos<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    repos: Vec<protocol::RepoEntry>,
) -> Harness<'a> {
    let mut fixture = Fixture::single(session("s1").build());
    fixture.repos = repos;
    let mut h = Harness::with(cx, dir, &fixture);
    h.answer_scrollback("s1", b"");
    h.sent();
    h
}

/// Settings open on its Worktrees tab, the root at its default.
fn worktrees_tab(h: &mut Harness<'_>) {
    h.send(DaemonMessage::WorktreesRootChanged {
        root: "C:/default".to_owned(),
        is_override: false,
    });
    h.keys("ctrl-,");
    h.click_on("settings-tab-worktrees");
}

/// The workspace creator open on its From VS Code tab.
fn vscode_creator(h: &mut Harness<'_>) {
    h.click_on("sidebar-more");
    h.click_on("sidebar-more-add-workspace");
    h.click_on("workspace-creator-tab-vscode");
}

fn two_repos() -> Vec<protocol::RepoEntry> {
    vec![repo("r1", "C:/r1"), repo("r2", "C:/r2")]
}

#[gpui::test]
fn shell_browse_disabled_on_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = with_repos(cx, &dir, Vec::new());
    h.set_connection(remote());
    h.set_picked_folder(Some("D:/work"));
    h.open_shell_dialog_from_more();
    assert!(tooltip_on(&mut h, "shell-browse", LOCAL_FILES));
    h.click_on("shell-browse");
    assert_eq!(h.folder_asks(), 0, "no folder picker opens on this PC");
    assert_eq!(
        h.root(RootView::shell_dialog_folder).as_deref(),
        Some(""),
        "the field is left for a typed host path"
    );
}

#[gpui::test]
fn worktrees_root_browse_disabled_on_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = with_repos(cx, &dir, Vec::new());
    h.set_connection(remote());
    h.set_picked_folder(Some("E:/trees"));
    worktrees_tab(&mut h);
    assert!(tooltip_on(
        &mut h,
        "settings-worktrees-root-browse",
        LOCAL_FILES
    ));
    h.click_on("settings-worktrees-root-browse");
    assert_eq!(h.folder_asks(), 0, "no folder picker opens on this PC");
}

#[gpui::test]
fn workspace_file_browse_disabled_on_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = with_repos(cx, &dir, two_repos());
    h.set_connection(remote());
    h.set_picked_file(Some("D:/work/team.code-workspace"));
    vscode_creator(&mut h);
    assert!(tooltip_on(&mut h, "workspace-creator-browse", LOCAL_FILES));
    h.click_on("workspace-creator-browse");
    assert_eq!(h.file_asks(), 0, "no file picker opens on this PC");
}

#[gpui::test]
fn cleanup_open_folder_disabled_on_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = with_repos(cx, &dir, Vec::new());
    h.set_connection(remote());
    cleanup_failed(&mut h);
    assert!(tooltip_on(&mut h, "cleanup-open-folder-0", LOCAL_FILES));
    h.click_on("cleanup-open-folder-0");
    assert_eq!(h.opened(), [], "the host's folder is not revealed here");
    assert_eq!(
        toasts(&mut h),
        [],
        "the button takes no press, so no open is even refused"
    );
}

fn shell_focus(h: &mut Harness<'_>) -> Option<&'static str> {
    h.root(RootView::shell_dialog_focus)
}

#[gpui::test]
fn shell_browse_leaves_the_tab_ring_on_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = with_repos(cx, &dir, Vec::new());
    h.open_shell_dialog_from_more();
    assert_eq!(shell_focus(&mut h), Some("shell-folder"));
    h.keys("tab");
    assert_eq!(
        shell_focus(&mut h),
        Some("shell-browse"),
        "Tab reaches Browse on the local daemon"
    );
    h.keys("escape");

    h.set_connection(remote());
    h.open_shell_dialog_from_more();
    h.keys("tab");
    assert_eq!(
        shell_focus(&mut h),
        Some("shell-cancel"),
        "Tab skips Browse and the quick-shell default on a remote host"
    );
    h.keys("shift-tab");
    assert_eq!(
        shell_focus(&mut h),
        Some("shell-folder"),
        "and so does Shift+Tab"
    );
}

fn settings_focus(h: &mut Harness<'_>) -> Option<&'static str> {
    let root = h.root.clone();
    h.cx.update(|window, cx| root.read(cx).settings_focus(window, cx))
}

#[gpui::test]
fn worktrees_root_browse_leaves_the_tab_ring_on_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = with_repos(cx, &dir, Vec::new());
    worktrees_tab(&mut h);
    h.click_on("settings-worktrees-root-input");
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-worktrees-root-browse"),
        "Tab reaches Browse on the local daemon"
    );

    h.set_connection(remote());
    h.click_on("settings-worktrees-root-input");
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-worktrees-open-manager"),
        "Tab skips Browse on a remote host"
    );
    h.keys("shift-tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-worktrees-root-input"),
        "and so does Shift+Tab"
    );
}

fn creator_focus(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.workspace_creator_focus())
}

#[gpui::test]
fn workspace_file_browse_leaves_the_tab_ring_on_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = with_repos(cx, &dir, two_repos());
    vscode_creator(&mut h);
    assert_eq!(
        creator_focus(&mut h).as_deref(),
        Some("workspace-creator-tab-vscode")
    );
    h.keys("tab");
    assert_eq!(
        creator_focus(&mut h).as_deref(),
        Some("workspace-creator-browse"),
        "Tab reaches Browse on the local daemon"
    );
    h.keys("escape");

    h.set_connection(remote());
    vscode_creator(&mut h);
    h.keys("tab");
    assert_eq!(
        creator_focus(&mut h).as_deref(),
        Some("workspace-creator-cancel"),
        "Tab skips Browse on a remote host"
    );
    h.keys("shift-tab");
    assert_eq!(
        creator_focus(&mut h).as_deref(),
        Some("workspace-creator-tab-vscode"),
        "and so does Shift+Tab"
    );
}

fn cleanup_focus(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.cleanup_failed_focus())
}

#[gpui::test]
fn cleanup_open_folder_leaves_the_tab_ring_on_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = with_repos(cx, &dir, Vec::new());
    cleanup_failed(&mut h);
    assert_eq!(cleanup_focus(&mut h).as_deref(), Some("cleanup-ignore"));
    h.keys("shift-tab");
    assert_eq!(
        cleanup_focus(&mut h).as_deref(),
        Some("cleanup-open-folder-0"),
        "Shift+Tab from Ignore reaches Open folder on the local daemon"
    );

    h.set_connection(remote());
    cleanup_failed(&mut h);
    assert_eq!(cleanup_focus(&mut h).as_deref(), Some("cleanup-ignore"));
    h.keys("shift-tab");
    assert_eq!(
        cleanup_focus(&mut h).as_deref(),
        Some("cleanup-close"),
        "Shift+Tab skips Open folder on a remote host"
    );
    h.keys("tab tab");
    assert_eq!(
        cleanup_focus(&mut h).as_deref(),
        Some("cleanup-retry"),
        "and so does Tab"
    );
}

#[gpui::test]
fn no_repos_dialog_keys_reach_no_add_repo_on_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = with_repos(cx, &dir, Vec::new());
    h.keys("ctrl-shift-n");
    assert!(h.root(|root, _| root.spawn_dialog_no_repos()));
    h.keys("enter");
    assert_eq!(
        h.folder_asks(),
        1,
        "Enter presses Add repo on the local daemon"
    );

    h.set_connection(remote());
    h.keys("enter space");
    assert_eq!(
        h.folder_asks(),
        1,
        "on a remote host Enter and Space press nothing"
    );
    assert!(
        h.root(|root, _| root.spawn_dialog_no_repos()),
        "the dialog stays for Esc or Cancel"
    );
}

/// Types `text` into the field that has the keyboard, one key per char.
fn type_text(h: &mut Harness<'_>, text: &str) {
    let keys: Vec<String> = text.chars().map(|c| c.to_string()).collect();
    h.keys(&keys.join(" "));
}

/// An empty registry on a remote host, then `r1` and its session `s1`
/// loaded from the host.
fn remote_with_r1<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = Harness::with(cx, dir, &Fixture::default());
    h.set_connection(remote());
    let mut fixture = Fixture::single(session("s1").in_repo("r1").build());
    fixture.repos = vec![repo("r1", "D:/src/r1")];
    h.load(&fixture);
    h.sent();
    h
}

/// A preset of `r1` with `sources` and `variables`, as the wizard's specs
/// build one.
#[expect(
    clippy::needless_pass_by_value,
    reason = "each spec builds its JSON inline at the call"
)]
fn preset(sources: serde_json::Value, variables: serde_json::Value) -> PresetEntry {
    serde_json::from_value(json!({
        "id": "triage", "name": "Triage", "source_repo_id": "r1",
        "prompt_sources": sources, "variables": variables,
        "prompt_template": "{prompt}", "branch_template": "tmp/{index}",
        "tab_grouping": {"kind": "none"},
        "injector": {"startup_delay_ms": 0, "pre_input": [], "post_input": []}
    }))
    .expect("preset fixture")
}

/// Opens r1's menu, answers it with `entry` and presses its row.
fn open_preset_wizard(h: &mut Harness<'_>, entry: PresetEntry) {
    h.right_click_on("container-repo:r1");
    h.send(DaemonMessage::Presets {
        target: PresetTarget::Repo {
            repo_id: "r1".to_owned(),
        },
        entries: vec![entry],
    });
    h.click_on("container-menu-launch-preset-0");
    assert!(
        h.root(|root, _| root.preset_wizard_open()),
        "the wizard opened"
    );
}

#[gpui::test]
fn preset_wizard_browse_opens_no_picker_on_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_with_r1(cx, &dir);
    h.set_picked_file(Some("E:/prompts/bugs.md"));
    h.set_picked_folder(Some("E:/prompts"));

    let sources = json!([{"kind": "file"}, {"kind": "folder", "relative_path": "docs/prompts"}]);
    open_preset_wizard(&mut h, preset(sources, json!([])));
    assert!(tooltip_on(&mut h, "preset-source-file-pick", LOCAL_FILES));
    h.click_on("preset-source-file-pick");
    h.click_on("preset-source-kind-folder");
    assert!(tooltip_on(&mut h, "preset-source-folder-pick", LOCAL_FILES));
    h.click_on("preset-source-folder-pick");
    h.click_on("preset-cancel");

    let variables = json!([
        {"name": "log", "label": "Log file", "kind": {"kind": "file_path", "extensions": ["log"]},
         "prompt_at_launch": true}
    ]);
    open_preset_wizard(&mut h, preset(json!([{"kind": "inline"}]), variables));
    h.click_on("preset-source-inline-text");
    type_text(&mut h, "fix");
    h.click_on("preset-next");
    assert!(tooltip_on(&mut h, "preset-variable-log-pick", LOCAL_FILES));
    h.click_on("preset-variable-log-pick");

    assert_eq!(h.file_asks(), 0, "no file picker opens on this PC");
    assert_eq!(h.folder_asks(), 0, "no folder picker opens on this PC");
}

/// Seeds `native-ui.json` with `folder` remembered for "+ Shell".
fn remember_quick_shell(dir: &TestDir, folder: &str) {
    std::fs::write(
        dir.path().join("native-ui.json"),
        json!({ "quick_shell_dir": folder }).to_string(),
    )
    .expect("seed native-ui.json");
}

/// The quick-shell folder `native-ui.json` remembers on disk.
fn remembered_quick_shell(dir: &TestDir) -> Option<String> {
    let text = std::fs::read_to_string(dir.path().join("native-ui.json")).expect("a saved layout");
    let ui: serde_json::Value = serde_json::from_str(&text).expect("native-ui.json is JSON");
    ui["quick_shell_dir"].as_str().map(str::to_owned)
}

fn shell_spawns(sent: &[ClientMessage]) -> Vec<SpawnRequest> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::SpawnSession(request) => Some(request.clone()),
            _ => None,
        })
        .collect()
}

#[gpui::test]
fn quick_shell_sends_no_cwd_on_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    remember_quick_shell(&dir, "C:\\work");
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    h.set_connection(remote());
    h.sent();

    h.click_on("sidebar-add-shell");
    let targets: Vec<SpawnTarget> = shell_spawns(&h.sent())
        .into_iter()
        .map(|request| request.target)
        .collect();
    assert_eq!(
        targets,
        [SpawnTarget::Standalone {
            cwd: None,
            add_dirs: Vec::new(),
        }],
        "this PC's folder is not the host's: the host's daemon picks its default"
    );
}

#[gpui::test]
fn empty_pane_shell_here_sends_no_cwd_on_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    remember_quick_shell(&dir, "C:\\work");
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    h.set_connection(remote());
    h.load(&Fixture {
        sessions: vec![session("s1").build()],
        tabs: vec![tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                pane("p1", Some("s1")),
                pane("p2", None),
            ),
        )],
        ..Fixture::default()
    });
    h.sent();

    h.click_on("empty-pane-shell-p2");
    let targets: Vec<SpawnTarget> = shell_spawns(&h.sent())
        .into_iter()
        .map(|request| request.target)
        .collect();
    assert_eq!(
        targets,
        [SpawnTarget::Standalone {
            cwd: None,
            add_dirs: Vec::new(),
        }],
        "this PC's folder is not the host's: the host's daemon picks its default"
    );
}

#[gpui::test]
fn quick_shell_tooltip_names_no_local_folder_on_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    remember_quick_shell(&dir, "C:\\work");
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    h.set_connection(remote());
    assert!(tooltip_on(
        &mut h,
        "sidebar-add-shell",
        "Open a standalone shell in your home folder"
    ));
}

#[gpui::test]
fn shell_dialog_leaves_the_remembered_folder_on_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    remember_quick_shell(&dir, "C:\\work");
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    h.set_connection(remote());
    h.sent();

    h.open_shell_dialog_from_more();
    assert_eq!(
        h.root(RootView::shell_dialog_folder).as_deref(),
        Some(""),
        "the field does not start at this PC's folder"
    );
    assert!(
        !h.root(|root, _| root.shell_dialog_saves_default()),
        "the dialog offers no quick-shell default"
    );
    assert!(
        !h.root(|root, _| root.shell_dialog_clears_default()),
        "nor to clear one"
    );
    assert!(!drawn(&mut h, "shell-save-default"));
    assert!(!drawn(&mut h, "shell-clear-default"));

    type_text(&mut h, "D:/host");
    h.click_on("shell-submit");
    let request = match shell_spawns(&h.sent()).as_slice() {
        [request] => request.clone(),
        other => panic!("expected one SpawnSession, sent {other:?}"),
    };
    assert_eq!(
        request.target,
        SpawnTarget::Standalone {
            cwd: Some("D:/host".to_owned()),
            add_dirs: Vec::new(),
        }
    );
    h.send(DaemonMessage::SessionUpdated {
        session: session("new").shell("D:/host").build(),
        request_id: request.request_id.clone(),
    });
    assert_eq!(
        remembered_quick_shell(&dir).as_deref(),
        Some("C:\\work"),
        "a shell on the host leaves this PC's quick-shell folder alone"
    );
}
