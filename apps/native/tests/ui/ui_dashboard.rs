//! Dashboard specs: which sessions it lists, the four groups and their order
//! over scripted snapshots (the Answer rule included), and the header counts,
//! busy-excluded sessions left out of the Working count; then the By status
//! view the tab draws: its header, queue strip, rows and nested rows, read
//! through the cells the view renders from and the ids it tags; last the
//! Compact view: its repo blocks, rows and chips, its fit with no scrolling
//! either way, its folds, clicks and toggle.

use crate::support;

use chrono::{DateTime, Duration, Local, Utc};
use gpui::{Font, FontWeight, Modifiers, TestAppContext, TextRun, black, font, px, size};
use protocol::{
    ActivitySegment, AgentState, ClientMessage, DaemonMessage, PromptChoice, SegmentActivity,
    SegmentKind, SessionSnapshot, TabEntry,
};
use rustling_tulip_native::fonts;
use rustling_tulip_native::{
    COMPACT_EMPTY_TEXT, ChipKind, CompactBlock, CompactRow, DashboardColumns, DashboardEntry,
    DrawerCells, RowCells, RunKind, Tally, enable_band_probe,
};
use serde_json::json;
use support::{
    Fixture, Harness, TestDir, activity_tracks_message, facts, facts_message, pane, repo, session,
    tab, workspace,
};

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

/// Widens the window so every column fits, then opens the Dashboard tab.
fn open_dashboard(h: &mut Harness<'_>) {
    h.cx.simulate_resize(size(px(1920.0), px(1200.0)));
    h.click_on("tab-d1");
    assert_eq!(
        h.root(|root, _| root.active_tab_id().map(str::to_owned)),
        Some("d1".to_owned())
    );
}

/// Opens the Dashboard tab on its By status view.
fn show_dashboard(h: &mut Harness<'_>) {
    open_dashboard(h);
    h.click_on("dashboard-view-by-status");
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
            track: true,
        },
        "a wide view draws every column"
    );
    assert!(drawn(&mut h, "dashboard-ctx-w1"));
    assert!(drawn(&mut h, "dashboard-spark-w1"));
}

#[gpui::test]
fn a_long_now_line_wraps_whole(cx: &mut TestAppContext) {
    let fixture = on_dashboard(vec![
        session("w1")
            .members(&[("r1", "feat/x", "")])
            .status("working")
            .build(),
        session("w2")
            .members(&[("r1", "feat/y", "")])
            .status("working")
            .build(),
    ]);
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture);
    show_dashboard(&mut h);
    let long = format!(
        "Running Bash · 1m: {}",
        "cargo nextest run --workspace ".repeat(12)
    );
    h.send(facts_message(vec![
        facts("w1").now("running_tool", &long).build(),
        facts("w2")
            .now("running_tool", "Running Bash · 1m: cargo test")
            .build(),
    ]));

    let long_now = h.bounds("dashboard-row-w1-now");
    let short_now = h.bounds("dashboard-row-w2-now");
    assert!(
        long_now.size.height > short_now.size.height * 1.5,
        "a long Now line wraps: long {long_now:?}, short {short_now:?}"
    );
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
            "started_at": minutes_ago(3), "model": "Opus 5.5", "effort": "high",
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
fn a_subagent_row_shows_its_spawn_after_its_name(cx: &mut TestAppContext) {
    let fixture = on_dashboard(vec![working("w1")]);
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture);
    show_dashboard(&mut h);
    h.send(facts_message(vec![
        facts("w1").agents(agent_tree()).build(),
    ]));

    let nested = cells(&mut h, "w1").nested;
    assert_eq!(
        nested[0].spawn.as_deref(),
        Some("Opus 5.5 · high"),
        "the model and effort after the name"
    );
    assert!(
        drawn(&mut h, "dashboard-sub-w1-subagent:1-spawn"),
        "its spawn is drawn"
    );
    assert_eq!(nested[1].spawn, None, "a subagent with neither shows none");
    assert!(
        !drawn(&mut h, "dashboard-sub-w1-subagent:2-spawn"),
        "and draws no element"
    );
    assert_eq!(nested[2].spawn, None, "a shell shows none either");
    assert!(
        !drawn(&mut h, "dashboard-sub-w1-bg:1-spawn"),
        "and draws no element"
    );
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

/// The `GetSessionDetail`s the client sent since the last read, as
/// `(session, request id)`.
fn detail_requests(h: &mut Harness<'_>) -> Vec<(String, String)> {
    h.sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::GetSessionDetail {
                session_id,
                request_id,
            } => Some((
                session_id,
                request_id.expect("a drawer's request carries an id"),
            )),
            _ => None,
        })
        .collect()
}

/// Clicks `id`'s row open and returns the one `GetSessionDetail` it sent.
fn open_drawer(h: &mut Harness<'_>, id: &str) -> String {
    h.sent();
    h.click_on(&format!("dashboard-row-{id}"));
    let requests = detail_requests(h);
    assert_eq!(requests.len(), 1, "one detail request: {requests:?}");
    assert_eq!(requests[0].0, id);
    requests[0].1.clone()
}

/// The drawer the view draws under `id`'s row, `None` while it is closed.
fn drawer(h: &mut Harness<'_>, id: &str) -> Option<DrawerCells> {
    h.root(|root, _| root.dashboard_drawer(id))
}

/// Two uncommitted files, a commit, a finished issue and an open pull
/// request whose checks passed.
fn busy_work() -> serde_json::Value {
    json!({
        "commits": [{
            "sha": "abc1234567890",
            "short_sha": "abc1234",
            "subject": "Add the drawer",
            "at": minutes_ago(10),
        }],
        "issues_done": [{ "key": "RT-1", "title": "Ship it" }],
        "files": [
            { "path": "src/a.rs", "status": "M", "added": 12, "deleted": 3 },
            { "path": "src/b.rs", "status": "A", "added": 4, "deleted": 0 },
        ],
        "files_total": 2,
        "pull_request": { "number": 118, "state": "open", "checks": "passed" },
    })
}

/// The daemon's `SessionDetail` for `id`, answering `request_id`: two
/// summary lines, a prompt over two lines and `work`.
fn detail_reply(id: &str, request_id: &str, work: &serde_json::Value) -> DaemonMessage {
    serde_json::from_value(json!({
        "type": "session_detail",
        "session_id": id,
        "lines": [
            { "text": "Wired the drawer", "at": minutes_ago(3), "source": "model" },
            { "text": "Read the design", "at": minutes_ago(20), "source": "template" },
        ],
        "prompt": "Build the\n  drawer",
        "work": work,
        "request_id": request_id,
    }))
    .expect("session detail fixture")
}

#[gpui::test]
fn a_row_click_toggles_its_drawer(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &on_dashboard(vec![working("s1")]));
    show_dashboard(&mut h);
    h.click_on("dashboard-row-s1");
    assert_eq!(
        h.root(|root, _| root.active_tab_id().map(str::to_owned)),
        Some("d1".to_owned()),
        "the Dashboard stays shown"
    );
    assert!(drawer(&mut h, "s1").is_some(), "the drawer is open");
    assert!(drawn(&mut h, "dashboard-drawer-s1"));
}

#[gpui::test]
fn the_drawers_go_to_pane_opens_its_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &on_dashboard(vec![working("s1")]));
    show_dashboard(&mut h);
    h.click_on("dashboard-row-s1");
    h.click_on("dashboard-drawer-s1-go-to-pane");
    assert_eq!(
        h.root(|root, _| root.active_tab_id().map(str::to_owned)),
        Some("t1".to_owned()),
        "the session's pane is shown"
    );
}

#[gpui::test]
fn opening_a_drawer_sends_get_session_detail(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &on_dashboard(vec![working("s1")]));
    show_dashboard(&mut h);
    open_drawer(&mut h, "s1");
    let cells = drawer(&mut h, "s1").expect("the drawer is open");
    assert!(!cells.loaded, "nothing has answered yet");
}

#[gpui::test]
fn a_session_detail_reply_draws_in_the_drawer(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &on_dashboard(vec![working("s1")]));
    show_dashboard(&mut h);
    let request_id = open_drawer(&mut h, "s1");
    h.send(detail_reply("s1", &request_id, &busy_work()));

    let cells = drawer(&mut h, "s1").expect("the drawer is open");
    assert!(cells.loaded);
    let summary: Vec<(&str, &str, &str)> = cells
        .summary
        .iter()
        .map(|line| (line.window, line.text.as_str(), line.meta.as_str()))
        .collect();
    assert_eq!(
        summary,
        vec![
            ("now", "Wired the drawer", "model, 3m ago"),
            ("15–30m", "Read the design", "template, 20m ago"),
        ]
    );
    assert_eq!(cells.prompt.as_deref(), Some("Build the drawer"));
    assert_eq!(cells.work_header.as_deref(), Some("2 changed +16 −3"));
    assert_eq!(
        cells.work,
        vec![
            "abc1234 Add the drawer · 10m ago",
            "→ Done · Ship it",
            "M src/a.rs +12 −3",
            "A src/b.rs +4 −0",
            "PR #118 · checks passed",
        ]
    );
    assert!(!cells.ended);
    assert_eq!(cells.error, None);
    for part in [
        "",
        "-summary",
        "-prompt",
        "-work",
        "-go-to-pane",
        "-open-diff",
    ] {
        assert!(
            drawn(&mut h, &format!("dashboard-drawer-s1{part}")),
            "dashboard-drawer-s1{part} is drawn"
        );
    }
}

#[gpui::test]
fn a_second_click_closes_the_drawer(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &on_dashboard(vec![working("s1")]));
    show_dashboard(&mut h);
    open_drawer(&mut h, "s1");
    h.click_on("dashboard-row-s1");
    assert!(drawer(&mut h, "s1").is_none(), "the drawer is closed");
    assert!(
        detail_requests(&mut h).is_empty(),
        "closing asks for nothing"
    );
}

#[gpui::test]
fn an_ended_sessions_drawer_stays_marked_ended(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &on_dashboard(vec![working("s1")]));
    show_dashboard(&mut h);
    let request_id = open_drawer(&mut h, "s1");
    h.send(detail_reply("s1", &request_id, &busy_work()));
    assert!(!drawn(&mut h, "dashboard-drawer-s1-ended"));

    update(
        &mut h,
        session("s1").in_repo("r1").status("stopped").build(),
    );
    h.send(facts_message(vec![facts("s1").context(40).build()]));
    h.advance(std::time::Duration::from_secs(6));

    let cells = drawer(&mut h, "s1").expect("the drawer stays open");
    assert!(cells.ended, "marked ended");
    assert_eq!(cells.summary.len(), 2, "its last content is kept");
    assert_eq!(cells.work_header.as_deref(), Some("2 changed +16 −3"));
    assert!(drawn(&mut h, "dashboard-drawer-s1-ended"));
    assert!(
        detail_requests(&mut h).is_empty(),
        "an ended session's drawer asks for nothing more"
    );
}

#[gpui::test]
fn open_diff_opens_the_first_changed_file(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let s1 = session("s1")
        .members(&[("r1", "main", "D:/src/r1")])
        .status("working")
        .build();
    let mut h = Harness::with(cx, &dir, &on_dashboard(vec![s1]));
    show_dashboard(&mut h);
    let request_id = open_drawer(&mut h, "s1");
    h.send(detail_reply("s1", &request_id, &busy_work()));
    h.click_on("dashboard-drawer-s1-open-diff");
    let opens: Vec<(String, String, Option<String>, Option<String>)> = h
        .sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::OpenDiffTab {
                repo_id,
                path,
                against,
                worktree_path,
                ..
            } => Some((repo_id, path, against, worktree_path)),
            _ => None,
        })
        .collect();
    assert_eq!(
        opens,
        vec![("r1".to_owned(), "src/a.rs".to_owned(), None, None)],
        "the first changed file's diff, in the session's tree"
    );
}

#[gpui::test]
fn open_diff_is_absent_with_no_changed_files(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &on_dashboard(vec![working("s1")]));
    show_dashboard(&mut h);
    let request_id = open_drawer(&mut h, "s1");
    let landed = json!({
        "commits": [{
            "sha": "abc1234567890",
            "short_sha": "abc1234",
            "subject": "Add the drawer",
            "at": minutes_ago(10),
        }],
        "landed": { "files": 7, "added": 212, "deleted": 48 },
    });
    h.send(detail_reply("s1", &request_id, &landed));
    let cells = drawer(&mut h, "s1").expect("the drawer is open");
    assert!(!cells.open_diff);
    assert_eq!(
        cells.work_header.as_deref(),
        Some("7 files · +212 −48 landed")
    );
    assert!(drawn(&mut h, "dashboard-drawer-s1-go-to-pane"));
    assert!(!drawn(&mut h, "dashboard-drawer-s1-open-diff"));
}

#[gpui::test]
fn a_drawer_error_reply_shows_its_error(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &on_dashboard(vec![working("s1")]));
    show_dashboard(&mut h);
    let request_id = open_drawer(&mut h, "s1");
    h.send(DaemonMessage::Error {
        message: "unknown session: s1".to_owned(),
        request_id: Some(request_id),
        code: Some("NOT_FOUND".to_owned()),
    });
    let cells = drawer(&mut h, "s1").expect("the drawer stays open");
    assert_eq!(
        cells.error.as_deref(),
        Some("Couldn't load the session's detail: unknown session: s1")
    );
    assert!(drawn(&mut h, "dashboard-drawer-s1-error"));
    assert!(
        toasts(&mut h).is_empty(),
        "the drawer shows it, not a toast"
    );
}

#[gpui::test]
fn a_reply_for_a_closed_drawer_is_dropped(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &on_dashboard(vec![working("s1")]));
    show_dashboard(&mut h);
    let first = open_drawer(&mut h, "s1");
    h.click_on("dashboard-row-s1");
    h.send(detail_reply("s1", &first, &busy_work()));
    assert!(drawer(&mut h, "s1").is_none(), "the reply opens nothing");

    let second = open_drawer(&mut h, "s1");
    assert_ne!(first, second);
    h.send(detail_reply("s1", &first, &busy_work()));
    h.send(DaemonMessage::Error {
        message: "unknown session: s1".to_owned(),
        request_id: Some(first),
        code: Some("NOT_FOUND".to_owned()),
    });
    let cells = drawer(&mut h, "s1").expect("the drawer is open");
    assert!(!cells.loaded, "the closed drawer's reply is not drawn");
    assert_eq!(cells.error, None, "nor its error");
    assert!(toasts(&mut h).is_empty(), "no toast for a closed drawer");

    h.send(detail_reply("s1", &second, &busy_work()));
    assert!(drawer(&mut h, "s1").expect("still open").loaded);
}

#[gpui::test]
fn a_session_detail_with_another_request_id_leaves_the_drawer_alone(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &on_dashboard(vec![working("s1")]));
    show_dashboard(&mut h);
    let request_id = open_drawer(&mut h, "s1");
    h.send(detail_reply("s1", "someone-else-1", &busy_work()));
    let mut unasked = detail_reply("s1", "unused", &busy_work());
    if let DaemonMessage::SessionDetail { request_id, .. } = &mut unasked {
        *request_id = None;
    }
    h.send(unasked);
    let cells = drawer(&mut h, "s1").expect("the drawer is open");
    assert!(
        !cells.loaded,
        "another request's detail is not the drawer's"
    );

    h.send(detail_reply("s1", &request_id, &busy_work()));
    assert!(drawer(&mut h, "s1").expect("still open").loaded);
}

#[gpui::test]
fn a_reconnect_before_the_reply_asks_again(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &on_dashboard(vec![working("s1")]));
    show_dashboard(&mut h);
    let first = open_drawer(&mut h, "s1");
    h.send_welcome(support::PROTOCOL);
    let again = detail_requests(&mut h);
    assert_eq!(again.len(), 1, "one new request: {again:?}");
    assert_eq!(again[0].0, "s1");
    assert_ne!(again[0].1, first);

    h.send(detail_reply("s1", &again[0].1, &busy_work()));
    assert!(drawer(&mut h, "s1").expect("still open").loaded);
    h.send_welcome(support::PROTOCOL);
    assert!(
        detail_requests(&mut h).is_empty(),
        "a drawer with content keeps it until the next change"
    );
}

#[gpui::test]
fn a_drawer_whose_session_leaves_the_list_is_marked_ended(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &on_dashboard(vec![working("s1"), working("s2")]));
    show_dashboard(&mut h);
    let request_id = open_drawer(&mut h, "s1");
    h.send(detail_reply("s1", &request_id, &busy_work()));

    h.send(DaemonMessage::Sessions {
        sessions: vec![working("s2")],
    });
    h.send(facts_message(vec![facts("s1").context(40).build()]));
    h.advance(std::time::Duration::from_secs(6));

    let cells = drawer(&mut h, "s1").expect("the drawer stays open");
    assert!(cells.ended, "a session the list no longer holds has ended");
    assert_eq!(cells.summary.len(), 2, "its last content is kept");
    assert!(
        detail_requests(&mut h).is_empty(),
        "a vanished session's drawer asks for nothing more"
    );
}

#[gpui::test]
fn drawer_refetch_is_throttled_to_once_per_five_seconds(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &on_dashboard(vec![working("s1")]));
    show_dashboard(&mut h);
    let request_id = open_drawer(&mut h, "s1");
    h.send(detail_reply("s1", &request_id, &busy_work()));

    h.send(facts_message(vec![facts("s1").context(40).build()]));
    update(
        &mut h,
        session("s1")
            .in_repo("r1")
            .status("working")
            .summary_lines("Newer", &[("Newer line", Utc::now())])
            .build(),
    );
    assert!(
        detail_requests(&mut h).is_empty(),
        "a change inside 5 s of the last fetch waits"
    );
    h.advance(std::time::Duration::from_secs(4));
    assert!(detail_requests(&mut h).is_empty(), "4 s is still inside");
    h.advance(std::time::Duration::from_secs(1));
    assert_eq!(
        detail_requests(&mut h).len(),
        1,
        "the changes are fetched once, when 5 s have passed"
    );

    h.advance(std::time::Duration::from_secs(10));
    assert!(
        detail_requests(&mut h).is_empty(),
        "nothing changed, so nothing is fetched"
    );
    h.send(facts_message(vec![facts("s1").context(50).build()]));
    assert_eq!(
        detail_requests(&mut h).len(),
        1,
        "a change 5 s after the last fetch is fetched at once"
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
                track: width >= 1550.0,
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

/// A session waiting since `since` on `pending`, as the wire spells it.
fn waiting_on(id: &str, since: DateTime<Utc>, pending: serde_json::Value) -> SessionSnapshot {
    session(id)
        .in_repo("r1")
        .status("awaiting_input")
        .status_since(since)
        .pending_input(pending)
        .build()
}

fn single_question() -> serde_json::Value {
    json!({
        "kind": "question",
        "questions": [{ "question": "Which branch?", "header": "", "options": ["main", "dev"], "multi_select": false }],
    })
}

fn gh_merge() -> serde_json::Value {
    json!({ "kind": "permission", "tool_name": "Bash", "summary": "gh pr merge 118 --squash" })
}

/// The `AnswerPrompt`s the client sent since the last look.
fn answers(h: &mut Harness<'_>) -> Vec<(String, DateTime<Utc>, PromptChoice, Option<String>)> {
    h.sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::AnswerPrompt {
                session_id,
                since,
                choice,
                request_id,
            } => Some((session_id, since, choice, request_id)),
            _ => None,
        })
        .collect()
}

fn toasts(h: &mut Harness<'_>) -> Vec<(String, Option<String>)> {
    h.root(|root, _| {
        root.toasts()
            .iter()
            .map(|t| (t.title.clone(), t.detail.clone()))
            .collect()
    })
}

#[gpui::test]
fn a_choice_button_sends_answer_prompt(cx: &mut TestAppContext) {
    let since = minutes_ago(3);
    let dir = TestDir::new();
    let fixture = on_dashboard(vec![waiting_on("q1", since, single_question())]);
    let mut h = Harness::with(cx, &dir, &fixture);
    show_dashboard(&mut h);
    assert!(drawn(&mut h, "dash-ask-q1"));
    assert!(drawn(&mut h, "dash-ask-choice-q1-1"));
    answers(&mut h);
    h.click_on("dash-ask-choice-q1-2");
    let sent = answers(&mut h);
    assert_eq!(sent.len(), 1, "one answer: {sent:?}");
    let (session_id, sent_since, choice, request_id) = &sent[0];
    assert_eq!(session_id, "q1");
    assert_eq!(*sent_since, since, "chosen against the snapshot's stamp");
    assert_eq!(*choice, PromptChoice::Option { index: 1 });
    assert!(request_id.is_some(), "the answer carries a request id");
}

/// Whether resting the mouse on `selector` draws a tooltip reading `text`
/// against the mouse there. gpui puts a tooltip's corner a pixel from the
/// mouse, so one drawn for an earlier control elsewhere does not count.
fn tooltip_on(h: &mut Harness<'_>, selector: &str, text: &str) -> bool {
    let at = h.center(selector);
    h.hover(at, Modifiers::none());
    h.advance(std::time::Duration::from_millis(600));
    let tip = h.bounds(&format!("tooltip={text}"));
    let slack = px(2.0);
    tip.origin.x >= px(0.0)
        && at.x >= tip.left() - slack
        && at.x <= tip.right() + slack
        && at.y >= tip.top() - slack
        && at.y <= tip.bottom() + slack
}

#[gpui::test]
fn hovering_an_ask_choice_shows_its_description(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let described = json!({
        "kind": "question",
        "questions": [{
            "question": "Which branch?", "header": "", "options": ["main", "dev"],
            "option_descriptions": ["The release branch.", ""], "multi_select": false,
        }],
    });
    let fixture = on_dashboard(vec![waiting_on("q1", minutes_ago(3), described)]);
    let mut h = Harness::with(cx, &dir, &fixture);
    show_dashboard(&mut h);

    assert!(
        tooltip_on(&mut h, "dash-ask-choice-q1-1", "The release branch."),
        "the first choice's description shows on hover"
    );
}

#[gpui::test]
fn a_permission_shows_allow_deny_and_go_to_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = on_dashboard(vec![waiting_on("p1", minutes_ago(2), gh_merge())]);
    let mut h = Harness::with(cx, &dir, &fixture);
    show_dashboard(&mut h);
    let ask = cells(&mut h, "p1")
        .ask
        .expect("a waiting row has an ask block");
    assert_eq!(ask.label, "Permission ·");
    assert_eq!(ask.command.as_deref(), Some("gh pr merge 118 --squash"));
    assert_eq!(
        ask.choices
            .iter()
            .map(|choice| (choice.label.as_str(), choice.hint.as_deref()))
            .collect::<Vec<_>>(),
        [("Allow", None), ("Deny", None)],
        "with no chat loaded, no Always allow and no key hint"
    );
    assert!(drawn(&mut h, "dash-ask-p1-command"));
    assert!(drawn(&mut h, "dash-ask-choice-p1-1"));
    assert!(drawn(&mut h, "dash-ask-choice-p1-2"));
    assert!(!drawn(&mut h, "dash-ask-choice-p1-3"));
    assert!(drawn(&mut h, "dash-ask-go-p1"));
    answers(&mut h);
    h.click_on("dash-ask-choice-p1-2");
    let sent = answers(&mut h);
    assert_eq!(sent.len(), 1, "one answer: {sent:?}");
    assert_eq!(sent[0].2, PromptChoice::Deny);
}

#[gpui::test]
fn a_multi_select_question_shows_only_go_to_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let multi = json!({
        "kind": "question",
        "questions": [{ "question": "Which crates?", "header": "", "options": ["a", "b"], "multi_select": true }],
    });
    let fixture = on_dashboard(vec![waiting_on("m1", minutes_ago(2), multi)]);
    let mut h = Harness::with(cx, &dir, &fixture);
    show_dashboard(&mut h);
    assert!(drawn(&mut h, "dash-ask-m1"));
    assert!(drawn(&mut h, "dash-ask-go-m1"));
    assert!(!drawn(&mut h, "dash-ask-choice-m1-1"));
    assert!(!drawn(&mut h, "dash-ask-choice-m1-2"));
}

#[gpui::test]
fn a_refused_answer_toasts(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = on_dashboard(vec![waiting_on("q1", minutes_ago(3), single_question())]);
    let mut h = Harness::with(cx, &dir, &fixture);
    show_dashboard(&mut h);
    answers(&mut h);
    h.click_on("dash-ask-choice-q1-1");
    let id = answers(&mut h)
        .pop()
        .and_then(|(_, _, _, request_id)| request_id)
        .expect("the answer carries a request id");
    let refusal = "the prompt changed since";
    h.send(DaemonMessage::Error {
        message: refusal.to_owned(),
        request_id: Some(id),
        code: None,
    });
    assert_eq!(
        toasts(&mut h),
        [(
            "Couldn't answer the prompt".to_owned(),
            Some(refusal.to_owned())
        )]
    );
}

#[gpui::test]
fn a_second_click_sends_nothing_while_an_answer_is_out(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = on_dashboard(vec![waiting_on("q1", minutes_ago(3), single_question())]);
    let mut h = Harness::with(cx, &dir, &fixture);
    show_dashboard(&mut h);
    answers(&mut h);
    h.click_on("dash-ask-choice-q1-1");
    let id = answers(&mut h)
        .pop()
        .and_then(|(_, _, _, request_id)| request_id)
        .expect("the first click sends its answer");
    h.click_on("dash-ask-choice-q1-1");
    h.click_on("dash-ask-choice-q1-2");
    assert!(
        answers(&mut h).is_empty(),
        "no second answer while the first is out"
    );
    h.send(DaemonMessage::Error {
        message: "the prompt changed since".to_owned(),
        request_id: Some(id),
        code: None,
    });
    h.click_on("dash-ask-choice-q1-2");
    assert_eq!(
        answers(&mut h).len(),
        1,
        "a refusal frees the buttons again"
    );
    let moved = minutes_ago(1);
    update(&mut h, waiting_on("q1", moved, single_question()));
    h.click_on("dash-ask-choice-q1-1");
    let sent = answers(&mut h);
    assert_eq!(sent.len(), 1, "a new wait frees the buttons: {sent:?}");
    assert_eq!(sent[0].1, moved);
}

#[gpui::test]
fn go_to_pane_opens_the_session(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = on_dashboard(vec![waiting_on("s1", minutes_ago(2), gh_merge())]);
    let mut h = Harness::with(cx, &dir, &fixture);
    show_dashboard(&mut h);
    h.click_on("dash-ask-go-s1");
    assert_eq!(
        h.root(|root, _| root.active_tab_id().map(str::to_owned)),
        Some("t1".to_owned()),
        "the session's pane is shown"
    );
    assert!(answers(&mut h).is_empty(), "Go to pane answers nothing");
}

#[gpui::test]
fn band_answer_button_sends_answer_prompt(cx: &mut TestAppContext) {
    let since = minutes_ago(3);
    let dir = TestDir::new();
    let fixture = on_dashboard(vec![waiting_on("q1", since, single_question())]);
    enable_band_probe();
    let mut h = Harness::with(cx, &dir, &fixture);
    assert!(drawn(&mut h, "dashboard-band"));
    assert!(drawn(&mut h, "dashboard-band-washed"), "a wait washes it");
    assert!(drawn(&mut h, "dashboard-band-wait=q1"));
    assert!(
        drawn(&mut h, "dash-ask-q1"),
        "the wait carries its ask block"
    );
    answers(&mut h);
    h.click_on("dash-ask-choice-q1-2");
    let sent = answers(&mut h);
    assert_eq!(sent.len(), 1, "one answer from the band: {sent:?}");
    assert_eq!(sent[0].0, "q1");
    assert_eq!(sent[0].1, since, "chosen against the snapshot's stamp");
    assert_eq!(sent[0].2, PromptChoice::Option { index: 1 });
}

#[gpui::test]
fn band_shows_no_agent_is_waiting_when_empty(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = on_dashboard(vec![working("w1")]);
    enable_band_probe();
    let mut h = Harness::with(cx, &dir, &fixture);
    assert!(drawn(&mut h, "dashboard-band"));
    assert!(drawn(&mut h, "dashboard-band-empty"));
    assert!(
        !drawn(&mut h, "dashboard-band-wait=w1"),
        "a working session is not a wait"
    );
}

#[gpui::test]
fn band_calm_state_has_no_wash(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = on_dashboard(vec![working("w1")]);
    enable_band_probe();
    let mut h = Harness::with(cx, &dir, &fixture);
    assert!(drawn(&mut h, "dashboard-band-calm"));
    assert!(!drawn(&mut h, "dashboard-band-washed"), "nothing waits");
}

#[gpui::test]
fn band_wait_without_an_ask_goes_to_its_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = on_dashboard(vec![
        session("s1")
            .in_repo("r1")
            .status("awaiting_input")
            .status_since(minutes_ago(2))
            .build(),
    ]);
    enable_band_probe();
    let mut h = Harness::with(cx, &dir, &fixture);
    assert!(drawn(&mut h, "dashboard-band-wait=s1"));
    assert!(
        !drawn(&mut h, "dash-ask-s1"),
        "a screen-detected wait reports no prompt"
    );
    assert!(drawn(&mut h, "dashboard-band-go=s1"));
    h.click_on("dashboard-band-go=s1");
    assert_eq!(
        h.root(|root, _| root.active_tab_id().map(str::to_owned)),
        Some("t1".to_owned()),
        "the session's pane is shown"
    );
}

/// A wire segment of session `id`'s track.
fn segment(
    id: &str,
    kind: SegmentKind,
    activity: Option<SegmentActivity>,
    from: DateTime<Utc>,
    to: Option<DateTime<Utc>>,
) -> ActivitySegment {
    ActivitySegment {
        repo: "r1".to_owned(),
        key: format!("session-{id}"),
        kind,
        label: id.to_owned(),
        activity,
        from,
        to,
    }
}

fn stretch(
    id: &str,
    activity: SegmentActivity,
    from: DateTime<Utc>,
    to: Option<DateTime<Utc>>,
) -> ActivitySegment {
    segment(id, SegmentKind::Session, Some(activity), from, to)
}

fn send_tracks(h: &mut Harness<'_>, segments: Vec<ActivitySegment>) {
    h.send(activity_tracks_message(Utc::now(), 2, segments));
}

/// The kinds of the runs `id`'s bar draws, left to right.
fn run_kinds(h: &mut Harness<'_>, id: &str) -> Vec<RunKind> {
    cells(h, id)
        .track
        .expect("the session has a track")
        .runs
        .iter()
        .map(|run| run.kind)
        .collect()
}

#[gpui::test]
fn scripted_segments_draw_in_the_last_2h_column(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &on_dashboard(vec![working("w1")]));
    show_dashboard(&mut h);
    assert!(!drawn(&mut h, "dashboard-track-caption=w1"));

    // The working stretch is closed, so its caption does not follow the clock.
    let base = Utc::now();
    send_tracks(
        &mut h,
        vec![
            stretch(
                "w1",
                SegmentActivity::Idle,
                minutes_ago(80),
                Some(minutes_ago(70)),
            ),
            stretch(
                "w1",
                SegmentActivity::Asking,
                minutes_ago(70),
                Some(base - Duration::minutes(46)),
            ),
            stretch(
                "w1",
                SegmentActivity::Working,
                base - Duration::minutes(46),
                Some(base),
            ),
            segment("w1", SegmentKind::Hold, None, minutes_ago(5), None),
        ],
    );

    assert_eq!(
        run_kinds(&mut h, "w1"),
        [RunKind::Idle, RunKind::Asking, RunKind::Working]
    );
    let track = cells(&mut h, "w1").track.expect("a track");
    assert_eq!(track.short, "busy 46m · in gate");
    assert_eq!(track.ticks.len(), 1);
    assert!(drawn(&mut h, "dashboard-track-caption=w1"));
    let cell = h.bounds("dashboard-track=w1");
    let spark = h.bounds("dashboard-spark-w1");
    assert!(
        cell.left() >= spark.right(),
        "the column sits after the ten-minute bars"
    );
    let caption = h.bounds("dashboard-track-caption=w1");
    assert!(caption.top() >= cell.top() + px(8.0), "the bar is on top");
}

#[gpui::test]
fn an_unseen_turns_idle_stretch_draws_finished(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(
        cx,
        &dir,
        &on_dashboard(vec![working("shown"), working("unseen")]),
    );
    show_dashboard(&mut h);
    let idle = |id: &str, from, to| stretch(id, SegmentActivity::Idle, minutes_ago(from), to);
    let turn = |id: &str| {
        vec![
            idle(id, 60, Some(minutes_ago(30))),
            stretch(
                id,
                SegmentActivity::Working,
                minutes_ago(30),
                Some(minutes_ago(10)),
            ),
            idle(id, 10, None),
        ]
    };
    send_tracks(&mut h, turn("unseen"));
    assert_eq!(
        run_kinds(&mut h, "unseen"),
        [RunKind::Idle, RunKind::Working, RunKind::Idle],
        "a working session has nothing unseen"
    );

    update(&mut h, session("unseen").in_repo("r1").build());

    assert_eq!(
        run_kinds(&mut h, "unseen"),
        [RunKind::Idle, RunKind::Working, RunKind::Finished],
        "only the last stretch draws finished"
    );
}

#[gpui::test]
fn a_gap_draws_hatched_with_its_note(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &on_dashboard(vec![working("w1")]));
    show_dashboard(&mut h);
    // The working stretch is closed, so its caption does not follow the clock.
    let base = Utc::now();
    let start = base - Duration::minutes(60);
    send_tracks(
        &mut h,
        vec![
            segment("w1", SegmentKind::Gap, None, minutes_ago(130), Some(start)),
            stretch("w1", SegmentActivity::Working, start, Some(base)),
        ],
    );

    let hhmm = start.with_timezone(&Local).format("%H:%M");
    let note = format!("no record before {hhmm} · the daemon restarted");
    let track = cells(&mut h, "w1").track.expect("a track");
    assert_eq!(track.note.as_deref(), Some(note.as_str()));
    assert_eq!(track.long, format!("since {hhmm} · busy 1h00m of 2h"));
    assert_eq!(run_kinds(&mut h, "w1"), [RunKind::Gap, RunKind::Working]);
    assert!(drawn(&mut h, "dashboard-track-gap=w1"));

    let tip = format!("tooltip={note}");
    assert!(!drawn(&mut h, &tip), "the note waits for the mouse");
    let gap = h.bounds("dashboard-track-gap=w1");
    h.hover(gap.center(), Modifiers::none());
    h.advance(std::time::Duration::from_millis(600));
    assert!(drawn(&mut h, &tip), "resting on the bar shows its note");
}

#[gpui::test]
fn tracks_after_unwatch_are_dropped(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &on_dashboard(vec![working("w1")]));
    show_dashboard(&mut h);
    let segments = vec![stretch(
        "w1",
        SegmentActivity::Working,
        minutes_ago(10),
        None,
    )];
    send_tracks(&mut h, segments.clone());
    assert!(cells(&mut h, "w1").track.is_some());

    h.click_on("tab-t1");
    assert!(
        cells(&mut h, "w1").track.is_none(),
        "leaving the Dashboard clears what was held"
    );

    send_tracks(&mut h, segments);
    assert!(
        cells(&mut h, "w1").track.is_none(),
        "a message for a watch this client ended is dropped"
    );
}

#[gpui::test]
fn the_column_drops_at_narrow_widths(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &on_dashboard(vec![working("w1")]));
    h.cx.simulate_resize(size(px(1500.0), px(1200.0)));
    h.click_on("tab-d1");
    h.click_on("dashboard-view-by-status");
    h.cx.run_until_parked();
    send_tracks(
        &mut h,
        vec![stretch(
            "w1",
            SegmentActivity::Working,
            minutes_ago(10),
            None,
        )],
    );
    assert!(cells(&mut h, "w1").track.is_some());

    let width = h.bounds("dashboard-body").size.width / px(1.0);
    assert!(width < 1550.0, "the view is {width} px wide");
    assert!(!h.root(|root, _| root.dashboard_columns()).track);
    // Only the caption can say so: gpui keeps what it painted before, and the
    // frame before the first layout draws every column, the empty cell included.
    assert!(!drawn(&mut h, "dashboard-track-caption=w1"));

    h.cx.simulate_resize(size(px(2000.0), px(1200.0)));
    assert!(h.root(|root, _| root.dashboard_columns()).track);
    assert!(drawn(&mut h, "dashboard-track=w1"));
    assert!(drawn(&mut h, "dashboard-track-caption=w1"));
}

#[gpui::test]
fn no_tracks_yet_draws_an_empty_cell(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &on_dashboard(vec![working("w1"), working("w2")]));
    show_dashboard(&mut h);

    assert!(cells(&mut h, "w1").track.is_none());
    assert!(
        drawn(&mut h, "dashboard-track=w1"),
        "the cell holds its place"
    );
    assert!(!drawn(&mut h, "dashboard-track-caption=w1"));

    send_tracks(
        &mut h,
        vec![stretch(
            "w1",
            SegmentActivity::Working,
            minutes_ago(10),
            None,
        )],
    );

    assert!(drawn(&mut h, "dashboard-track-caption=w1"));
    assert!(cells(&mut h, "w2").track.is_none(), "a session not listed");
    assert!(drawn(&mut h, "dashboard-track=w2"));
    assert!(!drawn(&mut h, "dashboard-track-caption=w2"));
}

/// Opens the Dashboard in its default view, Compact; By status is never drawn,
/// so a check that its body is not drawn is not read from stale bounds.
fn show_compact(h: &mut Harness<'_>) {
    open_dashboard(h);
    assert!(drawn(h, "dashboard-compact"), "Compact is the default");
}

fn compact_blocks(h: &mut Harness<'_>) -> Vec<CompactBlock> {
    h.root(rustling_tulip_native::RootView::dashboard_compact_blocks)
}

fn compact_rows(h: &mut Harness<'_>) -> Vec<CompactRow> {
    compact_blocks(h)
        .into_iter()
        .flat_map(|block| block.rows)
        .collect()
}

/// The block of the container named `name`.
fn block_named(h: &mut Harness<'_>, name: &str) -> CompactBlock {
    compact_blocks(h)
        .into_iter()
        .find(|block| block.name == name)
        .expect("the container has a block")
}

/// The row of session `id`.
fn compact_row(h: &mut Harness<'_>, id: &str) -> CompactRow {
    compact_rows(h)
        .into_iter()
        .find(|row| row.session_id == id)
        .expect("the session has a row")
}

/// `count` sessions in repo `repo` with ids `{prefix}-{n}` in `status`.
fn sessions_in(repo: &str, prefix: &str, status: &str, count: usize) -> Vec<SessionSnapshot> {
    (0..count)
        .map(|index| {
            session(&format!("{prefix}-{index}"))
                .in_repo(repo)
                .status(status)
                .build()
        })
        .collect()
}

/// `sessions` over repos `r1` to `r4` and workspace `w1` (over `r5`, so
/// the four repos keep their own containers), with a terminal tab and the
/// Dashboard tab.
fn fleet_fixture(sessions: Vec<SessionSnapshot>) -> Fixture {
    let mut fixture = on_dashboard(sessions);
    fixture.repos = (1..=5)
        .map(|n| repo(&format!("r{n}"), &format!("D:/src/r{n}")))
        .collect();
    fixture.workspaces = vec![workspace("w1", &["r5"])];
    fixture
}

/// Two subagents, the second finished, a failed shell, a gate and a queued
/// dispatch run.
fn five_nodes() -> serde_json::Value {
    json!([
        { "key": "subagent:1", "kind": "subagent", "name": "implementer", "task": "editing footer.rs", "state": "running", "started_at": minutes_ago(4) },
        { "key": "subagent:2", "kind": "subagent", "name": "Explore", "task": "mapped status spans", "state": "finished", "started_at": minutes_ago(3) },
        { "key": "bg:1", "kind": "shell", "name": "gate: clippy", "task": "2 warnings", "state": "failed", "started_at": minutes_ago(2) },
        { "key": "gate:1", "kind": "gate", "name": "gate: nextest", "task": "41%", "state": "running", "started_at": minutes_ago(2) },
        { "key": "dispatch:1", "kind": "dispatch", "name": "deepseek: RT-771", "task": "queued", "state": "queued", "started_at": minutes_ago(1) },
    ])
}

/// The selectors of every row, chips cell and chip of `rows`.
fn row_parts(rows: &[CompactRow]) -> Vec<String> {
    rows.iter()
        .flat_map(|row| {
            let id = &row.session_id;
            let mut parts = vec![
                format!("dashboard-compact-row-{id}"),
                format!("dashboard-compact-chips-{id}"),
            ];
            parts.extend((0..row.chips.len()).map(|n| format!("dashboard-compact-chip-{id}-{n}")));
            parts
        })
        .collect()
}

/// Whether a view `width` px wide draws the issue key and the age: the
/// session cell is up to 642 px while the chips keep 260, at least 360, and
/// the whole row when under 200 px would be left beside it; the key needs 560
/// and the age 470.
fn keeps_issue_and_age(width: f32) -> (bool, bool) {
    let rows = width - 20.0 - 150.0;
    let session = if rows - 360.0 < 200.0 {
        rows
    } else {
        (rows - 260.0).clamp(360.0, 642.0)
    };
    (session >= 560.0, session >= 470.0)
}

fn saved_view(dir: &TestDir) -> serde_json::Value {
    let text = std::fs::read_to_string(dir.path().join("native-ui.json")).expect("a saved layout");
    let file: serde_json::Value = serde_json::from_str(&text).expect("native-ui.json is JSON");
    file["dashboard"]["view"].clone()
}

/// Thirty sessions over four repos and the workspace; the first three in
/// `r1` wait on the user when `waiting`, else they work.
fn thirty_sessions(waiting: bool) -> Vec<SessionSnapshot> {
    let first = if waiting { "awaiting_input" } else { "working" };
    let mut fleet = [
        sessions_in("r1", "ask", first, 3),
        sessions_in("r1", "work", "working", 5),
        sessions_in("r2", "build", "working", 8),
        sessions_in("r3", "idle", "idle", 7),
        sessions_in("r4", "port", "working", 4),
    ]
    .concat();
    fleet.extend((0..3).map(|n| {
        session(&format!("deck-{n}"))
            .in_workspace("w1")
            .status("working")
            .build()
    }));
    fleet
}

/// Opens Compact over `fleet` at 1920 by 1080 and checks its five blocks.
fn compact_at_1080p<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    fleet: Vec<SessionSnapshot>,
) -> Harness<'a> {
    let mut h = Harness::with(cx, dir, &fleet_fixture(fleet));
    show_compact(&mut h);
    h.cx.simulate_resize(size(px(1920.0), px(1080.0)));
    assert_eq!(
        compact_blocks(&mut h)
            .iter()
            .map(|block| block.name.clone())
            .collect::<Vec<_>>(),
        ["w1", "r1", "r2", "r3", "r4"],
        "four repos and the workspace"
    );
    h
}

/// Asserts each of `selectors` is drawn and ends inside the view, which
/// ends inside the 1080 px window, and that nothing scrolls.
fn drawn_inside_the_view(h: &mut Harness<'_>, selectors: &[String]) {
    let view = h.bounds("dashboard-compact");
    assert!(view.bottom() <= px(1080.0), "the view ends in the window");
    for selector in selectors {
        assert!(drawn(h, selector), "{selector} is drawn");
        let part = h.bounds(selector);
        assert!(
            part.bottom() <= view.bottom(),
            "{selector} ends at {} past the view's {}",
            part.bottom(),
            view.bottom()
        );
    }
    assert!(!drawn(h, "dashboard-body"), "no scroll container");
    assert!(!drawn(h, "dashboard-columns"), "no column header");
}

fn row_selectors(rows: &[CompactRow]) -> Vec<String> {
    rows.iter()
        .map(|row| format!("dashboard-compact-row-{}", row.session_id))
        .collect()
}

#[gpui::test]
fn twenty_five_sessions_fit_a_1080p_window_as_rows(cx: &mut TestAppContext) {
    let fleet: Vec<SessionSnapshot> = thirty_sessions(false)
        .into_iter()
        .filter(|session| !(3..8).any(|n| session.id == format!("build-{n}")))
        .collect();
    assert_eq!(fleet.len(), 25);
    let dir = TestDir::new();
    let mut h = compact_at_1080p(cx, &dir, fleet);
    let rows = compact_rows(&mut h);
    let room = h.bounds("dashboard-compact").size.height;
    assert_eq!(
        rows.len(),
        25,
        "every session has its row in a view {room:?} tall"
    );
    let blocks = compact_blocks(&mut h);
    assert!(
        blocks.iter().all(|block| !block.has_fold_row()),
        "nothing folds, so no fold row"
    );
    for block in &blocks {
        assert!(!drawn(
            &mut h,
            &format!("dashboard-compact-fold-{}", block.index)
        ));
    }
    drawn_inside_the_view(&mut h, &row_selectors(&rows));
}

#[gpui::test]
fn thirty_sessions_stay_inside_a_1080p_window_with_no_scrolling(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = compact_at_1080p(cx, &dir, thirty_sessions(false));
    let blocks = compact_blocks(&mut h);
    let rows = compact_rows(&mut h);
    let folded: usize = blocks.iter().map(|block| block.folded.len()).sum();
    assert_eq!(rows.len() + folded, 30, "every session is a row or folded");
    let mut selectors = row_selectors(&rows);
    selectors.extend(
        blocks
            .iter()
            .filter(|block| block.has_fold_row())
            .map(|block| format!("dashboard-compact-fold-{}", block.index)),
    );
    drawn_inside_the_view(&mut h, &selectors);
}

#[gpui::test]
fn thirty_sessions_with_waits_fold_and_never_clip_at_1080p(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = compact_at_1080p(cx, &dir, thirty_sessions(true));
    assert!(
        drawn(&mut h, "dashboard-queue-strip"),
        "the queue strip shows"
    );
    let blocks = compact_blocks(&mut h);
    let rows = compact_rows(&mut h);
    let folded: usize = blocks.iter().map(|block| block.folded.len()).sum();
    assert_eq!(rows.len() + folded, 30, "every session is a row or folded");
    assert_eq!(
        rows.iter().filter(|row| row.asking).count(),
        3,
        "every waiting session keeps its row"
    );
    let mut selectors = row_selectors(&rows);
    selectors.extend(
        blocks
            .iter()
            .filter(|block| block.has_fold_row())
            .map(|block| format!("dashboard-compact-fold-{}", block.index)),
    );
    drawn_inside_the_view(&mut h, &selectors);
}

#[gpui::test]
fn nothing_passes_the_right_edge_at_any_width(cx: &mut TestAppContext) {
    let fleet = vec![
        working("w1"),
        session("w2")
            .in_repo("r1")
            .status("working")
            .status_since(minutes_ago(14))
            .build(),
        session("i1")
            .in_repo("r2")
            .status_since(minutes_ago(30))
            .build(),
        session("deck")
            .in_workspace("w1")
            .status("working")
            .status_since(minutes_ago(9))
            .build(),
    ];
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fleet_fixture(fleet));
    h.cx.simulate_resize(size(px(1000.0), px(700.0)));
    h.click_on("tab-d1");
    assert!(drawn(&mut h, "dashboard-compact"), "Compact is the default");
    let long = "Running Bash · 1m: cargo nextest run --workspace ".repeat(6);
    h.send(facts_message(vec![
        facts("w1")
            .agents(five_nodes())
            .issue("RT-773")
            .now("running_tool", &long)
            .build(),
        facts("w2").issue("RT-455").build(),
        facts("deck").agents(five_nodes()).build(),
    ]));

    // Narrowest first: bounds outlive an element, so a part a wider view
    // draws could not be shown missing after it.
    for (wide, high) in [(1000.0, 700.0), (1280.0, 800.0), (1920.0, 1080.0)] {
        h.cx.simulate_resize(size(px(wide), px(high)));
        let rows = compact_rows(&mut h);
        assert_eq!(rows.len(), 4, "every session has its row at {wide}");
        let view = h.bounds("dashboard-compact");
        assert!(view.right() <= px(wide), "{view:?} in {wide}");
        for selector in row_parts(&rows) {
            assert!(drawn(&mut h, &selector), "{selector} is drawn at {wide}");
            let part = h.bounds(&selector);
            assert!(
                part.right() <= view.right(),
                "{selector} ends at {} past the view's {} at {wide}",
                part.right(),
                view.right()
            );
        }
        let (issue, age) = keeps_issue_and_age(view.size.width / px(1.0));
        for row in &rows {
            let part = |name: &str| format!("dashboard-compact-row-{}-{name}", row.session_id);
            assert_eq!(drawn(&mut h, &part("issue")), issue, "the key at {wide}");
            assert_eq!(drawn(&mut h, &part("age")), age, "the age at {wide}");
        }
    }
}

#[gpui::test]
fn a_working_session_draws_its_agent_tree_as_chips(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &on_dashboard(vec![working("w1")]));
    show_compact(&mut h);
    h.send(facts_message(vec![
        facts("w1").agents(five_nodes()).build(),
    ]));

    let row = compact_row(&mut h, "w1");
    let keys: Vec<String> = row.chips.iter().map(|chip| chip.key.clone()).collect();
    assert_eq!(
        keys,
        ["subagent:1", "subagent:2", "bg:1", "gate:1", "dispatch:1"]
    );
    let nested: Vec<String> = cells(&mut h, "w1")
        .nested
        .into_iter()
        .map(|nested| nested.key)
        .collect();
    assert_eq!(keys, nested, "the nodes and order By status nests");
    assert_eq!(
        row.chips.iter().map(|chip| chip.kind).collect::<Vec<_>>(),
        [
            ChipKind::Agent,
            ChipKind::Agent,
            ChipKind::Shell,
            ChipKind::Shell,
            ChipKind::Dispatch
        ]
    );
    assert_eq!(row.chips[1].state, AgentState::Finished);
    assert_eq!(row.chips[2].state, AgentState::Failed);

    let cell = h.bounds("dashboard-compact-chips-w1");
    for n in 0..5 {
        let selector = format!("dashboard-compact-chip-w1-{n}");
        assert!(drawn(&mut h, &selector), "{selector} is drawn");
        let chip = h.bounds(&selector);
        assert!(
            chip.left() >= cell.left()
                && chip.right() <= cell.right()
                && chip.top() >= cell.top()
                && chip.bottom() <= cell.bottom(),
            "{selector} at {chip:?} inside {cell:?}"
        );
    }
    let block = compact_blocks(&mut h).remove(0);
    assert!(!block.has_fold_row());
    assert!(
        !drawn(&mut h, &format!("dashboard-compact-fold-{}", block.index)),
        "nothing folded or ended, no fold row"
    );
}

#[gpui::test]
fn the_repo_cell_names_its_container_and_tallies_its_sessions(cx: &mut TestAppContext) {
    let fleet = vec![
        session("ask")
            .in_repo("r1")
            .status("awaiting_input")
            .build(),
        working("work"),
        session("idle").in_repo("r1").build(),
        session("idle2").in_repo("r1").build(),
        session("deck").in_workspace("w1").status("working").build(),
    ];
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fleet_fixture(fleet));
    show_compact(&mut h);

    let blocks = compact_blocks(&mut h);
    assert_eq!(blocks.len(), 2, "only containers with a listed session");
    assert!(
        blocks.windows(2).all(|pair| pair[0].index < pair[1].index),
        "in the sidebar's order"
    );
    let repo = block_named(&mut h, "r1");
    assert!(!repo.is_workspace());
    assert_eq!(repo.tag(), "REPO");
    assert_eq!(
        repo.tally,
        Tally {
            asking: 1,
            working: 1,
            done: 0,
            idle: 2
        }
    );
    assert_eq!(repo.sessions(), 4);
    let space = block_named(&mut h, "w1");
    assert!(space.is_workspace());
    assert_eq!(space.tag(), "WORKSPACE");
    assert_eq!(space.tally.working, 1);

    for block in [&repo, &space] {
        let name = format!("dashboard-compact-repo-{}", block.index);
        assert!(drawn(&mut h, &name), "{name} is drawn");
        assert!(drawn(&mut h, &format!("{name}-name")), "{name}'s name");
        let tally = block.tally;
        for (state, count) in [
            ("asking", tally.asking),
            ("working", tally.working),
            ("done", tally.done),
            ("idle", tally.idle),
        ] {
            assert_eq!(
                drawn(&mut h, &format!("{name}-tally-{state}")),
                count > 0,
                "{name}'s {state} count {count}"
            );
        }
    }
}

#[gpui::test]
fn a_short_window_folds_idle_sessions_and_draws_nothing_below_the_view(cx: &mut TestAppContext) {
    let mut fleet = vec![
        session("ask")
            .in_repo("r1")
            .status("awaiting_input")
            .build(),
        session("busy").in_repo("r1").build(),
    ];
    fleet.extend(sessions_in("r1", "idle", "idle", 12));
    fleet.extend(sessions_in("r2", "work", "working", 3));
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fleet_fixture(fleet));
    show_compact(&mut h);
    h.send(facts_message(vec![
        facts("busy")
            .agents(json!([
                { "key": "bg:1", "kind": "shell", "name": "npm dev", "task": ":5174", "state": "running", "started_at": minutes_ago(9) },
            ]))
            .build(),
    ]));
    h.cx.simulate_resize(size(px(1920.0), px(500.0)));

    let r1 = block_named(&mut h, "r1");
    assert_eq!(
        r1.folded.len(),
        12,
        "every idle session with nothing running"
    );
    assert_eq!(r1.idle_text().as_deref(), Some("+12 idle"));
    assert_eq!(
        r1.rows
            .iter()
            .map(|row| row.session_id.as_str())
            .collect::<Vec<_>>(),
        ["ask", "busy"],
        "Needs you and the idle session with a running shell stay"
    );
    assert_eq!(
        block_named(&mut h, "r2").rows.len(),
        3,
        "Working never folds"
    );
    let fold = format!("dashboard-compact-fold-{}", r1.index);
    assert!(drawn(&mut h, &fold), "the fold row is drawn");
    assert!(drawn(&mut h, &format!("{fold}-idle")));

    let view = h.bounds("dashboard-compact");
    assert!(view.bottom() <= px(500.0), "the view ends in the window");
    let mut selectors: Vec<String> = compact_rows(&mut h)
        .iter()
        .map(|row| format!("dashboard-compact-row-{}", row.session_id))
        .collect();
    selectors.push(fold);
    for selector in selectors {
        assert!(drawn(&mut h, &selector), "{selector} is drawn");
        assert!(
            h.bounds(&selector).bottom() <= view.bottom(),
            "{selector} ends below the view"
        );
    }
}

#[gpui::test]
fn an_ended_session_is_counted_in_its_fold_row(cx: &mut TestAppContext) {
    let fleet = vec![
        working("w"),
        session("gone").in_repo("r1").exited(0).build(),
        session("other").in_repo("r2").status("working").build(),
    ];
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fleet_fixture(fleet));
    show_compact(&mut h);

    let r1 = block_named(&mut h, "r1");
    assert_eq!(r1.rows.len(), 1, "the ended session has no row");
    assert_eq!(r1.ended_text().as_deref(), Some("+1 ended"));
    assert_eq!(r1.idle_text(), None, "nothing folded");
    let fold = format!("dashboard-compact-fold-{}", r1.index);
    assert!(drawn(&mut h, &fold));
    assert!(drawn(&mut h, &format!("{fold}-ended")));
    assert!(!drawn(&mut h, &format!("{fold}-idle")));
    assert!(!drawn(&mut h, "dashboard-compact-row-gone"));
    let r2 = block_named(&mut h, "r2");
    assert!(!r2.has_fold_row());
    assert!(
        !drawn(&mut h, &format!("dashboard-compact-fold-{}", r2.index)),
        "no fold row with nothing folded or ended"
    );
}

#[gpui::test]
fn compact_rows_still_open_their_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &on_dashboard(vec![working("s1")]));
    show_compact(&mut h);
    h.sent();
    h.click_on("dashboard-compact-row-s1");
    assert_eq!(
        h.root(|root, _| root.active_tab_id().map(str::to_owned)),
        Some("t1".to_owned()),
        "the session's pane is shown"
    );
    assert!(
        detail_requests(&mut h).is_empty(),
        "a Compact row opens no drawer"
    );
    assert!(drawer(&mut h, "s1").is_none());
}

#[gpui::test]
fn the_view_toggle_is_remembered(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = on_dashboard(vec![working("s1")]);
    let mut h = Harness::with(cx, &dir, &fixture);
    show_compact(&mut h);
    assert!(drawn(&mut h, "dashboard-view-compact"));

    h.click_on("dashboard-view-by-status");
    assert_eq!(saved_view(&dir), "by_status", "saved at once");
    assert!(drawn(&mut h, "dashboard-body"));

    h.keys("escape");
    h.keys("tab");
    h.keys("left");
    assert_eq!(
        saved_view(&dir),
        "compact",
        "the keyboard reaches the toggle"
    );
    h.keys("enter");
    assert_eq!(saved_view(&dir), "by_status", "Enter flips the view");
    h.keys("escape");
    h.keys("left");
    assert_eq!(saved_view(&dir), "by_status", "Escape left the toggle");
    drop(h);

    let mut h = Harness::with(cx, &dir, &fixture);
    open_dashboard(&mut h);
    assert!(drawn(&mut h, "dashboard-body"), "the choice came back");
    assert!(!drawn(&mut h, "dashboard-compact"));
}

#[gpui::test]
fn a_held_enter_on_the_toggle_does_not_flip_the_view(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &on_dashboard(vec![working("s1")]));
    show_compact(&mut h);
    h.keys("tab");
    h.held("enter");
    h.held("enter");
    assert!(
        !drawn(&mut h, "dashboard-body"),
        "a held Enter's repeats flip nothing"
    );
    h.keys("enter");
    assert!(drawn(&mut h, "dashboard-body"), "a first Enter flips it");
    assert_eq!(saved_view(&dir), "by_status");
}

#[gpui::test]
fn a_queue_chip_in_compact_opens_by_status_at_the_waiting_row(cx: &mut TestAppContext) {
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
    h.click_on("dashboard-view-compact");
    assert_eq!(saved_view(&dir), "compact");

    h.click_on("dashboard-queue-ask");
    assert_eq!(saved_view(&dir), "by_status", "the chip opened By status");
    let (body, row) = (h.bounds("dashboard-body"), h.bounds("dashboard-row-ask"));
    assert!(
        row.top() >= body.top() && row.bottom() <= body.bottom(),
        "the chip scrolled its row into view: {row:?} in {body:?}"
    );
}

#[gpui::test]
fn the_view_toggle_stays_inside_a_narrow_view_with_the_day_figures(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &on_dashboard(vec![working("w1")]));
    show_compact(&mut h);
    h.cx.simulate_resize(size(px(1000.0), px(800.0)));
    h.send(DaemonMessage::DashboardFacts {
        facts: Vec::new(),
        totals: protocol::DayTotals {
            cost_usd: 182.46,
            tokens: 31_400_000,
            commits: 48,
            issues_done: 12,
            releases: vec![
                protocol::ReleaseProgress {
                    name: "vNext".to_owned(),
                    done: 11,
                    total: 18,
                },
                protocol::ReleaseProgress {
                    name: "vNext+1".to_owned(),
                    done: 3,
                    total: 21,
                },
            ],
        },
        linear_synced_at: Some(minutes_ago(2)),
    });
    assert!(
        h.root(|root, _| root.dashboard_figures()).is_some(),
        "the day figures are known"
    );
    assert!(drawn(&mut h, "dashboard-figures"));
    let view = h.bounds("dashboard-view");
    let toggle = h.bounds("dashboard-view-by-status");
    assert!(drawn(&mut h, "dashboard-view-by-status"));
    let header: Vec<_> = [
        "dashboard-header",
        "dashboard-pill-need-you",
        "dashboard-pill-done",
        "dashboard-figures",
        "dashboard-view-compact",
    ]
    .into_iter()
    .map(|selector| (selector, h.bounds(selector)))
    .collect();
    assert!(
        toggle.right() <= view.right(),
        "the toggle ends at {} past the view's {}: {header:?}",
        toggle.right(),
        view.right()
    );
}

#[gpui::test]
fn a_stuck_session_draws_its_flags_in_its_now_cell(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &on_dashboard(vec![working("s1")]));
    show_compact(&mut h);
    h.send(facts_message(vec![
        facts("s1")
            .stuck(json!([
                { "kind": "tool_failures", "count": 65_535 },
                { "kind": "context_high" },
            ]))
            .build(),
    ]));

    assert_eq!(
        compact_row(&mut h, "s1").stuck,
        ["⚑ 65535 tool calls failed in a row", "⚑ context high"]
    );
    let now = h.bounds("dashboard-compact-row-s1-now");
    for index in [0, 1] {
        let selector = format!("dashboard-compact-row-s1-flag-{index}");
        assert!(drawn(&mut h, &selector), "{selector} is drawn");
        let flag = h.bounds(&selector);
        assert!(
            flag.left() >= now.left() && flag.right() <= now.right(),
            "{selector} at {flag:?} inside the now cell {now:?}"
        );
    }
}

#[gpui::test]
fn a_session_without_facts_draws_its_label_state_and_empty_cells(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(
        cx,
        &dir,
        &on_dashboard(vec![session("bare").in_repo("r1").build(), working("w")]),
    );
    show_compact(&mut h);

    let bare = compact_row(&mut h, "bare");
    assert_eq!(bare.state, "Idle");
    assert_eq!(bare.issue, "", "no key reads empty, never a dash");
    assert_eq!(bare.age, None);
    assert_eq!(bare.chips, Vec::new(), "no facts, no chips");
    assert_eq!(compact_row(&mut h, "w").now, "Thinking");
    for part in ["glyph", "label", "state", "now", "issue", "age"] {
        let selector = format!("dashboard-compact-row-bare-{part}");
        assert!(drawn(&mut h, &selector), "{selector} is drawn");
    }
    assert!(drawn(&mut h, "dashboard-compact-chips-bare"));
    assert!(!drawn(&mut h, "dashboard-compact-chip-bare-0"), "no chip");

    h.send(facts_message(vec![facts("bare").issue("RT-1").build()]));
    assert_eq!(
        compact_row(&mut h, "bare").issue,
        "RT-1",
        "a key reads once known"
    );
}

#[gpui::test]
fn an_empty_fleet_draws_the_calm_line(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &on_dashboard(Vec::new()));
    open_dashboard(&mut h);
    assert_eq!(compact_blocks(&mut h), Vec::new());
    assert!(drawn(&mut h, "dashboard-compact-empty"));
    assert_eq!(COMPACT_EMPTY_TEXT, "No agents running.");
}

#[gpui::test]
fn tab_stays_in_a_focused_text_field(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &on_dashboard(vec![working("s1")]));
    open_dashboard(&mut h);
    h.keys("tab");
    h.keys("right");
    assert_eq!(
        saved_view(&dir),
        "by_status",
        "the pane left behind does not hold the keyboard"
    );
    h.keys("escape");

    h.right_click_on("tab-t1");
    h.click_on("tab-menu-rename");
    assert_eq!(
        h.root(|root, _| root.renaming_tab().map(str::to_owned)),
        Some("t1".to_owned()),
        "the rename field is open"
    );
    h.keys("tab");
    h.keys("left");
    assert_eq!(saved_view(&dir), "by_status", "the field kept Tab");
}

#[gpui::test]
fn each_block_is_as_tall_as_the_model_estimates(cx: &mut TestAppContext) {
    let fleet = [
        sessions_in("r1", "one", "idle", 1),
        sessions_in("r2", "two", "working", 2),
        sessions_in("r3", "eight", "idle", 8),
    ]
    .concat();
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fleet_fixture(fleet));
    show_compact(&mut h);
    h.cx.simulate_resize(size(px(1920.0), px(1080.0)));

    let blocks = compact_blocks(&mut h);
    assert_eq!(
        blocks
            .iter()
            .map(|block| block.rows.len())
            .collect::<Vec<_>>(),
        [1, 2, 8],
        "blocks of 1, 2 and 8 rows, nothing folded"
    );
    for block in &blocks {
        let estimate = h.root(|root, cx| root.dashboard_compact_block_h(block, cx));
        let selector = format!("dashboard-compact-repo-{}", block.index);
        let height = h.bounds(&selector).size.height / px(1.0);
        assert!(
            (height - estimate).abs() <= 1.0,
            "{selector} of {} rows is drawn {height} px tall, estimated {estimate}",
            block.rows.len()
        );
    }
}

#[gpui::test]
fn thirty_sessions_stay_inside_a_1080p_window_at_a_larger_text_size(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = compact_at_1080p(cx, &dir, thirty_sessions(false));
    h.root_update(|root, _, cx| root.set_ui_text_size(3, cx));

    let blocks = compact_blocks(&mut h);
    let rows = compact_rows(&mut h);
    let folded: usize = blocks.iter().map(|block| block.folded.len()).sum();
    assert_eq!(rows.len() + folded, 30, "every session is a row or folded");
    let mut selectors = row_selectors(&rows);
    selectors.extend(
        blocks
            .iter()
            .filter(|block| block.has_fold_row())
            .map(|block| format!("dashboard-compact-fold-{}", block.index)),
    );
    drawn_inside_the_view(&mut h, &selectors);
}

#[gpui::test]
fn a_narrow_view_keeps_the_label_and_state_inside_it(cx: &mut TestAppContext) {
    let fleet = vec![waiting_on("p1", minutes_ago(2), gh_merge()), working("w1")];
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fleet_fixture(fleet));
    show_compact(&mut h);
    h.cx.simulate_resize(size(px(1000.0), px(700.0)));
    let beside = 1000.0 - h.bounds("dashboard-compact").size.width / px(1.0);
    h.cx.simulate_resize(size(px(beside + 440.0), px(700.0)));

    let rows = compact_rows(&mut h);
    assert_eq!(rows.len(), 2, "both sessions have a row");
    let view = h.bounds("dashboard-compact");
    let width = view.size.width / px(1.0);
    assert!((430.0..=450.0).contains(&width), "a view {width} px wide");
    for row in &rows {
        for part in ["label", "state"] {
            let selector = format!("dashboard-compact-row-{}-{part}", row.session_id);
            assert!(drawn(&mut h, &selector), "{selector} is drawn");
            let right = h.bounds(&selector).right();
            assert!(
                right <= view.right(),
                "{selector} ends at {right} past the view's {}",
                view.right()
            );
        }
    }
}

#[gpui::test]
fn the_state_column_fits_permission_whole(cx: &mut TestAppContext) {
    #[cfg(debug_assertions)]
    rustling_tulip_native::ellipsis::enable_probe();
    let dir = TestDir::new();
    let fixture = on_dashboard(vec![waiting_on("p1", minutes_ago(2), gh_merge())]);
    let mut h = Harness::with(cx, &dir, &fixture);
    show_compact(&mut h);

    assert_eq!(compact_row(&mut h, "p1").state, "Permission");
    let measured = h.root_update(|_, window, cx| {
        let text = "Permission";
        let run = TextRun {
            len: text.len(),
            font: Font {
                weight: fonts::ui_weight(cx, FontWeight::SEMIBOLD),
                ..font(fonts::ui_family(cx))
            },
            color: black(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        window
            .text_system()
            .shape_line(text.into(), fonts::ui_px(cx, 12.0), &[run], None)
            .width
    });
    // A cell at least as wide as the shaped word never ends it in `…`.
    let width = h.bounds("dashboard-compact-row-p1-state").size.width;
    assert!(
        width >= measured,
        "the state cell is {width} wide, the word {measured}"
    );
    #[cfg(debug_assertions)]
    assert_eq!(
        rustling_tulip_native::ellipsis::drawn_text("dashboard-compact-row-p1-state-text")
            .as_deref(),
        Some("Permission"),
        "the word is drawn whole, with no ellipsis"
    );
}

#[gpui::test]
fn an_idle_session_shows_its_state_word_once(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = on_dashboard(vec![session("calm").in_repo("r1").build()]);
    let mut h = Harness::with(cx, &dir, &fixture);
    show_compact(&mut h);

    let row = compact_row(&mut h, "calm");
    assert_eq!(row.state, "Idle");
    assert_eq!(row.now, "", "the now line does not repeat the state word");
    assert!(drawn(&mut h, "dashboard-compact-row-calm-now"));
    assert!(
        !drawn(&mut h, "dashboard-compact-row-calm-now-text"),
        "the now cell draws no text"
    );
}

#[gpui::test]
fn escape_on_the_toggle_disarms_a_tab_close(cx: &mut TestAppContext) {
    let closes = |h: &mut Harness<'_>| {
        h.sent()
            .iter()
            .filter(|msg| matches!(msg, ClientMessage::CloseTab { tab_id } if tab_id == "t1"))
            .count()
    };
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &on_dashboard(vec![working("s1")]));
    show_compact(&mut h);
    h.sent();

    h.click_on("tab-close-t1");
    assert_eq!(closes(&mut h), 0, "the first click only arms");
    h.keys("tab");
    h.keys("right");
    assert_eq!(
        saved_view(&dir),
        "by_status",
        "the toggle holds the keyboard"
    );
    h.keys("escape");
    h.click_on("tab-close-t1");
    assert_eq!(
        closes(&mut h),
        0,
        "Escape disarmed the close, so this click arms again"
    );
}
