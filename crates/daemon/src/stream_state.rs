//! The stream-state accumulator a headless session and a dispatch run share.
//!
//! A plugin's `parse_headless_line` yields [`HeadlessEvent`]s; applying them in
//! order keeps the run's status, its usage metrics, its recent-actions tail
//! (capped by [`RECENT_ACTIONS_CAP`]) and the CLI's own conversation id.
//!
//! A headless session applies the same rules to its session record
//! ([`crate::headless`]), and the dispatch-run watcher reads a live run's
//! stream through [`StreamState`].

use agent_api::HeadlessEvent;
use chrono::Utc;
use protocol::{SessionMetrics, SessionStatus};

/// In-memory cap on a session's `recent_actions`, matching the tail the
/// clients' headless log draws before it offers the rest.
pub(crate) const RECENT_ACTIONS_CAP: usize = 200;

/// Push `action`, dropping the oldest entries beyond [`RECENT_ACTIONS_CAP`].
pub(crate) fn push_capped(actions: &mut Vec<String>, action: String) {
    actions.push(action);
    if actions.len() > RECENT_ACTIONS_CAP {
        let drop_count = actions.len() - RECENT_ACTIONS_CAP;
        actions.drain(0..drop_count);
    }
}

/// Apply one event to the pieces it changes, returning the conversation id a
/// [`HeadlessEvent::SessionId`] carried and `None` for every other event.
pub(crate) fn apply_event(
    event: HeadlessEvent,
    status: &mut Option<SessionStatus>,
    metrics: &mut SessionMetrics,
    actions: &mut Vec<String>,
) -> Option<String> {
    match event {
        HeadlessEvent::Status(new) => {
            *status = Some(new);
            None
        }
        HeadlessEvent::RecentAction(action) => {
            push_capped(actions, action);
            metrics.last_activity_at = Some(Utc::now());
            None
        }
        HeadlessEvent::Metrics {
            cost_usd,
            input_tokens,
            output_tokens,
        } => {
            apply_metrics(metrics, cost_usd, input_tokens, output_tokens);
            None
        }
        HeadlessEvent::SessionId(id) => Some(id),
    }
}

/// Replace each usage figure `apply_event` is given a `Some` for.
fn apply_metrics(
    metrics: &mut SessionMetrics,
    cost_usd: Option<f64>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
) {
    if let Some(cost) = cost_usd {
        metrics.cost_usd = cost;
    }
    if let Some(tokens) = input_tokens {
        metrics.input_tokens = tokens;
    }
    if let Some(tokens) = output_tokens {
        metrics.output_tokens = tokens;
    }
}

/// What a run's stream has reported so far.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "read by the dispatch-run watcher (DF.5)")
)]
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct StreamState {
    /// The last status the stream reported; `None` until the first one, which
    /// a dispatch run reads as `Starting`.
    pub status: Option<SessionStatus>,
    /// The usage figures the stream reported, each `Some` replacing its own.
    pub metrics: SessionMetrics,
    /// The run's recent-actions tail, oldest dropped beyond the cap.
    pub recent_actions: Vec<String>,
    /// The CLI's own conversation id, once its first line reported one.
    pub agent_session_id: Option<String>,
}

impl StreamState {
    /// Apply `event`, in the order the plugin yielded it.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "read by the dispatch-run watcher (DF.5)")
    )]
    pub(crate) fn apply(&mut self, event: HeadlessEvent) {
        let id = apply_event(
            event,
            &mut self.status,
            &mut self.metrics,
            &mut self.recent_actions,
        );
        if id.is_some() {
            self.agent_session_id = id;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_events() -> Vec<HeadlessEvent> {
        vec![
            HeadlessEvent::Status(SessionStatus::Working),
            HeadlessEvent::RecentAction("system: init".to_string()),
            HeadlessEvent::SessionId("abc-123".to_string()),
        ]
    }

    #[test]
    fn stream_state_follows_a_run() {
        let mut state = StreamState::default();
        for event in init_events() {
            state.apply(event);
        }
        assert_eq!(state.status, Some(SessionStatus::Working));
        assert_eq!(state.agent_session_id.as_deref(), Some("abc-123"));
        assert_eq!(state.recent_actions, ["system: init"]);
        assert!(state.metrics.last_activity_at.is_some());

        state.apply(HeadlessEvent::Metrics {
            cost_usd: Some(0.25),
            input_tokens: Some(1200),
            output_tokens: Some(340),
        });
        assert_eq!(state.metrics.cost_usd, 0.25);
        assert_eq!(state.metrics.input_tokens, 1200);
        assert_eq!(state.metrics.output_tokens, 340);

        state.apply(HeadlessEvent::Metrics {
            cost_usd: None,
            input_tokens: None,
            output_tokens: Some(400),
        });
        assert_eq!(state.metrics.cost_usd, 0.25, "a None leaves the figure");
        assert_eq!(state.metrics.output_tokens, 400);

        state.apply(HeadlessEvent::Status(SessionStatus::Stopped));
        assert_eq!(state.status, Some(SessionStatus::Stopped));
    }

    #[test]
    fn stream_state_keeps_the_newest_200_actions() {
        let mut state = StreamState::default();
        for i in 0..=RECENT_ACTIONS_CAP {
            state.apply(HeadlessEvent::RecentAction(format!("action {i}")));
        }
        assert_eq!(state.recent_actions.len(), RECENT_ACTIONS_CAP);
        assert_eq!(
            state.recent_actions.first().map(String::as_str),
            Some("action 1"),
            "the oldest went"
        );
        assert_eq!(
            state.recent_actions.last().map(String::as_str),
            Some("action 200")
        );
    }

    #[test]
    fn stream_state_starts_without_a_status() {
        let state = StreamState::default();
        assert_eq!(state.status, None);
        assert_eq!(state.agent_session_id, None);
        assert_eq!(state.recent_actions.len(), 0);
    }
}
