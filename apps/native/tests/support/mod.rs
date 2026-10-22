//! A fake daemon for the native client's UI specs: the root view on a test
//! window, fed daemon messages, read back through the commands it sends.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use chrono::{DateTime, Utc};
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use gpui::{
    App, Bounds, Entity, InputHandler, KeyDownEvent, Keystroke, Modifiers, MouseButton,
    MouseDownEvent, MouseUpEvent, Pixels, Point, TestAppContext, VisualTestContext, Window, point,
    px,
};
use protocol::{
    ClaudeProvider, ClientMessage, DaemonMessage, GridNode, RepoEntry, SessionSnapshot,
    SplitDirection, TabEntry, WorkspaceEntry,
};
use rustling_tulip_native::{
    Activity, Clock, Connection, ContainerKind, FileFilter, FilePicker, FolderPicker,
    HandshakeInfo, NetCommand, NetDeps, NetEvent, Notifier, NotifyState, OpenFailure, Opener,
    QuitFn, RootDeps, RootView, ScPanel, bind_keys, spawn_net,
};
use serde_json::{Value, json};

pub mod live;

pub const PROTOCOL: u32 = 1;

/// The features the fake daemon lists in its `Welcome`, as a current daemon
/// does.
pub fn daemon_features() -> Vec<String> {
    vec![protocol::FEATURE_CLAUDE_PROVIDER.to_owned()]
}

/// The fake daemon's `Welcome` on `protocol_version`, listing
/// [`daemon_features`].
pub fn welcome(protocol_version: u32) -> DaemonMessage {
    DaemonMessage::Welcome {
        protocol_version,
        supported_versions: vec![protocol_version],
        features: daemon_features(),
    }
}

/// A connection open on the fake daemon's port, welcomed with `features`.
fn connected(features: Vec<String>) -> Connection {
    let mut conn = Connection::new();
    conn.on_connecting();
    conn.on_socket_open(4242);
    conn.on_welcome(PROTOCOL, features);
    conn
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

    /// Routed to `provider`'s endpoint, as the daemon derives it from the
    /// session's spawn config.
    pub fn claude_provider(self, provider: ClaudeProvider) -> Self {
        self.set("claude_provider", json!(provider))
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

    pub fn build(self) -> SessionSnapshot {
        serde_json::from_value(self.0).expect("session fixture")
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
                "agent_options": {"kind": "claude", "permission_mode": null},
                "model": null, "extra_env": []
            },
            "members": [{"repo_id": "r1", "repo_name": "r1", "branch": "main", "worktree_path": "D:\\r1"}],
            "workspace_id": null, "primary_cwd": "D:\\r1", "current_cwd": null,
            "program_name": "claude", "started_at": null, "ended_at": ended,
            "end": {"type": "tracer_lost"}, "claude_session_id": null, "source": "record",
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
}

impl PickRecorder {
    fn picker(recorder: &Rc<Self>) -> FolderPicker {
        let recorder = Rc::clone(recorder);
        Rc::new(move |_: &mut App, start: Option<PathBuf>| {
            recorder.asks.set(recorder.asks.get() + 1);
            recorder.starts.borrow_mut().push(start);
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
}

/// A notifier that only records the notifications it was asked to show, as
/// a spec must never show a real toast, and reports the Windows state a
/// spec set (on until set).
pub struct NotifyRecorder {
    shown: Mutex<Vec<(String, String)>>,
    state: Mutex<NotifyState>,
}

impl Default for NotifyRecorder {
    fn default() -> Self {
        Self {
            shown: Mutex::new(Vec::new()),
            state: Mutex::new(NotifyState::On),
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

    fn state(&self) -> NotifyState {
        *self.state.lock().unwrap_or_else(PoisonError::into_inner)
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
}

/// A quit that only counts itself in `quits`, as the test platform's quit
/// does nothing.
pub fn quit_recorder(quits: &Rc<Cell<usize>>) -> QuitFn {
    let quits = Rc::clone(quits);
    Box::new(move |_| quits.set(quits.get() + 1))
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
        cx.update(bind_keys);
        let (tx, commands) = unbounded();
        let (events, rx) = unbounded();
        let clock = TestClock::new();
        let quits = Rc::new(Cell::new(0));
        let opener = Arc::new(OpenRecorder::default());
        let notifier = Arc::new(NotifyRecorder::default());
        let picker = Rc::new(PickRecorder::default());
        let deps = RootDeps {
            tx,
            events: rx,
            ui_dir: Some(dir.path().to_path_buf()),
            paths: Err("specs have no config dir".to_owned()),
            wanted: wanted.map(str::to_owned),
            now: clock.clock(),
            quit: quit_recorder(&quits),
            open: opener.clone(),
            notify: notifier.clone(),
            pick_folder: PickRecorder::picker(&picker),
            pick_file: PickRecorder::file_picker(&picker),
        };
        let (root, cx) =
            cx.add_window_view(move |window, cx| RootView::with_transport(deps, window, cx));
        let mut harness = Self {
            cx,
            root,
            events,
            commands,
            clock,
            answered: HashSet::new(),
            outbox: Outbox::default(),
            quits,
            opener,
            notifier,
            picker,
        };
        harness.connect();
        harness
    }

    /// Opens the window on the real network thread over `net`. The
    /// fake-daemon half of the harness ([`Self::send`], [`Self::sent`])
    /// does nothing here.
    pub fn open_on_net(cx: &'a mut TestAppContext, dir: &TestDir, net: NetDeps) -> Self {
        cx.update(bind_keys);
        let (tx, net_commands) = unbounded();
        let (net_events, rx) = unbounded();
        spawn_net(net, net_commands, net_events);
        let clock = TestClock::new();
        let quits = Rc::new(Cell::new(0));
        let opener = Arc::new(OpenRecorder::default());
        let notifier = Arc::new(NotifyRecorder::default());
        let picker = Rc::new(PickRecorder::default());
        let deps = RootDeps {
            tx,
            events: rx,
            ui_dir: Some(dir.path().to_path_buf()),
            paths: Err("specs have no config dir".to_owned()),
            wanted: None,
            now: clock.clock(),
            quit: quit_recorder(&quits),
            open: opener.clone(),
            notify: notifier.clone(),
            pick_folder: PickRecorder::picker(&picker),
            pick_file: PickRecorder::file_picker(&picker),
        };
        let (root, cx) =
            cx.add_window_view(move |window, cx| RootView::with_transport(deps, window, cx));
        Self {
            cx,
            root,
            events: unbounded().0,
            commands: unbounded().1,
            clock,
            answered: HashSet::new(),
            outbox: Outbox::default(),
            quits,
            opener,
            notifier,
            picker,
        }
    }

    /// How many times the view asked the app to quit.
    pub fn quit_requests(&self) -> usize {
        self.quits.get()
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
        self.event(NetEvent::Handshake(HandshakeInfo {
            port: 4242,
            pid: 1,
            protocol_version: PROTOCOL,
        }));
        self.send(welcome(PROTOCOL));
        // Every connection asks for the session history; the specs that
        // care about it send a Welcome of their own.
        self.drain();
        self.outbox
            .unread
            .retain(|msg| !matches!(msg, ClientMessage::ListSessionHistory { .. }));
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
                self.outbox.unread.push(*msg);
            }
        }
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
                } if session_id == session => Some(B64.decode(data_b64).expect("input base64")),
                _ => None,
            })
            .flatten()
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

    /// Opens the Shell… folder dialog through the sessions toolbar's ⋯ menu.
    pub fn open_shell_dialog_from_more(&mut self) {
        self.click_on("sidebar-more");
        self.click_on("sidebar-more-shell-dialog");
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
}
