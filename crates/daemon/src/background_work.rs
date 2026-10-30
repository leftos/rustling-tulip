//! The work a session's agent reported still running beside an idle turn: the
//! background tasks and the one-shot wakeups its last main-agent `Stop`
//! listed.
//!
//! Each main-agent `Stop` replaces the lists with the ones it carries, so a
//! task the payload no longer lists has ended. A task keeps the time it was
//! first seen running, and a wakeup keeps the time it was first computed to
//! fire, so neither slides forward while the work outlives one turn.
//!
//! The lists live on the record only: a reattached or rebuilt session starts
//! with none, and the next `Stop` fills them again.

use agent_api::hook_vocabulary::{StopTaskKind, StopWork};
use chrono::{DateTime, NaiveDate, TimeZone, Utc};

/// How long past its fire time a wakeup still counts as today's, in seconds:
/// a one-shot fires only once the agent is idle, so its `Stop` can arrive
/// after it.
const OVERDUE_GRACE_SECS: i64 = 60 * 60;

/// The work a session's agent reported at its last turn end.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackgroundWork {
    /// The background tasks still running, in the order the `Stop` listed
    /// them.
    pub tasks: Vec<WorkTask>,
    /// The one-shot wakeups still pending, in the order the `Stop` listed
    /// them.
    pub wakeups: Vec<WorkWakeup>,
}

/// One background task, with the time it was first seen running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkTask {
    /// The task's own id, which is what the stops that list it share.
    pub id: String,
    /// What kind of task it is.
    pub kind: StopTaskKind,
    /// Its description, or empty when the payload carried none.
    pub description: String,
    /// When it was first reported running: a later stop that still lists the
    /// same id keeps this time.
    pub since: DateTime<Utc>,
}

/// One pending one-shot wakeup, with the local time it will fire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkWakeup {
    /// The cron's own id, which is what the stops that list it share.
    pub id: String,
    /// When it fires: fixed the first time its id is seen, and `None` when
    /// its schedule is not one this reads.
    pub at: Option<DateTime<Utc>>,
}

impl BackgroundWork {
    /// Replace the held work with what `stop` lists, at `at`: a task or
    /// wakeup the payload still lists keeps the time it was first stamped
    /// with, a new one is stamped with `at`, and one the payload no longer
    /// lists is dropped.
    pub fn apply_stop<Tz: TimeZone>(&mut self, stop: &StopWork, at: &DateTime<Tz>) {
        let now = at.with_timezone(&Utc);
        let tasks = stop
            .tasks
            .iter()
            .map(|task| WorkTask {
                id: task.id.clone(),
                kind: task.kind.clone(),
                description: task.description.clone(),
                since: self
                    .tasks
                    .iter()
                    .find(|held| held.id == task.id)
                    .map_or(now, |held| held.since),
            })
            .collect();
        let wakeups = stop
            .wakeups
            .iter()
            .map(|wakeup| WorkWakeup {
                id: wakeup.id.clone(),
                at: self
                    .wakeups
                    .iter()
                    .find(|held| held.id == wakeup.id)
                    .map_or_else(|| next_daily(&wakeup.schedule, at), |held| held.at),
            })
            .collect();
        self.tasks = tasks;
        self.wakeups = wakeups;
    }

    /// When the earliest pending wakeup fires, when one of them has a time.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the chat's running-work strip reads it; no caller yet"
        )
    )]
    #[must_use]
    pub fn wakeup_at(&self) -> Option<DateTime<Utc>> {
        self.wakeups.iter().filter_map(|wakeup| wakeup.at).min()
    }
}

/// The next time `schedule` fires at or after `after`, in UTC.
///
/// Only a daily schedule is read: five whitespace-separated cron fields whose
/// last three are `*`, and whose first two are the minute and the hour of the
/// local day. Any other schedule answers `None`, as does a local time that
/// does not exist that day (a spring-forward gap).
///
/// Today's occurrence is the next one even up to an hour after it has passed:
/// a one-shot wakeup fires only once the agent is idle, so the `Stop` that
/// lists it can arrive after its fire minute, and reporting tomorrow would
/// show a wakeup that has not fired yet. Past that hour it is tomorrow's.
#[must_use]
pub fn next_daily<Tz: TimeZone>(schedule: &str, after: &DateTime<Tz>) -> Option<DateTime<Utc>> {
    let fields: Vec<&str> = schedule.split_whitespace().collect();
    if fields.len() != 5 || fields[2..] != ["*", "*", "*"] {
        return None;
    }
    let minute = number(fields[0], 59)?;
    let hour = number(fields[1], 23)?;
    let zone = after.timezone();
    let date = after.date_naive();
    let candidate = local_time(&zone, date, hour, minute)?;
    // A time that passed more than an hour ago is the same time tomorrow.
    let candidate = if after.timestamp() - candidate.timestamp() > OVERDUE_GRACE_SECS {
        let next = date.succ_opt()?;
        local_time(&zone, next, hour, minute)?
    } else {
        candidate
    };
    Some(candidate.with_timezone(&Utc))
}

/// `field` as a number no greater than `max`, when it is all digits.
fn number(field: &str, max: u32) -> Option<u32> {
    if field.is_empty() || !field.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    field.parse::<u32>().ok().filter(|value| *value <= max)
}

/// `date` at `hour`:`minute` in `zone`, when that local time exists there.
///
/// An ambiguous local time (a fall-back hour that happens twice) takes the
/// earlier of the two, so the wakeup does not slip to the second hour.
fn local_time<Tz: TimeZone>(
    zone: &Tz,
    date: NaiveDate,
    hour: u32,
    minute: u32,
) -> Option<DateTime<Tz>> {
    zone.from_local_datetime(&date.and_hms_opt(hour, minute, 0)?)
        .earliest()
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use chrono::{Duration, FixedOffset};

    /// The zone the tests run in: two hours east of UTC, with no daylight
    /// saving, so every local time below exists.
    fn zone() -> FixedOffset {
        FixedOffset::east_opt(2 * 3600).expect("a valid offset")
    }

    /// The local time the tests start from: noon on 2026-10-30.
    fn noon() -> DateTime<FixedOffset> {
        zone()
            .with_ymd_and_hms(2026, 10, 30, 12, 0, 0)
            .single()
            .expect("an unambiguous local time")
    }

    /// The UTC time `month`/`day` at `hour`:`minute` in 2026.
    fn utc(month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, month, day, hour, minute, 0)
            .single()
            .expect("an unambiguous UTC time")
    }

    /// A `StopWork` listing `tasks` and no wakeups.
    fn stop_with_tasks(tasks: Vec<agent_api::hook_vocabulary::StopTask>) -> StopWork {
        StopWork {
            tasks,
            wakeups: Vec::new(),
        }
    }

    /// A `StopWork` listing `wakeups` and no tasks.
    fn stop_with_wakeups(wakeups: Vec<agent_api::hook_vocabulary::StopWakeup>) -> StopWork {
        StopWork {
            tasks: Vec::new(),
            wakeups,
        }
    }

    /// A running shell task `id`.
    fn shell(id: &str) -> agent_api::hook_vocabulary::StopTask {
        agent_api::hook_vocabulary::StopTask {
            id: id.to_owned(),
            kind: StopTaskKind::Shell { command: None },
            description: String::new(),
        }
    }

    /// A running subagent task `id`.
    fn subagent(id: &str) -> agent_api::hook_vocabulary::StopTask {
        agent_api::hook_vocabulary::StopTask {
            id: id.to_owned(),
            kind: StopTaskKind::Subagent { agent_type: None },
            description: String::new(),
        }
    }

    /// A pending wakeup `id` on `schedule`.
    fn wakeup(id: &str, schedule: &str) -> agent_api::hook_vocabulary::StopWakeup {
        agent_api::hook_vocabulary::StopWakeup {
            id: id.to_owned(),
            schedule: schedule.to_owned(),
        }
    }

    #[test]
    fn next_daily_reads_only_a_daily_schedule() {
        let after = noon();
        assert_eq!(
            next_daily("21 12 * * *", &after),
            Some(utc(10, 30, 10, 21)),
            "later today"
        );
        assert_eq!(
            next_daily("0 12 * * *", &after),
            Some(utc(10, 30, 10, 0)),
            "exactly now counts as at or after"
        );
        assert_eq!(
            next_daily("0 9 * * *", &after),
            Some(utc(10, 31, 7, 0)),
            "a time already past today is tomorrow's"
        );
        assert_eq!(
            next_daily("21 11 * * *", &after),
            Some(utc(10, 30, 9, 21)),
            "a time that passed within the last hour is still today's"
        );
        assert_eq!(
            next_daily("0 11 * * *", &after),
            Some(utc(10, 30, 9, 0)),
            "exactly an hour past is still today's"
        );
        assert_eq!(
            next_daily("59 10 * * *", &after),
            Some(utc(10, 31, 8, 59)),
            "more than an hour past is tomorrow's"
        );
        for schedule in [
            "*/5 * * * *",
            "21 12 1 * *",
            "60 12 * * *",
            "21 24 * * *",
            "21 12 * *",
            "21 12 * * * *",
            "a 12 * * *",
            "",
        ] {
            assert_eq!(next_daily(schedule, &after), None, "{schedule:?}");
        }
    }

    #[test]
    fn a_task_keeps_its_first_seen_stamp() {
        let t1 = noon();
        let t2 = t1 + Duration::hours(1);
        let t3 = t2 + Duration::hours(1);
        let mut work = BackgroundWork::default();
        work.apply_stop(&stop_with_tasks(vec![shell("a")]), &t1);
        assert_eq!(work.tasks.len(), 1, "the stop's one task");
        assert_eq!(work.tasks[0].since, t1.with_timezone(&Utc));

        work.apply_stop(&stop_with_tasks(vec![shell("a"), subagent("b")]), &t2);
        assert_eq!(work.tasks.len(), 2, "both tasks it lists");
        assert_eq!(work.tasks[0].id, "a");
        assert_eq!(
            work.tasks[0].since,
            t1.with_timezone(&Utc),
            "a task already held keeps the time it was first seen"
        );
        assert_eq!(work.tasks[1].id, "b");
        assert_eq!(
            work.tasks[1].since,
            t2.with_timezone(&Utc),
            "a new task is stamped now"
        );

        work.apply_stop(&stop_with_tasks(vec![subagent("b")]), &t3);
        assert_eq!(
            work.tasks.len(),
            1,
            "a task the stop no longer lists is dropped"
        );
        assert_eq!(work.tasks[0].id, "b");
        assert_eq!(work.tasks[0].since, t2.with_timezone(&Utc));
    }

    #[test]
    fn a_wakeup_time_does_not_slide() {
        let t1 = noon();
        let t2 = t1 + Duration::hours(25);
        let first = utc(10, 30, 10, 21);
        let mut work = BackgroundWork::default();
        work.apply_stop(&stop_with_wakeups(vec![wakeup("w", "21 12 * * *")]), &t1);
        assert_eq!(
            work.wakeups[0].at,
            Some(first),
            "21 minutes past noon, locally"
        );

        work.apply_stop(&stop_with_wakeups(vec![wakeup("w", "21 12 * * *")]), &t2);
        assert_eq!(
            work.wakeups[0].at,
            Some(first),
            "the time is fixed the first time its id is seen"
        );

        // A stop that no longer lists it drops it; the earliest time a
        // wakeup has wins, and one whose schedule is unreadable has none.
        let t3 = t1 + Duration::minutes(1);
        work.apply_stop(
            &stop_with_wakeups(vec![
                wakeup("late", "30 12 * * *"),
                wakeup("unreadable", "* * * * *"),
                wakeup("early", "10 12 * * *"),
            ]),
            &t3,
        );
        assert!(
            work.wakeups.iter().all(|held| held.id != "w"),
            "a wakeup the stop no longer lists is dropped"
        );
        assert_eq!(
            work.wakeups[1].at, None,
            "a schedule it cannot read has no time"
        );
        assert_eq!(
            work.wakeup_at(),
            Some(utc(10, 30, 10, 10)),
            "the earliest time among the wakeups that have one"
        );
    }
}
