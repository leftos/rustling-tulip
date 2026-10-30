//! The running-work strip a session's chat and summary loaders are sent: the
//! background tasks its agent's last `Stop` listed, less those whose task
//! notification the transcript holds, the screen's counts standing in for
//! work no `Stop` has reported yet, and the earliest pending wakeup.
//!
//! The record names each task and knows when it started, so it is what the
//! strip lists; the screen only counts, so it can add placeholder items but
//! never removes one the record lists.

use std::collections::HashSet;

use agent_api::ScreenWork;
use agent_api::hook_vocabulary::StopTaskKind;
use protocol::{DaemonMessage, WorkShell, WorkSubagent};

use crate::background_work::BackgroundWork;

/// The `ChatWork` message for `session_id`.
///
/// Each task in `work` whose id is not in `ended` is a subagent or a shell
/// item; a task of another kind is left out. Where `screen` counts more of a
/// kind than that, placeholder items marked `from_screen` make up the
/// difference. The wakeup is `work`'s earliest one, past or not: a one-shot
/// fires only once the agent is idle, so the `Stop` that still lists it can
/// come after its time, and it leaves only when a later `Stop` no longer
/// lists it. A past time means it is due.
pub(crate) fn chat_work(
    session_id: &str,
    work: &BackgroundWork,
    ended: &HashSet<String>,
    screen: Option<ScreenWork>,
) -> DaemonMessage {
    let mut subagents = Vec::new();
    let mut shells = Vec::new();
    for task in work.tasks.iter().filter(|task| !ended.contains(&task.id)) {
        match &task.kind {
            StopTaskKind::Shell { command } => shells.push(WorkShell {
                id: task.id.clone(),
                description: task.description.clone(),
                command: command.clone(),
                since: Some(task.since),
                from_screen: false,
            }),
            StopTaskKind::Subagent { agent_type } => subagents.push(WorkSubagent {
                id: task.id.clone(),
                description: task.description.clone(),
                agent_type: agent_type.clone(),
                since: Some(task.since),
                from_screen: false,
            }),
            StopTaskKind::Other { .. } => {}
        }
    }
    if let Some(screen) = screen {
        pad(&mut subagents, screen.subagents, || WorkSubagent {
            id: String::new(),
            description: String::new(),
            agent_type: None,
            since: None,
            from_screen: true,
        });
        pad(&mut shells, screen.shells, || WorkShell {
            id: String::new(),
            description: String::new(),
            command: None,
            since: None,
            from_screen: true,
        });
    }
    DaemonMessage::ChatWork {
        session_id: session_id.to_owned(),
        subagents,
        shells,
        wakeup_at: work.wakeup_at(),
    }
}

/// Add `placeholder` items to `items` until it holds `count`; never removes
/// one.
fn pad<T>(items: &mut Vec<T>, count: u32, placeholder: impl FnMut() -> T) {
    let count = usize::try_from(count).unwrap_or(usize::MAX);
    items.resize_with(count.max(items.len()), placeholder);
}

#[cfg(test)]
#[expect(
    clippy::panic,
    reason = "tests fail on a message that is not the strip"
)]
mod tests {
    use super::*;
    use crate::background_work::{WorkTask, WorkWakeup};
    use chrono::{DateTime, Duration, Utc};

    fn now() -> DateTime<Utc> {
        "2026-10-30T12:00:00Z"
            .parse()
            .unwrap_or_else(|err| panic!("a valid time: {err}"))
    }

    fn task(id: &str, kind: StopTaskKind) -> WorkTask {
        WorkTask {
            id: id.to_owned(),
            kind,
            description: format!("task {id}"),
            since: now() - Duration::minutes(3),
        }
    }

    fn shell(id: &str) -> WorkTask {
        task(
            id,
            StopTaskKind::Shell {
                command: Some("cargo test".to_owned()),
            },
        )
    }

    fn subagent(id: &str) -> WorkTask {
        task(
            id,
            StopTaskKind::Subagent {
                agent_type: Some("Explore".to_owned()),
            },
        )
    }

    fn work_of(tasks: Vec<WorkTask>) -> BackgroundWork {
        BackgroundWork {
            tasks,
            wakeups: Vec::new(),
        }
    }

    /// The strip's subagents, shells and wakeup.
    fn parts(msg: DaemonMessage) -> (Vec<WorkSubagent>, Vec<WorkShell>, Option<DateTime<Utc>>) {
        match msg {
            DaemonMessage::ChatWork {
                session_id,
                subagents,
                shells,
                wakeup_at,
            } => {
                assert_eq!(session_id, "s1");
                (subagents, shells, wakeup_at)
            }
            other => panic!("expected a ChatWork, got {other:?}"),
        }
    }

    fn strip(
        work: &BackgroundWork,
        ended: &[&str],
        screen: Option<ScreenWork>,
    ) -> (Vec<WorkSubagent>, Vec<WorkShell>, Option<DateTime<Utc>>) {
        let ended: HashSet<String> = ended.iter().map(|&id| id.to_owned()).collect();
        parts(chat_work("s1", work, &ended, screen))
    }

    fn placeholder_shell() -> WorkShell {
        WorkShell {
            id: String::new(),
            description: String::new(),
            command: None,
            since: None,
            from_screen: true,
        }
    }

    #[test]
    fn the_record_alone_lists_its_tasks() {
        let work = work_of(vec![shell("b1"), subagent("a1")]);
        let (subagents, shells, wakeup_at) = strip(&work, &[], None);
        let since = Some(now() - Duration::minutes(3));
        assert_eq!(
            subagents,
            [WorkSubagent {
                id: "a1".to_owned(),
                description: "task a1".to_owned(),
                agent_type: Some("Explore".to_owned()),
                since,
                from_screen: false,
            }]
        );
        assert_eq!(
            shells,
            [WorkShell {
                id: "b1".to_owned(),
                description: "task b1".to_owned(),
                command: Some("cargo test".to_owned()),
                since,
                from_screen: false,
            }]
        );
        assert_eq!(wakeup_at, None);
    }

    #[test]
    fn an_ended_task_is_dropped() {
        let work = work_of(vec![shell("b1"), shell("b2"), subagent("a1")]);
        let (subagents, shells, _) = strip(&work, &["b1", "a1"], None);
        assert_eq!(subagents, [], "a1's notification ended it");
        let ids: Vec<&str> = shells.iter().map(|shell| shell.id.as_str()).collect();
        assert_eq!(ids, ["b2"]);
    }

    #[test]
    fn a_task_of_another_kind_is_dropped() {
        let work = work_of(vec![
            task(
                "x1",
                StopTaskKind::Other {
                    kind: "remote".to_owned(),
                },
            ),
            shell("b1"),
        ]);
        let (subagents, shells, _) = strip(&work, &[], None);
        assert_eq!(subagents, []);
        let ids: Vec<&str> = shells.iter().map(|shell| shell.id.as_str()).collect();
        assert_eq!(ids, ["b1"], "the other kind has no place on the strip");
    }

    #[test]
    fn the_screen_pads_above_the_record_but_never_removes() {
        let work = work_of(vec![shell("b1"), subagent("a1")]);
        let (subagents, shells, _) = strip(
            &work,
            &[],
            Some(ScreenWork {
                subagents: 0,
                shells: 3,
            }),
        );
        assert_eq!(
            subagents
                .iter()
                .map(|agent| agent.id.as_str())
                .collect::<Vec<_>>(),
            ["a1"],
            "a screen counting none keeps the record's subagent"
        );
        assert_eq!(shells.len(), 3, "the screen counts three shells");
        assert_eq!(shells[0].id, "b1", "the record's shell comes first");
        assert!(!shells[0].from_screen);
        assert_eq!(shells[1..], [placeholder_shell(), placeholder_shell()]);

        let (subagents, _, _) = strip(
            &work,
            &["a1"],
            Some(ScreenWork {
                subagents: 1,
                shells: 0,
            }),
        );
        assert_eq!(
            subagents,
            [WorkSubagent {
                id: String::new(),
                description: String::new(),
                agent_type: None,
                since: None,
                from_screen: true,
            }],
            "a running subagent the record no longer lists is the screen's"
        );
    }

    fn wakeup_work(at: DateTime<Utc>) -> BackgroundWork {
        BackgroundWork {
            tasks: Vec::new(),
            wakeups: vec![WorkWakeup {
                id: "w1".to_owned(),
                at: Some(at),
            }],
        }
    }

    #[test]
    fn the_wakeup_is_the_records() {
        let coming = now() + Duration::minutes(4);
        assert_eq!(
            strip(&wakeup_work(coming), &[], None).2,
            Some(coming),
            "a wakeup still to come"
        );
        let passed = now() - Duration::minutes(1);
        assert_eq!(
            strip(&wakeup_work(passed), &[], None).2,
            Some(passed),
            "a passed wakeup is due"
        );
    }

    /// A one-shot fires only once the agent is idle, so the `Stop` that still
    /// lists it can come after its time: it leaves only when a later `Stop`
    /// no longer lists it.
    #[test]
    fn a_passed_wakeup_still_listed_stays() {
        let passed = now() - Duration::minutes(10);
        let (_, _, wakeup_at) = strip(&wakeup_work(passed), &[], None);
        assert_eq!(wakeup_at, Some(passed));
    }
}
