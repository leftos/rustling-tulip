//! Presence report specs: the client tells each daemon that lists
//! `client_presence` whether the owner is at the desk (the window active and
//! input in the last 180 seconds), on connect and whenever the answer
//! changes, and re-reads it every 10 seconds.

use crate::support;

use std::time::Duration;

use gpui::TestAppContext;
use protocol::{ClientMessage, FEATURE_CLIENT_PRESENCE};
use rustling_tulip_native::set_idle_reader;
use support::{Fixture, Harness, TestDir, saved_remote};

/// The `active` of every `ReportPresence` in `sent`, oldest first.
fn reports(sent: &[ClientMessage]) -> Vec<bool> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::ReportPresence { active } => Some(*active),
            _ => None,
        })
        .collect()
}

/// This PC listing `client_presence` and the saved remote host listing
/// `remote_features`, what each was sent so far drained; the window is not
/// yet active.
fn two_hosts<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    remote_features: &[&str],
) -> Harness<'a> {
    let mut h = Harness::with(cx, dir, &Fixture::default());
    h.reconnect_with_features(&[FEATURE_CLIENT_PRESENCE]);
    h.sent();
    h.host(&saved_remote())
        .connect_with_features(remote_features);
    h
}

/// Activates the window and lets the activation land.
fn activate(h: &mut Harness<'_>) {
    h.cx.update(|window, _| window.activate_window());
    h.cx.run_until_parked();
}

#[gpui::test]
fn an_active_window_reports_presence_to_each_host_that_supports_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir, &[FEATURE_CLIENT_PRESENCE]);

    activate(&mut h);

    assert_eq!(reports(&h.sent()), vec![true], "This PC hears it");
    assert_eq!(
        reports(&h.host(&saved_remote()).sent()),
        vec![true],
        "the remote host hears it"
    );
}

#[gpui::test]
fn a_host_without_the_feature_is_sent_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir, &[]);

    activate(&mut h);

    assert_eq!(reports(&h.sent()), vec![true], "This PC lists the feature");
    assert_eq!(
        reports(&h.host(&saved_remote()).sent()),
        Vec::<bool>::new(),
        "the remote host lists no client_presence"
    );
}

#[gpui::test]
fn deactivating_the_window_reports_inactive(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir, &[FEATURE_CLIENT_PRESENCE]);
    activate(&mut h);
    h.sent();
    h.host(&saved_remote()).sent();

    h.cx.deactivate_window();
    h.cx.run_until_parked();

    assert_eq!(reports(&h.sent()), vec![false]);
    assert_eq!(reports(&h.host(&saved_remote()).sent()), vec![false]);
}

#[gpui::test]
fn idle_past_the_limit_reports_inactive_on_the_next_check(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir, &[FEATURE_CLIENT_PRESENCE]);
    activate(&mut h);
    h.sent();
    h.host(&saved_remote()).sent();

    h.cx.update(|_, cx| set_idle_reader(cx, || Some(Duration::from_secs(181))));
    h.advance(Duration::from_secs(10));

    assert_eq!(reports(&h.host(&saved_remote()).sent()), vec![false]);
}

#[gpui::test]
fn an_unchanged_answer_is_not_resent(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir, &[FEATURE_CLIENT_PRESENCE]);

    activate(&mut h);
    h.advance(Duration::from_secs(10));
    h.advance(Duration::from_secs(10));

    assert_eq!(
        reports(&h.host(&saved_remote()).sent()),
        vec![true],
        "one report across the activation and two checks"
    );
}

#[gpui::test]
fn a_host_that_reconnects_is_sent_the_current_answer(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir, &[FEATURE_CLIENT_PRESENCE]);
    activate(&mut h);
    h.sent();
    h.host(&saved_remote()).sent();

    let mut features = support::daemon_features();
    features.push(FEATURE_CLIENT_PRESENCE.to_owned());
    h.host(&saved_remote()).connect_keeping_sent_with(features);

    assert_eq!(reports(&h.host(&saved_remote()).sent()), vec![true]);
    assert_eq!(
        reports(&h.sent()),
        Vec::<bool>::new(),
        "only the host that said hello is sent it"
    );
}
