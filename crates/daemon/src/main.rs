//! rustling-tulipd: long-lived daemon that owns Claude Code sessions.

mod git;
mod git_inspect;
mod git_watch;
mod git_write;
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
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dirs = paths::Dirs::ensure()?;
    init_tracing(&dirs);
    info!(config_dir = %dirs.config.display(), "starting rustling-tulipd");
    info!(
        rustling_tulip_claude = %std::env::var("RUSTLING_TULIP_CLAUDE").unwrap_or_else(|_| "(unset)".to_string()),
        "claude binary override status"
    );

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

    // Persisted tabs reference session ids that may no longer be valid. After
    // orphan recovery, clear panes that point at dead sessions and drop tabs
    // with no surviving session bindings — otherwise a killed-daemon restart
    // resurrects an empty layout the user has to clear by hand.
    prune_stale_tabs(&state, &live);

    let result = server::run(state, dirs, live).await;
    info!(?result, "rustling-tulipd main returning");
    result
}

fn prune_stale_tabs(state: &Arc<state::AppState>, live_orphans: &[orphan::OrphanMeta]) {
    let live_ids: HashSet<String> = live_orphans.iter().map(|m| m.session_id.clone()).collect();
    let result = state.mutate(|s| {
        let prev_tab_count = s.tabs.len();
        let mut panes_cleared = 0usize;
        for tab in &mut s.tabs {
            let Some(grid) = tab.grid_mut() else {
                continue;
            };
            if tabs::prune_sessions_not_in(grid, &live_ids) {
                panes_cleared += 1;
            }
        }
        // Non-grid tabs (e.g. diff tabs) survive the prune unconditionally;
        // grid tabs are kept only if they still bind at least one session.
        s.tabs
            .retain(|t| t.grid().is_none_or(tabs::has_any_session));
        let tabs_dropped = prev_tab_count.saturating_sub(s.tabs.len());
        (panes_cleared, tabs_dropped)
    });
    match result {
        Ok((panes_cleared, tabs_dropped)) if panes_cleared > 0 || tabs_dropped > 0 => {
            info!(
                panes_cleared,
                tabs_dropped, "pruned tabs referencing dead sessions"
            );
        }
        Ok(_) => {}
        Err(err) => tracing::warn!(?err, "tab prune failed; continuing with stale state"),
    }
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
