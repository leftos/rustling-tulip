//! Jump palette specs: Ctrl+Shift+P opens it from anywhere (a pop-out's
//! opens the main window's), Esc and the key again close it, typing narrows
//! its rows, and Enter jumps to the highlighted pane, tab, session or
//! sidebar row, a repo row becoming the next spawn's target.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use gpui::{Modifiers, TestAppContext, point, px};
use protocol::{DaemonMessage, SplitDirection};
use rustling_tulip_native::target::Target;
use rustling_tulip_native::{OpenOutcome, PopoutKey};
use serde_json::json;
use support::{Fixture, Harness, TestDir, pane, repo, seed_ui, session, split, tab};

/// The tailnet host the folded-host spec saves beside This PC.
const REMOTE: &str = "box.tail1234.ts.net";

/// Gives `pane_id`'s terminal the keyboard and drops what went out.
fn focus_terminal(h: &mut Harness<'_>, pane_id: &str) {
    let cell = h.cell_center(pane_id, 0, 0);
    h.click(cell, Modifiers::none());
    h.sent();
}

fn open(h: &mut Harness<'_>) -> bool {
    h.root(|root, _| root.quick_jump_open())
}

fn rows(h: &mut Harness<'_>) -> Vec<String> {
    h.root(|root, _| root.quick_jump_rows())
}

fn highlighted(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.quick_jump_highlighted())
}

fn terminal_focused(h: &mut Harness<'_>, pane_id: &str) -> bool {
    let root = h.root.clone();
    let pane_id = pane_id.to_owned();
    h.cx.update(|window, cx| root.read(cx).pane_terminal_focused(&pane_id, window, cx))
}

/// Types `text` into the focused query field.
fn type_text(h: &mut Harness<'_>, text: &str) {
    h.cx.simulate_input(text);
    h.cx.run_until_parked();
}

/// Moves the highlight down to the first row that starts with `prefix`.
fn highlight_row(h: &mut Harness<'_>, prefix: &str) {
    let at = rows(h)
        .iter()
        .position(|row| row.starts_with(prefix))
        .expect("the palette lists the row");
    for _ in 0..at {
        h.keys("down");
    }
    assert!(
        highlighted(h).is_some_and(|row| row.starts_with(prefix)),
        "the highlight is on the {prefix} row"
    );
}

/// The spawn dialog's chosen target's selector.
fn spawn_target(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| {
        root.spawn_dialog_selected()
            .into_iter()
            .find(|selector| selector.starts_with("spawn-target-"))
    })
}

/// `s1` in repo `r1`, alone in pane `p1` of tab `t1`; This PC also lists
/// repo `r2`.
fn two_repos() -> Fixture {
    let mut fixture = Fixture::single(session("s1").in_repo("r1").build());
    fixture.repos = vec![repo("r1", "C:/r1"), repo("r2", "C:/r2")];
    fixture
}

#[gpui::test]
fn ctrl_shift_p_opens_the_palette_and_the_terminal_sees_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    focus_terminal(&mut h, "p1");

    h.keys("ctrl-shift-p");

    assert!(open(&mut h), "the palette is open");
    assert!(
        h.sent_input("s1").is_empty(),
        "the terminal never saw the key"
    );
    let panel = h.bounds("quick-jump");
    assert!(panel.origin.x >= px(0.0), "the palette is drawn");
}

#[gpui::test]
fn escape_and_ctrl_shift_p_close_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    focus_terminal(&mut h, "p1");

    h.keys("ctrl-shift-p");
    assert!(open(&mut h));
    h.keys("escape");
    assert!(!open(&mut h), "Esc closes it");
    assert!(
        terminal_focused(&mut h, "p1"),
        "the pane has the keyboard back"
    );

    h.keys("ctrl-shift-p");
    assert!(open(&mut h));
    h.keys("ctrl-shift-p");
    assert!(!open(&mut h), "the key again closes it");
    assert!(
        terminal_focused(&mut h, "p1"),
        "the pane has the keyboard back"
    );
    assert!(
        h.sent_input("s1").is_empty(),
        "the terminal never saw a key"
    );
}

#[gpui::test]
fn typing_narrows_and_highlights_the_best_match(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("s1").build());
    fixture.repos = vec![repo("zeta", "C:/zeta"), repo("yaat", "C:/yaat")];
    let mut h = Harness::with(cx, &dir, &fixture);
    focus_terminal(&mut h, "p1");
    h.keys("ctrl-shift-p");

    type_text(&mut h, "ya");

    assert_eq!(rows(&mut h), ["[REPO] yaat"]);
    assert_eq!(highlighted(&mut h).as_deref(), Some("[REPO] yaat"));
}

#[gpui::test]
fn enter_on_a_tab_shows_that_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("s1").build());
    fixture.sessions.push(session("s2").build());
    fixture.tabs.push(tab("t2", &pane("p2", Some("s2"))));
    let mut h = Harness::with(cx, &dir, &fixture);
    focus_terminal(&mut h, "p1");
    h.keys("ctrl-shift-p");

    type_text(&mut h, "t2");
    assert_eq!(highlighted(&mut h).as_deref(), Some("[TAB] t2"));
    h.keys("enter");

    assert!(!open(&mut h), "Enter closes the palette");
    assert_eq!(
        h.root(|root, _| root.active_tab_id().map(str::to_owned))
            .as_deref(),
        Some("t2")
    );
}

#[gpui::test]
fn enter_on_a_pane_focuses_that_exact_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        sessions: vec![session("s1").build()],
        tabs: vec![
            tab("t1", &pane("p1", Some("s1"))),
            tab("t2", &pane("p2", Some("s1"))),
        ],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    focus_terminal(&mut h, "p1");
    h.keys("ctrl-shift-p");

    // The focused p1 leads; p2, in the never-focused t2, comes last.
    let panes: Vec<usize> = rows(&mut h)
        .iter()
        .enumerate()
        .filter(|(_, row)| row.starts_with("[PANE]"))
        .map(|(at, _)| at)
        .collect();
    assert_eq!(panes.len(), 2, "both panes are listed: {:?}", rows(&mut h));
    for _ in 0..panes[1] {
        h.keys("down");
    }
    h.keys("enter");

    assert!(!open(&mut h));
    assert_eq!(
        h.root(|root, _| root.active_tab_id().map(str::to_owned))
            .as_deref(),
        Some("t2")
    );
    assert_eq!(h.root(|root, _| root.focused_pane()).as_deref(), Some("p2"));
    assert!(
        terminal_focused(&mut h, "p2"),
        "p2's terminal has the keyboard"
    );
}

#[gpui::test]
fn enter_on_a_session_in_no_pane_places_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("s1").build());
    fixture.sessions.push(session("s2").build());
    let mut h = Harness::with(cx, &dir, &fixture);
    focus_terminal(&mut h, "p1");
    h.keys("ctrl-shift-p");

    highlight_row(&mut h, "[SESSION]");
    h.keys("enter");

    assert!(!open(&mut h));
    let sent = h.sent();
    assert!(
        sent.iter().any(|msg| format!("{msg:?}").contains("\"s2\"")),
        "s2 is placed as a sidebar click places it: {sent:?}"
    );
}

#[gpui::test]
fn enter_on_a_repo_selects_its_row_and_preselects_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    // Focus events live, so a pane taking the keyboard back would show.
    h.cx.update(|window, _| window.activate_window());
    focus_terminal(&mut h, "p1");
    h.keys("ctrl-shift-p");

    type_text(&mut h, "r2");
    assert_eq!(highlighted(&mut h).as_deref(), Some("[REPO] r2"));
    h.keys("enter");

    assert!(!open(&mut h));
    assert_eq!(
        h.root(|root, _| root.sidebar_row_focus().map(str::to_owned))
            .as_deref(),
        Some("repo:r2")
    );
    let root = h.root.clone();
    assert!(
        h.cx.update(|window, cx| root.read(cx).sidebar_focused(window)),
        "the sidebar has the keyboard"
    );

    h.keys("ctrl-shift-n");
    assert!(h.root(|root, _| root.spawn_dialog_open()));
    assert_eq!(
        spawn_target(&mut h).as_deref(),
        Some("spawn-target-repo-r2"),
        "the dialog starts on the jumped repo"
    );
}

#[gpui::test]
fn a_pane_focus_clears_the_preselected_target(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    // The window starts inactive and gets no focus events, a pane's
    // included, until it is activated.
    h.cx.update(|window, _| window.activate_window());
    focus_terminal(&mut h, "p1");
    h.keys("ctrl-shift-p");
    type_text(&mut h, "r2");
    h.keys("enter");
    assert_eq!(
        h.root(|root, _| root.sidebar_row_focus().map(str::to_owned))
            .as_deref(),
        Some("repo:r2")
    );

    focus_terminal(&mut h, "p1");
    h.keys("ctrl-shift-n");

    assert_eq!(
        spawn_target(&mut h).as_deref(),
        Some("spawn-target-repo-r1"),
        "the focused session's repo, not the jumped one"
    );
}

#[gpui::test]
fn a_folded_host_unfolds_for_a_jump(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({
            "saved_hosts": [{ "name": REMOTE, "port": 8787 }],
            "collapsed_containers": [format!("host:{REMOTE}")],
        }),
    );
    let mut h = Harness::with(
        cx,
        &dir,
        &Fixture::single(session("a1").in_repo("r1").build()),
    );
    h.send(DaemonMessage::Repos {
        repos: vec![repo("r1", "C:/r1")],
    });
    let mut host = h.host(&Target::tailnet(REMOTE, Some(8787)));
    host.connect();
    host.send(DaemonMessage::Repos {
        repos: vec![repo("r2", "C:/r2")],
    });
    h.sent();
    let remote_key = format!("{REMOTE}::repo:r2");
    let shown = |h: &mut Harness<'_>| {
        h.root(|root, _| {
            root.sidebar_containers()
                .into_iter()
                .map(|container| container.key)
                .collect::<Vec<_>>()
        })
    };
    assert!(!shown(&mut h).contains(&remote_key), "the host is folded");
    focus_terminal(&mut h, "p1");

    h.keys("ctrl-shift-p");
    type_text(&mut h, "r2");
    assert_eq!(highlighted(&mut h).as_deref(), Some("[REPO] r2"));
    h.keys("enter");

    assert!(shown(&mut h).contains(&remote_key), "the host unfolds");
    assert_eq!(
        h.root(|root, _| root.sidebar_row_focus().map(str::to_owned)),
        Some(remote_key)
    );
}

#[gpui::test]
fn the_palette_does_not_open_over_the_spawn_dialog(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    focus_terminal(&mut h, "p1");
    h.keys("ctrl-shift-n");
    assert!(h.root(|root, _| root.spawn_dialog_open()));

    h.keys("ctrl-shift-p");

    assert!(
        h.root(|root, _| root.spawn_dialog_open()),
        "the dialog stays"
    );
    assert!(!open(&mut h), "no palette over it");
}

#[gpui::test]
fn the_palette_does_not_open_over_an_open_session_menu(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    focus_terminal(&mut h, "p1");
    h.right_click_on("leaf-s1");
    assert!(h.root(|root, _| root.session_menu().is_some()));

    h.keys("ctrl-shift-p");

    assert!(!open(&mut h), "no palette over the menu");
    assert!(
        h.root(|root, _| root.session_menu().is_some()),
        "the menu stays open"
    );
}

#[gpui::test]
fn a_press_on_the_panels_padding_keeps_typing_in_the_query(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    h.cx.update(|window, _| window.activate_window());
    focus_terminal(&mut h, "p1");
    h.keys("ctrl-shift-p");
    let panel = h.bounds("quick-jump");
    assert!(panel.origin.x >= px(0.0), "the palette is drawn");

    h.click(
        point(panel.origin.x + px(2.0), panel.origin.y + px(2.0)),
        Modifiers::none(),
    );
    type_text(&mut h, "r2");

    assert!(open(&mut h), "the press keeps the palette open");
    assert_eq!(
        highlighted(&mut h).as_deref(),
        Some("[REPO] r2"),
        "the typing reached the query"
    );
}

#[gpui::test]
fn running_a_stale_pane_entry_leaves_the_keyboard_on_the_active_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("s1").build());
    fixture
        .sessions
        .push(session("s2").user_label("Bravo").build());
    fixture.tabs.push(tab("t2", &pane("p2", Some("s2"))));
    let mut h = Harness::with(cx, &dir, &fixture);
    h.cx.update(|window, _| window.activate_window());
    focus_terminal(&mut h, "p1");
    h.keys("ctrl-shift-p");
    highlight_row(&mut h, "[PANE] Bravo");

    h.send(DaemonMessage::TabRemoved {
        tab_id: "t2".to_owned(),
    });
    h.keys("enter");

    assert!(!open(&mut h));
    assert!(
        terminal_focused(&mut h, "p1"),
        "the active pane has the keyboard"
    );
}

#[gpui::test]
fn ctrl_shift_p_in_a_popout_does_not_open_over_settings(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        sessions: vec![session("s1").build(), session("s2").build()],
        tabs: vec![tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                pane("p1", Some("s1")),
                pane("p2", Some("s2")),
            ),
        )],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.sent();
    let key = PopoutKey::Pane {
        tab_id: "t1".to_owned(),
        pane_id: "p2".to_owned(),
    };
    assert_eq!(
        h.root_update(move |root, window, cx| root.open_popout(key, window, cx)),
        OpenOutcome::Opened
    );
    h.keys("ctrl-,");
    assert!(h.root(|root, _| root.settings_open()), "Settings is open");
    let mut popout = h.popout_window();

    popout.simulate_keystrokes("ctrl-shift-p");
    popout.run_until_parked();

    assert!(!open(&mut h), "no palette over Settings");
    assert!(h.root(|root, _| root.settings_open()), "Settings stays");
}

#[gpui::test]
fn no_match_shows_the_empty_row_and_enter_keeps_the_palette_open(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    focus_terminal(&mut h, "p1");
    h.keys("ctrl-shift-p");

    type_text(&mut h, "zzzz");

    assert_eq!(rows(&mut h), Vec::<String>::new());
    assert_eq!(highlighted(&mut h), None);
    let empty = h.bounds("quick-jump-no-match");
    assert!(empty.origin.x >= px(0.0), "the no-match row is drawn");
    h.keys("enter");
    assert!(open(&mut h), "Enter on no match keeps it open");
    assert!(h.sent().is_empty(), "and jumps nowhere");
}

#[gpui::test]
fn ctrl_shift_p_in_a_popout_opens_the_palette_in_the_main_window(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        sessions: vec![session("s1").build(), session("s2").build()],
        tabs: vec![tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                pane("p1", Some("s1")),
                pane("p2", Some("s2")),
            ),
        )],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.sent();
    let key = PopoutKey::Pane {
        tab_id: "t1".to_owned(),
        pane_id: "p2".to_owned(),
    };
    assert_eq!(
        h.root_update(move |root, window, cx| root.open_popout(key, window, cx)),
        OpenOutcome::Opened
    );
    let mut popout = h.popout_window();

    popout.simulate_keystrokes("ctrl-shift-p");
    popout.run_until_parked();

    assert!(open(&mut h), "the palette is open");
    let panel = h.bounds("quick-jump");
    assert!(panel.origin.x >= px(0.0), "drawn in the main window");
    assert!(
        h.sent_input("s2").is_empty(),
        "the popped terminal never saw the key"
    );
}
