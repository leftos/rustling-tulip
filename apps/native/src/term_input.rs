//! What the terminal pane does with a keystroke or a paste, given the
//! attached session: send bytes, copy the selection, or paste.

use alacritty_terminal::vte::ansi::CursorShape;
use gpui::Keystroke;
use protocol::{Agent, SessionMode, SessionSnapshot, SessionStatus};

use crate::keys;

/// The parts of the attached session that shape its input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionContext {
    pub mode: SessionMode,
    pub agent: Agent,
    pub status: SessionStatus,
}

impl SessionContext {
    pub fn of(session: &SessionSnapshot) -> Self {
        Self {
            mode: session.mode,
            agent: session.agent,
            status: session.status,
        }
    }

    /// The context of session `id` in a full session list, if it is there.
    pub fn find(sessions: &[SessionSnapshot], id: &str) -> Option<Self> {
        sessions.iter().find(|s| s.id == id).map(Self::of)
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

    /// Shift+Enter inserts a newline without submitting: `\` + CR is line
    /// continuation for claude and shells, codex and cursor bind LF.
    fn shift_enter(self) -> &'static [u8] {
        if self.mode == SessionMode::PlainShell || self.agent == Agent::Claude {
            b"\\\r"
        } else {
            b"\n"
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
/// Ctrl+Shift+C only ever copies; Ctrl+V and Ctrl+Shift+V paste.
pub fn key_action(
    ks: &Keystroke,
    app_cursor: bool,
    has_selection: bool,
    session: SessionContext,
) -> Option<KeyAction> {
    let m = ks.modifiers;
    let key = ks.key.to_ascii_lowercase();
    if m.shift && !m.control && !m.platform && key == "enter" {
        return Some(KeyAction::Send(session.shift_enter().to_vec()));
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

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a malformed fixture should fail the test with its message"
)]
mod tests {
    use alacritty_terminal::vte::ansi::CursorShape;
    use gpui::{Keystroke, Modifiers};
    use protocol::{Agent, SessionMode, SessionSnapshot, SessionStatus};

    use super::{KeyAction, SessionContext, key_action, paste_bytes};

    fn session(mode: SessionMode, agent: Agent) -> SessionContext {
        SessionContext {
            mode,
            agent,
            status: SessionStatus::Idle,
        }
    }

    fn claude() -> SessionContext {
        session(SessionMode::Interactive, Agent::Claude)
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
        key_action(ks, false, has_selection, claude())
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
    fn shift_enter_sends_per_agent_newline() {
        let cases: [(SessionMode, Agent, &[u8]); 4] = [
            (SessionMode::PlainShell, Agent::Claude, b"\\\r"),
            (SessionMode::Interactive, Agent::Claude, b"\\\r"),
            (SessionMode::Interactive, Agent::Codex, b"\n"),
            (SessionMode::Interactive, Agent::Cursor, b"\n"),
        ];
        let enter = key("enter", shift());
        for (mode, agent, bytes) in cases {
            assert_eq!(
                key_action(&enter, false, false, session(mode, agent)),
                Some(KeyAction::Send(bytes.to_vec())),
                "{mode:?} {agent:?}"
            );
        }
        let plain = key("enter", Modifiers::default());
        assert_eq!(action(&plain, false), Some(KeyAction::Send(b"\r".to_vec())));
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
    fn session_list_refresh_finds_the_attached_session() {
        let snapshot = |id: &str, status: &str| -> SessionSnapshot {
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
                "agent": "codex",
            }))
            .expect("session fixture")
        };
        let sessions = [snapshot("a", "idle"), snapshot("b", "stopped")];
        assert_eq!(
            SessionContext::find(&sessions, "b"),
            Some(SessionContext {
                mode: SessionMode::PlainShell,
                agent: Agent::Codex,
                status: SessionStatus::Stopped,
            })
        );
        assert_eq!(SessionContext::find(&sessions, "c"), None);
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
            session(SessionMode::PlainShell, Agent::Claude).default_cursor_shape(),
            CursorShape::Block
        );
    }
}
