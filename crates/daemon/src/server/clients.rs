//! The record of live client connections, and telling them to update.
use super::connection::AuthKind;
use super::{ConnCtx, Hub};
use protocol::{ClientKind, ClientMessage, DaemonMessage};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tracing::{debug, info};

/// The id the next client update request takes.
static NEXT_CLIENT_UPDATE: AtomicU64 = AtomicU64::new(1);

/// Who asked, for a request that came from this PC.
const THIS_PC: &str = "a connection on this PC";

/// How many characters of a tailnet peer's name `requested_by` keeps.
const REQUESTER_CHARS: usize = 64;

/// What the daemon knows about one served connection.
#[derive(Clone, Debug)]
pub(crate) struct ClientEntry {
    /// Where a message addressed to this connection goes. A send on a closed
    /// channel is ignored.
    pub(super) out_tx: mpsc::UnboundedSender<DaemonMessage>,
    /// What kind of program it is; `None` for a command-line tool and for an
    /// older client.
    pub(super) kind: Option<ClientKind>,
    /// The version it reported, absent when it has none.
    pub(super) version: Option<String>,
    /// The `CLIENT_FEATURE_*` strings it implements.
    pub(super) features: Vec<String>,
    /// Its self-reported name, absent when it sent none.
    pub(super) name: Option<String>,
    /// How it authenticated.
    pub(super) auth: AuthKind,
}

/// Every served connection's record, by connection id. An entry lives only
/// while its connection does: [`Clients::register`] hands back a guard that
/// removes it on every way out, panic included.
#[derive(Clone, Default)]
pub(crate) struct Clients {
    entries: Arc<Mutex<HashMap<u64, ClientEntry>>>,
}

impl Clients {
    /// Record `connection` and hand back the guard that removes it.
    pub(super) fn register(&self, connection: u64, entry: ClientEntry) -> ClientGuard {
        crate::sync::lock(&self.entries).insert(connection, entry);
        ClientGuard {
            clients: self.clone(),
            connection,
        }
    }

    /// A copy of every entry, taken under the lock, which is released before
    /// the caller sends on any of them.
    fn snapshot(&self) -> Vec<(u64, ClientEntry)> {
        crate::sync::lock(&self.entries)
            .iter()
            .map(|(connection, entry)| (*connection, entry.clone()))
            .collect()
    }
}

/// Removes a connection's entry when dropped, on every way out, panic
/// included.
pub(crate) struct ClientGuard {
    clients: Clients,
    connection: u64,
}

impl Drop for ClientGuard {
    fn drop(&mut self) {
        crate::sync::lock(&self.clients.entries).remove(&self.connection);
    }
}

/// Handle [`ClientMessage::UpdateConnectedClients`], the one message `dispatch`
/// routes here.
pub(super) fn handle(hub: &Hub, msg: ClientMessage, ctx: &ConnCtx<'_>) {
    match msg {
        ClientMessage::UpdateConnectedClients { request_id } => {
            notify(hub, request_id, ctx);
        }
        other => debug!(
            ?other,
            "clients: a message that is not a client update message was routed here"
        ),
    }
}

/// Tell every other client connected to this host to update itself: a native
/// client that advertises `update_yourself` gets [`DaemonMessage::UpdateYourself`],
/// a phone gets [`DaemonMessage::ReloadApp`], and a native client without the
/// capability is counted rather than told. The requester answers with
/// [`DaemonMessage::ConnectedClientsNotified`].
fn notify(hub: &Hub, request_id: Option<String>, ctx: &ConnCtx<'_>) {
    let requested_by = requested_by(ctx);
    let update_id = mint_update_id();
    let mut native = 0_u32;
    let mut phones = 0_u32;
    let mut too_old = 0_u32;
    for (connection, entry) in hub.clients.snapshot() {
        if connection == ctx.connection {
            continue;
        }
        match entry.kind {
            Some(ClientKind::Native) if can_update(&entry.features) => {
                native += 1;
                info!(
                    %connection,
                    version = entry.version.as_deref().unwrap_or("(unknown)"),
                    name = entry.name.as_deref().unwrap_or("(none)"),
                    auth = ?entry.auth,
                    "client update: told a native client"
                );
                send(
                    &entry.out_tx,
                    DaemonMessage::UpdateYourself {
                        update_id: update_id.clone(),
                        requested_by: requested_by.clone(),
                    },
                );
            }
            Some(ClientKind::Native) => {
                too_old += 1;
                info!(%connection, "client update: a native client cannot update itself");
            }
            Some(ClientKind::Phone) => {
                phones += 1;
                send(&entry.out_tx, DaemonMessage::ReloadApp);
            }
            Some(ClientKind::Unknown) | None => {}
        }
    }
    info!(%native, %phones, %too_old, "client update: told the connected clients");
    send(
        ctx.out_tx,
        DaemonMessage::ConnectedClientsNotified {
            request_id,
            native,
            phones,
            too_old,
        },
    );
}

/// Whether the client advertised that it updates itself when asked.
fn can_update(features: &[String]) -> bool {
    features
        .iter()
        .any(|feature| feature == protocol::CLIENT_FEATURE_UPDATE_YOURSELF)
}

/// Who asked, as display text: the client's own name if it sent one, else a
/// tailnet device's name (bounded, control characters dropped), else this PC.
fn requested_by(ctx: &ConnCtx<'_>) -> String {
    if let Some(name) = ctx.client_name {
        return name.to_owned();
    }
    match ctx.auth {
        AuthKind::Local => THIS_PC.to_owned(),
        AuthKind::Tailnet(peer) => peer
            .name
            .chars()
            .filter(|c| !c.is_control())
            .take(REQUESTER_CHARS)
            .collect(),
    }
}

/// A fresh client update id: unique in this daemon run, and carrying the
/// process id so a later daemon's ids differ.
fn mint_update_id() -> String {
    let number = NEXT_CLIENT_UPDATE.fetch_add(1, Ordering::Relaxed);
    format!("client-update-{}-{number}", std::process::id())
}

/// Sends `message` to `out_tx`; a connection that is gone takes nothing.
fn send(out_tx: &mpsc::UnboundedSender<DaemonMessage>, message: DaemonMessage) {
    let _ = out_tx.send(message);
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use crate::server::test_support::{dispatch_one_as, test_hub};

    /// One registered connection and the channel its messages arrive on.
    struct Registered {
        rx: mpsc::UnboundedReceiver<DaemonMessage>,
        _guard: ClientGuard,
    }

    fn register(
        hub: &Hub,
        connection: u64,
        kind: Option<ClientKind>,
        features: &[&str],
    ) -> Registered {
        let (out_tx, rx) = mpsc::unbounded_channel();
        let entry = ClientEntry {
            out_tx,
            kind,
            version: Some("0.9.0".to_owned()),
            features: features
                .iter()
                .map(|feature| (*feature).to_owned())
                .collect(),
            name: None,
            auth: AuthKind::Local,
        };
        let guard = hub.clients.register(connection, entry);
        Registered { rx, _guard: guard }
    }

    /// Dispatch the request over the helper's connection 1 and return its
    /// answer.
    async fn update_connected_clients(hub: &Hub) -> DaemonMessage {
        dispatch_one_as(
            hub,
            ClientMessage::UpdateConnectedClients {
                request_id: Some("req-1".to_owned()),
            },
            &AuthKind::Local,
        )
        .await
    }

    #[tokio::test]
    async fn update_connected_clients_tells_capable_native_clients_and_phones() {
        let (hub, _scratch) = test_hub("clients-notify");
        let mut native = register(
            &hub,
            2,
            Some(ClientKind::Native),
            &[protocol::CLIENT_FEATURE_UPDATE_YOURSELF],
        );
        let mut phone = register(&hub, 3, Some(ClientKind::Phone), &[]);

        let reply = update_connected_clients(&hub).await;

        let update = native.rx.try_recv().expect("the native client is told");
        assert!(
            matches!(
                &update,
                DaemonMessage::UpdateYourself { update_id, requested_by }
                    if update_id.starts_with("client-update-")
                        && requested_by == THIS_PC
            ),
            "{update:?}"
        );
        assert!(native.rx.try_recv().is_err(), "one message per client");
        assert!(
            matches!(phone.rx.try_recv(), Ok(DaemonMessage::ReloadApp)),
            "the phone is told to reload"
        );
        assert!(
            matches!(
                &reply,
                DaemonMessage::ConnectedClientsNotified {
                    request_id: Some(id),
                    native: 1,
                    phones: 1,
                    too_old: 0,
                } if id == "req-1"
            ),
            "{reply:?}"
        );
    }

    #[tokio::test]
    async fn it_skips_the_requester() {
        let (hub, _scratch) = test_hub("clients-requester");
        // The dispatch helper speaks over connection 1.
        let mut requester = register(
            &hub,
            1,
            Some(ClientKind::Native),
            &[protocol::CLIENT_FEATURE_UPDATE_YOURSELF],
        );

        let reply = update_connected_clients(&hub).await;

        assert!(
            requester.rx.try_recv().is_err(),
            "the requester is not told"
        );
        assert!(
            matches!(
                &reply,
                DaemonMessage::ConnectedClientsNotified {
                    native: 0,
                    phones: 0,
                    too_old: 0,
                    ..
                }
            ),
            "{reply:?}"
        );
    }

    #[tokio::test]
    async fn it_counts_a_native_client_without_the_capability_as_too_old() {
        let (hub, _scratch) = test_hub("clients-too-old");
        let mut old = register(&hub, 2, Some(ClientKind::Native), &[]);

        let reply = update_connected_clients(&hub).await;

        assert!(
            old.rx.try_recv().is_err(),
            "an older native client gets nothing"
        );
        assert!(
            matches!(
                &reply,
                DaemonMessage::ConnectedClientsNotified {
                    native: 0,
                    phones: 0,
                    too_old: 1,
                    ..
                }
            ),
            "{reply:?}"
        );
    }

    #[tokio::test]
    async fn it_ignores_a_connection_with_no_kind() {
        let (hub, _scratch) = test_hub("clients-no-kind");
        let mut anonymous = register(&hub, 2, None, &[protocol::CLIENT_FEATURE_UPDATE_YOURSELF]);

        let reply = update_connected_clients(&hub).await;

        assert!(
            anonymous.rx.try_recv().is_err(),
            "a connection with no kind is not told"
        );
        assert!(
            matches!(
                &reply,
                DaemonMessage::ConnectedClientsNotified {
                    native: 0,
                    phones: 0,
                    too_old: 0,
                    ..
                }
            ),
            "{reply:?}"
        );
    }

    #[tokio::test]
    async fn a_closed_connection_leaves_the_record() {
        let (hub, _scratch) = test_hub("clients-closed");
        let registered = register(
            &hub,
            2,
            Some(ClientKind::Native),
            &[protocol::CLIENT_FEATURE_UPDATE_YOURSELF],
        );
        // The guard is what a connection's teardown drops.
        drop(registered);

        let reply = update_connected_clients(&hub).await;

        assert!(
            matches!(
                &reply,
                DaemonMessage::ConnectedClientsNotified {
                    native: 0,
                    phones: 0,
                    too_old: 0,
                    ..
                }
            ),
            "a closed connection is neither told nor counted: {reply:?}"
        );
    }
}
