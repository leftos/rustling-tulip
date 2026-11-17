//! The phone's sticky Ctrl as plain Rust: the flag the Terminal page's key
//! bar and the composer's Keys dock share, whether a key takes it, and the
//! keystroke an armed Ctrl makes of a character typed on the soft keyboard.
//! `terminal_view` and `session_view` draw the keys themselves.

use gpui::{Keystroke, Modifiers};

use super::key_grid::{GridKey, takes_sticky_ctrl};

/// The one sticky Ctrl the bar and the dock share: whether it is armed.
#[derive(Debug, Default)]
pub(crate) struct KeyBar {
    armed: bool,
}

impl KeyBar {
    /// A bar with Ctrl unarmed.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Whether the next keystroke is a Ctrl one.
    pub(crate) fn armed(&self) -> bool {
        self.armed
    }

    /// Arms Ctrl for the next key, or disarms it.
    pub(crate) fn toggle_ctrl(&mut self) {
        self.armed = !self.armed;
    }

    /// Whether armed Ctrl applies to `key`: true, and disarmed, for a key
    /// that takes sticky Ctrl while armed; false, and left as it was, else.
    pub(crate) fn take_ctrl_for(&mut self, key: &GridKey) -> bool {
        let takes = self.armed && takes_sticky_ctrl(key);
        if takes {
            self.armed = false;
        }
        takes
    }

    /// Text typed on the soft keyboard. While Ctrl is armed, one character
    /// becomes its Ctrl keystroke and disarms Ctrl; anything else is left to
    /// be typed as it is.
    pub(crate) fn typed(&mut self, text: &str) -> Option<Keystroke> {
        let mut chars = text.chars();
        let typed = chars.next()?;
        if chars.next().is_some() || !self.armed {
            return None;
        }
        self.armed = false;
        Some(stroke(&typed_name(typed), true))
    }
}

/// The gpui key name for a typed character: the name gpui gives the
/// whitespace keys, else the character itself in lower case.
fn typed_name(typed: char) -> String {
    match typed {
        ' ' => "space".to_owned(),
        '\n' | '\r' => "enter".to_owned(),
        '\t' => "tab".to_owned(),
        typed => typed.to_ascii_lowercase().to_string(),
    }
}

/// The keystroke for a gpui key name, with Ctrl held when `control`. A named
/// key types no character of its own, so `key_char` stays empty.
fn stroke(name: &str, control: bool) -> Keystroke {
    Keystroke {
        key: name.to_owned(),
        key_char: None,
        modifiers: Modifiers {
            control,
            ..Default::default()
        },
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a named test fixture that is missing should fail with its message"
)]
mod tests {
    use gpui::{Keystroke, Modifiers};
    use protocol::ShiftEnter;

    use crate::phone::key_grid::{self, GridKey, NamedKey, Step, StepKey};
    use crate::term_input::{CopyKeys, KeyAction, key_action};

    use super::KeyBar;

    fn stroke(name: &str, control: bool) -> Keystroke {
        Keystroke {
            key: name.to_owned(),
            key_char: None,
            modifiers: Modifiers {
                control,
                ..Default::default()
            },
        }
    }

    /// The default grid's key with `id`.
    fn grid_key(id: &str) -> GridKey {
        key_grid::defaults()
            .into_iter()
            .find(|key| key.id == id)
            .expect("the default grid holds the key")
    }

    /// The plain press a step is.
    fn plain(key: StepKey) -> Step {
        Step {
            ctrl: false,
            alt: false,
            shift: false,
            key,
        }
    }

    #[test]
    fn take_ctrl_for_unarmed_is_false() {
        let mut bar = KeyBar::new();
        assert!(!bar.take_ctrl_for(&grid_key("esc")));
        assert!(!bar.armed(), "an unarmed bar stays unarmed");
    }

    #[test]
    fn take_ctrl_for_a_plain_key_disarms() {
        let mut bar = KeyBar::new();
        bar.toggle_ctrl();
        assert!(bar.take_ctrl_for(&grid_key("esc")));
        assert!(!bar.armed(), "one key takes the armed Ctrl");
    }

    #[test]
    fn take_ctrl_for_a_chord_leaves_ctrl_armed() {
        let mut bar = KeyBar::new();
        bar.toggle_ctrl();
        assert!(!bar.take_ctrl_for(&grid_key("ctrl-c")), "a chord");
        assert!(bar.armed(), "a chord leaves Ctrl armed");
        let sequence = GridKey {
            id: "seq".to_owned(),
            label: "Esc :".to_owned(),
            action: key_grid::KeyAction::Send {
                steps: vec![
                    plain(StepKey::Named {
                        name: NamedKey::Esc,
                    }),
                    plain(StepKey::Char { value: ':' }),
                ],
            },
        };
        assert!(!bar.take_ctrl_for(&sequence), "a sequence");
        assert!(bar.armed(), "a sequence leaves Ctrl armed");
    }

    #[test]
    fn toggle_ctrl_arms_and_disarms() {
        let mut bar = KeyBar::new();
        bar.toggle_ctrl();
        assert!(bar.armed());
        bar.toggle_ctrl();
        assert!(!bar.armed());
    }

    #[test]
    fn armed_ctrl_turns_a_typed_letter_into_its_ctrl_keystroke_once() {
        let mut bar = KeyBar::new();
        bar.toggle_ctrl();
        assert_eq!(bar.typed("c"), Some(stroke("c", true)));
        assert!(!bar.armed(), "Ctrl disarms after one keystroke");
        assert_eq!(bar.typed("c"), None);
    }

    #[test]
    fn typed_text_passes_through_when_not_armed() {
        let mut bar = KeyBar::new();
        assert_eq!(bar.typed("c"), None);
        assert_eq!(bar.typed("hello"), None);
        assert!(!bar.armed());
    }

    #[test]
    fn armed_ctrl_then_space_is_nul() {
        let mut bar = KeyBar::new();
        bar.toggle_ctrl();
        let keystroke = bar
            .typed(" ")
            .expect("Ctrl turns the space into a keystroke");
        assert_eq!(
            key_action(
                &keystroke,
                false,
                false,
                ShiftEnter::BackslashCr,
                CopyKeys::Ctrl
            ),
            Some(KeyAction::Send(vec![0x00]))
        );
    }

    #[test]
    fn armed_ctrl_then_return_is_a_return() {
        for text in ["\r", "\n"] {
            let mut bar = KeyBar::new();
            bar.toggle_ctrl();
            let keystroke = bar
                .typed(text)
                .expect("Ctrl turns the return into a keystroke");
            assert_eq!(
                key_action(
                    &keystroke,
                    false,
                    false,
                    ShiftEnter::BackslashCr,
                    CopyKeys::Ctrl
                ),
                Some(KeyAction::Send(b"\r".to_vec())),
                "{text:?}"
            );
        }
    }

    #[test]
    fn ctrl_c_from_the_bar_is_the_interrupt_byte() {
        let mut bar = KeyBar::new();
        bar.toggle_ctrl();
        let keystroke = bar
            .typed("c")
            .expect("Ctrl turns the letter into a keystroke");
        assert_eq!(
            key_action(
                &keystroke,
                false,
                false,
                ShiftEnter::BackslashCr,
                CopyKeys::Ctrl
            ),
            Some(KeyAction::Send(vec![0x03]))
        );
    }
}
