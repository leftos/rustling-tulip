//! The app's own toast specs: with `toast_style` set to `app`, a Needs you
//! or Finished toast is a window of the client's own, placed through the
//! recording placer, never an OS toast.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use gpui::{Modifiers, TestAppContext, VisualTestContext, point, px};
use protocol::{AttentionReason, DaemonMessage};
use rustling_tulip_native::{BundledSound, ClipSource, PhysicalRect};
use serde_json::{Value, json};
use std::time::Duration;
use support::{Fixture, Harness, TestDir, pane, remote_lists, saved_remote, seed_ui, session, tab};

fn needs_you(id: &str) -> DaemonMessage {
    DaemonMessage::Attention {
        session_id: id.to_owned(),
        reason: AttentionReason::AwaitingInput,
    }
}

/// Delivers `id`'s turn and waits out its grace, raising its Finished toast.
fn finish(h: &mut Harness<'_>, id: &str) {
    for status in ["working", "idle"] {
        h.send(DaemonMessage::SessionUpdated {
            session: session(id).status(status).build(),
            request_id: None,
        });
    }
    h.advance(Duration::from_secs(5));
}

/// `s1` in `t1`'s pane and each of `others` in a tab of its own.
fn sessions(others: &[&str]) -> Fixture {
    let mut fixture = Fixture::single(session("s1").build());
    for (index, id) in others.iter().enumerate() {
        fixture.sessions.push(session(id).build());
        let pane_id = format!("p-{index}");
        fixture
            .tabs
            .push(tab(&format!("t-{index}"), &pane(&pane_id, Some(id))));
    }
    fixture
}

/// The harness over `fixture` with `notifications` saved, its own toasts on.
fn opened<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    fixture: &Fixture,
    mut notifications: Value,
) -> Harness<'a> {
    notifications["toast_style"] = json!("app");
    seed_ui(dir, &json!({ "notifications": notifications }));
    let mut h = Harness::with(cx, dir, fixture);
    h.answer_scrollback("s1", b"");
    h.sent();
    h
}

/// Presses `selector` in `toast`'s window.
fn press(toast: &mut VisualTestContext, selector: &'static str) {
    toast.run_until_parked();
    let bounds = toast
        .debug_bounds(selector)
        .expect("the control is drawn in the toast");
    toast.simulate_click(bounds.center(), Modifiers::none());
    toast.run_until_parked();
}

#[gpui::test]
fn app_style_opens_a_toast_window_and_no_os_toast(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir, &sessions(&["s2"]), json!({}));

    h.send(needs_you("s2"));

    assert_eq!(h.window_count(), 2, "the toast has a window of its own");
    assert!(h.notified().is_empty(), "never the OS's toast beside it");
    assert_eq!(
        h.toasts_shown(),
        [PhysicalRect {
            x: 1544,
            y: 948,
            width: 360,
            height: 116,
        }],
        "in the work area's bottom-right corner, 16 px in"
    );
    let mut toast = h.popout_window();
    toast.run_until_parked();
    for part in ["toast-card", "toast-go-to-pane", "toast-dismiss"] {
        assert!(toast.debug_bounds(part).is_some(), "{part} is drawn");
    }
    assert!(
        !toast.update(|window, _| window.is_window_active()),
        "the toast never takes focus"
    );
}

#[gpui::test]
fn os_style_opens_no_toast_window(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &sessions(&["s2"]));
    h.answer_scrollback("s1", b"");

    h.send(needs_you("s2"));

    assert_eq!(h.window_count(), 1, "the OS draws this toast");
    assert_eq!(h.notified().len(), 1);
    assert_eq!(h.toasts_shown(), Vec::<PhysicalRect>::new());
}

#[gpui::test]
fn a_busy_user_gets_no_app_toast_and_no_sound(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir, &sessions(&["s2"]), json!({}));
    h.set_do_not_disturb(true);

    h.send(needs_you("s2"));

    assert_eq!(h.window_count(), 1, "no toast window opens");
    assert!(h.sounds().is_empty(), "and no sound plays");
    assert!(h.notified().is_empty(), "nor does the OS's toast stand in");
}

#[gpui::test]
fn the_app_toast_plays_the_chosen_sound(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let chosen = json!({
        "needs_you_sound": { "bundled": "bell_falling_fifth" },
        "finished_sound": "off",
    });
    let mut h = opened(cx, &dir, &sessions(&["s2", "s3"]), chosen);

    h.send(needs_you("s2"));
    finish(&mut h, "s3");

    assert_eq!(h.window_count(), 3, "both toasts are up");
    assert_eq!(
        h.sounds(),
        [ClipSource::Bundled(BundledSound::BellFallingFifth)],
        "Needs you plays its choice, and Finished's is off"
    );
}

#[gpui::test]
fn a_later_toast_for_a_session_replaces_it_in_place(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir, &sessions(&["s2"]), json!({}));
    h.send(needs_you("s2"));
    assert_eq!(
        h.root(|root, _| root.shown_toasts()),
        [("s2".to_owned(), "Needs you".to_owned())]
    );

    finish(&mut h, "s2");

    assert_eq!(h.window_count(), 2, "one window for the session");
    assert_eq!(h.toasts_shown().len(), 1, "that window was shown once");
    assert_eq!(
        h.root(|root, _| root.shown_toasts()),
        [("s2".to_owned(), "Finished".to_owned())],
        "it takes the later toast's words"
    );
}

#[gpui::test]
fn dismiss_closes_the_window(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir, &sessions(&["s2"]), json!({}));
    h.send(needs_you("s2"));
    let mut toast = h.popout_window();

    press(&mut toast, "toast-dismiss");

    assert_eq!(h.window_count(), 1, "the toast window closed");
    assert_eq!(
        h.root(|root, _| root.shown_toasts()),
        Vec::<(String, String)>::new()
    );
}

#[gpui::test]
fn go_to_pane_selects_the_session_and_closes_the_toast(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir, &sessions(&["s2"]), json!({}));
    h.send(needs_you("s2"));
    assert_ne!(
        h.root(|root, _| root.active_tab_id().map(str::to_owned))
            .as_deref(),
        Some("t-0"),
        "s2's tab starts behind"
    );
    let mut toast = h.popout_window();

    press(&mut toast, "toast-go-to-pane");

    assert_eq!(
        h.root(|root, _| root.active_tab_id().map(str::to_owned))
            .as_deref(),
        Some("t-0"),
        "the session's tab is on screen"
    );
    assert_eq!(h.window_count(), 1, "the toast window closed");
    assert!(
        h.cx.update(|window, _| window.is_window_active()),
        "the main window is in front"
    );
}

#[gpui::test]
fn a_toast_expires_after_its_time(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir, &sessions(&["s2"]), json!({}));
    finish(&mut h, "s2");
    assert_eq!(h.window_count(), 2, "the Finished toast is up");

    h.advance(Duration::from_millis(5999));
    assert_eq!(h.window_count(), 2, "still up just before six seconds");

    h.advance(Duration::from_millis(1));
    assert_eq!(h.window_count(), 1, "gone at six seconds");
    assert_eq!(
        h.root(|root, _| root.shown_toasts()),
        Vec::<(String, String)>::new()
    );
}

#[gpui::test]
fn hover_holds_the_countdown(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir, &sessions(&["s2"]), json!({}));
    finish(&mut h, "s2");
    let mut toast = h.popout_window();
    toast.run_until_parked();
    let card = toast
        .debug_bounds("toast-card")
        .expect("the card is drawn in the toast");

    toast.simulate_mouse_move(card.center(), None, Modifiers::none());
    h.advance(Duration::from_secs(60));
    assert_eq!(h.window_count(), 2, "held while the pointer rests on it");

    toast.simulate_mouse_move(point(px(2000.0), px(2000.0)), None, Modifiers::none());
    h.advance(Duration::from_millis(5999));
    assert_eq!(h.window_count(), 2, "it had its six seconds left");
    h.advance(Duration::from_millis(1));
    assert_eq!(h.window_count(), 1, "the countdown ran on from the leave");
}

#[gpui::test]
fn a_fifth_toast_drops_the_oldest_finished(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = sessions(&["s2", "s3", "s4", "s5", "s6"]);
    let mut h = opened(cx, &dir, &fixture, json!({}));
    h.send(needs_you("s2"));
    finish(&mut h, "s3");
    for id in ["s4", "s5"] {
        h.send(needs_you(id));
    }
    assert_eq!(h.window_count(), 5, "four toasts are up");

    h.send(needs_you("s6"));

    assert_eq!(
        h.window_count(),
        5,
        "the fifth takes the Finished one's place"
    );
    let up: Vec<String> = h
        .root(|root, _| root.shown_toasts())
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(up, ["s2", "s4", "s5", "s6"], "s3 went, not the older s2");
    assert!(
        !h.toasts_restacked().is_empty(),
        "the rest moved to close the gap"
    );
}

#[gpui::test]
fn a_remote_session_shows_its_host_in_the_title(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({
            "saved_hosts": [{ "name": support::REMOTE, "port": 8787 }],
            "notifications": { "toast_style": "app" },
        }),
    );
    let mut h = Harness::with(cx, &dir, &sessions(&[]));
    h.host(&saved_remote()).connect();
    remote_lists(&mut h, &["b1"]);

    h.host(&saved_remote()).send(needs_you("b1"));
    h.send(needs_you("s1"));

    assert_eq!(
        h.root(|root, _| root.shown_toasts()),
        [
            ("b1".to_owned(), "Needs you · box".to_owned()),
            ("s1".to_owned(), "Needs you".to_owned()),
        ],
        "only the other host's toast names it"
    );
}
