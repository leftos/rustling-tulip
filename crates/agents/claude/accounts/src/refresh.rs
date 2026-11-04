//! Spending an account's refresh grant: the token POST ([`refresh_grant`]), the fingerprints that bind strikes and
//! stash rows, and the consume gate ([`consume_backup_grant`]) through which a backup account's grant is spent once.
//!
//! A refresh token is one-time-use. The gate holds the account's consume lock
//! across re-read, POST and compare-and-swap, so two consumers never POST the same grant; it re-reads under the account
//! lock and adopts a successor an earlier pass stashed; it POSTs
//! outside the account lock; and it writes the successor back only when the store still holds the generation it spent,
//! parking it in the unclaimed stash otherwise. A spent generation is never discarded.

pub(crate) mod oracle;
pub(crate) mod stash;

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Number, Value};
use sha2::{Digest, Sha256};
use zeroize::{Zeroize, Zeroizing};

use crate::drain::Drain;
use crate::locks::{AccountLock, AccountLockGuard, ConsumeLock, LockError, LockTiming};
use crate::paths::ClaudePaths;
use crate::poll::live_identity;
use crate::refresh::oracle::ProbeVerdicts;
use crate::refresh::stash::{ConfigSlot, StashFailed, StashWrite, Unrecorded};
use crate::secret::{CredentialBlob, RefreshToken};
use crate::store::{Retention, Store, StoreError, StoreErrorKind};
use crate::transport::{
    ProfileTransport, REFRESH_TIMEOUT, RawToken, RefreshRequest, RefreshTransport,
};
use crate::usage::Identity;

/// Why a refresh grant bought no new token, by name ([`RefreshError::kind`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RefreshError {
    /// The credential holds no refresh token: no retry can heal it. Permanent; nothing was posted.
    #[error("the stored credential has no refresh token; log in to the account again")]
    NoRefreshToken,
    /// The token endpoint refused the refresh token: this account's login lineage is dead. Permanent.
    #[error(
        "the token endpoint refused the refresh token (invalid_grant); log in to the account again"
    )]
    InvalidGrant,
    /// The token endpoint refused the client id: systemic, says nothing about the account, and strikes nothing.
    #[error("the token endpoint refused this client (invalid_client)")]
    InvalidClient,
    /// No verdict: the endpoint was unreachable, answered something else, or the credential could not be parsed.
    #[error("the token refresh failed for now; a later pass retries it")]
    Transient,
}

impl RefreshError {
    /// The error's name: `no_refresh_token`, `invalid_grant`, `invalid_client` or `transient`.
    #[must_use]
    pub fn kind(self) -> &'static str {
        match self {
            Self::NoRefreshToken => "no_refresh_token",
            Self::InvalidGrant => "invalid_grant",
            Self::InvalidClient => "invalid_client",
            Self::Transient => "transient",
        }
    }
}

/// The fingerprint of a stored credential: `sha256:` and the lowercase hex SHA-256 of the refresh token when the
/// text holds a non-empty one, else `sha256-full:` and the hash of the whole text. `None` only for empty text.
#[must_use]
pub fn credential_fingerprint(blob: &CredentialBlob) -> Option<String> {
    let text = blob.expose();
    if text.is_empty() {
        return None;
    }
    let parsed = Scrubbed::parse(text);
    match parsed
        .as_ref()
        .and_then(|parsed| parsed.oauth_string("refreshToken"))
    {
        Some(token) => Some(fingerprint("sha256:", token.as_bytes())),
        None => Some(fingerprint("sha256-full:", text.as_bytes())),
    }
}

/// The fingerprint of the access token alone, `sha256-at:` and its hex SHA-256: the part that rotates within a
/// lineage. `None` when the text holds no non-empty access token.
#[must_use]
pub fn access_token_fingerprint(blob: &CredentialBlob) -> Option<String> {
    let parsed = Scrubbed::parse(blob.expose())?;
    parsed
        .oauth_string("accessToken")
        .map(|token| fingerprint("sha256-at:", token.as_bytes()))
}

/// Whether the credential's OAuth block holds a non-empty refresh token, whatever its access token: a slot whose
/// access token a degenerate grant left empty is still refreshable.
pub(crate) fn has_refresh_token(blob: &CredentialBlob) -> bool {
    Scrubbed::parse(blob.expose())
        .is_some_and(|parsed| parsed.oauth_string("refreshToken").is_some())
}

/// Whether `candidate` is a newer generation of an account's credential than `stored`, the copy it would be written
/// over, so a stored copy is never rolled back:
/// - the same bytes are not newer;
/// - over a stored copy that holds a refresh token, has another fingerprint ([`credential_fingerprint`]) and whose
///   `expiresAt` is zero or missing, nothing is newer: its age cannot be told, so it is kept;
/// - otherwise `candidate` is newer only when its `expiresAt` is strictly later than the stored one's. A tie keeps the
///   stored copy, and so do two copies that both lack an expiry.
///
/// An `expiresAt` that is absent or not a number reads as 0, and so does a text that is not
/// JSON or holds no `claudeAiOauth` object.
#[must_use]
pub fn newer_generation(candidate: &CredentialBlob, stored: &CredentialBlob) -> bool {
    if candidate.expose() == stored.expose() {
        return false;
    }
    let stored_expiry = expiry_or_zero(stored);
    if stored_expiry <= 0.0
        && has_refresh_token(stored)
        && credential_fingerprint(candidate) != credential_fingerprint(stored)
    {
        return false;
    }
    expiry_or_zero(candidate) > stored_expiry
}

/// The credential's `expiresAt`, 0 when it is absent or not a number, or the text holds no OAuth block.
fn expiry_or_zero(blob: &CredentialBlob) -> f64 {
    Scrubbed::parse(blob.expose()).map_or(0.0, |parsed| parsed.expires_at())
}

/// `live` with its `claudeAiOauth` object replaced by `working`'s and every other key kept in its place, so what Claude
/// Code keeps beside its login (`mcpOAuth`, plugin secrets) stays the live file's own. `None` when `live` is not a JSON
/// object or `working` holds no `claudeAiOauth` object.
pub(crate) fn splice_oauth(
    live: &CredentialBlob,
    working: &CredentialBlob,
) -> Option<CredentialBlob> {
    let mut oauth = Scrubbed(Value::Object(
        Scrubbed::parse(working.expose())?.oauth()?.clone(),
    ));
    let mut spliced = Scrubbed::parse(live.expose())?;
    let map = spliced.0.as_object_mut()?;
    replace(map, "claudeAiOauth", std::mem::take(&mut oauth.0));
    spliced.to_blob(live.expose().len() + working.expose().len())
}

fn fingerprint(prefix: &str, bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut text = String::with_capacity(prefix.len() + 2 * digest.len());
    text.push_str(prefix);
    for byte in &digest {
        // Writing into a String cannot fail.
        let _infallible = write!(text, "{byte:02x}");
    }
    text
}

/// Spends `blob`'s refresh token at the token endpoint and returns the refreshed credential:
/// every key of the credential JSON is kept, `accessToken` and `expiresAt` (`now`
/// plus `expires_in` seconds, in milliseconds) are replaced, `refreshToken` only when the answer carries a non-empty
/// one, and `scope` becomes `scopes`, split on whitespace.
///
/// This is the bare POST. A backup account's grant is spent through [`consume_backup_grant`], which makes sure the
/// freshest generation is the one spent, and only once.
///
/// # Errors
///
/// [`RefreshError::NoRefreshToken`] without a POST when the OAuth block has no refresh token;
/// [`RefreshError::InvalidGrant`] and [`RefreshError::InvalidClient`] when a 400, 401 or 403 answer's top-level `error`
/// says so; [`RefreshError::Transient`] for everything else, a credential that is not a JSON object among them, and a
/// 2xx answer with neither a new refresh token nor a usable access token (the old refresh token is then still live). A
/// 2xx answer with a new refresh token is always a successor, with `accessToken` `""` standing in for a missing access
/// token and, for a missing or unusable expiry, an `expiresAt` of `now` or one millisecond after the spent copy's,
/// whichever is later: the spent generation's heir is kept, sorts after what it spent ([`newer_generation`]), and the
/// next pass refreshes it.
pub fn refresh_grant(
    transport: &dyn RefreshTransport,
    blob: &CredentialBlob,
    now: jiff::Timestamp,
) -> Result<RefreshedGrant, RefreshError> {
    refresh_grant_within(transport, blob, now, REFRESH_TIMEOUT)
}

/// What a 2xx token answer bought: the refreshed credential, and the account the endpoint said the token belongs to.
#[derive(Debug)]
#[non_exhaustive]
pub struct RefreshedGrant {
    /// The credential with the answer's tokens written into its OAuth block.
    pub credentials: CredentialBlob,
    /// The answer's `account` and `organization` objects, `None` when it carries neither.
    pub token_account: Option<TokenAccount>,
}

/// The account a token answer names as the token's owner, read leniently from its `account` (`uuid`, `email_address`)
/// and `organization` (`uuid`) objects: a missing, malformed or blank value is `None`, never an error, since the
/// identity is opportunistic. A `TokenAccount` is there whenever the answer carries either object, so it does not
/// promise a usable uuid; [`TokenAccount::account_uuid`] is the field a check needs. Its `Debug` hides the email.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct TokenAccount {
    /// The account's uuid, trimmed; `None` when absent, not a string or blank.
    pub account_uuid: Option<String>,
    /// The account's email address, when it is a string.
    pub email: Option<String>,
    /// The organization's uuid, when it is a string (possibly empty).
    pub organization_uuid: Option<String>,
}

impl TokenAccount {
    /// The identity in a token answer, `None` when it has neither an `account` nor an `organization` object.
    fn read(answer: &Map<String, Value>) -> Option<Self> {
        let account = answer.get("account").and_then(Value::as_object);
        let organization = answer.get("organization").and_then(Value::as_object);
        if account.is_none() && organization.is_none() {
            return None;
        }
        let text = |object: Option<&Map<String, Value>>, key: &str| {
            object?.get(key)?.as_str().map(str::to_owned)
        };
        Some(Self {
            account_uuid: text(account, "uuid")
                .map(|uuid| uuid.trim().to_owned())
                .filter(|uuid| !uuid.is_empty()),
            email: text(account, "email_address"),
            organization_uuid: text(organization, "uuid"),
        })
    }
}

impl std::fmt::Debug for TokenAccount {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TokenAccount")
            .field("account_uuid", &self.account_uuid)
            .field("email", &self.email.as_ref().map(|_| "<redacted>"))
            .field("organization_uuid", &self.organization_uuid)
            .finish()
    }
}

/// [`refresh_grant`] with the POST bounded by `timeout` ([`RefreshRequest::timeout`]).
pub(crate) fn refresh_grant_within(
    transport: &dyn RefreshTransport,
    blob: &CredentialBlob,
    now: jiff::Timestamp,
    timeout: Duration,
) -> Result<RefreshedGrant, RefreshError> {
    let Some(mut data) = Scrubbed::parse(blob.expose()).filter(|data| data.0.is_object()) else {
        return Err(RefreshError::Transient);
    };
    let Some(token) = data.oauth_string("refreshToken") else {
        return Err(RefreshError::NoRefreshToken);
    };
    let request = RefreshRequest::new(RefreshToken::new(token.to_owned()), timeout);
    let answer = transport
        .post_token(&request)
        .map_err(|_| RefreshError::Transient)?;
    let mut grant = classify(answer, now)?;
    let token_account = grant.token_account.take();
    grant.apply(&mut data, now)?;
    let credentials = data
        .to_blob(blob.expose().len())
        .ok_or(RefreshError::Transient)?;
    Ok(RefreshedGrant {
        credentials,
        token_account,
    })
}

/// A credential whose text is not JSON or holds no `claudeAiOauth` object, or an empty one.
pub(crate) struct NoOauthBlock;

/// The credential's `expiresAt` when it is a number:
/// `Ok(None)` when it is absent or not a number. An empty `claudeAiOauth` object counts as none.
pub(crate) fn oauth_expiry(blob: &CredentialBlob) -> Result<Option<f64>, NoOauthBlock> {
    let parsed = Scrubbed::parse(blob.expose()).ok_or(NoOauthBlock)?;
    let oauth = parsed
        .oauth()
        .filter(|oauth| !oauth.is_empty())
        .ok_or(NoOauthBlock)?;
    Ok(oauth.get("expiresAt").and_then(Value::as_f64))
}

/// The token endpoint's answer as a grant read at `now`, or the error its status and `error` member name.
fn classify(mut answer: RawToken, now: jiff::Timestamp) -> Result<TokenGrant, RefreshError> {
    let status = answer.status;
    let body = Scrubbed(std::mem::take(&mut answer.body));
    if (200..300).contains(&status) {
        return TokenGrant::read(&body.0, now).ok_or(RefreshError::Transient);
    }
    if matches!(status, 400 | 401 | 403) {
        match body.0.get("error").and_then(Value::as_str) {
            Some("invalid_grant") => return Err(RefreshError::InvalidGrant),
            Some("invalid_client") => return Err(RefreshError::InvalidClient),
            _ => {}
        }
    }
    Err(RefreshError::Transient)
}

/// The keys of a 2xx token answer the refresh writes back.
struct TokenGrant {
    access_token: Zeroizing<String>,
    /// `None` when the answer gave no usable expiry.
    expires_at: Option<Number>,
    refresh_token: Option<Zeroizing<String>>,
    scopes: Option<Vec<String>>,
    token_account: Option<TokenAccount>,
}

/// A truthy `scope` that is not a string: it cannot be split into scopes.
struct MalformedScope;

impl TokenGrant {
    /// The grant in `body`, read at `now`.
    ///
    /// The POST has already spent the old refresh token, so an answer carrying a new non-empty `refresh_token` is
    /// always a grant, whatever else it holds: a missing or empty `access_token` is written as `""` with no expiry of
    /// its own, a missing, non-numeric or out-of-range `expires_in` leaves no expiry either (the write stands one in,
    /// [`Self::apply`]), and a malformed `scope` is ignored, so the next pass refreshes the successor rather than losing
    /// it. Without a new refresh token the old one is still the live one, and the answer is a grant only with a usable access token: a non-empty `access_token`, a numeric
    /// `expires_in` and no malformed `scope`; otherwise `None`.
    fn read(body: &Value, now: jiff::Timestamp) -> Option<Self> {
        let answer = body.as_object()?;
        let refresh_token = answer
            .get("refresh_token")
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .map(|token| Zeroizing::new(token.to_owned()));
        let access_token = answer
            .get("access_token")
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty());
        let expires_at = match answer.get("expires_in") {
            Some(Value::Number(seconds)) => expires_at(seconds, now),
            _ => None,
        };
        let scopes = read_scopes(answer.get("scope"));
        if refresh_token.is_none()
            && (access_token.is_none() || expires_at.is_none() || scopes.is_err())
        {
            return None;
        }
        let (access_token, expires_at) = match access_token {
            Some(token) => (token, expires_at),
            None => ("", None),
        };
        Some(Self {
            access_token: Zeroizing::new(access_token.to_owned()),
            expires_at,
            refresh_token,
            scopes: scopes.ok().flatten(),
            token_account: TokenAccount::read(answer),
        })
    }

    /// Writes the grant into the credential's OAuth block, the spent copy's, read at `now`. A grant with no expiry of
    /// its own gets [`stand_in_expiry`], so it never stores `expiresAt` 0.
    fn apply(self, data: &mut Scrubbed, now: jiff::Timestamp) -> Result<(), RefreshError> {
        let expires_at = self
            .expires_at
            .unwrap_or_else(|| stand_in_expiry(data.expires_at(), now));
        let Some(Value::Object(oauth)) = data.0.get_mut("claudeAiOauth") else {
            return Err(RefreshError::Transient);
        };
        replace(
            oauth,
            "accessToken",
            Value::String(self.access_token.to_string()),
        );
        replace(oauth, "expiresAt", Value::Number(expires_at));
        if let Some(token) = &self.refresh_token {
            replace(oauth, "refreshToken", Value::String(token.to_string()));
        }
        if let Some(scopes) = self.scopes {
            replace(
                oauth,
                "scopes",
                Value::Array(scopes.into_iter().map(Value::String).collect()),
            );
        }
        Ok(())
    }
}

/// The `expiresAt` a successor whose answer gave no usable expiry is written with: `now` in milliseconds since the
/// epoch, or one millisecond after `spent`, the spent copy's `expiresAt` (0 when it had none), when that is later. The
/// successor so sorts after the generation it replaced ([`newer_generation`]) and, unless the spent copy had not yet
/// expired, reads as expired, so the next pass refreshes it.
fn stand_in_expiry(spent: f64, now: jiff::Timestamp) -> Number {
    let now_ms = now.as_millisecond();
    #[expect(
        clippy::cast_precision_loss,
        reason = "a millisecond timestamp is far inside f64's exact integer range"
    )]
    let now_f64 = now_ms as f64;
    let after_spent = spent + 1.0;
    if !(after_spent.is_finite() && after_spent > now_f64) {
        return Number::from(now_ms);
    }
    if after_spent.fract() == 0.0 && after_spent < 9.0e15 {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "a whole number under 2^53 converts exactly"
        )]
        return Number::from(after_spent as i64);
    }
    Number::from_f64(after_spent).unwrap_or_else(|| Number::from(now_ms))
}

/// A token answer's `scope` split on whitespace, `None` when absent or falsy.
fn read_scopes(scope: Option<&Value>) -> Result<Option<Vec<String>>, MalformedScope> {
    match scope {
        Some(Value::String(scope)) if !scope.is_empty() => {
            Ok(Some(scope.split_whitespace().map(str::to_owned).collect()))
        }
        Some(scope) if is_truthy(scope) => Err(MalformedScope),
        Some(_) | None => Ok(None),
    }
}

/// `now` plus `expires_in` seconds, in milliseconds since the epoch: a whole number for a whole `expires_in`, a float
/// for a float one.
fn expires_at(expires_in: &Number, now: jiff::Timestamp) -> Option<Number> {
    let now_ms = now.as_millisecond();
    if let Some(seconds) = expires_in.as_i64() {
        return seconds
            .checked_mul(1000)?
            .checked_add(now_ms)
            .map(Number::from);
    }
    if expires_in.is_u64() {
        return None;
    }
    #[expect(
        clippy::cast_precision_loss,
        reason = "a millisecond timestamp is far inside f64's exact integer range"
    )]
    let now_ms = now_ms as f64;
    Number::from_f64(now_ms + expires_in.as_f64()? * 1000.0)
}

/// Whether Python reads `value` as true: anything but null, false, zero and an empty string, array or object.
fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|number| number != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

/// Sets `key` in `map`, zeroing the value it replaces. A new key goes last; an existing one keeps its place.
fn replace(map: &mut Map<String, Value>, key: &str, value: Value) {
    if let Some(mut old) = map.insert(key.to_owned(), value) {
        scrub(&mut old);
    }
}

/// A parsed credential JSON tree whose strings are zeroed when it drops, since they hold tokens.
pub(crate) struct Scrubbed(pub(crate) Value);

impl Scrubbed {
    /// The tree `text` parses to, `None` when it is not JSON.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        serde_json::from_str(text).ok().map(Self)
    }

    /// The `claudeAiOauth` object, when the tree is an object holding one.
    fn oauth(&self) -> Option<&Map<String, Value>> {
        self.0.get("claudeAiOauth")?.as_object()
    }

    /// The OAuth block's `key` when it is a non-empty string.
    fn oauth_string(&self, key: &str) -> Option<&str> {
        self.oauth()?
            .get(key)?
            .as_str()
            .filter(|text| !text.is_empty())
    }

    /// The OAuth block's `expiresAt` as a number, 0 when absent or not a number.
    fn expires_at(&self) -> f64 {
        self.oauth()
            .and_then(|oauth| oauth.get("expiresAt"))
            .and_then(Value::as_f64)
            .unwrap_or(0.0)
    }

    /// The tree as compact JSON text in a credential blob. `hint` is the source text's length, to size the buffer.
    pub(crate) fn to_blob(&self, hint: usize) -> Option<CredentialBlob> {
        let mut bytes = Zeroizing::new(Vec::with_capacity(2 * hint + 4096));
        serde_json::to_writer(&mut *bytes, &self.0).ok()?;
        String::from_utf8(std::mem::take(&mut *bytes))
            .ok()
            .map(CredentialBlob::new)
    }
}

impl Drop for Scrubbed {
    fn drop(&mut self) {
        scrub(&mut self.0);
    }
}

/// An owned copy of a credential, itself zeroed when dropped.
pub(crate) fn copy(blob: &CredentialBlob) -> CredentialBlob {
    CredentialBlob::new(blob.expose().to_owned())
}

/// Zeroes every string in `value`, at any depth.
pub(crate) fn scrub(value: &mut Value) {
    match value {
        Value::String(text) => text.zeroize(),
        Value::Array(items) => items.iter_mut().for_each(scrub),
        Value::Object(map) => map.values_mut().for_each(scrub),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// Parks `live`, a live credential a switch is about to overwrite without backing it up, in the unclaimed stash, as a
/// direct activation does: reason `displaced-live-login`, filed under `slot` (sealed with `organization`, the
/// account's as the caller read it) and keyed to no generation, so no refresh ever adopts it; its bytes are kept and
/// never overwritten. Nothing is spent yet, so a newer store refuses it ([`StashWrite::BeforeSpend`]).
pub(crate) fn stash_displaced(
    context: &RefreshContext<'_>,
    live: &CredentialBlob,
    slot: ConfigSlot,
    organization: &str,
) -> Result<(), StashFailed> {
    let row = stash::Row {
        reason: "displaced-live-login",
        config_slot: slot,
        consumed_fp: None,
        fingerprint: credential_fingerprint(live),
        organization,
    };
    stash::write_unclaimed(
        context.store.root(),
        live,
        &row,
        (context.clock)(),
        StashWrite::BeforeSpend,
    )
    .map_err(|_| StashFailed)
}

/// What the consume gate needs from its caller besides the account.
pub struct RefreshContext<'a> {
    /// The account store.
    pub store: &'a Store,
    /// Claude Code's files: the gate refuses the account `~/.claude.json`'s `oauthAccount` names
    /// ([`GrantError::Active`]).
    pub claude: &'a ClaudePaths,
    /// The token endpoint.
    pub transport: &'a dyn RefreshTransport,
    /// The stop drain: a refresh takes a [`crate::WorkGuard`] only once its lock waits are over, just before the POST,
    /// and holds it until the successor is written or stashed, so a drain never waits out a lock wait.
    pub drain: &'a Arc<Drain>,
    /// Whether `CLAUDE_SECURESTORAGE_CONFIG_DIR` is set in the caller's environment: Claude Code then keeps its
    /// credentials in a store the account store does not mirror, and the gate refuses ([`GrantError::StoreUnmirrored`]).
    pub securestorage_dir_set: bool,
    /// The time now.
    pub clock: &'a dyn Fn() -> jiff::Timestamp,
    /// The profile endpoint, the ownership oracle the logged-in account's backup resync asks before writing a
    /// credential it cannot attribute by its bytes.
    pub profile: &'a dyn ProfileTransport,
    /// The ownership verdicts the oracle and this process's own refreshes gave, per credential lineage. Keep one for
    /// the life of the poller: it lives in memory only.
    pub verdicts: &'a ProbeVerdicts,
    /// How long Claude Code's credential and config locks are waited for when the logged-in account is refreshed:
    /// [`LockTiming::CLAUDE`] outside tests.
    pub claude_locks: LockTiming,
}

/// What an account's credential file looks like inside: whether it holds a
/// `claudeAiOauth` object, which of the two tokens are non-empty, and its `expiresAt` (0 when absent or not a number).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct OauthShape {
    pub(crate) access: bool,
    pub(crate) refresh: bool,
    pub(crate) expires_at: f64,
}

impl OauthShape {
    /// The shape of `blob`'s OAuth block, `None` when the text is not JSON or holds no `claudeAiOauth` object.
    pub(crate) fn of(blob: &CredentialBlob) -> Option<Self> {
        let parsed = Scrubbed::parse(blob.expose())?;
        parsed.oauth()?;
        Some(Self {
            access: parsed.oauth_string("accessToken").is_some(),
            refresh: parsed.oauth_string("refreshToken").is_some(),
            expires_at: parsed.expires_at(),
        })
    }

    /// Whether both tokens are there.
    pub(crate) fn full_pair(self) -> bool {
        self.access && self.refresh
    }
}

/// Why the consume gate handed back no freshened credential, by name
/// ([`GrantError::kind`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum GrantError {
    /// The stop drain has started: nothing was spent.
    Draining,
    /// `CLAUDE_SECURESTORAGE_CONFIG_DIR` is set: refused before any lock.
    StoreUnmirrored,
    /// Another consumer holds the account's consume lock past its 10 s wait; this pass yields.
    ConsumeBusy,
    /// A stashed successor, or the stash manifest, cannot be read, or a stashed entry does not match its row's
    /// fingerprint: deferred so its generation is not lost.
    StashUnreadable,
    /// The account is the active one, `accounts.json`'s or the one `~/.claude.json`'s `oauthAccount` names, as the
    /// gate found it under the account lock: Claude Code spends that grant, so nothing was spent.
    Active,
    /// The store refuses writes (a failed canary or a later `accounts.json` schema), so a successor could not be kept:
    /// refused before the drain guard, with nothing spent.
    StoreUnwritable,
    /// No verdict; nothing strikes. Also a spent grant whose successor was stashed rather than written.
    Transient,
    /// The credential has no refresh token (permanent).
    NoRefreshToken,
    /// The token endpoint refused the grant (permanent).
    InvalidGrant,
    /// The token endpoint refused the client id (systemic).
    InvalidClient,
}

impl GrantError {
    /// The outcome's name: `store-unmirrored`, `consume-busy`, `stash-unreadable`, `transient`,
    /// `no_refresh_token`, `invalid_grant`, `invalid_client`, `draining`,
    /// `active` or `store-unwritable`.
    #[must_use]
    pub fn kind(self) -> &'static str {
        match self {
            Self::Draining => "draining",
            Self::StoreUnmirrored => "store-unmirrored",
            Self::ConsumeBusy => "consume-busy",
            Self::StashUnreadable => "stash-unreadable",
            Self::Active => "active",
            Self::StoreUnwritable => "store-unwritable",
            Self::Transient => "transient",
            Self::NoRefreshToken => "no_refresh_token",
            Self::InvalidGrant => "invalid_grant",
            Self::InvalidClient => "invalid_client",
        }
    }
}

/// How the gate reports a credential write it could not make under the account lock: [`GrantError::StoreUnwritable`]
/// when the store refuses every write (a failed canary or a later `accounts.json` schema), as the gate's own pre-spend
/// check reports it, else [`GrantError::Transient`].
pub(crate) fn write_failure(error: &StoreError) -> GrantError {
    match error.kind() {
        StoreErrorKind::StoreReadOnly | StoreErrorKind::UnknownSchema => {
            GrantError::StoreUnwritable
        }
        _ => GrantError::Transient,
    }
}

impl From<RefreshError> for GrantError {
    fn from(error: RefreshError) -> Self {
        match error {
            RefreshError::NoRefreshToken => Self::NoRefreshToken,
            RefreshError::InvalidGrant => Self::InvalidGrant,
            RefreshError::InvalidClient => Self::InvalidClient,
            RefreshError::Transient => Self::Transient,
        }
    }
}

/// Why the gate parked a successor in the unclaimed stash instead of writing it, by its `consume-gate-*` reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum StashReason {
    /// The stored credential could not be read after the POST.
    StoreUnreadable,
    /// The account's credential disappeared during the POST: it is not resurrected.
    SlotRemoved,
    /// Another writer replaced the lineage during the POST: its credential is adopted.
    CasConflict,
    /// The account lock could not be taken after the POST.
    PersistLockFailed,
    /// Writing the successor failed.
    PersistFailed,
    /// Writing and stashing both failed: the successor survives only in the outcome.
    Unpersisted,
}

impl StashReason {
    /// The reason string, `consume-gate-…`.
    #[must_use]
    pub fn kind(self) -> &'static str {
        match self {
            Self::StoreUnreadable => "consume-gate-store-unreadable",
            Self::SlotRemoved => "consume-gate-slot-removed",
            Self::CasConflict => "consume-gate-cas-conflict",
            Self::PersistLockFailed => "consume-gate-persist-lock-failed",
            Self::PersistFailed => "consume-gate-persist-failed",
            Self::Unpersisted => "consume-gate-unpersisted",
        }
    }

    /// Whether the slot still holds the spent generation, so the outcome is demoted to [`GrantError::Transient`].
    fn demotes(self) -> bool {
        matches!(
            self,
            Self::PersistFailed
                | Self::PersistLockFailed
                | Self::Unpersisted
                | Self::StoreUnreadable
        )
    }
}

/// What one pass through the consume gate came to.
#[derive(Debug)]
pub struct GrantOutcome {
    /// The account's now-current credential: the successor, a racing writer's newer lineage, an adopted stash, or a
    /// successor that rides along with a demoted [`GrantError::Transient`].
    pub credentials: Option<CredentialBlob>,
    /// Why the credential is not a freshened, stored one; `None` on success.
    pub error: Option<GrantError>,
    /// The fingerprint of the generation the gate posted or would have, which a strike binds to; `None` when the gate
    /// stopped before reading the store.
    pub consumed_fp: Option<String>,
    /// Why the successor was stashed, when it was.
    pub stash: Option<StashReason>,
    /// The account the token endpoint named as the spent grant's owner: set only when this pass
    /// spent the grant (a demoted [`GrantError::Transient`] and a racing writer's adopted lineage included), `None` when
    /// it adopted a credential without a POST.
    pub token_account: Option<TokenAccount>,
}

impl GrantOutcome {
    fn failed(error: GrantError, consumed_fp: Option<String>) -> Self {
        Self {
            credentials: None,
            error: Some(error),
            consumed_fp,
            stash: None,
            token_account: None,
        }
    }
}

/// Spends account `number`'s backup refresh grant once, through the consume gate, and returns the account's
/// now-current credential. `snapshot` is the credential the caller read: the gate never spends it, only compares its
/// access token with the store's re-read.
///
/// In order: refuse when `CLAUDE_SECURESTORAGE_CONFIG_DIR` is set; take the consume lock (10 s); under the account
/// lock, refuse the active account, re-read the store, adopt a stashed successor and defer an empty slot; hand back a
/// re-read whose fresh access token differs from the snapshot's; refuse while the store would take no write (a failed
/// canary or a later `accounts.json` schema), before the drain guard; take a drain guard, past every lock wait; POST
/// outside the account lock; and write the successor under the account lock when the store
/// still holds the generation spent, stashing it otherwise. Never panics or errors once the grant is spent: every
/// outcome is a [`GrantOutcome`].
#[must_use]
pub fn consume_backup_grant(
    context: &RefreshContext<'_>,
    number: u32,
    snapshot: &CredentialBlob,
) -> GrantOutcome {
    if context.securestorage_dir_set {
        return GrantOutcome::failed(GrantError::StoreUnmirrored, None);
    }
    let _consume = match ConsumeLock::acquire(context.store.root(), number) {
        Ok(guard) => guard,
        Err(LockError::Timeout) => return GrantOutcome::failed(GrantError::ConsumeBusy, None),
        Err(_) => return GrantOutcome::failed(GrantError::Transient, None),
    };
    let slot = Slot::new(context, number);
    let (input, consumed_fp, organization) = match slot.prepare() {
        Ok(prepared) => prepared,
        Err(error) => return GrantOutcome::failed(error, None),
    };
    if slot.moved_past(&input, snapshot) {
        return GrantOutcome {
            credentials: Some(input),
            error: None,
            consumed_fp,
            stash: None,
            token_account: None,
        };
    }
    // Checked before the drain guard so a store that could not take the successor spends nothing and holds no guard. The
    // poll puts the account off on it, as it puts the logged-in account off on the active path's own pre-spend check.
    if context.store.write_refusal().is_some() {
        return GrantOutcome::failed(GrantError::StoreUnwritable, consumed_fp);
    }
    // Taken only now, past the consume and account lock waits, so the server's bounded drain never waits one out; it is
    // held through the POST and the persist.
    let Ok(_work) = context.drain.begin() else {
        return GrantOutcome::failed(GrantError::Draining, consumed_fp);
    };
    slot.spend(&input, consumed_fp, &organization)
}

/// One account inside the gate: its number.
pub(crate) struct Slot<'s, 'a> {
    context: &'s RefreshContext<'a>,
    number: u32,
}

impl<'s, 'a> Slot<'s, 'a> {
    pub(crate) fn new(context: &'s RefreshContext<'a>, number: u32) -> Self {
        Self { context, number }
    }

    /// Under the account lock: the generation to spend, its fingerprint and the account's organization as the roster
    /// names it (empty when unknown), after adopting a stash.
    fn prepare(&self) -> Result<(CredentialBlob, Option<String>, String), GrantError> {
        let lock =
            AccountLock::acquire(self.context.store.root()).map_err(|_| GrantError::Transient)?;
        let org_uuid = self.refuse_active()?;
        let current = self
            .context
            .store
            .read_credentials(self.number)
            .map_err(|_| GrantError::Transient)?;
        let adopted = stash::adopt(self, &lock, current.as_ref())?;
        let Some(input) = adopted.or(current) else {
            return Err(GrantError::Transient);
        };
        let consumed_fp = credential_fingerprint(&input);
        Ok((input, consumed_fp, org_uuid))
    }

    /// Under the account lock, before anything is read or written: [`GrantError::Active`] when the account is
    /// `accounts.json`'s active one or the one `~/.claude.json`'s `oauthAccount` names, since either may have changed
    /// since the caller looked. Otherwise the account's organization in `accounts.json`, empty when unknown.
    fn refuse_active(&self) -> Result<String, GrantError> {
        let Some(sequence) = self
            .context
            .store
            .read_sequence()
            .map_err(|_| GrantError::Transient)?
        else {
            return Ok(String::new());
        };
        if sequence.active() == Some(self.number) {
            return Err(GrantError::Active);
        }
        let Some(account) = sequence.account(self.number) else {
            return Ok(String::new());
        };
        if live_identity(self.context.claude).is_some_and(|live| live == Identity::of(&account)) {
            return Err(GrantError::Active);
        }
        Ok(account.org_uuid)
    }

    /// Spends `input`'s grant outside the account lock and persists the successor. A successor that has to be stashed
    /// is sealed with `organization`, read under the account lock before the POST.
    fn spend(
        &self,
        input: &CredentialBlob,
        consumed_fp: Option<String>,
        organization: &str,
    ) -> GrantOutcome {
        match refresh_grant(self.context.transport, input, (self.context.clock)()) {
            Ok(grant) => self.persist(grant, consumed_fp, organization),
            Err(error) => GrantOutcome::failed(error.into(), consumed_fp),
        }
    }

    /// Whether the world already moved past the caller's snapshot: the re-read's access token differs from it and is
    /// unexpired, so the refresh the caller wanted has happened.
    fn moved_past(&self, input: &CredentialBlob, snapshot: &CredentialBlob) -> bool {
        let (Some(input), Some(snapshot)) = (input.oauth(), snapshot.oauth()) else {
            return false;
        };
        input.access_token().expose() != snapshot.access_token().expose()
            && !input.is_expired((self.context.clock)())
    }

    /// Writes `blob` as the account's credential. Returns what the write did with the generation it replaced.
    pub(crate) fn write(
        &self,
        lock: &AccountLockGuard,
        blob: &CredentialBlob,
    ) -> Result<Retention, StoreError> {
        self.context
            .store
            .write_credentials(lock, self.number, blob)
    }

    /// Writes the successor back when the store still holds the generation spent, stashing it otherwise. A stash that
    /// kept its entry beside a manifest of a newer version ([`Unrecorded::Kept`]) is not repeated: the successor is
    /// already on disk once, and rides along in the outcome as [`StashReason::Unpersisted`].
    fn persist(
        &self,
        grant: RefreshedGrant,
        consumed_fp: Option<String>,
        organization: &str,
    ) -> GrantOutcome {
        let RefreshedGrant {
            credentials: successor,
            token_account,
        } = grant;
        let (stash, adopted) =
            match self.persist_locked(&successor, consumed_fp.as_deref(), organization) {
                Ok(result) => result,
                Err(Unrecorded::Kept) => (Some(StashReason::Unpersisted), None),
                Err(Unrecorded::Failed) => match self.stash(
                    &successor,
                    consumed_fp.as_deref(),
                    StashReason::PersistFailed,
                    organization,
                ) {
                    Ok(()) => (Some(StashReason::PersistFailed), None),
                    Err(_) => (Some(StashReason::Unpersisted), None),
                },
            };
        let error = stash
            .filter(|reason| reason.demotes())
            .map(|_| GrantError::Transient);
        GrantOutcome {
            credentials: Some(adopted.unwrap_or(successor)),
            error,
            consumed_fp,
            stash,
            token_account,
        }
    }

    /// The compare-and-swap under the account lock: the stash reason, if any, and the store's credential when a racing
    /// writer's is adopted. `Err` when the write or a stash failed, [`Unrecorded::Kept`] when a stash kept its entry
    /// without a row.
    fn persist_locked(
        &self,
        successor: &CredentialBlob,
        consumed_fp: Option<&str>,
        organization: &str,
    ) -> Result<(Option<StashReason>, Option<CredentialBlob>), Unrecorded> {
        let Ok(lock) = AccountLock::acquire(self.context.store.root()) else {
            self.stash(
                successor,
                consumed_fp,
                StashReason::PersistLockFailed,
                organization,
            )?;
            return Ok((Some(StashReason::PersistLockFailed), None));
        };
        let reason = match self.context.store.read_credentials(self.number) {
            Err(_) => StashReason::StoreUnreadable,
            Ok(None) => StashReason::SlotRemoved,
            Ok(Some(store_now)) if credential_fingerprint(&store_now).as_deref() != consumed_fp => {
                self.stash(
                    successor,
                    consumed_fp,
                    StashReason::CasConflict,
                    organization,
                )?;
                return Ok((Some(StashReason::CasConflict), Some(store_now)));
            }
            Ok(Some(_)) => {
                self.write(&lock, successor)
                    .map_err(|_| Unrecorded::Failed)?;
                return Ok((None, None));
            }
        };
        self.stash(successor, consumed_fp, reason, organization)?;
        Ok((Some(reason), None))
    }

    /// Parks `successor` in the unclaimed stash, keyed to the generation it superseded and sealed with `organization`,
    /// the account's as the caller already read it (empty when it could not be read): the stash never reads the roster.
    /// Every caller has spent a grant, so the write is a [`StashWrite::Rescue`], never refused on the store's version.
    pub(crate) fn stash(
        &self,
        successor: &CredentialBlob,
        consumed_fp: Option<&str>,
        reason: StashReason,
        organization: &str,
    ) -> Result<(), Unrecorded> {
        self.park(
            successor,
            consumed_fp,
            reason.kind(),
            organization,
            StashWrite::Rescue,
        )
    }

    /// Parks `live`, a live credential that is not the account's lineage, in the unclaimed stash before it is
    /// overwritten: reason `foreign`, keyed to no generation, so no refresh
    /// ever adopts it; its bytes are kept and never overwritten. `organization` is as [`Self::stash`] takes it. Nothing
    /// is spent yet, so a newer store refuses it ([`StashWrite::BeforeSpend`]).
    pub(crate) fn stash_foreign(
        &self,
        live: &CredentialBlob,
        organization: &str,
    ) -> Result<(), ()> {
        self.park(live, None, "foreign", organization, StashWrite::BeforeSpend)
            .map_err(|_| ())
    }

    fn park(
        &self,
        blob: &CredentialBlob,
        consumed_fp: Option<&str>,
        reason: &'static str,
        organization: &str,
        kind: StashWrite,
    ) -> Result<(), Unrecorded> {
        let row = stash::Row {
            reason,
            config_slot: ConfigSlot::Account(self.number),
            consumed_fp,
            fingerprint: credential_fingerprint(blob),
            organization,
        };
        stash::write_unclaimed(
            self.context.store.root(),
            blob,
            &row,
            (self.context.clock)(),
            kind,
        )
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{newer_generation, scrub};
    use crate::secret::CredentialBlob;

    /// A credential with `access`, an optional `refresh` token and an `expiresAt` of `expires_at` (absent when `None`).
    fn blob(access: &str, refresh: Option<&str>, expires_at: Option<Value>) -> CredentialBlob {
        let mut oauth = json!({ "accessToken": access });
        if let Some(refresh) = refresh {
            oauth["refreshToken"] = json!(refresh);
        }
        if let Some(expires_at) = expires_at {
            oauth["expiresAt"] = expires_at;
        }
        CredentialBlob::new(json!({ "claudeAiOauth": oauth }).to_string())
    }

    #[test]
    fn newer_generation_is_false_for_the_same_bytes() {
        let copy = blob("a", Some("r"), Some(json!(2000)));
        assert!(!newer_generation(
            &copy,
            &blob("a", Some("r"), Some(json!(2000)))
        ));
    }

    #[test]
    fn newer_generation_keeps_another_lineage_with_a_zero_expiry() {
        let stored = blob("s", Some("refresh-s"), Some(json!(0)));
        assert!(!newer_generation(
            &blob("p", Some("refresh-p"), Some(json!(2000))),
            &stored
        ));
    }

    #[test]
    fn newer_generation_keeps_another_lineage_with_no_expiry() {
        let stored = blob("s", Some("refresh-s"), None);
        assert!(!newer_generation(
            &blob("p", Some("refresh-p"), Some(json!(2000))),
            &stored
        ));
    }

    #[test]
    fn newer_generation_is_true_for_a_later_candidate() {
        let stored = blob("s", Some("refresh-s"), Some(json!(1000)));
        assert!(newer_generation(
            &blob("p", Some("refresh-p"), Some(json!(2000))),
            &stored
        ));
    }

    #[test]
    fn newer_generation_is_true_for_a_later_copy_of_the_same_lineage() {
        let stored = blob("a0", Some("refresh-a"), Some(json!(1000)));
        assert!(newer_generation(
            &blob("a1", Some("refresh-a"), Some(json!(2000))),
            &stored
        ));
    }

    #[test]
    fn newer_generation_lets_a_later_copy_of_the_same_lineage_replace_a_zero_expiry() {
        let stored = blob("a0", Some("refresh-a"), Some(json!(0)));
        assert!(newer_generation(
            &blob("a1", Some("refresh-a"), Some(json!(2000))),
            &stored
        ));
    }

    #[test]
    fn newer_generation_is_false_for_an_earlier_candidate() {
        let stored = blob("s", Some("refresh-s"), Some(json!(2000)));
        assert!(!newer_generation(
            &blob("p", Some("refresh-p"), Some(json!(1000))),
            &stored
        ));
    }

    #[test]
    fn newer_generation_keeps_the_stored_copy_on_a_tie() {
        let stored = blob("s", Some("refresh-s"), Some(json!(2000)));
        assert!(!newer_generation(
            &blob("p", Some("refresh-p"), Some(json!(2000))),
            &stored
        ));
    }

    #[test]
    fn newer_generation_keeps_the_stored_copy_when_both_lack_an_expiry() {
        let stored = blob("s", None, None);
        assert!(!newer_generation(
            &blob("p", Some("refresh-p"), None),
            &stored
        ));
    }

    #[test]
    fn newer_generation_is_false_for_a_candidate_with_no_expiry_over_a_real_one() {
        let stored = blob("s", Some("refresh-s"), Some(json!(2000)));
        assert!(!newer_generation(
            &blob("p", Some("refresh-p"), None),
            &stored
        ));
    }

    #[test]
    fn newer_generation_lets_a_real_expiry_replace_a_stored_copy_with_no_refresh_token_and_no_expiry()
     {
        let stored = blob("s", None, None);
        assert!(newer_generation(
            &blob("p", Some("refresh-p"), Some(json!(2000))),
            &stored
        ));
    }

    #[test]
    fn newer_generation_reads_a_non_number_expiry_as_zero() {
        let stored = blob("s", Some("refresh-s"), Some(json!("soon")));
        assert!(!newer_generation(
            &blob("p", Some("refresh-p"), Some(json!(2000))),
            &stored
        ));
    }

    #[test]
    fn scrub_zeroes_every_string_at_every_depth() {
        let mut tree = json!({
            "top": "secret-top",
            "list": ["secret-one", "secret-two"],
            "nested": {"inner": "secret-deep"},
            "count": 7,
            "flag": true,
        });

        scrub(&mut tree);

        assert_eq!(tree["top"], "");
        assert_eq!(tree["list"][0], "");
        assert_eq!(tree["list"][1], "");
        assert_eq!(tree["nested"]["inner"], "");
        assert_eq!(tree["count"], json!(7));
        assert_eq!(tree["flag"], json!(true));
    }
}
