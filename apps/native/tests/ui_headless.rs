//! Headless-pane specs: a pane showing a headless session draws a stats bar
//! and the session's recent-actions log where a terminal would be, with the
//! tail cap and its show-all button, and no exited overlay when it stops.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

#[expect(dead_code, reason = "each spec file uses its own share of the helper")]
mod support;

use gpui::{TestAppContext, px};
use protocol::{DaemonMessage, SplitDirection};
use support::{Fixture, Harness, TestDir, pane, repo, session, split, tab};

/// Whether the element tagged `selector` has been painted on screen.
fn painted(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).origin.x >= px(0.0)
}

fn focused_pane(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.focused_pane())
}

/// What a headless pane's body draws, as the view model holds it.
struct Body {
    stats: Vec<String>,
    show_all: Option<String>,
    rows: Vec<(usize, String)>,
    empty_note: Option<&'static str>,
}

/// The pane's headless body as the view model holds it.
fn body_of(h: &mut Harness<'_>, pane: &str) -> Body {
    let body = h
        .root(|root, _| root.headless_bodies())
        .into_iter()
        .find(|body| body.pane_id == pane)
        .expect("the pane draws a headless body");
    Body {
        stats: body.stats.into_iter().map(|(_, value)| value).collect(),
        show_all: body.show_all,
        rows: body.rows,
        empty_note: body.empty_note,
    }
}

fn stats(h: &mut Harness<'_>, pane: &str) -> Vec<String> {
    body_of(h, pane).stats
}

fn show_all(h: &mut Harness<'_>, pane: &str) -> Option<String> {
    body_of(h, pane).show_all
}

fn rows(h: &mut Harness<'_>, pane: &str) -> Vec<(usize, String)> {
    body_of(h, pane).rows
}

fn empty_note(h: &mut Harness<'_>, pane: &str) -> Option<&'static str> {
    body_of(h, pane).empty_note
}

/// The handle the window has given the keyboard, as the app sees it.
fn window_focus(h: &mut Harness<'_>) -> Option<String> {
    h.cx.update(|window, cx| window.focused(cx).map(|handle| format!("{handle:?}")))
}

/// The action texts `a1`.. `an`, oldest first.
fn actions(n: usize) -> Vec<String> {
    (1..=n).map(|i| format!("a{i}")).collect()
}

/// `s1`, headless with the given metrics and status, alone in pane `p1`.
fn headless_fixture() -> Fixture {
    Fixture::single(
        session("s1")
            .headless()
            .status("awaiting_input")
            .metrics(1234, 56, 0.012_34)
            .build(),
    )
}

fn open_with<'a>(cx: &'a mut TestAppContext, dir: &TestDir, fixture: &Fixture) -> Harness<'a> {
    let mut h = Harness::with(cx, dir, fixture);
    h.sent();
    h
}

#[gpui::test]
fn headless_pane_shows_stats(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_with(cx, &dir, &headless_fixture());

    for selector in [
        "headless-p1",
        "headless-stat-status-p1",
        "headless-stat-in-p1",
        "headless-stat-out-p1",
        "headless-stat-cost-p1",
    ] {
        assert!(painted(&mut h, selector), "{selector} is not painted");
    }
    assert_eq!(
        stats(&mut h, "p1"),
        vec!["awaiting input", "1,234", "56", "$0.0123"]
    );
}

#[gpui::test]
fn headless_pane_shows_empty_log(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_with(cx, &dir, &headless_fixture());

    assert!(painted(&mut h, "headless-log-p1"));
    assert!(painted(&mut h, "headless-empty-p1"), "the note is drawn");
    assert_eq!(empty_note(&mut h, "p1"), Some("No events yet…"));
    assert!(rows(&mut h, "p1").is_empty());
    assert!(!painted(&mut h, "headless-show-all-p1"));
}

#[gpui::test]
fn headless_log_updates_on_session_updated(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_with(cx, &dir, &headless_fixture());

    h.send(DaemonMessage::SessionUpdated {
        session: session("s1")
            .headless()
            .status("awaiting_input")
            .metrics(1234, 56, 0.012_34)
            .recent_actions(&["Read lib.rs".to_owned(), "Ran the tests".to_owned()])
            .build(),
        request_id: None,
    });

    assert_eq!(
        rows(&mut h, "p1"),
        vec![
            (1, "Read lib.rs".to_owned()),
            (2, "Ran the tests".to_owned())
        ]
    );
    assert!(
        painted(&mut h, "headless-row-2-p1"),
        "the newest action is drawn"
    );
    assert_eq!(empty_note(&mut h, "p1"), None);
}

#[gpui::test]
fn headless_log_caps_at_200_and_show_all_reveals_rest(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture::single(
        session("s1")
            .headless()
            .status("working")
            .recent_actions(&actions(250))
            .build(),
    );
    let mut h = open_with(cx, &dir, &fixture);

    let capped = rows(&mut h, "p1");
    assert_eq!(capped.len(), 200);
    assert_eq!(capped.first(), Some(&(51, "a51".to_owned())));
    assert_eq!(capped.last(), Some(&(250, "a250".to_owned())));
    assert!(
        !capped.iter().any(|(_, text)| text == "a50"),
        "the earlier entries are held back"
    );
    assert_eq!(
        show_all(&mut h, "p1").as_deref(),
        Some("Show all 250 entries (earlier 50 hidden)")
    );
    assert!(painted(&mut h, "headless-show-all-p1"));
    assert!(
        painted(&mut h, "headless-row-51-p1"),
        "the oldest drawn row"
    );
    assert!(
        !painted(&mut h, "headless-row-50-p1"),
        "the row the cap hid is not drawn"
    );

    h.click_on("headless-show-all-p1");

    let full = rows(&mut h, "p1");
    assert_eq!(full.len(), 250);
    assert_eq!(full.first(), Some(&(1, "a1".to_owned())));
    assert_eq!(full.last(), Some(&(250, "a250".to_owned())));
    assert_eq!(show_all(&mut h, "p1"), None, "the button's work is done");
    assert!(
        painted(&mut h, "headless-row-1-p1"),
        "the first row now draws"
    );
    assert!(
        !h.in_model("headless-show-all-p1"),
        "and the button is gone; its painted bounds alone never prove that"
    );
    assert_eq!(
        h.bounds("headless-row-1-p1").origin.y,
        h.bounds("headless-show-all-p1").origin.y,
        "the first row takes the button's place in the log"
    );
}

#[gpui::test]
fn stopped_headless_pane_has_no_exited_overlay(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        sessions: vec![
            session("s1").headless().exited(0).build(),
            session("s2").exited(1).build(),
        ],
        tabs: vec![tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                pane("p1", Some("s1")),
                pane("p2", Some("s2")),
            ),
        )],
        ..Fixture::default()
    };
    let mut h = open_with(cx, &dir, &fixture);

    assert!(
        !h.in_model("exited-p1"),
        "a headless pane has no exited overlay"
    );
    assert!(!painted(&mut h, "exited-restart-p1"));
    assert!(painted(&mut h, "headless-stat-status-p1"));
    assert_eq!(stats(&mut h, "p1")[0], "stopped");

    assert!(h.in_model("exited-p2"), "an interactive pane still does");
    assert!(painted(&mut h, "exited-restart-p2"));
}

#[gpui::test]
fn clicking_headless_body_focuses_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        sessions: vec![session("s1").build(), session("s2").headless().build()],
        tabs: vec![tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                pane("p1", Some("s1")),
                pane("p2", Some("s2")),
            ),
        )],
        ..Fixture::default()
    };
    let mut h = open_with(cx, &dir, &fixture);
    // gpui delivers no focus event to an inactive window, and the spec's
    // window starts inactive: activate it, as the desktop does, or the
    // pane's focus could never follow the click. A frame must pass for the
    // activation, and again for the focus, to land.
    h.cx.update(|window, _| window.activate_window());
    let _ = h.bounds("headless-p2");
    assert_eq!(focused_pane(&mut h).as_deref(), Some("p1"));
    assert!(
        h.cx.update(|window, _| window.is_window_active()),
        "the window is active"
    );

    let terminal = window_focus(&mut h);
    h.click_on("headless-p2");
    let _ = h.bounds("headless-p2");

    assert_eq!(focused_pane(&mut h).as_deref(), Some("p2"));
    assert_ne!(window_focus(&mut h), terminal, "the window focus moved");
}

#[gpui::test]
fn ctrl_n_works_after_clicking_headless_body(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        repos: vec![repo("r1", "C:/r1")],
        sessions: vec![session("s1").build(), session("s2").headless().build()],
        tabs: vec![tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                pane("p1", Some("s1")),
                pane("p2", Some("s2")),
            ),
        )],
        ..Fixture::default()
    };
    let mut h = open_with(cx, &dir, &fixture);

    h.click_on("headless-p2");

    h.keys("ctrl-n");

    assert!(
        h.root(|root, _| root.spawn_dialog_open()),
        "the headless pane's focus does not eat the app's shortcut"
    );
}
