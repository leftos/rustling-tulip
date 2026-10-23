//! The HTTP `/mock` route: an agent's `show_mock` page, written beside the
//! session and broadcast to clients.
use super::{Hub, StateEvent};
use axum::body::Body;
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use tracing::warn;

/// Most of a mock body the endpoint reads; a larger body is refused `413`.
const MOCK_BODY_LIMIT: usize = 8 << 20;

/// The body the `rt-tracer mcp` server posts.
#[derive(Deserialize)]
struct MockBody {
    title: String,
    html: String,
}

/// `POST /mock/{session_id}` — a page an agent asked to show the user.
///
/// Auth mirrors `/hook`: the daemon's own token as a bearer, compared in
/// constant time. The page is written to `sessions/<id>/mock.html` and
/// announced with [`StateEvent::MockShown`]; the answer is `204`. A body over
/// 8 MiB is `413`, one that is not `{title, html}` is `400`, an unknown
/// session is `404` and a failed write is `500`.
pub(super) async fn mock_handler(
    State(hub): State<Hub>,
    AxumPath(session_id): AxumPath<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let Some(token) = super::hooks::presented_bearer(&headers) else {
        return (StatusCode::UNAUTHORIZED, "missing bearer token").into_response();
    };
    if !crate::secret::constant_time_eq(token.as_bytes(), hub.auth_token.as_bytes()) {
        return (StatusCode::UNAUTHORIZED, "bad bearer token").into_response();
    }
    let Ok(body) = axum::body::to_bytes(body, MOCK_BODY_LIMIT).await else {
        return (StatusCode::PAYLOAD_TOO_LARGE, "mock body too large").into_response();
    };
    let Ok(mock) = serde_json::from_slice::<MockBody>(&body) else {
        return (StatusCode::BAD_REQUEST, "expected {title, html}").into_response();
    };
    if hub.sessions.get(&session_id).is_none() {
        return (StatusCode::NOT_FOUND, "no such session").into_response();
    }
    let path = hub.dirs.sessions_dir.join(&session_id).join("mock.html");
    let written = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .map_err(anyhow::Error::from)
        .and_then(|()| crate::orphan::write_atomic(&path, mock.html.as_bytes()));
    if let Err(err) = written {
        warn!(
            %session_id,
            path = %path.display(),
            error = %format!("{err:#}"),
            "writing the mock page failed"
        );
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "writing the mock page failed",
        )
            .into_response();
    }
    let _ = hub.state_events.send(StateEvent::MockShown {
        session_id,
        title: mock.title,
        path: path.display().to_string(),
    });
    StatusCode::NO_CONTENT.into_response()
}
