//! A fake daemon for the native client's UI specs: the root view on a test
//! window, fed daemon messages, read back through the commands it sends.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use chrono::{DateTime, Utc};
use daemon_client::device_tokens::MemoryTokens;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use gpui::{
    App, Bounds, Entity, InputHandler, KeyDownEvent, Keystroke, Modifiers, MouseButton,
    MouseDownEvent, MouseUpEvent, Pixels, Point, ScrollDelta, ScrollWheelEvent, TestAppContext,
    TouchPhase, VisualTestContext, Window, point, px,
};
use protocol::{
    ClientMessage, DaemonMessage, GridNode, KeyState, RepoEntry, SessionSnapshot, SplitDirection,
    SummarizerKey, SummarizerKeySource, SummarizerSettings, TabEntry, WorkspaceEntry,
};
use rustling_tulip_native::{
    Activity, Clock, Connection, ContainerKind, FileFilter, FilePicker, FolderPicker,
    HandshakeInfo, NetCommand, NetDeps, NetEvent, Notifier, NotifyState, OpenFailure,
    OpenLocalFuture, Opener, PairingAction, PairingActionSink, PairingToast, QuitFn, RootDeps,
    RootView, ScPanel, Transport, bind_keys,
    remote_access::Autostart,
    spawn_net,
    target::Target,
    ui_theme::UiTheme,
    update::{AlreadyRunning, LastResult, Ready, Updater},
};
use serde_json::{Value, json};

pub mod live;

pub const PROTOCOL: u32 = 1;

/// The `NetDeps::open_local` of every spec: a plain loopback connection, no
/// daemon proof.
pub fn plain_open() -> Box<dyn Fn(u16, String) -> OpenLocalFuture + Send> {
    Box::new(|port, _| {
        Box::pin(async move {
            tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .map_err(|err| daemon_client::ProvenConnectError::Connect(err.into()))
        })
    })
}

/// The features the fake daemon lists in its `Welcome`, as a current daemon
/// does.
pub fn daemon_features() -> Vec<String> {
    vec![protocol::FEATURE_AGENT_COMMANDS.to_owned()]
}

/// The fake daemon's `Welcome` on `protocol_version`, listing
/// [`daemon_features`].
pub fn welcome(protocol_version: u32) -> DaemonMessage {
    DaemonMessage::Welcome {
        protocol_version,
        supported_versions: vec![protocol_version],
        features: daemon_features(),
        daemon_id: None,
    }
}

/// The fake daemon's `Welcome` on `protocol_version` as [`welcome`] sends
/// it, naming the daemon `daemon_id`.
pub fn welcome_with_id(protocol_version: u32, daemon_id: &str) -> DaemonMessage {
    DaemonMessage::Welcome {
        protocol_version,
        supported_versions: vec![protocol_version],
        features: daemon_features(),
        daemon_id: Some(daemon_id.to_owned()),
    }
}

/// A connection to `target` open on `port`, welcomed with `features`.
fn connected_to(target: Target, port: u16, features: Vec<String>) -> Connection {
    let mut conn = Connection::for_target(target);
    conn.on_connecting();
    conn.on_socket_open(port);
    conn.on_welcome(PROTOCOL, features);
    conn
}

/// A connection open on the fake daemon's port, welcomed with `features`.
fn connected(features: Vec<String>) -> Connection {
    connected_to(Target::Local, 4242, features)
}

/// Whether the rail's side of the window shows what `selector` tags: the
/// panels, the divider, the badge and the source-control panel's parts.
/// Selectors it does not know count as present.
fn side_part_shown(root: &RootView, selector: &str) -> bool {
    let open = !root.sidebar_collapsed();
    match selector {
        "sidebar-panel" => open && root.activity() == Activity::Sessions,
        "sidebar-divider" => open,
        "activity-badge" => root.activity_badge().is_some(),
        "recover-badge" => root.recover_badge().is_some(),
        "needs-you-badge" => root.needs_you_badge().is_some(),
        "needs-you-panel" | "needs-you-header" => open && root.activity() == Activity::NeedsYou,
        "needs-you-empty" => {
            open && root.activity() == Activity::NeedsYou && root.needs_you_rows().is_empty()
        }
        _ if selector.starts_with("needs-you-row-") => {
            open && root.activity() == Activity::NeedsYou
                && root
                    .needs_you_rows()
                    .iter()
                    .any(|row| selector.strip_prefix("needs-you-row-") == Some(&row.session_id))
        }
        "sc-picker-menu" => sc_picker_shown(root),
        _ if selector.starts_with("sc-picker-") => {
            sc_picker_shown(root)
                && root
                    .sc_picker_rows()
                    .iter()
                    .any(|row| row.selector == selector)
        }
        _ if selector.starts_with("sc-") => {
            open && root.activity() == Activity::SourceControl
                && sc_part_shown(&root.source_control_panel(), selector)
        }
        _ => true,
    }
}

/// Whether the picker's menu draws: open, and its button offered, since the
/// menu hangs under the button.
fn sc_picker_shown(root: &RootView) -> bool {
    root.sc_picker_open() && root.source_control_panel().picker.is_some()
}

/// Whether the layout chooser, the worktree cleanup-failed dialog, the pane
/// dialogs, the empty pane's menu, the Recover dialog or the exit dialog
/// shows `selector`: the
/// dialog while it is open, a control while the open dialog has it, and the
/// header's move button on a shown pane with a session. `None` for any
/// other selector.
/// Whether the session menu shows its part `selector` tags: the menu, its
/// state actions, the rows under them, its appearance rows or an open
/// submenu's rows; `None` for a selector of no session menu part.
fn session_menu_part_shown(root: &RootView, selector: &str) -> Option<bool> {
    let selector = selector.to_owned();
    let shown = if selector == "session-menu-accent" || selector.starts_with("accent-") {
        root.appearance_group_rows().contains(&selector)
    } else if selector == "session-menu" {
        root.session_menu().is_some()
    } else if selector.starts_with("menu-") {
        root.menu_rows().contains(&selector)
    } else if selector.starts_with("session-menu-")
        || selector.starts_with("duplicate-")
        || selector.starts_with("move-")
    {
        root.menu_groups().concat().contains(&selector)
    } else {
        return None;
    };
    Some(shown)
}

/// Whether the model shows the toast part `selector` tags: an action
/// button (`toast-action-<id>-<action>`), the × or the card; None when it
/// tags no toast part.
fn toast_part_shown(root: &RootView, selector: &str) -> Option<bool> {
    if let Some((id, action)) = selector
        .strip_prefix("toast-action-")
        .and_then(|rest| rest.split_once('-'))
    {
        return Some(root.toasts().iter().any(|toast| {
            toast.id.to_string() == id && toast.actions.iter().any(|a| a.id == action)
        }));
    }
    let id = selector
        .strip_prefix("toast-close-")
        .or_else(|| selector.strip_prefix("toast-"))?;
    Some(root.toasts().iter().any(|toast| toast.id.to_string() == id))
}

fn modal_part_shown(root: &RootView, selector: &str) -> Option<bool> {
    let has = |controls: Vec<(String, String)>| controls.iter().any(|(c, _)| c == selector);
    let shown = if selector == "layout-chooser" {
        root.layout_chooser_open()
    } else if selector.starts_with("layout-choose-") || selector.starts_with("chooser-") {
        has(root.layout_chooser_controls())
    } else if selector == "cleanup-failed" {
        root.cleanup_failed_open()
    } else if selector.starts_with("cleanup-") {
        has(root.cleanup_failed_controls())
    } else if selector == "pane-close" {
        root.pane_close_open()
    } else if selector.starts_with("pane-close-") {
        has(root.pane_close_controls())
    } else if selector == "move-panes" {
        root.move_panes_open()
    } else if selector.starts_with("move-panes-") {
        has(root.move_panes_controls())
    } else if selector == "empty-pane-menu" {
        root.empty_pane_menu().is_some()
    } else if selector.starts_with("empty-pane-menu-")
        || selector.starts_with("empty-pane-move-")
        || selector == "empty-pane-close"
    {
        root.empty_pane_menu_rows()
            .iter()
            .any(|row| row == selector)
    } else if let Some(pane) = selector.strip_prefix("pane-move-new-tab-") {
        root.active_pane_ids().iter().any(|p| p == pane) && root.pane_session(pane).is_some()
    } else if selector == "undo-shelf" {
        !root.undo_entries().is_empty()
    } else if let Some(id) = selector
        .strip_prefix("undo-entry-")
        .or_else(|| selector.strip_prefix("undo-action-"))
        .or_else(|| selector.strip_prefix("undo-dismiss-"))
    {
        root.undo_entries()
            .iter()
            .any(|entry| entry.id.to_string() == id)
    } else if selector == "worktrees-manager" {
        root.worktrees_manager_open()
    } else if selector.starts_with("worktrees-manager-") {
        root.worktrees_manager_buttons()
            .iter()
            .any(|button| button.selector == selector)
    } else if selector == "recover" {
        root.recover_open()
    } else if selector.starts_with("recover-") && selector != "recover-badge" {
        has(root.recover_controls())
    } else if selector == "workspace-creator" || selector == "workspace-creator-panel" {
        root.workspace_creator_open()
    } else if selector.starts_with("workspace-creator-") {
        has(root.workspace_creator_controls())
    } else if selector == "vscode-prompt" || selector == "vscode-prompt-panel" {
        root.vscode_prompt_open()
    } else if selector.starts_with("vscode-prompt-") {
        has(root.vscode_prompt_controls())
    } else if selector == "repo-remove-dialog" || selector == "repo-remove-panel" {
        root.repo_remove_open()
    } else if selector.starts_with("repo-remove-") {
        has(root.repo_remove_buttons())
    } else if selector == "exit-confirm-dialog" {
        root.exit_dialog_open()
    } else if selector.starts_with("exit-") {
        root.exit_dialog_buttons()
            .iter()
            .any(|(button, _, _)| button == selector)
    } else {
        return None;
    };
    Some(shown)
}

/// Whether the sessions panel's model still shows what a sidebar selector
/// tags, assuming the panel itself is shown: a leaf while an expanded
/// container lists the session, its pill too once the tab list is in, the
/// Unbound or Detached banner while that container is expanded, and the
/// view toggle. `None` for any other selector.
fn sidebar_part_shown(root: &RootView, selector: &str) -> Option<bool> {
    if let Some(shown) = container_banner_part_shown(root, selector) {
        return Some(shown);
    }
    if let Some(shown) = container_row_shown(root, selector) {
        return Some(shown);
    }
    let containers = || {
        root.sidebar_containers()
            .into_iter()
            .filter(|c| !c.collapsed)
    };
    let listed = |id: &str| containers().any(|c| c.leaves.iter().any(|leaf| leaf.id == id));
    if selector == "unbound-banner" {
        Some(containers().any(|c| c.kind == ContainerKind::Unbound))
    } else if selector == "detached-banner" {
        Some(containers().any(|c| c.kind == ContainerKind::Detached))
    } else if selector.starts_with("sidebar-view-") {
        Some(true)
    } else if selector == "sidebar-no-repos" || selector == "sidebar-add-repo" {
        Some(root.no_repos_yet())
    } else if let Some(id) = selector.strip_prefix("leaf-pill-") {
        Some(root.leaf_tab_pill(id).is_some() && listed(id))
    } else if let Some(id) = selector
        .strip_prefix("leaf-resume-")
        .or_else(|| selector.strip_prefix("leaf-dismiss-"))
    {
        let offered = root.leaf_buttons(id).iter().any(|(s, _)| s == selector);
        Some(offered && listed(id))
    } else {
        Some(listed(selector.strip_prefix("leaf-")?))
    }
}

/// The prefixes a sidebar container's key starts with, and the keys that
/// are a whole key on their own.
const CONTAINER_KEY_PREFIXES: &[&str] = &["ws:", "repo:", "standalone:", "cwd:", "tab:"];
const CONTAINER_BARE_KEYS: &[&str] = &["detached", "unbound"];

/// Whether the sidebar lists the container a `container-<key>` row selector
/// names, as the last painted frame may still hold the bounds of a row the
/// sidebar no longer draws. A part of a listed container's row
/// (`container-<key>-name`) is left to the other checks; `None` for any
/// selector that names no container key.
fn container_row_shown(root: &RootView, selector: &str) -> Option<bool> {
    let rest = selector.strip_prefix("container-")?;
    let names_a_key = CONTAINER_KEY_PREFIXES
        .iter()
        .any(|prefix| rest.starts_with(prefix))
        || CONTAINER_BARE_KEYS.contains(&rest);
    if !names_a_key {
        return None;
    }
    let keys: Vec<String> = root
        .sidebar_containers()
        .into_iter()
        .map(|container| container.key)
        .collect();
    if keys.iter().any(|key| key == rest) {
        return Some(true);
    }
    let part_of_listed = keys.iter().any(|key| rest.starts_with(&format!("{key}-")));
    (!part_of_listed).then_some(false)
}

/// Whether the folder banner of the DIR or SH container a
/// `container-<key>-banner`, `-add-repo` or `-add-workspace` selector names
/// is drawn; `None` for any other selector.
fn container_banner_part_shown(root: &RootView, selector: &str) -> Option<bool> {
    let rest = selector.strip_prefix("container-")?;
    let (key, wants_suggestion) = if let Some(key) = rest.strip_suffix("-banner") {
        (key, false)
    } else if let Some(key) = rest.strip_suffix("-add-repo") {
        (key, false)
    } else {
        (rest.strip_suffix("-add-workspace")?, true)
    };
    let Some(path) = banner_path(root, key) else {
        return Some(false);
    };
    Some(!wants_suggestion || root.cwd_workspace_suggestion(&path).is_some())
}

/// The folder an expanded DIR or SH container with `key` draws its banner
/// for.
fn banner_path(root: &RootView, key: &str) -> Option<String> {
    root.sidebar_containers()
        .into_iter()
        .find(|c| c.key == key && !c.collapsed)
        .filter(|c| matches!(c.kind, ContainerKind::Shell | ContainerKind::Dir))
        .and_then(|c| c.path)
}

/// Whether the shown source-control panel draws what `selector` tags.
fn sc_part_shown(panel: &ScPanel, selector: &str) -> bool {
    let section = |id: &str| panel.sections.iter().any(|row| row.id == id);
    if let Some(id) = selector.strip_prefix("sc-section-body-") {
        section(id)
    } else if let Some(id) = selector.strip_prefix("sc-section-") {
        section(id)
    } else {
        match selector {
            "sc-refresh" => panel.refresh,
            "sc-picker" => panel.picker.is_some(),
            "sc-context" => panel.context.is_some(),
            _ => true,
        }
    }
}

/// A scratch directory under the system temp dir, removed on drop.
pub struct TestDir(PathBuf);

impl TestDir {
    pub fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "rt-native-spec-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create the spec's ui dir");
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A clock the spec moves by hand, together with the executor's.
#[derive(Clone)]
pub struct TestClock(Arc<Mutex<Instant>>);

impl TestClock {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(Instant::now())))
    }

    fn clock(&self) -> Clock {
        let now = Arc::clone(&self.0);
        Arc::new(move || *now.lock().expect("test clock lock"))
    }

    fn advance(&self, by: Duration) {
        *self.0.lock().expect("test clock lock") += by;
    }
}

/// A session fixture: an idle interactive claude session with no members.
pub struct SessionBuilder(Value);

pub fn session(id: &str) -> SessionBuilder {
    SessionBuilder(json!({
        "id": id,
        "label": id,
        "kind": "single",
        "members": [],
        "status": "idle",
        "mode": "interactive",
        "started_at": "2026-01-01T00:00:00Z",
        "exit_code": null,
        "metrics": { "input_tokens": 0, "output_tokens": 0, "cost_usd": 0.0, "last_activity_at": null },
        "recent_actions": [],
        "agent": "claude",
    }))
}

impl SessionBuilder {
    fn set(mut self, key: &str, value: Value) -> Self {
        self.0[key] = value;
        self
    }

    /// A plain shell outside every repo, in `cwd`.
    pub fn shell(self, cwd: &str) -> Self {
        self.set("mode", json!("plain_shell"))
            .set("kind", json!("standalone"))
            .set("current_cwd", json!(cwd))
    }

    /// A plain shell with an owner kind, so an unregistered cwd groups as DIR.
    pub fn dir_shell(self, cwd: &str) -> Self {
        self.set("mode", json!("plain_shell"))
            .set("current_cwd", json!(cwd))
    }

    pub fn agent(self, agent: &str) -> Self {
        self.set("agent", json!(agent))
    }

    /// Without an agent, so no summary of it can be written.
    pub fn no_agent(self) -> Self {
        self.set("agent", Value::Null)
    }

    pub fn headless(self) -> Self {
        self.set("mode", json!("headless"))
    }

    pub fn status(self, status: &str) -> Self {
        self.set("status", json!(status))
    }

    /// When the status last changed.
    pub fn status_since(self, at: DateTime<Utc>) -> Self {
        self.set("status_since", json!(at))
    }

    /// With a model-written summary of `kind` (`needs_answer`,
    /// `working_update`, `done_waiting`) made at `updated_at`.
    pub fn summary(self, kind: &str, updated_at: DateTime<Utc>) -> Self {
        self.set(
            "summary",
            json!({ "kind": kind, "headline": "", "lines": [], "updated_at": updated_at, "source": "model" }),
        )
    }

    /// Idle with its turn's background work still running.
    pub fn background(self) -> Self {
        self.set("background", json!(true))
    }

    /// Marked "Don't count as busy".
    pub fn busy_excluded(self) -> Self {
        self.set("busy_excluded", json!(true))
    }

    /// What its agent's hooks last reported it is waiting for.
    pub fn pending_input(self, value: Value) -> Self {
        self.set("pending_input", value)
    }

    pub fn in_repo(self, repo_id: &str) -> Self {
        self.set(
            "members",
            json!([{ "repo_id": repo_id, "repo_name": repo_id, "branch": "main", "worktree_path": "" }]),
        )
    }

    /// Members as `(repo id, branch, worktree path)`, each repo named by
    /// its id.
    pub fn members(self, members: &[(&str, &str, &str)]) -> Self {
        let members: Vec<Value> = members
            .iter()
            .map(|(repo_id, branch, path)| {
                json!({ "repo_id": repo_id, "repo_name": repo_id, "branch": branch, "worktree_path": path })
            })
            .collect();
        self.set("members", Value::Array(members))
    }

    /// A member of `repo_id` on a worktree of its own.
    pub fn worktree(self, repo_id: &str) -> Self {
        self.set(
            "members",
            json!([{ "repo_id": repo_id, "repo_name": repo_id, "branch": "wt/x", "worktree_path": "C:/wt/x" }]),
        )
        .set("has_per_session_worktree", json!(true))
        .set("worktree_paths", json!(["C:/wt/x"]))
    }

    /// Stopped with its child's exit code.
    pub fn exited(self, code: i32) -> Self {
        self.set("status", json!("stopped"))
            .set("exit_code", json!(code))
    }

    /// Left behind by a daemon restart that could not reattach it.
    pub fn abandoned(self) -> Self {
        self.set("status", json!("stopped"))
            .set("is_abandoned", json!(true))
    }

    /// Still running, but its PTY was lost across a daemon restart.
    pub fn orphan(self) -> Self {
        self.set("is_orphan", json!(true))
    }

    /// Parked: stopped and kept in the sidebar to resume.
    pub fn inactive(self) -> Self {
        self.set("status", json!("stopped"))
            .set("is_inactive", json!(true))
    }

    pub fn in_workspace(self, workspace_id: &str) -> Self {
        self.set("kind", json!("workspace"))
            .set("workspace_id", json!(workspace_id))
    }

    /// Named `label` by the user.
    pub fn user_label(self, label: &str) -> Self {
        self.set("user_label", json!(label))
    }

    /// With `label` as the daemon's label.
    pub fn label(self, label: &str) -> Self {
        self.set("label", json!(label))
    }

    /// Launched with approval prompts bypassed.
    pub fn trusted(self) -> Self {
        self.set("elevated_authority", json!(true))
    }

    /// Showing `chips`, as the daemon evaluates them from the session's
    /// agent options.
    pub fn agent_chips(self, chips: Value) -> Self {
        self.set("agent_chips", chips)
    }

    pub fn terminal_title(self, title: &str) -> Self {
        self.set("terminal_title", json!(title))
    }

    pub fn program_name(self, program: &str) -> Self {
        self.set("program_name", json!(program))
    }

    /// The prompt it was running when the daemon went down.
    pub fn last_prompt(self, prompt: &str) -> Self {
        self.set("last_prompt", json!(prompt))
    }

    pub fn cwd(self, cwd: &str) -> Self {
        self.set("current_cwd", json!(cwd))
    }

    /// When the session started.
    pub fn started_at(self, at: DateTime<Utc>) -> Self {
        self.set("started_at", json!(at))
    }

    /// With a model-written summary headed `headline` whose lines are
    /// `lines` as `(text, at)`, newest first.
    pub fn summary_lines(self, headline: &str, lines: &[(&str, DateTime<Utc>)]) -> Self {
        let lines: Vec<Value> = lines
            .iter()
            .map(|(text, at)| json!({ "text": text, "at": at, "source": "model" }))
            .collect();
        let updated_at = lines
            .first()
            .map_or_else(|| json!(Utc::now()), |line| line["at"].clone());
        self.set(
            "summary",
            json!({ "kind": null, "headline": headline, "lines": lines, "updated_at": updated_at, "source": "model" }),
        )
    }

    pub fn build(self) -> SessionSnapshot {
        serde_json::from_value(self.0).expect("session fixture")
    }
}

/// A Dashboard facts fixture: nothing known yet but the session id, and a
/// now line no daemon rule worded.
pub struct FactsBuilder(Value);

pub fn facts(session_id: &str) -> FactsBuilder {
    FactsBuilder(json!({ "session_id": session_id, "now": { "kind": "unknown", "text": "" } }))
}

impl FactsBuilder {
    fn set(mut self, key: &str, value: Value) -> Self {
        self.0[key] = value;
        self
    }

    /// The daemon's now line, e.g. `("running_tool", "Running Bash · 1m: cargo test")`.
    pub fn now(self, kind: &str, text: &str) -> Self {
        self.set("now", json!({ "kind": kind, "text": text }))
    }

    pub fn context(self, percent: u8) -> Self {
        self.set("context_percent", json!(percent))
    }

    pub fn cost(self, usd: f64) -> Self {
        self.set("cost_usd", json!(usd))
    }

    pub fn tokens_in(self, tokens: u64) -> Self {
        self.set("tokens_in", json!(tokens))
    }

    /// Ten one-minute tool-call counts, oldest first.
    pub fn activity(self, counts: &[u16]) -> Self {
        self.set("activity", json!(counts))
    }

    /// The stuck reasons, as the wire spells them.
    pub fn stuck(self, reasons: Value) -> Self {
        self.set("stuck", reasons)
    }

    /// The agent tree below the main agent, as the wire spells it.
    pub fn agents(self, tree: Value) -> Self {
        self.set("agents", tree)
    }

    pub fn last_activity(self, at: DateTime<Utc>) -> Self {
        self.set("last_activity_at", json!(at))
    }

    pub fn issue(self, key: &str) -> Self {
        self.set("issue", json!(key))
    }

    pub fn build(self) -> protocol::SessionFacts {
        serde_json::from_value(self.0).expect("facts fixture")
    }
}

/// A `DashboardFacts` message carrying `facts`, with the day's totals not
/// known yet.
pub fn facts_message(facts: Vec<protocol::SessionFacts>) -> DaemonMessage {
    DaemonMessage::DashboardFacts {
        facts,
        totals: protocol::DayTotals::default(),
        linear_synced_at: None,
    }
}

/// A `SummarizerStatus` fixture: summaries on, provider `p1` with a stored
/// key, nothing spent and the cap not reached.
pub fn summarizer_status() -> SummarizerStatusBuilder {
    SummarizerStatusBuilder {
        enabled: true,
        key: KeyState::Stored,
        cap_reached: false,
    }
}

/// The [`DaemonMessage::SummarizerStatus`] a spec words.
pub struct SummarizerStatusBuilder {
    enabled: bool,
    key: KeyState,
    cap_reached: bool,
}

impl SummarizerStatusBuilder {
    /// With summaries turned on or off.
    pub fn enabled(mut self, on: bool) -> Self {
        self.enabled = on;
        self
    }

    /// With provider `p1`'s key in `state`.
    pub fn key(mut self, state: KeyState) -> Self {
        self.key = state;
        self
    }

    /// With the day's spending cap reached or not.
    pub fn cap_reached(mut self, reached: bool) -> Self {
        self.cap_reached = reached;
        self
    }

    pub fn build(self) -> DaemonMessage {
        let provider = "p1".to_owned();
        DaemonMessage::SummarizerStatus {
            settings: SummarizerSettings {
                enabled: self.enabled,
                provider: provider.clone(),
                key_source: SummarizerKeySource::Stored,
                daily_cap_usd: 1.0,
            },
            providers: Vec::new(),
            keys: vec![SummarizerKey {
                provider,
                state: self.key,
            }],
            spend_today_usd: 0.0,
            cap_reached: self.cap_reached,
        }
    }
}

/// A history fixture: a Claude session in repo `r1` that the daemon lost
/// ten minutes ago, with one conversation to resume.
pub struct HistoryItemBuilder(Value);

pub fn history_item(id: &str) -> HistoryItemBuilder {
    let ended = chrono::Utc::now() - chrono::Duration::minutes(10);
    HistoryItemBuilder(json!({
        "entry": {
            "session_id": id, "label": format!("label-{id}"), "kind": "single",
            "mode": "interactive", "agent": "claude",
            "spawn_config": {
                "target": {
                    "kind": "single", "repo_id": "r1", "branch_name": "main", "base_branch": null,
                    "use_worktree": false, "worktree_reuse": "reuse"
                },
                "mode": "interactive", "dangerously_skip_permissions": false,
                "agent": { "agent": "claude", "options": { "permission_mode": null } }, "extra_env": []
            },
            "members": [{"repo_id": "r1", "repo_name": "r1", "branch": "main", "worktree_path": "D:\\r1"}],
            "workspace_id": null, "primary_cwd": "D:\\r1", "current_cwd": null,
            "program_name": "claude", "started_at": null, "ended_at": ended,
            "end": {"type": "tracer_lost"}, "conversation_id": null, "source": "record",
            "recovered_at": null
        },
        "candidates": [{"id": format!("c-{id}"), "last_active": ended, "title": format!("Chat {id}")}],
        "folder_is_git_repo": true, "folder_repo_id": null
    }))
}

impl HistoryItemBuilder {
    fn entry(mut self, key: &str, value: Value) -> Self {
        self.0["entry"][key] = value;
        self
    }

    /// Ended how `end` says (a `SessionEnd` in its wire form).
    pub fn end(self, end: Value) -> Self {
        self.entry("end", end)
    }

    /// Stopped by the user.
    pub fn stopped(self) -> Self {
        self.end(json!({"type": "stopped_by_user"}))
    }

    /// Run by the `agent` CLI: `"claude"`.
    pub fn agent(self, agent: &str) -> Self {
        self.entry("agent", json!(agent))
    }

    /// The daemon says whether a shell can resume the entry's conversation.
    pub fn shell_resume(mut self, can: bool) -> Self {
        self.0["shell_resume"] = json!(can);
        self
    }

    /// Ended `minutes` ago.
    pub fn ended_minutes_ago(self, minutes: i64) -> Self {
        let at = chrono::Utc::now() - chrono::Duration::minutes(minutes);
        self.entry("ended_at", json!(at))
    }

    /// Already recovered.
    pub fn recovered(self) -> Self {
        let at = chrono::Utc::now() - chrono::Duration::minutes(1);
        self.entry("recovered_at", json!(at))
    }

    /// Conversations `ids`, the first the newest.
    pub fn candidates(mut self, ids: &[&str]) -> Self {
        let candidates: Vec<Value> = (11_i64..)
            .zip(ids)
            .map(|(age, id)| {
                let at = chrono::Utc::now() - chrono::Duration::minutes(age);
                json!({"id": id, "last_active": at, "title": format!("Chat {id}")})
            })
            .collect();
        self.0["candidates"] = Value::Array(candidates);
        self
    }

    /// Known only by its folder `cwd`: no spawn config, no repo member.
    /// `repo_id` is the registered repo at that folder, if any.
    pub fn folder_only(mut self, cwd: &str, git: bool, repo_id: Option<&str>) -> Self {
        self.0["folder_is_git_repo"] = json!(git);
        self.0["folder_repo_id"] = json!(repo_id);
        self.entry("spawn_config", Value::Null)
            .entry("members", json!([]))
            .entry("primary_cwd", json!(cwd))
    }

    /// A plain shell in `cwd` with no conversation.
    pub fn shell(mut self, cwd: &str) -> Self {
        self.0["candidates"] = json!([]);
        self.0["folder_is_git_repo"] = json!(false);
        self.entry("mode", json!("plain_shell"))
            .entry("members", json!([]))
            .entry("primary_cwd", json!(cwd))
    }

    pub fn build(self) -> protocol::SessionHistoryItem {
        serde_json::from_value(self.0).expect("history item fixture")
    }
}

pub fn repo(id: &str, path: &str) -> RepoEntry {
    serde_json::from_value(json!({ "id": id, "name": id, "path": path })).expect("repo fixture")
}

pub fn workspace(id: &str, members: &[&str]) -> WorkspaceEntry {
    serde_json::from_value(json!({ "id": id, "name": id, "member_repo_ids": members }))
        .expect("workspace fixture")
}

pub fn pane(id: &str, session: Option<&str>) -> GridNode {
    GridNode::Pane {
        pane_id: id.to_owned(),
        session_id: session.map(str::to_owned),
        host: None,
    }
}

pub fn split(direction: SplitDirection, first: GridNode, second: GridNode) -> GridNode {
    GridNode::Split {
        direction,
        ratio: 0.5,
        first: Box::new(first),
        second: Box::new(second),
    }
}

pub fn tab(id: &str, grid: &GridNode) -> TabEntry {
    serde_json::from_value(json!({
        "id": id,
        "name": id,
        "content": { "kind": "grid", "grid": grid },
        "created_at": "2026-01-01T00:00:00Z",
    }))
    .expect("tab fixture")
}

/// The tailnet host the mixed-host specs connect beside This PC.
pub const REMOTE: &str = "box.tail1234.ts.net";

/// A pane showing `session` of [`REMOTE`].
pub fn remote_pane(id: &str, session: &str) -> GridNode {
    GridNode::Pane {
        pane_id: id.to_owned(),
        session_id: Some(session.to_owned()),
        host: Some(REMOTE.to_owned()),
    }
}

/// This PC lists `a1`; [`REMOTE`] lists `b1` and `b2`. Tab `t1` splits
/// `a1` (pane `p1`) beside `b1` (pane `p2`); tab `t2` holds `b2` alone
/// (pane `p3`).
pub fn mixed_tabs<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut fixture = Fixture::single(session("a1").build());
    fixture.tabs = vec![
        tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                pane("p1", Some("a1")),
                remote_pane("p2", "b1"),
            ),
        ),
        tab("t2", &remote_pane("p3", "b2")),
    ];
    let mut h = Harness::with(cx, dir, &fixture);
    let mut host = h.host(&Target::tailnet(REMOTE, None));
    host.connect();
    host.send(DaemonMessage::Sessions {
        sessions: vec![session("b1").build(), session("b2").build()],
    });
    h.sent();
    h
}

/// What the fake daemon holds at connect.
#[derive(Default)]
pub struct Fixture {
    pub repos: Vec<RepoEntry>,
    pub workspaces: Vec<WorkspaceEntry>,
    pub sessions: Vec<SessionSnapshot>,
    pub tabs: Vec<TabEntry>,
}

impl Fixture {
    /// `session` alone, in pane `p1` of tab `t1`.
    pub fn single(session: SessionSnapshot) -> Self {
        let grid = pane("p1", Some(&session.id));
        Self {
            sessions: vec![session],
            tabs: vec![tab("t1", &grid)],
            ..Self::default()
        }
    }
}

/// The root view on a test window, connected to a fake daemon.
pub struct Harness<'a> {
    pub cx: &'a mut VisualTestContext,
    pub root: Entity<RootView>,
    events: UnboundedSender<NetEvent>,
    commands: UnboundedReceiver<NetCommand>,
    /// Every target the window opened a connection to, in order.
    opened: Rc<RefCell<Vec<Target>>>,
    clock: TestClock,
    answered: HashSet<String>,
    outbox: Outbox,
    /// How many times the view asked the app to quit.
    quits: Rc<Cell<usize>>,
    /// What the terminals' Ctrl+clicks opened.
    opener: Arc<OpenRecorder>,
    /// The notifications attention events showed.
    notifier: Arc<NotifyRecorder>,
    /// What the folder picker answers, and how often it was asked.
    picker: Rc<PickRecorder>,
    /// The login launch the Remote access page reads and switches.
    autostart: Arc<AutostartRecorder>,
    /// The device tokens the client holds per tailnet host.
    tokens: Arc<MemoryTokens>,
    /// The updater the client checks, installs and reads back through.
    updater: Arc<UpdaterRecorder>,
    /// The fake-daemon ends of every host the window opened after This PC.
    extra_hosts: ExtraHosts,
}

/// A folder picker that answers with the folder a spec set, as the test
/// platform cannot prompt, and records the folder each ask opened at.
#[derive(Default)]
pub struct PickRecorder {
    next: std::cell::RefCell<Option<PathBuf>>,
    next_file: std::cell::RefCell<Option<PathBuf>>,
    asks: Cell<usize>,
    file_asks: Cell<usize>,
    starts: std::cell::RefCell<Vec<Option<PathBuf>>>,
    file_starts: std::cell::RefCell<Vec<Option<PathBuf>>>,
    file_filters: std::cell::RefCell<Vec<FileFilter>>,
    /// While set, a folder ask waits for [`Harness::release_picked_folder`].
    hold: Cell<bool>,
    held: std::cell::RefCell<Option<futures::channel::oneshot::Sender<Option<PathBuf>>>>,
}

impl PickRecorder {
    fn picker(recorder: &Rc<Self>) -> FolderPicker {
        let recorder = Rc::clone(recorder);
        Rc::new(move |_: &mut App, start: Option<PathBuf>| {
            recorder.asks.set(recorder.asks.get() + 1);
            recorder.starts.borrow_mut().push(start);
            if recorder.hold.get() {
                let (answer, answered) = futures::channel::oneshot::channel();
                *recorder.held.borrow_mut() = Some(answer);
                return Box::pin(async move { answered.await.ok().flatten() });
            }
            let picked = recorder.next.borrow().clone();
            Box::pin(async move { picked })
        })
    }

    fn file_picker(recorder: &Rc<Self>) -> FilePicker {
        let recorder = Rc::clone(recorder);
        Rc::new(
            move |_: &mut App, start: Option<PathBuf>, filter: FileFilter| {
                recorder.file_asks.set(recorder.file_asks.get() + 1);
                recorder.file_starts.borrow_mut().push(start);
                recorder.file_filters.borrow_mut().push(filter);
                let picked = recorder.next_file.borrow().clone();
                Box::pin(async move { picked })
            },
        )
    }
}

/// What a Ctrl+click on a terminal link opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Opened {
    Url(String),
    VsCode {
        path: PathBuf,
        line: u32,
        column: u32,
    },
    /// Folders opened as one multi-root VS Code workspace.
    VsCodeFolder(Vec<PathBuf>),
    /// A `.code-workspace` file opened in VS Code.
    VsCodeWorkspace(PathBuf),
    DefaultApp(PathBuf),
    /// Shown selected in its folder.
    Reveal(PathBuf),
}

/// An opener that only records what it was asked to open, as the test
/// platform can open nothing, and reports the mapped network hosts a spec
/// set.
#[derive(Default)]
pub struct OpenRecorder {
    opened: Mutex<Vec<Opened>>,
    mapped_hosts: Mutex<Vec<String>>,
    /// Every VS Code job reports VS Code missing while this is set.
    vscode_missing: Mutex<bool>,
    /// The paths a spec marked gone; every other path exists.
    missing: Mutex<Vec<PathBuf>>,
}

impl OpenRecorder {
    fn record(&self, opened: Opened) {
        self.opened
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(opened);
    }

    /// The failure a VS Code job reports, while a spec made VS Code missing.
    fn vscode_failure(&self) -> Option<OpenFailure> {
        (*self
            .vscode_missing
            .lock()
            .unwrap_or_else(PoisonError::into_inner))
        .then_some(OpenFailure::VsCodeNotFound)
    }
}

impl Opener for OpenRecorder {
    fn url(&self, url: &str) -> Result<(), String> {
        self.record(Opened::Url(url.to_owned()));
        Ok(())
    }

    fn vscode(&self, path: &Path, line: u32, column: u32) -> Result<(), OpenFailure> {
        if let Some(failure) = self.vscode_failure() {
            return Err(failure);
        }
        self.record(Opened::VsCode {
            path: path.to_path_buf(),
            line,
            column,
        });
        Ok(())
    }

    fn vscode_folder(&self, paths: &[PathBuf]) -> Result<(), OpenFailure> {
        if let Some(failure) = self.vscode_failure() {
            return Err(failure);
        }
        self.record(Opened::VsCodeFolder(paths.to_vec()));
        Ok(())
    }

    fn vscode_workspace(&self, path: &Path) -> Result<(), OpenFailure> {
        if let Some(failure) = self.vscode_failure() {
            return Err(failure);
        }
        self.record(Opened::VsCodeWorkspace(path.to_path_buf()));
        Ok(())
    }

    fn default_app(&self, path: &Path) -> Result<(), String> {
        self.record(Opened::DefaultApp(path.to_path_buf()));
        Ok(())
    }

    fn reveal(&self, path: &Path) -> Result<(), String> {
        self.record(Opened::Reveal(path.to_path_buf()));
        Ok(())
    }

    fn mapped_unc_hosts(&self) -> Vec<String> {
        self.mapped_hosts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// No type beyond the client's own list: the specs stay the same on
    /// every machine.
    fn is_dangerous_type(&self, _: &str) -> bool {
        false
    }

    /// A network path exists as it is, so a spec never reaches the network;
    /// any other is looked up on disk.
    fn existing(&self, reading: &Path) -> Option<Result<PathBuf, String>> {
        let text = reading.to_string_lossy();
        if text.starts_with(r"\\") || text.starts_with("//") {
            return Some(Ok(reading.to_path_buf()));
        }
        if !reading.exists() {
            return None;
        }
        Some(
            std::fs::canonicalize(reading)
                .map(|resolved| {
                    let text = resolved.to_string_lossy();
                    PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(&text))
                })
                .map_err(|err| err.to_string()),
        )
    }

    /// Every path exists unless a spec marked it gone; the disk is never
    /// looked at, since the specs name paths no machine has.
    fn path_exists(&self, path: &Path) -> bool {
        !self
            .missing
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .any(|gone| gone == path)
    }
}

/// A notifier that only records the notifications it was asked to show, as
/// a spec must never show a real toast, and reports the Windows state a
/// spec set (on until set). It records the pairing toasts too, and keeps the
/// last one's action sink, so a spec can press its buttons itself.
pub struct NotifyRecorder {
    shown: Mutex<Vec<(String, String)>>,
    pairing: Mutex<Vec<PairingToast>>,
    action: Mutex<Option<PairingActionSink>>,
    state: Mutex<NotifyState>,
}

impl Default for NotifyRecorder {
    fn default() -> Self {
        Self {
            shown: Mutex::new(Vec::new()),
            pairing: Mutex::new(Vec::new()),
            action: Mutex::new(None),
            state: Mutex::new(NotifyState::On),
        }
    }
}

impl NotifyRecorder {
    /// Every pairing toast the client raised, in order.
    #[must_use]
    pub fn pairing_toasts(&self) -> Vec<PairingToast> {
        self.pairing
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Presses the last pairing toast: with a button's argument, or with
    /// `None` for a press on the toast body.
    pub fn activate(&self, arg: Option<&str>) {
        let action = PairingAction::from_activation(arg);
        let sink = self
            .action
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let (Some(sink), Some(action)) = (sink, action) {
            sink(action);
        }
    }
}

impl Notifier for NotifyRecorder {
    fn notify(&self, title: &str, body: &str) {
        self.shown
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((title.to_owned(), body.to_owned()));
    }

    fn notify_pairing(&self, toast: &PairingToast, on_action: PairingActionSink) {
        self.pairing
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(toast.clone());
        *self.action.lock().unwrap_or_else(PoisonError::into_inner) = Some(on_action);
    }

    fn state(&self) -> NotifyState {
        *self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// How the recording updater answers an installer start.
#[derive(Debug, Clone, PartialEq, Eq)]
enum InstallAnswer {
    Started,
    Fails(String),
    AlreadyRunning,
}

/// An updater that answers the checks, installer starts and last result a
/// spec set, and records each call; it reaches no feed and runs nothing.
pub struct UpdaterRecorder {
    /// The release a check offers: its version and the installer file a
    /// `Ready` holds open.
    offer: Mutex<Option<(String, PathBuf)>>,
    /// The error a check fails with, before any offer.
    check_error: Mutex<Option<String>>,
    install: Mutex<InstallAnswer>,
    last_result: Mutex<Option<LastResult>>,
    checks: AtomicUsize,
    installs: Mutex<Vec<PathBuf>>,
    last_result_reads: AtomicUsize,
}

impl Default for UpdaterRecorder {
    fn default() -> Self {
        Self {
            offer: Mutex::new(None),
            check_error: Mutex::new(None),
            install: Mutex::new(InstallAnswer::Started),
            last_result: Mutex::new(None),
            checks: AtomicUsize::new(0),
            installs: Mutex::new(Vec::new()),
            last_result_reads: AtomicUsize::new(0),
        }
    }
}

fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl UpdaterRecorder {
    /// Checks offer `version`, its installer at `installer` (which must
    /// exist), and stop failing.
    pub fn offer(&self, version: &str, installer: &Path) {
        *locked(&self.offer) = Some((version.to_owned(), installer.to_path_buf()));
        *locked(&self.check_error) = None;
    }

    /// Checks find nothing newer.
    pub fn offer_nothing(&self) {
        *locked(&self.offer) = None;
        *locked(&self.check_error) = None;
    }

    /// Checks fail with `error`.
    pub fn fail_checks(&self, error: &str) {
        *locked(&self.check_error) = Some(error.to_owned());
    }

    /// Installer starts fail with `error`.
    pub fn fail_installs(&self, error: &str) {
        *locked(&self.install) = InstallAnswer::Fails(error.to_owned());
    }

    /// Installer starts find another update holding the lock.
    pub fn refuse_installs_as_running(&self) {
        *locked(&self.install) = InstallAnswer::AlreadyRunning;
    }

    /// The last update's outcome, handed out once.
    pub fn set_last_result(&self, result: LastResult) {
        *locked(&self.last_result) = Some(result);
    }

    /// How many checks ran.
    pub fn checks(&self) -> usize {
        self.checks.load(Ordering::SeqCst)
    }

    /// The installer each start was asked to run, oldest first.
    pub fn installs(&self) -> Vec<PathBuf> {
        locked(&self.installs).clone()
    }

    /// How many times the last result was read.
    pub fn last_result_reads(&self) -> usize {
        self.last_result_reads.load(Ordering::SeqCst)
    }

    fn answer_check(&self) -> anyhow::Result<Option<Ready>> {
        if let Some(error) = locked(&self.check_error).clone() {
            anyhow::bail!("{error}");
        }
        let Some((version, path)) = locked(&self.offer).clone() else {
            return Ok(None);
        };
        let version = semver::Version::parse(&version).map_err(|err| {
            anyhow::anyhow!("the spec's version {version:?} is not semver: {err}")
        })?;
        let file = std::fs::File::open(&path)
            .map_err(|err| anyhow::anyhow!("opening {}: {err}", path.display()))?;
        Ok(Some(Ready {
            version,
            path,
            file,
        }))
    }
}

impl Updater for UpdaterRecorder {
    fn check(
        &self,
        _current: &semver::Version,
    ) -> futures::future::BoxFuture<'static, anyhow::Result<Option<Ready>>> {
        use futures::FutureExt as _;
        self.checks.fetch_add(1, Ordering::SeqCst);
        futures::future::ready(self.answer_check()).boxed()
    }

    fn start_installer(&self, ready: &Ready) -> anyhow::Result<()> {
        locked(&self.installs).push(ready.path.clone());
        match locked(&self.install).clone() {
            InstallAnswer::Started => Ok(()),
            InstallAnswer::Fails(error) => Err(anyhow::anyhow!("{error}")),
            InstallAnswer::AlreadyRunning => Err(AlreadyRunning.into()),
        }
    }

    fn read_last_result(&self) -> Option<LastResult> {
        self.last_result_reads.fetch_add(1, Ordering::SeqCst);
        locked(&self.last_result).take()
    }
}

/// A login launch that never touches the registry: it starts off, records
/// each switch the client asks for, and fails the next one with the error a
/// spec queued.
#[derive(Default)]
pub struct AutostartRecorder {
    enabled: Mutex<bool>,
    sets: Mutex<Vec<bool>>,
    next_failure: Mutex<Option<String>>,
}

impl AutostartRecorder {
    /// Every switch the client asked for, in order.
    pub fn sets(&self) -> Vec<bool> {
        self.sets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Makes the next switch fail with `error`, leaving the launch as it is.
    pub fn fail_next_set(&self, error: &str) {
        *self
            .next_failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(error.to_owned());
    }
}

impl Autostart for AutostartRecorder {
    fn get(&self) -> Result<bool, String> {
        Ok(*self.enabled.lock().unwrap_or_else(PoisonError::into_inner))
    }

    fn set(&self, enabled: bool) -> Result<(), String> {
        self.sets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(enabled);
        if let Some(error) = self
            .next_failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            return Err(error);
        }
        *self.enabled.lock().unwrap_or_else(PoisonError::into_inner) = enabled;
        Ok(())
    }
}

/// What the client sent, drained from its channel: the messages
/// [`Harness::sent`] has not returned yet, the other commands
/// [`Harness::commands`] has not, and the ids of its `LoadScrollback`
/// requests by session, in the order sent.
#[derive(Default)]
pub struct Outbox {
    unread: Vec<ClientMessage>,
    commands: Vec<NetCommand>,
    scrollback_ids: HashMap<String, Vec<String>>,
    preset_ids: Vec<String>,
}

/// A quit that only counts itself in `quits`, as the test platform's quit
/// does nothing.
pub fn quit_recorder(quits: &Rc<Cell<usize>>) -> QuitFn {
    let quits = Rc::clone(quits);
    Box::new(move |_| quits.set(quits.get() + 1))
}

/// The fake-daemon ends of a connection the window opened after its first:
/// the target it was opened for, the sender its events go on, the receiver
/// its commands arrive on, and what the client sent on it not yet read.
pub struct ExtraHost {
    target: Target,
    events: UnboundedSender<NetEvent>,
    commands: UnboundedReceiver<NetCommand>,
    unread: Vec<ClientMessage>,
    other_commands: Vec<NetCommand>,
}

/// The connections a window opened after its first, in order.
pub type ExtraHosts = Rc<RefCell<Vec<ExtraHost>>>;

/// A transport that hands out `first` on its first call and a fresh pair on
/// any later one, and records every target it is asked for in `opened`.
pub fn fake_transport(
    first: (UnboundedSender<NetCommand>, UnboundedReceiver<NetEvent>),
    opened: &Rc<RefCell<Vec<Target>>>,
) -> Transport {
    fake_transport_sharing(first, opened, &ExtraHosts::default())
}

/// [`fake_transport`], keeping each later pair's fake-daemon ends in
/// `extra_hosts`, where [`Harness::host`] reaches them.
pub fn fake_transport_sharing(
    first: (UnboundedSender<NetCommand>, UnboundedReceiver<NetEvent>),
    opened: &Rc<RefCell<Vec<Target>>>,
    extra_hosts: &ExtraHosts,
) -> Transport {
    let first = RefCell::new(Some(first));
    let opened = Rc::clone(opened);
    let extra_hosts = Rc::clone(extra_hosts);
    Rc::new(move |target: &Target| {
        opened.borrow_mut().push(target.clone());
        if let Some(pair) = first.borrow_mut().take() {
            return pair;
        }
        let (tx, commands) = unbounded();
        let (events, rx) = unbounded();
        extra_hosts.borrow_mut().push(ExtraHost {
            target: target.clone(),
            events,
            commands,
            unread: Vec::new(),
            other_commands: Vec::new(),
        });
        (tx, rx)
    })
}

/// A host the window holds beside This PC's, as a spec drives its fake
/// daemon: what [`Harness::send`] and [`Harness::sent`] are for This PC.
pub struct HostHandle<'h, 'a> {
    harness: &'h mut Harness<'a>,
    target: Target,
    /// Which of the connections opened to the target this handle drives,
    /// counted back from the newest: 0 is the newest.
    back: usize,
}

impl<'h, 'a> HostHandle<'h, 'a> {
    /// The connection the window opened to this target before the one this
    /// handle drives: a replaced connection, whose events the window must
    /// ignore.
    #[must_use]
    pub fn old_connection(self) -> HostHandle<'h, 'a> {
        HostHandle {
            back: self.back + 1,
            ..self
        }
    }

    /// Runs `act` on this host's fake-daemon ends.
    fn wire<R>(&mut self, act: impl FnOnce(&mut ExtraHost) -> R) -> R {
        let mut hosts = self.harness.extra_hosts.borrow_mut();
        let host = hosts
            .iter_mut()
            .rev()
            .filter(|host| host.target == self.target)
            .nth(self.back)
            .expect("the window opened that connection to the host's target");
        act(host)
    }

    /// Delivers `event` on this host's connection and lets the view settle.
    pub fn event(&mut self, event: NetEvent) {
        self.wire(|host| host.events.unbounded_send(event))
            .expect("the root view listens");
        self.harness.cx.run_until_parked();
    }

    /// Delivers `events` on this host's connection back to back, then lets
    /// the view settle, so the view reads them in one go.
    pub fn events_together(&mut self, events: Vec<NetEvent>) {
        self.wire(|host| {
            for event in events {
                host.events
                    .unbounded_send(event)
                    .expect("the root view listens");
            }
        });
        self.harness.cx.run_until_parked();
    }

    /// Delivers `msg` from this host's daemon and lets the view settle.
    pub fn send(&mut self, msg: DaemonMessage) {
        self.event(NetEvent::Message(Box::new(msg)));
    }

    /// Reports this host's connection open and welcomed, then what its
    /// daemon sends first: `Welcome`, its agents and their Settings values.
    /// A tailnet host reports no handshake. What the client sends on
    /// `Welcome` is drained.
    pub fn connect(&mut self) {
        self.connect_keeping_sent();
        self.sent();
    }

    /// Connects as [`Self::connect`] does, leaving what the client sent on
    /// `Welcome` for [`Self::sent`] to read.
    pub fn connect_keeping_sent(&mut self) {
        let target = self.target.clone();
        self.event(NetEvent::State(connected_to(
            target,
            8787,
            daemon_features(),
        )));
        self.send(welcome(PROTOCOL));
        self.send(DaemonMessage::Agents {
            agents: agent_registry::descriptors(),
        });
        self.send(DaemonMessage::AgentSettings {
            settings: BTreeMap::new(),
        });
    }

    /// Every message the client sent on this host's connection since the
    /// last call.
    pub fn sent(&mut self) -> Vec<ClientMessage> {
        self.drain();
        self.wire(|host| std::mem::take(&mut host.unread))
    }

    /// Every command other than a message the client sent on this host's
    /// connection since the last call.
    pub fn commands(&mut self) -> Vec<NetCommand> {
        self.drain();
        self.wire(|host| std::mem::take(&mut host.other_commands))
    }

    fn drain(&mut self) {
        self.harness.cx.run_until_parked();
        self.wire(|host| {
            while let Ok(command) = host.commands.try_recv() {
                match command {
                    NetCommand::Send(msg) => host.unread.push(*msg),
                    other => host.other_commands.push(other),
                }
            }
        });
    }
}

impl<'a> Harness<'a> {
    /// Opens the window with its layout in `dir` and connects it.
    pub fn open(cx: &'a mut TestAppContext, dir: &TestDir) -> Self {
        Self::open_wanting(cx, dir, None)
    }

    /// Opens the window as [`Self::open`] does, `wanted` given on the
    /// command line as the session to focus once the layout and the
    /// sessions are in.
    pub fn open_wanting(cx: &'a mut TestAppContext, dir: &TestDir, wanted: Option<&str>) -> Self {
        Self::open_with_updater(cx, dir, wanted, Arc::new(UpdaterRecorder::default()))
    }

    /// Opens the window as [`Self::open_wanting`] does, over `updater`, so a
    /// spec sets what the launch reads from it first.
    ///
    /// The window then draws in [`UiTheme::CONTRAST`], whatever theme the
    /// boot resolved: the specs assert the palette's colours, which are
    /// Contrast's. [`Self::with_saved_theme`] keeps the boot's theme.
    pub fn open_with_updater(
        cx: &'a mut TestAppContext,
        dir: &TestDir,
        wanted: Option<&str>,
        updater: Arc<UpdaterRecorder>,
    ) -> Self {
        let mut harness = Self::boot(cx, dir, wanted, updater);
        harness.pin_contrast();
        harness
    }

    /// Opens the window as [`Self::with`] does, but drawing the theme the
    /// boot resolved from `native-ui.json`, not the pinned Contrast.
    pub fn with_saved_theme(cx: &'a mut TestAppContext, dir: &TestDir, fixture: &Fixture) -> Self {
        let mut harness = Self::boot(cx, dir, None, Arc::new(UpdaterRecorder::default()));
        harness.load(fixture);
        harness
    }

    /// Draws the window in [`UiTheme::CONTRAST`].
    fn pin_contrast(&mut self) {
        self.root_update(|root, _, cx| root.set_theme(UiTheme::CONTRAST, cx));
    }

    /// Opens the window over `updater` and connects it, in the theme the
    /// boot resolved.
    fn boot(
        cx: &'a mut TestAppContext,
        dir: &TestDir,
        wanted: Option<&str>,
        updater: Arc<UpdaterRecorder>,
    ) -> Self {
        cx.update(bind_keys);
        let (tx, commands) = unbounded();
        let (events, rx) = unbounded();
        let clock = TestClock::new();
        let quits = Rc::new(Cell::new(0));
        let opener = Arc::new(OpenRecorder::default());
        let notifier = Arc::new(NotifyRecorder::default());
        let picker = Rc::new(PickRecorder::default());
        let autostart = Arc::new(AutostartRecorder::default());
        let tokens = Arc::new(MemoryTokens::default());
        let opened = Rc::new(RefCell::new(Vec::new()));
        let extra_hosts = ExtraHosts::default();
        let deps = RootDeps {
            open_host: fake_transport_sharing((tx, rx), &opened, &extra_hosts),
            ui_dir: Some(dir.path().to_path_buf()),
            paths: Err("specs have no config dir".to_owned()),
            wanted: wanted.map(str::to_owned),
            now: clock.clock(),
            quit: quit_recorder(&quits),
            open: opener.clone(),
            notify: notifier.clone(),
            pick_folder: PickRecorder::picker(&picker),
            pick_file: PickRecorder::file_picker(&picker),
            autostart: autostart.clone(),
            tokens: tokens.clone(),
            updater: updater.clone(),
        };
        let (root, cx) =
            cx.add_window_view(move |window, cx| RootView::with_transport(deps, window, cx));
        let mut harness = Self {
            cx,
            root,
            events,
            commands,
            opened,
            clock,
            answered: HashSet::new(),
            outbox: Outbox::default(),
            quits,
            opener,
            notifier,
            picker,
            autostart,
            tokens,
            updater,
            extra_hosts,
        };
        harness.count_the_window_hovered();
        harness.connect();
        harness
    }

    /// gpui's test window never reports the pointer in the window, so the
    /// specs say it is there, as a real pointer over a row would be.
    fn count_the_window_hovered(&mut self) {
        self.root_update(|root, _, cx| root.set_window_hover_override(Some(true), cx));
    }

    /// Opens the window on the real network thread over `net`. The
    /// fake-daemon half of the harness ([`Self::send`], [`Self::sent`])
    /// does nothing here.
    pub fn open_on_net(cx: &'a mut TestAppContext, dir: &TestDir, net: NetDeps) -> Self {
        cx.update(bind_keys);
        let (tx, net_commands) = unbounded();
        let (net_events, rx) = unbounded();
        spawn_net(net, Target::Local, net_commands, net_events);
        let updater = Arc::new(UpdaterRecorder::default());
        let clock = TestClock::new();
        let quits = Rc::new(Cell::new(0));
        let opener = Arc::new(OpenRecorder::default());
        let notifier = Arc::new(NotifyRecorder::default());
        let picker = Rc::new(PickRecorder::default());
        let autostart = Arc::new(AutostartRecorder::default());
        let tokens = Arc::new(MemoryTokens::default());
        let opened = Rc::new(RefCell::new(Vec::new()));
        let extra_hosts = ExtraHosts::default();
        let deps = RootDeps {
            open_host: fake_transport_sharing((tx, rx), &opened, &extra_hosts),
            ui_dir: Some(dir.path().to_path_buf()),
            paths: Err("specs have no config dir".to_owned()),
            wanted: None,
            now: clock.clock(),
            quit: quit_recorder(&quits),
            open: opener.clone(),
            notify: notifier.clone(),
            pick_folder: PickRecorder::picker(&picker),
            pick_file: PickRecorder::file_picker(&picker),
            autostart: autostart.clone(),
            tokens: tokens.clone(),
            updater: updater.clone(),
        };
        let (root, cx) =
            cx.add_window_view(move |window, cx| RootView::with_transport(deps, window, cx));
        let mut harness = Self {
            cx,
            root,
            events: unbounded().0,
            commands: unbounded().1,
            opened,
            clock,
            answered: HashSet::new(),
            outbox: Outbox::default(),
            quits,
            opener,
            notifier,
            picker,
            autostart,
            tokens,
            updater,
            extra_hosts,
        };
        harness.count_the_window_hovered();
        harness.pin_contrast();
        harness
    }

    /// Every target the window opened a connection to, in order.
    pub fn opened_targets(&self) -> Vec<Target> {
        self.opened.borrow().clone()
    }

    /// The device tokens the client holds per tailnet host.
    pub fn tokens(&self) -> &MemoryTokens {
        &self.tokens
    }

    /// The updater the client checks, installs and reads back through.
    pub fn updater(&self) -> &UpdaterRecorder {
        &self.updater
    }

    /// How many times the view asked the app to quit.
    pub fn quit_requests(&self) -> usize {
        self.quits.get()
    }

    /// The login launch the Remote access page reads and switches.
    pub fn autostart(&self) -> &AutostartRecorder {
        &self.autostart
    }

    /// Makes every VS Code job report VS Code missing, so its caller falls
    /// back.
    pub fn fail_vscode_opens(&self) {
        *self
            .opener
            .vscode_missing
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = true;
    }

    /// Makes `path` gone: the opener reports it missing from now on.
    pub fn missing_path(&self, path: &str) {
        self.opener
            .missing
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(PathBuf::from(path));
    }

    /// Everything the opener was asked to open, oldest first, once the view
    /// has settled.
    pub fn opened(&mut self) -> Vec<Opened> {
        self.cx.run_until_parked();
        self.opener
            .opened
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Makes the folder picker answer `folder` (a cancel when `None`).
    pub fn set_picked_folder(&self, folder: Option<&str>) {
        *self.picker.next.borrow_mut() = folder.map(PathBuf::from);
    }

    /// Makes every folder ask wait until [`Self::release_picked_folder`]
    /// answers it.
    pub fn hold_picks(&self) {
        self.picker.hold.set(true);
    }

    /// Answers the held folder ask with `folder` (a cancel when `None`) and
    /// lets the view settle.
    pub fn release_picked_folder(&mut self, folder: Option<&str>) {
        let answer = self.picker.held.borrow_mut().take();
        assert!(answer.is_some(), "a folder ask is held");
        if let Some(answer) = answer {
            // The receiver is gone only when the view dropped the pick.
            answer.send(folder.map(PathBuf::from)).ok();
        }
        self.cx.run_until_parked();
    }

    /// How many times the folder picker was asked.
    pub fn folder_asks(&self) -> usize {
        self.picker.asks.get()
    }

    /// The folder each ask of the folder picker opened at, oldest first.
    pub fn folder_starts(&self) -> Vec<Option<PathBuf>> {
        self.picker.starts.borrow().clone()
    }

    /// Makes the file picker answer `file` (a cancel when `None`).
    pub fn set_picked_file(&self, file: Option<&str>) {
        *self.picker.next_file.borrow_mut() = file.map(PathBuf::from);
    }

    /// How many times the file picker was asked.
    pub fn file_asks(&self) -> usize {
        self.picker.file_asks.get()
    }

    /// The folder each ask of the file picker opened at, oldest first.
    pub fn file_starts(&self) -> Vec<Option<PathBuf>> {
        self.picker.file_starts.borrow().clone()
    }

    /// The filter each ask of the file picker offered, oldest first.
    pub fn file_filters(&self) -> Vec<FileFilter> {
        self.picker.file_filters.borrow().clone()
    }

    /// Reports `host` as behind a mapped network drive from now on.
    pub fn map_unc_host(&self, host: &str) {
        self.opener
            .mapped_hosts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(host.to_owned());
    }

    /// Every notification shown, as (title, body), oldest first, once the
    /// view has settled.
    pub fn notified(&mut self) -> Vec<(String, String)> {
        self.cx.run_until_parked();
        self.notifier
            .shown
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Every pairing toast the client raised, in order, once the view has
    /// settled.
    pub fn pairing_toasts(&mut self) -> Vec<PairingToast> {
        self.cx.run_until_parked();
        self.notifier.pairing_toasts()
    }

    /// Presses the last pairing toast as [`NotifyRecorder::activate`] does,
    /// then lets the view settle.
    pub fn activate_pairing_toast(&mut self, arg: Option<&str>) {
        self.cx.run_until_parked();
        self.notifier.activate(arg);
        self.cx.run_until_parked();
    }

    /// Reports `state` as Windows' toast setting from now on.
    pub fn set_notify_state(&self, state: NotifyState) {
        *self
            .notifier
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = state;
    }

    /// The run confirm's title and detail while it is open.
    pub fn run_confirm(&mut self) -> Option<(String, String)> {
        self.root(|root, _| root.run_confirm_text())
    }

    /// Reports `conn` as the network thread would.
    pub fn set_connection(&mut self, conn: Connection) {
        self.event(NetEvent::State(conn));
    }

    /// Opens the window and delivers `fixture` as a daemon's first
    /// messages would.
    pub fn with(cx: &'a mut TestAppContext, dir: &TestDir, fixture: &Fixture) -> Self {
        let mut harness = Self::open(cx, dir);
        harness.load(fixture);
        harness
    }

    /// Reports the connection as an older daemon's, whose `Welcome` lists
    /// none of [`daemon_features`].
    pub fn welcome_without_features(&mut self) {
        self.event(NetEvent::State(connected(Vec::new())));
    }

    /// Reports the connection as a current daemon's, whose `Welcome` lists
    /// [`daemon_features`], as after a restart onto a newer daemon.
    pub fn welcome_with_features(&mut self) {
        self.event(NetEvent::State(connected(daemon_features())));
    }

    fn connect(&mut self) {
        self.event(NetEvent::State(connected(daemon_features())));
        self.report_handshake();
        self.welcome_messages();
    }

    /// The `daemon.json` handshake the local daemon reports, and no tailnet
    /// host does.
    fn report_handshake(&mut self) {
        self.event(NetEvent::Handshake(HandshakeInfo {
            port: 4242,
            pid: 1,
            protocol_version: PROTOCOL,
        }));
    }

    /// What the daemon sends first on a connection, in its order: `Welcome`,
    /// the agents' descriptors and their Settings values (none set). What
    /// the client sends on `Welcome` is drained: every connection asks for
    /// the session history, which the specs that care about it ask for
    /// themselves.
    fn welcome_messages(&mut self) {
        self.send_welcome(PROTOCOL);
        self.drain();
        self.outbox
            .unread
            .retain(|msg| !matches!(msg, ClientMessage::ListSessionHistory { .. }));
    }

    /// Sends what the daemon sends first on a connection speaking
    /// `protocol_version`: `Welcome`, then the agents' descriptors and their
    /// Settings values (none set), as after a reconnect.
    pub fn send_welcome(&mut self, protocol_version: u32) {
        self.send(welcome(protocol_version));
        self.send(DaemonMessage::Agents {
            agents: agent_registry::descriptors(),
        });
        self.send(DaemonMessage::AgentSettings {
            settings: BTreeMap::new(),
        });
    }

    pub fn load(&mut self, fixture: &Fixture) {
        self.send(DaemonMessage::Repos {
            repos: fixture.repos.clone(),
        });
        self.send(DaemonMessage::Workspaces {
            workspaces: fixture.workspaces.clone(),
        });
        self.send(DaemonMessage::Sessions {
            sessions: fixture.sessions.clone(),
        });
        self.send(DaemonMessage::Tabs {
            tabs: fixture.tabs.clone(),
        });
    }

    /// Reports a connection that has not reached the daemon, which shows the
    /// connecting overlay.
    pub fn lose_connection(&mut self) {
        self.event(NetEvent::State(Connection::new()));
    }

    /// Delivers `event` as the network thread would and lets the view
    /// settle.
    pub fn event(&mut self, event: NetEvent) {
        self.events
            .unbounded_send(event)
            .expect("the root view listens");
        self.cx.run_until_parked();
    }

    /// Delivers every one of `events` before the view settles, as a burst
    /// the network thread queued faster than background work runs.
    pub fn events_together(&mut self, events: Vec<NetEvent>) {
        for event in events {
            self.events
                .unbounded_send(event)
                .expect("the root view listens");
        }
        self.cx.run_until_parked();
    }

    /// Delivers `msg` from the daemon and lets the view settle.
    pub fn send(&mut self, msg: DaemonMessage) {
        self.event(NetEvent::Message(Box::new(msg)));
    }

    /// Every message the client sent since the last call.
    pub fn sent(&mut self) -> Vec<ClientMessage> {
        self.drain();
        std::mem::take(&mut self.outbox.unread)
    }

    /// Every command other than a message the client sent since the last
    /// call.
    pub fn commands(&mut self) -> Vec<NetCommand> {
        self.drain();
        std::mem::take(&mut self.outbox.commands)
    }

    /// Moves what the client sent into the outbox, noting the id of every
    /// `LoadScrollback` on the way.
    fn drain(&mut self) {
        self.cx.run_until_parked();
        while let Ok(command) = self.commands.try_recv() {
            let NetCommand::Send(msg) = command else {
                self.outbox.commands.push(command);
                continue;
            };
            {
                if let ClientMessage::LoadScrollback {
                    session_id,
                    request_id: Some(id),
                } = msg.as_ref()
                {
                    self.outbox
                        .scrollback_ids
                        .entry(session_id.clone())
                        .or_default()
                        .push(id.clone());
                }
                if let ClientMessage::ListPresets {
                    request_id: Some(id),
                    ..
                } = msg.as_ref()
                {
                    self.outbox.preset_ids.push(id.clone());
                }
                self.outbox.unread.push(*msg);
            }
        }
    }

    /// The id of the latest `ListPresets` the client sent, read without
    /// taking the request from [`Self::sent`].
    pub fn preset_request(&mut self) -> String {
        self.drain();
        self.outbox
            .preset_ids
            .last()
            .cloned()
            .expect("the client asked for a preset list with an id")
    }

    /// The ids of `session`'s scrollback requests so far, oldest first.
    pub fn scrollback_requests(&mut self, session: &str) -> Vec<String> {
        self.drain();
        self.outbox
            .scrollback_ids
            .get(session)
            .cloned()
            .unwrap_or_default()
    }

    /// The input bytes sent to `session` since the last drain.
    pub fn sent_input(&mut self, session: &str) -> Vec<u8> {
        self.sent()
            .into_iter()
            .filter_map(|msg| match msg {
                ClientMessage::SendInput {
                    session_id,
                    data_b64,
                    ..
                } if session_id == session => Some(B64.decode(data_b64).expect("input base64")),
                _ => None,
            })
            .flatten()
            .collect()
    }

    /// The `automatic` flag on each input sent to `session` since the last
    /// drain, oldest first.
    pub fn sent_input_flags(&mut self, session: &str) -> Vec<bool> {
        self.sent()
            .into_iter()
            .filter_map(|msg| match msg {
                ClientMessage::SendInput {
                    session_id,
                    automatic,
                    ..
                } if session_id == session => Some(automatic),
                _ => None,
            })
            .collect()
    }

    /// Answers `session`'s latest scrollback request with `history`, echoing
    /// its id as the daemon does from a live snapshot, which restarts the
    /// session's forwarder.
    pub fn answer_scrollback(&mut self, session: &str, history: &[u8]) {
        let latest = self.scrollback_requests(session).pop();
        self.answer_scrollback_to(session, latest.as_deref(), true, history);
    }

    /// Answers `session`'s latest scrollback request with `history`, echoing
    /// its id as the daemon does when it reads the history from disk: no
    /// forwarder restart, so output held back meanwhile follows the history.
    pub fn answer_scrollback_from_file(&mut self, session: &str, history: &[u8]) {
        let latest = self.scrollback_requests(session).pop();
        self.answer_scrollback_to(session, latest.as_deref(), false, history);
    }

    /// Answers `session`'s scrollback with `history` under `request_id`:
    /// an earlier request's id, or none as a daemon that echoes none.
    /// `forwarder_restarted` is what the daemon says of its forwarder.
    pub fn answer_scrollback_to(
        &mut self,
        session: &str,
        request_id: Option<&str>,
        forwarder_restarted: bool,
        history: &[u8],
    ) {
        self.answered.insert(session.to_owned());
        self.send(DaemonMessage::Scrollback {
            session_id: session.to_owned(),
            data_b64: B64.encode(history),
            truncated: false,
            request_id: request_id.map(str::to_owned),
            forwarder_restarted,
        });
    }

    /// Live output for `session`, as it arrives while its scrollback may
    /// still be loading.
    pub fn pty_raw(&mut self, session: &str, bytes: &[u8]) {
        self.send(DaemonMessage::PtyOutput {
            session_id: session.to_owned(),
            data_b64: B64.encode(bytes),
        });
    }

    /// Live output for `session`, its (empty) scrollback answered first.
    pub fn pty(&mut self, session: &str, bytes: &[u8]) {
        if !self.answered.contains(session) {
            self.answer_scrollback(session, b"");
        }
        self.pty_raw(session, bytes);
    }

    pub fn grid_text(&mut self, pane: &str) -> Vec<String> {
        let root = self.root.clone();
        self.cx
            .update(|_, cx| root.read(cx).pane_grid_text(pane, cx))
            .expect("the pane has a terminal")
    }

    /// Where cell (`col`, `row`) of `pane` is on screen, checked against the
    /// pane's grid bounds.
    pub fn cell_center(&mut self, pane: &str, col: usize, row: usize) -> Point<Pixels> {
        let grid = self.bounds(&format!("pane-grid-{pane}"));
        let root = self.root.clone();
        let at = self
            .cx
            .update(|_, cx| root.read(cx).pane_cell_center(pane, col, row, cx))
            .expect("the pane has been laid out");
        assert!(
            grid.contains(&at),
            "cell ({col}, {row}) lies outside {pane}'s grid"
        );
        at
    }

    pub fn root<R>(&mut self, read: impl FnOnce(&RootView, &gpui::App) -> R) -> R {
        let root = self.root.clone();
        self.cx.update(|_, cx| read(root.read(cx), cx))
    }

    /// The bounds the element tagged `selector` was last painted at. gpui
    /// keeps them after the element leaves the tree, so they never prove it
    /// is still there: [`Self::center`] asks the model first.
    pub fn bounds(&mut self, selector: &str) -> Bounds<Pixels> {
        let selector: &'static str = Box::leak(selector.to_owned().into_boxed_str());
        self.cx.run_until_parked();
        self.cx
            .debug_bounds(selector)
            .unwrap_or_else(|| Bounds::new(point(px(-1.0), px(-1.0)), gpui::size(px(0.0), px(0.0))))
    }

    /// Whether the model still shows what `selector` tags. Selectors with no
    /// model counterpart (the pane header buttons, "new-tab") count as
    /// present; their bounds alone cannot prove it.
    pub fn in_model(&mut self, selector: &str) -> bool {
        let selector = selector.to_owned();
        // The shelf's model is the entries it shows; the column they are
        // drawn into carries the selector, so this needs both.
        let shelf_drawn = selector != "undo-shelf" || {
            self.cx.run_until_parked();
            self.cx
                .debug_bounds("undo-shelf")
                .is_some_and(|bounds| bounds.origin.x >= px(0.0))
        };
        self.root(move |root, cx| {
            let pane = |id: &str| root.active_pane_ids().iter().any(|p| p == id);
            let sessions_shown = !root.sidebar_collapsed() && root.activity() == Activity::Sessions;
            if selector == "undo-shelf" {
                shelf_drawn && !root.undo_entries().is_empty()
            } else if let Some(shown) = modal_part_shown(root, &selector) {
                shown
            } else if selector == "tab-menu" {
                root.tab_menu().is_some()
            } else if selector.starts_with("tab-menu-") {
                root.tab_menu_rows().contains(&selector)
            } else if let Some(id) = selector.strip_prefix("tab-close-") {
                root.tab_ids().iter().any(|t| t == id)
            } else if let Some(id) = selector.strip_prefix("tab-") {
                root.tab_ids().iter().any(|t| t == id)
            } else if let Some(shown) = sidebar_part_shown(root, &selector) {
                sessions_shown && shown
            } else if let Some(id) = selector.strip_prefix("pane-grid-") {
                pane(id)
            } else if let Some(shown) = session_menu_part_shown(root, &selector) {
                shown
            } else if let Some(id) = selector
                .strip_prefix("pane-stop-confirm-")
                .or_else(|| selector.strip_prefix("pane-stop-cancel-"))
            {
                root.armed_stop_pane() == Some(id)
            } else if selector == "delete-worktree-dialog"
                || selector == "delete-worktree-dialog-close"
            {
                root.delete_dialog_session().is_some()
            } else if selector.starts_with("delete-worktree-") {
                root.delete_dialog_buttons()
                    .iter()
                    .any(|(button, _)| *button == selector)
            } else if let Some(shown) = toast_part_shown(root, &selector) {
                shown
            } else if selector.starts_with("action-failed-") {
                root.action_failed().is_some()
            } else if selector.starts_with("checkout-") {
                root.checkout_prompt().is_some()
            } else if selector.starts_with("spawn-share-") {
                root.spawn_share_confirm_open()
            } else if selector == "spawn-no-repos" || selector == "spawn-add-repo" {
                root.spawn_dialog_no_repos()
            } else if selector.starts_with("spawn-") {
                root.spawn_dialog_open()
            } else if let Some((pane, n)) = selector
                .strip_prefix("shell-dot-")
                .and_then(|rest| rest.rsplit_once('-'))
            {
                n.parse::<usize>()
                    .is_ok_and(|n| n < root.pane_shell_records(pane, cx).len())
            } else if selector == "shell-menu" {
                root.shell_menu_open()
            } else if selector.starts_with("shell-menu-") {
                root.shell_menu_rows().contains(&selector)
            } else if selector == "shell-clear-default" {
                root.shell_dialog_clears_default()
            } else if selector.starts_with("shell-") {
                root.shell_dialog_open()
            } else if selector == "sidebar-add-session"
                || selector == "sidebar-session-hint"
                || selector == "sidebar-add-shell"
                || selector == "sidebar-more"
            {
                sessions_shown
            } else if selector == "sidebar-more-menu" || selector == "sidebar-more-shell-dialog" {
                sessions_shown && root.sidebar_more_open()
            } else if let Some(id) = selector
                .strip_prefix("empty-pane-new-session-")
                .or_else(|| selector.strip_prefix("empty-pane-shell-"))
            {
                root.empty_pane_ids().iter().any(|p| p == id)
            } else if selector.starts_with("exited-") {
                root.exited_overlay_selectors().contains(&selector)
            } else if selector.starts_with("abandoned-") || selector.starts_with("orphan-banner-") {
                root.pane_notice_selectors().contains(&selector)
            } else if selector == "empty-spawn-session" || selector == "empty-open-shell" {
                root.no_tab_choices_shown()
            } else if selector == "empty-repo-hint" || selector == "empty-add-repo" {
                root.no_tab_choices_shown() && !root.has_repos()
            } else {
                side_part_shown(root, &selector)
            }
        })
    }

    /// The centre of the element tagged `selector`, which the model must
    /// still show.
    pub fn center(&mut self, selector: &str) -> Point<Pixels> {
        assert!(self.in_model(selector), "{selector} is gone from the model");
        let bounds = self.bounds(selector);
        assert!(bounds.origin.x >= px(0.0), "{selector} is not on screen");
        bounds.center()
    }

    pub fn click(&mut self, at: Point<Pixels>, modifiers: Modifiers) {
        self.cx.simulate_click(at, modifiers);
        self.cx.run_until_parked();
    }

    /// Runs `change` on the root view, then lets the view settle.
    /// The host at `target`, beside This PC: the window connects to it
    /// through [`RootView::connect_host`] the first time a spec asks.
    pub fn host(&mut self, target: &Target) -> HostHandle<'_, 'a> {
        let known = self
            .extra_hosts
            .borrow()
            .iter()
            .any(|host| host.target == *target);
        if !known {
            let opening = target.clone();
            self.root_update(|root, window, cx| root.connect_host(opening, window, cx));
        }
        HostHandle {
            harness: self,
            target: target.clone(),
            back: 0,
        }
    }

    pub fn root_update<R>(
        &mut self,
        change: impl FnOnce(&mut RootView, &mut Window, &mut gpui::Context<RootView>) -> R,
    ) -> R {
        let root = self.root.clone();
        let out = self
            .cx
            .update(|window, cx| root.update(cx, |root, cx| change(root, window, cx)));
        self.cx.run_until_parked();
        out
    }

    pub fn click_on(&mut self, selector: &str) {
        let at = self.center(selector);
        self.click(at, Modifiers::none());
    }

    /// Scrolls `container` until what `selector` tags lies inside its bounds,
    /// so a click can reach a row below the fold.
    pub fn scroll_into_view(&mut self, container: &str, selector: &str) {
        let at = self.center(container);
        for _ in 0..50 {
            let outer = self.bounds(container);
            let inner = self.bounds(selector);
            let dy = if inner.bottom() > outer.bottom() {
                outer.bottom() - inner.bottom()
            } else if inner.top() < outer.top() {
                outer.top() - inner.top()
            } else {
                return;
            };
            self.cx.simulate_event(ScrollWheelEvent {
                position: at,
                delta: ScrollDelta::Pixels(point(px(0.0), dy)),
                modifiers: Modifiers::none(),
                touch_phase: TouchPhase::Moved,
            });
            self.cx.run_until_parked();
        }
        let (outer, inner) = (self.bounds(container), self.bounds(selector));
        assert!(
            inner.top() >= outer.top() && inner.bottom() <= outer.bottom(),
            "{selector} does not show in {container} after 50 scrolls: {inner:?} in {outer:?}"
        );
    }

    /// Opens the Shell… folder dialog through the sessions toolbar's ⋯ menu.
    pub fn open_shell_dialog_from_more(&mut self) {
        self.click_on("sidebar-more");
        self.click_on("sidebar-more-shell-dialog");
    }

    /// Opens the Fetch file… dialog through the sessions toolbar's ⋯ menu.
    pub fn open_fetch_dialog_from_more(&mut self) {
        self.click_on("sidebar-more");
        self.click_on("sidebar-more-fetch-file");
    }

    /// A right-button press and release on the element tagged `selector`.
    pub fn right_click_on(&mut self, selector: &str) {
        let at = self.center(selector);
        self.right_click(at);
    }

    /// A right-button press and release at `at`.
    pub fn right_click(&mut self, at: Point<Pixels>) {
        self.cx
            .simulate_mouse_down(at, MouseButton::Right, Modifiers::none());
        self.cx
            .simulate_mouse_up(at, MouseButton::Right, Modifiers::none());
        self.cx.run_until_parked();
    }

    pub fn double_click(&mut self, at: Point<Pixels>) {
        self.cx.simulate_event(MouseDownEvent {
            position: at,
            modifiers: Modifiers::none(),
            button: MouseButton::Left,
            click_count: 2,
            first_mouse: false,
        });
        self.cx.simulate_event(MouseUpEvent {
            position: at,
            modifiers: Modifiers::none(),
            button: MouseButton::Left,
            click_count: 2,
        });
        self.cx.run_until_parked();
    }

    /// A left-button drag: press at `from` with `down`, move to `to`, and
    /// release there with `up`.
    pub fn drag(&mut self, from: Point<Pixels>, to: Point<Pixels>, mods: [Modifiers; 2]) {
        let [down, up] = mods;
        self.cx.simulate_mouse_down(from, MouseButton::Left, down);
        self.cx.simulate_mouse_move(to, MouseButton::Left, up);
        self.cx.simulate_mouse_up(to, MouseButton::Left, up);
        self.cx.run_until_parked();
    }

    /// Starts a drag-and-drop gesture the way a hand does: a left press at
    /// `from`, then a move 6 px to the right, past the 2 px gpui waits for
    /// before it starts a drag. That first move starts the drag and reaches
    /// no drop target; [`Self::drag_to`] then moves the dragged item.
    pub fn start_drag(&mut self, from: Point<Pixels>) {
        self.cx
            .simulate_mouse_down(from, MouseButton::Left, Modifiers::none());
        self.cx.simulate_mouse_move(
            point(from.x + px(6.0), from.y),
            MouseButton::Left,
            Modifiers::none(),
        );
        self.cx.run_until_parked();
    }

    /// Moves the mouse to `to` with the left button held, as a drag does.
    pub fn drag_to(&mut self, to: Point<Pixels>) {
        self.cx
            .simulate_mouse_move(to, MouseButton::Left, Modifiers::none());
        self.cx.run_until_parked();
    }

    /// Releases the held left button at `at`, ending a drag there.
    pub fn release(&mut self, at: Point<Pixels>) {
        self.cx
            .simulate_mouse_up(at, MouseButton::Left, Modifiers::none());
        self.cx.run_until_parked();
    }

    /// A whole drag-and-drop: started at `from`, moved onto `to` and
    /// released there.
    pub fn drag_and_drop(&mut self, from: Point<Pixels>, to: Point<Pixels>) {
        self.start_drag(from);
        self.drag_to(to);
        self.release(to);
    }

    /// Whether a drag-and-drop gesture is under way.
    pub fn dragging(&mut self) -> bool {
        self.root(|_, cx| cx.has_active_drag())
    }

    /// Holds `modifiers` and moves the mouse to `at`, as the platform
    /// reports them: the modifier change, then the move carrying it.
    pub fn hover(&mut self, at: Point<Pixels>, modifiers: Modifiers) {
        self.cx.simulate_modifiers_change(modifiers);
        self.cx.simulate_mouse_move(at, None, modifiers);
        self.cx.run_until_parked();
    }

    /// Changes the held modifiers without moving the mouse.
    pub fn set_modifiers(&mut self, modifiers: Modifiers) {
        self.cx.simulate_modifiers_change(modifiers);
        self.cx.run_until_parked();
    }

    /// The text of the link `pane` underlines.
    pub fn hovered_link(&mut self, pane: &str) -> Option<String> {
        self.root(|root, cx| root.pane_hovered_link(pane, cx))
    }

    pub fn keys(&mut self, keystrokes: &str) {
        self.cx.simulate_keystrokes(keystrokes);
    }

    /// Dispatches one key press the way the platform delivers it, without the
    /// character `simulate_keystrokes` synthesizes from it.
    pub fn key_down(&mut self, keystroke: Keystroke) {
        self.cx.simulate_event(KeyDownEvent {
            keystroke,
            is_held: false,
        });
        self.cx.run_until_parked();
    }

    /// Runs `drive` against pane `pane`'s text input the way the platform
    /// does, then lets the view settle.
    fn input<R>(
        &mut self,
        pane: &str,
        drive: impl FnOnce(&mut dyn InputHandler, &mut Window, &mut App) -> R,
    ) -> R {
        let root = self.root.clone();
        let result = self.cx.update(|window, cx| {
            let mut handler = root
                .read(cx)
                .pane_input_handler(pane)
                .expect("the pane has a terminal");
            drive(&mut handler, window, cx)
        });
        self.cx.run_until_parked();
        result
    }

    /// An IME composition update in `pane`: `text` is the whole preedit.
    pub fn compose(&mut self, pane: &str, text: &str) {
        self.input(pane, |handler, window, cx| {
            handler.replace_and_mark_text_in_range(None, text, None, window, cx);
        });
    }

    /// Text committed in `pane`: an IME result, or a character the
    /// platform typed.
    pub fn commit(&mut self, pane: &str, text: &str) {
        self.input(pane, |handler, window, cx| {
            handler.replace_text_in_range(None, text, window, cx);
        });
    }

    /// The composition `pane` holds, read back through its marked range.
    pub fn marked_text(&mut self, pane: &str) -> Option<String> {
        self.input(pane, |handler, window, cx| {
            let range = handler.marked_text_range(window, cx)?;
            handler.text_for_range(range, &mut None, window, cx)
        })
    }

    /// The marked text `pane` draws at its cursor, a dead key's included.
    pub fn preedit(&mut self, pane: &str) -> Option<String> {
        self.root(|root, cx| root.pane_preedit(pane, cx))
    }

    /// Where `pane` anchors the IME window: the bounds of its selection.
    pub fn ime_bounds(&mut self, pane: &str) -> Option<Bounds<Pixels>> {
        self.input(pane, |handler, window, cx| {
            let selection = handler.selected_text_range(false, window, cx)?;
            handler.bounds_for_range(selection.range, window, cx)
        })
    }

    pub fn clipboard(&mut self) -> Option<String> {
        self.cx.read_from_clipboard().and_then(|item| item.text())
    }

    pub fn set_clipboard(&mut self, text: &str) {
        self.cx
            .write_to_clipboard(gpui::ClipboardItem::new_string(text.to_owned()));
    }

    /// Moves the terminals' clock and the executor's timers together.
    pub fn advance(&mut self, by: Duration) {
        self.clock.advance(by);
        self.cx.executor().advance_clock(by);
        self.cx.run_until_parked();
    }

    pub fn window_width(&mut self) -> f32 {
        self.cx
            .update(|window, _| window.viewport_size().width / px(1.0))
    }

    pub fn window_height(&mut self) -> f32 {
        self.cx
            .update(|window, _| window.viewport_size().height / px(1.0))
    }

    /// How many windows are open, the main one included.
    pub fn window_count(&mut self) -> usize {
        self.cx.run_until_parked();
        self.cx.windows().len()
    }

    /// The one open window other than the main one, as a test context of
    /// its own, to type into, read and close.
    pub fn popout_window(&mut self) -> VisualTestContext {
        self.cx.run_until_parked();
        let main = self.cx.update(|window, _| window.window_handle());
        let popout = self
            .cx
            .windows()
            .into_iter()
            .find(|window| *window != main)
            .expect("a pop-out window is open");
        VisualTestContext::from_window(popout, &self.cx.cx)
    }

    /// Asks the pop-out window to close, as its X button does, and closes it
    /// when its handler agrees, as the platform does (the test platform
    /// only asks); returns whether it may close.
    pub fn close_popout(&mut self) -> bool {
        let mut popout = self.popout_window();
        let closed = popout.simulate_close();
        if closed {
            popout.update(|window, _| window.remove_window());
        }
        self.cx.run_until_parked();
        closed
    }
}
