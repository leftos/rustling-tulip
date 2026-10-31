//! Cross-cutting key and focus specs: menus stay inside the viewport, the
//! branch list flips above a short dialog, Up moves its highlight, Esc closes
//! menus and modals and hands the keyboard back to the pane, Settings closes
//! from its fields, and the deliberate Esc exceptions stay as designed.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use gpui::{TestAppContext, point, px};
use protocol::{ClientMessage, DaemonMessage, RecoverItemResult, SplitDirection};
use rustling_tulip_native::RootView;
use support::{Fixture, Harness, TestDir, history_item, pane, repo, session, split, tab};

const PROMPT: &str = "\x1b]133;A\x07";
const OUTPUT: &str = "\x1b]133;C\x07";

/// `s1` of `r1` alone in pane `p1` of tab `t1`, with repo `r1` to spawn into.
fn fixture() -> Fixture {
    let mut fixture = Fixture::single(session("s1").in_repo("r1").build());
    fixture.repos = vec![repo("r1", "C:/r1")];
    fixture
}

/// Tab `t1`: pane `p1` showing `s1` beside the empty pane `p2`.
fn with_empty() -> Fixture {
    Fixture {
        sessions: vec![session("s1").in_repo("r1").build()],
        tabs: vec![tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                pane("p1", Some("s1")),
                pane("p2", None),
            ),
        )],
        repos: vec![repo("r1", "C:/r1")],
        ..Fixture::default()
    }
}

/// The harness on `fixture`, every scrollback answered, what it sent drained.
fn attached<'a>(cx: &'a mut TestAppContext, dir: &TestDir, fixture: &Fixture) -> Harness<'a> {
    let mut h = Harness::with(cx, dir, fixture);
    let ids: Vec<String> = fixture
        .sessions
        .iter()
        .map(|session| session.id.clone())
        .collect();
    for id in ids {
        h.answer_scrollback(&id, b"");
    }
    h.sent();
    h
}

/// Which part of the Settings frame has the keyboard.
fn settings_focus(h: &mut Harness<'_>) -> Option<&'static str> {
    let root = h.root.clone();
    h.cx.update(|window, cx| root.read(cx).settings_focus(window, cx))
}

/// Asserts the element tagged `selector` lies inside the window.
fn assert_inside(h: &mut Harness<'_>, selector: &str) {
    let bounds = h.bounds(selector);
    let (width, height) = (h.window_width(), h.window_height());
    assert!(
        bounds.top() >= px(0.0),
        "{selector} starts above the window: {bounds:?}"
    );
    assert!(
        bounds.bottom() <= px(height),
        "{selector} runs past the window's {height}px: {bounds:?}"
    );
    assert!(
        bounds.left() >= px(0.0),
        "{selector} starts left of the window: {bounds:?}"
    );
    assert!(
        bounds.right() <= px(width),
        "{selector} runs past the window's {width}px: {bounds:?}"
    );
}

/// The empty-pane menu is far shorter than the cap, so this proves
/// `snap_to_window()` placement, not the height cap;
/// [`accent_submenu_fits_a_short_window`] proves the cap.
#[gpui::test]
fn an_empty_pane_menu_opened_low_is_snapped_inside_the_viewport(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, &with_empty());
    h.cx.simulate_resize(gpui::size(px(900.0), px(520.0)));

    let grid = h.bounds("tab-grid-t1");
    let at = point(grid.right() - px(24.0), grid.bottom() - px(24.0));
    h.right_click(at);
    assert!(h.in_model("empty-pane-menu"), "the click opened the menu");
    assert_inside(&mut h, "empty-pane-menu");
    let menu = h.bounds("empty-pane-menu");
    let height = h.window_height();
    assert!(
        menu.size.height <= px(height * 0.6) + px(0.5),
        "a short menu is under the cap too: {menu:?} in {height}px"
    );
}

#[gpui::test]
fn accent_submenu_fits_a_short_window(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let recents: Vec<String> = (1..=12)
        .map(|i| format!("#{i:02x}{i:02x}{i:02x}"))
        .collect();
    std::fs::write(
        dir.path().join("native-ui.json"),
        serde_json::json!({ "recent_colors": recents }).to_string(),
    )
    .expect("write the saved recents");
    let mut h = attached(cx, &dir, &fixture());
    h.cx.simulate_resize(gpui::size(px(1000.0), px(360.0)));

    h.right_click_on("leaf-s1");
    assert!(h.in_model("session-menu"), "the menu opened");
    h.click_on("session-menu-accent");
    assert!(h.in_model("accent-inherit"), "the submenu is open");
    assert!(
        h.in_model("accent-recent-010101"),
        "the seeded recents are listed"
    );

    let menu = h.bounds("session-menu");
    let height = h.window_height();
    assert_inside(&mut h, "session-menu");
    assert!(
        menu.size.height <= px(height * 0.6) + px(0.5),
        "the menu is capped at 60% of the {height}px window: {menu:?}"
    );
}

#[gpui::test]
fn branch_list_flips_above_and_stays_inside_the_dialog(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, &with_empty());
    h.cx.simulate_resize(gpui::size(px(1000.0), px(320.0)));

    h.click_on("sidebar-add-session");
    assert!(
        h.root(|root, _| root.spawn_dialog_open()),
        "the dialog opened"
    );
    h.send(DaemonMessage::Branches {
        repo_id: "r1".to_owned(),
        branches: ["main", "wt/red-fox", "feature/Login"]
            .map(str::to_owned)
            .to_vec(),
        current: Some("main".to_owned()),
        remote_branches: vec!["origin/main".to_owned()],
    });
    h.sent();

    h.click_on("spawn-branch");
    h.keys("m");
    let list = h.bounds("spawn-branch-list");
    let field = h.bounds("spawn-branch");
    let panel = h.bounds("spawn-panel");
    assert!(list.size.height > px(0.0), "the list is drawn: {list:?}");
    assert!(
        list.bottom() <= field.top(),
        "the list flips above the field: {list:?} over {field:?}"
    );
    assert!(
        list.top() >= panel.top() && list.bottom() <= panel.bottom(),
        "the list lies inside the panel vertically: {list:?} in {panel:?}"
    );
    assert!(
        list.left() >= panel.left() && list.right() <= panel.right(),
        "the list lies inside the panel horizontally: {list:?} in {panel:?}"
    );
}

#[gpui::test]
fn up_moves_the_branch_highlight(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, &fixture());
    h.click_on("sidebar-add-session");
    assert!(h.root(|root, _| root.spawn_dialog_open()));
    h.send(DaemonMessage::Branches {
        repo_id: "r1".to_owned(),
        branches: ["main", "wt/red-fox", "feature/Login"]
            .map(str::to_owned)
            .to_vec(),
        current: Some("main".to_owned()),
        remote_branches: vec![],
    });
    h.sent();

    h.keys("down down up enter");
    assert_eq!(
        h.root(RootView::spawn_dialog_branch).as_deref(),
        Some("main"),
        "Up steps back from wt/red-fox to main"
    );
    assert_eq!(
        h.root(|root, _| root.spawn_branch_rows()),
        None,
        "Enter closed the list"
    );
}

#[gpui::test]
fn escape_closes_a_menu_and_returns_focus_to_the_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, &fixture());
    h.right_click_on("leaf-s1");
    assert!(h.in_model("session-menu"));
    h.keys("escape");
    assert!(!h.in_model("session-menu"), "Esc closes the session menu");
    h.keys("a");
    assert_eq!(h.sent_input("s1"), b"a", "the session menu's Esc");

    let dir = TestDir::new();
    let mut h = attached(cx, &dir, &with_empty());
    h.right_click_on("empty-pane-shell-p2");
    assert!(h.in_model("empty-pane-menu"));
    h.keys("escape");
    assert!(!h.in_model("empty-pane-menu"), "Esc closes the pane menu");
    h.keys("a");
    assert_eq!(h.sent_input("s1"), b"a", "the empty pane menu's Esc");

    let dir = TestDir::new();
    let mut h = attached(cx, &dir, &fixture());
    h.right_click_on("tab-t1");
    assert!(h.in_model("tab-menu"));
    h.keys("escape");
    assert!(!h.in_model("tab-menu"), "Esc closes the tab menu");
    h.keys("a");
    assert_eq!(h.sent_input("s1"), b"a", "the tab menu's Esc");

    let dir = TestDir::new();
    let mut h = attached(
        cx,
        &dir,
        &Fixture::single(session("s1").shell("C:/work").build()),
    );
    h.pty(
        "s1",
        format!("{PROMPT}$ make\r\n{OUTPUT}out\r\n\x1b]133;D;0\x07").as_bytes(),
    );
    h.click_on("shell-dot-p1-0");
    assert!(h.in_model("shell-menu"));
    h.keys("escape");
    assert!(!h.in_model("shell-menu"), "Esc closes the shell menu");
    h.keys("a");
    assert_eq!(h.sent_input("s1"), b"a", "the shell menu's Esc");

    let dir = TestDir::new();
    let mut h = attached(cx, &dir, &fixture());
    h.click_on("sidebar-more");
    assert!(h.root(|root, _| root.sidebar_more_open()));
    h.keys("escape");
    assert!(!h.root(|root, _| root.sidebar_more_open()), "Esc closes ⋯");
    h.keys("a");
    assert_eq!(h.sent_input("s1"), b"a", "the ⋯ menu's Esc");
}

#[gpui::test]
fn settings_escape_closes_from_its_fields(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, &fixture());

    h.keys("ctrl-,");
    assert!(h.root(|root, _| root.settings_open()));
    h.click_on("settings-tab-worktrees");
    h.click_on("settings-worktrees-root-input");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-worktrees-root-input"),
        "the path field has the keyboard"
    );
    h.keys("escape");
    assert!(
        !h.root(|root, _| root.settings_open()),
        "Esc closes Settings from the path field"
    );
    h.keys("a");
    assert_eq!(h.sent_input("s1"), b"a", "the pane has the keyboard");

    h.keys("ctrl-,");
    h.click_on("settings-tab-appearance");
    h.click_on("appearance-ui-family-filter");
    assert_eq!(
        settings_focus(&mut h),
        Some("appearance-ui-family-filter"),
        "the font filter has the keyboard"
    );
    h.keys("escape");
    assert!(
        !h.root(|root, _| root.settings_open()),
        "Esc closes Settings from the font filter"
    );
    h.keys("a");
    assert_eq!(h.sent_input("s1"), b"a", "the pane has the keyboard again");
}

#[gpui::test]
fn modal_escape_returns_focus_to_the_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, &with_empty());
    h.click_on("close-pane-p1");
    h.keys("escape");
    assert!(!h.in_model("pane-close"), "Esc closes the pane dialog");
    h.keys("a");
    assert_eq!(h.sent_input("s1"), b"a", "after the pane dialog's Esc");

    let dir = TestDir::new();
    let mut h = attached(cx, &dir, &fixture());
    h.click_on("sidebar-add-session");
    assert!(h.root(|root, _| root.spawn_dialog_open()));
    h.keys("escape");
    assert!(!h.root(|root, _| root.spawn_dialog_open()));
    h.keys("a");
    assert_eq!(h.sent_input("s1"), b"a", "after the spawn dialog's Esc");

    let dir = TestDir::new();
    let mut h = attached(cx, &dir, &fixture());
    h.open_shell_dialog_from_more();
    assert!(h.root(|root, _| root.shell_dialog_open()));
    h.keys("escape");
    assert!(!h.root(|root, _| root.shell_dialog_open()));
    h.keys("a");
    assert_eq!(h.sent_input("s1"), b"a", "after the Shell… dialog's Esc");

    let dir = TestDir::new();
    let mut h = attached(cx, &dir, &fixture());
    h.send(DaemonMessage::SessionHistory {
        request_id: None,
        items: vec![],
    });
    h.click_on("activity-recover");
    assert!(h.root(|root, _| root.recover_open()));
    h.keys("escape");
    assert!(!h.root(|root, _| root.recover_open()));
    h.keys("a");
    assert_eq!(h.sent_input("s1"), b"a", "after the Recover dialog's Esc");
}

#[gpui::test]
fn escape_is_ignored_where_designed(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, &fixture());
    h.send(DaemonMessage::LayoutInitRequired {
        has_legacy: false,
        active_session_count: 1,
        clonable: vec![],
    });
    assert!(h.root(|root, _| root.layout_chooser_open()));
    h.keys("escape");
    assert!(
        h.root(|root, _| root.layout_chooser_open()),
        "Esc never dismisses the layout chooser"
    );

    let dir = TestDir::new();
    let mut h = attached(cx, &dir, &fixture());
    h.send(DaemonMessage::SessionHistory {
        request_id: None,
        items: vec![history_item("a").ended_minutes_ago(10).build()],
    });
    h.click_on("activity-recover");
    assert!(h.root(|root, _| root.recover_open()));
    h.keys("enter");
    let request_id = h
        .sent()
        .into_iter()
        .find_map(|msg| match msg {
            ClientMessage::RecoverSessions { request_id, .. } => request_id,
            _ => None,
        })
        .expect("a recover request");
    h.keys("escape");
    assert!(
        h.root(|root, _| root.recover_open()),
        "Esc waits for the recover answer"
    );
    h.send(DaemonMessage::RecoverResult {
        request_id: Some(request_id),
        results: vec![RecoverItemResult {
            history_id: "a".to_owned(),
            session_id: Some("new-a".to_owned()),
            error: None,
        }],
    });
    assert!(!h.root(|root, _| root.recover_open()));

    let dir = TestDir::new();
    let mut h = attached(cx, &dir, &fixture());
    assert!(!h.cx.simulate_close(), "the close asks first");
    h.cx.run_until_parked();
    h.click_on("exit-stop-keep-worktrees");
    h.keys("escape");
    assert!(
        h.root(|root, _| root.exit_dialog_open()),
        "no Esc while the shutdown is on the way"
    );

    let dir = TestDir::new();
    let mut h = attached(cx, &dir, &fixture());
    h.right_click_on("leaf-s1");
    h.click_on("menu-rename");
    h.keys("escape");
    assert_eq!(
        h.root(|root, _| root.session_menu().map(str::to_owned))
            .as_deref(),
        Some("s1"),
        "the rename's Esc leaves the menu open"
    );
    assert!(
        h.root(|root, _| root.menu_rows())
            .contains(&"menu-rename".to_owned()),
        "and returns to the menu's rows"
    );
}
