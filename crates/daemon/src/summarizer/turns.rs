//! The turn pipeline: each main-agent `Stop`'s last message joins its
//! session's unsummarized reports, and a per-session driver summarizes them
//! through [`Summarizer::submit`] into the session's summary ring.
//!
//! A call starts at once unless the session's previous call finished under
//! [`COALESCE_WINDOW`] ago; it then starts when the window is up and covers
//! every report since. One call runs per session at a time. A call that made
//! no line (no client connected, or replaced by another request) leaves its
//! reports pending for the next `Stop`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use protocol::{SessionStatus, SummaryLine};
use tokio::time::Instant;

use crate::hook_status::truncate_bytes;
use crate::session::{SessionEvent, SessionRegistry};
use crate::summarizer::policy::Line;
use crate::summarizer::prompt::{self, Trigger};
use crate::summarizer::service::{Outcome, Request, Summarizer};
use crate::sync::lock;

/// How long after a session's call finishes its next call waits.
pub(crate) const COALESCE_WINDOW: Duration = Duration::from_secs(60);

/// Bytes of one report kept while it waits.
const REPORT_MAX_BYTES: usize = 8 * 1024;

/// Most reports one session holds; the oldest goes first.
const PENDING_MAX_REPORTS: usize = 12;

/// One finished turn's last message and when its `Stop` was applied.
#[derive(Debug, Clone)]
struct Report {
    text: String,
    at: DateTime<Utc>,
}

/// One session's unsummarized reports and its call state.
#[derive(Debug, Default)]
struct Driver {
    /// Oldest first.
    pending: Vec<Report>,
    /// A driver task runs for the session.
    running: bool,
    /// When the session's last call finished.
    finished_at: Option<Instant>,
}

/// The turn pipeline the hub holds.
pub(crate) struct Turns {
    sessions: Arc<SessionRegistry>,
    summarizer: Arc<Summarizer>,
    drivers: Mutex<HashMap<String, Driver>>,
}

impl Turns {
    /// The pipeline over `sessions`, which from now on asks it whether a
    /// session's stop line is on its way (see
    /// [`SessionRegistry::set_stop_line_due`]).
    pub(crate) fn new(sessions: Arc<SessionRegistry>, summarizer: Arc<Summarizer>) -> Arc<Self> {
        let turns = Arc::new(Self {
            sessions,
            summarizer,
            drivers: Mutex::default(),
        });
        let weak = Arc::downgrade(&turns);
        turns.sessions.set_stop_line_due(Box::new(move |id| {
            weak.upgrade().is_some_and(|turns| turns.has_work(id))
        }));
        turns
    }

    /// Add a main-agent `Stop`'s last message to `session_id`'s reports, and
    /// start its driver when none runs, as the hook route does in two steps.
    #[cfg(test)]
    pub(crate) fn on_stop(self: &Arc<Self>, session_id: &str, message: &str) {
        if self.add_report(session_id, message) {
            self.start(session_id);
        }
    }

    /// Add a main-agent `Stop`'s last message to `session_id`'s reports.
    /// Returns whether no driver runs for it, so the caller is to
    /// [`Self::start`] one; it is marked running already. Runs under the
    /// session's record lock, so it takes no registry lock.
    pub(crate) fn add_report(&self, session_id: &str, message: &str) -> bool {
        let report = Report {
            text: truncate_bytes(message, REPORT_MAX_BYTES),
            at: Utc::now(),
        };
        let mut drivers = lock(&self.drivers);
        let driver = drivers.entry(session_id.to_owned()).or_default();
        driver.pending.push(report);
        if driver.pending.len() > PENDING_MAX_REPORTS {
            driver.pending.remove(0);
        }
        !std::mem::replace(&mut driver.running, true)
    }

    /// Start the driver [`Self::add_report`] asked for, first forgetting the
    /// sessions the registry no longer holds. Called with no record lock held.
    pub(crate) fn start(self: &Arc<Self>, session_id: &str) {
        self.forget_gone_sessions();
        let turns = Arc::clone(self);
        let id = session_id.to_owned();
        tokio::spawn(async move { turns.drive(&id).await });
    }

    /// Drop the entries of sessions the registry no longer holds. The
    /// registry is asked with the pipeline's lock released, so the pipeline's
    /// lock is never held while a registry lock is taken.
    fn forget_gone_sessions(&self) {
        let tracked: Vec<String> = lock(&self.drivers).keys().cloned().collect();
        let gone: Vec<String> = tracked
            .into_iter()
            .filter(|id| self.sessions.get(id).is_none())
            .collect();
        if gone.is_empty() {
            return;
        }
        let mut drivers = lock(&self.drivers);
        for id in &gone {
            drivers.remove(id);
        }
    }

    /// Whether `id`'s driver runs, so a stop line is on its way: a call is
    /// in flight or about to start. Reports put back with the driver stopped
    /// wait for the next `Stop` and do not count. Asked under the session's
    /// record lock, so it takes no registry lock.
    pub(crate) fn has_work(&self, id: &str) -> bool {
        lock(&self.drivers)
            .get(id)
            .is_some_and(|driver| driver.running)
    }

    /// Whether `id` has a driver running or reports pending, so its next
    /// summary is a stop line: a refresh does not run ahead of it.
    pub(crate) fn has_reports(&self, id: &str) -> bool {
        lock(&self.drivers)
            .get(id)
            .is_some_and(|driver| driver.running || !driver.pending.is_empty())
    }

    /// Summarize `id`'s reports until none is pending, one call at a time.
    async fn drive(&self, id: &str) {
        loop {
            if let Some(ready) = self.ready_at(id) {
                tokio::time::sleep_until(ready).await;
            }
            let Some(reports) = self.take_pending(id) else {
                return;
            };
            if self.sessions.get(id).is_none() {
                lock(&self.drivers).remove(id);
                return;
            }
            let texts: Vec<&str> = reports.iter().map(|report| report.text.as_str()).collect();
            let request = Request {
                session_id: id.to_owned(),
                trigger: Trigger::Stop,
                input: prompt::shape(&texts),
            };
            match self.summarizer.submit(request).await {
                Outcome::Line { line, cost_usd } => {
                    let at = reports.last().map_or_else(Utc::now, |report| report.at);
                    if !self.publish(id, line, at) {
                        lock(&self.drivers).remove(id);
                        return;
                    }
                    // Only a billed call opens the window: a template line or
                    // a cache hit cost nothing, so the next stop goes at once.
                    if cost_usd > 0.0
                        && let Some(driver) = lock(&self.drivers).get_mut(id)
                    {
                        driver.finished_at = Some(Instant::now());
                    }
                }
                // A stop request is never skipped; were it, its reports wait.
                Outcome::Idle | Outcome::Replaced | Outcome::Skipped => {
                    self.put_back(id, reports);
                    return;
                }
            }
        }
    }

    /// When `id`'s next call may start, if not at once.
    fn ready_at(&self, id: &str) -> Option<Instant> {
        let finished_at = lock(&self.drivers).get(id)?.finished_at?;
        let ready = finished_at + COALESCE_WINDOW;
        (ready > Instant::now()).then_some(ready)
    }

    /// Take `id`'s pending reports for a call; with none, the driver stops.
    fn take_pending(&self, id: &str) -> Option<Vec<Report>> {
        let mut drivers = lock(&self.drivers);
        let driver = drivers.get_mut(id)?;
        if driver.pending.is_empty() {
            driver.running = false;
            return None;
        }
        Some(std::mem::take(&mut driver.pending))
    }

    /// Return `reports`, which no line covered, ahead of the ones that came
    /// since, and stop the driver; a session already gone is forgotten.
    fn put_back(&self, id: &str, mut reports: Vec<Report>) {
        let gone = self.sessions.get(id).is_none();
        let mut drivers = lock(&self.drivers);
        if gone {
            drivers.remove(id);
            return;
        }
        let Some(driver) = drivers.get_mut(id) else {
            return;
        };
        reports.append(&mut driver.pending);
        let excess = reports.len().saturating_sub(PENDING_MAX_REPORTS);
        reports.drain(..excess);
        driver.pending = reports;
        driver.running = false;
    }

    /// Put `line` into `id`'s ring and decide its alert in one registry
    /// update, whose broadcast the alert follows when the session waits; then
    /// write its `summary.json`. Returns whether the registry still holds the
    /// session.
    fn publish(&self, id: &str, line: Line, at: DateTime<Utc>) -> bool {
        let held = self.sessions.update_then(id, |record| {
            let ring_line = SummaryLine {
                text: line.notification.clone(),
                at,
                source: line.source,
                provisional: false,
            };
            record.push_summary_line(line.kind, line.headline, ring_line, Utc::now());
            let waiting = matches!(
                record.status,
                SessionStatus::Idle | SessionStatus::AwaitingInput
            );
            waiting.then(|| SessionEvent::WaitingAlert {
                session_id: id.to_owned(),
                kind: line.kind,
                notification: line.notification,
                source: line.source,
            })
        });
        if held {
            self.sessions.sync_summary(id);
        }
        held
    }

    /// `id`'s pending report count and whether its driver runs, while the
    /// pipeline tracks it.
    #[cfg(test)]
    pub(crate) fn state_of(&self, id: &str) -> Option<(usize, bool)> {
        lock(&self.drivers)
            .get(id)
            .map(|driver| (driver.pending.len(), driver.running))
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests assert preconditions with expect")]
mod tests {
    use std::time::Duration;

    use protocol::{SessionStatus, SummarizerSettings, SummarySource};
    use tokio::sync::broadcast;

    use crate::server::Hub;
    use crate::server::test_support::billing_hub;
    use crate::session::SessionEvent;
    use crate::summarizer::service::test_support::FAKE_NOTIFICATION;

    /// Hang guard for one awaited event; a passing run never waits it out.
    const HANG_GUARD: Duration = Duration::from_secs(600);

    /// Hang guard for a thread blocked on a lock; a passing run never waits
    /// it out, and a failing one would wait forever without it.
    const LOCK_HANG_GUARD: Duration = Duration::from_secs(60);

    /// Apply the hook `body` to `s1` at `event_time`, as the hook route does.
    fn apply_hook(hub: &Hub, body: &str, event_time: u128) {
        let event: serde_json::Value = serde_json::from_str(body).expect("parse hook body");
        let report = agent_registry::plugin(&agent_registry::default_agent())
            .expect("Claude is registered")
            .map_hook_event(&event, None, false)
            .expect("the body maps to a report");
        crate::hook_status::apply_signal(hub, "s1", Some(event_time), &report);
    }

    fn stop(message: &str) -> String {
        serde_json::json!({"hook_event_name": "Stop", "last_assistant_message": message})
            .to_string()
    }

    /// The next registry event that is a `SessionUpdated` carrying a summary,
    /// or a `WaitingAlert`, skipping the rest.
    async fn next_summary_event(events: &mut broadcast::Receiver<SessionEvent>) -> SessionEvent {
        tokio::time::timeout(HANG_GUARD, async {
            loop {
                match events.recv().await.expect("the registry is alive") {
                    SessionEvent::Updated(snapshot, _) if snapshot.summary.is_some() => {
                        return SessionEvent::Updated(snapshot, None);
                    }
                    event @ SessionEvent::WaitingAlert { .. } => return event,
                    _ => {}
                }
            }
        })
        .await
        .expect("hung waiting for a summary event")
    }

    /// The snapshot `event` carries, failing with `what` when it is not an
    /// update.
    fn updated(event: SessionEvent, what: &str) -> protocol::SessionSnapshot {
        match event {
            SessionEvent::Updated(snapshot, _) => Some(*snapshot),
            _ => None,
        }
        .expect(what)
    }

    /// Let the pipeline's tasks run until `cond` holds; returns whether it
    /// did. Yields only, so a passing run takes no wall time.
    async fn settles(cond: impl Fn() -> bool) -> bool {
        for _ in 0..100_000 {
            if cond() {
                return true;
            }
            tokio::task::yield_now().await;
        }
        cond()
    }

    /// The alerts waiting on `events`.
    fn alerts_waiting(events: &mut broadcast::Receiver<SessionEvent>) -> usize {
        let mut alerts = 0;
        while let Ok(event) = events.try_recv() {
            if matches!(event, SessionEvent::WaitingAlert { .. }) {
                alerts += 1;
            }
        }
        alerts
    }

    /// A `billing_hub` holding the idle interactive session `s1` with a
    /// sidecar, its events subscribed.
    fn hub_with_session(
        tag: &str,
    ) -> (
        Hub,
        crate::server::test_support::ScratchDir,
        std::sync::Arc<crate::summarizer::service::test_support::FakeCaller>,
        broadcast::Receiver<SessionEvent>,
    ) {
        let (hub, scratch, fake) = billing_hub(tag, 0.01);
        crate::history::test_support::write_meta_for(&hub.dirs, "s1");
        hub.sessions.insert(crate::history::test_support::record(
            "s1",
            protocol::SessionMode::Interactive,
        ));
        let events = hub.sessions.subscribe();
        (hub, scratch, fake, events)
    }

    fn set_settings(hub: &Hub, change: impl FnOnce(&mut SummarizerSettings)) {
        let mut settings = hub.state.summarizer_settings();
        change(&mut settings);
        hub.state
            .set_summarizer_settings(settings)
            .expect("store the settings");
    }

    /// The hook route runs `add_report`, and the registry asks `has_work`,
    /// under a session's record lock, while `snapshots()` holds the registry
    /// map's lock as it locks each record: neither may take the map lock.
    /// With the map write-locked here, both finish only if they never ask.
    #[tokio::test]
    async fn add_report_takes_no_registry_lock() {
        let (hub, _scratch, _fake, _events) = hub_with_session("turns-lock-order");
        hub.sessions.insert(crate::history::test_support::record(
            "gone",
            protocol::SessionMode::Interactive,
        ));
        hub.client_count.send_replace(0);
        hub.turns
            .on_stop("gone", "a report for a session about to go");
        assert!(settles(|| hub.turns.state_of("gone") == Some((1, false))).await);
        hub.sessions.remove("gone");

        let map = hub.sessions.hold_map();
        let turns = std::sync::Arc::clone(&hub.turns);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let start = turns.add_report("s1", "a report");
            let due = turns.has_work("s1");
            let _ = done_tx.send((start, due));
        });
        let finished = done_rx.recv_timeout(LOCK_HANG_GUARD);
        drop(map);
        assert_eq!(
            finished.expect("hung: add_report or has_work waited on the registry map lock"),
            (true, true)
        );
    }

    #[tokio::test]
    async fn a_stop_summarizes_into_the_ring_and_alerts_once() {
        let (hub, _scratch, fake, mut events) = hub_with_session("turns-stop");
        apply_hook(&hub, &stop("I fixed the bug."), 100);

        let snapshot = updated(
            next_summary_event(&mut events).await,
            "the update with the line comes first",
        );
        let summary = snapshot.summary.expect("a summary");
        assert_eq!(summary.lines.len(), 1);
        assert_eq!(summary.lines[0].text, FAKE_NOTIFICATION);
        assert_eq!(summary.source, SummarySource::Model);
        let alert = next_summary_event(&mut events).await;
        assert!(
            matches!(&alert, SessionEvent::WaitingAlert { session_id, notification, source: SummarySource::Model, .. }
                if session_id == "s1" && notification == FAKE_NOTIFICATION),
            "{alert:?}"
        );
        assert_eq!(alerts_waiting(&mut events), 0, "exactly one alert");
        assert_eq!(fake.calls(), 1);
        assert!(
            crate::paths::session_summary_file(&hub.dirs, "s1").exists(),
            "the ring is saved"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn two_stops_inside_the_window_make_one_call() {
        let (hub, _scratch, fake, mut events) = hub_with_session("turns-coalesce");
        apply_hook(&hub, &stop("report A"), 100);
        let _update = next_summary_event(&mut events).await;
        let _alert = next_summary_event(&mut events).await;
        assert_eq!(fake.calls(), 1);

        apply_hook(&hub, &stop("report B"), 200);
        apply_hook(&hub, &stop("report C"), 300);
        let _update = next_summary_event(&mut events).await;
        let _alert = next_summary_event(&mut events).await;
        tokio::time::sleep(super::COALESCE_WINDOW * 3).await;

        assert_eq!(fake.calls(), 2, "B and C share one call");
        let prompts = fake.prompts();
        let second = &prompts[1];
        let b = second.find("report B").expect("B is in the prompt");
        let c = second.find("report C").expect("C is in the prompt");
        assert!(b < c, "oldest first");
        assert!(!second.contains("report A"), "A was already summarized");
    }

    #[tokio::test]
    async fn a_subagent_stop_makes_no_summary() {
        let (hub, _scratch, fake, _events) = hub_with_session("turns-subagent");
        let body = serde_json::json!({
            "hook_event_name": "Stop",
            "agent_id": "sub-1",
            "last_assistant_message": "subagent done",
        });
        apply_hook(&hub, &body.to_string(), 100);
        assert_eq!(
            hub.turns.state_of("s1"),
            None,
            "nothing reached the pipeline"
        );
        assert_eq!(fake.calls(), 0);
    }

    #[tokio::test]
    async fn a_subagent_stop_does_not_overwrite_the_last_message() {
        let (hub, _scratch, _fake, _events) = hub_with_session("turns-subagent-message");
        apply_hook(&hub, &stop("main agent done"), 100);
        let body = serde_json::json!({
            "hook_event_name": "Stop",
            "agent_id": "sub-1",
            "last_assistant_message": "subagent done",
        });
        apply_hook(&hub, &body.to_string(), 200);
        let arc = hub.sessions.get("s1").expect("the session");
        assert_eq!(
            crate::sync::lock(&arc).last_assistant_message.as_deref(),
            Some("main agent done")
        );
    }

    #[tokio::test]
    async fn a_session_removed_mid_call_writes_nothing() {
        let (hub, _scratch, fake, mut events) = hub_with_session("turns-removed");
        apply_hook(&hub, &stop("a report"), 100);
        hub.sessions.delete_sidecar("s1");
        hub.sessions.delete_session_dir("s1");
        hub.sessions.remove("s1");

        assert!(
            settles(|| hub.turns.state_of("s1").is_none()).await,
            "the pipeline forgets the session"
        );
        assert_eq!(fake.calls(), 0, "a session gone before its call makes none");
        while let Ok(event) = events.try_recv() {
            assert!(
                !matches!(&event, SessionEvent::WaitingAlert { .. }),
                "no alert"
            );
            if let SessionEvent::Updated(snapshot, _) = &event {
                assert!(snapshot.summary.is_none(), "no update carries a line");
            }
        }
        assert!(
            !hub.dirs.sessions_dir.join("s1").exists(),
            "no file or dir is made"
        );
    }

    #[tokio::test]
    async fn a_session_removed_while_its_call_is_in_flight_writes_nothing() {
        let (mut hub, _scratch) = crate::server::test_support::test_hub("turns-in-flight");
        let gate = std::sync::Arc::new(tokio::sync::Notify::new());
        let fake = std::sync::Arc::new(
            crate::summarizer::service::test_support::FakeCaller::gated(gate.clone()),
        );
        hub.client_count.send_replace(1);
        hub.summarizer = crate::server::test_support::test_summarizer(
            &hub.state,
            &hub.dirs,
            &hub.client_count,
            &hub.state_events,
            fake.clone(),
        );
        hub.turns = super::Turns::new(
            std::sync::Arc::clone(&hub.sessions),
            std::sync::Arc::clone(&hub.summarizer),
        );
        crate::history::test_support::write_meta_for(&hub.dirs, "s1");
        hub.sessions.insert(crate::history::test_support::record(
            "s1",
            protocol::SessionMode::Interactive,
        ));
        let mut events = hub.sessions.subscribe();
        apply_hook(&hub, &stop("a report"), 100);
        assert!(settles(|| fake.calls() == 1).await, "the call is in flight");

        hub.sessions.delete_sidecar("s1");
        hub.sessions.delete_session_dir("s1");
        hub.sessions.remove("s1");
        gate.notify_one();
        assert!(
            settles(|| hub.turns.state_of("s1").is_none()).await,
            "the pipeline forgets the session"
        );
        while let Ok(event) = events.try_recv() {
            assert!(
                !matches!(&event, SessionEvent::WaitingAlert { .. }),
                "no alert"
            );
            if let SessionEvent::Updated(snapshot, _) = &event {
                assert!(snapshot.summary.is_none(), "no update carries a line");
            }
        }
        assert!(
            !hub.dirs.sessions_dir.join("s1").exists(),
            "no file or dir is made"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_session_removed_during_the_window_makes_no_call() {
        let (hub, _scratch, fake, mut events) = hub_with_session("turns-window-removed");
        apply_hook(&hub, &stop("report A"), 100);
        let _update = next_summary_event(&mut events).await;
        let _alert = next_summary_event(&mut events).await;
        apply_hook(&hub, &stop("report B"), 200);
        hub.sessions.remove("s1");

        tokio::time::sleep(super::COALESCE_WINDOW * 3).await;
        assert_eq!(fake.calls(), 1, "B's call is never made");
        assert_eq!(hub.turns.state_of("s1"), None);
    }

    #[tokio::test]
    async fn entries_for_removed_sessions_are_dropped_on_the_next_report() {
        let (hub, _scratch, _fake, _events) = hub_with_session("turns-reap");
        hub.client_count.send_replace(0);
        apply_hook(&hub, &stop("report A"), 100);
        assert!(settles(|| hub.turns.state_of("s1") == Some((1, false))).await);
        hub.sessions.remove("s1");
        hub.sessions.insert(crate::history::test_support::record(
            "s2",
            protocol::SessionMode::Interactive,
        ));

        hub.turns.on_stop("s2", "report for s2");
        assert_eq!(
            hub.turns.state_of("s1"),
            None,
            "the gone session is dropped"
        );
        assert!(hub.turns.state_of("s2").is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn a_template_line_does_not_hold_the_next_stop() {
        let (hub, _scratch, _fake, mut events) = hub_with_session("turns-template-window");
        set_settings(&hub, |settings| settings.enabled = false);
        apply_hook(&hub, &stop("report A"), 100);
        let _update = next_summary_event(&mut events).await;
        let _alert = next_summary_event(&mut events).await;

        let asked = tokio::time::Instant::now();
        apply_hook(&hub, &stop("report B"), 200);
        // The first update is B's own hook event; B's alert follows its line.
        while !matches!(
            next_summary_event(&mut events).await,
            SessionEvent::WaitingAlert { .. }
        ) {}
        let lines = crate::sync::lock(&hub.sessions.get("s1").expect("the session"))
            .summary
            .as_ref()
            .map_or(0, |summary| summary.lines.len());
        assert_eq!(lines, 2, "B's line is in the ring");
        assert!(
            asked.elapsed() < super::COALESCE_WINDOW,
            "B did not wait out a window"
        );
    }

    #[tokio::test]
    async fn the_alert_reflects_the_status_the_line_was_published_with() {
        let (hub, _scratch, _fake, mut events) = hub_with_session("turns-atomic");
        apply_hook(&hub, &stop("a report"), 100);
        let snapshot = updated(next_summary_event(&mut events).await, "the line");
        assert_eq!(snapshot.status, SessionStatus::Idle);
        let next = events.try_recv().expect("an event follows the update");
        assert!(
            matches!(next, SessionEvent::WaitingAlert { .. }),
            "the alert comes right after the update it was decided with: {next:?}"
        );
    }

    #[tokio::test]
    async fn no_alert_when_the_session_is_working_again() {
        let (hub, _scratch, _fake, mut events) = hub_with_session("turns-working");
        apply_hook(&hub, &stop("a report"), 100);
        apply_hook(&hub, r#"{"hook_event_name":"UserPromptSubmit"}"#, 200);

        let snapshot = updated(
            next_summary_event(&mut events).await,
            "the line is still written",
        );
        assert_eq!(snapshot.status, SessionStatus::Working);
        assert_eq!(alerts_waiting(&mut events), 0, "no alert while working");
    }

    #[tokio::test]
    async fn the_template_line_when_summaries_are_off() {
        let (hub, _scratch, fake, mut events) = hub_with_session("turns-off");
        set_settings(&hub, |settings| settings.enabled = false);
        apply_hook(&hub, &stop("First line here.\nMore detail."), 100);

        let snapshot = updated(
            next_summary_event(&mut events).await,
            "the template line is written",
        );
        let summary = snapshot.summary.expect("a summary");
        assert_eq!(summary.kind, None);
        assert_eq!(summary.source, SummarySource::Template);
        assert_eq!(summary.lines[0].text, "First line here.");
        let alert = next_summary_event(&mut events).await;
        assert!(
            matches!(
                &alert,
                SessionEvent::WaitingAlert {
                    kind: None,
                    source: SummarySource::Template,
                    ..
                }
            ),
            "{alert:?}"
        );
        assert_eq!(fake.calls(), 0);
    }

    #[tokio::test]
    async fn the_template_line_at_the_cap() {
        let (hub, _scratch, fake, mut events) = hub_with_session("turns-cap");
        set_settings(&hub, |settings| settings.daily_cap_usd = 0.0);
        apply_hook(&hub, &stop("Capped report."), 100);

        let snapshot = updated(
            next_summary_event(&mut events).await,
            "the template line is written",
        );
        let summary = snapshot.summary.expect("a summary");
        assert_eq!(summary.source, SummarySource::Template);
        assert_eq!(summary.lines[0].text, "Capped report.");
        let alert = next_summary_event(&mut events).await;
        assert!(
            matches!(
                &alert,
                SessionEvent::WaitingAlert {
                    source: SummarySource::Template,
                    ..
                }
            ),
            "{alert:?}"
        );
        assert_eq!(fake.calls(), 0);
    }

    #[tokio::test]
    async fn put_back_reports_do_not_keep_the_provisional_line() {
        let (hub, _scratch, _fake, _events) = hub_with_session("turns-put-back-provisional");
        hub.client_count.send_replace(0);
        apply_hook(&hub, &stop("report A"), 100);
        assert!(
            settles(|| hub.turns.state_of("s1") == Some((1, false))).await,
            "the report is put back with the driver stopped"
        );
        assert!(hub.turns.has_reports("s1"), "it is still the next summary");
        assert!(!hub.turns.has_work("s1"), "but no stop line is on its way");

        apply_hook(&hub, r#"{"hook_event_name":"UserPromptSubmit"}"#, 200);
        hub.sessions.update("s1", |record| {
            let line = protocol::SummaryLine {
                text: "working".to_owned(),
                at: chrono::Utc::now(),
                source: SummarySource::Model,
                provisional: true,
            };
            record.push_provisional_line(line, chrono::Utc::now());
        });
        hub.sessions
            .update("s1", |record| record.status = SessionStatus::Idle);
        let summary = crate::sync::lock(&hub.sessions.get("s1").expect("the session"))
            .summary
            .clone();
        assert_eq!(summary, None, "the turn ended with no stop line due");
    }

    #[tokio::test]
    async fn reports_wait_while_no_client_is_connected() {
        let (hub, _scratch, fake, mut events) = hub_with_session("turns-idle");
        hub.client_count.send_replace(0);
        apply_hook(&hub, &stop("report A"), 100);
        assert!(
            settles(|| hub.turns.state_of("s1") == Some((1, false))).await,
            "the report waits"
        );
        assert_eq!(fake.calls(), 0, "no call without a client");

        hub.client_count.send_replace(1);
        apply_hook(&hub, &stop("report B"), 200);
        let _update = next_summary_event(&mut events).await;
        assert_eq!(fake.calls(), 1);
        let prompt = &fake.prompts()[0];
        let a = prompt.find("report A").expect("A waited for this call");
        let b = prompt.find("report B").expect("B is in the call");
        assert!(a < b, "oldest first");
    }
}
