//! rustling-tulipd: long-lived daemon that owns Claude Code sessions.

mod git;
mod git_inspect;
mod headless;
mod orphan;
mod paths;
mod pty;
mod pty_state;
mod registry;
mod server;
mod session;
mod state;
mod sync;
mod vscode;
mod workspace;

use anyhow::Context as _;
use std::sync::Arc;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let dirs = paths::Dirs::ensure()?;
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

    server::run(state, dirs, live).await
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,daemon=debug,tower_http=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .compact()
        .init();
}
