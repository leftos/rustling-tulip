//! The HTTP `/hook` route: agent hook reports, their bearer check and event time.
use super::Hub;
use axum::body::{Body, Bytes};
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

/// Most of a hook body the endpoint reads; anything larger is treated as an
/// empty body and still answered `204`.
const HOOK_BODY_LIMIT: usize = 1 << 20;

/// `POST /hook/{session_id}` — one agent hook event for a session, from a
/// hook-driven agent session.
///
/// Auth mirrors `/shutdown`: the daemon's own token as a bearer, compared in
/// constant time. The answer is `204` for any authenticated request, whatever
/// the body holds and whether or not the session exists, since the shim
/// ignores it and a rejected event must never hold up the agent.
///
/// The session's hook plugin reads the body (see
/// [`crate::hook_status::deliver`]): a live line goes to the session's
/// live-line channel, untouched by the ordering below, and any other event is
/// mapped and applied. An event for a
/// session id the registry does not hold yet is held raw in
/// [`crate::hook_status::PendingHooks`], then mapped and applied when the
/// spawn inserts the record: the tracer starts the child, whose hooks fire at
/// once, before the daemon has inserted the record, and only the record says
/// which plugin maps them.
///
/// The body is read at most 1 MiB; a larger or unreadable body is treated as
/// empty. A body that is not a JSON object, or an event the mapping does not
/// know, changes nothing and does not order the session.
///
/// Async hooks finish out of order, so the event is applied only when its
/// `X-RT-Hook-Time` (nanoseconds since the epoch) is strictly newer than the
/// last one applied for the session. A missing or unparsable time is the
/// oldest possible and is dropped.
pub(super) async fn hook_handler(
    State(hub): State<Hub>,
    AxumPath(session_id): AxumPath<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let Some(token) = presented_bearer(&headers) else {
        return (StatusCode::UNAUTHORIZED, "missing bearer token").into_response();
    };
    if !crate::secret::constant_time_eq(token.as_bytes(), hub.auth_token.as_bytes()) {
        return (StatusCode::UNAUTHORIZED, "bad bearer token").into_response();
    }
    let body = axum::body::to_bytes(body, HOOK_BODY_LIMIT)
        .await
        .unwrap_or_else(|_| Bytes::new());
    let value = match serde_json::from_slice::<serde_json::Value>(&body) {
        Ok(value) if value.is_object() => value,
        _ => return StatusCode::NO_CONTENT.into_response(),
    };
    let event_time = hook_time(&headers);
    let Some(record) = hub.sessions.get(&session_id) else {
        // A spawn may have started the child — whose hooks fire from its
        // argv — before its record is inserted. Hold the event for the
        // insertion instead of dropping it on the floor.
        hub.pending_hooks
            .defer(&hub, &session_id, event_time, value, body.len());
        return StatusCode::NO_CONTENT.into_response();
    };
    crate::hook_status::deliver(&hub, &session_id, &record, event_time, &value);
    StatusCode::NO_CONTENT.into_response()
}

/// `GET /proof` — the daemon proves to the hook shim that it holds this run's
/// token before the shim sends it a hook body or the bearer token.
///
/// The request carries exactly one `X-RT-Challenge` of 64 lowercase hex (32
/// bytes); the answer is `204` with `x-rt-proof` set to
/// `HMAC-SHA256(hub.proof_key, challenge)` in lowercase hex and no body. A
/// missing, repeated or malformed challenge is `400`. Loopback router only.
pub(super) async fn proof_handler(State(hub): State<Hub>, headers: HeaderMap) -> Response {
    let mut values = headers.get_all("x-rt-challenge").iter();
    let (Some(value), None) = (values.next(), values.next()) else {
        return (StatusCode::BAD_REQUEST, "expected one X-RT-Challenge").into_response();
    };
    let Some(challenge) = value
        .to_str()
        .ok()
        .and_then(tracer_protocol::parse_lower_hex32)
    else {
        return (StatusCode::BAD_REQUEST, "malformed X-RT-Challenge").into_response();
    };
    let proof = tracer_protocol::daemon_proof(&hub.proof_key, &challenge);
    (
        StatusCode::NO_CONTENT,
        [("x-rt-proof", tracer_protocol::to_lower_hex(&proof))],
    )
        .into_response()
}

/// The `X-RT-Hook-Time` header, nanoseconds since the Unix epoch.
fn hook_time(headers: &HeaderMap) -> Option<u128> {
    headers
        .get("x-rt-hook-time")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse().ok())
}

/// The bearer token a request presents, when it uses the expected
/// `Authorization: Bearer <token>` form.
pub(super) fn presented_bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use crate::pty_state;
    use crate::server::build_loopback_router;
    use crate::server::spawn::announce_replayed_prompt;
    use crate::server::test_support::{
        hook_request, idle_record, snapshot_of, test_hub, test_hub_with_attention,
    };
    use protocol::{AttentionReason, SessionMode, SessionStatus};
    use tokio::sync::broadcast;
    use tower::ServiceExt as _;
    #[tokio::test]
    async fn hook_route_requires_a_bearer_token() {
        let (hub, _scratch) = test_hub("hook-auth");
        hub.sessions.insert(idle_record("s1"));
        let body = r#"{"hook_event_name":"UserPromptSubmit"}"#;
        for token in [None, Some("wrong-token")] {
            let response = build_loopback_router(hub.clone())
                .oneshot(hook_request("s1", token, Some("100"), body))
                .await
                .expect("router answers");
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{token:?}");
        }
        let snapshot = snapshot_of(&hub, "s1");
        assert_eq!(snapshot.status, SessionStatus::Idle);
        assert_eq!(snapshot.pending_input, None);
    }

    #[tokio::test]
    async fn hook_route_accepts_an_unknown_session() {
        let (hub, _scratch) = test_hub("hook-unknown");
        let response = build_loopback_router(hub.clone())
            .oneshot(hook_request(
                "no-such-session",
                Some("test-token"),
                Some("100"),
                "{}",
            ))
            .await
            .expect("router answers");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn hook_route_drops_a_stale_event() {
        let (hub, _scratch) = test_hub("hook-stale");
        hub.sessions.insert(idle_record("s1"));
        let newer = r#"{"hook_event_name":"UserPromptSubmit"}"#;
        let older = r#"{"hook_event_name":"Stop"}"#;
        for (hook_time, body) in [("200", newer), ("100", older)] {
            let response = build_loopback_router(hub.clone())
                .oneshot(hook_request(
                    "s1",
                    Some("test-token"),
                    Some(hook_time),
                    body,
                ))
                .await
                .expect("router answers");
            assert_eq!(response.status(), StatusCode::NO_CONTENT);
        }
        assert_eq!(snapshot_of(&hub, "s1").status, SessionStatus::Working);
    }

    #[tokio::test]
    async fn hook_route_applies_a_newer_event() {
        let (hub, _scratch) = test_hub("hook-apply");
        hub.sessions.insert(idle_record("s1"));
        let body = concat!(
            r#"{"hook_event_name":"PreToolUse","tool_name":"AskUserQuestion","#,
            r#""tool_input":{"questions":[{"question":"Which?","header":"Pick","#,
            r#""options":[{"label":"A"},{"label":"B"}],"multiSelect":false}]}}"#,
        );
        let response = build_loopback_router(hub.clone())
            .oneshot(hook_request("s1", Some("test-token"), Some("100"), body))
            .await
            .expect("router answers");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let snapshot = snapshot_of(&hub, "s1");
        assert_eq!(snapshot.status, SessionStatus::AwaitingInput);
        assert_eq!(
            snapshot.pending_input,
            Some(protocol::PendingInput::Question {
                questions: vec![protocol::PendingQuestion {
                    question: "Which?".to_owned(),
                    header: "Pick".to_owned(),
                    options: vec!["A".to_owned(), "B".to_owned()],
                    multi_select: false,
                }],
            })
        );
    }

    #[tokio::test]
    async fn hook_route_sends_a_live_line_to_the_sessions_channel() {
        let (hub, _scratch) = test_hub("hook-live-line");
        hub.sessions.insert(idle_record("s1"));
        let mut lines = hub.live_lines.subscribe("s1");
        let body = concat!(
            r#"{"hook_event_name":"MessageDisplay","prompt_id":"p1","message_id":"m1","#,
            r#""index":1,"final":true,"delta":"Yellow"}"#,
        );
        let response = build_loopback_router(hub.clone())
            .oneshot(hook_request("s1", Some("test-token"), Some("100"), body))
            .await
            .expect("router answers");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let line = lines.try_recv().expect("the route sent the live line");
        assert_eq!(
            line,
            agent_api::LiveLine {
                text: "Yellow".to_owned(),
                index: 1,
                message_key: Some("m1".to_owned()),
                prompt_key: Some("p1".to_owned()),
            }
        );
        let snapshot = snapshot_of(&hub, "s1");
        assert_eq!(snapshot.status, SessionStatus::Idle);
        assert_eq!(snapshot.pending_input, None);
    }

    #[tokio::test]
    async fn hook_route_ignores_a_malformed_body() {
        let (hub, _scratch) = test_hub("hook-malformed");
        hub.sessions.insert(idle_record("s1"));
        let response = build_loopback_router(hub.clone())
            .oneshot(hook_request(
                "s1",
                Some("test-token"),
                Some("100"),
                "not json",
            ))
            .await
            .expect("router answers");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let snapshot = snapshot_of(&hub, "s1");
        assert_eq!(snapshot.status, SessionStatus::Idle);
        assert_eq!(snapshot.pending_input, None);
    }

    #[tokio::test]
    async fn hook_route_unmapped_event_does_not_order() {
        let (hub, _scratch) = test_hub("hook-unmapped");
        hub.sessions.insert(idle_record("s1"));
        let unmapped = r#"{"hook_event_name":"Notification","notification_type":"something_else"}"#;
        let response = build_loopback_router(hub.clone())
            .oneshot(hook_request(
                "s1",
                Some("test-token"),
                Some("200"),
                unmapped,
            ))
            .await
            .expect("router answers");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let question = concat!(
            r#"{"hook_event_name":"PreToolUse","tool_name":"AskUserQuestion","#,
            r#""tool_input":{"questions":[]}}"#,
        );
        let response = build_loopback_router(hub.clone())
            .oneshot(hook_request(
                "s1",
                Some("test-token"),
                Some("100"),
                question,
            ))
            .await
            .expect("router answers");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(snapshot_of(&hub, "s1").status, SessionStatus::AwaitingInput);
    }

    #[tokio::test]
    async fn hook_route_drops_an_equal_time_event() {
        let (hub, _scratch) = test_hub("hook-equal-time");
        hub.sessions.insert(idle_record("s1"));
        let started = r#"{"hook_event_name":"UserPromptSubmit"}"#;
        let stop = r#"{"hook_event_name":"Stop"}"#;
        for body in [started, stop] {
            let response = build_loopback_router(hub.clone())
                .oneshot(hook_request("s1", Some("test-token"), Some("100"), body))
                .await
                .expect("router answers");
            assert_eq!(response.status(), StatusCode::NO_CONTENT);
        }
        assert_eq!(snapshot_of(&hub, "s1").status, SessionStatus::Working);
    }

    #[tokio::test]
    async fn hook_route_drops_an_event_without_a_usable_time() {
        let (hub, _scratch) = test_hub("hook-no-time");
        hub.sessions.insert(idle_record("s1"));
        let body = r#"{"hook_event_name":"UserPromptSubmit"}"#;
        for hook_time in [None, Some("abc")] {
            let response = build_loopback_router(hub.clone())
                .oneshot(hook_request("s1", Some("test-token"), hook_time, body))
                .await
                .expect("router answers");
            assert_eq!(response.status(), StatusCode::NO_CONTENT);
        }
        let snapshot = snapshot_of(&hub, "s1");
        assert_eq!(snapshot.status, SessionStatus::Idle);
        assert_eq!(snapshot.pending_input, None);
    }

    #[tokio::test]
    async fn hook_route_keeps_a_stopped_session_stopped() {
        let (hub, _scratch) = test_hub("hook-stopped");
        hub.sessions.insert(idle_record("s1"));
        hub.sessions
            .update("s1", |record| record.status = SessionStatus::Stopped);
        let body = r#"{"hook_event_name":"PostToolUse"}"#;
        let response = build_loopback_router(hub.clone())
            .oneshot(hook_request("s1", Some("test-token"), Some("100"), body))
            .await
            .expect("router answers");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let snapshot = snapshot_of(&hub, "s1");
        assert_eq!(snapshot.status, SessionStatus::Stopped);
        assert_eq!(snapshot.pending_input, None);
    }

    /// One `Attention` per entry into `AwaitingInput`, on the real hook path:
    /// the question dialog the screen then shows raises no second event, past
    /// the hook's hold and a whole confirmation window, and an event for a
    /// session already waiting raises none.
    #[tokio::test(start_paused = true)]
    async fn hook_awaiting_input_raises_one_attention() {
        let (hub, _scratch, mut attention) = test_hub_with_attention("hook-attention");
        hub.sessions.insert(idle_record("s1"));
        let (output, output_rx) = broadcast::channel(16);
        let screen: crate::screen::SharedScreen =
            std::sync::Arc::new(std::sync::Mutex::new(crate::screen::Screen::new(140, 40)));
        pty_state::watch(
            &hub.sessions,
            "s1".to_owned(),
            pty_state::AgentStreams {
                output: output_rx,
                screen,
                rules: crate::screen::rules::rules_for(&agent_registry::default_agent()),
            },
            hub.attention_tx.clone(),
        );
        let question = concat!(
            r#"{"hook_event_name":"PreToolUse","tool_name":"AskUserQuestion","#,
            r#""tool_input":{"questions":[{"question":"Which?","header":"Pick","#,
            r#""options":[{"label":"A"},{"label":"B"}],"multiSelect":false}]}}"#,
        );
        let event: serde_json::Value = serde_json::from_str(question).expect("parse hook body");
        let signal = report(&event);
        crate::hook_status::apply_signal(&hub, "s1", Some(100), &signal);
        assert!(
            matches!(
                attention.try_recv().map(|event| event.reason),
                Ok(AttentionReason::AwaitingInput)
            ),
            "the hook transition raises attention"
        );
        // The `AskUserQuestion` dialog of `screen/fixtures/c31-ask-q1.txt`.
        let dialog = [
            "\x1b[H\x1b[2J  Which function should be added to calc.py next?",
            "",
            "  ❯ 1. sub",
            "    2. div",
            "",
            "  Enter to select · Tab/Arrow keys to navigate · Esc to cancel",
        ]
        .join("\r\n");
        for _ in 0..2 {
            output
                .send(dialog.clone().into_bytes())
                .expect("watcher still subscribed");
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
        }
        crate::hook_status::apply_signal(&hub, "s1", Some(200), &signal);
        assert!(
            attention.try_recv().is_err(),
            "the dialog on screen and a repeat event raise no second event"
        );
    }

    /// A hook event that arrives while its session is still being spawned —
    /// the tracer has started the child, but the record is not inserted yet —
    /// is held and applied at insertion, so a `SessionStart` or the kickoff
    /// prompt's `UserPromptSubmit` is not dropped on the unknown-session
    /// branch.
    #[tokio::test]
    async fn hook_before_the_insert_is_applied_after_it() {
        let (hub, _scratch) = test_hub("hook-before-insert");
        let response = build_loopback_router(hub.clone())
            .oneshot(hook_request(
                "s1",
                Some("test-token"),
                Some("100"),
                r#"{"hook_event_name":"UserPromptSubmit"}"#,
            ))
            .await
            .expect("router answers");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(
            hub.sessions.get("s1").is_none(),
            "the spawn has not inserted its record yet"
        );
        let (pending, _awaiting) = hub
            .pending_hooks
            .insert_with_pending(&hub, idle_record("s1"));
        pending.publish();
        assert_eq!(
            snapshot_of(&hub, "s1").status,
            SessionStatus::Working,
            "the kickoff prompt's hook survived the insert"
        );
        assert!(hook_driven_of(&hub, "s1"), "hooks now drive the session");
    }

    /// A buffered prompt followed by an event that clears it must not raise a
    /// stale "Needs you": the announcement follows the record's final state,
    /// not the number of events that passed through `AwaitingInput`.
    #[tokio::test]
    async fn a_cleared_buffered_prompt_raises_no_attention() {
        let (hub, _scratch, mut attention) = test_hub_with_attention("hook-replay-cleared");
        for (time, body) in [
            (
                100,
                r#"{"hook_event_name":"PermissionRequest","tool_name":"Bash"}"#,
            ),
            (200, r#"{"hook_event_name":"Stop"}"#),
        ] {
            let time = time.to_string();
            let response = build_loopback_router(hub.clone())
                .oneshot(hook_request(
                    "s1",
                    Some("test-token"),
                    Some(time.as_str()),
                    body,
                ))
                .await
                .expect("router answers");
            assert_eq!(response.status(), StatusCode::NO_CONTENT);
        }
        let (pending, awaiting) = hub
            .pending_hooks
            .insert_with_pending(&hub, idle_record("s1"));
        pending.publish();
        announce_replayed_prompt(&hub, "s1", awaiting);
        let snapshot = snapshot_of(&hub, "s1");
        assert_eq!(
            snapshot.status,
            SessionStatus::Idle,
            "the Stop cleared the prompt"
        );
        assert_eq!(snapshot.pending_input, None, "no prompt is pending");
        assert!(
            attention.try_recv().is_err(),
            "a prompt cleared during the replay raises no attention"
        );
    }

    /// A buffered prompt still held after the whole replay raises exactly one
    /// attention, naming `AwaitingInput`.
    #[tokio::test]
    async fn a_buffered_prompt_still_held_raises_one_attention() {
        let (hub, _scratch, mut attention) = test_hub_with_attention("hook-replay-held");
        let response = build_loopback_router(hub.clone())
            .oneshot(hook_request(
                "s1",
                Some("test-token"),
                Some("100"),
                r#"{"hook_event_name":"PermissionRequest","tool_name":"Bash"}"#,
            ))
            .await
            .expect("router answers");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let (pending, awaiting) = hub
            .pending_hooks
            .insert_with_pending(&hub, idle_record("s1"));
        pending.publish();
        announce_replayed_prompt(&hub, "s1", awaiting);
        let snapshot = snapshot_of(&hub, "s1");
        assert_eq!(snapshot.status, SessionStatus::AwaitingInput);
        assert!(
            snapshot.pending_input.is_some(),
            "the prompt is still pending"
        );
        assert!(
            matches!(
                attention.try_recv().map(|event| event.reason),
                Ok(AttentionReason::AwaitingInput)
            ),
            "the held prompt raises one attention"
        );
        assert!(attention.try_recv().is_err(), "exactly one attention");
    }

    /// A `GET /proof` carrying one `X-RT-Challenge` header per entry of
    /// `challenges`.
    fn proof_request(challenges: &[&str]) -> axum::http::Request<Body> {
        let mut builder = axum::http::Request::builder().method("GET").uri("/proof");
        for challenge in challenges {
            builder = builder.header("x-rt-challenge", *challenge);
        }
        builder.body(Body::empty()).expect("build proof request")
    }

    #[tokio::test]
    async fn proof_answers_a_valid_challenge() {
        let (hub, _scratch) = test_hub("proof-valid");
        let challenge = [0x5au8; 32];
        let response = build_loopback_router(hub.clone())
            .oneshot(proof_request(&[&tracer_protocol::to_lower_hex(&challenge)]))
            .await
            .expect("router answers");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let proofs: Vec<_> = response.headers().get_all("x-rt-proof").iter().collect();
        assert_eq!(proofs.len(), 1, "exactly one proof header");
        let expected = tracer_protocol::to_lower_hex(&tracer_protocol::daemon_proof(
            &hub.proof_key,
            &challenge,
        ));
        assert_eq!(proofs[0].to_str().expect("ascii proof"), expected);
        assert_eq!(
            hub.proof_key,
            tracer_protocol::daemon_proof_key(&hub.auth_token),
            "the key is derived from the run's token"
        );
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .expect("read body");
        assert!(body.is_empty(), "no body: {body:?}");
    }

    /// The daemon's real `/proof` answer, as hyper writes it on the wire,
    /// passes the shim's own head check and verifies.
    #[tokio::test]
    async fn the_real_proof_response_passes_the_shims_check() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        // A hang guard, never a must-beat bound: a passing run answers in
        // milliseconds.
        const GUARD: std::time::Duration = std::time::Duration::from_secs(120);
        let (hub, _scratch) = test_hub("proof-wire");
        let proof_key = hub.proof_key;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the loopback router");
        let addr = listener.local_addr().expect("listener addr");
        let server = tokio::spawn(axum::serve(listener, build_loopback_router(hub)).into_future());

        let challenge = [0xc3u8; 32];
        let mut stream = tokio::net::TcpStream::connect(addr)
            .await
            .expect("connect to the router");
        let request = format!(
            "GET /proof HTTP/1.1\r\nHost: {addr}\r\nX-RT-Challenge: {}\r\n\r\n",
            tracer_protocol::to_lower_hex(&challenge)
        );
        stream
            .write_all(request.as_bytes())
            .await
            .expect("write the proof request");
        let mut head = Vec::new();
        let read_head = async {
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                let read = stream.read(&mut byte).await.expect("read the answer");
                assert_ne!(read, 0, "the router closed before a full head");
                head.push(byte[0]);
            }
        };
        tokio::time::timeout(GUARD, read_head)
            .await
            .expect("the router answered");
        server.abort();

        let head = String::from_utf8(head).expect("a text head");
        let head = head
            .strip_suffix("\r\n\r\n")
            .expect("the head's blank line");
        let proof = tracer_protocol::parse_daemon_proof_head(head);
        assert!(
            proof.is_some(),
            "the shim's check refused the real head: {head:?}"
        );
        let proof = proof.expect("checked above");
        assert!(
            tracer_protocol::verify_daemon_proof(&proof_key, &challenge, &proof),
            "the proof verifies against the hub's key"
        );
    }

    #[tokio::test]
    async fn proof_refuses_a_missing_or_malformed_challenge() {
        let (hub, _scratch) = test_hub("proof-malformed");
        let valid = tracer_protocol::to_lower_hex(&[0x11u8; 32]);
        let upper = valid.to_ascii_uppercase().replace('1', "A");
        let short = &valid[..63];
        let cases: [&[&str]; 4] = [&[], &[upper.as_str()], &[short], &[&valid, &valid]];
        for challenges in cases {
            let response = build_loopback_router(hub.clone())
                .oneshot(proof_request(challenges))
                .await
                .expect("router answers");
            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "challenges: {challenges:?}"
            );
            assert!(response.headers().get("x-rt-proof").is_none());
        }
    }

    #[tokio::test]
    async fn the_tailnet_router_has_no_proof_route() {
        use crate::server::remote::{build_tailnet_router, name_slot};
        let (hub, _scratch) = test_hub("proof-tailnet");
        let challenge = tracer_protocol::to_lower_hex(&[0x22u8; 32]);
        let response = build_tailnet_router(hub, name_slot(), 8787)
            .oneshot(proof_request(&[&challenge]))
            .await
            .expect("the router answers");
        assert!(
            matches!(
                response.status(),
                StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
            ),
            "status: {}",
            response.status()
        );
        assert!(response.headers().get("x-rt-proof").is_none());
    }

    /// Apply the hook `body` to `s1` at `event_time`, as the route does.
    fn apply_hook(hub: &Hub, body: &str, event_time: u128) {
        let event: serde_json::Value = serde_json::from_str(body).expect("parse hook body");
        crate::hook_status::apply_signal(hub, "s1", Some(event_time), &report(&event));
    }

    /// The report Claude's plugin maps `event` to, for a session with no
    /// spawn options.
    fn report(event: &serde_json::Value) -> agent_api::HookReport {
        agent_registry::plugin(&agent_registry::default_agent())
            .expect("Claude is registered")
            .map_hook_event(event, None, false)
            .expect("the body maps to a report")
    }

    /// Whether `id`'s record is driven by hook reports.
    fn hook_driven_of(hub: &Hub, id: &str) -> bool {
        let rec = hub.sessions.get(id).expect("session is in the registry");
        crate::sync::lock(&rec).hook_driven
    }

    /// The pending input `id`'s record holds.
    fn pending_input_of(hub: &Hub, id: &str) -> Option<protocol::PendingInput> {
        let rec = hub.sessions.get(id).expect("session is in the registry");
        crate::sync::lock(&rec).pending_input.clone()
    }

    /// A shell outlives the CLI whose hooks drive it: its `SessionEnd` hands
    /// the status back to the shell classifier, while an interactive session
    /// stays hook-driven.
    #[test]
    fn hook_status_clears_hook_driven_on_a_shell_session_end() {
        let (hub, _scratch) = test_hub("hook-session-end");
        let mut shell = idle_record("shell");
        shell.mode = SessionMode::PlainShell;
        hub.sessions.insert(shell);
        hub.sessions.insert(idle_record("agent"));
        let apply = |id: &str, body: &str, event_time: u128| {
            let event: serde_json::Value = serde_json::from_str(body).expect("parse hook body");
            crate::hook_status::apply_signal(&hub, id, Some(event_time), &report(&event));
        };

        for id in ["shell", "agent"] {
            apply(id, r#"{"hook_event_name":"UserPromptSubmit"}"#, 100);
            apply(
                id,
                r#"{"hook_event_name":"Elicitation","mcp_server_name":"MCP","message":"pick"}"#,
                150,
            );
            assert!(hook_driven_of(&hub, id), "{id} is driven by its hooks");
            assert!(
                pending_input_of(&hub, id).is_some(),
                "{id} waits on its prompt"
            );
        }

        let end = r#"{"hook_event_name":"SessionEnd","reason":"prompt_input_exit"}"#;
        apply("shell", end, 200);
        apply("agent", end, 200);
        assert!(
            !hook_driven_of(&hub, "shell"),
            "the shell falls back to its classifier once its CLI ends"
        );
        assert!(
            pending_input_of(&hub, "shell").is_none(),
            "the end clears the shell's pending prompt"
        );
        assert!(
            hook_driven_of(&hub, "agent"),
            "the agent session stays hook-driven"
        );
    }

    /// `id`'s conversation id, read under its record lock.
    fn conversation_id_of(hub: &Hub, id: &str) -> Option<String> {
        let rec = hub.sessions.get(id).expect("session is in the registry");
        crate::sync::lock(&rec).conversation_id.clone()
    }

    /// The last assistant message `id`'s record holds.
    fn last_message_of(hub: &Hub, id: &str) -> Option<String> {
        let rec = hub.sessions.get(id).expect("session is in the registry");
        crate::sync::lock(&rec).last_assistant_message.clone()
    }

    /// The newest recent action on `id`'s record.
    fn last_action_of(hub: &Hub, id: &str) -> Option<String> {
        let rec = hub.sessions.get(id).expect("session is in the registry");
        crate::sync::lock(&rec).recent_actions.last().cloned()
    }

    /// A `SessionStart` for a `/clear` carries a new conversation id, and the
    /// record moves to it.
    #[tokio::test]
    async fn hook_session_start_clear_updates_the_conversation_id() {
        let (hub, _scratch) = test_hub("hook-session-start-clear");
        hub.sessions.insert(idle_record("s1"));
        hub.sessions.update("s1", |rec| {
            rec.conversation_id = Some("old-conv".to_owned());
        });
        apply_hook(
            &hub,
            r#"{"hook_event_name":"SessionStart","source":"clear","session_id":"new-conv"}"#,
            100,
        );
        assert_eq!(
            conversation_id_of(&hub, "s1").as_deref(),
            Some("new-conv"),
            "a clear moves the session to its new conversation"
        );
    }

    /// A resumed session reports the id it already had, so the write is a
    /// no-op.
    #[tokio::test]
    async fn hook_session_start_resume_with_the_same_id_leaves_it() {
        let (hub, _scratch) = test_hub("hook-session-start-resume-same");
        hub.sessions.insert(idle_record("s1"));
        hub.sessions.update("s1", |rec| {
            rec.conversation_id = Some("same-conv".to_owned());
        });
        apply_hook(
            &hub,
            r#"{"hook_event_name":"SessionStart","source":"resume","session_id":"same-conv"}"#,
            100,
        );
        assert_eq!(
            conversation_id_of(&hub, "s1").as_deref(),
            Some("same-conv"),
            "a resume keeps the conversation it resumed"
        );
    }

    /// There is no source gate: a `resume` that reports a different id moves
    /// the record like any other `SessionStart`.
    #[tokio::test]
    async fn hook_session_start_resume_with_a_new_id_updates_it() {
        let (hub, _scratch) = test_hub("hook-session-start-resume-new");
        hub.sessions.insert(idle_record("s1"));
        hub.sessions.update("s1", |rec| {
            rec.conversation_id = Some("old-conv".to_owned());
        });
        apply_hook(
            &hub,
            r#"{"hook_event_name":"SessionStart","source":"resume","session_id":"new-conv"}"#,
            100,
        );
        assert_eq!(
            conversation_id_of(&hub, "s1").as_deref(),
            Some("new-conv"),
            "the source does not gate the id write"
        );
    }

    /// `Stop` keeps the message the turn ended with; a later `Stop` with no
    /// message keeps it.
    #[tokio::test]
    async fn hook_stop_stores_the_last_message() {
        let (hub, _scratch) = test_hub("hook-stop-message");
        hub.sessions.insert(idle_record("s1"));
        apply_hook(
            &hub,
            r#"{"hook_event_name":"Stop","last_assistant_message":"All done."}"#,
            100,
        );
        assert_eq!(
            last_message_of(&hub, "s1").as_deref(),
            Some("All done."),
            "a stop stores the message it ended with"
        );
        apply_hook(&hub, r#"{"hook_event_name":"Stop"}"#, 200);
        assert_eq!(
            last_message_of(&hub, "s1").as_deref(),
            Some("All done."),
            "an absent message keeps the previous one"
        );
        apply_hook(
            &hub,
            r#"{"hook_event_name":"Stop","last_assistant_message":""}"#,
            300,
        );
        assert_eq!(
            last_message_of(&hub, "s1").as_deref(),
            Some("All done."),
            "an empty message keeps the previous one too"
        );
    }

    /// `StopFailure` leaves a recent-actions line, naming the reason when the
    /// payload carries one.
    #[tokio::test]
    async fn hook_stop_failure_adds_a_recent_action() {
        let (hub, _scratch) = test_hub("hook-stop-failure");
        hub.sessions.insert(idle_record("s1"));
        apply_hook(
            &hub,
            r#"{"hook_event_name":"StopFailure","reason":"overloaded"}"#,
            100,
        );
        assert_eq!(
            last_action_of(&hub, "s1").as_deref(),
            Some("stop failed: overloaded"),
            "the newest action names the failure"
        );
        apply_hook(&hub, r#"{"hook_event_name":"StopFailure"}"#, 200);
        assert_eq!(
            last_action_of(&hub, "s1").as_deref(),
            Some("stop failed"),
            "a failure with no reason leaves the bare line"
        );
    }

    /// A `/clear`'s new id reaches the sidecar through the registry's mirror,
    /// so a reattach or daemon restart keeps it.
    #[tokio::test]
    async fn hook_session_start_clear_reaches_the_sidecar() {
        use crate::history::test_support::write_meta_for;
        let (hub, _scratch) = test_hub("hook-sidecar-id");
        write_meta_for(&hub.dirs, "s1");
        hub.sessions.insert(idle_record("s1"));
        // Bring the sidecar in line with the record first, so the id is the
        // only mirrored field that differs when the hook lands.
        hub.sessions.update("s1", |_| {});
        apply_hook(
            &hub,
            r#"{"hook_event_name":"SessionStart","source":"clear","session_id":"new-conv"}"#,
            100,
        );
        let meta = crate::orphan::load_meta(&hub.dirs, "s1").expect("load meta");
        assert_eq!(
            meta.conversation_id.as_deref(),
            Some("new-conv"),
            "the hook's id is mirrored into the sidecar"
        );
    }

    /// A terminal session takes no detail write: a late `/clear` event
    /// changes neither its id nor its last message.
    #[tokio::test]
    async fn hook_session_start_clear_for_a_terminal_session_changes_nothing() {
        let (hub, _scratch) = test_hub("hook-terminal-guard");
        hub.sessions.insert(idle_record("s1"));
        hub.sessions.update("s1", |rec| {
            rec.status = SessionStatus::Stopped;
            rec.conversation_id = Some("old-conv".to_owned());
            rec.last_assistant_message = Some("kept".to_owned());
        });
        apply_hook(
            &hub,
            r#"{"hook_event_name":"SessionStart","source":"clear","session_id":"new-conv"}"#,
            100,
        );
        assert_eq!(
            conversation_id_of(&hub, "s1").as_deref(),
            Some("old-conv"),
            "a terminal session keeps its id"
        );
        assert_eq!(
            last_message_of(&hub, "s1").as_deref(),
            Some("kept"),
            "a terminal session keeps its message"
        );
    }

    /// An event at or before the last applied time is dropped whole: it
    /// changes neither the id nor the message.
    #[tokio::test]
    async fn hook_a_stale_event_changes_nothing() {
        let (hub, _scratch) = test_hub("hook-stale-event");
        hub.sessions.insert(idle_record("s1"));
        hub.sessions.update("s1", |rec| {
            rec.last_hook_time = Some(100);
            rec.conversation_id = Some("old-conv".to_owned());
            rec.last_assistant_message = Some("kept".to_owned());
        });
        apply_hook(
            &hub,
            r#"{"hook_event_name":"SessionStart","source":"clear","session_id":"new-conv"}"#,
            100,
        );
        apply_hook(
            &hub,
            r#"{"hook_event_name":"Stop","last_assistant_message":"stale"}"#,
            99,
        );
        assert_eq!(
            conversation_id_of(&hub, "s1").as_deref(),
            Some("old-conv"),
            "a stale start changes no id"
        );
        assert_eq!(
            last_message_of(&hub, "s1").as_deref(),
            Some("kept"),
            "a stale stop changes no message"
        );
    }

    /// The id later becomes `claude --resume <id>` and a transcript path, so
    /// a value that is not plain is refused.
    #[tokio::test]
    async fn hook_session_start_refuses_a_non_plain_id() {
        let (hub, _scratch) = test_hub("hook-plain-id");
        hub.sessions.insert(idle_record("s1"));
        hub.sessions.update("s1", |rec| {
            rec.conversation_id = Some("old-conv".to_owned());
        });
        for (index, id) in ["../x", r"C:\conversations\x", "a/b"].iter().enumerate() {
            let body = serde_json::json!({
                "hook_event_name": "SessionStart",
                "source": "clear",
                "session_id": id,
            })
            .to_string();
            apply_hook(&hub, &body, 100 + index as u128);
            assert_eq!(
                conversation_id_of(&hub, "s1").as_deref(),
                Some("old-conv"),
                "{id} is refused"
            );
        }
    }

    /// A session with no id at all may gain one from a `/clear`.
    #[tokio::test]
    async fn hook_session_start_clear_sets_a_missing_id() {
        let (hub, _scratch) = test_hub("hook-missing-id");
        hub.sessions.insert(idle_record("s1"));
        apply_hook(
            &hub,
            r#"{"hook_event_name":"SessionStart","source":"clear","session_id":"fresh-conv"}"#,
            100,
        );
        assert_eq!(
            conversation_id_of(&hub, "s1").as_deref(),
            Some("fresh-conv"),
            "a missing id is filled in"
        );
    }

    /// The message is capped in bytes, so a multi-byte message over 8 KiB
    /// cannot grow past it and is cut with the ellipsis.
    #[tokio::test]
    async fn hook_stop_caps_the_message_at_eight_kib() {
        let (hub, _scratch) = test_hub("hook-stop-cap");
        hub.sessions.insert(idle_record("s1"));
        let body = serde_json::json!({
            "hook_event_name": "Stop",
            "last_assistant_message": "é".repeat(5000),
        })
        .to_string();
        apply_hook(&hub, &body, 100);
        let stored = last_message_of(&hub, "s1").expect("a message is stored");
        assert!(stored.len() <= 8 * 1024, "{} bytes", stored.len());
        assert!(stored.ends_with('…'), "the cut ends in the ellipsis");
    }

    /// The id the hook wrote is the one an ended session's history entry
    /// carries.
    #[tokio::test]
    async fn hook_session_start_clear_reaches_the_history_entry() {
        let (hub, _scratch) = test_hub("hook-history-id");
        hub.sessions.insert(idle_record("s1"));
        hub.sessions.update("s1", |rec| {
            rec.conversation_id = Some("old-conv".to_owned());
        });
        apply_hook(
            &hub,
            r#"{"hook_event_name":"SessionStart","source":"clear","session_id":"new-conv"}"#,
            100,
        );
        let rec = hub.sessions.get("s1").expect("session is in the registry");
        let guard = crate::sync::lock(&rec);
        let entry = crate::history::entry_from_record(
            &guard,
            protocol::SessionEnd::TracerLost,
            chrono::Utc::now(),
        );
        assert_eq!(entry.conversation_id.as_deref(), Some("new-conv"));
    }
}
