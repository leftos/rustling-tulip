//! rustling-tulipd: long-lived daemon that owns Claude Code sessions.

mod binary_cache;
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
mod tracer_client;
mod vscode;
mod workspace;

use anyhow::Context as _;
use std::collections::HashSet;
use std::path::PathBuf;
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
        abandoned = dead.len(),
        "orphan recovery scan complete"
    );
    sweep_binary_cache(&dirs, &live, &dead);
    reap_orphan_tracers(&live, &dead);
    // Pre-B.2: dead sidecars were unconditionally deleted, losing recovery
    // context. Now we keep them — they become "abandoned" sessions the user
    // can Resume (replay spawn config + last_prompt against a fresh process)
    // or Discard from the sidebar. The sidecar stays on disk until one of
    // those handlers consumes it.

    // Persisted tabs reference session ids that may no longer be valid. After
    // orphan recovery, clear panes that point at dead sessions and drop tabs
    // with no surviving session bindings — otherwise a killed-daemon restart
    // resurrects an empty layout the user has to clear by hand.
    //
    // Abandoned sessions are not pruned: they still appear in the sidebar
    // (as `is_abandoned = true`) and need their tab/pane bindings intact so
    // a Resume swaps the abandoned session out for the freshly-spawned one
    // without the user losing their layout.
    prune_stale_tabs(&state, &live, &dead);

    let result = server::run(state, dirs, live, dead).await;
    info!(?result, "rustling-tulipd main returning");
    result
}

/// Prune cached binaries that no live tracer (or this daemon's own exe) is
/// using. Called once at startup after orphan recovery so we don't grow the
/// cache without bound across rebuilds. Failures are logged and swallowed —
/// a stale cache entry never blocks startup.
fn sweep_binary_cache(
    dirs: &paths::Dirs,
    live: &[orphan::OrphanMeta],
    dead: &[orphan::OrphanMeta],
) {
    let mut in_use: HashSet<PathBuf> = HashSet::new();

    // The daemon's own running exe — copy it into the cache (so it gets
    // hashed and tracked) and pin it for retain. Failure here is non-fatal:
    // we just won't prune anything, which is the safe default.
    match std::env::current_exe()
        .context("locating current daemon exe for cache GC")
        .and_then(|exe| {
            let stem = exe
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("rustling-tulipd")
                .to_string();
            binary_cache::ensure_cached(&exe, &dirs.binaries_dir, &stem)
        }) {
        Ok(cached) => {
            in_use.insert(cached);
        }
        Err(err) => {
            tracing::warn!(?err, "cache GC: skipping daemon exe pin");
        }
    }

    // Plus every tracer the live + abandoned sidecars know about. Abandoned
    // sessions can still be Resumed later via the tracer cache (the file is
    // small; better to keep a few stragglers than risk deleting an entry an
    // about-to-resume session would have used).
    for meta in live.iter().chain(dead.iter()) {
        if let Some(p) = meta.tracer_exe_path.as_deref() {
            in_use.insert(PathBuf::from(p));
        }
    }

    match binary_cache::gc(&dirs.binaries_dir, &in_use) {
        Ok(report) => info!(
            kept = report.kept,
            removed = report.removed,
            skipped = report.skipped,
            tmp_removed = report.tmp_removed,
            "binary cache GC complete"
        ),
        Err(err) => tracing::warn!(?err, "binary cache GC failed"),
    }
}

/// Find every `rt-tracer.exe` running on the system that no sidecar
/// references and force-kill it. These are tracers from a previous install
/// (different config dir, sidecars wiped, etc.) — we can't reattach to them
/// because we don't have their session metadata, so they're holding system
/// resources for nothing.
///
/// Sidecars in BOTH the live and abandoned buckets count as "referenced" —
/// abandoned sessions can still be Resumed by the user, and we don't want
/// to kill the supervisor out from under them. Best-effort: failures are
/// logged and skipped.
fn reap_orphan_tracers(live: &[orphan::OrphanMeta], dead: &[orphan::OrphanMeta]) {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System};

    let mut referenced: HashSet<u32> = HashSet::new();
    for meta in live.iter().chain(dead.iter()) {
        if let Some(pid) = meta.tracer_pid {
            referenced.insert(pid);
        }
    }

    let mut sys = System::new_with_specifics(
        RefreshKind::new().with_processes(ProcessRefreshKind::new()),
    );
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::new(),
    );

    // Cached tracers are spawned from `<binaries>/rt-tracer-<hash>.exe`, so
    // matching by exact filename misses every cached copy. Prefix-match the
    // executable stem instead — both the template (`rt-tracer.exe`) and any
    // cached copy (`rt-tracer-aaaaaaaaaaaaaaaa.exe`) start with `rt-tracer`.
    let our_pid = std::process::id();

    let mut killed = 0_usize;
    let mut failed = 0_usize;
    let mut spared = 0_usize;
    for (pid, proc_) in sys.processes() {
        if !is_tracer_image(&proc_.name().to_string_lossy()) {
            continue;
        }
        let pid_u32 = pid.as_u32();
        if pid_u32 == our_pid {
            continue;
        }
        if referenced.contains(&pid_u32) {
            spared += 1;
            continue;
        }
        if proc_.kill() {
            killed += 1;
        } else {
            failed += 1;
            tracing::warn!(pid = pid_u32, "could not kill orphan tracer");
        }
    }
    if killed > 0 || failed > 0 || spared > 0 {
        info!(
            killed,
            failed, spared, "orphan tracer reap complete"
        );
    }
}

/// Match `rt-tracer.exe`, `rt-tracer`, or any cached copy named
/// `rt-tracer-<hash>.exe`. The leading `-` after the stem prevents matching
/// unrelated processes that happen to share the prefix.
fn is_tracer_image(name: &str) -> bool {
    let stem = name
        .to_ascii_lowercase()
        .strip_suffix(".exe")
        .map_or_else(|| name.to_ascii_lowercase(), str::to_string);
    stem == "rt-tracer" || stem.starts_with("rt-tracer-")
}

fn prune_stale_tabs(
    state: &Arc<state::AppState>,
    live_orphans: &[orphan::OrphanMeta],
    abandoned: &[orphan::OrphanMeta],
) {
    let live_ids: HashSet<String> = live_orphans
        .iter()
        .map(|m| m.session_id.clone())
        .chain(abandoned.iter().map(|m| m.session_id.clone()))
        .collect();
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

#[cfg(test)]
mod tests {
    use super::is_tracer_image;

    #[test]
    fn matches_template_names() {
        assert!(is_tracer_image("rt-tracer.exe"));
        assert!(is_tracer_image("rt-tracer"));
        assert!(is_tracer_image("RT-Tracer.EXE"));
    }

    #[test]
    fn matches_cached_hashed_names() {
        assert!(is_tracer_image("rt-tracer-aaaaaaaaaaaaaaaa.exe"));
        assert!(is_tracer_image("rt-tracer-0123456789abcdef.exe"));
        assert!(is_tracer_image("rt-tracer-aaaaaaaaaaaaaaaa")); // unix
    }

    #[test]
    fn rejects_unrelated_names() {
        assert!(!is_tracer_image("rt-tracerfoo.exe"));
        assert!(!is_tracer_image("rustling-tulipd.exe"));
        assert!(!is_tracer_image("tracer.exe"));
        assert!(!is_tracer_image(""));
    }
}
