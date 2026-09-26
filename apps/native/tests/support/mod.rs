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
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use gpui::{
    App, Bounds, Entity, InputHandler, KeyDownEvent, Keystroke, Modifiers, MouseButton,
    MouseDownEvent, MouseUpEvent, Pixels, Point, TestAppContext, VisualTestContext, Window, point,
    px,
};
use protocol::{
    ClientMessage, DaemonMessage, GridNode, RepoEntry, SessionSnapshot, SplitDirection, TabEntry,
    WorkspaceEntry,
};
use rustling_tulip_native::{
    Activity, Clock, Connection, HandshakeInfo, NetCommand, NetDeps, NetEvent, OpenFailure, Opener,
    QuitFn, RootDeps, RootView, ScPanel, bind_keys, spawn_net,
};
use serde_json::{Value, json};

pub mod live;

pub const PROTOCOL: u32 = 1;

/// Whether the rail's side of the window shows what `selector` tags: the
/// panels, the divider, the badge and the source-control panel's parts.
/// Selectors it does not know count as present.
fn side_part_shown(root: &RootView, selector: &str) -> bool {
    let open = !root.sidebar_collapsed();
    match selector {
        "sidebar-panel" => open && root.activity() == Activity::Sessions,
        "sidebar-divider" => open,
        "activity-badge" => root.activity_badge().is_some(),
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

    pub fn build(self) -> SessionSnapshot {
        serde_json::from_value(self.0).expect("session fixture")
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
}

impl OpenRecorder {
    fn record(&self, opened: Opened) {
        self.opened
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(opened);
    }
}

impl Opener for OpenRecorder {
    fn url(&self, url: &str) -> Result<(), String> {
        self.record(Opened::Url(url.to_owned()));
        Ok(())
    }

    fn vscode(&self, path: &Path, line: u32, column: u32) -> Result<(), OpenFailure> {
        self.record(Opened::VsCode {
            path: path.to_path_buf(),
            line,
            column,
        });
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
        cx.update(bind_keys);
        let (tx, commands) = unbounded();
        let (events, rx) = unbounded();
        let clock = TestClock::new();
        let quits = Rc::new(Cell::new(0));
        let opener = Arc::new(OpenRecorder::default());
        let deps = RootDeps {
            tx,
            events: rx,
            ui_dir: Some(dir.path().to_path_buf()),
            paths: Err("specs have no config dir".to_owned()),
            wanted: None,
            now: clock.clock(),
            quit: quit_recorder(&quits),
            open: opener.clone(),
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
        let deps = RootDeps {
            tx,
            events: rx,
            ui_dir: Some(dir.path().to_path_buf()),
            paths: Err("specs have no config dir".to_owned()),
            wanted: None,
            now: clock.clock(),
            quit: quit_recorder(&quits),
            open: opener.clone(),
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
        }
    }

    /// How many times the view asked the app to quit.
    pub fn quit_requests(&self) -> usize {
        self.quits.get()
    }

    /// Everything the terminals' Ctrl+clicks opened, oldest first, once
    /// the view has settled.
    pub fn opened(&mut self) -> Vec<Opened> {
        self.cx.run_until_parked();
        self.opener
            .opened
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Reports `host` as behind a mapped network drive from now on.
    pub fn map_unc_host(&self, host: &str) {
        self.opener
            .mapped_hosts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(host.to_owned());
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

    fn connect(&mut self) {
        let mut conn = Connection::new();
        conn.on_connecting();
        conn.on_socket_open(4242);
        conn.on_welcome(PROTOCOL);
        self.event(NetEvent::State(conn));
        self.event(NetEvent::Handshake(HandshakeInfo {
            port: 4242,
            pid: 1,
            protocol_version: PROTOCOL,
        }));
        self.send(DaemonMessage::Welcome {
            protocol_version: PROTOCOL,
            supported_versions: vec![PROTOCOL],
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
        self.root(move |root, cx| {
            let pane = |id: &str| root.active_pane_ids().iter().any(|p| p == id);
            let sessions_shown = !root.sidebar_collapsed() && root.activity() == Activity::Sessions;
            if selector == "layout-chooser" {
                root.layout_chooser_open()
            } else if selector.starts_with("layout-choose-") || selector.starts_with("chooser-") {
                root.layout_chooser_controls()
                    .iter()
                    .any(|(control, _)| *control == selector)
            } else if selector == "exit-confirm-dialog" {
                root.exit_dialog_open()
            } else if selector.starts_with("exit-") {
                root.exit_dialog_buttons()
                    .iter()
                    .any(|(button, _, _)| *button == selector)
            } else if selector == "tab-menu" {
                root.tab_menu().is_some()
            } else if selector.starts_with("tab-menu-") {
                root.tab_menu_rows().contains(&selector)
            } else if let Some(id) = selector.strip_prefix("tab-close-") {
                root.tab_ids().iter().any(|t| t == id)
            } else if let Some(id) = selector.strip_prefix("tab-") {
                root.tab_ids().iter().any(|t| t == id)
            } else if let Some(id) = selector.strip_prefix("leaf-") {
                sessions_shown
                    && root
                        .sidebar_containers()
                        .iter()
                        .any(|c| !c.collapsed && c.leaves.iter().any(|leaf| leaf.id == id))
            } else if let Some(id) = selector.strip_prefix("pane-grid-") {
                pane(id)
            } else if selector == "session-menu-accent" || selector.starts_with("accent-") {
                root.accent_menu_rows().contains(&selector)
            } else if selector == "session-menu" {
                root.session_menu().is_some()
            } else if selector.starts_with("menu-") {
                root.menu_rows().contains(&selector)
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
            } else if let Some(id) = selector
                .strip_prefix("toast-close-")
                .or_else(|| selector.strip_prefix("toast-"))
            {
                root.toasts().iter().any(|toast| toast.id.to_string() == id)
            } else if selector.starts_with("action-failed-") {
                root.action_failed().is_some()
            } else if selector.starts_with("checkout-") {
                root.checkout_prompt().is_some()
            } else if selector.starts_with("spawn-share-") {
                root.spawn_share_confirm_open()
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
                || selector == "sidebar-add-shell"
                || selector == "sidebar-shell-dialog"
            {
                sessions_shown
            } else if let Some(id) = selector
                .strip_prefix("empty-pane-new-session-")
                .or_else(|| selector.strip_prefix("empty-pane-shell-"))
            {
                root.empty_pane_ids().iter().any(|p| p == id)
            } else if selector.starts_with("exited-") {
                root.exited_overlay_selectors().contains(&selector)
            } else if selector == "empty-spawn-session" || selector == "empty-open-shell" {
                root.no_tab_choices_shown()
            } else if selector == "empty-repo-hint" {
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

    /// A right-button press and release on the element tagged `selector`.
    pub fn right_click_on(&mut self, selector: &str) {
        let at = self.center(selector);
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
}
