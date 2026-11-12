//! Dispatch runs as sidebar leaves: the status a run's leaf draws, whether a
//! run keeps its repo active, the folder that places it, its hover text and
//! the order runs sit in. Plain Rust, so every rule is unit-tested.

use protocol::{DispatchRunSnapshot, DispatchRunStatus, SessionMode, SessionStatus};

use crate::sidebar::{Leaf, LeafKind, LeafState};
use crate::target::HostKey;

/// The status dot a run's leaf draws for the run's `status`.
#[must_use]
pub fn leaf_status(status: DispatchRunStatus) -> SessionStatus {
    match status {
        DispatchRunStatus::Starting => SessionStatus::Spawning,
        DispatchRunStatus::Working => SessionStatus::Working,
        DispatchRunStatus::Stopping | DispatchRunStatus::Ended | DispatchRunStatus::Unknown => {
            SessionStatus::Stopped
        }
    }
}

/// Whether a run with `status` can still work, which keeps its repo or
/// workspace active.
#[must_use]
pub fn marks_active(status: DispatchRunStatus) -> bool {
    matches!(
        status,
        DispatchRunStatus::Starting | DispatchRunStatus::Working | DispatchRunStatus::Stopping
    )
}

/// The folder a run is placed by: its main checkout, else its worktree.
#[must_use]
pub fn run_home(run: &DispatchRunSnapshot) -> &str {
    run.main_checkout.as_deref().unwrap_or(&run.worktree)
}

/// The run leaf's hover text: what it is, its backend, its tier when it has
/// one, when it started in local time, and its worktree, one per line.
#[must_use]
pub fn hover_text(run: &DispatchRunSnapshot) -> String {
    let mut lines = vec![
        "Dispatch run".to_owned(),
        format!("Backend: {}", run.backend),
    ];
    if let Some(tier) = run.tier {
        lines.push(format!("Tier: {tier}"));
    }
    let started = run.started_at.with_timezone(&chrono::Local);
    lines.push(format!("Started: {}", started.format("%Y-%m-%d %H:%M")));
    lines.push(format!("Worktree: {}", run.worktree));
    lines.join("\n")
}

/// `run`'s leaf, listed by `host`.
#[must_use]
pub fn dispatch_leaf(run: &DispatchRunSnapshot, host: &HostKey) -> Leaf {
    Leaf {
        id: run.run_id.clone(),
        kind: LeafKind::Dispatch,
        host: host.clone(),
        status: leaf_status(run.status),
        mode: SessionMode::Headless,
        agent: None,
        label: run.title.clone(),
        account_login: false,
        agent_up: true,
        subline: None,
        runtime: Some("DS".to_owned()),
        icon: None,
        chips: Vec::new(),
        attention: false,
        busy_excluded: false,
        unseen: false,
        background: false,
        state: LeafState::Live,
        tooltip: hover_text(run),
    }
}

/// `runs` newest first by start time, ties by run id.
pub fn sort_newest_first(runs: &mut [&DispatchRunSnapshot]) {
    runs.sort_by(|a, b| {
        b.started_at
            .cmp(&a.started_at)
            .then_with(|| a.run_id.cmp(&b.run_id))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidebar::tests::run;

    #[test]
    fn leaf_status_maps_each_run_status() {
        for (status, want) in [
            (DispatchRunStatus::Starting, SessionStatus::Spawning),
            (DispatchRunStatus::Working, SessionStatus::Working),
            (DispatchRunStatus::Stopping, SessionStatus::Stopped),
            (DispatchRunStatus::Ended, SessionStatus::Stopped),
            (DispatchRunStatus::Unknown, SessionStatus::Stopped),
        ] {
            assert_eq!(leaf_status(status), want, "{status:?}");
        }
    }

    #[test]
    fn marks_active_is_true_while_the_run_can_still_work() {
        for (status, want) in [
            (DispatchRunStatus::Starting, true),
            (DispatchRunStatus::Working, true),
            (DispatchRunStatus::Stopping, true),
            (DispatchRunStatus::Ended, false),
            (DispatchRunStatus::Unknown, false),
        ] {
            assert_eq!(marks_active(status), want, "{status:?}");
        }
    }

    #[test]
    fn run_home_prefers_the_main_checkout() {
        let r = run(
            "d1",
            "X:\\r1.wt\\a",
            Some("X:\\r1"),
            0,
            DispatchRunStatus::Working,
        );
        assert_eq!(run_home(&r), "X:\\r1");
    }

    #[test]
    fn run_home_falls_back_to_the_worktree() {
        let r = run("d1", "X:\\r1.wt\\a", None, 0, DispatchRunStatus::Working);
        assert_eq!(run_home(&r), "X:\\r1.wt\\a");
    }

    #[test]
    fn hover_text_leaves_out_a_missing_tier() {
        let r = run("d1", "X:\\w", None, 0, DispatchRunStatus::Working);
        let text = hover_text(&r);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 4, "{text}");
        assert_eq!(lines[0], "Dispatch run");
        assert_eq!(lines[1], format!("Backend: {}", r.backend));
        assert!(lines[2].starts_with("Started: "), "{text}");
        assert_eq!(lines[3], "Worktree: X:\\w");
        assert!(!text.contains("Tier:"), "{text}");
    }

    #[test]
    fn hover_text_lists_backend_tier_start_and_worktree() {
        let mut r = run("d1", "X:\\w", None, 0, DispatchRunStatus::Working);
        r.tier = Some(2);
        let text = hover_text(&r);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 5, "{text}");
        assert_eq!(lines[0], "Dispatch run");
        assert_eq!(lines[1], format!("Backend: {}", r.backend));
        assert_eq!(lines[2], "Tier: 2");
        assert!(lines[3].starts_with("Started: "), "{text}");
        assert_eq!(lines[4], "Worktree: X:\\w");
    }

    #[test]
    fn runs_sort_newest_first() {
        let old = run("a", "X:\\w", None, 10, DispatchRunStatus::Working);
        let new = run("b", "X:\\w", None, 30, DispatchRunStatus::Working);
        let tie_z = run("z", "X:\\w", None, 20, DispatchRunStatus::Working);
        let tie_c = run("c", "X:\\w", None, 20, DispatchRunStatus::Working);
        let mut runs = vec![&old, &tie_z, &new, &tie_c];
        sort_newest_first(&mut runs);
        let ids: Vec<&str> = runs.iter().map(|r| r.run_id.as_str()).collect();
        assert_eq!(ids, ["b", "c", "z", "a"]);
    }
}
