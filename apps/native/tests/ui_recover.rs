//! Recover dialog specs: the client asks for the session history on
//! connect, the rail badge counts the sessions lost and not yet recovered,
//! the dialog groups the history with the losses ticked, one request
//! recovers the ticked rows as chosen, failures stay listed, and the
//! recovered sessions are placed like spawns.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

#[expect(dead_code, reason = "each spec file uses its own share of the helper")]
mod support;

use std::collections::HashSet;

use gpui::TestAppContext;
use protocol::{
    ClientMessage, DaemonMessage, RecoverAs, RecoverItem, RecoverItemResult, SessionHistoryItem,
    SplitDirection,
};
use rustling_tulip_native::RecoverRow;
use serde_json::json;
use support::{Fixture, Harness, PROTOCOL, TestDir, history_item, pane, repo, session, split, tab};

fn opened<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = Harness::open(cx, dir);
    h.sent();
    h
}

fn history(h: &mut Harness<'_>, items: Vec<SessionHistoryItem>) {
    h.send(DaemonMessage::SessionHistory {
        request_id: None,
        items,
    });
}

/// The history `items`, then the dialog opened from the rail.
fn open_with(h: &mut Harness<'_>, items: Vec<SessionHistoryItem>) {
    history(h, items);
    h.click_on("activity-recover");
    assert!(h.root(|root, _| root.recover_open()), "the dialog opened");
}

fn rows(h: &mut Harness<'_>) -> Vec<RecoverRow> {
    h.root(|root, _| root.recover_rows())
}

fn row(h: &mut Harness<'_>, id: &str) -> RecoverRow {
    rows(h)
        .into_iter()
        .find(|row| row.id == id)
        .expect("the row is shown")
}

fn row_ids(h: &mut Harness<'_>) -> Vec<String> {
    rows(h).into_iter().map(|row| row.id).collect()
}

fn ticked(h: &mut Harness<'_>) -> Vec<String> {
    rows(h)
        .into_iter()
        .filter(|row| row.ticked)
        .map(|row| row.id)
        .collect()
}

fn button(h: &mut Harness<'_>) -> (String, bool) {
    h.root(|root, _| root.recover_button())
        .expect("the dialog is open")
}

fn badge(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.recover_badge())
}

fn focus(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.recover_focus())
}

fn is_open(h: &mut Harness<'_>) -> bool {
    h.root(|root, _| root.recover_open())
}

/// The one `RecoverSessions` sent since the last read: its id and items.
fn recover_request(h: &mut Harness<'_>) -> (String, Vec<RecoverItem>) {
    let mut requests: Vec<(String, Vec<RecoverItem>)> = h
        .sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::RecoverSessions { request_id, items } => {
                Some((request_id.expect("a request id"), items))
            }
            _ => None,
        })
        .collect();
    assert_eq!(requests.len(), 1, "one recover request");
    requests.remove(0)
}

fn ok(history_id: &str, session_id: &str) -> RecoverItemResult {
    RecoverItemResult {
        history_id: history_id.to_owned(),
        session_id: Some(session_id.to_owned()),
        error: None,
    }
}

fn failed(history_id: &str, error: &str) -> RecoverItemResult {
    RecoverItemResult {
        history_id: history_id.to_owned(),
        session_id: None,
        error: Some(error.to_owned()),
    }
}

fn answer(h: &mut Harness<'_>, request_id: &str, results: Vec<RecoverItemResult>) {
    h.send(DaemonMessage::RecoverResult {
        request_id: Some(request_id.to_owned()),
        results,
    });
}

/// The messages of `sent` that put a session in a pane or a tab, each as
/// "<how> <pane> <session>".
fn placements(sent: Vec<ClientMessage>) -> Vec<String> {
    sent.into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::ReplacePaneSession {
                pane_id,
                session_id,
                ..
            } => Some(format!(
                "replace {pane_id} {}",
                session_id.unwrap_or_default()
            )),
            ClientMessage::SplitPane {
                pane_id,
                new_session_id,
                ..
            } => Some(format!(
                "split {pane_id} {}",
                new_session_id.unwrap_or_default()
            )),
            ClientMessage::CreateTab {
                initial_session_id, ..
            } => Some(format!(
                "new-tab {}",
                initial_session_id.unwrap_or_default()
            )),
            _ => None,
        })
        .collect()
}

fn welcome(h: &mut Harness<'_>) {
    h.send(DaemonMessage::Welcome {
        protocol_version: PROTOCOL,
        supported_versions: vec![PROTOCOL],
    });
}

fn asks_history(sent: &[ClientMessage]) -> bool {
    sent.iter().any(|msg| {
        matches!(
            msg,
            ClientMessage::ListSessionHistory {
                request_id: Some(_)
            }
        )
    })
}

#[gpui::test]
fn welcome_requests_session_history(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    welcome(&mut h);
    assert!(
        asks_history(&h.sent()),
        "each connection asks for the history"
    );
}

#[gpui::test]
fn badge_counts_unrecovered_lost_sessions(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    assert_eq!(badge(&mut h), None);
    assert!(!h.in_model("recover-badge"));
    history(
        &mut h,
        vec![
            history_item("a").build(),
            history_item("b").build(),
            history_item("c").recovered().build(),
            history_item("d").stopped().build(),
            history_item("e")
                .end(json!({"type": "exited", "code": 1}))
                .build(),
        ],
    );
    assert_eq!(
        badge(&mut h).as_deref(),
        Some("2"),
        "the unrecovered losses"
    );
    let at = h.center("recover-badge");
    let rail = h.bounds("activity-recover");
    assert!(rail.contains(&at), "the badge sits on the rail button");
}

#[gpui::test]
fn badge_updates_on_broadcast_and_clears_when_recovered(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    history(&mut h, vec![history_item("a").build()]);
    assert_eq!(badge(&mut h).as_deref(), Some("1"));
    history(
        &mut h,
        vec![history_item("a").build(), history_item("b").build()],
    );
    assert_eq!(
        badge(&mut h).as_deref(),
        Some("2"),
        "a broadcast replaces it"
    );
    history(
        &mut h,
        vec![
            history_item("a").recovered().build(),
            history_item("b").recovered().build(),
        ],
    );
    assert_eq!(badge(&mut h), None);
    assert!(!h.in_model("recover-badge"));
}

#[gpui::test]
fn dialog_groups_rows_and_preticks_lost(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    open_with(
        &mut h,
        vec![
            history_item("a").ended_minutes_ago(10).build(),
            history_item("b").ended_minutes_ago(11).build(),
            history_item("c").ended_minutes_ago(30).build(),
            history_item("s").stopped().ended_minutes_ago(5).build(),
        ],
    );
    let groups = h.root(|root, _| root.recover_groups());
    let titles: Vec<&str> = groups.iter().map(|(title, _)| title.as_str()).collect();
    assert_eq!(titles.len(), 3, "{titles:?}");
    assert!(titles[0].starts_with("2 sessions lost at "), "{titles:?}");
    assert!(titles[1].starts_with("1 session lost at "), "{titles:?}");
    assert_eq!(titles[2], "Other recent sessions");
    assert_eq!(groups[0].1, ["a", "b"]);
    assert_eq!(groups[1].1, ["c"]);
    assert!(groups[2].1.is_empty(), "the other sessions start collapsed");
    assert_eq!(row_ids(&mut h), ["a", "b", "c"]);
    assert_eq!(ticked(&mut h), ["a", "b", "c"], "the losses are ticked");
    let a = row(&mut h, "a");
    assert_eq!(
        (a.label.as_str(), a.place.as_str()),
        ("label-a", "r1"),
        "the label and the repo"
    );
    assert!(a.ended.ends_with(" · lost"), "{}", a.ended);
    assert_eq!(a.conversations, 1);
    assert_eq!(button(&mut h), ("Recover 3".to_owned(), true));

    h.click_on("recover-other-toggle");
    assert_eq!(row_ids(&mut h), ["a", "b", "c", "s"]);
    assert!(!row(&mut h, "s").ticked, "a user's stop is listed unticked");
    h.click_on("recover-row-s");
    assert_eq!(button(&mut h).0, "Recover 4");
}

#[gpui::test]
fn recover_as_choices_match_folder_flags(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.load(&Fixture {
        repos: vec![repo("r1", "D:\\r1")],
        ..Fixture::default()
    });
    open_with(
        &mut h,
        vec![
            history_item("git")
                .folder_only("D:\\proj", true, None)
                .ended_minutes_ago(10)
                .build(),
            history_item("reg")
                .folder_only("D:\\r1", true, Some("r1"))
                .ended_minutes_ago(12)
                .build(),
            history_item("plain")
                .folder_only("D:\\notes", false, None)
                .ended_minutes_ago(14)
                .build(),
            history_item("claude").ended_minutes_ago(16).build(),
        ],
    );
    let recover_as = |h: &mut Harness<'_>, id: &str| row(h, id).recover_as;
    assert_eq!(
        recover_as(&mut h, "git").as_deref(),
        Some("Shell running claude --resume"),
        "an unregistered git folder starts on the shell"
    );
    assert_eq!(
        recover_as(&mut h, "reg").as_deref(),
        Some("Claude session in r1")
    );
    assert_eq!(
        recover_as(&mut h, "plain").as_deref(),
        Some("Shell running claude --resume")
    );
    assert_eq!(recover_as(&mut h, "claude"), None, "no choice to make");
    assert!(!h.in_model("recover-as-claude"));

    h.click_on("recover-as-git");
    assert_eq!(
        recover_as(&mut h, "git").as_deref(),
        Some("Register D:\\proj as a repo, then Claude session"),
        "a click moves to the next choice"
    );
    h.click_on("recover-as-plain");
    assert_eq!(
        recover_as(&mut h, "plain").as_deref(),
        Some("Shell running claude --resume"),
        "the only choice stays"
    );
}

#[gpui::test]
fn disabled_rows_show_reason(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    open_with(
        &mut h,
        vec![
            history_item("r").recovered().ended_minutes_ago(10).build(),
            history_item("n")
                .candidates(&[])
                .ended_minutes_ago(12)
                .build(),
        ],
    );
    let reason = row(&mut h, "r").disabled.expect("a reason");
    assert!(reason.starts_with("recovered "), "{reason}");
    assert_eq!(
        row(&mut h, "n").disabled.as_deref(),
        Some("no conversation found")
    );
    assert!(ticked(&mut h).is_empty());
    h.click_on("recover-row-n");
    h.click_on("recover-select-all");
    assert!(ticked(&mut h).is_empty(), "a disabled row never ticks");
    assert_eq!(button(&mut h), ("Recover 0".to_owned(), false));
    h.click_on("recover-submit");
    assert!(
        !h.sent()
            .iter()
            .any(|msg| matches!(msg, ClientMessage::RecoverSessions { .. })),
        "nothing to recover"
    );
}

#[gpui::test]
fn recover_sends_one_recover_sessions_with_chosen_items(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    open_with(
        &mut h,
        vec![
            history_item("a")
                .candidates(&["x", "y"])
                .ended_minutes_ago(10)
                .build(),
            history_item("b").ended_minutes_ago(12).build(),
            history_item("sh")
                .shell("D:\\tmp")
                .ended_minutes_ago(14)
                .build(),
        ],
    );
    assert!(
        row(&mut h, "a")
            .conversation
            .is_some_and(|c| c.starts_with("Chat x")),
        "the newest conversation first"
    );
    h.click_on("recover-conversation-a");
    assert!(
        row(&mut h, "a")
            .conversation
            .is_some_and(|c| c.starts_with("Chat y"))
    );
    h.click_on("recover-submit");
    let (_, items) = recover_request(&mut h);
    assert_eq!(
        items,
        [
            RecoverItem {
                history_id: "a".to_owned(),
                conversation_id: Some("y".to_owned()),
                how: RecoverAs::Claude,
            },
            RecoverItem {
                history_id: "b".to_owned(),
                conversation_id: Some("c-b".to_owned()),
                how: RecoverAs::Claude,
            },
            RecoverItem {
                history_id: "sh".to_owned(),
                conversation_id: None,
                how: RecoverAs::Shell,
            },
        ]
    );
    assert_eq!(button(&mut h), ("Recovering…".to_owned(), false));
    h.click_on("recover-row-b");
    h.click_on("recover-submit");
    assert!(ticked(&mut h).contains(&"b".to_owned()), "controls wait");
    assert!(
        !h.sent()
            .iter()
            .any(|msg| matches!(msg, ClientMessage::RecoverSessions { .. })),
        "one request at a time"
    );
}

#[gpui::test]
fn failures_stay_listed_with_messages(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    open_with(
        &mut h,
        vec![
            history_item("a").ended_minutes_ago(10).build(),
            history_item("b").ended_minutes_ago(12).build(),
        ],
    );
    h.click_on("recover-submit");
    let (request_id, _) = recover_request(&mut h);
    h.advance(std::time::Duration::from_secs(120));
    assert_eq!(
        h.root(|root, _| root.recover_error()).as_deref(),
        Some("Recovery did not answer")
    );
    assert_eq!(
        button(&mut h),
        ("Recover 2".to_owned(), true),
        "usable again"
    );

    answer(
        &mut h,
        &request_id,
        vec![ok("a", "new-a"), failed("b", "worktree gone")],
    );
    assert!(is_open(&mut h), "a failure keeps the dialog open");
    assert_eq!(row_ids(&mut h), ["b"], "only the failed row stays");
    let b = row(&mut h, "b");
    assert_eq!(b.error.as_deref(), Some("worktree gone"));
    assert!(b.ticked, "ticked for another try");
    assert_eq!(button(&mut h), ("Recover 1".to_owned(), true));

    history(
        &mut h,
        vec![
            history_item("a").recovered().ended_minutes_ago(10).build(),
            history_item("b").ended_minutes_ago(12).build(),
        ],
    );
    assert_eq!(row_ids(&mut h), ["b"], "a fresh history adds no rows back");
    assert_eq!(row(&mut h, "b").error.as_deref(), Some("worktree gone"));
}

/// Two panes side by side, nothing in either: the first two placements fill
/// them, the third splits the first one.
fn empty_pair(h: &mut Harness<'_>) {
    let grid = split(
        SplitDirection::Horizontal,
        pane("p1", None),
        pane("p2", None),
    );
    h.load(&Fixture {
        repos: vec![repo("r1", "D:\\r1")],
        tabs: vec![tab("t1", &grid)],
        ..Fixture::default()
    });
    h.sent();
}

#[gpui::test]
fn all_succeeded_closes_and_places_sessions(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    empty_pair(&mut h);
    open_with(
        &mut h,
        vec![
            history_item("a").ended_minutes_ago(10).build(),
            history_item("b").ended_minutes_ago(12).build(),
            history_item("c").ended_minutes_ago(14).build(),
        ],
    );
    h.click_on("recover-submit");
    let (request_id, _) = recover_request(&mut h);
    for id in ["new-a", "new-b", "new-c"] {
        h.send(DaemonMessage::SessionUpdated {
            session: session(id).build(),
            request_id: None,
        });
    }
    assert!(
        placements(h.sent()).is_empty(),
        "nothing is placed before the answer"
    );
    answer(
        &mut h,
        &request_id,
        vec![ok("a", "new-a"), ok("b", "new-b"), ok("c", "new-c")],
    );
    assert!(!is_open(&mut h), "every item recovered: the dialog closes");
    assert_eq!(
        placements(h.sent()),
        ["replace p1 new-a", "replace p2 new-b", "split p1 new-c"],
        "the batch lands in one go, each session in a pane of its own"
    );
}

#[gpui::test]
fn late_snapshot_is_placed_with_its_batch(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    empty_pair(&mut h);
    open_with(
        &mut h,
        vec![
            history_item("a").ended_minutes_ago(10).build(),
            history_item("b").ended_minutes_ago(12).build(),
            history_item("c").ended_minutes_ago(14).build(),
        ],
    );
    h.click_on("recover-submit");
    let (request_id, _) = recover_request(&mut h);
    answer(
        &mut h,
        &request_id,
        vec![ok("a", "new-a"), ok("b", "new-b"), ok("c", "new-c")],
    );
    for id in ["new-a", "new-b"] {
        h.send(DaemonMessage::SessionUpdated {
            session: session(id).build(),
            request_id: None,
        });
        assert!(
            placements(h.sent()).is_empty(),
            "the batch waits for its last snapshot, {id} alone places nothing"
        );
    }
    h.send(DaemonMessage::SessionUpdated {
        session: session("new-c").build(),
        request_id: None,
    });
    let sent = h.sent();
    let replaced: Vec<String> = sent
        .iter()
        .filter_map(|msg| match msg {
            ClientMessage::ReplacePaneSession { pane_id, .. } => Some(pane_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(replaced.len(), 2, "the two empty panes");
    assert_eq!(
        replaced.iter().collect::<HashSet<_>>().len(),
        replaced.len(),
        "no pane takes two sessions of the batch"
    );
    assert_eq!(
        placements(sent),
        ["replace p1 new-a", "replace p2 new-b", "split p1 new-c"],
        "the whole batch lands as soon as the last snapshot arrives"
    );
}

#[gpui::test]
fn recovered_abandoned_session_keeps_its_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    let grid = split(
        SplitDirection::Horizontal,
        pane("p1", Some("old")),
        pane("p2", None),
    );
    h.load(&Fixture {
        repos: vec![repo("r1", "D:\\r1")],
        sessions: vec![session("old").abandoned().build()],
        tabs: vec![tab("t1", &grid)],
        ..Fixture::default()
    });
    h.sent();
    open_with(&mut h, vec![history_item("old").build()]);
    h.click_on("recover-submit");
    let (request_id, _) = recover_request(&mut h);
    answer(&mut h, &request_id, vec![ok("old", "new-old")]);
    assert!(!is_open(&mut h), "every item recovered: the dialog closes");
    assert!(
        placements(h.sent()).is_empty(),
        "the client places nothing: the daemon rebinds the abandoned panes"
    );
    h.send(DaemonMessage::SessionUpdated {
        session: session("new-old").build(),
        request_id: None,
    });
    assert!(
        placements(h.sent()).is_empty(),
        "its snapshot is left to the daemon too"
    );
}

#[gpui::test]
fn late_answer_after_timeout_is_placed(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    let grid = pane("p1", None);
    h.load(&Fixture {
        repos: vec![repo("r1", "D:\\r1")],
        tabs: vec![tab("t1", &grid)],
        ..Fixture::default()
    });
    h.sent();
    open_with(
        &mut h,
        vec![history_item("a").ended_minutes_ago(10).build()],
    );
    h.click_on("recover-submit");
    let (request_id, _) = recover_request(&mut h);
    h.advance(std::time::Duration::from_secs(120));
    assert_eq!(
        h.root(|root, _| root.recover_error()).as_deref(),
        Some("Recovery did not answer")
    );
    h.click_on("recover-cancel");
    assert!(!is_open(&mut h), "the dialog is gone");

    answer(&mut h, &request_id, vec![ok("a", "new-a")]);
    assert!(
        placements(h.sent()).is_empty(),
        "an answer with no snapshot yet waits"
    );
    h.send(DaemonMessage::SessionUpdated {
        session: session("new-a").build(),
        request_id: None,
    });
    assert_eq!(
        placements(h.sent()),
        ["replace p1 new-a"],
        "an answer to a timed-out request still places its session"
    );
}

#[gpui::test]
fn broadcast_while_open_keeps_choices(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    let a = || {
        history_item("a")
            .candidates(&["x", "y"])
            .ended_minutes_ago(10)
    };
    open_with(
        &mut h,
        vec![
            a().build(),
            history_item("b").ended_minutes_ago(12).build(),
            history_item("c").ended_minutes_ago(14).build(),
        ],
    );
    h.click_on("recover-conversation-a");
    h.click_on("recover-row-b");
    history(
        &mut h,
        vec![
            a().build(),
            history_item("b").ended_minutes_ago(12).build(),
            history_item("d").ended_minutes_ago(3).build(),
        ],
    );
    assert_eq!(row_ids(&mut h), ["d", "a", "b"], "c is gone, d joins");
    assert!(
        row(&mut h, "a")
            .conversation
            .is_some_and(|c| c.starts_with("Chat y")),
        "the conversation choice stays"
    );
    assert_eq!(ticked(&mut h), ["d", "a"], "b stays unticked, d is a loss");
}

#[gpui::test]
fn keys_arrows_space_enter_escape(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    open_with(
        &mut h,
        vec![
            history_item("a")
                .candidates(&["x", "y"])
                .ended_minutes_ago(10)
                .build(),
            history_item("b").ended_minutes_ago(12).build(),
        ],
    );
    assert_eq!(focus(&mut h).as_deref(), Some("recover-row-a"));
    h.keys("down");
    assert_eq!(focus(&mut h).as_deref(), Some("recover-row-b"));
    h.keys("space");
    assert!(!row(&mut h, "b").ticked, "space unticks the row");
    h.keys("up");
    assert_eq!(focus(&mut h).as_deref(), Some("recover-row-a"));
    h.keys("tab");
    assert_eq!(focus(&mut h).as_deref(), Some("recover-conversation-a"));
    h.keys("right");
    assert!(
        row(&mut h, "a")
            .conversation
            .is_some_and(|c| c.starts_with("Chat y"))
    );
    h.keys("left");
    assert!(
        row(&mut h, "a")
            .conversation
            .is_some_and(|c| c.starts_with("Chat x"))
    );
    h.keys("shift-tab");
    assert_eq!(focus(&mut h).as_deref(), Some("recover-row-a"));
    h.keys("enter");
    let (request_id, items) = recover_request(&mut h);
    let ids: Vec<&str> = items.iter().map(|i| i.history_id.as_str()).collect();
    assert_eq!(ids, ["a"], "Enter recovers the ticked rows");
    h.keys("escape");
    assert!(is_open(&mut h), "Esc waits for the answer");
    answer(&mut h, &request_id, vec![ok("a", "new-a")]);
    assert!(!is_open(&mut h));

    h.click_on("activity-recover");
    assert!(is_open(&mut h));
    h.keys("escape");
    assert!(!is_open(&mut h), "Esc closes");

    h.click_on("activity-recover");
    h.keys("shift-tab");
    h.keys("shift-tab");
    assert_eq!(focus(&mut h).as_deref(), Some("recover-cancel"));
    h.keys("enter");
    assert!(!is_open(&mut h), "Enter presses the focused button");
    assert!(
        !h.sent()
            .iter()
            .any(|msg| matches!(msg, ClientMessage::RecoverSessions { .. })),
        "Cancel recovers nothing"
    );
}

#[gpui::test]
fn welcome_closes_dialog(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    open_with(&mut h, vec![history_item("a").build()]);
    welcome(&mut h);
    assert!(!is_open(&mut h), "a new connection closes the dialog");
    assert!(!h.in_model("recover"));
    assert!(asks_history(&h.sent()));
}

#[gpui::test]
fn empty_history_says_so(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    open_with(&mut h, Vec::new());
    assert_eq!(
        h.root(|root, _| root.recover_empty_text()),
        Some("No ended sessions to recover.")
    );
    assert_eq!(button(&mut h), ("Recover 0".to_owned(), false));
    assert_eq!(badge(&mut h), None);
    h.click_on("recover-cancel");
    assert!(!is_open(&mut h));
}
