//! Persisted daemon state: registries of repos and workspaces.
//!
//! Sessions are *not* persisted across daemon restarts in Phase 1 — they're owned
//! by the daemon process. Phase 5 will add orphan recovery.

use crate::paths::Dirs;
use anyhow::Context as _;
use protocol::{RepoEntry, TabEntry, WorkspaceEntry};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PersistedState {
    pub repos: Vec<RepoEntry>,
    pub workspaces: Vec<WorkspaceEntry>,
    /// Tab layouts shared by all connected clients. Vec order is display
    /// order. Older state.json files without this field deserialize cleanly.
    #[serde(default)]
    pub tabs: Vec<TabEntry>,
}

#[derive(Debug)]
pub struct AppState {
    pub dirs: Dirs,
    inner: Mutex<PersistedState>,
}

impl AppState {
    pub fn load_or_default(dirs: &Dirs) -> anyhow::Result<Self> {
        let inner = if dirs.state_file.exists() {
            let bytes = std::fs::read(&dirs.state_file).context("reading state.json")?;
            serde_json::from_slice::<PersistedState>(&bytes).unwrap_or_else(|err| {
                tracing::warn!(?err, "state.json corrupt, starting fresh");
                PersistedState::default()
            })
        } else {
            PersistedState::default()
        };
        Ok(Self {
            dirs: dirs.clone(),
            inner: Mutex::new(inner),
        })
    }

    pub fn with_persisted<R>(&self, f: impl FnOnce(&PersistedState) -> R) -> R {
        let guard = crate::sync::lock(&self.inner);
        f(&guard)
    }

    pub fn mutate<R>(&self, f: impl FnOnce(&mut PersistedState) -> R) -> anyhow::Result<R> {
        let mut guard = crate::sync::lock(&self.inner);
        let result = f(&mut guard);
        let bytes = serde_json::to_vec_pretty(&*guard).context("serializing state")?;
        let tmp = self.dirs.state_file.with_extension("json.tmp");
        std::fs::write(&tmp, &bytes).context("writing state tmp")?;
        std::fs::rename(&tmp, &self.dirs.state_file).context("renaming state.json")?;
        Ok(result)
    }
}
