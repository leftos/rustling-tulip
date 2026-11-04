//! Secret text held in memory: zeroed when dropped, never cloned, printed, displayed or serialized.

use std::fmt;

use zeroize::Zeroizing;

/// A secret string. Its memory is zeroed on drop; `Debug` prints `Secret(<redacted>)`; it has no `Clone`, `Display`
/// or `Serialize`, so the only way to the text is [`Secret::expose`].
pub struct Secret(Zeroizing<String>);

impl Secret {
    /// Takes ownership of the text; it is zeroed when the secret is dropped.
    #[must_use]
    pub fn new(text: String) -> Self {
        Self(Zeroizing::new(text))
    }

    /// The secret text, for the one call that must send or write it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Secret(<redacted>)")
    }
}

/// One account's whole credential file as cswap stores it, held opaquely: the JSON Claude Code keeps in
/// `.credentials.json`, or a raw API key for an `api_key` account.
pub struct CredentialBlob(Secret);

impl CredentialBlob {
    /// Takes ownership of the credential text.
    #[must_use]
    pub fn new(text: String) -> Self {
        Self(Secret::new(text))
    }

    /// The credential text.
    #[must_use]
    pub fn expose(&self) -> &str {
        self.0.expose()
    }

    /// The `claudeAiOauth` block Claude Code keeps in its credential file, read straight into secret types. `None` when
    /// the text is not JSON (an API key), has no `claudeAiOauth` object, or that object has no non-empty access token.
    #[must_use]
    pub fn oauth(&self) -> Option<OauthCredentials> {
        let file: OauthFile = serde_json::from_str(self.expose()).ok()?;
        let block = file.claude_ai_oauth?;
        let access_token = block.access_token.filter(|token| !token.is_empty())?;
        Some(OauthCredentials {
            access_token: AccessToken(Secret(access_token)),
            refresh_token: block
                .refresh_token
                .filter(|token| !token.is_empty())
                .map(|token| RefreshToken(Secret(token))),
            expires_at: block.expires_at.as_ref().and_then(millis_timestamp),
            refresh_token_expires_at: block
                .refresh_token_expires_at
                .as_ref()
                .and_then(millis_timestamp),
        })
    }
}

/// How long before `expiresAt` an access token already counts as expired: cswap's and Claude Code's five minutes.
pub const OAUTH_EXPIRY_BUFFER: jiff::SignedDuration = jiff::SignedDuration::from_mins(5);

/// A credential file's `claudeAiOauth` block: the tokens and when they lapse. Its `Debug` prints no token; it has no
/// `Clone`, `Display` or `Serialize`.
pub struct OauthCredentials {
    access_token: AccessToken,
    refresh_token: Option<RefreshToken>,
    expires_at: Option<jiff::Timestamp>,
    refresh_token_expires_at: Option<jiff::Timestamp>,
}

impl OauthCredentials {
    /// The access token (`accessToken`).
    #[must_use]
    pub fn access_token(&self) -> &AccessToken {
        &self.access_token
    }

    /// The refresh token (`refreshToken`), if one is stored.
    #[must_use]
    pub fn refresh_token(&self) -> Option<&RefreshToken> {
        self.refresh_token.as_ref()
    }

    /// When the access token lapses (`expiresAt`, milliseconds since the epoch), if known.
    #[must_use]
    pub fn expires_at(&self) -> Option<jiff::Timestamp> {
        self.expires_at
    }

    /// When the login itself lapses (`refreshTokenExpiresAt`, milliseconds since the epoch), if known.
    #[must_use]
    pub fn refresh_token_expires_at(&self) -> Option<jiff::Timestamp> {
        self.refresh_token_expires_at
    }

    /// Whether the access token has lapsed at `now` or will within [`OAUTH_EXPIRY_BUFFER`], as cswap's
    /// `is_oauth_token_expired` decides. An unknown expiry is not expired.
    #[must_use]
    pub fn is_expired(&self, now: jiff::Timestamp) -> bool {
        self.expires_at.is_some_and(|expires_at| {
            now.checked_add(OAUTH_EXPIRY_BUFFER)
                .ok()
                .is_none_or(|soon| soon >= expires_at)
        })
    }
}

impl fmt::Debug for OauthCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OauthCredentials")
            .field("access_token", &self.access_token)
            .field("refresh_token", &self.refresh_token)
            .field("expires_at", &self.expires_at)
            .field("refresh_token_expires_at", &self.refresh_token_expires_at)
            .finish()
    }
}

/// A credential file, of which only `claudeAiOauth` is read; every other key is skipped unread.
#[derive(serde::Deserialize)]
struct OauthFile {
    #[serde(rename = "claudeAiOauth", default)]
    claude_ai_oauth: Option<OauthBlock>,
}

/// The `claudeAiOauth` keys cswap reads. The tokens go straight into zeroizing strings; the two expiries are numbers
/// when Claude Code wrote them, and anything else is read as unknown rather than failing the whole block.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct OauthBlock {
    #[serde(default)]
    access_token: Option<Zeroizing<String>>,
    #[serde(default)]
    refresh_token: Option<Zeroizing<String>>,
    #[serde(default)]
    expires_at: Option<serde_json::Value>,
    #[serde(default)]
    refresh_token_expires_at: Option<serde_json::Value>,
}

/// A JSON number of milliseconds since the epoch as a timestamp, truncated to whole milliseconds as cswap's `int()`
/// does; `None` for anything else or a time out of range.
fn millis_timestamp(value: &serde_json::Value) -> Option<jiff::Timestamp> {
    let millis = match value.as_i64() {
        Some(millis) => millis,
        None => whole_millis(value.as_f64()?)?,
    };
    jiff::Timestamp::from_millisecond(millis).ok()
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "the value is checked to be finite and inside i64's range first"
)]
fn whole_millis(millis: f64) -> Option<i64> {
    const LIMIT: f64 = 9.2e18;
    (millis.is_finite() && millis.abs() < LIMIT).then(|| millis.trunc() as i64)
}

impl fmt::Debug for CredentialBlob {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CredentialBlob(<redacted>)")
    }
}

/// An OAuth access token, sent as a bearer token.
pub struct AccessToken(Secret);

impl AccessToken {
    /// Takes ownership of the token text.
    #[must_use]
    pub fn new(text: String) -> Self {
        Self(Secret::new(text))
    }

    /// The token text.
    #[must_use]
    pub fn expose(&self) -> &str {
        self.0.expose()
    }
}

impl fmt::Debug for AccessToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AccessToken(<redacted>)")
    }
}

/// An OAuth refresh token: one use buys a new access token and a new refresh token.
pub struct RefreshToken(Secret);

impl RefreshToken {
    /// Takes ownership of the token text.
    #[must_use]
    pub fn new(text: String) -> Self {
        Self(Secret::new(text))
    }

    /// The token text.
    #[must_use]
    pub fn expose(&self) -> &str {
        self.0.expose()
    }
}

impl fmt::Debug for RefreshToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RefreshToken(<redacted>)")
    }
}
