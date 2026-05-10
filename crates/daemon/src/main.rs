//! rustling-tulipd: long-lived daemon that owns Claude Code sessions.

mod git;
mod paths;
mod pty;
mod registry;
mod server;
mod session;
mod state;
mod sync;

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

    server::run(state, dirs).await
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
