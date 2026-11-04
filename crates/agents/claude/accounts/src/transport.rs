//! The HTTP half of a usage read and of a token refresh, left to the caller: this crate has no HTTP client. The caller
//! implements [`UsageTransport`] with its own client and sends the usage request, and
//! [`RefreshTransport`] for the token refresh POST.

use std::fmt;
use std::time::Duration;

use serde_json::Value;
use zeroize::Zeroizing;

use crate::secret::{AccessToken, RefreshToken, Secret};

/// The usage endpoint: `GET` with the account's access token as a bearer token.
pub const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
/// The beta header the usage endpoint requires, as `(name, value)`.
pub const USAGE_BETA_HEADER: (&str, &str) = ("anthropic-beta", "oauth-2025-04-20");
/// How long the usage endpoint is waited for before giving up.
pub const USAGE_TIMEOUT: Duration = Duration::from_secs(5);
/// The prepaid-credits endpoint: `GET` with the account's access token as a bearer token and [`USAGE_BETA_HEADER`],
/// answering the organization's prepaid extra-usage balance. `{org_uuid}` stands for the organization's UUID; build the
/// URL with [`prepaid_url`], which refuses anything but a canonical UUID.
pub const PREPAID_URL_TEMPLATE: &str =
    "https://api.anthropic.com/api/oauth/organizations/{org_uuid}/prepaid/credits";

/// `template` (such as [`PREPAID_URL_TEMPLATE`]) with `{org_uuid}` replaced by `org_uuid`, or `None` when `org_uuid` is
/// not a canonical UUID: 36 characters, hex digits with hyphens at positions 8, 13, 18 and 23. Nothing else ever reaches
/// the URL.
#[must_use]
pub fn prepaid_url(template: &str, org_uuid: &str) -> Option<String> {
    is_org_uuid(org_uuid).then(|| template.replace("{org_uuid}", org_uuid))
}

/// Whether `org_uuid` is a canonical UUID: 36 characters, hex digits with hyphens at positions 8, 13, 18 and 23.
pub(crate) fn is_org_uuid(org_uuid: &str) -> bool {
    org_uuid.len() == 36
        && org_uuid.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

/// One answer from the usage endpoint, or from the prepaid-credits endpoint.
#[derive(Clone, Debug, PartialEq)]
pub struct RawUsage {
    /// The HTTP status.
    pub status: u16,
    /// The body parsed as JSON; `Value::Null` for an empty or unparseable body on a status other than 2xx.
    pub body: Value,
    /// The `Retry-After` header in seconds, when sent in its seconds form.
    pub retry_after: Option<f64>,
}

/// Why the usage endpoint gave no answer. It carries no body, header or token.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum TransportError {
    /// The request ran past [`USAGE_TIMEOUT`].
    #[error("the usage endpoint did not answer in time")]
    Timeout,
    /// The connection could not be made or broke (DNS, TLS, refused, reset).
    #[error("the usage endpoint could not be reached")]
    Connect,
    /// A 2xx answer whose body is not JSON.
    #[error("the usage endpoint's answer was not JSON")]
    MalformedBody,
    /// The transport does not send this request: it has no prepaid-credits client, or it refused the request before
    /// sending it (an organization UUID that is not canonical). No request was made.
    #[error("the transport does not send this request")]
    Unsupported,
}

/// Sends the usage request for one account.
pub trait UsageTransport {
    /// `GET` [`USAGE_URL`] with `Authorization: Bearer <token>` (marked sensitive, never logged) and
    /// [`USAGE_BETA_HEADER`], within [`USAGE_TIMEOUT`]. Every HTTP answer, whatever its status, is `Ok` as a
    /// [`RawUsage`] carrying its `Retry-After`, so a 429's wait reaches the backoff: a client that turns a status into
    /// an error (such as reqwest's `error_for_status`) must not be used here, and no [`TransportError`] stands for one.
    ///
    /// # Errors
    ///
    /// [`TransportError`] when no answer came, or a 2xx answer's body is not JSON.
    fn get_usage(&self, token: &AccessToken) -> Result<RawUsage, TransportError>;

    /// `GET` the prepaid-credits endpoint of organization `org_uuid` ([`prepaid_url`] over [`PREPAID_URL_TEMPLATE`])
    /// with `Authorization: Bearer <token>` (marked sensitive, never logged) and [`USAGE_BETA_HEADER`], within
    /// [`USAGE_TIMEOUT`], from the same client as [`UsageTransport::get_usage`]. Every HTTP answer is `Ok`, as for the
    /// usage request. The poll calls it only right after a usage read succeeded, with that read's token.
    ///
    /// The default sends nothing and answers [`TransportError::Unsupported`], which the poll takes as "no balance read
    /// happened".
    ///
    /// # Errors
    ///
    /// [`TransportError::Unsupported`] when the transport does not send it, or refuses an `org_uuid` that is not a
    /// canonical UUID; otherwise as [`UsageTransport::get_usage`].
    fn get_prepaid(
        &self,
        _token: &AccessToken,
        _org_uuid: &str,
    ) -> Result<RawUsage, TransportError> {
        Err(TransportError::Unsupported)
    }
}

/// The token endpoint: `POST` a refresh grant as JSON, get a new access token back.
pub const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
/// The OAuth client id Claude Code sends with every refresh grant.
pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
/// How long the token endpoint is waited for before giving up.
pub const REFRESH_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the logged-in account's refresh POST may take: it runs under the store's account lock, which a switch waits
/// 10 s for, so it is bounded well inside that.
pub const LOCKED_REFRESH_TIMEOUT: Duration = Duration::from_secs(6);

/// One answer from the token endpoint. Its `Debug` prints the status only, and every string in its body is zeroed when
/// it drops: a 2xx body carries tokens.
pub struct RawToken {
    /// The HTTP status.
    pub status: u16,
    /// The body parsed as JSON; `Value::Null` for an empty or unparseable body on a status other than 2xx.
    pub body: Value,
}

impl fmt::Debug for RawToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RawToken")
            .field("status", &self.status)
            .field("body", &"<redacted>")
            .finish()
    }
}

impl Drop for RawToken {
    fn drop(&mut self) {
        crate::refresh::scrub(&mut self.body);
    }
}

/// One refresh grant to send. It holds the refresh token, so its `Debug` prints nothing of it and its body is reached
/// only through [`RefreshRequest::json_body`].
pub struct RefreshRequest {
    refresh_token: RefreshToken,
    timeout: Duration,
}

impl RefreshRequest {
    pub(crate) fn new(refresh_token: RefreshToken, timeout: Duration) -> Self {
        Self {
            refresh_token,
            timeout,
        }
    }

    /// How long the POST may take before it is given up: [`REFRESH_TIMEOUT`] for an inactive account's grant,
    /// [`LOCKED_REFRESH_TIMEOUT`] for the logged-in account's, which is spent under the store's account lock.
    #[must_use]
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// The JSON body to `POST`: `{"grant_type":"refresh_token","refresh_token":…,"client_id":…}`, in that order, as
    /// secret text zeroed when dropped.
    #[must_use]
    pub fn json_body(&self) -> Secret {
        #[derive(serde::Serialize)]
        struct Body<'a> {
            grant_type: &'a str,
            refresh_token: &'a str,
            client_id: &'a str,
        }
        let token = self.refresh_token.expose();
        let body = Body {
            grant_type: "refresh_token",
            refresh_token: token,
            client_id: CLIENT_ID,
        };
        // Sized for the worst-case escaping of every byte, so the buffer never reallocates and leaves no copy behind.
        let mut bytes = Zeroizing::new(Vec::with_capacity(128 + 6 * token.len()));
        if serde_json::to_writer(&mut *bytes, &body).is_err() {
            return Secret::new(String::new());
        }
        match String::from_utf8(std::mem::take(&mut *bytes)) {
            Ok(text) => Secret::new(text),
            Err(error) => {
                // serde_json writes UTF-8, so this never runs; the bytes it hands back are zeroed all the same.
                drop(Zeroizing::new(error.into_bytes()));
                Secret::new(String::new())
            }
        }
    }
}

impl fmt::Debug for RefreshRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RefreshRequest(<redacted>)")
    }
}

/// Sends a refresh grant for one account.
pub trait RefreshTransport {
    /// `POST` [`TOKEN_URL`] with `Content-Type: application/json` and [`RefreshRequest::json_body`] (never logged),
    /// within [`RefreshRequest::timeout`], which the implementation reads for each request. Every HTTP answer, whatever
    /// its status, is `Ok` as a [`RawToken`], so a 400's `error` member reaches the classifier: a client that turns a
    /// status into an error must not be used here.
    ///
    /// The implementation caps the response body it reads at 64 KiB, so an endpoint cannot make it buffer without
    /// bound, and the request body it serialises from [`RefreshRequest::json_body`] is its own to zero once sent: any
    /// copy it makes (a byte buffer, a client's request body) holds the refresh token.
    ///
    /// # Errors
    ///
    /// [`TransportError`] when no answer came, or a 2xx answer's body is not JSON.
    fn post_token(&self, request: &RefreshRequest) -> Result<RawToken, TransportError>;
}

/// The profile endpoint: `GET` with an access token as a bearer token, answering whose token it is.
pub const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
/// How long the profile endpoint is waited for before giving up.
pub const PROFILE_TIMEOUT: Duration = Duration::from_secs(5);
/// The `User-Agent` sent to the profile endpoint.
pub const PROFILE_USER_AGENT: &str = "rustling-tulip/1.0";

/// One answer from the profile endpoint. Its body names an account (uuid, email, organization) and holds no token; its
/// `Debug` shows the status alone.
#[derive(Clone, PartialEq)]
pub struct RawProfile {
    /// The HTTP status.
    pub status: u16,
    /// The body parsed as JSON; `Value::Null` for an empty or unparseable body.
    pub body: Value,
}

impl fmt::Debug for RawProfile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RawProfile")
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

/// Asks the profile endpoint whose access token a credential holds: the ownership oracle the logged-in account's
/// backup resync consults before it writes a credential it cannot attribute by its bytes. It is never called while a
/// lock is held.
pub trait ProfileTransport {
    /// `GET` [`PROFILE_URL`] with `Authorization: Bearer <token>` (marked sensitive, never logged),
    /// `Content-Type: application/json` and `User-Agent:` [`PROFILE_USER_AGENT`], within [`PROFILE_TIMEOUT`]. Every
    /// HTTP answer, whatever its status, is `Ok` as a [`RawProfile`]; only a 2xx one resolves an account.
    ///
    /// The implementation caps the response body it reads at 64 KiB, so an endpoint cannot make it buffer without
    /// bound.
    ///
    /// # Errors
    ///
    /// [`TransportError`] when no answer came, or a 2xx answer's body is not JSON.
    fn get_profile(&self, access: &AccessToken) -> Result<RawProfile, TransportError>;
}
