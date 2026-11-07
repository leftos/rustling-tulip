//! Delivering one encrypted push message: the endpoint allowlist, the
//! per-session `Topic`, and the POST with the push service's answer sorted
//! into an [`Outcome`].

use std::time::Duration;

use anyhow::{Context, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::Url;
use reqwest::header::{AUTHORIZATION, CONTENT_ENCODING, CONTENT_TYPE, HeaderMap, RETRY_AFTER};
use sha2_011::{Digest, Sha256};

/// How long one push may take, connect to answer.
const TIMEOUT: Duration = Duration::from_secs(30);

/// How long a push service keeps an undelivered message: six hours.
const TTL_SECONDS: &str = "21600";

/// The most of a push service's answer kept as the `reason`.
const REASON_CHARS: usize = 200;

/// The `Topic` for `session_id`'s pushes: `rt-` and the first 12 characters
/// of the base64url SHA-256 of the id, so a phone that was offline gets only
/// the latest alert per session.
pub(crate) fn topic(session_id: &str) -> String {
    let digest = URL_SAFE_NO_PAD.encode(Sha256::digest(session_id.as_bytes()));
    format!("rt-{}", &digest[..12])
}

/// Which endpoints a push may go to besides the push services.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PushPolicy {
    /// Admit `http://127.0.0.1:{port}/…`, for the tests' fake push service.
    /// Always `false` in the daemon.
    pub allow_loopback_http: bool,
}

/// Whether `host` is one of the browsers' push services.
fn is_push_service(host: &str) -> bool {
    host == "web.push.apple.com"
        || host.ends_with(".push.apple.com")
        || host == "fcm.googleapis.com"
        || host.ends_with(".push.services.mozilla.com")
        || host.ends_with(".notify.windows.com")
}

/// Checks a subscription's endpoint before the daemon posts to it: the
/// endpoint comes from a paired device, so only `https` to a known push
/// service passes (and, under `policy`, plain `http` to `127.0.0.1`).
///
/// # Errors
///
/// `endpoint` is not a URL, or is not `https` to a push service; the message
/// names its host.
pub(crate) fn check_endpoint(endpoint: &str, policy: PushPolicy) -> anyhow::Result<Url> {
    let url = Url::parse(endpoint).context("the push endpoint is not a URL")?;
    let host = url.host_str().unwrap_or_default();
    let allowed = match url.scheme() {
        "https" => is_push_service(host),
        "http" => policy.allow_loopback_http && host == "127.0.0.1",
        _ => false,
    };
    if !allowed {
        bail!("refused the push endpoint on host {host:?}: not https to a known push service");
    }
    Ok(url)
}

/// What the push service's answer means for the subscription.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Accepted (any 2xx).
    Sent,
    /// The subscription no longer exists (404, 410): delete it.
    Gone,
    /// Refused for any other reason, 403 (a bad VAPID JWT) among them; the
    /// subscription is kept.
    Rejected { status: u16, reason: String },
    /// The body was too large (413): a payload bug.
    PayloadTooLarge { size: usize },
    /// Throttled or failing (429, 5xx): try again later, after `retry_after`
    /// when the push service gave a `Retry-After` in seconds.
    Retry {
        status: u16,
        retry_after: Option<Duration>,
    },
}

/// The HTTP client pushes go out on. It follows no redirect and uses no
/// proxy, so a push reaches only the host [`check_endpoint`] approved.
///
/// # Errors
///
/// The TLS setup failed, for one when the system's root certificates cannot
/// be loaded.
pub(crate) fn client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .context("the push HTTP client did not build")
}

/// A `Retry-After` given in delta-seconds; an HTTP date or a malformed value
/// gives `None`.
fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    headers
        .get(RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

/// Posts `body`, already encrypted for the subscription, to `endpoint`
/// (checked by [`check_endpoint`]) and sorts the answer.
///
/// # Errors
///
/// The push service could not be reached or did not answer.
pub(crate) async fn send(
    client: &reqwest::Client,
    endpoint: &Url,
    body: Vec<u8>,
    topic: &str,
    jwt: &str,
    public_key_b64: &str,
) -> anyhow::Result<Outcome> {
    let size = body.len();
    let host = endpoint.host_str().unwrap_or_default().to_owned();
    let response = client
        .post(endpoint.clone())
        .header("TTL", TTL_SECONDS)
        .header("Urgency", "high")
        .header("Topic", topic)
        .header(CONTENT_ENCODING, "aes128gcm")
        .header(CONTENT_TYPE, "application/octet-stream")
        .header(AUTHORIZATION, format!("vapid t={jwt}, k={public_key_b64}"))
        .body(body)
        .send()
        .await
        .with_context(|| format!("the push service at {host} could not be reached"))?;
    if response.status().is_success() {
        return Ok(Outcome::Sent);
    }
    let status = response.status().as_u16();
    let retry_after = retry_after(response.headers());
    let text = match response.text().await {
        Ok(text) => text,
        Err(err) => format!("(the response body could not be read: {err})"),
    };
    let reason: String = text.chars().take(REASON_CHARS).collect();
    let outcome = match status {
        404 | 410 => Outcome::Gone,
        413 => Outcome::PayloadTooLarge { size },
        429 | 500..=599 => Outcome::Retry {
            status,
            retry_after,
        },
        _ => Outcome::Rejected {
            status,
            reason: reason.clone(),
        },
    };
    if let Outcome::PayloadTooLarge { size } = outcome {
        tracing::warn!(status, size, %reason, %host, "push service refused the payload's size");
    } else {
        tracing::warn!(status, %reason, %host, "push service did not take the push");
    }
    Ok(outcome)
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use std::future::IntoFuture;
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::body::Bytes;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::post;
    use tokio::task::JoinHandle;

    use super::{Duration, Outcome, PushPolicy, Url, check_endpoint, client, send, topic};

    #[test]
    fn topic_obeys_apples_rule() {
        let charset = regex::Regex::new("^[A-Za-z0-9_-]+$").expect("a valid pattern");
        for n in 0..200 {
            let id = uuid::Uuid::new_v4().to_string();
            let id = if n % 2 == 0 {
                id
            } else {
                format!("session-{n}")
            };
            let topic = topic(&id);
            assert_eq!(topic.len(), 15, "{topic}");
            assert!(topic.starts_with("rt-"), "{topic}");
            assert!(charset.is_match(&topic), "{topic}");
            assert!(topic.len() <= 32, "{topic}");
            assert_ne!(topic.len() % 4, 1, "{topic}");
        }
    }

    const PRODUCTION: PushPolicy = PushPolicy {
        allow_loopback_http: false,
    };
    const TESTS: PushPolicy = PushPolicy {
        allow_loopback_http: true,
    };

    #[test]
    fn push_service_endpoints_pass() {
        for endpoint in [
            "https://web.push.apple.com/QGuQyavXutnMtA",
            "https://api.push.apple.com/3/device/abc",
            "https://fcm.googleapis.com/fcm/send/abc:def",
            "https://updates.push.services.mozilla.com/wpush/v2/gAAAA",
            "https://wns2-by3p.notify.windows.com/w/?token=BQYAAA",
        ] {
            let url = check_endpoint(endpoint, PRODUCTION).expect(endpoint);
            assert_eq!(url.as_str(), endpoint);
        }
    }

    #[test]
    fn lookalike_hosts_are_refused_by_name() {
        for (endpoint, host) in [
            (
                "https://web.push.apple.com.evil.example/x",
                "web.push.apple.com.evil.example",
            ),
            ("https://evilpush.apple.com/x", "evilpush.apple.com"),
            ("https://push.apple.com/x", "push.apple.com"),
            (
                "https://fcm.googleapis.com.evil.example/x",
                "fcm.googleapis.com.evil.example",
            ),
            ("https://evilfcm.googleapis.com/x", "evilfcm.googleapis.com"),
            (
                "https://push.services.mozilla.com.evil.example/x",
                "push.services.mozilla.com.evil.example",
            ),
            ("https://evilnotify.windows.com/x", "evilnotify.windows.com"),
            ("https://127.0.0.1:8443/x", "127.0.0.1"),
        ] {
            let err = check_endpoint(endpoint, TESTS).expect_err(endpoint);
            assert!(err.to_string().contains(host), "{endpoint}: {err}");
        }
    }

    #[test]
    fn plain_http_to_a_push_service_is_refused() {
        for endpoint in [
            "http://web.push.apple.com/QGuQyavXutnMtA",
            "http://fcm.googleapis.com/fcm/send/abc",
        ] {
            check_endpoint(endpoint, TESTS).expect_err(endpoint);
        }
    }

    #[test]
    fn loopback_http_passes_only_under_the_test_policy() {
        let endpoint = "http://127.0.0.1:8080/push/abc";
        let err = check_endpoint(endpoint, PRODUCTION).expect_err("production refuses it");
        assert!(err.to_string().contains("127.0.0.1"), "{err}");
        check_endpoint(endpoint, TESTS).expect("the tests' policy admits it");
        check_endpoint("http://localhost:8080/push/abc", TESTS)
            .expect_err("only the literal loopback address");
    }

    /// What the fake push service answers and what it was sent.
    struct Fake {
        status: u16,
        reply: String,
        reply_headers: Vec<(&'static str, String)>,
        seen: Mutex<Vec<(HeaderMap, Bytes)>>,
    }

    async fn answer(
        State(fake): State<Arc<Fake>>,
        headers: HeaderMap,
        body: Bytes,
    ) -> (StatusCode, HeaderMap, String) {
        fake.seen.lock().expect("unpoisoned").push((headers, body));
        let mut reply_headers = HeaderMap::new();
        for (name, value) in &fake.reply_headers {
            reply_headers.insert(*name, value.parse().expect("a valid header value"));
        }
        (
            StatusCode::from_u16(fake.status).expect("a valid status"),
            reply_headers,
            fake.reply.clone(),
        )
    }

    /// Aborts the fake push service when the test ends.
    struct Server(JoinHandle<()>);

    impl Drop for Server {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    async fn fake_push_service(status: u16, reply: &str) -> (Url, Arc<Fake>, Server) {
        fake_push_service_with(status, reply, &[]).await
    }

    async fn fake_push_service_with(
        status: u16,
        reply: &str,
        reply_headers: &[(&'static str, &str)],
    ) -> (Url, Arc<Fake>, Server) {
        let fake = Arc::new(Fake {
            status,
            reply: reply.to_owned(),
            reply_headers: reply_headers
                .iter()
                .map(|(name, value)| (*name, (*value).to_owned()))
                .collect(),
            seen: Mutex::new(Vec::new()),
        });
        let router = Router::new()
            .route("/push/{id}", post(answer))
            .with_state(Arc::clone(&fake));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the fake push service");
        let addr = listener.local_addr().expect("its address");
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .into_future()
                .await
                .expect("the fake push service serves");
        });
        let url = check_endpoint(&format!("http://{addr}/push/abc"), TESTS).expect("loopback");
        (url, fake, Server(task))
    }

    const BODY: &[u8] = b"an encrypted record";

    async fn outcome_for(status: u16, reply: &str) -> Outcome {
        outcome_with(status, reply, &[]).await
    }

    async fn outcome_with(
        status: u16,
        reply: &str,
        reply_headers: &[(&'static str, &str)],
    ) -> Outcome {
        let (url, _fake, _server) = fake_push_service_with(status, reply, reply_headers).await;
        post_to(&url).await
    }

    async fn post_to(url: &Url) -> Outcome {
        send(
            &client().expect("a client"),
            url,
            BODY.to_vec(),
            "rt-AbCdEf012345",
            "header.claims.signature",
            "BPublicKey",
        )
        .await
        .expect("the fake answers")
    }

    #[tokio::test]
    async fn created_is_sent() {
        assert_eq!(outcome_for(201, "").await, Outcome::Sent);
    }

    #[tokio::test]
    async fn not_found_is_gone() {
        assert_eq!(outcome_for(404, "").await, Outcome::Gone);
    }

    #[tokio::test]
    async fn gone_is_gone() {
        assert_eq!(outcome_for(410, "").await, Outcome::Gone);
    }

    #[tokio::test]
    async fn forbidden_is_rejected_with_its_reason() {
        let long = format!(r#"{{"reason":"BadJwtToken"}}{}"#, "x".repeat(300));
        let expected: String = long.chars().take(200).collect();
        assert!(expected.starts_with(r#"{"reason":"BadJwtToken"}"#));
        assert_eq!(
            outcome_for(403, &long).await,
            Outcome::Rejected {
                status: 403,
                reason: expected
            }
        );
    }

    #[tokio::test]
    async fn payload_too_large_carries_the_size() {
        assert_eq!(
            outcome_for(413, "").await,
            Outcome::PayloadTooLarge { size: BODY.len() }
        );
    }

    #[tokio::test]
    async fn no_content_is_sent() {
        assert_eq!(outcome_for(204, "").await, Outcome::Sent);
    }

    #[tokio::test]
    async fn too_many_requests_retries_after_its_delay() {
        assert_eq!(
            outcome_with(429, "", &[("retry-after", "600")]).await,
            Outcome::Retry {
                status: 429,
                retry_after: Some(Duration::from_secs(600))
            }
        );
    }

    #[tokio::test]
    async fn unavailable_retries() {
        assert_eq!(
            outcome_for(503, "").await,
            Outcome::Retry {
                status: 503,
                retry_after: None
            }
        );
    }

    #[tokio::test]
    async fn the_push_client_does_not_follow_redirects() {
        let (elsewhere, elsewhere_fake, _elsewhere_server) = fake_push_service(201, "").await;
        let (url, fake, _server) =
            fake_push_service_with(307, "", &[("location", elsewhere.as_str())]).await;

        let outcome = post_to(&url).await;

        assert!(
            matches!(outcome, Outcome::Rejected { status: 307, .. }),
            "{outcome:?}"
        );
        assert_eq!(fake.seen.lock().expect("unpoisoned").len(), 1);
        assert!(
            elsewhere_fake.seen.lock().expect("unpoisoned").is_empty(),
            "the redirect was followed"
        );
    }

    #[tokio::test]
    async fn every_header_arrives_as_written() {
        let (url, fake, _server) = fake_push_service(201, "").await;
        let outcome = send(
            &client().expect("a client"),
            &url,
            BODY.to_vec(),
            "rt-AbCdEf012345",
            "header.claims.signature",
            "BPublicKey",
        )
        .await
        .expect("the fake answers");
        assert_eq!(outcome, Outcome::Sent);

        let seen = fake.seen.lock().expect("unpoisoned");
        assert_eq!(seen.len(), 1, "one request");
        let (headers, body) = &seen[0];
        assert_eq!(body.as_ref(), BODY);
        for (name, value) in [
            ("ttl", "21600"),
            ("urgency", "high"),
            ("topic", "rt-AbCdEf012345"),
            ("content-encoding", "aes128gcm"),
            ("content-type", "application/octet-stream"),
            (
                "authorization",
                "vapid t=header.claims.signature, k=BPublicKey",
            ),
        ] {
            assert_eq!(
                headers.get(name).and_then(|v| v.to_str().ok()),
                Some(value),
                "{name}"
            );
        }
    }
}
