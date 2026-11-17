//! The phone terminal's key bar as plain Rust: the keys in the order it
//! draws them, what a tap yields, and the sticky Ctrl that arms one
//! keystroke. `terminal_view` draws the bar.

use gpui::{Keystroke, Modifiers};

use super::key_grid::{GridKey, takes_sticky_ctrl};

/// The bar's keys in the order it draws them; [`shown`] adds the Hide key
/// at their end while the on-screen keyboard is up.
pub(crate) const KEYS: &[Key] = &[
    Key::Esc,
    Key::Tab,
    Key::Ctrl,
    Key::Up,
    Key::Down,
    Key::Left,
    Key::Right,
    Key::Paste,
    Key::Quick,
];

/// The keys the bar draws: [`KEYS`], and Hide after them while the on-screen
/// keyboard is up.
#[must_use]
pub(crate) fn shown(keyboard_up: bool) -> Vec<Key> {
    let mut keys = KEYS.to_vec();
    if keyboard_up {
        keys.push(Key::Hide);
    }
    keys
}

/// One key of the phone terminal's bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Key {
    /// Escape.
    Esc,
    /// Tab.
    Tab,
    /// Sticky Ctrl: arms the next keystroke.
    Ctrl,
    /// Cursor up.
    Up,
    /// Cursor down.
    Down,
    /// Cursor left.
    Left,
    /// Cursor right.
    Right,
    /// Paste from the clipboard.
    Paste,
    /// Open the Quick replies.
    Quick,
    /// Put the on-screen keyboard away.
    Hide,
}

impl Key {
    /// The key's lower-case id.
    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::Esc => "esc",
            Self::Tab => "tab",
            Self::Ctrl => "ctrl",
            Self::Up => "up",
            Self::Down => "down",
            Self::Left => "left",
            Self::Right => "right",
            Self::Paste => "paste",
            Self::Quick => "quick",
            Self::Hide => "hide",
        }
    }

    /// The name drawn on the key.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Esc => "Esc",
            Self::Tab => "Tab",
            Self::Ctrl => "Ctrl",
            Self::Up => "↑",
            Self::Down => "↓",
            Self::Left => "←",
            Self::Right => "→",
            Self::Paste => "Paste",
            Self::Quick => "Quick",
            Self::Hide => "Hide",
        }
    }
}

/// What a tap on a bar key does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Action {
    /// The keystroke to send to the terminal pane, which turns it into bytes.
    Key(Keystroke),
    /// Paste from the clipboard.
    Paste,
    /// Open the Quick replies.
    Quick,
    /// Put the on-screen keyboard away.
    Hide,
    /// Nothing: the Ctrl key only arms.
    None,
}

/// The bar's state: whether Ctrl is armed.
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

    /// A tap on `key`: Ctrl toggles arming, Paste, Quick and Hide yield their
    /// action and leave arming as it is, and the rest send their keystroke
    /// and disarm Ctrl.
    pub(crate) fn press(&mut self, key: Key) -> Action {
        match key {
            Key::Ctrl => {
                self.armed = !self.armed;
                Action::None
            }
            Key::Paste => Action::Paste,
            Key::Quick => Action::Quick,
            Key::Hide => Action::Hide,
            Key::Esc => self.terminal_key("escape"),
            Key::Tab => self.terminal_key("tab"),
            Key::Up => self.terminal_key("up"),
            Key::Down => self.terminal_key("down"),
            Key::Left => self.terminal_key("left"),
            Key::Right => self.terminal_key("right"),
        }
    }

    /// A tap on a key that sends `name` to the terminal: that keystroke, with
    /// Ctrl held while it is armed, and armed Ctrl disarms after it.
    fn terminal_key(&mut self, name: &str) -> Action {
        let stroke = stroke(name, self.armed);
        self.armed = false;
        Action::Key(stroke)
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

    use super::{Action, KEYS, Key, KeyBar, shown};

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
    fn the_bar_lists_its_keys_in_order() {
        let keys: Vec<(&str, &str)> = KEYS.iter().map(|key| (key.label(), key.id())).collect();
        assert_eq!(
            keys,
            vec![
                ("Esc", "esc"),
                ("Tab", "tab"),
                ("Ctrl", "ctrl"),
                ("↑", "up"),
                ("↓", "down"),
                ("←", "left"),
                ("→", "right"),
                ("Paste", "paste"),
                ("Quick", "quick"),
            ]
        );
    }

    #[test]
    fn the_hide_key_shows_only_while_the_keyboard_is_up() {
        assert_eq!(shown(false), KEYS);
        let up = shown(true);
        assert_eq!(up.len(), KEYS.len() + 1);
        assert_eq!(up.last(), Some(&Key::Hide), "Hide draws at the bar's end");
    }

    #[test]
    fn hide_yields_its_action_and_leaves_ctrl_armed() {
        let mut bar = KeyBar::new();
        bar.press(Key::Ctrl);
        assert_eq!(bar.press(Key::Hide), Action::Hide);
        assert!(bar.armed(), "Hide leaves Ctrl armed");
    }

    #[test]
    fn esc_tab_and_the_arrows_make_their_keystrokes() {
        let mut bar = KeyBar::new();
        for (key, name) in [
            (Key::Esc, "escape"),
            (Key::Tab, "tab"),
            (Key::Up, "up"),
            (Key::Down, "down"),
            (Key::Left, "left"),
            (Key::Right, "right"),
        ] {
            assert_eq!(
                bar.press(key),
                Action::Key(stroke(name, false)),
                "a tap on {name} with Ctrl unarmed"
            );
            assert!(!bar.armed(), "{name} leaves Ctrl unarmed");
        }
    }

    #[test]
    fn ctrl_arms_and_a_second_tap_disarms() {
        let mut bar = KeyBar::new();
        assert!(!bar.armed());
        assert_eq!(bar.press(Key::Ctrl), Action::None);
        assert!(bar.armed());
        assert_eq!(bar.press(Key::Ctrl), Action::None);
        assert!(!bar.armed());
    }

    #[test]
    fn armed_ctrl_turns_the_next_bar_key_into_a_ctrl_keystroke_once() {
        let mut bar = KeyBar::new();
        bar.press(Key::Ctrl);
        assert_eq!(bar.press(Key::Up), Action::Key(stroke("up", true)));
        assert!(!bar.armed(), "Ctrl disarms after one keystroke");
        assert_eq!(bar.press(Key::Up), Action::Key(stroke("up", false)));
    }

    #[test]
    fn armed_ctrl_turns_a_typed_letter_into_its_ctrl_keystroke_once() {
        let mut bar = KeyBar::new();
        bar.press(Key::Ctrl);
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
    fn paste_and_quick_leave_ctrl_armed() {
        let mut bar = KeyBar::new();
        bar.press(Key::Ctrl);
        assert_eq!(bar.press(Key::Paste), Action::Paste);
        assert!(bar.armed());
        assert_eq!(bar.press(Key::Quick), Action::Quick);
        assert!(bar.armed());
    }

    #[test]
    fn armed_ctrl_then_space_is_nul() {
        let mut bar = KeyBar::new();
        bar.press(Key::Ctrl);
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
            bar.press(Key::Ctrl);
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
    fn bar_keystrokes_match_parsed_keystrokes() {
        let mut bar = KeyBar::new();
        for (key, name) in [
            (Key::Esc, "escape"),
            (Key::Tab, "tab"),
            (Key::Up, "up"),
            (Key::Down, "down"),
            (Key::Left, "left"),
            (Key::Right, "right"),
        ] {
            let parsed = Keystroke::parse(name).expect("gpui parses its own key name");
            assert_eq!(bar.press(key), Action::Key(parsed), "{name} unarmed");
            bar.press(Key::Ctrl);
            let mut armed = Keystroke::parse(name).expect("gpui parses its own key name");
            armed.modifiers.control = true;
            assert_eq!(bar.press(key), Action::Key(armed), "{name} armed");
        }
    }

    #[test]
    fn ctrl_c_from_the_bar_is_the_interrupt_byte() {
        let mut bar = KeyBar::new();
        bar.press(Key::Ctrl);
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
