//! What a headless pane shows where a terminal would be: the session's
//! stats and its recent-actions log, under the tail cap the Tauri app's
//! `HeadlessView` applies. Pure data and formatting; the body itself is in
//! `headless_view`.

use protocol::{SessionMode, SessionSnapshot, SessionStatus};

/// How many of the most recent actions the log draws before the user asks
/// for the rest.
pub const HEADLESS_TAIL: usize = 200;

/// The note the log shows while the session has reported no actions.
pub const EMPTY_LOG: &str = "No events yet…";

/// Whether `session` runs headless, so its pane shows the stats and the log
/// instead of a terminal.
pub fn is_headless(session: &SessionSnapshot) -> bool {
    session.mode == SessionMode::Headless
}

/// The actions to draw and how many earlier ones the cap hid: all of them
/// with `show_all`, the last [`HEADLESS_TAIL`] of them otherwise.
pub fn visible(actions: &[String], show_all: bool) -> (usize, &[String]) {
    if show_all || actions.len() <= HEADLESS_TAIL {
        return (0, actions);
    }
    let hidden = actions.len() - HEADLESS_TAIL;
    (hidden, &actions[hidden..])
}

/// The show-all button's label.
pub fn show_all_label(total: usize, hidden: usize) -> String {
    format!("Show all {total} entries (earlier {hidden} hidden)")
}

/// The session's status as the stats bar spells it.
pub fn status_label(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::Spawning => "starting",
        SessionStatus::Idle => "idle",
        SessionStatus::Working => "working",
        SessionStatus::AwaitingInput => "awaiting input",
        SessionStatus::Stopped => "stopped",
        SessionStatus::Error => "error",
    }
}

/// `n` with a `,` every three digits.
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// `usd` as the cost stat shows it.
pub fn cost(usd: f64) -> String {
    format!("${usd:.4}")
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `n` actions, oldest first, named `a1`.. `an`.
    fn actions(n: usize) -> Vec<String> {
        (1..=n).map(|i| format!("a{i}")).collect()
    }

    fn session(id: &str, mode: &str) -> SessionSnapshot {
        serde_json::from_value(json!({
            "id": id,
            "label": id,
            "kind": "single",
            "members": [],
            "status": "idle",
            "mode": mode,
            "started_at": "2026-01-01T00:00:00Z",
            "exit_code": null,
            "metrics": { "input_tokens": 0, "output_tokens": 0, "cost_usd": 0.0, "last_activity_at": null },
            "recent_actions": [],
            "agent": "claude",
        }))
        .expect("session fixture")
    }

    #[test]
    fn headless_tail_is_two_hundred() {
        assert_eq!(HEADLESS_TAIL, 200);
    }

    #[test]
    fn headless_visible_at_or_under_the_cap_keeps_everything() {
        for size in [0, HEADLESS_TAIL] {
            let all = actions(size);
            for show_all in [false, true] {
                assert_eq!(
                    visible(&all, show_all),
                    (0, all.as_slice()),
                    "{size} entries, show_all {show_all}"
                );
            }
        }
    }

    #[test]
    fn headless_visible_over_the_cap_keeps_the_tail_and_show_all_the_rest() {
        for size in [HEADLESS_TAIL + 1, 450] {
            let all = actions(size);
            let (hidden, shown) = visible(&all, false);
            assert_eq!(hidden, size - HEADLESS_TAIL, "{size} entries");
            assert_eq!(shown, &all[hidden..], "{size} entries");
            assert_eq!(shown.len(), HEADLESS_TAIL);
            let (first, last) = (format!("a{}", hidden + 1), format!("a{size}"));
            assert_eq!(shown.first().map(String::as_str), Some(first.as_str()));
            assert_eq!(shown.last().map(String::as_str), Some(last.as_str()));
            assert_eq!(
                visible(&all, true),
                (0, all.as_slice()),
                "{size} entries with show_all"
            );
        }
    }

    #[test]
    fn headless_show_all_label_names_the_hidden_earlier_entries() {
        assert_eq!(
            show_all_label(250, 50),
            "Show all 250 entries (earlier 50 hidden)"
        );
    }

    #[test]
    fn headless_status_labels() {
        assert_eq!(status_label(SessionStatus::Spawning), "starting");
        assert_eq!(status_label(SessionStatus::Idle), "idle");
        assert_eq!(status_label(SessionStatus::Working), "working");
        assert_eq!(status_label(SessionStatus::AwaitingInput), "awaiting input");
        assert_eq!(status_label(SessionStatus::Stopped), "stopped");
        assert_eq!(status_label(SessionStatus::Error), "error");
    }

    #[test]
    fn headless_thousands_separates_every_three_digits() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(1_234_567), "1,234,567");
        assert_eq!(thousands(u64::MAX), "18,446,744,073,709,551,615");
    }

    #[test]
    fn headless_cost_shows_four_decimals() {
        assert_eq!(cost(0.0), "$0.0000");
        assert_eq!(cost(0.01234), "$0.0123");
        assert_eq!(cost(12.5), "$12.5000");
    }

    #[test]
    fn headless_empty_log_is_the_no_events_note() {
        assert_eq!(EMPTY_LOG, "No events yet\u{2026}");
        assert!(!EMPTY_LOG.is_ascii(), "the ellipsis is U+2026");
    }

    #[test]
    fn headless_mode_is_headless() {
        assert!(is_headless(&session("h", "headless")));
        assert!(!is_headless(&session("i", "interactive")));
        assert!(!is_headless(&session("s", "plain_shell")));
    }
}
