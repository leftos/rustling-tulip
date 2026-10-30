//! The summarizer's messages: its settings, a provider key, and the test
//! summary. Success of a settings or key change is the status broadcast; a
//! test summary answers its requester alone.
use std::sync::Arc;

use protocol::{ClientMessage, DaemonMessage};
use tokio::sync::mpsc;
use tracing::warn;

use super::Hub;
use super::connection::ConnCtx;
use crate::user_env::Secret;

/// Handle the summarizer messages `dispatch` routes here. No log line or
/// reply carries a key.
///
/// # Errors
///
/// A refused change that carried no `request_id`, which the connection
/// answers with `Error { request_id: None }`.
pub(super) async fn handle(hub: &Hub, msg: ClientMessage, ctx: &ConnCtx<'_>) -> anyhow::Result<()> {
    match msg {
        ClientMessage::SetSummarizerSettings {
            settings,
            request_id,
        } => {
            let outcome = hub.summarizer.set_settings(settings).await;
            answer_refusal(ctx.out_tx, outcome, request_id)
        }
        ClientMessage::SetSummarizerKey {
            provider,
            key,
            request_id,
        } => {
            let key = key.map(|key| Secret::new(key.0));
            let outcome = hub.summarizer.set_key(&provider, key).await;
            answer_refusal(ctx.out_tx, outcome, request_id)
        }
        ClientMessage::TestSummary { request_id } => {
            spawn_test(hub, ctx.out_tx.clone(), request_id);
            Ok(())
        }
        _ => {
            warn!("summarizer::handle: a message that is not the summarizer's was routed here");
            Ok(())
        }
    }
}

/// Answer a refusal of a request that carried a `request_id` with an `Error`
/// echoing it, to this requester only; any other outcome is returned as it
/// came.
fn answer_refusal(
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
    outcome: anyhow::Result<()>,
    request_id: Option<String>,
) -> anyhow::Result<()> {
    match (outcome, request_id) {
        (Err(err), Some(request_id)) => {
            warn!(error = %format!("{err:#}"), %request_id, "summarizer request refused");
            let _ = out_tx.send(DaemonMessage::Error {
                message: err.to_string(),
                request_id: Some(request_id),
            });
            Ok(())
        }
        (outcome, _) => outcome,
    }
}

/// Run the test summary on its own task, so a slow provider does not hold up
/// the connection, and answer the requester with its `SummaryTest` or
/// `Error`.
fn spawn_test(hub: &Hub, out_tx: mpsc::UnboundedSender<DaemonMessage>, request_id: Option<String>) {
    let summarizer = Arc::clone(&hub.summarizer);
    tokio::spawn(async move {
        let reply = match summarizer.test().await {
            Ok(reply) => DaemonMessage::SummaryTest {
                result: reply.result,
                latency_ms: reply.latency_ms,
                cost_usd: reply.cost_usd,
                request_id,
            },
            Err(err) => {
                warn!(error = %format!("{err:#}"), "summarizer: the test summary was refused");
                DaemonMessage::Error {
                    message: err.to_string(),
                    request_id,
                }
            }
        };
        let _ = out_tx.send(reply);
    });
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests assert preconditions with expect")]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Arc;

    use protocol::{
        ClientMessage, DaemonMessage, KeyState, SummarizerKey, SummarizerSettings, SummarySource,
    };
    use tokio::sync::{Mutex as AsyncMutex, broadcast, mpsc};

    use crate::env_secrets::test_support::scratch_async;
    use crate::server::connection::{AuthKind, ConnCtx, FetchRegistry, PtyLane};
    use crate::server::handshake::push_initial_state;
    use crate::server::test_support::{
        billing_hub, drain, hello, loopback_ws_client, next_message, test_hub, test_peer, ws_next,
        ws_send,
    };
    use crate::server::{Hub, StateEvent, dispatch};
    use crate::summarizer::keys::Rejections;
    use crate::summarizer::policy::Line;
    use crate::summarizer::prompt::{self, Trigger};
    use crate::summarizer::providers;
    use crate::summarizer::service::test_support::{FAKE_NOTIFICATION, FakeCaller};
    use crate::summarizer::service::{Outcome, Request, Summarizer, SummarizerParts};
    use crate::user_env::Secret;

    /// A key no message, event or file may ever contain.
    const KEY_SENTINEL: &str = "sentinel-summarizer-key-7c41d2";

    /// The HTTP provider, the one that takes a key.
    fn keyed_provider() -> String {
        providers::TABLE
            .first()
            .expect("the table has a row")
            .id
            .to_owned()
    }

    /// One test connection: its id and the queue its messages land on.
    struct Conn {
        id: u64,
        out_tx: mpsc::UnboundedSender<DaemonMessage>,
        rx: mpsc::UnboundedReceiver<DaemonMessage>,
    }

    fn conn(id: u64) -> Conn {
        let (out_tx, rx) = mpsc::unbounded_channel();
        Conn { id, out_tx, rx }
    }

    async fn send_as(hub: &Hub, conn: &Conn, auth: &AuthKind, msg: ClientMessage) {
        let (file_tx, _file_rx) = mpsc::channel(1);
        let pty_lane: Arc<PtyLane> = Arc::default();
        let fetches: FetchRegistry = Arc::new(AsyncMutex::new(HashMap::new()));
        let ctx = ConnCtx {
            out_tx: &conn.out_tx,
            file_tx: &file_tx,
            pty_lane: &pty_lane,
            fetches: &fetches,
            client_id: "c1",
            client_name: None,
            connection: conn.id,
            auth,
        };
        dispatch(hub, msg, &ctx).await.expect("dispatch succeeds");
    }

    async fn send(hub: &Hub, conn: &Conn, msg: ClientMessage) {
        send_as(hub, conn, &AuthKind::Local, msg).await;
    }

    fn request(trigger: Trigger, text: &str) -> Request {
        Request {
            session_id: "s1".to_owned(),
            trigger,
            input: prompt::shape(&[text]),
        }
    }

    fn line(outcome: Outcome) -> Line {
        match outcome {
            Outcome::Line { line, .. } => Some(line),
            Outcome::Replaced | Outcome::Idle | Outcome::Skipped => None,
        }
        .expect("the request made a line")
    }

    /// The `cap_reached` of every status broadcast waiting on `events`.
    fn cap_flags(events: &mut broadcast::Receiver<StateEvent>) -> Vec<bool> {
        let mut flags = Vec::new();
        while let Ok(event) = events.try_recv() {
            if let StateEvent::SummarizerStatus { cap_reached, .. } = event {
                flags.push(cap_reached);
            }
        }
        flags
    }

    /// The key states of every status broadcast waiting on `events`.
    fn key_states(events: &mut broadcast::Receiver<StateEvent>) -> Vec<Vec<SummarizerKey>> {
        let mut states = Vec::new();
        while let Ok(event) = events.try_recv() {
            if let StateEvent::SummarizerStatus { keys, .. } = event {
                states.push(keys);
            }
        }
        states
    }

    fn holds(keys: &[SummarizerKey], provider: &str, state: KeyState) -> bool {
        keys.iter()
            .any(|key| key.provider == provider && key.state == state)
    }

    fn set_key(provider: &str, key: Option<&str>, request_id: &str) -> ClientMessage {
        ClientMessage::SetSummarizerKey {
            provider: provider.to_owned(),
            key: key.map(|key| protocol::Secret(key.to_owned())),
            request_id: Some(request_id.to_owned()),
        }
    }

    /// Every file's text under `dir`, with its path.
    fn files_under(dir: &Path, found: &mut Vec<(String, String)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                files_under(&path, found);
            } else if let Ok(bytes) = std::fs::read(&path) {
                found.push((
                    path.display().to_string(),
                    String::from_utf8_lossy(&bytes).into_owned(),
                ));
            }
        }
    }

    /// Read `client`'s frames until a status shows the keyed provider's key
    /// stored, checking that no frame carries the sentinel.
    async fn read_until_stored(client: &mut crate::server::test_support::WsClient, provider: &str) {
        loop {
            let message = ws_next(client)
                .await
                .expect("the daemon sends the status before closing");
            let json = serde_json::to_string(&message).expect("a message serializes");
            assert!(!json.contains(KEY_SENTINEL), "{json}");
            assert!(!format!("{message:?}").contains(KEY_SENTINEL));
            if let DaemonMessage::SummarizerStatus { keys, .. } = &message
                && holds(keys, provider, KeyState::Stored)
            {
                return;
            }
        }
    }

    #[tokio::test]
    async fn a_key_set_is_never_echoed() {
        let (_lock, _config) = scratch_async("summ-echo").await;
        let (hub, scratch) = test_hub("summ-echo");
        let provider = keyed_provider();
        hub.state
            .set_summarizer_settings(hub.state.summarizer_settings())
            .expect("write state.json, so its text is scanned too");
        let mut events = hub.state_events.subscribe();
        let mut setter = loopback_ws_client(&hub).await;
        let mut watcher = loopback_ws_client(&hub).await;
        ws_send(&mut setter, &hello(&hub.auth_token, None)).await;
        ws_send(&mut watcher, &hello(&hub.auth_token, None)).await;
        ws_send(&mut setter, &set_key(&provider, Some(KEY_SENTINEL), "k1")).await;

        read_until_stored(&mut setter, &provider).await;
        read_until_stored(&mut watcher, &provider).await;
        while let Ok(event) = events.try_recv() {
            assert!(!format!("{event:?}").contains(KEY_SENTINEL), "{event:?}");
        }
        let mut files = Vec::new();
        files_under(scratch.path(), &mut files);
        assert!(!files.is_empty(), "the hub wrote its files");
        for (path, text) in files {
            assert!(!text.contains(KEY_SENTINEL), "{path} holds the key");
        }
    }

    #[tokio::test]
    async fn setting_a_key_broadcasts_a_status_with_the_key_state() {
        let (_lock, _config) = scratch_async("summ-key-status").await;
        let (hub, _scratch) = test_hub("summ-key-status");
        let provider = keyed_provider();
        let mut events = hub.state_events.subscribe();
        let c1 = conn(1);

        send(&hub, &c1, set_key(&provider, Some("a-saved-key"), "k1")).await;
        let states = key_states(&mut events);
        assert_eq!(states.len(), 1, "one status per change: {states:?}");
        assert!(holds(&states[0], &provider, KeyState::Stored), "{states:?}");

        send(&hub, &c1, set_key(&provider, None, "k2")).await;
        let states = key_states(&mut events);
        assert_eq!(states.len(), 1, "{states:?}");
        assert!(
            holds(&states[0], &provider, KeyState::Missing),
            "{states:?}"
        );
    }

    #[tokio::test]
    async fn the_cap_switches_to_templates() {
        let (hub, _scratch, fake) = billing_hub("summ-cap", 0.6);
        let mut events = hub.state_events.subscribe();

        for text in ["First report.", "Second report."] {
            let made = line(hub.summarizer.submit(request(Trigger::Stop, text)).await);
            assert_eq!(made.source, SummarySource::Model, "{text}");
            assert_eq!(made.notification, FAKE_NOTIFICATION);
        }
        let capped = line(
            hub.summarizer
                .submit(request(Trigger::Stop, "Third report."))
                .await,
        );
        assert_eq!(capped.source, SummarySource::Template);
        assert_eq!(capped.headline, "Turn finished");
        assert_eq!(capped.notification, "Third report.");
        assert_eq!(fake.calls(), 2, "the capped request made no call");

        let flags = cap_flags(&mut events);
        assert_eq!(
            flags,
            [false, true],
            "one status carries the cap's crossing"
        );
    }

    #[tokio::test]
    async fn half_the_cap_stops_refreshes() {
        let (hub, _scratch, fake) = billing_hub("summ-half-cap", 0.5);
        let first = line(
            hub.summarizer
                .submit(request(Trigger::Stop, "Report one."))
                .await,
        );
        assert_eq!(first.source, SummarySource::Model);

        let refresh = hub
            .summarizer
            .submit(request(Trigger::Working, "Still working."))
            .await;
        assert_eq!(
            refresh,
            Outcome::Skipped,
            "a refresh stops at half, with no template line"
        );
        assert_eq!(fake.calls(), 1);

        let stop = line(
            hub.summarizer
                .submit(request(Trigger::Stop, "Report two."))
                .await,
        );
        assert_eq!(
            stop.source,
            SummarySource::Model,
            "a stop summary still calls"
        );
        assert_eq!(fake.calls(), 2);
    }

    #[tokio::test]
    async fn test_summary_answers_only_its_requester() {
        let (hub, _scratch) = test_hub("summ-test-requester");
        let mut c1 = conn(1);
        let mut c2 = conn(2);

        send(
            &hub,
            &c1,
            ClientMessage::TestSummary {
                request_id: Some("t1".to_owned()),
            },
        )
        .await;
        let reply = next_message(&mut c1.rx).await;
        assert!(
            matches!(
                &reply,
                DaemonMessage::SummaryTest { result, request_id: Some(id), .. }
                    if id == "t1"
                        && result.source == SummarySource::Model
                        && result.notification == FAKE_NOTIFICATION
            ),
            "{reply:?}"
        );
        assert!(drain(&mut c1.rx).is_empty(), "one reply");
        assert!(
            drain(&mut c2.rx).is_empty(),
            "the other connection hears nothing"
        );
    }

    #[tokio::test]
    async fn test_summary_is_refused_with_an_error_at_the_cap_and_with_no_provider() {
        let (hub, _scratch, fake) = billing_hub("summ-test-refused", 1.0);
        let mut c1 = conn(1);
        let test = |id: &str| ClientMessage::TestSummary {
            request_id: Some(id.to_owned()),
        };

        send(&hub, &c1, test("t1")).await;
        let reply = next_message(&mut c1.rx).await;
        assert!(
            matches!(reply, DaemonMessage::SummaryTest { .. }),
            "{reply:?}"
        );

        send(&hub, &c1, test("t2")).await;
        let reply = next_message(&mut c1.rx).await;
        assert!(
            matches!(
                &reply,
                DaemonMessage::Error { message, request_id: Some(id) }
                    if id == "t2" && message.contains("cap")
            ),
            "{reply:?}"
        );
        assert_eq!(fake.calls(), 1, "the refused test made no call");

        hub.state
            .set_summarizer_settings(SummarizerSettings {
                provider: "a-provider-no-longer-offered".to_owned(),
                ..hub.state.summarizer_settings()
            })
            .expect("store the settings");
        send(&hub, &c1, test("t3")).await;
        let reply = next_message(&mut c1.rx).await;
        assert!(
            matches!(
                &reply,
                DaemonMessage::Error { message, request_id: Some(id) }
                    if id == "t3" && message.contains("No summarizer provider")
            ),
            "{reply:?}"
        );
        assert_eq!(fake.calls(), 1);
    }

    #[tokio::test]
    async fn status_is_sent_on_connect() {
        let (hub, _scratch) = test_hub("summ-status-connect");
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();

        push_initial_state(&hub, &out_tx, "c1", false);

        let mut kinds = Vec::new();
        for message in drain(&mut out_rx) {
            match message {
                DaemonMessage::AgentCommands { .. } => kinds.push("agent_commands"),
                DaemonMessage::SummarizerStatus {
                    settings,
                    providers,
                    keys,
                    cap_reached,
                    ..
                } => {
                    assert_eq!(settings, hub.state.summarizer_settings());
                    assert!(providers.iter().any(|p| p.id == settings.provider));
                    assert!(
                        holds(&keys, &keyed_provider(), KeyState::Missing),
                        "{keys:?}"
                    );
                    assert!(!cap_reached);
                    kinds.push("summarizer_status");
                }
                _ => {}
            }
        }
        assert_eq!(kinds, ["agent_commands", "summarizer_status"]);
    }

    #[tokio::test]
    async fn a_startup_key_refresh_that_changes_a_key_state_broadcasts_status() {
        let (_lock, _config) = scratch_async("summ-startup-keys").await;
        let (hub, _scratch) = test_hub("summ-startup-keys");
        let provider = keyed_provider();
        crate::summarizer::keys::set(&provider, &Secret::new("a-saved-key".to_owned()))
            .expect("the key saves");
        let mut events = hub.state_events.subscribe();

        hub.summarizer.startup_key_check().await;
        let states = key_states(&mut events);
        assert_eq!(states.len(), 1, "{states:?}");
        assert!(holds(&states[0], &provider, KeyState::Stored), "{states:?}");

        hub.summarizer.startup_key_check().await;
        assert!(
            key_states(&mut events).is_empty(),
            "a check that changes nothing broadcasts nothing"
        );
    }

    #[tokio::test]
    async fn a_rejected_key_gives_the_template_without_a_call_or_broadcast() {
        let (_lock, _config) = scratch_async("summ-rejected-key").await;
        let (mut hub, _scratch) = test_hub("summ-rejected-key");
        let provider = keyed_provider();
        let key = Secret::new("a-refused-key".to_owned());
        crate::summarizer::keys::set(&provider, &key).expect("the key saves");
        let rejections = Arc::new(Rejections::default());
        rejections.reject(&provider, &key);
        let fake = Arc::new(FakeCaller::billing(0.0));
        let (client_count, _) = tokio::sync::watch::channel(1usize);
        hub.summarizer = Arc::new(Summarizer::new(SummarizerParts {
            state: Arc::clone(&hub.state),
            spend_file: crate::paths::summarizer_spend_file(&hub.dirs),
            caller: Some(fake.clone()),
            rejections,
            client_count: client_count.subscribe(),
            events: hub.state_events.clone(),
        }));
        hub.state
            .set_summarizer_settings(SummarizerSettings {
                enabled: true,
                provider: provider.clone(),
                key_source: protocol::SummarizerKeySource::Stored,
                daily_cap_usd: 1.0,
            })
            .expect("store the settings");
        hub.summarizer.startup_key_check().await;
        let mut events = hub.state_events.subscribe();

        let made = line(
            hub.summarizer
                .submit(request(Trigger::Stop, "A report."))
                .await,
        );
        assert_eq!(made.source, SummarySource::Template);
        assert_eq!(fake.calls(), 0, "no call with a refused key");
        assert!(key_states(&mut events).is_empty(), "and no broadcast");

        hub.summarizer
            .test()
            .await
            .expect("a test summary still calls, since the user is testing");
        assert_eq!(fake.calls(), 1);
    }

    #[tokio::test]
    async fn the_summarizer_test_says_the_vault_is_unreachable() {
        use crate::env_secrets::test_support::{
            NO_LOGON_SESSION, inject_error, store_error_with_code,
        };
        let (_lock, _config) = scratch_async("summ-test-unreachable").await;
        let (hub, _scratch, fake) = billing_hub("summ-test-unreachable", 0.0);
        let provider = keyed_provider();
        crate::summarizer::keys::set(&provider, &Secret::new("a-saved-key".to_owned()))
            .expect("the key saves");
        hub.state
            .set_summarizer_settings(SummarizerSettings {
                enabled: true,
                provider: provider.clone(),
                key_source: protocol::SummarizerKeySource::Stored,
                daily_cap_usd: 1.0,
            })
            .expect("store the settings");
        inject_error(
            &format!("summarizer/{provider}"),
            store_error_with_code(NO_LOGON_SESSION),
        );

        let refusal = hub
            .summarizer
            .test()
            .await
            .expect_err("the test cannot read the key");

        assert_eq!(refusal.to_string(), crate::env_secrets::VAULT_UNREACHABLE);
        assert_eq!(fake.calls(), 0, "no call without the key");
    }

    #[tokio::test]
    async fn a_tailnet_connection_may_set_the_key() {
        let (_lock, _config) = scratch_async("summ-tailnet-key").await;
        let (hub, _scratch) = test_hub("summ-tailnet-key");
        let provider = keyed_provider();
        let mut events = hub.state_events.subscribe();
        let mut c1 = conn(1);
        let tailnet = AuthKind::Tailnet(test_peer());

        send_as(
            &hub,
            &c1,
            &tailnet,
            set_key(&provider, Some("a-remote-key"), "k1"),
        )
        .await;

        assert!(drain(&mut c1.rx).is_empty(), "no refusal");
        let states = key_states(&mut events);
        assert_eq!(states.len(), 1, "{states:?}");
        assert!(holds(&states[0], &provider, KeyState::Stored), "{states:?}");
    }
}
