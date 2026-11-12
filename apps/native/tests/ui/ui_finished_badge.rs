//! Finished badge specs: a turn that ends while another session is focused
//! counts on the rail's Sessions item, on its tab's blue pill and on the
//! taskbar overlay, until the session is focused or clicked; needs you
//! outranks it on the taskbar.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use std::time::Duration;

use gpui::{Modifiers, TestAppContext};
use protocol::{DaemonMessage, SplitDirection};
use rustling_tulip_native::{BadgeOverlay, TabBadge, ui_theme::theme};
use support::{Fixture, Harness, TestDir, pane, session, split, tab};

/// Tab `t1`, active: panes `p1`, `p2` and `p3` show the working sessions
/// `s1`, `s2` and `s3`.
fn three_panes() -> Fixture {
    let grid = split(
        SplitDirection::Horizontal,
        pane("p1", Some("s1")),
        split(
            SplitDirection::Vertical,
            pane("p2", Some("s2")),
            pane("p3", Some("s3")),
        ),
    );
    Fixture {
        sessions: ["s1", "s2", "s3"]
            .into_iter()
            .map(|id| session(id).status("working").build())
            .collect(),
        tabs: vec![tab("t1", &grid)],
        ..Fixture::default()
    }
}

/// Clicks pane `pane`, which focuses it, and lets the badge settle.
fn focus_pane(h: &mut Harness<'_>, pane: &str) {
    let at = h.cell_center(pane, 0, 0);
    h.click(at, Modifiers::none());
    settle(h);
}

/// Lets the taskbar badge's debounce run out and drains what went out.
fn settle(h: &mut Harness<'_>) {
    h.advance(Duration::from_millis(400));
    h.sent();
}

/// Sends `id`'s new status.
fn status(h: &mut Harness<'_>, id: &str, status: &str) {
    h.send(DaemonMessage::SessionUpdated {
        session: session(id).status(status).build(),
        request_id: None,
    });
}

/// The rail's Sessions badge as drawn.
fn rail(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.sessions_badge())
}

/// Tab `t1`'s finished pill.
fn pill(h: &mut Harness<'_>) -> Option<TabBadge> {
    h.root(|root, _| root.tab_finished_badge("t1"))
}

/// The overlay the taskbar button shows now.
fn overlay(h: &mut Harness<'_>) -> Option<BadgeOverlay> {
    h.taskbar_badges().pop().and_then(|(_, overlay)| overlay)
}

/// The palette's colour `pick` takes.
fn colour(h: &mut Harness<'_>, pick: fn(&rustling_tulip_native::ui_theme::UiTheme) -> u32) -> u32 {
    h.root(move |_, cx| pick(&theme(cx)))
}

/// The harness on [`three_panes`], every scrollback answered, the window
/// active and `p1`, showing `s1`, focused.
fn s1_focused<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = Harness::with(cx, dir, &three_panes());
    for id in ["s1", "s2", "s3"] {
        h.answer_scrollback(id, b"");
    }
    settle(&mut h);
    h.cx.update(|window, _| window.activate_window());
    focus_pane(&mut h, "p1");
    h
}

#[gpui::test]
fn a_turn_ending_in_the_focused_pane_of_an_inactive_window_counts_as_finished(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let mut h = s1_focused(cx, &dir);
    h.cx.deactivate_window();

    status(&mut h, "s1", "idle");
    settle(&mut h);
    let blue = colour(&mut h, |t| t.working);
    assert_eq!(
        rail(&mut h).as_deref(),
        Some("1"),
        "the owner was in another app"
    );
    let shown = overlay(&mut h).expect("a blue overlay for the finished turn");
    assert_eq!((shown.text.as_str(), shown.colour), ("1", blue));

    // A tab update re-binds the panes while the window is still inactive.
    let t1 = three_panes().tabs.remove(0);
    h.send(DaemonMessage::TabUpdated { tab: t1 });
    settle(&mut h);
    assert_eq!(
        rail(&mut h).as_deref(),
        Some("1"),
        "re-binding the panes of an inactive window sees nothing"
    );

    h.cx.update(|window, _| window.activate_window());
    settle(&mut h);
    assert_eq!(rail(&mut h), None, "coming back to the window saw s1");
    assert_eq!(overlay(&mut h), None);
}

#[gpui::test]
fn a_turn_ending_in_the_focused_pane_of_the_active_window_is_not_finished(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = s1_focused(cx, &dir);

    status(&mut h, "s1", "idle");
    settle(&mut h);
    assert_eq!(rail(&mut h), None, "the owner watched s1 finish");
    assert_eq!(pill(&mut h), None);
    assert_eq!(overlay(&mut h), None);
}

#[gpui::test]
fn finished_badge_counts_unseen_turns_and_clears_on_focus(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &three_panes());
    for id in ["s1", "s2", "s3"] {
        h.answer_scrollback(id, b"");
    }
    settle(&mut h);
    h.cx.update(|window, _| window.activate_window());
    focus_pane(&mut h, "p3");

    status(&mut h, "s1", "idle");
    status(&mut h, "s2", "idle");
    settle(&mut h);
    let blue = colour(&mut h, |t| t.working);
    assert_eq!(rail(&mut h).as_deref(), Some("2"), "two turns ended unseen");
    assert_eq!(pill(&mut h), Some(TabBadge::Finished(2)));
    let shown = overlay(&mut h).expect("a blue overlay for the finished turns");
    assert_eq!(
        (
            shown.text.as_str(),
            shown.description.as_str(),
            shown.colour
        ),
        ("2", "2 finished", blue)
    );

    focus_pane(&mut h, "p1");
    assert_eq!(rail(&mut h).as_deref(), Some("1"), "focus saw s1");
    assert_eq!(pill(&mut h), Some(TabBadge::Finished(1)));
    let shown = overlay(&mut h).expect("s2 is still unseen");
    assert_eq!((shown.text.as_str(), shown.colour), ("1", blue));

    h.click_on("leaf-s2");
    settle(&mut h);
    assert_eq!(rail(&mut h), None, "the click saw s2");
    assert_eq!(pill(&mut h), None);
    assert_eq!(overlay(&mut h), None, "nothing finished is left unseen");

    focus_pane(&mut h, "p3");
    status(&mut h, "s2", "working");
    status(&mut h, "s2", "idle");
    status(&mut h, "s1", "awaiting_input");
    settle(&mut h);
    assert_eq!(pill(&mut h), Some(TabBadge::Finished(1)), "s2 is unseen");
    let red = colour(&mut h, |t| t.danger);
    let shown = overlay(&mut h).expect("an overlay while s1 needs you");
    assert_eq!(
        (
            shown.text.as_str(),
            shown.description.as_str(),
            shown.colour
        ),
        ("1", "1 needs you", red),
        "needs you outranks finished"
    );
}
