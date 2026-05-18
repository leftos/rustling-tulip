//! Persisted daemon state: registries of repos and workspaces.
//!
//! Sessions are *not* persisted across daemon restarts in Phase 1 — they're owned
//! by the daemon process. Phase 5 will add orphan recovery.

use crate::paths::{Dirs, simplify_path};
use anyhow::Context as _;
use protocol::{ContainerRef, RepoEntry, TabEntry, WorkspaceEntry};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PersistedState {
    pub repos: Vec<RepoEntry>,
    pub workspaces: Vec<WorkspaceEntry>,
    /// Tab layouts shared by all connected clients. Vec order is display
    /// order. Older state.json files without this field deserialize cleanly.
    #[serde(default)]
    pub tabs: Vec<TabEntry>,
    /// Manual sidebar-container order (workspaces + repos as a single
    /// flat list). Empty vec means "no manual order; clients fall back
    /// to alphabetical". Maintained by the registry helpers on every
    /// add/remove + replaced wholesale on `ReorderContainers`. New
    /// installations and old state.json files default to empty.
    #[serde(default)]
    pub container_order: Vec<ContainerRef>,
    /// Per-container session display order in the sidebar. Key is the
    /// workspace id, repo id, or tab id. Stale session ids are harmless
    /// and ignored on merge. Old state.json files default to empty.
    #[serde(default)]
    pub session_order: HashMap<String, Vec<String>>,
    /// User-customized worktrees root, persisted across daemon restarts.
    /// `None` means "use the env/platform default from `Dirs`". Old
    /// state.json files without this field deserialize cleanly.
    /// Mutated by `ClientMessage::SetWorktreesRoot` via
    /// [`AppState::set_worktrees_root`]. Read by every spawn path via
    /// [`AppState::worktrees_dir`] so a freshly-saved override takes
    /// effect on the next session without a daemon restart.
    #[serde(default)]
    pub worktrees_root_override: Option<String>,
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

    /// Effective worktrees root path: the user override from
    /// `PersistedState::worktrees_root_override` when set, else the
    /// resolved-at-startup default from `Dirs` (which already honors
    /// `RUSTLING_TULIP_WORKTREES_DIR` and platform defaults). Every
    /// spawn path that needs to compute member worktree paths must call
    /// this — never read `dirs.worktrees_dir` directly, or a freshly
    /// saved override won't take effect until daemon restart.
    pub fn worktrees_dir(&self) -> PathBuf {
        self.with_persisted(|s| s.worktrees_root_override.clone())
            .map_or_else(|| self.dirs.worktrees_dir.clone(), PathBuf::from)
    }

    /// True iff the active worktrees root came from the user setting
    /// (`worktrees_root_override`). False when falling back to the
    /// env/platform default. Used to drive the Settings UI's
    /// "currently overridden" indicator + the Reset-to-default button.
    pub fn worktrees_root_is_override(&self) -> bool {
        self.with_persisted(|s| s.worktrees_root_override.is_some())
    }

    /// Persist a worktrees-root override and create the directory if it
    /// doesn't exist. `None` clears the override, reverting to the
    /// env/platform default. Returns the new effective path + whether
    /// it's an override so the caller can broadcast the change.
    pub fn set_worktrees_root(
        &self,
        path: Option<String>,
    ) -> anyhow::Result<(PathBuf, bool)> {
        let normalized = if let Some(raw) = path {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                None
            } else {
                let pb = PathBuf::from(trimmed);
                std::fs::create_dir_all(&pb)
                    .with_context(|| format!("creating worktrees root {}", pb.display()))?;
                Some(pb.to_string_lossy().into_owned())
            }
        } else {
            None
        };
        self.mutate(|s| {
            s.worktrees_root_override = normalized;
        })?;
        Ok((self.worktrees_dir(), self.worktrees_root_is_override()))
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
