//! Dashboard specs: which sessions it lists, the four groups and their order
//! over scripted snapshots (the Answer rule included), and the header counts,
//! busy-excluded sessions left out of the Working count.

use crate::support;

use chrono::{Duration, Utc};
use gpui::TestAppContext;
use protocol::DaemonMessage;
use rustling_tulip_native::DashboardEntry;
use support::{Fixture, Harness, TestDir, repo, session};

/// Each group's heading and its session ids, in order.
fn groups(h: &mut Harness<'_>) -> Vec<(&'static str, Vec<String>)> {
    h.root(|root, _| root.dashboard_groups())
        .into_iter()
        .map(|(heading, rows)| {
            (
                heading,
                rows.into_iter().map(|row| row.session_id).collect(),
            )
        })
        .collect()
}

/// The rows of the group headed `heading`.
fn rows_of(h: &mut Harness<'_>, heading: &str) -> Vec<DashboardEntry> {
    h.root(|root, _| root.dashboard_groups())
        .into_iter()
        .find(|(name, _)| *name == heading)
        .map(|(_, rows)| rows)
        .expect("every group is listed, empty or not")
}

fn counts(h: &mut Harness<'_>) -> [usize; 4] {
    h.root(|root, _| root.dashboard_counts())
}

fn update(h: &mut Harness<'_>, session: protocol::SessionSnapshot) {
    h.send(DaemonMessage::SessionUpdated {
        session,
        request_id: None,
    });
}

#[gpui::test]
fn groups_scripted_snapshots_in_order(cx: &mut TestAppContext) {
    let now = Utc::now();
    let ago = |minutes: i64| now - Duration::minutes(minutes);
    let fixture = Fixture {
        repos: vec![repo("r1", "D:/src/r1")],
        sessions: vec![
            session("ask-new")
                .in_repo("r1")
                .status("awaiting_input")
                .status_since(ago(1))
                .build(),
            session("ask-old")
                .in_repo("r1")
                .status("awaiting_input")
                .status_since(ago(5))
                .build(),
            session("answer")
                .in_repo("r1")
                .status_since(ago(3))
                .summary("needs_answer", ago(2))
                .build(),
            session("stale")
                .in_repo("r1")
                .status_since(ago(1))
                .summary("needs_answer", ago(2))
                .build(),
            session("unstamped")
                .in_repo("r1")
                .summary("needs_answer", ago(1))
                .build(),
            session("work")
                .in_repo("r1")
                .status("working")
                .status_since(ago(10))
                .build(),
            session("spawn").in_repo("r1").status("spawning").build(),
            session("bg")
                .in_repo("r1")
                .background()
                .status_since(ago(0))
                .build(),
            session("stopped")
                .in_repo("r1")
                .exited(0)
                .status_since(ago(20))
                .build(),
            session("errored")
                .in_repo("r1")
                .status("error")
                .status_since(ago(40))
                .build(),
        ],
        ..Fixture::default()
    };
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture);

    let ids = |list: &[&str]| list.iter().map(|id| (*id).to_owned()).collect::<Vec<_>>();
    assert_eq!(
        groups(&mut h),
        [
            ("Needs you", ids(&["ask-old", "answer", "ask-new"])),
            ("Working", ids(&["bg", "work", "spawn"])),
            ("Idle", ids(&["stale", "unstamped"])),
            ("Done", ids(&["stopped", "errored"])),
        ],
        "longest wait first in Needs you, latest activity first elsewhere; a stale or \
         unstamped answer summary stays Idle, spawning is Working, ended is Done"
    );
    let answer = &rows_of(&mut h, "Needs you")[1];
    assert_eq!(answer.container, "r1");
    assert_eq!(answer.label, "answer");
    assert_eq!(counts(&mut h), [3, 3, 2, 2]);
}

#[gpui::test]
fn busy_excluded_session_is_left_out_of_the_working_count(cx: &mut TestAppContext) {
    let fixture = Fixture {
        repos: vec![repo("r1", "D:/src/r1")],
        sessions: vec![
            session("s1")
                .in_repo("r1")
                .status("working")
                .busy_excluded()
                .build(),
            session("s2").in_repo("r1").status("working").build(),
            session("s3")
                .in_repo("r1")
                .background()
                .busy_excluded()
                .build(),
            session("s4")
                .in_repo("r1")
                .status("awaiting_input")
                .busy_excluded()
                .build(),
        ],
        ..Fixture::default()
    };
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture);

    let mut working: Vec<String> = rows_of(&mut h, "Working")
        .into_iter()
        .map(|row| row.session_id)
        .collect();
    working.sort();
    assert_eq!(
        working,
        ["s1", "s2", "s3"],
        "excluded sessions are still listed"
    );
    assert_eq!(
        counts(&mut h),
        [1, 1, 0, 0],
        "only s2 counts as working; the excluded asking session still needs you"
    );
}

#[gpui::test]
fn a_session_update_moves_a_session_between_groups(cx: &mut TestAppContext) {
    let since = Utc::now() - Duration::minutes(2);
    let idle = || session("s1").in_repo("r1").status_since(since);
    let fixture = Fixture {
        repos: vec![repo("r1", "D:/src/r1")],
        sessions: vec![idle().build()],
        ..Fixture::default()
    };
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture);
    assert_eq!(counts(&mut h), [0, 0, 1, 0]);

    update(&mut h, idle().summary("needs_answer", since).build());
    assert_eq!(counts(&mut h), [1, 0, 0, 0], "the Answer rule raises it");

    update(&mut h, idle().summary("done_waiting", since).build());
    assert_eq!(
        counts(&mut h),
        [0, 0, 1, 0],
        "a done_waiting summary is not a question"
    );

    update(
        &mut h,
        session("s1")
            .in_repo("r1")
            .status("error")
            .status_since(Utc::now())
            .build(),
    );
    assert_eq!(
        counts(&mut h),
        [0, 0, 0, 1],
        "an ended session is Done, not a wait"
    );
}
