//! A fake daemon for the native client's UI specs: the root view on a test
//! window, fed daemon messages, read back through the commands it sends.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use gpui::{
    Bounds, Entity, Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, Pixels, Point,
    TestAppContext, VisualTestContext, point, px,
};
use protocol::{
    ClientMessage, DaemonMessage, GridNode, RepoEntry, SessionSnapshot, SplitDirection, TabEntry,
    WorkspaceEntry,
};
use rustling_tulip_native::{
    Clock, Connection, HandshakeInfo, NetCommand, NetEvent, RootDeps, RootView, bind_keys,
};
use serde_json::{Value, json};

pub mod live;

const PROTOCOL: u32 = 1;

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
}

impl<'a> Harness<'a> {
    /// Opens the window with its layout in `dir` and connects it.
    pub fn open(cx: &'a mut TestAppContext, dir: &TestDir) -> Self {
        cx.update(bind_keys);
        let (tx, commands) = unbounded();
        let (events, rx) = unbounded();
        let clock = TestClock::new();
        let deps = RootDeps {
            tx,
            events: rx,
            ui_dir: Some(dir.path().to_path_buf()),
            paths: Err("specs have no config dir".to_owned()),
            wanted: None,
            now: clock.clock(),
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
        };
        harness.connect();
        harness
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

    fn event(&mut self, event: NetEvent) {
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
        self.cx.run_until_parked();
        let mut out = Vec::new();
        while let Ok(command) = self.commands.try_recv() {
            if let NetCommand::Send(msg) = command {
                out.push(*msg);
            }
        }
        out
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

    /// Answers `session`'s scrollback request with `history`.
    pub fn answer_scrollback(&mut self, session: &str, history: &[u8]) {
        self.answered.insert(session.to_owned());
        self.send(DaemonMessage::Scrollback {
            session_id: session.to_owned(),
            data_b64: B64.encode(history),
            truncated: false,
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
        self.root(move |root, _| {
            let pane = |id: &str| root.active_pane_ids().iter().any(|p| p == id);
            if let Some(id) = selector.strip_prefix("tab-close-") {
                root.tab_ids().iter().any(|t| t == id)
            } else if let Some(id) = selector.strip_prefix("tab-") {
                root.tab_ids().iter().any(|t| t == id)
            } else if let Some(id) = selector.strip_prefix("leaf-") {
                !root.sidebar_collapsed()
                    && root
                        .sidebar_containers()
                        .iter()
                        .any(|c| !c.collapsed && c.leaves.iter().any(|leaf| leaf.id == id))
            } else if let Some(id) = selector.strip_prefix("pane-grid-") {
                pane(id)
            } else if selector == "session-menu" {
                root.session_menu().is_some()
            } else if selector.starts_with("menu-") {
                root.menu_rows().contains(&selector)
            } else if let Some(id) = selector
                .strip_prefix("pane-stop-confirm-")
                .or_else(|| selector.strip_prefix("pane-stop-cancel-"))
            {
                root.armed_stop_pane() == Some(id)
            } else if selector == "sidebar-show" {
                root.sidebar_collapsed()
            } else if selector == "sidebar-panel" || selector == "sidebar-divider" {
                !root.sidebar_collapsed()
            } else {
                true
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

    pub fn keys(&mut self, keystrokes: &str) {
        self.cx.simulate_keystrokes(keystrokes);
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
