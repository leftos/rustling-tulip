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

/// Reserved layout key for connections that don't send a `client_id` (older
/// app builds, or the plain-shell back-compat path). They all share this one
/// layout so they keep working without the per-client chooser.
pub const LEGACY_CLIENT_ID: &str = "__legacy__";

/// One client's persisted tab/pane layout. Sessions are global to the daemon;
/// only the layout — which sessions appear in which panes/tabs — is per-client.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientLayout {
    /// Human-readable label (e.g. the client's hostname), shown when another
    /// client offers to clone this layout. `None` until the client supplies one.
    #[serde(default)]
    pub name: Option<String>,
    /// Tab display order; each `TabEntry` owns its pane grid.
    #[serde(default)]
    pub tabs: Vec<TabEntry>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PersistedState {
    pub repos: Vec<RepoEntry>,
    pub workspaces: Vec<WorkspaceEntry>,
    /// Per-client tab layouts, keyed by `client_id`. Each client curates its
    /// own tabs/panes over the shared global session set.
    #[serde(default)]
    pub layouts: HashMap<String, ClientLayout>,
    /// Pre-per-client global layout, migrated in-place from the old top-level
    /// `tabs` field via the serde alias. Offered once to the first new client
    /// through the first-connect chooser (`CloneLegacy`), then cleared.
    #[serde(default, alias = "tabs")]
    pub legacy_tabs: Vec<TabEntry>,
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
    pub fn set_worktrees_root(&self, path: Option<String>) -> anyhow::Result<(PathBuf, bool)> {
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

    /// Clone of a client's tab layout. Empty when the client has no layout yet.
    pub fn client_layout(&self, client_id: &str) -> Vec<TabEntry> {
        self.with_persisted(|s| {
            s.layouts
                .get(client_id)
                .map_or_else(Vec::new, |l| l.tabs.clone())
        })
    }

    /// Whether a layout entry exists for this client. An entry with an empty
    /// `tabs` vec still counts — it's a deliberately-empty saved layout, not a
    /// first-connect that needs the chooser.
    pub fn has_client_layout(&self, client_id: &str) -> bool {
        self.with_persisted(|s| s.layouts.contains_key(client_id))
    }

    /// Mutate a client's tab vec in place (creating the entry if absent) and
    /// persist. The per-client analogue of [`AppState::mutate`] used by every
    /// tab-layout handler.
    pub fn mutate_client_layout<R>(
        &self,
        client_id: &str,
        f: impl FnOnce(&mut Vec<TabEntry>) -> R,
    ) -> anyhow::Result<R> {
        self.mutate(|s| {
            let layout = s.layouts.entry(client_id.to_string()).or_default();
            f(&mut layout.tabs)
        })
    }

    /// Create or replace a client's layout outright (first-connect init /
    /// chooser). Records the display name so other clients can clone it.
    pub fn set_client_layout(
        &self,
        client_id: &str,
        name: Option<String>,
        tabs: Vec<TabEntry>,
    ) -> anyhow::Result<()> {
        self.mutate(|s| {
            s.layouts
                .insert(client_id.to_string(), ClientLayout { name, tabs });
        })
    }

    /// Record/refresh a client's display name without touching its tabs. No-op
    /// when the client has no layout entry yet.
    pub fn set_client_name(&self, client_id: &str, name: Option<String>) -> anyhow::Result<()> {
        if name.is_none() {
            return Ok(());
        }
        self.mutate(|s| {
            if let Some(layout) = s.layouts.get_mut(client_id) {
                layout.name = name;
            }
        })
    }

    /// The pre-per-client global layout awaiting migration (empty once a client
    /// has adopted it via `CloneLegacy`).
    pub fn legacy_tabs(&self) -> Vec<TabEntry> {
        self.with_persisted(|s| s.legacy_tabs.clone())
    }

    pub fn clear_legacy_tabs(&self) -> anyhow::Result<()> {
        self.mutate(|s| s.legacy_tabs.clear())
    }

    /// Other clients' layouts available to clone: `(client_id, display name)`,
    /// excluding `exclude` (the requesting client) and any empty layouts (no
    /// point cloning an empty one).
    pub fn clonable_layouts(&self, exclude: &str) -> Vec<(String, Option<String>)> {
        self.with_persisted(|s| {
            let mut out: Vec<(String, Option<String>)> = s
                .layouts
                .iter()
                .filter(|(id, layout)| id.as_str() != exclude && !layout.tabs.is_empty())
                .map(|(id, layout)| (id.clone(), layout.name.clone()))
                .collect();
            out.sort_by(|a, b| a.0.cmp(&b.0));
            out
        })
    }

    /// Mutate every client's layout (session-removal fan-out) and persist once.
    pub fn mutate_all_layouts<R>(
        &self,
        f: impl FnOnce(&mut HashMap<String, ClientLayout>) -> R,
    ) -> anyhow::Result<R> {
        self.mutate(|s| f(&mut s.layouts))
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

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect for clearer failures"
)]
mod tests {
    use super::*;
    use protocol::{GridNode, TabContent};

    fn sample_tab(id: &str) -> TabEntry {
        TabEntry {
            id: id.to_string(),
            name: id.to_string(),
            content: TabContent::Grid {
                grid: GridNode::Pane {
                    pane_id: format!("pane-{id}"),
                    session_id: None,
                },
            },
            created_at: chrono::Utc::now(),
        }
    }

    fn scratch_dirs(tag: &str) -> Dirs {
        let root = std::env::temp_dir().join(format!("rt-state-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create scratch dir");
        Dirs {
            config: root.clone(),
            state_file: root.join("state.json"),
            handshake_file: root.join("daemon.json"),
            lan_config_file: root.join("lan.json"),
            lan_cert_file: root.join("lan-cert.pem"),
            lan_key_file: root.join("lan-key.pem"),
            sessions_dir: root.join("sessions"),
            worktrees_dir: root.join("worktrees"),
            binaries_dir: root.join("binaries"),
        }
    }

    #[test]
    fn old_tabs_key_migrates_into_legacy_tabs() {
        // A pre-per-client state.json used a top-level `tabs` array; the serde
        // alias must route it into `legacy_tabs` with `layouts` left empty.
        let json = r#"{
            "repos": [],
            "workspaces": [],
            "tabs": [
                {"id":"t1","name":"Tab","content":{"kind":"grid","grid":{"kind":"pane","pane_id":"p1","session_id":null}},"created_at":"2026-01-01T00:00:00Z"}
            ]
        }"#;
        let state: PersistedState = serde_json::from_str(json).expect("parse legacy state");
        assert_eq!(state.legacy_tabs.len(), 1, "old tabs land in legacy_tabs");
        assert!(state.layouts.is_empty(), "layouts start empty");
    }

    #[test]
    fn per_client_layouts_are_independent() {
        let dirs = scratch_dirs("independent");
        let state = AppState::load_or_default(&dirs).expect("load state");

        assert!(!state.has_client_layout("a"));
        state
            .mutate_client_layout("a", |tabs| tabs.push(sample_tab("ta")))
            .expect("mutate a");
        state
            .mutate_client_layout("b", |tabs| {
                tabs.push(sample_tab("tb1"));
                tabs.push(sample_tab("tb2"));
            })
            .expect("mutate b");

        assert!(state.has_client_layout("a"));
        assert_eq!(state.client_layout("a").len(), 1);
        assert_eq!(state.client_layout("a")[0].id, "ta");
        assert_eq!(state.client_layout("b").len(), 2);
        assert!(
            state.client_layout("c").is_empty(),
            "unknown client is empty"
        );

        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn set_client_layout_replaces_tabs_and_persists() {
        let dirs = scratch_dirs("setlayout");
        let state = AppState::load_or_default(&dirs).expect("load state");
        state
            .set_client_layout(
                "desktop",
                Some("desktop-host".to_string()),
                vec![sample_tab("t")],
            )
            .expect("set layout");
        assert_eq!(state.client_layout("desktop").len(), 1);
        // Reload from disk: the layout (and its name) survived the round-trip.
        let reloaded = AppState::load_or_default(&dirs).expect("reload state");
        assert_eq!(reloaded.client_layout("desktop").len(), 1);
        assert_eq!(reloaded.client_layout("desktop")[0].id, "t");

        let _ = std::fs::remove_dir_all(&dirs.config);
    }
}
