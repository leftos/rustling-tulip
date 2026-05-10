//! Watches a PTY output stream for OSC window-title sequences and updates the
//! session's label live, so the sidebar tree reflects whatever the agent set
//! via `printf '\e]0;...\a'` (or `]1;` / `]2;`).
//!
//! The recognized sequences are:
//! - `ESC ] 0 ; <title> BEL`  (set icon + window title)
//! - `ESC ] 1 ; <title> BEL`  (icon only — most terminals also surface this as the title)
//! - `ESC ] 2 ; <title> BEL`  (window title only)
//!
//! Either `BEL` (`0x07`) or `ST` (`ESC \`) terminates the string. Title bytes
//! are decoded as UTF-8 lossily — non-UTF-8 bytes turn into U+FFFD rather than
//! dropping the whole title.
//!
//! Defensive caps:
//! - Titles longer than [`MAX_TITLE_BYTES`] are dropped at emit time.
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
const MAX_BUFFER_BYTES: usize = 4096;
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
        /// Parsed number so far. Bounded; we don't accept >2 since we ignore
        /// non-title OSC commands.
        param: u8,
        /// True once we've seen at least one digit (so a stray `;` doesn't
        /// match an empty parameter).
        had_digit: bool,
    },
    /// In `ESC ] <0|1|2> ;`, accumulating the title until BEL or `ESC \`.
    OscString,
    /// Inside `OscString` and we just saw `ESC`; if next byte is `\` (ST), commit.
    OscStringAfterEsc,
}

struct Parser {
    state: State,
    buf: Vec<u8>,
}

impl Parser {
    fn new() -> Self {
        Self {
            state: State::Normal,
            buf: Vec::new(),
        }
    }

    /// Feed a chunk of bytes. Returns the most recent complete title in this
    /// chunk, if any. (If a chunk contains multiple title sequences, only the
    /// last is returned — intermediate ones would be overwritten in the UI
    /// anyway.)
    fn feed(&mut self, bytes: &[u8]) -> Option<String> {
        let mut latest: Option<String> = None;
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
                        // Only param values 0,1,2 set the title; reject anything else early.
                        if next > 2 {
                            self.state = State::Normal;
                        } else {
                            self.state = State::OscParam {
                                param: next,
                                had_digit: true,
                            };
                        }
                    } else if b == b';' && had_digit {
                        self.buf.clear();
                        self.state = State::OscString;
                    } else {
                        self.state = State::Normal;
                    }
                }
                State::OscString => {
                    if b == BEL {
                        if let Some(title) = self.emit() {
                            latest = Some(title);
                        }
                        self.state = State::Normal;
                    } else if b == ESC {
                        self.state = State::OscStringAfterEsc;
                    } else {
                        self.buf.push(b);
                        if self.buf.len() > MAX_BUFFER_BYTES {
                            warn!("OSC title buffer exceeded {MAX_BUFFER_BYTES} bytes; abandoning");
                            self.buf.clear();
                            self.state = State::Normal;
                        }
                    }
                }
                State::OscStringAfterEsc => {
                    if b == b'\\' {
                        if let Some(title) = self.emit() {
                            latest = Some(title);
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
        latest
    }

    fn emit(&mut self) -> Option<String> {
        let bytes = std::mem::take(&mut self.buf);
        if bytes.is_empty() || bytes.len() > MAX_TITLE_BYTES {
            return None;
        }
        let title = String::from_utf8_lossy(&bytes).trim().to_string();
        if title.is_empty() { None } else { Some(title) }
    }
}

/// Spawn the watcher task. The task ends when the PTY broadcast closes.
pub fn watch(
    registry: &Arc<SessionRegistry>,
    session_id: String,
    mut output: broadcast::Receiver<Vec<u8>>,
    dirs: Dirs,
) {
    let registry = Arc::clone(registry);
    tokio::spawn(async move {
        let mut parser = Parser::new();
        let mut last_applied: Option<String> = None;
        loop {
            match output.recv().await {
                Ok(bytes) => {
                    if let Some(title) = parser.feed(&bytes)
                        && last_applied.as_deref() != Some(title.as_str())
                    {
                        debug!(session_id = %session_id, %title, "applying OSC title");
                        let title_for_update = title.clone();
                        registry.update(&session_id, |rec| {
                            rec.label.clone_from(&title_for_update);
                        });
                        orphan::try_update_label(&dirs, &session_id, &title);
                        last_applied = Some(title);
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

    fn feed_all(p: &mut Parser, chunks: &[&[u8]]) -> Vec<String> {
        let mut out = Vec::new();
        for c in chunks {
            if let Some(t) = p.feed(c) {
                out.push(t);
            }
        }
        out
    }

    #[test]
    fn bel_terminator() {
        let mut p = Parser::new();
        let titles = feed_all(&mut p, &[b"\x1b]0;hello\x07"]);
        assert_eq!(titles, vec!["hello"]);
    }

    #[test]
    fn st_terminator() {
        let mut p = Parser::new();
        let titles = feed_all(&mut p, &[b"\x1b]2;world\x1b\\"]);
        assert_eq!(titles, vec!["world"]);
    }

    #[test]
    fn split_across_chunks() {
        let mut p = Parser::new();
        let titles = feed_all(&mut p, &[b"prefix\x1b]0;part", b"ial-then\x07rest"]);
        assert_eq!(titles, vec!["partial-then"]);
    }

    #[test]
    fn ignores_other_osc_commands() {
        // OSC 8 (hyperlink) should not be interpreted as title.
        let mut p = Parser::new();
        let titles = feed_all(&mut p, &[b"\x1b]8;;http://x\x07"]);
        assert!(titles.is_empty(), "got titles: {titles:?}");
    }

    #[test]
    fn drops_empty_title() {
        let mut p = Parser::new();
        let titles = feed_all(&mut p, &[b"\x1b]0;\x07"]);
        assert!(titles.is_empty());
    }

    #[test]
    fn caps_oversized_title() {
        let mut p = Parser::new();
        let mut s = Vec::from(b"\x1b]0;".as_slice());
        s.extend(std::iter::repeat_n(b'a', MAX_TITLE_BYTES + 1));
        s.push(BEL);
        let titles = feed_all(&mut p, &[&s]);
        assert!(titles.is_empty());
    }

    #[test]
    fn abandons_runaway_buffer() {
        let mut p = Parser::new();
        let mut s = Vec::from(b"\x1b]0;".as_slice());
        s.extend(std::iter::repeat_n(b'a', MAX_BUFFER_BYTES + 10));
        // No terminator at all in the chunk.
        let titles = feed_all(&mut p, &[&s]);
        assert!(titles.is_empty());
        // After the runaway, a fresh sequence should still parse.
        let titles = feed_all(&mut p, &[b"\x1b]0;ok\x07"]);
        assert_eq!(titles, vec!["ok"]);
    }

    #[test]
    fn accepts_param_1_and_2_collapses_to_latest_within_chunk() {
        // feed() intentionally returns only the most recent completed title
        // per call — intermediate values would be overwritten in the UI
        // before anyone could see them anyway.
        let mut p = Parser::new();
        let titles = feed_all(&mut p, &[b"\x1b]1;icon\x07\x1b]2;window\x07"]);
        assert_eq!(titles, vec!["window"]);
    }

    #[test]
    fn accepts_param_1_and_2_across_separate_chunks() {
        let mut p = Parser::new();
        let titles = feed_all(&mut p, &[b"\x1b]1;icon\x07", b"\x1b]2;window\x07"]);
        assert_eq!(titles, vec!["icon", "window"]);
    }
}
