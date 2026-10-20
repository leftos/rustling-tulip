//! Opt-in remote (Tailscale) access: the persisted `remote.json` switch.
//!
//! Remote access is off unless the user turns it on. The file lives in the
//! config dir and holds only the switch and the port; the
//! device tokens that authorize a tailnet client are not stored here.
//!
//! An absent file, an unreadable or corrupt one, and a zero port all mean the
//! listener stays down, so a bad config can never open the daemon to the
//! tailnet.

use crate::paths::Dirs;
use serde::{Deserialize, Serialize};

/// The port the tailnet listener binds when `remote.json` names none.
pub const DEFAULT_REMOTE_PORT: u16 = 8787;

/// Persisted contents of `remote.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RemoteConfig {
    /// Whether the tailnet listener should run. Off by default.
    pub enabled: bool,
    /// The port the tailnet listener binds on every tailnet address. Zero
    /// means off, whatever `enabled` says.
    pub port: u16,
}

impl Default for RemoteConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: DEFAULT_REMOTE_PORT,
        }
    }
}

/// Load `remote.json`, falling back to the off/8787 default when the file is
/// absent, unreadable or corrupt. A zero port means off, and the effective
/// port is then [`DEFAULT_REMOTE_PORT`].
#[must_use]
pub fn load(dirs: &Dirs) -> RemoteConfig {
    if !dirs.remote_config_file.exists() {
        return RemoteConfig::default();
    }
    let bytes = match std::fs::read(&dirs.remote_config_file) {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(
                ?err,
                "could not read remote.json; ignoring (remote access disabled)"
            );
            return RemoteConfig::default();
        }
    };
    match serde_json::from_slice::<RemoteConfig>(&bytes) {
        Ok(cfg) if cfg.port == 0 => {
            tracing::warn!("remote.json names port 0; remote access disabled");
            RemoteConfig::default()
        }
        Ok(cfg) => cfg,
        Err(err) => {
            tracing::warn!(
                ?err,
                "remote.json corrupt; ignoring (remote access disabled)"
            );
            RemoteConfig::default()
        }
    }
}

/// Write `cfg` to `remote.json` through a temp file and a rename, so a reader
/// never sees a half-written switch.
///
/// # Errors
///
/// Serializing, writing the temp file or renaming it failed; `remote.json`
/// is then unchanged.
pub fn save(dirs: &Dirs, cfg: RemoteConfig) -> anyhow::Result<()> {
    use anyhow::Context as _;
    let bytes = serde_json::to_vec_pretty(&cfg).context("serializing remote.json")?;
    crate::orphan::write_atomic(&dirs.remote_config_file, &bytes).context("writing remote.json")
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests assert preconditions with expect")]
mod tests {
    use super::{DEFAULT_REMOTE_PORT, RemoteConfig, load, save};
    use crate::paths::Dirs;

    fn scratch_dirs(tag: &str) -> Dirs {
        let root = std::env::temp_dir().join(format!("rt-remote-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create scratch dir");
        Dirs {
            config: root.clone(),
            state_file: root.join("state.json"),
            handshake_file: root.join("daemon.json"),
            remote_config_file: root.join("remote.json"),
            sessions_dir: root.join("sessions"),
            worktrees_dir: root.join("worktrees"),
            binaries_dir: root.join("binaries"),
            bin_templates: None,
        }
    }

    #[test]
    fn absent_file_is_off_on_port_8787() {
        let dirs = scratch_dirs("absent");
        let cfg = load(&dirs);
        assert!(
            !cfg.enabled,
            "absent remote.json means remote access is off"
        );
        assert_eq!(cfg.port, 8787);
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn corrupt_file_is_off() {
        let dirs = scratch_dirs("corrupt");
        std::fs::write(&dirs.remote_config_file, b"{ not json").expect("write corrupt config");
        let cfg = load(&dirs);
        assert!(
            !cfg.enabled,
            "a corrupt remote.json means remote access is off"
        );
        assert_eq!(cfg.port, DEFAULT_REMOTE_PORT);
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn zero_port_is_off() {
        let dirs = scratch_dirs("zero-port");
        std::fs::write(&dirs.remote_config_file, br#"{"enabled":true,"port":0}"#)
            .expect("write zero-port config");
        let cfg = load(&dirs);
        assert!(!cfg.enabled, "a zero port means remote access is off");
        assert_eq!(cfg.port, DEFAULT_REMOTE_PORT);
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn save_then_load_round_trips() {
        let dirs = scratch_dirs("round-trip");
        for cfg in [
            RemoteConfig {
                enabled: true,
                port: 9000,
            },
            RemoteConfig {
                enabled: false,
                port: DEFAULT_REMOTE_PORT,
            },
        ] {
            save(&dirs, cfg).expect("save remote.json");
            assert_eq!(load(&dirs), cfg);
        }
        let leftovers: Vec<_> = std::fs::read_dir(&dirs.config)
            .expect("list the scratch dir")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name() != "remote.json")
            .collect();
        assert!(leftovers.is_empty(), "no temp file is left: {leftovers:?}");
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn a_partial_file_defaults_the_missing_fields() {
        let dirs = scratch_dirs("partial");
        std::fs::write(&dirs.remote_config_file, br#"{"enabled":true}"#)
            .expect("write partial config");
        let cfg = load(&dirs);
        assert!(cfg.enabled);
        assert_eq!(cfg.port, DEFAULT_REMOTE_PORT);
        let _ = std::fs::remove_dir_all(&dirs.config);
    }
}
