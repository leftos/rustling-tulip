//! The gate view's slot-count write and run history. The write is host-only:
//! `host_only_refusal` refuses it on a tailnet connection before it reaches
//! [`handle`]. The history is served to a tailnet client too, which sees the
//! host's slots.

use ::gate_state::{SlotCount, SlotKind};
use protocol::{ClientMessage, DaemonMessage, GateSlotKind, error_codes};
use tracing::warn;

use super::Hub;
use super::connection::ConnCtx;
use crate::gate_state::{GateHub, HistoryLists, RepoFolders};

/// The message a write gets when the counts file's place cannot be read.
const NO_COUNTS_FILE: &str = "the gate's counts file could not be found";

/// Handle the gate messages `dispatch` routes here.
pub(super) fn handle(hub: &Hub, msg: ClientMessage, ctx: &ConnCtx<'_>) {
    match msg {
        ClientMessage::SetGateSlotCount {
            kind,
            count,
            request_id,
        } => set_slot_count(hub, kind, count, request_id, ctx),
        ClientMessage::GateHistory { request_id } => history(hub, request_id, ctx),
        other => warn!(?other, "gate::handle: routed a message it does not own"),
    }
}

/// Read the gate's run history and answer it from a task of its own, the
/// read on a blocking thread. The registered repos are snapshotted first, so
/// no lock is held across the read. A read that fails or stops answers an
/// empty history: the view shows no history and its slots stay up.
fn history(hub: &Hub, request_id: Option<String>, ctx: &ConnCtx<'_>) {
    let Some(gate) = &hub.gate_state else {
        let _ = ctx.out_tx.send(error(
            error_codes::UNSUPPORTED_MESSAGE,
            "the gate view is not served by this daemon".to_owned(),
            request_id,
        ));
        return;
    };
    let repos = hub.state.with_persisted(|state| {
        RepoFolders::new(
            state
                .repos
                .iter()
                .map(|repo| (repo.path.as_str(), repo.name.as_str())),
        )
    });
    let read = gate.history();
    let out_tx = ctx.out_tx.clone();
    tokio::spawn(async move {
        let lists = match tokio::task::spawn_blocking(move || read(repos)).await {
            Ok(lists) => lists.unwrap_or_default(),
            Err(failure) => {
                warn!(error = %failure, "gate state: the history read stopped; no history shown");
                HistoryLists::default()
            }
        };
        let _ = out_tx.send(DaemonMessage::GateHistory {
            request_id,
            runs: lists.runs,
            open: lists.open,
            summary: lists.summary,
        });
    });
}

/// An `Error` of `code` carrying `request_id`.
fn error(code: &str, message: String, request_id: Option<String>) -> DaemonMessage {
    DaemonMessage::Error {
        message,
        request_id,
        code: Some(code.to_owned()),
    }
}

/// Validate a slot-count write and start it: the write and its answer run
/// on a task of their own, so the connection's dispatch never waits on the
/// gate's retries. A written count asks the poll to read again, so the next
/// push carries it.
fn set_slot_count(
    hub: &Hub,
    kind: GateSlotKind,
    count: Option<u32>,
    request_id: Option<String>,
    ctx: &ConnCtx<'_>,
) {
    let refuse = |code: &str, message: &str, request_id| {
        let _ = ctx.out_tx.send(error(code, message.to_owned(), request_id));
    };
    let Some(gate) = &hub.gate_state else {
        refuse(
            error_codes::UNSUPPORTED_MESSAGE,
            "the gate view is not served by this daemon",
            request_id,
        );
        return;
    };
    let slot_count = match count.map(SlotCount::new) {
        None => None,
        Some(Some(slot_count)) => Some(slot_count),
        Some(None) => {
            refuse(
                error_codes::INVALID_REQUEST,
                "a slot count runs from 1 to 64",
                request_id,
            );
            return;
        }
    };
    let slot_kind = match kind {
        GateSlotKind::Heavy => SlotKind::Heavy,
        GateSlotKind::Light => SlotKind::Light,
        GateSlotKind::Unknown => {
            refuse(
                error_codes::INVALID_REQUEST,
                "unknown slot kind",
                request_id,
            );
            return;
        }
    };
    let gate = gate.clone();
    let out_tx = ctx.out_tx.clone();
    tokio::spawn(async move {
        match write(&gate, slot_kind, slot_count).await {
            Ok(()) => {
                let _ = out_tx.send(DaemonMessage::GateSlotCountSet {
                    kind,
                    count,
                    request_id,
                });
                gate.repoll();
            }
            Err(message) => {
                let _ = out_tx.send(error(error_codes::GATE_WRITE_FAILED, message, request_id));
            }
        }
    });
}

/// Find the counts file and apply `count` to `kind` in it, both on a
/// blocking thread; `Err` carries the answer's message.
async fn write(gate: &GateHub, kind: SlotKind, count: Option<SlotCount>) -> Result<(), String> {
    let counts_path = gate.counts_path();
    let done = tokio::task::spawn_blocking(move || {
        let Some(path) = counts_path() else {
            return Err(None);
        };
        ::gate_state::apply_slot_count(&path, kind, count).map_err(Some)
    })
    .await;
    match done {
        Ok(Ok(())) => Ok(()),
        Ok(Err(None)) => {
            warn!("gate state: the gate's counts file could not be found; no slot count written");
            Err(NO_COUNTS_FILE.to_owned())
        }
        Ok(Err(Some(failure))) => {
            warn!(error = %failure, "gate state: the slot count could not be written");
            Err(format!("the slot count could not be written: {failure}"))
        }
        Err(failure) => {
            warn!(error = %failure, "gate state: the slot count write stopped");
            Err(format!("the slot count could not be written: {failure}"))
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::panic,
    clippy::expect_used,
    reason = "a test asserts its preconditions and a wrong reply fails it loudly"
)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, PoisonError, mpsc as std_mpsc};

    use ::gate_state::{SlotCount, read_slot_count_setting};
    use protocol::{
        ClientMessage, DaemonMessage, GateCommandSummary, GateHistoryRun, GateRunState,
        GateSlotKind, error_codes,
    };
    use tokio::sync::{mpsc, watch};

    use crate::gate_state::{
        GateHub, GateReader, HistoryLists, HistorySource, POLL_INTERVAL, RepoFolders, spawn_poll,
    };
    use crate::server::Hub;
    use crate::server::accounts::fake::{HANG, ask_as, dispatch_local, next_reply};
    use crate::server::connection::AuthKind;
    use crate::server::test_support::{
        ScratchDir, dispatch_one, dispatch_one_as, enable_gate_state,
        enable_gate_state_with_history, test_hub, test_peer,
    };

    /// A `SetGateSlotCount` for `kind` carrying `request_id` `id`.
    fn set(kind: GateSlotKind, count: Option<u32>, id: &str) -> ClientMessage {
        ClientMessage::SetGateSlotCount {
            kind,
            count,
            request_id: Some(id.to_owned()),
        }
    }

    /// A hub with a gate state whose counts file is `slot-counts.json` in a
    /// `gate` folder of its scratch dir.
    fn gate_hub(tag: &str) -> (Hub, ScratchDir, PathBuf) {
        let (mut hub, scratch) = test_hub(tag);
        let path = scratch.path().join("gate").join("slot-counts.json");
        enable_gate_state(&mut hub, None, Some(path.clone()));
        (hub, scratch, path)
    }

    /// Fails unless `reply` is the `GateSlotCountSet` for `kind`, `count`
    /// and `id`.
    fn assert_set(reply: DaemonMessage, kind: GateSlotKind, count: Option<u32>, id: &str) {
        let DaemonMessage::GateSlotCountSet {
            kind: set_kind,
            count: set_count,
            request_id,
        } = reply
        else {
            panic!("not a set: {reply:?}");
        };
        assert_eq!(set_kind, kind);
        assert_eq!(set_count, count);
        assert_eq!(request_id.as_deref(), Some(id));
    }

    /// The code, request id and message of an `Error` reply.
    fn error_of(reply: DaemonMessage) -> (Option<String>, Option<String>, String) {
        let DaemonMessage::Error {
            message,
            request_id,
            code,
        } = reply
        else {
            panic!("not an error: {reply:?}");
        };
        (code, request_id, message)
    }

    /// The count the counts file at `path` sets for `kind`.
    fn setting(path: &std::path::Path, kind: GateSlotKind) -> Option<u32> {
        let counts = read_slot_count_setting(path);
        let count = match kind {
            GateSlotKind::Heavy => counts.heavy,
            GateSlotKind::Light => counts.light,
            GateSlotKind::Unknown => panic!("no count is read for an unknown kind"),
        };
        count.map(SlotCount::get)
    }

    #[tokio::test]
    async fn gate_slot_count_write_lands_in_the_counts_file() {
        let (hub, _scratch, path) = gate_hub("gate-write-lands");

        let reply = ask_as(
            &hub,
            set(GateSlotKind::Heavy, Some(3), "w1"),
            &AuthKind::Local,
        )
        .await;

        assert_set(reply, GateSlotKind::Heavy, Some(3), "w1");
        assert_eq!(setting(&path, GateSlotKind::Heavy), Some(3));
    }

    #[tokio::test]
    async fn gate_slot_count_keeps_the_other_kind() {
        let (hub, _scratch, path) = gate_hub("gate-keeps-other");

        let light = ask_as(
            &hub,
            set(GateSlotKind::Light, Some(2), "w1"),
            &AuthKind::Local,
        )
        .await;
        assert_set(light, GateSlotKind::Light, Some(2), "w1");
        let heavy = ask_as(
            &hub,
            set(GateSlotKind::Heavy, Some(5), "w2"),
            &AuthKind::Local,
        )
        .await;
        assert_set(heavy, GateSlotKind::Heavy, Some(5), "w2");

        assert_eq!(setting(&path, GateSlotKind::Light), Some(2));
        assert_eq!(setting(&path, GateSlotKind::Heavy), Some(5));
    }

    #[tokio::test]
    async fn gate_slot_count_clears_to_the_formula() {
        let (hub, _scratch, path) = gate_hub("gate-clears");

        let first = ask_as(
            &hub,
            set(GateSlotKind::Heavy, Some(4), "w1"),
            &AuthKind::Local,
        )
        .await;
        assert_set(first, GateSlotKind::Heavy, Some(4), "w1");
        let cleared = ask_as(&hub, set(GateSlotKind::Heavy, None, "w2"), &AuthKind::Local).await;

        assert_set(cleared, GateSlotKind::Heavy, None, "w2");
        assert_eq!(setting(&path, GateSlotKind::Heavy), None);
    }

    #[tokio::test]
    async fn gate_slot_count_refuses_out_of_range() {
        let (hub, _scratch, path) = gate_hub("gate-out-of-range");
        let first = ask_as(
            &hub,
            set(GateSlotKind::Heavy, Some(3), "w1"),
            &AuthKind::Local,
        )
        .await;
        assert_set(first, GateSlotKind::Heavy, Some(3), "w1");
        let before = fs::read(&path).expect("the counts file was written");

        for count in [0, 65] {
            let reply = dispatch_one(&hub, set(GateSlotKind::Heavy, Some(count), "bad")).await;
            let (code, request_id, message) = error_of(reply);
            assert_eq!(
                code.as_deref(),
                Some(error_codes::INVALID_REQUEST),
                "{count}"
            );
            assert_eq!(request_id.as_deref(), Some("bad"), "{count}");
            assert_eq!(message, "a slot count runs from 1 to 64");
        }

        assert_eq!(
            fs::read(&path).expect("the counts file is still there"),
            before,
            "a refused count leaves the file as it was"
        );
    }

    #[tokio::test]
    async fn gate_slot_count_reports_a_failed_write() {
        let (mut hub, scratch) = test_hub("gate-write-fails");
        let blocker = scratch.path().join("blocker");
        fs::write(&blocker, "a file where the counts folder should be")
            .expect("the blocking file is written");
        enable_gate_state(&mut hub, None, Some(blocker.join("slot-counts.json")));

        let reply = ask_as(
            &hub,
            set(GateSlotKind::Heavy, Some(3), "w1"),
            &AuthKind::Local,
        )
        .await;

        let (code, request_id, message) = error_of(reply);
        assert_eq!(code.as_deref(), Some(error_codes::GATE_WRITE_FAILED));
        assert_eq!(request_id.as_deref(), Some("w1"));
        assert!(
            message.starts_with("the slot count could not be written: "),
            "{message}"
        );
    }

    #[tokio::test]
    async fn gate_slot_count_is_refused_on_a_tailnet_connection() {
        let (hub, _scratch, path) = gate_hub("gate-tailnet");
        let tailnet = AuthKind::Tailnet(test_peer());

        let reply = dispatch_one_as(&hub, set(GateSlotKind::Heavy, Some(3), "w1"), &tailnet).await;

        let (code, request_id, message) = error_of(reply);
        assert_eq!(code.as_deref(), Some(error_codes::LOCAL_ONLY));
        assert_eq!(
            request_id, None,
            "a local-only refusal echoes no request id"
        );
        assert_eq!(message, "gate slot counts are local-only");
        assert!(!path.exists(), "nothing was written");
    }

    #[tokio::test]
    async fn gate_slot_count_without_gate_state_is_unsupported() {
        let (hub, _scratch) = test_hub("gate-unsupported");

        let reply = dispatch_one(&hub, set(GateSlotKind::Heavy, Some(3), "w1")).await;

        let (code, request_id, message) = error_of(reply);
        assert_eq!(code.as_deref(), Some(error_codes::UNSUPPORTED_MESSAGE));
        assert_eq!(request_id.as_deref(), Some("w1"));
        assert_eq!(message, "the gate view is not served by this daemon");
    }

    /// Wait for the scripted reader's next call.
    async fn next_call(calls: &mut mpsc::UnboundedReceiver<()>) {
        tokio::time::timeout(HANG, calls.recv())
            .await
            .expect("a read came")
            .expect("the reader is alive");
    }

    #[tokio::test(start_paused = true)]
    async fn gate_slot_count_repolls_after_a_write() {
        let (hub, _scratch, _path) = gate_hub("gate-repoll");
        let gate = hub.gate_state.clone().expect("the gate state was enabled");
        let reads = Arc::new(AtomicUsize::new(0));
        let (signal, mut calls) = mpsc::unbounded_channel();
        let counted = Arc::clone(&reads);
        let reader: GateReader = Arc::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
            let _ = signal.send(());
            None
        });
        let (_clients, clients_rx) = watch::channel(1_usize);
        let poll = spawn_poll(reader, gate, hub.state_events.clone(), clients_rx);
        next_call(&mut calls).await;
        let connected = tokio::time::Instant::now();

        let reply = ask_as(
            &hub,
            set(GateSlotKind::Light, Some(2), "w1"),
            &AuthKind::Local,
        )
        .await;
        assert_set(reply, GateSlotKind::Light, Some(2), "w1");
        next_call(&mut calls).await;

        assert_eq!(
            reads.load(Ordering::SeqCst),
            2,
            "one read on connect, one after the write"
        );
        assert!(
            connected.elapsed() < POLL_INTERVAL,
            "the read after the write waited for no tick"
        );
        poll.abort();
    }

    #[tokio::test]
    async fn gate_other_messages_dispatch_while_a_write_retries() {
        let (mut hub, scratch) = test_hub("gate-write-held");
        let path = scratch.path().join("gate").join("slot-counts.json");
        let (started_tx, started_rx) = std_mpsc::channel();
        let (release_tx, release_rx) = std_mpsc::channel::<()>();
        let started = Mutex::new(started_tx);
        let release = Mutex::new(release_rx);
        hub.gate_state = Some(GateHub::new(
            Arc::new(move || {
                let _ = started
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .send(());
                let _ = release
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .recv();
                Some(path.clone())
            }),
            source(|_| None),
        ));
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();

        tokio::time::timeout(
            HANG,
            dispatch_local(&hub, set(GateSlotKind::Heavy, Some(3), "w1"), &out_tx),
        )
        .await
        .expect("the write did not hold the connection's dispatch");
        // Off the runtime's thread, so the write's task can run meanwhile.
        tokio::task::spawn_blocking(move || started_rx.recv_timeout(HANG))
            .await
            .expect("the wait did not panic")
            .expect("the write reached its blocking task");
        // A count out of range is refused by the dispatch itself, at once.
        tokio::time::timeout(
            HANG,
            dispatch_local(&hub, set(GateSlotKind::Light, Some(0), "h1"), &out_tx),
        )
        .await
        .expect("another message was dispatched while the write is held");

        let (code, request_id, _) = error_of(next_reply(&mut out_rx).await);
        assert_eq!(
            request_id.as_deref(),
            Some("h1"),
            "the other message came first"
        );
        assert_eq!(code.as_deref(), Some(error_codes::INVALID_REQUEST));

        release_tx.send(()).expect("the write is still waiting");
        assert_set(
            next_reply(&mut out_rx).await,
            GateSlotKind::Heavy,
            Some(3),
            "w1",
        );
    }

    /// A history source from a closure.
    fn source(
        read: impl Fn(RepoFolders) -> Option<HistoryLists> + Send + Sync + 'static,
    ) -> HistorySource {
        Arc::new(read)
    }

    /// A `GateHistory` request carrying `request_id` `id`.
    fn ask_history(id: &str) -> ClientMessage {
        ClientMessage::GateHistory {
            request_id: Some(id.to_owned()),
        }
    }

    /// A hub with a gate state whose history reads come from `history`, and
    /// whose counts file is `slot-counts.json` in a `gate` folder of its
    /// scratch dir.
    fn history_hub(tag: &str, history: HistorySource) -> (Hub, ScratchDir) {
        let (mut hub, scratch) = test_hub(tag);
        let path = scratch.path().join("gate").join("slot-counts.json");
        enable_gate_state_with_history(&mut hub, None, Some(path), history);
        (hub, scratch)
    }

    /// A passed run of `cargo test` by gate `pid`.
    fn history_run(pid: u32) -> GateHistoryRun {
        GateHistoryRun {
            command: "cargo test".to_owned(),
            cwd: Some("X:\\dev\\repo".to_owned()),
            repo: Some("repo".to_owned()),
            kind: "heavy".to_owned(),
            slot: Some(0),
            remote: false,
            host: None,
            fallback: None,
            reason: None,
            pid,
            process_start: None,
            parent_pid: None,
            state: GateRunState::Ok,
            running: false,
            queued: None,
            started: None,
            waited_seconds: None,
            held_seconds: Some(12.0),
            status: Some(0),
            verdict: None,
            wall_seconds: None,
            adjusted_seconds: None,
            abandoned: Vec::new(),
            remote_take: None,
            cold: None,
            critical_pid: None,
        }
    }

    /// A history of one run, one open run and one summary row.
    fn some_history() -> HistoryLists {
        HistoryLists {
            runs: vec![history_run(1)],
            open: vec![history_run(2)],
            summary: vec![GateCommandSummary {
                command: "cargo test".to_owned(),
                repo: Some("repo".to_owned()),
                remote: false,
                count: 1,
                p50_seconds: 12.0,
                p90_seconds: 12.0,
                max_seconds: 12.0,
            }],
        }
    }

    /// The request id and lists of a `GateHistory` reply.
    fn history_of(reply: DaemonMessage) -> (Option<String>, HistoryLists) {
        let DaemonMessage::GateHistory {
            request_id,
            runs,
            open,
            summary,
        } = reply
        else {
            panic!("not a history: {reply:?}");
        };
        (
            request_id,
            HistoryLists {
                runs,
                open,
                summary,
            },
        )
    }

    #[tokio::test]
    async fn gate_history_reply_carries_the_source() {
        let history = some_history();
        let (hub, _scratch) =
            history_hub("gate-history-reply", source(move |_| Some(some_history())));

        let reply = ask_as(&hub, ask_history("h1"), &AuthKind::Local).await;

        assert_eq!(history_of(reply), (Some("h1".to_owned()), history));
    }

    #[tokio::test]
    async fn gate_history_read_failure_is_an_empty_reply() {
        let (hub, _scratch) = history_hub("gate-history-fails", source(|_| None));

        let reply = ask_as(&hub, ask_history("h1"), &AuthKind::Local).await;

        assert_eq!(
            history_of(reply),
            (Some("h1".to_owned()), HistoryLists::default())
        );
        let write = ask_as(
            &hub,
            set(GateSlotKind::Heavy, Some(3), "w1"),
            &AuthKind::Local,
        )
        .await;
        assert_set(write, GateSlotKind::Heavy, Some(3), "w1");
    }

    #[tokio::test]
    async fn gate_history_without_gate_state_is_unsupported() {
        let (hub, _scratch) = test_hub("gate-history-unsupported");

        let reply = dispatch_one(&hub, ask_history("h1")).await;

        let (code, request_id, message) = error_of(reply);
        assert_eq!(code.as_deref(), Some(error_codes::UNSUPPORTED_MESSAGE));
        assert_eq!(request_id.as_deref(), Some("h1"));
        assert_eq!(message, "the gate view is not served by this daemon");
    }

    #[tokio::test]
    async fn gate_history_is_served_on_a_tailnet_connection() {
        let history = some_history();
        let (hub, _scratch) = history_hub(
            "gate-history-tailnet",
            source(move |_| Some(some_history())),
        );
        let tailnet = AuthKind::Tailnet(test_peer());

        let reply = ask_as(&hub, ask_history("h1"), &tailnet).await;

        assert_eq!(history_of(reply), (Some("h1".to_owned()), history));
    }
}
