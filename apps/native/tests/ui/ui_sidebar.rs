//! Sidebar specs: container order, regrouping, attention, the Ctrl+B and
//! divider behaviour, and the persisted layout.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use gpui::{Modifiers, MouseButton, TestAppContext, point, px};
use protocol::{
    AttentionReason, ClientMessage, DaemonMessage, DispatchRunStatus, PaneFollow, SessionMode,
    SessionSnapshot, SpawnConfig, SpawnTarget, SplitDirection, VscodeWorkspaceFolder,
    VscodeWorkspaceSuggestion, WorktreeReusePolicy,
};
use rustling_tulip_native::target::{HostKey, Target};
use rustling_tulip_native::{
    Activity, Connection, DETACHED_BANNER, DIVIDER_WIDTH, NetEvent, RAIL_WIDTH, RepoFilter,
    RuntimeIcon, SidebarView, TabPill, fold_tip, resume_all_label,
};
use support::{
    Fixture, Harness, PROTOCOL, REMOTE, TestDir, dispatch_run, follow_pane,
    follow_run_subagent_pane, follow_subagent_pane, pane, repo, saved_remote, seed_ui, session,
    split, subagent, subagent_with, tab, workspace,
};

/// The keys of the sidebar's containers, in the order it shows them.
fn tree_keys(h: &mut Harness<'_>) -> Vec<String> {
    h.root(|root, _| {
        root.sidebar_containers()
            .into_iter()
            .map(|c| c.key)
            .collect()
    })
}

/// Whether the element tagged `selector` has been painted on screen. gpui
/// keeps the bounds of an element that left the tree, so a `false` only
/// holds for an element the spec never showed.
fn painted(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).origin.x >= px(0.0)
}

fn saved_ui(dir: &TestDir) -> serde_json::Value {
    let text = std::fs::read_to_string(dir.path().join("native-ui.json")).expect("a saved layout");
    serde_json::from_str(&text).expect("native-ui.json is JSON")
}

/// The tag and name of the container holding leaf `id`, and the leaf's and
/// the container's attention marks.
fn home_of(h: &mut Harness<'_>, id: &str) -> Option<(&'static str, String, bool, bool)> {
    h.root(|root, _| {
        root.sidebar_containers().into_iter().find_map(|c| {
            let leaf = c.leaves.iter().find(|leaf| leaf.id == id)?;
            Some((c.kind.tag(), c.name.clone(), leaf.attention, c.attention))
        })
    })
}

#[gpui::test]
fn rows_grow_with_the_ui_text_size_and_clicks_still_hit_them(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let grid = split(
        SplitDirection::Horizontal,
        pane("p1", Some("s1")),
        split(
            SplitDirection::Horizontal,
            pane("p2", Some("s2")),
            pane("p3", Some("s3")),
        ),
    );
    let fixture = Fixture {
        sessions: vec![
            session("s1").build(),
            session("s2").build(),
            session("s3").build(),
        ],
        tabs: vec![tab("t1", &grid)],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    let base = h.bounds("leaf-s3").size.height;

    h.root_update(|root, _, cx| root.set_ui_text_size(3, cx));
    let third = h.bounds("leaf-s3");
    assert!(
        third.size.height > base,
        "the row grows at +3: {:?} from {base:?}",
        third.size.height
    );
    assert!(
        h.bounds("leaf-s2").bottom() <= third.top(),
        "the rows do not overlap"
    );
    assert_ne!(
        h.root(|root, _| root.focused_pane()),
        Some("p3".to_owned()),
        "precondition: the third session's pane is not focused yet"
    );
    h.click(third.center(), Modifiers::none());
    assert_eq!(
        h.root(|root, _| root.focused_pane()),
        Some("p3".to_owned()),
        "a click at the third row's drawn centre picks the third session"
    );
}

#[gpui::test]
fn a_repo_row_with_a_summary_grows_by_two_offsets(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("s1").in_repo("r1").build());
    let mut launched = repo("r1", "D:/src/r1");
    launched.last_spawn_config = Some(SpawnConfig {
        target: SpawnTarget::Single {
            repo_id: "r1".to_owned(),
            branch_name: "feat/x".to_owned(),
            base_branch: Some("main".to_owned()),
            use_worktree: false,
            checkout_strategy: None,
            worktree_reuse: WorktreeReusePolicy::default(),
            existing_worktree: None,
        },
        mode: SessionMode::Interactive,
        dangerously_skip_permissions: false,
        agent: None,
        extra_args: Vec::new(),
        extra_env: Vec::new(),
    });
    fixture.repos = vec![launched, repo("r2", "D:/src/r2")];
    let mut h = Harness::with(cx, &dir, &fixture);
    assert_eq!(
        h.bounds("container-repo:r1").size.height,
        px(46.0),
        "precondition: a two-line row at no offset"
    );

    h.root_update(|root, _, cx| root.set_ui_text_size(4, cx));
    let row = h.bounds("container-repo:r1");
    assert_eq!(row.size.height, px(54.0), "two lines: two offsets");
    assert!(
        h.bounds("leaf-s1").top() >= row.bottom(),
        "the next row starts at or below this row's bottom"
    );
    let other = h.bounds("container-repo:r2");
    assert!(
        other.bottom() <= row.top() || other.top() >= row.bottom(),
        "the one-line row does not overlap it: {other:?} and {row:?}"
    );
    assert_eq!(other.size.height, px(34.0), "one line: one offset");
}

#[gpui::test]
fn sidebar_chips_grow_with_the_ui_text_size(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture::single(
        session("s1")
            .agent_chips(serde_json::json!([{ "text": "DeepSeek", "tip": "Runs on DeepSeek" }]))
            .build(),
    );
    let mut h = Harness::with(cx, &dir, &fixture);
    assert_eq!(
        h.bounds("leaf-provider-s1").size.height,
        px(17.0),
        "precondition: a subline chip at no offset"
    );
    h.root_update(|root, _, cx| root.set_ui_text_size(3, cx));
    assert_eq!(h.bounds("leaf-provider-s1").size.height, px(20.0));
}

#[gpui::test]
fn the_view_switcher_fits_at_text_size_plus_four(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(
        cx,
        &dir,
        &Fixture::single(session("s1").in_repo("r1").build()),
    );
    h.root_update(|root, _, cx| root.set_ui_text_size(4, cx));

    let panel = h.bounds("sidebar-panel");
    let tabs = h.bounds("sidebar-view-tabs");
    assert!(
        tabs.size.width > px(0.0),
        "the Repos / Tabs toggle is drawn: {tabs:?}"
    );
    assert!(
        tabs.right() <= panel.right(),
        "at text size +4 the toggle stays inside the panel: {tabs:?} and {panel:?}"
    );
    let active = h.bounds("sidebar-filter-active");
    let repos = h.bounds("sidebar-view-repos");
    assert!(
        active.right() <= repos.left() || active.bottom() <= repos.top(),
        "All / Active and Repos / Tabs do not overlap: {active:?} and {repos:?}"
    );
}

#[gpui::test]
fn the_header_fits_at_the_minimum_width_and_text_size_plus_four(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(
        cx,
        &dir,
        &Fixture::single(session("s1").in_repo("r1").build()),
    );

    // While the title and both controls fit on one row, the toggle's centre
    // sits on the title's.
    let panel = h.bounds("sidebar-panel");
    let title = h.bounds("sidebar-title");
    let tabs = h.bounds("sidebar-view-tabs");
    let all = h.bounds("sidebar-filter-all");
    let repos = h.bounds("sidebar-view-repos");
    let off = (title.center().y - tabs.center().y) / px(1.0);
    assert!(
        off.abs() <= 1.0,
        "at the default width the header is one row: {title:?} {tabs:?} {all:?} {repos:?} {panel:?}"
    );

    // Narrow the panel to its minimum, then raise the text size.
    let from = h.center("sidebar-divider");
    h.drag(from, point(px(50.0), from.y), [Modifiers::none(); 2]);
    let panel = h.bounds("sidebar-panel");
    assert!(
        (panel.size.width / px(1.0) - 200.0).abs() < 0.5,
        "the panel is at its minimum width: {panel:?}"
    );
    h.root_update(|root, _, cx| root.set_ui_text_size(4, cx));

    let panel = h.bounds("sidebar-panel");
    let title = h.bounds("sidebar-title");
    let controls = [
        "sidebar-filter-all",
        "sidebar-filter-active",
        "sidebar-view-repos",
        "sidebar-view-tabs",
    ];
    for selector in controls {
        let bounds = h.bounds(selector);
        assert!(
            bounds.left() >= panel.left() && bounds.right() <= panel.right(),
            "at the minimum width and text size +4, {selector} stays inside the panel: \
             {bounds:?} and {panel:?}"
        );
    }
    let active = h.bounds("sidebar-filter-active");
    let repos = h.bounds("sidebar-view-repos");
    assert!(
        active.right() <= repos.left() || active.bottom() <= repos.top(),
        "All / Active and Repos / Tabs do not overlap: {active:?} and {repos:?}"
    );
    for selector in controls {
        let bounds = h.bounds(selector);
        assert!(
            bounds.top() >= title.bottom()
                || bounds.right() <= title.left()
                || bounds.left() >= title.right(),
            "{selector} is below the title or beside it, never over it: {bounds:?} and {title:?}"
        );
    }
}

#[gpui::test]
fn containers_render_workspace_repo_shell_dir_detached_in_order(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        repos: vec![repo("r1", "D:/src/r1"), repo("r2", "D:/src/r2")],
        workspaces: vec![workspace("ws1", &["r1"])],
        sessions: vec![
            session("gone").in_repo("r9").build(),
            session("dir").dir_shell("D:/scratch/dir").build(),
            session("sh").shell("D:/elsewhere").build(),
            session("repo").in_repo("r2").build(),
            session("ws").in_workspace("ws1").build(),
        ],
        tabs: vec![tab("t1", &pane("p1", None))],
    };
    let mut h = Harness::with(cx, &dir, &fixture);

    let tags: Vec<&str> = h.root(|root, _| {
        root.sidebar_containers()
            .iter()
            .filter(|c| !c.leaves.is_empty())
            .map(|c| c.kind.tag())
            .collect()
    });
    assert_eq!(tags, ["WS", "REPO", "SH", "DIR", "DET"]);
    for id in ["ws", "repo", "sh", "dir", "gone"] {
        assert!(h.in_model(&format!("leaf-{id}")), "leaf {id} is listed");
    }
}

#[gpui::test]
fn excluded_leaf_shows_the_marker_and_no_badge(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut excluded = session("s1").in_repo("r1").status("awaiting_input").build();
    excluded.busy_excluded = true;
    let fixture = Fixture {
        repos: vec![repo("r1", "D:/src/r1")],
        sessions: vec![
            excluded,
            session("s2").in_repo("r1").status("awaiting_input").build(),
        ],
        tabs: vec![tab("t1", &pane("p1", None))],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    for id in ["s1", "s2"] {
        h.send(DaemonMessage::Attention {
            session_id: id.to_owned(),
            reason: AttentionReason::AwaitingInput,
        });
    }

    assert!(
        painted(&mut h, "leaf-busy-excluded-s1"),
        "the excluded leaf's marker"
    );
    assert!(
        !painted(&mut h, "leaf-badge-s1"),
        "no `!` on the excluded leaf"
    );
    assert!(
        painted(&mut h, "leaf-badge-s2"),
        "the counted leaf keeps its `!`"
    );
    assert!(
        !painted(&mut h, "leaf-busy-excluded-s2"),
        "and has no marker"
    );
}

#[gpui::test]
fn shell_moves_under_repo_when_its_cwd_enters_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        repos: vec![repo("r2", "D:/src/r2")],
        sessions: vec![session("sh").shell("D:/elsewhere").build()],
        tabs: vec![tab("t1", &pane("p1", None))],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    assert_eq!(home_of(&mut h, "sh").map(|home| home.0), Some("SH"));

    h.send(DaemonMessage::SessionUpdated {
        session: session("sh").shell("D:/src/r2/sub").build(),
        request_id: None,
    });

    let home = home_of(&mut h, "sh").expect("the shell is listed");
    assert_eq!((home.0, home.1.as_str()), ("REPO", "r2"));
}

#[gpui::test]
fn attention_marks_leaf_and_container_and_clears_on_click_or_working(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("s1").in_repo("r1").build());
    fixture.repos = vec![repo("r1", "D:/src/r1")];
    let mut h = Harness::with(cx, &dir, &fixture);
    let attention = |h: &mut Harness<'_>| {
        let home = home_of(h, "s1").expect("s1 is listed");
        (home.2, home.3)
    };
    let flag = DaemonMessage::Attention {
        session_id: "s1".to_owned(),
        reason: AttentionReason::AwaitingInput,
    };

    h.send(flag.clone());
    assert_eq!(attention(&mut h), (true, true), "leaf and container");
    h.click_on("leaf-s1");
    assert_eq!(attention(&mut h), (false, false), "a click clears it");

    h.send(flag);
    assert_eq!(attention(&mut h), (true, true));
    h.send(DaemonMessage::SessionUpdated {
        session: session("s1").in_repo("r1").status("working").build(),
        request_id: None,
    });
    assert_eq!(attention(&mut h), (false, false), "working clears it");
}

#[gpui::test]
fn ctrl_b_is_input_in_the_terminal_and_toggles_the_sidebar_elsewhere(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    h.answer_scrollback("s1", b"");
    let cell = h.cell_center("p1", 0, 0);
    h.click(cell, Modifiers::none());
    h.sent();

    h.keys("ctrl-b");
    assert_eq!(h.sent_input("s1"), [0x02]);
    assert!(!h.root(|root, _| root.sidebar_collapsed()));

    let panel = h.bounds("sidebar-panel");
    h.click(
        point(panel.center().x, panel.bottom() - px(10.0)),
        Modifiers::none(),
    );
    h.keys("ctrl-b");
    assert!(h.root(|root, _| root.sidebar_collapsed()), "hidden");
    assert_eq!(h.sent_input("s1"), [] as [u8; 0]);

    h.click_on("activity-sessions");
    assert!(!h.root(|root, _| root.sidebar_collapsed()), "shown again");
}

#[gpui::test]
fn sidebar_divider_clamps_persists_and_restores(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("s1").build());
    fixture.tabs.push(tab("t2", &pane("p2", None)));
    let mut h = Harness::with(cx, &dir, &fixture);
    let window = h.window_width();
    let drag_to = |h: &mut Harness<'_>, x: f32| {
        let from = h.center("sidebar-divider");
        h.drag(from, point(px(x), from.y), [Modifiers::none(); 2]);
        h.bounds("sidebar-panel").size.width / px(1.0)
    };

    assert!(
        (drag_to(&mut h, 50.0) - 200.0).abs() < 0.5,
        "clamped to 200"
    );
    assert!(
        (drag_to(&mut h, window - 10.0) - (window - 200.0)).abs() < 0.5,
        "clamped to window - 200"
    );
    // The sidebar starts right of the rail, so its width is the pointer's
    // distance from the rail.
    assert!((drag_to(&mut h, RAIL_WIDTH + 400.0) - 400.0).abs() < 0.5);
    let saved: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("native-ui.json")).expect("native-ui.json"),
    )
    .expect("native-ui.json is JSON");
    assert_eq!(saved["sidebar_width"].as_f64(), Some(400.0));

    h.click_on("tab-t2");
    let panel = h.bounds("sidebar-panel");
    h.click(
        point(panel.center().x, panel.bottom() - px(10.0)),
        Modifiers::none(),
    );
    h.keys("ctrl-b");
    assert!(h.root(|root, _| root.sidebar_collapsed()));
    drop(h);

    let mut h = Harness::with(cx, &dir, &fixture);
    assert!(
        h.root(|root, _| root.sidebar_collapsed()),
        "collapsed restored"
    );
    assert_eq!(
        h.root(|root, _| root.active_tab_id().map(str::to_owned)),
        Some("t2".to_owned())
    );
    h.click_on("activity-sessions");
    assert_eq!(h.bounds("sidebar-panel").size.width, px(400.0));
}

#[gpui::test]
fn dragging_the_sidebar_divider_moves_it_by_the_drag(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    // The divider's left edge follows the pointer, so a press at its centre
    // lands it at most half its width off the drag.
    let slack = DIVIDER_WIDTH / 2.0 + 1.0;
    let x0 = h.bounds("sidebar-divider").origin.x / px(1.0);
    let from = h.center("sidebar-divider");

    h.cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::none());
    h.cx.simulate_mouse_move(
        point(from.x + px(10.0), from.y),
        MouseButton::Left,
        Modifiers::none(),
    );
    h.cx.run_until_parked();
    let first = h.bounds("sidebar-divider").origin.x / px(1.0) - x0;
    assert!(
        (first - 10.0).abs() <= slack,
        "a first move of 10 px moved the divider {first} px"
    );

    let to = point(from.x + px(80.0), from.y);
    h.cx.simulate_mouse_move(to, MouseButton::Left, Modifiers::none());
    h.cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::none());
    h.cx.run_until_parked();
    let moved = h.bounds("sidebar-divider").origin.x / px(1.0) - x0;
    assert!(
        (moved - 80.0).abs() <= slack,
        "an 80 px drag moved the divider {moved} px"
    );
}

#[gpui::test]
fn attention_leaf_draws_its_badge_and_asking_glyph(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("s1").build());
    fixture.sessions.push(session("s2").build());
    let mut h = Harness::with(cx, &dir, &fixture);
    h.answer_scrollback("s1", b"");
    let leaf_attention = |h: &mut Harness<'_>, id: &str| home_of(h, id).map(|home| home.2);
    assert_eq!(leaf_attention(&mut h, "s2"), Some(false));
    assert!(!drawn(&mut h, "leaf-badge-s2"), "no badge yet");

    for id in ["s1", "s2"] {
        h.send(DaemonMessage::SessionUpdated {
            session: session(id).status("awaiting_input").build(),
            request_id: None,
        });
        h.send(DaemonMessage::Attention {
            session_id: id.to_owned(),
            reason: AttentionReason::AwaitingInput,
        });
    }
    for id in ["s1", "s2"] {
        assert_eq!(leaf_attention(&mut h, id), Some(true), "{id}");
        assert!(drawn(&mut h, &format!("leaf-badge-{id}")), "{id} badged");
        assert!(leaf_glyph(&mut h, id, "asking"), "{id} asks");
    }

    h.send(DaemonMessage::SessionUpdated {
        session: session("s2").status("working").build(),
        request_id: None,
    });
    assert_eq!(leaf_attention(&mut h, "s2"), Some(false), "calm again");
    assert!(leaf_glyph(&mut h, "s2", "working"));
}

#[gpui::test]
fn headless_leaf_click_only_clears_attention(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("s1").build());
    fixture.sessions.push(session("hl").headless().build());
    let mut h = Harness::with(cx, &dir, &fixture);
    h.send(DaemonMessage::Attention {
        session_id: "hl".to_owned(),
        reason: AttentionReason::AwaitingInput,
    });
    h.sent();
    assert_eq!(home_of(&mut h, "hl").map(|home| home.2), Some(true));

    h.click_on("leaf-hl");

    let sent = h.sent();
    assert!(sent.is_empty(), "sent {sent:?}");
    assert_eq!(home_of(&mut h, "hl").map(|home| home.2), Some(false));
}

#[gpui::test]
fn view_toggle_switches_to_tabs_and_saves(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("s1").in_repo("r1").build());
    fixture.repos = vec![repo("r1", "D:/src/r1")];
    fixture.sessions.push(session("s2").in_repo("r1").build());
    let mut h = Harness::with(cx, &dir, &fixture);
    assert_eq!(h.root(|root, _| root.sidebar_view()), SidebarView::Repos);
    assert_eq!(tree_keys(&mut h), ["repo:r1"]);

    h.click_on("sidebar-view-tabs");
    assert_eq!(h.root(|root, _| root.sidebar_view()), SidebarView::Tabs);
    assert_eq!(tree_keys(&mut h), ["tab:t1", "unbound"]);
    assert_eq!(saved_ui(&dir)["sidebar_view"], "tabs", "saved at once");
    assert!(h.in_model("leaf-s1") && h.in_model("leaf-s2"));

    h.click_on("sidebar-view-repos");
    assert_eq!(tree_keys(&mut h), ["repo:r1"]);
    assert_eq!(saved_ui(&dir)["sidebar_view"], "repos");
}

#[gpui::test]
fn tabs_view_lists_a_remote_session_with_its_host_chip(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = support::mixed_tabs(cx, &dir);
    let remote_leaf = format!("leaf-{}::b1", support::REMOTE);
    assert!(
        !drawn(&mut h, "sidebar-host-box-b1"),
        "the Repos view draws no leaf chip"
    );

    h.click_on("sidebar-view-tabs");
    assert_eq!(h.root(|root, _| root.sidebar_view()), SidebarView::Tabs);
    assert!(drawn(&mut h, &remote_leaf), "t1 lists box's b1");
    assert!(
        drawn(&mut h, "sidebar-host-box-b1"),
        "b1's leaf names its host"
    );
    assert!(drawn(&mut h, "leaf-a1"), "t1 lists This PC's a1 too");
    assert!(
        !drawn(&mut h, "sidebar-host-box-a1"),
        "This PC's leaf carries no chip"
    );
}

#[gpui::test]
fn saved_tabs_view_is_restored_on_launch(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    std::fs::write(
        dir.path().join("native-ui.json"),
        r#"{ "sidebar_view": "tabs" }"#,
    )
    .expect("write the layout");
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    assert_eq!(h.root(|root, _| root.sidebar_view()), SidebarView::Tabs);
    assert_eq!(tree_keys(&mut h), ["tab:t1"]);
    assert!(h.in_model("leaf-s1"));
}

#[gpui::test]
fn active_filter_hides_repos_with_no_live_session_and_saves(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("s1").in_repo("r1").status("working").build());
    fixture.repos = vec![repo("r1", "D:/src/r1"), repo("r2", "D:/src/r2")];
    let mut h = Harness::with(cx, &dir, &fixture);
    assert_eq!(tree_keys(&mut h), ["repo:r1", "repo:r2"]);

    h.click_on("sidebar-filter-active");
    assert_eq!(tree_keys(&mut h), ["repo:r1"], "r2 has no live session");
    assert_eq!(saved_ui(&dir)["repo_filter"], "active", "saved at once");

    h.click_on("sidebar-filter-all");
    assert_eq!(tree_keys(&mut h), ["repo:r1", "repo:r2"]);
    assert_eq!(saved_ui(&dir)["repo_filter"], "all");
}

#[gpui::test]
fn active_filter_shows_the_empty_text_when_nothing_is_active(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("sh").shell("D:/elsewhere").build());
    fixture.repos = vec![repo("r1", "D:/src/r1")];
    let mut h = Harness::with(cx, &dir, &fixture);
    assert!(!drawn(&mut h, "sidebar-filter-empty"), "All shows the repo");

    h.click_on("sidebar-filter-active");
    assert!(drawn(&mut h, "sidebar-filter-empty"));
    assert_eq!(tree_keys(&mut h), ["standalone:sh"], "the shell row stays");
    assert!(h.in_model("leaf-sh"));
}

#[gpui::test]
fn saved_active_filter_is_restored_on_launch(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    std::fs::write(
        dir.path().join("native-ui.json"),
        r#"{ "repo_filter": "active" }"#,
    )
    .expect("write the layout");
    let mut fixture = Fixture::single(session("s1").in_repo("r1").build());
    fixture.repos = vec![repo("r1", "D:/src/r1"), repo("r2", "D:/src/r2")];
    let mut h = Harness::with(cx, &dir, &fixture);
    assert_eq!(h.root(|root, _| root.repo_filter()), RepoFilter::Active);
    assert_eq!(tree_keys(&mut h), ["repo:r1"]);
}

#[gpui::test]
fn the_filter_control_is_not_drawn_in_the_tabs_view(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    std::fs::write(
        dir.path().join("native-ui.json"),
        r#"{ "sidebar_view": "tabs" }"#,
    )
    .expect("write the layout");
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    assert_eq!(h.root(|root, _| root.sidebar_view()), SidebarView::Tabs);
    assert!(
        drawn(&mut h, "sidebar-view-tabs"),
        "the grouping toggle shows"
    );
    assert!(!drawn(&mut h, "sidebar-filter-all"), "the filter does not");
    assert!(!drawn(&mut h, "sidebar-filter-active"));
}

#[gpui::test]
fn active_filter_note_is_not_drawn_in_the_tabs_view(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    std::fs::write(
        dir.path().join("native-ui.json"),
        r#"{ "repo_filter": "active", "sidebar_view": "tabs" }"#,
    )
    .expect("write the layout");
    let mut fixture = Fixture::single(session("s1").build());
    fixture.repos = vec![repo("r1", "D:/src/r1")];
    let mut h = Harness::with(cx, &dir, &fixture);
    assert_eq!(h.root(|root, _| root.sidebar_view()), SidebarView::Tabs);
    assert!(
        !drawn(&mut h, "sidebar-filter-empty"),
        "the tabs view draws no row note"
    );

    h.click_on("sidebar-view-repos");
    assert_eq!(h.root(|root, _| root.sidebar_view()), SidebarView::Repos);
    assert!(drawn(&mut h, "sidebar-filter-empty"), "the repos view does");
}

#[gpui::test]
fn unbound_container_shows_its_banner(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("s1").build());
    fixture.sessions.push(session("s2").build());
    let mut h = Harness::with(cx, &dir, &fixture);
    assert!(
        !h.in_model("unbound-banner"),
        "the repos view has no Unbound"
    );

    h.click_on("sidebar-view-tabs");
    assert!(h.in_model("unbound-banner"));
    let banner = h.bounds("unbound-banner");
    assert!(banner.origin.x >= px(0.0), "painted");
    assert!(
        h.bounds("container-unbound").bottom() <= banner.top(),
        "under its container"
    );

    h.click_on("container-unbound");
    assert!(!h.in_model("unbound-banner"), "folded with its container");
    assert!(!h.in_model("leaf-s2"));
}

#[gpui::test]
fn detached_container_shows_its_banner(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        sessions: vec![session("s1").in_repo("gone").abandoned().build()],
        tabs: vec![tab("t1", &pane("p1", None))],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    assert_eq!(
        DETACHED_BANNER,
        "These sessions are still running, but their repo or workspace is no longer registered.",
        "the owner-ruled wording"
    );
    assert!(h.in_model("detached-banner"), "the bucket's banner");
    let banner = h.bounds("detached-banner");
    assert!(painted(&mut h, "detached-banner"), "drawn");
    assert!(
        h.bounds("container-detached").bottom() <= banner.top(),
        "under its container row"
    );
    assert!(
        h.bounds("leaf-s1").top() >= banner.bottom(),
        "above its leaves"
    );

    h.click_on("container-detached");
    assert!(!h.in_model("detached-banner"), "folded with its container");
    assert!(!h.in_model("leaf-s1"));

    h.send(DaemonMessage::Repos {
        repos: vec![repo("gone", "D:/src/gone")],
    });
    assert!(h.in_model("leaf-s1"), "the repo is registered now");
    assert!(!h.in_model("detached-banner"), "so there is no Detached");
}

#[gpui::test]
fn leaf_shows_its_tab_pill_in_both_views(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let grid = split(
        SplitDirection::Horizontal,
        pane("p1", Some("s1")),
        pane("p2", Some("s2")),
    );
    let fixture = Fixture {
        sessions: vec![
            session("s1").build(),
            session("s2").build(),
            session("s3").build(),
        ],
        tabs: vec![tab("t1", &grid), tab("t2", &pane("p3", Some("s2")))],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    let pill = |h: &mut Harness<'_>, id: &str| h.root(|root, _| root.leaf_tab_pill(id));

    for view in ["repos", "tabs"] {
        h.click_on(&format!("sidebar-view-{view}"));
        assert_eq!(
            pill(&mut h, "s1"),
            Some(TabPill::One {
                tab_id: "t1".to_owned(),
                name: "t1".to_owned(),
            }),
            "{view}"
        );
        assert_eq!(
            pill(&mut h, "s2"),
            Some(TabPill::Many(vec!["t1".to_owned(), "t2".to_owned()])),
            "{view}"
        );
        assert_eq!(pill(&mut h, "s3"), Some(TabPill::Unbound), "{view}");
        for id in ["s1", "s2", "s3"] {
            let selector = format!("leaf-pill-{id}");
            assert!(h.in_model(&selector), "{view}: {selector} is shown");
            assert!(
                h.bounds(&selector).origin.x >= px(0.0),
                "{view}: {selector} is painted"
            );
        }
    }
}

#[gpui::test]
fn unbound_pill_click_creates_a_tab_with_the_session(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("s1").build());
    fixture.sessions.push(session("s2").build());
    let mut h = Harness::with(cx, &dir, &fixture);
    h.answer_scrollback("s1", b"");
    h.sent();

    h.click_on("leaf-pill-s2");

    let sent = h.sent();
    assert!(
        matches!(
            sent.as_slice(),
            [ClientMessage::CreateTab {
                name: None,
                initial_session_id: Some(id),
                ..
            }] if id == "s2"
        ),
        "sent {sent:?}"
    );
}

#[gpui::test]
fn headless_unbound_pill_is_inert(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("s1").build());
    fixture.sessions.push(session("hl").headless().build());
    let mut h = Harness::with(cx, &dir, &fixture);
    h.answer_scrollback("s1", b"");
    h.sent();
    assert_eq!(
        h.root(|root, _| root.leaf_tab_pill("hl")),
        Some(TabPill::Unbound)
    );

    h.click_on("leaf-pill-hl");

    let sent = h.sent();
    assert!(sent.is_empty(), "sent {sent:?}");
    assert_eq!(
        h.root(|root, _| root.tab_ids().len()),
        1,
        "no tab was asked for"
    );
}

#[gpui::test]
fn no_pills_before_tabs_load(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::open(cx, &dir);
    h.send(DaemonMessage::Repos { repos: Vec::new() });
    h.send(DaemonMessage::Sessions {
        sessions: vec![session("s1").build()],
    });
    assert!(h.in_model("leaf-s1"), "the leaf is listed");
    assert_eq!(h.root(|root, _| root.leaf_tab_pill("s1")), None);
    assert!(!h.in_model("leaf-pill-s1"), "no pill yet");

    h.click_on("sidebar-view-tabs");
    assert!(
        tree_keys(&mut h).is_empty(),
        "the tabs view waits for the tab list"
    );

    h.send(DaemonMessage::Tabs {
        tabs: vec![tab("t1", &pane("p1", Some("s1")))],
    });
    assert_eq!(tree_keys(&mut h), ["tab:t1"]);
    assert!(h.in_model("leaf-pill-s1"));
}

#[gpui::test]
fn bound_pill_click_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    h.answer_scrollback("s1", b"");
    h.sent();

    h.click_on("leaf-pill-s1");

    let sent = h.sent();
    assert!(sent.is_empty(), "sent {sent:?}");
}

/// `s1` listed in the sidebar and shown by no pane, and nothing sent yet.
fn listed<'a>(cx: &'a mut TestAppContext, dir: &TestDir, s1: SessionSnapshot) -> Harness<'a> {
    let fixture = Fixture {
        sessions: vec![s1],
        tabs: vec![tab("t1", &pane("p1", None))],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, dir, &fixture);
    h.sent();
    h
}

fn tags(h: &mut Harness<'_>, id: &str) -> Vec<(String, String)> {
    let id = id.to_owned();
    h.root(move |root, _| root.leaf_tags(&id))
}

fn tag(text: &str, tip: &str) -> (String, String) {
    (text.to_owned(), tip.to_owned())
}

#[gpui::test]
fn orphan_leaf_shows_its_tag(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = listed(cx, &dir, session("s1").orphan().build());

    assert_eq!(
        tags(&mut h, "s1"),
        [
            tag("claude", "Running claude"),
            tag("orphan", "Reattached after daemon restart; PTY detached"),
        ]
    );
    assert!(
        !h.in_model("leaf-resume-s1"),
        "an orphan has no inline Resume"
    );
}

#[gpui::test]
fn abandoned_leaf_resume_sends_resume_abandoned(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = listed(
        cx,
        &dir,
        session("s1").abandoned().last_prompt("fix it").build(),
    );

    assert_eq!(
        tags(&mut h, "s1"),
        [
            tag("claude", "Running claude"),
            tag("abandoned", "Daemon crashed mid-run. Last prompt:\nfix it"),
        ]
    );
    assert_eq!(
        h.root(|root, _| root.leaf_buttons("s1")),
        [
            tag(
                "leaf-resume-s1",
                "Spawn a fresh session from the captured config and replay the prompt"
            ),
            tag(
                "leaf-dismiss-s1",
                "Dismiss this abandoned session without resuming"
            ),
        ]
    );
    h.click_on("leaf-resume-s1");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::ResumeAbandoned { session_id }] if session_id == "s1"),
        "sent {sent:?}"
    );
}

#[gpui::test]
fn abandoned_leaf_dismiss_sends_discard_abandoned(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = listed(cx, &dir, session("s1").abandoned().build());

    assert_eq!(
        tags(&mut h, "s1")[1],
        tag("abandoned", "Daemon crashed mid-run")
    );
    h.click_on("leaf-dismiss-s1");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::DiscardAbandoned { session_id }] if session_id == "s1"),
        "sent {sent:?}"
    );
}

#[gpui::test]
fn inactive_leaf_resume_duplicates(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = listed(cx, &dir, session("s1").worktree("r1").inactive().build());

    assert_eq!(
        tags(&mut h, "s1")[1],
        tag("inactive", "Parked. Worktree kept on disk:\nC:/wt/x")
    );
    assert_eq!(
        h.root(|root, _| root.leaf_buttons("s1")),
        [tag(
            "leaf-resume-s1",
            "Spawn a fresh session that reuses this worktree"
        )]
    );
    h.click_on("leaf-resume-s1");
    let sent = h.sent();
    assert!(
        matches!(
            sent.as_slice(),
            [ClientMessage::DuplicateSession { session_id, request_id: Some(_), .. }] if session_id == "s1"
        ),
        "sent {sent:?}"
    );
}

#[gpui::test]
fn inline_button_does_not_focus_the_leaf(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = listed(cx, &dir, session("s1").abandoned().build());

    h.click_on("leaf-dismiss-s1");

    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::DiscardAbandoned { .. }]),
        "the leaf click placed nothing in the empty pane: {sent:?}"
    );
}

#[gpui::test]
fn leaf_tooltip_lists_title_and_cwd(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = listed(
        cx,
        &dir,
        session("s1")
            .shell("D:/src/app")
            .terminal_title("vim")
            .program_name("pwsh")
            .build(),
    );

    let tooltip = h.root(|root, _| {
        root.sidebar_containers()
            .into_iter()
            .flat_map(|c| c.leaves)
            .find(|leaf| leaf.id == "s1")
            .map(|leaf| leaf.tooltip)
    });
    assert_eq!(
        tooltip.as_deref(),
        Some("app\nSession: s1\nTerminal title: vim\nCwd: D:/src/app")
    );
    assert_eq!(tags(&mut h, "s1"), [tag("pwsh", "Running pwsh")]);
}

/// Whether leaf `id` draws the status glyph named `shape`.
fn leaf_glyph(h: &mut Harness<'_>, id: &str, shape: &str) -> bool {
    h.bounds(&format!("leaf-glyph-{id}-{shape}")).origin.x >= px(0.0)
}

#[gpui::test]
fn each_leaf_draws_its_status_glyph(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        sessions: vec![
            session("work").status("working").build(),
            session("ask").status("awaiting_input").build(),
            session("idle").build(),
            session("sh").shell("D:/src/app").build(),
            session("spawn").status("spawning").build(),
            session("stop").status("stopped").build(),
            session("err").status("error").build(),
        ],
        tabs: vec![tab("t1", &pane("p1", None))],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);

    for (id, shape) in [
        ("work", "working"),
        ("ask", "asking"),
        ("idle", "idle"),
        ("sh", "idle"),
        ("spawn", "spawning"),
        ("stop", "stopped"),
        ("err", "error"),
    ] {
        assert!(leaf_glyph(&mut h, id, shape), "{id} draws {shape}");
    }
}

/// Whether leaf `id` holds an unseen turn. The glyph selectors cannot say
/// a glyph is gone: gpui keeps a selector's bounds from earlier frames.
fn leaf_unseen(h: &mut Harness<'_>, id: &str) -> Option<bool> {
    h.root(|root, _| {
        root.sidebar_containers()
            .into_iter()
            .flat_map(|c| c.leaves)
            .find(|leaf| leaf.id == id)
            .map(|leaf| leaf.unseen)
    })
}

#[gpui::test]
fn background_turn_ending_waits_until_its_leaf_is_clicked(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("s1").status("working").build());
    fixture
        .sessions
        .push(session("s2").status("working").build());
    let mut h = Harness::with(cx, &dir, &fixture);
    h.cx.update(|window, _| window.activate_window());
    h.answer_scrollback("s1", b"");
    for id in ["s1", "s2"] {
        h.send(DaemonMessage::SessionUpdated {
            session: session(id).build(),
            request_id: None,
        });
    }

    assert!(leaf_glyph(&mut h, "s2", "waiting"), "s2 ended unseen");
    assert!(
        leaf_glyph(&mut h, "s1", "idle"),
        "s1 ended in the focused pane"
    );
    assert_eq!(leaf_unseen(&mut h, "s1"), Some(false));

    h.click_on("leaf-s2");
    assert!(leaf_glyph(&mut h, "s2", "idle"), "the click saw it");
    assert_eq!(leaf_unseen(&mut h, "s2"), Some(false));
}

#[gpui::test]
fn a_background_turn_end_shows_the_spinning_ring_and_stays_seen(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("s1").build());
    fixture.sessions.push(session("s2").build());
    let mut h = Harness::with(cx, &dir, &fixture);
    h.answer_scrollback("s1", b"");
    let send = |h: &mut Harness<'_>, session: SessionSnapshot| {
        h.send(DaemonMessage::SessionUpdated {
            session,
            request_id: None,
        });
    };

    send(&mut h, session("s2").status("working").build());
    let mut background = session("s2").build();
    background.background = true;
    send(&mut h, background);
    assert!(
        leaf_glyph(&mut h, "s2", "background"),
        "the turn ended with work still running"
    );
    assert_eq!(leaf_unseen(&mut h, "s2"), Some(false), "not finished yet");

    send(&mut h, session("s2").build());
    assert!(
        leaf_glyph(&mut h, "s2", "waiting"),
        "the background work ended"
    );
    assert_eq!(leaf_unseen(&mut h, "s2"), Some(true));
}

#[gpui::test]
fn unseen_session_rebound_into_the_focused_pane_is_seen(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("s1").build());
    fixture
        .sessions
        .push(session("s2").status("working").build());
    let mut h = Harness::with(cx, &dir, &fixture);
    h.cx.update(|window, _| window.activate_window());
    h.answer_scrollback("s1", b"");
    h.send(DaemonMessage::SessionUpdated {
        session: session("s2").build(),
        request_id: None,
    });
    assert_eq!(leaf_unseen(&mut h, "s2"), Some(true), "s2 ended unseen");

    h.send(DaemonMessage::TabUpdated {
        tab: tab("t1", &pane("p1", Some("s2"))),
    });

    assert_eq!(
        leaf_unseen(&mut h, "s2"),
        Some(false),
        "the focused pane shows s2 now"
    );
}

/// Whether the element tagged `selector` is laid out on screen.
fn drawn(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).origin.x >= px(0.0)
}

#[gpui::test]
fn session_button_draws_its_hint_even_with_no_repo(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    assert!(
        !h.root(|root, _| root.has_repos()),
        "the button is disabled"
    );

    assert!(drawn(&mut h, "sidebar-add-session"));
    assert!(drawn(&mut h, "sidebar-session-hint"), "Ctrl N still shows");
}

#[gpui::test]
fn more_menu_opens_the_shell_dialog_and_closes_on_escape_or_outside(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    assert!(
        !drawn(&mut h, "sidebar-more-shell-dialog"),
        "closed at first"
    );

    h.click_on("sidebar-more");
    assert!(h.root(|root, _| root.sidebar_more_open()));
    assert!(drawn(&mut h, "sidebar-more-shell-dialog"));
    h.keys("escape");
    assert!(!h.root(|root, _| root.sidebar_more_open()), "Esc closes it");

    h.click_on("sidebar-more");
    let panel = h.bounds("sidebar-panel");
    h.click(
        point(panel.center().x, panel.bottom() - px(10.0)),
        Modifiers::none(),
    );
    assert!(
        !h.root(|root, _| root.sidebar_more_open()),
        "a click outside closes it"
    );
    assert!(!h.root(|root, _| root.shell_dialog_open()));

    h.click_on("sidebar-more");
    h.click_on("sidebar-more-shell-dialog");
    assert!(!h.root(|root, _| root.sidebar_more_open()));
    assert!(
        h.root(|root, _| root.shell_dialog_open()),
        "Shell… opens the dialog"
    );
}

/// A branch too long to share the subline with a chip.
const LONG_BRANCH: &str = "feat/a-branch-name-long-enough-that-no-chip-fits-beside-it-at-all";

/// Repo `r1` holding `short` (on `main`) and `long` (on [`LONG_BRANCH`]),
/// both shown in tab `t1`, and `loose` (on [`LONG_BRANCH`]) in no tab.
fn density_fixture() -> Fixture {
    let grid = split(
        SplitDirection::Horizontal,
        pane("p1", Some("short")),
        pane("p2", Some("long")),
    );
    Fixture {
        repos: vec![repo("r1", "D:/src/r1")],
        sessions: vec![
            session("short").in_repo("r1").build(),
            session("long").members(&[("r1", LONG_BRANCH, "")]).build(),
            session("loose").members(&[("r1", LONG_BRANCH, "")]).build(),
        ],
        tabs: vec![tab("t1", &grid)],
        ..Fixture::default()
    }
}

/// Leaf `id`'s subline, as the model builds it.
fn subline(h: &mut Harness<'_>, id: &str) -> Option<String> {
    h.root(|root, _| {
        root.sidebar_containers()
            .into_iter()
            .flat_map(|c| c.leaves)
            .find(|leaf| leaf.id == id)
            .and_then(|leaf| leaf.subline)
    })
}

/// Whether `inner`'s bounds lie inside `outer`'s, top to bottom.
fn within(h: &mut Harness<'_>, inner: &str, outer: &str) -> bool {
    let (inner, outer) = (h.bounds(inner), h.bounds(outer));
    inner.top() >= outer.top() && inner.bottom() <= outer.bottom()
}

#[gpui::test]
fn comfortable_leaf_shows_its_subline_and_a_chip_row_only_for_chips_that_do_not_fit(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &density_fixture());

    assert_eq!(subline(&mut h, "short").as_deref(), Some("main"));
    assert_eq!(subline(&mut h, "long").as_deref(), Some(LONG_BRANCH));
    for id in ["short", "long", "loose"] {
        assert!(drawn(&mut h, &format!("leaf-subline-{id}")), "{id}");
    }
    assert!(!drawn(&mut h, "leaf-chips-short"), "its pill fits");
    assert!(
        within(&mut h, "leaf-pill-short", "leaf-subline-short"),
        "at the subline's end"
    );
    assert!(drawn(&mut h, "leaf-chips-long"), "the pill moved below");
    assert!(within(&mut h, "leaf-pill-long", "leaf-chips-long"));
    assert!(
        !drawn(&mut h, "leaf-chips-loose"),
        "an unbound pill that does not fit leaves no chips"
    );
    assert!(!drawn(&mut h, "leaf-pill-loose"));
    assert!(h.bounds("leaf-short").size.height > px(26.0), "two lines");
    assert_eq!(
        h.root(|root, _| root.leaf_hover("short")).as_deref(),
        Some("short"),
        "the hover leaves the drawn subline out"
    );

    h.click_on("sidebar-view-tabs");
    assert_eq!(
        subline(&mut h, "short").as_deref(),
        Some("r1:main"),
        "the tabs view names the repo"
    );
}

#[gpui::test]
fn compact_leaf_is_one_line_and_its_hover_holds_the_subline(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    std::fs::write(
        dir.path().join("native-ui.json"),
        r#"{ "general": { "leaf_density": "compact" } }"#,
    )
    .expect("write the layout");
    let mut fixture = density_fixture();
    fixture.sessions.push(
        session("sh")
            .shell("D:/src/app")
            .program_name("pwsh")
            .build(),
    );
    let mut h = Harness::with(cx, &dir, &fixture);

    for id in ["short", "long", "sh"] {
        assert_eq!(
            h.bounds(&format!("leaf-{id}")).size.height,
            px(26.0),
            "{id}"
        );
        assert!(!drawn(&mut h, &format!("leaf-subline-{id}")), "{id}");
        assert!(!drawn(&mut h, &format!("leaf-chips-{id}")), "{id}");
    }
    assert!(drawn(&mut h, "leaf-pill-long"), "chips stay inline");
    let hover = |h: &mut Harness<'_>, id: &str| {
        let id = id.to_owned();
        h.root(move |root, _| root.leaf_hover(&id))
    };
    assert_eq!(
        hover(&mut h, "long"),
        Some(format!("long\n{LONG_BRANCH}")),
        "the hover gains the subline"
    );
    assert_eq!(
        hover(&mut h, "sh").as_deref(),
        Some("app\nSession: sh\nCwd: D:/src/app"),
        "a shell's hover already names its folder"
    );
}

/// Add a repo session routed to `DeepSeek` to the density fixture.
fn with_deepseek(fixture: &mut Fixture) {
    fixture.sessions.push(
        session("deep")
            .in_repo("r1")
            .agent_chips(serde_json::json!([{ "text": "DeepSeek", "tip": "Runs on DeepSeek" }]))
            .build(),
    );
}

#[gpui::test]
fn leaf_shows_the_deepseek_chip_in_both_densities(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = density_fixture();
    with_deepseek(&mut fixture);

    {
        let mut h = Harness::with(cx, &dir, &fixture);
        assert!(drawn(&mut h, "leaf-provider-deep"), "Comfortable");
        assert!(
            !drawn(&mut h, "leaf-provider-short"),
            "an Anthropic leaf carries no provider chip"
        );
    }

    std::fs::write(
        dir.path().join("native-ui.json"),
        r#"{ "general": { "leaf_density": "compact" } }"#,
    )
    .expect("write the layout");
    let mut h = Harness::with(cx, &dir, &fixture);
    assert!(drawn(&mut h, "leaf-provider-deep"), "Compact");
    assert!(!drawn(&mut h, "leaf-provider-short"));
}

#[gpui::test]
fn a_leaf_hover_names_its_provider(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = density_fixture();
    with_deepseek(&mut fixture);
    let mut h = Harness::with(cx, &dir, &fixture);

    assert_eq!(
        h.root(|root, _| root.leaf_hover("deep")).as_deref(),
        Some("deep\nRuns on DeepSeek")
    );
    assert_eq!(
        h.root(|root, _| root.leaf_hover("short")).as_deref(),
        Some("short"),
        "an Anthropic leaf gains no line"
    );
}

/// A harness with `s1` in registered repo `r1`, so the spawn dialog can
/// open, and the ⋯ menu open.
fn more_menu_open_with_a_repo<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut fixture = Fixture::single(session("s1").in_repo("r1").build());
    fixture.repos = vec![repo("r1", "D:/src/r1")];
    let mut h = Harness::with(cx, dir, &fixture);
    h.click_on("sidebar-more");
    assert!(h.root(|root, _| root.sidebar_more_open()));
    h
}

#[gpui::test]
fn ctrl_n_closes_the_more_menu_and_opens_the_spawn_dialog(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = more_menu_open_with_a_repo(cx, &dir);

    h.keys("ctrl-n");

    assert!(h.root(|root, _| root.spawn_dialog_open()));
    assert!(
        !h.root(|root, _| root.sidebar_more_open()),
        "the menu gave way"
    );
}

#[gpui::test]
fn ctrl_comma_closes_the_more_menu_and_opens_settings(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = more_menu_open_with_a_repo(cx, &dir);

    h.keys("ctrl-,");

    assert!(h.root(|root, _| root.settings_open()));
    assert!(
        !h.root(|root, _| root.sidebar_more_open()),
        "the menu gave way"
    );
}

/// A right press outside the ⋯ menu gives way like a left one, and reaches
/// nothing under it: the tab it was aimed at opens no menu of its own.
#[gpui::test]
fn a_right_click_outside_closes_the_more_menu(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = more_menu_open_with_a_repo(cx, &dir);

    h.right_click_on("tab-t1");

    assert!(
        !h.root(|root, _| root.sidebar_more_open()),
        "the menu gave way"
    );
    assert!(
        h.root(|root, _| root.tab_menu().is_none()),
        "and opened nothing under it"
    );
}

#[gpui::test]
fn repo_container_draws_its_icon_tag_count_and_attention_badge(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("s1").in_repo("r1").build());
    fixture.repos = vec![repo("r1", "D:/src/r1")];
    let mut h = Harness::with(cx, &dir, &fixture);

    for part in ["icon-repo", "tag", "count"] {
        assert!(
            drawn(&mut h, &format!("container-repo:r1-{part}")),
            "{part}"
        );
    }
    assert!(!drawn(&mut h, "container-repo:r1-badge"), "no badge yet");

    h.send(DaemonMessage::Attention {
        session_id: "s1".to_owned(),
        reason: AttentionReason::AwaitingInput,
    });
    assert!(
        drawn(&mut h, "container-repo:r1-badge"),
        "attention badges it"
    );
}

#[gpui::test]
fn a_plain_folder_repo_draws_dir_and_the_folder_icon(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut plain = repo("r3", "D:/notes");
    plain.is_git = false;
    let fixture = Fixture {
        repos: vec![repo("r1", "D:/src/r1"), plain],
        sessions: vec![session("s3").in_repo("r3").build()],
        tabs: vec![tab("t1", &pane("p1", Some("s3")))],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);

    assert!(
        drawn(&mut h, "container-repo:r3-icon-folder"),
        "the folder icon"
    );
    assert!(
        !drawn(&mut h, "container-repo:r3-icon-repo"),
        "not the branch icon"
    );
    assert!(drawn(&mut h, "container-repo:r3-tag"));
    let tag = h.root(|root, _| {
        root.sidebar_containers()
            .iter()
            .find(|c| c.key == "repo:r3")
            .map(|c| c.tag().to_owned())
    });
    assert_eq!(tag.as_deref(), Some("DIR"), "the row reads DIR");
    assert!(drawn(&mut h, "container-repo:r1-icon-repo"), "a git repo");
}

/// Repos `ra`, `rb` and `rc`, manual order by name: `rb` holds a working
/// session, `rc` one awaiting input, `ra` none.
fn sort_fixture() -> Fixture {
    Fixture {
        repos: vec![
            repo("ra", "D:/src/ra"),
            repo("rb", "D:/src/rb"),
            repo("rc", "D:/src/rc"),
        ],
        sessions: vec![
            session("busy").in_repo("rb").status("working").build(),
            session("asks")
                .in_repo("rc")
                .status("awaiting_input")
                .build(),
        ],
        tabs: vec![tab("t1", &pane("p1", None))],
        ..Fixture::default()
    }
}

/// The repo rows' keys, top to bottom as painted.
fn painted_repo_rows(h: &mut Harness<'_>, keys: &[&str]) -> Vec<String> {
    let mut rows: Vec<(f32, String)> = keys
        .iter()
        .map(|key| {
            let bounds = h.bounds(&format!("container-{key}"));
            assert!(bounds.origin.x >= px(0.0), "{key} is painted");
            (bounds.origin.y / px(1.0), (*key).to_owned())
        })
        .collect();
    rows.sort_by(|a, b| a.0.total_cmp(&b.0));
    rows.into_iter().map(|(_, key)| key).collect()
}

#[gpui::test]
fn needs_you_on_top_floats_the_repo_that_needs_you(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &sort_fixture());

    assert_eq!(
        &tree_keys(&mut h)[..3],
        ["repo:rc", "repo:rb", "repo:ra"],
        "the repo that needs you, then the active one, then the rest"
    );
    assert_eq!(
        painted_repo_rows(&mut h, &["repo:ra", "repo:rb", "repo:rc"]),
        ["repo:rc", "repo:rb", "repo:ra"],
        "the rows read in that order"
    );
}

#[gpui::test]
fn manual_sort_keeps_the_saved_order(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    std::fs::write(
        dir.path().join("native-ui.json"),
        r#"{ "general": { "repo_sort": "manual" } }"#,
    )
    .expect("write the layout");
    let mut h = Harness::with(cx, &dir, &sort_fixture());

    assert_eq!(&tree_keys(&mut h)[..3], ["repo:ra", "repo:rb", "repo:rc"]);
    assert_eq!(
        painted_repo_rows(&mut h, &["repo:ra", "repo:rb", "repo:rc"]),
        ["repo:ra", "repo:rb", "repo:rc"]
    );
}

#[gpui::test]
fn busy_only_hides_idle_sessions_under_repos_and_keeps_the_count(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    std::fs::write(
        dir.path().join("native-ui.json"),
        r#"{ "general": { "sessions_under_repos": "busy" } }"#,
    )
    .expect("write the layout");
    // The focused pane holds a shell, so neither repo session is the one a
    // focused-pane rule would keep on screen.
    let mut fixture = Fixture::single(
        session("sh")
            .shell("D:/src/app")
            .program_name("pwsh")
            .build(),
    );
    fixture.repos = vec![repo("r1", "D:/src/r1")];
    fixture
        .sessions
        .push(session("s1").in_repo("r1").status("working").build());
    fixture.sessions.push(session("s2").in_repo("r1").build());
    let mut h = Harness::with(cx, &dir, &fixture);

    assert!(drawn(&mut h, "leaf-s1"), "the working session shows");
    assert!(!drawn(&mut h, "leaf-s2"), "the idle one does not");
    assert!(
        drawn(&mut h, "container-repo:r1-count"),
        "the row still counts its sessions"
    );
    assert_eq!(
        h.root(|root, _| root
            .sidebar_containers()
            .into_iter()
            .find(|container| container.key == "repo:r1")
            .map(|container| container.leaves.len())),
        Some(2),
        "the count is every session of the repo"
    );
}

#[gpui::test]
fn none_hides_every_session_under_repos_but_not_shell_rows(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    std::fs::write(
        dir.path().join("native-ui.json"),
        r#"{ "general": { "sessions_under_repos": "none" } }"#,
    )
    .expect("write the layout");
    // The focused pane holds a shell, so the repo's session is not the one a
    // focused-pane rule would keep on screen.
    let mut fixture = Fixture::single(
        session("sh")
            .shell("D:/src/app")
            .program_name("pwsh")
            .build(),
    );
    fixture.repos = vec![repo("r1", "D:/src/r1")];
    fixture
        .sessions
        .push(session("s1").in_repo("r1").status("awaiting_input").build());
    let mut h = Harness::with(cx, &dir, &fixture);

    assert!(!drawn(&mut h, "leaf-s1"), "no leaf under the repo");
    assert!(drawn(&mut h, "container-repo:r1-count"), "its count stays");
    assert!(drawn(&mut h, "leaf-sh"), "a shell row keeps its leaf");
}

#[gpui::test]
fn focused_session_stays_under_its_repo_when_sessions_under_repos_is_none(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    std::fs::write(
        dir.path().join("native-ui.json"),
        r#"{ "general": { "sessions_under_repos": "none" } }"#,
    )
    .expect("write the layout");
    let mut fixture = Fixture::single(session("s1").in_repo("r1").build());
    fixture.repos = vec![repo("r1", "D:/src/r1")];
    fixture.sessions.push(session("s2").in_repo("r1").build());
    let mut h = Harness::with(cx, &dir, &fixture);

    assert!(
        drawn(&mut h, "leaf-s1"),
        "the focused pane's session keeps its row"
    );
    assert!(!drawn(&mut h, "leaf-s2"), "the other one is hidden");
}

const FULL_SUMMARY: &str =
    "Claude · Interactive · new worktree from main · Accept edits · opus · 2 env";

fn summary_config(target: SpawnTarget) -> SpawnConfig {
    SpawnConfig {
        target,
        mode: SessionMode::Interactive,
        dangerously_skip_permissions: false,
        agent: Some(serde_json::from_value(serde_json::json!({ "agent": "claude", "options": { "permission_mode": "accept_edits", "model": "opus" } })).expect("agent selection")),
        extra_args: Vec::new(),
        extra_env: vec![
            ("A".to_owned(), "1".to_owned()),
            ("B".to_owned(), "2".to_owned()),
        ],
    }
}

fn single_target() -> SpawnTarget {
    SpawnTarget::Single {
        repo_id: "r1".to_owned(),
        branch_name: "feat/x".to_owned(),
        base_branch: Some("main".to_owned()),
        use_worktree: true,
        checkout_strategy: None,
        worktree_reuse: WorktreeReusePolicy::default(),
        existing_worktree: None,
    }
}

fn workspace_target() -> SpawnTarget {
    SpawnTarget::Workspace {
        workspace_id: "w1".to_owned(),
        branch_name: "feat/x".to_owned(),
        base_branch: Some("main".to_owned()),
        use_worktree: true,
        worktree_reuse: WorktreeReusePolicy::default(),
        existing_worktrees: Vec::new(),
    }
}

fn container_summary(h: &mut Harness<'_>, key: &str) -> Option<String> {
    h.root(|root, _| {
        root.sidebar_containers()
            .into_iter()
            .find(|container| container.key == key)
            .and_then(|container| container.summary)
    })
}

fn container_tooltip(h: &mut Harness<'_>, key: &str) -> Option<String> {
    h.root(|root, _| {
        root.sidebar_containers()
            .into_iter()
            .find(|container| container.key == key)
            .and_then(|container| container.tooltip())
    })
}

#[gpui::test]
fn repo_row_shows_its_last_launch_summary(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("s1").in_repo("r1").build());
    fixture.repos = vec![repo("r1", "D:/src/r1")];
    let mut h = Harness::with(cx, &dir, &fixture);

    assert!(
        !painted(&mut h, "container-repo:r1-summary"),
        "a repo that never launched has no summary"
    );
    assert_eq!(
        h.bounds("container-repo:r1").size.height,
        px(30.0),
        "one line"
    );

    let mut launched = repo("r1", "D:/src/r1");
    launched.last_spawn_config = Some(summary_config(single_target()));
    h.send(DaemonMessage::Repos {
        repos: vec![launched],
    });

    assert_eq!(
        container_summary(&mut h, "repo:r1").as_deref(),
        Some(FULL_SUMMARY),
        "the model carries the summary"
    );
    assert_eq!(
        container_tooltip(&mut h, "repo:r1"),
        Some(format!("Last launch: {FULL_SUMMARY}"))
    );
    assert!(painted(&mut h, "container-repo:r1-summary"), "drawn");
    assert_eq!(
        h.bounds("container-repo:r1").size.height,
        px(46.0),
        "two lines"
    );
    assert!(
        h.bounds("container-repo:r1-summary").top() >= h.bounds("container-repo:r1-tag").bottom(),
        "on the second line"
    );
}

#[gpui::test]
fn workspace_row_shows_its_last_launch_summary(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        repos: vec![repo("r1", "D:/src/r1")],
        workspaces: vec![workspace("w1", &["r1"])],
        sessions: vec![session("s1").in_workspace("w1").build()],
        tabs: vec![tab("t1", &pane("p1", Some("s1")))],
    };
    let mut h = Harness::with(cx, &dir, &fixture);

    assert!(
        !painted(&mut h, "container-ws:w1-summary"),
        "a workspace that never launched has no summary"
    );
    assert_eq!(
        h.bounds("container-ws:w1").size.height,
        px(30.0),
        "one line"
    );

    let mut launched = workspace("w1", &["r1"]);
    launched.last_spawn_config = Some(summary_config(workspace_target()));
    h.send(DaemonMessage::Workspaces {
        workspaces: vec![launched],
    });

    assert_eq!(
        container_summary(&mut h, "ws:w1").as_deref(),
        Some(FULL_SUMMARY),
        "the model carries the summary"
    );
    assert_eq!(
        container_tooltip(&mut h, "ws:w1"),
        Some(format!("Last launch: {FULL_SUMMARY}"))
    );
    assert!(painted(&mut h, "container-ws:w1-summary"), "drawn");
    assert_eq!(
        h.bounds("container-ws:w1").size.height,
        px(46.0),
        "two lines"
    );
    assert!(
        h.bounds("container-ws:w1-summary").top() >= h.bounds("container-ws:w1-tag").bottom(),
        "on the second line"
    );
}

#[gpui::test]
fn container_row_enter_and_space_fold_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("s1").in_repo("r1").build());
    fixture.repos = vec![repo("r1", "D:/src/r1"), repo("r2", "D:/src/r2")];
    fixture.sessions.push(session("s2").in_repo("r2").build());
    let mut h = Harness::with(cx, &dir, &fixture);
    h.answer_scrollback("s1", b"");
    h.sent_input("s1");

    h.keys("enter");
    assert_eq!(
        h.sent_input("s1"),
        b"\r",
        "with no row focus the terminal takes Enter"
    );

    h.click_on("container-repo:r1");
    assert_eq!(
        h.root(|root, _| root.sidebar_row_focus().map(str::to_owned)),
        Some("repo:r1".to_owned())
    );
    assert!(
        drawn(&mut h, "container-repo:r1-focus"),
        "the ring follows the row"
    );
    assert!(!h.in_model("leaf-s1"), "the click folded it");

    h.sent_input("s1");
    h.keys("enter");
    assert!(h.in_model("leaf-s1"), "Enter expands it");
    assert!(h.sent_input("s1").is_empty(), "Enter stays in the sidebar");
    h.keys("space");
    assert!(!h.in_model("leaf-s1"), "Space collapses it");
    assert!(h.sent_input("s1").is_empty(), "Space stays in the sidebar");
    assert!(
        saved_ui(&dir)["collapsed_containers"]
            .as_array()
            .is_some_and(|keys| keys.iter().any(|key| key == "repo:r1")),
        "the fold is saved"
    );

    h.keys("down");
    assert_eq!(
        h.root(|root, _| root.sidebar_row_focus().map(str::to_owned)),
        Some("repo:r2".to_owned()),
        "Down moves the ring"
    );
    assert!(h.sent_input("s1").is_empty(), "Down stays in the sidebar");
    h.keys("up");
    assert_eq!(
        h.root(|root, _| root.sidebar_row_focus().map(str::to_owned)),
        Some("repo:r1".to_owned()),
        "Up moves it back"
    );
    assert!(h.sent_input("s1").is_empty(), "Up stays in the sidebar");

    h.keys("escape");
    assert_eq!(
        h.root(|root, _| root.sidebar_row_focus().map(str::to_owned)),
        None,
        "Escape clears the row focus"
    );
    h.keys("a");
    assert_eq!(
        h.sent_input("s1"),
        b"a",
        "Escape returns the keyboard to the terminal"
    );

    // A menu takes the keyboard: the row keys no longer act on the row.
    h.click_on("container-repo:r1");
    h.click_on("sidebar-more");
    h.keys("escape");
    h.sent_input("s1");
    h.keys("enter");
    assert_eq!(
        h.sent_input("s1"),
        b"\r",
        "the row keys stop once the sidebar loses focus"
    );

    // An armed confirm survives the Escape that clears the row focus.
    let closes = |sent: &[ClientMessage], id: &str| {
        sent.iter()
            .filter(|message| matches!(message, ClientMessage::CloseTab { tab_id } if tab_id == id))
            .count()
    };
    h.click_on("tab-close-t1");
    assert!(h.sent().is_empty(), "the first close click only arms");
    // Focusing the row without a press: a press anywhere drops the arm.
    h.root_update(|root, window, cx| root.focus_container_row("repo:r1", window, cx));
    assert_eq!(
        h.root(|root, _| root.sidebar_row_focus().map(str::to_owned)),
        Some("repo:r1".to_owned())
    );
    h.keys("escape");
    assert_eq!(
        h.root(|root, _| root.sidebar_row_focus().map(str::to_owned)),
        None,
        "Escape clears the row focus first"
    );
    h.click_on("tab-close-t1");
    assert_eq!(
        closes(&h.sent(), "t1"),
        1,
        "the armed confirm survived that Escape"
    );
}

/// Two repos, with the keyboard on r1's row (folded by that click).
fn focused_container_row<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut fixture = Fixture::single(session("s1").in_repo("r1").build());
    fixture.repos = vec![repo("r1", "D:/src/r1"), repo("r2", "D:/src/r2")];
    fixture.sessions.push(session("s2").in_repo("r2").build());
    let mut h = Harness::with(cx, dir, &fixture);
    h.answer_scrollback("s1", b"");
    h.click_on("container-repo:r1");
    assert_eq!(
        h.root(|root, _| root.sidebar_row_focus().map(str::to_owned)),
        Some("repo:r1".to_owned())
    );
    assert!(!h.in_model("leaf-s1"), "the click folded it");
    h.sent_input("s1");
    h.sent();
    h
}

/// Back on the sessions panel: it is shown, no row holds the keyboard, and
/// r1 is still folded.
fn back_on_sessions_with_r1_still_folded(h: &mut Harness<'_>) {
    h.click_on("activity-sessions");
    assert!(
        h.root(|root, _| !root.sidebar_collapsed() && root.activity() == Activity::Sessions),
        "the sessions panel is shown"
    );
    assert_eq!(
        h.root(|root, _| root.sidebar_row_focus().map(str::to_owned)),
        None,
        "the row focus does not come back"
    );
    assert!(!h.in_model("leaf-s1"), "r1 is still folded");
}

/// A switch to `activity` hides the container list: the row focus goes with
/// it, so Enter folds nothing.
fn switching_away_clears_the_row_focus(cx: &mut TestAppContext, activity: &str) {
    let dir = TestDir::new();
    let mut h = focused_container_row(cx, &dir);
    h.click_on(activity);
    assert_eq!(
        h.root(|root, _| root.sidebar_row_focus().map(str::to_owned)),
        None,
        "the switch clears the row focus"
    );
    h.keys("enter");
    assert!(
        h.sent_input("s1").is_empty(),
        "nothing reaches the terminal"
    );
    back_on_sessions_with_r1_still_folded(&mut h);
}

#[gpui::test]
fn switching_to_source_control_clears_the_row_focus(cx: &mut TestAppContext) {
    switching_away_clears_the_row_focus(cx, "activity-source-control");
}

#[gpui::test]
fn switching_to_needs_you_clears_the_row_focus(cx: &mut TestAppContext) {
    switching_away_clears_the_row_focus(cx, "activity-needs-you");
}

#[gpui::test]
fn collapsing_the_sidebar_clears_the_row_focus(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = focused_container_row(cx, &dir);
    h.keys("ctrl-b");
    assert_eq!(
        h.root(|root, _| root.sidebar_row_focus().map(str::to_owned)),
        None,
        "the fold clears the row focus"
    );
    h.keys("enter");
    back_on_sessions_with_r1_still_folded(&mut h);
    h.keys("enter");
    assert!(
        !h.in_model("leaf-s1"),
        "Enter after the unfold folds nothing"
    );
}

#[gpui::test]
fn resume_all_appears_at_two_abandoned_and_sends_resume_all(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(
        cx,
        &dir,
        &Fixture::single(session("s1").abandoned().build()),
    );
    h.sent();
    assert_eq!(h.root(|root, _| root.sidebar_abandoned_count()), 1);

    h.click_on("sidebar-more");
    assert!(
        !painted(&mut h, "more-resume-all"),
        "one abandoned session uses its leaf's Resume"
    );
    assert!(
        !painted(&mut h, "more-resume-all-separator"),
        "so its separator is absent too"
    );

    h.send(DaemonMessage::Sessions {
        sessions: vec![
            session("s1").abandoned().build(),
            session("s2").abandoned().build(),
        ],
    });
    assert_eq!(h.root(|root, _| root.sidebar_abandoned_count()), 2);
    assert_eq!(
        resume_all_label(2).as_deref(),
        Some("Resume all abandoned (2)")
    );
    assert!(
        painted(&mut h, "more-resume-all"),
        "two abandoned sessions offer the bulk resume"
    );
    assert!(
        painted(&mut h, "more-resume-all-separator"),
        "the line above it is drawn"
    );
    assert!(
        h.bounds("sidebar-more-add-repo").bottom() <= h.bounds("more-resume-all-separator").top(),
        "the separator sits under the menu's other rows"
    );
    assert!(
        h.bounds("more-resume-all-separator").bottom() <= h.bounds("more-resume-all").top(),
        "and above Resume all"
    );

    h.click_on("more-resume-all");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::ResumeAllAbandoned]),
        "sent {sent:?}"
    );

    h.send(DaemonMessage::Sessions {
        sessions: vec![session("s1").abandoned().build()],
    });
    assert_eq!(
        h.root(|root, _| root.sidebar_abandoned_count()),
        1,
        "the count follows Sessions"
    );
}

/// The DIR container's folder banner selectors; the key carries the
/// normalized path.
const DIR_BANNER: &str = "container-cwd:D:\\scratch\\dir-banner";
const DIR_ADD_REPO: &str = "container-cwd:D:\\scratch\\dir-add-repo";
const DIR_ADD_WORKSPACE: &str = "container-cwd:D:\\scratch\\dir-add-workspace";
/// The SH container's folder banner selectors.
const SH_BANNER: &str = "container-standalone:sh-banner";
const SH_ADD_REPO: &str = "container-standalone:sh-add-repo";
const SH_ADD_WORKSPACE: &str = "container-standalone:sh-add-workspace";

/// A `.code-workspace` file in `D:/elsewhere`, the SH container's folder.
fn suggestion() -> VscodeWorkspaceSuggestion {
    VscodeWorkspaceSuggestion {
        source_path: "D:/elsewhere/team.code-workspace".to_owned(),
        suggested_name: "team".to_owned(),
        folders: vec![VscodeWorkspaceFolder {
            path: "D:/elsewhere".to_owned(),
            name: None,
            matched_repo_id: None,
        }],
    }
}

/// A DIR session at `D:/scratch/dir`, a standalone SH at `D:/elsewhere`, a
/// standalone with no cwd, a repo and a workspace session, and one whose
/// repo is gone (Detached).
fn banner_fixture() -> Fixture {
    Fixture {
        repos: vec![repo("r1", "D:/src/r1"), repo("r2", "D:/src/r2")],
        workspaces: vec![workspace("ws1", &["r1"])],
        sessions: vec![
            session("dir").dir_shell("D:/scratch/dir").build(),
            session("sh").shell("D:/elsewhere").build(),
            session("nocwd").shell("").build(),
            session("repo").in_repo("r2").build(),
            session("ws").in_workspace("ws1").build(),
            session("gone").in_repo("r9").abandoned().build(),
        ],
        tabs: vec![tab("t1", &pane("p1", None))],
    }
}

/// The paths the client asked the daemon to scan since the last call.
fn scanned_paths(h: &mut Harness<'_>) -> Vec<String> {
    h.sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::ScanVscodeWorkspaces { path } => Some(path),
            _ => None,
        })
        .collect()
}

#[gpui::test]
fn dir_and_shell_containers_draw_their_folder_banner(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &banner_fixture());

    assert!(h.in_model(DIR_BANNER), "the DIR's folder");
    assert!(h.in_model(SH_BANNER), "the SH's folder");
    assert!(painted(&mut h, DIR_BANNER), "drawn");
    let row = h.bounds("container-cwd:D:\\scratch\\dir");
    let banner = h.bounds(DIR_BANNER);
    assert!(
        row.bottom() <= banner.top(),
        "under its container row, above its leaves"
    );

    h.click_on("container-cwd:D:\\scratch\\dir");
    assert!(!h.in_model(DIR_BANNER), "folded with its container");
    assert!(!h.in_model(DIR_ADD_REPO));
    assert!(!h.in_model(DIR_ADD_WORKSPACE));
}

#[gpui::test]
fn only_dir_and_shell_containers_draw_a_folder_banner(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &banner_fixture());

    for key in ["ws:ws1", "repo:r2", "detached", "standalone:nocwd"] {
        let banner = format!("container-{key}-banner");
        assert!(!h.in_model(&banner), "{key} draws no banner");
        assert!(!painted(&mut h, &banner), "{key} paints no banner");
        let add_repo = format!("container-{key}-add-repo");
        assert!(!h.in_model(&add_repo), "{key} offers no Add repo");
    }

    h.click_on("sidebar-view-tabs");
    assert!(
        !h.in_model("container-tab:t1-banner"),
        "a tab draws no banner"
    );
    assert!(
        !painted(&mut h, "container-tab:t1-banner"),
        "a tab paints no banner"
    );
    assert!(!h.in_model("container-unbound-banner"), "nor does Unbound");
    assert!(
        !painted(&mut h, "container-unbound-banner"),
        "nor does Unbound paint one"
    );
}

#[gpui::test]
fn banner_add_repo_sends_the_folder_without_a_picker(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &banner_fixture());
    h.sent();

    h.click_on(DIR_ADD_REPO);
    let sent = h.sent();
    assert_eq!(sent.len(), 1, "sent {sent:?}");
    assert!(
        matches!(&sent[0], ClientMessage::AddRepo { path, name: None }
            if path == "D:\\scratch\\dir"),
        "the container's folder, named by the daemon"
    );
    assert_eq!(h.folder_asks(), 0, "no picker opened");
}

#[gpui::test]
fn the_account_login_shell_draws_no_add_repo_banner(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        sessions: vec![
            session("run-7")
                .shell("D:/scratch/login-run-7")
                .user_label("Log in a new account")
                .account_login()
                .build(),
            session("plain").shell("D:/elsewhere").build(),
        ],
        tabs: vec![tab("t1", &pane("p1", None))],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);

    for selector in [
        "container-standalone:run-7-banner",
        "container-standalone:run-7-add-repo",
    ] {
        assert!(!painted(&mut h, selector), "{selector} is drawn");
    }
    assert_eq!(
        home_of(&mut h, "run-7").map(|(_, name, _, _)| name),
        Some("Log in a new account".to_owned()),
        "the group header reads the name the user gave it"
    );
    assert!(
        painted(&mut h, "container-standalone:plain-banner"),
        "a shell group with no user name keeps its folder banner"
    );
    assert!(
        painted(&mut h, "container-standalone:plain-add-repo"),
        "and its Add repo"
    );
}

#[gpui::test]
fn a_renamed_plain_shell_keeps_its_add_repo_banner(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        sessions: vec![
            session("renamed")
                .shell("D:/scratch/build-box")
                .user_label("build box")
                .build(),
        ],
        tabs: vec![tab("t1", &pane("p1", None))],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);

    for selector in [
        "container-standalone:renamed-banner",
        "container-standalone:renamed-add-repo",
    ] {
        assert!(painted(&mut h, selector), "{selector} is drawn");
    }
    assert_eq!(
        home_of(&mut h, "renamed").map(|(_, name, _, _)| name),
        Some("build box".to_owned()),
        "the group header reads the name the user gave it"
    );
}

#[gpui::test]
fn folder_scans_run_once_per_distinct_path_and_welcome_clears_them(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = banner_fixture();
    let mut h = Harness::with(cx, &dir, &fixture);
    let mut first = scanned_paths(&mut h);
    first.sort();
    assert_eq!(
        first,
        ["D:\\elsewhere", "D:\\scratch\\dir"],
        "one scan per DIR and SH folder"
    );
    assert!(
        !first.iter().any(|path| path == "D:/src/r2"),
        "a repo's folder is never scanned"
    );

    h.send(DaemonMessage::Sessions {
        sessions: fixture.sessions.clone(),
    });
    assert!(
        scanned_paths(&mut h).is_empty(),
        "a second list rescans nothing"
    );

    h.send_welcome(support::PROTOCOL);
    h.sent();
    h.send(DaemonMessage::Sessions {
        sessions: fixture.sessions.clone(),
    });
    let mut rescanned = scanned_paths(&mut h);
    rescanned.sort();
    assert_eq!(
        rescanned, first,
        "Welcome clears the state, so the next list rescans"
    );
}

#[gpui::test]
fn add_workspace_appears_after_the_scan_and_accepts_the_suggestion(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &banner_fixture());

    assert!(!h.in_model(SH_ADD_WORKSPACE), "no scan answer yet");
    assert!(h.in_model(SH_ADD_REPO), "Add repo is always drawn");

    let suggestion = suggestion();
    h.send(DaemonMessage::VscodeWorkspacesScanned {
        path: "D:/elsewhere".to_owned(),
        suggestions: vec![suggestion.clone()],
    });
    assert!(h.in_model(SH_ADD_WORKSPACE), "the scan found one");

    h.sent();
    h.click_on(SH_ADD_WORKSPACE);
    let sent = h.sent();
    assert_eq!(sent.len(), 1, "sent {sent:?}");
    assert!(
        matches!(
            &sent[0],
            ClientMessage::AcceptVscodeWorkspaceSuggestion { suggestion: accepted, watch: false }
                if *accepted == suggestion
        ),
        "the folder's suggestion, accepted without watching"
    );
}

#[gpui::test]
fn empty_scan_answer_draws_no_add_workspace(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &banner_fixture());

    h.send(DaemonMessage::VscodeWorkspacesScanned {
        path: "D:\\scratch\\dir".to_owned(),
        suggestions: Vec::new(),
    });
    assert!(!h.in_model(DIR_ADD_WORKSPACE), "nothing to add");
    assert!(h.in_model(DIR_ADD_REPO), "Add repo stays");

    h.send(DaemonMessage::VscodeWorkspacesScanned {
        path: "D:/scratch/dir".to_owned(),
        suggestions: vec![suggestion()],
    });
    assert!(
        h.in_model(DIR_ADD_WORKSPACE),
        "answers are keyed by the normalized path"
    );
}

#[gpui::test]
fn a_session_update_scans_its_folder_once(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &banner_fixture());
    h.sent();

    h.send(DaemonMessage::SessionUpdated {
        session: session("late").shell("D:/late").build(),
        request_id: None,
    });
    assert_eq!(
        scanned_paths(&mut h),
        ["D:\\late"],
        "a shell spawned after the list scans its folder"
    );

    h.send(DaemonMessage::SessionUpdated {
        session: session("late").shell("D:/late").build(),
        request_id: None,
    });
    assert!(
        scanned_paths(&mut h).is_empty(),
        "the same session's folder is not rescanned"
    );

    h.send(DaemonMessage::SessionUpdated {
        session: session("sh").shell("D:/moved").build(),
        request_id: None,
    });
    assert_eq!(
        scanned_paths(&mut h),
        ["D:\\moved"],
        "a shell moved to a new folder scans it once"
    );
}

/// Windows only: folder keys fold case and separators there
/// (`paths::normalize_path_key`), and a Unix path keeps the case it was
/// given, so two spellings are two folders.
#[cfg(windows)]
#[gpui::test]
fn folder_scans_fold_case_and_answer_both_casings(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        sessions: Vec::new(),
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.sent();

    h.send(DaemonMessage::SessionUpdated {
        session: session("upper").dir_shell("D:/Elsewhere").build(),
        request_id: None,
    });
    h.send(DaemonMessage::SessionUpdated {
        session: session("lower").dir_shell("D:\\elsewhere").build(),
        request_id: None,
    });
    let scans = scanned_paths(&mut h);
    assert_eq!(scans.len(), 1, "one folder, one scan: {scans:?}");
    assert_eq!(scans[0].to_lowercase(), "d:\\elsewhere");

    h.send(DaemonMessage::VscodeWorkspacesScanned {
        path: "D:\\ELSEWHERE".to_owned(),
        suggestions: vec![suggestion()],
    });
    assert!(
        h.root(|root, _| root.cwd_workspace_suggestion("D:/elsewhere").is_some()),
        "the answer is found through the other casing"
    );
    assert!(
        h.root(|root, _| root.cwd_workspace_suggestion("D:\\Elsewhere").is_some()),
        "and through the folder's own casing"
    );
    assert!(h.in_model("container-cwd:D:\\Elsewhere-add-workspace"));
    assert!(h.in_model("container-cwd:D:\\elsewhere-add-workspace"));
}

#[gpui::test]
fn a_registry_change_scans_a_regrouped_shell_folder(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        repos: vec![repo("r1", "D:/src/r1")],
        sessions: vec![session("sh").shell("D:/src/r1").build()],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    assert_eq!(tree_keys(&mut h), ["repo:r1"], "the shell sits in its repo");
    assert!(
        !h.in_model("container-standalone:sh-banner"),
        "so there is no SH container yet"
    );
    h.sent();

    h.send(DaemonMessage::Repos { repos: Vec::new() });
    assert_eq!(tree_keys(&mut h), ["standalone:sh"], "regrouped into SH");
    assert!(h.in_model("container-standalone:sh-banner"));
    assert_eq!(
        scanned_paths(&mut h),
        ["D:\\src\\r1"],
        "the regrouped container's folder is scanned"
    );

    h.send(DaemonMessage::VscodeWorkspacesScanned {
        path: "D:/src/r1".to_owned(),
        suggestions: vec![suggestion()],
    });
    assert!(
        h.in_model("container-standalone:sh-add-workspace"),
        "the answer reaches the new SH container"
    );
}

#[gpui::test]
fn a_long_folder_path_keeps_the_links_on_the_row(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let path = format!("D:/{}", "segment/".repeat(15).trim_end_matches('/'));
    assert!(path.len() >= 120, "the path is long: {}", path.len());
    let key = format!("cwd:{}", path.replace('/', "\\"));
    let fixture = Fixture {
        sessions: vec![session("dir").dir_shell(&path).build()],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.send(DaemonMessage::VscodeWorkspacesScanned {
        path: path.clone(),
        suggestions: vec![suggestion()],
    });

    let banner = format!("container-{key}-banner");
    let add_repo = format!("container-{key}-add-repo");
    let add_workspace = format!("container-{key}-add-workspace");
    assert!(painted(&mut h, &banner), "the banner is drawn");
    assert!(painted(&mut h, &add_repo), "Add repo is drawn");
    assert!(painted(&mut h, &add_workspace), "Add workspace is drawn");

    let panel = h.bounds("sidebar-panel");
    let link = h.bounds(&add_repo);
    assert!(
        link.left() >= panel.left() && link.right() <= panel.right(),
        "the link stays inside the row: {link:?} in {panel:?}"
    );
}

#[gpui::test]
fn leaves_draw_their_runtime_as_an_icon(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        repos: vec![repo("r1", "D:/src/r1")],
        sessions: vec![
            session("s1").in_repo("r1").build(),
            session("s2")
                .shell("D:/elsewhere")
                .program_name("pwsh")
                .build(),
            session("s3")
                .shell("D:/elsewhere")
                .program_name("node")
                .build(),
        ],
        tabs: vec![tab("t1", &pane("p1", None))],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);

    for selector in [
        "leaf-runtime-s1-icon-agent",
        "leaf-runtime-s2-icon-pwsh",
        "leaf-runtime-s3-icon-generic",
    ] {
        assert!(painted(&mut h, selector), "{selector} is drawn");
    }
    let icon = h.root(|root, _| {
        root.sidebar_containers()
            .into_iter()
            .flat_map(|container| container.leaves)
            .find(|leaf| leaf.id == "s1")
            .and_then(|leaf| leaf.icon)
    });
    assert!(
        matches!(icon, Some(RuntimeIcon::Agent(_))),
        "the Claude leaf wears its agent's icon, not the text `claude`: {icon:?}"
    );
    assert_eq!(
        tags(&mut h, "s1"),
        [tag("claude", "Running claude")],
        "the tooltip still names the runtime"
    );
}

/// Session `s1` in repo `r1` at `C:\r1`.
fn runs_fixture() -> Fixture {
    Fixture {
        repos: vec![repo("r1", "C:\\r1")],
        ..Fixture::single(session("s1").build())
    }
}

/// The daemon's run list: run `d1` in a worktree of `C:\r1`.
fn runs_msg() -> DaemonMessage {
    DaemonMessage::DispatchRuns {
        runs: vec![
            dispatch_run("d1")
                .worktree("C:\\r1.wt\\a")
                .main_checkout("C:\\r1")
                .build(),
        ],
    }
}

#[gpui::test]
fn dispatch_runs_show_as_leaves_from_a_daemon_with_the_feature(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &runs_fixture());
    h.reconnect_with_features(&[protocol::FEATURE_DISPATCH_RUNS]);

    h.send(runs_msg());

    assert!(h.in_model("leaf-d1"), "the run shows as a leaf");
}

/// Session `s2` focused in pane `p1`, and session `s1` of conversation `c1`
/// beside it in repo `r1`, with run `d1` that `s1` dispatched.
fn nested_run_harness<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut fixture = Fixture::single(session("s2").in_repo("r1").build());
    fixture.repos = vec![repo("r1", "C:\\r1")];
    fixture
        .sessions
        .push(session("s1").in_repo("r1").conversation_id("c1").build());
    let mut h = Harness::with(cx, dir, &fixture);
    h.reconnect_with_features(&[protocol::FEATURE_DISPATCH_RUNS]);
    h.send(DaemonMessage::DispatchRuns {
        runs: vec![
            dispatch_run("d1")
                .worktree("C:\\r1.wt\\a")
                .main_checkout("C:\\r1")
                .session_id("c1")
                .build(),
        ],
    });
    h.answer_scrollback("s2", b"");
    h
}

#[gpui::test]
fn a_dispatched_run_draws_indented_right_after_its_host_session(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = nested_run_harness(cx, &dir);

    let host = h.bounds("leaf-s1");
    let run = h.bounds("leaf-d1");
    assert!(drawn(&mut h, "leaf-d1"), "the run draws");
    assert!(
        run.top() >= host.bottom(),
        "the run draws below its host: {run:?} and {host:?}"
    );
    assert_eq!(
        run.left() - host.left(),
        px(14.0),
        "the run's card sits one level right of its host's"
    );
}

#[gpui::test]
fn the_session_fold_hides_and_shows_its_runs_and_saves_the_key(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = nested_run_harness(cx, &dir);
    assert!(drawn(&mut h, "leaf-d1"), "precondition: the run draws");
    // gpui drops a gone element's bounds, so the row after the run proves
    // it left: s2 moves up into the place the run had.
    let open = h.bounds("leaf-s2").top();
    let run_bottom = h.bounds("leaf-d1").bottom();
    assert!(open >= run_bottom, "precondition: s2 draws after the run");
    let focused = h.root(|root, _| root.pane_session("p1"));
    h.sent();

    h.click_on("sidebar-session-fold-s1");

    assert!(
        h.bounds("leaf-s2").top() < run_bottom,
        "the fold hides the run: s2 takes its place"
    );
    assert!(
        saved_ui(&dir)["collapsed_containers"]
            .as_array()
            .is_some_and(|keys| keys.iter().any(|key| key == "session:s1")),
        "the fold is saved"
    );
    assert_eq!(
        h.root(|root, _| root.pane_session("p1")),
        focused,
        "the fold does not select the host"
    );
    let sent = h.sent();
    assert!(sent.is_empty(), "the fold sends nothing: {sent:?}");

    h.click_on("sidebar-session-fold-s1");

    assert_eq!(
        h.bounds("leaf-s2").top(),
        open,
        "a second click shows the run again"
    );
    assert!(
        saved_ui(&dir)["collapsed_containers"]
            .as_array()
            .is_some_and(|keys| keys.iter().all(|key| key != "session:s1")),
        "the open fold leaves the saved set"
    );
}

#[gpui::test]
fn a_session_without_runs_draws_no_fold(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = nested_run_harness(cx, &dir);

    assert!(
        drawn(&mut h, "sidebar-session-fold-s1"),
        "precondition: the host draws its fold"
    );
    assert!(
        !drawn(&mut h, "sidebar-session-fold-s2"),
        "a session with no runs draws none"
    );
}

#[gpui::test]
fn dispatch_runs_from_a_daemon_without_the_feature_are_ignored(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &runs_fixture());

    h.send(runs_msg());

    assert!(
        !h.in_model("leaf-d1"),
        "a daemon without the feature lists no run"
    );
}

#[gpui::test]
fn a_welcome_clears_the_hosts_runs(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &runs_fixture());
    h.reconnect_with_features(&[protocol::FEATURE_DISPATCH_RUNS]);
    h.send(runs_msg());
    assert!(h.in_model("leaf-d1"), "precondition: the run shows");

    h.reconnect_with_features(&[protocol::FEATURE_DISPATCH_RUNS]);

    assert!(!h.in_model("leaf-d1"), "a new connection forgets the runs");
}

#[gpui::test]
fn a_removed_run_leaves_the_sidebar(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &runs_fixture());
    h.reconnect_with_features(&[protocol::FEATURE_DISPATCH_RUNS]);
    h.send(runs_msg());
    assert!(h.in_model("leaf-d1"), "precondition: the run shows");

    h.send(DaemonMessage::DispatchRunRemoved {
        run_id: "d1".to_owned(),
    });

    assert!(!h.in_model("leaf-d1"), "the removed run's leaf goes");
}

#[gpui::test]
fn clicking_a_dispatch_leaf_opens_a_tab_following_the_run(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &runs_fixture());
    h.reconnect_with_features(&[protocol::FEATURE_DISPATCH_RUNS]);
    h.send(runs_msg());
    h.answer_scrollback("s1", b"");
    h.sent();

    h.click_on("leaf-d1");

    let sent = h.sent();
    assert!(
        matches!(
            sent.as_slice(),
            [ClientMessage::CreateTab {
                name: None,
                initial_session_id: None,
                host: None,
                follow: Some(PaneFollow::DispatchRun { run_id }),
                request_id: Some(_),
            }] if run_id == "d1"
        ),
        "sent {sent:?}"
    );
}

#[gpui::test]
fn a_dispatch_leaf_draws_the_ds_tag_and_no_pill(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &runs_fixture());
    h.reconnect_with_features(&[protocol::FEATURE_DISPATCH_RUNS]);
    h.send(runs_msg());

    assert!(
        drawn(&mut h, "leaf-runtime-d1"),
        "the leaf draws its DS tag"
    );
    assert!(
        !drawn(&mut h, "leaf-pill-d1"),
        "a dispatch leaf draws no pill"
    );
}

/// The pane the window's active tab has focused.
fn focused_pane(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.focused_pane())
}

#[gpui::test]
fn clicking_a_followed_dispatch_leaf_focuses_its_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        tabs: vec![tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                pane("p1", Some("s1")),
                follow_pane("p2", "d1"),
            ),
        )],
        ..runs_fixture()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.reconnect_with_features(&[protocol::FEATURE_DISPATCH_RUNS]);
    h.send(runs_msg());
    h.answer_scrollback("s1", b"");
    h.sent();
    assert_eq!(
        focused_pane(&mut h).as_deref(),
        Some("p1"),
        "the session's pane starts focused"
    );

    h.click_on("leaf-d1");

    let sent = h.sent();
    assert!(
        !sent
            .iter()
            .any(|msg| matches!(msg, ClientMessage::CreateTab { .. })),
        "the click opens no tab: {sent:?}"
    );
    assert_eq!(
        focused_pane(&mut h).as_deref(),
        Some("p2"),
        "the pane that follows the run takes the focus"
    );
}

#[gpui::test]
fn a_remote_run_in_unbound_shows_its_host_chip(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &serde_json::json!({ "saved_hosts": [{ "name": "box.tail1234.ts.net", "port": 8787 }] }),
    );
    let mut h = Harness::with(cx, &dir, &runs_fixture());
    let target = saved_remote();
    h.host(&target)
        .connect_with_features(&[protocol::FEATURE_DISPATCH_RUNS]);
    h.host(&target).send(DaemonMessage::DispatchRuns {
        runs: vec![dispatch_run("d1").worktree("C:\\r1.wt\\b").build()],
    });
    h.host(&target).sent();

    h.click_on("sidebar-view-tabs");

    assert!(
        drawn(&mut h, &format!("leaf-{REMOTE}::d1")),
        "Unbound lists the run"
    );
    assert!(
        drawn(&mut h, "sidebar-host-box-d1"),
        "Unbound's remote run names its host"
    );
}

/// The connection to `target` that was open and then dropped, as a
/// disconnect reports it.
fn dropped(target: Target) -> Connection {
    let mut conn = Connection::for_target(target);
    conn.on_connecting();
    conn.on_socket_open(8787);
    conn.on_welcome(PROTOCOL, Vec::new(), None);
    let _ = conn.on_closed("socket closed".to_owned());
    conn
}

#[gpui::test]
fn a_remote_hosts_run_dims_when_the_host_goes_offline(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &serde_json::json!({ "saved_hosts": [{ "name": "box.tail1234.ts.net", "port": 8787 }] }),
    );
    let mut h = Harness::with(cx, &dir, &runs_fixture());
    let target = saved_remote();
    h.host(&target)
        .connect_with_features(&[protocol::FEATURE_DISPATCH_RUNS]);
    h.host(&target).send(DaemonMessage::DispatchRuns {
        runs: vec![dispatch_run("d1").worktree("C:\\r1.wt\\b").build()],
    });
    h.host(&target).sent();
    let host = h.root(|root, _| {
        root.sidebar_containers()
            .into_iter()
            .flat_map(|container| container.leaves)
            .find(|leaf| leaf.id == "d1")
            .expect("the run's leaf")
            .host
    });
    assert_eq!(host, HostKey::of(&target));
    assert!(
        !h.root(|root, _| root.host_dimmed(&host)),
        "open: not dimmed"
    );

    h.host(&target)
        .event(NetEvent::State(dropped(target.clone())));

    assert!(
        h.root(|root, _| root.host_dimmed(&host)),
        "the dropped host's run row dims"
    );
    assert!(
        h.in_model(&format!("leaf-{REMOTE}::d1")),
        "the run stays listed"
    );
}

/// Session `s2` focused in pane `p1`, and session `s1` of conversation `c1`
/// beside it in repo `r1`, running `subagents` and having dispatched `runs`;
/// the fixture's `tabs` replace the default when given.
fn nested_subagent_harness<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    subagents: Vec<serde_json::Value>,
    runs: Vec<protocol::DispatchRunSnapshot>,
) -> Harness<'a> {
    let mut fixture = Fixture::single(session("s2").in_repo("r1").build());
    fixture.repos = vec![repo("r1", "C:\\r1")];
    fixture.sessions.push(
        session("s1")
            .in_repo("r1")
            .conversation_id("c1")
            .subagents(subagents)
            .build(),
    );
    let mut h = Harness::with(cx, dir, &fixture);
    h.reconnect_with_features(&[protocol::FEATURE_DISPATCH_RUNS]);
    h.send(DaemonMessage::DispatchRuns { runs });
    h.answer_scrollback("s2", b"");
    h.sent();
    h
}

/// Run `d1` that session `s1` (conversation `c1`) dispatched in repo `r1`.
fn s1_run() -> protocol::DispatchRunSnapshot {
    dispatch_run("d1")
        .worktree("C:\\r1.wt\\a")
        .main_checkout("C:\\r1")
        .session_id("c1")
        .build()
}

/// [`s1_run`] running the subagent `r1a`.
fn s1_run_with_r1a() -> protocol::DispatchRunSnapshot {
    let mut run = s1_run();
    run.subagents = vec![serde_json::from_value(subagent("r1a")).expect("subagent fixture")];
    run
}

#[gpui::test]
fn subagents_draw_indented_under_their_host_with_the_sa_tag(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = nested_subagent_harness(cx, &dir, vec![subagent("a1"), subagent("a2")], Vec::new());

    let host = h.bounds("leaf-s1");
    for id in ["a1", "a2"] {
        assert!(
            drawn(&mut h, &format!("leaf-runtime-{id}")),
            "{id} draws its SA tag"
        );
        let leaf = h.bounds(&format!("leaf-{id}"));
        assert!(
            leaf.top() >= host.bottom(),
            "{id} draws below its host: {leaf:?} and {host:?}"
        );
        assert_eq!(
            leaf.left() - host.left(),
            px(14.0),
            "{id}'s card sits one level right of its host's"
        );
    }
    assert!(
        !drawn(&mut h, "leaf-pill-a1"),
        "a subagent leaf draws no pill"
    );
}

#[gpui::test]
fn finished_subagents_hide_when_the_setting_is_off(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    std::fs::write(
        dir.path().join("native-ui.json"),
        r#"{ "general": { "show_finished_subagents": false } }"#,
    )
    .expect("write the layout");
    let mut h = nested_subagent_harness(
        cx,
        &dir,
        vec![subagent_with("a1", "", "", "finished"), subagent("a2")],
        Vec::new(),
    );

    assert!(!h.in_model("leaf-a1"), "a finished row hides");
    assert!(!drawn(&mut h, "leaf-runtime-a1"));
    assert!(h.in_model("leaf-a2"), "a running row stays");
    assert!(drawn(&mut h, "leaf-runtime-a2"));
}

#[gpui::test]
fn finished_subagent_rows_draw_compact_in_a_comfortable_sidebar(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    std::fs::write(
        dir.path().join("native-ui.json"),
        r#"{ "general": { "finished_subagent_rows": "compact" } }"#,
    )
    .expect("write the layout");
    let mut h = nested_subagent_harness(
        cx,
        &dir,
        vec![
            subagent_with("a1", "Explore", "sonnet", "finished"),
            subagent_with("a2", "Explore", "sonnet", "running"),
        ],
        Vec::new(),
    );

    assert_eq!(
        h.bounds("leaf-a1").size.height,
        px(26.0),
        "the finished row draws on one line"
    );
    assert!(
        !drawn(&mut h, "leaf-subline-a1"),
        "its subline moved into the hover"
    );
    assert!(
        drawn(&mut h, "leaf-subline-a2"),
        "a running row keeps the density's subline"
    );
    assert!(
        h.bounds("leaf-a2").size.height > px(26.0),
        "the running row draws two lines"
    );
}

#[gpui::test]
fn the_fold_names_subagents_and_runs_and_hides_both(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = nested_subagent_harness(cx, &dir, vec![subagent("a1")], vec![s1_run()]);
    assert!(
        drawn(&mut h, "leaf-runtime-a1") && drawn(&mut h, "leaf-runtime-d1"),
        "precondition: the subagent and the run draw"
    );
    let host = h.root(|root, _| {
        root.sidebar_containers()
            .into_iter()
            .flat_map(|container| container.leaves)
            .find(|leaf| leaf.id == "s1")
            .expect("the host's leaf")
    });
    assert_eq!(
        fold_tip(host.collapsed, host.nested, host.nested_subagents),
        "Hide its subagents and dispatch runs"
    );

    // gpui drops a gone element's bounds, so the row after the nested rows
    // proves they left: s2 moves up into the places they had.
    let subagent_bottom = h.bounds("leaf-runtime-a1").bottom();
    let run_bottom = h.bounds("leaf-runtime-d1").bottom();

    h.click_on("sidebar-session-fold-s1");

    let s2 = h.bounds("leaf-s2").top();
    assert!(
        s2 < subagent_bottom,
        "the fold hides the subagent: s2 takes its place"
    );
    assert!(s2 < run_bottom, "the fold hides the run");
}

#[gpui::test]
fn clicking_a_subagent_leaf_opens_a_tab_following_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = nested_subagent_harness(cx, &dir, vec![subagent("a1")], Vec::new());

    h.click_on("leaf-a1");

    let sent = h.sent();
    assert!(
        matches!(
            sent.as_slice(),
            [ClientMessage::CreateTab {
                name: None,
                initial_session_id: None,
                host: None,
                follow: Some(PaneFollow::Subagent { session_id, agent_id }),
                request_id: Some(_),
            }] if session_id == "s1" && agent_id == "a1"
        ),
        "sent {sent:?}"
    );
}

#[gpui::test]
fn clicking_a_run_subagent_leaf_opens_a_tab_following_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = nested_subagent_harness(cx, &dir, Vec::new(), vec![s1_run_with_r1a()]);
    h.answer_scrollback("s1", b"");
    h.sent();
    assert!(
        h.in_model("leaf-r1a"),
        "precondition: the run's subagent draws under it"
    );

    h.click_on("leaf-r1a");

    let sent = h.sent();
    assert!(
        matches!(
            sent.as_slice(),
            [ClientMessage::CreateTab {
                name: None,
                initial_session_id: None,
                host: None,
                follow: Some(PaneFollow::RunSubagent { run_id, agent_id }),
                request_id: Some(_),
            }] if run_id == "d1" && agent_id == "r1a"
        ),
        "sent {sent:?}"
    );
}

#[gpui::test]
fn clicking_a_followed_run_subagent_leaf_focuses_its_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture {
        tabs: vec![tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                pane("p1", Some("s1")),
                follow_run_subagent_pane("p2", "d1", "r1a"),
            ),
        )],
        ..Fixture::single(session("s1").conversation_id("c1").build())
    };
    fixture.repos = vec![repo("r1", "C:\\r1")];
    let mut h = Harness::with(cx, &dir, &fixture);
    h.reconnect_with_features(&[protocol::FEATURE_DISPATCH_RUNS]);
    h.send(DaemonMessage::DispatchRuns {
        runs: vec![s1_run_with_r1a()],
    });
    h.answer_scrollback("s1", b"");
    h.sent();
    assert!(
        h.in_model("leaf-r1a"),
        "precondition: the run's subagent draws under it"
    );
    assert_eq!(
        focused_pane(&mut h).as_deref(),
        Some("p1"),
        "the session's pane starts focused"
    );

    h.click_on("leaf-r1a");

    let sent = h.sent();
    assert!(
        !sent
            .iter()
            .any(|msg| matches!(msg, ClientMessage::CreateTab { .. })),
        "the click opens no tab: {sent:?}"
    );
    assert_eq!(
        focused_pane(&mut h).as_deref(),
        Some("p2"),
        "the pane that follows the run's subagent takes the focus"
    );
}

#[gpui::test]
fn clicking_a_followed_subagent_leaf_focuses_its_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture {
        tabs: vec![tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                pane("p1", Some("s1")),
                follow_subagent_pane("p2", "s1", "a1"),
            ),
        )],
        ..Fixture::single(session("s1").subagents(vec![subagent("a1")]).build())
    };
    fixture.repos = vec![repo("r1", "C:\\r1")];
    let mut h = Harness::with(cx, &dir, &fixture);
    h.answer_scrollback("s1", b"");
    h.sent();
    assert_eq!(
        focused_pane(&mut h).as_deref(),
        Some("p1"),
        "the session's pane starts focused"
    );

    h.click_on("leaf-a1");

    let sent = h.sent();
    assert!(
        !sent
            .iter()
            .any(|msg| matches!(msg, ClientMessage::CreateTab { .. })),
        "the click opens no tab: {sent:?}"
    );
    assert_eq!(
        focused_pane(&mut h).as_deref(),
        Some("p2"),
        "the pane that follows the subagent takes the focus"
    );
}

#[gpui::test]
fn right_clicking_a_subagent_leaf_opens_no_menu(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = nested_subagent_harness(cx, &dir, vec![subagent("a1")], Vec::new());

    h.right_click_on("leaf-a1");

    // `in_model` counts a selector it has no model for as present, so the
    // menu's rows and its painted Open row tell whether it opened.
    assert!(
        dispatch_menu_labels(&mut h).is_empty(),
        "no dispatch menu opens for a subagent"
    );
    assert!(!drawn(&mut h, "dispatch-menu-open"), "no menu row draws");
    assert!(
        !h.in_model("session-menu"),
        "the host session's menu stays closed"
    );
}

/// The open dispatch run menu's row labels, in order; empty when closed.
fn dispatch_menu_labels(h: &mut Harness<'_>) -> Vec<String> {
    h.root(|root, _| root.dispatch_menu_labels())
}

/// A harness whose This PC daemon lists run `d1`, with the sends so far
/// drained.
fn with_run<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = Harness::with(cx, dir, &runs_fixture());
    h.reconnect_with_features(&[protocol::FEATURE_DISPATCH_RUNS]);
    h.send(runs_msg());
    h.answer_scrollback("s1", b"");
    h.sent();
    h
}

#[gpui::test]
fn right_clicking_a_run_leaf_opens_its_menu(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = with_run(cx, &dir);

    h.right_click_on("leaf-d1");

    for row in [
        "dispatch-menu-open",
        "dispatch-menu-stop",
        "dispatch-menu-copy-folder",
    ] {
        assert!(h.in_model(row), "{row} is in the menu");
    }
    assert_eq!(
        dispatch_menu_labels(&mut h),
        ["Open", "Stop run", "Copy run folder"]
    );
}

#[gpui::test]
fn stop_arms_then_sends_stop_dispatch_run_to_the_runs_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = with_run(cx, &dir);
    h.right_click_on("leaf-d1");
    h.sent();

    h.click_on("dispatch-menu-stop");
    assert!(h.sent().is_empty(), "the first click only arms");
    assert_eq!(
        dispatch_menu_labels(&mut h),
        ["Open", "Confirm: stop run…", "Copy run folder"],
        "and the row says so"
    );

    h.click_on("dispatch-menu-stop");
    let sent = h.sent();
    assert!(
        matches!(
            sent.as_slice(),
            [ClientMessage::StopDispatchRun {
                run_id,
                request_id: Some(_),
            }] if run_id == "d1"
        ),
        "sent {sent:?}"
    );
    assert!(dispatch_menu_labels(&mut h).is_empty(), "the menu closes");
}

#[gpui::test]
fn stop_goes_to_a_remote_runs_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &serde_json::json!({ "saved_hosts": [{ "name": "box.tail1234.ts.net", "port": 8787 }] }),
    );
    let mut h = Harness::with(cx, &dir, &runs_fixture());
    let target = saved_remote();
    h.host(&target)
        .connect_with_features(&[protocol::FEATURE_DISPATCH_RUNS]);
    h.host(&target).send(DaemonMessage::DispatchRuns {
        runs: vec![dispatch_run("d1").worktree("C:\\r1.wt\\b").build()],
    });
    h.answer_scrollback("s1", b"");
    h.sent();
    h.host(&target).sent();
    let leaf = format!("leaf-{REMOTE}::d1");

    h.right_click_on(&leaf);
    h.click_on("dispatch-menu-stop");
    h.click_on("dispatch-menu-stop");

    let remote_sent = h.host(&target).sent();
    assert!(
        matches!(
            remote_sent.as_slice(),
            [ClientMessage::StopDispatchRun {
                run_id,
                request_id: Some(_),
            }] if run_id == "d1"
        ),
        "the run's host gets the stop: {remote_sent:?}"
    );
    let local_sent = h.sent();
    assert!(
        !local_sent
            .iter()
            .any(|msg| matches!(msg, ClientMessage::StopDispatchRun { .. })),
        "This PC gets no stop: {local_sent:?}"
    );
}

#[gpui::test]
fn open_in_the_run_menu_opens_a_tab_following_the_run(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = with_run(cx, &dir);
    h.right_click_on("leaf-d1");
    h.sent();

    h.click_on("dispatch-menu-open");

    let sent = h.sent();
    assert!(
        matches!(
            sent.as_slice(),
            [ClientMessage::CreateTab {
                name: None,
                initial_session_id: None,
                host: None,
                follow: Some(PaneFollow::DispatchRun { run_id }),
                request_id: Some(_),
            }] if run_id == "d1"
        ),
        "sent {sent:?}"
    );
    assert!(dispatch_menu_labels(&mut h).is_empty(), "the menu closes");
}

#[gpui::test]
fn copy_run_folder_copies_the_run_dir(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = with_run(cx, &dir);
    h.right_click_on("leaf-d1");

    h.click_on("dispatch-menu-copy-folder");

    assert_eq!(h.clipboard().as_deref(), Some("C:\\runs\\d1"));
    assert!(dispatch_menu_labels(&mut h).is_empty(), "the menu closes");
}

#[gpui::test]
fn a_run_menu_closes_when_its_run_goes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = with_run(cx, &dir);
    h.right_click_on("leaf-d1");
    assert!(
        h.root(|root, _| root.dispatch_menu_open()),
        "precondition: the menu is open"
    );

    h.send(DaemonMessage::DispatchRunRemoved {
        run_id: "d1".to_owned(),
    });

    assert!(
        !h.root(|root, _| root.dispatch_menu_open()),
        "the menu closes with its run, so a later Escape is not its"
    );
}

#[gpui::test]
fn stop_is_disabled_for_an_ended_run(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &runs_fixture());
    h.reconnect_with_features(&[protocol::FEATURE_DISPATCH_RUNS]);
    let mut run = dispatch_run("d1")
        .worktree("C:\\r1.wt\\a")
        .main_checkout("C:\\r1")
        .build();
    run.status = DispatchRunStatus::Ended;
    h.send(DaemonMessage::DispatchRuns { runs: vec![run] });
    h.answer_scrollback("s1", b"");
    h.right_click_on("leaf-d1");
    h.sent();

    h.click_on("dispatch-menu-stop");
    h.click_on("dispatch-menu-stop");

    let sent = h.sent();
    assert!(
        sent.is_empty(),
        "an ended run's Stop sends nothing: {sent:?}"
    );
}
