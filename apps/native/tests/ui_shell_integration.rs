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
use protocol::{ClientMessage, SessionSnapshot, SplitDirection};
use rustling_tulip_native::fonts::FontSettings;
use rustling_tulip_native::{ShellDot, ShellStatus};
use support::{Fixture, Harness, TestDir, pane, session, split, tab};

const PROMPT: &str = "\x1b]133;A\x07";
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

    // The program asked for the mouse: a click in the gutter reaches it not.
    h.pty("s1", b"\x1b[?1000h");
    h.click(dot, Modifiers::none());
    assert!(h.sent_input("s1").is_empty(), "nothing reported");
    let cell = h.cell_center("p1", 2, 2);
    h.click(cell, Modifiers::none());
    assert!(
        !h.sent_input("s1").is_empty(),
        "a click on the grid still is"
    );
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
