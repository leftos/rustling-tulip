//! Sidebar specs: container order, regrouping, attention, the Ctrl+B and
//! divider behaviour, and the persisted layout.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use gpui::{Modifiers, TestAppContext, point, px};
use protocol::{
    AttentionReason, ClientMessage, DaemonMessage, SessionMode, SessionSnapshot, SpawnConfig,
    SpawnTarget, SplitDirection, VscodeWorkspaceFolder, VscodeWorkspaceSuggestion,
    WorktreeReusePolicy,
};
use rustling_tulip_native::{
    Activity, DETACHED_BANNER, RepoFilter, SidebarView, TabPill, resume_all_label,
};
use support::{Fixture, Harness, TestDir, pane, repo, session, split, tab, workspace};

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
    assert!((drag_to(&mut h, 400.0) - 400.0).abs() < 0.5);
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
        "These sessions are still running, but their repo or workspace is no longer registered. \
         Resume one from its row, or stop it from its header.",
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
fn trusted_runtime_tag_tooltip(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        sessions: vec![session("s1").trusted().build(), session("s2").build()],
        tabs: vec![tab("t1", &pane("p1", None))],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);

    assert_eq!(
        tags(&mut h, "s1"),
        [tag(
            "claude",
            "Running claude; approval prompts were bypassed"
        )]
    );
    assert_eq!(tags(&mut h, "s2"), [tag("claude", "Running claude")]);
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
