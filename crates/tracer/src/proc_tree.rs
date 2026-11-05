//! Unix: the processes of a session's shell, read while the shell still
//! lives, and their kill.
//!
//! A process belongs to the session when its session id equals the shell's
//! pid (portable-pty makes the shell a session leader), or when a parent-pid
//! walk from the shell reaches it. The session id still finds a background
//! job in a process group of its own and a grandchild reparented to pid 1
//! after its parent exited; the walk adds a descendant that left the session
//! with `setsid`, while its parent link still holds. Pid reuse is guarded by
//! the start time: a pid is signalled only while its start time matches the
//! snapshot's.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};
use tracing::{debug, info};

/// Grace between SIGTERM and SIGKILL, for the shell's own group kill and the
/// tree kill alike.
pub const KILL_GRACE: Duration = Duration::from_millis(150);

/// How often the tree kill checks, within [`KILL_GRACE`], whether every
/// process is gone.
const GONE_POLL: Duration = Duration::from_millis(10);

/// One process of a session, as read for its kill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Proc {
    pub pid: i32,
    /// Epoch seconds, as `sysinfo` reports it.
    pub start_time: u64,
    pub pgid: i32,
}

/// The parent and start time of a live process.
struct Entry {
    parent: Option<i32>,
    start_time: u64,
}

/// Every process now running (threads left out), keyed by pid.
fn process_table() -> HashMap<i32, Entry> {
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::new());
    sys.processes()
        .iter()
        .filter(|(_, process)| process.thread_kind().is_none())
        .filter_map(|(pid, process)| {
            let pid = i32::try_from(pid.as_u32()).ok()?;
            let parent = process
                .parent()
                .and_then(|parent| i32::try_from(parent.as_u32()).ok());
            Some((
                pid,
                Entry {
                    parent,
                    start_time: process.start_time(),
                },
            ))
        })
        .collect()
}

fn own_pid() -> i32 {
    i32::try_from(std::process::id()).unwrap_or(0)
}

fn own_group() -> i32 {
    // SAFETY: `getpgrp` takes no arguments and cannot fail.
    unsafe { libc::getpgrp() }
}

fn session_of(pid: i32) -> Option<i32> {
    // SAFETY: `getsid` only reads the pid; a missing process returns -1.
    let sid = unsafe { libc::getsid(pid) };
    (sid > 0).then_some(sid)
}

fn group_of(pid: i32) -> Option<i32> {
    // SAFETY: `getpgid` only reads the pid; a missing process returns -1.
    let pgid = unsafe { libc::getpgid(pid) };
    (pgid > 0).then_some(pgid)
}

/// Sends `signal` to `pid`. Best-effort: a process already gone is not an error.
fn signal_pid(pid: i32, signal: libc::c_int) {
    // SAFETY: `kill` only inspects the pid and signal number.
    unsafe {
        libc::kill(pid, signal);
    }
}

/// Sends `signal` to the process group `pgid`. Best-effort, like `signal_pid`.
fn signal_group(pgid: i32, signal: libc::c_int) {
    // SAFETY: `killpg` only inspects the pgid and signal number.
    unsafe {
        libc::killpg(pgid, signal);
    }
}

/// Whether `pid` still exists (a zombie counts as existing).
fn exists(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that `pid` exists; nothing is sent.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// The processes of the session whose leader is `root`: every process whose
/// session id is `root`, plus every descendant a parent-pid walk from `root`
/// reaches. Pid 1, the tracer itself and a process whose start time cannot be
/// read are left out.
pub fn snapshot(root: u32) -> Vec<Proc> {
    let Ok(root) = i32::try_from(root) else {
        return Vec::new();
    };
    let table = process_table();
    let mut members = session_members(&table, root);
    let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
    for (&pid, entry) in &table {
        if let Some(parent) = entry.parent {
            children.entry(parent).or_default().push(pid);
        }
    }
    let mut visited = HashSet::new();
    let mut stack = vec![root];
    while let Some(pid) = stack.pop() {
        if !visited.insert(pid) {
            continue;
        }
        if table.contains_key(&pid) {
            members.insert(pid);
        }
        if let Some(kids) = children.get(&pid) {
            stack.extend(kids);
        }
    }
    to_procs(&table, members)
}

/// The start time of the live process `pid`, or `None` when no such process
/// exists or its start time cannot be read.
pub fn start_time_of(pid: u32) -> Option<u64> {
    let pid = i32::try_from(pid).ok()?;
    process_table()
        .get(&pid)
        .map(|entry| entry.start_time)
        .filter(|&start_time| start_time != 0)
}

/// Whether the shutdown may scan for the session of a shell that has exited:
/// only when no live process holds the shell's pid (`live_root_start` is
/// `None`), or the one that does has the shell's recorded start time. A
/// holder with another start time, or any holder when the shell's start time
/// was never read, may be an unrelated process that reused the pid.
fn fresh_scan_allowed(recorded_root_start: Option<u64>, live_root_start: Option<u64>) -> bool {
    match live_root_start {
        None => true,
        Some(live) => recorded_root_start == Some(live),
    }
}

/// The processes to kill at shutdown: `stash` (the stop's snapshots) merged
/// with a fresh scan of the processes whose session id is `root`, the
/// shell's pid. The shell has exited by then and its parent links are gone,
/// so the fresh scan selects by session id alone; it is skipped, leaving the
/// stash alone, when the shell's pid was reused (see `fresh_scan_allowed`).
pub fn shutdown_tree(root: u32, root_start: Option<u64>, stash: Vec<Proc>) -> Vec<Proc> {
    let Ok(root) = i32::try_from(root) else {
        return stash;
    };
    let table = process_table();
    let live_root_start = table.get(&root).map(|entry| entry.start_time);
    if !fresh_scan_allowed(root_start, live_root_start) {
        debug!(
            root,
            ?root_start,
            ?live_root_start,
            "supervisor: tree kill: the shell's pid was reused; using the stop snapshot"
        );
        return stash;
    }
    let members = session_members(&table, root);
    merge(stash, to_procs(&table, members))
}

/// Every pid in `table` whose session id is `root`.
fn session_members(table: &HashMap<i32, Entry>, root: i32) -> HashSet<i32> {
    table
        .keys()
        .copied()
        .filter(|&pid| session_of(pid) == Some(root))
        .collect()
}

/// `members` as kill entries, sorted by pid: pid 1, the tracer itself and a
/// process whose start time or group cannot be read are left out.
fn to_procs(table: &HashMap<i32, Entry>, members: HashSet<i32>) -> Vec<Proc> {
    let own = own_pid();
    let mut procs: Vec<Proc> = members
        .into_iter()
        .filter(|&pid| pid > 1 && pid != own)
        .filter_map(|pid| {
            let start_time = table.get(&pid)?.start_time;
            if start_time == 0 {
                debug!(
                    pid,
                    "supervisor: tree kill: start time unreadable; left out"
                );
                return None;
            }
            let pgid = group_of(pid)?;
            Some(Proc {
                pid,
                start_time,
                pgid,
            })
        })
        .collect();
    procs.sort_unstable_by_key(|p| p.pid);
    procs
}

/// `first` then the entries of `second` it lacks, one per pid and start time.
pub fn merge(first: Vec<Proc>, second: Vec<Proc>) -> Vec<Proc> {
    let mut seen = HashSet::new();
    first
        .into_iter()
        .chain(second)
        .filter(|p| seen.insert((p.pid, p.start_time)))
        .collect()
}

/// Whether `table` still has `p` under the same start time; logs a reused pid.
fn still_same(table: &HashMap<i32, Entry>, p: &Proc) -> bool {
    match table.get(&p.pid) {
        Some(entry) if entry.start_time == p.start_time => true,
        Some(entry) => {
            debug!(
                pid = p.pid,
                snapshot_start = p.start_time,
                live_start = entry.start_time,
                "supervisor: tree kill: pid reused; skipped"
            );
            false
        }
        None => false,
    }
}

/// Kills the processes of `snapshot` still running under their snapshot
/// start time: SIGTERM to each distinct process group, then to each process
/// outside a group already signalled; up to [`KILL_GRACE`] for them to go;
/// then SIGKILL to each pid whose start time still matches. Pid 1, the
/// tracer's own pid and its own group are never signalled. Nor are `root`
/// (the shell) and its group, as a group: the shell is the tracer's own
/// child, ended by the child kill and `kill_process_group`; its group's other
/// members are signalled one by one.
pub fn kill_tree(root: u32, snapshot: &[Proc]) {
    let root = i32::try_from(root).unwrap_or(0);
    let own = own_pid();
    let own_group = own_group();
    let table = process_table();
    let targets: Vec<Proc> = snapshot
        .iter()
        .copied()
        .filter(|p| p.pid > 1 && p.pid != own && p.pid != root && still_same(&table, p))
        .collect();
    // A group is signalled only through a member still in it, so a group
    // that member left since the snapshot is not reached by a stale pgid.
    let mut groups: Vec<i32> = targets
        .iter()
        .filter(|p| {
            p.pgid > 1 && p.pgid != own_group && p.pgid != root && group_of(p.pid) == Some(p.pgid)
        })
        .map(|p| p.pgid)
        .collect();
    groups.sort_unstable();
    groups.dedup();
    let (processes, group_count) = (targets.len(), groups.len());
    info!("supervisor: tree kill: {processes} processes, {group_count} groups");
    for &pgid in &groups {
        signal_group(pgid, libc::SIGTERM);
    }
    for p in targets.iter().filter(|p| !groups.contains(&p.pgid)) {
        signal_pid(p.pid, libc::SIGTERM);
    }
    let deadline = Instant::now() + KILL_GRACE;
    while targets.iter().any(|p| exists(p.pid)) && Instant::now() < deadline {
        std::thread::sleep(GONE_POLL);
    }
    if !targets.iter().any(|p| exists(p.pid)) {
        return;
    }
    let table = process_table();
    for p in targets.iter().filter(|p| still_same(&table, p)) {
        signal_pid(p.pid, libc::SIGKILL);
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests fail loudly on setup errors")]
mod tests {
    use super::*;
    use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
    use std::process::{Child, Command};
    use sysinfo::UpdateKind;

    /// Hang guard only: a poll that has not seen its condition by then fails.
    const HANG_GUARD: Duration = Duration::from_secs(60);
    const POLL_INTERVAL: Duration = Duration::from_millis(20);

    /// `/bin/sh` as the leader of a session of its own, like the PTY shell:
    /// a background job, a grandchild left behind by an exited subshell (it
    /// reparents to pid 1), and a foreground `sleep 602`.
    fn spawn_session_tree() -> Child {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 600 & sh -c \"sleep 601 &\"; sleep 602"]);
        // SAFETY: the closure runs in the forked child before exec and calls
        // only `setsid`, which is async-signal-safe and allocates nothing.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        command.spawn().expect("spawn /bin/sh")
    }

    /// The command line of each pid, joined with spaces.
    fn command_lines(pids: &[i32]) -> Vec<String> {
        let mut sys = System::new();
        sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::new().with_cmd(UpdateKind::Always),
        );
        pids.iter()
            .filter_map(|&pid| {
                let pid = sysinfo::Pid::from_u32(u32::try_from(pid).ok()?);
                let process = sys.process(pid)?;
                Some(
                    process
                        .cmd()
                        .iter()
                        .map(|arg| arg.to_string_lossy().into_owned())
                        .collect::<Vec<_>>()
                        .join(" "),
                )
            })
            .collect()
    }

    /// Polls the snapshot of `root` until it holds both `sleep 600` and
    /// `sleep 601`, failing past the hang guard.
    fn snapshot_with_both_sleeps(root: u32) -> Vec<Proc> {
        let started = Instant::now();
        loop {
            let snap = snapshot(root);
            let pids: Vec<i32> = snap.iter().map(|p| p.pid).collect();
            let lines = command_lines(&pids);
            if lines.iter().any(|l| l == "sleep 600") && lines.iter().any(|l| l == "sleep 601") {
                return snap;
            }
            assert!(
                started.elapsed() < HANG_GUARD,
                "hang guard: the snapshot never held both sleeps; last: {lines:?}"
            );
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    fn pid_gone(pid: i32) -> bool {
        // SAFETY: signal 0 only checks that `pid` exists; nothing is sent.
        let rc = unsafe { libc::kill(pid, 0) };
        rc == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    }

    #[test]
    fn snapshot_finds_background_jobs_and_a_reparented_grandchild() {
        let mut shell = spawn_session_tree();
        let root = shell.id();
        let snap = snapshot_with_both_sleeps(root);
        let leader = i32::try_from(root).expect("pid fits i32");
        assert!(
            snap.iter().any(|p| p.pid == leader),
            "the session leader is in the snapshot: {snap:?}"
        );
        kill_tree(root, &snap);
        shell.wait().expect("reap the shell");
    }

    #[test]
    fn kill_tree_ends_every_process_in_the_snapshot() {
        let mut shell = spawn_session_tree();
        let root = shell.id();
        let leader = i32::try_from(root).expect("pid fits i32");
        let snap = snapshot_with_both_sleeps(root);
        kill_tree(root, &snap);
        let started = Instant::now();
        loop {
            // Reap the shell, the test's own child, so it does not linger as
            // a zombie; the sleeps are reaped by pid 1. The shell itself is
            // left to the group kill (see the next test).
            let _ = shell.try_wait();
            let alive: Vec<i32> = snap
                .iter()
                .map(|p| p.pid)
                .filter(|&pid| pid != leader && !pid_gone(pid))
                .collect();
            if alive.is_empty() {
                break;
            }
            assert!(
                started.elapsed() < HANG_GUARD,
                "hang guard: still alive after the tree kill: {alive:?}"
            );
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    #[test]
    fn kill_tree_leaves_the_root_to_the_group_kill() {
        let mut shell = spawn_session_tree();
        let root = shell.id();
        let snap = snapshot_with_both_sleeps(root);
        kill_tree(root, &snap);
        // The tree kill ends the foreground `sleep 602`, so the shell runs to
        // the end of its script and exits on its own; a shell that was
        // signalled itself would report the signal instead.
        let status = shell.wait().expect("reap the shell");
        assert_eq!(
            status.signal(),
            None,
            "the root was signalled by the tree kill: {status:?}"
        );
    }

    #[test]
    fn fresh_scan_is_skipped_when_the_root_pid_is_reused() {
        // Reused: a live process holds the root pid under another start time.
        assert!(!fresh_scan_allowed(Some(100), Some(200)));
        // Unknown recorded start: a live holder cannot be told apart.
        assert!(!fresh_scan_allowed(None, Some(100)));
        // No live holder, or the shell itself still there.
        assert!(fresh_scan_allowed(Some(100), None));
        assert!(fresh_scan_allowed(None, None));
        assert!(fresh_scan_allowed(Some(100), Some(100)));
    }

    #[test]
    fn kill_tree_skips_a_pid_whose_start_time_changed() {
        let mut child = Command::new("sleep")
            .arg("600")
            .process_group(0)
            .spawn()
            .expect("spawn sleep");
        let pid = i32::try_from(child.id()).expect("pid fits i32");
        let mut sys = System::new();
        sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::new());
        let start_time = sys
            .process(sysinfo::Pid::from_u32(child.id()))
            .expect("the child is in the process table")
            .start_time();
        assert_ne!(start_time, 0, "the child's start time is readable");
        let stale = Proc {
            pid,
            start_time: start_time + 1,
            pgid: pid,
        };
        // Root pid 1: never a target, so it cannot be what spares the child.
        kill_tree(1, &[stale]);
        let still_running = child.try_wait().expect("poll the child").is_none();
        child.kill().expect("kill the child");
        child.wait().expect("reap the child");
        assert!(
            still_running,
            "a pid whose start time changed is not signalled"
        );
    }
}
