//! Activity rail and source-control panel specs: switching and folding the
//! panel, the persisted choice, status seeding and refresh, the badge, the
//! sections, the repo picker and the Settings gear.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

#[expect(dead_code, reason = "each spec file uses its own share of the helper")]
mod support;

use gpui::{Modifiers, TestAppContext, point, px};
use protocol::{ClientMessage, DaemonMessage};
use rustling_tulip_native::{Activity, ScContext, ScPanel, ScSectionRow};
use serde_json::json;
use support::{Fixture, Harness, TestDir, repo, session};

const AUTO: &str = "Auto · follow active pane";
const NO_ACTIVE_PANE_TIP: &str = "No pane is focused right now — falling back to the first registered repo. Pick a session in the sidebar to follow it, or pin a repo from the picker above.";

/// A daemon status for `(repo_id, worktree)` with these staged and unstaged
/// paths.
fn status(
    repo_id: &str,
    worktree: Option<&str>,
    staged: &[&str],
    changes: &[&str],
) -> DaemonMessage {
    let files = |paths: &[&str]| {
        paths
            .iter()
            .map(|path| json!({ "path": path, "status": "M", "from_path": null }))
            .collect::<Vec<_>>()
    };
    serde_json::from_value(json!({
        "type": "repo_status",
        "repo_id": repo_id,
        "index_changes": files(staged),
        "worktree_changes": files(changes),
        "worktree_path": worktree,
    }))
    .expect("status fixture")
}

/// The status requests the client sent since the last read, as
/// `(repo_id, worktree_path)`.
fn status_requests(h: &mut Harness<'_>) -> Vec<(String, Option<String>)> {
    h.sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::RepoStatus {
                repo_id,
                worktree_path,
                ..
            } => Some((repo_id, worktree_path)),
            _ => None,
        })
        .collect()
}

fn main_tree(repo_id: &str) -> (String, Option<String>) {
    (repo_id.to_owned(), None)
}

fn panel(h: &mut Harness<'_>) -> ScPanel {
    h.root(|root, _| root.source_control_panel())
}

fn activity(h: &mut Harness<'_>) -> Activity {
    h.root(|root, _| root.activity())
}

fn collapsed(h: &mut Harness<'_>) -> bool {
    h.root(|root, _| root.sidebar_collapsed())
}

fn badge(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.activity_badge())
}

fn saved_ui(dir: &TestDir) -> serde_json::Value {
    serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("native-ui.json")).expect("native-ui.json"),
    )
    .expect("native-ui.json is JSON")
}

fn two_repos() -> Fixture {
    Fixture {
        repos: vec![repo("r1", "D:/src/r1"), repo("r2", "D:/src/r2")],
        ..Fixture::default()
    }
}

#[gpui::test]
fn the_rail_switches_to_source_control_and_persists(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = two_repos();
    let mut h = Harness::with(cx, &dir, &fixture);
    assert_eq!(activity(&mut h), Activity::Sessions, "the default");
    assert!(h.in_model("sidebar-panel"));

    h.click_on("activity-source-control");
    assert_eq!(activity(&mut h), Activity::SourceControl);
    assert!(!collapsed(&mut h));
    assert!(!h.in_model("sidebar-panel"), "the sessions panel gave way");
    assert!(h.in_model("sc-panel"));
    let rail = h.bounds("activity-rail");
    let sc = h.bounds("sc-panel");
    assert_eq!(rail.size.width, px(40.0));
    assert_eq!(
        sc.origin.x,
        rail.right(),
        "the panel sits right of the rail"
    );
    assert_eq!(sc.size.width, px(280.0), "the sessions panel's width");
    assert_eq!(saved_ui(&dir)["activity"], json!("source_control"));
    drop(h);

    let mut h = Harness::with(cx, &dir, &fixture);
    assert_eq!(activity(&mut h), Activity::SourceControl, "restored");
    assert!(h.in_model("sc-panel"));
    assert_eq!(h.bounds("sc-panel").size.width, px(280.0));
}

#[gpui::test]
fn clicking_the_active_item_collapses_and_again_expands(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());

    h.click_on("activity-sessions");
    assert!(collapsed(&mut h), "the active item folds the panel");
    assert_eq!(activity(&mut h), Activity::Sessions);
    assert!(!h.in_model("sidebar-panel"));
    h.click_on("activity-sessions");
    assert!(!collapsed(&mut h), "and unfolds it");

    h.click_on("activity-source-control");
    assert_eq!(activity(&mut h), Activity::SourceControl);
    assert!(!collapsed(&mut h), "the other item switches, open");
    h.click_on("activity-source-control");
    assert!(collapsed(&mut h));

    h.click_on("activity-sessions");
    assert_eq!(activity(&mut h), Activity::Sessions);
    assert!(
        !collapsed(&mut h),
        "the other item on a folded panel switches and unfolds it"
    );
    assert!(h.in_model("sidebar-panel"));
    assert_eq!(saved_ui(&dir)["sidebar_collapsed"], json!(false));
}

#[gpui::test]
fn ctrl_b_toggles_the_panel_and_keeps_the_rail(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("s1").build());
    fixture.repos = vec![repo("r1", "D:/src/r1")];
    let mut h = Harness::with(cx, &dir, &fixture);
    h.answer_scrollback("s1", b"");
    h.click_on("activity-source-control");
    let sc = h.bounds("sc-panel");
    h.click(
        point(sc.center().x, sc.bottom() - px(10.0)),
        Modifiers::none(),
    );

    h.keys("ctrl-b");
    assert!(collapsed(&mut h), "folded");
    assert_eq!(
        activity(&mut h),
        Activity::SourceControl,
        "Ctrl+B leaves the choice alone"
    );
    assert!(!h.in_model("sc-panel"));
    let rail = h.bounds("activity-rail");
    assert_eq!((rail.origin.x, rail.size.width), (px(0.0), px(40.0)));
    let item = h.center("activity-source-control");
    assert!(rail.contains(&item), "the rail still holds its items");

    assert!(
        h.sent_input("s1").is_empty(),
        "Ctrl+B outside the terminal is no input"
    );

    h.keys("ctrl-b");
    assert_eq!(
        h.sent_input("s1"),
        [0x02],
        "folding handed the keyboard to the terminal, which takes Ctrl+B"
    );
    assert!(collapsed(&mut h));
    h.click_on("activity-source-control");
    assert!(!collapsed(&mut h), "shown again");
    assert_eq!(activity(&mut h), Activity::SourceControl);
}

#[gpui::test]
fn repos_are_seeded_with_a_main_tree_status_request(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = two_repos();
    let mut h = Harness::with(cx, &dir, &fixture);
    assert_eq!(
        status_requests(&mut h),
        [main_tree("r1"), main_tree("r2")],
        "one main-tree request per repo, with the panel not even shown"
    );

    h.send(DaemonMessage::Repos {
        repos: fixture.repos.clone(),
    });
    assert!(
        status_requests(&mut h).is_empty(),
        "a request out is not repeated"
    );

    h.send(status("r1", None, &[], &[]));
    h.send(DaemonMessage::Repos {
        repos: fixture.repos.clone(),
    });
    assert!(
        status_requests(&mut h).is_empty(),
        "a loaded status is not asked for again"
    );
}

#[gpui::test]
fn a_focused_worktree_session_requests_its_worktree_status(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("s1").worktree("r1").build());
    fixture.repos = vec![repo("r1", "D:/src/r1")];
    let mut h = Harness::with(cx, &dir, &fixture);

    let mut requests = status_requests(&mut h);
    requests.sort();
    assert_eq!(
        requests,
        [
            main_tree("r1"),
            ("r1".to_owned(), Some("C:/wt/x".to_owned()))
        ],
        "the worktree for the section, the main tree for the badge"
    );
    h.pty_raw("s1", b"output");
    assert!(
        status_requests(&mut h).is_empty(),
        "terminal output asks for no status"
    );
    h.click_on("activity-source-control");
    let sections = panel(&mut h).sections;
    assert_eq!(sections.len(), 1);
    assert_eq!(sections[0].id, "r1::C:/wt/x");
    assert_eq!(sections[0].title, "r1 · wt/x");
}

#[gpui::test]
fn the_badge_sums_distinct_paths_of_the_focused_session_and_shows_99_plus(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let s1 = session("s1")
        .members(&[("r1", "main", "D:/src/r1"), ("r2", "feat", "C:/wt/r2")])
        .build();
    let mut fixture = Fixture::single(s1);
    fixture.repos = vec![repo("r1", "D:/src/r1"), repo("r2", "D:/src/r2")];
    let mut h = Harness::with(cx, &dir, &fixture);
    assert_eq!(badge(&mut h), None, "nothing loaded");
    assert!(!h.in_model("activity-badge"));

    h.send(status("r1", None, &["a.rs", "b.rs"], &["b.rs", "c.rs"]));
    h.send(status("r2", Some("C:/wt/r2"), &[], &["x.rs"]));
    h.send(status("r2", None, &["n1", "n2", "n3", "n4", "n5"], &[]));
    assert_eq!(
        badge(&mut h).as_deref(),
        Some("4"),
        "b.rs counts once, and r2's main tree is not a member"
    );
    assert!(h.in_model("activity-badge"));
    assert!(h.bounds("activity-badge").size.height > px(0.0));

    let many: Vec<String> = (0..120).map(|i| format!("f{i}.rs")).collect();
    let many: Vec<&str> = many.iter().map(String::as_str).collect();
    h.send(status("r1", None, &[], &many));
    assert_eq!(badge(&mut h).as_deref(), Some("99+"));
    assert_eq!(h.root(|root, _| root.sc_badge()), 121);

    h.send(status("r1", None, &[], &[]));
    h.send(status("r2", Some("C:/wt/r2"), &[], &[]));
    assert_eq!(badge(&mut h), None, "hidden at 0");
    assert!(!h.in_model("activity-badge"));
}

#[gpui::test]
fn the_badge_counts_a_worktree_and_the_main_tree_separately(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("s1").worktree("r1").build());
    fixture.repos = vec![repo("r1", "D:/src/r1")];
    let mut h = Harness::with(cx, &dir, &fixture);
    h.click_on("activity-source-control");

    h.send(status("r1", Some("C:/wt/x"), &[], &["w.rs"]));
    h.send(status("r1", None, &["a.rs"], &["b.rs", "c.rs"]));
    assert_eq!(
        badge(&mut h).as_deref(),
        Some("1"),
        "the focused worktree's count, untouched by the main tree's status"
    );
    assert_eq!(panel(&mut h).sections[0].body, "1 changed");

    h.send(DaemonMessage::SessionRemoved {
        session_id: "s1".to_owned(),
    });
    assert_eq!(
        badge(&mut h).as_deref(),
        Some("3"),
        "with no members, the main tree's own count"
    );
    let sections = panel(&mut h).sections;
    assert_eq!(sections[0].id, "r1::");
    assert_eq!(sections[0].body, "3 changed");
}

#[gpui::test]
fn sections_show_loading_then_clean_then_count(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    h.click_on("activity-source-control");
    let row = |id: &str, count: Option<usize>, body: &str| ScSectionRow {
        id: id.to_owned(),
        title: "r1".to_owned(),
        count,
        body: body.to_owned(),
    };

    assert_eq!(panel(&mut h).sections, [row("r1::", None, "loading…")]);
    assert!(h.in_model("sc-section-r1::"));
    assert!(h.in_model("sc-section-body-r1::"));
    assert!(h.bounds("sc-section-body-r1::").size.height > px(0.0));
    assert!(
        !h.in_model("sc-section-r2::"),
        "only the first repo, unpinned"
    );

    h.send(status("r1", None, &[], &[]));
    assert_eq!(
        panel(&mut h).sections,
        [row("r1::", None, "working tree clean")]
    );

    h.send(status("r1", None, &["a.rs"], &["a.rs", "b.rs"]));
    assert_eq!(
        panel(&mut h).sections,
        [row("r1::", Some(2), "2 changed")],
        "distinct paths, shown beside the name"
    );
    assert_eq!(
        panel(&mut h).context,
        Some(ScContext {
            text: "r1 · no active pane".to_owned(),
            tooltip: Some(NO_ACTIVE_PANE_TIP),
        })
    );
}

#[gpui::test]
fn picker_pins_a_repo_and_auto_returns(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("s1").build());
    fixture.repos = vec![repo("r1", "D:/src/r1"), repo("r2", "D:/src/r2")];
    let mut h = Harness::with(cx, &dir, &fixture);
    h.click_on("activity-source-control");
    assert_eq!(panel(&mut h).picker, Some(format!("{AUTO} ▾")));

    h.click_on("sc-picker");
    assert!(h.root(|root, _| root.sc_picker_open()));
    let rows: Vec<(String, String, bool)> = h.root(|root, _| {
        root.sc_picker_rows()
            .into_iter()
            .map(|row| (row.selector, row.label, row.checked))
            .collect()
    });
    assert_eq!(
        rows,
        [
            ("sc-picker-auto".to_owned(), AUTO.to_owned(), true),
            ("sc-picker-r1".to_owned(), "r1".to_owned(), false),
            ("sc-picker-r2".to_owned(), "r2".to_owned(), false),
        ]
    );
    let menu = h.bounds("sc-picker-menu");
    let button = h.bounds("sc-picker");
    assert!(
        menu.top() >= button.bottom(),
        "the menu hangs under the button"
    );

    h.click_on("sc-picker-r2");
    assert!(!h.root(|root, _| root.sc_picker_open()));
    assert_eq!(
        h.root(|root, _| root.sc_pinned_repo().map(str::to_owned)),
        Some("r2".to_owned())
    );
    let pinned = panel(&mut h);
    assert_eq!(pinned.picker.as_deref(), Some("r2 ▾"));
    assert_eq!(pinned.sections[0].id, "r2::");
    assert_eq!(
        pinned.context,
        Some(ScContext {
            text: "r2 · pinned".to_owned(),
            tooltip: None,
        })
    );
    assert_eq!(saved_ui(&dir)["source_control"]["pinned_repo"], json!("r2"));

    h.click_on("sc-picker");
    h.keys("escape");
    assert!(!h.root(|root, _| root.sc_picker_open()), "Esc closes it");
    h.click_on("sc-picker");
    let outside = h.bounds("sc-section-body-r2::").center();
    h.click(outside, Modifiers::none());
    assert!(
        !h.root(|root, _| root.sc_picker_open()),
        "an outside click closes it"
    );
    assert_eq!(
        h.root(|root, _| root.sc_pinned_repo().map(str::to_owned)),
        Some("r2".to_owned()),
        "and picks nothing"
    );

    h.click_on("sc-picker");
    h.click_on("sc-picker-auto");
    assert_eq!(
        h.root(|root, _| root.sc_pinned_repo().map(str::to_owned)),
        None
    );
    assert_eq!(panel(&mut h).picker, Some(format!("{AUTO} ▾")));
    assert!(saved_ui(&dir)["source_control"]["pinned_repo"].is_null());

    h.send(DaemonMessage::SessionUpdated {
        session: session("s1")
            .members(&[("r1", "main", "D:/src/r1")])
            .build(),
        request_id: None,
    });
    assert_eq!(
        panel(&mut h).picker,
        None,
        "a session with members hides it"
    );
    assert!(!h.in_model("sc-picker"));
    h.send(DaemonMessage::SessionUpdated {
        session: session("s1").build(),
        request_id: None,
    });
    h.send(DaemonMessage::Repos {
        repos: vec![repo("r1", "D:/src/r1")],
    });
    assert_eq!(panel(&mut h).picker, None, "one repo has nothing to pick");
}

#[gpui::test]
fn a_picker_whose_button_goes_away_closes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("s1").build());
    fixture.sessions.push(session("s2").build());
    fixture.repos = vec![repo("r1", "D:/src/r1"), repo("r2", "D:/src/r2")];
    let mut h = Harness::with(cx, &dir, &fixture);
    h.click_on("activity-source-control");
    h.click_on("sc-picker");
    assert!(h.in_model("sc-picker-menu"));

    h.send(DaemonMessage::SessionUpdated {
        session: session("s1")
            .members(&[("r1", "main", "D:/src/r1")])
            .build(),
        request_id: None,
    });
    assert!(
        !h.root(|root, _| root.sc_picker_open()),
        "the button went, so the menu and its blocking layer go too"
    );
    assert!(!h.in_model("sc-picker-menu"));

    h.click_on("activity-sessions");
    assert_eq!(
        activity(&mut h),
        Activity::Sessions,
        "the rail takes clicks"
    );
    h.sent();
    h.click_on("leaf-s2");
    let sent = h.sent();
    assert!(!sent.is_empty(), "a session row takes clicks");
}

#[gpui::test]
fn the_picker_blocks_and_is_closed_by_other_ui(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    h.click_on("activity-source-control");
    h.click_on("sc-picker");
    assert!(h.in_model("sc-picker-menu"));

    let sessions_item = h.bounds("activity-sessions").center();
    h.click(sessions_item, Modifiers::none());
    assert_eq!(
        activity(&mut h),
        Activity::SourceControl,
        "the layer keeps the click from the rail, so the Shell… button stays out of reach"
    );
    assert!(!h.root(|root, _| root.shell_dialog_open()));
    assert!(
        !h.root(|root, _| root.sc_picker_open()),
        "the click closed it"
    );

    h.click_on("sc-picker");
    h.keys("ctrl-shift-n");
    assert!(
        !h.root(|root, _| root.spawn_dialog_open()),
        "Ctrl+Shift+N opens nothing over the picker"
    );
    assert!(h.root(|root, _| root.sc_picker_open()));

    h.keys("ctrl-,");
    assert!(
        h.root(|root, _| root.settings_open()),
        "Ctrl+, opens Settings"
    );
    assert!(
        !h.root(|root, _| root.sc_picker_open()),
        "and closes the picker"
    );
    assert!(!h.in_model("sc-picker-menu"));
}

#[gpui::test]
fn a_pin_to_a_removed_repo_is_dropped(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    h.click_on("activity-source-control");
    h.click_on("sc-picker");
    h.click_on("sc-picker-r2");
    assert_eq!(saved_ui(&dir)["source_control"]["pinned_repo"], json!("r2"));

    h.send(DaemonMessage::Repos {
        repos: vec![repo("r1", "D:/src/r1")],
    });
    assert!(
        saved_ui(&dir)["source_control"]["pinned_repo"].is_null(),
        "the pin went with its repo"
    );
    assert_eq!(
        h.root(|root, _| root.sc_pinned_repo().map(str::to_owned)),
        None
    );
}

#[gpui::test]
fn refresh_resends_status_without_clearing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    h.click_on("activity-source-control");
    h.send(status("r1", None, &[], &["a.rs", "b.rs"]));
    h.send(status("r2", None, &[], &["c.rs"]));
    h.sent();

    h.click_on("sc-refresh");
    assert_eq!(
        status_requests(&mut h),
        [main_tree("r1")],
        "every section asks again, and only the sections"
    );
    assert_eq!(
        panel(&mut h).sections[0].body,
        "2 changed",
        "the stored status stays until the answer"
    );

    h.send(status("r1", None, &[], &["a.rs"]));
    assert_eq!(panel(&mut h).sections[0].body, "1 changed");
}

#[gpui::test]
fn no_repos_shows_the_register_hint(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    h.click_on("activity-source-control");

    assert_eq!(
        panel(&mut h),
        ScPanel {
            refresh: false,
            picker: None,
            context: None,
            sections: Vec::new(),
            empty_hint: Some("Register a repo from the Sessions sidebar to inspect changes here."),
        }
    );
    assert!(!h.in_model("sc-refresh"));
    assert!(h.in_model("sc-panel"));
    assert!(status_requests(&mut h).is_empty());
    assert_eq!(badge(&mut h), None);
}

#[gpui::test]
fn welcome_reseeds(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = two_repos();
    let mut h = Harness::with(cx, &dir, &fixture);
    h.click_on("activity-source-control");
    h.send(status("r1", None, &[], &["a.rs"]));
    h.sent();

    h.send(DaemonMessage::Welcome {
        protocol_version: 1,
        supported_versions: vec![1],
    });
    assert_eq!(
        status_requests(&mut h),
        [main_tree("r1"), main_tree("r2")],
        "a new connection asks for everything again"
    );
    assert_eq!(panel(&mut h).sections[0].body, "loading…");

    h.load(&fixture);
    assert!(
        status_requests(&mut h).is_empty(),
        "the repeat registry snapshot finds the requests out"
    );
}

#[gpui::test]
fn the_settings_gear_sits_on_the_rail_and_opens_settings_with_the_panel_collapsed(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    h.click_on("activity-sessions");
    assert!(collapsed(&mut h));

    let rail = h.bounds("activity-rail");
    let gear = h.bounds("settings-open");
    assert!(rail.contains(&gear.center()), "the gear is on the rail");
    assert!(
        rail.bottom() - gear.bottom() < px(10.0),
        "pinned to the rail's bottom"
    );

    h.click_on("settings-open");
    assert!(h.root(|root, _| root.settings_open()));
}
