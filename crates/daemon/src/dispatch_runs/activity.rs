//! The dispatch runs' half of the status-change log: a run's status lines are
//! queued while the runs lock is held, so they follow the list's own order,
//! and written by one appender thread, off the lock.

use chrono::{DateTime, Utc};
use protocol::{DispatchRunStatus, SessionStatus};
use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::thread::JoinHandle;

/// One status line queued for a run's log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunLogLine {
    pub(crate) key: String,
    pub(crate) at: DateTime<Utc>,
    pub(crate) status: SessionStatus,
}

/// The line a run status logs: a run that has ended logs `Stopped`, and every
/// other status keeps it `Working`.
pub(crate) fn logged_status(status: DispatchRunStatus) -> SessionStatus {
    match status {
        DispatchRunStatus::Ended => SessionStatus::Stopped,
        DispatchRunStatus::Starting
        | DispatchRunStatus::Working
        | DispatchRunStatus::Stopping
        | DispatchRunStatus::Unknown => SessionStatus::Working,
    }
}

/// Start the thread that appends every line sent on the returned sender to its
/// key's log, until every sender is dropped.
pub(crate) fn spawn_appender(
    dir: PathBuf,
) -> std::io::Result<(Sender<RunLogLine>, JoinHandle<()>)> {
    let (tx, rx) = mpsc::channel::<RunLogLine>();
    let appender = std::thread::Builder::new()
        .name("dispatch-activity".to_owned())
        .spawn(move || {
            for line in rx {
                crate::activity_log::append(&dir, &line.key, line.at, line.status);
            }
        })?;
    Ok((tx, appender))
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests set up their scratch folder and send lines with expect"
)]
mod tests {
    use super::*;

    /// A fresh scratch activity folder for `tag`.
    fn scratch_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rt-dispatch-activity-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    #[test]
    fn the_appender_writes_each_line_to_its_keys_log() {
        let dir = scratch_dir("appender");
        let (tx, appender) = spawn_appender(dir.clone()).expect("the appender starts");
        let at = Utc::now();
        for (run_id, when, status) in [
            ("a", at, SessionStatus::Working),
            (
                "a",
                at + chrono::Duration::seconds(1),
                SessionStatus::Stopped,
            ),
            ("b", at, SessionStatus::Working),
        ] {
            tx.send(RunLogLine {
                key: crate::activity_log::dispatch_key(run_id),
                at: when,
                status,
            })
            .expect("the appender takes the line");
        }
        drop(tx);
        appender.join().expect("the appender returns");

        assert_eq!(
            crate::activity_log::read_lines(&dir, &crate::activity_log::dispatch_key("a")),
            vec![
                (at, SessionStatus::Working),
                (at + chrono::Duration::seconds(1), SessionStatus::Stopped),
            ]
        );
        assert_eq!(
            crate::activity_log::read_lines(&dir, &crate::activity_log::dispatch_key("b")),
            vec![(at, SessionStatus::Working)]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
