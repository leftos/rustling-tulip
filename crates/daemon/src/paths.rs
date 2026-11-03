//! Configuration / state directory layout.

use anyhow::Context as _;
use daemon_client::profile::{self, Overrides, Profile, ProfileRoots};
use std::path::{Path, PathBuf};

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
    /// launcher sets it for the daemon it starts. `None` falls back to the sibling-of-exe and
    /// installed-app lookups (see `tracer_client::locate_tracer_template`).
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

    /// A fresh folder path for one test, cleared of a previous run's copy.
    fn scratch(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("rt-profile-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        path
    }

    #[test]
    #[expect(clippy::expect_used, reason = "a failed setup fails the test loudly")]
    fn an_elevated_dirs_check_refuses_a_preexisting_default_root() {
        let data_local = scratch("default-root");
        let roots = profile::roots_for(
            Profile::Elevated,
            &Overrides::default(),
            &data_local.join("config"),
            &data_local,
        );
        let root = roots.root.clone().expect("an elevated profile has a root");
        std::fs::create_dir_all(&root).expect("create an ordinary folder at the root");
        let err = guard_elevated(&roots, &Overrides::default())
            .expect_err("an ordinary folder is no protected root");
        let text = format!("{err:#}");
        assert!(text.contains("is unsafe"), "unexpected refusal: {text}");
        let _ = std::fs::remove_dir_all(&data_local);
    }

    #[test]
    #[expect(clippy::expect_used, reason = "a failed setup fails the test loudly")]
    fn the_elevated_root_cannot_be_renamed_while_held() {
        let dir = scratch("held");
        let aside = scratch("held-aside");
        std::fs::create_dir_all(&dir).expect("create the folder");
        let held = crate::protected_root::open_held(&dir).expect("hold the folder");
        let renamed = std::fs::rename(&dir, &aside);
        drop(held);
        assert!(
            renamed.is_err(),
            "a held folder was renamed to {}",
            aside.display()
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&aside);
    }

    /// The guard holds every folder it checks before it checks any, so a
    /// refused root still leaves the config override held.
    #[test]
    #[expect(clippy::expect_used, reason = "a failed setup fails the test loudly")]
    fn a_checked_override_is_held() {
        let data_local = scratch("held-override");
        let config = scratch("held-override-config");
        let aside = scratch("held-override-aside");
        let overrides = Overrides {
            config: Some(config.clone()),
            ..Overrides::default()
        };
        let roots = profile::roots_for(
            Profile::Elevated,
            &overrides,
            &data_local.join("config"),
            &data_local,
        );
        let root = roots.root.clone().expect("an elevated profile has a root");
        std::fs::create_dir_all(&root).expect("create the root");
        std::fs::create_dir_all(&config).expect("create the config override");
        let _ = guard_elevated(&roots, &overrides);
        assert!(
            std::fs::rename(&config, &aside).is_err(),
            "the checked config override was renamed to {}",
            aside.display()
        );
    }

    /// The refusal comes from a check on the handle the guard holds: the
    /// refused folder is still held afterwards.
    #[test]
    #[expect(clippy::expect_used, reason = "a failed setup fails the test loudly")]
    fn the_check_runs_on_the_held_handle() {
        let data_local = scratch("held-check");
        let aside = scratch("held-check-aside");
        let roots = profile::roots_for(
            Profile::Elevated,
            &Overrides::default(),
            &data_local.join("config"),
            &data_local,
        );
        let root = roots.root.clone().expect("an elevated profile has a root");
        std::fs::create_dir_all(&root).expect("create an ordinary folder at the root");
        let err = guard_elevated(&roots, &Overrides::default())
            .expect_err("an ordinary folder is no protected root");
        let text = format!("{err:#}");
        assert!(text.contains("is unsafe"), "unexpected refusal: {text}");
        assert!(
            std::fs::rename(&root, &aside).is_err(),
            "the refused root was not held: renamed to {}",
            aside.display()
        );
    }

    #[test]
    fn leaves_volume_guid_alone() {
        let p = r"\\?\Volume{12345678-1234-1234-1234-123456789012}\dir";
        assert_eq!(simplify_path(Path::new(p)), PathBuf::from(p));
    }
}

impl Dirs {
    /// Resolve this daemon's folders by its profile and create them. An
    /// elevated daemon first creates its protected root, and the config and
    /// binaries overrides when set, holds each open so it cannot be renamed
    /// aside, and checks each through the handle that holds it
    /// ([`guard_elevated`]); only then are the subfolders created.
    ///
    /// # Errors
    ///
    /// A folder could not be resolved or created, or an elevated daemon's
    /// protected root (or an override it checks) is refused.
    pub fn ensure() -> anyhow::Result<Self> {
        let (profile, roots) = profile::roots()?;
        if profile == Profile::Elevated {
            let overrides = Overrides::from_env()?;
            guard_elevated(&roots, &overrides)?;
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

/// The folders an elevated daemon protects: its root, and the config and
/// binaries overrides when they are set, since an ordinary process could have
/// made any of them first.
fn protected_folders<'a>(roots: &'a ProfileRoots, overrides: &Overrides) -> Vec<&'a Path> {
    let mut folders = Vec::with_capacity(3);
    if let Some(root) = &roots.root {
        folders.push(root.as_path());
    }
    if overrides.config.is_some() {
        folders.push(roots.config.as_path());
    }
    if overrides.binaries.is_some() {
        folders.push(roots.binaries.as_path());
    }
    folders
}

/// Before an elevated daemon creates any folder: for each protected folder
/// ([`protected_folders`]), create it when missing and take a hold on it that
/// refuses a rename or delete for the life of the process
/// ([`crate::protected_root::open_held`]); then check each folder through the
/// handle that holds it ([`crate::protected_root::check_handle`]), so the
/// folder checked is the folder held and none can be swapped in between. A
/// second call checks again through the handles the first one took.
///
/// # Errors
///
/// A folder could not be created or opened, or the first held folder the
/// check refuses.
#[cfg(windows)]
fn guard_elevated(roots: &ProfileRoots, overrides: &Overrides) -> anyhow::Result<()> {
    use crate::protected_root::{check_handle, create_if_missing, open_held};
    use windows::Win32::Foundation::HANDLE;

    /// Each held folder and its raw handle value, never closed.
    static HELD: std::sync::Mutex<Vec<(PathBuf, usize)>> = std::sync::Mutex::new(Vec::new());

    let folders = protected_folders(roots, overrides);
    let mut handles = Vec::with_capacity(folders.len());
    {
        let mut held = HELD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for folder in &folders {
            let known = held
                .iter()
                .find(|(path, _)| path.as_path() == *folder)
                .map(|&(_, raw)| raw);
            let raw = if let Some(raw) = known {
                raw
            } else {
                create_if_missing(folder)?;
                let raw = open_held(folder)?.into_raw().0.addr();
                held.push((folder.to_path_buf(), raw));
                raw
            };
            handles.push(raw);
        }
    }
    for (folder, raw) in folders.into_iter().zip(handles) {
        check_handle(HANDLE(std::ptr::without_provenance_mut(raw)), folder)?;
    }
    Ok(())
}

/// Before an elevated daemon creates any folder: create or check each
/// protected folder ([`protected_folders`]). No other platform lets an open
/// handle refuse a rename, so nothing is held.
///
/// # Errors
///
/// The first folder [`crate::protected_root::create_or_check`] refuses.
#[cfg(not(windows))]
fn guard_elevated(roots: &ProfileRoots, overrides: &Overrides) -> anyhow::Result<()> {
    for folder in protected_folders(roots, overrides) {
        crate::protected_root::create_or_check(folder)?;
    }
    Ok(())
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

/// The summarizer's working folder by the profile, created if missing: an
/// ordinary daemon's is `<data_local>/summarizer`
/// (`%LOCALAPPDATA%\leftos\rustling-tulip\data\summarizer\` on Windows), an
/// elevated daemon's is under its protected root. An agent's summarizer
/// program runs in it, so no project's settings or instruction files apply to
/// a summary.
pub(crate) fn resolve_summarizer_dir() -> anyhow::Result<PathBuf> {
    ensure_summarizer_dir(profile::roots()?.1.summarizer)
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
/// the sibling-of-exe and installed-app locations.
fn resolve_bin_templates_dir() -> Option<PathBuf> {
    std::env::var_os("RUSTLING_TULIP_BIN_TEMPLATES")
        .filter(|value| !value.as_os_str().is_empty())
        .map(PathBuf::from)
}
