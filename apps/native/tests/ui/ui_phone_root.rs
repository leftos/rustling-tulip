//! The phone root: a `RootView` in the phone shape, with no This PC, drawn
//! by the phone's own root: the first-run form, the saved hosts' rows and
//! the Pair dialog.

use crate::support;

use daemon_client::device_tokens::DeviceTokenStore as _;
use gpui::{TestAppContext, point, px};
use protocol::{
    ClientMessage, DEVICE_NOT_APPROVED, DaemonMessage, DevicePairOutcome, FEATURE_CLIENT_PRESENCE,
    FEATURE_RELAY_PUSH,
};
use rustling_tulip_native::connection_picker::TypedHostError;
use rustling_tulip_native::phone::PhoneRoot;
use rustling_tulip_native::target::{HostKey, SshHost, Target};
use rustling_tulip_native::{Connection, NetCommand, NetEvent, RootView};
use serde_json::json;
use support::{Harness, REMOTE, TestDir, saved_remote, saved_ui, seed_ui, session};

/// A tailnet host the specs add by typing it.
const TYPED: &str = "den.tail1234.ts.net";

/// Whether the last frame drew what `selector` tags.
fn drawn(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).size.width > px(0.0)
}

/// Types `text` key by key into the focused field.
fn type_text(h: &mut Harness<'_>, text: &str) {
    let keys: Vec<String> = text.chars().map(|c| c.to_string()).collect();
    h.keys(&keys.join(" "));
}

/// Types `text` into the focused field exactly as written, spaces and
/// symbols included.
fn type_input(h: &mut Harness<'_>, text: &str) {
    h.cx.simulate_input(text);
    h.cx.run_until_parked();
}

/// The refusal the phone's add form shows, if any.
fn host_error(h: &mut Harness<'_>) -> Option<String> {
    h.cx.update(|window, cx| {
        window
            .root::<PhoneRoot>()
            .flatten()
            .and_then(|phone| phone.read(cx).host_error().map(str::to_owned))
    })
}

/// A connection to the saved [`REMOTE`] that its daemon refused: it does
/// not know this phone, which holds no token for it.
fn not_approved_without_token() -> Connection {
    let mut conn = Connection::for_target(saved_remote());
    conn.on_connecting();
    conn.on_socket_open(8787);
    conn.on_not_approved(DEVICE_NOT_APPROVED.to_owned(), false);
    conn
}

#[gpui::test]
fn no_saved_host_shows_the_first_run_screen(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::phone(cx, &dir);

    for selector in ["phone-host-name", "phone-host-port", "phone-host-add"] {
        assert!(drawn(&mut h, selector), "{selector} is drawn");
    }
    assert!(!drawn(&mut h, "phone-host-error"), "no refusal shows yet");
    assert!(
        h.opened_targets().is_empty(),
        "a phone with no saved host opens nothing"
    );
}

#[gpui::test]
fn adding_a_typed_host_saves_and_connects_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::phone(cx, &dir);

    h.click_on("phone-host-name");
    type_text(&mut h, TYPED);
    h.click_on("phone-host-port");
    h.keys("end backspace backspace backspace backspace");
    type_text(&mut h, "8787");
    h.click_on("phone-host-add");

    let typed = Target::tailnet(TYPED, Some(8787));
    assert_eq!(h.opened_targets(), vec![typed.clone()], "Add connects it");
    let saved = saved_ui(&dir)["saved_hosts"].clone();
    assert!(
        saved
            .as_array()
            .is_some_and(|hosts| hosts.iter().any(|host| host["name"] == TYPED)),
        "Add saves it: {saved}"
    );
    assert!(drawn(&mut h, &format!("phone-host-row-{TYPED}")));

    h.click_on("phone-host-add-another");
    h.click_on("phone-host-name");
    type_text(&mut h, "lab.tail1234.ts.net");
    h.click_on("phone-host-port");
    h.keys("end backspace backspace backspace backspace");
    type_text(&mut h, "abc");
    h.click_on("phone-host-add");

    assert!(drawn(&mut h, "phone-host-error"));
    assert_eq!(
        host_error(&mut h),
        Some(TypedHostError::Port.to_string()),
        "the form shows the typed host's refusal"
    );
    assert_eq!(
        h.opened_targets(),
        vec![typed],
        "a refused host opens nothing"
    );
}

#[gpui::test]
fn a_paired_open_host_offers_no_pair_and_a_refused_token_offers_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({ "saved_hosts": [{ "name": REMOTE, "port": 8787 }] }),
    );
    let mut h = Harness::phone(cx, &dir);
    h.tokens().set(REMOTE, "t1").expect("seed a token");
    h.host(&saved_remote()).connect();

    let pair = format!("phone-host-pair-{REMOTE}");
    assert!(drawn(&mut h, "phone-home"), "an open host shows Home");
    assert!(!drawn(&mut h, &pair), "a paired, open host offers no Pair");

    let mut revoked = Connection::for_target(saved_remote());
    revoked.on_connecting();
    revoked.on_socket_open(8787);
    revoked.on_not_approved(DEVICE_NOT_APPROVED.to_owned(), true);
    h.host(&saved_remote()).event(NetEvent::State(revoked));

    assert!(
        drawn(&mut h, &pair),
        "a host that refused the phone's token offers Pair"
    );
}

#[gpui::test]
fn a_saved_host_with_a_token_lists_its_sessions_at_window_width(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({ "saved_hosts": [{ "name": REMOTE, "port": 8787 }] }),
    );
    let mut h = Harness::phone(cx, &dir);
    h.tokens().set(REMOTE, "t1").expect("seed a token");
    {
        let mut host = h.host(&saved_remote());
        host.connect();
        host.send(DaemonMessage::Sessions {
            sessions: vec![session("s1").build()],
        });
    }
    let wire = HostKey::of(&saved_remote())
        .wire()
        .expect("a tailnet host has a wire key")
        .to_owned();
    let row_id = format!("phone-home-row-{wire}::s1");
    let row = row_id.as_str();

    assert!(drawn(&mut h, "phone-home"), "Home shows");
    let window_width = h.cx.update(|window, _| window.viewport_size().width);
    assert_eq!(
        h.bounds("phone-home").size.width,
        window_width,
        "the list spans the window"
    );
    assert!(drawn(&mut h, row), "the host's session is listed");
    assert!(
        !drawn(&mut h, &format!("phone-home-host-{wire}")),
        "one open host gets no header"
    );
    for chrome in [
        "sidebar-panel",
        "sidebar-session-hint",
        "sidebar-add-shell",
        "sidebar-more",
    ] {
        assert!(!drawn(&mut h, chrome), "no {chrome} on the phone");
    }
    assert!(
        !drawn(&mut h, &format!("phone-host-row-{REMOTE}")),
        "Home replaces the host's row"
    );
    assert!(drawn(&mut h, "phone-host-add-another"));

    h.host(&saved_remote()).sent();
    h.click_on(row);
    assert!(drawn(&mut h, "phone-session"), "a tap shows the session");
    assert!(
        h.host(&saved_remote()).sent().is_empty(),
        "a tap sends nothing"
    );
    assert!(
        h.host(&saved_remote()).commands().is_empty(),
        "a tap commands nothing"
    );
}

#[gpui::test]
fn the_phone_list_draws_no_detached_stop_all(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({ "saved_hosts": [{ "name": REMOTE, "port": 8787 }] }),
    );
    let mut h = Harness::phone(cx, &dir);
    h.tokens().set(REMOTE, "t1").expect("seed a token");
    {
        let mut host = h.host(&saved_remote());
        host.connect();
        host.send(DaemonMessage::Sessions {
            sessions: vec![session("s1").status("working").build()],
        });
    }

    assert!(drawn(&mut h, "detached-banner"), "the banner's text shows");
    assert!(
        !drawn(&mut h, "detached-stop-all"),
        "the phone's list offers no Stop all"
    );
}

#[gpui::test]
fn a_host_with_no_token_offers_pair_and_opens_the_dialog(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({ "saved_hosts": [{ "name": REMOTE, "port": 8787 }] }),
    );
    let mut h = Harness::phone(cx, &dir);
    h.host(&saved_remote())
        .event(NetEvent::State(not_approved_without_token()));

    let pair = format!("phone-host-pair-{REMOTE}");
    assert!(drawn(&mut h, &format!("phone-host-row-{REMOTE}")));
    assert!(drawn(&mut h, &pair), "a host with no token offers Pair");
    assert!(
        !h.root(|root, _| root.device_pairing_open()),
        "the dialog never opens on its own"
    );

    h.click_on(&pair);
    assert!(h.root(|root, _| root.device_pairing_open()));
    h.click_on("device-pairing-ask");
    h.host(&saved_remote())
        .send(DaemonMessage::DevicePairPending {
            request_id: "r1".to_owned(),
            expires_in_secs: 300,
        });
    h.host(&saved_remote())
        .send(DaemonMessage::DevicePairDecided {
            request_id: "r1".to_owned(),
            outcome: DevicePairOutcome::Approved,
        });
    h.keys("1 2 3 4 5 6");
    assert_eq!(
        h.root(RootView::device_pairing_code).as_deref(),
        Some("123456")
    );
    h.click_on("device-pairing-pair");

    let redeemed: Vec<String> = h
        .host(&saved_remote())
        .commands()
        .into_iter()
        .filter_map(|command| match command {
            NetCommand::PairRedeem { code } => Some(code),
            _ => None,
        })
        .collect();
    assert_eq!(redeemed, ["123456"]);
}

/// Whether `msg` is a presence report or one of This PC's tab and layout
/// messages, none of which a phone sends.
fn desktop_only(msg: &ClientMessage) -> bool {
    matches!(
        msg,
        ClientMessage::ReportPresence { .. }
            | ClientMessage::CreateTab { .. }
            | ClientMessage::RearrangeTab { .. }
            | ClientMessage::ListTabs
            | ClientMessage::RehostPanes { .. }
            | ClientMessage::RebindHostSessions { .. }
    )
}

#[gpui::test]
fn the_phone_sends_no_local_open_tab_layout_or_presence_message(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({ "saved_hosts": [{ "name": REMOTE, "port": 8787 }] }),
    );
    let mut h = Harness::phone(cx, &dir);
    assert_eq!(
        h.opened_targets(),
        vec![saved_remote()],
        "the saved host, and only it, connects at start"
    );
    h.tokens().set(REMOTE, "t1").expect("seed a token");
    let mut features = support::daemon_features();
    features.push(FEATURE_CLIENT_PRESENCE.to_owned());
    let mut sent = {
        let mut host = h.host(&saved_remote());
        host.connect_keeping_sent_with(features);
        host.send(DaemonMessage::Sessions {
            sessions: vec![session("s1").build()],
        });
        host.sent()
    };

    h.cx.update(|window, _| window.activate_window());
    h.cx.run_until_parked();
    sent.extend(h.host(&saved_remote()).sent());

    let opened = h.opened_targets();
    assert!(
        opened.contains(&saved_remote()),
        "the saved host is opened: {opened:?}"
    );
    assert!(
        !opened.contains(&Target::Local),
        "This PC is never opened: {opened:?}"
    );
    let offending: Vec<&ClientMessage> = sent.iter().filter(|msg| desktop_only(msg)).collect();
    assert!(offending.is_empty(), "the phone sent {offending:?}");
}

#[gpui::test]
fn returning_to_the_foreground_redials_a_dropped_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({
            "saved_hosts": [
                { "name": REMOTE, "port": 8787 },
                { "name": TYPED, "port": 8787 },
            ]
        }),
    );
    let mut h = Harness::phone(cx, &dir);
    h.tokens().set(REMOTE, "t1").expect("seed a token");
    h.tokens().set(TYPED, "t2").expect("seed a token");

    h.host(&saved_remote()).connect();
    let typed = Target::tailnet(TYPED, Some(8787));
    h.host(&typed).connect();

    let mut conn = Connection::for_target(saved_remote());
    conn.on_connecting();
    conn.on_socket_open(8787);
    let _ = conn.on_closed("suspended".to_owned());
    h.host(&saved_remote()).event(NetEvent::State(conn));

    let _ = h.host(&saved_remote()).commands();
    let _ = h.host(&typed).commands();
    let opened = h.opened_targets();

    h.cx.simulate_visibility_change(gpui::WindowVisibility::Hidden);
    h.cx.run_until_parked();
    let commanded = h.host(&saved_remote()).commands();
    assert!(
        !commanded
            .iter()
            .any(|command| matches!(command, NetCommand::Restart)),
        "going to the background redials nothing"
    );
    assert_eq!(
        h.opened_targets(),
        opened,
        "going to the background opens nothing"
    );

    h.cx.simulate_visibility_change(gpui::WindowVisibility::Visible);
    h.cx.run_until_parked();
    let restarts = h
        .host(&saved_remote())
        .commands()
        .iter()
        .filter(|command| matches!(command, NetCommand::Restart))
        .count();
    assert_eq!(restarts, 1, "the dropped host is redialled once");
    let commanded = h.host(&typed).commands();
    assert!(
        !commanded
            .iter()
            .any(|command| matches!(command, NetCommand::Restart)),
        "an open host is left alone"
    );
    assert_eq!(h.opened_targets(), opened, "a redial opens nothing new");
}

#[gpui::test]
fn a_phone_keeps_a_menu_open_while_a_host_sends_events(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({ "saved_hosts": [{ "name": REMOTE, "port": 8787 }] }),
    );
    let mut h = Harness::phone(cx, &dir);
    {
        let mut host = h.host(&saved_remote());
        host.connect();
        host.send(DaemonMessage::Sessions {
            sessions: vec![session("s1").build()],
        });
    }
    let remote_key = HostKey::of(&saved_remote());
    h.root_update(move |root, window, cx| {
        root.open_session_menu(
            &remote_key,
            "s1",
            None,
            point(px(40.0), px(12.0)),
            window,
            cx,
        );
    });
    assert_eq!(
        h.root(|root, _| root.session_menu().map(str::to_owned)),
        Some("s1".to_owned()),
        "the menu opens"
    );

    h.host(&saved_remote()).send(DaemonMessage::SessionUpdated {
        session: session("s1").build(),
        request_id: None,
    });
    h.cx.run_until_parked();

    assert_eq!(
        h.root(|root, _| root.session_menu().map(str::to_owned)),
        Some("s1".to_owned()),
        "a host's event leaves the menu open"
    );
}

/// The SSH host the host-key specs dial.
const SSH: &str = "me@box";

/// The target [`SSH`] is.
fn ssh_target() -> Target {
    Target::parse_ssh(SSH, None).expect("an SSH host string")
}

/// A connection to `target` its daemon refused, as an SSH host refuses an
/// unknown host key.
fn refused(target: &Target) -> Connection {
    let mut conn = Connection::for_target(target.clone());
    conn.on_connecting();
    conn.on_auth_failed("the host's key is not trusted".to_owned());
    conn
}

/// The pending host key the window holds for `key`.
fn pending(h: &mut Harness<'_>, key: &HostKey) -> Option<String> {
    h.root(|root, _| root.unverified_fingerprint(key).map(str::to_owned))
}

/// A phone over `dir` with [`SSH`] saved, its connection reporting
/// `SHA256:abc` as the host key the user has not trusted.
fn phone_with_an_unverified_key<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
) -> (Harness<'a>, HostKey) {
    seed_ui(
        dir,
        &json!({ "saved_hosts": [{ "kind": "ssh", "name": SSH }] }),
    );
    let mut h = Harness::phone(cx, dir);
    let target = ssh_target();
    let key = HostKey::of(&target);
    h.host(&target).event(NetEvent::HostKeyUnverified {
        fingerprint: "SHA256:abc".to_owned(),
    });
    h.host(&target).event(NetEvent::State(refused(&target)));
    (h, key)
}

#[gpui::test]
fn an_unverified_host_key_is_held_until_the_connection_leaves_auth_failed(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, key) = phone_with_an_unverified_key(cx, &dir);
    assert_eq!(
        pending(&mut h, &key),
        Some("SHA256:abc".to_owned()),
        "a refused host key stays pending until the user answers"
    );

    let mut connecting = Connection::for_target(ssh_target());
    connecting.on_connecting();
    h.host(&ssh_target()).event(NetEvent::State(connecting));

    assert_eq!(
        pending(&mut h, &key),
        None,
        "a connection that left auth failed holds no pending key"
    );
}

#[gpui::test]
fn trusting_an_unverified_host_key_saves_it_and_redials_with_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, key) = phone_with_an_unverified_key(cx, &dir);

    h.root_update(|root, window, cx| root.trust_host_key(&key, window, cx));

    let dialled = h
        .opened_targets()
        .pop()
        .expect("Trust dials the host again");
    assert_eq!(
        dialled.ssh_host().and_then(SshHost::host_fingerprint),
        Some("SHA256:abc"),
        "the redial carries the trusted key"
    );
    assert_eq!(
        saved_ui(&dir)["saved_hosts"][0]["host_fingerprint"],
        "SHA256:abc",
        "the saved record keeps the trusted key"
    );
    assert_eq!(pending(&mut h, &key), None, "nothing is pending any more");
}

#[gpui::test]
fn cancelling_an_unverified_host_key_saves_nothing_and_does_not_redial(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, key) = phone_with_an_unverified_key(cx, &dir);
    let opened = h.opened_targets();

    h.root_update(|root, _, cx| root.cancel_host_key(&key, cx));

    assert_eq!(h.opened_targets(), opened, "Cancel dials nothing");
    assert!(
        saved_ui(&dir)["saved_hosts"][0]
            .get("host_fingerprint")
            .is_none(),
        "Cancel saves no key"
    );
    assert_eq!(pending(&mut h, &key), None, "nothing is pending any more");
}

#[gpui::test]
fn re_adding_a_trusted_ssh_host_dials_with_its_pin(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({
            "saved_hosts": [{
                "kind": "ssh",
                "name": SSH,
                "enabled": false,
                "host_fingerprint": "SHA256:abc",
            }],
        }),
    );
    let mut h = Harness::phone(cx, &dir);
    assert!(
        h.opened_targets().is_empty(),
        "a switched-off host dials nothing at launch"
    );

    h.root_update(|root, window, cx| root.add_saved_host(ssh_target(), window, cx));

    let dialled = h.opened_targets().pop().expect("Add connects it");
    assert_eq!(
        dialled.ssh_host().and_then(SshHost::host_fingerprint),
        Some("SHA256:abc"),
        "the saved key is dialled with, not the typed target's"
    );
}

#[gpui::test]
fn the_add_form_has_an_ssh_mode_that_adds_a_typed_ssh_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::phone(cx, &dir);

    assert!(
        drawn(&mut h, "phone-host-mode-tailnet"),
        "a phone with no saved host opens the form on Tailnet"
    );
    h.click_on("phone-host-mode-ssh");
    assert!(
        drawn(&mut h, "phone-host-ssh"),
        "SSH mode shows its host field"
    );
    h.click_on("phone-host-ssh");
    type_input(&mut h, SSH);
    h.click_on("phone-host-command");
    type_input(&mut h, "hostname");
    h.click_on("phone-host-add");

    let typed = Target::parse_ssh(SSH, Some("hostname")).expect("an SSH target");
    assert_eq!(h.opened_targets(), vec![typed], "Add connects the SSH host");
    let saved = saved_ui(&dir)["saved_hosts"].clone();
    assert_eq!(
        saved[0]["kind"], "ssh",
        "the record is an SSH host: {saved}"
    );
    assert_eq!(saved[0]["name"], SSH, "the record keeps the typed host");
    assert_eq!(
        saved[0]["remote_command"], "hostname",
        "the record keeps the typed command"
    );
}

#[gpui::test]
fn an_ssh_host_without_user_at_is_refused_in_the_form(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::phone(cx, &dir);
    h.click_on("phone-host-mode-ssh");
    h.click_on("phone-host-ssh");
    type_input(&mut h, "box");
    h.click_on("phone-host-add");

    assert!(drawn(&mut h, "phone-host-error"), "the refusal shows");
    assert_eq!(
        host_error(&mut h),
        Some(
            "Type it as user@host or user@host:port: the phone has no SSH config to look a name up \
             in."
                .to_owned()
        )
    );
    assert!(
        h.opened_targets().is_empty(),
        "a refused host opens nothing"
    );
}

#[gpui::test]
fn the_ssh_form_shows_the_phones_public_key_and_copy_puts_it_on_the_clipboard(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let mut h = Harness::phone(cx, &dir);
    h.click_on("phone-host-mode-ssh");

    assert!(drawn(&mut h, "phone-host-key"), "the phone's key shows");
    h.click_on("phone-host-copy-key");
    let copied = h.clipboard().expect("Copy puts the key on the clipboard");
    assert!(
        copied.starts_with("ssh-ed25519 "),
        "an authorized_keys line: {copied}"
    );
    assert!(
        copied.ends_with(" rustling-tulip-phone"),
        "carrying the phone's key comment: {copied}"
    );
}

#[gpui::test]
fn an_unknown_host_key_shows_its_fingerprint_with_trust_and_cancel(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, key) = phone_with_an_unverified_key(cx, &dir);
    assert_eq!(
        pending(&mut h, &key),
        Some("SHA256:abc".to_owned()),
        "the host key is pending"
    );

    for selector in [
        format!("phone-host-hostkey-{SSH}"),
        format!("phone-host-trust-{SSH}"),
        format!("phone-host-cancel-{SSH}"),
    ] {
        assert!(drawn(&mut h, &selector), "{selector} shows");
    }
    assert!(
        !drawn(&mut h, &format!("phone-host-reason-{SSH}")),
        "the prompt replaces the reason line"
    );
    assert!(
        !drawn(&mut h, &format!("phone-host-retry-{SSH}")),
        "the prompt replaces Retry"
    );
}

#[gpui::test]
fn trust_on_the_row_saves_the_fingerprint_and_redials(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, key) = phone_with_an_unverified_key(cx, &dir);

    h.click_on(&format!("phone-host-trust-{SSH}"));

    let dialled = h
        .opened_targets()
        .pop()
        .expect("Trust dials the host again");
    assert_eq!(
        dialled.ssh_host().and_then(SshHost::host_fingerprint),
        Some("SHA256:abc"),
        "the redial carries the trusted key"
    );
    assert_eq!(
        saved_ui(&dir)["saved_hosts"][0]["host_fingerprint"],
        "SHA256:abc",
        "the saved record keeps the trusted key"
    );
    assert!(
        !drawn(&mut h, &format!("phone-host-hostkey-{SSH}")),
        "the prompt goes once answered"
    );
    assert_eq!(pending(&mut h, &key), None, "nothing is pending any more");
}

#[gpui::test]
fn cancel_on_the_row_saves_nothing_and_hides_the_prompt(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, key) = phone_with_an_unverified_key(cx, &dir);
    let opened = h.opened_targets();

    h.click_on(&format!("phone-host-cancel-{SSH}"));

    assert_eq!(h.opened_targets(), opened, "Cancel dials nothing");
    assert!(
        saved_ui(&dir)["saved_hosts"][0]
            .get("host_fingerprint")
            .is_none(),
        "Cancel saves no key"
    );
    assert!(
        !drawn(&mut h, &format!("phone-host-hostkey-{SSH}")),
        "the prompt goes once answered"
    );
    assert_eq!(pending(&mut h, &key), None, "nothing is pending any more");
}

#[gpui::test]
fn a_stopped_host_shows_its_reason_and_retry_restarts_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({ "saved_hosts": [{ "kind": "ssh", "name": SSH }] }),
    );
    let mut h = Harness::phone(cx, &dir);
    h.host(&ssh_target())
        .event(NetEvent::State(refused(&ssh_target())));

    assert!(
        drawn(&mut h, &format!("phone-host-reason-{SSH}")),
        "a stopped host shows its reason"
    );
    assert!(
        drawn(&mut h, &format!("phone-host-retry-{SSH}")),
        "a stopped host offers Retry"
    );
    let _ = h.host(&ssh_target()).commands();

    h.click_on(&format!("phone-host-retry-{SSH}"));

    let restarts = h
        .host(&ssh_target())
        .commands()
        .iter()
        .filter(|command| matches!(command, NetCommand::Restart))
        .count();
    assert_eq!(restarts, 1, "Retry restarts the host");
}

#[gpui::test]
fn an_ssh_only_host_offers_no_notifications_switch(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({ "saved_hosts": [{ "kind": "ssh", "name": SSH }] }),
    );
    let mut h = Harness::phone(cx, &dir);
    let mut features = support::daemon_features();
    features.push(FEATURE_RELAY_PUSH.to_owned());
    {
        let mut host = h.host(&ssh_target());
        host.connect_keeping_sent_with(features);
        host.send(DaemonMessage::Sessions {
            sessions: vec![session("s1").build()],
        });
    }

    assert!(drawn(&mut h, "phone-home"), "the open SSH host shows Home");
    assert!(
        !drawn(&mut h, &format!("phone-host-push-{SSH}")),
        "an SSH-only host offers no Notifications switch"
    );
    assert!(
        !h.host(&ssh_target())
            .sent()
            .iter()
            .any(|msg| matches!(msg, ClientMessage::RegisterPush { .. })),
        "an SSH-only host registers no push"
    );
}
