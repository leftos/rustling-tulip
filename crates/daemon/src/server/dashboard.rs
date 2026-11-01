//! The Dashboard's messages: watching a Dashboard's facts, asking for one
//! session's detail, and answering a waiting session's prompt
//! (`AnswerPrompt`), which types the keys through the chat answer's runner
//! ([`crate::chat_answer::answer_identified`]) and answers only a refusal.
use super::Hub;
use super::chat::{refuse, send_target};
use super::connection::ConnCtx;
use crate::chat_answer::{self, NOT_THIS_DIALOG, PROMPT_GONE};
use crate::inject::{TypeRefusal, TypeTarget, screen_reader};
use crate::screen::dialogs::prompt_identity;
use crate::sync::lock;
use crate::view_drive::Reading;
use chrono::{DateTime, Utc};
use protocol::error_codes::INVALID_REQUEST;
use protocol::{
    ChatAnswerKind, ChatPrompt, ChatPromptKind, ClientMessage, DaemonMessage, PendingInput,
    PendingQuestion, PromptChoice,
};
use tracing::{debug, warn};

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
        ClientMessage::AnswerPrompt {
            session_id,
            since,
            choice,
            request_id,
        } => answer_prompt(hub, ctx, &session_id, since, choice, request_id),
        _ => warn!("dashboard::handle: a message that is not the Dashboard's was routed here"),
    }
}

/// Answer `session_id`'s pending prompt with `choice` on a task of its own,
/// after the checks that need no waiting; a refusal, now or from the task,
/// is answered with an `Error` carrying `request_id`.
fn answer_prompt(
    hub: &Hub,
    ctx: &ConnCtx<'_>,
    session_id: &str,
    since: DateTime<Utc>,
    choice: PromptChoice,
    request_id: Option<String>,
) {
    let target = match send_target(hub, session_id) {
        Ok(target) => target,
        Err((code, message)) => return refuse(ctx.out_tx, code, message, request_id),
    };
    if choice == PromptChoice::Unknown {
        let refusal = not_this_prompt();
        return refuse(ctx.out_tx, refusal.code, refusal.message, request_id);
    }
    let out_tx = ctx.out_tx.clone();
    tokio::spawn(async move {
        if let Err(refusal) = answer_waiting(&target, since, choice).await {
            debug!(
                session_id = %target.session_id,
                code = refusal.code,
                message = %refusal.message,
                "dashboard answer refused"
            );
            refuse(&out_tx, refusal.code, refusal.message, request_id);
        }
    });
}

/// Answer `target`'s pending prompt with `choice` while the session still
/// waits on the prompt it waited on at `since` and its screen shows that
/// prompt's dialog.
async fn answer_waiting(
    target: &TypeTarget,
    since: DateTime<Utc>,
    choice: PromptChoice,
) -> Result<(), TypeRefusal> {
    let pending = pending_since(target, since)?;
    within_options(choice, &pending)?;
    let reading = screen_reader(target).current().await;
    let prompt = dialog_for(&reading, &pending)?;
    let kind = chat_answer_for(choice, prompt)?;
    let identity = prompt_identity(prompt);
    chat_answer::answer_identified(target, identity, since, &pending, &kind).await
}

/// Refuse an option index past the pending question's options.
fn within_options(choice: PromptChoice, pending: &PendingInput) -> Result<(), TypeRefusal> {
    let (PromptChoice::Option { index }, PendingInput::Question { questions }) = (choice, pending)
    else {
        return Ok(());
    };
    let listed = questions
        .first()
        .is_some_and(|question| usize::from(index) < question.options.len());
    if listed {
        Ok(())
    } else {
        Err(not_this_prompt())
    }
}

/// The prompt `target`'s record waits on, when its status has not changed
/// since `since` and the Dashboard offers answers for it.
fn pending_since(target: &TypeTarget, since: DateTime<Utc>) -> Result<PendingInput, TypeRefusal> {
    let record = target
        .sessions
        .get(&target.session_id)
        .ok_or_else(|| TypeRefusal::refused(PROMPT_GONE))?;
    let record = lock(&record);
    if record.status_since != Some(since) {
        return Err(TypeRefusal::refused(PROMPT_GONE));
    }
    let pending = record
        .pending_input
        .clone()
        .ok_or_else(|| TypeRefusal::refused(PROMPT_GONE))?;
    if !offered(&pending) {
        return Err(not_this_prompt());
    }
    Ok(pending)
}

/// Whether the Dashboard answers `pending`: one single-choice question, or
/// a permission.
fn offered(pending: &PendingInput) -> bool {
    match pending {
        PendingInput::Question { questions } => {
            matches!(questions.as_slice(), [question] if !question.multi_select)
        }
        PendingInput::Permission { .. } => true,
        PendingInput::PlanApproval { .. } | PendingInput::Other { .. } | PendingInput::Unknown => {
            false
        }
    }
}

/// The dialog `reading` shows for `pending`: for a question, a single-choice
/// question with at most one question tab, asking the pending question with
/// its options first in order; for a permission, a permission dialog whose
/// subject or body shows the pending call's summary.
fn dialog_for<'r>(
    reading: &'r Reading,
    pending: &PendingInput,
) -> Result<&'r ChatPrompt, TypeRefusal> {
    let shown = reading.chat.as_ref().and_then(|chat| chat.prompt.as_ref());
    shown
        .filter(|prompt| match pending {
            PendingInput::Question { questions } => {
                prompt.kind == ChatPromptKind::Question
                    && !prompt.multi
                    && prompt.questions.len() <= 1
                    && questions
                        .first()
                        .is_some_and(|question| asks(prompt, question))
            }
            PendingInput::Permission { summary, .. } => {
                prompt.kind == ChatPromptKind::Permission && shows_summary(prompt, summary)
            }
            _ => false,
        })
        .ok_or_else(|| TypeRefusal::refused(PROMPT_GONE))
}

/// Whether `prompt` asks `question`: the same question text, and the
/// question's option labels first among its options, in order. The dialog
/// may list rows of its own after them (`Type something`).
fn asks(prompt: &ChatPrompt, question: &PendingQuestion) -> bool {
    let labels: Vec<String> = prompt
        .options
        .iter()
        .map(|option| words(&option.label))
        .collect();
    let wanted: Vec<String> = question
        .options
        .iter()
        .map(String::as_str)
        .map(words)
        .collect();
    words(&prompt.subject) == words(&question.question) && labels.starts_with(&wanted)
}

/// Whether `prompt`'s subject or body shows `summary`; never for an empty
/// summary, which matches any dialog.
fn shows_summary(prompt: &ChatPrompt, summary: &str) -> bool {
    let summary = words(summary);
    !summary.is_empty()
        && (words(&prompt.subject).contains(&summary) || words(&prompt.body).contains(&summary))
}

/// `text` with each run of whitespace made one space and its ends trimmed,
/// so a line the screen wrapped compares equal to the hook's text.
fn words(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The chat answer that gives `choice` to `prompt`: a question's option by
/// its number, a permission's allow-once, don't-ask-again or deny option by
/// the number the screen reads for it.
///
/// # Errors
///
/// [`INVALID_REQUEST`] for a choice that is not one for this kind of
/// prompt; [`protocol::error_codes::CHAT_REFUSED`] when the dialog shows no
/// option for it.
fn chat_answer_for(
    choice: PromptChoice,
    prompt: &ChatPrompt,
) -> Result<ChatAnswerKind, TypeRefusal> {
    let number = match (choice, &prompt.kind) {
        (PromptChoice::Option { index }, ChatPromptKind::Question) => Some(u32::from(index) + 1),
        (PromptChoice::Allow, ChatPromptKind::Permission) => allow_once_option(prompt),
        (PromptChoice::AllowAlways, ChatPromptKind::Permission) => always_allow_option(prompt),
        (PromptChoice::Deny, ChatPromptKind::Permission) => labelled(prompt, DENY_LABEL),
        _ => return Err(not_this_prompt()),
    };
    number
        .map(|number| ChatAnswerKind::Option { number })
        .ok_or_else(|| TypeRefusal::refused(NOT_THIS_DIALOG))
}

/// The label of a permission dialog's option that allows once.
const ALLOW_ONCE_LABEL: &str = "Yes";
/// The label of a permission dialog's option that denies.
const DENY_LABEL: &str = "No";

/// The number of `prompt`'s last option labelled exactly `label`.
fn labelled(prompt: &ChatPrompt, label: &str) -> Option<u32> {
    prompt
        .options
        .iter()
        .rev()
        .find(|option| option.label.trim() == label)
        .map(|option| option.number)
}

fn allow_once_option(prompt: &ChatPrompt) -> Option<u32> {
    labelled(prompt, ALLOW_ONCE_LABEL)
}

/// The number of the option the screen reads as "don't ask again": the one
/// whose label ends with the rule it saves; `None` when the dialog offers
/// none.
fn always_allow_option(prompt: &ChatPrompt) -> Option<u32> {
    let rule = prompt.always_allow_tip.as_deref()?;
    let once = allow_once_option(prompt);
    prompt
        .options
        .iter()
        .find(|option| Some(option.number) != once && option.label.trim_end().ends_with(rule))
        .map(|option| option.number)
}

/// The refusal of an answer the Dashboard does not offer for the prompt.
fn not_this_prompt() -> TypeRefusal {
    TypeRefusal {
        code: INVALID_REQUEST,
        message: NOT_THIS_DIALOG.to_owned(),
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
            code: Some(protocol::error_codes::NOT_FOUND.to_owned()),
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
    use crate::server::connection::{AuthKind, ConnCtx, FetchRegistry, PtyLane};
    use crate::server::dispatch;
    use crate::server::test_support::{
        ScratchDir, drain, git_init, git_ok, hello, idle_record, loopback_ws_client, next_message,
        test_hub, ws_next, ws_send,
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
        let pty_lane: Arc<PtyLane> = Arc::default();
        let fetches: FetchRegistry = Arc::new(AsyncMutex::new(HashMap::new()));
        let local = AuthKind::Local;
        let ctx = ConnCtx {
            out_tx: &conn.out_tx,
            file_tx: &file_tx,
            pty_lane: &pty_lane,
            fetches: &fetches,
            tails: &Arc::default(),
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

    /// Ask for `session_id`'s detail and return the reply its task sends.
    async fn ask_detail(hub: &Hub, conn: &mut Conn, session_id: &str) -> DaemonMessage {
        let ask = ClientMessage::GetSessionDetail {
            session_id: session_id.to_owned(),
            request_id: Some("d1".to_owned()),
        };
        send(hub, conn, ask).await;
        next_message(&mut conn.rx).await
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

        let reply = ask_detail(&hub, &mut c1, "s1").await;
        assert!(
            matches!(
                reply,
                DaemonMessage::SessionDetail { ref session_id, ref lines, prompt: Some(ref prompt), ref work, request_id: Some(ref id) }
                    if session_id == "s1" && lines.is_empty() && prompt == "fix the login bug"
                        && *work == RecentWork::default() && id == "d1"
            ),
            "{reply:?}"
        );

        let reply = ask_detail(&hub, &mut c1, "nope").await;
        assert!(
            matches!(
                reply,
                DaemonMessage::Error { ref message, request_id: Some(ref id), .. }
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
        // The read shells out to git under its own 10 s timeout, well inside
        // the reply's hang guard.
        let reply = ask_detail(&hub, &mut c1, "s1").await;
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

    /// `AnswerPrompt` typed into a recording PTY while the test plays the
    /// TUI, with no chat loaded.
    mod answer_prompt {
        use std::sync::{Arc, Mutex};
        use std::time::Duration;

        use chrono::{DateTime, Utc};
        use protocol::error_codes::{CHAT_REFUSED, INVALID_REQUEST, NOT_FOUND};
        use protocol::{
            ChatAnswerKind, ChatPrompt, ClientMessage, DaemonMessage, PendingInput,
            PendingQuestion, PromptChoice, SessionStatus,
        };
        use tokio::sync::mpsc;

        use super::{Conn, conn, send};
        use crate::history::test_support::{FakeExit, recording_pty};
        use crate::pty::PtyHandle;
        use crate::screen::dialogs::read;
        use crate::screen::fixture_screen::screen_from;
        use crate::screen::{Screen, SharedScreen};
        use crate::server::Hub;
        use crate::server::dashboard::chat_answer_for;
        use crate::server::test_support::{
            HANG_GUARD, ScratchDir, drain, idle_record, next_message, test_hub,
        };
        use crate::sync::lock;
        use crate::typing_lane::Holder;
        use crate::view_drive::READ_QUIET;

        /// A permission dialog for `python -c "print(6*7)"`: 1 Yes, 2 don't
        /// ask again, 3 auto mode, 4 No.
        const PERMISSION: &str = include_str!("../screen/fixtures/c23-perm-bash.txt");
        /// One single-choice question: 1 Red, 2 Blue.
        const ONE_QUESTION: &str =
            include_str!("../screen/fixtures/v192-agent-view-ask-with-chat-about-this.txt");
        /// A multi-select question.
        const MULTI_SELECT: &str = include_str!("../screen/fixtures/v33-ask-q2-toggled.txt");
        /// Main while a turn runs, no dialog up.
        const NO_DIALOG: &str = include_str!("../screen/fixtures/c32-ask-chat.txt");

        const GONE: &str = "the prompt is no longer up";
        const NOT_THIS: &str = "that answer does not fit the prompt on screen";

        /// The session's terminal, whose TUI the test plays.
        struct Terminal {
            pty: Arc<PtyHandle>,
            screen: SharedScreen,
            input: mpsc::UnboundedReceiver<Vec<u8>>,
            exit: FakeExit,
        }

        impl Terminal {
            /// Draw `capture` on the grid and send it as the TUI's output.
            async fn show(&self, capture: &str) {
                let bytes = draw_on(&self.screen, capture);
                self.exit.chunk(bytes).await;
            }

            /// The next write typed into the terminal.
            async fn typed(&mut self) -> Vec<u8> {
                tokio::time::timeout(HANG_GUARD, self.input.recv())
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
                tokio::time::timeout(HANG_GUARD, async {
                    while self.pty.typing().holder().is_some() {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await
                .expect("hung: the typing lane was never given back");
            }
        }

        /// Draw `capture` whole on `screen`, sized to fit it; the bytes that
        /// draw it.
        fn draw_on(screen: &SharedScreen, capture: &str) -> Vec<u8> {
            let rows: Vec<&str> = capture.lines().collect();
            let widest = rows.iter().map(|row| row.chars().count()).max();
            let cols = u16::try_from(widest.unwrap_or(0) + 4).expect("a capture fits a grid");
            let height = u16::try_from(rows.len() + 2).expect("a capture fits a grid");
            let bytes = format!("\x1b[H\x1b[2J{}", rows.join("\r\n")).into_bytes();
            let mut screen = lock(screen);
            screen.resize(cols, height);
            screen.feed(&bytes);
            bytes
        }

        /// Session `s1` waiting on `pending` since `since`, its terminal
        /// showing `capture`, and a connection to answer it on.
        struct Waiting {
            hub: Hub,
            _scratch: ScratchDir,
            term: Terminal,
            since: DateTime<Utc>,
            conn: Conn,
        }

        fn waiting(tag: &str, capture: &str, pending: PendingInput) -> Waiting {
            let (hub, scratch) = test_hub(tag);
            let (pty, input, exit) = recording_pty();
            let term = Terminal {
                pty,
                screen: Arc::new(Mutex::new(Screen::new(1, 1))),
                input,
                exit,
            };
            draw_on(&term.screen, capture);
            let since = Utc::now();
            let mut record = idle_record("s1");
            record.hook_driven = true;
            record.status = SessionStatus::AwaitingInput;
            record.status_since = Some(since);
            record.pending_input = Some(pending);
            record.pty = Some(Arc::clone(&term.pty));
            record.screen = Some(Arc::clone(&term.screen));
            hub.sessions.insert(record);
            Waiting {
                hub,
                _scratch: scratch,
                term,
                since,
                conn: conn(1),
            }
        }

        impl Waiting {
            async fn answer_at(&self, since: DateTime<Utc>, choice: PromptChoice) {
                send(&self.hub, &self.conn, answer_prompt("s1", since, choice)).await;
            }

            async fn answer(&self, choice: PromptChoice) {
                self.answer_at(self.since, choice).await;
            }

            /// The answer's `Error`: its request id, code and message.
            async fn refusal(&mut self) -> (Option<String>, String, String) {
                refusal_of(next_message(&mut self.conn.rx).await)
            }

            /// The answer types `bytes`, then ends unrefused once the
            /// dialog goes.
            async fn types_and_ends(&mut self, bytes: &[u8], what: &str) {
                assert_eq!(self.term.typed().await, bytes, "{what}");
                self.term.show(NO_DIALOG).await;
                self.term.lane_free().await;
                assert!(
                    drain(&mut self.conn.rx).is_empty(),
                    "the answer was not refused"
                );
                assert!(self.term.typed_now().is_empty(), "nothing more is typed");
            }

            /// The answer is refused with `code` and `message`, and nothing
            /// is typed.
            async fn refused(&mut self, code: &str, message: &str, what: &str) {
                let (request_id, got_code, got_message) = self.refusal().await;
                assert_eq!(request_id.as_deref(), Some("a1"), "{what}");
                assert_eq!(
                    (got_code.as_str(), got_message.as_str()),
                    (code, message),
                    "{what}"
                );
                assert!(self.term.typed_now().is_empty(), "{what}: nothing is typed");
            }
        }

        fn answer_prompt(
            session_id: &str,
            since: DateTime<Utc>,
            choice: PromptChoice,
        ) -> ClientMessage {
            ClientMessage::AnswerPrompt {
                session_id: session_id.to_owned(),
                since,
                choice,
                request_id: Some("a1".to_owned()),
            }
        }

        fn refusal_of(msg: DaemonMessage) -> (Option<String>, String, String) {
            let shown = format!("{msg:?}");
            let parts = match msg {
                DaemonMessage::Error {
                    message,
                    code,
                    request_id,
                } => Some((request_id, code.unwrap_or_default(), message)),
                _ => None,
            };
            assert!(parts.is_some(), "expected an Error, got {shown}");
            parts.expect("an Error")
        }

        fn question(options: &[&str], multi_select: bool) -> PendingInput {
            PendingInput::Question {
                questions: vec![PendingQuestion {
                    question: "Pick a colour".to_owned(),
                    header: "Colour".to_owned(),
                    options: options.iter().map(|&option| option.to_owned()).collect(),
                    multi_select,
                }],
            }
        }

        fn bash_permission() -> PendingInput {
            PendingInput::Permission {
                tool_name: "Bash".to_owned(),
                summary: "python -c \"print(6*7)\"".to_owned(),
            }
        }

        /// The prompt `capture` shows, as the default agent reads it.
        fn prompt_on(capture: &str) -> ChatPrompt {
            let agent = agent_registry::default_agent();
            read(&agent, &screen_from(capture))
                .and_then(|screen| screen.prompt)
                .expect("the capture shows a prompt")
        }

        /// A choice's option number, or its refusal's code and message.
        type Mapped = Result<u32, (&'static str, &'static str)>;

        #[test]
        fn answer_prompt_choice_maps_to_the_option_the_screen_reads() {
            let permission = prompt_on(PERMISSION);
            let mut no_always = permission.clone();
            no_always.always_allow_tip = None;
            let question = prompt_on(ONE_QUESTION);
            let not_this = Err((INVALID_REQUEST, NOT_THIS));
            let table: [(&str, &ChatPrompt, PromptChoice, Mapped); 11] = [
                ("allow once", &permission, PromptChoice::Allow, Ok(1)),
                (
                    "don't ask again",
                    &permission,
                    PromptChoice::AllowAlways,
                    Ok(2),
                ),
                (
                    "deny is No, not Esc",
                    &permission,
                    PromptChoice::Deny,
                    Ok(4),
                ),
                (
                    "no don't-ask-again on screen",
                    &no_always,
                    PromptChoice::AllowAlways,
                    Err((CHAT_REFUSED, NOT_THIS)),
                ),
                (
                    "an option index on a permission",
                    &permission,
                    PromptChoice::Option { index: 0 },
                    not_this,
                ),
                (
                    "unknown on a permission",
                    &permission,
                    PromptChoice::Unknown,
                    not_this,
                ),
                (
                    "the first option",
                    &question,
                    PromptChoice::Option { index: 0 },
                    Ok(1),
                ),
                (
                    "the second option",
                    &question,
                    PromptChoice::Option { index: 1 },
                    Ok(2),
                ),
                (
                    "allow on a question",
                    &question,
                    PromptChoice::Allow,
                    not_this,
                ),
                (
                    "deny on a question",
                    &question,
                    PromptChoice::Deny,
                    not_this,
                ),
                (
                    "unknown on a question",
                    &question,
                    PromptChoice::Unknown,
                    not_this,
                ),
            ];
            for (what, prompt, choice, expected) in table {
                let got = chat_answer_for(choice, prompt)
                    .map_err(|refusal| (refusal.code, refusal.message));
                let expected = expected
                    .map(|number| ChatAnswerKind::Option { number })
                    .map_err(|(code, message)| (code, message.to_owned()));
                assert_eq!(got, expected, "{what}");
            }
        }

        #[tokio::test]
        async fn answer_prompt_types_the_digit_for_a_single_question() {
            let mut w = waiting(
                "dash-answer-question",
                ONE_QUESTION,
                question(&["Red", "Blue"], false),
            );
            w.answer(PromptChoice::Option { index: 1 }).await;
            w.types_and_ends(b"2", "the second option's digit").await;
        }

        #[tokio::test]
        async fn answer_prompt_types_allow_once_for_a_permission() {
            let mut w = waiting("dash-answer-allow", PERMISSION, bash_permission());
            w.answer(PromptChoice::Allow).await;
            w.types_and_ends(b"1", "the Yes option's digit").await;
            let record = w.hub.sessions.get("s1").expect("s1 is registered");
            assert_eq!(
                lock(&record).pending_input,
                None,
                "the dialog's going clears the prompt"
            );
        }

        #[tokio::test]
        async fn answer_prompt_types_the_deny_digit() {
            let mut w = waiting("dash-answer-deny", PERMISSION, bash_permission());
            w.answer(PromptChoice::Deny).await;
            w.types_and_ends(b"4", "the No option's digit, not Esc")
                .await;
        }

        #[tokio::test]
        async fn answer_prompt_refuses_a_changed_status_since() {
            let mut w = waiting("dash-answer-since", PERMISSION, bash_permission());
            let earlier = w.since - chrono::Duration::seconds(1);
            w.answer_at(earlier, PromptChoice::Allow).await;
            w.refused(CHAT_REFUSED, GONE, "an older status_since").await;
        }

        #[tokio::test]
        async fn answer_prompt_refuses_a_missing_dialog() {
            let mut w = waiting("dash-answer-no-dialog", NO_DIALOG, bash_permission());
            w.answer(PromptChoice::Allow).await;
            w.refused(CHAT_REFUSED, GONE, "no dialog on screen").await;

            let mut w = waiting(
                "dash-answer-other-dialog",
                PERMISSION,
                question(&["Red", "Blue"], false),
            );
            w.answer(PromptChoice::Option { index: 0 }).await;
            w.refused(CHAT_REFUSED, GONE, "a permission dialog for a question")
                .await;
        }

        #[tokio::test]
        async fn answer_prompt_refuses_a_multi_select() {
            let mut w = waiting(
                "dash-answer-multi",
                MULTI_SELECT,
                question(&["add", "mul", "edge cases", "property tests"], true),
            );
            w.answer(PromptChoice::Option { index: 0 }).await;
            w.refused(INVALID_REQUEST, NOT_THIS, "a multi-select question")
                .await;
        }

        #[tokio::test]
        async fn answer_prompt_refuses_an_unknown_choice() {
            let mut w = waiting("dash-answer-unknown", PERMISSION, bash_permission());
            w.answer(PromptChoice::Unknown).await;
            w.refused(
                INVALID_REQUEST,
                NOT_THIS,
                "a choice this build does not know",
            )
            .await;
        }

        #[tokio::test]
        async fn answer_prompt_works_without_a_chat_mirror() {
            let mut w = waiting("dash-answer-no-mirror", PERMISSION, bash_permission());
            assert_eq!(
                w.hub.chat_mirror.current_prompt("s1"),
                None,
                "no connection loads the chat"
            );
            w.answer(PromptChoice::Allow).await;
            w.types_and_ends(b"1", "the Yes option's digit").await;
        }

        #[tokio::test]
        async fn answer_prompt_refusal_carries_the_request_id() {
            let mut w = waiting("dash-answer-request-id", PERMISSION, bash_permission());
            send(
                &w.hub,
                &w.conn,
                answer_prompt("nope", w.since, PromptChoice::Allow),
            )
            .await;
            w.refused(
                NOT_FOUND,
                "unknown session: nope",
                "an unknown session, refused at once",
            )
            .await;
            w.answer_at(w.since + chrono::Duration::seconds(1), PromptChoice::Allow)
                .await;
            w.refused(
                CHAT_REFUSED,
                GONE,
                "a newer status_since, refused by the task",
            )
            .await;
        }

        /// The clock is paused so the steps run in timer order alone: the
        /// answer passes its checks, reads the screen and waits for the lane
        /// well inside the test's sleep, and the lane's wait outlasts it.
        #[tokio::test(start_paused = true)]
        async fn answer_prompt_refuses_when_since_changes_while_waiting_for_the_lane() {
            let mut w = waiting("dash-answer-lane-since", PERMISSION, bash_permission());
            let held = w
                .term
                .pty
                .typing()
                .try_take(Holder::Injector)
                .expect("a free lane");
            w.answer(PromptChoice::Allow).await;
            tokio::time::sleep(READ_QUIET * 3).await;
            w.hub.sessions.update("s1", |record| {
                record.status_since = Some(w.since + chrono::Duration::seconds(1));
            });
            w.term.show(PERMISSION).await;
            drop(held);
            w.refused(
                CHAT_REFUSED,
                GONE,
                "an identical dialog raised while the answer waited for the lane",
            )
            .await;
        }

        #[tokio::test]
        async fn answer_prompt_refuses_a_permission_whose_screen_shows_another_command() {
            let pending = PendingInput::Permission {
                tool_name: "Bash".to_owned(),
                summary: "git push origin main".to_owned(),
            };
            let mut w = waiting("dash-answer-other-command", PERMISSION, pending);
            w.answer(PromptChoice::Allow).await;
            w.refused(CHAT_REFUSED, GONE, "the dialog asks for another command")
                .await;

            let empty = PendingInput::Permission {
                tool_name: "Bash".to_owned(),
                summary: String::new(),
            };
            let mut w = waiting("dash-answer-no-summary", PERMISSION, empty);
            w.answer(PromptChoice::Allow).await;
            w.refused(CHAT_REFUSED, GONE, "a permission with no summary to match")
                .await;
        }

        #[tokio::test]
        async fn answer_prompt_refuses_a_question_whose_labels_differ() {
            let mut w = waiting(
                "dash-answer-other-labels",
                ONE_QUESTION,
                question(&["Red", "Green"], false),
            );
            w.answer(PromptChoice::Option { index: 0 }).await;
            w.refused(CHAT_REFUSED, GONE, "the dialog shows other options")
                .await;
        }

        #[tokio::test]
        async fn answer_prompt_refuses_an_option_past_the_list() {
            let mut w = waiting(
                "dash-answer-past-list",
                ONE_QUESTION,
                question(&["Red", "Blue"], false),
            );
            w.answer(PromptChoice::Option { index: 2 }).await;
            w.refused(INVALID_REQUEST, NOT_THIS, "the third of two options")
                .await;
        }
    }
}
