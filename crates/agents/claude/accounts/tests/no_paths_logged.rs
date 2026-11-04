//! Every `tracing` call in the crate's sources names the file it concerns in words, never as a path: no path under
//! `~/.claude*` is ever logged.
//!
//! This is a textual check over the sources: it sees a method that turns a value into text carrying its path, and a
//! `?` (Debug) field whose text names a path, a file, a directory, a root, a profile, a lock, a guard or `paths` — a
//! `PathBuf` or a `#[derive(Debug)]` struct holding one. A `String` built from a path on an earlier line and logged
//! by name is beyond a textual check.

use std::fs;
use std::path::{Path, PathBuf};

/// The stems of the `tracing` macros this crate logs with; `event` writes an event directly, and any name ending in
/// `_span` writes a span.
const MACRO_NAMES: [&str; 6] = ["warn", "info", "error", "debug", "trace", "event"];

/// The method calls that turn a value into text carrying its path.
const PATH_SHAPED: [&str; 5] = [
    ".display()",
    ".to_string_lossy()",
    ".to_str()",
    ".as_os_str()",
    ".file_name()",
];

/// A `?` (Debug) field whose text names one of these is a path, or a struct holding one.
const PATH_WORDS: [&str; 8] = [
    "path", "file", "dir", "root", "profile", "lock", "guard", "paths",
];

#[test]
fn no_tracing_call_logs_a_path() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let files = rust_files(&root);
    assert!(
        !files.is_empty(),
        "no Rust sources were found under {}",
        root.display()
    );

    let mut offenders: Vec<String> = Vec::new();
    let mut unreadable: Vec<String> = Vec::new();
    for path in &files {
        let Ok(source) = fs::read_to_string(path) else {
            unreadable.push(path.display().to_string());
            continue;
        };
        for (line, call) in tracing_calls(&code_only(&source)) {
            if let Some(method) = PATH_SHAPED
                .iter()
                .copied()
                .find(|method| call.contains(*method))
            {
                offenders.push(format!(
                    "{}:{line}: {call} turns a value into text with {method}",
                    path.display()
                ));
            }
            for field in fields(call) {
                if debug_field_names_a_path(field) {
                    offenders.push(format!(
                        "{}:{line}: {} logs a path through a `?` field: {}",
                        path.display(),
                        call,
                        field.trim()
                    ));
                }
            }
        }
    }

    assert!(
        unreadable.is_empty(),
        "these sources could not be read: {unreadable:?}"
    );
    assert!(
        offenders.is_empty(),
        "these tracing calls log a path; log a fixed description of the file instead:\n{}",
        offenders.join("\n")
    );
}

/// Every `.rs` file under `dir`, at any depth.
fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        let Ok(entries) = fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                files.push(path);
            }
        }
    }
    files
}

/// `source` with comments and string and char literals blanked to spaces (newlines kept), so that an `!`, a `(`, a
/// `)` or a path-shaped method call inside text can never be mistaken for code.
fn code_only(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = bytes.to_vec();
    let mut index = 0;
    while index < bytes.len() {
        index = match bytes[index] {
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                skip_line_comment(bytes, index, &mut out)
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                skip_block_comment(bytes, index, &mut out)
            }
            b'"' => skip_quoted(bytes, index, &mut out, b'"'),
            b'\'' if is_char_literal(bytes, index) => skip_quoted(bytes, index, &mut out, b'\''),
            _ => index + 1,
        };
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Blanks the `//` comment at `start` up to the newline that ends it; returns the newline's index.
fn skip_line_comment(bytes: &[u8], start: usize, out: &mut [u8]) -> usize {
    let mut index = start;
    while index < bytes.len() && bytes[index] != b'\n' {
        out[index] = b' ';
        index += 1;
    }
    index
}

/// Blanks the `/* */` comment at `start` (nesting counted); returns the index after its close.
fn skip_block_comment(bytes: &[u8], start: usize, out: &mut [u8]) -> usize {
    let mut index = start;
    let mut depth = 0u32;
    while index < bytes.len() {
        if bytes[index] == b'/' && bytes.get(index + 1) == Some(&b'*') {
            depth += 1;
            out[index] = b' ';
            out[index + 1] = b' ';
            index += 2;
        } else if bytes[index] == b'*' && bytes.get(index + 1) == Some(&b'/') {
            depth -= 1;
            out[index] = b' ';
            out[index + 1] = b' ';
            index += 2;
            if depth == 0 {
                break;
            }
        } else {
            if bytes[index] != b'\n' {
                out[index] = b' ';
            }
            index += 1;
        }
    }
    index
}

/// Blanks the quoted literal at `start` (its `\` escapes included); returns the index after its closing `quote`.
fn skip_quoted(bytes: &[u8], start: usize, out: &mut [u8], quote: u8) -> usize {
    let mut index = start;
    out[index] = b' ';
    index += 1;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            out[index] = b' ';
            if index + 1 < bytes.len() {
                if bytes[index + 1] != b'\n' {
                    out[index + 1] = b' ';
                }
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }
        if bytes[index] == quote {
            out[index] = b' ';
            return index + 1;
        }
        if bytes[index] != b'\n' {
            out[index] = b' ';
        }
        index += 1;
    }
    index
}

/// Whether the `'` at `start` opens a char literal rather than a lifetime: an escape, or one character then a `'`.
fn is_char_literal(bytes: &[u8], start: usize) -> bool {
    if bytes.get(start + 1) == Some(&b'\\') {
        return true;
    }
    bytes.get(start + 2) == Some(&b'\'')
}

/// Each `tracing` call in `code`, as `(line, text)`, where `text` runs from the macro's name through its closing `)`.
/// `line` is 1-based.
fn tracing_calls(code: &str) -> Vec<(usize, &str)> {
    let bytes = code.as_bytes();
    let mut calls = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'!' {
            let start = macro_name_start(code, index);
            let end = start.and_then(|start| call_end(code, index + 1).map(|end| (start, end)));
            if let Some((start, end)) = end {
                let line = 1 + code[..start].bytes().filter(|byte| *byte == b'\n').count();
                calls.push((line, &code[start..end]));
                index = end;
                continue;
            }
        }
        index += 1;
    }
    calls
}

/// The index where the macro name ending at `bang` begins, when that name is one of [`MACRO_NAMES`], ends in `_span`,
/// or is such a name under a `tracing::` path.
fn macro_name_start(code: &str, bang: usize) -> Option<usize> {
    let bytes = code.as_bytes();
    let mut start = bang;
    while start > 0 && is_name_byte(bytes[start - 1]) {
        start -= 1;
    }
    if start == bang {
        return None;
    }
    let name = &code[start..bang];
    let last = name.rsplit("::").next().unwrap_or(name);
    (MACRO_NAMES.contains(&last) || last.ends_with("_span")).then_some(start)
}

/// The index just past the `)` closing the call's argument list, when a `(` follows `after_bang`; `None` when the
/// `!` is not a call's.
fn call_end(code: &str, after_bang: usize) -> Option<usize> {
    let bytes = code.as_bytes();
    let mut index = after_bang;
    while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
        index += 1;
    }
    if bytes.get(index) != Some(&b'(') {
        return None;
    }
    let mut depth = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index + 1);
                }
            }
            _ => {}
        }
        index += 1;
    }
    None
}

/// A call's arguments split at its top-level commas, e.g. `["account = number", " error = %error", " \"msg\""]`.
fn fields(call: &str) -> Vec<&str> {
    let Some(open) = call.find('(') else {
        return Vec::new();
    };
    let inner = call
        .get(open + 1..call.len().saturating_sub(1))
        .unwrap_or_default();
    let mut fields = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (index, byte) in inner.bytes().enumerate() {
        match byte {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b',' if depth == 0 => {
                fields.push(&inner[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    fields.push(&inner[start..]);
    fields
}

/// Whether `field` logs a path or a struct holding one through `?`: its value starts with `?` and its text names a
/// path, a file, a directory, a root, a profile, a lock, a guard or `paths`.
fn debug_field_names_a_path(field: &str) -> bool {
    let trimmed = field.trim();
    let value = trimmed
        .split_once('=')
        .map_or(trimmed, |(_, value)| value.trim_start());
    value.starts_with('?')
        && PATH_WORDS
            .iter()
            .any(|word| trimmed.to_lowercase().contains(word))
}

/// Whether `byte` can be part of an identifier or a `::` path separator.
fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b':'
}
