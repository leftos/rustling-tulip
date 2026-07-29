//! rustling-tulipd: long-lived daemon that owns Claude Code sessions.

mod agents;
mod binary_cache;
mod discovery;
mod git;
mod git_inspect;
mod git_watch;
mod git_write;
mod headless;
mod inject;
mod lan;
mod lock_finder;
mod orphan;
mod osc_title;
mod pairing;
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
mod termstate;
mod tracer_client;
mod vscode;
mod workspace;
mod worktree_cleanup;
mod worktrees_admin;

use anyhow::Context as _;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
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
    reap_orphan_tracers(&dirs, &live, &dead);
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

    // Pin the daemon's own running exe. Tauri spawned us from a cached copy
    // (`<binaries_dir>/rustling-tulipd-<hash>.exe`), so current_exe() is
    // already a cache entry — including it here keeps GC from deleting the
    // file out from under our process. If the daemon was started directly
    // from `target/<profile>/rustling-tulipd.exe` (dev `cargo run`), the
    // path is outside `binaries_dir` and GC simply doesn't see it; pinning
    // a non-cache path is harmless.
    match std::env::current_exe().context("locating current daemon exe for cache GC") {
        Ok(exe) => {
            in_use.insert(exe);
        }
        Err(err) => {
            tracing::warn!(?err, "cache GC: could not pin daemon exe");
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

/// Find every `rt-tracer.exe` running from this daemon's binary cache that no
/// sidecar references and force-kill it. Tracers from a different launcher or
/// e2e run live in a different binary cache and are left alone.
///
/// Sidecars in BOTH the live and abandoned buckets count as "referenced" —
/// abandoned sessions can still be Resumed by the user, and we don't want
/// to kill the supervisor out from under them. Best-effort: failures are
/// logged and skipped.
fn reap_orphan_tracers(
    dirs: &paths::Dirs,
    live: &[orphan::OrphanMeta],
    dead: &[orphan::OrphanMeta],
) {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System, UpdateKind};

    let mut referenced: HashSet<u32> = HashSet::new();
    for meta in live.iter().chain(dead.iter()) {
        if let Some(pid) = meta.tracer_pid {
            referenced.insert(pid);
        }
    }

    let process_refresh = ProcessRefreshKind::new().with_exe(UpdateKind::Always);
    let mut sys = System::new_with_specifics(RefreshKind::new().with_processes(process_refresh));
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, process_refresh);

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
        if !proc_
            .exe()
            .is_some_and(|exe| path_is_under(exe, &dirs.binaries_dir))
        {
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
        info!(killed, failed, spared, "orphan tracer reap complete");
    }
}

/// Match `rt-tracer.exe`, `rt-tracer`, or any cached copy named
/// `rt-tracer-<hash>.exe`. The leading `-` after the stem prevents matching
/// unrelated processes that happen to share the prefix.
fn is_tracer_image(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    #[cfg(windows)]
    let stem = lower.strip_suffix(".exe").unwrap_or(&lower);
    #[cfg(not(windows))]
    let stem = lower.as_str();
    stem == "rt-tracer" || stem.starts_with("rt-tracer-")
}

fn path_is_under(path: &Path, root: &Path) -> bool {
    let path = normalize_process_path(path);
    let root = normalize_process_path(root);
    path == root || path.starts_with(&format!("{root}{}", std::path::MAIN_SEPARATOR))
}

#[cfg(windows)]
fn normalize_process_path(path: &Path) -> String {
    let raw = path.to_string_lossy().replace('/', "\\");
    let trimmed = raw
        .strip_prefix(r"\\?\UNC\")
        .map(|rest| format!(r"\\{rest}"))
        .or_else(|| raw.strip_prefix(r"\\?\").map(str::to_string))
        .unwrap_or(raw);
    trimmed.trim_end_matches('\\').to_ascii_lowercase()
}

#[cfg(not(windows))]
fn normalize_process_path(path: &Path) -> String {
    path.to_string_lossy().trim_end_matches('/').to_string()
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
    // Prune one tab vec in place: clear panes whose session is dead, then drop
    // grid tabs left with no live session. Non-grid tabs (e.g. diff tabs)
    // always survive. Returns (panes_cleared, tabs_dropped).
    let prune_vec = |tabs: &mut Vec<protocol::TabEntry>| {
        let prev = tabs.len();
        let mut panes_cleared = 0usize;
        for tab in tabs.iter_mut() {
            let Some(grid) = tab.grid_mut() else {
                continue;
            };
            if tabs::prune_sessions_not_in(grid, &live_ids) {
                panes_cleared += 1;
            }
        }
        tabs.retain(|t| t.grid().is_none_or(tabs::has_any_session));
        (panes_cleared, prev.saturating_sub(tabs.len()))
    };
    let result = state.mutate(|s| {
        let mut panes_cleared = 0usize;
        let mut tabs_dropped = 0usize;
        for layout in s.layouts.values_mut() {
            let (p, t) = prune_vec(&mut layout.tabs);
            panes_cleared += p;
            tabs_dropped += t;
        }
        let (p, t) = prune_vec(&mut s.legacy_tabs);
        panes_cleared += p;
        tabs_dropped += t;
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
    use super::{is_tracer_image, path_is_under};
    use std::path::Path;

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

    #[cfg(windows)]
    #[test]
    fn path_scope_matches_only_cache_children() {
        let root = Path::new(r"C:\rt\.tmp\e2e\binaries");
        assert!(path_is_under(
            Path::new(r"C:\rt\.tmp\e2e\binaries\rt-tracer-hash.exe"),
            root,
        ));
        assert!(!path_is_under(
            Path::new(r"C:\rt\.tmp\e2e\binaries-other\rt-tracer-hash.exe"),
            root,
        ));
    }

    #[cfg(not(windows))]
    #[test]
    fn path_scope_matches_only_cache_children() {
        let root = Path::new("/tmp/rt/.tmp/e2e/binaries");
        assert!(path_is_under(
            Path::new("/tmp/rt/.tmp/e2e/binaries/rt-tracer-hash"),
            root,
        ));
        assert!(!path_is_under(
            Path::new("/tmp/rt/.tmp/e2e/binaries-other/rt-tracer-hash"),
            root,
        ));
    }
}
