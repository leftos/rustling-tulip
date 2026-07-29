//! Sticky terminal-mode tracking for scrollback replay.
//!
//! xterm derives its bracketed-paste flag from the child's `\e[?2004h` /
//! `\e[?2004l` sequences in the output stream. An interactive agent (claude,
//! codex) enables bracketed paste ONCE at startup. When a long-lived session's
//! scrollback ring overflows and trims that early enable sequence, a
//! daemon-restart / reattach replays only the trimmed tail into a fresh xterm,
//! which therefore never re-sees the enable and leaves its flag off. Pastes
//! then go out un-bracketed and the agent submits on the first embedded
//! newline -- the "partial paste" bug this module exists to fix.
//!
//! Fix: track the latest bracketed-paste state as output flows (independent of
//! the ring, so trimming can't lose it), persist it to a tiny sidecar so it
//! survives daemon restarts even when BOTH the daemon scrollback ring and the
//! tracer ring have trimmed the original sequence, and re-assert it as a prefix
//! on the scrollback replay handed to a freshly-attaching client.
//!
//! Scope is deliberately narrow: only DEC private mode 2004 (bracketed paste).
//! Other sticky modes (alt-screen 1049, cursor keys, ...) are intentionally NOT
//! restored -- re-asserting them out of band could corrupt a TUI's screen
//! state, whereas a lone mode-2004 set is inert beyond enabling bracketing.

use crate::paths::Dirs;
use std::path::PathBuf;
use tracing::warn;

/// DEC private mode 2004 set (bracketed paste on).
const ENABLE: &[u8] = b"\x1b[?2004h";
/// DEC private mode 2004 reset (bracketed paste off).
const DISABLE: &[u8] = b"\x1b[?2004l";
/// Bytes carried between chunks so a mode sequence split across a chunk
/// boundary is still matched. One less than the (equal) sequence length: a
/// sequence fully contained in the previous chunk was already resolved there,
/// so keeping `len - 1` bytes catches every boundary split without ever
/// re-matching an already-counted sequence.
const CARRYOVER: usize = ENABLE.len() - 1;

fn state_path(dirs: &Dirs, session_id: &str) -> PathBuf {
    dirs.sessions_dir.join(session_id).join("termstate")
}

/// Incrementally tracks the child's bracketed-paste (DEC 2004) state across
/// output chunks, tolerating a sequence split across a chunk boundary.
pub struct BracketedPasteTracker {
    enabled: bool,
    /// Tail (< one full sequence) of the previous chunk, prepended to the next
    /// scan window so a boundary-split `\e[?2004h`/`l` is still detected.
    carry: Vec<u8>,
}

impl BracketedPasteTracker {
    #[must_use]
    pub fn new(initial: bool) -> Self {
        Self {
            enabled: initial,
            carry: Vec::new(),
        }
    }

    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Feed a chunk of child output. Returns `true` iff the tracked state
    /// changed, so the caller can persist the new value.
    pub fn observe(&mut self, chunk: &[u8]) -> bool {
        // Scan `carry + chunk` so a boundary-split sequence is caught; only the
        // LAST enable/disable in the window decides the resulting state.
        let mut window = std::mem::take(&mut self.carry);
        window.extend_from_slice(chunk);
        let resolved = last_mode_marker(&window);
        // Carry forward only the tail of *this chunk* (never the whole window),
        // bounded to `CARRYOVER`, so a sequence already fully seen here can't be
        // re-counted next call.
        let tail_start = chunk.len().saturating_sub(CARRYOVER);
        self.carry = chunk[tail_start..].to_vec();
        match resolved {
            Some(new_state) if new_state != self.enabled => {
                self.enabled = new_state;
                true
            }
            _ => false,
        }
    }
}

/// State implied by the LAST bracketed-paste enable/disable marker in `buf`, or
/// `None` if the window contains neither.
fn last_mode_marker(buf: &[u8]) -> Option<bool> {
    match (rfind(buf, ENABLE), rfind(buf, DISABLE)) {
        (None, None) => None,
        (Some(_), None) => Some(true),
        (None, Some(_)) => Some(false),
        (Some(enable_at), Some(disable_at)) => Some(enable_at > disable_at),
    }
}

/// Last index at which `needle` occurs in `haystack`.
fn rfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    (0..=haystack.len() - needle.len())
        .rev()
        .find(|&i| &haystack[i..i + needle.len()] == needle)
}

/// Load the persisted bracketed-paste state for a session. Missing or garbled
/// state reads as `false` (xterm's default) -- safe, because replay only ever
/// *adds* an enable prefix, never a spurious disable.
#[must_use]
pub fn load(dirs: &Dirs, session_id: &str) -> bool {
    match std::fs::read(state_path(dirs, session_id)) {
        Ok(bytes) => bytes.first() == Some(&b'1'),
        Err(_) => false,
    }
}

/// Persist the bracketed-paste state. Best-effort: a failure only means we may
/// have to rely on the live stream to re-establish the flag on the next attach.
pub fn save(dirs: &Dirs, session_id: &str, enabled: bool) {
    let path = state_path(dirs, session_id);
    if let Some(parent) = path.parent()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        warn!(?err, %session_id, "termstate: create dir failed");
        return;
    }
    let byte: &[u8] = if enabled { b"1" } else { b"0" };
    if let Err(err) = std::fs::write(&path, byte) {
        warn!(?err, %session_id, "termstate: write failed");
    }
}

/// Bytes to prepend to a scrollback replay so a freshly-attached xterm's
/// bracketed-paste flag matches the child. Empty unless bracketed paste is on.
#[must_use]
pub fn replay_prefix(enabled: bool) -> &'static [u8] {
    if enabled { ENABLE } else { b"" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_enable_then_disable() {
        let mut t = BracketedPasteTracker::new(false);
        assert!(t.observe(b"hello \x1b[?2004h world"));
        assert!(t.enabled());
        assert!(!t.observe(b"more output, no markers"));
        assert!(t.enabled());
        assert!(t.observe(b"bye \x1b[?2004l"));
        assert!(!t.enabled());
    }

    #[test]
    fn last_marker_in_chunk_wins() {
        let mut t = BracketedPasteTracker::new(false);
        // enable then disable in one chunk -> ends disabled (no change from
        // initial false, so no persist needed).
        assert!(!t.observe(b"\x1b[?2004h\x1b[?2004l"));
        assert!(!t.enabled());
        // disable then enable -> ends enabled.
        assert!(t.observe(b"\x1b[?2004l\x1b[?2004h"));
        assert!(t.enabled());
    }

    #[test]
    fn detects_sequence_split_across_chunks() {
        let mut t = BracketedPasteTracker::new(false);
        // Split "\x1b[?2004h" as "\x1b[?20" + "04h".
        assert!(!t.observe(b"padding\x1b[?20"));
        assert!(!t.enabled());
        assert!(t.observe(b"04h trailing"));
        assert!(t.enabled());
    }

    #[test]
    fn fully_contained_sequence_not_recounted() {
        let mut t = BracketedPasteTracker::new(false);
        assert!(t.observe(b"\x1b[?2004h"));
        assert!(t.enabled());
        // The carried tail must not re-trigger a change on a marker-free chunk.
        assert!(!t.observe(b"plain"));
        assert!(t.enabled());
    }

    #[test]
    fn replay_prefix_only_when_enabled() {
        assert_eq!(replay_prefix(true), ENABLE);
        assert!(replay_prefix(false).is_empty());
    }
}
