//! Persisted daemon state (`state.json`): the repo and workspace registries,
//! per-client tab layouts and ordering, and the host settings that must apply
//! with no window open (worktrees root override, keep-awake).
//!
//! Sessions are deliberately *not* stored here. They're rebuilt from their
//! per-session `meta.json` sidecars on startup (see `orphan.rs`), so the daemon
//! can restart without a single fragile state blob.

use crate::env_secrets::{self, SealOutcome};
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

/// serde default for [`PersistedState::keep_awake`]. A state.json written
/// before the setting existed must load as enabled, matching a fresh install.
fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// Host setting: hold the OS awake while any session has a live child.
    /// Lives here rather than in the app so it applies with no window open —
    /// the daemon outlives the window. Old state.json files (and a fresh
    /// install) default to enabled; see [`default_true`].
    #[serde(default = "default_true")]
    pub keep_awake: bool,
}

/// Hand-written so a fresh install (no state.json) matches what serde
/// produces for a file without the field: `keep_awake` on. A derived
/// `Default` would silently start with the hold disabled.
impl Default for PersistedState {
    fn default() -> Self {
        Self {
            repos: Vec::new(),
            workspaces: Vec::new(),
            layouts: HashMap::new(),
            legacy_tabs: Vec::new(),
            container_order: Vec::new(),
            session_order: HashMap::new(),
            worktrees_root_override: None,
            keep_awake: default_true(),
        }
    }
}

#[derive(Debug)]
pub struct AppState {
    pub dirs: Dirs,
    inner: Mutex<PersistedState>,
    /// True when `state.json` existed but could not be parsed, so `inner` is a
    /// fresh default and this process does not know what the file referenced.
    /// A startup sweep that must not miss a reference declines to act.
    state_file_corrupt: bool,
}

impl AppState {
    pub fn load_or_default(dirs: &Dirs) -> anyhow::Result<Self> {
        let (mut inner, state_file_corrupt) = if dirs.state_file.exists() {
            let bytes = std::fs::read(&dirs.state_file).context("reading state.json")?;
            match serde_json::from_slice::<PersistedState>(&bytes) {
                Ok(state) => (state, false),
                Err(err) => {
                    tracing::warn!(?err, "state.json corrupt, starting fresh");
                    (PersistedState::default(), true)
                }
            }
        } else {
            (PersistedState::default(), false)
        };
        // Migrate any Windows verbatim-prefixed paths persisted by an older
        // build that called `canonicalize` without simplifying. Without this
        // the `claude` CLI still sees `\\?\…` as its cwd on the next spawn,
        // producing a different per-project memory key than running it by hand.
        let migrated = migrate_paths_in_place(&mut inner);
        let state = Self {
            dirs: dirs.clone(),
            inner: Mutex::new(inner),
            state_file_corrupt,
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

    /// Seal the secret literal rows every persisted `last_spawn_config` holds,
    /// moving each plain-text secret into the credential store and leaving a
    /// `${secret:<id>}` reference in its place. Returns whether anything
    /// changed; `state.json` is rewritten only when something did.
    ///
    /// A store refusal leaves both the in-memory state and `state.json` exactly
    /// as they were — a warning naming the file and the key is logged — so the
    /// next start tries again.
    ///
    /// # Errors
    ///
    /// Fails only when the sealed state cannot be written back.
    pub fn seal_last_spawn_configs(&self) -> anyhow::Result<bool> {
        let mut guard = crate::sync::lock(&self.inner);
        if !seal_secret_rows_in_place(&mut guard, &self.dirs.state_file) {
            return Ok(false);
        }
        persist(&self.dirs, &guard)?;
        Ok(true)
    }

    /// Whether `state.json` existed but could not be parsed, so this process
    /// runs on a fresh default and does not know which repos, workspaces or
    /// `last_spawn_config`s the file carried.
    pub fn state_file_corrupt(&self) -> bool {
        self.state_file_corrupt
    }

    pub fn with_persisted<R>(&self, f: impl FnOnce(&PersistedState) -> R) -> R {
        let guard = crate::sync::lock(&self.inner);
        f(&guard)
    }

    fn write_to_disk(&self) -> anyhow::Result<()> {
        let guard = crate::sync::lock(&self.inner);
        persist(&self.dirs, &guard)
    }

    /// Run `f` on the live state and persist the result. Use this for closures
    /// that cannot fail; a closure that can fail halfway belongs on
    /// [`AppState::try_mutate`].
    pub fn mutate<R>(&self, f: impl FnOnce(&mut PersistedState) -> R) -> anyhow::Result<R> {
        let mut guard = crate::sync::lock(&self.inner);
        let result = f(&mut guard);
        persist(&self.dirs, &guard)?;
        Ok(result)
    }

    /// Fallible [`AppState::mutate`]: `f` runs against a clone, and the live
    /// state is replaced and `state.json` rewritten only once it returns `Ok`.
    /// A closure that fails after changing something — a pane pulled out of a
    /// tab before an unknown second pane id is reached, an extract whose insert
    /// target turns out to be missing — leaves both the in-memory state and the
    /// file exactly as they were, so the error a handler replies with never
    /// describes a half-applied layout.
    pub fn try_mutate<T>(
        &self,
        f: impl FnOnce(&mut PersistedState) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let mut guard = crate::sync::lock(&self.inner);
        let mut candidate = PersistedState::clone(&guard);
        let result = f(&mut candidate)?;
        persist(&self.dirs, &candidate)?;
        *guard = candidate;
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

    /// Whether the daemon should hold the OS awake while a session is live.
    /// Read at startup to seed the keep-awake watcher's setting watch.
    pub fn keep_awake(&self) -> bool {
        self.with_persisted(|s| s.keep_awake)
    }

    /// Persist the keep-awake setting. The caller re-publishes it on the
    /// watch so the running watcher re-derives the hold without a restart.
    pub fn set_keep_awake(&self, enabled: bool) -> anyhow::Result<()> {
        self.mutate(|s| {
            s.keep_awake = enabled;
        })
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

    /// Fallible [`AppState::mutate_client_layout`]: a closure that returns
    /// `Err` leaves the client's layout as it was, and a client that had no
    /// layout entry keeps none instead of gaining an empty one.
    pub fn try_mutate_client_layout<T>(
        &self,
        client_id: &str,
        f: impl FnOnce(&mut Vec<TabEntry>) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        self.try_mutate(|s| {
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

/// Serialize `state` and swap it in as `state.json` via a temporary file, so a
/// reader never observes a partially-written file. Written owner-only, like
/// the other files a spawn config reaches (`secret::write_private`).
fn persist(dirs: &Dirs, state: &PersistedState) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec_pretty(state).context("serializing state")?;
    let tmp = dirs.state_file.with_extension("json.tmp");
    crate::secret::write_private(&tmp, &bytes).context("writing state tmp")?;
    std::fs::rename(&tmp, &dirs.state_file).context("renaming state.json")?;
    Ok(())
}

/// Seal the secret literal rows in every persisted `last_spawn_config` (repos
/// and workspaces), returning whether any row changed.
///
/// Row by row, like the sidecar and history passes go file by file: a value
/// the store refuses stays in plain text — [`env_secrets::seal_stored_rows`]
/// warns with the file and the key — and every other row is still sealed, so
/// one over-long value never costs the rest of the file.
fn seal_secret_rows_in_place(state: &mut PersistedState, file: &Path) -> bool {
    let mut changed = false;
    for config in state
        .repos
        .iter_mut()
        .filter_map(|repo| repo.last_spawn_config.as_mut())
        .chain(
            state
                .workspaces
                .iter_mut()
                .filter_map(|workspace| workspace.last_spawn_config.as_mut()),
        )
    {
        if let SealOutcome::Sealed(rows) = env_secrets::seal_stored_rows(&config.extra_env, file) {
            config.extra_env = rows;
            changed = true;
        }
    }
    changed
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
    fn keep_awake_defaults_to_enabled_for_old_state_files() {
        let json = r#"{"repos": [], "workspaces": []}"#;
        let state: PersistedState = serde_json::from_str(json).expect("parse old state");
        assert!(
            state.keep_awake,
            "a state.json predating the setting must load as enabled"
        );
    }

    #[test]
    fn set_keep_awake_round_trips_through_disk() {
        let dirs = scratch_dirs("keepawake");
        let state = AppState::load_or_default(&dirs).expect("load state");
        assert!(state.keep_awake(), "fresh state starts enabled");
        state
            .set_keep_awake(false)
            .expect("persist keep_awake=false");
        assert!(!state.keep_awake());

        let reloaded = AppState::load_or_default(&dirs).expect("reload state");
        assert!(
            !reloaded.keep_awake(),
            "the disabled setting survives a reload"
        );

        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn fresh_install_without_state_file_keeps_awake() {
        let dirs = scratch_dirs("keepawake-fresh");
        assert!(!dirs.state_file.exists(), "scratch dir starts empty");
        let state = AppState::load_or_default(&dirs).expect("load state");
        assert!(
            state.keep_awake(),
            "a fresh install must default to holding the machine awake"
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

    #[test]
    fn try_mutate_ok_commits_and_writes() {
        let dirs = scratch_dirs("try-ok");
        let state = AppState::load_or_default(&dirs).expect("load state");
        state.set_keep_awake(false).expect("seed state.json");

        state
            .try_mutate(|s| {
                s.keep_awake = true;
                s.legacy_tabs.push(sample_tab("committed"));
                Ok(())
            })
            .expect("a closure that returns Ok commits");

        assert!(state.keep_awake(), "the clone's change is live");
        assert_eq!(state.legacy_tabs().len(), 1, "and so is its tab");

        let reloaded = AppState::load_or_default(&dirs).expect("reload state");
        assert!(reloaded.keep_awake(), "the change reached state.json");
        assert_eq!(
            reloaded.legacy_tabs().len(),
            1,
            "the whole commit reached state.json"
        );

        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn try_mutate_err_leaves_state_and_disk_unchanged() {
        let dirs = scratch_dirs("try-err");
        let state = AppState::load_or_default(&dirs).expect("load state");
        state.set_keep_awake(false).expect("seed state.json");
        let before_bytes = std::fs::read(&dirs.state_file).expect("read state.json");
        assert!(before_bytes.len() > 2, "the seeded state.json has content");

        let err = state
            .try_mutate(|s| -> anyhow::Result<()> {
                s.keep_awake = true;
                s.legacy_tabs.push(sample_tab("rolled-back"));
                anyhow::bail!("closure failed after mutating");
            })
            .expect_err("the closure's error propagates");
        assert_eq!(err.to_string(), "closure failed after mutating");

        assert!(!state.keep_awake(), "the in-memory change was rolled back");
        assert!(
            state.legacy_tabs().is_empty(),
            "the in-memory tab was rolled back"
        );
        let after_bytes = std::fs::read(&dirs.state_file).expect("read state.json");
        assert_eq!(
            after_bytes, before_bytes,
            "state.json on disk is byte-for-byte unchanged"
        );

        let _ = std::fs::remove_dir_all(&dirs.config);
    }
}
