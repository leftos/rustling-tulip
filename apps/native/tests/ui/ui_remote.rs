//! Remote-host specs: a remote host held beside This PC never has its files
//! opened, revealed or picked on this PC, never has its daemon stopped or
//! shut down (a window close only disconnects), draws no connecting overlay,
//! and has its session count recorded for its Connections row.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use std::time::Duration;

use gpui::{Modifiers, TestAppContext, px};
use protocol::{ClientMessage, DaemonMessage, RepoEntry, SessionSnapshot, WorktreeCleanupFailure};
use rustling_tulip_native::target::Target;
use rustling_tulip_native::{Connection, NetCommand, NetEvent};
use serde_json::json;
use support::{Fixture, Harness, Opened, TestDir, pane, remote_in_pane, session, tab};

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
