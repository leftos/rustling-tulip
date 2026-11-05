//! The login shell's `PATH` for a daemon that launchd started with a minimal
//! one.
//!
//! launchd gives the processes it starts a `PATH` of the system directories
//! only, so a daemon launched from Finder, the Dock or the login item cannot
//! find the agent CLI, `node`, Homebrew's `git` or `gh`. When the daemon starts
//! in its start form with exactly such a `PATH`, [`apply_at_start`] asks the
//! user's interactive login shell for the `PATH` it sets up (`$SHELL -ilc`,
//! under a hard deadline) and writes the merged result into the process
//! environment, before the tokio runtime or any other thread exists. Every site
//! in the daemon that looks a program up by name reads the process `PATH`, so
//! this one write covers all of them.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How long the login shell may take before it is killed and the inherited
/// `PATH` kept.
const LOGIN_PATH_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the probe checks whether its shell has exited.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// The line before the probe's `PATH` in the login shell's output.
const SENTINEL_BEGIN: &str = "__RT_LOGIN_PATH_BEGIN__";

/// The line after the probe's `PATH` in the login shell's output.
const SENTINEL_END: &str = "__RT_LOGIN_PATH_END__";

/// How much of the login shell's output a failure detail keeps.
const DETAIL_LIMIT: usize = 300;

/// The directories launchd's minimal `PATH` is made of.
const LAUNCHD_DIRS: [&str; 4] = ["/usr/bin", "/bin", "/usr/sbin", "/sbin"];

/// What [`apply_at_start`] found and did, kept for [`log_outcome`] to report
/// once tracing exists.
#[derive(Debug)]
enum Outcome {
    /// The merged `PATH` was written to the process environment, with that
    /// many entries.
    Applied { entries: usize },
    /// Nothing was probed, and the inherited `PATH` stands.
    Skipped(SkipReason),
    /// The probe failed; the inherited `PATH` stands and `String` says why.
    Failed(String),
}

/// Why [`apply_at_start`] did not probe.
#[expect(
    clippy::enum_variant_names,
    reason = "the skip reasons read as one family: each names what the daemon is not"
)]
#[derive(Debug)]
enum SkipReason {
    /// Not macOS.
    NotMacos,
    /// A subcommand, `--detach`, or an argument list the start form refuses.
    NotStartForm,
    /// The inherited `PATH` already has more than launchd's directories.
    NotMinimal,
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let line = match self {
            Self::NotMacos => "not macOS",
            Self::NotStartForm => "not the daemon's start form",
            Self::NotMinimal => "the inherited PATH is not launchd's minimal one",
        };
        f.write_str(line)
    }
}

/// Why the login shell could not give a `PATH`.
#[derive(Debug)]
enum ProbeFailure {
    /// The shell could not be started.
    Spawn(std::io::Error),
    /// The shell exited with a failure status.
    Exit(std::process::ExitStatus),
    /// The output held no `PATH` between the sentinels; `String` is what it
    /// printed instead, shortened to [`DETAIL_LIMIT`] characters.
    NoPath(String),
    /// The shell, or the output it left open, outlived the deadline.
    TimedOut,
}

impl std::fmt::Display for ProbeFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(err) => write!(f, "could not start the login shell: {err}"),
            Self::Exit(status) => write!(f, "the login shell exited with {status}"),
            Self::NoPath(output) => write!(
                f,
                "the login shell printed no PATH between the sentinels: {output:?}"
            ),
            Self::TimedOut => f.write_str("the login shell did not answer before the deadline"),
        }
    }
}

/// What [`apply_at_start`] found and did.
static OUTCOME: OnceLock<Outcome> = OnceLock::new();

/// Give the process the login shell's `PATH` when launchd started it with a
/// minimal one.
///
/// Does nothing unless this is macOS, the process was started in the daemon's
/// start form (`detach::launch_form` accepts it: no arguments, or
/// `--start-failure-file <path>`), and the inherited `PATH` is launchd's
/// minimal one. `--detach` hands over to a cached copy that starts in the
/// start form and probes itself, and a subcommand or a refused argument list
/// exits before it needs a program by name.
pub(crate) fn apply_at_start() {
    let _ = OUTCOME.set(decide_at_start());
}

/// What to do at start, recorded in [`OUTCOME`] for [`log_outcome`].
fn decide_at_start() -> Outcome {
    if !cfg!(target_os = "macos") {
        return Outcome::Skipped(SkipReason::NotMacos);
    }
    if !is_start_form(std::env::args_os().skip(1)) {
        return Outcome::Skipped(SkipReason::NotStartForm);
    }
    let inherited = std::env::var_os("PATH");
    if !is_launchd_minimal(inherited.as_deref()) {
        return Outcome::Skipped(SkipReason::NotMinimal);
    }
    let login = match probe_login_path(&login_shell(), LOGIN_PATH_TIMEOUT) {
        Ok(login) => login,
        Err(err) => return Outcome::Failed(err.to_string()),
    };
    let Some(merged) = merge_paths(login.as_os_str(), inherited.as_deref()) else {
        return Outcome::Failed("the login shell's PATH could not be joined".to_owned());
    };
    let entries = std::env::split_paths(&merged).count();
    if entries == 0 {
        return Outcome::Failed("the login shell gave an empty PATH".to_owned());
    }
    // SAFETY: this runs in `main` before the tokio runtime is built and before
    // any other thread this process creates exists, and the probe which joined
    // its own reader thread has already returned, so no other thread can be
    // reading the environment concurrently.
    unsafe {
        std::env::set_var("PATH", &merged);
    }
    Outcome::Applied { entries }
}

/// Log what [`apply_at_start`] did, once tracing is up.
pub(crate) fn log_outcome() {
    match OUTCOME.get() {
        Some(Outcome::Applied { entries }) => {
            tracing::info!(entries = *entries, "login shell PATH applied");
        }
        Some(Outcome::Failed(detail)) => {
            tracing::warn!(
                reason = %detail,
                "login shell PATH probe failed; keeping the inherited PATH"
            );
        }
        Some(Outcome::Skipped(reason)) => {
            tracing::debug!(reason = %reason, "login shell PATH not applied");
        }
        None => {}
    }
}

/// Whether `args` (the process's arguments without the program name) ask for
/// the daemon's start form, the only form whose process needs a program by
/// name: `detach::launch_form` refusing them means a subcommand claims them.
fn is_start_form(args: impl IntoIterator<Item = OsString>) -> bool {
    matches!(
        crate::detach::launch_form(args),
        Ok(crate::detach::LaunchRequest {
            form: crate::detach::Launch::Start,
            ..
        })
    )
}

/// Whether `path` is unset, empty, or made only of the directories launchd
/// gives a process it starts.
fn is_launchd_minimal(path: Option<&OsStr>) -> bool {
    let Some(path) = path else {
        return true;
    };
    if path.is_empty() {
        return true;
    }
    std::env::split_paths(path).all(|entry| {
        LAUNCHD_DIRS
            .iter()
            .any(|dir| Path::new(dir) == entry.as_path())
    })
}

/// The one non-empty line between the probe's sentinels, or `None` when there
/// is no end sentinel after the begin one, no such line, or more than one.
///
/// Lines before the begin sentinel and after the end sentinel are ignored, so
/// whatever a login rc file prints cannot be mistaken for the probe's answer.
/// The comparison is over bytes, so a non-UTF-8 banner costs nothing.
fn path_from_probe_output(output: &[u8]) -> Option<Vec<u8>> {
    let mut lines = output.split(|byte| *byte == b'\n');
    lines.find(|line| line.trim_ascii_end() == SENTINEL_BEGIN.as_bytes())?;
    let mut found: Option<Vec<u8>> = None;
    for line in lines {
        let line = line.trim_ascii_end();
        if line == SENTINEL_END.as_bytes() {
            return found;
        }
        if line.is_empty() {
            continue;
        }
        if found.is_some() {
            return None;
        }
        found = Some(line.to_vec());
    }
    None
}

/// The probe's `PATH` bytes as an environment value.
fn path_os_string(path: &[u8]) -> OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt as _;
        OsString::from_vec(path.to_vec())
    }
    #[cfg(not(unix))]
    {
        OsString::from(String::from_utf8_lossy(path).into_owned())
    }
}

/// The login `PATH`'s entries in order, then each of `inherited`'s entries not
/// already present, as one joined string.
///
/// An empty entry is dropped rather than kept. `None` means
/// `std::env::join_paths` refused the merged list, which `split_paths` cannot
/// produce.
fn merge_paths(login: &OsStr, inherited: Option<&OsStr>) -> Option<OsString> {
    let mut accepted: Vec<PathBuf> = Vec::new();
    for entry in std::iter::once(login)
        .chain(inherited)
        .flat_map(std::env::split_paths)
    {
        if entry.as_os_str().is_empty() || accepted.contains(&entry) {
            continue;
        }
        accepted.push(entry);
    }
    std::env::join_paths(&accepted).ok()
}

/// The shell to ask: `$SHELL`, or zsh when it is unset or empty.
fn login_shell() -> PathBuf {
    match std::env::var_os("SHELL") {
        Some(shell) if !shell.is_empty() => PathBuf::from(shell),
        _ => PathBuf::from("/bin/zsh"),
    }
}

/// The script the probe shell runs: its `PATH` between two sentinel lines, so
/// anything a login rc file prints is ignored.
///
/// The newline before the begin sentinel keeps an rc file's unterminated title
/// line from being glued to it. `printenv` rather than `env` so a multi-line
/// variable or an exported function can never be read as the `PATH`.
fn probe_script() -> String {
    format!("printf '\\n%s\\n' {SENTINEL_BEGIN}; printenv PATH; printf '%s\\n' {SENTINEL_END}")
}

/// Run `shell` as an interactive login shell and read the `PATH` it sets up.
///
/// The shell gets no stdin and its stderr is discarded, and it leads a process
/// group of its own. A reader thread sends the output as soon as it reads the
/// end sentinel, so nothing here waits past `timeout` for an answer. A shell
/// that exits on its own after answering is reaped and left unsignalled: with
/// no terminal it has no job control, so the jobs its rc files started share
/// its group and are the user's to keep. One that hangs after answering has its
/// group killed.
fn probe_login_path(shell: &Path, timeout: Duration) -> Result<OsString, ProbeFailure> {
    let mut command = std::process::Command::new(shell);
    command
        .arg("-ilc")
        .arg(probe_script())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    let mut child = command.spawn().map_err(ProbeFailure::Spawn)?;
    let pid = child.id();
    let Some(stdout) = child.stdout.take() else {
        let _ = kill_probe_shell(&mut child, pid);
        return Err(ProbeFailure::Spawn(std::io::Error::other(
            "the probe shell's stdout was not piped",
        )));
    };
    let (sender, receiver) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        use std::io::BufRead as _;
        let mut collected: Vec<u8> = Vec::new();
        let mut reader = std::io::BufReader::new(stdout);
        let mut line = Vec::new();
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            collected.extend_from_slice(&line);
            if line.trim_ascii_end() == SENTINEL_END.as_bytes() {
                break;
            }
        }
        let _ = sender.send(collected);
    });
    let deadline = Instant::now() + timeout;
    let mut answer = None;
    while answer.is_none() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match receiver.recv_timeout(remaining.min(POLL_INTERVAL)) {
            Ok(output) => answer = Some(output),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            // The reader ended without sending, so no answer can follow.
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let Some(output) = answer else {
        let _ = kill_probe_shell(&mut child, pid);
        return Err(ProbeFailure::TimedOut);
    };
    // The reader sent its answer, so this returns at once rather than waiting
    // on a pipe something else may still hold.
    let _ = reader.join();
    // Give the shell what is left of the deadline to exit on its own. When it
    // does, nothing is signalled: with no terminal it has no job control, so
    // every job an rc file started shares its group and is the user's to keep.
    let mut exited = shell_exited(pid);
    while !exited && Instant::now() < deadline {
        std::thread::sleep(POLL_INTERVAL);
        exited = shell_exited(pid);
    }
    let status = if exited {
        child.wait().map_err(ProbeFailure::Spawn)?
    } else {
        // It hung after answering, so its group goes.
        kill_probe_shell(&mut child, pid)?
    };
    // A complete answer stands whatever the shell did afterwards, a failing
    // logout script included; an incomplete one with a failing exit code is the
    // shell's refusal.
    if let Some(path) = path_from_probe_output(&output) {
        return Ok(path_os_string(&path));
    }
    if exited && status.code().is_some_and(|code| code != 0) {
        return Err(ProbeFailure::Exit(status));
    }
    let detail = String::from_utf8_lossy(&output);
    Err(ProbeFailure::NoPath(shorten(&detail)))
}

/// Whether the shell has exited, without reaping it: its unreaped pid is what
/// keeps its process group's id reserved for [`kill_process_group`].
#[cfg(unix)]
fn shell_exited(pid: u32) -> bool {
    // SAFETY: `waitid` writes only into `info`, and a zeroed `siginfo_t` is what
    // POSIX calls "no information about a child yet". `WNOWAIT` leaves the
    // shell reapable by `Child::wait` afterwards.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is a live, writable `siginfo_t` for the call's duration.
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            pid,
            &raw mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    result == 0 && info.si_signo != 0
}

/// The probe never runs off macOS, where the shell leads a group of its own;
/// elsewhere there is no group to inspect.
#[cfg(not(unix))]
fn shell_exited(_pid: u32) -> bool {
    true
}

/// Kill the probe shell with every process of its group, then reap it and
/// return the status it exited with.
///
/// # Safety invariant
///
/// The group may be signalled only while the shell is unreaped, which is what
/// keeps its pid, and so the group's id, from being handed to another process
/// group in between.
fn kill_probe_shell(
    child: &mut std::process::Child,
    pid: u32,
) -> Result<std::process::ExitStatus, ProbeFailure> {
    #[cfg(unix)]
    kill_process_group(pid);
    #[cfg(not(unix))]
    {
        // No process group to signal here; the probe never runs on these
        // targets, so the shell itself is all there is.
        let _ = pid;
        let _ = child.kill();
    }
    child.wait().map_err(ProbeFailure::Spawn)
}

/// Signal every process of the group the probe shell leads.
#[cfg(unix)]
fn kill_process_group(pid: u32) {
    let Ok(pid) = i32::try_from(pid) else {
        return;
    };
    // SAFETY: `kill` with a negated pid signals that whole process group, and
    // this pid is the shell this probe spawned as its own group's leader. The
    // caller signals only while that shell is unreaped, so the id still names
    // its group and cannot have been handed to another one, and it cannot name
    // the daemon's own group.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
}

/// `text`, cut to [`DETAIL_LIMIT`] characters so a noisy login rc file cannot
/// fill a log line.
fn shorten(text: &str) -> String {
    if text.chars().count() <= DETAIL_LIMIT {
        return text.to_owned();
    }
    let mut short: String = text.chars().take(DETAIL_LIMIT).collect();
    short.push('…');
    short
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests assert preconditions with expect")]
mod tests {
    #[cfg(unix)]
    use super::{Duration, Instant, PathBuf, ProbeFailure, probe_login_path};
    use super::{
        OsStr, OsString, is_launchd_minimal, is_start_form, merge_paths, path_from_probe_output,
    };

    /// The entries of `path`, in order, as lossy strings.
    fn entries(path: &OsStr) -> Vec<String> {
        std::env::split_paths(path)
            .map(|entry| entry.to_string_lossy().into_owned())
            .collect()
    }

    /// `list` joined the way this platform joins a `PATH`, so a test reads the
    /// same on Windows as on macOS.
    fn joined(list: &[&str]) -> OsString {
        std::env::join_paths(list).expect("test entries join")
    }

    /// `list` as the argument list a process was started with.
    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn is_start_form_probes_only_the_daemon_start_forms() {
        assert!(is_start_form(args(&[])));
        assert!(is_start_form(args(&["--start-failure-file", "/tmp/x"])));
        assert!(!is_start_form(args(&["--detach"])));
        assert!(!is_start_form(args(&[
            "apply-update",
            "--from",
            "a",
            "--to",
            "b",
        ])));
        assert!(!is_start_form(args(&["sessions"])));
    }

    #[test]
    fn is_launchd_minimal_accepts_only_launchd_directories() {
        assert!(is_launchd_minimal(None));
        assert!(is_launchd_minimal(Some(OsStr::new(""))));
        assert!(is_launchd_minimal(Some(&joined(&[
            "/usr/bin",
            "/bin",
            "/usr/sbin",
            "/sbin",
        ]))));
        assert!(is_launchd_minimal(Some(&joined(&["/bin", "/usr/bin"]))));
        assert!(!is_launchd_minimal(Some(&joined(&[
            "/usr/bin",
            "/bin",
            "/usr/sbin",
            "/sbin",
            "/opt/homebrew/bin",
        ]))));
        assert!(!is_launchd_minimal(Some(&joined(&[
            "/usr/local/bin",
            "/usr/bin",
        ]))));
        assert!(!is_launchd_minimal(Some(OsStr::new("/Users/x/.local/bin"))));
    }

    #[test]
    fn path_from_probe_output_ignores_everything_outside_the_sentinels() {
        let output = b"noise \xff\nPATH=/wrong\n__RT_LOGIN_PATH_BEGIN__\n/a:/b\n__RT_LOGIN_PATH_END__\nPATH=/after\n";
        assert_eq!(
            path_from_probe_output(output).as_deref(),
            Some(&b"/a:/b"[..])
        );
    }

    #[test]
    fn path_from_probe_output_without_a_begin_sentinel_is_none() {
        assert_eq!(
            path_from_probe_output(b"/a:/b\n__RT_LOGIN_PATH_END__\n"),
            None
        );
    }

    #[test]
    fn path_from_probe_output_with_nothing_between_the_sentinels_is_none() {
        assert_eq!(
            path_from_probe_output(b"__RT_LOGIN_PATH_BEGIN__\n\n__RT_LOGIN_PATH_END__\n"),
            None
        );
    }

    #[test]
    fn path_from_probe_output_with_more_than_one_line_between_is_none() {
        assert_eq!(
            path_from_probe_output(b"__RT_LOGIN_PATH_BEGIN__\n/a\n/b\n__RT_LOGIN_PATH_END__\n"),
            None
        );
    }

    #[test]
    fn path_from_probe_output_ignores_noise_glued_to_the_begin_sentinel() {
        assert_eq!(
            path_from_probe_output(
                b"noise-without-newline__RT_LOGIN_PATH_BEGIN__\n/a:/b\n__RT_LOGIN_PATH_END__\n"
            ),
            None
        );
    }

    #[test]
    fn merge_paths_puts_the_login_entries_first_then_the_missing_inherited_ones() {
        let merged = merge_paths(
            &joined(&["/opt/homebrew/bin", "/usr/bin"]),
            Some(&joined(&["/usr/bin", "/bin"])),
        )
        .expect("entries join");
        assert_eq!(entries(&merged), ["/opt/homebrew/bin", "/usr/bin", "/bin"]);
    }

    #[test]
    fn merge_paths_without_an_inherited_path_is_the_login_path() {
        let merged =
            merge_paths(&joined(&["/opt/homebrew/bin", "/usr/bin"]), None).expect("entries join");
        assert_eq!(entries(&merged), ["/opt/homebrew/bin", "/usr/bin"]);
    }

    #[test]
    fn merge_paths_keeps_a_duplicate_login_entry_once() {
        let merged =
            merge_paths(&joined(&["/bin", "/bin", "/usr/bin"]), None).expect("entries join");
        assert_eq!(entries(&merged), ["/bin", "/usr/bin"]);
    }

    /// A scratch directory under the OS temp root, removed when it is dropped.
    /// The daemon's other test modules hand-roll this rather than pull in
    /// `tempfile` for tests.
    #[cfg(unix)]
    struct Scratch {
        path: PathBuf,
    }

    #[cfg(unix)]
    impl Scratch {
        fn new(label: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("rt-login-path-{label}-{}", std::process::id()));
            std::fs::create_dir_all(&path).expect("scratch dir");
            Self { path }
        }

        /// Write `body` as an executable shell script and return its path.
        fn script(&self, label: &str, body: &str) -> PathBuf {
            use std::os::unix::fs::PermissionsExt as _;
            let path = self.path.join(label);
            std::fs::write(&path, body).expect("write script");
            let mut permissions = std::fs::metadata(&path).expect("script mode").permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&path, permissions).expect("make script executable");
            path
        }
    }

    #[cfg(unix)]
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// A deadline no probe test expects to reach: these assert on what the probe
    /// reports, never on how long it took.
    #[cfg(unix)]
    const HANG_GUARD: Duration = Duration::from_secs(60);

    /// Read the pid a probe script wrote to `path`.
    #[cfg(unix)]
    fn read_pid(path: &std::path::Path) -> u32 {
        std::fs::read_to_string(path)
            .expect("pid file")
            .trim()
            .parse()
            .expect("pid")
    }

    /// Whether `pid` is gone: `kill(pid, 0)` reports `ESRCH` only for a process
    /// that no longer exists.
    #[cfg(unix)]
    fn is_gone(pid: u32) -> bool {
        let pid = i32::try_from(pid).expect("pid fits an i32");
        // SAFETY: signal 0 runs only the existence and permission check.
        let result = unsafe { libc::kill(pid, 0) };
        result != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    }

    /// Wait under the hang guard for `pid` to disappear.
    #[cfg(unix)]
    fn wait_until_gone(pid: u32) {
        let deadline = Instant::now() + HANG_GUARD;
        while !is_gone(pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(is_gone(pid), "process {pid} outlived the probe");
    }

    #[cfg(unix)]
    #[test]
    fn probe_reads_the_path_between_the_sentinels() {
        let scratch = Scratch::new("ok");
        let shell = scratch.script(
            "fake-shell",
            "#!/bin/sh\n\
             printf '%s\\n' noise\n\
             printf '%s\\n' __RT_LOGIN_PATH_BEGIN__\n\
             printf '%s\\n' '/fake/bin:/usr/bin'\n\
             printf '%s\\n' __RT_LOGIN_PATH_END__\n",
        );
        let found = probe_login_path(&shell, HANG_GUARD).expect("probe succeeds");
        assert_eq!(found, OsString::from("/fake/bin:/usr/bin"));
    }

    #[cfg(unix)]
    #[test]
    fn probe_reports_a_non_zero_exit() {
        let scratch = Scratch::new("exit");
        let shell = scratch.script("fake-shell", "#!/bin/sh\nexit 3\n");
        let failure = probe_login_path(&shell, HANG_GUARD).expect_err("probe fails");
        assert!(
            matches!(failure, ProbeFailure::Exit(_)),
            "unexpected failure: {failure:?}"
        );
    }

    /// Kill `pid`, so a test leaves no process of its own behind.
    #[cfg(unix)]
    fn kill_pid(pid: u32) {
        let pid = i32::try_from(pid).expect("pid fits an i32");
        // SAFETY: signal 9 to a process this test started.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }

    #[cfg(unix)]
    #[test]
    fn probe_kills_a_background_child_that_holds_the_output() {
        let scratch = Scratch::new("held");
        let pidfile = scratch.path.join("child.pid");
        let pending = scratch.path.join("child.pid.tmp");
        let shell = scratch.script(
            "fake-shell",
            &format!(
                "#!/bin/sh\n\
                 sleep 30 &\n\
                 printf '%s\\n' \"$!\" > '{pending}'\n\
                 mv '{pending}' '{pidfile}'\n\
                 exit 0\n",
                pending = pending.display(),
                pidfile = pidfile.display()
            ),
        );
        let failure =
            probe_login_path(&shell, Duration::from_secs(1)).expect_err("probe times out");
        assert!(
            matches!(failure, ProbeFailure::TimedOut),
            "unexpected failure: {failure:?}"
        );
        // No pid file means the group was killed before the child was recorded,
        // which is the same answer.
        if pidfile.exists() {
            wait_until_gone(read_pid(&pidfile));
        }
    }

    #[cfg(unix)]
    #[test]
    fn probe_answers_although_a_background_child_holds_the_output() {
        let scratch = Scratch::new("answered");
        let pidfile = scratch.path.join("child.pid");
        let pending = scratch.path.join("child.pid.tmp");
        let shell = scratch.script(
            "fake-shell",
            &format!(
                "#!/bin/sh\n\
                 sleep 30 &\n\
                 printf '%s\\n' \"$!\" > '{pending}'\n\
                 mv '{pending}' '{pidfile}'\n\
                 printf '\\n%s\\n' __RT_LOGIN_PATH_BEGIN__\n\
                 printf '%s\\n' '/fake/bin:/usr/bin'\n\
                 printf '%s\\n' __RT_LOGIN_PATH_END__\n\
                 exit 0\n",
                pending = pending.display(),
                pidfile = pidfile.display()
            ),
        );
        let found = probe_login_path(&shell, HANG_GUARD).expect("probe answers");
        assert_eq!(found, OsString::from("/fake/bin:/usr/bin"));
        let pid = read_pid(&pidfile);
        assert!(
            !is_gone(pid),
            "the probe killed a job the rc file left running"
        );
        kill_pid(pid);
    }

    #[cfg(unix)]
    #[test]
    fn probe_kills_a_shell_that_never_exits() {
        let scratch = Scratch::new("hang");
        let pidfile = scratch.path.join("shell.pid");
        let pending = scratch.path.join("shell.pid.tmp");
        let shell = scratch.script(
            "fake-shell",
            &format!(
                "#!/bin/sh\n\
                 printf '%s\\n' \"$$\" > '{pending}'\n\
                 mv '{pending}' '{pidfile}'\n\
                 sleep 30\n",
                pending = pending.display(),
                pidfile = pidfile.display()
            ),
        );
        let failure =
            probe_login_path(&shell, Duration::from_secs(1)).expect_err("probe times out");
        assert!(
            matches!(failure, ProbeFailure::TimedOut),
            "unexpected failure: {failure:?}"
        );
        if pidfile.exists() {
            wait_until_gone(read_pid(&pidfile));
        }
    }
}
