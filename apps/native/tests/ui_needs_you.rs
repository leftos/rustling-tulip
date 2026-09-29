//! Needs You specs: the rail item and its badge, the panel's rows in wait
//! order, a click that shows the session, a right-click that opens its menu,
//! ended rows, the empty panel, the repaint cycle and the persisted choice.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

#[expect(dead_code, reason = "each spec file uses its own share of the helper")]
mod support;

use chrono::{Duration, Utc};
use gpui::{Modifiers, TestAppContext, point, px};
use protocol::{AttentionReason, DaemonMessage};
use rustling_tulip_native::{Activity, NeedsYouEntry};
use serde_json::json;
use support::{Fixture, Harness, TestDir, repo, session};

fn activity(h: &mut Harness<'_>) -> Activity {
    h.root(|root, _| root.activity())
}

fn collapsed(h: &mut Harness<'_>) -> bool {
    h.root(|root, _| root.sidebar_collapsed())
}

fn entries(h: &mut Harness<'_>) -> Vec<NeedsYouEntry> {
    h.root(|root, _| root.needs_you_rows())
}

fn ids(h: &mut Harness<'_>) -> Vec<String> {
    entries(h).into_iter().map(|row| row.session_id).collect()
}

fn header(h: &mut Harness<'_>) -> String {
    h.root(|root, _| root.needs_you_header())
}

fn badge(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.needs_you_badge())
}

/// How many times the panel's timer has repainted it.
fn repaints(h: &mut Harness<'_>) -> usize {
    h.root(|root, _| root.needs_you_repaints())
}

/// The waited text of `id`'s row, as the panel draws it.
fn waited_of(h: &mut Harness<'_>, id: &str) -> Option<String> {
    entries(h)
        .into_iter()
        .find(|row| row.session_id == id)
        .map(|row| row.waited)
}

/// The session whose context menu is open.
fn menu_of(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.session_menu().map(str::to_owned))
}

/// A session that has just started asking.
fn just_asking(id: &str) -> protocol::SessionSnapshot {
    session(id)
        .in_repo("r1")
        .status("awaiting_input")
        .status_since(Utc::now())
        .build()
}

fn saved_ui(dir: &TestDir) -> serde_json::Value {
    serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("native-ui.json")).expect("native-ui.json"),
    )
    .expect("native-ui.json is JSON")
}

/// `s1` waiting for 5 minutes, `s2` for 1, `s3` working, all in `r1`, none
/// shown in a pane.
fn waiting() -> Fixture {
    let now = Utc::now();
    Fixture {
        repos: vec![repo("r1", "D:/src/r1")],
        sessions: vec![
            session("s1")
                .in_repo("r1")
                .status("awaiting_input")
                .status_since(now - Duration::minutes(5))
                .build(),
            session("s3").in_repo("r1").status("working").build(),
            session("s2")
                .in_repo("r1")
                .status("awaiting_input")
                .status_since(now - Duration::minutes(1))
                .build(),
        ],
        ..Fixture::default()
    }
}

#[gpui::test]
fn lists_waiting_sessions_oldest_first(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &waiting());
    assert!(!h.in_model("needs-you-panel"), "Sessions is the default");

    h.click_on("activity-needs-you");
    assert_eq!(activity(&mut h), Activity::NeedsYou);
    assert!(h.in_model("needs-you-panel"));
    assert!(!h.in_model("sidebar-panel"), "the sessions panel gave way");
    let rows = entries(&mut h);
    let listed: Vec<&str> = rows.iter().map(|row| row.session_id.as_str()).collect();
    assert_eq!(
        listed,
        ["s1", "s2"],
        "the longer wait first; working is not listed"
    );
    assert_eq!(rows[0].container, "r1");
    assert_eq!(rows[0].label, "s1");
    assert_eq!(rows[0].detail, "Waiting for input");
    assert_eq!(rows[0].waited, "5m");
    assert_eq!(rows[1].waited, "1m");
    assert!(!rows[0].ended);
    assert_eq!(header(&mut h), "NEEDS YOU · 2");
    assert!(h.in_model("needs-you-header"));
    assert!(h.in_model("needs-you-row-s1"));
    assert!(!h.in_model("needs-you-row-s3"));
    assert!(!h.in_model("needs-you-empty"));
    let rail = h.bounds("activity-rail");
    assert_eq!(h.bounds("needs-you-panel").origin.x, rail.right());
}

#[gpui::test]
fn badge_counts_rows_while_folded_and_on_sessions(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &waiting());
    assert_eq!(activity(&mut h), Activity::Sessions);
    assert_eq!(badge(&mut h).as_deref(), Some("2"));
    assert!(h.in_model("needs-you-badge"));
    assert!(h.bounds("needs-you-badge").size.height > px(0.0));
    assert!(!h.in_model("needs-you-panel"));

    let panel = h.bounds("sidebar-panel");
    h.click(
        point(panel.center().x, panel.bottom() - px(10.0)),
        Modifiers::none(),
    );
    h.keys("ctrl-b");
    assert!(collapsed(&mut h), "folded");
    assert_eq!(badge(&mut h).as_deref(), Some("2"), "the rail still counts");
    assert!(h.in_model("needs-you-badge"));

    h.send(DaemonMessage::SessionUpdated {
        session: session("s1").in_repo("r1").status("working").build(),
        request_id: None,
    });
    h.send(DaemonMessage::SessionUpdated {
        session: session("s2").in_repo("r1").status("idle").build(),
        request_id: None,
    });
    assert_eq!(badge(&mut h), None, "hidden at 0");
    assert!(!h.in_model("needs-you-badge"));
}

#[gpui::test]
fn click_focuses_the_pane_and_the_row_stays(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(
        session("s1")
            .in_repo("r1")
            .status("awaiting_input")
            .status_since(Utc::now() - Duration::seconds(30))
            .build(),
    );
    fixture.repos = vec![repo("r1", "D:/src/r1")];
    let mut h = Harness::with(cx, &dir, &fixture);
    h.answer_scrollback("s1", b"");
    h.click_on("activity-needs-you");
    let panel = h.bounds("needs-you-panel");
    h.click(
        point(panel.center().x, panel.bottom() - px(10.0)),
        Modifiers::none(),
    );
    h.keys("a");
    assert!(
        h.sent_input("s1").is_empty(),
        "the panel holds the keyboard"
    );

    h.click_on("needs-you-row-s1");
    h.keys("b");
    assert_eq!(
        h.sent_input("s1"),
        b"b",
        "the session's pane took the keyboard"
    );
    assert_eq!(activity(&mut h), Activity::NeedsYou, "the panel stays");
    assert!(!collapsed(&mut h));
    assert_eq!(
        ids(&mut h),
        ["s1"],
        "a waiting row stays until it is answered"
    );
    assert!(h.in_model("needs-you-row-s1"));
}

#[gpui::test]
fn error_attention_adds_an_ended_row_that_a_click_removes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &waiting());
    h.click_on("activity-needs-you");

    h.send(DaemonMessage::SessionUpdated {
        session: session("s3")
            .in_repo("r1")
            .status("error")
            .status_since(Utc::now() - Duration::minutes(3))
            .build(),
        request_id: None,
    });
    h.send(DaemonMessage::Attention {
        session_id: "s3".to_owned(),
        reason: AttentionReason::Error,
    });
    let rows = entries(&mut h);
    let listed: Vec<(&str, bool)> = rows
        .iter()
        .map(|row| (row.session_id.as_str(), row.ended))
        .collect();
    assert_eq!(
        listed,
        [("s1", false), ("s2", false), ("s3", true)],
        "the ended row follows the waiting ones"
    );
    assert_eq!(rows[2].detail, "Error");
    assert_eq!(
        rows[2].waited, "3m",
        "an ended row counts the time since it ended"
    );
    assert_eq!(header(&mut h), "NEEDS YOU · 3");
    assert_eq!(badge(&mut h).as_deref(), Some("3"));

    h.click_on("needs-you-row-s3");
    assert_eq!(ids(&mut h), ["s1", "s2"], "the click dismissed it");
    assert!(!h.in_model("needs-you-row-s3"));
    assert_eq!(activity(&mut h), Activity::NeedsYou);
}

#[gpui::test]
fn working_update_drops_a_row(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &waiting());
    h.click_on("activity-needs-you");
    assert!(h.in_model("needs-you-row-s1"));

    h.send(DaemonMessage::SessionUpdated {
        session: session("s1").in_repo("r1").status("working").build(),
        request_id: None,
    });
    assert_eq!(ids(&mut h), ["s2"]);
    assert!(!h.in_model("needs-you-row-s1"));
    assert_eq!(header(&mut h), "NEEDS YOU · 1");
}

#[gpui::test]
fn empty_panel_shows_the_empty_text(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        repos: vec![repo("r1", "D:/src/r1")],
        sessions: vec![session("s1").in_repo("r1").build()],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.click_on("activity-needs-you");
    assert!(entries(&mut h).is_empty());
    assert!(h.in_model("needs-you-empty"));
    assert!(h.bounds("needs-you-empty").size.height > px(0.0));
    assert_eq!(header(&mut h), "NEEDS YOU · 0");
    assert_eq!(badge(&mut h), None);
    assert!(!h.in_model("needs-you-badge"));
    assert!(!h.in_model("needs-you-row-s1"));
}

#[gpui::test]
fn activity_persists_and_unknown_loads_as_sessions(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = waiting();
    let mut h = Harness::with(cx, &dir, &fixture);
    h.click_on("activity-needs-you");
    assert_eq!(saved_ui(&dir)["activity"], json!("needs_you"));
    drop(h);

    let mut h = Harness::with(cx, &dir, &fixture);
    assert_eq!(activity(&mut h), Activity::NeedsYou, "restored");
    assert!(h.in_model("needs-you-panel"));
    assert!(h.in_model("needs-you-row-s1"));
    drop(h);

    std::fs::write(
        dir.path().join("native-ui.json"),
        r#"{ "activity": "bogus", "sidebar_width": 333.0 }"#,
    )
    .expect("write native-ui.json");
    let mut h = Harness::with(cx, &dir, &fixture);
    assert_eq!(activity(&mut h), Activity::Sessions, "an unknown panel");
    assert!(h.in_model("sidebar-panel"));
    assert_eq!(
        h.bounds("sidebar-panel").size.width,
        px(333.0),
        "the rest of the file still loads"
    );
}

#[gpui::test]
fn a_fresh_row_rearms_the_repaint_to_one_second(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &waiting());
    h.click_on("activity-needs-you");

    // Every listed wait is over a minute old, so the panel is on its slow
    // cycle: nothing for 29 s, then one repaint at 30.
    h.advance(std::time::Duration::from_secs(29));
    assert_eq!(repaints(&mut h), 0, "the 30 s cycle has not come round");
    h.advance(std::time::Duration::from_secs(1));
    assert_eq!(repaints(&mut h), 1, "the 30 s cycle came round");

    h.send(DaemonMessage::SessionUpdated {
        session: just_asking("s4"),
        request_id: None,
    });
    assert_eq!(
        waited_of(&mut h, "s4").as_deref(),
        Some("0s"),
        "the row has just arrived"
    );

    // The slow cycle's next repaint is 30 s off; a repaint a second from now
    // can only come from the re-armed timer.
    h.advance(std::time::Duration::from_secs(1));
    assert_eq!(
        repaints(&mut h),
        2,
        "the fresh row moved the panel to its 1 s cycle"
    );
    h.advance(std::time::Duration::from_secs(1));
    assert_eq!(repaints(&mut h), 3, "and it keeps counting each second");
}

#[gpui::test]
fn waits_over_a_minute_repaint_every_thirty_seconds(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &waiting());
    h.click_on("activity-needs-you");

    h.advance(std::time::Duration::from_secs(29));
    assert_eq!(repaints(&mut h), 0, "no repaint before 30 s");
    h.advance(std::time::Duration::from_secs(1));
    assert_eq!(repaints(&mut h), 1, "one repaint at 30 s");
    h.advance(std::time::Duration::from_secs(30));
    assert_eq!(repaints(&mut h), 2, "and another 30 s later");
}

#[gpui::test]
fn an_empty_panel_does_not_repaint_until_a_row_arrives(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        repos: vec![repo("r1", "D:/src/r1")],
        sessions: vec![session("s1").in_repo("r1").build()],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.click_on("activity-needs-you");
    assert!(entries(&mut h).is_empty());

    h.advance(std::time::Duration::from_secs(60));
    assert_eq!(repaints(&mut h), 0, "an empty panel has nothing to count");

    h.send(DaemonMessage::SessionUpdated {
        session: just_asking("s4"),
        request_id: None,
    });
    h.advance(std::time::Duration::from_secs(1));
    assert_eq!(
        repaints(&mut h),
        1,
        "the arriving row started the panel's 1 s cycle"
    );
}

#[gpui::test]
fn a_hidden_panel_does_not_repaint(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &waiting());
    h.click_on("activity-needs-you");
    h.click_on("activity-sessions");
    assert_eq!(activity(&mut h), Activity::Sessions);

    h.send(DaemonMessage::SessionUpdated {
        session: just_asking("s4"),
        request_id: None,
    });
    h.advance(std::time::Duration::from_secs(2));
    assert_eq!(repaints(&mut h), 0, "no timer while Sessions shows");

    h.click_on("activity-needs-you");
    let panel = h.bounds("needs-you-panel");
    h.click(
        point(panel.center().x, panel.bottom() - px(10.0)),
        Modifiers::none(),
    );
    h.keys("ctrl-b");
    assert!(collapsed(&mut h), "folded");
    h.send(DaemonMessage::SessionUpdated {
        session: just_asking("s5"),
        request_id: None,
    });
    h.advance(std::time::Duration::from_secs(2));
    assert_eq!(repaints(&mut h), 0, "no timer while the panel is folded");
}

#[gpui::test]
fn right_click_on_a_row_opens_the_session_menu(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &waiting());
    h.click_on("activity-needs-you");
    h.sent();

    h.right_click_on("needs-you-row-s1");
    assert_eq!(
        menu_of(&mut h).as_deref(),
        Some("s1"),
        "the row opens its session's menu"
    );
    assert!(h.in_model("menu-rename"));
    assert!(h.in_model("menu-stop"));
    assert!(
        !h.in_model("menu-resume"),
        "a running session offers no Resume"
    );
    assert!(h.sent().is_empty(), "opening the menu sends nothing");

    h.keys("escape");
    assert_eq!(menu_of(&mut h), None, "Escape closes it");
    assert_eq!(activity(&mut h), Activity::NeedsYou, "the panel stays");
}
