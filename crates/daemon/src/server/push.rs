//! The phone's Web Push requests. Until the subscription store lands they are
//! refused.
use super::Hub;
use super::connection::{AuthKind, ConnCtx};
use protocol::{ClientMessage, DaemonMessage, error_codes};
use tracing::warn;

/// Answer a push request with an `Error` carrying its `request_id`: a
/// loopback connection is not a phone (`invalid_request`), and a tailnet one
/// hears push is not served yet (`unsupported_message`).
pub(super) fn handle(_hub: &Hub, msg: &ClientMessage, ctx: &ConnCtx<'_>) {
    let request_id = match msg {
        ClientMessage::GetPushStatus { request_id }
        | ClientMessage::RegisterPush { request_id, .. }
        | ClientMessage::UnregisterPush { request_id } => request_id.clone(),
        _ => {
            warn!("push: a non-push message reached the push handler; dropped");
            return;
        }
    };
    let (code, message) = if matches!(ctx.auth, AuthKind::Local) {
        (
            error_codes::INVALID_REQUEST,
            "push is for paired phones on the tailnet",
        )
    } else {
        (
            error_codes::UNSUPPORTED_MESSAGE,
            "push is not available on this daemon yet",
        )
    };
    let _ = ctx.out_tx.send(DaemonMessage::Error {
        message: message.to_owned(),
        request_id: Some(request_id),
        code: Some(code.to_owned()),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::test_support::{dispatch_one_as, test_hub, test_peer};

    fn get_push_status() -> ClientMessage {
        ClientMessage::GetPushStatus {
            request_id: "p1".to_owned(),
        }
    }

    #[tokio::test]
    async fn a_loopback_push_request_is_an_invalid_request() {
        let (hub, _scratch) = test_hub("push-loopback");
        let reply = dispatch_one_as(&hub, get_push_status(), &AuthKind::Local).await;
        assert!(
            matches!(&reply, DaemonMessage::Error { code: Some(code), request_id: Some(id), .. }
                if code == error_codes::INVALID_REQUEST && id == "p1"),
            "{reply:?}"
        );
    }

    #[tokio::test]
    async fn a_tailnet_push_request_is_not_supported_yet() {
        let (hub, _scratch) = test_hub("push-tailnet");
        let reply = dispatch_one_as(&hub, get_push_status(), &AuthKind::Tailnet(test_peer())).await;
        assert!(
            matches!(&reply, DaemonMessage::Error { code: Some(code), request_id: Some(id), .. }
                if code == error_codes::UNSUPPORTED_MESSAGE && id == "p1"),
            "{reply:?}"
        );
    }

    #[tokio::test]
    async fn a_tailnet_register_push_is_not_supported_yet() {
        let (hub, _scratch) = test_hub("push-tailnet-register");
        let msg = ClientMessage::RegisterPush {
            registration: protocol::PushRegistration::WebPush {
                endpoint: protocol::Secret("https://push.example.com/send/abc".to_owned()),
                p256dh: "BPubKey".to_owned(),
                auth: protocol::Secret("auth".to_owned()),
            },
            needs_you: true,
            finished: false,
            request_id: "p2".to_owned(),
        };
        let reply = dispatch_one_as(&hub, msg, &AuthKind::Tailnet(test_peer())).await;
        assert!(
            matches!(&reply, DaemonMessage::Error { code: Some(code), request_id: Some(id), .. }
                if code == error_codes::UNSUPPORTED_MESSAGE && id == "p2"),
            "{reply:?}"
        );
    }

    #[tokio::test]
    async fn a_tailnet_unregister_push_is_not_supported_yet() {
        let (hub, _scratch) = test_hub("push-tailnet-unregister");
        let msg = ClientMessage::UnregisterPush {
            request_id: "p3".to_owned(),
        };
        let reply = dispatch_one_as(&hub, msg, &AuthKind::Tailnet(test_peer())).await;
        assert!(
            matches!(&reply, DaemonMessage::Error { code: Some(code), request_id: Some(id), .. }
                if code == error_codes::UNSUPPORTED_MESSAGE && id == "p3"),
            "{reply:?}"
        );
    }
}
