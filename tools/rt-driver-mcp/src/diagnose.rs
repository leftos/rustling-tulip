//! What a timed-out driver call says about its client: busy (it still
//! answers `ping`), stuck (it does not, with its CPU and thread count), or
//! most likely paused at a breakpoint (a debugger is attached).
//!
//! The figures come through a [`Probe`]: [`ClientProbe`] reads a session's
//! client on Windows, and reads every figure as unknown elsewhere.

use std::process::Child;
use std::time::{Duration, Instant};

/// How long the diagnosis `ping` waits for its answer, in wall time.
pub const PING_GUARD: Duration = Duration::from_secs(2);

/// How long the client's CPU is sampled over.
const CPU_SAMPLE: Duration = Duration::from_secs(1);

/// What a diagnosis reads about the client.
pub trait Probe {
    /// Whether the client answers a `ping` on a fresh connection within [`PING_GUARD`].
    fn ping(&mut self) -> bool;
    /// The client's CPU over [`CPU_SAMPLE`], as a percent of one core.
    fn cpu_percent(&mut self) -> Option<f64>;
    /// How many threads the client runs.
    fn thread_count(&mut self) -> Option<u32>;
    /// Whether a debugger is attached to the client.
    fn debugger_attached(&mut self) -> Option<bool>;
}

/// The refusal of a call while a debugger holds the client `pid`.
#[must_use]
pub fn breakpoint(pid: u32) -> String {
    format!("the client is most likely paused at a breakpoint: a debugger is attached to pid {pid}")
}

/// One line on a client whose reply did not come: paused at a breakpoint
/// (checked first, with no ping sent), busy (it answers `ping`) or stuck,
/// with its CPU and thread count, each "unknown" when it cannot be read.
pub fn diagnose(probe: &mut dyn Probe, pid: u32) -> String {
    if probe.debugger_attached() == Some(true) {
        return breakpoint(pid);
    }
    if probe.ping() {
        return "diagnosis: busy: the client answers ping, so its UI thread runs; the request is still working"
            .to_owned();
    }
    let unknown = || "unknown".to_owned();
    let cpu = probe
        .cpu_percent()
        .map_or_else(unknown, |cpu| format!("{cpu:.0}"));
    let threads = probe
        .thread_count()
        .map_or_else(unknown, |threads| threads.to_string());
    format!(
        "diagnosis: stuck: no answer to ping within {} s; the client used {cpu}% of a core over {} s and runs {threads} threads",
        PING_GUARD.as_secs(),
        CPU_SAMPLE.as_secs()
    )
}

/// A session's client, read through its held process handle, and the pipe
/// its endpoint listens on.
pub struct ClientProbe<'a> {
    client: &'a Child,
    #[cfg(windows)]
    pipe_path: String,
}

impl<'a> ClientProbe<'a> {
    #[must_use]
    pub fn new(client: &'a Child, pipe_path: String) -> Self {
        #[cfg(not(windows))]
        let _ = pipe_path;
        Self {
            client,
            #[cfg(windows)]
            pipe_path,
        }
    }
}

impl Probe for ClientProbe<'_> {
    /// An endpoint error is an answer too; only a transport failure is none.
    #[cfg(windows)]
    fn ping(&mut self) -> bool {
        use serde_json::json;

        use crate::pipe::{CallError, PipeClient, Transport as _};

        let mut pipe = PipeClient::with_wall_reply_guard(self.pipe_path.clone(), PING_GUARD);
        !matches!(pipe.call("ping", json!({})), Err(CallError::Transport(_)))
    }

    #[cfg(not(windows))]
    fn ping(&mut self) -> bool {
        false
    }

    fn cpu_percent(&mut self) -> Option<f64> {
        let before = cpu_time(self.client)?;
        let started = Instant::now();
        std::thread::sleep(CPU_SAMPLE);
        let used = cpu_time(self.client)?.saturating_sub(before);
        let wall = started.elapsed();
        (!wall.is_zero()).then(|| used.as_secs_f64() / wall.as_secs_f64() * 100.0)
    }

    fn thread_count(&mut self) -> Option<u32> {
        thread_count(self.client.id())
    }

    #[cfg(windows)]
    fn debugger_attached(&mut self) -> Option<bool> {
        use std::os::windows::io::AsRawHandle as _;
        use windows::Win32::Foundation::HANDLE;

        debugger_present(HANDLE(self.client.as_raw_handle()))
    }

    #[cfg(not(windows))]
    fn debugger_attached(&mut self) -> Option<bool> {
        None
    }
}

/// `client`'s kernel + user CPU so far, read through its handle.
#[cfg(windows)]
fn cpu_time(client: &Child) -> Option<Duration> {
    use std::os::windows::io::AsRawHandle as _;
    use windows::Win32::Foundation::{FILETIME, HANDLE};
    use windows::Win32::System::Threading::GetProcessTimes;

    let (mut created, mut exited, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: GetProcessTimes reads a process handle with
    // PROCESS_QUERY_LIMITED_INFORMATION access and writes four FILETIMEs.
    // The handle is the client's own, alive while `client` is (the session
    // holds it, opened by CreateProcess with full access), and the
    // out-pointers are locals that outlive the call.
    let read = unsafe {
        GetProcessTimes(
            HANDLE(client.as_raw_handle()),
            &raw mut created,
            &raw mut exited,
            &raw mut kernel,
            &raw mut user,
        )
    };
    if let Err(err) = read {
        tracing::debug!(%err, "reading the client's CPU time");
        return None;
    }
    let units =
        |time: FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
    // 100 ns units.
    let cpu = units(kernel).saturating_add(units(user));
    Some(Duration::from_nanos(cpu.saturating_mul(100)))
}

#[cfg(not(windows))]
fn cpu_time(_client: &Child) -> Option<Duration> {
    None
}

/// How many threads process `pid` runs, from a thread snapshot of the system.
#[cfg(windows)]
fn thread_count(pid: u32) -> Option<u32> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD,
    };

    // SAFETY: CreateToolhelp32Snapshot takes two plain values (with
    // TH32CS_SNAPTHREAD the process id is ignored and every thread of the
    // system is included) and returns a new handle, closed below.
    let snapshot = match unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) } {
        Ok(snapshot) => snapshot,
        Err(err) => {
            tracing::debug!(%err, "taking a thread snapshot");
            return None;
        }
    };
    let count = count_threads(snapshot, pid);
    // SAFETY: `snapshot` is the open handle CreateToolhelp32Snapshot returned,
    // closed once here as its documentation asks, and not used after.
    if let Err(err) = unsafe { CloseHandle(snapshot) } {
        tracing::debug!(%err, "closing the thread snapshot");
    }
    count
}

/// The entries of `snapshot` whose owner is `pid`.
#[cfg(windows)]
fn count_threads(snapshot: windows::Win32::Foundation::HANDLE, pid: u32) -> Option<u32> {
    use windows::Win32::System::Diagnostics::ToolHelp::{
        THREADENTRY32, Thread32First, Thread32Next,
    };

    let mut entry = THREADENTRY32 {
        dwSize: u32::try_from(std::mem::size_of::<THREADENTRY32>()).ok()?,
        ..THREADENTRY32::default()
    };
    // SAFETY: `snapshot` is an open thread snapshot the caller holds, and
    // `entry` is a local THREADENTRY32 whose dwSize is set to its size, as
    // Thread32First requires before it copies the first entry into it.
    if let Err(err) = unsafe { Thread32First(snapshot, &raw mut entry) } {
        tracing::debug!(%err, "reading the thread snapshot's first entry");
        return None;
    }
    let mut count = 0;
    loop {
        if entry.th32OwnerProcessID == pid {
            count += 1;
        }
        // SAFETY: as for Thread32First; it fails with ERROR_NO_MORE_FILES
        // past the last entry, which ends the walk.
        if unsafe { Thread32Next(snapshot, &raw mut entry) }.is_err() {
            return Some(count);
        }
    }
}

#[cfg(not(windows))]
fn thread_count(_pid: u32) -> Option<u32> {
    None
}

/// Whether a debugger is attached to `process`, read with `CheckRemoteDebuggerPresent`.
#[cfg(windows)]
fn debugger_present(process: windows::Win32::Foundation::HANDLE) -> Option<bool> {
    use windows::Win32::System::Diagnostics::Debug::CheckRemoteDebuggerPresent;
    use windows::core::BOOL;

    let mut present = BOOL::default();
    // SAFETY: CheckRemoteDebuggerPresent reads a process handle and writes
    // one BOOL. The caller passes an open process handle (a session's held
    // client, or this process's pseudo-handle), and `present` is a local
    // that outlives the call.
    match unsafe { CheckRemoteDebuggerPresent(process, &raw mut present) } {
        Ok(()) => Some(present.as_bool()),
        Err(err) => {
            tracing::debug!(%err, "checking for a debugger on the client");
            None
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{Probe, diagnose};

    /// A probe that answers from its fields and records whether it was pinged.
    pub(crate) struct ScriptedProbe {
        pub(crate) debugger: Option<bool>,
        pub(crate) answers_ping: bool,
        pub(crate) cpu: Option<f64>,
        pub(crate) threads: Option<u32>,
        pub(crate) pinged: bool,
    }

    impl ScriptedProbe {
        /// No debugger, no answer to ping, and the given figures.
        pub(crate) fn stuck(cpu: Option<f64>, threads: Option<u32>) -> ScriptedProbe {
            ScriptedProbe {
                debugger: Some(false),
                answers_ping: false,
                cpu,
                threads,
                pinged: false,
            }
        }
    }

    impl Probe for ScriptedProbe {
        fn ping(&mut self) -> bool {
            self.pinged = true;
            self.answers_ping
        }

        fn cpu_percent(&mut self) -> Option<f64> {
            self.cpu
        }

        fn thread_count(&mut self) -> Option<u32> {
            self.threads
        }

        fn debugger_attached(&mut self) -> Option<bool> {
            self.debugger
        }
    }

    #[test]
    fn diagnose_reads_busy_when_ping_answers() {
        let mut probe = ScriptedProbe {
            answers_ping: true,
            ..ScriptedProbe::stuck(Some(50.0), Some(30))
        };
        assert_eq!(
            diagnose(&mut probe, 42),
            "diagnosis: busy: the client answers ping, so its UI thread runs; the request is still working"
        );
        assert!(probe.pinged);
    }

    #[test]
    fn diagnose_reads_stuck_with_cpu_and_threads_when_ping_does_not() {
        let mut idle = ScriptedProbe::stuck(Some(0.0), Some(41));
        assert_eq!(
            diagnose(&mut idle, 42),
            "diagnosis: stuck: no answer to ping within 2 s; the client used 0% of a core over 1 s and runs 41 threads"
        );
        let mut spinning = ScriptedProbe::stuck(Some(93.4), Some(7));
        assert_eq!(
            diagnose(&mut spinning, 42),
            "diagnosis: stuck: no answer to ping within 2 s; the client used 93% of a core over 1 s and runs 7 threads"
        );
        assert!(idle.pinged && spinning.pinged);
    }

    #[test]
    fn diagnose_says_unknown_for_a_figure_it_cannot_read() {
        let mut probe = ScriptedProbe {
            debugger: None,
            ..ScriptedProbe::stuck(None, None)
        };
        assert_eq!(
            diagnose(&mut probe, 42),
            "diagnosis: stuck: no answer to ping within 2 s; the client used unknown% of a core over 1 s and runs unknown threads"
        );
    }

    #[test]
    fn diagnose_reads_a_breakpoint_and_sends_no_ping_when_a_debugger_is_attached() {
        let mut probe = ScriptedProbe {
            debugger: Some(true),
            answers_ping: true,
            ..ScriptedProbe::stuck(Some(0.0), Some(41))
        };
        assert_eq!(
            diagnose(&mut probe, 42),
            "the client is most likely paused at a breakpoint: a debugger is attached to pid 42"
        );
        assert!(!probe.pinged, "no ping goes to a client a debugger holds");
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use windows::Win32::System::Threading::GetCurrentProcess;

    use super::debugger_present;

    #[test]
    fn check_remote_debugger_present_reads_false_for_this_test_process() {
        // SAFETY: takes no arguments and returns this process's pseudo-handle,
        // which needs no closing.
        let this = unsafe { GetCurrentProcess() };
        assert_eq!(debugger_present(this), Some(false));
    }
}
