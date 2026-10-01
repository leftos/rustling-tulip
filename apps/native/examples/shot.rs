//! Writes a PNG of the native client's real window showing one view, filled
//! with fixed fake content by a scripted fake daemon. No daemon starts, and
//! the user's config dir and live sessions are never read or written. The
//! window opens cloaked and is never activated, so it never shows on screen
//! or takes focus; its pixels are read with `PrintWindow`.
//!
//! `cargo run -p rustling-tulip-native --example shot -- [<view>] [<out.png>]`
//!
//! Views: `main` (the default: the sidebar and a split tab), `main-compact`
//! (the same window with the sidebar's leaves one line each),
//! `source-control`, `diff`, `settings` and `spawn`. The PNG goes to
//! `.tmp/shots/<view>.png` under the repo root unless `<out.png>` is given;
//! the window's layout file is kept beside it in `.ui/` and cleared on every
//! run.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use chrono::{DateTime, Utc};
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use futures::channel::oneshot;
use futures::{FutureExt as _, StreamExt as _};
use gpui::{
    App, AppContext as _, Application, AsyncApp, Bounds, Keystroke, Window, WindowBounds,
    WindowHandle, WindowOptions, px, size,
};
use protocol::{ClientMessage, DaemonMessage, GridNode, SessionSnapshot, SplitDirection};
use rustling_tulip_native::fonts;
use rustling_tulip_native::offscreen::show_cloaked;
use rustling_tulip_native::{
    Activity, Assets, Clock, Connection, FolderPicker, HandshakeInfo, NATIVE_PROTOCOL_VERSIONS,
    NetCommand, NetEvent, Notifier, NotifyState, OpenFailure, Opener, RAIL_ITEM_HEIGHT, RAIL_WIDTH,
    RootDeps, RootView, bind_keys,
};
use serde_json::{Value, json};

/// The window's size in logical pixels, the client's own.
const WINDOW_SIZE: (u16, u16) = (1000, 640);
/// How long the window gets to draw the fixture before the view is driven.
const LOAD_WAIT: Duration = Duration::from_millis(1500);
/// How long the view gets to answer the input that reaches it.
const DRIVE_WAIT: Duration = Duration::from_millis(1000);
/// The port and pid the fake handshake reports.
const FAKE_PORT: u16 = 4242;
const FAKE_PID: u32 = 1;

/// The Source control item's place on the rail, after Sessions and Needs You.
/// The rail's buttons stack from its top with no gap, each
/// [`RAIL_ITEM_HEIGHT`] tall.
const SOURCE_CONTROL_ITEM: f32 = 2.0;

const TULIP: &str = "rustling-tulip";
const YAAT: &str = "yaat";
const WORKSPACE: &str = "flight-deck";
const TULIP_PATH: &str = r"D:\src\rustling-tulip";
const YAAT_PATH: &str = r"D:\src\yaat";
const DECK_SERVER: &str = "deck-server";
const DECK_CLIENT: &str = "deck-client";
const DECK_SERVER_PATH: &str = r"D:\src\flight-deck\deck-server";
const DECK_CLIENT_PATH: &str = r"D:\src\flight-deck\deck-client";
const PETAL_TREE: &str = r"D:\worktrees\wt.feat-petal-footer\rustling-tulip";
const RECOVER_TREE: &str = r"D:\worktrees\wt.feat-recover-keys\rustling-tulip";
const PETAL_BRANCH: &str = "feat/petal-footer";
const DIFF_TAB: &str = "t-diff";
const DIFF_PATH: &str = "apps/native/src/footer.rs";

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_max_level(tracing::Level::WARN)
        .init();
    let shot = Shot::from_args(std::env::args().skip(1))?;
    shot.prepare()?;
    let outcome: Rc<RefCell<Option<Result<()>>>> = Rc::new(RefCell::new(None));
    let slot = Rc::clone(&outcome);
    Application::new()
        .with_assets(Assets)
        .run(move |cx: &mut App| {
            if let Err(err) = start(shot, Rc::clone(&slot), cx) {
                *slot.borrow_mut() = Some(Err(err));
                cx.quit();
            }
        });
    outcome
        .take()
        .unwrap_or_else(|| Err(anyhow!("the app quit before the shot was taken")))
}

/// One of the windows the shot can show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    Main,
    MainCompact,
    SourceControl,
    Diff,
    Settings,
    Spawn,
}

impl View {
    const ALL: [Self; 6] = [
        Self::Main,
        Self::MainCompact,
        Self::SourceControl,
        Self::Diff,
        Self::Settings,
        Self::Spawn,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::MainCompact => "main-compact",
            Self::SourceControl => "source-control",
            Self::Diff => "diff",
            Self::Settings => "settings",
            Self::Spawn => "spawn",
        }
    }

    fn parse(name: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|view| view.name() == name)
            .ok_or_else(|| {
                let names: Vec<&str> = Self::ALL.iter().map(|view| view.name()).collect();
                anyhow!("unknown view {name:?}; the views are {}", names.join(", "))
            })
    }

    /// Whether `root` shows this view.
    fn reached(self, root: &RootView) -> bool {
        match self {
            Self::Main | Self::MainCompact => {
                !root.sidebar_containers().is_empty() && !root.tab_ids().is_empty()
            }
            Self::SourceControl => root.activity() == Activity::SourceControl,
            Self::Diff => root.active_tab_id() == Some(DIFF_TAB),
            Self::Settings => root.settings_open(),
            Self::Spawn => root.spawn_dialog_open(),
        }
    }
}

/// What to show and where to write it.
struct Shot {
    view: View,
    out: PathBuf,
    ui_dir: PathBuf,
}

impl Shot {
    fn from_args(mut args: impl Iterator<Item = String>) -> Result<Self> {
        let view = View::parse(args.next().as_deref().unwrap_or("main"))?;
        let out = match args.next() {
            Some(out) => PathBuf::from(out),
            None => default_dir()?.join(format!("{}.png", view.name())),
        };
        if let Some(extra) = args.next() {
            bail!("unexpected argument {extra:?}; usage: shot [<view>] [<out.png>]");
        }
        let out = std::path::absolute(&out)
            .with_context(|| format!("resolving the output path {}", out.display()))?;
        let dir = out
            .parent()
            .with_context(|| format!("the output path {} has no folder", out.display()))?;
        let ui_dir = dir.join(".ui");
        Ok(Self { view, out, ui_dir })
    }

    /// Creates the output folders and clears the layout an earlier run saved,
    /// so every run starts from the same state; a `main-compact` shot then
    /// seeds the sidebar's compact leaf density, which no keystroke in a
    /// cloaked window can reach.
    fn prepare(&self) -> Result<()> {
        std::fs::create_dir_all(&self.ui_dir)
            .with_context(|| format!("creating {}", self.ui_dir.display()))?;
        let saved = self.ui_dir.join("native-ui.json");
        if let Err(err) = std::fs::remove_file(&saved)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            return Err(err).with_context(|| format!("clearing {}", saved.display()));
        }
        if self.view != View::MainCompact {
            return Ok(());
        }
        let layout = serde_json::to_vec_pretty(&compact_ui_state())
            .with_context(|| format!("serializing the layout for {}", saved.display()))?;
        std::fs::write(&saved, layout).with_context(|| format!("writing {}", saved.display()))
    }
}

/// The layout a `main-compact` shot starts from: the sidebar's leaves one
/// line each. A file holding the general section alone loads as the layout
/// defaults everywhere else, which is what a missing file loads as.
fn compact_ui_state() -> Value {
    json!({ "general": { "leaf_density": "compact" } })
}

/// `.tmp/shots` under the repo root.
fn default_dir() -> Result<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .context("the native client's folder has no repo root two levels up")?;
    Ok(root.join(".tmp").join("shots"))
}

/// Opens the window on the fake daemon, then drives it to the view and
/// captures it; `slot` receives the outcome and the app quits.
fn start(shot: Shot, slot: Rc<RefCell<Option<Result<()>>>>, cx: &mut App) -> Result<()> {
    bind_keys(cx);
    fonts::register_bundled(cx);
    let (cmd_tx, cmd_rx) = unbounded();
    let (event_tx, event_rx) = unbounded();
    let now = Utc::now();
    let fixture = Fixture::build(shot.view, now)?;
    let handle = open_window(deps(cmd_tx, event_rx, &shot.ui_dir), cx)?;
    let address = handle.update(cx, |_, window, _| win32::window_address(window))??;
    let failure = Arc::new(Mutex::new(None));
    let daemon = FakeDaemon {
        events: event_tx,
        now,
        failure: Arc::clone(&failure),
    };
    std::thread::Builder::new()
        .name("fake-daemon".to_owned())
        .spawn(move || daemon.run(&fixture, cmd_rx))
        .context("starting the fake daemon's thread")?;
    cx.spawn(async move |cx| {
        let result = drive_and_capture(handle, &shot, address, cx, &failure).await;
        *slot.borrow_mut() = Some(result);
        if let Err(err) = cx.update(|cx| cx.quit()) {
            tracing::warn!("quitting after the shot: {err:#}");
        }
    })
    .detach();
    Ok(())
}

/// The root view's dependencies: the fake daemon's channels, a frozen clock,
/// and an opener, notifier and folder picker that do nothing.
fn deps(
    tx: UnboundedSender<NetCommand>,
    events: UnboundedReceiver<NetEvent>,
    ui_dir: &Path,
) -> RootDeps {
    let started = Instant::now();
    let now: Clock = Arc::new(move || started);
    let pick_folder: FolderPicker =
        Rc::new(|_: &mut App, _: Option<PathBuf>| futures::future::ready(None).boxed_local());
    RootDeps {
        tx,
        events,
        ui_dir: Some(ui_dir.to_path_buf()),
        paths: Err("the shot has no config dir".to_owned()),
        wanted: None,
        now,
        quit: Box::new(|cx: &mut App| cx.quit()),
        open: Arc::new(NoOpener),
        notify: Arc::new(NoNotifier),
        pick_folder,
    }
}

/// Opens the root view in a cloaked window that is never activated.
fn open_window(deps: RootDeps, cx: &mut App) -> Result<WindowHandle<RootView>> {
    let (width, height) = WINDOW_SIZE;
    let bounds = Bounds::centered(None, size(px(width.into()), px(height.into())), cx);
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            // gpui activates any window it shows, so it opens hidden and
            // `show_cloaked` shows it.
            focus: false,
            show: false,
            ..Default::default()
        },
        move |window, cx| {
            show_cloaked(window);
            cx.new(|cx| RootView::with_transport(deps, window, cx))
        },
    )
    .context("opening the window")
}

/// Waits for the fixture to draw, drives the window to the view, checks it
/// got there, then captures it on a thread of its own.
async fn drive_and_capture(
    handle: WindowHandle<RootView>,
    shot: &Shot,
    address: usize,
    cx: &mut AsyncApp,
    failure: &Mutex<Option<anyhow::Error>>,
) -> Result<()> {
    let view = shot.view;
    cx.background_executor().timer(LOAD_WAIT).await;
    cx.update_window(handle.into(), |_, window, cx| {
        drive(view, address, window, cx)
    })??;
    cx.background_executor().timer(DRIVE_WAIT).await;
    let reached = handle.update(cx, |root, _, _| view.reached(root))?;
    if !reached {
        bail!("the input for the {} view did not open it", view.name());
    }
    check_daemon(failure)?;
    let (tx, rx) = oneshot::channel();
    let out = shot.out.clone();
    std::thread::Builder::new()
        .name("capture".to_owned())
        .spawn(move || {
            if tx.send(win32::write_settled(address, &out)).is_err() {
                tracing::warn!("the capture finished after the app stopped waiting");
            }
        })
        .context("starting the capture thread")?;
    let settled = rx
        .await
        .context("the capture thread ended without an answer")??;
    check_daemon(failure)?;
    if !settled {
        tracing::warn!("the window kept changing; wrote its last capture");
    }
    Ok(())
}

/// Sends the input that opens `view`: a keystroke dispatched in the window,
/// or, where no key opens it, a click posted to the window at `address`.
fn drive(view: View, address: usize, window: &mut Window, cx: &mut App) -> Result<()> {
    match view {
        View::Main | View::MainCompact => Ok(()),
        View::SourceControl => {
            let y = (SOURCE_CONTROL_ITEM + 0.5) * RAIL_ITEM_HEIGHT;
            #[expect(
                clippy::cast_possible_truncation,
                reason = "the rail's middle lies within a few hundred logical pixels"
            )]
            let at = ((RAIL_WIDTH / 2.0).round() as i32, y.round() as i32);
            win32::click(address, at)
        }
        View::Diff => press("ctrl-3", window, cx),
        View::Settings => press("ctrl-,", window, cx),
        View::Spawn => press("ctrl-shift-n", window, cx),
    }
}

fn press(key: &str, window: &mut Window, cx: &mut App) -> Result<()> {
    let keystroke =
        Keystroke::parse(key).map_err(|err| anyhow!("parsing the keystroke {key}: {err:?}"))?;
    window.dispatch_keystroke(keystroke, cx);
    Ok(())
}

/// An opener that opens nothing: the shot never leaves its window.
struct NoOpener;

impl Opener for NoOpener {
    fn url(&self, _url: &str) -> Result<(), String> {
        Err("the shot opens nothing".to_owned())
    }

    fn vscode(&self, _path: &Path, _line: u32, _column: u32) -> Result<(), OpenFailure> {
        Err(OpenFailure::Failed("the shot opens nothing".to_owned()))
    }

    fn default_app(&self, _path: &Path) -> Result<(), String> {
        Err("the shot opens nothing".to_owned())
    }

    fn reveal(&self, _path: &Path) -> Result<(), String> {
        Err("the shot opens nothing".to_owned())
    }

    fn mapped_unc_hosts(&self) -> Vec<String> {
        Vec::new()
    }

    fn is_dangerous_type(&self, _extension: &str) -> bool {
        true
    }

    fn existing(&self, _reading: &Path) -> Option<Result<PathBuf, String>> {
        None
    }
}

/// A notifier that shows nothing and reports notifications on.
struct NoNotifier;

impl Notifier for NoNotifier {
    fn notify(&self, _title: &str, _body: &str) {}

    fn state(&self) -> NotifyState {
        NotifyState::On
    }
}

/// What the fake daemon holds at connect.
struct Fixture {
    repos: DaemonMessage,
    workspaces: DaemonMessage,
    sessions: Vec<SessionSnapshot>,
    tabs: DaemonMessage,
    keep_awake: DaemonMessage,
    attention: DaemonMessage,
}

impl Fixture {
    /// Two repos, a workspace of two more (a workspace member's own
    /// sessions would file under Detached), seven sessions, and two tabs
    /// (the first a split); the diff view adds a diff tab third. Every
    /// fixed message is parsed here, so a change to the wire shape fails
    /// before the window opens rather than mid-shot.
    fn build(view: View, now: DateTime<Utc>) -> Result<Self> {
        let repos = parse_message(json!({
            "type": "repos",
            "repos": [
                { "id": TULIP, "name": TULIP, "path": TULIP_PATH, "default_branch": "main" },
                { "id": YAAT, "name": YAAT, "path": YAAT_PATH, "default_branch": "main" },
                { "id": DECK_SERVER, "name": DECK_SERVER, "path": DECK_SERVER_PATH, "default_branch": "main" },
                { "id": DECK_CLIENT, "name": DECK_CLIENT, "path": DECK_CLIENT_PATH, "default_branch": "main" },
            ],
        }))
        .context("the fixture's repos")?;
        let workspaces = parse_message(json!({
            "type": "workspaces",
            "workspaces": [
                { "id": WORKSPACE, "name": WORKSPACE, "member_repo_ids": [DECK_SERVER, DECK_CLIENT] },
            ],
        }))
        .context("the fixture's workspaces")?;
        let sessions = session_rows()
            .into_iter()
            .map(|row| row.snapshot(now))
            .collect::<Result<Vec<_>>>()?;
        let mut tabs = vec![
            grid_tab("t-tulip", "tulip", &tulip_grid()),
            grid_tab("t-yaat", "yaat", &pane("p3", "s-metar")),
        ];
        if view == View::Diff {
            tabs.push(json!({
                "id": DIFF_TAB,
                "name": DIFF_PATH,
                "content": {
                    "kind": "diff", "repo_id": TULIP, "path": DIFF_PATH,
                    "against": null, "worktree_path": PETAL_TREE,
                },
                "created_at": "2026-01-01T00:00:00Z",
            }));
        }
        check_reply_shapes(now)?;
        Ok(Self {
            repos,
            workspaces,
            sessions,
            tabs: parse_message(json!({ "type": "tabs", "tabs": tabs }))
                .context("the fixture's tabs")?,
            keep_awake: parse_message(json!({
                "type": "keep_awake_status", "enabled": true, "active": true,
            }))
            .context("the fixture's keep-awake status")?,
            attention: parse_message(json!({
                "type": "attention", "session_id": "s-recover", "reason": "awaiting_input",
            }))
            .context("the fixture's attention notice")?,
        })
    }
}

/// Parses `value` into the daemon message it names.
fn parse_message(value: Value) -> Result<DaemonMessage> {
    serde_json::from_value(value).context("a fake daemon message")
}

/// Builds and parses one sample of every reply the daemon builds per
/// request, so a shape change fails when the fixture is built rather than
/// mid-shot.
fn check_reply_shapes(now: DateTime<Utc>) -> Result<()> {
    for sample in [
        repo_status(TULIP, None),
        stashes(now, TULIP, None),
        commits(now, TULIP, 0, None),
        branches(TULIP),
    ] {
        parse_message(sample)?;
    }
    Ok(())
}

fn pane(id: &str, session: &str) -> GridNode {
    GridNode::Pane {
        pane_id: id.to_owned(),
        session_id: Some(session.to_owned()),
    }
}

/// The Petal session above its shell.
fn tulip_grid() -> GridNode {
    GridNode::Split {
        direction: SplitDirection::Vertical,
        ratio: 0.58,
        first: Box::new(pane("p1", "s-petal")),
        second: Box::new(pane("p2", "s-shell")),
    }
}

fn grid_tab(id: &str, name: &str, grid: &GridNode) -> Value {
    json!({
        "id": id,
        "name": name,
        "content": { "kind": "grid", "grid": grid },
        "created_at": "2026-01-01T00:00:00Z",
    })
}

fn member(repo: &str, branch: &str, tree: &str) -> Value {
    json!({ "repo_id": repo, "repo_name": repo, "branch": branch, "worktree_path": tree })
}

/// One session of the fixture: its status changed `minutes` ago, and
/// `extra` holds the snapshot fields past the common ones.
struct Row {
    id: &'static str,
    label: &'static str,
    status: &'static str,
    members: Value,
    minutes: i64,
    extra: Value,
}

impl Row {
    fn snapshot(self, now: DateTime<Utc>) -> Result<SessionSnapshot> {
        let since = now - chrono::Duration::minutes(self.minutes);
        let mut value = json!({
            "id": self.id,
            "label": self.label,
            "kind": "single",
            "members": self.members,
            "status": self.status,
            "status_since": since,
            "mode": "interactive",
            "started_at": since - chrono::Duration::minutes(40),
            "exit_code": null,
            "metrics": { "input_tokens": 0, "output_tokens": 0, "cost_usd": 0.0, "last_activity_at": since },
            "recent_actions": [],
            "agent": "claude",
        });
        if let (Some(fields), Value::Object(extra)) = (value.as_object_mut(), self.extra) {
            fields.extend(extra);
        }
        serde_json::from_value(value).with_context(|| format!("the session fixture {}", self.id))
    }
}

fn session_rows() -> Vec<Row> {
    vec![
        Row {
            id: "s-petal",
            label: "Petal footer polish",
            status: "working",
            members: json!([member(TULIP, PETAL_BRANCH, PETAL_TREE)]),
            minutes: 2,
            extra: json!({
                "has_per_session_worktree": true,
                "worktree_paths": [PETAL_TREE],
                "metrics": { "input_tokens": 182_400, "output_tokens": 9_870, "cost_usd": 1.42, "last_activity_at": null },
                "terminal_title": "Petal footer polish",
            }),
        },
        Row {
            id: "s-shell",
            label: "pwsh",
            status: "idle",
            members: json!([member(TULIP, "main", TULIP_PATH)]),
            minutes: 4,
            extra: json!({ "mode": "plain_shell", "current_cwd": TULIP_PATH, "program_name": "pwsh" }),
        },
        Row {
            id: "s-recover",
            label: "Recover dialog keys",
            status: "awaiting_input",
            members: json!([member(TULIP, "feat/recover-keys", RECOVER_TREE)]),
            minutes: 6,
            extra: json!({ "has_per_session_worktree": true, "worktree_paths": [RECOVER_TREE] }),
        },
        Row {
            id: "s-codex",
            label: "Review diff model",
            status: "idle",
            members: json!([member(TULIP, "main", TULIP_PATH)]),
            minutes: 1,
            extra: json!({ "agent": "codex", "program_name": "codex" }),
        },
        Row {
            id: "s-strip",
            label: "Flight strip layout",
            status: "spawning",
            members: json!([member(YAAT, "feat/strip-layout", YAAT_PATH)]),
            minutes: 0,
            extra: json!({}),
        },
        Row {
            id: "s-metar",
            label: "METAR parser fix",
            status: "idle",
            members: json!([member(YAAT, "main", YAAT_PATH)]),
            minutes: 25,
            extra: json!({}),
        },
        Row {
            id: "s-proto",
            label: "Protocol v24 bump",
            status: "idle",
            members: json!([
                member(DECK_SERVER, "feat/proto-v24", DECK_SERVER_PATH),
                member(DECK_CLIENT, "feat/proto-v24", DECK_CLIENT_PATH),
            ]),
            minutes: 48,
            extra: json!({ "kind": "workspace", "workspace_id": WORKSPACE }),
        },
    ]
}

/// Records `err` as the failure that stopped the fake daemon.
fn record_failure(failure: &Mutex<Option<anyhow::Error>>, err: anyhow::Error) {
    *failure.lock().unwrap_or_else(PoisonError::into_inner) = Some(err);
}

/// Fails with the fake daemon's failure, if it stopped.
fn check_daemon(failure: &Mutex<Option<anyhow::Error>>) -> Result<()> {
    match failure
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
    {
        Some(err) => Err(err).context("the fake daemon stopped"),
        None => Ok(()),
    }
}

/// The daemon side of the shot: greets the view with the fixture, then
/// answers the requests the views make.
struct FakeDaemon {
    events: UnboundedSender<NetEvent>,
    now: DateTime<Utc>,
    failure: Arc<Mutex<Option<anyhow::Error>>>,
}

impl FakeDaemon {
    fn run(&self, fixture: &Fixture, mut commands: UnboundedReceiver<NetCommand>) {
        if let Err(err) = self.greet(fixture) {
            self.fail(err);
            return;
        }
        while let Some(command) = futures::executor::block_on(commands.next()) {
            let NetCommand::Send(msg) = command else {
                continue;
            };
            let sent = self
                .answer(&msg)
                .and_then(|replies| replies.into_iter().try_for_each(|reply| self.send(reply)));
            if let Err(err) = sent {
                self.fail(err);
                return;
            }
        }
    }

    /// Records the error that stopped the daemon, so the shot fails with it
    /// instead of writing a PNG of a half-filled client.
    fn fail(&self, err: anyhow::Error) {
        tracing::error!("the fake daemon stopped: {err:#}");
        record_failure(&self.failure, err);
    }

    fn event(&self, event: NetEvent) -> Result<()> {
        self.events
            .unbounded_send(event)
            .map_err(|_| anyhow!("the window stopped listening"))
    }

    fn send(&self, msg: DaemonMessage) -> Result<()> {
        self.event(NetEvent::Message(Box::new(msg)))
    }

    /// The connection, the handshake and the lists a daemon sends first,
    /// then a finished Codex turn (unseen) and a session asking for input.
    fn greet(&self, fixture: &Fixture) -> Result<()> {
        let version = NATIVE_PROTOCOL_VERSIONS.first().copied().unwrap_or(1);
        let mut conn = Connection::new();
        conn.on_connecting();
        conn.on_socket_open(FAKE_PORT);
        conn.on_welcome(version);
        self.event(NetEvent::State(conn))?;
        self.event(NetEvent::Handshake(HandshakeInfo {
            port: FAKE_PORT,
            pid: FAKE_PID,
            protocol_version: version,
        }))?;
        self.send(DaemonMessage::Welcome {
            protocol_version: version,
            supported_versions: vec![version],
        })?;
        self.send(fixture.repos.clone())?;
        self.send(fixture.workspaces.clone())?;
        self.send(DaemonMessage::Sessions {
            sessions: fixture.sessions.clone(),
        })?;
        self.send(fixture.tabs.clone())?;
        self.send(fixture.keep_awake.clone())?;
        self.finish_codex_turn(fixture)?;
        self.send(fixture.attention.clone())
    }

    /// The Codex session works, then goes idle while nobody looks at it.
    fn finish_codex_turn(&self, fixture: &Fixture) -> Result<()> {
        let codex = fixture
            .sessions
            .iter()
            .find(|session| session.id == "s-codex")
            .context("the fixture has no Codex session")?;
        for status in [
            protocol::SessionStatus::Working,
            protocol::SessionStatus::Idle,
        ] {
            let mut session = codex.clone();
            session.status = status;
            session.status_since = Some(self.now);
            self.send(DaemonMessage::SessionUpdated {
                session,
                request_id: None,
            })?;
        }
        Ok(())
    }

    /// The replies to `msg`; requests the views do not need go unanswered.
    fn answer(&self, msg: &ClientMessage) -> Result<Vec<DaemonMessage>> {
        let reply = match msg {
            ClientMessage::LoadScrollback {
                session_id,
                request_id,
            } => DaemonMessage::Scrollback {
                session_id: session_id.clone(),
                data_b64: B64.encode(screen_for(session_id)),
                truncated: false,
                request_id: request_id.clone(),
                forwarder_restarted: true,
            },
            ClientMessage::RepoStatus {
                repo_id,
                worktree_path,
                ..
            } => parse_message(repo_status(repo_id, worktree_path.as_deref()))?,
            ClientMessage::ListStashes {
                repo_id,
                worktree_path,
                ..
            } => parse_message(stashes(self.now, repo_id, worktree_path.as_deref()))?,
            ClientMessage::ListCommits {
                repo_id,
                offset,
                worktree_path,
                ..
            } => parse_message(commits(
                self.now,
                repo_id,
                *offset,
                worktree_path.as_deref(),
            ))?,
            ClientMessage::GetFileSnapshot { .. } => file_snapshot(msg)?,
            ClientMessage::ListBranches { repo_id } => parse_message(branches(repo_id))?,
            ClientMessage::SuggestBranchName { target } => DaemonMessage::BranchNameSuggestion {
                target: target.clone(),
                name: "feat/sidebar-density".to_owned(),
            },
            _ => return Ok(Vec::new()),
        };
        Ok(vec![reply])
    }
}

/// The stashes a repo reports, as of `now`.
fn stashes(now: DateTime<Utc>, repo_id: &str, worktree_path: Option<&str>) -> Value {
    let at = |hours: i64| (now - chrono::Duration::hours(hours)).to_rfc3339();
    json!({
        "type": "stashes",
        "repo_id": repo_id,
        "worktree_path": worktree_path,
        "stashes": [
            { "id": "stash@{0}", "subject": "On feat/petal-footer: try a darker pill border", "created_at": at(3) },
            { "id": "stash@{1}", "subject": "WIP on main: 73ee3ee feat: recover Codex and Cursor rows as their own agent", "created_at": at(50) },
        ],
    })
}

/// The commits a repo reports, as of `now`: one page, then nothing.
fn commits(now: DateTime<Utc>, repo_id: &str, offset: u32, worktree_path: Option<&str>) -> Value {
    let subjects = [
        (
            "3de017a",
            "docs: plan agent CLI self-update and client screenshots for agents",
        ),
        (
            "87a6270",
            "docs: record Petal build-time rulings for PT.5-PT.9",
        ),
        (
            "3ee329a",
            "docs: plan built-in claude-swap accounts and CLI self-update",
        ),
        (
            "e35c200",
            "docs: land marked features through a feature branch PR",
        ),
        (
            "73ee3ee",
            "feat: recover Codex and Cursor rows as their own agent",
        ),
    ];
    let commits: Vec<Value> = if offset == 0 {
        (1_i64..)
            .zip(subjects)
            .map(|(hours, (short, subject))| {
                json!({
                    "sha": format!("{short}{}", "0".repeat(33)),
                    "short_sha": short,
                    "author_name": "Leftos Aslanoglou",
                    "author_email": "dev@example.com",
                    "authored_at": (now - chrono::Duration::hours(hours * 5)).to_rfc3339(),
                    "subject": subject,
                })
            })
            .collect()
    } else {
        Vec::new()
    };
    json!({
        "type": "commits",
        "repo_id": repo_id,
        "commits": commits,
        "offset": offset,
        "worktree_path": worktree_path,
    })
}

fn change(path: &str, status: &str) -> Value {
    json!({ "path": path, "status": status, "from_path": null })
}

/// Two staged and three unstaged files, for any repo and tree.
fn repo_status(repo_id: &str, worktree_path: Option<&str>) -> Value {
    json!({
        "type": "repo_status",
        "repo_id": repo_id,
        "worktree_path": worktree_path,
        "index_changes": [
            change("apps/native/src/footer.rs", "M"),
            change("apps/native/assets/icons/petal.svg", "A"),
        ],
        "worktree_changes": [
            change("apps/native/src/palette.rs", "M"),
            change("apps/native/src/sidebar_view.rs", "M"),
            change("docs/native-client.md", "M"),
        ],
    })
}

fn branches(repo_id: &str) -> Value {
    json!({
        "type": "branches",
        "repo_id": repo_id,
        "branches": ["main", PETAL_BRANCH, "feat/recover-keys", "feat/proto-v24"],
        "current": "main",
        "remote_branches": ["origin/main", "origin/feat/petal-footer"],
    })
}

/// The snapshot a `GetFileSnapshot` asks for: the footer before and after
/// the Petal session's edits, whatever path it names.
fn file_snapshot(msg: &ClientMessage) -> Result<DaemonMessage> {
    let ClientMessage::GetFileSnapshot {
        id,
        repo_id,
        path,
        against,
        worktree_path,
    } = msg
    else {
        bail!("not a file snapshot request");
    };
    Ok(DaemonMessage::FileSnapshot {
        id: id.clone(),
        repo_id: repo_id.clone(),
        path: path.clone(),
        against: against.clone(),
        old: FOOTER_OLD.to_owned(),
        new: footer_new()?,
        language: "rust".to_owned(),
        unavailable: None,
        worktree_path: worktree_path.clone(),
    })
}

/// The screen a session's scrollback replays.
fn screen_for(session_id: &str) -> String {
    let lines = match session_id {
        "s-shell" => SHELL_SCREEN,
        "s-codex" => CODEX_SCREEN,
        "s-petal" => CLAUDE_WORKING_SCREEN,
        _ => CLAUDE_IDLE_SCREEN,
    };
    lines.join("\r\n")
}

const CLAUDE_WORKING_SCREEN: &[&str] = &[
    "\x1b[90m>\x1b[0m Tighten the footer pill spacing and use the petal palette",
    "",
    "\x1b[32m●\x1b[0m \x1b[1mRead\x1b[0m(apps/native/src/footer.rs)",
    "  \x1b[90m⎿  Read 212 lines\x1b[0m",
    "",
    "\x1b[32m●\x1b[0m \x1b[1mUpdate\x1b[0m(apps/native/src/footer.rs)",
    "  \x1b[90m⎿  Updated footer.rs with 6 additions and 3 removals\x1b[0m",
    "",
    "\x1b[37m●\x1b[0m The pill now sits PILL_GAP (6 px) from the status text",
    "  and takes the petal accent. Running the sidebar specs next.",
    "",
    "\x1b[32m●\x1b[0m \x1b[1mBash\x1b[0m(cargo test -p rustling-tulip-native --test ui_sidebar)",
    "  \x1b[90m⎿  Running…\x1b[0m",
    "",
    "\x1b[38;5;209m✻ Brewing…\x1b[0m \x1b[90m(38s · ↑ 2.1k tokens · esc to interrupt)\x1b[0m",
    "\x1b[90m──────────────────────────────────────────────────────────────────\x1b[0m",
    "> ",
    "\x1b[90m──────────────────────────────────────────────────────────────────\x1b[0m",
    "  \x1b[38;5;177m⏵⏵ accept edits on\x1b[0m \x1b[90m(shift+tab to cycle)\x1b[0m\x1b[?25l",
];

const CLAUDE_IDLE_SCREEN: &[&str] = &[
    "\x1b[90m>\x1b[0m Summarize what changed since the last release",
    "",
    "\x1b[37m●\x1b[0m Three changes landed since v0.9.2: Codex and Cursor rows",
    "  recover as their own agent, the METAR parser accepts RMK groups,",
    "  and the strip layout keeps its column widths across restarts.",
    "",
    "\x1b[90m──────────────────────────────────────────────────────────────────\x1b[0m",
    "> ",
    "\x1b[90m──────────────────────────────────────────────────────────────────\x1b[0m",
    "  \x1b[90m? for shortcuts\x1b[0m\x1b[?25l",
];

const CODEX_SCREEN: &[&str] = &[
    "\x1b[1m>_ OpenAI Codex\x1b[0m \x1b[90m(v0.46.0)\x1b[0m",
    "",
    "\x1b[36m›\x1b[0m Review diff_model.rs for the hunk-merge edge cases",
    "",
    "\x1b[90m•\x1b[0m Two hunks one context line apart merge into one; the tests",
    "  cover a gap of three but not of one. Added that case.",
    "",
    "\x1b[36m›\x1b[0m ",
];

const SHELL_SCREEN: &[&str] = &[
    "\x1b[32mPS\x1b[0m D:\\src\\rustling-tulip> git log --oneline -3",
    "\x1b[33m3de017a\x1b[0m (\x1b[36mHEAD -> main\x1b[0m, \x1b[31morigin/main\x1b[0m) docs: plan agent CLI self-update",
    "\x1b[33m87a6270\x1b[0m docs: record Petal build-time rulings for PT.5-PT.9",
    "\x1b[33m3ee329a\x1b[0m docs: plan built-in claude-swap accounts and CLI self-update",
    "\x1b[32mPS\x1b[0m D:\\src\\rustling-tulip> cargo test -p protocol v22_compat",
    "\x1b[1;32m    Finished\x1b[0m `test` profile [unoptimized + debuginfo] target(s) in 0.41s",
    "running 3 tests",
    "test v22_compat::decodes_v22_session_snapshot ... \x1b[32mok\x1b[0m",
    "test v22_compat::decodes_v22_tabs ... \x1b[32mok\x1b[0m",
    "test v22_compat::keeps_unknown_types_readable ... \x1b[32mok\x1b[0m",
    "",
    "test result: \x1b[32mok\x1b[0m. 3 passed; 0 failed; 0 ignored; 0 measured; 118 filtered out",
    "",
    "\x1b[32mPS\x1b[0m D:\\src\\rustling-tulip> ",
];

/// The footer before the Petal session's edits.
const FOOTER_OLD: &str = r"//! The footer: the connection pill, the status text and the log flyout.

use gpui::{Div, SharedString, div, prelude::*, px};

use crate::palette::{BAR_BG, BORDER, MUTED, TEXT};

/// The pill's height in logical pixels.
const PILL_HEIGHT: f32 = 16.0;
/// Space between the pill and the status text.
const PILL_GAP: f32 = 4.0;

/// What the footer shows.
pub struct Footer {
    pub label: SharedString,
    pub status: SharedString,
    pub connected: bool,
}

impl Footer {
    /// The pill: the connection label in a rounded border.
    fn pill(&self) -> Div {
        let color = if self.connected { TEXT } else { MUTED };
        div()
            .flex()
            .items_center()
            .h(px(PILL_HEIGHT))
            .px(px(6.0))
            .rounded_full()
            .border_1()
            .border_color(gpui::rgb(BORDER))
            .text_color(gpui::rgb(color))
            .child(self.label.clone())
    }

    /// The whole footer row.
    pub fn render(&self) -> Div {
        div()
            .flex()
            .items_center()
            .gap(px(PILL_GAP))
            .px(px(8.0))
            .bg(gpui::rgb(BAR_BG))
            .child(self.pill())
            .child(div().text_color(gpui::rgb(MUTED)).child(self.status.clone()))
    }
}
";

/// The Petal session's edits to [`FOOTER_OLD`], each `(from, to)`.
const FOOTER_EDITS: &[(&str, &str)] = &[
    ("BORDER, MUTED, TEXT};", "BORDER, MUTED, PETAL, TEXT};"),
    (
        "const PILL_GAP: f32 = 4.0;\n",
        "const PILL_GAP: f32 = 6.0;\n/// The status dot left of the pill's label.\nconst DOT_SIZE: f32 = 6.0;\n",
    ),
    (
        "    /// The pill: the connection label in a rounded border.\n",
        "    /// The pill: a status dot and the connection label in a rounded border.\n",
    ),
    ("{ TEXT } else { MUTED };", "{ PETAL } else { MUTED };"),
    (
        "            .text_color(gpui::rgb(color))\n            .child(self.label.clone())",
        "            .gap(px(4.0))\n            .text_color(gpui::rgb(TEXT))\n            .child(div().size(px(DOT_SIZE)).rounded_full().bg(gpui::rgb(color)))\n            .child(self.label.clone())",
    ),
    (
        "            .px(px(8.0))\n            .bg(gpui::rgb(BAR_BG))",
        "            .px(px(10.0))\n            .border_t_1()\n            .border_color(gpui::rgb(BORDER))\n            .bg(gpui::rgb(BAR_BG))",
    ),
];

/// The footer after the Petal session's edits.
fn footer_new() -> Result<String> {
    FOOTER_EDITS
        .iter()
        .try_fold(FOOTER_OLD.to_owned(), |text, (from, to)| {
            if text.contains(from) {
                Ok(text.replacen(from, to, 1))
            } else {
                Err(anyhow!("the footer fixture has no {from:?} to edit"))
            }
        })
}

/// Clicking the cloaked window, reading its pixels and writing them as a PNG.
#[cfg(windows)]
mod win32 {
    use std::path::Path;
    use std::time::{Duration, Instant};

    use anyhow::{Context as _, Result, anyhow, bail};
    use gpui::Window;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
    use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleBitmap, CreateCompatibleDC,
        DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, ReleaseDC, SelectObject,
    };
    use windows::Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PW_CLIENTONLY, PrintWindow};
    use windows::Win32::UI::HiDpi::GetDpiForWindow;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClientRect, GetForegroundWindow, PW_RENDERFULLCONTENT, PostMessageW, WM_LBUTTONDOWN,
        WM_LBUTTONUP, WM_MOUSEMOVE,
    };

    /// How long the window gets to stop changing between captures.
    const SETTLE_TIMEOUT: Duration = Duration::from_secs(5);
    /// The gap between two captures that must match.
    const CAPTURE_GAP: Duration = Duration::from_millis(250);
    /// `wParam` of a mouse message while the left button is down.
    const MK_LBUTTON: usize = 0x0001;
    /// Where the pointer is parked after a click: off the client area, so no
    /// hover or tooltip shows in the capture.
    const PARKED: (i32, i32) = (-10, -10);

    fn hwnd(address: usize) -> HWND {
        HWND(std::ptr::with_exposed_provenance_mut(address))
    }

    /// Clicks client point `at`, in logical pixels, of the window at
    /// `address` with posted mouse messages (a move, a press, a release), as
    /// the OS delivers a click, then parks the pointer off the client area.
    /// Posted messages reach only this window: the user's pointer and focus
    /// are untouched.
    pub fn click(address: usize, (x, y): (i32, i32)) -> Result<()> {
        let hwnd = hwnd(address);
        // SAFETY: a plain query on this live window; 0 means it has no DPI.
        let dpi = match unsafe { GetDpiForWindow(hwnd) } {
            0 => 96,
            dpi => i32::try_from(dpi).unwrap_or(96),
        };
        let at = coordinates(x * dpi / 96, y * dpi / 96);
        post(hwnd, WM_MOUSEMOVE, 0, at)?;
        post(hwnd, WM_LBUTTONDOWN, MK_LBUTTON, at)?;
        post(hwnd, WM_LBUTTONUP, 0, at)?;
        post(hwnd, WM_MOUSEMOVE, 0, coordinates(PARKED.0, PARKED.1))
    }

    /// Client pixel (`x`, `y`) packed as a mouse message's `lParam`: two
    /// signed 16-bit words, `y` high.
    fn coordinates(x: i32, y: i32) -> isize {
        let packed = ((y & 0xffff) << 16) | (x & 0xffff);
        isize::try_from(packed).unwrap_or_default()
    }

    fn post(hwnd: HWND, message: u32, wparam: usize, lparam: isize) -> Result<()> {
        // SAFETY: posts a plain input message to a live window.
        unsafe { PostMessageW(Some(hwnd), message, WPARAM(wparam), LPARAM(lparam)) }
            .with_context(|| format!("posting mouse message {message:#x} to the window"))
    }

    /// The address of `window`'s Win32 handle, which crosses threads where
    /// an `HWND` cannot.
    pub fn window_address(window: &Window) -> Result<usize> {
        // `Window::window_handle` is gpui's own handle; the raw one is the trait's.
        match HasWindowHandle::window_handle(window).map(|handle| handle.as_raw()) {
            Ok(RawWindowHandle::Win32(handle)) => Ok(handle.hwnd.get().cast_unsigned()),
            other => Err(anyhow!("the window has no Win32 handle ({other:?})")),
        }
    }

    /// A capture of the client area: top-down rows of BGRA pixels.
    #[derive(PartialEq, Eq)]
    struct Frame {
        width: u32,
        height: u32,
        bgra: Vec<u8>,
    }

    /// Captures the window at `address` until two captures in a row match or
    /// [`SETTLE_TIMEOUT`] passes, and writes the last one to `out`. Returns
    /// whether the window settled.
    pub fn write_settled(address: usize, out: &Path) -> Result<bool> {
        let hwnd = hwnd(address);
        let deadline = Instant::now() + SETTLE_TIMEOUT;
        let mut last = capture(hwnd)?;
        let settled = loop {
            std::thread::sleep(CAPTURE_GAP);
            let next = capture(hwnd)?;
            let same = next == last;
            last = next;
            if same {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
        };
        let blank = last
            .bgra
            .as_chunks::<4>()
            .0
            .iter()
            .all(|&[blue, green, red, _]| [blue, green, red] == [0, 0, 0]);
        if blank {
            bail!("the window captured black: it drew nothing");
        }
        write_png(&last, out)?;
        Ok(settled)
    }

    /// The client area through `PrintWindow` with `PW_RENDERFULLCONTENT`,
    /// which reads the compositor's copy of a cloaked window without
    /// showing, moving or activating it.
    fn capture(hwnd: HWND) -> Result<Frame> {
        let mut rect = RECT::default();
        // SAFETY: an out-pointer to a local, on a live window handle.
        unsafe { GetClientRect(hwnd, &raw mut rect) }.context("reading the client rect")?;
        let (width, height) = (rect.right, rect.bottom);
        if width <= 0 || height <= 0 {
            bail!("the window has no client area ({width}x{height})");
        }
        let pixels = usize::try_from(width)? * usize::try_from(height)?;
        let mut bgra = vec![0u8; pixels * 4];
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: u32::try_from(size_of::<BITMAPINFOHEADER>())?,
                biWidth: width,
                // Negative: top-down rows.
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let flags = PRINT_WINDOW_FLAGS(PW_CLIENTONLY.0 | PW_RENDERFULLCONTENT);
        let rows = u32::try_from(height)?;
        // SAFETY: the DCs and bitmap are created, used and released here, on
        // a live window; `bgra` holds the bitmap's full size in the format
        // `info` describes.
        let (printed, lines) = unsafe {
            let screen = GetDC(None);
            let dc = CreateCompatibleDC(Some(screen));
            let bitmap = CreateCompatibleBitmap(screen, width, height);
            let previous = SelectObject(dc, bitmap.into());
            let printed = PrintWindow(hwnd, dc, flags);
            SelectObject(dc, previous);
            let bits = Some(bgra.as_mut_ptr().cast());
            let lines = GetDIBits(dc, bitmap, 0, rows, bits, &raw mut info, DIB_RGB_COLORS);
            let _ = DeleteObject(bitmap.into());
            let _ = DeleteDC(dc);
            ReleaseDC(None, screen);
            (printed.as_bool(), lines)
        };
        if !printed {
            bail!("PrintWindow failed on the shot window");
        }
        if lines != height {
            bail!("GetDIBits copied {lines} of {height} rows");
        }
        check_out_of_the_way(hwnd)?;
        Ok(Frame {
            width: u32::try_from(width)?,
            height: rows,
            bgra,
        })
    }

    /// Fails unless the window is still cloaked and not the foreground
    /// window: the shot must never show on screen or take the user's focus.
    fn check_out_of_the_way(hwnd: HWND) -> Result<()> {
        let mut cloaked: u32 = 0;
        let size = u32::try_from(size_of::<u32>())?;
        // SAFETY: an out-pointer to a local of the size passed, on a live
        // window handle.
        unsafe { DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, (&raw mut cloaked).cast(), size) }
            .context("reading the window's cloaked state")?;
        if cloaked == 0 {
            bail!("the shot window is not cloaked, so it may show on screen");
        }
        // SAFETY: a plain query.
        if unsafe { GetForegroundWindow() } == hwnd {
            bail!("the shot window became the foreground window");
        }
        Ok(())
    }

    /// Writes `frame` to `out` as an RGBA PNG; the capture's alpha is
    /// undefined, so every pixel is written opaque.
    fn write_png(frame: &Frame, out: &Path) -> Result<()> {
        let mut rgba = Vec::with_capacity(frame.bgra.len());
        for &[blue, green, red, _] in frame.bgra.as_chunks::<4>().0 {
            rgba.extend_from_slice(&[red, green, blue, u8::MAX]);
        }
        let file =
            std::fs::File::create(out).with_context(|| format!("creating {}", out.display()))?;
        let mut encoder =
            png::Encoder::new(std::io::BufWriter::new(file), frame.width, frame.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().context("writing the PNG header")?;
        writer
            .write_image_data(&rgba)
            .context("writing the PNG pixels")?;
        writer.finish().context("finishing the PNG")
    }
}

/// The shot drives and reads a Win32 window.
#[cfg(not(windows))]
mod win32 {
    use std::path::Path;

    use anyhow::{Result, bail};
    use gpui::Window;

    pub fn window_address(_window: &Window) -> Result<usize> {
        bail!("the shot reads a Win32 window's pixels, so it runs on Windows only")
    }

    pub fn click(_address: usize, _at: (i32, i32)) -> Result<()> {
        bail!("the shot clicks a Win32 window, so it runs on Windows only")
    }

    pub fn write_settled(_address: usize, _out: &Path) -> Result<bool> {
        bail!("the shot reads a Win32 window's pixels, so it runs on Windows only")
    }
}

/// The fixture is parsed before the window opens, so a change to the wire
/// shape fails here rather than in a PNG nobody looks at.
#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;

    #[test]
    fn every_view_has_a_fixture() {
        for view in View::ALL {
            let built = Fixture::build(view, Utc::now());
            assert!(
                built.is_ok(),
                "the {} fixture: {:?}",
                view.name(),
                built.err()
            );
        }
    }

    #[test]
    fn a_message_that_is_malformed_does_not_parse() {
        let malformed = parse_message(json!({ "type": "repos" }));
        assert!(
            malformed.is_err(),
            "a repos message without its repos field parsed"
        );
    }

    #[test]
    fn main_compact_seeds_compact_density() {
        let dir = scratch_dir("main-compact");
        shot(View::MainCompact, dir.clone())
            .prepare()
            .expect("preparing the main-compact shot");
        let file = dir.join("native-ui.json");
        let text = std::fs::read_to_string(&file).expect("the layout the shot saved");
        let layout: Value = serde_json::from_str(&text).expect("the saved layout parses");
        assert_eq!(
            layout
                .pointer("/general/leaf_density")
                .and_then(Value::as_str),
            Some("compact"),
            "the shot saved {text}"
        );
    }

    #[test]
    fn main_clears_saved_ui_state() {
        let dir = scratch_dir("main-clears");
        shot(View::MainCompact, dir.clone())
            .prepare()
            .expect("preparing the main-compact shot");
        let file = dir.join("native-ui.json");
        assert!(file.exists(), "the main-compact shot saved no layout");
        shot(View::Main, dir.clone())
            .prepare()
            .expect("preparing the main shot");
        assert!(!file.exists(), "the main shot left a saved layout behind");
    }

    /// A shot whose layout file lands in a `.ui` folder of its own under the
    /// repo's `.tmp/`, named after the test that asked for it.
    fn shot(view: View, ui_dir: PathBuf) -> Shot {
        Shot {
            view,
            out: ui_dir.join("shot.png"),
            ui_dir,
        }
    }

    /// `.tmp/shot-tests/<name>/.ui` under the repo root.
    fn scratch_dir(name: &str) -> PathBuf {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("the native client's folder has no repo root two levels up");
        root.join(".tmp").join("shot-tests").join(name).join(".ui")
    }
}
