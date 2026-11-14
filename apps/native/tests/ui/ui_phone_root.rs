//! The phone root: a `RootView` in the phone shape, with no This PC, drawn
//! by the phone's own root: the first-run form, the saved hosts' rows and
//! the Pair dialog.

use crate::support;

use daemon_client::device_tokens::DeviceTokenStore as _;
use gpui::{TestAppContext, point, px};
use protocol::{
    ClientMessage, DEVICE_NOT_APPROVED, DaemonMessage, DevicePairOutcome, FEATURE_CLIENT_PRESENCE,
};
use rustling_tulip_native::connection_picker::TypedHostError;
use rustling_tulip_native::phone::PhoneRoot;
use rustling_tulip_native::target::{HostKey, Target};
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
    assert!(drawn(&mut h, &format!("phone-host-row-{REMOTE}")));
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
