//! Remote-host specs: a remote host held beside This PC never has its files
//! opened, revealed or picked on this PC, never has its daemon stopped or
//! shut down (a window close only disconnects), draws no connecting overlay,
//! and has its session count recorded for its Connections row.

#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use std::time::Duration;

use gpui::{Modifiers, TestAppContext, px};
use protocol::{
    ClientMessage, DaemonMessage, RepoEntry, SessionSnapshot, SpawnRequest, SpawnTarget,
    SplitDirection, WorktreeCleanupFailure,
};
use rustling_tulip_native::target::Target;
use rustling_tulip_native::{Connection, NetCommand, NetEvent, RootView};
use serde_json::json;
use support::{
    Fixture, Harness, Opened, TestDir, pane, remote_in_pane, remote_pane, repo, session, split, tab,
};

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

/// [`HOST`] as a spec connects it beside This PC.
fn remote_target() -> Target {
    Target::tailnet(HOST, None)
}

/// [`HOST`] as the window dials it at launch once it is saved.
fn saved_target() -> Target {
    Target::tailnet(HOST, Some(8787))
}

/// Connects [`HOST`] beside This PC, its daemon listing `repos` and
/// `sessions`; what the client sent is drained.
fn remote_beside(h: &mut Harness<'_>, repos: Vec<RepoEntry>, sessions: Vec<SessionSnapshot>) {
    let mut host = h.host(&remote_target());
    host.connect();
    host.send(DaemonMessage::Repos { repos });
    host.send(DaemonMessage::Sessions { sessions });
    host.sent();
    h.sent();
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

    remote_beside(
        &mut h,
        Vec::new(),
        vec![session("b1").worktree("r2").build()],
    );
    h.right_click_on("leaf-b1");
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

/// `s` in pane `p2` beside This PC's `a1`, on the saved host [`HOST`]
/// listing the repo `r1`, its scrollback answered and `p2` focused; no
/// other host is saved.
fn attached_remote<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    s: SessionSnapshot,
) -> Harness<'a> {
    remote_in_pane(cx, dir, &json!({}), &["r1"], s)
}

#[gpui::test]
fn path_link_fetches_instead_of_opening_on_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached_remote(cx, &dir, session("s1").worktree("r1").build());
    h.host(&saved_target())
        .pty("s1", br"wrote C:\wt\x\docs\notes.md");
    let on = h.cell_center("p2", 9, 0);
    h.click(on, Modifiers::secondary_key());
    assert_eq!(h.opened(), [], "a host path never opens on this PC");
    let fetches: Vec<(String, Option<String>, String)> = h
        .host(&saved_target())
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
        !h.sent()
            .iter()
            .any(|msg| matches!(msg, ClientMessage::FetchFile { .. })),
        "nothing is fetched from This PC"
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
fn url_link_opens_on_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached_remote(cx, &dir, session("s1").build());
    h.host(&saved_target())
        .pty("s1", format!("see {URL} here").as_bytes());
    let on = h.cell_center("p2", 10, 0);
    h.click(on, Modifiers::secondary_key());
    assert_eq!(h.opened(), [Opened::Url(URL.to_owned())]);
    assert_eq!(toasts(&mut h), []);
}

#[gpui::test]
fn quit_does_not_shut_down_remote_daemon(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    remote_beside(
        &mut h,
        Vec::new(),
        vec![session("b1").in_repo("r1").build()],
    );
    h.host(&remote_target()).commands();

    let closed = h.cx.simulate_close();
    h.cx.run_until_parked();

    assert!(closed, "the window closes at once");
    assert_eq!(h.quit_requests(), 1);
    assert!(
        !h.root(|root, _| root.exit_dialog_open()),
        "no exit dialog asks about the host's sessions"
    );
    let commands = h.host(&remote_target()).commands();
    assert!(
        !commands
            .iter()
            .any(|command| matches!(command, NetCommand::Shutdown { .. } | NetCommand::Stop)),
        "the host's daemon is neither shut down nor stopped: {commands:?}"
    );
}

/// A connection to `target` that is still connecting.
fn connecting(target: Target) -> Connection {
    let mut conn = Connection::for_target(target);
    conn.on_connecting();
    conn
}

#[gpui::test]
fn a_remote_host_never_draws_the_overlay(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture::single(session("s1").build());
    let mut h = Harness::with(cx, &dir, &fixture);
    let host = remote_target();

    h.host(&host)
        .event(NetEvent::State(connecting(host.clone())));
    assert!(
        !drawn(&mut h, "connecting-overlay"),
        "a remote host connecting beside This PC draws no overlay"
    );
    h.host(&host).connect();
    h.host(&host).event(NetEvent::State(dropped_remote()));
    assert!(!drawn(&mut h, "connecting-overlay"), "nor does its drop");
    h.host(&host).connect();
    assert!(!drawn(&mut h, "connecting-overlay"), "nor its reconnect");
    assert!(
        shows(&mut h, "daemon-pill", "daemon · 1 session · :4242"),
        "This PC's pill is unchanged"
    );
}

/// A connection to the tailnet host [`HOST`] that was open and dropped: what
/// the user reconnects from.
fn dropped_remote() -> Connection {
    let mut conn = open_on(remote_target(), 8787);
    conn.on_closed("socket closed".to_owned());
    conn
}

/// A failed cleanup of one worktree folder, held by nothing.
fn cleanup_failure() -> DaemonMessage {
    DaemonMessage::WorktreeCleanupFailed {
        session_id: "s1".to_owned(),
        session_label: "feat/x".to_owned(),
        failures: vec![WorktreeCleanupFailure {
            member_path: "C:/wts/wt.feat-x/a".to_owned(),
            repo_path: "C:/src/a".to_owned(),
            reason: "fs::remove_dir_all: access denied".to_owned(),
            locking_processes: Vec::new(),
        }],
    }
}

#[gpui::test]
fn cleanup_open_folder_disabled_on_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    remote_beside(&mut h, Vec::new(), Vec::new());
    h.host(&remote_target()).send(cleanup_failure());
    assert!(tooltip_on(&mut h, "cleanup-open-folder-0", LOCAL_FILES));
    h.click_on("cleanup-open-folder-0");
    assert_eq!(h.opened(), [], "the host's folder is not revealed here");
    assert_eq!(
        toasts(&mut h),
        [],
        "the button takes no press, so no open is even refused"
    );
}

fn cleanup_focus(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.cleanup_failed_focus())
}

#[gpui::test]
fn cleanup_open_folder_leaves_the_tab_ring_on_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    h.send(cleanup_failure());
    assert_eq!(cleanup_focus(&mut h).as_deref(), Some("cleanup-ignore"));
    h.keys("shift-tab");
    assert_eq!(
        cleanup_focus(&mut h).as_deref(),
        Some("cleanup-open-folder-0"),
        "Shift+Tab from Ignore reaches Open folder on the local daemon"
    );
    h.click_on("cleanup-close");

    remote_beside(&mut h, Vec::new(), Vec::new());
    h.host(&remote_target()).send(cleanup_failure());
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

/// `native-ui.json` as the window last saved it, or empty when it saved
/// none.
fn saved_ui(dir: &TestDir) -> serde_json::Value {
    std::fs::read_to_string(dir.path().join("native-ui.json"))
        .ok()
        .map_or(json!({}), |text| {
            serde_json::from_str(&text).expect("native-ui.json is JSON")
        })
}

/// [`HOST`]'s session count as `native-ui.json` holds it on disk.
fn saved_host_count(dir: &TestDir) -> serde_json::Value {
    saved_ui(dir)["host_session_counts"][HOST].clone()
}

/// `n` sessions with no tab.
fn sessions(n: usize) -> Vec<SessionSnapshot> {
    (1..=n).map(|i| session(&format!("b{i}")).build()).collect()
}

#[gpui::test]
fn a_saved_hosts_session_list_records_its_count(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_saved_host(&dir, &json!({}));
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    let mut host = h.host(&saved_target());
    host.connect();
    host.send(DaemonMessage::Sessions {
        sessions: sessions(3),
    });
    assert_eq!(saved_host_count(&dir), json!(3));

    host.send(DaemonMessage::Sessions {
        sessions: sessions(5),
    });
    assert_eq!(saved_host_count(&dir), json!(5), "a later list records too");
}

#[gpui::test]
fn an_ssh_host_records_its_session_count_under_a_prefixed_key(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let ui = json!({
        "saved_hosts": [
            { "name": HOST, "port": 8787 },
            { "kind": "ssh", "name": "devbox" },
        ],
        "host_session_counts": {},
    });
    std::fs::write(
        dir.path().join("native-ui.json"),
        serde_json::to_vec_pretty(&ui).expect("serialize native-ui.json"),
    )
    .expect("write native-ui.json");
    let ssh = Target::parse_ssh("devbox", None).expect("an SSH host");
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    let mut host = h.host(&ssh);
    host.connect();
    host.send(DaemonMessage::Sessions {
        sessions: sessions(3),
    });

    assert_eq!(
        saved_ui(&dir)["host_session_counts"],
        json!({ "ssh:devbox": 3 }),
        "recorded under the SSH key alone"
    );
}

#[gpui::test]
fn a_host_not_saved_records_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    remote_beside(&mut h, Vec::new(), sessions(3));
    let ui = saved_ui(&dir);
    assert!(
        ui["host_session_counts"][HOST].is_null(),
        "a host not saved is not recorded: {ui}"
    );
}

#[gpui::test]
fn this_pcs_session_list_records_no_count(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_saved_host(&dir, &json!({ HOST: 1 }));
    let fixture = Fixture {
        sessions: (1..=3).map(|i| session(&format!("s{i}")).build()).collect(),
        tabs: vec![tab("t1", &pane("p1", Some("s1")))],
        ..Fixture::default()
    };
    let _h = Harness::with(cx, &dir, &fixture);
    assert_eq!(saved_host_count(&dir), json!(1), "this PC records nothing");
}

/// This PC registers no repo and shows `a1` in pane `p1`, beside [`HOST`]'s
/// `b1` in pane `p2`; [`HOST`] is connected (not saved) and lists no repo.
/// `focus` is clicked, so its session's host is the focused one, and
/// nothing sent is left unread on either host.
fn split_with_remote<'a>(cx: &'a mut TestAppContext, dir: &TestDir, focus: &str) -> Harness<'a> {
    let fixture = Fixture {
        sessions: vec![session("a1").build()],
        tabs: vec![tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                pane("p1", Some("a1")),
                remote_pane("p2", "b1"),
            ),
        )],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, dir, &fixture);
    h.answer_scrollback("a1", b"");
    let mut host = h.host(&remote_target());
    host.connect();
    host.send(DaemonMessage::Repos { repos: Vec::new() });
    host.send(DaemonMessage::Sessions {
        sessions: vec![session("b1").build()],
    });
    host.answer_scrollback("b1", b"");
    h.cx.update(|window, _| window.activate_window());
    focus_pane(&mut h, focus);
    h
}

/// Clicks pane `pane`, which focuses it, and drains what both hosts sent.
fn focus_pane(h: &mut Harness<'_>, pane: &str) {
    let at = h.cell_center(pane, 0, 0);
    h.click(at, Modifiers::none());
    h.sent();
    h.host(&remote_target()).sent();
}

/// [`split_with_remote`] with [`HOST`]'s pane `p2` focused.
fn remote_focused<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    split_with_remote(cx, dir, "p2")
}

fn prompt_open(h: &mut Harness<'_>) -> bool {
    h.root(|root, _| root.host_path_prompt_open())
}

fn prompt_error(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.host_path_prompt_error())
}

/// The paths of every `AddRepo` among `sent`.
fn add_repo_paths(sent: &[ClientMessage]) -> Vec<String> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::AddRepo { path, .. } => Some(path.clone()),
            _ => None,
        })
        .collect()
}

/// Types `text` into the field that has the keyboard, one key per char.
fn type_text(h: &mut Harness<'_>, text: &str) {
    let keys: Vec<String> = text.chars().map(|c| c.to_string()).collect();
    h.keys(&keys.join(" "));
}

/// Add repo from the ⋯ menu, which acts on the focused session's host.
fn add_repo_from_more(h: &mut Harness<'_>) {
    h.click_on("sidebar-more");
    h.click_on("sidebar-more-add-repo");
}

/// The saved host [`HOST`], switched off, with its session `b1` alone in
/// tab `t1`'s pane `p1`: the window holds no connection to it.
fn switched_off_pane<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    seed_saved_off(dir);
    let fixture = Fixture {
        tabs: vec![tab("t1", &remote_pane("p1", "b1"))],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, dir, &fixture);
    h.set_picked_folder(None);
    h.sent();
    h
}

/// Saves [`HOST`] switched off before the window opens.
fn seed_saved_off(dir: &TestDir) {
    std::fs::write(
        dir.path().join("native-ui.json"),
        json!({ "saved_hosts": [{ "name": HOST, "port": 8787, "enabled": false }] }).to_string(),
    )
    .expect("seed native-ui.json");
}

#[gpui::test]
fn add_repo_on_a_switched_off_hosts_pane_refuses(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = switched_off_pane(cx, &dir);
    add_repo_from_more(&mut h);
    assert_eq!(
        h.folder_asks(),
        0,
        "no picker of this PC opens for the host"
    );
    assert!(prompt_open(&mut h), "the host's path prompt opens");
    type_text(&mut h, "/srv/app");
    h.keys("enter");
    assert!(
        add_repo_paths(&h.sent()).is_empty(),
        "This PC registers nothing"
    );
    let offline = format!("{HOST} is offline");
    assert!(
        toasts(&mut h)
            .iter()
            .any(|(_, detail)| detail.as_deref() == Some(offline.as_str())),
        "the offline toast says why: {:?}",
        toasts(&mut h)
    );
}

#[gpui::test]
fn the_shell_dialog_on_a_switched_off_host_offers_no_browse(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = switched_off_pane(cx, &dir);
    h.open_shell_dialog_from_more();
    assert!(!h.root(|root, _| root.shell_dialog_saves_default()));
    h.click_on("shell-browse");
    assert_eq!(h.folder_asks(), 0, "Browse is disabled for the host");
    h.keys("tab");
    assert_eq!(
        shell_focus(&mut h),
        Some("shell-cancel"),
        "Tab skips Browse and the quick-shell default"
    );
}

/// The tailnet host `lab`, beside [`HOST`].
fn lab_target() -> Target {
    Target::tailnet("lab.tail1234.ts.net", None)
}

#[gpui::test]
fn the_host_row_leaves_out_an_offline_or_unpaired_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = split_with_remote(cx, &dir, "p1");
    h.host(&lab_target()).connect();
    let mut dropped = open_on(lab_target(), 8787);
    dropped.on_closed("socket closed".to_owned());
    h.host(&lab_target()).event(NetEvent::State(dropped));

    h.open_shell_dialog_from_more();
    assert!(drawn(&mut h, "shell-host-local") && drawn(&mut h, &remote_segment()));
    assert!(
        !drawn(&mut h, "shell-host-lab.tail1234.ts.net"),
        "an offline host is not offered"
    );
}

#[gpui::test]
fn the_host_row_is_absent_with_this_pc_alone_online(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = split_with_remote(cx, &dir, "p1");
    h.host(&remote_target())
        .event(NetEvent::State(dropped_remote()));
    h.open_shell_dialog_from_more();
    assert!(
        !drawn(&mut h, "shell-host-row"),
        "This PC alone online: no host row"
    );
}

#[gpui::test]
fn switching_host_and_back_keeps_the_default_checkbox(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = split_with_remote(cx, &dir, "p1");
    h.open_shell_dialog_from_more();
    assert!(h.root(|root, _| root.shell_dialog_saves_default()));
    h.click_on("shell-save-default");
    assert!(!h.root(|root, _| root.shell_dialog_saves_default()));
    h.click_on(&remote_segment());
    h.click_on("shell-host-local");
    assert!(
        !h.root(|root, _| root.shell_dialog_saves_default()),
        "the untick survives a trip to the host and back"
    );
}

#[gpui::test]
fn an_offline_shell_submit_keeps_the_dialog_open(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_focused(cx, &dir);
    h.open_shell_dialog_from_more();
    type_text(&mut h, "/srv");
    h.host(&remote_target())
        .event(NetEvent::State(dropped_remote()));
    h.click_on("shell-submit");
    assert!(
        h.root(|root, _| root.shell_dialog_open()),
        "the dialog stays"
    );
    assert_eq!(
        h.root(RootView::shell_dialog_folder).as_deref(),
        Some("/srv"),
        "with its folder"
    );
    let offline = format!("{HOST} is offline");
    assert!(
        toasts(&mut h)
            .iter()
            .any(|(_, detail)| detail.as_deref() == Some(offline.as_str())),
        "the offline toast says why: {:?}",
        toasts(&mut h)
    );
    let sent = shell_cwds(&h.host(&remote_target()).sent());
    assert_eq!(sent.len(), 0, "no shell reaches the offline host: {sent:?}");
}

const RELATIVE_REFUSAL: &str =
    "Use a full path on box, starting with a drive letter, \\\\server or /.";

#[gpui::test]
fn add_repo_on_remote_opens_the_host_path_prompt(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_focused(cx, &dir);
    h.set_picked_folder(None);
    assert!(!prompt_open(&mut h));

    h.click_on("sidebar-add-repo");
    assert!(
        !prompt_open(&mut h),
        "the sidebar note is This PC's, so its button opens no prompt"
    );
    assert_eq!(h.folder_asks(), 1, "it opens this PC's folder picker");

    h.click_on("sidebar-more");
    h.click_on("sidebar-more-add-repo");
    assert!(prompt_open(&mut h), "the ⋯ menu's row opens it");
    assert_eq!(
        h.root(|root, _| root.host_path_prompt_title()).as_deref(),
        Some("Add a repo on box")
    );
    assert!(drawn(&mut h, "host-path-input"));
    assert!(
        !h.root(|root, _| root.sidebar_more_open()),
        "and closes the menu"
    );
    h.keys("escape");

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

    assert_eq!(
        h.folder_asks(),
        1,
        "no folder picker opens for the host's controls"
    );
    let local = add_repo_paths(&h.sent());
    let remote = add_repo_paths(&h.host(&remote_target()).sent());
    assert!(
        local.is_empty() && remote.is_empty(),
        "nothing is registered: This PC {local:?}, the host {remote:?}"
    );
}

#[gpui::test]
fn closing_the_host_path_prompt_returns_focus_to_the_spawn_dialog(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_focused(cx, &dir);
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
            root.pane_holds_keyboard("p2", window, cx),
        )
    });
    assert!(dialog, "the no-repos dialog takes the keyboard back");
    assert!(!pane, "the pane under the dialog does not");
}

#[gpui::test]
fn add_repo_prompt_sends_add_repo_with_the_typed_path(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_focused(cx, &dir);
    add_repo_from_more(&mut h);
    type_text(&mut h, "/srv/app/");
    h.keys("enter");
    assert!(!prompt_open(&mut h), "Add closes the prompt");
    let sent = h.host(&remote_target()).sent();
    assert!(
        sent.iter().any(|msg| matches!(
            msg,
            ClientMessage::AddRepo { path, name: Some(name) }
                if path == "/srv/app/" && name == "app"
        )),
        "the typed path goes to the host, named by its last segment: sent {sent:?}"
    );
    assert_eq!(
        add_repo_paths(&h.sent()),
        Vec::<String>::new(),
        "This PC gets none"
    );

    add_repo_from_more(&mut h);
    type_text(&mut h, "/srv/web");
    h.keys("tab tab");
    assert_eq!(
        h.root(|root, _| root.host_path_prompt_focus()).as_deref(),
        Some("host-path-add")
    );
    h.keys("enter");
    assert!(!prompt_open(&mut h));
    assert_eq!(
        add_repo_paths(&h.host(&remote_target()).sent()),
        ["/srv/web"],
        "Add pressed from the keyboard sends too"
    );
}

#[gpui::test]
fn add_repo_prompt_refuses_a_relative_path_inline(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_focused(cx, &dir);
    add_repo_from_more(&mut h);
    type_text(&mut h, "srv/app");
    h.keys("enter");
    assert!(prompt_open(&mut h), "the prompt stays");
    assert_eq!(prompt_error(&mut h).as_deref(), Some(RELATIVE_REFUSAL));
    assert!(drawn(&mut h, "host-path-error"));
    assert!(
        add_repo_paths(&h.host(&remote_target()).sent()).is_empty(),
        "nothing is registered"
    );

    type_text(&mut h, "x");
    assert_eq!(prompt_error(&mut h), None, "an edit clears the refusal");

    h.click_on("host-path-add");
    assert_eq!(
        prompt_error(&mut h).as_deref(),
        Some(RELATIVE_REFUSAL),
        "a click on Add checks the path too"
    );
}

#[gpui::test]
fn add_repo_prompt_escape_closes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_focused(cx, &dir);
    add_repo_from_more(&mut h);
    type_text(&mut h, "/srv/app");
    h.keys("escape");
    assert!(!prompt_open(&mut h));
    assert!(
        add_repo_paths(&h.host(&remote_target()).sent()).is_empty(),
        "nothing is registered"
    );

    add_repo_from_more(&mut h);
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
    let mut h = remote_focused(cx, &dir);
    add_repo_from_more(&mut h);
    assert_eq!(
        h.root(RootView::host_path_prompt_text).as_deref(),
        Some(""),
        "this PC's folder is not offered on the host"
    );
    type_text(&mut h, "/srv/app");
    h.keys("enter");
    assert_eq!(
        add_repo_paths(&h.host(&remote_target()).sent()),
        ["/srv/app"]
    );
    let saved = std::fs::read_to_string(&ui).unwrap_or_default();
    assert!(
        !saved.contains("/srv"),
        "a host path is never remembered: {saved}"
    );
}

#[gpui::test]
fn no_repos_dialog_keys_open_the_host_path_prompt_on_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = split_with_remote(cx, &dir, "p1");
    h.keys("ctrl-shift-n");
    assert!(h.root(|root, _| root.spawn_dialog_no_repos()));
    h.keys("enter");
    assert_eq!(
        h.folder_asks(),
        1,
        "Enter presses Add repo on This PC's focused session"
    );
    assert!(!prompt_open(&mut h));
    h.keys("escape");

    focus_pane(&mut h, "p2");
    h.keys("ctrl-shift-n");
    assert!(h.root(|root, _| root.spawn_dialog_no_repos()));
    h.keys("space");
    assert!(
        prompt_open(&mut h),
        "Space presses Add repo on the focused session's remote host"
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

#[gpui::test]
fn a_host_groups_add_repo_acts_on_that_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_in_pane(cx, &dir, &json!({}), &[], session("b1").build());
    // This PC's session is focused, so only the header picks the host.
    let at = h.cell_center("p1", 0, 0);
    h.click(at, Modifiers::none());
    h.set_picked_folder(None);

    h.click_on(&format!("host-add-repo-{HOST}"));
    assert_eq!(
        h.root(|root, _| root.host_path_prompt_title()).as_deref(),
        Some("Add a repo on box"),
        "the remote header's Add repo opens the prompt for that host"
    );
    type_text(&mut h, "/srv/app");
    h.keys("enter");
    assert_eq!(
        add_repo_paths(&h.host(&saved_target()).sent()),
        ["/srv/app"]
    );
    assert!(add_repo_paths(&h.sent()).is_empty(), "This PC gets none");

    h.click_on("host-add-repo-local");
    assert!(!prompt_open(&mut h), "This PC's header opens no prompt");
    assert_eq!(h.folder_asks(), 1, "it opens this PC's folder picker");
}

#[gpui::test]
fn add_repo_on_an_offline_host_refuses(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_focused(cx, &dir);
    add_repo_from_more(&mut h);
    type_text(&mut h, "/srv/app");
    h.host(&remote_target())
        .event(NetEvent::State(dropped_remote()));
    h.keys("enter");

    assert!(
        add_repo_paths(&h.host(&remote_target()).sent()).is_empty(),
        "nothing goes to the offline host"
    );
    let offline = format!("{HOST} is offline");
    assert!(
        toasts(&mut h)
            .iter()
            .any(|(_, detail)| detail.as_deref() == Some(offline.as_str())),
        "the offline toast says why: {:?}",
        toasts(&mut h)
    );
    assert!(prompt_open(&mut h), "the prompt keeps the typed path");
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
    saved_ui(dir)["quick_shell_dir"].as_str().map(str::to_owned)
}

fn shell_spawns(sent: &[ClientMessage]) -> Vec<SpawnRequest> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::SpawnSession(request) => Some(request.clone()),
            _ => None,
        })
        .collect()
}

/// The folder of every standalone shell spawn among `sent`.
fn shell_cwds(sent: &[ClientMessage]) -> Vec<Option<String>> {
    shell_spawns(sent)
        .into_iter()
        .filter_map(|request| match request.target {
            SpawnTarget::Standalone { cwd, .. } => Some(cwd),
            _ => None,
        })
        .collect()
}

fn shell_focus(h: &mut Harness<'_>) -> Option<&'static str> {
    h.root(RootView::shell_dialog_focus)
}

/// The selector of [`HOST`]'s segment in the Shell dialog's host row.
fn remote_segment() -> String {
    format!("shell-host-{HOST}")
}

#[gpui::test]
fn shell_browse_disabled_on_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_focused(cx, &dir);
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
fn shell_folder_on_remote_follows_the_host_path_rules(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_focused(cx, &dir);
    h.open_shell_dialog_from_more();
    type_text(&mut h, "work");
    assert!(!h.root(RootView::shell_dialog_can_submit));
    assert_eq!(
        h.root(RootView::shell_dialog_hint).as_deref(),
        Some(RELATIVE_REFUSAL),
        "the hint names the host, as the Add repo prompt does"
    );
    h.keys("backspace backspace backspace backspace");
    type_text(&mut h, "/home/me");
    assert!(
        h.root(RootView::shell_dialog_can_submit),
        "a POSIX host path can go"
    );
    assert_eq!(h.root(RootView::shell_dialog_hint), None);
    h.click_on("shell-submit");
    assert_eq!(
        shell_cwds(&h.host(&remote_target()).sent()),
        [Some("/home/me".to_owned())],
        "the shell opens on the host"
    );
    assert_eq!(shell_cwds(&h.sent()), [], "This PC opens none");
}

#[gpui::test]
fn shell_browse_leaves_the_tab_ring_on_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = split_with_remote(cx, &dir, "p1");
    h.open_shell_dialog_from_more();
    assert_eq!(shell_focus(&mut h), Some("shell-folder"));
    h.keys("tab");
    assert_eq!(
        shell_focus(&mut h),
        Some("shell-browse"),
        "Tab reaches Browse on This PC"
    );
    h.keys("escape");

    focus_pane(&mut h, "p2");
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

#[gpui::test]
fn shell_dialog_leaves_the_remembered_folder_on_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    remember_quick_shell(&dir, "C:\\work");
    let mut h = remote_focused(cx, &dir);

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
    let request = match shell_spawns(&h.host(&remote_target()).sent()).as_slice() {
        [request] => request.clone(),
        other => panic!("expected one SpawnSession on the host, sent {other:?}"),
    };
    assert_eq!(
        request.target,
        SpawnTarget::Standalone {
            cwd: Some("D:/host".to_owned()),
            add_dirs: Vec::new(),
        }
    );
    h.host(&remote_target())
        .send(DaemonMessage::SessionUpdated {
            session: session("new").shell("D:/host").build(),
            request_id: request.request_id.clone(),
        });
    assert_eq!(
        remembered_quick_shell(&dir).as_deref(),
        Some("C:\\work"),
        "a shell on the host leaves this PC's quick-shell folder alone"
    );
}

#[gpui::test]
fn quick_shell_sends_no_cwd_on_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    remember_quick_shell(&dir, "C:\\work");
    let mut h = remote_focused(cx, &dir);

    h.click_on("sidebar-add-shell");
    assert_eq!(
        shell_cwds(&h.host(&remote_target()).sent()),
        [None],
        "this PC's folder is not the host's: the host's daemon picks its default"
    );
    assert_eq!(shell_cwds(&h.sent()), [], "This PC opens none");
}

#[gpui::test]
fn empty_pane_shell_here_sends_no_cwd_on_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    remember_quick_shell(&dir, "C:\\work");
    let fixture = Fixture {
        tabs: vec![tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                remote_pane("p1", "b1"),
                pane("p2", None),
            ),
        )],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    let mut host = h.host(&remote_target());
    host.connect();
    host.send(DaemonMessage::Sessions {
        sessions: vec![session("b1").build()],
    });
    host.answer_scrollback("b1", b"");
    h.cx.update(|window, _| window.activate_window());
    focus_pane(&mut h, "p1");

    h.click_on("empty-pane-shell-p2");
    assert_eq!(
        shell_cwds(&h.host(&remote_target()).sent()),
        [None],
        "the focused session's host gets the shell, with its own default folder"
    );
    assert_eq!(shell_cwds(&h.sent()), [], "This PC opens none");
}

#[gpui::test]
fn quick_shell_tooltip_names_no_local_folder_on_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    remember_quick_shell(&dir, "C:\\work");
    let mut h = remote_focused(cx, &dir);
    assert!(tooltip_on(
        &mut h,
        "sidebar-add-shell",
        "Open a standalone shell in box's home folder"
    ));
}

#[gpui::test]
fn the_quick_shell_follows_the_focused_sessions_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    remember_quick_shell(&dir, "C:\\work");
    let mut h = remote_focused(cx, &dir);
    h.click_on("sidebar-add-shell");
    assert_eq!(shell_cwds(&h.host(&remote_target()).sent()), [None]);
    assert_eq!(shell_cwds(&h.sent()), []);

    focus_pane(&mut h, "p1");
    h.click_on("sidebar-add-shell");
    assert_eq!(
        shell_cwds(&h.sent()),
        [Some("C:\\work".to_owned())],
        "This PC's session focused: the shell opens here, in the remembered folder"
    );
    assert_eq!(shell_cwds(&h.host(&remote_target()).sent()), []);
}

#[gpui::test]
fn the_shell_dialogs_host_row_picks_the_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = split_with_remote(cx, &dir, "p1");
    h.set_picked_folder(Some("D:/work"));
    h.open_shell_dialog_from_more();
    assert!(drawn(&mut h, "shell-host-local") && drawn(&mut h, &remote_segment()));
    assert!(
        h.root(|root, _| root.shell_dialog_saves_default()),
        "the dialog starts on the focused session's host, This PC"
    );

    h.click_on(&remote_segment());
    assert!(
        !h.root(|root, _| root.shell_dialog_saves_default()),
        "on the host, no quick-shell default is offered"
    );
    h.click_on("shell-browse");
    assert_eq!(h.folder_asks(), 0, "Browse is disabled on the host");
    h.click_on("shell-folder");
    type_text(&mut h, "/srv");
    h.click_on("shell-submit");
    assert_eq!(
        shell_cwds(&h.host(&remote_target()).sent()),
        [Some("/srv".to_owned())]
    );
    assert_eq!(shell_cwds(&h.sent()), [], "This PC opens none");
}

#[gpui::test]
fn changing_the_host_keeps_the_typed_folder(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = split_with_remote(cx, &dir, "p1");
    h.open_shell_dialog_from_more();
    type_text(&mut h, "work");
    assert_eq!(
        h.root(RootView::shell_dialog_hint).as_deref(),
        Some("Enter a full path, like C:\\Users\\you")
    );

    h.click_on(&remote_segment());
    assert_eq!(
        h.root(RootView::shell_dialog_folder).as_deref(),
        Some("work"),
        "the typed folder stays"
    );
    assert_eq!(
        h.root(RootView::shell_dialog_hint).as_deref(),
        Some(RELATIVE_REFUSAL),
        "and is judged by the host's path rules"
    );

    h.click_on("shell-host-local");
    assert_eq!(
        h.root(RootView::shell_dialog_folder).as_deref(),
        Some("work")
    );
    assert!(h.root(|root, _| root.shell_dialog_saves_default()));
}

#[gpui::test]
fn the_shell_dialog_shows_no_host_row_with_one_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    h.open_shell_dialog_from_more();
    assert!(h.root(|root, _| root.shell_dialog_open()));
    assert!(
        !drawn(&mut h, "shell-host-row"),
        "This PC alone: no host row"
    );
    h.keys("escape");

    remote_beside(&mut h, Vec::new(), Vec::new());
    h.open_shell_dialog_from_more();
    assert!(
        drawn(&mut h, "shell-host-row"),
        "a second host adds the row"
    );
}

#[gpui::test]
fn no_repos_note_names_the_remote_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut local = Fixture::single(session("s1").build());
    local.repos = vec![repo("r1", "C:/r1")];
    let mut h = support::saved_remote_over(cx, &dir, &local, &[]);
    h.host(&support::saved_remote())
        .send(DaemonMessage::Repos { repos: Vec::new() });

    assert!(
        shows(
            &mut h,
            &format!("sidebar-host-no-repos-{HOST}"),
            "No repos or workspaces on box yet."
        ),
        "the empty host's group says whose registry is empty"
    );
    assert!(
        !drawn(&mut h, "sidebar-host-no-repos-local"),
        "This PC holds a repo"
    );
    assert!(
        !drawn(&mut h, "sidebar-no-repos"),
        "the window-wide note is This PC's alone"
    );
}

/// The focused pane shows [`HOST`]'s `b1`, the host holding `r1` and `r2`
/// and This PC none; `native-ui.json` starts as `ui`.
fn creator_on_remote<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    ui: &serde_json::Value,
) -> Harness<'a> {
    remote_in_pane(cx, dir, ui, &["r1", "r2"], session("b1").build())
}

/// The workspace creator open on its From VS Code tab.
fn vscode_creator(h: &mut Harness<'_>) {
    h.click_on("sidebar-more");
    h.click_on("sidebar-more-add-workspace");
    h.click_on("workspace-creator-tab-vscode");
}

fn parses(sent: &[ClientMessage]) -> Vec<String> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::ParseVscodeWorkspace { path } => Some(path.clone()),
            _ => None,
        })
        .collect()
}

#[gpui::test]
fn workspace_file_browse_not_drawn_on_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = creator_on_remote(cx, &dir, &json!({}));
    h.set_picked_file(Some("D:/work/team.code-workspace"));
    vscode_creator(&mut h);
    assert!(
        !drawn(&mut h, "workspace-creator-browse"),
        "the typed field takes Browse's place"
    );
    assert!(drawn(&mut h, "workspace-creator-file-input"));
    assert_eq!(h.file_asks(), 0, "no file picker opens on this PC");
}

#[gpui::test]
fn workspace_creator_on_remote_takes_a_typed_workspace_file(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = creator_on_remote(cx, &dir, &json!({ "last_vscode_dir": "D:/vs" }));
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

    assert_eq!(
        parses(&h.host(&saved_target()).sent()),
        ["/srv/team.code-workspace".to_owned()],
        "the typed file is parsed on the host"
    );
    assert!(parses(&h.sent()).is_empty(), "This PC parses nothing");
    assert!(h.root(|root, _| root.workspace_creator_parsing()));
    assert_eq!(
        saved_ui(&dir).get("last_vscode_dir"),
        Some(&json!("D:/vs")),
        "a host path is never remembered"
    );
}

#[gpui::test]
fn workspace_creator_refuses_a_path_without_code_workspace(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = creator_on_remote(cx, &dir, &json!({}));
    vscode_creator(&mut h);
    h.keys("tab");
    type_text(&mut h, "/srv/team.json");
    h.keys("enter");
    assert_eq!(
        h.root(|root, _| root.workspace_creator_error()).as_deref(),
        Some("Type the path of a .code-workspace file.")
    );
    assert!(
        parses(&h.host(&saved_target()).sent()).is_empty(),
        "nothing is parsed"
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
fn workspace_file_browse_leaves_the_tab_ring_on_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = creator_on_remote(cx, &dir, &json!({}));
    vscode_creator(&mut h);
    assert_eq!(
        h.root(|root, _| root.workspace_creator_focus()).as_deref(),
        Some("workspace-creator-tab-vscode")
    );
    h.keys("tab");
    assert_eq!(
        h.root(|root, _| root.workspace_creator_focus()).as_deref(),
        Some("workspace-creator-file-input"),
        "Tab reaches the typed file field in Browse's place on a remote host"
    );
    h.keys("shift-tab");
    assert_eq!(
        h.root(|root, _| root.workspace_creator_focus()).as_deref(),
        Some("workspace-creator-tab-vscode"),
        "and Shift+Tab goes back to the tab"
    );
}

#[gpui::test]
fn workspace_creator_no_repos_note_names_the_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = creator_on_remote(cx, &dir, &json!({}));
    h.click_on("sidebar-more");
    h.click_on("sidebar-more-add-workspace");
    assert_eq!(
        h.root(|root, _| root.workspace_creator_members())
            .into_iter()
            .map(|(_, name, _)| name)
            .collect::<Vec<_>>(),
        ["r1", "r2"],
        "the members are the host's repos"
    );
    h.host(&saved_target())
        .send(DaemonMessage::Repos { repos: Vec::new() });
    assert!(shows(
        &mut h,
        "workspace-creator-no-repos",
        "No repos on box yet."
    ));
}

#[gpui::test]
fn this_pcs_no_repos_note_keeps_its_wording_beside_a_remote_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = support::saved_remote_over(cx, &dir, &Fixture::default(), &[]);
    h.host(&support::saved_remote()).send(DaemonMessage::Repos {
        repos: vec![repo("r2", "D:/r2")],
    });

    assert!(shows(
        &mut h,
        "sidebar-no-repos-note",
        "No repos or workspaces yet."
    ));
}
