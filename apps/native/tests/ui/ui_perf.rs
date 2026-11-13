//! Perf specs: the machines whose CPU and RAM the status bar tracks, This
//! PC by default; the `WatchPerf` subscription each tracked host whose
//! daemon reports CPU and RAM gets on every `Welcome`, and its end on an
//! untrack; the samples folded into each tracked host's pill, a waiting
//! pill after a reconnect, an offline one after a close; and the saved
//! choice. The footer draws one pill per tracked machine, amber when it
//! runs hot, or a placeholder when none is tracked; a right-click on a
//! pill opens the menu that tracks and untracks machines, and forgetting
//! a saved host untracks it.

use crate::support;

use std::sync::Arc;
use std::time::Duration;

use gpui::{Modifiers, TestAppContext, px};
use protocol::{ClientMessage, FEATURE_PERF_MONITOR};
use rustling_tulip_native::connection_picker::PeerSource;
use rustling_tulip_native::perf_pills::PillLevel;
use rustling_tulip_native::target::{HostKey, Target};
use rustling_tulip_native::{Connection, NetEvent};
use serde_json::json;
use support::{Fixture, Harness, TestDir, perf_msg, saved_ui, seed_ui};
use tailnet::{Status, TailnetError};

const GIB: u64 = 1_073_741_824;

/// The tailnet host the specs connect beside This PC.
const REMOTE: &str = "box.tail1234.ts.net";

fn remote() -> Target {
    Target::tailnet(REMOTE, None)
}

fn remote_key() -> HostKey {
    HostKey::of(&remote())
}

/// The `on` of each `WatchPerf` in `sent`, in order.
fn watches(sent: &[ClientMessage]) -> Vec<bool> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::WatchPerf { on } => Some(*on),
            _ => None,
        })
        .collect()
}

/// Each pill's host key and text, in the order the status bar shows them.
fn pills(h: &mut Harness<'_>) -> Vec<(String, String)> {
    h.root(|root, _| {
        root.perf_pills()
            .into_iter()
            .map(|(key, pill)| (key, pill.text))
            .collect()
    })
}

fn pill(key: &str, text: &str) -> (String, String) {
    (key.to_owned(), text.to_owned())
}

fn toggle(h: &mut Harness<'_>, host: &HostKey) {
    h.root_update(|root, _, cx| root.toggle_perf_tracked(host, cx));
}

/// This PC reconnected to a daemon reporting CPU and RAM, what it sent on
/// the `Welcome` drained.
fn this_pc_reports_perf(h: &mut Harness<'_>) {
    h.reconnect_with_features(&[FEATURE_PERF_MONITOR]);
    h.sent();
}

/// The remote host connected to a daemon reporting CPU and RAM.
fn remote_reports_perf(h: &mut Harness<'_>) {
    h.host(&remote())
        .connect_with_features(&[FEATURE_PERF_MONITOR]);
    h.sent();
}

#[gpui::test]
fn this_pc_is_tracked_by_default_and_subscribed_on_welcome(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    h.reconnect_with_features(&[FEATURE_PERF_MONITOR]);
    assert_eq!(watches(&h.sent()), [true], "This PC is subscribed");

    h.send(perf_msg(Some(23.4), 8 * GIB, 32 * GIB));
    assert_eq!(pills(&mut h), [pill("local", "This PC CPU 23% · RAM 25%")]);
}

#[gpui::test]
fn a_host_without_the_feature_is_never_subscribed(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    h.reconnect_with_features(&[]);
    assert_eq!(watches(&h.sent()), [] as [bool; 0]);
    assert_eq!(pills(&mut h), [] as [(String, String); 0]);
}

#[gpui::test]
fn tracking_a_remote_host_subscribes_it_and_saves_the_choice(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    this_pc_reports_perf(&mut h);
    remote_reports_perf(&mut h);
    assert_eq!(
        watches(&h.host(&remote()).sent()),
        [] as [bool; 0],
        "an untracked host is not subscribed"
    );

    toggle(&mut h, &remote_key());
    assert_eq!(watches(&h.host(&remote()).sent()), [true]);
    assert_eq!(watches(&h.sent()), [] as [bool; 0], "This PC hears nothing");
    assert_eq!(saved_ui(&dir)["perf_tracked"], json!(["local", REMOTE]));

    h.send(perf_msg(Some(23.4), 8 * GIB, 32 * GIB));
    h.host(&remote())
        .send(perf_msg(Some(50.0), 16 * GIB, 32 * GIB));
    assert_eq!(
        pills(&mut h),
        [
            pill("local", "This PC CPU 23% · RAM 25%"),
            pill(REMOTE, "box CPU 50% · RAM 50%"),
        ]
    );
}

#[gpui::test]
fn untracking_unsubscribes_and_drops_the_pill(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    this_pc_reports_perf(&mut h);
    h.send(perf_msg(Some(23.4), 8 * GIB, 32 * GIB));

    toggle(&mut h, &HostKey::Local);
    assert_eq!(watches(&h.sent()), [false]);
    assert_eq!(pills(&mut h), [] as [(String, String); 0]);
    assert_eq!(saved_ui(&dir)["perf_tracked"], json!([]));
}

#[gpui::test]
fn a_reconnect_subscribes_again_and_waits_for_a_new_sample(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    this_pc_reports_perf(&mut h);
    h.send(perf_msg(Some(23.4), 8 * GIB, 32 * GIB));

    h.reconnect_with_features(&[FEATURE_PERF_MONITOR]);
    assert_eq!(watches(&h.sent()), [true], "subscribed again");
    assert_eq!(pills(&mut h), [pill("local", "This PC CPU – · RAM –")]);

    h.send(perf_msg(None, 8 * GIB, 32 * GIB));
    assert_eq!(pills(&mut h), [pill("local", "This PC CPU – · RAM 25%")]);
}

#[gpui::test]
fn a_closed_host_shows_offline(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    remote_reports_perf(&mut h);
    toggle(&mut h, &remote_key());
    h.host(&remote())
        .send(perf_msg(Some(50.0), 16 * GIB, 32 * GIB));

    let mut closed = Connection::for_target(remote());
    closed.on_connecting();
    let _ = closed.on_closed("socket closed".to_owned());
    h.host(&remote()).event(NetEvent::State(closed));
    assert_eq!(
        pills(&mut h),
        [pill(REMOTE, "box offline")],
        "This PC's daemon reports no CPU and RAM, so only the remote host has a pill"
    );
}

#[gpui::test]
fn a_sample_from_an_untracked_host_is_ignored(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    remote_reports_perf(&mut h);
    h.host(&remote())
        .send(perf_msg(Some(50.0), 16 * GIB, 32 * GIB));
    assert_eq!(pills(&mut h), [] as [(String, String); 0]);

    toggle(&mut h, &remote_key());
    assert_eq!(
        pills(&mut h),
        [pill(REMOTE, "box CPU – · RAM –")],
        "the sample sent while untracked was not kept"
    );
}

#[gpui::test]
fn a_saved_choice_is_used_at_launch(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(&dir, &json!({ "perf_tracked": [] }));
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    h.reconnect_with_features(&[FEATURE_PERF_MONITOR]);
    assert_eq!(watches(&h.sent()), [] as [bool; 0], "nothing is tracked");
    assert_eq!(pills(&mut h), [] as [(String, String); 0]);
}

/// The tip of a connected host's row whose daemon reports no CPU and RAM.
const UNSUPPORTED_TIP: &str = "This host's daemon does not report CPU and RAM";

fn drawn(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).origin.x >= px(0.0)
}

fn menu_open(h: &mut Harness<'_>) -> bool {
    h.root(|root, _| root.perf_menu_open())
}

/// The menu's machine rows: each one's host key, label and whether it can
/// be pressed.
fn menu_rows(h: &mut Harness<'_>) -> Vec<(String, String, bool)> {
    h.root(|root, _| root.perf_menu_rows())
}

fn menu_row(key: &str, label: &str, enabled: bool) -> (String, String, bool) {
    (key.to_owned(), label.to_owned(), enabled)
}

fn row_selector(key: &str) -> String {
    format!("perf-menu-{key}")
}

/// Whether resting the mouse on `selector` draws a tooltip reading `text`
/// against the mouse there.
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

/// A Tailscale that sees no peer, so the host picker scans nothing real.
struct NoPeers;

impl PeerSource for NoPeers {
    fn status(&self) -> Result<Status, TailnetError> {
        Ok(Status::default())
    }
}

#[gpui::test]
fn the_tracked_pill_draws_its_text_in_the_footer(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    this_pc_reports_perf(&mut h);
    h.send(perf_msg(Some(23.4), 8 * GIB, 32 * GIB));
    assert!(drawn(&mut h, "footer-perf-local"), "the pill is on screen");
    assert!(
        drawn(&mut h, "footer-perf-local=This PC CPU 23% · RAM 25%"),
        "the pill draws its text"
    );
}

#[gpui::test]
fn a_hot_machine_draws_its_pill_amber(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    this_pc_reports_perf(&mut h);
    h.send(perf_msg(Some(23.4), 8 * GIB, 32 * GIB));
    assert_eq!(
        h.root(|root, _| root.perf_pill_level("local")),
        Some(PillLevel::Normal)
    );
    h.send(perf_msg(Some(87.4), 8 * GIB, 32 * GIB));
    assert_eq!(
        h.root(|root, _| root.perf_pill_level("local")),
        Some(PillLevel::Warn)
    );
}

#[gpui::test]
fn nothing_tracked_draws_the_placeholder(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(&dir, &json!({ "perf_tracked": [] }));
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    this_pc_reports_perf(&mut h);
    h.send(perf_msg(Some(23.4), 8 * GIB, 32 * GIB));
    assert!(
        drawn(&mut h, "footer-perf-none=CPU · RAM"),
        "the placeholder"
    );
    assert!(!drawn(&mut h, "footer-perf-local"), "no machine's pill");
    assert_eq!(pills(&mut h), [] as [(String, String); 0]);
    assert_eq!(h.root(|root, _| root.perf_pill_level("local")), None);
}

#[gpui::test]
fn right_click_opens_the_menu_and_a_row_toggles_tracking(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    this_pc_reports_perf(&mut h);
    remote_reports_perf(&mut h);
    h.send(perf_msg(Some(23.4), 8 * GIB, 32 * GIB));
    assert!(!menu_open(&mut h), "no menu before the right-click");

    h.right_click_on("footer-perf-local");
    assert!(menu_open(&mut h), "the right-click opens the menu");
    assert!(drawn(&mut h, &row_selector("local")));
    assert!(drawn(&mut h, &row_selector(REMOTE)));
    assert_eq!(
        menu_rows(&mut h),
        [
            menu_row("local", "✓ This PC", true),
            menu_row(REMOTE, "   box", true),
        ]
    );

    h.click_on(&row_selector(REMOTE));
    assert_eq!(watches(&h.host(&remote()).sent()), [true]);
    assert_eq!(watches(&h.sent()), [] as [bool; 0], "This PC hears nothing");
    assert_eq!(saved_ui(&dir)["perf_tracked"], json!(["local", REMOTE]));
    assert!(menu_open(&mut h), "the menu stays open");
    assert_eq!(
        menu_rows(&mut h),
        [
            menu_row("local", "✓ This PC", true),
            menu_row(REMOTE, "✓ box", true),
        ]
    );
}

#[gpui::test]
fn a_host_without_the_feature_is_a_disabled_row(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    this_pc_reports_perf(&mut h);
    h.host(&remote()).connect_with_features(&[]);
    h.sent();
    h.host(&remote()).sent();

    h.right_click_on("footer-perf-local");
    assert_eq!(
        menu_rows(&mut h),
        [
            menu_row("local", "✓ This PC", true),
            menu_row(REMOTE, "   box", false),
        ]
    );
    assert!(
        tooltip_on(&mut h, &row_selector(REMOTE), UNSUPPORTED_TIP),
        "the disabled row says why"
    );
    h.click_on(&row_selector(REMOTE));
    assert_eq!(watches(&h.host(&remote()).sent()), [] as [bool; 0]);
    assert_eq!(
        menu_rows(&mut h)[1],
        menu_row(REMOTE, "   box", false),
        "the click tracked nothing"
    );
}

#[gpui::test]
fn a_tracked_host_without_the_feature_can_still_be_untracked(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(&dir, &json!({ "perf_tracked": ["local", REMOTE] }));
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    this_pc_reports_perf(&mut h);
    h.host(&remote()).connect_with_features(&[]);
    h.host(&remote()).sent();

    h.right_click_on("footer-perf-local");
    assert_eq!(
        menu_rows(&mut h),
        [
            menu_row("local", "✓ This PC", true),
            menu_row(REMOTE, "✓ box", true),
        ]
    );
    assert!(
        tooltip_on(&mut h, &row_selector(REMOTE), UNSUPPORTED_TIP),
        "the row still says why it has no pill"
    );
    h.click_on(&row_selector(REMOTE));
    assert_eq!(saved_ui(&dir)["perf_tracked"], json!(["local"]));
    assert_eq!(watches(&h.host(&remote()).sent()), [] as [bool; 0]);
}

#[gpui::test]
fn an_offline_saved_host_is_an_enabled_row(cx: &mut TestAppContext) {
    const LAB: &str = "lab.tail1234.ts.net";
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({
            "saved_hosts": [
                { "name": REMOTE, "port": 8787 },
                { "name": LAB, "port": 8787 },
            ],
            "perf_tracked": ["local", LAB],
        }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    this_pc_reports_perf(&mut h);

    h.right_click_on("footer-perf-local");
    assert_eq!(
        menu_rows(&mut h),
        [
            menu_row("local", "✓ This PC", true),
            menu_row(REMOTE, "   box", true),
            menu_row(LAB, "✓ lab", true),
        ]
    );
    h.click_on(&row_selector(REMOTE));
    assert_eq!(
        saved_ui(&dir)["perf_tracked"],
        json!(["local", LAB, REMOTE])
    );
}

#[gpui::test]
fn forgetting_a_tracked_host_untracks_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({
            "saved_hosts": [{ "name": REMOTE, "port": 8787 }],
            "perf_tracked": ["local", REMOTE],
        }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    remote_reports_perf(&mut h);
    let keys =
        |h: &mut Harness<'_>| -> Vec<String> { pills(h).into_iter().map(|(key, _)| key).collect() };
    assert!(keys(&mut h).contains(&REMOTE.to_owned()), "tracked first");

    h.root_update(|root, _, _| root.set_peer_source(Arc::new(NoPeers)));
    h.click_on("footer-connections");
    h.click_on(&format!("connections-forget-{REMOTE}"));

    assert_eq!(saved_ui(&dir)["perf_tracked"], json!(["local"]));
    assert!(
        !keys(&mut h).contains(&REMOTE.to_owned()),
        "no pill outlives the host"
    );
}

#[gpui::test]
fn a_tracked_unknown_key_is_listed_so_it_can_be_untracked(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(&dir, &json!({ "perf_tracked": ["local", "gone.example"] }));
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    this_pc_reports_perf(&mut h);

    h.right_click_on("footer-perf-local");
    assert_eq!(
        menu_rows(&mut h),
        [
            menu_row("local", "✓ This PC", true),
            menu_row("gone.example", "✓ gone.example", true),
        ]
    );
    h.click_on(&row_selector("gone.example"));
    assert_eq!(saved_ui(&dir)["perf_tracked"], json!(["local"]));
}

#[gpui::test]
fn escape_and_an_outside_click_close_the_menu(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    this_pc_reports_perf(&mut h);

    h.right_click_on("footer-perf-local");
    assert!(menu_open(&mut h));
    h.click_on("perf-menu-backdrop");
    assert!(!menu_open(&mut h), "a click outside closes it");

    h.right_click_on("footer-perf-local");
    assert!(menu_open(&mut h), "it opens again");
    h.keys("escape");
    assert!(!menu_open(&mut h), "Esc closes it");
}
