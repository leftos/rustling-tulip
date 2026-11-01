//! The file follows: `TailFile` starts following a file inside the session's
//! folders ([`crate::file_tail`]) on a task of its own, whose
//! `FileTail` messages ride the connection's file queue; `UntailFile`, the
//! session's removal and the connection's end stop it. A connection runs at
//! most [`file_tail::MAX_FOLLOWS`] follows. `UntailFile` and a restart wait
//! for the stopped follow's task to end, so nothing it sends comes after
//! what follows; the follow does not wait for a read still blocked.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::Hub;
use super::connection::{ConnCtx, TailRegistry};
use crate::file_tail::{self, Follow};
use crate::session::{SessionEvent, SessionRegistry};
use crate::sync::lock;
use protocol::error_codes::{INVALID_REQUEST, LIMIT_REACHED, NOT_FOUND};
use protocol::{ClientMessage, DaemonMessage};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

/// Tells one follow's entry from a later follow's of the same file.
static NEXT_FOLLOW: AtomicU64 = AtomicU64::new(0);

/// One follow in a connection's [`TailRegistry`].
pub(super) struct TailEntry {
    id: u64,
    token: CancellationToken,
    task: JoinHandle<()>,
}

impl TailEntry {
    /// Stop the follow without waiting for it.
    pub(super) fn cancel(&self) {
        self.token.cancel();
    }

    /// Stop the follow and wait for its task to end.
    async fn stop(self) {
        self.token.cancel();
        if let Err(err) = self.task.await {
            warn!(error = %err, "tail: a follow's task failed");
        }
    }
}

/// Handle the follow messages `dispatch` routes here.
pub(super) async fn handle(hub: &Hub, msg: ClientMessage, ctx: &ConnCtx<'_>) {
    match msg {
        ClientMessage::TailFile {
            session_id,
            path,
            request_id,
        } => {
            if let Err((code, message)) = start(hub, ctx, session_id, path).await {
                // A closed outbox is a connection already ending.
                let _ = ctx.out_tx.send(DaemonMessage::Error {
                    message,
                    code: Some(code.to_owned()),
                    request_id,
                });
            }
        }
        ClientMessage::UntailFile { session_id, path } => {
            let stopped = ctx.tails.lock().await.remove(&(session_id, path));
            if let Some(entry) = stopped {
                entry.stop().await;
            } else {
                debug!("UntailFile for a file this connection does not follow");
            }
        }
        other => warn!(
            ?other,
            "tail: a message that is not a follow was routed here"
        ),
    }
}

/// Start following `path` for `session_id`, replacing this connection's
/// follow of the same file; the refusal's code and message otherwise.
async fn start(
    hub: &Hub,
    ctx: &ConnCtx<'_>,
    session_id: String,
    path: String,
) -> Result<(), (&'static str, String)> {
    // Subscribed before the lookup, so a removal right after it is seen.
    let events = hub.sessions.subscribe();
    let Some(record) = hub.sessions.get(&session_id) else {
        return Err((NOT_FOUND, format!("unknown session: {session_id}")));
    };
    let listed = file_tail::session_folders(&lock(&record));
    let refuse = |why: String| {
        (
            INVALID_REQUEST,
            format!("refusing to follow {path:?}: {why}"),
        )
    };
    let normal = file_tail::check(&listed, &path).map_err(refuse)?;
    let folders = file_tail::prepare(listed, normal.clone())
        .await
        .map_err(refuse)?;
    let key = (session_id.clone(), path.clone());
    let old = {
        let mut tails = ctx.tails.lock().await;
        let old = tails.remove(&key);
        let running = tails
            .values()
            .filter(|entry| !entry.task.is_finished())
            .count();
        if old.is_none() && running >= file_tail::MAX_FOLLOWS {
            return Err((
                LIMIT_REACHED,
                format!(
                    "a connection follows at most {} files; stop one before following {path:?}",
                    file_tail::MAX_FOLLOWS
                ),
            ));
        }
        old
    };
    let restart = old.is_some();
    if let Some(old) = old {
        old.stop().await;
    }
    let spec = Follow {
        session_id,
        path,
        normal,
        folders,
        restart,
    };
    let id = NEXT_FOLLOW.fetch_add(1, Ordering::Relaxed);
    let token = CancellationToken::new();
    let sessions = Arc::clone(&hub.sessions);
    let registry = Arc::clone(ctx.tails);
    let file_tx = ctx.file_tx.clone();
    let follow_token = token.clone();
    // Held across the spawn, so the task cannot forget its entry before it
    // is there.
    let mut tails = ctx.tails.lock().await;
    let entry_key = key.clone();
    let task = tokio::spawn(async move {
        let session_id = spec.session_id.clone();
        tokio::select! {
            () = file_tail::follow(spec, file_tx, follow_token) => {}
            () = removed(events, &sessions, &session_id) => {}
        }
        forget(&registry, &entry_key, id).await;
    });
    tails.insert(key, TailEntry { id, token, task });
    Ok(())
}

/// Drop a finished follow's entry, unless the key since belongs to a newer
/// follow or whoever stopped it already took it.
async fn forget(tails: &TailRegistry, key: &(String, String), id: u64) {
    let mut tails = tails.lock().await;
    if tails.get(key).is_some_and(|entry| entry.id == id) {
        tails.remove(key);
    }
}

/// Resolves once `session_id` leaves the registry.
async fn removed(
    mut events: broadcast::Receiver<SessionEvent>,
    sessions: &SessionRegistry,
    session_id: &str,
) {
    loop {
        match events.recv().await {
            Ok(SessionEvent::Removed(id)) if id == session_id => return,
            Ok(_) => {}
            Err(broadcast::error::RecvError::Lagged(_)) => {
                if sessions.get(session_id).is_none() {
                    return;
                }
            }
            Err(broadcast::error::RecvError::Closed) => std::future::pending::<()>().await,
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert preconditions with expect; panic messages aid debugging"
)]
mod tests {
    use super::super::connection::{AuthKind, FetchRegistry, PtyLane, stop_transfers};
    use super::super::dispatch;
    use super::super::test_support::{HANG_GUARD, ScratchDir, idle_record, test_hub};
    use super::*;
    use crate::file_tail::hooks::{OpenHold, make_dir_link};
    use base64::Engine as _;
    use protocol::SessionMember;
    use std::io::Write as _;
    use std::path::{Path, PathBuf};
    use tokio::sync::mpsc;

    /// One connection's queues and registries, as `client_session` holds
    /// them.
    struct Conn {
        out_tx: mpsc::UnboundedSender<DaemonMessage>,
        out_rx: mpsc::UnboundedReceiver<DaemonMessage>,
        file_tx: mpsc::Sender<DaemonMessage>,
        file_rx: mpsc::Receiver<DaemonMessage>,
        lane: Arc<PtyLane>,
        fetches: FetchRegistry,
        tails: TailRegistry,
    }

    impl Conn {
        fn new() -> Self {
            let (out_tx, out_rx) = mpsc::unbounded_channel();
            let (file_tx, file_rx) = mpsc::channel(16);
            Self {
                out_tx,
                out_rx,
                file_tx,
                file_rx,
                lane: Arc::default(),
                fetches: FetchRegistry::default(),
                tails: TailRegistry::default(),
            }
        }

        async fn send(&self, hub: &Hub, msg: ClientMessage) {
            let ctx = ConnCtx {
                out_tx: &self.out_tx,
                file_tx: &self.file_tx,
                pty_lane: &self.lane,
                fetches: &self.fetches,
                tails: &self.tails,
                client_id: "c1",
                client_name: None,
                connection: 1,
                auth: &AuthKind::Local,
            };
            dispatch(hub, msg, &ctx).await.expect("dispatch succeeds");
        }

        async fn tail(&self, hub: &Hub, path: &str) {
            let msg = ClientMessage::TailFile {
                session_id: "s1".to_owned(),
                path: path.to_owned(),
                request_id: Some("req".to_owned()),
            };
            self.send(hub, msg).await;
        }

        /// The next `FileTail`: its data, offset and reset flag.
        async fn next_tail(&mut self) -> (Vec<u8>, u64, bool) {
            let msg = tokio::time::timeout(HANG_GUARD, self.file_rx.recv())
                .await
                .expect("hung waiting for a FileTail")
                .expect("the file queue stays open");
            let DaemonMessage::FileTail {
                data_b64,
                offset,
                reset,
                session_id,
                ..
            } = msg
            else {
                panic!("expected a FileTail, got {msg:?}");
            };
            assert_eq!(session_id, "s1");
            let data = base64::engine::general_purpose::STANDARD
                .decode(data_b64)
                .expect("valid base64");
            (data, offset, reset)
        }

        /// Wait until every follow has ended: with this side's sender gone,
        /// the file queue closes once the last follow drops its own.
        async fn follows_ended(mut self) {
            drop(self.file_tx);
            let ended = tokio::time::timeout(HANG_GUARD, async {
                while self.file_rx.recv().await.is_some() {}
            })
            .await;
            assert!(ended.is_ok(), "hung: a follow kept running");
        }

        fn refusal(&mut self) -> (String, Option<String>) {
            match self.out_rx.try_recv() {
                Ok(DaemonMessage::Error {
                    code, request_id, ..
                }) => (code.expect("a refusal carries a code"), request_id),
                other => panic!("expected a refusal, got {other:?}"),
            }
        }

        /// The next refusal's code and message.
        fn refusal_why(&mut self) -> (String, String) {
            match self.out_rx.try_recv() {
                Ok(DaemonMessage::Error { code, message, .. }) => {
                    (code.expect("a refusal carries a code"), message)
                }
                other => panic!("expected a refusal, got {other:?}"),
            }
        }
    }

    /// A hub holding session `s1` whose working folder is `<scratch>/work`,
    /// beside `<scratch>/outside/secret.log`.
    fn hub_with_folder(tag: &str) -> (Hub, ScratchDir, PathBuf) {
        let (hub, scratch) = test_hub(tag);
        let folder = scratch.path().join("work");
        let outside = scratch.path().join("outside");
        std::fs::create_dir_all(&folder).expect("create folder");
        std::fs::create_dir_all(&outside).expect("create outside");
        std::fs::write(outside.join("secret.log"), "secret").expect("write outside");
        let mut record = idle_record("s1");
        record.members = vec![SessionMember {
            repo_id: "r1".to_owned(),
            repo_name: "repo".to_owned(),
            branch: "main".to_owned(),
            worktree_path: folder.display().to_string(),
        }];
        hub.sessions.insert(record);
        (hub, scratch, folder)
    }

    fn append(path: &Path, bytes: &[u8]) {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .expect("open for append");
        file.write_all(bytes).expect("append");
    }

    #[tokio::test]
    async fn a_tail_sends_the_last_64k_then_new_bytes() {
        let (hub, _scratch, folder) = hub_with_folder("tail-last");
        let content: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(folder.join("build.log"), &content).expect("write log");
        let mut conn = Conn::new();
        conn.tail(&hub, "build.log").await;

        let (data, offset, reset) = conn.next_tail().await;
        assert_eq!(offset, 100_000 - 65_536);
        assert_eq!(data, content[100_000 - 65_536..]);
        assert!(!reset);

        append(&folder.join("build.log"), b"hello\n");
        let (data, offset, reset) = conn.next_tail().await;
        assert_eq!(
            (data.as_slice(), offset, reset),
            (&b"hello\n"[..], 100_000, false)
        );
        assert!(conn.out_rx.try_recv().is_err(), "a follow sends no reply");
    }

    #[tokio::test]
    async fn a_shrunk_file_resets_the_tail() {
        let (hub, _scratch, folder) = hub_with_folder("tail-shrink");
        let log = folder.join("run.log");
        std::fs::write(&log, "0123456789").expect("write log");
        let mut conn = Conn::new();
        conn.tail(&hub, "run.log").await;
        assert_eq!(conn.next_tail().await, (b"0123456789".to_vec(), 0, false));

        std::fs::write(&log, "abc").expect("shrink log");
        // The truncation and the write may be seen apart: what counts is a
        // reset, after which the held text is the new file.
        let mut held = Vec::new();
        let mut saw_reset = false;
        while !(saw_reset && held == b"abc") {
            let (data, offset, reset) = conn.next_tail().await;
            if reset {
                saw_reset = true;
                held.clear();
            }
            assert_eq!(offset, held.len() as u64);
            held.extend(data);
        }
    }

    #[tokio::test]
    async fn a_path_outside_the_sessions_folders_is_refused() {
        let (hub, scratch, _folder) = hub_with_folder("tail-outside");
        let mut conn = Conn::new();
        let absolute = scratch.path().join("outside").join("secret.log");
        for path in [
            absolute.display().to_string(),
            "../outside/secret.log".to_owned(),
        ] {
            conn.tail(&hub, &path).await;
            assert_eq!(
                conn.refusal(),
                ("invalid_request".to_owned(), Some("req".to_owned()))
            );
        }
        conn.send(
            &hub,
            ClientMessage::TailFile {
                session_id: "nobody".to_owned(),
                path: "a.log".to_owned(),
                request_id: None,
            },
        )
        .await;
        assert_eq!(conn.refusal(), ("not_found".to_owned(), None));
        assert_eq!(conn.tails.lock().await.len(), 0);
    }

    #[tokio::test]
    async fn a_symlink_out_of_the_folder_is_refused() {
        let (hub, scratch, folder) = hub_with_folder("tail-link");
        // A symlink needs developer mode or elevation on Windows and a
        // junction needs `cmd`; with neither there is no link to test.
        if !make_dir_link(&folder.join("link"), &scratch.path().join("outside")) {
            return;
        }
        let mut conn = Conn::new();
        conn.tail(&hub, "link/secret.log").await;
        assert_eq!(
            conn.refusal(),
            ("invalid_request".to_owned(), Some("req".to_owned()))
        );
        conn.tail(&hub, "link/not-yet.log").await;
        assert_eq!(
            conn.refusal().0,
            "invalid_request",
            "a missing file under the link is refused too"
        );
    }

    #[tokio::test]
    async fn a_missing_file_is_followed_once_it_appears() {
        let (hub, _scratch, folder) = hub_with_folder("tail-missing");
        let mut conn = Conn::new();
        conn.tail(&hub, "later/new.log").await;
        assert!(
            conn.out_rx.try_recv().is_err(),
            "a missing file is not refused"
        );

        std::fs::create_dir_all(folder.join("later")).expect("create folder");
        std::fs::write(folder.join("later").join("new.log"), "hi").expect("write log");
        assert_eq!(conn.next_tail().await, (b"hi".to_vec(), 0, false));
    }

    #[tokio::test]
    async fn untail_stops_the_follow() {
        let (hub, _scratch, folder) = hub_with_folder("tail-untail");
        std::fs::write(folder.join("a.log"), "a").expect("write log");
        let mut conn = Conn::new();
        conn.tail(&hub, "a.log").await;
        conn.next_tail().await;
        conn.send(
            &hub,
            ClientMessage::UntailFile {
                session_id: "s1".to_owned(),
                path: "a.log".to_owned(),
            },
        )
        .await;
        assert_eq!(conn.tails.lock().await.len(), 0);
        conn.follows_ended().await;
    }

    #[tokio::test]
    async fn a_dropped_connection_stops_its_follows() {
        let (hub, _scratch, folder) = hub_with_folder("tail-dropped");
        std::fs::write(folder.join("a.log"), "a").expect("write log");
        std::fs::write(folder.join("b.log"), "b").expect("write log");
        let mut conn = Conn::new();
        conn.tail(&hub, "a.log").await;
        conn.tail(&hub, "b.log").await;
        conn.next_tail().await;
        conn.next_tail().await;
        // What `client_session` runs as the connection goes.
        stop_transfers(&conn.lane, &conn.fetches, &conn.tails).await;
        conn.follows_ended().await;
    }

    #[tokio::test]
    async fn removing_the_session_stops_its_follows() {
        let (hub, _scratch, folder) = hub_with_folder("tail-removed");
        std::fs::write(folder.join("a.log"), "a").expect("write log");
        let mut conn = Conn::new();
        conn.tail(&hub, "a.log").await;
        conn.next_tail().await;
        hub.sessions.remove("s1");
        let tails = Arc::clone(&conn.tails);
        conn.follows_ended().await;
        // The task forgets its entry just after its follow ends.
        let forgotten = tokio::time::timeout(HANG_GUARD, async {
            while !tails.lock().await.is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(
            forgotten.is_ok(),
            "hung: the ended follow was never forgotten"
        );
    }

    #[tokio::test]
    async fn a_fifth_follow_is_refused() {
        let (hub, _scratch, folder) = hub_with_folder("tail-fifth");
        let mut conn = Conn::new();
        for name in ["1.log", "2.log", "3.log", "4.log", "5.log"] {
            std::fs::write(folder.join(name), name).expect("write log");
        }
        for name in ["1.log", "2.log", "3.log", "4.log", "1.log"] {
            conn.tail(&hub, name).await;
        }
        assert!(
            conn.out_rx.try_recv().is_err(),
            "four follows and a repeat are taken"
        );
        conn.tail(&hub, "5.log").await;
        assert_eq!(
            conn.refusal(),
            ("limit_reached".to_owned(), Some("req".to_owned()))
        );
        assert_eq!(conn.tails.lock().await.len(), 4);
    }

    /// Refuse `path` and return why, asserting the code is `invalid_request`.
    async fn refused_why(conn: &mut Conn, hub: &Hub, path: &str) -> String {
        conn.tail(hub, path).await;
        let (code, why) = conn.refusal_why();
        assert_eq!(code, "invalid_request", "{path:?}: {why}");
        why
    }

    #[tokio::test]
    async fn a_unc_path_is_refused_without_touching_the_disk() {
        let (hub, _scratch, _folder) = hub_with_folder("tail-unc");
        let mut conn = Conn::new();
        // The unroutable address last: reaching it on disk would block.
        for path in [
            r"\\?\C:\x.log",
            r"\\.\pipe\rt681\x.log",
            "//server/share/x.log",
            r"\\10.255.255.1\s\x.log",
        ] {
            let why = refused_why(&mut conn, &hub, path).await;
            assert!(why.contains("network or device path"), "{path:?}: {why}");
        }
    }

    #[tokio::test]
    async fn a_path_over_the_length_or_depth_cap_is_refused() {
        let (hub, _scratch, _folder) = hub_with_folder("tail-caps");
        let mut conn = Conn::new();
        let deep = format!("{}x.log", "a/".repeat(64));
        let why = refused_why(&mut conn, &hub, &deep).await;
        assert!(why.contains("more than 64"), "{why}");
        let long = format!("{}.log", "a".repeat(1021));
        let why = refused_why(&mut conn, &hub, &long).await;
        assert!(why.contains("longer than 1024"), "{why}");
        let at_caps = format!("{}x.log", "a/".repeat(63));
        conn.tail(&hub, &at_caps).await;
        assert!(conn.out_rx.try_recv().is_err(), "64 deep is taken");
    }

    #[tokio::test]
    async fn a_link_to_an_unresolvable_target_is_refused() {
        let (hub, scratch, folder) = hub_with_folder("tail-dangling");
        // No link without developer mode, elevation or `cmd`: nothing to test.
        if !make_dir_link(&folder.join("link"), &scratch.path().join("missing")) {
            return;
        }
        let mut conn = Conn::new();
        let why = refused_why(&mut conn, &hub, "link/x.log").await;
        assert!(why.contains("link"), "{why}");
    }

    #[tokio::test]
    async fn a_huge_gap_jumps_to_the_last_64k_with_reset() {
        let (hub, _scratch, folder) = hub_with_folder("tail-gap");
        let log = folder.join("big.log");
        std::fs::write(&log, "0123456789").expect("write log");
        let mut conn = Conn::new();
        conn.tail(&hub, "big.log").await;
        assert_eq!(conn.next_tail().await, (b"0123456789".to_vec(), 0, false));

        // One length change, as a preallocating writer makes it.
        let grown = 10 + 200_000u64;
        std::fs::OpenOptions::new()
            .write(true)
            .open(&log)
            .expect("open log")
            .set_len(grown)
            .expect("grow log");
        let (data, offset, reset) = conn.next_tail().await;
        assert_eq!((offset, reset), (grown - 65_536, true));
        assert_eq!(data, vec![0u8; 65_536]);
    }

    #[tokio::test]
    async fn a_restarted_follow_starts_with_reset() {
        let (hub, _scratch, folder) = hub_with_folder("tail-restart");
        std::fs::write(folder.join("a.log"), "abc").expect("write log");
        let mut conn = Conn::new();
        conn.tail(&hub, "a.log").await;
        assert_eq!(conn.next_tail().await, (b"abc".to_vec(), 0, false));
        conn.tail(&hub, "a.log").await;
        assert_eq!(conn.next_tail().await, (b"abc".to_vec(), 0, true));
    }

    #[tokio::test]
    async fn a_hung_open_does_not_hold_a_follow_slot() {
        let (hub, _scratch, folder) = hub_with_folder("tail-hung");
        for name in ["hung.log", "1.log", "2.log", "3.log", "4.log"] {
            std::fs::write(folder.join(name), name).expect("write log");
        }
        let hold = OpenHold::new("hung.log");
        let mut conn = Conn::new();
        conn.tail(&hub, "hung.log").await;
        tokio::time::timeout(HANG_GUARD, hold.entered())
            .await
            .expect("hung: the follow never opened its file");
        for name in ["1.log", "2.log", "3.log"] {
            conn.tail(&hub, name).await;
        }
        let untail = ClientMessage::UntailFile {
            session_id: "s1".to_owned(),
            path: "hung.log".to_owned(),
        };
        tokio::time::timeout(HANG_GUARD, conn.send(&hub, untail))
            .await
            .expect("hung: stopping the follow waited on its open");
        conn.tail(&hub, "4.log").await;
        assert!(
            conn.out_rx.try_recv().is_err(),
            "the stopped follow's slot is free"
        );
        let mut first = Vec::new();
        for _ in 0..4 {
            first.push(conn.next_tail().await.0);
        }
        first.sort();
        assert_eq!(first, [&b"1.log"[..], b"2.log", b"3.log", b"4.log"]);
        drop(hold);
    }

    #[tokio::test]
    async fn a_rerun_that_regrows_past_the_offset_is_a_reset() {
        let (hub, _scratch, folder) = hub_with_folder("tail-regrow");
        let log = folder.join("run.log");
        std::fs::write(&log, "0123456789").expect("write log");
        let mut conn = Conn::new();
        conn.tail(&hub, "run.log").await;
        assert_eq!(conn.next_tail().await, (b"0123456789".to_vec(), 0, false));

        // Rewritten in place and longer, never shorter than the offset.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&log)
            .expect("open log")
            .write_all(b"abcdefghijklmnop")
            .expect("rewrite log");
        assert_eq!(
            conn.next_tail().await,
            (b"abcdefghijklmnop".to_vec(), 0, true)
        );
    }
}
