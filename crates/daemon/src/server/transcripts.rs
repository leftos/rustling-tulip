//! Following a transcript read-only (`FollowTranscript`,
//! `UnfollowTranscript`): each source goes to the service that tails it, a
//! dispatch run's to the dispatch-run list and a subagent's to the subagent
//! tails. A tailnet client may follow one too.

use protocol::{ClientMessage, DaemonMessage, TranscriptSource, error_codes};
use tracing::warn;

use super::Hub;
use super::connection::ConnCtx;
use super::dispatch_runs::not_followed;

/// Handle the transcript messages `dispatch` routes here. A follow answers
/// on the requester's queue before this returns; nothing is spawned.
pub(super) fn handle(hub: &Hub, msg: ClientMessage, ctx: &ConnCtx<'_>) {
    match msg {
        ClientMessage::FollowTranscript { source, request_id } => {
            follow(hub, source, request_id, ctx);
        }
        ClientMessage::UnfollowTranscript { source } => unfollow(hub, source, ctx),
        other => warn!(
            ?other,
            "transcripts::handle: routed a message it does not own"
        ),
    }
}

/// Make the connection follow `source`, answering `request_id` with the
/// backfill or an `Error`.
fn follow(hub: &Hub, source: TranscriptSource, request_id: Option<String>, ctx: &ConnCtx<'_>) {
    match source {
        TranscriptSource::DispatchRun { .. } => {
            let Some(dispatch) = &hub.dispatch_runs else {
                let _ = ctx.out_tx.send(not_followed(request_id));
                return;
            };
            dispatch
                .runs
                .follow_transcript(&source, request_id, ctx.connection, ctx.out_tx);
        }
        TranscriptSource::Subagent {
            session_id,
            agent_id,
        } => {
            hub.subagent_tails.follow(
                &(session_id, agent_id),
                request_id,
                ctx.connection,
                ctx.out_tx,
            );
        }
        TranscriptSource::Unknown | TranscriptSource::RunSubagent { .. } => {
            let _ = ctx.out_tx.send(error(
                error_codes::UNSUPPORTED_MESSAGE,
                "This daemon cannot follow that transcript source.".to_owned(),
                request_id,
            ));
        }
    }
}

/// Stop the connection following `source`; nothing when it does not.
fn unfollow(hub: &Hub, source: TranscriptSource, ctx: &ConnCtx<'_>) {
    match source {
        TranscriptSource::DispatchRun { .. } => {
            if let Some(dispatch) = &hub.dispatch_runs {
                dispatch.runs.unfollow_transcript(&source, ctx.connection);
            }
        }
        TranscriptSource::Subagent {
            session_id,
            agent_id,
        } => hub
            .subagent_tails
            .unfollow(&(session_id, agent_id), ctx.connection),
        TranscriptSource::Unknown | TranscriptSource::RunSubagent { .. } => {}
    }
}

/// An `Error` with `code`, answering `request_id`.
fn error(code: &str, message: String, request_id: Option<String>) -> DaemonMessage {
    DaemonMessage::Error {
        message,
        request_id,
        code: Some(code.to_owned()),
    }
}

#[cfg(test)]
#[expect(clippy::panic, reason = "tests fail on an unexpected message")]
mod tests {
    use super::super::test_support::{dispatch_on, enable_dispatch_runs, test_hub};
    use crate::subagents::tail::tests::{Fixture, SESSION, next};
    use protocol::{ClientMessage, DaemonMessage, TranscriptSource, error_codes};
    use tokio::sync::mpsc;

    /// Follow `source` over connection 1, returning the first message it is
    /// sent.
    async fn follow_reply(hub: &super::Hub, source: TranscriptSource) -> DaemonMessage {
        let (out_tx, mut inbox) = mpsc::unbounded_channel();
        let message = ClientMessage::FollowTranscript {
            source,
            request_id: Some("r1".to_owned()),
        };
        dispatch_on(hub, message, 1, &out_tx).await;
        next(&mut inbox).await
    }

    fn refusal(message: DaemonMessage) -> (Option<String>, String, Option<String>) {
        match message {
            DaemonMessage::Error {
                message,
                request_id,
                code,
            } => (code, message, request_id),
            other => panic!("expected an Error, got {other:?}"),
        }
    }

    fn subagent(session_id: &str, agent_id: &str) -> TranscriptSource {
        TranscriptSource::Subagent {
            session_id: session_id.to_owned(),
            agent_id: agent_id.to_owned(),
        }
    }

    #[tokio::test]
    async fn an_unknown_session_answers_not_found() {
        let fixture = Fixture::new("transcripts-unknown-session");
        let reply = follow_reply(&fixture.hub, subagent("nope", "a1b2")).await;
        assert_eq!(
            refusal(reply),
            (
                Some(error_codes::NOT_FOUND.to_owned()),
                "unknown subagent: nope/a1b2".to_owned(),
                Some("r1".to_owned())
            )
        );
        assert_eq!(fixture.hub.subagent_tails.tail_count(), 0);
    }

    #[tokio::test]
    async fn a_refused_agent_id_answers_not_found() {
        let fixture = Fixture::new("transcripts-refused-agent");
        let reply = follow_reply(&fixture.hub, subagent(SESSION, "../x")).await;
        assert_eq!(
            refusal(reply),
            (
                Some(error_codes::NOT_FOUND.to_owned()),
                format!("unknown subagent: {SESSION}/../x"),
                Some("r1".to_owned())
            )
        );
        assert_eq!(fixture.hub.subagent_tails.tail_count(), 0);
    }

    #[tokio::test]
    async fn a_dispatch_run_follow_still_works() {
        let (mut hub, scratch) = test_hub("transcripts-dispatch-run");
        let runs = enable_dispatch_runs(&mut hub, scratch.path().join("live"));
        runs.list_for_test("run-1", 4242, None, chrono::Utc::now());
        let source = TranscriptSource::DispatchRun {
            run_id: "run-1".to_owned(),
        };
        match follow_reply(&hub, source.clone()).await {
            DaemonMessage::Transcript {
                source: got,
                request_id,
                ..
            } => {
                assert_eq!(got, source);
                assert_eq!(request_id.as_deref(), Some("r1"));
            }
            other => panic!("expected the run's backfill, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_unknown_source_answers_unsupported() {
        let (hub, _scratch) = test_hub("transcripts-unknown-source");
        let reply = follow_reply(&hub, TranscriptSource::Unknown).await;
        let (code, _, request_id) = refusal(reply);
        assert_eq!(code.as_deref(), Some(error_codes::UNSUPPORTED_MESSAGE));
        assert_eq!(request_id.as_deref(), Some("r1"));
    }
}
