//! Native (GPUI) rustling-tulip client binary: logging, then the window.
//!
//! Usage: `rustling-tulip-native [session-id]`. The daemon keeps the layout;
//! a session id is focused once the layout and the sessions arrive, as a
//! sidebar click would, placing it when no pane shows it. A toast press on a
//! closed client starts it as `rustling-tulip-native -ToastActivated
//! -Embedding`, and neither flag is a session id. It starts the
//! daemon when none is running and reconnects when the connection drops.
//! Logs go to `<config dir>/logs/native.log` (the previous launch's log kept
//! as `native.log.old`), filtered by `RUST_LOG` (default `info`), and to
//! stderr only where the launch has one: a Windows build without debug
//! assertions is a GUI-subsystem exe with no console, so only a debug build
//! shows the log in a terminal.

// An installed launch (Start menu, desktop shortcut) must not open a console
// window beside the app, so release builds are GUI-subsystem exes; stderr
// logging then goes nowhere and native.log keeps the log. Debug builds keep
// the console so a terminal launch still shows the log.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use anyhow::Context as _;
use daemon_client::capped_log::{self, CappedLog};
use gpui::{App, Application};
use rustling_tulip_native::driver_gate::{self, GateDecision};
use rustling_tulip_native::start_guard::{self, NextStart, StartMode};
use rustling_tulip_native::{Assets, LOG_FILE, offscreen, toast_activation, update};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

/// Move a non-empty `native.log` to `native.log.old`, replacing any older
/// generation, so each launch logs to a fresh file and the previous launch's
/// log survives one more launch.
fn rotate_log(path: &Path) -> std::io::Result<()> {
    if std::fs::metadata(path).is_ok_and(|m| m.len() > 0) {
        let old = path.with_extension("log.old");
        // Windows rename fails when the target exists; drop the older
        // generation first (best-effort — rename reports the definitive
        // error).
        let _ = std::fs::remove_file(&old);
        std::fs::rename(path, &old)?;
    }
    Ok(())
}

/// Rotate and open `<config dir>/logs/native.log` for this launch, capped so a
/// long run rolls it to `native.log.1` instead of growing without limit (the
/// start-of-run rotation keeps its own `native.log.old` name).
fn open_log_file() -> anyhow::Result<CappedLog> {
    let dir = daemon_client::config_dir()?.join("logs");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(LOG_FILE);
    rotate_log(&path).with_context(|| format!("rotating {}", path.display()))?;
    CappedLog::open(&path, capped_log::DEFAULT_CAP)
        .with_context(|| format!("opening {}", path.display()))
}

/// Log to stderr and to `native.log`; stderr alone when the file cannot be
/// opened, which a Windows build without debug assertions has no console to
/// show.
fn init_tracing() {
    let filter = rustling_tulip_native::logging::filter_layer();
    let (file_layer, file_error) = match open_log_file() {
        Ok(file) => (
            Some(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(Mutex::new(file)),
            ),
            None,
        ),
        Err(err) => (None, Some(err)),
    };
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .with(file_layer)
        .init();
    if let Some(err) = file_error {
        tracing::warn!("logging to stderr only: {err:#}");
    }
}

/// Reads the start guard in `dir` and says how this start opens, logging a
/// safe start.
fn open_mode(dir: &Path) -> StartMode {
    let mode = start_guard::begin(dir);
    if mode != StartMode::Normal {
        tracing::warn!(?mode, "the last start crashed; opening safely");
    }
    mode
}

/// A panic on the main thread: counts it against this start, then says what
/// happened from a helper thread, since a modal loop on the panicking thread
/// would re-enter GPUI's window procedure. A cloaked run shows nothing, and
/// unwinding out of a Win32 callback runs the hook again, so only the first
/// panic shows the message.
fn on_main_thread_panic(config: Option<&Path>, message: &str) {
    let during_start = !start_guard::is_stable();
    let next = config.map_or(NextStart::Unaffected, |dir| {
        start_guard::next_start_after(during_start, start_guard::record_panic(dir, message))
    });
    if offscreen::requested() || !start_guard::claim_panic_box() {
        return;
    }
    let log_path = config.map_or_else(
        || PathBuf::from(LOG_FILE),
        |dir| dir.join("logs").join(LOG_FILE),
    );
    let text = start_guard::panic_text(message, next, &log_path);
    match std::thread::Builder::new()
        .name("panic-box".to_owned())
        .spawn(move || start_guard::show_panic_box(&text))
    {
        Ok(shower) => {
            let _ = shower.join();
        }
        Err(err) => tracing::warn!("showing the crash message: {err}"),
    }
}

/// Log every panic, the network thread's included, through tracing so it
/// reaches `native.log`, then run the default hook. A panic on the main thread
/// also counts against the start guard in `config` and tells the user; a
/// background thread's stays in the log, as the process carries on.
fn install_panic_hook(config: Option<PathBuf>) {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        tracing::error!(
            thread = thread.name().unwrap_or("<unnamed>"),
            "panic: {info}"
        );
        if thread.name() == Some("main") {
            on_main_thread_panic(config.as_deref(), &info.to_string());
        }
        default_hook(info);
    }));
}

/// Counts a run with no tab list from its daemon stable after its own window,
/// so a panic hours in is not read as a failed start, and marks it stable in
/// the start guard in `dir`, so a kill from then on counts nothing. The timer
/// runs on the app's main thread, where the guard's other writes happen.
fn start_no_tabs_timer(dir: Option<PathBuf>, cx: &mut App) {
    let timer = cx
        .background_executor()
        .timer(start_guard::no_tabs_stable_after(offscreen::requested()));
    cx.spawn(async move |_| {
        timer.await;
        match &dir {
            Some(dir) => start_guard::mark_stable_without_tabs(dir),
            None => start_guard::stable_without_tabs(),
        }
    })
    .detach();
}

/// Clears the run's mark in the start guard in `dir` when the app quits by a
/// way that skips the in-app quit, such as Cmd+Q on macOS; a safe start still
/// waiting on its prompt marks the run as asking instead.
fn clear_run_mark_on_quit(dir: PathBuf, cx: &mut App) {
    cx.on_app_quit(move |_| {
        start_guard::mark_clean_end(&dir);
        async {}
    })
    .detach();
}

/// Splits `--updated-from <version>`, which the installer's relaunch adds,
/// out of the launch arguments, so the version is never taken for a session
/// id.
fn take_updated_from(args: impl IntoIterator<Item = String>) -> (Vec<String>, Option<String>) {
    let mut rest = Vec::new();
    let mut updated_from = None;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--updated-from" {
            updated_from = args.next();
        } else {
            rest.push(arg);
        }
    }
    (rest, updated_from)
}

/// Whether this launch is a driven run, decided before anything touches a
/// folder; a refused one ends the process here.
fn driver_gate() -> bool {
    let override_vars: Vec<&str> = agent_registry::all()
        .iter()
        .map(|plugin| plugin.program_env_var())
        .collect();
    match driver_gate::decide(
        |variable| std::env::var(variable).ok(),
        cfg!(all(feature = "driver", windows)),
        &override_vars,
    ) {
        GateDecision::Off => false,
        GateDecision::Drive => true,
        GateDecision::Refuse(reason) => refuse_driver_run(&reason),
    }
}

/// Logs why a driven run was refused, to stderr only, and ends the process.
#[expect(
    clippy::exit,
    reason = "a refused driven run must stop before it touches any folder"
)]
fn refuse_driver_run(reason: &str) -> ! {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    tracing::error!("driver run refused: {reason}");
    std::process::exit(offscreen::DRIVER_REFUSED_EXIT)
}

fn main() {
    let drive = driver_gate();
    init_tracing();
    let config = daemon_client::config_dir()
        .inspect_err(|err| tracing::warn!("no config dir for the start guard: {err:#}"))
        .ok();
    let start_mode = config.as_deref().map_or_else(StartMode::default, open_mode);
    let guard_dir = config.clone();
    install_panic_hook(config);
    // Bring the login launch up to date: an entry already naming this install's
    // daemon is rewritten in its current `--detach` form. An entry naming any
    // other path, or none at all, is left alone. The cloaked smoke window
    // leaves the user's own login entry alone.
    if !offscreen::requested() {
        daemon_client::autostart::refresh_registration();
    }
    let (args, updated_from) = take_updated_from(std::env::args().skip(1));
    if let Some(version) = updated_from {
        tracing::info!(%version, "started by an update from this version");
    }
    // The cloaked smoke window leaves the user's own downloads alone.
    if !offscreen::requested() {
        update::clean_at_launch(daemon_client::updates_dir());
    }
    let launch = toast_activation::parse_launch_args(args);
    if launch.toast_activated {
        tracing::info!("started by a toast press");
    }
    // Before the window: a press that started this launch is waiting on the
    // activator's class object.
    toast_activation::register();
    let wanted_session = launch.session;
    Application::new()
        .with_assets(Assets)
        .run(move |cx: &mut App| {
            if let Some(dir) = guard_dir.clone() {
                clear_run_mark_on_quit(dir, cx);
            }
            start_no_tabs_timer(guard_dir, cx);
            rustling_tulip_native::open_main_window(wanted_session, drive, start_mode, cx);
        });
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::{rotate_log, take_updated_from};
    use std::path::Path;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    #[test]
    fn take_updated_from_leaves_args_without_the_flag_alone() {
        let (rest, from) = take_updated_from(args(&["s1", "-ToastActivated"]));
        assert_eq!(rest, args(&["s1", "-ToastActivated"]));
        assert_eq!(from, None);
    }

    #[test]
    fn take_updated_from_takes_the_flag_alone() {
        let (rest, from) = take_updated_from(args(&["--updated-from", "0.1.0"]));
        assert_eq!(rest, Vec::<String>::new());
        assert_eq!(from.as_deref(), Some("0.1.0"));
    }

    #[test]
    fn take_updated_from_keeps_the_other_args_in_order() {
        let (rest, from) = take_updated_from(args(&[
            "s1",
            "--updated-from",
            "0.1.0",
            "-ToastActivated",
            "-Embedding",
        ]));
        assert_eq!(rest, args(&["s1", "-ToastActivated", "-Embedding"]));
        assert_eq!(from.as_deref(), Some("0.1.0"));
    }

    #[test]
    fn take_updated_from_drops_a_trailing_flag_with_no_value() {
        let (rest, from) = take_updated_from(args(&["s1", "--updated-from"]));
        assert_eq!(rest, args(&["s1"]));
        assert_eq!(from, None);
    }

    #[test]
    fn rotate_moves_log_to_old() {
        let scratch = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("workspace root above apps/native")
            .join(".tmp")
            .join(format!("native-rotate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch).expect("create scratch dir");
        let log = scratch.join("native.log");
        let old = scratch.join("native.log.old");
        std::fs::write(&old, "two launches ago").expect("write older log");
        std::fs::write(&log, "last launch").expect("write log");

        rotate_log(&log).expect("rotate");

        assert!(!log.exists(), "native.log should have moved");
        let moved = std::fs::read_to_string(&old).expect("read native.log.old");
        let _ = std::fs::remove_dir_all(&scratch);
        assert_eq!(moved, "last launch");
    }
}
