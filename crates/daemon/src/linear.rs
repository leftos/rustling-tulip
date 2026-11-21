//! The daemon's Linear client: the API key, the workspace's team keys and
//! `urlKey`, batched issue fetches and the shared issue cache.
//!
//! Everything here is a plain function over a [`LinearTransport`] and a
//! `now` the caller passes in, so tests drive it with a fake transport and
//! fixed times. With no `LINEAR_API_KEY` the client makes no request at all.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use futures::future::BoxFuture;
use protocol::{LinearIssue, LinearState, LinearStateKind};
use reqwest::header::{AUTHORIZATION, HeaderValue};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tracing::warn;

use crate::user_env::{self, Secret};

/// Linear's GraphQL endpoint.
const ENDPOINT: &str = "https://api.linear.app/graphql";
/// The variable the API key is read from.
const KEY_VARIABLE: &str = "LINEAR_API_KEY";
/// The variable that takes the daemon off Linear: exactly `1` resolves no
/// key at all, so no request is ever made.
pub(crate) const OFFLINE_VARIABLE: &str = "RUSTLING_TULIP_LINEAR_OFFLINE";
/// How long one request to Linear may take, end to end.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// The most identifiers one issue query asks for.
const BATCH_SIZE: usize = 50;
/// How long the team keys and `urlKey` stay current: under the 5-minute
/// tick, so every tick fetches them again and a new team is seen within it.
const CONTEXT_MAX_AGE: TimeDelta = TimeDelta::minutes(4);
/// How long a cached issue stays current; an older copy is fetched again.
const ISSUE_MAX_AGE: TimeDelta = TimeDelta::minutes(1);
/// The workspace's team keys, its name and its `urlKey`.
const CONTEXT_QUERY: &str =
    "query Context { teams { nodes { key } } organization { name urlKey } }";
/// The Test button's query: what the context query asks, under its own name.
const TEST_QUERY: &str = "query Test { teams { nodes { key } } organization { name urlKey } }";
/// The GraphQL error code Linear answers a refused API key with.
const AUTHENTICATION_ERROR: &str = "AUTHENTICATION_ERROR";
/// The fields one issue query asks for, for each issue.
const ISSUE_FIELDS: &str = "identifier title url priority priorityLabel updatedAt \
     state { name type color } project { name } assignee { name } releases { nodes { name } }";
/// The text before the workspace in a Linear issue URL.
const URL_HOST: &str = "linear.app/";
/// The text between the workspace and the identifier in a Linear issue URL.
const URL_ISSUE: &str = "/issue/";

/// What went wrong talking to Linear.
#[derive(Debug, thiserror::Error)]
pub(crate) enum LinearError {
    /// Linear answered with a status outside 2xx.
    #[error("Linear answered HTTP {0}")]
    Status(u16),
    /// The request did not complete: no connection, a timeout, or a reply
    /// that is not JSON.
    #[error("the request to Linear failed")]
    Transport(#[source] reqwest::Error),
    /// The API key cannot be sent as a header value.
    #[error("the Linear API key is not a valid header value")]
    BadKey,
    /// The reply carried a GraphQL `errors` array.
    #[error("Linear reported an error: {0}")]
    GraphQl(String),
    /// The reply's `data` is missing or not the shape asked for.
    #[error("Linear's reply has an unexpected shape: {0}")]
    Shape(String),
    /// Linear refused the API key: HTTP 401 or 403, or a GraphQL error
    /// whose `extensions.code` is `AUTHENTICATION_ERROR`.
    #[error("Linear refused the API key")]
    Rejected,
}

impl LinearError {
    /// Whether Linear refused the key itself.
    pub(crate) fn is_rejected(&self) -> bool {
        matches!(self, Self::Rejected)
    }
}

/// A failed request as the wiring keeps it: the error's display text and
/// whether Linear refused the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Failure {
    pub(crate) text: String,
    pub(crate) rejected: bool,
}

impl Failure {
    pub(crate) fn of(err: &LinearError) -> Self {
        Self {
            text: err.to_string(),
            rejected: err.is_rejected(),
        }
    }
}

/// The error for an HTTP status outside 2xx: 401 and 403 are a refused key.
fn status_error(status: u16) -> LinearError {
    match status {
        401 | 403 => LinearError::Rejected,
        other => LinearError::Status(other),
    }
}

/// How a request reaches Linear's GraphQL endpoint: posts `body` (a JSON
/// `{query, variables}` object) with `key` and hands back the reply's JSON.
pub(crate) trait LinearTransport: Send + Sync {
    fn post<'a>(
        &'a self,
        key: &'a Secret,
        body: Value,
    ) -> BoxFuture<'a, Result<Value, LinearError>>;
}

/// The transport that posts to Linear over HTTPS.
pub(crate) struct ReqwestTransport {
    http: reqwest::Client,
}

impl ReqwestTransport {
    /// A transport whose requests time out after 10 s.
    ///
    /// # Errors
    ///
    /// The TLS setup failed, for one when the system's root certificates
    /// cannot be loaded.
    pub(crate) fn new() -> Result<Self, reqwest::Error> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()?;
        Ok(Self { http })
    }
}

impl LinearTransport for ReqwestTransport {
    fn post<'a>(
        &'a self,
        key: &'a Secret,
        body: Value,
    ) -> BoxFuture<'a, Result<Value, LinearError>> {
        Box::pin(async move {
            let response = self
                .http
                .post(ENDPOINT)
                .header(AUTHORIZATION, header_value(key)?)
                .json(&body)
                .send()
                .await
                .map_err(LinearError::Transport)?;
            let status = response.status();
            if !status.is_success() {
                return Err(status_error(status.as_u16()));
            }
            response.json().await.map_err(LinearError::Transport)
        })
    }
}

/// `key` as the `Authorization` header Linear takes: the key itself, with no
/// `Bearer`, marked sensitive so it never shows in a debug print.
fn header_value(key: &Secret) -> Result<HeaderValue, LinearError> {
    let mut value = HeaderValue::from_str(key.expose()).map_err(|_| LinearError::BadKey)?;
    value.set_sensitive(true);
    Ok(value)
}

/// The workspace's team keys, `urlKey` and name.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct LinearContextData {
    pub(crate) team_keys: Vec<String>,
    pub(crate) workspace: Option<String>,
    pub(crate) workspace_name: Option<String>,
}

/// The context with the time it was fetched.
#[derive(Debug)]
struct FetchedContext {
    data: LinearContextData,
    fetched_at: DateTime<Utc>,
}

/// The issues fetched so far, keyed by identifier.
#[derive(Debug, Default)]
pub(crate) struct LinearCache {
    issues: HashMap<String, LinearIssue>,
}

impl LinearCache {
    /// Store `issues`, each replacing any copy with its identifier.
    pub(crate) fn apply(&mut self, issues: impl IntoIterator<Item = LinearIssue>) {
        for issue in issues {
            self.issues.insert(issue.identifier.clone(), issue);
        }
    }

    /// Mark the cached copies of `identifiers` stale, after their refresh
    /// failed, and hand those copies back. An identifier with no copy gets
    /// none.
    pub(crate) fn mark_stale(&mut self, identifiers: &[String]) -> Vec<LinearIssue> {
        let mut marked = Vec::new();
        for identifier in identifiers {
            if let Some(issue) = self.issues.get_mut(identifier) {
                issue.stale = true;
                marked.push(issue.clone());
            }
        }
        marked
    }

    /// Whether `identifier` should be fetched at `now`: it has no copy, or
    /// its copy is more than a minute old.
    pub(crate) fn needs_fetch(&self, identifier: &str, now: DateTime<Utc>) -> bool {
        self.issues
            .get(identifier)
            .is_none_or(|issue| now - issue.fetched_at > ISSUE_MAX_AGE)
    }

    /// The cached copy of `identifier`.
    pub(crate) fn get(&self, identifier: &str) -> Option<&LinearIssue> {
        self.issues.get(identifier)
    }
}

/// The Linear client: the key, the transport, the context and the cache.
pub(crate) struct LinearClient {
    key: Option<Secret>,
    transport: Arc<dyn LinearTransport>,
    context: Option<FetchedContext>,
    cache: LinearCache,
}

impl LinearClient {
    /// Replace the key, as when one was set after the daemon started. A
    /// different key drops the context the old one fetched.
    pub(crate) fn set_key(&mut self, key: Option<Secret>) {
        let same = match (&self.key, &key) {
            (Some(old), Some(new)) => old.expose() == new.expose(),
            (None, None) => true,
            _ => false,
        };
        if !same {
            self.context = None;
        }
        self.key = key;
    }

    /// A client posting through `transport`; with no `key` it makes no
    /// request.
    pub(crate) fn new(key: Option<Secret>, transport: Box<dyn LinearTransport>) -> Self {
        Self {
            key,
            transport: Arc::from(transport),
            context: None,
            cache: LinearCache::default(),
        }
    }

    /// Whether the daemon has a Linear API key.
    pub(crate) fn key_configured(&self) -> bool {
        self.key.is_some()
    }

    /// The key in use, if any.
    pub(crate) fn key(&self) -> Option<&Secret> {
        self.key.as_ref()
    }

    /// The transport, for a request made without the client's lock held.
    pub(crate) fn transport(&self) -> Arc<dyn LinearTransport> {
        Arc::clone(&self.transport)
    }

    /// The team keys and `urlKey`, once fetched.
    pub(crate) fn context(&self) -> Option<&LinearContextData> {
        self.context.as_ref().map(|context| &context.data)
    }

    /// Whether the context should be fetched at `now`: it never was, or the
    /// copy is more than four minutes old, so every 5-minute tick refreshes
    /// it.
    pub(crate) fn context_due(&self, now: DateTime<Utc>) -> bool {
        self.context
            .as_ref()
            .is_none_or(|context| now - context.fetched_at > CONTEXT_MAX_AGE)
    }

    /// Fetch the team keys and `urlKey`, stamped `now`. Without a key this
    /// does nothing.
    ///
    /// # Errors
    ///
    /// See [`LinearError`]; the previous context is kept.
    pub(crate) async fn refresh_context(&mut self, now: DateTime<Utc>) -> Result<(), LinearError> {
        let Some(key) = &self.key else {
            return Ok(());
        };
        let body = json!({ "query": CONTEXT_QUERY, "variables": {} });
        let reply = self.transport.post(key, body).await?;
        let data = parse_context(reply)?;
        self.context = Some(FetchedContext {
            data,
            fetched_at: now,
        });
        Ok(())
    }

    /// Fetch `identifiers` in batches of at most 50, stamped `now`, and
    /// cache them. A batch that fails marks its cached copies stale. The
    /// outcome holds every issue fetched and every copy marked stale, and
    /// the identifiers a batch Linear answered came back without; an
    /// identifier in a failed batch is in none of them. Without a key this
    /// does nothing.
    pub(crate) async fn fetch_issues(
        &mut self,
        identifiers: &[String],
        now: DateTime<Utc>,
    ) -> FetchOutcome {
        let mut outcome = FetchOutcome::default();
        let Some(key) = &self.key else {
            return outcome;
        };
        for batch in identifiers.chunks(BATCH_SIZE) {
            if outcome
                .failure
                .as_ref()
                .is_some_and(|failure| failure.rejected)
            {
                // Linear refused the key: no further batch is asked with it.
                outcome.issues.extend(self.cache.mark_stale(batch));
                continue;
            }
            match fetch_batch(self.transport.as_ref(), key, batch, now).await {
                Ok(issues) => {
                    outcome.unknown.extend(
                        batch
                            .iter()
                            .filter(|asked| !issues.iter().any(|issue| &issue.identifier == *asked))
                            .cloned(),
                    );
                    self.cache.apply(issues.iter().cloned());
                    outcome.issues.extend(issues);
                }
                Err(err) => {
                    warn!(
                        error = %err,
                        identifiers = batch.len(),
                        "linear: an issue batch failed; its cached copies are stale"
                    );
                    outcome.issues.extend(self.cache.mark_stale(batch));
                    outcome.failure = Some(Failure::of(&err));
                }
            }
        }
        outcome
    }

    /// The issues fetched so far.
    pub(crate) fn cache(&self) -> &LinearCache {
        &self.cache
    }
}

/// What [`LinearClient::fetch_issues`] found.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct FetchOutcome {
    /// Every issue fetched and every cached copy a failed batch marked stale.
    pub(crate) issues: Vec<LinearIssue>,
    /// The identifiers a batch Linear answered came back without: Linear
    /// does not know them, or they cannot be read with this key.
    pub(crate) unknown: Vec<String>,
    /// The last batch failure; `None` when every batch Linear was asked for
    /// succeeded.
    pub(crate) failure: Option<Failure>,
}

/// Ask Linear, through `transport` with `key`, for the team keys and the
/// workspace with the Test button's query, bypassing any cached context.
///
/// # Errors
///
/// See [`LinearError`].
pub(crate) async fn test_key(
    transport: &dyn LinearTransport,
    key: &Secret,
) -> Result<LinearContextData, LinearError> {
    let body = json!({ "query": TEST_QUERY, "variables": {} });
    parse_context(transport.post(key, body).await?)
}

/// Whether `offline`, the value of `RUSTLING_TULIP_LINEAR_OFFLINE`, takes the
/// daemon off Linear: exactly `1`.
pub(crate) fn is_offline(offline: Option<&str>) -> bool {
    offline == Some("1")
}

/// The API key: none while `offline` (the value of
/// `RUSTLING_TULIP_LINEAR_OFFLINE`) is exactly `1`, without calling
/// `resolve`; else what `resolve` finds.
pub(crate) fn resolve_key(
    offline: Option<&str>,
    resolve: impl FnOnce() -> Option<Secret>,
) -> Option<Secret> {
    if is_offline(offline) {
        return None;
    }
    resolve()
}

/// The key `LINEAR_API_KEY` holds (see [`user_env::resolve`]), unless
/// `RUSTLING_TULIP_LINEAR_OFFLINE` is exactly `1`, when no environment is
/// read at all.
pub(crate) fn configured_key() -> Option<Secret> {
    resolve_key(std::env::var(OFFLINE_VARIABLE).ok().as_deref(), || {
        user_env::resolve(KEY_VARIABLE).map(|(key, _origin)| key)
    })
}

/// A connection's `nodes`.
#[derive(Deserialize)]
struct Nodes<T> {
    nodes: Vec<T>,
}

#[derive(Deserialize)]
struct Team {
    key: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Organization {
    url_key: String,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
struct ContextReply {
    teams: Nodes<Team>,
    #[serde(default)]
    organization: Option<Organization>,
}

/// An object Linear names, such as a project or a user.
#[derive(Deserialize)]
struct Named {
    name: String,
}

#[derive(Deserialize)]
struct StateNode {
    name: String,
    #[serde(rename = "type")]
    kind: String,
    color: String,
}

/// One issue as the issue query returns it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct IssueNode {
    title: String,
    url: String,
    #[serde(default)]
    priority: f64,
    #[serde(default)]
    priority_label: String,
    #[serde(default)]
    updated_at: Option<DateTime<Utc>>,
    #[serde(default)]
    state: Option<StateNode>,
    #[serde(default)]
    project: Option<Named>,
    #[serde(default)]
    assignee: Option<Named>,
    #[serde(default)]
    releases: Option<Nodes<Named>>,
}

impl IssueNode {
    /// The issue under `identifier`, the one it was asked for.
    fn into_issue(self, identifier: String, now: DateTime<Utc>) -> LinearIssue {
        LinearIssue {
            identifier,
            title: self.title,
            url: self.url,
            state: self.state.map(|state| LinearState {
                kind: state_kind(&state.kind),
                name: state.name,
                color: state.color,
            }),
            priority: priority_level(self.priority),
            priority_label: self.priority_label,
            project: self.project.map(|project| project.name),
            releases: self
                .releases
                .map(|releases| {
                    releases
                        .nodes
                        .into_iter()
                        .map(|release| release.name)
                        .collect()
                })
                .unwrap_or_default(),
            assignee: self.assignee.map(|assignee| assignee.name),
            updated_at: self.updated_at,
            fetched_at: now,
            stale: false,
        }
    }
}

/// A state's `type` as a [`LinearStateKind`], by its `snake_case` name; a
/// type this build does not know is `Unknown`.
fn state_kind(kind: &str) -> LinearStateKind {
    serde_json::from_value(Value::String(kind.to_owned())).unwrap_or(LinearStateKind::Unknown)
}

/// Linear's priority (a float, 0 to 4) as a whole level; anything else is 0.
fn priority_level(priority: f64) -> u8 {
    (0u8..=4)
        .find(|level| (f64::from(*level) - priority).abs() < 0.5)
        .unwrap_or(0)
}

/// The reply's `errors` array, empty when it has none.
fn error_list(reply: &Value) -> &[Value] {
    reply
        .get("errors")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// The messages of `errors`, joined for a log line.
fn error_text(errors: &[Value]) -> String {
    errors
        .iter()
        .map(|error| {
            error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("an error with no message")
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn shape(err: &serde_json::Error) -> LinearError {
    LinearError::Shape(err.to_string())
}

/// The reply's `data`, taken out of it.
fn take_data(reply: &mut Value) -> Value {
    reply.get_mut("data").map_or(Value::Null, Value::take)
}

/// Whether any of `errors` carries `extensions.code` `AUTHENTICATION_ERROR`.
fn is_authentication_error(errors: &[Value]) -> bool {
    errors.iter().any(|error| {
        error.pointer("/extensions/code").and_then(Value::as_str) == Some(AUTHENTICATION_ERROR)
    })
}

/// The error a non-empty `errors` array stands for: a refused key when any
/// is an authentication error, else their messages.
fn graphql_error(errors: &[Value]) -> LinearError {
    if is_authentication_error(errors) {
        LinearError::Rejected
    } else {
        LinearError::GraphQl(error_text(errors))
    }
}

fn parse_context(mut reply: Value) -> Result<LinearContextData, LinearError> {
    let errors = error_list(&reply);
    if !errors.is_empty() {
        return Err(graphql_error(errors));
    }
    let parsed: ContextReply =
        serde_json::from_value(take_data(&mut reply)).map_err(|err| shape(&err))?;
    let (workspace, workspace_name) = parsed.organization.map_or((None, None), |organization| {
        (Some(organization.url_key), organization.name)
    });
    Ok(LinearContextData {
        team_keys: parsed
            .teams
            .nodes
            .into_iter()
            .map(|team| team.key)
            .collect(),
        workspace,
        workspace_name,
    })
}

/// What one issue query's reply says.
enum BatchReply {
    /// The issues it returned; aliases that came back null are left out.
    Issues(Vec<LinearIssue>),
    /// The query failed only on these aliases' issues (Linear fails the
    /// whole query on an issue that does not exist or cannot be read).
    Unknown(Vec<usize>),
}

/// One aliased query for `batch`: `i0: issue(id: $i0) { … }` and so on, the
/// identifiers passed as variables so none is ever spliced into the query.
fn issues_body(batch: &[String]) -> Value {
    let params: Vec<String> = (0..batch.len())
        .map(|index| format!("$i{index}: String!"))
        .collect();
    let fields: Vec<String> = (0..batch.len())
        .map(|index| format!("i{index}: issue(id: $i{index}) {{ {ISSUE_FIELDS} }}"))
        .collect();
    let variables: Map<String, Value> = batch
        .iter()
        .enumerate()
        .map(|(index, identifier)| (format!("i{index}"), Value::String(identifier.clone())))
        .collect();
    let query = format!(
        "query Issues({}) {{ {} }}",
        params.join(", "),
        fields.join(" ")
    );
    json!({ "query": query, "variables": variables })
}

/// The alias indexes `errors` name, when every error's path is exactly one
/// of the batch's `count` aliases; `None` when any error is about something
/// else.
fn alias_errors(errors: &[Value], count: usize) -> Option<Vec<usize>> {
    let mut indexes = Vec::new();
    for error in errors {
        let [alias] = error.get("path")?.as_array()?.as_slice() else {
            return None;
        };
        let index: usize = alias.as_str()?.strip_prefix('i')?.parse().ok()?;
        if index >= count {
            return None;
        }
        if !indexes.contains(&index) {
            indexes.push(index);
        }
    }
    Some(indexes)
}

/// The reply to the issue query for `asked`. Each issue carries the
/// identifier it was asked for, which differs from Linear's own when the
/// issue has moved to another team.
fn parse_batch(
    mut reply: Value,
    asked: &[String],
    now: DateTime<Utc>,
) -> Result<BatchReply, LinearError> {
    let errors = error_list(&reply);
    if is_authentication_error(errors) {
        return Err(LinearError::Rejected);
    }
    if !errors.is_empty() {
        return alias_errors(errors, asked.len())
            .map(BatchReply::Unknown)
            .ok_or_else(|| LinearError::GraphQl(error_text(errors)));
    }
    let Value::Object(mut data) = take_data(&mut reply) else {
        return Err(LinearError::Shape("the reply holds no data".to_owned()));
    };
    let mut issues = Vec::new();
    for (index, identifier) in asked.iter().enumerate() {
        match data.remove(&format!("i{index}")) {
            None | Some(Value::Null) => {}
            Some(node) => {
                let node: IssueNode = serde_json::from_value(node).map_err(|err| shape(&err))?;
                issues.push(node.into_issue(identifier.clone(), now));
            }
        }
    }
    Ok(BatchReply::Issues(issues))
}

/// Fetch one batch. While Linear fails the query only on some identifiers'
/// issues, those identifiers are dropped and the rest asked for again; each
/// round drops at least one, so there are at most as many rounds as
/// identifiers.
async fn fetch_batch(
    transport: &dyn LinearTransport,
    key: &Secret,
    batch: &[String],
    now: DateTime<Utc>,
) -> Result<Vec<LinearIssue>, LinearError> {
    let mut asking = batch.to_vec();
    while !asking.is_empty() {
        let reply = transport.post(key, issues_body(&asking)).await?;
        let unknown = match parse_batch(reply, &asking, now)? {
            BatchReply::Issues(issues) => return Ok(issues),
            BatchReply::Unknown(unknown) => unknown,
        };
        let before = asking.len();
        asking = asking
            .into_iter()
            .enumerate()
            .filter(|(index, _)| !unknown.contains(index))
            .map(|(_, identifier)| identifier)
            .collect();
        if asking.len() == before {
            return Err(LinearError::GraphQl(
                "Linear refused issues without naming one asked for".to_owned(),
            ));
        }
    }
    Ok(Vec::new())
}

/// The Linear issue identifiers in `text`, deduplicated, in first-seen
/// order: every `KEY-123` that stands as a word of its own and whose key is
/// one of `team_keys` (case-sensitive), and the identifier in every
/// `linear.app/{workspace}/issue/{ID}` URL, whatever its key.
pub(crate) fn identifiers_in(text: &str, team_keys: &[String]) -> Vec<String> {
    let mut found: Vec<(usize, &str)> = bare_identifiers(text)
        .filter(|(_, identifier)| {
            team_keys
                .iter()
                .any(|key| Some(key.as_str()) == team_key(identifier))
        })
        .chain(url_identifiers(text))
        .collect();
    found.sort_by_key(|(start, _)| *start);
    let mut identifiers: Vec<String> = Vec::new();
    for (_, identifier) in found {
        if !identifiers.iter().any(|seen| seen == identifier) {
            identifiers.push(identifier.to_owned());
        }
    }
    identifiers
}

/// The team key of `identifier`, the text before its `-`.
fn team_key(identifier: &str) -> Option<&str> {
    identifier.split_once('-').map(|(key, _)| key)
}

/// Every `KEY-123` in `text` with no word character on either side, with
/// where it starts.
fn bare_identifiers(text: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut next = 0;
    std::iter::from_fn(move || {
        while next < text.len() {
            let start = next;
            if !word_before(text, start)
                && let Some(end) = identifier_at(text, start)
            {
                next = end;
                return Some((start, &text[start..end]));
            }
            next += text[start..].chars().next().map_or(1, char::len_utf8);
        }
        None
    })
}

/// The identifier of every `linear.app/{workspace}/issue/{ID}` URL in
/// `text`, with where the identifier starts.
fn url_identifiers(text: &str) -> impl Iterator<Item = (usize, &str)> {
    text.match_indices(URL_HOST).filter_map(move |(at, _)| {
        let workspace_start = at + URL_HOST.len();
        let workspace = text[workspace_start..]
            .bytes()
            .take_while(|byte| byte.is_ascii_alphanumeric() || *byte == b'-' || *byte == b'_')
            .count();
        let path = workspace_start + workspace;
        if workspace == 0 || !text[path..].starts_with(URL_ISSUE) {
            return None;
        }
        let start = path + URL_ISSUE.len();
        identifier_at(text, start).map(|end| (start, &text[start..end]))
    })
}

/// Where a `KEY-123` that starts at `start` ends: an uppercase letter, then
/// uppercase letters and digits, a `-`, at least one digit, and no word
/// character after it.
fn identifier_at(text: &str, start: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    if !bytes.get(start)?.is_ascii_uppercase() {
        return None;
    }
    let key_end = start
        + bytes[start..]
            .iter()
            .take_while(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
            .count();
    if bytes.get(key_end) != Some(&b'-') {
        return None;
    }
    let digits = bytes[key_end + 1..]
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    let end = key_end + 1 + digits;
    (digits > 0 && !word_at(text, end)).then_some(end)
}

/// Whether the character just before byte `at` is a word character.
fn word_before(text: &str, at: usize) -> bool {
    text[..at].chars().next_back().is_some_and(is_word)
}

/// Whether the character at byte `at` is a word character.
fn word_at(text: &str, at: usize) -> bool {
    text[at..].chars().next().is_some_and(is_word)
}

fn is_word(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

/// The fake transport and the canned replies the client's tests and the
/// daemon's Linear wiring tests share.
#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "fakes assert preconditions with expect; panic messages aid debugging"
)]
pub(crate) mod test_support {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use futures::future::BoxFuture;
    use serde_json::{Value, json};

    use super::{LinearClient, LinearError, LinearTransport};
    use crate::user_env::Secret;

    /// The key [`keyed`] gives a client; the fake checks every request
    /// carries it, or a key [`Fake::accept`] added.
    pub(crate) const KEY: &str = "lin_api_test";

    /// Canned replies, handed out in order, every request body posted and
    /// the key each was posted with. A request with no reply left fails the
    /// test.
    #[derive(Clone, Default)]
    pub(crate) struct Fake {
        replies: Arc<Mutex<VecDeque<Result<Value, LinearError>>>>,
        requests: Arc<Mutex<Vec<Value>>>,
        accepted: Arc<Mutex<Vec<String>>>,
        keys: Arc<Mutex<Vec<String>>>,
    }

    impl Fake {
        pub(crate) fn with(replies: Vec<Result<Value, LinearError>>) -> Self {
            Self {
                replies: Arc::new(Mutex::new(replies.into())),
                ..Self::default()
            }
        }

        /// Accept requests posted with `key` too, besides [`KEY`].
        pub(crate) fn accept(&self, key: &str) {
            self.accepted
                .lock()
                .expect("accepted lock")
                .push(key.to_owned());
        }

        /// The key each request was posted with, in order.
        pub(crate) fn keys(&self) -> Vec<String> {
            self.keys.lock().expect("keys lock").clone()
        }

        /// Queue `reply` after the ones already queued.
        pub(crate) fn push(&self, reply: Result<Value, LinearError>) {
            self.replies.lock().expect("replies lock").push_back(reply);
        }

        pub(crate) fn requests(&self) -> Vec<Value> {
            self.requests.lock().expect("requests lock").clone()
        }
    }

    impl LinearTransport for Fake {
        fn post<'a>(
            &'a self,
            key: &'a Secret,
            body: Value,
        ) -> BoxFuture<'a, Result<Value, LinearError>> {
            let accepted = key.expose() == KEY
                || self
                    .accepted
                    .lock()
                    .expect("accepted lock")
                    .iter()
                    .any(|other| other == key.expose());
            assert!(accepted, "the client posts with its key");
            self.keys
                .lock()
                .expect("keys lock")
                .push(key.expose().to_owned());
            self.requests.lock().expect("requests lock").push(body);
            let reply = self
                .replies
                .lock()
                .expect("replies lock")
                .pop_front()
                .expect("a canned reply for every request");
            Box::pin(async move { reply })
        }
    }

    /// A client with [`KEY`] posting through `fake`.
    pub(crate) fn keyed(fake: &Fake) -> LinearClient {
        LinearClient::new(Some(Secret::new(KEY.to_owned())), Box::new(fake.clone()))
    }

    pub(crate) fn issue_json(identifier: &str) -> Value {
        json!({
            "identifier": identifier,
            "title": format!("Title of {identifier}"),
            "url": format!("https://linear.app/acme/issue/{identifier}/title"),
            "priority": 2,
            "priorityLabel": "High",
            "updatedAt": "2026-11-17T10:30:00.000Z",
            "state": { "name": "In Progress", "type": "started", "color": "#f2c94c" },
            "project": { "name": "Linear links" },
            "assignee": { "name": "Owner" },
            "releases": { "nodes": [ { "name": "v0.9" }, { "name": "v1.0" } ] }
        })
    }

    /// A reply holding each identifier under its alias, in order.
    pub(crate) fn batch_reply(identifiers: &[String]) -> Value {
        let data: serde_json::Map<String, Value> = identifiers
            .iter()
            .enumerate()
            .map(|(index, id)| (format!("i{index}"), issue_json(id)))
            .collect();
        json!({ "data": data })
    }

    /// The context query's reply: `team_keys` and the workspace `url_key`,
    /// whose name is [`workspace_name`] of it.
    pub(crate) fn context_reply(team_keys: &[&str], url_key: &str) -> Value {
        let nodes: Vec<Value> = team_keys.iter().map(|key| json!({ "key": key })).collect();
        json!({
            "data": {
                "teams": { "nodes": nodes },
                "organization": { "name": workspace_name(url_key), "urlKey": url_key }
            }
        })
    }

    /// The workspace name [`context_reply`] gives the workspace `url_key`.
    pub(crate) fn workspace_name(url_key: &str) -> String {
        format!("{url_key} workspace")
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; panic messages aid debugging"
)]
mod tests {
    use chrono::{TimeDelta, TimeZone};
    use protocol::{LinearState, LinearStateKind};
    use serde_json::json;

    use super::test_support::{Fake, batch_reply, issue_json, keyed};
    use super::*;

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 18, 12, 0, 0)
            .single()
            .expect("a valid time")
    }

    fn ids(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    fn variables(request: &Value) -> serde_json::Map<String, Value> {
        request["variables"]
            .as_object()
            .expect("the request carries variables")
            .clone()
    }

    fn cached(identifier: &str, fetched_at: DateTime<Utc>) -> LinearIssue {
        LinearIssue {
            identifier: identifier.to_owned(),
            title: "A title".to_owned(),
            url: format!("https://linear.app/acme/issue/{identifier}"),
            state: None,
            priority: 0,
            priority_label: "No priority".to_owned(),
            project: None,
            releases: Vec::new(),
            assignee: None,
            updated_at: None,
            fetched_at,
            stale: false,
        }
    }

    #[tokio::test]
    async fn a_missing_key_makes_no_request() {
        let fake = Fake::default();
        let mut client = LinearClient::new(None, Box::new(fake.clone()));
        assert!(!client.key_configured());
        client
            .refresh_context(t0())
            .await
            .expect("no key is not an error");
        let issues = client.fetch_issues(&ids(&["RT-1"]), t0()).await.issues;
        assert_eq!(issues, Vec::<LinearIssue>::new());
        assert!(client.context().is_none());
        assert!(fake.requests().is_empty(), "no request without a key");
    }

    #[tokio::test]
    async fn the_context_query_parses_team_keys_and_the_workspace() {
        let fake = Fake::with(vec![Ok(json!({
            "data": {
                "teams": { "nodes": [ { "key": "RT" }, { "key": "OPS" } ] },
                "organization": { "name": "Acme Inc", "urlKey": "acme" }
            }
        }))]);
        let mut client = keyed(&fake);
        client
            .refresh_context(t0())
            .await
            .expect("the context parses");
        assert_eq!(
            client.context(),
            Some(&LinearContextData {
                team_keys: ids(&["RT", "OPS"]),
                workspace: Some("acme".to_owned()),
                workspace_name: Some("Acme Inc".to_owned()),
            })
        );
        let requests = fake.requests();
        assert_eq!(requests.len(), 1);
        let query = requests[0]["query"].as_str().expect("a query string");
        assert!(query.contains("teams { nodes { key } }"), "{query}");
        assert!(query.contains("organization { name urlKey }"), "{query}");
    }

    #[test]
    fn http_401_and_403_are_a_rejected_key_and_500_is_not() {
        assert!(status_error(401).is_rejected());
        assert!(status_error(403).is_rejected());
        let server = status_error(500);
        assert!(!server.is_rejected());
        assert!(matches!(server, LinearError::Status(500)), "{server:?}");
        assert_eq!(
            LinearError::Rejected.to_string(),
            "Linear refused the API key"
        );
    }

    /// Linear's reply to a request with a revoked or invalid key.
    fn authentication_error() -> Value {
        json!({
            "data": null,
            "errors": [ {
                "message": "Authentication required, not authenticated",
                "extensions": { "code": "AUTHENTICATION_ERROR" }
            } ]
        })
    }

    #[tokio::test]
    async fn a_graphql_authentication_error_is_a_rejected_key() {
        let fake = Fake::with(vec![Ok(authentication_error()), Ok(authentication_error())]);
        let mut client = keyed(&fake);
        let err = client
            .refresh_context(t0())
            .await
            .expect_err("the context fetch fails");
        assert!(err.is_rejected(), "{err:?}");

        let all: Vec<String> = (1..=60).map(|n| format!("RT-{n}")).collect();
        let outcome = client.fetch_issues(&all, t0()).await;
        let failure = outcome.failure.expect("the fetch failed");
        assert!(failure.rejected, "{failure:?}");
        assert_eq!(failure.text, "Linear refused the API key");
        assert_eq!(
            fake.requests().len(),
            2,
            "no second batch is asked with a refused key"
        );

        let fake = Fake::with(vec![Ok(json!({
            "data": null,
            "errors": [ { "message": "Internal error", "extensions": { "code": "INTERNAL" } } ]
        }))]);
        let mut client = keyed(&fake);
        let err = client
            .refresh_context(t0())
            .await
            .expect_err("the context fetch fails");
        assert!(!err.is_rejected(), "{err:?}");
    }

    #[tokio::test]
    async fn the_context_is_due_after_four_minutes() {
        let reply = json!({
            "data": { "teams": { "nodes": [ { "key": "RT" } ] }, "organization": { "urlKey": "acme" } }
        });
        let fake = Fake::with(vec![Ok(reply)]);
        let mut client = keyed(&fake);
        assert!(client.context_due(t0()), "due before the first fetch");
        client
            .refresh_context(t0())
            .await
            .expect("the context parses");
        assert!(!client.context_due(t0() + TimeDelta::minutes(3) + TimeDelta::seconds(59)));
        assert!(!client.context_due(t0() + TimeDelta::minutes(4)));
        assert!(client.context_due(t0() + TimeDelta::minutes(4) + TimeDelta::seconds(1)));
        assert!(
            client.context_due(t0() + TimeDelta::minutes(5)),
            "every 5-minute tick refreshes it"
        );
    }

    #[tokio::test]
    async fn issues_are_fetched_in_batches_of_fifty() {
        let all: Vec<String> = (1..=120).map(|n| format!("RT-{n}")).collect();
        let fake = Fake::with(vec![
            Ok(batch_reply(&all[0..50])),
            Ok(batch_reply(&all[50..100])),
            Ok(batch_reply(&all[100..120])),
        ]);
        let mut client = keyed(&fake);
        let issues = client.fetch_issues(&all, t0()).await.issues;
        assert_eq!(issues.len(), 120);
        let requests = fake.requests();
        assert_eq!(requests.len(), 3);
        let sizes: Vec<usize> = requests.iter().map(|r| variables(r).len()).collect();
        assert_eq!(sizes, vec![50, 50, 20]);
        assert_eq!(variables(&requests[1])["i0"], json!("RT-51"));
        let query = requests[0]["query"].as_str().expect("a query string");
        assert!(query.contains("i49: issue(id: $i49)"), "{query}");
        assert!(!query.contains("i50:"), "{query}");
        assert!(client.cache().get("RT-120").is_some());
    }

    #[tokio::test]
    async fn an_issue_parses_every_field_and_an_unknown_state_type() {
        let mut odd = issue_json("RT-2");
        odd["state"]["type"] = json!("paused");
        odd["project"] = Value::Null;
        odd["assignee"] = Value::Null;
        odd["updatedAt"] = Value::Null;
        odd["releases"] = json!({ "nodes": [] });
        let fake = Fake::with(vec![Ok(
            json!({ "data": { "i0": issue_json("RT-1"), "i1": odd } }),
        )]);
        let mut client = keyed(&fake);
        let issues = client
            .fetch_issues(&ids(&["RT-1", "RT-2"]), t0())
            .await
            .issues;
        let full = LinearIssue {
            identifier: "RT-1".to_owned(),
            title: "Title of RT-1".to_owned(),
            url: "https://linear.app/acme/issue/RT-1/title".to_owned(),
            state: Some(LinearState {
                name: "In Progress".to_owned(),
                kind: LinearStateKind::Started,
                color: "#f2c94c".to_owned(),
            }),
            priority: 2,
            priority_label: "High".to_owned(),
            project: Some("Linear links".to_owned()),
            releases: ids(&["v0.9", "v1.0"]),
            assignee: Some("Owner".to_owned()),
            updated_at: Some(
                Utc.with_ymd_and_hms(2026, 11, 17, 10, 30, 0)
                    .single()
                    .expect("a valid time"),
            ),
            fetched_at: t0(),
            stale: false,
        };
        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0], full);
        let second = &issues[1];
        assert_eq!(
            second.state.as_ref().map(|state| &state.kind),
            Some(&LinearStateKind::Unknown)
        );
        assert_eq!(second.project, None);
        assert_eq!(second.assignee, None);
        assert_eq!(second.updated_at, None);
        assert_eq!(second.releases, Vec::<String>::new());
        assert_eq!(client.cache().get("RT-1"), Some(&full));
    }

    #[tokio::test]
    async fn an_unknown_identifier_is_skipped() {
        // An alias that comes back null.
        let fake = Fake::with(vec![Ok(
            json!({ "data": { "i0": issue_json("RT-1"), "i1": null } }),
        )]);
        let mut client = keyed(&fake);
        let issues = client
            .fetch_issues(&ids(&["RT-1", "RT-404"]), t0())
            .await
            .issues;
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].identifier, "RT-1");
        assert_eq!(fake.requests().len(), 1);

        // Linear's own answer: the unknown issue fails the query with an
        // error at its alias, so the batch is asked again without it.
        let fake = Fake::with(vec![
            Ok(json!({
                "data": null,
                "errors": [ { "message": "Entity not found: Issue", "path": ["i1"] } ]
            })),
            Ok(batch_reply(&ids(&["RT-3"]))),
        ]);
        let mut client = keyed(&fake);
        let issues = client
            .fetch_issues(&ids(&["RT-3", "RT-404"]), t0())
            .await
            .issues;
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].identifier, "RT-3");
        assert!(!issues[0].stale);
        let requests = fake.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            variables(&requests[1]),
            json!({ "i0": "RT-3" })
                .as_object()
                .expect("an object")
                .clone()
        );
        assert!(client.cache().get("RT-404").is_none());
    }

    #[tokio::test]
    async fn an_http_error_marks_the_batch_stale() {
        let fake = Fake::with(vec![
            Ok(batch_reply(&ids(&["RT-1"]))),
            Err(LinearError::Status(500)),
        ]);
        let mut client = keyed(&fake);
        client.fetch_issues(&ids(&["RT-1"]), t0()).await;
        let later = t0() + TimeDelta::minutes(2);
        let issues = client
            .fetch_issues(&ids(&["RT-1", "RT-9"]), later)
            .await
            .issues;
        assert_eq!(issues.len(), 1, "only the cached copy comes back");
        assert!(issues[0].stale);
        assert_eq!(issues[0].fetched_at, t0(), "the copy keeps its fetch time");
        let copy = client.cache().get("RT-1").expect("the copy stays cached");
        assert!(copy.stale);
        assert!(
            client.cache().get("RT-9").is_none(),
            "a failed batch creates no entry"
        );
    }

    #[tokio::test]
    async fn a_graphql_error_marks_the_batch_stale() {
        let fake = Fake::with(vec![
            Ok(batch_reply(&ids(&["RT-1"]))),
            Ok(json!({ "data": null, "errors": [ { "message": "Internal error" } ] })),
        ]);
        let mut client = keyed(&fake);
        client.fetch_issues(&ids(&["RT-1"]), t0()).await;
        let issues = client
            .fetch_issues(&ids(&["RT-1"]), t0() + TimeDelta::minutes(2))
            .await
            .issues;
        assert_eq!(issues.len(), 1);
        assert!(issues[0].stale);
        assert!(client.cache().get("RT-1").expect("cached").stale);
        assert_eq!(
            fake.requests().len(),
            2,
            "an error without an alias path is not retried"
        );
    }

    /// Linear's error naming the issue at `alias` as not found.
    fn not_found(alias: &str) -> Value {
        json!({
            "data": null,
            "errors": [ { "message": "Entity not found: Issue", "path": [alias] } ]
        })
    }

    #[tokio::test]
    async fn two_unknown_identifiers_named_one_per_reply_still_fetch_the_rest() {
        let fake = Fake::with(vec![
            Ok(not_found("i1")),
            Ok(not_found("i1")),
            Ok(batch_reply(&ids(&["RT-1", "RT-2"]))),
        ]);
        let mut client = keyed(&fake);
        let asked = ids(&["RT-1", "RT-404", "RT-405", "RT-2"]);
        let issues = client.fetch_issues(&asked, t0()).await.issues;
        let fetched: Vec<&str> = issues
            .iter()
            .map(|issue| issue.identifier.as_str())
            .collect();
        assert_eq!(fetched, ["RT-1", "RT-2"]);
        assert!(issues.iter().all(|issue| !issue.stale));
        let requests = fake.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(
            variables(&requests[1]),
            json!({ "i0": "RT-1", "i1": "RT-405", "i2": "RT-2" })
                .as_object()
                .expect("an object")
                .clone()
        );
        assert_eq!(
            variables(&requests[2]),
            json!({ "i0": "RT-1", "i1": "RT-2" })
                .as_object()
                .expect("an object")
                .clone()
        );
        assert!(client.cache().get("RT-404").is_none());
        assert!(client.cache().get("RT-405").is_none());
    }

    #[tokio::test]
    async fn a_reply_naming_an_alias_no_longer_asked_for_fails_the_batch() {
        let fake = Fake::with(vec![
            Ok(batch_reply(&ids(&["RT-1"]))),
            Ok(not_found("i1")),
            Ok(not_found("i1")),
        ]);
        let mut client = keyed(&fake);
        client.fetch_issues(&ids(&["RT-1"]), t0()).await;
        let later = t0() + TimeDelta::minutes(2);
        let issues = client
            .fetch_issues(&ids(&["RT-1", "RT-404"]), later)
            .await
            .issues;
        assert_eq!(issues.len(), 1, "only the cached copy comes back");
        assert!(issues[0].stale, "no progress fails the batch");
        assert_eq!(fake.requests().len(), 3);
    }

    #[tokio::test]
    async fn a_moved_issue_is_cached_under_the_identifier_asked_for() {
        let fake = Fake::with(vec![Ok(json!({ "data": { "i0": issue_json("RT-9") } }))]);
        let mut client = keyed(&fake);
        let issues = client.fetch_issues(&ids(&["ABC-5"]), t0()).await.issues;
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].identifier, "ABC-5");
        assert_eq!(issues[0].url, "https://linear.app/acme/issue/RT-9/title");
        let copy = client.cache().get("ABC-5").expect("cached under ABC-5");
        assert_eq!(copy.title, "Title of RT-9");
        assert!(
            !client
                .cache()
                .needs_fetch("ABC-5", t0() + TimeDelta::seconds(30))
        );
        assert!(client.cache().get("RT-9").is_none());
    }

    #[test]
    fn a_copy_older_than_a_minute_needs_a_fetch() {
        let mut cache = LinearCache::default();
        assert!(
            cache.needs_fetch("RT-1", t0()),
            "an absent issue needs a fetch"
        );
        cache.apply([cached("RT-1", t0())]);
        assert!(!cache.needs_fetch("RT-1", t0() + TimeDelta::seconds(30)));
        assert!(!cache.needs_fetch("RT-1", t0() + TimeDelta::minutes(1)));
        assert!(cache.needs_fetch("RT-1", t0() + TimeDelta::seconds(61)));
        assert_eq!(cache.get("RT-1").map(|issue| issue.fetched_at), Some(t0()));
    }

    #[test]
    fn identifiers_match_only_known_team_keys() {
        let keys = ids(&["RT", "OPS"]);
        assert_eq!(identifiers_in("RT-123", &keys), ids(&["RT-123"]));
        assert_eq!(
            identifiers_in("see RT-1, RT-22.", &keys),
            ids(&["RT-1", "RT-22"])
        );
        assert_eq!(
            identifiers_in("OPS-7 then RT-1 and RT-1 again", &keys),
            ids(&["OPS-7", "RT-1"])
        );
        for text in [
            "UTF-8", "SHA-256", "ISO-8601", "ABC-1", "XRT-1", "RT-1X", "rt-1", "RT-", "_RT-1",
        ] {
            assert!(
                identifiers_in(text, &keys).is_empty(),
                "{text} must not match"
            );
        }
        assert!(
            identifiers_in("RT-1", &[]).is_empty(),
            "no team keys, no bare identifiers"
        );
    }

    #[test]
    fn a_linear_issue_url_yields_its_identifier() {
        let url = "https://linear.app/leftos/issue/RT-134/linear-protocol";
        assert_eq!(identifiers_in(url, &[]), ids(&["RT-134"]));
        assert_eq!(
            identifiers_in(url, &ids(&["RT"])),
            ids(&["RT-134"]),
            "deduplicated"
        );
        assert_eq!(
            identifiers_in(
                "RT-5 and https://linear.app/acme/issue/ABC-1/title",
                &ids(&["RT"])
            ),
            ids(&["RT-5", "ABC-1"]),
            "a URL counts whatever its key, in first-seen order"
        );
        assert_eq!(
            identifiers_in("(https://linear.app/acme/issue/ABC-12)", &[]),
            ids(&["ABC-12"])
        );
        assert_eq!(
            identifiers_in("https://linear.app/acme/project/ABC-1", &[]),
            Vec::<String>::new()
        );
    }
}
