//! Finds `ConPTY`'s startup cursor-position query in the PTY output.
//!
//! `ConPTY` sends `ESC[6n` at startup and paints nothing until it is answered.
//! The tracer answers it so a session starts with no client attached, and
//! removes it so no daemon, scrollback or client ever sees it. A client still
//! answers every later query.
//!
//! The scanner stays armed only through the whole CSI sequences of the startup
//! preamble. The first byte outside a CSI sequence belongs to the child, so it
//! disarms the scanner for good and a child's own query is never eaten.

/// The reply to the startup query: row 1, column 1. The grid is blank at a
/// fresh spawn, and this position is valid at any size.
pub const CURSOR_AT_ORIGIN: &[u8] = b"\x1b[1;1R";

const ESC: u8 = 0x1b;
/// `DSR 6`: the program asks where the cursor is.
const QUERY: &[u8] = b"\x1b[6n";
/// The CSI parameter and intermediate bytes.
const CSI_PARAMETER_BYTES: std::ops::RangeInclusive<u8> = 0x20..=0x3f;
/// The bytes that end a CSI sequence.
const CSI_FINAL_BYTES: std::ops::RangeInclusive<u8> = 0x40..=0x7e;

/// What one [`CursorQuery::feed`] call leaves.
pub struct Scanned {
    /// The bytes to deliver, in order, with the query removed.
    pub output: Vec<u8>,
    /// Whether this call removed the query, so it needs its reply.
    pub answer: bool,
}

/// The scanner over one PTY's output stream.
pub struct CursorQuery {
    armed: bool,
    /// The CSI sequence read so far and not yet delivered.
    held: Vec<u8>,
}

/// Where the bytes held so far stand in the CSI grammar.
enum Csi {
    Partial,
    Whole,
    Outside,
}

impl CursorQuery {
    /// A scanner that is armed, as at the start of a stream.
    pub fn new() -> Self {
        Self {
            armed: true,
            held: Vec::new(),
        }
    }

    /// Scans the next read of the stream. A partial sequence at its end is
    /// held for the next call, so nothing is reordered.
    pub fn feed(&mut self, bytes: &[u8]) -> Scanned {
        let mut output = Vec::with_capacity(bytes.len() + self.held.len());
        let mut answer = false;
        let mut rest = bytes;
        while self.armed
            && let Some((&byte, tail)) = rest.split_first()
        {
            rest = tail;
            answer |= self.step(byte, &mut output);
        }
        output.extend_from_slice(rest);
        Scanned { output, answer }
    }

    /// Takes one armed byte. Returns whether it completed the query.
    fn step(&mut self, byte: u8, output: &mut Vec<u8>) -> bool {
        self.held.push(byte);
        match classify(&self.held) {
            Csi::Partial => false,
            Csi::Whole if self.held == QUERY => {
                self.held.clear();
                self.armed = false;
                true
            }
            Csi::Whole => {
                output.append(&mut self.held);
                false
            }
            Csi::Outside => {
                output.append(&mut self.held);
                self.armed = false;
                false
            }
        }
    }

    /// Ends the stream, returning any partial sequence still held.
    pub fn finish(&mut self) -> Vec<u8> {
        self.armed = false;
        std::mem::take(&mut self.held)
    }
}

/// Classifies a non-empty run of bytes that started at a sequence boundary.
fn classify(held: &[u8]) -> Csi {
    match held {
        [ESC] | [ESC, b'['] => Csi::Partial,
        [ESC, b'[', .., last] => match last {
            byte if CSI_PARAMETER_BYTES.contains(byte) => Csi::Partial,
            byte if CSI_FINAL_BYTES.contains(byte) => Csi::Whole,
            _ => Csi::Outside,
        },
        _ => Csi::Outside,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lone_query_is_removed_and_answered() {
        let scanned = CursorQuery::new().feed(QUERY);
        assert_eq!(scanned.output, b"");
        assert!(scanned.answer);
    }

    #[test]
    fn a_query_split_at_every_offset_is_found() {
        let stream = b"\x1b[?9001h\x1b[6nhello";
        for split in 0..=stream.len() {
            let mut scanner = CursorQuery::new();
            let first = scanner.feed(&stream[..split]);
            let second = scanner.feed(&stream[split..]);
            let output = [first.output, second.output, scanner.finish()].concat();
            assert_eq!(output, b"\x1b[?9001hhello", "split at {split}");
            assert!(first.answer != second.answer, "split at {split}");
        }
    }

    #[test]
    fn a_csi_preamble_passes_before_the_query() {
        let scanned = CursorQuery::new().feed(b"\x1b[?1004h\x1b[?9001h\x1b[6n");
        assert_eq!(scanned.output, b"\x1b[?1004h\x1b[?9001h");
        assert!(scanned.answer);
    }

    #[test]
    fn bytes_after_the_query_are_kept() {
        let scanned = CursorQuery::new().feed(b"\x1b[6nPS C:\\> ");
        assert_eq!(scanned.output, b"PS C:\\> ");
        assert!(scanned.answer);
    }

    #[test]
    fn a_second_query_is_kept() {
        let mut scanner = CursorQuery::new();
        let same_read = scanner.feed(b"\x1b[6n\x1b[6n");
        assert_eq!(same_read.output, QUERY);
        assert!(same_read.answer);

        let next_read = scanner.feed(QUERY);
        assert_eq!(next_read.output, QUERY);
        assert!(!next_read.answer);
    }

    #[test]
    fn a_printable_first_byte_disarms() {
        let mut scanner = CursorQuery::new();
        let printed = scanner.feed(b"a\x1b[6n");
        assert_eq!(printed.output, b"a\x1b[6n");
        assert!(!printed.answer);

        let later = scanner.feed(QUERY);
        assert_eq!(later.output, QUERY);
        assert!(!later.answer);
    }

    #[test]
    fn a_partial_prefix_is_held_then_flushed_at_the_end() {
        let mut scanner = CursorQuery::new();
        let scanned = scanner.feed(b"\x1b[6");
        assert_eq!(scanned.output, b"");
        assert!(!scanned.answer);
        assert_eq!(scanner.finish(), b"\x1b[6");
        assert_eq!(scanner.finish(), b"");
    }

    #[test]
    fn a_csi_that_is_not_the_query_passes_whole() {
        let mut scanner = CursorQuery::new();
        let held = scanner.feed(b"\x1b[6");
        assert_eq!(held.output, b"");

        let whole = scanner.feed(b"8t\x1b[6n");
        assert_eq!(whole.output, b"\x1b[68t");
        assert!(whole.answer);
    }
}
