//! Resume report specs: when the daemon's start-up resume ends, the client
//! says how many sessions came back and how many stay for Recover, once, in
//! one toast.

use crate::support;

use gpui::TestAppContext;
use protocol::DaemonMessage;
use support::{Harness, TestDir};

/// Every toast's title and detail, oldest first.
fn toasts(h: &mut Harness<'_>) -> Vec<(String, Option<String>)> {
    h.root(|root, _| {
        root.toasts()
            .iter()
            .map(|toast| (toast.title.clone(), toast.detail.clone()))
            .collect()
    })
}

/// The window opened on the fake daemon.
fn opened<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = Harness::open(cx, dir);
    h.sent();
    h
}

/// The daemon's report that `resumed` sessions came back and
/// `left_in_recover` stayed for Recover.
fn report(h: &mut Harness<'_>, resumed: u32, left_in_recover: u32) {
    h.send(DaemonMessage::ResumeReport {
        resumed,
        left_in_recover,
    });
}

#[gpui::test]
fn toast_names_resumed_and_left_counts(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);

    report(&mut h, 2, 1);

    assert_eq!(
        toasts(&mut h),
        [(
            "Resumed 2 sessions after restart".to_owned(),
            Some("1 stays in Recover".to_owned())
        )]
    );
}

#[gpui::test]
fn toast_says_session_singular_for_one(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);

    report(&mut h, 1, 0);

    assert_eq!(
        toasts(&mut h),
        [("Resumed 1 session after restart".to_owned(), None)],
        "one session, and nothing left behind"
    );
}

#[gpui::test]
fn nothing_resumed_toast_points_to_recover(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);

    report(&mut h, 0, 3);

    assert_eq!(
        toasts(&mut h),
        [(
            "No sessions resumed after restart".to_owned(),
            Some("3 stay in Recover".to_owned())
        )],
        "the toast says where they are"
    );
}

#[gpui::test]
fn a_second_report_updates_the_toast_in_place(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);

    report(&mut h, 2, 1);
    report(&mut h, 0, 2);

    assert_eq!(
        toasts(&mut h),
        [(
            "No sessions resumed after restart".to_owned(),
            Some("2 stay in Recover".to_owned())
        )],
        "the report's key replaces the first toast instead of stacking one"
    );
}
