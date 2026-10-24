//! What counts as text for the built-in viewer, and where a file goes:
//! the viewer or the app the OS picks. Plain Rust: no GPUI, and no I/O
//! besides [`probe_file`].

use crate::open::OpenAction;

use std::io::Read;
use std::path::{Path, PathBuf};

/// The largest file the viewer opens; the same cap the daemon's snapshot
/// and the diff engine use.
pub(crate) const MAX_VIEW_BYTES: u64 = 2 * 1024 * 1024;

/// How many leading bytes are sniffed for a NUL byte and for valid text.
pub(crate) const SNIFF_BYTES: usize = 8000;

/// File extensions and the language id [`crate::syntax`] has a grammar for.
///
/// The daemon keeps its own list (`crates/daemon/src/git_inspect.rs`,
/// `language_for_path`) and the client cannot reach it; the test
/// `every_language_id_has_a_grammar` keeps this one honest.
const EXTENSIONS: [(&str, &str); 46] = [
    ("rs", "rust"),
    ("md", "markdown"),
    ("markdown", "markdown"),
    ("json", "json"),
    ("jsonc", "json"),
    ("toml", "toml"),
    ("js", "javascript"),
    ("mjs", "javascript"),
    ("cjs", "javascript"),
    ("jsx", "javascript"),
    ("ts", "typescript"),
    ("tsx", "typescript"),
    ("mts", "typescript"),
    ("cts", "typescript"),
    ("py", "python"),
    ("pyi", "python"),
    ("cs", "csharp"),
    ("sh", "shell"),
    ("bash", "shell"),
    ("zsh", "shell"),
    ("yml", "yaml"),
    ("yaml", "yaml"),
    ("html", "html"),
    ("htm", "html"),
    ("xml", "xml"),
    ("xsd", "xml"),
    ("xsl", "xml"),
    ("svg", "xml"),
    ("css", "css"),
    ("scss", "scss"),
    ("c", "c"),
    ("h", "c"),
    ("cpp", "cpp"),
    ("hpp", "cpp"),
    ("cc", "cpp"),
    ("cxx", "cpp"),
    ("hh", "cpp"),
    ("hxx", "cpp"),
    ("go", "go"),
    ("java", "java"),
    ("kt", "kotlin"),
    ("kts", "kotlin"),
    ("swift", "swift"),
    ("rb", "ruby"),
    ("php", "php"),
    ("sql", "sql"),
];

/// Why a file is not text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NotText {
    /// Over [`MAX_VIEW_BYTES`].
    TooLarge,
    /// A NUL byte in the first [`SNIFF_BYTES`] bytes.
    Binary,
    /// Neither valid UTF-8 nor UTF-16 with a BOM.
    NotDecodable,
}

/// A file's length and its first [`SNIFF_BYTES`] bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Probe {
    pub size: u64,
    pub head: Vec<u8>,
}

/// Reads the file's length and its first [`SNIFF_BYTES`] bytes.
pub(crate) fn probe_file(path: &Path) -> std::io::Result<Probe> {
    let file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    let mut head = Vec::new();
    let mut reader = file.take(SNIFF_BYTES as u64);
    reader.read_to_end(&mut head)?;
    Ok(Probe { size, head })
}

/// The encoding a UTF-16 BOM opens `bytes` with, and the bytes after it.
fn utf16_encoding(bytes: &[u8]) -> Option<(&'static encoding_rs::Encoding, &[u8])> {
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return Some((encoding_rs::UTF_16LE, rest));
    }
    bytes
        .strip_prefix(&[0xFE, 0xFF])
        .map(|rest| (encoding_rs::UTF_16BE, rest))
}

/// Whether `probe` can be text: within the cap, no NUL byte outside a UTF-16
/// file (whose two-byte units carry NULs for every ASCII character), and a
/// head that decodes.
pub(crate) fn check(probe: &Probe) -> Result<(), NotText> {
    if probe.size > MAX_VIEW_BYTES {
        return Err(NotText::TooLarge);
    }
    // A head shorter than the file was cut, so it may end mid-sequence.
    let cut = probe.size > probe.head.len() as u64;
    if let Some((encoding, rest)) = utf16_encoding(&probe.head) {
        return check_utf16(encoding, rest, cut);
    }
    if probe.head.contains(&0) {
        return Err(NotText::Binary);
    }
    let body = probe
        .head
        .strip_prefix(&[0xEF, 0xBB, 0xBF])
        .unwrap_or(probe.head.as_slice());
    match std::str::from_utf8(body) {
        Ok(_) => Ok(()),
        Err(error) if cut && error.error_len().is_none() => Ok(()),
        Err(_) => Err(NotText::NotDecodable),
    }
}

/// Whether the bytes after a UTF-16 BOM decode; `cut` lets the head end
/// inside a code unit or a surrogate pair.
fn check_utf16(
    encoding: &'static encoding_rs::Encoding,
    rest: &[u8],
    cut: bool,
) -> Result<(), NotText> {
    let mut len = rest.len() - rest.len() % 2;
    if len < rest.len() && !cut {
        return Err(NotText::NotDecodable);
    }
    loop {
        let decodes = encoding
            .decode_without_bom_handling_and_without_replacement(&rest[..len])
            .is_some();
        if decodes {
            return Ok(());
        }
        if !cut || len < 2 {
            return Err(NotText::NotDecodable);
        }
        len -= 2;
    }
}

/// `bytes` as text: UTF-16 with a BOM, else UTF-8 with any BOM stripped.
pub(crate) fn decode(bytes: &[u8]) -> Result<String, NotText> {
    if bytes.len() as u64 > MAX_VIEW_BYTES {
        return Err(NotText::TooLarge);
    }
    if let Some((encoding, rest)) = utf16_encoding(bytes) {
        return encoding
            .decode_without_bom_handling_and_without_replacement(rest)
            .map(std::borrow::Cow::into_owned)
            .ok_or(NotText::NotDecodable);
    }
    let head = &bytes[..bytes.len().min(SNIFF_BYTES)];
    if head.contains(&0) {
        return Err(NotText::Binary);
    }
    let body = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    std::str::from_utf8(body)
        .map(str::to_owned)
        .map_err(|_| NotText::NotDecodable)
}

/// `text`'s lines, on `\r\n`, `\n` and a lone `\r`; a final terminator adds
/// no empty line.
#[must_use]
pub(crate) fn split_lines(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut lines = Vec::new();
    let mut start = 0;
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'\r' => {
                lines.push(&text[start..at]);
                if bytes.get(at + 1) == Some(&b'\n') {
                    at += 1;
                }
                start = at + 1;
            }
            b'\n' => {
                lines.push(&text[start..at]);
                start = at + 1;
            }
            _ => {}
        }
        at += 1;
    }
    if start < bytes.len() {
        lines.push(&text[start..]);
    }
    lines
}

/// The language id [`crate::syntax`] has a grammar for that `path`'s
/// extension names, lower-cased; `None` for an unknown or missing one.
#[must_use]
pub(crate) fn language_for_path(path: &Path) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    EXTENSIONS
        .iter()
        .find(|(known, _)| known.eq_ignore_ascii_case(&extension))
        .map(|(_, language)| *language)
}

/// Where an opened file goes: the viewer, or out to an external app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// The built-in viewer, at the line the link named.
    Viewer { path: PathBuf, line: Option<u32> },
    /// The external app; why the file is not text, when it is not.
    External(Option<NotText>),
}

/// Where `action` sends `probe`'s file: the external app when the setting
/// is on or the file is not text, else the viewer at its line.
#[must_use]
pub(crate) fn decide(open_text_externally: bool, action: &OpenAction, probe: &Probe) -> Verdict {
    if open_text_externally {
        return Verdict::External(None);
    }
    if let Err(not_text) = check(probe) {
        return Verdict::External(Some(not_text));
    }
    let (path, line) = match action {
        OpenAction::VsCode { path, line, .. } => (path.clone(), Some(*line)),
        OpenAction::DefaultApp(path) => (path.clone(), None),
    };
    Verdict::Viewer { path, line }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use crate::syntax;
    use std::path::Path;

    fn probe(size: u64, head: &[u8]) -> Probe {
        Probe {
            size,
            head: head.to_vec(),
        }
    }

    fn text_probe() -> Probe {
        probe(5, b"hello")
    }

    #[test]
    fn the_setting_on_sends_a_line_ref_to_vscode_and_a_plain_path_to_the_default_app() {
        let line_ref = OpenAction::VsCode {
            path: std::path::PathBuf::from("C:/src/main.rs"),
            line: 7,
            column: Some(3),
        };
        assert_eq!(
            decide(true, &line_ref, &text_probe()),
            Verdict::External(None)
        );
        let plain = OpenAction::DefaultApp(std::path::PathBuf::from("C:/src/main.rs"));
        assert_eq!(decide(true, &plain, &text_probe()), Verdict::External(None));
    }

    #[test]
    fn text_goes_to_the_viewer_when_the_setting_is_off() {
        let line_ref = OpenAction::VsCode {
            path: std::path::PathBuf::from("C:/src/main.rs"),
            line: 7,
            column: Some(3),
        };
        assert_eq!(
            decide(false, &line_ref, &text_probe()),
            Verdict::Viewer {
                path: std::path::PathBuf::from("C:/src/main.rs"),
                line: Some(7),
            }
        );
    }

    #[test]
    fn a_binary_probe_goes_external_with_the_reason() {
        let plain = OpenAction::DefaultApp(std::path::PathBuf::from("C:/bin/blob"));
        assert_eq!(
            decide(false, &plain, &probe(9, b"plain\0txt")),
            Verdict::External(Some(NotText::Binary))
        );
    }

    #[test]
    fn a_latin1_head_is_not_decodable_and_goes_external() {
        let plain = OpenAction::DefaultApp(std::path::PathBuf::from("C:/src/cafe.txt"));
        assert_eq!(
            decide(false, &plain, &probe(11, b"caf\xE9 latin1")),
            Verdict::External(Some(NotText::NotDecodable))
        );
    }

    #[test]
    fn a_utf8_head_cut_mid_character_is_still_text() {
        let cut = probe(100, b"caf\xC3");
        assert_eq!(check(&cut), Ok(()), "an incomplete tail is a cut head");
        let plain = OpenAction::DefaultApp(std::path::PathBuf::from("C:/src/cafe.txt"));
        assert_eq!(
            decide(false, &plain, &cut),
            Verdict::Viewer {
                path: std::path::PathBuf::from("C:/src/cafe.txt"),
                line: None,
            }
        );
    }

    #[test]
    fn a_whole_utf16_file_with_an_odd_length_is_not_decodable() {
        let odd = [0xFF, 0xFE, b'h', 0x00, b'i'];
        assert_eq!(check(&probe(5, &odd)), Err(NotText::NotDecodable));
    }

    #[test]
    fn a_nul_byte_in_the_head_is_binary() {
        assert_eq!(check(&probe(9, b"plain\0txt")), Err(NotText::Binary));
        assert_eq!(decode(b"plain\0txt"), Err(NotText::Binary));
    }

    #[test]
    fn utf8_bom_is_stripped() {
        assert_eq!(decode(b"\xEF\xBB\xBFhello"), Ok("hello".to_owned()));
        assert_eq!(check(&probe(8, b"\xEF\xBB\xBFhello")), Ok(()));
    }

    #[test]
    fn utf16_with_bom_decodes() {
        let le = [0xFF, 0xFE, b'h', 0x00, b'i', 0x00];
        assert_eq!(decode(&le), Ok("hi".to_owned()));
        assert_eq!(check(&probe(6, &le)), Ok(()), "the NULs are expected");
        let be = [0xFE, 0xFF, 0x00, b'h', 0x00, b'i'];
        assert_eq!(decode(&be), Ok("hi".to_owned()));
        assert_eq!(check(&probe(6, &be)), Ok(()), "the NULs are expected");
    }

    #[test]
    fn invalid_utf8_is_not_text() {
        assert_eq!(decode(b"hello \xC3\x28"), Err(NotText::NotDecodable));
    }

    #[test]
    fn a_file_over_two_mib_is_not_text() {
        assert_eq!(
            check(&probe(MAX_VIEW_BYTES + 1, b"text")),
            Err(NotText::TooLarge)
        );
        let over = usize::try_from(MAX_VIEW_BYTES + 1).expect("2 MiB fits a usize");
        let bytes = vec![b'a'; over];
        assert_eq!(decode(&bytes), Err(NotText::TooLarge));
    }

    #[test]
    fn an_unknown_extension_with_text_is_text() {
        assert_eq!(language_for_path(Path::new("notes.weird")), None);
        assert_eq!(check(&probe(5, b"hello")), Ok(()));
        assert_eq!(decode(b"hello"), Ok("hello".to_owned()));
    }

    #[test]
    fn every_language_id_has_a_grammar() {
        for (extension, language) in EXTENSIONS {
            assert!(syntax::has_grammar(language), "{extension} -> {language}");
        }
    }

    #[test]
    fn crlf_lf_and_lone_cr_split_lines() {
        assert_eq!(
            split_lines("a\r\nb\nc\rd\re\n"),
            vec!["a", "b", "c", "d", "e"]
        );
    }

    #[test]
    fn a_final_newline_adds_no_empty_line() {
        assert_eq!(split_lines("a\n"), vec!["a"]);
        assert_eq!(split_lines("a\r\n"), vec!["a"]);
        assert_eq!(split_lines("a\r"), vec!["a"]);
        assert_eq!(split_lines(""), Vec::<&str>::new());
    }

    #[test]
    fn probe_file_reads_size_and_head() {
        let path = std::env::temp_dir().join(format!("rt-viewer-probe-{}.txt", std::process::id()));
        std::fs::write(&path, b"hello").expect("write the probe file");
        let probe = probe_file(&path).expect("probe the file");
        assert_eq!(probe.size, 5);
        assert_eq!(probe.head, b"hello");
        std::fs::remove_file(&path).expect("remove the probe file");
    }
}
