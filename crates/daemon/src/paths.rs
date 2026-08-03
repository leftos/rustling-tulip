//! Configuration / state directory layout.

use anyhow::{Context as _, anyhow};
use directories::ProjectDirs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Dirs {
    pub config: PathBuf,
    pub state_file: PathBuf,
    pub handshake_file: PathBuf,
    /// Opt-in LAN access config: `<config>/lan.json` holds `{ enabled, port,
    /// auth_token }`. The token is persisted here (not regenerated per start)
    /// so a paired remote client's saved profile survives daemon restarts.
    /// Absent until the user first enables LAN access.
    pub lan_config_file: PathBuf,
    /// Self-signed TLS leaf certificate (PEM) for the LAN listener, generated
    /// on first enable and pinned by remote clients (TOFU). Sibling key file
    /// is `lan_key_file`.
    pub lan_cert_file: PathBuf,
    /// Private key (PEM) paired with `lan_cert_file`.
    pub lan_key_file: PathBuf,
    /// Per-session sidecar directory: `<config>/sessions/<session-id>/` holds
    /// `meta.json` (orphan recovery) and `scrollback.bin` (replay on attach).
    pub sessions_dir: PathBuf,
    /// Worktree root: `<data_local>/leftos/rustling-tulip/data/worktrees/` on
    /// Windows (`%LOCALAPPDATA%\…`), equivalent on Linux/macOS. All session
    /// worktrees live under this base so the daemon doesn't need write access
    /// next to source repos; per-session worktree paths are
    /// `<worktrees_dir>/<sanitized-anchor>/wt.<branch-slug>/<rel-to-anchor>`
    /// (see `git::workspace_worktree_paths`).
    pub worktrees_dir: PathBuf,
    /// Cached-binary root: `<data_local>/leftos/rustling-tulip/data/binaries/`
    /// on Windows. Holds content-addressed copies of `rustling-tulipd.exe`
    /// and `rt-tracer.exe` so a rebuild or reinstall can replace the shipped
    /// templates without colliding with running processes (Windows refuses to
    /// overwrite a running `.exe`). Each cached file is named
    /// `<template-stem>-<sha256-prefix>.exe`. See [`crate::binary_cache`].
    pub binaries_dir: PathBuf,
}

/// Strip the Windows verbatim (`\\?\`) prefix from a canonicalized path when
/// the path can be safely represented in normal form. `std::fs::canonicalize`
/// on Windows always returns the verbatim form, which is correct for the
/// filesystem but bleeds into anything that uses the path as an *identifier* —
/// most notably the `claude` CLI, which encodes the cwd into a per-project
/// memory key under `~/.claude/projects/<encoded-cwd>/`. A verbatim cwd
/// produces a different key from what the user gets running `claude` by hand,
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
    fn leaves_volume_guid_alone() {
        let p = r"\\?\Volume{12345678-1234-1234-1234-123456789012}\dir";
        assert_eq!(simplify_path(Path::new(p)), PathBuf::from(p));
    }
}

impl Dirs {
    pub fn ensure() -> anyhow::Result<Self> {
        let config = resolve_config_dir()?;
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
            lan_config_file: config.join("lan.json"),
            lan_cert_file: config.join("lan-cert.pem"),
            lan_key_file: config.join("lan-key.pem"),
            sessions_dir,
            worktrees_dir,
            binaries_dir,
            config,
        })
    }
}

/// Resolve the config directory, honoring `RUSTLING_TULIP_CONFIG_DIR` when
/// set. The override lets the e2e harness point at a per-run tmpdir so test
/// runs never write to the user's real `%APPDATA%`. When unset (the
/// production path), falls back to `ProjectDirs::from("dev", "leftos",
/// "rustling-tulip").config_dir()`.
fn resolve_config_dir() -> anyhow::Result<PathBuf> {
    if let Ok(value) = std::env::var("RUSTLING_TULIP_CONFIG_DIR")
        && !value.is_empty()
    {
        return Ok(PathBuf::from(value));
    }
    let pd = ProjectDirs::from("dev", "leftos", "rustling-tulip")
        .ok_or_else(|| anyhow!("could not resolve config directory"))?;
    Ok(pd.config_dir().to_path_buf())
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
/// worktrees, machine-local and known-writable. The Tauri-side daemon
/// supervisor resolves the same path independently so both processes share
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
