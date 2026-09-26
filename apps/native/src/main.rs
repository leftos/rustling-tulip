//! Native (GPUI) rustling-tulip client binary: logging, then the window.
//!
//! Usage: `rustling-tulip-native [session-id]`. The daemon keeps the layout;
//! a session id is focused once the layout and the sessions arrive, as a
//! sidebar click would, placing it when no pane shows it. It starts the
//! daemon when none is running and reconnects when the connection drops.
//! Logs go to stderr and to `<config dir>/logs/native.log` (the previous
//! launch's log kept as `native.log.old`), filtered by `RUST_LOG` (default
//! `info`).

use anyhow::Context as _;
use gpui::{App, Application};
use rustling_tulip_native::{Assets, LOG_FILE};
use std::fs::File;
use std::path::Path;
use std::sync::Mutex;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

/// Move a non-empty `native.log` to `native.log.old`, replacing any older
/// generation, so each launch logs to a fresh file and the previous launch's
/// log survives one more launch. Mirrors the Tauri host's `app.log` rotation.
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

/// Rotate and open `<config dir>/logs/native.log` for this launch.
fn open_log_file() -> anyhow::Result<File> {
    let dir = daemon_client::config_dir()?.join("logs");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(LOG_FILE);
    rotate_log(&path).with_context(|| format!("rotating {}", path.display()))?;
    std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))
}

/// Log to stderr and to `native.log`; stderr alone when the file cannot be
/// opened.
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
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

/// Log every panic, the network thread's included, through tracing so it
/// reaches `native.log`, then run the default hook.
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        tracing::error!(
            thread = thread.name().unwrap_or("<unnamed>"),
            "panic: {info}"
        );
        default_hook(info);
    }));
}

fn main() {
    init_tracing();
    install_panic_hook();
    let wanted_session = std::env::args().nth(1);
    Application::new()
        .with_assets(Assets)
        .run(move |cx: &mut App| {
            rustling_tulip_native::open_main_window(wanted_session, cx);
        });
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::rotate_log;
    use std::path::Path;

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
