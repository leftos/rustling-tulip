//! Bounded byte-ring for PTY output. The tracer keeps the most recent N MB
//! of output so a reconnecting daemon can replay what it missed during the
//! outage.
//!
//! Phase C.1's empirical spike will pick the actual cap (the design doc
//! debates 2 MB vs 4 MB); for now we default to 4 MB which buys roughly
//! 30 minutes of typical claude output under light load.

const DEFAULT_CAP_BYTES: usize = 4 * 1024 * 1024;

/// Append-only ring with an overwrite-oldest policy when full. Designed for
/// single-producer / single-consumer access with the consumer being the
/// pipe-writer task; the supervisor wraps it in a mutex when needed.
pub struct OutputRing {
    buf: Vec<u8>,
    /// Total bytes ever appended (monotonic). Lets readers compute "how much
    /// did I miss" via deltas.
    written: u64,
    /// Set when an append truncated older bytes. Surfaced via `Status` so the
    /// daemon can warn the user that scrollback was lost across the outage.
    overflowed: bool,
}

impl OutputRing {
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAP_BYTES)
    }

    #[must_use]
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            buf: Vec::with_capacity(cap),
            written: 0,
            overflowed: false,
        }
    }

    /// Append a chunk. When the chunk plus the existing buffer exceeds the
    /// capacity, the oldest bytes are dropped until everything fits.
    /// Sets `overflowed = true` for any truncation; never reset on its own
    /// since the daemon should know the ring was lossy at any point.
    pub fn push(&mut self, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        self.written = self.written.saturating_add(chunk.len() as u64);
        let cap = self.buf.capacity();
        if chunk.len() > cap {
            // Chunk alone strictly exceeds capacity — drop everything and
            // keep the tail of the chunk.
            self.buf.clear();
            self.buf.extend_from_slice(&chunk[chunk.len() - cap..]);
            self.overflowed = true;
            return;
        }
        if self.buf.len() + chunk.len() > cap {
            // Slide window: drop just enough from the front to fit.
            let drop = (self.buf.len() + chunk.len()) - cap;
            self.buf.drain(0..drop);
            self.overflowed = true;
        }
        self.buf.extend_from_slice(chunk);
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    #[must_use]
    pub fn overflowed(&self) -> bool {
        self.overflowed
    }

    // `snapshot()`, `is_empty()`, `total_written()` will land when the
    // pipe-server task in supervisor.rs grows the (re)connect replay and
    // the status-reply path. Not added speculatively per project policy.
}

impl Default for OutputRing {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_under_cap_keeps_everything() {
        let mut ring = OutputRing::with_capacity(16);
        ring.push(b"hello");
        ring.push(b" world");
        assert_eq!(ring.len(), 11);
        assert!(!ring.overflowed());
    }

    #[test]
    fn push_over_cap_slides_window() {
        let mut ring = OutputRing::with_capacity(8);
        ring.push(b"01234567");
        assert!(!ring.overflowed());
        ring.push(b"abcd");
        assert!(ring.overflowed());
        assert_eq!(ring.len(), 8);
    }

    #[test]
    fn push_chunk_larger_than_cap_keeps_tail() {
        let mut ring = OutputRing::with_capacity(4);
        ring.push(b"abcdefghij");
        assert!(ring.overflowed());
        assert_eq!(ring.len(), 4);
    }
}
