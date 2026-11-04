//! The plugin's account service over the `claude-accounts` store, the three
//! blocking HTTP transports the store's polls, refreshes and profile checks go
//! through, and the process table its engine checks lease holders against.
//!
//! Every reqwest client here is blocking: it is built and dropped inside the
//! call that sends, which the daemon runs off its async threads.

use std::collections::BTreeMap;
use std::io::{self, Read};
use std::time::Duration;

use agent_api::{
    AccountKindRow, AccountRow, AccountsConfig, AccountsError, AccountsService, AccountsSnapshot,
    UsageRow, UsageStatus, UsageWindowRow,
};
use chrono::{DateTime, Utc};
use claude_accounts::{
    AccessToken, AccountKind, AccountRecord, AccountsRoot, Identity, PROFILE_TIMEOUT, PROFILE_URL,
    PROFILE_USER_AGENT, ProfileTransport, RawProfile, RawToken, RawUsage, RefreshRequest,
    RefreshTransport, Secret, Store, StoreError, TOKEN_URL, TransportError, USAGE_BETA_HEADER,
    USAGE_TIMEOUT, USAGE_URL, UsageEntry, UsageTable, UsageTransport, UsageWindow,
};
use reqwest::blocking::{Body, Client, RequestBuilder};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue, RETRY_AFTER, USER_AGENT};
use reqwest::redirect::Policy;
use serde_json::Value;
use zeroize::Zeroizing;

/// The most of a response body a transport reads: a longer body is not read
/// on, and counts as one that does not parse.
pub const BODY_CAP: usize = 64 * 1024;

/// The account service over the store at an [`AccountsConfig`]'s root. It
/// opens the store on each call, so it shares nothing between calls.
pub struct ClaudeAccounts {
    cfg: AccountsConfig,
}

impl ClaudeAccounts {
    /// The service over the store `cfg` names.
    #[must_use]
    pub fn new(cfg: AccountsConfig) -> Self {
        Self { cfg }
    }
}

impl AccountsService for ClaudeAccounts {
    fn snapshot(&self) -> Result<AccountsSnapshot, AccountsError> {
        let store = Store::open(AccountsRoot::new(self.cfg.root.clone()))
            .map_err(|error| store_error(&error))?;
        let Some(sequence) = store.read_sequence().map_err(|error| store_error(&error))? else {
            return Ok(AccountsSnapshot::default());
        };
        // The rotation order first, then every record it leaves out, by number.
        let order = sequence.numbers();
        let records: Vec<AccountRecord> = order
            .iter()
            .filter_map(|&number| sequence.account(number))
            .chain(
                sequence
                    .every_record()
                    .filter(|record| !order.contains(&record.number))
                    .cloned(),
            )
            .collect();
        let identities: BTreeMap<u32, Identity> = records
            .iter()
            .map(|record| (record.number, Identity::of(record)))
            .collect();
        let mut usage = UsageTable::entries(store.root(), &identities);
        let active = sequence.active();
        let accounts = records
            .into_iter()
            .map(|record| {
                let entry = usage.remove(&record.number).unwrap_or_default();
                account_row(record, active, &entry)
            })
            .collect();
        Ok(AccountsSnapshot {
            accounts,
            auto: None,
        })
    }
}

/// A store error as the seam's error.
fn store_error(error: &StoreError) -> AccountsError {
    AccountsError::new(format!("the account store could not be read: {error}"))
}

/// The `lastError` names of a poll whose access token was refused and could
/// not be refreshed: a 401, and a 401 the refresh could not heal.
const TOKEN_EXPIRED_ERRORS: [&str; 2] = ["http-401", "refresh-failed"];

/// The `lastError` names of a dead refresh lineage, which only a new login
/// heals.
const RELOGIN_ERRORS: [&str; 2] = ["invalid_grant", "no_refresh_token"];

/// What the usage `entry` of an OAuth account says: the latest poll's failure
/// when it failed, else [`UsageStatus::Ok`] with a saved reading and
/// [`UsageStatus::Unavailable`] without one.
fn oauth_status(entry: &UsageEntry) -> UsageStatus {
    if entry.consecutive_failures > 0
        && let Some(error) = entry.last_error.as_deref()
    {
        return if TOKEN_EXPIRED_ERRORS.contains(&error) {
            UsageStatus::TokenExpired
        } else if RELOGIN_ERRORS.contains(&error) {
            UsageStatus::ReloginRequired
        } else {
            UsageStatus::Unavailable
        };
    }
    if entry.last_good.is_some() || entry.five_hour.is_some() || entry.seven_day.is_some() {
        UsageStatus::Ok
    } else {
        UsageStatus::Unavailable
    }
}

/// `record` as a row, with the usage `entry` saved for it. An API-key account
/// reads as [`UsageStatus::ApiKey`]; an OAuth account as [`oauth_status`].
fn account_row(record: AccountRecord, active: Option<u32>, entry: &UsageEntry) -> AccountRow {
    let kind = if record.kind == AccountKind::ApiKey {
        AccountKindRow::ApiKey
    } else {
        AccountKindRow::Oauth
    };
    let status = if kind == AccountKindRow::ApiKey {
        UsageStatus::ApiKey
    } else {
        oauth_status(entry)
    };
    AccountRow {
        number: record.number,
        email: record.email.as_str().to_owned(),
        alias: record.alias,
        org_name: Some(record.org_name).filter(|name| !name.is_empty()),
        kind,
        active: active == Some(record.number),
        disabled: record.disabled,
        usage: UsageRow {
            status,
            five_hour: entry.five_hour.as_ref().map(window_row),
            seven_day: entry.seven_day.as_ref().map(window_row),
            model_limits: Vec::new(),
            extra_usage: None,
            fetched_at: entry
                .fetched_at
                .and_then(|at| chrono_time(at.as_nanosecond())),
            error: entry.last_error.clone(),
        },
    }
}

fn window_row(window: &UsageWindow) -> UsageWindowRow {
    UsageWindowRow {
        pct: window.pct,
        resets_at: window
            .resets_at
            .and_then(|at| chrono_time(at.as_nanosecond())),
    }
}

/// The time `nanos` nanoseconds past the epoch (before it when negative),
/// `None` when chrono cannot hold it.
fn chrono_time(nanos: i128) -> Option<DateTime<Utc>> {
    // Euclidean division keeps the sub-second part in 0..1e9 before the epoch.
    let second = i64::try_from(nanos.div_euclid(1_000_000_000)).ok()?;
    let subsec = u32::try_from(nanos.rem_euclid(1_000_000_000)).ok()?;
    DateTime::from_timestamp(second, subsec)
}

/// The process table the account engine checks a lease holder against: the
/// live Windows table where the store's locks work, an empty one elsewhere,
/// where a crashed holder's lease waits out its heartbeat.
#[must_use]
pub fn process_table() -> Box<dyn proc_table::ProcessTable + Send + Sync> {
    #[cfg(all(windows, target_pointer_width = "64"))]
    {
        Box::new(proc_table::Win32ProcessTable)
    }
    #[cfg(not(all(windows, target_pointer_width = "64")))]
    {
        Box::new(proc_table::EmptyTable)
    }
}

/// `real` (an `https://host/path` endpoint) at `origin` in place of its own
/// scheme and host, or `real` itself without one.
fn endpoint(real: &str, origin: Option<&str>) -> String {
    let Some(origin) = origin else {
        return real.to_owned();
    };
    let path = real
        .find("://")
        .and_then(|at| {
            let rest = &real[at + 3..];
            rest.find('/').map(|slash| &rest[slash..])
        })
        .unwrap_or("/");
    format!("{}{path}", origin.trim_end_matches('/'))
}

/// A client that waits `timeout` for the whole exchange, follows no redirect,
/// uses no proxy, and speaks only HTTPS unless an origin override is set.
fn client(timeout: Duration, origin: Option<&str>) -> Result<Client, TransportError> {
    Client::builder()
        .timeout(timeout)
        .connect_timeout(timeout)
        .redirect(Policy::none())
        .no_proxy()
        .https_only(origin.is_none())
        .build()
        .map_err(|_| TransportError::Connect)
}

/// `Bearer <token>` as a header value marked sensitive, so reqwest never
/// shows it. A token that cannot be a header value is refused unsent.
fn bearer(token: &AccessToken) -> Result<HeaderValue, TransportError> {
    let text = Secret::new(format!("Bearer {}", token.expose()));
    let mut value =
        HeaderValue::from_str(text.expose()).map_err(|_| TransportError::Unsupported)?;
    value.set_sensitive(true);
    Ok(value)
}

/// One answer as read: its status, its `Retry-After` in seconds, and its
/// body parsed (`Null` for an empty, oversized or unparseable body other than
/// a 2xx one).
struct Answer {
    status: u16,
    retry_after: Option<f64>,
    body: Value,
}

/// Sends `request` and reads its answer, whatever its status, reading at
/// most [`BODY_CAP`] of the body.
fn exchange(request: RequestBuilder) -> Result<Answer, TransportError> {
    let response = request.send().map_err(|error| request_error(&error))?;
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|text| text.trim().parse::<f64>().ok())
        .filter(|secs| secs.is_finite() && *secs >= 0.0);
    let mut bytes = Zeroizing::new(Vec::with_capacity(BODY_CAP + 1));
    response
        .take(u64::try_from(BODY_CAP + 1).unwrap_or(u64::MAX))
        .read_to_end(&mut bytes)
        .map_err(|error| read_error(&error))?;
    let parsed = if bytes.len() > BODY_CAP {
        None
    } else {
        serde_json::from_slice::<Value>(&bytes).ok()
    };
    let body = match parsed {
        Some(body) => body,
        None if (200..300).contains(&status) => return Err(TransportError::MalformedBody),
        None => Value::Null,
    };
    Ok(Answer {
        status,
        retry_after,
        body,
    })
}

/// A failed send as a transport error.
fn request_error(error: &reqwest::Error) -> TransportError {
    if error.is_timeout() {
        TransportError::Timeout
    } else {
        TransportError::Connect
    }
}

/// A failed body read as a transport error.
fn read_error(error: &io::Error) -> TransportError {
    let timed_out = error.kind() == io::ErrorKind::TimedOut
        || error
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<reqwest::Error>())
            .is_some_and(reqwest::Error::is_timeout);
    if timed_out {
        TransportError::Timeout
    } else {
        TransportError::Connect
    }
}

/// The usage endpoint over a blocking reqwest client.
pub struct UsageClient {
    origin: Option<String>,
}

impl UsageClient {
    /// The client for the real endpoint, or for `origin` in its place.
    #[must_use]
    pub fn new(origin: Option<&str>) -> Self {
        Self {
            origin: origin.map(str::to_owned),
        }
    }
}

impl UsageTransport for UsageClient {
    fn get_usage(&self, token: &AccessToken) -> Result<RawUsage, TransportError> {
        let origin = self.origin.as_deref();
        let client = client(USAGE_TIMEOUT, origin)?;
        let (beta_name, beta_value) = USAGE_BETA_HEADER;
        let answer = exchange(
            client
                .get(endpoint(USAGE_URL, origin))
                .header(AUTHORIZATION, bearer(token)?)
                .header(beta_name, beta_value),
        )?;
        Ok(RawUsage {
            status: answer.status,
            body: answer.body,
            retry_after: answer.retry_after,
        })
    }
}

/// The token endpoint over a blocking reqwest client.
pub struct RefreshClient {
    origin: Option<String>,
}

impl RefreshClient {
    /// The client for the real endpoint, or for `origin` in its place.
    #[must_use]
    pub fn new(origin: Option<&str>) -> Self {
        Self {
            origin: origin.map(str::to_owned),
        }
    }
}

/// A request body read from secret text, which is zeroed when the client
/// drops the body.
struct SecretBody {
    text: Secret,
    at: usize,
}

impl Read for SecretBody {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let rest = self.text.expose().as_bytes().get(self.at..).unwrap_or(&[]);
        let count = rest.len().min(buf.len());
        buf[..count].copy_from_slice(&rest[..count]);
        self.at += count;
        Ok(count)
    }
}

impl RefreshTransport for RefreshClient {
    fn post_token(&self, request: &RefreshRequest) -> Result<RawToken, TransportError> {
        let origin = self.origin.as_deref();
        let client = client(request.timeout(), origin)?;
        let text = request.json_body();
        let length = u64::try_from(text.expose().len()).unwrap_or(u64::MAX);
        let body = Body::sized(SecretBody { text, at: 0 }, length);
        let answer = exchange(
            client
                .post(endpoint(TOKEN_URL, origin))
                .header(CONTENT_TYPE, "application/json")
                .body(body),
        )?;
        Ok(RawToken {
            status: answer.status,
            body: answer.body,
        })
    }
}

/// The profile endpoint over a blocking reqwest client.
pub struct ProfileClient {
    origin: Option<String>,
}

impl ProfileClient {
    /// The client for the real endpoint, or for `origin` in its place.
    #[must_use]
    pub fn new(origin: Option<&str>) -> Self {
        Self {
            origin: origin.map(str::to_owned),
        }
    }
}

impl ProfileTransport for ProfileClient {
    fn get_profile(&self, access: &AccessToken) -> Result<RawProfile, TransportError> {
        let origin = self.origin.as_deref();
        let client = client(PROFILE_TIMEOUT, origin)?;
        let answer = exchange(
            client
                .get(endpoint(PROFILE_URL, origin))
                .header(AUTHORIZATION, bearer(access)?)
                .header(CONTENT_TYPE, "application/json")
                .header(USER_AGENT, PROFILE_USER_AGENT),
        )?;
        Ok(RawProfile {
            status: answer.status,
            body: answer.body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chrono_time_converts_a_pre_epoch_time() {
        let before = chrono_time(-1_500_000_000).map(|at| at.to_rfc3339());
        let after = chrono_time(1_500_000_000).map(|at| at.to_rfc3339());

        assert_eq!(before.as_deref(), Some("1969-12-31T23:59:58.500+00:00"));
        assert_eq!(after.as_deref(), Some("1970-01-01T00:00:01.500+00:00"));
    }
}
