//! The daemon's start forms ([`launch_form`]) and its `--detach` relaunch
//! path.
//!
//! The "start on login" setting writes an HKCU `Run` entry pointing at the
//! installed `rustling-tulipd.exe`, so at login Windows starts that file
//! directly — and a running executable cannot be replaced by the next
//! installer or rebuild. With `--detach` that launch becomes a short-lived
//! launcher instead: it copies itself into the content-addressed binaries
//! cache and spawns the cached copy, exactly as the clients do, then exits.
//! With no arguments the daemon runs in place; any other argument list that
//! no subcommand took is refused with a usage line.

use anyhow::Context as _;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use tracing::info;
use tracing_subscriber::EnvFilter;

/// The one line that refuses an argument list naming no subcommand and asking
/// for no start form, printed verbatim before anything is created.
const USAGE: &str = "usage: rustling-tulipd [--start-failure-file <path>] | --detach | --version | <command> ..., where <command> is apply-update, bridge, kill, pair, read, screen, send, sessions, shutdown, spawn, wait or watch";

/// How the daemon was asked to start, once every subcommand has declined
/// the arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Launch {
    /// Run the daemon here.
    Start,
    /// `--detach`: hand over to a cached copy and exit.
    Detach,
}

/// A start the arguments ask for: the form, and the file a failure to start
/// is written to when the caller named one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchRequest {
    /// Run the daemon here, or hand over to a cached copy.
    pub form: Launch,
    /// The file the caller named with `--start-failure-file`.
    pub start_failure: Option<PathBuf>,
}

/// The launch `args` (without the program name) ask for, or the one stderr
/// line that refuses them: no arguments, `--detach` alone, or a start with one
/// `--start-failure-file <path>` in any position.
///
/// # Errors
///
/// Every other argument list — an unknown argument, `--start-failure-file`
/// with no value after it or given twice, `--detach` given twice, or
/// `--start-failure-file` beside `--detach` (a launcher hands over to a cached
/// child that never sees the file): the usage line naming the offending
/// argument, which the caller prints before `paths::Dirs::ensure` so a typo
/// touches nothing.
pub fn launch_form(args: impl IntoIterator<Item = OsString>) -> Result<LaunchRequest, String> {
    let mut args = args.into_iter();
    let mut form = Launch::Start;
    let mut start_failure = None;
    while let Some(arg) = args.next() {
        if arg == "--detach" {
            if form == Launch::Detach {
                return Err(refusal(&arg));
            }
            form = Launch::Detach;
        } else if arg == "--start-failure-file" {
            if start_failure.is_some() {
                return Err(refusal(&arg));
            }
            let Some(path) = args.next() else {
                return Err(refusal(&arg));
            };
            start_failure = Some(PathBuf::from(path));
        } else {
            return Err(refusal(&arg));
        }
    }
    if form == Launch::Detach && start_failure.is_some() {
        return Err(refusal(OsStr::new("--start-failure-file")));
    }
    Ok(LaunchRequest {
        form,
        start_failure,
    })
}

/// Write `err`'s whole chain (`{err:#}`) to the start-failure file `path`, a
/// daemon that failed to start tells its client what went wrong with, through
/// a sibling temp file and a rename so a reader never sees half a message.
pub fn write_start_failure(path: &Path, err: &anyhow::Error) {
    let written = create_parent(path)
        .and_then(|()| crate::atomic_file::write_atomic(path, format!("{err:#}").as_bytes()));
    if let Err(write_err) = written {
        // No subscriber and no stderr exist this early, so nothing else can
        // record this.
        tracing::error!(?write_err, path = %path.display(), "could not write the start-failure file");
    }
}

/// Record `err` as the start failure in `start_failure`, unless the daemon
/// already published its handshake: an error after that is a daemon that
/// started and served, then failed, which is no start failure.
pub fn record_start_failure(start_failure: Option<&Path>, published: bool, err: &anyhow::Error) {
    if published {
        return;
    }
    if let Some(path) = start_failure {
        write_start_failure(path, err);
    }
}

/// Create `path`'s parent folder, when it has one.
fn create_parent(path: &Path) -> anyhow::Result<()> {
    match path.parent() {
        Some(parent) => std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display())),
        None => Ok(()),
    }
}

/// The usage line naming `arg` as the argument no start form or subcommand
/// took.
fn refusal(arg: &OsStr) -> String {
    format!(
        "rustling-tulipd: unknown argument \"{}\"; {USAGE}",
        arg.to_string_lossy()
    )
}

/// Cache this executable and start the cached copy in its place, then return
/// so the launcher can exit. The cached child outlives this process.
pub fn relaunch_from_cache(dirs: &crate::paths::Dirs) -> anyhow::Result<()> {
    init_logging(dirs);
    match relaunch(dirs) {
        Ok(()) => Ok(()),
        Err(err) => {
            tracing::error!(?err, "--detach relaunch failed");
            Err(err)
        }
    }
}

fn relaunch(dirs: &crate::paths::Dirs) -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("locating the running daemon exe")?;
    let template_dir = exe
        .parent()
        .map(Path::to_path_buf)
        .context("the running daemon exe has no parent directory")?;
    // The name hint must match what `daemon_client` computes for this same
    // template (`rustling-tulipd-<sha16>.exe`), or a client would read the
    // autostarted daemon as a foreign build and retire it.
    let cached = crate::binary_cache::ensure_cached(&exe, &dirs.binaries_dir, "rustling-tulipd")
        .context("caching the daemon binary")?;
    let templates_env_already_set = std::env::var_os("RUSTLING_TULIP_BIN_TEMPLATES").is_some();

    let mut cmd = child_command(&cached, &template_dir, templates_env_already_set);
    let child = cmd.spawn().context("spawning the cached daemon")?;
    info!(
        exe = %exe.display(),
        cached = %cached.display(),
        template_dir = %template_dir.display(),
        pid = child.id(),
        "--detach: relaunched the daemon from the binary cache"
    );
    // The child outlives us; dropping the handle leaves it running.
    drop(child);
    Ok(())
}

/// The command that starts the cached daemon: no arguments, no inherited
/// stdio, and — on Windows — no console window and no console-group tie to
/// this short-lived launcher. `RUSTLING_TULIP_BIN_TEMPLATES` tells the child
/// which install dir it came from so it can find its sibling `rt-tracer`; when
/// the launcher already has that variable, the child inherits it instead.
pub fn child_command(
    cached: &Path,
    template_dir: &Path,
    templates_env_already_set: bool,
) -> Command {
    let mut cmd = Command::new(cached);
    if !templates_env_already_set {
        cmd.env("RUSTLING_TULIP_BIN_TEMPLATES", template_dir);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // Same constants `daemon_client::supervisor::spawn_daemon` uses.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        cmd.creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS);
    }
    cmd
}

/// Log the launcher's short life to `<config>/logs/autostart.log`. Truncated
/// on every run, no ANSI (the file is read by hand), and deliberately not
/// touching `daemon.log` — the cached child rotates that one. Falls back to
/// stderr when the file can't be opened, the way `main::init_tracing` does.
fn init_logging(dirs: &crate::paths::Dirs) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let log_dir = dirs.config.join("logs");
    let log_path = log_dir.join("autostart.log");
    let dir_err = std::fs::create_dir_all(&log_dir).err();
    let opened = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&log_path);

    match opened {
        Ok(file) => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(Mutex::new(file))
                .with_ansi(false)
                .with_target(true)
                .compact()
                .init();
            info!(log_file = %log_path.display(), "autostart logging to file");
            if let Some(err) = dir_err {
                tracing::warn!(?err, dir = %log_dir.display(), "autostart log dir create failed (continuing)");
            }
        }
        Err(err) => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_target(true)
                .compact()
                .init();
            tracing::warn!(?err, path = %log_path.display(), "autostart could not open log file; using stderr");
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::{
        Launch, LaunchRequest, child_command, launch_form, record_start_failure,
        write_start_failure,
    };
    use std::ffi::{OsStr, OsString};
    use std::path::{Path, PathBuf};

    const TEMPLATE_ENV: &str = "RUSTLING_TULIP_BIN_TEMPLATES";

    /// A fresh folder under the repo's `.tmp/`, removed on drop.
    struct TestDir(PathBuf);

    impl TestDir {
        fn new(tag: &str) -> Self {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join(".tmp")
                .join(format!("detach-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create the test dir");
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn args(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    fn template_env(cmd: &std::process::Command) -> Option<OsString> {
        cmd.get_envs()
            .find(|(key, _)| *key == OsStr::new(TEMPLATE_ENV))
            .and_then(|(_, value)| value.map(OsString::from))
    }

    /// The refusal line for `arg`, spelled out so a change to the wording
    /// fails the test.
    fn refusal(arg: &str) -> String {
        format!(
            "rustling-tulipd: unknown argument \"{arg}\"; usage: rustling-tulipd [--start-failure-file <path>] | --detach | --version | <command> ..., where <command> is apply-update, bridge, kill, pair, read, screen, send, sessions, shutdown, spawn, wait or watch"
        )
    }

    /// The start `launch_form` returns for `form` and the file `failure`.
    fn request(form: Launch, failure: Option<&str>) -> LaunchRequest {
        LaunchRequest {
            form,
            start_failure: failure.map(PathBuf::from),
        }
    }

    #[test]
    fn only_a_plain_start_or_detach_alone_is_accepted() {
        assert_eq!(launch_form(args(&[])), Ok(request(Launch::Start, None)));
        assert_eq!(
            launch_form(args(&["--detach"])),
            Ok(request(Launch::Detach, None))
        );
        for (list, offending) in [
            (&["frobnicate"][..], "frobnicate"),
            (&["sesions"][..], "sesions"),
            (&["--foo", "--detach"][..], "--foo"),
            (&["--detach", "extra"][..], "extra"),
            (&["--detached"][..], "--detached"),
            (&["--detach", "--detach"][..], "--detach"),
        ] {
            assert_eq!(launch_form(args(list)), Err(refusal(offending)), "{list:?}");
        }
    }

    #[test]
    fn the_start_failure_option_is_accepted_only_with_the_start_form() {
        assert_eq!(
            launch_form(args(&["--start-failure-file", "p"])),
            Ok(request(Launch::Start, Some("p")))
        );
        for list in [
            &["--detach", "--start-failure-file", "p"][..],
            &["--start-failure-file", "p", "--detach"][..],
        ] {
            assert_eq!(
                launch_form(args(list)),
                Err(refusal("--start-failure-file")),
                "{list:?}"
            );
        }
    }

    #[test]
    fn a_start_failure_option_without_a_value_is_refused() {
        assert_eq!(
            launch_form(args(&["--start-failure-file"])),
            Err(refusal("--start-failure-file"))
        );
    }

    #[test]
    fn a_repeated_start_failure_option_is_refused() {
        assert_eq!(
            launch_form(args(&[
                "--start-failure-file",
                "p",
                "--start-failure-file",
                "q"
            ])),
            Err(refusal("--start-failure-file"))
        );
    }

    #[test]
    fn write_start_failure_writes_the_error_chain() {
        let dir = TestDir::new("write-start-failure");
        // The `logs` folder does not exist yet, so `write_start_failure` has
        // to create it.
        let path = dir.0.join("logs").join("start-failure.txt");
        let err = anyhow::anyhow!("inner").context("outer");

        write_start_failure(&path, &err);

        let text = std::fs::read_to_string(&path).expect("the failure file was written");
        assert_eq!(text, "outer: inner");
    }

    #[test]
    fn a_start_failure_is_written_only_before_the_handshake() {
        let dir = TestDir::new("record-start-failure");
        let path = dir.0.join("start-failure.txt");
        let err = anyhow::anyhow!("inner").context("outer");

        record_start_failure(Some(&path), false, &err);
        let text = std::fs::read_to_string(&path).expect("a start failure is written");
        assert_eq!(text, "outer: inner");
        std::fs::remove_file(&path).expect("clear the first file");

        record_start_failure(Some(&path), true, &err);
        assert!(
            !path.exists(),
            "an error after the handshake is no start failure"
        );

        record_start_failure(None, false, &err);
        assert!(!path.exists(), "no file was named");
    }

    #[test]
    fn child_command_sets_the_template_dir_when_unset() {
        let cached = Path::new(r"C:\cache\rustling-tulipd-0123456789abcdef.exe");
        let template_dir = Path::new(r"C:\Program Files\rustling-tulip");
        let cmd = child_command(cached, template_dir, false);

        assert_eq!(cmd.get_program(), cached.as_os_str());
        assert_eq!(cmd.get_args().count(), 0);
        assert_eq!(
            template_env(&cmd).as_deref(),
            Some(template_dir.as_os_str())
        );
    }

    #[test]
    fn child_command_leaves_an_inherited_template_dir_alone() {
        let cached = Path::new(r"C:\cache\rustling-tulipd-0123456789abcdef.exe");
        let template_dir = Path::new(r"C:\Program Files\rustling-tulip");
        let cmd = child_command(cached, template_dir, true);

        assert_eq!(template_env(&cmd), None);
    }
}
