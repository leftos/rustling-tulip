//! End-to-end specs for several hosts in one window: This PC on one real
//! daemon and a remote host on a second, reached through `tools/e2e/fake-ssh`
//! and the real `rustling-tulipd bridge`, each daemon isolated under `.tmp/`.
//! They need that daemon build beside the test binary, so they are ignored
//! by default and run by `.\rt.ps1 native-e2e`.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

#[expect(dead_code, reason = "each spec file uses its own share of the helper")]
mod support;

use std::sync::Arc;

use daemon_client::device_tokens::MemoryTokens;
use gpui::TestAppContext;
use protocol::{DaemonMessage, GridNode};
use rustling_tulip_native::connection_picker::{PeerSource, RowState};
use rustling_tulip_native::target::{HostKey, Target};
use rustling_tulip_native::{Connection, HeaderChipKind};
use support::Harness;
use support::live::{FakeSsh, LiveClient, LiveDaemon, LiveHostSpec, LiveHosts, kill_tree};

/// The saved ssh host the remote daemon is reached by; the fake ssh ignores
/// the host string.
const HOST: &str = "devbox";
/// A second ssh alias to the same daemon, for the dedupe spec.
const ALIAS: &str = "devbox2";
/// The stderr the fake ssh refuses logins with while a spec holds a host
/// down.
const DENIED: &str = "me@devbox: Permission denied (publickey).";

/// The argv the client runs `ssh` with for `host` and the default remote
/// command (`apps/native/src/ssh.rs`), as `e2e_ssh.rs` pins it for its host.
fn expected_argv(host: &str) -> Vec<String> {
    [
        "-T",
        "-o",
        "BatchMode=yes",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=3",
        "-o",
        "ConnectTimeout=10",
        "-o",
        "Compression=no",
        "--",
        host,
        "rustling-tulipd bridge",
    ]
    .map(str::to_owned)
    .to_vec()
}

/// `+ Shell` on the remote host's header spawns the shell on the remote
/// daemon; its pane sits in This PC's layout naming the host, carries the
/// host chip and echoes typed input.
#[gpui::test]
#[ignore = "e2e: run via .\\rt.ps1 native-e2e"]
fn plus_shell_on_a_remote_host_runs_there_and_echoes(cx: &mut TestAppContext) {
    let home = LiveDaemon::start("mh-shell-local");
    let box_ = LiveDaemon::start("mh-shell-remote");
    let fake = use_fake_ssh(&box_);
    let (mut h, hosts) = open_window(cx, &home, &box_, &[HOST], &[]);
    let (local, remote) = (hosts.client(&HostKey::Local), hosts.client(&key(HOST)));
    wait_open(&mut h, &local, "This PC to connect");
    wait_open(&mut h, &remote, "the ssh host to connect");

    let pane = h.place_shell_on(&key(HOST));
    echo(&mut h, &pane, "rt-mh-remote");
    let session = h
        .root(|root, _| root.pane_session(&pane))
        .expect("the placed pane holds the shell");

    assert!(
        lists_session(&remote, &session),
        "the remote daemon lists the shell"
    );
    assert!(
        !lists_session(&local, &session),
        "This PC's daemon does not list the remote shell"
    );
    let chips = h
        .root(|root, _| root.pane_header_parts(&pane))
        .expect("the pane has a header")
        .chips;
    assert!(
        chips
            .iter()
            .any(|chip| chip.kind == HeaderChipKind::Host && chip.text.contains(HOST)),
        "the pane header names the host: {chips:?}"
    );
    assert!(
        layout_names(&local, &session, Some(&wire(HOST))),
        "This PC's layout holds the pane with its host"
    );
    assert_no_tabs(&remote);
    assert_runs(&fake, &[HOST]);
}

/// A tab holding a This PC shell and a remote shell: one tab in This PC's
/// layout, each pane on its own daemon, and what is typed into one pane
/// shows in that pane only.
#[gpui::test]
#[ignore = "e2e: run via .\\rt.ps1 native-e2e"]
fn a_mixed_tab_runs_each_pane_on_its_own_host(cx: &mut TestAppContext) {
    let home = LiveDaemon::start("mh-mixed-local");
    let box_ = LiveDaemon::start("mh-mixed-remote");
    let fake = use_fake_ssh(&box_);
    let (mut h, hosts) = open_window(cx, &home, &box_, &[HOST], &[]);
    let (local, remote) = (hosts.client(&HostKey::Local), hosts.client(&key(HOST)));
    wait_open(&mut h, &local, "This PC to connect");
    wait_open(&mut h, &remote, "the ssh host to connect");

    let (local_pane, remote_pane) = open_mixed_tab(&mut h);
    let local_session = h
        .root(|root, _| root.pane_session(&local_pane))
        .expect("the This PC pane holds its shell");
    let remote_session = h
        .root(|root, _| root.pane_session(&remote_pane))
        .expect("the remote pane holds its shell");
    assert_ne!(local_session, remote_session);
    assert!(lists_session(&local, &local_session));
    assert!(lists_session(&remote, &remote_session));

    echo(&mut h, &local_pane, "rt-mh-local");
    echo(&mut h, &remote_pane, "rt-mh-remote");
    let local_rows = rows(&mut h, &local_pane);
    let remote_rows = rows(&mut h, &remote_pane);
    assert!(
        !remote_rows.iter().any(|row| row.contains("rt-mh-local")),
        "This PC's input stays out of the remote pane: {remote_rows:#?}"
    );
    assert!(
        !local_rows.iter().any(|row| row.contains("rt-mh-remote")),
        "the remote input stays out of This PC's pane: {local_rows:#?}"
    );

    let tabs = h.root(|root, _| root.tab_ids());
    assert_eq!(tabs.len(), 1, "one tab holds both panes: {tabs:?}");
    assert_eq!(
        h.root(|root, _| root.tab_host_chip(&tabs[0])),
        None,
        "a mixed tab's pill names no host"
    );
    assert!(layout_names(&local, &local_session, None));
    assert!(layout_names(&local, &remote_session, Some(&wire(HOST))));
    assert_no_tabs(&remote);
    assert_runs(&fake, &[HOST]);
}

/// Killing the remote host's ssh dims that host alone: This PC's pane in
/// the same tab keeps echoing and the remote pane keeps its last frame.
/// Once ssh logs in again the host reconnects and its pane echoes again.
#[gpui::test]
#[ignore = "e2e: run via .\\rt.ps1 native-e2e"]
fn a_killed_ssh_dims_only_its_host_and_reconnects(cx: &mut TestAppContext) {
    let home = LiveDaemon::start("mh-kill-local");
    let box_ = LiveDaemon::start("mh-kill-remote");
    let fake = use_fake_ssh(&box_);
    let (mut h, hosts) = open_window(cx, &home, &box_, &[HOST], &[]);
    let (local, remote) = (hosts.client(&HostKey::Local), hosts.client(&key(HOST)));
    wait_open(&mut h, &local, "This PC to connect");
    wait_open(&mut h, &remote, "the ssh host to connect");
    let (local_pane, remote_pane) = open_mixed_tab(&mut h);
    echo(&mut h, &remote_pane, "rt-mh-before");

    let first = fake.runs();
    assert_eq!(first.len(), 1, "one ssh run so far: {first:?}");
    let local_from = local.states().len();
    let states_from = remote.states().len();
    let from = remote.message_count();
    let frame = rows(&mut h, &remote_pane);
    // Refused logins hold the host down until the spec lets it back in, so
    // the down state is observed whatever the reconnect's pace.
    fake.set_fail(Some((255, DENIED)));
    kill_tree(first[0].pid);

    h.wait_until("the ssh host to dim", |h| {
        h.root(|root, _| root.host_dimmed(&key(HOST)))
    });
    assert!(!h.root(|root, _| root.host_dimmed(&HostKey::Local)));
    echo(&mut h, &local_pane, "rt-mh-local-while-down");
    assert_eq!(
        rows(&mut h, &remote_pane),
        frame,
        "the remote pane keeps its last frame while its host is down"
    );

    fake.set_fail(None);
    h.wait_until("the ssh host to reconnect through a new ssh", |_| {
        let states = remote.states();
        let after = &states[states_from..];
        after.iter().any(|conn| !conn.is_open()) && after.last().is_some_and(Connection::is_open)
    });
    h.wait_until("the session list on the new connection", |_| {
        remote
            .find_message_from(from, |msg| match msg {
                DaemonMessage::Sessions { sessions } => (!sessions.is_empty()).then_some(()),
                _ => None,
            })
            .is_some()
    });
    h.wait_until("the ssh host to undim", |h| {
        !h.root(|root, _| root.host_dimmed(&key(HOST)))
    });
    echo(&mut h, &remote_pane, "rt-mh-after");

    let local_states = local.states();
    assert!(
        local_states[local_from..].iter().all(Connection::is_open),
        "This PC's connection never closed: {:?}",
        local.footer_labels()
    );
    let runs = fake.runs();
    let last = runs.last().expect("the reconnect ran ssh");
    assert_ne!(last.pid, first[0].pid, "the reconnect ran ssh anew");
    for run in &runs {
        assert_eq!(run.argv, expected_argv(HOST), "every ssh run's argv");
    }
}

/// Two saved aliases of one daemon: with the later alias holding a pane,
/// switching the earlier alias on gives it the daemon by saved order, drops
/// the later one as its duplicate and moves the pane to it, where the
/// session keeps echoing.
#[gpui::test]
#[ignore = "e2e: run via .\\rt.ps1 native-e2e"]
fn switching_on_the_earlier_alias_takes_the_daemon_and_its_panes(cx: &mut TestAppContext) {
    let home = LiveDaemon::start("mh-dedupe-local");
    let box_ = LiveDaemon::start("mh-dedupe-remote");
    let fake = use_fake_ssh(&box_);
    let (mut h, hosts) = open_window(cx, &home, &box_, &[HOST, ALIAS], &[key(HOST)]);
    let local = hosts.client(&HostKey::Local);
    wait_open(&mut h, &local, "This PC to connect");
    wait_open(&mut h, &hosts.client(&key(ALIAS)), "the alias to connect");

    let pane = h.place_shell_on(&key(ALIAS));
    echo(&mut h, &pane, "rt-mh-alias");
    let session = h
        .root(|root, _| root.pane_session(&pane))
        .expect("the placed pane holds the shell");
    assert!(layout_names(&local, &session, Some(&wire(ALIAS))));

    h.root_update(|root, _, _| root.set_peer_source(Arc::new(NoPeers)));
    h.click_on("footer-connections");
    h.wait_until("Connections to open", |h| {
        h.root(|root, _| root.connection_picker_open())
    });
    h.click_on(&format!("connections-row-{}-enabled", wire(HOST)));
    h.wait_until("the earlier alias to take the daemon", |h| {
        h.root(|root, _| root.duplicate_of(&key(ALIAS)).cloned()) == Some(key(HOST))
    });
    h.wait_until(
        "This PC's layout to move the pane to the earlier alias",
        |_| {
            local
                .find_message(|msg| match msg {
                    DaemonMessage::TabUpdated { tab } => tab
                        .grid()
                        .is_some_and(|grid| {
                            grid_panes(grid).iter().any(|(id, host)| {
                                id.as_deref() == Some(session.as_str())
                                    && host.as_deref() == Some(wire(HOST).as_str())
                            })
                        })
                        .then_some(()),
                    _ => None,
                })
                .is_some()
        },
    );

    assert_eq!(
        h.root(|root, _| root.held_hosts()),
        [HostKey::Local, key(HOST)]
    );
    let detail = h
        .root(|root, _| root.connection_picker_row_detail(&wire(ALIAS)))
        .expect("Connections lists the alias");
    assert_eq!(
        detail.state,
        Some(RowState::SameDaemonAs {
            owner: HOST.to_owned()
        }),
        "the alias's row names its owner: {detail:?}"
    );
    assert_eq!(
        detail.state.map(|state| state.label()).as_deref(),
        Some("Same daemon as devbox")
    );
    h.keys("escape");

    let moved = h.first_pane().expect("the tab still holds the pane");
    assert_eq!(
        h.root(|root, _| root.pane_session(&moved)).as_deref(),
        Some(session.as_str()),
        "the moved pane still shows the session"
    );
    echo(&mut h, &moved, "rt-mh-owner");
    assert_runs(&fake, &[ALIAS, HOST]);
}

/// A Tailscale that sees no peers: Connections lists only the saved hosts
/// and never runs this PC's real `tailscale`.
struct NoPeers;

impl PeerSource for NoPeers {
    fn status(&self) -> Result<tailnet::Status, tailnet::TailnetError> {
        Ok(tailnet::Status::default())
    }
}

/// Sets up the fake ssh for `daemon` and points this process's client at it.
fn use_fake_ssh(daemon: &LiveDaemon) -> FakeSsh {
    let fake = daemon.fake_ssh(None);
    fake.use_for_this_process();
    fake
}

/// The ssh target of `host`.
fn target(host: &str) -> Target {
    Target::parse_ssh(host, None).expect("a valid ssh host")
}

/// The host key of `host`'s ssh target.
fn key(host: &str) -> HostKey {
    HostKey::of(&target(host))
}

/// `host`'s key as the wire and the selectors spell it, `ssh:<host>`.
fn wire(host: &str) -> String {
    key(host)
        .wire()
        .expect("a remote host has a wire key")
        .to_owned()
}

/// A window whose This PC is `home` and whose saved hosts are `saved`, in
/// that order, each reaching `remote` through the fake ssh; those in `off`
/// are saved switched off.
fn open_window<'a>(
    cx: &'a mut TestAppContext,
    home: &LiveDaemon,
    remote: &LiveDaemon,
    saved: &[&str],
    off: &[HostKey],
) -> (Harness<'a>, LiveHosts) {
    let specs = saved
        .iter()
        .map(|host| LiveHostSpec {
            target: target(host),
            config_dir: remote.config_dir(),
            tls: None,
            connects: true,
        })
        .collect();
    Harness::open_hosts_switched(cx, home, specs, off, Arc::new(MemoryTokens::default()))
}

/// Waits until `client`'s latest state is open.
fn wait_open(h: &mut Harness<'_>, client: &LiveClient, what: &str) {
    h.wait_until(what, |_| {
        client.states().last().is_some_and(Connection::is_open)
    });
}

/// `+ Shell` on This PC's header, then on [`HOST`]'s: the second shell
/// splits the tab the first opened. Returns This PC's pane and the remote
/// pane.
fn open_mixed_tab(h: &mut Harness<'_>) -> (String, String) {
    let local_pane = h.place_shell_on(&HostKey::Local);
    let button = format!("host-shell-{}", wire(HOST));
    h.click_on(&button);
    let mut placed = None;
    h.wait_until("a second pane holding the remote shell", |h| {
        let panes = h.root(|root, _| root.active_pane_ids());
        placed = panes.into_iter().find(|pane| {
            *pane != local_pane
                && h.root(|root, _| {
                    let Some(session) = root.pane_session(pane) else {
                        return false;
                    };
                    root.sidebar_containers().iter().any(|container| {
                        container.host == key(HOST)
                            && container.leaves.iter().any(|leaf| leaf.id == session)
                    })
                })
        });
        placed.is_some()
    });
    (local_pane, placed.expect("the wait just found it"))
}

/// Waits for `pane`'s shell prompt, types `echo <marker>` into it and waits
/// for the marker on a line of its own.
fn echo(h: &mut Harness<'_>, pane: &str, marker: &str) {
    h.wait_for_row(pane, "the shell's prompt", |row| {
        row.trim_end().ends_with('>')
    });
    h.type_into(pane, &format!("echo {marker}\n"));
    h.wait_for_row(pane, "the echoed marker on a line of its own", |row| {
        row.trim() == marker
    });
}

/// The rows `pane` shows.
fn rows(h: &mut Harness<'_>, pane: &str) -> Vec<String> {
    h.root(|root, cx| root.pane_grid_text(pane, cx))
        .expect("the pane is on screen")
}

/// Whether a session list `client`'s daemon sent names `session`.
fn lists_session(client: &LiveClient, session: &str) -> bool {
    client
        .find_message(|msg| match msg {
            DaemonMessage::Sessions { sessions } => sessions
                .iter()
                .any(|listed| listed.id == session)
                .then_some(()),
            DaemonMessage::SessionUpdated {
                session: listed, ..
            } => (listed.id == session).then_some(()),
            _ => None,
        })
        .is_some()
}

/// Whether a tab layout `client`'s daemon sent holds a pane showing
/// `session` with `host` (`None` for that daemon's own session).
fn layout_names(client: &LiveClient, session: &str, host: Option<&str>) -> bool {
    let names = |grid: &GridNode| {
        grid_panes(grid)
            .iter()
            .any(|(id, on)| id.as_deref() == Some(session) && on.as_deref() == host)
    };
    client
        .find_message(|msg| match msg {
            DaemonMessage::Tabs { tabs } => tabs
                .iter()
                .filter_map(protocol::TabEntry::grid)
                .any(names)
                .then_some(()),
            DaemonMessage::TabUpdated { tab } => tab.grid().is_some_and(names).then_some(()),
            _ => None,
        })
        .is_some()
}

/// Each pane of `grid` as its session id and host.
fn grid_panes(grid: &GridNode) -> Vec<(Option<String>, Option<String>)> {
    match grid {
        GridNode::Pane {
            session_id, host, ..
        } => vec![(session_id.clone(), host.clone())],
        GridNode::Split { first, second, .. } => {
            let mut panes = grid_panes(first);
            panes.extend(grid_panes(second));
            panes
        }
    }
}

/// The remote daemon keeps no tab for this client: its layout is the empty
/// one the window started it with, and it never sent a tab change.
fn assert_no_tabs(remote: &LiveClient) {
    let latest = remote.find_message(|msg| match msg {
        DaemonMessage::Tabs { tabs } => Some(tabs.len()),
        _ => None,
    });
    assert!(
        latest.is_none_or(|count| count == 0),
        "the remote daemon holds {latest:?} tabs for this client"
    );
    assert!(
        remote
            .find_message(|msg| matches!(msg, DaemonMessage::TabUpdated { .. }).then_some(()))
            .is_none(),
        "the remote daemon sent a tab change"
    );
}

/// The fake ssh ran once per host of `hosts`, in that order, each with the
/// documented argv.
fn assert_runs(fake: &FakeSsh, hosts: &[&str]) {
    let argvs: Vec<Vec<String>> = fake.runs().into_iter().map(|run| run.argv).collect();
    let expected: Vec<Vec<String>> = hosts.iter().map(|host| expected_argv(host)).collect();
    assert_eq!(
        argvs, expected,
        "one ssh run per host with the documented argv"
    );
}
