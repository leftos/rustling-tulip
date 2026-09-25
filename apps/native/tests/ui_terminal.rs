//! Terminal pane specs: keys, selection and copy, paste, mouse reports,
//! terminal replies, the scrollback retry and the cursor shape.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

#[expect(dead_code, reason = "each spec file uses its own share of the helper")]
mod support;

use std::time::Duration;

use alacritty_terminal::vte::ansi::CursorShape;
use gpui::{Modifiers, Point, TestAppContext, point, px, size};
use protocol::{ClientMessage, DaemonMessage, SessionSnapshot, SplitDirection};
use support::{Fixture, Harness, TestDir, pane, session, split, tab};

/// `s` alone in pane `p1`, its scrollback answered, the pane focused and
/// everything sent so far drained.
fn attached<'a>(cx: &'a mut TestAppContext, dir: &TestDir, s: SessionSnapshot) -> Harness<'a> {
    let id = s.id.clone();
    let mut h = Harness::with(cx, dir, &Fixture::single(s));
    h.answer_scrollback(&id, b"");
    let at = h.cell_center("p1", 0, 0);
    h.click(at, Modifiers::none());
    h.sent();
    h
}

/// A point in cell (`col`, `row`), `dx` pixels off its centre, so a drag
/// starts on a cell's left half and ends on its right half.
fn near(h: &mut Harness<'_>, col: usize, row: usize, dx: f32) -> Point<gpui::Pixels> {
    let at = h.cell_center("p1", col, row);
    point(at.x + px(dx), at.y)
}

#[gpui::test]
fn shift_enter_sends_line_continuation_per_agent(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let cases = [
        (
            session("sh").shell("D:/x").agent("codex").build(),
            b"\\\r".as_slice(),
        ),
        (session("cl").build(), b"\\\r".as_slice()),
        (session("cx").agent("codex").build(), b"\n".as_slice()),
    ];
    for (s, want) in cases {
        let id = s.id.clone();
        let mut h = attached(cx, &dir, s);
        h.keys("shift-enter");
        assert_eq!(h.sent_input(&id), want, "session {id}");
    }
}

#[gpui::test]
fn drag_selects_copies_on_release_and_ctrl_c_copies_instead_of_interrupting(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, session("s1").build());
    h.pty("s1", b"hello world");

    let (from, to) = (near(&mut h, 0, 0, -2.0), near(&mut h, 4, 0, 2.0));
    h.drag(from, to, [Modifiers::none(); 2]);
    assert_eq!(h.clipboard().as_deref(), Some("hello"), "copy on select");

    h.set_clipboard("other");
    h.keys("ctrl-c");
    assert!(h.sent_input("s1").is_empty(), "no ^C with a selection");
    assert_eq!(h.clipboard().as_deref(), Some("hello"));
    h.keys("ctrl-c");
    assert_eq!(h.sent_input("s1"), [0x03], "the selection was cleared");
}

#[gpui::test]
fn ctrl_c_without_selection_interrupts_and_ctrl_shift_c_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, session("s1").build());
    h.keys("ctrl-c");
    assert_eq!(h.sent_input("s1"), [0x03]);
    h.keys("ctrl-shift-c");
    assert!(h.sent_input("s1").is_empty());
}

#[gpui::test]
fn double_click_selects_a_word(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, session("s1").build());
    h.pty("s1", b"hello world");
    let at = h.cell_center("p1", 7, 0);
    h.double_click(at);
    assert_eq!(h.clipboard().as_deref(), Some("world"));
}

#[gpui::test]
fn ctrl_v_pastes_and_brackets_when_the_child_asks(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, session("s1").build());
    h.set_clipboard("a\r\nb\nc");
    h.keys("ctrl-v");
    assert_eq!(h.sent_input("s1"), b"a\rb\rc");

    h.pty("s1", b"\x1b[?2004h");
    h.set_clipboard("x\r\ny\x1b[201~z");
    h.keys("ctrl-v");
    assert_eq!(h.sent_input("s1"), b"\x1b[200~x\ryz\x1b[201~");
}

#[gpui::test]
fn sgr_mouse_reports_clicks_and_shift_selects_instead(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, session("s1").build());
    h.pty("s1", b"hello world\x1b[?1000h\x1b[?1006h");

    let at = h.cell_center("p1", 2, 3);
    h.click(at, Modifiers::none());
    assert_eq!(h.sent_input("s1"), b"\x1b[<0;3;4M\x1b[<0;3;4m");

    h.click(at, Modifiers::shift());
    assert!(h.sent_input("s1").is_empty(), "Shift selects, no report");

    let (from, to) = (near(&mut h, 0, 0, -2.0), near(&mut h, 4, 0, 2.0));
    h.drag(from, to, [Modifiers::shift(), Modifiers::none()]);
    assert!(
        h.sent_input("s1").is_empty(),
        "releasing Shift keeps selecting"
    );
    assert_eq!(h.clipboard().as_deref(), Some("hello"));
}

#[gpui::test]
fn utf8_mouse_reports_encode_columns_past_95(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, session("s1").build());
    h.cx.simulate_resize(size(px(1600.0), px(900.0)));
    h.pty("s1", b"\x1b[?1005h\x1b[?1000h");

    let at = h.cell_center("p1", 99, 0);
    h.click(at, Modifiers::none());

    let want = format!("\x1b[M {c}!\x1b[M#{c}!", c = '\u{84}');
    assert_eq!(h.sent_input("s1"), want.as_bytes());
}

#[gpui::test]
fn stopped_session_drops_keys_and_pastes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, session("s1").build());
    h.keys("a");
    assert_eq!(h.sent_input("s1"), b"a", "a live session takes the key");

    h.send(DaemonMessage::SessionUpdated {
        session: session("s1").status("stopped").build(),
        request_id: None,
    });
    h.keys("a enter");
    h.set_clipboard("x");
    h.keys("ctrl-v");
    assert!(h.sent_input("s1").is_empty());
}

#[gpui::test]
fn cursor_position_query_is_answered_live_but_not_from_history(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    h.answer_scrollback("s1", b"old\x1b[6nnew");
    assert!(h.sent_input("s1").is_empty(), "history is not answered");

    h.pty("s1", b"\x1b[6n");
    let reply = h.sent_input("s1");
    assert!(
        reply.starts_with(b"\x1b[") && reply.ends_with(b"R"),
        "cursor position reply, got {reply:?}"
    );
}

/// The input sent to `s1` after its scrollback is answered with `history`.
fn input_after_history(cx: &mut TestAppContext, history: &[u8]) -> Vec<u8> {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    h.answer_scrollback("s1", history);
    h.sent_input("s1")
}

#[gpui::test]
fn conpty_startup_query_still_pending_in_history_is_answered(cx: &mut TestAppContext) {
    let reply = input_after_history(cx, b"\x1b[6n");
    assert!(
        reply.starts_with(b"\x1b[") && reply.ends_with(b"R"),
        "the pending query is answered, got {reply:?}"
    );
}

#[gpui::test]
fn trailing_query_after_other_history_is_answered(cx: &mut TestAppContext) {
    let reply = input_after_history(cx, b"hello\r\n\x1b[6n");
    assert!(
        reply.starts_with(b"\x1b[") && reply.ends_with(b"R"),
        "the pending query is answered, got {reply:?}"
    );
}

#[gpui::test]
fn query_followed_by_output_in_history_is_not_answered(cx: &mut TestAppContext) {
    let reply = input_after_history(cx, b"\x1b[6nPS C:\\> ");
    assert!(
        reply.is_empty(),
        "an answered query is not answered again, got {reply:?}"
    );
}

#[gpui::test]
fn trailing_query_followed_by_buffered_live_output_is_not_answered_twice(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    h.pty_raw("s1", b"PS C:\\> ");
    h.answer_scrollback("s1", b"hello\r\n\x1b[6n");
    let reply = h.sent_input("s1");
    assert!(
        reply.is_empty(),
        "output after the query means another client answered it, got {reply:?}"
    );
}

#[gpui::test]
fn trailing_query_repeated_in_buffered_live_is_answered_once(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    h.pty_raw("s1", b"\x1b[6n");
    h.answer_scrollback("s1", b"hello\r\n\x1b[6n");
    let reply = h.sent_input("s1");
    assert_eq!(
        cursor_replies(&reply),
        1,
        "one query pending, one answer, got {reply:?}"
    );
}

/// How many cursor-position replies (`ESC [ row ; col R`) `input` holds.
fn cursor_replies(input: &[u8]) -> usize {
    input
        .split(|&byte| byte == 0x1b)
        .filter(|reply| reply.starts_with(b"[") && reply.ends_with(b"R"))
        .count()
}

/// `s1` in both panes of one tab, `p1` and `p2`.
fn two_panes_on_s1<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let grid = split(
        SplitDirection::Horizontal,
        pane("p1", Some("s1")),
        pane("p2", Some("s1")),
    );
    let fixture = Fixture {
        sessions: vec![session("s1").build()],
        tabs: vec![tab("t1", &grid)],
        ..Fixture::default()
    };
    Harness::with(cx, dir, &fixture)
}

#[gpui::test]
fn two_panes_on_one_session_answer_a_live_query_once(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_panes_on_s1(cx, &dir);
    h.answer_scrollback("s1", b"");
    h.sent();

    h.pty("s1", b"\x1b[6n");
    let reply = h.sent_input("s1");
    assert_eq!(
        cursor_replies(&reply),
        1,
        "one pane answers for the session, got {reply:?}"
    );
}

#[gpui::test]
fn two_panes_on_one_session_answer_a_pending_history_query_once(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_panes_on_s1(cx, &dir);
    h.answer_scrollback("s1", b"hello\r\n\x1b[6n");
    let reply = h.sent_input("s1");
    assert_eq!(
        cursor_replies(&reply),
        1,
        "one pane answers for the session, got {reply:?}"
    );
}

#[gpui::test]
fn scrollback_timeout_retries_and_then_drains_buffered_output(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    h.sent();
    let requests = |sent: &[ClientMessage]| {
        sent.iter()
            .filter(
                |m| matches!(m, ClientMessage::LoadScrollback { session_id } if session_id == "s1"),
            )
            .count()
    };

    h.advance(Duration::from_secs(8));
    let screen = h.grid_text("p1").join("\n");
    assert!(
        screen.contains("retrying (1/2)"),
        "retry line, got {screen:?}"
    );
    assert_eq!(requests(&h.sent()), 0, "the retry waits out its backoff");

    h.advance(Duration::from_secs(2));
    assert_eq!(requests(&h.sent()), 1, "one retry request");

    h.pty_raw("s1", b"live");
    assert!(!h.grid_text("p1").join("\n").contains("live"), "held back");
    h.answer_scrollback("s1", b"hist\r\n");
    let screen = h.grid_text("p1").join("\n");
    assert!(
        screen.contains("hist") && screen.contains("live"),
        "got {screen:?}"
    );
}

#[gpui::test]
fn cursor_is_a_bar_for_claude_and_a_block_for_a_shell(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    for (s, want) in [
        (session("cl").build(), CursorShape::Beam),
        (session("sh").shell("D:/x").build(), CursorShape::Block),
    ] {
        let id = s.id.clone();
        let mut h = attached(cx, &dir, s);
        let shape = h.root(|root, cx| root.pane_cursor_shape("p1", cx));
        assert_eq!(shape, Some(want), "session {id}");
    }
}
