//! rustling-tulipd: long-lived daemon that owns Claude Code sessions.

mod git;
mod git_inspect;
mod headless;
mod inject;
mod orphan;
mod osc_title;
mod paths;
mod presets;
mod pty;
mod pty_state;
mod registry;
mod scrollback;
mod server;
mod session;
mod state;
mod sync;
mod tabs;
mod vscode;
mod workspace;

use anyhow::Context as _;
use std::sync::{Arc, Mutex};
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dirs = paths::Dirs::ensure()?;
    init_tracing(&dirs);
    info!(config_dir = %dirs.config.display(), "starting rustling-tulipd");

    let state = state::AppState::load_or_default(&dirs).context("loading persisted state")?;
    let state = Arc::new(state);

    let metas = orphan::read_all_metas(&dirs).unwrap_or_else(|err| {
        tracing::warn!(?err, "failed to read orphan metas; starting fresh");
        Vec::new()
    });
    let (live, dead) = orphan::partition_live(metas);
    info!(
        live = live.len(),
        dead = dead.len(),
        "orphan recovery scan complete"
    );
    for meta in &dead {
        orphan::try_delete_meta(&dirs, &meta.session_id);
    }

    let result = server::run(state, dirs, live).await;
    info!(?result, "rustling-tulipd main returning");
    result
}

fn init_tracing(dirs: &paths::Dirs) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,daemon=debug,tower_http=info"));

    // Truncate-on-start so each launch's log is a clean slate. Falls back
    // silently to stderr if the file can't be opened (e.g. ACL issues) —
    // since the supervisor redirects stderr to NUL on Windows the user
    // wouldn't see those logs anyway, but losing the writer must not panic.
    let log_dir = dirs.config.join("logs");
    let dir_create_err = std::fs::create_dir_all(&log_dir).err();
    let log_path = log_dir.join("daemon.log");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&log_path);

    match file {
        Ok(f) => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(Mutex::new(f))
                .with_ansi(false)
                .with_target(true)
                .compact()
                .init();
            info!(log_file = %log_path.display(), "daemon logging to file");
            if let Some(err) = dir_create_err {
                tracing::warn!(?err, dir = %log_dir.display(), "log dir create failed (continuing)");
            }
        }
        Err(err) => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_target(true)
                .compact()
                .init();
            tracing::warn!(?err, path = %log_path.display(), "daemon could not open log file; using stderr");
            if let Some(err) = dir_create_err {
                tracing::warn!(?err, dir = %log_dir.display(), "log dir create failed");
            }
        }
    }
}
