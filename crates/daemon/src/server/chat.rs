//! The chat requests: `LoadChat` makes the connection a loader of a session's
//! chat and answers with the chat as it stands, and `UnloadChat` ends that;
//! `LoadSummary` and `UnloadSummary` do the same for the session's summary
//! lines (see [`crate::chat_mirror`]).

use super::Hub;
use super::connection::ConnCtx;
use crate::chat_mirror::LoadError;
use protocol::{ClientMessage, DaemonMessage};
use tracing::warn;

/// Handle the chat messages `dispatch` routes here.
pub(super) fn handle(hub: &Hub, msg: ClientMessage, ctx: &ConnCtx<'_>) {
    match msg {
        ClientMessage::LoadChat {
            session_id,
            request_id,
        } => {
            let loaded =
                hub.chat_mirror
                    .load(&session_id, request_id.clone(), ctx.connection, ctx.out_tx);
            if let Err(err) = loaded {
                refuse(ctx, err, request_id);
            }
        }
        ClientMessage::UnloadChat { session_id } => {
            hub.chat_mirror.unload(&session_id, ctx.connection);
        }
        ClientMessage::LoadSummary {
            session_id,
            request_id,
        } => {
            let loaded = hub.chat_mirror.load_summary(
                &session_id,
                request_id.clone(),
                ctx.connection,
                ctx.out_tx,
            );
            if let Err(err) = loaded {
                refuse(ctx, err, request_id);
            }
        }
        ClientMessage::UnloadSummary { session_id } => {
            hub.chat_mirror.unload_summary(&session_id, ctx.connection);
        }
        other => warn!(?other, "chat::handle: routed a message it does not own"),
    }
}

/// Answer a load that failed with `err`, carrying its `request_id`.
fn refuse(ctx: &ConnCtx<'_>, err: LoadError, request_id: Option<String>) {
    // A closed outbox is a connection already ending.
    let _ = ctx.out_tx.send(DaemonMessage::Error {
        message: err.message,
        code: Some(err.code.to_owned()),
        request_id,
    });
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert preconditions with expect and fail on an unexpected message"
)]
mod tests {
    use super::super::Hub;
    use super::super::connection::AuthKind;
    use super::super::test_support::{
        self, ScratchDir, dispatch_on, hello, idle_record, mem_session, test_hub,
    };
    use crate::chat_mirror::CHAT_REPLY_CAP;
    use crate::chat_mirror::test_lines::{answer, live_body, prompt, stamp};
    use crate::screen::fixture_screen::screen_from;
    use protocol::{
        ChatEntry, ChatPromptKind, ChatScreen, ClientMessage, DaemonMessage, SessionMode,
        SpawnConfig, SpawnTarget, StretchLine,
    };
    use std::collections::HashSet;
    use std::io::Write as _;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Barrier, Mutex};
    use std::time::Duration;
    use tokio::sync::mpsc;

    type Inbox = mpsc::UnboundedReceiver<DaemonMessage>;
    type Outbox = mpsc::UnboundedSender<DaemonMessage>;

    const QUESTION: &str = include_str!("../screen/fixtures/c31-ask-q1.txt");
    const PERMISSION: &str = include_str!("../screen/fixtures/c23-perm-bash.txt");

    /// A hub with one client connected, so tails read, holding Claude session
    /// `s1` in a scratch project folder whose Claude home is in the scratch
    /// dir too.
    struct Fixture {
        hub: Hub,
        folder: PathBuf,
        extra_env: Vec<(String, String)>,
        _scratch: ScratchDir,
    }

    impl Fixture {
        fn new(tag: &str, conversation: Option<&str>) -> Self {
            let (hub, scratch) = test_hub(tag);
            hub.client_count.send_replace(1);
            let folder = scratch.path().join("proj");
            std::fs::create_dir_all(&folder).expect("create the project folder");
            let home = scratch.path().join("claude-home");
            let extra_env = vec![("CLAUDE_CONFIG_DIR".to_owned(), home.display().to_string())];
            let mut record = idle_record("s1");
            record.conversation_id = conversation.map(str::to_owned);
            record.spawn_config = Some(SpawnConfig {
                target: SpawnTarget::Standalone {
                    cwd: Some(folder.display().to_string()),
                    add_dirs: Vec::new(),
                },
                mode: SessionMode::Interactive,
                dangerously_skip_permissions: false,
                agent: None,
                extra_env: extra_env.clone(),
            });
            hub.sessions.insert(record);
            let fixture = Self {
                hub,
                folder,
                extra_env,
                _scratch: scratch,
            };
            if let Some(conversation) = conversation {
                fixture.transcript(conversation);
            }
            fixture
        }

        /// The transcript of `conversation`, its folder made.
        fn transcript(&self, conversation: &str) -> PathBuf {
            let plugin = agent_registry::plugin(&agent_registry::default_agent())
                .expect("Claude is registered");
            let rows = crate::conversations::lookup_rows(&self.extra_env);
            let path = plugin
                .transcript_path(&self.folder, conversation, &rows)
                .expect("Claude keeps a transcript");
            std::fs::create_dir_all(path.parent().expect("a transcript has a folder"))
                .expect("create the project dir");
            path
        }

        fn append(&self, conversation: &str, lines: &[String]) {
            append_to(&self.transcript(conversation), lines);
        }

        async fn load(&self, connection: u64, out_tx: &Outbox, request_id: &str) {
            let msg = ClientMessage::LoadChat {
                session_id: "s1".to_owned(),
                request_id: Some(request_id.to_owned()),
            };
            dispatch_on(&self.hub, msg, connection, out_tx).await;
        }

        async fn unload(&self, connection: u64, out_tx: &Outbox) {
            let msg = ClientMessage::UnloadChat {
                session_id: "s1".to_owned(),
            };
            dispatch_on(&self.hub, msg, connection, out_tx).await;
        }

        async fn load_summary(&self, connection: u64, out_tx: &Outbox, request_id: &str) {
            let msg = ClientMessage::LoadSummary {
                session_id: "s1".to_owned(),
                request_id: Some(request_id.to_owned()),
            };
            dispatch_on(&self.hub, msg, connection, out_tx).await;
        }

        async fn unload_summary(&self, connection: u64, out_tx: &Outbox) {
            let msg = ClientMessage::UnloadSummary {
                session_id: "s1".to_owned(),
            };
            dispatch_on(&self.hub, msg, connection, out_tx).await;
        }

        fn deliver(&self, body: &serde_json::Value) {
            let record = self.hub.sessions.get("s1").expect("s1 is registered");
            crate::hook_status::deliver(&self.hub, "s1", &record, None, body);
        }

        /// Deliver `body` as a hook event stamped `time`, in nanoseconds
        /// since the epoch.
        fn deliver_at(&self, time: u128, body: &serde_json::Value) {
            let record = self.hub.sessions.get("s1").expect("s1 is registered");
            crate::hook_status::deliver(&self.hub, "s1", &record, Some(time), body);
        }
    }

    /// The next message that is not a `ChatWork`: every load's reply is
    /// followed by the strip, which the tests of other messages skip.
    async fn next_message(inbox: &mut Inbox) -> DaemonMessage {
        loop {
            let msg = test_support::next_message(inbox).await;
            if !matches!(msg, DaemonMessage::ChatWork { .. }) {
                return msg;
            }
        }
    }

    /// [`next_message`] with its own hang guard.
    async fn next_message_within(inbox: &mut Inbox, guard: Duration) -> DaemonMessage {
        loop {
            let msg = test_support::next_message_within(inbox, guard).await;
            if !matches!(msg, DaemonMessage::ChatWork { .. }) {
                return msg;
            }
        }
    }

    /// The messages waiting on `inbox` but the strips.
    fn drain(inbox: &mut Inbox) -> Vec<DaemonMessage> {
        test_support::drain(inbox)
            .into_iter()
            .filter(|msg| !matches!(msg, DaemonMessage::ChatWork { .. }))
            .collect()
    }

    /// A subagent's and a shell's ids, as a strip lists them.
    type WorkIds = (Vec<String>, Vec<String>);

    fn work_ids(msg: DaemonMessage) -> Option<WorkIds> {
        match msg {
            DaemonMessage::ChatWork {
                subagents, shells, ..
            } => Some((
                subagents.into_iter().map(|agent| agent.id).collect(),
                shells.into_iter().map(|shell| shell.id).collect(),
            )),
            _ => None,
        }
    }

    /// The next `ChatWork` on `inbox`, skipping every other message.
    async fn next_work(inbox: &mut Inbox) -> WorkIds {
        loop {
            if let Some(ids) = work_ids(test_support::next_message(inbox).await) {
                return ids;
            }
        }
    }

    /// The `ChatWork`s already waiting on `inbox`.
    fn waiting_work(inbox: &mut Inbox) -> Vec<WorkIds> {
        test_support::drain(inbox)
            .into_iter()
            .filter_map(work_ids)
            .collect()
    }

    /// The strips on `inbox` before the first message `last` matches, which
    /// it consumes.
    async fn strips_until(
        inbox: &mut Inbox,
        last: impl Fn(&DaemonMessage) -> bool,
    ) -> Vec<WorkIds> {
        let mut strips = Vec::new();
        loop {
            let msg = test_support::next_message(inbox).await;
            if last(&msg) {
                return strips;
            }
            strips.extend(work_ids(msg));
        }
    }

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|&id| id.to_owned()).collect()
    }

    /// The first hook time the strip tests stamp: 2026-09-21, in nanoseconds.
    const T0: u128 = 1_790_000_000_000_000_000;

    /// One second, in nanoseconds.
    const SECOND: u128 = 1_000_000_000;

    /// A main-agent `Stop` listing each `(id, type)` of `tasks` as running.
    fn stop_listing(tasks: &[(&str, &str)]) -> serde_json::Value {
        let tasks: Vec<serde_json::Value> = tasks
            .iter()
            .map(|(id, kind)| {
                serde_json::json!({
                    "id": id, "type": kind, "status": "running",
                    "description": format!("task {id}"), "command": "cargo test",
                })
            })
            .collect();
        serde_json::json!({
            "hook_event_name": "Stop", "background_tasks": tasks, "session_crons": [],
        })
    }

    /// The task notification `uuid` at second `n` that ends shell `task_id`.
    fn notification(uuid: &str, n: usize, task_id: &str) -> String {
        let text = format!(
            "<task-notification><task-id>{task_id}</task-id><tool-use-id>toolu_01</tool-use-id>\
             <output-file>o</output-file><status>completed</status>\
             <summary>Background command \"Run tests\" completed (exit code 0)</summary>\
             </task-notification>"
        );
        serde_json::json!({
            "type": "user", "uuid": uuid, "timestamp": stamp(n),
            "origin": {"kind": "task-notification"},
            "message": {"role": "user", "content": text},
        })
        .to_string()
    }

    fn append_to(path: &PathBuf, lines: &[String]) {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("open the transcript");
        for line in lines {
            writeln!(file, "{line}").expect("append a line");
        }
        file.flush().expect("flush the transcript");
    }

    fn user(n: usize) -> String {
        prompt(&format!("u-{n}"), n, None, &format!("line {n}"))
    }

    /// An answer record `uuid` at second `n` that calls tool `tool_id`.
    fn tool_call(uuid: &str, n: usize, tool_id: &str) -> String {
        serde_json::json!({
            "type": "assistant", "uuid": uuid, "timestamp": stamp(n),
            "message": {"id": format!("msg-{uuid}"), "role": "assistant", "type": "message",
                "content": [{"type": "tool_use", "id": tool_id, "name": "Bash",
                    "input": {"command": "ls"}}]},
        })
        .to_string()
    }

    /// A user record `uuid` at second `n` carrying tool `tool_id`'s result.
    fn tool_done(uuid: &str, n: usize, tool_id: &str) -> String {
        serde_json::json!({
            "type": "user", "uuid": uuid, "timestamp": stamp(n),
            "message": {"role": "user",
                "content": [{"type": "tool_result", "tool_use_id": tool_id, "content": "ok"}]},
        })
        .to_string()
    }

    fn id_of(entry: &ChatEntry) -> String {
        match entry {
            ChatEntry::User { id, .. }
            | ChatEntry::Answer { id, .. }
            | ChatEntry::Thinking { id, .. }
            | ChatEntry::Tool { id, .. }
            | ChatEntry::System { id, .. }
            | ChatEntry::Receipt { id, .. }
            | ChatEntry::TurnEnd { id, .. } => id.clone(),
            ChatEntry::Unknown => String::new(),
        }
    }

    /// The next message, which must be a `Chat`: its entries, hidden count,
    /// screen and request id.
    async fn next_chat(inbox: &mut Inbox) -> (Vec<ChatEntry>, u32, ChatScreen, Option<String>) {
        match next_message(inbox).await {
            DaemonMessage::Chat {
                entries,
                hidden,
                screen,
                request_id,
                ..
            } => (entries, hidden, screen, request_id),
            other => panic!("expected a Chat, got {other:?}"),
        }
    }

    /// The next message, which must be a `ChatAppend`: its entries and the
    /// ids it removes.
    async fn next_append(inbox: &mut Inbox) -> (Vec<ChatEntry>, Vec<String>) {
        match next_message(inbox).await {
            DaemonMessage::ChatAppend {
                entries, removed, ..
            } => (entries, removed),
            other => panic!("expected a ChatAppend, got {other:?}"),
        }
    }

    /// A passing test never reaches this: it fails only when the product is
    /// broken, so it is sized by the slow path it rules out.
    const HANG: Duration = Duration::from_secs(30);

    /// Apply one chat message to the entry ids a client shows, in order: a
    /// `Chat` replaces them, an append drops its removed ids and adds its
    /// entries.
    fn fold(ids: &mut Vec<String>, msg: DaemonMessage) {
        match msg {
            DaemonMessage::Chat { entries, .. } => *ids = entries.iter().map(id_of).collect(),
            DaemonMessage::ChatAppend {
                entries, removed, ..
            } => {
                ids.retain(|id| !removed.contains(id));
                ids.extend(entries.iter().map(id_of));
            }
            other => panic!("expected chat messages, got {other:?}"),
        }
    }

    /// The entry ids the loader shows once `last` arrives.
    async fn ids_until(inbox: &mut Inbox, last: &str) -> Vec<String> {
        let mut ids = Vec::new();
        while !ids.iter().any(|id| id == last) {
            fold(&mut ids, next_message(inbox).await);
        }
        ids
    }

    /// The next message, which must be a `Summary`: its lines and request id.
    async fn next_summary(inbox: &mut Inbox) -> (Vec<StretchLine>, Option<String>) {
        match next_message(inbox).await {
            DaemonMessage::Summary {
                lines, request_id, ..
            } => (lines, request_id),
            other => panic!("expected a Summary, got {other:?}"),
        }
    }

    /// The next message, which must be a `SummaryAppend`: its lines.
    async fn next_summary_append(inbox: &mut Inbox) -> Vec<StretchLine> {
        match next_message(inbox).await {
            DaemonMessage::SummaryAppend { lines, .. } => lines,
            other => panic!("expected a SummaryAppend, got {other:?}"),
        }
    }

    /// Apply one summary message to the lines a client shows, in order: a
    /// `Summary` replaces them, an append replaces a known line by id and
    /// adds the rest.
    fn fold_summary(shown: &mut Vec<StretchLine>, msg: DaemonMessage) {
        match msg {
            DaemonMessage::Summary { lines, .. } => *shown = lines,
            DaemonMessage::SummaryAppend { lines, .. } => {
                for line in lines {
                    match shown.iter_mut().find(|held| held.id == line.id) {
                        Some(held) => *held = line,
                        None => shown.push(line),
                    }
                }
            }
            other => panic!("expected summary messages, got {other:?}"),
        }
    }

    /// The lines a summary loader shows once the line `last` arrives.
    async fn summary_until(inbox: &mut Inbox, last: &str) -> Vec<StretchLine> {
        let mut shown = Vec::new();
        while !shown.iter().any(|line: &StretchLine| line.id == last) {
            fold_summary(&mut shown, next_message(inbox).await);
        }
        shown
    }

    fn line_ids(lines: &[StretchLine]) -> Vec<&str> {
        lines.iter().map(|line| line.id.as_str()).collect()
    }

    /// Wait until no live-line follower runs; an abort ends one at its task's
    /// next poll, not at once.
    async fn no_live_task_remains(hub: &Hub) {
        tokio::time::timeout(HANG, async {
            while hub.chat_mirror.live_task_count() > 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("hung: a live-line follower outlived its mirror");
    }

    #[tokio::test]
    async fn chat_work_reaches_loaders_once() {
        let fx = Fixture::new("chat-work-loaders", Some("conv-a"));
        let (chat_tx, mut chat_rx) = mpsc::unbounded_channel();
        let (summary_tx, mut summary_rx) = mpsc::unbounded_channel();
        let (both_tx, mut both_rx) = mpsc::unbounded_channel();
        fx.load(1, &chat_tx, "ra").await;
        fx.load_summary(2, &summary_tx, "rb").await;
        fx.load(3, &both_tx, "rc").await;
        fx.load_summary(3, &both_tx, "rc2").await;
        let empty: WorkIds = (Vec::new(), Vec::new());
        assert_eq!(
            next_work(&mut chat_rx).await,
            empty,
            "the chat load's strip"
        );
        assert_eq!(
            next_work(&mut summary_rx).await,
            empty,
            "the summary load's strip"
        );
        assert_eq!(
            next_work(&mut both_rx).await,
            empty,
            "the chat load's strip"
        );
        assert_eq!(
            next_work(&mut both_rx).await,
            empty,
            "the summary load's strip"
        );
        fx.deliver_at(T0, &stop_listing(&[("b1", "shell")]));
        for (name, inbox) in [
            ("chat", &mut chat_rx),
            ("summary", &mut summary_rx),
            ("both", &mut both_rx),
        ] {
            assert_eq!(next_work(inbox).await, (Vec::new(), ids(&["b1"])), "{name}");
            assert_eq!(waiting_work(inbox), Vec::<WorkIds>::new(), "{name}: once");
        }
    }

    #[tokio::test]
    async fn chat_work_is_sent_on_load() {
        let fx = Fixture::new("chat-work-on-load", Some("conv-a"));
        fx.deliver_at(T0, &stop_listing(&[("b1", "shell"), ("a1", "subagent")]));
        let (a_tx, mut a_rx) = mpsc::unbounded_channel();
        let (b_tx, mut b_rx) = mpsc::unbounded_channel();
        fx.load(1, &a_tx, "ra").await;
        let reply = test_support::next_message(&mut a_rx).await;
        assert!(
            matches!(reply, DaemonMessage::Chat { .. }),
            "the reply comes first, got {reply:?}"
        );
        let strip = work_ids(test_support::next_message(&mut a_rx).await);
        assert_eq!(strip, Some((ids(&["a1"]), ids(&["b1"]))), "then the strip");
        fx.load_summary(2, &b_tx, "rb").await;
        let reply = test_support::next_message(&mut b_rx).await;
        assert!(
            matches!(reply, DaemonMessage::Summary { .. }),
            "the reply comes first, got {reply:?}"
        );
        let strip = work_ids(test_support::next_message(&mut b_rx).await);
        assert_eq!(strip, Some((ids(&["a1"]), ids(&["b1"]))), "then the strip");
        assert_eq!(
            waiting_work(&mut a_rx),
            Vec::<WorkIds>::new(),
            "B's load sends A no strip"
        );
    }

    #[tokio::test]
    async fn a_task_notification_drops_its_task() {
        let fx = Fixture::new("chat-work-notification", Some("conv-a"));
        fx.append("conv-a", &[user(1)]);
        let (a_tx, mut a_rx) = mpsc::unbounded_channel();
        fx.load(1, &a_tx, "ra").await;
        ids_until(&mut a_rx, "u-1").await;
        fx.deliver_at(T0, &stop_listing(&[("b1", "shell"), ("a1", "subagent")]));
        assert_eq!(next_work(&mut a_rx).await, (ids(&["a1"]), ids(&["b1"])));
        fx.append("conv-a", &[notification("n-2", 2, "b1")]);
        assert_eq!(
            next_work(&mut a_rx).await,
            (ids(&["a1"]), Vec::new()),
            "the shell's notification ended it"
        );
    }

    #[tokio::test]
    async fn a_conversation_switch_keeps_the_ended_tasks() {
        let fx = Fixture::new("chat-work-switch", Some("conv-a"));
        fx.transcript("conv-b");
        let (a_tx, mut a_rx) = mpsc::unbounded_channel();
        fx.load(1, &a_tx, "ra").await;
        assert_eq!(next_work(&mut a_rx).await, (Vec::new(), Vec::new()));
        fx.deliver_at(T0, &stop_listing(&[("b1", "shell")]));
        assert_eq!(next_work(&mut a_rx).await, (Vec::new(), ids(&["b1"])));
        fx.append("conv-a", &[notification("n-1", 1, "b1")]);
        assert_eq!(
            next_work(&mut a_rx).await,
            (Vec::new(), Vec::new()),
            "the shell's notification ended it"
        );
        fx.hub.sessions.update("s1", |record| {
            record.conversation_id = Some("conv-b".to_owned());
        });
        let mut strips =
            strips_until(&mut a_rx, |msg| matches!(msg, DaemonMessage::Chat { .. })).await;
        // conv-b's line arrives after the strip its first read sent.
        fx.append("conv-b", &[user(2)]);
        strips.extend(
            strips_until(&mut a_rx, |msg| {
                matches!(msg, DaemonMessage::ChatAppend { entries, .. }
                    if entries.iter().any(|entry| id_of(entry) == "u-2"))
            })
            .await,
        );
        assert!(
            strips.iter().all(|(_, shells)| shells.is_empty()),
            "the switch brought b1 back: {strips:?}"
        );
    }

    #[tokio::test]
    async fn no_chat_work_when_nothing_changed() {
        let fx = Fixture::new("chat-work-unchanged", Some("conv-a"));
        let (a_tx, mut a_rx) = mpsc::unbounded_channel();
        fx.load(1, &a_tx, "ra").await;
        assert_eq!(next_work(&mut a_rx).await, (Vec::new(), Vec::new()));
        let shell_only = stop_listing(&[("b1", "shell")]);
        fx.deliver_at(T0, &shell_only);
        assert_eq!(next_work(&mut a_rx).await, (Vec::new(), ids(&["b1"])));
        fx.deliver_at(T0 + SECOND, &shell_only);
        fx.deliver_at(
            T0 + 2 * SECOND,
            &stop_listing(&[("b1", "shell"), ("a1", "subagent")]),
        );
        assert_eq!(
            next_work(&mut a_rx).await,
            (ids(&["a1"]), ids(&["b1"])),
            "the same Stop again sent no strip before the changed one"
        );
    }

    #[tokio::test]
    async fn appends_reach_loaders_only() {
        let fx = Fixture::new("chat-loaders-only", Some("conv-a"));
        fx.append("conv-a", &[user(1)]);
        let (a_tx, mut a_rx) = mpsc::unbounded_channel();
        let (b_tx, mut b_rx) = mpsc::unbounded_channel();
        fx.load(1, &a_tx, "ra").await;
        ids_until(&mut a_rx, "u-1").await;
        fx.append("conv-a", &[user(2)]);
        let (entries, removed) = next_append(&mut a_rx).await;
        assert_eq!(entries.iter().map(id_of).collect::<Vec<_>>(), ["u-2"]);
        assert_eq!(removed, Vec::<String>::new());
        // B, which never loaded, was sent nothing: its first message is its
        // own reply, holding both rows.
        fx.load(2, &b_tx, "rb").await;
        let (entries, hidden, _, request_id) = next_chat(&mut b_rx).await;
        assert_eq!(request_id.as_deref(), Some("rb"));
        assert_eq!(hidden, 0);
        assert_eq!(
            entries.iter().map(id_of).collect::<Vec<_>>(),
            ["u-1", "u-2"]
        );
        assert!(drain(&mut a_rx).is_empty(), "B's load sends A nothing");
    }

    #[tokio::test]
    async fn the_reply_carries_the_newest_entries_and_echoes_the_request_id() {
        let fx = Fixture::new("chat-cap", Some("conv-a"));
        let total = CHAT_REPLY_CAP + 3;
        let lines: Vec<String> = (1..=total).map(user).collect();
        fx.append("conv-a", &lines);
        let (a_tx, mut a_rx) = mpsc::unbounded_channel();
        fx.load(1, &a_tx, "ra").await;
        ids_until(&mut a_rx, &format!("u-{total}")).await;
        let (b_tx, mut b_rx) = mpsc::unbounded_channel();
        fx.load(2, &b_tx, "r-cap").await;
        let (entries, hidden, _, request_id) = next_chat(&mut b_rx).await;
        assert_eq!(request_id.as_deref(), Some("r-cap"));
        assert_eq!(entries.len(), CHAT_REPLY_CAP);
        assert_eq!(hidden, 3);
        assert_eq!(entries.first().map(id_of).as_deref(), Some("u-4"));
        assert_eq!(entries.last().map(id_of), Some(format!("u-{total}")));
    }

    #[tokio::test]
    async fn a_missing_transcript_yields_an_empty_chat_then_its_lines() {
        let fx = Fixture::new("chat-missing", Some("conv-a"));
        let (a_tx, mut a_rx) = mpsc::unbounded_channel();
        fx.load(1, &a_tx, "r-missing").await;
        let (entries, hidden, _, request_id) = next_chat(&mut a_rx).await;
        assert_eq!(entries, []);
        assert_eq!(hidden, 0);
        assert_eq!(request_id.as_deref(), Some("r-missing"));
        // The line arrives as an append, or in the first read's `Chat` when
        // the file appeared before that read.
        fx.append("conv-a", &[user(1)]);
        assert_eq!(ids_until(&mut a_rx, "u-1").await, ["u-1"]);
    }

    #[tokio::test]
    async fn an_unknown_session_is_refused_and_registers_nothing() {
        let fx = Fixture::new("chat-unknown", Some("conv-a"));
        let (a_tx, mut a_rx) = mpsc::unbounded_channel();
        let msg = ClientMessage::LoadChat {
            session_id: "nope".to_owned(),
            request_id: Some("r-x".to_owned()),
        };
        dispatch_on(&fx.hub, msg, 1, &a_tx).await;
        match next_message(&mut a_rx).await {
            DaemonMessage::Error {
                message,
                code,
                request_id,
            } => {
                assert_eq!(message, "unknown session: nope");
                assert_eq!(code.as_deref(), Some(protocol::error_codes::NOT_FOUND));
                assert_eq!(request_id.as_deref(), Some("r-x"));
            }
            other => panic!("expected an Error, got {other:?}"),
        }
        assert_eq!(fx.hub.chat_mirror.tail_count(), 0);
        assert_eq!(fx.hub.chat_mirror.mirror_count(), 0);
        assert_eq!(fx.hub.chat_mirror.live_task_count(), 0);
    }

    #[tokio::test]
    async fn a_session_removed_while_its_chat_loads_keeps_no_mirror() {
        let fx = Fixture::new("chat-removed-mid-load", Some("conv-a"));
        let sessions = Arc::clone(&fx.hub.sessions);
        fx.hub
            .chat_mirror
            .before_load_locks(move || sessions.remove("s1"));
        let (a_tx, _a_rx) = mpsc::unbounded_channel();
        fx.load(1, &a_tx, "ra").await;
        assert_eq!(
            fx.hub.chat_mirror.mirror_count(),
            0,
            "the removal was handled before the load registered its mirror"
        );
        no_live_task_remains(&fx.hub).await;
    }

    #[tokio::test]
    async fn the_last_unload_stops_the_tail() {
        let fx = Fixture::new("chat-unload", Some("conv-a"));
        let (a_tx, mut a_rx) = mpsc::unbounded_channel();
        let (b_tx, mut b_rx) = mpsc::unbounded_channel();
        fx.load(1, &a_tx, "ra").await;
        fx.load(2, &b_tx, "rb").await;
        next_chat(&mut a_rx).await;
        next_chat(&mut b_rx).await;
        assert_eq!(fx.hub.chat_mirror.tail_count(), 1);
        fx.unload(1, &a_tx).await;
        assert_eq!(fx.hub.chat_mirror.tail_count(), 1, "B still loads the chat");
        fx.append("conv-a", &[user(1)]);
        ids_until(&mut b_rx, "u-1").await;
        assert!(
            drain(&mut a_rx).is_empty(),
            "an unloaded connection is sent nothing"
        );
        fx.unload(2, &b_tx).await;
        assert_eq!(fx.hub.chat_mirror.tail_count(), 0);
        assert_eq!(fx.hub.chat_mirror.mirror_count(), 0);
        no_live_task_remains(&fx.hub).await;
    }

    #[tokio::test]
    async fn a_closed_connection_unloads_its_chats() {
        let fx = Fixture::new("chat-close", Some("conv-a"));
        let (mut client, session) = mem_session(&fx.hub, AuthKind::Local);
        client.send(&hello("test-token", None));
        client.send(&ClientMessage::LoadChat {
            session_id: "s1".to_owned(),
            request_id: Some("r-close".to_owned()),
        });
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(5), client.next(false))
                .await
                .expect("the chat reply hung")
                .expect("the connection stays open");
            if matches!(msg, DaemonMessage::Chat { .. }) {
                break;
            }
        }
        assert_eq!(fx.hub.chat_mirror.tail_count(), 1);
        drop(client);
        tokio::time::timeout(Duration::from_secs(5), session)
            .await
            .expect("the connection's end hung")
            .expect("the connection task ends cleanly");
        assert_eq!(fx.hub.chat_mirror.tail_count(), 0);
        assert_eq!(fx.hub.chat_mirror.mirror_count(), 0);
        no_live_task_remains(&fx.hub).await;
    }

    #[tokio::test]
    async fn the_first_read_reaches_the_first_loader_as_one_capped_chat() {
        let fx = Fixture::new("chat-first-read", Some("conv-a"));
        let total = CHAT_REPLY_CAP + 3;
        let lines: Vec<String> = (1..=total).map(user).collect();
        fx.append("conv-a", &lines);
        let (a_tx, mut a_rx) = mpsc::unbounded_channel();
        fx.load(1, &a_tx, "ra").await;
        let (entries, hidden, _, request_id) = next_chat(&mut a_rx).await;
        assert_eq!(request_id.as_deref(), Some("ra"));
        assert_eq!((entries.len(), hidden), (0, 0), "nothing is read yet");
        let (entries, hidden, _, request_id) = next_chat(&mut a_rx).await;
        assert_eq!(request_id, None, "the first read arrives as a reset");
        assert_eq!(entries.len(), CHAT_REPLY_CAP);
        assert_eq!(hidden, 3);
        assert_eq!(entries.last().map(id_of), Some(format!("u-{total}")));
        // The next message is the next line: no append went out per line of
        // the backlog.
        fx.append("conv-a", &[user(total + 1)]);
        let (entries, _) = next_append(&mut a_rx).await;
        assert_eq!(
            entries.iter().map(id_of).collect::<Vec<_>>(),
            [format!("u-{}", total + 1)]
        );
    }

    #[tokio::test]
    async fn a_dropped_mirrors_tail_never_writes_into_a_new_one() {
        let fx = Fixture::new("chat-epoch", Some("conv-a"));
        let (a_tx, mut a_rx) = mpsc::unbounded_channel();
        fx.load(1, &a_tx, "ra").await;
        next_chat(&mut a_rx).await;
        let old = fx.hub.chat_mirror.epoch("s1").expect("s1 is mirrored");
        fx.unload(1, &a_tx).await;
        fx.hub.sessions.update("s1", |record| {
            record.conversation_id = Some("conv-b".to_owned());
        });
        fx.transcript("conv-b");
        let (b_tx, mut b_rx) = mpsc::unbounded_channel();
        fx.load(2, &b_tx, "rb").await;
        // The old tail, stopped but still delivering the read it was in.
        fx.hub.chat_mirror.tail_line("s1", old, &user(1));
        fx.append("conv-b", &[user(2)]);
        assert_eq!(
            ids_until(&mut b_rx, "u-2").await,
            ["u-2"],
            "conv-a's line stays out of conv-b's chat"
        );
    }

    #[tokio::test]
    async fn a_live_message_after_a_mid_turn_load_shows_until_its_answer() {
        let fx = Fixture::new("chat-mid-turn", Some("conv-a"));
        fx.append(
            "conv-a",
            &[prompt("u-1", 1, Some("P1"), "go"), answer("a-1", 2, "One")],
        );
        let (a_tx, mut a_rx) = mpsc::unbounded_channel();
        fx.load(1, &a_tx, "ra").await;
        ids_until(&mut a_rx, "a-1").await;
        fx.deliver(&live_body("P1", "m2", 0, "Two"));
        let (entries, removed) = next_append(&mut a_rx).await;
        assert_eq!(entries.iter().map(id_of).collect::<Vec<_>>(), ["live:m2"]);
        assert_eq!(removed, Vec::<String>::new());
        fx.append("conv-a", &[answer("a-2", 3, "Two, in full")]);
        let (entries, removed) = next_append(&mut a_rx).await;
        assert_eq!(entries.iter().map(id_of).collect::<Vec<_>>(), ["a-2"]);
        assert_eq!(removed, ["live:m2"]);
    }

    #[tokio::test]
    async fn a_new_conversation_starts_the_chat_over_on_its_file() {
        let fx = Fixture::new("chat-clear", Some("conv-a"));
        fx.append("conv-a", &[user(1)]);
        fx.transcript("conv-b");
        let (a_tx, mut a_rx) = mpsc::unbounded_channel();
        fx.load(1, &a_tx, "ra").await;
        ids_until(&mut a_rx, "u-1").await;
        fx.hub.sessions.update("s1", |record| {
            record.conversation_id = Some("conv-b".to_owned());
        });
        let (entries, hidden, _, request_id) = next_chat(&mut a_rx).await;
        assert_eq!(request_id, None, "a reset is not a reply");
        assert_eq!(hidden, 0);
        assert_eq!(
            entries,
            [ChatEntry::System {
                id: "cleared:conv-b".to_owned(),
                text: "Conversation cleared".to_owned(),
            }]
        );
        fx.append("conv-a", &[user(2)]);
        fx.append("conv-b", &[user(3)]);
        let (entries, _) = next_append(&mut a_rx).await;
        assert_eq!(entries.iter().map(id_of).collect::<Vec<_>>(), ["u-3"]);
        assert!(
            drain(&mut a_rx).is_empty(),
            "the old file's line never arrives"
        );
    }

    #[tokio::test]
    async fn the_screen_yields_its_chat_screen_and_its_changes() {
        let fx = Fixture::new("chat-screen", None);
        fx.hub.sessions.update("s1", |record| {
            record.screen = Some(Arc::new(Mutex::new(screen_from(QUESTION))));
        });
        let (a_tx, mut a_rx) = mpsc::unbounded_channel();
        fx.load(1, &a_tx, "ra").await;
        let (_, _, screen, _) = next_chat(&mut a_rx).await;
        let question = screen.prompt.expect("the question is read");
        assert_eq!(question.kind, ChatPromptKind::Question);
        assert!(!question.id.is_empty(), "the prompt is given an id");

        let agent = agent_registry::default_agent();
        let moved = QUESTION.replacen("  ❯ 1. sub", "    1. sub", 1).replacen(
            "    2. div",
            "  ❯ 2. div",
            1,
        );
        let cursor_of = |text: &str| {
            crate::screen::dialogs::read(&agent, &screen_from(text))
                .and_then(|screen| screen.prompt)
                .and_then(|prompt| prompt.cursor)
        };
        assert_eq!((cursor_of(QUESTION), cursor_of(&moved)), (Some(1), Some(2)));
        let tap = fx.hub.chat_mirror.tap("s1", &agent);
        tap.observe(&screen_from(&moved));
        tap.observe(&screen_from(PERMISSION));
        match next_message(&mut a_rx).await {
            DaemonMessage::ChatScreenChanged { screen, .. } => {
                let permission = screen.prompt.expect("the permission is read");
                assert_eq!(permission.kind, ChatPromptKind::Permission);
                assert_ne!(permission.id, question.id, "a new prompt gets a new id");
            }
            other => panic!("expected a ChatScreenChanged, got {other:?}"),
        }
        assert!(
            drain(&mut a_rx).is_empty(),
            "the cursor's move sent nothing"
        );
    }

    #[tokio::test]
    async fn the_transcript_answer_replaces_its_live_entry_once() {
        let fx = Fixture::new("chat-live", Some("conv-a"));
        fx.append("conv-a", &[prompt("u-1", 1, Some("P1"), "go")]);
        let (a_tx, mut a_rx) = mpsc::unbounded_channel();
        fx.load(1, &a_tx, "ra").await;
        ids_until(&mut a_rx, "u-1").await;

        fx.deliver(&live_body("P1", "m1", 0, "Hel"));
        let (entries, removed) = next_append(&mut a_rx).await;
        assert_eq!(
            entries,
            [ChatEntry::Answer {
                id: "live:m1".to_owned(),
                text: "Hel".to_owned(),
                live: true,
            }]
        );
        assert_eq!(removed, Vec::<String>::new());

        fx.append("conv-a", &[answer("a-1", 2, "Hello")]);
        let (entries, removed) = next_append(&mut a_rx).await;
        assert_eq!(
            entries,
            [ChatEntry::Answer {
                id: "a-1".to_owned(),
                text: "Hello".to_owned(),
                live: false,
            }]
        );
        assert_eq!(removed, ["live:m1"]);

        // The late piece of m1 is dropped; m2, sent after it on the same
        // channel, is the next thing the loader hears.
        fx.deliver(&live_body("P1", "m1", 1, "lo"));
        fx.deliver(&live_body("P1", "m2", 0, "More"));
        let (entries, removed) = next_append(&mut a_rx).await;
        assert_eq!(entries.iter().map(id_of).collect::<Vec<_>>(), ["live:m2"]);
        assert_eq!(removed, Vec::<String>::new());
        assert!(drain(&mut a_rx).is_empty(), "nothing follows m2");
    }

    #[tokio::test]
    async fn a_replacement_outside_the_reply_window_is_not_sent() {
        let fx = Fixture::new("chat-replace-window", Some("conv-a"));
        let mut lines = vec![tool_call("c-old", 0, "t-old")];
        lines.extend((1..=CHAT_REPLY_CAP).map(user));
        lines.push(tool_call("c-new", CHAT_REPLY_CAP + 1, "t-new"));
        fx.append("conv-a", &lines);
        let (a_tx, mut a_rx) = mpsc::unbounded_channel();
        fx.load(1, &a_tx, "ra").await;
        let shown = ids_until(&mut a_rx, "t-new").await;
        assert!(
            !shown.iter().any(|id| id == "t-old"),
            "t-old is past the newest {CHAT_REPLY_CAP}"
        );
        fx.append(
            "conv-a",
            &[
                tool_done("r-old", CHAT_REPLY_CAP + 2, "t-old"),
                tool_done("r-new", CHAT_REPLY_CAP + 3, "t-new"),
            ],
        );
        let (entries, removed) = next_append(&mut a_rx).await;
        assert_eq!(
            entries.iter().map(id_of).collect::<Vec<_>>(),
            ["t-new"],
            "only the replacement inside the window is sent"
        );
        assert_eq!(removed, Vec::<String>::new());
    }

    #[tokio::test]
    async fn a_replacement_below_a_loaders_reply_is_not_sent_after_live_rows_end() {
        let fx = Fixture::new("chat-replace-floor", Some("conv-a"));
        // 500 entries, P1's prompt the last one typed.
        let mut lines = vec![tool_call("c-0", 0, "t-0"), tool_call("c-1", 1, "t-1")];
        lines.extend((2..CHAT_REPLY_CAP - 2).map(user));
        lines.push(prompt("u-p1", CHAT_REPLY_CAP - 2, Some("P1"), "go"));
        lines.push(tool_call("c-mark", CHAT_REPLY_CAP - 1, "t-mark"));
        fx.append("conv-a", &lines);
        let (a_tx, mut a_rx) = mpsc::unbounded_channel();
        fx.load(1, &a_tx, "ra").await;
        ids_until(&mut a_rx, "t-mark").await;
        fx.deliver(&live_body("P1", "m1", 0, "one"));
        fx.deliver(&live_body("P1", "m2", 0, "two"));
        ids_until(&mut a_rx, "live:m2").await;

        // B's reply leaves out the two oldest entries: t-0 and t-1.
        let (b_tx, mut b_rx) = mpsc::unbounded_channel();
        fx.load(2, &b_tx, "rb").await;
        let (entries, hidden, _, _) = next_chat(&mut b_rx).await;
        assert_eq!(hidden, 2);
        assert_eq!(entries.first().map(id_of).as_deref(), Some("u-2"));

        // P2 ends both live rows, so the chat is short enough again for t-1
        // to fit a reply, but B never held it.
        fx.append(
            "conv-a",
            &[
                prompt("u-p2", CHAT_REPLY_CAP, Some("P2"), "again"),
                tool_done("r-1", CHAT_REPLY_CAP + 1, "t-1"),
                tool_done("r-mark", CHAT_REPLY_CAP + 2, "t-mark"),
            ],
        );
        let (entries, removed) = next_append(&mut b_rx).await;
        assert_eq!(entries.iter().map(id_of).collect::<Vec<_>>(), ["u-p2"]);
        assert_eq!(removed, ["live:m1", "live:m2"]);
        let (entries, _) = next_append(&mut b_rx).await;
        assert_eq!(
            entries.iter().map(id_of).collect::<Vec<_>>(),
            ["t-mark"],
            "t-1 was never sent to B"
        );
    }

    #[tokio::test]
    async fn a_loaders_oldest_entry_is_replaced_past_a_long_open_live_row() {
        let fx = Fixture::new("chat-replace-oldest", Some("conv-a"));
        fx.append(
            "conv-a",
            &[
                tool_call("c-0", 0, "t-0"),
                prompt("u-p1", 1, Some("P1"), "go"),
            ],
        );
        let (a_tx, mut a_rx) = mpsc::unbounded_channel();
        fx.load(1, &a_tx, "ra").await;
        ids_until(&mut a_rx, "u-p1").await;
        fx.deliver(&live_body("P1", "m1", 0, "still going"));
        ids_until(&mut a_rx, "live:m1").await;

        // The live row stays open while a reply's worth of entries arrives.
        let last = CHAT_REPLY_CAP + 1;
        let calls: Vec<String> = (2..=last)
            .map(|n| tool_call(&format!("c-{n}"), n, &format!("t-{n}")))
            .collect();
        fx.append("conv-a", &calls);
        ids_until(&mut a_rx, &format!("t-{last}")).await;

        // t-{last}'s result follows, so a missing t-0 shows as the wrong append.
        fx.append(
            "conv-a",
            &[
                tool_done("r-0", last + 1, "t-0"),
                tool_done("r-last", last + 2, &format!("t-{last}")),
            ],
        );
        let (entries, _) = next_append(&mut a_rx).await;
        assert_eq!(
            entries.iter().map(id_of).collect::<Vec<_>>(),
            ["t-0"],
            "A has held t-0 since its first reset"
        );
    }

    #[tokio::test]
    async fn a_new_prompt_ends_the_live_answers_of_earlier_prompts() {
        let fx = Fixture::new("chat-live-ends", Some("conv-a"));
        fx.append("conv-a", &[prompt("u-1", 1, Some("P1"), "go")]);
        let (a_tx, mut a_rx) = mpsc::unbounded_channel();
        fx.load(1, &a_tx, "ra").await;
        ids_until(&mut a_rx, "u-1").await;
        fx.deliver(&live_body("P1", "m1", 0, "Hel"));
        let (entries, _) = next_append(&mut a_rx).await;
        assert_eq!(entries.iter().map(id_of).collect::<Vec<_>>(), ["live:m1"]);

        // P1's turn ended with no answer in the transcript (an Esc, a failed
        // turn); the next prompt takes its live answer away.
        fx.append("conv-a", &[prompt("u-2", 2, Some("P2"), "again")]);
        let (entries, removed) = next_append(&mut a_rx).await;
        assert_eq!(entries.iter().map(id_of).collect::<Vec<_>>(), ["u-2"]);
        assert_eq!(removed, ["live:m1"]);

        // A late piece of m1 is dropped; m2, sent after it, is the next thing
        // the loader hears.
        fx.deliver(&live_body("P1", "m1", 1, "lo"));
        fx.deliver(&live_body("P2", "m2", 0, "More"));
        let (entries, removed) = next_append(&mut a_rx).await;
        assert_eq!(entries.iter().map(id_of).collect::<Vec<_>>(), ["live:m2"]);
        assert_eq!(removed, Vec::<String>::new());
        assert!(drain(&mut a_rx).is_empty(), "nothing follows m2");
    }

    #[tokio::test]
    async fn a_lagging_live_follower_keeps_the_lines_its_channel_kept() {
        // More pieces than the live-line channel holds.
        const BURST: u64 = 300;
        let fx = Fixture::new("chat-live-lag", Some("conv-a"));
        fx.append("conv-a", &[prompt("u-1", 1, Some("P1"), "go")]);
        let (a_tx, mut a_rx) = mpsc::unbounded_channel();
        fx.load(1, &a_tx, "ra").await;
        ids_until(&mut a_rx, "u-1").await;
        fx.deliver(&live_body("P1", "m1", 0, "p0"));
        let (entries, _) = next_append(&mut a_rx).await;
        assert_eq!(entries.iter().map(id_of).collect::<Vec<_>>(), ["live:m1"]);

        // On this one-thread runtime the follower runs only when the test
        // awaits, so the whole burst lands before it reads any of it.
        for index in 1..=BURST {
            fx.deliver(&live_body("P1", "m1", index, &format!("p{index}")));
        }
        // The follower's first read is the lag, which resends the answers.
        let (entries, _) = next_append(&mut a_rx).await;
        let mut m1_text = match entries.as_slice() {
            [ChatEntry::Answer { id, text, .. }] if id == "live:m1" => text.clone(),
            other => panic!("expected m1 resent, got {other:?}"),
        };
        // m2 follows everything the channel kept.
        fx.deliver(&live_body("P1", "m2", 0, "marker"));
        let mut marker = false;
        while !marker {
            match next_message_within(&mut a_rx, HANG).await {
                DaemonMessage::ChatAppend { entries, .. } => {
                    for entry in entries {
                        match entry {
                            ChatEntry::Answer { id, text, .. } if id == "live:m1" => m1_text = text,
                            ChatEntry::Answer { id, .. } if id == "live:m2" => marker = true,
                            _ => {}
                        }
                    }
                }
                other => panic!("expected a ChatAppend, got {other:?}"),
            }
        }
        let pieces: Vec<u64> = m1_text
            .split('\n')
            .map(|piece| {
                piece
                    .trim_start_matches('p')
                    .parse()
                    .expect("a piece's index")
            })
            .collect();
        let (&first, kept) = pieces.split_first().expect("m1 has its first piece");
        assert_eq!(first, 0);
        let start = *kept.first().expect("the pieces after the lag are merged");
        assert!(
            start > 1,
            "the channel dropped the oldest pieces: {pieces:?}"
        );
        assert_eq!(
            kept,
            (start..=BURST).collect::<Vec<_>>(),
            "every piece the channel kept is merged"
        );
    }

    #[tokio::test]
    async fn load_summary_echoes_its_request_id() {
        let fx = Fixture::new("summary-reply", Some("conv-a"));
        fx.append("conv-a", &[user(1), tool_call("c-1", 2, "t-1"), user(3)]);
        let (a_tx, mut a_rx) = mpsc::unbounded_channel();
        fx.load_summary(1, &a_tx, "ra").await;
        summary_until(&mut a_rx, "u-3").await;
        let (b_tx, mut b_rx) = mpsc::unbounded_channel();
        fx.load_summary(2, &b_tx, "rb").await;
        let (lines, request_id) = next_summary(&mut b_rx).await;
        assert_eq!(request_id.as_deref(), Some("rb"));
        assert_eq!(line_ids(&lines), ["u-1", "u-3"]);
        assert_eq!(
            (lines[0].from_entry.as_str(), lines[0].to_entry.as_str()),
            ("u-1", "t-1")
        );
        assert_eq!(lines[0].text, "line 1 (<1m, 1 tool)");
        assert_eq!(lines[1].text, "line 3 (<1m)");
        assert!(drain(&mut a_rx).is_empty(), "B's load sends A nothing");
    }

    #[tokio::test]
    async fn summary_append_sends_only_changed_lines() {
        let fx = Fixture::new("summary-append", Some("conv-a"));
        fx.append("conv-a", &[user(1), user(2)]);
        let (a_tx, mut a_rx) = mpsc::unbounded_channel();
        fx.load_summary(1, &a_tx, "ra").await;
        summary_until(&mut a_rx, "u-2").await;
        fx.append("conv-a", &[tool_call("c-3", 3, "t-3")]);
        let lines = next_summary_append(&mut a_rx).await;
        assert_eq!(line_ids(&lines), ["u-2"], "u-1's line did not change");
        assert_eq!(lines[0].to_entry, "t-3");
        assert_eq!(lines[0].text, "line 2 (<1m, 1 tool)");
        assert!(drain(&mut a_rx).is_empty(), "one append per change");
    }

    #[tokio::test]
    async fn chat_loaders_get_no_summary_messages_and_back() {
        let fx = Fixture::new("summary-apart", Some("conv-a"));
        fx.append("conv-a", &[user(1)]);
        let (a_tx, mut a_rx) = mpsc::unbounded_channel();
        let (b_tx, mut b_rx) = mpsc::unbounded_channel();
        // Each fold fails on a message of the other kind.
        fx.load(1, &a_tx, "ra").await;
        ids_until(&mut a_rx, "u-1").await;
        fx.load_summary(2, &b_tx, "rb").await;
        summary_until(&mut b_rx, "u-1").await;
        fx.append("conv-a", &[user(2)]);
        let (entries, _) = next_append(&mut a_rx).await;
        assert_eq!(entries.iter().map(id_of).collect::<Vec<_>>(), ["u-2"]);
        assert_eq!(line_ids(&next_summary_append(&mut b_rx).await), ["u-2"]);
        assert!(
            drain(&mut a_rx).is_empty(),
            "the chat loader hears no summary"
        );
        assert!(
            drain(&mut b_rx).is_empty(),
            "the summary loader hears no chat"
        );
    }

    #[tokio::test]
    async fn the_mirror_lives_while_either_load_holds() {
        let fx = Fixture::new("summary-lifetime", Some("conv-a"));
        fx.append("conv-a", &[user(1)]);
        let (a_tx, mut a_rx) = mpsc::unbounded_channel();
        fx.load(1, &a_tx, "rc").await;
        fx.load_summary(1, &a_tx, "rs").await;
        let (mut ids, mut lines) = (Vec::new(), Vec::new());
        while !(ids.iter().any(|id| id == "u-1") && line_ids(&lines).contains(&"u-1")) {
            match next_message(&mut a_rx).await {
                msg @ (DaemonMessage::Chat { .. } | DaemonMessage::ChatAppend { .. }) => {
                    fold(&mut ids, msg);
                }
                msg => fold_summary(&mut lines, msg),
            }
        }
        fx.unload(1, &a_tx).await;
        assert_eq!(
            fx.hub.chat_mirror.mirror_count(),
            1,
            "the summary still holds the mirror"
        );
        fx.append("conv-a", &[user(2)]);
        assert_eq!(line_ids(&next_summary_append(&mut a_rx).await), ["u-2"]);
        assert!(
            drain(&mut a_rx).is_empty(),
            "the unloaded chat hears nothing"
        );
        fx.unload_summary(1, &a_tx).await;
        assert_eq!(fx.hub.chat_mirror.tail_count(), 0);
        assert_eq!(fx.hub.chat_mirror.mirror_count(), 0);
        no_live_task_remains(&fx.hub).await;
    }

    /// One thread delivers tail lines while another loads the chat again and
    /// again; each loader, folding its reply and appends in order, shows
    /// every line exactly once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_line_racing_a_load_is_sent_once() {
        const LOADERS: u64 = 20;
        // Under the reply cap, so a reply always carries the whole chat.
        const MOST_LINES: usize = 450;
        let fx = Fixture::new("chat-race", Some("conv-a"));
        fx.append("conv-a", &[user(0)]);
        let (a_tx, mut a_rx) = mpsc::unbounded_channel();
        fx.load(1, &a_tx, "ra").await;
        ids_until(&mut a_rx, "u-0").await;
        let mirror = fx.hub.chat_mirror.clone();
        let epoch = mirror.epoch("s1").expect("s1 is mirrored");
        let start = Arc::new(Barrier::new(2));
        let loading = Arc::new(AtomicBool::new(true));
        let writer = {
            let (mirror, start, loading) =
                (mirror.clone(), Arc::clone(&start), Arc::clone(&loading));
            tokio::task::spawn_blocking(move || {
                start.wait();
                let mut written = 0;
                loop {
                    written += 1;
                    mirror.tail_line("s1", epoch, &user(written));
                    if written == MOST_LINES || !loading.load(Ordering::SeqCst) {
                        return written;
                    }
                }
            })
        };
        let loader = tokio::task::spawn_blocking(move || {
            start.wait();
            let inboxes: Vec<Inbox> = (10..10 + LOADERS)
                .map(|connection| {
                    let (tx, rx) = mpsc::unbounded_channel();
                    mirror
                        .load("s1", Some("r-race".to_owned()), connection, &tx)
                        .expect("s1 loads");
                    rx
                })
                .collect();
            loading.store(false, Ordering::SeqCst);
            inboxes
        });
        let written = writer.await.expect("the writer finishes");
        let inboxes = loader.await.expect("the loads finish");
        let expected: HashSet<String> = (0..=written).map(|n| format!("u-{n}")).collect();
        for mut inbox in inboxes {
            let mut ids = Vec::new();
            for msg in drain(&mut inbox) {
                fold(&mut ids, msg);
            }
            let unique: HashSet<String> = ids.iter().cloned().collect();
            assert_eq!(unique.len(), ids.len(), "no line is sent twice");
            assert_eq!(unique, expected, "no line is missed");
        }
    }
}
