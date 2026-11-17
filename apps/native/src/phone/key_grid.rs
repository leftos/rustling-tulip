//! The phone's key grid as plain Rust: the stored keys, the default grid,
//! and the bytes a key's presses send. The Terminal page's bar and the
//! composer's Keys dock draw it, and [`super::key_bar`] holds the sticky
//! Ctrl that a single-stroke key takes.

use gpui::{Keystroke, Modifiers};
use serde::{Deserialize, Serialize};

use crate::keys;

/// One key of the grid: what the bar and the dock draw and what a tap does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GridKey {
    /// Unique in the grid; the tail of the bar key's driver id.
    pub id: String,
    /// The name drawn on the key.
    pub label: String,
    pub action: KeyAction,
}

/// What a tap on a key does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum KeyAction {
    /// Types its presses in order.
    Send { steps: Vec<Step> },
    /// Sticky Ctrl.
    Ctrl,
    /// Paste from the clipboard.
    Paste,
    /// The Quick replies.
    Quick,
    /// A kind a later release added: drawn dimmed, does nothing.
    #[serde(other)]
    Unknown,
}

/// One press: a key with the modifiers held.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    #[serde(default)]
    pub ctrl: bool,
    #[serde(default)]
    pub alt: bool,
    #[serde(default)]
    pub shift: bool,
    pub key: StepKey,
}

/// The key one press presses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StepKey {
    Char {
        value: char,
    },
    Named {
        name: NamedKey,
    },
    /// A press key a later release added: types nothing.
    #[serde(other)]
    Unknown,
}

/// A key with a name, which sends a fixed xterm sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NamedKey {
    Esc,
    Tab,
    Enter,
    Backspace,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
    /// A named key a later release added: types nothing.
    #[serde(other)]
    Unknown,
}

impl NamedKey {
    /// The gpui key name its press builds its bytes from.
    fn key_name(self) -> Option<&'static str> {
        Some(match self {
            Self::Esc => "escape",
            Self::Tab => "tab",
            Self::Enter => "enter",
            Self::Backspace => "backspace",
            Self::Up => "up",
            Self::Down => "down",
            Self::Left => "left",
            Self::Right => "right",
            Self::Home => "home",
            Self::End => "end",
            Self::PageUp => "pageup",
            Self::PageDown => "pagedown",
            Self::F1 => "f1",
            Self::F2 => "f2",
            Self::F3 => "f3",
            Self::F4 => "f4",
            Self::F5 => "f5",
            Self::F6 => "f6",
            Self::F7 => "f7",
            Self::F8 => "f8",
            Self::F9 => "f9",
            Self::F10 => "f10",
            Self::F11 => "f11",
            Self::F12 => "f12",
            Self::Unknown => return None,
        })
    }
}

impl StepKey {
    /// Whether the key is one this build does not know.
    fn is_unknown(self) -> bool {
        match self {
            Self::Char { .. } => false,
            Self::Named { name } => name == NamedKey::Unknown,
            Self::Unknown => true,
        }
    }
}

/// What the terminal's modes say about the bytes a press sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct KeyModes {
    /// The terminal's DECCKM mode: arrows send `ESC O x` instead of `ESC [ x`.
    pub(crate) app_cursor: bool,
}

/// The grid a phone with nothing saved draws: the Terminal bar's keys in
/// their order, then the Keys tray's extras.
pub(crate) fn defaults() -> Vec<GridKey> {
    let mut keys = vec![
        named("esc", "Esc", NamedKey::Esc),
        named("tab", "Tab", NamedKey::Tab),
        GridKey {
            id: "ctrl".to_owned(),
            label: "Ctrl".to_owned(),
            action: KeyAction::Ctrl,
        },
        named("up", "↑", NamedKey::Up),
        named("down", "↓", NamedKey::Down),
        named("left", "←", NamedKey::Left),
        named("right", "→", NamedKey::Right),
        GridKey {
            id: "paste".to_owned(),
            label: "Paste".to_owned(),
            action: KeyAction::Paste,
        },
        GridKey {
            id: "quick".to_owned(),
            label: "Quick".to_owned(),
            action: KeyAction::Quick,
        },
        send(
            "shift-tab",
            "Shift+Tab",
            vec![Step {
                ctrl: false,
                alt: false,
                shift: true,
                key: StepKey::Named {
                    name: NamedKey::Tab,
                },
            }],
        ),
        named("enter", "Enter", NamedKey::Enter),
        ctrl_key("ctrl-c", "Ctrl+C", 'c'),
    ];
    for digit in '1'..='9' {
        let label = digit.to_string();
        keys.push(send(
            &label,
            &label,
            vec![plain(StepKey::Char { value: digit })],
        ));
    }
    keys
}

/// A key of one plain press of a named key.
fn named(id: &str, label: &str, name: NamedKey) -> GridKey {
    send(id, label, vec![plain(StepKey::Named { name })])
}

/// A key of one press of the character `value` with Ctrl held.
fn ctrl_key(id: &str, label: &str, value: char) -> GridKey {
    let mut step = plain(StepKey::Char { value });
    step.ctrl = true;
    send(id, label, vec![step])
}

/// A send key of `steps`.
fn send(id: &str, label: &str, steps: Vec<Step>) -> GridKey {
    GridKey {
        id: id.to_owned(),
        label: label.to_owned(),
        action: KeyAction::Send { steps },
    }
}

/// A press of `key` with no modifier held.
fn plain(key: StepKey) -> Step {
    Step {
        ctrl: false,
        alt: false,
        shift: false,
        key,
    }
}

/// The bytes `step` sends, or `None` when it types nothing.
pub(crate) fn step_bytes(step: &Step, modes: KeyModes) -> Option<Vec<u8>> {
    match step.key {
        StepKey::Char { value } => char_bytes(step, value),
        StepKey::Named { name } => name_bytes(step, name, modes.app_cursor),
        StepKey::Unknown => None,
    }
}

/// A character press's bytes: the character itself, its upper case with
/// Shift, the control byte of its Ctrl keystroke (Shift ignored), and an
/// `ESC` prefix with Alt.
fn char_bytes(step: &Step, value: char) -> Option<Vec<u8>> {
    let mut bytes = if step.ctrl {
        let key = if value == ' ' {
            "space".to_owned()
        } else {
            value.to_string()
        };
        let keystroke = Keystroke {
            key,
            key_char: None,
            modifiers: Modifiers {
                control: true,
                ..Modifiers::default()
            },
        };
        keys::to_bytes(&keystroke, false)?
    } else if step.shift {
        upper_case(value)?
    } else {
        value.to_string().into_bytes()
    };
    if step.alt {
        bytes.insert(0, 0x1b);
    }
    Some(bytes)
}

/// `value`'s upper case, or `None` when it has none (a digit, a symbol, a
/// character whose upper case is more than one character).
fn upper_case(value: char) -> Option<Vec<u8>> {
    if !value.is_lowercase() && !value.is_uppercase() {
        return None;
    }
    let mut upper = value.to_uppercase();
    let first = upper.next()?;
    if upper.next().is_some() {
        return None;
    }
    Some(first.to_string().into_bytes())
}

/// A named press's bytes, as `keys::to_bytes` gives its keystroke.
fn name_bytes(step: &Step, name: NamedKey, app_cursor: bool) -> Option<Vec<u8>> {
    let keystroke = Keystroke {
        key: name.key_name()?.to_owned(),
        key_char: None,
        modifiers: Modifiers {
            control: step.ctrl,
            alt: step.alt,
            shift: step.shift,
            ..Modifiers::default()
        },
    };
    keys::to_bytes(&keystroke, app_cursor)
}

/// The bytes `steps` send in order, or `None` when one of them types
/// nothing. Armed Ctrl joins a key of one plain press.
pub(crate) fn key_bytes(steps: &[Step], sticky_ctrl: bool, modes: KeyModes) -> Option<Vec<u8>> {
    if steps.is_empty() {
        return None;
    }
    let sticky = sticky_ctrl && single_plain_press(steps);
    let mut bytes = Vec::new();
    for step in steps {
        let mut step = *step;
        if sticky {
            step.ctrl = true;
        }
        bytes.extend(step_bytes(&step, modes)?);
    }
    Some(bytes)
}

/// Whether armed Ctrl applies to `key`: a send key of one press with no
/// modifier of its own.
pub(crate) fn takes_sticky_ctrl(key: &GridKey) -> bool {
    match &key.action {
        KeyAction::Send { steps } => single_plain_press(steps),
        KeyAction::Ctrl | KeyAction::Paste | KeyAction::Quick | KeyAction::Unknown => false,
    }
}

/// Whether `steps` is one press of a key this build knows with no modifier
/// held.
fn single_plain_press(steps: &[Step]) -> bool {
    match steps {
        [step] => !step.ctrl && !step.alt && !step.shift && !step.key.is_unknown(),
        _ => false,
    }
}

/// Whether `key` cannot be removed: Ctrl, Paste or Quick.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the specs check the protected keys")
)]
pub(crate) fn is_protected(key: &GridKey) -> bool {
    matches!(
        key.action,
        KeyAction::Ctrl | KeyAction::Paste | KeyAction::Quick
    )
}

/// Whether `key` draws dimmed and does nothing: an action or a press key a
/// later release added.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the specs check the inert keys")
)]
pub(crate) fn is_inert(key: &GridKey) -> bool {
    match &key.action {
        KeyAction::Send { steps } => steps.iter().any(|step| step.key.is_unknown()),
        KeyAction::Unknown => true,
        KeyAction::Ctrl | KeyAction::Paste | KeyAction::Quick => false,
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a named test fixture that is missing should fail with its message"
)]
mod tests {
    use super::*;

    /// The step a plain press of `key` is.
    fn plain_step(key: StepKey) -> Step {
        Step {
            ctrl: false,
            alt: false,
            shift: false,
            key,
        }
    }

    /// The step a press of `key` with the three modifiers held is.
    fn held_step(ctrl: bool, alt: bool, shift: bool, key: StepKey) -> Step {
        Step {
            ctrl,
            alt,
            shift,
            key,
        }
    }

    fn named_step(name: NamedKey) -> StepKey {
        StepKey::Named { name }
    }

    fn char_step(value: char) -> StepKey {
        StepKey::Char { value }
    }

    fn modes(app_cursor: bool) -> KeyModes {
        KeyModes { app_cursor }
    }

    fn off() -> KeyModes {
        modes(false)
    }

    /// `pairs` as owned rows, for comparing against decoded keys.
    fn rows(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(id, label)| ((*id).to_owned(), (*label).to_owned()))
            .collect()
    }

    /// The default grid's send keys' bytes, for comparing against `pairs`.
    fn sent() -> Vec<(String, Option<Vec<u8>>)> {
        defaults()
            .into_iter()
            .filter_map(|key| match key.action {
                KeyAction::Send { steps } => Some((key.id, key_bytes(&steps, false, off()))),
                KeyAction::Ctrl | KeyAction::Paste | KeyAction::Quick | KeyAction::Unknown => None,
            })
            .collect()
    }

    /// `pairs` as the rows [`sent`] gives, for comparing against it.
    fn sends(pairs: &[(&str, &[u8])]) -> Vec<(String, Option<Vec<u8>>)> {
        pairs
            .iter()
            .map(|(id, bytes)| ((*id).to_owned(), Some((*bytes).to_vec())))
            .collect()
    }

    /// The default grid's key with `id`.
    fn key_of(id: &str) -> GridKey {
        defaults()
            .into_iter()
            .find(|key| key.id == id)
            .expect("the default grid holds the key")
    }

    /// A grid key of one press.
    fn key_of_step(id: &str, step: Step) -> GridKey {
        GridKey {
            id: id.to_owned(),
            label: id.to_owned(),
            action: KeyAction::Send { steps: vec![step] },
        }
    }

    /// A grid key whose action this build does not know.
    fn unknown_key() -> GridKey {
        GridKey {
            id: "macro".to_owned(),
            label: "Macro".to_owned(),
            action: KeyAction::Unknown,
        }
    }

    #[test]
    fn the_default_grid_is_the_bar_then_the_trays_extras() {
        let keys: Vec<(String, String)> = defaults()
            .into_iter()
            .map(|key| (key.id, key.label))
            .collect();
        assert_eq!(
            keys,
            rows(&[
                ("esc", "Esc"),
                ("tab", "Tab"),
                ("ctrl", "Ctrl"),
                ("up", "↑"),
                ("down", "↓"),
                ("left", "←"),
                ("right", "→"),
                ("paste", "Paste"),
                ("quick", "Quick"),
                ("shift-tab", "Shift+Tab"),
                ("enter", "Enter"),
                ("ctrl-c", "Ctrl+C"),
                ("1", "1"),
                ("2", "2"),
                ("3", "3"),
                ("4", "4"),
                ("5", "5"),
                ("6", "6"),
                ("7", "7"),
                ("8", "8"),
                ("9", "9"),
            ])
        );
    }

    #[test]
    fn the_default_grid_sends_the_old_tray_bytes() {
        assert_eq!(
            sent(),
            sends(&[
                ("esc", b"\x1b"),
                ("tab", b"\t"),
                ("up", b"\x1b[A"),
                ("down", b"\x1b[B"),
                ("left", b"\x1b[D"),
                ("right", b"\x1b[C"),
                ("shift-tab", b"\x1b[Z"),
                ("enter", b"\r"),
                ("ctrl-c", b"\x03"),
                ("1", b"1"),
                ("2", b"2"),
                ("3", b"3"),
                ("4", b"4"),
                ("5", b"5"),
                ("6", b"6"),
                ("7", b"7"),
                ("8", b"8"),
                ("9", b"9"),
            ])
        );
    }

    #[test]
    fn step_bytes_named_keys() {
        for (name, bytes) in [
            (NamedKey::Esc, &b"\x1b"[..]),
            (NamedKey::Tab, b"\t"),
            (NamedKey::Enter, b"\r"),
            (NamedKey::Backspace, b"\x7f"),
        ] {
            assert_eq!(
                step_bytes(&plain_step(named_step(name)), off()).as_deref(),
                Some(bytes),
                "{name:?}"
            );
        }
    }

    #[test]
    fn step_bytes_ctrl_tab_is_tab() {
        assert_eq!(
            step_bytes(
                &held_step(true, false, false, named_step(NamedKey::Tab)),
                off()
            )
            .as_deref(),
            Some(&b"\t"[..]),
            "to_bytes leaves Tab plain under Ctrl"
        );
    }

    #[test]
    fn step_bytes_shift_tab() {
        assert_eq!(
            step_bytes(
                &held_step(false, false, true, named_step(NamedKey::Tab)),
                off()
            )
            .as_deref(),
            Some(&b"\x1b[Z"[..])
        );
    }

    #[test]
    fn step_bytes_ctrl_backspace() {
        assert_eq!(
            step_bytes(
                &held_step(true, false, false, named_step(NamedKey::Backspace)),
                off()
            )
            .as_deref(),
            Some(&b"\x08"[..])
        );
    }

    #[test]
    fn step_bytes_up_honours_application_cursor_mode() {
        let up = plain_step(named_step(NamedKey::Up));
        assert_eq!(step_bytes(&up, off()).as_deref(), Some(&b"\x1b[A"[..]));
        assert_eq!(
            step_bytes(&up, modes(true)).as_deref(),
            Some(&b"\x1bOA"[..]),
            "DECCKM sends SS3"
        );
    }

    #[test]
    fn step_bytes_modified_arrow_is_always_csi() {
        let step = held_step(true, false, false, named_step(NamedKey::Up));
        for app_cursor in [false, true] {
            assert_eq!(
                step_bytes(&step, modes(app_cursor)).as_deref(),
                Some(&b"\x1b[1;5A"[..]),
                "app_cursor {app_cursor}"
            );
        }
    }

    #[test]
    fn step_bytes_alt_left() {
        assert_eq!(
            step_bytes(
                &held_step(false, true, false, named_step(NamedKey::Left)),
                off()
            )
            .as_deref(),
            Some(&b"\x1b[1;3D"[..])
        );
    }

    #[test]
    fn step_bytes_navigation_and_function_keys() {
        for (name, bytes) in [
            (NamedKey::Home, &b"\x1b[H"[..]),
            (NamedKey::PageUp, b"\x1b[5~"),
            (NamedKey::F1, b"\x1bOP"),
            (NamedKey::F5, b"\x1b[15~"),
        ] {
            assert_eq!(
                step_bytes(&plain_step(named_step(name)), off()).as_deref(),
                Some(bytes),
                "{name:?}"
            );
        }
    }

    #[test]
    fn step_bytes_character_and_its_upper_case() {
        assert_eq!(
            step_bytes(&plain_step(char_step('b')), off()).as_deref(),
            Some(&b"b"[..])
        );
        assert_eq!(
            step_bytes(&held_step(false, false, true, char_step('b')), off()).as_deref(),
            Some(&b"B"[..])
        );
    }

    #[test]
    fn step_bytes_ctrl_letters_are_control_bytes() {
        for value in ['b', 'c', 'v'] {
            let expected = match value {
                'b' => &b"\x02"[..],
                'c' => b"\x03",
                _ => b"\x16",
            };
            assert_eq!(
                step_bytes(&held_step(true, false, false, char_step(value)), off()).as_deref(),
                Some(expected),
                "Ctrl+{value}"
            );
        }
        assert_eq!(
            step_bytes(&held_step(true, false, true, char_step('c')), off()).as_deref(),
            Some(&b"\x03"[..]),
            "Shift is ignored under Ctrl"
        );
    }

    #[test]
    fn step_bytes_ctrl_space_is_nul() {
        for value in [' ', '2'] {
            assert_eq!(
                step_bytes(&held_step(true, false, false, char_step(value)), off()).as_deref(),
                Some(&b"\x00"[..]),
                "Ctrl+{value:?}"
            );
        }
    }

    #[test]
    fn step_bytes_alt_prefixes_escape() {
        assert_eq!(
            step_bytes(&held_step(false, true, false, char_step('b')), off()).as_deref(),
            Some(&b"\x1bb"[..])
        );
        assert_eq!(
            step_bytes(&held_step(true, true, false, char_step('b')), off()).as_deref(),
            Some(&b"\x1b\x02"[..]),
            "Ctrl+Alt+B is ESC then the control byte"
        );
    }

    #[test]
    fn step_bytes_a_press_that_types_nothing() {
        assert_eq!(
            step_bytes(&held_step(true, false, false, char_step('1')), off()),
            None,
            "Ctrl+1 has no control byte"
        );
        assert_eq!(
            step_bytes(&held_step(false, false, true, char_step('1')), off()),
            None,
            "1 has no upper case"
        );
    }

    #[test]
    fn step_bytes_utf8_character() {
        assert_eq!(
            step_bytes(&plain_step(char_step('é')), off()).as_deref(),
            Some(&b"\xc3\xa9"[..])
        );
    }

    #[test]
    fn step_bytes_unknown_key_types_nothing() {
        assert_eq!(step_bytes(&plain_step(StepKey::Unknown), off()), None);
        assert_eq!(
            step_bytes(&plain_step(named_step(NamedKey::Unknown)), off()),
            None
        );
    }

    #[test]
    fn key_bytes_sends_each_press_in_order() {
        let steps = [
            plain_step(named_step(NamedKey::Esc)),
            plain_step(char_step(':')),
            plain_step(char_step('w')),
            plain_step(named_step(NamedKey::Enter)),
        ];
        assert_eq!(
            key_bytes(&steps, false, off()).as_deref(),
            Some(&b"\x1b:w\r"[..])
        );
    }

    #[test]
    fn key_bytes_one_silent_press_silences_the_key() {
        let steps = [
            plain_step(named_step(NamedKey::Esc)),
            plain_step(StepKey::Unknown),
        ];
        assert_eq!(key_bytes(&steps, false, off()), None);
    }

    #[test]
    fn key_bytes_no_presses_types_nothing() {
        assert_eq!(key_bytes(&[], false, off()), None);
    }

    #[test]
    fn key_bytes_sticky_ctrl_joins_a_single_press() {
        let steps = [plain_step(named_step(NamedKey::Up))];
        assert_eq!(
            key_bytes(&steps, true, off()).as_deref(),
            Some(&b"\x1b[1;5A"[..])
        );
    }

    #[test]
    fn key_bytes_sticky_ctrl_on_a_letter_is_its_control_byte() {
        let steps = [plain_step(char_step('c'))];
        assert_eq!(
            key_bytes(&steps, true, off()).as_deref(),
            Some(&b"\x03"[..])
        );
    }

    #[test]
    fn key_bytes_sticky_ctrl_on_esc_is_esc() {
        let steps = [plain_step(named_step(NamedKey::Esc))];
        assert_eq!(
            key_bytes(&steps, true, off()).as_deref(),
            Some(&b"\x1b"[..])
        );
    }

    #[test]
    fn a_single_plain_press_takes_sticky_ctrl() {
        assert!(takes_sticky_ctrl(&key_of("esc")), "Esc");
        assert!(
            takes_sticky_ctrl(&key_of_step("one", plain_step(char_step('1')))),
            "a character alone"
        );
    }

    #[test]
    fn a_chord_or_sequence_ignores_sticky_ctrl() {
        assert!(!takes_sticky_ctrl(&key_of("shift-tab")), "Shift+Tab");
        assert!(!takes_sticky_ctrl(&key_of("ctrl-c")), "Ctrl+C");
        let sequence = GridKey {
            id: "seq".to_owned(),
            label: "Esc Esc".to_owned(),
            action: KeyAction::Send {
                steps: vec![
                    plain_step(named_step(NamedKey::Esc)),
                    plain_step(named_step(NamedKey::Esc)),
                ],
            },
        };
        assert!(!takes_sticky_ctrl(&sequence), "a sequence");
    }

    #[test]
    fn action_keys_ignore_sticky_ctrl() {
        for id in ["ctrl", "paste", "quick"] {
            assert!(!takes_sticky_ctrl(&key_of(id)), "{id}");
        }
        assert!(!takes_sticky_ctrl(&unknown_key()), "an Unknown action");
    }

    #[test]
    fn ctrl_paste_and_quick_are_protected() {
        for id in ["ctrl", "paste", "quick"] {
            assert!(is_protected(&key_of(id)), "{id}");
        }
        assert!(!is_protected(&key_of("esc")), "a send key");
        assert!(!is_protected(&unknown_key()), "an Unknown action");
    }

    #[test]
    fn unknown_keys_are_inert() {
        assert!(is_inert(&unknown_key()), "an Unknown action");
        let unknown_press = key_of_step("press", held_step(false, false, false, StepKey::Unknown));
        assert!(is_inert(&unknown_press), "a press with an Unknown key");
        assert!(!is_inert(&key_of("esc")), "Esc");
    }
}
