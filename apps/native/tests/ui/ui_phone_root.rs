//! The phone root: a `RootView` in the phone shape, with no This PC.

use crate::support;

use daemon_client::device_tokens::DeviceTokenStore as _;
use gpui::{TestAppContext, point, px};
use protocol::{ClientMessage, DaemonMessage, FEATURE_CLIENT_PRESENCE};
use rustling_tulip_native::target::{HostKey, Target};
use serde_json::json;
use support::{Harness, REMOTE, TestDir, saved_remote, seed_ui, session};

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
