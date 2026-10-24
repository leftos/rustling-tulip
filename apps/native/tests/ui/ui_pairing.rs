//! Device approval dialog specs: the Confirm phase a row's Approve… opens,
//! its Approve sending `ApproveDevice`, the code phase with its QR code,
//! grouped code and countdown, Deny and Close, the dialog closing with its
//! request or its code, and its keys.

use std::time::Duration;

use crate::support;

use chrono::{TimeDelta, Utc};
use gpui::{TestAppContext, px};
use protocol::{ClientMessage, PendingDevice, Secret};
use support::{Fixture, Harness, TestDir, session};

const TAB: &str = "settings-tab-remote-access";
const ROW_APPROVE: &str = "settings-waiting-approve-r1";
const APPROVE: &str = "pairing-approve";
const DENY: &str = "pairing-deny";
const CLOSE: &str = "pairing-close";
const CONFIRM_BUTTONS: [&str; 3] = ["Approve", "Deny", "Close"];
const CODE_BUTTONS: [&str; 2] = ["Deny", "Close"];

/// A request from `phone` that asked 14 seconds ago.
fn request(id: &str) -> PendingDevice {
    PendingDevice {
        request_id: id.to_owned(),
        name: "phone".to_owned(),
        login: "alice@example.com".to_owned(),
        asked_at: Utc::now() - TimeDelta::seconds(14),
        approved: false,
    }
}

/// The window on session `s1` alone, its scrollback answered and what it
/// sent at connect drained.
fn opened<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = Harness::with(cx, dir, &Fixture::single(session("s1").build()));
    h.answer_scrollback("s1", b"");
    h.sent();
    h
}

fn fold(h: &mut Harness<'_>, requests: &[PendingDevice]) {
    h.root_update(|root, window, cx| root.fold_pending_devices(requests, window, cx));
}

/// Folds in the code `735160`, minted for `request_id`, good for `secs`.
fn fold_code(h: &mut Harness<'_>, request_id: Option<&str>, secs: u64) -> bool {
    let code = Secret("735160".to_owned());
    h.root_update(|root, window, cx| root.fold_device_code(request_id, &code, secs, window, cx))
}

/// Settings open on the Remote access page with request `r1` waiting, its
/// dialog opened from the row's Approve… and what was sent drained.
fn confirm_open(h: &mut Harness<'_>) {
    h.keys("ctrl-,");
    h.click_on(TAB);
    fold(h, &[request("r1")]);
    h.click_on(ROW_APPROVE);
    assert!(dialog_open(h), "Approve… opens the dialog");
    h.sent();
}

/// [`confirm_open`], then Approve and the code the daemon minted for it.
fn code_shown(h: &mut Harness<'_>) {
    confirm_open(h);
    h.click_on(APPROVE);
    h.sent();
    assert!(fold_code(h, Some("r1"), 120), "the client shows the code");
}

fn dialog_open(h: &mut Harness<'_>) -> bool {
    h.root(|root, _| root.pairing_dialog_open())
}

fn texts(h: &mut Harness<'_>) -> Vec<String> {
    h.root(|root, _| root.pairing_dialog_texts())
}

fn buttons(h: &mut Harness<'_>) -> Vec<&'static str> {
    h.root(|root, _| root.pairing_dialog_buttons())
}

fn focused(h: &mut Harness<'_>) -> Option<&'static str> {
    h.root(|root, _| root.pairing_focused_button())
}

fn pairing_focus(h: &mut Harness<'_>) -> bool {
    let root = h.root.clone();
    h.cx.update(|window, cx| root.read(cx).pairing_has_focus(window))
}

fn approves(id: &str) -> impl Fn(&ClientMessage) -> bool + '_ {
    move |msg| matches!(msg, ClientMessage::ApproveDevice { request_id } if request_id == id)
}

fn denies(id: &str) -> impl Fn(&ClientMessage) -> bool + '_ {
    move |msg| matches!(msg, ClientMessage::DenyDevice { request_id } if request_id == id)
}

/// Whether the client sent exactly one message since the last read, and it
/// is the one `wanted` matches.
fn sent_only(h: &mut Harness<'_>, wanted: impl Fn(&ClientMessage) -> bool) -> bool {
    let sent = h.sent();
    sent.len() == 1 && wanted(&sent[0])
}

#[gpui::test]
fn confirm_phase_shows_the_device_and_the_sentence(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    confirm_open(&mut h);
    assert_eq!(
        texts(&mut h),
        [
            "Approve phone",
            "phone · signed in to Tailscale as alice@example.com",
            "Approving shows a one-time code for this device."
        ]
    );
    assert_eq!(buttons(&mut h), CONFIRM_BUTTONS);
    assert_eq!(h.root(|root, _| root.pairing_qr_width()), None, "no QR yet");
    assert!(
        h.root(|root, _| root.settings_open()),
        "it sits over Settings"
    );
}

#[gpui::test]
fn confirm_approve_sends_approve_device(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    confirm_open(&mut h);
    h.click_on(APPROVE);
    assert!(sent_only(&mut h, approves("r1")), "ApproveDevice r1 alone");
    assert!(dialog_open(&mut h), "the dialog waits for the code");
    assert_eq!(buttons(&mut h), CONFIRM_BUTTONS);
}

#[gpui::test]
fn approve_sends_once(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    confirm_open(&mut h);
    h.click_on(APPROVE);
    h.click_on(APPROVE);
    assert!(sent_only(&mut h, approves("r1")), "one ApproveDevice");
    h.keys("tab enter");
    assert!(h.sent().is_empty(), "a pressed Approve waits for its code");
    assert!(dialog_open(&mut h));

    h.click_on(CLOSE);
    h.click_on(ROW_APPROVE);
    h.click_on(APPROVE);
    assert!(
        sent_only(&mut h, approves("r1")),
        "a reopened dialog approves again"
    );
}

#[gpui::test]
fn a_lapsed_code_takes_no_focus(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    confirm_open(&mut h);
    h.click_on(CLOSE);
    assert!(!pairing_focus(&mut h));
    fold_code(&mut h, Some("r1"), 0);
    assert!(!dialog_open(&mut h), "a lapsed code shows nothing");
    assert!(!pairing_focus(&mut h), "nothing drawn takes the keyboard");
}

#[gpui::test]
fn the_pairing_dialog_draws_above_recover(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    fold(&mut h, &[request("r1")]);
    h.click_on("activity-recover");
    assert!(h.root(|root, _| root.recover_open()));
    assert!(fold_code(&mut h, Some("r1"), 120));
    h.click_on(CLOSE);
    assert!(
        !dialog_open(&mut h),
        "Close was the topmost thing at its spot"
    );
    assert!(h.root(|root, _| root.recover_open()), "Recover stays open");
}

#[gpui::test]
fn the_code_phase_shows_qr_and_grouped_code(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    code_shown(&mut h);
    let shown = texts(&mut h);
    assert_eq!(shown[0], "Approve phone");
    assert!(shown.iter().any(|t| t == "735 160"), "{shown:?}");
    assert!(
        shown
            .iter()
            .any(|t| t == "expires in 2:00 · this request only"),
        "{shown:?}"
    );
    assert!(
        !shown.iter().any(|t| t.contains("735160")),
        "the code reads in two groups: {shown:?}"
    );
    assert_eq!(h.root(|root, _| root.pairing_qr_width()), Some(21));
    assert_eq!(buttons(&mut h), CODE_BUTTONS);
    let qr = h.bounds("pairing-qr");
    assert_eq!(
        qr.size.width,
        px(174.0),
        "21 modules and the quiet zone at 6 px"
    );
    assert_eq!(qr.size.height, px(174.0));
}

#[gpui::test]
fn a_code_for_a_request_opens_its_dialog(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    fold(&mut h, &[request("r1")]);
    assert!(fold_code(&mut h, Some("r1"), 120));
    assert!(dialog_open(&mut h));
    assert_eq!(buttons(&mut h), CODE_BUTTONS);
}

#[gpui::test]
fn code_countdown_ticks_and_dialog_closes_at_zero(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    code_shown(&mut h);
    h.advance(Duration::from_secs(2));
    assert!(
        texts(&mut h)
            .iter()
            .any(|t| t == "expires in 1:58 · this request only")
    );
    h.advance(Duration::from_secs(118));
    assert!(!dialog_open(&mut h), "the dialog ends with its code");
    assert!(
        !h.root(|root, _| root.pairing_dialog_held()),
        "the tick let the lapsed dialog go"
    );
    assert!(!pairing_focus(&mut h), "the keyboard went back to Settings");
    assert!(h.sent().is_empty(), "the lapse sends nothing");
    h.keys("escape");
    assert!(
        !h.root(|root, _| root.settings_open()),
        "Esc reaches Settings again"
    );
}

#[gpui::test]
fn deny_sends_deny_device_and_closes_the_dialog(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    confirm_open(&mut h);
    h.click_on(DENY);
    assert!(sent_only(&mut h, denies("r1")), "Confirm phase");
    assert!(!dialog_open(&mut h));

    h.click_on(ROW_APPROVE);
    h.click_on(APPROVE);
    h.sent();
    assert!(fold_code(&mut h, Some("r1"), 120));
    h.click_on(DENY);
    assert!(sent_only(&mut h, denies("r1")), "code phase");
    assert!(!dialog_open(&mut h));
}

#[gpui::test]
fn close_sends_nothing_and_drops_the_code(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    code_shown(&mut h);
    h.click_on(CLOSE);
    assert!(h.sent().is_empty());
    assert!(!dialog_open(&mut h));
    assert!(
        h.root(|root, _| root.settings_open()),
        "Settings stays open"
    );

    h.click_on(ROW_APPROVE);
    assert_eq!(buttons(&mut h), CONFIRM_BUTTONS, "the code went with it");
    assert!(!texts(&mut h).iter().any(|t| t == "735 160"));
}

#[gpui::test]
fn dialog_closes_when_the_daemon_drops_the_request(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    code_shown(&mut h);
    fold(&mut h, &[]);
    assert!(!dialog_open(&mut h));
    assert_eq!(texts(&mut h), Vec::<String>::new());
}

#[gpui::test]
fn cli_minted_code_opens_no_dialog(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    fold(&mut h, &[request("r1")]);
    assert!(!fold_code(&mut h, None, 120));
    assert!(!dialog_open(&mut h));
}

#[gpui::test]
fn escape_closes_the_dialog_and_leaves_settings_open(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    code_shown(&mut h);
    h.keys("escape");
    assert!(!dialog_open(&mut h));
    assert!(h.sent().is_empty());
    assert!(h.root(|root, _| root.settings_open()));
    assert_eq!(h.root(|root, _| root.settings_tab()), "Remote access");
}

#[gpui::test]
fn tab_cycles_the_buttons_and_enter_waits_for_focus(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    confirm_open(&mut h);
    assert_eq!(focused(&mut h), None, "focus starts on the panel");
    h.keys("enter space");
    assert!(h.sent().is_empty(), "Enter approves nothing unfocused");
    assert!(dialog_open(&mut h));

    let mut walk = Vec::new();
    for _ in 0..4 {
        h.keys("tab");
        walk.push(focused(&mut h));
    }
    assert_eq!(
        walk,
        [
            Some("Approve"),
            Some("Deny"),
            Some("Close"),
            Some("Approve")
        ]
    );
    h.keys("shift-tab");
    assert_eq!(focused(&mut h), Some("Close"));
    h.keys("tab enter");
    assert!(sent_only(&mut h, approves("r1")), "Enter presses Approve");

    assert!(fold_code(&mut h, Some("r1"), 120));
    assert_eq!(focused(&mut h), None, "the code phase starts unfocused");
    h.keys("tab tab enter");
    assert!(h.sent().is_empty(), "Close sends nothing");
    assert!(!dialog_open(&mut h));
}
