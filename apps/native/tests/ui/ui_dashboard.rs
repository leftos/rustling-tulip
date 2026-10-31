//! Dashboard specs: which sessions it lists, the four groups and their order
//! over scripted snapshots (the Answer rule included), and the header counts,
//! busy-excluded sessions left out of the Working count; then the By status
//! view the tab draws: its header, queue strip, rows and nested rows, read
//! through the cells the view renders from and the ids it tags.

use crate::support;

use chrono::{DateTime, Duration, Utc};
use gpui::{TestAppContext, px, size};
use protocol::{AgentState, ClientMessage, DaemonMessage, SessionSnapshot, TabEntry};
use rustling_tulip_native::{DashboardColumns, DashboardEntry, RowCells};
use serde_json::json;
use support::{Fixture, Harness, TestDir, facts, facts_message, pane, repo, session, tab};

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

fn dashboard_tab(id: &str) -> TabEntry {
    serde_json::from_value(json!({
        "id": id,
        "name": "Dashboard",
        "content": { "kind": "dashboard" },
        "created_at": "2026-01-01T00:00:00Z",
    }))
    .expect("dashboard tab fixture")
}

/// `sessions` in repo `r1`, a terminal tab `t1` showing the first of them in
/// pane `p1`, and a Dashboard tab `d1`.
fn on_dashboard(sessions: Vec<SessionSnapshot>) -> Fixture {
    let first = sessions.first().map(|session| session.id.clone());
    Fixture {
        repos: vec![repo("r1", "D:/src/r1")],
        sessions,
        tabs: vec![
            tab("t1", &pane("p1", first.as_deref())),
            dashboard_tab("d1"),
        ],
        ..Fixture::default()
    }
}

/// Widens the window so every column fits, then shows the Dashboard tab.
fn show_dashboard(h: &mut Harness<'_>) {
    h.cx.simulate_resize(size(px(1920.0), px(1200.0)));
    h.click_on("tab-d1");
    assert_eq!(
        h.root(|root, _| root.active_tab_id().map(str::to_owned)),
        Some("d1".to_owned())
    );
}

/// The cells the view draws for `id`'s row.
fn cells(h: &mut Harness<'_>, id: &str) -> RowCells {
    h.root(|root, _| root.dashboard_view_rows())
        .into_iter()
        .flat_map(|(_, rows)| rows)
        .find(|row| row.session_id == id)
        .expect("the session has a row")
}

/// Whether the element `selector` tags was ever painted.
fn drawn(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).origin.x >= px(0.0)
}

fn minutes_ago(minutes: i64) -> DateTime<Utc> {
    Utc::now() - Duration::minutes(minutes)
}

fn working(id: &str) -> SessionSnapshot {
    session(id).in_repo("r1").status("working").build()
}

#[gpui::test]
fn dashboard_tab_draws_header_group_headings_and_a_row_per_listed_session(cx: &mut TestAppContext) {
    let fixture = on_dashboard(vec![
        session("ask")
            .in_repo("r1")
            .status("awaiting_input")
            .build(),
        working("work"),
        session("idle").in_repo("r1").build(),
        session("done").in_repo("r1").exited(0).build(),
    ]);
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture);
    show_dashboard(&mut h);

    assert!(drawn(&mut h, "dashboard-header"));
    assert!(drawn(&mut h, "dashboard-columns"));
    for pill in ["need-you", "working", "idle", "done"] {
        assert!(
            drawn(&mut h, &format!("dashboard-pill-{pill}")),
            "{pill} pill"
        );
    }
    for group in ["needs-you", "working", "idle", "done"] {
        assert!(
            drawn(&mut h, &format!("dashboard-group-{group}")),
            "{group} heading"
        );
    }
    let rows: Vec<(&str, Vec<String>)> = h
        .root(|root, _| root.dashboard_view_rows())
        .into_iter()
        .map(|(heading, rows)| {
            (
                heading,
                rows.into_iter().map(|row| row.session_id).collect(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            ("Needs you", vec!["ask".to_owned()]),
            ("Working", vec!["work".to_owned()]),
            ("Idle", vec!["idle".to_owned()]),
            ("Done", vec!["done".to_owned()]),
        ],
        "one row per listed session, under its group"
    );
    for id in ["ask", "work", "idle", "done"] {
        assert!(drawn(&mut h, &format!("dashboard-row-{id}")), "{id}'s row");
    }
}

#[gpui::test]
fn each_column_shows_its_field(cx: &mut TestAppContext) {
    let fixture = on_dashboard(vec![
        session("w1")
            .members(&[("r1", "feat/x", "")])
            .status("working")
            .started_at(minutes_ago(70))
            .status_since(minutes_ago(2))
            .summary_lines(
                "Refactoring auth",
                &[
                    ("Refactoring auth", minutes_ago(1)),
                    ("Read the module", minutes_ago(20)),
                ],
            )
            .build(),
    ]);
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture);
    show_dashboard(&mut h);
    h.send(facts_message(vec![
        facts("w1")
            .now("running_tool", "Running Bash · 1m: cargo test")
            .context(38)
            .cost(0.46)
            .tokens_in(3_100_000)
            .activity(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9])
            .stuck(json!([{ "kind": "tool_failures", "count": 3 }]))
            .agents(json!([
                { "key": "subagent:1", "kind": "subagent", "name": "look", "state": "running", "started_at": minutes_ago(1) },
                { "key": "bg:1", "kind": "shell", "name": "bg shell 1", "state": "finished", "started_at": minutes_ago(3) },
            ]))
            .last_activity(minutes_ago(5))
            .issue("RT-444")
            .build(),
    ]));

    let row = cells(&mut h, "w1");
    assert_eq!(row.label, "w1", "Session: the label");
    assert_eq!(
        (row.container.as_str(), row.branch.as_str()),
        ("r1", "feat/x"),
        "Session: container and branch"
    );
    assert_eq!(row.issue, "RT-444", "Linear: the issue key");
    assert_eq!(row.state_word, "Working", "Now: the state word");
    assert_eq!(row.age.as_deref(), Some("2m"), "Now: the status age");
    assert_eq!(
        row.stuck,
        ["⚑ 3 tool calls failed in a row"],
        "Now: the stuck flag"
    );
    assert_eq!(
        row.now, "Running Bash · 1m: cargo test",
        "Now: the now line"
    );
    assert_eq!(row.summary, "Refactoring auth", "Summary: the newest line");
    assert_eq!(
        row.summary_older.as_deref(),
        Some("20m · Read the module"),
        "Summary: the older line"
    );
    assert_eq!(row.agents, "1 sub (1 live) · 1 bg", "Agents");
    assert_eq!(row.context, "38%", "Context");
    assert_eq!(row.activity.len(), 10, "10 min: ten bars");
    assert!(!row.activity_quiet);
    assert_eq!(
        (row.run.as_str(), row.last.as_str()),
        ("1h10m", "5m ago"),
        "Run · last"
    );
    assert_eq!(
        (row.cost.as_str(), row.tokens.as_str()),
        ("$0.46", "3.1M"),
        "Cost"
    );
    assert_eq!(
        h.root(|root, _| root.dashboard_columns()),
        DashboardColumns {
            summary: true,
            linear: true,
            activity: true,
            agents: true,
        },
        "a wide view draws every column"
    );
    assert!(drawn(&mut h, "dashboard-ctx-w1"));
    assert!(drawn(&mut h, "dashboard-spark-w1"));
}

#[gpui::test]
fn a_hot_context_draws_in_the_danger_colour_past_80(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(
        cx,
        &dir,
        &on_dashboard(vec![working("hot"), working("cool")]),
    );
    show_dashboard(&mut h);
    h.send(facts_message(vec![
        facts("hot").context(81).build(),
        facts("cool").context(80).build(),
    ]));
    assert!(cells(&mut h, "hot").context_hot);
    assert!(!cells(&mut h, "cool").context_hot, "80% is not past 80");
    assert!(drawn(&mut h, "dashboard-ctx-hot-hot"));
    assert!(drawn(&mut h, "dashboard-ctx-cool"));
    assert!(!drawn(&mut h, "dashboard-ctx-cool-hot"));
}

#[gpui::test]
fn a_waiting_row_is_washed_and_striped(cx: &mut TestAppContext) {
    let fixture = on_dashboard(vec![
        session("a1").in_repo("r1").status("awaiting_input").build(),
        session("i1").in_repo("r1").build(),
    ]);
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture);
    show_dashboard(&mut h);
    assert!(cells(&mut h, "a1").asking);
    assert!(!cells(&mut h, "i1").asking);
    assert!(drawn(&mut h, "dashboard-row-a1-asking"));
    assert!(drawn(&mut h, "dashboard-row-i1"));
    assert!(!drawn(&mut h, "dashboard-row-i1-asking"));
}

#[gpui::test]
fn the_now_line_follows_the_rule_for_each_status(cx: &mut TestAppContext) {
    let fixture = on_dashboard(vec![
        session("ask")
            .in_repo("r1")
            .status("awaiting_input")
            .pending_input(
                json!({ "kind": "permission", "tool_name": "Bash", "summary": "cargo build" }),
            )
            .build(),
        working("work"),
        working("unseen"),
        session("ended").in_repo("r1").exited(3).build(),
        session("errored").in_repo("r1").status("error").build(),
    ]);
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture);
    show_dashboard(&mut h);
    assert_eq!(
        cells(&mut h, "work").now,
        "Thinking",
        "a working row before its facts"
    );
    h.send(facts_message(vec![
        facts("work")
            .now("waiting_on_subagent", "Waiting on subagent \"look\"")
            .build(),
    ]));
    update(&mut h, session("unseen").in_repo("r1").build());

    assert_eq!(cells(&mut h, "ask").now, "Allow Bash: cargo build");
    assert_eq!(cells(&mut h, "ask").state_word, "Permission");
    assert_eq!(cells(&mut h, "work").now, "Waiting on subagent \"look\"");
    assert_eq!(
        cells(&mut h, "unseen").now,
        "Turn finished · not looked at yet"
    );
    assert_eq!(cells(&mut h, "unseen").state_word, "Finished");
    assert_eq!(cells(&mut h, "ended").now, "Session ended · exit 3");
    assert_eq!(cells(&mut h, "ended").state_word, "Exited 3");
    assert_eq!(cells(&mut h, "errored").now, "Error");
}

/// A subagent with a finished child, then a finished shell.
fn agent_tree() -> serde_json::Value {
    json!([
        {
            "key": "subagent:1", "kind": "subagent", "name": "look at the parser", "state": "running",
            "started_at": minutes_ago(3),
            "children": [
                { "key": "subagent:2", "kind": "subagent", "name": "read it", "state": "finished", "started_at": minutes_ago(2) },
            ],
        },
        { "key": "bg:1", "kind": "shell", "name": "bg shell 1", "state": "finished", "started_at": minutes_ago(1) },
    ])
}

#[gpui::test]
fn dashboard_rows_grow_with_the_ui_text_size(cx: &mut TestAppContext) {
    let fixture = on_dashboard(vec![working("w1")]);
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture);
    show_dashboard(&mut h);
    h.send(facts_message(vec![
        facts("w1").agents(agent_tree()).build(),
    ]));
    let sub = format!("dashboard-sub-w1-{}", cells(&mut h, "w1").nested[0].key);
    let row = h.bounds(&sub).size.height;
    let columns = h.bounds("dashboard-columns").size.height;

    h.root_update(|root, _, cx| root.set_ui_text_size(3, cx));
    assert_eq!(h.bounds(&sub).size.height, row + px(3.0), "the nested row");
    assert_eq!(
        h.bounds("dashboard-columns").size.height,
        columns + px(3.0),
        "the column header"
    );
}

#[gpui::test]
fn nested_rows_show_under_working_rows_only(cx: &mut TestAppContext) {
    let fixture = on_dashboard(vec![working("w1"), session("i1").in_repo("r1").build()]);
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture);
    show_dashboard(&mut h);
    h.send(facts_message(vec![
        facts("w1").agents(agent_tree()).build(),
        facts("i1").agents(agent_tree()).build(),
    ]));

    let nested = cells(&mut h, "w1").nested;
    assert_eq!(
        nested
            .iter()
            .map(|row| (row.key.as_str(), row.depth, row.dimmed))
            .collect::<Vec<_>>(),
        [
            ("subagent:1", 0, false),
            ("subagent:2", 1, true),
            ("bg:1", 0, true)
        ],
        "depth first, the finished nodes dimmed"
    );
    assert_eq!(nested[0].state, AgentState::Running);
    for key in ["subagent:1", "subagent:2", "bg:1"] {
        assert!(
            drawn(&mut h, &format!("dashboard-sub-w1-{key}")),
            "{key} under w1"
        );
        assert!(
            !drawn(&mut h, &format!("dashboard-sub-i1-{key}")),
            "nothing under the idle row"
        );
    }
    assert_eq!(cells(&mut h, "i1").nested.len(), 0);
}

#[gpui::test]
fn facts_arriving_after_the_rows_redraw_them(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &on_dashboard(vec![working("w1")]));
    show_dashboard(&mut h);
    assert_eq!(cells(&mut h, "w1").context, "—");
    assert!(!drawn(&mut h, "dashboard-ctx-w1-hot"));

    h.send(facts_message(vec![
        facts("w1")
            .context(90)
            .now("running_tool", "Running Bash · 1m: cargo test")
            .build(),
    ]));
    let row = cells(&mut h, "w1");
    assert_eq!(row.context, "90%");
    assert_eq!(row.now, "Running Bash · 1m: cargo test");
    assert!(
        drawn(&mut h, "dashboard-ctx-w1-hot"),
        "the row was drawn again with its facts"
    );
}

#[gpui::test]
fn the_queue_strip_lists_waits_longest_first_and_is_absent_when_none_wait(cx: &mut TestAppContext) {
    let fixture = on_dashboard(vec![
        session("new").in_repo("r1").build(),
        session("old").in_repo("r1").build(),
    ]);
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture);
    show_dashboard(&mut h);
    assert_eq!(h.root(|root, _| root.dashboard_queue()).len(), 0);
    assert!(!drawn(&mut h, "dashboard-queue-strip"), "no wait, no strip");

    let asking = |id: &str, since: i64| {
        session(id)
            .in_repo("r1")
            .status("awaiting_input")
            .status_since(minutes_ago(since))
            .build()
    };
    update(&mut h, asking("new", 1));
    update(&mut h, asking("old", 9));
    let chips = h.root(|root, _| root.dashboard_queue());
    assert_eq!(
        chips
            .iter()
            .map(|chip| (chip.session_id.as_str(), chip.age.as_str()))
            .collect::<Vec<_>>(),
        [("old", "9m"), ("new", "1m")],
        "longest wait first"
    );
    assert_eq!(chips[0].title, "old", "no issue key reads as the label");
    assert!(drawn(&mut h, "dashboard-queue-strip"));
    assert!(drawn(&mut h, "dashboard-queue-old"));
    assert!(drawn(&mut h, "dashboard-queue-new"));

    update(&mut h, session("new").in_repo("r1").build());
    update(&mut h, session("old").in_repo("r1").build());
    assert_eq!(
        h.root(|root, _| root.dashboard_queue()).len(),
        0,
        "no wait left"
    );
}

#[gpui::test]
fn a_queue_chip_click_scrolls_its_row_into_view(cx: &mut TestAppContext) {
    let mut sessions = vec![
        session("ask")
            .in_repo("r1")
            .status("awaiting_input")
            .build(),
    ];
    sessions.extend((0..40).map(|n| {
        session(&format!("idle{n:02}"))
            .in_repo("r1")
            .status_since(minutes_ago(n))
            .build()
    }));
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &on_dashboard(sessions));
    show_dashboard(&mut h);
    h.scroll_into_view("dashboard-body", "dashboard-row-idle39");
    let body = h.bounds("dashboard-body");
    assert!(
        h.bounds("dashboard-row-ask").bottom() <= body.top(),
        "the waiting row scrolled out of view"
    );

    h.click_on("dashboard-queue-ask");
    let (body, row) = (h.bounds("dashboard-body"), h.bounds("dashboard-row-ask"));
    assert!(
        row.top() >= body.top() && row.bottom() <= body.bottom(),
        "the chip scrolled its row back into view: {row:?} in {body:?}"
    );
}

#[gpui::test]
fn a_row_click_opens_its_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &on_dashboard(vec![working("s1")]));
    show_dashboard(&mut h);
    h.click_on("dashboard-row-s1");
    assert_eq!(
        h.root(|root, _| root.active_tab_id().map(str::to_owned)),
        Some("t1".to_owned()),
        "the session's pane is shown"
    );
}

#[gpui::test]
fn header_figures_are_hidden_while_unknown(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &on_dashboard(vec![working("w1")]));
    show_dashboard(&mut h);
    h.send(facts_message(vec![facts("w1").build()]));
    assert_eq!(
        h.root(|root, _| root.dashboard_figures()),
        None,
        "zero totals are not known"
    );
    assert!(!drawn(&mut h, "dashboard-figures"));
    assert!(
        drawn(&mut h, "dashboard-pill-working"),
        "the count pills always show"
    );

    h.send(DaemonMessage::DashboardFacts {
        facts: Vec::new(),
        totals: protocol::DayTotals {
            cost_usd: 1.5,
            tokens: 1_000,
            ..protocol::DayTotals::default()
        },
        linear_synced_at: None,
    });
    let figures = h
        .root(|root, _| root.dashboard_figures())
        .expect("measured totals are known");
    assert_eq!(
        (figures.cost.as_str(), figures.tokens.as_str()),
        ("$1.50", "1k")
    );
    assert!(drawn(&mut h, "dashboard-figures"));
}

#[gpui::test]
fn a_session_without_facts_draws_dashes_not_zero(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(
        cx,
        &dir,
        &on_dashboard(vec![session("i1").in_repo("r1").build()]),
    );
    show_dashboard(&mut h);
    let row = cells(&mut h, "i1");
    assert_eq!(row.context, "—");
    assert_eq!(row.agents, "—");
    assert_eq!((row.cost.as_str(), row.tokens.as_str()), ("—", "—"));
    assert_eq!(row.last, "—");
    assert_eq!(row.issue, "—");
    assert!(row.activity_quiet, "quiet bars");
    assert_eq!(row.activity.len(), 10);
    assert_eq!(row.nested.len(), 0);
}

#[gpui::test]
fn facts_for_a_removed_session_are_pruned(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &on_dashboard(vec![working("w1")]));
    show_dashboard(&mut h);
    h.send(facts_message(vec![facts("w1").context(90).build()]));
    assert_eq!(cells(&mut h, "w1").context, "90%", "the facts are held");

    h.send(DaemonMessage::SessionRemoved {
        session_id: "w1".to_owned(),
    });
    h.send(facts_message(Vec::new()));
    update(&mut h, working("w1"));
    assert_eq!(
        cells(&mut h, "w1").context,
        "—",
        "the removed session's facts were pruned, not kept for its id to come back to"
    );
}

#[gpui::test]
fn facts_arriving_after_unwatch_are_dropped(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &on_dashboard(vec![working("w1")]));
    show_dashboard(&mut h);
    h.click_on("tab-t1");
    assert_eq!(
        h.root(|root, _| root.active_tab_id().map(str::to_owned)),
        Some("t1".to_owned()),
        "another tab is active"
    );
    assert!(
        h.sent()
            .iter()
            .any(|msg| matches!(msg, ClientMessage::WatchDashboard { on: false })),
        "leaving the Dashboard ended its watch"
    );

    h.send(facts_message(vec![facts("w1").context(90).build()]));
    h.click_on("tab-d1");
    assert_eq!(
        cells(&mut h, "w1").context,
        "—",
        "no fact of a watch that ended is applied"
    );
}

#[gpui::test]
fn long_stuck_flags_stay_inside_the_now_cell(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &on_dashboard(vec![working("s1")]));
    show_dashboard(&mut h);
    // Narrow enough that the Context column is the one drawn right after
    // Now, so its left edge is the Now cell's own right boundary.
    h.cx.simulate_resize(size(px(1000.0), px(1200.0)));
    assert!(
        !h.root(|root, _| root.dashboard_columns()).agents,
        "the Context column follows Now at this width"
    );
    h.send(facts_message(vec![
        facts("s1")
            .stuck(json!([
                { "kind": "tool_failures", "count": 65_535 },
                { "kind": "context_high" },
            ]))
            .build(),
    ]));
    let next = h.bounds("dashboard-ctx-s1").left();
    assert!(next > px(0.0), "the Context column is drawn");
    for index in [0, 1] {
        assert!(
            drawn(&mut h, &format!("dashboard-row-s1-flag-{index}")),
            "flag {index} is drawn"
        );
        let flag = h.bounds(&format!("dashboard-row-s1-flag-{index}"));
        assert!(
            flag.right() <= next,
            "flag {index} ends at {} but the next column starts at {next}",
            flag.right()
        );
    }
}

#[gpui::test]
fn narrow_views_drop_columns_in_order(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &on_dashboard(vec![working("w1")]));
    show_dashboard(&mut h);
    let mut seen = Vec::new();
    for window in [
        2600.0, 1800.0, 1600.0, 1450.0, 1350.0, 1250.0, 1100.0, 900.0,
    ] {
        h.cx.simulate_resize(size(px(window), px(1200.0)));
        let width = h.bounds("dashboard-body").size.width / px(1.0);
        let columns = h.root(|root, _| root.dashboard_columns());
        assert_eq!(
            columns,
            DashboardColumns {
                summary: width >= 1400.0,
                linear: width >= 1200.0,
                activity: width >= 1050.0,
                agents: width >= 950.0,
            },
            "a view {width} px wide"
        );
        seen.push(columns);
    }
    assert!(
        seen.first().is_some_and(|wide| wide.summary && wide.agents),
        "the widest view shows every column"
    );
    assert!(
        seen.last().is_some_and(|narrow| !narrow.agents),
        "the narrowest drops them all"
    );
}
