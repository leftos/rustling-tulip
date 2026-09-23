//! Keystroke → bytes the PTY expects (xterm conventions).

use gpui::Keystroke;

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

fn named_key(key: &str, shift: bool, app_cursor: bool) -> Option<Vec<u8>> {
    let arrow = |c: char| {
        let lead = if app_cursor { 'O' } else { '[' };
        format!("\x1b{lead}{c}").into_bytes()
    };
    let seq: &[u8] = match key {
        "enter" => b"\r",
        "backspace" => b"\x7f",
        "tab" if shift => b"\x1b[Z",
        "tab" => b"\t",
        "escape" => b"\x1b",
        "up" => return Some(arrow('A')),
        "down" => return Some(arrow('B')),
        "right" => return Some(arrow('C')),
        "left" => return Some(arrow('D')),
        "home" => b"\x1b[H",
        "end" => b"\x1b[F",
        "pageup" => b"\x1b[5~",
        "pagedown" => b"\x1b[6~",
        "insert" => b"\x1b[2~",
        "delete" => b"\x1b[3~",
        "f1" => b"\x1bOP",
        "f2" => b"\x1bOQ",
        "f3" => b"\x1bOR",
        "f4" => b"\x1bOS",
        "f5" => b"\x1b[15~",
        "f6" => b"\x1b[17~",
        "f7" => b"\x1b[18~",
        "f8" => b"\x1b[19~",
        "f9" => b"\x1b[20~",
        "f10" => b"\x1b[21~",
        "f11" => b"\x1b[23~",
        "f12" => b"\x1b[24~",
        _ => return None,
    };
    Some(seq.to_vec())
}
