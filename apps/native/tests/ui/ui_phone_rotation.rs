//! The phone's Terminal page re-claims the PTY size when the window turns
//! or otherwise changes size under an open terminal, so the session redraws
//! at the new width; a keyboard rising or falling, or a resize to the same
//! size, sends no claim.

use crate::support;

use daemon_client::device_tokens::DeviceTokenStore as _;
use gpui::{Edges, Pixels, TestAppContext, WindowInsets, px, size};
use protocol::{ClientMessage, DaemonMessage, SessionSnapshot};
use rustling_tulip_native::phone::PhoneRoot;
use rustling_tulip_native::target::HostKey;
use serde_json::json;
use support::{Harness, REMOTE, TestDir, repo, saved_remote, seed_ui, session};

/// `n`, a shell session working with no agent, in r1.
fn listed() -> Vec<SessionSnapshot> {
    vec![
        session("n")
            .label("November")
            .in_repo("r1")
            .status("working")
            .no_agent()
            .build(),
    ]
}

/// A phone with the saved [`REMOTE`] open, listing [`listed`], everything
/// sent so far drained.
fn open_phone<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    seed_ui(
        dir,
        &json!({ "saved_hosts": [{ "name": REMOTE, "port": 8787 }] }),
    );
    let mut h = Harness::phone(cx, dir);
    h.tokens().set(REMOTE, "t1").expect("seed a token");
    let mut host = h.host(&saved_remote());
    host.connect();
    host.send(DaemonMessage::Repos {
        repos: vec![repo("r1", "C:/r1")],
    });
    host.send(DaemonMessage::Sessions { sessions: listed() });
    h.host(&saved_remote()).sent();
    h
}

/// The Home row that opens session `id`.
fn row(id: &str) -> String {
    let wire = HostKey::of(&saved_remote())
        .wire()
        .expect("a tailnet host has a wire key")
        .to_owned();
    format!("phone-home-row-{wire}::{id}")
}

/// Shell session `n` on its Terminal page in a portrait phone window, its
/// scrollback answered and everything sent so far drained.
fn on_terminal<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = open_phone(cx, dir);
    h.cx.simulate_resize(size(px(390.0), px(844.0)));
    h.cx.run_until_parked();
    h.click_on(&row("n"));
    h.host(&saved_remote()).pty("n", b"");
    h.cx.run_until_parked();
    h.host(&saved_remote()).sent();
    h
}

/// Runs `drive` on the phone's root and lets the window settle.
fn phone_update(
    h: &mut Harness<'_>,
    drive: impl FnOnce(&mut PhoneRoot, &mut gpui::Context<PhoneRoot>),
) {
    h.cx.update(|window, cx| {
        let phone = window
            .root::<PhoneRoot>()
            .flatten()
            .expect("the window's root is the phone's");
        phone.update(cx, drive);
    });
    h.cx.run_until_parked();
}

/// The insets a phone draws with a `keyboard`-tall on-screen keyboard up.
fn keyboard_insets(keyboard: Pixels) -> WindowInsets {
    WindowInsets {
        safe_area: Edges {
            top: px(62.0),
            bottom: px(34.0),
            ..Edges::default()
        },
        ime: Edges {
            bottom: keyboard,
            ..Edges::default()
        },
        ..WindowInsets::default()
    }
}

/// The claim, cols and rows of the first `Resize` for session `id` in
/// `sent`, if any.
fn first_resize(sent: &[ClientMessage], id: &str) -> Option<(bool, u16, u16)> {
    sent.iter().find_map(|msg| match msg {
        ClientMessage::Resize {
            session_id,
            cols,
            rows,
            claim,
        } if session_id == id => Some((*claim, *cols, *rows)),
        _ => None,
    })
}

#[gpui::test]
fn a_rotation_claims_the_new_size(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_terminal(cx, &dir);

    h.cx.simulate_resize(size(px(844.0), px(390.0)));
    h.cx.run_until_parked();

    let sent = h.host(&saved_remote()).sent();
    let (claim, cols, rows) = first_resize(&sent, "n").expect("the rotation sends a resize");
    assert!(claim, "the rotation claims the size: {sent:?}");
    assert!(cols > rows, "the landscape size is wide: {cols}x{rows}");
}

#[gpui::test]
fn a_resize_to_the_same_size_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_terminal(cx, &dir);

    h.cx.simulate_resize(size(px(390.0), px(844.0)));
    h.cx.run_until_parked();

    let sent = h.host(&saved_remote()).sent();
    assert_eq!(first_resize(&sent, "n"), None, "no resize: {sent:?}");
}

#[gpui::test]
fn a_keyboard_rising_and_falling_claims_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_terminal(cx, &dir);
    phone_update(&mut h, |phone, cx| {
        phone.set_insets_override(Some(keyboard_insets(px(0.0))), cx);
    });
    h.host(&saved_remote()).sent();

    phone_update(&mut h, |phone, cx| {
        phone.set_insets_override(Some(keyboard_insets(px(336.0))), cx);
    });
    let risen = h.host(&saved_remote()).sent();
    phone_update(&mut h, |phone, cx| {
        phone.set_insets_override(Some(keyboard_insets(px(0.0))), cx);
    });
    let fallen = h.host(&saved_remote()).sent();

    assert_eq!(first_resize(&risen, "n"), None, "rising: {risen:?}");
    assert_eq!(first_resize(&fallen, "n"), None, "falling: {fallen:?}");
}
