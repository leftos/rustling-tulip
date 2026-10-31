//! Host group specs: the repos view's header row per host while more than
//! one host is configured, its fold, its `+ Shell` and `Pair` buttons, and
//! a container drag that stays within its own host.

use crate::support;

use gpui::{Pixels, Point, TestAppContext, point, px};
use protocol::{ClientMessage, ContainerRef, DaemonMessage, SpawnTarget};
use rustling_tulip_native::target::Target;
use rustling_tulip_native::{Connection, NetEvent, RootView};
use support::{Fixture, Harness, PROTOCOL, TestDir, repo, session};

/// The tailnet host the specs save beside This PC.
const REMOTE: &str = "box.tail1234.ts.net";

/// The saved host [`REMOTE`] as launch dials it.
fn saved_remote() -> Target {
    Target::tailnet(REMOTE, Some(8787))
}

/// The remote host's repo row, keyed with its host in front.
fn remote_repo_key() -> String {
    format!("{REMOTE}::repo:r2")
}

/// Writes `ui` as the `native-ui.json` the window opens over.
fn seed_ui(dir: &TestDir, ui: &serde_json::Value) {
    std::fs::write(dir.path().join("native-ui.json"), ui.to_string())
        .expect("write native-ui.json");
}

/// The `native-ui.json` the window last saved.
fn saved_ui(dir: &TestDir) -> serde_json::Value {
    let text = std::fs::read_to_string(dir.path().join("native-ui.json")).expect("a saved layout");
    serde_json::from_str(&text).expect("native-ui.json is JSON")
}

/// A window on This PC listing `a1` in repo `r1`, with [`REMOTE`] saved,
/// connected and listing `b1` in repo `r2`.
fn saved_remote_listing<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    seed_ui(
        dir,
        &serde_json::json!({ "saved_hosts": [{ "name": REMOTE, "port": 8787 }] }),
    );
    let mut h = Harness::with(
        cx,
        dir,
        &Fixture::single(session("a1").in_repo("r1").build()),
    );
    h.send(DaemonMessage::Repos {
        repos: vec![repo("r1", "C:/r1")],
    });
    let mut host = h.host(&saved_remote());
    host.connect();
    host.send(DaemonMessage::Repos {
        repos: vec![repo("r2", "C:/r2")],
    });
    host.send(DaemonMessage::Sessions {
        sessions: vec![session("b1").in_repo("r2").build()],
    });
    h.sent();
    h
}

/// Every container key the sidebar shows.
fn shown_keys(h: &mut Harness<'_>) -> Vec<String> {
    h.root(|root, _| {
        root.sidebar_containers()
            .into_iter()
            .map(|container| container.key)
            .collect()
    })
}

/// Whether the element tagged `selector` was ever painted; gpui keeps what
/// it painted, so a spec asks this only about an element it expects never
/// drawn, or one it expects on screen.
fn drawn(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).origin.x >= px(0.0)
}

/// A connection to `target` that was open and dropped.
fn dropped(target: Target) -> Connection {
    let mut conn = Connection::for_target(target);
    conn.on_connecting();
    conn.on_socket_open(8787);
    conn.on_welcome(PROTOCOL, Vec::new());
    let _ = conn.on_closed("socket closed".to_owned());
    conn
}

/// The plain-shell spawns among `sent`, by the folder each names.
fn shell_spawns(sent: Vec<ClientMessage>) -> Vec<Option<String>> {
    sent.into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::SpawnSession(request) => match request.target {
                SpawnTarget::Standalone { cwd, .. } => Some(cwd),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// The container orders among `sent`.
fn container_orders(sent: Vec<ClientMessage>) -> Vec<Vec<ContainerRef>> {
    sent.into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::ReorderContainers { ordered } => Some(ordered),
            _ => None,
        })
        .collect()
}

/// Drags the row tagged `from` onto the row tagged `to`, `fy` down it, and
/// holds it there.
fn drag_row_over(h: &mut Harness<'_>, from: &str, to: &str, fy: f32) -> Point<Pixels> {
    let row = |h: &mut Harness<'_>, selector: &str, fy: f32| {
        let bounds = h.bounds(selector);
        assert!(bounds.size.height > Pixels::ZERO, "{selector} is on screen");
        point(
            bounds.origin.x + bounds.size.width * 0.3,
            bounds.origin.y + bounds.size.height * fy,
        )
    };
    let start = row(h, from, 0.5);
    let end = row(h, to, fy);
    h.start_drag(start);
    assert!(h.dragging(), "{from} drags");
    h.drag_to(end);
    end
}

#[gpui::test]
fn one_host_draws_no_header(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(
        cx,
        &dir,
        &Fixture::single(session("a1").in_repo("r1").build()),
    );
    h.send(DaemonMessage::Repos {
        repos: vec![repo("r1", "C:/r1")],
    });

    assert!(drawn(&mut h, "container-repo:r1"), "the repo row shows");
    assert!(
        !drawn(&mut h, "host-header-local"),
        "with This PC alone there is no host header"
    );
    assert!(
        drawn(&mut h, "sidebar-add-shell"),
        "the toolbar keeps its Shell button"
    );
}

#[gpui::test]
fn two_hosts_draw_a_header_each_this_pc_first(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = saved_remote_listing(cx, &dir);

    let local = h.bounds("host-header-local");
    let remote = h.bounds(&format!("host-header-{REMOTE}"));
    let local_repo = h.bounds("container-repo:r1");
    let remote_repo = h.bounds(&format!("container-{}", remote_repo_key()));
    assert!(
        local.origin.x >= px(0.0) && remote.origin.x >= px(0.0),
        "both headers draw"
    );
    assert!(
        local.origin.y < local_repo.origin.y
            && local_repo.origin.y < remote.origin.y
            && remote.origin.y < remote_repo.origin.y,
        "This PC's header, its repo, then the remote host's header and its repo"
    );
    assert_eq!(local.size.height, px(26.0), "a header is 26 px tall");
    assert!(
        drawn(&mut h, "host-shell-local") && drawn(&mut h, &format!("host-shell-{REMOTE}")),
        "each header carries + Shell"
    );
    assert!(
        !drawn(&mut h, "sidebar-add-shell"),
        "This PC's header's + Shell replaces the toolbar's"
    );
}

#[gpui::test]
fn folding_a_host_hides_its_rows_and_persists(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = saved_remote_listing(cx, &dir);
    assert_eq!(
        shown_keys(&mut h),
        ["repo:r1".to_owned(), remote_repo_key()]
    );

    h.click_on(&format!("host-fold-{REMOTE}"));

    assert_eq!(
        shown_keys(&mut h),
        ["repo:r1"],
        "the folded host's rows leave the list and the keyboard walk"
    );
    let folds = saved_ui(&dir)["collapsed_containers"].clone();
    assert_eq!(
        folds,
        serde_json::json!([format!("host:{REMOTE}")]),
        "the fold is saved by the host's key"
    );
    assert!(
        drawn(&mut h, &format!("host-header-{REMOTE}")),
        "its header stays"
    );

    h.click_on(&format!("host-fold-{REMOTE}"));
    assert_eq!(
        shown_keys(&mut h),
        ["repo:r1".to_owned(), remote_repo_key()],
        "unfolding brings them back"
    );
}

#[gpui::test]
fn plus_shell_on_a_remote_header_spawns_there_with_no_folder(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = saved_remote_listing(cx, &dir);
    h.host(&saved_remote()).sent();

    h.click_on(&format!("host-shell-{REMOTE}"));

    assert_eq!(
        shell_spawns(h.host(&saved_remote()).sent()),
        [None],
        "one plain shell goes to the remote host, with no folder"
    );
    assert_eq!(
        shell_spawns(h.sent()),
        Vec::<Option<String>>::new(),
        "This PC is sent none"
    );
}

#[gpui::test]
fn plus_shell_is_disabled_on_an_offline_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = saved_remote_listing(cx, &dir);
    h.host(&saved_remote())
        .event(NetEvent::State(dropped(saved_remote())));
    h.host(&saved_remote()).sent();

    h.click_on(&format!("host-shell-{REMOTE}"));

    assert_eq!(
        shell_spawns(h.host(&saved_remote()).sent()),
        Vec::<Option<String>>::new(),
        "an offline host's + Shell sends nothing"
    );
    assert_eq!(shell_spawns(h.sent()), Vec::<Option<String>>::new());
    assert!(
        drawn(&mut h, &format!("host-header-{REMOTE}")),
        "the offline host keeps its header"
    );
}

#[gpui::test]
fn a_not_approved_host_shows_pair(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &serde_json::json!({ "saved_hosts": [{ "name": REMOTE, "port": 8787 }] }),
    );
    let mut h = Harness::with(
        cx,
        &dir,
        &Fixture::single(session("a1").in_repo("r1").build()),
    );
    assert!(
        drawn(&mut h, &format!("host-shell-{REMOTE}"))
            && !drawn(&mut h, &format!("host-pair-{REMOTE}")),
        "a host still connecting ends its header with + Shell"
    );
    let mut refused = Connection::for_target(saved_remote());
    refused.on_connecting();
    refused.on_socket_open(8787);
    refused.on_not_approved(protocol::DEVICE_NOT_APPROVED.to_owned(), false);
    h.host(&saved_remote()).event(NetEvent::State(refused));

    assert!(
        drawn(&mut h, &format!("host-pair-{REMOTE}")),
        "a host that has not approved this PC ends its header with Pair"
    );
    h.click_on(&format!("host-pair-{REMOTE}"));

    assert!(
        h.root(|root, _| root.device_pairing_open()),
        "Pair opens the pairing dialog for the host"
    );
}

#[gpui::test]
fn a_container_dragged_over_another_hosts_rows_shows_no_drop_line_and_drops_nothing(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let mut h = saved_remote_listing(cx, &dir);
    h.host(&saved_remote()).sent();
    let before = shown_keys(&mut h);
    let remote_row = format!("container-{}", remote_repo_key());

    // Each drop would change the order across hosts: r1 after the remote
    // repo, the remote repo before r1.
    for (from, to, fy) in [
        ("container-repo:r1", remote_row.as_str(), 0.8),
        (remote_row.as_str(), "container-repo:r1", 0.2),
    ] {
        let at = drag_row_over(&mut h, from, to, fy);
        assert_eq!(
            h.root(RootView::sidebar_drop_mark),
            None,
            "{from} over {to}: no line on another host's row"
        );
        h.release(at);

        assert_eq!(
            container_orders(h.sent()),
            Vec::<Vec<ContainerRef>>::new(),
            "{from} on {to}: This PC is sent no order"
        );
        assert_eq!(
            container_orders(h.host(&saved_remote()).sent()),
            Vec::<Vec<ContainerRef>>::new(),
            "{from} on {to}: nor is the remote host"
        );
        assert_eq!(shown_keys(&mut h), before, "the rows keep their order");
    }
}

#[gpui::test]
fn a_drop_that_changes_no_order_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = saved_remote_listing(cx, &dir);
    h.send(DaemonMessage::Repos {
        repos: vec![repo("r1", "C:/r1"), repo("r3", "C:/r3")],
    });
    h.send(DaemonMessage::Sessions {
        sessions: vec![
            session("a1").in_repo("r1").build(),
            session("a3").in_repo("r3").build(),
        ],
    });
    h.sent();
    h.host(&saved_remote()).sent();
    assert_eq!(
        shown_keys(&mut h),
        [
            "repo:r1".to_owned(),
            "repo:r3".to_owned(),
            remote_repo_key()
        ]
    );

    let at = drag_row_over(&mut h, "container-repo:r3", "container-repo:r1", 0.2);
    h.release(at);

    assert_eq!(
        container_orders(h.sent()),
        [vec![
            ContainerRef::Repo("r3".to_owned()),
            ContainerRef::Repo("r1".to_owned()),
        ]],
        "This PC, whose order changed, is sent its new order"
    );
    assert_eq!(
        container_orders(h.host(&saved_remote()).sent()),
        Vec::<Vec<ContainerRef>>::new(),
        "the remote host's order did not change: it is sent nothing"
    );
    assert_eq!(
        shown_keys(&mut h),
        [
            "repo:r3".to_owned(),
            "repo:r1".to_owned(),
            remote_repo_key()
        ]
    );
}
