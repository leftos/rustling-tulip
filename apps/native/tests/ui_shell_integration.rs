//! Shell integration specs: a plain shell's finished commands get a dot in
//! a gutter left of the grid, coloured by exit code, with the exit and the
//! duration as its tooltip, anchored to the command's prompt row.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

#[expect(dead_code, reason = "each spec file uses its own share of the helper")]
mod support;

use std::time::Duration;

use gpui::{Modifiers, Pixels, Point, ScrollDelta, ScrollWheelEvent, TestAppContext, point, px};
use protocol::{ClientMessage, DaemonMessage, SessionSnapshot, SplitDirection};
use rustling_tulip_native::fonts::FontSettings;
use rustling_tulip_native::{RootView, ShellDot, ShellStatus};
use support::{Fixture, Harness, TestDir, pane, repo, session, split, tab};

const PROMPT: &str = "\x1b]133;A\x07";
const TYPED: &str = "\x1b]133;B\x07";
const OUTPUT: &str = "\x1b]133;C\x07";

fn shell() -> SessionSnapshot {
    session("s1").shell("C:/work").build()
}

/// `s` alone in pane `p1`, its scrollback answered with `history`, the pane
/// focused and everything sent so far drained.
fn attached<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    s: SessionSnapshot,
    history: &[u8],
) -> Harness<'a> {
    let id = s.id.clone();
    let mut h = Harness::with(cx, dir, &Fixture::single(s));
    h.answer_scrollback(&id, history);
    let at = h.cell_center("p1", 0, 0);
    h.click(at, Modifiers::none());
    h.sent();
    h
}

/// A whole command: prompt, command line, output and its end with `exit`.
fn command(line: &str, exit: &str) -> String {
    format!("{PROMPT}$ {line}\r\n{OUTPUT}out\r\n\x1b]133;D{exit}\x07")
}

fn dots(h: &mut Harness<'_>, pane: &str) -> Vec<ShellDot> {
    h.root(|root, cx| root.pane_shell_records(pane, cx))
}

fn rows(h: &mut Harness<'_>, pane: &str) -> Vec<usize> {
    dots(h, pane).iter().map(|dot| dot.row).collect()
}

/// Scrolls the wheel over `at` by `lines` (positive is back through the
/// history).
fn wheel(h: &mut Harness<'_>, at: Point<Pixels>, lines: f32) {
    h.cx.simulate_event(ScrollWheelEvent {
        position: at,
        delta: ScrollDelta::Lines(point(0.0, lines)),
        ..ScrollWheelEvent::default()
    });
    h.cx.run_until_parked();
}

#[gpui::test]
fn a_finished_command_gets_a_dot_with_its_exit_and_duration(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, shell(), b"");
    h.pty("s1", format!("{PROMPT}$ make\r\n{OUTPUT}").as_bytes());
    assert_eq!(dots(&mut h, "p1"), [], "no dot while the command runs");
    h.advance(Duration::from_millis(1500));
    h.pty("s1", b"built\r\n\x1b]133;D;0\x07");
    assert_eq!(
        dots(&mut h, "p1"),
        [ShellDot {
            row: 0,
            status: ShellStatus::Ok,
            exit: Some(0),
            tooltip: "exit 0 · 1.50s".to_owned(),
        }]
    );
    assert!(h.in_model("shell-dot-p1-0"));
    assert!(!h.in_model("shell-dot-p1-1"));
    let dot = h.center("shell-dot-p1-0");
    let text = h.cell_center("p1", 0, 0);
    assert!(
        dot.x < text.x,
        "the dot sits in the gutter, left of the text"
    );
    assert!(
        (dot.y - text.y).abs() < px(1.0),
        "the dot is centred on the prompt row: {dot:?} vs {text:?}"
    );
}

#[gpui::test]
fn failed_command_dot_is_red(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, shell(), b"");
    h.pty("s1", command("false", ";1").as_bytes());
    h.pty("s1", command("true", "").as_bytes());
    let found: Vec<(usize, ShellStatus, Option<i32>)> = dots(&mut h, "p1")
        .into_iter()
        .map(|dot| (dot.row, dot.status, dot.exit))
        .collect();
    assert_eq!(
        found,
        [
            (0, ShellStatus::Fail, Some(1)),
            (2, ShellStatus::Unknown, None),
        ]
    );
    assert!(dots(&mut h, "p1")[1].tooltip.starts_with("exit ? · "));
}

#[gpui::test]
fn dot_follows_its_line_as_output_scrolls(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, shell(), b"");
    let screen = h.grid_text("p1").len();
    h.pty("s1", "\r\n".repeat(3).as_bytes());
    h.pty("s1", command("ls", ";0").as_bytes());
    assert_eq!(rows(&mut h, "p1"), [3]);
    // The cursor is on row 5: this output scrolls the screen by two.
    h.pty("s1", "x\r\n".repeat(screen - 6 + 2).as_bytes());
    assert_eq!(rows(&mut h, "p1"), [1]);
    assert_eq!(
        h.grid_text("p1")[1],
        "$ ls",
        "the dot is on its prompt's row"
    );

    h.pty("s1", "x\r\n".repeat(2).as_bytes());
    assert!(rows(&mut h, "p1").is_empty(), "scrolled off the screen");
    let at = h.cell_center("p1", 1, 1);
    wheel(&mut h, at, 2.0);
    assert_eq!(rows(&mut h, "p1"), [1], "back in view in the history");
}

#[gpui::test]
fn dot_follows_its_prompt_when_the_font_size_changes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, shell(), b"");
    let screen = h.grid_text("p1").len();
    h.pty("s1", "x\r\n".repeat(screen).as_bytes());
    h.pty(
        "s1",
        format!("{}{PROMPT}$ ", command("ls", ";0")).as_bytes(),
    );
    let before = rows(&mut h, "p1");
    assert_eq!(before, [screen - 3], "the prompt sits above the output");

    let root = h.root.clone();
    h.cx.update(|_, cx| {
        root.update(cx, |root, cx| {
            root.set_app_font(
                FontSettings {
                    size: 24.0,
                    ..FontSettings::default()
                },
                cx,
            );
        });
    });
    h.cx.run_until_parked();

    let after = rows(&mut h, "p1");
    assert_eq!(after.len(), 1);
    assert!(after[0] < before[0], "fewer rows, so the prompt row rose");
    let dot = h.center("shell-dot-p1-0");
    let text = h.cell_center("p1", 0, after[0]);
    assert!(
        (dot.y - text.y).abs() < px(1.0),
        "the dot moved with its prompt row: {dot:?} vs {text:?}"
    );
}

#[gpui::test]
fn dot_disappears_when_its_prompt_leaves_scrollback(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, shell(), b"");
    let screen = h.grid_text("p1").len();
    h.pty("s1", command("ls", ";0").as_bytes());
    // From row 2 to the bottom row, then 5000 rows into the history: the
    // prompt is the history's oldest line.
    h.pty("s1", "x\r\n".repeat(screen - 3 + 5000).as_bytes());
    let at = h.cell_center("p1", 1, 1);
    wheel(&mut h, at, 6000.0);
    assert_eq!(rows(&mut h, "p1"), [0], "the oldest line keeps its dot");
    assert_eq!(h.grid_text("p1")[0], "$ ls");

    h.pty("s1", b"x\r\n");
    assert_eq!(h.grid_text("p1")[0], "out", "the prompt left the history");
    assert!(rows(&mut h, "p1").is_empty(), "and its dot with it");
}

#[gpui::test]
fn no_gutter_for_agent_sessions(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let grid = split(
        SplitDirection::Horizontal,
        pane("p1", Some("s1")),
        pane("p2", Some("s2")),
    );
    let fixture = Fixture {
        sessions: vec![shell(), session("s2").build()],
        tabs: vec![tab("t1", &grid)],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.answer_scrollback("s1", b"");
    h.answer_scrollback("s2", b"");
    h.pty("s2", command("ls", ";0").as_bytes());
    assert_eq!(dots(&mut h, "p2"), [], "an agent's marks make no dots");
    assert!(!h.in_model("shell-dot-p2-0"));

    let sent = h.sent();
    let cols = |id: &str| {
        sent.iter()
            .rev()
            .find_map(|m| match m {
                ClientMessage::Resize {
                    session_id, cols, ..
                } if session_id == id => Some(*cols),
                _ => None,
            })
            .expect("the pane sized its session")
    };
    let (shell_cols, agent_cols) = (cols("s1"), cols("s2"));
    assert!(
        shell_cols < agent_cols,
        "the shell's gutter takes columns: {shell_cols} vs {agent_cols}"
    );
    let shell_x = h.cell_center("p1", 0, 0).x - h.bounds("pane-grid-p1").origin.x;
    let agent_x = h.cell_center("p2", 0, 0).x - h.bounds("pane-grid-p2").origin.x;
    let gutter = shell_x - agent_x;
    assert!(
        (gutter - px(14.0)).abs() < px(0.01),
        "the gutter is 14px wide: {gutter:?}"
    );
}

#[gpui::test]
fn gutter_click_starts_no_selection(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, shell(), b"");
    h.pty("s1", command("echo hello world", ";0").as_bytes());
    let dot = h.center("shell-dot-p1-0");
    let text = h.cell_center("p1", 8, 0);
    h.drag(dot, text, [Modifiers::none(); 2]);
    assert_eq!(h.clipboard(), None, "a drag from the dot selects nothing");
    let blank_gutter = point(dot.x, h.cell_center("p1", 0, 3).y);
    h.drag(blank_gutter, text, [Modifiers::none(); 2]);
    assert_eq!(h.clipboard(), None, "nor does one from the empty gutter");

    // The program asked for the mouse: a click on the grid reaches it, a
    // click on the dot (which only opens its menu) not. The grid click
    // comes first, before the menu's own layer is up.
    h.pty("s1", b"\x1b[?1000h");
    let cell = h.cell_center("p1", 2, 2);
    h.click(cell, Modifiers::none());
    assert!(
        !h.sent_input("s1").is_empty(),
        "a click on the grid still is"
    );
    h.click(dot, Modifiers::none());
    assert!(h.sent_input("s1").is_empty(), "nothing reported");
}

#[gpui::test]
fn replayed_history_dots_have_no_duration(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let history = format!("{}{}", command("ls", ";0"), command("bad", ";2"));
    let mut h = attached(cx, &dir, shell(), history.as_bytes());
    let found: Vec<(usize, String)> = dots(&mut h, "p1")
        .into_iter()
        .map(|dot| (dot.row, dot.tooltip))
        .collect();
    assert_eq!(
        found,
        [(0, "exit 0".to_owned()), (2, "exit 2".to_owned())],
        "a replayed command shows its exit only"
    );
    h.pty("s1", format!("{PROMPT}$ ok\r\n{OUTPUT}").as_bytes());
    h.advance(Duration::from_millis(20));
    h.pty("s1", b"\x1b]133;D;0\x07");
    assert_eq!(
        dots(&mut h, "p1")[2].tooltip,
        "exit 0 · 20ms",
        "a live one its duration"
    );
}

/// One command in the order bash, zsh and pwsh write it: the prompt, the
/// typed `line`, the Enter's newline, the output mark at the start of the
/// next row, `out`, the end at the start of the row after it, and the next
/// prompt there.
fn ran(line: &str, out: &str, exit: &str) -> String {
    format!("{PROMPT}$ {TYPED}{line}\r\n{OUTPUT}{out}\r\n\x1b]133;D{exit}\x07{PROMPT}$ ")
}

/// `s` alone in pane `p1` with one finished command in it, its dot on row
/// zero.
fn with_command<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    line: &str,
    out: &str,
) -> Harness<'a> {
    let mut h = attached(cx, dir, shell(), b"");
    h.pty("s1", ran(line, out, ";0").as_bytes());
    h
}

#[gpui::test]
fn clicking_a_dot_opens_its_menu_with_the_exit_header(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, shell(), b"");
    h.pty(
        "s1",
        format!("{PROMPT}$ {TYPED}make\r\n{OUTPUT}").as_bytes(),
    );
    h.advance(Duration::from_millis(1500));
    h.pty(
        "s1",
        format!("built\r\n\x1b]133;D;0\x07{PROMPT}$ ").as_bytes(),
    );
    h.click_on("shell-dot-p1-0");
    assert!(h.in_model("shell-menu"), "the dot's menu opened");
    for row in [
        "shell-menu-copy-command",
        "shell-menu-copy-output",
        "shell-menu-copy-both",
        "shell-menu-rerun",
    ] {
        assert!(h.in_model(row), "{row} is in the menu");
    }
    assert_eq!(
        h.root(|root, _| root.shell_menu_header()),
        Some("exit 0 · 1.50s".to_owned())
    );
    let dot = h.bounds("shell-dot-p1-0");
    let menu = h.bounds("shell-menu");
    assert!(
        (menu.origin.x - (dot.origin.x + dot.size.width) - px(4.0)).abs() < px(0.5),
        "the menu opens 4px right of the dot: {menu:?} vs {dot:?}"
    );
    assert!(
        (menu.origin.y - dot.origin.y).abs() < px(1.0),
        "and level with its top: {menu:?} vs {dot:?}"
    );
}

#[gpui::test]
fn copy_command_copies_the_633e_text_and_shows_the_chip(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, shell(), b"");
    // The shell's own line differs from the row it drew, and wins.
    h.pty(
        "s1",
        format!(
            "{PROMPT}$ {TYPED}ls\r\n\x1b]633;E;ls --color=auto\x07{OUTPUT}out\r\n\x1b]133;D;0\x07{PROMPT}$ "
        )
        .as_bytes(),
    );
    h.click_on("shell-dot-p1-0");
    h.click_on("shell-menu-copy-command");
    assert_eq!(h.clipboard().as_deref(), Some("ls --color=auto"));
    assert_eq!(h.root(|root, _| root.copied_chip()), Some(15));
    assert!(!h.in_model("shell-menu"), "the copy closed the menu");
}

#[gpui::test]
fn copy_output_copies_the_rows_between_c_and_d(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = with_command(cx, &dir, "ls", "one\r\ntwo");
    h.click_on("shell-dot-p1-0");
    h.click_on("shell-menu-copy-output");
    assert_eq!(h.clipboard().as_deref(), Some("one\ntwo"));
}

#[gpui::test]
fn copy_both_joins_with_a_newline(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = with_command(cx, &dir, "ls -la", "one");
    h.click_on("shell-dot-p1-0");
    h.click_on("shell-menu-copy-both");
    assert_eq!(h.clipboard().as_deref(), Some("ls -la\none"));
}

#[gpui::test]
fn rerun_types_the_command_without_enter(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = with_command(cx, &dir, "ls -la", "one");
    h.click_on("shell-dot-p1-0");
    h.click_on("shell-menu-rerun");
    assert_eq!(
        h.sent_input("s1"),
        b"ls -la".to_vec(),
        "the command's bytes, with no newline"
    );
    assert_eq!(h.root(|root, _| root.focused_pane()), Some("p1".to_owned()));
}

#[gpui::test]
fn rerun_is_disabled_for_a_stopped_session(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = with_command(cx, &dir, "ls", "one");
    h.click_on("shell-dot-p1-0");
    assert!(h.root(RootView::shell_menu_can_rerun));
    h.send(DaemonMessage::SessionUpdated {
        session: session("s1").shell("C:/work").exited(0).build(),
        request_id: None,
    });
    assert!(
        !h.root(RootView::shell_menu_can_rerun),
        "a stopped session's command cannot be re-run"
    );
    h.click_on("shell-menu-rerun");
    assert!(h.sent_input("s1").is_empty(), "nothing was typed");
}

#[gpui::test]
fn escape_and_outside_click_close_the_menu(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = with_command(cx, &dir, "ls", "one");
    h.click_on("shell-dot-p1-0");
    assert!(h.in_model("shell-menu"));
    h.keys("escape");
    assert!(!h.in_model("shell-menu"), "Esc closes the menu");

    h.click_on("shell-dot-p1-0");
    assert!(h.in_model("shell-menu"));
    let grid = h.bounds("pane-grid-p1");
    let far = point(
        grid.origin.x + grid.size.width - px(10.0),
        grid.origin.y + grid.size.height - px(10.0),
    );
    assert!(
        !h.bounds("shell-menu").contains(&far),
        "the click is over the pane, not the menu"
    );
    h.click(far, Modifiers::none());
    assert!(!h.in_model("shell-menu"), "and a click outside it");
    assert_eq!(h.clipboard(), None, "which acted on no menu row");
}

#[gpui::test]
fn output_does_not_close_the_menu(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = with_command(cx, &dir, "ls", "one");
    h.click_on("shell-dot-p1-0");
    assert!(h.in_model("shell-menu"));
    h.pty("s1", b"more output\r\n");
    assert!(
        h.in_model("shell-menu"),
        "output leaves it up; only a scroll dismisses it"
    );
}

#[gpui::test]
fn a_wheel_over_the_pane_closes_the_menu(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = with_command(cx, &dir, "ls", "one");
    h.click_on("shell-dot-p1-0");
    assert!(h.in_model("shell-menu"));
    let grid = h.bounds("pane-grid-p1");
    let far = point(
        grid.origin.x + grid.size.width - px(10.0),
        grid.origin.y + grid.size.height - px(10.0),
    );
    assert!(
        !h.bounds("shell-menu").contains(&far),
        "the wheel is over the pane, not the menu"
    );
    wheel(&mut h, far, 1.0);
    assert!(!h.in_model("shell-menu"), "the wheel closed the menu");
}

#[gpui::test]
fn rerun_from_a_pane_without_focus_focuses_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let grid = split(
        SplitDirection::Horizontal,
        pane("p1", Some("s1")),
        pane("p2", Some("s2")),
    );
    let fixture = Fixture {
        sessions: vec![shell(), session("s2").shell("C:/work").build()],
        tabs: vec![tab("t1", &grid)],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.answer_scrollback("s1", b"");
    h.answer_scrollback("s2", b"");
    let at = h.cell_center("p1", 0, 0);
    h.click(at, Modifiers::none());
    h.sent();
    assert_eq!(h.root(|root, _| root.focused_pane()), Some("p1".to_owned()));
    h.pty("s2", ran("ls -la", "one", ";0").as_bytes());
    h.click_on("shell-dot-p2-0");
    h.click_on("shell-menu-rerun");
    assert_eq!(h.sent_input("s2"), b"ls -la".to_vec());
    assert!(h.sent_input("s1").is_empty(), "the other pane got nothing");
    assert_eq!(
        h.root(|root, _| root.focused_pane()),
        Some("p2".to_owned()),
        "the command's pane is its tab's focused one"
    );
}

/// `s` alone in `p1` with one finished two-line command, as the shell's
/// own `OSC 633;E` line, and its dot's menu open.
fn with_two_line_command<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = attached(cx, dir, shell(), b"");
    h.pty(
        "s1",
        format!(
            "{PROMPT}$ {TYPED}echo a\r\n> echo b\r\n\x1b]633;E;echo a\\x0aecho b\x07{OUTPUT}a\r\nb\r\n\x1b]133;D;0\x07{PROMPT}$ "
        )
        .as_bytes(),
    );
    h
}

#[gpui::test]
fn rerun_sends_a_multi_line_command_as_a_bracketed_paste(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = with_two_line_command(cx, &dir);
    h.pty("s1", b"\x1b[?2004h");
    h.click_on("shell-dot-p1-0");
    assert!(h.root(RootView::shell_menu_can_rerun));
    h.click_on("shell-menu-rerun");
    assert_eq!(
        h.sent_input("s1"),
        b"\x1b[200~echo a\necho b\x1b[201~".to_vec(),
        "wrapped as a paste, its newline kept and no Enter after it"
    );
}

#[gpui::test]
fn rerun_is_disabled_for_a_multi_line_command_without_bracketed_paste(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = with_two_line_command(cx, &dir);
    h.click_on("shell-dot-p1-0");
    assert!(
        !h.root(RootView::shell_menu_can_rerun),
        "a newline would run the first line alone"
    );
    h.click_on("shell-menu-rerun");
    assert!(h.sent_input("s1").is_empty(), "nothing was typed");
}

#[gpui::test]
fn rerun_is_disabled_for_an_empty_command(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, shell(), b"");
    // No `OSC 633;E`, and the prompt row holds nothing to read it from.
    h.pty(
        "s1",
        format!("{PROMPT}{TYPED}\r\n{OUTPUT}out\r\n\x1b]133;D;0\x07{PROMPT}$ ").as_bytes(),
    );
    h.click_on("shell-dot-p1-0");
    assert!(!h.root(RootView::shell_menu_can_rerun));
    h.click_on("shell-menu-rerun");
    assert!(h.sent_input("s1").is_empty(), "nothing was typed");
}

#[gpui::test]
fn rerun_is_disabled_for_a_command_holding_a_carriage_return(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, shell(), b"");
    h.pty("s1", b"\x1b[?2004h");
    h.pty(
        "s1",
        format!(
            "{PROMPT}$ {TYPED}ls\r\n\x1b]633;E;ls\\x0drm x\x07{OUTPUT}out\r\n\x1b]133;D;0\x07{PROMPT}$ "
        )
        .as_bytes(),
    );
    h.click_on("shell-dot-p1-0");
    assert!(
        !h.root(RootView::shell_menu_can_rerun),
        "the carriage return would run `ls` before the rest was typed"
    );
    h.click_on("shell-menu-rerun");
    assert!(h.sent_input("s1").is_empty(), "nothing was typed");
}

#[gpui::test]
fn ctrl_shift_n_with_the_menu_open_opens_no_spawn_dialog(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(shell());
    fixture.repos = vec![repo("r1", "C:/r1")];
    let mut h = Harness::with(cx, &dir, &fixture);
    h.answer_scrollback("s1", b"");
    h.pty("s1", ran("ls", "one", ";0").as_bytes());
    h.click_on("shell-dot-p1-0");
    h.keys("ctrl-shift-n");
    assert!(
        !h.root(|root, _| root.spawn_dialog_open()),
        "the dot menu blocks the spawn dialog"
    );
    h.keys("escape");
    h.keys("ctrl-shift-n");
    assert!(
        h.root(|root, _| root.spawn_dialog_open()),
        "with the menu closed, it opens"
    );
}

#[gpui::test]
fn a_font_key_closes_the_menu(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = with_command(cx, &dir, "ls", "one");
    h.click_on("shell-dot-p1-0");
    assert!(h.in_model("shell-menu"));
    h.keys("ctrl-=");
    assert!(!h.in_model("shell-menu"), "Ctrl+= closed the menu");
}
