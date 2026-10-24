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
    ClientMessage, DaemonMessage, InitLayoutKind, PresetEntry, PresetTarget, RearrangeLayout,
    SessionSnapshot, SpawnRequest, SpawnTarget, SplitDirection, WorktreeCleanupFailure,
};
use rustling_tulip_native::target::Target;
use rustling_tulip_native::{Connection, NetCommand, NetEvent, RootView};
use serde_json::json;
use support::{Fixture, Harness, Opened, TestDir, pane, repo, session, split, tab};

const HOST: &str = "box.tail1234.ts.net";
const LOCAL_FILES: &str = "Not available on a remote host: this opens files on this PC";
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

/// [`HOST`] as a target to open on.
fn remote_target() -> Target {
    Target::tailnet(HOST, None)
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
    let fixture = Fixture::single(session("s1").worktree("r1").build());
    let mut h = Harness::with(cx, &dir, &fixture);

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
    h.load(&fixture);
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
    h.load(&fixture);
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

fn prompt_open(h: &mut Harness<'_>) -> bool {
    h.root(|root, _| root.host_path_prompt_open())
}

fn prompt_error(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.host_path_prompt_error())
}

/// Whether an `AddRepo` went out among `sent`.
fn sent_add_repo(sent: &[ClientMessage]) -> bool {
    sent.iter()
        .any(|msg| matches!(msg, ClientMessage::AddRepo { .. }))
}

#[gpui::test]
fn add_repo_on_remote_opens_the_host_path_prompt(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::on_target(cx, &dir, remote_target(), &Fixture::default());
    h.set_picked_folder(Some("D:/work/repo"));
    assert!(!prompt_open(&mut h));

    h.click_on("sidebar-add-repo");
    assert!(prompt_open(&mut h), "the sidebar note's button opens it");
    assert_eq!(
        h.root(|root, _| root.host_path_prompt_title()).as_deref(),
        Some("Add a repo on box")
    );
    assert!(drawn(&mut h, "host-path-input"));
    h.keys("escape");
    assert!(!prompt_open(&mut h));

    h.click_on("empty-add-repo");
    assert!(prompt_open(&mut h), "the empty pane's button opens it");
    h.keys("escape");

    h.click_on("sidebar-more");
    h.click_on("sidebar-more-add-repo");
    assert!(prompt_open(&mut h), "the ⋯ menu's row opens it");
    assert!(
        !h.root(|root, _| root.sidebar_more_open()),
        "and closes the menu"
    );
    h.keys("escape");

    // A tab's pane takes the keys, so the shortcut opens the dialog.
    h.load(&Fixture::single(session("s1").build()));
    h.keys("ctrl-shift-n");
    assert!(h.root(|root, _| root.spawn_dialog_no_repos()));
    h.click_on("spawn-add-repo");
    assert!(prompt_open(&mut h), "the no-repos dialog's button opens it");
    h.keys("escape");
    assert!(!prompt_open(&mut h), "Esc closes the prompt first");
    assert!(
        h.root(|root, _| root.spawn_dialog_no_repos()),
        "the no-repos dialog stays under it"
    );
    h.keys("enter");
    assert!(prompt_open(&mut h), "Enter on the no-repos dialog opens it");
    h.keys("escape");

    assert_eq!(h.folder_asks(), 0, "no folder picker opens on this PC");
    let sent = h.sent();
    assert!(
        !sent_add_repo(&sent),
        "nothing is registered: sent {sent:?}"
    );
}

/// No repo registered, `s1` in pane `p1`, on a remote host.
fn remote_without_repos<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut fixture = Fixture::single(session("s1").build());
    fixture.repos = Vec::new();
    let mut h = Harness::on_target(cx, dir, remote_target(), &fixture);
    h.answer_scrollback("s1", b"");
    h.sent();
    h
}

#[gpui::test]
fn closing_the_host_path_prompt_returns_focus_to_the_spawn_dialog(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_without_repos(cx, &dir);
    let at = h.cell_center("p1", 0, 0);
    h.click(at, Modifiers::none());
    h.keys("ctrl-shift-n");
    assert!(h.root(|root, _| root.spawn_dialog_no_repos()));
    h.keys("enter");
    assert!(prompt_open(&mut h));
    h.keys("escape");
    assert!(!prompt_open(&mut h));
    assert!(h.root(|root, _| root.spawn_dialog_no_repos()));
    let root = h.root.clone();
    let (dialog, pane) = h.cx.update(|window, cx| {
        let root = root.read(cx);
        (
            root.spawn_dialog_holds_keyboard(window),
            root.pane_holds_keyboard("p1", window, cx),
        )
    });
    assert!(dialog, "the no-repos dialog takes the keyboard back");
    assert!(!pane, "the pane under the dialog does not");
}

#[gpui::test]
fn add_repo_prompt_sends_add_repo_with_the_typed_path(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_without_repos(cx, &dir);
    h.click_on("sidebar-add-repo");
    type_text(&mut h, "/srv/app/");
    h.keys("enter");
    assert!(!prompt_open(&mut h), "Add closes the prompt");
    let sent = h.sent();
    assert!(
        sent.iter().any(|msg| matches!(
            msg,
            ClientMessage::AddRepo { path, name: Some(name) }
                if path == "/srv/app/" && name == "app"
        )),
        "the typed path goes out, named by its last segment: sent {sent:?}"
    );

    h.click_on("sidebar-add-repo");
    type_text(&mut h, "/srv/web");
    h.keys("tab tab");
    assert_eq!(
        h.root(|root, _| root.host_path_prompt_focus()).as_deref(),
        Some("host-path-add")
    );
    h.keys("enter");
    assert!(!prompt_open(&mut h));
    let sent = h.sent();
    assert!(
        sent.iter().any(|msg| matches!(
            msg,
            ClientMessage::AddRepo { path, .. } if path == "/srv/web"
        )),
        "Add pressed from the keyboard sends too: sent {sent:?}"
    );
}

#[gpui::test]
fn add_repo_prompt_refuses_a_relative_path_inline(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_without_repos(cx, &dir);
    h.click_on("sidebar-add-repo");
    type_text(&mut h, "srv/app");
    h.keys("enter");
    assert!(prompt_open(&mut h), "the prompt stays");
    assert_eq!(
        prompt_error(&mut h).as_deref(),
        Some("Use a full path on box, starting with a drive letter, \\\\server or /.")
    );
    assert!(drawn(&mut h, "host-path-error"));
    assert!(!sent_add_repo(&h.sent()), "nothing is registered");

    type_text(&mut h, "x");
    assert_eq!(prompt_error(&mut h), None, "an edit clears the refusal");

    h.click_on("host-path-add");
    assert_eq!(
        prompt_error(&mut h).as_deref(),
        Some("Use a full path on box, starting with a drive letter, \\\\server or /."),
        "a click on Add checks the path too"
    );
}

#[gpui::test]
fn add_repo_prompt_escape_closes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_without_repos(cx, &dir);
    h.click_on("sidebar-add-repo");
    type_text(&mut h, "/srv/app");
    h.keys("escape");
    assert!(!prompt_open(&mut h));
    assert!(!sent_add_repo(&h.sent()), "nothing is registered");

    h.click_on("sidebar-add-repo");
    assert_eq!(
        h.root(RootView::host_path_prompt_text).as_deref(),
        Some(""),
        "a reopened prompt starts empty"
    );
    h.click_on("host-path-cancel");
    assert!(!prompt_open(&mut h), "Cancel closes it too");
}

#[gpui::test]
fn add_repo_on_remote_writes_no_last_repo_dir(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let ui = dir.path().join("native-ui.json");
    std::fs::write(&ui, r#"{"last_repo_dir":"C:/src"}"#).expect("seed the last repo folder");
    let mut h = remote_without_repos(cx, &dir);
    h.click_on("sidebar-add-repo");
    assert_eq!(
        h.root(RootView::host_path_prompt_text).as_deref(),
        Some(""),
        "this PC's folder is not offered on the host"
    );
    type_text(&mut h, "/srv/app");
    h.keys("enter");
    assert!(sent_add_repo(&h.sent()));
    let saved = std::fs::read_to_string(&ui).unwrap_or_default();
    assert!(
        !saved.contains("/srv"),
        "a host path is never remembered: {saved}"
    );
}

#[gpui::test]
fn target_switch_closes_the_workspace_creator(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = with_repos(cx, &dir, two_repos());
    vscode_creator(&mut h);
    assert!(h.root(|root, _| root.workspace_creator_open()));

    h.set_connection(remote());
    assert!(
        !h.root(|root, _| root.workspace_creator_open()),
        "the switch closes the creator opened on the local daemon"
    );
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

/// `s` alone in pane `p1`, its scrollback answered, the pane focused, on a
/// remote host.
fn attached_remote<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    s: SessionSnapshot,
) -> Harness<'a> {
    let id = s.id.clone();
    let mut h = Harness::on_target(cx, dir, remote_target(), &Fixture::single(s));
    h.answer_scrollback(&id, b"");
    let at = h.cell_center("p1", 0, 0);
    h.click(at, Modifiers::none());
    h.sent();
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
fn path_link_fetches_instead_of_opening_on_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached_remote(cx, &dir, session("s1").worktree("r1").build());
    h.sent();
    h.pty("s1", br"wrote C:\wt\x\docs\notes.md");
    let on = h.cell_center("p1", 9, 0);
    h.click(on, Modifiers::secondary_key());
    assert_eq!(h.opened(), [], "a host path never opens on this PC");
    let fetches: Vec<(String, Option<String>, String)> = h
        .sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::FetchFile {
                repo_id,
                worktree_path,
                path,
                ..
            } => Some((repo_id, worktree_path, path)),
            _ => None,
        })
        .collect();
    assert_eq!(
        fetches,
        [(
            "r1".to_owned(),
            Some("C:/wt/x".to_owned()),
            "docs/notes.md".to_owned()
        )],
        "the link's file is fetched from the member's worktree"
    );
    assert!(
        !toasts(&mut h)
            .iter()
            .any(|(title, _)| title == "Couldn't open"),
        "nothing is refused: {:?}",
        toasts(&mut h)
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
    let mut h = Harness::on_target(cx, &dir, remote_target(), &fixture);
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
    let fixture = Fixture::single(session("s1").build());
    let mut h = Harness::with(cx, &dir, &fixture);
    assert!(shows(&mut h, "daemon-pill", "daemon · 1 session · :4242"));
    h.set_connection(connecting(Target::Local));
    assert!(
        shows(&mut h, "overlay-restart", "Restart daemon"),
        "the local overlay offers a restart"
    );

    h.set_connection(remote());
    h.load(&fixture);
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
    h.load(&Fixture::default());
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
        !h.root(|root, _| root.spawn_dialog_no_repos()),
        "the switch closes the dialog opened on the local daemon"
    );
    reload_on_remote(&mut h, Vec::new());
    h.keys("ctrl-shift-n");
    assert!(h.root(|root, _| root.spawn_dialog_no_repos()));
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

/// `s1` alone in pane `p1`, its scrollback answered, with `repos`, on a
/// remote host.
fn remote_with_repos<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    repos: Vec<protocol::RepoEntry>,
) -> Harness<'a> {
    let mut fixture = Fixture::single(session("s1").build());
    fixture.repos = repos;
    let mut h = Harness::on_target(cx, dir, remote_target(), &fixture);
    h.answer_scrollback("s1", b"");
    h.sent();
    h
}

/// Switches to a remote host that lists `s1` alone in pane `p1`, its
/// scrollback answered, with `repos`: the second half of a spec whose first
/// half ran on the local daemon.
fn reload_on_remote(h: &mut Harness<'_>, repos: Vec<protocol::RepoEntry>) {
    h.set_connection(remote());
    let mut fixture = Fixture::single(session("s1").build());
    fixture.repos = repos;
    h.load(&fixture);
    h.answer_scrollback("s1", b"");
    h.sent();
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
    let mut h = remote_with_repos(cx, &dir, Vec::new());
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
    let mut h = remote_with_repos(cx, &dir, Vec::new());
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
fn workspace_file_browse_not_drawn_on_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_with_repos(cx, &dir, two_repos());
    h.set_picked_file(Some("D:/work/team.code-workspace"));
    vscode_creator(&mut h);
    assert!(
        !h.in_model("workspace-creator-browse"),
        "the typed field takes Browse's place"
    );
    assert!(drawn(&mut h, "workspace-creator-file-input"));
    assert_eq!(h.file_asks(), 0, "no file picker opens on this PC");
}

#[gpui::test]
fn workspace_creator_on_remote_takes_a_typed_workspace_file(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let ui = dir.path().join("native-ui.json");
    std::fs::write(&ui, r#"{"last_vscode_dir":"D:/vs"}"#).expect("seed the picker folder");
    let mut h = remote_with_repos(cx, &dir, two_repos());
    vscode_creator(&mut h);
    assert!(shows(
        &mut h,
        "workspace-creator-file-label",
        "Workspace file on box"
    ));
    assert_eq!(
        h.root(RootView::workspace_creator_file_text).as_deref(),
        Some(""),
        "this PC's remembered folder is not offered on the host"
    );
    h.keys("tab");
    type_text(&mut h, "/srv/team.code-workspace");
    h.keys("enter");
    let sent = h.sent();
    assert!(
        sent.iter().any(|msg| matches!(
            msg,
            ClientMessage::ParseVscodeWorkspace { path } if path == "/srv/team.code-workspace"
        )),
        "the typed file is parsed on the host: sent {sent:?}"
    );
    assert!(h.root(|root, _| root.workspace_creator_parsing()));
    let saved = std::fs::read_to_string(&ui).unwrap_or_default();
    assert!(
        !saved.contains("/srv"),
        "a host path is never remembered: {saved}"
    );
}

#[gpui::test]
fn workspace_creator_refuses_a_path_without_code_workspace(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_with_repos(cx, &dir, two_repos());
    vscode_creator(&mut h);
    h.keys("tab");
    type_text(&mut h, "/srv/team.json");
    h.keys("enter");
    assert_eq!(
        h.root(|root, _| root.workspace_creator_error()).as_deref(),
        Some("Type the path of a .code-workspace file.")
    );
    let sent = h.sent();
    assert!(
        !sent
            .iter()
            .any(|msg| matches!(msg, ClientMessage::ParseVscodeWorkspace { .. })),
        "nothing is parsed: sent {sent:?}"
    );
    assert!(!h.root(|root, _| root.workspace_creator_parsing()));

    type_text(&mut h, "x");
    assert_eq!(
        h.root(|root, _| root.workspace_creator_error()),
        None,
        "an edit clears the refusal"
    );
}

#[gpui::test]
fn workspace_creator_no_repos_note_names_the_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_with_repos(cx, &dir, two_repos());
    h.click_on("sidebar-more");
    h.click_on("sidebar-more-add-workspace");
    h.send(DaemonMessage::Repos { repos: Vec::new() });
    assert!(shows(
        &mut h,
        "workspace-creator-no-repos",
        "No repos on box yet."
    ));
}

#[gpui::test]
fn worktrees_root_on_remote_says_it_is_created_on_the_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = with_repos(cx, &dir, Vec::new());
    worktrees_tab(&mut h);
    assert!(
        !shows(
            &mut h,
            "settings-worktrees-host-hint",
            "Created on box if it does not exist."
        ),
        "no host hint on the local daemon"
    );
    h.keys("escape");

    reload_on_remote(&mut h, Vec::new());
    worktrees_tab(&mut h);
    assert!(shows(
        &mut h,
        "settings-worktrees-host-hint",
        "Created on box if it does not exist."
    ));
    h.click_on("settings-worktrees-root-input");
    type_text(&mut h, "x");
    assert_eq!(
        h.root(|root, _| root.worktrees_path()).as_deref(),
        Some("x"),
        "the field stays enabled"
    );
}

#[gpui::test]
fn cleanup_open_folder_disabled_on_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_with_repos(cx, &dir, Vec::new());
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

    reload_on_remote(&mut h, Vec::new());
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

    reload_on_remote(&mut h, Vec::new());
    worktrees_tab(&mut h);
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

    reload_on_remote(&mut h, two_repos());
    vscode_creator(&mut h);
    h.keys("tab");
    assert_eq!(
        creator_focus(&mut h).as_deref(),
        Some("workspace-creator-file-input"),
        "Tab reaches the typed file field in Browse's place on a remote host"
    );
    h.keys("shift-tab");
    assert_eq!(
        creator_focus(&mut h).as_deref(),
        Some("workspace-creator-tab-vscode"),
        "and Shift+Tab goes back to the tab"
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

    reload_on_remote(&mut h, Vec::new());
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
fn no_repos_dialog_keys_open_the_host_path_prompt_on_remote(cx: &mut TestAppContext) {
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

    reload_on_remote(&mut h, Vec::new());
    h.keys("ctrl-shift-n");
    assert!(h.root(|root, _| root.spawn_dialog_no_repos()));
    h.keys("space");
    assert!(
        prompt_open(&mut h),
        "Space presses Add repo on a remote host"
    );
    h.keys("escape");
    h.keys("enter");
    assert!(prompt_open(&mut h), "and so does Enter");
    assert_eq!(h.folder_asks(), 1, "no folder picker opens on this PC");
    assert!(
        h.root(|root, _| root.spawn_dialog_no_repos()),
        "the dialog stays under the prompt"
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
    let mut fixture = Fixture::single(session("s1").in_repo("r1").build());
    fixture.repos = vec![repo("r1", "D:/src/r1")];
    let mut h = Harness::on_target(cx, dir, remote_target(), &fixture);
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
    let mut h = Harness::on_target(cx, &dir, remote_target(), &Fixture::default());
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
    let fixture = Fixture {
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
    };
    let mut h = Harness::on_target(cx, &dir, remote_target(), &fixture);
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

/// Saves [`HOST`] as a host the picker offers, with `counts` as the
/// recorded session counts, before the window opens.
fn seed_saved_host(dir: &TestDir, counts: &serde_json::Value) {
    let ui = json!({
        "saved_hosts": [{ "name": HOST, "port": 8787 }],
        "host_session_counts": counts,
    });
    std::fs::write(
        dir.path().join("native-ui.json"),
        serde_json::to_vec_pretty(&ui).expect("serialize native-ui.json"),
    )
    .expect("write native-ui.json");
}

/// Saves [`HOST`] with `count` as its last session count before the window
/// opens.
fn seed_host_count(dir: &TestDir, count: u32) {
    seed_saved_host(dir, &json!({ HOST: count }));
}

/// [`HOST`]'s session count as `native-ui.json` holds it on disk.
fn saved_host_count(dir: &TestDir) -> serde_json::Value {
    let text = std::fs::read_to_string(dir.path().join("native-ui.json")).expect("native-ui.json");
    let ui: serde_json::Value = serde_json::from_str(&text).expect("native-ui.json is JSON");
    ui["host_session_counts"][HOST].clone()
}

/// `n` sessions, `s1` shown in pane `p1` of tab `t1`.
fn with_sessions(n: usize) -> Fixture {
    Fixture {
        sessions: (1..=n).map(|i| session(&format!("s{i}")).build()).collect(),
        tabs: vec![tab("t1", &pane("p1", Some("s1")))],
        ..Fixture::default()
    }
}

fn import_open(h: &mut Harness<'_>) -> bool {
    h.root(|root, _| root.import_modal_open())
}

fn init_layouts(sent: &[ClientMessage]) -> Vec<&ClientMessage> {
    sent.iter()
        .filter(|msg| matches!(msg, ClientMessage::InitLayout { .. }))
        .collect()
}

/// A reconnect to the same host whose first list has `n` sessions,
/// followed by the client's tab list.
fn reconnect_with(h: &mut Harness<'_>, n: usize) {
    let fixture = with_sessions(n);
    h.send(support::welcome(support::PROTOCOL));
    h.send(DaemonMessage::Sessions {
        sessions: fixture.sessions,
    });
    h.send(DaemonMessage::Tabs { tabs: fixture.tabs });
}

#[gpui::test]
fn count_change_on_a_remote_host_offers_import(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_host_count(&dir, 1);
    let mut h = Harness::on_target(cx, &dir, remote_target(), &with_sessions(3));

    assert!(import_open(&mut h), "1 recorded, 3 arrived");
    assert!(drawn(&mut h, "import-keep"));
    assert!(drawn(&mut h, "import-open-all"));
    assert!(drawn(&mut h, "import-empty"));
    assert!(drawn(&mut h, "import-close"));
    let sent = h.sent();
    assert!(init_layouts(&sent).is_empty(), "sent {sent:?}");
    assert_eq!(
        saved_host_count(&dir),
        json!(3),
        "the new count is recorded"
    );
}

#[gpui::test]
fn keep_leaves_the_layout(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_host_count(&dir, 1);
    let mut h = Harness::on_target(cx, &dir, remote_target(), &with_sessions(3));
    assert!(import_open(&mut h));
    h.sent();
    h.keys("escape");
    assert!(!import_open(&mut h), "Esc keeps");
    let sent = h.sent();
    assert!(sent.is_empty(), "sent {sent:?}");

    reconnect_with(&mut h, 4);
    assert!(import_open(&mut h), "3 recorded, 4 arrived");
    h.sent();
    h.click_on("import-keep");
    assert!(!import_open(&mut h));
    let sent = h.sent();
    assert!(sent.is_empty(), "sent {sent:?}");

    reconnect_with(&mut h, 2);
    assert!(import_open(&mut h), "4 recorded, 2 arrived");
    h.sent();
    h.click_on("import-close");
    assert!(!import_open(&mut h));
    let sent = h.sent();
    assert!(sent.is_empty(), "sent {sent:?}");
}

#[gpui::test]
fn import_arranges_the_incoming_sessions(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_host_count(&dir, 1);
    let mut h = Harness::on_target(cx, &dir, remote_target(), &with_sessions(5));
    assert!(import_open(&mut h));
    h.sent();
    h.click_on("import-open-all");
    h.click_on("chooser-grid-shape-2");
    h.click_on("chooser-max-up");
    h.click_on("chooser-max-up");
    h.click_on("chooser-confirm-all");
    let sent = h.sent();
    assert!(
        matches!(
            sent.as_slice(),
            [ClientMessage::InitLayout {
                kind: InitLayoutKind::AllSessions
            }]
        ),
        "sent {sent:?}"
    );
    assert!(!import_open(&mut h));

    let row = |a: &str, b: &str| split(SplitDirection::Horizontal, pane(a, None), pane(b, None));
    let grid = split(
        SplitDirection::Vertical,
        row("p1", "p2"),
        split(SplitDirection::Vertical, row("p3", "p4"), pane("p5", None)),
    );
    h.send(DaemonMessage::Tabs {
        tabs: vec![tab("t1", &grid)],
    });
    let layout = RearrangeLayout::Grid {
        cols: 2,
        aspect: None,
    };
    let sent = h.sent();
    let extracted: Vec<Vec<String>> = sent
        .iter()
        .filter_map(|m| match m {
            ClientMessage::ExtractToNewTab {
                source_tab_id,
                pane_ids,
                name: None,
                layout: Some(l),
                request_id: None,
            } if source_tab_id == "t1" && *l == layout => Some(pane_ids.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        extracted,
        [
            vec!["p3".to_owned(), "p4".to_owned()],
            vec!["p5".to_owned()]
        ],
        "sent {sent:?}"
    );
    // The reply's panes show no session, so `s1`'s old pane also detaches.
    let arranged: Vec<&ClientMessage> = sent
        .iter()
        .filter(|m| {
            matches!(
                m,
                ClientMessage::ExtractToNewTab { .. } | ClientMessage::RearrangeTab { .. }
            )
        })
        .collect();
    assert_eq!(arranged.len(), 3, "sent {sent:?}");
    assert!(
        matches!(
            arranged.last(),
            Some(ClientMessage::RearrangeTab { tab_id, layout: l }) if tab_id == "t1" && *l == layout
        ),
        "sent {sent:?}"
    );
    assert!(
        !import_open(&mut h),
        "the reply to the import offers nothing"
    );
}

#[gpui::test]
fn start_empty_sends_empty(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_host_count(&dir, 4);
    let mut h = Harness::on_target(cx, &dir, remote_target(), &with_sessions(3));
    assert!(import_open(&mut h));
    h.sent();
    h.click_on("import-empty");
    let sent = h.sent();
    assert!(
        matches!(
            sent.as_slice(),
            [ClientMessage::InitLayout {
                kind: InitLayoutKind::Empty
            }]
        ),
        "sent {sent:?}"
    );
    assert!(!import_open(&mut h));
}

#[gpui::test]
fn an_unrecorded_host_records_its_count_silently(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_saved_host(&dir, &json!({}));
    let mut h = Harness::on_target(cx, &dir, remote_target(), &with_sessions(3));
    assert!(!import_open(&mut h));
    assert_eq!(saved_host_count(&dir), json!(3));

    h.send(DaemonMessage::SessionUpdated {
        session: session("s4").build(),
        request_id: None,
    });
    h.advance(Duration::from_millis(600));
    assert_eq!(saved_host_count(&dir), json!(4), "a new session counts");
    h.send(DaemonMessage::SessionRemoved {
        session_id: "s1".to_owned(),
    });
    h.advance(Duration::from_millis(600));
    assert_eq!(saved_host_count(&dir), json!(3), "a removed one too");
}

#[gpui::test]
fn a_host_not_saved_records_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::on_target(cx, &dir, remote_target(), &with_sessions(3));
    h.send(DaemonMessage::SessionUpdated {
        session: session("s4").build(),
        request_id: None,
    });
    h.advance(Duration::from_millis(600));
    let ui: serde_json::Value = std::fs::read_to_string(dir.path().join("native-ui.json"))
        .ok()
        .map_or(json!({}), |text| {
            serde_json::from_str(&text).expect("native-ui.json is JSON")
        });
    assert!(
        ui["host_session_counts"][HOST].is_null(),
        "a host not saved is not recorded: {ui}"
    );
}

#[gpui::test]
fn a_dialog_closing_over_the_import_hands_it_the_keyboard(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_host_count(&dir, 1);
    let mut h = Harness::on_target(cx, &dir, remote_target(), &with_sessions(3));
    let import_focused = |h: &mut Harness<'_>| {
        let root = h.root.clone();
        h.cx.update(|window, cx| root.read(cx).import_modal_focused(window))
    };
    assert!(import_focused(&mut h), "the modal takes the keyboard");

    cleanup_failed(&mut h);
    assert!(!import_focused(&mut h), "the cleanup dialog has it");
    h.click_on("cleanup-close");
    assert!(import_open(&mut h));
    assert!(
        import_focused(&mut h),
        "back to the import modal, not the pane"
    );
}

#[gpui::test]
fn no_offer_when_the_chooser_is_due(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_host_count(&dir, 1);
    let mut h = Harness::open(cx, &dir);
    h.event(NetEvent::State(remote()));
    h.send(support::welcome(support::PROTOCOL));
    h.send(DaemonMessage::Sessions {
        sessions: with_sessions(3).sessions,
    });
    h.send(DaemonMessage::LayoutInitRequired {
        has_legacy: false,
        active_session_count: 3,
        clonable: Vec::new(),
    });
    assert!(h.root(|root, _| root.layout_chooser_open()));
    assert!(!import_open(&mut h));

    h.keys("enter");
    h.send(DaemonMessage::Tabs { tabs: Vec::new() });
    assert!(!import_open(&mut h), "the chooser's answer offers nothing");
}

#[gpui::test]
fn a_resync_sessions_list_offers_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_host_count(&dir, 3);
    let fixture = with_sessions(3);
    let mut h = Harness::on_target(cx, &dir, remote_target(), &fixture);
    assert!(!import_open(&mut h), "same count");

    h.send(DaemonMessage::Sessions {
        sessions: with_sessions(5).sessions,
    });
    h.send(DaemonMessage::Tabs { tabs: fixture.tabs });
    assert!(!import_open(&mut h), "a later list is a resync");
    assert_eq!(
        saved_host_count(&dir),
        json!(5),
        "a resync still records the count"
    );
}

#[gpui::test]
fn no_offer_on_this_pc(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_host_count(&dir, 1);
    let mut h = Harness::on_target(cx, &dir, Target::Local, &with_sessions(3));
    assert!(!import_open(&mut h));
    assert_eq!(saved_host_count(&dir), json!(1), "this PC records nothing");
}
