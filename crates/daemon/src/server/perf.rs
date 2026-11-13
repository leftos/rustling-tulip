//! The performance monitor's message: a connection subscribing to, or
//! leaving, the machine's CPU and memory samples. Read-only, so a tailnet or
//! SSH connection may send it too.
use super::Hub;
use super::connection::ConnCtx;
use protocol::ClientMessage;
use tracing::warn;

/// Handle the performance monitor's messages `dispatch` routes here.
pub(super) fn handle(hub: &Hub, msg: &ClientMessage, ctx: &ConnCtx<'_>) {
    match msg {
        ClientMessage::WatchPerf { on: true } => {
            hub.perf.subscribe(ctx.connection, ctx.out_tx.clone());
        }
        ClientMessage::WatchPerf { on: false } => hub.perf.unsubscribe(ctx.connection),
        _ => warn!("perf::handle: a message that is not the performance monitor's was routed here"),
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests assert preconditions with expect")]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use protocol::ClientMessage;
    use tokio::sync::{Mutex as AsyncMutex, mpsc};

    use crate::server::connection::{AuthKind, ConnCtx, FetchRegistry, PtyLane};
    use crate::server::dispatch;
    use crate::server::test_support::{test_hub, test_peer};

    #[tokio::test]
    async fn watch_perf_from_a_tailnet_connection_is_accepted() {
        let (hub, _scratch) = test_hub("perf-tailnet");
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        let (file_tx, _file_rx) = mpsc::channel(1);
        let pty_lane: Arc<PtyLane> = Arc::default();
        let fetches: FetchRegistry = Arc::new(AsyncMutex::new(HashMap::new()));
        let tailnet = AuthKind::Tailnet(test_peer());
        let ctx = ConnCtx {
            out_tx: &out_tx,
            file_tx: &file_tx,
            pty_lane: &pty_lane,
            fetches: &fetches,
            tails: &Arc::default(),
            client_id: "c1",
            client_name: None,
            connection: 7,
            auth: &tailnet,
        };
        let count = hub.perf.watcher_count();

        dispatch(&hub, ClientMessage::WatchPerf { on: true }, &ctx)
            .await
            .expect("dispatch succeeds");

        assert!(out_rx.try_recv().is_err(), "no refusal is sent");
        assert_eq!(*count.borrow(), 1, "the tailnet connection watches");

        dispatch(&hub, ClientMessage::WatchPerf { on: false }, &ctx)
            .await
            .expect("dispatch succeeds");
        assert_eq!(*count.borrow(), 0);
        assert!(out_rx.try_recv().is_err());
    }
}
