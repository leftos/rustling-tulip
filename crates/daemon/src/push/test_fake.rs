//! A fake push service for the push tests: a loopback HTTP server that
//! answers every POST with one status and keeps what it was sent.

#![expect(
    clippy::expect_used,
    reason = "test helpers assert preconditions with expect; failure messages aid debugging"
)]

use std::collections::HashMap;
use std::future::IntoFuture;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::routing::post;
use reqwest::Url;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use super::send::{PushPolicy, check_endpoint};

/// The policy the tests post under: loopback `http` is admitted.
pub(crate) const TESTS: PushPolicy = PushPolicy {
    allow_loopback_http: true,
};

/// Work the fake does when a POST arrives, before it answers.
pub(crate) type Hook = Box<dyn FnOnce() + Send>;

/// The reply headers a fake answers with.
type ReplyHeaders = Vec<(&'static str, String)>;

/// Per-endpoint answers, by the endpoint's last path segment.
type ByPath = HashMap<String, (u16, ReplyHeaders)>;

/// The answers [`fake_push_service_by_path`] serves: for each path segment,
/// its status and its reply headers.
pub(crate) type PathAnswers<'a> = &'a [(&'a str, u16, &'a [(&'static str, &'static str)])];

/// What the fake push service answers and what it was sent.
pub(crate) struct Fake {
    status: AtomicU16,
    reply: String,
    reply_headers: Vec<(&'static str, String)>,
    /// Run once, on the first POST, before the answer goes back.
    before_answer: Mutex<Option<Hook>>,
    /// Answered only once this is notified, when set.
    gate: Option<Arc<Notify>>,
    /// Per-endpoint answers, by the endpoint's last path segment.
    by_path: ByPath,
    /// Every POST's headers and body, in arrival order.
    pub seen: Mutex<Vec<(HeaderMap, Bytes)>>,
    /// Every POST's path as it arrived (percent-encoded), in the order of
    /// [`Fake::seen`].
    paths: Mutex<Vec<String>>,
}

impl Fake {
    /// How many POSTs arrived.
    pub(crate) fn posts(&self) -> usize {
        self.seen.lock().expect("unpoisoned").len()
    }

    /// Every POST's path, in arrival order.
    pub(crate) fn paths(&self) -> Vec<String> {
        self.paths.lock().expect("unpoisoned").clone()
    }

    /// Answer every later POST with `status`.
    pub(crate) fn set_status(&self, status: u16) {
        self.status.store(status, Ordering::SeqCst);
    }
}

async fn answer(
    State(fake): State<Arc<Fake>>,
    Path(params): Path<HashMap<String, String>>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, HeaderMap, String) {
    let id = params.get("id").cloned().unwrap_or_default();
    fake.paths
        .lock()
        .expect("unpoisoned")
        .push(uri.path().to_owned());
    fake.seen.lock().expect("unpoisoned").push((headers, body));
    let hook = fake.before_answer.lock().expect("unpoisoned").take();
    if let Some(hook) = hook {
        hook();
    }
    if let Some(gate) = &fake.gate {
        gate.notified().await;
    }
    let (status, reply, wanted_headers) = match fake.by_path.get(&id) {
        Some((status, headers)) => (*status, String::new(), headers.clone()),
        None => (
            fake.status.load(Ordering::SeqCst),
            fake.reply.clone(),
            fake.reply_headers.clone(),
        ),
    };
    let mut answer_headers = HeaderMap::new();
    for (name, value) in &wanted_headers {
        answer_headers.insert(*name, value.parse().expect("a valid header value"));
    }
    (
        StatusCode::from_u16(status).expect("a valid status"),
        answer_headers,
        reply,
    )
}

/// Aborts the fake push service when the test ends.
pub(crate) struct Server(JoinHandle<()>);

impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A fake answering `status` with `reply`; the endpoint is
/// `http://127.0.0.1:{port}/push/abc`.
pub(crate) async fn fake_push_service(status: u16, reply: &str) -> (Url, Arc<Fake>, Server) {
    fake_push_service_with(status, reply, &[]).await
}

/// A fake answering `status` with `reply` and `reply_headers`.
pub(crate) async fn fake_push_service_with(
    status: u16,
    reply: &str,
    reply_headers: &[(&'static str, &str)],
) -> (Url, Arc<Fake>, Server) {
    serve(status, reply, reply_headers, None, None).await
}

/// A fake answering `status` that runs `hook` when the first POST arrives,
/// before it answers.
pub(crate) async fn fake_push_service_hooked(status: u16, hook: Hook) -> (Url, Arc<Fake>, Server) {
    serve(status, "", &[], Some(hook), None).await
}

/// A fake answering `status` only once `gate` is notified.
pub(crate) async fn fake_push_service_gated(
    status: u16,
    gate: Arc<Notify>,
) -> (Url, Arc<Fake>, Server) {
    serve(status, "", &[], None, Some(gate)).await
}

/// A fake whose answer depends on the endpoint's last path segment: each
/// named segment answers its status and reply headers, and any other one
/// answers 500.
pub(crate) async fn fake_push_service_by_path(
    answers: PathAnswers<'_>,
) -> (Url, Arc<Fake>, Server) {
    let by_path = answers
        .iter()
        .map(|(id, status, headers)| {
            let headers = headers
                .iter()
                .map(|(name, value)| (*name, (*value).to_owned()))
                .collect();
            ((*id).to_owned(), (*status, headers))
        })
        .collect();
    let fake = Arc::new(Fake {
        status: AtomicU16::new(500),
        reply: String::new(),
        reply_headers: Vec::new(),
        before_answer: Mutex::new(None),
        gate: None,
        by_path,
        seen: Mutex::new(Vec::new()),
        paths: Mutex::new(Vec::new()),
    });
    serve_fake(fake).await
}

async fn serve(
    status: u16,
    reply: &str,
    reply_headers: &[(&'static str, &str)],
    hook: Option<Hook>,
    gate: Option<Arc<Notify>>,
) -> (Url, Arc<Fake>, Server) {
    let fake = Arc::new(Fake {
        status: AtomicU16::new(status),
        reply: reply.to_owned(),
        reply_headers: reply_headers
            .iter()
            .map(|(name, value)| (*name, (*value).to_owned()))
            .collect(),
        before_answer: Mutex::new(hook),
        gate,
        by_path: HashMap::new(),
        seen: Mutex::new(Vec::new()),
        paths: Mutex::new(Vec::new()),
    });
    serve_fake(fake).await
}

/// The origin of a fake's `url`, `http://127.0.0.1:{port}`: a fake push
/// relay's origin, under which it also answers the relay's
/// `/v1/apns/{environment}/{token}` and `/v1/fcm/{token}`, by the token.
pub(crate) fn origin_of(url: &Url) -> Url {
    Url::parse(&url.origin().ascii_serialization()).expect("an origin is a URL")
}

/// Serve `fake` on a loopback port.
async fn serve_fake(fake: Arc<Fake>) -> (Url, Arc<Fake>, Server) {
    let router = Router::new()
        .route("/push/{id}", post(answer))
        .route("/v1/apns/{environment}/{id}", post(answer))
        .route("/v1/fcm/{id}", post(answer))
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
