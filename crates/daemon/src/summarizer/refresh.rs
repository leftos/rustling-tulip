//! The working refresh: while at least one connection watches a Dashboard,
//! every [`REFRESH_TICK`] each interactive `Working` session whose last
//! assistant text changed gets one working summary, put at the front of its
//! summary ring as a provisional line that the next line replaces.
//!
//! The text is the one the Dashboard's facts pass last read, so only that
//! pass reads transcripts. A session's text is compared with the one last
//! summarized in its current turn; a session never refreshed, or whose stop
//! line changed since, counts its last `Stop` message's ending as summarized,
//! so an unchanged text makes no call. The sessions due on a tick are
//! refreshed one at a time; a session whose turn pipeline has reports is
//! skipped, when the tick comes and again when its call's turn comes, so a
//! refresh never stands in a stop line's way.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use protocol::{SessionMode, SessionStatus, SummaryLine};
use sha2::{Digest as _, Sha256};
use tokio::sync::watch;

use crate::dashboard::Dashboard;
use crate::session::SessionRegistry;
use crate::summarizer::policy::Line;
use crate::summarizer::prompt;
use crate::summarizer::service::{Outcome, Summarizer};
use crate::summarizer::turns::Turns;
use crate::sync::lock;

/// How often the watched sessions are refreshed.
pub(crate) const REFRESH_TICK: Duration = Duration::from_secs(180);

/// The SHA-256 of a shaped input.
type Digest = [u8; 32];

/// What a refresh reads of a session before its call.
struct Due {
    input: String,
    digest: Digest,
    /// The latest stop line's stamp when the text was read; a refresh whose
    /// session got a newer stop line meanwhile is dropped.
    stop_line_stamp: Option<DateTime<Utc>>,
}

/// What a session's text is compared with.
struct Seen {
    /// The session's stop-line stamp when this was recorded; a different
    /// one means a turn ended since, and the entry is seeded again.
    stop_line_stamp: Option<DateTime<Utc>>,
    /// The digest of the shaped input last summarized, or seeded from the
    /// last `Stop` message; `None` when there is neither.
    digest: Option<Digest>,
}

/// The working refresh the hub runs.
pub(crate) struct Refresh {
    sessions: Arc<SessionRegistry>,
    dashboard: Dashboard,
    summarizer: Arc<Summarizer>,
    turns: Arc<Turns>,
    watchers: watch::Receiver<usize>,
    /// Each session's last summarized text in its current turn. Kept across
    /// watch gaps; a session gone is forgotten.
    seen: Mutex<HashMap<String, Seen>>,
}

/// Start the working refresh for the daemon's life: it parks while no
/// connection watches a Dashboard, and refreshes every [`REFRESH_TICK`]
/// while one does, the first time one tick after the first watcher came.
pub(crate) fn spawn(
    sessions: Arc<SessionRegistry>,
    dashboard: Dashboard,
    summarizer: Arc<Summarizer>,
    turns: Arc<Turns>,
) -> tokio::task::JoinHandle<()> {
    let mut count = dashboard.watcher_count();
    let refresh = Refresh {
        watchers: dashboard.watcher_count(),
        sessions,
        dashboard,
        summarizer,
        turns,
        seen: Mutex::default(),
    };
    tokio::spawn(async move {
        loop {
            if count.wait_for(|watchers| *watchers > 0).await.is_err() {
                return;
            }
            let start = tokio::time::Instant::now() + REFRESH_TICK;
            let mut interval = tokio::time::interval_at(start, REFRESH_TICK);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = interval.tick() => refresh.pass().await,
                    changed = count.changed() => {
                        if changed.is_err() {
                            return;
                        }
                        if *count.borrow_and_update() == 0 {
                            break;
                        }
                    }
                }
            }
        }
    })
}

impl Refresh {
    /// Refresh every session due, one at a time.
    async fn pass(&self) {
        let ids: Vec<String> = self
            .sessions
            .snapshots()
            .into_iter()
            .filter(|snapshot| snapshot.status == SessionStatus::Working)
            .map(|snapshot| snapshot.id)
            .collect();
        lock(&self.seen).retain(|id, _| self.sessions.get(id).is_some());
        for id in ids {
            self.refresh(&id).await;
        }
    }

    /// Refresh `id` when it is due: make its call, and put the line in its
    /// ring as provisional unless the session moved on meanwhile.
    async fn refresh(&self, id: &str) {
        let Some(due) = self.due(id) else {
            return;
        };
        // A stop report that came in, or the last watcher leaving, while the
        // request waited would make publish drop the line: no call then.
        let still_due = || !self.turns.has_reports(id) && *self.watchers.borrow() > 0;
        let outcome = self
            .summarizer
            .submit_refresh(id, due.input, still_due)
            .await;
        let Outcome::Line { line, .. } = outcome else {
            return;
        };
        if self.publish(id, line, due.stop_line_stamp) {
            let seen = Seen {
                stop_line_stamp: due.stop_line_stamp,
                digest: Some(due.digest),
            };
            lock(&self.seen).insert(id.to_owned(), seen);
        }
    }

    /// What `id`'s refresh sends, when it is an interactive `Working`
    /// session the Dashboard lists, its turn pipeline has no reports, and
    /// its last assistant text changed since the last summarized one.
    fn due(&self, id: &str) -> Option<Due> {
        if self.turns.has_reports(id) {
            return None;
        }
        let text = self.dashboard.last_assistant_text(id)?;
        let (last_message, stop_line_stamp) = {
            let record = self.sessions.get(id)?;
            let record = lock(&record);
            if record.mode != SessionMode::Interactive || record.status != SessionStatus::Working {
                return None;
            }
            (
                record.last_assistant_message.clone(),
                record.stop_line_stamp(),
            )
        };
        let text = text.trim();
        let input = prompt::shape(&[text]);
        let digest = digest_of(&input);
        let mut seen = lock(&self.seen);
        let stale = seen
            .get(id)
            .is_none_or(|entry| entry.stop_line_stamp != stop_line_stamp);
        if stale {
            // The reader's text is the message's last text block, so the
            // text was summarized when the stop's message ends with it.
            let seeded = last_message
                .filter(|message| message.trim().ends_with(text))
                .map(|_| digest);
            let entry = Seen {
                stop_line_stamp,
                digest: seeded,
            };
            seen.insert(id.to_owned(), entry);
        }
        let last = seen.get(id).and_then(|entry| entry.digest);
        (last != Some(digest)).then_some(Due {
            input,
            digest,
            stop_line_stamp,
        })
    }

    /// Put `line` at the front of `id`'s ring as provisional, stamped now,
    /// while a Dashboard is still watched and, under the record lock, the
    /// session is still `Working` with no stop line newer than
    /// `stop_line_stamp`. Returns whether it was put there. Never written to
    /// `summary.json` and never alerted.
    fn publish(&self, id: &str, line: Line, stop_line_stamp: Option<DateTime<Utc>>) -> bool {
        if *self.watchers.borrow() == 0 {
            return false;
        }
        let mut published = false;
        self.sessions.update_if_changed(id, |record| {
            if record.status != SessionStatus::Working
                || record.stop_line_stamp() != stop_line_stamp
            {
                return false;
            }
            let now = Utc::now();
            let ring_line = SummaryLine {
                text: line.notification,
                at: now,
                source: line.source,
                provisional: true,
            };
            record.push_provisional_line(ring_line, now);
            published = true;
            true
        });
        published
    }
}

/// The SHA-256 of `input`.
fn digest_of(input: &str) -> Digest {
    Sha256::digest(input.as_bytes()).into()
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests assert preconditions with expect")]
mod tests {
    use std::sync::Arc;

    use agent_api::ActivitySnapshot;
    use chrono::Utc;
    use protocol::{
        DaemonMessage, SessionMember, SessionMode, SessionStatus, SessionSummary, SummarySource,
    };
    use tokio::sync::{Notify, mpsc};

    use super::REFRESH_TICK;
    use crate::dashboard::Dashboard;
    use crate::dashboard::test_support::Script;
    use crate::server::Hub;
    use crate::server::test_support::{ScratchDir, test_hub, test_summarizer};
    use crate::session::SessionEvent;
    use crate::summarizer::prompt::{self, Trigger};
    use crate::summarizer::service::test_support::{FAKE_NOTIFICATION, FakeCaller};
    use crate::summarizer::service::{Outcome, Request};
    use crate::summarizer::turns::Turns;

    /// A hub whose provider is `fake` with one client connected, whose
    /// Dashboard reads through the returned script.
    fn hub_with(tag: &str, fake: &Arc<FakeCaller>) -> (Hub, ScratchDir, Script) {
        let (mut hub, scratch) = test_hub(tag);
        hub.client_count.send_replace(1);
        hub.summarizer = test_summarizer(
            &hub.state,
            &hub.dirs,
            &hub.client_count,
            &hub.state_events,
            Arc::clone(fake) as Arc<dyn crate::summarizer::service::Caller>,
        );
        hub.turns = Turns::new(Arc::clone(&hub.sessions), Arc::clone(&hub.summarizer));
        let script = Script::default();
        hub.dashboard = Dashboard::with_reader_factory(
            Arc::clone(&hub.sessions),
            hub.dirs.activity_dir(),
            script.factory(),
        )
        .with_pr_program(crate::dashboard::pr::NO_PROGRAM);
        (hub, scratch, script)
    }

    /// Add listed session `id` in `mode` and `status`, its transcript's last
    /// assistant text `text`, with a sidecar.
    fn add_session(
        hub: &Hub,
        script: &Script,
        id: &str,
        mode: SessionMode,
        status: SessionStatus,
        text: &str,
    ) {
        crate::history::test_support::write_meta_for(&hub.dirs, id);
        let mut record = crate::history::test_support::record(id, mode);
        record.conversation_id = Some(format!("conv-{id}"));
        record.members = vec![SessionMember {
            repo_id: "r1".to_owned(),
            repo_name: "repo".to_owned(),
            branch: "main".to_owned(),
            worktree_path: "C:/work/repo".to_owned(),
        }];
        record.status = status;
        hub.sessions.insert(record);
        set_text(script, id, text);
    }

    fn set_text(script: &Script, id: &str, text: &str) {
        script.set(
            &format!("conv-{id}"),
            ActivitySnapshot {
                last_assistant_text: Some(text.to_owned()),
                ..ActivitySnapshot::default()
            },
        );
    }

    /// Make connection `connection` watch; its messages land on the
    /// returned queue, which the test keeps so the watch stays open.
    async fn watch(hub: &Hub, connection: u64) -> mpsc::UnboundedReceiver<DaemonMessage> {
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        hub.dashboard.watch(connection, out_tx).await;
        out_rx
    }

    fn spawn_refresh(hub: &Hub) -> tokio::task::JoinHandle<()> {
        super::spawn(
            Arc::clone(&hub.sessions),
            hub.dashboard.clone(),
            Arc::clone(&hub.summarizer),
            Arc::clone(&hub.turns),
        )
    }

    /// Let the tasks run; yields only, so a passing run takes no wall time.
    async fn settle() {
        for _ in 0..10_000 {
            tokio::task::yield_now().await;
        }
    }

    /// Let the tasks run until `cond` holds; returns whether it did.
    async fn settles(cond: impl Fn() -> bool) -> bool {
        for _ in 0..100_000 {
            if cond() {
                return true;
            }
            tokio::task::yield_now().await;
        }
        cond()
    }

    /// Wait out one refresh tick and let its pass run.
    async fn next_tick() {
        tokio::time::sleep(REFRESH_TICK).await;
        settle().await;
    }

    fn summary(hub: &Hub, id: &str) -> Option<SessionSummary> {
        crate::sync::lock(&hub.sessions.get(id).expect("the session"))
            .summary
            .clone()
    }

    /// Apply the hook `body` to `s1` at `event_time`, as the hook route does.
    fn apply_hook(hub: &Hub, body: &str, event_time: u128) {
        let event: serde_json::Value = serde_json::from_str(body).expect("parse hook body");
        let report = agent_registry::plugin(&agent_registry::default_agent())
            .expect("Claude is registered")
            .map_hook_event(&event, None, false)
            .expect("the body maps to a report");
        crate::hook_status::apply_signal(hub, "s1", Some(event_time), &report);
    }

    fn stop_hook(message: &str) -> String {
        serde_json::json!({"hook_event_name": "Stop", "last_assistant_message": message})
            .to_string()
    }

    #[tokio::test(start_paused = true)]
    async fn an_unchanged_transcript_makes_no_call() {
        let fake = Arc::new(FakeCaller::default());
        let (hub, _scratch, script) = hub_with("refresh-unchanged", &fake);
        add_session(
            &hub,
            &script,
            "s1",
            SessionMode::Interactive,
            SessionStatus::Working,
            "I fixed the bug.\n",
        );
        hub.sessions.update("s1", |record| {
            record.last_assistant_message = Some("I fixed the bug.".to_owned());
        });
        let _watch = watch(&hub, 1).await;
        let task = spawn_refresh(&hub);

        for _ in 0..3 {
            next_tick().await;
        }
        assert_eq!(fake.calls(), 0, "the last stop's text makes no call");
        assert!(summary(&hub, "s1").is_none());

        set_text(&script, "s1", "Now the tests.");
        hub.dashboard.tick(Utc::now()).await;
        next_tick().await;
        assert!(settles(|| fake.calls() == 1).await, "a new text makes one");
        for _ in 0..3 {
            next_tick().await;
        }
        assert_eq!(fake.calls(), 1, "and the same text again makes none");
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn a_grown_transcript_makes_one_call() {
        let fake = Arc::new(FakeCaller::default());
        let (hub, _scratch, script) = hub_with("refresh-grown", &fake);
        add_session(
            &hub,
            &script,
            "s1",
            SessionMode::Interactive,
            SessionStatus::Working,
            "Reading the code.",
        );
        let mut events = hub.sessions.subscribe();
        let _watch = watch(&hub, 1).await;
        let task = spawn_refresh(&hub);

        next_tick().await;
        assert!(
            settles(|| summary(&hub, "s1").is_some()).await,
            "a line lands"
        );
        assert_eq!(fake.calls(), 1);
        assert!(
            fake.prompts()[0].contains("Reading the code."),
            "the call carries the text"
        );
        assert!(
            fake.prompts()[0].contains(Trigger::Working.as_str()),
            "as a working refresh"
        );
        let summary = summary(&hub, "s1").expect("a summary");
        assert_eq!(summary.lines.len(), 1);
        assert_eq!(summary.lines[0].text, FAKE_NOTIFICATION);
        assert!(summary.lines[0].provisional);
        assert_eq!(summary.kind, None, "the latest summary is left alone");
        let status = crate::sync::lock(&hub.sessions.get("s1").expect("the session")).status;
        assert_eq!(status, SessionStatus::Working);
        while let Ok(event) = events.try_recv() {
            assert!(
                !matches!(event, SessionEvent::WaitingAlert { .. }),
                "a refresh never alerts"
            );
        }
        assert!(
            !crate::paths::session_summary_file(&hub.dirs, "s1").exists(),
            "a provisional line is not saved"
        );

        next_tick().await;
        assert_eq!(fake.calls(), 1, "the same text makes no second call");
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn a_second_refresh_replaces_the_first_provisional_line() {
        let fake = Arc::new(FakeCaller::default());
        let (hub, _scratch, script) = hub_with("refresh-second", &fake);
        add_session(
            &hub,
            &script,
            "s1",
            SessionMode::Interactive,
            SessionStatus::Working,
            "Step one.",
        );
        let _watch = watch(&hub, 1).await;
        let task = spawn_refresh(&hub);
        next_tick().await;
        assert!(settles(|| summary(&hub, "s1").is_some()).await);

        set_text(&script, "s1", "Step two.");
        hub.dashboard.tick(Utc::now()).await;
        next_tick().await;
        assert!(
            settles(|| fake.calls() == 2).await,
            "the new text is refreshed"
        );
        settle().await;
        let summary = summary(&hub, "s1").expect("a summary");
        assert_eq!(summary.lines.len(), 1, "the second replaced the first");
        assert!(summary.lines[0].provisional);
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn no_refresh_runs_with_no_dashboard_watcher() {
        let fake = Arc::new(FakeCaller::default());
        let (hub, _scratch, script) = hub_with("refresh-no-watcher", &fake);
        add_session(
            &hub,
            &script,
            "s1",
            SessionMode::Interactive,
            SessionStatus::Working,
            "Text one.",
        );
        let task = spawn_refresh(&hub);
        for _ in 0..3 {
            next_tick().await;
        }
        assert_eq!(fake.calls(), 0, "nothing runs before a watcher");

        let first = watch(&hub, 1).await;
        next_tick().await;
        assert!(settles(|| fake.calls() == 1).await, "a watcher starts it");

        hub.dashboard.unwatch(1);
        drop(first);
        set_text(&script, "s1", "Text two.");
        for _ in 0..3 {
            next_tick().await;
        }
        assert_eq!(fake.calls(), 1, "nothing runs after the last watcher left");

        let _second = watch(&hub, 2).await;
        next_tick().await;
        assert!(
            settles(|| fake.calls() == 2).await,
            "a new watcher starts it again"
        );
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn no_refresh_runs_past_half_the_cap() {
        let fake = Arc::new(FakeCaller::billing(0.6));
        let (hub, _scratch, script) = hub_with("refresh-half-cap", &fake);
        let cap = hub.state.summarizer_settings().daily_cap_usd;
        assert!(cap > 0.6 && cap / 2.0 < 0.6, "0.6 is past half the cap");
        let spent = hub
            .summarizer
            .submit(Request {
                session_id: "other".to_owned(),
                trigger: Trigger::Stop,
                input: prompt::shape(&["A report."]),
            })
            .await;
        assert!(matches!(spent, Outcome::Line { .. }));
        add_session(
            &hub,
            &script,
            "s1",
            SessionMode::Interactive,
            SessionStatus::Working,
            "Working on it.",
        );
        let _watch = watch(&hub, 1).await;
        let task = spawn_refresh(&hub);
        for _ in 0..3 {
            next_tick().await;
        }
        assert_eq!(fake.calls(), 1, "only the stop called");
        assert!(summary(&hub, "s1").is_none(), "and no template line is put");
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_line_replaces_the_provisional_line() {
        let fake = Arc::new(FakeCaller::default());
        let (hub, _scratch, script) = hub_with("refresh-stop-replaces", &fake);
        add_session(
            &hub,
            &script,
            "s1",
            SessionMode::Interactive,
            SessionStatus::Working,
            "Halfway there.",
        );
        let _watch = watch(&hub, 1).await;
        let task = spawn_refresh(&hub);
        next_tick().await;
        assert!(settles(|| summary(&hub, "s1").is_some()).await);

        apply_hook(&hub, &stop_hook("All done."), 100);
        assert!(
            settles(|| summary(&hub, "s1").is_some_and(|summary| !summary.lines[0].provisional))
                .await,
            "the stop line lands"
        );
        let summary = summary(&hub, "s1").expect("a summary");
        assert_eq!(summary.lines.len(), 1, "it replaced the provisional line");
        assert_eq!(summary.source, SummarySource::Model);
        assert_eq!(fake.calls(), 2);
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn a_refresh_finishing_after_a_stop_line_is_dropped() {
        let gate = Arc::new(Notify::new());
        let fake = Arc::new(FakeCaller::gated(Arc::clone(&gate)));
        let (hub, _scratch, script) = hub_with("refresh-late", &fake);
        add_session(
            &hub,
            &script,
            "s1",
            SessionMode::Interactive,
            SessionStatus::Working,
            "Halfway there.",
        );
        let _watch = watch(&hub, 1).await;
        let task = spawn_refresh(&hub);
        tokio::time::sleep(REFRESH_TICK).await;
        assert!(
            settles(|| fake.calls() == 1).await,
            "the refresh is in flight"
        );

        // A stop line lands while the session is working again.
        hub.sessions.update("s1", |record| {
            let line = protocol::SummaryLine {
                text: "The stop line.".to_owned(),
                at: Utc::now(),
                source: SummarySource::Model,
                provisional: false,
            };
            record.push_summary_line(None, "Stop headline".to_owned(), line, Utc::now());
        });
        gate.notify_one();
        settle().await;

        let summary = summary(&hub, "s1").expect("a summary");
        assert_eq!(summary.lines.len(), 1, "the refresh's line was dropped");
        assert_eq!(summary.lines[0].text, "The stop line.");
        assert_eq!(summary.headline, "Stop headline");
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn a_refresh_admitted_after_a_stop_makes_no_call() {
        let gate = Arc::new(Notify::new());
        let fake = Arc::new(FakeCaller::gated(Arc::clone(&gate)));
        let (hub, _scratch, script) = hub_with("refresh-after-stop", &fake);
        add_session(
            &hub,
            &script,
            "s1",
            SessionMode::Interactive,
            SessionStatus::Working,
            "Halfway there.",
        );
        let _watch = watch(&hub, 1).await;
        let summarizer = Arc::clone(&hub.summarizer);
        let stop = tokio::spawn(async move {
            summarizer
                .submit(Request {
                    session_id: "s1".to_owned(),
                    trigger: Trigger::Stop,
                    input: prompt::shape(&["A stop report."]),
                })
                .await
        });
        assert!(settles(|| fake.calls() == 1).await, "the stop is in flight");
        let task = spawn_refresh(&hub);
        tokio::time::sleep(REFRESH_TICK).await;
        settle().await;

        // The refresh waits behind the stop; the turn's next report comes in
        // before the refresh's turn does.
        assert!(hub.turns.add_report("s1", "The turn's report."));
        gate.notify_one();
        stop.await.expect("the stop request ends");
        settle().await;
        assert_eq!(fake.calls(), 1, "the refresh made no call");
        assert!(summary(&hub, "s1").is_none(), "and put no line");
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_turn_does_not_resummarize_the_last_turns_ending() {
        let fake = Arc::new(FakeCaller::default());
        let (hub, _scratch, script) = hub_with("refresh-new-turn", &fake);
        add_session(
            &hub,
            &script,
            "s1",
            SessionMode::Interactive,
            SessionStatus::Working,
            "Turn one, halfway.",
        );
        let _watch = watch(&hub, 1).await;
        let task = spawn_refresh(&hub);
        next_tick().await;
        assert!(settles(|| fake.calls() == 1).await, "turn one is refreshed");

        apply_hook(&hub, &stop_hook("Turn one is done."), 100);
        assert!(
            settles(|| summary(&hub, "s1").is_some_and(|summary| !summary.lines[0].provisional))
                .await,
            "turn one's stop line lands"
        );
        assert_eq!(fake.calls(), 2);

        apply_hook(&hub, r#"{"hook_event_name":"UserPromptSubmit"}"#, 200);
        set_text(&script, "s1", "Turn one is done.\n");
        hub.dashboard.tick(Utc::now()).await;
        for _ in 0..3 {
            next_tick().await;
        }
        assert_eq!(fake.calls(), 2, "turn one's ending is not summarized again");
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn a_final_message_with_several_text_blocks_still_seeds() {
        let fake = Arc::new(FakeCaller::default());
        let (hub, _scratch, script) = hub_with("refresh-text-blocks", &fake);
        add_session(
            &hub,
            &script,
            "s1",
            SessionMode::Interactive,
            SessionStatus::Working,
            "I fixed the bug.\n",
        );
        hub.sessions.update("s1", |record| {
            record.last_assistant_message =
                Some("Let me check the logs.\n\nI fixed the bug.".to_owned());
        });
        let _watch = watch(&hub, 1).await;
        let task = spawn_refresh(&hub);
        for _ in 0..3 {
            next_tick().await;
        }
        assert_eq!(
            fake.calls(),
            0,
            "the reader's last text block ends the stop's message"
        );
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn refresh_skips_a_session_whose_turn_pipeline_has_work() {
        let fake = Arc::new(FakeCaller::default());
        let (hub, _scratch, script) = hub_with("refresh-skips-turns", &fake);
        add_session(
            &hub,
            &script,
            "s1",
            SessionMode::Interactive,
            SessionStatus::Working,
            "Working on it.",
        );
        hub.client_count.send_replace(0);
        hub.turns.on_stop("s1", "a report");
        assert!(
            settles(|| hub.turns.state_of("s1") == Some((1, false))).await,
            "the report waits"
        );
        hub.client_count.send_replace(1);
        let _watch = watch(&hub, 1).await;
        let task = spawn_refresh(&hub);
        for _ in 0..3 {
            next_tick().await;
        }
        assert_eq!(fake.calls(), 0, "the pending report goes first");
        assert_eq!(hub.turns.state_of("s1"), Some((1, false)));
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn headless_and_idle_sessions_are_not_refreshed() {
        let fake = Arc::new(FakeCaller::default());
        let (hub, _scratch, script) = hub_with("refresh-not-due", &fake);
        let skipped = [
            ("headless", SessionMode::Headless, SessionStatus::Working),
            ("idle", SessionMode::Interactive, SessionStatus::Idle),
            (
                "asking",
                SessionMode::Interactive,
                SessionStatus::AwaitingInput,
            ),
        ];
        for (id, mode, status) in skipped {
            add_session(&hub, &script, id, mode, status, "Some text.");
        }
        let _watch = watch(&hub, 1).await;
        let task = spawn_refresh(&hub);
        for _ in 0..3 {
            next_tick().await;
        }
        assert_eq!(fake.calls(), 0);
        for (id, _, _) in skipped {
            assert!(summary(&hub, id).is_none(), "{id} has no line");
        }

        add_session(
            &hub,
            &script,
            "working",
            SessionMode::Interactive,
            SessionStatus::Working,
            "Some text.",
        );
        hub.dashboard.tick(Utc::now()).await;
        next_tick().await;
        assert!(
            settles(|| fake.calls() == 1).await,
            "an interactive working session is refreshed"
        );
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn a_session_removed_while_its_refresh_is_in_flight_writes_nothing() {
        let gate = Arc::new(Notify::new());
        let fake = Arc::new(FakeCaller::gated(Arc::clone(&gate)));
        let (hub, _scratch, script) = hub_with("refresh-removed", &fake);
        add_session(
            &hub,
            &script,
            "s1",
            SessionMode::Interactive,
            SessionStatus::Working,
            "Halfway there.",
        );
        let mut events = hub.sessions.subscribe();
        let _watch = watch(&hub, 1).await;
        let task = spawn_refresh(&hub);
        tokio::time::sleep(REFRESH_TICK).await;
        assert!(
            settles(|| fake.calls() == 1).await,
            "the refresh is in flight"
        );

        hub.sessions.delete_sidecar("s1");
        hub.sessions.delete_session_dir("s1");
        hub.sessions.remove("s1");
        gate.notify_one();
        settle().await;
        while let Ok(event) = events.try_recv() {
            if let SessionEvent::Updated(snapshot, _) = &event {
                assert!(snapshot.summary.is_none(), "no update carries a line");
            }
        }
        assert!(
            !hub.dirs.sessions_dir.join("s1").exists(),
            "no file or dir is made"
        );
        task.abort();
    }

    #[tokio::test]
    async fn the_text_accessor_is_none_after_the_last_unwatch() {
        let fake = Arc::new(FakeCaller::default());
        let (hub, _scratch, script) = hub_with("refresh-accessor", &fake);
        add_session(
            &hub,
            &script,
            "s1",
            SessionMode::Interactive,
            SessionStatus::Working,
            "The last text.",
        );
        assert_eq!(hub.dashboard.last_assistant_text("s1"), None, "never read");
        let _first = watch(&hub, 1).await;
        let _second = watch(&hub, 2).await;
        assert_eq!(
            hub.dashboard.last_assistant_text("s1").as_deref(),
            Some("The last text.")
        );
        hub.dashboard.unwatch(1);
        assert!(
            hub.dashboard.last_assistant_text("s1").is_some(),
            "one watcher is left"
        );
        hub.dashboard.unwatch(2);
        assert_eq!(
            hub.dashboard.last_assistant_text("s1"),
            None,
            "the last unwatch"
        );
    }
}
