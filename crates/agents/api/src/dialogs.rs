//! The answer recipes a plugin hands the daemon: the keys that answer a
//! prompt its screen reader found, in groups, each with the reading the
//! daemon waits for before it types the next group.
//!
//! Keys are a closed set: there is no Esc and no ctrl key, so no recipe can
//! open Rewind or interrupt a turn. These types never cross the wire.

use protocol::ChatPromptKind;

/// One key a recipe types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    /// A digit key, 0 to 9.
    Digit(u8),
    /// Tab.
    Tab,
    /// Enter.
    Enter,
    /// The up arrow.
    Up,
    /// The down arrow.
    Down,
    /// The left arrow.
    Left,
    /// The right arrow.
    Right,
    /// Text typed as it is, without Enter.
    Text(String),
}

impl Key {
    /// The digit key for `n`, or `None` when `n` takes more than one key.
    #[must_use]
    pub fn digit(n: u32) -> Option<Self> {
        u8::try_from(n).ok().filter(|&d| d <= 9).map(Self::Digit)
    }

    /// The bytes the key writes to the PTY.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            Self::Digit(d) => vec![b'0' + d],
            Self::Tab => b"\t".to_vec(),
            Self::Enter => b"\r".to_vec(),
            Self::Up => b"\x1b[A".to_vec(),
            Self::Down => b"\x1b[B".to_vec(),
            Self::Right => b"\x1b[C".to_vec(),
            Self::Left => b"\x1b[D".to_vec(),
            Self::Text(text) => text.as_bytes().to_vec(),
        }
    }
}

/// The reading a step waits for before the recipe goes on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScreenExpect {
    /// No prompt is up any more.
    DialogGone,
    /// The same prompt is still up.
    SameDialog,
    /// A new prompt of this kind is up.
    NextPrompt(ChatPromptKind),
    /// The composer reads ready or working.
    ComposerReady,
    /// The cursor is on this option.
    OptionSelected(u32),
    /// This option is ticked (`true`) or not.
    OptionTicked(u32, bool),
    /// No check.
    Any,
}

/// One group of keys typed together, and the reading to wait for after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipeStep {
    /// The keys, in order.
    pub keys: Vec<Key>,
    /// What the screen must read once they are typed.
    pub expect: ScreenExpect,
}

/// The keys that answer one prompt, in steps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnswerRecipe {
    /// The steps, in order.
    pub steps: Vec<RecipeStep>,
}

/// Why a plugin has no recipe for an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnswerRefusal {
    /// The plugin answers no prompt of this kind.
    Unsupported,
    /// The prompt has no option with this number.
    UnknownOption(u32),
    /// This answer does not fit this prompt.
    NotThisDialog,
    /// The answer's text is empty.
    TextEmpty,
    /// The option's number takes more than one key.
    TooManyOptions,
}

#[cfg(test)]
mod tests {
    use super::Key;

    #[test]
    fn key_bytes() {
        let cases: [(Key, &[u8]); 9] = [
            (Key::Digit(0), b"0"),
            (Key::Digit(7), b"7"),
            (Key::Tab, b"\t"),
            (Key::Enter, b"\r"),
            (Key::Up, b"\x1b[A"),
            (Key::Down, b"\x1b[B"),
            (Key::Right, b"\x1b[C"),
            (Key::Left, b"\x1b[D"),
            (Key::Text("ok ✓".to_string()), "ok ✓".as_bytes()),
        ];
        for (key, bytes) in cases {
            assert_eq!(key.to_bytes(), bytes, "{key:?}");
        }
    }

    #[test]
    fn digit_refuses_ten() {
        assert_eq!(Key::digit(0), Some(Key::Digit(0)));
        assert_eq!(Key::digit(9), Some(Key::Digit(9)));
        assert_eq!(Key::digit(10), None);
        assert_eq!(Key::digit(u32::MAX), None);
    }
}
