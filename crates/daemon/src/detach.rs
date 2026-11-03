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
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use tracing::info;
use tracing_subscriber::EnvFilter;

/// The one line that refuses an argument list naming no subcommand and asking
/// for no start form, printed verbatim before anything is created.
const USAGE: &str = "usage: rustling-tulipd [--detach | --version | <command> ...], where <command> is apply-update, bridge, kill, pair, read, screen, send, sessions, shutdown, spawn, wait or watch";

/// How the daemon was asked to start, once every subcommand has declined
/// the arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Launch {
    /// No arguments: run the daemon here.
    Start,
    /// Exactly `--detach`: hand over to a cached copy and exit.
    Detach,
}

/// The launch `args` (without the program name) ask for, or the one stderr
/// line that refuses them: only `[]` and `["--detach"]` start a daemon.
///
/// # Errors
///
/// Every other argument list, including `--detach` beside anything else: the
/// usage line naming the offending argument, which the caller prints before
/// `paths::Dirs::ensure` so a typo touches nothing.
pub fn launch_form(args: impl IntoIterator<Item = OsString>) -> Result<Launch, String> {
    let mut args = args.into_iter();
    match args.next() {
        None => Ok(Launch::Start),
        Some(first) if first == "--detach" => match args.next() {
            None => Ok(Launch::Detach),
            Some(extra) => Err(refusal(&extra)),
        },
        Some(first) => Err(refusal(&first)),
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
mod tests {
    use super::{Launch, child_command, launch_form};
    use std::ffi::{OsStr, OsString};
    use std::path::Path;

    const TEMPLATE_ENV: &str = "RUSTLING_TULIP_BIN_TEMPLATES";

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
            "rustling-tulipd: unknown argument \"{arg}\"; usage: rustling-tulipd [--detach | --version | <command> ...], where <command> is apply-update, bridge, kill, pair, read, screen, send, sessions, shutdown, spawn, wait or watch"
        )
    }

    #[test]
    fn launch_form_starts_only_for_no_arguments_or_detach_alone() {
        assert_eq!(launch_form(args(&[])), Ok(Launch::Start));
        assert_eq!(launch_form(args(&["--detach"])), Ok(Launch::Detach));
        for (list, offending) in [
            (&["frobnicate"][..], "frobnicate"),
            (&["sesions"][..], "sesions"),
            (&["--foo", "--detach"][..], "--foo"),
            (&["--detach", "extra"][..], "extra"),
            (&["--detached"][..], "--detached"),
        ] {
            assert_eq!(launch_form(args(list)), Err(refusal(offending)), "{list:?}");
        }
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
