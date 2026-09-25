//! Sidebar specs: container order, regrouping, attention, the Ctrl+B and
//! divider behaviour, and the persisted layout.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

#[expect(dead_code, reason = "each spec file uses its own share of the helper")]
mod support;

use gpui::{Modifiers, TestAppContext, point, px};
use protocol::{AttentionReason, DaemonMessage};
use support::{Fixture, Harness, TestDir, pane, repo, session, tab, workspace};

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
    assert_eq!(tags, ["WS", "REPO", "SH", "DIR", "Detached"]);
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
    assert!(h.sent_input("s1").is_empty());

    h.click_on("sidebar-show");
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
    h.click_on("sidebar-show");
    assert_eq!(h.bounds("sidebar-panel").size.width, px(400.0));
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
