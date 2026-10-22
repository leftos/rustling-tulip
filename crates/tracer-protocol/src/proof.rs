//! The daemon proof: how the hook shim makes a loopback listener prove it
//! holds this run's daemon token before the shim sends it anything else.
//!
//! The shim sends 32 random challenge bytes; the daemon answers
//! `HMAC-SHA256(proof_key, challenge)`, where `proof_key` is
//! `HMAC-SHA256(auth_token, DAEMON_PROOF_CONTEXT)`, derived once per daemon
//! run. Both sides call these functions, so the derivation lives once.

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// The message the proof key is derived from, so a proof key is never the
/// token itself and is tied to this one purpose and version.
pub const DAEMON_PROOF_CONTEXT: &[u8] = b"rustling-tulip/daemon-proof/v1";

/// An HMAC-SHA256 instance keyed with `key`.
fn keyed(key: &[u8]) -> HmacSha256 {
    let Ok(mac) = HmacSha256::new_from_slice(key) else {
        unreachable!("HMAC accepts a key of any length")
    };
    mac
}

/// `HMAC-SHA256(key = auth_token, msg = DAEMON_PROOF_CONTEXT)`: the key a
/// daemon run answers challenges with, derived once from its auth token.
#[must_use]
pub fn daemon_proof_key(auth_token: &str) -> [u8; 32] {
    let mut mac = keyed(auth_token.as_bytes());
    mac.update(DAEMON_PROOF_CONTEXT);
    mac.finalize().into_bytes().into()
}

/// `HMAC-SHA256(proof_key, challenge)`: the daemon's answer to a challenge.
#[must_use]
pub fn daemon_proof(proof_key: &[u8; 32], challenge: &[u8; 32]) -> [u8; 32] {
    let mut mac = keyed(proof_key);
    mac.update(challenge);
    mac.finalize().into_bytes().into()
}

/// Whether `proof` answers `challenge` under `proof_key`, compared in
/// constant time.
#[must_use]
pub fn verify_daemon_proof(proof_key: &[u8; 32], challenge: &[u8; 32], proof: &[u8; 32]) -> bool {
    let mut mac = keyed(proof_key);
    mac.update(challenge);
    mac.verify_slice(proof).is_ok()
}

/// `bytes` as 64 lowercase hex characters.
#[must_use]
pub fn to_lower_hex(bytes: &[u8; 32]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(64);
    for byte in bytes {
        text.push(char::from(DIGITS[usize::from(byte >> 4)]));
        text.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    text
}

/// Parses exactly 64 characters of `0-9a-f`; anything else (uppercase,
/// whitespace, a `0x` prefix, another length) is `None`.
#[must_use]
pub fn parse_lower_hex32(text: &str) -> Option<[u8; 32]> {
    let digits = text.as_bytes();
    if digits.len() != 64 {
        return None;
    }
    let mut bytes = [0u8; 32];
    let (pairs, _) = digits.as_chunks::<2>();
    for (byte, &[high, low]) in bytes.iter_mut().zip(pairs) {
        *byte = (lower_hex_digit(high)? << 4) | lower_hex_digit(low)?;
    }
    Some(bytes)
}

/// The proof a daemon's answer to `GET /proof` carries, when its head is one
/// the daemon would send; `None` refuses it.
///
/// `head` is the status line and headers, without the blank line that ends
/// them. It must be a `204` with exactly one `x-rt-proof` header (name matched
/// case-insensitively) holding 64 lowercase hex, announce no body (any
/// `Content-Length` is `0`, no `Transfer-Encoding`), and every header line
/// must have a `:`. The caller still verifies the proof against its challenge
/// with [`verify_daemon_proof`].
#[must_use]
pub fn parse_daemon_proof_head(head: &str) -> Option<[u8; 32]> {
    let mut lines = head.split("\r\n");
    let status = lines.next()?.split_whitespace().nth(1)?;
    if status != "204" {
        return None;
    }
    let mut proofs = Vec::new();
    for line in lines {
        let (name, value) = line.split_once(':')?;
        let value = value.trim();
        if name.eq_ignore_ascii_case("x-rt-proof") {
            proofs.push(value);
        } else if !header_allows_no_body(name, value) {
            return None;
        }
    }
    let [proof] = proofs.as_slice() else {
        return None;
    };
    parse_lower_hex32(proof)
}

/// Whether a header leaves the proof answer bodiless: a `Content-Length` must
/// be `0`, and any `Transfer-Encoding` announces a body.
fn header_allows_no_body(name: &str, value: &str) -> bool {
    if name.eq_ignore_ascii_case("content-length") {
        return value == "0";
    }
    !name.eq_ignore_ascii_case("transfer-encoding")
}

/// The value of one lowercase hex digit.
fn lower_hex_digit(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests assert preconditions with expect")]
mod tests {
    use super::*;

    const KNOWN_KEY: &str = "3d8e5de218cc1727ff9d1df559b29b0c1d49befff55fb8f2b43daeb8eb0f5c99";
    const KNOWN_PROOF: &str = "9f4157efeb1af10ab080dfd713d7bac5d49821070cd3a18628288186da9ff9d6";

    fn counting_challenge() -> [u8; 32] {
        let mut challenge = [0u8; 32];
        for (byte, value) in challenge.iter_mut().zip(0u8..) {
            *byte = value;
        }
        challenge
    }

    #[test]
    fn daemon_proof_matches_the_known_answer() {
        let key = daemon_proof_key("test-token");
        assert_eq!(to_lower_hex(&key), KNOWN_KEY);

        let challenge = counting_challenge();
        let proof = daemon_proof(&key, &challenge);
        assert_eq!(to_lower_hex(&proof), KNOWN_PROOF);
        assert!(verify_daemon_proof(&key, &challenge, &proof));

        let mut wrong = proof;
        wrong[31] ^= 1;
        assert!(!verify_daemon_proof(&key, &challenge, &wrong));
        let other_key = daemon_proof_key("other-token");
        assert!(!verify_daemon_proof(&other_key, &challenge, &proof));
    }

    #[test]
    fn parse_lower_hex32_accepts_only_64_lowercase_hex() {
        let parsed = parse_lower_hex32(KNOWN_PROOF).expect("64 lowercase hex parses");
        assert_eq!(to_lower_hex(&parsed), KNOWN_PROOF);

        let upper = KNOWN_PROOF.to_ascii_uppercase();
        let short = &KNOWN_PROOF[..63];
        let long = format!("{KNOWN_PROOF}0");
        let spaced = format!("{} ", &KNOWN_PROOF[..63]);
        let prefixed = format!("0x{}", &KNOWN_PROOF[..62]);
        let leading_space = format!(" {}", &KNOWN_PROOF[..63]);
        let non_hex = format!("{}g", &KNOWN_PROOF[..63]);
        for text in [
            upper.as_str(),
            short,
            long.as_str(),
            spaced.as_str(),
            prefixed.as_str(),
            leading_space.as_str(),
            non_hex.as_str(),
            "",
        ] {
            assert_eq!(parse_lower_hex32(text), None, "{text:?} must be refused");
        }
    }
}
