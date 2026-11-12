//! A live run's process guard: is the run's child still the process the
//! live-run file names (`docs/plans/dispatch-follow.md` section 6)?
//!
//! On Windows with a recorded start, the child's creation FILETIME must equal
//! it exactly, the dispatch tool's own rule, so a recycled pid never passes.
//! Without one (a tool off Windows), the process's start time from `sysinfo`
//! must fall within 60 s before to 5 s after the file's `started_at`.

use chrono::{DateTime, Utc};
#[cfg(windows)]
use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
#[cfg(windows)]
use windows::Win32::System::Threading::{
    GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// How long before the file's `started_at` the process may have started.
const START_BEFORE_SECS: i64 = 60;
/// How long after the file's `started_at` the process may have started.
const START_AFTER_SECS: i64 = 5;

/// What the guard could tell of a run's child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Liveness {
    /// Running, and the process the file recorded.
    Alive,
    /// Known gone: no such pid, exited, or a different process on that pid.
    Dead,
    /// Something could not be read; counted as alive, as the tool does.
    Unknown,
}

/// What `OpenProcess` and the reads through its handle gave (Windows).
#[cfg_attr(
    all(not(windows), not(test)),
    expect(dead_code, reason = "only the Windows guard opens a process")
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessRead {
    /// `OpenProcess` failed with `ERROR_INVALID_PARAMETER`: no process has
    /// that pid.
    NoSuchPid,
    /// `OpenProcess` failed otherwise, such as `ERROR_ACCESS_DENIED` for a
    /// process this user may not query.
    OpenFailed,
    /// The process opened; each read is `None` when it failed.
    Opened {
        exit_code: Option<u32>,
        created: Option<u64>,
    },
}

/// What `sysinfo` gave of a pid's start time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartRead {
    /// Listed, started at this Unix second (0 when it could not be read).
    Started(u64),
    /// Not listed, and nothing else says a process has that pid.
    Absent,
    /// Not listed, but the pid exists (a signal 0 reached it or was refused).
    #[cfg_attr(
        all(not(unix), not(test)),
        expect(
            dead_code,
            reason = "only the Unix signal check finds a pid it cannot read"
        )
    )]
    Unreadable,
}

/// The exit code `GetExitCodeProcess` reports for a process still running.
#[cfg_attr(
    all(not(windows), not(test)),
    expect(dead_code, reason = "only the Windows guard reads an exit code")
)]
pub(crate) const STILL_ACTIVE: u32 = 259;

/// The verdict on a Windows read, as the tool's `runner_alive` decides it:
/// dead only on evidence. Without a recorded start, the `sysinfo` start time
/// decides once the process is known to be running.
#[cfg_attr(
    all(not(windows), not(test)),
    expect(dead_code, reason = "only the Windows guard opens a process")
)]
fn classify_process(
    read: ProcessRead,
    child_start: Option<u64>,
    start_read: Option<StartRead>,
    started_at: DateTime<Utc>,
) -> Liveness {
    let (exit_code, created) = match read {
        ProcessRead::NoSuchPid => return Liveness::Dead,
        ProcessRead::OpenFailed => return Liveness::Unknown,
        ProcessRead::Opened { exit_code, created } => (exit_code, created),
    };
    if exit_code.is_some_and(|code| code != STILL_ACTIVE) {
        return Liveness::Dead;
    }
    let identity = match child_start {
        Some(start) => match created {
            None => Liveness::Unknown,
            Some(created) if created == start => Liveness::Alive,
            Some(_) => return Liveness::Dead,
        },
        None => start_read.map_or(Liveness::Unknown, |read| classify_start(read, started_at)),
    };
    if exit_code.is_none() && identity == Liveness::Alive {
        Liveness::Unknown
    } else {
        identity
    }
}

/// The verdict on a `sysinfo` read: the start time must fall in the window
/// around `started_at`; a start of 0 or an unreadable pid is unknown.
fn classify_start(read: StartRead, started_at: DateTime<Utc>) -> Liveness {
    match read {
        StartRead::Started(0) | StartRead::Unreadable => Liveness::Unknown,
        StartRead::Started(start) if start_within_window(start, started_at) => Liveness::Alive,
        StartRead::Started(_) | StartRead::Absent => Liveness::Dead,
    }
}

/// What the guard can tell of the run's child now.
pub(crate) fn liveness(
    child_pid: u32,
    child_start: Option<u64>,
    started_at: DateTime<Utc>,
) -> Liveness {
    #[cfg(windows)]
    {
        let (read, _held) = read_process(child_pid);
        let start_read = (child_start.is_none() && matches!(read, ProcessRead::Opened { .. }))
            .then(|| read_start(child_pid));
        classify_process(read, child_start, start_read, started_at)
    }
    #[cfg(not(windows))]
    {
        let _ = child_start;
        classify_start(read_start(child_pid), started_at)
    }
}

/// Whether the run may still be alive: false only when the guard knows its
/// child is gone or is another process.
pub(crate) fn process_alive(
    child_pid: u32,
    child_start: Option<u64>,
    started_at: DateTime<Utc>,
) -> bool {
    liveness(child_pid, child_start, started_at) != Liveness::Dead
}

/// Open `pid` and read its exit code and creation time, keeping the handle
/// open for the caller's further reads.
#[cfg(windows)]
fn read_process(pid: u32) -> (ProcessRead, Option<GuardedProcess>) {
    use windows::Win32::Foundation::ERROR_INVALID_PARAMETER;
    // SAFETY: a plain open by pid; the handle is closed by `GuardedProcess`.
    match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
        Ok(handle) => {
            let process = GuardedProcess { handle };
            let read = ProcessRead::Opened {
                exit_code: process.exit_code(),
                created: process.creation_filetime(),
            };
            (read, Some(process))
        }
        Err(err) if err.code() == ERROR_INVALID_PARAMETER.to_hresult() => {
            (ProcessRead::NoSuchPid, None)
        }
        Err(err) => {
            tracing::debug!(pid, %err, "dispatch_runs: opening a run's process failed; its liveness is unknown");
            (ProcessRead::OpenFailed, None)
        }
    }
}

/// `pid`'s start time from `sysinfo`.
fn read_start(pid: u32) -> StartRead {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    let mut sys = System::new();
    let sys_pid = Pid::from_u32(pid);
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[sys_pid]),
        true,
        ProcessRefreshKind::new(),
    );
    match sys.process(sys_pid) {
        Some(process) => StartRead::Started(process.start_time()),
        None => absent_read(pid),
    }
}

/// A pid `sysinfo` does not list: on Unix a signal 0 tells a gone pid
/// (ESRCH) from one this user may not read (EPERM), as the tool's
/// `_signal_reaches` does.
#[cfg(unix)]
fn absent_read(pid: u32) -> StartRead {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return StartRead::Absent;
    };
    // SAFETY: signal 0 sends nothing; it only checks that `pid` exists.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return StartRead::Unreadable;
    }
    if std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
        StartRead::Absent
    } else {
        StartRead::Unreadable
    }
}

/// A pid `sysinfo` does not list is gone.
#[cfg(not(unix))]
fn absent_read(_pid: u32) -> StartRead {
    StartRead::Absent
}

/// Whether a process that started at `start_unix` (Unix seconds) can be the
/// run that the file says started at `started_at`: 60 s before to 5 s after.
pub(crate) fn start_within_window(start_unix: u64, started_at: DateTime<Utc>) -> bool {
    let Ok(start) = i64::try_from(start_unix) else {
        return false;
    };
    let at = started_at.timestamp();
    (at - START_BEFORE_SECS..=at + START_AFTER_SECS).contains(&start)
}

/// An open handle to a run's child, which keeps its pid from being reused
/// while it is held; closed on drop.
#[cfg(windows)]
#[derive(Debug)]
pub(crate) struct GuardedProcess {
    handle: HANDLE,
}

#[cfg(windows)]
impl GuardedProcess {
    /// Open `pid` for limited queries.
    fn open_pid(pid: u32) -> Option<Self> {
        // SAFETY: a plain open by pid; the handle is closed on drop.
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
        Some(Self { handle })
    }

    /// The process's creation time as a FILETIME read as one `u64`.
    fn creation_filetime(&self) -> Option<u64> {
        let mut creation = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        // SAFETY: the handle is open with query rights; every out-pointer is
        // a live local.
        unsafe {
            GetProcessTimes(
                self.handle,
                &raw mut creation,
                &raw mut exit,
                &raw mut kernel,
                &raw mut user,
            )
        }
        .ok()?;
        Some((u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime))
    }

    /// The process's exit code, [`STILL_ACTIVE`] while it runs; `None` when
    /// it cannot be read. An exited process stays openable while anyone
    /// holds a handle to it.
    fn exit_code(&self) -> Option<u32> {
        let mut code = 0u32;
        // SAFETY: the handle is open with query rights; `code` is a live local.
        unsafe { GetExitCodeProcess(self.handle, &raw mut code) }.ok()?;
        Some(code)
    }

    /// The held process's exit code now, [`STILL_ACTIVE`] while it runs;
    /// `None` when it cannot be read.
    pub(crate) fn current_exit_code(&self) -> Option<u32> {
        self.exit_code()
    }
}

#[cfg(windows)]
impl Drop for GuardedProcess {
    fn drop(&mut self) {
        // SAFETY: the handle was opened by `open_pid` and is closed once.
        let _ = unsafe { CloseHandle(self.handle) };
    }
}

/// Open the run's child when it is running and is the process the file
/// recorded, holding its handle so the pid cannot be reused meanwhile.
/// Strict, unlike [`liveness`]: anything unreadable opens nothing.
#[cfg(windows)]
pub(crate) fn open(
    child_pid: u32,
    child_start: Option<u64>,
    started_at: DateTime<Utc>,
) -> Option<GuardedProcess> {
    let process = GuardedProcess::open_pid(child_pid)?;
    if process.exit_code() != Some(STILL_ACTIVE) {
        return None;
    }
    let matches = match child_start {
        Some(start) => process.creation_filetime() == Some(start),
        None => classify_start(read_start(child_pid), started_at) == Liveness::Alive,
    };
    matches.then_some(process)
}

/// `pid`'s creation time as a FILETIME read as one `u64`, the value a
/// live-run file's `child_start` records; tests record a child's start with it.
#[cfg(all(windows, test))]
pub(crate) fn creation_filetime(pid: u32) -> Option<u64> {
    GuardedProcess::open_pid(pid)?.creation_filetime()
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests spawn their child with expect")]
mod tests {
    use super::*;
    use chrono::TimeDelta;

    #[test]
    fn start_within_window_accepts_60_s_before_to_5_s_after() {
        let started_at = DateTime::<Utc>::from_timestamp(1_800_000_000, 0).expect("a valid time");
        let at = |offset: i64| {
            let start = started_at + TimeDelta::seconds(offset);
            start_within_window(start.timestamp().try_into().expect("positive"), started_at)
        };
        assert!(!at(-61));
        assert!(at(-60));
        assert!(at(0));
        assert!(at(5));
        assert!(!at(6));
    }

    /// One Windows row: the case, the read, the file's start, the `sysinfo`
    /// read and the verdict.
    type ProcessRow = (
        &'static str,
        ProcessRead,
        Option<u64>,
        Option<StartRead>,
        Liveness,
    );

    /// The rows of the tool's `runner_alive` docstring, for a run whose file
    /// says it started at Unix second `at`.
    fn process_rows(at: u64) -> [ProcessRow; 9] {
        use Liveness::{Alive, Dead, Unknown};
        let start = Some(134_036_037_030_000_000);
        let opened = |exit_code, created| ProcessRead::Opened { exit_code, created };
        [
            (
                "no process has that pid",
                ProcessRead::NoSuchPid,
                start,
                None,
                Dead,
            ),
            (
                "open refused (access denied)",
                ProcessRead::OpenFailed,
                start,
                None,
                Unknown,
            ),
            (
                "the process has exited",
                opened(Some(0), start),
                start,
                None,
                Dead,
            ),
            (
                "the pid was reused",
                opened(Some(STILL_ACTIVE), start.map(|s| s + 1)),
                start,
                None,
                Dead,
            ),
            (
                "creation time unreadable",
                opened(Some(STILL_ACTIVE), None),
                start,
                None,
                Unknown,
            ),
            (
                "exit code unreadable",
                opened(None, start),
                start,
                None,
                Unknown,
            ),
            (
                "running with its start",
                opened(Some(STILL_ACTIVE), start),
                start,
                None,
                Alive,
            ),
            (
                "no recorded start, sysinfo start in the window",
                opened(Some(STILL_ACTIVE), start),
                None,
                Some(StartRead::Started(at)),
                Alive,
            ),
            (
                "no recorded start, sysinfo start outside the window",
                opened(Some(STILL_ACTIVE), start),
                None,
                Some(StartRead::Started(at + 6)),
                Dead,
            ),
        ]
    }

    /// The `sysinfo` path's rows, for a run whose file says it started at
    /// Unix second `at`.
    fn start_rows(at: u64) -> [(&'static str, StartRead, Liveness); 6] {
        use Liveness::{Alive, Dead, Unknown};
        [
            (
                "listed, start in the window",
                StartRead::Started(at - 60),
                Alive,
            ),
            (
                "listed, start before the window",
                StartRead::Started(at - 61),
                Dead,
            ),
            (
                "listed, start after the window",
                StartRead::Started(at + 6),
                Dead,
            ),
            (
                "listed, start unreadable (0)",
                StartRead::Started(0),
                Unknown,
            ),
            ("not listed and no such pid", StartRead::Absent, Dead),
            (
                "not listed but the pid exists (EPERM)",
                StartRead::Unreadable,
                Unknown,
            ),
        ]
    }

    /// Dead only on evidence, as the tool's `runner_alive` decides; anything
    /// unreadable is unknown.
    #[test]
    fn liveness_classifies_like_the_tool() {
        let started_at = DateTime::<Utc>::from_timestamp(1_800_000_000, 0).expect("a valid time");
        let at = u64::try_from(started_at.timestamp()).expect("positive");
        for (case, read, child_start, start_read, expected) in process_rows(at) {
            assert_eq!(
                classify_process(read, child_start, start_read, started_at),
                expected,
                "{case}"
            );
        }
        for (case, read, expected) in start_rows(at) {
            assert_eq!(classify_start(read, started_at), expected, "{case}");
        }
    }

    #[cfg(windows)]
    mod windows {
        use super::*;
        use std::process::{Child, Command, Stdio};

        /// A long-lived child, killed and waited on when dropped.
        struct Pinger(Child);

        impl Pinger {
            fn spawn() -> Self {
                let child = Command::new("cmd")
                    .args(["/C", "ping", "-n", "300", "127.0.0.1"])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("the ping child starts");
                Self(child)
            }

            fn pid(&self) -> u32 {
                self.0.id()
            }
        }

        impl Drop for Pinger {
            fn drop(&mut self) {
                self.kill_tree();
            }
        }

        impl Pinger {
            /// Kill the whole tree, `ping` included, so no grandchild outlives
            /// the test holding its inherited handles; then reap the child.
            fn kill_tree(&mut self) {
                let _ = Command::new("taskkill")
                    .args(["/T", "/F", "/PID", &self.pid().to_string()])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        #[test]
        fn guard_accepts_a_live_process_with_its_start() {
            let child = Pinger::spawn();
            let start = creation_filetime(child.pid()).expect("the child's start is read");
            assert!(process_alive(child.pid(), Some(start), Utc::now()));
        }

        #[test]
        fn guard_rejects_a_wrong_start() {
            let child = Pinger::spawn();
            let start = creation_filetime(child.pid()).expect("the child's start is read");
            assert!(!process_alive(child.pid(), Some(start + 1), Utc::now()));
        }

        #[test]
        fn guard_rejects_an_exited_process() {
            let mut child = Pinger::spawn();
            let pid = child.pid();
            let start = creation_filetime(pid).expect("the child's start is read");
            child.kill_tree();
            assert!(child.0.try_wait().expect("the child is polled").is_some());
            assert!(!process_alive(pid, Some(start), Utc::now()));
        }
    }
}
