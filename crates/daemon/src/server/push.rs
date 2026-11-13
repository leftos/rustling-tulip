//! The phone's Web Push requests: its push status, and its one subscription
//! registered, replaced or removed, keyed by the device record its tailnet
//! stable id resolves to (`docs/plans/mobile-app.md`, "Push").
use super::Hub;
use super::connection::{AuthKind, ConnCtx};
use crate::push::state::Loaded;
use crate::push::store::{ApnsEnvironment, Channel, Subscription};
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

/// Serve one push request in three phases, so `devices` is never held
/// across the push folder's file I/O: [`lookup`] finds the peer's device id
/// under `devices` alone, [`write`] loads the push state and stores or
/// removes the subscription under `push` alone, and [`confirm`] takes
/// `devices` then `push` (`server/pairing.rs` has the lock order), sweeps
/// the state and checks the device again, undoing a write that raced a
/// block, forget or re-pair.
fn serve(
    hub: &Hub,
    msg: &ClientMessage,
    auth: &AuthKind,
    request_id: &str,
) -> Result<DaemonMessage, Refusal> {
    let AuthKind::Tailnet(peer) = auth else {
        return Err(Refusal::invalid("push is for paired phones on the tailnet"));
    };
    let device_id = lookup(hub, peer)?;
    let (reply, registered) = write(hub, msg, &device_id, request_id)?;
    confirm(hub, peer, &device_id, registered)?;
    Ok(reply)
}

/// The id of `peer`'s unrevoked device record, read under `devices` alone.
fn lookup(hub: &Hub, peer: &tailnet::PeerIdentity) -> Result<String, Refusal> {
    crate::sync::lock(&hub.devices)
        .unrevoked_id_of(&peer.stable_id)
        .map(str::to_owned)
        .ok_or_else(not_paired)
}

/// The refusal of a peer without an unrevoked device record.
fn not_paired() -> Refusal {
    Refusal::invalid("this device is not paired")
}

/// The request's push work under `push` alone: the state is loaded (unswept;
/// [`confirm`] sweeps it), a `RegisterPush` stores the device's subscription
/// and an `UnregisterPush` removes it. Returns the `PushStatus` reply and
/// whether a subscription was stored.
fn write(
    hub: &Hub,
    msg: &ClientMessage,
    device_id: &str,
    request_id: &str,
) -> Result<(DaemonMessage, bool), Refusal> {
    let mut push = crate::sync::lock(&hub.push);
    let policy = push.policy();
    let new_subscription = match msg {
        ClientMessage::RegisterPush {
            registration,
            needs_you,
            finished,
            hold_at_desk,
            ..
        } => Some(subscription(
            registration,
            *needs_you,
            *finished,
            *hold_at_desk,
            policy,
        )?),
        _ => None,
    };
    let loaded = push.load_unswept().map_err(|err| Refusal::internal(&err))?;
    match store(loaded, msg, new_subscription, device_id) {
        Ok(registered) => Ok((push_status(loaded, device_id, request_id), registered)),
        Err(err) => {
            // The state may be unswept, and `confirm` does not run after a
            // refusal: unloaded, it is swept again on the next load.
            push.unload();
            Err(Refusal::internal(&err))
        }
    }
}

/// Store `new` for a `RegisterPush`, or remove the device's subscription
/// for an `UnregisterPush`; returns whether a subscription was stored.
fn store(
    loaded: &mut Loaded,
    msg: &ClientMessage,
    new_subscription: Option<Subscription>,
    device_id: &str,
) -> anyhow::Result<bool> {
    match (msg, new_subscription) {
        (ClientMessage::RegisterPush { .. }, Some(mut new)) => {
            // The same address is the same subscription with new switches:
            // it keeps when it was first stored. A replacement token or
            // endpoint is a new subscription.
            if let Some(stored) = loaded.subs.get(device_id)
                && stored.channel.address() == new.channel.address()
            {
                new.created_at.clone_from(&stored.created_at);
            }
            loaded.subs.put(device_id, new)?;
            info!(%device_id, "push: the device's subscription was stored");
            Ok(true)
        }
        (ClientMessage::UnregisterPush { .. }, _) => {
            if loaded.subs.remove(device_id)? {
                info!(%device_id, "push: the device's subscription was removed");
            }
            Ok(false)
        }
        _ => Ok(false),
    }
}

/// Check the device again under `devices` then `push`, after [`write`]
/// released `push`: the state is swept against `devices`, and when this
/// request stored a subscription (`registered`) but `peer` no longer
/// resolves to `device_id` (blocked, forgotten or re-paired meanwhile), the
/// subscription is deleted and the request refused as [`lookup`] refuses. An
/// unregister is never refused here.
fn confirm(
    hub: &Hub,
    peer: &tailnet::PeerIdentity,
    device_id: &str,
    registered: bool,
) -> Result<(), Refusal> {
    let devices = crate::sync::lock(&hub.devices);
    let mut push = crate::sync::lock(&hub.push);
    if let Err(err) = push.sweep(&devices) {
        // The sweep unloaded the state, so nothing it would drop stays in
        // memory and the next load sweeps again; the request still stands.
        warn!(
            err = format!("{err:#}"),
            "push: the sweep could not save; the push state is unloaded until the next request"
        );
    }
    if registered && devices.unrevoked_id_of(&peer.stable_id) != Some(device_id) {
        push.remove_device(device_id);
        info!(%device_id, "push: the device changed during the request; its subscription was undone");
        return Err(not_paired());
    }
    Ok(())
}

/// The longest relay token a registration may carry.
const MAX_RELAY_TOKEN: usize = 512;

/// The subscription a `RegisterPush` asks for, checked: a Web Push
/// subscription's endpoint is `https` to a known push service, a relay
/// channel's token and environment are as the relay addresses them, and
/// every kind's keys decode.
fn subscription(
    registration: &PushRegistration,
    needs_you: bool,
    finished: bool,
    hold_at_desk: bool,
    policy: crate::push::send::PushPolicy,
) -> Result<Subscription, Refusal> {
    let channel = match registration {
        PushRegistration::WebPush {
            endpoint,
            p256dh,
            auth,
        } => web_push_channel(endpoint, p256dh, auth, policy)?,
        PushRegistration::Apns {
            token,
            environment,
            p256dh,
            auth,
        } => apns_channel(token, *environment, p256dh, auth)?,
        PushRegistration::Fcm {
            token,
            p256dh,
            auth,
        } => fcm_channel(token, p256dh, auth)?,
        PushRegistration::Unknown => {
            return Err(Refusal::invalid(
                "this daemon does not know that kind of push registration",
            ));
        }
    };
    Ok(Subscription {
        channel,
        needs_you,
        finished,
        hold_at_desk,
        created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    })
}

/// A Web Push subscription's channel: the endpoint is `https` to a known
/// push service and the keys decode. The refusals name the host or the key at
/// fault, never the endpoint or the secret itself.
fn web_push_channel(
    endpoint: &Secret,
    p256dh: &str,
    auth: &Secret,
    policy: crate::push::send::PushPolicy,
) -> Result<Channel, Refusal> {
    crate::push::send::check_endpoint(&endpoint.0, policy)
        .map_err(|err| Refusal::invalid(err.to_string()))?;
    push_crypto::Subscriber::from_b64url(p256dh, &auth.0)
        .map_err(|err| Refusal::invalid(err.to_string()))?;
    Ok(Channel::WebPush {
        endpoint: endpoint.0.clone(),
        p256dh: p256dh.to_owned(),
        auth: auth.0.clone(),
    })
}

/// An APNs channel: the token is hexadecimal and its environment is one APNs
/// has, and the keys decode.
fn apns_channel(
    token: &Secret,
    environment: protocol::ApnsEnvironment,
    p256dh: &str,
    auth: &Secret,
) -> Result<Channel, Refusal> {
    check_relay_token(&token.0)?;
    if !token.0.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(Refusal::invalid("the APNs push token is not hexadecimal"));
    }
    let environment = match environment {
        protocol::ApnsEnvironment::Production => ApnsEnvironment::Production,
        protocol::ApnsEnvironment::Sandbox => ApnsEnvironment::Sandbox,
        protocol::ApnsEnvironment::Unknown => {
            return Err(Refusal::invalid(
                "this daemon does not know that APNs environment",
            ));
        }
    };
    push_crypto::Subscriber::from_b64url(p256dh, &auth.0)
        .map_err(|err| Refusal::invalid(err.to_string()))?;
    Ok(Channel::Apns {
        token: token.0.clone(),
        environment,
        p256dh: p256dh.to_owned(),
        auth: auth.0.clone(),
    })
}

/// An FCM channel: the token is any non-empty one within the length bound,
/// and the keys decode.
fn fcm_channel(token: &Secret, p256dh: &str, auth: &Secret) -> Result<Channel, Refusal> {
    check_relay_token(&token.0)?;
    push_crypto::Subscriber::from_b64url(p256dh, &auth.0)
        .map_err(|err| Refusal::invalid(err.to_string()))?;
    Ok(Channel::Fcm {
        token: token.0.clone(),
        p256dh: p256dh.to_owned(),
        auth: auth.0.clone(),
    })
}

/// A relay token is checked before it is stored: never empty and never over
/// [`MAX_RELAY_TOKEN`] characters. The refusals name the fault, never the
/// token.
fn check_relay_token(token: &str) -> Result<(), Refusal> {
    if token.is_empty() {
        return Err(Refusal::invalid("the push token is empty"));
    }
    if token.chars().count() > MAX_RELAY_TOKEN {
        return Err(Refusal::invalid("the push token is over 512 characters"));
    }
    Ok(())
}

/// `device_id`'s push status: the daemon's key and the device's
/// subscription, or no endpoint and both switches off without one. Only a
/// Web Push subscription has an endpoint; a relay channel answers its
/// `device_token` instead, which the phone app compares with its own token.
fn push_status(loaded: &Loaded, device_id: &str, request_id: &str) -> DaemonMessage {
    let stored = loaded.subs.get(device_id);
    DaemonMessage::PushStatus {
        vapid_public_key: loaded.key.public_b64url(),
        endpoint: stored.and_then(|sub| match &sub.channel {
            Channel::WebPush { endpoint, .. } => Some(Secret(endpoint.clone())),
            Channel::Apns { .. } | Channel::Fcm { .. } => None,
        }),
        device_token: stored.and_then(|sub| match &sub.channel {
            Channel::Apns { token, .. } | Channel::Fcm { token, .. } => Some(Secret(token.clone())),
            Channel::WebPush { .. } => None,
        }),
        needs_you: stored.is_some_and(|sub| sub.needs_you),
        finished: stored.is_some_and(|sub| sub.finished),
        hold_at_desk: stored.is_none_or(|sub| sub.hold_at_desk),
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
    use crate::push::store::{Channel, SUBSCRIPTIONS_FILE, Subscription, SubscriptionStore};
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
            hold_at_desk: true,
            request_id: "p2".to_owned(),
        }
    }

    fn register(endpoint: &str) -> ClientMessage {
        let (p256dh, auth) = keys();
        register_with(endpoint, p256dh, auth)
    }

    fn register_apns(
        token: &str,
        environment: protocol::ApnsEnvironment,
        p256dh: String,
        auth: String,
    ) -> ClientMessage {
        ClientMessage::RegisterPush {
            registration: protocol::PushRegistration::Apns {
                token: protocol::Secret(token.to_owned()),
                environment,
                p256dh,
                auth: protocol::Secret(auth),
            },
            needs_you: true,
            finished: false,
            hold_at_desk: true,
            request_id: "p2".to_owned(),
        }
    }

    /// An APNs registration for `token` in the production environment, with a
    /// fresh key pair.
    fn apns_with(token: &str) -> ClientMessage {
        let (p256dh, auth) = keys();
        register_apns(token, protocol::ApnsEnvironment::Production, p256dh, auth)
    }

    /// An FCM registration for `token`, with a fresh key pair.
    fn fcm_with(token: &str) -> ClientMessage {
        let (p256dh, auth) = keys();
        ClientMessage::RegisterPush {
            registration: protocol::PushRegistration::Fcm {
                token: protocol::Secret(token.to_owned()),
                p256dh,
                auth: protocol::Secret(auth),
            },
            needs_you: true,
            finished: false,
            hold_at_desk: true,
            request_id: "p2".to_owned(),
        }
    }

    /// The reply's `device_token`, failing on anything but `PushStatus`.
    fn token_of(reply: &DaemonMessage, want_id: &str) -> Option<String> {
        match reply {
            DaemonMessage::PushStatus {
                device_token,
                request_id,
                ..
            } => {
                assert_eq!(request_id.as_deref(), Some(want_id), "{reply:?}");
                device_token.as_ref().map(|token| token.0.clone())
            }
            other => panic!("want PushStatus, got {other:?}"),
        }
    }

    /// The channel stored for `device_id`, failing when none is stored.
    fn stored_channel(
        scratch: &crate::server::test_support::ScratchDir,
        device_id: &str,
    ) -> Channel {
        SubscriptionStore::load(&push_dir(scratch).join(SUBSCRIPTIONS_FILE))
            .get(device_id)
            .expect("the device's subscription is stored")
            .channel
            .clone()
    }

    /// The reply's `PushStatus` fields, failing on anything else.
    fn status(reply: &DaemonMessage, want_id: &str) -> (String, Option<String>, bool, bool, bool) {
        match reply {
            DaemonMessage::PushStatus {
                vapid_public_key,
                endpoint,
                device_token: _,
                needs_you,
                finished,
                hold_at_desk,
                request_id,
            } => {
                assert_eq!(request_id.as_deref(), Some(want_id), "{reply:?}");
                (
                    vapid_public_key.clone(),
                    endpoint.as_ref().map(|e| e.0.clone()),
                    *needs_you,
                    *finished,
                    *hold_at_desk,
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
        assert!(
            first.4,
            "nothing registered reports the hold switch in its default, on"
        );
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
    async fn register_push_stores_hold_at_desk() {
        let (hub, scratch) = test_hub("push-hold-at-desk");
        let device_id = pair(&hub);
        let (p256dh, auth) = keys();
        let mut msg = register_with(APPLE, p256dh, auth);
        if let ClientMessage::RegisterPush { hold_at_desk, .. } = &mut msg {
            *hold_at_desk = false;
        }

        let reply = status(&dispatch_one_as(&hub, msg, &tailnet()).await, "p2");

        assert!(!reply.4, "the status echoes the switch");
        let stored = SubscriptionStore::load(&push_dir(&scratch).join(SUBSCRIPTIONS_FILE));
        assert!(
            !stored.get(&device_id).expect("stored").hold_at_desk,
            "the subscription is stored with the switch off"
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
            hold_at_desk: true,
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
    async fn an_apns_registration_is_stored_and_answered_with_its_token() {
        let (hub, scratch) = test_hub("push-apns");
        let device_id = pair(&hub);
        // The longest token the check accepts: 512 hexadecimal characters.
        let token = "a0".repeat(256);

        let reply = dispatch_one_as(&hub, apns_with(&token), &tailnet()).await;

        assert_eq!(token_of(&reply, "p2").as_deref(), Some(token.as_str()));
        let echoed = status(&reply, "p2");
        assert_eq!(echoed.1, None, "an APNs channel has no endpoint");
        assert_eq!((echoed.2, echoed.3), (true, false), "the switches echo");
        assert!(
            matches!(
                stored_channel(&scratch, &device_id),
                Channel::Apns {
                    token: stored,
                    environment: ApnsEnvironment::Production,
                    ..
                } if stored == token
            ),
            "the channel is stored as it came"
        );
    }

    #[tokio::test]
    async fn an_fcm_registration_is_stored_and_answered_with_its_token() {
        let (hub, scratch) = test_hub("push-fcm");
        let device_id = pair(&hub);
        let token = "fcm-token-1".to_owned();

        let reply = dispatch_one_as(&hub, fcm_with(&token), &tailnet()).await;

        assert_eq!(token_of(&reply, "p2").as_deref(), Some(token.as_str()));
        assert_eq!(
            status(&reply, "p2").1,
            None,
            "an FCM channel has no endpoint"
        );
        assert!(matches!(
            stored_channel(&scratch, &device_id),
            Channel::Fcm { token: stored, .. } if stored == token
        ));
    }

    #[tokio::test]
    async fn a_replacement_apns_token_replaces_the_subscription() {
        let (hub, scratch) = test_hub("push-apns-replace");
        let device_id = pair(&hub);
        let first = "a1b2c3d4";
        let second = "d4c3b2a1";
        status(
            &dispatch_one_as(&hub, apns_with(first), &tailnet()).await,
            "p2",
        );

        let reply = dispatch_one_as(&hub, apns_with(second), &tailnet()).await;

        assert_eq!(token_of(&reply, "p2").as_deref(), Some(second));
        assert_eq!(
            stored_ids(&scratch),
            vec![device_id.clone()],
            "one entry per device"
        );
        assert!(
            matches!(
                stored_channel(&scratch, &device_id),
                Channel::Apns { token: stored, .. } if stored == second
            ),
            "the new token is the one stored"
        );
    }

    #[tokio::test]
    async fn a_malformed_apns_token_is_refused_without_naming_it() {
        let (hub, scratch) = test_hub("push-apns-bad-token");
        pair(&hub);
        for (token, want) in [
            (String::new(), "the push token is empty"),
            ("a".repeat(513), "the push token is over 512 characters"),
            ("zz12".to_owned(), "the APNs push token is not hexadecimal"),
        ] {
            let message = refused(
                &dispatch_one_as(&hub, apns_with(&token), &tailnet()).await,
                error_codes::INVALID_REQUEST,
                "p2",
            );
            assert_eq!(message, want);
            if !token.is_empty() {
                assert!(!message.contains(&token), "{message}");
            }
        }
        assert!(stored_ids(&scratch).is_empty(), "nothing is stored");
    }

    #[tokio::test]
    async fn an_unknown_apns_environment_is_refused() {
        let (hub, scratch) = test_hub("push-apns-unknown-environment");
        pair(&hub);
        let (p256dh, auth) = keys();
        let msg = register_apns("a1b2c3", protocol::ApnsEnvironment::Unknown, p256dh, auth);

        let message = refused(
            &dispatch_one_as(&hub, msg, &tailnet()).await,
            error_codes::INVALID_REQUEST,
            "p2",
        );

        assert_eq!(message, "this daemon does not know that APNs environment");
        assert!(stored_ids(&scratch).is_empty(), "nothing is stored");
    }

    #[tokio::test]
    async fn an_fcm_token_need_not_be_hex() {
        let (hub, scratch) = test_hub("push-fcm-non-hex");
        let device_id = pair(&hub);
        let token = "fcm:token-with-dashes_1".to_owned();

        let reply = dispatch_one_as(&hub, fcm_with(&token), &tailnet()).await;

        assert_eq!(token_of(&reply, "p2").as_deref(), Some(token.as_str()));
        assert!(matches!(
            stored_channel(&scratch, &device_id),
            Channel::Fcm { token: stored, .. } if stored == token
        ));
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
                        channel: Channel::WebPush {
                            endpoint: APPLE.to_owned(),
                            p256dh: p256dh.clone(),
                            auth: auth.clone(),
                        },
                        needs_you: true,
                        finished: true,
                        hold_at_desk: true,
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

    // ---- a block, forget or re-pair racing a request, phase by phase ----

    /// The phase's value, failing on a refusal.
    fn served<T>(result: Result<T, Refusal>) -> T {
        match result {
            Ok(value) => value,
            Err(refusal) => panic!("refused: {} {}", refusal.code, refusal.message),
        }
    }

    /// Fail unless `confirm` refused as the lookup refuses an unpaired peer.
    fn not_paired(result: Result<(), Refusal>) {
        match result {
            Err(refusal) => assert_eq!(
                (refusal.code, refusal.message.as_str()),
                (error_codes::INVALID_REQUEST, "this device is not paired")
            ),
            Ok(()) => panic!("want the not-paired refusal, got Ok"),
        }
    }

    /// Whether the loaded push state holds a subscription for `device_id`.
    fn held_in_memory(hub: &Hub, device_id: &str) -> bool {
        let devices = crate::sync::lock(&hub.devices);
        let mut push = crate::sync::lock(&hub.push);
        let loaded = push.load(&devices).expect("loaded");
        loaded.subs.get(device_id).is_some()
    }

    #[tokio::test]
    async fn a_block_between_lookup_and_write_leaves_no_subscription() {
        let (hub, scratch) = test_hub("push-race-block-early");
        let device_id = pair(&hub);
        let peer = test_peer();

        let id = served(lookup(&hub, &peer));
        // `block_device`'s store calls, on a state not loaded yet.
        crate::sync::lock(&hub.devices).revoke(&id).expect("revoke");
        crate::sync::lock(&hub.push).remove_device(&id);
        let (_, registered) = served(write(&hub, &register(APPLE), &id, "p2"));
        not_paired(confirm(&hub, &peer, &id, registered));

        assert!(!stored_ids(&scratch).contains(&device_id), "none on disk");
        assert!(!held_in_memory(&hub, &device_id), "none in memory");
    }

    #[tokio::test]
    async fn a_failed_write_unloads_the_push_state() {
        let (hub, scratch) = test_hub("push-write-save-fails");
        let device_id = pair(&hub);
        crate::sync::lock(&hub.push).load_unswept().expect("loaded");
        // A folder where the file belongs makes every save fail.
        std::fs::create_dir(push_dir(&scratch).join(SUBSCRIPTIONS_FILE))
            .expect("make a folder in the file's place");

        let Err(refusal) = write(&hub, &register(APPLE), &device_id, "p2") else {
            panic!("want the save to fail");
        };

        assert_eq!(refusal.code, error_codes::INTERNAL);
        assert!(
            !crate::sync::lock(&hub.push).is_loaded(),
            "an unswept state does not stay loaded past a failed save"
        );
    }

    #[tokio::test]
    async fn a_block_between_write_and_confirm_leaves_no_subscription() {
        let (hub, scratch) = test_hub("push-race-block");
        let device_id = pair(&hub);
        let peer = test_peer();

        let id = served(lookup(&hub, &peer));
        let (_, registered) = served(write(&hub, &register(APPLE), &id, "p2"));
        assert!(registered, "the register wrote a subscription");
        crate::server::pairing::block_device(&hub, &id).expect("block");
        not_paired(confirm(&hub, &peer, &id, registered));

        assert!(!stored_ids(&scratch).contains(&device_id), "none on disk");
        assert!(!held_in_memory(&hub, &device_id), "none in memory");
    }

    #[tokio::test]
    async fn a_forget_between_lookup_and_write_leaves_no_subscription() {
        let (hub, scratch) = test_hub("push-race-forget");
        let device_id = pair(&hub);
        let peer = test_peer();

        let id = served(lookup(&hub, &peer));
        crate::sync::lock(&hub.devices).forget(&id).expect("forget");
        crate::sync::lock(&hub.push).remove_device(&id);
        let (_, registered) = served(write(&hub, &register(APPLE), &id, "p2"));
        not_paired(confirm(&hub, &peer, &id, registered));

        assert!(!stored_ids(&scratch).contains(&device_id), "none on disk");
        assert!(!held_in_memory(&hub, &device_id), "none in memory");
    }

    #[tokio::test]
    async fn a_re_pair_between_lookup_and_write_leaves_no_subscription() {
        let (hub, scratch) = test_hub("push-race-re-pair");
        let old_id = pair(&hub);
        let peer = test_peer();

        let id = served(lookup(&hub, &peer));
        let new_id = pair(&hub);
        crate::sync::lock(&hub.push).remove_device(&id);
        let (_, registered) = served(write(&hub, &register(APPLE), &id, "p2"));
        not_paired(confirm(&hub, &peer, &id, registered));

        assert_ne!(new_id, old_id, "the re-pair made a new device id");
        assert!(stored_ids(&scratch).is_empty(), "none on disk");
        assert!(!held_in_memory(&hub, &old_id), "none in memory");
    }

    #[tokio::test]
    async fn an_unregister_racing_a_block_answers_as_before() {
        let (hub, scratch) = test_hub("push-race-unregister");
        let device_id = pair(&hub);
        status(
            &dispatch_one_as(&hub, register(APPLE), &tailnet()).await,
            "p2",
        );
        let peer = test_peer();
        let unregister = ClientMessage::UnregisterPush {
            request_id: "p3".to_owned(),
        };

        let id = served(lookup(&hub, &peer));
        crate::server::pairing::block_device(&hub, &id).expect("block");
        let (reply, registered) = served(write(&hub, &unregister, &id, "p3"));
        assert!(!registered, "an unregister registers nothing");
        assert!(
            confirm(&hub, &peer, &id, registered).is_ok(),
            "an unregister is never refused at the re-check"
        );

        let after = status(&reply, "p3");
        assert_eq!((after.1, after.2, after.3), (None, false, false));
        assert!(stored_ids(&scratch).is_empty(), "the subscription is gone");
        assert!(!held_in_memory(&hub, &device_id), "none in memory");
    }
}
