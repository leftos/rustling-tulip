//! The chat requests: `LoadChat` makes the connection a loader of a session's
//! chat and answers with the chat as it stands, and `UnloadChat` ends that;
//! `LoadSummary` and `UnloadSummary` do the same for the session's summary
//! lines (see [`crate::chat_mirror`]). `ChatSend` types a message into the
//! session's terminal on a task of its own ([`crate::inject::type_message`]),
//! and `ChatAnswer` types the keys that answer its prompt the same way
//! ([`crate::chat_answer::answer`]); both answer only a refusal.

use std::sync::Arc;

use super::Hub;
use super::connection::ConnCtx;
use crate::chat_answer;
use crate::chat_mirror::LoadError;
use crate::inject::{TypeTarget, check_typed_text, type_message};
use crate::sync::lock;
use protocol::error_codes::{INVALID_REQUEST, NO_SCREEN, NOT_FOUND};
use protocol::{ChatAnswerKind, ClientMessage, DaemonMessage};
use tokio::sync::mpsc;
use tracing::{debug, warn};

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
                refuse_load(ctx, err, request_id);
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
                refuse_load(ctx, err, request_id);
            }
        }
        ClientMessage::UnloadSummary { session_id } => {
            hub.chat_mirror.unload_summary(&session_id, ctx.connection);
        }
        ClientMessage::ChatSend {
            session_id,
            text,
            request_id,
        } => send(hub, ctx, &session_id, &text, request_id),
        ClientMessage::ChatAnswer {
            session_id,
            prompt_id,
            answer: kind,
            request_id,
        } => answer(hub, ctx, &session_id, prompt_id, kind, request_id),
        other => warn!(?other, "chat::handle: routed a message it does not own"),
    }
}

/// Answer the prompt `prompt_id` on `session_id`'s screen with `kind` on a
/// task of its own, after the checks that need no waiting; a refusal, now or
/// from the task, is answered with an `Error` carrying `request_id`.
fn answer(
    hub: &Hub,
    ctx: &ConnCtx<'_>,
    session_id: &str,
    prompt_id: String,
    kind: ChatAnswerKind,
    request_id: Option<String>,
) {
    let target = match send_target(hub, session_id) {
        Ok(target) => target,
        Err((code, message)) => return refuse(ctx.out_tx, code, message, request_id),
    };
    let out_tx = ctx.out_tx.clone();
    tokio::spawn(async move {
        if let Err(refusal) = chat_answer::answer(&target, &prompt_id, &kind).await {
            debug!(
                session_id = %target.session_id,
                code = refusal.code,
                message = %refusal.message,
                "chat answer refused"
            );
            refuse(&out_tx, refusal.code, refusal.message, request_id);
        }
    });
}

/// Type `text` into `session_id`'s terminal on a task of its own, after the
/// checks that need no waiting; a refusal, now or from the task, is answered
/// with an `Error` carrying `request_id`.
fn send(hub: &Hub, ctx: &ConnCtx<'_>, session_id: &str, text: &str, request_id: Option<String>) {
    let target = match send_target(hub, session_id) {
        Ok(target) => target,
        Err((code, message)) => return refuse(ctx.out_tx, code, message, request_id),
    };
    let text = match check_typed_text(text) {
        Ok(text) => text,
        Err(why) => return refuse(ctx.out_tx, INVALID_REQUEST, why.to_owned(), request_id),
    };
    let out_tx = ctx.out_tx.clone();
    tokio::spawn(async move {
        if let Err(refusal) = type_message(&target, &text).await {
            debug!(
                session_id = %target.session_id,
                code = refusal.code,
                message = %refusal.message,
                "chat send refused"
            );
            refuse(&out_tx, refusal.code, refusal.message, request_id);
        }
    });
}

/// The terminal `session_id`'s message is typed into; the code and message
/// of the refusal when the session is unknown or has no terminal.
fn send_target(hub: &Hub, session_id: &str) -> Result<TypeTarget, (&'static str, String)> {
    let record = hub
        .sessions
        .get(session_id)
        .ok_or_else(|| (NOT_FOUND, format!("unknown session: {session_id}")))?;
    let (pty, screen, agent) = {
        let record = lock(&record);
        (
            record.pty.clone(),
            record.screen.clone(),
            record.agent.clone(),
        )
    };
    let (Some(pty), Some(screen)) = (pty, screen) else {
        return Err((NO_SCREEN, "the session has no terminal".to_owned()));
    };
    Ok(TypeTarget {
        session_id: session_id.to_owned(),
        agent,
        pty,
        screen,
        sessions: Arc::clone(&hub.sessions),
        mirror: hub.chat_mirror.clone(),
    })
}

/// Answer a load that failed with `err`, carrying its `request_id`.
fn refuse_load(ctx: &ConnCtx<'_>, err: LoadError, request_id: Option<String>) {
    refuse(ctx.out_tx, err.code, err.message, request_id);
}

/// Answer a request with an `Error` of `code` and `message`, carrying its
/// `request_id`.
fn refuse(
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
    code: &str,
    message: String,
    request_id: Option<String>,
) {
    // A closed outbox is a connection already ending.
    let _ = out_tx.send(DaemonMessage::Error {
        message,
        code: Some(code.to_owned()),
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
    use crate::summarizer::service::test_support::{FAKE_NOTIFICATION, FakeCaller};
    use protocol::{
        ChatEntry, ChatPromptKind, ChatScreen, ClientMessage, DaemonMessage, SessionMode,
        SpawnConfig, SpawnTarget, StretchLine, SummarizerSettings, SummarySource,
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

        /// [`Self::new`] with the chat mirror's lines written by a summarizer
        /// whose provider `caller` answers.
        fn with_summarizer(tag: &str, conversation: Option<&str>, caller: Arc<FakeCaller>) -> Self {
            let fixture = Self::new(tag, conversation);
            let summarizer = test_support::test_summarizer(
                &fixture.hub.state,
                &fixture.hub.dirs,
                &fixture.hub.client_count,
                &fixture.hub.state_events,
                caller,
            );
            fixture.hub.chat_mirror.set_summarizer(summarizer);
            fixture
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
            match next_message(&mut a_rx).await {
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

    /// Wait until no summary request is queued or running.
    async fn no_summary_job_remains(hub: &Hub) {
        tokio::time::timeout(HANG, async {
            while hub.chat_mirror.summary_jobs_pending() > 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("hung: a summary request never finished");
    }

    /// The lines a summary loader shows once line `id` comes from the model.
    async fn model_line_until(inbox: &mut Inbox, id: &str) -> Vec<StretchLine> {
        let mut shown = Vec::new();
        while !shown
            .iter()
            .any(|line: &StretchLine| line.id == id && line.source == SummarySource::Model)
        {
            fold_summary(&mut shown, next_message(inbox).await);
        }
        shown
    }

    fn line<'a>(lines: &'a [StretchLine], id: &str) -> &'a StretchLine {
        lines
            .iter()
            .find(|line| line.id == id)
            .expect("the line is shown")
    }

    /// Two turns, the first closed by the second.
    fn two_turns() -> Vec<String> {
        vec![user(1), answer("a-1", 2, "Fixed the login bug"), user(3)]
    }

    #[tokio::test]
    async fn a_closed_stretch_gets_its_model_line() {
        let fake = Arc::new(FakeCaller::default());
        let fx = Fixture::with_summarizer("summary-model", Some("conv-a"), Arc::clone(&fake));
        fx.append("conv-a", &two_turns());
        let (a_tx, mut a_rx) = mpsc::unbounded_channel();
        fx.load_summary(1, &a_tx, "ra").await;
        let shown = model_line_until(&mut a_rx, "u-1").await;
        assert_eq!(line(&shown, "u-1").text, FAKE_NOTIFICATION);
        assert_eq!(
            line(&shown, "u-3").source,
            SummarySource::Template,
            "the open stretch waits"
        );
        assert_eq!(fake.calls(), 1);
        let prompts = fake.prompts();
        assert!(
            prompts[0].contains("line 1\nFixed the login bug"),
            "the stretch's text is sent: {}",
            prompts[0]
        );
    }

    #[tokio::test]
    async fn an_unchanged_stretch_makes_no_call() {
        let fake = Arc::new(FakeCaller::default());
        let fx = Fixture::with_summarizer("summary-unchanged", Some("conv-a"), Arc::clone(&fake));
        fx.append("conv-a", &two_turns());
        let (a_tx, mut a_rx) = mpsc::unbounded_channel();
        fx.load_summary(1, &a_tx, "ra").await;
        model_line_until(&mut a_rx, "u-1").await;

        let (b_tx, mut b_rx) = mpsc::unbounded_channel();
        fx.load_summary(2, &b_tx, "rb").await;
        let (lines, _) = next_summary(&mut b_rx).await;
        assert_eq!(
            line(&lines, "u-1").text,
            FAKE_NOTIFICATION,
            "a new loader gets the model line"
        );

        fx.append("conv-a", &[answer("a-4", 4, "And the docs")]);
        assert_eq!(line_ids(&next_summary_append(&mut a_rx).await), ["u-3"]);
        no_summary_job_remains(&fx.hub).await;
        assert_eq!(fake.calls(), 1, "the closed stretch is not asked for again");
    }

    #[tokio::test]
    async fn a_failure_keeps_the_template() {
        let fake = Arc::new(FakeCaller::failing());
        let fx = Fixture::with_summarizer("summary-failure", Some("conv-a"), Arc::clone(&fake));
        fx.append("conv-a", &two_turns());
        let (a_tx, mut a_rx) = mpsc::unbounded_channel();
        fx.load_summary(1, &a_tx, "ra").await;
        let mut shown = summary_until(&mut a_rx, "u-3").await;
        no_summary_job_remains(&fx.hub).await;
        assert_eq!(fake.calls(), 1);

        fx.append("conv-a", &[answer("a-4", 4, "And the docs")]);
        fold_summary(&mut shown, next_message(&mut a_rx).await);
        no_summary_job_remains(&fx.hub).await;
        for msg in drain(&mut a_rx) {
            fold_summary(&mut shown, msg);
        }
        assert_eq!(line(&shown, "u-1").source, SummarySource::Template);
        assert_eq!(line(&shown, "u-1").text, "line 1 (<1m)");
        assert_eq!(
            fake.calls(),
            1,
            "a miss is not retried while its text stands"
        );
    }

    #[tokio::test]
    async fn the_cap_keeps_the_template() {
        let fake = Arc::new(FakeCaller::default());
        let fx = Fixture::with_summarizer("summary-cap", Some("conv-a"), Arc::clone(&fake));
        let settings = SummarizerSettings {
            daily_cap_usd: 0.0,
            ..fx.hub.state.summarizer_settings()
        };
        fx.hub
            .state
            .set_summarizer_settings(settings)
            .expect("the settings save");
        fx.append("conv-a", &two_turns());
        let (a_tx, mut a_rx) = mpsc::unbounded_channel();
        fx.load_summary(1, &a_tx, "ra").await;
        let mut shown = summary_until(&mut a_rx, "u-3").await;
        no_summary_job_remains(&fx.hub).await;
        for msg in drain(&mut a_rx) {
            fold_summary(&mut shown, msg);
        }
        assert_eq!(line(&shown, "u-1").source, SummarySource::Template);
        assert_eq!(fake.calls(), 0, "the cap makes no provider call");
    }

    /// Wait until the fake has been called `calls` times.
    async fn calls_reach(fake: &FakeCaller, calls: usize) {
        tokio::time::timeout(HANG, async {
            while fake.calls() < calls {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("hung: the summarizer was never called");
    }

    /// Let each gated call through until no summary request is queued or
    /// running.
    async fn release_until_idle(hub: &Hub, gate: &tokio::sync::Notify) {
        tokio::time::timeout(HANG, async {
            while hub.chat_mirror.summary_jobs_pending() > 0 {
                gate.notify_one();
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("hung: a summary request never finished");
    }

    #[tokio::test]
    async fn a_second_load_keeps_queued_requests() {
        let gate = Arc::new(tokio::sync::Notify::new());
        let fake = Arc::new(FakeCaller::gated(Arc::clone(&gate)));
        let fx = Fixture::with_summarizer("summary-requeue", Some("conv-a"), Arc::clone(&fake));
        fx.append("conv-a", &two_turns());
        let (a_tx, mut a_rx) = mpsc::unbounded_channel();
        fx.load_summary(1, &a_tx, "ra").await;
        let mut shown = summary_until(&mut a_rx, "u-3").await;
        calls_reach(&fake, 1).await;

        // u-1's call waits at the gate while 22 more turns queue u-3 and u-4
        // behind it, past the newest 20 closed stretches.
        let turns: Vec<String> = (4..=25).map(user).collect();
        fx.append("conv-a", &turns);
        while !line_ids(&shown).contains(&"u-25") {
            fold_summary(&mut shown, next_message(&mut a_rx).await);
        }
        let (b_tx, mut b_rx) = mpsc::unbounded_channel();
        fx.load_summary(2, &b_tx, "rb").await;
        next_summary(&mut b_rx).await;

        release_until_idle(&fx.hub, &gate).await;
        for msg in drain(&mut a_rx) {
            fold_summary(&mut shown, msg);
        }
        for id in ["u-1", "u-3", "u-4"] {
            assert_eq!(
                line(&shown, id).source,
                SummarySource::Model,
                "{id}'s queued request is kept"
            );
        }
    }

    #[tokio::test]
    async fn unloading_the_summary_stops_requests() {
        let gate = Arc::new(tokio::sync::Notify::new());
        let fake = Arc::new(FakeCaller::gated(Arc::clone(&gate)));
        let fx = Fixture::with_summarizer("summary-unload", Some("conv-a"), Arc::clone(&fake));
        let turns: Vec<String> = (1..=5).map(user).collect();
        fx.append("conv-a", &turns);
        let (a_tx, mut a_rx) = mpsc::unbounded_channel();
        fx.load(1, &a_tx, "rc").await;
        ids_until(&mut a_rx, "u-5").await;
        let (b_tx, mut b_rx) = mpsc::unbounded_channel();
        fx.load_summary(2, &b_tx, "rs").await;
        summary_until(&mut b_rx, "u-5").await;
        calls_reach(&fake, 1).await;

        fx.unload_summary(2, &b_tx).await;
        assert_eq!(
            fx.hub.chat_mirror.mirror_count(),
            1,
            "the chat still holds the mirror"
        );
        release_until_idle(&fx.hub, &gate).await;
        assert_eq!(fake.calls(), 1, "only the call in flight ran");
    }

    /// The lines reach the mirror through its tail hook, so no file is read
    /// and the paused clock's schedule is the only one.
    #[tokio::test(start_paused = true)]
    async fn the_open_stretch_waits_for_quiet() {
        let fake = Arc::new(FakeCaller::default());
        let fx = Fixture::with_summarizer("summary-quiet", None, Arc::clone(&fake));
        let (a_tx, mut a_rx) = mpsc::unbounded_channel();
        fx.load_summary(1, &a_tx, "ra").await;
        let started = tokio::time::Instant::now();
        let mirror = fx.hub.chat_mirror.clone();
        let epoch = mirror.epoch("s1").expect("s1 is mirrored");
        mirror.tail_line("s1", epoch, &user(1));
        mirror.tail_line("s1", epoch, &answer("a-1", 2, "Working on it"));
        drain(&mut a_rx);

        tokio::time::advance(Duration::from_secs(29)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(fake.calls(), 0, "no call before the stretch is quiet");
        assert!(drain(&mut a_rx).is_empty());

        let lines = next_summary_append(&mut a_rx).await;
        assert_eq!(line_ids(&lines), ["u-1"]);
        assert_eq!(lines[0].source, SummarySource::Model);
        assert_eq!(fake.calls(), 1);
        assert!(
            started.elapsed() >= Duration::from_secs(30),
            "the call waited for 30 s of quiet"
        );
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

    /// Chat sends typed into a recording PTY while the test plays the TUI:
    /// after each key it draws the captured frame that key brought up.
    mod send {
        use super::*;
        use crate::chat_mirror::test_lines::queued;
        use crate::history::test_support::{FakeExit, recording_pty};
        use crate::inject::enter_seam;
        use crate::pty::PtyHandle;
        use crate::screen::{Screen, SharedScreen};
        use crate::sync::lock;
        use crate::typing_lane::Holder;
        use protocol::PendingInput;
        use protocol::error_codes::{BUSY, CHAT_REFUSED};

        /// Main with the agents panel and a Heads up, the box empty and
        /// holding the keys.
        const V176: &str = include_str!("../screen/fixtures/v176-main-heads-up-box-keys.txt");
        /// Sleeper A's view with a Heads up for main.
        const V174: &str = include_str!("../screen/fixtures/v174-agent-view-heads-up-builtin.txt");
        /// Main in view, the panel holding the keys.
        const V175: &str = include_str!("../screen/fixtures/v175-main-heads-up-panel-keys.txt");
        /// Main while a turn runs, the box empty.
        const C32: &str = include_str!("../screen/fixtures/c32-ask-chat.txt");
        /// The model picker overlay.
        const V140: &str = include_str!("../screen/fixtures/v140-model-picker.txt");

        /// v174's mode line, under the box of Sleeper A's view.
        const V174_MODE_LINE: &str =
            "  ⏵⏵ auto mode on (shift+tab to cycle) · ← 3 agents · ↓ to manage";

        const DOWN: &[u8] = b"\x1b[B";
        const UP: &[u8] = b"\x1b[A";
        const ENTER: &[u8] = b"\r";

        /// Built from v174: the panel holds the keys with its cursor on
        /// `main`, the agent still in view. No capture shows this frame.
        fn v174_panel_on_main() -> String {
            assert!(V174.contains(V174_MODE_LINE) && V174.contains("  ◯ main"));
            V174.replacen(V174_MODE_LINE, "  ↑/↓ to select · Enter to view", 1)
                .replacen("  ◯ main", "❯ ◯ main", 1)
        }

        /// `capture` with `text` typed in its empty box: the paste shown, or
        /// a draft. Built: the captures that show text in the box are of a
        /// Claude Code whose rule over the box named the session, which reads
        /// as an agent in view.
        fn with_box(capture: &str, text: &str) -> String {
            let mut typed = 0;
            let rows: Vec<String> = capture
                .lines()
                .map(|row| {
                    if row.trim_end() == "❯" {
                        typed += 1;
                        format!("❯ {text}")
                    } else {
                        row.to_owned()
                    }
                })
                .collect();
            assert_eq!(typed, 1, "the capture shows one empty box");
            rows.join("\n")
        }

        /// The session's terminal, whose TUI the test plays.
        struct Terminal {
            pty: Arc<PtyHandle>,
            screen: SharedScreen,
            input: mpsc::UnboundedReceiver<Vec<u8>>,
            exit: FakeExit,
        }

        impl Terminal {
            /// Draw each of `captures` in turn and send them as one piece of
            /// output, the way the TUI paints and the watcher draws it.
            async fn show_all(&self, captures: &[&str]) {
                let bytes: Vec<u8> = captures
                    .iter()
                    .flat_map(|capture| self.draw(capture))
                    .collect();
                self.exit.chunk(bytes).await;
            }

            async fn show(&self, capture: &str) {
                self.show_all(&[capture]).await;
            }

            /// Draw `capture` whole on the grid, sized as `screen_from` sizes
            /// it; the bytes that draw it.
            fn draw(&self, capture: &str) -> Vec<u8> {
                draw_on(&self.screen, capture)
            }

            /// The next write typed into the terminal.
            async fn typed(&mut self) -> Vec<u8> {
                tokio::time::timeout(HANG, self.input.recv())
                    .await
                    .expect("hung: nothing was typed")
                    .expect("the PTY is open")
            }

            /// The writes typed and not yet read.
            fn typed_now(&mut self) -> Vec<Vec<u8>> {
                let mut writes = Vec::new();
                while let Ok(write) = self.input.try_recv() {
                    writes.push(write);
                }
                writes
            }

            /// Wait until no writer holds the typing lane.
            async fn lane_free(&self) {
                tokio::time::timeout(HANG, async {
                    while self.pty.typing().holder().is_some() {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await
                .expect("hung: the typing lane was never given back");
            }
        }

        /// Draw `capture` whole on `screen`, sized as `screen_from` sizes it;
        /// the bytes that draw it.
        fn draw_on(screen: &SharedScreen, capture: &str) -> Vec<u8> {
            let rows: Vec<&str> = capture.lines().collect();
            let widest = rows.iter().map(|row| visible_width(row)).max();
            let cols = u16::try_from(widest.unwrap_or(0) + 4).expect("a capture fits a grid");
            let height = u16::try_from(rows.len() + 2).expect("a capture fits a grid");
            let bytes = format!("\x1b[H\x1b[2J{}", rows.join("\r\n")).into_bytes();
            let mut screen = lock(screen);
            screen.resize(cols, height);
            screen.feed(&bytes);
            bytes
        }

        /// How many cells `row` shows, its escape sequences aside.
        fn visible_width(row: &str) -> usize {
            let mut width = 0;
            let mut chars = row.chars();
            while let Some(c) = chars.next() {
                if c == '\x1b' {
                    for c in chars.by_ref() {
                        if c.is_ascii_alphabetic() {
                            break;
                        }
                    }
                } else {
                    width += 1;
                }
            }
            width
        }

        impl Fixture {
            /// [`Fixture::new`] with `s1` given a recording PTY and a grid
            /// that shows `first`.
            fn with_terminal(
                tag: &str,
                conversation: Option<&str>,
                first: &str,
            ) -> (Self, Terminal) {
                let fixture = Self::new(tag, conversation);
                let (pty, input, exit) = recording_pty();
                let terminal = Terminal {
                    pty,
                    screen: Arc::new(Mutex::new(Screen::new(1, 1))),
                    input,
                    exit,
                };
                terminal.draw(first);
                fixture.hub.sessions.update("s1", |record| {
                    record.pty = Some(Arc::clone(&terminal.pty));
                    record.screen = Some(Arc::clone(&terminal.screen));
                });
                (fixture, terminal)
            }

            async fn send(&self, out_tx: &Outbox, text: &str) {
                let msg = ClientMessage::ChatSend {
                    session_id: "s1".to_owned(),
                    text: text.to_owned(),
                    request_id: Some("r-send".to_owned()),
                };
                dispatch_on(&self.hub, msg, 9, out_tx).await;
            }
        }

        fn paste(text: &str) -> Vec<u8> {
            format!("\x1b[200~{text}\x1b[201~").into_bytes()
        }

        /// Send `text` on main's empty box and play it through: the paste,
        /// the frame showing it, the Enter and the frame after it, until the
        /// send gives the lane back.
        async fn send_through(fx: &Fixture, term: &mut Terminal, out_tx: &Outbox, text: &str) {
            fx.send(out_tx, text).await;
            assert_eq!(term.typed().await, paste(text));
            term.show(&with_box(V176, text)).await;
            assert_eq!(term.typed().await, ENTER);
            term.show(V176).await;
            term.lane_free().await;
        }

        /// The code and message of `msg`, which must be the send's `Error`.
        fn refusal_of(msg: DaemonMessage) -> (String, String) {
            match msg {
                DaemonMessage::Error {
                    message,
                    code,
                    request_id,
                } => {
                    assert_eq!(request_id.as_deref(), Some("r-send"));
                    (code.unwrap_or_default(), message)
                }
                other => panic!("expected an Error, got {other:?}"),
            }
        }

        async fn next_refusal(inbox: &mut Inbox) -> (String, String) {
            refusal_of(next_message(inbox).await)
        }

        fn refused(code: &str, message: &str) -> (String, String) {
            (code.to_owned(), message.to_owned())
        }

        /// The pending entries the `ChatAppend`s waiting on `inbox` add: id
        /// and text.
        fn pending_added(inbox: &mut Inbox) -> Vec<(String, String)> {
            drain(inbox)
                .into_iter()
                .flat_map(|msg| match msg {
                    DaemonMessage::ChatAppend { entries, .. } => entries,
                    _ => Vec::new(),
                })
                .filter_map(|entry| match entry {
                    ChatEntry::User {
                        id,
                        text,
                        pending: true,
                    } => Some((id, text)),
                    _ => None,
                })
                .collect()
        }

        #[tokio::test]
        async fn a_ready_box_gets_the_paste_then_enter() {
            let (fx, mut term) = Fixture::with_terminal("send-ready", Some("conv-a"), V176);
            fx.append("conv-a", &[user(1)]);
            let (a_tx, mut a_rx) = mpsc::unbounded_channel();
            fx.load(1, &a_tx, "ra").await;
            ids_until(&mut a_rx, "u-1").await;
            let (s_tx, mut s_rx) = mpsc::unbounded_channel();
            fx.send(&s_tx, "hello there").await;
            assert_eq!(
                term.typed().await,
                paste("hello there"),
                "the message is one bracketed paste"
            );
            assert!(
                term.typed_now().is_empty(),
                "no Enter before a frame shows the paste"
            );
            assert!(
                pending_added(&mut a_rx).is_empty(),
                "no entry before the Enter"
            );
            term.show(&with_box(V176, "hello there")).await;
            assert_eq!(term.typed().await, ENTER);
            let added = pending_added(&mut a_rx);
            assert!(
                matches!(added.as_slice(), [(id, text)]
                    if id.starts_with("pending:") && text == "hello there"),
                "the pending entry reached the loader before the Enter: {added:?}"
            );
            term.show(V176).await;
            term.lane_free().await;
            assert!(drain(&mut s_rx).is_empty(), "the send was not refused");
        }

        #[tokio::test]
        async fn a_working_session_gets_it_too_and_the_entry_waits_for_its_record() {
            let (fx, mut term) = Fixture::with_terminal("send-working", Some("conv-a"), C32);
            fx.append("conv-a", &[user(1)]);
            let (a_tx, mut a_rx) = mpsc::unbounded_channel();
            fx.load(1, &a_tx, "ra").await;
            ids_until(&mut a_rx, "u-1").await;
            let (s_tx, _s_rx) = mpsc::unbounded_channel();
            fx.send(&s_tx, "and then this").await;
            assert_eq!(term.typed().await, paste("and then this"));
            term.show(&with_box(C32, "and then this")).await;
            assert_eq!(term.typed().await, ENTER, "the CLI queues it");
            let added = pending_added(&mut a_rx);
            let [(pending, _)] = added.as_slice() else {
                panic!("expected one pending entry, got {added:?}");
            };
            fx.append("conv-a", &[queued("q-2", Some(2), "and then this")]);
            let (entries, removed) = next_append(&mut a_rx).await;
            assert_eq!(entries.iter().map(id_of).collect::<Vec<_>>(), ["q-2"]);
            assert_eq!(
                removed,
                std::slice::from_ref(pending),
                "the record replaces the entry"
            );
        }

        #[tokio::test]
        async fn a_dialog_a_draft_and_an_overlay_type_nothing() {
            let draft = with_box(V176, "half a thought");
            let cases = [
                ("dialog", PERMISSION, "a dialog is open in the terminal"),
                ("draft", draft.as_str(), "a draft is in the terminal's box"),
                ("overlay", V140, "the terminal is not at its prompt"),
            ];
            for (name, frame, why) in cases {
                let (fx, mut term) = Fixture::with_terminal(&format!("send-{name}"), None, frame);
                let (s_tx, mut s_rx) = mpsc::unbounded_channel();
                fx.send(&s_tx, "hi").await;
                assert_eq!(
                    next_refusal(&mut s_rx).await,
                    refused(CHAT_REFUSED, why),
                    "{name}"
                );
                assert!(term.typed_now().is_empty(), "{name}: nothing is typed");
            }
        }

        #[tokio::test]
        async fn an_agent_in_view_returns_first() {
            let (fx, mut term) = Fixture::with_terminal("send-agent-view", None, V174);
            let (s_tx, mut s_rx) = mpsc::unbounded_channel();
            fx.send(&s_tx, "back in main").await;
            assert_eq!(term.typed().await, DOWN, "Down to the panel");
            term.show(&v174_panel_on_main()).await;
            assert_eq!(term.typed().await, ENTER, "Enter on main");
            term.show(V175).await;
            assert_eq!(term.typed().await, UP, "Up to the box");
            term.show(V176).await;
            assert_eq!(
                term.typed().await,
                paste("back in main"),
                "the paste comes once main is in view"
            );
            term.show(&with_box(V176, "back in main")).await;
            assert_eq!(term.typed().await, ENTER);
            term.show(V176).await;
            term.lane_free().await;
            assert!(drain(&mut s_rx).is_empty(), "the send was not refused");
        }

        #[tokio::test]
        async fn a_view_that_stays_on_the_agent_refuses_without_a_paste() {
            let (fx, mut term) = Fixture::with_terminal("send-stays", None, V174);
            let (s_tx, mut s_rx) = mpsc::unbounded_channel();
            fx.send(&s_tx, "hi").await;
            let mut keys = Vec::new();
            let refusal = loop {
                tokio::select! {
                    key = term.input.recv() => {
                        keys.push(key.expect("the PTY is open"));
                        term.show(V174).await;
                    }
                    msg = next_message(&mut s_rx) => break refusal_of(msg),
                }
            };
            assert_eq!(
                refusal,
                refused(
                    CHAT_REFUSED,
                    "the terminal could not return to the main conversation"
                )
            );
            keys.extend(term.typed_now());
            assert!(
                !keys.is_empty() && keys.iter().all(|key| key == DOWN),
                "only the drive's keys, no paste: {keys:?}"
            );
        }

        #[tokio::test]
        async fn no_enter_until_the_paste_shows() {
            let (fx, mut term) = Fixture::with_terminal("send-no-paste", None, V176);
            let (s_tx, mut s_rx) = mpsc::unbounded_channel();
            fx.send(&s_tx, "hi").await;
            assert_eq!(term.typed().await, paste("hi"));
            let refusal = tokio::time::timeout(HANG, async {
                loop {
                    term.show(V176).await;
                    let next = tokio::time::timeout(Duration::from_millis(100), s_rx.recv());
                    if let Ok(msg) = next.await {
                        return msg.expect("the outbox is open");
                    }
                }
            })
            .await
            .expect("hung: the send was never refused");
            assert_eq!(
                refusal_of(refusal),
                refused(
                    CHAT_REFUSED,
                    "the message did not appear in the box; it may be left there"
                )
            );
            assert!(term.typed_now().is_empty(), "no Enter");
        }

        #[tokio::test]
        async fn a_dialog_drawn_before_the_enter_stops_it() {
            let (fx, mut term) = Fixture::with_terminal("send-late-dialog", None, V176);
            let (s_tx, mut s_rx) = mpsc::unbounded_channel();
            let screen = Arc::clone(&term.screen);
            enter_seam::set(&term.pty, move || {
                draw_on(&screen, PERMISSION);
            });
            fx.send(&s_tx, "hi").await;
            assert_eq!(term.typed().await, paste("hi"));
            term.show(&with_box(V176, "hi")).await;
            assert_eq!(
                next_refusal(&mut s_rx).await,
                refused(
                    CHAT_REFUSED,
                    "a dialog is open in the terminal; the message was left in the box"
                ),
                "the paste showed, then the dialog came up before the Enter"
            );
            assert!(term.typed_now().is_empty(), "no Enter");
        }

        #[tokio::test]
        async fn a_prompt_raised_before_the_enter_stops_it() {
            let (fx, mut term) = Fixture::with_terminal("send-late-prompt", None, V176);
            let (s_tx, mut s_rx) = mpsc::unbounded_channel();
            let sessions = Arc::clone(&fx.hub.sessions);
            enter_seam::set(&term.pty, move || {
                sessions.update("s1", |record| {
                    record.pending_input = Some(PendingInput::Permission {
                        tool_name: "Bash".to_owned(),
                        summary: "ls".to_owned(),
                    });
                });
            });
            fx.send(&s_tx, "hi").await;
            assert_eq!(term.typed().await, paste("hi"));
            term.show(&with_box(V176, "hi")).await;
            assert_eq!(
                next_refusal(&mut s_rx).await,
                refused(
                    CHAT_REFUSED,
                    "Claude is waiting for an answer in the terminal; the message was left in the box"
                )
            );
            assert!(term.typed_now().is_empty(), "no Enter");
        }

        #[tokio::test]
        async fn a_user_draft_that_starts_like_the_placeholder_refuses() {
            let draft = with_box(V176, "Try \"npm test\" again");
            let (fx, mut term) = Fixture::with_terminal("send-try-draft", None, &draft);
            let (s_tx, mut s_rx) = mpsc::unbounded_channel();
            fx.send(&s_tx, "hi").await;
            assert_eq!(
                next_refusal(&mut s_rx).await,
                refused(CHAT_REFUSED, "a draft is in the terminal's box")
            );
            assert!(term.typed_now().is_empty(), "nothing is typed");
        }

        #[tokio::test]
        async fn a_message_that_starts_like_the_placeholder_is_sent() {
            let (fx, mut term) = Fixture::with_terminal("send-try-text", None, V176);
            let (s_tx, mut s_rx) = mpsc::unbounded_channel();
            send_through(&fx, &mut term, &s_tx, "Try \"npm test\" again").await;
            assert!(drain(&mut s_rx).is_empty(), "the send was not refused");
        }

        #[tokio::test]
        async fn a_message_shaped_like_the_placeholder_never_shows() {
            let text = "Try \"npm test\"";
            let (fx, mut term) = Fixture::with_terminal("send-try-whole", None, V176);
            let (s_tx, mut s_rx) = mpsc::unbounded_channel();
            fx.send(&s_tx, text).await;
            assert_eq!(term.typed().await, paste(text));
            let refusal = tokio::time::timeout(HANG, async {
                loop {
                    term.show(&with_box(V176, text)).await;
                    let next = tokio::time::timeout(Duration::from_millis(100), s_rx.recv());
                    if let Ok(msg) = next.await {
                        return msg.expect("the outbox is open");
                    }
                }
            })
            .await
            .expect("hung: the send was never refused");
            assert_eq!(
                refusal_of(refusal),
                refused(
                    CHAT_REFUSED,
                    "the message did not appear in the box; it may be left there"
                ),
                "the box reads as its placeholder"
            );
            assert!(term.typed_now().is_empty(), "no Enter");
        }

        #[tokio::test]
        async fn a_draft_with_a_blank_first_row_refuses_in_an_agents_view() {
            let placeholder = "❯\u{a0}Message @general-purpose…";
            assert!(V174.contains(placeholder));
            let draft = V174.replacen(placeholder, "❯\n  the rest of a draft", 1);
            let (fx, mut term) = Fixture::with_terminal("send-blank-first-row", None, &draft);
            let (s_tx, mut s_rx) = mpsc::unbounded_channel();
            fx.send(&s_tx, "hi").await;
            assert_eq!(
                next_refusal(&mut s_rx).await,
                refused(CHAT_REFUSED, "a draft is in the terminal's box")
            );
            assert!(term.typed_now().is_empty(), "nothing is typed");
        }

        #[tokio::test]
        async fn a_command_adds_no_pending_entry() {
            let (fx, mut term) = Fixture::with_terminal("send-command", Some("conv-a"), V176);
            fx.append("conv-a", &[user(1)]);
            let (a_tx, mut a_rx) = mpsc::unbounded_channel();
            fx.load(1, &a_tx, "ra").await;
            ids_until(&mut a_rx, "u-1").await;
            let (s_tx, mut s_rx) = mpsc::unbounded_channel();
            send_through(&fx, &mut term, &s_tx, "/compact").await;
            send_through(&fx, &mut term, &s_tx, " !ls").await;
            assert!(drain(&mut s_rx).is_empty(), "neither send was refused");
            assert_eq!(
                pending_added(&mut a_rx),
                Vec::<(String, String)>::new(),
                "a command's record never becomes a user entry"
            );
        }

        #[tokio::test]
        async fn an_ended_agent_refuses() {
            type End = fn(&mut crate::session::SessionRecord);
            let ended: [(&str, End); 2] = [
                ("announced", |record| record.agent_exit_announced = true),
                ("shell-named", |record| {
                    record.program_name = record.shell_label.clone();
                }),
            ];
            for (name, end) in ended {
                let (fx, mut term) =
                    Fixture::with_terminal(&format!("send-ended-{name}"), None, V176);
                fx.hub.sessions.update("s1", |record| {
                    record.agent_in_shell = true;
                    record.shell_label = Some("pwsh".to_owned());
                    end(record);
                });
                let (s_tx, mut s_rx) = mpsc::unbounded_channel();
                fx.send(&s_tx, "hi").await;
                assert_eq!(
                    next_refusal(&mut s_rx).await,
                    refused(CHAT_REFUSED, "the agent is not running in this session"),
                    "{name}"
                );
                assert!(term.typed_now().is_empty(), "{name}: nothing is typed");
            }
        }

        #[tokio::test]
        async fn an_owner_key_mid_send_stops_it() {
            let (fx, mut term) = Fixture::with_terminal("send-owner-key", None, V176);
            let (s_tx, mut s_rx) = mpsc::unbounded_channel();
            fx.send(&s_tx, "hi").await;
            assert_eq!(term.typed().await, paste("hi"));
            term.pty.typing().owner_typed();
            term.show(&with_box(V176, "hi")).await;
            assert_eq!(
                next_refusal(&mut s_rx).await,
                refused(
                    CHAT_REFUSED,
                    "someone typed in the terminal; the message was left in the box"
                )
            );
            assert!(term.typed_now().is_empty(), "no Enter");
        }

        #[tokio::test]
        async fn a_pending_prompt_refuses() {
            let (fx, mut term) = Fixture::with_terminal("send-pending-prompt", None, V176);
            fx.hub.sessions.update("s1", |record| {
                record.pending_input = Some(PendingInput::Permission {
                    tool_name: "Bash".to_owned(),
                    summary: "ls".to_owned(),
                });
            });
            let (s_tx, mut s_rx) = mpsc::unbounded_channel();
            fx.send(&s_tx, "hi").await;
            assert_eq!(
                next_refusal(&mut s_rx).await,
                refused(
                    CHAT_REFUSED,
                    "Claude is waiting for an answer in the terminal"
                )
            );
            assert!(term.typed_now().is_empty(), "nothing is typed");
        }

        /// On a paused clock, so the refusal racing a 1 ms timer shows by the
        /// order of events whether the send waited for the lane: no timer
        /// fires while a task can still run.
        #[tokio::test(start_paused = true)]
        async fn a_second_send_is_busy() {
            let (fx, mut term) = Fixture::with_terminal("send-busy", None, V176);
            let _held = term
                .pty
                .typing()
                .try_take(Holder::ChatSend)
                .expect("a free lane");
            let (s_tx, mut s_rx) = mpsc::unbounded_channel();
            fx.send(&s_tx, "hi").await;
            let first = tokio::select! {
                biased;
                msg = s_rx.recv() => msg,
                () = tokio::time::sleep(Duration::from_millis(1)) => None,
            };
            let refusal = first.expect("the send was refused before any wait for the lane");
            assert_eq!(
                refusal_of(refusal),
                refused(BUSY, "another message is being typed")
            );
            assert!(term.typed_now().is_empty(), "nothing is typed");
        }

        #[tokio::test]
        async fn a_send_waits_out_a_view_switch_then_refuses_busy() {
            let (fx, mut term) = Fixture::with_terminal("send-busy-view", None, V176);
            let _held = term
                .pty
                .typing()
                .try_take(Holder::ViewReturn)
                .expect("a free lane");
            let (s_tx, mut s_rx) = mpsc::unbounded_channel();
            fx.send(&s_tx, "hi").await;
            assert_eq!(
                next_refusal(&mut s_rx).await,
                refused(BUSY, "the terminal is switching views")
            );
            assert!(term.typed_now().is_empty(), "nothing is typed");
        }

        #[tokio::test]
        async fn two_identical_sends_clear_in_order() {
            let (fx, mut term) = Fixture::with_terminal("send-twice", Some("conv-a"), V176);
            fx.append("conv-a", &[user(1)]);
            let (a_tx, mut a_rx) = mpsc::unbounded_channel();
            fx.load(1, &a_tx, "ra").await;
            ids_until(&mut a_rx, "u-1").await;
            let (s_tx, mut s_rx) = mpsc::unbounded_channel();
            send_through(&fx, &mut term, &s_tx, "same").await;
            send_through(&fx, &mut term, &s_tx, "same").await;
            assert!(drain(&mut s_rx).is_empty(), "neither send was refused");
            let pending: Vec<String> = pending_added(&mut a_rx)
                .into_iter()
                .map(|(id, _)| id)
                .collect();
            assert_eq!(pending.len(), 2, "{pending:?}");
            fx.append("conv-a", &[prompt("u-2", 2, None, "same")]);
            let (entries, removed) = next_append(&mut a_rx).await;
            assert_eq!(entries.iter().map(id_of).collect::<Vec<_>>(), ["u-2"]);
            assert_eq!(removed, [pending[0].clone()], "the oldest goes first");
            fx.append("conv-a", &[prompt("u-3", 3, None, " same ")]);
            let (entries, removed) = next_append(&mut a_rx).await;
            assert_eq!(entries.iter().map(id_of).collect::<Vec<_>>(), ["u-3"]);
            assert_eq!(removed, [pending[1].clone()]);
        }

        #[tokio::test]
        async fn an_unloaded_chat_adds_no_pending_entry() {
            let (fx, mut term) = Fixture::with_terminal("send-unloaded", Some("conv-a"), V176);
            fx.append("conv-a", &[user(1)]);
            let (s_tx, mut s_rx) = mpsc::unbounded_channel();
            send_through(&fx, &mut term, &s_tx, "solo").await;
            assert!(drain(&mut s_rx).is_empty(), "the send was not refused");
            let (a_tx, mut a_rx) = mpsc::unbounded_channel();
            fx.load(1, &a_tx, "ra").await;
            let shown = ids_until(&mut a_rx, "u-1").await;
            assert_eq!(shown, ["u-1"], "no pending entry was kept for a later load");
        }

        /// Chat answers typed into a recording PTY while the test plays the
        /// TUI, as the sends are.
        mod answer {
            use super::*;
            use crate::screen::dialogs::{read, recipe_for};
            use protocol::error_codes::{INVALID_REQUEST, NO_SCREEN};
            use protocol::{ChatAnswerKind, SessionStatus};

            const V01: &str = include_str!("../screen/fixtures/v01-start.txt");
            const V21: &str = include_str!("../screen/fixtures/v21-perm-bash.txt");
            const V22: &str = include_str!("../screen/fixtures/v22-perm-bash.txt");
            const V23: &str = include_str!("../screen/fixtures/v23-amend-open.txt");
            const V32: &str = include_str!("../screen/fixtures/v32-ask-q2.txt");
            const V33: &str = include_str!("../screen/fixtures/v33-ask-q2-toggled.txt");
            const V38: &str = include_str!("../screen/fixtures/v38-ask-q3.txt");
            const V51: &str = include_str!("../screen/fixtures/v51-plan-dialog.txt");
            const V52: &str = include_str!("../screen/fixtures/v52-plan-feedback-open.txt");
            const V152: &str = include_str!("../screen/fixtures/v152-heads-up-collapsed.txt");
            const V192: &str =
                include_str!("../screen/fixtures/v192-agent-view-ask-with-chat-about-this.txt");

            const TAB: &[u8] = b"\t";

            /// `capture` with `from` replaced by `to` once, which it must hold.
            fn edited(capture: &str, from: &str, to: &str) -> String {
                assert!(capture.contains(from), "the capture shows {from:?}");
                capture.replacen(from, to, 1)
            }

            /// Load `s1`'s chat on connection 1: the id the mirror gave the
            /// prompt on screen, and the loader's inbox.
            async fn loaded(fx: &Fixture) -> (String, Inbox) {
                let (a_tx, mut a_rx) = mpsc::unbounded_channel();
                fx.load(1, &a_tx, "ra").await;
                let (_, _, screen, _) = next_chat(&mut a_rx).await;
                let prompt = screen.prompt.expect("the screen shows a prompt");
                (prompt.id, a_rx)
            }

            async fn answer(fx: &Fixture, out_tx: &Outbox, prompt_id: &str, kind: ChatAnswerKind) {
                let msg = ClientMessage::ChatAnswer {
                    session_id: "s1".to_owned(),
                    prompt_id: prompt_id.to_owned(),
                    answer: kind,
                    request_id: Some("r-answer".to_owned()),
                };
                dispatch_on(&fx.hub, msg, 9, out_tx).await;
            }

            /// Load the chat on a terminal showing `first` and answer its
            /// prompt with `kind`.
            async fn answer_on(tag: &str, first: &str, kind: ChatAnswerKind) -> Answering {
                let (fx, term) = Fixture::with_terminal(tag, None, first);
                let (prompt_id, loader) = loaded(&fx).await;
                let (s_tx, s_rx) = mpsc::unbounded_channel();
                answer(&fx, &s_tx, &prompt_id, kind).await;
                Answering {
                    fx,
                    term,
                    prompt_id,
                    loader,
                    s_tx,
                    s_rx,
                }
            }

            /// An answer under way: the session, its terminal, the chat's
            /// loader and the answer's own inbox.
            struct Answering {
                fx: Fixture,
                term: Terminal,
                prompt_id: String,
                loader: Inbox,
                s_tx: Outbox,
                s_rx: Inbox,
            }

            impl Answering {
                /// The next write is `bytes`.
                async fn typed(&mut self, bytes: &[u8], what: &str) {
                    assert_eq!(self.term.typed().await, bytes, "{what}");
                }

                /// The answer still holds the lane and types nothing more
                /// until the screen moves.
                fn waits(&mut self, what: &str) {
                    assert_eq!(
                        self.term.pty.typing().holder(),
                        Some(Holder::ChatAnswer),
                        "{what}: the answer holds the lane"
                    );
                    assert!(
                        self.term.typed_now().is_empty(),
                        "{what}: no key before the screen moves"
                    );
                }

                /// The answer ends and gives the lane back, unrefused.
                async fn answered(&mut self) {
                    self.term.lane_free().await;
                    assert!(
                        drain(&mut self.s_rx).is_empty(),
                        "the answer was not refused"
                    );
                }

                async fn refusal(&mut self) -> (String, String) {
                    answer_refusal(next_message(&mut self.s_rx).await)
                }
            }

            /// The code and message of `msg`, which must be the answer's
            /// `Error`.
            fn answer_refusal(msg: DaemonMessage) -> (String, String) {
                match msg {
                    DaemonMessage::Error {
                        message,
                        code,
                        request_id,
                    } => {
                        assert_eq!(request_id.as_deref(), Some("r-answer"));
                        (code.unwrap_or_default(), message)
                    }
                    other => panic!("expected an Error, got {other:?}"),
                }
            }

            fn option(number: u32) -> ChatAnswerKind {
                ChatAnswerKind::Option { number }
            }

            #[tokio::test]
            async fn permission_allow_and_deny_type_their_digit() {
                for (number, digit) in [(1, b"1"), (4, b"4")] {
                    let mut a =
                        answer_on(&format!("answer-perm-{number}"), PERMISSION, option(number))
                            .await;
                    a.typed(digit, "the option's digit, no Enter").await;
                    a.waits("the digit");
                    a.term.show(C32).await;
                    a.answered().await;
                }
            }

            #[tokio::test]
            async fn a_single_choice_question_types_its_digit() {
                let mut a = answer_on("answer-single", QUESTION, option(2)).await;
                a.typed(b"2", "the option's digit").await;
                a.waits("the digit");
                a.term.show(C32).await;
                a.answered().await;
            }

            #[tokio::test]
            async fn type_something_moves_to_its_row_then_types() {
                let text = || ChatAnswerKind::TypeSomething {
                    text: "fuzz tests".to_owned(),
                };
                let mut a = answer_on("answer-type-something", QUESTION, text()).await;
                for n in 1..=3 {
                    a.typed(DOWN, &format!("Down {n}")).await;
                }
                a.waits("the Downs");
                let on_row = edited(
                    &edited(QUESTION, "  ❯ 1. sub", "    1. sub"),
                    "    4. Type something.",
                    "  ❯ 4. Type something.",
                );
                a.term.show(&on_row).await;
                a.typed(b"fuzz tests", "the text, as typed").await;
                a.typed(ENTER, "then Enter").await;
                a.waits("the Enter");
                a.term.show(C32).await;
                a.answered().await;
            }

            #[tokio::test]
            async fn a_multi_select_toggles_then_submits() {
                let mut a =
                    answer_on("answer-multi", V33, ChatAnswerKind::Toggle { number: 3 }).await;
                a.typed(b"3", "the option's digit").await;
                a.waits("the toggle");
                let unticked = edited(V33, "  3. [✔] edge cases", "  3. [ ] edge cases");
                a.term.show(&unticked).await;
                a.answered().await;

                // A tick is not a new prompt: the id stays.
                answer(&a.fx, &a.s_tx, &a.prompt_id, ChatAnswerKind::Submit).await;
                for n in 1..=5 {
                    a.typed(DOWN, &format!("Down {n}")).await;
                }
                a.waits("the Downs to Next");
                a.term
                    .show(&edited(&unticked, "❯ 1. [✔] add", "  1. [✔] add"))
                    .await;
                a.typed(ENTER, "Enter on Next").await;
                a.waits("the Enter");
                a.term.show(V38).await;
                a.answered().await;
            }

            #[tokio::test]
            async fn a_plan_is_approved_with_its_digit() {
                let mut a = answer_on("answer-plan", V51, option(1)).await;
                a.typed(b"1", "the option's digit").await;
                a.waits("the digit");
                a.term.show(C32).await;
                a.answered().await;
            }

            #[tokio::test]
            async fn plan_feedback_opens_its_field_then_types() {
                let kind = ChatAnswerKind::TypeSomething {
                    text: "also test div".to_owned(),
                };
                let mut a = answer_on("answer-plan-feedback", V51, kind).await;
                a.typed(b"3", "the feedback option's digit").await;
                a.waits("the digit");
                a.term.show(V52).await;
                a.typed(b"also test div", "the text").await;
                a.typed(ENTER, "then Enter").await;
                a.waits("the Enter");
                a.term.show(C32).await;
                a.answered().await;
            }

            #[tokio::test]
            async fn trust_moves_its_cursor_then_enters() {
                let mut a = answer_on("answer-trust", V01, option(2)).await;
                a.typed(DOWN, "Down to the second row").await;
                a.waits("the Down");
                let on_yes = edited(
                    &edited(V01, " ❯ No, exit", "   No, exit"),
                    "   Yes, I trust this folder",
                    " ❯ Yes, I trust this folder",
                );
                a.term.show(&on_yes).await;
                a.typed(ENTER, "then Enter").await;
                a.waits("the Enter");
                a.term.show(C32).await;
                a.answered().await;
            }

            #[tokio::test]
            async fn a_heads_up_types_its_bare_digit() {
                let mut a = answer_on("answer-heads-up", V152, option(2)).await;
                a.typed(b"2", "the bare digit").await;
                a.waits("the digit");
                a.term.show(C32).await;
                a.answered().await;
            }

            #[tokio::test]
            async fn allow_with_text_tabs_then_types_with_no_pending_entry() {
                let kind = ChatAnswerKind::AllowWithText {
                    text: "but print 6*8 instead".to_owned(),
                };
                // v23 is v22's Bash dialog with its amend field open.
                let mut a = answer_on("answer-allow-text", V22, kind).await;
                a.typed(TAB, "Tab opens the amend field").await;
                a.waits("the Tab");
                a.term.show(V23).await;
                a.typed(b"but print 6*8 instead", "the text").await;
                a.typed(ENTER, "then Enter").await;
                a.waits("the Enter");
                a.term.show(C32).await;
                a.answered().await;
                assert!(
                    pending_added(&mut a.loader).is_empty(),
                    "the amend adds no pending entry"
                );
            }

            #[tokio::test]
            async fn deny_then_send_waits_for_the_box() {
                let deny = || ChatAnswerKind::DenyWithText {
                    text: "use 6*8".to_owned(),
                };
                let mut a = answer_on("answer-deny-send", PERMISSION, deny()).await;
                a.typed(b"4", "the No option's digit").await;
                a.waits("the denial");
                a.term.show(C32).await;
                a.typed(&paste("use 6*8"), "the message, once the box is back")
                    .await;
                a.term.show(&with_box(C32, "use 6*8")).await;
                a.typed(ENTER, "then Enter").await;
                let added = pending_added(&mut a.loader);
                assert!(
                    matches!(added.as_slice(), [(id, text)]
                        if id.starts_with("pending:") && text == "use 6*8"),
                    "the message is a pending entry, as a send's is: {added:?}"
                );
                a.term.show(C32).await;
                a.answered().await;

                let mut a = answer_on("answer-deny-no-box", PERMISSION, deny()).await;
                a.typed(b"4", "the No option's digit").await;
                a.term.show(V140).await;
                assert_eq!(
                    a.refusal().await,
                    refused(CHAT_REFUSED, DENIED_NOT_SENT),
                    "the dialog went, but the box never came back"
                );
                assert!(a.term.typed_now().is_empty(), "no paste");
            }

            const DENIED_NOT_SENT: &str = "the denial was typed, but the message was not sent";
            const NOT_TAKEN: &str = "the terminal did not take the answer";
            const PROMPT_GONE: &str = "the prompt is no longer up";

            fn deny_with(text: &str) -> ChatAnswerKind {
                ChatAnswerKind::DenyWithText {
                    text: text.to_owned(),
                }
            }

            #[tokio::test]
            async fn a_denial_the_screen_never_shows_was_not_taken() {
                let mut a = answer_on("answer-deny-unseen", PERMISSION, deny_with("x")).await;
                a.typed(b"4", "the No option's digit").await;
                assert_eq!(a.refusal().await, refused(CHAT_REFUSED, NOT_TAKEN));
                assert!(a.term.typed_now().is_empty(), "no paste");
            }

            #[tokio::test]
            async fn a_denial_followed_by_another_dialog_is_not_sent() {
                let mut a = answer_on("answer-deny-next", PERMISSION, deny_with("x")).await;
                a.typed(b"4", "the No option's digit").await;
                a.term.show(V21).await;
                assert_eq!(a.refusal().await, refused(CHAT_REFUSED, DENIED_NOT_SENT));
                assert!(a.term.typed_now().is_empty(), "no paste");
            }

            /// On a clock paused once the chat has loaded, so the ceiling
            /// expires, by the order of the timers alone, while the answer
            /// waits for the box after the dialog went.
            #[tokio::test]
            async fn a_ceiling_after_the_denial_says_it_was_typed() {
                let (fx, mut term) =
                    Fixture::with_terminal("answer-deny-ceiling", None, PERMISSION);
                let (prompt_id, _loader) = loaded(&fx).await;
                let target =
                    crate::server::chat::send_target(&fx.hub, "s1").expect("s1 has a terminal");
                tokio::time::pause();
                let answering = tokio::spawn(async move {
                    crate::chat_answer::answer_within(
                        &target,
                        &prompt_id,
                        &deny_with("x"),
                        crate::view_drive::KEY_WAIT / 2,
                    )
                    .await
                });
                assert_eq!(term.typed().await, b"4", "the No option's digit");
                term.show(V140).await;
                let answered = answering.await.expect("the answer ran");
                assert_eq!(
                    answered.map_err(|refusal| (refusal.code.to_owned(), refusal.message)),
                    Err(refused(CHAT_REFUSED, DENIED_NOT_SENT))
                );
                assert!(term.typed_now().is_empty(), "no paste");
            }

            #[tokio::test]
            async fn a_tab_that_only_repaints_stops_the_amend() {
                let kind = ChatAnswerKind::AllowWithText {
                    text: "but print 6*8 instead".to_owned(),
                };
                let mut a = answer_on("answer-allow-repaint", V22, kind).await;
                a.typed(TAB, "Tab opens the amend field").await;
                a.term.show(V22).await;
                assert_eq!(a.refusal().await, refused(CHAT_REFUSED, NOT_TAKEN));
                assert!(a.term.typed_now().is_empty(), "the text is never typed");
            }

            #[tokio::test]
            async fn a_feedback_digit_that_changes_nothing_stops_the_feedback() {
                let kind = ChatAnswerKind::TypeSomething {
                    text: "also test div".to_owned(),
                };
                let mut a = answer_on("answer-plan-repaint", V51, kind).await;
                a.typed(b"3", "the feedback option's digit").await;
                a.term.show(V51).await;
                assert_eq!(a.refusal().await, refused(CHAT_REFUSED, NOT_TAKEN));
                assert!(a.term.typed_now().is_empty(), "the text is never typed");
            }

            #[tokio::test]
            async fn a_return_to_another_prompt_types_no_answer() {
                let mut a = answer_on("answer-return-other", V174, option(2)).await;
                a.typed(DOWN, "Down to the panel").await;
                a.term.show(&v174_panel_on_main()).await;
                a.typed(ENTER, "Enter on main").await;
                a.term.show(V175).await;
                a.typed(UP, "Up to the box").await;
                // A dialog in main fails the return itself; a Heads up whose
                // options differ is a prompt the return reaches.
                a.term
                    .show(&edited(V176, "2: Knew this already", "2: Seen it"))
                    .await;
                assert_eq!(a.refusal().await, refused(CHAT_REFUSED, PROMPT_GONE));
                assert!(a.term.typed_now().is_empty(), "no digit");
            }

            #[tokio::test]
            async fn bad_text_in_an_agents_view_is_refused_before_any_key() {
                let kind = ChatAnswerKind::TypeSomething {
                    text: "a\x1b[Bb".to_owned(),
                };
                let mut a = answer_on("answer-agent-bad-text", V174, kind).await;
                assert_eq!(
                    a.refusal().await,
                    refused(INVALID_REQUEST, "the message holds a control character")
                );
                assert!(a.term.typed_now().is_empty(), "nothing is typed");
            }

            #[tokio::test]
            async fn a_permission_answer_followed_by_the_next_dialog_clears_it() {
                let mut a = answer_pending("answer-next-dialog", &bash_prompt("python -c")).await;
                // v21 is the next dialog: the file's creation, after c23's Bash.
                a.term.show(V21).await;
                a.answered().await;
                assert_eq!(
                    waiting(&a.fx),
                    (SessionStatus::Idle, None),
                    "the next dialog shows the answered one gone"
                );
            }

            #[tokio::test]
            async fn a_heads_up_answer_leaves_mains_pending_prompt() {
                let pending = bash_prompt("python -c");
                let (fx, term) = Fixture::with_terminal("answer-heads-up-pending", None, V152);
                fx.hub.sessions.update("s1", |record| {
                    record.hook_driven = true;
                    record.status = SessionStatus::AwaitingInput;
                    record.pending_input = Some(pending.clone());
                });
                let (prompt_id, loader) = loaded(&fx).await;
                let (s_tx, s_rx) = mpsc::unbounded_channel();
                answer(&fx, &s_tx, &prompt_id, option(2)).await;
                let mut a = Answering {
                    fx,
                    term,
                    prompt_id,
                    loader,
                    s_tx,
                    s_rx,
                };
                a.typed(b"2", "the bare digit").await;
                a.term.show(C32).await;
                a.answered().await;
                assert_eq!(
                    waiting(&a.fx),
                    (SessionStatus::AwaitingInput, Some(pending)),
                    "a Heads up is not the prompt main waits on"
                );
            }

            #[tokio::test]
            async fn a_hook_arriving_mid_answer_is_cleared() {
                let (fx, term) = Fixture::with_terminal("answer-late-hook", None, PERMISSION);
                fx.hub
                    .sessions
                    .update("s1", |record| record.hook_driven = true);
                let (prompt_id, loader) = loaded(&fx).await;
                let (s_tx, s_rx) = mpsc::unbounded_channel();
                answer(&fx, &s_tx, &prompt_id, option(1)).await;
                let mut a = Answering {
                    fx,
                    term,
                    prompt_id,
                    loader,
                    s_tx,
                    s_rx,
                };
                a.typed(b"1", "the digit").await;
                a.waits("the digit");
                a.fx.hub.sessions.update("s1", |record| {
                    record.status = SessionStatus::AwaitingInput;
                    record.pending_input = Some(bash_prompt("python -c"));
                });
                a.term.show(C32).await;
                a.answered().await;
                assert_eq!(
                    waiting(&a.fx),
                    (SessionStatus::Idle, None),
                    "the hook was for the dialog the answer took"
                );
            }

            #[tokio::test]
            async fn an_unreadable_screen_after_the_last_key_is_refused() {
                let mut a = answer_on("answer-unreadable", V152, option(2)).await;
                a.typed(b"2", "the bare digit").await;
                a.term.show("  Thinking it over\n  still going").await;
                assert_eq!(a.refusal().await, refused(CHAT_REFUSED, NOT_TAKEN));
            }

            #[tokio::test]
            async fn a_denial_with_a_message_is_refused_in_an_agents_view() {
                let mut a = answer_on("answer-agent-deny", V192, deny_with("x")).await;
                assert_eq!(
                    a.refusal().await,
                    refused(
                        CHAT_REFUSED,
                        "a denial with a message works only in the main conversation"
                    )
                );
                assert!(a.term.typed_now().is_empty(), "nothing is typed");
            }

            #[tokio::test]
            async fn a_stale_prompt_id_is_refused() {
                let gone = refused(CHAT_REFUSED, "the prompt is no longer up");
                let (fx, mut term) = Fixture::with_terminal("answer-stale-id", None, PERMISSION);
                let (prompt_id, _loader) = loaded(&fx).await;
                let (s_tx, mut s_rx) = mpsc::unbounded_channel();
                answer(&fx, &s_tx, &format!("{prompt_id}-old"), option(1)).await;
                assert_eq!(
                    answer_refusal(next_message(&mut s_rx).await),
                    gone,
                    "another prompt's id"
                );
                term.draw(V51);
                answer(&fx, &s_tx, &prompt_id, option(1)).await;
                assert_eq!(
                    answer_refusal(next_message(&mut s_rx).await),
                    gone,
                    "the grid shows another dialog than the mirror's"
                );
                assert!(term.typed_now().is_empty(), "nothing is typed");

                let (fx, mut term) = Fixture::with_terminal("answer-unloaded", None, PERMISSION);
                answer(&fx, &s_tx, &prompt_id, option(1)).await;
                assert_eq!(
                    answer_refusal(next_message(&mut s_rx).await),
                    gone,
                    "a chat nobody loads has no prompt ids"
                );
                assert!(term.typed_now().is_empty(), "nothing is typed");
            }

            #[tokio::test]
            async fn a_screen_that_does_not_move_stops_the_recipe() {
                let kind = ChatAnswerKind::AllowWithText {
                    text: "x".to_owned(),
                };
                let mut a = answer_on("answer-still", V22, kind).await;
                a.typed(TAB, "the first group").await;
                assert_eq!(
                    a.refusal().await,
                    refused(CHAT_REFUSED, "the terminal did not take the answer")
                );
                assert!(a.term.typed_now().is_empty(), "no second group");
            }

            #[test]
            fn no_recipe_sends_esc() {
                let agent = agent_registry::default_agent();
                let captures = [
                    PERMISSION, V21, V23, QUESTION, V32, V33, V51, V52, V01, V152, V174, V192,
                ];
                let text = || "x".to_owned();
                let mut kinds = vec![
                    ChatAnswerKind::Submit,
                    ChatAnswerKind::Back,
                    ChatAnswerKind::Skip,
                    ChatAnswerKind::TypeSomething { text: text() },
                    ChatAnswerKind::AllowWithText { text: text() },
                    ChatAnswerKind::DenyWithText { text: text() },
                ];
                for number in 0..=6 {
                    kinds.push(option(number));
                    kinds.push(ChatAnswerKind::Toggle { number });
                }
                let mut built = 0;
                for capture in captures {
                    let prompt = read(&agent, &screen_from(capture))
                        .and_then(|screen| screen.prompt)
                        .expect("the capture shows a prompt");
                    for kind in &kinds {
                        let Ok(recipe) = recipe_for(&agent, &prompt, kind) else {
                            continue;
                        };
                        built += 1;
                        for step in &recipe.steps {
                            let bytes: Vec<u8> = step
                                .keys
                                .iter()
                                .flat_map(agent_api::Key::to_bytes)
                                .collect();
                            for key in &step.keys {
                                assert_ne!(key.to_bytes(), b"\x1b", "{kind:?}: a lone Esc");
                            }
                            assert!(
                                !bytes.windows(2).any(|pair| pair == b"\x1b\x1b"),
                                "{kind:?}: Esc Esc in {bytes:?}"
                            );
                            assert!(!bytes.contains(&0x03), "{kind:?}: ctrl+c in {bytes:?}");
                        }
                    }
                }
                assert!(built > 20, "the table builds recipes: {built}");
            }

            #[tokio::test]
            async fn a_heads_up_in_an_agents_view_returns_first() {
                let mut a = answer_on("answer-agent-view", V174, option(2)).await;
                a.typed(DOWN, "Down to the panel").await;
                a.term.show(&v174_panel_on_main()).await;
                a.typed(ENTER, "Enter on main").await;
                a.term.show(V175).await;
                a.typed(UP, "Up to the box").await;
                a.term.show(V176).await;
                a.typed(b"2", "the digit, once main is in view").await;
                a.waits("the digit");
                a.term.show(C32).await;
                a.answered().await;
            }

            #[tokio::test]
            async fn a_dialog_in_an_agents_view_is_answered_in_place() {
                let mut a = answer_on("answer-agent-dialog", V192, option(1)).await;
                a.typed(b"1", "the digit, with no return first").await;
                a.waits("the digit");
                a.term.show(C32).await;
                a.answered().await;
            }

            #[tokio::test]
            async fn a_held_lane_refuses_busy() {
                let (fx, mut term) = Fixture::with_terminal("answer-busy", None, PERMISSION);
                let (prompt_id, _loader) = loaded(&fx).await;
                let _held = term
                    .pty
                    .typing()
                    .try_take(Holder::ChatSend)
                    .expect("a free lane");
                let (s_tx, mut s_rx) = mpsc::unbounded_channel();
                answer(&fx, &s_tx, &prompt_id, option(1)).await;
                assert_eq!(
                    answer_refusal(next_message(&mut s_rx).await),
                    refused(BUSY, "another message is being typed")
                );
                assert!(term.typed_now().is_empty(), "nothing is typed");
            }

            #[tokio::test]
            async fn an_owner_key_mid_answer_stops_it() {
                let kind = ChatAnswerKind::AllowWithText {
                    text: "x".to_owned(),
                };
                let mut a = answer_on("answer-owner-key", V22, kind).await;
                a.typed(TAB, "the first group").await;
                a.term.pty.typing().owner_typed();
                a.term.show(V23).await;
                assert_eq!(
                    a.refusal().await,
                    refused(CHAT_REFUSED, "someone typed in the terminal")
                );
                assert!(a.term.typed_now().is_empty(), "no second group");
            }

            fn bash_prompt(summary: &str) -> PendingInput {
                PendingInput::Permission {
                    tool_name: "Bash".to_owned(),
                    summary: summary.to_owned(),
                }
            }

            /// The status and pending input of `s1`'s record.
            fn waiting(fx: &Fixture) -> (SessionStatus, Option<PendingInput>) {
                let record = fx.hub.sessions.get("s1").expect("s1 is registered");
                let record = lock(&record);
                (record.status, record.pending_input.clone())
            }

            /// Answer the permission dialog of a hook-driven `s1` whose
            /// record holds `pending`, up to its digit.
            async fn answer_pending(tag: &str, pending: &PendingInput) -> Answering {
                let (fx, term) = Fixture::with_terminal(tag, None, PERMISSION);
                fx.hub.sessions.update("s1", |record| {
                    record.hook_driven = true;
                    record.status = SessionStatus::AwaitingInput;
                    record.pending_input = Some(pending.clone());
                });
                let (prompt_id, loader) = loaded(&fx).await;
                let (s_tx, s_rx) = mpsc::unbounded_channel();
                answer(&fx, &s_tx, &prompt_id, option(1)).await;
                let mut a = Answering {
                    fx,
                    term,
                    prompt_id,
                    loader,
                    s_tx,
                    s_rx,
                };
                a.typed(b"1", "the digit").await;
                a.waits("the digit");
                a
            }

            #[tokio::test]
            async fn the_answer_clears_the_pending_prompt() {
                let pending = bash_prompt("python -c");
                let mut a = answer_pending("answer-clears", &pending).await;
                assert_eq!(
                    waiting(&a.fx),
                    (SessionStatus::AwaitingInput, Some(pending)),
                    "nothing is cleared while the dialog is up"
                );
                a.term.show(C32).await;
                a.answered().await;
                assert_eq!(
                    waiting(&a.fx),
                    (SessionStatus::Idle, None),
                    "the dialog's going clears the prompt"
                );
                assert_eq!(
                    a.term.pty.typing().owner_epoch(),
                    0,
                    "an answer is no owner key"
                );
            }

            #[tokio::test]
            async fn a_different_pending_prompt_is_left() {
                let mut a = answer_pending("answer-leaves", &bash_prompt("python -c")).await;
                let next = bash_prompt("rm notes.txt");
                a.fx.hub.sessions.update("s1", |record| {
                    record.pending_input = Some(next.clone());
                });
                a.term.show(C32).await;
                a.answered().await;
                assert_eq!(
                    waiting(&a.fx),
                    (SessionStatus::AwaitingInput, Some(next)),
                    "a prompt raised under the answer is not the one it answered"
                );
            }

            #[tokio::test]
            async fn chat_answer_is_handled() {
                let fx = Fixture::new("answer-no-terminal", None);
                let (a_tx, mut a_rx) = mpsc::unbounded_channel();
                answer(&fx, &a_tx, "p1", ChatAnswerKind::Submit).await;
                assert_eq!(
                    answer_refusal(next_message(&mut a_rx).await),
                    refused(NO_SCREEN, "the session has no terminal")
                );
                let (fx, _term) = Fixture::with_terminal("answer-bad-text", None, QUESTION);
                let (prompt_id, _loader) = loaded(&fx).await;
                let cases = [
                    ("a\x1b[Bb", "the message holds a control character"),
                    ("a\nb", "the answer holds a control character"),
                ];
                for (text, why) in cases {
                    let kind = ChatAnswerKind::TypeSomething {
                        text: text.to_owned(),
                    };
                    answer(&fx, &a_tx, &prompt_id, kind).await;
                    assert_eq!(
                        answer_refusal(next_message(&mut a_rx).await),
                        refused(INVALID_REQUEST, why),
                        "{text:?}"
                    );
                }
            }
        }
    }
}
