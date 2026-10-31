//! Mixed-host tab specs: a remote host's session placed into This PC's
//! layout names its host, the layout message goes to This PC and the
//! session-level messages to the session's own host.

#![expect(
    clippy::panic,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use std::sync::Arc;
use std::time::Duration;

use gpui::{Modifiers, TestAppContext};
use protocol::{
    ClientMessage, DaemonMessage, GridNode, SessionMode, SessionSnapshot, SpawnRequest,
    SpawnTarget, SplitDirection, WorktreeReusePolicy,
};
use rustling_tulip_native::connection_picker::PeerSource;
use rustling_tulip_native::target::Target;
use rustling_tulip_native::{Connection, NetEvent, OpenIn, OpenOutcome, PaneAim, PopoutKey};
use support::{
    Fixture, Harness, PROTOCOL, REMOTE, TestDir, mixed_tabs, pane, remote_pane, repo, session,
    split, tab,
};

fn remote() -> Target {
    Target::tailnet(REMOTE, None)
}

/// This PC lists `a1` and lays tab `t1` out as `grid`; [`REMOTE`] is
/// connected beside it and lists repo `r2` and `remote_sessions`. Nothing
/// sent is left unread on either host.
fn hosts_over<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    grid: &GridNode,
    remote_sessions: Vec<SessionSnapshot>,
) -> Harness<'a> {
    let fixture = Fixture {
        sessions: vec![session("a1").build()],
        tabs: vec![tab("t1", grid)],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, dir, &fixture);
    let mut host = h.host(&remote());
    host.connect();
    host.send(DaemonMessage::Repos {
        repos: vec![repo("r2", "C:/r2")],
    });
    host.send(DaemonMessage::Sessions {
        sessions: remote_sessions,
    });
    host.sent();
    h.sent();
    h
}

/// A claude spawn on branch `feature` of [`REMOTE`]'s repo `r2`.
fn remote_request() -> SpawnRequest {
    SpawnRequest {
        label: None,
        target: SpawnTarget::Single {
            repo_id: "r2".to_owned(),
            branch_name: "feature".to_owned(),
            base_branch: None,
            use_worktree: true,
            checkout_strategy: None,
            worktree_reuse: WorktreeReusePolicy::default(),
            existing_worktree: None,
        },
        mode: SessionMode::Interactive,
        initial_prompt: None,
        dangerously_skip_permissions: false,
        agent: Some(
            serde_json::from_value(serde_json::json!({ "agent": "claude", "options": {} }))
                .expect("agent selection"),
        ),
        extra_env: Vec::new(),
        secret_env_keys: Vec::new(),
        prompt_injector: None,
        request_id: None,
        resume_conversation: None,
    }
}

/// Spawns [`remote_request`] into `open_in` through the view's entry point;
/// returns the request id it was asked under.
fn spawn_remote(h: &mut Harness<'_>, open_in: OpenIn) -> String {
    h.root_update(move |root, _, cx| root.spawn(remote_request(), open_in, cx))
}

/// The request ids of the spawns among `sent`.
fn spawn_ids(sent: &[ClientMessage]) -> Vec<Option<String>> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::SpawnSession(request) => Some(request.request_id.clone()),
            _ => None,
        })
        .collect()
}

/// `session` sent by [`REMOTE`] as the answer to `request_id`.
fn remote_reply(h: &mut Harness<'_>, session: SessionSnapshot, request_id: &str) {
    h.host(&remote()).send(DaemonMessage::SessionUpdated {
        session,
        request_id: Some(request_id.to_owned()),
    });
}

/// A layout message that puts a session in a pane or a tab, as the session
/// it places and the host it names.
#[derive(Debug, PartialEq, Eq)]
struct Placed {
    session: Option<String>,
    host: Option<String>,
}

/// The session placements among `sent`: every `CreateTab`, `SplitPane`
/// and `ReplacePaneSession`.
fn placements(sent: &[ClientMessage]) -> Vec<Placed> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::CreateTab {
                initial_session_id: session,
                host,
                ..
            }
            | ClientMessage::SplitPane {
                new_session_id: session,
                host,
                ..
            }
            | ClientMessage::ReplacePaneSession {
                session_id: session,
                host,
                ..
            } => Some(Placed {
                session: session.clone(),
                host: host.clone(),
            }),
            _ => None,
        })
        .collect()
}

/// `session` placed with [`REMOTE`] as its host.
fn placed_on_remote(session: &str) -> Placed {
    Placed {
        session: Some(session.to_owned()),
        host: Some(REMOTE.to_owned()),
    }
}

/// The sessions `sent` discards.
fn discards(sent: &[ClientMessage]) -> Vec<String> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::DiscardSession { session_id, .. } => Some(session_id.clone()),
            _ => None,
        })
        .collect()
}

/// The toasts on screen, as their titles and details.
fn toasts(h: &mut Harness<'_>) -> Vec<(String, Option<String>)> {
    h.root(|root, _| {
        root.toasts()
            .iter()
            .map(|toast| (toast.title.clone(), toast.detail.clone()))
            .collect()
    })
}

/// Whether a spawning toast is on screen.
fn spawning_shown(h: &mut Harness<'_>) -> bool {
    toasts(h)
        .iter()
        .any(|(title, _)| title == "Spawning session…")
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

/// Reports [`REMOTE`]'s connection dropped and drains what it sent.
fn drop_remote(h: &mut Harness<'_>) {
    let mut host = h.host(&remote());
    host.event(NetEvent::State(dropped(remote())));
    host.sent();
}

#[gpui::test]
fn a_remote_spawn_into_the_current_tab_is_placed_with_its_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = hosts_over(cx, &dir, &pane("p1", Some("a1")), Vec::new());

    let id = spawn_remote(&mut h, OpenIn::CurrentTab("t1".to_owned()));
    assert_eq!(
        spawn_ids(&h.host(&remote()).sent()),
        [Some(id.clone())],
        "the spawn goes to the repo's host"
    );
    remote_reply(&mut h, session("b2").in_repo("r2").build(), &id);

    assert_eq!(
        placements(&h.sent()),
        [placed_on_remote("b2")],
        "This PC lays the remote session out, naming its host"
    );
    let remote_sent = h.host(&remote()).sent();
    assert_eq!(
        placements(&remote_sent),
        [],
        "no layout message goes to the remote host: {remote_sent:?}"
    );
}

#[gpui::test]
fn a_remote_spawn_aimed_at_an_empty_pane_fills_it_with_its_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let grid = split(
        SplitDirection::Horizontal,
        pane("p1", Some("a1")),
        pane("p2", None),
    );
    let mut h = hosts_over(cx, &dir, &grid, Vec::new());

    let aim = PaneAim {
        tab_id: "t1".to_owned(),
        pane_id: "p2".to_owned(),
        expected: None,
        discard: None,
    };
    let id = spawn_remote(&mut h, OpenIn::Pane(aim));
    assert_eq!(spawn_ids(&h.host(&remote()).sent()), [Some(id.clone())]);
    remote_reply(&mut h, session("b2").in_repo("r2").build(), &id);

    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::ReplacePaneSession { tab_id, pane_id, session_id: Some(new), host: Some(host) }]
            if tab_id == "t1" && pane_id == "p2" && new == "b2" && host == REMOTE),
        "the aimed pane takes the remote session with its host: sent {sent:?}"
    );
    assert_eq!(placements(&h.host(&remote()).sent()), []);
}

#[gpui::test]
fn a_remote_duplicate_into_a_new_tab_carries_its_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = mixed_tabs(cx, &dir);
    // The panes' scrollback loads on the remote host are not this spec's.
    h.host(&remote()).sent();

    h.right_click_on("leaf-b1");
    h.click_on("session-menu-duplicate");
    h.click_on("duplicate-new-tab");
    let remote_sent = h.host(&remote()).sent();
    let id = match remote_sent.as_slice() {
        [
            ClientMessage::DuplicateSession {
                session_id,
                request_id: Some(id),
                ..
            },
        ] if session_id == "b1" => id.clone(),
        other => {
            panic!("expected one DuplicateSession of b1 on the remote host, sent {other:?}")
        }
    };
    assert_eq!(
        placements(&h.sent()),
        [],
        "nothing is placed before the reply"
    );

    remote_reply(&mut h, session("b3").build(), &id);

    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::CreateTab { initial_session_id: Some(new), host: Some(host), .. }]
            if new == "b3" && host == REMOTE),
        "the copy opens a tab on This PC naming its host: sent {sent:?}"
    );
    assert_eq!(placements(&h.host(&remote()).sent()), []);
}

#[gpui::test]
fn restarting_a_remote_session_in_its_pane_discards_on_the_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let grid = split(
        SplitDirection::Horizontal,
        pane("p1", Some("a1")),
        remote_pane("p2", "b1"),
    );
    let mut h = hosts_over(
        cx,
        &dir,
        &grid,
        vec![session("b1").in_repo("r2").exited(0).build()],
    );

    h.right_click_on("leaf-b1");
    h.click_on("menu-restart");
    let remote_sent = h.host(&remote()).sent();
    let id = match remote_sent.as_slice() {
        [
            ClientMessage::DuplicateSession {
                session_id,
                request_id: Some(id),
                ..
            },
        ] if session_id == "b1" => id.clone(),
        other => panic!("expected one restart of b1 on the remote host, sent {other:?}"),
    };

    remote_reply(&mut h, session("b4").in_repo("r2").build(), &id);

    let local_sent = h.sent();
    assert!(
        matches!(local_sent.as_slice(), [ClientMessage::ReplacePaneSession { tab_id, pane_id, session_id: Some(new), host: Some(host) }]
            if tab_id == "t1" && pane_id == "p2" && new == "b4" && host == REMOTE),
        "the restart takes its original's pane, naming its host: sent {local_sent:?}"
    );
    assert_eq!(
        discards(&local_sent),
        Vec::<String>::new(),
        "This PC discards nothing"
    );
    let remote_sent = h.host(&remote()).sent();
    assert_eq!(
        discards(&remote_sent),
        ["b1"],
        "the original is discarded on its own host"
    );
    assert_eq!(placements(&remote_sent), []);
}

#[gpui::test]
fn clicking_a_remote_leaf_in_no_tab_places_it_with_its_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let grid = split(
        SplitDirection::Horizontal,
        pane("p1", Some("a1")),
        pane("p2", None),
    );
    let mut h = hosts_over(cx, &dir, &grid, vec![session("b1").in_repo("r2").build()]);

    h.click_on("leaf-b1");

    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::ReplacePaneSession { tab_id, pane_id, session_id: Some(id), host: Some(host) }]
            if tab_id == "t1" && pane_id == "p2" && id == "b1" && host == REMOTE),
        "the empty pane takes the remote session with its host: sent {sent:?}"
    );
    assert_eq!(placements(&h.host(&remote()).sent()), []);
}

#[gpui::test]
fn add_to_new_tab_for_a_remote_session_carries_its_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = hosts_over(
        cx,
        &dir,
        &pane("p1", Some("a1")),
        vec![session("b1").in_repo("r2").build()],
    );

    h.right_click_on("leaf-b1");
    h.click_on("session-menu-add-new");

    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::CreateTab { initial_session_id: Some(id), host: Some(host), .. }]
            if id == "b1" && host == REMOTE),
        "the new tab names the session's host: sent {sent:?}"
    );
    assert_eq!(placements(&h.host(&remote()).sent()), []);
}

#[gpui::test]
fn a_tab_of_remote_panes_counts_its_busy_sessions(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = mixed_tabs(cx, &dir);

    h.host(&remote()).send(DaemonMessage::Sessions {
        sessions: vec![
            session("b1").status("working").build(),
            session("b2").status("working").build(),
        ],
    });

    assert_eq!(
        h.root(|root, _| root.tab_badge("t2")),
        Some((1, 1)),
        "t2's one pane shows box's working b2"
    );
    assert_eq!(
        h.root(|root, _| root.tab_badge("t1")),
        Some((1, 2)),
        "t1 counts This PC's idle a1 and box's working b1"
    );
}

#[gpui::test]
fn a_remote_host_dropping_ends_its_pending_spawn(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = hosts_over(cx, &dir, &pane("p1", Some("a1")), Vec::new());
    let id = spawn_remote(&mut h, OpenIn::NewTab);
    assert_eq!(spawn_ids(&h.host(&remote()).sent()), [Some(id.clone())]);
    assert!(spawning_shown(&mut h), "the spawn shows its toast");

    drop_remote(&mut h);

    assert!(
        !spawning_shown(&mut h),
        "the dropped host's spawn ends with its toast: {:?}",
        toasts(&mut h)
    );
    h.host(&remote()).connect();
    remote_reply(&mut h, session("b2").in_repo("r2").build(), &id);
    assert_eq!(
        placements(&h.sent()),
        [],
        "a late reply to the ended spawn is placed nowhere"
    );
}

#[gpui::test]
fn a_spawn_asked_of_an_offline_host_is_not_sent_and_says_so(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = hosts_over(cx, &dir, &pane("p1", Some("a1")), Vec::new());
    drop_remote(&mut h);

    spawn_remote(&mut h, OpenIn::NewTab);

    let remote_sent = h.host(&remote()).sent();
    assert_eq!(
        spawn_ids(&remote_sent),
        Vec::<Option<String>>::new(),
        "nothing goes to the offline host: {remote_sent:?}"
    );
    assert!(
        toasts(&mut h).contains(&(
            "Couldn't start the session".to_owned(),
            Some(format!("{REMOTE} is offline")),
        )),
        "the spawn fails with a toast: {:?}",
        toasts(&mut h)
    );
    assert!(!spawning_shown(&mut h), "no spawning toast stays");
}

/// [`REMOTE`] as launch dials it from the saved hosts.
fn saved_remote() -> Target {
    Target::tailnet(REMOTE, Some(8787))
}

/// A Tailscale that sees no peers: Connections lists only the saved hosts.
struct NoPeers;

impl PeerSource for NoPeers {
    fn status(&self) -> Result<tailnet::Status, tailnet::TailnetError> {
        Ok(tailnet::Status::default())
    }
}

/// Flips [`REMOTE`]'s Connected switch in Connections, then closes it.
fn flip_remote_switch(h: &mut Harness<'_>) {
    h.root_update(|root, _, _| root.set_peer_source(Arc::new(NoPeers)));
    h.click_on("footer-connections");
    h.click_on(&format!("connections-row-{REMOTE}-enabled"));
    h.keys("escape");
}

/// The sessions `sent` asks to duplicate.
fn duplicated(sent: &[ClientMessage]) -> Vec<String> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::DuplicateSession { session_id, .. } => Some(session_id.clone()),
            _ => None,
        })
        .collect()
}

/// Lists repo `r2` and the exited `b1` on [`REMOTE`]'s newest connection,
/// connecting it first; drains what it sent.
fn list_remote(h: &mut Harness<'_>) {
    let mut host = h.host(&saved_remote());
    host.connect();
    host.send(DaemonMessage::Repos {
        repos: vec![repo("r2", "C:/r2")],
    });
    host.send(DaemonMessage::Sessions {
        sessions: vec![session("b1").in_repo("r2").exited(0).build()],
    });
    host.sent();
}

#[gpui::test]
fn switching_a_remote_host_off_ends_its_pending_spawn(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let ui = serde_json::json!({ "saved_hosts": [{ "name": REMOTE, "port": 8787 }] });
    std::fs::write(dir.path().join("native-ui.json"), ui.to_string())
        .expect("write native-ui.json");
    let grid = split(
        SplitDirection::Horizontal,
        pane("p1", Some("a1")),
        remote_pane("p2", "b1"),
    );
    let fixture = Fixture {
        sessions: vec![session("a1").build()],
        tabs: vec![tab("t1", &grid)],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    list_remote(&mut h);
    h.sent();
    let id = spawn_remote(&mut h, OpenIn::NewTab);
    h.right_click_on("leaf-b1");
    h.click_on("menu-restart");
    let remote_sent = h.host(&saved_remote()).sent();
    assert_eq!(spawn_ids(&remote_sent), [Some(id)]);
    assert_eq!(duplicated(&remote_sent), ["b1"], "b1's restart is pending");
    assert!(spawning_shown(&mut h), "the spawn shows its toast");

    flip_remote_switch(&mut h);

    assert!(
        !spawning_shown(&mut h),
        "the switched-off host's spawn ends with its toast: {:?}",
        toasts(&mut h)
    );
    flip_remote_switch(&mut h);
    list_remote(&mut h);
    h.right_click_on("leaf-b1");
    h.click_on("menu-restart");
    assert_eq!(
        duplicated(&h.host(&saved_remote()).sent()),
        ["b1"],
        "the ended restart no longer holds b1: a new one is sent"
    );
}

/// Clicks cell (0, 0) of `pane`, giving it the keyboard.
fn click_pane(h: &mut Harness<'_>, pane: &str) {
    let at = h.cell_center(pane, 0, 0);
    h.click(at, Modifiers::none());
}

/// Whether `sent` resizes `session`.
fn resizes(sent: &[ClientMessage], session: &str) -> bool {
    sent.iter()
        .any(|msg| matches!(msg, ClientMessage::Resize { session_id, .. } if session_id == session))
}

/// The input bytes `sent` carries to `session`, in order.
fn input_in(sent: &[ClientMessage], session: &str) -> Vec<u8> {
    use base64::Engine as _;
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::SendInput {
                session_id,
                data_b64,
                ..
            } if session_id == session => Some(
                base64::engine::general_purpose::STANDARD
                    .decode(data_b64)
                    .expect("input base64"),
            ),
            _ => None,
        })
        .flatten()
        .collect()
}

#[gpui::test]
fn typing_in_a_remote_pane_laid_out_before_its_hosts_list_goes_to_that_host(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let mut h = mixed_tabs(cx, &dir);
    h.host(&remote()).answer_scrollback("b1", b"");
    click_pane(&mut h, "p2");
    h.sent();
    h.host(&remote()).sent();

    h.keys("x");

    assert_eq!(h.host(&remote()).sent_input("b1"), b"x");
    assert!(
        h.sent_input("b1").is_empty(),
        "nothing typed in p2 goes to This PC"
    );
}

#[gpui::test]
fn typing_in_an_empty_pane_filled_by_a_remote_session_goes_to_that_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = hosts_over(cx, &dir, &pane("p1", None), Vec::new());

    h.send(DaemonMessage::TabUpdated {
        tab: tab("t1", &remote_pane("p1", "b9")),
    });
    let mut host = h.host(&remote());
    host.send(DaemonMessage::Sessions {
        sessions: vec![session("b9").build()],
    });
    host.answer_scrollback("b9", b"");
    click_pane(&mut h, "p1");
    h.sent();
    h.host(&remote()).sent();

    h.keys("x");

    assert_eq!(h.host(&remote()).sent_input("b9"), b"x");
    assert!(h.sent_input("b9").is_empty(), "nothing goes to This PC");
}

#[gpui::test]
fn a_pane_rebound_from_a_local_to_a_remote_session_switches_its_sender(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = mixed_tabs(cx, &dir);
    h.answer_scrollback("a1", b"");
    h.host(&remote()).answer_scrollback("b1", b"");
    h.sent();
    h.host(&remote()).sent();

    // p1 now shows b1 of the remote host, p2 a1 of This PC.
    h.send(DaemonMessage::TabUpdated {
        tab: tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                remote_pane("p1", "b1"),
                pane("p2", Some("a1")),
            ),
        ),
    });
    h.host(&remote()).answer_scrollback("b1", b"");
    h.answer_scrollback("a1", b"");
    click_pane(&mut h, "p1");
    h.advance(Duration::from_secs(6));
    h.keys("x");

    let remote_sent = h.host(&remote()).sent();
    assert_eq!(input_in(&remote_sent, "b1"), b"x", "{remote_sent:?}");
    assert!(
        resizes(&remote_sent, "b1"),
        "p1 sizes b1 on its host: {remote_sent:?}"
    );
    let local_sent = h.sent();
    assert!(
        input_in(&local_sent, "b1").is_empty() && !resizes(&local_sent, "b1"),
        "nothing for b1 goes to This PC: {local_sent:?}"
    );
}

#[gpui::test]
fn output_reaches_only_the_panes_of_the_host_that_sent_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let grid = split(
        SplitDirection::Horizontal,
        pane("p1", Some("s1")),
        remote_pane("p2", "s1"),
    );
    let fixture = Fixture {
        sessions: vec![session("s1").build()],
        tabs: vec![tab("t1", &grid)],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    let mut host = h.host(&remote());
    host.connect();
    host.send(DaemonMessage::Sessions {
        sessions: vec![session("s1").build()],
    });
    h.answer_scrollback("s1", b"");

    h.host(&remote()).pty("s1", b"B!");
    assert_eq!(
        h.grid_text("p2")[0],
        "B!",
        "the remote host's pane shows it"
    );
    assert_eq!(h.grid_text("p1")[0], "", "This PC's pane does not");

    h.pty("s1", b"A!");
    assert_eq!(h.grid_text("p1")[0], "A!", "This PC's pane shows its own");
    assert_eq!(h.grid_text("p2")[0], "B!", "the remote pane does not");
}

#[gpui::test]
fn a_remote_session_popout_answers_its_cursor_query_and_sizes_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = mixed_tabs(cx, &dir);
    let mut host = h.host(&remote());
    host.send(DaemonMessage::Sessions {
        sessions: vec![
            session("b1").build(),
            session("b2").build(),
            session("b5").build(),
        ],
    });
    host.sent();
    let key = PopoutKey::Session {
        session_id: "b5".to_owned(),
        host: Some(REMOTE.to_owned()),
    };
    let outcome = h.root_update(move |root, window, cx| root.open_popout(key, window, cx));
    assert_eq!(outcome, OpenOutcome::Opened);
    h.host(&remote()).answer_scrollback("b5", b"");

    h.host(&remote()).pty("b5", b"\x1b[6n");

    let remote_sent = h.host(&remote()).sent();
    let reply = input_in(&remote_sent, "b5");
    assert!(
        reply.starts_with(b"\x1b[") && reply.ends_with(b"R"),
        "the pop-out answers b5's cursor query on its host: {remote_sent:?}"
    );
    assert!(
        resizes(&remote_sent, "b5"),
        "the pop-out sizes b5 on its host: {remote_sent:?}"
    );
    assert!(
        input_in(&h.sent(), "b5").is_empty(),
        "nothing for b5 goes to This PC"
    );
}
