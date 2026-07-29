//! Comparison helper for secrets that arrive over the wire.
//!
//! Every secret the daemon checks against a caller-supplied value — the auth
//! token on the WS handshake and the `/shutdown` endpoint, the short pairing
//! code — goes through [`constant_time_eq`] rather than `==`. Rust's `==` on
//! slices and strings short-circuits at the first differing byte, so its
//! runtime leaks how long a shared prefix was.
//!
//! On the loopback listener that leak is uninteresting. It matters because the
//! same router is served by the opt-in LAN TLS listener on `0.0.0.0` (see
//! `server::start_lan_listener`), which puts these comparisons in front of
//! anyone on the network. Remote timing analysis through TLS and network jitter
//! is not a practical attack today; using the constant-time path everywhere
//! costs nothing and removes the question.

/// Compare two secrets without short-circuiting on the first differing byte.
///
/// Length is *not* hidden — an early return on mismatched lengths still leaks
/// the expected length, which is fine for the fixed-size tokens and codes this
/// is used for and avoids pretending to a stronger guarantee than it gives.
#[must_use]
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::constant_time_eq;

    #[test]
    fn equal_slices_match() {
        assert!(constant_time_eq(b"token-abc", b"token-abc"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn different_lengths_never_match() {
        assert!(!constant_time_eq(b"token", b"token-longer"));
        assert!(!constant_time_eq(b"", b"x"));
    }

    #[test]
    fn same_length_differences_are_caught_at_every_position() {
        // A short-circuiting comparison would still get these right; the point
        // is that a difference anywhere in the buffer is detected, including
        // the final byte, where an off-by-one in the loop bound would hide it.
        assert!(!constant_time_eq(b"aaaaaaaa", b"baaaaaaa"));
        assert!(!constant_time_eq(b"aaaaaaaa", b"aaaabaaa"));
        assert!(!constant_time_eq(b"aaaaaaaa", b"aaaaaaab"));
    }
}
