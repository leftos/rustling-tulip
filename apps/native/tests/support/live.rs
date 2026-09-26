//! A real daemon for the end-to-end and smoke specs, isolated from the
//! user's: its config, binaries and worktrees dirs and its tracer pipe prefix
//! all live under `<repo>/.tmp/native-e2e/<test>-<pid>/`, set on the child
//! process only. [`Harness::open_live`] connects the root view to it through
//! the real network thread.

use std::collections::HashSet;
use std::ffi::OsString;
use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use gpui::{Modifiers, TestAppContext};
use protocol::{ClientMessage, DaemonHandshake};
use rustling_tulip_native::{
    Connection, NetCommand, NetDeps, NetEvent, RootDeps, RootView, bind_keys, spawn_net,
};
use serde_json::{Value, json};

use super::{Harness, OpenRecorder, Outbox, TestClock};

/// How long the daemon has to write `daemon.json` and answer `/health`.
const START_TIMEOUT: Duration = Duration::from_secs(15);
/// The id file the native client keeps its identity in.
const CLIENT_ID_FILE: &str = "client-id-native";
const MISSING_DAEMON: &str = "rustling-tulipd.exe not found next to the test binary; \
                              run `.\\rt.ps1 native-e2e` or `.\\rt.ps1 native-smoke`; \
                              they build daemon + tracer first";

/// The repository root, for the `.tmp` dir and `tools/e2e/fake-claude`.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("apps/native sits two levels under the repo root")
        .to_path_buf()
}

/// `<target>/<profile>/`: the test binary runs from its `deps/` subdir, and
/// cargo puts the daemon and tracer binaries beside that.
pub fn target_bin_dir() -> PathBuf {
    let exe = std::env::current_exe().expect("the test binary's path");
    exe.parent()
        .and_then(Path::parent)
        .expect("the test binary lives in <target>/<profile>/deps")
        .to_path_buf()
}

/// A shell whose prompt and echo are the same on every run.
fn test_shell() -> OsString {
    std::env::var_os("ComSpec").unwrap_or_else(|| OsString::from("cmd.exe"))
}

/// A running `rustling-tulipd` with every directory under this test's own
/// scratch dir. Drop kills it and the tracers it spawned, then removes the
/// dir.
pub struct LiveDaemon {
    root: PathBuf,
    child: Child,
    envs: Vec<(&'static str, OsString)>,
    handshake: DaemonHandshake,
}

impl LiveDaemon {
    /// Starts a daemon for `test` and waits for its handshake and `/health`.
    pub fn start(test: &str) -> Self {
        let daemon =
            target_bin_dir().join(format!("rustling-tulipd{}", std::env::consts::EXE_SUFFIX));
        assert!(
            daemon.is_file(),
            "{MISSING_DAEMON} (looked for {})",
            daemon.display()
        );
        let root = repo_root()
            .join(".tmp")
            .join("native-e2e")
            .join(format!("{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for sub in ["config", "binaries", "worktrees", "ui"] {
            std::fs::create_dir_all(root.join(sub)).expect("create the test's scratch dirs");
        }
        let envs = isolated_envs(&root, test);
        let mut command = Command::new(&daemon);
        command
            .envs(envs.iter().map(|(key, value)| (*key, value)))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        no_console(&mut command);
        let child = command.spawn().expect("spawn rustling-tulipd");
        let mut live = Self {
            root,
            child,
            envs,
            handshake: DaemonHandshake {
                protocol_version: 0,
                port: 0,
                auth_token: String::new(),
                pid: 0,
                supported_versions: Vec::new(),
            },
        };
        live.handshake = live.wait_healthy();
        live
    }

    fn wait_healthy(&mut self) -> DaemonHandshake {
        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            let exited = self.child.try_wait().ok().flatten();
            assert!(
                exited.is_none(),
                "rustling-tulipd exited during startup ({exited:?}); see {}",
                self.config_dir().join("logs").join("daemon.log").display()
            );
            if let Ok(handshake) = daemon_client::read_handshake_in(&self.config_dir())
                && handshake.pid == self.child.id()
                && health_ok(handshake.port)
            {
                return handshake;
            }
            assert!(
                Instant::now() < deadline,
                "rustling-tulipd wrote no healthy daemon.json within {START_TIMEOUT:?}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// The test's scratch dir; fixtures go here.
    pub fn dir(&self) -> &Path {
        &self.root
    }

    pub fn config_dir(&self) -> PathBuf {
        self.root.join("config")
    }

    /// The daemon's binary cache: the tracers, and a daemon it respawns,
    /// run from copies here named `<name>-<hash>.exe`.
    pub fn binaries_dir(&self) -> PathBuf {
        self.root.join("binaries")
    }

    /// The variables that isolate a process to this daemon, to set on a
    /// child with `Command::envs`.
    pub fn envs(&self) -> &[(&'static str, OsString)] {
        &self.envs
    }

    pub fn handshake(&self) -> &DaemonHandshake {
        &self.handshake
    }

    /// Force-kills the daemon alone, leaving its tracers running, as a crash
    /// would.
    pub fn kill(&mut self) {
        self.child.kill().expect("kill rustling-tulipd");
        self.child.wait().expect("reap rustling-tulipd");
    }

    /// The id of the registered repo at `path`, from the daemon's own
    /// `state.json`.
    pub fn repo_id(&self, path: &Path) -> Option<String> {
        let text = std::fs::read_to_string(self.config_dir().join("state.json")).ok()?;
        let state: Value = serde_json::from_str(&text).ok()?;
        let want = path.to_string_lossy();
        state["repos"].as_array()?.iter().find_map(|repo| {
            if same_path(repo["path"].as_str()?, &want) {
                repo["id"].as_str().map(str::to_owned)
            } else {
                None
            }
        })
    }

    /// A git repo under the test dir with one commit on `main`.
    pub fn git_fixture(&self) -> PathBuf {
        let repo = self.root.join("fixture");
        std::fs::create_dir_all(&repo).expect("create the fixture repo dir");
        git(&repo, &["init", "-q", "-b", "main"]);
        git(
            &repo,
            &[
                "-c",
                "user.name=rt-e2e",
                "-c",
                "user.email=rt-e2e@example.invalid",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "init",
            ],
        );
        repo
    }
}

impl Drop for LiveDaemon {
    fn drop(&mut self) {
        let config = self.config_dir();
        let tracers = sidecar_tracer_pids(&config);
        let ours = self.child.id();
        let respawned = daemon_client::read_handshake_in(&config)
            .ok()
            .map(|handshake| handshake.pid)
            .filter(|pid| *pid != ours);
        if matches!(self.child.try_wait(), Ok(None)) {
            kill_tree(ours);
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        // The tracers and a respawned daemon run from copies in this
        // daemon's own binaries dir; a pid whose image lies anywhere else
        // was reused by another process, the user's tracers included.
        let binaries = self.binaries_dir();
        let pids = respawned.into_iter().chain(tracers);
        for pid in pids.filter(|pid| image_under(*pid, &binaries)) {
            kill_tree(pid);
        }
        // A failed spec keeps its dir: the daemon and client logs are under
        // `config/logs/`.
        if !std::thread::panicking() {
            remove_dir_retrying(&self.root);
        }
    }
}

fn isolated_envs(root: &Path, test: &str) -> Vec<(&'static str, OsString)> {
    let fake_claude = repo_root()
        .join("tools")
        .join("e2e")
        .join("fake-claude")
        .join(if cfg!(windows) {
            "fake-claude.cmd"
        } else {
            "fake-claude.sh"
        });
    vec![
        (
            "RUSTLING_TULIP_CONFIG_DIR",
            root.join("config").into_os_string(),
        ),
        (
            "RUSTLING_TULIP_BINARIES_DIR",
            root.join("binaries").into_os_string(),
        ),
        (
            "RUSTLING_TULIP_WORKTREES_DIR",
            root.join("worktrees").into_os_string(),
        ),
        (
            "RUSTLING_TULIP_TRACER_PIPE_PREFIX",
            OsString::from(format!("rt-native-e2e-{}-{test}", std::process::id())),
        ),
        ("RUSTLING_TULIP_SHELL", test_shell()),
        ("RUSTLING_TULIP_SHELL_INTEGRATION", OsString::from("0")),
        ("RUSTLING_TULIP_CLAUDE", fake_claude.into_os_string()),
        // The tracer the daemon copies into its binaries dir: the one built
        // beside it, never the installed app's.
        (
            "RUSTLING_TULIP_BIN_TEMPLATES",
            target_bin_dir().into_os_string(),
        ),
    ]
}

fn same_path(a: &str, b: &str) -> bool {
    let norm = |p: &str| {
        p.replace('/', "\\")
            .trim_end_matches('\\')
            .to_ascii_lowercase()
    };
    norm(a) == norm(b)
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdout(Stdio::null())
        .status()
        .expect("run git (is it on PATH?)");
    assert!(status.success(), "git {args:?} failed: {status}");
}

/// A plain `GET /health` over loopback; true on a 200.
fn health_ok(port: u16) -> bool {
    let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let request = "GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut reply = String::new();
    let _ = stream.read_to_string(&mut reply);
    reply.starts_with("HTTP/1.1 200")
}

/// The tracer pids the daemon's session sidecars name.
fn sidecar_tracer_pids(config: &Path) -> HashSet<u32> {
    let Ok(entries) = std::fs::read_dir(config.join("sessions")) else {
        return HashSet::new();
    };
    entries
        .flatten()
        .filter_map(|entry| std::fs::read_to_string(entry.path().join("meta.json")).ok())
        .filter_map(|text| serde_json::from_str::<Value>(&text).ok())
        .filter_map(|meta| meta["tracer_pid"].as_u64())
        .filter_map(|pid| u32::try_from(pid).ok())
        .collect()
}

fn remove_dir_retrying(dir: &Path) {
    for _ in 0..20 {
        if std::fs::remove_dir_all(dir).is_ok() || !dir.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    tracing::warn!("could not remove {}", dir.display());
}

#[cfg(windows)]
fn no_console(command: &mut Command) {
    use std::os::windows::process::CommandExt as _;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn no_console(_command: &mut Command) {}

/// Force-kills `pid` and every process under it.
#[cfg(windows)]
pub fn kill_tree(pid: u32) {
    let mut command = Command::new("taskkill");
    command
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    no_console(&mut command);
    let _ = command.status();
}

/// Force-kills `pid`.
#[cfg(not(windows))]
pub fn kill_tree(pid: u32) {
    let _ = Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .status();
}

/// Whether process `pid` runs an image under `dir`, so a pid read from a
/// file is never killed after another process reused it.
fn image_under(pid: u32, dir: &Path) -> bool {
    image_path(pid).is_some_and(|image| path_under(&image, dir))
}

/// Every running process whose image lies under `dir`, with that image.
pub fn processes_under(dir: &Path) -> Vec<(u32, PathBuf)> {
    all_pids()
        .into_iter()
        .filter_map(|pid| {
            image_path(pid)
                .filter(|image| path_under(image, dir))
                .map(|image| (pid, image))
        })
        .collect()
}

/// Whether `path` lies inside `dir`, compared as Windows compares paths.
fn path_under(path: &Path, dir: &Path) -> bool {
    let norm = |p: &Path| {
        let text = p.to_string_lossy().replace('/', "\\").to_ascii_lowercase();
        let text = text.strip_prefix("\\\\?\\").unwrap_or(&text).to_owned();
        text.trim_end_matches('\\').to_owned()
    };
    norm(path)
        .strip_prefix(&norm(dir))
        .is_some_and(|rest| rest.starts_with('\\'))
}

/// The full path of the image process `pid` runs, when it can be read.
#[cfg(windows)]
fn image_path(pid: u32) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt as _;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };
    use windows::core::PWSTR;
    // SAFETY: opens a query-only handle, closed below.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let mut buffer = vec![0_u16; 32_768];
    let mut len = u32::try_from(buffer.len()).ok()?;
    // SAFETY: the buffer and its length are locals that outlive the call.
    let read = unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &raw mut len,
        )
    };
    // SAFETY: the handle OpenProcess returned, closed once.
    let _ = unsafe { CloseHandle(process) };
    read.ok()?;
    buffer.truncate(usize::try_from(len).ok()?);
    Some(PathBuf::from(OsString::from_wide(&buffer)))
}

#[cfg(not(windows))]
fn image_path(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

/// The id of every running process.
#[cfg(windows)]
fn all_pids() -> Vec<u32> {
    use windows::Win32::System::ProcessStatus::EnumProcesses;
    let mut pids = vec![0_u32; 1024];
    loop {
        let Ok(size) = u32::try_from(pids.len() * size_of::<u32>()) else {
            return Vec::new();
        };
        let mut needed = 0;
        // SAFETY: `pids` holds `size` bytes and outlives the call.
        if unsafe { EnumProcesses(pids.as_mut_ptr(), size, &raw mut needed) }.is_err() {
            return Vec::new();
        }
        if needed < size {
            pids.truncate(usize::try_from(needed).unwrap_or(0) / size_of::<u32>());
            return pids;
        }
        pids.resize(pids.len() * 2, 0);
    }
}

#[cfg(not(windows))]
fn all_pids() -> Vec<u32> {
    std::fs::read_dir("/proc")
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// The client side of a live harness: the root view's own command sender and
/// every connection state the network thread reported to the view.
pub struct LiveClient {
    tx: UnboundedSender<NetCommand>,
    states: Arc<Mutex<Vec<Connection>>>,
}

impl LiveClient {
    /// Sends `msg` to the daemon as the client would.
    pub fn send(&self, msg: ClientMessage) {
        self.tx
            .unbounded_send(NetCommand::Send(Box::new(msg)))
            .expect("the network thread is running");
    }

    /// The footer label of every state the view was sent, oldest first.
    pub fn footer_labels(&self) -> Vec<String> {
        self.states
            .lock()
            .expect("state log lock")
            .iter()
            .map(|conn| conn.footer(0).label)
            .collect()
    }

    /// The latest state's footer port and overlay text.
    pub fn latest(&self) -> Option<(Option<u16>, Option<&'static str>)> {
        self.states
            .lock()
            .expect("state log lock")
            .last()
            .map(|conn| (conn.footer(0).port, conn.overlay()))
    }
}

/// A standalone plain shell in `cwd`.
pub fn spawn_shell(cwd: &Path) -> ClientMessage {
    spawn_message(&json!({ "kind": "standalone", "cwd": cwd }), "plain_shell")
}

/// An interactive claude session on `main`, in place in `repo_id`'s checkout.
pub fn spawn_claude_in_place(repo_id: &str) -> ClientMessage {
    let target = json!({
        "kind": "single",
        "repo_id": repo_id,
        "branch_name": "main",
        "base_branch": null,
        "use_worktree": false,
    });
    spawn_message(&target, "interactive")
}

fn spawn_message(target: &Value, mode: &str) -> ClientMessage {
    serde_json::from_value(json!({
        "type": "spawn_session",
        "label": null,
        "target": target,
        "mode": mode,
        "initial_prompt": null,
        "dangerously_skip_permissions": false,
        "agent_options": { "kind": "claude" },
        "model": null,
    }))
    .expect("spawn request fixture")
}

impl<'a> Harness<'a> {
    /// Opens the window on the real network thread, connected to `daemon`
    /// through its isolated handshake. The fake-daemon half of the harness
    /// ([`Self::send`], [`Self::sent`]) does nothing here.
    pub fn open_live(cx: &'a mut TestAppContext, daemon: &LiveDaemon) -> (Self, LiveClient) {
        cx.update(bind_keys);
        let (tx, net_commands) = unbounded();
        let (net_events, from_net) = unbounded();
        let (to_view, view_events) = unbounded();
        spawn_net(live_deps(&daemon.config_dir()), net_commands, net_events);
        let states = Arc::new(Mutex::new(Vec::new()));
        tee_states(from_net, to_view, Arc::clone(&states));
        let clock = TestClock::new();
        let quits = std::rc::Rc::new(std::cell::Cell::new(0));
        let opener = Arc::new(OpenRecorder::default());
        let deps = RootDeps {
            tx: tx.clone(),
            events: view_events,
            ui_dir: Some(daemon.dir().join("ui")),
            paths: Err("live specs have no log paths".to_owned()),
            wanted: None,
            now: clock.clock(),
            quit: super::quit_recorder(&quits),
            open: opener.clone(),
        };
        let (root, cx) =
            cx.add_window_view(move |window, cx| RootView::with_transport(deps, window, cx));
        let harness = Self {
            cx,
            root,
            events: unbounded().0,
            commands: unbounded().1,
            clock,
            answered: HashSet::new(),
            outbox: Outbox::default(),
            quits,
            opener,
        };
        (harness, LiveClient { tx, states })
    }

    /// Lets the view settle and asks `done` until it holds, sleeping real
    /// time between polls: the daemon and the network thread run on real
    /// clocks, which the test executor never advances.
    pub fn wait_until(
        &mut self,
        what: &str,
        timeout: Duration,
        mut done: impl FnMut(&mut Self) -> bool,
    ) {
        let deadline = Instant::now() + timeout;
        loop {
            self.cx.run_until_parked();
            if done(self) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out after {timeout:?} waiting for {what}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Waits until a row of `pane` satisfies `row`; on a timeout the failure
    /// shows what the pane holds.
    pub fn wait_for_row(
        &mut self,
        pane: &str,
        what: &str,
        timeout: Duration,
        row: impl Fn(&str) -> bool,
    ) {
        let deadline = Instant::now() + timeout;
        loop {
            self.cx.run_until_parked();
            let root = self.root.clone();
            let rows = self
                .cx
                .update(|_, cx| root.read(cx).pane_grid_text(pane, cx))
                .unwrap_or_default();
            if rows.iter().any(|line| row(line)) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out after {timeout:?} waiting for {what}; pane {pane} shows {rows:#?}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The first pane of the tab on screen, once there is one.
    pub fn first_pane(&mut self) -> Option<String> {
        self.root(|root, _| root.active_pane_ids().into_iter().next())
    }

    /// Clicks into `pane` to focus it, then types `text` key by key.
    pub fn type_into(&mut self, pane: &str, text: &str) {
        let at = self.cell_center(pane, 0, 0);
        self.click(at, Modifiers::none());
        for c in text.chars() {
            match c {
                ' ' => self.keys("space"),
                '\n' => self.keys("enter"),
                c => self.keys(&c.to_string()),
            }
        }
    }
}

/// Network deps for the daemon under `config`: its handshake as written,
/// an identity kept beside it, and a stop that kills only its pid.
fn live_deps(config: &Path) -> NetDeps {
    let identity = daemon_client::client_identity_in(config, CLIENT_ID_FILE)
        .expect("create the live client's identity");
    let ensure_dir = config.to_path_buf();
    let stop_dir = config.to_path_buf();
    NetDeps {
        ensure: Box::new(move || {
            let dir = ensure_dir.clone();
            Box::pin(async move { daemon_client::read_handshake_in(&dir) })
        }),
        identity: Some(identity),
        stop: Box::new(move || {
            let dir = stop_dir.clone();
            Box::pin(async move { daemon_client::stop_in(&dir).await })
        }),
    }
}

/// Forwards every network event to the view, recording each state on the
/// way.
fn tee_states(
    mut from_net: futures::channel::mpsc::UnboundedReceiver<NetEvent>,
    to_view: UnboundedSender<NetEvent>,
    states: Arc<Mutex<Vec<Connection>>>,
) {
    std::thread::spawn(move || {
        futures::executor::block_on(async move {
            while let Some(event) = from_net.next().await {
                if let NetEvent::State(conn) = &event {
                    states.lock().expect("state log lock").push(conn.clone());
                }
                if to_view.unbounded_send(event).is_err() {
                    break;
                }
            }
        });
    });
}
