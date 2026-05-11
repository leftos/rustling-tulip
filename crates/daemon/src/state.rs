//! Persisted daemon state: registries of repos and workspaces.
//!
//! Sessions are *not* persisted across daemon restarts in Phase 1 — they're owned
//! by the daemon process. Phase 5 will add orphan recovery.

use crate::paths::{Dirs, simplify_path};
use anyhow::Context as _;
use protocol::{RepoEntry, TabEntry, WorkspaceEntry};
use serde::{Deserialize, Serialize};
use std::path::Path;
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
        let mut inner = if dirs.state_file.exists() {
            let bytes = std::fs::read(&dirs.state_file).context("reading state.json")?;
            serde_json::from_slice::<PersistedState>(&bytes).unwrap_or_else(|err| {
                tracing::warn!(?err, "state.json corrupt, starting fresh");
                PersistedState::default()
            })
        } else {
            PersistedState::default()
        };
        // Migrate any Windows verbatim-prefixed paths persisted by an older
        // build that called `canonicalize` without simplifying. Without this
        // the `claude` CLI still sees `\\?\…` as its cwd on the next spawn,
        // producing a different per-project memory key than running it by hand.
        let migrated = migrate_paths_in_place(&mut inner);
        let state = Self {
            dirs: dirs.clone(),
            inner: Mutex::new(inner),
        };
        if migrated {
            // Best-effort: persist the simplified paths immediately so the
            // next daemon start sees clean state without re-migrating.
            if let Err(err) = state.write_to_disk() {
                tracing::warn!(?err, "failed to persist path-migrated state.json");
            }
        }
        Ok(state)
    }

    pub fn with_persisted<R>(&self, f: impl FnOnce(&PersistedState) -> R) -> R {
        let guard = crate::sync::lock(&self.inner);
        f(&guard)
    }

    fn write_to_disk(&self) -> anyhow::Result<()> {
        let guard = crate::sync::lock(&self.inner);
        let bytes = serde_json::to_vec_pretty(&*guard).context("serializing state")?;
        let tmp = self.dirs.state_file.with_extension("json.tmp");
        std::fs::write(&tmp, &bytes).context("writing state tmp")?;
        std::fs::rename(&tmp, &self.dirs.state_file).context("renaming state.json")?;
        Ok(())
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

/// Walk through every stored path and rewrite it to the simplified form (no
/// `\\?\` prefix on Windows). Returns `true` if anything changed.
fn migrate_paths_in_place(state: &mut PersistedState) -> bool {
    let mut changed = false;
    for repo in &mut state.repos {
        if let Some(next) = simplify_str(&repo.path) {
            repo.path = next;
            changed = true;
        }
    }
    for ws in &mut state.workspaces {
        if let Some(linked) = ws.linked_vscode_workspace.as_ref()
            && let Some(next) = simplify_str(linked)
        {
            ws.linked_vscode_workspace = Some(next);
            changed = true;
        }
    }
    changed
}

/// Return a simplified copy of `s` iff simplification actually changes it.
fn simplify_str(s: &str) -> Option<String> {
    let simplified = simplify_path(Path::new(s));
    let as_str = simplified.to_string_lossy();
    (as_str != s).then(|| as_str.into_owned())
}
