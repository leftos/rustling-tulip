//! Watches a PTY output stream for OSC terminal metadata sequences and stores
//! the latest terminal title / cwd alongside the session's stable label. The
//! UI can use that title as a dynamic display label and can use cwd to keep
//! plain shells grouped under their current repo or workspace.
//!
//! The recognized sequences are:
//! - `ESC ] 0 ; <title> BEL`  (set icon + window title)
//! - `ESC ] 1 ; <title> BEL`  (icon only — most terminals also surface this as the title)
//! - `ESC ] 2 ; <title> BEL`  (window title only)
//! - `ESC ] 7 ; file://<host>/<cwd> BEL`  (current working directory)
//! - for plain shells only, visible `PS <cwd>` prompt text as a fallback for
//!   sessions that were spawned before the prompt hook emitted OSC 7.
//!
//! Either `BEL` (`0x07`) or `ST` (`ESC \`) terminates the string. The 8-bit C1
//! forms (`0x9D` to open, `0x9C` to terminate) are intentionally NOT recognized:
//! both bytes are valid UTF-8 continuation bytes that appear inside common
//! multi-byte characters (e.g. `"` U+201C = `0xE2 0x80 0x9C`), so accepting
//! them as control plane would truncate titles mid-character. Titles whose
//! bytes are not valid UTF-8 are dropped rather than emitted with U+FFFD
//! replacement characters.
//!
//! Defensive caps:
//! - Titles longer than [`MAX_TITLE_BYTES`] are dropped at emit time.
//! - Cwd payloads longer than [`MAX_CWD_BYTES`] are dropped at emit time.
//! - Unterminated buffers longer than [`MAX_BUFFER_BYTES`] cause a return to
//!   `Normal` state so a malformed (or hostile) producer can't grow the buffer
//!   unboundedly.

use crate::orphan;
use crate::paths::Dirs;
use crate::session::SessionRegistry;
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::{debug, warn};

const MAX_TITLE_BYTES: usize = 256;
const MAX_CWD_BYTES: usize = 1024;
const MAX_BUFFER_BYTES: usize = 4096;
const MAX_PROMPT_TAIL_BYTES: usize = 8192;
const BEL: u8 = 0x07;
const ESC: u8 = 0x1B;

#[derive(Debug)]
enum State {
    /// Outside any escape sequence.
    Normal,
    /// Just saw `ESC`; the next byte decides what kind of sequence this is.
    AfterEsc,
    /// In `ESC ]`, reading the OSC parameter digits before `;`.
    OscParam {
        /// Parsed number so far. Bounded to the commands this watcher accepts.
        param: u8,
        /// True once we've seen at least one digit (so a stray `;` doesn't
        /// match an empty parameter).
        had_digit: bool,
    },
    /// In `ESC ] <0|1|2|7> ;`, accumulating until BEL or `ESC \`.
    OscString { param: u8 },
    /// Inside `OscString` and we just saw `ESC`; if next byte is `\` (ST), commit.
    OscStringAfterEsc { param: u8 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum OscEvent {
    Title(String),
    CurrentCwd(String),
}

struct Parser {
    state: State,
    buf: Vec<u8>,
    prompt_tail: Vec<u8>,
    infer_prompt_cwd: bool,
}

impl Parser {
    fn new(infer_prompt_cwd: bool) -> Self {
        Self {
            state: State::Normal,
            buf: Vec::new(),
            prompt_tail: Vec::new(),
            infer_prompt_cwd,
        }
    }

    /// Feed a chunk of bytes. Returns every complete metadata event in order.
    fn feed(&mut self, bytes: &[u8]) -> Vec<OscEvent> {
        let mut events = Vec::new();
        for &b in bytes {
            match self.state {
                State::Normal => {
                    if b == ESC {
                        self.state = State::AfterEsc;
                    }
                }
                State::AfterEsc => {
                    if b == b']' {
                        self.state = State::OscParam {
                            param: 0,
                            had_digit: false,
                        };
                    } else {
                        self.state = State::Normal;
                    }
                }
                State::OscParam { param, had_digit } => {
                    if b.is_ascii_digit() {
                        let digit = b - b'0';
                        let next = param.saturating_mul(10).saturating_add(digit);
                        if is_supported_param_prefix(next) {
                            self.state = State::OscParam {
                                param: next,
                                had_digit: true,
                            };
                        } else {
                            self.state = State::Normal;
                        }
                    } else if b == b';' && had_digit {
                        if is_supported_param(param) {
                            self.buf.clear();
                            self.state = State::OscString { param };
                        } else {
                            self.state = State::Normal;
                        }
                    } else {
                        self.state = State::Normal;
                    }
                }
                State::OscString { param } => {
                    if b == BEL {
                        if let Some(event) = self.emit(param) {
                            events.push(event);
                        }
                        self.state = State::Normal;
                    } else if b == ESC {
                        self.state = State::OscStringAfterEsc { param };
                    } else {
                        self.buf.push(b);
                        if self.buf.len() > MAX_BUFFER_BYTES {
                            warn!("OSC title buffer exceeded {MAX_BUFFER_BYTES} bytes; abandoning");
                            self.buf.clear();
                            self.state = State::Normal;
                        }
                    }
                }
                State::OscStringAfterEsc { param } => {
                    if b == b'\\' {
                        if let Some(event) = self.emit(param) {
                            events.push(event);
                        }
                        self.state = State::Normal;
                    } else {
                        // ESC inside an OSC string but not followed by `\`:
                        // protocol violation. Abandon the buffer and go back
                        // to interpreting bytes normally — if this byte was
                        // itself an ESC starting a fresh sequence, handle it.
                        self.buf.clear();
                        self.state = if b == ESC {
                            State::AfterEsc
                        } else {
                            State::Normal
                        };
                    }
                }
            }
        }
        if self.infer_prompt_cwd {
            self.remember_prompt_bytes(bytes);
            if let Some(cwd) = self.latest_prompt_cwd() {
                events.push(OscEvent::CurrentCwd(cwd));
            }
        }
        events
    }

    fn emit(&mut self, param: u8) -> Option<OscEvent> {
        let bytes = std::mem::take(&mut self.buf);
        if bytes.is_empty() || bytes.len() > max_bytes_for_param(param) {
            return None;
        }
        let Ok(decoded) = std::str::from_utf8(&bytes) else {
            return None;
        };
        let value = decoded.trim().to_string();
        if value.is_empty() {
            return None;
        }
        match param {
            0..=2 => Some(OscEvent::Title(value)),
            7 => parse_osc7_cwd(&value).map(OscEvent::CurrentCwd),
            _ => None,
        }
    }

    fn remember_prompt_bytes(&mut self, bytes: &[u8]) {
        self.prompt_tail.extend_from_slice(bytes);
        if self.prompt_tail.len() > MAX_PROMPT_TAIL_BYTES {
            let extra = self.prompt_tail.len() - MAX_PROMPT_TAIL_BYTES;
            self.prompt_tail.drain(..extra);
        }
    }

    fn latest_prompt_cwd(&self) -> Option<String> {
        let text = String::from_utf8_lossy(&self.prompt_tail);
        let visible = strip_terminal_sequences(&text);
        let normalized = visible.replace('\r', "\n");
        normalized
            .lines()
            .rev()
            .find_map(parse_powershell_prompt_cwd)
    }
}

fn is_supported_param_prefix(param: u8) -> bool {
    matches!(param, 0 | 1 | 2 | 7)
}

fn is_supported_param(param: u8) -> bool {
    matches!(param, 0 | 1 | 2 | 7)
}

const fn max_bytes_for_param(param: u8) -> usize {
    match param {
        7 => MAX_CWD_BYTES,
        _ => MAX_TITLE_BYTES,
    }
}

fn parse_osc7_cwd(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let Some(rest) = raw.strip_prefix("file://") else {
        return Some(normalize_path_separators(raw));
    };
    let slash = rest.find('/')?;
    let path = percent_decode(&rest[slash..]);
    if path.is_empty() {
        None
    } else {
        Some(normalize_file_uri_path(&path))
    }
}

fn normalize_file_uri_path(path: &str) -> String {
    let mut out = path.to_string();
    if cfg!(windows) && out.starts_with('/') && out.as_bytes().get(2) == Some(&b':') {
        out.remove(0);
    }
    normalize_path_separators(&out)
}

fn normalize_path_separators(path: &str) -> String {
    if cfg!(windows) {
        path.replace('/', "\\")
    } else {
        path.to_string()
    }
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

const fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(10 + (b - b'a')),
        b'A'..=b'F' => Some(10 + (b - b'A')),
        _ => None,
    }
}

fn strip_terminal_sequences(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == ESC {
            i += 1;
            if i >= bytes.len() {
                break;
            }
            if bytes[i] == b'[' {
                i += 1;
                while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                    i += 1;
                }
                i = (i + 1).min(bytes.len());
            } else if bytes[i] == b']' {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == BEL {
                        i += 1;
                        break;
                    }
                    if bytes[i] == ESC && bytes.get(i + 1) == Some(&b'\\') {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            } else {
                i += 1;
            }
        } else {
            let Some(ch) = input[i..].chars().next() else {
                break;
            };
            if !ch.is_control() || ch == '\r' || ch == '\n' || ch == '\t' {
                out.push(ch);
            }
            i += ch.len_utf8();
        }
    }
    out
}

fn parse_powershell_prompt_cwd(line: &str) -> Option<String> {
    let line = line.trim_start();
    let rest = line.strip_prefix("PS ")?;
    let raw = rest.split('>').next()?.trim();
    let path = raw.rsplit_once("::").map_or(raw, |(_, path)| path).trim();
    if is_filesystem_path(path) {
        Some(normalize_path_separators(path))
    } else {
        None
    }
}

fn is_filesystem_path(path: &str) -> bool {
    if path.starts_with('/') || path.starts_with(r"\\") {
        return true;
    }
    let bytes = path.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
}

/// Spawn the watcher task. The task ends when the PTY broadcast closes.
pub fn watch(
    registry: &Arc<SessionRegistry>,
    session_id: String,
    mut output: broadcast::Receiver<Vec<u8>>,
    dirs: Dirs,
    infer_prompt_cwd: bool,
) {
    let registry = Arc::clone(registry);
    tokio::spawn(async move {
        let mut parser = Parser::new(infer_prompt_cwd);
        let mut last_title: Option<String> = None;
        let mut last_cwd: Option<String> = None;
        loop {
            match output.recv().await {
                Ok(bytes) => {
                    for event in parser.feed(&bytes) {
                        match event {
                            OscEvent::Title(title)
                                if last_title.as_deref() != Some(title.as_str()) =>
                            {
                                debug!(session_id = %session_id, %title, "applying OSC title");
                                let title_for_update = title.clone();
                                registry.update(&session_id, |rec| {
                                    rec.terminal_title = Some(title_for_update.clone());
                                });
                                orphan::try_update_terminal_title(&dirs, &session_id, &title);
                                last_title = Some(title);
                            }
                            OscEvent::CurrentCwd(cwd)
                                if last_cwd.as_deref() != Some(cwd.as_str()) =>
                            {
                                debug!(session_id = %session_id, %cwd, "applying OSC cwd");
                                let cwd_for_update = cwd.clone();
                                registry.update(&session_id, |rec| {
                                    rec.current_cwd = Some(cwd_for_update.clone());
                                });
                                orphan::try_update_current_cwd(&dirs, &session_id, &cwd);
                                last_cwd = Some(cwd);
                            }
                            OscEvent::Title(_) | OscEvent::CurrentCwd(_) => {}
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(session_id = %session_id, lagged = n, "osc_title lagged");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(p: &mut Parser, chunks: &[&[u8]]) -> Vec<OscEvent> {
        let mut out = Vec::new();
        for c in chunks {
            out.extend(p.feed(c));
        }
        out
    }

    fn title(value: &str) -> OscEvent {
        OscEvent::Title(value.to_string())
    }

    fn cwd(value: &str) -> OscEvent {
        OscEvent::CurrentCwd(value.to_string())
    }

    #[test]
    fn bel_terminator() {
        let mut p = Parser::new(false);
        let titles = feed_all(&mut p, &[b"\x1b]0;hello\x07"]);
        assert_eq!(titles, vec![title("hello")]);
    }

    #[test]
    fn st_terminator() {
        let mut p = Parser::new(false);
        let titles = feed_all(&mut p, &[b"\x1b]2;world\x1b\\"]);
        assert_eq!(titles, vec![title("world")]);
    }

    #[test]
    fn split_across_chunks() {
        let mut p = Parser::new(false);
        let titles = feed_all(&mut p, &[b"prefix\x1b]0;part", b"ial-then\x07rest"]);
        assert_eq!(titles, vec![title("partial-then")]);
    }

    #[test]
    fn ignores_other_osc_commands() {
        // OSC 8 (hyperlink) should not be interpreted as terminal metadata.
        let mut p = Parser::new(false);
        let titles = feed_all(&mut p, &[b"\x1b]8;;http://x\x07"]);
        assert!(titles.is_empty(), "got titles: {titles:?}");
    }

    #[test]
    fn drops_empty_title() {
        let mut p = Parser::new(false);
        let titles = feed_all(&mut p, &[b"\x1b]0;\x07"]);
        assert!(titles.is_empty());
    }

    #[test]
    fn caps_oversized_title() {
        let mut p = Parser::new(false);
        let mut s = Vec::from(b"\x1b]0;".as_slice());
        s.extend(std::iter::repeat_n(b'a', MAX_TITLE_BYTES + 1));
        s.push(BEL);
        let titles = feed_all(&mut p, &[&s]);
        assert!(titles.is_empty());
    }

    #[test]
    fn abandons_runaway_buffer() {
        let mut p = Parser::new(false);
        let mut s = Vec::from(b"\x1b]0;".as_slice());
        s.extend(std::iter::repeat_n(b'a', MAX_BUFFER_BYTES + 10));
        // No terminator at all in the chunk.
        let titles = feed_all(&mut p, &[&s]);
        assert!(titles.is_empty());
        // After the runaway, a fresh sequence should still parse.
        let titles = feed_all(&mut p, &[b"\x1b]0;ok\x07"]);
        assert_eq!(titles, vec![title("ok")]);
    }

    #[test]
    fn title_with_curly_double_quote_round_trips() {
        // U+201C LEFT DOUBLE QUOTATION MARK = 0xE2 0x80 0x9C.
        // The 0x9C used to be misread as C1 ST and truncated the title mid-char.
        let mut p = Parser::new(false);
        let titles = feed_all(&mut p, &[b"\x1b]0;\xe2\x80\x9chello\xe2\x80\x9d\x07"]);
        assert_eq!(titles, vec![title("\u{201c}hello\u{201d}")]);
    }

    #[test]
    fn title_with_six_pointed_star_round_trips() {
        // U+273B SIX POINTED BLACK STAR = 0xE2 0x9C 0xBB.
        // Reproduces the Claude-CLI title bug where 0x9C terminated mid-glyph.
        let mut p = Parser::new(false);
        let titles = feed_all(&mut p, &[b"\x1b]2;\xe2\x9c\xbb Claude\x07"]);
        assert_eq!(titles, vec![title("\u{273b} Claude")]);
    }

    #[test]
    fn title_with_u_umlaut_round_trips() {
        // U+00DC LATIN CAPITAL LETTER U WITH DIAERESIS = 0xC3 0x9C.
        let mut p = Parser::new(false);
        let titles = feed_all(&mut p, &[b"\x1b]0;\xc3\x9cber\x07"]);
        assert_eq!(titles, vec![title("\u{00dc}ber")]);
    }

    #[test]
    fn invalid_utf8_title_is_dropped() {
        // 0xFE is never valid in UTF-8. Old behavior: emit a U+FFFD-only title.
        // New behavior: drop it entirely so a real follow-up can win.
        let mut p = Parser::new(false);
        let titles = feed_all(&mut p, &[b"\x1b]0;\xfe\xfe\x07"]);
        assert!(titles.is_empty(), "got titles: {titles:?}");
    }

    #[test]
    fn accepts_param_1_and_2_within_chunk() {
        let mut p = Parser::new(false);
        let titles = feed_all(&mut p, &[b"\x1b]1;icon\x07\x1b]2;window\x07"]);
        assert_eq!(titles, vec![title("icon"), title("window")]);
    }

    #[test]
    fn accepts_param_1_and_2_across_separate_chunks() {
        let mut p = Parser::new(false);
        let titles = feed_all(&mut p, &[b"\x1b]1;icon\x07", b"\x1b]2;window\x07"]);
        assert_eq!(titles, vec![title("icon"), title("window")]);
    }

    #[test]
    fn parses_osc_7_file_uri() {
        let mut p = Parser::new(false);
        let sequence: &[u8] = if cfg!(windows) {
            b"\x1b]7;file:///C:/Users/Leftos/a%20b\x07"
        } else {
            b"\x1b]7;file:///home/leftos/a%20b\x07"
        };
        let expected = if cfg!(windows) {
            "C:\\Users\\Leftos\\a b"
        } else {
            "/home/leftos/a b"
        };
        let events = feed_all(&mut p, &[sequence]);
        assert_eq!(events, vec![cwd(expected)]);
    }

    #[test]
    fn keeps_title_and_cwd_events_from_one_chunk() {
        let mut p = Parser::new(false);
        let sequence: &[u8] = if cfg!(windows) {
            b"\x1b]0;title\x07\x1b]7;file:///C:/dev/repo\x07"
        } else {
            b"\x1b]0;title\x07\x1b]7;file:///tmp/repo\x07"
        };
        let expected = if cfg!(windows) {
            "C:\\dev\\repo"
        } else {
            "/tmp/repo"
        };
        let events = feed_all(&mut p, &[sequence]);
        assert_eq!(events, vec![title("title"), cwd(expected)]);
    }

    #[test]
    fn infers_powershell_cwd_from_visible_prompt() {
        let mut p = Parser::new(true);
        let events = feed_all(
            &mut p,
            &[b"PS X:\\dev> cd .\\rustling-tulip\\\r\nPS X:\\dev\\rustling-tulip> "],
        );
        assert_eq!(events, vec![cwd("X:\\dev\\rustling-tulip")]);
    }

    #[test]
    fn ignores_powershell_prompt_text_when_prompt_inference_is_off() {
        let mut p = Parser::new(false);
        let events = feed_all(&mut p, &[b"PS X:\\dev\\rustling-tulip> "]);
        assert!(events.is_empty());
    }

    #[test]
    fn strips_csi_before_powershell_prompt_inference() {
        let mut p = Parser::new(true);
        let events = feed_all(&mut p, &[b"\x1b[32mPS X:\\dev\\repo> \x1b[0m"]);
        assert_eq!(events, vec![cwd("X:\\dev\\repo")]);
    }
}
