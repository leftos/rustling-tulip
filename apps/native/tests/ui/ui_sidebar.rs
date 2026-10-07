//! Sidebar specs: container order, regrouping, attention, the Ctrl+B and
//! divider behaviour, and the persisted layout.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use gpui::{Modifiers, TestAppContext, point, px};
use protocol::{AttentionReason, ClientMessage, DaemonMessage, SessionSnapshot, SplitDirection};
use rustling_tulip_native::{SidebarView, TabPill};
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
            [ClientMessage::DuplicateSession { session_id, request_id: Some(_) }] if session_id == "s1"
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
        sessions: vec![
            session("s1").trusted().agent("codex").build(),
            session("s2").build(),
        ],
        tabs: vec![tab("t1", &pane("p1", None))],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);

    assert_eq!(
        tags(&mut h, "s1"),
        [tag(
            "codex",
            "Running codex; approval prompts were bypassed"
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
