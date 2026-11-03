//! Keystroke → bytes the PTY expects (xterm conventions), and the app keys
//! that switch the focused pane's view.

use gpui::{Keystroke, Modifiers};

use crate::chat::ViewKey;

/// The view key `ks` is: Ctrl+Shift+M, which switches the focused pane
/// between its terminal and its chat, or Ctrl+Shift+U, which opens its
/// summary (and leaves it for the terminal). Both are taken before the
/// terminal sees them, so neither reaches the PTY as the Enter Ctrl+M or the
/// line kill Ctrl+U sends.
pub fn pane_view_key(ks: &Keystroke) -> Option<ViewKey> {
    let m = ks.modifiers;
    if !m.control || !m.shift || m.alt || m.platform {
        return None;
    }
    if ks.key.eq_ignore_ascii_case("m") {
        Some(ViewKey::Chat)
    } else if ks.key.eq_ignore_ascii_case("u") {
        Some(ViewKey::Summary)
    } else {
        None
    }
}

/// Keys whose xterm sequence takes a modifier parameter: the `CSI 1 ; m`
/// forms end in their final byte, the `CSI n ; m ~` forms in `~`.
const MODIFIABLE_KEYS: &[(&str, &str, char)] = &[
    ("home", "1", 'H'),
    ("end", "1", 'F'),
    ("f1", "1", 'P'),
    ("f2", "1", 'Q'),
    ("f3", "1", 'R'),
    ("f4", "1", 'S'),
    ("insert", "2", '~'),
    ("delete", "3", '~'),
    ("pageup", "5", '~'),
    ("pagedown", "6", '~'),
    ("f5", "15", '~'),
    ("f6", "17", '~'),
    ("f7", "18", '~'),
    ("f8", "19", '~'),
    ("f9", "20", '~'),
    ("f10", "21", '~'),
    ("f11", "23", '~'),
    ("f12", "24", '~'),
];

/// Keys that send a fixed sequence regardless of terminal mode.
const FIXED_KEYS: &[(&str, &[u8])] = &[
    ("enter", b"\r"),
    ("backspace", b"\x7f"),
    ("tab", b"\t"),
    ("escape", b"\x1b"),
    ("home", b"\x1b[H"),
    ("end", b"\x1b[F"),
    ("pageup", b"\x1b[5~"),
    ("pagedown", b"\x1b[6~"),
    ("insert", b"\x1b[2~"),
    ("delete", b"\x1b[3~"),
    ("f1", b"\x1bOP"),
    ("f2", b"\x1bOQ"),
    ("f3", b"\x1bOR"),
    ("f4", b"\x1bOS"),
    ("f5", b"\x1b[15~"),
    ("f6", b"\x1b[17~"),
    ("f7", b"\x1b[18~"),
    ("f8", b"\x1b[19~"),
    ("f9", b"\x1b[20~"),
    ("f10", b"\x1b[21~"),
    ("f11", b"\x1b[23~"),
    ("f12", b"\x1b[24~"),
];

/// Returns the bytes to send for `ks`, or `None` when the key itself sends
/// nothing. A text key with neither Ctrl nor Alt is `None`: its text arrives
/// through the window's text input, which composes dead keys and IME text.
/// Ctrl+Alt with a character other than an ASCII letter or digit is `AltGr`
/// on a layout that types a symbol there, and sends that symbol.
/// `app_cursor` is the terminal's DECCKM mode: arrows send `ESC O x` instead of `ESC [ x`.
pub fn to_bytes(ks: &Keystroke, app_cursor: bool) -> Option<Vec<u8>> {
    let m = ks.modifiers;
    if let Some(bytes) = modified_key(ks.key.as_str(), m) {
        return Some(bytes);
    }
    // Ctrl+Backspace sends ^H, as Windows Terminal does: shells and agent
    // CLIs read it as deleting the previous word.
    if ks.key == "backspace" && m.control {
        return Some(prefix_alt(m.alt, vec![0x08]));
    }
    if let Some(bytes) = named_key(ks.key.as_str(), m.shift, app_cursor) {
        return Some(prefix_alt(m.alt, bytes));
    }
    if m.control && !m.alt {
        return control_byte(ks.key.as_str()).map(|b| vec![b]);
    }
    if ks.key == "space" {
        // Alt+Space belongs to the window's system menu, so it stays
        // unconsumed whichever `key_char` shape the platform sends.
        return if m.alt { None } else { Some(b" ".to_vec()) };
    }
    let text = ks.key_char.as_deref()?;
    if !m.alt {
        return None;
    }
    if m.control && !matches!(text.as_bytes(), [b] if b.is_ascii_alphanumeric()) {
        return Some(text.as_bytes().to_vec());
    }
    Some(prefix_alt(true, text.as_bytes().to_vec()))
}

fn prefix_alt(alt: bool, mut bytes: Vec<u8>) -> Vec<u8> {
    if alt {
        bytes.insert(0, 0x1b);
    }
    bytes
}

fn control_byte(key: &str) -> Option<u8> {
    match key {
        "space" | "@" | "2" => Some(0x00),
        "[" => Some(0x1b),
        "\\" => Some(0x1c),
        "]" => Some(0x1d),
        "/" => Some(0x1f),
        k if k.len() == 1 => {
            let c = k.as_bytes()[0].to_ascii_lowercase();
            c.is_ascii_lowercase().then(|| c - b'a' + 1)
        }
        _ => None,
    }
}

fn arrow_letter(key: &str) -> Option<char> {
    match key {
        "up" => Some('A'),
        "down" => Some('B'),
        "right" => Some('C'),
        "left" => Some('D'),
        _ => None,
    }
}

/// xterm's modifier parameter: 1, plus 1 for Shift, 2 for Alt and 4 for
/// Ctrl. `None` when none of the three is held.
fn modifier_parameter(m: Modifiers) -> Option<u8> {
    let value = u8::from(m.shift) + 2 * u8::from(m.alt) + 4 * u8::from(m.control);
    (value > 0).then_some(value + 1)
}

/// The sequence an arrow, Home/End, Insert/Delete, PageUp/PageDown or a
/// function key sends with Shift, Alt or Ctrl held, as xterm and Windows
/// Terminal send it: `CSI 1 ; m X`, or `CSI n ; m ~`. Ctrl+Left is
/// `ESC [ 1 ; 5 D`, which programs read as a word left. These forms are
/// always CSI, whatever the cursor-key mode.
fn modified_key(key: &str, m: Modifiers) -> Option<Vec<u8>> {
    let param = modifier_parameter(m)?;
    let (number, last) = match arrow_letter(key) {
        Some(letter) => ("1", letter),
        None => MODIFIABLE_KEYS
            .iter()
            .find(|(name, _, _)| *name == key)
            .map(|(_, number, last)| (*number, *last))?,
    };
    Some(format!("\x1b[{number};{param}{last}").into_bytes())
}

fn named_key(key: &str, shift: bool, app_cursor: bool) -> Option<Vec<u8>> {
    if key == "tab" && shift {
        return Some(b"\x1b[Z".to_vec());
    }
    if let Some(letter) = arrow_letter(key) {
        let lead = if app_cursor { 'O' } else { '[' };
        return Some(format!("\x1b{lead}{letter}").into_bytes());
    }
    FIXED_KEYS
        .iter()
        .find(|(name, _)| *name == key)
        .map(|(_, seq)| seq.to_vec())
}

#[cfg(test)]
mod tests {
    use gpui::Modifiers;

    use super::to_bytes;

    fn key(name: &str) -> gpui::Keystroke {
        gpui::Keystroke {
            key: name.to_owned(),
            ..Default::default()
        }
    }

    fn typed(name: &str, text: &str) -> gpui::Keystroke {
        gpui::Keystroke {
            key: name.to_owned(),
            key_char: Some(text.to_owned()),
            ..Default::default()
        }
    }

    fn with(mut ks: gpui::Keystroke, modifiers: Modifiers) -> gpui::Keystroke {
        ks.modifiers = modifiers;
        ks
    }

    fn ctrl() -> Modifiers {
        Modifiers {
            control: true,
            ..Default::default()
        }
    }

    fn alt() -> Modifiers {
        Modifiers {
            alt: true,
            ..Default::default()
        }
    }

    fn shift() -> Modifiers {
        Modifiers {
            shift: true,
            ..Default::default()
        }
    }

    fn bytes(ks: &gpui::Keystroke) -> Option<Vec<u8>> {
        to_bytes(ks, false)
    }

    #[test]
    fn enter_sends_cr() {
        assert_eq!(bytes(&key("enter")), Some(b"\r".to_vec()));
    }

    #[test]
    fn backspace_sends_del() {
        assert_eq!(bytes(&key("backspace")), Some(b"\x7f".to_vec()));
    }

    #[test]
    fn tab_sends_ht_and_shift_tab_sends_back_tab() {
        assert_eq!(bytes(&key("tab")), Some(b"\t".to_vec()));
        assert_eq!(bytes(&with(key("tab"), shift())), Some(b"\x1b[Z".to_vec()));
    }

    #[test]
    fn escape_sends_esc() {
        assert_eq!(bytes(&key("escape")), Some(b"\x1b".to_vec()));
    }

    #[test]
    fn arrow_keys_honour_app_cursor_mode() {
        let cases = [
            ("up", b'A'),
            ("down", b'B'),
            ("right", b'C'),
            ("left", b'D'),
        ];
        for (name, letter) in cases {
            assert_eq!(
                to_bytes(&key(name), false),
                Some(vec![0x1b, b'[', letter]),
                "{name} normal"
            );
            assert_eq!(
                to_bytes(&key(name), true),
                Some(vec![0x1b, b'O', letter]),
                "{name} app cursor"
            );
        }
    }

    fn mods(shift: bool, alt: bool, control: bool) -> Modifiers {
        Modifiers {
            shift,
            alt,
            control,
            ..Default::default()
        }
    }

    #[test]
    fn ctrl_arrows_send_the_modifier_parameter_in_either_cursor_mode() {
        for app_cursor in [false, true] {
            assert_eq!(
                to_bytes(&with(key("left"), ctrl()), app_cursor),
                Some(b"\x1b[1;5D".to_vec()),
                "ctrl+left, app cursor {app_cursor}"
            );
            assert_eq!(
                to_bytes(&with(key("right"), ctrl()), app_cursor),
                Some(b"\x1b[1;5C".to_vec()),
                "ctrl+right, app cursor {app_cursor}"
            );
        }
    }

    #[test]
    fn modifier_parameter_counts_shift_alt_and_ctrl() {
        let cases: [(Modifiers, &[u8]); 7] = [
            (mods(true, false, false), b"\x1b[1;2A"),
            (mods(false, true, false), b"\x1b[1;3A"),
            (mods(true, true, false), b"\x1b[1;4A"),
            (mods(false, false, true), b"\x1b[1;5A"),
            (mods(true, false, true), b"\x1b[1;6A"),
            (mods(false, true, true), b"\x1b[1;7A"),
            (mods(true, true, true), b"\x1b[1;8A"),
        ];
        for (modifiers, seq) in cases {
            assert_eq!(
                bytes(&with(key("up"), modifiers)),
                Some(seq.to_vec()),
                "{modifiers:?}"
            );
        }
    }

    #[test]
    fn modified_navigation_and_function_keys_send_the_parameter() {
        let cases: [(&str, Modifiers, &[u8]); 8] = [
            ("home", ctrl(), b"\x1b[1;5H"),
            ("end", shift(), b"\x1b[1;2F"),
            ("delete", ctrl(), b"\x1b[3;5~"),
            ("insert", alt(), b"\x1b[2;3~"),
            ("pageup", shift(), b"\x1b[5;2~"),
            ("pagedown", ctrl(), b"\x1b[6;5~"),
            ("f1", shift(), b"\x1b[1;2P"),
            ("f12", ctrl(), b"\x1b[24;5~"),
        ];
        for (name, modifiers, seq) in cases {
            assert_eq!(
                bytes(&with(key(name), modifiers)),
                Some(seq.to_vec()),
                "{name} {modifiers:?}"
            );
        }
    }

    #[test]
    fn keys_without_a_modifier_form_keep_their_bytes() {
        assert_eq!(bytes(&with(key("tab"), shift())), Some(b"\x1b[Z".to_vec()));
        assert_eq!(bytes(&with(key("tab"), ctrl())), Some(b"\t".to_vec()));
        assert_eq!(
            bytes(&with(key("backspace"), alt())),
            Some(b"\x1b\x7f".to_vec())
        );
        assert_eq!(bytes(&with(key("enter"), ctrl())), Some(b"\r".to_vec()));
        assert_eq!(
            bytes(&with(key("escape"), alt())),
            Some(b"\x1b\x1b".to_vec())
        );
    }

    #[test]
    fn ctrl_backspace_sends_a_word_delete() {
        assert_eq!(bytes(&with(key("backspace"), ctrl())), Some(vec![0x08]));
        assert_eq!(bytes(&key("backspace")), Some(vec![0x7f]));
        let ctrl_alt = Modifiers {
            control: true,
            alt: true,
            ..Modifiers::default()
        };
        assert_eq!(
            bytes(&with(key("backspace"), ctrl_alt)),
            Some(b"\x1b\x08".to_vec())
        );
    }

    #[test]
    fn navigation_keys_send_csi_sequences() {
        let cases: [(&str, &[u8]); 6] = [
            ("home", b"\x1b[H"),
            ("end", b"\x1b[F"),
            ("pageup", b"\x1b[5~"),
            ("pagedown", b"\x1b[6~"),
            ("insert", b"\x1b[2~"),
            ("delete", b"\x1b[3~"),
        ];
        for (name, seq) in cases {
            assert_eq!(bytes(&key(name)), Some(seq.to_vec()), "{name}");
        }
    }

    #[test]
    fn function_keys_send_xterm_sequences() {
        let cases: [(&str, &[u8]); 12] = [
            ("f1", b"\x1bOP"),
            ("f2", b"\x1bOQ"),
            ("f3", b"\x1bOR"),
            ("f4", b"\x1bOS"),
            ("f5", b"\x1b[15~"),
            ("f6", b"\x1b[17~"),
            ("f7", b"\x1b[18~"),
            ("f8", b"\x1b[19~"),
            ("f9", b"\x1b[20~"),
            ("f10", b"\x1b[21~"),
            ("f11", b"\x1b[23~"),
            ("f12", b"\x1b[24~"),
        ];
        for (name, seq) in cases {
            assert_eq!(bytes(&key(name)), Some(seq.to_vec()), "{name}");
        }
    }

    #[test]
    fn ctrl_letter_sends_control_byte() {
        assert_eq!(bytes(&with(typed("c", "c"), ctrl())), Some(vec![0x03]));
        assert_eq!(bytes(&with(typed("a", "a"), ctrl())), Some(vec![0x01]));
        assert_eq!(bytes(&with(typed("Z", "Z"), ctrl())), Some(vec![0x1a]));
    }

    #[test]
    fn ctrl_punctuation_sends_control_byte() {
        let cases = [
            ("space", 0x00),
            ("@", 0x00),
            ("2", 0x00),
            ("[", 0x1b),
            ("\\", 0x1c),
            ("]", 0x1d),
            ("/", 0x1f),
        ];
        for (name, byte) in cases {
            assert_eq!(
                bytes(&with(key(name), ctrl())),
                Some(vec![byte]),
                "ctrl+{name}"
            );
        }
    }

    #[test]
    fn ctrl_with_unmapped_key_yields_nothing() {
        assert_eq!(bytes(&with(typed("1", "1"), ctrl())), None);
        assert_eq!(bytes(&with(key("f13"), ctrl())), None);
    }

    #[test]
    fn space_without_key_char_sends_space() {
        assert_eq!(bytes(&key("space")), Some(b" ".to_vec()));
        assert_eq!(bytes(&with(key("space"), shift())), Some(b" ".to_vec()));
    }

    #[test]
    fn ctrl_space_still_sends_nul() {
        assert_eq!(bytes(&with(key("space"), ctrl())), Some(vec![0x00]));
    }

    #[test]
    fn alt_space_is_left_to_the_system_menu() {
        let ctrl_alt = Modifiers {
            control: true,
            alt: true,
            ..Default::default()
        };
        assert_eq!(bytes(&with(key("space"), alt())), None);
        assert_eq!(bytes(&with(typed("space", " "), alt())), None);
        assert_eq!(bytes(&with(key("space"), ctrl_alt)), None);
    }

    #[test]
    fn alt_prefixes_escape() {
        assert_eq!(
            bytes(&with(typed("x", "x"), alt())),
            Some(b"\x1bx".to_vec())
        );
        assert_eq!(bytes(&with(key("enter"), alt())), Some(b"\x1b\r".to_vec()));
    }

    #[test]
    fn plain_char_is_left_to_text_input() {
        assert_eq!(bytes(&typed("a", "a")), None);
        assert_eq!(bytes(&typed("e", "é")), None);
        assert_eq!(bytes(&with(typed("a", "A"), shift())), None);
    }

    #[test]
    fn unmapped_key_yields_nothing() {
        assert_eq!(bytes(&key("f13")), None);
    }
}
