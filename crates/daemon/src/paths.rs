//! Configuration / state directory layout.

use anyhow::{Context as _, anyhow};
use directories::ProjectDirs;
use std::ffi::OsStr;
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
    fn tailnet_dir_is_data_local_and_honours_the_override() {
        let data_local = std::env::temp_dir().join("rt-data-local");
        let override_dir = std::env::temp_dir().join("rt-e2e-tailnet");
        let from_data_local = || Ok(data_local.clone());
        assert_eq!(
            tailnet_dir_from(None, from_data_local).expect("resolve without override"),
            data_local.join("tailnet")
        );
        assert_eq!(
            tailnet_dir_from(Some(OsStr::new("")), from_data_local)
                .expect("resolve empty override"),
            data_local.join("tailnet"),
            "an empty override falls back to the data-local dir"
        );
        assert_eq!(
            tailnet_dir_from(Some(override_dir.as_os_str()), from_data_local)
                .expect("resolve override"),
            override_dir
        );
    }

    #[test]
    fn summarizer_dir_is_data_local_summarizer_and_is_created() {
        let data_local =
            std::env::temp_dir().join(format!("rt-summarizer-data-local-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data_local);
        let dir = ensure_summarizer_dir_in(&data_local).expect("create the summarizer dir");
        assert_eq!(dir, data_local.join("summarizer"));
        assert!(dir.is_dir(), "{}", dir.display());
        let again = ensure_summarizer_dir_in(&data_local).expect("an existing dir resolves");
        assert_eq!(again, dir);
        let _ = std::fs::remove_dir_all(&data_local);
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

    #[test]
    #[expect(clippy::expect_used, reason = "a failed resolve fails the test loudly")]
    fn relative_config_dir_override_is_made_absolute() {
        let out = config_dir_override(r"rel\config").expect("resolve override");
        assert!(out.is_absolute(), "{out:?}");
        assert_eq!(
            out,
            std::env::current_dir()
                .expect("current dir")
                .join(r"rel\config")
        );
        let absolute = config_dir_override(r"C:\abs\config").expect("resolve override");
        assert_eq!(absolute, PathBuf::from(r"C:\abs\config"));
    }

    #[test]
    fn leaves_volume_guid_alone() {
        let p = r"\\?\Volume{12345678-1234-1234-1234-123456789012}\dir";
        assert_eq!(simplify_path(Path::new(p)), PathBuf::from(p));
    }
}

impl Dirs {
    pub fn ensure() -> anyhow::Result<Self> {
        let config = config_dir()?;
        std::fs::create_dir_all(&config).context("creating config dir")?;

        let sessions_dir = config.join("sessions");
        std::fs::create_dir_all(&sessions_dir).context("creating sessions dir")?;

        let worktrees_dir = resolve_worktrees_dir()?;
        std::fs::create_dir_all(&worktrees_dir).context("creating worktrees dir")?;

        let binaries_dir = resolve_binaries_dir()?;
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

/// This daemon's stable id: `<config>/daemon-id`, a bare UUID with no newline
/// (see [`crate::daemon_id`]).
#[must_use]
pub fn daemon_id_file(dirs: &Dirs) -> PathBuf {
    dirs.config.join("daemon-id")
}

/// The summarizer's spend ledger: `<config>/summarizer-spend.json`, the
/// current local day's billed spend (see [`crate::summarizer::ledger`]).
#[must_use]
pub fn summarizer_spend_file(dirs: &Dirs) -> PathBuf {
    dirs.config.join("summarizer-spend.json")
}

/// A session's summary ring and latest summary:
/// `<sessions>/<id>/summary.json`, beside its `meta.json` sidecar.
#[must_use]
pub fn session_summary_file(dirs: &Dirs, session_id: &str) -> PathBuf {
    dirs.sessions_dir.join(session_id).join("summary.json")
}

/// Resolve the tailnet listener's certificate dir, honoring
/// `RUSTLING_TULIP_TAILNET_DIR` when set (used by e2e isolation). Falls back
/// to `<data_local>/leftos/rustling-tulip/data/tailnet/`: the private key
/// `tailscale cert` writes stays machine-local and never roams.
pub fn resolve_tailnet_dir() -> anyhow::Result<PathBuf> {
    let override_dir = std::env::var_os("RUSTLING_TULIP_TAILNET_DIR");
    tailnet_dir_from(override_dir.as_deref(), || {
        let pd = ProjectDirs::from("dev", "leftos", "rustling-tulip")
            .ok_or_else(|| anyhow!("could not resolve tailnet directory"))?;
        Ok(pd.data_local_dir().to_path_buf())
    })
}

/// The tailnet dir an override value names, else `tailnet` under the
/// data-local dir. An empty override counts as unset.
fn tailnet_dir_from(
    override_dir: Option<&OsStr>,
    data_local: impl FnOnce() -> anyhow::Result<PathBuf>,
) -> anyhow::Result<PathBuf> {
    match override_dir {
        Some(dir) if !dir.is_empty() => Ok(PathBuf::from(dir)),
        _ => Ok(data_local()?.join("tailnet")),
    }
}

/// The summarizer's working folder, `<data_local>/summarizer`
/// (`%LOCALAPPDATA%\leftos\rustling-tulip\data\summarizer\` on Windows),
/// created if missing. An agent's summarizer program runs in it, so no
/// project's settings or instruction files apply to a summary.
pub(crate) fn resolve_summarizer_dir() -> anyhow::Result<PathBuf> {
    let pd = ProjectDirs::from("dev", "leftos", "rustling-tulip")
        .ok_or_else(|| anyhow!("could not resolve the summarizer directory"))?;
    ensure_summarizer_dir_in(pd.data_local_dir())
}

/// `summarizer` under `data_local`, created if missing.
fn ensure_summarizer_dir_in(data_local: &Path) -> anyhow::Result<PathBuf> {
    let dir = data_local.join("summarizer");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating the summarizer dir {}", dir.display()))?;
    Ok(dir)
}

/// Resolve the config directory, honoring `RUSTLING_TULIP_CONFIG_DIR` when
/// set. The override lets the e2e harness point at a per-run tmpdir so test
/// runs never write to the user's real `%APPDATA%`. When unset (the
/// production path), falls back to `ProjectDirs::from("dev", "leftos",
/// "rustling-tulip").config_dir()`.
///
/// Callers that need the subdirectories too want [`Dirs::ensure`] instead.
pub(crate) fn config_dir() -> anyhow::Result<PathBuf> {
    if let Ok(value) = std::env::var("RUSTLING_TULIP_CONFIG_DIR")
        && !value.is_empty()
    {
        return config_dir_override(&value);
    }
    let pd = ProjectDirs::from("dev", "leftos", "rustling-tulip")
        .ok_or_else(|| anyhow!("could not resolve config directory"))?;
    Ok(pd.config_dir().to_path_buf())
}

/// The config dir a `RUSTLING_TULIP_CONFIG_DIR` value names, made absolute
/// against the current directory: tracers carry the config dir as their
/// owner, and a relative one would name a different dir from their cwd.
fn config_dir_override(value: &str) -> anyhow::Result<PathBuf> {
    std::path::absolute(value)
        .with_context(|| format!("resolving RUSTLING_TULIP_CONFIG_DIR={value} to an absolute path"))
}

/// Resolve the worktrees root, honoring `RUSTLING_TULIP_WORKTREES_DIR` when
/// set. The override lets the e2e harness keep worktrees inside its per-run
/// tmpdir. When unset, falls back to `ProjectDirs::data_local_dir()` joined
/// with `worktrees` — `%LOCALAPPDATA%\leftos\rustling-tulip\data\worktrees\`
/// on Windows; equivalent under `~/.local/share/...` on Linux. Worktrees are
/// deliberately under `data_local_dir` (machine-local) rather than the
/// roaming `config_dir` — they can be large and shouldn't sync.
fn resolve_worktrees_dir() -> anyhow::Result<PathBuf> {
    if let Ok(value) = std::env::var("RUSTLING_TULIP_WORKTREES_DIR")
        && !value.is_empty()
    {
        return Ok(PathBuf::from(value));
    }
    let pd = ProjectDirs::from("dev", "leftos", "rustling-tulip")
        .ok_or_else(|| anyhow!("could not resolve worktrees directory"))?;
    Ok(pd.data_local_dir().join("worktrees"))
}

/// Resolve the cached-binary root, honoring `RUSTLING_TULIP_BINARIES_DIR` when
/// set (used by e2e isolation). Falls back to
/// `<data_local>/leftos/rustling-tulip/data/binaries/` — same root as
/// worktrees, machine-local and known-writable. `crates/daemon-client`
/// resolves the same path independently so the daemon and its clients share
/// one cache.
pub fn resolve_binaries_dir() -> anyhow::Result<PathBuf> {
    if let Ok(value) = std::env::var("RUSTLING_TULIP_BINARIES_DIR")
        && !value.is_empty()
    {
        return Ok(PathBuf::from(value));
    }
    let pd = ProjectDirs::from("dev", "leftos", "rustling-tulip")
        .ok_or_else(|| anyhow!("could not resolve binaries directory"))?;
    Ok(pd.data_local_dir().join("binaries"))
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
