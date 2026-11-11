//! Configuration / state directory layout.

use anyhow::Context as _;
use daemon_client::profile::{self, Overrides, Profile};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use tracing::warn;

#[derive(Debug, Clone)]
pub struct Dirs {
    pub config: PathBuf,
    pub state_file: PathBuf,
    pub handshake_file: PathBuf,
    /// Opt-in remote (Tailscale) access config: `<config>/remote.json` holds
    /// `{ enabled, port }`. Absent until the user turns remote access on; a
    /// missing, unreadable or zero-port file means off.
    pub remote_config_file: PathBuf,
    /// Per-session sidecar directory: `<config>/sessions/<session-id>/` holds
    /// `meta.json` (orphan recovery) and `scrollback.bin` (replay on attach).
    pub sessions_dir: PathBuf,
    /// Worktree root: `<data_local>/leftos/rustling-tulip/data/worktrees/` on
    /// Windows (`%LOCALAPPDATA%\…`), equivalent on Linux/macOS. All session
    /// worktrees live under this base so the daemon doesn't need write access
    /// next to source repos; a single repo's worktree is
    /// `<worktrees_dir>/wt.<branch-slug>/<repo-slug>` and a workspace
    /// member's `<worktrees_dir>/wt.<branch-slug>/<workspace-slug>/<offset>`
    /// (see `git::workspace_worktree_paths`).
    pub worktrees_dir: PathBuf,
    /// Cached-binary root: `<data_local>/leftos/rustling-tulip/data/binaries/`
    /// on Windows. Holds content-addressed copies of `rustling-tulipd.exe`
    /// and `rt-tracer.exe` so a rebuild or reinstall can replace the shipped
    /// templates without colliding with running processes (Windows refuses to
    /// overwrite a running `.exe`). Each cached file is named
    /// `<template-stem>-<sha256-prefix>.exe`. See [`crate::binary_cache`].
    pub binaries_dir: PathBuf,
    /// Directory holding the shipped `rt-tracer` template, from
    /// `RUSTLING_TULIP_BIN_TEMPLATES` as `daemon-client` or the `--detach`
    /// launcher sets it for the daemon it starts. `None` falls back to the
    /// sibling-of-exe lookup (see `tracer_client::locate_tracer_template`). An
    /// elevated daemon reads it once, when it pins its tracer at start
    /// (`tracer_client::pin_tracer_at_start`), and never again.
    pub bin_templates: Option<PathBuf>,
}

/// Strip the Windows verbatim (`\\?\`) prefix from a canonicalized path when
/// the path can be safely represented in normal form. `std::fs::canonicalize`
/// on Windows always returns the verbatim form, which is correct for the
/// filesystem but bleeds into anything that uses the path as an *identifier* —
/// most notably an agent CLI that encodes the cwd into a per-project memory
/// key under its own config dir. A verbatim cwd produces a different key
/// from what the user gets running the agent by hand,
/// so memories, MCP state, and session history don't carry over.
///
/// Conversions:
///   * `\\?\C:\foo` → `C:\foo`
///   * `\\?\UNC\server\share\foo` → `\\server\share\foo`
///   * `\\?\Volume{…}\…` → unchanged (no normal-form equivalent)
///
/// No-op on non-Windows targets.
#[must_use]
pub fn simplify_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let s = path.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = s.strip_prefix(r"\\?\")
            && rest.chars().nth(1) == Some(':')
        {
            return PathBuf::from(rest);
        }
    }
    path.to_path_buf()
}

/// Normalize a path for comparison: strip trailing separators, and on Windows
/// also lowercase and unify separators. Used wherever a stored path is matched
/// against a freshly-computed one — session worktree paths, repo paths read
/// back out of a worktree's gitfile, and `git worktree list` output, which
/// prints forward slashes even on Windows where everything else uses
/// backslashes.
///
/// Separator unification is Windows-only on purpose: a backslash is a legal
/// character in a POSIX filename, so collapsing it there would conflate
/// genuinely different paths.
#[must_use]
pub fn normalize_path_key(p: &str) -> String {
    let trimmed = p.trim_end_matches(['/', '\\']);
    if cfg!(windows) {
        trimmed.to_lowercase().replace('\\', "/")
    } else {
        trimmed.to_string()
    }
}

/// Validate that `raw` names an existing directory and return it in the
/// normalized form session records store paths in, so later cross-references
/// (worktrees-root scan, in-use checks) match on it.
///
/// Used for caller-supplied worktree pins. A pin whose directory is gone is a
/// hard error rather than something to recreate: pins replay out of persisted
/// spawn configs, and quietly resurrecting a worktree the user deleted would be
/// worse than reporting it missing.
pub fn resolve_existing_dir(raw: &str) -> anyhow::Result<PathBuf> {
    use anyhow::{Context as _, anyhow};

    let path = PathBuf::from(raw);
    let metadata = std::fs::metadata(&path)
        .with_context(|| format!("no longer readable at {}", path.display()))?;
    if !metadata.is_dir() {
        return Err(anyhow!("not a directory: {}", path.display()));
    }
    let canonical = std::fs::canonicalize(&path).unwrap_or(path);
    Ok(simplify_path(&canonical))
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests assert preconditions with expect")]
#[cfg_attr(
    not(windows),
    expect(
        clippy::items_after_test_module,
        reason = "the non-Windows variants of the items below follow the test module"
    )
)]
mod dir_tests {
    use super::*;

    #[test]
    fn resolve_existing_dir_rejects_a_missing_path() {
        let missing = std::env::temp_dir().join("rt-pin-does-not-exist-1a2b3c");
        let err = resolve_existing_dir(&missing.to_string_lossy())
            .expect_err("a missing directory must not resolve");
        assert!(
            format!("{err:#}").contains("no longer readable"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn resolve_existing_dir_rejects_a_file() {
        let file = std::env::temp_dir().join("rt-pin-is-a-file-1a2b3c");
        std::fs::write(&file, b"").expect("writing the fixture file");
        let err = resolve_existing_dir(&file.to_string_lossy())
            .expect_err("a file must not resolve as a worktree");
        assert!(
            format!("{err:#}").contains("not a directory"),
            "unexpected error: {err:#}"
        );
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn resolve_existing_dir_normalizes_a_real_directory() {
        let dir = std::env::temp_dir();
        let out = resolve_existing_dir(&dir.to_string_lossy()).expect("temp dir must resolve");
        assert!(out.is_dir());
        assert!(
            !out.to_string_lossy().starts_with(r"\\?\"),
            "verbatim prefix should have been simplified: {}",
            out.display()
        );
    }

    #[test]
    fn summarizer_dir_is_created() {
        let data_local =
            std::env::temp_dir().join(format!("rt-summarizer-data-local-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data_local);
        let wanted = data_local.join("summarizer");
        let dir = ensure_summarizer_dir(wanted.clone()).expect("create the summarizer dir");
        assert_eq!(dir, wanted);
        assert!(dir.is_dir(), "{}", dir.display());
        let again = ensure_summarizer_dir(wanted).expect("an existing dir resolves");
        assert_eq!(again, dir);
        let _ = std::fs::remove_dir_all(&data_local);
    }

    #[test]
    fn elevated_profile_has_no_accounts_dir() {
        let accounts = PathBuf::from(r"C:\scratch\accounts");
        assert_eq!(accounts_dir_of(Profile::Elevated, accounts.clone()), None);
        assert_eq!(
            accounts_dir_of(Profile::Ordinary, accounts.clone()),
            Some(accounts)
        );
    }

    #[test]
    fn profile_rule_agrees_with_integrity_elevation() {
        use crate::integrity::IntegrityLevel;

        for rid in [0x1000, 0x2000, 0x2100, 0x3000, 0x4000] {
            assert_eq!(
                profile::profile_for_rid(rid) == Profile::Elevated,
                IntegrityLevel::from_rid(rid).is_elevated(),
                "{rid:#x}"
            );
        }
    }

    #[test]
    fn summarizer_spend_file_is_in_the_config_dir() {
        let root = PathBuf::from(r"C:\scratch\config");
        let dirs = Dirs {
            config: root.clone(),
            state_file: root.join("state.json"),
            handshake_file: root.join("daemon.json"),
            remote_config_file: root.join("remote.json"),
            sessions_dir: root.join("sessions"),
            worktrees_dir: root.join("worktrees"),
            binaries_dir: root.join("binaries"),
            bin_templates: None,
        };
        assert_eq!(
            summarizer_spend_file(&dirs),
            root.join("summarizer-spend.json")
        );
    }
}

#[cfg(all(test, windows))]
#[expect(
    clippy::expect_used,
    reason = "tests assert the resolved override is present"
)]
mod tests {
    use super::*;

    #[test]
    fn strips_drive_verbatim_prefix() {
        let out = simplify_path(Path::new(r"\\?\C:\Users\foo\repo"));
        assert_eq!(out, PathBuf::from(r"C:\Users\foo\repo"));
    }

    #[test]
    fn converts_unc_verbatim_to_normal() {
        let out = simplify_path(Path::new(r"\\?\UNC\server\share\dir"));
        assert_eq!(out, PathBuf::from(r"\\server\share\dir"));
    }

    #[test]
    fn leaves_normal_paths_unchanged() {
        let out = simplify_path(Path::new(r"C:\Users\foo\repo"));
        assert_eq!(out, PathBuf::from(r"C:\Users\foo\repo"));
    }

    #[test]
    fn leaves_volume_guid_alone() {
        let p = r"\\?\Volume{12345678-1234-1234-1234-123456789012}\dir";
        assert_eq!(simplify_path(Path::new(p)), PathBuf::from(p));
    }

    #[test]
    fn dispatch_dir_env_override_wins() {
        let root = if cfg!(windows) { r"C:\logs" } else { "/logs" };
        assert_eq!(
            dispatch_live_dir_from(Some(root.into()), [Some(PathBuf::from("plugin"))]),
            Some(PathBuf::from(root))
        );
        let relative = dispatch_live_dir_from(Some("logs".into()), [Some(PathBuf::from("plugin"))])
            .expect("a relative override is made absolute");
        assert!(relative.is_absolute());
        assert!(relative.ends_with("logs"));
    }

    #[test]
    fn dispatch_dir_empty_var_counts_as_unset() {
        assert_eq!(
            dispatch_live_dir_from(Some(OsString::new()), [Some(PathBuf::from("plugin"))]),
            Some(PathBuf::from("plugin"))
        );
    }

    #[test]
    fn dispatch_dir_falls_back_to_the_first_plugin_folder() {
        assert_eq!(
            dispatch_live_dir_from(
                None,
                [None, Some(PathBuf::from("a")), Some(PathBuf::from("b"))]
            ),
            Some(PathBuf::from("a"))
        );
    }

    #[test]
    fn dispatch_dir_is_none_without_either() {
        assert_eq!(dispatch_live_dir_from(None, [None]), None);
    }
}

impl Dirs {
    /// Resolve this daemon's folders by its profile and create them. An
    /// elevated daemon first creates its protected root, and the config and
    /// binaries overrides when set, holds each open so it cannot be renamed
    /// aside, and checks each through the handle that holds it
    /// ([`daemon_client::profile::ensure_protected`]); only then are the
    /// subfolders created.
    ///
    /// # Errors
    ///
    /// A folder could not be resolved or created, or an elevated daemon's
    /// protected root (or an override it checks) is refused.
    pub fn ensure() -> anyhow::Result<Self> {
        let (profile, roots) = profile::roots()?;
        if profile == Profile::Elevated {
            let overrides = Overrides::from_env()?;
            profile::ensure_protected(&roots, &overrides)?;
        }

        let config = roots.config;
        std::fs::create_dir_all(&config).context("creating config dir")?;

        let sessions_dir = config.join("sessions");
        std::fs::create_dir_all(&sessions_dir).context("creating sessions dir")?;

        // Worktrees are machine-local rather than in the roaming config dir:
        // they can be large and shouldn't sync. An override lets the e2e
        // harness keep them inside its per-run tmpdir.
        let worktrees_dir = roots.worktrees;
        std::fs::create_dir_all(&worktrees_dir).context("creating worktrees dir")?;

        // The content-addressed binary cache, machine-local beside the
        // worktrees. `crates/daemon-client` resolves the ordinary path
        // independently so the daemon and its clients share one cache.
        let binaries_dir = roots.binaries;
        std::fs::create_dir_all(&binaries_dir).context("creating binaries dir")?;

        Ok(Self {
            state_file: config.join("state.json"),
            handshake_file: config.join("daemon.json"),
            remote_config_file: config.join("remote.json"),
            sessions_dir,
            worktrees_dir,
            binaries_dir,
            bin_templates: resolve_bin_templates_dir(),
            config,
        })
    }

    /// Ended-session history: `<config>/history/<session-id>.json`, one entry
    /// per session (see [`crate::history`]). Created on first write.
    #[must_use]
    pub fn history_dir(&self) -> PathBuf {
        self.config.join("history")
    }

    /// Contested conversation ids: `<config>/contested/<agent id>.json`, one
    /// store per agent (see [`crate::conversations::contested`]). Created on
    /// first write.
    #[must_use]
    pub fn contested_dir(&self) -> PathBuf {
        self.config.join("contested")
    }

    /// Status-change log: `<config>/activity/<key>.jsonl`, one file per
    /// session and the daemon's own `daemon.json` beside them (see
    /// [`crate::activity_log`]). Created on first write.
    #[must_use]
    pub fn activity_dir(&self) -> PathBuf {
        self.config.join("activity")
    }

    /// The tailnet listener's certificate dir (see [`resolve_tailnet_dir`]),
    /// created if missing.
    pub fn ensure_tailnet_dir() -> anyhow::Result<PathBuf> {
        let dir = resolve_tailnet_dir()?;
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating tailnet dir {}", dir.display()))?;
        Ok(dir)
    }
}

/// The device-token store: `<config>/devices.json`, beside `remote.json`. It
/// holds one record per approved tailnet device, each with the hex SHA-256 of
/// that device's token and never the token itself (see [`crate::devices`]).
#[must_use]
pub fn devices_file(dirs: &Dirs) -> PathBuf {
    dirs.config.join("devices.json")
}

/// This daemon's stable id file, a bare UUID with no newline (see
/// [`crate::daemon_id`]), from the profile
/// ([`daemon_client::profile::roots_for`]): `<data_local>/daemon-id` for an
/// ordinary daemon, machine-local, so two machines sharing a roaming profile
/// never report one id; `<elevated root>/daemon-id` for an elevated one. While
/// `RUSTLING_TULIP_CONFIG_DIR` overrides the config dir it is
/// `<config>/daemon-id`, so an isolated daemon keeps its id in its own tree.
///
/// # Errors
///
/// The profile or the app's folders could not be resolved.
pub fn resolve_daemon_id_file(_dirs: &Dirs) -> anyhow::Result<PathBuf> {
    Ok(profile::roots()?.1.daemon_id)
}

/// The summarizer's spend ledger: `<config>/summarizer-spend.json`, the
/// current local day's billed spend (see [`crate::summarizer::ledger`]).
#[must_use]
pub fn summarizer_spend_file(dirs: &Dirs) -> PathBuf {
    dirs.config.join("summarizer-spend.json")
}

/// The Dashboard's day ledger: `<config>/day-ledger.json`, the tokens, cost
/// and commits of each local day (see [`crate::dashboard::ledger`]).
#[must_use]
pub fn day_ledger_file(dirs: &Dirs) -> PathBuf {
    dirs.config.join("day-ledger.json")
}

/// A session's summary ring and latest summary:
/// `<sessions>/<id>/summary.json`, beside its `meta.json` sidecar.
#[must_use]
pub fn session_summary_file(dirs: &Dirs, session_id: &str) -> PathBuf {
    dirs.sessions_dir.join(session_id).join("summary.json")
}

/// Resolve the tailnet listener's certificate dir by the profile, honoring
/// `RUSTLING_TULIP_TAILNET_DIR` when set (used by e2e isolation): an ordinary
/// daemon's is `<data_local>/leftos/rustling-tulip/data/tailnet/`, so the
/// private key `tailscale cert` writes stays machine-local and never roams; an
/// elevated daemon's is under its protected root.
pub fn resolve_tailnet_dir() -> anyhow::Result<PathBuf> {
    Ok(profile::roots()?.1.tailnet)
}

/// Resolve the push folder by the profile, honoring `RUSTLING_TULIP_PUSH_DIR`
/// when set (used by e2e isolation): an ordinary daemon's is
/// `<data_local>/leftos/rustling-tulip/data/push/`, so the VAPID key pair and
/// the device subscriptions stay machine-local and never roam; an elevated
/// daemon's is under its protected root. The folder is created on the first
/// push request ([`crate::push::ensure_folder`]).
pub fn resolve_push_dir() -> anyhow::Result<PathBuf> {
    Ok(profile::roots()?.1.push)
}

/// The dispatch tool's live-run folder: `RUSTLING_TULIP_DISPATCH_DIR` when set
/// and not empty (made absolute against the current dir), else the first folder
/// a registered agent names, else none, and the daemon follows no dispatch
/// runs. An override that cannot be made absolute answers none rather than
/// another folder, so an isolated run never reads the owner's real runs.
/// Nothing is created here.
#[expect(dead_code, reason = "read by the dispatch-run watcher (DF.5)")]
#[must_use]
pub fn resolve_dispatch_live_dir() -> Option<PathBuf> {
    dispatch_live_dir_from(
        std::env::var_os(daemon_client::isolation::DISPATCH_DIR_VAR),
        agent_registry::all()
            .iter()
            .map(|plugin| plugin.live_runs_dir()),
    )
}

/// [`resolve_dispatch_live_dir`] over an explicit override `var`, taking the
/// first folder the registered agents name when the override is unset or
/// empty. A set override that cannot be made absolute answers none.
fn dispatch_live_dir_from(
    var: Option<OsString>,
    plugin_dirs: impl IntoIterator<Item = Option<PathBuf>>,
) -> Option<PathBuf> {
    match var.filter(|value| !value.is_empty()) {
        Some(dir) => match std::path::absolute(Path::new(&dir)) {
            Ok(dir) => Some(dir),
            Err(err) => {
                warn!(%err, "dispatch dir override could not be made absolute; following no dispatch runs");
                None
            }
        },
        None => plugin_dirs.into_iter().flatten().next(),
    }
}

/// The summarizer's working folder by the profile, created if missing: an
/// ordinary daemon's is `<data_local>/summarizer`
/// (`%LOCALAPPDATA%\leftos\rustling-tulip\data\summarizer\` on Windows), an
/// elevated daemon's is under its protected root. An agent's summarizer
/// program runs in it, so no project's settings or instruction files apply to
/// a summary.
pub(crate) fn resolve_summarizer_dir() -> anyhow::Result<PathBuf> {
    ensure_summarizer_dir(profile::roots()?.1.summarizer)
}

/// The root of the daemon-driven account logins' scratch folders by the
/// profile ([`profile::ProfileRoots::account_login`]): an ordinary daemon's
/// is `%LOCALAPPDATA%\leftos\rustling-tulip\data\account-login\` on Windows,
/// an elevated daemon's is under its protected root, and under a
/// `RUSTLING_TULIP_CONFIG_DIR` override it is `<config dir>/account-login`,
/// so an isolated daemon keeps its logins in its own tree. Never under an
/// agent's own config home. Nothing is created here; a login run creates
/// the root, and its own folder in it, when it starts.
pub(crate) fn resolve_account_login_dir() -> anyhow::Result<PathBuf> {
    Ok(profile::roots()?.1.account_login)
}

/// The account store's root by the profile ([`profile::ProfileRoots::accounts`]),
/// honoring `RUSTLING_TULIP_ACCOUNTS_DIR` when set: an ordinary daemon's is
/// `<data_local>/accounts`, or `<config>/accounts` while
/// `RUSTLING_TULIP_CONFIG_DIR` overrides the config dir. `None` for an
/// elevated daemon: the store belongs to the ordinary user's login, and an
/// elevated daemon never opens it. Nothing is created here; the store creates
/// its root owner-only when it first opens it.
pub(crate) fn resolve_accounts_dir() -> anyhow::Result<Option<PathBuf>> {
    let (profile, roots) = profile::roots()?;
    Ok(accounts_dir_of(profile, roots.accounts))
}

/// `accounts` for an ordinary profile, `None` for an elevated one.
pub(crate) fn accounts_dir_of(profile: Profile, accounts: PathBuf) -> Option<PathBuf> {
    match profile {
        Profile::Ordinary => Some(accounts),
        Profile::Elevated => None,
    }
}

/// `dir`, created if missing.
fn ensure_summarizer_dir(dir: PathBuf) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating the summarizer dir {}", dir.display()))?;
    Ok(dir)
}

/// Resolve the config directory by the profile, honoring
/// `RUSTLING_TULIP_CONFIG_DIR` when set, made absolute against the current
/// directory. The override lets the e2e harness point at a per-run tmpdir so
/// test runs never write to the user's real `%APPDATA%`. When unset, an
/// ordinary daemon's is `ProjectDirs::from("dev", "leftos",
/// "rustling-tulip").config_dir()` and an elevated daemon's is under its
/// protected root.
///
/// Callers that need the subdirectories too want [`Dirs::ensure`] instead.
pub(crate) fn config_dir() -> anyhow::Result<PathBuf> {
    Ok(profile::roots()?.1.config)
}

/// The template directory `RUSTLING_TULIP_BIN_TEMPLATES` names, as
/// `daemon-client` or the `--detach` launcher sets it for the daemon it starts. `None`
/// when the variable is unset or empty — the tracer lookup then falls back to
/// the sibling of the daemon's exe. An elevated daemon looks the tracer up
/// only once, when it pins it at start.
pub(crate) fn resolve_bin_templates_dir() -> Option<PathBuf> {
    std::env::var_os("RUSTLING_TULIP_BIN_TEMPLATES")
        .filter(|value| !value.as_os_str().is_empty())
        .map(PathBuf::from)
}
