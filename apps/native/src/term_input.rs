//! What the terminal pane does with a keystroke or a paste, given the
//! attached session: send bytes, copy the selection, or paste.

use alacritty_terminal::vte::ansi::CursorShape;
use gpui::Keystroke;
use protocol::{AgentDescriptor, SessionMode, SessionSnapshot, SessionStatus, ShiftEnter};

use crate::keys;
use crate::sidebar::{agent_up, runtime_label};

/// Shift+Enter's fallback bytes: `\` + CR is line continuation for claude and
/// for shells. An agent whose descriptor names [`ShiftEnter::Lf`] gets a line
/// feed instead.
const SHIFT_ENTER: &[u8] = b"\\\r";

/// The bytes Shift+Enter sends to a session that chose `shift_enter`.
fn shift_enter_bytes(shift_enter: ShiftEnter) -> &'static [u8] {
    match shift_enter {
        ShiftEnter::Lf => b"\n",
        ShiftEnter::BackslashCr | ShiftEnter::Unknown => SHIFT_ENTER,
    }
}

/// The parts of the attached session that shape its input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionContext {
    pub mode: SessionMode,
    pub status: SessionStatus,
    /// What Shift+Enter sends while the session's agent is up; a shell's
    /// prompt takes `\` + CR.
    pub shift_enter: ShiftEnter,
}

impl SessionContext {
    pub fn of(session: &SessionSnapshot, agents: &[AgentDescriptor]) -> Self {
        let runtime = runtime_label(session, agents);
        Self {
            mode: session.mode,
            status: session.status,
            shift_enter: agent_up(session, runtime.as_deref(), agents)
                .map_or(ShiftEnter::BackslashCr, |descriptor| descriptor.shift_enter),
        }
    }

    /// A stopped or failed session takes no input: keys, pastes, mouse
    /// reports and terminal replies are all dropped.
    pub fn accepts_input(self) -> bool {
        !matches!(self.status, SessionStatus::Stopped | SessionStatus::Error)
    }

    /// Agents get a bar cursor, shells and everything else a block. A
    /// program's own cursor-style request still wins.
    pub fn default_cursor_shape(self) -> CursorShape {
        if self.mode == SessionMode::Interactive {
            CursorShape::Beam
        } else {
            CursorShape::Block
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum KeyAction {
    Send(Vec<u8>),
    /// Copy the selection, then clear it when `clear_selection` is set.
    Copy {
        clear_selection: bool,
    },
    Paste,
    /// Handled, with nothing to send.
    Consume,
}

/// Decides what a keystroke does. `None` means the terminal does not handle
/// it. Ctrl+C copies when there is a selection and interrupts otherwise;
/// Ctrl+Shift+C only ever copies; Ctrl+V and Ctrl+Shift+V paste. Shift+Enter
/// sends the bytes `shift_enter` names.
pub fn key_action(
    ks: &Keystroke,
    app_cursor: bool,
    has_selection: bool,
    shift_enter: ShiftEnter,
) -> Option<KeyAction> {
    let m = ks.modifiers;
    let key = ks.key.to_ascii_lowercase();
    if m.shift && !m.control && !m.platform && key == "enter" {
        return Some(KeyAction::Send(shift_enter_bytes(shift_enter).to_vec()));
    }
    if m.control && !m.alt && !m.platform {
        match key.as_str() {
            "v" => return Some(KeyAction::Paste),
            "c" if has_selection => {
                return Some(KeyAction::Copy {
                    clear_selection: !m.shift,
                });
            }
            "c" if m.shift => return Some(KeyAction::Consume),
            _ => {}
        }
    }
    keys::to_bytes(ks, app_cursor).map(KeyAction::Send)
}

/// What a key the terminal handles itself does to a pending dead key's
/// accent.
#[derive(Debug, PartialEq, Eq)]
pub enum DeadKeyFate {
    /// The accent is withdrawn and the key does nothing else.
    Cancel,
    /// The accent is dropped and the key acts as usual.
    Drop,
    /// The accent is sent, then the key acts as usual.
    SendFirst,
    /// The accent is sent in place of the key.
    SendInstead,
}

/// Backspace cancels a pending accent, Escape drops it, Space (Shift
/// allowed) types it alone as Windows does, and every other key the terminal
/// handles sends it ahead of its own input.
pub fn dead_key_fate(ks: &Keystroke) -> DeadKeyFate {
    let m = ks.modifiers;
    match ks.key.as_str() {
        "backspace" => DeadKeyFate::Cancel,
        "escape" => DeadKeyFate::Drop,
        "space" if !m.control && !m.alt => DeadKeyFate::SendInstead,
        _ => DeadKeyFate::SendFirst,
    }
}

const PASTE_START: &str = "\x1b[200~";
const PASTE_END: &str = "\x1b[201~";

/// Removes bracket markers until none are left, including ones that a single
/// pass would assemble from the pieces around a removed marker.
fn strip_bracket_markers(mut text: String) -> String {
    loop {
        let stripped = text.replace(PASTE_START, "").replace(PASTE_END, "");
        if stripped == text {
            return text;
        }
        text = stripped;
    }
}

/// The bytes a paste sends: newlines become CR, as a typed Enter would, and
/// the text is wrapped in bracketed-paste markers when the program asked.
/// Markers inside a bracketed paste are removed, so the text cannot end the
/// bracket early and have the rest read as typed input.
pub fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    let text = text.replace("\r\n", "\r").replace('\n', "\r");
    if bracketed {
        let text = strip_bracket_markers(text);
        format!("{PASTE_START}{text}{PASTE_END}").into_bytes()
    } else {
        text.into_bytes()
    }
}

/// The bytes a paste sends. With no text on the clipboard, an image-only
/// clipboard reads as no text on Windows, so the paste goes out as a plain
/// `^V` (0x16) and the program decides what to do with it — an agent's own
/// image paste reads the bitmap itself. Text pastes exactly as
/// [`paste_bytes`] sends it.
pub fn paste_input(text: Option<&str>, bracketed: bool) -> Vec<u8> {
    match text {
        Some(text) if !text.is_empty() => paste_bytes(text, bracketed),
        _ => vec![0x16],
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a malformed fixture should fail the test with its message"
)]
mod tests {
    use alacritty_terminal::vte::ansi::CursorShape;
    use gpui::{Keystroke, Modifiers};
    use protocol::{
        AgentDescriptor, AgentId, SessionMode, SessionSnapshot, SessionStatus, ShiftEnter,
    };

    use super::{KeyAction, SessionContext, key_action, paste_bytes, paste_input};

    fn session(mode: SessionMode) -> SessionContext {
        SessionContext {
            mode,
            status: SessionStatus::Idle,
            shift_enter: ShiftEnter::BackslashCr,
        }
    }

    /// The test agent's descriptor under `id`, sending `shift_enter` for a
    /// newline without submitting.
    fn descriptor(id: &str, shift_enter: ShiftEnter) -> AgentDescriptor {
        let mut descriptor = agent_api::test_agent::DESCRIPTOR.clone();
        descriptor.id = AgentId(id.to_owned());
        descriptor.shift_enter = shift_enter;
        descriptor
    }

    /// An idle-elsewhere snapshot running `agent`, whose `program_name` is
    /// `program`; `None` for the agent is a plain shell.
    fn snapshot(
        id: &str,
        status: &str,
        agent: Option<&str>,
        program: Option<&str>,
    ) -> SessionSnapshot {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "label": id,
            "kind": "single",
            "members": [],
            "status": status,
            "mode": "plain_shell",
            "started_at": "2026-01-01T00:00:00Z",
            "exit_code": null,
            "metrics": { "input_tokens": 0, "output_tokens": 0, "cost_usd": 0.0, "last_activity_at": null },
            "recent_actions": [],
            "agent": agent,
            "program_name": program,
        }))
        .expect("session fixture")
    }

    /// What Shift+Enter does in `session` under `agents`.
    fn shift_enter_action_of(
        session: &SessionSnapshot,
        agents: &[AgentDescriptor],
    ) -> Option<KeyAction> {
        let context = SessionContext::of(session, agents);
        action_with(&key("enter", shift()), false, context.shift_enter)
    }

    /// What Shift+Enter does in a session running `agent` whose program is
    /// not reported yet.
    fn shift_enter_action(agent: Option<&str>, agents: &[AgentDescriptor]) -> Option<KeyAction> {
        shift_enter_action_of(&snapshot("s1", "idle", agent, None), agents)
    }

    fn claude() -> SessionContext {
        session(SessionMode::Interactive)
    }

    fn key(name: &str, modifiers: Modifiers) -> Keystroke {
        Keystroke {
            key: name.to_owned(),
            key_char: Some(name.to_owned()),
            modifiers,
        }
    }

    fn ctrl(shift: bool) -> Modifiers {
        Modifiers {
            control: true,
            shift,
            ..Default::default()
        }
    }

    fn shift() -> Modifiers {
        Modifiers {
            shift: true,
            ..Default::default()
        }
    }

    fn alt(control: bool) -> Modifiers {
        Modifiers {
            control,
            alt: true,
            ..Default::default()
        }
    }

    fn typed(name: &str, text: &str, modifiers: Modifiers) -> Keystroke {
        Keystroke {
            key: name.to_owned(),
            key_char: Some(text.to_owned()),
            modifiers,
        }
    }

    fn action(ks: &Keystroke, has_selection: bool) -> Option<KeyAction> {
        action_with(ks, has_selection, ShiftEnter::BackslashCr)
    }

    fn action_with(
        ks: &Keystroke,
        has_selection: bool,
        shift_enter: ShiftEnter,
    ) -> Option<KeyAction> {
        key_action(ks, false, has_selection, shift_enter)
    }

    #[expect(
        clippy::unnecessary_wraps,
        reason = "compared with key_action's result"
    )]
    fn sends(text: &str) -> Option<KeyAction> {
        Some(KeyAction::Send(text.as_bytes().to_vec()))
    }

    #[test]
    fn plain_text_key_is_left_to_the_input_handler() {
        let cases = [
            typed("a", "a", Modifiers::default()),
            typed("a", "A", shift()),
            typed("´", "´", Modifiers::default()),
        ];
        for ks in cases {
            assert_eq!(action(&ks, false), None, "{ks:?}");
        }
        assert_eq!(
            action(&typed("space", " ", Modifiers::default()), false),
            sends(" ")
        );
        assert_eq!(action(&typed("enter", "\r", shift()), false), sends("\\\r"));
    }

    #[test]
    fn ctrl_alt_symbol_is_sent_as_text() {
        for (name, text) in [("q", "@"), ("e", "€"), ("7", "{")] {
            assert_eq!(
                action(&typed(name, text, alt(true)), false),
                sends(text),
                "{name}"
            );
        }
    }

    #[test]
    fn ctrl_alt_letter_keeps_escape_prefix() {
        assert_eq!(action(&typed("a", "a", alt(true)), false), sends("\x1ba"));
        assert_eq!(action(&typed("1", "1", alt(true)), false), sends("\x1b1"));
    }

    #[test]
    fn alt_letter_still_sends_escape_prefix() {
        assert_eq!(action(&typed("x", "x", alt(false)), false), sends("\x1bx"));
        assert_eq!(action(&typed("x", "X", alt(false)), false), sends("\x1bX"));
    }

    #[test]
    fn shift_enter_sends_line_continuation() {
        assert_eq!(
            action(&key("enter", shift()), false),
            Some(KeyAction::Send(b"\\\r".to_vec()))
        );
        let plain = key("enter", Modifiers::default());
        assert_eq!(action(&plain, false), Some(KeyAction::Send(b"\r".to_vec())));
    }

    #[test]
    fn shift_enter_sends_lf_for_an_lf_agent() {
        let agents = [descriptor("test-agent", ShiftEnter::Lf)];
        let tag = agents[0].tag.clone();
        assert_eq!(
            shift_enter_action_of(
                &snapshot("s1", "idle", Some("test-agent"), Some(&tag)),
                &agents
            ),
            sends("\n"),
            "the agent is up: the runtime tag is its own"
        );
    }

    #[test]
    fn shift_enter_uses_shell_bytes_while_the_agents_shell_label_shows() {
        let agents = [descriptor("test-agent", ShiftEnter::Lf)];
        let tag = agents[0].tag.clone();
        assert_eq!(
            shift_enter_action_of(
                &snapshot("s1", "idle", Some("test-agent"), Some("pwsh")),
                &agents
            ),
            sends("\\\r"),
            "the shell's prompt has the pane, where a line feed runs the command"
        );
        assert_eq!(
            shift_enter_action_of(
                &snapshot("s1", "idle", Some("test-agent"), Some(&tag)),
                &agents
            ),
            sends("\n"),
            "the agent is up again"
        );
    }

    #[test]
    fn shift_enter_sends_backslash_cr_for_a_plain_shell() {
        let agents = [descriptor("test-agent", ShiftEnter::Lf)];
        assert_eq!(shift_enter_action(None, &agents), sends("\\\r"));
        assert_eq!(
            shift_enter_action(Some("gone-agent"), &agents),
            sends("\\\r"),
            "an agent the daemon does not describe keeps today's bytes"
        );
    }

    #[test]
    fn shift_enter_falls_back_for_an_unknown_choice() {
        let agents = [descriptor("future-agent", ShiftEnter::Unknown)];
        let tag = agents[0].tag.clone();
        assert_eq!(
            shift_enter_action_of(
                &snapshot("s1", "idle", Some("future-agent"), Some(&tag)),
                &agents
            ),
            sends("\\\r"),
            "the agent is up but names a choice this client does not know"
        );
    }

    #[test]
    fn ctrl_c_copies_a_selection_and_interrupts_without_one() {
        let ctrl_c = key("c", ctrl(false));
        assert_eq!(
            action(&ctrl_c, true),
            Some(KeyAction::Copy {
                clear_selection: true
            })
        );
        assert_eq!(action(&ctrl_c, false), Some(KeyAction::Send(vec![0x03])));
    }

    #[test]
    fn ctrl_shift_c_copies_and_is_a_no_op_without_a_selection() {
        let ctrl_shift_c = key("c", ctrl(true));
        assert_eq!(
            action(&ctrl_shift_c, true),
            Some(KeyAction::Copy {
                clear_selection: false
            })
        );
        assert_eq!(action(&ctrl_shift_c, false), Some(KeyAction::Consume));
    }

    #[test]
    fn ctrl_v_and_ctrl_shift_v_both_paste() {
        assert_eq!(
            action(&key("v", ctrl(false)), false),
            Some(KeyAction::Paste)
        );
        assert_eq!(action(&key("v", ctrl(true)), false), Some(KeyAction::Paste));
    }

    #[test]
    fn paste_normalises_newlines_and_brackets_on_request() {
        assert_eq!(paste_bytes("a\r\nb\nc", false), b"a\rb\rc".to_vec());
        assert_eq!(
            paste_bytes("a\r\nb", true),
            b"\x1b[200~a\rb\x1b[201~".to_vec()
        );
    }

    #[test]
    fn bracketed_paste_strips_bracket_markers_from_the_text() {
        assert_eq!(
            paste_bytes("a\x1b[201~rm -rf\x1b[200~b", true),
            b"\x1b[200~arm -rfb\x1b[201~".to_vec()
        );
        assert_eq!(
            paste_bytes("\x1b[20\x1b[201~1~x", true),
            b"\x1b[200~x\x1b[201~".to_vec()
        );
        assert_eq!(paste_bytes("a\x1b[201~b", false), b"a\x1b[201~b".to_vec());
    }

    #[test]
    fn paste_without_text_sends_ctrl_v() {
        assert_eq!(paste_input(None, false), vec![0x16]);
        assert_eq!(paste_input(None, true), vec![0x16]);
        assert_eq!(paste_input(Some(""), false), vec![0x16]);
        assert_eq!(paste_input(Some(""), true), vec![0x16]);
    }

    #[test]
    fn paste_with_text_matches_paste_bytes() {
        for bracketed in [false, true] {
            assert_eq!(
                paste_input(Some("hi"), bracketed),
                paste_bytes("hi", bracketed)
            );
        }
    }

    #[test]
    fn a_session_context_reads_mode_and_status() {
        assert_eq!(
            SessionContext::of(&snapshot("b", "stopped", Some("claude"), None), &[]),
            SessionContext {
                mode: SessionMode::PlainShell,
                status: SessionStatus::Stopped,
                shift_enter: ShiftEnter::BackslashCr,
            }
        );
    }

    #[test]
    fn input_is_dropped_only_for_stopped_and_error() {
        let with = |status| SessionContext { status, ..claude() };
        for status in [SessionStatus::Stopped, SessionStatus::Error] {
            assert!(!with(status).accepts_input(), "{status:?}");
        }
        for status in [
            SessionStatus::Idle,
            SessionStatus::Working,
            SessionStatus::AwaitingInput,
        ] {
            assert!(with(status).accepts_input(), "{status:?}");
        }
    }

    #[test]
    fn default_cursor_is_a_bar_for_agents_and_a_block_for_shells() {
        assert_eq!(claude().default_cursor_shape(), CursorShape::Beam);
        assert_eq!(
            session(SessionMode::PlainShell).default_cursor_shape(),
            CursorShape::Block
        );
    }
}
