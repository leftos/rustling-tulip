//! Helpers for the daemon's secrets: minting the auth token, comparing a
//! caller-supplied value against a secret, and writing a secret to disk.
//!
//! Every secret the daemon checks against a caller-supplied value — the auth
//! token on the WS handshake and the `/shutdown` endpoint — goes through
//! [`constant_time_eq`] rather than `==`. Rust's `==` on slices and strings
//! short-circuits at the first differing byte, so its runtime leaks how long a
//! shared prefix was. On the loopback listener that leak is uninteresting;
//! using the constant-time path everywhere costs nothing and removes the
//! question.
//!
//! [`write_private`] covers the other half: the files those secrets are
//! persisted to (`daemon.json` and the others that hold a secret).

use rand::Rng as _;
use rand::distributions::Alphanumeric;

const AUTH_TOKEN_LEN: usize = 48;

/// Entropy in a device token: 256 bits, which base64url without padding
/// renders as 43 characters.
const DEVICE_TOKEN_BYTES: usize = 32;

/// One past the largest six-digit pairing code.
const PAIRING_CODE_RANGE: u32 = 1_000_000;

/// Generate a fresh 48-char alphanumeric auth token, the `daemon.json` token
/// for one daemon run.
#[must_use]
pub fn generate_auth_token() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(AUTH_TOKEN_LEN)
        .map(char::from)
        .collect()
}

/// Generate a fresh 256-bit device token as 43 base64url characters without
/// padding.
///
/// A device token is handed to the paired device once and stored by the
/// daemon only as the hex SHA-256 of this string (see [`crate::devices`]).
#[must_use]
pub fn generate_device_token() -> String {
    use base64::Engine as _;
    use rand::RngCore as _;

    let mut bytes = [0u8; DEVICE_TOKEN_BYTES];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Generate a fresh pairing code: six decimal digits, zero-padded, uniform
/// over all million values.
///
/// Twenty bits is only enough because the code never leaves memory, lives
/// [`crate::devices::CODE_LIFETIME`] and is dropped after
/// [`crate::devices::MAX_WRONG_ATTEMPTS`] wrong tries.
#[must_use]
pub fn generate_pairing_code() -> String {
    let value: u32 = rand::rngs::OsRng.gen_range(0..PAIRING_CODE_RANGE);
    format!("{value:06}")
}

/// Write a secret-bearing file so only its owner can read it.
///
/// On Windows the config dir's ACL is already user-scoped, so this is the same
/// as `fs::write`. On Unix, `fs::write` creates `0666 & ~umask` — typically
/// `0644`, i.e. world-readable — which would expose the auth token to every
/// local user. The mode is set at creation time so a
/// freshly created file is never briefly readable, and applied again afterwards
/// so an existing file written by an older build gets tightened.
///
/// Callers using tmp+rename should point this at the *tmp* path: `rename`
/// preserves the mode, so the destination lands already restricted.
pub fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;

    let mut file = create_private(path)?;
    file.write_all(bytes)?;
    drop(file);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Open `path` for writing, creating or truncating it, owner-only from the
/// moment it exists: Unix `0600`, set at creation so the file is never briefly
/// world-readable. Windows relies on the config dir's ACL, as [`write_private`]
/// does.
pub(crate) fn create_private(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    opts.open(path)
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
    fn device_token_is_43_url_safe_chars() {
        let token = super::generate_device_token();
        assert_eq!(token.len(), 43, "{token}");
        assert!(
            token
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "base64url without padding: {token}"
        );
        assert_ne!(token, super::generate_device_token(), "each call is fresh");
    }

    #[test]
    fn pairing_code_is_six_digits() {
        for _ in 0..100 {
            let code = super::generate_pairing_code();
            assert_eq!(code.len(), 6, "{code}");
            assert!(code.chars().all(|c| c.is_ascii_digit()), "{code}");
        }
    }

    #[test]
    fn auth_tokens_are_48_alphanumeric_chars_and_fresh() {
        let token = super::generate_auth_token();
        assert_eq!(token.len(), 48, "{token}");
        assert!(token.chars().all(|c| c.is_ascii_alphanumeric()), "{token}");
        assert_ne!(token, super::generate_auth_token(), "each call is fresh");
    }

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
