//! Prompt jump specs: a pane header's previous and next prompt buttons put
//! the prompts the owner sent at the top of the view, a Claude pane's found
//! as the `❯ ` rows Claude echoes, a plain shell's from its OSC 133 marks.

use crate::support;

use gpui::{Modifiers, TestAppContext, px};
use protocol::SessionSnapshot;
use support::{Fixture, Harness, TestDir, session};

const PREVIOUS: &str = "pane-prompt-prev-p1";
const NEXT: &str = "pane-prompt-next-p1";
/// Claude's live prompt box, the bottom of every Claude screen.
const LIVE_BOX: &str = "────────\r\n❯ \r\n────────\r\n";

/// `s` alone in pane `p1`, its scrollback answered empty, the pane focused
/// and everything sent so far drained.
fn attached<'a>(cx: &'a mut TestAppContext, dir: &TestDir, s: SessionSnapshot) -> Harness<'a> {
    let id = s.id.clone();
    let mut h = Harness::with(cx, dir, &Fixture::single(s));
    h.answer_scrollback(&id, b"");
    let at = h.cell_center("p1", 0, 0);
    h.click(at, Modifiers::none());
    h.sent();
    h
}

/// `rows` lines of output named `tag`.
fn output(tag: &str, rows: usize) -> String {
    let mut text = String::new();
    for n in 0..rows {
        text.push_str(tag);
        text.push(' ');
        text.push_str(&n.to_string());
        text.push_str("\r\n");
    }
    text
}

/// A Claude transcript: each prompt echoed as `❯ <prompt>`, then more
/// output than the screen holds, then the live prompt box.
fn claude_turns(prompts: &[&str], screen: usize) -> String {
    let mut text = String::new();
    for prompt in prompts {
        text.push_str("❯ ");
        text.push_str(prompt);
        text.push_str("\r\n");
        text.push_str(&output(prompt, screen + 2));
    }
    text.push_str(LIVE_BOX);
    text
}

/// Whether the previous and next buttons of `p1` can jump.
fn jumps(h: &mut Harness<'_>) -> (bool, bool) {
    h.root(|root, cx| root.pane_prompt_jumps("p1", cx))
}

fn top_row(h: &mut Harness<'_>) -> String {
    h.grid_text("p1")[0].trim_end().to_owned()
}

#[gpui::test]
fn previous_prompt_button_puts_the_prompt_at_the_top_row(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, session("s1").build());
    let screen = h.grid_text("p1").len();
    h.pty(
        "s1",
        claude_turns(&["first", "second", "third"], screen).as_bytes(),
    );
    h.click_on(PREVIOUS);
    assert_eq!(
        top_row(&mut h),
        "❯ third",
        "the newest prompt above the view"
    );
    h.click_on(PREVIOUS);
    assert_eq!(top_row(&mut h), "❯ second");
    h.click_on(PREVIOUS);
    assert_eq!(top_row(&mut h), "❯ first");
    assert_eq!(
        jumps(&mut h),
        (false, true),
        "nothing is older than the first"
    );
}

#[gpui::test]
fn next_from_the_newest_returns_to_the_live_bottom(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, session("s1").build());
    let screen = h.grid_text("p1").len();
    h.pty("s1", claude_turns(&["first", "second"], screen).as_bytes());
    let live = h.grid_text("p1");
    assert_eq!(
        jumps(&mut h),
        (true, false),
        "next waits on the live screen"
    );
    h.click_on(PREVIOUS);
    h.click_on(PREVIOUS);
    assert_eq!(top_row(&mut h), "❯ first");
    h.click_on(NEXT);
    assert_eq!(top_row(&mut h), "❯ second");
    h.click_on(NEXT);
    assert_eq!(h.grid_text("p1"), live, "from the newest: the live screen");
    assert_eq!(jumps(&mut h), (true, false));
}

#[gpui::test]
fn buttons_are_disabled_with_no_prompts(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, session("s1").build());
    let screen = h.grid_text("p1").len();
    h.pty(
        "s1",
        format!("{}{LIVE_BOX}", output("line", 2 * screen)).as_bytes(),
    );
    assert_eq!(jumps(&mut h), (false, false));
    for button in [PREVIOUS, NEXT] {
        let width = h.bounds(button).size.width / px(1.0);
        assert!(
            (width - 26.0).abs() < 0.5,
            "{button} is drawn, disabled, at 26 px: {width}"
        );
    }
    let live = h.grid_text("p1");
    h.click_on(PREVIOUS);
    assert_eq!(h.grid_text("p1"), live, "a disabled button does nothing");
}

#[gpui::test]
fn a_plain_shell_jumps_by_its_osc_133_prompts(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, session("s1").shell("C:/work").build());
    let screen = h.grid_text("p1").len();
    let command = |line: &str, out: &str| {
        format!("\x1b]133;A\x07$ {line}\r\n\x1b]133;C\x07{out}\x1b]133;D;0\x07")
    };
    let fake = format!("{}❯ not a prompt\r\n{}", output("b", 2), output("c", 2));
    h.pty("s1", command("one", &output("a", screen + 2)).as_bytes());
    h.pty(
        "s1",
        command("two", &format!("{fake}{}", output("d", screen))).as_bytes(),
    );
    h.pty("s1", b"\x1b]133;A\x07$ ");
    h.click_on(PREVIOUS);
    assert_eq!(
        top_row(&mut h),
        "$ two",
        "the shell's mark, not the `❯ ` row"
    );
    h.click_on(PREVIOUS);
    assert_eq!(top_row(&mut h), "$ one");
    assert_eq!(jumps(&mut h), (false, true));
}

#[gpui::test]
fn a_plain_shell_jumps_to_the_prompt_of_a_running_command(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, session("s1").shell("C:/work").build());
    let screen = h.grid_text("p1").len();
    let finished = format!(
        "\x1b]133;A\x07$ one\r\n\x1b]133;C\x07{}\x1b]133;D;0\x07",
        output("a", 2)
    );
    h.pty("s1", finished.as_bytes());
    let running = format!(
        "\x1b]133;A\x07$ cargo build\r\n\x1b]133;C\x07{}",
        output("b", screen + 2)
    );
    h.pty("s1", running.as_bytes());
    h.click_on(PREVIOUS);
    assert_eq!(
        top_row(&mut h),
        "$ cargo build",
        "the running command's prompt, scrolled off the screen"
    );
    h.click_on(PREVIOUS);
    assert_eq!(top_row(&mut h), "$ one");
}

#[gpui::test]
fn the_buttons_scroll_under_mouse_reporting(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, session("s1").build());
    let screen = h.grid_text("p1").len();
    h.pty("s1", claude_turns(&["first", "second"], screen).as_bytes());
    h.pty("s1", b"\x1b[?1000h\x1b[?1006h");
    h.sent();
    h.click_on(PREVIOUS);
    assert_eq!(top_row(&mut h), "❯ second", "the button scrolled the view");
    assert!(h.sent_input("s1").is_empty(), "and sent the child nothing");
}
