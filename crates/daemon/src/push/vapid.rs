//! The VAPID identity (RFC 8292): the daemon's P-256 key and the ES256 JWT
//! each push carries, cached per push service origin.

use std::collections::HashMap;

use anyhow::Context;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, TimeDelta, Utc};
use p256::SecretKey;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use p256::elliptic_curve::Generate;
use p256::elliptic_curve::sec1::ToSec1Point;
use serde::Serialize;

/// How long a signed token is reused for one push service: Apple asks for a
/// new one no more than once an hour.
const RESIGN_AFTER: TimeDelta = TimeDelta::hours(1);

/// How far past signing a token's `exp` lies: under the day RFC 8292 allows.
const LIFETIME: TimeDelta = TimeDelta::hours(12);

/// The daemon's VAPID key pair.
pub(crate) struct VapidKey(SecretKey);

impl VapidKey {
    /// A new random key from the system's random source.
    ///
    /// # Errors
    ///
    /// The operating system's random source failed.
    pub(crate) fn generate() -> anyhow::Result<Self> {
        SecretKey::try_generate()
            .map(Self)
            .map_err(|err| anyhow::anyhow!("the OS random source failed: {err}"))
    }

    /// The key whose private scalar is `raw` (big-endian), as [`Self::to_raw`]
    /// wrote it.
    ///
    /// # Errors
    ///
    /// `raw` is zero or not below the curve order.
    pub(crate) fn from_raw(raw: &[u8; 32]) -> anyhow::Result<Self> {
        SecretKey::from_slice(raw)
            .map(Self)
            .map_err(|_| anyhow::anyhow!("the stored VAPID key is not a P-256 private key"))
    }

    /// The private scalar, big-endian.
    pub(crate) fn to_raw(&self) -> [u8; 32] {
        self.0.to_bytes().into()
    }

    /// The 65-octet uncompressed public point, base64url without padding:
    /// the `k=` of the `Authorization` header and the page's
    /// `applicationServerKey`.
    pub(crate) fn public_b64url(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.0.public_key().to_sec1_point(false).as_bytes())
    }
}

#[derive(Serialize)]
struct Header {
    typ: &'static str,
    alg: &'static str,
}

#[derive(Serialize)]
struct Claims<'a> {
    aud: &'a str,
    exp: i64,
    sub: &'a str,
}

/// The VAPID JWT for `audience` (the push service's origin) from `subject`
/// (the phone app's origin), expiring at `exp_unix`, signed ES256.
///
/// # Errors
///
/// The header or claims did not serialize.
pub(crate) fn sign_jwt(
    key: &VapidKey,
    audience: &str,
    subject: &str,
    exp_unix: i64,
) -> anyhow::Result<String> {
    let header = serde_json::to_vec(&Header {
        typ: "JWT",
        alg: "ES256",
    })
    .context("the JWT header did not serialize")?;
    let claims = serde_json::to_vec(&Claims {
        aud: audience,
        exp: exp_unix,
        sub: subject,
    })
    .context("the JWT claims did not serialize")?;
    let signing_input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header),
        URL_SAFE_NO_PAD.encode(claims)
    );
    let signature: Signature = SigningKey::from(&key.0).sign(signing_input.as_bytes());
    Ok(format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    ))
}

struct Cached {
    token: String,
    subject: String,
    signed_at: DateTime<Utc>,
}

/// One signed token per signing key and push service origin, re-signed at
/// most once an hour.
#[derive(Default)]
pub(crate) struct JwtCache {
    /// Keyed by the signing key's public point (base64url) and the audience.
    tokens: HashMap<(String, String), Cached>,
}

impl JwtCache {
    /// The token for `audience` at `now`: the cached one while it was signed
    /// under an hour ago with `key` for the same `subject`, else a new one
    /// expiring twelve hours from `now`. `public_key` is `key`'s
    /// [`VapidKey::public_b64url`], which the caller computes once.
    ///
    /// # Errors
    ///
    /// Signing failed ([`sign_jwt`]).
    pub(crate) fn token(
        &mut self,
        key: &VapidKey,
        public_key: &str,
        audience: &str,
        subject: &str,
        now: DateTime<Utc>,
    ) -> anyhow::Result<String> {
        let slot = (public_key.to_owned(), audience.to_owned());
        if let Some(cached) = self.tokens.get(&slot) {
            let age = now - cached.signed_at;
            if cached.subject == subject && age >= TimeDelta::zero() && age < RESIGN_AFTER {
                return Ok(cached.token.clone());
            }
        }
        let token = sign_jwt(key, audience, subject, (now + LIFETIME).timestamp())?;
        self.tokens.insert(
            slot,
            Cached {
                token: token.clone(),
                subject: subject.to_owned(),
                signed_at: now,
            },
        );
        Ok(token)
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::{JwtCache, TimeDelta, URL_SAFE_NO_PAD, VapidKey};
    use base64::Engine;
    use chrono::{DateTime, TimeZone, Utc};
    use p256::ecdsa::signature::Verifier;
    use p256::ecdsa::{Signature, VerifyingKey};
    use serde_json::Value;

    const APPLE: &str = "https://web.push.apple.com";
    const MOZILLA: &str = "https://updates.push.services.mozilla.com";
    const SUBJECT: &str = "https://host.tail1234.ts.net:7443";

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 6, 9, 30, 0)
            .single()
            .expect("a valid time")
    }

    fn b64(text: &str) -> Vec<u8> {
        URL_SAFE_NO_PAD.decode(text).expect("valid base64url")
    }

    /// The token's header and claims as text, and its signature checked
    /// against `key`'s public key.
    fn open(token: &str, key: &VapidKey) -> (String, Value) {
        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3, "{token}");
        let verifying =
            VerifyingKey::from_sec1_bytes(&b64(&key.public_b64url())).expect("a public point");
        let signature = Signature::from_slice(&b64(parts[2])).expect("a 64-octet r||s");
        verifying
            .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
            .expect("the signature verifies");
        let header = String::from_utf8(b64(parts[0])).expect("utf-8");
        let claims = serde_json::from_slice(&b64(parts[1])).expect("JSON claims");
        (header, claims)
    }

    #[test]
    fn jwt_verifies_and_carries_the_designed_claims() {
        let key = VapidKey::from_raw(&VapidKey::generate().expect("a key").to_raw())
            .expect("round-trips");
        assert_eq!(b64(&key.public_b64url()).len(), 65);
        let token = JwtCache::default()
            .token(&key, &key.public_b64url(), APPLE, SUBJECT, now())
            .expect("signs");

        let (header, claims) = open(&token, &key);
        assert_eq!(header, r#"{"typ":"JWT","alg":"ES256"}"#);
        let claims_text =
            String::from_utf8(b64(token.split('.').nth(1).expect("claims"))).expect("utf-8");
        assert!(claims_text.starts_with(r#"{"aud":"#), "{claims_text}");
        assert_eq!(claims["aud"], APPLE);
        assert_eq!(claims["sub"], SUBJECT);
        let exp = claims["exp"].as_i64().expect("a numeric exp");
        let ahead = exp - now().timestamp();
        assert!(ahead > 0 && ahead < 24 * 60 * 60, "exp is {ahead} s ahead");
    }

    #[test]
    fn cache_reuses_inside_the_hour_and_resigns_at_it() {
        let key = VapidKey::generate().expect("a key");
        let mut cache = JwtCache::default();
        let first = cache
            .token(&key, &key.public_b64url(), APPLE, SUBJECT, now())
            .expect("signs");
        let inside = now() + TimeDelta::minutes(59) + TimeDelta::seconds(59);
        assert_eq!(
            cache
                .token(&key, &key.public_b64url(), APPLE, SUBJECT, inside)
                .expect("signs"),
            first
        );

        let at_hour = cache
            .token(
                &key,
                &key.public_b64url(),
                APPLE,
                SUBJECT,
                now() + TimeDelta::hours(1),
            )
            .expect("signs");
        assert_ne!(at_hour, first);
        let (_, claims) = open(&at_hour, &key);
        assert_eq!(
            claims["exp"].as_i64(),
            Some((now() + TimeDelta::hours(13)).timestamp())
        );

        let other_subject = cache
            .token(
                &key,
                &key.public_b64url(),
                APPLE,
                "https://other.tail1234.ts.net:7443",
                inside,
            )
            .expect("signs");
        assert_ne!(other_subject, at_hour, "a new subject re-signs");
    }

    #[test]
    fn each_audience_gets_its_own_token() {
        let key = VapidKey::generate().expect("a key");
        let mut cache = JwtCache::default();
        let apple = cache
            .token(&key, &key.public_b64url(), APPLE, SUBJECT, now())
            .expect("signs");
        let mozilla = cache
            .token(&key, &key.public_b64url(), MOZILLA, SUBJECT, now())
            .expect("signs");
        assert_ne!(apple, mozilla);
        assert_eq!(open(&apple, &key).1["aud"], APPLE);
        assert_eq!(open(&mozilla, &key).1["aud"], MOZILLA);
        assert_eq!(
            cache
                .token(&key, &key.public_b64url(), APPLE, SUBJECT, now())
                .expect("signs"),
            apple
        );
    }

    #[test]
    fn a_new_key_inside_the_hour_gets_its_own_token() {
        let old_key = VapidKey::generate().expect("a key");
        let new_key = VapidKey::generate().expect("a key");
        let mut cache = JwtCache::default();
        let old = cache
            .token(&old_key, &old_key.public_b64url(), APPLE, SUBJECT, now())
            .expect("signs");
        let inside = now() + TimeDelta::minutes(30);
        let new = cache
            .token(&new_key, &new_key.public_b64url(), APPLE, SUBJECT, inside)
            .expect("signs");
        assert_ne!(
            new, old,
            "a token signed with another key is never returned"
        );
        open(&new, &new_key);
    }

    /// The push relay's test fixture is this signer's own output: ES256 in
    /// `p256` is deterministic (RFC 6979), so a fixed key, audience, subject
    /// and `exp` always give the same token.
    #[test]
    fn relay_fixture_is_the_daemons_jwt() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../../services/push-relay/test/fixtures/daemon-vapid.json"
        ))
        .expect("the fixture is JSON");
        let key = VapidKey::from_raw(&[1u8; 32]).expect("a valid scalar");
        assert_eq!(fixture["key_raw_hex"], "01".repeat(32).as_str());
        let aud = fixture["aud"].as_str().expect("aud");
        let sub = fixture["sub"].as_str().expect("sub");
        let exp = fixture["exp"].as_i64().expect("exp");
        let public = key.public_b64url();
        let jwt = super::sign_jwt(&key, aud, sub, exp).expect("signs");
        assert_eq!(
            fixture["k"],
            public.as_str(),
            "computed k={public} jwt={jwt}"
        );
        assert_eq!(fixture["jwt"], jwt.as_str(), "computed jwt={jwt}");
    }
}
