//! The key spec `send --keys` takes, as the bytes a terminal would send.
//!
//! Plain text stands for itself. The escapes are `\r \n \t \e \xHH`, and a
//! name between angle brackets is one key: `<Enter>`, `<Up>`, `<C-c>`. A `<`
//! with no `>` after it is plain text; `\x3c` writes one before a `>`.

/// The bytes `spec` stands for, or the usage error that says what is wrong.
pub fn decode(spec: &str) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::with_capacity(spec.len());
    let mut rest = spec;
    while let Some(first) = rest.chars().next() {
        let tail = &rest[first.len_utf8()..];
        rest = match first {
            '\\' => escape(tail, &mut bytes)?,
            '<' => {
                if let Some((name, after)) = tail.split_once('>') {
                    bytes.extend_from_slice(&named_key(name)?);
                    after
                } else {
                    bytes.push(b'<');
                    tail
                }
            }
            _ => {
                bytes.extend_from_slice(&rest.as_bytes()[..first.len_utf8()]);
                tail
            }
        };
    }
    Ok(bytes)
}

/// Decode the escape whose body starts `rest`, answering what follows it.
fn escape<'a>(rest: &'a str, bytes: &mut Vec<u8>) -> Result<&'a str, String> {
    let Some(letter) = rest.chars().next() else {
        return Err("a key spec ends in a lone \\".to_owned());
    };
    let tail = &rest[letter.len_utf8()..];
    let byte = match letter {
        'r' => b'\r',
        'n' => b'\n',
        't' => b'\t',
        'e' => 0x1B,
        'x' => {
            let hex = tail
                .get(..2)
                .filter(|hex| hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
                .ok_or_else(|| "\\x takes two hex digits".to_owned())?;
            bytes.push(u8::from_str_radix(hex, 16).map_err(|err| format!("bad \\x{hex}: {err}"))?);
            return Ok(&tail[2..]);
        }
        other => return Err(format!("unknown key escape \\{other}")),
    };
    bytes.push(byte);
    Ok(tail)
}

/// The bytes of the key called `name`.
fn named_key(name: &str) -> Result<Vec<u8>, String> {
    let bytes: &[u8] = match name {
        "Enter" => b"\r",
        "Esc" => b"\x1b",
        "Tab" => b"\t",
        "Up" => b"\x1b[A",
        "Down" => b"\x1b[B",
        "Right" => b"\x1b[C",
        "Left" => b"\x1b[D",
        "Backspace" => b"\x7f",
        "Space" => b" ",
        _ => return control_key(name).map(|byte| vec![byte]),
    };
    Ok(bytes.to_vec())
}

/// The byte of `C-a` to `C-z`: 1 to 26.
fn control_key(name: &str) -> Result<u8, String> {
    match name.strip_prefix("C-").map(str::as_bytes) {
        Some(&[letter]) if letter.is_ascii_lowercase() => Ok(letter - b'a' + 1),
        _ => Err(format!("unknown key name <{name}>")),
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;

    #[test]
    fn keys_decode_escapes_and_names() {
        let cases: [(&str, &[u8]); 8] = [
            ("ls\\r", b"ls\r"),
            ("\\n\\t\\e\\x41\\x7f", b"\n\t\x1bA\x7f"),
            ("<Enter><Esc><Tab>", b"\r\x1b\t"),
            ("<Up><Down><Right><Left>", b"\x1b[A\x1b[B\x1b[C\x1b[D"),
            ("<Backspace><Space>", b"\x7f "),
            ("<C-a><C-c><C-z>", &[1, 3, 26]),
            ("é<Enter>", "é\r".as_bytes()),
            ("a < b", b"a < b"),
        ];
        for (spec, bytes) in cases {
            assert_eq!(decode(spec).expect("a valid spec"), bytes, "{spec:?}");
        }
    }

    #[test]
    fn an_unknown_key_name_is_a_usage_error() {
        for (spec, message) in [
            ("<Home>", "unknown key name <Home>"),
            ("<C-1>", "unknown key name <C-1>"),
            ("<C-A>", "unknown key name <C-A>"),
            ("<C-ab>", "unknown key name <C-ab>"),
            ("\\q", "unknown key escape \\q"),
            ("\\x4", "\\x takes two hex digits"),
            ("\\xzz", "\\x takes two hex digits"),
            ("\\", "a key spec ends in a lone \\"),
        ] {
            assert_eq!(decode(spec).expect_err(spec), message);
        }
    }
}
