//! Configuration / state directory layout.

use anyhow::{Context as _, anyhow};
use directories::ProjectDirs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Dirs {
    pub config: PathBuf,
    pub state_file: PathBuf,
    pub handshake_file: PathBuf,
    /// Per-session sidecar directory: `<config>/sessions/<session-id>/` holds
    /// `meta.json` (orphan recovery) and `scrollback.bin` (replay on attach).
    pub sessions_dir: PathBuf,
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

        Ok(Self {
            state_file: config.join("state.json"),
            handshake_file: config.join("daemon.json"),
            sessions_dir,
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
