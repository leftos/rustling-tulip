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
//!
//! [`write_private`] covers the other half: the files those secrets are
//! persisted to (`daemon.json`, `lan.json`, `lan-key.pem`).

/// Write a secret-bearing file so only its owner can read it.
///
/// On Windows the config dir's ACL is already user-scoped, so this is the same
/// as `fs::write`. On Unix, `fs::write` creates `0666 & ~umask` — typically
/// `0644`, i.e. world-readable — which would expose the LAN TLS private key and
/// the auth token to every local user. The mode is set at creation time so a
/// freshly created file is never briefly readable, and applied again afterwards
/// so an existing file written by an older build gets tightened.
///
/// Callers using tmp+rename should point this at the *tmp* path: `rename`
/// preserves the mode, so the destination lands already restricted.
pub fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    file.write_all(bytes)?;
    drop(file);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

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
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
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
    fn write_private_round_trips_and_restricts_on_unix() {
        let path = std::env::temp_dir().join(format!("rt-secret-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&path);
        super::write_private(&path, b"s3cret").expect("write");
        assert_eq!(std::fs::read(&path).expect("read"), b"s3cret");
        // Overwriting an existing file must also truncate, not leave a tail.
        super::write_private(&path, b"hi").expect("rewrite");
        assert_eq!(std::fs::read(&path).expect("reread"), b"hi");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
            assert_eq!(mode & 0o077, 0, "group/other bits must be clear");
        }
        let _ = std::fs::remove_file(&path);
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
