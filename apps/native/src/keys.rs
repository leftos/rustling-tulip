//! Keystroke → bytes the PTY expects (xterm conventions).

use gpui::Keystroke;

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

/// Returns the bytes to send for `ks`, or `None` when the key produces no input.
/// `app_cursor` is the terminal's DECCKM mode: arrows send `ESC O x` instead of `ESC [ x`.
pub fn to_bytes(ks: &Keystroke, app_cursor: bool) -> Option<Vec<u8>> {
    let m = ks.modifiers;
    if let Some(bytes) = named_key(ks.key.as_str(), m.shift, app_cursor) {
        return Some(prefix_alt(m.alt, bytes));
    }
    if m.control && !m.alt {
        return control_byte(ks.key.as_str()).map(|b| vec![b]);
    }
    let text = ks.key_char.as_deref()?;
    Some(prefix_alt(m.alt, text.as_bytes().to_vec()))
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
    fn alt_prefixes_escape() {
        assert_eq!(
            bytes(&with(typed("x", "x"), alt())),
            Some(b"\x1bx".to_vec())
        );
        assert_eq!(bytes(&with(key("enter"), alt())), Some(b"\x1b\r".to_vec()));
    }

    #[test]
    fn plain_char_passes_through() {
        assert_eq!(bytes(&typed("a", "a")), Some(b"a".to_vec()));
        assert_eq!(bytes(&typed("e", "é")), Some("é".as_bytes().to_vec()));
    }

    #[test]
    fn unmapped_key_yields_nothing() {
        assert_eq!(bytes(&key("f13")), None);
    }
}
