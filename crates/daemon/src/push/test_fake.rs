//! A fake push service for the push tests: a loopback HTTP server that
//! answers every POST with one status and keeps what it was sent.

#![expect(
    clippy::expect_used,
    reason = "test helpers assert preconditions with expect; failure messages aid debugging"
)]

use std::future::IntoFuture;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use reqwest::Url;
use tokio::task::JoinHandle;

use super::send::{PushPolicy, check_endpoint};

/// The policy the tests post under: loopback `http` is admitted.
pub(crate) const TESTS: PushPolicy = PushPolicy {
    allow_loopback_http: true,
};

/// Work the fake does when a POST arrives, before it answers.
pub(crate) type Hook = Box<dyn FnOnce() + Send>;

/// What the fake push service answers and what it was sent.
pub(crate) struct Fake {
    status: AtomicU16,
    reply: String,
    reply_headers: Vec<(&'static str, String)>,
    /// Run once, on the first POST, before the answer goes back.
    before_answer: Mutex<Option<Hook>>,
    /// Every POST's headers and body, in arrival order.
    pub seen: Mutex<Vec<(HeaderMap, Bytes)>>,
}

impl Fake {
    /// How many POSTs arrived.
    pub(crate) fn posts(&self) -> usize {
        self.seen.lock().expect("unpoisoned").len()
    }

    /// Answer every later POST with `status`.
    pub(crate) fn set_status(&self, status: u16) {
        self.status.store(status, Ordering::SeqCst);
    }
}

async fn answer(
    State(fake): State<Arc<Fake>>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, HeaderMap, String) {
    fake.seen.lock().expect("unpoisoned").push((headers, body));
    let hook = fake.before_answer.lock().expect("unpoisoned").take();
    if let Some(hook) = hook {
        hook();
    }
    let mut reply_headers = HeaderMap::new();
    for (name, value) in &fake.reply_headers {
        reply_headers.insert(*name, value.parse().expect("a valid header value"));
    }
    (
        StatusCode::from_u16(fake.status.load(Ordering::SeqCst)).expect("a valid status"),
        reply_headers,
        fake.reply.clone(),
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
    serve(status, reply, reply_headers, None).await
}

/// A fake answering `status` that runs `hook` when the first POST arrives,
/// before it answers.
pub(crate) async fn fake_push_service_hooked(status: u16, hook: Hook) -> (Url, Arc<Fake>, Server) {
    serve(status, "", &[], Some(hook)).await
}

async fn serve(
    status: u16,
    reply: &str,
    reply_headers: &[(&'static str, &str)],
    hook: Option<Hook>,
) -> (Url, Arc<Fake>, Server) {
    let fake = Arc::new(Fake {
        status: AtomicU16::new(status),
        reply: reply.to_owned(),
        reply_headers: reply_headers
            .iter()
            .map(|(name, value)| (*name, (*value).to_owned()))
            .collect(),
        before_answer: Mutex::new(hook),
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
