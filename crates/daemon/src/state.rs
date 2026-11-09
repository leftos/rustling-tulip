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
use chrono::{DateTime, Utc};
use protocol::{
    AgentId, ContainerRef, DefaultShell, KeepAwakeMode, OptionValues, RepoEntry,
    SummarizerSettings, TabEntry, WorkspaceEntry,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap};
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    /// Host setting: when the daemon holds the OS awake. Lives here rather
    /// than in the app so it applies with no window open — the daemon
    /// outlives the window. A fresh install and a state.json without the
    /// field get [`KeepAwakeMode::WhenBusy`].
    #[serde(default)]
    pub keep_awake_mode: KeepAwakeMode,
    /// Host setting: whether the daemon's log filter runs at the detailed
    /// level. Lives here rather than in the app so the choice applies from
    /// the first line of the next run, with no window open. A fresh install
    /// and a state.json without the field log at the default level.
    #[serde(default)]
    pub detailed_logging: bool,
    /// Host setting: the command each agent's interactive spawns launch,
    /// keyed by the agent's id. Holds only agents whose command is set; an
    /// absent agent launches its plugin's own program. Read at spawn through
    /// [`AppState::agent_command`], never copied into a session's spawn
    /// config, so Restart and Resume use the current command.
    #[serde(default)]
    pub agent_commands: HashMap<AgentId, AgentCommand>,
    /// Host setting: each agent's Settings page values, validated against
    /// its option schema, keyed by the agent's id. A client prefills its
    /// spawn dialog from them; a spawn never reads them.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub agent_settings: BTreeMap<AgentId, OptionValues>,
    /// Host setting: the Dashboard summarizer's settings, absent until they
    /// are first saved; [`AppState::summarizer_settings`] builds the defaults
    /// meanwhile. It holds a key's source, never a key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summarizer: Option<SummarizerSettings>,
    /// Host setting: the shell plain-shell sessions run and an interactive
    /// agent is typed into. Absent means Automatic, as does an id this build
    /// does not know. Read at spawn through [`AppState::default_shell`], so
    /// Restart and Resume use the current choice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_shell: Option<DefaultShell>,
}

/// One agent's stored launch command, both fields as the user typed them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCommand {
    /// The program that replaces the plugin's own; blank for the default.
    #[serde(default)]
    pub program: String,
    /// Extra arguments after the plugin's argv, split by POSIX shell-word
    /// rules at spawn (typed verbatim in the shell form).
    #[serde(default)]
    pub args: String,
}

impl AgentCommand {
    /// Whether both fields are blank (whitespace only), so the agent launches
    /// as if nothing were set.
    pub fn is_blank(&self) -> bool {
        self.program.trim().is_empty() && self.args.trim().is_empty()
    }
}

#[derive(Debug)]
pub struct AppState {
    pub dirs: Dirs,
    inner: Mutex<PersistedState>,
    /// True when `state.json` existed but could not be parsed, so `inner` holds
    /// only what could be salvaged and this process may not know every repo,
    /// workspace or `last_spawn_config` the file carried. A startup sweep that
    /// must not miss a reference declines to act.
    state_file_corrupt: bool,
}

impl AppState {
    pub fn load_or_default(dirs: &Dirs) -> anyhow::Result<Self> {
        let (mut inner, state_file_corrupt) = if dirs.state_file.exists() {
            let bytes = std::fs::read(&dirs.state_file).context("reading state.json")?;
            match serde_json::from_slice::<PersistedState>(&bytes) {
                Ok(state) => (state, false),
                Err(err) => {
                    // The bytes are the only record of every repo, workspace
                    // and host setting this file held. Keep them beside the
                    // file the next save writes, rather than overwriting them.
                    match keep_corrupt_state(&dirs.state_file, &bytes, Utc::now()) {
                        Ok(kept) => tracing::warn!(
                            at = %crate::logging::json_error_at(&err),
                            kept = %kept.display(),
                            "state.json corrupt; kept its bytes and salvaging what still reads"
                        ),
                        Err(keep_err) => tracing::error!(
                            at = %crate::logging::json_error_at(&err),
                            error = ?keep_err,
                            "state.json corrupt and could not be kept; salvaging what still reads"
                        ),
                    }
                    (salvage_state(&bytes), true)
                }
            }
        } else {
            (PersistedState::default(), false)
        };
        // Migrate any Windows verbatim-prefixed paths persisted by an older
        // build that called `canonicalize` without simplifying. Without this
        // the agent CLI still sees `\\?\…` as its cwd on the next spawn,
        // producing a different per-project memory key than running it by hand.
        let migrated = migrate_paths_in_place(&mut inner);
        let sanitized = sanitize_loaded_layouts(&mut inner);
        let folded = fold_trusted_configs(&mut inner);
        let state = Self {
            dirs: dirs.clone(),
            inner: Mutex::new(inner),
            state_file_corrupt,
        };
        if migrated || state_file_corrupt || sanitized || folded {
            // Best-effort: put the simplified paths, whatever a corrupt file
            // still yielded, and any tab list put right, on disk right away. A
            // start that never writes again then finds the file it should
            // keep, rather than none: the corrupt original has moved aside and
            // only this write stands between the salvage and an empty next
            // start.
            if let Err(err) = state.write_to_disk() {
                tracing::warn!(?err, "failed to persist the loaded state.json");
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
    /// runs on what could be salvaged from it and a dropped entry may have held
    /// a reference it no longer knows about.
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

    /// When the daemon should hold the OS awake. Read at startup to seed the
    /// keep-awake watcher's setting watch.
    pub fn keep_awake_mode(&self) -> KeepAwakeMode {
        self.with_persisted(|s| s.keep_awake_mode)
    }

    /// Persist the keep-awake mode. The caller re-publishes it on the watch
    /// so the running watcher re-derives the hold without a restart.
    pub fn set_keep_awake_mode(&self, mode: KeepAwakeMode) -> anyhow::Result<()> {
        self.mutate(|s| {
            s.keep_awake_mode = mode;
        })
    }

    /// Whether the daemon's log filter runs at the detailed level. Read at
    /// startup, after the state loads, to reload the filter before the rest
    /// of the run logs.
    pub fn detailed_logging(&self) -> bool {
        self.with_persisted(|s| s.detailed_logging)
    }

    /// Persist the Detailed logging switch. The caller reloads the daemon's
    /// log filter to match.
    pub fn set_detailed_logging(&self, enabled: bool) -> anyhow::Result<()> {
        self.mutate(|s| {
            s.detailed_logging = enabled;
        })
    }

    /// The stored default shell; `None` is Automatic. Every shell spawn reads
    /// it here, at spawn time.
    pub fn default_shell(&self) -> Option<DefaultShell> {
        self.with_persisted(|s| s.default_shell.clone())
    }

    /// Persist the default shell; `None` (Automatic) removes the key.
    pub fn set_default_shell(&self, shell: Option<DefaultShell>) -> anyhow::Result<()> {
        self.mutate(|s| {
            s.default_shell = shell;
        })
    }

    /// The launch command `agent`'s interactive spawns use: blank when none is
    /// set. Every spawn path reads it here, at spawn time.
    pub fn agent_command(&self, agent: &AgentId) -> AgentCommand {
        self.with_persisted(|s| s.agent_commands.get(agent).cloned().unwrap_or_default())
    }

    /// Persist `agent`'s launch command; a blank pair removes its entry.
    pub fn set_agent_command(&self, agent: &AgentId, command: AgentCommand) -> anyhow::Result<()> {
        self.mutate(|s| {
            if command.is_blank() {
                s.agent_commands.remove(agent);
            } else {
                s.agent_commands.insert(agent.clone(), command);
            }
        })
    }

    /// Every agent's stored Settings page values.
    pub fn agent_settings(&self) -> BTreeMap<AgentId, OptionValues> {
        self.with_persisted(|s| s.agent_settings.clone())
    }

    /// Persist `agent`'s Settings page values, already validated.
    pub fn set_agent_settings(&self, agent: &AgentId, options: OptionValues) -> anyhow::Result<()> {
        self.mutate(|s| {
            s.agent_settings.insert(agent.clone(), options);
        })
    }

    /// The summarizer's settings: the saved ones, else the defaults
    /// (`summarizer::policy::default_settings`).
    pub fn summarizer_settings(&self) -> SummarizerSettings {
        self.with_persisted(|s| s.summarizer.clone())
            .unwrap_or_else(crate::summarizer::policy::default_settings)
    }

    /// Persist the summarizer's settings, already validated.
    pub fn set_summarizer_settings(&self, settings: SummarizerSettings) -> anyhow::Result<()> {
        self.mutate(|s| {
            s.summarizer = Some(settings);
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

/// The prefix a quarantined copy of an unparsable `state.json` carries, followed
/// by the UTC stamp that tells the copies apart.
const CORRUPT_STATE_PREFIX: &str = "state.json.corrupt-";

/// How many quarantined copies of `state.json` are kept; the older ones go.
const KEPT_CORRUPT_STATES: usize = 3;

/// `at` stamped the way a quarantined copy's name carries it.
fn corrupt_stamp(at: DateTime<Utc>) -> String {
    at.format("%Y%m%dT%H%M%SZ").to_string()
}

/// Move the unparsable `state_file` aside as `state.json.corrupt-<stamp>` in its
/// folder, then drop every such copy but the newest few, so a start that fell
/// back to what still parses never destroys what was there.
///
/// The move is a rename; when it fails (the file is held open, or the two names
/// sit on different volumes) `bytes` — the content the caller already read — is
/// written to the same name instead, through the owner-only write `persist`
/// uses. The copy just made is never pruned, whatever its name sorts as. `at` is
/// a parameter so a test pins the stamp; no clock is read here.
fn keep_corrupt_state(
    state_file: &Path,
    bytes: &[u8],
    at: DateTime<Utc>,
) -> anyhow::Result<PathBuf> {
    let folder = state_file
        .parent()
        .context("the state file has no folder")?;
    let target = free_corrupt_path(folder, at);
    if let Err(rename_err) = std::fs::rename(state_file, &target) {
        tracing::warn!(
            ?rename_err,
            path = %state_file.display(),
            "could not move the corrupt state.json aside; writing its bytes beside it"
        );
        crate::secret::write_private(&target, bytes)
            .with_context(|| format!("writing {}", target.display()))?;
    }
    prune_corrupt_states(folder, &target);
    Ok(target)
}

/// The first free `state.json.corrupt-<stamp>` path in `folder`: the stamp from
/// `at`, then `-2`, `-3`, … while a copy of that name is already there.
fn free_corrupt_path(folder: &Path, at: DateTime<Utc>) -> PathBuf {
    let stamp = corrupt_stamp(at);
    let first = folder.join(format!("{CORRUPT_STATE_PREFIX}{stamp}"));
    if !first.exists() {
        return first;
    }
    let mut n = 2_u32;
    loop {
        let candidate = folder.join(format!("{CORRUPT_STATE_PREFIX}{stamp}-{n}"));
        if !candidate.exists() {
            return candidate;
        }
        n += 1;
    }
}

/// Whether `path` is a quarantined copy of an unparsable `state.json`.
pub(crate) fn is_corrupt_copy(path: &Path) -> bool {
    path.is_file()
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(CORRUPT_STATE_PREFIX))
}

/// Delete every quarantined copy in `folder` but the newest
/// [`KEPT_CORRUPT_STATES`], judged by name (the stamp sorts), never touching
/// `kept` — the copy this start just made — whatever its name sorts as. Pruning
/// is housekeeping: a copy that will not go stays in place with a warning, and a
/// listing failure never fails the start that called it.
fn prune_corrupt_states(folder: &Path, kept: &Path) {
    let entries = match std::fs::read_dir(folder) {
        Ok(entries) => entries,
        Err(err) => {
            tracing::warn!(
                ?err,
                path = %folder.display(),
                "could not list the corrupt state.json copies"
            );
            return;
        }
    };
    let mut copies = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => {
                let path = entry.path();
                if is_corrupt_copy(&path) {
                    copies.push(path);
                }
            }
            Err(err) => tracing::warn!(?err, "could not read a config folder entry"),
        }
    }
    copies.sort();
    let excess = copies.len().saturating_sub(KEPT_CORRUPT_STATES);
    for path in copies.into_iter().take(excess) {
        if path == kept {
            continue;
        }
        if let Err(err) = std::fs::remove_file(&path) {
            tracing::warn!(
                ?err,
                path = %path.display(),
                "could not prune an old corrupt state.json copy"
            );
        }
    }
}

/// What can still be read from a `state.json` that failed to parse: every field
/// and every list entry that parses on its own, the rest dropped with a logged
/// reason. Entirely unreadable bytes give the defaults.
fn salvage_state(bytes: &[u8]) -> PersistedState {
    let Ok(value) = serde_json::from_slice::<Value>(bytes) else {
        log_dropped_field("<file>");
        return PersistedState::default();
    };
    let Value::Object(fields) = value else {
        log_dropped_field("<file>");
        return PersistedState::default();
    };
    // The two registries carry no serde default, so a file missing either one
    // still needs the key there to parse at all; their empty arrays read as a
    // file that simply held none.
    let mut salvaged = Map::new();
    salvaged.insert("repos".to_owned(), Value::Array(Vec::new()));
    salvaged.insert("workspaces".to_owned(), Value::Array(Vec::new()));
    for (key, value) in fields {
        let value = keep_parsing_field(&key, value);
        salvaged.insert(key.clone(), value);
        if serde_json::from_value::<PersistedState>(Value::Object(salvaged.clone())).is_err() {
            salvaged.remove(&key);
            log_dropped_field(&key);
        }
    }
    if let Ok(state) = serde_json::from_value::<PersistedState>(Value::Object(salvaged)) {
        state
    } else {
        log_dropped_field("<file>");
        PersistedState::default()
    }
}

/// One `"state.json: dropped a bad field"` warning naming `field` and nothing
/// else, so no value out of the file can reach the log.
fn log_dropped_field(field: &str) {
    tracing::warn!(field, count = 1_usize, "state.json: dropped a bad field");
}

/// One `"state.json: dropped a bad entry"` warning naming where under `field`
/// the entries were dropped and how many, never their content.
fn log_dropped_entries(field: &str, count: usize) {
    tracing::warn!(field, count, "state.json: dropped a bad entry");
}

/// `value` with every entry that does not parse on its own dropped, so one bad
/// entry costs only itself. A key this build holds no entries under is returned
/// as it came.
fn keep_parsing_field(key: &str, value: Value) -> Value {
    match key {
        "repos" => keep_parsing::<RepoEntry>(key, value),
        "workspaces" => keep_parsing::<WorkspaceEntry>(key, value),
        "legacy_tabs" | "tabs" => keep_parsing::<TabEntry>(key, value),
        "layouts" => keep_parsing_layouts(value),
        _ => value,
    }
}

/// `value` filtered to the array elements that parse as one `T`, logging what
/// it dropped under `field`. A value that is not an array reads as an empty one.
fn keep_parsing<T: DeserializeOwned>(field: &str, value: Value) -> Value {
    let Value::Array(items) = value else {
        log_dropped_field(field);
        return Value::Array(Vec::new());
    };
    let total = items.len();
    let kept: Vec<Value> = items
        .into_iter()
        .filter(|item| serde_json::from_value::<T>(item.clone()).is_ok())
        .collect();
    if kept.len() < total {
        log_dropped_entries(field, total - kept.len());
    }
    Value::Array(kept)
}

/// Every layout whose tabs, filtered to the ones that parse, still parse as a
/// `ClientLayout`; a layout of any other shape is dropped. A value that is not a
/// JSON object reads as an empty one.
fn keep_parsing_layouts(value: Value) -> Value {
    let Value::Object(layouts) = value else {
        log_dropped_field("layouts");
        return Value::Object(Map::new());
    };
    let mut kept = Map::new();
    for (client_id, mut layout) in layouts {
        keep_layout_fields(&client_id, &mut layout);
        if serde_json::from_value::<ClientLayout>(layout.clone()).is_ok() {
            kept.insert(client_id, layout);
        } else {
            tracing::warn!(
                client_id = %client_id,
                count = 1_usize,
                "state.json: dropped a bad entry"
            );
        }
    }
    Value::Object(kept)
}

/// One layout value with its `"tabs"` array filtered to the tabs that parse and
/// a `"name"` that is not text removed, logging what it dropped under the client
/// the layout belongs to. A layout that is not a JSON object is left as it is
/// for its own parse to drop; one with no `"tabs"` parses as no tabs and is kept.
fn keep_layout_fields(client_id: &str, layout: &mut Value) {
    let Some(fields) = layout.as_object_mut() else {
        return;
    };
    if let Some(name) = fields.get("name").cloned()
        && serde_json::from_value::<Option<String>>(name).is_err()
    {
        fields.remove("name");
        log_dropped_entries(&format!("layouts.{client_id}.name"), 1);
    }
    let Some(tabs) = fields.remove("tabs") else {
        return;
    };
    let kept = keep_parsing::<TabEntry>(&format!("layouts.{client_id}.tabs"), tabs);
    fields.insert("tabs".to_owned(), kept);
}

/// Serialize `state` and swap it in as `state.json` through
/// [`crate::atomic_file::write_atomic`], so a reader never observes a
/// partially-written file and a power loss never leaves a torn one.
fn persist(dirs: &Dirs, state: &PersistedState) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec_pretty(state).context("serializing state")?;
    crate::atomic_file::write_atomic(&dirs.state_file, &bytes).context("writing state.json")
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

/// Fold the trusted flag of every persisted `last_spawn_config` (repos and workspaces) into its
/// agent's bypass choice, returning whether any config changed.
fn fold_trusted_configs(state: &mut PersistedState) -> bool {
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
        changed |= crate::trusted_fold::fold_config(config);
    }
    changed
}

/// Brings every tab list read from disk into a shape the daemon can serve:
/// repeated tab and pane ids are dropped and out-of-range split ratios are
/// clamped, each with a logged reason. Returns `true` if anything changed.
fn sanitize_loaded_layouts(state: &mut PersistedState) -> bool {
    let mut changed = false;
    for layout in state.layouts.values_mut() {
        changed |= crate::tabs::sanitize_loaded_tabs(&mut layout.tabs);
    }
    changed |= crate::tabs::sanitize_loaded_tabs(&mut state.legacy_tabs);
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
    use serde_json::json;

    fn sample_tab(id: &str) -> TabEntry {
        TabEntry {
            id: id.to_string(),
            name: id.to_string(),
            content: TabContent::Grid {
                grid: GridNode::Pane {
                    pane_id: format!("pane-{id}"),
                    session_id: None,
                    host: None,
                },
            },
            created_at: chrono::Utc::now(),
            picked_layout: None,
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
            remote_config_file: root.join("remote.json"),
            sessions_dir: root.join("sessions"),
            worktrees_dir: root.join("worktrees"),
            binaries_dir: root.join("binaries"),
            bin_templates: None,
        }
    }

    /// Every quarantined `state.json` copy in `folder`, sorted by name.
    fn corrupt_copies(folder: &Path) -> Vec<PathBuf> {
        let mut copies: Vec<PathBuf> = std::fs::read_dir(folder)
            .expect("list the config folder")
            .map(|entry| entry.expect("read a folder entry").path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("state.json.corrupt-"))
            })
            .collect();
        copies.sort();
        copies
    }

    #[test]
    fn a_damaged_state_file_logs_its_position_not_its_values() {
        use tracing_subscriber::layer::SubscriberExt;

        let dirs = scratch_dirs("damaged-state-position");
        std::fs::write(&dirs.state_file, br#"{"repos":"sk-live-123"}"#)
            .expect("seed a damaged state.json");
        let capture = crate::logging::tests::Capture::default();
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_writer(capture.clone())
                .with_ansi(false),
        );
        tracing::subscriber::with_default(subscriber, || {
            AppState::load_or_default(&dirs)
                .expect("a damaged state.json still salvages what reads");
        });
        let text = capture.text();
        assert!(
            !text.contains("sk-live-123"),
            "the log names where the file stops parsing, not the value: {text}"
        );
        assert!(
            text.contains("line 1 column"),
            "the log names the position: {text}"
        );
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

    /// The `state.json` v0.5.1-alpha wrote: every field of the released
    /// `PersistedState`, each set to a value no default would produce.
    const V0_5_1_STATE_JSON: &str = r##"{
        "repos": [
            {
                "id": "repo-a",
                "name": "Alpha",
                "path": "X:\\dev\\alpha",
                "default_branch": "trunk",
                "default_use_worktree": false,
                "appearance": {
                    "accent_color": "#f07a62",
                    "terminal_background_color": "#101010",
                    "terminal_frame_color": "#202020",
                    "terminal_font_family": "JetBrains Mono",
                    "terminal_font_size": 15,
                    "terminal_font_bold": true
                },
                "last_agent": "claude",
                "last_spawn_config": {
                    "target": {
                        "kind": "single",
                        "repo_id": "repo-a",
                        "branch_name": "feat/alpha",
                        "base_branch": "trunk",
                        "use_worktree": true,
                        "checkout_strategy": null,
                        "worktree_reuse": "reuse",
                        "existing_worktree": null
                    },
                    "mode": "interactive",
                    "dangerously_skip_permissions": true,
                    "agent": {
                        "agent": "claude",
                        "options": {"model": "opus", "trusted": true}
                    },
                    "extra_env": [["ALPHA_ENV", "${secret:1f0c2b3a}"]]
                }
            }
        ],
        "workspaces": [
            {
                "id": "ws-a",
                "name": "Alpha Workspace",
                "member_repo_ids": ["repo-a", "repo-b"],
                "linked_vscode_workspace": "X:\\dev\\alpha\\alpha.code-workspace",
                "watch_vscode_workspace": true,
                "default_use_worktree": false,
                "appearance": {
                    "accent_color": null,
                    "terminal_background_color": null,
                    "terminal_frame_color": null,
                    "terminal_font_family": "Fira Code",
                    "terminal_font_size": 13,
                    "terminal_font_bold": false
                },
                "last_spawn_config": {
                    "target": {
                        "kind": "workspace",
                        "workspace_id": "ws-a",
                        "branch_name": "feat/alpha",
                        "base_branch": "trunk",
                        "use_worktree": true,
                        "worktree_reuse": "reuse",
                        "existing_worktrees": [
                            {"repo_id": "repo-a", "path": "X:\\dev\\wt\\alpha"}
                        ]
                    },
                    "mode": "headless",
                    "dangerously_skip_permissions": true,
                    "agent": {
                        "agent": "claude",
                        "options": {"model": "sonnet"}
                    },
                    "extra_env": [["WORKSPACE_ENV", "on"]]
                }
            }
        ],
        "layouts": {
            "client-1": {
                "name": "leftos-desktop",
                "tabs": [
                    {
                        "id": "tab-1",
                        "name": "Alpha",
                        "content": {
                            "kind": "grid",
                            "grid": {"kind": "pane", "pane_id": "pane-1", "session_id": "sess-1"}
                        },
                        "created_at": "2025-01-02T03:04:05Z",
                        "picked_layout": {"kind": "grid", "cols": 2, "aspect": 1.5}
                    }
                ]
            }
        },
        "legacy_tabs": [
            {
                "id": "tab-legacy",
                "name": "Legacy",
                "content": {
                    "kind": "grid",
                    "grid": {"kind": "pane", "pane_id": "pane-legacy", "session_id": "sess-2"}
                },
                "created_at": "2024-12-01T00:00:00Z",
                "picked_layout": {"kind": "horizontal"}
            }
        ],
        "container_order": [
            {"kind": "repo", "id": "repo-a"},
            {"kind": "workspace", "id": "ws-a"}
        ],
        "session_order": {"repo-a": ["sess-1", "sess-2"]},
        "worktrees_root_override": "X:\\dev\\wt",
        "keep_awake_mode": "always",
        "agent_commands": {
            "claude": {"program": "C:\\Tools\\claude.cmd", "args": "--verbose"}
        },
        "agent_settings": {
            "claude": {"model": "opus", "trusted": true}
        },
        "summarizer": {
            "enabled": true,
            "provider": "deepseek",
            "key_source": {"kind": "env", "variable": "DEEPSEEK_API_KEY"},
            "daily_cap_usd": 2.5
        }
    }"##;

    /// The repo a v0.5.1-alpha `state.json` held, field by field.
    fn assert_v0_5_1_repo(repo: &protocol::RepoEntry) {
        assert_eq!(repo.id, "repo-a");
        assert_eq!(repo.name, "Alpha");
        assert_eq!(repo.path, r"X:\dev\alpha");
        assert_eq!(repo.default_branch.as_deref(), Some("trunk"));
        assert!(
            !repo.default_use_worktree,
            "the repo's worktree default is kept"
        );
        assert_eq!(repo.appearance.accent_color.as_deref(), Some("#f07a62"));
        assert_eq!(
            repo.appearance.terminal_background_color.as_deref(),
            Some("#101010")
        );
        assert_eq!(
            repo.appearance.terminal_frame_color.as_deref(),
            Some("#202020")
        );
        assert_eq!(
            repo.appearance.terminal_font_family.as_deref(),
            Some("JetBrains Mono")
        );
        assert_eq!(repo.appearance.terminal_font_size, Some(15));
        assert_eq!(
            repo.appearance.terminal_font_bold,
            Some(true),
            "the released bold switch is kept"
        );
        assert_eq!(
            repo.appearance.terminal_font_weight, None,
            "a weight in a field the release never wrote stays unset"
        );
        assert_eq!(
            repo.appearance.terminal_frame_width, None,
            "a frame width the release never wrote stays unset"
        );
        assert_eq!(
            repo.last_agent,
            Some(protocol::AgentId("claude".to_owned()))
        );

        let spawn = repo
            .last_spawn_config
            .as_ref()
            .expect("the repo keeps its last spawn config");
        assert_eq!(spawn.mode, protocol::SessionMode::Interactive);
        assert!(spawn.dangerously_skip_permissions);
        assert_eq!(
            spawn.extra_env,
            vec![("ALPHA_ENV".to_owned(), "${secret:1f0c2b3a}".to_owned())],
            "a stored secret reference is kept as a reference"
        );
        let selection = spawn.agent.as_ref().expect("the spawn keeps its agent");
        assert_eq!(selection.agent, protocol::AgentId("claude".to_owned()));
        assert_eq!(
            selection.options.get("model"),
            Some(&protocol::OptionValue::Text("opus".to_owned()))
        );
        assert_eq!(
            selection.options.get("trusted"),
            Some(&protocol::OptionValue::Bool(true)),
            "an option the descriptor does not know is kept as it was"
        );
        assert_eq!(
            selection.options.get("permission_mode"),
            Some(&protocol::OptionValue::Text(
                "bypass_permissions".to_owned()
            )),
            "the saved trusted flag loads as the bypass choice"
        );
        assert!(
            matches!(
                &spawn.target,
                protocol::SpawnTarget::Single {
                    repo_id,
                    branch_name,
                    base_branch,
                    use_worktree,
                    checkout_strategy,
                    worktree_reuse,
                    existing_worktree,
                } if repo_id == "repo-a"
                    && branch_name == "feat/alpha"
                    && base_branch.as_deref() == Some("trunk")
                    && *use_worktree
                    && checkout_strategy.is_none()
                    && *worktree_reuse == protocol::WorktreeReusePolicy::Reuse
                    && existing_worktree.is_none()
            ),
            "the released single target reads back field for field: {:?}",
            spawn.target
        );
    }

    /// The workspace a v0.5.1-alpha `state.json` held, field by field.
    fn assert_v0_5_1_workspace(workspace: &protocol::WorkspaceEntry) {
        assert_eq!(workspace.id, "ws-a");
        assert_eq!(workspace.name, "Alpha Workspace");
        assert_eq!(
            workspace.member_repo_ids,
            vec!["repo-a".to_owned(), "repo-b".to_owned()]
        );
        assert_eq!(
            workspace.linked_vscode_workspace.as_deref(),
            Some(r"X:\dev\alpha\alpha.code-workspace")
        );
        assert!(workspace.watch_vscode_workspace);
        assert!(!workspace.default_use_worktree);
        assert_eq!(
            workspace.appearance.terminal_font_family.as_deref(),
            Some("Fira Code")
        );
        assert_eq!(workspace.appearance.terminal_font_size, Some(13));
        assert_eq!(workspace.appearance.terminal_font_bold, Some(false));
        let spawn = workspace
            .last_spawn_config
            .as_ref()
            .expect("the workspace keeps its last spawn config");
        assert_eq!(spawn.mode, protocol::SessionMode::Headless);
        assert!(spawn.dangerously_skip_permissions);
        assert_eq!(
            spawn.agent.as_ref().map(|selection| &selection.options),
            Some(&protocol::OptionValues::from([
                (
                    "model".to_owned(),
                    protocol::OptionValue::Text("sonnet".to_owned())
                ),
                (
                    "permission_mode".to_owned(),
                    protocol::OptionValue::Text("bypass_permissions".to_owned())
                ),
            ])),
            "the headless config's trusted flag loads as the bypass choice"
        );
        assert!(
            matches!(
                &spawn.target,
                protocol::SpawnTarget::Workspace {
                    workspace_id,
                    branch_name,
                    existing_worktrees,
                    ..
                } if workspace_id == "ws-a"
                    && branch_name == "feat/alpha"
                    && existing_worktrees.len() == 1
                    && existing_worktrees[0].repo_id == "repo-a"
                    && existing_worktrees[0].path == r"X:\dev\wt\alpha"
            ),
            "the released workspace target keeps its pinned member: {:?}",
            spawn.target
        );
    }

    /// The tab layouts a v0.5.1-alpha `state.json` held, field by field.
    fn assert_v0_5_1_layout(persisted: &PersistedState) {
        let layout = persisted
            .layouts
            .get("client-1")
            .expect("the client's layout is kept");
        assert_eq!(layout.name.as_deref(), Some("leftos-desktop"));
        assert_eq!(layout.tabs.len(), 1);
        let tab = &layout.tabs[0];
        assert_eq!(tab.id, "tab-1");
        assert_eq!(tab.name, "Alpha");
        assert_eq!(tab.created_at.to_rfc3339(), "2025-01-02T03:04:05+00:00");
        assert_eq!(
            tab.picked_layout,
            Some(protocol::RearrangeLayout::Grid {
                cols: 2,
                aspect: Some(1.5)
            })
        );
        assert!(
            matches!(
                &tab.content,
                protocol::TabContent::Grid {
                    grid: protocol::GridNode::Pane { pane_id, session_id, .. }
                } if pane_id == "pane-1" && session_id.as_deref() == Some("sess-1")
            ),
            "the tab's pane and session are kept: {:?}",
            tab.content
        );
        assert_eq!(
            persisted.legacy_tabs.len(),
            1,
            "the pre-per-client tabs field keeps its tab"
        );
        assert_eq!(persisted.legacy_tabs[0].id, "tab-legacy");
    }

    /// The host settings a v0.5.1-alpha `state.json` held, field by field.
    fn assert_v0_5_1_host_settings(persisted: &PersistedState) {
        assert_eq!(
            persisted.container_order,
            vec![
                protocol::ContainerRef::Repo("repo-a".to_owned()),
                protocol::ContainerRef::Workspace("ws-a".to_owned())
            ]
        );
        assert_eq!(
            persisted.session_order.get("repo-a"),
            Some(&vec!["sess-1".to_owned(), "sess-2".to_owned()])
        );
        assert_eq!(
            persisted.worktrees_root_override.as_deref(),
            Some(r"X:\dev\wt")
        );
        assert_eq!(persisted.keep_awake_mode, protocol::KeepAwakeMode::Always);
        assert!(
            !persisted.detailed_logging,
            "the oldest release's file logs at the default level"
        );
        assert!(
            persisted.default_shell.is_none(),
            "the oldest release's file runs the Automatic shell"
        );
        assert_eq!(
            persisted
                .agent_commands
                .get(&protocol::AgentId("claude".to_owned())),
            Some(&AgentCommand {
                program: r"C:\Tools\claude.cmd".to_owned(),
                args: "--verbose".to_owned()
            })
        );
        assert_eq!(
            persisted
                .agent_settings
                .get(&protocol::AgentId("claude".to_owned()))
                .and_then(|values| values.get("model")),
            Some(&protocol::OptionValue::Text("opus".to_owned()))
        );
        assert_eq!(
            persisted.summarizer,
            Some(protocol::SummarizerSettings {
                enabled: true,
                provider: "deepseek".to_owned(),
                key_source: protocol::SummarizerKeySource::Env {
                    variable: "DEEPSEEK_API_KEY".to_owned()
                },
                daily_cap_usd: 2.5
            })
        );
    }

    /// A `state.json` written by the oldest release users may still run keeps
    /// every value it set: the repos, workspaces, layouts and host settings
    /// are read through the daemon's own load path.
    #[test]
    fn state_json_written_by_v0_5_1_alpha_loads_with_every_set_value_kept() {
        let dirs = scratch_dirs("v051-shape");
        std::fs::write(&dirs.state_file, V0_5_1_STATE_JSON).expect("write the v0.5.1 state.json");

        let state = AppState::load_or_default(&dirs).expect("the v0.5.1 state.json loads");
        assert!(!state.state_file_corrupt(), "the released file parses");
        assert!(
            corrupt_copies(&dirs.config).is_empty(),
            "nothing was quarantined"
        );
        let persisted = state.with_persisted(PersistedState::clone);

        assert_eq!(persisted.repos.len(), 1, "the repo is kept");
        assert_v0_5_1_repo(&persisted.repos[0]);
        assert!(
            persisted.repos[0].is_git,
            "a repo saved before is_git existed reads as git"
        );
        assert_eq!(persisted.workspaces.len(), 1, "the workspace is kept");
        assert_v0_5_1_workspace(&persisted.workspaces[0]);
        assert_v0_5_1_layout(&persisted);
        assert_v0_5_1_host_settings(&persisted);
        let rewritten =
            std::fs::read_to_string(&dirs.state_file).expect("read the rewritten state.json");
        assert!(
            rewritten.contains("\"bypass_permissions\""),
            "the folded bypass choice is written back: {rewritten}"
        );
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    /// A v0.5.1-alpha repo whose folder holds no `.git` loads as git, and the
    /// first refresh corrects it without touching its worktree default.
    #[test]
    fn v0_5_1_alpha_repo_without_git_is_corrected_by_the_first_refresh() {
        let dirs = scratch_dirs("v051-plain");
        let folder = dirs.config.join("alpha");
        std::fs::create_dir_all(&folder).expect("create the plain folder");
        let folder_json =
            serde_json::to_string(&folder.to_string_lossy()).expect("encode the folder path");
        let shaped = V0_5_1_STATE_JSON.replacen(
            r#""path": "X:\\dev\\alpha""#,
            &format!(r#""path": {folder_json}"#),
            1,
        );
        assert_ne!(shaped, V0_5_1_STATE_JSON, "the repo path was rewritten");
        std::fs::write(&dirs.state_file, shaped).expect("write the v0.5.1 state.json");

        let state = AppState::load_or_default(&dirs).expect("the v0.5.1 state.json loads");
        assert!(
            state.with_persisted(|s| s.repos[0].is_git),
            "an absent is_git reads as git"
        );

        assert!(
            crate::registry::refresh_is_git(&state),
            "the flag is corrected"
        );
        let repo = state.with_persisted(|s| s.repos[0].clone());
        assert!(!repo.is_git, "a folder without .git is plain: {repo:?}");
        assert!(
            !repo.default_use_worktree,
            "the saved worktree default is kept"
        );
        let _ = std::fs::remove_dir_all(&dirs.config);
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
    fn keep_awake_mode_defaults_to_when_busy_and_round_trips() {
        let json = r#"{"repos": [], "workspaces": []}"#;
        let old: PersistedState = serde_json::from_str(json).expect("parse old state");
        assert_eq!(
            old.keep_awake_mode,
            KeepAwakeMode::WhenBusy,
            "a state.json without the setting loads as When busy"
        );

        let dirs = scratch_dirs("keepawake");
        assert!(!dirs.state_file.exists(), "scratch dir starts empty");
        let state = AppState::load_or_default(&dirs).expect("load state");
        assert_eq!(
            state.keep_awake_mode(),
            KeepAwakeMode::WhenBusy,
            "a fresh install holds the machine awake while a session is busy"
        );
        state
            .set_keep_awake_mode(KeepAwakeMode::Always)
            .expect("persist the mode");
        assert_eq!(state.keep_awake_mode(), KeepAwakeMode::Always);

        let reloaded = AppState::load_or_default(&dirs).expect("reload state");
        assert_eq!(
            reloaded.keep_awake_mode(),
            KeepAwakeMode::Always,
            "the mode survives a reload"
        );
        let text = std::fs::read_to_string(&dirs.state_file).expect("read state.json");
        assert!(
            text.contains("\"keep_awake_mode\""),
            "stored as keep_awake_mode"
        );
        assert!(!text.contains("\"keep_awake\""), "the boolean key is gone");

        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn detailed_logging_defaults_off_in_an_old_state_json() {
        let old: PersistedState =
            serde_json::from_str(r#"{"repos": [], "workspaces": []}"#).expect("parse old state");
        assert!(
            !old.detailed_logging,
            "a state.json without the setting logs at the default level"
        );
        let released: PersistedState =
            serde_json::from_str(V0_5_1_STATE_JSON).expect("parse the v0.5.1 state.json");
        assert!(
            !released.detailed_logging,
            "the oldest release's state.json logs at the default level"
        );
    }

    #[test]
    fn default_shell_reads_automatic_in_an_old_state_json() {
        let old: PersistedState =
            serde_json::from_str(r#"{"repos": [], "workspaces": []}"#).expect("parse old state");
        assert!(
            old.default_shell.is_none(),
            "a state.json without the setting runs the Automatic shell"
        );
        let released: PersistedState =
            serde_json::from_str(V0_5_1_STATE_JSON).expect("parse the v0.5.1 state.json");
        assert!(
            released.default_shell.is_none(),
            "the oldest release's state.json runs the Automatic shell"
        );
    }

    #[test]
    fn default_shell_round_trips_and_none_removes_the_key() {
        let dirs = scratch_dirs("default-shell");
        assert!(!dirs.state_file.exists(), "scratch dir starts empty");
        let state = AppState::load_or_default(&dirs).expect("load state");
        assert_eq!(state.default_shell(), None, "a fresh install is Automatic");
        let set = protocol::DefaultShell {
            id: "custom".to_owned(),
            command: Some("bash -l".to_owned()),
        };
        state
            .set_default_shell(Some(set.clone()))
            .expect("persist the shell");
        assert_eq!(state.default_shell(), Some(set.clone()));

        let reloaded = AppState::load_or_default(&dirs).expect("reload state");
        assert_eq!(
            reloaded.default_shell(),
            Some(set),
            "the shell survives a reload"
        );
        let text = std::fs::read_to_string(&dirs.state_file).expect("read state.json");
        let stored: serde_json::Value = serde_json::from_str(&text).expect("parse state.json");
        assert_eq!(
            stored["default_shell"],
            serde_json::json!({"id": "custom", "command": "bash -l"})
        );

        reloaded.set_default_shell(None).expect("clear the shell");
        assert_eq!(reloaded.default_shell(), None);
        let text = std::fs::read_to_string(&dirs.state_file).expect("read state.json");
        let stored: serde_json::Value = serde_json::from_str(&text).expect("parse state.json");
        assert!(
            stored.get("default_shell").is_none(),
            "Automatic removes the key: {text}"
        );

        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn detailed_logging_round_trips() {
        let dirs = scratch_dirs("detailed-logging");
        assert!(!dirs.state_file.exists(), "scratch dir starts empty");
        let state = AppState::load_or_default(&dirs).expect("load state");
        assert!(
            !state.detailed_logging(),
            "a fresh install logs at the default level"
        );
        state
            .set_detailed_logging(true)
            .expect("persist the switch");
        assert!(state.detailed_logging());

        let reloaded = AppState::load_or_default(&dirs).expect("reload state");
        assert!(reloaded.detailed_logging(), "the switch survives a reload");
        let text = std::fs::read_to_string(&dirs.state_file).expect("read state.json");
        assert!(
            text.contains("\"detailed_logging\": true"),
            "state.json holds the switch: {text}"
        );

        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn agent_commands_default_empty_round_trip_and_blank_removes() {
        let json = r#"{"repos": [], "workspaces": []}"#;
        let old: PersistedState = serde_json::from_str(json).expect("parse old state");
        assert!(
            old.agent_commands.is_empty(),
            "a state.json without the setting has no launch commands"
        );

        let dirs = scratch_dirs("agentcommands");
        let state = AppState::load_or_default(&dirs).expect("load state");
        assert_eq!(
            state.agent_command(&agent_registry::default_agent()),
            AgentCommand::default()
        );
        let set = AgentCommand {
            program: "claude-wrapper".to_owned(),
            args: "--debug \"a b\"".to_owned(),
        };
        state
            .set_agent_command(&agent_registry::default_agent(), set.clone())
            .expect("persist the command");
        assert_eq!(state.agent_command(&agent_registry::default_agent()), set);

        let reloaded = AppState::load_or_default(&dirs).expect("reload state");
        assert_eq!(
            reloaded.agent_command(&agent_registry::default_agent()),
            set,
            "the command survives a reload"
        );
        let text = std::fs::read_to_string(&dirs.state_file).expect("read state.json");
        let stored: serde_json::Value = serde_json::from_str(&text).expect("parse state.json");
        assert_eq!(
            stored["agent_commands"],
            serde_json::json!({"claude": {"program": "claude-wrapper", "args": "--debug \"a b\""}}),
            "keyed by the agent's serde name"
        );

        reloaded
            .set_agent_command(
                &agent_registry::default_agent(),
                AgentCommand {
                    program: "  ".to_owned(),
                    args: String::new(),
                },
            )
            .expect("clear the command");
        assert_eq!(
            reloaded.agent_command(&agent_registry::default_agent()),
            AgentCommand::default()
        );
        let text = std::fs::read_to_string(&dirs.state_file).expect("read state.json");
        let stored: serde_json::Value = serde_json::from_str(&text).expect("parse state.json");
        assert_eq!(
            stored["agent_commands"],
            serde_json::json!({}),
            "a blank pair removes the agent's entry"
        );

        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn agent_settings_round_trip_and_are_omitted_when_empty() {
        let dirs = scratch_dirs("agentsettings");
        let state = AppState::load_or_default(&dirs).expect("load state");
        assert!(state.agent_settings().is_empty(), "a fresh state has none");
        state
            .set_keep_awake_mode(KeepAwakeMode::Always)
            .expect("persist something else");
        let text = std::fs::read_to_string(&dirs.state_file).expect("read state.json");
        assert!(
            !text.contains("\"agent_settings\""),
            "empty settings are omitted from state.json"
        );

        let agent = agent_registry::default_agent();
        let options = OptionValues::from([(
            "provider".to_owned(),
            protocol::OptionValue::Text("deepseek".to_owned()),
        )]);
        state
            .set_agent_settings(&agent, options.clone())
            .expect("persist the settings");
        let reloaded = AppState::load_or_default(&dirs).expect("reload state");
        assert_eq!(
            reloaded.agent_settings(),
            BTreeMap::from([(agent.clone(), options.clone())]),
            "the settings survive a reload"
        );
        let text = std::fs::read_to_string(&dirs.state_file).expect("read state.json");
        let stored: serde_json::Value = serde_json::from_str(&text).expect("parse state.json");
        assert_eq!(
            stored["agent_settings"],
            serde_json::json!({ "claude": { "provider": "deepseek" } }),
            "stored keyed by agent id"
        );

        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn summarizer_settings_default_and_round_trip() {
        let dirs = scratch_dirs("summarizersettings");
        let state = AppState::load_or_default(&dirs).expect("load state");
        let default = state.summarizer_settings();
        assert_eq!(
            default,
            protocol::SummarizerSettings {
                enabled: false,
                provider: agent_registry::default_agent().0,
                key_source: protocol::SummarizerKeySource::Stored,
                daily_cap_usd: 1.0,
            },
            "the registered CLI summarizer, off until the user turns it on, with a $1.00 cap"
        );
        state
            .set_keep_awake_mode(KeepAwakeMode::Always)
            .expect("persist something else");
        let text = std::fs::read_to_string(&dirs.state_file).expect("read state.json");
        assert!(
            !text.contains("\"summarizer\""),
            "unsaved settings are omitted from state.json"
        );

        let chosen = protocol::SummarizerSettings {
            enabled: false,
            provider: "deepseek".to_owned(),
            key_source: protocol::SummarizerKeySource::Env {
                variable: "DEEPSEEK_API_KEY".to_owned(),
            },
            daily_cap_usd: 2.5,
        };
        state
            .set_summarizer_settings(chosen.clone())
            .expect("persist the settings");
        let reloaded = AppState::load_or_default(&dirs).expect("reload state");
        assert_eq!(
            reloaded.summarizer_settings(),
            chosen,
            "the settings survive a reload"
        );

        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    /// A `state.json` that carries no `summarizer` block reads as off: a
    /// summary spends the chosen provider's account, so the owner made it
    /// opt-in, and a user who never saved the settings gets it off.
    #[test]
    fn a_state_file_without_a_summarizer_block_reads_summaries_off() {
        let dirs = scratch_dirs("no-summarizer-block");
        let json = r#"{
            "repos": [
                {
                    "id": "repo-a",
                    "name": "Alpha",
                    "path": "X:\\dev\\alpha",
                    "default_branch": "trunk"
                }
            ],
            "workspaces": [],
            "layouts": {
                "client-1": {
                    "name": "leftos-desktop",
                    "tabs": [
                        {
                            "id": "tab-1",
                            "name": "Alpha",
                            "content": {
                                "kind": "grid",
                                "grid": {"kind": "pane", "pane_id": "pane-1", "session_id": "sess-1"}
                            },
                            "created_at": "2025-01-02T03:04:05Z"
                        }
                    ]
                }
            }
        }"#;
        std::fs::write(&dirs.state_file, json).expect("write the v0.5.1 state.json");

        let state = AppState::load_or_default(&dirs).expect("the v0.5.1 state.json loads");
        assert!(!state.state_file_corrupt(), "the released file parses");
        assert!(
            !state.summarizer_settings().enabled,
            "summaries the user never turned on stay off"
        );
        let persisted = state.with_persisted(PersistedState::clone);
        assert_eq!(persisted.repos.len(), 1, "the repo it held is still read");
        assert_eq!(persisted.repos[0].name, "Alpha");
        assert_eq!(
            persisted.layouts.get("client-1").map(|l| l.tabs.len()),
            Some(1),
            "the layout it held is still read"
        );
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn no_key_text_reaches_state_json() {
        let dirs = scratch_dirs("summarizernokey");
        let state = AppState::load_or_default(&dirs).expect("load state");
        state
            .set_summarizer_settings(protocol::SummarizerSettings {
                enabled: true,
                provider: "deepseek".to_owned(),
                key_source: protocol::SummarizerKeySource::Env {
                    variable: "DEEPSEEK_API_KEY".to_owned(),
                },
                daily_cap_usd: 1.0,
            })
            .expect("persist the settings");
        let text = std::fs::read_to_string(&dirs.state_file).expect("read state.json");
        let stored: serde_json::Value = serde_json::from_str(&text).expect("parse state.json");
        let block = stored["summarizer"]
            .as_object()
            .expect("the summarizer block is stored");
        let mut fields: Vec<&str> = block.keys().map(String::as_str).collect();
        fields.sort_unstable();
        assert_eq!(
            fields,
            ["daily_cap_usd", "enabled", "key_source", "provider"],
            "the block has no field that could hold a key"
        );
        assert_eq!(
            stored["summarizer"]["key_source"],
            serde_json::json!({ "kind": "env", "variable": "DEEPSEEK_API_KEY" }),
            "an env source names the variable, never its value"
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
    fn a_viewer_tab_survives_a_state_reload() {
        let dirs = scratch_dirs("viewer-tab");
        let state = AppState::load_or_default(&dirs).expect("load state");
        let tab = TabEntry {
            id: "v".to_string(),
            name: "a.md".to_string(),
            content: TabContent::Viewer {
                path: "C:\\notes\\a.md".to_string(),
            },
            created_at: chrono::Utc::now(),
            picked_layout: None,
        };
        state
            .set_client_layout(
                "desktop",
                Some("desktop-host".to_string()),
                vec![tab.clone()],
            )
            .expect("set layout");

        let reloaded = AppState::load_or_default(&dirs).expect("reload state");
        assert_eq!(reloaded.client_layout("desktop"), vec![tab]);

        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn try_mutate_ok_commits_and_writes() {
        let dirs = scratch_dirs("try-ok");
        let state = AppState::load_or_default(&dirs).expect("load state");
        state
            .set_keep_awake_mode(KeepAwakeMode::Off)
            .expect("seed state.json");

        state
            .try_mutate(|s| {
                s.keep_awake_mode = KeepAwakeMode::Always;
                s.legacy_tabs.push(sample_tab("committed"));
                Ok(())
            })
            .expect("a closure that returns Ok commits");

        assert_eq!(
            state.keep_awake_mode(),
            KeepAwakeMode::Always,
            "the clone's change is live"
        );
        assert_eq!(state.legacy_tabs().len(), 1, "and so is its tab");

        let reloaded = AppState::load_or_default(&dirs).expect("reload state");
        assert_eq!(
            reloaded.keep_awake_mode(),
            KeepAwakeMode::Always,
            "the change reached state.json"
        );
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
        state
            .set_keep_awake_mode(KeepAwakeMode::Off)
            .expect("seed state.json");
        let before_bytes = std::fs::read(&dirs.state_file).expect("read state.json");
        assert!(before_bytes.len() > 2, "the seeded state.json has content");

        let err = state
            .try_mutate(|s| -> anyhow::Result<()> {
                s.keep_awake_mode = KeepAwakeMode::Always;
                s.legacy_tabs.push(sample_tab("rolled-back"));
                anyhow::bail!("closure failed after mutating");
            })
            .expect_err("the closure's error propagates");
        assert_eq!(err.to_string(), "closure failed after mutating");

        assert_eq!(
            state.keep_awake_mode(),
            KeepAwakeMode::Off,
            "the in-memory change was rolled back"
        );
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

    #[test]
    fn a_corrupt_state_file_is_kept_beside_a_fresh_one() {
        let dirs = scratch_dirs("corrupt-kept");
        let bad = b"{ this is not json".to_vec();
        std::fs::write(&dirs.state_file, &bad).expect("seed a corrupt state.json");

        let state = AppState::load_or_default(&dirs).expect("load state");
        assert!(state.state_file_corrupt(), "the flag stays true");
        assert!(
            state.with_persisted(|persisted| persisted.repos.is_empty()),
            "the process runs on a fresh default"
        );
        let copies = corrupt_copies(&dirs.config);
        assert_eq!(copies.len(), 1, "exactly one copy: {copies:?}");
        assert_eq!(
            std::fs::read(&copies[0]).expect("read the copy"),
            bad,
            "the copy holds the original bytes"
        );

        state
            .set_keep_awake_mode(KeepAwakeMode::Always)
            .expect("persist the fresh defaults");
        let saved: PersistedState =
            serde_json::from_slice(&std::fs::read(&dirs.state_file).expect("read state.json"))
                .expect("the fresh state.json parses");
        assert!(
            saved.repos.is_empty() && saved.workspaces.is_empty(),
            "state.json holds the fresh defaults"
        );
        assert_eq!(saved.keep_awake_mode, KeepAwakeMode::Always);
        assert_eq!(
            std::fs::read(&copies[0]).expect("read the copy again"),
            bad,
            "the save leaves the copy untouched"
        );

        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn only_the_three_newest_corrupt_copies_are_kept() {
        let dirs = scratch_dirs("corrupt-prune");
        let now = chrono::Utc::now();
        // `older[0]` is the newest of the four seeded copies.
        let older: Vec<PathBuf> = (1..=4)
            .map(|days| {
                let stamp = (now - chrono::Duration::days(days))
                    .format("%Y%m%dT%H%M%SZ")
                    .to_string();
                let path = dirs.config.join(format!("state.json.corrupt-{stamp}"));
                std::fs::write(&path, b"an older copy").expect("seed an older copy");
                path
            })
            .collect();
        std::fs::write(&dirs.state_file, b"not json either").expect("seed a corrupt state.json");

        AppState::load_or_default(&dirs).expect("load state");

        let copies = corrupt_copies(&dirs.config);
        assert_eq!(copies.len(), 3, "three copies are kept: {copies:?}");
        assert!(
            older[0].exists() && older[1].exists(),
            "the two next-newest seeded copies stay"
        );
        assert!(
            !older[2].exists() && !older[3].exists(),
            "the two oldest copies go"
        );
        assert!(
            copies
                .iter()
                .any(|copy| std::fs::read(copy).expect("read a copy") == b"not json either"),
            "the copy just made is among the kept ones"
        );

        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    /// The v0.5.1-alpha `state.json` as a mutable JSON tree, for the tests that
    /// damage one part of it and check what salvage still keeps.
    fn v0_5_1_tree() -> serde_json::Value {
        serde_json::from_str(V0_5_1_STATE_JSON).expect("parse the v0.5.1 state.json")
    }

    #[test]
    fn a_bad_tab_keeps_the_repos_and_the_other_tabs() {
        let mut tree = v0_5_1_tree();
        let tabs = tree["layouts"]["client-1"]["tabs"]
            .as_array_mut()
            .expect("the layout holds a tabs array");
        let mut broken = tabs[0].clone();
        broken["id"] = serde_json::json!("tab-2");
        broken["content"]["grid"]["kind"] = serde_json::json!("nonsense");
        tabs.push(broken);

        let state = salvage_state(tree.to_string().as_bytes());

        assert_eq!(state.repos.len(), 1, "the repo is kept");
        assert_eq!(state.workspaces.len(), 1, "the workspace is kept");
        let layout = state.layouts.get("client-1").expect("the layout is kept");
        assert_eq!(layout.tabs.len(), 1, "only the good tab is kept");
        assert_eq!(layout.tabs[0].id, "tab-1", "the good tab is the one kept");
    }

    #[test]
    fn a_bad_repo_entry_keeps_the_other_repos() {
        let mut tree = v0_5_1_tree();
        tree["repos"]
            .as_array_mut()
            .expect("repos is an array")
            .push(serde_json::json!({"id": 3}));

        let state = salvage_state(tree.to_string().as_bytes());

        assert_eq!(state.repos.len(), 1, "one repo is kept");
        assert_eq!(state.repos[0].id, "repo-a", "the good repo is the one kept");
        assert_eq!(state.workspaces.len(), 1, "the workspace is kept");
    }

    #[test]
    fn a_bad_workspace_entry_keeps_the_other_workspaces() {
        let mut tree = v0_5_1_tree();
        tree["workspaces"]
            .as_array_mut()
            .expect("workspaces is an array")
            .push(serde_json::json!({"id": 3}));

        let state = salvage_state(tree.to_string().as_bytes());

        assert_eq!(state.workspaces.len(), 1, "one workspace is kept");
        assert_eq!(
            state.workspaces[0].id, "ws-a",
            "the good workspace is the one kept"
        );
        assert_eq!(state.repos.len(), 1, "the repo is kept");
    }

    #[test]
    fn a_wrong_typed_field_keeps_the_rest() {
        let mut tree = v0_5_1_tree();
        tree["session_order"] = serde_json::json!(7);

        let state = salvage_state(tree.to_string().as_bytes());

        assert!(state.session_order.is_empty(), "the bad field reads empty");
        assert_eq!(state.repos.len(), 1, "the repo is kept");
        assert_eq!(state.layouts.len(), 1, "the layout is kept");
    }

    #[test]
    fn a_file_missing_repos_keeps_the_rest() {
        let mut tree = v0_5_1_tree();
        tree.as_object_mut()
            .expect("the state is a JSON object")
            .remove("repos");

        let state = salvage_state(tree.to_string().as_bytes());

        assert!(state.repos.is_empty(), "the missing list reads empty");
        assert_eq!(state.workspaces.len(), 1, "the workspace is kept");
        assert_eq!(state.layouts.len(), 1, "the layout is kept");
    }

    #[test]
    fn a_wrong_kind_of_list_reads_as_empty() {
        let mut tree = v0_5_1_tree();
        tree["repos"] = serde_json::json!(3);
        tree["layouts"] = serde_json::json!([]);

        let state = salvage_state(tree.to_string().as_bytes());

        assert!(state.repos.is_empty(), "a repos number reads empty");
        assert!(state.layouts.is_empty(), "a layouts array reads empty");
        assert_eq!(state.workspaces.len(), 1, "the workspace is kept");
    }

    #[test]
    fn a_layout_that_is_not_an_object_is_dropped() {
        let mut tree = v0_5_1_tree();
        tree["layouts"]["other"] = serde_json::json!(3);

        let state = salvage_state(tree.to_string().as_bytes());

        assert_eq!(state.layouts.len(), 1, "only the good layout is kept");
        assert!(
            state.layouts.contains_key("client-1"),
            "the good client's layout is the one kept"
        );
    }

    #[test]
    fn a_bad_legacy_tab_is_dropped() {
        for key in ["legacy_tabs", "tabs"] {
            let mut tree = v0_5_1_tree();
            let fields = tree.as_object_mut().expect("the state is a JSON object");
            let good = fields
                .remove("legacy_tabs")
                .expect("the v0.5.1 state holds a legacy tab");
            let mut tabs = good.as_array().expect("legacy_tabs is an array").clone();
            tabs.push(serde_json::json!({"id": 3}));
            fields.insert(key.to_owned(), serde_json::Value::Array(tabs));

            let state = salvage_state(tree.to_string().as_bytes());

            assert_eq!(state.legacy_tabs.len(), 1, "one tab survives under {key}");
            assert_eq!(
                state.legacy_tabs[0].id, "tab-legacy",
                "the good tab survives under {key}"
            );
        }
    }

    #[test]
    fn bytes_that_are_not_json_give_the_defaults() {
        let state = salvage_state(b"{ not json");

        assert!(state.repos.is_empty(), "no repos");
        assert!(state.layouts.is_empty(), "no layouts");
    }

    #[test]
    fn json_that_is_not_an_object_gives_the_defaults() {
        let state = salvage_state(b"[1,2]");

        assert!(state.repos.is_empty(), "no repos");
        assert!(state.layouts.is_empty(), "no layouts");
    }

    #[test]
    fn a_damaged_file_salvages_keeps_its_copy_and_counts_as_corrupt() {
        let dirs = scratch_dirs("salvage-kept");
        let mut tree = v0_5_1_tree();
        tree["layouts"]["client-1"]["tabs"][0]["content"]["grid"]["kind"] =
            serde_json::json!("nonsense");
        let damaged = tree.to_string();
        std::fs::write(&dirs.state_file, &damaged).expect("seed a damaged state.json");

        let state = AppState::load_or_default(&dirs).expect("load state");

        assert!(state.state_file_corrupt(), "the flag stays true");
        assert!(
            state.with_persisted(|persisted| persisted.repos.len() == 1),
            "the repo is salvaged"
        );
        let copies = corrupt_copies(&dirs.config);
        assert_eq!(copies.len(), 1, "exactly one copy: {copies:?}");
        assert_eq!(
            std::fs::read(&copies[0]).expect("read the copy"),
            damaged.as_bytes(),
            "the copy holds the original bytes"
        );

        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn a_wrong_typed_layout_name_keeps_its_tabs() {
        let mut tree = v0_5_1_tree();
        tree["layouts"]["client-1"]["name"] = serde_json::json!(7);

        let state = salvage_state(tree.to_string().as_bytes());

        let layout = state.layouts.get("client-1").expect("the layout is kept");
        assert_eq!(layout.tabs.len(), 1, "the layout keeps its tab");
        assert_eq!(layout.tabs[0].id, "tab-1", "the good tab is kept");
        assert!(layout.name.is_none(), "the bad name is dropped");
    }

    #[test]
    fn a_salvage_is_on_disk_when_the_load_returns() {
        let dirs = scratch_dirs("salvage-written");
        let mut tree = v0_5_1_tree();
        tree["layouts"]["client-1"]["tabs"][0]["content"]["grid"]["kind"] =
            serde_json::json!("nonsense");
        let damaged = tree.to_string();
        std::fs::write(&dirs.state_file, &damaged).expect("seed a damaged state.json");

        let state = AppState::load_or_default(&dirs).expect("load state");

        assert!(state.state_file_corrupt(), "the flag stays true");
        let saved: PersistedState = serde_json::from_slice(
            &std::fs::read(&dirs.state_file).expect("the salvage is written before load returns"),
        )
        .expect("the salvaged state.json parses");
        assert_eq!(saved.repos.len(), 1, "the salvaged repo is on disk");
        let copies = corrupt_copies(&dirs.config);
        assert_eq!(copies.len(), 1, "exactly one copy: {copies:?}");
        assert_eq!(
            std::fs::read(&copies[0]).expect("read the copy"),
            damaged.as_bytes(),
            "the copy holds the original bytes"
        );

        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn the_new_copy_survives_pruning_even_when_older_copies_sort_after_it() {
        let dirs = scratch_dirs("corrupt-keep-new");
        // Three copies whose names sort after the stamp used below, so the
        // three newest by name are those, not the copy about to be made.
        let later: Vec<PathBuf> = ["20990101T000000Z", "20990102T000000Z", "20990103T000000Z"]
            .into_iter()
            .map(|stamp| {
                let path = dirs.config.join(format!("state.json.corrupt-{stamp}"));
                std::fs::write(&path, b"a later copy").expect("seed a later copy");
                path
            })
            .collect();
        let at = chrono::NaiveDate::from_ymd_opt(2026, 10, 29)
            .expect("valid date")
            .and_hms_opt(15, 30, 0)
            .expect("valid time")
            .and_utc();
        let bad = b"the bytes that must be kept".to_vec();
        std::fs::write(&dirs.state_file, &bad).expect("seed a corrupt state.json");

        let kept = keep_corrupt_state(&dirs.state_file, &bad, at).expect("keep the corrupt file");

        assert_eq!(
            std::fs::read(&kept).expect("the copy just made is still there"),
            bad,
            "the copy just made keeps its bytes: {}",
            kept.display()
        );
        assert!(
            later.iter().all(|path| path.exists()),
            "the three newer names are still there"
        );

        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn a_name_clash_gets_a_numbered_suffix() {
        let dirs = scratch_dirs("corrupt-clash");
        let at = chrono::NaiveDate::from_ymd_opt(2026, 10, 29)
            .expect("valid date")
            .and_hms_opt(15, 30, 0)
            .expect("valid time")
            .and_utc();
        let taken = dirs.config.join("state.json.corrupt-20261029T153000Z");
        std::fs::write(&taken, b"an earlier copy").expect("seed the taken name");
        let bad = b"still not json".to_vec();
        std::fs::write(&dirs.state_file, &bad).expect("seed a corrupt state.json");

        let kept = keep_corrupt_state(&dirs.state_file, &bad, at).expect("keep the corrupt file");

        assert_eq!(
            kept.file_name().and_then(|name| name.to_str()),
            Some("state.json.corrupt-20261029T153000Z-2"),
            "the clash takes -2: {}",
            kept.display()
        );
        assert_eq!(std::fs::read(&kept).expect("read the kept copy"), bad);
        assert_eq!(
            std::fs::read(&taken).expect("read the earlier copy"),
            b"an earlier copy".to_vec(),
            "the earlier copy is untouched"
        );
        assert!(!dirs.state_file.exists(), "the bad file moved aside");

        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn a_bad_tab_list_is_put_right_on_load() {
        let dirs = scratch_dirs("bad-tabs");
        let state = json!({
            "repos": [],
            "workspaces": [],
            "layouts": {
                "bad": {
                    "tabs": [
                        {
                            "id": "t1",
                            "name": "Tab",
                            "content": {"kind": "grid", "grid": {
                                "kind": "split",
                                "direction": "horizontal",
                                "ratio": 7.0,
                                "first": {"kind": "pane", "pane_id": "a", "session_id": null},
                                "second": {"kind": "pane", "pane_id": "b", "session_id": null},
                            }},
                            "created_at": "2026-01-01T00:00:00Z",
                        },
                        {
                            "id": "t1",
                            "name": "Again",
                            "content": {"kind": "grid", "grid": {
                                "kind": "pane",
                                "pane_id": "c",
                                "session_id": null,
                            }},
                            "created_at": "2026-01-01T00:00:00Z",
                        }
                    ]
                }
            }
        });
        std::fs::write(&dirs.state_file, state.to_string()).expect("write state.json");

        let loaded = AppState::load_or_default(&dirs).expect("load state");

        assert_eq!(
            loaded.client_layout("bad").len(),
            1,
            "the tab repeating an id is gone from the loaded state"
        );
        assert!(
            !loaded.state_file_corrupt(),
            "a parsing file is not corrupt"
        );
        let saved: Value = serde_json::from_str(
            &std::fs::read_to_string(&dirs.state_file).expect("read state.json"),
        )
        .expect("valid JSON");
        let tabs = saved["layouts"]["bad"]["tabs"]
            .as_array()
            .expect("a tab list");
        assert_eq!(tabs.len(), 1, "the corrected list is on disk: {saved}");
        assert_eq!(
            tabs[0]["content"]["grid"]["ratio"],
            json!(0.95),
            "the saved ratio is in range: {saved}"
        );

        let _ = std::fs::remove_dir_all(&dirs.config);
    }
}
