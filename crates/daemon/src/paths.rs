//! Configuration / state directory layout.

use anyhow::{Context as _, anyhow};
use directories::ProjectDirs;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Dirs {
    pub config: PathBuf,
    pub state_file: PathBuf,
    pub handshake_file: PathBuf,
    /// Per-session sidecar directory: `<config>/sessions/<session-id>/` holds
    /// `meta.json` (orphan recovery) and `scrollback.bin` (replay on attach).
    pub sessions_dir: PathBuf,
}

impl Dirs {
    pub fn ensure() -> anyhow::Result<Self> {
        let pd = ProjectDirs::from("dev", "leftos", "rustling-tulip")
            .ok_or_else(|| anyhow!("could not resolve config directory"))?;

        let config = pd.config_dir().to_path_buf();
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
