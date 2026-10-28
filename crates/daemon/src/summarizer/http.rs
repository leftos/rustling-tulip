//! The summarizer's HTTP call to a provider's Messages endpoint: the request a
//! [`ProviderRow`] describes, the timeouts and the one retry, the refused-key
//! rule, the reply read into a [`Summary`], and its cost from the reply's
//! token usage.

use std::sync::Arc;
use std::time::Duration;

use reqwest::StatusCode;
use reqwest::header::HeaderValue;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Semaphore;
use tracing::warn;

use crate::summarizer::keys::Rejections;
use crate::summarizer::providers::ProviderRow;
use crate::summarizer::reply::{self, ReplyError, Summary};
use crate::user_env::Secret;

/// The most calls in flight at once; more wait their turn.
const MAX_IN_FLIGHT: usize = 4;

/// The request's output token budget.
const MAX_TOKENS: u32 = 4000;

/// How long a call may take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HttpConfig {
    /// The most time opening the connection may take.
    pub(crate) connect_timeout: Duration,
    /// The most time one attempt may take, from sending to the reply's end.
    pub(crate) total_timeout: Duration,
    /// The wait before the one retry.
    pub(crate) retry_delay: Duration,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(3),
            total_timeout: Duration::from_secs(15),
            retry_delay: Duration::from_secs(1),
        }
    }
}

/// A summary and what the call cost.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HttpReply {
    pub(crate) summary: Summary,
    /// US dollars, from the reply's token usage; 0 when it reported none.
    pub(crate) cost_usd: f64,
}

/// Why a call gave no summary. No variant carries the key.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CallError {
    /// The provider refused this key (401 or 403), now or on an earlier call;
    /// no call is made with it again until the key changes.
    #[error("the provider refused the key; save or set a new key to try again")]
    Rejected,
    /// The key holds characters an HTTP header cannot carry.
    #[error("the key holds characters an HTTP header cannot carry")]
    BadKey,
    /// The provider answered with an error status.
    #[error("the provider answered HTTP {0}")]
    Status(u16),
    /// The connection failed, timed out or broke off.
    #[error("the call failed")]
    Transport(#[source] reqwest::Error),
    /// The reply was not the Messages reply shape.
    #[error("the reply could not be read: {0}")]
    Malformed(String),
    /// The reply stopped at the token budget; it was still billed.
    #[error("the reply was cut at the token limit")]
    MaxTokens { cost_usd: f64 },
    /// The reply's text held no usable summary; it was still billed.
    #[error("the reply held no summary")]
    Reply {
        #[source]
        source: ReplyError,
        cost_usd: f64,
    },
    /// The client is shutting down.
    #[error("the summarizer is shutting down")]
    Closed,
}

/// The HTTP client every provider call goes through.
pub(crate) struct HttpClient {
    http: reqwest::Client,
    config: HttpConfig,
    permits: Semaphore,
    rejections: Arc<Rejections>,
}

impl HttpClient {
    /// A client with `config`'s timeouts that records refused keys in
    /// `rejections`.
    ///
    /// # Errors
    ///
    /// The TLS setup failed, for one when the system's root certificates
    /// cannot be loaded.
    pub(crate) fn new(config: HttpConfig, rejections: Arc<Rejections>) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            .timeout(config.total_timeout)
            .build()?;
        Ok(Self {
            http,
            config,
            permits: Semaphore::new(MAX_IN_FLIGHT),
            rejections,
        })
    }

    /// Ask `row`'s provider to summarize `prompt` with `key`.
    ///
    /// # Errors
    ///
    /// See [`CallError`]: a refused key, an error status, a transport failure
    /// (each of these but a refused key or a 4xx after one retry), or a reply
    /// that holds no summary.
    pub(crate) async fn summarize(
        &self,
        row: &ProviderRow,
        key: &Secret,
        prompt: &str,
    ) -> Result<HttpReply, CallError> {
        let header = header_value(key)?;
        let Ok(_permit) = self.permits.acquire().await else {
            return Err(CallError::Closed);
        };
        if self.rejections.is_rejected(row.id, key) {
            return Err(CallError::Rejected);
        }
        let reply = match self.send_with_retry(row, &header, prompt).await {
            Ok(reply) => reply,
            Err(CallError::Rejected) => {
                self.rejections.reject(row.id, key);
                return Err(CallError::Rejected);
            }
            Err(err) => return Err(err),
        };
        finish(row, &reply)
    }

    /// Send the request, and send it once more after the retry wait when the
    /// first attempt timed out, lost its connection, or got a 429 or a 5xx.
    async fn send_with_retry(
        &self,
        row: &ProviderRow,
        key: &HeaderValue,
        prompt: &str,
    ) -> Result<MessagesReply, CallError> {
        let err = match self.send_once(row, key, prompt).await {
            Attempt::Done(result) => return result,
            Attempt::Retry(err) => err,
        };
        warn!(provider = row.id, error = %error_text(&err), "summarizer: the call failed; retrying once");
        tokio::time::sleep(self.config.retry_delay).await;
        match self.send_once(row, key, prompt).await {
            Attempt::Done(result) => result,
            Attempt::Retry(err) => Err(err),
        }
    }

    async fn send_once(&self, row: &ProviderRow, key: &HeaderValue, prompt: &str) -> Attempt {
        let body = json!({
            "model": row.model,
            "max_tokens": MAX_TOKENS,
            "thinking": {"type": "disabled"},
            "messages": [{"role": "user", "content": prompt}],
        });
        let sent = self
            .http
            .post(row.endpoint)
            .header(row.key_header, key.clone())
            .header(row.version_header.0, row.version_header.1)
            .json(&body)
            .send()
            .await;
        let response = match sent {
            Ok(response) => response,
            Err(err) => return Attempt::Retry(CallError::Transport(err)),
        };
        let status = response.status();
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Attempt::Done(Err(CallError::Rejected));
        }
        if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
            return Attempt::Retry(CallError::Status(status.as_u16()));
        }
        if !status.is_success() {
            return Attempt::Done(Err(CallError::Status(status.as_u16())));
        }
        match response.text().await {
            Ok(text) => Attempt::Done(
                serde_json::from_str(&text).map_err(|err| CallError::Malformed(err.to_string())),
            ),
            Err(err) => Attempt::Retry(CallError::Transport(err)),
        }
    }
}

/// `key` as a header value marked sensitive, so no debug output shows it.
fn header_value(key: &Secret) -> Result<HeaderValue, CallError> {
    let mut value = HeaderValue::from_str(key.expose()).map_err(|_| CallError::BadKey)?;
    value.set_sensitive(true);
    Ok(value)
}

/// An error with its causes, for a log line.
fn error_text(err: &CallError) -> String {
    let mut text = err.to_string();
    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

/// The summary in a successful reply's text blocks, and the reply's cost.
fn finish(row: &ProviderRow, reply: &MessagesReply) -> Result<HttpReply, CallError> {
    let cost_usd = cost(row, reply.usage.as_ref());
    if reply.stop_reason.as_deref() == Some("max_tokens") {
        return Err(CallError::MaxTokens { cost_usd });
    }
    let text: String = reply
        .content
        .iter()
        .filter(|block| block.kind == "text")
        .filter_map(|block| block.text.as_deref())
        .collect();
    let summary = reply::parse(&text).map_err(|source| CallError::Reply { source, cost_usd })?;
    Ok(HttpReply { summary, cost_usd })
}

/// US dollars for `usage`: every input and cache token at the input rate, the
/// output tokens at the output rate. A reply without usage costs nothing and
/// is logged, since the spend cap then undercounts it.
#[expect(
    clippy::cast_precision_loss,
    reason = "token counts stay far below 2^52, where f64 is exact"
)]
fn cost(row: &ProviderRow, usage: Option<&Usage>) -> f64 {
    let Some(usage) = usage else {
        warn!(
            provider = row.id,
            "summarizer: the reply reported no token usage; counting it as free"
        );
        return 0.0;
    };
    let count = |tokens: Option<u64>| tokens.unwrap_or(0);
    let input = count(usage.input_tokens)
        .saturating_add(count(usage.cache_creation_input_tokens))
        .saturating_add(count(usage.cache_read_input_tokens));
    let output = count(usage.output_tokens);
    (input as f64 * row.input_usd_per_million + output as f64 * row.output_usd_per_million)
        / 1_000_000.0
}

/// One attempt's outcome: final, or worth the one retry.
enum Attempt {
    Done(Result<MessagesReply, CallError>),
    Retry(CallError),
}

/// The Messages reply, as far as the summarizer reads it.
#[derive(Debug, Deserialize)]
struct MessagesReply {
    #[serde(default)]
    content: Vec<Block>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
struct Block {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
}

/// Token counts; a count the reply leaves out is zero tokens.
#[derive(Debug, Deserialize)]
#[expect(
    clippy::struct_field_names,
    reason = "the fields are the reply's own names"
)]
struct Usage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests fail loudly on setup errors")]
mod tests {
    use super::*;
    use crate::summarizer::providers;
    use crate::user_env;
    use axum::Router;
    use axum::extract::State;
    use axum::http::HeaderMap;
    use axum::routing::post;
    use protocol::AlertKind;
    use std::collections::VecDeque;
    use std::future::{Future, IntoFuture as _};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::task::JoinHandle;

    /// A hang guard, never a must-beat bound: a passing call answers in
    /// milliseconds.
    const GUARD: Duration = Duration::from_secs(120);
    /// A key no error or log line may ever contain.
    const KEY_SENTINEL: &str = "sentinel-key-0b7d3e95";
    const PROMPT: &str = "Summarize: the agent ran the tests and they pass.";
    const GOOD: &str = r#"{"kind": "done_waiting", "headline": "Tests pass", "notification": "The agent ran the tests; they pass."}"#;

    /// Generous timeouts, so a loaded machine never times a good call out,
    /// and a short retry wait.
    fn quick() -> HttpConfig {
        HttpConfig {
            connect_timeout: Duration::from_secs(60),
            total_timeout: Duration::from_secs(60),
            retry_delay: Duration::from_millis(10),
        }
    }

    fn key(text: &str) -> Secret {
        Secret::new(text.to_owned())
    }

    fn test_row(endpoint: String) -> ProviderRow {
        ProviderRow {
            id: "test-provider",
            name: "Test provider",
            endpoint: Box::leak(endpoint.into_boxed_str()),
            model: "test-model",
            key_header: "x-test-key",
            version_header: ("x-test-version", "7"),
            input_usd_per_million: 0.30,
            output_usd_per_million: 1.20,
            default_key_var: "RT_SUMMARIZER_HTTP_TEST_UNUSED",
            privacy: "test",
        }
    }

    fn client(config: HttpConfig) -> (HttpClient, Arc<Rejections>) {
        let rejections = Arc::new(Rejections::default());
        let client = HttpClient::new(config, Arc::clone(&rejections)).expect("the client builds");
        (client, rejections)
    }

    async fn guarded<F: Future>(future: F) -> F::Output {
        tokio::time::timeout(GUARD, future)
            .await
            .expect("the call hung")
    }

    fn reply_body(text: &str, usage: Option<serde_json::Value>) -> String {
        let mut body = json!({
            "content": [{"type": "text", "text": text}],
            "stop_reason": "end_turn",
        });
        if let Some(usage) = usage {
            body["usage"] = usage;
        }
        body.to_string()
    }

    fn ok(text: &str) -> (u16, String) {
        (
            200,
            reply_body(
                text,
                Some(json!({"input_tokens": 1000, "output_tokens": 100})),
            ),
        )
    }

    /// Aborts the mock server when the test ends.
    struct Server(JoinHandle<()>);

    impl Drop for Server {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    async fn serve(router: Router) -> (ProviderRow, Server) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the mock");
        let addr = listener.local_addr().expect("the mock's address");
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .into_future()
                .await
                .expect("the mock serves");
        });
        (test_row(format!("http://{addr}/v1/messages")), Server(task))
    }

    /// Answers each request with the next scripted reply, recording what it
    /// was sent.
    #[derive(Default)]
    struct Script {
        replies: Mutex<VecDeque<(u16, String)>>,
        hits: AtomicUsize,
        seen: Mutex<Vec<(HeaderMap, String)>>,
    }

    impl Script {
        fn hits(&self) -> usize {
            self.hits.load(Ordering::SeqCst)
        }
    }

    async fn scripted_reply(
        State(script): State<Arc<Script>>,
        headers: HeaderMap,
        body: String,
    ) -> (axum::http::StatusCode, String) {
        script.hits.fetch_add(1, Ordering::SeqCst);
        script
            .seen
            .lock()
            .expect("not poisoned")
            .push((headers, body));
        let (status, body) = script
            .replies
            .lock()
            .expect("not poisoned")
            .pop_front()
            .unwrap_or_else(|| (500, "no scripted reply".to_owned()));
        (
            axum::http::StatusCode::from_u16(status).expect("a valid status"),
            body,
        )
    }

    async fn scripted(replies: Vec<(u16, String)>) -> (ProviderRow, Arc<Script>, Server) {
        let script = Arc::new(Script {
            replies: Mutex::new(replies.into()),
            ..Script::default()
        });
        let router = Router::new()
            .route("/v1/messages", post(scripted_reply))
            .with_state(Arc::clone(&script));
        let (row, server) = serve(router).await;
        (row, script, server)
    }

    #[tokio::test]
    async fn a_200_reply_parses_into_kind_headline_and_notification() {
        let (row, script, _server) = scripted(vec![ok(GOOD)]).await;
        let (client, _) = client(quick());
        let reply = guarded(client.summarize(&row, &key(KEY_SENTINEL), PROMPT))
            .await
            .expect("the call succeeds");
        assert_eq!(reply.summary.kind, AlertKind::DoneWaiting);
        assert_eq!(reply.summary.headline, "Tests pass");
        assert_eq!(
            reply.summary.notification,
            "The agent ran the tests; they pass."
        );
        assert_eq!(script.hits(), 1);
    }

    #[tokio::test]
    async fn the_request_carries_the_key_version_model_and_thinking_disabled() {
        let (row, script, _server) = scripted(vec![ok(GOOD)]).await;
        let (client, _) = client(quick());
        guarded(client.summarize(&row, &key(KEY_SENTINEL), PROMPT))
            .await
            .expect("the call succeeds");
        let seen = script.seen.lock().expect("not poisoned");
        let (headers, body) = seen.first().expect("one request");
        assert_eq!(
            headers.get("x-test-key").expect("the key header"),
            KEY_SENTINEL
        );
        assert_eq!(
            headers.get("x-test-version").expect("the version header"),
            "7"
        );
        let body: serde_json::Value = serde_json::from_str(body).expect("a JSON body");
        assert_eq!(body["model"], "test-model");
        assert_eq!(body["max_tokens"], 4000);
        assert_eq!(body["thinking"], json!({"type": "disabled"}));
        assert_eq!(
            body["messages"],
            json!([{"role": "user", "content": PROMPT}])
        );
    }

    #[tokio::test]
    async fn a_fenced_reply_after_a_thinking_block_parses() {
        let body = json!({
            "content": [
                {"type": "thinking", "thinking": "{\"kind\": \"needs_answer\", \"headline\": \"x\", \"notification\": \"x\"}"},
                {"type": "text", "text": format!("```json\n{GOOD}\n```")},
            ],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 10},
        });
        let (row, _script, _server) = scripted(vec![(200, body.to_string())]).await;
        let (client, _) = client(quick());
        let reply = guarded(client.summarize(&row, &key(KEY_SENTINEL), PROMPT))
            .await
            .expect("the call succeeds");
        assert_eq!(reply.summary.kind, AlertKind::DoneWaiting);
    }

    #[tokio::test]
    async fn a_429_then_a_200_succeeds_after_one_retry() {
        let (row, script, _server) = scripted(vec![(429, "slow down".to_owned()), ok(GOOD)]).await;
        let (client, _) = client(quick());
        guarded(client.summarize(&row, &key(KEY_SENTINEL), PROMPT))
            .await
            .expect("the retry succeeds");
        assert_eq!(script.hits(), 2);
    }

    #[tokio::test]
    async fn a_second_429_fails() {
        let (row, script, _server) = scripted(vec![
            (429, "slow down".to_owned()),
            (429, "slow down".to_owned()),
            ok(GOOD),
        ])
        .await;
        let (client, _) = client(quick());
        let err = guarded(client.summarize(&row, &key(KEY_SENTINEL), PROMPT))
            .await
            .expect_err("two 429s fail");
        assert!(matches!(err, CallError::Status(429)), "{err:?}");
        assert_eq!(script.hits(), 2, "one retry, no more");
    }

    #[tokio::test]
    async fn a_5xx_is_retried_once() {
        let (row, script, _server) = scripted(vec![(503, "busy".to_owned()), ok(GOOD)]).await;
        let (client, _) = client(quick());
        guarded(client.summarize(&row, &key(KEY_SENTINEL), PROMPT))
            .await
            .expect("the retry succeeds");
        assert_eq!(script.hits(), 2);

        let (row, script, _server) =
            scripted(vec![(500, "down".to_owned()), (502, "down".to_owned())]).await;
        let err = guarded(client.summarize(&row, &key(KEY_SENTINEL), PROMPT))
            .await
            .expect_err("two 5xx fail");
        assert!(matches!(err, CallError::Status(502)), "{err:?}");
        assert_eq!(script.hits(), 2);
    }

    #[tokio::test]
    async fn a_dropped_connection_is_retried_once() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the listener");
        let addr = listener.local_addr().expect("the listener's address");
        let accepted = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&accepted);
        let _server = Server(tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                drop(stream);
            }
        }));
        let row = test_row(format!("http://{addr}/v1/messages"));
        let (client, _) = client(quick());
        let err = guarded(client.summarize(&row, &key(KEY_SENTINEL), PROMPT))
            .await
            .expect_err("a dropped connection fails");
        assert!(matches!(err, CallError::Transport(_)), "{err:?}");
        assert_eq!(accepted.load(Ordering::SeqCst), 2, "one retry, no more");
    }

    #[tokio::test]
    async fn a_400_is_not_retried() {
        let (row, script, _server) =
            scripted(vec![(400, "bad request".to_owned()), ok(GOOD)]).await;
        let (client, rejections) = client(quick());
        let err = guarded(client.summarize(&row, &key(KEY_SENTINEL), PROMPT))
            .await
            .expect_err("a 400 fails");
        assert!(matches!(err, CallError::Status(400)), "{err:?}");
        assert_eq!(script.hits(), 1);
        assert!(!rejections.is_rejected(row.id, &key(KEY_SENTINEL)));
    }

    #[tokio::test]
    async fn a_401_marks_the_key_rejected_and_does_not_retry() {
        for status in [401, 403] {
            let (row, script, _server) = scripted(vec![(status, "no".to_owned()), ok(GOOD)]).await;
            let (client, rejections) = client(quick());
            let err = guarded(client.summarize(&row, &key(KEY_SENTINEL), PROMPT))
                .await
                .expect_err("a refused key fails");
            assert!(matches!(err, CallError::Rejected), "{status}: {err:?}");
            assert_eq!(script.hits(), 1, "{status} is not retried");
            assert!(
                rejections.is_rejected(row.id, &key(KEY_SENTINEL)),
                "{status}"
            );
        }
    }

    #[tokio::test]
    async fn a_rejected_key_makes_no_further_calls_until_the_key_changes() {
        let (row, script, _server) = scripted(vec![(401, "no".to_owned()), ok(GOOD)]).await;
        let (client, _) = client(quick());
        guarded(client.summarize(&row, &key("refused-key"), PROMPT))
            .await
            .expect_err("the key is refused");
        let err = guarded(client.summarize(&row, &key("refused-key"), PROMPT))
            .await
            .expect_err("the same key makes no call");
        assert!(matches!(err, CallError::Rejected), "{err:?}");
        assert_eq!(script.hits(), 1, "no request went out with the refused key");
        guarded(client.summarize(&row, &key("new-key"), PROMPT))
            .await
            .expect("a new key is tried");
        assert_eq!(script.hits(), 2);
    }

    #[tokio::test]
    async fn a_timeout_fails_after_one_retry() {
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        let router = Router::new().route(
            "/v1/messages",
            post(move || {
                counter.fetch_add(1, Ordering::SeqCst);
                std::future::pending::<String>()
            }),
        );
        let (row, _server) = serve(router).await;
        let (client, _) = client(HttpConfig {
            total_timeout: Duration::from_millis(100),
            ..quick()
        });
        let err = guarded(client.summarize(&row, &key(KEY_SENTINEL), PROMPT))
            .await
            .expect_err("a reply that never comes times out");
        assert!(
            matches!(&err, CallError::Transport(source) if source.is_timeout()),
            "{err:?}"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 2, "one retry, no more");
    }

    #[tokio::test]
    async fn an_unknown_kind_is_a_failure() {
        let text = r#"{"kind": "celebrate", "headline": "h", "notification": "n"}"#;
        let (row, script, _server) = scripted(vec![ok(text), ok(GOOD)]).await;
        let (client, _) = client(quick());
        let err = guarded(client.summarize(&row, &key(KEY_SENTINEL), PROMPT))
            .await
            .expect_err("an unknown kind fails");
        assert!(
            matches!(
                &err,
                CallError::Reply { source: ReplyError::UnknownKind, cost_usd } if *cost_usd > 0.0
            ),
            "the billed reply keeps its cost: {err:?}"
        );
        assert_eq!(script.hits(), 1, "a bad reply is not retried");
    }

    #[tokio::test]
    async fn stop_reason_max_tokens_is_a_failure() {
        let body = json!({
            "content": [{"type": "text", "text": GOOD}],
            "stop_reason": "max_tokens",
            "usage": {"input_tokens": 1000, "output_tokens": 4000},
        });
        let (row, script, _server) = scripted(vec![(200, body.to_string()), ok(GOOD)]).await;
        let (client, _) = client(quick());
        let err = guarded(client.summarize(&row, &key(KEY_SENTINEL), PROMPT))
            .await
            .expect_err("a cut reply fails");
        let expected = (1000.0 * 0.30 + 4000.0 * 1.20) / 1_000_000.0;
        assert!(
            matches!(&err, CallError::MaxTokens { cost_usd } if (cost_usd - expected).abs() < 1e-12),
            "{err:?}"
        );
        assert_eq!(script.hits(), 1);
    }

    #[tokio::test]
    async fn a_reply_that_is_not_json_is_malformed() {
        let (row, _script, _server) = scripted(vec![(200, "<html>".to_owned())]).await;
        let (client, _) = client(quick());
        let err = guarded(client.summarize(&row, &key(KEY_SENTINEL), PROMPT))
            .await
            .expect_err("an HTML page fails");
        assert!(matches!(err, CallError::Malformed(_)), "{err:?}");
    }

    #[tokio::test]
    async fn cost_comes_from_usage_tokens() {
        let usage = json!({"input_tokens": 1_000_000, "output_tokens": 1_000_000});
        let (row, _script, _server) = scripted(vec![(200, reply_body(GOOD, Some(usage)))]).await;
        let (client, _) = client(quick());
        let reply = guarded(client.summarize(&row, &key(KEY_SENTINEL), PROMPT))
            .await
            .expect("the call succeeds");
        assert!((reply.cost_usd - 1.50).abs() < 1e-9, "{}", reply.cost_usd);
    }

    #[tokio::test]
    async fn cache_tokens_are_priced_at_the_input_rate() {
        let usage = json!({
            "input_tokens": 400_000,
            "cache_creation_input_tokens": 300_000,
            "cache_read_input_tokens": 300_000,
            "output_tokens": 0,
        });
        let (row, _script, _server) = scripted(vec![(200, reply_body(GOOD, Some(usage)))]).await;
        let (client, _) = client(quick());
        let reply = guarded(client.summarize(&row, &key(KEY_SENTINEL), PROMPT))
            .await
            .expect("the call succeeds");
        assert!((reply.cost_usd - 0.30).abs() < 1e-9, "{}", reply.cost_usd);
    }

    #[tokio::test]
    async fn a_reply_without_usage_costs_zero_and_succeeds() {
        let (row, _script, _server) = scripted(vec![(200, reply_body(GOOD, None))]).await;
        let (client, _) = client(quick());
        let reply = guarded(client.summarize(&row, &key(KEY_SENTINEL), PROMPT))
            .await
            .expect("the call succeeds");
        assert!(reply.cost_usd.abs() < f64::EPSILON, "{}", reply.cost_usd);
        assert_eq!(reply.summary.headline, "Tests pass");
    }

    #[tokio::test]
    async fn the_key_never_appears_in_an_error() {
        let echo = format!("unknown key {KEY_SENTINEL}");
        let (row, _script, _server) = scripted(vec![(400, echo.clone()), (401, echo)]).await;
        let (client, _) = client(quick());
        for _ in 0..3 {
            let err = guarded(client.summarize(&row, &key(KEY_SENTINEL), PROMPT))
                .await
                .expect_err("the call fails");
            let shown = anyhow::Error::new(err);
            assert!(!format!("{shown:#}").contains(KEY_SENTINEL), "{shown:#}");
            assert!(!format!("{shown:?}").contains(KEY_SENTINEL), "{shown:?}");
        }
        let err = guarded(client.summarize(&row, &key("bad\nkey"), PROMPT))
            .await
            .expect_err("a key with a newline fails");
        assert!(matches!(err, CallError::BadKey), "{err:?}");
    }

    /// Holds each request until the test lets it go, counting the requests
    /// in flight.
    struct Gate {
        current: AtomicUsize,
        peak: AtomicUsize,
        release: tokio::sync::Semaphore,
        hits: tokio::sync::watch::Sender<usize>,
    }

    async fn gated_reply(State(gate): State<Arc<Gate>>) -> (axum::http::StatusCode, String) {
        let now = gate.current.fetch_add(1, Ordering::SeqCst) + 1;
        gate.peak.fetch_max(now, Ordering::SeqCst);
        gate.hits.send_modify(|hits| *hits += 1);
        gate.release
            .acquire()
            .await
            .expect("the gate stays open")
            .forget();
        gate.current.fetch_sub(1, Ordering::SeqCst);
        let (status, body) = ok(GOOD);
        (
            axum::http::StatusCode::from_u16(status).expect("a valid status"),
            body,
        )
    }

    #[tokio::test]
    async fn no_more_than_four_calls_run_at_once() {
        let gate = Arc::new(Gate {
            current: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            release: tokio::sync::Semaphore::new(0),
            hits: tokio::sync::watch::Sender::new(0),
        });
        let mut hits = gate.hits.subscribe();
        let router = Router::new()
            .route("/v1/messages", post(gated_reply))
            .with_state(Arc::clone(&gate));
        let (row, _server) = serve(router).await;
        let client = Arc::new(client(quick()).0);
        let calls: Vec<_> = (0..6)
            .map(|_| {
                let client = Arc::clone(&client);
                tokio::spawn(
                    async move { client.summarize(&row, &key(KEY_SENTINEL), PROMPT).await },
                )
            })
            .collect();
        for reached in [4, 5, 6] {
            guarded(hits.wait_for(|hits| *hits >= reached))
                .await
                .expect("the gate's sender lives");
            gate.release.add_permits(1);
        }
        gate.release.add_permits(6);
        for call in calls {
            guarded(call)
                .await
                .expect("the call task ends")
                .expect("the call succeeds");
        }
        assert_eq!(gate.peak.load(Ordering::SeqCst), MAX_IN_FLIGHT);
    }

    /// Measures whether the live endpoint honours `thinking: disabled`: the
    /// reply must carry no thinking block. Needs the provider's key in its
    /// default variable; run by hand with `--run-ignored only`.
    #[tokio::test]
    #[ignore = "calls the real provider with the user's key"]
    async fn live_provider_honours_thinking_disabled() {
        let row = providers::row("deepseek").expect("the table has the row");
        let (secret, _) = user_env::resolve(row.default_key_var).expect("the key variable is set");
        let (client, _) = client(HttpConfig::default());
        let header = header_value(&secret).expect("the key fits a header");
        let prompt = format!(
            "Reply with only this JSON object, filled in for the report below: {GOOD}\n\nReport: {PROMPT}"
        );
        let reply = guarded(client.send_with_retry(row, &header, &prompt))
            .await
            .expect("the live call succeeds");
        assert!(
            reply.content.iter().all(|block| block.kind != "thinking"),
            "the reply carried a thinking block: {:?}",
            reply
                .content
                .iter()
                .map(|block| &block.kind)
                .collect::<Vec<_>>()
        );
        let usage = reply.usage.as_ref().expect("the reply reports usage");
        assert!(usage.output_tokens.unwrap_or(0) < 1000, "{usage:?}");
    }
}
