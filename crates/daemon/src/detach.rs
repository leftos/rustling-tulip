//! The daemon's `--detach` relaunch path.
//!
//! The "start on login" setting writes an HKCU `Run` entry pointing at the
//! installed `rustling-tulipd.exe`, so at login Windows starts that file
//! directly — and a running executable cannot be replaced by the next
//! installer or rebuild. With `--detach` that launch becomes a short-lived
//! launcher instead: it copies itself into the content-addressed binaries
//! cache and spawns the cached copy, exactly as the clients do, then exits.
//! Without the flag the daemon behaves as before.

use anyhow::Context as _;
use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use tracing::info;
use tracing_subscriber::EnvFilter;

/// True when `--detach` is one of the daemon's arguments (`args` is the
/// argument list without the program name). Any other argument is ignored, as
/// it is today.
pub fn requested(args: impl IntoIterator<Item = OsString>) -> bool {
    args.into_iter().any(|arg| arg == "--detach")
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
    use super::{child_command, requested};
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

    #[test]
    fn requested_matches_the_detach_flag_only() {
        assert!(requested(args(&["--detach"])));
        assert!(requested(args(&["--foo", "--detach"])));
        assert!(!requested(Vec::<OsString>::new()));
        assert!(!requested(args(&["--detached"])));
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
