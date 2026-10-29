//! The Dashboard's messages: watching a Dashboard's facts and asking for one
//! session's detail.
use super::Hub;
use super::connection::ConnCtx;
use crate::sync::lock;
use protocol::{ClientMessage, DaemonMessage};
use tracing::warn;

/// Handle the Dashboard messages `dispatch` routes here.
pub(super) async fn handle(hub: &Hub, msg: ClientMessage, ctx: &ConnCtx<'_>) {
    match msg {
        ClientMessage::WatchDashboard { on: true } => {
            hub.dashboard
                .watch(ctx.connection, ctx.out_tx.clone())
                .await;
        }
        ClientMessage::WatchDashboard { on: false } => hub.dashboard.unwatch(ctx.connection),
        ClientMessage::GetSessionDetail {
            session_id,
            request_id,
        } => {
            // The reads behind the detail shell out to git: they run in a
            // task of their own, so a slow repository never holds this
            // connection's later messages.
            let hub = hub.clone();
            let out_tx = ctx.out_tx.clone();
            tokio::spawn(async move {
                let reply = session_detail(&hub, session_id, request_id).await;
                let _ = out_tx.send(reply);
            });
        }
        _ => warn!("dashboard::handle: a message that is not the Dashboard's was routed here"),
    }
}

/// One session's drawer detail: its first prompt as last read, and what its
/// worktrees did since it started. An unknown session is refused with the
/// request's id.
async fn session_detail(
    hub: &Hub,
    session_id: String,
    request_id: Option<String>,
) -> DaemonMessage {
    let Some(record) = hub.sessions.get(&session_id) else {
        return DaemonMessage::Error {
            message: format!("unknown session: {session_id}"),
            request_id,
        };
    };
    let (members, started_at, lines) = {
        let record = lock(&record);
        let lines = record
            .summary
            .as_ref()
            .map(|summary| summary.lines.clone())
            .unwrap_or_default();
        (record.members.clone(), record.started_at, lines)
    };
    let prompt = hub.dashboard.prompt(&session_id);
    let work = hub.dashboard.recent_work(&members, started_at).await;
    DaemonMessage::SessionDetail {
        prompt,
        session_id,
        lines,
        work,
        request_id,
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests assert preconditions with expect")]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;

    use agent_api::ActivitySnapshot;
    use chrono::Utc;
    use protocol::{
        ClientMessage, DaemonMessage, RecentWork, SessionFacts, SessionMember, WorkTotals,
    };
    use tokio::sync::{Mutex as AsyncMutex, mpsc};

    use crate::dashboard::test_support::Script;
    use crate::dashboard::{Dashboard, TICK};
    use crate::server::Hub;
    use crate::server::connection::{AuthKind, ConnCtx, FetchRegistry, PtyForwarders};
    use crate::server::dispatch;
    use crate::server::test_support::{
        ScratchDir, drain, git_init, git_ok, hello, idle_record, loopback_ws_client,
        next_message_within, test_hub, ws_next, ws_send,
    };

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

    async fn send(hub: &Hub, conn: &Conn, msg: ClientMessage) {
        let (file_tx, _file_rx) = mpsc::channel(1);
        let pty_forwarders: PtyForwarders = Arc::new(AsyncMutex::new(HashMap::new()));
        let fetches: FetchRegistry = Arc::new(AsyncMutex::new(HashMap::new()));
        let local = AuthKind::Local;
        let ctx = ConnCtx {
            out_tx: &conn.out_tx,
            file_tx: &file_tx,
            pty_forwarders: &pty_forwarders,
            fetches: &fetches,
            client_id: "c1",
            client_name: None,
            connection: conn.id,
            auth: &local,
        };
        dispatch(hub, msg, &ctx).await.expect("dispatch succeeds");
    }

    /// The facts of every `DashboardFacts` waiting on `conn`, one list per
    /// message.
    fn facts_messages(conn: &mut Conn) -> Vec<Vec<SessionFacts>> {
        drain(&mut conn.rx)
            .into_iter()
            .filter_map(|message| match message {
                DaemonMessage::DashboardFacts { facts, .. } => Some(facts),
                _ => None,
            })
            .collect()
    }

    fn ids(facts: &[SessionFacts]) -> Vec<&str> {
        facts
            .iter()
            .map(|facts| facts.session_id.as_str())
            .collect()
    }

    /// A hub whose Dashboard reads through a script.
    fn scripted_hub(tag: &str) -> (Hub, ScratchDir, Script) {
        let (mut hub, scratch) = test_hub(tag);
        let script = Script::default();
        hub.dashboard = Dashboard::with_reader_factory(Arc::clone(&hub.sessions), script.factory())
            .with_pr_program(crate::dashboard::pr::NO_PROGRAM);
        (hub, scratch, script)
    }

    /// A path that is not a repository: the session's detail carries no
    /// work for it.
    const NOT_A_REPO: &str = "C:/work/repo";

    /// Add listed session `id` on conversation `conversation`, its reading
    /// carrying `tokens_in`, its one member's worktree at `worktree`.
    fn add_session(
        hub: &Hub,
        script: &Script,
        id: &str,
        conversation: &str,
        tokens_in: u64,
        worktree: &str,
    ) {
        let mut record = idle_record(id);
        record.conversation_id = Some(conversation.to_owned());
        record.members = vec![SessionMember {
            repo_id: "r1".to_owned(),
            repo_name: "repo".to_owned(),
            branch: "main".to_owned(),
            worktree_path: worktree.to_owned(),
        }];
        hub.sessions.insert(record);
        script.set(conversation, tokens(tokens_in));
    }

    fn tokens(tokens_in: u64) -> ActivitySnapshot {
        ActivitySnapshot {
            tokens_in,
            ..ActivitySnapshot::default()
        }
    }

    const ON: ClientMessage = ClientMessage::WatchDashboard { on: true };
    const OFF: ClientMessage = ClientMessage::WatchDashboard { on: false };

    #[tokio::test]
    async fn a_new_watcher_gets_every_sessions_facts_at_once() {
        let (hub, _scratch, script) = scripted_hub("dash-at-once");
        add_session(&hub, &script, "s1", "conv1", 11, NOT_A_REPO);
        add_session(&hub, &script, "s2", "conv2", 22, NOT_A_REPO);
        let mut c1 = conn(1);
        send(&hub, &c1, ON).await;
        let messages = facts_messages(&mut c1);
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert_eq!(ids(&messages[0]), vec!["s1", "s2"]);
        let figures: Vec<u64> = messages[0].iter().map(|facts| facts.tokens_in).collect();
        assert_eq!(
            figures,
            vec![11, 22],
            "the first message carries a fresh reading"
        );
        assert_eq!(script.reads(), 2);
    }

    #[tokio::test]
    async fn a_new_watcher_with_no_session_is_told_there_is_none() {
        let (hub, _scratch, _script) = scripted_hub("dash-empty");
        let mut c1 = conn(1);
        send(&hub, &c1, ON).await;
        assert_eq!(facts_messages(&mut c1), vec![Vec::<SessionFacts>::new()]);
    }

    #[tokio::test]
    async fn facts_reach_only_watchers() {
        let (hub, _scratch, script) = scripted_hub("dash-only-watchers");
        add_session(&hub, &script, "s1", "conv1", 1, NOT_A_REPO);
        let mut watcher = conn(1);
        let mut other = conn(2);
        send(&hub, &watcher, ON).await;
        send(&hub, &other, OFF).await;
        script.set("conv1", tokens(2));
        hub.dashboard.tick(Utc::now()).await;
        let messages = facts_messages(&mut watcher);
        assert_eq!(messages.len(), 2, "{messages:?}");
        assert_eq!(messages[1][0].tokens_in, 2);
        assert!(
            drain(&mut other.rx).is_empty(),
            "a connection that never watched gets nothing"
        );
    }

    #[tokio::test]
    async fn only_changed_entries_follow() {
        let (hub, _scratch, script) = scripted_hub("dash-changed");
        add_session(&hub, &script, "s1", "conv1", 1, NOT_A_REPO);
        add_session(&hub, &script, "s2", "conv2", 2, NOT_A_REPO);
        let mut c1 = conn(1);
        send(&hub, &c1, ON).await;
        let _ = facts_messages(&mut c1);
        script.set("conv2", tokens(3));
        hub.dashboard.tick(Utc::now()).await;
        let messages = facts_messages(&mut c1);
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert_eq!(ids(&messages[0]), vec!["s2"]);
        hub.dashboard.tick(Utc::now()).await;
        assert!(
            facts_messages(&mut c1).is_empty(),
            "a tick that changed nothing sends nothing"
        );
    }

    #[tokio::test]
    async fn a_second_watcher_gets_the_whole_set_and_the_first_only_changes() {
        let (hub, _scratch, script) = scripted_hub("dash-second");
        add_session(&hub, &script, "s1", "conv1", 1, NOT_A_REPO);
        let mut c1 = conn(1);
        let mut c2 = conn(2);
        send(&hub, &c1, ON).await;
        let _ = facts_messages(&mut c1);
        send(&hub, &c2, ON).await;
        assert_eq!(facts_messages(&mut c2).len(), 1);
        assert!(
            facts_messages(&mut c1).is_empty(),
            "the first watcher saw no change"
        );
        assert_eq!(
            script.reads(),
            1,
            "a second watcher is served from the cache"
        );
    }

    #[tokio::test]
    async fn a_removed_session_leaves_the_facts() {
        let (hub, _scratch, script) = scripted_hub("dash-removed");
        add_session(&hub, &script, "s1", "conv1", 1, NOT_A_REPO);
        add_session(&hub, &script, "s2", "conv2", 2, NOT_A_REPO);
        let c1 = conn(1);
        send(&hub, &c1, ON).await;
        hub.sessions.remove("s1");
        hub.dashboard.tick(Utc::now()).await;
        let mut c2 = conn(2);
        send(&hub, &c2, ON).await;
        let messages = facts_messages(&mut c2);
        assert_eq!(ids(&messages[0]), vec!["s2"]);
        assert_eq!(hub.dashboard.prompt("s1"), None);
    }

    #[tokio::test]
    async fn the_cache_empties_when_the_last_watcher_leaves() {
        let (hub, _scratch, script) = scripted_hub("dash-cache-empties");
        add_session(&hub, &script, "s1", "conv1", 1, NOT_A_REPO);
        script.set(
            "conv1",
            ActivitySnapshot {
                first_prompt: Some("fix the login bug".to_owned()),
                ..ActivitySnapshot::default()
            },
        );
        let c1 = conn(1);
        send(&hub, &c1, ON).await;
        assert_eq!(
            hub.dashboard.prompt("s1").as_deref(),
            Some("fix the login bug")
        );
        send(&hub, &c1, OFF).await;
        assert_eq!(
            hub.dashboard.prompt("s1"),
            None,
            "an unwatch by the last watcher"
        );
        let gone = conn(2);
        send(&hub, &gone, ON).await;
        assert!(
            hub.dashboard.prompt("s1").is_some(),
            "the first watcher's pass rebuilds the cache"
        );
        drop(gone.rx);
        script.set("conv1", tokens(5));
        hub.dashboard.tick(Utc::now()).await;
        assert_eq!(
            hub.dashboard.prompt("s1"),
            None,
            "the last watcher's connection is gone"
        );
    }

    #[tokio::test]
    async fn a_new_conversation_id_builds_a_fresh_reader() {
        let (hub, _scratch, script) = scripted_hub("dash-new-conv");
        add_session(&hub, &script, "s1", "conv1", 1, NOT_A_REPO);
        let mut c1 = conn(1);
        send(&hub, &c1, ON).await;
        hub.dashboard.tick(Utc::now()).await;
        assert_eq!(script.builds(), 1, "a reader is kept between passes");
        script.set("conv9", tokens(9));
        hub.sessions.update("s1", |record| {
            record.conversation_id = Some("conv9".to_owned());
        });
        hub.dashboard.tick(Utc::now()).await;
        assert_eq!(script.builds(), 2);
        let messages = facts_messages(&mut c1);
        assert_eq!(messages.last().expect("a change was sent")[0].tokens_in, 9);
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_is_read_with_no_watcher() {
        let (hub, _scratch, script) = scripted_hub("dash-no-watcher");
        add_session(&hub, &script, "s1", "conv1", 1, NOT_A_REPO);
        let ticker = hub.dashboard.spawn_ticker();
        tokio::time::sleep(TICK * 10).await;
        hub.dashboard.tick(Utc::now()).await;
        assert_eq!(script.reads(), 0);
        let c1 = conn(1);
        send(&hub, &c1, ON).await;
        assert_eq!(script.reads(), 1, "the first watcher's own pass");
        tokio::time::sleep(TICK * 3).await;
        assert!(
            script.reads() >= 3,
            "the tick reads while watched: {}",
            script.reads()
        );
        ticker.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn unwatching_stops_the_tick() {
        let (hub, _scratch, script) = scripted_hub("dash-unwatch");
        add_session(&hub, &script, "s1", "conv1", 1, NOT_A_REPO);
        let ticker = hub.dashboard.spawn_ticker();
        let c1 = conn(1);
        send(&hub, &c1, ON).await;
        tokio::time::sleep(TICK * 3).await;
        send(&hub, &c1, OFF).await;
        // A pass already under way when the watch ended may still finish.
        tokio::time::sleep(TICK).await;
        let settled = script.reads();
        tokio::time::sleep(TICK * 10).await;
        assert_eq!(script.reads(), settled, "no pass runs with no watcher");
        ticker.abort();
    }

    #[tokio::test]
    async fn a_closed_connection_stops_watching() {
        let (hub, _scratch) = test_hub("dash-closed");
        let mut count = hub.dashboard.watcher_count();
        let mut client = loopback_ws_client(&hub).await;
        ws_send(&mut client, &hello(&hub.auth_token, None)).await;
        ws_send(&mut client, &ON).await;
        loop {
            let message = ws_next(&mut client)
                .await
                .expect("the daemon sends the Dashboard's facts before closing");
            if matches!(message, DaemonMessage::DashboardFacts { .. }) {
                break;
            }
        }
        assert_eq!(*count.borrow_and_update(), 1);
        drop(client);
        tokio::time::timeout(
            Duration::from_secs(120),
            count.wait_for(|watchers| *watchers == 0),
        )
        .await
        .expect("the closed connection stops watching")
        .expect("the dashboard is alive");
    }

    /// Ask for `session_id`'s detail and return the reply its task sends,
    /// waiting at most `guard`.
    async fn ask_detail(
        hub: &Hub,
        conn: &mut Conn,
        session_id: &str,
        guard: Duration,
    ) -> DaemonMessage {
        let ask = ClientMessage::GetSessionDetail {
            session_id: session_id.to_owned(),
            request_id: Some("d1".to_owned()),
        };
        send(hub, conn, ask).await;
        next_message_within(&mut conn.rx, guard).await
    }

    #[tokio::test]
    async fn get_session_detail_answers_with_the_known_fields() {
        let (hub, _scratch, script) = scripted_hub("dash-detail");
        add_session(&hub, &script, "s1", "conv1", 1, NOT_A_REPO);
        script.set(
            "conv1",
            ActivitySnapshot {
                first_prompt: Some("fix the login bug".to_owned()),
                ..ActivitySnapshot::default()
            },
        );
        let mut c1 = conn(1);
        send(&hub, &c1, ON).await;
        let _ = drain(&mut c1.rx);

        let reply = ask_detail(&hub, &mut c1, "s1", Duration::from_secs(5)).await;
        assert!(
            matches!(
                reply,
                DaemonMessage::SessionDetail { ref session_id, ref lines, prompt: Some(ref prompt), ref work, request_id: Some(ref id) }
                    if session_id == "s1" && lines.is_empty() && prompt == "fix the login bug"
                        && *work == RecentWork::default() && id == "d1"
            ),
            "{reply:?}"
        );

        let reply = ask_detail(&hub, &mut c1, "nope", Duration::from_secs(5)).await;
        assert!(
            matches!(
                reply,
                DaemonMessage::Error { ref message, request_id: Some(ref id) }
                    if message == "unknown session: nope" && id == "d1"
            ),
            "{reply:?}"
        );
    }

    #[tokio::test]
    async fn get_session_detail_carries_the_sessions_recent_work() {
        let (hub, scratch, script) = scripted_hub("dash-detail-work");
        let repo = scratch.path().join("repo");
        std::fs::create_dir_all(&repo).expect("create repo dir");
        git_init(&repo);
        git_ok(&repo, &["commit", "--allow-empty", "-m", "seed"]);
        add_session(
            &hub,
            &script,
            "s1",
            "conv1",
            1,
            repo.to_string_lossy().as_ref(),
        );
        // The session started before both commits, so both are its work.
        hub.sessions.update("s1", |record| {
            record.started_at = Utc::now() - chrono::Duration::hours(1);
        });
        std::fs::write(repo.join("work.txt"), "one\n").expect("write work file");
        git_ok(&repo, &["add", "--", "work.txt"]);
        git_ok(&repo, &["commit", "-m", "the first turn"]);

        let mut c1 = conn(1);
        // The read shells out to git under its own 10 s timeout: this wait is
        // a hang guard beyond that, not a deadline of its own.
        let reply = ask_detail(&hub, &mut c1, "s1", Duration::from_secs(60)).await;
        let work = match reply {
            DaemonMessage::SessionDetail { work, .. } => Some(work),
            _ => None,
        }
        .expect("a detail reply");
        let subjects: Vec<&str> = work.commits.iter().map(|c| c.subject.as_str()).collect();
        assert_eq!(subjects, vec!["the first turn", "seed"]);
        assert_eq!(work.files_total, 0, "the tree is clean");
        assert_eq!(
            work.landed,
            Some(WorkTotals {
                files: 1,
                added: 1,
                deleted: 0
            }),
            "the empty seed commit is the oldest commit since the start"
        );
    }
}
