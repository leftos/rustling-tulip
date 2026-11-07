//! The phone's Web Push requests: its push status, and its one subscription
//! registered, replaced or removed, keyed by the device record its tailnet
//! stable id resolves to (`docs/plans/mobile-app.md`, "Push").
use super::Hub;
use super::connection::{AuthKind, ConnCtx};
use crate::push::state::Loaded;
use crate::push::store::Subscription;
use protocol::{ClientMessage, DaemonMessage, PushRegistration, Secret, error_codes};
use tracing::{info, warn};

/// Why a push request was refused: the `Error` code and its message, which
/// never carries an endpoint or a key.
struct Refusal {
    code: &'static str,
    message: String,
}

impl Refusal {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            code: error_codes::INVALID_REQUEST,
            message: message.into(),
        }
    }

    /// A folder, key or store failure: the reply carries a fixed message and
    /// the error chain, which names the host's files, goes to the log only.
    fn internal(err: &anyhow::Error) -> Self {
        warn!(err = format!("{err:#}"), "push: request failed");
        Self {
            code: error_codes::INTERNAL,
            message: "push storage failed on the host".to_owned(),
        }
    }
}

/// Answer a push request with `PushStatus`, or an `Error`, carrying its
/// `request_id`. A loopback connection is not a phone and a tailnet peer
/// without an unrevoked device record is not paired (`invalid_request`); a
/// push folder, key or store failure is `internal`.
pub(super) fn handle(hub: &Hub, msg: &ClientMessage, ctx: &ConnCtx<'_>) {
    let request_id = match msg {
        ClientMessage::GetPushStatus { request_id }
        | ClientMessage::RegisterPush { request_id, .. }
        | ClientMessage::UnregisterPush { request_id } => request_id.clone(),
        _ => {
            warn!("push: a non-push message reached the push handler; dropped");
            return;
        }
    };
    let reply = match serve(hub, msg, ctx.auth, &request_id) {
        Ok(status) => status,
        Err(refusal) => DaemonMessage::Error {
            message: refusal.message,
            request_id: Some(request_id),
            code: Some(refusal.code.to_owned()),
        },
    };
    let _ = ctx.out_tx.send(reply);
}

/// Serve one push request. `devices` stays locked until the push work is
/// done, so a block, forget or re-pair cannot land between the lookup and a
/// write; `push` is taken after it (`server/pairing.rs` has the lock order).
fn serve(
    hub: &Hub,
    msg: &ClientMessage,
    auth: &AuthKind,
    request_id: &str,
) -> Result<DaemonMessage, Refusal> {
    let AuthKind::Tailnet(peer) = auth else {
        return Err(Refusal::invalid("push is for paired phones on the tailnet"));
    };
    let devices = crate::sync::lock(&hub.devices);
    let Some(device_id) = devices.unrevoked_id_of(&peer.stable_id) else {
        return Err(Refusal::invalid("this device is not paired"));
    };
    let mut push = crate::sync::lock(&hub.push);
    let policy = push.policy();
    let new_subscription = match msg {
        ClientMessage::RegisterPush {
            registration,
            needs_you,
            finished,
            ..
        } => Some(subscription(registration, *needs_you, *finished, policy)?),
        _ => None,
    };
    let loaded = push.load(&devices).map_err(|err| Refusal::internal(&err))?;
    match (msg, new_subscription) {
        (ClientMessage::RegisterPush { .. }, Some(mut new)) => {
            // The same endpoint is the same subscription with new switches:
            // it keeps when it was first stored.
            if let Some(stored) = loaded.subs.get(device_id)
                && stored.endpoint == new.endpoint
            {
                new.created_at.clone_from(&stored.created_at);
            }
            loaded
                .subs
                .put(device_id, new)
                .map_err(|err| Refusal::internal(&err))?;
            info!(%device_id, "push: the device's subscription was stored");
        }
        (ClientMessage::UnregisterPush { .. }, _) => {
            let removed = loaded
                .subs
                .remove(device_id)
                .map_err(|err| Refusal::internal(&err))?;
            if removed {
                info!(%device_id, "push: the device's subscription was removed");
            }
        }
        _ => {}
    }
    Ok(push_status(loaded, device_id, request_id))
}

/// The subscription a `RegisterPush` asks for, checked: the endpoint is
/// `https` to a known push service and the keys decode.
fn subscription(
    registration: &PushRegistration,
    needs_you: bool,
    finished: bool,
    policy: crate::push::send::PushPolicy,
) -> Result<Subscription, Refusal> {
    let PushRegistration::WebPush {
        endpoint,
        p256dh,
        auth,
    } = registration
    else {
        return Err(Refusal::invalid(
            "this daemon does not know that kind of push registration",
        ));
    };
    // The top-level messages only: they name the host or the key at fault,
    // never the endpoint or the secret itself.
    crate::push::send::check_endpoint(&endpoint.0, policy)
        .map_err(|err| Refusal::invalid(err.to_string()))?;
    crate::push::webpush::Subscriber::from_b64url(p256dh, &auth.0)
        .map_err(|err| Refusal::invalid(err.to_string()))?;
    Ok(Subscription {
        kind: "web_push".to_owned(),
        endpoint: endpoint.0.clone(),
        p256dh: p256dh.clone(),
        auth: auth.0.clone(),
        needs_you,
        finished,
        created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    })
}

/// `device_id`'s push status: the daemon's key and the device's
/// subscription, or no endpoint and both switches off without one.
fn push_status(loaded: &Loaded, device_id: &str, request_id: &str) -> DaemonMessage {
    let stored = loaded.subs.get(device_id);
    DaemonMessage::PushStatus {
        vapid_public_key: loaded.key.public_b64url(),
        endpoint: stored.map(|sub| Secret(sub.endpoint.clone())),
        needs_you: stored.is_some_and(|sub| sub.needs_you),
        finished: stored.is_some_and(|sub| sub.finished),
        request_id: Some(request_id.to_owned()),
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert preconditions with expect and panic"
)]
mod tests {
    use super::*;
    use crate::push::store::{SUBSCRIPTIONS_FILE, Subscription, SubscriptionStore};
    use crate::server::test_support::{dispatch_one, dispatch_one_as, test_hub, test_peer};
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use p256::elliptic_curve::Generate as _;
    use p256::elliptic_curve::sec1::ToSec1Point as _;

    const APPLE: &str = "https://web.push.apple.com/send/abc";

    fn get_push_status() -> ClientMessage {
        ClientMessage::GetPushStatus {
            request_id: "p1".to_owned(),
        }
    }

    fn tailnet() -> AuthKind {
        AuthKind::Tailnet(test_peer())
    }

    /// Pair the test peer straight into the store; returns its device id.
    fn pair(hub: &Hub) -> String {
        crate::sync::lock(&hub.devices)
            .issue(&test_peer(), chrono::Utc::now())
            .expect("issue a device token")
            .0
    }

    /// A fresh subscriber's `p256dh` and `auth`, base64url.
    fn keys() -> (String, String) {
        let secret = p256::SecretKey::try_generate().expect("a P-256 key");
        let p256dh = URL_SAFE_NO_PAD.encode(secret.public_key().to_sec1_point(false).as_bytes());
        let auth = <[u8; 16]>::try_generate().expect("16 random bytes");
        (p256dh, URL_SAFE_NO_PAD.encode(auth))
    }

    fn register_with(endpoint: &str, p256dh: String, auth: String) -> ClientMessage {
        ClientMessage::RegisterPush {
            registration: protocol::PushRegistration::WebPush {
                endpoint: protocol::Secret(endpoint.to_owned()),
                p256dh,
                auth: protocol::Secret(auth),
            },
            needs_you: true,
            finished: false,
            request_id: "p2".to_owned(),
        }
    }

    fn register(endpoint: &str) -> ClientMessage {
        let (p256dh, auth) = keys();
        register_with(endpoint, p256dh, auth)
    }

    /// The reply's `PushStatus` fields, failing on anything else.
    fn status(reply: &DaemonMessage, want_id: &str) -> (String, Option<String>, bool, bool) {
        match reply {
            DaemonMessage::PushStatus {
                vapid_public_key,
                endpoint,
                needs_you,
                finished,
                request_id,
            } => {
                assert_eq!(request_id.as_deref(), Some(want_id), "{reply:?}");
                (
                    vapid_public_key.clone(),
                    endpoint.as_ref().map(|e| e.0.clone()),
                    *needs_you,
                    *finished,
                )
            }
            other => panic!("want PushStatus, got {other:?}"),
        }
    }

    /// The reply's error message, failing unless it is an `Error` with
    /// `code` and `want_id`.
    fn refused(reply: &DaemonMessage, code: &str, want_id: &str) -> String {
        match reply {
            DaemonMessage::Error {
                message,
                request_id,
                code: Some(got),
            } if got == code && request_id.as_deref() == Some(want_id) => message.clone(),
            other => panic!("want Error {code}, got {other:?}"),
        }
    }

    fn push_dir(scratch: &crate::server::test_support::ScratchDir) -> std::path::PathBuf {
        scratch.path().join("push")
    }

    fn stored_ids(scratch: &crate::server::test_support::ScratchDir) -> Vec<String> {
        SubscriptionStore::load(&push_dir(scratch).join(SUBSCRIPTIONS_FILE))
            .iter()
            .map(|(id, _)| id.to_owned())
            .collect()
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
    async fn a_paired_peer_gets_the_key_made_once_and_no_subscription() {
        let (hub, scratch) = test_hub("push-status");
        pair(&hub);

        let first = status(
            &dispatch_one_as(&hub, get_push_status(), &tailnet()).await,
            "p1",
        );
        assert!(!first.0.is_empty(), "the key is sent");
        assert_eq!((first.1.as_deref(), first.2, first.3), (None, false, false));
        let second = status(
            &dispatch_one_as(&hub, get_push_status(), &tailnet()).await,
            "p1",
        );
        assert_eq!(second.0, first.0, "the key is made once");
        assert!(push_dir(&scratch).join("vapid.json").is_file());
    }

    #[tokio::test]
    async fn an_unpaired_tailnet_peer_is_an_invalid_request() {
        let (hub, _scratch) = test_hub("push-unpaired");
        let reply = dispatch_one_as(&hub, get_push_status(), &tailnet()).await;
        refused(&reply, error_codes::INVALID_REQUEST, "p1");
    }

    #[tokio::test]
    async fn a_revoked_peer_is_an_invalid_request() {
        let (hub, _scratch) = test_hub("push-revoked");
        let device_id = pair(&hub);
        crate::sync::lock(&hub.devices)
            .revoke(&device_id)
            .expect("revoke");
        let reply = dispatch_one_as(&hub, get_push_status(), &tailnet()).await;
        refused(&reply, error_codes::INVALID_REQUEST, "p1");
    }

    #[tokio::test]
    async fn register_echoes_the_subscription_and_a_second_one_replaces_it() {
        let (hub, scratch) = test_hub("push-register");
        let device_id = pair(&hub);

        let first = status(
            &dispatch_one_as(&hub, register(APPLE), &tailnet()).await,
            "p2",
        );
        assert_eq!(
            (first.1.as_deref(), first.2, first.3),
            (Some(APPLE), true, false)
        );
        let other = "https://fcm.googleapis.com/fcm/send/xyz";
        let second = status(
            &dispatch_one_as(&hub, register(other), &tailnet()).await,
            "p2",
        );
        assert_eq!(second.1.as_deref(), Some(other), "the endpoint is replaced");
        assert_eq!(
            stored_ids(&scratch),
            vec![device_id],
            "one entry per device"
        );
    }

    #[tokio::test]
    async fn unregister_removes_the_subscription() {
        let (hub, scratch) = test_hub("push-unregister");
        pair(&hub);
        status(
            &dispatch_one_as(&hub, register(APPLE), &tailnet()).await,
            "p2",
        );

        let msg = ClientMessage::UnregisterPush {
            request_id: "p3".to_owned(),
        };
        let after = status(&dispatch_one_as(&hub, msg, &tailnet()).await, "p3");
        assert_eq!((after.1, after.2, after.3), (None, false, false));
        assert!(stored_ids(&scratch).is_empty(), "the removal is saved");
    }

    #[tokio::test]
    async fn an_unknown_registration_is_an_invalid_request() {
        let (hub, _scratch) = test_hub("push-unknown");
        pair(&hub);
        let msg = ClientMessage::RegisterPush {
            registration: protocol::PushRegistration::Unknown,
            needs_you: true,
            finished: true,
            request_id: "p2".to_owned(),
        };
        refused(
            &dispatch_one_as(&hub, msg, &tailnet()).await,
            error_codes::INVALID_REQUEST,
            "p2",
        );
    }

    #[tokio::test]
    async fn an_endpoint_off_the_allowlist_is_refused_without_echoing_it() {
        let (hub, scratch) = test_hub("push-bad-endpoint");
        pair(&hub);
        for endpoint in [
            "http://web.push.apple.com/send/secret-path",
            "https://push.example.com/send/secret-path",
        ] {
            let message = refused(
                &dispatch_one_as(&hub, register(endpoint), &tailnet()).await,
                error_codes::INVALID_REQUEST,
                "p2",
            );
            assert!(!message.contains("secret-path"), "{message}");
        }
        assert!(stored_ids(&scratch).is_empty(), "nothing is stored");
    }

    #[tokio::test]
    async fn a_bad_p256dh_is_refused_without_echoing_auth() {
        let (hub, _scratch) = test_hub("push-bad-key");
        pair(&hub);
        let (_, auth) = keys();
        let message = refused(
            &dispatch_one_as(
                &hub,
                register_with(APPLE, "not-a-key".to_owned(), auth.clone()),
                &tailnet(),
            )
            .await,
            error_codes::INVALID_REQUEST,
            "p2",
        );
        assert!(!message.contains(&auth), "{message}");
        assert!(!message.contains("send/abc"), "{message}");
    }

    #[tokio::test]
    async fn blocking_the_device_deletes_its_subscription() {
        let (hub, scratch) = test_hub("push-block");
        let device_id = pair(&hub);
        status(
            &dispatch_one_as(&hub, register(APPLE), &tailnet()).await,
            "p2",
        );

        crate::server::pairing::block_device(&hub, &device_id).expect("block");
        assert!(stored_ids(&scratch).is_empty(), "the subscription is gone");
    }

    #[tokio::test]
    async fn forgetting_the_device_deletes_its_subscription() {
        let (hub, scratch) = test_hub("push-forget");
        let device_id = pair(&hub);
        status(
            &dispatch_one_as(&hub, register(APPLE), &tailnet()).await,
            "p2",
        );

        let reply = dispatch_one(&hub, ClientMessage::ForgetDevice { device_id }).await;
        assert!(matches!(reply, DaemonMessage::Devices { .. }), "{reply:?}");
        assert!(stored_ids(&scratch).is_empty(), "the subscription is gone");
    }

    #[tokio::test]
    async fn a_corrupt_subscriptions_file_is_internal_and_left_as_it_is() {
        let (hub, scratch) = test_hub("push-corrupt");
        pair(&hub);
        let file = push_dir(&scratch).join(SUBSCRIPTIONS_FILE);
        std::fs::create_dir_all(push_dir(&scratch)).expect("create the push folder");
        let corrupted = b"{ not the subscriptions file";
        std::fs::write(&file, corrupted).expect("write a corrupt file");

        let message = refused(
            &dispatch_one_as(&hub, register(APPLE), &tailnet()).await,
            error_codes::INTERNAL,
            "p2",
        );
        assert!(
            !message.contains('/') && !message.contains('\\'),
            "the reply names no path: {message}"
        );
        assert_eq!(std::fs::read(&file).expect("read it back"), corrupted);
    }

    #[tokio::test]
    async fn a_failed_delete_on_block_unloads_the_push_state() {
        let (hub, scratch) = test_hub("push-block-save-fails");
        let device_id = pair(&hub);
        status(
            &dispatch_one_as(&hub, register(APPLE), &tailnet()).await,
            "p2",
        );
        // A folder where the file belongs makes every save fail.
        let file = push_dir(&scratch).join(SUBSCRIPTIONS_FILE);
        std::fs::remove_file(&file).expect("remove the file");
        std::fs::create_dir(&file).expect("make a folder in its place");

        crate::server::pairing::block_device(&hub, &device_id).expect("block");

        assert!(
            !crate::sync::lock(&hub.push).is_loaded(),
            "the blocked device's subscription does not stay in memory"
        );
    }

    #[tokio::test]
    async fn registering_the_same_endpoint_keeps_its_created_at() {
        let (hub, _scratch) = test_hub("push-same-endpoint");
        let device_id = pair(&hub);
        status(
            &dispatch_one_as(&hub, register(APPLE), &tailnet()).await,
            "p2",
        );
        let old = "2020-01-01T00:00:00Z";
        {
            let devices = crate::sync::lock(&hub.devices);
            let mut push = crate::sync::lock(&hub.push);
            let loaded = push.load(&devices).expect("loaded");
            let mut stored = loaded.subs.get(&device_id).expect("stored").clone();
            stored.created_at = old.to_owned();
            loaded.subs.put(&device_id, stored).expect("backdate it");
        }
        let created_at = |hub: &Hub| {
            let devices = crate::sync::lock(&hub.devices);
            let mut push = crate::sync::lock(&hub.push);
            let loaded = push.load(&devices).expect("loaded");
            loaded
                .subs
                .get(&device_id)
                .expect("stored")
                .created_at
                .clone()
        };

        let (p256dh, auth) = keys();
        let mut again = register_with(APPLE, p256dh, auth);
        if let ClientMessage::RegisterPush {
            needs_you,
            finished,
            ..
        } = &mut again
        {
            *needs_you = false;
            *finished = true;
        }
        let switched = status(&dispatch_one_as(&hub, again, &tailnet()).await, "p2");
        assert_eq!(
            (switched.2, switched.3),
            (false, true),
            "the switches change"
        );
        assert_eq!(
            created_at(&hub),
            old,
            "the same endpoint keeps its created_at"
        );

        let other = "https://fcm.googleapis.com/fcm/send/xyz";
        status(
            &dispatch_one_as(&hub, register(other), &tailnet()).await,
            "p2",
        );
        assert_ne!(
            created_at(&hub),
            old,
            "a new endpoint is a new subscription"
        );
    }

    #[tokio::test]
    async fn loading_drops_the_subscription_of_an_unknown_device() {
        let (hub, scratch) = test_hub("push-sweep");
        let device_id = pair(&hub);
        std::fs::create_dir_all(push_dir(&scratch)).expect("create the push folder");
        let mut store = SubscriptionStore::load(&push_dir(&scratch).join(SUBSCRIPTIONS_FILE));
        let (p256dh, auth) = keys();
        for id in [device_id.as_str(), "gone-device"] {
            store
                .put(
                    id,
                    Subscription {
                        kind: "web_push".to_owned(),
                        endpoint: APPLE.to_owned(),
                        p256dh: p256dh.clone(),
                        auth: auth.clone(),
                        needs_you: true,
                        finished: true,
                        created_at: "2026-11-07T00:00:00Z".to_owned(),
                    },
                )
                .expect("store a subscription");
        }

        let loaded = status(
            &dispatch_one_as(&hub, get_push_status(), &tailnet()).await,
            "p1",
        );
        assert_eq!(
            loaded.1.as_deref(),
            Some(APPLE),
            "the paired device's stays"
        );
        assert_eq!(
            stored_ids(&scratch),
            vec![device_id],
            "the orphan is dropped"
        );
    }
}
