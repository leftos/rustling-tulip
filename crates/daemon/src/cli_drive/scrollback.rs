//! Raw PTY bytes as readable text, for `read --scrollback`. Programs lay text
//! out with cursor moves, so the moves that stand for a gap or a new row turn
//! into spaces and line breaks; everything else that is not text goes.

/// The most spaces one cursor-forward sequence prints.
const MAX_FORWARD: usize = 512;

const ESC: u8 = 0x1B;
const BEL: u8 = 0x07;

/// `bytes` as text: `ESC[nC` is n spaces; `ESC[r;cH`, `ESC[r;cf` and `ESC[nE`
/// are one line break; `\r\n` and a lone `\r` are `\n`; every other escape
/// sequence is dropped. What remains decodes as lossy UTF-8.
pub fn plain_text(bytes: &[u8]) -> String {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&byte) = bytes.get(i) {
        match byte {
            ESC => i = skip_escape(bytes, i + 1, &mut out),
            b'\r' => {
                out.push(b'\n');
                i += if bytes.get(i + 1) == Some(&b'\n') {
                    2
                } else {
                    1
                };
            }
            _ => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Handle the escape sequence whose ESC sits just before `start`, answering
/// where the text after it begins. A sequence cut off by the end of `bytes`
/// is dropped.
fn skip_escape(bytes: &[u8], start: usize, out: &mut Vec<u8>) -> usize {
    match bytes.get(start) {
        Some(b'[') => skip_csi(bytes, start + 1, out),
        Some(b']' | b'P' | b'X' | b'^' | b'_') => skip_string(bytes, start + 1),
        _ => skip_plain_escape(bytes, start),
    }
}

/// Handle a CSI sequence whose parameters begin at `start`.
fn skip_csi(bytes: &[u8], start: usize, out: &mut Vec<u8>) -> usize {
    let length = bytes
        .iter()
        .skip(start)
        .take_while(|byte| (0x20..=0x3F).contains(*byte))
        .count();
    let end = start + length;
    let Some(&last) = bytes.get(end) else {
        return bytes.len();
    };
    if !(0x40..=0x7E).contains(&last) {
        return end;
    }
    let params = &bytes[start..end];
    let numeric = params.iter().all(|b| b.is_ascii_digit() || *b == b';');
    match last {
        b'C' => {
            if let Some(count) = forward_count(params) {
                out.resize(out.len() + count, b' ');
            }
        }
        b'H' | b'f' | b'E' if numeric => out.push(b'\n'),
        _ => {}
    }
    end + 1
}

/// How far `ESC[<params>C` moves: its one number, 1 when absent or 0, capped
/// at [`MAX_FORWARD`]. `None` when `params` is anything but one number.
fn forward_count(params: &[u8]) -> Option<usize> {
    if !params.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let count = params.iter().fold(0usize, |count, digit| {
        count
            .saturating_mul(10)
            .saturating_add(usize::from(digit - b'0'))
    });
    Some(count.clamp(1, MAX_FORWARD))
}

/// The end of a string sequence (OSC and its kin) whose body begins at
/// `start`: after its BEL or its ST (`ESC \`).
fn skip_string(bytes: &[u8], start: usize) -> usize {
    let mut i = start;
    while let Some(&byte) = bytes.get(i) {
        if byte == BEL {
            return i + 1;
        }
        if byte == ESC && bytes.get(i + 1) == Some(&b'\\') {
            return i + 2;
        }
        i += 1;
    }
    bytes.len()
}

/// The end of a two-part escape such as `ESC =` or `ESC ( B`, whose
/// intermediate bytes begin at `start`.
fn skip_plain_escape(bytes: &[u8], start: usize) -> usize {
    let length = bytes
        .iter()
        .skip(start)
        .take_while(|byte| (0x20..=0x2F).contains(*byte))
        .count();
    let end = start + length;
    match bytes.get(end) {
        Some(0x30..=0x7E) => end + 1,
        _ => end,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_forward_becomes_spaces() {
        assert_eq!(plain_text(b"a\x1b[3Cb\x1b[Cc"), "a   b c");
        assert_eq!(
            plain_text(b"\x1b[99999999999999999999999C"),
            " ".repeat(MAX_FORWARD)
        );
    }

    #[test]
    fn cursor_position_breaks_the_line() {
        assert_eq!(
            plain_text(b"one\x1b[2;1Htwo\x1b[3;1fthree\x1b[2Efour"),
            "one\ntwo\nthree\nfour"
        );
        assert_eq!(plain_text(b"one\r\ntwo\rthree"), "one\ntwo\nthree");
    }

    #[test]
    fn utf8_text_survives() {
        let text = "Καλημέρα ─ 日本語";
        assert_eq!(plain_text(text.as_bytes()), text);
    }

    #[test]
    fn a_split_or_invalid_utf8_sequence_does_not_panic() {
        assert_eq!(plain_text(b"a\xCE"), "a\u{FFFD}");
        assert_eq!(plain_text(b"\xFFb"), "\u{FFFD}b");
        assert_eq!(plain_text(b"\xCE\x1b[1C\x91"), "\u{FFFD} \u{FFFD}");
    }

    #[test]
    fn osc_and_other_escapes_are_dropped() {
        assert_eq!(plain_text(b"a\x1b]0;title\x07b"), "ab");
        assert_eq!(plain_text(b"a\x1b]8;;url\x1b\\b"), "ab");
        assert_eq!(plain_text(b"a\x1b[1;31mb\x1b[?25lc\x1b[2Kd"), "abcd");
        assert_eq!(plain_text(b"a\x1b(Bb\x1b=c"), "abc");
    }

    #[test]
    fn an_unterminated_escape_at_the_end_is_dropped() {
        for tail in [
            &b"\x1b"[..],
            b"\x1b[",
            b"\x1b[12",
            b"\x1b]0;title",
            b"\x1b(",
        ] {
            assert_eq!(plain_text(&[b"text", tail].concat()), "text", "{tail:?}");
        }
    }

    #[test]
    fn stream_json_passes_through() {
        let lines = "{\"type\":\"text\",\"text\":\"a\\tb ─\"}\n{\"type\":\"done\"}\n";
        assert_eq!(plain_text(lines.as_bytes()), lines);
    }
}
