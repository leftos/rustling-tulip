//! rustling-tulipd: long-lived daemon that owns Claude Code sessions.

// A login launch (the HKCU `Run` entry) must not flash a console window, so on
// Windows the daemon is a GUI-subsystem exe. Every child it spawns sets
// `CREATE_NO_WINDOW`, so nothing downstream needs a console either. Tests keep
// the console so a failing test still prints.
#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

mod agents;
mod binary_cache;
mod branch_fate;
mod branch_names;
mod codex_rollout;
mod detach;
mod discovery;
mod env_secrets;
mod file_fetch;
mod git;
mod git_inspect;
mod git_watch;
mod git_write;
mod headless;
mod history;
mod idle_exit;
mod inject;
mod instance_lock;
mod keep_awake;
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
mod secret;
mod server;
mod session;
mod spawn_plan;
mod state;
mod sync;
mod tabs;
mod termstate;
mod tracer_client;
mod tracer_log;
mod transcripts;
mod user_env;
mod vscode;
mod workspace;
mod worktree_cleanup;
mod worktrees_admin;

use anyhow::Context as _;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::info;
use tracing_subscriber::EnvFilter;

/// How long to wait for another daemon's instance lock before concluding one
/// already serves this config dir. A client that retires an incompatible
/// daemon and spawns a replacement can have both processes alive for a moment;
/// the wait covers that handover, where a genuinely stale second launch would
/// otherwise exit at once.
const INSTANCE_LOCK_WAIT: Duration = Duration::from_secs(5);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dirs = paths::Dirs::ensure()?;
    // The login `Run` entry runs the installed exe with `--detach`: hand over
    // to a cached copy and exit, without taking the instance lock, the log or
    // the binary-cache sweep from a daemon that may already serve this config
    // dir.
    if detach::requested(std::env::args_os().skip(1)) {
        return detach::relaunch_from_cache(&dirs);
    }
    // One daemon per config dir. A second launch must exit here, before it
    // rotates the live daemon's log, sweeps its binary cache, reaps the orphan
    // tracers its live sessions depend on or overwrites its state.
    let Some(_instance_lock) = instance_lock::acquire(&dirs.config, INSTANCE_LOCK_WAIT)? else {
        init_stderr_only_tracing();
        info!(
            config_dir = %dirs.config.display(),
            "another rustling-tulipd holds daemon.lock for this config dir; exiting"
        );
        return Ok(());
    };
    init_tracing(&dirs);
    info!(config_dir = %dirs.config.display(), "starting rustling-tulipd");
    info!(
        rustling_tulip_claude = %std::env::var("RUSTLING_TULIP_CLAUDE").unwrap_or_else(|_| "(unset)".to_string()),
        "claude binary override status"
    );

    // Windows Credential Manager is the store secret environment rows are
    // sealed into. Install it before anything can seal a value; a machine that
    // cannot install it still starts, and a spawn carrying a secret row then
    // refuses rather than writing the value to disk.
    if let Err(err) = env_secrets::init() {
        tracing::warn!(?err, "the secret store could not be installed");
    }

    let state = state::AppState::load_or_default(&dirs).context("loading persisted state")?;
    let state = Arc::new(state);

    // A file written before secrets were sealed may still hold a literal value
    // under a secret-like key. Seal `state.json` and the sidecars now — before
    // orphan recovery reads the sidecars — so a reattached or recovered
    // session's config already carries references and the next start finds
    // nothing to seal. The history pass follows its prune, further down.
    seal_stored_secrets(&dirs, &state);

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
    let processes = running_processes();
    sweep_binary_cache(&dirs, &live, &dead, &processes);
    reap_orphan_tracers(&dirs, &live, &dead, &processes);
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
    import_tracer_logs(&dirs, &state, &live, &dead);
    prune_and_seal_history(&dirs, chrono::Utc::now());

    // The files just sealed and pruned are the only things that reference a
    // saved secret. An id nothing references and older than the grace period
    // is a value nothing can open again, so sweep it from the store and index.
    sweep_unreferenced_secrets(&dirs, &state, chrono::Utc::now());

    let result = server::run(state, dirs, live, dead).await;
    info!(?result, "rustling-tulipd main returning");
    result
}

/// Seal the secret literal rows `state.json` and the session sidecars hold.
/// Run before orphan recovery reads the sidecars, so a reattached or abandoned
/// session's config already carries references instead of literals.
fn seal_stored_secrets(dirs: &paths::Dirs, state: &state::AppState) {
    if let Err(err) = state.seal_last_spawn_configs() {
        tracing::warn!(?err, "sealing state.json's last spawn configs failed");
    }
    orphan::seal_sidecar_secrets(dirs);
}

/// Prune the ended-session history and then seal what survives. An entry past
/// the retention window is deleted before it is sealed, so a value only it
/// named never reaches the credential store and no id is swept an hour later.
fn prune_and_seal_history(dirs: &paths::Dirs, now: chrono::DateTime<chrono::Utc>) {
    history::prune(dirs, now, history::HISTORY_RETENTION);
    history::seal_history_secrets(dirs);
}

/// A stored source the reference walk could not read in full, so the sweep
/// cannot tell which secrets are still referenced from it.
#[derive(Debug)]
struct ReferenceGap {
    /// What the source is, for the warning line.
    source: &'static str,
    /// The file or folder that could not be read.
    path: String,
}

/// Every id the files a startup pass walks still reference through
/// `${secret:<id>}`: `state.json`'s `last_spawn_config`s, every session sidecar
/// and every history entry. Built after the history prune, so an id a pruned
/// entry alone named no longer counts as referenced.
///
/// # Errors
///
/// The first source the walk could not read — a `state.json` that failed to
/// parse, a session sidecar, a history entry, or a folder holding them.
fn referenced_secret_ids(
    dirs: &paths::Dirs,
    state: &state::AppState,
) -> Result<HashSet<String>, ReferenceGap> {
    if state.state_file_corrupt() {
        return Err(ReferenceGap {
            source: "state.json",
            path: dirs.state_file.display().to_string(),
        });
    }
    let mut ids = HashSet::new();
    state.with_persisted(|persisted| {
        for config in persisted
            .repos
            .iter()
            .filter_map(|repo| repo.last_spawn_config.as_ref())
            .chain(
                persisted
                    .workspaces
                    .iter()
                    .filter_map(|workspace| workspace.last_spawn_config.as_ref()),
            )
        {
            env_secrets::referenced_ids(&config.extra_env, &mut ids);
        }
    });
    let metas = orphan::read_all_metas_for_references(dirs).map_err(|path| ReferenceGap {
        source: "a session sidecar",
        path: path.display().to_string(),
    })?;
    for config in metas.iter().filter_map(|meta| meta.spawn_config.as_ref()) {
        env_secrets::referenced_ids(&config.extra_env, &mut ids);
    }
    let entries = history::read_all_for_references(dirs).map_err(|path| ReferenceGap {
        source: "a session history entry",
        path: path.display().to_string(),
    })?;
    for config in entries
        .iter()
        .filter_map(|entry| entry.spawn_config.as_ref())
    {
        env_secrets::referenced_ids(&config.extra_env, &mut ids);
    }
    Ok(ids)
}

/// Delete the saved secrets nothing stored references and older than the grace
/// period. `now` is a parameter so a test can age an id past that period.
///
/// A source the walk could not read — a sidecar or history file it skipped, a
/// folder it could not list, a `state.json` that failed to parse — may hold
/// the only reference to an id, so the sweep is skipped for that start rather
/// than deleting a value a later Recover would still need.
fn sweep_unreferenced_secrets(
    dirs: &paths::Dirs,
    state: &state::AppState,
    now: chrono::DateTime<chrono::Utc>,
) {
    match referenced_secret_ids(dirs, state) {
        Ok(referenced) => {
            let removed = env_secrets::collect_unreferenced(&referenced, now);
            info!(removed, "swept unreferenced saved secrets");
        }
        Err(gap) => tracing::warn!(
            source = gap.source,
            path = %gap.path,
            "a stored file could not be read; skipping the unreferenced-secret sweep"
        ),
    }
}

/// Add the sessions whose tracer logs predate the session history to it,
/// leaving out the live and abandoned sessions orphan recovery just found.
fn import_tracer_logs(
    dirs: &paths::Dirs,
    state: &state::AppState,
    live: &[orphan::OrphanMeta],
    dead: &[orphan::OrphanMeta],
) {
    let skip: std::collections::HashSet<String> = live
        .iter()
        .chain(dead)
        .map(|meta| meta.session_id.clone())
        .collect();
    let (repos, workspaces) = state.with_persisted(|s| (s.repos.clone(), s.workspaces.clone()));
    let registered = history::Registered {
        repos: &repos,
        workspaces: &workspaces,
    };
    history::import_tracer_logs(dirs, &registered, &skip, chrono::Utc::now());
}

/// One snapshot of every running process with its exe path and environment,
/// shared by the cache GC and the tracer reap.
fn running_processes() -> sysinfo::System {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System, UpdateKind};

    let refresh = ProcessRefreshKind::new()
        .with_exe(UpdateKind::Always)
        .with_environ(UpdateKind::Always);
    let mut sys = System::new_with_specifics(RefreshKind::new().with_processes(refresh));
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);
    sys
}

/// Prune cached binaries that no live tracer, no running process (another
/// daemon sharing the cache included) and not this daemon's own exe is
/// using. Called once at startup after orphan recovery so we don't grow the
/// cache without bound across rebuilds. Failures are logged and swallowed —
/// a stale cache entry never blocks startup.
fn sweep_binary_cache(
    dirs: &paths::Dirs,
    live: &[orphan::OrphanMeta],
    dead: &[orphan::OrphanMeta],
    processes: &sysinfo::System,
) {
    let mut in_use: HashSet<PathBuf> = HashSet::new();

    // Pin the daemon's own running exe. The client's supervisor
    // (`daemon-client`) spawned us from a cached copy
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

    // Plus every binary a running process uses from the cache: a daemon on
    // another config dir sharing it, and that daemon's tracers.
    in_use.extend(pinned_cache_paths(
        processes
            .processes()
            .values()
            .filter_map(sysinfo::Process::exe),
        &dirs.binaries_dir,
    ));

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

/// The cache entry path for every exe in `exes` that runs from under
/// `binaries_dir`, re-joined onto `binaries_dir` so it names the entry the
/// way cache GC reads it, whatever form the process reported its path in.
fn pinned_cache_paths<'a>(
    exes: impl IntoIterator<Item = &'a Path>,
    binaries_dir: &Path,
) -> HashSet<PathBuf> {
    exes.into_iter()
        .filter(|exe| path_is_under(exe, binaries_dir))
        .filter_map(Path::file_name)
        .map(|name| binaries_dir.join(name))
        .collect()
}

/// Find every `rt-tracer.exe` running from the binary cache that this config
/// dir owns and no sidecar references, and force-kill it. The cache is
/// machine-wide, so a tracer another config dir's daemon spawned is left
/// alone, and so is one that names no owner.
///
/// Sidecars in BOTH the live and abandoned buckets count as "referenced" —
/// abandoned sessions can still be Resumed by the user, and we don't want
/// to kill the supervisor out from under them. Best-effort: failures are
/// logged and skipped.
fn reap_orphan_tracers(
    dirs: &paths::Dirs,
    live: &[orphan::OrphanMeta],
    dead: &[orphan::OrphanMeta],
    processes: &sysinfo::System,
) {
    let referenced: HashSet<u32> = live
        .iter()
        .chain(dead.iter())
        .filter_map(|meta| meta.tracer_pid)
        .collect();
    let scope = ReapScope {
        config: &dirs.config,
        binaries_dir: &dirs.binaries_dir,
        referenced: &referenced,
        our_pid: std::process::id(),
    };

    let mut killed = 0_usize;
    let mut failed = 0_usize;
    let mut spared_foreign = 0_usize;
    let mut spared_unmarked = 0_usize;
    let mut spared_referenced = 0_usize;
    let mut spared_conflicting = 0_usize;
    for (pid, proc_) in processes.processes() {
        let name = proc_.name().to_string_lossy();
        let view = ProcessView {
            pid: pid.as_u32(),
            name: &name,
            exe: proc_.exe(),
            environ: proc_.environ(),
        };
        match reap_verdict(&view, &scope) {
            ReapVerdict::NotConsidered => {}
            ReapVerdict::Spare(SpareReason::Foreign) => spared_foreign += 1,
            ReapVerdict::Spare(SpareReason::Unmarked) => spared_unmarked += 1,
            ReapVerdict::Spare(SpareReason::Referenced) => spared_referenced += 1,
            ReapVerdict::Spare(SpareReason::ConflictingOwner) => spared_conflicting += 1,
            ReapVerdict::Kill if proc_.kill() => killed += 1,
            ReapVerdict::Kill => {
                failed += 1;
                tracing::warn!(pid = view.pid, "could not kill orphan tracer");
            }
        }
    }
    let spared = spared_foreign + spared_unmarked + spared_referenced + spared_conflicting;
    if killed + failed + spared > 0 {
        info!(
            killed,
            spared_foreign,
            spared_unmarked,
            spared_referenced,
            spared_conflicting,
            failed,
            "orphan tracer reap complete"
        );
    }
}

/// Why the startup reap left a tracer from the binary cache running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpareReason {
    /// Another config dir's daemon spawned it.
    Foreign,
    /// It names no owner, or its environment could not be read.
    Unmarked,
    /// A sidecar of this config dir references it.
    Referenced,
    /// Its owner marker and its log path name different config dirs.
    ConflictingOwner,
}

/// What the startup reap does with one running process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReapVerdict {
    /// Not a tracer running from the binary cache, or this process itself.
    NotConsidered,
    Kill,
    Spare(SpareReason),
}

/// One running process as the reap sees it.
struct ProcessView<'a> {
    pid: u32,
    name: &'a str,
    exe: Option<&'a Path>,
    /// `KEY=VALUE` entries; empty when the environment could not be read.
    environ: &'a [std::ffi::OsString],
}

/// Whose tracers the reap may kill.
struct ReapScope<'a> {
    config: &'a Path,
    binaries_dir: &'a Path,
    /// Tracer pids this config dir's sidecars reference.
    referenced: &'a HashSet<u32>,
    our_pid: u32,
}

/// Kill a tracer running from the binary cache only when this config dir
/// owns it and no sidecar references it.
///
/// Cached tracers are spawned from `<binaries>/rt-tracer-<hash>.exe`, so the
/// image is matched by stem prefix (see [`is_tracer_image`]), not exact name.
fn reap_verdict(process: &ProcessView<'_>, scope: &ReapScope<'_>) -> ReapVerdict {
    let from_cache = process
        .exe
        .is_some_and(|exe| path_is_under(exe, scope.binaries_dir));
    if !is_tracer_image(process.name) || !from_cache || process.pid == scope.our_pid {
        return ReapVerdict::NotConsidered;
    }
    let owner = match tracer_owner(process.environ) {
        TracerOwner::Known(owner) => owner,
        TracerOwner::Conflicting => return ReapVerdict::Spare(SpareReason::ConflictingOwner),
        TracerOwner::Unknown => return ReapVerdict::Spare(SpareReason::Unmarked),
    };
    if normalize_process_path(&owner) != normalize_process_path(scope.config) {
        return ReapVerdict::Spare(SpareReason::Foreign);
    }
    if scope.referenced.contains(&process.pid) {
        return ReapVerdict::Spare(SpareReason::Referenced);
    }
    ReapVerdict::Kill
}

/// The config dir whose daemon spawned a tracer, read from its environment:
/// the owner marker, else the config dir its per-session log lies under
/// (`<config>/logs/tracer-<id>.log`).
///
/// The two can disagree: a tracer passes its environment down to its child,
/// so a daemon started from inside a session shell inherits the outer owner
/// marker and hands it to its own tracers, whose log path names the inner
/// config dir. No owner can be trusted then.
fn tracer_owner(environ: &[std::ffi::OsString]) -> TracerOwner {
    let marker = env_value(environ, tracer_client::TRACER_OWNER_ENV).map(PathBuf::from);
    let from_log = env_value(environ, tracer_client::TRACER_LOG_ENV)
        .and_then(|log| Path::new(&log).parent()?.parent().map(Path::to_path_buf));
    match (marker, from_log) {
        (Some(marker), Some(from_log))
            if normalize_process_path(&marker) != normalize_process_path(&from_log) =>
        {
            TracerOwner::Conflicting
        }
        (Some(owner), _) | (None, Some(owner)) => TracerOwner::Known(owner),
        (None, None) => TracerOwner::Unknown,
    }
}

/// A tracer's owner as its environment names it.
enum TracerOwner {
    Known(PathBuf),
    /// The owner marker and the log path name different config dirs.
    Conflicting,
    /// Neither is set, or the environment could not be read.
    Unknown,
}

/// The non-empty value of `key` in `KEY=VALUE` entries; the key matches
/// case-insensitively on Windows, as its environment does.
fn env_value(environ: &[std::ffi::OsString], key: &str) -> Option<String> {
    environ.iter().find_map(|entry| {
        let entry = entry.to_string_lossy();
        let (name, value) = entry.split_once('=')?;
        let matches = if cfg!(windows) {
            name.eq_ignore_ascii_case(key)
        } else {
            name == key
        };
        (matches && !value.is_empty()).then(|| value.to_string())
    })
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

/// Logging for a daemon that lost the single-instance race: stderr only, since
/// the file writer would rotate the live daemon's `daemon.log` out from under
/// it. The Windows supervisor redirects stderr to NUL, so this line is
/// normally invisible — it exists for a hand-run second daemon and for any
/// caller that captures stderr.
fn init_stderr_only_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(true)
        .compact()
        .init();
}

fn init_tracing(dirs: &paths::Dirs) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,daemon=debug,tower_http=info"));

    // Rotate-then-truncate so each launch's log is a clean slate while the
    // previous run's log survives as `daemon.log.old` — the record of what a
    // dying/misbehaving daemon did is only ever needed AFTER its successor
    // has started. Falls back silently to stderr if the file can't be opened
    // (e.g. ACL issues) — since the supervisor redirects stderr to NUL on
    // Windows the user wouldn't see those logs anyway, but losing the writer
    // must not panic.
    let log_dir = dirs.config.join("logs");
    let dir_create_err = std::fs::create_dir_all(&log_dir).err();
    let log_path = log_dir.join("daemon.log");
    let rotate_err = rotate_log(&log_path).err();
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
    if let Some(err) = rotate_err {
        tracing::warn!(?err, path = %log_path.display(), "previous log rotation failed (continuing)");
    }
}

/// Rotate `path` to `<path>.old`, replacing any earlier generation. A missing
/// or empty log is left alone (nothing worth keeping). Runs before the
/// tracing subscriber exists, so the error is returned for the caller to log
/// once a writer is up; rotation failure must never block startup.
fn rotate_log(path: &Path) -> std::io::Result<()> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.len() > 0 => {}
        _ => return Ok(()),
    }
    let mut old = path.as_os_str().to_owned();
    old.push(".old");
    let old = PathBuf::from(old);
    // Windows rename fails when the target exists; drop the older generation
    // first (best-effort — rename reports the definitive error).
    let _ = std::fs::remove_file(&old);
    std::fs::rename(path, &old)
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::{
        ProcessView, ReapScope, ReapVerdict, SpareReason, is_tracer_image, path_is_under,
        pinned_cache_paths, reap_verdict, rotate_log,
    };
    use crate::tracer_client::{TRACER_LOG_ENV, TRACER_OWNER_ENV};
    use std::collections::HashSet;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    /// A scratch root for reap scopes; nothing is read from or written to it.
    fn reap_root() -> PathBuf {
        std::env::temp_dir().join("rt-reap-fixture")
    }

    fn env_entry(key: &str, value: &Path) -> OsString {
        OsString::from(format!("{key}={}", value.display()))
    }

    /// The verdict for pid 42, a cached tracer under `binaries`, whose
    /// environment is `environ`, for a daemon on `config`.
    fn verdict(
        config: &Path,
        binaries: &Path,
        environ: &[OsString],
        referenced: &[u32],
    ) -> ReapVerdict {
        let exe = binaries.join("rt-tracer-aaaaaaaaaaaaaaaa.exe");
        let referenced: HashSet<u32> = referenced.iter().copied().collect();
        reap_verdict(
            &ProcessView {
                pid: 42,
                name: "rt-tracer-aaaaaaaaaaaaaaaa.exe",
                exe: Some(&exe),
                environ,
            },
            &ReapScope {
                config,
                binaries_dir: binaries,
                referenced: &referenced,
                our_pid: 1,
            },
        )
    }

    #[test]
    fn reap_kills_unreferenced_tracer_owned_by_this_config_dir() {
        let root = reap_root();
        let config = root.join("config");
        let environ = [env_entry(TRACER_OWNER_ENV, &config)];
        assert_eq!(
            verdict(&config, &root.join("binaries"), &environ, &[]),
            ReapVerdict::Kill
        );
    }

    #[test]
    fn reap_spares_tracer_owned_by_another_config_dir() {
        let root = reap_root();
        let environ = [env_entry(TRACER_OWNER_ENV, &root.join("other-config"))];
        assert_eq!(
            verdict(&root.join("config"), &root.join("binaries"), &environ, &[]),
            ReapVerdict::Spare(SpareReason::Foreign)
        );
    }

    #[test]
    fn reap_uses_tracer_log_path_when_owner_marker_missing() {
        let root = reap_root();
        let config = root.join("config");
        let binaries = root.join("binaries");
        let ours = [env_entry(
            TRACER_LOG_ENV,
            &config.join("logs").join("tracer-abc.log"),
        )];
        assert_eq!(verdict(&config, &binaries, &ours, &[]), ReapVerdict::Kill);
        let theirs = [env_entry(
            TRACER_LOG_ENV,
            &root
                .join("other-config")
                .join("logs")
                .join("tracer-abc.log"),
        )];
        assert_eq!(
            verdict(&config, &binaries, &theirs, &[]),
            ReapVerdict::Spare(SpareReason::Foreign)
        );
    }

    #[test]
    fn reap_spares_tracer_without_any_owner_hint() {
        let root = reap_root();
        let config = root.join("config");
        let binaries = root.join("binaries");
        let unrelated = [OsString::from("PATH=somewhere")];
        assert_eq!(
            verdict(&config, &binaries, &unrelated, &[]),
            ReapVerdict::Spare(SpareReason::Unmarked)
        );
        assert_eq!(
            verdict(&config, &binaries, &[], &[]),
            ReapVerdict::Spare(SpareReason::Unmarked),
            "an unreadable environment reads as empty"
        );
    }

    #[test]
    fn reap_spares_tracer_whose_owner_hints_disagree() {
        let root = reap_root();
        let config = root.join("config");
        let binaries = root.join("binaries");
        let inner = root.join("inner-config");
        let inherited = [
            env_entry(TRACER_OWNER_ENV, &config),
            env_entry(TRACER_LOG_ENV, &inner.join("logs").join("tracer-abc.log")),
        ];
        assert_eq!(
            verdict(&config, &binaries, &inherited, &[]),
            ReapVerdict::Spare(SpareReason::ConflictingOwner)
        );
        let agreeing = [
            env_entry(TRACER_OWNER_ENV, &config),
            env_entry(TRACER_LOG_ENV, &config.join("logs").join("tracer-abc.log")),
        ];
        assert_eq!(
            verdict(&config, &binaries, &agreeing, &[]),
            ReapVerdict::Kill
        );
    }

    #[test]
    fn reap_spares_referenced_tracer() {
        let root = reap_root();
        let config = root.join("config");
        let environ = [env_entry(TRACER_OWNER_ENV, &config)];
        assert_eq!(
            verdict(&config, &root.join("binaries"), &environ, &[42]),
            ReapVerdict::Spare(SpareReason::Referenced)
        );
    }

    #[cfg(windows)]
    #[test]
    fn owner_compare_ignores_case_and_verbatim_prefix() {
        let config = Path::new(r"C:\Users\Someone\AppData\Config");
        let binaries = Path::new(r"C:\Users\Someone\AppData\binaries");
        let environ = [OsString::from(format!(
            r"{}=\\?\c:\users\someone\appdata\CONFIG",
            TRACER_OWNER_ENV.to_ascii_lowercase()
        ))];
        assert_eq!(verdict(config, binaries, &environ, &[]), ReapVerdict::Kill);
    }

    #[test]
    fn pinned_cache_paths_keeps_running_exes_under_the_cache() {
        let root = reap_root();
        let binaries = root.join("binaries");
        let tracer = binaries.join("rt-tracer-aaaaaaaaaaaaaaaa.exe");
        let daemon = binaries.join("rustling-tulipd-bbbbbbbbbbbbbbbb.exe");
        let elsewhere = root.join("target").join("rt-tracer.exe");
        let pinned = pinned_cache_paths(
            [tracer.as_path(), daemon.as_path(), elsewhere.as_path()],
            &binaries,
        );
        let expected: HashSet<PathBuf> = [tracer, daemon].into_iter().collect();
        assert_eq!(pinned, expected);
    }

    #[test]
    fn rotate_log_moves_previous_generation_aside() {
        let dir = std::env::temp_dir().join(format!("rt-rotate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        let log = dir.join("daemon.log");
        let old = dir.join("daemon.log.old");

        // Missing log: nothing to do, no error.
        rotate_log(&log).expect("rotating a missing log is a no-op");
        assert!(!old.exists());

        // Empty log: left alone (nothing worth keeping).
        std::fs::write(&log, b"").expect("write empty log");
        rotate_log(&log).expect("rotating an empty log is a no-op");
        assert!(log.exists());
        assert!(!old.exists());

        // Non-empty log: moved to .old.
        std::fs::write(&log, b"first run").expect("write log");
        rotate_log(&log).expect("rotate non-empty log");
        assert!(!log.exists());
        assert_eq!(std::fs::read(&old).expect("read rotated log"), b"first run");

        // Next rotation replaces the previous generation.
        std::fs::write(&log, b"second run").expect("write log again");
        rotate_log(&log).expect("rotate over existing .old");
        assert_eq!(
            std::fs::read(&old).expect("read rotated log"),
            b"second run"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

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
