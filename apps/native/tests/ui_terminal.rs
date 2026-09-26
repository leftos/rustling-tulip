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
use rustling_tulip_native::fonts::FontSettings;
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
fn space_with_windows_key_shape_reaches_the_pty(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, session("s1").build());
    h.key_down(gpui::Keystroke {
        modifiers: Modifiers::none(),
        key: "space".into(),
        key_char: None,
    });
    assert_eq!(h.sent_input("s1"), b" ", "a Windows Space has no key_char");
}

#[gpui::test]
fn typed_text_reaches_the_pty_once(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, session("s1").build());
    h.keys("a shift-b space c");
    assert_eq!(h.sent_input("s1"), b"aB c");
}

#[gpui::test]
fn composition_sends_only_committed_text(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, session("s1").build());
    h.compose("p1", "に");
    h.compose("p1", "にほ");
    assert!(
        h.sent_input("s1").is_empty(),
        "nothing sent while composing"
    );
    assert_eq!(h.marked_text("p1").as_deref(), Some("にほ"));

    h.commit("p1", "日本");
    assert_eq!(h.sent_input("s1"), "日本".as_bytes());
    assert_eq!(h.marked_text("p1"), None, "the commit ends the composition");
}

/// A key press with no modifiers, as the platform delivers it.
fn press(h: &mut Harness<'_>, key: &str, key_char: Option<&str>) {
    h.key_down(gpui::Keystroke {
        modifiers: Modifiers::none(),
        key: key.into(),
        key_char: key_char.map(Into::into),
    });
}

/// The ´ dead key in `p1`: its key press, then the mark Windows makes of it.
fn dead_acute(h: &mut Harness<'_>) {
    press(h, "´", Some("´"));
    h.compose("p1", "´");
}

#[gpui::test]
fn dead_key_then_letter_sends_composed_char(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, session("s1").build());
    dead_acute(&mut h);
    assert!(
        h.sent_input("s1").is_empty(),
        "the dead key alone sends nothing"
    );
    assert_eq!(h.preedit("p1").as_deref(), Some("´"));

    h.commit("p1", "é");
    assert_eq!(h.sent_input("s1"), "é".as_bytes());
    assert_eq!(h.preedit("p1"), None);
}

#[gpui::test]
fn dead_key_mark_is_not_reported_as_composition(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, session("s1").build());
    dead_acute(&mut h);
    assert_eq!(h.marked_text("p1"), None, "keys must still reach the pane");
    assert_eq!(h.preedit("p1").as_deref(), Some("´"), "still drawn");
}

#[gpui::test]
fn ime_mark_is_still_reported_as_composition(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, session("s1").build());
    press(&mut h, "n", Some("n"));
    h.compose("p1", "に");
    assert_eq!(h.marked_text("p1").as_deref(), Some("に"));
    assert_eq!(h.preedit("p1").as_deref(), Some("に"));
}

#[gpui::test]
fn dead_key_then_enter_sends_accent_then_cr(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, session("s1").build());
    dead_acute(&mut h);
    press(&mut h, "enter", None);
    assert_eq!(h.sent_input("s1"), "´\r".as_bytes());
    assert_eq!(h.preedit("p1"), None);
}

#[gpui::test]
fn dead_key_then_backspace_cancels_the_accent(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, session("s1").build());
    dead_acute(&mut h);
    press(&mut h, "backspace", None);
    assert!(h.sent_input("s1").is_empty(), "nothing is erased");
    assert_eq!(h.preedit("p1"), None);
}

#[gpui::test]
fn dead_key_then_escape_sends_only_escape(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, session("s1").build());
    dead_acute(&mut h);
    press(&mut h, "escape", None);
    assert_eq!(h.sent_input("s1"), b"\x1b");
    assert_eq!(h.preedit("p1"), None);
}

#[gpui::test]
fn dead_key_then_space_sends_only_the_accent(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, session("s1").build());
    for modifiers in [Modifiers::none(), Modifiers::shift()] {
        dead_acute(&mut h);
        h.key_down(gpui::Keystroke {
            modifiers,
            key: "space".into(),
            key_char: None,
        });
        assert_eq!(h.sent_input("s1"), "´".as_bytes(), "{modifiers:?}");
        assert_eq!(h.preedit("p1"), None);
    }
}

#[gpui::test]
fn marked_text_dropped_on_blur(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_panes_on_s1(cx, &dir);
    h.answer_scrollback("s1", b"");
    // A test window starts inactive, and gpui reports no focus change in one.
    h.cx.update(|window, _| window.activate_window());
    let p1 = h.cell_center("p1", 0, 0);
    h.click(p1, Modifiers::none());
    h.compose("p1", "´");
    assert_eq!(h.marked_text("p1").as_deref(), Some("´"));

    let p2 = h.cell_center("p2", 0, 0);
    h.click(p2, Modifiers::none());
    assert_eq!(h.marked_text("p1"), None, "blur drops the composition");
    assert!(h.sent_input("s1").is_empty());
}

#[gpui::test]
fn committed_text_dropped_when_stopped(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, session("s1").build());
    h.compose("p1", "´");
    h.send(DaemonMessage::SessionUpdated {
        session: session("s1").status("stopped").build(),
        request_id: None,
    });
    assert_eq!(h.marked_text("p1"), None, "a stop drops the composition");

    h.commit("p1", "x");
    assert!(h.sent_input("s1").is_empty());
}

#[gpui::test]
fn ime_bounds_follow_cursor_cell(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, session("s1").build());
    h.pty("s1", b"abc\r\nx");
    let anchor = h.ime_bounds("p1").expect("an IME anchor");
    let on = h.cell_center("p1", 1, 1);
    let (left, right) = (h.cell_center("p1", 0, 1), h.cell_center("p1", 2, 1));
    let above = h.cell_center("p1", 1, 0);
    assert!(anchor.contains(&on), "the cursor cell, got {anchor:?}");
    for off in [left, right, above] {
        assert!(!anchor.contains(&off), "one cell only, got {anchor:?}");
    }
}

#[gpui::test]
fn ime_bounds_follow_hidden_cursor(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, session("s1").build());
    h.pty("s1", b"abc\r\nxy\x1b[?25l");
    let anchor = h.ime_bounds("p1").expect("an IME anchor");
    let on = h.cell_center("p1", 2, 1);
    let top_left = h.cell_center("p1", 0, 0);
    assert!(
        anchor.contains(&on),
        "the hidden cursor's cell, got {anchor:?}"
    );
    assert!(
        !anchor.contains(&top_left),
        "not the fallback, got {anchor:?}"
    );
}

#[gpui::test]
fn ctrl_alt_symbol_types_the_symbol(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, session("s1").build());
    h.key_down(gpui::Keystroke {
        modifiers: Modifiers {
            control: true,
            alt: true,
            ..Modifiers::default()
        },
        key: "q".into(),
        key_char: Some("@".into()),
    });
    assert_eq!(h.sent_input("s1"), b"@");
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
    // Only a reply that restarted no forwarder leaves held-back live output
    // that is not in the history.
    h.answer_scrollback_from_file("s1", b"hello\r\n\x1b[6n");
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
    h.answer_scrollback_from_file("s1", b"hello\r\n\x1b[6n");
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
                |m| matches!(m, ClientMessage::LoadScrollback { session_id, .. } if session_id == "s1"),
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
    // A reply read from disk restarts no forwarder, so the output held back
    // follows the history.
    h.answer_scrollback_from_file("s1", b"hist\r\n");
    let screen = h.grid_text("p1").join("\n");
    assert!(
        screen.contains("hist") && screen.contains("live"),
        "got {screen:?}"
    );
}

#[gpui::test]
fn reattach_a_b_a_uses_the_reply_to_the_latest_request(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        sessions: vec![session("a").build(), session("b").build()],
        tabs: vec![tab("t1", &pane("p1", Some("a")))],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    for shown in ["b", "a"] {
        h.send(DaemonMessage::TabUpdated {
            tab: tab("t1", &pane("p1", Some(shown))),
        });
    }
    let requests = h.scrollback_requests("a");
    assert_eq!(requests.len(), 2, "two requests for a, got {requests:?}");
    let (first, second) = (&requests[0], &requests[1]);
    assert_ne!(first, second, "every request gets its own id");

    h.pty_raw("a", b"OLD");
    h.answer_scrollback_to("a", Some(first), true, b"STALE\r\n");
    assert!(
        !h.grid_text("p1").join("\n").contains("STALE"),
        "the reply to the first request is dropped"
    );
    h.answer_scrollback_to("a", Some(second), true, b"FRESH\r\n");
    h.pty_raw("a", b"NEW");
    let screen = h.grid_text("p1").join("\n");
    assert!(
        screen.contains("FRESH") && screen.contains("NEW"),
        "got {screen:?}"
    );
    assert!(
        !screen.contains("STALE") && !screen.contains("OLD"),
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

#[gpui::test]
fn osc52_write_reaches_the_clipboard_and_shows_the_chip(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, session("s1").build());
    h.pty("s1", b"\x1b]52;c;aGk=\x07");
    assert_eq!(h.clipboard().as_deref(), Some("hi"));
    assert_eq!(h.root(|root, _| root.copied_chip()), Some(2));
}

#[gpui::test]
fn osc52_read_is_answered_empty(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, session("s1").build());
    h.pty("s1", b"\x1b]52;c;?\x07");
    assert_eq!(h.sent_input("s1"), b"\x1b]52;c;\x07".to_vec());
}

#[gpui::test]
fn osc52_in_loaded_history_is_ignored(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    h.answer_scrollback("s1", b"\x1b]52;c;aGk=\x07");
    assert_ne!(
        h.clipboard().as_deref(),
        Some("hi"),
        "a store in the history copies nothing"
    );
    assert_eq!(h.root(|root, _| root.copied_chip()), None);
}

#[gpui::test]
fn osc52_in_a_session_shown_twice_copies_once(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_panes_on_s1(cx, &dir);
    h.answer_scrollback("s1", b"");
    h.sent();
    h.pty("s1", b"\x1b]52;c;aGk=\x07");
    assert_eq!(h.clipboard().as_deref(), Some("hi"));
    assert_eq!(h.root(|root, _| root.copied_chip()), Some(2));
    assert_eq!(
        h.root(|root, _| root.copied_chip_generation()),
        1,
        "one pane copies for the session"
    );
}

#[gpui::test]
fn selection_copy_shows_the_chip(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, session("s1").build());
    h.pty("s1", b"hello world");

    let (from, to) = (near(&mut h, 0, 0, -2.0), near(&mut h, 4, 0, 2.0));
    h.drag(from, to, [Modifiers::none(); 2]);
    assert_eq!(h.clipboard().as_deref(), Some("hello"), "copy on select");
    assert_eq!(h.root(|root, _| root.copied_chip()), Some(5));

    h.set_clipboard("other");
    h.keys("ctrl-shift-c");
    assert_eq!(h.clipboard().as_deref(), Some("hello"));
    assert_eq!(h.root(|root, _| root.copied_chip()), Some(5));
    assert_eq!(
        h.root(|root, _| root.copied_chip_generation()),
        2,
        "the second copy restarts the chip"
    );
}

#[gpui::test]
fn chip_fades_after_1200ms(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, session("s1").build());
    h.pty("s1", b"hello world");
    let (from, to) = (near(&mut h, 0, 0, -2.0), near(&mut h, 4, 0, 2.0));
    h.drag(from, to, [Modifiers::none(); 2]);
    assert_eq!(h.root(|root, _| root.copied_chip()), Some(5));

    h.advance(Duration::from_millis(1199));
    assert_eq!(h.root(|root, _| root.copied_chip()), Some(5), "still shown");
    h.advance(Duration::from_millis(1));
    assert_eq!(h.root(|root, _| root.copied_chip()), None, "it went");
}

#[gpui::test]
fn a_sync_update_holds_output_back_until_its_timeout(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, session("s1").build());
    h.pty("s1", b"\x1b[?2026hheld");
    assert_eq!(h.grid_text("p1")[0], "", "held back");

    // The pane wakes at the deadline it set when the update began: 150 ms on
    // the injected clock, which the harness moves together with its timers.
    h.advance(Duration::from_millis(150));
    assert_eq!(h.grid_text("p1")[0], "held", "the timeout releases it");

    h.pty("s1", b"\x1b[?2026hhello\x1b[?2026l");
    assert_eq!(h.grid_text("p1")[0], "heldhello", "an end releases it too");
}

/// The column counts of the `Resize`s in `sent` for session `id`, in order.
fn resize_cols(sent: &[ClientMessage], id: &str) -> Vec<u16> {
    sent.iter()
        .filter_map(|m| match m {
            ClientMessage::Resize {
                session_id, cols, ..
            } if session_id == id => Some(*cols),
            _ => None,
        })
        .collect()
}

fn set_app_font(h: &mut Harness<'_>, font: FontSettings) {
    let root = h.root.clone();
    h.cx.update(|_, cx| root.update(cx, |root, cx| root.set_app_font(font, cx)));
    h.cx.run_until_parked();
}

#[gpui::test]
fn cell_width_follows_the_app_font_size(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    h.answer_scrollback("s1", b"");
    let before = *resize_cols(&h.sent(), "s1")
        .last()
        .expect("the pane sized its PTY");

    set_app_font(
        &mut h,
        FontSettings {
            size: 20.0,
            ..FontSettings::default()
        },
    );

    let after = *resize_cols(&h.sent(), "s1")
        .last()
        .expect("the font change resizes the PTY");
    assert!(
        after < before,
        "a larger font fits fewer columns: {before} -> {after}"
    );
}

#[gpui::test]
fn bold_setting_is_applied(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, session("s1").build());
    let bold = |h: &mut Harness<'_>| h.root(|root, cx| root.pane_font("p1", cx).map(|f| f.bold));
    assert_eq!(bold(&mut h), Some(false), "normal weight by default");

    set_app_font(
        &mut h,
        FontSettings {
            bold: true,
            ..FontSettings::default()
        },
    );

    assert_eq!(bold(&mut h), Some(true), "the open pane draws bold");
    let saved: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("native-ui.json")).expect("native-ui.json"),
    )
    .expect("native-ui.json is JSON");
    assert_eq!(
        saved["terminal_font"]["bold"],
        serde_json::Value::Bool(true),
        "the app default is saved"
    );
}
